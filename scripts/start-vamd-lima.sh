#!/usr/bin/env bash
# Start vamd inside the Lima "vam" instance for local Mac testing.
# Requires: limactl instance "vam" from scripts/lima-macos.yaml
# Requires: /tmp/lima/rootfs.ext4 from scripts/build-rootfs-lima.sh
#
# Optional first org admin (forwarded into Lima; never echo the password):
#   VAMD_BOOTSTRAP_ADMIN=admin VAMD_BOOTSTRAP_ADMIN_PASSWORD='…' ./scripts/start-vamd-lima.sh
#
# vamd runs with --insecure-dev here (plain HTTP on the forwarded port; the node reports
# TLS off) and --no-jailer (vamd is not root in Lima, and the jailer needs root; the node
# reports `jailer off`). The CLI defaults to https://, so point it at the http:// URL
# explicitly.
#
# On the Mac (after this script):
#   vam config set-host http://127.0.0.1:8080
#   vam config set-key --from-env VAMD_BOOTSTRAP_API_KEY   # not --token
#   vam health
#   vam login --username admin   # if VAMD_BOOTSTRAP_ADMIN was set

set -euo pipefail

INSTANCE="${LIMA_INSTANCE:-vam}"
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BOOTSTRAP_KEY="${VAMD_BOOTSTRAP_API_KEY:-vam_sk_dev_18cd74a2a97ff4d1}"
BOOTSTRAP_ADMIN_EXPORT=""
BOOTSTRAP_ADMIN_PASSWORD_EXPORT=""
if [[ -n "${VAMD_BOOTSTRAP_ADMIN:-}" ]]; then
  BOOTSTRAP_ADMIN_EXPORT="export VAMD_BOOTSTRAP_ADMIN=$(printf '%q' "$VAMD_BOOTSTRAP_ADMIN")"
fi
if [[ -n "${VAMD_BOOTSTRAP_ADMIN_PASSWORD:-}" ]]; then
  BOOTSTRAP_ADMIN_PASSWORD_EXPORT="export VAMD_BOOTSTRAP_ADMIN_PASSWORD=$(printf '%q' "$VAMD_BOOTSTRAP_ADMIN_PASSWORD")"
fi
# Scripted mock model (test only, design/e2e-journeys.md D3). Forwarded only when the caller sets it.
MOCK_SCRIPTS_EXPORT=""
if [[ -n "${VAMD_E2E_MOCK_SCRIPTS:-}" ]]; then
  MOCK_SCRIPTS_EXPORT="export VAMD_E2E_MOCK_SCRIPTS=$(printf '%q' "$VAMD_E2E_MOCK_SCRIPTS")"
fi
ROOTFS_LIMA="/tmp/lima/rootfs.ext4"
KERNEL_PATH="/var/lib/vam/kernels/vmlinux-6.18"

limactl start "$INSTANCE" 2>/dev/null || true

# Ensure guest kernel is present
if ! limactl shell "$INSTANCE" -- test -f "$KERNEL_PATH"; then
  echo "Guest kernel missing at $KERNEL_PATH — fetching from Firecracker CI..."
  "$REPO/scripts/fetch-guest-kernel-lima.sh"
fi

# Require Lima-built aarch64 rootfs
if ! limactl shell "$INSTANCE" -- test -f "$ROOTFS_LIMA"; then
  echo "ERROR: $ROOTFS_LIMA not found inside Lima." >&2
  echo "Run ./scripts/build-rootfs-lima.sh first (expect 10–20 minutes on first build)." >&2
  exit 1
fi

# vamd reads the mock scripts from the Mac checkout, which Lima mounts read-only at the same path.
if [[ -n "${VAMD_E2E_MOCK_SCRIPTS:-}" ]] && ! limactl shell "$INSTANCE" -- test -d "$VAMD_E2E_MOCK_SCRIPTS"; then
  echo "ERROR: VAMD_E2E_MOCK_SCRIPTS=$VAMD_E2E_MOCK_SCRIPTS is not a directory inside Lima." >&2
  exit 1
fi

limactl shell "$INSTANCE" -- bash -s <<EOF
set -euo pipefail
export PATH="\$HOME/.cargo/bin:/usr/local/bin:\$PATH"

# Build vamd (aarch64 Linux inside Lima)
echo "Building vamd inside Lima..."
cd "$REPO"
CARGO_TARGET_DIR=/tmp/vam-target cargo build -p vam-orchestrator --release

# Writable Drive9 tree (default /var/lib/vam/drive9 needs root)
mkdir -p /tmp/vam-drive9 /tmp/vam-kernels /tmp/vam-snapshots

# Firecracker needs read/write on /dev/kvm (Lima user is not in group kvm by default).
if [[ -e /dev/kvm ]] && ! [[ -r /dev/kvm && -w /dev/kvm ]]; then
  if getent group kvm >/dev/null; then
    sudo usermod -aG kvm "\$(whoami)" 2>/dev/null || true
  fi
  sudo setfacl -m "u:\$(whoami):rw" /dev/kvm 2>/dev/null || sudo chmod 666 /dev/kvm
fi

# vamd has no built-in guest rootfs path: it is passed as VAM_ROOTFS_PATH below.

# Firecracker aarch64 v1.16.1 (skip if already installed)
if [[ ! -x /usr/local/bin/firecracker ]]; then
  FC_VER=1.16.1
  curl -fsSL -o /tmp/firecracker.tgz \\
    "https://github.com/firecracker-microvm/firecracker/releases/download/v\${FC_VER}/firecracker-v\${FC_VER}-aarch64.tgz"
  tar xf /tmp/firecracker.tgz -C /tmp
  sudo install -m 755 /tmp/release-v\${FC_VER}-aarch64/firecracker-v\${FC_VER}-aarch64 /usr/local/bin/firecracker
  sudo install -m 755 /tmp/release-v\${FC_VER}-aarch64/jailer-v\${FC_VER}-aarch64 /usr/local/bin/jailer
fi
# The jailer preflight wants the binaries owned by root (or the vamd user) and not group/other
# writable.
sudo chown root:root /usr/local/bin/firecracker /usr/local/bin/jailer
sudo chmod go-w /usr/local/bin/firecracker /usr/local/bin/jailer

pkill -9 vamd 2>/dev/null || true
pkill -9 firecracker 2>/dev/null || true
sleep 1

# Drop stale dev DB so orphaned session rows do not block new guest_cid allocation.
rm -f /tmp/vamd-test.db /tmp/vamd-test.db-wal /tmp/vamd-test.db-shm

# Import LLM / egress keys from repo .env into the vamd secret vault at startup.
ENV_FILE="$REPO/.env"
if [[ -f "\$ENV_FILE" ]]; then
  set -a
  # shellcheck disable=SC1090
  source "\$ENV_FILE"
  set +a
  echo "Loaded \$ENV_FILE for VAMD_IMPORT_ENV_SECRETS"
else
  echo "WARN: \$ENV_FILE not found — set OPENROUTER_API_KEY in Lima env or add repo .env" >&2
fi
export VAMD_IMPORT_ENV_SECRETS=1
# Report every inference key that is set, by name only. Values are never printed.
INFERENCE_KEYS_SET=()
for key_name in OPENROUTER_API_KEY OPENAI_API_KEY ANTHROPIC_API_KEY AI_GATEWAY_API_KEY; do
  if [[ -n "\${!key_name:-}" ]]; then
    INFERENCE_KEYS_SET+=("\$key_name")
  fi
done
if [[ "\${#INFERENCE_KEYS_SET[@]}" -gt 0 ]]; then
  echo "inference keys set (will import into vault at vamd startup): \${INFERENCE_KEYS_SET[*]}"
else
  echo "WARN: no inference API key in env — runs will fail or use mock-model without OPENROUTER_API_KEY" >&2
fi

export VAMD_LISTEN=0.0.0.0:8080
export VAMD_TELEMETRY_CAPTURE="${VAMD_TELEMETRY_CAPTURE:-redacted-full}"
export VAMD_TELEMETRY_LISTEN="${VAMD_TELEMETRY_LISTEN:-127.0.0.1:9090}"
export VAMD_OTLP_ENDPOINT="${VAMD_OTLP_ENDPOINT:-http://127.0.0.1:4318}"
export VAMD_DB_PATH=/tmp/vamd-test.db
export VAMD_BOOTSTRAP_API_KEY="$BOOTSTRAP_KEY"
${BOOTSTRAP_ADMIN_EXPORT}
${BOOTSTRAP_ADMIN_PASSWORD_EXPORT}
${MOCK_SCRIPTS_EXPORT}
export VAMD_FIRECRACKER_BIN=/usr/local/bin/firecracker
export VAMD_JAILER_BIN=/usr/local/bin/jailer
export VAM_ROOTFS_PATH="$ROOTFS_LIMA"
export VAM_DRIVE9_ROOT=/tmp/vam-drive9
export VAMD_SECRETS_MASTER_KEY="\${VAMD_SECRETS_MASTER_KEY:-\$(openssl rand -hex 32)}"

# Optional loopback collector. Do not fail the script if the binary is missing.
# OTLP stays inside Lima; Mac still uses 8080 only (no 4317/4318 forward).
COLLECTOR_CFG="$REPO/scripts/otel-collector-config.yaml"
if command -v otelcol >/dev/null 2>&1; then
  pkill -x otelcol 2>/dev/null || true
  nohup otelcol --config "\$COLLECTOR_CFG" > /tmp/otelcol.log 2>&1 &
  echo "Started otelcol with \$COLLECTOR_CFG (loopback 4317/4318)"
elif command -v otelcol-contrib >/dev/null 2>&1; then
  pkill -x otelcol-contrib 2>/dev/null || true
  nohup otelcol-contrib --config "\$COLLECTOR_CFG" > /tmp/otelcol.log 2>&1 &
  echo "Started otelcol-contrib with \$COLLECTOR_CFG (loopback 4317/4318)"
else
  echo "otelcol not installed — vamd OTLP export will fail-open (loopback 4318)"
fi

# vamd serves TLS by default. This is a dev host reached over a forwarded port with
# curl and the vam CLI at plain http://, so it starts with --insecure-dev: plain HTTP, a loud
# warning in the log, and the node reports TLS off.
# The jailer is on by default and needs root; vamd is not root here, so it opts out
# explicitly (a WARN in the log, and the node reports `jailer off`).
echo "NOTE: starting vamd with --insecure-dev (API over plain HTTP, dev host only)"
echo "NOTE: starting vamd with --no-jailer (Firecracker unconfined, dev host only)"
nohup /tmp/vam-target/release/vamd --insecure-dev --no-jailer > /tmp/vamd.log 2>&1 &
disown

echo "Waiting for vamd to start..."
READY=0
for i in {1..30}; do
  if curl -sf http://127.0.0.1:8080/v1/health >/dev/null 2>&1; then
    READY=1
    echo "vamd is ready and listening on 0.0.0.0:8080!"
    break
  fi
  sleep 1
done

if [[ "\$READY" -eq 0 ]]; then
  echo "ERROR: vamd failed to become healthy within 30 seconds." >&2
  tail -n 30 /tmp/vamd.log >&2
  exit 1
fi
EOF

echo ""
echo "Mac CLI setup:"
echo "  export VAMD_BOOTSTRAP_API_KEY=$BOOTSTRAP_KEY"
echo "  vam config set-host http://127.0.0.1:8080"
echo "  vam config set-key --from-env VAMD_BOOTSTRAP_API_KEY"
echo "  vam health"
if [[ -n "${VAMD_BOOTSTRAP_ADMIN:-}" || -n "${VAMD_BOOTSTRAP_ADMIN_PASSWORD:-}" ]]; then
  echo "First org admin (password login, not the bootstrap key):"
  echo "  vam login --username ${VAMD_BOOTSTRAP_ADMIN:-operator}"
fi
