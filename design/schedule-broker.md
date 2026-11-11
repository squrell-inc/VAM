# The schedule broker: `vam.schedule`

**Status:** Proposed (plan of record for E5 Phase 2, "work outside chat")
**Date:** 2026-10-29
**Owner:** maintainer
**Repos:** `squrell-inc/VAM`
**Depends on:** `design/hsp-triggers.md` (run grants, the trigger gateway,
amendment 2026-10-29) and `design/vam-apps-spaces.md` (Credentials, Space and
thread ids, amendment 2026-10-29). Phases: `design/hsp-triggers.md` §
"Phases (E5 getting-started slice)".

## Decision record

**Context.** P7: "real work doesn't happen in a chat window"
(`design/agent-experience.md` P7). The first trigger a user asks for is a
clock: "summarise my Jira board every day at 6 PM". The launch demo is a
scheduled `close-prep` run (`design/launch-0.1.0.md` §1).

A prototype exists on `origin/feat/scheduler-broker` (tip 647
commits behind `main`, 14 ahead, criss-cross merge base): a TypeScript broker
under `examples/vam-scheduler-ts` (`vam.scheduler`, Node, `better-sqlite3`,
`croner`). Its calendar logic is sound and has 59 tests. Its one
load-bearing choice is not:

- `schedule_create` declares a required secret `vam_api_credential`
  (`src/tools.ts:75-76`), which the manifest maps to the vault key
  `vam.scheduler.credential` (`manifest.json:4-17`). The operator runbook has
  the operator mint a `runs:write` API key owned by the user and log in **as
  the user, with their password**, to put that key in the user's vault
  (`ONBOARDING.md:18-76`).
- On every create the broker copies that key out of the Invoke and stores it
  **in plain text**, one per owner, in its own SQLite file
  (`src/create.ts:316-325`, table `owner_credentials`, `src/schema.ts:43-47`),
  outside `vamd`'s list and revoke path, forever (deleted only with the
  owner's last job, `src/write.ts:150`).
- Because vault resolution falls back to org scope
  (`crates/vam-orchestrator/src/caller_context.rs:190-193`), it then calls
  `GET /v1/me` with the key to check it belongs to the caller
  (`src/create.ts:304-310`, `src/me.ts:44-82`).
- At fire time it calls `POST /v1/runs` over HTTP with the stored key
  (`src/tick.ts:57-85, 297-303`). On `main` a run for another thread on a busy
  workspace is now queued (`crates/vam-orchestrator/src/api/inbox.rs:388-410`),
  so its "busy" branch (`src/tick.ts:112-142`) is dead.
- The branch also adds run-delegation bearers to `vamd` (every
  `create_run` mints a `vam_rd_…` bearer that can mint 7-day API keys) with a
  non-CSPRNG secret (`run_delegation.rs:37-48`). The broker never uses them.

That is exactly what `design/hsp-triggers.md` forbids ("Brokers holding a
`runs:write` API key and calling `POST /v1/runs`", "Brokers asserting
`user_id`").

**Decision (owner, 2026-10-29): redesign, do not rescue.** `vam.schedule` is
a new Rust crate, `brokers/vam-schedule`, built on run grants and the trigger
gateway. It ports the prototype's calendar, idempotency derivation, tool
shapes, ownership rules and tests, and drops its credential path entirely.

> The broker decides **when**. `vamd` holds **who, what and where**, bound in
> a grant the user approved.

## Audience and positioning

People who use an agent and want it to work while they are away: "every
weekday at 9 send me the overnight PR summary", "at 17:00 on the last day of
the month run close-prep", "tomorrow at 8 remind me in this thread". Also the
benchmark's P7 line (`design/benchmark.md` §1: "tasks started by a trigger …
that complete; parked cost (must be 0)"), and B2's `candidate-onboarding`
task.

## What we are building

1. **`brokers/vam-schedule`**: a library and a binary `vam-schedule-broker`,
   its own process, serving `HostedBroker` over UDS and calling `vamd`'s
   `HostedTriggerGateway` over UDS. Depends on `vam-spec` and shared
   libraries only (agent-runtime locked decision 22), never on
   `vam-orchestrator`.
2. **Six tools** for the agent: `schedule_create`, `schedule_list`,
   `schedule_get`, `schedule_update`, `schedule_delete`, `schedule_run_now`.
3. **A clock**: one loop that fires due schedules through the gateway, with a
   catch-up policy, a stable idempotency key per fire, and backoff.
4. **A store** keyed by schedule and grant: timing, the grant token, fire
   history. No user credential of any kind.

## Why

- A clock is standing work with a listener and a timer; the layer rule puts
  it in a broker (agent-runtime locked decision 15; decision 16: `vamd`
  stores no standing schedules).
- The grant is listed, revocable, expiring, refused over HTTP, bound to one
  broker, one agent, one Space, one thread and one approved request. The
  prototype's API key was none of these.
- Rust under `brokers/` is the shape `vam.web` already set
  (`brokers/vam-web/Cargo.toml`, `src/serve.rs`), with one build, one test
  runner, one supply chain.

## What we are not building

- Notifications of results to Slack or email (`vam.notify`, E12 Phase 2).
- Event triggers, `wait_for`, resume grants, `vam.github` (E5 later phase;
  `design/hsp-triggers.md` amendment 2026-10-29 §8).
- A schedule editor in the app. The app shows grants and lets the user revoke
  (Phase 2 item WO-2.6); editing is through the agent.
- Schedules owned by a team or a shared App (Apps are cut from 0.1.0,
  `design/launch-0.1.0.md` §2).
- Sub-minute schedules, or cron with seconds or years fields.
- Anything from the prototype on the `vamd` side (run-delegation bearers,
  `POST /v1/runs/{id}/keys`, caller-asserted `RunOrigin`).

## Decision summary (proposed)

| Topic | Decision |
| --- | --- |
| Home | `brokers/vam-schedule`, Rust, own process, HSP only (decision 22). Broker id `vam.schedule`, tier `TRUST_TIER_FIRST_PARTY`. |
| Credential | None stored but the run grant token. No vault binding, no API key, no `/v1/me`. |
| What is bound where | `vamd` binds agent, Space, thread, message, inputs, playbook, on-busy and expiry into the grant at approval. The broker owns only timing. |
| Tools | `schedule_create`, `_list`, `_get`, `_update`, `_delete`, `_run_now`. Pause and resume are `update {enabled}`. |
| Side effects | create, update, delete, run_now: `MUTABLE_EXTERNAL` (ask by default under permissions-as-data); list, get: `READ_ONLY`. |
| Grant tools | `schedule_create`; `schedule_update` when its args carry `run`. |
| Loop guard | `schedule_create`, `schedule_update`, `schedule_run_now` are refused by `vamd` inside a run a trigger started. |
| Ownership | Reads: the caller's own schedules, owner first. Writes: owner and creating agent only. From host-stamped `CallerContext`. |
| Busy | Per schedule, bound in the grant: `queue` (default) or `skip`. At most one queued fire per grant. |
| Catch-up | `latest` (default): after downtime, fire once for the most recent missed time inside a grace window; never backfill more. `skip` available. |
| Idempotency | `sha256("vam.schedule\n" + schedule_id + "\n" + fire_at RFC 3339 UTC + "\n" + kind)`, reused on every retry. |
| Expiry | Every grant expires: default 90 days, cap 365; renewal is an `update` the user approves. |
| Delete | Revokes the grant at `vamd` before the row goes. |
| Receipt | Surface `schedule`; "requested by schedule `<id>` via grant `<id>`". |

## Target model

### Roles

**`vamd` (Phase 1 of this slice, `design/hsp-triggers.md`)**
- Runs the permission decision on the tool call (ask by default).
- Validates the `run` object, resolves it to ids, mints the grant, hands it
  to the broker inside the Invoke.
- At fire time verifies the grant, starts the run as the owner through the
  shared start path, or queues it behind the live task, or answers why not.

**`vam.schedule`**
- Stores schedules and their grant tokens; computes next fire times; fires.
- Never sees an API key or a password; never asserts who the user is.

**Guest agent**
- Fills the schedule request from its run context and the user's words,
  calls the tool, reports the result. Never sees a grant token.

### Sequence: "summarise my Jira board every weekday at 18:00"

```mermaid
sequenceDiagram
    participant G as Guest agent
    participant V as vamd
    participant S as vam.schedule
    G->>V: schedule_create {title, when: cron "0 18 * * 1-5" Europe/Berlin, run: {message, workspace SAME, thread NEW, on_busy QUEUE}}
    V->>V: permissions: MUTABLE_EXTERNAL → ask; user approves the card
    V->>V: loop guard; validate run; resolve ids (space sp_…, new thread th_…); mint grant rg_…
    V->>S: Invoke + CallerContext + RunGrant {token, grant_id, expires_at, target}
    S->>S: store schedule sch_… (grant token, next_fire_at)
    S-->>V: {schedule_id, next_fire_at, expires_at}
    Note over S: weekday 18:00 Europe/Berlin
    S->>V: RequestRunTrigger {token, idempotency_key, fire_at, trigger_ref sch_…, kind SCHEDULED, not_after}
    V->>V: verify grant; dedupe; rate limit; busy? → queue or start
    V-->>S: ACCEPTED run_id (or QUEUED / BUSY / DENIED …)
```

## The tools

Names use `_` like `web_search` and the prototype (`src/tools.ts:9-16`).
Schemas reject unknown keys and any identity-shaped key (`user`, `owner`,
`user_id`, `agent_urn` at top level), as the prototype does
(`src/create.ts:13-23, 56-65`). Errors are typed (`INVALID_ARGUMENT`,
`NOT_FOUND`, `FAILED_PRECONDITION`) so the model can repair a call.

| Tool | Side effect | Default decision | Grant | Loop-guarded | Does |
| --- | --- | --- | --- | --- | --- |
| `schedule_create` | `MUTABLE_EXTERNAL` | ask | yes | yes | Store a schedule with the grant `vamd` attached |
| `schedule_list` | `READ_ONLY` | allow | no | no | The caller's schedules: id, title, when, next fire, state, target labels, expiry, last outcome. No message text |
| `schedule_get` | `READ_ONLY` | allow | no | no | One schedule with its message and the last 10 fires |
| `schedule_update` | `MUTABLE_EXTERNAL` | ask | when `run` is present | yes | Change title, `when`, `enabled`, `catch_up`; with `run`, re-bind (new grant, old one revoked) |
| `schedule_delete` | `MUTABLE_EXTERNAL` | ask | no | no | Revoke the grant, then delete |
| `schedule_run_now` | `MUTABLE_EXTERNAL` | ask | no | yes | Fire once now with the stored grant (kind `RUN_NOW`) |

"Default decision" is the `bySideEffect` default
(`crates/vam-security/src/permissions.rs:496-516`: read-only and mutable-local
allow, mutable-external ask). The user may "always allow" any of them; the
grant still binds what was approved, and every scheduled run is decided by
the same policy as a chat run (its own asks park and wait).

`schedule_delete` is not loop-guarded: a scheduled run may end its own
schedule ("stop once the board is empty").

### `schedule_create` arguments

```json
{
  "title": "Daily Jira summary",
  "when": { "cron": { "expression": "0 18 * * 1-5", "timezone": "Europe/Berlin" } },
  "catchUp": "LATEST",
  "clientRequestId": "optional, dedupes a retried create",
  "run": {
    "agentUrn": "",
    "workspace": { "mode": "SAME" },
    "thread": { "mode": "NEW", "name": "daily-jira" },
    "message": "Summarise my Jira board: what moved today, what is blocked.",
    "playbookRef": null,
    "inputs": null,
    "onBusy": "QUEUE",
    "expiresAt": null
  }
}
```

`when` is one of:
- `{"once": {"at": "2026-11-02T08:00", "timezone": "Europe/Berlin"}}`: a wall
  time in an IANA zone, or an instant with an offset (`…Z`, `…+01:00`),
  parsed as the prototype does (`src/next_fire.ts:50-93`). Must be in the
  future (a past time is `INVALID_ARGUMENT`, not "fire now").
- `{"cron": {"expression": "<5 fields>", "timezone": "<IANA>"}}`: five-field
  cron in the zone (`src/next_fire.ts:95-120`). The broker refuses an
  expression whose next two fires are closer than the operator floor
  (`--min-interval`, default 15 minutes).

`run` is the **schedule request** `vamd` reads. Its JSON is the proto message
`RunGrantRequest` (`design/hsp-triggers.md` amendment 2026-10-29 §3); the
broker's tool schema embeds it unchanged under the key `run`. The agent fills
it from its run context plus the user's ask:

| Field | Values | Default | `vamd` resolves it to |
| --- | --- | --- | --- |
| `agentUrn` | a catalog URN | the calling agent | an agent the owner may run today (`catalog_resolve::load_agent_for_run`, `catalog_resolve.rs:43`) |
| `workspace` | `SAME`, `NEW {name?}`, `ID {id}` | `SAME` | a `space_id` (`SAME`: the calling run's Space; `NEW`: a Space created now, label from `name` or `schedule-<6>`; `ID`: an owned Space bound to the target agent) |
| `thread` | `SAME`, `NEW {name?}`, `ID {id}` | `NEW` | a `thread_id` (`SAME`: the calling thread, only with workspace `SAME`; `NEW`: a thread created now in the target Space, dedicated to this schedule; `ID`: an existing thread of that Space) |
| `message` | text, ≤ 32 KiB | required unless `playbookRef` | stored on the grant; the user turn every run starts with |
| `playbookRef` | a playbook the agent may start | none | an explicit start (agent-runtime decision 18 keeps explicit starts for trigger grants) |
| `inputs` | object | none | checked against the playbook's `input_schema` now and again at each fire |
| `onBusy` | `QUEUE`, `SKIP` | `QUEUE` | stored on the grant; applied by `vamd` |
| `expiresAt` | RFC 3339 | now + 90 days | capped at now + 365 days; for `once`, at most the fire time + 24 h |

Rules `vamd` enforces at issuance (Invoke refused before the broker sees it,
with a typed reason the agent can repair):

- `SAME` workspace with another agent is refused: a workspace binds one agent
  (`AgentWorkspaceMismatch`, `crates/vam-orchestrator/src/api/mod.rs:1272-1282`).
  Another agent needs `NEW` or the id of a Space bound to it.
- `SAME` thread with a `NEW` or `ID` workspace is refused.
- At most `max_active_grants_per_user` live grants per owner and broker
  (default 50).

Why `NEW` means one dedicated place, not one per fire: the prototype gave
every fire of a `newWorkspace` job its own workspace and every fire into a
reused workspace its own thread (`src/tick.ts:242-254`). That leaves a
workspace or a thread per day with no cleanup (retention is cut from 0.1.0),
and loses "yesterday's summary" as context. One Space or thread per schedule
keeps history where the user expects it; compaction (E10 Phase 2) keeps it
bounded.

### Result of `schedule_create`

```json
{
  "scheduleId": "sch_7f3a…",
  "title": "Daily Jira summary",
  "nextFireAt": "2026-10-30T17:00:00Z",
  "nextFireLocal": "2026-10-30 18:00 Europe/Berlin",
  "expiresAt": "2027-01-27T09:12:00Z",
  "target": { "agentUrn": "…", "space": "assistant (sp_…)", "thread": "daily-jira (th_…)", "onBusy": "QUEUE" }
}
```

The target labels come from `RunGrant.target`, never from the broker's own
reading of the args. The grant token is never returned to the agent.

## The store

SQLite in the broker's `--state-dir` (file mode 0600, directory 0700), one
process per store (an exclusive `flock` on `vam-schedule.lock`; the prototype
double-fires with two processes, `src/tick.ts:1-8`).

```sql
CREATE TABLE schedules (
  schedule_id TEXT PRIMARY KEY,        -- "sch_" + 26 hex of sha256("vam.schedule.id\n" + creating grant id)
  grant_id TEXT NOT NULL UNIQUE,       -- the current grant (changes on re-bind)
  grant_token TEXT NOT NULL,           -- rg_<id>.<secret>; never logged, never returned
  grant_expires_at INTEGER NOT NULL,
  target_json TEXT NOT NULL,           -- RunGrant.target, for display only
  message_preview TEXT NOT NULL,       -- first 200 chars, for get/list display only
  owner_user_id TEXT NOT NULL,         -- CallerContext.user_id at create
  creator_agent_urn TEXT NOT NULL,     -- CallerContext.agent_urn at create
  visibility TEXT NOT NULL DEFAULT 'agent' CHECK(visibility IN ('agent','user')),
  title TEXT NOT NULL,
  kind TEXT NOT NULL CHECK(kind IN ('once','cron')),
  expression TEXT NOT NULL,
  timezone TEXT NOT NULL,
  catch_up TEXT NOT NULL DEFAULT 'latest' CHECK(catch_up IN ('latest','skip')),
  enabled INTEGER NOT NULL DEFAULT 1,
  state TEXT NOT NULL DEFAULT 'active'
    CHECK(state IN ('active','completed','revoked','expired','needs_attention','deleting')),
  state_reason TEXT,
  next_fire_at INTEGER,                -- UTC seconds; NULL when nothing is due
  backoff_until INTEGER,
  attempts INTEGER NOT NULL DEFAULT 0,
  client_request_id TEXT,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  UNIQUE(owner_user_id, creator_agent_urn, client_request_id)
);
CREATE INDEX schedules_due ON schedules(next_fire_at) WHERE enabled = 1 AND state = 'active';

CREATE TABLE fires (
  schedule_id TEXT NOT NULL,
  fire_at INTEGER NOT NULL,            -- the nominal time (UTC seconds)
  kind TEXT NOT NULL CHECK(kind IN ('scheduled','run_now')),
  idempotency_key TEXT NOT NULL,
  outcome TEXT,                        -- accepted|queued|duplicate|skipped_busy|denied|failed_unavailable|missed
  run_id TEXT,
  missed_count INTEGER NOT NULL DEFAULT 0,
  attempts INTEGER NOT NULL DEFAULT 0,
  first_attempt_at INTEGER,
  last_attempt_at INTEGER,
  PRIMARY KEY (schedule_id, fire_at, kind)
);

CREATE TABLE pending_revokes (            -- grants to give up when the gateway answers
  grant_id TEXT PRIMARY KEY,
  grant_token TEXT NOT NULL,
  reason TEXT NOT NULL,
  created_at INTEGER NOT NULL
);
```

**Keyed by grant.** The schedule id is derived from the id of the grant that
created it: `sch_` + the first 26 hex characters of
`sha256("vam.schedule.id\n" + grant_id)`, so a retried create derives the same
id. (Amended 2026-10-31, E5 Phase 2 wave 4: the first draft used the grant id's
suffix, but `vamd` redacts any 12-character run of the grant token from a
broker's answer, and the grant id is part of the token, so the agent saw
`sch***`.) The receipt names both the schedule (`triggerRef`) and the grant, and
the grant list names the grant, so no lookup table is needed. A re-bind (`update` with `run`) stores the
new `grant_id` and token on the same row and queues the old token in
`pending_revokes`.

**Why the token is stored as given.** A run grant is usable only on this
broker's own gateway socket, by this broker, for one agent, one Space, one
thread and the one request the user approved; it expires and the user can
revoke it (`design/hsp-triggers.md` amendment §4). Encrypting it with a key
kept beside it would add nothing. The file mode and "never logged" rules are
what is required. The prototype's key, by contrast, opened all of `POST
/v1/runs` for the user, over HTTP, from anywhere.

**Ownership** (ported from `src/read.ts` and `src/write.ts`):
- Reads (`list`, `get`): rows where `owner_user_id = caller.user_id` and
  (`creator_agent_urn = caller.agent_urn` or `visibility = 'user'`). Owner
  first: the owner filter is in every query, before any other filter.
  Another owner's id answers `NOT_FOUND`, never `PERMISSION_DENIED`.
- Writes (`update`, `delete`, `run_now`): owner **and** creating agent
  (`src/write.ts:12`, `CREATOR_MATCH`). Visibility never grants a write.
- `CallerContext` is host-stamped; the manifest requires `user_id` and
  `agent` (`IdentityRequirement`, `proto/vam/v1/hosted_services.proto:77-83`),
  so an Invoke without them never reaches the broker
  (`caller_context::assert_identity_satisfied`, `host_tools.rs:2845-2849`).

## The clock

- **Time is UTC instants.** `next_fire_at` is computed from the local wall
  time in the schedule's IANA zone (`chrono-tz`) and stored as UTC. Cron uses
  the Rust `croner` crate (the same library family as the prototype); both
  are new workspace dependencies, pinned, licence and release age checked by
  the wave.
- **DST.** The ported tests pin behaviour across both transitions for a
  02:30 daily schedule in `Europe/Berlin` and `America/New_York`: on the
  spring-forward day it fires once, at the first valid instant after the
  gap; on the fall-back day it fires once, at the first 02:30. If the Rust
  crate differs from the prototype, the tests decide and this section is
  amended.
- **One loop.** It sleeps until the earliest `next_fire_at` (capped at 30 s,
  and woken early by any create, update or delete), selects due rows
  (`next_fire_at <= now`, enabled, active, `backoff_until` passed), fires at
  most 4 at once, and advances each row in the same transaction that records
  the fire's outcome. A `Clock` trait is injected so unit tests move time
  without sleeping.
- **Catch-up after downtime** (the broker was stopped, the host slept, or a
  sleep overshot). For each due row whose nominal time is in the past:
  - `catch_up = latest` (default): if the most recent missed time is within
    the grace window, fire it once (kind `scheduled`, its own nominal
    `fire_at`, so a fire that did reach `vamd` before a crash dedupes);
    older missed times are recorded as one `missed` row with
    `missed_count`. Grace: `once` 24 h; cron half the interval, capped at
    1 h.
  - `catch_up = skip`: record `missed`, advance to the next future time
    (the prototype's `skipMissedFires`, `src/tick.ts:195-240`).
  - Never more than one catch-up fire per schedule.
- **`once`**: after an accepted, queued or duplicate fire, the schedule is
  `completed` and its grant revoked. After a terminal refusal it is
  `needs_attention` or `revoked`.

## Firing

**Request** (`HostedTriggerGateway.RequestRunTrigger`, fields in
`design/hsp-triggers.md` amendment §3):

| Field | Value |
| --- | --- |
| `run_grant` | the stored token |
| `idempotency_key` | `hex(sha256("vam.schedule\n" + schedule_id + "\n" + fire_at RFC 3339 UTC + "\n" + kind))` |
| `fire_at` | the nominal time (not the time the call is made) |
| `trigger_ref` | `schedule_id` |
| `kind` | `FIRE_KIND_SCHEDULED` or `FIRE_KIND_RUN_NOW` |
| `not_after` | the next nominal time (cron), or `fire_at` + 24 h (once, run now): a queued fire that has not started by then ends `EXPIRED` |

The key is the prototype's derivation (`sha256(job_id "\n" fire_at)`,
`src/tick.ts:57-59`) with a namespace and the kind added, so a run-now and a
scheduled fire at the same second never collide. For `run_now`, `fire_at` is
the Invoke time truncated to the second and the key also hashes the calling
run id, so a retried tool call dedupes and two deliberate presses do not.

**Outcomes** (one row in `fires` each; `vamd` writes its own events):

| Outcome | Broker does |
| --- | --- |
| `ACCEPTED`, `QUEUED`, `DUPLICATE` | Record `run_id`; advance. |
| `BUSY` with `busy_reason = SKIPPED` or `ALREADY_QUEUED` | Record `skipped_busy`; advance. No retry. |
| `BUSY` with `retry_after_ms` (the live task is finalizing) | Retry with the same key until `not_after`, then `skipped_busy`. |
| `RATE_LIMITED`, `UNAVAILABLE`, transport error | Retry with the same key: full-jitter exponential backoff from 2 s, cap 5 min, never sooner than `retry_after_ms`; persisted in `backoff_until` and `attempts`; give up at `not_after` with `failed_unavailable`. The schedule stays active. |
| `DENIED` `REVOKED` / `EXPIRED` | State `revoked` / `expired`, disabled. No retry. |
| `DENIED` other reasons (agent no longer runnable, Space gone, owner disabled, tool no longer approved) | State `needs_attention` with the reason, disabled. Shown on `list` and `get`. |

## Revocation

- **On delete:** the broker calls `RevokeRunGrant(token)` first. If the
  gateway answers, the row is deleted. If not, the row goes to `deleting`
  (never fires) and the token to `pending_revokes`; the loop retries the
  revoke every minute and deletes the row when it succeeds.
- **On re-bind and on `once` completion:** the old token goes to
  `pending_revokes`.
- **By the user, without the broker:** `vam keys revoke <grant id>` or
  `DELETE /v1/keys/{id}` (`design/vam-apps-spaces.md` amendment §2). The
  next fire is `DENIED REVOKED` and the schedule shows `revoked`.
- **By the operator:** disabling the broker's approval closes its gateway
  socket; every fire fails and nothing starts.

## What the receipt shows

`vamd` stamps the trigger facts on the run (`design/hsp-triggers.md`
amendment §6). The receipt's requester block reads:

```
requested by   alice, surface schedule
               schedule sch_7f3a… via grant rg_7f3a… (vam.schedule), fire time 2026-10-30T17:00:00Z
```

`surface = schedule` comes from the operator-approved manifest
(`run_request_permission.surface`), not from the broker's request. The grant
id and owner are host facts; the schedule id is the broker's label, pinned on
the grant at its first fire.

## Manifest

```json
{
  "brokerId": "vam.schedule",
  "trustTier": "TRUST_TIER_FIRST_PARTY",
  "secretBindings": [],
  "egressDomains": [],
  "tools": [
    { "toolName": "schedule_create", "sideEffect": "SIDE_EFFECT_CLASS_MUTABLE_EXTERNAL" },
    { "toolName": "schedule_list", "sideEffect": "SIDE_EFFECT_CLASS_READ_ONLY" },
    { "toolName": "schedule_get", "sideEffect": "SIDE_EFFECT_CLASS_READ_ONLY" },
    { "toolName": "schedule_update", "sideEffect": "SIDE_EFFECT_CLASS_MUTABLE_EXTERNAL" },
    { "toolName": "schedule_delete", "sideEffect": "SIDE_EFFECT_CLASS_MUTABLE_EXTERNAL" },
    { "toolName": "schedule_run_now", "sideEffect": "SIDE_EFFECT_CLASS_MUTABLE_EXTERNAL" }
  ],
  "identityRequirement": { "userId": true, "agent": true },
  "runRequestPermission": {
    "grantTools": ["schedule_create", "schedule_update"],
    "loopGuardedTools": ["schedule_create", "schedule_update", "schedule_run_now"],
    "maxRequestsPerMinute": 60,
    "maxRequestsPerDay": 5000,
    "surface": "schedule",
    "maxActiveGrantsPerUser": 50
  }
}
```

No secrets and no egress: the broker talks only to `vamd`.

## Ported from the prototype, and dropped

| Prototype (`origin/feat/scheduler-broker`) | Here |
| --- | --- |
| `src/next_fire.ts` once/cron, IANA zone, ISO wall time | **Ported** to Rust with its tests, plus DST tests |
| `idempotencyKeyFor(job_id, fire_at)`, `src/tick.ts:57-59` | **Ported**, namespaced, kind added |
| Tool schemas and argument hygiene, `src/tools.ts`, `src/create.ts:13-65` | **Ported**: same names; `pause`/`resume` folded into `update {enabled}`; `run` object added; `workspace`/`newWorkspace` replaced by `run.workspace` |
| Owner-first reads, creator-only writes, visibility, `src/read.ts`, `src/write.ts` | **Ported** with their tests |
| `skipMissedFires`, `src/tick.ts:195-240` | **Ported** as `catch_up = skip`; `latest` is new and the default |
| 5 s ticker, `src/tick.ts:15, 377-416` | **Replaced** by sleep-until-next-due with a 30 s cap |
| 59 `node:test` cases | **Ported** where the behaviour survives (create, read, write, next-fire, tick outcomes); the `me` and credential tests are dropped |
| `owner_credentials` table, `src/schema.ts:43-47`, `src/create.ts:316-325`, `src/tick.ts:297-303`, `src/write.ts:150` | **Dropped**. No user credential in the broker |
| `requiredSecrets: [vam_api_credential]`, vault key `vam.scheduler.credential`, `manifest.json:4-17` | **Dropped** |
| `GET /v1/me` identity check, `src/me.ts`, `src/create.ts:304-310` | **Dropped**. Identity is the grant's |
| `httpRunClient` (`POST /v1/runs`), HTTP status mapping, `src/tick.ts:61-142` | **Dropped**; gateway client and typed outcomes |
| Per-fire workspace / per-fire thread, `src/tick.ts:242-254` | **Dropped**; one dedicated Space or thread per schedule |
| `ONBOARDING.md` (operator logs in as the user with their password) | **Dropped** |
| Run-delegation bearers, `POST /v1/runs/{id}/keys`, `run_delegations` table; non-CSPRNG secret `run_delegation.rs:37-48` | **Dropped**, not merged |
| Caller-asserted `RunOrigin` (`RunStatus.origin = 19`, collides with `waiting_for = 19`) | **Dropped**; host-stamped `RunStatus.source = 22` instead |
| `examples/vam-scheduler-ts`, `scripts/lima-hsp-scheduler-ts.sh`, stray `state.db`, root `package.json` | **Not merged.** The branch stays as history; WO-2.7 marks it superseded |

## Phases

E5 Phase 2 in `design/hsp-triggers.md` § "Phases (E5 getting-started
slice)". Items `WO-2.1` to `WO-2.7` are listed there with their Decide lines
and scenarios; this doc is the reasoning they cite.

## Open questions

1. **Cron floor.** Default 15 minutes, operator flag. Lower for demos?
   Default: 15 minutes; the benchmark's scheduled task uses `once`.
2. **Expiry reminder.** A schedule stops silently at expiry. Default: `vamd`
   emits `grant.expiring` 7 days ahead (outbox, Phase 1) and `vam.notify`
   delivers it once E12 Phase 2 lands; `list` shows `expiresAt` from day one.
3. **`schedule_delete` asks.** It is `MUTABLE_EXTERNAL` like create. Default:
   keep ask; users who mind choose "always allow".

## Implementer constraints

- No API key, password, vault binding or HTTP call to `vamd`. The only
  credential is the grant token, used only on the gateway socket.
- Never log the token, the message text or the args at info level.
- Never trust the broker's own reading of `run`: display labels come from
  `RunGrant.target`.
- Every retry of one fire reuses its idempotency key.
- No dependency on `vam-orchestrator` (decision 22).
- Tests: unit tests per module with an injected `Clock`; an integration test
  against a fake gateway; the scenarios of Phase 2. Run `cargo test -p
  vam-schedule`, `cargo check --workspace`, `cargo fmt --all --check`.
