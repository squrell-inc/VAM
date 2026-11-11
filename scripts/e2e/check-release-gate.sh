#!/usr/bin/env bash
# Release gate: process mode must never ship. Runs every check (no early abort):
# manifests, the default/feature vamd markers, the default/feature guest markers,
# the default dependency tree, and that a release needs a green microVM lane (rust.yml).
# Exit 0 = PASS, 1 = any part failed.
set -uo pipefail
cd "$(dirname "$0")/../.."
failed=()
run() { local name="$1"; shift; echo "=== $name ==="; "$@" || failed+=("$name"); }

run manifests scripts/e2e/check-manifests.sh
run vamd-build scripts/e2e/check-vamd-build.sh
run guest-build scripts/e2e/check-guest-build.sh
run notify-build scripts/e2e/check-notify-build.sh
tree_check() {
  if cargo tree -q --workspace -e normal,build,features | grep -E 'process-guest|uds-transport' >/dev/null; then
    echo "FAIL: the default workspace dependency tree enables process-guest or uds-transport"; return 1
  fi
  echo "ok:   the default workspace dependency tree enables neither feature"
}
run default-tree tree_check
# The microVM lane blocks releases (launch audit S4): rust.yml must force the lane on for release
# PRs (into main from feat/*, or labelled `release`) and keep the `release lane (microVM)` job that fails such a PR unless the lane is green.
lane_check() {
  local wf=.github/workflows/rust.yml ok=1
  [ "$(grep -c "RELEASE: \${{ github.base_ref == 'main' && (startsWith(github.head_ref, 'feat/')" "$wf")" -ge 2 ] || { echo "FAIL: the changes and release-lane jobs no longer agree on what a release PR is"; ok=0; }
  grep -q 'if \[ "$RELEASE" = "true" \]; then' "$wf" || { echo "FAIL: the changes job no longer forces the microVM lane on for release PRs"; ok=0; }
  grep -q '^  release-lane:$' "$wf" || { echo "FAIL: the release-lane job is gone"; ok=0; }
  grep -q 'needs: \[changes, firecracker\]' "$wf" || { echo "FAIL: release-lane no longer waits for the firecracker job"; ok=0; }
  grep -q 'if \[ "$LANE" != "success" \]; then' "$wf" || { echo "FAIL: release-lane no longer requires a green lane"; ok=0; }
  [ "$ok" = 1 ] && echo "ok:   a release PR runs the microVM lane and needs it green"
  [ "$ok" = 1 ]
}
run microvm-lane lane_check

if [ "${#failed[@]}" -ne 0 ]; then echo "release-gate: FAILED (${failed[*]})"; exit 1; fi
echo "release-gate: PASS"
