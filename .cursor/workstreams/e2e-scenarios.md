# Workstream: E2E scenarios (E9, from scratch)

```yaml
status: active
slug: e2e-scenarios
current_phase: 2 (Coverage: approvals, gates and chat; all waves merged; release to main awaits the owner)
design_docs:
  - design/scenarios.md
base: vam feat/e2e-scenarios, the VAM app feat/e2e-scenarios (both cut from main 2026-10-18)
orchestrator: cloud session (owner, 2026-10-18), not a local machine
```

Replaces the paused journeys harness (`the earlier journeys harness (removed)`). Nothing from it is resumed; what is reused is named
in Phase 1.

## Goal

Every phase of every epic is planned and proven through **scenarios**: runnable
given/when/then flows named by behaviour, written first, failing for the right
reason, then turned green by the build. Most scenarios run on every PR and in
cloud sessions (process-mode guest, scripted mock); only what needs a real
microVM runs on a KVM host. A session that runs them gets one command, a time
budget and a `brief.md` per failure, and stops when nothing moves. Done when
E10 Phase 2 is planned and closed with this flow (Phase 1 exit) and the old
coverage is rebuilt (later phases).

## Out of scope

- Product changes. Found on the way → handoff, conservative default.
- Real-model quality runs beyond keeping the benchmark runnable (`--real`, owner's word).
- A second Lima VM.

## Locked decisions

Do not re-litigate without editing this list (full text: `design/scenarios.md`).

1. **S1 Scenarios are the spec.** A phase is planned as the scenarios it adds,
   changes or retires (§8–9).
2. **S2 No plan numbers.** No scenario id, file, world piece, fixture or mock
   script carries an epic, phase, wave or story number. Plans point at
   scenarios; never the reverse. Tags are areas.
3. **S3 Independent.** Each scenario arranges its own state from named world
   pieces with run-unique names and cleans up. Any subset, any order.
4. **S4 vam owns the harness** (runner, scenarios, world pieces, mock scripts,
   CI). The VAM app holds unit tests and 10–15 UI scenarios that check the screen
   only and arrange data through the API.
5. **S5 Process mode is test-only**: cargo feature, refused by vamd without a
   test flag, mock model only, never in a release build.
6. **S6 Mock matches by step label** (`e2e-step:<label>`) and tool conditions;
   `userTurn` retired; one `mock.rule_matched` / `mock.unmatched` event per call.
7. **S7 Waits watch the run.** Every wait ends on the expected state or on the
   run reaching a terminal or error state. Every step has a budget.
8. **S8 Test-first guardrails**: behaviour not implementation; `pending`
   scenarios must fail at the expected step; builders never edit `then` to pass
   (spec change = PR line + orchestrator or owner approval); 3–8 scenarios per
   phase.
9. **S9 Stop rules**: a fix cycle counts only if a scenario moves; two cycles
   without movement → stop and report; no rerun without a change.
10. **S10 Old harness deleted** in Phase 1 (owner, 2026-10-18); its 159 checks
    are kept only as a behaviour backlog in the design doc.
11. **S11 The orchestrator is this cloud session** (owner, 2026-10-18). Waves
    run as background agents in worktrees per the playbook's model table.

## Status legend

- `[ ]` not started
- `[~]` in progress
- `[x]` done (code + tests, or merged)

## Phases

### Phase 0 — Spike: guest as a process

**Goal:** prove vamd + guest as a host process + scripted mock completes one
run with a tool call, without KVM.
**Depends on:** none
**Exit:** verdict and blockers in `design/scenarios.md` §13. **Done 2026-10-18.**

- [x] Background spike in a worktree, branch `spike/process-guest` (local). **PASS**, re-run by the orchestrator in 5 s. Blockers and timings: design §13.

### Phase 1 — Foundation: what E10 Phase 2 needs

**Goal:** the next phase of any epic can be planned and built test-first.
**Depends on:** Phase 0
**Status:** released to `main` 2026-10-18; `main` merged into `feat/agent-runtime` and `feature/agent-runtime`. **Phase 1 done.**  
**Plan:** approved 2026-10-18 (owner: all defaults, question 10 changed to the builtin with `ask`; new crate `crates/vam-e2e` approved)
**Exit:** smoke scenarios green on every vam PR in < 5 min and runnable in a
cloud session; a pending scenario is reported as "red for the right reason";
playbook and working agreement carry the scenario flow; old harness gone from
both repos; the VAM app has PR CI; released to `main`; `feat/agent-runtime` has
merged `main`.

**Scenario delta**

All five are new, on the process layer, and land as `pending` in wave 3. Wave 6
flips them to `active` once they pass. They rebuild the old smoke set (old
`p4-check12`, `p5-check3-ask`, `p5-check14`, `p4-check9`, `workspace-isolation`)
by behaviour. They do not port the old checks.

| Scenario | Change | Layer | Given | Why |
|---|---|---|---|---|
| `plain-chat-gets-a-reply` | new | process | `org-admin`, `agent-plain` | Smoke: a run starts, the mock answers, the run succeeds and the reply is in the events |
| `approval-suspends-and-resumes` | new | process | `org-admin`, `agent-with-guarded-tool` | Smoke: a guarded call parks the run (`approval: pending`); `approve latest` resumes it to `succeeded` with `tool.completed`. E10 Phase 2 changes this one (checkpoints by reference) |
| `gate-decided-finishes-the-run` | new | process | `org-admin`, `agent-plain`, `playbook-with-gate` | Smoke: a playbook run pauses at its gate; deciding it finishes the run and the step reads decided |
| `playbook-run-starts-with-inputs` | new | process | `org-admin`, `agent-plain`, `playbook-one-step` | Smoke: a run started with a playbook and its required input completes the step; a start without the required input is refused |
| `agent-workspaces-are-isolated` | new | process | `org-admin`, `agent-with-builtins` ×2 | Smoke: agent A's `write_file` note is in A's workspace; agent B's `read_file` of the same path fails, and B's workspace does not have it |

World pieces for Phase 1: `org-admin`, `agent-plain`, `agent-with-guarded-tool`,
`agent-with-builtins`, `playbook-with-gate`, `playbook-one-step` (new: one task
step, one required input). `org-member`, `released-agent-v1` and
`web-broker-mock` wait for the first scenario that needs them.

**Items**

- [x] **ES-1.1 Coverage backlog appendix.** Copy every check of the VAM app
  `e2e/journeys.yaml` into a new appendix A of `design/scenarios.md`, grouped by
  area tag and named by behaviour. Drop phase or check numbers, selectors and
  file names. For each check keep a likely layer (process / ui / firecracker).
  At the VAM app there are 164 `{name:}` entries: the 159 the design
  counts plus the 5 of `workspace-isolation`. State the count in the appendix.
  *Files:* `design/scenarios.md`. *Accept:* each journey id maps to appendix
  rows (the PR lists the count per journey); no plan numbers in the appendix
  (`grep -nE 'p[0-9]-check|h1-|E[0-9]+|SQU-' design/scenarios.md` finds none in
  the appendix). *Tests:* none (docs). *Serves:* S10.
- [x] **ES-1.2 Cleanup, the VAM app, plus minimal PR CI.** Delete `e2e/` (scenarios, `order.txt`,
  `lib/` and its tests, `mock-scripts/`, `journeys.yaml`, `report.ts`,
  `codemap.json`, `wdio.conf.ts`, `tsconfig.json`), `scripts/e2e/`,
  `scripts/codemap/`, `docs/e2e-*.md`, `.claude/skills/mac-check` and
  `mac-phase-close`. In `package.json`, remove the `e2e*`, `journeys:check` and
  `codemap*` scripts and the `@wdio/*` / `webdriverio` devDependencies, update
  the lockfile, and drop `e2e/**` from the `test` glob. Keep the unit tests, the
  `src-tauri` `e2e` cargo feature (WebDriver plugin) and
  `tauri.e2e.conf.json`. Add `.github/workflows/ci.yml` for PRs and pushes to
  `main` and `feat/**`: on ubuntu, Node 20 and pnpm 10 (as in `release.yml`), run
  `pnpm install --frozen-lockfile`, `CI=1 pnpm test` and `pnpm exec tsc --noEmit`.
  *Accept:* `git grep -nE 'e2e/lib|journeys|mac-check|codemap|wdio'` is empty
  outside `src-tauri`; tests and tsc pass. *Tests:* `CI=1 pnpm test`,
  `pnpm exec tsc --noEmit`, and the workflow parses
  (`python3 -c 'import yaml,sys;yaml.safe_load(open(sys.argv[1]))' .github/workflows/ci.yml`).
  *Serves:* S4, S10, exit "the VAM app has PR CI".
- [x] **ES-1.3 Cleanup, vam.** Delete `scripts/dev/seed-phase{4,5,6}.sh`,
  `seed-base.sh`, `check-h1-budgets.sh` and `scripts/dev/{phase4,phase5,phase6,h1}/`.
  Two tests in `crates/vam-orchestrator/tests/api_tests.rs` read
  `scripts/dev/phase4`:
  - `put_phase4_seed_fixtures_are_valid_specs` only checks seed fixtures, so
    delete it.
  - The skill-package scan (around line 3995) drops the `phase4/skills` dir and
    keeps `examples/`.

  *Accept:* `git grep -n 'scripts/dev/\(phase\|seed-\|h1\|check-h1\)'` finds hits
  only in `design/`, `archive/` and this tracker. *Tests:*
  `cargo test -p vam-orchestrator --test api_tests`, `cargo fmt --all --check`.
  *Serves:* S10, S2.
- [x] **ES-1.4 Mock matches by step label, with trace events (S6).** In
  `crates/vam-inference/src/mock_script.rs`:
  - Add `when.step`. It holds when the latest user message that carries
    `e2e-step:<label>` has that label.
  - `toolName`, `afterTool` and `notAfterTool` look only at messages after that
    labelled message, so a second message in the same thread starts clean.
    Unlabelled requests still match rules that have no `step`.
  - Remove `userTurn`; with `deny_unknown_fields`, an old script fails to load
    with an error that names the field.
  - Tool-call ids become `mock_<script>_<rule>_<step>_<n>`. They stay
    deterministic and never collide across turns.
  - The provider returns the match outcome. The inference broker in vamd
    (`crates/vam-orchestrator/src/brokers.rs`, next to `inference_completed_json`)
    appends one run event per scripted call: `vam.v1.mock.rule_matched
    {script, rule}` or `vam.v1.mock.unmatched {script, last_role, preview}`.

  Migrate every script that uses `userTurn`:
  - the `scripts/bench/mock-scripts/*.json` scripts (5 use it);
  - the inline scripts in `crates/vam-runtime/src/agent.rs` tests;
  - `crates/vam-orchestrator/tests/playbook_examples_tests.rs`.

  Update the doc comment, `docs/src/content/docs/runtime/evals.md` (if it
  mentions turns) and the mock section of the inference docs page. *Accept:* unit
  tests cover:
  - a guest-added user message (the empty-reply nudge, the "task is not marked
    complete" reprompt, the approval resume note) does not change which rule
    matches;
  - the second labelled message in a thread matches its own rules;
  - a script with `userTurn` is rejected;
  - one event per call, each with the rule index.

  *Tests:* `cargo test -p vam-inference`,
  `cargo test -p vam-orchestrator --lib brokers`,
  `cargo test -p vam-orchestrator --test playbook_examples_tests`,
  `cargo test -p vam-runtime --lib agent`, `scripts/bench/test.sh`.
  *Serves:* S6, S7 (the trace feeds `brief.md`).
- [x] **ES-1.5 Runner scaffold, and the five scenarios as `pending`.** New
  workspace crate `crates/vam-e2e` (lib plus bin `vam-e2e`). It depends on
  `vam-spec` and `vam-inference` (mock types) and may use the `vam_cli::client`
  lib. It **never depends on `vam-orchestrator`, `vam-core` or
  `vam-runtime`**, so the process-guest feature cannot be unified into
  workspace builds (S5). Contents:
  - Typed scenario and world-piece formats: `id, title, tags, needs, given,
    when, then, guards, mock, status, expectFail {step, class}`, with
    `deny_unknown_fields`.
  - The inline `mock:` block is checked by loading it through
    `vam_inference::MockScripts::from_scripts`.
  - A lint that enforces S2. Ids, file names, pieces and labels must not match
    plan patterns (`p\d`, `check\d`, `phase`, `wave`, `E\d+`, `SQU-`). Tags
    must come from the area list in design §3. A `pending` scenario must have
    `expectFail`.
  - The five scenario files in `e2e/scenarios/<id>.yaml` and the six pieces in
    `e2e/world/<piece>.yaml` (including any skill or playbook package they need,
    named by what it is).
  - `vam-e2e check` validates the catalog and runs nothing.
  - Add the crate to the `AGENTS.md` crate table and to the workspace members.

  *Accept:* `vam-e2e check` passes on the catalog and fails, with a line per
  error, on fixtures covering a plan-numbered id, an unknown tag, a pending
  scenario without `expectFail`, and a mock rule using `userTurn`. *Tests:*
  `cargo test -p vam-e2e`, `cargo run -p vam-e2e -- check`. *Serves:* S1–S3,
  S8; writes the five scenarios red-first.
- [x] **ES-1.6 Runner engine.** `vam-e2e run --scenarios a,b | --tags x |
  --all [--budget <dur>] [--out <dir>]`, wrapped by `scripts/e2e/run.sh`
  (builds the binaries it needs, then runs).
  - **vamd.** Start one throwaway vamd per invocation: an `env -i`-style env,
    temp state dirs, a loopback port, the bootstrap key and the mock-scripts dir.
    Write every selected scenario's inline `mock:` before vamd starts, because
    scripts load at startup. Readiness is a `/v1/health` wait, and the runner
    asserts the node's guest mode once ES-1.8 exposes it.
  - **World pieces** are applied through the HTTP API with run-unique names
    (`<piece>-<scenario>-<runTag>`), release included, and cleaned up after each
    scenario (S3).
  - **Steps:** `chat` (new thread, or `thread: continue`), `approve` /
    `deny` latest, `decide-gate`, `start-playbook {inputs}`,
    `stop-session` (terminate), `restart-vamd`, and `expect` on run state,
    event (type + fields), approval, step state, and workspace file present,
    absent or equal.
  - **Waits (S7).** Every wait ends on the expected state **or** on the run
    reaching a terminal or error state. It polls, so the commit can land after
    the run is terminal (spike: 1.5–1.7 s).
  - **Budgets:** chat step 30 s, scenario 2 min, run 10 min (design §7).
  - **Output.** One line per scenario; `summary.json`; exit 0 or 1.
  - **`brief.md` per failure:** the failed step, expected vs observed, the
    last run events, the `mock.*` trace, the vamd and guest log tails, the
    suspect files from `guards`, and one class from
    `product | scenario | harness | env`.
  - **Pending.** A pending scenario that fails at `expectFail.step` with
    `expectFail.class` reports `red (right reason)` and does not fail the run.
    One that passes, or fails elsewhere, reports `red (wrong reason)` and fails
    the run.
  - **Self-tests.** Harness self-test fixtures in `crates/vam-e2e/tests/`, not
    in the catalog, cover `thread: continue`, `stop-session` and
    `restart-vamd`, so E10 Phase 2 can write on them.

  *Accept:*
  - Unit tests against a stub HTTP server (`httpmock` is already a dev-dep in
    vam-cli) cover each step, both wait exits, the budget overrun, the class
    picked, and the right- and wrong-reason reporting.
  - A preflight checks the node's guest mode. The field is absent until ES-1.8,
    so the preflight fails each `needs: process` scenario at its first step with
    class `env` ("process guest not available"). Against today's vamd,
    `scripts/e2e/run.sh --all` therefore reports all five as
    `red (right reason)`; their `expectFail` says so until wave 6.

  *Tests:* `cargo test -p vam-e2e`, `scripts/e2e/run.sh --all`. *Serves:*
  S3, S7, S8, S9, exit "pending reported as red for the right reason".
- [x] **ES-1.7 Process mode, guest side** (spike blockers B1, B3, B5, B6).
  - **B1.** Keep the spike's `UnixMux` in `crates/vam-vsock/src/guest.rs`: it
    emulates Firecracker's hybrid vsock (`CONNECT <port>` / `OK <port>`,
    `<base>_<port>` dial paths). Redo the selection: an explicit transport,
    `Vsock` or `Uds { base }`, read once from config and never chosen by
    auto-fallback. Delete the `/tmp/vam-mock-vsock-*` fallback in both `bind`
    and `guest_dial`. Move the tests that relied on it to the explicit `Uds`
    transport: `crates/vam-vsock/tests/loopback.rs` (which then also passes in
    this container) and the 3 uses in `crates/vam-runtime/src/agent.rs` tests.
  - **B3.** Redo the spike's `VAM_GUEST_MODE=process` runtime checks as a
    non-default cargo feature, `process-guest`, on `vam-vsock` and
    `vam-runtime`. With the feature on, the PID-1 work in
    `src/bin/vam-runtime.rs` is compiled out: mounts, `kill(-1)`, `reboot`,
    `umount`, `clock_settime`, `/dev/urandom` seeding and `bring_up_loopback`.
    Split it into an init module, so the feature build holds none of it. Process
    mode refuses to start without its UDS base, and never tries real vsock. The
    instance id and CID come from env only in this mode.
  - **B5.** Compute the Landlock rule paths from `workspace_root()` and
    `skills_mount_root()` before `fork`. The `pre_exec` closure must not
    allocate or read env.
  - **B6.** In process mode, the egress proxy address and socket path come from
    env and are per guest. It never falls back to `0.0.0.0` (`egress.rs:341`).
    The VM build keeps its current behaviour.
  - Keep the spike's `exec.rs` cwd change (`workspace_root()`) as is.
  - Default builds and the `release-guest` profile do not change.

  *Accept:*
  - `cargo build -p vam-runtime` (default features) contains no `UnixMux` or
    process-mode code path. A test or a `strings` check on the binary shows that
    process mode is absent.
  - The feature build has no call to `reboot`, `kill(-1)` or `umount`.
  - The Landlock rules follow `VAM_WORKSPACE_ROOT`, with a unit test.
  - Two process guests get distinct egress endpoints, with a unit test.

  *Tests:* `cargo test -p vam-vsock`, `cargo test -p vam-vsock --features
  process-guest`, `cargo test -p vam-runtime`, `cargo test -p vam-runtime
  --features process-guest`, `cargo build -p vam-runtime --profile
  release-guest` (host target is enough). *Serves:* S5.
- [x] **ES-1.8 Process mode, host side, and the smoke set goes green** (spike
  blockers B2, B4, B7; S5 runtime gate).
  - **ProcessVmm.** Keep the spike's `crates/vam-core/src/process_vmm.rs` shape:
    `env_clear()` plus the guest env, its own process group, SIGSTOP and SIGCONT
    for pause, errors for snapshot calls, and a chown to 1000:100 when root.
    Gate it behind `vam-core/process-guest`, and pass `PATH` through so that
    `bun` and `sh` resolve.
  - **B2.** Redo the deleted `drive9.ext4` as `Drive9Backend { Ext4Image,
    Directory }` on `WorkspaceManager` (`crates/vam-storage/src/workspace.rs`,
    `disk.rs`). In directory mode, `materialize` skips `build_image`, `commit`
    skips `extract_image`, and the skills copy is read-only. This also removes
    the e2fsprogs need in CI.
  - **B4.** Redo the global env check in `manager.rs` as `adopt(pid)` on the
    factory and the `Vmm` trait. Every `FirecrackerVmm::new_adopted` call site
    (`manager.rs:52`, `:1939`, `:2155`, `:2204`) goes through the configured
    VMM.
  - **B7.** `ProcessVmm::shutdown` and the reconcile sweep remove the state dir,
    pid file and base socket, also when the guest has already exited.
  - **vamd** (`vam-orchestrator/process-guest`). `VAMD_PROCESS_GUEST_BIN`
    selects `ProcessVmm` in `VmmFactory`. vamd refuses to start in this mode in
    any of these cases:
    - `VAMD_E2E_MOCK_SCRIPTS` is not set;
    - `VAMD_LISTEN` is not on loopback;
    - `VAMD_IMPORT_ENV_SECRETS` is set.

    It refuses any run whose resolved model is not `mock-model`, logs a WARN on
    every boot, and reports `guestMode: process` in `GET /v1/node` (a vam-spec
    field). Without the feature, the variable is rejected at startup with a clear
    error.
  - Turn the five scenarios `active` and fix the harness until they pass,
    without editing any `then` (S8). A needed spec change is a PR line.

  *Accept:*
  - `scripts/e2e/run.sh --tags smoke` is green in this container, as root, in
    under 5 min with warm builds. Two consecutive runs leave no files under the
    process-guest state dir and no `/tmp/vam-vsock-*` files.
  - Unit tests cover the directory backend, `adopt` through the factory, the
    leftover cleanup, and each vamd refusal.
  - A default-feature vamd rejects `VAMD_PROCESS_GUEST_BIN`.

  *Tests:* `cargo test -p vam-storage`, `cargo test -p vam-core --features
  process-guest`, `cargo test -p vam-orchestrator --lib`,
  `cargo test -p vam-spec --test examples_json`, `scripts/e2e/run.sh --tags
  smoke`. *Turns green:* all five. *Serves:* S5, S7.
- [x] **ES-1.9 CI job and release gate, vam.** Add a `scenarios` job to
  `.github/workflows/rust.yml` with the same `paths-ignore` and a non-root
  runner, its own `rust-cache` key and the low-disk env. It installs `bun` (open question 10) and builds `vamd`
  and `vam-runtime` with `process-guest`, plus `vam` and `vam-e2e`, then runs
  `scripts/e2e/run.sh --tags smoke --budget 5m` and uploads `summary.json` and
  every `brief.md` as an artifact. A `release-gate` step (or
  `scripts/e2e/check-release-gate.sh`) fails the build in any of these cases:
  - a workspace `Cargo.toml` enables `process-guest` on a dependency;
  - `default` lists it;
  - the default-feature `vamd` or `vam-runtime` binary contains
    `VAMD_PROCESS_GUEST_BIN` or the UDS-mux marker.

  *Accept:* the job is green on the wave's own PR, and its wall time (warm cache)
  is in the PR body. A deliberately broken scenario on a scratch commit fails the
  job and attaches `brief.md` (shown in the PR, then reverted). *Tests:* the
  release-gate script runs locally; the workflow YAML parses. *Serves:* S5,
  exit "smoke green on every vam PR in < 5 min".
- [x] **ES-1.10 Scenario flow in the playbook and working agreement.** Apply
  design §9–10 to `.cursor/workstreams/phase-playbook.md` (§2, §3 step 2, §6) and
  `working-agreement.md` (Testing):
  - The breakdown writes a **Scenario delta** first.
  - **Wave 0 "scenarios"** (Sonnet low) lands the new and changed scenarios as
    `pending`, red for the right reason.
  - Each build-wave row names the scenarios it turns green.
  - Every PR runs the process scenarios in CI, and any cloud session can run
    them with `scripts/e2e/run.sh`.
  - The progress-log **scoreboard** columns: green, red, fix cycles, wall time.
  - The stop rules (S9) and the spec-change rule (S8).

  Remove the "E9 is paused" lines. Write a short
  `docs/src/content/docs/…/scenarios.md` (and add it to the sidebar in
  `docs/astro.config.mjs`), or a README in `e2e/`, on how to write and run a
  scenario. *Accept:* the orchestrator can brief E10 Phase 2's
  breakdown from the playbook alone. *Tests:* none (docs). *Serves:* S1, S8,
  S9, exit "playbook and working agreement carry the scenario flow".

**Waves**

Each wave runs as a background agent in a worktree of the named repo (S11). It
runs only container-runnable checks. Known local failures it should not chase:
`hsp_uds::uds_socket_acl_deny_fails_fast` fails as root, and until wave 5 so
does `vam-vsock` `loopback` (the container has `/dev/vsock`). No local
`cargo test --workspace`; CI runs it.

| Wave | Items | Repo | Model | Budget | Depends on | Status | PR |
|---|---|---|---|---|---|---|---|
| 1 | ES-1.1, ES-1.2, ES-1.3 | vam, the VAM app | Sonnet low | 60 min | — | [x] | the VAM app |
| 2 | ES-1.4 | vam | Sonnet medium: crosses `vam-inference` and `vam-orchestrator` (new run event types) and migrates scripts in three places | 60 min | — | [x] | — |
| 3 | ES-1.5 | vam | Sonnet low | 45 min | 2 | [x] | — |
| 4 | ES-1.6 | vam | Sonnet medium: a new crate's engine (vamd lifecycle, waits, classes); the plan cannot fully specify the step semantics | 120 min | 3 | [x] | — |
| 5 | ES-1.7 | vam | **Sonnet high**: the guest side of the vsock transport, Landlock rule paths, the egress proxy bind and compiling out PID-1 init are the security boundary (§2). An auto-fallback or a missed guard ships an unsandboxed or host-wide path | 120 min | 1 | [x] | — |
| 6 | ES-1.8 | vam | Sonnet medium: 4 crates, a vam-spec field and the teardown path. The S5 gate is fully specified by the spike report, and the reviewer reads the gate code, not the summary. Step up to high on retry | 150 min | 4, 5 | [x] | — |
| 7 | ES-1.9, ES-1.10 | vam | Sonnet low | 45 min | 6 | [x] | — |

Waves 1 and 2 are independent. Wave 1 goes first so that old and new never live
side by side (design §11). Wave 5 needs only wave 1 and could run before 3 or 4
if a wave stalls. After the close, the orchestrator releases both repos to `main`
(owner's word) and merges `main` into `feat/agent-runtime`.

**Spike code.** Branch `spike/process-guest` (pushed to vam origin as a reference) is never
merged. Waves 5 and 6 port from it as each item says. Its `run.sh` and fixtures
are replaced by the runner and `agent-workspaces-are-isolated`. Delete the
branch and worktree after wave 6 merges.

**Open questions** (defaults in bold)

1. Runner language and home: **a new Rust crate, `crates/vam-e2e`, typed on
   `vam-spec`** (design default; a new crate needs the owner's word). The code
   supports it: `vam_cli::client` is already a lib, the API types are in
   `vam-spec`, and keeping the runner off `vam-orchestrator` keeps the test
   feature out of workspace builds.
2. Scenario file format: **YAML** (design §3.1), through `serde_yaml` 0.9. This
   is the workspace's first YAML dependency. The alternative is JSON, like the
   mock scripts and agent fixtures, with no new dependency.
3. Step-label semantics: **`step` is the latest labelled user message, and the
   tool conditions only look after it.** This makes multi-message threads (E10
   Phase 2) script cleanly.
4. `scripts/bench/run-bench.sh --mock` (spike side finding): **defer.** Phase 1
   migrates the bench mock scripts off `userTurn` and keeps
   `scripts/bench/test.sh` green, but leaves the live `--mock` path as a known
   issue. The fix touches E10's premise: the U1 agent has an empty role and
   instructions, which the API now rejects, and nothing releases the agent.
   Record it in the E10 handoff; it is fixed when bench folds into the runner
   (Coverage phase).
5. `seed-base.sh` and the VAM app code map: **delete both.** World pieces replace
   the seed. The code map serves only UI selectors, and the UI layer can rebuild
   it.
6. "< 5 min" means **the `scenarios` job's wall time on a warm `rust-cache`**.
   A cold cache, about 3–4 min of build alone (spike: 3 m 22 s for 3 crates),
   may exceed it.
7. The VAM app PR CI scope: **`pnpm test` and `tsc` only.**
   `cargo test --manifest-path src-tauri/Cargo.toml` needs the webkit/GTK
   packages on Linux and waits for the UI-layer phase.
8. Scenarios run **one at a time in one vamd** in Phase 1, with per-guest names
   already in place (B6), so `--jobs N` can come later without a guest change.
9. E10 Phase 2 needs `thread: continue`, `stop-session` and `restart-vamd`
   before any catalog scenario uses them: **harness self-test fixtures in
   `crates/vam-e2e/tests/` prove them in CI**. There is no sixth catalog
   scenario (S8: 3–8 per phase).
10. **Owner, 2026-10-18: builtin with the per-tool `ask` permission; no `bun` in CI.** Original default: `agent-with-guarded-tool` uses a skill tool
    marked `approvalRequired`**, as the old `create_record` did, so the
    approval path matches what users hit. It needs `bun` on the CI runner
    (`oven-sh/setup-bun`). The fallback is a builtin with the per-tool `ask`
    permission, which needs no `bun`.

### Phase 2 — Coverage: approvals, gates and chat

**Goal:** the approval, gate and chat rows of Appendix A run as process-layer
scenarios on every PR, merged into a few longer flows that also guard what E10
Phase 2 (context, checkpoints, follow-ups) is most likely to break.
**Depends on:** Phase 1
**Status:** breakdown approved 2026-10-18 (owner: all defaults). All three waves merged; 10 of 11 scenarios green, the restart scenario `pending` on a product bug. Release to `main` awaits the owner's word. **Product bug found:** a run resumed after a vamd restart loses its agent identity (see progress log, W1).
**Exit:** the six scenarios below are `active` and green in CI (`--smoke` runs
every `needs: process` scenario, so they join the PR gate with no CI change);
the three smoke scenarios this phase extends are unchanged; any product bug
found is reported to the owner with its `brief.md`.

These scenarios describe behaviour that **already exists**, so there is no wave
0: each lands `active` and should pass when written. A wave picks mock rules and
exact field paths from a real run while writing a scenario (authoring), but never
drops or weakens an asserted behaviour to make it pass. A `product`-class failure
is a bug found: open question 2.

**Scenario delta**

| Scenario | Change | Layer | Given | Why |
|---|---|---|---|---|
| `released-agent-approvals-are-once-only` | new | process | `org-admin`, `agent-with-guarded-tool` | On a released version the approval names its lock, refuses "Always", runs once on Allow once and saves nothing (the next call asks again); Deny once keeps the tool from running |
| `always-allow-and-always-deny-are-remembered` | new | process | `org-admin`, `agent-with-guarded-write-and-delete` (new) | On the owner's draft run "Always allow" and "Always deny" are offered with the target, save a rule to the draft, and decide later calls in the same thread and in a new draft run without asking; the released version still asks |
| `long-thread-resumes-after-approval` | new | process | `org-admin`, `agent-with-guarded-tool` | Four continued turns with large replies are each answered in one thread; the fifth suspends on the guarded tool and Allow once finishes it with the reply. **E10 Phase 2 guard** (context growth, checkpoint at the fifth turn) |
| `gate-offers-its-options-and-stop-ends-the-run` | new | process | `org-admin`, `agent-plain`, `playbook-with-gate` | The pending gate carries its question and options; an option it does not offer is refused; the terminal option ends the run and skips the rest; a follow-up message in the same thread gets a plain reply. **E10 Phase 2 guard** (follow-up after a playbook run) |
| `parked-approval-survives-a-restart` | new | process | `org-admin`, `agent-with-guarded-tool` | A run parked on an approval is still pending after vamd restarts, and approving it then finishes the run. **E10 Phase 2 guard** (approval resume from a checkpoint) |
| `member-chats-with-a-released-agent` | new | process | `org-admin`, `org-member` (new), `agent-with-guarded-write-and-delete` (new) | A member's chat on a released agent replies; an allowed tool runs without a prompt; a guarded tool offers the member Allow once only; the owner's draft still asks afterwards |

Unchanged and not duplicated: `approval-suspends-and-resumes` (approve once,
fresh thread), `gate-decided-finishes-the-run` (non-terminal option `go`),
`plain-chat-gets-a-reply` (one turn), and `playbook-run-starts-with-inputs`
(the required-input refusal).

New world pieces (new files only; no existing piece changes):
- `org-member`: kind `org`, role `Member`. The engine creates a user and mints its
  key through the API (ES-2.2).
- `agent-with-guarded-write-and-delete`: released. `tools.default: deny`,
  `builtin:read_file` allow, `builtin:write_file` ask, `builtin:delete_file` ask.

**Appendix A mapping** (by behaviour text)

- `released-agent-approvals-are-once-only`: A.4 "A locked tool card offers Allow
  once and Deny only, and names the lock" (the released-version lock); A.4 "Allow
  once runs the tool and saves nothing"; the deny half of "approve and deny".
- `always-allow-and-always-deny-are-remembered`: A.4 "The approval card offers
  Allow once, Always allow and Always deny, and names the target"; A.4 "Always
  allow continues the run and the card says what it saved" (the continue and the
  saved rule's effect; the card text is UI); A.4 "A second call in the same
  thread does not prompt"; A.4 "A new draft run does not prompt"; A.3 "A draft
  workspace accepts a chat"; A.3 "A prompt that calls a skill tool runs, and the
  run is of the draft agent" (with a builtin tool on a draft run; the skill-tool
  half is deferred, below).
- `long-thread-resumes-after-approval`: A.3 "A thread with the guarded agent opens
  and its first turn is answered", "The second / third / fourth turn is answered
  in the same thread"; A.4 "The fifth turn suspends on a guarded tool and the
  in-chat card offers Allow once"; A.4 "Allow once runs the tool, the run
  succeeds and the reply shows, with no continuation error".
- `gate-offers-its-options-and-stop-ends-the-run`: A.5 "Starting a playbook with
  its input pauses at the gate, shown as a card in the chat" and "A playbook run
  paused at its gate shows the gate as a card in the chat thread" (the card's
  data: question and options on the pending gate); A.5 "Deciding the gate in the
  chat finishes the run and the panel shows it decided" (with the terminal option;
  the panel is UI); A.3 "A second message in the thread has no form and gets a
  reply".
- `parked-approval-survives-a-restart`: no single Appendix row; it is the
  "resume" in this phase's coverage line and the case E10 Phase 2 checkpoints
  change.
- `member-chats-with-a-released-agent`: A.9 "A chat on the released agent is
  accepted and replies"; A.9 "A member run of the released version runs an
  allowed tool without a prompt"; A.9 "A different tool offers the member Allow
  once only"; A.9 "The admin draft is unchanged after the member run" (as: the
  owner's draft run still asks with Always offered).
- A.4 "Read-only and local mutable tools run, and a mutable external tool
  suspends for approval": **partly**. Read-only and local tools running without a
  prompt is in the member and always scenarios. The default suspend of a mutable
  external tool is deferred: no builtin is mutable external
  (`crates/vam-runtime/src/primitives.rs:635-685`), so it needs a skill or MCP
  tool, and owner Q10 (Phase 1) keeps `bun` out of CI.

**Appendix A rows dropped or moved, and why**

- UI only (out: process layer): A.3 "Picking a playbook … shows its input form",
  "Test in chat opens the draft chat with a draft banner", "The chat journey view
  shows the tool call and the policy decision" (its process half, the
  `policy.decided` event, is asserted in the always scenario); A.5 "The playbook
  runs panel shows the same gate".
- Already covered by a smoke scenario: A.3 "An empty required input is flagged and
  nothing is sent" (`playbook-run-starts-with-inputs`); A.5 "Deciding the gate in
  the chat finishes the run" (`gate-decided-finishes-the-run`).
- Observations, **dropped** (not turned into assertions): A.4 "The stored
  checkpoint size after the fifth turn is recorded": the runner has no checkpoint
  read-out and E10 Phase 2 changes checkpoints to by-reference; the long thread
  carries the large replies (13 000 filler bytes per turn, as the old check did)
  under the scenario's 2 min budget, which stands in as the budget. A.4 "Legacy
  approval cards still appear on an agent in permissions mode": legacy policy mode
  belongs with the legacy-conversion rows of Phase 4.
- Moved to Phase 3 (draft vs released): A.3 "A draft agent has its default
  playbook set" and "The run resolves the playbook from the admin draft". They
  need an agent piece that points at another piece's run-unique URN, which world
  pieces cannot express yet.
- Deferred until a skill-tool world piece exists (needs `bun`, owner's word): the
  skill-tool half of A.3 "A prompt that calls a skill tool runs"; the source-rule
  lock ("skill marks the tool approvalRequired",
  `crates/vam-security/src/permissions.rs:114-122`) in A.4's locked-tool row.

**Scenario sketches**

Mock pattern for every tool call below: `{step: <label>, stepStart: true}` replies
with the tool call, and a later `{step: <label>}` rule replies with text. That way
the reply after an approval, a denial note or a reprompt cannot loop back into
the tool.

1. `released-agent-approvals-are-once-only` (tags `approval`, `chat`).
   *When:* chat "write first.txt" → `approval: pending, tool: write_file` with
   `lockedReason: "released version: approvals are once only"` → `approve` with
   "Always allow", expected **refused** (409 from `plan_remember`), the approval
   still pending → `approve: latest` → run `succeeded`, `first.txt` present →
   chat (new thread) "write second.txt" → `approval: pending` again (nothing
   was saved) → `deny: latest`. *Then:* run `succeeded` (open question 3),
   `hitl.decided` with the REJECTED decision, `second.txt` absent.
   *Guards:* `crates/vam-orchestrator/src/api/mod.rs` (`decide_approval`
   :1638, `list_approvals` :1597, `resume_run_after_rejection` :3340),
   `crates/vam-orchestrator/src/session_targets.rs` (`scoped_row` :295,
   `plan_remember` :581), `crates/vam-orchestrator/src/workflow.rs`,
   `crates/vam-runtime/src/hitl_checkpoint.rs`.
2. `always-allow-and-always-deny-are-remembered` (tags `approval`,
   `permissions`). *When:* draft chat "write note.txt" → pending `write_file`
   with `alwaysOffered: true`, `target: "builtin:write_file"` → approve with
   "Always allow" → run `succeeded` → draft chat, `thread: continue`, "write
   note2.txt" → run `succeeded` with no prompt, `policy.decided` for
   `write_file` with disposition ALLOW → draft chat (new thread) "delete
   note.txt" → pending `delete_file`, `alwaysOffered: true` → deny with "Always
   deny" → run `succeeded` → draft chat (new thread) "delete note.txt, then write
   third.txt". *Then:* run `succeeded` with no prompt; `policy.decided`
   `delete_file` DENY and `write_file` ALLOW; `note.txt` and `third.txt`
   present; a **released** chat (not draft) "write released.txt" still gives
   `approval: pending` with the released-version `lockedReason` (the rules live
   on the draft until a release). Note: proto3 JSON omits `alwaysOffered: false`,
   so a locked approval is asserted through `lockedReason`, an offered one
   through `alwaysOffered: true`. *Guards:* `session_targets.rs`
   (`plan_remember`, `save_remembered_rule` :652,
   `apply_agent_rule_to_session`), `crates/vam-orchestrator/src/catalog_publish.rs`
   (`update_draft_permissions`), `crates/vam-orchestrator/src/catalog_resolve.rs`
   (`run_resolve_context` :180), `crates/vam-orchestrator/src/policy_legacy.rs`
   (`draft_run` :107), `crates/vam-orchestrator/src/security.rs` (decision and
   `policy.decided` :1222-1293), `crates/vam-security/src/permissions.rs`,
   `proto/vam/v1/hitl.proto` (`Remember` :51, claims :38-44).
3. `long-thread-resumes-after-approval` (tags `chat`, `approval`, `context`).
   *When:* chat turn 1 (new thread), then turns 2-4 with `thread: continue`; each
   run `succeeded` and its `inference.completed` `data.outcomePreview`
   `{contains: "turn <n>"}`; mock replies carry `fillerBytes: 13000` → turn 5,
   continue, "write note.txt" → `approval: pending, tool: write_file` →
   `approve: latest`. *Then:* run `succeeded`, `tool.completed write_file`,
   `inference.completed` preview contains "turn five done", `note.txt` present.
   A `mock.unmatched` anywhere fails it as class `scenario` (runner rule).
   *Guards:* `crates/vam-runtime/src/agent.rs`,
   `crates/vam-runtime/src/hitl_checkpoint.rs`,
   `crates/vam-orchestrator/src/run_checkpoint.rs`,
   `crates/vam-orchestrator/src/run_context.rs`,
   `crates/vam-orchestrator/src/api/mod.rs` (`resume_run_after_approval`).
4. `gate-offers-its-options-and-stop-ends-the-run` (tags `gate`, `playbook`,
   `chat`). *When:* `start-playbook` (label `run`) → pending gate with
   `approvalMessage: "Go on?"` and `requestedArgs.options: [go, stop]` →
   `decide-gate` option `later`, expected **refused** (not in the response
   schema's enum) → step `confirm` still `waiting` → `decide-gate` option `stop`
   → run `succeeded`, step `confirm` `decided`, step `finish` `skipped` → chat,
   `thread: continue`, "what happened?". *Then:* run `succeeded`,
   `inference.completed` preview contains the follow-up reply. Mock: the gate
   smoke's `prepare` rule, plus a text rule for the follow-up label. *Guards:*
   `crates/vam-orchestrator/src/workflow.rs` (`gate_response_schema` :3587,
   `insert_gate_approval` :3810, terminal outcome skips the rest, test at :5105),
   `crates/vam-orchestrator/src/api/mod.rs` (`resume_after_hitl_decision`
   :3363, gate branch of `decide_approval`).
5. `parked-approval-survives-a-restart` (tags `approval`, `context`). *When:*
   chat "write note.txt" → `approval: pending, tool: write_file` →
   `restart-vamd` → `approval: pending, tool: write_file` (still listed) →
   `approve: latest`. *Then:* run `succeeded`, `tool.completed write_file`,
   `note.txt` present. Uses `restart-vamd` in the catalog for the first time
   (open question 4). *Guards:* `crates/vam-orchestrator/src/reconcile.rs`
   (parked runs :88-220), `crates/vam-orchestrator/src/manager.rs`,
   `crates/vam-orchestrator/src/run_checkpoint.rs`,
   `crates/vam-runtime/src/hitl_checkpoint.rs`,
   `crates/vam-core/src/process_vmm.rs`.
6. `member-chats-with-a-released-agent` (tags `members`, `chat`, `approval`).
   *When:* as `member`: chat "write note.txt" → `approval: pending, tool:
   write_file` with the released-version `lockedReason` → `approve: latest` (the
   decision uses the run's identity) → run `succeeded`, `note.txt` present → as
   `member`, `thread: continue`, "read note.txt" → run `succeeded`,
   `tool.completed read_file` and `policy.decided read_file` ALLOW (no prompt).
   *Then:* as the admin (default identity), draft chat "write admin.txt" →
   `approval: pending, tool: write_file` with `alwaysOffered: true` (the
   member's run changed nothing on the draft). *Guards:*
   `crates/vam-orchestrator/src/api/users_admin.rs`,
   `crates/vam-orchestrator/src/api/users_keys_admin.rs`,
   `crates/vam-orchestrator/src/keys.rs` (`MEMBER_SCOPES` :71),
   `crates/vam-orchestrator/src/owner.rs`, `crates/vam-orchestrator/src/api/auth.rs`,
   `session_targets.rs` (`scoped_row`).

**What the runner lacks today** (all in `crates/vam-e2e`; nothing in vamd).
Scenarios 3 and 5 run on today's runner. Scenarios 1, 2, 4 and 6 need ES-2.2:

- Every run is a release run: `run_body` sends no `agentRef.version`
  (`crates/vam-e2e/src/engine.rs:560-580`). "Always" is offered only on the
  agent owner's draft run (`session_targets.rs:295-312`, `:594`), so no path to
  it exists.
- `approve` / `deny` send APPROVED / REJECTED only, with no `remember`
  (`engine.rs:1030-1031`), and a refused decision always fails the step
  (`engine.rs:729-790`).
- `expect: {approval}` accepts only `pending` and only checks
  `targetToolName` (`engine.rs:900-925`). `fields` passes `check` on an approval
  but the engine ignores it silently. There is no expectation on a pending gate.
- `org` pieces do nothing (`engine.rs:261`): one identity (the bootstrap key)
  runs everything, and every run of an agent uses workspace `volume://<agent>`.
- `stop-session` / `restart-vamd` are documented as "reserved for the engine's
  self-tests" (`scenario.rs:128-131`), though `check` accepts them.

**Items**

- [x] **ES-2.1 Scenarios on today's runner.** (`parked-approval-survives-a-restart` landed `pending` on a product bug, owner-approved) Write
  `e2e/scenarios/long-thread-resumes-after-approval.yaml` and
  `e2e/scenarios/parked-approval-survives-a-restart.yaml` as `active`, per sketches
  3 and 5. No world piece or runner change. *Files:* those two files only.
  *Accept:* `cargo run -p vam-e2e -- check` ok;
  `scripts/e2e/run.sh --scenarios long-thread-resumes-after-approval,parked-approval-survives-a-restart`
  passes; `scripts/e2e/run.sh --smoke` green (the 5 old plus these 2). A
  `product` failure follows open question 2. *Turns green:* both.
- [x] **ES-2.2 Runner: draft runs, remembered decisions, refusals, approval
  fields, a second identity.** `crates/vam-e2e` only (`scenario.rs`, `engine.rs`,
  `world.rs`, `check.rs`, `api.rs`, `tests/`), plus `e2e/README.md` lines.
  - `chat` and `start-playbook` take `draft: true`, which sends
    `agentRef.version {major: 1, prerelease: "draft"}`.
  - `approve` and `deny` keep `latest` and gain a map form
    `{always: true, refused: true, as: <org alias>}`. `always` sends
    `remember: REMEMBER_ALWAYS_ALLOW` / `REMEMBER_ALWAYS_DENY`. With
    `refused: true` the step passes only on a 4xx answer while the approval is
    still pending, and it skips the "leaves PAUSED" wait. `decide-gate` takes
    `refused: true` the same way.
  - `expect: {approval: pending, tool, fields}` matches `fields` (exact dotted path, as
    for events) on the approval claims. A new family `expect: {gate: pending,
    step: <id>?, fields}` does the same for a gate (kind
    `APPROVAL_KIND_GATE`). `check` rejects `fields` on a family that does not
    read it.
  - Identities: an `org` piece with `role: Member` (or any non-owner role)
    creates a user named `<piece>-<scenario>-<runTag>` (`POST /v1/users`) and
    mints its key (`POST /v1/users/:id/keys`). `chat` and `start-playbook` take
    `as: <org alias>`; the default stays the bootstrap key. Approvals are listed
    and decided with the run's identity. A non-default identity's workspace is
    `volume://<agent>-<identity>`. `check` makes sure `as` names an org alias.
  - A `run:` wait for any state other than `paused` fails once the run has sat
    `PAUSED` with a pending approval past the settle time. It fails as class
    `product` ("parked on approval for <tool>") instead of running out the step
    budget as `harness` (open question 6).
  - The `stop-session` / `restart-vamd` doc comments allow catalog use.

  *Accept:* self-tests against the fake vamd in `crates/vam-e2e/tests/` cover
  each new form: the draft version in the body, `remember` in the decide body,
  the refused pass and fail paths, approval and gate `fields`, the member's
  user and key calls with that key used for its run and its decisions, the
  parked fast-fail, and the new `check` errors (fixtures). Existing catalog
  unchanged; `cargo test -p vam-e2e`, `cargo run -p vam-e2e -- check`,
  `cargo fmt --all --check`, and `scripts/e2e/run.sh --smoke` still green.
  *Turns green:* none (enables ES-2.3, ES-2.4).
- [x] **ES-2.3 Approval scenarios.** New piece
  `e2e/world/agent-with-guarded-write-and-delete.yaml`. Scenarios
  `released-agent-approvals-are-once-only` and
  `always-allow-and-always-deny-are-remembered`, `active`, per sketches 1-2.
  *Files:* those three files. *Accept:* `check` ok;
  `scripts/e2e/run.sh --scenarios released-agent-approvals-are-once-only,always-allow-and-always-deny-are-remembered`
  passes; `--smoke` green. *Turns green:* both.
- [x] **ES-2.4 Gate and member scenarios.** New piece `e2e/world/org-member.yaml`.
  Scenarios `gate-offers-its-options-and-stop-ends-the-run` and
  `member-chats-with-a-released-agent`, `active`, per sketches 4 and 6 (the
  member scenario reuses ES-2.3's agent piece). *Files:* those three files.
  *Accept:* `check` ok;
  `scripts/e2e/run.sh --scenarios gate-offers-its-options-and-stop-ends-the-run,member-chats-with-a-released-agent`
  passes; `--smoke` green (all 11). *Turns green:* both.

**Waves**

Each wave runs as a background agent in a worktree of vam, base
`feat/e2e-scenarios`, plan-only first turn. No `scripts/bench`, no existing
scenario or world piece, no product code. Known local failure not to chase:
`hsp_uds::uds_socket_acl_deny_fails_fast` as root.

| Wave | Items | Turns green | Repo | Model | Budget | Depends on | Status | PR |
|---|---|---|---|---|---|---|---|---|
| 1 | ES-2.1 | `long-thread-resumes-after-approval`, `parked-approval-survives-a-restart` | vam | Sonnet low | 45 min | — | [x] | — |
| 2 | ES-2.2 | — (runner self-tests) | vam | Sonnet medium: new step and expect semantics plus a second identity through the engine; the users/keys calls and claim field paths are confirmed on a real vamd, so the plan cannot fully specify them | 90 min | — (after wave 1) | [x] | — |
| 3 | ES-2.3, ES-2.4 | `released-agent-approvals-are-once-only`, `always-allow-and-always-deny-are-remembered`, `gate-offers-its-options-and-stop-ends-the-run`, `member-chats-with-a-released-agent` | vam | Sonnet low | 75 min | 2 | [x] | — |

Wave 1 goes first because it needs no harness and gives the earliest
product-bug signal on the two E10-sensitive flows. Wave 2 does not depend on it
and could go first if wave 1 stalls. Release to `main` at the phase close, on the
owner's word.

**Open questions** (defaults in bold)

1. ES-2.2 grows runner code, beyond "new files only": **yes, confined to
   `crates/vam-e2e` and `e2e/README.md`**. No vamd, spec or CI change. Without
   it, scenarios 1, 2, 4 and 6 cannot be written, and the phase shrinks to
   scenarios 3 and 5.
2. A scenario that fails as class `product` when written: **report it to the owner
   with its `brief.md`, and land the scenario as `pending` with `expectFail` at the
   failing step (class `product`). The `then` stays exactly as specified, and the
   handoff gets a line naming the bug.** CI stays green and the spec is kept. The
   owner picks the epic that fixes it, and that fix flips the scenario to
   `active`.
3. Run state after a once-only Deny: **`succeeded`**. vamd resumes the guest with
   the denial note (`resume_run_after_rejection`) and the model answers. If vamd
   ends the run otherwise, that is a product rule for the owner to confirm, not a
   `then` to change silently.
4. `restart-vamd` in a catalog scenario (first use; self-tests only so far):
   **allow it**. ES-2.1 proves it on a real process vamd. If it flakes in CI as
   class `env`, rerun once per the stop rules, then hold the scenario `pending`
   and report.
5. Deferred rows (the mutable-external default suspend, skill-tool calls,
   source-rule locks, draft playbook resolution): **defer**. The skill-tool rows
   wait until a skill-tool world piece is allowed (`bun`, owner's word; Q10 stands).
   Draft playbook resolution goes to Phase 3.
6. Fast fail of a `run:` wait on a run parked on an approval: **yes** (ES-2.2).
   A missed "no prompt" then fails in about 5 s as `product` instead of 30 s as
   `harness`. The existing smoke scenarios never wait on a parked run, so they
   are unaffected.
7. Member creation: **through the API by the `org-member` piece** (user with a
   random password and a minted key, run-unique name). Decisions use the run's
   identity. A CLI profile per identity is not needed at the process layer.
8. The smoke scenarios' `guards` name directories that do not exist
   (`crates/vam-orchestrator/src/approvals/**`, `runs/**`, `workspaces/**`,
   `playbooks/**`): **leave them unchanged in this phase** (owner rule: existing
   scenarios unchanged). Record them for the selection phase, where `--changed`
   reads guards. New scenarios use real paths.

**Phase close** (2026-10-21, orchestrator)

- Scenarios: of the six new, five are `active` and green in CI on `feat/e2e-scenarios`; the catalog is 11 (`--smoke` 10 pass + 1 red right reason, 63 s locally, CI `scenarios (smoke)` green on). `parked-approval-survives-a-restart` is `pending` (`expectFail then.1 product`) on the product bug in the handoff; flip it to `active` with the fix.
- Spec change (owner, 2026-10-21): a remembered Always deny withholds the tool from the model, so `always-allow-and-always-deny-are-remembered` asserts no prompt and the file intact instead of a DENY event.
- Taken as intended (owner informed): a run with no version by the agent's owner follows the owner's draft once one exists; the admin key cannot read a member's runs or files.
- No `needs: firecracker` scenario touched; no local microVM run needed.

**Team checklist** (team instance, app + CLI, about 10 minutes)

Setup: deploy `vamd` from `main` after the release; no env var. Sign in as an org admin; have one member account.

1. Chat with a released agent that has a guarded write tool; ask it to write a file. You see an approval card offering Allow once and Deny only, saying approvals on a released version are once only.
2. Click Allow once. The run finishes and the file is in the workspace.
3. Ask it to write a second file; the card appears again (nothing was saved). Click Deny. The run finishes and the second file is not there.
4. Open the agent's draft ("Test in chat") and ask it to write a file. The card offers Always allow and Always deny and names `builtin:write_file`. Click Always allow; the run finishes.
5. In the same thread, ask for another write: no card. In a new draft chat, ask again: no card.
6. Start a playbook with a gate. The chat shows the gate card with its question and options. Choose stop: the run ends, the remaining step shows skipped; a follow-up message in the thread gets a reply.
7. Have a five-turn chat in one thread with the guarded agent; on the fifth turn ask for a write, Allow once. The reply shows and the run succeeds.
8. As the member, chat with the released agent and ask for a write: the card offers Allow once only. Approve; the file is written. Ask it to read the file: no card.

Report: pass, or the step number with a screenshot or the error text.

### Later phases (planned one at a time)

Coverage is split by area so each phase stays within 3–8 scenarios (S8),
ordered by what E10 Phase 2 (context, checkpoints, follow-ups) is most likely
to break. Each merges the backlog rows of Appendix A in `design/scenarios.md`
into fewer, longer scenarios; each is released to `main` on its own so other
epics pick the scenarios up. Coverage adds new files only: world pieces are
extended, never changed.

- **Phase 2 — Coverage: approvals, gates and chat**: all waves merged
  2026-10-21; release to `main` awaits the owner. Approve and deny,
  "always allow", gate decisions and resume, multi-turn chat, a member's chat.
- **Phase 3 — Coverage: release and versions.** Draft vs released, release,
  pin, bump, unchanged release refused, member sees only released.
- **Phase 4 — Coverage: permissions and members.** Defaults, locked rules,
  effective view and `explain`, tamper refusal, member isolation.
- **Phase 5 — Coverage: catalog and workspace.** Skill and playbook cards,
  CLI agrees with the API, workspace files.
- **Benchmark fold** (`scripts/bench` into the runner, pass^k with `--real`):
  after E10 Phase 2 closes, because `scripts/bench` is E10's.
- **UI layer:** 10–15 the VAM app UI scenarios, Linux CI if possible, else a macOS host.
- **Firecracker lane and selection:** `needs: firecracker` scenarios on Lima
  (egress, isolation, checkpoints), `--changed` from guards, PR guard check.

## Progress log

| Date | Phase | What landed |
|---|---|---|
| 2026-10-18 | — | Tracker opened from scratch. Design `design/scenarios.md`. Owner: orchestrate from the cloud session, base `feat/e2e-scenarios` in both repos, delete the old harness. Old tracker and handoff moved to `archive/e2e-journeys-v1*.md`. Phase 0 spike running. |
| 2026-10-18 | 0 | Spike **PASS**: real vamd + `vam-runtime` as a host process + scripted mock; `write_file`/`read_file` and `run` succeeded, workspace committed; whole script 5 s, run 0.4–0.7 s. Seven blockers recorded (design §13), none needs KVM. Branch `spike/process-guest` local. |
| 2026-10-18 | 1 | Breakdown approved by the owner (Q10: builtin `ask`, no `bun`; crate `vam-e2e` approved). Process per wave: background agent plans first, orchestrator validates, owner approves, agent builds and opens the PR, orchestrator verifies and merges. Spike branch pushed as reference. Wave 1 planning. |
| 2026-10-18 | 1 W1 | **Merged**  and . Old harness gone (the VAM app 90 files, ~25k lines; vam `scripts/dev/`, 36 files); Appendix A holds the 164 old checks by behaviour (130 + 34 sign-in steps). The VAM app PR CI added (Node 22), green in 39 s. Orchestrator re-ran: the VAM app 627/627, tsc clean, lockfile only removals; vam api_tests 106, fmt ok; CI fmt + protos green, workspace job merged on local verification past 5 min. Deviations accepted: `@wdio/tauri-plugin` kept (main.tsx import), `catalog` area added. Scoreboard: 0 fix cycles, wave ~5 min build. Remote branch delete blocked by the git proxy (403); delete `e9/w1-cleanup` in both repos by hand. |
| 2026-10-18 | 1 W2 | **Merged**. Mock matches by `step` label plus `stepStart` (owner addition); tool conditions scoped after the label; `userTurn` removed (load error names file and field); ids `mock_<script>_<rule>_<step>_<n>`; events `vam.v1.mock.rule_matched` / `vam.v1.mock.unmatched` (reason `no_rule`/`tool_not_offered`/`not_loaded`, 80-char redacted preview) via the outbox; bench scripts, Oracle test and agent.rs scripts migrated. Orchestrator re-ran: vam-inference 58, brokers 16, events 10, playbook_examples 6, vam-runtime 178, bench 73, fmt ok; CI fmt + protos green, workspace job merged on local verification past 5 min. Known: `cargo clippy -D warnings` fails in vam-spec generated code (pre-existing, not in CI); preview cut before redaction (test-only, accepted). Scoreboard: 0 fix cycles, build ~8 min. |
| 2026-10-18 | 1 W3 | **Merged**. `crates/vam-e2e` (lib + bin; deps vam-inference, serde_yaml 0.9; no path to vamd/core/runtime), `vam-e2e check` (all errors, exit 0/1/2), plan-number lint narrowed by the owner to number-carrying tokens, 5 smoke scenarios + 6 world pieces `pending` (expectFail when.0 class env), engine seams; `run` stub. Orchestrator re-ran: vam-e2e 29 tests, check ok, dep-tree clean, fmt ok; CI fmt + protos green, workspace job merged on local verification past 5 min. Carried to wave 4: (1) `assistant.message` is not a vamd event (real: `vam.v1.tool.completed`, `vam.v1.tool.failed`, `vam.v1.run.state`, `vam.v1.playbook.step`, `vam.v1.inference.completed`); `check` must validate event names against vamd's list, and the scenario is corrected while pending (spec change approved by the orchestrator); (2) confirm a playbook start can carry the step label. Scoreboard: 0 fix cycles, build ~6 min. |
| 2026-10-18 | 1 W4 | **Merged**. Runner engine: throwaway vamd (loopback port, temp state, random bootstrap key, mock scripts, health wait, teardown on drop), allows `mock-model` via `PUT /v1/node/allowed-models` (owner change), world pieces with run-unique names, steps chat (new/continue), approve/deny, decide-gate, start-playbook (label in `inputs.prompt`, no product change), stop-session, restart-vamd, expect families; waits end on expected or terminal state (5 s settle), budgets 30 s step / 120 s scenario / 10 min run; `summary.json`, `briefs/<id>.md`, classes env > scenario > harness > product; preflight on `/v1/node guestMode`; `--smoke` flag (owner change); `scripts/e2e/run.sh`. Event names checked against `KNOWN_EVENTS` (19), drift test in vam-e2e reads vamd source as text (owner change). Spec change (orchestrator-approved, pending): plain-chat `assistant.message` → `inference.completed` `outcomePreview`. Orchestrator re-ran: vam-e2e 61 tests, check ok, dep tree clean, fmt ok, live `run.sh --all` 5 × RED (right reason) at when.0 class env, exit 0; CI fmt + protos green, workspace job merged on local verification past 5 min. **Wave 6 must:** tighten the `fields` matcher (exact key path and exact value by default; `contains` only when asked; today it matches any key at any depth by substring), and confirm on real runs the step-state words, approval/gate field names, `outcomePreview`, and the gate mock rule after resume. Scoreboard: 0 fix cycles, build ~12 min of 120. |
| 2026-10-18 | 1 W5 | **Merged**, CI fully green (workspace 6.5 min). Guest side of process mode: explicit `GuestTransport {Vsock, Uds}` (Uds only behind vam-vsock `uds-transport`, `process-guest` alias; on in test builds via dev-dependency features; auto-fallback removed, `loopback` now passes here); PID-1 init moved verbatim to `src/bin/guest_init/pid1.rs` (orchestrator diffed all 572 lines: only doc comments and 7 order-preserving wrappers new), `process.rs` under `process-guest` (refuses PID 1, needs `VAM_GUEST_VSOCK_UDS`/`VAM_INSTANCE_ID`/`VAM_GUEST_CID`, exit 2 on bad config, no reboot/umount/clock/urandom/`kill(-1)`); `compile_error!` for process-guest on musl; `LandlockPolicy` from configured roots, built before fork; `kill_process_group` refuses pgid 0/1; per-guest loopback-only egress in process mode. New `scripts/e2e/check-guest-build.sh` (binary symbol and string guards) PASS. Orchestrator re-ran: vam-vsock 14 (both builds), vam-runtime 185+8+3 / 184+9+3+3, musl default ok and feature blocked, vamd tests compile, fmt ok. Carried to wave 7: the release gate reuses `check-guest-build.sh` and allows `uds-transport` only in dev-dependencies; stale `rust.yml` loopback comment. Out of scope, recorded for E8 Phase 2: VM proxy `0.0.0.0:3128` fallback (design Part V §V.5 q4). Scoreboard: 0 fix cycles, build ~75 min of 120. |
| 2026-10-18 | 1 W6 | **Merged**, CI fully green. **The five smoke scenarios are `active` and PASS on real process guests, no KVM.** Orchestrator re-ran `scripts/e2e/run.sh --smoke` twice: 5/5 PASS each (107 s with build, **16 s warm**; 1.6–3.8 s per scenario), leftover check ok both times. Host side: `ProcessVmm` (vam-core feature `process-guest`; cleared env, own process group, start-time-verified signals, exact-name sweep), Drive9 `Directory` backend (no ext4 image), the four `manager.rs` adoption sites through the factory + `adopt()` with today's Firecracker behaviour kept (owner change; jailer-dir leak on adopted VMs recorded as a finding for a Mac phase). S5 gate (`process_mode.rs`, read line by line): default vamd exits on any `VAMD_PROCESS_*`; feature vamd refuses without mock scripts, non-loopback or unparsable listeners, env-secret import at any value, snapshot restore on, jailer on, non-absolute guest bin; `create_run` and the inference broker (owner change: required) refuse any model but `mock-model`; WARN banner; `NodeStatus.guest_mode` (additive proto field 12). New `check-vamd-build.sh` and `check-leftovers.sh` PASS. Gate tests: 8 refusal bin tests + 1 run-refusal API test (feature), 2 default-build rejection tests. Spec changes (listed in PR): `fields` matcher exact dotted path + exact value, `{contains:}` explicit; mock replies end with plain text (`task_complete` not offered by vamd); playbook scenarios call `step_complete`; gate scenario rule for the post-resume "Missing required steps" message. Harness fixes: unique idempotency keys, workspace-relative file paths, `VAMD_TELEMETRY_CAPTURE=redacted-full`, `run.sh` build of all packages. No product bugs found. Disk ran out during the wave; orchestrator removed the finished spike worktree (8.6 GB, branch kept on origin). Carried to wave 7: release gate checks the full markers (`VAMD_PROCESS_GUEST_BIN`, `ProcessVmm`, `PROCESS GUEST MODE`, `process-guest`), not the `VAMD_PROCESS_` prefix; reuse both check scripts; allow `uds-transport` only in dev-dependencies; stale `rust.yml` loopback comment. Scoreboard: 0 fix cycles on the PR, 0 product bugs. |
| 2026-10-18 | 1 W7 | **Merged**, CI all green. `rust.yml`: `scenarios (smoke)` job (own cache, `run.sh --smoke`, artifact `summary.json` + `briefs/` on failure, 20 min timeout) and `release gate` job (manifest check every PR; binary guards on PRs into `main` and pushes to `main`); new `check-manifests.sh`, `check-release-gate.sh`; stale loopback comment fixed. Playbook, working agreement, `_template.md`: scenario delta first, wave 0 "scenarios", "Turns green" and "Budget" columns, plan-only first turn per wave, test-first guardrails, stop rules, scoreboard, phase close = scenarios green in CI, Mac only for `needs: firecracker`, cloud orchestrator allowed. `e2e/README.md`. **Failure path proven on GitHub** (orchestrator, owner-approved): a throwaway commit breaking plain-chat's expected preview turned the job red with exactly `FAIL plain-chat-gets-a-reply at then.1, class product` (4 others PASS, leftovers ok) and uploaded artifact `scenarios-39123715526` (3.7 KB); reverted, green again. CI times: scenarios 3 m 08 s cold, **1 m 15 s warm** (target < 5 min met); release gate 24–29 s. Scoreboard: 0 fix cycles. |
| 2026-10-18 | 1 | **All Phase 1 waves merged** (–). Scoreboard for the phase: 7 waves, 0 fix cycles on any PR, 0 product bugs found, 5 scenarios active and green in CI. Remaining for the exit: release `feat/e2e-scenarios` to `main` in both repos (owner's word), then merge `main` into `feat/agent-runtime`; then E10 Phase 2 is planned with a scenario delta. Owner to set branch protection: `scenarios (smoke)` required on `feat/**`, `release gate` on `main`. Remote branches `e9/w*` and `spike/process-guest` to delete by hand (the git proxy refuses deletes). |
| 2026-10-18 | 1 | **Released to `main`** on the owner's word: (merge; CI green incl. the first full `release gate` run, 5 m 46 s, and `scenarios (smoke)` 4 m 13 s on a cold `main` cache) (merge; `pnpm test` + `tsc` green). Both `feat/e2e-scenarios` bases fast-forwarded to `main`. `main` merged into vam `feat/agent-runtime` (docs conflicts in `design/scenarios.md`, `design/README.md` resolved to `main`; index rows combined) and the VAM app `feature/agent-runtime`. **Phase 1 closed.** Next: E10 Phase 2 planned scenario-first. Owner: branch protection and stale remote branches (handoff). |
| 2026-10-18 | 2 | Phase 2 breakdown written (this commit, `docs(e2e-scenarios): Phase 2 breakdown`), awaiting owner approval. Six new `active` scenarios (no wave 0: behaviour exists), two new world pieces (`org-member`, `agent-with-guarded-write-and-delete`), three waves (low, medium for the runner growth confined to `crates/vam-e2e`, low). Found: "Always" is reachable only on the owner's draft run and the runner starts release runs only; approval `fields` are silently ignored by the engine; smoke guards name non-existent directories. Scoreboard: 0 green, 0 red, 0 fix cycles, ~25 min breakdown. |
| 2026-10-18 | 2 | Breakdown **approved by the owner with all eight defaults**. Wave 1 planning. |
| 2026-10-18 | 2 W1 | **Merged**. `long-thread-resumes-after-approval` **active, PASS** (4 continued turns with 13 KB replies, fifth suspends, Allow once finishes). Harness fix (owner-approved scope widening): the mock's `step_label` reads the **last** `e2e-step:` marker, because vamd folds earlier turns into the new user message (`vam-inference/src/mock_script.rs`, + unit test); without it no multi-turn scenario can script. **Product bug found:** `parked-approval-survives-a-restart` landed `pending` (`expectFail then.1 product`, `then` unchanged, owner-approved per Q2): after a vamd restart the approval stays pending and Approve finishes the run and writes the file, but the resumed session spawns as `urn:vam:agent:unresolved` (resolution `NoPolicyDeclared`, builtins only) and emits no `tool.completed`. Belongs to resume-from-checkpoint (E10 Phase 2's area); recorded in the handoff. Orchestrator re-ran: fmt ok, vam-inference 61, check ok, `--smoke` 6 pass + 1 red (right reason), 39 s, leftovers ok; CI rustfmt, protos, release gate, scenarios (smoke) green; workspace job merged on local verification past 5 min. Scoreboard: 1 green, 1 red (product, pending), 1 fix cycle (moved long-thread fail → pass), wall ~45 min. |
| 2026-10-19 | 2 W2 | **Merged**. Runner (`crates/vam-e2e` + `e2e/README.md` only): `chat`/`start-playbook` take `draft: true` (owner's draft, `agentRef.version` prerelease `draft`) or `draft: false` (pins release 1.0.0) and `as: <org alias>`; `approve`/`deny`/`decide-gate` map form `{always, refused, as}` (`remember` always paired with its decision); `refused: true` passes only on 4xx with the request still pending; `expect` `fields` matched on approval claims, new `gate: pending` family; `org` pieces with a non-Owner role create a real user + key through the API (random password, key in a redacting wrapper, self-test proves neither prints); run reads/decisions use the run's own key; Q6 parked fast-fail (class product after settle). Live findings (feed wave 3): admin key gets 404 on a member's run and files (per-user isolation, runner uses the run's key); invalid gate option → `400 InvalidArgument`; gate claims `approvalMessage`, `requestedArgs.options.N` as sketched; refused Always on a release → `409 AlreadyExists` "once only"; **a version-less run by the agent's owner follows the owner's draft once one exists** (taken as intended; sketch 2's released step must use `draft: false`); a workspace binds one agent version, so draft runs use `<agent>-draft` and identities `<agent>-<alias>`. Two stale catalog-size asserts made size-agnostic. Orchestrator re-ran: fmt ok, vam-e2e 83 tests, check ok, `--smoke` 6 pass + 1 red (right reason), 44 s, leftovers ok; CI rustfmt, protos, release gate, scenarios (smoke) green; workspace job merged on local verification past 5 min. Scoreboard: 0 scenarios turned (runner wave), 1 red (product, pending, unchanged), ~3 fix cycles each moving, wall ~70 min of 90. |
| 2026-10-21 | 2 W3 | **Merged**. Four scenarios `active` and PASS: `released-agent-approvals-are-once-only`, `always-allow-and-always-deny-are-remembered`, `gate-offers-its-options-and-stop-ends-the-run`, `member-chats-with-a-released-agent`; world pieces `agent-with-guarded-write-and-delete`, `org-member`. Runner fix (Q1): a plain Deny sends `rejectionReason` (vamd requires it; only Always deny worked before), self-test `decision_bodies`. Gate follow-up first failed on a case mismatch in the scenario (`contains` is case-sensitive), not a product bug: fixed in the mock reply, `then` unchanged. **Spec change (owner, option A):** a remembered Always deny withholds the tool from the model, so the scenario asserts no prompt, `note.txt` intact and `third.txt` written instead of a `policy.decided delete_file DENY`. Confirmed paths: `hitl.decided` `data.decision`, `policy.decided` `disposition`, gate `requestedArgs.options.N`. Orchestrator re-ran: fmt ok, vam-e2e 84 tests, check ok (11 scenarios, 8 pieces), `--smoke` 10 pass + 1 red (right reason), 63 s, leftovers ok; CI rustfmt, protos, release gate, scenarios (smoke) green; workspace job merged on local verification past 5 min. Scoreboard: 4 green, 0 new red, 3 fix cycles (each moved a scenario), wall ~40 min of 75. |
| 2026-10-21 | 2 | **All Phase 2 waves merged**. Scoreboard for the phase: 3 waves, 5 scenarios turned green, 1 `pending` on a product bug (restart loses agent identity), 2 harness bugs fixed (mock label, deny reason), 1 owner spec change, ~7 fix cycles, 0 without movement. Phase close block and team checklist written. Next: release `feat/e2e-scenarios` to `main` on the owner's word. |
