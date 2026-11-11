# E2E journeys (E9): a standing harness that tests VAM's mechanics cheaply

Plan of record, 2026-10-16. Epic E9, the standing E2E harness track (not a stage;
`project_development_order`). Tracker: archived (superseded by `e2e-scenarios`).
Milestones T0–T5; wave 0 (finish H1) in milestone **H1 Runtime hotfix**.

Repos: `vam` (base `feat/agent-experience`) and the VAM app (base
`feature/agent-experience`). The harness itself lives in the VAM app
(`docs/e2e-mac.md`, `scripts/e2e/`, `e2e/`); the model, vamd and fixtures in vam.

---

## 1. Why

The Mac harness (WebdriverIO driving the real Tauri app against vamd in
Lima) works: it found real bugs in Phases 4–6 and H1. But the way it is organised
now costs more than it returns, and the cost grows with every phase.

**Evidence, from the repos and the 2026-10-15 runs:**

1. **Tests are organised by phase, and nobody owns them after the phase.**
   34 spec files, ≈158 checks, named `p4-check7`, `p5-check14`, `p6-guided`,
   `h1-check1`. A later phase that breaks `p4-check9` has no reason to run it;
   the name says when it was written, not what it proves. `phase-check.sh` maps an
   issue id to a seed and a glob (→ `seed-phase4.sh`, `p4-*`), so a phase
   close runs its own phase and nothing that protects earlier work.
2. **Scenarios are coupled through state, by run order.** `e2e/scenarios/order.txt`
   carries the coupling in comments: check 8 must run before check 5 (5 attaches
   `github-tools` for good); admin's check 7 releases 1.1.0 before bob's check 9
   pins it; check 11 watches check 9's session; Phase 5 chains 4 → 6 → 8 → 9 → 13
   on one agent's draft. One failure poisons the rest; nothing can run alone or in
   another order; a rerun of one check needs the checks before it.
3. **The real model is used to test mechanics.** Every chat scenario runs on
   `anthropic/claude-haiku-5.5` via Vercel AI Gateway: slow (5 s per call; a gate
   card waited 170 s while the model looped), token-costly, and nondeterministic.
   To reach a 48 KiB approval checkpoint, h1-check1 asked for ~2,500-word essays
   per turn and still stopped at 17.9 KiB, because the guest's keyword nudges
 replaced the long answers. A mechanics test failed for a model-
   behaviour reason and passed without exercising its path.
4. **Heavy per-run setup and per-spec sign-in.** Each run restarts vamd with a
   fresh DB, seeds, and (unless told not to) rebuilds the app. Each spec file
   launches a new app and signs in through the UI: 34 launches per full run.
   The sign-in is the most common failure: `e2e-results/20261015-182553` and
   `…-224552` fail "<user> signs in" after 30 s with the form rendered but the
   webview `visibility: "hidden"`, `focus: false`, and every later check in the
   spec fails with it (0 of 14 in the 18:25 run).
5. **UI and microVM cost for API logic.** Several checks assert only CLI or HTTP
   results (`vam agent check --json`, `permissions get --effective`, the tamper
   refusal), but run inside an app session on a Lima VM. Those belong in CI.
6. **Fix sessions re-read the repo.** A failing check gives a screenshot and a
   message. To find the code, each fix session read both repos from the top. There
   is no map from a testid or a command to the files behind it.
7. **The H1 close took about an hour and 30% of a session quota** (owner,
   2026-10-15): 29 harness runs on 15 Oct (`the VAM app/e2e-results/20261015-*`), many
   of them reruns of 1–14 checks after one fix, several 0-of-14 runs from the
   sign-in flake, on the real model.

**What we want instead:** tests named and owned by the behaviour they protect,
independent of each other, running VAM's mechanics on a deterministic scripted
model, selected from the diff, cheap to rerun one at a time, and leaving a small
diagnosis bundle when they fail.

## 2. Goals and non-goals

Goals:

- A **journey catalog** that outlives phases: each journey says what user-visible
  behaviour it protects, which code it guards, how it is tagged, which tier it runs on.
- **Deterministic, fast, free** live runs: a scripted mock model by default.
- **Independent journeys**: any subset, any order, alone.
- **Cheap selection**: from the diff, from tags, or a phase close = the phase's
  journeys + `@smoke`; the full suite once before a release to `main`.
- **Cheap diagnosis**: a failure bundle and a code map, so a fix reads a handful
  of files.

Non-goals:

- Testing model quality in the default path. Real-model journeys are opt-in
  (`@agent-quality`), only on the owner's word.
- Replacing the owner's native smoke test (`docs/e2e-mac.md` §4).
- A CI runner for live journeys (they need KVM on the Mac via Lima). CI gets the
  contract tier.
- E1 Phase 7 or any product change. Product decisions found on the way go to the
  tracker with a conservative default.

## 3. Locked decisions

Do not re-litigate; edit this list (and the tracker's copy) to change one.

- **D1 Journeys, not phase checks.** A journey is named by the behaviour it
  protects (`approval-suspend-resume`), tagged by area and by the phase that
  introduced it (`e1p5`). Phase names are retired in T5; until then ids keep the
  old names.
- **D2 Three layers.** *Contract* (API/CLI results, no app, no microVM) runs in
  vam CI (`api_tests.rs`, `vam-cli` tests). *UI* (needs the app, no run) and *VM*
  (starts a run or session) run on the Mac harness. A check lives in the cheapest
  layer that can prove it.
- **D3 The scripted mock is test-only.** It is active only when vamd starts with
  `VAMD_E2E_MOCK_SCRIPTS=<dir>`. Without the flag, `PUT /v1/node/allowed-models`
  refuses `mock-model` and a run on `mock-model` is refused. vamd logs a warning
  banner when the flag is on. `start-vamd-lima.sh` forwards the flag only when the
  caller sets it.
- **D4 Stateless matching.** A script's reply is a pure function of the request
  (messages and offered tools): first matching rule wins, no server-side state.
  This survives guest restarts, HITL resume from a checkpoint and the
  playbook-finish loop.
- **D5 Selection by marker.** A request uses script `<id>` when a system message
  contains `mock-script:<id>`; journeys put it in their own agent's
  `systemInstructions`. Without a marker, the existing content fixtures behave
  as today (unit tests depend on them).
- **D6 Real model is opt-in.** Live runs use the mock. A journey that tests model
  behaviour is tagged `agent-quality` and runs only with `E2E_REAL_MODEL=1` on the
  owner's word. None exist today.
- **D7 Independent arrange.** One base seed (`scripts/dev/seed-base.sh`,
  idempotent) provides templates; each journey arranges what it needs via CLI/API
  with names unique to the run (`<what>-<journey>-<runTag>`). Multi-user journeys
  carry values in a journey-local `state.json`, never through another journey.
  `order.txt` is retired.
- **D8 One app launch and one sign-in per user per run.** A user's journeys share
  one WebDriver session; each journey starts with `ensureSignedIn()` and
  `resetToHome()`. The app window is brought to the front before sign-in.
- **D9 Warm by default.** A run reuses a healthy vamd built from the current vam
  sources, skips the seed when unchanged, and rebuilds the app and the rootfs only
  when their inputs changed. `--cold` forces the old behaviour. Build caches are
  never deleted.
- **D10 Every issue carries a "Journeys" section** (affected, new/changed,
  contract tests, live before merge). The PR repeats it.
- **D11 Run policy.** During a wave: unit/contract tests plus the issue's "Live
  before merge" journeys. Phase close: `pnpm e2e:phase-close <phase-tag>` = the
  phase's journeys + `@smoke`, one pass plus at most two fix cycles (~45 min,
  phase-playbook §6). Before a release to `main`: `pnpm e2e:run --all` once.
- **D12 Guards drive selection.** Each journey lists repo-prefixed path globs it
  guards (`app:`, `vam:`); `select.mjs` maps a diff to journeys + `@smoke`; a PR
  that touches a guarded path lists those journeys or says `none: <reason>`.
- **D13 Failure bundle, no secrets.** Each failed step writes a bundle
  (screenshot, visible testids, last IPC calls, vamd log slice, run events,
  failing spec line, class, suspects, `brief.md`). The e2e-only IPC buffer keeps
  command names, argument *key names*, timings and error messages, never argument
  values or results.
- **D14 Code map is generated, committed, and checked.** `scripts/codemap/gen.mjs`
  writes `e2e/codemap.json`; `pnpm codemap:check` fails when it is stale; every
  E9 issue regenerates it.
- **D15 Built locally by Haiku, one issue at a time.** All E9 issues carry
  `runs=local-session`, `model=claude-haiku-5-5`, `effort=high` (or `max` where
  the issue says why). Wave order; one branch and one PR per issue, merged by the
  building session into the base (never `main`); worktrees per issue; the handoff
  updated after each issue so a new session resumes without re-reading the repo.
  This plan was drafted by an Opus session (owner correction 2026-10-16: Opus
  plans, Haiku builds).

## 4. The model

### 4.1 Three layers

| Layer | Runs where | Proves | Cost |
|---|---|---|---|
| Contract | vam CI (`cargo test`), the VAM app `pnpm test` | HTTP routes, CLI output, store rules | seconds, every PR |
| UI journey | Mac harness, app + vamd, no microVM | what the app shows and sends | ~10–60 s per journey |
| VM journey | Mac harness, app + vamd + Firecracker in Lima, mock model | runs, sessions, approvals, gates, checkpoints, egress | ~30–120 s per journey |

### 4.2 Journeys catalog

`the VAM app/e2e/journeys.yaml`, one entry per journey: `id`, `title`, `specs`
(ordered `{file, user}`), `tier`, `tags`, `model` (`mock`|`none`|`real`),
`mockScript`, `seed`, `needs`/`provides` (only until T2 removes them), `guards`,
per-check tiers. `scripts/e2e/journeys.mjs` validates it, lists by tag/tier,
prints dependencies and resolves specs. A `globalGuards` block maps files that
affect every journey of a tier (`e2e/lib/**`, `vam:crates/vam-runtime/**` → all
`vm`).

### 4.3 Tags and tiers

Phase tags: `e1p3 e1p4 e1p5 e1p6 h1` (a new phase adds its own, e.g. `e1p7`,
`e10p0`). Area tags: `chat approval playbook gate release permissions workspace
guided catalog cli egress members`. Special: `smoke` (exactly five),
`agent-quality` (real model, opt-in). Tiers: `contract | ui | vm`.

**Smoke set** (five, one per critical path; replaces the `smoke.txt` that
phase-playbook §6 names): plain chat; tool approval that suspends and resumes;
playbook gate decided in the chat; playbook started from the composer; workspace
isolation between two agents (new journey, T1-3).

### 4.4 "Journeys" section in every issue

```markdown
## Journeys
- Affected: <journey ids or @tags>
- New or changed: <journey ids, or "none">
- Contract tests: <test names, or "none">
- Live before merge: <journey ids, or "none: <reason>">
```

A phase's exit criteria name its tag plus `@smoke`. When a phase adds behaviour, it
adds or extends journeys tagged with that phase; the journeys stay after the phase.

### 4.5 Selection and commands (the VAM app)

| Command | Runs |
|---|---|
| `pnpm e2e:run --journeys a,b` | those journeys |
| `pnpm e2e:run --tags chat,gate` | journeys with any of the tags |
| `pnpm e2e:run --changed` | `select.mjs` on both repos' diffs → guarded journeys + `@smoke` |
| `pnpm e2e:phase-close <tag>` | `--tags <tag>,smoke`, then prints the mac-check comment |
| `pnpm e2e:run --all` | everything except `agent-quality` (before a release to `main`) |
| flags | `--reverse` (order independence), `--cold` (fresh vamd, seed, builds) |

`run.sh` takes and releases the Lima lock. `guard-check.mjs --pr <n>` checks a
PR's "Journeys" section against its diff.

### 4.6 Diagnosis

On a failed step, `e2e-results/<ts>/bundles/<NN>-<user>-<journey>-<step>/`:
`screenshot.png`, `dom.json` (visible testids, aria-labels, headings, dialogs),
`ipc.json` (last 50 Tauri invokes, keys only), `error.txt` (message +
`file:line`), `meta.json`, `vamd.log.txt` (lines with the journey's ids + recent
errors), `events.json` (run events), `classification.txt`
(`e2e/known-failures.yaml`), `suspects.txt` (code map), `brief.md`. A diagnosis
session reads `brief.md` and the suspects, not the repo. `phase-close.sh` lists
each `brief.md` in its result comment.

## 5. Code map

`the VAM app/scripts/codemap/gen.mjs` (regex, no dependencies) builds
`the VAM app/e2e/codemap.json` from both repos:

```
data-testid / aria-label / #id ──▶ component file (src/**/*.tsx)
component ──▶ invoke("cmd")
Tauri command (src-tauri/src/lib.rs #[tauri::command]) ──▶ vamd::authenticated_*(…, "/v1/…")
vamd route (vam crates/vam-orchestrator/src/api/mod.rs .route) ──▶ handler fn ──▶ file:line
e2e spec ──▶ helpers (e2e/lib) ──▶ selectors, API paths, CLI commands ──▶ reaches{components, commands, routes, files}
```

Facts it starts from (2026-10-16): 85 Tauri commands in `src-tauri/src/lib.rs`;
~62 distinct `invoke(...)` call sites; 26 distinct `data-testid`s (e2e also uses
aria-labels and element ids heavily: `aria-label="Gate decision"`,
`[data-testid="chat-gate-card"]`, `#vamUrl`); vamd routes in
`crates/vam-orchestrator/src/api/mod.rs`. `scripts/codemap/query.mjs <term>`
prints the chain with `file:line`. Journeys' `guards` are derived from
`reaches.files`.

## 6. Scripted mock model

Where it lives: `crates/vam-inference/src/provider.rs` `MockProvider` (host side,
inside vamd's inference broker; the guest never sees it). The catalog already has
`mock-model` (`crates/vam-inference/src/catalog.rs`), the broker builds
`MockProvider` for provider `mock` (`broker.rs`), and the guest maps `mock-model`
to provider `mock` (`vam-runtime/src/agent.rs` `resolve_model_provider`). Today
the mock answers from content fixtures (`vam-playbook-finish-fixture`,
`vam-nudge-probe`, `run-echo`, a default sentence); those stay.

New (`crates/vam-inference/src/mock_script.rs`): JSON scripts, one per journey,
in `the VAM app/e2e/mock-scripts/<id>.json`:

```json
{ "id": "approval-suspend-resume",
  "rules": [
    { "when": {"lastRole": "user", "userTurn": 1},
      "reply": {"toolCall": {"name": "create_record", "args": {"title": "e2e"}}} },
    { "when": {"lastRole": "tool", "toolName": "create_record"},
      "reply": {"text": "Created the record."} },
    { "when": {}, "reply": {"text": "ok"} } ] }
```

Conditions: `lastRole`, `contains`, `userTurn`, `toolName`, `afterTool`,
`notAfterTool`. Reply: `text`, `toolCall`, `fillerBytes` (exact-size
deterministic ASCII; four 13 KB turns give the >48 KiB checkpoint the real model
could not reach), `delayMs`. A tool not offered, or no rule matching, is an
error chunk that names the script, so the failure shows in run events and the
bundle's classification. vamd loads the directory at startup when
`VAMD_E2E_MOCK_SCRIPTS` is set (D3); `mac-run.sh` sets it to
`$VAM_APP/e2e/mock-scripts` and restricts the node to `mock-model`. Lima mounts
`~` read-only at the same path, so vamd reads the Mac checkout directly.

## 7. Independence and speed

- **Arrange** (`e2e/lib/arrange.ts`): `uniqueName`, `arrangeAgent` (from a seeded
  template, with `mockScript`), `arrangeWorkspace`, `arrangePlaybookAllow`,
  journey-local `state.json`, session teardown.
- **Base seed** (`vam/scripts/dev/seed-base.sh`): the union of the phase 4–6
  fixtures as templates, idempotent. Phase seeds retire in T5.
- **One launch per user** (D8): specs grouped into one WDIO worker per user; the
  hidden-webview sign-in fixed by bringing the window to the front and by not
  relying on `isDisplayed()` while `document.visibilityState` is `hidden`.
- **Warm runs** (D9): stamp of the vam sources in Lima, seed hash, app-input hash
  in `src-tauri/target/.e2e-build-hash`, rootfs-input hash in
  `/tmp/lima/.rootfs-inputs-hash`; `report.md` states cold/warm and phase times.

## 8. Journey catalog

Classified (2026-10-16). The source of record is
`the VAM app/e2e/journeys.yaml`, checked by `node scripts/e2e/journeys.mjs --check`;
the table below copies it for reading. Tiers: **contract** = API or CLI results
only; **ui** = the app, no run or session; **vm** = starts or continues a run or
session. Per-check tiers are a heuristic over each check's body (run-start and
chat-turn helpers mark vm), provisional until T5 confirms them. `model` is
`none` for ui and contract and `real` for vm (T1 switches to `mock`). The
`Checks` column gives the count and the tier mix.

Smoke set (tag `smoke`, exactly five): `p4-check12-draft-chat` (plain chat),
`p5-check3-ask` (tool approval), `p5-check14-gate-in-chat` (gate in the chat),
`p4-check9-chat-playbook` (playbook from the composer), `workspace-isolation`
(todo, written in T1-3).

| Journey (id) | Users | Checks | Tier | Model | Tags | Seed | Needs |
|---|---|---|---|---|---|---|---|
| `p4-check1-playbooks` | admin, bob | 6 (ui 2, contract 4) | ui | none | e1p4 catalog playbook | seed-phase4 | - |
| `p4-check2-playbook-inspector` | admin | 7 (ui 2, contract 5) | ui | none | e1p4 playbook | seed-phase4 | - |
| `p4-check3-skill-capabilities` | admin | 6 (ui 2, contract 4) | ui | none | e1p4 catalog | seed-phase4 | - |
| `p4-check8-cli-agrees` | admin | 5 (ui 2, contract 3) | ui | none | e1p4 cli | seed-phase4 | - |
| `p4-check4-attach-playbook` | admin | 8 (ui 6, contract 2) | ui | none | e1p4 playbook | seed-phase4 | - |
| `p4-check5-missing-skill` | admin | 6 (ui 3, contract 3) | ui | none | e1p4 catalog | seed-phase4 | p4-check8-cli-agrees |
| `p4-check6-attach-dialog` | admin | 7 (ui 5, contract 2) | ui | none | e1p4 catalog | seed-phase4 | - |
| `p4-check7-edit-agent` | admin, bob | 7 (ui 6, vm 1) | vm | real | e1p4 release | seed-phase4 | - |
| `p4-check9-chat-playbook` | bob | 7 (ui 3, vm 4) | vm | real | e1p4 chat playbook gate smoke | seed-phase4 | p4-check7-edit-agent:release-1.1.0 |
| `p4-check11-workspace` | bob | 1 (ui 1) | ui | none | e1p4 workspace | seed-phase4 | p4-check9-chat-playbook:session-up |
| `p4-check10-release-agent` | admin, bob | 5 (ui 5) | ui | none | e1p4 release members | seed-phase4 | p4-check5-missing-skill |
| `p4-check12-draft-chat` | admin | 4 (ui 2, vm 2) | vm | real | e1p4 chat smoke | seed-phase4 | p4-check4-attach-playbook:undone |
| `p4-check13-unchanged-release` | admin | 5 (ui 5) | ui | none | e1p4 release | seed-phase4 | - |
| `p4-check14-refresh-release` | admin | 3 (ui 3) | ui | none | e1p4 release | seed-phase4 | - |
| `p6-guided` | admin | 18 (ui 8, contract 9, vm 1) | vm | real | e1p6 guided release | seed-phase6 | - |
| `p6-check6-member` | bob | 3 (ui 1, contract 1, vm 1) | vm | real | e1p6 members chat | seed-phase6 | p6-guided |
| `p5-check1-defaults` | admin | 6 (ui 4, contract 2) | ui | none | e1p5 permissions | seed-phase5 | - |
| `p5-check2-open-internet` | admin | 2 (ui 1, vm 1) | vm | real | e1p5 egress chat | seed-phase5 | - |
| `p5-check3-ask` | admin | 2 (ui 1, vm 1) | vm | real | e1p5 approval smoke | seed-phase5 | - |
| `p5-check4-always-allow` | admin | 7 (ui 2, contract 1, vm 4) | vm | real | e1p5 approval permissions | seed-phase5 | - |
| `p5-check5-locked` | admin | 3 (ui 1, vm 2) | vm | real | e1p5 permissions | seed-phase5 | - |
| `p5-check6-release` | admin | 4 (ui 3, contract 1) | ui | none | e1p5 release permissions | seed-phase5 | p5-check4-always-allow:saved-always-allow |
| `p5-check8-bump` | admin | 4 (ui 4) | ui | none | e1p5 release | seed-phase5 | p5-check6-release:release-1.1.0 |
| `p5-check9-explain` | admin | 4 (ui 3, contract 1) | ui | none | e1p5 permissions | seed-phase5 | - |
| `p5-check11-tamper` | admin | 3 (ui 1, vm 2) | vm | real | e1p5 permissions | seed-phase5 | - |
| `p5-check12-migrate` | admin | 5 (ui 4, contract 1) | ui | none | e1p5 permissions | seed-phase5 | - |
| `p5-check13-egress` | admin | 3 (ui 1, contract 1, vm 1) | vm | real | e1p5 egress | seed-phase5 | p5-check9-explain:pastebin-block |
| `p5-check14-gate-in-chat` | admin | 3 (ui 1, vm 2) | vm | real | e1p5 gate chat smoke | seed-phase5 | - |
| `p5-check7-member` | bob | 4 (ui 1, contract 1, vm 2) | vm | real | e1p5 members chat | seed-phase5 | p5-check6-release:release-1.1.0 |
| `p5-check10-legacy` | bob | 3 (ui 1, contract 1, vm 1) | vm | real | e1p5 permissions members | seed-phase5 | p5-check11-tamper:restored |
| `h1-check1-approval-turn5` | admin | 8 (ui 2, contract 4, vm 2) | vm | real | h1 approval chat | seed-phase5 | p5-check5-locked |
| `workspace-isolation (todo)` | - | 0 | vm | real | workspace smoke | - | - |

31 journeys from 34 spec files (the admin and bob halves of three scenarios
share one journey), plus the todo placeholder `workspace-isolation`; 159 checks:
ui 86, contract 46, vm 27. Journey tiers: ui 16, vm 15.

Changes from the preliminary table (2026-10-16, before the classification):
p4-check11-workspace is ui, not vm (it reads the session that check 9 leaves up
and starts nothing); p5-check11-tamper is vm, not contract (a new run must fail
at session start); p6-guided is vm (its check that calls p6_echo starts a run).
The other preliminary "ui + contract" and "ui + vm" rows keep their tier as the
highest tier of their checks, which is what `journeys.yaml` records.

## 9. Testing plan

### 9.1 Phase close (after T3)

1. `pnpm e2e:phase-close <tag>` (warm). It takes the lock, runs the phase's
   journeys + `@smoke` on the mock, writes `report.md` and bundles, prints the
   mac-check comment body.
2. Failures: read `brief.md`; at most two fix cycles (one branch and PR per fix);
   rerun only the failed journeys with `pnpm e2e:run --journeys <ids>`.
3. Post the comment on the phase's check issue; the owner runs the native smoke
   test.

During waves: each PR's "Live before merge" list (`pnpm e2e:run --journeys …` or
`--changed`). Before a release to `main`: `pnpm e2e:run --all` once.

### 9.2 Measured timings

To be filled by W5 (smoke on the mock, first numbers) (one launch),
W11 (cold vs warm) and W20 (full suite cold and warm, one phase close).
Baseline (real model, old harness, 2026-10-15): a 51-check partial run took
6 m 43 s (`20261015-224552`, 22:45:52 → 22:52:35, 5 failures from the sign-in
flake); a 14-check Phase 4 subset took ~4–6 min per run; the H1 close as a whole
took ~1 h over 29 runs.

| Run | Cold | Warm | Notes |
|---|---|---|---|
| `@smoke` (5) | — | — | W5 |
| `@h1` + smoke | — | — | W13 |
| `--all` | — | — | W20 |

### 9.3 What remains after T5

- `agent-quality` journeys (none yet) and when the owner wants them run.
- Contract tier for the VAM app (Tauri command handlers against a fake vamd) if the UI
  journeys keep catching command bugs.
- A second Lima VM to run UI and VM journeys in parallel (one VM today; the lock
  serialises).

## 10. Phases

Each phase lists its issues; every issue is self-contained (files, functions,
commands, acceptance, journeys) and is built locally by a Haiku 5.5 session in
wave order, one issue at a time.

| Wave | Phase | Title | Model / effort | Depends on |
|---|---|---|---|---|
| W0.1 | H1 | Merge; close (in base via); merge | haiku-5-5 / high | — |
| W0.2 | H1 | Targeted Lima run on merged code; close an issue | haiku-5-5 / high | W0.1 |
| W1 | T0 | Code map generator, `codemap.json`, query tool | haiku-5-5 / high | W0.2 |
| W2 | T0 | Inventory, `journeys.yaml`, smoke set, dependency map, §8 catalog | haiku-5-5 / high | W1 |
| W3 | T1 | Scripted `MockProvider` (vam-inference) | haiku-5-5 / **max** | W2 |
| W4 | T1 | vamd flag `VAMD_E2E_MOCK_SCRIPTS`, allowlist gate, Lima forward, docs | haiku-5-5 / high | W3 |
| W5 | T1 | Harness on the mock; five smoke scripts; workspace-isolation journey | haiku-5-5 / high | W4 |
| W6 | T1 | 48 KiB approval journey on the mock | haiku-5-5 / high | W5 |
| W7 | T2 | `seed-base.sh` + `arrange.ts`; smoke and h1 ported | haiku-5-5 / high | W6 |
| W8 | T2 | One app launch and sign-in per user; hidden-webview flake | haiku-5-5 / **max** | W7 |
| W9 | T2 | Phase 4 journeys independent | haiku-5-5 / high | W8 |
| W10 | T2 | Phase 5, 6, H1 journeys independent; their mock scripts | haiku-5-5 / high | W9 |
| W11 | T2 | Warm runs; incremental app and rootfs builds | haiku-5-5 / high | W10 |
| W12 | T3 | "Journeys" section in issues, PRs, playbook, skills | haiku-5-5 / high | W11 |
| W13 | T3 | `run.sh` / `phase-close.sh`; report by journey | haiku-5-5 / high | W12 |
| W14 | T3 | `select.mjs` diff → journeys | haiku-5-5 / high | W13 |
| W15 | T3 | `guard-check.mjs` for PRs | haiku-5-5 / high | W14 |
| W16 | T4 | Failure bundle, app side (IPC buffer, dom, error line) | haiku-5-5 / high | W15 |
| W17 | T4 | Failure bundle, host side (log slice, events, class, suspects, brief) | haiku-5-5 / high | W16 |
| W18 | T5 | Contract checks to vam CI | haiku-5-5 / high | W17 |
| W19 | T5 | Area names; retire phase names, order.txt, phase seeds | haiku-5-5 / high | W18 |
| W20 | T5 | Docs, playbook §5–6, skills; full suite on the mock, timings | haiku-5-5 / high | W19 |

Parallel-safe pairs, if the owner ever splits the work: W1 ∥ W3 (app
tooling vs vam crate); W12 ∥ W13; W16 ∥ W18.

**Phase exits.** T0: `journeys.mjs --check` and `codemap:check` green, catalog
filled. T1: smoke and the 48 KiB journey PASS on the mock with no gateway key.
T2: every journey passes alone and in reverse order; one launch per user; warm
smoke restarts and rebuilds nothing. T3: `e2e:run`, `e2e:phase-close`, `--changed`
and the guard check work; the section is in the templates. T4: a forced failure
yields a correct `brief.md`. T5: no phase-named journeys, contract checks in CI,
docs updated, full suite timed.

## 11. Risks and open questions

- **Mock fidelity.** A script can pass where the real model would not call the
  tool. Accepted: the mock tests mechanics; `agent-quality` covers behaviour on
  demand.
- **Tool names reaching the mock** may be canonical or aliased; W3 confirms
  and documents it.
- **WDIO spec grouping with the tauri service** must keep one app per worker;
  W8 verifies before relying on it. Fallback: a pre-authenticated app config
  per user (the app keeps login state in `$XDG_CONFIG_HOME/vam/config.toml`).
- **Warm DB growth**: unique names accumulate rows across warm runs; `--cold`
  resets. If listings slow down adds a periodic cold run.
- **Open (owner):** whether `phase-playbook §6`'s Haiku-runner / Sonnet-diagnosis
  roles change once `brief.md` exists (default: keep them; the brief is their input).

## 12. Rejected options

- **Keep phase scenarios and add a smoke list only.** Leaves the order coupling,
  the real model and the per-spec sign-in; the cost stays.
- **Record-and-replay of real model traffic.** Brittle to prompt changes (every
  system-prompt edit invalidates recordings) and still opaque; a short hand-written
  script per journey is easier to read and keep.
- **Mock inside the guest.** Would need a rootfs rebuild per script change and
  puts test logic inside the untrusted boundary; the host broker is the right place.
- **Playwright against the web build instead of the Tauri app.** Skips the Tauri
  command layer, where several real bugs were (decision stands).
