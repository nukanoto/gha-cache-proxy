#!/usr/bin/env bash
set -euo pipefail

: "${RUNNER_URL:?RUNNER_URL is required}"
: "${RUNNER_TOKEN:?RUNNER_TOKEN is required}"
: "${RUNNER_NAME:?RUNNER_NAME is required}"
: "${RUNNER_LABELS:?RUNNER_LABELS is required}"

caddy_root=/caddy-data/caddy/pki/authorities/local/root.crt
for _ in $(seq 1 60); do
  [ -s "$caddy_root" ] && break
  sleep 1
done
[ -s "$caddy_root" ] || {
  echo "Caddy root CA was not created" >&2
  exit 1
}

export NODE_EXTRA_CA_CERTS="$caddy_root"
cd /home/runner
./config.sh \
  --unattended \
  --replace \
  --ephemeral \
  --url "$RUNNER_URL" \
  --token "$RUNNER_TOKEN" \
  --name "$RUNNER_NAME" \
  --labels "$RUNNER_LABELS" \
  --work "${RUNNER_WORKDIR:-_work}"
exec ./run.sh
