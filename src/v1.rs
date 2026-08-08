//! Legacy v1 REST API under /_apis/artifactcache, matching the wire behavior
//! of @actions/cache: reserve -> PATCH chunks -> commit, and key lookup that
//! answers 204 on a miss.

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::blob::{self, Mode, DOWNLOAD_URL_TTL_SECS};
use crate::index::{self, validate_key, validate_version};
use crate::store::Store;
use crate::{iso8601, json_error, unix_now, AppState};

/// "bytes start-end/*" from the action's uploadFile; the total is ignored.
fn parse_content_range(h: &str) -> Option<(u64, u64)> {
    let rest = h.strip_prefix("bytes ")?;
    let (range, _total) = rest.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    let start: u64 = start.parse().ok()?;
    let end: u64 = end.parse().ok()?;
    (start <= end).then_some((start, end))
}

/// Inclusive Content-Range length with checked arithmetic; None on malformed
/// input or on u64 overflow (e.g. `bytes 0-18446744073709551615/*`).
fn content_range_len(h: &str) -> Option<u64> {
    let (start, end) = parse_content_range(h)?;
    end.checked_sub(start)?.checked_add(1)
}

/// Lookup and diagnostic listing share this route.
pub async fn caches_get(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let entries = state.index.lock().await.entries.clone();
    if let Some(keys) = params.get("keys") {
        return lookup(&state, &entries, keys, params.get("version"));
    }
    if let Some(key) = params.get("key") {
        return list(&state, &entries, key);
    }
    json_error(StatusCode::BAD_REQUEST, "missing keys or key parameter")
}

/// Singular v1 endpoint used for cache lookup by the actions client.
pub async fn cache_lookup(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let Some(keys) = params.get("keys") else {
        return json_error(StatusCode::BAD_REQUEST, "missing keys parameter");
    };
    let entries = state.index.lock().await.entries.clone();
    lookup(&state, &entries, keys, params.get("version"))
}

fn lookup(
    state: &AppState,
    entries: &[index::Entry],
    keys: &str,
    version: Option<&String>,
) -> Response {
    let Some(version) = version else {
        return json_error(StatusCode::BAD_REQUEST, "missing version parameter");
    };
    let keys: Vec<&str> = keys.split(',').collect();
    if keys.is_empty() || !keys.iter().all(|k| validate_key(k)) || !validate_version(version) {
        return json_error(StatusCode::BAD_REQUEST, "invalid keys or version");
    }
    let restore: Vec<String> = keys.iter().skip(1).map(|s| s.to_string()).collect();
    let hit = index::lookup(entries, keys[0], &restore, version);
    let Some(hit) = hit else {
        // 204 is the v1 miss signal; the action then logs a diagnostics list.
        return StatusCode::NO_CONTENT.into_response();
    };
    let url = blob_url(&state.cfg.public_url, &state.cfg.auth_token, hit.id);
    (
        StatusCode::OK,
        axum::Json(serde_json::json!({
            "cacheKey": hit.key,
            "version": hit.version,
            "scope": state.cfg.namespace,
            "creationTime": iso8601(hit.created_at),
            "archiveLocation": url,
        })),
    )
        .into_response()
}

fn list(state: &AppState, entries: &[index::Entry], key: &str) -> Response {
    let mut matched: Vec<&index::Entry> =
        entries.iter().filter(|e| e.key.starts_with(key)).collect();
    matched.sort_by(|a, b| b.created_at.cmp(&a.created_at).then(b.id.cmp(&a.id)));
    matched.truncate(100);
    (
        StatusCode::OK,
        axum::Json(serde_json::json!({
            "totalCount": matched.len(),
            "artifactCaches": matched.iter().map(|e| serde_json::json!({
                "cacheKey": e.key,
                "cacheVersion": e.version,
                "scope": state.cfg.namespace,
                "creationTime": iso8601(e.created_at),
            })).collect::<Vec<_>>(),
        })),
    )
        .into_response()
}

/// POST /_apis/artifactcache/caches — reserve a cache slot.
pub async fn caches_post(State(state): State<Arc<AppState>>, body: Body) -> Response {
    let bytes = match read_small_body(body).await {
        Ok(b) => b,
        Err(r) => return r,
    };
    let value: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(_) => return json_error(StatusCode::BAD_REQUEST, "invalid JSON body"),
    };
    let Some(key) = value.get("key").and_then(|v| v.as_str()) else {
        return json_error(StatusCode::BAD_REQUEST, "missing key");
    };
    let Some(version) = value.get("version").and_then(|v| v.as_str()) else {
        return json_error(StatusCode::BAD_REQUEST, "missing version");
    };
    if !validate_key(key) || !validate_version(version) {
        return json_error(StatusCode::BAD_REQUEST, "invalid key or version");
    }
    let cache_size = value.get("cacheSize").and_then(|v| v.as_u64());
    match index::reserve(&state, key, version, "v1", cache_size).await {
        Ok(id) => (
            StatusCode::CREATED,
            axum::Json(serde_json::json!({ "cacheId": id })),
        )
            .into_response(),
        Err(index::ReserveError::TooLarge) => json_error(
            StatusCode::BAD_REQUEST,
            format!(
                "cache size of ~{} MB ({} B) is over the data cap limit, not saving cache",
                cache_size.unwrap_or(0) / (1024 * 1024),
                cache_size.unwrap_or(0)
            ),
        ),
        Err(index::ReserveError::Conflict) => json_error(
            StatusCode::CONFLICT,
            format!("cache entry with key '{key}' and version '{version}' already exists or is being created"),
        ),
        Err(index::ReserveError::Internal) => {
            json_error(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
        }
    }
}

/// PATCH /_apis/artifactcache/caches/{id} — stream one chunk into staging.
pub async fn cache_patch(
    State(state): State<Arc<AppState>>,
    Path(cache_id): Path<u64>,
    headers: axum::http::HeaderMap,
    body: Body,
) -> Response {
    let reservation_ok = {
        let idx = state.index.lock().await;
        idx.find_reservation(cache_id)
            .is_some_and(|r| r.protocol == "v1")
    };
    if !reservation_ok {
        return json_error(StatusCode::BAD_REQUEST, "reservation not found");
    }
    let Some(content_range) = headers
        .get(header::CONTENT_RANGE)
        .and_then(|v| v.to_str().ok())
    else {
        return json_error(StatusCode::BAD_REQUEST, "missing Content-Range header");
    };
    let Some(expected) = content_range_len(content_range) else {
        return json_error(StatusCode::BAD_REQUEST, "invalid Content-Range header");
    };
    let (start, end) = parse_content_range(content_range).expect("validated above");
    // The range must fit inside an archive of the configured maximum: the end
    // offset alone can be too large even when the length is small.
    if end >= state.cfg.max_cache_size_bytes || expected > state.cfg.max_cache_size_bytes {
        return json_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "chunk exceeds maximum cache size",
        );
    }
    // Reserve the staging budget before any bytes hit S3 so parallel parts
    // cannot push the reservation over the aggregate limit; the reservation
    // is rolled back when the upload fails.
    let chunk_key = state.store.chunk_key(cache_id, start);
    let old_size = match index::reserve_staging(&state, cache_id, &chunk_key, expected).await {
        Ok(old) => old,
        Err(index::StagingError::NoReservation) => {
            return json_error(StatusCode::BAD_REQUEST, "reservation not found");
        }
        Err(index::StagingError::OverBudget) => {
            return json_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "chunk exceeds the staging budget for this reservation",
            );
        }
        Err(index::StagingError::Internal) => {
            return json_error(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
        }
    };
    // The body is limited to the declared chunk length so a lying Content-Range
    // can never smuggle extra bytes into staging.
    match state
        .store
        .put(
            &chunk_key,
            expected,
            Store::upload_stream(body, expected),
        )
        .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => {
            tracing::warn!("chunk upload to S3 failed: {e}");
            if let Err(re) = index::release_staging(&state, cache_id, old_size, expected).await {
                tracing::error!(
                    "failed to roll back staging budget for reservation {cache_id}: {re}"
                );
            }
            json_error(StatusCode::INTERNAL_SERVER_ERROR, "upload failed")
        }
    }
}

/// POST /_apis/artifactcache/caches/{id} — commit the assembled archive.
pub async fn cache_commit(
    State(state): State<Arc<AppState>>,
    Path(cache_id): Path<u64>,
    body: Body,
) -> Response {
    let bytes = match read_small_body(body).await {
        Ok(b) => b,
        Err(r) => return r,
    };
    let value: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(_) => return json_error(StatusCode::BAD_REQUEST, "invalid JSON body"),
    };
    let Some(size) = value.get("size").and_then(|v| v.as_u64()) else {
        return json_error(StatusCode::BAD_REQUEST, "missing size");
    };
    match index::commit_v1(&state, cache_id, size).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(index::CommitError::NotFound) => {
            json_error(StatusCode::BAD_REQUEST, "reservation not found")
        }
        Err(index::CommitError::BadUpload) => json_error(
            StatusCode::BAD_REQUEST,
            "upload chunks do not cover the declared size",
        ),
        Err(index::CommitError::TooLarge) => {
            json_error(StatusCode::BAD_REQUEST, "cache exceeds maximum cache size")
        }
        Err(index::CommitError::Internal) => {
            json_error(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
        }
    }
}

fn blob_url(public_url: &str, auth_token: &str, entry_id: u64) -> String {
    format!(
        "{}/artifacts/{}",
        public_url.trim_end_matches('/'),
        blob::sign(
            auth_token,
            Mode::Download,
            entry_id,
            DOWNLOAD_URL_TTL_SECS,
            unix_now()
        )
    )
}

async fn read_small_body(body: Body) -> Result<Vec<u8>, Response> {
    const MAX: usize = 1024 * 1024;
    match axum::body::to_bytes(body, MAX).await {
        Ok(bytes) => Ok(bytes.to_vec()),
        Err(e) => {
            tracing::warn!("failed to read request body: {e}");
            Err(json_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "request body too large",
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn コンテントレンジの解析が正しい() {
        println!("Content-Range ヘッダの解析を確認");
        assert_eq!(parse_content_range("bytes 0-1023/*"), Some((0, 1023)));
        assert_eq!(
            parse_content_range("bytes 1024-2047/2048"),
            Some((1024, 2047))
        );
        assert_eq!(parse_content_range("bytes 5-5/*"), Some((5, 5)));
        assert_eq!(parse_content_range("bytes 5-1/*"), None);
        assert_eq!(parse_content_range("bytes 0-1023"), None);
        assert_eq!(parse_content_range("0-1023/*"), None);
        assert_eq!(parse_content_range("bytes a-b/*"), None);
        assert_eq!(parse_content_range(""), None);
    }

    #[test]
    fn コンテントレンジのインクルーシブ長がオーバーフローしない() {
        println!("Content-Range の長さ計算がオーバーフロー時に拒否されることを確認");
        assert_eq!(content_range_len("bytes 0-1023/*"), Some(1024));
        assert_eq!(content_range_len("bytes 5-5/*"), Some(1));
        assert_eq!(
            content_range_len("bytes 0-18446744073709551615/*"),
            None
        );
        assert_eq!(content_range_len("bytes 5-1/*"), None);
        assert_eq!(
            content_range_len("bytes 18446744073709551615-18446744073709551615/*"),
            Some(1)
        );
    }

    #[test]
    fn チャンクキーからオフセットを取り出せる() {
        println!("チャンクオブジェクトのキーからオフセットを取り出すことを確認");
        let key = "ns/staging/7/chunks/4096";
        let offset: u64 = key.rsplit('/').next().unwrap().parse().unwrap();
        assert_eq!(offset, 4096);
    }
}
