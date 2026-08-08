//! Twirp CacheService (v2) at /twirp/github.actions.results.api.v1.CacheService.
//! Accepts both the JSON and the protobuf wire format used by @actions/cache;
//! responses mirror the request content type. Logical misses and conflicts are
//! ok:false over HTTP 200, exactly like the real service.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::State;
use axum::http::{header, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use prost::Message;
use serde::Serialize;

use crate::blob::{self, Mode, UPLOAD_URL_TTL_SECS};
use crate::index::{self, validate_key, validate_version};
use crate::{unix_now, AppState};

// ---------------------------------------------------------------------------
// Protobuf messages (fields match results/api/v1/cache.proto)
// ---------------------------------------------------------------------------

#[derive(Clone, PartialEq, prost::Message, Serialize)]
pub struct CacheScope {
    #[prost(string, tag = "1")]
    pub scope: String,
    #[prost(int64, tag = "2")]
    pub permission: i64,
}

#[derive(Clone, PartialEq, prost::Message, Serialize)]
pub struct CacheMetadata {
    #[prost(int64, tag = "1")]
    pub repository_id: i64,
    #[prost(message, repeated, tag = "2")]
    pub scope: Vec<CacheScope>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct CreateCacheEntryRequest {
    #[prost(message, tag = "1")]
    pub metadata: Option<CacheMetadata>,
    #[prost(string, tag = "2")]
    pub key: String,
    #[prost(string, tag = "3")]
    pub version: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct FinalizeCacheEntryUploadRequest {
    #[prost(message, tag = "1")]
    pub metadata: Option<CacheMetadata>,
    #[prost(string, tag = "2")]
    pub key: String,
    #[prost(int64, tag = "3")]
    pub size_bytes: i64,
    #[prost(string, tag = "4")]
    pub version: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct GetCacheEntryDownloadURLRequest {
    #[prost(message, tag = "1")]
    pub metadata: Option<CacheMetadata>,
    #[prost(string, tag = "2")]
    pub key: String,
    #[prost(string, repeated, tag = "3")]
    pub restore_keys: Vec<String>,
    #[prost(string, tag = "4")]
    pub version: String,
}

#[derive(Clone, PartialEq, prost::Message, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateCacheEntryResponse {
    #[prost(bool, tag = "1")]
    pub ok: bool,
    #[prost(string, tag = "2")]
    pub signed_upload_url: String,
    #[prost(string, tag = "3")]
    pub message: String,
}

#[derive(Clone, PartialEq, prost::Message, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FinalizeCacheEntryUploadResponse {
    #[prost(bool, tag = "1")]
    pub ok: bool,
    #[prost(int64, tag = "2")]
    pub entry_id: i64,
    #[prost(string, tag = "3")]
    pub message: String,
}

#[derive(Clone, PartialEq, prost::Message, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GetCacheEntryDownloadURLResponse {
    #[prost(bool, tag = "1")]
    pub ok: bool,
    #[prost(string, tag = "2")]
    pub signed_download_url: String,
    #[prost(string, tag = "3")]
    pub matched_key: String,
}

// ---------------------------------------------------------------------------
// Shared plumbing
// ---------------------------------------------------------------------------

fn is_protobuf(content_type: &str) -> bool {
    content_type.starts_with("application/protobuf")
}

fn is_json(content_type: &str) -> bool {
    content_type.starts_with("application/json")
}

fn twirp_error(status: StatusCode, code: &str, msg: &str) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        serde_json::json!({ "code": code, "msg": msg }).to_string(),
    )
        .into_response()
}

fn respond<T: Message + Serialize>(content_type: &str, message: &T) -> Response {
    if is_protobuf(content_type) {
        (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/protobuf")],
            message.encode_to_vec(),
        )
            .into_response()
    } else {
        let mut value = serde_json::to_value(message).unwrap_or_default();
        // Protobuf JSON represents int64 values as strings for JavaScript safety.
        if let Some(entry_id) = value.get("entryId").and_then(|v| v.as_i64()) {
            if let Some(object) = value.as_object_mut() {
                object.insert("entryId".into(), serde_json::json!(entry_id.to_string()));
            }
        }
        (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/json")],
            value.to_string(),
        )
            .into_response()
    }
}

/// Extract the v2 request fields from the JSON form. Accepts the protobuf JSON
/// lowerCamelCase names used by @actions/cache (`sizeBytes`, `restoreKeys`)
/// and the snake_case aliases. size_bytes is a JSON string because protobuf
/// int64 maps to string.
fn extract_json_fields(
    value: &serde_json::Value,
    want_size: bool,
) -> Result<(String, String, Vec<String>, Option<u64>), String> {
    let key = value
        .get("key")
        .and_then(|v| v.as_str())
        .ok_or("missing key")?
        .to_string();
    let version = value
        .get("version")
        .and_then(|v| v.as_str())
        .ok_or("missing version")?
        .to_string();
    let mut restore_keys = Vec::new();
    if let Some(arr) = value
        .get("restore_keys")
        .or_else(|| value.get("restoreKeys"))
        .and_then(|v| v.as_array())
    {
        for item in arr {
            restore_keys.push(
                item.as_str()
                    .ok_or("restore_keys must be strings")?
                    .to_string(),
            );
        }
    }
    let size_bytes = if want_size {
        match value.get("size_bytes").or_else(|| value.get("sizeBytes")) {
            Some(serde_json::Value::String(s)) => Some(
                s.parse::<u64>()
                    .map_err(|_| "size_bytes must be a non-negative integer")?,
            ),
            Some(serde_json::Value::Number(n)) => Some(
                n.as_u64()
                    .ok_or("size_bytes must be a non-negative integer")?,
            ),
            Some(_) => return Err("size_bytes must be a string or number".into()),
            None => None,
        }
    } else {
        None
    };
    Ok((key, version, restore_keys, size_bytes))
}

async fn read_twirp_body(body: Body) -> Result<Vec<u8>, Response> {
    const MAX: usize = 2 * 1024 * 1024;
    match axum::body::to_bytes(body, MAX).await {
        Ok(bytes) => Ok(bytes.to_vec()),
        Err(e) => {
            tracing::warn!("failed to read twirp body: {e}");
            Err(twirp_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "resource_exhausted",
                "request body too large",
            ))
        }
    }
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

pub async fn create_cache_entry(
    State(state): State<Arc<AppState>>,
    req: Request<Body>,
) -> Response {
    let content_type = req
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let body = match read_twirp_body(req.into_body()).await {
        Ok(b) => b,
        Err(r) => return r,
    };
    let (key, version) = if is_protobuf(&content_type) {
        let parsed = match CreateCacheEntryRequest::decode(&body[..]) {
            Ok(p) => p,
            Err(_) => {
                return twirp_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_argument",
                    "malformed protobuf request",
                )
            }
        };
        (parsed.key, parsed.version)
    } else if is_json(&content_type) {
        let value: serde_json::Value = match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(_) => {
                return twirp_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_argument",
                    "malformed JSON request",
                )
            }
        };
        match extract_json_fields(&value, false) {
            Ok((key, version, _, _)) => (key, version),
            Err(msg) => return twirp_error(StatusCode::BAD_REQUEST, "invalid_argument", &msg),
        }
    } else {
        return twirp_error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "bad_route",
            "unsupported content type",
        );
    };
    if !validate_key(&key) || !validate_version(&version) {
        return twirp_error(
            StatusCode::BAD_REQUEST,
            "invalid_argument",
            "invalid key or version",
        );
    }
    match index::reserve(&state, &key, &version, "v2", None).await {
        Ok(id) => {
            let url = format!(
                "{}/artifacts/{}",
                state.cfg.public_url.trim_end_matches('/'),
                blob::sign(
                    &state.cfg.auth_token,
                    Mode::Upload,
                    id,
                    UPLOAD_URL_TTL_SECS,
                    unix_now()
                )
            );
            respond(
                &content_type,
                &CreateCacheEntryResponse {
                    ok: true,
                    signed_upload_url: url,
                    message: String::new(),
                },
            )
        }
        Err(index::ReserveError::Conflict) => respond(
            &content_type,
            &CreateCacheEntryResponse {
                ok: false,
                signed_upload_url: String::new(),
                message: format!(
                    "cache entry with key '{key}' and version '{version}' already exists or is being created"
                ),
            },
        ),
        Err(index::ReserveError::TooLarge) => respond(
            &content_type,
            &CreateCacheEntryResponse {
                ok: false,
                signed_upload_url: String::new(),
                message: "cache size exceeds the configured maximum".into(),
            },
        ),
        Err(index::ReserveError::Internal) => {
            twirp_error(StatusCode::INTERNAL_SERVER_ERROR, "internal", "internal error")
        }
    }
}

pub async fn finalize_cache_entry_upload(
    State(state): State<Arc<AppState>>,
    req: Request<Body>,
) -> Response {
    let content_type = req
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let body = match read_twirp_body(req.into_body()).await {
        Ok(b) => b,
        Err(r) => return r,
    };
    let (key, version, size_bytes) = if is_protobuf(&content_type) {
        let parsed = match FinalizeCacheEntryUploadRequest::decode(&body[..]) {
            Ok(p) => p,
            Err(_) => {
                return twirp_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_argument",
                    "malformed protobuf request",
                )
            }
        };
        if parsed.size_bytes < 0 {
            return twirp_error(
                StatusCode::BAD_REQUEST,
                "invalid_argument",
                "size_bytes must be non-negative",
            );
        }
        (parsed.key, parsed.version, parsed.size_bytes as u64)
    } else if is_json(&content_type) {
        let value: serde_json::Value = match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(_) => {
                return twirp_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_argument",
                    "malformed JSON request",
                )
            }
        };
        match extract_json_fields(&value, true) {
            Ok((key, version, _, size)) => {
                let size = size.ok_or_else(|| {
                    twirp_error(
                        StatusCode::BAD_REQUEST,
                        "invalid_argument",
                        "missing size_bytes",
                    )
                });
                match size {
                    Ok(s) => (key, version, s),
                    Err(r) => return r,
                }
            }
            Err(msg) => return twirp_error(StatusCode::BAD_REQUEST, "invalid_argument", &msg),
        }
    } else {
        return twirp_error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "bad_route",
            "unsupported content type",
        );
    };
    if !validate_key(&key) || !validate_version(&version) {
        return twirp_error(
            StatusCode::BAD_REQUEST,
            "invalid_argument",
            "invalid key or version",
        );
    }
    match index::finalize_v2(&state, &key, &version, size_bytes).await {
        Ok(entry_id) => respond(
            &content_type,
            &FinalizeCacheEntryUploadResponse {
                ok: true,
                entry_id: entry_id as i64,
                message: String::new(),
            },
        ),
        Err(index::FinalizeError::NoReservation) => respond(
            &content_type,
            &FinalizeCacheEntryUploadResponse {
                ok: false,
                entry_id: 0,
                message: "no active reservation for this key and version".into(),
            },
        ),
        Err(index::FinalizeError::NoUpload) => respond(
            &content_type,
            &FinalizeCacheEntryUploadResponse {
                ok: false,
                entry_id: 0,
                message: "no upload found for this reservation".into(),
            },
        ),
        Err(index::FinalizeError::SizeMismatch) => respond(
            &content_type,
            &FinalizeCacheEntryUploadResponse {
                ok: false,
                entry_id: 0,
                message: "uploaded archive size does not match the declared size".into(),
            },
        ),
        Err(index::FinalizeError::Internal) => twirp_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            "internal error",
        ),
    }
}

pub async fn get_cache_entry_download_url(
    State(state): State<Arc<AppState>>,
    req: Request<Body>,
) -> Response {
    let content_type = req
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let body = match read_twirp_body(req.into_body()).await {
        Ok(b) => b,
        Err(r) => return r,
    };
    let (key, version, restore_keys) = if is_protobuf(&content_type) {
        let parsed = match GetCacheEntryDownloadURLRequest::decode(&body[..]) {
            Ok(p) => p,
            Err(_) => {
                return twirp_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_argument",
                    "malformed protobuf request",
                )
            }
        };
        (parsed.key, parsed.version, parsed.restore_keys)
    } else if is_json(&content_type) {
        let value: serde_json::Value = match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(_) => {
                return twirp_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_argument",
                    "malformed JSON request",
                )
            }
        };
        match extract_json_fields(&value, false) {
            Ok((key, version, restore_keys, _)) => (key, version, restore_keys),
            Err(msg) => return twirp_error(StatusCode::BAD_REQUEST, "invalid_argument", &msg),
        }
    } else {
        return twirp_error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "bad_route",
            "unsupported content type",
        );
    };
    if !validate_key(&key)
        || !validate_version(&version)
        || !restore_keys.iter().all(|k| validate_key(k))
        || restore_keys.len() > 10
    {
        return twirp_error(
            StatusCode::BAD_REQUEST,
            "invalid_argument",
            "invalid key, version or restore keys",
        );
    }
    let hit = {
        let idx = state.index.lock().await;
        index::lookup(&idx.entries, &key, &restore_keys, &version).cloned()
    };
    let Some(hit) = hit else {
        // Logical miss: ok:false with HTTP 200.
        return respond(
            &content_type,
            &GetCacheEntryDownloadURLResponse {
                ok: false,
                signed_download_url: String::new(),
                matched_key: String::new(),
            },
        );
    };
    let url = format!(
        "{}/artifacts/{}",
        state.cfg.public_url.trim_end_matches('/'),
        blob::sign(
            &state.cfg.auth_token,
            Mode::Download,
            hit.id,
            crate::blob::DOWNLOAD_URL_TTL_SECS,
            unix_now()
        )
    );
    respond(
        &content_type,
        &GetCacheEntryDownloadURLResponse {
            ok: true,
            signed_download_url: url,
            matched_key: hit.key,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn プロトバフリクエストのラウンドトリップが壊れない() {
        println!("プロトバフのリクエストエンコードとデコードが内容を保持することを確認");
        let req = CreateCacheEntryRequest {
            metadata: Some(CacheMetadata {
                repository_id: 42,
                scope: vec![CacheScope {
                    scope: "main".into(),
                    permission: 1,
                }],
            }),
            key: "linux-abc".into(),
            version: "v1".into(),
        };
        let decoded = CreateCacheEntryRequest::decode(&*req.encode_to_vec()).unwrap();
        assert_eq!(decoded, req);

        let fin = FinalizeCacheEntryUploadRequest {
            metadata: None,
            key: "k".into(),
            size_bytes: 1234,
            version: "v".into(),
        };
        let decoded_fin = FinalizeCacheEntryUploadRequest::decode(&*fin.encode_to_vec()).unwrap();
        assert_eq!(decoded_fin.size_bytes, 1234);

        let dl = GetCacheEntryDownloadURLRequest {
            metadata: None,
            key: "k".into(),
            restore_keys: vec!["a".into(), "b".into()],
            version: "v".into(),
        };
        let decoded_dl = GetCacheEntryDownloadURLRequest::decode(&*dl.encode_to_vec()).unwrap();
        assert_eq!(decoded_dl.restore_keys, vec!["a", "b"]);
    }

    #[test]
    fn レスポンスのjsonはlower_camel_case名で出力される() {
        println!("レスポンスJSONがprotobuf JSONのlowerCamelCase名で出力されることを確認");
        let resp = CreateCacheEntryResponse {
            ok: true,
            signed_upload_url: "https://example/artifacts/x".into(),
            message: String::new(),
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["ok"], true);
        assert_eq!(json["signedUploadUrl"], "https://example/artifacts/x");
        assert!(json.get("signed_upload_url").is_none());
        let resp2 = GetCacheEntryDownloadURLResponse {
            ok: false,
            signed_download_url: String::new(),
            matched_key: String::new(),
        };
        let json2 = serde_json::to_value(&resp2).unwrap();
        assert_eq!(json2["ok"], false);
        assert_eq!(json2["matchedKey"], "");
        assert_eq!(json2["signedDownloadUrl"], "");
    }

    #[tokio::test]
    async fn jsonレスポンスのentry_idは文字列になる() {
        println!("JSONレスポンスのentryIdがint64のため文字列として出力されることを確認");
        let resp = FinalizeCacheEntryUploadResponse {
            ok: true,
            entry_id: 99,
            message: String::new(),
        };
        let response = respond("application/json", &resp);
        let bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["entryId"], "99");
        assert_eq!(value["ok"], true);
    }

    #[test]
    fn jsonリクエストはcamel_caseエイリアスを受け付ける() {
        println!("JSONリクエストがlowerCamelCaseのエイリアスを受け付けることを確認");
        let value: serde_json::Value = serde_json::json!({
            "key": "k",
            "version": "v",
            "restoreKeys": ["a", "b"],
            "sizeBytes": "1024"
        });
        let (key, version, restore, size) = extract_json_fields(&value, true).unwrap();
        assert_eq!(key, "k");
        assert_eq!(version, "v");
        assert_eq!(restore, vec!["a", "b"]);
        assert_eq!(size, Some(1024));
    }

    #[test]
    fn jsonリクエストのフィールド抽出が型違いを拒否する() {
        println!("JSONリクエストのフィールド抽出が欠落や型違いを拒否することを確認");
        let value: serde_json::Value = serde_json::json!({
            "metadata": {"repository_id": "7"},
            "key": "k",
            "version": "v",
            "size_bytes": "1024"
        });
        let (key, version, restore, size) = extract_json_fields(&value, true).unwrap();
        assert_eq!(key, "k");
        assert_eq!(version, "v");
        assert!(restore.is_empty());
        assert_eq!(size, Some(1024));

        let bad: serde_json::Value = serde_json::json!({"key": "k"});
        assert!(extract_json_fields(&bad, false).is_err());
        let bad2: serde_json::Value = serde_json::json!({"key": 1, "version": "v"});
        assert!(extract_json_fields(&bad2, false).is_err());
        let bad3: serde_json::Value =
            serde_json::json!({"key": "k", "version": "v", "size_bytes": "-5"});
        assert!(extract_json_fields(&bad3, true).is_err());
    }

    #[test]
    fn レスポンスのプロトバフエンコードが成功する() {
        println!("レスポンスをプロトバフでエンコードできることを確認");
        let resp = FinalizeCacheEntryUploadResponse {
            ok: true,
            entry_id: 99,
            message: "ok".into(),
        };
        let bytes = resp.encode_to_vec();
        assert!(!bytes.is_empty());
        let decoded = FinalizeCacheEntryUploadResponse::decode(&bytes[..]).unwrap();
        assert_eq!(decoded.entry_id, 99);
        assert!(decoded.ok);
    }
}
