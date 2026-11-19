#!/usr/bin/env bash
# Proves the `vam upgrade` test seam (VAM_UPGRADE_API_URL, feature `upgrade-seam`) is absent from a
# normal vam-cli build and present only with the feature: the default `vam` has no way to be
# pointed at another release server. Builds the default flavour, inspects it, builds the feature
# flavour, inspects it (so the check can fail), then rebuilds the default so target/debug/vam is
# left as an ordinary build.
set -euo pipefail

cd "$(dirname "$0")/../.."
target_dir="${CARGO_TARGET_DIR:-target}"
bin="$target_dir/debug/vam"
fail=0
say() { printf '%s\n' "$*"; }
bad() { say "FAIL: $*"; fail=1; }
ok() { say "ok:   $*"; }

marker=VAM_UPGRADE_API_URL
# No grep -q here: it would SIGPIPE strings, and pipefail would report a match as a miss.
has_string() { strings -a "$bin" | grep -F -- "$1" >/dev/null; }

say "== default build =="
cargo build -q -p vam-cli
if has_string "$marker"; then bad "default vam contains the upgrade seam marker: $marker"; else ok "default vam has no '$marker'"; fi
if cargo tree -q -p vam-cli -e normal,features | grep -F 'upgrade-seam' >/dev/null; then
  bad "default vam-cli dependency tree enables upgrade-seam"
else
  ok "default vam-cli dependency tree does not enable upgrade-seam"
fi

say "== upgrade-seam build =="
cargo build -q -p vam-cli --features upgrade-seam
if has_string "$marker"; then ok "feature vam has '$marker' (the check can fail)"; else bad "feature vam lacks marker: $marker"; fi
cargo build -q -p vam-cli

if [ "$fail" -ne 0 ]; then say "check-upgrade-build: FAILED"; exit 1; fi
say "check-upgrade-build: PASS"
