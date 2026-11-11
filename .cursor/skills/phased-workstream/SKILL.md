---
name: phased-workstream
description: >-
  Turns a brainstorm or design doc into a stored, phase-wise workstream, then
  drives plan-then-build per phase in waves. Use when splitting work into phases,
  asking what to do next, starting a workstream tracker, planning a phase into
  waves, or after a design discussion before implementation.
---

# Phased workstream

```text
brainstorm → design doc → tracker → phase breakdown (background Opus-high agent:
  SCENARIO DELTA first, then items + waves + model) → owner approves
  → wave 0 "scenarios" (new/changed scenarios land as pending, red for the right reason)
  → one build wave at a time (plan only → orchestrator validates → owner approves
    → build → PR → orchestrator verifies and merges), each turning named scenarios green
  → phase close (the phase's scenarios active and green in CI) → next phase
```

Trackers live in `.cursor/workstreams/<slug>.md`. Template: `.cursor/workstreams/_template.md`.
The tracker is the only work tracker: no Linear issues. Do **not** put trackers
in the repo root or in Starlight. Full procedure for epic phases:
`.cursor/workstreams/phase-playbook.md`.

**Every phase is planned and built test-first through scenarios** (design
`design/scenarios.md`, how to write and run them `e2e/README.md`). A scenario is
a runnable given/when/then flow in `e2e/scenarios/<id>.yaml`, named by the
behaviour it protects, with setup from named world pieces in `e2e/world/`.
Run them anywhere, no KVM: `scripts/e2e/run.sh --smoke | --scenarios a,b | --all`.

## Developer loop (what to say)

| You want | Mode | Prompt |
|---|---|---|
| Capture phases after a design talk | Agent | "Open a workstream from this discussion" |
| See what is next | Agent | "What's next on `<slug>`?" |
| Break down a phase | Agent (orchestrator) | "Break down phase N of `<slug>`" (starts a background Opus-high agent) |
| Build a wave | Plan, then Agent | "Build wave W of phase N of `<slug>`" |

One phase per breakdown, one wave per session.

## Stage A — Capture

1. Confirm a design home exists (`design/<slug>.md` or `docs/src/content/docs/...`).
2. Copy `_template.md` to `.cursor/workstreams/<slug>.md`.
3. Fill goal, out of scope, locked decisions, phases with **Goal / Depends on / Exit / checkboxes**.
4. Set `status: active`, `current_phase` to the first undone phase.
5. Add a row to `.cursor/workstreams/README.md`.
6. Stop. No code in this stage unless the user asks.

## Stage B — Pick next (orchestrator)

1. Read the tracker. Do not reconstruct phases from chat.
2. List unblocked undone phases. Recommend one, say why others wait.
3. Ask which phase to plan.

## Stage C — Phase breakdown (background agent, Opus high)

Not plan mode. A high-level evaluation of the chosen phase only, written into
its tracker section:

- **Scenario delta first**, before any item: the scenarios the phase adds,
  changes or retires (table: Scenario | Change | Layer | Given | Why). Find the
  touched ones from the existing scenarios' `guards` and `given`, not from
  memory. 3 to 8 per phase; more means the phase is too big.
- **Wave 0 "scenarios"** (Sonnet low) always comes first: it writes the new and
  changed scenarios and world pieces as `pending` and proves each fails at the
  step the phase adds, class `product`.
- **Items** (`<PREFIX>-<phase>.<n>`): scope, files, acceptance check, test commands.
- **Waves** table: wave, items, **turns green** (the pending scenarios it makes
  pass), repo, **model**, **budget** (wall clock), depends on, status, PR. One wave
  fits one session's context and lands as one PR per repo.
- **Model per wave** (playbook §2): Sonnet 5.5 **low** by default; **medium** when
  it crosses crates or repos, adds an API/schema/migration or changes a state
  machine; **high** rarely, with a reason (security boundary, concurrency, a
  retry after medium failed).
- **Open questions** with defaults. Out of scope (later phases).

Commit it as `docs(<slug>): Phase <n> breakdown`. The owner approves before any
wave starts.

## Stage D — Build a wave

1. In the wave's worktree, read the wave's items, the scenarios it turns green,
   and only the files they name.
2. **First turn is plan only**: check the items against the code, return the
   wave plan, edit nothing. The orchestrator validates it and the owner
   approves before any build.
3. Then implement only that wave. Add unit tests for how; the scenarios cover
   what works end to end.
4. Run the wave's checks (`cargo test -p …`, `cargo fmt --all --check`,
   `pnpm test`, `tsc`) and its scenarios (`scripts/e2e/run.sh --scenarios a,b`).
   Done = those scenarios pass and CI is green (CI runs `scenarios (smoke)`).
5. Open the PR; do not merge. The orchestrator re-runs the checks and reads
   security code itself before merging.

**Test-first guardrails** (locked in `e2e-scenarios.md`, S1–S9):
- Scenarios assert behaviour seen through the CLI, API, UI and events, never
  internal names.
- Never edit a scenario's `then` to make it pass. A needed change is a **spec
  change**: one line in the PR, approved by the orchestrator, or by the owner
  if product behaviour changes.
- No epic, phase, wave or story numbers in scenario ids, files, world pieces or
  mock labels (`vam-e2e check` rejects them). Plans point at scenarios, never
  the reverse.
- **Stop rules:** a fix cycle counts only if a scenario moves (passes, or fails
  at a later step). Two cycles without movement → stop and report with the
  failure's `brief.md`. No rerun without a change. Mock model only; the real
  model only on the owner's word.

## Stage E — Record

After each merge: tick the items, set the wave `[x]` with its PR, add a
**Progress log** row with the **scoreboard**: scenarios turned green, scenarios
red, fix cycles, wall time. A wave whose scoreboard does not move is flagged to
the owner. At the phase close: mark the phase done, set
`current_phase` to the next undone phase (or `done`), update the README row, and
move the phase section and its log rows to `.cursor/workstreams/archive/<slug>.md`,
leaving one row in the tracker's "Done phases (archived)" table.
If blocked, mark `[~]`, log the blocker, do not skip ahead.

## Rules

- The tracker is cross-session memory. Never gitignore it.
- Subagents do not store phases. The markdown file does.
- Locked decisions: do not re-litigate unless the user updates the tracker.
