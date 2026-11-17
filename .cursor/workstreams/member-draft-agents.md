# Workstream: Member draft agents (members author their own agents from released parts)

```yaml
status: active
slug: member-draft-agents
current_phase: 1 (breakdown drafted 2026-11-17, waiting for owner approval)
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

### Phase 1 — API keys authorize by the live role

**Goal:** A key's rights follow its user's live role: a default key gains
`catalog:write` on promotion and loses it on demotion, an explicitly narrowed
key never widens past its list, a member key minted with `catalog:write` cannot
write the catalog, and release honours a key narrowed without `publish:grant`.  
**Depends on:** none  
**Plan:** drafted 2026-11-17, waiting for owner approval  
**Exit:** the three new scenarios are `active` and green; operator, grant and
session behaviour unchanged (re-run list below green).

- [ ] Wave 0: scenarios and harness
- [ ] Wave 1: effective scopes, `scope_mode`, release on effective scopes
- [ ] Wave 2: queued start re-derives scopes; docs lines

**Benchmark delta:** none. Access control moves no line in
`design/benchmark.md` §1; the line is the launch proof "a member builds their
own agent" (Phase 3), which needs this phase.

**Scenario delta** (design/scenarios.md §9; touched scenarios found from
`guards` on `keys.rs`, `api/auth.rs`, `users_keys_admin.rs` and `given:
org-*`)

| Scenario | Change | Layer | Given | Why |
|---|---|---|---|---|
| `promotion-widens-a-default-key-but-not-a-narrowed-one` | new | process | `org-admin`, `org-member` as `member`, `org-member-with-a-narrow-key` as `narrow` (new), `playbook-draft-probe` (new) | exit: a default key gains on promotion; a narrowed key does not |
| `demotion-takes-catalog-writes-from-a-publisher-key` | new | process | `org-admin`, `org-publisher` as `pub` (new), `playbook-draft-probe` | exit: demotion sticks on keys (R2) |
| `member-key-minted-with-catalog-write-cannot-write-the-catalog` | new | process | `org-admin`, `org-member-with-a-catalog-key` as `member` (new), `playbook-draft-probe` | exit: the live role caps an explicit key (R3) |
| `member-chats-with-a-released-agent` | unchanged; re-run in W1 | process | `org-admin`, `org-member` | default member key must keep run and approve rights |
| `trigger-starts-a-run-as-the-grant-owner` | changed: guards add `crates/vam-orchestrator/src/keys.rs`, `crates/vam-orchestrator/src/inbox.rs`; steps unchanged | process | as today | grant holders must be unaffected (decision 3) |
| `connectors-reach-members-only-after-an-org-admin-enables-them` | unchanged; re-run in W1 | process | `org-member`, `org-administrator` | an org admin's default key keeps `users:admin` |
| `api-key-cannot-change-a-password` | unchanged; re-run in W1 | process | `org-admin`, `org-member` | bootstrap key unchanged |

The probe writes a **playbook** draft (`PUT /v1/playbooks`), because that route
stays on `catalog:write` for good; an agent draft would open to members in
Phase 3 and turn these scenarios into the wrong test.

Sketch of each new scenario (all red class `product`):

- `promotion-widens-a-default-key-but-not-a-narrowed-one`. `when`:
  `put-draft {piece: playbook-draft-probe, as: member, status: 403, contains:
  "catalog:write"}`; `set-role {as: member, role: publisher}`; `set-role {as:
  narrow, role: publisher}`. `then`: `put-draft {as: member, status: 200}`;
  `identity-request {as: member, credential: key, path: /v1/me, status: 200,
  contains: "catalog:write"}`; `put-draft {as: narrow, status: 403, contains:
  "catalog:write"}`; `identity-request {as: narrow, credential: key, path:
  /v1/me, status: 200, lacks: "catalog:write"}`. Red at `then.0` (403: the
  stored member scopes do not change on promotion).
- `demotion-takes-catalog-writes-from-a-publisher-key`. `when`: `put-draft {as:
  pub, status: 200}`; `set-role {as: pub, role: member}`. `then`: `put-draft {as:
  pub, status: 403, contains: "missing required scope 'catalog:write'"}`;
  `identity-request {as: pub, credential: key, path: /v1/me, status: 200, lacks:
  "publish:grant"}`. Red at `then.0` (200 today).
- `member-key-minted-with-catalog-write-cannot-write-the-catalog`. `when`:
  `put-draft {as: member, status: 403, contains: "catalog:write"}`. `then`:
  `identity-request {as: member, credential: key, path: /v1/me, status: 200,
  lacks: "catalog:write"}`; `set-role {as: member, role: publisher}`;
  `put-draft {as: member, status: 200}` (minted for it, so it gains it).
  Red at `when.0` (200 today).

**Wave 0 "scenarios"** writes the three scenario files `pending` with
`expectFail` at the steps above, the guards change on
`trigger-starts-a-run-as-the-grant-owner`, the new world pieces and these
harness additions in `crates/vam-e2e` (documented in `e2e/README.md`):

- `org` pieces take `scopes: [..]`, sent as the scope list of
  `POST /v1/users/{id}/keys` (absent = role defaults, as today). New pieces
  `org-publisher` (role Publisher), `org-member-with-a-narrow-key` (role Member,
  `scopes: [sessions:write, runs:write, approvals:write, notifications:read,
  node:read]`), `org-member-with-a-catalog-key` (role Member, the member set plus
  `catalog:write`).
- `playbook-draft-probe`: a `playbook` piece with `deferred: true` (not written
  at `given`), body copied from `playbook-one-step`.
- Step `put-draft: { piece, as, status, contains }`: PUTs the piece's body to
  `/v1/playbooks` or `/v1/agents` (by the piece's kind) with an
  `Idempotency-Key`, as the identity (the bootstrap key when `as` is omitted).
  The URN's `{{name}}` becomes `<piece>-<as>-<scenario>-<runTag>`, so every
  identity writes its own row and the owner check never decides the answer.
  Exact status; `contains` is a body substring. Cleanup: none needed (drafts
  are run-unique).
- Step `set-role: { as, role: publisher | member }`: as the bootstrap key,
  `POST` (publisher) or `DELETE` (member) `/v1/users/{id}/publish-grant` for
  the identity; a non-2xx answer is class `harness`.
- `identity-request` takes `contains` and `lacks` (body substrings, judged after
  the status).

**Items** (each names the pending scenarios it turns green)

- [ ] `MD-1.0` — Wave 0 above. Files: `e2e/scenarios/` (3 new, 1 guards edit),
  `e2e/world/` (4 new pieces), `crates/vam-e2e/src/{world.rs,engine.rs,identity.rs,scenario.rs}`
  (or a new `roles.rs`), `crates/vam-e2e/src/check.rs` if the linter must
  know the new fields, `e2e/README.md`. **Accept:** `cargo run -p vam-e2e --
  check` clean; `scripts/e2e/run.sh --scenarios
  promotion-widens-a-default-key-but-not-a-narrowed-one,demotion-takes-catalog-writes-from-a-publisher-key,member-key-minted-with-catalog-write-cannot-write-the-catalog`
  reports each red at its `expectFail` step, class `product`; `--smoke` green.
  `Decide: NOW — proof 3 (security floor R2, R3), cost 1, line launch proof "a member builds their own agent".`
- [ ] `MD-1.1` — **`scope_mode` on keys.** Column, migration with the backfill
  rule (decision 2), `create_api_key_owned` (or a sibling) takes the mode, both
  mint routes set `role` for an empty request and `explicit` otherwise. Files:
  `crates/vam-orchestrator/src/keys.rs` (`initialize_api_keys_schema` :269,
  `create_api_key_owned` :328-363, `key_cols!`, `parse_api_key_row` :645),
  `api/mod.rs:402-450` (`create_key`), `api/users_keys_admin.rs:64-110`
  (`mint_user_key`). Keep existing callers compiling (test helpers in
  `users.rs:1371`, `:1409`, `api/connectors.rs:385`, `api/mcp_connections.rs:1484`,
  `api/mod.rs:6531`, `notify_gateway/tests.rs:1097`, `tests/*.rs`). **Accept:**
  unit tests: empty request stores `role`; a list stores `explicit`; backfill
  marks a member's member-set key `role`, a publisher's member-set key
  `explicit`, a key with an extra scope `explicit`; a reopened store keeps the
  column. `cargo test -p vam-orchestrator keys`.
  `Decide: NOW — proof 3 (security floor), cost 2 (schema), line launch proof.`
- [ ] `MD-1.2` — **Effective scopes at authentication; release on effective
  scopes.** `effective_scopes(record, role)` per decision 1, applied in
  `authenticate_api_key` (`keys.rs:589-643`) before the `AuthenticatedKey` is
  built (the only place a key row becomes an `AuthenticatedKey`).
  `require_publish_grant` (`api/catalog_auth.rs:33-42`) requires
  `publish:grant` in effective scopes or `keys:admin`. Files: `keys.rs`,
  `api/catalog_auth.rs`, their unit tests; `api/auth.rs` tests if helpers move.
  **Accept:** unit tests for each branch: bootstrap key and Operator owner keep
  stored scopes; role mode follows promote and demote; explicit mode is capped
  and never widened beyond its list; unknown owner = member set; deactivated
  owner refused (unchanged); a publisher key minted without `publish:grant` is
  403 on release; `GET /v1/me` with a key shows effective scopes
  (`tests/me_tests.rs`). Turns green the three new scenarios.
  `Decide: NOW — proof 3, cost 1, line launch proof.`
- [ ] `MD-1.3` — **Test fallout.** `tests/isolation_coverage_map_tests.rs:48-52`
  mints members with `catalog:write`: mint Alice and Bob as publishers (the
  walk's intent is owner isolation, not role); `keys.rs:762-768`
  (`catalog_write_does_not_imply_publish_grant_on_role_defaults`): keep the
  stored assertion, add that the effective member set lacks `catalog:write`;
  every other test that writes the catalog with a member-role key (grep
  `UserRole::Member` next to `catalog:write` in `crates/vam-orchestrator/tests`).
  No test's assertion of product behaviour other than these changes; anything
  else that breaks is reported, not edited. **Accept:**
  `scripts/ci/local.sh --full` green.
  `Decide: NOW — part of MD-1.2, cost 0.`
- [ ] `MD-1.4` — **Queued start re-derives scopes.** `QueuedCaller::to_caller`
  (`inbox.rs:146-200`): for an API key, read the key row and use
  `effective_scopes(row, live role)`; for a session token, the live role's
  defaults; recompute `users_admin` from them. The stored queue-time scopes are
  no longer trusted. Files: `inbox.rs`, its tests (and `api/queue.rs` only if the
  call shape changes). **Accept:** unit tests: a message queued by a publisher
  key whose user was demoted starts with member scopes; one queued by an org
  admin session starts with `users_admin` only while the role holds it; revoked
  key and deactivated user still start nothing.
  `Decide: NOW — proof 3 (R5), cost 0, line launch proof.`
- [ ] `MD-1.5` — **Docs lines** (the full pass is Phase 4): in
  `docs/src/content/docs/orchestrator/user-isolation.md` the "Users and roles"
  sentence and the "API keys" row say a key's scopes follow the live role
  (default keys follow it, keys minted with a list are capped by it); same
  sentence in `getting-started/users-and-roles.md`; add the three scenarios to
  `proves:`. **Accept:** `cargo run -p vam-e2e -- check`; `pnpm build` in
  `docs/` if available.
  `Decide: NOW — docs say what ships, cost 0.`

**Waves** (model: Sonnet low by default, medium second, high rarely with a reason)

| Wave | Items | Turns green | Repo | Model | Budget | Depends on | Actions | Status | PR |
|---|---|---|---|---|---|---|---|---|---|
| 0 | `MD-1.0` (3 scenarios, 4 world pieces, `put-draft`, `set-role`, `identity-request` `contains`/`lacks`, `org` `scopes`, README) | — (3 red, right reason, class `product`) | vam | Sonnet low: test harness following the `identity.rs` pattern | 90 min | — | fast (`vam-e2e`, `e2e/`: `skip-vm-lane`) | [ ] | |
| 1 | `MD-1.1`, `MD-1.2`, `MD-1.3` | `promotion-widens-a-default-key-but-not-a-narrowed-one`, `demotion-takes-catalog-writes-from-a-publisher-key`, `member-key-minted-with-catalog-write-cannot-write-the-catalog` (and the re-run list stays green) | vam | Sonnet high: the authentication path of every API request plus a schema migration, a security boundary; the orchestrator reads `authenticate_api_key`, `effective_scopes` and `require_publish_grant` before merging | 150 min | 0 | fast (`vam-orchestrator`: `skip-vm-lane`) | [ ] | |
| 2 | `MD-1.4`, `MD-1.5` | — (unit; the three stay green) | vam | Sonnet medium: the queued-start re-check crosses `inbox.rs` and the run-start caller, plus docs | 75 min | 1 | fast (`skip-vm-lane`) | [ ] | |

**Open questions** (each with a default)

- Migration rule for existing keys (decision 2): `role` only when the stored set
  equals the owner's **current** role defaults. **Default: yes.** (Comparing
  with any role's set would widen a publisher's deliberately member-sized key.)
- Mint keeps an above-role explicit scope on the row and caps it at
  authentication, instead of refusing it at mint. **Default: keep and cap**;
  key listings show the stored list, `GET /v1/me` the effective one.
- Show `followsRole` (the mode) on key listings (proto field). **Default: no**
  in this phase; Phase 4 may add it if the app asks.
- Order of phases: the inline fix moved from the proposed Phase 3 to Phase 2,
  before member drafts, because it is exploitable today. **Default: yes.**

**Phase close:** unit suites on the merged base; release; team checklist out
(the next phase does not wait for it; playbook §6).

**Team checklist — Phase 1**

Setup: deploy vamd from the base branch over an existing node (the migration
runs at start). Have the bootstrap key, one org admin and a test member with a
password.

1. As the org admin, mint the member an API key with no scope list
   (`POST /v1/users/{id}/keys` `{"name":"t1"}`). **See:** `GET /v1/me` with it
   lists no `catalog:write`.
2. With that key, `PUT /v1/playbooks` a small draft. **See:** 403 naming
   `catalog:write`.
3. Promote the member (`POST /v1/users/{id}/publish-grant`). Repeat step 2 with
   the same key. **See:** 200; `GET /v1/me` now lists `catalog:write` and
   `publish:grant`.
4. Mint a second key for them with `scopes: ["runs:write","sessions:write"]`.
   **See:** `PUT /v1/playbooks` with it is 403.
5. Demote (`DELETE /v1/users/{id}/publish-grant`). **See:** the first key's
   `PUT /v1/playbooks` is 403 again; chat with a released agent still works.
6. Use the bootstrap key for `GET /v1/users`. **See:** 200, unchanged.

Report: "Phase 1 pass", or the step number with the error text.

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
