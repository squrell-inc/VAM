#!/usr/bin/env bash
# Runs every scripts/release/test/*.test.sh (by glob, so a later wave adds a test file without
# editing this one). Offline, no sudo. Exit 0 only when every file passes.
#   scripts/release/test/run.sh [name-substring]
# Env: REQUIRE_ACTIONLINT=1 makes the workflow test fail when actionlint is missing (CI sets it).
set -uo pipefail
cd "$(dirname "$0")"
filter="${1:-}"
failed=()
n=0
for t in *.test.sh; do
  [ -e "$t" ] || continue
  case "$t" in *"$filter"*) ;; *) continue ;; esac
  n=$((n + 1))
  echo "=== $t ==="
  start=$(date +%s)
  if bash "$t"; then :; else failed+=("$t"); fi
  echo "    ($(( $(date +%s) - start )) s)"
done
if [ "$n" = 0 ]; then echo "release tests: no test files matched" >&2; exit 1; fi
if [ ${#failed[@]} -ne 0 ]; then echo "release tests: FAILED (${failed[*]})"; exit 1; fi
echo "release tests: PASS ($n files)"
