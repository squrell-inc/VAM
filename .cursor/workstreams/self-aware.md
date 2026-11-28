# Workstream: Self-aware (denials that carry their fix, one-click agent upgrade)

```yaml
status: active          # active | paused | done
slug: self-aware
current_phase: S1
base_branches:
  S1: fix/typed-denials
  S2: fix/space-upgrade
  S3: later (after agent access is in main)
  S4: later (inside or after Apps P2, on feat/apps-spaces)
design_docs:
  - design/self-aware.md
  - design/vam-apps-spaces.md   # Amendment 2026-11-28 §5 (the shared seam), P2/P4
```

## Goal

Every refusal VAM gives says, in host-written words, what would fix it and who
can do it, with a button that applies the fix or sends a request to that
person; the model gets one "Suggested fix" line it can repeat to the user. A
Space's owner sees when a newer release of its agent exists and moves the Space
to it, or back, with one click. Later, an agent can propose a change from a
closed list that a person approves, and a shared App can follow the latest
release with canary and rollback.

## Out of scope

- Guest-side settings writes of any kind (the guest only proposes, S3).
- Applying any change without a human.
- Fix text written by the model or by tools.
- New `VamErrorCode`s (agent-access A8).
- Hot reload of policy into a running session.
- Per-message agent version switch.
- Automatic upgrade of personal Spaces.
- Carrying conversation into a thread that starts fresh after an upgrade (S2).

## Locked decisions

Do not re-litigate without editing this list. All entries are **proposed, owner
to confirm** (2026-11-28); none is approved yet.

1. A refusal carries a `vam.v1.Denial {reason, subject, fix, text}` in
   `ErrorStatus.details` and as `denial` in HTTP error bodies; the message text
   is unchanged. `DenialReason`: policy_rule, egress_rule, secret_missing,
   connector_not_connected, connector_disabled, agent_not_granted,
   version_stale, quota; `space_busy` is reserved for S2.
2. `SuggestedFix {change, target, who: self|agent_owner|org_admin, route,
   caller_can_apply}`; `change` is a closed enum. Clients act on `change` and
   `target`, never on an arbitrary path from the body.
3. Fix text comes from host templates and host identifiers only. The guest
   builds the model's line from typed fields and its own template table; the
   line never tells the model to retry, escalate or look for another way.
4. A fix the caller cannot apply becomes a request: agent access requests for
   `agent_not_granted`, `POST /v1/fix-requests` (the denial carries a
   reference to its stored decision row; the host resolves it, checks the
   requester's entitlement, and derives the fix and recipients from it) and a
   `fix.requested` notification for the rest. No queue in S1.
5. The Space row is the authority for a Space's version pin (`agent_version`,
   `previous_agent_version`), backfilled lazily from `workspace.json`; the file
   becomes "last version run here" and is rewritten from the row at the next
   session start. Apps P4 (AS-2.6) calls the same seam to route App Spaces to
   the App pin.
6. **Change to isolation-verify-closeout decision 5 (proposed):** the pin is set
   at the first run and may be changed later only by the Space owner through
   `set_space_agent_version(owner, space_id, version)`. Its "Explicitly later:
   workspace migrates from one agent version to another in place" moves here
   (S2). Decision 6 (who may pin draft) applies to the re-pin target unchanged.
7. A personal re-pin is refused while anything is live, queued or parked on the
   Space; an App pin move (S4) is not, and live runs finish on their spec.
8. A thread whose log called a tool the new version lacks, or whose schema
   changed, continues in a new thread linked by `parent_thread`; logs are
   never rewritten.
9. Q10: personal Spaces stay pinned with an "Agent updated, use vX?" prompt;
   shared App Spaces follow the App pin.
10. Q11: a yanked version refuses new runs, its Spaces get a forced upgrade
    prompt, running sessions finish (settles DX-94).
11. Q15: agent-proposed changes after agent access is in `main`; human approves;
    host applies with the approver's rights; closed list; not decidable from
    notification surfaces.

## Status legend

- `[ ]` not started
- `[~]` in progress
- `[x]` done (code + tests, or merged)

## Phases

### Done phases

Done phase sections are deleted; git history keeps them.

| Phase | Name | Done |
|---|---|---|

### Phase S1 — Typed denials

**Goal:** every refusal site returns a typed denial with a host-written fix;
CLI and Squrell draw a fix card; "ask your admin" becomes a request.  
**Depends on:** none (`agent_not_granted` is reserved until agent access is in
`main`, then wired).  
**Base branch:** `fix/typed-denials`  
**Plan:** breakdown below (draft 2026-11-28, awaiting owner approval; summary page private to the owner)  
**Exit:** the Cedar, permissions-row, catalog, egress, missing-secret,
connector-disabled, connector-park, version-mismatch and quota refusals carry a
`Denial`; the denied observation carries the fix line; `vam` prints `Fix:` or
`Ask:` from `denial`; Squrell draws the card from error bodies and run events;
a member's request reaches the right admin as `fix.requested`. Design:
`design/self-aware.md` §S1.

- [ ] `Denial`, `DenialReason`, `SuggestedFix` in proto; descriptor baseline
- [ ] Denials at every `PERMISSION_DENIED` site in `security.rs`, with `who` from the deciding scope
- [ ] `denial` on `ApiError` bodies (412 secret, 409 version mismatch, 429 inbox full, connector refusals)
- [ ] Egress denial on `egress.decided` and in the proxy refusal body
- [ ] Guest fix line in `denied_observation`, from typed fields only
- [ ] `POST /v1/fix-requests` and the `fix.requested` notification kind
- [ ] CLI reads `denial` (fallback to message matching); version-mismatch hint
- [ ] Squrell fix card (error bodies and run events), in Squrell-App
- [ ] Docs: `orchestrator/denials.md`, error body in `orchestrator/api.md`

#### Issues found (checked in code against `main` @ f1c1431)

| # | Existing problem | Resolution |
|---|---|---|
| 1 | The design says a denial points at "the stored decision row". `run_decisions` is keyed `(run_id, seq)` with `seq` computed inside the INSERT and never returned (`run_facts.rs:418-447`); recording is evidence-only and may fail silently (`:452-468`); a session with no run records nothing (`security.rs:864-873`). Egress denials and pre-run 412s have no row at all. | New `denials` table (id `dnl_<ulid>`, run_id nullable, session, caller, agent URN and version, reason, subject, scope, target, created_at). Every denial with a fix the caller cannot apply writes one; `decisionRef` is its id. If the write fails the refusal still goes out, with no `decisionRef` (text only, no request button). `run_decisions` and receipts are unchanged. |
| 2 | The design maps only deciding scope `agent` and `host`. The code produces `org`, `source`, `agent`, `platform` (`session_targets.rs:396-420`), `session`, `host`, `playbook`, `path`, `policy` (`security.rs:784-826`). | One mapping table (below, SA-1.3), unit-tested per scope. `agent` and `platform` (no rule matched, default deny) → agent owner, `ALLOW_TOOL`; `org` → org admin; `source`, `session`, `host`, `path`, `playbook` → no fix; legacy `policy`/`none` → agent owner, text only ("migrate to permissions first"). |
| 3 | The connector gate is a sixth `PERMISSION_DENIED` site the design does not list (`connector_park.rs:290-302`), and its refusals record no decision. The "server does not know this connector" case (`:113`) is also a dead end. | SA-1.3 covers `gate_connector`: `CONNECTOR_DISABLED` (enable, org admin) for both texts, with a `denials` row so the member can send the request. The park keeps working as today and gains `CONNECTOR_NOT_CONNECTED` (self). |
| 4 | Marketplace-installed agents are read-only on the node, so their "owner" cannot change a rule here; "agent admins" exist only on `feat/agent-access`. | On `main`, `AGENT_OWNER` resolves to the catalog owner (`owner.rs:237`). An installed agent's policy deny has no fix and the text says its publisher sets the rule. One function `fix_recipients()` holds this, so agent access swaps it to agent admins later. (Q3) |
| 5 | There is no one-call route to add one permission rule. The owner changes their draft with `PUT /v1/agents/{urn}/permissions` (replace) and then releases. | `ALLOW_TOOL`/`ALLOW_HOST` fixes name that route; Squrell opens the draft permissions editor prefilled with the rule; the CLI prints the `vam agent permissions` command and says a release follows. No new write route. (Q4) |
| 6 | The design's CLI hint `vam space upgrade` does not exist until S2, and the version mismatch is raised in `catalog_resolve.rs`/`run_start.rs`, which S1 must not touch. | S1 builds the `VERSION_STALE` denial only in the `From<OrchestratorError>` mapping in `api/error.rs` (the error already carries workspace, bound and requested version). Fix in S1: run again on the Space's bound version (self). `SET_SPACE_VERSION` is reserved in the enum for S2. |
| 7 | `MissingRequiredSecrets(String)` is one pre-formatted string that can list several secrets (`secret_requirements.rs:226-252`); a denial has one subject. | The error variant carries a list of `(binding, scope)`; the message text stays byte-identical. The body gets `denials` (one per secret) and `denial` = the first. The 412 row is written in `admit_run_secret_requirements` (`api/mod.rs:2322`), which has the caller and agent. (Q2) |
| 8 | Squrell desktop drops error bodies: `handle_http_error` keeps only `message` (`src-tauri/src/vamd.rs:121-208`), and 412 falls through to `Unreachable("HTTP 412: ...")`. Web keeps the body (`apiClient.ts` `ApiError.body`). | `ConnectError` variants gain an optional `denial`; 412 gets its own mapping. You run `cargo test` for `src-tauri` locally, as in agent access Phase 4. |
| 9 | Egress: the decision callback passes only `(domain, port, disposition, policy_id)` (`broker.rs:181`); the proxy's 403 body is one line capped at 200 chars (`proxy/mod.rs:814`); `vam-network` already has an internal struct named `Denial`. | The orchestrator's egress callback builds the typed denial from the rule label it already makes (`brokers.rs:1300-1311`: `agent rule ...` → agent owner `ALLOW_HOST`; IP floor, port list, secret destination → no fix). `EgressAuthorization::Deny` gains `owner_can_allow: bool`, so the proxy appends one fixed phrase inside the cap. The proto type is always written `vam_spec::v1::Denial`. |
| 10 | Notification rows have no typed payload (`notifications.rs:209-236`), so `fix.requested` cannot carry the fix for the admin's button, and there is no 24-hour dedupe. | New `fix_requests` table (id, requester, change, target, who, denial id, created_at, unique per requester+change+target per 24 h). The notification's `subject_key` is the request id; its JSON gains `fixRequest`. A repeat within 24 h answers 200 with the existing request. |
| 11 | Files S1 changes are also changed on `feat/agent-access`, not yet in `main`: `notifications.rs` (Kind enum), `api/error.rs`, `error.rs`, `vam-cli/src/http_error.rs`, `openapi.yaml`; in Squrell-App `vamd.rs`, `apiClient.ts`, `types/vam.ts`. | All S1 edits are additive. The S1 PR into `main` goes after agent access is released, rebased on it; `AGENT_NOT_GRANTED` is wired in that same PR if agent access is in `main` by then, otherwise it stays reserved. (Q6) |
| 12 | Tracker locked decision 1 lists 8 reasons; the design's table has 9 (`SPACE_BUSY`, raised by S2). | The proto reserves `SPACE_BUSY` and `SET_SPACE_VERSION` now so S2 adds no enum churn; the tracker line is corrected. |

#### Deciding scope → who (SA-1.3)

| Deciding scope (code) | Reason | Change | Who |
|---|---|---|---|
| `agent`, `platform` | POLICY_RULE | ALLOW_TOOL | AGENT_OWNER (SELF when the caller owns the agent's draft and runs it) |
| `org` | POLICY_RULE | ALLOW_TOOL | ORG_ADMIN |
| `policy`, `none` (legacy capability_policy) | POLICY_RULE | none, text "the agent owner must migrate it to permissions" | AGENT_OWNER |
| `source`, `session`, `host`, `path`, `playbook` | POLICY_RULE | none | none |
| connector unknown or not enabled | CONNECTOR_DISABLED | ENABLE_CONNECTOR | ORG_ADMIN |
| connector not connected (park) | CONNECTOR_NOT_CONNECTED | CONNECT_CONNECTOR | SELF |
| egress `agent rule ...` | EGRESS_RULE | ALLOW_HOST | AGENT_OWNER |
| egress IP floor, port list, secret destination, policy error | EGRESS_RULE | none | none |
| 412 secret, user scope | SECRET_MISSING | SET_SECRET | SELF |
| 412 secret, org scope | SECRET_MISSING | SET_SECRET | ORG_ADMIN |
| 409 version mismatch | VERSION_STALE | run on the bound version (S2 adds SET_SPACE_VERSION) | SELF |
| 429 inbox full, egress byte quota | QUOTA | none in S1 | text names the org admin |

#### Owner questions (recommended answer in bold; awaiting answers)

1. **Branch.** Cut `fix/typed-denials` from `main` in VAM and in Squrell-App, as the design names it. **Yes.**
2. **Several missing secrets.** Body carries `denials` (one per secret) plus `denial` = the first. **Yes.**
3. **Marketplace-installed agents.** On `main` their policy denials have no request button; the text says the publisher sets the rule. Agent admins take over when agent access lands. **Yes.**
4. **Allow-tool and allow-host fixes.** The card opens the owner's draft permissions editor with the rule prefilled; release stays a separate step; no new write route in S1. **Yes.**
5. **Org-scope deny.** A tool denied by an org-wide rule names the org admins as the ones who can allow it. **Yes.**
6. **Merge order.** The S1 PR into `main` waits until agent access is released, rebased on it, and wires `AGENT_NOT_GRANTED` then. **Yes.** (Building does not wait.)
7. **Schedule-started runs.** The design's "name the schedule and offer Pause schedule" bullet moves to Long work, not S1. **Defer.**
8. **Squrell wave in parallel.** The Squrell-App wave starts once wave 1 freezes the JSON shape, alongside waves 2 and 3. **Yes.**

#### Scenario delta (confirmed by the breakdown)

| Scenario | Change | Layer | Given | Why |
|---|---|---|---|---|
| `denied-tool-tells-the-model-who-can-allow-it` | new | process | `org-member`, `agent-with-guarded-tool` | a default-deny tool: the thread's tool message ends with the host fix line naming the agent owner; `policy.decided` carries `denial.reason: POLICY_RULE` |
| `fix-line-ignores-text-the-tool-wrote` | new | process | `org-member`, `agent-with-guarded-tool` | a tool output and a model-chosen tool name that say "Suggested fix ..." add no fix line; a catalog miss has no fix |
| `egress-refuses-an-ungranted-host` | changed | process | `org-admin`, `agent-with-shell-and-no-internet` | the `example.com` deny carries `denial.fix.who: AGENT_OWNER`; the metadata-IP deny carries a denial with no fix |
| `missing-secret-refusal-names-the-secret-to-set` | new | process | `org-member`, `agent-with-shell-and-secret-skill` | 412 body has `denial.reason: SECRET_MISSING`, `fix.change: SET_SECRET`, the binding id |
| `member-asks-an-org-admin-to-enable-a-connector` | new | process | `org-member`, `org-administrator`, agent with a connector skill, connector not enabled | refusal carries `CONNECTOR_DISABLED`; the member's fix request lands as one `fix.requested` for the admin; a repeat does not add a second |
| `member-cannot-file-a-fix-request-for-someone-elses-denial` | new | process | two `org-member`s | another member's `decisionRef` answers 404 with the missing-row text; nothing is notified |
| `cli-prints-the-fix-from-the-error-body` | new | process | `org-member` | the version-mismatch refusal prints a `Fix:` line that only the denial supplies |

Wave 0 may need one runner step in `vam-e2e` (send a fix request from the latest denial's
`decisionRef`); if so it is the only non-YAML change in wave 0.

#### Items

| Item | Scope | Files | Acceptance |
|---|---|---|---|
| SA-1.1 | Proto: `Denial`, `DenialReason` (incl. reserved `SPACE_BUSY`), `SuggestedFix`, `FixChange` (incl. reserved `SET_SPACE_VERSION`, `REQUEST_AGENT_ACCESS`, `STOP_RUN`), `FixWho`; `denial` on `PolicyDecidedPayload` and `EgressDecidedPayload`; descriptor baseline | `proto/vam/v1/common.proto`, `events.proto`, baseline | `cargo run -p vam-spec --example generate_baseline`; proto checks green |
| SA-1.2 | `denials` store and id; host text templates keyed `(reason, change, who)`; `fix_recipients()` | new `crates/vam-orchestrator/src/denials.rs`, schema init | unit tests per template; write failure leaves the refusal intact |
| SA-1.3 | Denials at every `PERMISSION_DENIED` site in `security.rs` (5) and `connector_park.rs` (1), with the mapping table above; `policy.decided` carries it | `security.rs`, `connector_park.rs`, `events.rs` | per-scope unit tests; scenario `denied-tool-...` |
| SA-1.4 | Guest fix line in `denied_observation` from `ErrorStatus.details` only, fixed template table in the guest | `crates/vam-runtime/src/agent.rs` (+ a small template module) | unit tests incl. injected text; scenario `fix-line-...` |
| SA-1.5 | `ApiError.denial`/`denials` and the JSON body; 412 (list of secrets, row written in `admit_run_secret_requirements`), 409 version, 429 inbox | `api/error.rs`, `error.rs`, `secret_requirements.rs`, `api/mod.rs`, `docs/api/openapi.yaml` | message text byte-identical (existing tests); scenario `missing-secret-...` |
| SA-1.6 | Egress denial on `egress.decided` and in the proxy body | `brokers.rs`, `events.rs`, `vam-network` `gate.rs`, `broker.rs`, `proxy/` | scenario `egress-refuses-...` (changed) |
| SA-1.7 | CLI reads `denial` first, falls back to matching; version-mismatch hint; `Ask:` line | `crates/vam-cli/src/http_error.rs` | unit tests; scenario `cli-prints-...` |
| SA-1.8 | `POST /v1/fix-requests`, `fix_requests` table, `fix.requested` kind, entitlement check, 24 h dedupe; `vam fix request <ref>` | new `api/fix_requests.rs`, `notifications.rs`, CLI | scenarios `member-asks-...`, `member-cannot-file-...` |
| SA-1.9 | Squrell: denial on web `ApiError` and desktop `ConnectError`; `FixCard` from error bodies and run events; `fix.requested` card; connector card on the same type | Squrell-App `src-tauri/src/{error,vamd}.rs`, `src/services/apiClient.ts`, `src/types/vam.ts`, `src/components/chat/` | vitest, `tsc`, Squrell e2e; you run `src-tauri` cargo tests |
| SA-1.10 | Docs: new `orchestrator/denials.md`, error body in `orchestrator/api.md`, `notifications.md` kind; tracker close | `docs/src/content/docs/orchestrator/` | docs workflow (`vam-e2e check`, `pnpm build`) |

#### Waves

| Wave | Items | Turns green | Repo | Model | Budget | Depends on |
|---|---|---|---|---|---|---|
| W0 scenarios | the 7 scenarios as `pending`, world pieces, runner step if needed | none (all red for the right reason) | VAM | Sonnet medium | 1.5 h | approval |
| W1 contract and tool denials (security) | SA-1.1 to SA-1.4 | `denied-tool-...`, `fix-line-...` | VAM | Sonnet high, Opus high plan review first | 3 h | W0 |
| W2 HTTP, egress and CLI | SA-1.5 to SA-1.7 | `missing-secret-...`, `egress-refuses-...`, `cli-prints-...` | VAM | Sonnet medium | 2.5 h | W1 |
| W3 fix requests (security) | SA-1.8 | `member-asks-...`, `member-cannot-file-...` | VAM | Sonnet high, Opus high plan review first | 2.5 h | W2 |
| W4 Squrell fix card | SA-1.9 | Squrell tests | Squrell-App | Sonnet medium | 3 h | W1 (JSON frozen); parallel with W2, W3 |
| W5 docs and close | SA-1.10; Opus high close review; fix wave if needed | all 7 active and green in CI | VAM | Sonnet low, Opus high review | 1.5 h | W3, W4 |

Build waves in the same repo never run in parallel. Each wave lands as one PR into
`fix/typed-denials`; the phase branch goes to `main` only on your word (Q6).

#### Out of scope (later)

S2 Space upgrade route and `SET_SPACE_VERSION` button; S3 agent-proposed changes; App quotas
(Apps P6); `AGENT_NOT_GRANTED` until agent access is in `main`; "Pause schedule" on schedule-started
runs (Long work); an org-wide cap on fix requests.

### Phase S2 — Space upgrade

**Goal:** a Space shows its pinned and latest version, and its owner re-pins
it (or rolls back) with one call; minimal yank.  
**Depends on:** agent access in `main` (touches `run_start.rs`,
`catalog_resolve.rs`; holds the hot-file lock while building, Q16); owner
answers on Q10, Q11 and on the change to isolation-verify-closeout decision 5
(locked decision 6 above; owner question).  
**Base branch:** `fix/space-upgrade`  
**Plan:** not drafted  
**Exit:** GET Space shows `agentVersion`, `latestVersion`,
`previousAgentVersion`, `updateAvailable`, `pinnedVersionYanked`;
`POST /v1/spaces/{id}/agent-version` re-pins with the owner, target, access and
busy checks (busy check and pin write serialized with run admission), an audit record and `agent.updated`; the next run uses the new
pin; affected threads continue fresh; a yanked version refuses new runs and
prompts the upgrade; Squrell banner and `vam space upgrade`. Design:
`design/self-aware.md` §S2.

- [ ] `spaces.agent_version`, `previous_agent_version`; lazy backfill; `apply_workspace_version_pin` reads the row
- [ ] `repin_workspace_json` at session start
- [ ] `set_space_agent_version` seam with every check
- [ ] GET Space fields; `POST /v1/spaces/{id}/agent-version`; audit; `agent.updated`
- [ ] Thread compatibility check and fresh-thread fallback
- [ ] `vam space show`, `vam space upgrade`
- [ ] Yank route; yanked refusal with `version_stale` denial; latest skips yanked
- [ ] Squrell "Agent updated" banner and yanked prompt, in Squrell-App

**Scenario delta** (proposed in the design; the breakdown confirms it)

| Scenario | Change | Layer | Given | Why |
|---|---|---|---|---|
| `space-shows-a-newer-released-version` | new | process | `org-member`, `org-publisher` | read side |
| `owner-moves-a-space-to-the-latest-version-and-the-next-run-uses-it` | new | process | as above | exit |
| `space-upgrade-waits-for-the-live-task` | new | process | as above | busy |
| `space-upgrade-can-be-rolled-back` | new | process | as above | rollback |
| `member-cannot-move-another-members-space` | new | process | two members | owner |
| `thread-that-used-a-removed-tool-continues-in-a-new-thread` | new | process | agent dropping a tool | thread check |
| `yanked-version-refuses-new-runs-and-asks-to-upgrade` | new | process | one yanked release | Q11 |

### Phase S3 — Agent-proposed change card

**Goal:** an agent proposes a change from a closed list; a person approves; the
host applies it with the approver's rights or sends it to the admin queue.  
**Depends on:** agent access in `main` (admin queue); S1; owner answer on Q15.  
**Base branch:** not chosen  
**Plan:** not drafted  
**Exit:** `propose_change` with approval kind `change_proposal` and jig
`vam.change_proposal`; host validation per change kind (hosts only from declared
skill egress); per-run cap and per-target cooldown; not decidable from
notification surfaces; Cedar gates the next call. Design:
`design/self-aware.md` §S3.

- [ ] Built-in tool, approval kind, built-in jig
- [ ] Host validation and apply through existing write routes
- [ ] Admin queue routing (agent access queue with a `kind`)
- [ ] Limits and surface rule
- [ ] Squrell card and CLI `vam approve` support

### Phase S4 — App version follow and bulk upgrade

**Goal:** a shared App pins exactly or follows the latest release, with canary
and rollback; one write upgrades every Space.  
**Depends on:** Apps P2 (lane 1, `feat/apps-spaces`; S4 is built inside or
after it, the App-pin routing lands with Apps P4); S2. Built inside the Apps
lane's hot-file lock.  
**Base branch:** not chosen  
**Plan:** not drafted  
**Exit:** `version_mode` exact or follow; canary Spaces move first; live runs
finish on the old version; rollback restores the previous pin; a yank moves
follow Apps back and blocks exact Apps with a prompt to the App owner;
turning on follow is the owner's revocable approval for later moves, each
notified. Design:
`design/self-aware.md` §S4.

- [ ] App row fields and the follow resolver
- [ ] Canary and rollback
- [ ] App owner prompt for a yanked exact pin

## Explicitly later

- Carrying a text-only digest into a fresh-start thread.
- `agent.update_available` notifications on release.
- An org-wide cap on fix requests.
- Agent-proposed Space upgrades (the proposing task is itself live).

## Progress log

| Date | Phase | What landed (end with the scoreboard: scenarios green, red, fix cycles, wall time) |
|---|---|---|
| 2026-11-28 | — | Tracker opened with `design/self-aware.md` |
| 2026-11-28 | S1 | Breakdown drafted (issues, scenario delta, items SA-1.1 to SA-1.10, waves W0 to W5); awaiting owner approval |
