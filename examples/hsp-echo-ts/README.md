# HSP TypeScript echo broker

Phase 0 sample: a Node.js `HostedBroker` that binds a Unix domain socket and
exports `echo.ping`. Loads [`proto/vam/v1/hosted_services.proto`](../../proto/vam/v1/hosted_services.proto)
at runtime (`@grpc/proto-loader`) — no TypeScript codegen in this phase.

Phase 2: every HSP RPC prints `[hsp] <rpc> {…}` to stdout (broker console). The
PUT manifest declares optional participation in all three hook phases so a guest
run produces `onRequest` / `invoke` / `onComplete` / `onResponse` lines, not
only `invoke`.

Protocol design: [Hosted services protocol](../../docs/src/content/docs/hosted/protocol.md).

## Quick start (local)

```bash
cd examples/hsp-echo-ts
npm install
npm run build
node dist/index.js --socket /tmp/hsp-echo-ts.sock
```

Hermetic test (no vamd):

```bash
npm test
```

## Lima: register before the session

Lima mounts the repo read-only, so `npm install` must run under `/tmp/lima`.
With `vamd` already up (`./scripts/start-vamd-lima.sh`):

```bash
./scripts/lima-hsp-echo-ts.sh
```

That script copies this package + `proto/` into the VM, starts the broker on
`/tmp/hsp-echo-ts.sock`, then from the Mac issues:

- `PUT /v1/hosted/brokers` with `broker_id: echo-ts`, `trust_tier: FIRST_PARTY`,
  and `hooks` for phases 1–3 (`on_request`, `on_complete`, `on_response`)
- `GET /v1/hosted/brokers` and asserts `echo.ping` is in the catalog

**Register the broker before you create the guest session** so `hosted-echo-ts`
is in the skill catalog.

Open Scalar at `http://127.0.0.1:8080/docs` to inspect the same paths.

## Watch the broker console

This is the observe surface for guest invoke. In a dedicated terminal:

```bash
limactl shell vam -- tail -f /tmp/hsp-echo-ts.log
```

Stdout from the Node process is redirected there. Each RPC logs
`[hsp] <rpcName>` plus JSON of the request (`session_id`, `run_id`, `tool_name`,
args, hook bodies). `resolvedSecrets` values are **not** printed — only secret
**keys**.

Expected lines after register + a guest run:

| When | Log |
|---|---|
| PUT / reconnect | `[hsp] register` then `[hsp] listTools` |
| Run start (hooks in PUT) | `[hsp] onRequest` with session/run ids |
| Agent calls `echo.ping` | `[hsp] invoke` with `toolName` / args (`message`) |
| Run end | `[hsp] onComplete` then `[hsp] onResponse` |

Handlers still return OK / echo the ping payload.

## Load and run the example agent

Suggested prompt:

`Load skill hosted-echo-ts, then call echo.ping with message hello-hsp.`

**CLI** (do not use `--follow` as the proof — watch the tail instead):

```bash
vam run --file ./examples/hsp-echo-ts
# or:
vam run --file ./examples/hsp-echo-ts/run.json --prompt "Load skill hosted-echo-ts, then call echo.ping with message hello-hsp."
```

**The VAM app:** load this example directory (`agent.json` + `policy.cedar`), start a
session after the broker is registered, and send the same prompt in chat.

Cedar (`policy.cedar`) permits `ExecuteTool` on `load_skill` and `echo.ping`
only. Instructions in `agent.json` require `load_skill` with `"hosted-echo-ts"`
before `echo.ping`.

## Manifest shape

```json
{
  "brokerId": "echo-ts",
  "trustTier": 2,
  "tools": [{ "toolName": "echo.ping", "sideEffect": 0, "requiredSecrets": [] }],
  "hooks": [
    { "phase": 1, "required": false },
    { "phase": 2, "required": false },
    { "phase": 3, "required": false }
  ]
}
```

`trustTier` is `TrustTier.TRUST_TIER_FIRST_PARTY` (= 2). Hook phases are
`HOOK_PHASE_ON_REQUEST` (1), `ON_COMPLETE` (2), `ON_RESPONSE` (3).
The VAM app “Use echo.ping preset” helper sends the same hooks array.
