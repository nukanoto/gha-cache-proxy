# gha-cache-server

[English](README.md)

## 概要

`gha-cache-server` は、GitHub Actions cache v1 の REST API と v2 の Twirp API を実装し、キャッシュアーカイブを S3 互換ストレージへ保存する Rust サービスです。

プロキシが接続する先は、設定した S3 エンドポイントだけです。
GitHub の control plane や upstream の cache service へは接続しないため、キャッシュ操作で GitHub API のレート制限を消費しません。

## Requirements

- Docker Engine。
- 任意の S3 互換オブジェクトストレージのバケット（AWS S3、Cloudflare R2、MinIO、SeaweedFS など）。
- cache service のリクエストを `PUBLIC_URL` へ送れる runner または network proxy。
- Docker の外で build する場合は Rust 1.97.1 以上。

## インストール

`gha-cache-server` は、任意の専用 S3 互換バケットへキャッシュアーカイブとメタデータを保存します。
利用する S3 バケットの接続情報を渡して起動します。

```sh
docker run -d --name gha-cache-server \
  -p 8080:8080 \
  -e PUBLIC_URL=http://localhost:8080 \
  -e AUTH_TOKEN=dev-secret-token \
  -e S3_ENDPOINT=https://s3.example.com \
  -e S3_BUCKET=gha-cache \
  -e S3_REGION=us-east-1 \
  -e S3_ACCESS_KEY_ID=your-access-key \
  -e S3_SECRET_ACCESS_KEY=your-secret-key \
  ghcr.io/nukanoto/gha-cache-server:latest
```

サービスの起動を確認します。

```sh
curl -fsS http://localhost:8080/healthz
```

## 設定

直接起動する場合、以下の環境変数が必要です。

| 変数 | 説明 |
| --- | --- |
| `PUBLIC_URL` | runner から到達できる URL。署名付き blob URL の生成に使います。 |
| `AUTH_TOKEN` | control API が受け付ける Bearer token。 |
| `S3_ENDPOINT` | `https://s3.example.com` などの S3 互換 endpoint。 |
| `S3_BUCKET` | サーバー専用の S3 バケット。 |
| `S3_REGION` | `us-east-1` などの S3 region。 |
| `S3_ACCESS_KEY_ID` | S3 access key。 |
| `S3_SECRET_ACCESS_KEY` | S3 secret key。 |

任意の環境変数は以下のとおりです。

| 変数 | 既定値 | 説明 |
| --- | --- | --- |
| `LISTEN_ADDR` | `0.0.0.0:8080` | 平文 HTTP の listen address。 |
| `TLS_LISTEN_ADDR` | 未設定 | 内蔵 TLS 終端と非キャッシュ `results-receiver` 通信の GitHub 中継を有効化（例: `0.0.0.0:443`）。 |
| `TLS_CA_CERT_OUT` | 未設定 | 自動生成したローカル CA の公開証明書を書き出すパス（例: `/runner-ca/root.crt`）。 |
| `TLS_DATA_DIR` | 未設定 | 生成したローカル CA の鍵・証明書を再起動後も保持するディレクトリ。 |
| `MAX_CACHE_SIZE_BYTES` | `10737418240` | アーカイブと staging の最大サイズ（10 GiB）。 |
| `MAX_TOTAL_CACHE_SIZE_BYTES` | `0`（無制限） | キャッシュ全体の総容量上限（バイト）。超過時は commit/finalize 時に最も古くアクセスされたエントリから自動で LRU 削除されます。 |

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

## Runner image への patch

Dockerfile に `ENV ACTIONS_CACHE_URL=...` を追加するだけでは不十分です。
公式 runner は、各 job の開始時に `SystemVssConnection` の endpoint 情報から `ACTIONS_CACHE_URL` と `ACTIONS_RESULTS_URL` を上書きします。

workflow を変更しない構成では、`actions/runner` に小さな patch を適用した runner image を build します。
`NodeScriptActionHandler` と `ContainerActionHandler` で、既存の `CacheServerUrl` と `ResultsServiceUrl` の設定後に以下を実行します。

```csharp
var serverUrl = System.Environment.GetEnvironmentVariable("GHA_CACHE_SERVER_URL");
if (!string.IsNullOrWhiteSpace(serverUrl))
{
    var url = serverUrl.TrimEnd('/') + "/";
    Environment["ACTIONS_CACHE_URL"] = url;
    Environment["ACTIONS_RESULTS_URL"] = url;
}
```

runner image または runner を起動する controller で、独自の環境変数を設定します。

```text
GHA_CACHE_SERVER_URL=http://gha-cache-server:8080
```

controller には patch 済み image を指定します。

```yaml
runner:
  image: ghcr.io/example/actions-runner-cache@sha256:<digest>
```

`PUBLIC_URL` には job から到達できる同じ URL を設定します。
他の Actions runtime service は job 単位の token を使うため、`ACTIONS_RUNTIME_TOKEN` は変更しません。
現在の proxy は固定の `AUTH_TOKEN` を使うため、本番利用前に runner から proxy への認証方式を別途設定する必要があります。

この patch では workflow を変更しませんが、upstream runner を更新したときは runner image を再 build します。

## Interception の例

[`examples/interception/`](examples/interception/) に、`gha-cache-server` 内蔵の TLS 終端・ローカル CA 生成・`results-receiver` 中継・SeaweedFS・公式 runner を組み合わせた Compose example があります。
プライベートネットワーク内で利用してください。

## Build と検証

```sh
cargo fmt --check
cargo test --locked
cargo clippy --locked --all-targets --all-features -- -D warnings
```
