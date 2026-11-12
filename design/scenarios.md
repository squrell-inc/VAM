# Scenarios: test-first phases and the E2E harness

Status: **confirmed direction, 2026-10-18** (owner). Replaces
the earlier journeys harness (removed) as epic **E9**. Tracker:
`.cursor/workstreams/e2e-scenarios.md` (written from scratch). The
`phase-playbook.md` changes in §9 land in E9 Phase 1.

Kept out of `agent-experience.md` on purpose: that file plans the product; this
one plans how every phase of every epic proves it works.

---

## 1. What we want

The owner's two goals, in their words:

1. **Improve VAM with a concrete plan.** Every phase says up front how it will
   be tested, and the test says what "done" means.
2. **Never loop.** An agent session must not run for hours, spend millions of
   tokens and end with nothing. Every unit of agent work is judged by a check
   that can fail, and the work stops when the check stops moving.

The answer in one line: **scenarios are the spec**. A phase is planned as the
scenarios it adds or changes. They are written first, they fail, the code makes
them pass, and they keep running after the phase is over.

## 2. What went wrong before (evidence)

From the paused E9 harness (the VAM app `e2e/`, 34 spec files, 159 checks) and the
2026-10-15/16 runs:

| Problem | Cause in the code |
|---|---|
| Tests hang 2–4 minutes, then fail with "no card" | Every wait watches only the screen (`LLM_WAIT_MS = 240_000`, `the VAM app/e2e/lib/chat.ts:9`; checks up to 800 s, `e2e/lib/deadline.ts:17`). A run that failed on the host in second 5 is not noticed |
| The mock answers differently from the script | Scripts match by `userTurn`, a count of user messages (`vam-inference/src/mock_script.rs:396`). The guest adds its own user-role messages: the "task is not marked complete" reprompt (`vam-runtime/src/agent.rs:794`), the approval resume and denied notes (`agent.rs:166`, `agent.rs:1559`). The harness also re-sends prompts up to 3 times (`e2e/lib/p5.ts:336`). Every extra message shifts the count |
| Everything goes through the app | All checks drive Tauri through WebDriver, although every chat turn is `POST /v1/runs` and approvals, gates, events and files have HTTP routes and CLI commands |
| Tests coupled by order | `e2e/scenarios/order.txt`: check 8 before 5, admin's release before bob's pin; one failure poisons the rest |
| Nobody updates old tests | Names say when a test was written (`p4-check7`), not what it protects. E10 Phase 0 rewrote four scenarios and could only typecheck them |
| Fix sessions guess | A failure is a screenshot and "timed out". Sessions re-read both repos and rerun blindly |
| Only the Mac can run it | Every run needs Firecracker in Lima. Cloud sessions and CI have no `/dev/kvm` |

## 3. The core idea: scenarios

A **scenario** is one thing a user can do, written as given / when / then, that
the machine can run.

- **Named by behaviour, never by plan.** `approval-suspends-and-resumes`,
  `member-sees-only-own-workspaces`, `follow-up-continues-after-stop`. A
  scenario id, file, seed, fixture or mock script never contains an epic,
  phase, wave or story number. Plans point at scenarios; scenarios never point
  at plans.
- **Independent.** It sets up what it needs, with names unique to the run, and
  cleans up after itself. Any subset, any order, alone.
- **Owned by its area.** Tags are areas (`chat approval gate playbook release
  permissions workspace members egress web context`), not phases.
- **Kept.** When a later phase changes a behaviour, it edits that scenario. The
  catalog is always the current truth of what VAM does.

### 3.1 Shape (draft)

One file per scenario in `vam/e2e/scenarios/<id>.yaml`:

```yaml
id: approval-suspends-and-resumes
title: A guarded tool call waits for approval, then finishes after Approve
tags: [approval, chat]
needs: process            # process | firecracker | ui
given:                    # named world pieces, see §4
  - org-admin
  - agent-with-guarded-tool
when:
  - chat: "e2e-step:ask Create a record titled e2e"
  - expect: { approval: pending, tool: create_record }
  - approve: latest
then:
  - expect: { run: succeeded }
  - expect: { event: tool.completed, tool: create_record }
guards:                   # code this scenario protects (drives selection)
  - vam:crates/vam-orchestrator/src/approvals/**
  - vam:crates/vam-runtime/src/hitl_checkpoint.rs
mock:                     # inline script; matched by step label, not by count
  - when: { step: ask }
    reply: { toolCall: { name: create_record, args: { title: e2e } } }
  - when: { afterTool: create_record }
    reply: { toolCall: { name: task_complete, args: { summary: done } } }
status: active            # pending | active | retired
```

Every step has a time budget. A wait ends on the expected state **or** on the
run reaching a terminal or error state, whichever comes first.

### 3.2 The mock, matched by step label

The scripted mock engine stays (host side, test-only, D3 of the old plan). What
changes: rules match a **step label** the scenario writes into its own message
(`e2e-step:<label>`), plus tool conditions. Messages the guest adds never carry
a label, so they cannot shift the script. `userTurn` is retired. The mock writes
one event per model call, `mock.rule_matched {rule}` or `mock.unmatched {last
message}`, so a wrong answer shows in the run itself. Scripts that mean "done"
end with `task_complete`.

## 4. Seed data by scenario ("worlds")

Today seeds are per phase (`scripts/dev/seed-phase4.sh`, `phase5/`, …). They
are replaced by small, named **world pieces**, each one a reusable piece of
state a scenario can ask for in `given`:

| Piece | Gives |
|---|---|
| `org-admin`, `org-member` | users, signed-in CLI profiles |
| `agent-plain` | an agent that only chats |
| `agent-with-guarded-tool` | an agent with one tool behind "ask" |
| `agent-with-builtins` | an agent offered `read_file` / `write_file` |
| `playbook-with-gate` | a released playbook with one gate |
| `released-agent-v1` | an agent with one released version |
| `web-broker-mock` | the web broker in mock mode |

Rules:

- A piece is idempotent, creates names unique to the run
  (`<piece>-<scenario>-<runTag>`), and has a cleanup.
- Pieces live in `vam/e2e/world/<piece>.yaml` (or code), named by what they
  are, never by who introduced them.
- **A planner reads `given` to see which state a change touches.** "This phase
  changes how approvals checkpoint" → search `given: agent-with-guarded-tool`
  and `guards:` → the scenarios to re-check.

## 5. Layers, owners, where they run

| Layer | Drives | Owner repo | Runs on | Share (target) |
|---|---|---|---|---|
| Unit / contract | Rust tests, `api_tests.rs`, CLI tests; app `pnpm test` | each repo | every PR | as today |
| **Scenario, process** | `vam` CLI and HTTP → real vamd → guest as a host process → scripted mock | **vam** | **every PR in GitHub CI, any cloud session** | **~80% of E2E** |
| Scenario, UI | WebDriver in Tauri, data arranged through the API, checks the screen only | The VAM app | every the VAM app PR on Linux if the UI spike works; else Mac on demand | ~10%, 10–15 scenarios |
| Scenario, firecracker | same runner, real microVM | vam | Mac + Lima, before a release or on the owner's word | ~10% |
| Real model | E10 benchmark tasks (pass^k) | vam | `--real`, owner's word | separate |

- **vam owns the harness**: runner, scenarios, world pieces, mock scripts, CI job.
- **The VAM app** keeps unit tests and only UI scenarios: a card renders, a button
  sends the right command, a member's screen hides what it should. It never
  re-tests vamd's logic and never sets up data through the UI.
- **The VAM app has no PR CI today** (only `release.yml`). It gets one: `pnpm test`,
  `tsc`, and the UI scenarios when they can run on Linux.
- E10's `scripts/bench` becomes scenarios with a scorer, so there is one
  catalog and one mock format (open question 3).

## 6. Process mode (step 0 spike)

The guest runs as a normal host process instead of inside Firecracker, so the
process layer needs no KVM. Why it looks feasible:

- The agent loop is library code (`vam-runtime/src/lib.rs`, `agent.rs`); only
  the PID 1 startup (`src/bin/vam-runtime.rs`) mounts, brings up loopback and
  halts.
- The transport already falls back to a Unix socket
  (`vam-vsock/src/guest.rs:173`, `/tmp/vam-mock-vsock-<port>`).
- `VAM_WORKSPACE_ROOT` and `VAM_SKILLS_MOUNT` already move guest paths.
- vamd picks its VMM through `VmmFactory`; `MockVmm` already exists for tests.

Open risk: how vamd prepares Drive9 without a block device.

**What process mode cannot prove** (stays `needs: firecracker`): the microVM
boundary, landlock, the egress proxy and its TLS CA, TAP networking, Drive9 as
an ext4 device, snapshots and golden restore, the rootfs contents, boot time.

**Gating.** It runs guest code without a sandbox, so: built only with a cargo
feature, refused by vamd unless a test flag is set, mock model only, never in a
release build.

Spike result: **PASS** (2026-10-18, §13).

## 7. The run contract (what a session gets)

- **One command**: `scripts/e2e/run.sh --scenarios a,b | --tags x | --changed
  | --all` (exact name decided with the runner). One line per scenario, a
  `summary.json`, exit code 0 or 1.
- **Budgets**: a chat step 30 s on the mock, a UI wait 10 s, a whole scenario
  2 min, a full process run 10 min. Over budget = fail with the reason.
- **`brief.md` per failure**: scenario, failed step, expected vs observed, the
  run's last events, the mock trace, the screen as text (UI only; screenshot
  kept for humans), suspect files from the guards and code map, one class.
- **Failure class → fixed next action**:

  | Class | Means | Action |
  |---|---|---|
  | `product` | the behaviour is wrong (events show it) | fix the code |
  | `scenario` | the scenario or mock script is out of date | fix the scenario, say so in the PR |
  | `harness` | selector, runner or world piece broken | fix the harness |
  | `env` | vamd down, disk full, lock held | run `doctor`; change no code |

## 8. Test-first: write the scenario, then the code

The owner's proposal: when a phase is planned, its scenarios are written first,
then the code is built until they pass. **Recommended, with guardrails** so the
tests guide the work instead of fighting it:

1. **Behaviour, not implementation.** Scenarios assert what a user sees through
   the CLI/API/UI and events, never internal names. Otherwise they lock in a
   design before it is built.
2. **Red for the right reason.** A new scenario lands as `pending` and must
   fail at the step the phase adds (the runner checks "fails at step X, class
   `product`"). A pending scenario that passes before the code exists is
   wrong.
3. **Builders do not move the goalposts.** A wave session may not edit a
   scenario's `then` to make it pass. A needed change is a "spec change": one
   line in the PR, approved by the orchestrator (or the owner if it changes
   product behaviour).
4. **Outer loop only.** Scenarios say *what* works end to end; wave sessions
   still write unit tests for *how*.
5. **Small delta.** A phase typically adds or changes 3–8 scenarios. More means
   the phase is too big.

## 9. How phases are planned with scenarios (playbook changes, proposed)

**Breakdown agent** (playbook §3 step 2). Its first output, before items:

```markdown
**Scenario delta**

| Scenario | Change | Layer | Given | Why |
|---|---|---|---|---|
| follow-up-continues-after-stop | new | process | agent-with-builtins | the phase's exit |
| approval-suspends-and-resumes | changed: checkpoint by reference | process | agent-with-guarded-tool | guards hitl_checkpoint.rs |
| draft-chat-replays-history | retired: replaced by turn checkpoints | — | — | — |
```

It finds touched scenarios from `guards`, `given` and the code map, not from
memory. Items then name the scenarios they turn green.

**Wave 0, "scenarios"** (Sonnet low). Writes the new and changed scenarios as
`pending`, plus any new world pieces. Proves each fails for the right reason.
Merges first.

**Build waves.** Each wave row names the pending scenarios it turns green. A
wave is done when those pass and nothing else regressed (CI runs the process
layer on every PR).

**Phase close.** All of the phase's scenarios are `active` and green in CI;
`needs: firecracker` ones run once on the Mac if the phase touched them. The
team checklist is written from the same scenarios.

**Worked example: E10 Phase 2 (Context).** Exit: "the two-message task passes;
files and conversation match after a forced stop between messages." Scenario
delta: `follow-up-continues-after-stop` (new), `follow-up-after-compaction`
(new), `approval-suspends-and-resumes` (changed: checkpoints move by
reference), `workspace-files-survive-restart` (new, `needs: firecracker` for
the copy-on-write image).

## 10. Never loop: stop rules and evaluation

- **Progress must be visible.** A fix cycle counts only if a scenario moves: it
  passes, or it fails at a later step. No movement in two cycles → stop and
  report with the `brief.md`.
- **No rerun without a change.** Rerun only failed scenarios.
- **Caps per wave**: two fix cycles (playbook §2 already says so), a wall-clock
  budget in the wave row, then the orchestrator steps the model up once or
  records a known issue.
- **Mock only** in waves; the real model only for benchmark tasks on the
  owner's word.
- **Scoreboard.** Each progress-log row records: scenarios turned green,
  scenarios red, fix cycles, wall time. A phase whose scoreboard does not move
  for a wave is flagged to the owner.

## 11. Cleanup of the old harness

Once, at the start, one PR per repo, so old and new never live side by side.
The old harness is paused and not running, so nothing is lost. First copy the
159 checks of `the VAM app/e2e/journeys.yaml` into an appendix of this doc as the
coverage backlog (behaviour only).

- **The VAM app, delete**: `e2e/scenarios/` (34 phase specs, `order.txt`),
  `e2e/lib/p5.ts`, `p6.ts` and other phase helpers, `e2e/mock-scripts/`,
  `e2e/journeys.yaml`, `scripts/e2e/mac-run.sh`, `phase-check.sh`,
  `spec-order.sh`, `docs/e2e-mac.md`, `docs/e2e-arrange.md`, the `e2e*`
  scripts in `package.json`, `.claude/skills/mac-check`, `mac-phase-close`.
- **vam, delete**: `scripts/dev/seed-phase4.sh`, `seed-phase5.sh`,
  `seed-phase6.sh` and the `phase4/ phase5/ phase6/ h1/` fixture folders;
  the old tracker and handoff are retired (history only, not kept).
- **Keep**: the VAM app unit tests; the Tauri WebDriver plugin behind
  `--features e2e`; the mock engine in `vam-inference` (matching changes);
  `seed-base.sh` and the code map only if they fit (decided in S1).

## 12. Plan

| Step | What | Done when |
|---|---|---|
| **S0 Spike** | Process mode: vamd + guest as a process + mock, one run with a tool call, in a cloud container | Verdict in §13 |
| **S1 Foundation** | This doc confirmed; E9 tracker rewritten; cleanup (§11); mock matches by step label; playbook §9 changes | Both repos clean; the next phase breakdown uses a scenario delta |
| **S2 Runner + smoke** | Runner, world pieces, 5 smoke scenarios (chat, approval, gate, playbook start, workspace isolation), CI job | Smoke green on every vam PR in < 5 min |
| **S3 Coverage** | Backlog from §11 rebuilt as scenarios; bench folded in | Each old behaviour is a scenario, a UI scenario, or dropped on purpose |
| **S4 UI layer** | 10–15 UI scenarios, Linux CI if possible; the VAM app PR CI | UI run < 10 min; one hang never stops the rest |
| **S5 Firecracker lane + selection** | `needs: firecracker` scenarios on Lima; `--changed` from guards; PR guard check | First phase close done this way |

**Adopt now, before the runner exists:** the next phase breakdown (E10 Phase 2)
writes its scenario delta in plain words. The scenarios become runnable when S2
lands.

## 13. Spike results

**PASS, 2026-10-18** (background agent in a cloud container, no `/dev/kvm`;
re-run by the orchestrator: PASS, 5 s). Real `vamd` took the run over HTTP from
the `vam` CLI, started the real `vam-runtime` as a host child process, and the
scripted mock answered. Two runs reached `RUN_STATE_SUCCEEDED`: `write_file`
then `read_file`, and `run` (a real `sh`). Broker, policy, telemetry,
attestation and the Drive9 commit ran for real. Local branch
`spike/process-guest` (13 files, +597), not pushed.

| Step | Time (debug) |
|---|---|
| Cold build / incremental | 3 m 22 s / 11–36 s |
| vamd healthy | ~0.3 s |
| Session boot | 130–420 ms |
| Run start → terminal | 0.4–0.7 s |
| Terminal → workspace committed | 1.5–1.7 s (shutdown grace; the runner polls) |

Spike changes: guest transport chosen by `VAM_GUEST_VSOCK_UDS`; guest
`VAM_GUEST_MODE=process` skips PID 1 work; tool cwd from `VAM_WORKSPACE_ROOT`;
`ProcessVmm` in vam-core; vamd `VAMD_PROCESS_GUEST_BIN`, refused without
`VAMD_E2E_MOCK_SCRIPTS`.

Blockers for the real version (none needs a kernel feature):

1. **Transport must be explicit.** The old Unix fallback makes the guest dial
   and listen on the same path; a container with `/dev/vsock` would reach its
   own hypervisor host (also why `vam-vsock/tests/loopback.rs:72` fails here).
   Choose by config; remove the auto-fallback.
2. **Drive9 directory backend** in the workspace manager (the spike deleted the
   ext4 image).
3. **Guest init acts on the whole machine**: `kill(-1)`, `reboot`, `umount`,
   `clock_settime`, `/dev/urandom`, `ifconfig lo`. Split init from the agent
   server, or compile it out by feature.
4. **Teardown bypasses `VmmFactory`** (`manager.rs:47`, near `:1933`).
5. **Paths and ownership**: tools drop to uid 1000 (workspace chown); landlock
   paths hardcoded to `/mnt/drive9`.
6. **Fixed egress names** (`127.0.0.1:3128`, `/tmp/vam-egress.sock`): per-guest
   names for parallel scenarios.
7. **Leftovers**: base socket and pid file after teardown.

Side finding: `scripts/bench/run-bench.sh --mock` likely broken
(bench agents have empty `role`, never released; `vam run start` 404s a draft).

Size of the real version: **medium**, about 10–14 files in 5 crates plus
harness, CI and docs (~1–2k lines with tests).

## 14. Open questions (defaults in bold)

1. Runner: **a Rust workspace crate `crates/vam-e2e` typed on `vam-spec`** (a
   proto change breaks the build, not a live run), or TypeScript. A new crate
   needs the owner's word.
2. Scenario format: **YAML data + a small step vocabulary**, or code.
3. Fold `scripts/bench` into the runner: **yes, in S3**.
4. Cleanup before the new scenarios exist: **yes, in S1**.
5. UI scenarios on Linux CI (Tauri WebDriver under a virtual display): **spike in
   S4**; fall back to the Mac.

## 15. Rejected

- **Keep phase-named tests and patch them.** Leaves order coupling, count-based
  mock matching and UI-first cost in place.
- **UI-first E2E.** Slow, flaky, and most checks are not about the screen.
- **Record and replay real model traffic.** Every prompt change breaks it.
- **Tests linked to stories or tickets.** Plans change; behaviour is what stays.

---

## Appendix A. Coverage backlog from the old harness

The old live harness had 32 journeys with **164** checks: the 159 this doc counts plus the 5 of `workspace-isolation`. They are listed here by behaviour and area, with a likely layer, so the Coverage phase can turn them into scenarios. They are a backlog, not scenarios: nothing here has an id, a `given` or a status. Of the 164, **130** are behaviours and **34** are sign-in setup steps, which a scenario gets from `org-admin` and `org-member` and which are therefore counted once per area below, not listed as rows.

Areas are the §3 list plus `catalog` (skills, playbooks and agents shown in the app and CLI; folds in the old CLI-agrees check). Layers: `process` (CLI or API against vamd with the scripted mock), `ui` (the screen only), `firecracker` (needs a real microVM or real network). Rows marked (observation) recorded a value and never passed or failed; keep them as a budget or log, not an assertion.

| Area | Behaviours | Sign-in setup steps | Total |
|---|---|---|---|
| catalog | 22 | 4 | 26 |
| playbook | 13 | 3 | 16 |
| chat | 13 | 2 | 15 |
| approval | 11 | 3 | 14 |
| gate | 5 | 1 | 6 |
| release | 31 | 7 | 38 |
| permissions | 18 | 6 | 24 |
| workspace | 5 | 1 | 6 |
| members | 9 | 5 | 14 |
| egress | 3 | 2 | 5 |
| **All** | **130** | **34** | **164** |

### A.1 catalog (22 behaviours + 4 sign-in steps)

| Behaviour | Layer |
|---|---|
| The playbooks list shows released playbooks with their version | process |
| A draft playbook shows a Draft badge to its author | process |
| Opening a skill shows a plain-language card of what it can do | ui |
| The skill card names the tools it uses | process |
| The skill card names the hosts it can reach | process |
| The skill card names the secrets it uses | process |
| The skill card shows no raw capability strings | process |
| The app flags a playbook whose required skill is not attached | ui |
| The CLI and the app list the same playbooks and skills at the same moment | process |
| The CLI marks a required skill as not attached | process |
| The app flag count of missing skills matches the CLI | process |
| A draft agent opens as a draft | ui |
| A flag says which required skill a playbook needs | process |
| One click on Add clears the missing-skill flag | ui |
| The added skill appears under the agent attached skills | process |
| The CLI sees the added skill on the draft | process |
| A draft agent without skills can be created | process |
| After a reload the app lists the new agent as a draft | ui |
| Attach skill opens the skill picker | ui |
| Choosing a skill shows its capabilities before attaching | ui |
| One click on Attach attaches the skill with no confirm step | ui |
| The CLI sees the attached skill on the draft | process |

### A.2 playbook (13 behaviours + 3 sign-in steps)

| Behaviour | Layer |
|---|---|
| Opening a playbook shows its inspector | ui |
| The inspector shows the playbook mode | process |
| The inspector lists the playbook steps | process |
| The inspector lists the skills the playbook requires | process |
| The inspector lists the capabilities the playbook requires | process |
| The inspector shows the version history | process |
| A draft agent with no playbooks opens as a draft with an empty playbook list | ui |
| A playbook can be set as the agent default | process |
| Another playbook can be allowed on the agent | ui |
| The app stops showing a saving state once the change is stored | ui |
| After a reload the default and the allowed playbook are still set | ui |
| The CLI sees the playbook refs by URN only, with no version or digest | process |
| Removing the playbook refs restores the draft | ui |

### A.3 chat (13 behaviours + 2 sign-in steps)

| Behaviour | Layer |
|---|---|
| Picking a playbook in a new thread shows its input form above the composer | ui |
| An empty required input is flagged and nothing is sent | process |
| A second message in the thread has no form and gets a reply | process |
| A draft agent has its default playbook set | process |
| A draft workspace accepts a chat | process |
| The run resolves the playbook from the admin draft | process |
| Test in chat opens the draft chat with a draft banner | ui |
| A prompt that calls a skill tool runs, and the run is of the draft agent | process |
| The chat journey view shows the tool call and the policy decision | ui |
| A thread with the guarded agent opens and its first turn is answered | process |
| The second turn is answered in the same thread | process |
| The third turn is answered in the same thread | process |
| The fourth turn is answered in the same thread | process |

### A.4 approval (11 behaviours + 3 sign-in steps)

| Behaviour | Layer |
|---|---|
| Read-only and local mutable tools run, and a mutable external tool suspends for approval | process |
| The approval card offers Allow once, Always allow and Always deny, and names the target | process |
| Always allow continues the run and the card says what it saved | process |
| A second call in the same thread does not prompt | process |
| A new draft run does not prompt | process |
| Legacy approval cards still appear on an agent in permissions mode (observation) | process |
| A locked tool card offers Allow once and Deny only, and names the lock | process |
| Allow once runs the tool and saves nothing | process |
| The fifth turn suspends on a guarded tool and the in-chat card offers Allow once | process |
| Allow once runs the tool, the run succeeds and the reply shows, with no continuation error | process |
| The stored checkpoint size after the fifth turn is recorded (observation) | process |

### A.5 gate (5 behaviours + 1 sign-in steps)

| Behaviour | Layer |
|---|---|
| Starting a playbook with its input pauses at the gate, shown as a card in the chat | process |
| The playbook runs panel shows the same gate | ui |
| Deciding the gate in the chat finishes the run | process |
| A playbook run paused at its gate shows the gate as a card in the chat thread | process |
| Deciding the gate in the chat finishes the run and the panel shows it decided | process |

### A.6 release (31 behaviours + 7 sign-in steps)

| Behaviour | Layer |
|---|---|
| A custom agent shows no Platform badge and offers Edit | ui |
| Editing the instructions saves them on the draft | ui |
| Releasing a new version from the dialog publishes the edited instructions | ui |
| A run pinned to the new version completes its playbook | process |
| An attached skill carries over to the release | ui |
| The suggested release publishes the agent | ui |
| A throwaway draft agent can be created | process |
| The first release of an agent is 1.0.0 | process |
| A higher version with no change is refused | process |
| After one edit the release succeeds | process |
| The app shows an agent released version | ui |
| A release made from the CLI shows in the app after Refresh without signing in again | ui |
| New agent from the dialog lands on the guided Instructions step | ui |
| The new draft appears in the admin agent list | process |
| Instructions entered in the guide are saved on the draft | process |
| A skill attaches after its consent text is shown | ui |
| An unreleased playbook is allowed and an unreleased skill is flagged and added in one click | ui |
| The CLI sees the draft refs by URN only, with no version or digest | process |
| Back to steps returns to the Test step in the same session | ui |
| The release step lists the skill before the playbook | ui |
| An agent-only release is refused with a conflict naming the unreleased skill and playbook | ui |
| The skill and the playbook stay unreleased until the combined release | process |
| Release together releases the skill, the playbook and the agent | ui |
| The versions list shows each of the three released | process |
| The agent released refs are frozen to the versions just released | process |
| The release dialog lists the permission change and suggests a minor bump | ui |
| The release dialog explains the permission change in plain words | ui |
| Releasing at the suggested bump writes no new policy version | process |
| Setting a permission to ask suggests a minor bump, in the dialog and the CLI | ui |
| Setting a permission to deny suggests a major bump, in the dialog and the CLI | ui |
| Setting a permission back to allow shows no permission change | ui |

### A.7 permissions (18 behaviours + 6 sign-in steps)

| Behaviour | Layer |
|---|---|
| The guide permissions card shows the defaults | process |
| A new agent with no permissions edited gets the defaults | ui |
| The permissions page shows side-effect decisions and any site | ui |
| The effective permissions in the CLI agree with the page | process |
| A seeded agent shows side-effect decisions for its skill tools | ui |
| The effective permissions in the CLI agree for the seeded agent | process |
| The saved rule shows on the permissions page and nowhere else in the inspector | ui |
| Explain prints the decision and marks the row that decided it | process |
| The app effective view shows the same locked row and reason | ui |
| A host block added on the page is named by explain for that host | ui |
| A new run fails at session start, naming URN, version and digest, when the pinned policy was changed | process |
| After the policy row is restored a new run starts | process |
| A legacy draft shows a conversion banner and a read-only permissions page | ui |
| Convert lists what was converted and what was kept as advanced policy | ui |
| The permissions page is editable after conversion | ui |
| The CLI migrate command converts a legacy draft | process |
| The effective view of a legacy release matches its original policy | process |
| In a legacy-release run reads and local writes go, a monitored tool suspends, and a secret reaches no host | process |

### A.8 workspace (5 behaviours + 1 sign-in steps)

| Behaviour | Layer |
|---|---|
| The workspace is committed after a chat reply | process |
| Two throwaway agents can be created with the mock script | process |
| An agent saves a note in its own workspace | process |
| A second agent cannot read the first agent note from its own workspace | process |
| Each thread shows only its own agent messages | ui |

### A.9 members (9 behaviours + 5 sign-in steps)

| Behaviour | Layer |
|---|---|
| A member sees the released playbooks | process |
| A member does not see a draft playbook | process |
| A member sees the agent read-only with the new instructions | ui |
| A member sees playbooks and requirements read-only | ui |
| A member sees an agent at its released version through the CLI | process |
| A chat on the released agent is accepted and replies | process |
| A member run of the released version runs an allowed tool without a prompt | process |
| A different tool offers the member Allow once only | process |
| The admin draft is unchanged after the member run | process |

### A.10 egress (3 behaviours + 2 sign-in steps)

| Behaviour | Layer |
|---|---|
| A draft chat fetches a public website under the open-internet default | firecracker |
| A secret bound to a host can be created | process |
| The egress floor, a block rule and the open internet decide as the permissions say | firecracker |
