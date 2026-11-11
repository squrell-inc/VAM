#!/bin/bash
# Build the guest rootfs (ext4) with vam-runtime injected as /sbin/init.
#
#   scripts/build-rootfs.sh             standard base: Debian slim + Python, git, curl, jq, Bun
#   scripts/build-rootfs.sh --minimal   the old busybox:musl image (deprecated; removed next release)
#
#   VAM_BASE=standard|minimal   same choice by environment (default standard)
#   VAM_ROOTFS_OUT=PATH         image path (default target/rootfs.ext4)
#   VAM_ROOTFS_SKIP_BUN=1       minimal only: leave Bun out
#
# Beside the image it writes <image without .ext4>.base.json (the base manifest vamd reads)
# and, for the standard base, <image without .ext4>.packages.txt. The standard base is built
# from scripts/base/Dockerfile at the pins in scripts/base/pins.
set -euo pipefail
export PAGER=cat

# 1. Paths and choices
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
STAGING_DIR="$WORKSPACE_ROOT/target/rootfs-staging"
ROOTFS_OUT="${VAM_ROOTFS_OUT:-$WORKSPACE_ROOT/target/rootfs.ext4}"
ROOTFS_STEM="${ROOTFS_OUT%.ext4}"
BASE_MANIFEST_OUT="$ROOTFS_STEM.base.json"
PACKAGES_OUT="$ROOTFS_STEM.packages.txt"

VAM_BASE="${VAM_BASE:-standard}"
for arg in "$@"; do
  case "$arg" in
    --minimal) VAM_BASE=minimal ;;
    --standard) VAM_BASE=standard ;;
    *) echo "usage: $0 [--standard|--minimal]" >&2; exit 2 ;;
  esac
done
case "$VAM_BASE" in
  standard|minimal) ;;
  *) echo "VAM_BASE must be standard or minimal, got '$VAM_BASE'" >&2; exit 2 ;;
esac
if [ "$VAM_BASE" = minimal ]; then
  echo "DEPRECATED: the minimal base (busybox:musl, vam/base:1) is kept for one release only; use the standard base." >&2
fi

if [ -z "${CC_x86_64_unknown_linux_musl:-}" ] && ! command -v x86_64-linux-musl-gcc >/dev/null 2>&1; then
  if command -v musl-gcc >/dev/null 2>&1; then
    export CC_x86_64_unknown_linux_musl="musl-gcc"
  elif command -v gcc >/dev/null 2>&1; then
    export CC_x86_64_unknown_linux_musl="gcc"
  fi
fi

echo "=== Building static VAM runtime binary ==="
RUSTFLAGS="-C target-feature=+crt-static" cargo build \
  --manifest-path "$WORKSPACE_ROOT/Cargo.toml" \
  --target x86_64-unknown-linux-musl \
  -p vam-runtime --bin vam-runtime \
  --release

BINARY_PATH="${CARGO_TARGET_DIR:-$WORKSPACE_ROOT/target}/x86_64-unknown-linux-musl/release/vam-runtime"

echo "=== Creating staging directory ($VAM_BASE base) ==="
sudo rm -rf "$STAGING_DIR"
mkdir -p "$STAGING_DIR"
mkdir -p "$(dirname "$ROOTFS_OUT")"

BASE_NAME=""
BASE_VERSION=""
BASE_EXTRA_JSON=""
RUNTIMES_JSON=""

if [ "$VAM_BASE" = standard ]; then
  # ---- Standard base: docker build, export, then the same injection steps ----
  # shellcheck disable=SC1091
  . "$SCRIPT_DIR/base/pins"
  for v in DEBIAN_TAG DEBIAN_DIGEST SNAPSHOT_TIMESTAMP BUN_VERSION BUN_SHA256 BASE_NAME BASE_REVISION; do
    case "${!v:-UNRESOLVED}" in
      UNRESOLVED*) echo "scripts/base/pins: $v is unresolved; run scripts/base/resolve-pins.sh --write" >&2; exit 1 ;;
    esac
  done
  BASE_VERSION="$BASE_REVISION"

  echo "=== Building the standard base image (scripts/base/Dockerfile) ==="
  BASE_IMAGE="vam-base-build:${BASE_REVISION}-${DEBIAN_DIGEST#sha256:}"
  BASE_IMAGE="${BASE_IMAGE:0:60}"
  docker build \
    -f "$SCRIPT_DIR/base/Dockerfile" \
    --build-arg "DEBIAN_TAG=$DEBIAN_TAG" \
    --build-arg "DEBIAN_DIGEST=$DEBIAN_DIGEST" \
    --build-arg "SNAPSHOT_TIMESTAMP=$SNAPSHOT_TIMESTAMP" \
    --build-arg "BUN_VERSION=$BUN_VERSION" \
    --build-arg "BUN_SHA256=$BUN_SHA256" \
    -t "$BASE_IMAGE" \
    "$SCRIPT_DIR/base"

  echo "=== Reading versions and packages from the built image ==="
  FACTS="$(docker run --rm --entrypoint /bin/sh "$BASE_IMAGE" -c '
    echo "python=$(python3 -c "import platform; print(platform.python_version())")"
    echo "git=$(git --version | awk "{print \$3}")"
    echo "bun=$(/usr/local/bin/bun --version)"
    echo "curl=$(curl --version | head -n 1 | awk "{print \$2}")"
    echo "jq=$(jq --version | sed "s/^jq-//")"
  ')"
  echo "$FACTS"
  fact() { printf '%s\n' "$FACTS" | sed -n "s/^$1=//p" | head -n 1; }
  RUNTIMES_JSON="$(printf '"python": "%s", "git": "%s", "bun": "%s", "curl": "%s", "jq": "%s"' \
    "$(fact python)" "$(fact git)" "$(fact bun)" "$(fact curl)" "$(fact jq)")"
  { docker run --rm --entrypoint /bin/sh "$BASE_IMAGE" -c 'dpkg-query -W; echo "--- pip freeze"; pip list --format=freeze'; } > "$PACKAGES_OUT"
  echo "wrote $PACKAGES_OUT ($(wc -l < "$PACKAGES_OUT") lines)"
  BASE_EXTRA_JSON="$(printf '  "from": "debian:%s@%s",\n  "snapshot": "%s",\n  "bunVersion": "%s",\n  "bunSha256": "%s",\n' \
    "$DEBIAN_TAG" "$DEBIAN_DIGEST" "$SNAPSHOT_TIMESTAMP" "$BUN_VERSION" "$BUN_SHA256")"

  echo "=== Exporting the image filesystem ==="
  CONTAINER_ID=$(docker create "$BASE_IMAGE")
  # Keep ownership and modes (Debian's system users, shadow group): extract as root.
  docker export "$CONTAINER_ID" | sudo tar -x -p --numeric-owner -C "$STAGING_DIR"
  docker rm "$CONTAINER_ID"
  sudo rm -f "$STAGING_DIR/.dockerenv"

  echo "=== Configuring users and groups (Debian's kept, guest 1000:100 added) ==="
  sudo mkdir -p "$STAGING_DIR/etc"
  if ! sudo grep -q '^guest:' "$STAGING_DIR/etc/passwd"; then
    echo "guest:x:1000:100:guest:/home/guest:/bin/bash" | sudo tee -a "$STAGING_DIR/etc/passwd" >/dev/null
  fi
  if [ -f "$STAGING_DIR/etc/shadow" ] && ! sudo grep -q '^guest:' "$STAGING_DIR/etc/shadow"; then
    echo "guest:*:19000:0:99999:7:::" | sudo tee -a "$STAGING_DIR/etc/shadow" >/dev/null
  fi
  # gid 100 is Debian's `users` group, so guest's primary group already exists.
  if ! sudo grep -q '^[^:]*:[^:]*:100:' "$STAGING_DIR/etc/group"; then
    echo "users:x:100:" | sudo tee -a "$STAGING_DIR/etc/group" >/dev/null
  fi
  printf 'vam-guest\n' | sudo tee "$STAGING_DIR/etc/hostname" >/dev/null
  printf '127.0.0.1\tlocalhost\n::1\t\tlocalhost\n' | sudo tee "$STAGING_DIR/etc/hosts" >/dev/null
  printf 'passwd:     files\ngroup:      files\nhosts:      files\n' | sudo tee "$STAGING_DIR/etc/nsswitch.conf" >/dev/null
else
  # ---- Minimal base (deprecated): busybox:musl, Bun musl baseline, musl libs ----
  BASE_NAME="vam/base:1"
  BASE_VERSION="1"

  echo "=== Exporting busybox:musl filesystem ==="
  CONTAINER_ID=$(docker create busybox:musl)
  docker export "$CONTAINER_ID" | tar -x -C "$STAGING_DIR"
  docker rm "$CONTAINER_ID"

  if [ "${VAM_ROOTFS_SKIP_BUN:-0}" = "1" ]; then
    echo "=== Skipping Bun (VAM_ROOTFS_SKIP_BUN=1) ==="
  else
    echo "=== Injecting Bun (musl baseline) ==="
    BUN_ZIP_NAME="bun-linux-x64-musl-baseline.zip"
    if [ -n "${BUN_VERSION:-}" ]; then
      BUN_URL="https://github.com/oven-sh/bun/releases/download/${BUN_VERSION}/bun-linux-x64-musl-baseline.zip"
      BUN_ZIP_NAME="${BUN_VERSION}-${BUN_ZIP_NAME}"
    else
      BUN_URL="https://github.com/oven-sh/bun/releases/latest/download/bun-linux-x64-musl-baseline.zip"
    fi
    CACHE_DIR="${VAM_BUN_CACHE:-${XDG_CACHE_HOME:-${HOME:-/tmp}/.cache}/vam/bun}"
    if ! mkdir -p "$CACHE_DIR" 2>/dev/null; then
      CACHE_DIR="${HOME:-/tmp}/.cache/vam/bun"
      mkdir -p "$CACHE_DIR"
    fi

    if [ ! -f "$CACHE_DIR/$BUN_ZIP_NAME" ]; then
      echo "=== Downloading Bun musl baseline ==="
      curl -fL "$BUN_URL" -o "$CACHE_DIR/$BUN_ZIP_NAME"
    fi

    mkdir -p "$STAGING_DIR/usr/local/bin"
    unzip -o -p "$CACHE_DIR/$BUN_ZIP_NAME" "bun-linux-x64-musl-baseline/bun" > "$STAGING_DIR/usr/local/bin/bun"
    chmod +x "$STAGING_DIR/usr/local/bin/bun"
    RUNTIMES_JSON="\"bun\": \"${BUN_VERSION:-latest}\""
  fi

  echo "=== Injecting musl dynamic loader, libc, libstdc++, and libgcc_s ==="
  CONTAINER_ID_ALPINE=$(docker run -d alpine:latest sh -c "apk add --no-cache libstdc++ && sleep 3600")
  sleep 3

  mkdir -p "$STAGING_DIR/lib"
  mkdir -p "$STAGING_DIR/usr/lib"

  docker cp "$CONTAINER_ID_ALPINE:/lib/ld-musl-x86_64.so.1" "$STAGING_DIR/lib/ld-musl-x86_64.so.1"
  docker cp "$CONTAINER_ID_ALPINE:/usr/lib/libgcc_s.so.1" "$STAGING_DIR/usr/lib/libgcc_s.so.1"

  STD_BACKING=$(docker exec "$CONTAINER_ID_ALPINE" find /usr/lib -name "libstdc++.so.6.*" | head -n 1)
  docker cp "$CONTAINER_ID_ALPINE:$STD_BACKING" "$STAGING_DIR/usr/lib/"
  STD_FILENAME=$(basename "$STD_BACKING")

  docker rm -f "$CONTAINER_ID_ALPINE"

  ln -sf ld-musl-x86_64.so.1 "$STAGING_DIR/lib/libc.musl-x86_64.so.1"
  ln -sf "$STD_FILENAME" "$STAGING_DIR/usr/lib/libstdc++.so.6"

  echo "=== Configuring users and groups ==="
  mkdir -p "$STAGING_DIR/etc"
  cat <<EOF > "$STAGING_DIR/etc/passwd"
root:x:0:0:root:/root:/bin/sh
guest:x:1000:100:guest:/home/guest:/bin/sh
EOF

  cat <<EOF > "$STAGING_DIR/etc/group"
root:x:0:
guest:x:100:
EOF

  cat <<EOF > "$STAGING_DIR/etc/hostname"
vam-guest
EOF

  cat <<EOF > "$STAGING_DIR/etc/nsswitch.conf"
passwd:     files
group:      files
hosts:      files
EOF
fi

echo "=== Creating necessary empty directories for mountpoints ==="
# The root drive is attached read-only, so every mount point must exist in the image.
sudo mkdir -p \
  "$STAGING_DIR/proc" "$STAGING_DIR/sys" "$STAGING_DIR/dev" "$STAGING_DIR/tmp" \
  "$STAGING_DIR/work" "$STAGING_DIR/home/guest" "$STAGING_DIR/mnt/drive9"

# Set permissions for tmp and guest home in staging
sudo chmod 1777 "$STAGING_DIR/tmp"
sudo chmod 755 "$STAGING_DIR/home/guest"

# vam-runtime goes in LAST, so nothing the image holds can replace PID 1 (locked decision 4).
echo "=== Injecting vam-runtime as /sbin/init ==="
sudo mkdir -p "$STAGING_DIR/sbin"
sudo rm -f "$STAGING_DIR/sbin/init"
sudo cp "$BINARY_PATH" "$STAGING_DIR/sbin/init"
sudo chmod +x "$STAGING_DIR/sbin/init"

echo "=== Adjusting ownerships to root:root inside rootfs ==="
# Since mke2fs -d preserves the source files' uid/gid, the minimal image (extracted as the
# caller) is set to root:root. The standard image keeps its own ownership; only what this
# script wrote or copied is handed to root.
if [ "$VAM_BASE" = minimal ]; then
  sudo chown -R root:root "$STAGING_DIR"
else
  sudo chown root:root "$STAGING_DIR/sbin/init" "$STAGING_DIR/etc/hostname" "$STAGING_DIR/etc/hosts" \
    "$STAGING_DIR/etc/nsswitch.conf" "$STAGING_DIR/tmp" "$STAGING_DIR/work" "$STAGING_DIR/home/guest" \
    "$STAGING_DIR/mnt/drive9" "$STAGING_DIR/proc" "$STAGING_DIR/sys" "$STAGING_DIR/dev"
fi

echo "=== Creating the ext4 image via mke2fs ==="
rm -f "$ROOTFS_OUT"
if [ "$VAM_BASE" = standard ]; then
  # Content-sized: on-disk use of the tree plus 25%, rounded up to 64 MiB.
  USED_BYTES_TREE=$(sudo du -s --block-size=1 "$STAGING_DIR" | cut -f1)
  MIB64=$((64 * 1024 * 1024))
  WANT=$((USED_BYTES_TREE + USED_BYTES_TREE / 4))
  IMAGE_BYTES=$(((WANT + MIB64 - 1) / MIB64 * MIB64))
  echo "tree uses $USED_BYTES_TREE bytes; image is $IMAGE_BYTES bytes"
  truncate -s "$IMAGE_BYTES" "$ROOTFS_OUT"
  # Fixed UUID, hash seed and clock make the layout repeatable. Inode ctimes still come from
  # the build host (mke2fs -d copies them), so the image bytes can differ between builds; the
  # package list beside the image is what two builds on the same pins must agree on.
  FIXED_UUID="7a3f1c52-5d0e-4b86-9a41-0c6e2d8b1f37"
  sudo env E2FSPROGS_FAKE_TIME=1700000000 mke2fs -t ext4 -F -O ^has_journal -m 0 \
    -U "$FIXED_UUID" -E "hash_seed=$FIXED_UUID,lazy_itable_init=0" \
    -d "$STAGING_DIR" "$ROOTFS_OUT"
else
  truncate -s 256M "$ROOTFS_OUT"
  # Format ext4 without journal (for speed and size) and copy staging directory
  sudo mke2fs -t ext4 -F -O ^has_journal -m 0 -d "$STAGING_DIR" "$ROOTFS_OUT"
fi

echo "=== Repairing permissions on the host file ==="
# mke2fs ran as sudo; hand the image back to the caller
sudo chown "$(id -u):$(id -g)" "$ROOTFS_OUT"

echo "=== Verifying the rootfs image ==="
# 1. Clean check
e2fsck -fn "$ROOTFS_OUT"

# 2. debugfs assertions
echo "=== debugfs: Verify sbin/init exists and is root-owned ==="
sudo PAGER=cat debugfs -R "stat /sbin/init" "$ROOTFS_OUT"

echo "=== debugfs: Verify etc/passwd contains guest ==="
PASSWD_IN_IMAGE="$(sudo PAGER=cat debugfs -R "cat /etc/passwd" "$ROOTFS_OUT")"
echo "$PASSWD_IN_IMAGE"
printf '%s\n' "$PASSWD_IN_IMAGE" | grep -q '^guest:x:1000:100:'

if [ "$VAM_BASE" = standard ]; then
  for f in /usr/bin/python3 /usr/bin/git /usr/local/bin/bun; do
    echo "=== debugfs: Verify $f exists ==="
    sudo PAGER=cat debugfs -R "stat $f" "$ROOTFS_OUT"
  done
  echo "=== debugfs: Verify /bin/busybox exists (PID 1 brings up loopback with it) ==="
  BB_STAT="$(sudo PAGER=cat debugfs -R "stat /bin/busybox" "$ROOTFS_OUT" 2>/dev/null || true)"
  if ! printf '%s\n' "$BB_STAT" | grep -q 'Inode:'; then
    sudo PAGER=cat debugfs -R "stat /usr/bin/busybox" "$ROOTFS_OUT"
  fi
  echo "=== debugfs: Verify the mount points exist ==="
  for d in /proc /sys /dev /tmp /work /home/guest /mnt/drive9; do
    D_STAT="$(sudo PAGER=cat debugfs -R "stat $d" "$ROOTFS_OUT")"
    printf '%s\n' "$D_STAT" | grep -q 'Type: directory' || { echo "mount point $d missing" >&2; exit 1; }
  done
elif [ "${VAM_ROOTFS_SKIP_BUN:-0}" != "1" ]; then
  echo "=== debugfs: Verify usr/local/bin/bun exists ==="
  sudo PAGER=cat debugfs -R "stat /usr/local/bin/bun" "$ROOTFS_OUT"
fi

echo "=== Writing the base manifest beside the image ==="
IMAGE_SHA256="$(sha256sum "$ROOTFS_OUT" | cut -d' ' -f1)"
IMAGE_SIZE="$(stat -c %s "$ROOTFS_OUT")"
USED_BYTES="$(sudo dumpe2fs -h "$ROOTFS_OUT" 2>/dev/null | awk -F: '
  /^Block count:/ {b=$2+0} /^Free blocks:/ {f=$2+0} /^Block size:/ {s=$2+0}
  END {print (b-f)*s}')"
{
  printf '{\n'
  printf '  "name": "%s",\n' "$BASE_NAME"
  printf '  "version": "%s",\n' "$BASE_VERSION"
  printf '  "digest": "sha256:%s",\n' "$IMAGE_SHA256"
  if [ -n "$BASE_EXTRA_JSON" ]; then printf '%s\n' "$BASE_EXTRA_JSON"; fi
  printf '  "imageBytes": %s,\n' "$IMAGE_SIZE"
  printf '  "usedBytes": %s,\n' "$USED_BYTES"
  printf '  "runtimes": {%s}\n' "$RUNTIMES_JSON"
  printf '}\n'
} > "$BASE_MANIFEST_OUT"
cat "$BASE_MANIFEST_OUT"

echo "=== Successfully built rootfs ($BASE_NAME) at $ROOTFS_OUT ==="
echo "BASE_NAME=$BASE_NAME"
echo "BASE_DIGEST=sha256:$IMAGE_SHA256"
echo "IMAGE_BYTES=$IMAGE_SIZE"
echo "USED_BYTES=$USED_BYTES"
