#!/usr/bin/env bash
# The one way to build the guest init (vam-runtime, static musl). build-rootfs.sh, the
# firecracker lane and the release build all call this; nothing else invokes cargo for it.
#
#   scripts/release/build-guest.sh [--target x86_64|aarch64] [--out PATH]
#
#   --target   architecture (default x86_64); the triple is <arch>-unknown-linux-musl
#   --out PATH also copy the binary to PATH
#
# CARGO_TARGET_DIR is honoured. The last stdout line is the absolute path of the binary;
# progress goes to stderr, so BIN=$(scripts/release/build-guest.sh) works.
# The build is the default one: no features, release profile (tracker open question 9).
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
ARCH=x86_64
OUT=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    --target) [ "$#" -ge 2 ] || { echo "--target needs a value" >&2; exit 2; }; ARCH="$2"; shift 2 ;;
    --out) [ "$#" -ge 2 ] || { echo "--out needs a value" >&2; exit 2; }; OUT="$2"; shift 2 ;;
    *) echo "usage: $0 [--target x86_64|aarch64] [--out PATH]" >&2; exit 2 ;;
  esac
done
case "$ARCH" in
  x86_64|aarch64) ;;
  *) echo "--target must be x86_64 or aarch64, got '$ARCH'" >&2; exit 2 ;;
esac
TRIPLE="$ARCH-unknown-linux-musl"
CC_VAR="CC_${TRIPLE//-/_}"

if [ -z "${!CC_VAR:-}" ] && ! command -v "$ARCH-linux-musl-gcc" >/dev/null 2>&1; then
  if command -v musl-gcc >/dev/null 2>&1; then
    export "$CC_VAR=musl-gcc"
  elif command -v gcc >/dev/null 2>&1; then
    export "$CC_VAR=gcc"
  fi
fi

echo "=== Building static VAM runtime binary ($TRIPLE) ===" >&2
RUSTFLAGS="-C target-feature=+crt-static" cargo build \
  --manifest-path "$WORKSPACE_ROOT/Cargo.toml" \
  --target "$TRIPLE" \
  -p vam-runtime --bin vam-runtime \
  --release >&2

BIN="${CARGO_TARGET_DIR:-$WORKSPACE_ROOT/target}/$TRIPLE/release/vam-runtime"
[ -f "$BIN" ] || { echo "build-guest.sh: $BIN was not produced" >&2; exit 1; }
if [ -n "$OUT" ]; then
  mkdir -p "$(dirname "$OUT")"
  cp "$BIN" "$OUT"
  BIN="$OUT"
fi
BIN="$(cd "$(dirname "$BIN")" && pwd)/$(basename "$BIN")"
printf '%s\n' "$BIN"
