> Moved from the docs site on 2026-10-31: design intent, not built (no files or RAG broker; no triggered runs).

---
title: Broker-Triggered Runs
description: Design for scheduled and event-driven agent runs — brokers own the schedule and ask, vamd verifies a host-issued run grant and starts the run.
---

:::caution[Contract defined, build in progress]
The protos, validation and approval gate on this page are in `main`'s spec. The grant issuance, the gateway socket and the shared run start land in later E5 Phase 1 waves. Until then no `vamd` issues a grant or serves a gateway. Design: `design/hsp-triggers.md`, amendment 2026-10-29.
:::

## The idea in one sentence

**Brokers own the schedule and ask `vamd` to start a run. Only `vamd` can actually start it, and only as the user who created the schedule.**

A broker watches a clock, a Slack channel, or a long-running job. It stores its own schedules and runs its own tickers. When something fires, it sends a short request to `vamd` carrying a **run grant** — an opaque bearer token `vamd` issued (like an API key) when the user's agent created the schedule. `vamd` looks up the grant record, verifies it, checks policy, dedupe, and capacity, and starts the run the same way it would for a normal API call.

The broker never boots a microVM or creates a session on its own, and it never tells `vamd` who the user is.

---

## Why this exists

HSP today is mostly **outbound from `vamd`**:

- `vamd` calls the broker for tools (`Invoke`) and lifecycle hooks (`OnRequest`, …).
- Runs start only when a user or app calls `POST /v1/runs`.

That works for interactive use. It does not cover:

| Need | What has to watch and wake up |
|---|---|
| **Scheduler** — “run this every day at 6 PM” | A clock |
| **Slack / Jira** — “run when someone mentions the bot” | An incoming webhook or poll loop |
| **Long-running job** — “run the next step when the export finishes” | A callback or queue consumer |

Those watchers need credentials, vendor SDKs, and their own release cycle. That is exactly what HSP brokers are for. But they still need a safe way to **start** a run without bypassing Cedar, budget, or admission control.

---

## Examples

### Example 1 — Daily scheduler (6 PM)

**User says (in chat):** “Summarize my Jira board every day at 6 PM.”

**Flow:**

```text
1. Agent calls the broker tool schedule.create { cron "0 18 * * *", tz, prompt }
      → vamd authorizes the tool call (Cedar / HITL) as for any hosted tool
      → vamd stamps CallerContext and attaches a RunGrant to InvokeRequest

2. Scheduler broker stores the schedule + the RunGrant in its own storage
      → vamd stores nothing about the schedule

3. At 6 PM the broker's own ticker fires and asks vamd:
      RequestRunTrigger { run_grant, idempotency_key, fire_at, trigger_ref, kind }

4. vamd verifies the grant → checks → start run as the grant's owner
      → QUEUED if the workspace has a live task (one queued fire per grant); BUSY otherwise

5. The scheduled run posts its turn into the chosen thread
      → it cannot create another schedule (no grant is issued inside a
        broker-started run)
```

**Important:** Creating a schedule is a **setup** action. Running on a schedule is a **repeat** action. They must not be the same step in the agent playbook.

### Example 2 — Slack mention

**User says:** “When someone @mentions the bot in #support, start the triage agent.”

**Flow:**

```text
1. Operator registers vam.slack broker with webhook secret in vault

2. User's agent calls the broker's subscribe tool (channel filter)
      → broker stores the subscription + the RunGrant vamd attached

3. Slack sends webhook → broker verifies signature → normalizes payload

4. Broker asks vamd: RequestRunTrigger { run_grant, idempotency_key = event id } (event brokers are a later phase)

5. vamd validates → starts triage agent with event context in run inputs

6. Agent posts a reply in Slack
      → broker ignores messages the agent itself posted (no echo loop)
```

### Example 3 — Long-running job callback

**User says:** “When the nightly export finishes, run the report agent.”

**Flow:**

```text
1. Playbook step starts an external export (via broker tool) and pauses (SUSPENDED)

2. Export service calls back → broker receives completion event

3. Broker asks vamd: RequestRunTrigger or ResumeRun { continuation_id, ... }

4. vamd validates → resumes or starts the next run with export output as input
```

This reuses the same “external signal arrives → host decides” pattern as [Human-in-the-Loop](/runtime/workflow/) resume, but for brokers instead of only `DecideApproval`.

---

## Architecture

```text
  User chat → agent (guest) → schedule.create tool call
           │
           ▼
  ┌─────────────────────────────────────────────┐
  │  vamd (trusted control plane)               │
  │  • Stamps CallerContext, issues RunGrant    │
  │  • Stores grant records (hash only)         │
  │  • Verifies grant + checks at fire time     │
  │  • start_run → session → ReAct loop         │
  │  • Stores NO schedules and runs NO tickers  │
  └───────────────┬─────────────────────────────┘
                  │
     HSP today    │    HSP proposed (reverse)
     vamd → broker│    broker → vamd
     (+ RunGrant) │    (RequestRunTrigger)
                  ▼
  ┌─────────────────────────────────────────────┐
  │  Broker (scheduler, slack, jira, …)         │
  │  • Stores schedules + their RunGrants       │
  │  • Runs its own clock / webhook / queue     │
  │  • Holds vendor credentials (via manifest)  │
  │  • Asks vamd to fire — never starts alone   │
  └─────────────────────────────────────────────┘
```

### What lives where

| Piece | Owner | Why |
|---|---|---|
| Schedule storage (cron, timezone, paused/deleted) | Broker | Broker product; `vamd` has no trigger store |
| Clock / webhook / queue loop and misfire policy | Broker | `vamd` never runs tickers |
| Agent tools to create/list/pause/delete schedules | Broker (HSP tool exports) | Same declare → approve → enforce path as other hosted tools |
| Grant record: who the run is for (owner, agent scope, workspace, creating thread), which broker may use it, status | `vamd` (a Credential row in `api_keys`, secret stored as SHA-256 only) | Host-stamped at creation; listable and revocable per grant. A delegation record, not schedule state |
| Grant token (`vam_rg_<grant_id>.<secret>`) | Broker, next to its schedule | Presented on every fire; useless outside that broker's gateway socket |
| Run creation, session, Cedar, admission | `vamd` | Host authority — same as today |

### Why a grant, and not the broker echoing `CallerContext`

The broker already sees a host-stamped `CallerContext` on every `Invoke`. It is tempting to let it send those fields back at fire time and have `vamd` run "as" that user. That makes every approved broker able to start runs as **any** user — a confused deputy. The grant keeps storage in the broker while identity stays a host decision:

- `vamd` writes the grant record from the same host-stamped values it puts in `CallerContext`.
- The broker only holds a bearer token. Owner, agent scope, and broker binding live in `vamd`'s record, so the broker has nothing it could edit.
- A grant is bound to the broker that received it; another broker presenting it is denied.

---

## The contract

Today `HostedBroker` is **server-only**: `vamd` is always the gRPC client. Two additions, both in `proto/vam/v1/hosted_services.proto` and additive:

1. **Grant issuance on `Invoke`**: `InvokeRequest.run_grant = 9`.
2. A small **reverse service**, `HostedTriggerGateway`, served by `vamd` on one Unix socket per approved broker.

### Manifest addition

```protobuf
// CapabilityManifest
RunRequestPermission run_request_permission = 10;

message RunRequestPermission {
  repeated string grant_tools = 1;         // subset of tools. Their Invoke may carry a RunGrant.
  int32 max_requests_per_minute = 2;       // per broker, enforced by the gateway; 0 = 60
  int32 max_requests_per_day = 3;          // 0 = 1440
  repeated string loop_guarded_tools = 4;  // subset of tools, superset of grant_tools. Refused inside a triggered run.
  string surface = 5;                      // receipt surface, [a-z]{3,16}; not cli|app|api|system
  int32 max_active_grants_per_user = 6;    // per owner and broker; 0 = 50
}
```

Validation (`vam-spec`): `grant_tools` must be non-empty and a subset of `tools`; `loop_guarded_tools` must be a subset of `tools` and contain every grant tool; `surface` is 3 to 16 lowercase letters and not `cli`, `app`, `api` or `system`; limits cannot be negative; a `THIRD_PARTY` manifest may not declare it. A limit left at 0 means the node default (60 per minute, 1440 per day, 50 active grants per user); it is never unlimited.

Approval uses the same subset gate as `secret_bindings` and `egress_domains`. A presented manifest may not declare the permission when the approved one does not, may not add grant tools, may not drop a loop-guarded tool, may not change the surface, and may not raise a limit (compared after the defaults replace 0).

| Tier | May declare it, receive grants and call `RequestRunTrigger` |
|---|---|
| `TRUST_TIER_CORE` | Yes |
| `TRUST_TIER_FIRST_PARTY` | Yes |
| `TRUST_TIER_OPERATOR` | Yes |
| `TRUST_TIER_THIRD_PARTY` | No until marketplace VM isolation ships |

### Grant on `Invoke`

```protobuf
message RunGrant {
  string token = 1;                 // vam_rg_<grant_id>.<secret>; returned once, to this broker
  string grant_id = 2;              // rg_<26 chars base32>
  google.protobuf.Timestamp issued_at = 3;
  google.protobuf.Timestamp expires_at = 4;
  GrantTarget target = 5;           // what vamd bound, for the broker's display
}
```

A grant tool asks for a run in its arguments, at key `run`, shaped as `RunGrantRequest`: `agent_urn`, `workspace` and `thread` (each a `PlaceChoice` of `SAME`, `NEW` or `ID`), `message`, `playbook_ref`, `inputs`, `on_busy` (`QUEUE` default, or `SKIP`) and `expires_at`. In JSON the short enum names (`SAME`, `NEW`, `ID`, `QUEUE`, `SKIP`) and the full proto names are both accepted; unknown keys are refused. `vamd` resolves the target once, at issuance, to a Space id and a thread id and binds them in the grant. `RequestRunTriggerRequest` carries no target and no prompt.

### Grant record (stored in `vamd`)

A run grant is one kind of **Credential** (`design/vam-apps-spaces.md`): a row in `api_keys` with holder `broker:<id>`, scope `runs:start`, issued via `invoke`. `vamd` keeps a SHA-256 of the secret, never the token. HTTP bearer auth refuses every row whose holder is not `http`. The API key record gains `holder`, `target_kind`, `target_id`, `target_label`, `message_preview`, `issued_via`, `tool_name` and `expires_at`; `GET /v1/keys` accepts `holder` (empty lists the user's HTTP keys; `broker` lists grants).

### Reverse RPC

```protobuf
service HostedTriggerGateway {
  rpc RequestRunTrigger(RequestRunTriggerRequest) returns (RequestRunTriggerResponse);
  rpc RevokeRunGrant(RevokeRunGrantRequest) returns (RevokeRunGrantResponse);
}

message RequestRunTriggerRequest {
  string run_grant = 1;
  string idempotency_key = 2;   // at most 128 chars; the same on every retry of one fire
  google.protobuf.Timestamp fire_at = 3;
  string trigger_ref = 4;       // broker's label; pinned on the first accepted fire
  FireKind kind = 5;            // SCHEDULED | RUN_NOW | EVENT
  google.protobuf.Timestamp not_after = 6;  // a queued fire not started by then ends EXPIRED
  TraceContext trace_context = 7;
  reserved 8;                   // event_context: event brokers, a later phase
}
```

The response carries an `outcome` and the fields that go with it:

| Outcome | Meaning |
|---|---|
| `ACCEPTED` | Run started; `run_id`, `space_id`, `thread_id` set |
| `QUEUED` | The workspace has a live task; the fire is queued behind it (`on_busy QUEUE`, at most one per grant) |
| `DUPLICATE` | The key was already used; that run's id is returned |
| `BUSY` | `busy_reason`: `SKIPPED` (`on_busy SKIP`), `ALREADY_QUEUED`, or `ACTIVE_RUN` (task finalizing or stopping; `retry_after_ms` set) |
| `RATE_LIMITED`, `UNAVAILABLE` | Retry after `retry_after_ms` with the same key |
| `DENIED` | `deny_reason` (`REVOKED`, `EXPIRED`, `WRONG_BROKER`, `TOOL_NOT_APPROVED`, `OWNER_DISABLED`, `AGENT_UNAVAILABLE`, `TARGET_GONE`, `INVALID_REQUEST`, `NOT_PERMITTED`, `TRIGGER_REF_MISMATCH`); do not retry |

Rules for the wire contract:

- The broker sends a grant, a key, a time, a label and a kind. Never an arbitrary `RunSpec`, owner, agent, prompt or target. Those come only from the `vamd` grant record.
- The calling broker is the socket the call arrived on; no broker id is read from the request.
- `RevokeRunGrant` proves possession of the token, revokes the grant and discards its queued fire.

### Ids and the caller

`CallerContext` gains `space_id = 8`, `thread_id = 9` and `run_source = 10` (empty for a person, `broker:<id>` for a triggered run). `RunSpec` gains `space_id = 12` and `thread_id = 13`; `RunStatus` gains `space_id = 20`, `thread_id = 21` and `source = 22` (`RunSource`: kind, broker id, grant id, trigger ref, fire time, fire kind). Space and thread ids are opaque (`sp_...`, `th_...`); names are labels, and every API accepts either.

### Agent-facing tools (broker-owned)

The scheduler broker exports normal HSP tools (`schedule_create` receives the grant; list, pause and delete do not). It partitions its storage by `CallerContext.user_id`, so the manifest must approve `identity_requirement.user_id`.

---

## What `vamd` checks on a fire

On `RequestRunTrigger`, in order: the socket's broker is approved, enabled, of an allowed tier and declares the permission; the token parses and its secret hash matches in constant time; the holder is the calling broker; the grant is not revoked or expired; its tool is still an approved grant tool; the owner is active; the `trigger_ref` matches the pinned one; the per-broker rate limit; dedupe on `hsp:<broker_id>:<key>`; the Space and thread still exist for the owner; busy handling (queue or skip); then the same start path as `POST /v1/runs`. Revoking works on grants: a user lists and revokes them, the broker calls `RevokeRunGrant`, and disabling the broker's approval closes its socket.

---

## Stopping runaway loops

| Loop pattern | Guard |
|---|---|
| Scheduled run creates another schedule | Inside a run a broker started, `vamd` refuses the Invoke of any `loop_guarded_tools` entry (a superset of `grant_tools`), reading the run's recorded source |
| Broker fires twice for one event | The idempotency key is deduped by `vamd` |
| Pile-up (job still running at next fire) | The fire is queued once per grant, or skipped, as the grant says |
| Broker retry storm | Per-broker rate limits (60 per minute and 1440 per day unless the manifest sets others) |
| Broker impersonates a user | The owner comes only from the grant record, which is bound to the receiving broker |

---

## Relationship to existing HSP surfaces

| Surface | Direction | Use |
|---|---|---|
| `Invoke` | Guest to `vamd` to broker | Agent calls a tool during ReAct |
| `OnRequest` / `OnComplete` / `OnResponse` | `vamd` to broker | Lifecycle around a run that already started |
| `RunGrant` on `Invoke` | `vamd` to broker | Host-issued permission to start runs later as the caller |
| `RequestRunTrigger` | Broker to `vamd` | Start or queue a run from a clock |
| `POST /v1/runs` | API client to `vamd` | Interactive run start (unchanged) |

Inbound triggers add no guest-visible vsock RPCs. The guest still uses Port 52 only.

---

## Non-goals

- Cron tickers, schedule storage, or misfire handling inside `vamd`. A queued fire's `not_after` is a task deadline, not a schedule.
- Brokers that hold a long-lived `runs:write` API key and call `POST /v1/runs` directly.
- Brokers asserting `user_id`, agent, or owner at fire time.
- A fire-time target or prompt in `RequestRunTrigger`.
- Event brokers (Slack, Jira): a later phase.

---

## Related

* [Hosted Services Overview](/hosted/overview/) — HSP motivation and plane boundary
* [HSP Architecture](/hosted/architecture/) — broker lifecycle today
* [Capability Manifests](/hosted/capability-manifest/) — declare, approve, enforce
* [Lifecycle Hooks](/hosted/hooks/) — run-boundary hooks (orthogonal to triggers)
* [Admission Control & Queuing](/orchestrator/overview/) — capacity at fire time
* [Human-in-the-Loop](/runtime/workflow/) — pause/resume pattern for external signals
