# gha-cache-proxy

[English](README.md)

## 概要

`gha-cache-proxy` は、GitHub Actions cache v1 の REST API と v2 の Twirp API を実装し、キャッシュアーカイブを S3 互換ストレージへ保存する Rust サービスです。

プロキシが接続する先は、設定した S3 エンドポイントだけです。
GitHub の control plane や upstream の cache service へは接続しないため、キャッシュ操作で GitHub API のレート制限を消費しません。

## 特徴

- GitHub Actions cache v1 の REST endpoint に対応。
- GitHub Actions cache v2 の Twirp endpoint に JSON と protobuf で対応。
- プライマリキーと restore-key による検索、および version による分離。
- Azure Blob 互換の署名付き URL によるアーカイブの streaming upload と download。
- 単発 upload、Azure block upload、blocklist、`HEAD`、`GET`、単一 range download に対応。
- metadata と予約状態を S3 に保存し、プロセス再起動後も復元。
- control API は Bearer 認証、blob URL は期限付き HMAC 認証。
- アーカイブ単位と予約単位の staging サイズ制限。

## Requirements

- Docker Engine と Docker Compose plugin。
- MinIO などの S3 互換 object storage。
- cache service のリクエストを `PUBLIC_URL` へ送れる runner または network proxy。
- Docker の外で build する場合は Rust 1.97.1 以上。

## インストール

同梱の Compose file は、プロキシと永続化されたローカル MinIO を起動します。

```sh
docker compose up -d --build
curl -fsS http://localhost:8080/healthz
```

起動中のサービスに対して protocol smoke test を実行します。

```sh
scripts/smoke.sh
docker compose restart proxy
scripts/smoke.sh --after-restart
```

終了時に container とローカル MinIO の volume を削除します。

```sh
docker compose down -v
```

Compose file は、超過時の拒否を smoke test で検証できるよう、キャッシュ上限を 10 MiB に設定しています。
本番環境では `MAX_CACHE_SIZE_BYTES` を実際の上限へ変更してください。

## 設定

直接起動する場合、以下の環境変数が必要です。

| 変数 | 説明 |
| --- | --- |
| `PUBLIC_URL` | runner から到達できる URL。署名付き blob URL の生成に使います。 |
| `AUTH_TOKEN` | control API が受け付ける Bearer token。 |
| `CACHE_NAMESPACE` | metadata と object を分離する namespace。 |
| `S3_ENDPOINT` | `https://minio.example:9000` などの S3 互換 endpoint。 |
| `S3_BUCKET` | namespace に使う bucket。 |
| `S3_REGION` | `us-east-1` などの S3 region。 |
| `S3_ACCESS_KEY_ID` | S3 access key。 |
| `S3_SECRET_ACCESS_KEY` | S3 secret key。 |

任意の環境変数は以下のとおりです。

| 変数 | 既定値 | 説明 |
| --- | --- | --- |
| `LISTEN_ADDR` | `0.0.0.0:8080` | listen address。 |
| `MAX_CACHE_SIZE_BYTES` | `10737418240` | アーカイブと staging の最大サイズ（10 GiB）。 |

token と S3 credential はプロセスの環境変数から渡してください。
repository に commit したり、署名付き URL に含めたりしないでください。

## GitHub Actions との接続

runner は cache control request を `PUBLIC_URL` へ送信し、`AUTH_TOKEN` と一致する Bearer token で認証する必要があります。
v1 では cache service URL、v2 では results service URL を runner の接続方法に合わせて設定します。

```sh
ACTIONS_CACHE_URL=https://cache.example.internal
ACTIONS_RESULTS_URL=https://cache.example.internal
```

control plane request には `Authorization: Bearer <AUTH_TOKEN>` を指定します。
blob request は v1 または v2 API が返す署名付き URL を使うため、Bearer header は必要ありません。

## API

| Method と path | 認証 | 用途 |
| --- | --- | --- |
| `GET /healthz` | なし | liveness endpoint。 |
| `POST /_apis/artifactcache/caches` | Bearer | v1 cache の予約。 |
| `PATCH /_apis/artifactcache/caches/{cacheId}` | Bearer | v1 archive chunk の upload。 |
| `POST /_apis/artifactcache/caches/{cacheId}` | Bearer | v1 cache の commit。 |
| `GET /_apis/artifactcache/cache?keys=&version=` | Bearer | v1 cache の検索。 |
| `GET /_apis/artifactcache/caches?key=` | Bearer | v1 cache の診断用一覧。 |
| `POST /twirp/github.actions.results.api.v1.CacheService/CreateCacheEntry` | Bearer | v2 cache の予約。 |
| `POST /twirp/github.actions.results.api.v1.CacheService/FinalizeCacheEntryUpload` | Bearer | v2 cache の finalize。 |
| `POST /twirp/github.actions.results.api.v1.CacheService/GetCacheEntryDownloadURL` | Bearer | v2 cache の検索。 |
| `PUT /artifacts/{token}` | 署名付き URL | archive または block の upload。 |
| `PUT /artifacts/{token}?comp=blocklist` | 署名付き URL | Azure block upload の組み立て。 |
| `HEAD /artifacts/{token}` | 署名付き URL | archive metadata の取得。 |
| `GET /artifacts/{token}` | 署名付き URL | archive または単一 range の download。 |

v2 の JSON response は `signedUploadUrl`、`matchedKey`、`entryId` のような protobuf lowerCamelCase 名を使います。
int64 である `entryId` は JSON では文字列になります。
JSON request は snake_case と lowerCamelCase の両方を受け付けます。

## 運用上の境界

- 同じ `CACHE_NAMESPACE` と bucket を共有する proxy は 1 replica にしてください。
- metadata と予約状態は、process-local lock で保護した 1 個の S3 JSON index に保存します。
- eviction、garbage collection、放棄された staging object の自動削除は提供しません。
- cache miss は v1 では HTTP 204、v2 では `ok: false` で返します。
- 署名付き blob URL は期限付きで、S3 credential を含みません。
- Range は 1 個の `start-end` または open-ended range に対応します。
  suffix range と multi-range request には対応しません。

## Build と検証

release image を build します。

```sh
docker build -t gha-cache-proxy .
```

ローカルの Rust toolchain を使う場合は、以下を実行します。

```sh
cargo fmt --check
cargo test --locked
cargo clippy --locked --all-targets --all-features -- -D warnings
```

smoke test が使うのはローカルの proxy、MinIO、`curl`、Python 標準ライブラリだけです。
GitHub へはリクエストを送信しません。
