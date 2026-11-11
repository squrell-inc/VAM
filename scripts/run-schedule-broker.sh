#!/usr/bin/env bash
# Run and register the first-party schedule broker (vam-schedule-broker, broker id vam.schedule).
#
#   scripts/run-schedule-broker.sh             build, start, register, start again with the gateway socket
#   scripts/run-schedule-broker.sh --dry-run   print the registration request and exit
#   scripts/run-schedule-broker.sh --stop      stop the broker this script started
#
# Where it runs: on the vamd host, as the SAME USER as vamd (the broker's socket is mode 0600 and
# vamd dials it). On a Mac, vamd runs inside Lima: run this script inside the Lima VM, where the
# repo is mounted, not on macOS.
#
# Steps: (1) find or build the broker; (2) start it with --socket and --state-dir; (3) register
# its manifest (printed by `vam-schedule-broker manifest`, the one source) with
# PUT /v1/hosted/brokers; (4) read gatewaySocket from GET /v1/hosted/brokers/vam.schedule;
# (5) restart the broker with --gateway-socket, which is how it starts schedules and withdraws
# approvals; (6) check all six tools are listed. The broker needs no secrets.
#
# Options:
#   --foreground    run the final broker process in the foreground (Ctrl-C stops it)
#   --stop          stop the broker started by this script
#   --dry-run       print the request body (no network) and exit
#   -h, --help      this text
#
# Environment:
#   VAMD_URL        vamd base URL (default http://127.0.0.1:8080, a dev node started with --insecure-dev;
#                   use https:// plus CURL_CA_BUNDLE for a TLS node)
#   VAMD_ADMIN_KEY  API key with brokers:admin (default: $VAM_API_KEY)
#   VAM_SCHEDULE_SOCKET     socket path (default /tmp/vam-schedule.sock)
#   VAM_SCHEDULE_BIN        path to vam-schedule-broker (default: built from this repo if not found)
#   VAM_SCHEDULE_STATE_DIR  database, lock, pid and log (default /tmp/vam-schedule-broker)
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/.." && pwd)"
SOCKET="${VAM_SCHEDULE_SOCKET:-/tmp/vam-schedule.sock}"
STATE_DIR="${VAM_SCHEDULE_STATE_DIR:-/tmp/vam-schedule-broker}"
VAMD_URL="${VAMD_URL:-http://127.0.0.1:8080}"
BROKER_ID="vam.schedule"

foreground=0 stop=0 dry=0
while [ $# -gt 0 ]; do
  case "$1" in
    --foreground) foreground=1; shift ;;
    --stop) stop=1; shift ;;
    --dry-run) dry=1; shift ;;
    -h|--help) sed -n '2,/^set -euo/p' "${BASH_SOURCE[0]}" | sed '$d' | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "run-schedule-broker.sh: unknown argument: $1" >&2; exit 2 ;;
  esac
done

die() { echo "run-schedule-broker.sh: $1" >&2; exit "${2:-1}"; }
command -v jq >/dev/null 2>&1 || die "jq is required" 2

PID_FILE="$STATE_DIR/broker.pid"

stop_broker() {
  local pid
  if [ -f "$PID_FILE" ] && kill -0 "$(cat "$PID_FILE")" 2>/dev/null; then
    pid="$(cat "$PID_FILE")"
    kill "$pid" 2>/dev/null || true
    for _ in $(seq 1 25); do kill -0 "$pid" 2>/dev/null || break; sleep 0.2; done
    return 0
  fi
  return 1
}

if [ "$stop" = 1 ]; then
  if stop_broker; then echo "stopped vam-schedule-broker"; else echo "no running vam-schedule-broker from this script"; fi
  rm -f "$PID_FILE" "$SOCKET"
  exit 0
fi

# 1. Find or build the broker binary.
bin="${VAM_SCHEDULE_BIN:-}"
if [ -z "$bin" ]; then
  for cand in "$REPO/target/release/vam-schedule-broker" "$REPO/target/debug/vam-schedule-broker"; do
    [ -x "$cand" ] && { bin="$cand"; break; }
  done
fi
if [ -z "$bin" ]; then
  echo "building vam-schedule-broker ..."
  (cd "$REPO" && cargo build -p vam-schedule --bin vam-schedule-broker)
  bin="$REPO/target/debug/vam-schedule-broker"
fi
[ -x "$bin" ] || die "broker binary not found: $bin" 2

request_body() {
  "$bin" manifest | jq --arg path "$SOCKET" '{manifest: ., unixPath: $path}'
}

if [ "$dry" = 1 ]; then
  echo "# PUT $VAMD_URL/v1/hosted/brokers (Idempotency-Key: <generated>)" >&2
  request_body
  exit 0
fi

command -v curl >/dev/null 2>&1 || die "curl is required" 2
ADMIN_KEY="${VAMD_ADMIN_KEY:-${VAM_API_KEY:-}}"
[ -n "$ADMIN_KEY" ] || die "set VAMD_ADMIN_KEY (an API key with brokers:admin)" 2

mkdir -p "$STATE_DIR"
chmod 700 "$STATE_DIR"

wait_for_socket() {
  for _ in $(seq 1 50); do
    [ -S "$SOCKET" ] && return 0
    sleep 0.2
  done
  echo "run-schedule-broker.sh: socket did not appear: $SOCKET (see $STATE_DIR/broker.log)" >&2
  return 1
}

start_broker() { # [extra args...]; background, logs to broker.log
  rm -f "$SOCKET"
  nohup "$bin" --socket "$SOCKET" --state-dir "$STATE_DIR/state" "$@" >>"$STATE_DIR/broker.log" 2>&1 &
  echo $! > "$PID_FILE"
  wait_for_socket
}

curl_vamd() { curl -sS -H "Authorization: Bearer $ADMIN_KEY" "$@"; }

# 2. Start (stopping an earlier one from this script first).
stop_broker || true
start_broker || exit 1

# 3. Register: operator approval, and vamd dials the socket.
idem="schedule-broker-$(od -An -N8 -tx1 /dev/urandom | tr -d ' \n')"
status="$(request_body | curl_vamd -o "$STATE_DIR/put.out" -w '%{http_code}' -X PUT "$VAMD_URL/v1/hosted/brokers" \
  -H "Idempotency-Key: $idem" -H 'Content-Type: application/json' --data-binary @-)"
if [ "$status" != "200" ]; then
  echo "run-schedule-broker.sh: registration failed (HTTP $status):" >&2
  head -c 600 "$STATE_DIR/put.out" >&2; echo >&2
  exit 1
fi

# 4. vamd opens the trigger gateway only after the registration and reports where.
gateway="$(curl_vamd "$VAMD_URL/v1/hosted/brokers/$BROKER_ID" | jq -r '.broker.gatewaySocket // empty')"
[ -n "$gateway" ] || die "vamd reports no gatewaySocket for $BROKER_ID (is run-request permission approved?)"

# 5. Start again with the gateway socket.
stop_broker || true
if [ "$foreground" = 1 ]; then
  rm -f "$SOCKET" "$PID_FILE"
  echo "$BROKER_ID registered; starting in the foreground on $SOCKET (Ctrl-C stops it)"
  exec "$bin" --socket "$SOCKET" --state-dir "$STATE_DIR/state" --gateway-socket "$gateway"
fi
start_broker --gateway-socket "$gateway" || exit 1

# 6. All six tools listed?
list="$(curl_vamd "$VAMD_URL/v1/hosted/brokers")"
for tool in schedule_create schedule_list schedule_get schedule_update schedule_delete schedule_run_now; do
  printf '%s' "$list" | grep -q "$tool" || die "$tool is not listed by vamd"
done

cat <<NEXT
$BROKER_ID registered: six schedule tools listed; gateway $gateway.
Next, for each agent that may schedule work, allow schedule_create, schedule_update,
schedule_delete and schedule_run_now (ask-before-use is the default for them) and keep
schedule_list and schedule_get on. Deleting always asks.
Stop the broker with: scripts/run-schedule-broker.sh --stop
NEXT
