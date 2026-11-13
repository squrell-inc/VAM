---
title: Telemetry
description: What vamd records about a run, where it goes (event stream, audit ledger, OTLP, CloudEvents, files), how it is kept, and what is not built.
status: shipped
proves:
  - crates/vam-orchestrator/tests/standards_egress_test.rs
  - crates/vam-orchestrator/tests/telemetry_stream_test.rs
  - crates/vam-orchestrator/tests/audit_fail_closed_test.rs
  - crates/vam-orchestrator/tests/trajectory_timeline_test.rs
  - crates/vam-e2e/tests/usage_recorded.rs
  - crates/vam-orchestrator/tests/evidence_pack_test.rs
  - crates/vam-orchestrator/tests/api_tests.rs::change_my_password_success_writes_one_identity_audit_record_without_secrets
  - crates/vam-orchestrator/tests/api_tests.rs::change_my_password_refusals_write_no_identity_audit_record
---

Telemetry is host-side. `vamd` records events, writes an audit ledger and exports to places you configure. The guest never talks to a collector. The code is in `crates/vam-telemetry` (the library) and `crates/vam-orchestrator` (the wiring in `src/bin/vamd.rs`).

## What is recorded

| Signal | Where it lives | Reader |
|---|---|---|
| Run events (the outbox) | SQLite, one row per event, in `vamd`'s database | `GET /v1/runs/{id}/events`, `vam run events` |
| Trajectory | built from the events and the audit chain | `GET /v1/runs/{id}/trajectory`, `vam run trajectory` |
| Audit ledger | hash-chained records, in memory and as JSONL under `{spool}/{tenant}/audit/` | the evidence pack (`vam evidence export`) |
| Traces | OpenTelemetry spans from `vamd` | an OTLP collector you run |
| Cost | token usage on the run, a cost summary on the signed attestation | the evidence pack, `focus.jsonl` |

The event types are listed in the [event catalog](/telemetry/event-catalog/). On startup `vamd` creates one broadcast bus (`BroadcastEventBus`, `bus.rs:15`), attaches the audit ledger, and starts the standards export worker (`vamd.rs:347-370`). The outbox stays the system of record; every export below is a projection of it (`egress.rs:3`).

A run's events (`GET .../events`) and its evidence pack include the egress decisions of its session: `egress.decided` is filed under the session id and, when a run exists, the run id too, so a denied host shows on the run.

## Where it goes

| Output | Switch | Notes |
|---|---|---|
| OTLP traces | `VAMD_OTLP_ENABLED` (default on), `VAMD_OTLP_ENDPOINT` (default `http://127.0.0.1:4318`), `VAMD_OTLP_TLS_CA`, `_CERT`, `_KEY`, `VAMD_OTLP_SUCCESS_SAMPLE_RATE`, `VAMD_OTLP_SLOW_MS` | Tail sampling keeps every error and every trace over 1000 ms, and 5% of the rest by default (`sampling.rs:14-17`). Secrets are redacted before export. |
| CloudEvents over HTTP | `VAMD_CLOUDEVENTS_URL` | At-least-once. On a failed post the event goes to a disk spool and is retried; the spool is capped at 64 MiB (`egress.rs:29`). Proof: `cloudevents_http_500_then_200_spools_then_drains`. |
| OpenLineage over HTTP | `VAMD_OPENLINEAGE_URL` | Same spool and retry. |
| JSONL files in the spool | `VAMD_SPOOL_DIR` (default `/var/lib/vam/spool`) | `cloudevents.jsonl`, `ocsf.jsonl`, `focus.jsonl`, `lineage.jsonl`, `attestations.jsonl` (`telemetry/src/egress.rs:20-24`). |
| gRPC event stream | `VAMD_TELEMETRY_LISTEN` (default `127.0.0.1:9090`) | Plaintext, no TLS, so keep it on loopback (`daemon.rs:305`). Replays history then follows live; can filter by session id and event type (`telemetry_stream.rs:55-63`). The live feed is a `tokio` broadcast channel of 10,000 events (`bus.rs:3-10`). A subscriber that falls behind, or that has received more than 16 MiB of events in total, is evicted: its stream ends with gRPC `RESOURCE_EXHAUSTED` (`bus.rs:87-103`, `telemetry_stream.rs:147-150`). Skipped events are not replayed on that stream; reconnect with `resume_from_sequence` to replay from the store (`telemetry_stream.rs:60-90`). |
| Timing lines | `VAM_TIMING=1`, `VAM_TIMING_STDERR_JSON=1` | Cold-start phase timings on stderr (`timing.rs:24`). No test covers them. |

The OCSF file holds security findings (policy denials, egress blocks, secret reads, admission rejects, attestation failures, approval decisions; `ocsf.rs`). It is a file, not a feed to a SIEM.

## Audit ledger

Every audited action appends a record whose hash covers the previous record, per partition (`audit.rs:13-49`). A checkpoint signs the chain head with Ed25519 (`audit.rs:164`). Verification re-walks the chain and fails on any edit (`audit.rs:241`). Proofs: `test_audit_ledger_tamper_detection` and `test_audit_ledger_fail_closed_on_tampered_partition` in `audit_fail_closed_test.rs`.

A successful self-service password change writes one `vam.v1.user.password_changed` record to the node-level `vam_audit_identity` partition, attributed to the user and carrying no password, hash or session token (`crates/vam-orchestrator/tests/api_tests.rs::change_my_password_success_writes_one_identity_audit_record_without_secrets`); a refused change writes none (`crates/vam-orchestrator/tests/api_tests.rs::change_my_password_refusals_write_no_identity_audit_record`). Admin resets and `vamd --reset-password` are not audited yet.

Writes of class A (audit) records are checked against a disk quota first. If the quota is full, the record is refused rather than dropping older ones (`evidence.rs:225-245`, `retention.rs:134-160`). The run receipt and the run attestation are built from the ledger; see [Run receipt](/security/run-receipt/).

## Retention and tenant

Four classes, each with a byte cap you can set with `VAMD_SPOOL_MAX_A_BYTES` to `_D_BYTES` (`retention.rs:9-12`). Defaults: A (audit) 5 GiB, B (operational events) 1 GiB, C (traces) 512 MiB, D (metrics) 256 MiB (`retention.rs:71-74`). Class A is never age-deleted or evicted; lossy classes are pruned first.

`VAMD_TENANT_ID` (default `local`) is the partition key for the spool and audit paths. It is a telemetry label, not a user account, and a value with path parts such as `../` is rejected (`tenant.rs:59`).

## What gets redacted

Before an event is spooled or exported, `DefaultRedactor` rewrites text matching these patterns (`redact.rs:14-110`): `Bearer <token>` (10+ characters), `vam_sk_` keys, `sk-` keys (20+ characters), AWS access keys (`AKIA` plus 16), `password`, `secret`, `token`, `api_key` and `apikey` followed by `:` or `=` and a value, US social security numbers (`123-45-6789`), 16-digit card numbers, and email addresses. In JSON, any value whose key contains `password`, `secret`, `token`, `api_key`, `apikey`, `authorization` or `credential` is redacted (`redact.rs:60-70,124`). Nothing else is redacted: names, phone numbers and postal addresses pass through. Tool arguments and inference output are stored as digests by default; `VAMD_TELEMETRY_CAPTURE` chooses `none`, `digest` or `redacted-full` (see the event catalog). Pattern matching is not a guarantee that no secret text reaches an export.

## Cost

Token usage is recorded on each run as `vam.v1.inference.completed` events. At finish, `vamd` builds a cost summary (spend in micro-units, tokens, price catalog version, for example `2026-v3`, `vam-inference/src/catalog.rs:380`) and puts it on the signed outcome attestation. It also appends one FOCUS-shaped line to `focus.jsonl`. That line is a stand-in: it is not a billed FOCUS exporter and writes no Parquet. Budget caps are enforced by the orchestrator, not by telemetry. Proof: `usage_recorded.rs`.

## Not built

- No telemetry from the guest to a collector. The guest streams log frames to the host over vsock, and `vamd` validates, rate-limits and redacts them (`guest_ingress.rs`). Only the host exports.
- No SIEM connector or Splunk, Datadog or Elastic mapping. OCSF lines go to a file.
- No Prometheus or `/metrics` endpoint. Metrics are named in `crates/vam-telemetry/src/metrics.rs` and exported through OTLP only.
- No billed FOCUS export, no Parquet.
- No TLS on the gRPC stream.
- No per-user telemetry quota.

Earlier pages for architecture, OpenTelemetry, trace context, CloudEvents, streaming, reliability, metrics, cost, latency, audit ledger, privacy, retention, errors and invariants described a larger design. They were folded into this page; the unbuilt parts are listed under "Not built" above.
