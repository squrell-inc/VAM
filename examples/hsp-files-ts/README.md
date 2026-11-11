# HSP TypeScript file-attachment broker

Phase 0 sample: a Node.js `HostedBroker` that binds a Unix domain socket, stores
small images on **local disk**, and exports `files.put` / `files.list` /
`files.get`. Loads [`proto/vam/v1/hosted_services.proto`](../../proto/vam/v1/hosted_services.proto)
at runtime (`@grpc/proto-loader`) — no TypeScript codegen.

This package is the gRPC **server**. `vamd` is the gRPC **client**. Phase 0
hermetic tests play the client. Phase 1 registers the broker into live `vamd`
(`PUT /v1/hosted/brokers` → HSP `Register` + `ListTools`). Guest `Invoke` is
Phase 2.

Protocol design: [Hosted services protocol](../../docs/src/content/docs/hosted/protocol.md).

## Quick start (local, no vamd)

```bash
cd examples/hsp-files-ts
npm install
npm run build
node dist/index.js --socket /tmp/hsp-files-ts.sock --data-dir /tmp/hsp-files-ts-data
```

Hermetic test (no vamd):

```bash
npm test
```

Unix domain sockets are required for live `vamd` (Phase 1). Run the broker on
Linux, macOS, Lima, or WSL:

```bash
node dist/index.js --socket /tmp/hsp-files-ts.sock --data-dir /tmp/hsp-files-ts-data
```

`npm test` uses UDS on Unix. On native Windows, the gRPC suite falls back to
TCP loopback (`127.0.0.1`) because UDS bind is often `EACCES`; the disk-store
test still always runs. That TCP fallback is tests/debug only — `PUT /v1/hosted/brokers`
still requires a Unix path.

## What each HSP RPC does

| RPC | Who calls it later | Phase 0 behavior |
|---|---|---|
| `Register` | `vamd` after `PUT /v1/hosted/brokers` | Accepts the manifest; logs `[hsp] register` |
| `ListTools` | `vamd` at catalog merge | Returns `files.put`, `files.list`, `files.get` |
| `Invoke` | `vamd` when the guest runs a hosted tool | `files.put` writes bytes under `--data-dir`; `list`/`get` read metadata |
| `Download` | `vamd` on `GET /v1/hosted/files` | Streams object bytes from disk / S3 / OCI |
| `OnRequest` / `OnComplete` / `OnResponse` | `vamd` at run boundaries | Lists stored placeholders for the session/run; injects names on OnRequest; no guest PAR |

`files.put` args: `name`, `media_type` (`image/png` \| `image/jpeg` \| `image/webp`),
`content_base64`, optional `placeholder`. The broker stores bytes **and** a
placeholder (default = sanitized `name`, e.g. `pixel.png`). That is the handle
`files.get` uses to find the locator — you do not pass an S3/OCI key.

`files.get` args (one of): `placeholder`, `name` (same as filename), or `file_id`.
It returns `placeholder`, `file_id`, `name`, `media_type`, `size_bytes`, plus
`buffer` (JSON array of 0–255). Stream with HSP `Download` /
`GET /v1/hosted/files?placeholder=pixel.png`.

## Broker identity

```json
{
  "brokerId": "files-ts",
  "trustTier": 3,
  "tools": [
    { "toolName": "files.put", "sideEffect": 3, "requiredSecrets": [] },
    { "toolName": "files.list", "sideEffect": 1, "requiredSecrets": [] },
    { "toolName": "files.get", "sideEffect": 1, "requiredSecrets": [] }
  ]
}
```

Each tool lists the secrets it needs in `requiredSecrets` (here the object
store bindings; see `put-broker.json`). `vamd` sends an invoke only the secrets
its tool's export lists, so a binding no tool lists reaches no tool. Keep
`requiredSecrets` per tool as small as the tool allows.

`trustTier` is `TrustTier.TRUST_TIER_OPERATOR` (= 3). Adapter ids in `ListTools`
are `hosted:files-ts:<tool>`.

## Phase 1: register with `vamd` (before any session)

**Register this broker before you create the guest session** so `hosted-files-ts`
can appear in the skill catalog in Phase 2.

Manifest source of truth: [`put-broker.json`](./put-broker.json) (`brokerId: files-ts`,
`trustTier: 3` operator, three `files.*` tools, optional hooks 1–3).

Open Scalar at `http://127.0.0.1:8080/docs` for `PUT`/`GET /v1/hosted/brokers`.

### Lima (full proof)

With `vamd` already up (`./scripts/start-vamd-lima.sh`):

```bash
./scripts/lima-hsp-files-ts.sh
```

That script copies this package + `proto/` into the VM, starts the broker on
`/tmp/hsp-files-ts.sock`, then from the Mac:

- `PUT /v1/hosted/brokers` with the operator manifest and `unixPath`
- `GET /v1/hosted/brokers` and asserts `files.put` / `files.list` / `files.get`

Watch `vamd` call HSP:

```bash
limactl shell vam -- tail -f /tmp/hsp-files-ts.log
```

After PUT you should see `[hsp] register` then `[hsp] listTools`.

### Curl (broker already listening on Linux / WSL)

`PUT` requires a **Unix** path. Native Windows cannot complete this step;
run the broker next to `vamd` (Lima or Linux).

```bash
MANIFEST="$(jq -c . put-broker.json)"
curl -fsS -X PUT http://127.0.0.1:8080/v1/hosted/brokers \
  -H "Authorization: Bearer ${VAMD_BOOTSTRAP_API_KEY}" \
  -H "Idempotency-Key: hsp-files-ts-1" \
  -H "Content-Type: application/json" \
  -d "{\"manifest\": ${MANIFEST}, \"unixPath\": \"/tmp/hsp-files-ts.sock\", \"idempotencyKey\": \"hsp-files-ts-1\"}"
```

The VAM app already has a generic hosted-broker PUT form (echo-ts catalog UI). There
is no extra preset in this checkout; paste the JSON above.

## Object storage (S3 or OCI)

Tools stay `files.put` / `files.list` / `files.get`. The cloud is **broker
config**, not HSP.

```bash
# default — local disk (npm test)
unset HSP_FILES_BACKEND

# AWS S3 (or MinIO with s3-endpoint)
export HSP_FILES_BACKEND=s3

# OCI Object Storage
export HSP_FILES_BACKEND=oci
```

Put credentials in the **`vamd` vault**, then `PUT` the bindings in
`put-broker.json` (`required: false` so local mode still works). On `Invoke`,
`vamd` fills `resolved_secrets`. Do not commit keys.

| Backend | Secrets | Optional |
|---|---|---|
| `s3` | `s3-access-key`, `s3-secret-key`, `s3-region`, `s3-bucket` | `s3-endpoint` (MinIO / custom URL) |
| `oci` | `oci-tenancy`, `oci-user`, `oci-fingerprint`, `oci-private-key`, `oci-region`, `oci-namespace`, `oci-bucket` | — |

Restart the Node broker after changing `HSP_FILES_BACKEND`. Prefix defaults:
`HSP_S3_PREFIX` / `HSP_OCI_PREFIX` (`vam/files-ts/`).

Hermetic `npm test` uses in-memory S3/OCI transports — no real cloud.

### Vault + HTTP Invoke (Postman / curl)

The Node broker must still be running locally (`--socket /tmp/hsp-files-ts.sock`)
with `HSP_FILES_BACKEND=s3` or `oci`. Secrets and the upload itself are HTTP.

1. `PUT /v1/secrets` for each vault key (`kind: 2`).
2. `PUT /v1/hosted/brokers` with `put-broker.json` (include `secretBindings`).
3. `POST /v1/hosted/invoke` — vamd injects vault secrets and calls HSP `Invoke`.

```bash
curl -fsS -X POST http://127.0.0.1:8080/v1/hosted/invoke \
  -H "Authorization: Bearer ${VAMD_BOOTSTRAP_API_KEY}" \
  -H "Idempotency-Key: files-put-s3-1" \
  -H "Content-Type: application/json" \
  -d '{
    "brokerId": "files-ts",
    "toolName": "files.put",
    "sessionId": "api",
    "runId": "s3",
    "args": {
      "name": "pixel.png",
      "media_type": "image/png",
      "content_base64": "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg=="
    }
  }'
```

Object key: `vam/files-ts/api/s3/<sha256>.png`. Scalar: `http://127.0.0.1:8080/docs`.

Download the object (chunked octets, not a JSON buffer). **`sessionId` / `runId` must
match the put** (`POST /v1/hosted/invoke` defaults `runId` to `default` if omitted):

```bash
curl -fsS -o pixel.png \
  -H "Authorization: Bearer ${VAMD_BOOTSTRAP_API_KEY}" \
  "http://127.0.0.1:8080/v1/hosted/files?brokerId=files-ts&sessionId=api&runId=default&placeholder=pixel.png"
```

Guest results still must not include signed URLs.

## Not in this phase

- Guest `ExecuteHostTool` → `Invoke` (Phase 2; needs a booting microVM)
- `POST /v1/files`, signed download URLs, product `vam.files`
