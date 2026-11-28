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
   version_stale, quota.
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
**Plan:** not drafted  
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

**Scenario delta** (proposed in the design; the breakdown confirms it)

| Scenario | Change | Layer | Given | Why |
|---|---|---|---|---|
| `denied-tool-tells-the-model-who-can-allow-it` | new | process | `org-member`, `agent-with-guarded-tool` | fix line |
| `egress-refuses-an-ungranted-host` | changed: deny carries a denial naming the agent owner | process | `org-admin`, `agent-with-shell-and-no-internet` | egress typed |
| `missing-secret-refusal-names-the-secret-to-set` | new | process | `org-member`, `agent-with-shell-and-secret-skill` | 412 body |
| `member-asks-an-org-admin-to-enable-a-connector` | new | process | `org-member`, `org-admin` | request |
| `cli-prints-the-fix-from-the-error-body` | new | process | `org-member` | CLI |
| `fix-line-ignores-text-the-tool-wrote` | new | process | `agent-with-guarded-tool` | injection |

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
