# Workstream: Member draft agents (members author their own agents from released parts)

```yaml
status: active
slug: member-draft-agents
current_phase: 2 (breakdown next)
base_branch: feat/member-draft-agents
design_docs:
  - design/member-draft-agents.md
  - docs/src/content/docs/orchestrator/user-isolation.md
  - docs/src/content/docs/orchestrator/catalog.md
  - docs/src/content/docs/getting-started/users-and-roles.md
```

## Goal

A member creates, edits and runs their own draft agents, built only from
released skills, released playbooks, released policies and connectors the org
admin enabled and the member connected; a member's agent uses released parts
only, at save and at run. API keys authorize by the user's live role, so
promotion and demotion take effect on keys as they do on sessions. The run route
never writes a released catalog row. Publishers keep today's rights; members
still cannot release, cannot author skills, playbooks, policies or skill
packages, cannot see or attach anyone's drafts, and cannot overwrite another
user's row.

## Out of scope

- Members releasing or sharing agents (a publish request flow).
- Deleting drafts (nobody can today).
- The VAM app's screens (the frontend team builds on the Phase 4 handoff).
- Honouring `resourceRequirements` (not honoured for anyone today).
- `extends` / `overlay` merge at run time (not wired for anyone today).
- Renaming decision 9's branches in `isolation-verify-closeout` (a separate
  old-repo cleanup task).

## Locked decisions

Do not re-litigate without editing this list. Locked on the owner's approval of
this capture; the open questions of Phase 1 may still adjust 2 and 3 before
wave 0 starts. Rationale and evidence: `design/member-draft-agents.md` §4.

1. **Effective API-key scopes.** At authentication: the bootstrap key and any
   key whose owner's live role is Operator keep their stored scopes; a key with
   `scope_mode = role` gets `default_scopes_for_role(live role)`; a key with
   `scope_mode = explicit` gets `stored ∩ default_scopes_for_role(live role)`.
   The result replaces `record.scopes` on the `AuthenticatedKey`; the stored list
   stays on the row and in key listings. `GET /v1/me` reports effective scopes.
2. **`api_keys.scope_mode`.** `role` when minted with no scope list, `explicit`
   otherwise. Migration: `role` where the stored list equals, as a set, the
   defaults of the owner's current role; every other existing row `explicit`.
   Mint keeps an above-role explicit scope on the row (it is capped at
   authentication).
3. Session tokens (already live), grant holders and brokers (never HTTP
   bearers; the gateway already reads the live role) do not change.
4. Release requires `publish:grant` in the **effective** scopes (or
   `keys:admin`), so a key narrowed without it cannot release and a revoked
   publisher cannot either. A queued message re-derives its caller's scopes at
   start.
5. **The run route never releases.** An inline agent with a `resourceId` is
   saved only as the caller's draft, through the same function and rules as
   `PUT /v1/agents`, and the run pins that draft; a caller who may not save it
   gets 403 and nothing is written. An inline playbook is saved as a draft only
   by a caller with `catalog:write`; anyone else gets 403. The draft-and-release
   writers (`put_*_with_owner`, `put_*_released_with_owner`) are test-only.
6. **`agents:draft`** is a new scope in the member, publisher, org admin and
   bootstrap defaults. Restricted mode = the caller holds `agents:draft` and not
   `catalog:write`. Restricted mode may use `PUT /v1/agents` and
   `PUT /v1/agents/{urn}/permissions` on its own drafts (and Always allow/deny on
   its own draft runs, as today). Every other `catalog:write` route stays closed
   to it: skills, skill packages, playbooks, policies, migrate, delete, release,
   release suggestion, hosted brokers.
7. **Released only, at save and at run.** A restricted save refuses (400) any
   skill, playbook or policy ref that does not resolve to an existing release,
   with the same message for a missing URN and someone else's draft. A run whose
   agent row is owned by a user whose live role does not author (Member), and an
   inline agent with no row run by such a caller, resolves in a
   `ReleasedOnly` mode: floating = latest release, `X.Y.Z` = that release or an
   error (no "kept as-is"), `X.Y.Z-draft` = error. Recording and session start
   use the same mode; `permissions.advanced` refs are recorded and loaded by pin.
8. A restricted save refuses `capabilityPolicy`, `hitlPolicy`,
   `instructionArtifact`, `extends`, `overlay` and `completionPolicy.evaluatorRefs`.
9. A restricted save may create a new agent URN only under
   `<registry domain>/<default_namespace(username)>/`; editing an own draft
   skips this. At most `VAMD_MEMBER_DRAFT_AGENTS_MAX` (default 50) agent drafts
   per member.
10. A member may pin their own draft agent on a workspace; foreign drafts stay
    404. Drafts are visible to their owner and to `users:admin` only.
11. Amends `isolation-verify-closeout` locked decisions 3 and 6 (amendment lines
    there point here): **3** members list/get released catalog without
    `catalog:write` and create/edit their own draft agents with `agents:draft`;
    skills, playbooks, policies, packages and every release stay on
    `catalog:write` / `publish:grant`. **6** members pin released versions or
    their own draft agent, never a foreign draft.

## Status legend

- `[ ]` not started
- `[~]` in progress
- `[x]` done (code + tests, or merged)

## Phases

### Done phases

Done phase sections are deleted; git history keeps them.

| Phase | Name | Done |
|---|---|---|
| 1 | API keys authorize by the live role | 2026-11-17: #22 (scenarios), #24 (scope_mode, effective scopes, release on effective `publish:grant`), #25 (queued start, docs). 3 scenarios active and green. |

### Phase 2 — The run route never writes released rows

**Goal:** `POST /v1/runs` with an inline agent or playbook never releases
anything; an inline agent with a `resourceId` becomes at most the caller's
private draft (same authority as `PUT /v1/agents`), an inline playbook needs
`catalog:write`, and no production path but the release routes and the
marketplace import can write a released row.  
**Depends on:** none in code; runs after Phase 1 in this lane (both touch `api/mod.rs`, so never in parallel)  
**Plan:** not drafted (scenario delta and wave sketch below)  
**Exit:** the three scenarios below are `active` and green; `vam run <dir>`
works for a publisher and runs their draft.

- [ ] Wave 0: scenarios and harness (`chat` takes `inline: <agent piece>` and
  `inlinePlaybook: <playbook piece>` with run-unique URNs, `refused`,
  `contains`; `identity-request` paths take `{{urn:<piece>}}`)
- [ ] Wave 1: run route saves drafts through the PUT path; release writers
  test-only; test fallout
- [ ] Wave 2: `vam run <dir>` message and behaviour; docs lines

**Scenario delta**

| Scenario | Change | Layer | Given | Why |
|---|---|---|---|---|
| `inline-agent-run-saves-a-private-draft-and-releases-nothing` | new | process | `org-admin`, `org-publisher`, `org-member` | a publisher's inline run works, leaves a draft only; another user gets 404 (red: the agent is released and visible today) |
| `member-cannot-publish-an-agent-through-a-run` | new | process | `org-admin`, `org-member` as two aliases | R1: 403 naming the draft rule, nothing written (red: 202 and released today) |
| `member-cannot-create-a-playbook-through-a-run` | new | process | `org-admin`, `org-member`, `agent-plain` | R1 for playbooks (red: accepted and released today) |

Rust tests that change with it: the inline-agent cases in
`tests/api_tests.rs` (2058, 2107, 2337, 2371, 3274, 3899),
`tests/hermetic_tests.rs:3188`, `tests/process_mode_run_api.rs:119`,
`tests/allowed_models_tests.rs:100` (any that rely on the release must pin the
draft instead).

**Wave sketch**

| Wave | Items | Model | Why |
|---|---|---|---|
| 0 | scenarios, harness | Sonnet low | harness pattern |
| 1 | shared "save draft as caller" used by `PUT /v1/agents` and `run_start.rs:345-351`; inline playbook rule at `:382-388`; agent and playbook pinned `-draft`; `put_*_with_owner` / `put_*_released_with_owner` test-only; test fallout | Sonnet high | the authority to publish (security boundary); orchestrator reads `run_start.rs` and the save function |
| 2 | `crates/vam-cli/src/cmd/run.rs:2090-2150` message; docs lines in `orchestrator/session.md` / `api.md` | Sonnet low | one crate, docs |

### Phase 3 — Members create, edit and run their own draft agents from released parts

**Goal:** A member saves, edits and chats with their own draft agents whose
skills, playbooks and policies are released; the save refuses anything else
with a message naming the ref; the run resolves released-only even for a former
publisher's old drafts; drafts stay private and in the member's namespace.  
**Depends on:** Phase 1, Phase 2  
**Plan:** not drafted (scenario delta and wave sketch below)  
**Exit:** the five scenarios below `active` and green;
`catalog_read_authorize_tests::member_put_agent_is_403` replaced by the
owner-approved spec change (member PUT of an own draft is 200).

- [ ] Wave 0: scenarios and harness (`publish-skill` takes `as` and
  `release: false`; agent pieces deferred and written by `put-draft` as a
  member; `chat {draft: true, as}` refused form if missing)
- [ ] Wave 1: `agents:draft`, restricted mode on the two routes, the save
  validator (refs, refused fields, namespace, cap)
- [ ] Wave 2: `ResolveMode::ReleasedOnly` in `run_resolve_context`; advanced
  refs recorded and loaded by pin; explain and check routes use the mode;
  inline agent without a row run by a member

**Scenario delta**

| Scenario | Change | Layer | Given | Why |
|---|---|---|---|---|
| `member-builds-and-runs-own-draft-agent-from-a-released-skill` | new | process | `org-admin`, `org-member`, a released skill piece, a deferred member agent piece (new) | the product: save 200, draft chat succeeds with the skill's tool; then a playbook `put-draft` and a `publish-skill` as the member are refused (red at the save: 403 today) |
| `member-draft-agent-refuses-a-skill-that-is-not-released` | new | process | `org-admin`, `org-member`, a draft-only skill piece | decision 7 at save: 400 naming the skill (red: 403 scope today) |
| `members-draft-run-takes-the-release-not-an-old-draft` | new | process | `org-admin`, `org-publisher`, a skill released then re-drafted with a different tool result | decision 7 at run: after demotion the floating ref runs the release (red: the owner's draft is used today) |
| `member-draft-agent-is-private-and-in-the-members-namespace` | new | process | `org-admin`, `org-member` as two aliases, `org-publisher`, `agent-plain` | R9, R10: another member gets 404; a URN outside the namespace is 400; another user's URN is 403 "only the artifact owner" (red at the first save) |
| `member-chats-with-a-released-agent` | unchanged; re-run | process | as today | members keep consuming released agents |

**Wave sketch**

| Wave | Items | Model | Why |
|---|---|---|---|
| 0 | scenarios, harness | Sonnet low | harness pattern |
| 1 | scope in `keys.rs` sets; `api/mod.rs:576`, `api/permissions.rs:155`; validator; namespace; cap; spec change to `member_put_agent_is_403` | Sonnet high | catalog write authority for a new class of user (security boundary) |
| 2 | `catalog_resolve.rs:180-202`, `:561-640`, `record_resolved_refs` advanced refs, `security.rs:398-470`, `permissions_view.rs:640-660`, `api/composition.rs:102` | Sonnet high | run-time resolution and session start must agree (security boundary) |

### Phase 4 — Docs and frontend handoff

**Goal:** The docs say what members, publishers and keys can do; the frontend
team has the handoff (`design/member-draft-agents.md` §9) on the docs site.  
**Depends on:** Phase 3  
**Plan:** not drafted  
**Exit:** pages updated, `vam-e2e check` and `pnpm build` green.

- [ ] `getting-started/users-and-roles.md`, `orchestrator/user-isolation.md`,
  `orchestrator/catalog.md`, `security/permissions.md`, `orchestrator/api.md`
  (member section with the handoff: show vamd's message, attach by refs only,
  never re-PUT a skill, show or hide create/release/author from `GET /v1/me`
  scopes, the member URN namespace, draft pins)

**Scenario delta:** none (docs only; `proves:` lists the scenarios of Phases
1–3). **Wave sketch:** one wave, Haiku 5.5 high (docs only), 45 min, actions
`docs` only.

## Explicitly later

- A member asks a publisher to release their agent (request and review flow).
- Draft deletion and archive.
- Capping `resourceRequirements` for members once the host honours it.
- Member access to `extends` / `overlay` once the run-time merge is wired.
- Renaming `isolation-verify-closeout` decision 9's branch names (old-repo
  cleanup).

## Progress log

| Date | Phase | What landed (end with the scoreboard: scenarios green, red, fix cycles, wall time) |
|---|---|---|
| 2026-11-17 | — | Tracker opened from the owner's member-authoring brief; design `design/member-draft-agents.md`; Phase 1 breakdown drafted |
| 2026-11-17 | 1 | **Phase 1 closed.** Merged #22, #24, #25 into `feat/member-draft-agents` (Actions green on each). Scoreboard: 3 scenarios green, 0 red; 0 scenario fix cycles. Next: Phase 2 breakdown (the run route never writes released rows). |
