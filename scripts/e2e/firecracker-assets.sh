#!/usr/bin/env bash
# Installs the assets the firecracker CI lane needs (x86_64 Linux):
# pinned Firecracker + jailer, a pinned guest kernel, and a rootfs built from this
# tree's vam-runtime (scripts/build-rootfs.sh) on the base VAM_BASE names.
#
#   scripts/e2e/firecracker-assets.sh [--check] [--env-file FILE]
#
#   --check          print /dev/kvm, tool versions, pinned hashes and disk; build nothing
#   --env-file FILE  append the four KEY=path lines to FILE (for $GITHUB_ENV)
#
# VAM_BASE=standard (default) builds the standard base (Debian slim, Python, git, Bun: what
# users get). VAM_BASE=minimal builds the deprecated busybox image without Bun, the "before"
# of the cost comparison. The base manifest is written beside the rootfs and exported as
# VAM_BASE_MANIFEST.
#
# VAM_ASSETS_DIR (default $RUNNER_TEMP/vam-assets, else target/vam-assets) holds the
# result and is the directory the lane caches. Every download is sha256-verified; a
# file that is already present and matches is not downloaded again.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/../.."

# The pins (versions, URLs, sha256) live in scripts/assets.pins, which "vam node init" embeds.
# shellcheck source=../assets.pins
. scripts/assets.pins
FC_TGZ="firecracker-${FC_VERSION}-x86_64.tgz"
FC_URL="https://github.com/firecracker-microvm/firecracker/releases/download/${FC_VERSION}/${FC_TGZ}"

VAM_BASE="${VAM_BASE:-standard}"
case "$VAM_BASE" in
  standard|minimal) ;;
  *) echo "VAM_BASE must be standard or minimal, got '$VAM_BASE'" >&2; exit 2 ;;
esac

check=0 env_file=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    --check) check=1; shift ;;
    --env-file) env_file="$2"; shift 2 ;;
    *) echo "usage: $0 [--check] [--env-file FILE]" >&2; exit 2 ;;
  esac
done

assets="${VAM_ASSETS_DIR:-${RUNNER_TEMP:-$PWD/target}/vam-assets}"
mkdir -p "$assets/bin" "$assets/dl"
FC_BIN="$assets/bin/firecracker"
JAILER_BIN="$assets/bin/jailer"
KERNEL="$assets/$KERNEL_NAME"
ROOTFS="$assets/rootfs.ext4"
BASE_MANIFEST="${ROOTFS%.ext4}.base.json"
STAMP="$assets/rootfs.stamp"

sha_ok() { [ -f "$1" ] && [ "$(sha256sum "$1" | cut -d' ' -f1)" = "$2" ]; }

if [ "$check" -eq 1 ]; then
  echo "== /dev/kvm =="; ls -l /dev/kvm || echo "MISSING /dev/kvm"
  echo "== uname =="; uname -a
  echo "== pinned =="
  echo "firecracker ${FC_VERSION} sha256 ${FC_SHA256}"
  echo "kernel ${KERNEL_NAME} sha256 ${KERNEL_SHA256}"
  echo "base ${VAM_BASE}"
  grep -E '^(DEBIAN_TAG|DEBIAN_DIGEST|SNAPSHOT_TIMESTAMP|BUN_VERSION|BUN_SHA256)=' scripts/base/pins || true
  echo "== tools =="
  for t in docker mke2fs debugfs e2fsck unzip musl-gcc; do
    printf '%s: ' "$t"; command -v "$t" || echo "not found"
  done
  df -h / || true
fi

if ! sha_ok "$FC_BIN" "$(cat "$assets/fc.installed" 2>/dev/null || echo none)"; then
  if ! sha_ok "$assets/dl/$FC_TGZ" "$FC_SHA256"; then
    echo "downloading $FC_TGZ"
    curl -fsSL -o "$assets/dl/$FC_TGZ" "$FC_URL"
    sha_ok "$assets/dl/$FC_TGZ" "$FC_SHA256" || { echo "sha256 mismatch for $FC_TGZ" >&2; exit 1; }
  fi
  tar -xzf "$assets/dl/$FC_TGZ" -C "$assets/dl"
  rel="$assets/dl/release-${FC_VERSION}-x86_64"
  install -m 755 "$rel/firecracker-${FC_VERSION}-x86_64" "$FC_BIN"
  install -m 755 "$rel/jailer-${FC_VERSION}-x86_64" "$JAILER_BIN"
  sha256sum "$FC_BIN" | cut -d' ' -f1 > "$assets/fc.installed"
  rm -rf "$rel"
fi
# The jailer preflight refuses binaries not owned by root or the user running vamd (the lane
# runs vamd as root), so hand them to root; best effort where sudo is not available.
if [ "$(stat -c %u "$FC_BIN")" != 0 ] && command -v sudo >/dev/null 2>&1; then
  sudo -n chown root:root "$FC_BIN" "$JAILER_BIN" || echo "WARN: could not chown $FC_BIN, $JAILER_BIN to root" >&2
fi
"$FC_BIN" --version | head -1

if ! sha_ok "$KERNEL" "$KERNEL_SHA256"; then
  echo "downloading $KERNEL_NAME"
  curl -fsSL -o "$KERNEL.part" "$KERNEL_URL"
  sha_ok "$KERNEL.part" "$KERNEL_SHA256" || { echo "sha256 mismatch for $KERNEL_NAME" >&2; rm -f "$KERNEL.part"; exit 1; }
  mv "$KERNEL.part" "$KERNEL"
fi

if [ "$check" -eq 1 ]; then
  echo "== assets (check only; rootfs not built) =="
  ls -l "$assets" "$assets/bin"
  exit 0
fi

want="$(cat scripts/build-rootfs.sh | sha256sum | cut -d' ' -f1)-$(git ls-files crates/vam-runtime Cargo.lock | xargs cat | sha256sum | cut -d' ' -f1)-$(git ls-files scripts/base | xargs cat | sha256sum | cut -d' ' -f1)-${VAM_BASE}"
if [ -f "$ROOTFS" ] && [ -f "$BASE_MANIFEST" ] && [ "$(cat "$STAMP" 2>/dev/null)" = "$want" ]; then
  echo "rootfs up to date"
else
  # The standard base carries Bun (pinned); the minimal "before" image stays Bun-free as it was.
  skip_bun=0; [ "$VAM_BASE" = minimal ] && skip_bun=1
  VAM_ROOTFS_OUT="$ROOTFS" VAM_BASE="$VAM_BASE" VAM_ROOTFS_SKIP_BUN="$skip_bun" scripts/build-rootfs.sh
  echo "$want" > "$STAMP"
fi

echo "VAMD_FIRECRACKER_BIN=$FC_BIN"
echo "VAMD_JAILER_BIN=$JAILER_BIN"
echo "VAM_KERNEL_PATH=$KERNEL"
echo "VAM_ROOTFS_PATH=$ROOTFS"
echo "VAM_BASE=$VAM_BASE"
echo "VAM_BASE_MANIFEST=$BASE_MANIFEST"
if [ -n "$env_file" ]; then
  {
    echo "VAMD_FIRECRACKER_BIN=$FC_BIN"
    echo "VAM_FIRECRACKER_BIN=$FC_BIN"
    echo "VAMD_JAILER_BIN=$JAILER_BIN"
    echo "VAM_JAILER_BIN=$JAILER_BIN"
    echo "VAM_KERNEL_PATH=$KERNEL"
    echo "VAM_ROOTFS_PATH=$ROOTFS"
    echo "VAM_BASE_MANIFEST=$BASE_MANIFEST"
  } >> "$env_file"
fi
