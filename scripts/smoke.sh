#!/usr/bin/env bash
# End-to-end smoke test for gha-cache-server against running services.
# Phase 1 (default): full protocol coverage. Phase 2 (--after-restart):
# re-verifies persisted entries after the proxy container was restarted.
# Uses only curl and the Python standard library; never talks to GitHub.

set -euo pipefail

cd "$(dirname "$0")"

PROXY_URL="${PROXY_URL:-http://localhost:8080}"
AUTH_TOKEN="${AUTH_TOKEN:-dev-secret-token}"
STATE_FILE="${SMOKE_STATE_FILE:-/tmp/gha-cache-server-smoke-state.json}"
TS="$(date +%s)"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

V1_VERSION="v1-ver-$TS"
V2_VERSION="v2-ver-$TS"
PB_VERSION="pb-ver-$TS"
V1_KEY="v1-key-$TS"
V2_KEY="v2-key-$TS"
V2_BLOCK_KEY="v2-blocks-$TS"
PB_KEY="pb-key-$TS"
AUTH=(-H "Authorization: Bearer $AUTH_TOKEN")

V2_CREATE="$PROXY_URL/twirp/github.actions.results.api.v1.CacheService/CreateCacheEntry"
V2_FINALIZE="$PROXY_URL/twirp/github.actions.results.api.v1.CacheService/FinalizeCacheEntryUpload"
V2_GET="$PROXY_URL/twirp/github.actions.results.api.v1.CacheService/GetCacheEntryDownloadURL"

fail() { echo "FAIL: $*" >&2; exit 1; }
pass() { echo "PASS: $*"; }

# curl_raw: runs curl, sets STATUS (000 when the connection was reset) and
# leaves the response body in $WORK/body, headers in $WORK/hdr.
curl_raw() {
  curl -sS -o "$WORK/body" -D "$WORK/hdr" -w '%{http_code}' "$@" > "$WORK/code" || true
  STATUS="$(cat "$WORK/code")"
  STATUS="${STATUS:-000}"
  BODY="$(cat "$WORK/body" 2>/dev/null || true)"
}

assert_status() { # expected status, description
  if [ "$STATUS" != "$1" ]; then
    fail "$2: expected HTTP $1, got $STATUS (body: $(echo "$BODY" | head -c 300))"
  fi
  pass "$2 (HTTP $STATUS)"
}

assert_2xx() {
  case "$STATUS" in
    2[0-9][0-9]) pass "$1 (HTTP $STATUS)" ;;
    *) fail "$1: expected 2xx, got $STATUS (body: $(echo "$BODY" | head -c 300))" ;;
  esac
}

assert_4xx() {
  case "$STATUS" in
    4[0-9][0-9]) pass "$1 (HTTP $STATUS)" ;;
    *) fail "$1: expected 4xx, got $STATUS (body: $(echo "$BODY" | head -c 300))" ;;
  esac
}

json_get() { # key [file] -> value parsed from the JSON file (default $WORK/body)
  local file="${2:-$WORK/body}"
  python3 -c 'import json, sys; print(json.load(open(sys.argv[1]))[sys.argv[2]])' "$file" "$1"
}

header_value() { # name -> value parsed from $WORK/hdr
  python3 - "$WORK/hdr" "$1" <<'PY'
import sys
name = sys.argv[2].lower() + ":"
for line in open(sys.argv[1], errors="replace"):
    if line.lower().startswith(name):
        print(line[len(name):].strip())
        break
PY
}

gen_payload() { # file, size -> deterministic repeating byte pattern
  python3 - "$1" "$2" <<'PY'
import sys
pat = b"0123456789abcdefghijklmnopqrstuvwxyz"
size = int(sys.argv[2])
with open(sys.argv[1], "wb") as f:
    f.write((pat * ((size + len(pat) - 1) // len(pat)))[:size])
PY
}

wait_health() { # /healthz is unauthenticated and does not probe S3
  local n=0
  while [ "$n" -lt 60 ]; do
    local code
    code="$(curl -sS -o /dev/null -w '%{http_code}' "$PROXY_URL/healthz" 2>/dev/null || echo 000)"
    if [ "$code" = "200" ]; then
      pass "health endpoint is up"
      return 0
    fi
    sleep 1
    n=$((n + 1))
  done
  fail "proxy health endpoint did not become ready"
}

download_sha() { # url -> sha256 of the downloaded body (no bearer needed)
  curl -sS -o "$WORK/dl" "$1"
  sha256sum "$WORK/dl" | awk '{print $1}'
}

v1_reserve() { # key, version, cache_size -> cacheId
  curl_raw -X POST "${AUTH[@]}" -H 'Content-Type: application/json' \
    -d "{\"key\":\"$1\",\"version\":\"$2\",\"cacheSize\":$3}" \
    "$PROXY_URL/_apis/artifactcache/caches"
  case "$STATUS" in
    200|201) ;;
    *) fail "v1 reserve $1: expected 200/201, got $STATUS (body: $(echo "$BODY" | head -c 300))" ;;
  esac
  local id
  id="$(json_get cacheId)"
  case "$id" in
    ''|*[!0-9]*) fail "v1 reserve returned non-numeric cacheId: '$id'" ;;
  esac
  echo "$id"
}

v1_upload_chunks() { # cache_id, payload_file -> two Content-Range PATCHes
  local id="$1" payload="$2"
  local size mid
  size="$(wc -c < "$payload")"
  mid=$((size / 2))
  head -c "$mid" "$payload" > "$WORK/chunk1"
  tail -c "+$((mid + 1))" "$payload" > "$WORK/chunk2"
  curl_raw -X PATCH "${AUTH[@]}" -H 'Content-Type: application/octet-stream' \
    -H "Content-Range: bytes 0-$((mid - 1))/*" --data-binary @"$WORK/chunk1" \
    "$PROXY_URL/_apis/artifactcache/caches/$id"
  assert_2xx "v1 chunk 1 upload (bytes 0-$((mid - 1)))"
  curl_raw -X PATCH "${AUTH[@]}" -H 'Content-Type: application/octet-stream' \
    -H "Content-Range: bytes $mid-$((size - 1))/*" --data-binary @"$WORK/chunk2" \
    "$PROXY_URL/_apis/artifactcache/caches/$id"
  assert_2xx "v1 chunk 2 upload (bytes $mid-$((size - 1)))"
}

v1_commit() { # cache_id, size
  curl_raw -X POST "${AUTH[@]}" -H 'Content-Type: application/json' \
    -d "{\"size\":$2}" "$PROXY_URL/_apis/artifactcache/caches/$1"
  assert_2xx "v1 commit cache $1"
}

v1_lookup() { # keys, version -> leaves response in $WORK/body
  curl_raw -G "${AUTH[@]}" --data-urlencode "keys=$1" --data-urlencode "version=$2" \
    "$PROXY_URL/_apis/artifactcache/cache"
}

twirp_post() { # url, request_file -> JSON POST
  curl_raw -X POST "${AUTH[@]}" -H 'Content-Type: application/json' --data-binary @"$2" "$1"
}

test_auth_and_limit() {
  echo "== auth and limits =="
  curl_raw "$PROXY_URL/_apis/artifactcache/caches"
  assert_status 401 "v1 without auth returns 401"
  curl_raw -H 'Authorization: Bearer wrong-token' "$PROXY_URL/_apis/artifactcache/caches"
  assert_status 401 "v1 with wrong token returns 401"
  printf '{"key":"x","version":"y"}' > "$WORK/req.json"
  curl_raw -X POST -H 'Content-Type: application/json' --data-binary @"$WORK/req.json" "$V2_CREATE"
  assert_status 401 "v2 without auth returns 401"
  curl_raw "$PROXY_URL/healthz"
  assert_status 200 "healthz without auth returns 200"

  # Oversized uploads may return 413 or reset the connection.
  gen_payload "$WORK/payload_over" 11534336
  printf '{"key":"v2-over-%s","version":"%s"}' "$TS" "$V2_VERSION" > "$WORK/req.json"
  twirp_post "$V2_CREATE" "$WORK/req.json"
  assert_status 200 "v2 create for limit test"
  local url
  url="$(json_get signedUploadUrl)"
  curl_raw -X PUT --data-binary @"$WORK/payload_over" "$url"
  case "$STATUS" in
    413) pass "oversized upload rejected with 413" ;;
    000) pass "oversized upload aborted by server (connection reset)" ;;
    4[0-9][0-9]) pass "oversized upload rejected (HTTP $STATUS)" ;;
    *) fail "oversized upload: expected 4xx or abort, got $STATUS" ;;
  esac
  curl_raw "$PROXY_URL/healthz"
  assert_status 200 "proxy still healthy after limit rejection"
}

test_v1() {
  echo "== v1 REST =="
  local payload="$WORK/payload_v1"
  gen_payload "$payload" 1000
  local sha
  sha="$(sha256sum "$payload" | awk '{print $1}')"

  local id
  id="$(v1_reserve "$V1_KEY" "$V1_VERSION" 1000)"
  pass "v1 reserve cacheId=$id"

  # A duplicate key/version reservation must be rejected.
  curl_raw -X POST "${AUTH[@]}" -H 'Content-Type: application/json' \
    -d "{\"key\":\"$V1_KEY\",\"version\":\"$V1_VERSION\",\"cacheSize\":1000}" \
    "$PROXY_URL/_apis/artifactcache/caches"
  assert_4xx "v1 duplicate reservation rejected"

  v1_upload_chunks "$id" "$payload"
  v1_commit "$id" 1000

  # Completed entries are immutable: PATCH is rejected, a same-size commit
  # retry is idempotent, a different-size commit is rejected.
  curl_raw -X PATCH "${AUTH[@]}" -H 'Content-Type: application/octet-stream' \
    -H 'Content-Range: bytes 0-99/*' --data-binary @"$WORK/chunk1" \
    "$PROXY_URL/_apis/artifactcache/caches/$id"
  assert_4xx "v1 completed entry rejects PATCH"
  curl_raw -X POST "${AUTH[@]}" -H 'Content-Type: application/json' \
    -d '{"size":1000}' "$PROXY_URL/_apis/artifactcache/caches/$id"
  assert_2xx "v1 completed entry commit is idempotent (same size)"
  curl_raw -X POST "${AUTH[@]}" -H 'Content-Type: application/json' \
    -d '{"size":999}' "$PROXY_URL/_apis/artifactcache/caches/$id"
  assert_4xx "v1 completed entry rejects commit with different size"

  v1_lookup "$V1_KEY" "$V1_VERSION"
  assert_status 200 "v1 exact lookup"
  local arch cache_key
  arch="$(json_get archiveLocation)"
  cache_key="$(json_get cacheKey)"
  [ "$cache_key" = "$V1_KEY" ] || fail "v1 lookup cacheKey mismatch: $cache_key"
  pass "v1 lookup cacheKey=$cache_key"
  case "$arch" in
    "$PROXY_URL/artifacts/"*) pass "v1 archiveLocation is a signed blob URL" ;;
    *) fail "v1 archiveLocation unexpected: $arch" ;;
  esac
  [ "$(download_sha "$arch")" = "$sha" ] || fail "v1 archive content mismatch"
  pass "v1 archive download matches uploaded bytes"

  # A miss returns 204.
  curl_raw -G "${AUTH[@]}" --data-urlencode "keys=no-such-key-$TS" --data-urlencode "version=$V1_VERSION" \
    "$PROXY_URL/_apis/artifactcache/cache"
  assert_status 204 "v1 miss returns 204"

  # Check the diagnostic cache list.
  curl_raw -G "${AUTH[@]}" --data-urlencode "key=$V1_KEY" "$PROXY_URL/_apis/artifactcache/caches"
  assert_status 200 "v1 diagnostic cache list"
  python3 - "$WORK/body" "$V1_KEY" <<'PY' || fail "v1 diagnostic list missing entry"
import json, sys
data = json.load(open(sys.argv[1]))
assert any(e.get("cacheKey") == sys.argv[2] for e in data.get("artifactCaches", [])), data
print("ok: v1 diagnostic list contains entry")
PY
  pass "v1 diagnostic list contains entry"

  # Restore-key prefixes are tried in order; each reservation must stage
  # valid chunks before committing, exactly like the primary entry.
  local res_a="resA-1-$TS" res_b="resB-9-$TS"
  local id_a id_b
  id_a="$(v1_reserve "$res_a" "$V1_VERSION" 1000)"
  id_b="$(v1_reserve "$res_b" "$V1_VERSION" 1000)"
  v1_upload_chunks "$id_a" "$payload"
  v1_upload_chunks "$id_b" "$payload"
  v1_commit "$id_a" 1000
  v1_commit "$id_b" 1000
  v1_lookup "primary-miss-$TS,resA" "$V1_VERSION"
  assert_status 200 "v1 restore-key prefix match"
  [ "$(json_get cacheKey)" = "$res_a" ] || fail "v1 prefix match returned wrong key"
  pass "v1 prefix match returns $res_a"
  v1_lookup "primary-miss-$TS,resA,resB" "$V1_VERSION"
  assert_status 200 "v1 restore-key ordering"
  [ "$(json_get cacheKey)" = "$res_a" ] || fail "v1 restore ordering returned wrong key"
  pass "v1 restore keys tried in order"

  V1_SHA="$sha"
}

test_v2_json() {
  echo "== v2 Twirp JSON =="
  local payload="$WORK/payload_v2"
  gen_payload "$payload" 2048
  local sha
  sha="$(sha256sum "$payload" | awk '{print $1}')"

  printf '{"key":"%s","version":"%s"}' "$V2_KEY" "$V2_VERSION" > "$WORK/req.json"
  twirp_post "$V2_CREATE" "$WORK/req.json"
  assert_status 200 "v2 create (JSON)"
  python3 - "$WORK/body" "$PROXY_URL" <<'PY' || fail "v2 create response shape"
import json, sys
d = json.load(open(sys.argv[1]))
assert d.get("ok") is True, d
assert d.get("signedUploadUrl", "").startswith(sys.argv[2] + "/artifacts/"), d
print("ok: v2 create ok=true with signed upload URL")
PY
  local url
  url="$(json_get signedUploadUrl)"

  # A duplicate reservation is a logical conflict.
  twirp_post "$V2_CREATE" "$WORK/req.json"
  assert_status 200 "v2 duplicate create returns 200"
  python3 - "$WORK/body" <<'PY' || fail "v2 duplicate create should be ok:false"
import json, sys
d = json.load(open(sys.argv[1]))
assert d.get("ok") is False and d.get("message"), d
print("ok: v2 duplicate create ok=false with message")
PY

  # Upload the blob in one request.
  curl_raw -X PUT -H 'Content-Type: application/octet-stream' --data-binary @"$payload" "$url"
  assert_status 201 "v2 blob plain PUT returns 201"

  # finalize
  printf '{"key":"%s","version":"%s","size_bytes":"2048"}' "$V2_KEY" "$V2_VERSION" > "$WORK/req.json"
  twirp_post "$V2_FINALIZE" "$WORK/req.json"
  assert_status 200 "v2 finalize (JSON)"
  python3 - "$WORK/body" <<'PY' || fail "v2 finalize response"
import json, sys
d = json.load(open(sys.argv[1]))
assert d.get("ok") is True and str(d.get("entryId", "")).isdigit(), d
print("ok: v2 finalize entryId=%s" % d["entryId"])
PY

  # Resolve the archive URL; blob reads only work with a download URL
  # obtained after finalize, never with the upload token.
  printf '{"key":"%s","version":"%s","restore_keys":[]}' "$V2_KEY" "$V2_VERSION" > "$WORK/req.json"
  twirp_post "$V2_GET" "$WORK/req.json"
  assert_status 200 "v2 get download URL (JSON)"
  local dl
  dl="$(json_get signedDownloadUrl)"
  [ "$(json_get matchedKey)" = "$V2_KEY" ] || fail "v2 matchedKey mismatch"
  pass "v2 matchedKey=$V2_KEY"
  case "$dl" in
    "$PROXY_URL/artifacts/"*) pass "v2 signed download URL" ;;
    *) fail "v2 download URL unexpected: $dl" ;;
  esac

  curl_raw -I "$dl"
  assert_status 200 "v2 blob HEAD after finalize"
  [ "$(header_value content-length)" = "2048" ] || fail "v2 blob HEAD content-length: $(header_value content-length)"
  pass "v2 blob HEAD content-length=2048"
  case "$(header_value content-type)" in
    *octet-stream*) pass "v2 blob HEAD content-type" ;;
    *) fail "v2 blob HEAD content-type: $(header_value content-type)" ;;
  esac

  curl_raw "$dl"
  assert_status 200 "v2 blob full GET"
  cmp -s "$WORK/body" "$payload" || fail "v2 blob full GET content mismatch"
  pass "v2 blob full GET content matches"
  [ -n "$(header_value content-length)" ] || fail "v2 blob GET content-length header missing"
  pass "v2 blob GET content-length header present"

  # Range GET
  curl_raw -H 'Range: bytes=100-199' "$dl"
  assert_status 206 "v2 blob range GET returns 206"
  [ "$(header_value content-range)" = "bytes 100-199/2048" ] || fail "v2 blob range content-range: $(header_value content-range)"
  pass "v2 blob range content-range correct"
  tail -c +101 "$payload" | head -c 100 > "$WORK/range_exp"
  cmp -s "$WORK/body" "$WORK/range_exp" || fail "v2 blob range GET content mismatch"
  pass "v2 blob range GET content matches"

  [ "$(download_sha "$dl")" = "$sha" ] || fail "v2 download content mismatch"
  pass "v2 download content matches"

  # A miss returns ok:false.
  printf '{"key":"v2-miss-%s","version":"%s","restore_keys":[]}' "$TS" "$V2_VERSION" > "$WORK/req.json"
  twirp_post "$V2_GET" "$WORK/req.json"
  assert_status 200 "v2 miss returns 200"
  python3 - "$WORK/body" <<'PY' || fail "v2 miss should be ok:false"
import json, sys
d = json.load(open(sys.argv[1]))
assert d.get("ok") is False, d
print("ok: v2 miss ok=false")
PY

  # Restore-key prefixes are tried in order.
  local res_c="resC-1-$TS"
  gen_payload "$WORK/payload_resc" 512
  printf '{"key":"%s","version":"%s"}' "$res_c" "$V2_VERSION" > "$WORK/req.json"
  twirp_post "$V2_CREATE" "$WORK/req.json"
  assert_status 200 "v2 create restore entry"
  curl_raw -X PUT --data-binary @"$WORK/payload_resc" "$(json_get signedUploadUrl)"
  assert_status 201 "v2 restore entry blob PUT"
  printf '{"key":"%s","version":"%s","size_bytes":"512"}' "$res_c" "$V2_VERSION" > "$WORK/req.json"
  twirp_post "$V2_FINALIZE" "$WORK/req.json"
  assert_status 200 "v2 finalize restore entry"
  # restoreKeys exercises the lowerCamelCase request alias.
  printf '{"key":"miss-%s","version":"%s","restoreKeys":["resC"]}' "$TS" "$V2_VERSION" > "$WORK/req.json"
  twirp_post "$V2_GET" "$WORK/req.json"
  assert_status 200 "v2 restore-key prefix match"
  [ "$(json_get matchedKey)" = "$res_c" ] || fail "v2 prefix match returned wrong key"
  pass "v2 restore-key prefix match returns $res_c"

  V2_SHA="$sha"
}

test_blob_blocks() {
  echo "== Azure blob block operations =="
  local payload="$WORK/payload_blocks"
  gen_payload "$payload" 1536
  local sha
  sha="$(sha256sum "$payload" | awk '{print $1}')"

  printf '{"key":"%s","version":"%s"}' "$V2_BLOCK_KEY" "$V2_VERSION" > "$WORK/req.json"
  twirp_post "$V2_CREATE" "$WORK/req.json"
  assert_status 200 "v2 create for block test"
  local url
  url="$(json_get signedUploadUrl)"

  head -c 1024 "$payload" > "$WORK/block1"
  tail -c +1025 "$payload" > "$WORK/block2"
  local id1 id2
  id1="$(printf '0001' | base64)"
  id2="$(printf '0002' | base64)"

  curl_raw -X PUT -H 'Content-Type: application/octet-stream' --data-binary @"$WORK/block1" "$url?comp=block&blockid=$id1"
  assert_status 201 "v2 blob stage block 1 (comp=block)"
  curl_raw -X PUT -H 'Content-Type: application/octet-stream' --data-binary @"$WORK/block2" "$url?comp=block&blockid=$id2"
  assert_status 201 "v2 blob stage block 2 (comp=block)"

  printf '<?xml version="1.0" encoding="utf-8"?>\n<BlockList><Latest>%s</Latest><Latest>%s</Latest></BlockList>\n' "$id1" "$id2" > "$WORK/blocklist.xml"
  curl_raw -X PUT -H 'Content-Type: application/xml' --data-binary @"$WORK/blocklist.xml" "$url?comp=blocklist"
  assert_status 201 "v2 blob commit block list (comp=blocklist)"

  printf '{"key":"%s","version":"%s","size_bytes":"1536"}' "$V2_BLOCK_KEY" "$V2_VERSION" > "$WORK/req.json"
  twirp_post "$V2_FINALIZE" "$WORK/req.json"
  assert_status 200 "v2 finalize block-built entry"
  python3 - "$WORK/body" <<'PY' || fail "v2 finalize block-built entry response"
import json, sys
d = json.load(open(sys.argv[1]))
assert d.get("ok") is True and str(d.get("entryId", "")).isdigit(), d
print("ok: v2 finalize block-built entry entryId=%s" % d["entryId"])
PY

  printf '{"key":"%s","version":"%s","restore_keys":[]}' "$V2_BLOCK_KEY" "$V2_VERSION" > "$WORK/req.json"
  twirp_post "$V2_GET" "$WORK/req.json"
  assert_status 200 "v2 get download URL for block-built entry"
  local dl
  dl="$(json_get signedDownloadUrl)"

  curl_raw -I "$dl"
  assert_status 200 "v2 blob HEAD after blocklist"
  [ "$(header_value content-length)" = "1536" ] || fail "v2 blob HEAD content-length after blocklist"
  pass "v2 blob HEAD content-length=1536"

  curl_raw "$dl"
  assert_status 200 "v2 blob GET after blocklist"
  cmp -s "$WORK/body" "$payload" || fail "v2 blob GET content mismatch after blocklist"
  pass "v2 blob GET content matches after blocklist"

  curl_raw -H 'Range: bytes=1024-1535' "$dl"
  assert_status 206 "v2 blob range GET after blocklist"
  [ "$(header_value content-range)" = "bytes 1024-1535/1536" ] || fail "v2 blob range content-range: $(header_value content-range)"
  pass "v2 blob range content-range correct"
  cmp -s "$WORK/body" "$WORK/block2" || fail "v2 blob range content mismatch"
  pass "v2 blob range GET content matches"

  [ "$(download_sha "$dl")" = "$sha" ] || fail "v2 block-built download content mismatch"
  pass "v2 block-built download content matches"
}

test_v2_protobuf() {
  echo "== v2 Twirp protobuf =="
  local payload="$WORK/payload_pb"
  gen_payload "$payload" 512
  local sha
  sha="$(sha256sum "$payload" | awk '{print $1}')"

  python3 twirp_proto.py encode CreateCacheEntry --key "$PB_KEY" --version "$PB_VERSION" > "$WORK/req.pb"
  curl_raw -X POST "${AUTH[@]}" -H 'Content-Type: application/protobuf' --data-binary @"$WORK/req.pb" "$V2_CREATE"
  assert_status 200 "v2 create (protobuf)"
  python3 twirp_proto.py decode CreateCacheEntry < "$WORK/body" > "$WORK/create.json"
  python3 - "$WORK/create.json" <<'PY' || fail "v2 protobuf create response"
import json, sys
d = json.load(open(sys.argv[1]))
assert d.get("ok") is True and d.get("signed_upload_url"), d
print("ok: v2 protobuf create ok=true")
PY
  local url
  url="$(json_get signed_upload_url "$WORK/create.json")"
  curl_raw -X PUT --data-binary @"$payload" "$url"
  assert_status 201 "v2 protobuf entry blob PUT"

  python3 twirp_proto.py encode FinalizeCacheEntryUpload --key "$PB_KEY" --version "$PB_VERSION" --size-bytes 512 > "$WORK/req.pb"
  curl_raw -X POST "${AUTH[@]}" -H 'Content-Type: application/protobuf' --data-binary @"$WORK/req.pb" "$V2_FINALIZE"
  assert_status 200 "v2 finalize (protobuf)"
  python3 twirp_proto.py decode FinalizeCacheEntryUpload < "$WORK/body" > "$WORK/finalize.json"
  python3 - "$WORK/finalize.json" <<'PY' || fail "v2 protobuf finalize response"
import json, sys
d = json.load(open(sys.argv[1]))
assert d.get("ok") is True and str(d.get("entry_id", "")).isdigit(), d
print("ok: v2 protobuf finalize entry_id=%s" % d["entry_id"])
PY

  python3 twirp_proto.py encode GetCacheEntryDownloadURL --key "$PB_KEY" --version "$PB_VERSION" > "$WORK/req.pb"
  curl_raw -X POST "${AUTH[@]}" -H 'Content-Type: application/protobuf' --data-binary @"$WORK/req.pb" "$V2_GET"
  assert_status 200 "v2 get download URL (protobuf)"
  python3 twirp_proto.py decode GetCacheEntryDownloadURL < "$WORK/body" > "$WORK/get.json"
  python3 - "$WORK/get.json" "$PB_KEY" <<'PY' || fail "v2 protobuf get response"
import json, sys
d = json.load(open(sys.argv[1]))
assert d.get("ok") is True and d.get("matched_key") == sys.argv[2] and d.get("signed_download_url"), d
print("ok: v2 protobuf get matched_key=%s" % d["matched_key"])
PY
  local dl
  dl="$(json_get signed_download_url "$WORK/get.json")"
  [ "$(download_sha "$dl")" = "$sha" ] || fail "v2 protobuf download content mismatch"
  pass "v2 protobuf download content matches"

  PB_SHA="$sha"
}

write_state() {
  V1_KEY="$V1_KEY" V1_VERSION="$V1_VERSION" V1_SHA="$V1_SHA" \
  V2_KEY="$V2_KEY" V2_VERSION="$V2_VERSION" V2_SHA="$V2_SHA" \
  PB_KEY="$PB_KEY" PB_VERSION="$PB_VERSION" PB_SHA="$PB_SHA" \
  python3 - "$STATE_FILE" <<'PY'
import json, os, sys
keys = ("V1_KEY", "V1_VERSION", "V1_SHA", "V2_KEY", "V2_VERSION",
        "V2_SHA", "PB_KEY", "PB_VERSION", "PB_SHA")
json.dump({k: os.environ[k] for k in keys}, open(sys.argv[1], "w"), indent=2)
PY
  echo "state written to $STATE_FILE"
}

run_after_restart() {
  echo "== persistence after restart =="
  [ -f "$STATE_FILE" ] || fail "state file $STATE_FILE not found; run phase 1 first"
  python3 - "$STATE_FILE" > "$WORK/state.sh" <<'PY'
import json, sys
state = json.load(open(sys.argv[1]))
for k, v in state.items():
    print('%s="%s"' % (k.lower(), v))
PY
  # shellcheck disable=SC1090
  source "$WORK/state.sh"

  wait_health

  v1_lookup "$v1_key" "$v1_version"
  assert_status 200 "v1 lookup after restart"
  [ "$(download_sha "$(json_get archiveLocation)")" = "$v1_sha" ] || fail "v1 archive content after restart"
  pass "v1 archive restored after restart"

  curl_raw -G "${AUTH[@]}" --data-urlencode "keys=no-such-key-$TS" --data-urlencode "version=$v1_version" \
    "$PROXY_URL/_apis/artifactcache/cache"
  assert_status 204 "v1 miss still 204 after restart"

  printf '{"key":"%s","version":"%s","restore_keys":[]}' "$v2_key" "$v2_version" > "$WORK/req.json"
  twirp_post "$V2_GET" "$WORK/req.json"
  assert_status 200 "v2 lookup after restart"
  [ "$(download_sha "$(json_get signedDownloadUrl)")" = "$v2_sha" ] || fail "v2 archive content after restart"
  pass "v2 archive restored after restart"

  python3 twirp_proto.py encode GetCacheEntryDownloadURL --key "$pb_key" --version "$pb_version" > "$WORK/req.pb"
  curl_raw -X POST "${AUTH[@]}" -H 'Content-Type: application/protobuf' --data-binary @"$WORK/req.pb" "$V2_GET"
  assert_status 200 "v2 protobuf lookup after restart"
  local dl
  dl="$(python3 twirp_proto.py decode GetCacheEntryDownloadURL < "$WORK/body" | python3 -c 'import json, sys; print(json.load(sys.stdin)["signed_download_url"])')"
  [ "$(download_sha "$dl")" = "$pb_sha" ] || fail "v2 protobuf archive content after restart"
  pass "v2 protobuf archive restored after restart"
}

main() {
  if [ "${1:-}" = "--after-restart" ]; then
    run_after_restart
    echo "SMOKE PHASE 2 (after restart) PASSED"
    exit 0
  fi
  [ -z "${1:-}" ] || { echo "usage: $0 [--after-restart]" >&2; exit 2; }

  wait_health
  test_auth_and_limit
  test_v1
  test_v2_json
  test_blob_blocks
  test_v2_protobuf
  write_state
  echo "SMOKE PHASE 1 PASSED (state: $STATE_FILE)"
}

main "$@"
