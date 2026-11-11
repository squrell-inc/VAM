# Phase playbook: how an epic phase runs

How one phase goes from plan to close: who does what, the steps in order, the
checks before a merge, cleanup, testing, and records. Rewritten 2026-10-16
by the owner's decision: **the tracker is the only work tracker** and every wave
names its model. Updated 2026-10-18: **scenarios are the
spec** (`design/scenarios.md`): a phase is planned as the scenarios it adds,
changes or retires, and is built test-first. Use it for every phase of every epic.

**Reference run: E1 Phase 7 (2026-10-16).** Breakdown by a background Opus agent
in about 5 minutes; owner approval with one decision changed; two waves (server on
Sonnet medium, app on Sonnet low), two PRs, no fix cycles; released to `main`
the same day. Copy its tracker section when in doubt.

The rules are in [working-agreement.md](working-agreement.md). This file is the
procedure. If the two disagree, the working agreement wins, and this file is
fixed in the same PR. The current state of the work is in
the tracker (current phase and progress log).

## 1. Roles

| Role | Runs where | Model | Does | Never |
|---|---|---|---|---|
| Owner | — | — | Scope and product calls, approves each phase plan, does the manual check at the phase close, approves releases | — |
| Orchestrator | A local or cloud agent session (owner, 2026-10-18) | Sonnet 5.5 medium | Starts the phase-breakdown agent, brings its result to the owner, starts one wave session at a time, validates each wave plan, verifies and merges, cleans up, keeps the tracker current. In a cloud session: background agents in worktrees, `gh` or the GitHub tools for PRs and merges; the git proxy may refuse remote branch deletes (ask the owner) | Write product code. Merge into `main` or release without the owner's word |
| Phase-breakdown agent | Background agent started by the orchestrator (Agent tool, `run_in_background: true`) | Opus 5.5 high | High-level evaluation of the phase against the design and the code; writes the **Scenario delta** first, then splits it into items, groups them into waves, suggests the model per wave; writes all of it into the tracker | Write product code. Plan or build a wave in detail (the wave session does that) |
| Wave session | An agent session (local or cloud), in its own worktree | From the wave's row in the tracker (§2) | One wave: plan mode first, then code and tests, one PR per repo | Merge. Touch the main checkouts. Change scope outside its wave |

Any model outside §2, or a tool outside this table: tell the owner first.

## 2. Model per wave

The phase plan picks the model and effort for each wave and writes it in the
wave table. Pick the cheapest row that fits.

| Pick | When |
|---|---|
| **Sonnet 5.5 low** (default) | Wave 0 "scenarios" (below); well-specified work that follows an existing pattern: one crate or one app area, UI cards and forms, CRUD endpoints, CLI flags, tests, docs and tracker edits, renames, small scaffolds |
| **Sonnet 5.5 medium** | Crosses two or more crates or both repos; adds a new API, proto, schema or migration; changes the session or run state machine; anything the plan cannot fully specify |
| **Sonnet 5.5 high** (rare, say why in the row) | The security boundary (vsock, policy and Cedar, secrets, egress, auth); concurrency or reconciliation logic; a retry after a medium session failed its wave |
| Opus 5.5 high | The phase-breakdown agent only |

A wave session that fails its checks twice stops and reports. The orchestrator
then either reruns the wave one step up the table or records it as a known issue
in the tracker. Never a third cycle at the same model.

## 3. The steps

1. **Start.** Read the working agreement, this file and the phase section of
   the tracker. Check that the main checkouts are clean on their base
   branch, then pull. Compare the base with `main`
   (`git rev-list --left-right --count origin/main...origin/<base>`).
2. **Phase breakdown (background agent, Opus high).** The orchestrator starts
   one background agent with the tracker path and phase number. Not plan mode,
   and not a detailed plan: a high-level evaluation. It reads the design section
   for the phase and maps the code it touches, then writes into the tracker's
   phase section:
   - **Scenario delta** (first, before any item): the scenarios the phase
     adds, changes or retires, found from the scenarios' `guards` and `given`
     and from the code map, not from memory (`design/scenarios.md` §9). 3 to 8
     scenarios; more means the phase is too big.

     ```markdown
     | Scenario | Change | Layer | Given | Why |
     |---|---|---|---|---|
     | follow-up-continues-after-stop | new | process | agent-with-builtins | the phase's exit |
     ```
   - **Wave 0 "scenarios"** (Sonnet low, merges first): writes the new and
     changed scenarios and world pieces as `pending` and proves each is red for
     the right reason: `scripts/e2e/run.sh --scenarios a,b` fails at the step the
     phase adds, class `product`. A pending scenario that passes early is wrong.
   - **Items**: `<prefix>-<phase>.<n>` (e.g. `AX-7.1`), each with scope, the
     files or areas it touches, acceptance check and test commands, and the
     pending scenarios it turns green. Enough for a
     cold wave session to plan from; the detailed plan is the wave session's job.
   - **Waves**: the wave table below. Items in one wave share a session and a
     PR, so they must fit one context. Parallel work is not used: one wave at a
     time. Land a small scaffold first when later waves share a trait or
     `Cargo.toml`.
   - **Open questions** with a default for each.
   - **Model per wave** from §2, with a one-line reason for any medium or high.
   - Commit it to the base as `docs(<slug>): Phase <n> breakdown`, and return a
     short summary. The orchestrator relays it to the owner.

   ```markdown
   **Waves**

   | Wave | Items | Turns green | Repo | Model | Budget | Depends on | Status | PR |
   |---|---|---|---|---|---|---|---|---|
   | 0 | scenarios | — (red, right reason) | vam | Sonnet low | 45 min | — | [ ] | |
   | 1 | AX-7.1, AX-7.2 | follow-up-continues-after-stop | vam | Sonnet low | 60 min | 0 | [ ] | |
   ```
3. **Approval.** The owner approves the breakdown and answers or accepts the open
   question defaults. No wave starts before this.
4. **Run a wave.** Start one session (`start_session`, `use_worktree: true`,
   model and effort from the wave row). Its brief: the tracker path and wave
   number, the base branch, the allowed checks (§6), the PR rule (title
   `<wave id>: <summary>`, e.g. `AX-7 W1: …`, body lists the items and says
   "Do not merge"). The brief also carries:
   - **What earlier waves shipped** that this wave builds on: exact field names,
     edge cases found in review (P7: `marketplaceDomain` is absent from the JSON
     when unset). Do not make the session rediscover them.
   - **Known local failures** it should not chase: the vam-orchestrator VM and
     session tests that fail on macOS without KVM (CI is the gate for those).
   - **CI runs in the session** (working-agreement.md, 2026-10-30): the PR
     is a draft until `scripts/ci/local.sh --full` passes in the session
     (fmt, protos, manifests, check, the clippy gate at `-D warnings` on both
     feature sets, the workspace suite, smoke); `--fast` while building
     (no clippy). Build settings come from the script; never
     another `target` dir. The agent pastes the script's summary into the PR
     body under "Local CI".
   - **Before the final report, stop any monitors or wakeups it started**, so the
     session can be archived after merge.

   **The per-wave loop:** the wave session's first turn is **plan only** (items
   checked against the code, the plan returned, nothing edited) → the
   orchestrator validates it → the owner approves → the session builds and tests
   → opens the PR → the orchestrator verifies (re-runs the checks, reads security
   code) and merges. Mark the wave `[~]` in the tracker.

   **Relaying a plan or a breakdown to the owner (owner, 2026-10-27).** The
   owner reads only the orchestrator's relay. The background agent's text is
   not in front of them. A relay that says "I approve the plan and recommend
   the following" with one line per item assumes a reading that did not
   happen, and is wrong. Every relay stands on its own, in plain words, in
   this order:
   1. **What it builds.** Three to six sentences: the problem the wave solves,
      what a user or operator can do afterwards that they cannot do today,
      and which scenarios turn green.
   2. **How it does it.** At most five bullets, one design choice each, naming
      the component or file it touches in words, no code.
   3. **Recommendations.** Each one as a short paragraph: the change, why,
      what happens to the wave if it is taken, what happens if it is not.
      Mark each `required` (the orchestrator will not approve without it) or
      `suggested`.
   4. **Risks and questions for the owner.** Only what needs the owner's
      answer, each with the orchestrator's default.
   5. **The ask.** One line: approve as is, approve with recommendations n and
      m, or answer question k first.
   Length: 150 to 350 words for a wave, up to 500 for a phase breakdown.
   Jargon gets its plain meaning the first time (say "the host rejects a
   step the agent has not earned yet", not only "guard"). The same format
   applies to the breakdown relay in step 2 and to a fix-wave relay in §6.

   **Test-first guardrails** (`design/scenarios.md` §8, locked decisions S2, S8, S9):
   - Scenarios assert behaviour seen through the CLI, API, UI and events, never
     internal names. No plan numbers (epic, phase, wave, story) in a scenario
     id, file, world piece, fixture or mock label.
   - A build wave may not edit a scenario's `then` to make it pass. A needed
     change is a **spec change**: one line in the PR, approved by the
     orchestrator, or by the owner if product behaviour changes.
   - Scenarios say what works end to end; waves still write unit tests for how.
   - **Stop rules:** a fix cycle counts only if a scenario moves (passes, or
     fails at a later step). No movement in two cycles: stop and report with the
     `brief.md`. No rerun without a change; rerun only the failed scenarios.
     Mock model only; the real model only on the owner's word.
5. **Verify and merge** (§4), then **clean up** (§5). Tick the items, set the
   wave `[x]` with its PR, add a progress-log row.
6. **Next wave.** Repeat steps 4 and 5.
7. **Phase close** (§6), then **after the close** (§7).

## 4. Verify and merge a PR

- **Shape.** The base is the epic base branch. Only the files the wave's items
  name change.
- **Diff.** Read it against the acceptance checks. For security-relevant code
  (egress, auth, secrets, vsock) read the code, not the summary. Review in a
  worktree detached at the PR head, with `git diff origin/<base>...HEAD -- <paths>`.
- **Tests.** vam: the session is the gate (working-agreement.md, 2026-10-30):
  the orchestrator re-runs `scripts/ci/local.sh --full` (add `--release` on
  a release PR) in a worktree detached at the PR head, with the shared
  target dir, and reads the summary; "Local CI" in the PR body is a claim,
  not evidence. A build wave is done when the scenarios it names pass there
  and the full suite is green there; re-run named scenarios with
  `scripts/ci/local.sh --scenarios a,b`. The app repo (no CI, the local run is the gate): in a
  worktree detached at the PR head, `CI=1 pnpm test`, `pnpm exec tsc --noEmit`,
  `cargo test --manifest-path src-tauri/Cargo.toml`, and `pnpm e2e:typecheck`
  when e2e files change. To test a merged state, use `git merge --no-commit`;
  never create an unsigned commit (no `--no-gpg-sign`, no `commit.gpgsign=false`).
- **Contracts.** New Tauri commands are registered in `lib.rs`
  (`generate_handler!`). Endpoint names match what the CLI and the app call.
- **Session claims are not evidence.** "Tests pass" is re-run, not trusted.
- **Actions (vam code PRs).** Normally none: the PR was proven in the session
  and is marked ready with `[skip ci]` on its last commit (or the
  `skip-vm-lane` label when it touches a VM path but needs no microVM).
  Actions runs only for a microVM-path change the lane must prove, a release
  PR into `main`, or a security-sensitive change the orchestrator wants
  confirmed on a second machine; then check once, about 5 minutes for the
  fast jobs, and never wait on the advisory lane. A docs-only PR runs only the `docs` check
  (`.github/workflows/docs.yml`: `vam-e2e check` and `pnpm build`).
- **Merge.** `gh pr merge <N> --squash --match-head-commit <sha>` with the SHA
  you verified. No auto-merge. Never into `main`. If a permission check blocks
  the merge, report it and wait.

## 5. After every merge: cleanup

- Archive the wave session. If archiving is refused because the session still
  has live work (a leftover monitor or wakeup), do not retry in a loop: ask the
  owner to archive it from the sidebar.
- Remove its worktree and directory, build output included
  (`git worktree remove --force <path>`, `git worktree prune`).
- Delete the branch locally with `git branch -D` (squash merges look unmerged to
  `-d`) and on origin if GitHub did not.
- Pull the base branch into the main checkout. Report free disk (`df -h /`).

## 6. Testing rule and phase close

Changed 2026-10-18: the process-layer scenarios run on every PR in CI and in
any session with `scripts/e2e/run.sh`. A KVM host is only for scenarios marked
`needs: firecracker`.

| When | What |
|---|---|
| Every wave | Unit and integration tests of the crates and app areas it changes, `cargo check --workspace`, `tsc`, and the scenarios the wave names (`scripts/e2e/run.sh --scenarios a,b`; `--smoke` for the smoke set). New behaviour comes with tests at the cheapest layer that covers it |
| Every PR | In the session, `scripts/ci/local.sh --full` on the PR head (fmt, protos, manifests, check, workspace tests, smoke); `--release` adds the release gate. GitHub Actions only for microVM-path changes the lane must prove and for release PRs into `main` (§12) |
| Phase close | All of the phase's scenarios are `active` and green in CI. Scenarios marked `needs: firecracker` run once on a KVM host if the phase touched them. The app: the unit suites on the merged base, once. Then release (§8) and hand the **team checklist** (below) to the team. The next phase starts right away |
| Lima or live journey runs | Only when the owner asks: one run, at most 15 minutes, at most two fix cycles, then record what is open |

**Scoreboard.** Every progress-log row records: scenarios turned green,
scenarios red, fix cycles, wall time. A phase whose scoreboard does not move for
a wave is flagged to the owner.

**Team checklist (owner, 2026-10-16).** At the end of every phase the
orchestrator writes a checklist the team runs on the team instance, and the
owner hands it over. Nobody waits on it.

- Written from the phase's scenarios and exit criteria, in the tracker's **Phase close**
  block. At most 10 steps, about 10 minutes, no developer tools beyond the app,
  the CLI and `curl`.
- Starts with **Setup**: what to deploy, in which order, and any env var.
  Each step says what to do and what the tester should see. It ends with
  **Report**: pass, or the step number with a screenshot or the error text.
- The phase is marked `released, checklist out` in the tracker, and the next
  phase's breakdown starts at once.
- When the team reports: on pass, close the phase (§7). On failure, the
  orchestrator adds a fix wave (Sonnet low or medium, one PR against the
  current base) to the **phase in progress**, runs it before that phase's next
  wave, and records it in both phases' progress logs. Two fix cycles at most,
  then the owner decides: ship with a known issue, or hold.

## 7. After the phase close (orchestrator)

1. Update the tracker: tick the phase's checkboxes, mark the phase done with its
   date, add the progress row, fix `current_phase` and the row in `README.md`.
   Then **archive it**: move the phase section and its progress-log rows to
   `.cursor/workstreams/archive/<slug>.md` (append), and leave one row in the
   tracker's "Done phases (archived)" table. The tracker keeps Goal, Locked
   decisions, the open phases, Explicitly later and the open phases' log, so a
   session reads only what is current.
2. This runs when the team reports a pass, which may be during a later phase.
   The release (§8) and the next phase's breakdown do not wait for it.

## 8. Release to `main`

On the owner's word only. Open a PR from the epic base into `main` (both repos
when both moved, vam first), titled `release: <epic> <phases>`. The body has a
**deploy note**: order (vamd before the app when the app needs new vamd fields)
and any env var to set once. Merge with a **merge commit**, never squash. If the code is unchanged since a green
CI run, do not wait for CI again (§4). Then fast-forward each base to `main` so
they start level. Someone may push to the app's `main` directly (release
workflow); check `git merge-tree` for conflicts before opening the PR.

## 9. Sessions and records

- **One orchestrator session per phase.** Start it from the tracker. Before
  closing, update the tracker's current state and progress log.
- **Start message.** "You are the orchestrator for `<slug>`. Read
  `.cursor/workstreams/working-agreement.md`,
  `.cursor/workstreams/phase-playbook.md` and
  `.cursor/workstreams/<slug>.md` on `<base branch>`, then continue from the
  current phase."
- **Where state lives.** The tracker has the plan, waves, items, locked
  decisions, the current state and the progress log. GitHub has branches and
  PRs. Nothing lives only in chat.
- **Base branches.** Each epic has its own base, cut from `main` (table in
  `working-agreement.md`). Phases start on it, not on `main`.
- **One orchestrator per epic.** Start a new orchestrator session per epic (or per phase when the
  context gets long) from the repo root on the epic's base branch.
- **Status messages** say what changed, what is next, and what
  needs a decision.

## 10. Ask first

Ask the owner before: merging into `main`, any release, a live Lima run, a model
not in §2, changing a locked decision, changing a scenario's `then` in a way that alters product behaviour, a third fix cycle, or deleting anything §5
does not cover. Everything else in §3 to §7 is approved in advance.

## 11. Deciding what to build, and proving it (added 2026-10-21)

Applies to every phase breakdown and every proposal from this date; phases
already in progress finish under their existing plan.

- **Benchmark delta.** Next to the Scenario delta (§3), the phase-breakdown
  agent writes the **Benchmark delta**: the lines in `design/benchmark.md`
  §1 the phase moves, the tasks it adds, and the baseline on the current
  base (run once, two models, owner's word for paid endpoints per §10). The
  phase close records before and after on the same models in the scoreboard
  (§6), beside scenarios green and red. A line that did not move is written
  down as a finding. A problem with no line gets its line first.
- **Decision line.** Every item, wave, backlog row and new design carries
  one line from the rubric in `design/launch-0.1.0.md` §3:
  `Decide: NOW | SLICE | BACKLOG | CUT — proof N, cost N, line <metric or
  launch proof>`. The breakdown agent writes it, the orchestrator checks
  it, the owner reads it. A proposal with no line is not planned.
- **Overrides.** A finding that weakens P1 is NOW whatever the cost. A
  locked decision is never scored; it is a conversation with the owner.

## 12. CI minutes and disk in cloud sessions (owner, 2026-10-30)

The rule in one line: **prove in the session, spend Actions only on the
microVM lane and on releases.** Details in `working-agreement.md` ("CI runs
in the session" and "Disk in cloud sessions"). What every orchestrator and
wave brief carries:

- **Draft first.** A wave PR opens as a draft and stays one until
  `scripts/ci/local.sh --full` passes in the session (`--fast` while
  building; `--scenarios a,b` for the wave's own). The summary goes into the
  PR body under "Local CI"; the orchestrator re-runs it at the PR head before
  merging.
- **Ready without Actions.** No VM-path change: the last commit says
  `[skip ci]`, then mark ready and merge. VM-path change but no microVM
  needed: label `skip-vm-lane`, mark ready, the fast jobs run once. MicroVM
  path the lane must prove, or the release PR into `main`: full Actions, once.
- **One target dir, one command.** `CARGO_TARGET_DIR=/home/user/vam/target`
  for every build in every worktree; the script sets the rest
  (`CARGO_INCREMENTAL=0`, debug info off). No ad-hoc `cargo test` with other
  flags; no `target` symlinks; no `cargo clean` except on the cleanup ladder
  with the owner told.
- **Worktree hygiene.** Record `df -h /home/user` in the wave row before the
  build. After the merge: `git worktree remove --force <path>`,
  `git worktree prune`, delete the local wave branch. `git worktree list`
  shows only the main checkout between waves. Under 3 GB free: stop and
  report; the ladder in the working agreement decides what goes.
- **The breakdown says which waves need Actions.** The phase-breakdown agent
  marks each wave `actions: none | fast | lane` from the files it touches,
  so the orchestrator knows before the wave starts whether a run on GitHub
  is coming and can batch VM-path waves.
