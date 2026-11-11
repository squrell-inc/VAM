#!/usr/bin/env bash
# Benchmark VAM cold-start session creation with per-step timing aggregation.
#
# Requires vamd running with VAM_TIMING=1 and stderr redirected to the log file this
# script reads (default: ~/.cache/vam/vamd.log — not /tmp; a full tmpfs breaks writes).
# Each iteration uses a distinct workspace id.
#
# Start vamd (restart if VAM_TIMING was not set at process start). vamd serves TLS by
# default; this bench targets a local dev node on http://127.0.0.1:8080, so it starts
# with --insecure-dev (plain HTTP, loud warning, node reports TLS off) and, unless vamd
# runs as root, --no-jailer (the jailer is on by default and needs root; the node then
# reports `jailer off`):
#   mkdir -p ~/.cache/vam
#   : > ~/.cache/vam/vamd.log
#   VAM_TIMING=1 ./target/debug/vamd --insecure-dev --no-jailer 2>>~/.cache/vam/vamd.log &
# Against a TLS node instead: --host https://HOST:8080 and VAM_CA_FILE=<node ca.pem>
# (the file vamd keeps at <state dir>/tls/ca.pem), which is passed to curl as --cacert.
#
# If /tmp is full (e.g. cursor-sandbox-cache), free space or set TMPDIR before bench:
#   rm -rf /tmp/cursor-sandbox-cache   # if safe on your host
#   TMPDIR=$PWD/.tmp ./scripts/bench-coldstart.sh ...
#
# Usage:
#   ./scripts/bench-coldstart.sh --runs 10 --cache cold
#   VAM_KERNEL_PATH=/var/lib/vam/kernels/vmlinux-6.1 ./scripts/bench-coldstart.sh --runs 10 --cache warm
#
# API key resolution: --token, VAM_API_KEY, or api_key in ~/.config/vam/config.toml
# (same path as vam-cli).
#
# Options:
#   --runs N          Number of cold-start iterations (default: 10)
#   --cache MODE      cold|warm — cold clears session dirs first (default: cold)
#   --host URL        vamd base URL (default: http://127.0.0.1:8080, an --insecure-dev node)
#   --token KEY       Bearer token (default: $VAM_API_KEY)
#   --log FILE        vamd stderr log to parse (default: ~/.cache/vam/vamd.log)
#   --kernel PATH     Pin guest kernel via VAM_KERNEL_PATH on vamd (informational only here)
#   --workspace PREFIX  Workspace id prefix (default: bench-ws)
#   --wait-secs N     Seconds to wait for timing line per run (default: 120)
#
# Output: per-step p50/p95 in microseconds and human-readable units (µs / ms / s).

set -euo pipefail

# Strip NUL bytes vamd/sparse logs may contain; byte-offset tail misses writes inside holes.
sanitize_log() {
  tr -d '\000' < "${1}"
}

grep_phase_timing() {
  sanitize_log "${1}" | grep -E '"type"[[:space:]]*:[[:space:]]*"vam\.phase_timing"' || true
}

wait_for_workspace_timing() {
  local log_file="$1" workspace="$2" wait_secs="$3"
  local line=""
  for i in $(seq 1 "${wait_secs}"); do
    sleep 1
    if [[ -f "${log_file}" ]]; then
      line="$(sanitize_log "${log_file}" \
        | grep -E '"type"[[:space:]]*:[[:space:]]*"vam\.phase_timing"' \
        | grep -F "\"workspace_id\":\"${workspace}\"" \
        | tail -1 || true)"
      if [[ -n "${line}" ]]; then
        printf '%s\n' "${line}"
        return 0
      fi
    fi
    if (( i % 10 == 0 )); then
      echo "  ... still waiting for timing (${i}/${wait_secs}s, workspace=${workspace})" >&2
    fi
  done
  return 1
}

# tmpfs at 100% cannot grow truncated log files; vamd eprintln then fails silently.
check_filesystem_headroom() {
  local path="$1"
  local dir
  dir="$(dirname "${path}")"
  mkdir -p "${dir}" 2>/dev/null || true
  local use_pct avail_kb
  read -r _ _ _ avail_kb use_pct _ < <(df -Pk "${dir}" | tail -1)
  use_pct="${use_pct%%%}"
  if [[ "${use_pct}" -ge 99 ]]; then
    echo "ERROR: filesystem for ${dir} is ${use_pct}% full; vamd cannot append timing lines." >&2
    if [[ "${dir}" == /tmp* ]]; then
      echo "  /tmp is often consumed by cursor-sandbox-cache. Free space or use --log under ~." >&2
      echo "  Example: rm -rf /tmp/cursor-sandbox-cache  (only if safe on this host)" >&2
    fi
    return 1
  fi
  return 0
}

verify_log_appendable() {
  local log_file="$1"
  local probe="${log_file}.bench-write-probe"
  if ! printf 'vam.bench.probe\n' >> "${log_file}" 2>/dev/null; then
    echo "ERROR: cannot append to vamd log ${log_file} (disk full or permission denied)." >&2
    echo "  vamd timing uses eprintln (stderr); if the log cannot grow, the bench will hang." >&2
    echo "  Use a log path on a filesystem with free space, e.g. ~/.cache/vam/vamd.log" >&2
    return 1
  fi
  rm -f "${probe}"
  return 0
}

load_api_key_from_config() {
  local cfg="${1}"
  [[ -f "${cfg}" ]] || return 1
  python3 - "${cfg}" <<'PY'
import sys, re
path = sys.argv[1]
try:
    import tomllib
    with open(path, "rb") as f:
        data = tomllib.load(f)
    key = data.get("api_key") or ""
except Exception:
    text = open(path, encoding="utf-8").read()
    m = re.search(r'(?m)^\s*api_key\s*=\s*"([^"]+)"', text)
    key = m.group(1) if m else ""
if key:
    print(key)
PY
}

check_vamd_timing_preflight() {
  local log_file="$1"
  local vamd_pid timing_env=0 log_ok=0 stderr_target=""

  if vamd_pid="$(pgrep -xo vamd 2>/dev/null || true)"; then
    if tr '\0' '\n' < "/proc/${vamd_pid}/environ" 2>/dev/null | grep -qx 'VAM_TIMING=1'; then
      timing_env=1
    fi
    stderr_target="$(readlink -f "/proc/${vamd_pid}/fd/2" 2>/dev/null || true)"
  fi

  if [[ -f "${log_file}" ]]; then
    if grep_phase_timing "${log_file}" | grep -q 'vam.phase_timing'; then
      log_ok=1
    fi
    if python3 - "${log_file}" <<'PY'
import sys
data = open(sys.argv[1], "rb").read()
if b"\x00" in data:
    raise SystemExit(0)
raise SystemExit(1)
PY
    then
      echo "WARNING: ${log_file} contains NUL/sparse regions; using workspace grep (not byte offsets)." >&2
    fi
  fi

  if [[ "${timing_env}" -ne 1 ]]; then
    echo "ERROR: vamd is not running with VAM_TIMING=1." >&2
    echo "  Restart: mkdir -p $(dirname "${log_file}") && VAM_TIMING=1 ./target/debug/vamd 2>>${log_file} &" >&2
    return 1
  fi

  if [[ ! -f "${log_file}" ]]; then
    echo "ERROR: vamd log ${log_file} not found. Start vamd with stderr redirected, e.g.:" >&2
    echo "  mkdir -p $(dirname "${log_file}")" >&2
    echo "  VAM_TIMING=1 ./target/debug/vamd 2>>${log_file} &" >&2
    return 1
  fi

  if [[ -n "${stderr_target}" ]]; then
    local log_resolved
    log_resolved="$(readlink -f "${log_file}" 2>/dev/null || echo "${log_file}")"
    if [[ "${stderr_target}" != "${log_resolved}" ]]; then
      echo "ERROR: running vamd stderr is ${stderr_target}, not ${log_resolved}." >&2
      echo "  Restart vamd so stderr matches --log / VAMD_LOG, e.g.:" >&2
      echo "  pkill -x vamd; VAM_TIMING=1 ./target/debug/vamd 2>>${log_file} &" >&2
      return 1
    fi
  fi

  if ! check_filesystem_headroom "${log_file}"; then
    return 1
  fi
  if ! verify_log_appendable "${log_file}"; then
    return 1
  fi

  if [[ "${log_ok}" -ne 1 ]]; then
    echo "NOTE: no vam.phase_timing lines in ${log_file} yet (expected before first run)." >&2
    echo "      Ensure vamd stderr is redirected to this file, not only stdout or a TTY." >&2
  fi

  return 0
}

RUNS=10
CACHE_MODE="cold"
HOST="${VAM_HOST:-http://127.0.0.1:8080}"
TOKEN="${VAM_API_KEY:-}"
DEFAULT_VAMD_LOG="${XDG_CACHE_HOME:-${HOME}/.cache}/vam/vamd.log"
LOG_FILE="${VAMD_LOG:-${DEFAULT_VAMD_LOG}}"
WORKSPACE_PREFIX="bench-ws"
WAIT_SECS=120
KERNEL_PIN="${VAM_KERNEL_PATH:-}"
# Trust the node's own certificate for https:// hosts.
CURL_CA=()
if [[ -n "${VAM_CA_FILE:-}" ]]; then CURL_CA=(--cacert "${VAM_CA_FILE}"); fi

while [[ $# -gt 0 ]]; do
  case "$1" in
    --runs) RUNS="$2"; shift 2 ;;
    --cache) CACHE_MODE="$2"; shift 2 ;;
    --host) HOST="$2"; shift 2 ;;
    --token) TOKEN="$2"; shift 2 ;;
    --log) LOG_FILE="$2"; shift 2 ;;
    --kernel) KERNEL_PIN="$2"; shift 2 ;;
    --workspace) WORKSPACE_PREFIX="$2"; shift 2 ;;
    --wait-secs) WAIT_SECS="$2"; shift 2 ;;
    -h|--help)
      sed -n '2,22p' "$0"
      exit 0
      ;;
    *)
      echo "Unknown option: $1" >&2
      exit 1
      ;;
  esac
done

if [[ -z "${TOKEN}" ]]; then
  for cfg in "${XDG_CONFIG_HOME:-${HOME}/.config}/vam/config.toml" "${HOME}/.vam/config.toml"; do
    TOKEN="$(load_api_key_from_config "${cfg}" 2>/dev/null || true)"
    [[ -n "${TOKEN}" ]] && break
  done
fi

if [[ -z "${TOKEN}" ]]; then
  echo "ERROR: set VAM_API_KEY, pass --token, or configure api_key via vam-cli (~/.config/vam/config.toml)" >&2
  exit 1
fi

if [[ "${CACHE_MODE}" == "cold" ]]; then
  echo "=== Clearing stale session dirs for cold cache ==="
  sudo rm -rf /var/lib/vam/drive9/sessions/* 2>/dev/null || true
fi

DRIVE9_ROOT="${VAM_DRIVE9_ROOT:-/var/lib/vam/drive9}"
mkdir -p "${DRIVE9_ROOT}/cache/files" "${DRIVE9_ROOT}/sessions"

echo "=== Probing reflink support on XFS ==="
REFLINK_PROBE_DIR="${DRIVE9_ROOT}/cache"
PROBE_SRC="${REFLINK_PROBE_DIR}/.bench-probe-src"
PROBE_DST="${REFLINK_PROBE_DIR}/.bench-probe-dst"
echo probe > "${PROBE_SRC}"
if cp --reflink=always "${PROBE_SRC}" "${PROBE_DST}" 2>/dev/null; then
  REFLINK="yes"
else
  REFLINK="no"
fi
rm -f "${PROBE_SRC}" "${PROBE_DST}"
echo "cp --reflink=always on ${REFLINK_PROBE_DIR}: ${REFLINK}"

if [[ -n "${KERNEL_PIN}" ]]; then
  echo "NOTE: pin kernel on vamd with: export VAM_KERNEL_PATH=${KERNEL_PIN}"
fi

if ! curl -sf ${CURL_CA[@]+"${CURL_CA[@]}"} "${HOST}/v1/health" >/dev/null; then
  echo "ERROR: vamd not reachable at ${HOST}/v1/health" >&2
  echo "  vamd serves TLS by default: start it with --insecure-dev for an http:// host," >&2
  echo "  or pass --host https://... and set VAM_CA_FILE to the node's ca.pem." >&2
  exit 1
fi

check_vamd_timing_preflight "${LOG_FILE}"

BENCH_TMP="${TMPDIR:-/tmp}"
if ! check_filesystem_headroom "${BENCH_TMP}/." 2>/dev/null; then
  BENCH_TMP="${PWD}/.tmp"
  mkdir -p "${BENCH_TMP}"
  echo "NOTE: using TMPDIR=${BENCH_TMP} (default /tmp lacks free space)" >&2
fi
TIMING_FILE="$(mktemp "${BENCH_TMP}/vam-bench-timings.XXXXXX")"
: > "${TIMING_FILE}"

agent_json='{
  "spec": {
    "resourceId": {"urn": "urn:vam:agent:local.vam.dev/acme/bench-agent"},
    "displayName": "Bench Agent",
    "description": "cold-start benchmark",
    "role": "Tester",
    "systemInstructions": "Reply with OK.",
    "allowAnyPlaybook": true
  },
  "idempotencyKey": "bench-agent-setup"
}'

curl -sf ${CURL_CA[@]+"${CURL_CA[@]}"} -X PUT "${HOST}/v1/agents" \
  -H "authorization: Bearer ${TOKEN}" \
  -H "Idempotency-Key: bench-agent-setup" \
  -H "content-type: application/json" \
  -d "${agent_json}" >/dev/null || true

for i in $(seq 1 "${RUNS}"); do
  WS="${WORKSPACE_PREFIX}-${i}-$(date +%s)"
  IDEM="bench-run-${i}-$(date +%s%N)"
  RUN_URN="urn:vam:run:local.vam.dev/acme/bench-${i}-$(date +%s)"

  run_body="$(cat <<EOF
{
  "spec": {
    "runId": {"urn": "${RUN_URN}"},
    "agentRef": {
      "resourceId": {"urn": "urn:vam:agent:local.vam.dev/acme/bench-agent"},
      "version": {"major": 1, "minor": 0, "patch": 0},
      "digest": {"algorithm": "ALGORITHM_SHA256", "hexHash": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"}
    },
    "inputs": {"fields": {"prompt": {"stringValue": "bench"}}},
    "workspaceRefUri": "volume://${WS}",
    "idempotencyKey": "${IDEM}"
  },
  "inlineAgent": {
    "resourceId": {"urn": "urn:vam:agent:local.vam.dev/acme/bench-agent"},
    "displayName": "Bench Agent",
    "description": "bench",
    "role": "Tester",
    "systemInstructions": "Reply OK",
    "allowAnyPlaybook": true
  }
}
EOF
)"

  echo "=== Run ${i}/${RUNS} workspace=${WS} ==="

  http_code="$(curl -s ${CURL_CA[@]+"${CURL_CA[@]}"} -o /dev/null -w '%{http_code}' -X POST "${HOST}/v1/runs" \
    -H "authorization: Bearer ${TOKEN}" \
    -H "Idempotency-Key: ${IDEM}" \
    -H "content-type: application/json" \
    -d "${run_body}")"

  if [[ "${http_code}" != "202" ]]; then
    echo "ERROR: POST /v1/runs returned ${http_code}" >&2
    exit 1
  fi

  timing_line=""
  if ! timing_line="$(wait_for_workspace_timing "${LOG_FILE}" "${WS}" "${WAIT_SECS}")"; then
    echo "ERROR: timed out waiting for timing line on run ${i} (workspace=${WS})" >&2
    echo "  vamd log: ${LOG_FILE} (expect vam.phase_timing JSON on stderr with matching workspace_id)" >&2
    if [[ -f "${LOG_FILE}" ]]; then
      echo "  --- last log lines (NUL-stripped) ---" >&2
      sanitize_log "${LOG_FILE}" | tail -20 >&2 || true
    fi
    exit 1
  fi
  printf '%s\n' "${timing_line}" >> "${TIMING_FILE}"
done

echo "=== Aggregating p50/p95 (${RUNS} runs, cache=${CACHE_MODE}, reflink=${REFLINK}) ==="

python3 - "${TIMING_FILE}" "${CACHE_MODE}" "${REFLINK}" <<'PY'
import json, sys
from collections import defaultdict

path, cache_mode, reflink = sys.argv[1], sys.argv[2], sys.argv[3]
rows = []
with open(path) as f:
    for line in f:
        line = line.strip()
        if not line:
            continue
        rows.append(json.loads(line))

if not rows:
    print("No timing rows collected", file=sys.stderr)
    sys.exit(1)

steps = defaultdict(list)
totals = []
kernels = set()
for row in rows:
    totals.append(row.get("total_us", 0))
    if row.get("kernel_path"):
        kernels.add(row["kernel_path"])
    for k, v in row.get("steps", {}).items():
        steps[k].append(int(v))

def pct(vals, p):
    if not vals:
        return 0
    s = sorted(vals)
    idx = max(0, min(len(s) - 1, int(round(p / 100.0 * (len(s) - 1)))))
    return s[idx]

def fmt_us(us):
    """Format microseconds as us + human-readable ms/s."""
    us = int(us)
    if us < 1000:
        human = f"{us} us" if us < 100 else f"{us / 10:.1f} us"
    elif us < 1_000_000:
        human = f"{us / 1000:.2f} ms"
    else:
        human = f"{us / 1_000_000:.2f} s"
    return f"{us} us  {human}"

def fmt_dual(p50, p95):
    return f"p50={fmt_us(p50)}  p95={fmt_us(p95)}"

print(f"runs={len(rows)} cache={cache_mode} reflink={reflink} kernels={sorted(kernels)}")
print(f"total  {fmt_dual(pct(totals, 50), pct(totals, 95))}")
print()
print(f"{'step':<32}  {'p50':>12}  {'human p50':>12}  {'p95':>12}  {'human p95':>12}")
for name in sorted(steps):
    vals = steps[name]
    p50, p95 = pct(vals, 50), pct(vals, 95)
    p50_us, p95_us = int(p50), int(p95)
    if p50_us < 1000:
        p50_h = f"{p50_us / 10:.1f} us" if p50_us < 100 else f"{p50_us} us"
    elif p50_us < 1_000_000:
        p50_h = f"{p50_us / 1000:.2f} ms"
    else:
        p50_h = f"{p50_us / 1_000_000:.2f} s"
    if p95_us < 1000:
        p95_h = f"{p95_us / 10:.1f} us" if p95_us < 100 else f"{p95_us} us"
    elif p95_us < 1_000_000:
        p95_h = f"{p95_us / 1000:.2f} ms"
    else:
        p95_h = f"{p95_us / 1_000_000:.2f} s"
    print(f"{name:<32}  {p50_us:>12}  {p50_h:>12}  {p95_us:>12}  {p95_h:>12}")
PY

rm -f "${TIMING_FILE}"
echo "=== Done ==="
