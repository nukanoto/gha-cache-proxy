//! gha-cache-server: local S3-backed cache server for GitHub Actions cache v1/v2.
//!
//! In plain HTTP mode (`LISTEN_ADDR`), the service only talks to the configured
//! S3-compatible endpoint. When `TLS_LISTEN_ADDR` is also set, it terminates
//! TLS with an auto-generated local CA, handles cache requests directly, and
//! transparently proxies non-cache `results-receiver` traffic (logs, step
//! updates, artifacts) to GitHub.

mod blob;
mod index;
mod intercept;
mod store;
mod tls;
mod v1;
mod v2;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::State;
use axum::http::{header, Request, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, patch, post};
use axum::Router;
use tracing_subscriber::EnvFilter;

pub const DEFAULT_MAX_CACHE_SIZE_BYTES: u64 = 10 * 1024 * 1024 * 1024; // 10 GiB
pub const DEFAULT_TLS_HOSTS: &str =
    "artifactcache.actions.githubusercontent.com,results-receiver.actions.githubusercontent.com";
pub const DEFAULT_UPSTREAM_RESULTS_RECEIVER_URL: &str =
    "https://results-receiver.actions.githubusercontent.com";

#[derive(Clone)]
pub struct TlsConfig {
    pub listen_addr: String,
    pub hosts: Vec<String>,
    pub ca_cert_out: Option<PathBuf>,
    pub data_dir: Option<PathBuf>,
    pub upstream_results_receiver_url: String,
}

/// All settings come from the environment; no defaults for S3 credentials.
#[derive(Clone)]
pub struct Config {
    pub listen_addr: String,
    pub public_url: String,
    pub auth_token: String,
    pub require_http_auth: bool,
    pub s3_endpoint: String,
    pub s3_bucket: String,
    pub s3_region: String,
    pub s3_access_key_id: String,
    pub s3_secret_access_key: String,
    pub max_cache_size_bytes: u64,
    pub tls: Option<TlsConfig>,
}

impl Config {
    fn from_env() -> Result<Self, String> {
        let required =
            |name: &str| std::env::var(name).map_err(|_| format!("missing required env {name}"));
        let optional = |name: &str| {
            std::env::var(name)
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        let max_cache_size_bytes = match std::env::var("MAX_CACHE_SIZE_BYTES") {
            Ok(v) => v
                .parse::<u64>()
                .map_err(|_| "MAX_CACHE_SIZE_BYTES must be a number".to_string())?,
            Err(_) => DEFAULT_MAX_CACHE_SIZE_BYTES,
        };

        let tls = match optional("TLS_LISTEN_ADDR") {
            Some(listen_addr) => {
                let hosts_raw =
                    optional("TLS_HOSTS").unwrap_or_else(|| DEFAULT_TLS_HOSTS.to_string());
                let hosts: Vec<String> = hosts_raw
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                let upstream_results_receiver_url = std::env::var("UPSTREAM_RESULTS_RECEIVER_URL")
                    .unwrap_or_else(|_| DEFAULT_UPSTREAM_RESULTS_RECEIVER_URL.to_string());
                Some(TlsConfig {
                    listen_addr,
                    hosts,
                    ca_cert_out: optional("TLS_CA_CERT_OUT").map(PathBuf::from),
                    data_dir: optional("TLS_DATA_DIR").map(PathBuf::from),
                    upstream_results_receiver_url,
                })
            }
            None => None,
        };

        let (auth_token, require_http_auth) = match optional("AUTH_TOKEN") {
            Some(token) => (token, true),
            None if tls.is_some() => {
                // TLS intercept mode accepts runner requests directly without a shared
                // static bearer token; mint an internal secret for blob URL HMAC signing.
                let secret = format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple());
                (secret, false)
            }
            None => return Err("missing required env AUTH_TOKEN".to_string()),
        };

        Ok(Self {
            listen_addr: std::env::var("LISTEN_ADDR")
                .unwrap_or_else(|_| "0.0.0.0:8080".to_string()),
            public_url: required("PUBLIC_URL")?,
            auth_token,
            require_http_auth,
            s3_endpoint: required("S3_ENDPOINT")?,
            s3_bucket: required("S3_BUCKET")?,
            s3_region: required("S3_REGION")?,
            s3_access_key_id: required("S3_ACCESS_KEY_ID")?,
            s3_secret_access_key: required("S3_SECRET_ACCESS_KEY")?,
            max_cache_size_bytes,
            tls,
        })
    }
}

/// Shared process state; the index mutex serializes all state mutations.
pub struct AppState {
    pub cfg: Config,
    pub store: store::Store,
    pub index: tokio::sync::Mutex<index::Index>,
}

pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// UTC RFC 3339 timestamp for the JSON API responses.
pub fn iso8601(secs: u64) -> String {
    time::OffsetDateTime::from_unix_timestamp(secs as i64)
        .ok()
        .and_then(|dt| {
            dt.format(&time::format_description::well_known::Rfc3339)
                .ok()
        })
        .unwrap_or_default()
}

/// Constant-time comparison so the bearer token cannot be probed byte by byte.
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn json_error(status: StatusCode, message: impl Into<String>) -> Response {
    (
        status,
        axum::Json(serde_json::json!({ "message": message.into() })),
    )
        .into_response()
}

/// Health is unauthenticated and never probes S3, so load balancers can poll freely.
async fn health() -> impl IntoResponse {
    (StatusCode::OK, "ok")
}

/// Bearer auth for every control-plane route on the plain HTTP listener;
/// signed blob URLs authenticate themselves, so /artifacts/* and /healthz are exempt.
async fn require_auth(
    State(state): State<Arc<AppState>>,
    req: Request<Body>,
    next: Next,
) -> Response {
    if !state.cfg.require_http_auth {
        return next.run(req).await;
    }
    let path = req.uri().path();
    if path == "/healthz" || path.starts_with("/artifacts/") {
        return next.run(req).await;
    }
    let expected = format!("Bearer {}", state.cfg.auth_token);
    let authorized = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| ct_eq(v.as_bytes(), expected.as_bytes()));
    if authorized {
        next.run(req).await
    } else {
        (
            StatusCode::UNAUTHORIZED,
            axum::Json(serde_json::json!({ "message": "unauthorized", "code": "unauthenticated" })),
        )
            .into_response()
    }
}

fn cache_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/healthz", get(health))
        .route(
            "/_apis/artifactcache/caches",
            get(v1::caches_get).post(v1::caches_post),
        )
        // v1 lookup is a singular endpoint; keep diagnostics on the plural path.
        .route("/_apis/artifactcache/cache", get(v1::cache_lookup))
        .route(
            "/_apis/artifactcache/caches/{cache_id}",
            patch(v1::cache_patch).post(v1::cache_commit),
        )
        .route(
            "/twirp/github.actions.results.api.v1.CacheService/CreateCacheEntry",
            post(v2::create_cache_entry),
        )
        .route(
            "/twirp/github.actions.results.api.v1.CacheService/FinalizeCacheEntryUpload",
            post(v2::finalize_cache_entry_upload),
        )
        .route(
            "/twirp/github.actions.results.api.v1.CacheService/GetCacheEntryDownloadURL",
            post(v2::get_cache_entry_download_url),
        )
        .route(
            "/artifacts/{token}",
            get(blob::get).head(blob::head).put(blob::put),
        )
        .with_state(state)
}

/// Built-in healthcheck probe (`gha-cache-server --healthcheck`) so containers do
/// not need curl installed.
fn run_healthcheck() -> i32 {
    let listen = std::env::var("LISTEN_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".to_string());
    let port = listen.rsplit(':').next().unwrap_or("8080");
    let addr = format!("127.0.0.1:{port}");
    let Ok(mut stream) = TcpStream::connect(&addr) else {
        return 1;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
    if stream
        .write_all(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .is_err()
    {
        return 1;
    }
    let mut buf = [0u8; 64];
    let Ok(n) = stream.read(&mut buf) else {
        return 1;
    };
    let head = String::from_utf8_lossy(&buf[..n]);
    if head.starts_with("HTTP/1.1 200") || head.starts_with("HTTP/1.0 200") {
        0
    } else {
        1
    }
}

#[tokio::main]
async fn main() {
    if std::env::args().nth(1).as_deref() == Some("--healthcheck") {
        std::process::exit(run_healthcheck());
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cfg = match Config::from_env() {
        Ok(c) => c,
        Err(msg) => {
            eprintln!("configuration error: {msg}");
            std::process::exit(1);
        }
    };

    // Prepare TLS material and export the public CA certificate before opening
    // /healthz so dependents waiting on service_healthy always see the CA file.
    let tls_setup = match &cfg.tls {
        Some(tls_cfg) => {
            let material = match tls::load_or_generate(
                &tls_cfg.hosts,
                tls_cfg.data_dir.as_deref(),
                tls_cfg.ca_cert_out.as_deref(),
            ) {
                Ok(m) => m,
                Err(e) => {
                    eprintln!("TLS setup error: {e}");
                    std::process::exit(1);
                }
            };
            let tls_listener = match tokio::net::TcpListener::bind(&tls_cfg.listen_addr).await {
                Ok(l) => l,
                Err(e) => {
                    eprintln!("failed to bind TLS listener {}: {e}", tls_cfg.listen_addr);
                    std::process::exit(1);
                }
            };
            let upstream_client = reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("failed to build upstream HTTP client");
            Some((tls_cfg.clone(), material, tls_listener, upstream_client))
        }
        None => None,
    };

    let store = store::Store::new(store::build_client(&cfg), cfg.s3_bucket.clone());
    let index = match index::Index::load(&store).await {
        Ok(i) => i,
        Err(e) => {
            eprintln!("failed to load cache index: {e}");
            std::process::exit(1);
        }
    };

    tracing::info!(
        "gha-cache-server starting: listen={} tls_listen={} bucket={} endpoint={} max_size_bytes={}",
        cfg.listen_addr,
        cfg.tls
            .as_ref()
            .map(|t| t.listen_addr.as_str())
            .unwrap_or("disabled"),
        cfg.s3_bucket,
        cfg.s3_endpoint,
        cfg.max_cache_size_bytes
    );

    let state = Arc::new(AppState {
        cfg,
        store,
        index: tokio::sync::Mutex::new(index),
    });

    let http_app = cache_router(state.clone())
        .layer(middleware::from_fn_with_state(state.clone(), require_auth));

    let http_listener = tokio::net::TcpListener::bind(&state.cfg.listen_addr)
        .await
        .expect("failed to bind HTTP listener");
    tracing::info!("listening on http://{}", state.cfg.listen_addr);

    if let Some((tls_cfg, material, tls_listener, upstream_client)) = tls_setup {
        let intercept_state = intercept::InterceptState {
            local_router: cache_router(state.clone()),
            upstream_client,
            upstream_results_receiver_url: tls_cfg.upstream_results_receiver_url.clone(),
        };
        let tls_app = Router::new()
            .fallback(any(intercept::handle))
            .with_state(intercept_state);
        tracing::info!(
            "listening on https://{} (hosts={}, ca_out={})",
            tls_cfg.listen_addr,
            tls_cfg.hosts.join(","),
            tls_cfg
                .ca_cert_out
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "none".to_string())
        );

        let http_srv = async {
            axum::serve(http_listener, http_app)
                .await
                .map_err(|e| format!("HTTP server error: {e}"))
        };
        let tls_srv = tls::serve(tls_listener, material.server_config, tls_app);
        if let Err(e) = tokio::try_join!(http_srv, tls_srv) {
            eprintln!("{e}");
            std::process::exit(1);
        }
    } else {
        axum::serve(http_listener, http_app)
            .await
            .expect("server error");
    }
}
