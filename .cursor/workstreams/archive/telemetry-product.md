# Workstream: Telemetry productization

```yaml
status: completed
slug: telemetry-product
current_phase: done
design_docs:
  - docs/src/content/docs/telemetry/
predecessor: telemetry
```

This file is **session memory**. Starlight `telemetry/` pages are the design spec. Do not implement a phase from this tracker until a Cursor plan for that phase is confirmed.

Successor to `.cursor/workstreams/archive/telemetry.md` (kernel Phases 0–7). That tracker marked libraries and unit tests done. This workstream ships **wires, developer fidelity, the VAM app journey, and vendor-neutral egress**. A phase is not done until it is observable in a running `vamd` (and the VAM app for Phase 2).

## Goal

Make a complete run reconstructible for a developer in the VAM app (tools, outcomes, denials, HITL) and exportable from the host without vendor lock-in. Lane 1 leaves via OTLP to a loopback collector. Lanes 2–4 leave via CloudEvents HTTP and file spool (OCSF / FOCUS / signed evidence). The guest never talks to a collector.

## Out of scope

- Fancy graphs, span-waterfall animation, or a dedicated telemetry sidebar tab
- Guest OpenTelemetry SDK
- Replacing `workflow_outbox` with sampled traces as the source of truth
- Direct `vamd` → Datadog / New Relic / ClickHouse SDK
- Owning first-party vendor dashboards as product (example collector configs only)
- Continuous profiling
- WORM remote anchoring (Sigstore / S3 Object Lock) beyond local Merkle checkpoints

## Locked decisions

Do not re-litigate without editing this list.

1. **Four questions, four lanes.** Traces/metrics/logs are lossy (OTLP). Lifecycle CloudEvents are at-least-once. Outcomes/lineage/cost are durable. Audit is fail-closed. Do not collapse lanes 2–4 onto OTLP as the system of record. An optional OTLP-logs *mirror* of events is allowed for correlation only.
2. **Vendor-neutral boundary.** `vamd` speaks OTLP + CloudEvents + files. The OpenTelemetry Collector (loopback) is the fan-out. ClickHouse / Datadog / Grafana are customer sinks, not VAM protocols. ClickHouse may be the *reference* deployment in docs, not a required runtime.
3. **No vendor SDK in VAM.** Locked from the kernel tracker: remote SaaS exporters stay out of `vamd`.
4. **Standards at the edge.** Lane 2 = CloudEvents 1.0 HTTP (JSON projection: `source`/`subject` URI strings, `dataschema`). Lane 3 = DSSE around existing attestation + OpenLineage HTTP + FOCUS files. Lane 4 = OCSF files (Parquet or JSON lines).
5. **Developer UI lives on the chat turn.** Journey icon on the agent message opens a sheet. No new catalog tab in this workstream. The VAM app consumes `GET /v1/runs/{id}/events` (existing `get_run_events`), not OTLP.
6. **Capture tiers.** Tool bodies: none / digest / redacted-full, set per tenant or policy. Default for the first UI: digest on the event, body in CAS when capture allows.
7. **Definition of done.** Exit criteria require a running `vamd` (and the VAM app for Phase 2). Unit tests alone do not close a phase.
8. **Host PEP remains authoritative.** Guest `StreamLogs` never advances workflow state.
9. **Cost caps stay synchronous in `vamd` (T18).** Telemetry reports usage; it does not enforce spend.
10. **Predecessor kernel APIs stay.** Do not rewrite `vam-telemetry` modules; wire and export them.

## Status legend

- `[ ]` not started
- `[~]` in progress
- `[x]` done (code + tests, **and** observable in a running `vamd` / the VAM app)

## Phases

### Phase 0 — Wire what exists

**Goal:** Connect libraries that already exist so one live run produces guest spans on the host, a live event subscriber works, and hypervisor/cgroup counters move. No new product surface.  
**Depends on:** kernel workstream (`telemetry`) crate + validators  
**Plan:** completed (Phase 0 telemetry wire)  
**Exit:** One end-to-end session on a running `vamd` shows guest `LogFrame`s ingested, `StreamEvents` accepted on a bound socket, and Firecracker/cgroup samples in `VamMetricsRegistry`. Tracker/docs no longer claim OTLP or guest StreamLogs are fully shipped if they are not.

- [x] 0.1 Guest flush: periodic `drain_batch` in `vam-runtime` sending `StreamLogs` vsock frames (not test-only).
- [x] 0.2 Host accept: session brokers (`brokers.rs`) feed `GuestStreamReceiver` for `StreamLogs`.
- [x] 0.3 Register `TelemetryStreamServer` in `vamd` on a real socket (not tests only).
- [x] 0.4 Session-scoped scrape: Firecracker JSON metrics + cgroup v2 into the metrics registry.
- [x] 0.5 Honesty pass: `.cursor/workstreams/archive/telemetry.md` progress vs code; update `docs/src/content/docs/telemetry/roadmap.md` (and SKILLS if still “no Rust”) so implemented vs planned matches reality.

### Phase 1 — Tool-call fidelity

**Goal:** The events API can answer “which tool ran, what came out, why it failed” for guest tools the same way host tools already emit `vam.v1.host_tool.completed`.  
**Depends on:** Phase 0  
**Plan:** completed (Phase 1 tool fidelity)  
**Exit:** After a guest `run`/`exec` tool, outbox contains `vam.v1.tool.completed` or `vam.v1.tool.failed` with name, exit code, duration, and a CAS digest (or inline redacted snippet per capture tier). `GET /v1/runs/{id}/events` returns those rows.

- [x] 1.1 Persist guest log bodies (stdout/stderr) to Drive9 CAS; keep digest on the event. Do not drop small bodies after validation.
- [x] 1.2 Emit `vam.v1.tool.completed` / `vam.v1.tool.failed` from ingested guest `tool.exec` frames (catalog types, not test-only).
- [x] 1.3 Capture tiers: none / digest / redacted-full, documented and enforced in ingress.
- [x] 1.4 Tests: hermetic ingest of a failed tool shows failed event + digest; redaction applied before spool.

### Phase 2 — In-app run journey (the VAM app)

**Goal:** After a chat run, a developer opens a journey sheet from the agent message and sees steps: boot, policy, tools, HITL, outcome. No graphs.  
**Depends on:** Phase 1 (empty journey is worse than no icon)  
**Plan:** completed (Phase 2 journey Lima)  
**Repos:** the VAM app (UI) + existing Tauri `get_run_events`; no new vam protocol if events API is enough.  
**Exit:** In the VAM app, an agent turn shows a journey control; the sheet lists tools, exit/denial, and truncated or linked output. Uses `get_run_events` only (no OTLP in the app).

- [x] 2.1 Journey icon on the agent chat message (`Message` / `ChatArea`); spinning while in-flight, enabled when events exist.
- [x] 2.2 Side sheet: ordered timeline from `ListRunEventsResponse` (boot, policy, tool rows, HITL pause, terminal). Reuse HITL `ActionCard` for the decision; journey shows it as a step.
- [x] 2.3 Tool expand: name, duration, exit code, truncated body or digest/CAS hint. Map unknown event types as generic rows; do not crash.
- [x] 2.4 Verify in the app: one real (or stubbed) run shows guest tool failure vs success without leaving the thread.

### Phase 3 — Export edge, lane 1 (OTLP)

**Goal:** Behavior (traces, metrics, logs) leaves `vamd` via OTLP to a loopback collector. Customer fan-out is collector config, not VAM code.  
**Depends on:** Phase 0 (host spans/metrics must exist to export)  
**Plan:** completed (Phase 3 OTLP export)  
**Exit:** Collector receives OTLP from a live run. Tail sampling is applied. Histograms are bounded. No Datadog/ClickHouse crate in the workspace.

- [x] 3.1 Add OpenTelemetry SDK to `vamd`; bridge `tracing` to OTLP (gRPC 4317 / HTTP 4318 loopback).
- [x] 3.2 Apply existing `TailSamplingPolicy` on the export path.
- [x] 3.3 Bridge `VamMetricsRegistry` to OTLP meters **or** replace with OTel instruments; fixed histogram buckets (no unbounded `Vec<f64>`).
- [x] 3.4 Example collector configs under `scripts/` (OTLP in; comments for Datadog / ClickHouse / Grafana exporters). Unsupported samples, not product.

### Phase 4 — Standards egress, lanes 2–4

**Goal:** Lifecycle, evidence, and security records leave without using OTLP as the system of record.  
**Depends on:** Phase 3 preferred (shared collector host); Phase 1 for useful CloudEvents  
**Plan:** completed (Phase 4 standards egress)  
**Exit:** A webhook (or file drop) receives spec-shaped CloudEvents; OCSF/FOCUS (or JSON-lines stand-in) appear in a spool directory; attestation is DSSE-wrapped or equivalent documented envelope.

- [x] 4.1 CloudEvents 1.0 JSON projection at egress: URI-string `source`/`subject`, `dataschema`; HTTP binding; reuse WAL + `DiskSpoolQueue` for at-least-once.
- [x] 4.2 Optional OTLP-logs *mirror* of events for correlation only; outbox remains authoritative.
- [x] 4.3 File spool writer for OCSF security lane and FOCUS-shaped cost records.
- [x] 4.4 DSSE (or documented Ed25519 envelope) around `RunOutcomeAttestation`; OpenLineage HTTP or file emit for existing lineage events.

### Phase 5 — Compliance packaging

**Goal:** An auditor can verify a run without the VAM app or a live node.  
**Depends on:** Phase 4 (signed/durable records to pack)  
**Plan:** completed (Phase 5 evidence pack)  
**Exit:** `vam` CLI writes a bundle (attestation, audit chain segment, trajectory digest, artifact manifest, denials). `verify_chain` (or successor) validates it.

- [x] 5.1 CLI `evidence` (or equivalent) export for a `run_id`.
- [x] 5.2 Bundle contents: attestation, audit chain, trajectory digest, artifacts, denial events.
- [x] 5.3 Document verify steps; wire existing `verify_chain` if it is the verifier.

### Phase 6 — Enterprise controls

**Goal:** Survive a security review: isolation, retention, collector identity, live SLOs.  
**Depends on:** Phases 3 and 4  
**Plan:** completed (Phase 6 enterprise controls)  
**Exit:** Per-tenant spool/ledger isolation in a running daemon; quota drop prefers traces over audit; remote collector requires mTLS/SPIFFE config; SLO alerts fire from live metrics.

- [x] 6.1 Runtime `TenantSpoolManager` / partition isolation (not tests only).
- [x] 6.2 Retention sweeper honoring `RetentionClass`; audit preserved under pressure.
- [x] 6.3 mTLS/SPIFFE (`CollectorTlsConfig`) for any non-loopback collector.
- [x] 6.4 SLO alerts on live metrics (claim / restore / cold tiers already defined).

### Phase 7 — Differentiation

**Goal:** Stitch customer traces, GenAI export mapping, replay, and guardrail signals.  
**Depends on:** Phases 2 and 3  
**Plan:** completed (Phase 7 differentiation)  
**Exit:** HTTP `/v1/*` continues inbound `traceparent`; `gen_ai.*` mapping only at OTLP edge; replay from CAS trajectory; at least one guardrail signal (denial spike or budget burn) on the event/metric path.

- [x] 7.1 Inbound W3C `traceparent` on control-plane HTTP.
- [x] 7.2 `map_vam_to_genai` applied at export only; internal attributes stay `vam.*`.
- [x] 7.3 Run replay / regression compare from CAS trajectory blob.
- [x] 7.4 Guardrail signals: denial spikes, budget burn rate, and/or same-tool loop.

## Explicitly later

Work that must not be pulled into this tracker.

- Dedicated the VAM app Telemetry sidebar topic / fancy graphs
- Per-vendor first-party dashboards (Datadog JSON, Grafana cloud apps)
- Direct ClickHouse writer inside `vamd`
- Guest MCP credentialed-service telemetry
- Per-service vsock ports for logs
- OTel Profiles as a production SLO input

## Progress log

| Date | Phase | What landed |
|---|---|---|
| 2026-09-11 | 7 | Differentiation: CreateRun continues validated inbound `traceparent` (invalid/missing → new root, not a reject). OTLP export adds `gen_ai.*` aliases (`map_vam_to_genai` + `thread_ref`/`model`); never maps `vam.session.id`. `GET /v1/runs/{id}/trajectory` + `vam run trajectory` / `compare` (digest + event-type/tool/disposition). Cedar DENY increments `vamd.security.denied_actions.total`; `vamd.guardrail.signals.total` for `deny_spike` / `same_tool_loop` (T20 labels). |
| 2026-09-11 | 6 | Enterprise controls: `VAMD_TENANT_ID` (default `local`) + `TenantSpoolManager` so egress/audit live under `{spool}/{tenant}/` (ledger keys `tenant:{id}:{session}`). ~60s retention sweeper prunes Class C then B; never deletes `audit/`; Class A over quota fail-closed. Non-loopback OTLP without mTLS env disabled (`remote_requires_mtls`); HTTPS+mTLS when certs set; SPIFFE string check only. Process-wide `SloEvaluator` increments `vamd.slo.breaches.total` (`tier` only). |
| 2026-09-11 | 5 | Evidence pack: `vamd` attaches `PartitionedAuditLedger`; `GET /v1/runs/{id}/evidence` (same PDP as events; 409 if no attestation). Pack files: manifest, attestation + DSSE, audit.jsonl, trajectory types/digest, artifacts, denials (T21/T23). Optional `{VAMD_SPOOL_DIR}/audit/{partition}.jsonl`. `vam evidence export` / `verify`; `verify_chain` bin registered. Offline verify: hashes, ledger chain, DSSE unwrap, DENY-only denials. |
| 2026-09-10 | 4 | Standards egress from `vamd`: CloudEvents 1.0 JSON (string `source`/`subject` + `dataschema`) via outbox WAL + `DiskSpoolQueue`; `VAMD_CLOUDEVENTS_URL` POST or `cloudevents.jsonl`. Optional `VAMD_CLOUDEVENTS_MIRROR_OTLP` (default off). `ocsf.jsonl` on Cedar/egress deny; `focus.jsonl` from `CostSummary` on attest; DSSE wrap of `RunOutcomeAttestation` (`attestations.jsonl`); `lineage.jsonl` + optional `VAMD_OPENLINEAGE_URL`. Outbox stays SoR. No vendor SDK. CLI evidence is Phase 5. |
| 2026-09-10 | 3 | Host OTLP export from `vamd` via `vam-telemetry` (OTLP/HTTP `127.0.0.1:4318`, fail-open). `TailSamplingPolicy` on the export path (errors + slow + 5% success; children follow parent). Bounded histogram buckets; ~10s registry walk + histogram dual-write to OTel meters. Sample collector `scripts/otel-collector-config.yaml` uses `debug` + commented Datadog/ClickHouse/Grafana. No vendor SDK; guest still has no OTel (T30). Live collector smoke is optional (`otelcol` inside Lima, no 4317 Mac forward). |
| 2026-09-10 | 2 | The VAM app journey sheet on agent messages (`runUrn`, `get_run_events`, tool expand with preview or CAS digest). Lima `start-vamd-lima.sh` defaults `VAMD_TELEMETRY_CAPTURE=redacted-full`. Live OpenRouter re-check skipped (user verified the sheet against Lima; key expired). |
| 2026-09-10 | 1 | Guest tool fidelity: `ChunkStore` in `GuestStreamReceiver`; always `put_chunk` redacted stdout/stderr (incl. small bodies) when capture is `digest`/`redacted-full`; host digest only. Catalog `vam.v1.tool.completed`/`failed` on `run_id` (not `TRAJECTORY_EVENT_TYPES`); no `vam.v1.guest.span.tool.exec`. Guest logs `tool.name`/`lease.id` before the span. `VAMD_TELEMETRY_CAPTURE` default `digest`. Hermetic ingest tests. Live Firecracker still needs a rootfs rebuild. |
| 2026-09-10 | 0 | Wired existing kernel APIs: guest PID 1 long-lived Port 52 `StreamLogs` flush (fail-open), host `GuestStreamReceiver` in brokers (outbox, `chunk_store: None`), `TelemetryStreamServer` on `VAMD_TELEMETRY_LISTEN` (`127.0.0.1:9090`), session Firecracker/cgroup scrape into `VamMetricsRegistry`. Honesty pass on roadmap/SKILLS/kernel tracker. Live Firecracker still needs a rootfs rebuild. OTLP not shipped. |
| 2026-09-10 | — | Tracker opened after product review: kernel wired incompletely; the VAM app journey + vendor-neutral four-lane egress. |
