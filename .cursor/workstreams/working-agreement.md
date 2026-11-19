# Working agreement: how VAM is built with agents

How the product owner, the orchestrator session and the wave sessions work
together. Current state lives in each tracker's current phase and progress log.
The step-by-step procedure is `phase-playbook.md`.

Rewritten 2026-10-16 (owner): all work runs in agent sessions (local or cloud)
and is tracked in the workstream trackers. Updated 2026-10-18: scenarios are the
spec (`design/scenarios.md`); they run on every PR.

## Sources of truth

- **Trackers** (`.cursor/workstreams/<slug>.md`): locked decisions, phases, and
  for the active phase its items, waves (with the model per wave), open
  questions, the current state and the progress log.
- **Scenarios** (`e2e/scenarios/`, `design/scenarios.md`): what must work, as
  runnable behaviour. A phase is planned as the scenarios it adds, changes or
  retires.
- **Design docs** (`design/<slug>.md`): the reasoning. Read only the section for
  the phase in hand.
- **GitHub**: branches and PRs. Nothing important lives only in a chat.

## Repos and branches

| Work | Base branch |
|---|---|
| E1 Create & release (server, CLI) | `feat/agent-experience` |
| E10 Agent runtime (server, guest, CLI) | `feat/agent-runtime` |
| E8 Egress and secrets | `feat/egress-and-secrets` |
| E12 Run receipt (server, CLI) | `feat/run-receipt` |
| E9 E2E scenarios | `feat/e2e-scenarios` |
| Connectors (server, CLI) | `feat/connectors` |
| A fix, follow-up or investigation session | its own `fix/<slug>` (or `feat/<slug>`), cut from `main` |
| Small cross-cutting fixes with no session of their own (docs drift, deps, CI lint) | `chore/maintenance`, cut from `main` |

- Feature branches take `main` by **merge** (never rebase), and only at the
  moments in "Integration branches" below.
- Changes land through PRs. `main` takes an epic only through an owner-approved
  release (playbook §8).

## Integration branches, and one release to `main` (owner, 2026-11-19)

Several sessions merging small PRs into `main` made them fight: one session's
merge put another's open PR into conflict, the other merged `main` back in,
and the loop repeated (four `ra-sync-main-into-a*` merges in two days). Each
of those merges, and each release, also re-ran the whole CI suite. So, for
every session (phase orchestrators, wave sessions, fix and follow-up
sessions, and sessions the owner starts by hand):

1. **One integration branch per session.** At its start, the session cuts
   its branch from `main` (the table above) and names it in its first
   message and in its tracker. A session the harness gives a `claude/*`
   branch uses that branch for its PRs' head, never as their base.
2. **Every PR targets that branch, never `main`.** Waves, review and
   CodeRabbit fixes, follow-ups, tracker and backlog updates, docs. Each one
   merges into the branch on green with a merge commit (the orchestrator
   merges wave PRs; a fix session may merge its own PRs into its own branch).
3. **`main` takes the work once.** When the phase, or the session's whole
   job, is done, one release PR goes from the integration branch into
   `main`, opened on the owner's word (playbook §8). The tracker close-out,
   backlog lines and docs ride in that PR, not in their own PRs afterwards.
   Follow-ups found after a release go to the next phase's branch or a new
   `fix/<slug>`, batched, not to `main` one at a time.
4. **A PR straight into `main` only when the owner asks for it**: a
   security hotfix, a broken `main`, or a change another release cannot wait
   for. Its body says so in one line ("Owner asked for a direct release:
   <reason>").
5. **Merge `main` in once, not on every change to `main`.** Merge it into
   the integration branch right before opening the release PR, or earlier
   only when a conflict blocks the work or the work needs a fix that is on
   `main`. Do it as one merge PR into the branch; never rebase a shared
   branch.
6. **One release PR into `main` at a time.** Before opening one, list the
   open PRs into `main`. If another session's release PR is open, wait for
   it to merge (tell the orchestrator), then merge `main` in once and open
   yours. The orchestrator orders the releases.
7. **Hands off other sessions' work.** Never push to, merge, retarget or
   resolve conflicts in another session's branch or PR. A conflict with
   another session's work goes to the orchestrator.
8. **Spend CI on purpose.** Push when a PR is ready, not on every commit
   (each push re-runs the suite); keep a PR a draft while still pushing; put
   small fixes in one PR; let tracker and docs commits ride with a code PR.
   No empty commits, no re-runs "to see", at most one re-run for an
   infrastructure failure.

`chore/maintenance` collects small cross-cutting fixes that have no session
of their own. The orchestrator releases it into `main` in one PR when the
owner asks, or ahead of a tag.

## Roles

- **Product owner**: decides scope and product rules, approves each phase plan,
  answers open questions, does the manual check at each phase close, approves
  releases.
- **Orchestrator** (a local or cloud agent session, owner 2026-10-18): starts the phase-breakdown agent,
  brings its breakdown to the owner, starts one wave session at a time, reviews every PR, merges into the
  base, keeps the tracker current. Does not write product code.
- **Phase-breakdown agent** (background agent from the orchestrator): evaluates
  the phase at a high level, writes the **Scenario delta** first, splits it into
  items and waves (wave 0 "scenarios" first), suggests the model
  per wave, and saves it in the tracker.
- **Wave sessions** (agent sessions, each in its own worktree): build one
  wave. Their first turn is plan only; the orchestrator validates it and the owner approves; then they implement and test it, then
  open a PR. They never merge.
- **Design sessions**: open-ended design. Output goes into the design doc and
  tracker, never only into chat.

## Models

- **Phase breakdown**: Opus 5.5 high, as a background agent started by the
  orchestrator, once per phase. Not plan mode.
- **Orchestrator while running waves**: Sonnet 5.5 medium.
- **Wave sessions**: Sonnet 5.5, effort chosen per wave in the phase plan:
  **low** by default, **medium** second, **high** rarely and with a reason
  (rubric: `phase-playbook.md` §2).
- Other models are not used unless the owner asks. Tell the owner before
  using anything outside this list.

## How a phase runs (summary)

1. A background Opus-high agent writes the phase breakdown into the tracker:
   the Scenario delta, then items with acceptance checks (naming the pending
   scenarios they turn green), waves with model and dependencies, open
   questions.
2. The owner approves.
3. Wave 0 "scenarios" lands the new scenarios as `pending`, red for the right
   reason. Then one wave at a time: plan-only first turn, orchestrator validates,
   owner approves, the session builds, tests and opens one PR per repo. The
   orchestrator verifies (re-runs, reads security code), merges, cleans up,
   ticks the tracker and logs the scoreboard (scenarios green, red, fix cycles,
   wall time).
4. Phase close: the phase's scenarios are `active` and green in CI; the team
   checklist is written from them. Failures become a fix wave; two fix cycles at
   most.
5. The owner decides the release to `main` and the next phase.

## Testing

- Each wave ships tests for what it changes, at the cheapest layer (contract or
  unit test in vam before a UI test).
- Scenarios run on every PR in CI and anywhere with `scripts/e2e/run.sh`
  (`--smoke`, `--scenarios a,b`, `--all`). A KVM host is only for scenarios marked
  `needs: firecracker`, run once at a phase close if the phase touched them.
- Test-first guardrails: scenarios assert behaviour, not implementation; a new
  scenario is `pending` and red for the right reason; a build wave never edits a
  `then` to pass (a spec change is a PR line approved by the orchestrator, or the
  owner for product behaviour); 3 to 8 scenarios per phase; no plan numbers in
  scenario names.
- Stop rules: a fix cycle counts only if a scenario moves; two cycles without
  movement, stop and report; no rerun without a change; mock model only.
- Other live Lima or journey runs only when the owner asks, capped at 15 minutes
  and two fix cycles.
- Agents' "tests pass" claims are re-run, not trusted. vam has CI (including the scenarios).

## Running Rust tests with low disk

```
export CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0
cargo test -p <changed crate> [--test <suite>]
cargo fmt --all --check
```
Never delete build caches to free disk (`target/`, `src-tauri/target`); trim
`incremental/` only.

## Standing approvals

- Merge a verified PR into its base without asking again (playbook §4). Never
  into `main` without the owner's word.
- A PR whose code already passed CI in another PR (a release PR, or only docs
  commits since) merges without waiting for CI again (owner, 2026-10-16).
- Releases merge with a merge commit, never squash.
- CI: check once; never wait past 10 minutes.
- Cleanup after every merge (playbook §5).
- Lima cache: `/tmp/vam-drive9/templates` and `/tmp/vam-drive9/sessions` may be
  cleared when Lima's disk fills, between runs. Never `/tmp/lima`.

## Rules that bit us (do not repeat)

- **Never commit in a main checkout another session is using.** On 2026-10-15
  the orchestrator committed docs in the vam checkout while a phase-close session
  had a fix staged there; commit swept it onto the base under a
  docs title. With one orchestrator per epic, **each orchestrator runs in its own
  worktree on its epic's base branch**, never in the main checkouts.
- **Commit signing:** commits are signed. When signing fails, ask the owner.
  Never change git config, never `--no-gpg-sign`.
- **Never delete build caches** (`target/`, `src-tauri/target`) to free disk;
  trim `incremental/` only. Never touch `/tmp/lima` in the VM.
- **Chat-flow changes** need a step in the owner's manual check at the phase
  close: unit tests missed the clipped playbook menu in H0.
- **Background long Bash** (builds, test suites, e2e runs) with
  `run_in_background`, and read the output file when notified.

- **Never commit after a merge without checking it finished.** On 2026-10-21
  the E10 orchestrator merged `main` into `feat/agent-runtime`; the merge stopped
  on conflicts and a scripted `git commit -a` committed the conflict markers and
  pushed them , breaking the base until resolved it. Check the
  merge's exit status and `git diff --name-only --diff-filter=U` before any
  commit; a merge with code conflicts goes to a wave session as a PR, not a
  scripted commit.

## Conventions

- Plain, short status updates: what changed, what is next, what needs a decision.
- Before any irreversible or outward action the owner has not asked for
  (deleting, force-pushing shared branches, publishing, releasing), ask first.

## Session hygiene

- One orchestrator session per phase. Close it when the phase is closed, after
  about a day, or once its context has been compacted. Update the tracker first.
- Archive each wave session as soon as its PR is merged.
- **Disk in cloud sessions (owner, 2026-10-25; replaces the 2026-10-21 rule).**
  The session disk is a fixed allowance (about 40 GB, cannot be raised); CI
  runners are separate. The shared `target/` is what fills it: Cargo keeps every
  feature variant of every crate (about 20 per crate after a few waves: process
  vamd, VM vamd, single-crate tests, the e2e runner, lockfile shifts after `main`
  merges) and never deletes old ones; full debug info and incremental state
  multiply it (29 GB on 2026-10-25).
  - **Build settings for every local build** (orchestrator and every wave agent,
    named in each brief), the same as CI: `CARGO_TARGET_DIR=/home/user/vam/target`,
    `CARGO_INCREMENTAL=0`, `CARGO_PROFILE_DEV_DEBUG=0`,
    `CARGO_PROFILE_TEST_DEBUG=0`. Never a `target` symlink in a worktree.
  - **One command, same flags, every time:** `scripts/ci/local.sh` (2026-10-30;
    first validated run: `--fast` on `main` in a cloud session,
    smoke 56/56, exit 0, 15 GB free before and after)
    sets these variables itself and runs the CI jobs in a fixed order, so
    every wave and every worktree hits the same cached artifacts instead of
    adding a feature variant. `--fast` before pushing; `--full` (the whole
    workspace suite) only to reproduce a CI failure (GitHub Actions runs it
    on every PR). Ad-hoc `cargo test`
    with other flags is what multiplies `target/`; avoid it.
  - **Worktrees share the one target dir and are removed after merge.** A
    wave agent's worktree (`isolation: worktree`, under `.claude/worktrees/`)
    must build with `CARGO_TARGET_DIR=/home/user/vam/target`, never its own
    `target/`; the orchestrator runs `git worktree remove --force <path> &&
    git worktree prune` right after the wave's PR merges, and deletes the
    local wave branch. Before starting a wave: `git worktree list` shows only
    the main checkout, and `df -h /home/user` is recorded in the wave row.
  - **Check:** `df -h /home/user` before a wave build and after merges. To see
    what grew: `du -sh target/debug/{deps,incremental,build} target/e2e-bin`,
    and count variants with
    `ls target/debug/deps | sed 's/-[0-9a-f]*\..*$//' | sort | uniq -c | sort -rn | head`.
  - **Cleanup ladder**, as far down as needed, telling the owner each time:
    (1) under 5 GB free: delete `target/debug/incremental`; (2) still under
    5 GB: delete older duplicate test executables (keep the newest per name);
    (3) still under 5 GB, or `target/` over 20 GB: `cargo clean` between waves
    (never while an agent builds), then one full rebuild (about 15–20 min).
    Never delete `~/.cargo` (registry, toolchain) or another session's files.
  - Agents never delete shared build output; they stop and report when under
    3 GB free.
- CI (2026-10-25): a job-level `env` cannot read the `runner` context. GitHub then
  rejects the whole workflow: the PR shows no checks at all and the branch gets a
  zero-job failed push run. Set runner paths in a step (`>> "$GITHUB_ENV"`).
- **GitHub Actions is the CI gate (owner, 2026-11-17; replaces the
  2026-10-30 "CI runs in the session" rule).** The repository is public, and
  Actions on standard GitHub-hosted runners (`ubuntu-latest`, every job in
  `rust.yml`, the microVM lane included) is free for public repositories.
  Running the full suite in a cloud session costs session time and disk for
  nothing. So:
  - Before pushing, the wave session runs only the fast local checks:
    `scripts/ci/local.sh --fast` (fmt, protos, manifests, `cargo check`,
    smoke) or `--scenarios a,b` for the wave's own scenarios, plus the unit
    tests of the crates it changed. This catches the obvious breaks without
    a 15-minute round trip.
  - The wave PR opens **ready for review** (not a draft) once those pass, so
    the full suite runs on Actions: fmt, clippy on both feature sets, the
    workspace tests, smoke, docs, DCO, and the microVM lane when a VM path
    changed. A draft is only for work still in progress (drafts run no CI).
  - The orchestrator subscribes to the PR and waits for the CI event instead
    of polling or re-running `--full` itself. It merges (merge commit into
    the base) only when every required check on the PR head is green and it
    has reviewed the diff. "Local CI" in a PR body is optional and is a claim,
    not evidence; the Actions run is the evidence.
  - `scripts/ci/local.sh --full` is for reproducing a CI failure locally, not
    a routine step. No `[skip ci]` commits; the `skip-vm-lane` label is only
    a time saver when a VM-path change cannot affect the lane.
  - The release PR into `main` runs the same suite plus the release gate and
    the lane, as before.
  - A red result on `main` after a release is fixed forward at once. After
    a release, the next phase starts from a branch level with `main`
    (fast-forward the released base to `main`); other open integration
    branches merge `main` in only as "Integration branches" rule 5 says.
  - **Never** push an empty commit or close and reopen a PR to re-run CI; a
    failing job gets at most one re-run, and only when the failure is clearly
    infrastructure (runner lost, network), never to "see". Never merge on red.
  - Limits that still apply: larger or self-hosted runners are not free; a
    first-time contributor's fork PR needs a maintainer's approval to run;
    fork PRs get no secrets; never use `pull_request_target` for untrusted
    code.