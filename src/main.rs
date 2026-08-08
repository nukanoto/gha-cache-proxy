//! gha-cache-proxy: local S3-backed cache proxy for GitHub Actions cache v1/v2.
//!
//! The service never talks to GitHub; every archive byte is streamed to and
//! from the configured S3-compatible endpoint only.

mod blob;
mod index;
mod store;
mod v1;
mod v2;

use std::sync::Arc;

use axum::body::Body;
use axum::extract::State;
use axum::http::{header, Request, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post};
use axum::Router;
use tracing_subscriber::EnvFilter;

pub const DEFAULT_MAX_CACHE_SIZE_BYTES: u64 = 10 * 1024 * 1024 * 1024; // 10 GiB

/// All settings come from the environment; no defaults for credentials.
#[derive(Clone)]
pub struct Config {
    pub listen_addr: String,
    pub public_url: String,
    pub auth_token: String,
    pub s3_endpoint: String,
    pub s3_bucket: String,
    pub s3_region: String,
    pub s3_access_key_id: String,
    pub s3_secret_access_key: String,
    pub max_cache_size_bytes: u64,
}

impl Config {
    fn from_env() -> Result<Self, String> {
        let required =
            |name: &str| std::env::var(name).map_err(|_| format!("missing required env {name}"));
        let max_cache_size_bytes = match std::env::var("MAX_CACHE_SIZE_BYTES") {
            Ok(v) => v
                .parse::<u64>()
                .map_err(|_| "MAX_CACHE_SIZE_BYTES must be a number".to_string())?,
            Err(_) => DEFAULT_MAX_CACHE_SIZE_BYTES,
        };
        let auth_token = required("AUTH_TOKEN")?;
        if auth_token.is_empty() {
            return Err("AUTH_TOKEN must not be empty".to_string());
        }
        Ok(Self {
            listen_addr: std::env::var("LISTEN_ADDR")
                .unwrap_or_else(|_| "0.0.0.0:8080".to_string()),
            public_url: required("PUBLIC_URL")?,
            auth_token,
            s3_endpoint: required("S3_ENDPOINT")?,
            s3_bucket: required("S3_BUCKET")?,
            s3_region: required("S3_REGION")?,
            s3_access_key_id: required("S3_ACCESS_KEY_ID")?,
            s3_secret_access_key: required("S3_SECRET_ACCESS_KEY")?,
            max_cache_size_bytes,
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

/// Bearer auth for every control-plane route; signed blob URLs authenticate
/// themselves, so /artifacts/* and /healthz are exempt.
async fn require_auth(
    State(state): State<Arc<AppState>>,
    req: Request<Body>,
    next: Next,
) -> Response {
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

#[tokio::main]
async fn main() {
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

    let store = store::Store::new(store::build_client(&cfg), cfg.s3_bucket.clone());
    let index = match index::Index::load(&store).await {
        Ok(i) => i,
        Err(e) => {
            eprintln!("failed to load cache index: {e}");
            std::process::exit(1);
        }
    };

    tracing::info!(
        "gha-cache-proxy starting: listen={} bucket={} endpoint={} max_size_bytes={}",
        cfg.listen_addr,
        cfg.s3_bucket,
        cfg.s3_endpoint,
        cfg.max_cache_size_bytes
    );

    let state = Arc::new(AppState {
        cfg,
        store,
        index: tokio::sync::Mutex::new(index),
    });

    let app = Router::new()
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
        .layer(middleware::from_fn_with_state(state.clone(), require_auth))
        .with_state(state.clone());

    let listener = tokio::net::TcpListener::bind(&state.cfg.listen_addr)
        .await
        .expect("failed to bind listener");
    tracing::info!("listening on {}", state.cfg.listen_addr);
    axum::serve(listener, app).await.expect("server error");
}
