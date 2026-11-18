#!/usr/bin/env bash
# Installs rustup (the pinned rustup-init, checked by sha256) and the release toolchain, for a build
# container that has no Rust (the AlmaLinux 9 jobs of release-build.yml). Never pipes a script to a shell.
#
#   scripts/release/install-rust.sh [toolchain]      default 1.97.0 (CLIPPY_TOOLCHAIN in scripts/ci/local.sh)
#
# Env: RUSTUP_PINS      pins file (default rustup.pins beside this script; tests point it at fixtures)
#      RUSTUP_PLATFORM  override the detected platform (tests)
# Installs with --profile minimal and --no-modify-path; on GitHub Actions $CARGO_HOME/bin goes to $GITHUB_PATH.
set -euo pipefail

die() { echo "install-rust: $*" >&2; exit 1; }
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
toolchain="${1:-1.97.0}"
pins="${RUSTUP_PINS:-$here/rustup.pins}"
[ -f "$pins" ] || die "no pins file $pins"
if [ -n "${RUSTUP_PLATFORM:-}" ]; then
  platform="$RUSTUP_PLATFORM"
else
  case "$(uname -m)" in x86_64|amd64) arch=x86_64 ;; aarch64|arm64) arch=aarch64 ;; *) arch="$(uname -m)" ;; esac
  platform="linux-$arch"
fi
line="$(awk -v p="$platform" '$1 == p' "$pins")"
[ -n "$line" ] || die "no rustup-init pinned for $platform in $pins"
[ "$(printf '%s\n' "$line" | wc -l | tr -d ' ')" -eq 1 ] || die "$platform is pinned more than once in $pins"
read -r _ version url sha <<< "$line"
[[ "$sha" =~ ^[0-9a-f]{64}$ ]] || die "the pinned hash is not 64 hex digits"

work="$(mktemp -d "${TMPDIR:-/tmp}/install-rust.XXXXXX")"
trap 'rm -rf "$work"' EXIT
echo "install-rust: rustup-init $version from $url"
curl -fsSL --retry 3 --retry-delay 2 -o "$work/rustup-init" "$url" || die "download failed"
got="$(sha256sum "$work/rustup-init" | awk '{print $1}')"
[ "$got" = "$sha" ] || die "sha256 mismatch (pinned $sha, downloaded $got); not running it"
chmod +x "$work/rustup-init"
# from a neutral directory: the repository's rust-toolchain.toml (stable) must not override the choice
cd "$work"
"$work/rustup-init" -y --profile minimal --default-toolchain "$toolchain" --no-modify-path
cargo_bin="${CARGO_HOME:-$HOME/.cargo}/bin"
export PATH="$cargo_bin:$PATH"
rustc --version
if [ -n "${GITHUB_PATH:-}" ]; then echo "$cargo_bin" >> "$GITHUB_PATH"; fi
