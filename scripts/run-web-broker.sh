#!/usr/bin/env bash
# Run and register the first-party web broker (vam-web-broker, broker id vam.web).
#
#   BRAVE_API_KEY=... scripts/run-web-broker.sh      real search (Brave), key stored in the vault
#   scripts/run-web-broker.sh --mock                 fixtures, no key, no network (benchmarks, demos)
#   scripts/run-web-broker.sh --dry-run              print the registration request and exit
#   scripts/run-web-broker.sh --stop                 stop the broker this script started
#
# Where it runs: on the vamd host, as the SAME USER as vamd. The broker listens on a Unix socket with
# mode 0600 and vamd dials it, so a different user cannot connect. On a Mac, vamd runs inside Lima:
# run this script inside the Lima VM (the repo is mounted there), not on macOS.
#
# The search key: if BRAVE_API_KEY is set it is stored ONCE in the vamd vault as `web/brave-api-key`
# (kind inference, scope org: a kind the guest egress proxy never injects). The broker process never
# sees the key in its environment or arguments; vamd sends it with each web_search call. The key is
# never printed.
#
# Options:
#   --mock          mock provider and the fixtures in scripts/bench/fixtures/web (a canary value is
#                   stored as the key and written to $STATE_DIR/canary; it is not a real secret)
#   --fixtures DIR  fixtures for --mock
#   --foreground    run the broker in the foreground (Ctrl-C stops it)
#   --stop          stop the broker started by this script
#   --dry-run       print the request body (no key, no network) and exit
#   -h, --help      this text
#
# Environment:
#   VAMD_URL        vamd base URL (default http://127.0.0.1:8080: a dev node started with
#                   --insecure-dev; vamd serves TLS by default, so use https:// plus
#                   curl's CURL_CA_BUNDLE for a TLS node)
#   VAMD_ADMIN_KEY  API key with brokers:admin (default: $VAM_API_KEY; the dev bootstrap key works)
#   VAM             vam CLI (default: vam on PATH; must be logged in to the same vamd)
#   VAM_WEB_SOCKET  socket path (default /tmp/vam-web.sock)
#   VAM_WEB_BIN     path to vam-web-broker (default: built from this repo if not found)
#   VAM_WEB_STATE_DIR  pid, log and canary files (default /tmp/vam-web-broker)
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/.." && pwd)"
MANIFEST="$HERE/web-broker/manifest.json"
SOCKET="${VAM_WEB_SOCKET:-/tmp/vam-web.sock}"
STATE_DIR="${VAM_WEB_STATE_DIR:-/tmp/vam-web-broker}"
VAMD_URL="${VAMD_URL:-http://127.0.0.1:8080}"
VAM="${VAM:-vam}"
KEY_BINDING="web/brave-api-key"

mock=0 foreground=0 stop=0 dry=0 fixtures="$REPO/scripts/bench/fixtures/web"
while [ $# -gt 0 ]; do
  case "$1" in
    --mock) mock=1; shift ;;
    --fixtures) fixtures="$2"; shift 2 ;;
    --foreground) foreground=1; shift ;;
    --stop) stop=1; shift ;;
    --dry-run) dry=1; shift ;;
    -h|--help) sed -n '2,/^set -euo/p' "${BASH_SOURCE[0]}" | sed '$d' | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "run-web-broker.sh: unknown argument: $1" >&2; exit 2 ;;
  esac
done

command -v jq >/dev/null 2>&1 || { echo "run-web-broker.sh: jq is required" >&2; exit 2; }

request_body() {
  jq --arg path "$SOCKET" '.unixPath = $path' "$MANIFEST"
}

if [ "$dry" = 1 ]; then
  echo "# PUT $VAMD_URL/v1/hosted/brokers (Idempotency-Key: <generated>)" >&2
  request_body
  exit 0
fi

PID_FILE="$STATE_DIR/broker.pid"
if [ "$stop" = 1 ]; then
  if [ -f "$PID_FILE" ] && kill -0 "$(cat "$PID_FILE")" 2>/dev/null; then
    kill "$(cat "$PID_FILE")"
    echo "stopped vam-web-broker (pid $(cat "$PID_FILE"))"
  else
    echo "no running vam-web-broker from this script"
  fi
  rm -f "$PID_FILE" "$SOCKET"
  exit 0
fi

command -v curl >/dev/null 2>&1 || { echo "run-web-broker.sh: curl is required" >&2; exit 2; }
command -v "$VAM" >/dev/null 2>&1 || { echo "run-web-broker.sh: vam CLI not found: set VAM=/path/to/vam" >&2; exit 2; }
ADMIN_KEY="${VAMD_ADMIN_KEY:-${VAM_API_KEY:-}}"
[ -n "$ADMIN_KEY" ] || { echo "run-web-broker.sh: set VAMD_ADMIN_KEY (an API key with brokers:admin)" >&2; exit 2; }

# 1. Find or build the broker binary.
bin="${VAM_WEB_BIN:-}"
if [ -z "$bin" ]; then
  for cand in "$REPO/target/release/vam-web-broker" "$REPO/target/debug/vam-web-broker"; do
    [ -x "$cand" ] && { bin="$cand"; break; }
  done
fi
if [ -z "$bin" ]; then
  echo "building vam-web-broker ..."
  (cd "$REPO" && cargo build -p vam-web --bin vam-web-broker)
  bin="$REPO/target/debug/vam-web-broker"
fi
[ -x "$bin" ] || { echo "run-web-broker.sh: broker binary not found: $bin" >&2; exit 2; }

mkdir -p "$STATE_DIR"
chmod 700 "$STATE_DIR"

# 2. Store the search key in the vault. The value is read from the environment only by the vam CLI.
if [ "$mock" = 1 ]; then
  canary="BENCH_SECRET_CANARY_$(od -An -N8 -tx1 /dev/urandom | tr -d ' \n')"
  ( umask 077; printf '%s' "$canary" > "$STATE_DIR/canary" )
  VAM_WEB_KEY_VALUE="$canary" "$VAM" secrets set "$KEY_BINDING" --kind inference --scope org \
    --from-env VAM_WEB_KEY_VALUE >/dev/null
  echo "stored a canary value as $KEY_BINDING (mock mode)"
elif [ -n "${BRAVE_API_KEY:-}" ]; then
  "$VAM" secrets set "$KEY_BINDING" --kind inference --scope org --from-env BRAVE_API_KEY >/dev/null
  echo "stored $KEY_BINDING in the vault"
else
  echo "BRAVE_API_KEY is not set: assuming $KEY_BINDING is already in the vault (vam secrets list --scope org)"
fi

# 3. Start the broker. It gets no key: not in its arguments and not in its environment.
args=(--socket "$SOCKET")
if [ "$mock" = 1 ]; then
  [ -d "$fixtures" ] || { echo "run-web-broker.sh: fixtures not found: $fixtures" >&2; exit 2; }
  args+=(--provider mock --mock-fixtures "$fixtures")
else
  args+=(--provider brave)
fi
wait_for_socket() {
  for _ in $(seq 1 50); do
    [ -S "$SOCKET" ] && return 0
    sleep 0.2
  done
  echo "run-web-broker.sh: socket did not appear: $SOCKET" >&2
  return 1
}

# Register with vamd (operator approval + dial), then check both tools are listed.
register() {
  local idem status list tool
  idem="web-broker-$(od -An -N8 -tx1 /dev/urandom | tr -d ' \n')"
  status="$(request_body | curl -sS -o "$STATE_DIR/put.out" -w '%{http_code}' -X PUT "$VAMD_URL/v1/hosted/brokers" \
    -H "Authorization: Bearer $ADMIN_KEY" -H "Idempotency-Key: $idem" -H 'Content-Type: application/json' \
    --data-binary @-)"
  if [ "$status" != "200" ]; then
    echo "run-web-broker.sh: registration failed (HTTP $status):" >&2
    head -c 600 "$STATE_DIR/put.out" >&2; echo >&2
    return 1
  fi
  list="$(curl -sS "$VAMD_URL/v1/hosted/brokers" -H "Authorization: Bearer $ADMIN_KEY")"
  for tool in web_search web_fetch; do
    printf '%s' "$list" | grep -q "$tool" || { echo "run-web-broker.sh: $tool is not listed by vamd" >&2; return 1; }
  done
  print_next_steps
}

print_next_steps() {
  cat <<'NEXT'
vam.web registered: web_search and web_fetch are listed.
Next: turn web on for an agent (it is off by default):
  vam agent permissions web <agent> on
Stop the broker with: scripts/run-web-broker.sh --stop
NEXT
}

if [ -f "$PID_FILE" ] && kill -0 "$(cat "$PID_FILE")" 2>/dev/null; then
  kill "$(cat "$PID_FILE")" 2>/dev/null || true
  sleep 0.5
fi
rm -f "$SOCKET"

if [ "$foreground" = 1 ]; then
  echo "starting vam-web-broker in the foreground on $SOCKET (Ctrl-C stops it)"
  ( wait_for_socket && register ) &
  exec env -u BRAVE_API_KEY -u VAM_WEB_KEY_VALUE "$bin" "${args[@]}"
fi

env -u BRAVE_API_KEY -u VAM_WEB_KEY_VALUE nohup "$bin" "${args[@]}" >"$STATE_DIR/broker.log" 2>&1 &
echo $! > "$PID_FILE"
if ! wait_for_socket; then
  echo "see $STATE_DIR/broker.log" >&2
  exit 1
fi
register
