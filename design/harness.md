# The harness: levers, profiles, and the balance with the model

Status: **proposed** (2026-10-26). Standing track **H**, beside B (benchmark)
and IT (isolation truth). Opens `.cursor/workstreams/harness.md` when H0 is
planned. Extends `agent-runtime.md` Part IV (approach), Part X (the machine
for every model) and §X.4 (enhancements); supplies the configurations that
`benchmark.md` measures. Serves P5, P12, P13, P10.

`Decide: NOW — proof 3 (the table; every benchmark row needs a disclosed harness), cost 2 (switches in the existing loop, no new trust boundary)`

## What we are building

A harness that is **inventoried, switchable, measured and profiled**:

1. **Inventoried.** Every lever the machine controls is named, located in
   the code, and tagged with the evidence for it and the model class it is
   expected to help (§1).
2. **Switchable.** Each lever is a run-level setting with a default. The
   effective configuration is recorded on the run and printed on the
   receipt, so every benchmark row discloses its harness.
3. **Measured.** The benchmark runs an ablation grid: per task, per model,
   each lever on and off against a base, with k runs, confidence intervals
   and a variance decomposition between model and harness (§3).
4. **Profiled.** The result is a **harness profile per model class**, pinned
   in the model catalog and versioned like any artifact, that the community
   can contribute to with a benchmark row (§4).
5. **Recursive, later and offline.** A propose, probe, compose loop in the
   style of Harness² edits profiles against the benchmark, under review,
   never at run time (§6, H4).

> Model and harness are a pair. The balance point moves with the model, so
> the product is not one harness but a measured profile per model.

## Why

**The harness decides as much as the model, and which lever helps depends
on the model.** State as of 2026-10-26; sources and dates in
`research_notes/harness/` (secondary where the primary was unreachable):

| Finding | Number | Source |
|---|---|---|
| Harness-driven variance vs model-driven variance, same tasks | 7.8x; ranking reversals in 6 of 9 model-pair comparisons | "Stop Comparing LLM Agents Without Disclosing the Harness", arXiv 2605.23950, May 2026 |
| Same model, different harness | Sonnet 4.5: 68% under SWE-agent vs 34% under a generalist harness; GLM-5.2 59.8 to 72.6 across four harnesses | HAL (arXiv 2510.11977); Terminal-Bench 2.1 submissions |
| Lever effects are model-dependent | Planning +11.6 pts for a 30B model, about 30% cost saving only for strong ones; bash-only +3.6 pts and −53% cost for a 550B model, harmful for weaker ones; context management closes a 35.7-pt gap at 32k context | "An Empirical Study of Harness Design for Coding Agents", arXiv 2609.20804, Sept 2026, 176 matched settings |
| Harness edits alone, frozen open model, business tasks | up to +11.5 pts WorkBuddy-Bench, +4.5 JobBench, +11.8 pts LAB partial pass for frozen GLM-5 and Gemini 3.5 Flash | "Harness²: Recursive Agent Harnessing for an Open World", arXiv 2610.14224, Oct 2026, google-research/harness2 (Apache-2.0) |
| Tool search instead of a full catalog | 49 to 74 (Opus 4), 79.5 to 88.1 (Opus 4.5), about 85% fewer tokens | Anthropic, Nov 2025 |
| Context editing and memory | +29% and +39% on a 100-turn eval, −84% tokens | Anthropic, Sept 2025 |
| Tool-use examples in descriptions | 72% to 90% | Anthropic, Nov 2025 |
| More reasoning effort | lowered accuracy in 21 of 36 settings | HAL |
| Human in the loop placed well | GAIA 30.3% to 51.9% with about 10% help requests | Magentic-UI report, July 2025 |

**And VAM's loop is thin on the model-facing side.** The audit of `main` at
 (`research_notes/harness/vam_harness_audit.md`):

- The host is strong: Cedar at every call, the playbook machine (steps,
  guards, gates, evidence, finish check), HITL suspend and resume, the
  thread log and turn commits (E10 Phase 2), summary compaction with a
  host-verified cut, a host-owned cache plan.
- The loop is not: **no loop detection** (the old same-tool heuristic is a
  test stub, `agent.rs:1233`); **no schema check on the model's own tool
  calls, unparsable arguments silently become `{}`** (`agent.rs:605-612`);
  no tool ranking or search; no resampling or `tool_choice`; no constrained
  decoding; thinking deltas dropped; no reasoning-effort control; no todo
  recitation; a fixed prompt; the catalog has no Ollama or vLLM entry.
- **Almost nothing is switchable per run** beyond permissions,
  `reasoningPolicy` and the playbook. The bench cannot ablate a lever.

Without switches there is no attribution, and without attribution the
table in `benchmark.md` cannot say *why* a row moved.

## Locked decisions (proposed)

Do not re-litigate without editing this list.

1. **Every lever is a named switch with a default.** It lives in one
   `harness` struct on the run input, read where the loop builds its tool
   list, prompt and model request. The effective configuration is recorded
   on the run and printed on the receipt.
2. **Profiles are per model class, pinned in the catalog, versioned.** A
   run names its profile; a benchmark row names it too. Changing a profile
   is a release with a benchmark delta.
3. **A lever becomes a default only with a row.** No lever ships on by
   default without an ablation row on the benchmark showing its effect for
   the model class it is on for.
4. **`machine off` is the minimal functional loop, not a handicap.** Same
   tools, same Cedar, same caps and overflow protection; no playbook
   tracking, no ranking, no recitation, no summary compaction beyond what
   prevents overflow; the procedure as prose. (`benchmark.md` decision 2.)
5. **Errors are honest.** An invalid tool call never becomes `{}`. The model
   receives a typed, actionable error naming the field, and the repair
   counts in the metrics.
6. **The harness never loosens security.** Ranking, masking and deferral
   are presentation; Cedar remains the gate on every call; a masked tool is
   still denied if called. (E10 locked decision 1.)
7. **Model-specific handling lives in the catalog entry**, never in the
   loop: parser, thinking mode, `tool_choice`, strict or guided decoding,
   context budget, price.
8. **No self-modifying harness at run time.** Recursive harnessing runs
   offline against the benchmark and produces a proposed profile change for
   review. Runtime behaviour is what the pinned profile says.

## 1. The lever inventory

Eight layers, from the model boundary outward. "Today" is `main` at
. "Helps" is the expected beneficiary from the evidence: **W** weak
or small open models, **S** strong models, **B** both. Switch names are
proposals for the `harness` struct.

### L1 Model adapter (the model boundary)

| Lever | What | Today | Helps | Switch |
|---|---|---|---|---|
| Tool-call format normalisation | Map Hermes, Qwen XML, DeepSeek, GLM, Kimi formats to one call shape; vLLM `--tool-call-parser` per model | OpenAI-compatible and Anthropic only; text-JSON fallback (`text_tool_calls.rs`) | W | `adapter.parser` (catalog) |
| Thinking handling | Keep or strip reasoning content; disable thinking for models that emit tool calls inside it (Qwen3.5) | thinking deltas dropped (`provider.rs:1352`) | B | `adapter.thinking` (catalog) |
| `tool_choice` and strict schemas | `required` or named tool when the procedure demands a call; strict mode on vLLM | absent | W | `adapter.tool_choice`, `adapter.strict` |
| Guided decoding | Structural tags so an invalid call cannot be generated; known cost: can suppress the call and add per-token overhead | absent | W | `adapter.guided` |
| Resample on invalid | One re-sample at higher temperature before returning an error | absent | W | `adapter.resample_on_invalid` |
| Reasoning effort | Pass effort where the model supports it; HAL: more effort often lowers accuracy | absent | B | `adapter.effort` |

### L2 Tool surface

| Lever | What | Today | Helps | Switch |
|---|---|---|---|---|
| Names, descriptions, examples | Clear names; one example per tool; concise `response_format` | fenced descriptions, 4096-char cap, no examples | B | `tools.examples` |
| Ranking and search | Show the few tools that fit; defer the rest behind `find_tools`; BM25 or embedding | absent; whole Cedar-filtered list | B | `tools.rank`, `tools.deferred` |
| Masking vs removal | Keep the list stable for the cache, mask availability | Cedar removes; no masking | S | `tools.mask` |
| Side-effect classes in the surface | Tell the model which calls are mutating | present (`ToolSpec.side_effect`) | B | fixed |
| Stable ordering for cache | Deterministic tool order | present through the host cache plan | B | fixed |

### L3 Call hygiene

| Lever | What | Today | Helps | Switch |
|---|---|---|---|---|
| Schema validation and typed error | Validate the model's arguments in the guest; return the field and the fix | MCP only, host side; guest turns bad args into `{}` | W | `calls.validate` (default on) |
| Repair budget | Bounded repairs per call before giving up | absent | W | `calls.repair_budget` |
| Parallel calls | Execute independent read-only calls together | accepted, executed sequentially (`agent.rs:598`) | S | `calls.parallel` |
| Result caps and spill | Cap tool output, spill to a file with a pointer | present, 16 KiB with pointers | B | `calls.result_cap` |
| Verify-reads after writes | A read that confirms a mutation landed (feeds the journal) | absent (E10 Phase 4) | B | `calls.verify_reads` |

### L4 Context

| Lever | What | Today | Helps | Switch |
|---|---|---|---|---|
| System prompt altitude and size | Short principles over long rules; the April 2026 Claude Code regression came from prompt changes | fixed template v11 | B | `context.prompt_variant` |
| Progressive disclosure | Skills load their body on demand; docs grepped through an index | present | B | fixed |
| Summary compaction | Trigger, keep window, summary instruction | present (70% trigger, keep 3 rounds) | B | `context.compaction` (thresholds) |
| Clearing old tool results | Drop stale results, keep pointers | present (every 8 rounds) | B | `context.clearing` |
| File memory and notes | `PLAN.md`, `notes/`; Harness-1 and StateM show gains from externalised bookkeeping | prompt guidance only | W | `context.file_memory` |
| Todo recitation | Re-read the todo at every turn (Manus) | `todo_write` exists, no recitation | W | `context.recite_todo` |
| Cache-aware layout | Stable prefix, append-only, breakpoints | present (host cache plan) | B | fixed |
| Reminders | The three `SYSTEM.md` bullets (`improving-the-vam.md`) | present | B | `context.reminders` |

### L5 Control loop

| Lever | What | Today | Helps | Switch |
|---|---|---|---|---|
| Budgets | Iterations, step timeout, duration, spend | present | B | fixed caps |
| Stop conditions | No-tool reply, `task_complete`, finish check | present | B | fixed |
| Loop detection | Repeated identical call, A-B-A-B, repeated error, monologue; inject an observation, then stop | absent (Gemini CLI: 5/10/50 constants; OpenHands `StuckDetector`) | W | `loop.detect` (default on) |
| Planning step | A plan before acting; +11.6 for small models, cost only for strong | prompt guidance (`PLAN.md`) | W | `loop.plan_first` |
| Reflection or verification turn | A check turn before finish on long tasks | absent | B | `loop.verify_before_finish` |
| Escalation ladder | Routine turns on a small model, failed repairs or planning to a stronger allowed model | absent (`agent-runtime.md` §X.4) | W | `loop.escalate` |
| Context reset on long runs | Fresh session at 80 to 100k tokens for models that loop (Kimi K3 reports) | compaction only | W | `loop.reset_at_tokens` |
| Fresh-context helper (in-VM sub-agent) | A second model call with a clean context, same VM, same permissions, that reviews, verifies or summarises and returns a short result (Cognition's clean-context reviewer; Codex's review guardian; Anthropic: sub-agents return 1 to 2k-token summaries). Not delegation, not fan-out | absent | B | `loop.helper` (review, verify, summarise) |

### L6 Process (what the machine holds)

| Lever | What | Today | Helps | Switch |
|---|---|---|---|---|
| Playbook step tracking | Host-owned state, `step_complete` validation | present | W | `process.playbook` (off = prose) |
| Guards | Deny-only, keyed on step state, `args_within` | present | B | `process.guards` |
| Gates | Human decisions with schemas; park and resume | present | B | `process.gates` |
| Evidence | A step completes only with an observed call | present | B | `process.evidence` |
| Exit conditions | No success with missing steps | present | B | `process.exit_check` |
| Procedure rendering | How the procedure appears in the prompt; status block per step | present | B | `process.render_variant` |
| Activation | Model picks the playbook from `when` text | present | B | fixed |

### L7 Reliability

| Lever | What | Today | Helps | Switch |
|---|---|---|---|---|
| Checkpoints and turn commits | Conversation and files together | present (E10 Phase 2) | B | fixed |
| Effect journal | Decide, persist, act; idempotency keys | contract in place (E10 Phase 4); always on from this version: `reliability.journal` is fixed `true` and a run that names `false` is refused | B | `reliability.journal` |
| Park and resume | Waits cost nothing | present | B | fixed |
| Provider retry and fallback | Retry on 429 and 5xx; never on mutating calls; fallback within the allowlist | retry present, fallback absent | B | `reliability.fallback` |

### L8 Human loop

| Lever | What | Today | Helps | Switch |
|---|---|---|---|---|
| Ask placement | Ask early on ambiguity vs at the gate; Magentic-UI +71% relative with about 10% asks | gates and tool HITL only; `ask_user` is E10 Phase 3 | B | `human.ask_policy` |
| Structured asks | Jig forms instead of prose | E10 Phase 3 basic | B | `human.jig` |
| Approval saving | "Always allow" on drafts | present | B | fixed |

## 2. How we identify what to build

The order is literature, inventory, switches, ablation, interactions,
profiles, publication, recursion. Steps 1 and 2 are done
(`research_notes/harness/`). The rest is this track.

1. **Hypotheses from the literature.** For each lever a row: metric it
   should move (from `benchmark.md` §3), model class, expected sign, source.
   The §1 tables are the first draft; H0 writes them into the tracker.
2. **Inventory from the code.** Done; §1 "Today" column.
3. **Switches.** Every lever in §1 with a switch name becomes a field in the
   `harness` run input, read in the three places the loop decides: tool
   list, prompt, model request. Defaults reproduce today's behaviour
   exactly, so H0 changes no scenario.
4. **Ablation grid** (§3). One lever at a time against a base per model.
5. **Interactions.** For the top three levers on weak models, pairs.
6. **Profiles.** The configuration that wins per model class, pinned.
7. **Publication with disclosure.** Every row names task-set version,
   model, endpoint, profile version, k, and the harness switches, per the
   disclosure standard proposed in arXiv 2605.23950.
8. **Recursion, offline.** Harness²'s propose, probe, compose loop over
   profiles, with the benchmark as the probe and a reviewer, producing a
   profile PR (H4).

## 3. The ablation grid

**Tasks:** `close-prep` and `invoice-approval` (procedures), U1 (first-try
tool use), U3 (follow-up). All from `benchmark.md` B1.

**Models:** two frontier (Sonnet 5.5, one OpenAI) and three open-weight
through **Vercel AI Gateway** (GLM-5.x, DeepSeek V4, Qwen3.6; one small
Qwen as the "weak" row). Owner's decision 2026-10-26: open-weight rows run
through the gateway, not a self-hosted vLLM or Ollama, so no time goes
into serving. The catalog already routes `vercel` endpoints. Consequence:
levers that need control of the inference server (guided decoding, strict
schemas, server-side tool parsers, logit masking) are **deferred** until a
self-hosted row exists; the gateway normalises tool calls to the OpenAI
shape, so the adapter work shrinks to thinking handling, `tool_choice`,
resampling and effort.

**Configurations:**

| Config | Meaning |
|---|---|
| `off` | decision 4: minimal functional loop, procedure as prose |
| `base` | today's `main` defaults |
| `base+<lever>` | one lever on (or off, for levers that are on today), about ten rows |
| `full` | every lever on |
| `profile` | the candidate profile for that model class |

**Runs:** k = 5 per cell. Four tasks, five models, about thirteen configs,
five runs is about 1,300 runs. Frontier rows run at k = 3 first and only
the tasks in the 30 to 70% pass band, per the task-subset protocol, to
hold cost. Open-weight rows run through the gateway at gateway prices,
which the cost-per-success line uses as is.

**Reporting, per cell:** pass^k, pass@k, cost per success, tokens per
success, turns per success, repairs, loop detections, guarded-call
violations, approvals; delta against `base` with a paired bootstrap
interval; a variance decomposition (model vs harness vs interaction) across
the grid; reversal count. A lever whose interval includes zero is not
adopted. A lever that helps one class and hurts another goes into the
profile for the class it helps.

## 4. Harness profiles

A profile is data on the model catalog entry:

```jsonc
"harness_profile": {
  "name": "open-strong",          // frontier | open-strong | open-small | mock
  "version": "0.1.0",
  "adapter": { "parser": "glm47", "thinking": "strip", "tool_choice": "auto", "strict": true, "guided": false, "resample_on_invalid": 1 },
  "tools":   { "rank": true, "deferred": true, "examples": true, "mask": false },
  "calls":   { "validate": true, "repair_budget": 2, "parallel": false, "verify_reads": true },
  "context": { "recite_todo": true, "file_memory": true, "compaction": { "trigger": 0.7, "keep_rounds": 3 } },
  "loop":    { "detect": true, "plan_first": true, "verify_before_finish": true, "reset_at_tokens": 90000 },
  "process": { "playbook": true, "guards": true, "gates": true, "evidence": true },
  "evidence": "bench/results/<task-set-version>.json#open-strong"
}
```

- Default profiles ship with VAM: `frontier`, `open-strong`, `open-small`,
  `mock`. An agent may name a profile; otherwise the catalog entry's
  applies; a run may override single switches for the bench only.
- A profile change is a release: it needs a benchmark delta on the rows it
  claims. Community profiles arrive as PRs with their rows (`benchmark.md`
  decision 9). This is the "model profiles as packages" item from
  `agent-runtime.md` §X.4, made concrete.
- The receipt prints `profile open-strong v0.1.0` and any overrides.

## 5. The walkthrough challenge

The public face of the table is one page per challenge:

1. **The story.** The procedure in plain words; who approves what.
2. **The table.** Rows per model, columns `off` and `profile`: pass^k, cost
   per success, repairs, denials, approvals. Confidence intervals shown.
3. **What the machine did.** For one run per model: the denial before the
   gate, the repaired call, the compaction, the loop that was caught.
4. **The receipts.** One per cell, linked and verifiable.
5. **Where it still loses.** The honest line, per model.

The first challenge is `close-prep`; the second `invoice-approval`.

## 6. Phases

| Phase | Outcome | Contents | Depends on | Exit |
|---|---|---|---|---|
| **H0 Switches and honesty** | Every lever switchable; the loop stops lying | `harness` run input with defaults equal to today; guest schema validation with typed errors (decision 5); loop detector (repeated call, A-B-A-B, repeated error) that injects an observation then stops; todo recitation via `read_todo_plan`; effective configuration on the run record and receipt; scenario `model` block and usage in `summary.json` (shared with B0) | E10 Phase 2 (done) | All existing scenarios green with defaults; new scenarios in §7 green; a bench run prints its switches |
| **H1 Open-model adapters** | Open models run first-class through the gateway | Catalog entries for the three gateway open-weight models with `adapter` fields and prices; thinking and reasoning-content handling on the OpenAI-compatible path; `tool_choice` passthrough; one resample on invalid; reasoning-effort passthrough; tool examples in descriptions; the `open-small` and `open-strong` default profiles as data. Deferred to a self-hosted row: parsers, strict, guided | H0 | U1 and `close-prep` run on GLM-5.x and a small Qwen through the gateway with 0 silent `{}` calls |
| **H2 Ablation and first profiles** | The grid is run, the profiles are evidence-backed | The §3 grid on four tasks and five models; variance decomposition; profiles pinned with their rows; the first challenge page | H1, B1 | Profiles `frontier`, `open-strong`, `open-small` published with rows; the `close-prep` walkthrough live |
| **H3 Ranking, deferral, escalation** | The levers the grid asked for | `find_tools` with deferred tools and ranking; escalation ladder across allowed models; `verify_before_finish`; whichever the grid shows dominate repair cost or token cost | H2 | Rows re-run; profiles bumped with deltas |
| **H4 Recursive harnessing, offline** | Profiles improve themselves under review | Propose, probe, compose over profile fields with the benchmark as the probe; a reviewer model; output is a profile PR with its rows; never applied at run time (decision 8) | H2, B3 | One accepted profile PR produced by the loop |

H0 overlaps B0 on the `model` block and usage recording; plan them as one
wave.

### 6.1 Coverage: which phase builds which lever

Every lever in §1, with the phase that makes it switchable, the phase that
builds it if it is absent today, and the track that owns it when it is not
H. "Switchable in H0" means H0 adds the switch with today's behaviour as
the default; the lever itself may already exist.

| Lever | Switchable | Built or changed | Owner |
|---|---|---|---|
| Tool-call format normalisation | H0 | deferred (gateway normalises; self-hosted row later) | H |
| Thinking handling | H0 | H1 | H |
| `tool_choice` | H0 | H1 | H |
| Strict schemas, guided decoding | H0 | deferred (needs the inference server) | H |
| Resample on invalid | H0 | H1 | H |
| Reasoning effort | H0 | H1 | H |
| Names, descriptions, examples | H0 | H1 (examples) | H |
| Ranking and search, deferred tools | H0 | H3 | H |
| Masking vs removal | H0 | H3 (with ranking; cache effect measured in H2) | H |
| Schema validation and typed error | H0 | H0 | H |
| Repair budget | H0 | H0 (with validation) | H |
| Parallel calls | H0 | H3 | H |
| Result caps and spill | H0 | present | H |
| Verify-reads | H0 | E10 Phase 4 | E10 |
| System prompt variant | H0 | H2 (two variants in the grid: today's v11 and a short-altitude one) | H |
| Summary compaction thresholds | H0 | present; thresholds swept in H2 | H |
| Clearing | H0 | present | H |
| File memory and notes | H0 | H3 (a notes convention the prompt enforces, measured first in H2 as prompt-only) | H |
| Todo recitation | H0 | H0 | H |
| Reminders | H0 | present | H |
| Loop detection | H0 | H0 | H |
| Planning step | H0 | H2 (prompt-level `plan_first` variant in the grid; a tool-level plan step in H3 if it wins) | H |
| Verify before finish | H0 | H3 | H |
| Escalation ladder | H0 | H3 | H |
| Context reset at tokens | H0 | H3 (only if a loop-at-length failure shows in H2) | H |
| Fresh-context helper | H0 | H3 (with verify-before-finish; same VM, same permissions, result capped) | H |
| Playbook tracking, guards, gates, evidence, exit check (`machine off`) | H0 | present; `off` rendering in H0 | H and playbooks |
| Procedure render variant | H0 | H2 (two variants in the grid) | H |
| Effect journal | H0 | E10 Phase 4 | E10 |
| Provider retry and fallback | H0 | H3 (fallback within the allowlist) | H |
| Ask placement, structured asks | H0 | E10 Phase 3 | E10 |
| Approval saving | present | present | E1 |

Nothing in §1 is unassigned. Three levers are deferred until a
self-hosted model row exists; four belong to E10 phases already planned;
the rest land in H0 to H3.

## 7. Scenarios

- `invalid-tool-arguments-return-a-typed-error`
- `repeated-identical-call-is-caught-and-observed`
- `todo-is-recited-each-turn-when-enabled`
- `harness-switches-print-on-the-receipt`
- `machine-off-runs-the-procedure-as-prose-with-same-tools`
- `vllm-model-runs-with-its-catalog-parser` (`needs: model`)
- `thinking-model-tool-calls-are-not-dropped` (`needs: model`)
- `profile-change-without-rows-is-rejected-at-release`

## 8. Open questions

1. Where does `machine off` render the procedure: `system_instructions`
   verbatim, or a prose variant generated once at release?
2. Guided decoding: on by default for `open-small`, given the reports that
   grammars can suppress the call entirely? Proposal: off; `strict` on;
   measure in H2.
3. Loop detector thresholds: Gemini CLI's constants (5 identical calls, 10
   identical results) as the start, or tuned per class in H2?
4. Escalation ladder: which decisions escalate (planning, repair failure,
   finish check) and who sets the allowed stronger model?
5. Does a profile live on the catalog entry or as its own
   `urn:vam:profile` artifact? Proposal: catalog entry first; promote when
   the community contributes.
6. Reviewer model for H4: the owner's choice per run, logged.

## What we are not building

- No second agent loop. Switches live in `run_react_agent_loop_with_trace`
  and its helpers.
- No run-time self-modifying harness (decision 8).
- No reinforcement learning or fine-tuning of models (Agent Lightning is a
  lab's tool, not a runtime's).
- No multi-agent orchestration in this track. The only sub-agent shape
  here is the fresh-context helper (L5): same VM, same permissions, no
  tools of its own beyond reads, a capped result. Fan-out over items with
  least privilege per child (child VMs), delegation between agents, and
  background agents are E10 Phase 5, E5 and `agent-runtime.md` §X.2, under
  E10 locked decision 4 (a child never exceeds its parent).
- No lever shipped on without a row (decision 3).
