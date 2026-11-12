# Workstream: Notifications (one record, many surfaces)

```yaml
status: active          # active | paused | done
slug: notifications
current_phase: 2
base_branch: feat/notifications   # the VAM app: feature/notifications
lane: E12 (row 7 of the 0.1.0 launch scope)
design_docs:
  - design/notifications.md
  - design/run-receipt.md          # decision 4, §1.1 (decision provenance)
  - design/primary-agent.md        # §5, §14.2 (consumers of slice 1)
```

## Goal

When something needs the user, or something they left running is done, `vamd`
writes one per-user notification record, in the same transaction (or the same
reconciling sweeper) that records the underlying fact. The user sees it as a
badge and a list in the app and in `vam notifications`; the primary agent can
read it through the read-only `notifications_list` tool; deciding an approval or
gate from any surface clears it everywhere and the receipt names that surface.
Slice 2 adds channel adapters (Slack first) through the `vam.notify` broker;
slice 3 adds email, push and digests. Done means the three slices' scenarios are
active and green and row 7 of the 0.1.0 launch scope is met.

## Out of scope

- Channels and channel adapters (Slack, Teams, email, push), the `vam.notify`
  broker, `DeliverNotification` / `HostedNotificationGateway`, action handles,
  `decide_as` extraction: slice 2 (Phase 2 here), not slice 1.
- Contacts and account linking, node notify settings and vault refs,
  preferences and quiet hours, the delivery log: slice 2.
- Producers for `grant.expiring`, `budget.hit`, `trigger.fired`,
  `trigger.skipped`: slice 2.
- Push registrations, email, digests: slice 3.
- An org-wide approvals inbox (E3), a notification as a steer into a thread,
  any agent acting on a notification.
- Building the primary agent (`design/primary-agent.md`); slice 1 ships the
  `report.arrived` / `delegation.*` kinds and the function it will call.

## Locked decisions

Do not re-litigate without editing this list. Decisions 1 to 9 copied from
`design/notifications.md` §8, **approved 2026-11-02** by the owner.

1. **One record, derived.** `vamd` writes a notification at the moment it
   records the underlying fact, in the same transaction where one exists, or
   from the sweeper that reconciles that fact. No second event bus; no new
   observation.
2. **No broker produces a record.** Brokers deliver and relay; only `vamd`
   decides what the user is told.
3. **Per user.** One recipient per row; foreign ids are "not found"; no admin
   view before E3.
4. **Two tiers.** The row holds host facts; agent text is attached at read time,
   fenced and labelled, and never given to an agent.
5. **Acted clears everywhere; deciding has one path.** Decision kinds clear only
   through the approval decision (`decide_as`), whatever the surface; the
   notification API never decides.
6. **A channel decision is a person's action.** It needs a `vamd`-minted
   single-use handle delivered to that broker, a Slack account the user linked,
   and the approver check; the surface comes from the operator-approved
   manifest (the handle `vamd` issued, §9.2), never from a client header.
7. **Agents read, never act.** `notifications_list` is read-only, opt-in, host
   tier only, and absent from runs a person did not start. No tool marks,
   decides, answers or renews.
8. **Never a steer.** A notification never enters a thread, inbox or model
   context.
9. **Push carries no content; Slack goes to the user's DM only.**

**Owner's confirmed rules (design §9.6, approved 2026-11-02):**

10. `run.finished` is recorded only for background runs, runs that waited on the
    user, and failures (§2.3). A chat reply the user was watching is not a
    notification.
11. Quiet hours hold approvals too (a waiting run costs nothing); kinds can be
    marked "always". (Slice 2.)
12. Standing rules ("always allow") are set only in the app or CLI, never from a
    channel.
13. Slack connects through Socket Mode (outbound only). (Slice 2.)
14. **Provenance split (§9.2):** delivery facts live in the broker's delivery
    log; decision facts (who, where, when) live in `vamd`, and the surface of a
    decision is recorded from what `vamd` itself authenticated (the client's
    channel in slice 1, the handle it issued in slice 2), never from a body
    field or a broker's claim.

## Status legend

- `[ ]` not started
- `[~]` in progress
- `[x]` done (code + tests, or merged)

## Phases

### Done phases (archived)

Full sections: not kept; see the progress log and git history.

| Phase | Name | Done |
|---|---|---|

### Phase 1 — Slice 1: the record and the list

**Goal:** every pending approval, gate and question of a user, every
background run that finished, and every report back has one record the user can
list in the app and the CLI, and the primary agent can read.  
**Depends on:** E5 Phase 1 (`runs.source`, Space and thread ids) and E12 Phase 1
(decision provenance), both on `main` .  
**Status:** released to `main` 2026-11-02; checklist out. **Plan:** approved 2026-11-02 (owner): recommendations 1 (the ui scenario `badge-counts-what-waits-on-you` stays `needs: ui` and pending; proven by the VAM app unit tests and the close checklist; the close is seven process scenarios green plus that proof), 2 (spec change accepted: `restarted-approval-keeps-one-notification` retries after a deny, then restarts) and 3 (wave 1 may split the routes into 1b if its plan does not fit one session); every open-question default accepted  
**Exit:** a run in another thread parks on an approval; the user's list and
badge show it within one poll; deciding it from the CLI clears it with surface
`cli`; an agent with `notifications_list` reads it and the approval stays
pending. Eight scenarios (`design/notifications.md` §6) active and green; the two
benchmark lines (P2 pending decisions listed, P7 learned within a minute)
recorded on the mock.

- [x] NT-1.1 Table, `record()`, kinds, dedupe, prune
- [x] NT-1.2 Producers: approval, question, gate (in the transaction), resolution paths
- [x] NT-1.3 Producer: `run.finished` in `issue_receipt` and the sweeper
- [x] NT-1.4 `report.arrived` and `delegation.*` kinds and function (no caller yet)
- [x] NT-1.5 API: list, summary, get, read, acted; PDP actions and scopes
- [x] NT-1.6 CLI `vam notifications`
- [x] NT-1.7 `notifications_list` host built-in, opt-in, person-started runs only
- [x] NT-1.8 the VAM app badge and list with existing cards, `X-Vam-Surface: app`

#### Phase 1 breakdown (phase-breakdown agent, 2026-11-02; awaiting owner approval)

Written against `feat/notifications` at `4063516` (= `main` plus the
tracker). Line numbers are that tree. Where the code and the design differ,
the code is the fact; the differences are listed under **Design vs `main`**.

##### Scenario delta

Seven process scenarios and one app scenario, as approved in design §6. Two
new world pieces. No existing scenario's `then` changes.

| Scenario | Change | Layer | Given | Why |
|---|---|---|---|---|
| `approval-in-another-thread-reaches-the-list` | new | process | `org-admin`, `agent-plain` (the user chats with it in thread A), `agent-with-guarded-tool` (its run parks in thread B) | Phase exit. Parking writes one unread `action` record (`approval.pending`) with run, Space, thread and approval ids; `summary.action` is 1. The record's `approvalId` equals the pending approval's `requestId` |
| `decision-anywhere-clears-the-notification` | new | process | `org-admin`, `agent-with-guarded-tool` (two runs) | Run 1 is decided with `approve: { via: cli }` and its record shows `actedSurface: cli`. Run 2 is decided through the API with `X-Vam-Surface: app` and shows `app`. In both, `resolvedReason: acted`, the summary drops, and the receipt's tool line names the surface. `POST …/acted` on a pending decision kind answers `409 decide_required` and the approval stays pending |
| `gate-left-open-shows-until-it-times-out` | new | process | `org-admin`, `agent-plain`, **new** `playbook-with-timed-gate` (gate `timeout: 3s`, `on_timeout: stop`) | `gate.opened` listed, then `resolvedReason: timed_out`, `actedSurface` absent. The 60 s sweeper drives it (open question 5) |
| `background-run-end-notifies-and-chat-turn-does-not` | new | process | `org-admin`, `agent-with-trigger-tool`, `trigger-test-broker` (as in `trigger-starts-a-run-as-the-grant-owner`) | The fired run gets one `run.finished` with `receiptDigest` equal to the receipt's digest. A later plain chat turn in the same thread (no park) gets none. The arming turn parked on an approval, so it gets one too: two in total, never one per turn |
| `restarted-approval-keeps-one-notification` | new | process | `org-admin`, `agent-with-guarded-tool` | Park; `restart-vamd`: still one open `approval.pending` record, `count: 1`. Deny resolves it (`acted`); a follow-up in the same thread with the identical call gets its own new record. One record per approval: never grouped across runs, a resolved record never reopens (owner, option B, 2026-11-02; replaces the deny-then-retry wording, which cannot occur on `main`: after a deny the guest takes no more tool calls) |
| `notifications-are-never-shown-to-another-user` | new | process | `org-admin`, `{ piece: org-member, as: bob }`, `agent-with-guarded-tool` | The owner's run parks. Bob's list and summary are empty. Bob's `GET`, `/read` and `/acted` on the owner's id answer 404, never 403 |
| `agent-reads-what-waits-without-acting` | new | process | `org-admin`, `agent-with-guarded-tool` (parks), **new** `agent-reads-notifications` (permission rule `builtin:notifications_list` allow, plus the trigger test tool), `trigger-test-broker` | The tool result names the pending approval and holds none of the call's arguments (host tier only). Afterwards the approval is still pending and the record is still unread. A run started by the trigger and calling the tool gets a typed refusal |
| `badge-counts-what-waits-on-you` | new | ui (the VAM app) | the same node state as the first scenario, arranged through the API | The badge equals `action + warn`, the list draws the existing approve card, deciding clears the badge on the next poll, and the decide request carries `X-Vam-Surface: app`. No UI runner exists (`Needs::Ui` is parsed but nothing runs it, `crates/vam-e2e/src/scenario.rs:81`): the file lands `pending` and stays so; the proof is the VAM app unit tests and the close checklist (open question 9) |

**Existing scenarios.** None has a `then` that changes (design §5: nothing new
on the receipt; §2.4: no new event). The runner's existing `approve` and
`decide-gate` keep deciding through the API with no surface header, so
`receipt-names-who-decided-and-from-where` keeps `surface: api`. Optional
**guards-only** additions, which are not a spec change: add
`vam:crates/vam-orchestrator/src/notifications.rs` to
`approval-suspends-and-resumes`, `parked-approval-survives-a-restart`,
`stop-denies-a-pending-approval`, `gate-decided-finishes-the-run`,
`question-waits-for-an-answer-and-resumes` and
`receipt-names-who-decided-and-from-where`. Their transactions now include the
record write, and a failed write fails the approval.

##### Benchmark delta

Both lines are already in `design/benchmark.md` §1 (P2 and P7 rows, added in
 with the owner's approval), so no doc edit is needed. This phase adds
how they are measured. Baseline on this base: **P2 0%** (no list exists, so no
pending decision is in one), **P7 unmeasured**. All runs use the mock model;
no paid model is run.

| Line | Measured on the mock by | Fields | Added in |
|---|---|---|---|
| P2: pending decisions present in the user's list (must be 100%) | After every `expect: { approval: pending }` and `expect: { gate: pending }` in any process scenario, the runner reads `GET /v1/approvals` (all pages) and `GET /v1/notifications?state=open` with the same key, and counts approvals whose `requestId` appears as an unresolved record's `approvalId`. Ratio per scenario, summed over the run, written to `summary.json` as `bench.notify.pendingListed {listed, pending}` | `approvals[].requestId.urn`, `records[].approvalId`, `records[].resolvedAt` | W0 adds the probe (it reports `0/N` until W1); W1 makes it 100% |
| P7: decisions the user learned about within a minute (share; median time to act) | The runner polls `GET /v1/notifications/summary` every 15 s, as the app does, from the moment a scenario parks. When `action` rises it lists once, which sets `first_shown_at`. At the end of the scenario it reads every `action` record and computes the share with `firstShownAt − createdAt ≤ 60` and the median `actedAt − createdAt`. These go to `summary.json` as `bench.notify.shownWithin60s` and `bench.notify.medianToActSecs` | `createdAt`, `firstShownAt`, `actedAt` | W0 adds the probe (it reports "unmeasured" while the routes 404); W1 makes it measurable |

On the mock, P7 measures the host side only: that a record exists and is
listable on the first poll. The median time to act is the script's own pace,
so it is recorded and never scored. The human number comes from the team
checklist and, later, the bench lane. The probe goes in the notifications
module of the runner (below), not in `bench.rs`, which the proof lane is
changing.

##### Items

Shared invariants. Every item's acceptance check holds these, and the
orchestrator checks them in review:

- **I1 Same transaction.** A decision-kind record is written by `vamd` only, in
  the same SQLite transaction as the approval or gate row it reflects, and in
  the same transaction as that row's resolution. A failed `record()` fails that
  transaction. No broker, route or tool writes a record.
- **I2 Dedupe.** One row per `(user_id, kind, subject_key)`, enforced by a
  `UNIQUE` index. A re-suspend of the same subject updates the row:
  `count + 1`, new `approval_id`, `updated_at`, and the resolution, acted and
  read fields cleared (open question 4). It is never one row per retry.
- **I3 `run.finished` per §2.3 and §9.6.** A run qualifies if any of:
  `runs.source` starts with `broker:`; it has a published approval row of any
  kind; or it ended `FAILED`, `TIMED_OUT`, `EXPIRED` or `REJECTED`. Never when
  the user stopped it (the stop result `reason: stopped`,
  `session_end.rs:81-86`). Never for a person's chat turn that succeeded
  without parking. Never for an asked run: on `main` only `broker:` sources
  exist, and the match is on `broker:`, not "non-empty", so the delegation
  sources the primary agent adds stay out. The record is written where the
  receipt is stored and by the sweeper, idempotent by run id, with a 60 s
  no-digest fallback.
- **I4 Surface.** The surface is taken from the authenticated channel:
  `X-Vam-Surface` mapped by `surface_from_header` (`run_facts.rs:25-31`; `cli`,
  `app`, else `api`), or `system` on sweeper paths. Never from a body field.
  Decision kinds copy `decided_surface` and `decided_by_user_id` from the
  approval row in the same transaction.
- **I5 Per user.** Every read and write goes through one
  `(user_id, id)` lookup. A foreign id answers 404 on every route.
- **I6 Host facts only.** The row holds templated host text. Agent text
  (`approval_message`, the argument preview, a rendered gate question) is
  attached at read time as `agentText {label, text ≤ 500}` on
  `GET /v1/notifications/{id}` only, and never reaches `notifications_list`.
- **I7 Agents read, never act.** `notifications_list` is read-only and opt-in.
  It is refused in a run whose `runs.source` is not NULL and does not set
  `read_at` or `first_shown_at`.
- **I8 Scopes.** Routes need `notifications:read` (GET) and
  `notifications:write` (read and acted marks). The PDP action labels are
  `ReadNotifications` and `UpdateNotifications`
  (`api/auth.rs:80-97`: the scope is what is checked; the action name is a
  label).

**NT-1.1 Table, `record()`, kinds, dedupe, prune.**
- **Scope.** New module `crates/vam-orchestrator/src/notifications.rs`. It holds:
  - the `notifications` table per design §2.1, created in the store's init
    block (`workflow.rs:653-660`, beside `spaces::initialize_schema`), with
    `UNIQUE(user_id, kind, subject_key)` and indexes on
    `(user_id, resolved_at, created_at)` and `(user_id, read_at)`;
  - ids from `spaces::mint_id("nt_")` (`spaces.rs:33`);
  - a `Kind` enum carrying all slice 1 and slice 2 kinds, with severity
    and a fixed title and body template per kind;
  - `record(tx: &Connection, NewNotification) -> Result<RecordOutcome>` as the
    upsert (I2), plus `resolve(tx, approval_id, reason, surface, by)` and
    `resolve_for_run(tx, run_id, reason)`;
  - `get_for_user`, list with the three-tier order (unresolved `action` oldest
    first, then unread newest, then read newest) and a cursor of
    `(tier, ts, id)`;
  - mark read, mark acted, and set `first_shown_at` where it is NULL;
  - `prune(now)`: §4.4, info 30 days and warn 90 days after resolved or read,
    unresolved `action` rows kept while their approval exists, cap of 2,000
    rows per user (oldest resolved first). It runs in the 60 s sweeper
    (`bin/vamd.rs:636`);
  - the counter `vam_notifications_total{kind}`, in the `gateway.rs:34` and
    `:364` pattern.
- **Files.** `notifications.rs` (new), `lib.rs`, the `workflow.rs` init block,
  `bin/vamd.rs` (prune call).
- **Acceptance.** Unit tests:
  - upsert: the second record of the same subject gives `count = 2`, one row,
    the new approval id, and the resolution cleared;
  - a foreign `(user, id)` is not found;
  - prune keeps unresolved `action` rows, removes info at 30 days and warn at
    90, and enforces the cap;
  - a `record()` that fails inside a transaction rolls the caller back;
  - `title` ≤ 120 characters and `body` ≤ 500, both from templates.
- **Tests.** `scripts/ci/local.sh --fast`.
- **Turns green.** None alone; with NT-1.2 and NT-1.5, see W1.
- **Decide.** `Decide: SLICE — proof 3 (row 7: the Slack gate rides this record), cost 2 (new table and API) → slice: the four slice-1 producers; other kinds in slice 2. Line: P2 pending decisions listed 100%`

**NT-1.2 Producers: approval, question, gate, and every resolution path.**
- **Scope.** Call `record()` or `resolve()` inside the existing transactions:
  - `create_approval` (`workflow.rs:938`), beside the `hitl.paused` append at
    `:1058`. Kind `tool_call` gives `approval.pending`, `question` gives
    `question.asked`. The recipient is `approver_user_id` (always the run
    owner today, `hitl.rs:37-90`). The subject is
    `(run_id, tool_name, args_digest)`;
  - `insert_gate_approval` (`:4296`, insert at `:4337`) gives `gate.opened`,
    subject `{run}/gate/{step}`;
  - `decide_approval_with_surface` (`:1234`, after the CAS update at `:1486`,
    beside `hitl.decided` at `:1557`) gives `acted` with that surface. The
    route, `reauth::resolve` (`api/reauth.rs:41`, surface `system`) and the
    `decide_approval` wrapper (`:1216`) all reach it;
  - `expire_stale_approvals` (`:1618`, `:1661`) gives `expired`;
  - `apply_one_gate_timeout` (`:1907`, `:1968`) gives `timed_out`;
  - `reject_pending_for_stop` (`:1715`, `:1760`) gives `stopped`;
  - the gate cancel in `update_playbook_step` (`:3995`, a skipped gate, which
    the design does not list) gives `superseded`.
  - A run that ends with a record still unresolved is closed `run_ended` by
    NT-1.3's call.
  - `reauth` parks: open question 7.
- **Files.** `workflow.rs` (approval paths only), `notifications.rs`.
- **Acceptance.**
  - Store tests for each path: a record exists if and only if the approval
    row exists (the transaction rolled back on an injected failure), it is
    resolved with the right reason and surface, and the surface is
    `system` on sweeper paths.
  - The question path: a record is created and cleared by `SUBMITTED` and by
    `REJECTED`.
  - Deny followed by an identical re-suspend gives one row with `count = 2`.
  - Existing HITL tests are unchanged.
- **Tests.** `--fast`; then
  `--scenarios approval-in-another-thread-reaches-the-list,restarted-approval-keeps-one-notification,gate-left-open-shows-until-it-times-out`.
- **Turns green.** With NT-1.5: the five W1 scenarios.
- **Decide.** `Decide: NOW — proof 2 (P2 line), cost 1 (files the item touches already hold the fact)`

**NT-1.3 Producer: `run.finished` in `issue_receipt` and the sweeper.**
- **Scope.**
  - `issue_receipt` (`receipt.rs:923`) ignores the `bool` from
    `insert_run_receipt` (`:78`; `:952` uses `?;` and drops it). Use it: when
    it is true, call `notifications::record_run_finished(store, run_id,
    Some(digest))`, which applies I3.
  - `issue_missing_receipts` (`:976`; startup `bin/vamd.rs:452`, the 5 s
    sweeper `:711`) covers restarts through the same call.
  - A second pass over `runs_missing_receipts(60)` (`:130`) records without a
    digest when no keys are installed. A later receipt fills the digest
    (`receipt_digest` NULL → set).
  - The same call closes the run's still-open decision records `run_ended`.
  - A duration `budget.hit` run gets no `run.finished` (§2.3). There is no
    `budget.hit` producer in slice 1, so this is a guard for slice 2.
  - It is one call in the receipt lane's file. The builder (`gather`,
    `build`) is not touched.
- **Files.** `receipt.rs` (two call lines), `notifications.rs`.
- **Acceptance.** Unit tests, one per §2.3 branch:
  - a trigger run produces one, a parked run produces one, a failed run
    produces one (warn);
  - a person-started succeeded chat turn and a user stop produce none;
  - a second issue and the sweeper add nothing (idempotent);
  - with no keys, the record arrives after 60 s without a digest and gets
    the digest later;
  - the record's `receiptDigest` equals `run_receipts.digest`.
- **Tests.** `--fast`;
  `--scenarios background-run-end-notifies-and-chat-turn-does-not`.
- **Turns green.** `background-run-end-notifies-and-chat-turn-does-not`.
- **Decide.** `Decide: NOW — proof 2 (P7 line: results reach the user), cost 1`

**NT-1.4 `report.arrived` and `delegation.*` kinds and their function.**
- **Scope.** Add `record_report_arrived(tx, asking_user, child_run_id,
  asking_thread, digest, succeeded)` and `record_delegation_ended(tx, …,
  stopped|expired)`, with subject = child run id and host templates.
  Nothing calls them yet (primary agent Phase 1 will). The kinds are listed
  by the API and the tool like any other.
- **Files.** `notifications.rs`.
- **Acceptance.** Unit tests: idempotent by child run id, `warn` when the child
  did not succeed, the action is `open` on the asking thread.
- **Tests.** `--fast`.
- **Turns green.** None (no caller).
- **Decide.** `Decide: SLICE — proof 1 (no 0.1.0 line; primary agent dependency), cost 0 → ship the kind and the function only`

**NT-1.5 API: list, summary, get, read, acted; scopes.**
- **Scope.** New `crates/vam-orchestrator/src/api/notifications.rs` with a
  `routes()` merged in `api/mod.rs` next to `spaces::routes()` (`:178`); that
  merge is the only edit to `mod.rs`. The five routes of design §3.2:
  - list: `state=open|all`, `unread`, `kind`, `size ≤ 100`; it sets
    `first_shown_at`;
  - `summary`: `{unread, action, warn, etag}` with `304` on `If-None-Match`;
  - `get`: attaches `agentText` and, for a decision kind, the approval as
    `GET /v1/approvals/{id}` returns it (reuse `stored_to_approval_request`
    and `stored_to_approval_status`);
  - `read`: single and bulk `{ids | before}`;
  - `acted`: `409 decide_required` with the decide link for decision kinds;
    otherwise surface from `request_surface` (`api/mod.rs:2398`).
  - Scopes `notifications:read` and `notifications:write` go into
    `BOOTSTRAP_SCOPES`, `ORG_ADMIN_SCOPES`, `PUBLISHER_SCOPES` and
    `MEMBER_SCOPES` (`keys.rs:33-76`). App session tokens get them at once,
    because `session_tokens.rs:354` reads the role defaults on each request.
    Stored API keys do not (open question 6).
- **Files.** `api/notifications.rs` (new), one line in `api/mod.rs`, `keys.rs`,
  and the OpenAPI yaml if routes are listed there.
- **Acceptance.**
  - Route tests for the order, paging (`size + 1`, cursor), the etag and 304,
    404 for a foreign id on all five routes, and 409 on a decision kind's
    `acted`.
  - `read` is idempotent.
  - `first_shown_at` is set once, by list (and by summary per open
    question 2), never by the tool.
  - A member key minted after the change can list; an old member key behaves
    as open question 6 decides.
- **Tests.** `--fast`; W1's five scenarios; `--full` before ready.
- **Turns green.** `approval-in-another-thread-reaches-the-list`,
  `decision-anywhere-clears-the-notification`,
  `gate-left-open-shows-until-it-times-out`,
  `restarted-approval-keeps-one-notification`,
  `notifications-are-never-shown-to-another-user`.
- **Decide.** `Decide: NOW — proof 2 (P2 line), cost 2 → the five routes only`

**NT-1.6 CLI `vam notifications`.**
- **Scope.** New `crates/vam-cli/src/cmd/notifications.rs`:
  - `vam notifications [--unread] [--kind k] [--all]`, `show <id>`, and
    `read <id>|--all`, with the global `--json`;
  - the list prints the decide line for decision kinds. On `main` that is
    `vam approve decide <id> --decision …`, not `vam approve <id>`
    (`cmd/approve.rs:20`);
  - `X-Vam-Surface: cli` is already on every request (`client.rs:9-31`).
- **Files.** `vam-cli` only.
- **Acceptance.** CLI tests against a stub server: table and JSON output,
  `show` prints `agentText` fenced and labelled, `read --all`, and a 404
  prints "not found".
- **Tests.** `--fast`.
- **Turns green.** None directly. The CLI decide in
  `decision-anywhere-clears-the-notification` uses the existing
  `vam approve decide`. The close checklist runs `vam notifications`.
- **Decide.** `Decide: NOW — proof 2 (P2: the list where a CLI user sees it), cost 1`

**NT-1.7 `notifications_list` host built-in.**
- **Scope.**
  - The guest needs a `ToolSpec` in `crates/vam-runtime/src/skills.rs`, in the
    `start_playbook_tool_spec` pattern (`:487`). Host tool schemas live on
    the guest side (`proto/vam/v1/vsock_rpc.proto:39`).
  - On the host:
    - add the catalog entry in the manager's catalog seeding
      (`manager.rs:1355-1392`) only when the agent's permissions hold an
      allow rule for `builtin:notifications_list`, and the run is
      person-started;
    - dispatch on the adapter id in `host_tools.rs` (`:765-795`, the
      `check_playbook_finish` pattern);
    - add a Cedar overlay constant in `security.rs`, not in
      `OUTBOX_OVERLAY` (`:167`), composed only when listed.
  - At each call, the host re-reads `runs.source`, as the loop guard does
    (`grants.rs:192-213`). If it is not NULL, it refuses with a typed error.
    The catalog is per session, so this check is the real one.
  - The result: unresolved and unread records of the run owner, host fields
    only, `kind` filter, `limit` 10 by default and 20 at most, capped at
    4 KiB. Side effect `READ_ONLY`.
  - The call sets no `read_at` and no `first_shown_at`.
- **Files.** `vam-runtime/src/skills.rs` and `agent.rs` (offer it when the
  host catalog has it), `vam-orchestrator` `host_tools.rs`, `manager.rs`,
  `security.rs`, `session_targets.rs` (the target row).
- **Acceptance.**
  - Unit tests:
    - absent from the catalog without the rule;
    - refused in a triggered run;
    - no `agentText` or argument bytes in the output;
    - `read_at` is unchanged after a call;
    - the output fits 4 KiB with 20 records.
  - The policy digest of agents that do not list the tool is unchanged.
- **Tests.** `--fast`;
  `--scenarios agent-reads-what-waits-without-acting`.
- **Turns green.** `agent-reads-what-waits-without-acting`.
- **Decide.** `Decide: SLICE — proof 1 (primary agent; proposed P7 latency line), cost 1 → read-only tool only`

**NT-1.8 the VAM app badge and list with the existing cards, `X-Vam-Surface: app`.**
- **Scope** (the app repo, base `feature/notifications`).
  - Tauri commands for `list`, `summary`, `get`, `read` and `acted` in
    `src-tauri/src/vamd.rs` and `lib.rs`. Register them in
    `generate_handler!` (`lib.rs:1818` area).
  - `X-Vam-Surface: app` on every authenticated request built in
    `vamd.rs` (`:553-629`). Today the app sends no surface, so its
    decisions are recorded `api`. `decide_approval` is at `:1454`.
  - A badge on `SidebarActivityBar.tsx`, showing `action + warn` and polled
    every 15 s with the etag.
  - A list view. A row opens:
    - approve rows: `ActionCard.tsx`, the `vam.approve` jig;
    - question rows: `jig/cards/QuestionCard.tsx`;
    - gate rows: the gate form;
    - `run.finished` rows: the run's thread. The app has no receipt view
      yet; that is the receipt lane's E12 Phase 2.
  - Opening a row marks it read. Following an `open` action marks it acted.
- **Files.** `src-tauri/src/vamd.rs`, `lib.rs`, `SidebarActivityBar.tsx`, a new
  `NotificationsPanel.tsx`, a hook.
- **Acceptance.**
  - `CI=1 pnpm test`, `pnpm exec tsc --noEmit`, and
    `cargo test --manifest-path src-tauri/Cargo.toml` pass.
  - Unit tests: the poller honours 304, the badge sum, the header is present
    on decide, and the list row picks the right card.
- **Tests.** the VAM app gate (playbook §4).
- **Turns green.** `badge-counts-what-waits-on-you`, by its unit tests plus
  the close check (open question 9).
- **Decide.** `Decide: NOW — proof 2 (P2: the inbox the user sees), cost 1 (one app wave)`

##### Waves

Run one wave at a time. Branches are `nt-1/wave-N` into `feat/notifications`;
the app wave is `feature/notifications` in the app repo.

| Wave | Items | Turns green | Repo | Model | Budget | Depends on | actions | df before | Status | PR |
|---|---|---|---|---|---|---|---|---|---|---|
| 0 | scenarios: 8 files + 2 world pieces + runner module | — (7 red at the first notifications step, class `product`; the ui one pending) | vam | Sonnet low | 60 min | — | fast (`vam-e2e`, `e2e/` are VM paths: `skip-vm-lane`) | 30 GB | [x] | — |
| 1 | NT-1.1, NT-1.2, NT-1.5 | the five W1 scenarios | vam | Sonnet medium: new table and API, writes inside the approval transactions | 120 min | 0 | fast (`skip-vm-lane`) | 22 GB | [x] | — |
| 2 | NT-1.3, NT-1.4 | `background-run-end-notifies-and-chat-turn-does-not` | vam | Sonnet medium: receipt issuance plus a reconciling sweeper | 60 min | 1 | fast (`skip-vm-lane`) | 22 GB | [x] | — |
| 3 | NT-1.6, NT-1.7 | `agent-reads-what-waits-without-acting` | vam | Sonnet low (owner's guidance). The tool's person-started check copies the loop guard; the orchestrator reads that code | 75 min | 1 | fast (`skip-vm-lane`; touches `vam-runtime`) | 22 GB | [x] | — |
| 4 | NT-1.8 | `badge-counts-what-waits-on-you` (unit tests + close check) | The VAM app | Sonnet medium: Tauri commands plus a new view and poller across Rust and TS. Low if the owner trims it to badge + list | 90 min | 1 (routes); can follow 2 | none (`[skip ci]`) | 21 GB | [x] | — |

Wave 0 also does these things:
- It adds the runner module `crates/vam-e2e/src/notifications.rs`, with
  `#[path]` in `engine.rs` like `triggers.rs`. That module is the only place
  the new steps and probes live; `scenario.rs` and `engine.rs` get dispatch
  lines only.
- The module's steps:
  - `expect: { notifications: { kind, count, run: latest, unread,
    state, fields, absent, as, within } }`;
  - `expect: { notification-summary: { action, warn, unread, as } }`;
  - `notification-mark: { action: read|acted, refused, status, as }`;
  - `via: cli|app` on `approve`, `deny` and `decide-gate`. `cli` runs the
    built `vam approve decide` through `VAM_E2E_CLI`; `app` sends
    `X-Vam-Surface: app`.
- It adds the P2 and P7 probes.
- It adds one section in `e2e/README.md`.

##### Shared surfaces per wave

| Wave | Shared surface | Rule |
|---|---|---|
| 0 | e2e runner (`scenario.rs`, `engine.rs`) | Own module. Dispatch lines only in shared files. The proof lane is adding bench modules; do not touch `bench.rs` |
| 1 | `hitl.rs`, `api/question.rs`, approval paths in `workflow.rs` | This lane's for slice 1. Change only the transactions named in NT-1.2. `api/mod.rs`: the `routes()` merge only. `keys.rs`: the four scope lists |
| 2 | Receipt issuance | One call in `issue_receipt` and one in the sweeper pass. `gather` and `build` stay the receipt lane's |
| 3 | `host_tools.rs`, `security.rs`, `manager.rs`, `vam-runtime` | New constants and branches only, next to the playbook tools. `OUTBOX_OVERLAY` unchanged |
| 4 | The VAM app `vamd.rs` request builders, sidebar | The header goes on in one place; cards are reused, not forked |
| — | `docs/` | Deferred until isolation-truth Phase 3 lands (handoff rule) |

##### Open questions, each with a default

1. **Retention.** Default §4.4: info pruned 30 days and warn 90 days after
   resolved or read; unresolved `action` rows kept while their approval
   exists; 2,000 rows per user. Runs in the 60 s sweeper.
2. **Where `first_shown_at` is set.** Default: by list and get, **and by
   summary** for the `action` and `warn` rows it counted, because the badge
   is the moment the user learns. Without summary, P7 measures list opens.
   Never by `notifications_list`.
3. **Badge: poll or push.** Default: poll `summary` every 15 s with the etag;
   a per-user stream only if P7 misses (Explicitly later).
4. **A re-suspend after the record was resolved.** **Decided (owner, option B,
   2026-11-02):** one record per approval. The subject is run-scoped
   (`run_id`, tool, args digest), so a follow-up run asking the identical call
   gets its own record, and a resolved record never reopens. `count + 1`
   applies only to a re-suspend of the same subject while it is unresolved
   (unit-tested; no scenario can produce it on `main`).
5. **Gate timeout in a scenario.** The gate and expiry sweeper runs every
   60 s (`bin/vamd.rs:636`). Default: the scenario waits `within: 90s`.
   Alternative: a process-mode-only env for the sweeper period. Rejected for
   now, because it adds a test seam to `vamd`.
6. **Stored API keys lack the new scopes** (scopes are stored per key at mint
   time, `keys.rs:335-341`; only session tokens read the role defaults).
   Default: a one-time migration adds `notifications:read` and
   `notifications:write` to every unrevoked key that holds `approvals:write`,
   so existing CLI keys work. Alternative: accept `approvals:write` in place
   of the new scopes.
7. **`reauth` parks** (a fourth approval kind on `main`, `hitl.rs:263`, not in
   the design). Default: no record in slice 1. The user resolves a reauth
   park by reconnecting, and the MCP connection page already shows it. Add
   one in slice 2 if wanted.
8. **Questions clear by any route.** Default: yes. Any resolution through the
   approval row clears it, with `resolved_reason` per path (`acted`,
   `stopped`, `expired`, `run_ended`).
9. **The app scenario has no runner.** Default: it lands `needs: ui`,
   `status: pending`, and is proven by the VAM app unit tests plus the close
   checklist. It is not counted "green" until a UI runner exists.
10. **How `notifications_list` detects a person-started run.** Default:
    `runs.source IS NULL`, read per call. This is primary-agent P-A: "any run
    a person did not start".
11. **Whether "waited on the user" is detectable at issue time.** Yes: a
    published approval row for the run, of any kind
    (`SELECT 1 FROM approvals WHERE run_id = ? AND published = 1`), read in
    `record_run_finished`.
12. **A trigger run the owner stopped.** Default: no `run.finished`, because
    the user did it (§2.3).

##### Design vs `main` (`main` is the fact)

1. **A restart does not re-suspend.** A parked approval survives a restart as
   the same row (`parked-approval-survives-a-restart`), and request ids are
   fresh on each suspension (`security.rs:1494`, `:2366`). The "re-suspend
   after restart" of §6 is therefore produced as deny, then an identical
   retry, then a restart. Owner's word on the scenario wording.
2. **The PDP checks scopes only.** `ReadNotifications` and
   `UpdateNotifications` are labels (`api/auth.rs:80-97`). There is no
   "owner" role; the default scope lists are Operator, OrgAdmin, Publisher
   and Member.
3. **The app does not send `X-Vam-Surface: app` today.** No header in
   the VAM app `src-tauri/src/vamd.rs`, so app decisions are recorded `api`.
4. **A user stop ends a run `FAILED`, reason `stopped`**, not `CANCELLED`
   (`api/stop.rs:4-16`). §2.3's exclusion must read the stop result.
5. **A fifth resolution path:** a skipped gate step is `CANCELLED`
   (`workflow.rs:3995`).
6. **`issue_receipt` drops the insert's `bool`** (`receipt.rs:952`).
7. **There is no `builtins:` field on agents.** Opt-in is a permission rule on
   the target `builtin:notifications_list`, not `tool:`.
8. **The CLI decide is `vam approve decide <id>`.**
9. **The app has no receipt view yet.** `run.finished` opens the thread.

##### Phase close (2026-11-02, closed in base; release to `main` waits on the owner's word)

- Scenarios: 7 process scenarios `active` and green in the orchestrator's runs (`approval-in-another-thread-reaches-the-list`, `decision-anywhere-clears-the-notification`, `gate-left-open-shows-until-it-times-out`, `background-run-end-notifies-and-chat-turn-does-not`, `restarted-approval-keeps-one-notification`, `notifications-are-never-shown-to-another-user`, `agent-reads-what-waits-without-acting`); `badge-counts-what-waits-on-you` stays `needs: ui` pending, proven by the VAM app unit tests (owner, recommendation 1) and step 3 and 8 below. Smoke at the last vam merge: 89 pass, 0 fail.
- Benchmark (mock model, smoke run, runner probes): **P2 pending decisions present in the user's list: 49/49 = 100%** (before: 0%, no list). **P7 decisions the user learned about within a minute: 46/46 = 100%; median created → acted 1 s** (before: unmeasured). Note: on the mock this measures the host side only (records exist and are shown by the first poll); a person's real reaction time is not measured.
- The VAM app: `CI=1 pnpm test` 789 pass, `tsc` clean, `src-tauri` 54 pass on the merged head.
- Totals: 5 waves (0–4), 2 fix cycles (W1, W3), 0 stop rules hit.

##### Team checklist — Phase 1

Setup: deploy `vamd` from `feat/notifications` first, then the app from
`feature/notifications`. A member key minted after the deploy, or migrated
per open question 6.

1. In the app, open thread A with any agent and chat.
2. In thread B, ask an agent whose `write_file` asks for approval to write a
   file. The run parks.
3. Within 15 s the sidebar badge shows 1 and the list shows
   "write_file waits for your approval" with the agent and thread.
4. `vam notifications` lists the same row with the line
   `vam approve decide <id> …`.
5. Run that line with `--decision approved`. The run finishes.
6. Within 15 s the badge is 0. `vam notifications --all` shows the row
   acted, surface `cli`.
7. `vam receipt show <run>`: the tool line says surface `cli`.
8. Repeat step 2 and approve in the app from the notifications list. The badge clears on the next poll and the receipt says `app`.
9. Give an agent the permission rule `builtin:notifications_list` allow, park a run as in step 2, and ask that agent in another thread "anything waiting on me?". It names the approval; the approval stays pending and the row stays unread. An agent without the rule does not have the tool.

Report: pass, or the step number with a screenshot or the error text.

### Phase 2 — Slice 2: channel adapters, Slack first, the remaining kinds

**Goal:** a user with a verified Slack contact gets approvals, gates, questions,
skipped schedules, expiring grants and budget stops as DMs, can decide there,
and the receipt says `slack`.  
**Depends on:** Phase 1 (on `main`); E5 Phase 1 gateway (on `main`).  
**Status:** closed in base 2026-11-04; release to `main` waits on the owner's word.  
**Plan:** approved 2026-11-03 (owner): recommendations 1 (11 scenarios merged to 8) and 2 (the exit scenario's run is started by a trigger), both spec changes accepted; NT-2.8 `notify_send` to BACKLOG; every open-question default accepted.  
**Exit:** `gate-decided-from-slack-names-slack-on-the-receipt` (the E12 Phase 2
exit, `design/run-receipt.md` §3 row 2) and the other seven scenarios below
active and green; the channel benchmark lines recorded on the mock.

- [x] NT-2.1 `brokers/vam-notify`: process, adapter registry, Slack adapter, fake transport, delivery log
- [x] NT-2.2 Contract: `NotificationChannel` (manifest 11), `HostedNotificationChannel.DeliverNotification`, `HostedNotificationGateway`
- [x] NT-2.3 Delivery worker in `vamd`: deliveries, retries, updates on resolve, rate fold, rendering
- [x] NT-2.4 `decide_as` extraction; `ActOnNotification` with the checklist; action handles
- [x] NT-2.5 Contacts and account linking (code, `LinkChannelAccount`, unlink)
- [x] NT-2.6 Preferences and quiet hours (API, CLI)
- [x] NT-2.7 Producers: `grant.expiring`, `budget.hit` (both points), `trigger.fired`, `trigger.skipped`
- [ ] NT-2.8 `notify_send` agent tool: **BACKLOG** (owner, 2026-11-03)
- [x] NT-2.9 Node notify settings: `PUT /v1/node/notify`, `vam node notify`, vault refs
- [x] NT-2.10 Jig rendering rule: native when simple, Open in VAM otherwise
- [x] NT-2.11 the VAM app: operator notify row, the user's contacts and preferences

#### Phase 2 breakdown (phase-breakdown agent, 2026-11-03; approved by the owner 2026-11-03)

Written against `feat/notifications` (= `main` plus one
docs commit). Line numbers are that tree. Where the code and the design
differ, the code is the fact (**Design vs `main`** below). No product code,
build or test was run for this breakdown.

##### Scenario delta

The design names 11 scenarios (§6 slice 2: seven; §9.7: four), all on the
mock model, the channel ones through a **fake transport** inside the broker.
The playbook allows 3 to 8. **Recommendation 1 (required): merge to 8**, by
folding the three that test the same gateway refusal path into one, and the
two that test the same rendering rule into one:

| Design scenarios | Becomes | Why it is one |
|---|---|---|
| `slack-click-from-an-unlinked-account-is-refused`, `channel-broker-cannot-decide-what-it-was-not-sent`, `decision-surface-comes-from-the-handle-not-the-adapter` | `slack-decision-needs-the-handle-and-the-linked-account` | All three drive `ActOnNotification` with one input wrong (account, handle, socket) and assert the approval stays pending; the surface check is the positive control at the end of the same script |
| `channel-adapter-renders-a-simple-jig-natively`, `complex-jig-falls-back-to-open-in-vam` | `simple-jigs-render-in-slack-and-the-rest-open-in-vam` | One rule (§9.3), two inputs; both only read what the fake transport sent |

Effect if taken: one phase, 8 scenarios, the exit at wave 5. If not taken:
either 11 scenarios in one phase (over the rule; three more files that
duplicate setup and add about 2 minutes to the scenario run), or a split
into **2a "Slack decides"** (A, B, C, D, H below; waves 0a, 0b, 2–6) and **2b
"preferences and the remaining kinds"** (E, F, G; waves 1, the prefs half of
2), which gets the demo path to `main` one release earlier at the cost of a
second breakdown, approval and release.

| # | Scenario | Change | Layer | Given | Why |
|---|---|---|---|---|---|
| A | `gate-decided-from-slack-names-slack-on-the-receipt` | new | process (fake transport) | `org-admin`, `agent-with-trigger-tool`, `trigger-test-broker`, `playbook-with-gate`, **new** `notify-broker`, **new** `slack-bot-token`, **new** `slack-app-token` | Phase exit and the demo shape. Steps: `node-notify` enables Slack with the two vault refs; `link-contact` verifies the owner's Slack account by code; a **trigger** starts `playbook-with-gate` (recommendation 2); the gate parks; one DM arrives with the gate's options as buttons and an Open in VAM link; the fake transport clicks `go` as the linked account; the run resumes and succeeds; `receipt.procedure.steps.1.gate.surface: slack`; the record is `acted` with `actedSurface: slack`; the summary drops; the DM is updated with no buttons |
| B | `decision-in-the-app-updates-the-slack-message` | new | process (fake transport) | `org-admin`, `agent-with-guarded-tool`, `notify-broker`, the two token pieces | A tool approval is delivered as a DM with Approve and Deny (no "always" buttons); it is decided with `approve: { via: app }`; within the worker period the fake transport records an update of that same message that names `app` and carries no buttons |
| C | `unverified-contact-receives-nothing` | new | process (fake transport) | as B, plus `{ piece: org-member, as: bob }` | Bob adds a Slack contact and never sends the code; the owner links and verifies. Both park an approval. The owner gets one DM; nothing is sent to Bob's account (fake outbox empty for it), Bob's record is still in his list. Unlink: the owner's next approval sends nothing |
| D | `slack-decision-needs-the-handle-and-the-linked-account` | new | process (fake transport) | as C, plus `trigger-test-broker` (its gateway socket is "another broker") | One parked approval, delivered. Refused, each leaving the approval pending: a click from an unlinked Slack account (`NOT_LINKED`); a forged handle (`INVALID`); the right handle presented on the trigger test broker's socket (`NOT_PERMITTED`); a decision outside the allowed set (`INVALID`). Then a good click decides it and the receipt says `slack`; replaying the same handle answers `ALREADY_RESOLVED`. Last: the broker's state directory holds neither token value nor any `vam_na_` string (the owner's credential check, open question 1) |
| E | `quiet-hours-hold-slack-until-morning` | new | process (wall clock, open question 10) | as B | Quiet hours set to a window that holds now; an approval parks; it is in the app list at once and no DM is sent; when the window ends, one DM arrives with it; a kind marked `always` is sent during the window |
| F | `expiring-grant-reaches-the-owner` | new | process | `org-admin`, `agent-with-trigger-tool`, `trigger-test-broker` | A grant whose lifetime is longer than the 7-day lead but expires inside it (open question 12) gives one `grant.expiring` (`warn`, action `renew`), once across two sweeps. **`budget.hit` left this scenario in wave 0b** (no scenario can set `max_duration`, the mock prices at 0): wave 1 proves it with unit tests at both points (one record per run, no `run.finished` beside it). List only (no channel), so it turns green before the broker exists |
| G | `skipped-schedule-is-told-once-a-day` | new | process | `org-admin`, `agent-with-trigger-tool`, `trigger-test-broker` (grant `onBusy: SKIP`) | Two fires on a busy workspace answer `BUSY SKIPPED`: one `trigger.skipped` record with `count: 2`; a revoke (the user's own act) adds none |
| H | `simple-jigs-render-in-slack-and-the-rest-open-in-vam` | new | process (fake transport) | as B, plus `playbook-with-gate`, **new** `playbook-with-file-gate` | The option gate and a tool approval arrive as native buttons plus an Open in VAM link; a gate whose form has a file field arrives as host title and body with **only** Open in VAM (no inputs, no submit); a click on an action the message did not offer is refused |

**New world pieces (6):** `notify-broker` (kind `broker`, `bin:
VAM_E2E_NOTIFY_BROKER`, args `--socket {{socket}} --state-dir {{dir}}/state
--fake-transport {{dir}}/fake`, `restartWithGateway: true`, manifest
`brokerId: vam.notify`, `TRUST_TIER_FIRST_PARTY`,
`notificationChannel: { channel: slack, surface: slack }`, egress per open
question 2); `slack-bot-token` and `slack-app-token` (kind `secret`, runner-made
values); `playbook-with-file-gate` (a gate whose `response_schema` has a
`vam-workspace-file` field, so its form holds a `FileInput`);
`agent-with-short-time-budget` (a run `max_duration` of a few seconds and a
slow tool). `agent-with-trigger-tool` and `trigger-test-broker` are reused;
the grant for A carries `playbookRef` (E5 locked: triggered playbook runs).

**Existing scenarios whose `then` changes: none.** Checked from `guards` and
`given` (`gateway.rs`: 8 schedule/trigger scenarios; `api/mod.rs`: 45;
`hitl.rs`: 1; `run_facts.rs`: 1):
- the HTTP decide keeps its behaviour after the `decide_as` extraction (W5),
  so `receipt-names-who-decided-and-from-where` keeps `surface: api` and
  `decision-anywhere-clears-the-notification` keeps `cli` and `app`;
- `trigger-on-a-busy-workspace-runs-after-the-task` now also leaves a
  `trigger.fired` record (queued fire) and
  `schedule-set-to-skip-does-not-wait-for-a-busy-workspace` a
  `trigger.skipped` record; neither `then` reads notifications;
- no scenario sets `max_duration` or a spend budget, so the new "a duration
  `budget.hit` run gets no `run.finished`" rule moves no `then`.

**Spec changes for the owner** (none to an existing `then`; these are to the
design's list):
1. The two merges above (recommendation 1): five design ids become two new ids.
2. A's run is started by a trigger, not by a person (recommendation 2), so the
   exit also moves the P7 "triggered and decided from a notify surface" half.
3. F is list-only (no Slack assertion) so it turns green with the producers;
   delivery of those kinds is unit-tested against the default preferences.

**Risk to existing scenarios (not a spec change).** Every scenario shares one
`vamd`. `notification-summary` assertions are exact
(`approval-in-another-thread-reaches-the-list`: `unread: 1`). A sweeper record
written mid-scenario for an earlier scenario's grant (`grant.expiring`, every
60 s) would bump `unread`. Defaults: open question 12 (no `grant.expiring`
for grants shorter than the lead, which covers every `once` schedule) and the
runner revokes the grants its broker pieces armed at cleanup.

**Guards-only additions** (not spec changes): add
`vam:crates/vam-orchestrator/src/api/decide.rs` (new) to
`receipt-names-who-decided-and-from-where`,
`decision-anywhere-clears-the-notification`, `approval-suspends-and-resumes`,
`gate-decided-finishes-the-run`, `question-waits-for-an-answer-and-resumes`
and `always-allow-and-always-deny-are-remembered` (the "remember" write stays
on the HTTP route only); `vam:crates/vam-orchestrator/src/notifications.rs` to
`trigger-on-a-busy-workspace-runs-after-the-task` and
`schedule-set-to-skip-does-not-wait-for-a-busy-workspace`.

##### Benchmark delta

Baseline on this base (slice 1 close, mock, smoke): **P2 49/49 =
100%; P7 learned within a minute 46/46 = 100%, median created → acted 1 s;
no channel exists, so every channel line is 0 or unmeasured.** No paid model.

| Line (`design/benchmark.md` §1) | Measured on the mock by | Field in `summary.json` | Expected after |
|---|---|---|---|
| P7 decisions the user learned about within a minute, **with a channel in the path** | For every decision record in a scenario with a verified contact, the runner takes the fake transport's timestamp of the first message for that record (`sent.jsonl`) and the record's `createdAt`; a delivery held by quiet hours is excluded (the user chose it). The worker sets `firstShownAt` on `sent`, which the runner cross-checks | `bench.notify.channelShownWithin60s {within, total}`, `bench.notify.channelMedianSecs` | 100%; median about the worker period (5 s) |
| P7 tasks started by a trigger and decided from a notify surface that complete (`benchmark.md:95`, notify half) | A trigger-started run that parked, was decided through the fake transport, and succeeded with the receipt naming `slack` | `bench.notify.triggeredDecidedFromChannel {completed, total}` | 1/1 (scenario A). The three-day gate on a simulated clock stays the bench lane's B2 row |
| Demo proof "a gate decided from Slack shows `slack` on the receipt" | Scenario A's `then` | scenario green | green |
| P2 pending decisions listed (regression) | Phase 1 probe, unchanged | `bench.notify.pendingListed` | stays 100% |
| P8 budget hits that stop a run cleanly | Not measured here; `budget.hit` (W1) gives the bench lane's runaway task its record | — | — |

The probes live in the new runner module, not in `bench.rs`. On the mock the
channel latency is the worker's period plus the fake transport, not a person;
the human number comes from the team checklist (a real Slack workspace).

##### Wave 0b "scenarios" (Sonnet low)

- The 8 scenario files `pending`, the 6 world pieces, and the runner module
  `crates/vam-e2e/src/channels.rs` (`#[path]` in `engine.rs` like
  `triggers.rs` and `notifications.rs`). `scenario.rs`, `engine.rs` and
  `world.rs` get dispatch lines only. `bench.rs` is not touched (the proof
  lane is adding bench modules).
- Steps (contract with the fake build, written into `e2e/README.md` under
  "Channels"):
  - `node-notify: { slack: { botToken: <piece>, appToken: <piece> }, appUrl }`
    → `PUT /v1/node/notify`;
  - `link-contact: { channel: slack, account: T1/U1, as, verify: true|false }`
    → `POST /v1/me/contacts`, then (when `verify`) the fake DM of the code
    from that account;
  - `unlink-contact: { channel: slack, as }`;
  - `notify-prefs: { quietHours: now|{start,end,tz}, always: [kinds], kinds }`
    → `PUT /v1/me/notification-prefs`;
  - `channel-click: { account, message: latest|<kind>, action, handle:
    forged|replay|<default>, via-socket: <piece>, status }` through
    `vam-notify-broker fake …` subcommands (the `hsp-echo-broker fire`
    precedent: the runner never speaks gRPC);
  - `expect: { channel-messages: { to, kind, count, buttons, openInVam,
    inputs, updated, mentions, absent, within } }` reads
    `<dir>/fake/sent.jsonl`;
  - `expect: { broker-state: { piece, lacks: [secret pieces, "vam_na_"] } }`
    greps the broker's state directory.
- The P7 channel probes above.
- `scripts/e2e/run.sh` already builds and exports `VAM_E2E_NOTIFY_BROKER` from
  wave 0a. Red for the right reason, class `product`: A–E and H fail at
  `node-notify` (404 on `/v1/node/notify`); F and G at their first
  `notifications` expect (no record of that kind). Proven with
  `scripts/ci/local.sh --scenarios <the 8>`.

##### Items

Shared invariants (every item's acceptance holds these; the orchestrator
checks them in review):

- **J1 One record, many deliveries.** Delivery rows are created by `vamd` only,
  inside the transaction that writes or resolves the record (`notifications::record`,
  `resolve`, `resolve_for_run`), from preferences, verified contacts and the
  node settings. No broker creates or changes a record (locked 2).
- **J2 Decision facts in `vamd`, delivery facts in the broker** (locked 14,
  §9.2). `vamd` keeps the delivery *intent and its state*
  (`notification_deliveries`: id, record, contact, `post|update`, `pending|
  sent|failed|suppressed|held`, attempts, `not_before`, `last_error_code`,
  handle credential id), because retries, holds, the fold and the update on
  resolve are `vamd`'s decisions and must survive a `vamd` restart. The broker
  keeps the *vendor facts* in its delivery log: delivery id, adapter, the
  Slack DM channel and message `ts`, sent at, vendor status and error. `vamd`
  never stores a Slack `ts`: an update names the original `delivery_id` and the
  broker maps it. §3.5's `external_ref` column is therefore not built.
- **J3 Surface from what `vamd` issued.** A channel decision's surface is the
  surface stored on the handle row at mint (the manifest's
  `notification_channel.surface`), never a request field. `ActOnNotificationRequest`
  has no surface field. `surface_from_header` is unchanged (`slack` from a
  header is still `api`).
- **J4 One decide path.** Every decision from any surface goes through
  `decide_as`; the channel path never writes a remembered rule, never passes
  `AMENDED`, and its response is validated by the same store code as the app's
  (`validate_gate_response`, `validate_question_answer`, `workflow.rs:1295`).
- **J5 No vendor credential at rest in the broker** (open question 1): tokens
  arrive per call and are dropped; handles live in broker memory only.
- **J6 Only verified contacts receive.** A contact without `verified_at`, or
  revoked, gets no delivery row at all.
- **J7 Host text by default.** Agent-written text reaches a channel only fenced
  and labelled, capped at 1 KiB, assembled at delivery time, never stored in a
  delivery row.

**NT-2.1 `brokers/vam-notify`: the broker.**
- **Scope.** A Rust crate `brokers/vam-notify`, binary `vam-notify-broker`, own
  process, HSP only, depending on `vam-spec` and shared crates, never
  `vam-orchestrator` (E10 locked 22; the `vam-schedule` layout: `state.rs`
  0700 dir and `flock`, `serve.rs` 0600 socket, `manifest` subcommand).
  - **Adapter registry** behind one trait: `capabilities()` (jig primitives
    rendered natively, takes decisions back, needs a verified contact),
    `deliver(rendering, binding, credentials) -> DeliveryResult`,
    `receive(event) -> Option<Relay>` (a decision with the delivery id, or a
    link code).
  - **Slack module** (DMs only): `conversations.open` then `chat.postMessage`
    to the user's DM; `chat.update` for an `update_of`; Block Kit from the
    neutral rendering (NT-2.10); forms as input blocks in the message, never a
    modal (a modal needs the bot token at click time, J5); Socket Mode
    envelopes acked on the socket. The live Socket Mode and Web API client
    is wave 6.
  - **Transport trait** under the adapter: `live` (wave 6) and `fake` (cargo
    feature `fake-transport`, open question 3): writes each Web API call
    (method and body, token removed) to `<dir>/sent.jsonl`; takes clicks and
    DMs from `vam-notify-broker fake click|dm|act-raw|handles` over a control
    socket in `<dir>`.
  - **Store** (`deliveries` log only, J2): no token, no handle column (open
    question 1).
  - Serves `HostedBroker` (Register, ListTools: no tools unless NT-2.8) and
    `HostedNotificationChannel.DeliverNotification` (idempotent by
    `delivery_id`) on its HSP socket; calls `LinkChannelAccount`,
    `GetChannelConfig`, `ActOnNotification` on its gateway socket.
- **Files.** `brokers/vam-notify/**`, workspace `Cargo.toml`, `Cargo.lock`
  (once, wave 0a), `scripts/e2e/run.sh` (build with the feature, export).
- **Acceptance.** Unit tests: rendering per built-in jig; `update_of` maps to
  the stored `ts`; a repeated `delivery_id` posts once; a click relays the
  delivery id and the Slack account, never a surface; the store schema has no
  secret column (a test reads `sqlite_master`); the default build has no
  `fake` subcommand and no `--fake-transport` flag.
- **Tests.** `--fast`; wave 4: `--scenarios
  decision-in-the-app-updates-the-slack-message,unverified-contact-receives-nothing,quiet-hours-hold-slack-until-morning,simple-jigs-render-in-slack-and-the-rest-open-in-vam`.
- **Turns green.** With NT-2.3, NT-2.5, NT-2.9, NT-2.10: B, C, E, H (wave 4);
  with NT-2.4: A, D (wave 5).
- **Decide.** `Decide: SLICE — proof 3 (demo: gate decided from Slack), cost 3 (new process, vendor trust boundary) → slice: Slack only, DMs only, no tools; Teams, email, push are slice 3`

**NT-2.2 Contract.**
- **Scope.** `proto/vam/v1/hosted_services.proto`, additive:
  - `CapabilityManifest.notification_channel = 11` (confirmed free; field 10 is
    `run_request_permission`) with `NotificationChannel {channel, surface,
    max_actions_per_minute}`;
  - **`service HostedNotificationChannel { rpc DeliverNotification }`**, served
    by the broker on its existing HSP socket, instead of a new RPC on
    `HostedBroker` (open question 8: `HostedBroker` has 12 implementors on
    `main`, each would need a stub). `DeliverNotificationRequest` per §3.5 with
    `Rendering` (title, body, `agent_text {label, text}`, actions each
    `{id, label, decision, option, style}`, optional jig IR as `Struct`,
    `open_url`, `cli_line`), `update_of` = the original delivery id,
    `resolved_secrets`; no Slack ids from `vamd` beyond the contact address;
  - `service HostedNotificationGateway { ActOnNotification;
    LinkChannelAccount; GetChannelConfig }` with typed outcomes
    (`ACCEPTED`, `NOT_PERMITTED`, `INVALID`, `EXPIRED`, `ALREADY_RESOLVED`
    with who and surface, `NOT_LINKED`, `RATE_LIMITED`, `UNAVAILABLE`).
  - `vam-spec` validation: `channel` in a closed list (`slack`); `surface`
    `[a-z]{3,16}` and not `cli|app|api|system`; not on a `THIRD_PARTY` manifest;
    `hsp_registry` narrowing at Register (a presented channel must equal the
    approved one).
  - The struct-literal ripple: `CapabilityManifest { … }` is written out in 6
    source files and 12 test files (`run_request_permission:` grep), plus the
    proto baseline descriptor if `local.sh` checks it.
- **Files.** `proto/`, `vam-spec` (validation, encoding test), `vam-orchestrator`
  (`hsp_registry.rs` subset check, the literal ripple).
- **Acceptance.** Validation tests for each rule; JSON round trip of
  `notificationChannel`; a manifest without it is byte-for-byte unchanged in
  digest; `vam-notify-broker manifest` validates.
- **Tests.** `--full` (the ripple touches many tests).
- **Turns green.** None alone (wave 0a: it lets the `notify-broker` piece
  register, so wave 0b's scenarios are red at a product step).
- **Decide.** `Decide: NOW — proof 3 (demo), cost 2 (proto)`

**NT-2.3 Delivery worker in `vamd`.**
- **Scope.** New `crates/vam-orchestrator/src/notify_delivery.rs`:
  - table `notification_deliveries` (J2), indexes on `(state, not_before)`;
    pruned with its record (§4.4);
  - rows created in `notifications::record` (on insert) and `resolve`/
    `resolve_for_run` (an `update` row for every `sent` post; open `pending`
    and `held` posts become `suppressed`), from preferences (NT-2.6), verified
    contacts (NT-2.5) and enabled adapters (NT-2.9). Defaults per §3.5;
  - a worker loop beside the 5 s receipt sweeper (`bin/vamd.rs:716`): due rows,
    `DeliverNotification` to the adapter's approved broker with the adapter's
    secrets resolved from the vault per call (J5), at-least-once, backoff per
    open question 4, then `failed`; `first_shown_at` set on the first `sent`;
    per user and channel 30 immediate an hour, the rest folded into one
    "and N more" message;
  - quiet hours: `held` with `not_before` = the window's end; released as one
    message (open question 11); `always` kinds skip the hold;
  - the neutral rendering builder: host title and body from the record, the
    approval's preview as `agent_text` (J7), the allowed actions, the jig IR
    when NT-2.10 says native, `open_url` from the node's app URL else the CLI
    line `vam approve decide <id> --decision …` (open question 6).
  - `vam_notify_deliveries_total{channel, state}`.
- **Files.** `notify_delivery.rs` (new), `notifications.rs` (the three hook
  calls), `bin/vamd.rs` (spawn the loop), `hsp.rs` (a client for the new
  service on the same endpoint).
- **Acceptance.** Store tests: no row for an unverified or revoked contact;
  a resolve queues one update per sent post and suppresses unsent ones;
  retries follow the schedule and end `failed` at 24 h; the fold at 31; held
  rows release at the window's end as one; a failed record write rolls the
  delivery rows back with it; secrets never appear in a row or a log line.
- **Tests.** `--fast`; wave 4's four scenarios.
- **Turns green.** With NT-2.1, NT-2.5, NT-2.9, NT-2.10: B, C, E, H.
- **Decide.** `Decide: NOW — proof 3 (demo), cost 2 (table, worker)`

**NT-2.4 `decide_as`, `ActOnNotification`, action handles.**
- **Scope.**
  - **Extraction.** New `api/decide.rs` with one
    `decide_as(state, DeciderCaller {user_id, principal, surface}, request_id,
    Decision {decision, response, rejection_reason, expected_revision})`: it
    calls the store's `decide_approval_with_surface`, flips the run state and
    spawns the resume, exactly as `decide_approval` does today
    (`api/mod.rs:1586-1773`). The HTTP route keeps parsing, the PDP check, the
    idempotency key and the "remember" write (`plan_remember`,
    `save_remembered_rule`), then calls `decide_as` with
    `request_surface(&headers)`. No copy of the body. `api/reauth.rs:41` and the
    store-level callers in `workflow.rs`, `host_tools.rs`,
    `playbook_activation.rs`, `security.rs` are not changed (system paths).
  - **Handles.** Minted by NT-2.3's builder for decision kinds:
    `vam_na_<id>.<secret>` as an `api_keys` row (`credentials.rs`), holder
    `broker:<id>`, target `approval:<request_id>`, permissions = the allowed
    decisions, constraints `{notification_id, user_id, contact_id, surface}`,
    expiring with the approval, SHA-256 stored. Single use = revoked in the
    decide transaction. Revoked when the record resolves, on unlink, and when
    re-minted at broker start (open question 1). HTTP never accepts one
    (`keys.rs::authenticate_bearer` already refuses non-`http` holders).
  - **Gateway.** New `notify_gateway.rs` serving `ActOnNotification` with the
    checklist of §3.6 step 3 in order: socket → approved, tier core or
    first-party, manifest has `notification_channel`, `SO_PEERCRED`
    (`NOT_PERMITTED`); handle parses, hash, holder is this broker, unused,
    unexpired (`INVALID`, `EXPIRED`; **a used handle whose record is resolved answers `ALREADY_RESOLVED`, not `INVALID`**, orchestrator 2026-11-04, so the replay step of scenario D holds); record unresolved (`ALREADY_RESOLVED`
    with who and surface); the external account is a verified, unrevoked
    contact **of the handle's user** (`NOT_LINKED`); the user is active; the
    decision is in the handle's set (`INVALID`); per-broker rate
    (`RATE_LIMITED`); then `decide_as` with the handle's user and surface.
  - Broker side: `receive(click)` → `ActOnNotification`; on
    `ALREADY_RESOLVED` it waits for `vamd`'s update delivery (it never edits
    the message on its own).
- **Files.** `api/decide.rs` (new), `api/mod.rs` (route body replaced by the
  call; one `mod`), `notify_gateway.rs` (new), `credentials.rs` (handle kind),
  `notify_delivery.rs` (mint), `brokers/vam-notify` (relay).
- **Acceptance.** Unit tests per checklist row; a forged surface is impossible
  by type (no field); the HTTP route's existing tests pass unchanged; a
  channel decision never writes a permission rule; `AMENDED` refused; a gate
  submit with an unoffered option fails in the store's validation; the
  approver check is the store's (`HitlApprover::allows_decider`).
- **Tests.** `--full`; `--scenarios
  gate-decided-from-slack-names-slack-on-the-receipt,slack-decision-needs-the-handle-and-the-linked-account`
  plus the six guards-only scenarios above.
- **Turns green.** A, D.
- **Decide.** `Decide: NOW — proof 3 (demo; P1 floor: no broker authority), cost 2`

**NT-2.5 Contacts and account linking.**
- **Scope.** §9.5's **contacts** are §3.5's `notification_channels`; one table
  `contacts` (`ct_` id, user, channel, address = Slack `team_id/user_id`,
  display, created, verified, revoked). Routes `POST /v1/me/contacts`
  (`{channel: slack}` → a ten-minute code, bound to the user, stored hashed,
  5 attempts), `GET` (no codes), `DELETE /{id}` (revokes the contact and every
  open handle for it). `LinkChannelAccount {code, external_account}` on the
  gateway verifies (binds the address, sets `verified_at`); for Slack the code
  goes user → bot (§3.6), not bot → user (email in slice 3 does the reverse).
  A Slack account is verified for at most one user. CLI `vam notifications
  contacts`, `link slack`, `unlink <id>`. Records and receipts keep user ids,
  never addresses.
- **Files.** `contacts.rs` (new), `api/contacts.rs` (new, one `routes()` merge),
  `notify_gateway.rs` (Link), `keys.rs` (scopes reuse
  `notifications:read|write`), `vam-cli`.
- **Acceptance.** Route and store tests: expired, reused and wrong codes; a
  second user cannot claim a verified account; unlink revokes handles; 404 for
  a foreign id.
- **Tests.** `--fast`; C in wave 4.
- **Turns green.** With the delivery waves: C (and the link step of A, B, D, E, H).
- **Decide.** `Decide: NOW — proof 3 (no Slack decision without it), cost 1`

**NT-2.6 Preferences and quiet hours.**
- **Scope.** `notification_prefs` per user: per kind (or `*`) `{channels,
  mode: immediate|off, always}`; `quiet_hours {start, end, tz}` (IANA); no
  `digest` mode (slice 3). `GET/PUT /v1/me/notification-prefs`, `vam
  notifications prefs [--quiet 22:00-07:00 --tz …] [--kind k --off|--always]`.
  Read by NT-2.3 when it creates rows. Defaults from the node's kind defaults
  (NT-2.9), else §3.5.
- **Files.** `notification_prefs.rs` (new), `api/notifications.rs` (two routes),
  `vam-cli`.
- **Acceptance.** Validation (bad tz, equal start and end, unknown kind);
  quiet window across midnight; defaults when unset.
- **Tests.** `--fast`; E in wave 4.
- **Turns green.** With the delivery waves: E.
- **Decide.** `Decide: SLICE — proof 2 (P7), cost 2 → defaults plus per-kind on/off, always and quiet hours; digests in slice 3`

**NT-2.7 Producers.**
- **Scope.** Each writes through `notifications::record` with host text:
  - `grant.expiring`: in `sweep_expiring_grants` (`ttl.rs:157`), the claim, the
    outbox append and the record in one transaction (today they are separate
    statements); skipped when the grant's lifetime is shorter than the lead
    (open question 12); resolved `acted` on renew (re-bind) or revoke
    (`gateway.rs::revoke`, `grants.rs`), `expired` at expiry. Recipient: the
    grant owner. Action `renew` targets the grant.
  - `budget.hit` (duration): in `expire_over_active_budget`
    (`workflow.rs:2453`), same transaction; and the I3 rule in
    `run_finished_wanted` skips `run.finished` for that run.
  - `budget.hit` (spend): a callback `vamd` installs on the inference broker,
    called on the first `check_preflight` refusal of a run
    (`vam-inference/src/broker.rs:561`); idempotent by run id.
  - `trigger.fired` (only `QUEUED`) and `trigger.skipped` (`BUSY SKIPPED`,
    `TARGET_GONE`, `AGENT_UNAVAILABLE`, `EXPIRED`; never `REVOKED`,
    `OWNER_DISABLED`) in `gateway.rs::fire` after the answer, subject
    `grant id + UTC day`, `count + 1` while unresolved. The broker's surface
    names it ("Schedule `sch_…` skipped: the workspace was busy" needs the
    schedule id, which `vamd` does not have: the title uses the grant's tool
    and the broker's surface instead).
- **Files.** `ttl.rs`, `workflow.rs` (`expire_over_active_budget` only),
  `gateway.rs` (`fire`, `revoke`), `grants.rs` (re-bind resolve),
  `vam-inference` (callback hook), `bin/vamd.rs` (install it),
  `notifications.rs`.
- **Acceptance.** Unit tests per kind: once per grant; once per run per point;
  one row per grant and day with `count`; none for `REVOKED`; a duration
  `budget.hit` run has no `run.finished`.
- **Tests.** `--fast`; `--scenarios
  expiring-grant-reaches-the-owner,skipped-schedule-is-told-once-a-day`.
- **Turns green.** F, G.
- **Decide.** `Decide: NOW — proof 2 (P7, P8 runaway task), cost 1`

**NT-2.8 `notify_send` agent tool.** See the recommendation below.
- **Decide.** `Decide: BACKLOG — proof 1 (no launch proof names the step; only the sample receipt and the B1 task text do), cost 1 (a MUTABLE_EXTERNAL tool, a channel allowlist, Cedar) → trigger: B1 writes the close-prep world with a notify step, or the owner keeps the step in the demo script`

**NT-2.9 Node notify settings.**
- **Scope.** Beside the model allowlist (`allowed_models.rs`,
  `api/allowed_models.rs` pattern): `GET/PUT /v1/node/notify` (`node:read` /
  `BROKERS_ADMIN_SCOPE`, idempotency key) with
  `{adapters: [{channel: slack, enabled, secretRefs: {botToken, appToken}}],
  kindDefaults: {kind: immediate|off}, appUrl}`. Validated before write: refs
  name existing **org-scope** vault secrets (`secrets.rs`); an enabled adapter
  needs an approved, enabled broker whose manifest declares that channel;
  `appUrl` is `https`. Only refs are stored; values are read per delivery
  (NT-2.3) and per `GetChannelConfig` (open question 1). `vam node notify
  [show|set …]`. Users may link only to enabled adapters.
- **Files.** `notify_settings.rs` (new), `api/notify_settings.rs` (new, one
  route line), `vam-cli/src/cmd/node.rs`.
- **Acceptance.** Validation tests; a member gets 403 on PUT; GET never returns
  a secret value.
- **Tests.** `--fast`; A–E, H in wave 4/5.
- **Turns green.** Every channel scenario's first step.
- **Decide.** `Decide: SLICE — proof 3 (no delivery without the bot token), cost 2 (new route) → slice: the Slack adapter's refs, kind defaults and the app URL; other adapters' rows in slice 3`

**NT-2.10 Jig rendering rule (§9.3).**
- **Scope.** `vamd` side (in NT-2.3's builder): picks the IR — `vam.approve`
  **without** its `always` and `never` buttons (locked 12) and with the
  allowed decisions only; `vam.question`; an option gate as one button per
  option; a schema gate as `form_ir("vam.gate_default", …)`
  (`vam-spec/src/jig.rs:1298`). Broker side (NT-2.1): Slack's capability set
  from the catalog's primitives: `Stack`, `Text`, `Badge`, `KeyValue`, `Button`,
  `Input`, `TextArea`, `Select` (≤ 100 options), `MultiSelect`, `Toggle`,
  `DateInput`; not `NumberInput`, `FileInput`, `ProgressBar`. Native only when
  every node is in the set and the only action is a submit; otherwise title,
  body and **Open in VAM** only. Open in VAM is present on native messages
  too. A channel submit carries only the offered fields; `vamd` validates it
  through the store (J4) and `check_result` for built-ins.
- **Files.** `notify_delivery.rs`, `brokers/vam-notify/src/slack/render.rs`.
- **Acceptance.** Tests per built-in: native; a `FileInput` gate falls back;
  the approve rendering has no remember actions; a submit with an unoffered
  value is refused by `vamd`.
- **Tests.** `--fast`; H.
- **Turns green.** H.
- **Decide.** `Decide: SLICE — proof 3 (the demo gate as buttons), cost 2 → slice: the built-in jigs and option gates natively, everything else Open in VAM (launch §2 keeps full Jig rendering in Slack out)`

**NT-2.11 the VAM app rows** (the app repo, base `feature/notifications`).
- **Scope.** Operator: a "Notifications" row beside the models row
  (`OrgSettingsModelsWorkspace.tsx`): enable Slack, pick the two vault secrets,
  kind defaults, app URL (`GET/PUT /v1/node/notify`). User: contacts (add
  Slack → show the code and "send it to the VAM bot in a DM"; list; unlink)
  and preferences (per kind on/off/always, quiet hours). Tauri commands in
  `src-tauri/src/vamd.rs`, registered in `lib.rs`.
- **Acceptance.** `CI=1 pnpm test`, `pnpm exec tsc --noEmit`, `cargo test
  --manifest-path src-tauri/Cargo.toml`; unit tests for the forms and the code
  display; the operator row hidden without `brokers:admin`.
- **Turns green.** None (UI; the close checklist).
- **Decide.** `Decide: NOW — proof 2 (P7: linking is how a user turns Slack on), cost 1`

##### Waves

One wave at a time. Branches `nt-2/wave-N` into `feat/notifications`; the app
wave on `feature/notifications` in the app repo. Wave 0a is the small
scaffold the playbook allows before the scenarios: the `notify-broker` piece
cannot register or start until the proto field and the crate exist (a missing
binary is class `env`, not `product`).

| Wave | Items | Turns green | Repo | Model | Budget | Depends on | actions | df before | Status | PR |
|---|---|---|---|---|---|---|---|---|---|---|
| 0a | NT-2.2; NT-2.1 skeleton (crate, every dependency in `Cargo.toml`, adapter and transport traits, `fake-transport` feature and its build check, `manifest`, stub `HostedBroker`); `run.sh` build line | — | vam | Sonnet medium: new proto services on a trust boundary, the manifest ripple, a new workspace crate (`Cargo.lock`). `--full` and an orchestrator read of the proto and validation before merge | 90 min | — | fast (`proto/`, `Cargo.lock`, `scripts/e2e/`: `skip-vm-lane`) | 30 GB | [x] | — |
| 0b | scenarios: 8 files, 6 world pieces, `channels.rs`, probes, README | — (8 red, right reason, class `product`) | vam | Sonnet low | 75 min | 0a | fast (`skip-vm-lane`) | 20 GB | [x] | — |
| 1 | NT-2.7 | F, G | vam | Sonnet low (owner) | 75 min | 0b | fast (`skip-vm-lane`) | 20 GB | [x] | — |
| 2 | NT-2.9, NT-2.5 (`vamd` routes, code, CLI; verification is wave 3), NT-2.6 | — (unit tests; scoreboard flat, expected) | vam | Sonnet low (owner: contacts and preferences); node settings store refs only | 90 min | 0b | fast (`skip-vm-lane`) | 20 GB | [x] | — |
| 3 | NT-2.3, NT-2.10 (`vamd` half), the gateway for channel brokers (`sync_broker` opens the socket for `notification_channel`; `notify_gateway.rs` with `LinkChannelAccount`, `GetChannelConfig`; `ActOnNotification` answers `NOT_PERMITTED` until wave 5) | — (scoreboard flat: no broker behaviour yet) | vam | Sonnet medium: new table and worker, vault reads per call, a new service on the gateway socket. `--full` and an orchestrator code read. May split 3a gateway / 3b deliveries if the plan does not fit one session | 150 min | 2 | fast (`skip-vm-lane`) | 16 GB | [x] | — |
| 4 | NT-2.1 (registry, Slack adapter rendering, fake transport behaviour, delivery log, link code relay), NT-2.10 (broker half) | B, C, E, H | vam | Sonnet medium (owner: the broker crate) | 150 min | 3 | none (`brokers/vam-notify` only; `[skip ci]`). If a dependency is added after 0a, `Cargo.lock` makes it fast | 16 GB | [x] | — |
| 5 | NT-2.4 (`decide_as`, handles, `ActOnNotification`, broker relay) | A, D | vam | Sonnet medium (owner: the approval path and a trust boundary). `--full`, the six guards-only scenarios, and an orchestrator read of `decide.rs`, `notify_gateway.rs`, `credentials.rs` before merge | 120 min | 4 | fast (`skip-vm-lane`) | 15 GB | [x] | — |
| 6 | NT-2.1 live transport: Socket Mode (`apps.connections.open` with the app token fetched per connect, WSS, acks, reconnect), Web API client with the per-call bot token, 429 `Retry-After` | — (no scenario: a live check on a test Slack workspace in the close checklist) | vam | Sonnet medium: vendor network and tokens; the orchestrator reads the token handling | 90 min | 5 | none (`[skip ci]`) | 15 GB | [x] | — |
| 7 | NT-2.11 | — (unit tests + close checklist) | The VAM app | Sonnet low (owner) | 75 min | 2 (routes); can follow 5 | none (`[skip ci]`) | 9 GB | [x] | — |

Scoreboard expectation: 0 green until wave 1 (2), flat in waves 2 and 3
(foundations; stated here so it is not flagged as a stall), 6 after wave 4,
8 after wave 5.

##### Shared surfaces per wave

| Wave | Shared surface | Rule |
|---|---|---|
| 0a | `hosted_services.proto`, `vam-spec` validation, `hsp_registry.rs` | Additive only; the manifest literal ripple is mechanical (`notification_channel: None`) |
| 0b | e2e runner (`scenario.rs`, `engine.rs`, `world.rs`) | Own module `channels.rs`; dispatch lines only; `bench.rs` untouched (proof lane) |
| 1 | `gateway.rs` (`fire`, `revoke`), `grants.rs`, `ttl.rs`, `workflow.rs` (`expire_over_active_budget` only), `vam-inference` broker (one callback) | This lane's for the notification kinds; the primary agent lane takes `gateway.rs` later |
| 2 | `keys.rs` (scope lists), `api/mod.rs` (one `routes()` merge per new module), `vam-cli` | No other edits to `mod.rs` |
| 3 | `notifications.rs` (`record`, `resolve`, `resolve_for_run`: one hook call each), `gateway.rs` (`sync_broker` condition and one `add_service`), `bin/vamd.rs` (one loop), `hsp.rs` (client) | `gateway.rs` keeps the trigger service unchanged; the notification service lives in `notify_gateway.rs` |
| 5 | `api/mod.rs` `decide_approval` (body moves into `api/decide.rs::decide_as`); `hitl.rs` read-only (`HitlApprover`); `api/question.rs` unchanged (`resume_run_after_answer` is called from `decide_as`); `workflow.rs::decide_approval_with_surface` unchanged; `credentials.rs` (handle kind) | This lane owns `decide_approval`, `decide_as`, the resume dispatch and the handle code. An E12 Phase 3 session may edit the receipt builder (`gather`, `build`), not these. `api/reauth.rs` unchanged |
| 7 | The VAM app `feature/notifications`: `vamd.rs` request builders, `lib.rs` handlers, the org settings page | Rows added beside the models row; existing cards reused |
| — | `docs/` | Deferred until isolation-truth Phase 3 lands (handoff rule) |

##### Open questions, each with a default

1. **Credentials held by the broker (the owner will read the store schema).**
   The tension: §3.6 step 1 has the broker keep the action handle in its own
   store (0600); §3.5 and q5 have it hold the Slack app-level token in memory
   for the Socket Mode connection's life via `GetChannelConfig`; §9.4 says
   "the broker holds no credentials between calls"; `vam-schedule` keeps its
   grant tokens in plain text in SQLite (`brokers/vam-schedule/src/store.rs:23-26`).
   Facts that bound it: a handle (like a grant) is good only on that broker's
   gateway socket (`holder = broker:<id>`; HTTP refuses non-`http` holders),
   single use, bound to one record, user and contact, and needs the click to
   come from the linked Slack account; a Slack token is good anywhere on the
   internet. Options:
   - **(a) Handle in the broker store, plain** (as §3.6). Survives a broker
     restart. A stolen DB plus socket access decides pending approvals; the DB
     alone does nothing, but it is a credential at rest, which fails the
     owner's reading.
   - **(b) Handle stored hashed, the secret in the Slack button value.** The
     broker store is clean, but the secret goes to Slack (a vendor). Rejected.
   - **(c) Handle in broker memory only; `vamd` re-mints on broker start.**
     When the broker calls `GetChannelConfig` at start, `vamd` revokes the
     broker's open handles and queues an `update` delivery with a fresh handle
     for every open decision post. A click in the seconds between restart and
     re-delivery answers "try again" in Slack. Store holds no credential;
     locked decision 6 unchanged.
   - **(d) Handle never leaves `vamd`; the broker relays the delivery id.** The
     socket plus the linked account are the only proofs. Simplest, but it
     drops "a handle delivered to that broker" from **locked decision 6**
     (owner's conversation).
   - **(e) Handle sealed in the broker store with a key `vamd` gives it at
     start.** Survives restart without re-delivery; a sealed column is still
     "stored".
   - **Tokens:** the app-level token only for the life of one
     `apps.connections.open` call (fetched through `GetChannelConfig` on each
     connect and reconnect, then dropped; the WSS URL is a short-lived ticket);
     the bot token per `DeliverNotification` (`resolved_secrets`), never kept.
     Every outbound Slack call is a `vamd`-initiated delivery, including the
     link confirmation and the "decided" update, so the broker needs no token
     between calls; forms are input blocks in the message, never a modal
     (`views.open` would need the bot token at click time).
   - **Default: (c) plus the token rule.** No vendor credential on disk or
     held between calls; the broker store holds delivery ids, Slack ids and
     message `ts` only; nothing in a stolen broker DB can act alone. Scenario D
     asserts the state directory holds no token and no `vam_na_` string.
2. **Slack app manifest and scopes.** Default: Socket Mode on; interactivity on;
   App Home messages tab on; event `message.im`. Bot scopes `chat:write`,
   `im:write` (open the DM), `im:history` (read the code DM to the bot); the
   app-level token `connections:write`. No `users:read`, no channel scopes, no
   `commands`. Broker manifest `egressDomains: [slack.com, wss-primary.slack.com,
   wss-backup.slack.com]`. Shipped as `brokers/vam-notify/slack-app.yaml`.
3. **Selecting the fake transport.** Default: a cargo feature `fake-transport`,
   off by default, built only by `scripts/e2e/run.sh`; the `--fake-transport`
   flag and the `fake` subcommands exist only in that build. A check beside
   `scripts/e2e/check-guest-build.sh` asserts the default dependency tree does
   not enable it and the release binary lacks its marker strings (the
   `process-guest` precedent). Not an env var (could be set by accident in
   production). It cannot be enabled in a release build short of rebuilding
   with the feature.
4. **Retry schedule.** Default: 30 s, 2 m, 10 m, 30 m, 1 h, then hourly until
   24 h after the row was created, then `failed` (the app list keeps the
   record). `429` honours `Retry-After`. Permanent errors
   (`user_not_found`, `account_inactive`, `invalid_auth`) fail at once;
   `invalid_auth` also marks the adapter unhealthy on `GET /v1/node/notify`. A
   decision post whose record resolved is `suppressed`, not retried.
5. **Agent-written text in Slack.** Default: as approved (§3.6, q6): a tool
   approval shows the app's argument preview, ≤ 1 KiB, fenced and labelled
   "written by <agent>, not verified by the host"; a gate question rendered
   with step outputs is labelled "from the playbook". Assembled at delivery
   time from the approval row, never stored in a delivery row or the broker
   log. Alternative: host text only, preview behind Open in VAM.
6. **Links when the node has no public app URL.** Default: `appUrl` in
   `/v1/node/notify`; unset → no link; the message carries the CLI line
   `vam approve decide <id> --decision …` (the command on `main`, not
   `vam approve <id>`), and a complex jig says "open VAM to answer".
7. **Delivery facts vs `notification_deliveries`.** Default: J2.
8. **`DeliverNotification` placement.** Default: its own service
   `HostedNotificationChannel` on the broker's HSP socket, not an RPC on
   `HostedBroker` (12 implementors on `main`).
9. **One Slack workspace per node.** Default: yes; the first verified contact
   pins the team id in the node settings and a contact from another team is
   refused.
10. **Quiet hours in a scenario.** No clock seam in `vamd`. Default: minute
    granularity; scenario E sets a window ending at the next minute boundary
    and waits `within: 150s`; E stays out of the smoke set. Alternative: a
    clock offset accepted only on a mock-scripts node (the
    `require_scripted_mode_for_model` precedent).
11. **Held deliveries at the window's end.** Default: one message, one section
    per held record (decision records keep their own buttons and handles), at
    most 10, then "and N more" with Open in VAM.
12. **`grant.expiring` for short grants.** A `once` schedule's grant expires at
    most 24 h after its fire, so it is always inside the 7-day lead: a
    notice within a minute of creation is noise, and in the shared e2e `vamd`
    it lands in later scenarios. Default: record only when the grant's
    lifetime (expiry − issue) exceeds the lead; the outbox event is unchanged.
13. **Spend `budget.hit` on the mock.** The mock prices calls at 0, so a spend
    budget cannot trip in a scenario. Default: F covers the duration point;
    the spend point is unit-tested through the installed callback.
14. **Questions from Slack.** Default: native per NT-2.10 (choices as buttons
    or a select, free text as an input block in the message); §9.3 governs
    over §3.6's "free text opens the app".
15. **Allowed decisions per kind.** Default: tool approval `APPROVED`,
    `REJECTED` (no `AMENDED`, no remember); gate `SUBMITTED` (option or form)
    and `REJECTED`; question `SUBMITTED`.
16. **`reauth` parks.** Default: still no record and no delivery (Phase 1 q7).
17. **`run.finished` in Slack.** Default: `warn` immediate, `info` off (§3.5);
    the message is the record's host title and body plus Open in VAM, which is
    the "notify short form with a link" of run-receipt decision 7; no receipt
    fields beyond the outcome.

##### NT-2.8 `notify_send`: BACKLOG (owner, 2026-11-03)

Evidence: the step appears in the sample receipt (`design/run-receipt.md:126`,
`5 notify_controller … evidence: vam.notify.send ✔`) and in the B1 task text
(`design/benchmark.md:113`, `close-prep`: "… post approved subset, notify").
The launch proof (0.1.0 launch scope) is "a scheduled `close-prep`
run, one denial, a gate decided from Slack a day later, five effects posted
once, a signed receipt": no notify step. No `close-prep` playbook, world piece
or example exists in the repo (`harness.md:119`: "a B1 task with no world
yet"); only the CLI receipt fixtures name it. The controller is already told
by the `run.finished` DM. `notify_send` would add a `MUTABLE_EXTERNAL` tool
that posts to shared Slack channels (an operator allowlist, Cedar, the bot
token on an invoke path), which slice 2 otherwise avoids (DMs only). If the
owner keeps the step in the demo script, build it with wave 6 (cost 1).

##### Design vs `main` (`main` is the fact)

1. **Gateway socket path.** `<db dir>/hsp/gw/<first 12 hex of sha256(broker_id)>.sock`
   or `VAMD_HSP_GATEWAY_DIR` (`gateway.rs:118-157`), not
   `<state_dir>/hsp/gateway/<broker_id>.sock`.
2. **The socket opens only for `run_request_permission`** (`gateway.rs:179`)
   and `prove()` step 1 requires it (`:421`): the notification service needs
   its own step 1 and `sync_broker` a wider condition.
3. **`HostedBroker` has 12 implementors** (vam-web, vam-schedule, eight in
   `hsp_echo*.rs`, two in tests): open question 8.
4. **`CapabilityManifest` literals** in 6 source and 12 test files; field 11
   is free.
5. **`decide_approval` is at `api/mod.rs:1586`** (design: `:1578`). It writes
   "remember" rules before deciding, flips run state and spawns the resume;
   `api/reauth.rs:41` calls the store function directly with surface `system`.
6. **Gate and question validation is inside the store's decide**
   (`validate_gate_response`, `workflow.rs:1295`), so a channel submit is
   validated by the same code with no new path.
7. **The CLI decide is `vam approve decide <id>`** (design q10 says `vam approve <id>`).
8. **Credentials have no single-use column.** Single use = revoke in the decide
   transaction; target, permissions and constraints columns fit the handle.
9. **`vam.approve` carries `always` and `never` buttons and a `reason` input**
   (`spec/jig/builtins/approve.json`); a channel rendering drops the first two.
10. **The catalog has no radio or checkbox primitive.** §9.3's Slack set maps
    to `Select`, `MultiSelect`, `Toggle`; `NumberInput`, `FileInput`,
    `ProgressBar` fall back.
11. **§3.6 vs §9.3** on free text from Slack, and **§3.5/q5 vs §9.4** on the
    app token: §9 governs (open questions 14 and 1).
12. **Line references moved:** spend preflight `vam-inference/src/broker.rs:561`
    (design `:423`); `expire_over_active_budget` `workflow.rs:2453` (design
    `:2349`).
13. **`grant.expiring` claim and outbox append are separate statements**
    (`ttl.rs:197-226`), not one transaction.
14. **No clock seam in `vamd`**, and the mock prices calls at 0 (open
    questions 10, 13).
15. **`vam-schedule` stores grant tokens in plain text** in its SQLite
    (`store.rs:23-26` `schedules.grant_token`, and `pending_revokes.grant_token`). Out of this phase; the
    owner's rule for `vam.notify` would also question it (a separate item if
    wanted).
16. **Launch §2 "Full Jig rendering in Slack: not building"** stays true: NT-2.10
    renders only the built-ins and option gates natively.
17. **E10 locked decision 9 wording** (delivery through the notification
    module's channels) is still the owner's word, not applied.

##### Stays for slice 3 (Phase 3, not 0.1.0)

Email (one-way, outbound code for contacts, link only), Teams, APNs, FCM and
Web Push adapters and push registrations, digests (`digest` mode and
`digest_at`), more adapters' rows in the node settings.

##### Phase close (2026-11-04, closed in base; release to `main` waits on the owner's word)

- **Scenarios:** 8 of 8 `active` and green in the orchestrator's runs: `gate-decided-from-slack-names-slack-on-the-receipt` (phase exit), `decision-in-the-app-updates-the-slack-message`, `unverified-contact-receives-nothing`, `slack-decision-needs-the-handle-and-the-linked-account`, `quiet-hours-hold-slack-until-morning` (tag `slow`, outside smoke; one unexplained failure at W5's first head, then 7 passes: watch), `expiring-grant-reaches-the-owner`, `skipped-schedule-is-told-once-a-day`, `simple-jigs-render-in-slack-and-the-rest-open-in-vam`. Phase 1's seven process scenarios still green. Smoke at the last vam wave: 96/96.
- **Benchmark** (mock model, smoke at W5, runner probes): P2 pending decisions in the list **67/67 = 100%** (slice 1: 49/49). P7 learned within a minute **59/59 = 100%**, median to act 1 s (slice 1: 46/46). **P7 with Slack in the path: 54/54 within 60 s, median 0 s** (before: unmeasured). **P7 triggered and decided from a notify surface, completed: 1/1** (`design/benchmark.md:95`, notify half; before: 0). Demo proof met: a gate decided from Slack shows `slack` on the receipt. On the mock these measure the host side; a person's reaction time and real Slack latency come from the checklist.
- **Not proven here:** the live Slack transport against a real workspace (wave 6 is tested against local fake servers only; the live run needs the owner's word, step 3 below). `budget.hit` by unit tests only (no scenario can trip a budget on the mock).
- **Open in VAM:** the VAM app is a desktop app with no URL router (only the `squrell://` OAuth scheme), so `{appUrl}/notifications/<id>` has nowhere to land. For 0.1.0 operators leave `appUrl` unset and Slack shows the CLI line `vam approve decide <id>`; `appUrl` is for a later web shell.
- **The VAM app:** W7 gates at the head: `CI=1 pnpm test` 796 pass, `tsc` clean, `src-tauri` 57 pass.
- **Totals:** 9 waves (0a, 0b, 1–7), 8 vam PRs (–) and one the VAM app PR; fix cycles 6 (W1 1, W2 1, W4 1, W5 2, W6 1 own), no third cycle, no stop rule hit. Remote wave branches `nt-2/wave-0a` … `nt-2/wave-7` to delete (auto mode refused remote deletes).

##### Team checklist — Phase 2

Setup: deploy `vamd` and `vam-notify-broker` (default build, no `fake-transport`) from `feat/notifications`, then the app from `feature/notifications`. A test Slack workspace with an app created from the manifest in (Socket Mode on; bot scopes `chat:write`, `im:write`, `im:history`; app token with `connections:write`; events `message.im`; interactivity on). Put the two tokens in the vault as org secrets.

1. Operator: approve the `vam.notify` broker manifest (`PUT /v1/hosted/brokers`, start the broker with `--gateway-socket` from `GET /v1/hosted/brokers/vam.notify`). In the app, Org settings → Notifications: enable Slack, pick the two secrets, leave App URL empty, save. The broker log says it connected.
2. User: in the app's notifications panel, Link Slack. A code `XXXXX-XXXXX` shows once. DM it to the VAM bot in Slack within 10 minutes. The bot answers that the account is linked; the contact shows `verified`.
3. In thread A chat with any agent. In thread B ask a scheduled or triggered playbook run that has a gate (or ask an agent whose `write_file` asks for approval). Within a minute a Slack DM arrives with the title, Approve/Deny or the gate options, and the CLI line.
4. Click the gate option (or Approve) in Slack. The run resumes and finishes; the DM is edited to "Decided by you in slack at …" without buttons.
5. `vam receipt show <run>`: the decision says surface `slack`. The app badge cleared on its next poll.
6. Park another approval. Approve it in the app. The Slack DM is edited to "Decided by you in app at …", buttons gone.
7. Set quiet hours covering the next two minutes (`vam notifications prefs --quiet HH:MM-HH:MM --tz <zone>`). Park an approval: the app list shows it at once, Slack shows nothing; after the window one DM arrives.
8. Unlink Slack in the app. Park an approval: no DM arrives. An old message's button now answers "not linked" or "try again in the app" and decides nothing.

Report: pass, or the step number with a screenshot or the error text.

### Phase 3 — Slice 3: email, push, digests (later, not 0.1.0)

**Goal:** the same records by email (one-way) and as content-free push; digests.  
**Depends on:** Phase 2; a VAM app mobile or web shell.  
**Plan:** not drafted

- [ ] NT-3.x per `design/notifications.md` §6 slice 3 and §9.7

## Explicitly later

- Org approvals inbox (E3).
- Account creation by email invite (users and roles).
- A per-user event stream for the badge (only if the P7 latency line misses).

## Progress log

| Date | Phase | What landed (end with the scoreboard: scenarios green, red, fix cycles, wall time) |
|---|---|---|
| 2026-11-07 | 2 | **Release PRs opened** on the owner's word: (`feat/notifications` → `main`) and (`feature/notifications` → `main`, after vam). Merge commits; full Actions on the vam release PR once. Merging waits on the owner's word. |
| 2026-11-04 | 2 | **Phase 2 closed in base.** 8/8 scenarios green (exit `gate-decided-from-slack-names-slack-on-the-receipt`); benchmark on the mock: P2 67/67, P7 59/59 (median 1 s), P7 through Slack 54/54 (median 0 s), triggered and decided from Slack 1/1. Team checklist written (operator enables Slack with vault refs and approves the broker; user links by code; a gate decided from the Slack DM reads `slack` on the receipt). Live Slack not yet run (owner's word). Open in VAM: no app router; `appUrl` unset for 0.1.0. Release PRs (vam first, then the VAM app) wait on the owner's word. |
| 2026-11-04 | 2 W7 | **Wave 7 merged** (merge commit into `feature/notifications`; Sonnet 5.5 low). Org settings → Notifications row (enable Slack, bot and app token **secret names** from the org secrets list, kind defaults, app URL; hidden without `brokers:admin`; never a token value). Notifications panel: Link Slack (code shown once, component state only, "send this code to the VAM bot in a Slack DM within 10 minutes"), contacts with state and Unlink, per-kind Default/On/Off/Always, quiet hours (minute pickers, time zone). Seven Tauri commands on the shared client (`X-Vam-Surface: app`; PUTs and unlink carry `Idempotency-Key`, captured-request tests). Finding: the desktop app has no URL router, so Open in VAM links have nowhere to land; `appUrl` stays unset for 0.1.0. The agent installed GTK/webkit dev packages and node_modules in its worktree (disk 15 → 9 GB, back to 14 GB after cleanup). Orchestrator re-ran: `CI=1 pnpm test` 796 pass, `tsc` clean, `src-tauri` 57 pass. Scoreboard: no scenario (UI; checklist), 0 fix cycles, about 20 min. |
| 2026-11-04 | 2 W6 | **Wave 6 merged** (merge commit; Sonnet 5.5 medium). `brokers/vam-notify/src/live.rs`: `LiveTransport` Web API client (POST JSON, bearer token per call, 20 s timeout, no redirects, 429/`ratelimited` → retry with clamped `Retry-After`, 5xx → retry, `invalid_auth`/`user_not_found` permanent, every error text scrubbed of the token) and Socket Mode (`apps.connections.open` with the app token fetched per connect via `GetChannelConfig {purpose: connect}` and dropped; `wss://` only; ack every envelope first; `hello`, `disconnect` → reconnect with a fresh token, `message.im` link DMs, `block_actions` clicks with form state; bot-authored DMs ignored; 120 s idle limit; events from another team dropped when a workspace pin is configured, vamd sends none yet). The transport holds no token; the test base URL (`LiveTransport::for_test`) exists only under `cfg(test)` or `fake-transport`, no env or flag; a default build always talks to `https://slack.com/api`. 13 tests in `tests/live_slack.rs` against local fake servers (no call to real Slack), including token absence from logs, errors and the state dir. No new dependency; `Cargo.lock` unchanged. Slack app manifest (Socket Mode; bot scopes `chat:write`, `im:write`, `im:history`; `connections:write` on the app token; `message.im`; interactivity) is in the PR body for the operator checklist. Orchestrator read the token handling; re-run: `--full` only the known root-only `hsp_uds` (live_slack 13/13); `--fast` smoke 96/96; `check-notify-build.sh` PASS. Marked ready with `skip-vm-lane` (the head commit carried no `[skip ci]`; fast jobs run once, not awaited). Not proven against a real Slack workspace: that run waits for the owner's word (close checklist). Scoreboard: no scenario (by plan), 8/8 green unchanged, 1 fix cycle (the agent's own compile error), about 30 min. Disk 15 GB. |
| 2026-11-04 | 2 W5 | **Wave 5 merged** (merge commit `5963714`; Sonnet 5.5 medium). `api/decide.rs::decide_as(state, DeciderCaller{user_id, principal, surface, allow_remember}, request_id, DecideInput)` holds the whole former `decide_approval` body (remember rule, decision, run state, resume); callers: the HTTP route (surface from `X-Vam-Surface`, never `slack`; `allow_remember: true`) and `notify_gateway::act_on` (the handle's user and the surface stored on the handle; `allow_remember: false`; `AMENDED` never). Action handles: `api_keys` rows (`issued_via = notification`, holder `broker:<id>`, SHA-256 only, `vam_na_<id>.<256-bit secret>`), bound to notification, approval, user, contact, channel, offered decisions and surface; expire with the approval (7 d if none); revoked in the decision's own transaction, on unlink, and on broker connect; never authenticate `/v1/*` (tested). `ActOnNotification` check order: approval enabled → tier → manifest channel → `SO_PEERCRED` → handle parse, constant-time hash and holder, channel → unexpired → used/revoked (`ALREADY_RESOLVED` if resolved, else `INVALID`) → record unresolved → account is a verified contact of the handle's user and contact (`NOT_LINKED`; another user's linked account `INVALID`) → user active → decision offered and response valid → per-broker rate → `decide_as`. **Contract addition (accepted):** `GetChannelConfigRequest.purpose` (`connect` returns app and bot tokens and re-mints: open handles revoked, each still-waiting post edited with a fresh handle; `reply` returns the bot token only, changes nothing); the broker fetches the bot token per reply and drops it. `notification_deliveries.refresh` column. Runner: cleanup cancels runs as each run's owner. Arrangement only: `agent-with-trigger-tool` gains `builtin:check_playbook_finish: allow`. Fix cycles: 2. Cycle 1 (agent): own unit tests. Cycle 2 (orchestrator): `quiet-hours-hold-slack-until-morning` failed once (no message within 150 s; passed on the base); not reproduced in 4 agent runs nor in 3 orchestrator runs, cause not found, recorded as a **flake to watch**; the cycle added a reason code on every suppressed decision post (`last_error`: resolved, contact_gone, approval_expired, no_actions, no_approval) and unit test `a_held_decision_released_after_quiet_hours_goes_out_with_a_fresh_handle`. Orchestrator read the decide path and gateway; re-run: quiet hours 3/3 pass; `--full` only the known root-only `hsp_uds`; `--fast` smoke 96/96; `check-notify-build.sh` PASS. Benchmark (mock, smoke): P2 listed 67/67; P7 shown within 60 s 59/59, median to act 1 s; **P7 with Slack in the path 54/54, median 0 s; triggered and decided from Slack 1/1**. Scoreboard: 2 green (A `gate-decided-from-slack-names-slack-on-the-receipt` (phase exit), D `slack-decision-needs-the-handle-and-the-linked-account`), **8 of 8 Phase 2 scenarios green**, 0 red, 2 fix cycles, about 3.5 h. Disk 15 GB. |
| 2026-11-04 | 2 W4 | **Wave 4 merged** (merge commit; Sonnet 5.5 medium). Broker: `hub.rs` (idempotent `DeliverNotification`, `update_of` edits via the log's message ts, in-memory handle map `delivery_id → handle` capped at 4096, click relay to `ActOnNotification`, link relay to `LinkChannelAccount`, connect loop calling `GetChannelConfig` per connect), `log.rs` (only table `deliveries`: delivery id, notification id, channel, Slack channel id, message ts, sent at, attempts, vendor status; 0600 in a 0700 dir; writes refuse values starting `xox`, `xapp`, `vam_na_` or outside `[A-Za-z0-9_.:-]`), `slack/render.rs` (Block Kit, DM only, button values are delivery ids, never handles; native inputs only when `jig_ir`; Open in VAM or the CLI line), `control.rs` (fake control socket, feature only; `fake handles` lists ids only). Option-gate clicks send the option id. Test `broker_flow.rs` scans the state dir after a delivery, a link and a click for the tokens, handle and code. **Spec change (orchestrator, arrangement only, no `then` changed):** C decides the first approval in the app before the second; H decides the tool approval and uses `agent-plain` for the playbooks; world piece `agent-with-trigger-tool` gains `builtin:step_complete: allow` so A reaches its gate. Runner: `node-notify` unlinks the shared owner's leftover contacts. Fix cycle 1 (orchestrator review): three scenarios were failing on wave-0b arrangement bugs, class harness. Open for wave 5: the broker has no bot token for click and link replies (vamd to return it per call from `GetChannelConfig`). Orchestrator read the store and secret handling; re-run at `6550183`: `--full` only the known root-only `hsp_uds`; `--fast` smoke 96: 94 pass, 2 red right reason, 0 wrong; `check-notify-build.sh` PASS; `Cargo.lock` unchanged. Scoreboard: 4 green this wave (B `decision-in-the-app-updates-the-slack-message`, C `unverified-contact-receives-nothing`, E `quiet-hours-hold-slack-until-morning`, H `simple-jigs-render-in-slack-and-the-rest-open-in-vam`; 6 of 8 total), 2 red right reason (A, D at `when.9`, the first Slack click, class product), 1 fix cycle, about 3 h. Disk 16 → 15 GB. |
| 2026-11-04 | 2 W3 | **Wave 3 merged** (merge commit; Sonnet 5.5 medium). `notify_delivery.rs`: `notification_deliveries` (intent and state only: post/update/summary, pending/sent/held/failed/suppressed, hold quiet/rate, attempts, `not_before`, error code; no token, handle, address, Slack message id or agent text, test greps every table for the vault values), rows created in the record's transaction (hooks in `record`/`resolve`/`resolve_for_run`) only for verified unrevoked contacts of an enabled adapter with an approved channel broker; quiet hours hold all but `always` kinds and release as one summary; rate fold at 30/h; update on resolve ("Decided by … in … at …", no actions; updates are not held); retries 30 s…24 h; 2 s worker loop in `vamd`; bot token from the vault per send in `resolved_secrets`; `first_shown_at` on SENT. Neutral rendering (NT-2.10 vamd side): host title/body, offered actions (tool approvals Approve/Deny only), `cli_line`, `open_url` = `{appUrl}/notifications/<id>` (wave 7 to confirm), `jig_ir` only when native; `action_handle` empty until wave 5. `notify_gateway.rs` on the broker's gateway socket (`wants_gateway`: `notification_channel` on core/first-party): checks in order approval enabled → tier → manifest channel → `SO_PEERCRED`; `GetChannelConfig` (own channel, enabled, app token from the vault per call, not cached), `LinkChannelAccount` (`verify_contact_with_code`; first linked workspace pins the node, accepted), `ActOnNotification` stub `UNAVAILABLE`. Runner fix: `link-contact` takes the code from `POST /v1/me/contacts`; six scenarios' `expectFail.step` moved (no `then` edits). Orchestrator read the gateway code; re-run: `--full` only the known root-only `hsp_uds`; `--fast` smoke 96: 91 pass, 5 red right reason, 0 wrong. Scoreboard: 0 green, 6 red moved later (five at `when.1`, `unverified-contact-receives-nothing` at `when.2`: the wave 4 fake link), 0 fix cycles, about 1.5 h. Disk 16 GB before and after. |
| 2026-11-04 | 2 W2 | **Wave 2 merged** (merge commit; Sonnet 5.5 low). `node_notify.rs` + `GET/PUT /v1/node/notify` (`{slack:{enabled,botTokenRef,appTokenRef}, kindDefaults, appUrl}`; PUT needs `brokers:admin` and an idempotency key; refs checked as org-scope vault secrets by `host_secret_exists`, no value read or stored; GET hides ref names from non-operators), `vam node notify`. `contacts.rs` + `/v1/me/contacts` (start link returns a one-time code once; list; unlink/DELETE; `verify_contact_with_code` for wave 3's `LinkChannelAccount`; `revoke_open_handles_for_contact` hook for wave 5), `vam notifications contacts|link|unlink`. `notification_prefs.rs` + `GET/PUT /v1/me/notification-prefs` (per kind, quiet hours HH:MM + IANA tz, `always`), `vam notifications prefs`. No new scopes (contacts/prefs reuse `notifications:*`). Fix cycle 1 (orchestrator review, security): the link code was 6 digits (~20 bits) looked up across all pending contacts; now 10 Crockford base32 symbols from OsRng (~50 bits), `XXXXX-XXXXX`, case-insensitive; stored as a channel-salted SHA-256 only; 10 min TTL, per-contact and per-account attempt limits kept. Wire shape follows the runner (`slack:{…}`, not the breakdown's `adapters` list). Orchestrator re-run: `--full` only the known root-only `hsp_uds`; `--fast` smoke 96: 91 pass, 5 red right reason, 0 wrong. Scoreboard: 0 green (flat, expected), 6 red: the broker scenarios still fail at `when.0`, now on "no gatewaySocket for vam.notify" (wave 3), class product; 1 fix cycle, about 1.5 h. Disk 20 → 16 GB. |
| 2026-11-04 | 2 W1 | **Wave 1 merged** (merge commit; Sonnet 5.5 low). Producers: `grant.expiring` in `sweep_expiring_grants` (claim, outbox and record in one transaction; grants whose lifetime is not longer than the lead are skipped), cleared `acted` on revoke (single key and all of a user's keys, same transaction) and `expired` by the sweeper; no renew path exists on `main`. `budget.hit` at both points: duration stop in `expire_over_active_budget` (same transaction; `run_finished_wanted` false for a duration stop), spend via a new `BudgetRefusalCallback` in `vam-inference` installed in `brokers.rs` (idempotent per run); proven by unit tests `budget_hit_duration_stop_is_one_record_and_no_run_finished`, `budget_hit_spend_refusal_is_one_record_per_run`. `trigger.fired` (queued) and `trigger.skipped` (BUSY SKIPPED, TARGET_GONE, AGENT_UNAVAILABLE, EXPIRED; not REVOKED or OWNER_DISABLED) from `gateway::fire` after the answer, per grant and UTC day, `count` grows; recorded after the fire, not inside its transaction (the outcome spans several). Records carry the issuing run's id so the runner's run filter sees them. **Test seam (accepted by the orchestrator):** `VAMD_E2E_GRANT_EXPIRING_LEAD_SECS`, honoured only when `VAMD_E2E_MOCK_SCRIPTS` is set (unit-tested), set by the runner to 90 d − 45 s. Fix cycle 1 (orchestrator review): the clearing rule and a whole-smoke proof that the shorter lead breaks no other scenario. Container restart during the agent's last runs; the orchestrator re-ran: `--full` only the known root-only `hsp_uds`; `--fast` smoke 96: 91 pass, 5 red right reason, 0 wrong. Scoreboard: 2 green (`expiring-grant-reaches-the-owner`, `skipped-schedule-is-told-once-a-day`), 6 red (right reason), 1 fix cycle, about 3 h. Disk 20 GB before and after. |
| 2026-11-04 | 2 W0b | **Wave 0b merged** (merge commit; Sonnet 5.5 low). Eight pending scenarios, red right reason, class `product`: six at `when.0` (`node-notify`, no `/v1/node/notify`), `expiring-grant-reaches-the-owner` at `when.5`, `skipped-schedule-is-told-once-a-day` at `when.9`. World pieces (5): `notify-broker` (fake transport, restarted with its gateway socket by `node-notify`; list it last in `given`), `slack-bot-token`, `slack-app-token`, `playbook-with-file-gate`, `playbook-with-named-gate`. Runner module `crates/vam-e2e/src/channels.rs`: `node-notify`, `link-contact`, `unlink-contact`, `notify-prefs` (quiet hours as whole minutes; `now` is runner shorthand), `channel-click` (forged, `from:<account>`, replay, `via-socket`, status), `expect.channel-messages` (reads `<state>/sent.jsonl`), `expect.broker-state`; probes `channelShownWithin60s`, `channelMedianSecs`, `triggeredDecidedFromChannel` merged into `bench.notify` (`bench.rs` untouched). The `channel-click` / `fake click` flag contract in `e2e/README.md` "Channels" is wave 4's to implement; the contacts, prefs and `/v1/node/notify` wire shapes the steps send are wave 2's to match or to amend in the runner. New `slow` tag keeps quiet hours out of smoke. `run.sh` builds the fake-transport broker in its own cargo call (the combined call produced a binary without the flag; root cause not found). Spec change (orchestrator): scenario F renamed, `budget.hit` to wave 1 unit tests. Orchestrator re-run: `--full` only the known root-only `hsp_uds`; `--fast` smoke 96: 89 pass, 7 red right reason, 0 wrong; quiet hours red at `when.0`. Scoreboard: 0 green, 8 red (right reason), 0 fix cycles, about 1 h. Disk 20 GB before and after. |
| 2026-11-04 | 2 | **Phase 2 working rules.** Asked first: merge to `main`, release, a locked decision or a `then` that changes product behaviour, a third fix cycle, a real model or a real Slack workspace. Push fails on signing: try the GitHub API, else keep local commits and continue. Actions only for the Firecracker or benchmark lane when required. Wave 0b approved with recommendations 1 (quiet hours in whole minutes; runner computes a one-minute window, `within: 150s`) and 2 (each refused click named). |
| 2026-11-03 | 2 W0a | **Wave 0a merged** (merge commit; Sonnet 5.5 medium). Contract (additive): manifest field 11 `NotificationChannel` (closed channel list `slack`; surface `[a-z]{3,16}`, not reserved; owner rec. 1: only CORE and FIRST_PARTY may declare it, a test per refused tier), `HostedNotificationChannel.DeliverNotification` with `NotificationRendering`, `HostedNotificationGateway` (`ActOnNotification` has no surface field; `LinkChannelAccount`; `GetChannelConfig` secrets per call). `vam_spec::validate_channel_decision` (owner rec. 2: closed set APPROVED/REJECTED/SUBMITTED or an offered option; AMENDED, remember, always refused), for wave 5. Re-approval gate `notification_channel_is_subset` in `hsp_registry.rs`. Crate `brokers/vam-notify` skeleton (adapter and transport traits, stub services over UDS, state dir 0700 with a lock only), feature `fake-transport` off by default; `scripts/e2e/check-notify-build.sh` (in the release gate) proves the default binary has no fake markers and no `vam-orchestrator` dependency; `run.sh` builds the feature binary into `e2e-bin` (`VAM_E2E_NOTIFY_BROKER`). `Cargo.lock` +131 (tokio-tungstenite 0.21 brings a third rustls version, 0.22; accepted). Orchestrator re-run: `--full` only the known root-only `hsp_uds` failure; `--fast` smoke 89/89, leftovers ok. Remote branch `nt-2/wave-0a` not deleted (auto mode refused; owner may delete). Scoreboard: 0 green, 0 red (no scenarios yet, by plan), 0 fix cycles, about 1.5 h. Disk 30 → 20 GB. |
| 2026-11-03 | 2 | **Breakdown approved** by the owner with recommendations 1 and 2 and every default; NT-2.8 `notify_send` to BACKLOG. Wave 0a started. |
| 2026-11-03 | 2 | **Breakdown drafted** (phase-breakdown agent, Opus 5.5 high), awaiting owner approval. Scenario delta: the design's 11 merged to 8 (recommendation 1: the three gateway-refusal ones into `slack-decision-needs-the-handle-and-the-linked-account`, the two rendering ones into `simple-jigs-render-in-slack-and-the-rest-open-in-vam`), 6 new world pieces, no existing `then` changed. Benchmark: P7 within a minute through the channel, P7 triggered-and-decided-from-a-notify-surface (exit scenario started by a trigger, recommendation 2); baseline slice 1 (P2 49/49, P7 46/46, median 1 s, mock), channel lines 0. Items NT-2.1 to NT-2.11 (NT-2.8 `notify_send` recommended BACKLOG). Waves 0a scaffold+contract (medium), 0b scenarios (low), 1 producers (low), 2 settings/contacts/prefs (low), 3 delivery + gateway (medium), 4 broker (medium), 5 `decide_as` + handles (medium), 6 Slack live transport (medium), 7 the VAM app (low). 17 open questions; default for the credential tension: handles in broker memory only, re-minted by `vamd` at broker start; tokens per call. Scoreboard: 0 green, 0 red (not written), 0 fix cycles |
| 2026-11-02 | 1 W4 | **Wave 4 merged** (merge commit into `feature/notifications`; Sonnet 5.5 medium). Badge (`action + warn`, 15 s poll with etag, 304 keeps the count) and notifications view in the sidebar; detail drawn with the existing `ActionCard`, `QuestionCard` and `GateForm` unforked (fallback to the thread only when no approval is attached); agent text fenced and labelled apart from host fields; open → read, `open` action → acted, never for decision kinds. One shared `http_client()` sends `X-Vam-Surface: app` on every vamd request (owner, recommendation 2; the external proxy fetch keeps its own client); captured-request test proves a decide carries it. Orchestrator re-ran: `CI=1 pnpm test` 789 pass, `tsc` clean, `src-tauri` 54 pass. Note: the agent installed GTK/webkit dev packages in the sandbox to build Tauri. Scoreboard: `badge-counts-what-waits-on-you` proven by unit tests (stays `needs: ui`), 0 fix cycles, about 1 h. Disk 21 → 19 GB. |
| 2026-11-02 | 1 | **Released to `main`** on the owner's word: (merge commit; Actions: checks, workspace tests, smoke green; release gate PASS locally; microVM lane advisory, not awaited) and . Bases fast-forwarded to `main`. Team checklist out. Next: Phase 2 (slice 2) breakdown. |
| 2026-11-02 | 1 | **Phase 1 closed in base.** 7/7 process scenarios green; benchmark P2 100% (49/49), P7 100% within 60 s (46/46), median to act 1 s on the mock; team checklist written. Release PRs (vam first, then the VAM app) wait on the owner's word. |
| 2026-11-02 | 1 W3 | **Wave 3 merged** (merge commit; Sonnet 5.5 low). CLI `vam notifications [--unread] [--kind] [--all] [--json]`, `show <id>`, `read <id>\|--all` (one page plus a "more" hint; decision rows print `vam approve decide <id>`). Tool `notifications_list` through the existing `ExecuteHostTool` (no proto change; `reserved_builtin_tool_names` untouched, no digest moved): READ_ONLY, run owner and source from the leased run, refused when `runs.source` is set, host fields only (no `agentText`), 4 KiB cap, never marks read or shown; guest change is the definition only (`vam-runtime/src/skills.rs`). Mock-fixture fix (orchestrator): two mock rules `afterTool` → `toolName`; `when`/`then` untouched. Fix cycle 1 (orchestrator review): opt-in held only under DENY defaults; now offered and executable only with an explicit `builtin:notifications_list` allow rule whatever the default (session rows, catalog, call path), tested under ALLOW, BY_SIDE_EFFECT, ASK, DENY. Process note: the plan-only agent's worktree was auto-removed, so its build turn first checked out in the main checkout; moved to its own worktree before any edit. Orchestrator re-run: smoke 89 pass, 0 fail, leftovers ok (a first run overlapped the agent's own `--full` and failed the leftovers check only); `--full` only the known root-only `hsp_uds` failure. Scoreboard: 1 green (`agent-reads-what-waits-without-acting`; all 7 process scenarios green), 0 red, 1 fix cycle, about 2.5 h. Disk 22 → 21 GB. |
| 2026-11-02 | 1 W2 | **Wave 2 merged** (merge commit; Sonnet 5.5 medium). `run.finished` written inside `insert_run_receipt` in the receipt's transaction (gather/build untouched); a 60 s no-keys pass in `issue_missing_receipts` records without a digest, filled when the receipt lands; one record per run. Rule `run_finished_wanted`: trigger (`broker:`), waited (published approval row) or failed; excluded: user stop (`reason: stopped`, incl. owner-stopped trigger runs), `broker:vam.delegate` and `report:` sources (named constants). NT-1.4 `record_report`/`record_delegation_end` (no caller). Accepted extra: the runner marks earlier records read before each scenario (`quiesce_notifications`), since finished runs now leave records in the shared vamd. Orchestrator resolved a tracker-row conflict on the PR branch, then re-ran: smoke 88 pass, 1 red right reason, 0 fail; `--full` only the known root-only `hsp_uds` failure. Scoreboard: 1 green (`background-run-end-notifies-and-chat-turn-does-not`; 6 of 7 process green), 1 red (right reason), 0 fix cycles, about 1.5 h. Disk 22 GB before and after. |
| 2026-11-02 | — | Tracker opened from `design/notifications.md` (approved 2026-11-02); base `feat/notifications` cut from `main` |
| 2026-11-02 | 1 W1 | **Wave 1 merged** (merge commit; Sonnet 5.5 medium). `notifications.rs` (table, `record()`/`resolve()`/`resolve_for_run()`, kinds as data, prune in the 60 s sweeper, one-time scope migration for keys with `approvals:write`), `api/notifications.rs` (list `pageSize`/`pageToken`, summary with etag/304, get with `agentText` and the approval, read, bulk read, acted with 409 `decide_required`; surface only from `X-Vam-Surface`). Producers in the same transaction: `create_approval` (reached by a parked run through `apply_guest_task_outcome`, `api/mod.rs:4520`), `insert_gate_approval`; resolution in decide (acted + surface + user), expire, stop, gate timeout, skipped gate (`superseded`); reauth parks get no record. Accepted extra: `update_run_state` closes a run's open decision records as `run_ended` on terminal state (same transaction). Fix cycle 1 (owner-approved): runner scopes `count` without `run:` to the scenario's runs; expectFail moved for the two later-wave scenarios (`then.0`, `when.4`, class product; a notification-wait timeout now reads as product); whole-invocation e2e budget 10m → 15m (smoke scenario time 465 s → 525 s). Orchestrator re-run: smoke 87 pass, 2 red right reason, 0 fail; `--full` only the known root-only `hsp_uds` failure. Scoreboard: 5 green (`approval-in-another-thread-reaches-the-list`, `decision-anywhere-clears-the-notification`, `gate-left-open-shows-until-it-times-out`, `restarted-approval-keeps-one-notification`, `notifications-are-never-shown-to-another-user`), 2 red (right reason), 1 fix cycle, about 3 h. Disk 22 GB before and after. |
| 2026-11-02 | 1 W0 | **Wave 0 merged** (merge commit; Sonnet 5.5 low). Eight scenarios (7 process `pending`, red right reason, class `product`, 404 on `/v1/notifications`: `approval-in-another-thread-reaches-the-list` at `when.4`, the other six at `when.2`; `badge-counts-what-waits-on-you` `needs: ui`, skipped). World pieces `playbook-with-timed-gate`, `agent-reads-notifications`; runner module `crates/vam-e2e/src/notifications.rs` (steps `notifications`, `notification-summary`, `notification-mark` get/read/acted, `via: cli\|app`; probes `bench.notify.pendingListed` 0/7, `shownWithin60s`/`medianToActSecs` unmeasured). Shapes W1 must match (README): list `{records, pagination.nextPageToken}`, `pageSize`/`pageToken`; flat summary `{action, warn, unread}`; 409 body has `decide_required`; camelCase fields. W3 note: the triggered-run refusal is asserted as `tool.failed`. Spec change (owner, B): `restarted-approval-keeps-one-notification`. Orchestrator re-run at the head: `--full` all workspace tests pass except the known root-only `hsp_uds::uds_socket_acl_deny_fails_fast`; `--fast` smoke 82 pass, 7 red right reason; 7 named red right reason; `vam-e2e check` 99/31. Actions fast jobs started, not awaited (merged on local proof). Scoreboard: 0 green, 7 red (right reason), 0 fix cycles, about 1.5 h. Disk 30 → 22 GB. |
| 2026-11-02 | 1 | **Spec change (owner, option B):** `retried-approval-keeps-one-notification` becomes `restarted-approval-keeps-one-notification`; dedupe stays one record per approval (run-scoped subject), no cross-run grouping or reopen. |
| 2026-11-02 | 1 | **Breakdown approved** by the owner with recommendations 1–3 and all defaults. |
| 2026-11-02 | 1 | **Breakdown drafted** (phase-breakdown agent, Opus 5.5 high), awaiting owner approval. Scenario delta: 8 new (7 process, 1 ui pending), 2 new world pieces, no existing `then` changed. Benchmark: P2 and P7 lines already in `benchmark.md` §1; probes added in W0, baseline P2 0%, P7 unmeasured. Waves: W0 low (scenarios + runner module), W1 medium (NT-1.1/1.2/1.5), W2 medium (NT-1.3/1.4), W3 low (NT-1.6/1.7), W4 medium (the VAM app NT-1.8). Found on `main`: the app sends no surface header; a user stop ends `FAILED`/`stopped`; a skipped gate is a fifth resolution path; `issue_receipt` drops the insert flag; a fourth approval kind `reauth`; stored keys need the new scopes. 12 open questions with defaults. Scoreboard: 0 green, 0 red (not written), 0 fix cycles |
