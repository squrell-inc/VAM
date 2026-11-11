#!/usr/bin/env bash
# Fails when a Cargo.toml turns on a process-mode feature where it must not.
#   check-manifests.sh [root]     (default: the repo root)
# Allowed: `process-guest` / `uds-transport` as a feature definition under
# [features] (a feature may list another crate's feature), and `uds-transport`
# in a dev-dependencies section. Anything else, or a `default = [...]` that
# lists either feature, fails with file:line.
set -euo pipefail
root="${1:-$(cd "$(dirname "$0")/../.." && pwd)}"
cd "$root"
files="$(find . -name Cargo.toml -not -path './target/*' -not -path './.git/*' -not -path '*/node_modules/*' | sort)"
fail=0
for f in $files; do
  out="$(awk '
    /^[[:space:]]*\[/ { sect=$0; gsub(/[[:space:]]/,"",sect); next }
    { line=$0; sub(/#.*/,"",line) }
    line !~ /process-guest|uds-transport/ { next }
    {
      isdev = (sect ~ /dev-dependencies/)
      if (sect == "[features]") {
        if (line ~ /^[[:space:]]*default[[:space:]]*=/) { print FNR": default feature list names a process-mode feature: "$0; next }
        if (line ~ /^[[:space:]]*(process-guest|uds-transport)[[:space:]]*=/) next
        print FNR": feature list names a process-mode feature outside its definition: "$0; next
      }
      if (line ~ /process-guest/) { print FNR": process-guest enabled outside [features]: "$0; next }
      if (!isdev) print FNR": uds-transport enabled outside [features] and dev-dependencies: "$0
    }' "$f")"
  if [ -n "$out" ]; then
    while IFS= read -r l; do echo "FAIL: $f:$l"; done <<< "$out"
    fail=1
  fi
done
if [ "$fail" -ne 0 ]; then echo "check-manifests: FAILED"; exit 1; fi
echo "ok:   no Cargo.toml enables process-guest or uds-transport outside features and dev-dependencies"
echo "check-manifests: PASS"
