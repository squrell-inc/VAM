# Long work: hand off a long task and come back to the result

Status: **proposed** (2026-11-28). Epic **E10 Agent runtime, Phase 5 (U4)**.
Tracker: `.cursor/workstreams/agent-runtime.md` "Phase 5 — Long work".
**Design only.** The owner decided on 2026-11-28 that the build waits for
the first Apps pilot (`design/vam-apps-spaces.md`); the pilot's evidence
decides which slices are built and in what order (§9).

This document refines `design/agent-runtime.md` §IV.3 (park, background
processes, sub-agents, recitation), §V.5 (capacity), §VII.3 row "5 Long
work" and §IX.4 (waits). It does not change the tracker's locked decisions;
where it adds a rule, the rule is listed in §10 as proposed.

## What we are building

A user, or an App's backend, hands a VAM agent a job that takes hours or
days ("triage these 200 issues", "watch this migration and fix what
breaks", "draft replies for this week's tickets") and comes back to the
result. While the job runs:

- The task **parks between turns for any reason** (approval, question,
  timer, child result, a pause, a budget stop) and costs nothing while
  parked. It resumes on a new VM from its last turn commit.
- The task runs within a **task budget** (turns, active time, money,
  wakes, a wall-clock deadline) that the agent may raise up to an
  operator ceiling, instead of fixed caps that end the run.
- The agent treats its **context as a cache**: the plan and its notes live
  in files, are restated each turn, and are re-read after every
  compaction.
- **Long waits cost nothing** (no VM between wakes) once the task parks:
  an event first, then a broker-side condition, then a timer with backoff
  and a maximum number of wakes. A timer wait longer than 30 s parks at
  once; other waits park after the 300 s keep-warm; nothing parks while a
  background process or worker runs (§3, §6).
- **Background processes** and **in-VM workers** (sub-agents) let one task
  do slow or parallel work without a new VM.
- **Watchdogs** turn a stalled or runaway job into a notification, not a
  silent bill.
- The user can **leave and return**: progress, a heartbeat and the result
  are in the thread and the notification list.

## Why

- **U4** (`design/agent-runtime.md` Part I): "the agent works for as long
  as the task needs, within a budget. It shows progress, keeps its context
  clean, and tells the user when it's done or needs them." Measure: pass^k
  on a 50+ call task, context never overflows, progress messages arrive,
  leave-and-return works.
- **P13** ("can't do real, ongoing work"): Phases 2 to 4 made a task
  durable (thread log, turn commits, inbox, effect journal, crash resume).
  What is left is length: today a task that needs more than 50 turns or 30
  minutes of active time ends, and a task can only park on an approval.
- **Apps.** An App's backend starts runs with no human watching
  (`design/vam-apps-spaces.md`, owner answer C1 to C3). Those runs are the
  first place long, unattended work shows up, so the pilot is the right
  source of evidence for scope.

## 1. What the code does today

Every limit below was checked against the current branch (cut from
`origin/main`, 2026-11-28).

| Limit | Value | Where |
|---|---|---|
| Turns per task | 50 default, hard cap 200; a caller may only lower it | `vam-orchestrator/src/run_context.rs:72`, `:74`, `resolve_run_budgets` `:316`; guest mirror `vam-runtime/src/agent.rs:1567-1568` |
| Step timeout (one model call or tool round) | 300 s | `vam-runtime/src/agent.rs:1569`, `RoundBudget` `:1669` |
| Active time per task (`maxDuration`) | 1800 s; the clock stops while `PAUSED` | `run_context.rs:78`; `workflow.rs:2581-2586` (`expire_over_active_budget`); sweeper `manager.rs:556` |
| Money | optional per-run cap, enforced by the broker | `vam-inference/src/budget.rs:41` (`SessionBudget.max_financial_budget`) |
| Compaction | at 70% of the window (`open-small` profile: 60%); last 3 rounds kept verbatim, tail within 25% | `vam-runtime/src/compaction.rs:28`, `:34`, `:36`; `vam-spec/src/harness.rs:218`, `:452` |
| Thread log size | 256 MiB per thread, 1 GiB per workspace | `vam-storage/src/log_quota.rs:17`, `:19` |
| Idle session | 24 h (`VAMD_SESSION_IDLE_TTL_SECS`) | `vam-orchestrator/src/daemon.rs:220`, `ttl.rs:4` |
| Approval wait | 1 h operator default (`VAMD_TOOL_HITL_TIMEOUT_SECS`) | `daemon.rs:206`, `security.rs:1566` |
| Gate timeout | 72 h default, 30 days max at publish | `workflow.rs:51` |
| Loop detector | 5 identical calls, 3 A-B cycles, 3 identical errors; **on only in the `open-strong` and `open-small` profiles** (`detect: false` in `base`) | `harness.rs:259`, `:443` |
| Todo recitation | `context.reciteTodo`; on in `open-strong`/`open-small`, off in `base` | `harness.rs:190`, `:444`; `thread_recorder.rs:167` |
| Inbox | 20 undelivered messages per run | `vam-orchestrator/src/inbox_store.rs:23` |
| Live tasks per workspace | one; a parked run keeps a workspace reservation | `api/inbox.rs:87` (`busy_run`), `:146`; `workflow.rs:64`, `:2370` |
| Keep-warm then park | 300 s; **only a run with an undecided row in `approvals`** (tool approval, gate, question) parks | `workflow.rs:47`; `runs_due_to_park` `workflow.rs:2309` (inner join on `approvals`); `manager.rs:611` |
| `on_paused` hook | 15 s budget | `hsp_hooks.rs:35` |

What already survives a long job:

- **Conversation** lives in the thread log, hash-chained, versioned with the
  files (tracker decision 23; `vam-storage/src/thread_log.rs:11`). Records
  already carry `lane` and `parent_lane` (`thread_log.rs:396-398`); today's
  lanes are main, `summary`, `note`, `reasoning` and `resample`
  (`vam-spec/src/compaction.rs:18-25`).
- **Files** survive through turn commits (decision 12).
- **The VM is disposable**: Phase 4 resumes a task on a new VM after a crash
  or a `vamd` restart, and the effect journal makes a retry safe
  (decision 11).
- **Progress** reaches the app through the outbox (`post_message`;
  scenario `long-task-posts-progress`).
- **Notifications**: a run that waited on someone, or that a trigger
  started, leaves a "finished" record (scenario
  `background-run-end-notifies-and-chat-turn-does-not`).

## 2. What is missing

| Gap | Effect on a long job |
|---|---|
| Park only on an approval | A task that waits for a timer, a child or a slow process keeps its VM running and its active-time clock ticking, or ends. |
| Fixed caps | At turn 50 or minute 30 the task ends (`DURATION_STOP_ERROR`, `notifications.rs:1340`). The user cannot extend and continue; V.4's "budget reached → park" is not built. |
| No wall-clock cap | Active time stops while parked, so a task that re-checks on a timer could live forever. Timers need a maximum number of wakes and the task needs a deadline. |
| Stop is the only way to halt | Stop rolls files and conversation back to the last complete turn commit and discards queued steers (`inbox.rs:251` `discard_for_stop`). There is no "pause and keep everything". |
| One live task per workspace | A long job holds its workspace (and its reservation while parked); every other thread on that workspace queues behind it. |
| Compaction proven only on short scenarios | `compaction-keeps-an-early-correction` and `follow-up-after-compaction` compact once. A long job compacts many times; summary drift is unmeasured. |
| No 50+ call task in the benchmark | The tracker's exit names "the 50+ call triage task"; the task sets have only `u4-progress` (`e2e/task-sets.yaml:20`). |
| Unattended finish is silent | A task that ran long but never waited on anyone leaves no "finished" notification. |

## 3. The task lifecycle: park for any reason

A task is one unit of work (decision 2). It moves between **running**
(a VM, the clock ticks) and **parked** (no VM, the clock stopped) as many
times as it needs, until it ends.

**Wait kinds.** Each park records why, as `waiting_for` on the run:

| `waiting_for` | Set by | Resolved by | Exists today |
|---|---|---|---|
| `approval`, `question`, gate | PEP, `post_message` question, playbook gate | the decider (or its timeout) | yes |
| `reauth` | Phase 4 | the connection owner reconnects | yes |
| `timer` | guest `wait_until` (§6) | `vamd` deadline sweeper | no |
| `event` | guest `wait_for` | broker `DeliverEvent` (E5 slice, not this phase) | no |
| `workers` | guest `wait_for_workers` (§8) when every worker is itself waiting | a worker's result or the deadline | no |
| `pause` | the user's **Pause** | the user's **Resume**, or a steer | no |
| `budget` | `vamd` when a task budget is spent (§4) | the user extends, or stops | no |

**Rule.** At a turn boundary, if the guest reports it is waiting and no
background process or worker is running, the run goes `PAUSED` with its
`waiting_for`, gets the keep-warm deadline (300 s, decision 5), and
`sweep_keep_warm` parks it. A `timer` wait longer than 30 s skips
keep-warm and parks at the turn boundary, keeping only its timer wake; a
shorter one keeps the VM up and counts active time. This generalises `runs_due_to_park`
(`workflow.rs:2309`) from "an undecided approval" to "any open wait row".
The wait row holds the deadline (decision 16); `vamd` stores no standing
schedule.

**Pause is not Stop.** Pause takes effect at the next turn boundary: the
turn commits, the queued steers stay queued, the task parks with
`waiting_for: pause`, active time stops. Nothing rolls back. Resume (or any
steer) wakes it from that commit. Stop keeps its meaning (decision 3, 12):
deny pending approvals, roll back to the last complete commit, discard
queued steers.

**Messages to a parked task** go to its inbox and wake it (as §IX.4 says
for triggers). A steer during `timer` or `pause` ends that wait early.

**The workspace.** A parked task keeps its reservation, so one long job
still holds its workspace (the "one live task per workspace" rule, design
§"Still to drill into"). We keep the rule. What changes: the queued message
on another thread tells the user which task holds the workspace and offers
Stop on it or a separate Space (Pause keeps the reservation, so it does not
free the workspace), and an App caller is told to use a separate Space for
concurrent jobs. Per-thread overlays wait for Drive9 merge, as before.

## 4. Task budgets

Fixed caps become a **task budget** with five meters. Each has a starting
value, an **operator ceiling**, and a behaviour when spent.

| Meter | Starting value | Operator ceiling (default) | When spent |
|---|---|---|---|
| Turns | 50 (decision 5) | 200 (today's hard cap, now a setting) | park `budget` |
| Active time | 30 min (decision 5) | 4 h | park `budget`; live background processes are stopped first, their captured output kept (§8.1) |
| Money | operator's existing budget, or none | operator's budget | park `budget` |
| Wakes (timer and condition re-checks) | 20 | 200 | the wait resolves `max_wakes`; the agent decides what next |
| Wall-clock deadline | 7 days from start | 30 days (same bound as gates, `workflow.rs:51`) | the task ends `deadline_reached`, running or parked, live processes included; owner notified |

- **Where the ceiling comes from.** Node operator settings first; for a
  shared App, the App's limits (Apps lane P5 "App limits") narrow it. A
  caller may lower a starting value, never raise the ceiling (today's
  `caller_max_iterations` rule, `run_context.rs:316`).
- **The agent may ask for more** (decision 5: "Agents may raise them within
  operator limits"). A `request_budget {meter, amount, reason}` tool raises
  the meter at once if the result stays under the ceiling, records
  `budget.raised` on the run and posts one outbox line ("Raised turns to
  120: 140 issues left"). Above the ceiling it is refused with the ceiling
  in the error; the task carries on until spent, then parks `budget`.
- **Budget reached parks; it does not end the task** (design §V.4). The
  user, or the App owner for a shared App, sees "Budget reached: turns
  200/200" with Extend and Stop. Extend is a human action and may exceed
  the agent's ceiling up to the operator's absolute maximum.
- **Workers draw from the task budget** (decision 4): a worker gets a slice
  (default: an even share of what is left, at most half), returns what it
  did not use, and its spend is counted on its lane.
- The step timeout (300 s) stays a per-step guard, not a budget.

## 5. Context as a cache

A long job will compact many times. The design rule: **nothing the job
needs lives only in the conversation.**

1. **Todo recitation** (exists): `todo_write` writes
   `.vam/agent/todos.json` (`security.rs:25`); `context.reciteTodo`
   restates it at the end of each request. Phase 5 turns recitation on for
   every task that has parked or compacted once, whatever the profile.
2. **`NOTES.md` convention**: a long-running task is told (one line in the
   system prompt when the task budget exceeds the starting values, or on
   the first compaction) to keep durable findings in `NOTES.md` at the
   workspace root. It is an ordinary workspace file (turn commits cover
   it; no new tool). After every compaction the runtime re-reads it into
   the next request, fenced and capped (default 8 KiB, the tail kept).
3. **Results as files** (Phase 0) keep large tool output out of the
   context.
4. **Repeated-compaction benchmark**: a bench task that forces at least
   five compactions and checks that the goal, an early correction and the
   todo state survive. This is a Phase 5 exit item, and an input for the
   harness lane's H2 compaction levers (held until pilots give real tasks).

## 6. Waits that cost nothing

"Cost nothing" means no VM and no active time between wakes, and it holds
only while the task is parked (§3). A `timer` wait longer than 30 s parks
at the turn boundary; a `timer` wait of 30 s or less keeps the VM up and
counts active time, because parking and resuming would cost more. Event and
broker waits park after the 300 s keep-warm, so they cost up to 300 s of VM
first. No wait parks while a background process or worker runs (§8); that
time counts as active time.

Per decision 16 the guest only asks; `vamd` holds deadlines; brokers own
vendor watching and standing schedules. Order of preference:

1. **Event** (`wait_for`): E5 slice, after Phase 6. Not built here.
2. **Broker-side condition**: a broker polls the vendor with no model and no
   VM and fires only when the condition holds. Also E5.
3. **Timer with backoff** (`wait_until`): built in Phase 5.

`wait_until {at | after, recheck?: {backoff: [60, 120, 300, 600], cap: 600,
max_wakes}, reason}`:

- Without `recheck`, the task waits until `at` (bounded by the task
  deadline) and resumes once with "time reached"; whether it parks follows
  the next rule.
- A wait longer than 30 s parks at the turn boundary with no keep-warm
  hold (§3), so no VM runs until the wake; a wait of 30 s or less keeps
  the VM up and counts active time. The rule is the same with or without
  `recheck`, and applies to each delay of a backoff.
- With `recheck`, each wake is a **check turn**: the model runs its check
  and either continues the job or calls `wait_until` again; the next delay
  follows the backoff. A wake counts against the wakes meter, not turns
  (a check turn that does real work counts as a turn too).
- At `max_wakes` (default 20, ceiling 200) the wait resolves with
  `max_wakes`, and the agent must report or stop; it cannot silently
  re-arm past the meter.
- A deadline past the task's wall-clock deadline is refused with the
  deadline in the error.
- `vamd` adds a fifth sweeper next to the four in `manager.rs` and
  `api/mod.rs` (approvals, active duration, keep-warm, gate timeouts):
  `sweep_wait_deadlines`, driven by the wait row's `due_at`.

## 7. Watchdogs

| Watchdog | Rule (starting values, tuned by the benchmark) | Action |
|---|---|---|
| No progress across wakes | N wakes (default 5) where the workspace tree hash, the todo file and the outbox (ignoring the host's heartbeat lines) are unchanged and no tool call wrote anything | park `budget` with reason `no_progress`; notify |
| Spend velocity | money spent in the last 10 min above 3 times the task's average rate, or more than 25% of the money budget in one turn | Pause; notify with the numbers |
| Heartbeat | every 15 min of active time, or every wake, the host posts a system line to the outbox: turns, active time, spend, todo done/total, workers live | none (it is the heartbeat) |
| Stall | a running task with no turn boundary for 2 step timeouts | treated as a crash (Phase 4 resume) |
| Loop detector | on for every long task, whatever the profile | as today (`harness.rs:259`) |

**Stalls become notifications.** Every watchdog action, every
`budget` park and every `deadline_reached` end goes to the notification module
(decision 9, existing `on_paused` hook), addressed to the task's owner,
or the App owner for a shared App (owner answer of 2026-11-28).

**Unattended finish notifies.** A task that parked at least once, ran
longer than 10 minutes of active time, or was started by an App key or a
trigger leaves a "finished" record with its receipt digest, extending the
rule of `background-run-end-notifies-and-chat-turn-does-not`.

## 8. Background processes and workers

### 8.1 Background processes (first)

`bg_start {command, cwd?, max_seconds?, max_output_bytes?}` returns an id;
`bg_status {id}`, `bg_output {id, from?}`, `bg_kill {id}`; the model is told
in the next turn when a process exits (design §IV.3 "Background
processes"). Rules:

- A process is a guest process in the task's VM. It goes through the same
  Cedar check as `run`. Direct egress from it is outside the effect
  guarantee (decision 13).
- Per-process wall-clock cap (default 30 min, ceiling = the active-time
  budget) and capture cap (default 4 MiB, kept as a workspace file under
  `.vam/bg/<id>.log`, the tail shown to the model).
- At most 4 live processes per task.
- **A task with a live process does not park** on a wait; active time
  counts while it runs. When the active-time meter is spent with a process
  live, `vamd` stops the process (its captured output stays in
  `.vam/bg/<id>.log`) and the task parks `budget` (§4). The wall-clock
  deadline ends the task and its processes. `bg_wait {ids, timeout}` lets the model block cheaply without a
  model call, but the VM stays up.
- A process dies with its VM. After a crash resume the model is told
  "process <id> was lost in a restart; its log up to the last commit is in
  `.vam/bg/<id>.log`". It is never restarted automatically.

### 8.2 In-VM workers (`spawn_agent`)

Decision 4: same VM first, with the parent's permissions and budget.
Decision 5: at most 4 live, depth 1.

| Tool | Does |
|---|---|
| `spawn_agent {role, brief, files?, tools?, budget?}` | starts a worker loop in the same guest; returns `worker_id` |
| `worker_status {ids?}` | state, turns, spend, last progress line |
| `worker_send {id, text}` | a steer to the worker, delivered at its next turn boundary |
| `worker_stop {id}` | stops the worker at its next boundary; its files stay (no per-worker rollback in v1) |
| `wait_for_workers {ids, deadline}` | returns results as they finish; parks the task (`waiting_for: workers`) only if every live worker is itself waiting |

- **Own lane, own context.** Each worker writes lane `worker/<id>` with
  `parent_lane` = the orchestrator's lane in the thread log
  (`thread_log.rs:396-398` already has the fields). It starts from its
  brief and the named files, not the parent's conversation, and compacts on
  its own.
- **Tools are a subset** of the parent's (the parent may narrow; never
  widen). Every call still goes through the PEP as the agent. Effect
  journal call ids are prefixed with the worker id so they stay unique per
  task.
- **Depth 1:** `spawn_agent` is not in a worker's tool list. Default 2 live,
  cap 4 (decision 5); the agent may raise the default up to the cap through
  `request_budget`.
- **Progress** goes to the orchestrator's inbox as tagged steers
  (`[worker w2] 40/100 done`), coalesced to at most one per worker per
  orchestrator turn, and to the outbox with the worker tag.
- **Result** is structured: `{summary, files[], status: done | failed |
  stopped | budget}`; at most 4 KiB of summary, files by path.
- **Shared workspace.** Each worker gets a scratch dir
  `.vam/workers/<id>/`. Writes outside it need a **host-side write lease**
  on a path prefix, given at spawn (`files` and `leases`) or by the
  orchestrator later; a write outside any lease the worker holds fails
  with a typed `write_lease_denied {path, holder}`. The orchestrator holds
  every path not leased to a worker.
- **One commit sequence.** Turn commits stay per task: a commit is taken at
  the orchestrator's turn boundary and covers every worker's files. A Stop
  rolls back the orchestrator and every worker together (decision 12, v1).
- **Approvals park only the worker.** A worker that hits `ask` waits; the
  others and the orchestrator go on. The approval card names the worker
  and its role.
- **Recovery.** After a crash or a restart, the orchestrator resumes from
  its last commit (Phase 4), and each worker unfinished at that commit is
  re-spawned with its lane history replayed up to the commit; the effect
  journal answers its recorded calls (`REPLAYED`) and never re-sends an
  unknown one.
- **Model.** Workers use the task's model in v1. A cheaper model from the
  same allowlist is a later option (open question 2).

Child-VM workers with attenuated permissions and workspace fork and merge
stay later (tracker "Explicitly later"; they need A16 and Drive9 merge).

## 9. What the Apps pilot must tell us

The pilot (Apps lane: one internal app, one client) runs on today's limits.
Before any Phase 5 breakdown, the orchestrator reads these signals from
the pilot's run rows, events and receipts. No new code is needed to read
them; a short query script over the node's database is enough.

| Signal | Read from | Decides |
|---|---|---|
| **Task length**: turns, active minutes and wall-clock per run (p50, p90, max) | run rows, receipts | whether budgets matter at all; the operator ceilings |
| **Cap hits**: runs ended by max iterations or `DURATION_STOP_ERROR`, and whether the caller retried | run errors, events | slice 5a (park and budgets) first, or not |
| **Background vs chat**: share of runs started by an App key or a trigger with no human in the thread, and how long until anyone reads the result | run source, thread reads, notifications | Pause, unattended-finish notification, heartbeat |
| **Fan-out need**: tasks that are N independent items (issues, tickets, files) where wall time matters to the caller | task text, todo lists, tool-call shape | slice 5d (workers) now or later; long single job vs fan-out first |
| **Waiting on the outside world**: runs that poll (repeated identical reads, `sleep` in `run`), and what they wait for | tool calls, loop detector observations | timer waits (5c) or straight to the E5 broker-side condition |
| **Slow commands**: `run` calls near the 300 s step timeout | tool timings | background processes (5b) |
| **Compactions per run**, and failures after a compaction | `summary` lane records | repeated-compaction benchmark priority; `NOTES.md` |
| **Workspace contention**: messages queued behind a busy Space, by App | inbox `started` delays | whether "one live task per workspace" blocks the pilot |
| **Approvals in long runs**: how many, who answered, how long they waited | approvals table | worker approval routing; approval wait default |
| **Cost per run** and its spread | receipts, `SessionBudget` totals | spend-velocity thresholds; money meter defaults |

**Decision rule (proposed).** Build 5a if more than 5% of pilot runs hit a
cap or run unattended longer than 10 minutes. Build 5b if slow commands
appear in more than one pilot task. Build 5c only for checks no broker can
express; otherwise move the E5 slice forward. Build 5d only if a pilot task
has independent items and its owner needs it faster than one loop can do
it. 5e ships with whichever of 5a to 5d ships first.

## 10. Locked decisions (proposed, owner to confirm)

These add to the tracker's list; none changes an existing entry.

- **LW1.** A task parks between turns for any open wait (approval,
  question, reauth, timer, workers, pause, budget), after keep-warm (a
  timer wait longer than 30 s parks at once), with no VM and no
  active-time clock. (Extends decision 10's "park after
  keep-warm".)
- **LW2.** Task budgets have five meters (turns, active time, money,
  wakes, wall-clock deadline). Starting values are decision 5's; ceilings
  are operator (and App) settings; today's hard cap 200 becomes the default
  turn ceiling. The agent raises within the ceiling; only a human extends
  past it. A spent budget parks the task (a spent active-time meter first
  stops live background processes, keeping their output); the wall-clock
  deadline ends it.
- **LW3.** Every task has a wall-clock deadline (default 7 days, maximum 30)
  and every timer wait has a maximum number of wakes.
- **LW4.** Pause parks at the next turn boundary and keeps files,
  conversation and queued steers; Stop is unchanged.
- **LW5.** "One live task per workspace" stays; a parked task keeps its
  reservation. Concurrent jobs use separate Spaces.
- **LW6.** Workers: own lane and context, a subset of the parent's tools,
  a slice of the parent's budget, depth 1, default 2 live and cap 4, a
  scratch dir plus host-side write leases, one shared commit sequence, an
  approval parks only that worker.
- **LW7.** Background processes never keep a task alive past its active
  time (a spent meter stops them and parks the task) or its wall-clock
  deadline (the deadline stops them and ends the task
  `deadline_reached`), and are never restarted after a crash.
- **LW8.** Stalls, budget parks and watchdog actions always notify the
  task's owner (the App owner for a shared App).

## 11. Phases

Phase 5 is built in slices, each with its own breakdown (3 to 8
scenarios) and its own exit. The order follows the brainstorm of
2026-11-26; §9 may drop or reorder slices. Each slice depends on Phase 4
being in `main`, and on agent access being in `main` where it touches the
agent spec. Hot files (proposal Q16, owner to confirm): `manager.rs` and the agent spec
proto are shared with the Apps lane; a slice that edits them waits for the
lane that holds them.

### 5a — Park and task budgets

Park for any wait; the five meters with `request_budget`; budget park with
Extend; Pause; wall-clock deadline; unattended-finish notification;
recitation on for long tasks; `NOTES.md` re-read after compaction.

**Exit:** the 50+ call triage bench task (`u4-triage`, new) passes on the
benchmarked models with k = 5 without a cap ending it; leave and return
works in the app; a parked task holds no VM.

| Scenario | Change | Layer | Given | Why |
|---|---|---|---|---|
| `task-at-its-turn-budget-parks-and-continues-when-extended` | new | process | org-admin, agent-with-shell | budget parks, not ends |
| `agent-raises-its-budget-within-the-operator-ceiling` | new | process | org-admin, agent-with-shell | `request_budget` and its refusal above the ceiling |
| `paused-task-keeps-its-files-and-queued-messages` | new | process | org-admin, agent-with-shell | Pause is not Stop |
| `unattended-long-task-notifies-when-done` | new | process | org-admin, agent-with-shell | leave and return |
| `notes-file-is-reread-after-compaction` | new | process | org-admin, agent-with-shell | context as a cache |
| `vm-parked-task-resumes-on-a-new-vm-with-its-budget` | new | firecracker | org-admin, agent-with-shell | park frees the VM |
| `long-triage-finishes-within-its-budget` | new | process (bench) | org-admin, agent-with-shell, ticket-backlog (new world piece: 200 seeded issues) | exit; `u4-triage` |

### 5b — Background processes

`bg_start`, `bg_status`, `bg_output`, `bg_kill`, `bg_wait`; exit notice;
per-process caps; lost-after-crash notice.

**Exit:** a build-and-test task that runs a 10-minute command passes
without a step timeout; a crash loses the process and the model is told.

| Scenario | Change | Layer | Given | Why |
|---|---|---|---|---|
| `background-command-runs-while-the-agent-keeps-working` | new | process | org-admin, agent-with-shell | the tools |
| `agent-is-told-when-a-background-command-exits` | new | process | org-admin, agent-with-shell | exit notice |
| `background-command-past-its-time-limit-is-killed` | new | process | org-admin, agent-with-shell | caps |
| `background-command-lost-in-a-crash-is-reported-not-rerun` | new | process | org-admin, agent-with-shell | LW7 |

### 5c — Timer waits

`wait_until` with and without `recheck`; wakes meter; the deadline
sweeper; a steer ends a wait early.

**Exit:** a timer wait resumes the task (design §VII.3 exit); a re-check
stops at its maximum wakes; no VM runs between wakes longer than 30 s
(they park at the turn boundary with no keep-warm hold).

| Scenario | Change | Layer | Given | Why |
|---|---|---|---|---|
| `timer-wait-parks-the-task-and-resumes-it` | new | process | org-admin, agent-with-shell | exit |
| `recheck-backs-off-and-stops-at-its-maximum-wakes` | new | process | org-admin, agent-with-shell | LW3 |
| `wait-past-the-task-deadline-is-refused` | new | process | org-admin, agent-with-shell | LW3 |
| `message-during-a-timer-wait-wakes-the-task` | new | process | org-admin, agent-with-shell | inbox wakes a parked task |
| `timer-wait-survives-a-vamd-restart` | new | process | org-admin, agent-with-shell | deadlines are rows |

### 5d — In-VM workers

`spawn_agent`, `worker_status`, `worker_send`, `worker_stop`,
`wait_for_workers`; lanes; leases; per-worker approvals; recovery.

**Exit:** a fan-out triage (two workers, 50+ calls in all) passes with
k = 5; a kill mid-fan-out finishes with 0 duplicate effects and 0 lost
workers.

| Scenario | Change | Layer | Given | Why |
|---|---|---|---|---|
| `agent-fans-out-to-two-workers-and-merges-their-results` | new | process | org-admin, agent-with-shell | the tools, lanes |
| `worker-write-outside-its-lease-gets-a-typed-error` | new | process | org-admin, agent-with-shell | shared workspace |
| `worker-approval-parks-only-that-worker` | new | process | org-admin, agent-with-guarded-tool | LW6 |
| `worker-cannot-start-a-worker` | new | process | org-admin, agent-with-shell | depth 1 |
| `stop-rolls-back-the-agent-and-every-worker` | new | process | org-admin, agent-with-shell | decision 12 |
| `workers-resume-after-a-crash-without-repeating-effects` | new | process | org-admin, effects-test-broker, agent-with-effect-tools | Phase 4 guarantees per lane |

### 5e — Observability and watchdogs

Worker rows and per-lane cost in the run view and receipt; heartbeat;
no-progress and spend-velocity watchdogs; stall as crash.

**Exit:** a seeded runaway (same check every wake, nothing changes) parks
within 5 wakes and notifies; if 5d is built, per-lane cost sums to the
task's cost; otherwise a single-lane task reports its cost as one lane.

| Scenario | Change | Layer | Given | Why |
|---|---|---|---|---|
| `task-with-no-progress-across-wakes-parks-and-notifies` | new | process | org-admin, agent-with-shell | watchdog |
| `spend-spike-pauses-the-task-and-notifies` | new | process | org-admin, agent-with-shell | watchdog |
| `long-task-posts-a-heartbeat` | new | process | org-admin, agent-with-shell | leave and return |
| `worker-cost-shows-per-lane-and-sums-to-the-task` | new | process | org-admin, agent-with-shell | observability; only if 5d is built |
| `long-task-posts-progress` | unchanged | process | — | still guards the outbox |

**Benchmark delta:** `u4-triage` (50+ calls, single loop, task set 0.4.0);
`u4-triage-fanout` (with workers, only if 5d is built);
`u4-repeated-compaction` (at least five compactions; goal, early
correction and todo state checked). Rows report pass^k, cost per
success, compactions, parks and wakes.

### 5f — Later (not Phase 5)

Child-VM workers with attenuated permissions and workspace fork and merge;
cheaper worker models; event waits and broker-side conditions (E5 slice);
probes.

## 12. What we are not building

- A general workflow engine or DAG of tasks. One task, many turns.
- Standing schedules in `vamd` (decision 16; they belong to `vam.schedule`).
- Per-worker rollback or per-worker commits in v1.
- Workers that spawn workers, or workers in other VMs.
- Parallel live tasks in one workspace.
- Automatic restart of background processes.
- A raise past the operator ceiling by the agent alone.

## 13. Open questions (each with a recommended answer)

1. **Long single job or fan-out first?** Recommend the long single job
   (5a to 5c) first: it is what the 50+ call exit measures and what every
   fan-out needs underneath. The pilot decides (§9).
2. **Cheaper model for workers?** Recommend allowed later, only from the
   task's model allowlist, chosen by the agent per worker and shown on the
   worker row. Not in v1.
3. **Do worker approvals park only the worker?** Recommend yes (LW6). The
   task parks only when every worker and the orchestrator wait.
4. **Shared rollback in v1?** Recommend yes: one commit sequence, a Stop
   rolls back everyone (decision 12). Per-worker rollback needs Drive9
   merge.
5. **Wall-clock deadline default?** Recommend 7 days, maximum 30 days
   (the gate bound). The pilot's wall-clock p90 may lower it.
6. **Active-time ceiling default?** Recommend 4 h per task; operators may
   raise it.
7. **Who may Extend a budget in a shared App?** Recommend the App owner or
   ops (same routing as approvals in shared Apps, owner answer of
   2026-11-28); the caller's end user later, with end-user approvals.
8. **Does a long job's workspace lock need relief before Drive9 merge?**
   Recommend no; LW5 plus "use another Space" until the pilot shows
   contention (§9 "Workspace contention").
9. **Should `NOTES.md` be a tool rather than a convention?** Recommend a
   convention first (no new tool, no new Cedar resource); revisit if the
   repeated-compaction benchmark shows models ignore it.
10. **Do background processes count as active time?** Recommend yes: the
    VM is running. `bg_wait` avoids model cost, not VM cost.
