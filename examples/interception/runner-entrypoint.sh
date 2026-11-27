#!/usr/bin/env bash
set -euo pipefail

: "${RUNNER_URL:?RUNNER_URL is required}"
: "${RUNNER_TOKEN:?RUNNER_TOKEN is required}"
: "${RUNNER_NAME:?RUNNER_NAME is required}"
: "${RUNNER_LABELS:?RUNNER_LABELS is required}"

ca_cert=/runner-ca/root.crt
for _ in $(seq 1 60); do
  [ -s "$ca_cert" ] && break
  sleep 1
done
[ -s "$ca_cert" ] || {
  echo "gha-cache-server root CA was not created" >&2
  exit 1
}

cp "$ca_cert" /usr/local/share/ca-certificates/gha-cache-server.crt
update-ca-certificates >/dev/null

export NODE_EXTRA_CA_CERTS=/etc/ssl/certs/ca-certificates.crt
export SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt
export HOME=/home/runner

cd /home/runner
setpriv --reuid=runner --regid=runner --init-groups ./config.sh \
  --unattended \
  --replace \
  --ephemeral \
  --url "$RUNNER_URL" \
  --token "$RUNNER_TOKEN" \
  --name "$RUNNER_NAME" \
  --labels "$RUNNER_LABELS"

exec setpriv --reuid=runner --regid=runner --init-groups ./run.sh
