#!/usr/bin/env bash
# Installs zig (for cargo-zigbuild) from the sha256-pinned PyPI wheel in zig.requirements.txt into a
# private virtualenv and puts a `zig` link in TOOLS_BIN. Linux only (the macOS builds do not use zig).
#
#   scripts/release/install-zig.sh
#
# Env: TOOLS_BIN  install directory (default $HOME/.local/bin); the venv is TOOLS_BIN/../zig-venv
# On GitHub Actions the directory is also appended to $GITHUB_PATH.
set -euo pipefail

die() { echo "install-zig: $*" >&2; exit 1; }
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
bin="${TOOLS_BIN:-$HOME/.local/bin}"
req="$here/zig.requirements.txt"
[ -f "$req" ] || die "no $req"
grep -Eq -- '--hash=sha256:[0-9a-f]{64}' "$req" || die "$req has no sha256 hash"
mkdir -p "$bin"
venv="$(cd "$bin" && pwd)/../zig-venv"
python3 -m venv "$venv" || die "python3 -m venv failed"
"$venv/bin/python" -m pip install --quiet --disable-pip-version-check --require-hashes --only-binary :all: -r "$req" \
  || die "pip install of the pinned ziglang wheel failed"
zig="$(find "$venv" -type f -name zig -path '*ziglang*' | head -n 1)"
[ -n "$zig" ] && [ -x "$zig" ] || die "no zig binary in the wheel"
ln -sf "$(cd "$(dirname "$zig")" && pwd)/zig" "$bin/zig"
"$bin/zig" version
if [ -n "${GITHUB_PATH:-}" ]; then echo "$bin" >> "$GITHUB_PATH"; fi
