#!/usr/bin/env bash
# Checks resolve-target-dir.sh: an absolute path that does not exist yet stays absolute.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
fail() { echo "FAIL: $*" >&2; exit 1; }
got="$(CARGO_TARGET_DIR=/nonexistent-vam-test/tgt "$here/resolve-target-dir.sh")"
[ "$got" = "/nonexistent-vam-test/tgt" ] || fail "absolute non-existent: got $got"
cd /tmp
got="$(CARGO_TARGET_DIR=rel/tgt "$here/resolve-target-dir.sh")"
[ "$got" = "/tmp/rel/tgt" ] || fail "relative: got $got"
got="$(env -u CARGO_TARGET_DIR "$here/resolve-target-dir.sh")"
[ "$got" = "/tmp/target" ] || fail "default: got $got"
echo "ok: resolve-target-dir"
