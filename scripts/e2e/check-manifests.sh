#!/usr/bin/env bash
# Fails when a Cargo.toml turns on a process-mode feature where it must not.
#   check-manifests.sh [root]     (default: the repo root)
# Allowed: `process-guest` / `uds-transport` as a feature definition under
# [features] (a feature may list another crate's feature), and `uds-transport`
# in a dev-dependencies section. Anything else, or a `default = [...]` that
# lists either feature, fails with file:line.
# `upgrade-seam` (the `vam upgrade` test seam) is allowed only as a feature definition under
# [features] in crates/vam-cli/Cargo.toml, and never in `default`.
set -euo pipefail
root="${1:-$(cd "$(dirname "$0")/../.." && pwd)}"
cd "$root"
files="$(find . -name Cargo.toml -not -path './target/*' -not -path './.git/*' -not -path '*/node_modules/*' | sort)"
fail=0
for f in $files; do
  out="$(awk -v file="$f" '
    /^[[:space:]]*\[/ { sect=$0; gsub(/[[:space:]]/,"",sect); next }
    { line=$0; sub(/#.*/,"",line) }
    line !~ /process-guest|uds-transport|upgrade-seam/ { next }
    line ~ /upgrade-seam/ && line !~ /process-guest|uds-transport/ {
      if (sect != "[features]") { print FNR": upgrade-seam enabled outside [features]: "$0; next }
      if (line ~ /^[[:space:]]*default[[:space:]]*=/) { print FNR": default feature list names upgrade-seam: "$0; next }
      if (line ~ /^[[:space:]]*upgrade-seam[[:space:]]*=/ && file == "./crates/vam-cli/Cargo.toml") next
      print FNR": upgrade-seam named outside its definition in crates/vam-cli/Cargo.toml: "$0
      next
    }
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
echo "ok:   upgrade-seam is defined only in crates/vam-cli/Cargo.toml [features] and enabled nowhere"
echo "check-manifests: PASS"
