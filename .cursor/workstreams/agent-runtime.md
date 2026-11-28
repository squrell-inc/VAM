# Workstream: Agent runtime (E10): agents that finish the job

```yaml
status: active
slug: agent-runtime
current_phase: 3 (released to `main` 2026-10-29, checklist out); Phase 4 breakdown drafted 2026-11-01, owner approval pending
design_docs:
  - design/agent-runtime.md
  - docs/src/content/docs/runtime/workflow.md        # park and resume, extended here
  - design/agent-experience.md # P1–P11, epic order
```

> **Archive.** The pre-Phase-0 hotfixes, the Phases 0–3 records and the older progress-log rows live in
> `.cursor/workstreams/archive/agent-runtime-phases-0-3.md`. Read it only when a task needs that detail (cost rule, `phase-playbook.md` §13).


## Goal

VAM agents finish the job: they use a connected tool in plain words the
first time, look things up on the web, keep full context across
follow-ups, take messages while they work, run long tasks within a
budget, never lose finished work, and never repeat an external effect.
This works for any model the broker serves, and the published benchmark
shows it. Serves problems **P12** (can't use what it's given) and **P13**
(can't do real, ongoing work), design §Why.

## Out of scope

- A new isolation model. VM, vsock, Cedar and the E8 egress design stay.
- Training or fine-tuning models.
- A general workflow engine. Tasks are agent runs only.
- Slack, email, schedules and webhooks (E5), beyond the outbox and
  parking they build on.
- Code mode, child-VM sub-agents, embedding-based tool ranking: listed
  under "Explicitly later".

## Locked decisions

Do not re-litigate without editing this list. Confirmed by the product
owner on 2026-10-15 (design Part VIII and "Rules this would change").

1. **Security stays first.** Every change here keeps the zero-trust
   guest. New powers (web, sub-agents, background work, memory, the
   inbox) go through the host and Cedar. A change that weakens P1 is
   rejected, whatever it does for P12/P13.
2. **The thread holds durable context; a task is the unit of work.** A
   task is today's run: budget, effects, attestation and metering stay
   per task. A message during a task steers it. A message while idle
   starts a new task, resuming from the thread's last turn checkpoint.
3. **A mid-run message is a steer by default**, delivered at the next
   turn boundary. **Stop** and **Queue** are explicit. A pending approval
   stays open; Stop denies it.
4. **Sub-agents run in the same VM first** (Phase 5), with the parent's
   permissions and budget. Child-VM sub-agents are later.
5. **Starting budgets:** 50 turns (hard cap 200), 30 minutes active
   time per task, cost from the operator's budgets, keep-warm 5 minutes,
   at most 4 live sub-agents with depth 1. The benchmark tunes these.
   Agents may raise them within operator limits.
6. **Memory:** personal facts (user × agent) apply automatically and are
   visible and editable. Recipes per connection are proposed by the host
   and approved by the connection owner before anyone else uses them.
7. **Web search:** bring your own key through a first-party web broker,
   one provider adapter first. Provider built-in search is not the
   default (it bypasses egress policy and audit).
8. **Benchmarked models:** Claude Haiku 5.5, Claude Sonnet 5.5, one OpenAI
   model, one open model through Ollama. Everything else the broker
   serves "works" with no benchmark claim.
9. **Delivery:** the app, the existing `on_paused` hooks, and the
   notification module's channels (`docs/src/content/docs/orchestrator/notifications.md`); Slack first.
   (Reworded by the owner 2026-11-02; was "Slack and email stay in E5".)
10. **Rules replaced** (each lands in the phase named):
    - One run = one VM, always terminated → park after keep-warm; a
      stopped VM never ends the task (Phase 5).
    - A second message is rejected (`AgentRunning`) → the inbox
      (Phase 3). The workspace upload rule (`workspace-files-upload`
      decision 1) is unchanged.
    - Follow-ups replay flattened text → turn checkpoints (Phase 2).
    - MCP tools only after `load_skill` → listed directly (Phase 0).
    - MCP descriptions capped at 512 and OAuth errors blanked → fencing
      and redaction (Phase 0). `developer-experience` decision 20 is
      amended accordingly.
    - A failed workspace commit discards the work → kept, retried,
      alerted (Phase 2).
11. **Effects: decide, persist, act.** The model's tool-call message is
    persisted before any call is sent. A non-idempotent call with an
    unknown outcome is never re-sent blindly (design §V.1).
12. **Files and conversation roll back together** to the last complete
    turn commit. Nothing outside the VM is rolled back or repeated
    (design §V.2).
13. **Direct egress from guest processes is outside the effect
    guarantee**, and this is documented. Host-mediated calls are inside it.
14. **The benchmark is a standing track.** Each phase adds its tasks to
    its exit criteria. A harness change that lowers pass^k is flagged.
15. **Layer rule (design §IX.1).** Vendor SDKs, vendor credentials,
    network listeners and clocks for standing work → an HSP broker. Who,
    whether, and what survives a VM → `vamd`. Thinking, files and code →
    the guest. A guest never talks to a broker directly.
16. **Waits belong to the task (design §IX.4).** The guest only asks
    (`wait_until`, `wait_for`). `vamd` holds deadlines and resumes. Brokers
    watch vendors and own standing schedules. `vamd` stores no standing
    schedules. Polling prefers an event, then a broker-side condition, then
    a timer with backoff; the model wakes only when something changed.
17. **Memory is a first-party broker** (`vam.memory`), using the existing
    `on_request` and `on_complete` hooks.
18. **In chat, the model picks the playbook (owner, 2026-10-16).** The user
    never selects a playbook in the chat composer: the agent calls
    `start_playbook` when the request matches a playbook's `when`, and the
    user confirms on the ask card (prefilled input form). The composer
    picker and `/playbook-name` are removed. Explicit starts stay only
    where there is no conversation to match: CLI `--playbook`, the API, and
    trigger grants. Mode defaults to `ask`; `auto` only for read-only
    playbooks. Supersedes the "Explicit start" row of design §IV.3a for
    chat.
19. **One standard schema form (owner, 2026-10-16).** Playbook inputs, gate
    forms and the start-playbook ask card render from the input schema
    through one shared form component with standard elements: text, long
    text, number, boolean, single select, multi-select and date, each with
    label, help text, required marker and validation errors. File and
    `choices_from` select are added by decision 21.
20. **`activation.when` is required (owner, 2026-10-16).** Publish rejects a
    playbook without it. No fallback and no backward compatibility; existing
    playbooks must be edited. A playbook may have no start inputs.
21. **Playbooks in flight (design §IV.3b, owner, 2026-10-16).** Guidance-only
    playbooks (zero steps plus `guidance`); gate defaults templated from
    earlier step outputs (no prefill tool); a playbook status block after
    every step event; the procedure rule "a gate collects user information;
    never invent values"; templates in bound-step arguments; file and date
    form elements.

22. **Brokers live in `brokers/` (owner, 2026-10-17).** First-party HSP
    brokers are their own workspace crates under `brokers/<name>`, each its
    own process talking to `vamd` only over HSP. They depend on `vam-spec` and
    shared libraries, never on `vam-orchestrator`. `crates/` holds the
    machine.

23. **A thread's conversation lives in its workspace (owner, 2026-10-19).**
    The host records each thread's conversation, as the model saw it, into an
    append-only, hash-chained, versioned log under `.vam/threads/<thread>/` in
    the workspace, from what it brokers (inference requests and responses)
    plus a small guest report at an approval suspend. The guest never uploads
    the conversation per round and never writes the host's copy. Drive9
    versions the log with the files, so files and conversation commit, roll
    back, move and fork together (decision 12). SQLite holds only an index
    (turn commits, fetch grants, legacy checkpoints), never conversation
    bytes. Every API, CLI and app view of a thread's history reads the log.
    Workspace access is conversation access (revisited before the first
    release). Supersedes Phase 2 owner decision 2 of 2026-10-18 (checkpoint
    chain in SQLite + CAS); decision 10's "flattened replay → turn
    checkpoints" reads "→ the thread log".

## Placement

Confirmed by the product owner on 2026-10-15 (E5 slice moved before E2
the same day):

```
workspace hotfix ─▶ H1 runtime hotfix ─▶ E1 P7 ─▶ E10 P0–3 ─▶ E12 P1 ─▶ E5 slice ─▶ E10 P4 (with E12 P3) ─▶ E4 slice ─▶ E8, E7 P1 ─▶ E10 P5–6 ─▶ E11 Jig P1–3 ─▶ E2 ─▶ E7 rest, E3, E5 rest, E4, E6
```

One phase at a time. E10 changes `vam-runtime` (in the guest image), so
it never overlaps E8 Phase 2 or E7 Phase 1.

## Status legend

- `[ ]` not started
- `[~]` in progress
- `[x]` done (code + tests, or merged)

## Phases


### Phase 0 — First try (U1, U8, U9)

**Goal:** a connected tool works in plain words, the first time, and a playbook starts only when the request matches it.

Released; the full plan, waves and close are in the archive (`.cursor/workstreams/archive/agent-runtime-phases-0-3.md`).

### Phase 1 — Web (U2)

**Goal:** the agent can search and read the web; the key stays on the host.

Released; the full plan, waves and close are in the archive (`.cursor/workstreams/archive/agent-runtime-phases-0-3.md`).

### Phase 2 — Context (U3, part of U5)

**Goal:** follow-ups continue exactly where the agent was.

Released; the full plan, waves and close are in the archive (`.cursor/workstreams/archive/agent-runtime-phases-0-3.md`).

### Phase 3 — Steer (U3, U4 visibility)

**Goal:** talk to the agent while it works and see what it's doing.

**Status:** **released to `main` 2026-10-29, checklist out.** Paid benchmark on all catalog models pending (owner's word; with Harness H2).

Released; the full plan, waves and close are in the archive (`.cursor/workstreams/archive/agent-runtime-phases-0-3.md`).

### Phase 4 — Never twice (U5)

**Goal:** a crash never loses finished work and never repeats an effect.
**Depends on:** Phase 3 (on `main` 2026-10-29). Before E8 and E7 Phase 1
(launch-0.1.0 §4 rows 9 and 14; build order revised 2026-10-21; owner,
2026-11-01).
**Plan:** breakdown drafted 2026-11-01 (background agent, Opus 5.5 high),
**approved by the owner 2026-11-01** with the decisions below. Evaluated (base level with `main`
: E10 Phase 3, E12 Phase 1, E5 Phases 1–2, Harness H1, IT Phase 2).
High-level only: each wave session plans its wave before building.
**Repos:** vam only (no app wave: the reconnect and "outcome unknown" cards
reuse Phase 3's question and approval cards; an app wave is backlog).
**Exit:** kill-mid-task benchmark: 0 duplicate effects, 0 lost tasks.

**Owner decisions (2026-11-01).** Breakdown approved with the orchestrator's
recommendations; all open-question defaults accepted, and:

- **R1 (required). A failed journal write blocks the effect.** If the
  `intended` or `sent` row cannot be written, the call is not sent: the guest
  gets a typed host error and nothing leaves the host (fail closed). Wave 2
  (`AR-4.4`) adds a unit test: a store that fails the write means the test
  adapter never sees the request.
- **R2. Wave 3 runs on Sonnet high** (reason in its row).
- **A. Spec change approved:** `work-survives-vamd-restart-mid-task` takes the
  proposed `then` (the first run succeeds after the restart with
  `run.resumed`; `partial.txt` equals `half`; the follow-up answer unchanged).
  Wave 0 writes it.
- **B. Provider fallback kept** as `SLICE`: one fallback behind
  `reliability.fallback`, off by default.
- **C. `needs_reauth` is proven by contract tests**; the scenario
  `reauth-parks-the-task` goes to the backlog (needs an MCP server in e2e).
- **D. "Depends on" and Placement** lines applied as proposed.

- [ ] Effect journal: decide, persist, act; classes; verify reads;
  stable idempotency keys.
- [ ] Resume after a VM crash and after a `vamd` restart.
- [ ] Provider fallback within the allowlist; `needs_reauth` →
  `waiting(input)`.

**In short.** Today a task whose VM dies, or whose `vamd` restarts, simply
ends as failed: the work it finished is kept (Phase 2 turn commits), but the
task is lost and the person has to ask again. Nothing records which outside
calls (an MCP create, a broker call, a host tool) already happened, so asking
again can do them twice; a Stop that rolls files back even hides such a call
from the conversation, so a follow-up repeats it. After this phase `vamd`
writes every outside call down **before** it sends it (a journal row: what,
with which arguments, under a stable key, and whether it was sent, confirmed,
failed or unknown). A task that crashes is picked up again on a new VM from its
last finished step, with the model's last decision kept: a call that already
happened is answered from the journal instead of being sent again; a call that
never left is sent; a call whose answer was lost is checked with a declared
read ("find the ticket by title"), or the model is told it is unknown and must
check, and an identical retry asks the person first. This holds for chat runs,
runs a schedule started and queued tasks. A provider outage can fall back to
another model the operator allows, and an MCP connection that needs signing in
again parks the task until it is reconnected. The kill-mid-task benchmark
measures it: 0 duplicate effects, 0 lost tasks, counted by the test world, not
by the journal itself.

**Code map** (facts the wave sessions build on):

- **One host door for outside calls.** Every guest host-tool call arrives as
  port-52 `ExecuteHostTool` (`brokers.rs:453`) into
  `SessionSecurityContext::handle_execute_host_tool`
  (`host_tools.rs:531-551`) → `execute_host_tool_inner` (`:553`). It branches
  to the three send points: the host adapter `adapter.invoke(&ctx)` (`:795`),
  HSP `finish_hosted_call` → `invoke_hosted_tool` (`:706-727`, `:2840`; the
  vendor send is `client.invoke(request)` at `:2979`) and MCP
  `invoke_mcp_tool` → `call_mcp_once` (`:728-778`, `:2760`; `call_tool` at
  `:2773`). Playbook `run_step` re-enters the same function for its bound tool
  (`:1204`), so a journal around `handle_execute_host_tool` covers nested calls
  too. `compute_args_digest` (`:401`) is the canonical arguments digest the
  capability token already binds.
- **No call id crosses the boundary.** `ToolIntentRequest` (lease, tool,
  arguments, digests; `vsock_rpc.proto:228-234`) and `ExecuteHostToolRequest`
  (lease, tool, arguments, token; `:244-249`) carry no model call id, and
  `CapabilityClaims` (`security.rs:1639-1649`) binds session, lease, lifecycle
  generation, tool and argument digest, expiring 30 s after the intent. The
  journal key needs the call id: an additive field on both requests and in
  the claims.
- **The intent is the decide point.** `handle_tool_intent_request`
  (`security.rs:988`) answers Authorized with a token (`:1651-1659`) or
  Suspended with the approval id (`:1662-1680`). That is where a row is written
  `intended` (with the approval id when suspended). A resumed guest re-intents
  each call with a new token, so a replay must be decided here **before** the
  PDP: a call already confirmed is answered from the journal, never re-asked
  (design §V.1 "never re-asked"; a policy change since then must not deny a call
  that already happened).
- **Retries by class exist only for MCP reads.** The MCP path retries a 5xx or
  transport error only for the read class (`host_tools.rs:2417-2420`, class from
  `mcp_tool_side_effect`, `mcp_registry.rs:1761`; unset is
  `MCP_SEEDED_SIDE_EFFECT = 3`, mutating, `:1445`). Vendor `idempotentHint` and
  `readOnlyHint` are kept but untrusted (`mcp_client.rs:83-97`); the owner
  confirms read-only claims (`mcp_registry.rs:1691`). HSP has no retry; a
  timeout is classified only for metrics (`host_tools.rs:2992`).
- **Grants are minted inside the HSP send.** `invoke_hosted_tool` mints a run
  grant after preflight and secrets (`host_tools.rs:2939-2951`) and revokes it
  on any failure through a guard disarmed only on success (`:3022`). A
  replayed `schedule_create` or `delegate_run` must therefore never reach
  `mint` again: the replay sits before it.
- **Manifests have a side-effect class, no effect class or verify read.** Skill
  `ToolSpec` fields 1–12 (`skills.proto:48-66`; 13 free), HSP `ToolExport`
  fields 1–4 (`hosted_services.proto:67-72`; 5 free), `SideEffectClass`
  `READ_ONLY | MUTABLE_LOCAL | MUTABLE_EXTERNAL` (`skills.proto:14-19`).
  `InvokeRequest` ends at `run_grant = 9` (`hosted_services.proto:208-218`; 10
  free for an idempotency key). `CallerContext.run_source` (10) already tells a
  broker a call comes from a triggered run.
- **Decide-then-persist is half there, best effort.** The host records each
  assistant message from the inference response it brokers
  (`thread_recorder.rs:1-43`), so the model's tool-call message is on the host
  before the guest acts, but "failure never fails the task" (`:39-42`): a
  recorder gap does not stop the call. The journal row is the durable record of
  the decision; the log stays the conversation.
- **A stop or restart can hide an outside call.** `record_interrupted`
  (`thread_recorder.rs:1746`) rolls the log back to the last complete turn
  commit when a file-writing tool result lies after it (`:1824-1843`), which
  hides every call after the floor, outside ones included; otherwise dangling
  calls get `STOPPED_CALL_RESULT` ("check whether it took effect before
  retrying", `:1715`). Phase 3 Q21 left journaling this to Phase 4.
- **`vamd` restart ends running tasks.** Startup: `run_startup_reconciliation`
  (`bin/vamd.rs:446-448`; `reconcile.rs:107`) adopts Firecracker processes whose
  start time matches (`reconcile.rs:143-166`), marks dead running sessions
  Failed (`:182-209`) and hands back trees still holding their lock
  (`:219-252`); process guests are all killed (`bin/vamd.rs:472-479`,
  `process_mode.rs:244`); `settle_dead_trees` (`bin/vamd.rs:535-540`,
  `session_end.rs:1045-1137`) writes `task_end{interrupted}`, rolls the
  workspace back like a stop and marks the run FAILED
  `vamd_restarted` (`session_end.rs:1122-1133`). An **adopted** VM keeps its lock
  and CID, but nothing awaits its `ExecuteTask` any more and its run is not
  touched (finding: A19 is still open). The queue starter's first pass runs
  after this (`api/mod.rs:251`), so a run left non-terminal keeps its queue
  waiting (`api/queue.rs:107-117`).
- **A guest crash ends the task.** `dispatch_run_prompt`
  (`api/mod.rs:4607-4663`) and the approval resume
  (`resume_run_after_hitl_decision`, `:4187`, Err at `:4302-4334`) turn any
  `run_task_on_guest` error into FAILED `{error}` (or `stopped` when a stop was
  recorded). Crash, timeout and transport loss are not told apart today.
- **Cold resume exists for approvals.** `boot_resume_session`
  (`api/mod.rs:4101-4167`: resume key per generation, run bound before brokers
  start, grants copied), `resume_lease_generation` (`:4060`),
  `attach_log_resume_context` (`:3378`, only for approval inputs, `:3392`) and
  the host-assembled `ThreadContext` with `SuspendResume {pending, unanswered,
  state}` (`vam-spec/src/thread_context.rs:79`, `:161-167`). Turn commits are
  looked up per session (`turn_commit.rs:692`), so a resume on a new session
  needs a per-run lookup.
- **The guest has three start modes, none for "pick up a decided round".**
  `run_react_loop` (`vam-runtime/src/agent.rs:36`): a denial resume
  (`:170-257`), an approval resume that executes the approved call
  (`:258-372`), or a follow-up from the thread context (`:374-386`). The round
  pushes the assistant message (`:685-690`), then per call: arguments check,
  intent (`:812`), suspend tail on Suspended (`:857-951`), host tool
  (`:1026-1043`) or in-VM subprocess (`:1049-1079`), then the turn commit
  (`:1170`). Calls run one at a time. Loop state (iterations, tokens, inbox
  seq) reaches the host only in a suspend tail.
- **Triggered and queued runs are rows.** `runs.source` (`catalog.rs:755`;
  `RunSource`, `api/run_start.rs:17-50`) and `credential_id` are on the run row;
  the loop guard reads `runs.source` on every Invoke (`caller_context.rs:75`).
  A queued message is a `run_inbox` row plus a PENDING run started by the queue
  starter once the run it waits behind is terminal and the workspace is free
  (`api/queue.rs:80-117`); a fire past `not_after` ends EXPIRED (`:59-65`).
  Nothing in the resume path needs the HTTP caller.
- **Model choice and retry.** `resolve_run_model` (`allowed_models.rs:435`,
  operator allowlist `:114`) picks the model once at start and records
  `model.resolved {model, source, provider, note?}` (`api/run_start.rs:501-517`,
  `events.rs:41`). The provider retries 429, 529 and 5xx three times with
  backoff (`vam-inference/src/provider.rs:94-148`); there is no fallback.
- **Harness switches exist, unbuilt.** `calls.verifyReads` (default false,
  `vam-spec/src/harness.rs:165`, `:176`), `reliability.journal` and
  `reliability.fallback` (`:299-302`) accept only their default (`NotBuilt`,
  `:81`; test `:861-893`).
- **`needs_reauth` is a tool error.** A connection already marked
  `needs_reauth` is refused before any send (`host_tools.rs:2465-2468`); a
  definitive refresh failure persists the status (`:2645-2667`); the model gets
  `mcp needs_reauth: … reconnect it to resume tool calls` (`:1475-1480`).
- **The receipt says "not journaled".** `EFFECTS_STATEMENT` (`receipt.rs:22`)
  is written unconditionally as `{journaled: false, statement}` (`:903-906`);
  `ReceiptEffects {journaled, statement}` (`receipt.proto:162-166`). E12
  Phase 3 owns this field.
- **Keys and secrets.** `signing.rs` derives per-session keys from the node
  master with `hmac_sha256` (`:140-170`): the same root serves a host-only
  effect-key secret.
- **The runner.** `restart-vamd` is kill plus respawn of vamd
  (`vam-e2e/src/vamd.rs:182-187`, `engine.rs:1422`); `stop-session` is a
  terminate (`engine.rs:1386`), a stop, not a crash. There is no step that
  kills a guest. Process guests record `<state_root>/<vm id>/pid`
  (`vam-core/src/process_vmm.rs:15`, `:93-100`). New steps register in
  `scenario.rs` (`Step` `:164-167`, keys `:237-238`), `engine.rs` (dispatch
  `:2027-2028`) and `check.rs`; event names in `KNOWN_EVENTS`
  (`vam-e2e/src/events.rs:7-37`). `scripts/e2e/run.sh` exports the test broker
  `VAM_E2E_TEST_BROKER` only for process mode (`:66-67`); the `--firecracker`
  build path does not (`:39-50`), so a real-model bench task that uses a broker
  piece needs that export. Bench rows have `budgetTimeouts` but no duplicate or
  lost counts (`vam-e2e/src/bench.rs:323`). The test broker is
  `hsp-echo-broker` (`vam-orchestrator/src/hsp_echo.rs`, bin
  `src/bin/hsp-echo-broker.rs`). The mock model matches on step labels and
  messages and cannot fail a request or tell models apart
  (`vam-inference/src/mock_script.rs:40-57`).
- **Hooks are outside the tool path.** `on_paused` and `on_complete` HSP hooks
  (`hsp_hooks.rs`) are host-initiated calls, not model decisions; they are not
  journaled by this plan (Q17).

**Scenario delta**

| Scenario | Change | Layer | Given | Why |
|---|---|---|---|---|
| effect-is-not-repeated-after-the-guest-crashes | new | process | org-admin, agent-with-effect-tools, effects-test-broker | U5: the guest dies while a create is in flight; the task resumes on a new guest, the recorded result is replayed, the world counts one create |
| unknown-effect-is-reconciled-by-a-verify-read | new | process | org-admin, agent-with-effect-tools, effects-test-broker | §V.1 rule 2: vamd restarts while a create is sent and unanswered; the outcome is unknown; the declared verify read finds it, so nothing is re-sent |
| unknown-effect-without-a-verify-read-is-left-to-the-model | new | process | org-admin, agent-with-effect-tools, effects-test-broker | §V.1 rule 2 for a tool with no verify (most MCP tools): the model is told the outcome is unknown; its identical retry asks the person |
| follow-up-after-a-stop-sees-the-external-effect | new | process | org-admin, agent-with-effect-tools, effects-test-broker | §V.3 "stop during a host call": the call finishes and is journaled; the rollback of files never hides it from the conversation |
| triggered-run-resumes-like-a-chat-run | new | process | org-admin, agent-with-trigger-tool, trigger-test-broker | a run a trigger started (`runs.source`) resumes after a vamd restart; the message queued behind it starts once after it |
| provider-fallback-stays-in-the-allowlist | new | process | org-admin, agent-with-builtins, two allowlisted mock models | §V.4: the run's model fails after retries; the task continues on another allowlisted model, never an unlisted one |
| work-survives-vamd-restart-mid-task | **changed (spec change for the owner, below)** | process | org-admin, agent-with-shell | "lost task" becomes "resumed task": the interrupted task now finishes |

Seven scenarios (six new, one changed); `task-resumes-after-vamd-restart`
from the starting set is merged into the changed
`work-survives-vamd-restart-mid-task`. **`reauth-parks-the-task` is not a
process scenario this phase:** e2e has no MCP server, and `needs_reauth` exists
only on the MCP OAuth path; an MCP-and-OAuth test server world piece is a new
binary for one behaviour. It is proven by contract tests in `vam-orchestrator`
(item AR-4.9) and the process scenario is backlog (trigger: "an MCP test
server world piece lands", e.g. with E5's "connect your services").

**World piece (wave 0).** Duplicates are counted by the world, never by the
journal under test:

- `effects-test-broker` (kind `broker`, the existing `hsp-echo-broker` binary
  started in a new `--effects --state-dir {{dir}}` mode, its code in its own
  module next to `hsp_echo.rs`): tools `echo_create_ticket {title, holdMs?}`
  (`MUTABLE_EXTERNAL`, non-idempotent, declares the verify read below),
  `echo_create_note {title, holdMs?}` (`MUTABLE_EXTERNAL`, **no** verify) and
  `echo_find_ticket {title}` (`READ_ONLY`). A create appends one line to
  `<dir>/ledger.jsonl` (tool, title, the `InvokeRequest` idempotency key once it
  exists, time) **before** it sleeps `holdMs` and answers `{id: "T-<n>"}`, so a
  kill during the hold leaves an effect that happened with no answer. A
  subcommand `hsp-echo-broker ledger --state-dir <dir>` prints counts per tool
  and title. The broker is its own process, so it survives a guest kill and a
  vamd restart. Until AR-4.3 adds `ToolExport.effect`, the verify declaration
  is in the piece as the field the contract will read (wave 0 writes it in the
  shape of Q2; vamd ignores unknown manifest fields today, else the piece adds
  it in wave 1).
- `agent-with-effect-tools`: an agent allowed the three tools (and
  `write_file`, `read_file`), `default: deny` otherwise.
- `provider-fallback-stays-in-the-allowlist` needs two mock model ids on the
  operator allowlist (a node-settings step or piece; wave 0 picks the cheaper of
  the existing `agent-prefers-an-unlisted-model` pattern or a new
  `operator-allows-two-mock-models` piece) and mock faults (AR-4.8).

**New runner steps** (own module `crates/vam-e2e/src/effects.rs`; shared runner
files get registration lines only, as `steer.rs` did; the proof lane adds bench
modules there): `kill-guest {}` (SIGKILL the latest run's guest: the process
guest from its pid file in process mode, the session's Firecracker process on
the microVM lane; waits until vamd reports the session dead or the run moving;
never a stop), `expect: {ledger: {piece, tool, title?, count}}` (reads the
broker's ledger through the subcommand; a mismatch is `product`). Event names
in `KNOWN_EVENTS` with constants and no emitter in
`vam-orchestrator/src/events.rs` (as Phase 3 did): `effect.recorded`,
`effect.replayed`, `effect.reconciled`, `run.resumed`, `model.fallback`.

What each scenario does, and where it is red today (all class `product`):

- **effect-is-not-repeated-after-the-guest-crashes.** `chat {wait: false}`
  `e2e-step:make`: the mock calls `echo_create_ticket {title: Q4 plan, holdMs:
  4000}`. `expect {ledger: {tool: echo_create_ticket, count: 1}}` (the broker
  did it), `kill-guest {}`. vamd is alive, so the broker's answer arrives and
  the row ends `confirmed`. Mock `{step: make, toolName: echo_create_ticket}` →
  text `Created T-1`; `{step: make, stepStart: true}` → the create (a re-asked
  decision would create again). `then`: `run: succeeded` (the same run),
  `ledger count: 1`, event `run.resumed` (`data.reason: guest_crashed`), event
  `effect.replayed {data.tool: echo_create_ticket}`, `thread {role: tool,
  contains: T-1}`. **Red at `then.0`**: the run ends FAILED on the guest error
  (`api/mod.rs:4648-4661`).
- **unknown-effect-is-reconciled-by-a-verify-read.** As above with `holdMs:
  6000`, but `restart-vamd {}` during the hold: the row was `sent` when vamd
  died, so it is `unknown` at startup. `then`: `run: succeeded`, `ledger
  {tool: echo_create_ticket, count: 1}`, `ledger {tool: echo_find_ticket,
  count: 1}` (the verify ran), event `effect.reconciled {data.by: verify,
  data.outcome: confirmed}`, `thread {role: tool, contains: "confirmed by a
  check"}`. **Red at `then.0`**: FAILED `vamd_restarted`
  (`session_end.rs:1122-1133`).
- **unknown-effect-without-a-verify-read-is-left-to-the-model.** The same
  restart during `echo_create_note`. Mock `{step: make, toolName:
  echo_create_note, contains: "Outcome unknown"}` → the same call again (a blind
  retry). `expect {approval: pending, tool: echo_create_note, fields:
  {approvalMessage: {contains: "may already have taken effect"}}}`, `deny:
  latest`; mock after the denial → text `Left it`. `then`: `run: succeeded`,
  `ledger {tool: echo_create_note, count: 1}`, `thread {role: tool, contains:
  "Outcome unknown"}`. **Red at `when`'s first `expect` after the restart** (no
  resume; FAILED `vamd_restarted`).
- **follow-up-after-a-stop-sees-the-external-effect.** Round 1 `write_file
  a.txt` (turn commit), round 2 `write_file b.txt` then `echo_create_ticket
  {holdMs: 4000}`; `expect {ledger count: 1}`, `stop {}`. Then `chat {thread:
  continue}` `e2e-step:next`: mock `{step: next, anyContains: "T-1"}` → text
  `T-1 already exists`; `{step: next}` → `echo_create_ticket` (what a model
  that cannot see the create does). `then`: `ledger count: 1`, `b.txt` absent
  (rolled back), `a.txt` present, the follow-up's reply. **Red at the ledger
  `then`**: today the rollback hides the call (`thread_recorder.rs:1824-1843`),
  the follow-up creates again, the ledger counts 2.
- **triggered-run-resumes-like-a-chat-run.** The trigger flow of
  `trigger-starts-a-run-as-the-grant-owner` (arm, approve, `fire-trigger
  {label: slow}`); the fired run writes `a.txt` then runs `echo half >
  partial.txt; sleep 4`; `expect {event: tool.invoked, tool: run}`; `send {mode:
  queue, text: "e2e-step:after Write b.txt"}`; `restart-vamd {}`. `then`: the
  fired run `succeeded` with `runs.latest.fields.source.kind: trigger` kept,
  event `run.resumed (vamd_restarted)`, `expect {runs: {count}}` shows no second
  fired run, the queued run `succeeded` and `b.txt` present
  (`agent-with-trigger-tool` already allows `run`, `read_file`, `write_file`).
  **Red at `then.0`**: FAILED `vamd_restarted`; the queued run then
  starts behind a failed task instead of a finished one.
- **provider-fallback-stays-in-the-allowlist.** Allowlist `[mock-a, mock-b]`,
  the run on `mock-a` with `harness: {reliability: {fallback: true}}`; mock
  `{model: mock-a}` → `fail: {status: 503}` (every attempt), `{model: mock-b,
  step: work}` → `write_file`, then text. `then`: `run: succeeded`, event
  `model.fallback {data.from: mock-a, data.to: mock-b}`, event
  `inference.completed` for `mock-b` only, no request on a model off the
  allowlist. **Red at `when.0`**: the run is refused at create, `harness switch
  'reliability.fallback' is not built` (`harness.rs:81`).
- **work-survives-vamd-restart-mid-task (changed).** Below.

**Existing scenarios.** Checked by `guards`, `given` and the code map. Stay
green, re-run because their guards are touched: `parked-approval-survives-a-restart`
and `stop-ends-a-run-parked-across-a-restart` (a parked run is not running at
restart: no resume), `follow-up-continues-after-vamd-restart` (the first task
ended before the restart), `workspace-matches-conversation-after-vm-stop`
(`needs: firecracker`; `stop-session` is a user terminate, a stop, never a
crash: no resume; on the microVM lane in wave 3), `missed-fire-runs-once-after-the-broker-restarts`
(guards `host_tools.rs`; the approved `schedule_create` is journaled `intended`
with its approval id, then sent), `approval-suspends-and-resumes`,
`long-thread-resumes-after-approval`, `stop-ends-a-running-task-cleanly`,
`stop-denies-a-pending-approval`, `queued-message-runs-after-the-task`,
`trigger-on-a-busy-workspace-runs-after-the-task`,
`receipt-names-who-decided-and-from-where`. **`receipt-states-effects-not-journaled-before-journal`
does not change in this phase**: the receipt field is E12's (`receipt.rs:903-906`
is not edited here); E12 Phase 3 retires the scenario (`design/run-receipt.md`
§4). Q16 names the window in between.

**Spec change for the owner (one).** `work-survives-vamd-restart-mid-task`:

- **Old `when`/`then`:** chat `e2e-step:first` (write `note.txt`, read it, then
  `run "echo half > partial.txt; sleep 30"`), `restart-vamd`, then a follow-up
  `e2e-step:next`. `then`: the follow-up answers "The note says kiwi-42.",
  `note.txt` equals `kiwi-42`, `partial.txt` absent. Today the first task is
  lost (FAILED `vamd_restarted`) and only its files survive.
- **Proposed:** title "When vamd restarts in the middle of a task, the task
  resumes from its last finished step and finishes". `when`: the same up to
  `restart-vamd`, the slow command shortened to `sleep 4`, then `expect: {run:
  succeeded}` on the first run (it resumed) before the follow-up; mock gains
  `{step: first, toolName: run}` → text `Done.`. `then`: event `run.resumed
  (vamd_restarted)` on the first run; `note.txt` equals `kiwi-42`;
  **`partial.txt` equals `half`** (the interrupted in-VM step ran again on the
  files rolled back to the last finished step: a turn is at least once, §V.1);
  the follow-up answers "The note says kiwi-42.". Guards add
  `vam:crates/vam-orchestrator/src/resume.rs` and `api/mod.rs`. The rollback
  itself stays proven by `workspace-matches-conversation-after-vm-stop` and the
  stop scenarios. Wave 0 lands this as `status: pending` with `expectFail` at
  the new `expect: {run: succeeded}` once the owner approves; if the owner
  keeps the old `then`, the scenario is left as is and wave 3 must keep a
  restarted task **lost** when a follow-up is already waiting (not proposed).

**Benchmark delta** (playbook §11)

- **Lines moved:** `design/benchmark.md` §1 P13 (l.101) "kill-mid-task
  duplicates and lost tasks (U5)", both must be 0 (§3 l.136; benchmark decision
  6: a non-zero row is published red). §3 says the source is "the receipt's
  effects section"; until E12 Phase 3 the receipt has no such section, so the
  scorer reads the **world** (the test broker's ledger: creates beyond the one
  asked for are duplicates) and the run state (lost tasks, Q9); the receipt
  count becomes a cross-check when E12 Phase 3 lands (a mismatch is a finding).
- **Task added (wave 6):** `u5-kill-mid-task`, as the scenario
  `task-killed-mid-effect-finishes-once` with a `model` block: "Create one
  ticket titled Q4 plan with the ticket tool, then tell me its id"; `kill-guest`
  during the create's hold, the task resumes; `then`: `run: succeeded`, ledger 1,
  the answer names the id. Task set **0.3.0** = 0.2.0 + `u5-kill-mid-task`
  (`e2e/task-sets.yaml`, additive; the next free version if another lane takes
  0.3.0 first). Bench rows gain `duplicateEffects` and `lostTasks` (sums over
  k, from the ledger and the run state). It runs `needs: process`, active, in
  smoke on the mock.
- **Baseline on the current base:** **lost tasks k of k by construction**: a
  guest crash ends the task FAILED (`api/mod.rs:4648-4661`), a vamd restart
  FAILED `vamd_restarted` (`session_end.rs:1122-1133`). **Duplicate effects 0
  by construction and vacuous**: nothing resumes or re-sends, so nothing can
  duplicate inside the task; today's duplicate comes from the person asking
  again, which the task does not model. Effects are "not journaled": the
  guarantee is unmeasurable on this base. Not run (the mock would print the
  same numbers by construction).
- **Real-model row (owner's word only, after the mock row is green):** a
  bench.yml dispatch, a free dry run first:
  `gh workflow run bench.yml --ref feat/agent-runtime -f mock=true -f k=1 -f task_set=0.3.0 -f scenarios=task-killed-mid-effect-finishes-once`,
  then
  `gh workflow run bench.yml --ref feat/agent-runtime -f task_set=0.3.0 -f models=<the two benchmarked Claude models' gateway ids, as listed in e2e/README.md "Bench on GitHub Actions"> -f profiles=base -f k=5 -f max_usd_per_job=0.5 -f scenarios=task-killed-mid-effect-finishes-once`.
  Cost: 10 trials, each about 4 to 8 short model requests plus one
  resume; the per-job cap (`max_usd_per_job`) bounds it. Two microVM jobs run
  one at a time, mostly the build.

**Items**

- [x] `AR-4.1` — **Runner steps, world piece, event names (wave 0).** The
      `effects-test-broker` mode and its ledger subcommand (own module beside
      `hsp_echo.rs`, one flag in `src/bin/hsp-echo-broker.rs`); world pieces
      `effects-test-broker`, `agent-with-effect-tools` (and the two-model
      allowlist piece if needed); `kill-guest` and `expect: {ledger}` in
      `crates/vam-e2e/src/effects.rs` with registration only in
      `scenario.rs`, `engine.rs`, `check.rs`; `KNOWN_EVENTS` and the
      orchestrator constants above; `e2e/README.md` section. Files:
      `vam-orchestrator/src/{hsp_echo_effects,events}.rs`,
      `vam-orchestrator/src/bin/hsp-echo-broker.rs`,
      `vam-e2e/src/{effects,scenario,engine,check,events}.rs`,
      `vam-e2e/tests/engine_selftests.rs`, `e2e/world/`, `e2e/README.md`.
      **Accept:** broker unit tests (a create is in the ledger before the hold
      ends; a kill during the hold leaves the line); engine self-tests for
      `kill-guest` (process pid file) and `ledger` against a stub; existing
      scenarios green. **Test:** `cargo test -p vam-e2e`, `cargo test -p
      vam-orchestrator --lib hsp_echo`, `cargo run -p vam-e2e -- check`,
      `scripts/ci/local.sh --fast`.
      `Decide: NOW — proof 3 (the must-be-zero U5 lines need a world that counts), cost 1`
- [x] `AR-4.2` — **Scenario files (wave 0).** The six new scenarios `pending`
      with `expectFail` at the steps above; the approved spec change to
      `work-survives-vamd-restart-mid-task` (status `pending`, `expectFail` at
      the new `expect: {run: succeeded}`). **Accept:** `vam-e2e check` clean;
      `scripts/e2e/run.sh --scenarios
      effect-is-not-repeated-after-the-guest-crashes,unknown-effect-is-reconciled-by-a-verify-read,unknown-effect-without-a-verify-read-is-left-to-the-model,follow-up-after-a-stop-sees-the-external-effect,triggered-run-resumes-like-a-chat-run,provider-fallback-stays-in-the-allowlist,work-survives-vamd-restart-mid-task`
      reports each red at its `expectFail` step, class `product`. **Turns
      green:** none (red, right reason).
      `Decide: NOW — proof 3, cost 1, line U5 duplicates and lost tasks`
- [x] `AR-4.3` — **Contracts scaffold.** All additive. (a) `vsock_rpc.proto`:
      `ToolIntentRequest.call_id = 6`, `ExecuteHostToolRequest.call_id = 5`;
      `CapabilityClaims` gains `call_id` (a token is good for one call). (b)
      `skills.proto`: `EffectClass {UNSPECIFIED, READ, IDEMPOTENT,
      NON_IDEMPOTENT}` and `EffectSpec {class, verify {tool, args_template,
      found}}` (Q2), `ToolSpec.effect = 13`; `hosted_services.proto`:
      `ToolExport.effect = 5`, `InvokeRequest.idempotency_key = 10`; the MCP
      connection's owner-set per-tool `effects` map (JSON, beside `ask_tools`).
      (c) `workflow.proto`: `RunStatus.resumes` (count) and the `waiting_for`
      value `reauth` (Q8). (d) SQLite `effect_journal` (run id, call id, parent
      call id, tool, adapter, class, state `intended|sent|confirmed|failed|unknown`,
      idempotency key, args digest, approval id, attempts, generation sent,
      result digest, result bytes until recorded, reconciled by, timestamps)
      through a migration in its own module `effect_store.rs` (one call from
      store init), with the read `effects_for_run(run) -> {rows, counts}` E12
      Phase 3 will call. (e) `effect_key.rs`: the key of Q1, unit-tested stable
      across generations and sessions. (f) `effect_class.rs`: class from the
      manifest, else inferred from `side_effect` (READ_ONLY → read; anything
      else → non-idempotent), never from a vendor hint. (g) Harness:
      `reliability.journal` handled per Q13. Files:
      `proto/vam/v1/{vsock_rpc,skills,hosted_services,workflow}.proto`, the
      proto baseline descriptor, `vam-spec` (encoding, `harness.rs`),
      `vam-orchestrator/src/{effect_store,effect_key,effect_class}.rs`,
      `docs/api/openapi.yaml` (additive stubs; not a docs page). **Accept:**
      round-trip tests per field; the baseline conformance check passes
      (additive only); migration on an existing database; key derivation
      vectors (same inputs, new session and generation → same key; one argument
      byte changed → another key). **Test:** `cargo test -p vam-spec`, `cargo
      test -p vam-orchestrator --lib effect_`, `scripts/ci/local.sh --fast`.
      `Decide: SLICE — proof 3, cost 2 (new proto fields and a table) → additive fields only, no new run state; its cost is the scaffold of AR-4.4–AR-4.9`
- [x] `AR-4.4` — **The journal at the host tool path (decide, persist, act).**
      (a) Intent (`security.rs:988`): with a call id, write `intended` before
      the token is minted; on Suspended, `intended` with the approval id; a
      denial at decide marks it `failed (denied)` (`hitl.rs` decide path).
      (b) `handle_execute_host_tool`: look up the row by (run, call id); check
      the token's call id and args digest; write `sent` (with the key and the
      lifecycle generation) immediately before each of the three send points
      (`adapter.invoke`, HSP `client.invoke` with the grant mint after the row,
      MCP `call_tool`); after the answer, `confirmed` with the final redacted
      response (bytes kept until the log records the tool message, Q4),
      `failed` with the typed error, or `unknown` on a timeout or a transport
      loss after the request left. MCP's read-only 5xx retry stays (locked
      decision 11). (c) The key goes to vendors (Q18). (d) **Stop during a
      host call** (V.3): the call finishes and is journaled; the run stops
      after. (e) **Rollback never hides an effect:** `record_interrupted`
      appends, after its `rollback` record, each journaled host effect after
      the floor as the call and its recorded outcome (Q6). (f) Guest:
      `vam-runtime` sends the model's call id on intent and execute
      (`pep.rs:42`, `:791`; `agent.rs:812`, `:1029`, and the approval resume
      `:2375`, `:2422` with the pending call's id); a nested `run_step` call
      uses `<call id>/<step id>`. (g) Events `effect.recorded` (non-read
      classes). Files: `host_tools.rs` (wraps the send points; the IT
      redaction code is not changed), `security.rs`, `hitl.rs`,
      `thread_recorder.rs`, `effect_store.rs`, `events.rs`,
      `vam-runtime/src/{pep,agent}.rs`. **Accept:** unit tests: the row is
      `sent` before the adapter sees the request (a test adapter asserts the
      row); a timeout leaves `unknown`; a denial leaves `failed`; an approval
      leaves `intended` with its id and then `confirmed`; the grant is minted
      only after `sent`; a stop during a held HSP call leaves `confirmed` and a
      rolled-back log that still holds the call and its result; a guest
      without call ids (old image) is journaled under a host-derived id and
      flagged. **Turns green:** follow-up-after-a-stop-sees-the-external-effect;
      `stop-ends-a-running-task-cleanly`, `missed-fire-runs-once-after-the-broker-restarts`,
      `approval-suspends-and-resumes` stay green. **Test:** `cargo test -p
      vam-orchestrator --lib` (filters `effect`, `host_tools`, `security`,
      `thread_recorder`), `cargo test -p vam-runtime`, `scripts/ci/local.sh
      --scenarios follow-up-after-a-stop-sees-the-external-effect,stop-ends-a-running-task-cleanly,missed-fire-runs-once-after-the-broker-restarts,approval-suspends-and-resumes`.
      `Decide: NOW — locked decision 11 (not scored); line U5 duplicate effects`
- [x] `AR-4.5` — **Resume, host side.** New `resume.rs`. (a) Crash detection:
      a guest error with no stop recorded, no budget timeout and no reported
      outcome is a crash (`dispatch_run_prompt` and the approval resume Err
      paths call one function instead of FAILED). (b) vamd restart:
      `settle_dead_trees` keeps the rollback but marks a running run
      `resume_pending` instead of FAILED `vamd_restarted`; an adopted VM whose
      run was running is stopped and treated the same (Q11). (c) A resume pass
      (spawned once with the app state, after the manager is up; also woken by
      a crash): for each `resume_pending` run, a new session through
      `boot_resume_session` at the next generation, files at the last complete
      turn commit of the run (per-run lookup), and a host-assembled context
      that ends with the last recorded assistant message (the decided round)
      plus the host-derived loop state (rounds from turn commits, spend from
      metering, inbox seq from the last ack). (d) **Replay before the PDP** at
      intent: a known call id with a `confirmed` or `failed` row answers
      `REPLAYED` with the recorded result and no token (event
      `effect.replayed`); `intended` is decided as a new call (with its
      approval id when it has one); `sent` with vamd alive waits for its
      outcome; `unknown` answers with the model message of Q3 in this wave
      (verify and the person in AR-4.7). (e) Attempts cap and reasons (Q10),
      `RunStatus.resumes`, event `run.resumed {generation, reason, fromRound}`.
      (f) Triggered and queued runs (Q12): `runs.source` and the loop guard
      unchanged; a `resume_pending` run is busy for the queue starter and
      `workspace_busy`; steers stay pending and are pulled after the resume.
      Files: `resume.rs`, `session_end.rs`, `reconcile.rs`, `manager.rs`,
      `api/mod.rs` (the two Err paths and one spawn), `api/queue.rs` or
      `api/inbox.rs` (one busy condition), `security.rs` (the replay branch),
      `thread_context.rs`, `turn_commit.rs` (per-run lookup), `bin/vamd.rs`
      (order only), `vam-spec/src/thread_context.rs` (an additive
      `CrashResume` beside `SuspendResume`). **Accept:** tests: a crash resumes
      once and a third crash ends `crashed_repeatedly` with files kept; a stop
      racing a crash ends `stopped`, never resumed; a budget timeout is not
      resumed; a restart resumes a running run and leaves a parked one parked;
      the queue starter never starts a message behind a `resume_pending` run;
      a replayed call never reaches the PDP or `grants::mint`; a triggered run
      keeps its source and grant facts.
      `Decide: SLICE — proof 3 (U5 lost tasks), cost 3 (a guest path, VM lifecycle) → resume from the last complete turn commit on a new VM, bounded attempts; re-attaching an adopted VM in place and snapshot-warm resume backlog, trigger "B2 shows resume time matters"`
- [x] `AR-4.6` — **Resume, guest side.** A fourth start mode in
      `run_react_loop`: with `crash_resume` in the inputs, restore the context
      (dangling assistant message included), then run that message's calls in
      order through the normal per-call path (in-VM calls run again on the
      restored files; host calls get `REPLAYED`, a send, or the unknown text)
      before the next model request; the model is not asked again for that
      round. Loop state comes from the host. Files: `vam-runtime/src/{agent,pep,hitl_checkpoint}.rs`,
      `context_window.rs`. **Accept:** unit tests against the socket host:
      a resumed round with `[write_file, echo_create_ticket]` re-runs the write
      and replays the create, then makes one model request; no model request
      before the round's calls; a resume with no dangling round starts with the
      next model request. **Turns green (with AR-4.5):**
      effect-is-not-repeated-after-the-guest-crashes,
      work-survives-vamd-restart-mid-task (changed),
      triggered-run-resumes-like-a-chat-run;
      `workspace-matches-conversation-after-vm-stop` (lane),
      `parked-approval-survives-a-restart`,
      `stop-ends-a-run-parked-across-a-restart`,
      `follow-up-continues-after-vamd-restart`,
      `queued-message-runs-after-the-task` stay green. **Test:** `cargo test
      -p vam-runtime`, `cargo test -p vam-orchestrator --lib` (filters
      `resume`, `session_end`, `reconcile`, `queue`), `--test api_tests`,
      `scripts/ci/local.sh --scenarios
      effect-is-not-repeated-after-the-guest-crashes,work-survives-vamd-restart-mid-task,triggered-run-resumes-like-a-chat-run,parked-approval-survives-a-restart,stop-ends-a-run-parked-across-a-restart,follow-up-continues-after-vamd-restart,queued-message-runs-after-the-task`;
      the microVM lane on the PR (`workspace-matches-conversation-after-vm-stop`,
      plus `effect-is-not-repeated-after-the-guest-crashes` with
      `kill-guest` on Firecracker if wave 0 marks a copy `needs: firecracker`).
      `Decide: SLICE — with AR-4.5; one start mode, no new RPC`
- [x] `AR-4.7` — **Reconcile `unknown`.** On resume, an `unknown` row of class
      read is re-sent; idempotent is re-sent with the same key; non-idempotent
      runs the declared verify read (host-initiated, through the PEP as the
      agent with its own `policy.decided`; the verify tool must be a read-class
      tool of the same skill, broker or connection): found → `confirmed`
      with the verify result in the Q3 wording; not found → the model is told
      "a check found no sign it took effect" (no automatic re-send); no verify
      → the Q3 text. **Ask the person:** a later call with the same tool and
      args digest as a row still `unknown` (or `confirmed` by verify) asks
      before it is sent, as an approval whose message says "an identical call
      may already have taken effect" (the Phase 2/3 park and decide path).
      Event `effect.reconciled {outcome, by: verify|model|person}`. Carried
      here (BACKLOG items in the same files): `calls.verifyReads` built,
      default off (Q14). Files: `host_tools.rs`, `security.rs`, `resume.rs`,
      `effect_store.rs`, `vam-spec/src/harness.rs`. **Accept:** tests: an
      unknown non-idempotent call with a verify that finds it is never re-sent;
      a verify that is not read-class is refused at publish or registration;
      an identical re-send after `unknown` parks on an approval; reads and
      idempotent calls re-send with the same key. **Turns green:**
      unknown-effect-is-reconciled-by-a-verify-read,
      unknown-effect-without-a-verify-read-is-left-to-the-model. **Test:**
      `cargo test -p vam-orchestrator --lib` (filters `effect`, `resume`,
      `security`), `scripts/ci/local.sh --scenarios
      unknown-effect-is-reconciled-by-a-verify-read,unknown-effect-without-a-verify-read-is-left-to-the-model,effect-is-not-repeated-after-the-guest-crashes`.
      `Decide: NOW — proof 3 (duplicates must be 0 when an answer is lost), cost 1 on AR-4.3's declaration`
- [x] `AR-4.8` — **Provider fallback within the allowlist.** In the
      inference broker plane: after the provider's own retries fail with 429,
      529, 5xx or a transport error before the first streamed byte, the next
      model in the operator allowlist order that has a credential and that the
      agent does not exclude (Q7); one switch per task, sticky; never on a 4xx
      or for a caller-pinned model; behind `reliability.fallback` (default
      off, Q7); event `model.fallback {from, to, reason}` shaped like
      `model.resolved`; the receipt's model line is E12's (the event is the
      evidence). Mock: rules may match `model`, replies may be `fail: {status}`
      (mock only). Files: `vam-inference/src/{broker,provider,mock_script}.rs`,
      the orchestrator's broker wiring and `allowed_models.rs` (read only),
      `vam-spec/src/harness.rs`, `events.rs`. **Accept:** tests: a 503 on every
      attempt moves to the next allowlisted model once; a 400 does not; a model
      off the allowlist is never chosen; off by default; mock fault rules.
      **Turns green:** provider-fallback-stays-in-the-allowlist. **Test:**
      `cargo test -p vam-inference`, `cargo test -p vam-orchestrator --lib`
      (filter `fallback`), `scripts/ci/local.sh --scenarios
      provider-fallback-stays-in-the-allowlist,plain-chat-gets-a-reply`.
      `Decide: SLICE — proof 1 by the rubric alone (no benchmark line for outages), cost 2, which reads CUT; kept as SLICE because launch-0.1.0 §4 row 9 names it → one fallback, behind the switch; escalation and scoring stay H3 (Q7 asks the owner to keep or cut)`
- [x] `AR-4.9` — **`needs_reauth` parks the task.** An MCP call refused
      `needs_reauth` (before the send, or a definitive refresh failure) is
      journaled `intended`, never sent; the run parks PAUSED with
      `waiting_for: reauth` and the connection id (event
      `connection.needs_reauth`, an approval-like park the app shows with the
      Phase 3 question card text "Reconnect <connection> to continue"); a
      reconnect (the connection back to connected) cold-resumes every run
      parked on it and sends the intended call; Stop ends it like an approval.
      Files: `host_tools.rs`, `api/mcp_connections.rs` (one call on
      reconnect), `resume.rs`, `hitl.rs`, `workflow.rs`. **Accept:** contract
      tests with the MCP connector seam (`host_tools.rs` `McpConnector`): park,
      reconnect, resume, the call sent once; stop while parked. **Turns
      green:** none at the process layer (Q8).
      `Decide: BACKLOG — proof 1 (no line), cost 1; carried by wave 4, which already edits host_tools.rs and resume.rs (launch rubric)`
- [x] `AR-4.10` — **Bench task and rows.** `u5-kill-mid-task` as
      `task-killed-mid-effect-finishes-once` (`model` block, task set 0.3.0);
      bench rows `duplicateEffects` and `lostTasks` (from the ledger and the
      run state, Q9; the proof lane's `bench.rs`, additive fields);
      `scripts/e2e/run.sh --firecracker` also builds and exports the test
      broker; `e2e/README.md` bench lines. Files: `e2e/scenarios/` (one),
      `e2e/task-sets.yaml`, `vam-e2e/src/bench.rs`, `scripts/e2e/run.sh`,
      `e2e/README.md`. **Accept:** `vam-e2e check`; the bench scenario green in
      smoke on the mock; a `--bench --mock` run prints both counts as 0;
      bench unit test: a ledger of 2 makes `duplicateEffects: 1` and the row
      red. **Test:** `cargo test -p vam-e2e`, `scripts/ci/local.sh --scenarios
      task-killed-mid-effect-finishes-once`.
      `Decide: NOW — playbook §11 (a moved line needs its task), cost 1`
- [ ] `AR-4.11` — **Docs (HOLD until IT Phase 3 lands; `docs/` is the truth
      lane's).** A runtime page on effects (classes, keys, states, the
      guarantees table of §V.1, verify declarations, what is outside: direct
      egress, locked decision 13; hooks), resume after a crash, fallback,
      reauth; event catalog rows; `authoring/skill-manifest.md` `effect`;
      sidebar. **Accept:** `cd docs && pnpm build`. Files: `docs/src/content/docs/{runtime,telemetry,authoring}/…`,
      `docs/astro.config.mjs`.
      `Decide: NOW — proof 2 (team checklist, honest page), cost 0; held for the truth lane`

**Waves** (model: Sonnet low by default, medium second, high rarely with a reason; one wave at a time; each wave takes `main` into its branch first; branches `ar-4/wave-N`)

| Wave | Items | Turns green | Repo | Model | Budget | Depends on | Shared | actions | df | Status | PR |
|---|---|---|---|---|---|---|---|---|---|---|---|
| 0 | `AR-4.1`, `AR-4.2` (scenarios) | — (red, right reason) | vam | Sonnet low | 75 min | — | vam-e2e `scenario.rs`, `engine.rs`, `check.rs`, `events.rs`: registration lines only (the proof lane adds bench modules there); `hsp-echo-broker` bin: one flag | fast (`skip-vm-lane`: vam-e2e, vam-orchestrator, `e2e/` paths) | 30 GB → 22 GB | [x] local | local (signing down; PR when re-signed) |
| 1 | `AR-4.3` | — | vam | Sonnet medium — new proto fields, a migration and the key derivation every later wave depends on | 90 min | 0 | protos (additive; the primary-agent lane adds fields later), `vam-spec` `harness.rs` (Harness lane: take `main` first) | fast | 22 GB → 22 GB | [x] local | local (signing down) |
| 2 | `AR-4.4` | follow-up-after-a-stop-sees-the-external-effect | vam | Sonnet medium — the write-ahead journal at three send points across `vam-orchestrator` and `vam-runtime` | 120 min | 1 | `host_tools.rs` and the PEP `security.rs` (this lane's; IT's redaction there is finished and not touched); `thread_recorder.rs`; `hitl.rs` | fast (`skip-vm-lane`); **guest image changes** (`vam-runtime`) | 22 GB → 21 GB | [x] local | local (signing down) |
| 3 | `AR-4.5`, `AR-4.6` | effect-is-not-repeated-after-the-guest-crashes, work-survives-vamd-restart-mid-task (changed), triggered-run-resumes-like-a-chat-run | vam | **Sonnet high** — reconciliation and concurrency (the resume pass against the queue starter, Stop, the finalizer and the startup settle), telling a crash from a stop or a timeout, and a PEP branch that answers recorded results without a new decision (the security boundary) | 150 min | 2 | `api/mod.rs` (two Err paths, one spawn), `session_end.rs`, `reconcile.rs`, `manager.rs` (no other lane), `api/queue.rs`/`api/inbox.rs` (one busy condition; the primary-agent lane touches the inbox later), `security.rs`, `bin/vamd.rs` (order) | **lane** — changes VM lifecycle (crash detection, adopted VMs, resume boot): the microVM lane runs `workspace-matches-conversation-after-vm-stop` and the kill-guest scenario on Firecracker; **guest image changes** | 21 GB → 20 GB | [x] local; lane pending | local (signing down; lane when pushed) |
| 4 | `AR-4.7`, `AR-4.9` (carried) | unknown-effect-is-reconciled-by-a-verify-read, unknown-effect-without-a-verify-read-is-left-to-the-model | vam | Sonnet medium — a host-run verify through the PEP, the ask on an identical retry and the reauth park, all on the waves 2–3 paths | 120 min | 3 | `host_tools.rs`, `security.rs`, `api/mcp_connections.rs` (one call), `vam-spec` `harness.rs` | fast | 20 GB → 20 GB | [x] local | local (signing down) |
| 5 | `AR-4.8` | provider-fallback-stays-in-the-allowlist | vam | Sonnet medium — `vam-inference` and the broker plane, a harness switch, mock faults | 90 min | 1 (after 4 in order) | `vam-inference/src/mock_script.rs` (every lane's mock; additive rule and reply keys), `harness.rs` | fast | | [x] | local `ar-4/wave-5` |
| 6 | `AR-4.10` | — (the bench scenario is active on the mock in smoke) | vam | Sonnet low — a scenario, two row fields and one export on names fixed by waves 0–4 | 60 min | 3, 4 | `vam-e2e/src/bench.rs` (proof lane's: additive fields), `e2e/task-sets.yaml` (additive 0.3.0), `scripts/e2e/run.sh` (one export) | fast; then **bench**: the real-model dispatch on the owner's word | 13 GB → 1.5 GB (ladder to step 3) → 17 GB | [x] local; real-model row pending | local `5696960`, (signing down) |
| 7 | `AR-4.11` | — | vam | Sonnet low — docs on names fixed by waves 1–6 | 45 min | 6; **HOLD until IT Phase 3 lands** | `docs/` (truth lane's until its Phase 3 lands) | none (`[skip ci]`) | | [ ] | |

Order: 0 → 1 → 2 → 3 → 4 → 5 → 6, then 7 when the truth lane allows. Wave 5
depends only on wave 1 and may move up if wave 3 or 4 is held. **Guest image:**
waves 2 and 3 change `vam-runtime` (rootfs rebuild for live runs; process
scenarios need none). **VM lifecycle:** wave 3 only. **Why not high for wave
2:** it adds rows around existing send points and changes no decision; the
orchestrator reads `host_tools.rs` and `security.rs` in review. **Why high for
wave 3:** the rubric's "concurrency or reconciliation" and "security boundary"
both apply (Phase 3's stop wave was high for the same race class).

**Shared surfaces per wave** (conflict rule, `phase-playbook.md` §11.2: two lanes
never build in one file at once)

- `host_tools.rs` and the PEP (`security.rs`): this lane's (waves 2, 3, 4);
  the isolation-truth redaction code there is finished and is not edited.
- `api/inbox.rs`, `api/queue.rs` and `gateway.rs`: this lane's this phase (wave
  3 adds one busy condition; `gateway.rs` is not edited unless Q12 is changed to
  re-check grants on resume). The primary-agent lane (launch row 16) touches
  `gateway.rs`, the inbox, the PEP and the receipt builder later: its build
  waves hold until wave 4 merges; its breakdown and wave 0 may run now.
- The receipt builder (`receipt.rs`): **E12's.** This phase does not edit it;
  it provides `effect_store::effects_for_run` (wave 1) for E12 Phase 3's
  effects field (coordination note in `run-receipt.md` Phase 3).
- The e2e runner: own module `effects.rs`; registration lines only in
  `scenario.rs`, `engine.rs`, `check.rs`, `events.rs`; `bench.rs` additive
  fields in wave 6 (proof lane's file).
- `vam-spec/src/harness.rs` (waves 1, 4, 5): the Harness lane's switch table;
  each wave takes `main` first. **Coordination with the Harness lane:**
  `harness.md` §6.1 assigns "provider retry and fallback" to H3 and launch row
  13 lists fallback for H3; this plan builds the one fallback behind
  `reliability.fallback` and leaves scoring and the escalation ladder to H3
  (Q7).
- `vam-inference/src/mock_script.rs` (wave 5): additive keys only.
- `docs/`: the truth lane's until its Phase 3 lands; wave 7 holds (as E5 did).

**Open questions** (each has a default; the owner approves or changes them)

1. **Idempotency key.** **Default:** `vamfx_` + base32 of the first 16 bytes of
   `HMAC-SHA256(K_effect, "vam-effect/v1" ‖ run id ‖ 0 ‖ call id ‖ 0 ‖ args
   digest)`; `K_effect = HMAC(node master, "vam-effect-key/v1")`
   (`signing.rs:140-170`), host-only, never sent to a guest; run id is the task
   id (locked decision 2); call id is the model's id as the host recorded it
   (`<call id>/<step id>` for a `run_step` call); args digest is
   `compute_args_digest` (`host_tools.rs:401`). **"Stable across generations"**
   means no input depends on the session, lease, lifecycle generation, token or
   time, so a resumed generation re-sends exactly the same key; the key is
   stored in the row, so a later master rotation does not change it. The host
   checks that the call id is in the run's recorded context with the same tool
   and digest; a call id it has not recorded (a recorder gap, an old guest) is
   still journaled and flagged.
2. **Verify-read declaration.** **Default:** one `EffectSpec {class,
   verify {tool, args_template, found}}` (`args_template` maps the call's
   arguments, `{"title": "{{args.title}}"}`; `found` is `{path, nonEmpty}` or
   `{path, equals: "{{args.x}}"}`), on a skill `ToolSpec.effect`, an HSP
   `ToolExport.effect`, and for MCP an owner-set per-tool `effects` map on the
   connection (`PATCH /v1/mcp/connections/:id`, like `ask_tools`; vendor
   `idempotentHint` is shown, never trusted). The verify tool must be a
   read-class tool of the same skill, broker or connection, checked at publish
   or registration; it runs through the PEP as the agent.
3. **Wording shown to the model.** **Default:** unknown, no verify: "Outcome
   unknown: <tool> was sent, but the task was interrupted before an answer came
   back. It may or may not have taken effect. Check with a read before doing it
   again; calling it again with the same arguments will ask the user first."
   Verified: "Done (confirmed by a check after an interruption): <verify
   result>". Verify finds nothing: "A check found no sign that <tool> took
   effect; you may call it again." Replayed: the recorded result, unchanged
   (the conversation reads as if nothing happened).
4. **Retention.** **Default:** rows live with the run record (as receipts,
   `run-receipt.md` decision 8) and go with it; arguments are digests only
   (never raw, receipt Q2); result bytes are kept only until the thread log
   records the tool message carrying them (digest checked), then cleared.
5. **Where the journal lives (locked decision 23).** **Default:** SQLite
   `effect_journal`, an index plus result bytes until the log has them (the
   reading of Phase 3 Q3 for pending inbox text). Not in the thread log: the
   log is versioned with the workspace and rolls back with it (decision 12),
   while an effect outside the VM never rolls back. The log stays the only
   source of conversation bytes. The owner confirms this reading.
6. **Rollback and a decided round (decisions 11 and 12 together).**
   **Default:** a resume restores the files of the last complete turn commit
   and the conversation up to and including the last assistant message the host
   recorded (the decision already acted on); that round's host calls are
   answered from the journal and its in-VM calls run again on the restored
   files. A stop that rolls back past host effects appends, after the
   `rollback` record, each journaled effect after the floor (the call and its
   outcome), so the conversation never forgets something done outside the VM.
   The owner confirms this reading of decision 12.
7. **Fallback.** **Default:** order = the operator allowlist order, skipping
   the failed model, models with no credential and models the agent's spec
   excludes; only after the provider's retries fail with 429, 529, 5xx or a
   transport error before the first streamed byte; one switch per task,
   sticky; never on 4xx; a caller-pinned model never falls back; "within the
   allowlist" = in `operator_allowed_models` and allowed for the agent; nothing
   left → FAILED `provider_unavailable` (a timed `waiting` needs Phase 5's
   `wait_until`). Off by default (`reliability.fallback`, harness decision 3:
   a lever becomes a default only with a row). The owner also decides keep
   (launch row 9) or cut (the rubric reads CUT; H3 then owns it whole).
8. **`needs_reauth`.** **Default:** as AR-4.9: journaled `intended`, PAUSED
   `waiting_for: reauth` with the connection id, cold resume of every run
   parked on that connection when it is reconnected, the approval timeout as
   its limit, Stop as for approvals; contract tests only this phase.
9. **"Lost task" for scoring.** **Default:** a trial counts one lost task when,
   after the kill or restart, the task ends FAILED or CANCELLED with a crash or
   restart reason while resumes remained; or is not terminal at the scenario
   budget; or a message queued behind it never starts; or a file at a complete
   turn commit, or a confirmed effect's result, is missing from the final files
   or conversation. An EXPIRED queued fire (past `not_after`) and a task that
   fails for its own reasons (budget, model) are not lost. Duplicates = the
   ledger's creates minus the one asked for, counted by the test broker; the
   receipt's count (E12 Phase 3) is a cross-check.
10. **Crash detection and attempts.** **Default:** a crash is a guest exit with
    no stop recorded, no budget timeout and no reported outcome; at most 2
    resumes per task (3 lifecycles), then FAILED `crashed_repeatedly` with files
    kept; a session terminate stays a stop.
11. **A VM adopted after a restart whose run was running.** **Default:** stop
    it and resume on a new VM like a dead one (the host lost the `ExecuteTask`
    exchange). Re-attaching in place is backlog.
12. **Triggered and queued runs.** **Default:** a resumed triggered run keeps
    `runs.source`, its credential and the loop guard; its grant is not
    re-checked on resume (checked at start; revoking ends later fires, Stop ends
    this one); a deactivated owner refuses the resume. A queued message waits
    behind a run awaiting resume, and its `not_after` keeps counting.
13. **`reliability.journal`.** **Default:** the journal is always on, also in
    `machine off` (locked decision 11, a correctness rule, not a lever): the
    switch becomes "fixed" like turn commits and accepts only `true`. This
    edits the L7 row of `design/harness.md`; the owner confirms (harness
    decision 3 does not apply to a locked decision).
14. **`calls.verifyReads`** (a read after every confirmed write). **Default:**
    built in wave 4 on the same declaration, default off; an H2 grid row
    decides.
15. **Event names.** **Default:** `effect.recorded {callId, tool, class, state,
    attempt, keyPrefix}` (terminal states of non-read classes only, to keep
    reads quiet), `effect.replayed {callId, tool}`, `effect.reconciled {callId,
    tool, outcome, by}`, `run.resumed {generation, reason, fromRound}`,
    `model.fallback {from, to, reason}`, `connection.needs_reauth
    {connectionId}`. The key prefix is 8 characters, never the whole key.
16. **The receipt in between.** **Default:** between this phase and E12 Phase
    3 the receipt still says "effects not journaled on this version" though the
    journal exists; that line is E12's (decision 5) and E12 Phase 3 replaces it.
    Alternative for E12: "effects journaled; not yet shown on the receipt".
17. **Hooks.** **Default:** `on_paused` and `on_complete` hook calls are not
    journaled (host-initiated, no model decision); backlog.
18. **Keys to vendors.** **Default:** always sent: HSP
    `InvokeRequest.idempotency_key`; MCP `params._meta["io.vam/idempotencyKey"]`
    (`_meta` is allowed on any request); the HTTP host adapter
    `Idempotency-Key` header only when its manifest config says the API honours
    it. The class, never the key, decides a re-send. Which MCP servers honour it
    (design "Still to drill into", Phase 4) is recorded per connection by its
    owner, not discovered.
19. **Internal host adapters** (`step_complete`, `start_playbook`,
    `check_playbook_finish`, `run_step` itself). **Default:** journaled as
    idempotent (the host is its own vendor and re-checks on a re-send); the
    outbox (`post_message`, `show`, `ask_user`) is a port-52 method, not
    journaled (the thread log records it).

**Locked decisions and shared files this plan touches** (none changed; Q5, Q6
and Q13 ask the owner to confirm readings)

- Decision 1 (P1): the journal and resume are host-side; the guest only adds a
  call id, which the host checks against what it recorded; a replay answers
  only the run's own call ids with already redacted results and skips
  `grants::mint`; the verify read passes the PEP; the key secret never leaves
  the host; arguments are digests only.
- Decision 2: the task (run id) is a key input; a resume is the same task.
  Decision 3: a stop during a host call lets it finish and journals it (V.3).
- Decision 11: implemented as written; MCP's mutating no-retry rule stays.
- Decision 12: Q6. Decision 13: direct egress stays outside, written in the
  docs page (wave 7). Decision 15: journal and resume in `vamd`; brokers get a
  key, never a decision. Decision 16: the reauth wait belongs to the task;
  `vamd` holds it. Decision 22: the effects test mode lives in the existing
  in-repo test broker (`hsp-echo-broker`), not in `brokers/` (it is a test
  fixture, as the trigger test broker is). Decision 23: Q5.
- Other trackers: E12 decision 5 (Q16; the coordination note is in
  `run-receipt.md` Phase 3); harness decision 3 (Q7, Q13); benchmark decisions
  3 (task set 0.3.0, additive) and 6 (a non-zero row is red).
- Shared files: as "Shared surfaces per wave" above. No wave needs a HOLD on a
  shared file today except wave 7 (docs).

**Depends on and Placement (proposed, owner to approve).** The stub's line
"Phases 2 and 3; after E8 and E7 Phase 1" predates the revised order:
`agent-experience.md` "Build order (revised 2026-10-21)" puts E10 Phase 4 with
E12 Phase 3 before E8 Phases 2–3 and E7 Phase 1, and the 0.1.0 launch plan
blocks row 9 (this phase) only on E10 Phase 3, while row 14 (E7 Phase 1) is
"scheduled after the lane frees" and E8 is folded into isolation-truth.
Proposed line: **"Depends on: Phase 3 (on `main` 2026-10-29). Before E8 and E7
Phase 1 (launch-0.1.0 §4 rows 9 and 14; build order revised 2026-10-21)."**
The **Placement** block needs the same note: its chain still reads `E10 P0–3
─▶ E8 ─▶ E7 P1 ─▶ E10 P4–6`; proposed `… E10 P0–3 ─▶ E12 P1 ─▶ E5 slice ─▶
E10 P4 (with E12 P3) ─▶ E4 slice ─▶ E8, E7 P1 ─▶ E10 P5–6 ─▶ …`, keeping the
rule under it: waves 2 and 3 change `vam-runtime`, so they never overlap E7
Phase 1 or E8 Phase 2 (rootfs). Both lines applied (owner, 2026-11-01).

**Phase close (stub)**

All seven scenarios `active` and green in CI (the six new, the changed
`work-survives-vamd-restart-mid-task`) and the bench scenario green on the
mock; `workspace-matches-conversation-after-vm-stop` and the kill-guest
scenario green on the microVM lane; benchmark before (by construction, above)
and after (mock row; the real-model row on the owner's word) recorded in the
progress log; the E12 Phase 3 breakdown can start (the journal read exists).

**Team checklist — Phase 4 (never twice)** (final, written by the orchestrator
2026-11-02 from the scenarios; at most 10 steps, about 10 minutes)

Setup (ops, once): deploy vamd from vam `main` at the Phase 4 release commit,
then **rebuild and install the guest rootfs** from the same commit (waves 2 and
3 change `vam-runtime`; on a Mac `./scripts/build-rootfs-lima.sh`). Register
the effects test broker (`hsp-echo-broker --effects --socket <sock>
--state-dir <dir>`, world piece `e2e/world/effects-test-broker.yaml`) and an
agent allowed `echo_create_ticket`, `echo_create_note`, `echo_find_ticket` and
`write_file`. Count creates any time with `hsp-echo-broker ledger --state-dir
<dir>`.

1. CLI: `vam run <agent> "Create a ticket titled Q4 plan, holdMs 8000"`, then
   `vam run events <run>`. **See:** `tool.invoked` for `echo_create_ticket`.
2. While it holds, kill the VM (`sudo pkill -9 firecracker`). **See:** the run
   stays running, then `run.resumed` (`guest_crashed`), `effect.replayed`, and
   the run succeeds naming `T-1`; the ledger shows one create.
3. Same as step 1, but restart vamd during the hold. **See:** `run.resumed`
   (`vamd_restarted`), `effect.reconciled` with `by: verify`, the run
   succeeds; the ledger shows one create.
4. Same with `echo_create_note` (no verify declared), restarting vamd during
   the hold. **See:** the thread says the outcome is unknown and that calling
   it again will ask; asking the agent to "do it again" shows an approval card
   "… may already have taken effect"; deny it; the ledger shows one note.
5. Start a slow create and press Stop (app or `vam run stop <run>`) during the
   hold; then ask "did you make the ticket?". **See:** the run ends "stopped
   by user"; the answer names `T-1`; one create.
6. `vam receipt show <run>` and `vam receipt verify <file>` on the run of step
   3. **See:** verify passes; one create in the tool calls; the effects line
   still says "not journaled on this version" (expected until E12 Phase 3).
7. A scheduled or triggered run doing a slow step; restart vamd during it.
   **See:** the same run id resumes and succeeds; no second run starts.
8. Two allowlisted models with the first unreachable, the run started with
   `harness: {reliability: {fallback: true}}` and no pinned model. **See:**
   `model.fallback {from, to}`; the run succeeds on the second model. With the
   model pinned, it fails instead (no fallback).
9. (Only with an OAuth MCP connection) mark it needs-reauth at the vendor, run
   a task that uses it. **See:** the run waits "Reconnect <connection> to
   continue"; reconnect; the task continues and the call runs once. If that
   tool's policy is ask, the task asks again after the reconnect.

Report: "Phase 4 pass", or the step number with the error text. The paid
kill-mid-task row (task set 0.3.0, `vm-task-killed-mid-effect-finishes-once`,
k = 5) runs on GitHub Actions (`bench.yml`, the owner authorised it
2026-11-01) after the push.

### Phase 5 — Long work (U4)

**Goal:** hand off a long task and come back to the result.
**Depends on:** Phase 4
**Design:** design/long-work.md (2026-11-28; build waits for the first Apps pilot)
**Plan:** not drafted (one breakdown per slice 5a–5e; the pilot signals in design §9 decide which slices and their order)
**Exit:** the 50+ call triage task passes; leave and return works.

Proposed decisions LW1–LW8 (design §10, owner to confirm) add to the list
above and change none of it.

- [ ] Park between turns after keep-warm; task budgets replace fixed caps.
  (5a: park on any open wait, not only an approval; five meters: turns,
  active time, money, wakes, wall-clock deadline; `request_budget` within
  the operator/App ceiling; a spent budget parks with Extend; **Pause**
  keeps files and queued steers; bench task `u4-triage`.)
- [ ] Background processes (`run` background, `bg_*`).
  (5b: `bg_start`, `bg_status`, `bg_output`, `bg_kill`, `bg_wait`;
  per-process wall-clock and capture caps; a live process keeps the task
  awake and counts active time; a spent active-time meter stops it (output
  kept) and parks the task; the wall-clock deadline ends the task; lost
  after a crash, never restarted.)
- [ ] In-VM sub-agents (`spawn_agent`).
  (5d: `worker_status`, `worker_send`, `worker_stop`, `wait_for_workers`;
  own lane, subset of tools, budget slice, depth 1, default 2 live, cap 4;
  scratch dir plus host-side write leases; shared commits and rollback;
  an approval parks only that worker.)
- [ ] Todo recitation; notify on finish through `on_paused` hooks.
  (5a: recitation on for every long task; `NOTES.md` re-read after each
  compaction; unattended long tasks notify on finish.)
- [ ] Timer waits: `wait_until`, `waiting(timer)`, backoff re-checks with
  a maximum number of wakes.
  (5c: deadline sweeper; a wait longer than 30 s parks with no keep-warm
  hold; a wait past the task deadline is refused; a steer wakes a parked
  task.)
- [ ] Observability and watchdogs (5e): worker rows and per-lane cost,
  heartbeat, no-progress and spend-velocity watchdogs; stalls notify.
- [ ] Repeated-compaction bench task (`u4-repeated-compaction`, at least
  five compactions).

### Phase 6 — Learns (U7)

**Goal:** teach it once.
**Depends on:** Phase 4
**Plan:** not drafted
**Exit:** the repeat Notion task takes fewer turns than the first.

- [ ] `vam.memory` broker: personal facts (user × agent), visible and
  editable, through memory tools and an `on_request` hook.
- [ ] Recipes proposed from `on_complete` trajectory summaries, approved
  by the connection owner.

## Backlog

Not yet in a phase's scope. Each is scoped when its target phase is planned.

- Constrained decoding and tool masking for self-hosted models (Phase 0).
- Escalation ladder to a stronger allowed model (Phases 0 and 4).
- Model profiles as marketplace packages; public VAM benchmark score (E6).
- Safe replay of past tasks against a candidate model (after Phase 4).
- Teach once: frontier-model runs become reviewed recipes (Phase 6).
- Child-VM sub-agents pulled forward (after Phase 5).

## Test infrastructure backlog (phase-playbook §6, 2026-10-15)

Needed for the cheaper phase close. Planned as small issues before the next close.

- Scripted mock model in the e2e harness: vamd starts with vam-inference
  `MockProvider` and a per-scenario script of tool calls and replies. Live
  scenarios use it by default; real-model runs are opt-in.
- One phase-check command that builds only what changed (rootfs only when
  `vam-runtime` or rootfs inputs change), runs selected scenarios, writes the
  report.
- `the VAM app/scripts/e2e/smoke.txt`: the five-check smoke set.
- Fix the admin "signs in" window-focus flake.
- Map changed areas to scenarios, so a PR's targeted live scenario is picked
  from the diff.

## Explicitly later

- Code mode (typed MCP clients in the VM, journaled, deterministic replay).
- Child-VM sub-agents with attenuated permissions, workspace fork and
  merge (needs A16 fixed and Drive9 merge).
- Embedding-based tool ranking.
- Event waits (`wait_for`), standing schedules, `vam.github`, `vam.schedule`:
  E5 slice, right after Phase 6 (`docs/src/content/docs/hosted/triggered-runs.md` amendment). Slack and
  email: E5 later.
- Probes (a script in a fresh VM, no model, that wakes the agent).
- Admin management of HSP brokers (keys, provider choice) from the app or
  API, and broker-contributed UI in the app (owner, 2026-10-17: not decided).
- Web: more search adapters (Parallel, Tavily, Exa, Perplexity), a vendor
  extract fallback for JavaScript-heavy pages, streamed answer tokens.

## Progress log

| Date | Phase | What landed |
|---|---|---|
| 2026-11-01 | 4 | **Breakdown drafted** (background agent, Opus 5.5 high) (base level with `main`): code map, scenario delta (6 new, 1 changed: `work-survives-vamd-restart-mid-task`, a spec change for the owner), a test-broker world piece that counts creates, benchmark delta (U5 duplicates and lost tasks; baseline lost k of k and duplicates 0 by construction), items `AR-4.1`–`AR-4.11`, waves 0–7 (wave 3 Sonnet high, `actions: lane`; wave 7 docs held for the truth lane), 19 open questions with defaults, proposed new "Depends on" and Placement lines. E12 coordination note added to `run-receipt.md` Phase 3. Owner approval pending. Scoreboard: 0 green, 0 red (not yet written), 0 fix cycles. |
| 2026-11-01 | 4 | **Breakdown approved** by the owner with the orchestrator's recommendations (R1 fail-closed journal write, R2 wave 3 on Sonnet high) and decisions A–D (spec change to `work-survives-vamd-restart-mid-task` approved; fallback kept behind the switch; `needs_reauth` by contract tests, scenario to backlog; Depends-on and Placement applied). Wave 0 next. |
| 2026-11-01 | 4 W0 | **Wave 0 merged locally** into `ar-4/base` (commit, unsigned local commit: the session's signing service is down; owner, 2026-11-01: commit locally, re-sign and push when it is back; owner also delegated wave approvals for this phase to the orchestrator). Six new scenarios plus the changed `work-survives-vamd-restart-mid-task`, all `pending`, red right reason (class `product`) in the orchestrator's re-run: guest-crash `then.0`, verify-read `then.0`, no-verify `when.3`, follow-up-after-stop `then.0` (ledger 2 vs 1), triggered `then.0`, fallback `when.1`, restart `when.4`. Effects test broker (`hsp-echo-broker --effects`) writes and syncs its ledger before answering; runner steps `kill-guest` and `expect: ledger` in `vam-e2e/src/effects.rs`; five event names with no emitter. Deviations accepted (no `then` changed): fallback scenario uses catalog ids and its failing-model mock rules come in wave 5; follow-up-after-stop drops the `b.txt` check; triggered scenario's queued message added in wave 3; verify declaration a comment until the manifest field (wave 1). `--full`: all workspace tests pass except the known root-only `hsp_uds::uds_socket_acl_deny_fails_fast`; smoke (agent run) 74 pass, 7 red right reason. Scoreboard: 0 green, 7 red (right reason), 0 fix cycles, about 1.5 h. |
| 2026-11-01 | 4 W1 | **Wave 1 merged locally** into `ar-4/base` (unsigned; Sonnet 5.5 medium). Contracts: `ToolIntentRequest.call_id = 6`, `ExecuteHostToolRequest.call_id = 5`, `CapabilityClaims.call_id` (serde, `vam-security` token), `EffectClass`/`EffectSpec`/`EffectVerify`/`EffectFound` in `skills.proto`, `ToolSpec.effect = 13`, `ToolExport.effect = 5`, `InvokeRequest.idempotency_key = 10`, `RunStatus.resumes = 23`, `WAITING_FOR_REAUTH = 4`; MCP owner-set `effects` map (`effects_json`, PATCH and GET). `effect_store.rs` (`effect_journal`, PK run+call, every write returns `Result`, checked transitions, `effects_for_run` for E12 Phase 3, extra `detail` column); `effect_key.rs` (`vamfx_` + base32 of HMAC over run, call, `vam_spec::canonical_args_digest`; secret `NodeKeys::effect_key_secret()`, no `Debug`; Q1 corrected from the private `compute_args_digest`); `effect_class.rs`. **Orchestrator decision:** `reliability.journal` defaults `true`, `Fixed` (a receipt must not print `false` for a journaled run); base profile digest bumped to `sha256:…`; historical bench results and fixtures keep…`. Effects test broker declares its verify read for real. Orchestrator re-run at the head: `--full` only the known root `hsp_uds` failure; `--fast` smoke ok, the seven pending red right reason at the same steps. Scoreboard: 0 green, 7 red (right reason), 0 fix cycles, about 1.2 h. |
| 2026-11-01 | 4 W2 | **Wave 2 merged locally** into `ar-4/base` (unsigned; Sonnet 5.5 medium). **Green and `active`: `follow-up-after-a-stop-sees-the-external-effect`.** Journal at the host tool path (`effect_path.rs`): `intended` before the token is minted, `sent` immediately before each of the three send points (host adapter `invoke`; HSP after connect and before the grant mint, non-read calls via `HspClient::invoke_once`, no reconnect-and-resend; MCP with the key in `_meta["io.vam/idempotencyKey"]`), then `confirmed`/`failed`/`unknown` (unknown = request left with no definite answer; a host-adapter error on a non-idempotent call is unknown). R1 fail closed: a failed `intended` or `sent` write sends nothing and returns `effect_journal_unavailable: not sent: the host could not record this call; nothing left the host` at intent and execute. Execute checks call id, tool, args digest and re-derived key against the row; a row past `intended` is answered from the record, never re-sent; `effect_mark_sent` is a locked checked transition, so two executes cannot both send. Stop waits up to 60 s for in-flight host calls; the log keeps journaled outcomes, also after a rollback (Q6). Denial and stop fail the waiting row; amended approvals re-bind. Orchestrator reviewed the send points and the store transition. Gaps: skill-declared classes not reachable at intent (inferred class used, safe default); legacy-id runs (old guest image) are not resumable blindly (wave 3); outer `run_step` not journaled (inner calls are). Orchestrator re-run: `--full` only the known root `hsp_uds` failure; smoke ok, six pending red right reason. Scoreboard: 1 green, 6 red (right reason), 0 fix cycles, about 1.3 h. |
| 2026-11-01 | 4 W3 | **Wave 3 merged locally** into `ar-4/base` (unsigned; Sonnet 5.5 high). **Green and `active`: `effect-is-not-repeated-after-the-guest-crashes`, `work-survives-vamd-restart-mid-task` (changed, owner decision A), `triggered-run-resumes-like-a-chat-run`.** Typed `GuestTaskError`; a crash is a transport loss with no stop recorded; the stop wins at classification, after `begin_finalize` (atomic with `begin_stop`) and inside `mark_resume_pending` through a **persisted stop mark** (a stop then a restart never resumes). `run_resume` table (`resume_store.rs`), compare-and-set moves; `resume.rs` `interrupt` (rollback to the last complete turn commit, the decided round kept open, incomplete commit rows removed); resume pass at startup, on a crash wake and a 2 s tick, single-flight; adopted RUNNING VMs interrupted and resumed on a new VM, PAUSED warm ones left. Replay before the PDP (`security::replay_from_journal`): `confirmed`/`failed` answer `REPLAYED` with the stored redacted result, no token, no grant, `effect.replayed`; `sent`/`unknown` answer the unknown text, never re-sent; `intended` gets a fresh decision; a reused call id with other arguments is refused. Guest: a fourth start mode runs the dangling round's calls before the next model request. **Q10 answered by the orchestrator:** crashes and restarts counted separately, cap 2 → `crashed_repeatedly`, cap 5 → `restarted_repeatedly`; legacy call ids → `not_resumable`; `busy_run` blocks new runs behind a resume. Mock fix (no `then`): `echo half` → `printf half`. `unknown-effect-is-reconciled-by-a-verify-read` now red at `then.2` (expectFail moved). Finding: the default `LOCAL_CI_OUT` in a worktree makes the gateway socket path 116 > 107 bytes; scenario runs in worktrees use `LOCAL_CI_OUT=/tmp/claude-0/ci…`. Orchestrator re-run: `--full` only the known root `hsp_uds`; smoke ok, three pending red right reason. **microVM lane pending** (needs a push): `workspace-matches-conversation-after-vm-stop`, `vm-run-waits-at-a-gate-and-finishes`. Scoreboard: 3 green, 3 red (right reason), 0 fix cycles, about 1.6 h. |
| 2026-11-01 | 4 W4 | **Wave 4 merged locally** into `ar-4/base` (unsigned; Sonnet 5.5 medium). **Green and `active`: `unknown-effect-is-reconciled-by-a-verify-read`, `unknown-effect-without-a-verify-read-is-left-to-the-model`.** Reconcile (`effect_reconcile.rs`, ahead of the intent in `brokers.rs`): read/idempotent `unknown` re-sent with the same key (`effect_resend`, `by: host`); non-idempotent runs the declared verify read once, through the PEP as the agent with its own `policy.decided` (row `<call>/verify`, HSP fence unwrapped before parsing): found → `confirmed`, `reconciled_by: verify`; **not found stays `unknown` (orchestrator decision: a verify can miss a late write)** with "A check found no sign that <tool> took effect. Calling it again with the same arguments will ask the user first."; an identical later call (tool + args digest) on a row `unknown` or verify-confirmed parks on an ask ("may already have taken effect"). `effect.reconciled {callId, tool, outcome, by}`. `calls.verifyReads` built, default off (one verify after each confirmed non-idempotent call that declares one). Reauth (`api/reauth.rs`): an MCP call to a `needs_reauth` connection parks (`waiting_for: reauth`, `connection.needs_reauth`), row `intended`, nothing sent; **a reconnect decides the park `RECONNECTED`, which mints no grant** (orchestrator requirement; checked in `workflow.rs`), the call then gets its normal decision; only that connection and owner resolve it; Stop ends it; contract tests `reauth_park_tests`. **Known issue (fixed by):** after a reconnect, a call whose policy is ask or deny failed the task instead of parking again. Gap: skill-declared verify reads unreachable (broker and MCP work). Orchestrator re-run: `--full` only the known root `hsp_uds`; smoke ok, only the fallback scenario red right reason. Scoreboard: 2 green, 1 red (right reason), 0 fix cycles, about 1.2 h. |
| 2026-11-02 | 4 W5 | **Wave 5 built locally** (`ar-4/wave-5`, unsigned; Sonnet 5.5 medium). `reliability.fallback` is a free switch (default off; base digest…` unchanged). Broker (`vam-inference`): the first thing the provider sends decides; 429, 529, 5xx or a transport error before any byte moves the task once to the next allowlisted model with a key (`FallbackPlan`, sticky, never a 4xx); host side `fallback_plan_for` (switch on, source not `caller`, non-empty allowlist) with `inputs.model_source` written by the host; event `model.fallback {from, to, provider, reason}`. Mock: `model` condition, `fail: {status}` reply. Process-guest test node answers fallback candidates from the mock (`serve_as_mock`). **`provider-fallback-stays-in-the-allowlist` active and green** (`when` now `model: none`, mock rules filled; `then` untouched). |
| 2026-11-02 | 4 W5 | **Wave 5 merged locally** into `ar-4/base` (unsigned). Orchestrator review: `serve_as_mock` is compiled only with `process-guest` and on only in process mode (never in a VM build); a caller-pinned model never falls back (`inputs.model_source` stamped by the host, caller value overwritten). Orchestrator re-run on the merged base: `--full` only the known root `hsp_uds`; smoke **81/81 pass**, all ten Phase 4 scenarios green. Disk: 20 → 8.5 GB during the wave; ladder step 1 (deleted `target/debug/incremental`, no agent building) → 13 GB. Scoreboard: 1 green, 0 red, 0 fix cycles, about 1.3 h. |
| 2026-11-02 | 4 W6 | **Wave 6 merged locally** into `ar-4/base` (`5696960`, unsigned; Sonnet 5.5 low). Bench task `u5-kill-mid-task` (task set 0.3.0) as `task-killed-mid-effect-finishes-once` (process, active, smoke) and `vm-task-killed-mid-effect-finishes-once` (`needs: firecracker`, same `then`); `kill-guest` kills the session's Firecracker VMM (a `/proc` scan by session id, else every `firecracker` process; one run per bench node). Bench rows gain `duplicateEffects` (ledger creates beyond one) and `lostTasks`; either above 0 fails the trial. **Lost rule (orchestrator):** never terminal, or ended `guest_crashed`, `vamd_restarted`, `crashed_repeatedly`, `restarted_repeatedly`, `not_resumable`; `PAUSED` and an `EXPIRED` queued fire are not lost. `run.sh --firecracker` exports the test broker; `bench.yml` bench steps get `VAM_E2E_TEST_BROKER`. **Benchmark (mock, process):** U5 duplicate effects 0, lost tasks 0 (k = 1); baseline by construction: lost k of k, duplicates 0 only because nothing resumed. Real-model row: `bench.yml` dispatch with `scenarios=vm-task-killed-mid-effect-finishes-once`, two models, k = 5, after the push. **Disk:** the mock bench took free space to 1.5 GB; ladder steps 1–3 (incremental, duplicate test executables, `cargo clean` between waves with no agent building) → 30 GB, rebuild 7.7 GB target. Orchestrator re-run: `--full` only the known root `hsp_uds`; smoke 82/82; `vam-e2e` 94 tests. Scoreboard: 1 green (bench scenario), 0 red, 0 fix cycles, about 1.5 h. |
| 2026-11-02 | 4 | **Signing back; stack re-signed and pushed** (`rebase -r`, tree unchanged) as. GitHub Actions: checks, workspace tests, smoke green; **microVM lane 9/9** after one CI fix (`rust.yml` lane step now passes `VAM_E2E_TEST_BROKER`; the first run failed `vm-task-killed-mid-effect-finishes-once` at `given`, class env), incl. `vm-run-waits-at-a-gate-and-finishes`, `workspace-matches-conversation-after-vm-stop` and the real VM kill. **Benchmark U5 (task set 0.3.0, `vm-task-killed-mid-effect-finishes-once`, Firecracker, profile base):** mock 1/1; **Claude Sonnet 5.5 5/5, cost per success 4,922.6 micro-USD; Claude Haiku 5.5 5/5, 261.4 micro-USD; duplicate effects 0 and lost tasks 0 on every trial** (a trial with either above 0 is not a success). Before: lost k of k by construction (every crash ended the task), duplicates 0 only because nothing resumed. **Phase exit met.** |
| 2026-11-02 | 4 | ** merged** into `feat/agent-runtime` (merge commit). **Fix wave merged** (merge commit; Sonnet 5.5 medium; owner asked for it 2026-11-02): resuming after an approval or a reconnect now handles every answer: `REPLAYED` becomes the recorded result (never sent), `DENIED` becomes the normal denial and the task continues, `SUSPENDED` parks again on the new approval (same call; the recorder writes only a new `suspend` record for a re-suspend with nothing logged since). Host: an `intended` row moves to the new approval id; a reconnect into deny marks the row `failed (denied)`; `waiting_for` moves `reauth` → approval. Closes the W4 known issue. Remaining known edge: a vamd restart between the two suspends ends the task clearly with nothing sent. Orchestrator re-run at the head: `--full` only the known root `hsp_uds`; smoke 82/82; Actions: fast jobs and the microVM lane green. Scoreboard: 0 green (no scenario; contract and unit tests), 0 red, 0 fix cycles, about 0.8 h. **Phase 4 built; release PR open, awaiting the owner.** |
