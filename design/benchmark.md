# The VAM benchmark: proof that a problem is solved

Status: **proposed** (2026-10-21). Standing track **B** (like the E9
scenario harness): it has its own tracker once Phase B0 is planned, and
every product phase after it names the benchmark lines it moves. Extends
`agent-runtime.md` Part VI (task set U1–U9, metrics, gate). Amends `agent-experience.md` "How we build it".

## What we are building

A **public, reproducible benchmark of business procedures** that answers
one question per problem in `agent-experience.md` Part I: *is it solved,
on which model, at what cost?* It is the proof behind the run receipt
(`run-receipt.md`) and the table a model lab, an operator or a community
member can run themselves.

Three properties:

1. **Two axes.** Every task runs per **model** and per **machine mode**:
   `machine on` (playbook, guards, gates, ranked tools, journal) and
   `machine off` (the same model, the same tools, a plain chat agent with
   the procedure as prose). The gap between the two columns is the product.
2. **Business procedures, not coding puzzles.** Month-end preparation, a
   Salesforce change request, candidate onboarding, a PR process, an
   invoice approval with a branch gate, a kill-mid-task recovery. The
   tasks come from the problems P1–P14, not from SWE-bench.
3. **Runs anywhere.** The scored lane runs in the process-mode guest with
   mock tools and a real model endpoint, so it runs in GitHub Actions and
   in any session with an API key or a local vLLM. A Firecracker lane
   adds the isolation facts to the receipt. No KVM is needed to reproduce
   a score.

> Claim outcomes per cost plus sovereignty, measured. (`agent-runtime.md`
> §X.5)

## Why

- **P5 and U8** ("is it getting better, which model should we use?") are
  unanswered. The runner exists (`vam-e2e --bench`; the old shell runner is retired), the task set is
  designed, and no real-model number has been recorded (E10 Phase 0 and
  Phase 1 exits were both logged "open, not measured").
- The claim to measure is *outcome per dollar with the model as a dial*:
  open-weight models are near parity on bounded tool tasks and far behind
  on open-ended ones, and the host-enforced machine is what makes the
  bounded case fair. That claim is worth nothing unpublished.
- **Phases need a way to show they solved a problem**, not only that
  their scenarios are green. Scenarios prove behaviour with a scripted
  model; the benchmark proves outcomes with real models. A phase close
  records both.

## Locked decisions (proposed)

Do not re-litigate without editing this list.

1. **Every problem has a line.** Each P in `agent-experience.md` Part I
   maps to at least one benchmark metric (§1). A phase that serves a
   problem names the line it moves and records before and after at phase
   close. "Not measured" is allowed only with a reason and a date.
2. **Two machine modes on every task.** `machine off` is a first-class
   configuration, kept honest: same model, same tools, same mock world,
   the procedure given as prose. It is never handicapped to make the gap
   look larger.
3. **Fixed, versioned task set.** Tasks change only by adding a version;
   scores name the task-set version. A phase may add tasks, never edit a
   task's `then`.
4. **Real models, real numbers, published with the losses.** The docs
   site shows the table including where VAM with an open model still
   loses. The scripted mock is for scenarios, not for scores.
5. **Cost is per success.** `cost per success = total cost of k runs /
   successes`. Tokens and wall time are reported the same way. A cheaper
   model that fails more is not cheaper.
6. **Duplicate effects and lost tasks must be zero** on every published
   row, as Part VI already says. A row with a non-zero count is published
   in red, not hidden.
7. **The benchmark lane is the scenario runner.** Tasks are scenarios
   (`e2e/scenarios/`, `needs: process`) whose `mock` block is replaced by
   a `model` block naming a catalog endpoint. No second harness.
8. **Harness regressions are gated like releases.** A PR that lowers
   pass^k on the smoke task set by more than the noise band is flagged
   (Part VI gate; E4 applies the same to agent releases).
9. **The community can add tasks, worlds and model entries** through PRs
   with the same review as scenarios. Squrell curates the published set.

## 1. Problems and their lines

| Problem | Line(s) in the table | Shown by |
|---|---|---|
| P1 safe action | guarded-call violations (must be 0); injected-instruction compliance (must be 0) | adversarial tasks: the prompt tries to skip the gate or post early |
| P2 governance and audit | receipt completeness: decisions with a named rule and scope (100%); verify passes; pending decisions present in the user's notification list (must be 100%) | every run ends with a receipt that verifies offline; every approval and gate appears in the list |
| P3 creating an agent is hard | time and steps from empty to a released agent that passes its task | the onboarding task, timed from the team checklist |
| P4 encode and share expertise | pass^k of a playbook on a weaker model vs prose on a stronger one | `machine on` with a small model vs `machine off` with a large one |
| P5 reliability and outcomes | pass^k, turns and tokens per success, cost per success, per agent version | the core table |
| P6 SaaS access without keys | tasks completed through brokered connections with 0 secret bytes in guest output; minutes and clicks from an admin's "Enable" to a member's "Connected", member given no instructions (connectors, 2026-11-07) | secret-leak check on every run's transcript; Enable → Connected timed by hand at the connectors phase close |
| P7 work outside chat | tasks started by a trigger and decided from a notify surface that complete; parked cost (must be 0); decisions the user learned about within a minute (share; median time to act) | the scheduled task; the three-day gate with a simulated clock; the notification lands and is acted on |
| P8 cost control | cost per success; budget hits that stop a run cleanly | the runaway task |
| P9 supply chain | malicious-skill tasks that fail safely (100%) | a skill that tries to read a secret, call an undeclared tool, or exfiltrate |
| P10 self-hosting | the same table on a local vLLM endpoint | the open-model rows |
| P11 machine lacks tools | tasks needing Python or git that pass on the standard base | the toolchain task |
| P12 can't use the tool | first-try tool success rate; repairs per success | U1, U8 |
| P13 can't do real work | follow-up success (U3); 50+ call triage pass^k (U4); kill-mid-task duplicates and lost tasks (U5) | the continuity, long-work and crash tasks |
| P14 forms not chat | turns and tokens to collect a structured input, jig vs chat | the purchase-requisition gate, with and without a jig |

## 2. The task set

The existing tasks stay (U1 Notion draft, U2 cited research, U3 follow-up
and correction, U4 50+ call triage, U5 kill-mid-task, U6 two agents one
workspace, U7 teach once, U9 playbook start). Business procedures are
added, each with a mock world under `e2e/world/`:

| Task | Procedure | Exercises | Problems |
|---|---|---|---|
| `close-prep` | Pull trial balance, flag variances, gate on adjustments, post approved subset, notify | gates, `args_within` guard, evidence, park and resume, Slack decision | P1, P2, P4, P5, P7 |
| `sfdc-change` | Read a change request, propose field and flow changes, gate, apply through MCP, verify | MCP OAuth connection, drift re-approval, side-effect defaults | P6, P12, P1 |
| `candidate-onboarding` | Collect documents through a jig, create accounts through brokers, schedule, report | jig Ask, triggers, multiple brokers | P14, P7, P3 |
| `pr-process` | Open PR, wait for CI, address review, merge after approval | `wait_for`, GitHub broker, long work | P7, P13 |
| `invoice-approval` | Match invoice, branch gate (approve, dispute, hold), act per branch | branch gates, outcomes, exit conditions | P4, P5 |
| `runaway` | A task that would loop; budget must stop it cleanly with a receipt | budgets, max_iterations, receipt on failure | P8, P2 |
| `hostile-skill` | A skill that tries to read a secret, call an undeclared tool, and exfiltrate | capability tokens, redaction, egress policy | P9, P1, P6 |
| `toolchain` | A task needing Python and git on the standard base | E7 Phase 1 | P11 |

Each task declares `k` (default 5), its `then` (as scenarios do), and the
machine modes it supports. `machine off` for a task with a gate means the
approval still exists as a tool-level HITL ask; the procedure is prose.

## 3. Metrics and the table

Per task, model, machine mode, task-set version:

| Metric | Definition |
|---|---|
| pass^k | runs where every `then` holds, over k |
| cost per success | sum of run cost over k, divided by successes (tokens priced from the catalog; tokens only when no price). **Price source (owner, 2026-10-27):** always the model maker's own list price (Anthropic for Claude, Alibaba for Qwen, DeepSeek, Moonshot for Kimi), also when the run goes through a gateway; the gateway is transport, not a price |
| tokens per success, turns per success, wall time per success | same shape |
| parked cost | cost accrued while a run is parked (must be 0) |
| duplicate effects, lost tasks | from the receipt's effects section (must be 0) |
| guarded-call violations | tool calls that executed against a guard or a denied rule (must be 0) |
| first-try tool success | tool calls accepted without repair, over tool calls |
| secret bytes in guest output | from the redaction check (must be 0) |

The published table is a JSON file per task-set version
(`bench/results/<version>.json`) rendered on one docs page, with a row per
(task, model, mode). Noise band: the standard error over k; two scores
differ only beyond it.

**Harness disclosure (added 2026-10-26).** Every row names the harness
profile and version it ran under, and any switch overrides, from
`harness.md` §4. The `machine on` column is the model's pinned profile; the
ablation grid in `harness.md` §3 is how the profile earned its rows.

## 4. How a phase uses it

Amends `agent-experience.md` "How we build it" and `phase-playbook.md` §6:

- **Phase plan** names, next to its scenario delta, the **benchmark
  delta**: lines it moves, tasks it adds, and the baseline value on the
  current base (run once, at the start, two models).
- **Phase close** records before and after for each named line, on the
  same models, in the progress-log scoreboard, beside scenarios green and
  red. A line that did not move is a finding, not a failure, and is
  written down.
- **A phase that serves a problem with no line** adds the line first
  (decision 1).
- **Models:** the benchmarked tier in `agent-runtime.md` §VIII.7 (two
  Claude, one OpenAI) plus, from Phase B1, open-weight models **through
  Vercel AI Gateway** (owner, 2026-10-26: no self-hosted vLLM or Ollama for
  the launch table; the P10 self-hosted row is added when needed). Owner's word
  for any run on a paid endpoint beyond the smoke set (playbook §10).

## 5. Phases

| Phase | Outcome | Contents | Depends on | Exit |
|---|---|---|---|---|
| **B0 Baseline** | The first real numbers exist | Fold `scripts/bench/` into the scenario runner (`model` block, `--bench`); fix the `--mock` live path; run U1, U2, U9 on two models, `machine on` only; publish the first JSON and docs page; the noise band | E9 Phase 1 (done) | A table with real numbers on `main`; the docs page says what is and is not measured |
| **B1 Two axes, business tasks** | The product gap is visible | `machine off` mode; `close-prep`, `invoice-approval`, `runaway`, `hostile-skill` tasks and worlds; cost per success; the receipt as the scorer's input (E12 Phase 1); open-weight rows through the gateway | B0, E12 Phase 1 | The `on` vs `off` gap published for four tasks on four models, with the losses |
| **B2 Triggers, continuity, effects** | The hard claims are measured | `pr-process`, `candidate-onboarding`, `sfdc-change`; parked cost; U3, U4, U5 scored; duplicates and lost tasks from the journal | E5 slice, E10 Phases 2–4 | Kill-mid-task row shows 0 and 0; parked cost 0 on the three-day gate |
| **B3 Public lane** | Anyone reproduces a score | GitHub Action that runs the smoke task set on a PR with a model secret; the dispatch-only `bench.yml` (microVM node on a KVM runner, gateway key from a repository secret, results as artifacts) is the first half of this lane; community task and model entries through PRs; the Firecracker lane adds isolation facts (`isolation-truth.md`) | B1, isolation-truth Phase 1 | A community PR adds a task and a model row without Squrell running anything |

B0 is small and can be planned now, in parallel with E10 Phase 2 and E9
Phase 2; it changes `scripts/bench/`, `crates/vam-e2e` and one docs page.

## 6. Scenarios

- `bench-run-records-cost-per-success`
- `bench-machine-off-uses-same-tools-and-model`
- `bench-row-fails-red-on-duplicate-effect`
- `bench-task-set-version-is-pinned-in-results`

## 7. Open questions

1. Noise band: standard error over k, or a fixed threshold per metric?
2. Which open-weight model is the reference row in B1 (GLM-5.x, Qwen3.6,
   DeepSeek V4), and where does it run (a self-hosted vLLM host, or
   a gateway)?
3. Does the public lane run on every PR (cost) or nightly on `main` with
   a PR label to opt in?
4. `machine off` for tasks that need a gate: tool-level HITL ask (as
   proposed) or no human at all?

## What we are not building

- No new harness. Tasks are scenarios with a `model` block.
- No leaderboard service. A JSON file and a docs page.
- No handicapped `machine off` mode.
- No scores from the scripted mock.
- No claim on a model that has no row.
