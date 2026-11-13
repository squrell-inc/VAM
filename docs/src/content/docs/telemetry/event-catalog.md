---
title: Event Type Catalog
description: The event types vamd writes to a run's event stream, checked against the code, with payload shapes and capture tiers.
status: shipped
proves:
  - crates/vam-orchestrator/tests/trajectory_timeline_test.rs
  - crates/vam-orchestrator/tests/trajectory_replay_test.rs
  - crates/vam-orchestrator/tests/api_tests.rs::change_my_password_success_writes_one_identity_audit_record_without_secrets
---

To enable predictable consumption of VAM state transitions by dashboards, analytics pipelines, and external webhooks, VAM maintains a strict **Event Type Catalog**. This page documents the events `vamd` writes to a run's event stream, their producers, and how they are ordered. Every row was checked against the constants and builders in `crates/vam-orchestrator/src/events.rs` and the call sites that append them.

---

## 1. Canonical Event Registry

Every event emitted by VAM contains a structured dot-separated type name:

$$\text{vam} \cdot \text{v1} \cdot \langle\text{domain}\rangle \cdot \langle\text{action}\rangle$$

| Event Type | Producer | Triggering Condition | Payload Schema |
|---|---|---|---|
| `vam.v1.inbox.received` | Host Orchestrator | A message was accepted into a run's inbox. | inbox record (seq, mode, run) |
| `vam.v1.inbox.delivered` | Host Orchestrator | A steer was taken in by the task at a delivery point. | inbox record |
| `vam.v1.inbox.discarded` | Host Orchestrator | A stop or cancel dropped a message the task never read. | inbox record (with text) |
| `vam.v1.inbox.started` | Host Orchestrator | A queued message began its task. | inbox record |
| `vam.v1.outbox.message` | Host Orchestrator | The agent posted text with `post_message`. | `{text}` |
| `vam.v1.outbox.card` | Host Orchestrator | The agent showed a jig card with `show`. | `{jig, data, card_id?}` |
| `vam.v1.tool.invoked` | Host Orchestrator (PEP) | Tool accepted into Cedar evaluate. | `ToolInvokedPayload` (digest + optional path; optional CAS + preview) |
| `vam.v1.policy.decided` | Host Orchestrator (PEP) | Cedar disposition for a tool intent. | `PolicyDecidedPayload` |
| `vam.v1.skill.loaded` | Host Orchestrator (PEP) | Authorized `load_skill`. | `SkillLoadedPayload` |
| `vam.v1.host_tool.completed` | Host Orchestrator | Successful `ExecuteHostTool` adapter. | `HostToolCompletedPayload` |
| `vam.v1.hsp.hook.completed` | Host Orchestrator | HSP lifecycle hook returned OK. | hook audit payload |
| `vam.v1.hsp.hook.degraded` | Host Orchestrator | Optional hook timed out or unreachable. | hook audit payload |
| `vam.v1.hsp.hook.failed` | Host Orchestrator | Required hook FAIL / timeout aborted the wave. | hook audit payload |
| `vam.v1.thread.turn` | Host Storage | Successful run appended to volume timeline. | `ThreadTurnPayload` |
| `vam.v1.tool.completed` | Host Orchestrator (guest `StreamLogs` ingest) | Guest `tool.exec` with `exit_code == 0`. Observational; not trajectory-authoritative. | JSON (`tool_name`, `exit_code`, `duration_ns`, `lease_id`, `capture`, optional digests / preview) |
| `vam.v1.tool.failed` | Host Orchestrator (guest `StreamLogs` ingest) | Guest `tool.exec` with `exit_code != 0`. | same as completed |
| `vam.v1.hitl.paused` | Host Daemon | Checkpoint barrier passed; approval published. | `HitlPausedPayload` (digest only) |
| `vam.v1.hitl.decided` | Host Daemon | First verified approval decision committed. | `HitlDecidedPayload` |
| `vam.v1.hitl.expired` | Host Daemon | Approval expired without decision (sweeper). | `HitlExpiredPayload` |
| `vam.v1.step.lease-issued` | Host Daemon | Host issues a fenced bounded task lease. | `WorkflowTransitionPayload` |
| `vam.v1.step.outcome-recorded` | Host Daemon | Host accepts a matching task outcome. | `WorkflowTransitionPayload` |
| `vam.v1.evaluation.recorded` | Host Daemon | Pinned authoritative evaluation is recorded. | `WorkflowTransitionPayload` |
| `vam.v1.inference.completed` | Host Inference | Host broker finishes an LLM call. | `UsageSummary` + model id |
| `vam.v1.context.compacted` | Host Orchestrator | The host recorded a compaction: older turns of a thread are replaced by a summary in the model's context (the log keeps them). Carries no message text. | run, thread, compaction `seq`, kept-from `seq`, tokens before and after, reason (`tokens`, `bytes`, `provider_limit`), model window |
| `vam.v1.workspace.commit_failed` | Host Orchestrator | A turn commit or the end-of-task commit could not be stored. The work is kept on the node, not discarded; the run ends `storage_commit_failed`. Carries no host path and no file names. | workspace, session, run, `kept_id` (when the tree was kept), `phase` (`turn_commit` or `session_end`), `reason`, a short error |
| `vam.v1.egress.decided` | Host Network | A connect or DNS allow or deny at the egress gate. Filed under the session id and, when the session belongs to a run, the run id too (`events.rs` `append_egress_decided`, called from the egress callback in `brokers.rs`), so the run's events list each decision, denies included. | domain, port, disposition, policy id, domain digest |
| `vam.v1.secret.accessed` | Host Vault | A vault read for an invoke binding. | logical name only |
| `vam.v1.web.used` | Host Orchestrator | A web broker tool call finished. Carries no query, snippet, page text or URL path. | broker, tool, host, status, optional code, bytes, sources added |
| `vam.v1.web.citations_unresolved` | Host Orchestrator | The final answer cited `[S<n>]` ids no source answers to. Flag only. | `unresolved` ids, `resolved`, `total_sources` |
| `vam.v1.user.password_changed` | Host Orchestrator | A signed-in user changed their own password (`POST /v1/me/password`, success only). Written to the audit ledger partition `vam_audit_identity`, attributed to the user, not to the run event stream. An admin reset and `vamd --reset-password` do not write it yet. | `{userId, username, role, actor: "self", credential: "session", passwordUpdatedAt}`; never a password, a hash or a session token. Proof: `crates/vam-orchestrator/tests/api_tests.rs::change_my_password_success_writes_one_identity_audit_record_without_secrets` |
| `vam.v1.session.booted` | Host Daemon | A session booted. Filed under the session id and, when a run exists, the run id. Boot path (`manager.rs:1976` passes `cold`). | session id, boot path, snapshot id (none) and a duration that is always 0 (`events.rs:427-436`) |
| `vam.v1.mock.rule_matched` | Host Inference *(test-only)* | The scripted mock model answered a call from a rule. | script, rule index, optional step label |
| `vam.v1.mock.unmatched` | Host Inference *(test-only)* | The scripted mock model had no answer for a call (no rule, tool not offered, script not loaded). | script, reason, last role, preview (80 chars, redacted), optional step |

---

## 2. Event Payload Schemas (JSON Representations)

### `ToolInvokedPayload`

Host PEP trajectory/outbox JSON (not the full proto `arguments` struct). Args are redacted on the host before CAS or outbox (T21).

```json
{
  "tool_name": "write_file",
  "args_digest": "7d865e959b2466918c9863afca942d0fb89d7c9ac0c99bafc3749504ded97730",
  "path": "notes/hello.txt",
  "args_cas": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
  "args_preview": "{\"path\":\"notes/hello.txt\",\"content\":\"…\"}"
}
```

| Field | When present |
|---|---|
| `args_digest` | Always (canonical args hash used for policy). |
| `path` | When the tool carries a workspace path. |
| `args_cas` | `digest` or `redacted-full` capture — SHA-256 of **redacted** args JSON in Drive9 CAS. |
| `args_preview` | `redacted-full` only — truncated redacted args JSON (~4 KiB). |

Journey UI reads these from `GET /v1/runs/{id}/events`. Trajectory replay remains digest-only.

### `InferenceCompletedPayload`

CloudEvent on the run outbox (`vam.v1.inference.completed`). Usage metadata plus optional **outcome** fields (no prompt / message list).

```json
{
  "modelName": "gpt-4o",
  "provider": "openai",
  "promptTokens": 1200,
  "completionTokens": 48,
  "durationMs": 890,
  "outcomePreview": "I'll search the repo for …",
  "toolCallsPreview": "[{\"name\":\"glob\",\"arguments\":\"{\\\"pattern\\\":\\\"**/*.rs\\\"}\"}]",
  "outcomeCas": "7d865e959b2466918c9863afca942d0fb89d7c9ac0c99bafc3749504ded97730"
}
```

| Field | When present |
|---|---|
| `outcomePreview` / `outcomeCas` | Assistant text accumulated from the stream, redacted then CAS/preview per capture tier. |
| `toolCallsPreview` / `toolCallsCas` | JSON array of tool calls from the completion stream (outcome only). |

Outbox rows must use the workflow **run** URN as `run_id`, not the VM session id, so journey queries match.

### `PolicyDecidedPayload`
```json
{
  "tool_name": "query_db",
  "disposition": "DENY",
  "args_digest": "a1b2c3…"
}
```

### `ThreadTurnPayload` (volume timeline)
```json
{
  "prompt": "Write a note",
  "answer": "Done — see notes/hello.txt",
  "trajectory_digest": "e3b0c442…",
  "touched_paths": ["notes/hello.txt"],
  "skill_ids": ["db-tools"],
  "tool_names": ["load_skill", "write_file"]
}
```

### `ToolCompleted` / `ToolFailed` (guest `StreamLogs`)
```json
{
  "tool_name": "run",
  "exit_code": 2,
  "duration_ns": 42000000,
  "lease_id": "lease-1",
  "capture": "digest",
  "stdout_digest": "e3b0c442…",
  "stderr_digest": "7d865e95…"
}
```

Host-computed SHA-256 of **redacted** bytes. Guest-supplied digests are ignored as CAS keys. `GET /v1/runs/{id}/events` returns these rows by `run_id`. They are **not** in the host PEP trajectory set.

### Capture tiers (`VAMD_TELEMETRY_CAPTURE`)

Default is `digest`. Unknown values fall back to `digest`.

| Tier | CAS | Event fields |
|---|---|---|
| `none` | no `put_chunk` | `tool_name`, `exit_code`, `duration_ns` (plus `lease_id` / `capture`) |
| `digest` | redacted body | plus SHA-256 digests |
| `redacted-full` | redacted body | plus a short redacted preview (~4 KiB) |

The same tiers apply to host PEP **tool args** on `vam.v1.tool.invoked` and to **inference outcomes** on `vam.v1.inference.completed`.

### `HitlPausedPayload`
```json
{
  "approval_request_id": "urn:vam:approval:control.vam.dev/acme/request-123",
  "attempt_id": "urn:vam:attempt:control.vam.dev/acme/attempt-123",
  "tool_name": "delete_file",
  "workspace_checkpoint": "sha256:...",
  "reason": "Risk limit triggered: file deletion requested on protected directory"
}
```

---

## 3. Producer Sequence vs. Host Ingestion Sequence

In highly concurrent and distributed serverless environments, events can occasionally be delayed, reordered, or duplicated during transport over virtual sockets. VAM handles sequencing by applying two distinct timestamps and counters to every `VamCloudEvent`:

```text
 ┌────────────────────────────────────────────────────────┐
 │                      GUEST VM                          │
 │                                                        │
 │ 1. Event Generated                                     │
 │    - `time`            = 14:32:00.005Z                 │
 │    - `sequence_number` = 12 (Monotonic Guest Counter)  │
 └───────────────────────────┬────────────────────────────┘
                             │
                             │ (AF_VSOCK Transit / Delay)
                             ▼
 ┌────────────────────────────────────────────────────────┐
 │                        HOST                            │
 │                                                        │
 │ 2. Host Ingestion                                      │
 │    - `ingested_time`     = 14:32:00.120Z               │
 │    - `ingestion_sequence`= 1042 (Host Order Counter)   │
 └───────────────────────────┬────────────────────────────┘
                             │
                             ▼
 ┌────────────────────────────────────────────────────────┐
 │                 Downstream Consumer                    │
 │                                                        │
 │ * Uses `sequence_number` to sort and reconstruct       │
 │   the exact chronological execution path of the VM.    │
 │ * Uses `ingestion_sequence` to track processing backlog│
 │   and detect packet loss over the network link.        │
 └────────────────────────────────────────────────────────┘
```

This dual-sequence pattern ensures that consumer systems can reconstruct the precise timeline of guest actions, even if network delays or host re-queuing alter the delivery order.

---

## 4. Other events and what is not listed

This section lists names only; it documents no payloads.

The run stream also carries `vam.v1.model.resolved` (the model a run got and why), `vam.v1.run.state`, `vam.v1.run.attested`, `vam.v1.lineage.emitted`, `vam.v1.playbook.step`, `vam.v1.playbook.state`, `vam.v1.policy.resolution`, `vam.v1.thread.context_unavailable`, `vam.v1.thread.log_warning` and `vam.v1.vendor_text.flagged`. Their payloads are not documented here; read the call sites named in `events.rs` and `api/mod.rs`.

Guest runtime start and stop, and a workspace-sync event, are not emitted: earlier versions of this page listed `agent.started`, `agent.stopped` and `workspace.synced`, and no code produces them. `vam.v1.session.booted` is emitted (`manager.rs:1750`, `:1976`; `events.rs:406-436`) and is in the table above.

## Next Steps

- See where events go once written (OTLP, CloudEvents, the gRPC stream, the audit ledger) in [Telemetry](/telemetry/overview/).
