# syntax=docker/dockerfile:1
# Multi-stage build: pinned Rust builder, minimal non-root runtime.
# No secrets are baked in; all runtime configuration comes from the
# environment (see docs/deployment.md).

FROM rust:1.97.1-bookworm AS builder
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src/ ./src/
RUN cargo build --release --locked

FROM debian:bookworm-slim
# ca-certificates is needed for TLS S3 endpoints; curl is used by the compose
# healthcheck only.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --create-home --uid 10001 gha-cache
COPY --from=builder /build/target/release/gha-cache-proxy /usr/local/bin/gha-cache-proxy
USER gha-cache
EXPOSE 8080
ENTRYPOINT ["gha-cache-proxy"]
