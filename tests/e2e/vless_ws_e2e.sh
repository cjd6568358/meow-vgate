#!/usr/bin/env bash
# E2E: mihomo v1.19.34 client -> meow-vgate VLESS/WS server -> direct.
# Run from the repo root on a Linux CI runner. Needs:
#   MEOW_SERVER_BIN  - built meow-server binary (VLESS/WS smoke relay)
#   MIHOMO_BIN       - mihomo v1.19.34 binary (default /tmp/mihomo)
set -euo pipefail

ROOT="${GITHUB_WORKSPACE:-$(pwd)}"
SERVER_BIN="${MEOW_SERVER_BIN:-$ROOT/server/target/debug/meow-server}"
MIHOMO="${MIHOMO_BIN:-/tmp/mihomo}"
CLIENT_CFG="$ROOT/tests/e2e/client-vless.yaml"
UUID="00000000-0000-4000-8000-000000000001"

for f in "$SERVER_BIN" "$MIHOMO"; do
  if [ ! -x "$f" ]; then echo "missing executable: $f" >&2; exit 1; fi
done

cleanup() {
  kill "${SERVER_PID:-}" "${CLIENT_PID:-}" "${HTTP_PID:-}" 2>/dev/null || true
}
trap cleanup EXIT

# 1. Start our VLESS/WS server (TCP 19001, /ws, smoke relay to upstream).
"$SERVER_BIN" --vless-ws 127.0.0.1:19001 --ws-path /ws \
  --uuid "$UUID" --relay >/tmp/meow-vless-server-e2e.log 2>&1 &
SERVER_PID=$!
sleep 2
if ! kill -0 "$SERVER_PID" 2>/dev/null; then
  echo "VLESS server exited early:" >&2
  cat /tmp/meow-vless-server-e2e.log >&2
  exit 1
fi

# 2. Start the mihomo client (VLESS/WS outbound -> our server).
"$MIHOMO" -f "$CLIENT_CFG" >/tmp/meow-vless-client-e2e.log 2>&1 &
CLIENT_PID=$!
sleep 2
if ! kill -0 "$CLIENT_PID" 2>/dev/null; then
  echo "mihomo client exited early:" >&2
  cat /tmp/meow-vless-client-e2e.log >&2
  exit 1
fi

# 3. Local target on 127.0.0.2 (127.0.0.1 is a mihomo direct bypass). A
# non-000 HTTP code proves bytes crossed mihomo -> VLESS/WS -> relay and back.
python3 -m http.server 18100 --bind 0.0.0.0 >/tmp/meow-vless-http-e2e.log 2>&1 &
HTTP_PID=$!
sleep 1
echo "=== curl via VLESS/WS chain (target 127.0.0.2:18100) ==="
set +e
HTTP_CODE="$(curl -sS --max-time 25 \
  --proxy http://127.0.0.1:17892 \
  "http://127.0.0.2:18100/" \
  -o /dev/null \
  -w '%{http_code}')"
CURL_STATUS=$?
set -e
kill "$HTTP_PID" 2>/dev/null || true
echo "HTTP=$HTTP_CODE"

echo "=== server log tail ==="
tail -40 /tmp/meow-vless-server-e2e.log || true
echo "=== client log tail ==="
tail -40 /tmp/meow-vless-client-e2e.log || true

if [ "$CURL_STATUS" -eq 0 ] && [ "$HTTP_CODE" != "000" ] && [ -n "$HTTP_CODE" ]; then
  exit 0
fi
exit 1
