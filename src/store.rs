//! Thin S3 wrapper for the dedicated cache bucket.

use std::error::Error;

use aws_sdk_s3::config::{Credentials, Region, RequestChecksumCalculation};
use aws_sdk_s3::operation::get_object::GetObjectOutput;
use aws_sdk_s3::operation::head_object::HeadObjectOutput;
use aws_sdk_s3::types::{Delete, Object, ObjectIdentifier};
use aws_sdk_s3::Client as S3Client;
use futures_util::StreamExt;
use http_body_util::{BodyExt, Limited};

pub use aws_sdk_s3::primitives::ByteStream;

use crate::Config;

/// Makes any `Body<Data = Bytes> + Send` Sync-safe for the S3 client. Polls are
/// serialized through a std mutex; the guard never spans an await because
/// `poll_frame` itself never blocks, so this is sound and contention-free.
struct SyncBody<B> {
    inner: std::sync::Mutex<B>,
}

impl<B> SyncBody<B> {
    fn new(inner: B) -> Self {
        Self {
            inner: std::sync::Mutex::new(inner),
        }
    }
}

impl<B> http_body::Body for SyncBody<B>
where
    B: http_body::Body<Data = bytes::Bytes> + Send + Unpin,
{
    type Data = bytes::Bytes;
    type Error = B::Error;

    fn poll_frame(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        let mut inner = self.inner.lock().expect("sync body lock poisoned");
        std::pin::Pin::new(&mut *inner).poll_frame(cx)
    }
}

pub type BoxError = Box<dyn Error + Send + Sync>;
pub type S3Result<T> = Result<T, BoxError>;

/// Static credentials only; ambient credential discovery is deliberately
/// disabled so the proxy never makes hidden metadata/control-plane requests.
pub fn build_client(cfg: &Config) -> S3Client {
    let creds = Credentials::new(
        cfg.s3_access_key_id.clone(),
        cfg.s3_secret_access_key.clone(),
        None,
        None,
        "static",
    );
    let conf = aws_sdk_s3::config::Builder::new()
        .credentials_provider(creds)
        .region(Region::new(cfg.s3_region.clone()))
        .endpoint_url(&cfg.s3_endpoint)
        .force_path_style(true)
        .request_checksum_calculation(RequestChecksumCalculation::WhenRequired)
        .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest())
        .build();
    S3Client::from_conf(conf)
}

#[derive(Clone)]
pub struct Store {
    client: S3Client,
    bucket: String,
}

impl Store {
    pub fn new(client: S3Client, bucket: String) -> Self {
        Self { client, bucket }
    }

    pub fn index_key(&self) -> String {
        "index.json".into()
    }
    pub fn chunk_key(&self, id: u64, offset: u64) -> String {
        format!("staging/{id}/chunks/{offset}")
    }
    pub fn block_key(&self, id: u64, block_id: &str) -> String {
        // Standard base64 block IDs can contain '/' and '+'; map them to URL-safe
        // characters so S3 gateways like SeaweedFS never treat '/' as a path separator.
        let safe: String = block_id
            .chars()
            .map(|c| match c {
                '+' => '-',
                '/' => '_',
                _ => c,
            })
            .collect();
        format!("staging/{id}/blocks/{safe}")
    }
    pub fn staging_blob_key(&self, id: u64) -> String {
        format!("staging/{id}/blob")
    }
    pub fn chunk_prefix(&self, id: u64) -> String {
        format!("staging/{id}/chunks/")
    }
    pub fn block_prefix(&self, id: u64) -> String {
        format!("staging/{id}/blocks/")
    }
    pub fn staging_prefix(&self, id: u64) -> String {
        format!("staging/{id}/")
    }
    pub fn archive_key(&self, id: u64) -> String {
        format!("archives/{id}")
    }

    /// Stream a body of a known length into an object.
    pub async fn put(&self, key: &str, len: u64, body: ByteStream) -> S3Result<()> {
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .content_length(len as i64)
            .body(body)
            .send()
            .await?;
        Ok(())
    }

    pub async fn put_bytes(&self, key: &str, bytes: Vec<u8>) -> S3Result<()> {
        let len = bytes.len() as u64;
        self.put(key, len, ByteStream::from(bytes)).await
    }

    pub async fn get_object(&self, key: &str) -> S3Result<Option<GetObjectOutput>> {
        match self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
        {
            Ok(out) => Ok(Some(out)),
            Err(aws_sdk_s3::error::SdkError::ServiceError(e)) if e.err().is_no_such_key() => {
                Ok(None)
            }
            Err(e) => Err(e.into()),
        }
    }

    pub async fn get_range(&self, key: &str, range: &str) -> S3Result<Option<GetObjectOutput>> {
        match self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .range(range)
            .send()
            .await
        {
            Ok(out) => Ok(Some(out)),
            Err(aws_sdk_s3::error::SdkError::ServiceError(e)) if e.err().is_no_such_key() => {
                Ok(None)
            }
            Err(e) => Err(e.into()),
        }
    }

    pub async fn get_bytes(&self, key: &str) -> S3Result<Option<Vec<u8>>> {
        match self.get_object(key).await? {
            Some(out) => {
                let bytes = out.body.collect().await?.into_bytes().to_vec();
                Ok(Some(bytes))
            }
            None => Ok(None),
        }
    }

    pub async fn head(&self, key: &str) -> S3Result<Option<HeadObjectOutput>> {
        match self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
        {
            Ok(out) => Ok(Some(out)),
            Err(aws_sdk_s3::error::SdkError::ServiceError(e)) if e.err().is_not_found() => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub async fn list(&self, prefix: &str) -> S3Result<Vec<Object>> {
        let mut out = Vec::new();
        let mut paginator = self
            .client
            .list_objects_v2()
            .bucket(&self.bucket)
            .prefix(prefix)
            .into_paginator()
            .send();
        while let Some(page) = paginator.next().await {
            let page = page?;
            if let Some(objects) = page.contents {
                out.extend(objects);
            }
        }
        Ok(out)
    }

    pub async fn delete_many(&self, keys: Vec<String>) -> S3Result<()> {
        for batch in keys.chunks(1000) {
            let objects = batch
                .iter()
                .map(|k| ObjectIdentifier::builder().key(k).build())
                .collect::<Result<Vec<_>, _>>()?;
            let req = Delete::builder().set_objects(Some(objects)).build()?;
            self.client
                .delete_objects()
                .bucket(&self.bucket)
                .delete(req)
                .send()
                .await?;
        }
        Ok(())
    }

    /// Best-effort bulk delete; failures surface so callers can log them.
    pub async fn delete_prefix(&self, prefix: &str) -> S3Result<()> {
        let keys: Vec<String> = self
            .list(prefix)
            .await?
            .into_iter()
            .filter_map(|o| o.key)
            .collect();
        if !keys.is_empty() {
            self.delete_many(keys).await?;
        }
        Ok(())
    }

    /// Server-side copy within the bucket avoids proxying archive bytes.
    pub async fn copy(&self, src: &str, dst: &str) -> S3Result<()> {
        self.client
            .copy_object()
            .bucket(&self.bucket)
            .key(dst)
            .copy_source(format!("{}/{}", self.bucket, src))
            .send()
            .await?;
        Ok(())
    }

    /// Stream-concatenate objects into one destination object of a known total
    /// size. Never buffers archive bytes in memory; the size is verified after
    /// assembly so a short part cannot silently produce a corrupt archive.
    pub async fn concat(&self, dst: &str, total: u64, parts: Vec<(String, u64)>) -> S3Result<()> {
        let mut streams = Vec::with_capacity(parts.len());
        for (key, _) in &parts {
            let out = self
                .client
                .get_object()
                .bucket(&self.bucket)
                .key(key)
                .send()
                .await?;
            streams.push(out.body.into_inner().into_data_stream());
        }
        let chained = futures_util::stream::iter(streams)
            .flatten()
            .map(|item| item.map(http_body::Frame::data));
        let body = http_body_util::StreamBody::new(chained);
        if let Err(e) = self.put(dst, total, ByteStream::from_body_1_x(body)).await {
            let _ = self
                .client
                .delete_object()
                .bucket(&self.bucket)
                .key(dst)
                .send()
                .await;
            return Err(e);
        }
        match self.head(dst).await? {
            Some(h) if h.content_length() == Some(total as i64) => Ok(()),
            _ => {
                let _ = self
                    .client
                    .delete_object()
                    .bucket(&self.bucket)
                    .key(dst)
                    .send()
                    .await;
                Err("assembled archive size mismatch".into())
            }
        }
    }

    /// Wrap a request body so it can never exceed `limit` bytes while streaming
    /// into S3 (the axum body is not Sync, hence the SyncBody adapter).
    pub fn upload_stream(body: axum::body::Body, limit: u64) -> ByteStream {
        let limited = Limited::new(body, usize::try_from(limit).unwrap_or(usize::MAX));
        ByteStream::from_body_1_x(SyncBody::new(limited))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Config;

    fn test_client() -> S3Client {
        // No network is touched: the client is only used for key construction.
        let cfg = Config {
            listen_addr: "0.0.0.0:8080".into(),
            public_url: "http://localhost:8080".into(),
            auth_token: "t".into(),
            require_http_auth: true,
            s3_endpoint: "http://localhost:8333".into(),
            s3_bucket: "bucket".into(),
            s3_region: "us-east-1".into(),
            s3_access_key_id: "a".into(),
            s3_secret_access_key: "b".into(),
            max_cache_size_bytes: 1024,
            max_total_cache_size_bytes: 0,
            tls: None,
        };
        build_client(&cfg)
    }

    #[test]
    fn keys_use_the_dedicated_bucket_layout() {
        println!("Verify that generated keys use the dedicated bucket layout");
        let store = Store::new(test_client(), "bucket".into());
        assert_eq!(store.index_key(), "index.json");
        assert_eq!(store.chunk_key(7, 0), "staging/7/chunks/0");
        assert_eq!(store.block_key(7, "abc"), "staging/7/blocks/abc");
        assert_eq!(store.staging_blob_key(7), "staging/7/blob");
        assert_eq!(store.archive_key(7), "archives/7");
    }
}
