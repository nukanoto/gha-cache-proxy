//! Azure Blob-compatible artifact endpoint. The SDK never sends bearer tokens,
//! so every blob URL carries an opaque HMAC-signed token that encodes the
//! mode (upload/download), expiry and the reservation or entry id.

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderValue, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use base64::Engine;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use uuid::Uuid;

use crate::index;
use crate::store::Store;
use crate::{ct_eq, json_error, unix_now, AppState};

pub const UPLOAD_URL_TTL_SECS: u64 = 3600;
pub const DOWNLOAD_URL_TTL_SECS: u64 = 24 * 3600;

/// Blocks must fit in memory for the blocklist check; the SDK sends ~40-byte
/// base64 ids, so a few megabytes covers an absurd number of blocks.
const MAX_BLOCKLIST_BYTES: u64 = 4 * 1024 * 1024;
const MAX_BLOCK_ID_BYTES: usize = 1024;

const TOKEN_VERSION: u8 = 1;
const NONCE_LEN: usize = 16;
const MAC_LEN: usize = 32;
const TOKEN_LEN: usize = 1 + 1 + 8 + 8 + NONCE_LEN + MAC_LEN;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Mode {
    Upload,
    Download,
}

fn mode_char(mode: Mode) -> u8 {
    match mode {
        Mode::Upload => b'u',
        Mode::Download => b'd',
    }
}

/// The auth token doubles as the signing secret, bound to this purpose only.
fn signing_key(auth_token: &str) -> Hmac<Sha256> {
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(auth_token.as_bytes())
        .expect("hmac accepts any key length");
    mac.update(b"gha-cache-proxy blob signing v1");
    let derived = mac.finalize().into_bytes();
    Hmac::new_from_slice(&derived).expect("hmac accepts any key length")
}

/// Sign a token for `id` valid for `ttl_secs` from `now`. The nonce makes two
/// tokens for the same id indistinguishable even when minted in the same
/// second, and keeps URLs unguessable for anyone without the key.
pub fn sign(auth_token: &str, mode: Mode, id: u64, ttl_secs: u64, now: u64) -> String {
    let mut payload = Vec::with_capacity(TOKEN_LEN);
    payload.push(TOKEN_VERSION);
    payload.push(mode_char(mode));
    payload.extend_from_slice(&now.saturating_add(ttl_secs).to_le_bytes());
    payload.extend_from_slice(&id.to_le_bytes());
    payload.extend_from_slice(Uuid::new_v4().as_bytes());
    let mut mac = signing_key(auth_token);
    mac.update(&payload);
    let tag = mac.finalize().into_bytes();
    payload.extend_from_slice(&tag);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload)
}

/// Verify signature, version, expiry and return (mode, id). `now` is injected
/// so the expiry logic is unit-testable.
pub fn verify(auth_token: &str, token: &str, now: u64) -> Option<(Mode, u64)> {
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(token)
        .ok()?;
    if bytes.len() != TOKEN_LEN || bytes[0] != TOKEN_VERSION {
        return None;
    }
    let (payload, tag) = bytes.split_at(TOKEN_LEN - MAC_LEN);
    let mut mac = signing_key(auth_token);
    mac.update(payload);
    let expected = mac.finalize().into_bytes();
    if !ct_eq(tag, &expected) {
        return None;
    }
    let mode = match payload[1] {
        b'u' => Mode::Upload,
        b'd' => Mode::Download,
        _ => return None,
    };
    let expires = u64::from_le_bytes(payload[2..10].try_into().ok()?);
    if now > expires {
        return None;
    }
    let id = u64::from_le_bytes(payload[10..18].try_into().ok()?);
    Some((mode, id))
}

/// Parse a single HTTP Range header. Ok(Some) = satisfiable range,
/// Ok(None) = unsatisfiable (416), Err = malformed or unsupported.
fn parse_range(h: &str, size: u64) -> Result<Option<(u64, u64)>, ()> {
    let spec = h.strip_prefix("bytes=").ok_or(())?;
    if spec.contains(',') {
        return Err(());
    }
    let (s, e) = spec.split_once('-').ok_or(())?;
    let start: u64 = s.parse().map_err(|_| ())?;
    let end: u64 = if e.is_empty() {
        size.saturating_sub(1)
    } else {
        e.parse().map_err(|_| ())?
    };
    if size == 0 || start >= size || end < start {
        return Ok(None);
    }
    Ok(Some((start, end.min(size - 1))))
}

/// Parse the block list XML body into the ordered base64 block ids. The root
/// element must be BlockList and every Latest element must be closed; quick-xml
/// alone is too lenient about truncated input to rely on.
fn parse_blocklist(xml: &[u8]) -> Result<Vec<String>, String> {
    let mut reader = quick_xml::Reader::from_reader(xml);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut ids = Vec::new();
    let mut in_latest = false;
    let mut seen_block_list = false;
    let mut list_open = false;
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(quick_xml::events::Event::Start(e)) => match e.local_name().as_ref() {
                b"BlockList" => {
                    seen_block_list = true;
                    list_open = true;
                }
                b"Latest" => in_latest = true,
                _ => {}
            },
            Ok(quick_xml::events::Event::Empty(e)) => match e.local_name().as_ref() {
                b"BlockList" => seen_block_list = true,
                b"Latest" => return Err("empty Latest element".into()),
                _ => {}
            },
            Ok(quick_xml::events::Event::Text(t)) if in_latest => {
                ids.push(t.unescape().map_err(|e| e.to_string())?.into_owned());
            }
            Ok(quick_xml::events::Event::End(e)) => {
                if e.local_name().as_ref() == b"BlockList" {
                    list_open = false;
                }
                in_latest = false;
            }
            Ok(quick_xml::events::Event::Eof) => break,
            Ok(_) => {}
            Err(e) => return Err(format!("invalid block list XML: {e}")),
        }
        buf.clear();
    }
    if !seen_block_list || list_open || in_latest {
        return Err("truncated or missing BlockList element".into());
    }
    Ok(ids)
}

/// Static header values used on blob responses; cannot fail to parse.
fn hv(value: &str) -> HeaderValue {
    value.parse().expect("static header value")
}

/// Shared validation for every blob request before any S3 work happens.
fn verify_token(auth_token: &str, token: &str, want: Mode) -> Option<u64> {
    let (mode, id) = verify(auth_token, token, unix_now())?;
    (mode == want).then_some(id)
}

fn content_length(req: &Request<Body>) -> Option<u64> {
    req.headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

pub async fn put(
    State(state): State<Arc<AppState>>,
    Path(token): Path<String>,
    Query(query): Query<HashMap<String, String>>,
    req: Request<Body>,
) -> Response {
    let Some(id) = verify_token(&state.cfg.auth_token, &token, Mode::Upload) else {
        return json_error(StatusCode::FORBIDDEN, "invalid or expired upload token");
    };
    let reservation_ok = {
        let idx = state.index.lock().await;
        idx.find_reservation(id).is_some()
    };
    if !reservation_ok {
        return json_error(StatusCode::NOT_FOUND, "reservation not found");
    }
    match query.get("comp").map(String::as_str) {
        Some("block") => put_block(&state, id, query, req).await,
        Some("blocklist") => put_blocklist(&state, id, req).await,
        None => put_blob(&state, id, req).await,
        Some(other) => json_error(
            StatusCode::BAD_REQUEST,
            format!("unsupported comp parameter: {other}"),
        ),
    }
}

async fn put_blob(state: &AppState, id: u64, req: Request<Body>) -> Response {
    let Some(len) = content_length(&req) else {
        return json_error(StatusCode::LENGTH_REQUIRED, "Content-Length required");
    };
    if len > state.cfg.max_cache_size_bytes {
        return json_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "body exceeds maximum cache size",
        );
    }
    // Count plain uploads too; otherwise a caller can combine a blob and
    // uncommitted blocks to exceed the per-reservation storage budget.
    let blob_key = state.store.staging_blob_key(id);
    let old_size = match index::reserve_staging(state, id, &blob_key, len).await {
        Ok(old) => old,
        Err(index::StagingError::NoReservation) => {
            return json_error(StatusCode::NOT_FOUND, "reservation not found");
        }
        Err(index::StagingError::OverBudget) => {
            return json_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "blob exceeds the staging budget for this reservation",
            );
        }
        Err(index::StagingError::Internal) => {
            return json_error(StatusCode::INTERNAL_SERVER_ERROR, "internal error");
        }
    };
    match state
        .store
        .put(&blob_key, len, Store::upload_stream(req.into_body(), len))
        .await
    {
        Ok(()) => created(),
        Err(e) => {
            tracing::warn!("blob upload to S3 failed: {e}");
            if let Err(re) = index::release_staging(state, id, old_size, len).await {
                tracing::error!("failed to roll back staging budget for reservation {id}: {re}");
            }
            json_error(StatusCode::INTERNAL_SERVER_ERROR, "upload failed")
        }
    }
}

async fn put_block(
    state: &AppState,
    id: u64,
    query: HashMap<String, String>,
    req: Request<Body>,
) -> Response {
    let Some(block_id) = query.get("blockid") else {
        return json_error(StatusCode::BAD_REQUEST, "missing blockid parameter");
    };
    if block_id.is_empty() || block_id.len() > MAX_BLOCK_ID_BYTES {
        return json_error(StatusCode::BAD_REQUEST, "invalid blockid");
    }
    if base64::engine::general_purpose::STANDARD
        .decode(block_id)
        .is_err()
    {
        return json_error(StatusCode::BAD_REQUEST, "blockid must be base64");
    }
    let Some(len) = content_length(&req) else {
        return json_error(StatusCode::LENGTH_REQUIRED, "Content-Length required");
    };
    if len > state.cfg.max_cache_size_bytes {
        return json_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "block exceeds maximum cache size",
        );
    }
    // Reserve the staging budget before any bytes hit S3; the reservation is
    // rolled back when the upload fails. Parallel blocks of different ids
    // still proceed concurrently because the budget update is serialized only
    // around the check-and-reserve, not the upload itself.
    let block_key = state.store.block_key(id, block_id);
    let old_size = match index::reserve_staging(state, id, &block_key, len).await {
        Ok(old) => old,
        Err(index::StagingError::NoReservation) => {
            return json_error(StatusCode::NOT_FOUND, "reservation not found");
        }
        Err(index::StagingError::OverBudget) => {
            return json_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "block exceeds the staging budget for this reservation",
            );
        }
        Err(index::StagingError::Internal) => {
            return json_error(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
        }
    };
    match state
        .store
        .put(&block_key, len, Store::upload_stream(req.into_body(), len))
        .await
    {
        Ok(()) => created(),
        Err(e) => {
            tracing::warn!("block upload to S3 failed: {e}");
            if let Err(re) = index::release_staging(state, id, old_size, len).await {
                tracing::error!("failed to roll back staging budget for reservation {id}: {re}");
            }
            json_error(StatusCode::INTERNAL_SERVER_ERROR, "block upload failed")
        }
    }
}

async fn put_blocklist(state: &AppState, id: u64, req: Request<Body>) -> Response {
    let body = match axum::body::to_bytes(req.into_body(), MAX_BLOCKLIST_BYTES as usize).await {
        Ok(bytes) => bytes.to_vec(),
        Err(e) => {
            tracing::warn!("failed to read block list body: {e}");
            return json_error(StatusCode::PAYLOAD_TOO_LARGE, "block list too large");
        }
    };
    let block_ids = match parse_blocklist(&body) {
        Ok(ids) => ids,
        Err(msg) => return json_error(StatusCode::BAD_REQUEST, msg),
    };
    if block_ids.is_empty() {
        return json_error(StatusCode::BAD_REQUEST, "empty block list");
    }
    let mut unique = block_ids.clone();
    unique.sort_unstable();
    unique.dedup();
    if unique.len() != block_ids.len() {
        return json_error(StatusCode::BAD_REQUEST, "duplicate block in block list");
    }

    let objects = match state.store.list(&state.store.block_prefix(id)).await {
        Ok(o) => o,
        Err(e) => {
            tracing::error!("failed to list staged blocks: {e}");
            return json_error(StatusCode::INTERNAL_SERVER_ERROR, "internal error");
        }
    };
    let sizes: HashMap<String, u64> = objects
        .iter()
        .filter_map(|o| {
            let key = o.key.as_ref()?;
            let block_id = key.rsplit('/').next()?;
            Some((block_id.to_string(), o.size.unwrap_or(0) as u64))
        })
        .collect();

    let mut parts = Vec::with_capacity(block_ids.len());
    let mut total = 0u64;
    for block_id in &block_ids {
        let Some(size) = sizes.get(block_id) else {
            return json_error(
                StatusCode::BAD_REQUEST,
                format!("unknown block in block list: {block_id}"),
            );
        };
        total = match total.checked_add(*size) {
            Some(total) if total <= state.cfg.max_cache_size_bytes => total,
            _ => {
                return json_error(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "blob exceeds maximum cache size",
                )
            }
        };
        parts.push((state.store.block_key(id, block_id), *size));
    }
    if total == 0 {
        return json_error(
            StatusCode::BAD_REQUEST,
            "block list assembles an empty blob",
        );
    }
    if total > state.cfg.max_cache_size_bytes {
        return json_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "blob exceeds maximum cache size",
        );
    }

    // Assemble the blocks into the staging blob, then free the blocks.
    if let Err(e) = state
        .store
        .concat(&state.store.staging_blob_key(id), total, parts)
        .await
    {
        tracing::error!("failed to assemble staged blob: {e}");
        return json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "block list commit failed",
        );
    }
    let store = state.store.clone();
    tokio::spawn(async move {
        if let Err(e) = store.delete_prefix(&store.block_prefix(id)).await {
            tracing::warn!("block cleanup failed for {id}: {e}");
        }
    });
    created()
}

fn created() -> Response {
    // Azure answers 201 with an ETag; the SDK only checks the status.
    (
        StatusCode::CREATED,
        [(header::ETAG, format!("\"{}\"", Uuid::new_v4()))],
    )
        .into_response()
}

pub async fn head(State(state): State<Arc<AppState>>, Path(token): Path<String>) -> Response {
    let Some(id) = verify_token(&state.cfg.auth_token, &token, Mode::Download) else {
        return json_error(StatusCode::FORBIDDEN, "invalid or expired download token");
    };
    let entry = {
        let idx = state.index.lock().await;
        idx.find_entry(id).cloned()
    };
    let Some(entry) = entry else {
        return json_error(StatusCode::NOT_FOUND, "cache entry not found");
    };
    // Only a confirmed missing object is a 404; S3 failures must not look
    // like cache misses or the client would overwrite a healthy cache.
    let archive = match state.store.head(&state.store.archive_key(id)).await {
        Ok(Some(h)) => h,
        Ok(None) => return json_error(StatusCode::NOT_FOUND, "archive not found"),
        Err(e) => {
            tracing::error!("failed to inspect archive for entry {id}: {e}");
            return json_error(StatusCode::INTERNAL_SERVER_ERROR, "internal error");
        }
    };
    let mut response = Response::new(Body::empty());
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_LENGTH, hv(&entry.size.to_string()));
    headers.insert(header::CONTENT_TYPE, hv("application/octet-stream"));
    headers.insert(header::ACCEPT_RANGES, hv("bytes"));
    if let Some(etag) = archive.e_tag() {
        headers.insert(header::ETAG, hv(etag));
    }
    if let Some(modified) = archive.last_modified() {
        headers.insert(
            header::LAST_MODIFIED,
            hv(&modified
                .fmt(aws_smithy_types::date_time::Format::HttpDate)
                .unwrap_or_default()),
        );
    }
    response
}

pub async fn get(
    State(state): State<Arc<AppState>>,
    Path(token): Path<String>,
    req: Request<Body>,
) -> Response {
    let Some(id) = verify_token(&state.cfg.auth_token, &token, Mode::Download) else {
        return json_error(StatusCode::FORBIDDEN, "invalid or expired download token");
    };
    let entry = {
        let idx = state.index.lock().await;
        idx.find_entry(id).cloned()
    };
    let Some(entry) = entry else {
        return json_error(StatusCode::NOT_FOUND, "cache entry not found");
    };
    let key = state.store.archive_key(id);
    let range = req
        .headers()
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok());

    match range {
        None => {
            let out = match state.store.get_object(&key).await {
                Ok(Some(o)) => o,
                Ok(None) => return json_error(StatusCode::NOT_FOUND, "archive not found"),
                Err(e) => {
                    tracing::error!("failed to read archive for entry {id}: {e}");
                    return json_error(StatusCode::INTERNAL_SERVER_ERROR, "internal error");
                }
            };
            let etag = out.e_tag().map(str::to_string);
            let modified = out.last_modified().map(|d| {
                d.fmt(aws_smithy_types::date_time::Format::HttpDate)
                    .unwrap_or_default()
            });
            let body = out.body.into_inner();
            let mut response = Response::new(Body::new(body));
            let headers = response.headers_mut();
            headers.insert(header::CONTENT_LENGTH, hv(&entry.size.to_string()));
            headers.insert(header::CONTENT_TYPE, hv("application/octet-stream"));
            headers.insert(header::ACCEPT_RANGES, hv("bytes"));
            if let Some(etag) = etag {
                headers.insert(header::ETAG, hv(&etag));
            }
            if let Some(modified) = modified {
                headers.insert(header::LAST_MODIFIED, hv(&modified));
            }
            response
        }
        Some(range_header) => match parse_range(range_header, entry.size) {
            Ok(Some((start, end))) => {
                let range = format!("bytes={start}-{end}");
                let out = match state.store.get_range(&key, &range).await {
                    Ok(Some(o)) => o,
                    Ok(None) => return json_error(StatusCode::NOT_FOUND, "archive not found"),
                    Err(e) => {
                        tracing::error!("failed to read archive range for entry {id}: {e}");
                        return json_error(StatusCode::INTERNAL_SERVER_ERROR, "internal error");
                    }
                };
                let length = out.content_length().map(|l| l.to_string());
                let content_range = out.content_range().map(str::to_string);
                let body = out.body.into_inner();
                let mut response = Response::new(Body::new(body));
                let headers = response.headers_mut();
                // Pass through S3's own lengths so the declared range always
                // matches what the client requested.
                if let Some(len) = length {
                    headers.insert(header::CONTENT_LENGTH, hv(&len));
                }
                if let Some(cr) = content_range {
                    headers.insert(header::CONTENT_RANGE, hv(&cr));
                }
                headers.insert(header::CONTENT_TYPE, hv("application/octet-stream"));
                *response.status_mut() = StatusCode::PARTIAL_CONTENT;
                response
            }
            Ok(None) => (
                StatusCode::RANGE_NOT_SATISFIABLE,
                [(header::CONTENT_RANGE, format!("bytes */{}", entry.size))],
            )
                .into_response(),
            Err(()) => json_error(StatusCode::BAD_REQUEST, "unsupported Range header"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> String {
        "test-secret".to_string()
    }

    #[test]
    fn signed_token_round_trip_succeeds() {
        println!("Verify that a signed token passes round-trip validation");
        let token = sign(&key(), Mode::Download, 42, 3600, 1000);
        assert_eq!(verify(&key(), &token, 1000), Some((Mode::Download, 42)));
        assert_eq!(verify(&key(), &token, 4599), Some((Mode::Download, 42)));
    }

    #[test]
    fn expired_token_is_rejected() {
        println!("Verify that an expired token is rejected");
        let token = sign(&key(), Mode::Upload, 7, 60, 1000);
        assert_eq!(verify(&key(), &token, 1060), Some((Mode::Upload, 7)));
        assert_eq!(verify(&key(), &token, 1061), None);
    }

    #[test]
    fn tampered_tokens_and_garbage_are_rejected() {
        println!("Verify that tampered tokens and garbage are rejected");
        let token = sign(&key(), Mode::Upload, 7, 60, 1000);
        let mut bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(&token)
            .unwrap();
        bytes[10] ^= 1; // Flip one bit of the id
        let tampered = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&bytes);
        assert_eq!(verify(&key(), &tampered, 1050), None);
        assert_eq!(verify(&key(), "not-a-token", 1050), None);
        // A different key must not verify.
        assert_eq!(verify("other-secret", &token, 1050), None);
    }

    #[test]
    fn token_mode_is_fixed() {
        println!("Verify that an upload token cannot be used for download");
        let token = sign(&key(), Mode::Upload, 7, 60, 1000);
        assert_eq!(verify(&key(), &token, 1050), Some((Mode::Upload, 7)));
        let (mode, _) = verify(&key(), &token, 1050).unwrap();
        assert_ne!(mode, Mode::Download);
    }

    #[test]
    fn range_header_parsing_is_correct() {
        println!("Verify Range header parsing");
        assert_eq!(parse_range("bytes=0-99", 1000), Ok(Some((0, 99))));
        assert_eq!(parse_range("bytes=100-199", 1000), Ok(Some((100, 199))));
        assert_eq!(parse_range("bytes=0-", 1000), Ok(Some((0, 999))));
        assert_eq!(parse_range("bytes=990-2000", 1000), Ok(Some((990, 999))));
        assert_eq!(parse_range("bytes=1000-1100", 1000), Ok(None));
        assert_eq!(parse_range("bytes=100-50", 1000), Ok(None));
        assert_eq!(parse_range("bytes=-100", 1000), Err(()));
        assert_eq!(parse_range("bytes=0-1,3-4", 1000), Err(()));
        assert_eq!(parse_range("0-99", 1000), Err(()));
        assert_eq!(parse_range("bytes=5", 1000), Err(()));
        assert_eq!(parse_range("bytes=a-b", 1000), Err(()));
        assert_eq!(parse_range("bytes=0-1", 0), Ok(None));
    }

    #[test]
    fn blocklist_xml_parsing_is_correct() {
        println!("Verify blocklist XML parsing");
        let xml = r#"<?xml version="1.0" encoding="utf-8"?><BlockList><Latest>YmxvY2stMQ==</Latest><Latest>YmxvY2stMg==</Latest></BlockList>"#;
        assert_eq!(
            parse_blocklist(xml.as_bytes()).unwrap(),
            vec!["YmxvY2stMQ==", "YmxvY2stMg=="]
        );
        let single = r#"<BlockList><Latest>YWJj</Latest></BlockList>"#;
        assert_eq!(parse_blocklist(single.as_bytes()).unwrap(), vec!["YWJj"]);
        // Malformed XML must fail.
        assert!(parse_blocklist(b"<BlockList><Latest>").is_err());
        assert!(parse_blocklist(b"not xml").is_err());
        // A block list without entries is empty.
        assert!(parse_blocklist(b"<BlockList></BlockList>")
            .unwrap()
            .is_empty());
    }
}
