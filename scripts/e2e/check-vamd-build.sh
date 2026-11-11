#!/usr/bin/env bash
# Proves process-guest mode is absent from the default vamd and present (and
# bounded) in the `process-guest` build. Container-runnable, no root needed.
# Builds both flavours of vamd one after the other and inspects each right after
# its build (cargo reuses target/debug/vamd). Leaves the feature build in place.
set -euo pipefail

cd "$(dirname "$0")/../.."
target_dir="${CARGO_TARGET_DIR:-target}"
bin="$target_dir/debug/vamd"
fail=0
say() { printf '%s\n' "$*"; }
bad() { say "FAIL: $*"; fail=1; }
ok() { say "ok:   $*"; }

# Strings only the process-guest build may contain.
# UpstreamRedirect: the test-only gate redirect for the Graph mock (vam-network, feature process-guest).
# VAMD_PROCESS_MARKETPLACE_URLS and protocol.file.allow: the marketplace seam and the `file://` transport
# it needs (vamd's marketplace fetch is https only; the file transport is compiled out of a default vamd).
# /v1/process/marketplace-reset: the test-only route that returns a node's marketplace installs to the boot state (CN-4.5).
markers=(VAMD_PROCESS_GUEST_BIN ProcessVmm "PROCESS GUEST MODE" process-guest oversize.e2e.test VAMD_PROCESS_CONNECTOR_ENDPOINTS UpstreamRedirect VAMD_PROCESS_MARKETPLACE_URLS "protocol.file.allow" "/v1/process/marketplace-reset")
# The default build's rejection must still name the prefix, or the check below is vacuous.
reject_marker="not compiled into this vamd"

# No grep -q here: it would SIGPIPE strings, and pipefail would report a match as a miss.
has_string() { strings -a "$bin" | grep -F -- "$1" >/dev/null; }

say "== default build (a normal vamd) =="
cargo build -q -p vam-orchestrator --bin vamd
for m in "${markers[@]}"; do
  if has_string "$m"; then bad "default vamd contains process-mode marker: $m"; else ok "default vamd has no '$m'"; fi
done
if has_string "$reject_marker" && has_string "VAMD_PROCESS_"; then
  ok "default vamd carries the VAMD_PROCESS_ rejection (the marker check can fail)"
else
  bad "default vamd lacks the VAMD_PROCESS_ rejection"
fi
if cargo tree -q -p vam-orchestrator -e normal,features | grep -E 'process-guest' >/dev/null; then
  bad "default dependency tree enables process-guest"
else
  ok "default dependency tree does not enable process-guest"
fi

say "== process-guest build =="
cargo build -q -p vam-orchestrator --bin vamd --features process-guest
for m in "${markers[@]}"; do
  if has_string "$m"; then ok "feature vamd has '$m' (the check can fail)"; else bad "feature vamd lacks marker: $m"; fi
done

say "== sources =="
# Nothing in the workspace turns the feature on for a dependency, or by default.
if scripts/e2e/check-manifests.sh; then ok "manifests are clean"; else bad "manifest check failed"; fi
if grep -vE '^[[:space:]]*//' crates/vam-orchestrator/src/manager.rs | grep -n 'new_adopted' >/dev/null; then
  bad "manager.rs still builds FirecrackerVmm::new_adopted (teardown must go through the VmmFactory)"
else
  ok "manager.rs has no FirecrackerVmm::new_adopted"
fi

if [ "$fail" -ne 0 ]; then say "check-vamd-build: FAILED"; exit 1; fi
say "check-vamd-build: PASS"
