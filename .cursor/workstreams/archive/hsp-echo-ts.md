# Workstream: TypeScript HSP echo + operator REST

```yaml
status: done
slug: hsp-echo-ts
current_phase: done
design_docs:
  - docs/src/content/docs/hosted/
  - docs/src/content/docs/orchestrator/api.md
```

This file is **session memory**. Starlight `hosted/` and `orchestrator/api` pages are the design/spec home.

## Goal

Prove a non-Rust HSP broker can register into live `vamd` over UDS: ship a TypeScript `echo-ts` broker, add `PUT`/`GET /v1/hosted/brokers` so operators can approve/register without in-process test helpers, automate Lima catalog proof, and (Phase 2) observe guest `echo.ping` plus hooks on the broker console.

## Out of scope

- HITL-style pending broker-request queue
- Cedar advertise for hosted tools, `vam.files`, `vam hosted` CLI
- TCP endpoints in the product REST API
- TypeScript HSP SDK / protoc codegen (runtime proto-loader only in Phase 0)
- Auto-running the guest agent as the proof (`host_tool_once`, operator Invoke REST, `vam run --follow` as the build)

## Locked decisions

1. **REST attach** via `PUT /v1/hosted/brokers` (manifest + Unix path); no TCP in the product API.
2. **Phase 0 Lima proof** = register + catalog GET only (no guest VM invoke).
3. **Phase 0 TS broker** loads `.proto` at runtime (`@grpc/grpc-js` + `@grpc/proto-loader`); no generated TS SDK yet.
4. **No HITL broker-request queue** — unknown brokers stay fail-closed at Register.
5. **Phase 1 catalog UI** lists/inspects/registers via existing REST (no disable API, no guest invoke from app).
6. **Phase 2 observe surface** is the broker console only (`limactl shell vam -- tail -f /tmp/hsp-echo-ts.log`). Operator loads the example agent and sends the prompt.
7. **PUT `manifest.hooks`** lists all three phases (`on_request`, `on_complete`, `on_response`) as optional. Register the broker before creating the session so `hosted-echo-ts` is in the catalog.

## Status legend

- `[ ]` not started
- `[~]` in progress
- `[x]` done (code + tests, or merged)

## Phases

### Phase 0 — TS echo + operator REST + Lima catalog

**Goal:** TypeScript echo broker + `PUT`/`GET /v1/hosted/brokers` + Lima script proving register + catalog.  
**Depends on:** HSP kernel workstream (`hsp`) done  
**Plan:** `.cursor/plans/ts_echo_broker_f2bd9d60.plan.md`  
**Exit:** `npm test` in `examples/hsp-echo-ts`, `cargo test -p vam-orchestrator --test api_tests`, `cargo check --workspace`; Lima script can register `echo-ts` and GET `echo.ping`.

- [x] `examples/hsp-echo-ts` UDS gRPC broker (`echo.ping`, hooks OK) + hermetic Node test
- [x] Proto messages for hosted broker REST (`PutHostedBroker*`, `Get*`, `List*`, `HostedBrokerRecord`)
- [x] Axum `PUT`/`GET /v1/hosted/brokers` (+ get by id); OpenAPI + `orchestrator/api.md`
- [x] Hermetic `api_tests` with Rust `EchoBroker` on temp UDS
- [x] `scripts/lima-hsp-echo-ts.sh` (copy to `/tmp/lima`, register, GET)

### Phase 1 — Operator catalog UI

**Goal:** the VAM app “Hosted brokers” list/inspector calling `PUT`/`GET /v1/hosted/brokers`.  
**Depends on:** Phase 0  
**Plan:** `.cursor/plans/hosted_brokers_catalog_ui_64ec11fe.plan.md`  
**Exit:** Catalog studio can list/enable brokers without curl.

- [x] `HostedBrokersCatalogList` + inspector in the VAM app

### Phase 2 — Broker console + example agent

**Goal:** Instrument echo-ts so every HSP RPC logs the received payload; PUT declares all three hook phases; hermetic `echo-ts` naming; example agent/policy the operator loads in the VAM app or `vam run`.  
**Depends on:** Phase 1  
**Plan:** `.cursor/plans/echo-ts_guest_invoke_d9602d30.plan.md`  
**Exit:** `npm test` in `examples/hsp-echo-ts`, `cargo test -p vam-orchestrator --test hsp_invoke`; Lima broker restarted with PUT hooks; README documents tail + prompt.

- [x] echo-ts `console.log` every HSP RPC (`register`, `listTools`, `invoke`, `onRequest`, `onComplete`, `onResponse`; secret keys only)
- [x] lima PUT + the VAM app echo.ping preset declare all three hooks
- [x] `hsp_invoke.rs` happy path: `broker_id=echo-ts`, `hosted:echo-ts:echo.ping`, skill `hosted-echo-ts`
- [x] example `agent.json` / `policy.cedar` / `run.json` + README tail command
- [x] Lima refresh (copy/rebuild/restart Node, PUT with hooks)

## Explicitly later

- HITL pending broker-request queue
- Generated TypeScript HSP SDK / `packages/hsp-sdk`
- `vam.files` broker, marketplace vsock brokers
- Disable REST, `host_tool_once`, SSH into Firecracker, auto-running the agent

## Progress log

| Date | Phase | What landed |
|---|---|---|
| 2026-09-03 | — | Tracker opened |
| 2026-09-03 | 0 | TS echo package, PUT/GET `/v1/hosted/brokers`, OpenAPI/docs, api_tests, Lima script |
| 2026-09-03 | 0 | Lima live catalog proof: `start-vamd-lima.sh` + `lima-hsp-echo-ts.sh`; PUT/GET verified `echo.ping`; OpenAPI/Scalar include `/v1/hosted/brokers`; script falls back to nodejs.org binary when apt mirrors fail |
| 2026-09-03 | 1 | The VAM app Hosted brokers catalog/inspector: Tauri list/get/put, Sidebar hosted view, register form (`echo.ping` preset), admission error surfacing |
| 2026-09-04 | 2 | Tracker reopened as Phase 2 (broker console + example agent) |
| 2026-09-04 | 2 | echo-ts RPC console logs; PUT/UI hooks 1–3; `hsp_invoke` echo-ts naming; example agent/policy/run; Lima rebuild + PUT verified |
| 2026-09-04 | 2 | Fix immediate run fail: synthetic HSP skill resolve before Drive9, SKILLS.md body, Cedar URN mapping, OpenAI dotted tool sanitization (`echo.ping` -> `echo_ping`), live invoke verified |
