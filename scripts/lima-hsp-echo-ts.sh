#!/usr/bin/env bash
# Register the TypeScript HSP echo broker with a live vamd on Lima and assert catalog.
#
# Prerequisites:
#   - limactl instance "vam" (scripts/lima-macos.yaml)
#   - vamd listening on host-forwarded http://127.0.0.1:8080 (start-vamd-lima.sh runs it with --insecure-dev)
#     (./scripts/start-vamd-lima.sh)
#
# Lima mounts ~ read-only, so npm install runs under /tmp/lima/hsp-echo-ts.

set -euo pipefail

INSTANCE="${LIMA_INSTANCE:-vam}"
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BOOTSTRAP_KEY="${VAMD_BOOTSTRAP_API_KEY:-vam_sk_dev_18cd74a2a97ff4d1}"
VAMD_URL="${VAMD_URL:-http://127.0.0.1:8080}"
SOCKET="/tmp/hsp-echo-ts.sock"
LIMA_DIR="/tmp/lima/hsp-echo-ts"
IDEM="hsp-echo-ts-$(date +%s)"

echo "==> Checking vamd at ${VAMD_URL}/v1/health"
curl -fsS "${VAMD_URL}/v1/health" >/dev/null

echo "==> Ensuring Node.js inside Lima"
limactl shell "$INSTANCE" -- bash -s <<'EOF'
set -euo pipefail
export PATH="/usr/local/bin:$PATH"
if ! command -v node >/dev/null 2>&1 || ! command -v npm >/dev/null 2>&1; then
  # Prefer apt when mirrors work; fall back to official Node binary (HTTPS).
  if sudo apt-get update -qq && sudo apt-get install -y -qq nodejs npm; then
    echo "Installed Node.js via apt"
  else
    echo "apt unavailable; installing Node.js LTS binary from nodejs.org..."
    ARCH="$(uname -m)"
    case "$ARCH" in
      aarch64|arm64) NODE_ARCH=arm64 ;;
      x86_64|amd64) NODE_ARCH=x64 ;;
      *) echo "Unsupported arch: $ARCH" >&2; exit 1 ;;
    esac
    NODE_VER=v22.14.0
    TARBALL="node-${NODE_VER}-linux-${NODE_ARCH}.tar.xz"
    curl -fsSL "https://nodejs.org/dist/${NODE_VER}/${TARBALL}" -o "/tmp/${TARBALL}"
    sudo tar -xJf "/tmp/${TARBALL}" -C /usr/local --strip-components=1
    rm -f "/tmp/${TARBALL}"
  fi
fi
node --version
npm --version
EOF

echo "==> Copying examples/hsp-echo-ts + proto/ to ${LIMA_DIR}"
limactl shell "$INSTANCE" -- bash -s <<EOF
set -euo pipefail
rm -rf "$LIMA_DIR"
mkdir -p "$LIMA_DIR"
# Repo is mounted read-only; copy into writable /tmp/lima.
cp -a "$REPO/examples/hsp-echo-ts/." "$LIMA_DIR/"
mkdir -p "$LIMA_DIR/proto"
cp -a "$REPO/proto/." "$LIMA_DIR/proto/"
cd "$LIMA_DIR"
npm install
npm run build
EOF

echo "==> Starting TypeScript echo broker on ${SOCKET}"
limactl shell "$INSTANCE" -- bash -s <<EOF
set -euo pipefail
pkill -f "hsp-echo-ts.sock" 2>/dev/null || true
rm -f "$SOCKET"
cd "$LIMA_DIR"
nohup node dist/index.js --socket "$SOCKET" --proto-root "$LIMA_DIR/proto" \\
  >/tmp/hsp-echo-ts.log 2>&1 &
echo \$! >/tmp/hsp-echo-ts.pid
for i in \$(seq 1 50); do
  if [[ -S "$SOCKET" ]]; then
    echo "socket ready"
    exit 0
  fi
  sleep 0.1
done
echo "ERROR: socket did not appear" >&2
cat /tmp/hsp-echo-ts.log >&2 || true
exit 1
EOF

echo "==> PUT /v1/hosted/brokers (echo-ts)"
PUT_BODY=$(cat <<JSON
{
  "manifest": {
    "brokerId": "echo-ts",
    "trustTier": 2,
    "tools": [
      {
        "toolName": "echo.ping",
        "sideEffect": 0,
        "requiredSecrets": []
      }
    ],
    "hooks": [
      { "phase": 1, "required": false },
      { "phase": 2, "required": false },
      { "phase": 3, "required": false }
    ]
  },
  "unixPath": "${SOCKET}",
  "idempotencyKey": "${IDEM}"
}
JSON
)

PUT_RESP=$(curl -fsS -X PUT "${VAMD_URL}/v1/hosted/brokers" \
  -H "Authorization: Bearer ${BOOTSTRAP_KEY}" \
  -H "Idempotency-Key: ${IDEM}" \
  -H "Content-Type: application/json" \
  -d "${PUT_BODY}")
echo "$PUT_RESP" | grep -q "echo.ping"
echo "PUT ok"

echo "==> GET /v1/hosted/brokers"
GET_RESP=$(curl -fsS "${VAMD_URL}/v1/hosted/brokers" \
  -H "Authorization: Bearer ${BOOTSTRAP_KEY}")
echo "$GET_RESP" | grep -q "echo.ping"
echo "$GET_RESP" | grep -q "echo-ts"
echo "GET ok — echo.ping present in catalog"

echo
echo "Done. Scalar docs: ${VAMD_URL}/docs"
echo "Broker log (Lima): /tmp/hsp-echo-ts.log"
