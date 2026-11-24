#!/usr/bin/env bash
# E2E: mihomo v1.19.34 client -> meow-vgate VMess/mKCP server -> direct.
# Run from the repo root on a Linux CI runner with meow-app built at
# target/debug/meow and mihomo at /tmp/mihomo.
set -euo pipefail

ROOT="${GITHUB_WORKSPACE:-$(pwd)}"
MEOW="$ROOT/target/debug/meow"
MIHOMO="${MIHOMO_BIN:-/tmp/mihomo}"
SERVER_CFG="$ROOT/tests/e2e/server-vmess.yaml"
CLIENT_CFG="$ROOT/tests/e2e/client-vmess.yaml"

for f in "$MEOW" "$MIHOMO"; do
  if [ ! -x "$f" ]; then echo "missing executable: $f" >&2; exit 1; fi
done

cleanup() {
  kill "${SERVER_PID:-}" "${CLIENT_PID:-}" 2>/dev/null || true
}
trap cleanup EXIT

# 1. Start our server (VMess/mKCP inbound on UDP 19002).
"$MEOW" -f "$SERVER_CFG" >/tmp/meow-server-e2e.log 2>&1 &
SERVER_PID=$!
sleep 3
if ! kill -0 "$SERVER_PID" 2>/dev/null; then
  echo "meow server exited early:" >&2
  cat /tmp/meow-server-e2e.log >&2
  exit 1
fi

# 2. Start the mihomo client (VMess/mKCP outbound -> our server).
"$MIHOMO" -f "$CLIENT_CFG" >/tmp/meow-client-e2e.log 2>&1 &
CLIENT_PID=$!
sleep 3
if ! kill -0 "$CLIENT_PID" 2>/dev/null; then
  echo "mihomo client exited early:" >&2
  cat /tmp/meow-client-e2e.log >&2
  exit 1
fi

# 3. Request through the client; must traverse our VMess/mKCP server.
# Use a FIXED public IP (Cloudflare 1.1.1.1) with no Host override so the
# upstream tunnel routes by IP and needs no DNS — CI runners may block
# external DNS, which is unrelated to the server-inbound implementation.
# Any HTTP response (even 400) proves the full double-direction chain works.
echo "=== curl via VMess/mKCP chain (target 1.1.1.1) ==="
set +e
HTTP_CODE="$(curl -sS --max-time 25 \
  --proxy http://127.0.0.1:17891 \
  "http://1.1.1.1/" \
  -o /dev/null \
  -w '%{http_code}')"
CURL_STATUS=$?
set -e
echo "HTTP=$HTTP_CODE"
# 1.1.1.1 answers plain HTTP with 400; any non-zero code means bytes flowed
# through mihomo -> our mKCP/VMess server -> direct outbound and back.
if [ "$CURL_STATUS" -eq 0 ] && [ "$HTTP_CODE" != "000" ] && [ -n "$HTTP_CODE" ]; then
  CURL_STATUS=0
else
  CURL_STATUS=1
fi

echo "=== server log tail ==="
tail -60 /tmp/meow-server-e2e.log || true
echo "=== client log tail ==="
tail -60 /tmp/meow-client-e2e.log || true

exit "$CURL_STATUS"
