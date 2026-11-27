//! TLS intercept router: normalizes per-job path prefixes for local cache APIs
//! and transparently stream-proxies non-cache `results-receiver` traffic (logs,
//! step updates, artifacts) to the real GitHub Actions service.

use std::collections::HashSet;

use axum::body::Body;
use axum::extract::State;
use axum::http::uri::PathAndQuery;
use axum::http::{header, HeaderName, Request, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::Router;
use tower::ServiceExt;

#[derive(Clone)]
pub struct InterceptState {
    pub local_router: Router,
    pub upstream_client: reqwest::Client,
    pub upstream_results_receiver_url: String,
}

/// Strip a single leading job-scoped segment (e.g. `/<job-id>/_apis/artifactcache/...`)
/// when the remainder targets a cache or artifact endpoint.
pub fn strip_job_path_prefix(path: &str) -> Option<&str> {
    let rest = path.strip_prefix('/')?;
    let (prefix, after) = rest.split_once('/')?;
    if prefix.is_empty() {
        return None;
    }
    if after == "_apis/artifactcache"
        || after.starts_with("_apis/artifactcache/")
        || after.starts_with("twirp/")
        || after.starts_with("artifacts/")
    {
        Some(&path[prefix.len() + 1..])
    } else {
        None
    }
}

/// Returns true if the normalized path is handled locally by the cache proxy.
pub fn is_local_cache_path(path: &str) -> bool {
    path == "/healthz"
        || path == "/_apis/artifactcache/caches"
        || path == "/_apis/artifactcache/cache"
        || path.starts_with("/_apis/artifactcache/caches/")
        || path.starts_with("/twirp/github.actions.results.api.v1.CacheService/")
        || path.starts_with("/artifacts/")
}

fn request_host(req: &Request<Body>) -> String {
    if let Some(host) = req
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
    {
        return host.split(':').next().unwrap_or(host).to_ascii_lowercase();
    }
    if let Some(auth) = req.uri().authority() {
        return auth.host().to_ascii_lowercase();
    }
    String::new()
}

fn rewrite_uri_path(uri: &Uri, new_path: &str) -> Result<Uri, String> {
    let pq = match uri.query() {
        Some(q) => format!("{new_path}?{q}"),
        None => new_path.to_string(),
    };
    let mut parts = uri.clone().into_parts();
    parts.path_and_query = Some(
        pq.parse::<PathAndQuery>()
            .map_err(|e| format!("invalid rewritten path {pq}: {e}"))?,
    );
    Uri::from_parts(parts).map_err(|e| format!("rebuild uri: {e}"))
}

fn is_hop_by_hop(name: &HeaderName, extra_connection_headers: &HashSet<String>) -> bool {
    let lower = name.as_str();
    matches!(
        lower,
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "proxy-connection"
            | "te"
            | "trailers"
            | "transfer-encoding"
            | "upgrade"
    ) || extra_connection_headers.contains(lower)
}

fn collect_connection_headers(headers: &axum::http::HeaderMap) -> HashSet<String> {
    let mut set = HashSet::new();
    for val in headers.get_all(header::CONNECTION) {
        if let Ok(s) = val.to_str() {
            for item in s.split(',') {
                let trimmed = item.trim().to_ascii_lowercase();
                if !trimmed.is_empty() {
                    set.insert(trimmed);
                }
            }
        }
    }
    set
}

pub async fn handle(State(state): State<InterceptState>, mut req: Request<Body>) -> Response {
    let method = req.method().clone();
    let host = request_host(&req);
    let orig_path = req.uri().path().to_string();

    // Check whether this request (with optional /<job-id>/ prefix) belongs to local cache APIs.
    let normalized = strip_job_path_prefix(&orig_path)
        .unwrap_or(&orig_path)
        .to_string();
    if is_local_cache_path(&normalized) {
        if normalized != orig_path {
            match rewrite_uri_path(req.uri(), &normalized) {
                Ok(uri) => *req.uri_mut() = uri,
                Err(e) => {
                    tracing::warn!(error = %e, path = %orig_path, "failed to rewrite URI");
                    return (StatusCode::BAD_REQUEST, "invalid path").into_response();
                }
            }
        }
        let resp = match state.local_router.clone().oneshot(req).await {
            Ok(r) => r,
            Err(e) => match e {},
        };
        tracing::info!(
            mode = "local",
            method = %method,
            host = %host,
            path = %orig_path,
            status = resp.status().as_u16(),
            "intercept"
        );
        return resp;
    }

    // Non-cache traffic on results-receiver (logs, step updates, artifacts) is
    // forwarded verbatim to the real GitHub Actions service.
    if host != "artifactcache.actions.githubusercontent.com"
        && !state.upstream_results_receiver_url.is_empty()
    {
        let resp = proxy_upstream(&state, req).await;
        tracing::info!(
            mode = "upstream",
            method = %method,
            host = %host,
            path = %orig_path,
            status = resp.status().as_u16(),
            "intercept"
        );
        return resp;
    }

    tracing::info!(
        mode = "local",
        method = %method,
        host = %host,
        path = %orig_path,
        status = 404,
        "intercept"
    );
    (StatusCode::NOT_FOUND, "not found").into_response()
}

async fn proxy_upstream(state: &InterceptState, req: Request<Body>) -> Response {
    let base = state.upstream_results_receiver_url.trim_end_matches('/');
    let pq = req
        .uri()
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or("/");
    let target_url = format!("{base}{pq}");

    let method = req.method().clone();
    let conn_headers = collect_connection_headers(req.headers());

    let mut builder = state.upstream_client.request(method, &target_url);
    for (name, value) in req.headers() {
        if *name == header::HOST || is_hop_by_hop(name, &conn_headers) {
            continue;
        }
        builder = builder.header(name, value);
    }

    let body_stream = req.into_body().into_data_stream();
    builder = builder.body(reqwest::Body::wrap_stream(body_stream));

    let upstream_resp = match builder.send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(url = %target_url, error = %e, "upstream proxy request failed");
            return (StatusCode::BAD_GATEWAY, "upstream proxy error").into_response();
        }
    };

    let status = upstream_resp.status();
    let resp_conn_headers = collect_connection_headers(upstream_resp.headers());
    let mut out = Response::builder().status(status);
    for (name, value) in upstream_resp.headers() {
        if is_hop_by_hop(name, &resp_conn_headers) {
            continue;
        }
        out = out.header(name, value);
    }

    let stream = upstream_resp.bytes_stream();
    out.body(Body::from_stream(stream))
        .unwrap_or_else(|_| (StatusCode::BAD_GATEWAY, "failed to build response").into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_prefix_matches_caddy_rules() {
        assert_eq!(
            strip_job_path_prefix("/abc123/_apis/artifactcache/caches"),
            Some("/_apis/artifactcache/caches")
        );
        assert_eq!(
            strip_job_path_prefix(
                "/abc123/twirp/github.actions.results.api.v1.CacheService/CreateCacheEntry"
            ),
            Some("/twirp/github.actions.results.api.v1.CacheService/CreateCacheEntry")
        );
        assert_eq!(
            strip_job_path_prefix("/abc123/artifacts/tok"),
            Some("/artifacts/tok")
        );
        assert_eq!(strip_job_path_prefix("/_apis/artifactcache/caches"), None);
        assert_eq!(
            strip_job_path_prefix(
                "/twirp/github.actions.results.api.v1.ArtifactService/CreateArtifact"
            ),
            None
        );
        assert_eq!(strip_job_path_prefix("/abc/other/path"), None);
    }

    #[test]
    fn classifies_cache_vs_passthrough() {
        assert!(is_local_cache_path("/_apis/artifactcache/caches"));
        assert!(is_local_cache_path("/_apis/artifactcache/cache"));
        assert!(is_local_cache_path("/_apis/artifactcache/caches/42"));
        assert!(is_local_cache_path(
            "/twirp/github.actions.results.api.v1.CacheService/GetCacheEntryDownloadURL"
        ));
        assert!(is_local_cache_path("/artifacts/abc"));
        assert!(!is_local_cache_path(
            "/twirp/github.actions.results.api.v1.ArtifactService/CreateArtifact"
        ));
        assert!(!is_local_cache_path(
            "/twirp/github.actions.results.api.v1.ResultsService/UpdateWorkflowSteps"
        ));
    }

    #[tokio::test]
    async fn local_prefix_rewrite_and_upstream_passthrough() {
        use axum::routing::{any, get};
        use http_body_util::BodyExt;
        use tokio::net::TcpListener;

        // Mock upstream results-receiver echoing path+query, auth header, and body.
        let upstream_app = Router::new().fallback(any(|req: Request<Body>| async move {
            let auth = req
                .headers()
                .get(header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            let pq = req
                .uri()
                .path_and_query()
                .map(|p| p.as_str())
                .unwrap_or("")
                .to_string();
            let body = req.into_body().collect().await.unwrap().to_bytes();
            (
                StatusCode::CREATED,
                format!("upstream:{pq}|{auth}|{}", String::from_utf8_lossy(&body)),
            )
        }));
        let upstream_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_addr = upstream_listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(upstream_listener, upstream_app).await.unwrap();
        });

        let local_router = Router::new().route(
            "/_apis/artifactcache/caches",
            get(|req: Request<Body>| async move {
                format!(
                    "local:{}",
                    req.uri().path_and_query().map(|p| p.as_str()).unwrap_or("")
                )
            }),
        );
        let state = InterceptState {
            local_router,
            upstream_client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap(),
            upstream_results_receiver_url: format!("http://{upstream_addr}"),
        };
        let app = Router::new().fallback(any(handle)).with_state(state);

        // 1. Job-prefixed cache request is rewritten and handled locally.
        let req1 = Request::builder()
            .uri("/job-999/_apis/artifactcache/caches?key=foo")
            .header(header::HOST, "artifactcache.actions.githubusercontent.com")
            .body(Body::empty())
            .unwrap();
        let resp1 = app.clone().oneshot(req1).await.unwrap();
        assert_eq!(resp1.status(), StatusCode::OK);
        let body1 = resp1.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&body1[..], b"local:/_apis/artifactcache/caches?key=foo");

        // 2. Non-cache results-receiver request is streamed to upstream verbatim.
        let req2 = Request::builder()
            .method("POST")
            .uri("/twirp/github.actions.results.api.v1.ArtifactService/CreateArtifact?x=1")
            .header(
                header::HOST,
                "results-receiver.actions.githubusercontent.com",
            )
            .header(header::AUTHORIZATION, "Bearer gh-jwt-123")
            .body(Body::from("hello-artifact"))
            .unwrap();
        let resp2 = app.clone().oneshot(req2).await.unwrap();
        assert_eq!(resp2.status(), StatusCode::CREATED);
        let body2 = resp2.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(
            &body2[..],
            b"upstream:/twirp/github.actions.results.api.v1.ArtifactService/CreateArtifact?x=1|Bearer gh-jwt-123|hello-artifact"
        );
    }
}
