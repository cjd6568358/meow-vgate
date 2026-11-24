#!/usr/bin/env bash
# E2E: ONE meow process running BOTH server inbounds (VLESS/WS 19001 +
# VMess/mKCP 19002) from a SINGLE config.yaml, sharing one Arc<Tunnel>.
# This is the ADR 0002 drop-in path (meow-app --features server-inbound),
# distinct from the standalone server-bin smoke relay covered separately.
# Needs: target/debug/meow (built with --features server-inbound), /tmp/mihomo.
set -euo pipefail

ROOT="${GITHUB_WORKSPACE:-$(pwd)}"
MEOW="$ROOT/target/debug/meow"
MIHOMO="${MIHOMO_BIN:-/tmp/mihomo}"
BOTH_CFG="$ROOT/tests/e2e/server-both.yaml"
VLESS_CFG="$ROOT/tests/e2e/client-vless.yaml"
VMESS_CFG="$ROOT/tests/e2e/client-vmess.yaml"

for f in "$MEOW" "$MIHOMO" "$BOTH_CFG" "$VLESS_CFG" "$VMESS_CFG"; do
  if [ ! -e "$f" ]; then echo "missing: $f" >&2; exit 1; fi
done
if [ ! -x "$MEOW" ] || [ ! -x "$MIHOMO" ]; then echo "not executable" >&2; exit 1; fi

PIDS=""
cleanup() { kill $PIDS 2>/dev/null || true; }
trap cleanup EXIT

# 1. ONE meow process with BOTH listeners.
"$MEOW" -f "$BOTH_CFG" >/tmp/meow-both-server.log 2>&1 &
SERVER_PID=$!
PIDS="$PIDS $SERVER_PID"
sleep 3
if ! kill -0 "$SERVER_PID" 2>/dev/null; then
  echo "single-process meow exited early:" >&2
  cat /tmp/meow-both-server.log >&2
  exit 1
fi

# Both listeners must report started in the same process log.
echo "=== listener startup lines ==="
grep -E "VLESS/WS listener|VMess/mKCP listener" /tmp/meow-both-server.log || {
  echo "both listeners did not start" >&2; cat /tmp/meow-both-server.log >&2; exit 1;
}

# 2. Two mihomo clients, each to its own inbound (distinct mixed ports).
"$MIHOMO" -f "$VLESS_CFG" >/tmp/meow-both-vless-client.log 2>&1 &
PIDS="$PIDS $!"
"$MIHOMO" -f "$VMESS_CFG" >/tmp/meow-both-vmess-client.log 2>&1 &
PIDS="$PIDS $!"
sleep 3

# 3. Local targets (127.0.0.2 bypasses mihomo direct-connect exemption).
python3 -m http.server 18100 --bind 0.0.0.0 >/tmp/meow-both-http1.log 2>&1 &
PIDS="$PIDS $!"
python3 -m http.server 18101 --bind 0.0.0.0 >/tmp/meow-both-http2.log 2>&1 &
PIDS="$PIDS $!"
sleep 1

probe() {
  local name="$1" proxy_port="$2" target_port="$3"
  echo "=== $name via single-process meow (target 127.0.0.2:$target_port) ==="
  local code
  code="$(curl -sS --max-time 25 --proxy "http://127.0.0.1:$proxy_port" \
    "http://127.0.0.2:$target_port/" -o /dev/null -w '%{http_code}')" || code="000"
  echo "$name HTTP=$code"
  [ "$code" != "000" ] && [ -n "$code" ]
}

FAIL=0
probe "VLESS/WS 19001" 17892 18100 || FAIL=1
probe "VMess/mKCP 19002" 17891 18101 || FAIL=1

echo "=== server log tail ==="
tail -50 /tmp/meow-both-server.log || true

exit "$FAIL"
