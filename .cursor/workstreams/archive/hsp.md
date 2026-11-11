# Workstream: HSP kernel

```yaml
status: done
slug: hsp
current_phase: done
design_docs:
  - docs/src/content/docs/hosted/
```

This file is **session memory**. Starlight `hosted/` pages are the design spec.

## Goal

Ship the HSP kernel in `vamd`: proto + codegen, gRPC over Unix domain socket, capability manifest (declare / approve / enforce), `hosted:` adapter shim (guest still uses Port 52 `ExecuteHostTool`), lifecycle hooks as gRPC RPCs, and tests.

## Out of scope

- Files broker (S3/local store), RAG, `POST /v1/files`
- The VAM app download UI
- Marketplace Firecracker for third-party brokers

## Locked decisions

1. **Product + platform:** first-party brokers shipped *on* HSP, not *inside* the `vamd` process.
2. **Transport:** gRPC is the protocol. UDS is the pipe. Same messages later over vsock for marketplace VMs. TCP loopback is test/debug only.
3. **Hooks ≠ CloudEvents.** Hooks are synchronous, fail-closed RPCs. CloudEvents (`vam.v1.hsp.hook.*`) are emitted *after* a hook returns.
4. **Cedar names tools, not brokers.** Manifest + registration gate brokers.
5. **Contract neutrality:** proto never names a language, cloud, or product feature.
6. **Secrets:** per-invoke from vault using approved bindings only.
7. **Guest never sees signed download URLs.** That is `on_response` → API client (later workstream).

## Status legend

- `[ ]` not started
- `[~]` in progress
- `[x]` done (code + tests, or merged)

## Phases

### Phase 0 — Freeze the contract

**Goal:** `proto/vam/v1/hosted_services.proto` is the source of truth.  
**Depends on:** none  
**Plan:** `.cursor/plans/hsp_phase_0_proto_0ab77f30.plan.md`  
**Exit:** sketches in docs match committed proto.

- [x] Add `hosted_services.proto` (Register, ListTools, Invoke, OnRequest, OnComplete, OnResponse, CapabilityManifest, HookDisposition)
- [x] Reuse `ToolSpec`, `SideEffectClass`, `SecretRequirement`, `ErrorStatus` — do not fork
- [x] Wire `crates/vam-spec` codegen (`tonic-build`)
- [x] Baseline fixtures under `proto/vam/v1/baseline/`
- [x] `cargo test -p vam-spec`

### Phase 1 — gRPC over UDS

**Goal:** two processes speak HSP. No Cedar, no catalog.  
**Depends on:** Phase 0  
**Plan:** `.cursor/plans/hsp_phase_1_uds_0811f847.plan.md`  
**Exit:** `Register` + `Invoke` over UDS works.

- [x] HSP client in `vamd`
- [x] In-repo echo/stub broker binding a UDS path
- [x] Health / reconnect / fail-closed on broker crash
- [x] Tests: round-trip, socket ACL deny, hang-not-allowed on crash
- [x] TCP `127.0.0.1` only as a test double if needed

### Phase 2 — Capability manifest

**Goal:** VAM controls what a broker may access.  
**Depends on:** Phase 1  
**Plan:** `.cursor/plans/hsp_phase_2_manifest_0a5c01b1.plan.md`  
**Exit:** undeclared brokers cannot register.

- [x] Manifest fields: broker_id, secret bindings, egress domains, hooks, tool exports
- [x] Operator approval record (config or SQLite)
- [x] Reject unapproved / drifted / extra-secret / extra-domain / undeclared-tool
- [x] Hermetic tests for deny matrix

### Phase 3 — ListTools + `hosted:` catalog merge

**Goal:** guest sees HSP tools as normal host tools.  
**Depends on:** Phase 2  
**Plan:** `.cursor/plans/hsp_phase_3_catalog_c1609941.plan.md`  
**Exit:** `ListAdvertisedTools` can include HSP tools.

- [x] `ListTools` → `host_exec.adapter_id = hosted:<broker_id>:<tool_name>`
- [x] Merge into session catalog (`session.rs` path)
- [x] Thin `HostedToolAdapter` in `host_tools.rs` (do not replace Cedar/token path)
- [x] Tests: catalog merge, builtin name collision fail-closed, advertise filter uses tool names

### Phase 4 — Invoke end-to-end

**Goal:** guest authorize → `ExecuteHostTool` → HSP `Invoke` → redact.  
**Depends on:** Phase 3  
**Plan:** `.cursor/plans/hsp_phase_4_invoke_97ce4f23.plan.md`  
**Exit:** guest can call a hosted echo tool.

- [x] Token + replay still required before broker call
- [x] Per-invoke secret injection from approved bindings
- [x] Timeouts / size caps reuse host-tool limits
- [x] Tests: happy path, Cedar deny never dials broker, secret leak redacted, timeout → `tool_error:`

### Phase 5 — Hook RPCs

**Goal:** `vamd` dispatches hooks; brokers can block, inject, or degrade a run.  
**Depends on:** Phase 2 (manifest) and Phase 1 (transport); may run after Phase 4  
**Plan:** `.cursor/plans/hsp_phase_5_hooks_4fe6d838.plan.md`  
**Exit:** hook plane exists. No files ingest, no RAG, no app UI.

- [x] Dispatch OnRequest / OnComplete / OnResponse over the same UDS gRPC
- [x] Manifest: hook participation, required vs optional
- [x] Ordering via `depends_on`, per-hook time budgets
- [x] Stub broker: inject context / FAIL / DEGRADED
- [x] Tests: required FAIL aborts run; optional DEGRADED continues + audit; timeout matrix; parallel independent brokers
- [x] (Optional 5b) Emit CloudEvents `vam.v1.hsp.hook.{completed,degraded,failed}` to the run outbox

### Phase 6 — Trust-tier hardening

**Goal:** fail-closed table is complete for first-party/operator UDS brokers.  
**Depends on:** Phases 4 and 5  
**Plan:** `.cursor/plans/hsp_phase_6_trust_04556251.plan.md`  
**Exit:** HSP kernel is shippable. Files + the VAM app are a *next* workstream.

- [x] Trust-tier field (core / first-party / operator; third-party vsock later)
- [x] Deny: unknown broker, unbound secret, egress not allowlisted, tool not exported
- [x] Assert vsock-required-but-got-UDS (implement vsock later)
- [x] Hermetic orchestrator tests

## Explicitly later

- `vam.files` / `vam.rag` first-party brokers
- `POST /v1/files`, FileRef in the VAM app
- Marketplace: plain Firecracker + drive, signing, distribution registry
- Guest MCP for credentialed services (rejected)

## Progress log

| Date | Phase | What landed |
|---|---|---|
| 2026-09-02 | — | Design docs: `docs/src/content/docs/hosted/*` + sidebar + AGENTS.md map. No proto/code. |
| 2026-09-03 | — | Moved into `.cursor/workstreams/hsp.md`; phased-workstream skill added. |
| 2026-09-03 | 0 | Committed `hosted_services.proto`, `FileRef`/`KnowledgeRef` in `common.proto`, vam-spec codegen + Validate + baseline fixtures; docs updated (runtime still not implemented). |
| 2026-09-03 | 1 | `HspClient` (UDS + test TCP), `hsp-echo-broker`, `hsp_uds` tests; docs updated; Phase 2 next. |
| 2026-09-03 | 2 | SQLite `hsp_broker_approvals`, `HspRegistry` admit gate, `hsp_manifest` deny-matrix tests; docs updated; Phase 3 next. |
| 2026-09-03 | 3 | `ListTools` snapshot (`hsp_broker_tools`), `hosted:` adapter ids, 3-way catalog merge, `HostedToolAdapter` shim, `hsp_catalog` tests; docs updated; Phase 4 next. |
| 2026-09-03 | 4 | Endpoint persistence, async `ExecuteHostTool` → `Invoke` (vault inject/redact/timeout), synthetic `hosted-<broker>` skill seed, `hsp_invoke` tests; docs updated; Phase 5 next. |
| 2026-09-03 | 5 | `HspHookDispatcher` (depends_on waves, required/optional budgets), `admit_hook`, ScriptedHookBroker, create_run OnRequest + terminal OnComplete wiring, outbox `vam.v1.hsp.hook.*`, `hsp_hooks` tests; docs updated; Phase 6 next. |
| 2026-09-03 | 6 | `assert_trust_transport` matrix, unbound-secret prefix, invoke-time egress + SSRF, `hsp_trust` tests; docs honesty; workstream done. Next: new `vam.files` / `vam.rag` workstream. |
