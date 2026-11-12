# Workstream: Work outside chat (E5 getting-started slice)

```yaml
status: active
slug: work-outside-chat
current_phase: 2
base_branch: feat/work-outside-chat
wave_branches: e5-2/wave-N   # Phase 2 closed in base 2026-11-01
design_docs:
  - docs/src/content/docs/hosted/triggered-runs.md
  - design/vam-apps-spaces.md         # Amendment 2026-10-29 §1–§6
  - docs/src/content/docs/hosted/schedule.md         # Phase 2
  - docs/src/content/docs/hosted/
```

## Goal

A user can leave work for the agent to do later, outside a chat: an approved
tool call leaves a first-party broker a **run grant**; the broker later starts a
run as that user, in a fixed Space and thread, through its own gateway socket,
or queues it behind the task that holds the workspace; the run's receipt says
it was requested via that grant. Phase 1 builds the machine side and proves it
with a Rust test broker; Phase 2 ships the schedule broker `vam.schedule`, so
"summarise my board every weekday at 18:00" works end to end with a receipt
that names the schedule. Every Space and thread gets an opaque id beside its
name. Launch demo: a scheduled `close-prep` run (the 0.1.0 launch plan
rows 6a and 6b).

## Out of scope

- Apps, shared Spaces, App authority, Space retention (`vam-apps-spaces.md`
  Phases 2, 4–6; cut from 0.1.0).
- `wait_for`, resume grants, `DeliverEvent`, `vam.github` (a later E5 phase,
  after E10 Phase 5).
- "Connect your services" (user-key credentials for vendor brokers).
- Slack and email delivery (`vam.notify`, E12 Phase 2).
- The schedule broker itself in Phase 1 (it is Phase 2; Phase 1 ships only the
  Rust test broker).
- Moving user API keys off `mint_api_key_secret`: a separate hotfix owns
  `keys.rs` until it merges; this lane reuses its CSPRNG generator.

## Locked decisions

Do not re-litigate without editing this list. Copied from
`docs/src/content/docs/hosted/triggered-runs.md` as amended by the
2026-10-15 amendment and by §2 of the 2026-10-29 amendment (P1 and P2
approved by the owner 2026-10-29, with the five §7 defaults).

1. **Scheduling.** The broker owns storage, tickers, cron, misfire and firing.
   `vamd` has no ticker and no schedule table: a grant row holds an approved
   run request and an expiry, no time of day, no recurrence, no clock. `vamd`
   does hold deadlines of a task (a queued fire's `not_after`).
2. **Registration** is through broker-exported tools (`schedule_create`, …).
3. **Identity** comes only from the `vamd`-stored grant. A broker never asserts
   it; on the gateway the calling broker is the socket the call arrived on.
4. **Grant storage.** A Credential (`vam-apps-spaces.md`): a row in `api_keys`
   with the SHA-256 of the secret in `vamd`, the token in the broker. Per-grant
   list and revoke. Holder `broker:<id>`; HTTP refuses any non-`http` holder.
5. **Grant issuance** only on `Invoke` of a tool in the approved
   `run_request_permission.grant_tools`, after the permission decision, and
   only when the arguments carry `run`. If the broker's Invoke fails, `vamd`
   revokes the grant it just minted.
6. **Loop guard.** No grant is issued inside a broker-started run: inside a
   triggered run `vamd` refuses the Invoke of any tool in
   `loop_guarded_tools` (⊇ `grant_tools`), read from the authoritative
   `runs.source`, never the evidence table.
7. **Busy target (P1 amendment §2.2).** A fire on a workspace with a live or
   parked task is queued through the inbox when the grant's `on_busy` is
   `QUEUE` (default): always a queued task, never a steer; at most one queued
   fire per grant (`BUSY ALREADY_QUEUED`); `not_after` → `EXPIRED`. `SKIP`
   answers `BUSY SKIPPED`. `BUSY ACTIVE_RUN` with `retry_after_ms` remains for a
   finalizing or stopping task. One live task per workspace stays.
8. **Run start.** One `start_run` shared by `POST /v1/runs`, the inbox queue
   starter and the gateway. No copy of `create_run`.
9. **Target shape (P1, approved 2026-10-29).** The target is resolved once, at
   issuance, to `space_id` and `thread_id` and bound in the grant with the
   agent, message, playbook, inputs, on-busy and expiry.
   `RequestRunTriggerRequest` carries no target and no prompt: only a key, a
   time, a label and a kind. The broker decides when; the grant decides who,
   what and where. `RunTarget` on `CreateRunRequest` is not part of E5.
10. **Ids are the identity; names are labels (owner, 2026-10-30; replaces
    the 2026-10-29 wording and the Apps-Spaces row "Existing workspaces keep
    their IDs. Backfill only adds rows.").** VAM is unreleased, so nothing
    from before ids is carried over: no backfill, no old-workspace support
    (deploy note: start from a fresh state directory). A Space is stored
    under its id alone (Drive9 workspace directory `sp_…`, no owner or agent
    prefix); a thread under `th_…`. Ids are `sp_`/`th_` + 26 base32 chars
    from `rand::rngs::OsRng`. Owner and bound agent live in the `spaces`
    row; every access goes through one function taking (owner, space id)
    that answers "not found" for a foreign id. Threads are keyed by
    `(space_id, thread_id)`; a `th_…` id resolves only inside its Space.
    Names are labels only: any characters (spaces allowed), not unique,
    never used for lookup; the user names threads for now (model naming
    later). A request with no Space id uses the agent's default Space
    (created on first use); with no thread id it starts a new thread
    labelled with the given name or "untitled"; follow-ups send the thread
    id that `RunStatus` returns. Grants, queued runs, receipts and run
    status carry ids. Until Apps ship, `app_id` is NULL (P2 stands).
11. **Field numbers.** `CapabilityManifest.run_request_permission = 10`;
    `InvokeRequest.run_grant = 9`.
12. **Broker development.** Phase 1 ships a Rust test broker; Phase 2 ships
    `vam.schedule` (`docs/src/content/docs/hosted/schedule.md`) in `brokers/` (E10 decision 22).
13. **Grant expiry (§7 default 2).** Always set: default 90 days, cap 365; a
    one-off schedule's grant at most fire time + 24 h; no silent renewal;
    `grant.expiring` 7 days ahead.
14. **Grant secrets** come from the CSPRNG generator the `keys.rs` hotfix
    introduces; no grant is minted on `mint_api_key_secret`.

## Status legend

- `[ ]` not started
- `[~]` in progress
- `[x]` done (code + tests, or merged)

## Phases

### Done phases (archived)

Full sections: not kept; see the progress log and git history.

| Phase | Name | Done |
|---|---|---|

### Phase 1 — Grants, gateway and ids

**Goal:** An approved tool call can leave a broker a grant; the broker can
later start a run as the user, in a fixed Space and thread, or queue it behind
the live task; every Space and thread has an opaque id. Proven with a Rust test
broker.  
**Depends on:** E10 Phase 3 (on `main`); the `keys.rs` CSPRNG hotfix before
the first grant is minted.  
**Plan:** drafted 2026-10-29; owner accepted recommendations 1–3 (Q1 default, receipt key agreed before wave 0 merges, Q6 settled by the hotfix: `OsRng`) 2026-10-29; id scoping (locked decision 10) and Q3 default approved 2026-10-29; **approved**  
**Exit:** the eight Phase 1 scenarios `active` and green in process mode;
every existing scenario green unchanged; `cargo test -p vam-spec`,
`-p vam-orchestrator`, `-p vam-cli`, `cargo check --workspace`,
`cargo fmt --all --check`; the Firecracker lane once (the start path moved).

Coordination points:
- **Receipt lane (E12, `run-receipt.md`), agreed 2026-10-29 and recorded there:** flat receipt key `requestTrigger` (Q4), populated in wave 9. E12 adds
  `ReceiptRequester.trigger = 3` / `ReceiptTrigger` to `receipt.proto`; this
  lane populates it. Record the wave here when the breakdown fixes it.
- **`keys.rs` hotfix:** owns `keys.rs` until merged; grant waves take `main`
  after it.
- **Proof lane (H2):** may add bench steps to the e2e runner; this lane keeps
  its runner steps in their own module and takes `main` before each wave.

<!-- Breakdown below is filled by the phase-breakdown agent. -->

Breakdown by the phase-breakdown agent, 2026-10-29, against base
`feat/work-outside-chat` (= `main` + tracker commit). `main` has
no code change since so the design's file:line citations hold on
this base (spot-checked: `api/mod.rs:1205, 2158, 2607`, `api/inbox.rs:333`,
`api/queue.rs:177`, `inbox.rs:104-205`, `keys.rs:142-189`,
`host_tools.rs:2830`, `run_context.rs:136-165`, `catalog.rs:1769, 1831`,
`vam_dir.rs:144, 210, 359`, `threads.rs:41-48`, the proto field numbers).
The `keys.rs` CSPRNG hotfix is **not** on `main` yet (no branch on origin).

#### Amendment 2026-10-30: ids first (owner)

Approved by the owner after wave 2 (locked decision 10 rewritten). Changes
to the breakdown below, which otherwise stands:
- **Scenarios:** `existing-workspace-keeps-its-id-and-history` and its world
  pieces `workspace-from-before-ids`, `agent-before-ids` are **retired**;
  **new** `names-are-only-labels` (two threads with one label get different
  ids; a label with spaces is kept as written; the thread list and
  `GET /v1/spaces` show id and label). `thread-is-reached-by-id-or-by-name`
  becomes **`thread-is-reached-by-its-id`**: its `then` changes (a name no
  longer reaches a thread) — spec change approved by the owner. Existing
  scenarios that send thread names for follow-ups (32 of 72) move their
  `when` to `thread: { id: latest }`; any `then` that asserts a thread name
  or a prefixed workspace id is listed in the wave's PR as a spec change for
  the owner.
- **Q1, Q3, Q9, Q10 are superseded:** `RunStatus.threadRef` carries the
  label, `threadId` the id; the thread list's `threadId` is the id; no
  backfill, no fixture.
- **Wave 3** (ids as identity): tables, id minting, the one access function,
  Space and thread resolution by id with the defaults above, `sp_…`/`th_…`
  directories, `RunStatus` ids, queued request carries ids, `GET /v1/spaces`;
  retires the old scenario and adds `names-are-only-labels` (green).
- **Wave 4** (routes, CLI, scenarios): thread and workspace-file routes and
  CLI by id; migrates the 32 scenarios' `when`; `thread-is-reached-by-its-id`
  green.
- **Create and rename (owner, 2026-10-30).** Users create more Spaces
  whenever they need, beside the agent's default one, and rename Spaces and
  threads in place (same id, new label). Wave 4 adds `POST /v1/spaces
  {agentUrn, label}` → id, `PATCH /v1/spaces/{id} {label}`, `PATCH
  /v1/spaces/{id}/threads/{threadId} {label}`, and CLI `vam space create`,
  `vam space rename`, `vam thread rename`; owner-scoped through the one access
  function; a rename never changes an id or a directory. Threads are created
  with a label (or "untitled"). Wave 4A exposes create and rename in the app.
- **Default Space split:** one default Space per (owner, agent) for released
  versions and one for drafts, exactly today's split; never one per version.
- **Grants end with their user (owner, 2026-10-30).** When a user is
  deactivated (today's only removal path; there is no user-delete route),
  every grant they hold is revoked (`deactivate_user` →
  `revoke_api_keys_for_owner`, which covers grant rows; wave 6 adds the
  test) and stays revoked if the user is reactivated. Wave 8 also discards
  any queued fire from those grants at deactivation, and the gateway
  answers `DENIED OWNER_DISABLED`. Any later user-delete route must delete or
  revoke the user's grants in the same transaction.
- **Grant privacy (owner, 2026-10-30).** Grants are owner-scoped for every
  role: an admin or operator cannot list, read or revoke another user's
  grants (a foreign grant is "not found"). Operators stop all of a broker's
  grants by disabling the broker (wave 8).
- **New wave 4A, the VAM app** (the app repo, base
  `feature/work-outside-chat` cut from the VAM app `main`; Sonnet 5.5 medium —
  both repos' contract): the app sends Space and thread ids for follow-ups
  and shows labels; replaces Phase 2's WO-2.6. Must land before the release.

#### Scenario delta

All new, `needs: process`, tags `triggers`/`workspace`. Ids and files are
behaviour names (no plan numbers).

| Scenario | Change | Layer | Given | Why |
|---|---|---|---|---|
| `trigger-starts-a-run-as-the-grant-owner` | new | process | org-admin, agent-with-trigger-tool, trigger-test-broker | the phase's exit: an approved call leaves a grant; a fire starts a run as the user in the bound thread; `RunStatus.source` and the receipt name the trigger, grant and the manifest's surface |
| `repeated-fire-with-the-same-key-starts-one-run` | new | process | same | gateway dedupe: three fires with one key → one run, `DUPLICATE` twice with that run id |
| `revoked-grant-starts-nothing` | new | process | same | user revokes (`DELETE /v1/keys/{id}` / `vam keys revoke`); next fire `DENIED REVOKED`; no run |
| `grant-token-is-refused-over-http` | new | process | same | P1 floor: `POST /v1/runs` with the grant as bearer is 401, both as the full `vam_rg_…` token and as its bare secret part (the form whose hash is stored); no run |
| `triggered-run-cannot-arm-another-trigger` | new | process | same | loop guard: the triggered run calls the grant tool; the host refuses it (`tool.failed`); still one grant listed |
| `trigger-on-a-busy-workspace-runs-after-the-task` | new | process | same (the piece offers `run`) | busy → `QUEUED`; the queued run reads PENDING with no session while the slow task runs (parked cost 0), then starts and succeeds |
| `thread-is-reached-by-id-or-by-name` | new | process | org-admin, agent-with-builtins | a follow-up by `threadId` and one by name land in one thread; `GET /v1/spaces` and the thread list show id and name |
| `existing-workspace-keeps-its-id-and-history` | new | process | org-admin, workspace-from-before-ids | backfill: after `restart-vamd` the seeded workspace has a `spaceId`, the same `workspaceId`, and its thread history reads unchanged by name and by id |

**Existing scenarios whose `then` changes: none**, provided open questions
Q1–Q4 keep their defaults (all new response fields are additive;
`threadRef`, `workspaceId`, `requestedBy`, `requestSurface` and the default
`GET /v1/keys` list keep their values). **No spec change is requested.** If
the owner picks a non-default answer to Q1, Q3 or Q4, the affected scenarios
below become a spec change and are listed again then.

Re-run unchanged (their `guards` or code paths are touched): 
`queued-message-runs-after-the-task`, `steer-changes-the-running-task`,
`correction-sent-mid-task-is-followed`, `follow-up-continues-after-stop`,
`follow-up-continues-after-vamd-restart`, `follow-up-uses-earlier-tool-results`,
`follow-up-after-compaction`, `long-thread-resumes-after-approval`,
`thread-history-lists-tool-results`, `work-survives-vamd-restart-mid-task`
(start path, thread resolution, `chat {thread: continue}` echoes `threadRef`);
`member-chats-with-a-released-agent`, `api-rate-limits-a-flood` (`keys.rs`,
`auth.rs`, `rate_limit.rs:298` calls `authenticate_bearer`);
`receipt-names-who-decided-and-from-where` and the other `receipt-*`
(`receipt.rs`); `host-tool-output-fits-frame`,
`secret-never-reaches-guest-encoded` (`host_tools.rs`);
`agent-workspaces-are-isolated` (workspace resolution).

**World pieces** (Wave 0 writes them; each arranges on today's vamd, since an
arrange failure is reported at `given`, not at a `when` step):
- `trigger-test-broker` (new piece kind `broker`): starts the test broker
  (`$VAM_E2E_TEST_BROKER`, the `hsp-echo-broker` binary `run.sh` copies to
  `e2e-bin`) on a short socket path, registers it with `PUT
  /v1/hosted/brokers` (bootstrap key), id `{{name}}`; manifest lists the
  grant tool `echo_arm_trigger` (`MUTABLE_EXTERNAL`). Wave 0 registers it
  **without** `runRequestPermission` (today's proto JSON refuses the key);
  the test-broker wave adds it (`grantTools`, `loopGuardedTools`, `surface:
  echo`). Cleanup: stop the process; disable the approval once `DELETE
  /v1/hosted/brokers/{id}` exists (gateway wave), so its preloaded tools do
  not reach later scenarios.
- `agent-with-trigger-tool` (new agent): read/write/run allowed and the
  test broker's grant tool on `ask` (`requires: [trigger-test-broker]`).
  The design's `given` names `agent-with-builtins`, but that piece is
  `default: deny` with builtin rules only, so it cannot call a hosted tool.
- `workspace-from-before-ids` (new piece kind `workspace-seed`, Q9): a
  checked-in Drive9 fixture of one legacy workspace (a `main` thread with a
  user, tool and assistant record, a file, `thread.json` with no `id`),
  copied under the scenario owner's prefix and a run-tagged name.

**Runner steps** (own module `crates/vam-e2e/src/triggers.rs`, wired into
`scenario.rs`/`engine.rs`/`check.rs` the way `steer.rs` is; take `main`
before each wave because the proof lane may add bench steps):
- `expect: { grant: { tool, fields, count, absent } }` — reads `GET
  /v1/keys?holder=broker` (the user's grants).
- `fire-trigger: { key, times, kind }` — runs `$VAM_E2E_TEST_BROKER fire
  --state-dir <piece dir> --key <k> --times <n>` (contract fixed here; built
  in the test-broker wave), records each outcome; an `ACCEPTED`/`QUEUED`
  run becomes the latest run (as `send {mode: queue}` does), so `expect:
  {run}`, `{event}`, `{receipt}` and `{thread}` follow it.
- `expect: { fire: { outcomes: [..], reason, sameRun } }` — judges the last
  `fire-trigger`.
- `revoke-grant: { via: api | cli }` — the latest grant.
- `bearer-request: { method, path, token: grant | grant-secret, status }` —
  one request with the grant from the broker's state dir; never printed.
- `chat { thread: { id: latest } }` and `expect: { spaces: { fields } }`,
  `expect: { threads: { fields } }` — read `GET /v1/spaces` and the thread
  list.

Red for the right reason (class `product`): the six trigger scenarios fail
at their first `expect: { grant }` (no grant is listed today: the query is
ignored and only API keys come back); the two id scenarios fail at the first
read of `threadId` / `GET /v1/spaces` (absent / 404).

#### Benchmark delta

| Line (`benchmark.md` §1, P7) | Before | After Phase 1 | How |
|---|---|---|---|
| tasks started by a trigger … that complete | 0 | **0 by construction, not measured** | a Rust test broker is not a product trigger; the line moves in Phase 2 (the scheduled task) |
| parked cost (must be 0) | not measured | **0, proven** for a queued fire | `trigger-on-a-busy-workspace-runs-after-the-task`: the queued fire is a PENDING row with no session until the task ends |

No task is added to the task set; no paid baseline run (nothing on the line
can move). The phase-close scoreboard records the two rows above as
findings.

#### Items

Item ids follow the design (the `work-outside-chat` tracker (E5 Phase 1)); WO-1.3 and
WO-1.6 are split so each wave fits one session, and WO-1.8 (docs) is added.

- [ ] **WO-1.1 Contract.** Protos of `docs/src/content/docs/hosted/triggered-runs.md` in
  `hosted_services.proto` (`RunRequestPermission`, `RunGrant`,
  `GrantTarget`, `RunGrantRequest`, `PlaceChoice`, enums,
  `CallerContext.space_id/thread_id/run_source` 8–10,
  `HostedTriggerGateway` with server and client codegen) and
  `workflow.proto` (`RunSpec` 12–13, `RunStatus` 20–22, `RunSource`);
  `orchestrator.proto` `Space`, `Thread`, `ListSpaces*`, and what the design
  leaves out but the routes need: `ApiKeyRecord` gains `holder`,
  `target_kind`, `target_id`, `target_label`, `message_preview`,
  `issued_via`, `tool_name`, `expires_at`; `ListApiKeysRequest.holder`;
  `PutHostedBrokerRequest.peer_uid`; `HostedBrokerRecord.gateway_socket`.
  Short enum names in JSON for `RunGrantRequest` (Q5). Validation in
  `vam-spec/src/validation.rs` (subset rules, `surface` charset and reserved
  names, third-party tier refused) and the approved-manifest subset gate in
  `hsp_registry.rs`; baseline fixtures in `proto/vam/v1/baseline`.
  **Not** `receipt.proto` (E12's). Files: `proto/vam/v1/*.proto`,
  `crates/vam-spec/{build.rs,src/validation.rs,src/encoding.rs}`,
  `hsp_registry.rs`. Accept: protos compile, field numbers as locked
  (decision 11), a manifest with `runRequestPermission` round-trips through
  `PUT /v1/hosted/brokers`, the schedule broker's `run` JSON
  (`docs/src/content/docs/hosted/schedule.md` "`schedule_create` arguments", with `"SAME"`,
  `"NEW"`, `"QUEUE"`, `null`s) decodes to `RunGrantRequest`, unknown keys
  refused. Tests: `cargo test -p vam-spec`, `cargo test -p vam-orchestrator
  hsp_registry`, `cargo check --workspace`. Turns green: none (scaffold).
  `Decide: SLICE — proof 3 (demo), cost 2 (proto) → slice: fields the two
  phases read; event_context and DeliverEvent reserved, not built.`
- [ ] **WO-1.7 Test broker and world piece.** `hsp-echo-broker --triggers
  --state-dir D`: exports `echo_arm_trigger {run, after_ms?, times?}`, keeps
  each `RunGrant` it receives in `D` (test-only binary), answers the tool
  call with `GrantTarget` labels, never the token; `after_ms` fires on its
  own (for the team checklist); subcommand `hsp-echo-broker fire
  --state-dir D [--gateway-socket P] --key K [--times N] [--kind run_now]`
  prints one JSON line per outcome (the runner's contract). Gateway client
  from the WO-1.1 codegen. `trigger-test-broker` gains
  `runRequestPermission`. Files: `src/hsp_echo.rs`,
  `src/bin/hsp-echo-broker.rs`, `e2e/world/trigger-test-broker.yaml`,
  `crates/vam-e2e/src/triggers.rs` (only if the contract needs a fix).
  Accept: unit test with an in-process fake gateway; the piece arranges.
  Tests: `cargo test -p vam-orchestrator hsp_echo`, `scripts/e2e/run.sh
  --scenarios <the six trigger scenarios>` still red at `expect: {grant}`.
  Turns green: none. `Decide: NOW — proof 2 (the phase's scenarios cannot
  run without it), cost 1 (test binary, world piece).`
- [ ] **WO-1.3a Space and thread ids: store, resolution, backfill.**
  `spaces` and `threads` tables (`vam-apps-spaces.md` amendment §3–§4), new
  `spaces.rs`; ids `sp_…`/`th_…` from an OS CSPRNG helper outside `keys.rs`
  (Q6); id-or-name resolution in `run_context.rs`
  (`resolve_run_workspace_id`, `resolve_thread_ref`: id wins, both must
  agree, else `InvalidContract`) called from `create_run`; `RunSpec`
  ids honoured, `RunStatus.space_id/thread_id` in `build_run_status`;
  `inbox::request_from_resolved` carries ids; `ThreadMeta.id/name` with
  serde defaults, `ensure_thread_json` writes them; new threads' directory
  per Q1 with `RunStatus.threadRef` and the thread routes still answering
  the `{owner}--{name}` value (so `chat {thread: continue}` and the thread
  reads of every existing scenario stay green in this wave); backfill at
  start and lazily on first touch, adopting a `thread.json` id when present
  (Q10); `GET /v1/spaces`, `GET /v1/spaces/:id`. Files: `spaces.rs` (new),
  `run_context.rs`, `api/mod.rs` (`create_run`, `build_run_status`, routes),
  `inbox.rs`, `vam-storage/src/vam_dir.rs`, schema init, `docs/api/openapi.yaml`.
  Accept: unit tests for resolution (id, name, both agree, both disagree,
  foreign owner), backfill idempotent and add-only, legacy and agent-scoped
  workspaces with one name are two Spaces. Tests: `cargo test -p
  vam-storage`, `-p vam-orchestrator spaces run_context`, `scripts/e2e/run.sh
  --smoke` and the re-run list above. Turns green:
  `existing-workspace-keeps-its-id-and-history`. `Decide: SLICE — proof 2
  (grants must bind ids, not reusable names; U6), cost 2 (tables, API,
  proto) → slice: ids and labels, backfill, no Apps, no rename UI, no
  retention.`
- [ ] **WO-1.3b Ids on every route and in the CLI.** Thread routes `:id`
  take `sp_…`, a workspace id or a name and `:thread` takes `th_…` or a
  name; thread list adds `name`, `spaceId` and `threadId` per Q3;
  `api/workspace_files.rs` takes `sp_…`; CLI `--workspace`/`--thread` take
  either, `vam run` prints `space sp_… (name) · thread th_… (name)`, `vam
  thread ls` shows id and name, new `vam space ls` (from `GET /v1/spaces`;
  `vam workspace ls` already lists files). Files: `api/threads.rs`, `api/workspace_files.rs`,
  `vam-cli/src/cmd/{run,thread,workspace}.rs`, openapi. Accept: route and
  CLI tests (httpmock). Tests: `cargo test -p vam-cli`, `-p vam-orchestrator
  threads`. Turns green: `thread-is-reached-by-id-or-by-name`. `Decide: NOW
  — proof 2 (same line as WO-1.3a: a person or script reaches a thread by
  a stable id), cost 1 (existing routes and flags).`
- [ ] **WO-1.4 Shared start path and run source.** `run_start::start_run(state,
  RunCaller, CreateRunRequest)` extracted from `create_run`; the HTTP handler
  keeps auth, PDP and the idempotency header; the queue starter
  (`api/queue.rs:177`) calls `start_run` with a `RunCaller` built from
  `QueuedCaller` (no more synthetic `AuthContext` + HTTP handler call);
  `inbox::admit` and `queue_message` take `RunCaller` and return a typed
  outcome (queued run id, steered, busy reason) that the HTTP handler
  renders (the gateway needs the typed form); `runs.source`,
  `runs.credential_id` in `persist_run_spec_with_owner`; `QueuedCaller`
  gains `source`, `credential_id`, `not_after` (serde defaults);
  `run_inbox.credential_id`; `admit` keeps playbook and inputs for a
  triggered queued run (people's refusal unchanged); one queued fire per
  credential; past `not_after` the starter ends the run `EXPIRED`. Files:
  `api/mod.rs`, new `run_start.rs`, `api/inbox.rs`, `api/queue.rs`,
  `inbox.rs`, `inbox_store.rs`, `catalog.rs`. Accept: no copy of
  `create_run` (decision 8); unit tests for the typed admit, the queued
  re-check and `EXPIRED`; every existing scenario green unchanged; the
  Firecracker job runs on the PR (no `skip-vm-lane`). Tests: `cargo test -p
  vam-orchestrator inbox queue run_start`, `scripts/e2e/run.sh --all`.
  Turns green: none (enabler). `Decide: SLICE — proof 3 (no trigger without
  it), cost 2 (refactor of the busiest handler) → slice: extraction plus the
  fields the gateway needs; typed start errors only where the gateway maps
  them.`
- [ ] **WO-1.2 One Credential type, for grants.** Nullable `api_keys`
  columns (`holder`, `target_kind`, `target_id`, `constraints_json`,
  `issued_via`, `external_ref`, `expires_at`, `source_run_id`, `tool_name`;
  NULL reads as holder `http`); `check_credential(secret, transport)`
  (hash, constant-time, holder matches transport, not revoked, not expired)
  in a new `credentials.rs`; HTTP bearer auth refuses every non-`http`
  holder with 401 inside the `authenticate_bearer` path, so the rate
  limiter's call (`rate_limit.rs:298`) refuses it too; `GET
  /v1/keys?holder=` (Q2) with the new fields, owner-scoped; `DELETE
  /v1/keys/{id}` revokes a grant; `vam keys list --grants` (holder, target
  labels, message preview, `issued_via`, `expires_at`, `last_used_at`) and
  the alias `vam grants list|revoke` (Q12). **Needs the `keys.rs` hotfix
  merged and the base synced** (it edits `keys.rs`). Files: `keys.rs`,
  `credentials.rs` (new), `api/auth.rs`, `api/mod.rs` (keys handlers),
  `vam-cli/src/cmd/keys.rs` (+ `grants` alias), openapi. Accept: unit tests
  — a grant row's secret as bearer is 401; a user key is unchanged; NULL
  columns read as `http`; list without `holder` equals today's list.
  Tests: `cargo test -p vam-orchestrator keys credentials auth`, `-p
  vam-cli`, scenarios `member-chats-with-a-released-agent`,
  `api-rate-limits-a-flood`. Full CI green before merge. Turns green: none
  alone. `Decide: NOW — proof 3 (P1 floor: a grant must not open HTTP),
  cost 1 (keys.rs, auth.rs, existing routes).` Alias: `Decide: BACKLOG —
  proof 1 (checklist reads plainer), cost 0 → carried by this wave.`
- [ ] **WO-1.5 Grant issuance and loop guard.** In `invoke_hosted_tool`
  (`host_tools.rs:2830`), after the approval and identity checks and before
  preflight: loop guard (tool in `loop_guarded_tools` and the calling run's
  `runs.source` starts with `broker:` → host error); `args.run` decoded as
  `RunGrantRequest`; resolve and validate in a new `grants.rs` (agent;
  Space `SAME`/`NEW`/`ID` with rows created at issuance; thread
  `SAME`/`NEW`/`ID`, `NEW` = `reserved` row; playbook and inputs via the
  existing admission and `input_schema` checks; message ≤ 32 KiB; expiry
  default 90 d, cap 365 d; `max_active_grants_per_user`); typed tool errors,
  nothing minted on failure; mint `rg_…` and the secret with the hotfix's
  generator (Q6); insert the row; set `InvokeRequest.run_grant`; push the
  token into `trace.secrets` so the existing redaction removes it from any
  broker echo (**the isolation-truth redaction code is not edited**); revoke
  in the same handler if the broker call fails or returns a tool error.
  **Needs the hotfix.** Files: `host_tools.rs` (`invoke_hosted_tool` only),
  `grants.rs` (new), `spaces.rs`. Accept: unit tests — no grant without
  `run`, outside `grant_tools`, or on a denied call; revoke on broker
  failure; loop guard reads `runs.source` only; the token never appears in
  the tool result or thread. Tests: `cargo test -p vam-orchestrator
  host_tools grants`, `host-tool-output-fits-frame`,
  `secret-never-reaches-guest-encoded`. Full CI green before merge. Turns
  green: `grant-token-is-refused-over-http`. `Decide: NOW — proof 3 (P1:
  grants only for approved calls; demo), cost 1 (one handler, new tests).`
- [ ] **WO-1.6a Gateway.** New `gateway.rs`: one Unix socket per enabled
  approval that declares `run_request_permission` (opened at start and on
  `PUT`, closed on disable; path per Q7; `SO_PEERCRED` when `peer_uid` is
  set); `hsp_broker_approvals` gains `gateway_socket`, `peer_uid`; `GET
  /v1/hosted/brokers/{id}` shows the path; new `DELETE
  /v1/hosted/brokers/{id}` (`brokers:admin`, disables the approval, closes
  the socket; Q8). `RequestRunTrigger` checklist steps 1–13 with typed
  outcomes, dedupe key `hsp:<broker_id>:<key>`, busy → typed admit from
  WO-1.4 (`QUEUED`, `BUSY SKIPPED/ALREADY_QUEUED/ACTIVE_RUN`), start through
  `start_run`; `RevokeRunGrant` (possession, idempotent, discards a pending
  queued fire); in-memory per-broker rate limits; `trigger.fired` outbox
  event and `vam_hsp_trigger_total{broker_id, outcome}`; refusals logged
  with grant id and reason, never the token; queue starter re-checks the
  grant at start. **Needs the hotfix** (token check). Files: `gateway.rs`
  (new), `hsp_store.rs`, `api/mod.rs` (broker routes, start-up wiring),
  `api/queue.rs`, `run_start.rs`, events catalog, openapi. Accept: unit
  tests per checklist row (wrong socket → `WRONG_BROKER`, revoked,
  expired, owner disabled, trigger-ref pinned, rate limited, duplicate,
  target gone); no broker id read from a request. Tests: `cargo test -p
  vam-orchestrator gateway`, the five scenarios below. Full CI green before
  merge. Turns green: `repeated-fire-with-the-same-key-starts-one-run`,
  `revoked-grant-starts-nothing`, `triggered-run-cannot-arm-another-trigger`,
  `trigger-on-a-busy-workspace-runs-after-the-task`. `Decide: SLICE — proof
  3 (demo), cost 3 (new trust boundary) → slice: two RPCs, one socket per
  broker, in-memory limits; DeliverEvent later.`
- [ ] **WO-1.6b Trigger facts on the run, the receipt and the caller.**
  `run_requests` gains `broker_id`, `grant_id`, `trigger_ref`, `fire_at`,
  `fire_kind`, surface from the manifest; `RunStatus.source` (22);
  receipt key per Q4 (populated here; **E12 coordination point**); CLI
  receipt line "requested by …, surface echo — trigger … via grant rg_…
  (broker), fire time …"; `CallerContext.space_id`, `thread_id`,
  `run_source` stamped in `caller_context.rs` (and `thread_handle` keeps its
  `{owner}--{name}` value); `grant.expiring` once per grant, 7 days ahead,
  from the existing TTL sweep (`ttl.rs`; Q11). Files: `run_facts.rs`,
  `receipt.rs`, `api/mod.rs` (`build_run_status`), `caller_context.rs`,
  `ttl.rs`, `vam-cli/src/cmd/receipt.rs`, events catalog. Accept: receipt
  of a person's run is byte-identical in shape (no new key); verify still
  passes offline. Tests: `cargo test -p vam-orchestrator receipt run_facts
  caller_context ttl`, `-p vam-cli`, the `receipt-*` scenarios. Turns
  green: `trigger-starts-a-run-as-the-grant-owner`. `Decide: NOW — proof 3
  (launch demo: the receipt says "via grant"; P7), cost 1 (columns on
  existing tables, one receipt key, one event).` (`grant.expiring` is
  locked decision 13: not scored.)
- [ ] **WO-1.8 Docs.** Starlight `hosted/triggered-runs.md` rewritten to
  amendment §3–§6 (drops "no `vamd`-side queue" and the fire-time
  `RunTarget`), `hosted/protocol.md` (gateway RPCs, fields 9/10),
  `hosted/capability-manifest.md` (`runRequestPermission`), grants and ids
  in the keys/CLI and orchestrator pages, a "try it with the test broker"
  section that the team checklist follows; `docs/astro.config.mjs` if pages
  move. Files: `docs/src/content/docs/**`. Accept: docs build; every route
  and flag named exists. Turns green: none. `Decide: NOW — proof 2 (install
  proof: an operator can turn it on), cost 0 (docs).`

#### Waves

| Wave | Items | Turns green | Repo | Model | Budget | Depends on | Status | PR |
|---|---|---|---|---|---|---|---|---|
| 0 | scenarios, world pieces, runner steps | — (8 red at the right step, class `product`) | vam | Sonnet 5.5 low | 90 min | — | [x] | — |
| 1 | WO-1.1 | — (scaffold) | vam | Sonnet 5.5 medium — new protos across three files, codegen, validation | 90 min | 0 | [x] | — |
| 2 | WO-1.7 | — | vam | Sonnet 5.5 low | 60 min | 1 | [x] | — |
| 3 | WO-1.3a (amended: ids as identity) | names-are-only-labels, thread-is-reached-by-its-id | vam | Sonnet 5.5 medium | 120 min | 1 | [x] | — |
| 4 | WO-1.3b (amended: create and rename Spaces and threads, CLI by id) | space-and-thread-can-be-renamed-in-place | vam | Sonnet 5.5 medium — new API | 90 min | 3 | [x] | — |
| 5 | WO-1.4 | — (enabler; all existing green) | vam | Sonnet 5.5 medium — changes the run start and queue state machine; retry at high if it fails twice | 120 min | 3 | [x] | — |
| 6 | WO-1.2 | — | vam | Sonnet 5.5 medium — owner rule (auth, security floor); full CI green | 75 min | 1, **`keys.rs` hotfix on `main` + base synced** | [x] | — |
| 7 | WO-1.5 | grant-token-is-refused-over-http | vam | Sonnet 5.5 medium — owner rule (grant issuance); full CI green | 120 min | 2, 3, 5, 6, hotfix | [x] | — |
| 8 | WO-1.6a | repeated-fire-with-the-same-key-starts-one-run, revoked-grant-starts-nothing, triggered-run-cannot-arm-another-trigger, trigger-on-a-busy-workspace-runs-after-the-task | vam | Sonnet 5.5 medium — owner rule (gateway, new trust boundary); full CI green | 150 min | 5, 7, hotfix | [x] | — |
| 9 | WO-1.6b | trigger-starts-a-run-as-the-grant-owner | vam | Sonnet 5.5 medium — crosses run_facts, receipt, caller context, CLI; E12 coordination | 90 min | 8 | [x] | — |
| 10 | WO-1.8 | — | vam | Sonnet 5.5 low | 45 min | 9 | deferred | |

One wave at a time. Hotfix: waves 6, 7 and 8 need the `keys.rs` CSPRNG
hotfix merged to `main` and the base synced first (6 edits `keys.rs`; 7
mints grant secrets; 8 checks them). Waves 0–5 do not (wave 3 mints `sp_`/
`th_` ids, which are not secrets, from an OS CSPRNG helper outside
`keys.rs`, Q6). If the hotfix is late, waves 0–5 run and the phase waits at
wave 6. Phase close: the Firecracker lane once on a KVM host (the start
path moved in wave 5).

**Shared surfaces per wave** (take `main` before each wave):

| Wave | Shared surfaces | Rule |
|---|---|---|
| 0 | e2e runner (`scenario.rs`, `engine.rs`, `check.rs` hooks; steps in new `triggers.rs`), `scripts/e2e/run.sh` (copy `hsp-echo-broker`, export `VAM_E2E_TEST_BROKER`) | proof lane (H2) may add bench steps: keep ours in `triggers.rs`, hooks minimal |
| 1 | `workflow.proto` thread/space fields (ours), `hosted_services.proto`, `orchestrator.proto`, `vam-spec` validation/encoding | `receipt.proto` untouched (E12's) |
| 2 | `hsp_echo.rs`, `bin/hsp-echo-broker.rs`, e2e world piece | — |
| 3 | `api/mod.rs` (ours this phase), `run_context.rs`, `inbox.rs`, `vam-storage/vam_dir.rs` | — |
| 4 | `api/threads.rs`, `api/workspace_files.rs`, `vam-cli` run/thread/workspace | — |
| 5 | `api/mod.rs`, `api/inbox.rs` (ours this phase), `api/queue.rs`, `inbox.rs`, `inbox_store.rs`, `catalog.rs` | — |
| 6 | `keys.rs` | owned by the CSPRNG hotfix until merged: this wave starts only after it; reuse its generator, no other edit to its code |
| 7 | `host_tools.rs` `invoke_hosted_tool` (ours) | isolation-truth's redaction code in the same file is finished: not altered (the token goes into `trace.secrets`) |
| 8 | `api/mod.rs` broker routes, `hsp_store.rs`, `api/queue.rs` | — |
| 9 | `receipt.rs`, `vam-cli/src/cmd/receipt.rs` (E12's area), `run_facts.rs`, `caller_context.rs`, `ttl.rs` | **E12 coordination point:** E12 adds `ReceiptRequester.trigger`/`ReceiptTrigger` to `receipt.proto`; this wave populates the emitted key (Q4). Agree the key before wave 0 merges (wave 0's scenario reads it); if E12 Phase 2 is building `receipt.rs` at the same time, the later lane waits (launch §4 conflict rule) |
| 10 | Starlight docs | — |


#### CI rules (owner, 2026-10-29)

The local run is the gate; GitHub Actions is used sparingly. This replaces,
for this lane, "a grant, gateway or auth change waits for green CI".

1. **Every wave PR stays a draft for its whole life.** Every job in
   `rust.yml` is skipped on a draft (`draft != true`), and pushes to the base
   branch run nothing (push CI is `main` only). Never mark a wave PR ready.
2. **Local gate, run by the wave agent and re-run by the orchestrator** in a
   worktree at the PR head: `cargo fmt --all --check`; `cargo clippy` on the
   changed crates `-- -D warnings`; `cargo check --workspace`; `cargo test -p`
   each changed crate; `cargo run -p vam-e2e -- check`; the wave's named
   scenarios and `scripts/e2e/run.sh --smoke`. Build settings per the working
   agreement (`CARGO_TARGET_DIR=/home/user/vam/target`, no incremental, no
   debug info).
3. **Security and start-path waves (5, 6, 7, 8)** also run the full
   `cargo test --workspace` locally, once, on the final head, run by the
   orchestrator (the substitute for full CI). `df -h` first; cleanup ladder if
   needed.
4. **Merge** locally with a merge commit (`git merge --no-ff`) into
   `feat/work-outside-chat` and push; GitHub records the draft PR as merged.
5. **Firecracker lane only when required:** a wave that touches the VM path
   (`vam-runtime`, `vam-firecracker`, `vam-vsock`, rootfs, guest protocol), and
   once at phase close because the start path moved. Run it as one
   `workflow_dispatch` with `lane_only: true` on the base branch (or on a
   KVM host when the owner prefers; ask first). The UDS test broker never
   needs it.
6. **The release PR to `main`** is the one PR that runs full CI (the release
   gate requires it), once, when the owner gives the word.
7. Never: mark ready to "see what CI says", empty commits, reruns without a
   change.

#### Open questions (default in bold)

1. **Thread directory and `threadRef`.** The design gives new threads the
   directory `th_…` and makes `RunSpec.thread_ref` hold the directory after
   resolution, so `RunStatus.threadRef` would change from `{owner}--{name}`
   to `th_…` for new threads, and `chat {thread: continue}` (which echoes
   `threadRef` back as a name) would open a new thread named `th_…`.
   **Default: follow the design for directories, but `RunStatus.threadRef`,
   `CreateRunResponse.threadRef` and `CallerContext.thread_handle` keep the
   `{owner}--{name}` value, and a `th_…` value in `threadRef` is read as an
   id.** Alternative (lower risk, no Phase 1 payoff lost): keep name
   directories for new threads in E5; the id lives in `thread.json` and the
   index.
2. Keys list default. **`GET /v1/keys` without `holder` lists only `http`
   keys (today's list); `?holder=broker` lists grants; `?holder=all` both.**
3. Thread list `threadId` already exists and holds the directory key
   (`api/threads.rs:175`; read by `vam thread ls`). **It becomes the
   `th_…` id; the directory key moves to `dir`; `name` added.** An API value
   change for the VAM app (WO-2.6 adapts); no scenario reads it.
4. Receipt shape. The emitted receipt is flat — `receipt.requestedBy` is a
   user-id string and `receipt.requestSurface` a string
   (`receipt.rs:796-806`) — not the `ReceiptRequester {user, surface}`
   object `receipt.proto` describes, so `requestedBy.trigger` cannot be
   added. **Add a flat `receipt.requestTrigger {brokerId, grantId,
   triggerRef, fireAtMs, fireKind}`, absent for a person; E12's
   `ReceiptTrigger` proto documents it; `requestedBy` unchanged.**
5. `RunGrantRequest` JSON. The schedule broker sends `"mode": "SAME"`,
   `"onBusy": "QUEUE"` and `null`s; proto JSON needs `PLACE_MODE_SAME`.
   **Accept short and full enum names (precedent:
   `expand_permission_enum_json` in `vam-spec/src/encoding.rs`); unknown
   keys refused.**
6. Generator. The design says `rand::rngs::OsRng`; decision 14 says the
   hotfix's generator. Conflict only if the hotfix's generator is not
   OS-backed or only returns a `vam_sk_…` string. **Grant secrets: 32 bytes
   from the hotfix's generator (ask it to expose bytes, OS-backed),
   base64url, token `vam_rg_<grant_id>.<secret>`. `sp_`/`th_`/`rg_` ids:
   OS CSPRNG via a small helper outside `keys.rs`.**
7. Gateway socket path. `vamd` has no single state dir, and the Unix path
   limit is 108 bytes (e2e paths under `target/e2e/<time>-<pid>/…` plus a
   run-tagged broker id exceed it). **`<dir of VAMD_DB_PATH>/hsp/gw/<first
   12 hex of sha256(broker_id)>.sock`, override `VAMD_HSP_GATEWAY_DIR`;
   dir 0700, socket 0600.**
8. No route disables a broker today (`disable_hsp_broker_approval` has no
   caller in `api/`), yet the design closes the socket on disable and the
   schedule broker doc tells operators to disable. **Add `DELETE
   /v1/hosted/brokers/{id}` in wave 8; `peer_uid` is set through
   `PutHostedBrokerRequest.peer_uid`.**
9. Pre-ids workspace for the backfill scenario. **A checked-in Drive9
   fixture (generated once by a `vam-storage` test helper, regeneration
   command in `e2e/README.md`), copied by a `workspace-seed` piece.**
   Alternative: a process-mode-only vamd switch that skips the index.
10. Backfill thread id. Amendment §5 says "a new id", §4 says the id
    travels in `thread.json`. **Adopt `thread.json`'s `id` when present,
    else mint.**
11. `grant.expiring` needs a clock in `vamd` (decision 1: no ticker). **Emit
    from the existing TTL sweep (`ttl.rs`), once per grant (a column marks
    it), to the outbox of the grant's source run — a credential deadline,
    not a schedule.**
12. `vam grants list`. **Yes: `vam grants list|revoke` as an alias of `vam
    keys list --grants` / `vam keys revoke`, in wave 6.**
13. Firing in e2e. **Runner-driven `hsp-echo-broker fire` (deterministic for
    revoke, dedupe and busy); the design's `after_ms`/`times` stay for
    manual use.**

#### Phase close (2026-10-31): released, checklist out

**Released to `main`:** (merge commit), the VAM app
 (merge commit). Base branches fast-forwarded to `main`.

**Exit, as met:** all nine Phase 1 scenarios `active` and green in CI (smoke 68/0/0 on the
non-root runner); full `cargo test --workspace` green in CI
(locally only the known root-only `hsp_uds` test fails); the Firecracker lane green in
CI (the start path moved); the VAM app unit suites (773), `tsc` and Rust tests
(50) green. Docs deferred to a follow-up after isolation-truth Phase 3 (owner).

**Scoreboard (phase):** 9 scenarios turned green (8 designed, 1 added by the owner), 0
red; 1 retired (`existing-workspace-keeps-its-id-and-history`), 1 changed by the owner
(`thread-is-reached-by-id-or-by-name` → `thread-is-reached-by-its-id`). Fix cycles: 1
on the release PR (CI smoke as non-root: sealed skills dir) plus a harness cleanup fix.
Replans: 1 (ids first). Wall: 2026-10-29 to 2026-10-31.

**Benchmark delta (recorded):** "tasks started by a trigger that complete" = 0 by
construction (a test broker is not a product trigger; Phase 2's `vam.schedule` moves
it). "Parked cost of a queued fire" = 0, proven by
`trigger-on-a-busy-workspace-runs-after-the-task` (a queued fire is a PENDING row, no VM).

**Team checklist — Phase 1** (about 10 minutes; CLI, the app and `curl`)

Setup: deploy `vamd` from `main`  **with a fresh state directory** and
restart it; then the app from the VAM app `main` . Build the test broker:
`cargo build -p vam-orchestrator --bin hsp-echo-broker`. Use an agent whose policy
allows `echo_arm_trigger` on `ask` (as in `e2e/world/agent-with-trigger-tool.yaml`).

1. `vam grants list` → **See:** an empty list. `vam keys list` → your keys as before.
2. `vam space create --agent <agent-urn> demo` → **See:** `sp_… (demo)`.
3. `vam run <agent> -p "hello" --workspace <sp_…> --thread "first chat"` → **See:**
   `space sp_… (demo) · thread th_… (first chat)`.
4. `vam run <agent> -p "again" --workspace <sp_…> --thread <th_…>`, then
   `vam thread show --space <sp_…> <th_…>` → **See:** both turns in one thread.
   `vam thread rename --space <sp_…> <th_…> "renamed"`; `vam thread ls --space <sp_…>`
   → **See:** the new label, the same `th_…` id.
5. In the app: open the agent, send a message and a follow-up → **See:** one thread;
   rename the Space and the thread; "New Space" creates a second Space; no `--` names
   or ids are shown anywhere.
6. Start the broker: `hsp-echo-broker --triggers --socket /tmp/tb/broker.sock
   --state-dir /tmp/tb`, and register it with `curl -X PUT <vamd>/v1/hosted/brokers`
   using the manifest in `e2e/world/trigger-test-broker.yaml` → **See:** the answer
   carries `gatewaySocket`.
7. In chat, ask the agent to "arm a trigger that says hello" → approve the card →
   `vam grants list` → **See:** one grant: holder `broker:<id>`, target `demo / <thread>`,
   the message preview, expiry about 90 days ahead. The app's "Scheduled access" shows it.
8. `curl -X POST <vamd>/v1/runs -H "Authorization: Bearer $(cat /tmp/tb/grant-token)"`
   → **See:** 401, and `vam run ls` shows no new run.
9. `hsp-echo-broker fire --state-dir /tmp/tb --gateway-socket <gatewaySocket> --key k1
   --times 2` → **See:** `ACCEPTED` with a run id, then `DUPLICATE` with the same id.
10. `vam receipt show <that run id>` → **See:** "Requested by <you>, surface echo — …
    via grant rg_… (<broker>)".

Report: "Phase 1 pass", or the step number with a screenshot or the error text.

### Phase 2 — The schedule broker

**Goal:** A user asks the agent for a schedule, approves it once, and the run
happens at its time with a receipt that names the schedule.  
**Depends on:** Phase 1 (released to `main`). Needs no new `vamd`
surface (no API, proto or table). One optional `vamd` behaviour fix (WO-2.8,
Q2) waits for the owner's word.  
**Status:** closed in the base 2026-11-01 (seven of seven green); release PR to `main` next, on the owner's word.  
**Plan:** drafted 2026-10-31; **approved** by the owner 2026-10-31 with
Q1 (relative `once {in}`) and Q4 (delay the `once` revoke on QUEUED) at their
defaults, and **yes to wave 6 (WO-2.8, Q2)**: a `vamd` behaviour change, no
new surface. All other open questions at their defaults.  
**Design:** `docs/src/content/docs/hosted/schedule.md`; items and scenarios
the `work-outside-chat` tracker (E5 Phase 2).  
**Exit:** the seven Phase 2 scenarios `active` and green in process mode;
every existing scenario green unchanged (smoke, once as a non-root user before
the release PR); `cargo test -p vam-schedule`, `cargo check --workspace`,
`cargo fmt --all --check`, `cargo clippy -p vam-schedule -- -D warnings`;
the Benchmark delta rows recorded; one manual run on a KVM host with the
broker registered inside Lima (the operator script). No Firecracker lane run
is needed for the phase (no microVM path moves); the release PR into `main`
runs full Actions once, as always.

<!-- Breakdown below is filled by the phase-breakdown agent. -->

Breakdown by the phase-breakdown agent, 2026-10-31, against base
`feat/work-outside-chat` (= `main` + tracker commit).
Code read on this base: `hsp_echo.rs` (test broker, its gateway client
`fire_grant`), `gateway.rs` (checklist, `revoke`, `recheck_queued_fire`),
`grants.rs::check_call`, `host_tools.rs::invoke_hosted_tool` (2861–2950),
`hsp_registry.rs` (`preflight_invoke`, `assert_invoke_egress`),
`api/mod.rs::put_hosted_broker`, `vam-spec/src/encoding.rs`, the proto,
`vam-cli/src/cmd/receipt.rs::trigger_line`, `ttl.rs`, the e2e runner
(`triggers.rs`, `world.rs`, `engine.rs`, `mock.rs`), `scripts/e2e/run.sh`,
`brokers/vam-web`, and the prototype on `origin/feat/scheduler-broker`
(`next_fire.ts`, `tools.ts`, tests; read only).

**Phase 1 contract check (what the broker relies on, all present):** short
enum names in `RunGrantRequest` JSON (`SAME`/`NEW`/`ID`, `QUEUE`/`SKIP`;
`encoding.rs:214-219`, unknown keys refused); `FireKind` `RUN_NOW` (stored as
`run_now`, `gateway.rs:758`); `not_after` carried to the queued run;
`RevokeRunGrant` (possession, idempotent, discards a pending queued fire,
`gateway.rs:831`); `trigger_ref` pinned on the first `ACCEPTED`/`QUEUED`
fire and checked after (`gateway.rs:551, 716`); `grant.expiring` from the TTL
sweep (`ttl.rs:153`); loop guard read from `runs.source` for every tool in
`loop_guarded_tools`; issuance only with `run` present, typed refusals
(`grants.rs:186`); `max_active_grants_per_user` (default 50). Four places
where the code and the design disagree are open questions Q2–Q5 below; none
needs new `vamd` surface.

#### Scenario delta

All new, `needs: process`, tag `schedule` (plus `triggers`), status `pending`
in wave 0. The broker runs as a real process beside the process-mode node, as
the test broker does; the model is the mock. Times are seconds ahead through a
relative `once` (Q1), never cron, so no clock seam is needed in e2e; cron, DST
and catch-up arithmetic are unit tests with an injected `Clock`.

| Scenario | Change | Layer | Given | Why |
|---|---|---|---|---|
| `scheduled-message-runs-at-its-time` | new | process | org-admin, agent-with-schedule-tools, schedule-broker | the phase's exit: the agent creates a once-schedule a few seconds ahead, the user approves; no run exists before the time (parked cost 0); after it a run starts by itself in a new thread labelled as asked, succeeds, and `RunStatus.source` and the receipt say surface `schedule`, trigger ref `sch_…`, grant `rg_…`, broker `vam.schedule` |
| `schedule-is-saved-only-after-the-user-approves` | new | process | same | the user denies the create card: no grant is listed, a follow-up `schedule_list` shows none, and nothing fires |
| `deleted-schedule-never-fires-again` | new | process | same | `schedule_delete` (asked, approved) revokes the grant (`revokedAt` set in the user's grant list) and no run starts at the time |
| `schedule-for-a-new-workspace-runs-there` | new | process | same | `workspace NEW {name}`: the scheduled run happens in a Space with that label (its file is there), not in the chat's Space |
| `schedule-set-to-skip-does-not-wait-for-a-busy-workspace` | new | process | same (the agent piece has `run`, so `agent-with-shell` is not needed) | `onBusy SKIP`: a fire during a slow task starts nothing, and `schedule_get` shows the skipped fire |
| `missed-fire-runs-once-after-the-broker-restarts` | new | process | same | the broker is stopped over the fire time (nothing starts while it is down) and restarted inside the grace window; exactly one run starts |
| `another-users-schedules-are-invisible` | new | process | org-admin, org-member, agent-with-schedule-tools, schedule-broker | owner-first reads (P1): the member's `schedule_list` shows none and `schedule_get` of the admin's schedule id answers `NOT_FOUND` |

**Existing scenarios whose `then` changes: none.** The broker adds no `vamd`
code; WO-2.8 (if the owner takes it) narrows a refusal no scenario asserts.
**No spec change is requested by the scenarios.** Re-run unchanged in wave 0
(the `broker` piece and its cleanup change): the six trigger scenarios
(`trigger-starts-a-run-as-the-grant-owner`, `repeated-fire-…`,
`revoked-grant-…`, `grant-token-is-refused-over-http`,
`triggered-run-cannot-arm-another-trigger`, `trigger-on-a-busy-workspace-…`)
and `member-chats-with-a-released-agent`; in wave 6, `secret-never-reaches-guest-encoded`
and the `host-tool-*` scenarios (`hsp_registry.rs`).

**World pieces** (wave 0):
- `schedule-broker` (kind `broker`): starts `$VAM_E2E_SCHEDULE_BROKER`
  (`vam-schedule-broker`) with `--socket {{socket}} --state-dir {{dir}}` on a
  short path, registers it with `PUT /v1/hosted/brokers` under the fixed id
  `vam.schedule` with the design's manifest (six tools, `identityRequirement
  {userId, agent}`, `runRequestPermission {grantTools: [schedule_create,
  schedule_update], loopGuardedTools: [+ schedule_run_now], surface:
  schedule, maxRequestsPerMinute: 60, maxRequestsPerDay: 5000,
  maxActiveGrantsPerUser: 50}`, no secrets, no egress), then reads
  `broker.gatewaySocket` from the answer and **restarts the process with
  `--gateway-socket <path>`** (vamd dials the broker at `PUT` and opens the
  gateway only after, so the path cannot be passed on the first start; Q3).
  Cleanup: stop the process, remove its dir, and `DELETE
  /v1/hosted/brokers/vam.schedule` so its tools do not reach later scenarios
  (the same cleanup is added to `trigger-test-broker`, which Phase 1 planned
  once the route existed).
- `agent-with-schedule-tools` (agent, `requires: [schedule-broker]`):
  `default: deny`; `builtin:read_file`, `write_file`, `run` allow;
  `builtin:schedule_list`, `schedule_get` allow; `builtin:schedule_create`,
  `schedule_update`, `schedule_delete`, `schedule_run_now` ask (the rule form
  `agent-with-trigger-tool` uses). `agent-with-builtins` cannot be used: its
  `default: deny` with builtin rules only never reaches a hosted tool.
- `org-member` (existing) for the last scenario.

**Runner** (own module `crates/vam-e2e/src/schedule.rs`, a child of `engine`
like `triggers.rs`; hooks in `scenario.rs`/`engine.rs`/`check.rs`/`world.rs`
only register steps and the two piece fields; take `main` before each wave,
the proof lane may add bench steps):
- `WorldPiece` gains two optional fields for kind `broker`: `bin` (the env var
  naming the binary; default `VAM_E2E_TEST_BROKER`) and `args` (placeholders
  `{{socket}}`, `{{dir}}`; default today's `--triggers --socket {{socket}}`),
  plus `restartWithGateway: true` for the two-step start above. The test
  broker's piece is unchanged in behaviour.
- `wait-scheduled-run: { within: 30s }` — polls `GET /v1/runs` as the
  scenario's owner for a run of the scenario's agent with `source.kind:
  trigger` and `source.brokerId: vam.schedule` that the scenario has not seen;
  it becomes the latest run (as `fire-trigger` does), so `expect: {run}`,
  `{file}`, `{thread}`, `{receipt}`, `{runs}` and `{spaces}` follow it. None in
  time is `product`.
- `expect: { scheduled-runs: { count, within } }` — watches the whole window
  and passes only if exactly `count` such runs started in this scenario at its
  end and never more (proves "nothing fires" and "exactly one").
- `stop-broker: {}` / `start-broker: {}` — stops the schedule broker (SIGTERM,
  SIGKILL after 5 s) and starts it again with the same dir, socket and gateway
  socket; no re-`PUT` (vamd dials per Invoke).
- `{{schedule-id}}` in a `chat` text — the newest `sch_…` word in a tool
  result of the latest run's thread (read through the thread API). Used to
  hand the admin's id to the member.
- **Mock placeholder** `{{last:<prefix>}}` in a reply's `toolCall.args` string
  values (`crates/vam-inference/src/mock_script.rs`, where args are
  serialised, line ~644): replaced by the newest word starting with `<prefix>`
  in the request's messages (user text and tool results). The scripted model
  needs it to pass the schedule id it was given to `schedule_get` and
  `schedule_delete` (mock scripts load at vamd start and cannot hold a run
  id). Unit tests in `mock_script.rs`; no product path reads it.
- `scripts/e2e/run.sh`: add `-p vam-schedule` to the process-guest
  `cargo build`, copy `vam-schedule-broker` to `e2e-bin`, export
  `VAM_E2E_SCHEDULE_BROKER`. The `--firecracker` branch is unchanged.
- `e2e/README.md`: a "Schedules" section for the piece fields, the steps and
  the placeholders (the e2e how-to, not the Starlight docs).

**How the scenarios drive the agent** (as Phase 1 scripted
`echo_arm_trigger`): `when: { step: create, stepStart: true }` replies with a
`schedule_create` tool call whose args are the design's JSON with `when:
{once: {in: PT5S}}` (seconds vary per scenario), `run.message:
"e2e-step:fired Write fired.txt mock-script:<scenario id>"`, `workspace`,
`thread {mode: NEW, name}`, `onBusy`; `afterTool: schedule_create` replies
with text. The scheduled run's message carries its own `e2e-step` label and
`mock-script:` marker, so the mock answers it without a runner hint. Delete and
get use `scheduleId: "{{last:sch_}}"`.

**Red for the right reason on today's base** (class `product`): no broker
exists, and an arrange failure at `given` is not the right reason, so **wave 0
adds a crate stub** `brokers/vam-schedule` (lib + bin `vam-schedule-broker`):
flags parsed, `Register` accepted, `ListTools` lists the six tools with the
manifest's side effects, every `Invoke` answers the typed tool error
`UNIMPLEMENTED: the schedule broker is not built yet`. The givens then arrange
on today's vamd; vamd asks, the runner approves, vamd mints the grant, the stub
refuses and vamd revokes it. Each scenario fails at the first step that needs
the broker: six at `expect: { thread: { role: tool, contains: "sch_" } }` right
after the approved create; `schedule-is-saved-only-after-the-user-approves` at
the follow-up `schedule_list` result. The stub's `Cargo.toml` already declares
every dependency the phase needs, so `Cargo.lock` changes once (wave 0).

#### Benchmark delta

| Line (`benchmark.md` §1, P7) | Before (base) | After Phase 2 | How measured |
|---|---|---|---|
| tasks started by a trigger … that complete | **0 by construction** (only a test broker exists) | **measured on the mock**: scheduled runs that complete ÷ scheduled runs started, from `scheduled-message-runs-at-its-time`, `schedule-for-a-new-workspace-runs-there` and `missed-fire-runs-once-after-the-broker-restarts` (expected 3 of 3) | the phase-close scoreboard; when the benchmark lane adds "the scheduled task" (`scheduled-task`, a new task-set version with a `model:` block on the first scenario) it is the same scenario under `--bench --mock`, k = 5 |
| parked cost (must be 0) | 0, proven for a queued fire (Phase 1) | **stays 0**: nothing exists in `vamd` between fires (no run, no session: `expect: {runs: {count: 1}}` before the fire time in `scheduled-message-runs-at-its-time`); a queued fire is still Phase 1's PENDING row with no session | `scheduled-message-runs-at-its-time`; `trigger-on-a-busy-workspace-runs-after-the-task` re-run |

No paid baseline: the base has no product trigger, so a paid run measures
nothing. The paid run of the scheduled task waits for the owner's word
(playbook §10); the "decided from a notify surface" half of the line moves
with E12 Phase 2.

#### Items

Item ids follow the design (the `work-outside-chat` tracker (E5 Phase 2)); WO-2.3 is
split so each wave fits one session, and WO-2.8 is proposed (owner's word).

- [x] **WO-2.1 Crate and process.** Wave 0: the stub above (workspace member,
  full dependency list; check workspace deps first: `rusqlite` and `sha2`
  are workspace deps, `tonic 0.10`, `tokio`, `tokio-stream`, `tower 0.4`,
  `http 0.2`, `prost-types 0.12`, `clap 4`, `serde_json`, `thiserror`,
  `tracing`, `rand 0.8`, `libc` and `chrono 0.4` are already in `Cargo.lock`;
  new: `croner` and `chrono-tz`, pinned exact, licence MIT/Apache, release at
  least two weeks old; dev `tempfile`). Wave 1: `vam-schedule-broker --socket
  --state-dir [--gateway-socket] [--min-interval 15m]`; UDS serve (the
  `vam-web/src/serve.rs` pattern, copied: a broker never depends on another
  broker); state dir 0700, `vam-schedule.db` 0600, exclusive `flock` on
  `vam-schedule.lock` (a second process exits with a clear error); SQLite
  migrations for `schedules`, `fires`, `pending_revokes` (+ `not_before`,
  Q4); the manifest as a constant, `vam-schedule-broker manifest` prints it
  (the operator script uses it) and a unit test checks it equals the
  `schedule-broker` piece's manifest; `ListTools` with the final schemas left
  to wave 4; a gateway client module `src/gateway.rs` on `vam-spec`'s
  generated `HostedTriggerGatewayClient` over UDS (connect per call, timeout,
  typed outcome mapping; the shape of `hsp_echo::fire_grant`, not shared with
  it; Q6); no `--gateway-socket` → fires wait with backoff and one log line;
  an Invoke without `CallerContext.user_id`/`agent_urn` → `INVALID_ARGUMENT`
  (vamd checks first; defence in depth); log rules (never the token, the
  message or the args at info). Depends only on `vam-spec` and shared
  libraries (decision 22). Files: `brokers/vam-schedule/**`, root
  `Cargo.toml` (member, wave 0), `Cargo.lock` (wave 0). Accept: lock, file
  modes, migrations idempotent, client against an in-process fake gateway
  (as `hsp_echo`'s `trigger_broker_tests`), `ListTools` side effects equal the
  manifest. Tests: `cargo test -p vam-schedule`, `cargo check --workspace`.
  Turns green: none. `Decide: NOW — proof 3 (demo), cost 1 (new crate on an
  existing pattern, no vamd surface).`
- [x] **WO-2.2 Time.** `when` parsing and next fire: `once {at, timezone}`
  (wall time in an IANA zone, or an instant with an offset), `once {in}`
  (ISO-8601 duration counted from when the schedule is saved, after the
  user's approval; Q1), `cron {expression, timezone}` (five fields, `croner`,
  zone via `chrono-tz`); a past or zero time is `INVALID_ARGUMENT`; cron floor
  (next two fires at least `--min-interval` apart); grace window (once 24 h;
  cron half the interval, at most 1 h); a `Clock` trait (system and test).
  Ported tests from the prototype's `create.test.ts` next-fire cases (future
  next fire, past once refused, bad zone refused) plus DST: 02:30 daily in
  `Europe/Berlin` and `America/New_York` across both transitions (spring:
  once, first valid instant after the gap; fall: once, the first 02:30); if
  `croner` differs from the prototype, the tests decide and the design
  section is amended. Files: `src/when.rs`, `src/clock.rs`. Tests: `cargo
  test -p vam-schedule when clock`. Turns green: none. `Decide: NOW — proof 2
  (wrong time = failed task), cost 1 (port with tests).`
- [x] **WO-2.4 Store and fire loop.** Store access (`schedules`, `fires`,
  `pending_revokes`); one loop: sleep until the earliest `next_fire_at`
  (30 s cap, woken by any create, update or delete), select due rows (enabled,
  active, `backoff_until` passed), at most 4 fires at once, advance each row
  in the transaction that records its outcome; idempotency key
  `hex(sha256("vam.schedule\n" + schedule_id + "\n" + fire_at RFC 3339 UTC +
  "\n" + kind))` reused on every retry (run-now also hashes the calling run
  id); request fields per the design (`trigger_ref` = schedule id, `kind`,
  `not_after` = next nominal time for cron, `fire_at` + 24 h for once and run
  now); the outcome table (ACCEPTED/QUEUED/DUPLICATE advance; BUSY
  SKIPPED/ALREADY_QUEUED → `skipped_busy`; BUSY with `retry_after_ms`,
  RATE_LIMITED, UNAVAILABLE, transport error → full-jitter backoff 2 s → 5
  min, never sooner than `retry_after_ms`, persisted, give up at `not_after`
  as `failed_unavailable`; DENIED REVOKED/EXPIRED → `revoked`/`expired`;
  other DENIED → `needs_attention` with the reason); catch-up `latest`
  (default) and `skip` with one `missed` row and `missed_count`, never more
  than one catch-up fire; `once` completion → `completed`, its token queued in
  `pending_revokes` (`not_before` = `not_after` when the fire was QUEUED, else
  now; Q4). Files: `src/store.rs`, `src/fire.rs`. Accept: unit tests with the
  test `Clock` and a fake gateway for every outcome row, retry reuses the key,
  both catch-up modes, a fire recorded before a crash dedupes after restart,
  backoff survives restart. Tests: `cargo test -p vam-schedule`. Turns green:
  none alone (needs WO-2.3a). `Decide: NOW — proof 3 (P7 line), cost 1.`
- [x] **WO-2.3a Tools: create, list, get, update.** Final schemas for the six
  tools (unknown and identity-shaped keys refused: `user`, `owner`,
  `user_id`, `agent_urn` at top level; typed `INVALID_ARGUMENT`, `NOT_FOUND`,
  `FAILED_PRECONDITION`) and descriptions that tell the model how to fill
  `run` from its run context and the user's words (target agent, Space and
  thread `SAME`/`NEW`/`ID`, message, inputs, `onBusy`, expiry; name a `NEW`
  thread after the title, since vamd labels an unnamed one "untitled"); `run`
  is passed through unchanged (vamd reads it; the broker never trusts its own
  reading). `schedule_create`: no `run_grant` on the Invoke →
  `FAILED_PRECONDITION`; `when` validated (a tool error makes vamd revoke the
  grant it just minted); a `once` whose grant expires before its fire time →
  `INVALID_ARGUMENT` (Q5); schedule id `sch_` + the grant id's suffix; row
  keyed by grant, token stored as given, never returned or logged; result
  labels from `RunGrant.target`; `clientRequestId` dedupe per owner and agent
  (a retried create answers the existing schedule and queues the new grant's
  token for revoke; Q12). `schedule_list`/`schedule_get`: owner-first
  (`owner_user_id = caller.user_id` in every query, then creator agent or
  `visibility = user`), foreign id → `NOT_FOUND`, list without message text,
  get with the message preview and the last 10 fires. `schedule_update`:
  title, `when`, `enabled`, `catchUp`; owner and creating agent only; with
  `run` a new grant is stored on the same row and the old token queued in
  `pending_revokes` (a queued fire of the old grant is discarded by vamd: the
  user changed the schedule). Ported read/write ownership tests from
  `read.test.ts`/`write.test.ts`. "Always allow" on a draft needs nothing new
  (the grant still binds the approved request). Files: `src/tools.rs`,
  `src/broker.rs`. Tests: `cargo test -p vam-schedule`, `scripts/e2e/run.sh
  --scenarios <the six below>`. Turns green:
  `scheduled-message-runs-at-its-time`,
  `schedule-is-saved-only-after-the-user-approves`,
  `schedule-for-a-new-workspace-runs-there`,
  `schedule-set-to-skip-does-not-wait-for-a-busy-workspace`,
  `missed-fire-runs-once-after-the-broker-restarts`,
  `another-users-schedules-are-invisible`. `Decide: NOW — proof 3 (demo; P1
  ownership), cost 1.`
- [x] **WO-2.3b Delete, revocation and run now.** `schedule_delete`:
  `RevokeRunGrant` first; answered → row deleted; not answered → state
  `deleting` (never fires) and the token in `pending_revokes`. A drain of
  `pending_revokes` at start and every minute, honouring `not_before`.
  `schedule_run_now`: kind `RUN_NOW`, `fire_at` = Invoke time truncated to
  the second, key also hashes the calling run id (a retried call dedupes, two
  presses do not), `not_after` = `fire_at` + 24 h, the outcome returned to the
  agent. States `revoked`, `expired`, `needs_attention`, `deleting` shown on
  list and get. Files: `src/tools.rs`, `src/fire.rs`, `src/store.rs`. Accept:
  unit tests (gateway down on delete, drain, run-now key, idempotent revoke).
  Turns green: `deleted-schedule-never-fires-again`. `Decide: NOW — proof 3
  (P1: delete really stops future runs; demo), cost 1.`
- [ ] **WO-2.5 Operator path.** World piece `schedule-broker` (wave 0) and
  `scripts/run-schedule-broker.sh` + `scripts/schedule-broker/manifest.json`
  (wave 5, the `run-web-broker.sh` pattern): build, start as the vamd user,
  `PUT /v1/hosted/brokers`, read `gatewaySocket`, restart with
  `--gateway-socket`, `--stop`, `--dry-run`; works inside Lima; prints the
  agent rules to add. Docs part (Starlight `hosted/schedule.md`, sidebar)
  **deferred** with Phase 1's wave 10 until isolation-truth Phase 3 is on
  `main` (it has not been drafted: `isolation-truth.md` Phase 3 "Plan: not
  drafted"). `Decide: NOW — proof 2 (install proof: an outsider can turn it
  on), cost 0 (script, piece).`
- [x] **WO-2.6 App: see and stop — nothing left for Phase 2.** Phase 1 wave
  4A (released in) already sends ids, shows labels and lists
  grants with Revoke ("Scheduled access"). Remaining: the run label "Started
  by schedule". `vamd` knows `source.kind`, `brokerId` and `triggerRef`
  (`sch_…`), not the title, so a label without a title is a few lines in the
  run view; with a title it needs the broker. `Decide: BACKLOG — proof 1 (the
  receipt and RunStatus already say it), cost 1 (a VAM app wave) → carried by
  the next app wave that touches the run view.` (Verify against the VAM app
  `main` when that wave is planned.)
- [ ] **WO-2.7 Retire the prototype.** Docs only (note that
  `origin/feat/scheduler-broker` is superseded, no merge or cherry-pick). **Deferred** with the docs.
  `Decide: BACKLOG — proof 0, cost 0; carried by the docs follow-up.`
- [x] **WO-2.8 (approved by the owner 2026-10-31) The grant request is not egress.**
  `assert_invoke_egress` (`hsp_registry.rs:375`) scans every string in a
  hosted call's arguments for `http(s)://` and, with `egressDomains: []`,
  refuses the call ("hsp denied: egress not allowlisted"). So "every morning
  check https://status.example.com" fails at preflight, before any grant is
  minted (safe, but the agent cannot repair it except by dropping the link).
  Change: skip the `run` object of a call to a tool in the approved
  `grant_tools` (vamd decodes and validates that object itself; the broker
  never fetches it; the future run applies its own egress then). Other keys
  (`title`) are still scanned. **A `vamd` behaviour change, no new surface:
  spec change for the owner** (Q2). Files: `hsp_registry.rs`
  (`preflight_invoke`/`assert_invoke_egress`), unit tests beside the two at
  `:962, :982`. Tests: `cargo test -p vam-orchestrator hsp_registry`,
  `secret-never-reaches-guest-encoded`, `host-tool-*`. `Decide: NOW — proof 2
  (confirmed defect in a common ask), cost 1 (one function, tests).` If not
  taken: the tool descriptions say links in a schedule are refused today, and
  the item goes to backlog.

#### Waves

**Actions rule (owner, 2026-10-31, replaces the `actions:` column below):** no GitHub Actions on wave PRs unless the wave touches the Firecracker path. Every wave PR is a draft, proven in the session with `scripts/ci/local.sh` (`--full` plus the smoke set), then merged with a merge commit; no `skip-vm-lane`, no marking ready. Full Actions only on the release PR into `main`.

| Wave | Items | Turns green | Repo | Model | Budget | Depends on | actions | Status | PR |
|---|---|---|---|---|---|---|---|---|---|
| 0 | scenarios, world pieces, runner steps (`schedule.rs`), mock placeholder, `run.sh`, crate stub with all deps (WO-2.1 stub, WO-2.5 piece) | — (7 red at the right step, class `product`) | vam | Sonnet 5.5 low | 120 min | Q1 answered | fast (`skip-vm-lane`: `Cargo.lock`, `vam-e2e`, `scripts/e2e/`, `e2e/`) | [x] df 30G→22G | — |
| 1 | WO-2.1 | — (scaffold) | vam | Sonnet 5.5 medium — a new process: UDS serve, single-instance lock, SQLite schema and a gateway client on the generated proto | 90 min | 0 | none | [x] df 22G→22G | — |
| 2 | WO-2.2 | — | vam | Sonnet 5.5 low | 60 min | 1 | none | [x] df 22G→22G | — |
| 3 | WO-2.4 | — (needs wave 4) | vam | Sonnet 5.5 medium — concurrency and retry state: one loop, backoff, catch-up, crash recovery | 120 min | 1, 2 | none | [x] df 22G→22G | — |
| 4 | WO-2.3a | the six named in WO-2.3a | vam | Sonnet 5.5 medium — the grant request and the ownership rules (P1), six schemas the model must fill | 120 min | 3 | fast (flips scenarios to `active`; `[skip ci]` if the orchestrator's `--full` covers it) | [x] df 22G→22G | — |
| 5 | WO-2.3b, WO-2.5 script | deleted-schedule-never-fires-again | vam | Sonnet 5.5 low | 75 min | 4 | fast (same) | [x] df 22G→22G | — |
| 6 | WO-2.8 (owner: yes, 2026-10-31) | — | vam | Sonnet 5.5 low | 45 min | 0 | fast (`skip-vm-lane`: `vam-orchestrator`) | [x] df 22G→18G | — |
| 7 | WO-2.5 docs, WO-2.7 | — | vam | Sonnet 5.5 low | 45 min | 5; isolation-truth Phase 3 on `main` | none | deferred | |

One wave at a time. Waves 1–3 touch only `brokers/vam-schedule` (`[skip ci]`,
merge on the local proof). Wave 0 merges first and is the only wave that
changes `Cargo.lock`. Phase close: smoke once as a non-root user; no
Firecracker lane.

**Shared surfaces per wave** (take `main` before each wave):

| Wave | Shared surfaces | Rule |
|---|---|---|
| 0 | e2e runner (`world.rs` two piece fields, `scenario.rs`/`engine.rs`/`check.rs` hooks; steps in new `schedule.rs`; cleanup in our `triggers.rs`), `scripts/e2e/run.sh`, `vam-inference/src/mock_script.rs` (one placeholder), root `Cargo.toml` members, `Cargo.lock` | proof lane (H1/H2) may add bench steps and edit `vam-inference`: keep ours in `schedule.rs` and one function in `mock_script.rs`, hooks minimal |
| 1–5 | `brokers/vam-schedule` (new, ours) | no `vamd` file is touched: `host_tools.rs` and `api/` stay as Phase 1 left them |
| 4, 5 | `e2e/scenarios/*` status flips (ours) | never edit a `then` |
| 5 | `scripts/run-schedule-broker.sh`, `scripts/schedule-broker/` (new) | — |
| 6 | `hsp_registry.rs` (`preflight_invoke` only) | not the truth lane's redaction or egress code in `egress.rs`/`host_tools.rs`; spec change approved first |
| 7 | Starlight docs | the truth lane owns `docs/` until its Phase 3 is on `main` |

#### Open questions (default in bold)

1. **Relative `once`** (needed by wave 0's scenarios). The design's `once`
   takes only an absolute `at`; scripted mock args load at vamd start and
   cannot hold "now + 5 s". **Add `when: {once: {in: "PT5S"}}` (ISO-8601
   duration, counted from when the schedule is saved, after approval) to the
   tool schema and amend `docs/src/content/docs/hosted/schedule.md` "The tools"**: a design
   amendment to the broker's schema only, also good for "in two hours" asks
   (models are poor at absolute times). Alternative: a mock placeholder
   `{{now+15s}}` rendered when the model replies (no product change; flaky if
   the approval takes longer).
2. **Links in a schedule are refused by preflight** (the handoff's question,
   confirmed in code: `hsp_registry.rs:271, 375-402` scans every argument
   string, including `run.message` and `run.inputs`; the manifest has no
   egress, so any URL refuses the whole call before a grant is minted).
   **Take WO-2.8 as wave 6 (spec change: a `vamd` behaviour fix, no new
   surface).** If the owner says no: Phase 2 ships with the limit stated in
   the tool descriptions and the deploy note; WO-2.8 goes to backlog.
3. **The gateway path is known only after `PUT`.** `put_hosted_broker` dials
   the broker to register it and opens the gateway socket after, so the
   broker cannot get `--gateway-socket` on its first start. **The broker
   connects to the gateway per call; the operator script and the e2e piece
   start it, register, read `gatewaySocket`, and restart it with the flag.**
   The path is stable for one broker id and vamd state dir, so a service unit
   sets it once. Not planned: `RegisterResponse` carrying the path (new vamd
   surface).
4. **Revoking a `once` grant at completion discards its queued fire.**
   `RevokeRunGrant` calls `discard_queued_fires` (`gateway.rs:868`), so the
   design's "after an accepted, queued or duplicate fire … its grant revoked"
   would drop a QUEUED run. **On QUEUED the token is queued for revoke with
   `not_before = not_after`; on ACCEPTED/DUPLICATE it is revoked at once.**
   Broker-only; amend the design's "`once`" line.
5. **"A one-off schedule's grant at most fire time + 24 h" (decision 13)
   cannot be enforced by `vamd`**, which never sees `when` (`grants.rs`
   expiry is default 90 d, cap 365 d). **The broker enforces it: a grant that
   expires before the fire time is refused at create; the grant is revoked at
   completion (Q4) or when the grace window passes.** No locked-decision
   change: the bound holds; who enforces it is recorded here.
6. **Gateway client location.** `hsp_echo::fire_grant` lives in
   `vam-orchestrator`, which a broker may not depend on. **An own module in
   `brokers/vam-schedule` on `vam-spec`'s generated client (about 40 lines);
   no extraction now** (it would touch a VM-path crate for no gain).
   Extraction into a small shared crate is backlog, when `vam.notify` (E12
   Phase 2) needs the same client.
7. **Cron floor in e2e.** **E2e uses `once` only; the floor stays 15 minutes
   and is not lowered for tests; cron and DST are unit tests.**
8. **WO-2.6 remainder.** **Nothing in Phase 2; "Started by schedule" is
   backlog** (see the item).
9. **CLI surface.** The design names none (grants are listed and revoked with
   `vam grants`, the receipt with `vam receipt show`). **No `vam schedule`
   command.** The CLI receipt line reads "alice, surface schedule — sch_… via
   grant rg_… (vam.schedule), fire time …" (`trigger_line`), not the design's
   "schedule sch_… via grant": **unchanged** (E12's file; the facts are all
   there; a one-word tweak is backlog).
10. **`schedule_delete` asks.** **Keep ask** (design Q3); users who mind pick
    "always allow".
11. **Mock placeholder in `vam-inference`.** **Yes, in wave 0** (`{{last:<prefix>}}`,
    mock-only); the alternative, the runner calling the broker's `Invoke`
    directly, bypasses `vamd` and proves nothing a user sees.
12. **A retried create with the same `clientRequestId`** mints a second grant
    (vamd mints on every approved call). **The broker answers with the
    existing schedule and queues the new token for revoke.**

#### Deploy note (sketch, for the release PR)

1. Build: `cargo build --release -p vam-schedule` → `vam-schedule-broker`.
2. Run it **as the vamd user** (its socket is 0600 and vamd dials it; the
   gateway socket vamd opens is 0600 too): `vam-schedule-broker --socket
   /run/vam/schedule.sock --state-dir /var/lib/vam-schedule`. The state dir
   holds the grant tokens (0700/0600): back it up like a key file; one
   process per state dir (lock).
3. Approve it: `PUT /v1/hosted/brokers` (a `brokers:admin` key) with
   the manifest `vam-schedule-broker manifest` prints (broker id `vam.schedule`,
   first-party, six tools, `runRequestPermission.grantTools: [schedule_create,
   schedule_update]`, surface `schedule`) and `unixPath`. **No secrets**
   (`secretBindings: []`, no `required_secrets`), **no egress**.
4. Read `broker.gatewaySocket` from the answer; restart the broker with
   `--gateway-socket <path>` (stable across restarts). Optional `peerUid` for
   `SO_PEERCRED`. `scripts/run-schedule-broker.sh` does 1–4.
5. Agents: add rules for `builtin:schedule_*` (create, update, delete, run now
   ask; list, get allow), or rely on the side-effect defaults.
6. Stop: `DELETE /v1/hosted/brokers/vam.schedule` closes the gateway; every
   fire is then refused. Users revoke one schedule's access with `vam grants
   revoke` or "Scheduled access" in the app.
7. Links in a schedule's request are allowed (WO-2.8); the scheduled run applies
   its own egress when it fires.

#### Phase close (2026-11-01): closed in the base, release PR next

**Exit, as met** (base): all seven Phase 2 scenarios `active` and green in
process mode; every existing scenario green unchanged — smoke **75 pass, 0 fail** as
root (orchestrator `local.sh --fast` at the wave 6 head) and **75 pass, 0 fail as a
non-root user** (temporary `ci` user, `VAM_E2E_SKIP_BUILD=1`, removed after); full
`cargo test --workspace` green except the known root-only `hsp_uds` test (CI's
non-root job runs it); `cargo fmt`, `cargo check --workspace`; `cargo clippy -p
vam-schedule --all-targets --no-deps -- -D warnings` clean (`--no-deps`: `vam-spec`'s
generated code trips `large_enum_variant` on the base, also for `vam-web`; noted in
2 W1). `cargo tree -p vam-schedule` has no `vam-orchestrator`. **Not yet:** the manual
run on a KVM host with the broker inside Lima (owner's call, §10) — default: the
team checklist below on the team instance replaces it.

**Scoreboard (phase):** 7 scenarios turned green, 0 red; 0 retired; no `then` changed.
Fix cycles: 1 (wave 4: schedule id redacted by vamd → design amended). Replans: 0.
Waves 0–6; wave 7 (docs) deferred until
isolation-truth Phase 3 is on `main`. Wall: 2026-10-31 to 2026-11-01.

**Benchmark delta (recorded):** "tasks started by a trigger that complete" = **3 of 3
on the mock** (`scheduled-message-runs-at-its-time`,
`schedule-for-a-new-workspace-runs-there`, `missed-fire-runs-once-after-the-broker-restarts`
each assert the scheduled run succeeded); was 0 by construction. "Parked cost of a
queued fire" = **0** (no run or session exists between fires: `expect runs {count: 1}`
before the fire in `scheduled-message-runs-at-its-time`; a queued fire is Phase 1's
PENDING row). The paid run of the scheduled task waits for the owner's word.

**Known issues (backlog):** two run-nows from different runs in the same second share
one `fires` row and dedupe to the first run (2 W5).

**Team checklist — Phase 2** (about 10 minutes; CLI, chat in the app, `curl`)

Setup: deploy `vamd` from `main` after the release and restart it (the Phase 1 state
directory is fine). Give your agent the schedule rules from
`e2e/world/agent-with-schedule-tools.yaml` (`builtin:schedule_list`/`_get` allow; create,
update, delete, run now ask). Have a `brokers:admin` key.

1. On the vamd host, as the vamd user: `VAMD_URL=<vamd> VAMD_ADMIN_KEY=<key>
   scripts/run-schedule-broker.sh` → **See:** the broker registered as `vam.schedule`,
   restarted with its gateway socket, six `schedule_*` tools listed.
2. `curl -H "Authorization: Bearer <key>" <vamd>/v1/hosted/brokers/vam.schedule` →
   **See:** enabled, `gatewaySocket` set, no secrets, no egress.
3. In chat: "Every weekday at 18:00 <your time zone>, summarise what I did today, in a
   new thread called daily-summary" → **See:** an approval card for `schedule_create`;
   approve → the agent answers with `sch_…`, the next fire in your local time, and the
   target "… (sp_…) / daily-summary (th_…)".
4. `vam grants list` → **See:** one grant, holder `broker:vam.schedule`, target your
   Space / daily-summary, the message preview, expiry about 90 days ahead.
5. In chat: "list my schedules" → **See:** the schedule, its next fire and expiry, no
   message text.
6. In chat: "run the daily summary now" → approve the card → **See:** a run id;
   `vam run ls` shows that run in the daily-summary thread, and it finishes.
7. `vam receipt show <that run id>` → **See:** "requested by <you>, surface schedule —
   schedule sch_… via grant rg_… (vam.schedule)", fire kind run now.
8. In chat: "in two minutes, write a file called hello.txt" → approve → wait two
   minutes → **See:** a run starts by itself and `hello.txt` exists in that Space.
9. In chat: "delete the daily summary schedule" → approve → **See:** "deleted".
10. `vam grants list` → **See:** the daily-summary grant shows REVOKED; nothing fires
    at 18:00.

Report: "Phase 2 pass", or the step number with a screenshot or the error text.

## Explicitly later

- **On-demand broker runs (owner, 2026-10-29, option B).** A user grants a
  broker a scoped standing permission (agent, Space, limits, expiry), given in
  the app or CLI instead of a tool call; the broker's request carries the
  message per fire; identity still only from the stored credential;
  third-party brokers stay barred. Touches locked decisions 5 and 9. Design
  amendment, then its own phase after Phase 2, beside E12 Phase 2 (the Slack
  broker). Reuses Phase 1's credential, socket, checks, `start_run`, queue and
  receipt line. `Decide: SLICE` (not a 0.1.0 launch proof).

- Resume grants, `wait_for`, `DeliverEvent`, `vam.github` (later E5
  phase). The primary agent does not need them: its result path is
  **report back**, a host-written queued message `vamd` puts in the
  asking thread when a run it started for another agent ends
  (the primary-agent proposal (backlog in `.cursor/workstreams/README.md`) §5), built in the `primary-agent` lane.
  `DeliverEvent` will reuse that inbox entry.
- Delegation by the primary agent is **not** option B: it uses a per-call
  grant minted on an approved `delegate_run` call, so locked decisions 5
  and 9 hold unchanged. It reads decision 6 as "any run a person did not
  start" (the primary-agent proposal (backlog in `.cursor/workstreams/README.md`) §9, P-A, approved by the owner
  2026-11-01).
- Notifications and `vam.notify` (`docs/src/content/docs/orchestrator/notifications.md`, `notifications`
  tracker; slice 2 reuses this lane's gateway socket and credential checks and
  serves `HostedNotificationGateway` on it).
- Apps, shared Spaces, retention, rename UI, ending name lookups.
- "Started by schedule" run label in the app (WO-2.6 remainder; `Decide:
  BACKLOG`); a shared broker gateway client crate when a second broker needs
  one (Phase 2 Q6); the CLI receipt line naming the surface before the
  trigger ref (Phase 2 Q9).

## Progress log

| Date | Phase | What landed (end with the scoreboard: scenarios green, red, fix cycles, wall time) |
|---|---|---|
| 2026-10-29 | — | Tracker opened from `main` (E5 design approved: P1, P2, five §7 defaults). Base `feat/work-outside-chat` cut. Phase 1 breakdown agent started. |
| 2026-10-29 | 1 | Breakdown approved (owner): recommendations 1–3, Q3 default, thread key `(space_id, thread_id)` with scoped lookups (locked decision 10 amended; wave 3 builds it), CI rules. Wave 0 started (plan only). |
| 2026-10-29 | 1 | Base takes `main` (`keys.rs` CSPRNG hotfix merge). The hotfix mints from `rand::rngs::OsRng` (`keys.rs:183-189`), so Q6 is settled: grant and id secrets use `OsRng`, the same generator, with no `keys.rs` change for it. Waves 6–8 unblocked. Wave 0 plan approved with: no-run check on the HTTP-refusal scenario, existing scenarios parse unchanged, pre-ids workspace fixture recorded from today's vamd. |
| 2026-10-29 | 1 W0 | **Wave 0 merged** (merge commit, kept draft: no CI minutes; Sonnet 5.5 low). Eight scenarios `pending`, red for the right reason: six trigger scenarios at `when.4` (`expect: grant`, 0 listed), `thread-is-reached-by-id-or-by-name` at `when.1` (no `th_…` id), `existing-workspace-keeps-its-id-and-history` at `when.1` (`GET /v1/spaces` 404). Runner steps in `crates/vam-e2e/src/triggers.rs`; world pieces `trigger-test-broker` (new kind `broker`, fixed id), `agent-with-trigger-tool`, `agent-before-ids`, `workspace-from-before-ids` (recorded from a real vamd, new kind `workspace-seed`). Deviations for later waves: scenarios use `echo.ping` until wave 2 adds `echo_arm_trigger` (the broker refuses an unserved tool); `revoke-grant` is `via: api` until wave 6's CLI; broker `fire` prints one JSON line per fire and writes `<dir>/grant-token` (contract in `e2e/README.md`). Orchestrator re-run: fmt ok, `vam-e2e check` 72 scenarios / 27 pieces, `cargo test -p vam-e2e` ok, smoke **56 pass, 0 fail, 8 red (right reason)**. Known: `cargo clippy -p vam-e2e -D warnings` fails on three lints already on the base (`report.rs` too_many_arguments, `Step` large_enum_variant, `engine_selftests.rs` cloned_ref_to_slice_refs), not this wave's. Scoreboard: 0 green, 8 red (right reason), 0 fix cycles, wall ~35 min. |
| 2026-10-29 | 1 W1 | Wave 1 plan approved (owner) with: rate limits left at 0 = node default 60/min and 1440/day (never unlimited); `run_request_permission` with empty `grant_tools` refused. Building (Sonnet 5.5 medium). Owner question on on-demand broker runs answered with option B (Explicitly later). |
| 2026-10-29 | 1 W1 | **Wave 1 merged** (merge commit, draft; Sonnet 5.5 medium). Contract: `hosted_services.proto` (`RunRequestPermission` = 10, `InvokeRequest.run_grant` = 9, `RunGrant`, `GrantTarget`, `RunGrantRequest`, `PlaceChoice`, `OnBusy`, `FireKind`, `HostedTriggerGateway` with `RequestRunTrigger`/`RevokeRunGrant`, outcomes and reasons, `CallerContext` 8/9/10), `workflow.proto` (`RunSpec` 12/13, `RunStatus` 20/21/22, `RunSource`), `orchestrator.proto` (`Space`, `Thread`, `ListSpaces*`, `ApiKeyRecord` grant fields, `ListApiKeysRequest.holder`, `PutHostedBrokerRequest.peer_uid` int64, `HostedBrokerRecord.gateway_socket`). Validation in `vam-spec` (non-empty `grant_tools` ⊆ tools, loop-guarded ⊆ tools and ⊇ grant tools, surface charset and reserved names, no negatives, third party refused); limits 0 → `DEFAULT_RUN_REQUESTS_PER_MINUTE` 60, `_PER_DAY` 1440, `DEFAULT_MAX_ACTIVE_GRANTS_PER_USER` 50 via `effective_run_request_limits`; subset gate in `hsp_registry.rs`; short enum names (`SAME`, `QUEUE`, …) decode. Docs: `hosted/triggered-runs.md` rewritten, `protocol.md`, `capability-manifest.md`. `host_tools.rs`: one struct-literal line only. Orchestrator re-run: fmt ok, `cargo check --workspace` ok, `vam-spec` 231 + integration ok, `hsp_registry` 15, hsp integration suites ok except `hsp_uds::uds_socket_acl_deny_fails_fast`, which **fails identically on the base** (the container runs as uid 0, which ignores the socket ACL; CI runs non-root); clippy 291 findings on branch = 291 on base (none added); smoke 56 pass, 0 fail, 8 red right reason. **Carried to wave 8 (required):** validation refuses only `THIRD_PARTY`; a manifest with an unspecified tier passes, so gateway check 1 must allow only core, first-party and operator tiers. Scoreboard: 0 green (scaffold), 8 red right reason, 0 fix cycles, wall ~25 min.
| 2026-10-30 | 1 W2 | Wave 2 plan approved (owner) with: `fire` takes `--gateway-socket` explicitly, read by the runner from the broker record's `gatewaySocket` (no guessed default; wave 8 fills it). Spec change approved by the orchestrator (tool name only): `triggered-run-cannot-arm-another-trigger` renames `echo.ping` → `echo_arm_trigger` in three `then` lines, assertions unchanged. Test broker stays in vam-orchestrator (a test fixture, not a product broker; decision 22 covers product brokers). Building (Sonnet 5.5 low). |
| 2026-10-30 | 1 W2 | **Wave 2 merged** (merge commit, draft; Sonnet 5.5 low). `hsp-echo-broker --triggers` (`TriggerTestBroker` in `hsp_echo.rs`): exports `echo_arm_trigger`, manifest `runRequestPermission {grantTools, loopGuardedTools: [echo_arm_trigger], surface: echo}`; on Invoke writes the `RunGrant` token to `<state-dir>/grant-token` (0600; never in a response or log), refuses a call without a grant, answers with labels only; `after_ms`/`times` arm self-firing with one key. `fire --state-dir --gateway-socket (required) --key --times --kind` prints one JSON line per fire. Runner reads `broker.gatewaySocket` from `GET /v1/hosted/brokers/{id}` and passes it (empty until wave 8 → class-product failure). Six trigger scenarios and two world pieces switched `echo.ping` → `echo_arm_trigger` (spec change, name only, three `then` lines in `triggered-run-cannot-arm-another-trigger`). Broker stays in vam-orchestrator (test fixture). Orchestrator re-run: fmt ok, `cargo check --workspace` ok, `hsp_echo` unit test 1 pass (fake gateway: refuse without grant, 0600 token, no token in answer, ACCEPTED then DUPLICATE), vam-e2e tests ok, `vam-e2e check` 72/27, clippy 291 = base, smoke 56 pass, 0 fail, 8 red right reason (the six still red at `when.4`). Scoreboard: 0 green, 8 red right reason, 0 fix cycles, wall ~20 min. |
| 2026-10-30 | 1 | **Owner: ids first.** Locked decision 10 rewritten (ids are the identity, `sp_…`/`th_…` directories with no owner prefix, ownership in the `spaces` row, names are labels, no backfill, fresh state on deploy). Amendment block added to Phase 1 (scenario retire/new/change, waves 3 and 4 rescoped, new the VAM app wave 4A). Wave 3 sent back to replan. |
| 2026-10-30 | 1 W3 | Wave 3 replan approved (owner): id-only Spaces and threads, one access function, runner carries ids for follow-ups (no scenario `when` edits), routes accept `sp_…`/`th_…` now, `names-are-only-labels` and `thread-is-reached-by-its-id` turn green, old scenario and fixtures retired; default Space split draft/released only (not per version). Owner added create and rename of Spaces and threads (wave 4 API + CLI, wave 4A app). Expected smoke 58 pass, 0 fail, 6 red. Building (Sonnet 5.5 medium). |
| 2026-10-30 | 1 W3 | **Wave 3 merged** (merge commit, draft; Sonnet 5.5 medium; replanned once on the owner's ids-first decision). New `spaces.rs`: `spaces` and `threads` tables (threads keyed `(space_id, thread_id)`; labels plain columns), `mint_id` (`OsRng`, 26 base32), one access function `get_space(owner, id)` / `space_for_route` (foreign = missing, NotFound), `get_thread` inside its Space, default Space per (owner, agent, draft|released), `resolve_place` (Space id or default; thread id or new thread labelled as given or "untitled"), `create_space` ready for wave 4. Workspaces stored as `sp_…`, threads as `.vam/threads/th_…`, no owner prefix; `RunStatus` `spaceId`/`threadId`/`workspaceId` (= space id), `threadRef` = label; queued requests carry ids; `AgentWorkspaceMismatch` kept for a Space bound to another agent. `GET /v1/spaces[/:id]`; thread and workspace-file routes take `sp_…` through the access function; seed directory chosen by the Space label. Runner sends ids on follow-ups (no scenario `when` edits). Scenarios: retired `existing-workspace-keeps-its-id-and-history` (+ `workspace-from-before-ids`, `agent-before-ids`, fixture); new `names-are-only-labels` green; `thread-is-reached-by-id-or-by-name` → `thread-is-reached-by-its-id` (by-name step dropped; owner-approved spec change) green. 19 Rust tests moved from prefixed names to ids; the user-isolation test now proves foreign Space id = 404 on runs, threads, files and spaces. Old prefix helpers left in place (test callers only) until removed. Orchestrator re-run: fmt ok, `cargo check --workspace` ok, vam-storage ok, vam-orchestrator + vam-cli full suites ok except the known root-only `hsp_uds::uds_socket_acl_deny_fails_fast`, vam-e2e ok, `vam-e2e check` 72/25, clippy 290 (base 291), smoke **58 pass, 0 fail, 6 red right reason**. Disk 13 GB free. Scoreboard: 2 green (`names-are-only-labels`, `thread-is-reached-by-its-id`), 6 red right reason, 0 fix cycles (1 replan), wall ~75 min. |
| 2026-10-30 | 1 W4 | Wave 4 plan approved (owner): `POST /v1/spaces` (retry-safe with the idempotency key), `PATCH` rename of Spaces and threads (same id, label only, foreign = 404), CLI `vam space ls/create/rename`, `vam thread rename`, `--workspace`/`--thread` take ids, `--space` required on thread and workspace-file commands; old prefix helpers removed where only tests call them. **Owner chose option A:** new scenario `space-and-thread-can-be-renamed-in-place` (phase now 9 scenarios: 8 + this one; one over the playbook guideline, accepted). Model raised to Sonnet 5.5 medium (new API). Expected smoke 59 pass, 0 fail, 6 red. |
| 2026-10-30 | 1 W4 | **Wave 4 merged** (merge commit, draft; Sonnet 5.5 medium). `POST /v1/spaces {agentUrn, label}` (requires `Idempotency-Key`; retry returns the same Space via `space_create_keys (owner, key)`; agent checked with `get_agent_visible`), `PATCH /v1/spaces/{id}` and `PATCH /v1/spaces/{id}/threads/{threadId}` (label only, through `space_for_route`/`get_thread`, foreign = 404, idempotent); scope `sessions:write` (members hold it). CLI: `vam space ls|create|rename`, `vam thread rename --space`; `thread ls/show`, `workspace ls/cat/put` require `--space <sp_id>` (alias `--workspace`; `vam workspace ls` no longer positional — breaking for scripts); `vam run --workspace` takes `sp_…`, `--thread` a `th_…` or a label for a new thread; prints `space sp_… (label) · thread th_… (label)`. New scenario `space-and-thread-can-be-renamed-in-place` (owner option A) green. Removed `run_context::resolve_workspace_id`, `resolve_run_workspace_id`, `parse_run_workspace_name`; vam-storage prefix helpers stay (gc and tests use them). Orchestrator re-run: fmt ok, `cargo check --workspace` ok, vam-orchestrator + vam-cli + vam-storage full suites ok except the known root-only `hsp_uds` test, vam-e2e ok, `vam-e2e check` 73/25, clippy vam-spec+orchestrator 290 (= base), vam-cli+vam-e2e 9 (= base), smoke **59 pass, 0 fail, 6 red right reason**. Scoreboard: 1 green (`space-and-thread-can-be-renamed-in-place`; phase total 3 green, 6 red right reason), 0 fix cycles, wall ~45 min. |
| 2026-10-30 | 1 W5 | Wave 5 plan approved (owner): `run_start::start_run(state, RunCaller, req, StartOpts)` shared by the HTTP handler and the queue starter (no rebuilt `AuthContext`); typed `admit` (`Proceed | Steered | Queued | Busy(Live|Parked|Finalizing|Stopping|AlreadyQueued)`) with `to_http()` keeping today's responses; `runs.source` (`broker:<id>` or NULL) and `runs.credential_id`, `run_inbox.credential_id` with a partial unique index (one queued fire per credential); `QueuedCaller` gains `source`, `credential_id`, `not_after` (serde defaults); `expire_queued_run` past `not_after`. `run_requests` trigger columns stay for wave 9, with `RunCaller` passed to its writer now. Orchestrator runs full `cargo test --workspace` before merge. Building (Sonnet 5.5 medium). |
| 2026-10-30 | 1 W5 | **Wave 5 merged** (merge commit, draft; Sonnet 5.5 medium). `api/run_start.rs`: `RunCaller` (HTTP always `Person`, no credential), `RunSource` (Person | Trigger), `StartOpts`, `StartOutcome::to_http`, `start_run` — `create_run` keeps auth/PDP/idempotency/body, the queue starter calls `start_run` directly (`begin_queue_start` held). Typed `admit` (`Proceed | Steered | Queued | Busy{Live, Parked, Finalizing, Stopping, AlreadyQueued}`); a triggered fire always queues (never steers), keeps playbook and inputs, one queued fire per credential (`credential_has_queued_fire` + partial unique index on `run_inbox(credential_id)`); past `not_after` → `expire_queued_run` (EXPIRED), checked over every queued row. `runs.source`/`runs.credential_id`, `run_inbox.credential_id`; `QueuedCaller` gains `source`, `credential_id`, `not_after`; `record_run_request_for(run_id, &RunCaller)` ready for wave 9. **Signing outage:** the session's commit signer refused connections for about an hour after the build; the owner chose GitHub-side commits (B), which the proxy refused (Git Data API writes are 403; the connector needs inline content, 840 KB here); the signer recovered and the commit is SSH-signed. Orchestrator re-run: full `cargo test --workspace` ok except the known root-only `hsp_uds` test; fmt ok; clippy 289 (base 290); smoke **59 pass, 0 fail, 6 red right reason**. Scoreboard: 0 green (refactor; phase 3 green, 6 red right reason), 0 fix cycles, wall ~2 h including the outage. |
| 2026-10-30 | 1 | **Base takes `main`** (bench CI gateway model routing; 10 commits) through merge commit (Sonnet 5.5 medium sync session). Conflict in `api/mod.rs`: main changed `create_run`'s old body, which wave 5 had moved to `run_start.rs`; main's three hunks (provider resolution, provider in the process-guest refusal, `provider` on `model.resolved`) ported unchanged into `start_run`. Checks: full orchestrator suite ok (known root-only `hsp_uds`), main's three new scenarios pass, smoke **62 pass, 0 fail, 6 red right reason** (new baseline). ** is unsigned, once, on the owner's word**: the session's commit signer was down for over an hour (second outage of the day). **Owner rule (2026-10-30):** check the signer first; if it is down, commit unsigned and say so in the commit and the PR. |
| 2026-10-30 | 1 W6 | Wave 6 plan approved (owner): nine nullable `api_keys` columns (NULL = holder `http`), new `credentials.rs::check_credential(secret, Transport)` (constant-time compare; holder must match transport; revoked/expired/foreign all `Unauthenticated`; NULL expiry on a non-http holder fails closed), HTTP refuses `vam_rg_` tokens before lookup and any non-`http` holder after the hash lookup (covers `api/rate_limit.rs`), `insert_grant_credential` (`rg_` id and 32-byte `OsRng` secret, token `vam_rg_<id>.<secret>`, hash only), `GET /v1/keys?holder=broker|all` (no holder = today's list), `DELETE /v1/keys/{id}` revokes grants, `vam keys list --grants`, `vam grants list|revoke`. Grants owner-scoped for every role (owner decision). Grants end with their user (owner decision; test in this wave). Building (Sonnet 5.5 medium). |
| 2026-10-30 | 1 W6 | **Wave 6 merged** (merge commit, draft, signed; Sonnet 5.5 medium). Nine nullable `api_keys` columns (`PRAGMA table_info` guards; NULL = holder `http`); `credentials.rs::check_credential(secret, Transport)` (hash lookup, constant-time re-compare, holder = transport, revoked, expired, NULL expiry on non-http fails closed; stamps `last_used_at`) and `insert_grant_credential` (`rg_` id from `spaces::mint_id`, 32-byte `OsRng` secret base64url, token `vam_rg_<id>.<secret>`, hash only). HTTP: `authenticate_bearer` refuses `vam_rg_` before lookup; `authenticate_api_key` refuses non-http holders right after the hash lookup (before the revoked 403 and any write); rate limiter shares the path. Keys list: no holder = today's list, `holder=broker|all`; grants owner-scoped for every role incl. `keys:admin` and the `users:admin` key routes (foreign = NotFound). Deactivation revokes grants (tested; no reactivate path exists). CLI `vam keys list --grants`, `vam grants list|revoke`. Grant list label = "space / thread" and preview from `constraints_json` keys `thread_id` and `message` (wave 7 must write those keys). **Carried to wave 8 (required):** the `<grant_id>` in the token is not checked against the row; check it. Orchestrator re-run: full `cargo test --workspace` ok except the known root-only `hsp_uds` test; fmt ok; clippy 289 (= base), vam-cli 4 (= base); smoke **62 pass, 0 fail, 6 red right reason**. Scoreboard: 0 green (no scenario until grants are minted), 6 red right reason, 0 fix cycles, wall ~40 min. |
| 2026-10-30 | 1 W7 | Wave 7 plan approved (owner): issuance in `invoke_hosted_tool` via new `grants.rs` — loop guard reads `runs.source` (fail closed), `RunGrantRequest` decoded strictly, id-only resolution (foreign = missing), playbook and inputs checked, 32 KiB message, expiry 90 d default / 365 d clamp, live-grant cap; mint after `preflight_invoke` and secrets (refinement of the design's order: a preflight refusal mints nothing); token and secret added to `trace.secrets` (redaction untouched); `GrantGuard` revokes and deletes created rows on any error or drop. Owner: a grant's NEW thread is a plain `live` row read as empty before its first run (no `reserved` state). Expected: `grant-token-is-refused-over-http` green; smoke 63/0/5. Building (Sonnet 5.5 medium). |
| 2026-10-30 | 1 W7 | **Wave 7 merged** (merge commit, draft, signed; Sonnet 5.5 medium). New `grants.rs`: `check_call` (loop guard from `runs.source`, fail closed; strict `RunGrantRequest` decode; id-only resolution through `get_space`/`get_thread`, foreign = missing; SAME Space via the per-user lookup; Space bound to another agent refused; playbook pinned and `input_schema` checked; 32 KiB message; expiry 90 d default, 365 d clamp; live-grant cap, best effort) and `mint` (rows for NEW Space/thread, `insert_grant_credential`, typed `GrantConstraints` with `thread_id`/`message`, `GrantTarget` with `request_digest`); `GrantGuard` revokes and deletes created rows on any error or drop, disarmed only after a successful broker call with no tool error. `invoke_hosted_tool`: checks after the identity check, mint after preflight and secrets; token and secret pushed into `trace.secrets` (redaction untouched). A grant's NEW thread reads as an empty thread before its first run (`api/threads.rs`). Runner fixes: grant expectations count only the scenario's own Spaces; `revoke-grant` reads `keyId.urn`. 13 tests in `tests/hsp_grants.rs` (incl. two added on review: a playbook grant pins the playbook and binds inputs; schema-failing inputs refused with no grant and no rows). **`grant-token-is-refused-over-http` green and `active`.** Five trigger scenarios move forward and are red at their first fire (no gateway socket yet): `when.5` (repeated-fire, triggered-run-cannot-arm, starts-a-run-as-the-grant-owner), `when.6` (revoked), `when.7` (busy). **Note for Phase 2:** preflight's egress check reads URLs in tool arguments, so a `schedule_create` whose message or inputs carry a URL may be refused unless the schedule broker's egress allowlist covers it — open question for the Phase 2 breakdown. Orchestrator re-run: full `cargo test --workspace` ok except the known root-only `hsp_uds` test; fmt ok; clippy 289 (= base), vam-e2e 5 (= base); smoke **63 pass, 0 fail, 5 red right reason**. Scoreboard: 1 green (phase 4 green, 5 red right reason), 0 fix cycles (one test addition on review), wall ~55 min. |
| 2026-10-31 | 1 W8 | Wave 8 plan approved (owner): new `gateway.rs` (one UDS per approved broker with `run_request_permission`, path `VAMD_HSP_GATEWAY_DIR` or `<db dir>/hsp/gw/<12 hex>.sock`, dir 0700, socket 0600, opened on approval and at start, closed on `DELETE /v1/hosted/brokers/{id}`); the 13-step checklist with: tiers core/first-party/operator only, token `<grant_id>` must match the row, deactivated owner → `OWNER_DISABLED`, **the owner's current role must still allow run creation (owner-approved; else `NOT_PERMITTED`, also re-checked by the queue starter)**, **`peer_uid` NULL = no check, any set value incl. 0 enforced**, duplicate key on another grant → `INVALID_REQUEST`; `start_run` with `skip_if_busy`; `external_ref` pin; in-memory rate windows; `RevokeRunGrant`; queued fire discarded on revoke and deactivation; starter re-checks the grant; `trigger.fired` and `vam_hsp_trigger_total{broker_id, result}` (no `vam-telemetry` edit). Expected smoke 67/0/1. Building (Sonnet 5.5 medium), one PR in two commits. |
| 2026-10-31 | 1 W8 | **Wave 8 merged** (merge commit, draft, all commits signed; Sonnet 5.5 medium; the first build session was stopped by a container restart, its work was saved as a signed WIP commit and a second session finished it). New `gateway.rs`: one UDS per approved broker with `run_request_permission` (`VAMD_HSP_GATEWAY_DIR` or `<db dir>/hsp/gw/<12 hex>.sock`, ≤ 107 bytes, dir 0700, socket 0600; opened on register and at start, closed on `DELETE /v1/hosted/brokers/{id}`); `RequestRunTrigger` checklist: approval enabled, tiers core/first-party/operator only, `peer_uid` (NULL = no check, any set value incl. 0 enforced), `check_grant_token` (token `<grant_id>` must match the row; WRONG_BROKER, REVOKED, EXPIRED; revoked with a deactivated owner → OWNER_DISABLED), tool still approved, owner active and current role still allows runs (else NOT_PERMITTED; `RunCaller.scopes` from the current role), idempotency key validated, `trigger_ref` pin, per-broker minute/day windows, dedupe key `hsp:<broker>:<grant>:<key>` (**accepted deviation:** per grant, so two schedules never share or reveal a run), Space and thread still exist (TARGET_GONE), `start_run` with `skip_if_busy`; outcomes ACCEPTED/QUEUED/DUPLICATE/BUSY {SKIPPED, ALREADY_QUEUED, ACTIVE_RUN 2000 ms}/DENIED/UNAVAILABLE. `RevokeRunGrant`; queued fire discarded on revoke, on `revoke_key` for grants and on `deactivate_user`; the queue starter re-checks a trigger caller. `trigger.fired` outbox event; `vam_hsp_trigger_total{broker_id, result}`; info logs without the token. **Accepted deviation:** `host_tools.rs` emits a failed tool outcome for a hosted call that did not succeed (redaction untouched; `triggered-run-cannot-arm-another-trigger` needs it). Noted: an owner with no user row reads as a member (as the queue starter does; no user-delete route exists). openapi updated. **Green and `active`: `repeated-fire-with-the-same-key-starts-one-run`, `revoked-grant-starts-nothing`, `triggered-run-cannot-arm-another-trigger`, `trigger-on-a-busy-workspace-runs-after-the-task`.** `trigger-starts-a-run-as-the-grant-owner` red at `then.4` (run source, wave 9). Orchestrator re-run: full `cargo test --workspace` ok except the known root-only `hsp_uds` test; fmt ok; clippy 289 (= base), vam-e2e 5 (= base); smoke **67 pass, 0 fail, 1 red right reason**. Scoreboard: 4 green (phase 8 green, 1 red right reason), 0 fix cycles, wall ~2 h including the restart. |
| 2026-10-31 | 1 W9 | Wave 9 plan approved (owner): `run_requests` trigger columns written by `record_run_request_for`; `RunStatus.source` for every run (**owner: `kind: person` for people, `trigger` with details for triggered runs**); flat receipt `requestTrigger` (absent and byte-identical for people; signed with the rest); CLI receipt line; `CallerContext` space/thread/run_source stamping; `grant.expiring` from the existing sweeper, sent once via a new `grant_expiry_notices` table (accepted instead of an `api_keys` column). Expected: all 9 Phase 1 scenarios green, smoke 68/0/0. Building (Sonnet 5.5 medium). |
| 2026-10-31 | 1 W9 | **Wave 9 merged** (merge commit, draft, signed; Sonnet 5.5 medium). `run_requests` gains `broker_id, grant_id, trigger_ref, fire_at, fire_kind` (written by `record_run_request_for`); `RunStatus.source` for every run (`kind: person`, or `trigger` with broker, grant, ref, fire time, kind); flat receipt `requestTrigger {brokerId, grantId, triggerRef, fireAtMs, fireKind}` (absent for a person; a test proves a trigger receipt = the person receipt plus that key; signed with the rest; `receipt.proto` untouched); `vam receipt show` line "requested by …, surface … — <ref> via grant rg_… (<broker>), fire time …"; `CallerContext.space_id/thread_id/run_source`; `grant.expiring` from the existing vamd sweeper (`ttl.rs::sweep_expiring_grants`, once per grant via `grant_expiry_notices`, retried if the append fails). **`trigger-starts-a-run-as-the-grant-owner` green and `active` — all nine Phase 1 scenarios green and `active`.** Orchestrator re-run: full `cargo test --workspace` ok except the known root-only `hsp_uds` test; fmt ok; clippy 289 (= base), vam-cli 4 (= base); smoke **68 pass, 0 fail, 0 red**. Disk fell to 2.7 GB during the run: cleanup ladder steps 1 and 2 (incremental, 168 older test executables, 8.5 GB) → 11 GB free. Scoreboard: 1 green (phase 9 green, 0 red), 0 fix cycles, wall ~45 min. |
| 2026-10-31 | 1 | **Owner: wave 10 (docs) deferred** until isolation-truth Phase 3 is on `main`: that lane rewrites the docs site (11 of the 12 pages wave 10 would edit are changed on `feat/isolation-truth`, `security/key-management.md` and `orchestrator/daemon.md` deleted; it also edits `astro.config.mjs` and `openapi.yaml`). Pending docs follow-up (own PR after IT Phase 3 lands): Spaces and threads by id with labels, create/rename, grants, the gateway for broker authors, receipt `requestTrigger` and "via grant", `RunStatus.source`, openapi gaps (`gatewaySocket`, `peerUid`, `RunStatus.source`/`spaceId`/`threadId`, key grant fields), the breaking `--space` change; the docs site must build. Release is not blocked; the release PR carries the deploy note. Known conflict: wave 1 rewrote `hosted/triggered-runs.md`, `protocol.md`, `capability-manifest.md`; the later of the two lanes to reach `main` resolves them. **Owner (2026-10-31):** start wave 4A (the VAM app); GitHub CI may be used for testing; release Phase 1 to `main` once 4A is done (CI green). |
| 2026-10-31 | 1 W4A | Wave 4A plan approved (owner): the VAM app sends `spaceId`/`threadId` on follow-ups (one `buildRunSpec`), Spaces from `/v1/spaces`, history and files by id, labels only in the UI, owner-prefix helpers removed; "New Space" and rename for Spaces and threads; **owner: include the "Scheduled access" grants list with revoke; remove the workspace delete button** (no Space-delete route; backlog item: a Space-delete route that also revokes grants bound to that Space, whose brokers then answer TARGET_GONE). The VAM app base `feature/work-outside-chat` cut from the VAM app `main`. opened early (vam unchanged by 4A) so full CI runs meanwhile. Building (Sonnet 5.5 medium). |
| 2026-10-31 | 1 | **Release PR opened** (full CI allowed by the owner). CI: checks, workspace tests and the Firecracker lane green; **smoke red: 22 pass, 46 fail** (all pass locally as root). **Fix cycle 1** (Sonnet 5.5 medium, merged into the base): root cause a root-vs-non-root permission bug — the directory backend seals `materialized/skills` read-only at the end of materialize, then `seed_hsp_skills`/`seed_mcp_skills` write into it (EACCES as a non-root user; root ignores mode bits), and `copy_dir_all` opened a 0444 file hard-linked into the chunk cache for writing. It surfaced only once a broker was registered (the first `hosted-<broker>` skill), so every run after the first trigger scenario failed on CI's non-root runner. Fix: `vam_storage::with_skills_writable` unseals `skills/` only for the seeding and seals it again before the guest starts; `copy_dir_all` replaces a destination file instead of writing through it (no cache write-through). Regression tests: `copy_dir_all_replaces_read_only_and_hard_linked_files`, `with_skills_writable_unseals_and_reseals`, `seed_hsp_skills_again_into_a_sealed_skills_dir` (run in the non-root CI test job). Smoke 68/0/0 as root and as a non-root user. Lesson for the playbook: run the smoke set once as a non-root user before a release PR. **Wave 4A merged** into the VAM app `feature/work-outside-chat` ( → signed; 773 app tests, tsc clean, 50 Rust tests re-run by the orchestrator); app release PR open, CI green, merges after. |
| 2026-10-31 | 1 | **Phase 1 released to `main`** (owner's word 2026-10-31): → (merge commit) → (merge commit); bases fast-forwarded to `main`. Release CI: run 3  checks, workspace tests and Firecracker lane green; harness cleanup fix (`scripts/e2e/run.sh` makes the state tree writable before `rm -rf`; signed); run 4 (`skip-vm-lane`) checks and smoke **68/0/0 green on the non-root runner**, the remaining workspace-test job cancelled under the owner's shortcut (same product code proven). CI used for the release: about 80 runner minutes on plus the push run on `main`. Team checklist written (10 steps). Phase status: released, checklist out. Next: Phase 2 (the schedule broker) breakdown; docs follow-up after isolation-truth Phase 3. |
| 2026-10-31 | 2 | **Phase 2 breakdown drafted** (phase-breakdown agent, Opus 5.5 high): 7 new scenarios, no existing `then` changes; waves 0–5 plus optional wave 6 (WO-2.8, links in a schedule refused by preflight: a `vamd` behaviour fix, owner's word) and deferred docs wave 7; Q1 (relative `once`) to answer before wave 0. Awaiting owner approval. |
| 2026-10-31 | 2 | **Breakdown approved** (owner): recommendations 1 (relative `once`) and 3 (delay the `once` revoke on QUEUED) taken; yes to wave 6 (WO-2.8). Merge gate open (Phase 1 on `main`, base level with it). Wave 0 next. |
| 2026-10-31 | 2 W0 | **Wave 0 merged** (merge commit, signed; Sonnet 5.5 low). Seven scenarios `pending`, red for the right reason (class `product`): six at `when.4` (no `sch_` in the tool result; the stub answers UNIMPLEMENTED), `schedule-is-saved-only-after-the-user-approves` at `when.7` (`"schedules":[]` absent). Pinned result contracts in `e2e/README.md` "Schedules" (`skipped_busy`, `"schedules":[]`, `NOT_FOUND`, `sch_`, `revokedAt`). `brokers/vam-schedule` stub (six tools, refuses every call) declares every phase dependency (new: `croner =4.0.1`, `chrono-tz =0.10.4`); runner `crates/vam-e2e/src/schedule.rs` (`wait-scheduled-run`, `expect scheduled-runs`, `stop-broker`, `start-broker`, `{{schedule-id}}`); broker piece gains `bin`, `args`, `restartWithGateway`, cleanup `DELETE /v1/hosted/brokers/{id}` (test broker too); mock `{{last:<prefix>}}`. Deviations: `schedule` tag added to the catalog; fire times PT10S/PT12S/PT1H where a check would race the fire. Carried to wave 4: `expect thread` reads with the bootstrap key; the member steps of `another-users-schedules-are-invisible` may need a per-identity read (runner fix, not a `then` change). Orchestrator re-run at the PR head: `--full` workspace tests ok except the known root-only `hsp_uds`; smoke **68 pass, 0 fail, 7 red right reason**; `cargo tree -p vam-schedule` has no vam-orchestrator. Actions ran the fast jobs once (marked ready before the owner's 2026-10-31 rule; merged without waiting). Scoreboard: 0 green, 7 red (right reason), 0 fix cycles, wall ~1 h 10 min. |
| 2026-10-31 | 2 W1 | **Wave 1 merged** (local merge commit into the base, PR kept draft: the API refuses to merge a draft; Sonnet 5.5 medium). WO-2.1: `StateDir` (dir 0700, tightened if looser; exclusive `try_lock` on `vam-schedule.lock` 0600; a second process exits 1), `Store` (design schema verbatim plus `pending_revokes.not_before` for Q4; `PRAGMA user_version` migrations; db 0600), `GatewayClient` (connect per call, 10 s timeout, typed `FireOutcome`; `RevokeRunGrant`), `GrantToken` (redacted `Debug`/`Display`, `expose()` only to build a request or store the row), `idempotency_key`, built-in manifest (test: equals `e2e/world/schedule-broker.yaml`; dev-dep `serde_yaml`, one `Cargo.lock` line), Invoke without user or agent → INVALID_ARGUMENT; tools still UNIMPLEMENTED. 28 crate tests (fake gateway incl. restart and hang; log-redaction test). Orchestrator review: the store holds no credential but the grant token. Orchestrator re-run at the PR head: `--full` workspace tests ok except the known root-only `hsp_uds`; smoke **68 pass, 0 fail, 7 red right reason** (same steps). Noted: `cargo clippy -p vam-schedule -- -D warnings` stops in `vam-spec` generated code (`large_enum_variant`, also hits `vam-web` on the base); crate clean with `--no-deps` — the Phase 2 exit uses `--no-deps` unless the owner asks for a vam-spec fix. Scoreboard: 0 green, 7 red (right reason), 0 fix cycles, wall ~45 min. |
| 2026-10-31 | 2 W2 | **Wave 2 merged** (local merge commit into the base, PR kept draft; Sonnet 5.5 low). WO-2.2: `clock.rs` (`Clock`, `SystemClock`, `TestClock`), `when.rs` (`When::parse`, `first_fire(saved_at)`, `next_after`, `check_floor`, `grace_window(nominal)`; `WhenError`, all INVALID_ARGUMENT): once at a wall time in an IANA zone or an instant with offset, once `in` (integer `PnWnDTnHnMnS`, > 0, from approval: Q1), five-field cron in a zone; a gap time resolves to the first valid instant after it, a repeated time to its first occurrence. Owner changes: NY fall instants corrected (02:30 on 2026-11-01 = 07:30Z; added 01:30 = 05:30Z first occurrence); cron floor scans every fire within 31 days (cap 5000) so a dense pair that is not next in line is refused (`0,5 9 1 * *` saved on the 2nd). `croner` 4.0.1 matched every DST expectation: no design amendment. 42 lib tests + 7 integration. Orchestrator re-run at the PR head: `--full` workspace tests ok except the known root-only `hsp_uds`; smoke **68 pass, 0 fail, 7 red right reason**; no `unwrap`/`expect` outside tests; `Cargo.lock` unchanged. Scoreboard: 0 green, 7 red (right reason), 0 fix cycles, wall ~35 min. |
| 2026-10-31 | 2 W3 | **Wave 3 merged** (local merge commit into the base, PR kept draft; Sonnet 5.5 medium). **Unsigned:** the wave commit and the merge commit — the session signer refused connections; one retry after a minute each, then unsigned per the signing rule. WO-2.4: `fire.rs` (`Gateway` trait over `GatewayClient`; `Scheduler` with injected `Clock`; `run()` sleeps ≤ 30 s, woken by `wake_handle()`; ≤ 4 in flight, one per schedule; full-jitter backoff 2 s → 5 min, never sooner than `retry_after_ms`, persisted; give up at `not_after` with `failed_unavailable`/`skipped_busy`; catch-up `latest` (one fire inside the grace window, older ones one `missed` row) and `skip`; ≤ 60 s late is on time; `once` completion per Q4, DUPLICATE timed like QUEUED; `pending_revokes` retried every 60 s honouring `not_before`; a begun fire resumes with its own key after a crash). Store: `begin_fire` writes the ledger before the call; `settle` records outcome, advances the row and queues any revoke in one transaction; `mark_deleting`. Owner change: DENIED other than REVOKED/EXPIRED → `needs_attention`, disabled, **grant kept** (re-enable without a new approval; tested for four reasons). Bin starts the loop only with `--gateway-socket`. Carried to wave 5: `settle` updates `fires` with `kind = 'scheduled'`; run-now needs its kind there. 70 crate tests, three clean runs. Orchestrator re-run at the PR head: `--full` workspace tests ok except the known root-only `hsp_uds`; smoke **68 pass, 0 fail, 7 red right reason**; logs carry ids only; no `unwrap`/`expect` outside tests; `Cargo.lock` unchanged. Scoreboard: 0 green, 7 red (right reason), 0 fix cycles, wall ~45 min. |
| 2026-10-31 | 2 W4 | **Wave 4 merged** (local merge commit into the base, PR kept draft; signed; Sonnet 5.5 medium). WO-2.3a: `ops.rs` with `schedule_create`, `_list`, `_get`, `_update` (schemas `additionalProperties:false`; unknown and identity-shaped keys refused; typed `INVALID_ARGUMENT`/`NOT_FOUND`/`FAILED_PRECONDITION` prefixes; `run` passed to vamd unchanged; target labels from `RunGrant.target`; token never in an answer — answers built field by field). Owner-first reads (`owner AND (creator OR visibility='user')`), creator-only writes; foreign id → NOT_FOUND. Create: grant required, Q1, floor, Q5 (grant expiring at or before the first fire refused), Q12 (retry returns the existing schedule, extra token to `pending_revokes`), wake. Update: title/when/enabled/catchUp; with `run` re-bind (old token queued); from `needs_attention` re-enable keeps the grant. **Owner change:** `schedules.version` (migration 2), bumped by every user write; `settle`/`schedule_retry` advance the row or queue a revoke only if unchanged, the outcome is always recorded (pause, `when` change, once completion, backoff, delete marking during a fire tested). **Design amended (orchestrator):** schedule id = `sch_` + 26 hex of `sha256("vam.schedule.id\n" + grant_id)` — vamd redacts 12-char runs of the token from broker answers, so a grant-id suffix reached the agent as `sch***` (1 fix cycle, 0 → 5 green). Runner fix: `expect_thread` reads with the run's identity key (member scenario). `deleted-schedule-never-fires-again` `expectFail` moved `when.4` → `when.9` (fails later; `then` untouched). **Green and `active`: `scheduled-message-runs-at-its-time`, `schedule-is-saved-only-after-the-user-approves`, `schedule-for-a-new-workspace-runs-there`, `schedule-set-to-skip-does-not-wait-for-a-busy-workspace`, `missed-fire-runs-once-after-the-broker-restarts`, `another-users-schedules-are-invisible`.** 96 crate tests. Orchestrator re-run at the PR head: `--full` workspace tests ok except the known root-only `hsp_uds`; smoke **74 pass, 0 fail, 1 red right reason**; no `then` edited; no `unwrap`/`expect` outside tests; `Cargo.lock` unchanged. Scoreboard: 6 green (phase 6/7), 1 red right reason, 1 fix cycle, wall ~1 h 15 min. |
| 2026-10-31 | 2 W5 | **Wave 5 merged** (local merge commit into the base, PR kept draft; signed; Sonnet 5.5 low). WO-2.3b: `manage.rs` — `schedule_delete` (owner + creator; `RevokeRunGrant` first, then the row, its fires and any queued revoke go; gateway down → `deleting`, token queued, version bumped, the per-minute drain purges it; a repeat delete is idempotent); `schedule_run_now` (owner + creator; one gateway attempt; `fire_at` = now to the second, key also hashes `InvokeRequest.run_id`, `not_after` + 24 h, kind `run_now`; own ledger row via `Store::finish_run_now`; never changes the schedule row; a deny returns the outcome and leaves the state). Receipt: nothing needed from the broker (vamd builds it from the trigger facts). WO-2.5 script: `scripts/run-schedule-broker.sh` (build, start, `PUT /v1/hosted/brokers` with the manifest printed by `vam-schedule-broker manifest` — one source, owner's choice — read `gatewaySocket`, restart with it, verify six tools; `--stop`, `--dry-run`, `--foreground`); `bash -n` only (no shellcheck here); not yet run against a live vamd. **Green and `active`: `deleted-schedule-never-fires-again`** (first run). **Known issue:** two run-nows from different runs in the same second share one `fires` row (key `(schedule, fire_at, kind)`), so the second dedupes to the first run — rare, backlog. 102 crate tests. Orchestrator re-run at the PR head: `--full` workspace tests ok except the known root-only `hsp_uds`; smoke **75 pass, 0 fail, 0 red**; no `then` edited; `Cargo.lock` unchanged. Scoreboard: 1 green (**phase 7/7**), 0 red, 0 fix cycles, wall ~40 min. |
| 2026-11-01 | 2 W6 | **Wave 6 merged** (local merge commit into the base, PR kept draft; signed; Sonnet 5.5 low). WO-2.8 (owner-approved spec change, Q2): `hsp_registry::preflight_invoke` calls `assert_invoke_egress_for_tool`, which drops only the top-level `run` object before the URL scan for a tool in the **approved** manifest's `run_request_permission.grant_tools`; every other key and tool is scanned as before; `grants::check_call` still decodes and validates `run` before preflight and the grant is minted after it. Tests: link in `run.message` passes; links in `run` and `title` refused; non-grant tool with a `run` link refused; manifest without `run_request_permission` refused. `schedule_create`'s description now says links are allowed. Orchestrator: read the diff line by line; re-run at the PR head: `--full` workspace tests ok except the known root-only `hsp_uds`; smoke **75 pass, 0 fail**, `secret-never-reaches-guest-encoded` green. Deploy note updated (manifest from the binary; links allowed). Scoreboard: 0 new green (7/7 hold), 0 red, 0 fix cycles, wall ~30 min. |
| 2026-11-01 | 2 | **Phase close in the base**: 7/7 green; smoke 75/0 as root and as a non-root user; clippy `--no-deps` clean on `vam-schedule`; benchmark rows recorded (3 of 3 on the mock; parked cost 0); team checklist written. Release PR next. |
