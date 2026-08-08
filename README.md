# gha-cache-proxy

[日本語](README.ja.md)

## Overview

`gha-cache-proxy` is a Rust service that implements the GitHub Actions cache v1 REST API and v2 Twirp API while storing cache archives in S3-compatible object storage.

The proxy connects only to the configured S3 endpoint.
It does not call the GitHub control plane or any upstream cache service, so cache operations do not consume GitHub API rate limits.

## Features

- GitHub Actions cache v1 REST endpoints.
- GitHub Actions cache v2 Twirp endpoints with JSON and protobuf support.
- Primary-key and restore-key matching with version isolation.
- Azure Blob-compatible signed URLs for streaming archive upload and download.
- Single-object uploads, Azure block uploads, block lists, `HEAD`, `GET`, and single-range downloads.
- S3-persisted metadata and reservations that survive process restarts.
- Bearer authentication for control APIs and expiring HMAC authentication for blob URLs.
- Per-archive and per-reservation staging size limits.

## Requirements

- Docker Engine with the Docker Compose plugin.
- An S3-compatible object store such as MinIO.
- A runner or network proxy that can route cache service requests to `PUBLIC_URL`.
- Rust 1.97.1 or later when building outside Docker.

## Installation

The included Compose file starts the proxy and a persistent local MinIO instance.

```sh
docker compose up -d --build
curl -fsS http://localhost:8080/healthz
```

Run the protocol smoke test against the running services:

```sh
scripts/smoke.sh
docker compose restart proxy
scripts/smoke.sh --after-restart
```

Remove the containers and the local MinIO volume when finished:

```sh
docker compose down -v
```

The Compose file uses a 10 MiB cache limit so the smoke test can check rejection.
Override `MAX_CACHE_SIZE_BYTES` with the production limit before deploying.

## Configuration

The proxy requires these environment variables when run directly:

| Variable | Description |
| --- | --- |
| `PUBLIC_URL` | URL reachable by the runner; used to create signed blob URLs. |
| `AUTH_TOKEN` | Bearer token accepted by the control APIs. |
| `CACHE_NAMESPACE` | Namespace that isolates cache metadata and objects. |
| `S3_ENDPOINT` | S3-compatible endpoint such as `https://minio.example:9000`. |
| `S3_BUCKET` | Bucket used for the namespace. |
| `S3_REGION` | S3 region such as `us-east-1`. |
| `S3_ACCESS_KEY_ID` | S3 access key. |
| `S3_SECRET_ACCESS_KEY` | S3 secret key. |

Optional variables:

| Variable | Default | Description |
| --- | --- | --- |
| `LISTEN_ADDR` | `0.0.0.0:8080` | Listen address. |
| `MAX_CACHE_SIZE_BYTES` | `10737418240` | Maximum archive and staging size in bytes (10 GiB). |

Inject tokens and S3 credentials through the process environment.
Do not commit them to the repository or put them in signed URLs.

## GitHub Actions integration

The runner must send cache control requests to `PUBLIC_URL` and authenticate them with `AUTH_TOKEN`.
Use the cache service URL for v1 and the results service URL for v2 according to the runner integration:

```sh
ACTIONS_CACHE_URL=https://cache.example.internal
ACTIONS_RESULTS_URL=https://cache.example.internal
```

The proxy accepts the control-plane requests with `Authorization: Bearer <AUTH_TOKEN>`.
Blob requests use the signed URLs returned by the v1 or v2 API and do not need the Bearer header.

## API surface

| Method and path | Authentication | Purpose |
| --- | --- | --- |
| `GET /healthz` | None | Liveness endpoint. |
| `POST /_apis/artifactcache/caches` | Bearer | Reserve a v1 cache. |
| `PATCH /_apis/artifactcache/caches/{cacheId}` | Bearer | Upload a v1 archive chunk. |
| `POST /_apis/artifactcache/caches/{cacheId}` | Bearer | Commit a v1 cache. |
| `GET /_apis/artifactcache/cache?keys=&version=` | Bearer | Look up a v1 cache. |
| `GET /_apis/artifactcache/caches?key=` | Bearer | List v1 cache entries for diagnostics. |
| `POST /twirp/github.actions.results.api.v1.CacheService/CreateCacheEntry` | Bearer | Reserve a v2 cache. |
| `POST /twirp/github.actions.results.api.v1.CacheService/FinalizeCacheEntryUpload` | Bearer | Finalize a v2 cache. |
| `POST /twirp/github.actions.results.api.v1.CacheService/GetCacheEntryDownloadURL` | Bearer | Look up a v2 cache. |
| `PUT /artifacts/{token}` | Signed URL | Upload an archive or block. |
| `PUT /artifacts/{token}?comp=blocklist` | Signed URL | Assemble an Azure block upload. |
| `HEAD /artifacts/{token}` | Signed URL | Read archive metadata. |
| `GET /artifacts/{token}` | Signed URL | Download an archive or a single range. |

v2 JSON responses use protobuf lowerCamelCase names such as `signedUploadUrl`, `matchedKey`, and `entryId`.
The JSON representation of the int64 `entryId` is a string.
The proxy accepts both snake_case and lowerCamelCase JSON request fields.

## Operational boundaries

- Run one proxy replica per shared `CACHE_NAMESPACE` and bucket.
- Metadata and reservations are stored in one S3 JSON index protected by a process-local lock.
- The proxy does not provide eviction, garbage collection, or automatic cleanup of abandoned staging objects.
- Cache misses return HTTP 204 for v1 and `ok: false` for v2.
- Signed blob URLs expire and contain no S3 credentials.
- Range support covers one `start-end` or open-ended range; suffix and multi-range requests are not supported.

## Build and validation

Build the release image:

```sh
docker build -t gha-cache-proxy .
```

For a local Rust toolchain:

```sh
cargo fmt --check
cargo test --locked
cargo clippy --locked --all-targets --all-features -- -D warnings
```

The smoke test uses only the local proxy, MinIO, `curl`, and Python's standard library.
It does not send requests to GitHub.
