# Notifications: one record, many surfaces

Status: **approved by the owner 2026-11-02** (the design as written, the two required recommendations, every default in §8, and the owner review in §9; see the Appendix for the amendments applied). The module itself was approved on 2026-11-01 as proposal P-D of `design/primary-agent.md` §14.2;
everything below (the record, the kinds, the channel broker, the slices) is
this doc's proposal. Written by a design session against `main` at
 (the primary agent design; E5 Phases 1 and 2 and E12
Phase 1 on `main`). Where `main` disagrees with an earlier design, `main`
is the fact. Nothing in a tracker is edited by this doc; proposed
amendments are in the Appendix. Tracker slug proposed: `notifications`,
item prefix `NT`.

The owner, 2026-11-01: "we have to build a common notification system which
can produce different types of events and then can be used to notify the
user on different devices that the user might have, from mobile to desktop
app or to the web. There should be a dedicated notification system which
can tell about this, and similarly the other systems can also read from
this notification module."

The case that motivated it: an asked run parks on a tool approval while the
user is talking to the primary agent in another thread, and nobody tells
them. Today the approval sits in `GET /v1/approvals` and the app shows it
only where the user happens to look.

## Decision record

| Topic | Decision (proposed) |
| --- | --- |
| What it is | A **notification record** in `vamd`'s store: one row per event and user, written by `vamd` at the moment it already records the underlying state change. Not a second event bus: every kind is derived from a state change or outbox event that exists on `main` (§2). |
| Producers | `vamd` only. The HITL suspend path, gate open, run end (with the receipt), the grant sweeper, budget stops, gateway fire outcomes; later the report-back task (primary agent). **No broker writes a record.** |
| Consumers | The API (list, get, read, acted), the app (badge and list, drawing the existing cards), the CLI (`vam notifications`), agents (a read-only `notifications_list` tool), and **channels**: Slack first, then email and mobile push, delivered by a first-party broker `vam.notify`. |
| Clearing | A kind with an action is cleared by the action wherever it was taken: a gate decided in Slack clears the app badge; a gate decided in the app updates the Slack message. Read is per user, not per channel. |
| Deciding from Slack | A person's click, relayed by `vam.notify` through its gateway socket with a single-use handle `vamd` minted for that delivery, from a Slack account the user linked. `vamd` decides through the same code as `POST /v1/approvals/{id}/decide`, with surface `slack`. The broker has no authority of its own. |
| Agents | Read, never act. `notifications_list` returns the calling user's unread host-written records with ids and links; no tool marks, decides or answers (primary-agent §14.1 rule). |
| Scope | Per user, never cross-user. Org approvals inbox is E3 (not designed here). |
| Phases | Slice 1 (record, four producers, API, app badge and list, tool, CLI) is E12 Phase 2's first half and the primary agent's Phase 1 dependency. Slice 2 (`vam.notify` with Slack, decide from Slack, preferences, grant and budget kinds) completes row 7 and the live demo's Slack gate. Slice 3 (email, push, digests) is later. |

## What we are building

When something needs the user, or something they left running is done, VAM
writes one record for them: what happened, which run, thread and Space it
belongs to, and what they can do about it. The app shows a badge and a list;
the CLI prints the same list; the primary agent can read it and tell the user
"two things wait on you"; and once the user links Slack, the record arrives
there too, with Approve and Deny buttons for an approval or a gate. Whichever
surface the user decides on, every other surface clears. The receipt records
where each decision was made.

## Why

- **P2 governance and audit** (`design/agent-experience.md:92-99`): "An
  approvals inbox covering every run, not just one chat." Approvals exist
  per user on `main` (`GET /v1/approvals`, `api/mod.rs:1521-1554`), but
  nothing tells the user one is waiting unless they look, and gates,
  questions, finished background runs and expiring grants have no common
  place at all.
- **P7 work outside chat** (`design/agent-experience.md:143-150`): "Delivering
  approvals and results where people already work (Slack, email)." The
  benchmark line (`design/benchmark.md:95`) counts "tasks started by a
  trigger and decided from a notify surface that complete".
- **The launch demo** (`design/launch-0.1.0.md` §1): "a gate decided from
  Slack a day later". Row 7 of §4 is this module.
- **The primary agent** (`design/primary-agent.md` §14.2): the primary must
  answer "anything waiting on me?" and the user must learn that an asked run
  parked, without the primary acting on it.

## 1. What `main` has today

| Fact | Where | What it gives this design |
| --- | --- | --- |
| Approvals table with kind `tool_call`, `gate`, `question` | `hitl.rs:15-23` | Three of the "waiting on you" kinds already share one row shape. |
| Tool and question approvals written at the suspend barrier, in one transaction with `vam.v1.hitl.paused` | `workflow.rs:915-1036` (`create_approval`; insert at `:980`, outbox at `:1033`) | The producer point for `approval.pending` and `question.asked`. |
| Gate approvals inserted when a gate step opens | `workflow.rs:4172-4206` (`open_ready_gates`), `:4208-4269` (`insert_gate_approval`); run parks in `park_for_open_gates` `:1957` | The producer point for `gate.opened`. |
| Decisions record who, when and from which surface | `workflow.rs:1209` (`decide_approval_with_surface`), `vam.v1.hitl.decided` at `:1495`; E12 Phase 1 wave 2 added `decided_at`, `decided_surface`, `decided_by_user_id` (`.cursor/workstreams/run-receipt.md:552`) | `acted_at`, acted-by and surface are copied, not re-observed. |
| Surface is a client hint: `cli`, `app`, else `api` | `run_facts.rs:21-31`, `api/mod.rs:2390-2398` | A client cannot claim `slack`; only the gateway path can (§3.6). |
| Expiry, gate timeout and stop resolve approvals | `workflow.rs:1556` (`expire_stale_approvals`, `hitl.expired` at `:1612`), `:1794` (`apply_gate_timeouts`), `:1653` (`reject_pending_for_stop`) | Resolution without a person's action (`resolved_reason`). |
| What a parked run waits for | `api/question.rs:17-27` (`WaitingFor` approval, gate, input) | The app already distinguishes the three. |
| `on_paused` hooks to subscribed brokers | `hsp_hooks.rs:445-465` (`notify_run_paused`), `OnPausedRequest` `hosted_services.proto:277-285` | A lifecycle hook, kept as is; not the delivery path (§3.5). |
| Receipt issued last when a run ends; 5 s sweeper and startup reconcile | `receipt.rs:923-961` (`issue_receipt`), `:976-986` (`issue_missing_receipts`), `bin/vamd.rs:452`, `:711` | The producer point for `run.finished`, with the receipt digest, and its restart safety. |
| `grant.expiring` once per grant, 7 days ahead, on the grant's last run outbox | `ttl.rs:153-226` (`sweep_expiring_grants`, claims in `grant_expiry_notices`), `events.rs:39-40`, `bin/vamd.rs:683` | The producer point for `grant.expiring`. |
| Active-duration budget fails the run | `workflow.rs:2349-2400` (`expire_over_active_budget`, "run exceeded max_duration of active time") | One producer point for `budget.hit`. |
| Spend budget refuses a model call | `vam-inference/src/broker.rs:422-431` (`check_preflight`, `RESOURCE_EXHAUSTED` to the guest) | The other `budget.hit` point; today the host records nothing there. |
| Gateway fire outcomes | `gateway.rs:508` (`fire_inner`), skip at `:692-697`, `trigger.fired` at `:715-736`, refusals only logged and counted at `:364-391` | Producer points for `trigger.fired` and `trigger.skipped`. |
| One socket per broker, identity is the socket, opened only for a manifest with `run_request_permission` | `gateway.rs:1-5`, `:172-194`; `design/hsp-triggers.md` amendment §5 | Slice 2 opens it also for a manifest with a notification channel (§3.5). |
| Credentials: one `api_keys` row type, holder `broker:<id>` good only on that broker's socket | `credentials.rs:1-30` | The single-use action handle is one more Credential (§3.6). |
| Outbox rows are per run; the event bus fans them out | `workflow.rs:2797-2818` (`append_outbox_event`), `workflow.rs:2735` (`event_bus`), `vam-telemetry/src/bus.rs:15` | The record is per user and is not put on the bus as a new stream (§2.4). |
| Built-in cards | `design/jig.md` §1.5, §7 (`vam.approve`, `vam.question`, `vam.progress`, generated gate default); `vam-spec/src/outbox.rs:11-25` | The app draws a notification's detail with the card it already has. |
| Host built-in tools composed into every agent | `security.rs:160-190` (`OUTBOX_OVERLAY`: `post_message`, `show`) | `notifications_list` is a host built-in too, but opt-in (§3.4). |
| HSP services | `hosted_services.proto:296-304` (`HostedBroker`), `:361-364` (`HostedTriggerGateway`), `InvokeRequest.resolved_secrets` `:213` | `vamd`→broker delivery is a new `HostedBroker` RPC; broker→`vamd` is a new service on the same gateway socket. |

What is missing: a per-user record, any "unread" or "acted" state, a place
for kinds that are not approvals, any channel outside the app, and a way for
an agent to read what waits on its user.

## 2. The record

### 2.1 Fields

One row per event and **recipient user**. Table `notifications` in the
workflow store (SQLite, beside `approvals`).

| Field | Type | Meaning |
| --- | --- | --- |
| `id` | `nt_` + 26 base32 (OsRng, `spaces::mint_id`) | Opaque; foreign ids answer "not found". |
| `user_id` | text | The recipient. Never empty; never a broker. |
| `kind` | text | §2.2. |
| `severity` | `action` \| `warn` \| `info` | `action`: the user must do something; `warn`: something failed or is about to; `info`: done, nothing to do. |
| `subject_key` | text | Dedupe key per kind (§4.3). `UNIQUE (user_id, kind, subject_key)`. |
| `run_id`, `space_id`, `thread_id` | text, nullable | Where it happened (ids, never names). |
| `approval_id` | text, nullable | The approval, gate or question request id. |
| `grant_id` | text, nullable | For grant and trigger kinds. |
| `receipt_digest` | text, nullable | For `run.finished` and `report.arrived`. |
| `agent_urn`, `agent_label` | text | Who the run belongs to, for the title. |
| `via` | JSON, nullable | How the run came to be: `{trigger: {brokerId, grantId}}` or `{delegation: {byRunId, byAgentUrn, threadId}}`. So the card can say "Researcher, asked by Assistant, waits for you". |
| `title` | ≤ 120 chars | **Host-written**, from a fixed template per kind. |
| `body` | ≤ 500 chars | **Host-written**: facts only (tool name, rule, step id, outcome, reason, expiry). |
| `action` | JSON | `{verb, target, api}`: `verb` ∈ `decide`, `answer`, `open`, `renew`, `none`; `target` `{kind: approval\|run\|thread\|grant, id}`; `api` the call that resolves it (e.g. `POST /v1/approvals/{id}/decide`). Surfaces build their own links from `target`. |
| `count` | int | How many times the same subject recurred (§4.3). |
| `created_at`, `updated_at` | unix s | |
| `first_shown_at` | unix s, nullable | First time the record reached the user: returned to an authenticated app or CLI list, or a channel delivery succeeded. Feeds the benchmark line (§6). |
| `read_at` | unix s, nullable | The user saw it. Per user, not per channel. |
| `acted_at`, `acted_by_user_id`, `acted_surface` | nullable | Who resolved it, from where (`app`, `cli`, `api`, `slack`, later `email`, `push`). For decision kinds, copied from the approval's `decided_*` in the same transaction. |
| `resolved_at`, `resolved_reason` | nullable | Closed: `acted`, `expired`, `timed_out`, `stopped`, `superseded`, `run_ended`. An unresolved `action` record stays in the badge even when read. |

**Two trust tiers, as in the receipt** (`design/run-receipt.md` locked
decision 1). The row holds host facts only. Agent-written text is **never
copied into the record**: the API attaches it at read time from where it
already lives (the approval row's stored preview, the thread log), as a
separate field `agentText {label, text}` capped at 500 characters, labelled
"written by <agent>, not verified by the host" (or "from the playbook" for a
gate question rendered with earlier step outputs, `workflow.rs:4147`). Every
surface draws it fenced. This also keeps SQLite free of conversation bytes
(E10 locked decision 23).

### 2.2 Kinds for 0.1.0

| Kind | Severity | Derived from (existing) | Action | Recipient |
| --- | --- | --- | --- | --- |
| `approval.pending` | action | `create_approval` with kind `tool_call` (`workflow.rs:915`, outbox `vam.v1.hitl.paused`) | `decide` | the approval's approver (today always the run owner: `HitlApprover`, `hitl.rs:37-90`; reviewers are not minted) |
| `question.asked` | action | the same insert with kind `question` (`ask_user`; `WaitingFor::Input`) | `answer` | run owner |
| `gate.opened` | action | `insert_gate_approval` (`workflow.rs:4208`) | `decide` | run owner |
| `run.finished` | info (`SUCCEEDED`) or warn (`FAILED`, `TIMED_OUT`, `EXPIRED`, `REJECTED`, `CANCELLED` by someone else) | run terminal state, written when the receipt is stored (`receipt.rs:923-961`) | `open` (the receipt) | run owner; only for the runs in §2.3 |
| `report.arrived` | info, or warn if the asked run did not succeed | the report task's creation (`design/primary-agent.md` §5, idempotent by child run id) | `open` (the asking thread) | the asking user |
| `delegation.stopped`, `delegation.expired` | warn | an asked run that will send **no** report: `delegation.report_undelivered` (primary-agent §5 table, last row), or the grant or queued start expired before the run began (queue starter `not_after`, `design/hsp-triggers.md` amendment §5) | `open` (the asked thread) | the asking user |
| `grant.expiring` | warn | `sweep_expiring_grants` (`ttl.rs:157`), same claim row | `renew` (opens the schedule; renewal is a re-bind the user approves, `work-outside-chat.md` locked decision 13) | grant owner |
| `budget.hit` | warn | duration: `expire_over_active_budget` (`workflow.rs:2349`); spend: the preflight refusal (`vam-inference/src/broker.rs:423`), which gains a host record (§3.1) | `open` | run owner |
| `trigger.fired` | info | `trigger.fired` outbox event (`gateway.rs:715-736`) when the fire was **queued** behind a live task | `open` | grant owner |
| `trigger.skipped` | warn | a fire refused or skipped: `BUSY SKIPPED`, `TARGET_GONE`, `AGENT_UNAVAILABLE`, `EXPIRED` (`gateway.rs:692-697`, `:364-391`); not `REVOKED` (the user did it) nor `OWNER_DISABLED` | `open` (the schedule) | grant owner |

The owner's list says "schedule fired and skipped". The gateway serves any
broker that may request runs, so the kinds are named `trigger.*` and the
title uses the broker's surface: "Schedule `sch_…` skipped: the workspace was
busy" for `vam.schedule`.

**The clearing rule.** A kind with an action is cleared by that action
wherever it was taken, and only by it:

| Kind | Cleared by | `resolved_reason` |
| --- | --- | --- |
| `approval.pending`, `question.asked`, `gate.opened` | the decision through `decide_approval_with_surface` from any surface | `acted` (with surface) |
| same | expiry, gate timeout, stop of the run | `expired`, `timed_out`, `stopped` |
| `grant.expiring` | the grant is renewed or revoked, or expires | `acted` / `expired` |
| `run.finished`, `report.arrived`, `delegation.*`, `budget.hit`, `trigger.*` | `POST /v1/notifications/{id}/acted` (the user opened it), or read | `acted` |

A gate decided in Slack clears the app badge on the app's next poll; a gate
decided in the app updates the Slack message (slice 2). Decision kinds are
never "acted" by the notification API: that answers `409 decide_required`
with the link, so no surface can clear a pending approval without deciding
it.

### 2.3 Which runs produce `run.finished`

Every chat turn is a run (E10 locked decision 2); a record per turn would bury
the ones that matter. A run produces `run.finished` when **any** of:

- it was started by a trigger (`runs.source` = `broker:<id>`,
  `design/hsp-triggers.md` amendment §6);
- it parked at least once waiting on a person (an approval, gate or question
  row exists for it);
- it ended `FAILED`, `TIMED_OUT`, `EXPIRED` or `REJECTED`.

Not for a person-started run that succeeded without waiting, nor one the user
stopped themselves. An asked run (delegation) never produces `run.finished`:
its user gets `report.arrived` or `delegation.*` instead. A run that already
has `budget.hit` for a duration stop does not also get `run.finished`.

### 2.4 Not a second event bus

The record is written from the transaction or sweeper that already records
the fact; it is a **projection per user**, not a new stream. It is not
published as a CloudEvent on `BroadcastEventBus` (that bus carries run-scoped
outbox events; a per-user projection on it would be a second bus with a
different scope). Each write adds one counter
`vam_notifications_total{kind}`; nothing else. Run timelines keep reading
the outbox (`vam run events`), unchanged.

## 3. Producers and consumers

### 3.1 Producers (all in `vamd`)

One function, `notifications::record(tx, NewNotification)`, inserting or
updating by `(user_id, kind, subject_key)`. Callers:

| Kind | Call site | Same transaction as the fact? |
| --- | --- | --- |
| `approval.pending`, `question.asked` | `create_approval`, beside the `hitl.paused` append (`workflow.rs:1033`) | yes |
| `gate.opened` | `insert_gate_approval` (`workflow.rs:4208`) | yes |
| resolution of the three | `decide_approval_with_surface` (`:1209`, beside `hitl.decided` at `:1495`), `expire_stale_approvals` (`:1556`), `apply_gate_timeouts` (`:1794`), `reject_pending_for_stop` (`:1653`) | yes |
| `run.finished` | `issue_receipt` after `insert_run_receipt` returns true (`receipt.rs:78`, `:952`); the startup reconcile and the 5 s sweeper (`bin/vamd.rs:452`, `:711`) therefore cover restarts. A run whose receipt cannot be signed (no node keys) gets the record from the same sweeper after 60 s, without a digest. | idempotent by run id |
| `report.arrived`, `delegation.*` | the report-back task creation and `report_undelivered` (primary agent Phase 1 code; this module ships the function and the kinds) | yes / idempotent by child run id |
| `grant.expiring` | `sweep_expiring_grants`, after the `grant_expiry_notices` claim (`ttl.rs:197-206`) | idempotent by grant id |
| `budget.hit` (duration) | `expire_over_active_budget` (`workflow.rs:2349`) | yes |
| `budget.hit` (spend) | the inference broker's preflight refusal (`broker.rs:423`) through a callback `vamd` installs on the broker (the broker is a library inside `vamd`, not an HSP broker); first refusal per run only | idempotent by run id |
| `trigger.fired`, `trigger.skipped` | `fire_inner` after the answer is known (`gateway.rs:715`), beside `record` (`:364`) | idempotent by grant and UTC day |

A failed `record` call inside a transaction fails that transaction (the
approval and its notification exist together or not at all). A failed call
from a sweeper logs and retries on the next pass, as the receipt sweeper
does.

### 3.2 The API

All routes take the authenticated user from `AuthContext` and answer only
that user's rows. New PDP actions `ReadNotifications` (`notifications:read`)
and `UpdateNotifications` (`notifications:write`); both in the default
member and owner scopes.

| Route | Does |
| --- | --- |
| `GET /v1/notifications?unread=&kind=&state=open\|all&cursor=&size=` | Unresolved `action` records first (oldest first: the longest wait on top), then unread by newest, then read by newest. Paged with the existing cursor helpers (`api/paging.rs:71-100`), size ≤ 100. Sets `first_shown_at`. |
| `GET /v1/notifications/summary` | `{unread, action, warn, etag}` for the badge; `304` on a matching `If-None-Match`. |
| `GET /v1/notifications/{id}` | One record, with `agentText` attached when the kind has one, and for decision kinds the approval as `GET /v1/approvals/{id}` returns it, so the app draws the existing card. |
| `POST /v1/notifications/{id}/read`, `POST /v1/notifications/read` `{ids \| before}` | Idempotent. |
| `POST /v1/notifications/{id}/acted` | Idempotent; surface from `X-Vam-Surface`. `409 decide_required` for decision kinds. |

Deciding stays where it is: `POST /v1/approvals/{id}/decide`. The notification
API never decides.

### 3.3 The app, the CLI, mobile and web

- **App (the VAM app).** A badge with `summary.action + summary.warn`, polled
  every 15 s with the etag (a stream is an open question, §8 q1). A list
  view from `GET /v1/notifications`; each row opens its detail drawn with the
  card the app already has: `vam.approve` for `approval.pending`,
  `vam.question` for `question.asked`, the gate's form (generated default for
  `response_schema`, `design/jig.md` §1.5) for `gate.opened`, the receipt
  view for `run.finished`, the thread for `report.arrived`. Opening a row
  marks it read; following an `open` action marks it acted. The app sends
  `X-Vam-Surface: app` on decide (E12 tracker Q9 default, already planned).
- **CLI.** `vam notifications [--unread] [--kind k]`,
  `vam notifications show <id>`, `vam notifications read <id>|--all`. The
  list prints the decide command for decision kinds (`vam approve <id>`), and
  `vam approve` already sends `X-Vam-Surface: cli`.
- **Mobile and web.** The same list through the same API; no separate
  store. The only new thing is a **push registration** so a phone or a
  browser can be woken (record below); delivery is slice 3.

**Push registration record** (designed now, delivered in slice 3). Table
`push_registrations`:

| Field | Meaning |
| --- | --- |
| `id` | `pr_` + 26 base32 |
| `user_id` | owner of the device; per user only |
| `platform` | `apns`, `fcm`, `webpush` |
| `app_id` | the bundle or web origin the token belongs to |
| `device_label` | the user's label ("Ana's phone") |
| `token_sealed` | device token or Web Push subscription, sealed with the node secret store (`secrets.rs`), never returned by the API |
| `created_at`, `last_seen_at`, `revoked_at` | lifecycle; a token the vendor reports invalid is revoked by the delivery path |

Routes: `POST /v1/me/push-registrations`, `GET` (no tokens), `DELETE /{id}`.
**Push payloads carry no content**: kind, record id and the badge count
only ("1 approval waits for you"); the device fetches the record over the
API. Nothing host-written or agent-written passes through Apple or Google.

### 3.4 Agents: `notifications_list`

A host built-in tool, beside `post_message` and `show`
(`vam-spec/src/outbox.rs:11-19`), side effect `READ_ONLY`.

- **Arguments:** `kind` (optional list), `limit` (default 10, max 20).
- **Returns:** the calling run owner's **unresolved and unread** records:
  `id`, `kind`, `severity`, `title`, `body`, `run_id`, `thread_id`,
  `approval_id`, `agent_label`, `via`, `created_at`, and the action as a link
  and a CLI line. Capped at 4 KiB. **Host tier only**: `agentText` is never
  returned to an agent, so one agent's text cannot reach another agent's
  context through this tool.
- **Opt-in:** not in `OUTBOX_OVERLAY` (`security.rs:160-190`). An agent gets
  it by listing it (`builtins: [notifications_list]`, or a permission row
  `tool:notifications_list` allow); the shipped primary agent lists it.
- **Only in a run a person started** (the same reading as the primary
  agent's loop guard P-A): a scheduled or asked run cannot read the user's
  notifications, so a run nobody watches cannot collect them.
- **Read-only, and the only agent surface.** There is no tool to mark read,
  mark acted, decide, answer or renew. Reading through the tool does not set
  `read_at` (the agent seeing it is not the user seeing it). The primary
  never approves, decides a gate or supplies a credential
  (`design/primary-agent.md` §14.1); it names the item and the user decides.

### 3.5 Channels and the `vam.notify` broker (slice 2)

Slack, then email and push, are **channels**: a way to deliver a record and,
for Slack, to take a decision back. A channel is served by the first-party
broker **`vam.notify`**: a Rust crate `brokers/vam-notify`, its own process,
HSP only, depending on `vam-spec` and shared libraries, never on
`vam-orchestrator` (E10 locked decision 22). Vendor credentials, the Slack
connection and network listeners live in the broker (E10 locked decision 15);
who, whether and what is recorded stays in `vamd`.

**Data in `vamd`:**

| Table | Holds |
| --- | --- |
| `notification_channels` | `id` (`nc_…`), `user_id`, `channel` (`slack`, later `email`, `push`), `address` (Slack team id + user id; email address), `display`, `linked_at`, `verified_at`, `revoked_at` |
| `notification_prefs` | per user: for each kind (or `*`), which channels and `mode` (`immediate`, `digest`, `off`); `quiet_hours {start, end, tz}`; `digest_at` |
| `notification_deliveries` | `notification_id`, `channel_id`, `state` (`pending`, `sent`, `failed`, `suppressed`, `held`), `attempts`, `not_before`, `external_ref` (the Slack message ts, to update it later), `last_error` |

**Defaults** (no preference set): `approval.pending`, `question.asked`,
`gate.opened`, `trigger.skipped`, `grant.expiring`, `budget.hit` and
`report.arrived` immediate to Slack once linked; `run.finished` warn
immediate, info off; `trigger.fired` off. Quiet hours hold every external
delivery (a parked run costs nothing while it waits, `design/benchmark.md:95`);
held deliveries go out at the end as one message.

**Delivery (`vamd` → broker).** Rows in `notification_deliveries` are created
in the same transaction as the record, from the user's preferences. A
delivery worker in `vamd` (a loop beside the receipt sweeper) sends due rows
with a new RPC on the broker's existing HSP endpoint:

```protobuf
// HostedBroker (hosted_services.proto)
rpc DeliverNotification(DeliverNotificationRequest) returns (DeliverNotificationResponse);

message DeliverNotificationRequest {
  string delivery_id = 1;                  // idempotency key; the broker dedupes
  string channel_address = 2;              // the linked Slack user (DM only)
  Notification notification = 3;          // the record: host fields, plus agent_text fenced and labelled
  string action_handle = 4;                // single-use, for decision kinds only (§3.6)
  repeated string allowed_decisions = 5;   // e.g. APPROVED, REJECTED; gate option labels
  string update_of = 6;                    // external_ref: edit this message (resolved elsewhere)
  map<string, string> resolved_secrets = 7;// the bot token, per call, as InvokeRequest does (:213)
}
```

At-least-once, backoff to 24 h, then `failed` (the app list still has it).
When a record is resolved, `vamd` queues an update delivery for every
`sent` row ("Decided by alice in the app at 09:12"; buttons removed). Per
user and channel at most 30 immediate deliveries an hour; the rest fold into
one "and 12 more" message.

`on_paused` hooks (`hsp_hooks.rs:445`) stay as they are: a broker hook that
can veto or log a pause. `vam.notify` does not subscribe to them; a hook
would make the broker the one deciding what to tell whom.

**Taking a decision back (broker → `vamd`).** The broker's gateway socket
(`<state_dir>/hsp/gateway/<broker_id>.sock`, identity = the socket,
`design/hsp-triggers.md` amendment §5) is opened today only for a manifest
with `run_request_permission` (`gateway.rs:179`). Slice 2 also opens it for a
manifest with a **notification channel** block, and serves a second service
on it:

```protobuf
// CapabilityManifest
NotificationChannel notification_channel = 11;
message NotificationChannel {
  string channel = 1;                 // "slack"
  string surface = 2;                 // receipt surface, [a-z]{3,16}; not cli|app|api|system
  int32 max_actions_per_minute = 3;   // per broker; 0 = 60
}

// Served by vamd on the same per-broker socket.
service HostedNotificationGateway {
  rpc ActOnNotification(ActOnNotificationRequest) returns (ActOnNotificationResponse);
  rpc LinkChannelAccount(LinkChannelAccountRequest) returns (LinkChannelAccountResponse);
  rpc GetChannelConfig(GetChannelConfigRequest) returns (GetChannelConfigResponse);
}
```

`GetChannelConfig` hands the broker its Slack app-level token at start, in
memory only, over the socket that identifies it (§8 q5): the broker keeps no
credential on disk, as `vam-web` takes its key per invoke.

### 3.6 Deciding from Slack, with provenance

The broker relays a person's click; it never decides on its own authority.

1. When a decision-kind record is delivered, `vamd` mints an **action
   handle**: a Credential row (`api_keys`, holder `broker:vam.notify`,
   `credentials.rs`), `vam_na_<id>.<secret>`, SHA-256 stored, bound to
   `(notification id, approval id, user id, channel id, allowed decisions)`,
   single use, expiring with the approval. It is sent only in
   `DeliverNotification` to that broker. The broker keeps it in its own store
   (0600) and puts only an opaque delivery id in the Slack button value.
2. The user clicks Approve in their Slack DM. The broker verifies the Slack
   request (Socket Mode, §8 q4) and calls `ActOnNotification {handle,
   external_account {team_id, user_id}, decision, response}`.
3. `vamd` checks, in order: the socket's broker is approved, tier core or
   first-party, manifest has `notification_channel` (else
   `NOT_PERMITTED`); the handle parses, matches, is unused and unexpired
   (`INVALID`, `EXPIRED`); the record is unresolved (`ALREADY_RESOLVED`, with
   who and where, so the broker can update the message); the Slack account is
   a **linked, active channel of the handle's user** (`NOT_LINKED`); the user
   is active (`users.rs:140`); the decision is in the handle's allowed set
   and the gate response validates (`validate_gate_response`); the rate limit.
4. `vamd` decides through **the same function the HTTP route uses**: the
   body of `decide_approval` (`api/mod.rs:1578`) is extracted into one
   `decide_as(state, DeciderCaller { user_id, surface }, id, request)`, called
   by the route (surface from `X-Vam-Surface`) and by the gateway (surface =
   the manifest's `notification_channel.surface`, `slack`). No copy of
   `decide_approval`, as `start_run` has no copy of `create_run`
   (`work-outside-chat.md` locked decision 8). The approver check
   (`HitlApprover::allows_decider`, `hitl.rs:83`), the stored provenance and
   the resume are unchanged.
5. The approval row records `decided_by_user_id` = the linked user and
   `decided_surface` = `slack`; the notification row copies them; other
   surfaces clear.

`slack` is never accepted from `X-Vam-Surface`: `surface_from_header`
(`run_facts.rs:25-31`) maps anything but `cli` and `app` to `api` today, and
stays so. Only the gateway path can write a channel surface.

**As built (NT-2.4).** The handle is an `api_keys` row (`issued_via = notification`, holder
`broker:<id>`, SHA-256 only), revoked in the decision's own transaction (`notifications::resolve`),
on unlink and when the broker asks for its config at start. `GetChannelConfig {channel, purpose}`:
`connect` returns the app and bot tokens and re-mints (an edit of each sent, still-waiting post
carrying a new handle); `reply` returns the bot token only and changes nothing. The broker asks per
reply and drops the token when the reply is sent. A linked account presenting another user's handle
answers `INVALID`; an unlinked account answers `NOT_LINKED`.

**Not from Slack:** "always allow" or "always deny" (a standing rule is set
where its scope can be read: app or CLI); free-text answers to a question or
a gate form with fields (Slack shows an Open button instead); supplying a
credential. Tool approvals show the tool name, the deciding rule and the
argument preview the app shows, capped at 1 KiB, fenced and labelled as
agent-written.

**Linking an account.** In the app or with `vam notifications link slack`,
the user gets a ten-minute code bound to them. They send it to the bot in a
Slack DM; the broker calls `LinkChannelAccount {code, external_account}`;
`vamd` binds the channel row to the user who minted the code. Unlink in the
app or CLI revokes the channel and every open handle for it. Delivery goes to
the user's DM only, never to a shared Slack channel.

**`notify_send`** (an agent tool on the same broker: post a message to an
operator-allowed Slack channel, `MUTABLE_EXTERNAL`) is what the close-prep
playbook's notify step calls (`design/run-receipt.md:126`). It is an effect
of the run, journaled like any effect; it is **not** a notification and
writes no record.

## 4. Scope rules

1. **Per user, never cross-user.** Every read and write goes through one
   function taking `(user_id, id)` that answers "not found" for a foreign
   id, as Spaces do (`work-outside-chat.md` locked decision 10). A record has
   exactly one recipient; an event for two users is two rows.
2. **Org inbox only in E3.** An org admin's view of everyone's pending
   approvals is the E3 governance console ("one approvals inbox",
   `design/agent-experience.md:195`). Not designed here; nothing in this
   module gives an admin another user's rows.
3. **Dedupe and rate.** `subject_key` per kind: `approval.pending` and
   `question.asked` by `(run_id, tool_name, args_digest)` while unresolved, so
   a call that re-suspends after a restart or retry updates the open row
   (new `approval_id`, `count + 1`, `updated_at`) instead of adding one;
   `gate.opened` by the gate request id (`{run}/gate/{step}`, stable,
   `workflow.rs:4247`); `run.finished`, `budget.hit` by run id;
   `report.arrived`, `delegation.*` by child run id; `grant.expiring` by grant
   id; `trigger.*` by grant id and UTC day. Channel rate in §3.5.
4. **Retention.** A resolved or read `info` record is pruned 30 days after
   it was resolved or read; `warn` after 90; an unresolved `action` record
   is kept while its approval exists. Hard cap 2,000 rows per user (oldest
   resolved first). Deliveries are pruned with their record. A run deletion
   (none today) deletes its records in the same transaction.
5. **Restart.** Nothing is lost: records are in the store, written in the
   fact's transaction or by a sweeper that reconciles at start
   (`bin/vamd.rs:452`); deliveries are rows the worker resumes; Slack
   deliveries are idempotent by `delivery_id`.
6. **Never a steer.** A notification never enters a thread, an inbox or a
   model context. Report back is the primary agent's own queued task
   (`design/primary-agent.md` §5); its notification sits beside it.

## 5. What the receipt shows

The surface of each decision, which E12 Phase 1 already records and prints
(`decided_surface`, "receipt-names-who-decided-and-from-where"): slice 2 adds
the value `slack`, written only by the gateway path. Nothing else: no
delivery facts, no read times, no notification ids on the receipt.

## 6. Slices

### Slice 1: the record and the list (E12 Phase 2, first half)

**Outcome:** every pending approval, gate and question of a user, every
background run that finished, and every report back has one record the user
can list in the app and the CLI, and the primary agent can read.  
**Depends on:** E5 Phase 1 (on `main`: Space and thread ids, `runs.source`),
E12 Phase 1 (on `main`: decision provenance).  
**Exit:** a run in another thread parks on an approval; the user's list and
badge show it within one poll; deciding it from the CLI clears it with
surface `cli`; an agent with `notifications_list` reads it and the approval
stays pending.

| Scenario | Layer | Shows |
| --- | --- | --- |
| `approval-in-another-thread-reaches-the-list` | process | park → one unread `action` record with run, thread, approval ids |
| `decision-anywhere-clears-the-notification` | process | decide via CLI and via API with `app` → acted, surface recorded, summary drops |
| `gate-left-open-shows-until-it-times-out` | process | `gate.opened`, then `resolved_reason: timed_out` |
| `background-run-end-notifies-and-chat-turn-does-not` | process | triggered run → `run.finished` with digest; a watched chat turn → none |
| `retried-approval-keeps-one-notification` | process | re-suspend after restart → one row, `count` 2, still listed after restart |
| `notifications-are-never-shown-to-another-user` | process | bob lists none of alice's; get by id is 404 |
| `agent-reads-what-waits-without-acting` | process | `notifications_list` returns the record; no read, no decision; a triggered run is refused the tool |
| `badge-counts-what-waits-on-you` | app (the VAM app) | badge, list, card drawn, cleared after decide |

| Item | Decide |
| --- | --- |
| NT-1.1 Table, `record()`, kinds, dedupe, prune | `Decide: SLICE — proof 3 (row 7: the Slack gate rides this record), cost 2 (new table and API) → slice: the four slice-1 producers; other kinds in slice 2. Line: P2 pending decisions listed 100%` |
| NT-1.2 Producers: approval, question, gate (in the transaction), resolution paths | `Decide: NOW — proof 2 (P2 line), cost 1 (files the item touches already hold the fact)` |
| NT-1.3 Producer: `run.finished` in `issue_receipt` and the sweeper | `Decide: NOW — proof 2 (P7 line: results reach the user), cost 1` |
| NT-1.4 `report.arrived` and `delegation.*` kinds and function (called by primary agent Phase 1) | `Decide: SLICE — proof 1 (no 0.1.0 line; primary agent dependency), cost 0 → ship the kind and the function only` |
| NT-1.5 API: list, summary, get, read, acted; PDP actions | `Decide: NOW — proof 2 (P2 line), cost 2 → the five routes only` |
| NT-1.6 CLI `vam notifications` | `Decide: NOW — proof 2 (scenarios run through it), cost 1` |
| NT-1.7 `notifications_list` host built-in, opt-in, person-started runs only | `Decide: SLICE — proof 1 (primary agent; proposed P7 latency line), cost 1 → read-only tool only` |
| NT-1.8 the VAM app badge and list with existing cards, `X-Vam-Surface: app` | `Decide: NOW — proof 2 (P2: the inbox the user sees), cost 1 (one app wave)` |

**Benchmark lines moved:** P2 "an approvals inbox covering every run", as a
proposed line: **pending decisions present in the user's list (must be
100%)**. P7, proposed: **decisions the user learned about within a minute**:
share of `action` records whose `first_shown_at - created_at ≤ 60 s` on a
surface the user has open, and the median time from created to acted.
Requests to the benchmark lane (Appendix).

### Slice 2: Slack, preferences, the remaining kinds (E12 Phase 2, second half)

**Outcome:** a user who linked Slack gets their approvals, gates, questions,
skipped schedules, expiring grants and budget stops as DMs, can approve,
deny or choose a gate option there, and the receipt says `slack`.  
**Depends on:** slice 1; E5 Phase 1 gateway (on `main`).  
**Exit:** the E12 Phase 2 exit: "a gate decided from the notify surface shows
that surface in the receipt".

| Scenario | Layer | Shows |
| --- | --- | --- |
| `gate-decided-from-slack-names-slack-on-the-receipt` | process (fake Slack transport in the broker) | DM delivered, option clicked, run resumes, receipt `surface: slack` |
| `decision-in-the-app-updates-the-slack-message` | process | decide in app → delivery update, buttons gone |
| `slack-click-from-an-unlinked-account-is-refused` | process | `NOT_LINKED`, approval still pending |
| `channel-broker-cannot-decide-what-it-was-not-sent` | process | forged or reused handle, another broker's socket → refused |
| `quiet-hours-hold-slack-until-morning` | process (simulated clock) | held, then one message; the app list unaffected |
| `expiring-grant-and-budget-stop-reach-the-owner` | process | `grant.expiring` once; `budget.hit` once per run |
| `skipped-schedule-is-told-once-a-day` | process | `trigger.skipped` per grant per day, `count` grows |

| Item | Decide |
| --- | --- |
| NT-2.1 `brokers/vam-notify` crate: Slack Socket Mode client, DM delivery, message update, fake transport for scenarios | `Decide: NOW — proof 3 (demo: gate decided from Slack), cost 3 → slice: Slack only, DMs only` |
| NT-2.2 Contract: `DeliverNotification`, `NotificationChannel` (manifest 11), `HostedNotificationGateway` | `Decide: NOW — proof 3 (demo), cost 2` |
| NT-2.3 Delivery worker, deliveries table, retries, updates on resolve, rate fold | `Decide: NOW — proof 3 (demo), cost 2` |
| NT-2.4 `decide_as` extraction; gateway `ActOnNotification` with the checklist; action handles | `Decide: NOW — proof 3 (demo; P1 floor: no broker authority), cost 2` |
| NT-2.5 Account linking (code, `LinkChannelAccount`, unlink) | `Decide: NOW — proof 3 (no Slack decision without it), cost 1` |
| NT-2.6 Preferences and quiet hours (API, CLI, app settings row) | `Decide: SLICE — proof 2 (P7), cost 2 → defaults plus per-kind on/off and quiet hours; digests in slice 3` |
| NT-2.7 Producers: `grant.expiring`, `budget.hit` (both points), `trigger.fired`, `trigger.skipped` | `Decide: NOW — proof 2 (P7, P8 runaway task), cost 1` |
| NT-2.8 `notify_send` agent tool | `Decide: SLICE — proof 3 only if the demo keeps close-prep's notify step, cost 1 → build with NT-2.1, else BACKLOG` |

**Benchmark lines moved:** P7 "tasks started by a trigger and decided from a
notify surface that complete" (`design/benchmark.md:95`), the notify half;
the proposed P7 latency line on Slack.

### Slice 3: email, push, digests (later, not 0.1.0)

**Outcome:** the same records by email and as a content-free push to phones
and browsers; daily or hourly digests.  
**Depends on:** slice 2; a VAM app mobile or web shell that can register.  
**Exit:** a registered device is woken for an approval and opens it; a digest
collects the day's `info` records.

| Scenario | Layer |
| --- | --- |
| `push-registration-is-listed-and-revocable` | process |
| `push-wakes-the-device-without-content` | process (fake push transport) |
| `daily-digest-collects-the-days-results` | process (simulated clock) |
| `email-link-opens-the-decision-in-the-app` | process |

| Item | Decide |
| --- | --- |
| NT-3.1 Push registration API and table | `Decide: BACKLOG — proof 1 (no 0.1.0 line), cost 1; trigger: a mobile or web shell exists` |
| NT-3.2 APNs, FCM, Web Push adapters in `vam.notify` | `Decide: CUT for 0.1.0 — proof 1, cost 3; number that changes it: a design partner on mobile` |
| NT-3.3 Email adapter (SMTP, link only) | `Decide: BACKLOG — proof 1, cost 2` |
| NT-3.4 Digests | `Decide: BACKLOG — proof 1, cost 1` |

## 7. Placement

Row 7 of `design/launch-0.1.0.md` §4 is this module, slices 1 and 2. It is
**blocked by** E5 Phase 1 (row 6a: the gateway socket, credential checks,
Space and thread ids), which is on `main`. It **unblocks**: primary agent
Phase 1 (row 16, by slice 1 only); the live demo's Slack gate (with row 6b,
by slice 2); B2's parked-cost row (row 11: the three-day gate decided from a
notify surface on a simulated clock, `design/benchmark.md:95`).

**Conflict with other lanes.** Slice 1 edits `workflow.rs` (approval and
gate paths), `receipt.rs` and `api/mod.rs`; slice 2 edits `gateway.rs`,
`hosted_services.proto` and `api/mod.rs` (`decide_as`). Primary agent Phase 1
shares `gateway.rs` and the receipt builder: its build waves follow slice 1,
as row 16 says.

**Tracker.** A new tracker `.cursor/workstreams/notifications.md`, slug
`notifications`, prefix `NT`, base `feat/notifications`, in the E12 lane
(row 7, same orchestrator). The E12 tracker continues for the receipt: its
Phase 2 keeps the app receipt view and points its two Slack items here
(Appendix). Reason: the module has its own locked decisions, three slices and
consumers outside the receipt (the primary agent); folding it into
`run-receipt.md` would hide it from the lanes that depend on it.

## 8. Locked decisions (proposed), open questions, not building

### Locked decisions (proposed)

1. **One record, derived.** `vamd` writes a notification at the moment it
   records the underlying fact, in the same transaction where one exists, or
   from the sweeper that reconciles that fact. No second event bus; no new
   observation.
2. **No broker produces a record.** Brokers deliver and relay; only `vamd`
   decides what the user is told.
3. **Per user.** One recipient per row; foreign ids are "not found"; no
   admin view before E3.
4. **Two tiers.** The row holds host facts; agent text is attached at read
   time, fenced and labelled, and never given to an agent.
5. **Acted clears everywhere; deciding has one path.** Decision kinds clear
   only through the approval decision (`decide_as`), whatever the surface;
   the notification API never decides.
6. **A channel decision is a person's action.** It needs a `vamd`-minted
   single-use handle delivered to that broker, a Slack account the user
   linked, and the approver check; the surface comes from the
   operator-approved manifest, never from a client header.
7. **Agents read, never act.** `notifications_list` is read-only, opt-in,
   host tier only, and absent from runs a person did not start. No tool
   marks, decides, answers or renews.
8. **Never a steer.** A notification never enters a thread, inbox or model
   context.
9. **Push carries no content; Slack goes to the user's DM only.**

**Locked rows this touches.** E10 locked decision 9 ("Delivery: the app plus
the existing `on_paused` hooks. Slack and email stay in E5") was already
overtaken by the 2026-10-29 amendment (Slack in E12 Phase 2). Proposed
wording for the owner: "Delivery: the app, the existing `on_paused` hooks,
and the notification module's channels (`design/notifications.md`); Slack
first." Not applied.

### Open questions, each with a default

| # | Question | Default |
| --- | --- | --- |
| 1 | Live badge: poll or stream? | Poll `summary` every 15 s with an etag in slice 1; a per-user stream later if the latency line misses. |
| 2 | Which runs produce `run.finished`? | The §2.3 rule. |
| 3 | Is `notifications_list` on by default for every agent? | No: opt-in; the shipped primary lists it. |
| 4 | Slack connection: HTTP request URL or Socket Mode? | Socket Mode: outbound only, works on a single host behind NAT (the 0.1.0 install). |
| 5 | How does the broker get its Slack tokens? | Operator secrets in `vamd`; the bot token per `DeliverNotification` (`resolved_secrets`), the app-level token once at start through `GetChannelConfig` on its socket, memory only. |
| 6 | Tool approvals in Slack: show arguments? | Yes, the app's preview, ≤ 1 KiB, fenced as agent text; "always" not offered. |
| 7 | Questions from Slack? | Single-choice options as buttons; anything else opens the app. |
| 8 | Quiet hours hold approvals too? | Yes (parked runs cost 0); a user may mark kinds "always". |
| 9 | Recipient when the approver is not the owner? | The approver gets the `action` record, the owner an `info` one. Moot until reviewers are minted (`hitl.rs:25-29`). |
| 10 | Where do links in Slack point? | The node's public app URL from node config; if unset, buttons plus the CLI line `vam approve <id>`, no link. |
| 11 | Retention numbers | §4 rule 4 (30 / 90 days, 2,000 rows). |
| 12 | New tracker or the E12 tracker? | New `notifications` tracker in the E12 lane. |

### What we are not building

- **A second event bus.** The record is a per-user projection of facts
  `vamd` already records; run timelines stay on the outbox.
- **An org inbox before E3.** No admin view of other users' records.
- **A notification as a steer** into a running task, or as a message in any
  thread.
- **A broker producing records**, or deciding on its own authority.
- **An agent acting on a notification**: no approve, decide, answer, renew,
  mark read or mark acted by any agent.
- Shared Slack channels as a delivery target, decide-by-email-reply, "always
  allow" from a channel, content in push payloads.
- Delivery facts on the receipt.

---

## 9. Owner review, 2026-11-02: adapters, provenance, jigs, configuration, contacts

Added after the owner read the design. These amend §3.5, §3.6 and §6; the
phase breakdown absorbs them as items. All approved by the owner.

### 9.1 Channels are adapters; Slack is the first, not the design

Slack is a connector like any other. `vamd` produces **one neutral
rendering** per record (title, body, the actions, each with its one-time
handle); nothing in `vamd` knows a channel's format. The `vam.notify` broker
holds a **registry of channel adapters** behind one interface:

| Method | Direction | Does |
|---|---|---|
| `capabilities()` | adapter → broker | the jig primitives it renders natively, whether it can take a decision back, whether it needs a verified contact |
| `deliver(record, rendering, binding, credentials)` | broker → channel | sends; returns a delivery result (channel message id, status) the broker keeps in its own delivery log |
| `receive(event)` | channel → broker | turns a channel event into a decision carrying the handle, which the broker relays to `vamd` through the gateway |

Slack is the first adapter module; Teams, email, web push and phone push
are later modules in the same crate, each with its own credentials in the
vault, enabled per node (§9.4). An organisation enables what it has. The
boundary already allows a channel to move into its own process later:
`vamd` only ever says "deliver this rendering" and receives "a decision with
this handle". Third-party brokers stay barred, as E5 locked.

### 9.2 Provenance: delivery facts in the broker, decision facts in `vamd`

Two kinds of fact, two homes. **Delivery facts** (which adapter, the
channel's message id, sent at, retries, failures) belong to the broker's
delivery log; `vamd` reads them only as a status line. **Decision facts**
(who decided, where, when) belong to `vamd`, because the surface of a
decision goes on the receipt and the receipt holds only what the host can
vouch for. `vamd` vouches through the handle: it issued that one-time handle
for that record, that user and that channel, so when the decision returns
with it through the gateway, `vamd` records the surface from the handle it
issued, never from the broker's claim. A buggy or compromised adapter cannot
write "approved in the app" over a Slack click. §3.6 reads as this.

### 9.3 Jigs in a channel: native when simple, a link when not

Each adapter declares the jig primitives it renders (`capabilities()`). A
jig renders natively in a channel only when every component of its IR is
in that set and its only action is a submit (no live queries against VAM
tools, no mutation other than the final submit). Otherwise the channel gets
the host-written title and body and one button, **Open in VAM**, a deep
link into the interaction in the app. The link is present even when the
form renders natively, so a person can switch. Slack's set in slice 2:
text, buttons, radio, checkboxes, a static select of up to 100 options, a
text input, a date picker; that covers every built-in jig (approve,
question, playbook start, a plain gate form). A channel submit goes to
`vamd` with the handle and is validated against the result schema exactly as
an app submit is (`design/jig.md` decision 14); a channel never submits what
the form did not offer. The adapter interface carries the jig IR as its
rendering input from slice 2, so later adapters inherit it.

### 9.4 Channel configuration: operator settings in `vamd`, credentials in the vault

The pattern is the web broker's: its search key arrives per invoke from
`vamd` and is never read from env or disk. A node setting for notifications
beside the model allowlist lists the enabled adapters and, per adapter, the
vault secret refs it needs (`notify/slack/bot-token`, `notify/slack/app-token`,
`notify/smtp/password`, ...) plus org defaults for which kinds are on; set
through `PUT /v1/node/notify` and `vam node notify`, shown on the operator
page of the app; users bind only to enabled adapters. On each delivery
`vamd` passes the adapter's secrets with the invoke; the broker holds no
credentials between calls. Slice 2.

### 9.5 Contacts: an address is a verified contact, not a login field

Users have no email today, and should not get one as a login field. A
per-user **contacts** record in `vamd` (user, channel, address, verified at)
holds a Slack account id, an email address, a Teams user id or a device
token. The user adds one in the app or CLI; `vamd` issues a code; the
adapter sends it; the user confirms. Only verified contacts receive
anything. Records and receipts keep the user id, never the address.
**Email is one-way:** it notifies and carries the Open in VAM link; no
decision by reply or by clicking a link in an email (a decision on a link
click is a decision by anyone holding the email). The operator's relay
(SMTP, SES, Graph) is configured as the email adapter's credentials (§9.4).
Push registrations are contacts too, held by `vamd` and handed to the push
adapter per delivery. Account creation by email invite is a later item
under users and roles, not part of this module.

### 9.6 Confirmed rules

- `run.finished` is recorded only for background runs, runs that waited on
  the user, and failures (§2.3). A chat reply the user was watching is not
  a notification.
- Quiet hours hold approvals too (a waiting run costs nothing); kinds can be
  marked "always".
- Standing rules ("always allow") are set only in the app or CLI, never from
  a channel.
- Slack connects through Socket Mode (outbound only).

### 9.7 Changes to the slices

- Slice 1 unchanged (the record, API, badge, list, CLI, `notifications_list`).
- Slice 2 adds: the adapter interface and registry, the Slack adapter as its
  first module, the node notify settings and vault refs (§9.4), the contacts
  record with the code flow for Slack account linking (§9.5), the jig
  rendering rule (§9.3), and the delivery log. Scenarios added:
  `channel-adapter-renders-a-simple-jig-natively`,
  `complex-jig-falls-back-to-open-in-vam`,
  `decision-surface-comes-from-the-handle-not-the-adapter`,
  `unverified-contact-receives-nothing`.
- Slice 3 (later): the email adapter (one-way), Teams, push delivery,
  digests.

## Appendix: amendments (applied 2026-11-02 with the owner's approval, except where marked)

**`design/run-receipt.md` §3, Phase 2 row** (not applied):

> | **2 Surfaces** | The same receipt everywhere; every decision names its surface | App receipt view (built-in jig); the run-finished notification links the receipt, delivered by `vam.notify` (`design/notifications.md` slices 1 and 2); gate decisions from Slack recorded with surface `slack` through the gateway, never from a client header | Phase 1, E5 Phase 1, notifications slice 1 | Scenario: a gate decided from Slack shows surface `slack` in the receipt |

**`design/launch-0.1.0.md` §4 row 7** (not applied), Phase column:

> **E12 Phase 2: notifications, one record, many surfaces** (`design/notifications.md`, P-D): slice 1, the record, the list in the app and CLI, the `notifications_list` tool; slice 2, `vam.notify` with Slack as the first channel, deciding from Slack with surface `slack`, preferences; plus the app receipt view. Unblocks: the live demo (with 6b), row 16 (slice 1), B2 parked cost (slice 2).

**`.cursor/workstreams/run-receipt.md` Phase 2** (not applied): keep "App
receipt view"; replace the two other items with "Slack short form and gate
decisions from Slack: `notifications` tracker slice 2 (NT-2.1 to NT-2.4); this
phase's exit is met by `gate-decided-from-slack-names-slack-on-the-receipt`".

**`.cursor/workstreams/work-outside-chat.md` "Explicitly later"** (not
applied): replace the `vam.notify` bullet with "Notifications and
`vam.notify` (`design/notifications.md`, `notifications` tracker; slice 2
reuses this lane's gateway socket and credential checks and serves
`HostedNotificationGateway` on it)."

**`.cursor/workstreams/agent-runtime.md` locked decision 9**: wording in §8
(owner's word; a locked row).

**Benchmark lane** (`design/benchmark.md` §1, not applied): P2 add "pending
decisions present in the user's list (must be 100%)"; P7 add "decisions the
user learned about within a minute (share; median time to act)".

**`design/README.md`** (applied with this doc): one line under the plans of
record.
