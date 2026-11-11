#!/usr/bin/env bash
# Prints the absolute cargo target dir for $CARGO_TARGET_DIR (default `target`), existing or not.
# An absolute path stays absolute; a relative one is taken from the current directory.
# Test: scripts/e2e/test-resolve-target-dir.sh
set -euo pipefail
t="${CARGO_TARGET_DIR:-target}"
case "$t" in
  /*) echo "$t" ;;
  *) echo "$PWD/$t" ;;
esac
