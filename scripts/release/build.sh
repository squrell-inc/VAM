#!/usr/bin/env bash
# Builds the release binaries for one target (locked decisions 8, 11; design §6.1).
#
#   scripts/release/build.sh <target> --set node|cli|guest --out DIR
#
#   <target>   x86_64-unknown-linux-gnu | aarch64-unknown-linux-gnu | aarch64-apple-darwin |
#              x86_64-apple-darwin | x86_64-unknown-linux-musl (the guest init, --set guest only)
#   --set cli    vam
#   --set node   vamd, vam, vam-web-broker, vam-schedule-broker, vam-notify-broker
#   --set guest  vam-runtime, through scripts/release/build-guest.sh (the one way to build it)
#   --out DIR    the binaries are copied here by name
#
# Env: VAM_BUILD_CHANNEL  release | dev (default dev). `release` makes the version Cargo.toml's
#                         exactly, so the build refuses to continue when VERSION is set and the
#                         binary says anything else.
#      VERSION            the version being built (checked against `vam --version`)
#      COMMIT             full SHA to build (default HEAD; HEAD must be it)
#      RUST_TOOLCHAIN     default 1.97.0 (CLIPPY_TOOLCHAIN in scripts/ci/local.sh)
#      SKIP_AUDIT=1       skip `cargo audit bin` (offline tests)
# CARGO_TARGET_DIR is honoured. Linux gnu targets must be built on a glibc 2.34 host (locked
# decision 8: the AlmaLinux 9 container of release-build.yml, native gcc, no zig; cargo-zigbuild
# cannot link `cargo auditable` binaries). The highest GLIBC_ symbol of every binary is checked
# and printed (to $GITHUB_STEP_SUMMARY too).
#
# Reproducibility (design §6.1): --locked, CARGO_INCREMENTAL=0, SOURCE_DATE_EPOCH from the commit,
# the checkout, $CARGO_HOME and $RUSTUP_HOME remapped, host binaries stripped. The promise is same
# inputs, same versions, not bit-identical output. The build ends by refusing a binary that still
# holds one of those paths.
set -euo pipefail

die() { echo "build.sh: $*" >&2; exit 1; }
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
GLIBC_FLOOR=2.34

target="${1:-}"; [ $# -gt 0 ] && shift
set_name=""; out=""
while [ $# -gt 0 ]; do
  case "$1" in
    --set) [ $# -ge 2 ] || die "--set needs a value"; set_name="$2"; shift 2 ;;
    --out) [ $# -ge 2 ] || die "--out needs a value"; out="$2"; shift 2 ;;
    *) die "usage: build.sh <target> --set node|cli|guest --out DIR" ;;
  esac
done
case "$target" in
  x86_64-unknown-linux-gnu|aarch64-unknown-linux-gnu|aarch64-apple-darwin|x86_64-apple-darwin|x86_64-unknown-linux-musl) ;;
  *) die "unknown target '$target'" ;;
esac
case "$set_name" in
  node|cli) [ "$target" != x86_64-unknown-linux-musl ] || die "--set $set_name is not built for musl" ;;
  guest) [ "$target" = x86_64-unknown-linux-musl ] || die "--set guest builds for x86_64-unknown-linux-musl only" ;;
  *) die "--set must be node, cli or guest" ;;
esac
[ -n "$out" ] || die "--out DIR is required"
mkdir -p "$out"
out="$(cd "$out" && pwd)"

cd "$repo"
channel="${VAM_BUILD_CHANNEL:-dev}"
case "$channel" in release|dev) ;; *) die "VAM_BUILD_CHANNEL must be release or dev, got '$channel'" ;; esac
commit="${COMMIT:-$(git rev-parse HEAD)}"
[ "$(git rev-parse HEAD)" = "$commit" ] || die "HEAD is $(git rev-parse HEAD), not the commit to build ($commit)"
if [ -n "$(git status --porcelain --untracked-files=no)" ]; then die "the tree has uncommitted changes; a release build is of a commit"; fi
SOURCE_DATE_EPOCH="$(git log -1 --format=%ct "$commit")"
export SOURCE_DATE_EPOCH
export VAM_BUILD_CHANNEL="$channel"
VAM_GIT_SHA="$(git rev-parse --short=7 "$commit")"
VAM_COMMIT_DATE="$(git show -s --format=%cs "$commit")"
export VAM_GIT_SHA VAM_COMMIT_DATE
export CARGO_INCREMENTAL=0
export CARGO_PROFILE_RELEASE_STRIP=true
toolchain="${RUST_TOOLCHAIN:-1.97.0}"
cargo_home="${CARGO_HOME:-$HOME/.cargo}"
rustup_home="${RUSTUP_HOME:-$HOME/.rustup}"
remap_rust="--remap-path-prefix=$repo=/vam-src --remap-path-prefix=$cargo_home=/cargo --remap-path-prefix=$rustup_home=/rustup"
remap_c="-ffile-prefix-map=$repo=/vam-src -ffile-prefix-map=$cargo_home=/cargo"
echo "build.sh: $target set=$set_name channel=$channel commit=$VAM_GIT_SHA date=$VAM_COMMIT_DATE epoch=$SOURCE_DATE_EPOCH" >&2

if [ "$set_name" = guest ]; then
  # The guest init is built exactly as build-rootfs.sh builds it (decision 11); no remap flags here.
  # RUSTUP_TOOLCHAIN beats rust-toolchain.toml, so the guest uses the release toolchain too.
  rustup toolchain install "$toolchain" --profile minimal >&2
  rustup target add --toolchain "$toolchain" "$target" >&2
  RUSTUP_TOOLCHAIN="$toolchain" "$here/build-guest.sh" --target x86_64 --out "$out/vam-runtime" >/dev/null
  echo "build.sh: $out/vam-runtime" >&2
  exit 0
fi

pkgs=(-p vam-cli --bin vam)
bins=(vam)
if [ "$set_name" = node ]; then
  pkgs+=(-p vam-orchestrator --bin vamd -p vam-web --bin vam-web-broker -p vam-schedule --bin vam-schedule-broker -p vam-notify --bin vam-notify-broker)
  bins=(vamd vam vam-web-broker vam-schedule-broker vam-notify-broker)
fi

export RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }$remap_rust"
export CFLAGS="${CFLAGS:+$CFLAGS }$remap_c"
export CXXFLAGS="${CXXFLAGS:+$CXXFLAGS }$remap_c"
target_dir="${CARGO_TARGET_DIR:-$repo/target}"
rustup toolchain install "$toolchain" --profile minimal >&2
rustup target add --toolchain "$toolchain" "$target" >&2
case "$target" in
  *-linux-gnu)
    cargo "+$toolchain" auditable build --release --locked --target "$target" "${pkgs[@]}" >&2 ;;
  *-apple-darwin)
    cargo "+$toolchain" auditable build --release --locked --target "$target" "${pkgs[@]}" >&2 ;;
esac
rel="$target_dir/$target/release"
for b in "${bins[@]}"; do
  [ -f "$rel/$b" ] || die "$rel/$b was not produced"
  install -m 755 "$rel/$b" "$out/$b"
done

# 1. no build path left in a binary
for b in "${bins[@]}"; do
  for p in "$repo" "$cargo_home" "$rustup_home"; do
    if grep -aqF -- "$p/" "$out/$b"; then die "$b still contains the build path $p/ (path remapping did not cover it)"; fi
  done
done

# 2. the glibc floor on every gnu binary
summary() { if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then printf '%s\n' "$1" >> "$GITHUB_STEP_SUMMARY"; fi; }
case "$target" in
  *-linux-gnu)
    command -v objdump >/dev/null || die "objdump (binutils) is required for the glibc check"
    summary "### $target highest GLIBC_ symbol (floor $GLIBC_FLOOR)"
    for b in "${bins[@]}"; do
      top="$(objdump -T "$out/$b" | grep -o 'GLIBC_[0-9][0-9.]*' | sed 's/^GLIBC_//' | sort -V | tail -n 1)"
      [ -n "$top" ] || die "$b: no GLIBC_ symbol found (is it a gnu binary?)"
      echo "build.sh: $b highest GLIBC_$top" >&2
      summary "- \`$b\`: GLIBC_$top"
      if [ "$(printf '%s\n%s\n' "$top" "$GLIBC_FLOOR" | sort -V | tail -n 1)" != "$GLIBC_FLOOR" ]; then
        die "$b needs GLIBC_$top, above the floor $GLIBC_FLOOR (Linux gnu builds belong in the AlmaLinux 9 container)"
      fi
    done ;;
esac

# 3. the dependency list embedded by cargo auditable, against the advisories deny.toml accepts
if [ "${SKIP_AUDIT:-0}" != 1 ]; then
  ignores=()
  while IFS= read -r id; do ignores+=(--ignore "$id"); done < <(grep -oE 'RUSTSEC-[0-9]+-[0-9]+' deny.toml | sort -u)
  for b in "${bins[@]}"; do
    "cargo" "+$toolchain" audit bin "${ignores[@]}" "$out/$b" >&2 || die "cargo audit bin failed for $b"
  done
fi

# 4. the build info the binary reports
line="$("$out/vam" --version | head -n 1)"
echo "build.sh: $line" >&2
summary "- \`$line\`"
if [ "$channel" = release ] && [ -n "${VERSION:-}" ]; then
  case "$line" in "vam $VERSION ("*) ;; *) die "vam --version says '$line', not 'vam $VERSION (...)'" ;; esac
elif [ "$channel" = dev ]; then
  case "$line" in *"+dev"*) ;; *) die "a dev build must report +dev, got '$line'" ;; esac
fi
case "$line" in *"($VAM_GIT_SHA"*) ;; *) die "vam --version does not name the commit $VAM_GIT_SHA: '$line'" ;; esac
echo "build.sh: ok" >&2
