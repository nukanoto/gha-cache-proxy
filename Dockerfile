# syntax=docker/dockerfile:1
# Multi-stage build: pinned Rust builder, minimal non-root runtime.
# No secrets are baked in; all runtime configuration comes from the
# environment.

FROM rust:1.97.1-bookworm AS builder
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src/ ./src/
RUN cargo build --release --locked

FROM debian:bookworm-slim
# ca-certificates is needed for TLS S3 / upstream endpoints; libcap2-bin grants
# cap_net_bind_service so the non-root user can bind port 443 in TLS mode.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates libcap2-bin \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --create-home --uid 10001 gha-cache \
    && mkdir -p /var/lib/gha-cache-server/tls /runner-ca \
    && chown -R gha-cache:gha-cache /var/lib/gha-cache-server /runner-ca
COPY --from=builder /build/target/release/gha-cache-server /usr/local/bin/gha-cache-server
RUN setcap 'cap_net_bind_service=+ep' /usr/local/bin/gha-cache-server
USER gha-cache
EXPOSE 8080 443
ENTRYPOINT ["gha-cache-server"]
