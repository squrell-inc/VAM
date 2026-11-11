# Archive: agent-experience (E1), done phases

Moved out of [../agent-experience.md](../agent-experience.md) on 2026-10-16 so
the tracker shows only the open phase. Locked decisions stay in the tracker.

## Phases

### Phase 0 — Capture

**Goal:** A design doc and tracker, so later plans don't have to
reconstruct this from chat.
**Depends on:** none
**Plan:** this capture
**Exit:** design doc, tracker, both README rows, and amended locked
decisions in `developer-experience` (DX-46) and `isolation-verify-closeout`
(decision 6).

- [x] `design/agent-experience.md`
- [x] Tracker and README rows
- [x] Amend DX-46 and isolation-verify-closeout decision 6 to point here

### Phase 1 — Release works more than once ✅ done 2026-10-10

**Goal:** Fix both release bugs. The draft stops living at a fixed version
slot.
**Depends on:** Phase 0
**Plan:** Milestone P1. Issues:
- Wave 1: AX-1.1 (vam), AX-1.3 (the VAM app)
- Wave 2: AX-1.2 (vam), AX-1.4 (the VAM app), AX-1.6 (vam, draft owner check)
- Wave 3: AX-1.7 (vam, version on agent/policy records)
- Wave 4: AX-1.8 (the VAM app, agent/policy release with a chosen version), AX-1.9 (vam, workspace-only run uses the workspace pin; found by, AX-1.5 part A)
- Verify: AX-1.5 on a KVM host. Part A (upgrade, CLI release, runs, privacy) now; part B (app agent/policy release) after AX-1.8
**Repos:** vam (orchestrator, CLI), the VAM app
**Exit:** an agent, playbook, policy and skill package can each be released
as `0.1.0`, edited, then released as `0.1.1` and `1.0.0`. Members see all
three versions. The owner still has one draft throughout.

- [x] The draft is keyed by artifact, not by version. Replace the fixed
      `1.0.0-draft` slot (`catalog_publish.rs` `upsert_spec_draft`,
      `catalog.rs:32-34`) for agents, playbooks and policies. Make the skill
      package draft independent of the manifest version too.
- [x] `release(urn, version)` snapshots the current draft at `version`. It
      enforces "greater than the latest release" and keeps the no-op for the
      same digest. A conflict message names the next free version.
- [x] Migrate existing `X.Y.Z-draft` rows to become the artifact's draft,
      without data loss.
- [x] Workspace and run pins: "draft" resolves to the owner's draft,
      whatever its old slot. Keep isolation-verify-closeout Phase 3
      behaviour.
- [x] the VAM app: skill release strips the prerelease. Agent release sends a
      chosen version instead of the default `1.0.0`.
- [x] Tests: release twice, release lower (rejected), the same-digest no-op,
      a foreign draft stays invisible, and the migration.

### Phase 2 — Versions are visible ✅ done 2026-10-11

**Goal:** People can see and choose versions.
**Depends on:** Phase 1
**Plan:** Milestone P2. Issues:
- Wave 1: AX-2.1 (vam, release note + releaser + versions API), AX-2.2 (vam, suggest next version + CLI), AX-2.4 (the VAM app, workspace picker shows real versions)
- Wave 2: AX-2.3 (the VAM app, release dialog; closes an issue), AX-2.5 (the VAM app, version history panel)
- Verify: AX-2.6 on a KVM host
**Repos:** the VAM app, vam (CLI)
**Exit:** the catalog lists, inspector and workspace pin all show versions.
The release dialog suggests a bump and takes a note.

- [ ] A version list per artifact, with the release note and who released
      it.
- [ ] A release dialog with the suggested bump (patch, minor or major per
      design §II.4) and a required note.
- [ ] `vam agent|skill|playbook|policy release` with no `--version` suggests
      the next version. It requires a flag when stdin is not a TTY.
- [ ] Workspace pin: Draft or any released version.

### Phase 3 — The server owns pins ✅ done 2026-10-12

**Goal:** Digests disappear from authoring.
**Depends on:** Phase 1
**Plan:** Milestone P3. Issues:
- Wave 1: AX-3.1 (vam-spec, authored refs may float), AX-3.2 (orchestrator, one resolver; floating ref uses the agent owner's draft on a draft run, else latest release)
- Wave 2: AX-3.3 (release freezes refs, fails on draft-only deps, frozen-to-frozen "unchanged"), AX-3.4 (the VAM app stops stamping), AX-3.5 (examples and docs)
- Verify: AX-3.6 on a KVM host
- Out of scope: runtime `extends` merge (not wired today); policy pin enforcement at session start (DX-6A, moves to the policy-stack design).
**Repos:** vam, the VAM app
**Exit:** an agent JSON with no version or digest on its references
publishes, runs as a draft, and releases with every reference frozen. A
reference to a dependency that only exists as a draft fails release with a
message that fixes it.

- [x] Draft references may omit version and digest. They resolve to the
      latest release, or to the owner's own draft of a dependency.
- [x] Release resolves and freezes every reference (skills, playbooks,
      policies, `extends`) to `version + digest`.
- [x] Release fails on a dependency that only exists as a draft, naming the
      artifact and the fix.
- [x] the VAM app stops writing `EMPTY_SHA256_HEX` and draft-prerelease stamps
      into references.
- [x] Examples and `docs/authoring/*` drop digests and versions written by
      hand.

### Phase 4 — Compose in the app ✅ done 2026-10-12

**Goal:** Skills and playbooks attach by name, with their requirements
visible.
**Depends on:** Phase 3
**Plan:** Milestone P4. Build: AX-4.1 (composition check API + `vam agent check`), AX-4.2 (playbook list and inspector), AX-4.3 (skill capability text), AX-4.4 (attach playbooks, missing-skill flag) (edit instructions). Fix waves from KVM-host runs, plus. Verify: AX-4.5, automated by the E9 harness (`phase-check.sh AX-4.5`), which also closed the Phase 3 check AX-3.6.
**Repos:** the VAM app (plus a `vamd` list endpoint if one is missing)
**Exit:** a user attaches a playbook to an agent in the app. A missing
required skill is flagged and added in one click.

- [x] A playbook catalog list, inspector, and attach/detach on the agent.
- [x] Attaching a skill shows its declared capabilities as consent text.
- [x] Playbook `requires` check (DX-85) with a one-click fix.

### Phase 5 — Permissions as data ✅ done 2026-10-14

**Goal:** Nobody writes Cedar to set what an agent may do. A common rule
("allow all tools") and a specific one ("ask before `jira_create_issue`")
combine with no third policy, and nothing but the agent needs a release.
**Depends on:** Phases 3, 4
**Design:** `design/agent-experience.md` Part IV (§IV.12 has the items).
**Plan:** Milestone P5 (renamed "P5 Permissions as data"), 20 issues
planned 2026-10-13. Waves (parallel issues inside a wave never
edit the same files; `runs=` per issue in its tag line):
- Any time from wave 0 (the VAM app, no server dependency): AX-5.5c (playbook gates decided in the chat as a card, like tool approvals; mac). Product owner rule: every run decision is made on a card in the chat thread, never in another section.
- Wave 0: AX-5.0 (all Phase 5 proto fields, literal fixes, empty orchestrator modules and 501 route stubs; cloud), AX-5.3a (legacy regression tests, before any session-assembly change; cloud)
- Wave 1: AX-5.1 (vam-spec grammar, validation; cloud), AX-5.2 (vam-security: platform policy set, `Tool` attributes, entities per request, `evaluate_egress`, `classify_tools` scope + rule; cloud)
- Wave 2: AX-5.2b (resolver; cloud), AX-5.3c (legacy path, DX-6A pin + digest, run's pinned agent; mac), AX-5.4 (approvalRequired, MCP ask rules; cursor), AX-5.7a (release diff + bump; cursor)
- Wave 3: AX-5.3b (session assembly; guest tool name → target kept per session and on approvals; mac), AX-5.6a (server defaults on publish, draft permissions GET/PUT; cloud)
- Wave 4: AX-5.3d (egress Cedar; mac; **needs E8 Phase 1 merged first**), AX-5.5a (Always allow/deny API; mac), AX-5.6b (migrate legacy draft; cloud), AX-5.8a (effective, explain, presets API; cloud)
- Wave 5: AX-5.8b (CLI; cursor), AX-5.0b (the VAM app types + Tauri commands; mac)
- Wave 6 (the VAM app): AX-5.5b (Always allow/deny on the in-chat approval card; also blocked by AX-5.5c), AX-5.6c (permissions page), AX-5.7b (release dialog, no auto-released policies); all mac
- Verify: AX-5.9 Mac phase session; adds `seed-phase5.sh` and `p5-*` scenarios to the E9 harness
**Repos:** vam (`vam-spec`, `vam-security`, orchestrator, `vam-network`
call site, CLI), the VAM app
**Exit:** a new agent with no secrets reaches the public internet and asks
before a mutable external tool. The owner clicks "Always allow" on a draft
run, releases, and a member's run of that version allows the tool. `vam
policy explain` names the deciding rule. A legacy released agent behaves
exactly as before, and a tampered pinned policy fails closed.

Getting-started slice (all of these):

- [x] 5.1 `AgentPermissions` in `vam-spec` (proto and JSON), the target
      grammar, and one resolver in `vam-security`: specificity per scope,
      `bySideEffect`. *(AX-5.0 W0, AX-5.1 W1, AX-5.2b
      AX-5.2b W2)*
- [x] 5.2 Fixed platform policy set, schema `Tool` attributes (always set),
      entities per request; `classify_tools` returns the deciding scope and
      rule. *(AX-5.2 W1)*
- [x] 5.3 Session assembly from `permissions`. Compatibility path for
      legacy agents with pin and digest enforcement (DX-6A). Egress broker
      evaluates Cedar `EgressRequest` in place of the secret-union
      allowlist. *(AX-5.3a W0 tests first, AX-5.3c W2,
      AX-5.3b W3, AX-5.3d W4)*
- [x] 5.4 `ToolSpec.approvalRequired` as a source rule;
      `SkillManifest.policy` deprecated. *(AX-5.4 W2)*
- [x] 5.5 "Always allow / Always deny" on a draft run writes the agent
      rule: API, and the approval prompt in the VAM app. *(AX-5.5a
      W4, AX-5.5b W6; gates in the chat AX-5.5c)*
- [x] 5.6 the VAM app permissions page: toggles, presets, effective view,
      migration banner. *(AX-5.6a W3, AX-5.6b W4,
      AX-5.0b W5, AX-5.6c W6)*
- [x] 5.7 Release diff for permissions and the bump rule; the VAM app stops
      auto-releasing policies. *(AX-5.7a W2, AX-5.7b W6)*
- [x] 5.8 `vam policy explain` (tool and `--host`) and `vam agent
      permissions`. *(AX-5.8a W4, AX-5.8b W5)*
- [x] 5.9 Verify on a KVM host. *(AX-5.9 harness green 2026-10-14, 51 pass / 0 fail / 2 info; Phase 4 regression 76 pass; manual smoke test on the release build passed)*

Later (not in the slice): read-only advanced Cedar view, org presets as
artifacts, "ask" for hosts, run narrowing, `vam policy test` (DX-66), Cedar
templates (DX-67), qualified Cedar resource ids (DX-68).

### Phase 6 — Guided create, test, release ✅ done 2026-10-14

**Goal:** The journey in the app from design §II.7 works end to end.
**Depends on:** Phases 2, 4, 5
**Plan:** Milestone P6, 10 issues, planned 2026-10-14. Waves (parallel issues inside a wave never edit the same files; `runs=` per issue in its tag line; open questions for the product owner on AX-6.0):
- Wave 0: AX-6.0 (the VAM app scaffold: types, Tauri release/suggestion commands for playbook, skill, policy, guided-mode wiring, test ids; mac), AX-6.1 (vam: release suggestion lists draft-only dependencies in release order; cursor), AX-6.2 (vam: `seed-phase6.sh` fixtures; mac)
- Wave 1 (the VAM app): AX-6.3 (guided steps on the inspector; mac), AX-6.4 (step 1 dialog, one place for the new-agent URN, collision check; cursor), AX-6.5 (test step: draft chat in a scratch workspace, existing journey sheet; mac), AX-6.6 (release-together plan logic; cursor)
- Wave 2: AX-6.7 (release dialog releases draft-only dependencies together; integration point; mac), AX-6.8 (`p6-*` harness scenarios; mac)
- Verify: AX-6.9 phase close, one session (`mac-phase-close`): the P6, P5 and P4 seeds once each, targeted fixes capped at 3 reruns per scenario, one final pass if a fix landed, then the owner's native smoke test.
**Repos:** the VAM app (plus vam: AX-6.1 server field and, AX-6.2 harness seed)
**Exit:** a new user goes from New agent to a released org agent without
editing JSON, and tests the draft in chat first.

- [x] A step-by-step flow: name, instructions, skills, playbooks,
      permissions, test, release.
- [x] Test runs the draft in a scratch workspace with the journey timeline.
- [x] Release offers to release draft-only dependencies together.

**Closed 2026-10-14** by the phase close (result comment fix PR). Open, not blocking: Phase 4 check 7; Phase 6 checks 3 and 6 assert the agent URN only until the run status exposes the pinned version.


## Progress log (done phases)

| Date | Phase | What landed |
|---|---|---|
| 2026-10-14 | 6 | **Phase 6 closed (PASS, with open items).** Phase close, AX-6.9 on the Mac: final pass Phase 6 21/0/0 (`20261014-175336`), Phase 5 51/0/2 (`175422`), Phase 4 75/1/1 (`175943`). Fix PR (harness only: checks 1 to 5 share one app session; release steps fill the note first; run checks assert the agent URN; Phase 5 check 1 exits guided mode) merged as after local checks: typecheck, e2e typecheck, spec order, 589 unit tests. Result comment on AX-6.9 . Open, not blocking per the backlog: Phase 4 check 7 (failed 4 of 5 runs today); Phase 6 checks 3 and 6 assert the agent URN only. Mac lock released. Owner: native smoke test and guided walk. |
| 2026-10-14 | 6 | **Waves 1 and 2 merged; phase close started.** Wave 1: (Cursor) (Cursor). Wave 2: (`p6-*` harness scenarios) (release-together). Docs: (`mac-phase-close` skill) and (phase close runs in the main checkouts). The phase close took the Mac lock at 17:24 for a background Haiku 5.5 session; its result goes to AX-6.9, and the tracker is updated after it. |
| 2026-10-14 | 6 | **Wave 0 merged; wave 1 started.** Merged: (AX-6.0 scaffold) and (AX-6.2 seed fixtures). AX-6.1 (AX-6.1 release-order field) is with Cursor. Wave 1 started: AX-6.3 on a Mac worker in a worktree; AX-6.4 and, AX-6.6 with Cursor; AX-6.5 follows, AX-6.3 on the Mac. Merge, CI and cleanup rules are in the working agreement's Orchestration section. |
| 2026-10-14 | 6 | **Phase 6 approved; wave 0 started.** The owner approved the plan, the defaults on AX-6.0, the plan as a docs PR, and the orchestration policy in the working agreement: Cursor delegation per wave with event monitoring; one Mac worker at a time, each in its own worktree; merge and cleanup per PR; the phase close as one session (`mac-phase-close`, to be written) with Sonnet 5.5 at high as the escalation after two failed fix cycles. Started: AX-6.1 delegated to Cursor; AX-6.0 on a Mac worker in its own worktree. Wave 1 starts after, AX-6.0 merges. |
| 2026-10-14 | 6 | **Phase 6 planned.** Milestone P6, 10 issues in waves 0–2 plus the Mac check (see the Phase 6 Plan line). Findings from the code: (1) every editor already exists as a card on `AgentInspector`, so the step-by-step flow is a client-only guided mode with no data-model change; (2) the existing `JourneySheet`/`JourneyTimeline` already shows tool and `policy.decided` rows for a draft run (no deciding rule, no egress decisions); (3) the app shows the draft-only 409 as returned but cannot release a playbook or plain skill at all, and cannot tell which dependencies are draft-only (agent-check resolves like a draft run), so vam gets a structured `unreleasedDependencies` field on the release suggestion (AX-6.1); (4) the harness seed lives in vam, so Phase 6 is not the VAM app only. Domain: P6 keeps `local.vam.dev/default` (moved to one constant, none added) and Phase 7 replaces it. Ten open questions for the product owner on AX-6.0 (instructions in the Create dialog, guided-mode entry, name collisions, gating, scratch workspace, docked journey, release-together form, control names, legacy policies card, deciding rule in the timeline). |
| 2026-10-14 | 5 | **Phase 5 closed and on `main`.** (Phases 1–5 + E8 Phase 1, merge commit) and (app work + MCP OAuth fixes, merge commit; no release is published because the branch disables the push-to-`main` trigger in `release.yml`) merged; (loopback prefix fix) merged first. The team smoke-tests the release app themselves, so no further local smoke test is tracked. Open: an issue and an issue (follow-ups, still in milestone P5 so it shows 91%). Next phase: E8 Phase 2 or E1 Phase 6, decided in a separate session; branch from `main`. |
| 2026-10-14 | 5 | **Release path to main prepared.** vam `main` merged into `feat/agent-experience` (no file changes; the only difference was the merge commit) and opened to `main` (Phases 1 to 5 + E8 Phase 1, 138 files; CI green; held for the product owner). The VAM app: the team's `feat/oauth-bugs` (to an issue MCP OAuth fixes) merged into `feature/agent-experience` as a merge commit after a Mac regression on the combined tree: `pnpm test` 543, `phase-check.sh AX-5.9` 51 PASS / 0 FAIL / 2 INFO, `AX-4.5` 75/1/1 (the one FAIL is bob's check 7, the an issue flake, green on rerun). One unexplained red first run of AX-5.9 (five of bob's sign-ins stalled, hidden webview), not reproduced. Review found `isValidMcpHttpsUrl` and `parseOAuthCallbackUrl` accepted `http://localhost.attacker.example` as loopback (prefix test); fix in the VAM app (new tests fail on the old code), held for review. Release note: the team's branch comments out the push-to-`main` trigger in `release.yml` (manual `workflow_dispatch` only) and adds a CIMD upload+verify step, so merging the VAM app to `main` no longer publishes by itself; a release needs a version bump (still 0.10.1, tag v0.10.1 exists). Also noted: `oauth_http_request` checks the scheme only on the first URL, has no host allowlist and the app has `csp: null` (code reading only); the stale release CLI at `$VAM_REPO/target/release/vam` fails the Phase 5 seed (rebuild it or use `VAM_BIN`). |
| 2026-10-14 | 5 | **Phase 5 closed.** Manual native smoke test on the real release `.app` (unsigned local build against the Phase 5 vamd) passed: permissions page, release dialog Permission changes, approval card with Allow once / Always allow / Always deny (saved to the draft, run continued), locked variant naming the skill, playbook gate card decided in the chat (run Succeeded), folder picker, external link, quit and relaunch keeps the login, config file mode 0600; updater prompt skipped (feed 0.10.1 = this build). Findings (in an issue): the shipped macOS app is unsigned and not notarized (no signing identity in `tauri.conf.json`, no Apple secrets in `release.yml`), so a downloaded copy is blocked by Gatekeeper and the "signed and notarized" smoke item does not apply yet; the updater feed has no `darwin-x86_64` entry (Intel Macs get no update); the release dialog shows literal backticks around blocked domains; after a gate resolved in the chat the reply mentioned `tool_error: invalid contract` for earlier steps (not investigated, may be the model describing the first open attempt). Not verified live: port 55 from a guest, an MCP tool source, org rules. |
| 2026-10-14 | 5 | **Phase 5 build and Mac check done; manual native smoke test open.** AX-5.5b Always allow/deny on the approval card (503 app tests; its journeys were seen only through the harness, not by the build session). AX-5.9 (`phase-check.sh AX-5.9`): 51 PASS, 0 FAIL, 2 INFO across checks 1 to 14; check 15 Phase 4 regression 76 PASS, 0 FAIL, 1 INFO (checks 7 and 9 did not flake); check 16 not run (needs the signed `.app` and a person). Two real bugs found and fixed by the check: effective permissions and `vam agent migrate` ignored skills published as packages; `vam policy explain <agent> --host <site>` collided with the global `--host` (limit: with that form the vamd URL must come from config or `VAM_HOST`). Merged: (seed) (scenarios). Not verified live: port 55 from a guest (the in-guest proxy returns 501 for CONNECT; covered by unit tests and port 54), an MCP tool source (no fixture MCP server; J2 used a mutable external skill tool), org rules. Observations (INFO, to triage): release dialog says "was: bySideEffect" where §IV.8 says "was: ask"; the inspector of a permissions-mode agent still shows the legacy Capability Policies card; `/tmp/vam-drive9/templates` and `sessions` are not reclaimed after sessions end and filled Lima's disk; the `early eof` dispatch log line lines up with scenarios terminating sessions. Exit criteria met by the harness: new agent reaches the internet and asks before a mutable external tool (1 to 3), Always allow then release then member run (4, 6, 7), explain names the deciding rule (9), legacy unchanged and tamper fails closed (10, 11). |
| 2026-10-14 | 5 | **App work merged except the Always-allow card** (AX-5.0b types and commands, AX-5.6c permissions page, AX-5.7b release dialog shows permission changes and stops auto-releasing policies, AX-5.5c playbook gates decided in the chat). Checks: the VAM app has no CI, so each PR's `pnpm test` and `tsc` were rerun by the orchestrator (496 tests on the branch tip). Not verified live: the permissions page UI was never driven in the native app (HTTP contract only); AX-5.7b's J3/J4 only through the release harness checks. Phase 4 checks 7 and 9 flake on the base too (the `p4-edit-agent` fixture advertises only playbook tools, so the model's extract-invoice step sometimes ends the run FAILED). Open: AX-5.5b (Always allow/deny card, card issued), then AX-5.9 (Phase 5 Mac check; should drive the permissions page, release dialog, gate card and port 55 egress using the `permissions-card`, `perm-row-<target>`, `release-permission-changes`, `chat-gate-card` test ids). |
| 2026-10-14 | 5 | **All Phase 5 server work merged** (AX-5.6b migrate a legacy draft, AX-5.3d egress broker evaluates Cedar AX-5.8b CLI `vam policy explain` / `vam agent permissions` / `vam agent migrate`); AX-5.0b types and Tauri commands. Steps 5.3 and 5.8 done; 5.5, 5.6, 5.7 server halves done. Egress (Lima-checked from a guest on port 54): new agent with no secrets reaches example.com; `*.pastebin.com` denied with the rule named, apex reachable by design; IP floor refuses 169.254.169.254 and 10.0.2.2 even with `internet:* allow`; secret injected only for its own destination; legacy agents unchanged (no secrets reach nothing, with a secret exactly its destination). Port 55 (TLS proxy) shares `BrokerGate` but was not run from a guest (the in-guest proxy returns 501 for CONNECT); cover it in AX-5.9. Findings: migration turns a monitored tool that the legacy Cedar never permitted into `ask` (small loosening, owner-reviewed draft), file as follow-up. Open: app screens AX-5.5c (gates in chat), 383 (permissions page), 384 (release dialog), then 382 (Always allow card), then AX-5.9 (Phase 5 Mac check with harness scenarios). |
| 2026-10-14 | 5 | **Wave 4 partly merged; E8 Phase 1 now in this branch.** AX-5.8a (effective/explain/presets API; Read-only preset also allows `builtin:load_skill`) AX-5.5a (Always allow/deny writes the agent rule; Lima-checked: MCP allow, approvalRequired 409, released 409, deny). E8: an issue Mac check passed 12/12 after boot fixes (closes 325), then `feat/egress-and-secrets` merged into `feat/agent-experience` as a merge commit (2 conflicts; provider rule kept once via `resolve_model_provider`). The Mac regression run on the merged tree found one real bug: golden-restored sessions were fenced to lease generation 2 while the session row and run dispatch used 1, so every first task was rejected as stale; fixed (session stamped to generation 2, dispatch leases at the session generation). Phase 4 harness on the merged tree: 75 PASS, 0 FAIL, 1 INFO (`e2e-results/20261014-001159`). Known limit: one golden-restored session at a time (shared CID 3), others cold-boot. Review finds worth remembering: compiled egress Cedar must keep most-specific-wins (forbid beats permit); the mock-VMM check uses PID 12345 (latent, follow-up in E8). Open: AX-5.6b (draft, one failing test) (Mac card issued). |
| 2026-10-13 | 5 | **Wave 3 merged**: AX-5.6a draft permissions defaults on publish (`bySideEffect` / `allow`) and owner GET/PUT API; AX-5.3b session assembly from permissions, `session_tool_targets` per session, target stamped on approvals (Lima-checked: J1, J2, J3, deleted connection, legacy). Found on Lima: stored skill manifests have `resourceId.urn`, not `name`, so the skill name falls back to the URN's last segment. Found at merge: the legacy test fixture became Permissions mode once publish defaults landed (fixture now carries a `capability_policy`). Hosted (HSP) tools get `builtin:<tool>` targets (no host-tool target form yet). Next: Wave 4 (egress, needs E8 Phase 1; AX-5.5a, 378, 379). |
| 2026-10-13 | 5 | **Waves 0–2 merged** (–): AX-5.0 proto fields and 501 route stubs legacy regression tests target grammar and validation platform policy set + `Tool` attributes + `classify_tools` scope/rule resolver legacy path with pin + digest (Lima-checked, tamper fails closed) `approvalRequired` and MCP ask rules release permission diff and bump rule. Steps 5.1, 5.2, 5.4 done; 5.3 and 5.7 partly. Review finds fixed before merge: the resolver's compiled egress Cedar disagreed with `explain_host` (Cedar forbid beats permit, so `*.com` deny beat a more specific `*.pastebin.com` allow); `EgressRule` exceptions added. Open for later: a legacy org MCP connection with no recorded creator is claimed by the next connector (ask rules only tighten). Next: Wave 3 (mac). |
| 2026-10-13 | 5 | Product owner: all run decisions are made on cards in the chat thread. Tool approvals already are (`ActionCard` in `ChatArea.tsx`), so "Always allow / deny" goes on that card (retitled and scoped). Playbook gates were only decidable in the Playbooks sidebar panel; added AX-5.5c (gate card in the chat, reusing `GateForm`; small, app-only, can start now). AX-5.5b and the Mac check AX-5.9 (new check 14) are blocked by it. |
| 2026-10-13 | 5 | **Phase 5 planned.** Milestone P5 renamed "P5 Permissions as data"; 20 issues in waves 0–6 plus the Mac check (see the Phase 5 Plan line), each tagged `runs=` and model. Planning findings from the code: (1) approvals store only the guest `tool_name`, and MCP guest names are lossy, so the tool → target mapping is kept per session and stamped on the approval at suspend time (AX-5.3b) for "Always allow" to write the right target; (2) session policy loads the agent's latest release (else the owner's draft), not the run's pinned version, fixed in AX-5.3c; (3) proto field additions break struct literals across crates, so all proto lands first (AX-5.0); (4) E8 Phase 1 rewrote the egress broker (`BrokerGate`), so, AX-5.3d waits for E8 Phase 1 to be merged into this branch. Planning defaults to confirm with the product owner (not locked): vamd fills `bySideEffect`/`allow` on publishing a draft that has no permissions and no legacy policy (AX-5.6a); migrating a legacy draft keeps its internet reach (deny plus the old secret destinations) rather than opening it (AX-5.6b). |
| 2026-10-13 | 5 | §IV.14 open questions answered; locked decisions 20–27 added (tightening to deny is major, allow → ask minor; no member-remembered approvals; MCP targets origin plus path; collision check enough; no host ask; Squrell-only presets; connection owner edits source rules; draft "Always" rules shown on the permissions page only). Phase 5 journeys walked through with the product owner. Plan not yet drafted. |
| 2026-10-12 | 3, 4 | **Phases 3 and 4 closed.** AX-4.5 run 5 on the Mac (E9 harness, 14 checks incl. the Phase 3 re-checks): 75 PASS, 0 FAIL, check 11 INFO. Fix waves after runs 1–4: examples.local reserved and examples moved to local.vam.dev, skills under local.vam.dev, procedure names each step's action, guest prompt only describes advertised tools, decided gates in the finish nudge. Model routing honours the declared provider; E2E chat uses anthropic/claude-haiku-5.5 via the Vercel AI Gateway. Backlog from the run: an issue (run fails on an incomplete finish after a gate) (runs panel polling, workspace pile-up) (run record names the playbook) (Draft badge only on unreleased changes). Process change: Mac-first execution (working agreement). |
| 2026-10-11 | 3 | AX-3.6 Mac check: 5 PASS, 2 FAIL. Filed an issue (duplicate release at a higher version; conflicts with §II.4, needs a decision) (app chat rejected with an allowed playbook) (app needs re-login to see server changes). Phase 3 not ticked. Orchestration notes: `working-agreement.md`, `orchestrator-handoff.md`. |
| 2026-10-11 | 3 | Phase 3 waves 1–2 merged: AX-3.1; the VAM app AX-3.4. Release now freezes every ref and refuses draft-only dependencies (409). Next: AX-3.6 hardware check on the Mac. Note: the VAM app `bundle.rs` changes were reviewed but not compiled in the cloud (no GTK); the Mac build is the first compile. |
| 2026-10-11 | 5 | Policy-composition design session settled design Parts IV (permissions as data) and V (E8 egress and secrets). Phase 5 rewritten as "Permissions as data" with a getting-started slice (5.1–5.9). Locked decisions 11–19 added. E8 added to the epics table as a parallel epic; org guardrails moved to E3. `developer-experience` DX-60–68 and decisions 6–7 marked superseded or amended. Branch `design/policy-stack`. |
| 2026-10-11 | 3 | Phase 3 issues created. Decision: a floating ref on a draft run resolves to the agent owner's draft of the dependency if one exists, else the latest release; release freezes to released versions only. Main merged into `feat/agent-experience` (test fixes). |
| 2026-10-11 | 2 | Phase 2 verified on Lima: all six checks pass after fixes an issue (unchanged draft releases only via Custom) and an issue (workspace picker defaults to the newest release). Follow-ups, not blocking: CLI panics when the login session expires; `vam skill get --version` returns the draft; duplicate error banner behind the release dialog; skill header shows `-draft` after release; member sees a Draft placeholder hint. Policy composition (policy stack) moved to a design session. |
| 2026-10-10 | 2 | Waves 1–2 merged. vam: AX-2.2. The VAM app: AX-2.4 version history panel release dialog with suggested bump and required note, closes an issue. Next: AX-2.6 hardware check on the Mac. |
| 2026-10-10 | 2 | AX-2.1 (branch `squ-276-release-notes-versions`): release `note` (trimmed, 500-character cap), `release_note` and `released_by` columns with an idempotent migration, `GET .../versions` for agents, playbooks, policies, skills and skill packages, `vam <kind> versions` and `release --note`. The operator-as-releaser case is blocked by the owner-only release rule; flagged on AX-2.1. |
| 2026-10-10 | 2 | Phase 2 issues created. Found while scoping: the workspace version picker ignores record versions. An issue folded into AX-2.3. |
| 2026-10-10 | 1 | **Phase 1 closed.** An issue merged (strip app-only fields before put_agent). AX-1.5 part B passed on Lima: workspace-only draft run, skill/agent/policy release in the app, attach skill and policy, member view. Follow-ups moved out of P1: an issue (P2, release message names next free version) (P4, edit instructions in the app). |
| 2026-10-10 | 1 | Wave 4 merged: AX-1.8 (agent/policy release with a chosen version, one catalog row per URN) and AX-1.9 (workspace-only run uses the workspace pin; digest filled from the current draft, not the stale bind). First, AX-1.9 session ran out of disk; retried with low-disk cargo settings. Next: AX-1.5 part B on Lima. |
| 2026-10-10 | 1 | AX-1.7 merged (version on agent/policy records). AX-1.5 part A on Lima: migration, CLI releases (agent/playbook/policy), member visibility and draft privacy PASS; workspace-only draft run FAIL -> AX-1.9 (AX-1.9); app skill release not run (needs native window). Wave 4 started: AX-1.8. 7 more failing api_tests found on main, added to an issue. |
| 2026-10-10 | 1 | AX-1.9 a run with a workspace and no agentRef takes the workspace's bound agent URN, digest and version (draft pins still load through the owner-checked lookups). Explicit agentRef mismatches still rejected. Branch `squ-271-workspace-pin-v2`. |
| 2026-10-10 | 1 | Wave 2 merged: AX-1.2 skill part. Agent/policy records lack a version, so, AX-1.7 and, AX-1.8 added. Upgrade baseline captured on Lima (comment). |
| 2026-10-10 | 0 | Tracker opened. Problem-led roadmap and the authoring model confirmed by the product owner. |
| 2026-10-10 | 1 | AX-1.6: draft playbook, policy and skill pins check the owner; foreign drafts return `no draft of <kind> '<urn>' for this user`. |
| 2026-10-10 | 1 | AX-1.2 (vam PR for `feat/ax-1-2-draft-any-higher-version`): release snapshots the one draft at any version above the latest release. Same-digest no-op, 409 with next free version, 400 for a lower version, 404 with no draft. Covers agent, playbook, policy and skill package. |
| 2026-10-10 | 1 | Wave 1 merged: AX-1.1. AX-1.6 added (owner check on draft playbook/policy pins, pre-existing gap). Wave 2 started. Base-branch test failures tracked in an issue (VAM 1.0). |
| 2026-10-10 | 1 | Milestones P1–P7 and Phase 1 issues created. Branches `feat/agent-experience` and `feature/agent-experience` cut. |
| 2026-10-10 | — | Design doc gains P11, E7 (Part III), getting-started slices per epic, and "How we build it". Decisions 9–10 added. |
