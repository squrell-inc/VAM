# Agent runtime: agents that finish the job

Status: **confirmed** by the product owner on 2026-10-15: Part VIII answers,
the placement in §VII.2, and the rule changes, with security kept first.
Tracker: `.cursor/workstreams/agent-runtime.md`. Part IX (how the guest
runtime, `vamd` and HSP combine, and waits and triggers) and the E5 move
confirmed 2026-10-15. Trigger wire details live in
[`hsp-triggers.md`](hsp-triggers.md), amended the same day.
Proposes epic **E10 Agent runtime** (problems P12, P13), a hotfix for
workspace identity, and where E10 sits in the epic order. Builds on
[`agent-experience.md`](agent-experience.md) (P1–P11, E1–E8) and extends
the park-and-resume model in [`playbooks.md`](playbooks.md).

Every finding below comes from reading `main`. None has been
measured on a live run yet. The first issue of Phase 0 measures them
(§VII).

## What we are building

The VAM boundary works: a microVM per run, Cedar on every call, and no
secrets in the guest. The work done inside the boundary does not. Users
connect a tool and the agent can't use it without coaching. It can't look
anything up on the web. It forgets what it did between messages, can't
take a message while it works, and gives up after 5 turns or 120 seconds.

> A secure environment that cannot do the job it was designed for makes
> no sense. — product owner, 2026-10-14

E10 makes VAM agents **finish the job**: the first time, across follow-ups,
over long tasks, and without breaking anything when a machine stops. It
does this in the harness and runtime, so it works for any model the
broker serves.

## Why

`agent-experience.md` lists eleven adoption problems. None covers "the
agent is bad at using what it was given" or "the agent can't do real,
ongoing work." P5 (reliability) measures outcomes but does not own the
loop. P7 (work outside chat) owns triggers and delivery, not the
execution underneath. Two problems are missing:

| # | Problem, as a customer would say it | Who feels it |
|---|---|---|
| **P12** | "We connected the tool and it still doesn't know how to use it, and it can't look anything up." | Everyone who connects an MCP or skill |
| **P13** | "It can't do real work: it forgets, I can't talk to it while it works, long tasks die, and a crash can break things." | Anyone using agents for more than a one-shot question |

P12 and P13 hit **every run of every user**. P6 (E8) hits users whose
skills use standard HTTPS clients. P4 (E2) matters once agents are worth
sharing. This shapes the placement in §VII.

---

# Part I — The users' problems

Eight situations users actually hit. Each one says what the user does,
what happens today, what good looks like, and how we'd know. The approach
(Part IV) is chosen from these, not the other way round.

### U1 — "I connected Notion; save my draft there"

**Who:** any user with an MCP connection.
**Today:** the tools sit behind an opaque `load_skill mcp-notion` (A1). The
server's own usage guide is dropped (A2), and its tool descriptions are cut
to 512 characters (A3). When a call fails, the model sees only "mcp tool
returned an error" (A4). Five turns are about the minimum the task needs
(A6). The developer ends up writing the how-to into the agent's
instructions.
**Good:** works in plain words, first try, with no instructions about
Notion in the agent.
**Measure:** pass^k on "save this draft to Notion under X", agent
instructions empty, per benchmarked model.

### U2 — "What changed in X this week?"

**Who:** anyone asking about something current or outside the org.
**Today:** no web search or fetch (A9). `curl` is blocked until E8.
**Good:** searches, reads sources, answers with citations. The search key
never enters the VM.
**Measure:** pass^k on a cited-research task; zero secrets in the guest.

### U3 — "No, the other page. And add tags."

**Who:** everyone, on every follow-up.
**Today:** each message boots a new VM. It gets the last 10 turns as
flattened "User: … Assistant: …" text (A12), with no tool results, no
files read and no plan, so the agent redoes lookups or guesses. A message
sent while it works is rejected (A10).
**Good:** the follow-up continues from exactly where the agent was. A
message sent while it works is taken into account at the next step.
**Measure:** a two-message task where message 2 depends on IDs found in
message 1. A mid-run correction task.

### U4 — "Go through these 200 issues and triage them"

**Who:** operations, support, engineering: real delegated work.
**Today:** the run dies at 5 turns (A6) or 120 seconds (A13). Results older
than three turns are replaced by "re-run" (A5). The user sees nothing until
the end (A14) and can't leave and come back.
**Good:** the agent works for as long as the task needs, within a budget.
It shows progress, keeps its context clean, and tells the user when it's
done or needs them.
**Measure:** pass^k on a 50+ call task. Context never overflows. Progress
messages arrive. Leave-and-return works.

### U5 — "The machine crashed. Did it create the invoice twice?"

**Who:** anyone whose agent changes real systems. Security teams (P1).
**Today:** a crash loses the run. The workspace is committed only at
session end, and a failed commit **discards** all the run's file work
(A18). Nothing records which external effects already happened, so a
retry can repeat them.
**Good:** a stopped or crashed VM never loses finished work and never
repeats an external effect. Unknown outcomes are checked, not guessed.
**Measure:** a kill-mid-task benchmark task. Duplicate effects must be 0,
lost tasks must be 0.

### U6 — "Two agents, both with a workspace called main"

**Who:** any user with more than one agent.
**Today:** workspace IDs are `{user}--{name}`, with no agent in them, and
every agent's default workspace is `{user}--main` (A17). Whether this
mixes files or blocks runs was not checked.
**Good:** workspaces never collide.
**Measure:** a scenario with two agents on the same name.

### U7 — "Teach it once"

**Who:** team leads and power users.
**Today:** every run starts from zero. What worked yesterday isn't kept.
**Good:** personal facts ("drafts go under Drafts/2026") are remembered
and visible. Proven ways of using a connection are reused across the org
after review.
**Measure:** the second Notion task takes fewer turns than the first.

### U9 — "Hi" starts the purchase-requisition procedure

**Who:** anyone whose agent has a playbook attached.
**Today:** the app attaches the agent's default playbook to the **first
message of every thread**, whatever it says (A20). Other attached
playbooks can't be started from chat (A21). A playbook can't start partway
through a thread (A22). Playbooks have no "when to use" field (A23).
**Good:** a playbook starts when the request matches its purpose ("help me
draft a PR", "let's create a purchase requisition"), at any point in a
thread, and then runs its journey to the end. "Hi" or an unrelated
question stays plain chat.
**Measure:** benchmark pairs: matching requests start the right playbook;
non-matching ones start none. Zero playbooks started by injected text.

### U8 — "Is it actually getting better? Which model should we use?"

**Who:** platform owners and admins.
**Today:** no harness benchmark. Model choice is guesswork. The direct
Anthropic provider is a stub (A8).
**Good:** a published benchmark per model and per harness change, compared
with Claude Code and Codex on the same tasks.
**Measure:** the benchmark exists and gates harness changes.

### What users are not asking for (yet)

These came up in brainstorming but don't map to a frequent problem today.
They are deferred, not dropped (§IV.4):

- **Code mode** (scripts that chain tool calls). It helps U4 and cost
  (P8) on data-heavy tasks; a plain loop with files-as-memory covers most
  of U4 first.
- **Child-VM sub-agents with narrower permissions.** A strong security
  story (P1, P9), but no user is blocked on it before in-VM sub-agents
  exist.
- **Timers and wake on events.** These belong to E5 (work outside chat)
  and build on E10's parking.

---

# Part II — What the code does today

### II.1 Tool use (P12)

| # | Finding | Where |
|---|---|---|
| A1 | MCP tools are only reachable after `load_skill mcp-<id>`, described only as "Tools for connection <id>". | `vam-orchestrator/src/mcp_registry.rs:1657` |
| A2 | The MCP server's `instructions` are not deserialized. Resources and prompts are unsupported. | `vam-orchestrator/src/mcp_client.rs:64` |
| A3 | MCP descriptions are capped at 512 characters with newlines removed. | `mcp_registry.rs:1320`, `:1325` |
| A4 | OAuth MCP errors are replaced by "mcp tool returned an error". | `vam-orchestrator/src/host_tools.rs:1338` |
| A5 | Only the text parts of MCP results are kept; results are capped at 4 KB; after three turns large results become "not on disk — re-run"; MCP results never become files. | `host_tools.rs:1499`; `vam-runtime/src/agent.rs:1720`, `:1865` |
| A6 | Default turn budget is 5 (hard cap 100). | `agent.rs:703` |
| A7 | Hard-coded nudges from earlier demos (`"cga-"`, `jira-integration`, "call the first loaded tool"). | `agent.rs:1116`, `:1207` |
| A8 | The direct Anthropic provider is a stub (`"messages": []`, `max_tokens: 1024`). Real traffic uses the OpenAI-compatible path: text-only messages, no prompt caching, temperature 0.7 by default. | `vam-inference/src/provider.rs:730`, `:400` |
| A9 | No web search or fetch. Built-ins: file tools, `glob`, `grep`, `run`, `todo_write`, `load_skill`. | `vam-runtime/src/primitives.rs:635` |

### II.2 Execution (P13)

| # | Finding | Where |
|---|---|---|
| A10 | One live session per workspace; a second message is rejected with `AgentRunning`. | `vam-orchestrator/src/api/mod.rs:1108` |
| A11 | Every run terminates its VM at the end. | `api/mod.rs:2629` (`finalize_run_session`) |
| A12 | Follow-ups get the last 10 turns as flattened text. | `vam-storage/src/vam_dir.rs:540` |
| A13 | Guest task timeout is 120 s when the run has no active budget. | `api/mod.rs:2991` |
| A14 | `ExecuteTask` is one request, one reply: no progress messages or questions mid-run. | `proto/vam/v1/vsock_rpc.proto:99` |
| A15 | No sub-agents; `run` blocks until the command exits. | `primitives.rs` |
| A16 | One golden-restored session at a time (shared CID 3); others cold-boot. | tracker log, 2026-10-14 |
| A18 | The workspace is an ext4 image committed to Drive9 only at session end. If the commit fails, the session's work is discarded. | `vam-storage/src/workspace.rs:447`; `vam-orchestrator/src/manager.rs:1959`, `:1966` |
| A19 | On restart, `vamd` adopts live Firecracker processes. Whether an in-flight run's result is recovered was not checked. | `vam-orchestrator/src/reconcile.rs:91` |
| A20 | The app sends the agent's `defaultPlaybookRef` with the first message of every thread, whatever the message says. Later messages are plain runs. | `the VAM app/src/types/vamPlaybookInputs.ts:237`; `hooks/useVamChat.ts:178` |
| A21 | Other attached playbooks (`allowedPlaybookRefs`) can't be started from chat; they only gate API runs. | `the VAM app/src/components/vam/hooks/useVamChat.ts:172` |
| A22 | A playbook is bound when the run is created. Nothing can start one partway through a run or thread. | `proto/vam/v1/workflow.proto:196` (`RunSpec.playbook_ref`) |
| A23 | `PlaybookSpec` has `description` but no "when to use" or trigger field. | `proto/vam/v1/workflow.proto:177` |
| A24 | The approval checkpoint is capped at 48 KiB (one 64 KiB vsock frame), pretty-printed, and stores `messages[0]` (the whole system prompt) even though it is rebuilt every run. The same JSON is sent back inline in the resume task, so it hits the frame limit both ways. Approvals fail after two or three chat turns ("HITL continuation is 75176 bytes; max is 49152"). | `vam-runtime/src/hitl_checkpoint.rs:28`, `:135`; `vam-orchestrator/src/api/mod.rs:3161` |
| A25 | `vamd` never reads the agent's `reasoningPolicy`. The app applies `maxIterations` (default 15) to plain chat only; playbook, CLI and API runs fall to the runtime default of 5. `stepTimeout` and `termination` are read nowhere. | `vam-orchestrator/src/run_context.rs` (`merge_agent_into_inputs`); `the VAM app/src/components/vam/dashboardHelpers.ts:277` |
| A26 | `vamd` honours a run's `maxDuration`, but the app never sends one and the agent spec has no duration, so every app run gets the 120 s default. | `vam-orchestrator/src/api/mod.rs:3029` |
| A27 | MCP call timeouts come from a resource tier chosen by matching the text `heavy` or `light` in policy ids: 30 s by default, 60 s for heavy. No setting per connection. | `vam-orchestrator/src/security.rs:1268`; `host_tools.rs:450` |
| A28 | `McpTool` keeps only name, description and input schema (drops `title`, `annotations`, `outputSchema`). `cap_output` truncates at the output limit without saying so. | `vam-orchestrator/src/mcp_client.rs:50`; `host_tools.rs:217` |

### II.3 Workspace identity

| # | Finding | Where |
|---|---|---|
| A17 | Workspace ID is `{user}--{name}` in the app and `vamd`, with no agent. The app checks duplicates only within the active agent. Every agent defaults to `{user}--main`. | `the VAM app/src/components/vam/dashboardHelpers.ts:561`; `hooks/useVamChat.ts:443`, `:1432`; `vam-orchestrator/src/run_context.rs:46` |

### II.4 What already exists to build on

- **Park and resume** (`playbooks.md`): a conversation checkpoint
  (`HitlContinuation`: messages, loaded skills, counters, pending call)
  uploaded and digest-verified on the host; resume on a fresh VM with a
  higher lifecycle generation; secret placeholders stable per run; spend
  per run; a workspace reservation while parked; a keep-warm sweeper.
  This is most of a durable task, limited to human waits.
- **The host sees every host-mediated call** (MCP, HSP, host exec): PEP
  intent, execution, and the trajectory outbox (`trajectory.rs`).
- **Drive9 manifests carry a parent hash** (`workspace.rs:447`), so
  workspace history is a chain in a content-addressed store. Cheap forks
  look possible (a new HEAD pointing at an existing manifest); merging is
  not built.
- **The guest is a real machine** with Bun and `vam-runtime` as PID 1,
  which already supervises processes.
- **The MCP `needs_reauth` error contract** (`host_tools.rs:1057`) already
  tells clients when a connection needs the user.
- **Evaluations**: `evals.proto`, `vam playbook test` with pass^k, and
  the E9 harness.

---

# Part III — Compared with Claude Code and Codex

### III.1 On features, VAM would reach parity, not a breakthrough

| Capability | Claude Code | Codex | VAM today | VAM with E10 |
|---|---|---|---|---|
| Uses a connected tool without coaching | Yes | Yes | No (A1–A4) | Yes |
| Web search | Yes | Yes | No | Yes, key on host |
| Follow-ups keep full context | Yes | Yes | No (A12) | Yes |
| Messages while working | Yes | Partly | Rejected (A10) | Steer, queue, stop |
| Long tasks | Session lifetime | Cloud task | 5 turns / 120 s | Budgeted |
| Sub-agents | Yes | Parallel tasks | No | In-VM first |
| Background processes | Yes | Yes | No | Yes |
| Memory | Instruction files | `AGENTS.md` | No | Facts, reviewed recipes |
| Any model | Claude only | OpenAI only | Several (weakly) | Several, benchmarked |

### III.2 Where VAM loses

- **Models are trained on their vendor's harness.** Claude models fit
  Claude Code's tools, OpenAI models fit Codex's. A model-agnostic
  harness pays a tax. Mitigation: model profiles (§IV.1), measured.
- **Latency.** VM boot, the vsock hop and a policy check per call make
  each step slower. VAM must win on outcomes, not speed.
- **Provider-only API features** arrive at the provider first.

### III.3 Where VAM can be genuinely better

These need a runtime that owns **the machine and the path every call
takes**, which no provider harness does:

1. **No lost work:** a task survives the VM crashing, timing out or being
   reclaimed.
2. **No duplicate effects:** on resume, external effects already done are
   not repeated, and unknown outcomes are checked (§V.1). This is the
   hardest and most valuable guarantee.
3. **Learning across the org:** the host sees every successful run;
   reviewed recipes help every model next time.
4. **Waiting costs nothing:** a parked task holds no VM and no tokens.
5. Later: **least-privilege sub-agents** in their own VMs, and **code mode
   under policy**, where every call a script makes is still authorized and
   audited.

**Verdict:** parity first, then guarantees. The claim to earn, with
benchmark numbers (Part VI), is **agents that finish the job, never
lose work and never do anything twice, on any model.**

---

# Part IV — The approach, problem by problem

Each feature names the problem it serves. A feature that serves none
waits.

### IV.1 First-try tool use (U1, U8)

| Feature | What it does | Fixes |
|---|---|---|
| Direct MCP tools | Connection tools are listed directly with their full descriptions and the server's `instructions`. No `load_skill` step for MCP. | A1, A2, A3 |
| Fencing instead of truncation | Vendor text (descriptions, instructions, results) is marked as untrusted data in the prompt and scanned for injection patterns. The 512-character cap is lifted. Drift re-approval still applies. | A3 |
| Tool ranking | With many tools, the harness ranks them per task (text ranking first) and shows the top ones, plus `find_tools` for the rest. Anthropic reports MCP accuracy going from 49% to 74% on Opus 4 with tool search; this works for any model. | A1 |
| Call repair | Arguments are checked against the JSON schema before sending. Schema errors go back to the model. Vendor errors are **redacted, not blanked**: every secret value known for that attempt (`attempt_secrets`) is removed, and the raw body stays host-side for audit. Bounded retry for 429/5xx. | A4 |
| Results as files | Any result over a threshold is written to the workspace; the model gets a preview and the path. Compaction always has a path to point to. | A5 |
| Sensible budgets | Turn budget and task time from the agent's spec, with defaults that fit real tasks (§VIII.4). Keyword nudges removed; a completion check driven by the plan replaces them. | A6, A7, A13 |
| Real model layer | A working Anthropic adapter; structured content (images, reasoning blocks) instead of text only; a stable, deterministic prompt prefix so caching works; model profiles (context size, native tool calls, conventions) validated by the benchmark. | A8 |

### IV.2 Current information (U2)

| Feature | What it does |
|---|---|
| Web broker | First-party `web_search` and `web_fetch` as an HSP broker. One search provider adapter first (§VIII.6). The key stays on the host. Fetch runs on the host through egress policy and the SSRF guard, turns HTML into markdown, and writes the page to a workspace file with its URL. Results are fenced as untrusted. |

### IV.3 Continuity, steering and long work (U3, U4, U5)

| Feature | What it does | Fixes |
|---|---|---|
| Thread context | A thread keeps a chain of **turn checkpoints** (generalized `HitlContinuation`). A follow-up resumes from the last one: real messages, compacted, not flattened text. | A12 |
| Turn commits | At each turn boundary, the conversation checkpoint and a workspace snapshot are recorded together (§V.2). A failed commit keeps the image and retries; it never discards. | A18 |
| Effect journal | Every host-mediated external effect is recorded write-ahead; resume replays recorded results and reconciles unknown ones (§V.1). | U5 |
| Crash resume | After a VM crash or `vamd` restart, a running task resumes on a new VM from its last turn commit. | A19 |
| Inbox | A message during a task is a **steer** by default, delivered at the next turn boundary. **Stop** interrupts. **Queue** runs after the task (§V.3). | A10 |
| Outbox | A `post_message` tool and RPC stream progress, partial results and questions to the app. A question puts the task in `waiting(input)`. | A14 |
| Park between turns | When a task waits on the user, the VM stays warm for a while, then parks. Waiting costs nothing. | A11 |
| Background processes | `run` with `background: true`; `bg_status`, `bg_output`, `bg_kill`; the model is told when a process exits. | A15 |
| In-VM sub-agents | A `spawn_agent` tool: another loop in the same VM with its own context and the same permissions. It shares the workspace and the task budget, and returns a summary and files. | A15, U4 context |
| Todo recitation | The todo list is restated at the end of the context each turn, keeping the goal in recent attention. | U4 |

### IV.3a Playbooks start when the request matches (U9)

A playbook is a job contract. It should start when the user asks for that
job, not because it is attached.

| Feature | What it does | Fixes |
|---|---|---|
| Activation field | `PlaybookSpec.activation`: `when` (one or two sentences: "Use when the user wants to draft or open a pull request"), optional `examples` (sample requests), and `mode`: `ask` (default) or `auto`. Shown on the playbook page and in the marketplace. | A23 |
| `start_playbook` tool | The agent sees the playbooks attached to it (default and allowed), with their `when` text, ranked like tools (§IV.1). It calls `start_playbook(urn, inputs)` when a request matches. The host checks: attached to this agent, inputs valid against `input_schema`, Cedar allows `start_playbook`. Then it binds step state to the **current task** and adds the procedure to the prompt; guided execution runs as today. | A20, A21, A22 |
| Ask before starting | In `ask` mode the chat shows a card: "Start *Draft a PR*?" with the input form prefilled from the request. Confirm, edit, or decline. `auto` is for read-only or low-risk playbooks. A playbook with a mutable step is always `ask` when the request came from untrusted content (web, MCP, event payload). | U9 safety |
| Explicit start | A playbook picker in the composer (and `/playbook-name`), the CLI `--playbook`, and triggers (a run grant that captures a `playbook_ref`, `hsp-triggers.md` open question 4) still start one directly, with no matching. | A21 |
| Default playbook, redefined | `default_playbook_ref` means "this agent's main job": it ranks first, is the target for API and trigger runs that name none, and is highlighted in the composer. It no longer starts on its own in chat. | A20 |
| One journey at a time | A task runs at most one playbook. A different matching request mid-journey asks: finish, pause, or abandon the current one (journaled). After it finishes, the thread returns to plain chat. A journey continues across follow-ups, which needs Phase 2's thread context (today a follow-up is a plain run unless the playbook is parked at a gate). | — |

Security: starting a playbook never grants a permission. Guards and gates
only restrict, Cedar still decides every call, and the agent can only
start playbooks attached to it.

**When:** an app-only quick fix goes with the workspace hotfix: stop the
automatic start on the first message, and add the composer picker for
default and allowed playbooks. The activation field, `start_playbook`,
the ask card and binding mid-run are E10 Phase 0, the same problem as
choosing tools. Continuing a journey across follow-ups lands with
Phase 2.

#### IV.3b Playbooks in flight: forms, captured values, guidance (owner, 2026-10-16)

A playbook is a job contract the model starts and then follows step by
step, collecting what it needs from the user through forms. The existing
step model carries most of it: task steps (`bound`, `assisted`,
`agentic`, optional `output_schema`), gate steps (`question`,
`response_schema` form, `options` branches, `choices_from` select fed by an
earlier step's output), `after: "step:outcome"` branching, and
`steps.<id>.output` as the captured values.

Worked example, Oracle purchase requisition (`when`: "Use when the user
wants to raise a purchase requisition in Oracle"; no start inputs):
1. `mode` gate: "Guided, or attach an invoice?" options `guided`, `invoice`.
2. Invoice path: `get-invoice` gate with a file field, then
   `extract-invoice` agentic task (`output_schema`: supplier, lines,
   amount).
3. Guided path: `supplier-name` gate (text).
4. `find-suppliers` bound task: supplier search with the name from 2 or 3.
5. `pick-supplier` gate: `choices_from: steps.find-suppliers.output.suppliers`
   (select); captures the supplier id.
6. More gates and tasks (items, cost centre, need-by date).
7. `confirm` gate: summary of the captured values; approve or change.
8. `create-pr` bound task with arguments from the captured values;
   `evidence` requires the create tool's success.

Additions:

| # | Rule |
|---|---|
| G1 | `activation.when` is required on every playbook at publish. No fallback, no backward compatibility. |
| G2 | A playbook may have no start inputs: `start_playbook` takes `{}` and the ask card shows only "Start <name>?". |
| G3 | Guidance-only playbooks: zero steps plus a `guidance` text that is added to the conversation on start; nothing is tracked; the run ends normally. |
| G4 | Form elements: text, long text, number, boolean, single select, multi-select, select from `choices_from`, date, and file (a workspace upload; the value is the workspace path). One shared schema form renders playbook inputs, gate forms and the ask card. |
| G5 | Gate defaults may be templated from earlier step outputs (`{{steps.<id>.output.<path>}}`), so an "extract from the request" step can prefill a gate the user only confirms. No new tool. |
| G6 | Playbook status block: after every step event the host returns a status (steps done, captured values, steps available now, gate awaited). It is delivered after the cached prompt prefix. |
| G7 | Procedure rule for the model: information a step needs from the user is collected by a gate; do not ask for it in chat and never invent values. |
| G8 | Templates resolve in bound-step arguments, so the final action takes its arguments from the captured values. |
| G9 | A journey resumes from a parked gate (up to its timeout); continuing across plain follow-ups stays Phase 2. |

More scenarios used as examples and tests: expense claim (receipt file,
extraction, policy check, manager gate only above a limit, submit), support
refund (order id input, eligibility outcomes, amount gate prefilled,
refund), release-notes style guide (guidance only), vendor onboarding
(multi-select categories, contract date, W-9 file, create).

### IV.4 Learning and later work (U7, deferred items)

| Feature | Serves | When |
|---|---|---|
| Personal facts | U7: a memory tool, scoped to user × agent, visible and editable | E10 late phase |
| Reviewed recipes | U7: the host proposes a recipe per connection from successful trajectories; the connection owner approves it before the org uses it | E10 late phase |
| Code mode | U4 at scale, P8: typed MCP clients in the VM, each call checked by policy and journaled | Later; needs the journal |
| Child-VM sub-agents | P1, P9: own VM, permissions a strict subset of the parent's, workspace fork and merge | Later; needs A16 fixed and Drive9 merge |
| Timer waits (`wait_until`, backoff re-checks) | U4 | E10 Phase 5 (§IX.4) |
| Event waits (`wait_for`), standing schedules, Slack/email | P7 | E5, right after E10 Phase 6, on top of parking, the inbox and the outbox (§IX.4) |

---

# Part V — Reliability

### V.1 Effect journal

**Scope.** The journal covers effects that go through the host: MCP, HSP
and host-exec tools. Effects inside the VM (files) are covered by turn
commits (§V.2). Traffic a guest process sends directly through the egress
proxy (`curl` in a script) is **outside the guarantee**: the host sees
bytes, not intent. Code mode (later) routes calls through the host, so
they are inside it.

**Rule 1: decide, persist, act.** Today a crash after the model chose its
tool calls means the turn is redone, and the model may choose different
calls. So the order is:

1. The model returns tool calls.
2. The assistant message with those calls is persisted (as part of the
   turn checkpoint) **before** any call is sent.
3. Each call is journaled `intended`, then `sent`, then `confirmed`,
   `failed` or `unknown`.
4. On resume, the conversation already holds the calls. For each call
   ID: `confirmed` or `failed` → inject the recorded result; `intended` →
   send it; `unknown` → reconcile (below).

The model is never re-asked for a decision that was already acted on.

**Rule 2: classes decide retries.**

| Class | Examples | `unknown` outcome |
|---|---|---|
| Read | search, fetch, list | Re-send. |
| Idempotent | PUT by ID, upsert, vendor idempotency key | Re-send with the same key. Exactly once in effect. |
| Non-idempotent | create page, send email, comment | **Never re-sent blindly.** Run the tool's declared **verify** read if it has one ("find page by title"). Otherwise tell the model the outcome is unknown and let it check. Otherwise ask the user. |

The class comes from the manifest. Without a declaration it is inferred
from `side_effect`: read-only → Read, anything else → Non-idempotent, the
safe default.

**Rule 3: stable keys.** Idempotency keys are derived from task ID, call
ID and an arguments hash, keyed with a host secret. They are stable across
lifecycle generations, the same approach as stable secret placeholders.
They are passed to vendors that support one (HTTP `Idempotency-Key`, MCP
`_meta` when the server honours it).

**Approvals.** A call waiting for approval is journaled `intended` with
the approval ID. On approval it's sent, on denial it's `failed`. The
approval and the effect can't drift apart.

**Code mode, when built.** A script is re-run from the start on resume.
Each SDK call carries an ordinal and an arguments hash. A call that
matches a `confirmed` entry returns the recorded result; the first one
that doesn't match stops the script and hands control back to the model
(it diverged, for example on time or randomness). This is
deterministic replay, as in durable-workflow engines. An approval inside
a script parks the task; on resume, the script replays up to that call.

**Guarantees, stated plainly:**

| Thing | Guarantee |
|---|---|
| A turn | At least once (may be redone after a crash) |
| A model decision already acted on | Never re-asked |
| Read effects | Any number of times; harmless |
| Idempotent effects | Exactly once in effect |
| Non-idempotent effects | At most once, plus a reconcile step when the outcome is unknown |
| Direct egress from guest processes | No guarantee (documented) |

### V.2 Turn commits: conversation and files together

**Today:** the workspace is an ext4 image committed only at session end,
and a failed commit discards the work (A18).

**Proposed:** at each turn boundary where a tool that writes files ran,
take a **turn commit**:

1. The guest syncs and freezes the workspace filesystem (`fsfreeze`),
   which takes milliseconds.
2. The host takes a copy-on-write copy of the image file (or a Firecracker
   drive snapshot), then the guest thaws.
3. The host extracts the copy into a Drive9 manifest in the background.
4. The turn commit is **complete** when the conversation checkpoint
   digest and the manifest hash are both recorded against the turn.

Resume uses the latest complete turn commit. The rule for users: **inside
the VM, work rolls back to the last complete turn; outside the VM,
nothing is rolled back and nothing is repeated** (V.1). Files and
conversation always match.

A failed commit keeps the image, retries, and marks the task `failed
(storage)` with an alert if it still fails. It never discards. Disk
pressure (the `/tmp/vam-drive9` fill seen on Lima) becomes a visible
error, not lost work.

Cost to measure in Phase 2: freeze time, copy time per GB, extraction
time. If extraction per turn is too heavy, extract only at park and
session end, and keep image copies in between.

### V.3 Inbox semantics

- **Injection points are turn boundaries only**, after every tool result
  of the turn is appended. Tool calls and results stay paired.
- **Ordering:** the host numbers messages per task. At a boundary, every
  pending steer is delivered together, in order, as one user message.
- **Steer while parked:** wakes the task; delivered at the first boundary.
- **Steer while an approval is pending:** the approval stays open. The
  card says the message will be delivered after the decision. To change
  course, the user presses **Stop**, which denies the pending call with
  "stopped by user".
- **Stop during a host call:** the call can't be safely cut mid-flight,
  so it finishes and is journaled, then the task stops. A local `run`
  process is killed.
- **Queue:** becomes the next task on the thread after the current one
  ends.
- **Duplicates:** the client sends an idempotency key per message. The
  guest acknowledges by sequence number. The last acknowledged number is
  in the checkpoint, so redelivery after resume is ignored.

### V.4 Failure modes

| Failure | Today | With E10 |
|---|---|---|
| VM crash during a model call | Run lost | Turn redone from the last commit; no effects yet |
| VM crash after tool calls chosen, before sending | Run lost | Calls sent on resume (Rule 1) |
| VM crash after a write was sent, no reply | Run lost; a retry may duplicate | `unknown` → reconcile per class |
| `vamd` restart during a task | Live VMs adopted; result recovery not checked (A19) | Tasks are rows; resumed on a new VM from the last commit |
| Workspace commit fails (disk full) | Work discarded (A18) | Kept, retried, alerted |
| Provider outage or 429 | Run fails | Backoff, then another model within the operator's allowlist, else `waiting` with the reason |
| MCP timeout on a write | Error to the model | `unknown` → reconcile |
| OAuth `needs_reauth` | Error text | `waiting(input)` with a reconnect card; continues after reconnect |
| Corrupt checkpoint | Digest check (approvals only) | Digest check every turn; fall back to the previous complete commit |
| Budget reached | Run ends | Task parks with "budget reached"; the user can extend and continue |
| Many steers in a burst | Rejected | Merged at the next boundary |
| Injection through web or MCP text | Truncation as a partial guard | Fenced as data; Cedar still decides every call |
| Node full | Not handled | Task waits in `queued` instead of failing |

### V.5 Capacity

In-VM sub-agents and background processes add no VMs. Parking frees VMs
while users are away, so capacity improves for long tasks. Child VMs
(later) need A16 fixed and a per-node limit with a `queued` state.

---

# Part VI — The benchmark

A standing track, like the E9 harness: each phase adds its tasks to its
exit criteria.

- **Baseline first.** Phase 0 starts by running the tasks on today's
  `main`, to confirm Part II's findings with numbers.
- **Tasks**, fixed and versioned: U1 Notion draft (empty agent
  instructions), U2 cited research, U3 two-message follow-up and mid-run
  correction, U4 a 50+ call triage, U5 kill-mid-task, U6 two agents on
  one workspace name, U7 a repeat Notion task.
- **Models:** benchmarked tier (§VIII.7).
- **Metrics:** pass^k, turns and tokens per success, cost per success,
  cache hit rate, duplicate effects (must be 0), lost tasks (must be 0).
- **Comparison:** the same tasks in Claude Code and Codex where they can
  run them, published next to VAM's.
- **Gate:** a harness change that lowers pass^k is flagged (the same
  rule E4 proposes for releases).

---

# Part VII — Placement and phases

### VII.1 Constraints

- The owner builds **one phase at a time**, not in parallel.
- Current order: E1 Phase 7, then E8 (plan, Phases 2–3), then E7
  Phase 1, E2, the rest of E7, E3, E5, E4, E6.
- E8 Phase 2 and E7 Phase 1 both change the guest image. E10 changes
  `vam-runtime`, which is also in the image, so these must not overlap.
- E2 needs E1 Phase 7's registry domain.

### VII.2 Recommendation

1. **Now, before E1 Phase 7: the workspace hotfix (U6).** It's a
   data-integrity bug that grows with every agent created. First confirm
   on Lima what happens (mixed files or a blocked run). Then: the
   workspace ID includes the agent, `vamd` enforces uniqueness, and
   existing workspaces are migrated. One vam issue and one the VAM app issue,
   not a phase.
1b. **Then, still before E1 Phase 7: the H1 runtime hotfix** (added
   2026-10-15 from team reports). Approvals break after a few chat turns
   (A24), every app run is killed at 120 s (A26), playbook and API runs
   stop at 5 turns (A25), and slow MCP tools time out at 30 s (A27).
   These are correctness bugs in what is already on `main`. Fixes:
   - **Checkpoints by reference:** stop storing `messages[0]` (rebuild
     it on resume from the run inputs and loaded skills), compact JSON,
     upload in chunks, keep it on the host, and pass only its digest in
     the resume task. Host cap 4 MiB. This is the transfer E10 Phase 2's
     turn checkpoints will reuse, so it is built once.
   - **`vamd` owns run budgets:** every run kind gets `maxIterations`
     and `stepTimeout` from the agent's `reasoningPolicy`, with locked
     decision 5's defaults (50 turns, hard cap 200; 30 minutes active
     time) when unset. The runtime enforces `stepTimeout` per round. The
     app stops sending its own iteration count.
   - **MCP timeout per connection:** a `timeoutMs` on the connection,
     default 120 s, maximum 600 s, used for MCP calls instead of the
     text-matched tier.
2. **E1 Phase 7 as planned.** It's small, closes E1, and unblocks E2.
   Moving E10 ahead of it gains little and leaves E1 open.
3. **E10 Phases 0–3 next, before E8.** P12 and P13 hit every run of
   every user; E8 helps users whose skills use HTTPS clients. A shared
   agent that can't finish the job makes E2 worth little too. After
   Phase 3, VAM is at parity with Claude Code and Codex on the everyday
   experience.
4. **Then E8 and E7 Phase 1**, as already ordered. Long tasks (U4) need
   Python and git in the base (E7) and standard HTTPS (E8) as much as they
   need the runtime.
5. **Then E10 Phases 4–6.**
6. **Then E5's getting-started slice** (§IX.4): "wait for the PR to merge,
   then continue" and "every day at 6 PM" are the journeys that make the
   runtime visible. Confirmed 2026-10-15.
7. **Then E2 and the rest** (the rest of E7, E3, E4, E6).

```
hotfix U6 ─▶ H1 ─▶ E1 P7 ─▶ E10 P0 ─▶ P1 ─▶ P2 ─▶ P3 ─▶ E8 ─▶ E7 P1 ─▶ E10 P4 ─▶ P5 ─▶ P6 ─▶ E5 slice ─▶ E2 ─▶ …
                      first try  web  context steer                never   long  learns  waits on
                                                                  twice   work           events,
                                                                                         schedules
```

Confirmed by the owner on 2026-10-15: E10 before E8, E10's later phases
and E5's slice before E2.

### VII.3 Phases

Phases follow the house rules: 3–6 issues each, in waves, with a Mac
check for anything that needs a real microVM. Each exit names its
benchmark tasks.

| Phase | Outcome for users | Contents | Depends on | Exit |
|---|---|---|---|---|
| **0 First try** | U1, U8, U9 | Baseline benchmark run; direct MCP tools with instructions and fenced full descriptions; call repair (schema check, redacted errors, bounded retry); results as files; budgets from the spec with real defaults; nudges removed; working Anthropic adapter; playbook activation (§IV.3a) | E1 P7 | Notion task passes on the benchmarked models with empty instructions; better than the baseline; playbook pairs start the right playbook or none |
| **1 Web** | U2 | Web broker (search + fetch), one search provider, results as fenced files, egress and SSRF checks on fetch | 0 | Cited-research task passes; no key in the guest |
| **2 Context** | U3 (follow-ups), part of U5 | Turn checkpoints as the thread's state; follow-ups resume real context; compaction by summary; turn commits (§V.2); commit failure never discards | 0 | Two-message task passes; files and conversation match after a forced stop between messages |
| **3 Steer** | U3 (mid-run), U4 visibility | Inbox (steer, stop, queue) replacing `AgentRunning`; outbox (`post_message`, questions → `waiting(input)`); app chat shows progress and accepts messages mid-run | 2 | Mid-run correction task passes; progress shows in the app |
| *E8, E7 P1* | | | | |
| **4 Never twice** | U5 | Effect journal (decide, persist, act; classes; verify reads; stable keys); crash and `vamd`-restart resume; provider fallback; `needs_reauth` → waiting | 2, 3 | Kill-mid-task: 0 duplicate effects, 0 lost tasks |
| **5 Long work** | U4 | Park between turns with keep-warm; task budgets replace fixed caps; background processes; in-VM sub-agents; todo recitation; timer waits (`wait_until`, backoff re-checks, §IX.4); notify when done (existing `on_paused` hooks) | 4 | 50+ call triage passes; leave and return works; a timer wait resumes the task |
| **6 Learns** | U7 | `vam.memory` first-party broker (§IX.3): personal facts, then recipes proposed from trajectories and approved by the connection owner | 4 | Repeat Notion task uses fewer turns |
| *E5 slice* | P7 | Credentials (AS-1.1, AS-1.2), the trigger gateway (`hsp-triggers` Phases 0–3) plus resume delivery, `wait_for`, `vam.schedule` and a GitHub event broker (§IX.4) | E10 P5 | PR-merge resume and a daily schedule work end to end |
| *Later* | P1, P8, P9 | Code mode; child-VM sub-agents with attenuation and workspace fork/merge; tool ranking with embeddings | 4, A16 | — |

**Additions from team reports (2026-10-15):** Phase 0 also honours
`reasoningPolicy.termination` (an explicit "done" claim), keeps MCP
`title`, `annotations` (`readOnlyHint` feeds the side-effect class) and
`outputSchema`, and says when a result was truncated. Phase 2's turn
checkpoints use H1's by-reference transfer and carry a size budget.

**Getting-started slice:** Phases 0–3. Before them, Phase 0 alone is the
smallest step that shows users a difference.

---

# Part VIII — Recommended answers to the open questions

For the owner to confirm. None is locked.

1. **Is a job per thread or per message?** Recommend: **the thread holds
   durable context; a task is the unit of work.** A task is today's run
   (budget, effects, attestation, metering stay per task). A message
   while a task runs steers that task. A message while idle starts a new
   task on the thread, resuming from the thread's last turn checkpoint.
   This keeps the run model, metering and audit as they are, and avoids
   tasks that never end.
2. **Default for a mid-run message?** Recommend **steer**, with **Stop**
   and **Queue** as explicit buttons. Most mid-run messages are
   corrections or additions to the current task; Claude Code behaves the
   same way.
3. **Child-VM sub-agents in the first slice?** Recommend **no.** In-VM
   sub-agents in Phase 5; child VMs later, once the journal exists and
   A16 is fixed.
4. **Budget defaults?** Recommend starting points, to be tuned by the
   benchmark: **50 turns** (hard cap 200), **30 minutes active time** per
   task, cost capped by the operator's existing budgets, keep-warm 5
   minutes (as in playbooks), in-VM sub-agents at most 4 live with depth
   1. Agents can raise their own within operator limits.
5. **Recipes automatic or reviewed?** Recommend: **personal facts apply
   automatically** and are visible and editable. **Recipes are proposed
   by the host and approved by the connection owner** before anyone else
   uses them, like a release.
6. **Search provider?** Recommend **bring your own key through the web
   broker**, with one adapter first. Provider built-in search (through
   the model API) runs outside VAM's egress policy and audit, so it's not
   the default.
7. **Which models are supported?** Recommend two tiers. **Benchmarked:**
   Claude Haiku 5.5 (already the E2E model), Claude Sonnet 5.5, one OpenAI
   model, and one open model through Ollama. **Works:** anything the broker
   serves, with no benchmark claim.
8. **Delivery for long tasks?** Recommend **the app, plus the existing
   `on_paused` hooks**. Slack and email stay in E5, on top of the outbox.
9. **Does Drive9 support cheap workspace branches?** Likely: manifests
   have a parent hash in a content-addressed store, so a fork is a new
   HEAD on an existing manifest. Merge is not built. Only needed for
   child VMs (later); verify when that phase is planned.

---

# Part IX — One experience: guest runtime, vamd and HSP

Parts IV–V describe features. This part says which layer owns each one,
so the three layers feel like **one machine** to the user and to the model.

### IX.1 The model: a machine with a kernel and drivers

VAM is a *Virtual Agentic Machine*. Read it like an operating system:

| Layer | OS analogy | Owns | Never owns |
|---|---|---|---|
| **Guest runtime** (`vam-runtime`) | User space | The agent loop, the workspace, local processes, sub-agent loops, scripts. Thinks and does. | Secrets, identity, timers, listeners, anything that must outlive the VM |
| **`vamd`** | Kernel | Identity, Cedar, the **task** (checkpoints, effect journal, inbox, outbox, waits, budgets), the one tool catalog, the inference broker, the sweepers | Vendor SDKs, vendor credentials beyond the vault, public listeners, standing schedules |
| **HSP brokers** | Drivers | The outside world: tools (web, files), context hooks (memory, knowledge), clocks (schedules), event sources (GitHub, Slack), delivery (Slack, email) | Identity, policy, task state |

**The placement rule for any new capability:**

1. Needs a vendor SDK, a vendor credential, a network listener or a clock
   for standing work → **an HSP broker**.
2. Decides who, whether, or what survives a VM → **`vamd`**.
3. Thinks, edits files, runs code → **the guest**.

A guest never talks to a broker directly. Everything crosses `vamd`, so
policy, audit, redaction and the journal apply to every path.

### IX.2 Five planes through `vamd`

The user-facing experience is these five planes working together. Each
plane is one interface in `vamd`, whatever is behind it.

| Plane | One interface | Sources behind it | Built in |
|---|---|---|---|
| **Tools** | One catalog per session, ranked per task, one call path (PEP → journal → executor) | Built-ins, skills, MCP connections, HSP broker tools | E10 P0 (direct MCP), P4 (journal) |
| **Models** | The inference broker, with model profiles | Anthropic, OpenAI-compatible, Ollama | E10 P0 |
| **Context** | What goes into the prompt each turn: system prompt, ranked tools, compacted history, todo, memory, hook excerpts | Turn checkpoints, HSP `on_request` hooks (`vam.memory`, `vam.rag`) | E10 P2, P6 |
| **Messages** | Inbox and outbox per task | The app, later HSP delivery brokers (Slack, email) | E10 P3, E5 |
| **Time and events** | Waits per task (timer, event, approval, input, child) with deadlines | `vamd` sweepers (timers), HSP brokers (schedules, events) | E10 P5, E5 |

Today these exist in pieces: MCP tools are seeded as skills, HSP tools are
host-exec adapters, approvals have their own sweeper, gate timeouts
another. Unifying each plane is what makes behaviour consistent. For
example, a tool's call repair, redaction and journaling are the same
whether it comes from MCP or HSP.

### IX.3 First-party brokers

The first-party brokers E10 and E5 need, all on the existing HSP contract:

| Broker | Plane | Does | Phase |
|---|---|---|---|
| `vam.web` | Tools | `web_search`, `web_fetch`; key on the host; pages saved as fenced workspace files | E10 P1 |
| `vam.memory` | Context, Tools | Facts per user × agent through memory tools and an `on_request` hook. Recipes proposed from `on_complete` trajectory summaries, approved by the connection owner | E10 P6 |
| `vam.schedule` | Time | Standing schedules ("every day at 6 PM") through the `hsp-triggers` gateway | E5 |
| `vam.github` | Time and events | Webhook or polling watcher: PR merged, CI finished, review requested; conditions evaluated broker-side | E5 |
| `vam.notify` | Messages | Delivers outbox messages and `on_paused` notices to Slack and email | E5 (later stories) |

Putting memory in a broker rather than in `vamd` keeps `vamd` small, uses
the hook contract that already exists, and lets an org swap the backend
(for example a vector store) without touching the kernel. `vam.files` and
`vam.rag` stay as designed in `docs/src/content/docs/hosted/first-party.md`.

### IX.4 Waits and triggers

**Who owns what:**

| | Guest | `vamd` | Broker |
|---|---|---|---|
| A task waits for time | Calls `wait_until(time)` | Holds the deadline, parks the task, wakes it | — |
| A task waits for an event | Calls `wait_for(source, filter, deadline)` | Cedar check, stores the wait on the task, issues a **resume grant**, delivers the event to the task's inbox | Watches the vendor, fires `DeliverEvent` when the condition is true |
| A standing schedule | Calls `schedule.create` (a broker tool) | Issues a **run grant**, starts a task on fire | Stores the schedule, owns the clock |

`vamd` stores **no standing schedules**. It holds **deadlines for waits
that belong to a task**, as it already does for approvals, gate timeouts,
keep-warm and active duration (four sweepers in `manager.rs` and
`api/mod.rs`). This keeps `hsp-triggers.md`'s rule ("brokers decide when")
for standing work.

**Example: wait for a PR to merge, then continue**

```
guest:   wait_for(github.pull_request.merged, repo, pr=42, deadline=3d)
vamd:    Cedar (may this agent subscribe to github?) → vam.github.watch(...)
         → wait stored on the task, resume grant issued to the broker
         → task parks, VM shuts down (costs nothing)
broker:  PR 42 merged → DeliverEvent(resume grant, event id, payload)
vamd:    verify grant, dedupe event id → event into the task's inbox
         → new VM resumes from the last turn commit → the agent continues
deadline passes first → the wait's on_timeout: resume with "timed out",
         notify the owner, or fail (same pattern as playbook gates)
```

**Polling: the cheapest thing that can check does the checking.**

1. **An event**, where the vendor sends one. The model wakes once.
2. **A broker-side condition** ("CI status for this commit is not
   pending"). The broker polls the vendor, with no model and no VM, and
   fires only when the condition holds.
3. **A timer with backoff** (1, 2, 5, 10 minutes, capped, maximum wakes)
   for checks no broker can express. Each wake is a short check turn.
   This is E10 Phase 5's `wait_until` with a retry policy.
4. Later: a **probe**, a small script run in a fresh VM without the model,
   that wakes the agent only when it succeeds.

**Security rules:**

- **A subscription is a permission.** Cedar decides which sources an
  agent may subscribe to. It shows on the permissions page and can be
  `ask`.
- **A resume grant is narrower than a run grant:** one task, one wait,
  revoked when the wait resolves or the task ends. Same Credential store
  and hashing as run grants (AS-1.1, AS-1.2).
- **Event payloads are untrusted** (a PR title can carry an injection) and
  are fenced like web and MCP text.
- **Limits:** a deadline on every wait; maximum open waits per task and
  per user; deduplication by event ID; per-broker rate limits (existing
  gateway checklist).
- **A trigger for a thread with a running or parked task** goes to its
  inbox (steer or queue), not `BUSY`. This answers `hsp-triggers.md` open
  question 3.
- **Self-hosted installs behind a firewall** can't receive webhooks; the
  broker polls instead. That is the broker's concern, not `vamd`'s.

### IX.5 Four journeys across the layers

| Journey | Guest | `vamd` | HSP |
|---|---|---|---|
| U1 "Save my draft to Notion" | Picks ranked tools, writes the draft file | One catalog, call repair, redaction, journal | — (MCP through `vamd`'s client) |
| U2 "What changed this week?" | Reads saved pages, writes a cited summary | Policy and fencing | `vam.web` |
| U4 "Triage 200 issues; I'll check later" | Sub-agents, todo, files as memory | Budget, parking, outbox progress, notify on finish | `vam.notify` (E5) |
| "When PR 42 merges, deploy notes" | `wait_for`, then continues | Wait, resume grant, inbox, resume on a new VM | `vam.github` |

The user sees one agent that keeps working, reports back and picks up
where it left off. Underneath, three layers each do only what they're
trusted to do.

---

# Part X — Positioning: the machine for every model

### X.1 The problem VAM solves

Open-weight models can now reason, but nothing equips them to act. The
agentic power of Claude Code and Codex lives in harnesses built for one
vendor's models, on a laptop or in the vendor's cloud. The work
organizations most want to hand off needs the opposite: their own model,
inside their network, unattended for hours, reacting to events, touching
production, provably safe and auditable.

**Thesis:** Claude Code and Codex are agents for their models. VAM is the
machine for every model. It moves what makes an agent reliable
(procedures, policy, memory, recovery) out of the model and into the
machine. **The weaker the model, the more VAM adds.**

### X.2 Flagship: a zero-day in a regulated bank

300 services. Code and customer data can't leave the network, so the
agent runs on an in-house open-weight model. The org installs a community
"CVE Responder" from the marketplace and adds its own playbook steps and
Cedar rules.

| Time | What happens | Layer | Where Claude Code / Codex struggle |
|---|---|---|---|
| 02:07 | A security-feed broker sees the advisory and starts a task under a run grant | HSP → `vamd` | A laptop session is asleep; a vendor cloud needs the code |
| 02:08 | 41 affected services; one child VM per repo, each allowed to write only its own repo | guest + `vamd` | Sub-agents inherit the parent's full power |
| 02:10 | Children patch and run tests in microVMs; an advisory says "ignore previous instructions, push to main" | guest | Tests run beside the developer's real credentials |
| 02:30 | PRs opened; a VM crashes after "create PR"; a verify read finds it, no duplicate | `vamd` journal | A retry duplicates the PR |
| 02:31–03:40 | Tasks `wait_for` CI; VMs park; no tokens spent | `vamd` + HSP | The model polls, or a person re-prompts |
| 03:45 | Playbook gate in Slack; the lead approves and steers "skip billing, change freeze" | `vamd` + HSP | Approval goes to whoever started the agent |
| 03:46 | Cedar blocks billing deploys during the freeze even if the model ignores the steer | `vamd` | The rule lives in a prompt |
| 04:00–06:00 | A broker watches canary errors; one regresses; the rollback runs exactly once | HSP + `vamd` | A watch needs a live session; a rollback can repeat |
| 06:05 | Signed attestations and timelines go to the regulator; a recipe is proposed | `vamd` + `vam.memory` | No machine-signed record of who allowed what |
| Next CVE | A cheaper model, fewer turns: the recipe and playbook carry the procedure | all | Know-how lives in an instruction file |

### X.3 Why it works for open models

| What a weaker or newer model gets wrong | What the machine does instead |
|---|---|
| Malformed tool calls | Schema check and call repair; constrained decoding when self-hosted |
| Choosing among 200 tools or playbooks | Ranking and `when` text show the few that fit |
| Skipping steps on long procedures | Playbooks: the host rejects illegal transitions |
| Losing the thread over hours | Turn checkpoints, files as memory, todo recitation |
| Being talked into things | Cedar on every call, no secrets in the VM, fenced text |
| Repeating a mistake on retry | The effect journal |
| Not knowing "how we do it here" | Reviewed recipes and playbooks |

**Honest line:** for one developer on a laptop with a frontier model,
Claude Code and Codex stay better. VAM wins when two or more of these
hold: the org's own model, inside its network, long or unattended work,
touching production, must be audited, shared across an org.

### X.4 Enhancements this suggests

In the tracker's backlog; each gets scoped when its phase is planned.

| Enhancement | Why | Target |
|---|---|---|
| Constrained decoding and tool masking for self-hosted models | Invalid tool calls become impossible; a Cedar-denied tool can't even be generated. Needs control of the inference server, which closed APIs don't give | E10 Phase 0 (model layer) |
| Escalation ladder | Routine turns on a small model; planning, or a turn that fails repair twice, goes to a stronger allowed model | E10 Phases 0 and 4 |
| Model profiles as marketplace packages, public VAM benchmark score | A new open model is supported on release day by the community; labs get distribution | E6 + benchmark |
| Safe replay for model swaps | The journal holds past results, so real tasks replay against a candidate model with no side effects, comparing decisions until they diverge | After E10 Phase 4 |
| Teach once, repeat cheaply | A frontier model's successful runs become reviewed recipes a small model follows | E10 Phase 6 |
| Child-VM sub-agents earlier | The flagship's fan-out needs least privilege per repo | After E10 Phase 5 |

### X.5 Launch lines

- **Organizations:** Claude Code– and Codex-class agents on any model,
  inside your walls, under your policy.
- **Model labs:** ship a model; get an agent runtime, a benchmark score
  and a marketplace of agents on day one.
- **Community:** publish an agent once; it runs on any model an org
  allows, under that org's rules.

Claim outcomes per cost plus sovereignty, measured, not "beats Claude".
The benchmark also publishes where VAM with an open model still loses.

## Rules this would change

Some of these may be locked in other trackers (not yet checked). Each
needs an explicit owner decision and an edit to that tracker's locked
list before building:

1. One run = one VM, always terminated at the end (A11). Becomes: park
   after keep-warm; a stopped VM never ends the task.
2. One live session per workspace; a second message is rejected (A10).
   Becomes: the inbox.
3. Follow-ups replay flattened text (A12). Becomes: turn checkpoints.
4. MCP tools only reachable after `load_skill` (A1).
5. MCP descriptions capped and OAuth errors blanked as security measures
   (A3, A4). Becomes: fencing and redaction.
6. A failed workspace commit discards the session's work (A18).
7. `hsp-triggers.md`: "`vamd` has no ticker and no schedule table" becomes
   "`vamd` stores no standing schedules and holds deadlines only for waits
   that belong to a task" (§IX.4). A trigger for a busy thread goes to
   the inbox instead of `BUSY`. Amended in `hsp-triggers.md` on 2026-10-15.

## Still to drill into, by phase

- **Phase 0:** the fencing format and injection scan, so lifting the
  512-character cap doesn't weaken P1.
- **Phase 2:** measured cost of freeze, copy and extraction per turn
  (§V.2); compaction prompt and its benchmark effect.
- **Phase 3:** the guest RPC shape (host push or guest poll) and the app
  chat design for progress and mid-run messages.
- **One live task per workspace (owner, 2026-10-27).** A workspace is one
  read-write image in one VM; a turn commit snapshots all of it and a stop
  rolls all of it back (decision 12), so two live threads on one workspace
  would roll back or capture each other's files. Phase 3 keeps one live task
  per workspace and queues a message on another thread instead of refusing
  it. Parallel activities use separate workspaces of the same agent; Phase 6
  memory carries personal facts across them. In-VM sub-agents (Phase 5) run
  inside the parent's task, so they keep the rule. Truly parallel threads in
  one workspace (per-thread overlays, fork and merge on commit) wait for
  Drive9 merge, with child-VM sub-agents.
- **Phase 4:** verify-read declarations in the skill and MCP manifests;
  which MCP servers honour idempotency keys.
- **Later:** child workspace merge and conflicts; capacity limits per
  node.

## What we are not building

- A new isolation model. The VM, vsock, Cedar and the E8 egress design
  stay as they are.
- Training or fine-tuning models.
- A general workflow engine. The task model is for agent runs only.
- Slack, email, schedules and webhooks (E5), beyond the outbox and
  parking they will build on.
