# SKILLS.md — Telemetry & Observability Invariants

This file is a developer-facing contract. Every contributor working on `crates/vam-telemetry/` must respect these invariants.

---

## 1. Trust & Network Boundaries

* **Host-Only Export (Decision T3)**: Untrusted guest instances must never carry exporter API tokens or establish direct connections to external telemetry collectors. All signals (traces, metrics, logs, events) must traverse AF_VSOCK port 52 to the host for normalization and export.
* **Trace Context Validation (Decision T7)**: Incoming W3C `traceparent` or `tracestate` headers from guest runtimes are untrusted input. Validate formats (via regex), strip unapproved baggage, and never use trace headers for authentication or authorization decisions.

## 2. Privacy & Data Minimization

* **Redaction-Before-Export (Decision T21)**: Sensitive customer data, credit card numbers, passwords, and API credentials must be redacted immediately upon ingestion by the host. No un-redacted plaintext data must ever be written to local spool disks or exported to remote collectors.
* **Audit Minimization (Decision T23)**: Do not include raw prompts, tool arguments, or file contents in audit logs by default. Use SHA-256 cryptographic hashes and reference pointers.

## 3. Host Node Stability

* **Low-Cardinality Metrics (Decision T20)**: Do not use session IDs, user IDs, raw prompts, file paths, domains, or network IP addresses as OpenTelemetry metric labels/tags. Use Trace IDs and exemplars to handle correlation safely.
* **Slow-Consumer Disconnection (Decision T13)**: The streaming event channels must cap unread frames per client to `16MB`. If a subscriber falls behind, they must be forcefully disconnected to free host RAM.
* **Cost Enforcements (Decision T18)**: Do not rely on asynchronous telemetry signals to prevent run-away billing. Hard budget caps and monetary reservations must be processed synchronously inside the Orchestrator before executing VM actions.
* **No Guest OTel SDK (Decision T30)**: `vam-runtime` emits compact protobuf over vsock. It must not link the OpenTelemetry SDK or hold collector credentials.
* **Content in CAS (Decision T31)**: Prompts, tool args, and stdout bodies are Drive9 CAS blobs referenced by digest. They do not ride spans or default audit records.
* **Capture tiers**: `VAMD_TELEMETRY_CAPTURE` is `none` / `digest` (default) / `redacted-full`. Host computes the digest on **redacted** bytes. Unknown env values fall back to `digest`. Guest `tool.exec` becomes `vam.v1.tool.completed` or `vam.v1.tool.failed` on `run_id`; do not add those types to the host PEP trajectory set.
* **Four lanes stay distinct**: Traces are lossy (OTLP). CloudEvents are at-least-once. Outcomes/lineage/cost are durable. Audit is fail-closed. Do not make sampled spans the system of record for agent actions. An optional OTLP-logs *mirror* (`VAMD_CLOUDEVENTS_MIRROR_OTLP`, default off) may carry `event_type` / `run_id` / `sequence` only.
* **Standards egress (lanes 2–4)**: CloudEvents 1.0 JSON (string `source`/`subject` + `dataschema`) over HTTP or `cloudevents.jsonl`. OCSF / FOCUS / OpenLineage / DSSE JSONL under `VAMD_SPOOL_DIR`. Outbox remains SoR. Redact before spool (T21). No vendor SDKs. No OTel in `vam-runtime` (T30).
* **Evidence pack (Phase 5)**: `GET /v1/runs/{id}/evidence` and `vam evidence export` / `verify`. Pack files use digests, not raw prompts or tool args (T21/T23). Export is owner-scoped (cross-user 404). Verify still accepts partition keys `tenant:{id}:{session}` — `user_id` is a record attribute, not a key rewrite. `vam-cli` may use this crate for verify helpers only — do not call `init_telemetry` from the CLI.
* **Host OTLP export**: `vamd` uses the OpenTelemetry SDK in this crate (OTLP/HTTP loopback, fail-open). Guest `vam-runtime` still has no SDK (T30). Do not add vendor SDKs (Datadog, ClickHouse) to VAM crates. Collector fan-out is `scripts/otel-collector-config.yaml`. OTLP is not the system of record for actions.
* **Tenant spool path**: `VAMD_TENANT_ID` (default `local`, `TenantId` validation — reject `../`). Standards egress and audit JSONL live under `{VAMD_SPOOL_DIR}/{tenant}/` (audit under `{tenant}/audit/`). Ledger keys are `tenant:{id}:{session}`.
* **Host-stamped `vam.user.id`**: Stamp from the run's `owner_user_id` on traces, events, approved baggage, and audit/evidence **record attributes**. Strip guest-asserted `vam.user.id`. Do not put `user_id` in the audit partition key or spool path. SessionBudget may tag spend with `user_id` for attribution only — no per-user quota.
* **Remote OTLP requires TLS**: Non-loopback `VAMD_OTLP_ENDPOINT` without `VAMD_OTLP_TLS_CA` / `CERT` / `KEY` disables export (`otlp_disabled_reason=remote_requires_mtls`). Loopback HTTP is unchanged. `VAMD_OTLP_SPIFFE_ID` is a string check only (no SPIFFE SDK). Guest never talks to a collector (T3).
* **Live SLOs**: Process-wide `SloEvaluator`. Breaches increment `vamd.slo.breaches.total` with label `tier` only (`claim`|`restore`|`cold`). No `session_id` on the metric (T20).
* **Inbound HTTP `traceparent`**: Control-plane writes (CreateRun) may continue a validated W3C parent. Invalid/missing starts a new root and must not 4xx. Trace context is never auth (T7).
* **`gen_ai.*` at export only**: `map_vam_to_genai` runs on OTLP span export. Internal fields stay `vam.*`. Never map `vam.session.id`. Pin `OTEL_GENAI_SEMCONV_VERSION = development-2026`.
* **Trajectory replay**: `GET /v1/runs/{id}/trajectory` and `vam run trajectory` / `compare` read the host CAS blob. Event type / tool / disposition only. `TRAJECTORY_EVENT_TYPES` stay the host PEP set.
* **Guardrail signals**: Cedar DENY increments `vamd.security.denied_actions.total` (`status=denied`). Windowed spike and same-tool loop use `vamd.guardrail.signals.total` (`status` only). Tool name stays on logs (T20). Do not enforce spend (T18).
