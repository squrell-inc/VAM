#!/usr/bin/env bash
# Runs E2E scenarios: builds the binaries, then `vam-e2e run`.
#   scripts/e2e/run.sh --all | --smoke | --scenarios a,b | --tags chat [--budget 15m] [--out DIR] [--keep]
#   scripts/e2e/run.sh --firecracker [--out DIR] [--keep]
#   scripts/e2e/run.sh --bench --mock [-k N]   # the `model` scenarios k times on the mock; writes bench-results.json (never publishable)
#   scripts/e2e/run.sh --bench ... [--profile NAME] [--max-usd N]   # a harness profile for every trial; stop starting trials past N USD (e2e/README.md)
# Set VAM_E2E_SKIP_BUILD=1 to skip the build (then VAM_E2E_VAMD and VAMD_PROCESS_GUEST_BIN
# must name the process-mode vamd and guest; VAM_E2E_CLI the `vam` for verify-receipt steps). See design/scenarios.md section 7.
#
# The scenarios run on a TEST-ONLY node: vamd and vam-runtime are built with their
# `process-guest` feature (guests are host processes, no isolation; mock-model only).
# Those two binaries are copied to $CARGO_TARGET_DIR/e2e-bin so that an ordinary
# `cargo build` afterwards cannot replace them with default builds.
#
# --firecracker is the microVM lane: it builds the DEFAULT vamd (no process-guest) and runs every
# non-retired `needs: firecracker` scenario against it, with real Firecracker. The assets come from
# the environment (scripts/e2e/firecracker-assets.sh --env-file): VAMD_FIRECRACKER_BIN,
# VAMD_JAILER_BIN, VAM_KERNEL_PATH, VAM_ROOTFS_PATH. Needs /dev/kvm.
# The node runs the jailer (on by default), which needs root, so the run itself is root: build
# first as your user with `VAM_E2E_BUILD_ONLY=1 scripts/e2e/run.sh --firecracker`, then
#   sudo env "PATH=$PATH" "VAMD_FIRECRACKER_BIN=$VAMD_FIRECRACKER_BIN" ... VAM_E2E_SKIP_BUILD=1 \
#     VAM_E2E_VAMD=$PWD/target/e2e-bin-vm/vamd scripts/e2e/run.sh --firecracker
# (pass the asset variables explicitly: hosted runners ignore `sudo -E`)
# (an unprivileged run stops with a message saying so).
#
# After the run it checks that no guest left files behind (scripts/e2e/check-leftovers.sh);
# a leftover makes the script exit non-zero.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/../.."
if [ "$#" -eq 0 ]; then
  echo "usage: scripts/e2e/run.sh --all | --smoke | --scenarios a,b | --tags x [--budget 15m] [--out DIR] [--keep]" >&2
  exit 2
fi
fc=0
for a in "$@"; do [ "$a" = "--firecracker" ] && fc=1; done
target_dir="$(scripts/e2e/resolve-target-dir.sh)"
bin_dir="$target_dir/e2e-bin"

if [ "$fc" -eq 1 ]; then
  for v in VAMD_FIRECRACKER_BIN VAMD_JAILER_BIN VAM_KERNEL_PATH VAM_ROOTFS_PATH; do
    [ -n "${!v:-}" ] || { echo "run.sh: --firecracker needs $v (scripts/e2e/firecracker-assets.sh --env-file)" >&2; exit 2; }
  done
  if [ -z "${VAM_E2E_SKIP_BUILD:-}" ]; then
    # The default vamd: no process-guest feature, so the node reports guestMode: vm.
    cargo build -q -p vam-orchestrator -p vam-e2e
    mkdir -p "$target_dir/e2e-bin-vm"
    cp -f "$target_dir/debug/vamd" "$target_dir/e2e-bin-vm/vamd"
    export VAM_E2E_VAMD="$target_dir/e2e-bin-vm/vamd"
    # The effects test broker (built by vam-orchestrator): the kill-mid-task row reads its ledger.
    cp -f "$target_dir/debug/hsp-echo-broker" "$target_dir/e2e-bin-vm/hsp-echo-broker"
    export VAM_E2E_TEST_BROKER="$target_dir/e2e-bin-vm/hsp-echo-broker"
    if [ -n "${VAM_E2E_BUILD_ONLY:-}" ]; then
      echo "run.sh: built $VAM_E2E_VAMD and $target_dir/debug/vam-e2e"
      exit 0
    fi
  elif [ -z "${VAM_E2E_VAMD:-}" ]; then
    echo "run.sh: VAM_E2E_SKIP_BUILD=1 needs VAM_E2E_VAMD (a default vamd build)" >&2
    exit 2
  fi
elif [ -z "${VAM_E2E_SKIP_BUILD:-}" ]; then
  cargo build -q -p vam-orchestrator -p vam-runtime -p vam-cli -p vam-e2e -p vam-schedule \
    --features vam-orchestrator/process-guest,vam-runtime/process-guest
  # The fake-transport notification broker is built on its own and last: a build of the same
  # package without the feature would replace target/debug/vam-notify-broker.
  cargo build -q -p vam-notify --features fake-transport
  mkdir -p "$bin_dir"
  cp -f "$target_dir/debug/vamd" "$bin_dir/vamd"
  cp -f "$target_dir/debug/vam-runtime" "$bin_dir/vam-runtime"
  # The built `vam`, for the verify-receipt step (offline receipt and pack checks).
  cp -f "$target_dir/debug/vam" "$bin_dir/vam"
  # The test broker the `trigger-test-broker` piece starts (and `fire-trigger` drives).
  cp -f "$target_dir/debug/hsp-echo-broker" "$bin_dir/hsp-echo-broker"
  export VAM_E2E_TEST_BROKER="$bin_dir/hsp-echo-broker"
  # The schedule broker the `schedule-broker` piece starts.
  cp -f "$target_dir/debug/vam-schedule-broker" "$bin_dir/vam-schedule-broker"
  export VAM_E2E_SCHEDULE_BROKER="$bin_dir/vam-schedule-broker"
  # The notification broker the `notify-broker` piece starts: the fake-transport build (test only).
  cp -f "$target_dir/debug/vam-notify-broker" "$bin_dir/vam-notify-broker"
  export VAM_E2E_NOTIFY_BROKER="$bin_dir/vam-notify-broker"
  export VAM_E2E_CLI="$bin_dir/vam"
  export VAM_E2E_VAMD="$bin_dir/vamd"
  export VAMD_PROCESS_GUEST_BIN="$bin_dir/vam-runtime"
elif [ -z "${VAM_E2E_VAMD:-}" ] || [ -z "${VAMD_PROCESS_GUEST_BIN:-}" ]; then
  echo "run.sh: VAM_E2E_SKIP_BUILD=1 needs VAM_E2E_VAMD and VAMD_PROCESS_GUEST_BIN (process-guest builds)" >&2
  exit 2
fi

# A process guest run as root drops each skill tool to uid 1000 before exec (vam-runtime exec.rs).
# Skill tools run `/usr/local/bin/bun`; when that is a link into a root-only home (a cloud session
# installs bun under /root/.bun, mode 0700), the tool cannot start ("Failed to spawn child:
# Permission denied"), nothing reaches the fake providers, and the connector scenarios fail with
# 0 requests. Stop before the run with the fix instead.
if [ "$fc" -eq 0 ] && [ "$(id -u)" -eq 0 ] && [ -e /usr/local/bin/bun ] && command -v setpriv >/dev/null; then
  if ! setpriv --reuid=1000 --regid=100 --clear-groups sh -c '/usr/local/bin/bun --version' >/dev/null 2>&1; then
    echo "run.sh: /usr/local/bin/bun cannot be run by uid 1000, which skill tools drop to when vamd runs as root." >&2
    echo "        It resolves to $(readlink -f /usr/local/bin/bun). Install a copy outside the root-only home:" >&2
    echo "          install -m 0755 \"\$(readlink -f /usr/local/bin/bun)\" /usr/local/bin/bun.new && mv -f /usr/local/bin/bun.new /usr/local/bin/bun" >&2
    exit 2
  fi
fi

# The leftover check reads the vamd state dir, so run with --keep and an explicit
# --out, then remove the state dir again unless the caller asked to keep it.
out="" keep=0 args=()
while [ "$#" -gt 0 ]; do
  case "$1" in
    --out) out="$2"; shift 2 ;;
    --out=*) out="${1#--out=}"; shift ;;
    --keep) keep=1; shift ;;
    *) args+=("$1"); shift ;;
  esac
done
[ -n "$out" ] || out="$target_dir/e2e/$(date +%Y%m%d-%H%M%S)-$$"
mkdir -p "$out"

before="$(scripts/e2e/check-leftovers.sh --list)"
rc=0
"$target_dir/debug/vam-e2e" run "${args[@]}" --out "$out" --keep || rc=$?
lc=0
if [ "$fc" -eq 1 ]; then
  # Guests are microVMs: the leftover that matters is a Firecracker process that outlived vamd
  # (a zombie is already dead: its parent has not reaped it yet), and a jail directory.
  sleep 1
  live="$(ps -eo pid=,stat=,args= | awk '$2 !~ /^Z/ && $3 ~ /(^|\/)firecracker/' || true)"
  if [ -n "$live" ]; then
    echo "LEFTOVER: firecracker process still running after the run:"
    echo "$live"
    lc=1
  fi
  if [ -d /srv/jailer/firecracker ] && [ -n "$(ls -A /srv/jailer/firecracker 2>/dev/null)" ]; then
    echo "LEFTOVER: jail directories still under /srv/jailer/firecracker after the run:"
    ls -A /srv/jailer/firecracker
    lc=1
  fi
else
  scripts/e2e/check-leftovers.sh --after "$before" "$out" || lc=$?
fi
if [ "$keep" -eq 0 ] && [ "$rc" -eq 0 ] && [ "$lc" -eq 0 ]; then
  # vamd seals materialized/skills read-only (0444 files, 0555 dirs); a non-root
  # rm -rf cannot unlink inside those, so make the tree writable first.
  chmod -R u+w "$out/vamd/state" 2>/dev/null || true
  rm -rf "$out/vamd/state"
fi
if [ "$rc" -ne 0 ]; then exit "$rc"; fi
exit "$lc"
