#!/usr/bin/env bash
# Runs the GitHub `Rust` workflow's jobs in this session, with the same build
# settings CI uses, so a PR does not need GitHub Actions minutes unless it
# touches the microVM path or is a release into `main`.
#
#   scripts/ci/local.sh --fast            fmt, example protos, manifests, cargo check,
#                                         the smoke scenarios            (a wave's own check)
#   scripts/ci/local.sh --full            --fast plus the clippy gate (default and process-guest
#                                         features, -D warnings, pinned toolchain) and
#                                         `cargo test --workspace --no-fail-fast`. Every wave
#                                         runs --full before its PR.
#   scripts/ci/local.sh --full --release  --full plus the release gate (PRs into `main`)
#   scripts/ci/local.sh --scenarios a,b   runs the named scenarios instead of the smoke set
#   SMOKE_CHUNK=25 (default)              smoke runs in chunks of this many scenarios, one vamd
#                                         each; LOCAL_CI_OUT=<dir> again resumes after a restart
#
# Every invocation uses one shared target dir and identical flags, so Cargo
# reuses artifacts across waves and worktrees instead of building a new
# feature variant each time (working-agreement.md, "Disk in cloud sessions").
# Run it from the worktree you are checking; the target dir is shared anyway.
set -euo pipefail

mode=fast
release=0
scenarios=""
while [ $# -gt 0 ]; do
  case "$1" in
    --fast) mode=fast ;;
    --full) mode=full ;;
    --release) release=1 ;;
    --scenarios) shift; scenarios="${1:-}" ;;
    -h|--help) sed -n '2,20p' "$0"; exit 0 ;;
    *) echo "unknown flag: $1" >&2; exit 2 ;;
  esac
  shift
done

repo="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$repo"

# The same settings as .github/workflows/rust.yml. The shared target dir is the
# main checkout's, never a worktree's own (no `target` symlinks in worktrees).
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-/home/user/vam/target}"
export CARGO_INCREMENTAL=0
export CARGO_PROFILE_DEV_DEBUG=0
export CARGO_PROFILE_TEST_DEBUG=0
mkdir -p "$CARGO_TARGET_DIR"
out="${LOCAL_CI_OUT:-$(mktemp -d "${TMPDIR:-/tmp}/local-ci.XXXXXX")}"

free_gb() { df -BG --output=avail "$CARGO_TARGET_DIR" | tail -1 | tr -dc '0-9'; }
# Process-mode tools run on the host under Bun. Same pin as .github/workflows/rust.yml.
BUN_VERSION="${BUN_VERSION:-1.4.4}"
ensure_bun() {
  if [ "$(bun --version 2>/dev/null || true)" != "$BUN_VERSION" ]; then
    echo "installing bun $BUN_VERSION"
    curl -fsSL https://bun.sh/install | bash -s "bun-v$BUN_VERSION" >/dev/null
    export PATH="$HOME/.bun/bin:$PATH"
  fi
  [ "$(bun --version)" = "$BUN_VERSION" ] || { echo "bun $BUN_VERSION is required" >&2; exit 1; }
  # Tool processes run with PATH=/usr/local/bin:/usr/bin:/bin and dropped privileges: bun must be a
  # real, world-executable file there (a symlink into a 0700 /root/.bun is "Permission denied").
  if [ ! -x /usr/local/bin/bun ] || [ -L /usr/local/bin/bun ] || [ "$(/usr/local/bin/bun --version 2>/dev/null || true)" != "$BUN_VERSION" ]; then
    src="$(readlink -f "$(command -v bun)")"
    echo "installing $src as /usr/local/bin/bun"
    if [ "$(id -u)" = 0 ]; then rm -f /usr/local/bin/bun; install -m 755 "$src" /usr/local/bin/bun; else sudo rm -f /usr/local/bin/bun; sudo install -m 755 "$src" /usr/local/bin/bun; fi
  fi
}
step() { printf '\n==> %s\n' "$*"; }
# The smoke set in chunks of $SMOKE_CHUNK scenarios, one run.sh (one vamd) per chunk (a
# long smoke process coincided with cloud-container restarts; nothing leaks per scenario, so
# chunks only bound the damage). Each chunk's names are logged before it runs, and a passed chunk
# is recorded in $out/smoke-chunks.done: rerun with the same LOCAL_CI_OUT to resume after a
# restart. Every chunk runs; the step fails at the end if any chunk failed.
SMOKE_CHUNK="${SMOKE_CHUNK:-25}"
smoke_chunks() {
  local ids n i chunk list failed=() done_file="$out/smoke-chunks.done"
  mapfile -t ids < <(scripts/e2e/smoke-ids.sh)
  n=${#ids[@]}
  touch "$done_file"
  for ((i = 0; i < n; i += SMOKE_CHUNK)); do
    chunk=$((i / SMOKE_CHUNK + 1))
    list="$(IFS=,; echo "${ids[*]:i:SMOKE_CHUNK}")"
    if grep -qxF "$list" "$done_file"; then
      step "smoke chunk $chunk: passed in an earlier run, skipped"
      continue
    fi
    step "smoke chunk $chunk ($((i + 1))-$((i + SMOKE_CHUNK < n ? i + SMOKE_CHUNK : n)) of $n): ${list//,/ }"
    if scripts/e2e/run.sh --scenarios "$list" --out "$out/e2e/chunk-$chunk"; then
      echo "$list" >> "$done_file"
    else
      failed+=("$chunk")
    fi
  done
  if [ ${#failed[@]} -gt 0 ]; then
    echo "smoke: chunk(s) ${failed[*]} failed (outputs under $out/e2e)" >&2
    return 1
  fi
  record smoke "ok ($n scenarios in chunks of $SMOKE_CHUNK)"
}
results=()
record() { results+=("$1: $2"); }

step "disk before: $(free_gb) GB free, target $(du -sh "$CARGO_TARGET_DIR" 2>/dev/null | cut -f1)"
if [ "$(free_gb)" -lt 3 ]; then
  echo "under 3 GB free: stop and report (working-agreement.md cleanup ladder)" >&2
  exit 3
fi

step "rustfmt";               cargo fmt --all --check;                         record rustfmt ok
step "example JSON protos";   cargo test -p vam-spec --test examples_json -q;  record protos ok
step "manifests";             scripts/e2e/check-manifests.sh;                 record manifests ok
step "marketplace fixture";    scripts/marketplace/check-snapshot.sh;           record marketplace-snapshot ok
step "bun tests (m365-mail)"; ensure_bun; (cd e2e/world/fixtures/marketplace-squrell && bun test tests); record bun-tests ok
step "cargo check";           cargo check --workspace -q;                      record check ok

if [ "$mode" = full ]; then
  # The clippy gate. Keep CLIPPY_TOOLCHAIN equal to the `clippy` job's
  # dtolnay/rust-toolchain@<version> in .github/workflows/rust.yml. A Rust bump is
  # one PR that changes both and fixes the new lints. The script installs
  # that toolchain with the clippy component when it is missing.
  CLIPPY_TOOLCHAIN="1.97.0"
  clippy_features="vam-runtime/process-guest,vam-orchestrator/process-guest"
  if ! rustup toolchain list | grep -q "^$CLIPPY_TOOLCHAIN"; then
    step "installing Rust $CLIPPY_TOOLCHAIN with clippy"
    rustup toolchain install "$CLIPPY_TOOLCHAIN" --profile minimal --component clippy
  fi
  step "clippy ($CLIPPY_TOOLCHAIN, default features)"
  cargo "+$CLIPPY_TOOLCHAIN" clippy --workspace --all-targets -q -- -D warnings;  record clippy-default ok
  step "clippy ($CLIPPY_TOOLCHAIN, process-guest)"
  cargo "+$CLIPPY_TOOLCHAIN" clippy --workspace --all-targets --features "$clippy_features" -q -- -D warnings;  record clippy-process-guest ok

  step "cargo test --workspace --no-fail-fast"
  # hsp_uds::uds_socket_acl_deny_fails_fast skips itself as root (0o000 does not stop
  # root); CI runs as a normal user and checks it there.
  cargo test --workspace --no-fail-fast -q;                                     record workspace-tests ok
fi

if [ -n "$scenarios" ]; then
  step "scenarios: $scenarios";  scripts/e2e/run.sh --scenarios "$scenarios" --out "$out/e2e"; record scenarios ok
else
  smoke_chunks
fi

if [ "$release" = 1 ]; then
  step "release gate";          scripts/e2e/check-release-gate.sh;              record release-gate ok
fi

step "disk after: $(free_gb) GB free, target $(du -sh "$CARGO_TARGET_DIR" 2>/dev/null | cut -f1)"
printf '\nlocal CI (%s%s): all steps passed\n' "$mode" "$([ "$release" = 1 ] && echo ', release')"
printf '  %s\n' "${results[@]}"
echo "  artifacts: $out"
