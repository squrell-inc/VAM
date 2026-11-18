#!/usr/bin/env bash
# Installs pinned release tools from scripts/release/tools.pins: download, check the sha256 of the
# archive, unpack the one binary, install it. No compile, no third-party install action.
#
#   scripts/release/install-tool.sh cargo-deny [actionlint ...]
#
# A tools.pins line may end with a platform (linux-x86_64 when absent; linux-aarch64, darwin-arm64,
# darwin-x86_64): the tool is looked up for the machine's platform. A tool with no line for this
# platform but a `name version` line in cargo.pins is built with `cargo install --locked --version`
# (crates.io checks the crate's own checksum; nothing else is pinned for it).
#
# Env: TOOLS_BIN         install directory (default $HOME/.local/bin)
#      TOOLS_PINS        pins file (default tools.pins beside this script; tests point it at fixtures)
#      TOOLS_CARGO_PINS  the cargo.pins file (default beside this script)
#      TOOLS_PLATFORM    override the detected platform (tests)
# On GitHub Actions the directory is also appended to $GITHUB_PATH.
set -euo pipefail

die() { echo "install-tool: $*" >&2; exit 1; }
[ $# -gt 0 ] || die "usage: scripts/release/install-tool.sh <tool>..."

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
pins="${TOOLS_PINS:-$here/tools.pins}"
cargo_pins="${TOOLS_CARGO_PINS:-$here/cargo.pins}"
if [ -n "${TOOLS_PLATFORM:-}" ]; then
  platform="$TOOLS_PLATFORM"
else
  case "$(uname -s)" in Linux) os=linux ;; Darwin) os=darwin ;; *) os="$(uname -s | tr '[:upper:]' '[:lower:]')" ;; esac
  case "$(uname -m)" in x86_64|amd64) arch=x86_64 ;; aarch64|arm64) arch="$([ "$os" = darwin ] && echo arm64 || echo aarch64)" ;; *) arch="$(uname -m)" ;; esac
  platform="$os-$arch"
fi
bin="${TOOLS_BIN:-$HOME/.local/bin}"
[ -f "$pins" ] || die "no pins file $pins"
mkdir -p "$bin"

work="$(mktemp -d "${TMPDIR:-/tmp}/install-tool.XXXXXX")"
trap 'rm -rf "$work"' EXIT
# Line counts are compared with -eq: BSD wc (macOS) pads its count with spaces.

for tool in "$@"; do
  line="$(awk -v t="$tool" -v p="$platform" '$1 == t && (($6 == "" ? "linux-x86_64" : $6) == p)' "$pins")"
  if [ -z "$line" ]; then
    cline=""; [ -f "$cargo_pins" ] && cline="$(awk -v t="$tool" '$1 == t' "$cargo_pins")"
    [ -n "$cline" ] || die "'$tool' is not in $pins for $platform (and not in $cargo_pins)"
    [ "$(printf '%s\n' "$cline" | wc -l)" -eq 1 ] || die "'$tool' is pinned more than once in $cargo_pins"
    read -r name version <<< "$cline"
    [[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "$name: '$version' in $cargo_pins is not a version"
    echo "install-tool: $name $version by cargo install --locked ($platform has no pinned binary)"
    cargo install --locked --version "$version" --root "$work/cargo-$name" "$name" || die "$name: cargo install failed"
    install -m 755 "$work/cargo-$name/bin/$name" "$bin/$name"
    echo "install-tool: installed $bin/$name"
    continue
  fi
  [ "$(printf '%s\n' "$line" | wc -l)" -eq 1 ] || die "'$tool' is pinned more than once in $pins for $platform"
  read -r name version url sha member _platform <<< "$line"
  [[ "$sha" =~ ^[0-9a-f]{64}$ ]] || die "$name: the pinned hash is not 64 hex digits"
  archive="$work/$name.archive"
  echo "install-tool: $name $version from $url"
  curl -fsSL --retry 3 --retry-delay 2 -o "$archive" "$url" || die "$name: download failed"
  got="$(sha256sum "$archive" | awk '{print $1}')"
  [ "$got" = "$sha" ] || die "$name: sha256 mismatch (pinned $sha, downloaded $got); not installing"
  mkdir -p "$work/$name"
  tar -xf "$archive" -C "$work/$name" "$member" || die "$name: '$member' is not in the archive"
  install -m 755 "$work/$name/$member" "$bin/$(basename "$member")"
  echo "install-tool: installed $bin/$(basename "$member")"
done
if [ -n "${GITHUB_PATH:-}" ]; then echo "$bin" >> "$GITHUB_PATH"; fi
