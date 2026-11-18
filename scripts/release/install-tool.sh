#!/usr/bin/env bash
# Installs pinned release tools from scripts/release/tools.pins: download, check the sha256 of the
# archive, unpack the one binary, install it. No compile, no third-party install action.
#
#   scripts/release/install-tool.sh cargo-deny [actionlint ...]
#
# Env: TOOLS_BIN   install directory (default $HOME/.local/bin)
#      TOOLS_PINS  pins file (default tools.pins beside this script; tests point it at fixtures)
# On GitHub Actions the directory is also appended to $GITHUB_PATH.
set -euo pipefail

die() { echo "install-tool: $*" >&2; exit 1; }
[ $# -gt 0 ] || die "usage: scripts/release/install-tool.sh <tool>..."

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
pins="${TOOLS_PINS:-$here/tools.pins}"
bin="${TOOLS_BIN:-$HOME/.local/bin}"
[ -f "$pins" ] || die "no pins file $pins"
mkdir -p "$bin"

work="$(mktemp -d "${TMPDIR:-/tmp}/install-tool.XXXXXX")"
trap 'rm -rf "$work"' EXIT

for tool in "$@"; do
  line="$(awk -v t="$tool" '$1 == t' "$pins")"
  [ -n "$line" ] || die "'$tool' is not in $pins"
  [ "$(printf '%s\n' "$line" | wc -l)" = 1 ] || die "'$tool' is pinned more than once in $pins"
  read -r name version url sha member <<< "$line"
  [[ "$sha" =~ ^[0-9a-f]{64}$ ]] || die "$name: the pinned hash is not 64 hex digits"
  archive="$work/$name.archive"
  echo "install-tool: $name $version from $url"
  curl -fsSL --retry 3 --retry-delay 2 -o "$archive" "$url" || die "$name: download failed"
  got="$(sha256sum "$archive" | awk '{print $1}')"
  [ "$got" = "$sha" ] || die "$name: sha256 mismatch (pinned $sha, downloaded $got); not installing"
  mkdir -p "$work/$name"
  tar -xzf "$archive" -C "$work/$name" "$member" || die "$name: '$member' is not in the archive"
  install -m 755 "$work/$name/$member" "$bin/$(basename "$member")"
  echo "install-tool: installed $bin/$(basename "$member")"
done
if [ -n "${GITHUB_PATH:-}" ]; then echo "$bin" >> "$GITHUB_PATH"; fi
