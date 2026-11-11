> Moved from the docs site on 2026-11-02: design intent, not built. For what is built, read the Telemetry overview page on the docs site.

---
title: Telemetry Implementation Roadmap
description: What exists today, the four-plane target, companion standards beyond OpenTelemetry, and the phase order for vam-telemetry.
---

:::caution[Alpha Stability Warning]
`vam-telemetry` is a **workspace member** with kernel libraries, unit tests, and a **host OTLP export edge** in `vamd`. Product wiring (guest `StreamLogs` flush, host ingest, bound `StreamEvents` gRPC, Firecracker/cgroup scrape, OTLP to a loopback collector) is tracked in `.cursor/workstreams/archive/telemetry-product.md`. **CLI evidence pack shipped in Phase 5** (`vam evidence export` / `verify`). CloudEvents HTTP and file spool shipped in Phase 4. Pages under `/telemetry/` describe the **target** architecture unless a section is marked implemented.
:::

This page is the implementation home for the observability engine. Kernel checkboxes live in `.cursor/workstreams/archive/telemetry.md` (historical). **Do not treat that tracker as “all product work done.”** Remaining product work (OTLP export and vendor-neutral egress) is `.cursor/workstreams/archive/telemetry-product.md`. **Live Lima + the VAM app journey is the product Phase 2 exit** (rootfs rebuild is no longer a gap).

---

## 1. What Exists Today (do not over-read the docs)

| Surface | Status |
|---|---|
| `crates/vam-telemetry/` | **Workspace member.** Validators, registry, audit, hypervisor/cgroup parsers. Host OTLP export from `vamd` (OTLP/HTTP loopback, tail sampling, bounded histograms). Collector fan-out is config, not a VAM crate. |
| `TraceContext` | On vsock / HSP / CloudEvent RPCs. Validated on guest ingress (T7). |
| `TraceContext` | On vsock / HSP / CloudEvent RPCs. Validated on guest ingress (T7). |
| `StreamLogs` / `LogFrame` | **Wired (product Phases 0–2):** guest flush over Port 52, host `GuestStreamReceiver`, observational `vam.v1.tool.completed` / `failed` on `run_id`, redacted bodies in Drive9 CAS per `VAMD_TELEMETRY_CAPTURE` (default `digest`). Live Lima Firecracker uses a rebuilt rootfs. |
| `workflow_outbox` + `GET /v1/runs/{id}/events` | **Implemented.** Host CloudEvents for PEP, host tools, HITL, HSP hooks, run lifecycle, guest spans/logs. |
| The VAM app journey sheet | **Phase 2 exit.** Agent-message control opens a right sheet from `GET /v1/runs/{id}/events` (`get_run_events`). Tool rows, policy, HITL steps, and truncated logs (or CAS digest hints) show against a live Lima `vamd`. |
| `TelemetryStreamService.StreamEvents` | **Bound** on `VAMD_TELEMETRY_LISTEN` (default `127.0.0.1:9090`). Localhost only; no PDP on this socket yet. |
| Guest `.vam/agent/observations.jsonl` | **Implemented**, observational only. Never advances workflow state. |
| HSP broker logs | Process stdout (e.g. `/tmp/hsp-echo-ts.log`). Not tunneled. |
| Firecracker `PUT /logger` / `PUT /metrics` | Enabled on VMM start. Session scrape records into `VamMetricsRegistry` when the metrics file exists; skipped on macOS/Lima if missing. |
| Token usage in `vam-inference` | Recorded into `SessionBudget` and host outbox. Telemetry does not enforce spend (T18). |
| CloudEvents HTTP + spool | **Phase 4.** JSON projection (string `source`/`subject`, `dataschema`) from the outbox WAL; `POST application/cloudevents+json` when `VAMD_CLOUDEVENTS_URL` is set; else `cloudevents.jsonl`. Fail → `DiskSpoolQueue` retry. |
| OCSF / FOCUS / DSSE / OpenLineage files | **Phase 4 JSONL stand-in** under `VAMD_SPOOL_DIR` (`ocsf.jsonl`, `focus.jsonl`, `attestations.jsonl`, `lineage.jsonl`). Optional `VAMD_OPENLINEAGE_URL`. No Parquet. Optional OTLP-logs *mirror* of event type/run_id/sequence (`VAMD_CLOUDEVENTS_MIRROR_OTLP`, default off). |
| CLI evidence pack | **Phase 5.** `GET /v1/runs/{id}/evidence` plus `vam evidence export` / `verify`. Pack is attestation + DSSE, audit.jsonl, trajectory digest/types, artifacts, denials. Unfinished runs return 409. Offline verify uses `verify_ledger_chain` (and `verify_chain` on `audit.jsonl`). |
| Tenant spool + retention + remote OTLP mTLS + live SLOs | **Phase 6 wired.** `VAMD_TENANT_ID` (default `local`) scopes JSONL/spool/audit under `{spool}/{tenant}/`. Retention sweeper prunes Class C then B; never deletes `audit/`. Non-loopback OTLP requires mTLS env. Process-wide `SloEvaluator` increments `vamd.slo.breaches.total` (`tier` only). |
| Inbound `traceparent` + `gen_ai` export + CAS replay + guardrails | **Phase 7 wired.** HTTP CreateRun continues a valid W3C `traceparent` (invalid/missing → new root, not a 4xx). OTLP export adds `gen_ai.*` aliases and keeps `vam.*`. `GET /v1/runs/{id}/trajectory` plus `vam run trajectory` / `compare` read the CAS replay (types/tool/disposition only). Cedar DENY increments `vamd.security.denied_actions.total`; windowed `deny_spike` and per-run `same_tool_loop` use `vamd.guardrail.signals.total` (`status` only). |

The reconstructible “journey” today is a **lifecycle timeline** by `run_id` + outbox sequence — not an OpenTelemetry trace. The VAM app shows that timeline on the chat turn (product Phase 2). Host control-plane traces and registry metrics can also leave via OTLP to a loopback collector (product Phase 3). CloudEvents HTTP and file spool are Phase 4. CLI evidence bundle (`vam evidence`) is Phase 5. Tenant isolation, retention sweeper, remote mTLS, and live SLO counters are Phase 6. Product Phase 7 stitches inbound `traceparent`, `gen_ai.*` export aliases, CAS trajectory replay/compare, and guardrail counters. The workstream is complete.

---

## 2. Target: four questions, four lanes

OTel traces are sampled and lossy. They cannot be the system of record for “what did the agent do.” `vam-telemetry` is **one normalization core** with four output lanes:

| Question | Data shape | Lane | Standard |
|---|---|---|---|
| How did it **behave**? | Span tree (timing, retries, parallelism) | Lossy, tail-sampled | OpenTelemetry traces + logs |
| What did it **do**? | Decision + effect ledger | At-least-once durable | CloudEvents 1.0 (extends today’s outbox) |
| What did it **achieve**? | Outcome, artifacts, lineage, cost | Durable, signed | Attestations + OpenLineage + FOCUS |
| How did it **end**? | Terminal state + cleanup completeness | Fail-closed | Hash-chained audit ledger |

`workflow_outbox` is the seed of lane 2. Do not replace it with spans.

---

## 3. Four ingest planes

Trust tier selects the ingress. This is more important than the signal taxonomy.

```text
                    +-----------------------------------------+
                    |         Host-local OTel Collector       |
                    |         (localhost:4317 / 4318)         |
                    +--------^----------^----------^----------+
                             |          |          |
              OTLP (trusted) |          | OTLP     | scraped files
                             |          |          |
 +---------------+   +-------+------+   |   +------+-------+
 | 1. Control    |   | 2. Broker    |   |   | 4. Hypervisor|
 |    vamd       +---+    HSP /     |   |   |  Firecracker |
 |  (in-process) |   |    inference |   |   |  logger/metrics
 +-------+-------+   +--------------+   |   +--------------+
         |  inject traceparent          |
         |                              |
         | AF_VSOCK :52  (untrusted)    |
         v                              |
 +---------------+                      |
 | 3. Guest      |  compact protobuf ---+
 |    vam-runtime|  host reconstructs OTel
 |    + tools    |  T7 validate / drop
 +---------------+
```

1. **Control plane** — `vamd`. Direct instrumentation. Full attribute fidelity.
2. **Broker plane** — trusted UDS HSP brokers and the inference broker. They export OTLP **themselves** to the same collector and **continue** context from `vamd`. Do not tunnel broker logs through the daemon.
3. **Guest plane** — untrusted. `StreamLogs` over Port 52. Host is the sole exporter (T3). Third-party marketplace brokers in microVMs use this plane, not plane 2.
4. **Hypervisor plane** — Firecracker log/metrics files scraped on the host. Not guest `StreamLogs`.

---

## 4. Companion standards (beyond OpenTelemetry)

OpenTelemetry is the **diagnostic** export. The other lanes need other contracts:

| System | Role | Phase |
|---|---|---|
| **CloudEvents 1.0** | Durable lifecycle bus (already partial) | 2 |
| **OTel Logs Bridge** | Correlate structured logs without a second pipeline | 1 |
| **Hash-chain + Ed25519** (later Sigstore/Rekor) | Tamper-evident audit | 2, 6 |
| **in-toto / SLSA attestations** | Signed run outcome, composes with `vam-build` signing | 6 |
| **OpenLineage** | Artifact and knowledge provenance graph | 6 |
| **OCSF / ECS** | SIEM-native security events | 7 |
| **FOCUS** | Cost / chargeback records | 6 |
| **Firecracker native metrics** | Hypervisor ingest | 5 |
| **NATS JetStream / Kafka** | Optional CloudEvents fanout | 2 |
| **Tempo+Loki+Mimir or ClickHouse** | Query backend (decide in Phase 1) | 1 |
| **OpenSLO** | Declarative SLOs on documented latency tiers | 7 |
| **OTel Profiles** | Evaluation only (public alpha, OpenTelemetry's profiling agent for Linux) | 7 optional |

**GenAI semantic conventions.** As of 2026 the `gen_ai.*` namespace lives in a dedicated repo, is **Development**, and has no tagged release. Map at the **export edge** only. Internal attributes stay `vam.*`. Do not treat `gen_ai.agent.*` as a frozen database contract.

---

## 5. Correlation spine (already present)

Do not invent a second ID scheme. Propagate W3C `traceparent` **alongside**:

| ID | Owner | Role |
|---|---|---|
| `run_id` | Workflow store | Authoritative run |
| `session_id` | Session manager | Live VM binding |
| `step_id` / `attempt_id` / `lease_id` | Execution lease | Fenced guest work |
| `thread_ref` | Drive9 timeline | Conversation / follow-up |
| `InvokeContext.iteration` | HSP / ReAct | Loop index |
| `Frame.correlation_id` | vsock framing | Per-connection RPC match only — **not** a span ID |

HITL pauses and snapshot restores **link** traces; they do not keep a multi-hour parent span open. `ResumeFromSnapshotRequest` is the re-seed point for entropy, clock, lease generation, **and** trace context.

---

## 6. Phase order

One Cursor plan per phase. Do not implement from this page.

| Phase | Name | Why this order |
|---|---|---|
| **0** | Foundations | Contracts and validators are cheap now, expensive after four planes emit. |
| **1** | Host `vamd` | Highest value per unit effort. No rootfs. Real traces on day one. |
| **2** | Events + audit | Host PEP already observes every tool intent — most of “how it behaved” without the guest. |
| **3** | HSP brokers | Parallel with Phase 2. Different modules. Language-boundary proof. |
| **4** | Guest `StreamLogs` | Richest behavioral data, most expensive (rootfs + hostile-input hardening). Do not pull forward. |
| **5** | Firecracker / net / Drive9 | Attribution of slow runs. |
| **6** | Cost, lineage, attestations | Close the broken `spend_micro` loop; signed “what it achieved.” |
| **7** | SIEM, SLOs, adversarial | Harden after the pipes exist. |

Phase 0–2 produce a useful system. 3–5 complete coverage. 6–7 make it audit-grade.

See `.cursor/workstreams/archive/telemetry-product.md` for Goal / Depends on / Exit / checkboxes (product wires). Kernel tracker `.cursor/workstreams/archive/telemetry.md` is historical. Product Phase 3 shipped host OTLP; do not treat sampled traces as the system of record for agent actions.

---

## Next Steps

- [Telemetry Architecture](/telemetry/architecture/) — four-plane ingest and host-only export.
- [Trace Context Propagation](/telemetry/trace-context/) — W3C carriers, guest validation, HITL links.
- [Event Type Catalog](/telemetry/event-catalog/) — implemented vs planned CloudEvents.
- [Telemetry Invariants](/telemetry/invariants/) — T3, T7, T13, T18, T20, T21, T23.
