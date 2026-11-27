# Built-in TLS interception

This example routes the cache and results service hostnames used by a self-hosted runner to `gha-cache-server` without changing any workflow.
`gha-cache-server` terminates TLS directly with its built-in local CA, handles GitHub Actions cache v1/v2 APIs against S3-compatible storage, and transparently forwards non-cache `results-receiver` traffic (logs, artifacts, etc.) to GitHub.

## Request flow

```text
runner ──(HTTPS + local CA)──▶ gha-cache-server ──▶ SeaweedFS / S3 bucket (cache)
                                    └─────────────▶ GitHub (logs, artifacts, etc.)
```

## Compose runner

`compose.yaml` starts SeaweedFS (so you can run `docker compose up` locally without preparing an external bucket), `gha-cache-server`, and one ephemeral official GitHub Actions runner.
The runner image adds the generated local CA certificate to the system trust store and exports `NODE_EXTRA_CA_CERTS` / `SSL_CERT_FILE`.

Create a repository or organization runner registration token, then start the example:

```sh
export RUNNER_URL=https://github.com/example/project
export RUNNER_TOKEN=replace-with-a-runner-registration-token
export RUNNER_NAME=gha-cache-interception
export RUNNER_LABELS=gha-cache-interception
docker compose -f examples/interception/compose.yaml up --build
```

Use the matching label in a workflow:

```yaml
jobs:
  cache-check:
    runs-on: gha-cache-interception
    steps:
      - uses: actions/cache@v4
        with:
          path: .cache
          key: interception-example
```

The runner is ephemeral and exits after one job.
Stop the stack and remove its volumes with:

```sh
docker compose -f examples/interception/compose.yaml down -v
```

## DNS and certificates

Resolve the intercepted hostnames to `gha-cache-server` from the runner (and any job containers that use cache actions):

```text
10.10.0.5 artifactcache.actions.githubusercontent.com
10.10.0.5 results-receiver.actions.githubusercontent.com
```

On startup, `gha-cache-server` generates and persists a local CA under `TLS_DATA_DIR` and writes the public root certificate to `TLS_CA_CERT_OUT` (`/runner-ca/root.crt`).
Persist the `cache-tls` volume so the CA stays stable across restarts, and keep `gha-cache-server` on a private network.
