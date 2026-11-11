#!/usr/bin/env bash
# Download the latest aarch64 guest vmlinux from Firecracker CI and install inside Lima.
# Installs to /var/lib/vam/kernels/vmlinux-6.18 (matches resolve_guest_kernel_path()).
#
# Usage (from repo root on macOS):
#   ./scripts/fetch-guest-kernel-lima.sh

set -euo pipefail

INSTANCE="${LIMA_INSTANCE:-vam}"
KERNEL_DEST="${VAM_KERNEL_PATH:-/var/lib/vam/kernels/vmlinux-6.18}"

limactl start "$INSTANCE" 2>/dev/null || true

limactl shell "$INSTANCE" -- bash -s <<EOF
set -euo pipefail

KERNEL_DEST="$KERNEL_DEST"

if [[ -f "\$KERNEL_DEST" ]]; then
  echo "Guest kernel already present at \$KERNEL_DEST"
  file "\$KERNEL_DEST"
  exit 0
fi

echo "=== Fetching latest aarch64 vmlinux from Firecracker CI ==="
ARCH="aarch64"
S3="https://s3.amazonaws.com/spec.ccfc.min"

# Latest CI artifact prefix (date-based build id)
CI_ARTIFACTS_PREFIX=\$(curl -fsSL "\$S3?list-type=2&prefix=firecracker-ci/&delimiter=/" \\
  | grep -oP "(?<=<Prefix>)firecracker-ci/[0-9]{8}-[^/]+/(?=</Prefix>)" \\
  | sort \\
  | tail -1)

if [[ -z "\$CI_ARTIFACTS_PREFIX" ]]; then
  echo "ERROR: Could not resolve Firecracker CI artifact prefix from \$S3" >&2
  exit 1
fi

latest_kernel_key=\$(curl -fsSL "\$S3?list-type=2&prefix=\${CI_ARTIFACTS_PREFIX}\${ARCH}/vmlinux-" \\
  | grep -oP "(?<=<Key>)(\${CI_ARTIFACTS_PREFIX}\${ARCH}/vmlinux-[0-9]+\\.[0-9]+\\.[0-9]{1,3})(?=</Key>)" \\
  | sort -V \\
  | tail -1)

if [[ -z "\$latest_kernel_key" ]]; then
  echo "ERROR: No aarch64 vmlinux found under \$CI_ARTIFACTS_PREFIX" >&2
  exit 1
fi

echo "Downloading \$S3/\$latest_kernel_key"
TMP_KERNEL="/tmp/vmlinux-download"
curl -fsSL -o "\$TMP_KERNEL" "\$S3/\$latest_kernel_key"

file "\$TMP_KERNEL"

sudo mkdir -p "\$(dirname "\$KERNEL_DEST")"
sudo install -m 644 "\$TMP_KERNEL" "\$KERNEL_DEST"
rm -f "\$TMP_KERNEL"

echo "=== Installed guest kernel at \$KERNEL_DEST ==="
file "\$KERNEL_DEST"
EOF
