#!/bin/bash
# Run cargo inside the fixed Rust Docker builder with a persistent cargo home.
# Usage: rbuild.sh <cargo args...>
set -e
cd "$(dirname "$0")"
docker run --rm -e CARGO_HOME=/cargo -v /tmp/cargo-home:/cargo -v "$PWD":/build -w /build \
  rust:1.97.1-bookworm bash -c 'rustup component add rustfmt clippy; cargo "$@"' bash "$@"
