#!/usr/bin/env bash
# Download x86_64 guest vmlinux 6.1 and 6.18 from Firecracker CI and install on the host.
# Keeps any existing vmlinux-5.10.211 in place.
#
# Usage (from repo root on x86_64 Linux):
#   ./scripts/fetch-guest-kernels.sh
#
# Installs to:
#   /var/lib/vam/kernels/vmlinux-6.1
#   /var/lib/vam/kernels/vmlinux-6.18

set -euo pipefail

KERNEL_DIR="${VAM_KERNEL_DIR:-/var/lib/vam/kernels}"
ARCH="x86_64"
S3="https://s3.amazonaws.com/spec.ccfc.min"

fetch_kernel() {
  local dest_name="$1"
  local dest_path="${KERNEL_DIR}/${dest_name}"

  if [[ -f "${dest_path}" ]]; then
    echo "Guest kernel already present at ${dest_path}"
    file "${dest_path}"
    return 0
  fi

  echo "=== Fetching latest ${ARCH} ${dest_name} from Firecracker CI ==="

  local ci_prefix
  ci_prefix="$(curl -fsSL "${S3}?list-type=2&prefix=firecracker-ci/&delimiter=/" \
    | grep -oP '(?<=<Prefix>)firecracker-ci/[0-9]{8}-[^/]+/(?=</Prefix>)' \
    | sort \
    | tail -1)"

  if [[ -z "${ci_prefix}" ]]; then
    echo "ERROR: Could not resolve Firecracker CI artifact prefix from ${S3}" >&2
    exit 1
  fi

  local version_prefix
  case "${dest_name}" in
    vmlinux-6.1) version_prefix="vmlinux-6.1" ;;
    vmlinux-6.18) version_prefix="vmlinux-6.18" ;;
    *)
      echo "ERROR: unsupported kernel name ${dest_name}" >&2
      exit 1
      ;;
  esac

  local latest_key
  # Use a trailing dot for 6.1 so the S3 prefix does not also match vmlinux-6.18.*
  local list_prefix="${version_prefix}"
  if [[ "${dest_name}" == "vmlinux-6.1" ]]; then
    list_prefix="vmlinux-6.1."
  fi
  latest_key="$(curl -fsSL "${S3}?list-type=2&prefix=${ci_prefix}${ARCH}/${list_prefix}" \
    | grep -oP "(?<=<Key>)(${ci_prefix}${ARCH}/${version_prefix}[0-9.]*)(?=</Key>)" \
    | grep -v '/vmlinux-6\.18' \
    | sort -V \
    | tail -1)"

  if [[ -z "${latest_key}" ]]; then
    echo "ERROR: No ${ARCH} ${version_prefix} found under ${ci_prefix}" >&2
    exit 1
  fi

  echo "Downloading ${S3}/${latest_key}"
  local tmp_kernel
  tmp_kernel="$(mktemp /tmp/vmlinux-download.XXXXXX)"
  curl -fsSL -o "${tmp_kernel}" "${S3}/${latest_key}"
  file "${tmp_kernel}"

  sudo mkdir -p "${KERNEL_DIR}"
  sudo install -m 644 "${tmp_kernel}" "${dest_path}"
  rm -f "${tmp_kernel}"

  echo "=== Installed guest kernel at ${dest_path} ==="
  file "${dest_path}"
}

fetch_kernel "vmlinux-6.1"
fetch_kernel "vmlinux-6.18"

echo "=== Kernel install complete ==="
ls -la "${KERNEL_DIR}"/vmlinux-* 2>/dev/null || true
