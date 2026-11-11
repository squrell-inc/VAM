#!/usr/bin/env bash
# Build an aarch64 Bun rootfs inside the Lima "vam" instance.
# Output: /tmp/lima/rootfs.ext4 (writable mount; home is read-only in lima-macos.yaml)
#
# Usage (from repo root on macOS):
#   ./scripts/build-rootfs-lima.sh

# NOTE (machine-base, open question 12): this script stays on the minimal busybox image.
# scripts/base/Dockerfile is not arm64-ready yet: it pins the x64 Bun build (bun-linux-x64-baseline,
# one sha256) and its image digest and snapshot were only checked for amd64. Checked by hand
# on Lima it needs a bun-linux-aarch64 pin; until then Lima builds the minimal base, which the
# standard build script also keeps for one release as `VAM_BASE=minimal`.
set -euo pipefail

INSTANCE="${LIMA_INSTANCE:-vam}"
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

limactl start "$INSTANCE" 2>/dev/null || true

limactl shell "$INSTANCE" -- bash -s <<EOF
set -euo pipefail
export PAGER=cat
export PATH="\$HOME/.cargo/bin:/usr/local/bin:\$PATH"

WORKSPACE_ROOT="$REPO"
STAGING_DIR="/tmp/rootfs-staging"
ROOTFS_OUT="/tmp/lima/rootfs.ext4"
CACHE_DIR="\$HOME/.cache/vam/bun"
CARGO_TARGET_DIR=/tmp/vam-target

echo "=== Installing one-time build dependencies ==="
if ! command -v docker >/dev/null 2>&1 || ! command -v mke2fs >/dev/null 2>&1; then
  sudo apt-get update -qq
  sudo apt-get install -y -qq docker.io e2fsprogs unzip musl-tools build-essential curl
  sudo usermod -aG docker "\$USER" 2>/dev/null || true
fi

if ! groups | grep -q docker; then
  echo "Note: docker group not active in this shell; using sudo for docker commands."
  DOCKER="sudo docker"
else
  DOCKER="docker"
fi

if ! command -v cargo >/dev/null 2>&1; then
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
  source "\$HOME/.cargo/env"
fi
source "\$HOME/.cargo/env" 2>/dev/null || true
rustup target add aarch64-unknown-linux-musl 2>/dev/null || true

if [ -z "\${CC_aarch64_unknown_linux_musl:-}" ] && ! command -v aarch64-linux-musl-gcc >/dev/null 2>&1; then
  if command -v musl-gcc >/dev/null 2>&1; then
    export CC_aarch64_unknown_linux_musl="musl-gcc"
  elif command -v gcc >/dev/null 2>&1; then
    export CC_aarch64_unknown_linux_musl="gcc"
  fi
fi

mkdir -p /tmp/lima "\$CACHE_DIR"

echo "=== Building static VAM runtime binary (aarch64 musl) ==="
RUSTFLAGS="-C target-feature=+crt-static" CARGO_TARGET_DIR="\$CARGO_TARGET_DIR" cargo build \\
  --manifest-path "\$WORKSPACE_ROOT/Cargo.toml" \\
  --target aarch64-unknown-linux-musl \\
  --bin vam-runtime \\
  --release

BINARY_PATH="\$CARGO_TARGET_DIR/aarch64-unknown-linux-musl/release/vam-runtime"

echo "=== Creating staging directory ==="
sudo rm -rf "\$STAGING_DIR"
mkdir -p "\$STAGING_DIR"

echo "=== Exporting busybox:musl filesystem ==="
CONTAINER_ID=\$(\$DOCKER create busybox:musl)
\$DOCKER export "\$CONTAINER_ID" | tar -x -C "\$STAGING_DIR"
\$DOCKER rm "\$CONTAINER_ID"

echo "=== Injecting vam-runtime as /sbin/init ==="
mkdir -p "\$STAGING_DIR/sbin"
cp "\$BINARY_PATH" "\$STAGING_DIR/sbin/init"
chmod +x "\$STAGING_DIR/sbin/init"

echo "=== Injecting Bun (aarch64 musl) ==="
BUN_ZIP_NAME="bun-linux-aarch64-musl.zip"
BUN_URL="https://github.com/oven-sh/bun/releases/latest/download/bun-linux-aarch64-musl.zip"
mkdir -p "\$CACHE_DIR"

if [ ! -f "\$CACHE_DIR/\$BUN_ZIP_NAME" ]; then
  echo "=== Downloading Bun aarch64 musl ==="
  curl -L "\$BUN_URL" -o "\$CACHE_DIR/\$BUN_ZIP_NAME"
fi

mkdir -p "\$STAGING_DIR/usr/local/bin"
unzip -o -p "\$CACHE_DIR/\$BUN_ZIP_NAME" "bun-linux-aarch64-musl/bun" > "\$STAGING_DIR/usr/local/bin/bun"
chmod +x "\$STAGING_DIR/usr/local/bin/bun"

echo "=== Injecting musl dynamic loader, libc, libstdc++, and libgcc_s ==="
CONTAINER_ID_ALPINE=\$(\$DOCKER run -d alpine:latest sh -c "apk add --no-cache libstdc++ && sleep 3600")
sleep 3

mkdir -p "\$STAGING_DIR/lib"
mkdir -p "\$STAGING_DIR/usr/lib"

\$DOCKER cp "\$CONTAINER_ID_ALPINE:/lib/ld-musl-aarch64.so.1" "\$STAGING_DIR/lib/ld-musl-aarch64.so.1"
\$DOCKER cp "\$CONTAINER_ID_ALPINE:/usr/lib/libgcc_s.so.1" "\$STAGING_DIR/usr/lib/libgcc_s.so.1"

STD_BACKING=\$(\$DOCKER exec "\$CONTAINER_ID_ALPINE" find /usr/lib -name "libstdc++.so.6.*" | head -n 1)
\$DOCKER cp "\$CONTAINER_ID_ALPINE:\$STD_BACKING" "\$STAGING_DIR/usr/lib/"
STD_FILENAME=\$(basename "\$STD_BACKING")

\$DOCKER rm -f "\$CONTAINER_ID_ALPINE"

ln -sf ld-musl-aarch64.so.1 "\$STAGING_DIR/lib/libc.musl-aarch64.so.1"
ln -sf "\$STD_FILENAME" "\$STAGING_DIR/usr/lib/libstdc++.so.6"

echo "=== Configuring users and groups ==="
mkdir -p "\$STAGING_DIR/etc"
cat <<PASSWD > "\$STAGING_DIR/etc/passwd"
root:x:0:0:root:/root:/bin/sh
guest:x:1000:100:guest:/home/guest:/bin/sh
PASSWD

cat <<GROUP > "\$STAGING_DIR/etc/group"
root:x:0:
guest:x:100:
GROUP

cat <<HOSTNAME > "\$STAGING_DIR/etc/hostname"
vam-guest
HOSTNAME

cat <<NSS > "\$STAGING_DIR/etc/nsswitch.conf"
passwd:     files
group:      files
hosts:      files
NSS

echo "=== Creating necessary empty directories for mountpoints ==="
mkdir -p "\$STAGING_DIR/proc"
mkdir -p "\$STAGING_DIR/sys"
mkdir -p "\$STAGING_DIR/dev"
mkdir -p "\$STAGING_DIR/tmp"
mkdir -p "\$STAGING_DIR/work"
mkdir -p "\$STAGING_DIR/home/guest"
mkdir -p "\$STAGING_DIR/mnt/drive9"

chmod 1777 "\$STAGING_DIR/tmp"
chmod 755 "\$STAGING_DIR/home/guest"

echo "=== Adjusting ownerships to root:root inside rootfs ==="
sudo chown -R root:root "\$STAGING_DIR"

echo "=== Creating the ext4 image via mke2fs ==="
rm -f "\$ROOTFS_OUT"
truncate -s 256M "\$ROOTFS_OUT"

sudo mke2fs -t ext4 -F -O ^has_journal -m 0 -d "\$STAGING_DIR" "\$ROOTFS_OUT"

echo "=== Repairing permissions on the output file ==="
sudo chown "\$(whoami):\$(whoami)" "\$ROOTFS_OUT"

echo "=== Verifying the rootfs image ==="
e2fsck -fn "\$ROOTFS_OUT"

echo "=== debugfs: Verify sbin/init exists and is root-owned ==="
sudo PAGER=cat debugfs -R "stat /sbin/init" "\$ROOTFS_OUT"

echo "=== debugfs: Verify etc/passwd contains guest ==="
sudo PAGER=cat debugfs -R "cat /etc/passwd" "\$ROOTFS_OUT"

echo "=== debugfs: Verify usr/local/bin/bun exists ==="
sudo PAGER=cat debugfs -R "stat /usr/local/bin/bun" "\$ROOTFS_OUT"

echo "=== Successfully built rootfs at \$ROOTFS_OUT ==="
EOF

echo ""
echo "Rootfs ready at /tmp/lima/rootfs.ext4 (inside Lima; also on Mac at /tmp/lima/rootfs.ext4)."
echo "Next: ./scripts/start-vamd-lima.sh"
