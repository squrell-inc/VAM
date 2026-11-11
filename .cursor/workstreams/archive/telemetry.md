# Workstream: Telemetry kernel

```yaml
status: completed
slug: telemetry
current_phase: 7
design_docs:
  - docs/src/content/docs/telemetry/
```

This file is **session memory**. Starlight `telemetry/` pages are the design spec. Do not implement a phase from this tracker until a Cursor plan for that phase is confirmed.

**Successor:** product wires, the VAM app journey, and vendor-neutral egress live in `.cursor/workstreams/archive/telemetry-product.md`. This kernel tracker is historical — do not silently un-check completed `[x]` rows here.

## Goal

Ship `vam-telemetry` as the host-centric logging and monitoring engine for `vamd`, connected HSP brokers, Firecracker, and guest `vam-runtime`. A complete run is reconstructible as four complementary answers: how the agent behaved (OTel traces), what it did (durable CloudEvents), what it achieved (signed outcome + lineage + cost), and how it ended (fail-closed audit). W3C `traceparent` is propagated on HTTP, vsock, and HSP hops. Guest telemetry never exports OTLP.

## Out of scope

- The VAM app telemetry UI / waterfall (moved to successor `.cursor/workstreams/archive/telemetry-product.md`)
- Guest OpenTelemetry SDK (guest emits compact protobuf over vsock only)
- Replacing `workflow_outbox` with sampled traces as the source of truth
- Continuous profiling as a required deliverable (OTel Profiles is alpha; evaluation only)
- First-party files/RAG product brokers (separate HSP workstreams)
- Remote vendor SaaS exporters from `vamd` (loopback collector only)

## Locked decisions

Do not re-litigate without editing this list.

1. **Four questions, four lanes.** Traces/metrics are lossy. Lifecycle CloudEvents are at-least-once. Outcomes/lineage/cost are durable. Audit is fail-closed. Do not collapse “what did it do” onto sampled spans.
2. **Four ingest planes, trust-tiered.** Control plane (`vamd`) is in-process. Trusted UDS HSP brokers export OTLP to the host-local collector and continue context from `vamd`. Guest and third-party VM brokers use the untrusted vsock ingress. Firecracker logs/metrics are a host scraper, not guest `StreamLogs`.
3. **No OTel SDK in the guest.** Compact protobuf over existing vsock framing (`StreamLogs` / `LogFrame`). Host reconstructs OTel spans. Guest never holds exporter credentials (T3).
4. **Link, don’t nest, across pauses.** HITL and snapshot restore close the pre-pause span and join the resumed segment with OTel span links. Each run is its own trace; threads link via `thread_ref` (`gen_ai.conversation.id`).
5. **Content in CAS; digests on the wire.** Prompts, tool args, and stdout bodies do not ride spans. Drive9 CAS blobs are referenced by SHA-256 (T21/T23).
6. **`gen_ai.*` at the export edge only.** Internal model stays VAM-native (`vam.*`). GenAI semantic conventions are Development and will churn; pin a version and isolate the mapping. Build cost/latency dashboards on stable client metrics, not agent span attributes.
7. **Trace context is never authorization.** `ExecutionLease` stays authoritative. Do not overload `Frame.correlation_id` as a span ID.
8. **Cost caps stay synchronous in `vamd` (T18).** Telemetry reports usage; it does not enforce spend.
9. **Host PEP events remain authoritative.** Guest `StreamLogs` and `.vam/agent/observations.jsonl` never advance workflow state.
10. **Query backend is a Phase 1 decision.** Pick Tempo+Loki+Mimir *or* a columnar store (ClickHouse) before instrumenting `vamd`; Jaeger-only boxes later joins.

## Status legend

- `[ ]` not started
- `[~]` in progress
- `[x]` done (code + tests, or merged)

## Phases

### Phase 0 — Foundations: crate, contracts, correlation spine

**Goal:** Scaffold `vam-telemetry` and freeze the correlation / validation / label contracts. No exporters, no behavior change in `vamd`.  
**Depends on:** none  
**Plan:** not drafted  
**Exit:** `cargo check --workspace` clean; contract tests reject invalid `traceparent`, oversize baggage, and high-cardinality labels. Nothing exported.

- [x] 0.1 Add `vam-telemetry` to `[workspace].members`; implement `VamTelemetryError` from [Error Taxonomy](/telemetry/errors/). Lift the “No Rust Code” line in `crates/vam-telemetry/SKILLS.md` in the same change.
- [x] 0.2 Wire existing `TraceContext` (`proto/vam/v1/telemetry.proto`) into `VsockEnvelope`, HSP `InvokeRequest` / hook requests, and `VamCloudEvent` as an additive extension.
- [x] 0.3 Write the semantic-convention contract: pinned semconv version, `gen_ai.*` mapping table, `vam.*` namespace, low-cardinality allowlist **enforced in code**.
- [x] 0.4 Redaction trait + `vam-security` integration, unit-tested against secret fixtures. No I/O.
- [x] 0.5 Traceparent / baggage validator: W3C regex, length caps, 5-key / 64-byte allowlist (T7).

### Phase 1 — Host control plane: make `vamd` observable

**Goal:** One run produces a coherent host-only trace in a local collector. Zero guest work.  
**Depends on:** Phase 0  
**Plan:** completed  
**Exit:** `POST /v1/runs` → session boot → lease → dispatch → finalize appears as one trace in the chosen backend. Collector-down does not fail the run.

- [x] 1.1 Replace orchestrator `eprintln!` sites with structured `tracing`; initialize one subscriber in the `vamd` binary.
- [x] 1.2 Run-lifecycle span tree over `run_lifecycle` / `finalize_run_session` (`create` → `boot` claim/restore/cold → `lease` → `dispatch` → `finalize` → `snapshot`).
- [x] 1.3 Fold `PhaseTimer` (`VAM_TIMING=1`) into real spans; delete the parallel stderr JSON path once spans cover the same steps.
- [x] 1.4 Metrics registry for documented `vamd.*` names with the Phase 0 allowlist enforced.
- [x] 1.5 OTLP export to loopback collector + reference config under `scripts/`. Tail-sample: keep errors and slow traces; sample successes hard. Decide query backend (see locked decision 10).
- [x] 1.6 Multi-lane bounded queues: lossy ring buffer for traces/metrics; disk-spool skeleton for events.

### Phase 2 — Durable event and audit lanes

**Goal:** Promote `workflow_outbox` into a real bus and start the fail-closed audit ledger. This is “what did it do.”  
**Depends on:** Phase 0 (Phase 1 preferred for shared subscriber, not strictly required)  
**Plan:** complete
**Exit:** `StreamEvents` survives a collector outage; a verifier detects ledger tampering; fail-closed on audit-append failure has a test.

- [x] 2.1 Publisher over `workflow_outbox` + `tokio::broadcast` (10k cap) with 16MB slow-consumer eviction (T13).
- [x] 2.2 Implement `TelemetryStreamService.StreamEvents` with cursor replay from WAL ([streaming](/telemetry/streaming/)).
- [x] 2.3 Close host event-catalog gaps **before** guest work: inference calls, egress decisions, secret access, snapshot/restore, session boot path. Host PEP already sees every tool intent.
- [x] 2.4 Partitioned hash-chain audit ledger, Ed25519 checkpoints via `vam-security`, fail-closed wired to session termination, plus a chain verification tool.
- [x] 2.5 Retention / quota enforcement; spool eviction preserves audit over traces.

### Phase 3 — HSP broker plane

**Goal:** A single trace spans client → `vamd` → broker → back, across process and language.  
**Depends on:** Phase 0.2 (context on HSP messages). May run in parallel with Phase 2.  
**Plan:** completed  
**Exit:** Rust `hsp-echo-broker` and TS `hsp-echo-ts` both continue `traceparent`. Broker stdout is no longer the observe surface.

- [x] 3.1 Propagate `TraceContext` on `Invoke`, `OnRequest`, `OnComplete`, `OnResponse`. `vamd` injects; broker continues.
- [x] 3.2 Instrument both reference brokers: `hsp-echo-broker` (Rust) and `hsp-echo-ts` (Node OTel SDK).
- [x] 3.3 Broker SLIs in `vamd`: invoke latency, hook-wave duration, timeout/degrade/fail counters. `broker_id` is an explicit T20 exception (bounded cardinality).
- [x] 3.4 Trusted UDS brokers export OTLP **directly** to the host collector. Add an HSP telemetry RPC only for brokers that cannot, and for VM-isolated third-party brokers later.
- [x] 3.5 Trust-tier routing: third-party / vsock brokers use the untrusted ingress path.

### Phase 4 — Guest plane: runtime and agent behavior

**Goal:** Guest spans nest under the host run span without a guest SDK. Most expensive phase (rootfs rebuild).  
**Depends on:** Phases 0, 1, and 2.3 (host event coverage first).  
**Plan:** completed  
**Exit:** guest spans nest under the host run; forged-`traceparent` is dropped; guest telemetry cannot influence Cedar; hard VM kill loses only the in-memory buffer.

- [x] 4.1 Implement `StreamLogs` host server + guest client: framing, 5000-event buffer, dual guest/host sequence numbers.
- [x] 4.2 Guest emitter in `vam-runtime` (compact protobuf, no SDK): init/mounts, Drive9, each ReAct iteration, inference wait, tool exec, PEP round-trips, egress. Monotonic durations + clock-domain labels.
- [x] 4.3 Host ingress hardening: Phase 0 validator, attribute caps, per-session rate limit, drop rather than crash.
- [x] 4.4 Inject `OTEL_TRACEPARENT` into tool subprocess env in `exec.rs`.
- [x] Rebuild guest via `./scripts/build-rootfs.sh` (or Lima) and prove the path in an e2e run.

### Phase 5 — Hypervisor and infrastructure

**Goal:** Attribute a slow run to hypervisor / IO / network vs LLM vs guest tool.  
**Depends on:** Phase 1  
**Plan:** completed  
**Exit:** Firecracker logger/metrics and `vam_network_*` counters appear as children of the session span.

- [x] 5.1 Enable Firecracker `PUT /logger` and `PUT /metrics` from `vamd` (client already exists in `vam-core`/`vam-firecracker`).
- [x] 5.2 Scrape Firecracker JSON metrics (vcpu, block, net, vsock) into OTel metrics under the session span.
- [x] 5.3 Host resource attribution: cgroup v2 / CPU pin stats per session.
- [x] 5.4 Export documented `vam_network_*` counters; route DNS and deny logs to the security lane.
- [x] 5.5 Drive9: CAS chunking, sync duration, materialization cache hit rate.

### Phase 6 — Outcome, cost, lineage, provenance

**Goal:** Offline, with APM off, answer “what did it achieve” and “how did it end” from durable records.  
**Depends on:** Phases 1 and 2  
**Plan:** completed  
**Exit:** `DispatchOutcome.spend_micro` is populated from `SessionBudget`; a signed run-outcome attestation exists; OpenLineage-shaped events cover Drive9 / knowledge refs.

- [x] 6.1 Close the cost loop: `vam-inference` usage → pricing catalog → `UsageSummary` with price provenance. Remove the hardcoded $0.50 `SessionBudget` and the `spend_micro: None` construction sites. Caps stay synchronous (T18).
- [x] 6.2 Signed run-outcome attestation: goal, terminal state, terminator, trajectory digest, artifacts, paths, skills, tools, eval, cost.
- [x] 6.3 OpenLineage-shaped events for Drive9 artifacts and knowledge refs.
- [x] 6.4 Record pinned evaluator results (`vam.v1.evaluation.recorded`).
- [x] 6.5 Trajectory replay blob in CAS, referenced by digest from the span and the event.

### Phase 7 — Security monitoring, SLOs, hardening

**Goal:** SIEM-ready security lane, documented SLOs, and an adversarial suite.  
**Depends on:** Phases 2, 4, and 6  
**Plan:** completed  
**Exit:** Cedar / egress / vault / HITL / admission events map to OCSF (or ECS); SLO alerts exist for claim/restore/cold; adversarial tests pass.

- [x] 7.1 Map the security lane to OCSF (or ECS) for SIEM: Cedar denials, egress blocks, secret access, HITL, admission rejects, attestation failures.
- [x] 7.2 SLOs and alerts on documented latency tiers (claim <10ms, restore <50ms, cold <250ms) plus capacity metrics.
- [x] 7.3 Multi-tenancy: per-tenant isolation of spool/ledger/streams, retention classes, mTLS/SPIFFE for any remote collector.
- [x] 7.4 Adversarial suite: collector down, disk full, malicious guest flood, forged context, slow consumer, clock skew, snapshot-restore continuity.
- [x] 7.5 Optional / evaluation only: OTel Profiles (public alpha, eBPF/Linux, Collector ≥0.148 behind a feature gate). Not on the critical path.

## Explicitly later

Work that must not be pulled into this tracker.

- The VAM app unified telemetry UI (new workstream after Phase 2.2 `StreamEvents`)
- Guest MCP credentialed-service telemetry (rejected architecture)
- Per-service vsock ports for logs
- Direct `vamd` → Datadog/New Relic without a local collector
- WORM remote anchoring (Sigstore / S3 Object Lock) beyond a local Merkle checkpoint — design exists; ship after 2.4 is proven
- Continuous profiling as a production SLO input

## Progress log

| Date | Phase | What landed |
|---|---|---|
| 2026-09-07 | — | Tracker opened. Design home: `docs/src/content/docs/telemetry/` (roadmap + four-plane architecture). No Rust. |
| 2026-09-07 | 0 | Phase 0 foundations landed: `crates/vam-telemetry` scaffolded with `VamTelemetryError`, `trace_context` wired into protobuf RPCs, W3C traceparent/baggage validator, semconv & low-cardinality label validator, zero-I/O redactor. `cargo check --workspace` clean. |
| 2026-09-07 | 1 | Phase 1 host control plane landed: `vamd` structured tracing subscriber, run-lifecycle span tree, PhaseTimer folded into spans, VamMetricsRegistry with low-cardinality validation, multi-lane bounded queues (LossyRingBuffer & DiskSpoolQueue), tail-sampling policy, and reference OTel collector topology. |
| 2026-09-07 | 2 | Phase 2 durable event and audit lanes landed: `workflow_outbox` broadcast bus with slow-consumer lag eviction, `TelemetryStreamService.StreamEvents` cursor replay + live streaming, closed host event gaps, partitioned hash-chain audit ledger with Ed25519 checkpoints and fail-closed termination, retention/spool eviction. |
| 2026-09-07 | 3 | Phase 3 HSP broker plane landed: `TraceContext` propagation on `Invoke`, `OnRequest`, `OnComplete`, `OnResponse`; reference brokers instrumented in Rust (`hsp-echo-broker`) and TypeScript (`hsp-echo-ts`); broker SLI metrics (`invoke.duration_ms`, `hook_wave.duration_ms`, error counters); direct OTLP export for trusted UDS brokers and trust-tier routing isolation. |
| 2026-09-07 | 4 | Phase 4 guest plane landed: `StreamLogs` vsock RPC contracts with dual sequence numbers; guest emitter in `vam-runtime` (compact protobuf, zero OTel SDK, 5000-event circular buffer, monotonic durations, `guest_domain` clock domain); span emissions across init/mounts, Drive9 sync, ReAct iterations, inference, tool execution, PEP authorization/execution, and egress; `OTEL_TRACEPARENT`/`OTEL_TRACESTATE` child subprocess environment injection; host-side ingress hardening in `vam-telemetry` and `vam-orchestrator` (`GuestIngressValidator`, `GuestStreamReceiver`) dropping forged traceparents, capping attribute cardinality, applying token-bucket rate limits, redacting secrets, and spilling >4 KiB payloads to CAS ChunkStore. |
| 2026-09-07 | 5 | Phase 5 hypervisor and infrastructure landed: Firecracker `PUT /logger` & `PUT /metrics` activation with FIFO pipes in `vam-core` and `vam-orchestrator`; JSON metrics scraping for vCPU exits, block I/O bytes, net rx/tx, and vsock rx/tx in `vam-telemetry::hypervisor`; cgroup v2 host resource parsing (`cpu.stat`, `memory.current`, `memory.peak`) in `vam-telemetry::cgroup`; network metrics (`vam_network_*`) and security lane logging for DNS/CIDR block events in `vam-network`; Drive9 CAS chunking (`chunk_count`, `chunk_bytes`) and materialization cache hit/miss accounting in `vam-storage` with low-cardinality label validation. |
| 2026-09-08 | 6 | Phase 6 outcome, cost, lineage, and provenance landed: closed cost loop with `vam-inference` usage tracking, price provenance (`PriceProvenance`, `effective_date`, `catalog_version`), dynamic `SessionBudget` allocation from `RunSpec` (eliminating hardcoded $0.50 budget and all `spend_micro: None` construction sites) while maintaining synchronous budget enforcement (Decision T18); Ed25519-signed `RunOutcomeAttestation` envelope (`vam.v1.run.attested` outbox event and `vam_audit_attestation` partitioned audit ledger); OpenLineage-compatible provenance events (`vam.v1.lineage.emitted`) capturing Drive9 input/output datasets and knowledge refs; authoritative evaluator recording (`vam.v1.evaluation.recorded`) publishing to the event bus and appending to the partitioned audit ledger (`vam_audit_evaluation`); and canonical CAS trajectory replay blob persistence (`persist_trajectory_replay_blob`) annotated with content address digests. |
| 2026-09-08 | 7 | Phase 7 security monitoring, SLOs, and hardening landed: OCSF v1.2.0 schema mappings (`vam_telemetry::ocsf`) for Cedar authorization decisions (Class 1007), network egress blocks (Class 4001), DNS query blocks (Class 4003), secret accesses (Class 1001), admission rejections, attestation failures, and HITL decisions (Class 1002); documented SLO latency tiers (`SloTier::Claim` <10ms, `SloTier::Restore` <50ms, `SloTier::Cold` <250ms) with `SloEvaluator`, adherence metrics, alert emission, and `CapacityMetrics` tracking in `vam-orchestrator::manager`; multi-tenant data isolation (`TenantId`, `TenantContext`, `TenantSpoolManager`) preventing directory traversal and separating audit partitions (`record_tenant_audit_event`), with mTLS/SPIFFE remote collector validation (`CollectorTlsConfig`); comprehensive adversarial suite (`tests/adversarial_telemetry_test.rs`) verifying collector outage spooling, quota eviction preserving Class A audit, malicious guest log flooding (50k EPS), forged traceparent sanitization, slow consumer lag eviction (>16MB), clock skew monotonic duration invariance, and cryptographic audit hash-chain tamper detection. |


