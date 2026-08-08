# Caddy interception

This example routes the cache and results service hostnames used by a self-hosted runner to `gha-cache-proxy` without changing a workflow.
It uses Caddy for TLS termination and DNS interception for the runner.

## Request flow

```text
runner -> original cache/results hostname -> Caddy -> gha-cache-proxy -> SeaweedFS
```

The example deliberately replaces the incoming job Bearer token with a fixed internal token because the proxy currently authenticates control APIs with `AUTH_TOKEN`.
This is not validation of `ACTIONS_RUNTIME_TOKEN`.
Keep Caddy on a private network or require mTLS so an untrusted client cannot use the replacement token.

## Caddy

Use [`Caddyfile`](Caddyfile) and set the same token on Caddy and the Rust proxy:

```sh
export CACHE_PROXY_AUTH_TOKEN=change-this-internal-token
```

The Caddy site names must match the hostnames in the runner's `ACTIONS_CACHE_URL` and `ACTIONS_RESULTS_URL`.
Those hostnames can differ between GitHub.com, GHES, and future service deployments.

Set the proxy's public URL to a hostname routed to Caddy:

```text
PUBLIC_URL=https://artifactcache.actions.githubusercontent.com
```

The Caddy upstream name `gha-cache-proxy` must resolve to the Rust proxy on the Caddy network.

## DNS and certificates

Resolve the intercepted hostnames to the Caddy address from the runner and every job container that runs cache actions:

```text
10.10.0.5 artifactcache.actions.githubusercontent.com
10.10.0.5 results-receiver.actions.githubusercontent.com
```

`tls internal` creates a Caddy local CA.
Persist Caddy's `/data` directory so the CA does not change after a restart.
Copy its root certificate from the Caddy container and install it in the runner image:

```sh
docker cp caddy:/data/caddy/pki/authorities/local/root.crt ./caddy-root.crt
```

Set `NODE_EXTRA_CA_CERTS` when the Node runtime does not use the system trust store:

```dockerfile
COPY caddy-root.crt /usr/local/share/ca-certificates/caddy-root.crt
RUN update-ca-certificates
ENV NODE_EXTRA_CA_CERTS=/usr/local/share/ca-certificates/caddy-root.crt
```

Do not expose this Caddy listener to an untrusted network.
The proxy's signed blob URLs use `PUBLIC_URL`, so that hostname must resolve to Caddy for `PUT`, `GET`, `HEAD`, and range requests as well.
