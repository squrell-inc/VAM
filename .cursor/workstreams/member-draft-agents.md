# Workstream: Member draft agents (members author their own agents from released parts)

```yaml
status: active
slug: member-draft-agents
current_phase: 2 (breakdown drafted 2026-11-17, waiting for owner approval)
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
**Plan:** drafted 2026-11-17, waiting for owner approval  
**Exit:** the three new scenarios below are `active` and green, the re-run list
stays green; `vam run --file <dir>` works for a publisher and runs their draft
(CLI tests and the team checklist; the `cli` scenario step has no host or key,
so it cannot prove this).

- [ ] Wave 0: scenarios and harness
- [ ] Wave 1: run route saves drafts through the PUT path; release writers
  test-only; test fallout
- [ ] Wave 2: `vam run --file` stops its silent pre-PUTs and shows vamd's
  refusal; docs lines

**Benchmark delta:** none. A security floor (design §6 R1), no benchmark line.

**CI (owner, 2026-11-17):** fast local checks in the session
(`scripts/ci/local.sh --fast` and the wave's scenarios), the PR opens ready,
GitHub Actions is the gate.

**What the code does today** (checked 2026-11-17 on the base, Phase 1 merged)

- `start_run` (`api/run_start.rs:216-601`): an inline agent with a
  `resourceId` calls `put_agent_with_owner` (`:345-351`; `catalog.rs:767-784`:
  draft **and** release at 1.0.0); an inline playbook with a `resourceId` calls
  `put_playbook_with_owner` (`:382-388`; `catalog.rs:951-970`). The only gate is
  `runs:write` (`api/mod.rs:1360`). The run uses the request body as its agent
  (`:345-352`, the saved row is dropped), and `pin_agent_ref`
  (`catalog_resolve.rs:400-458`) pins the agent to its **latest release** when
  the request names no version, so today's inline run only works because of
  the release.
- Production callers of `put_*_with_owner`, `put_*_released_with_owner`,
  `put_agent|playbook|skill|policy` and `put_platform_policy`: **only the two
  lines above.** Every other caller is test code: `#[cfg(test)]` modules in
  `catalog.rs`, `catalog_freeze.rs`, `catalog_publish.rs`, `catalog_resolve.rs`,
  `catalog_tier.rs`, `permissions_migrate.rs`, `playbook_activation.rs`,
  `policy_legacy.rs`, `run_context.rs`, `security.rs`, `api/mod.rs` (`:6043`,
  inside `mod hitl_resume_tests`), and **17 calls in 6 integration-test files**
  (`tests/api_tests.rs:122,195,4674,4675`, `hermetic_tests.rs:1725,3149,3152,3219,3222,3228`,
  `legacy_permissions_tests.rs:145,739,803`, `agent_check_tests.rs:198`,
  `playbook_examples_tests.rs:48`, `process_mode_run_api.rs:70`). Integration
  tests link the library without `cfg(test)`, so `#[cfg(test)]` alone cannot
  hide the writers (MD-2.3 uses a feature). Not callers: the marketplace import
  writes its released rows in its own transaction (`marketplace/import.rs`,
  `users:admin`), the release routes use `release_*_with_meta`
  (`api/catalog_release.rs:47-112`), vamd seeds nothing at boot
  (`put_platform_policy` says so, `catalog.rs:1140-1141`). Fate of each writer:
  test-only (MD-2.3); the two run-route lines are replaced (MD-2.2).
- Who sends an inline agent: `vam run --file <dir | run.json>` (CLI,
  `crates/vam-cli/src/cmd/run.rs:2080-2172` reads `agent.json` and
  `playbook.json` next to the run file; `start_run` at `:350-412` first PUTs
  both to `/v1/agents` and `/v1/playbooks` and **discards the answer**
  (`:357-374`), then posts the run with both inline). The examples' `run.json`
  files name the same URN in `agentRef` with `version 1.0.0` (e.g.
  `examples/jira-assistant/run.json`, `bun-assistant`), the quickstart and the
  Jira tutorial use them (`getting-started/quickstart.md:83`,
  `tutorials/jira-assistant.md:66`), and `scripts/bench-coldstart.sh:305-327`
  does the same with curl. The trigger gateway (`gateway.rs:831`) and the queue
  starter (`api/queue.rs:172`) never send inline parts; a message to a busy
  workspace with inline parts is refused 400 (`api/inbox.rs:471`). The e2e
  harness never sends one.
- **An inline agent without a `resourceId` does not work today, for anyone.**
  `POST /v1/runs` answers 202 and stores the run with an empty agent URN; session
  start then refuses it ("the run records no agent", `manager.rs:313-319`) and
  the run fails. No CLI flow, example or test relies on it (every inline body in
  the repo has a `resourceId`). An inline playbook without a `resourceId` is
  silently ignored (`run_context.rs:592-612`). See open question 2.

**Scenario delta** (design/scenarios.md §9; touched scenarios found from
`guards` on `catalog_publish.rs`, `api/mod.rs`, `api/catalog_auth.rs`,
`keys.rs` and `given: org-*`; no scenario guards `api/run_start.rs`,
`catalog.rs` of the orchestrator or `cmd/run.rs`)

| Scenario | Change | Layer | Given | Why |
|---|---|---|---|---|
| `inline-agent-run-saves-a-private-draft-and-releases-nothing` | new | process | `org-admin`, `org-publisher` as `pub`, `org-member` as `other`, `agent-inline-probe` (new) | exit: a publisher's inline run works and leaves a draft only; another user gets 404 |
| `member-cannot-publish-an-agent-through-a-run` | new | process | `org-admin`, `org-member` as `member`, `org-member` as `other`, `agent-inline-probe` | R1: refused, nothing written |
| `member-cannot-create-a-playbook-through-a-run` | new | process | `org-admin`, `org-member` as `member`, `org-member` as `other`, `agent-plain`, `playbook-draft-probe` | R1 for playbooks (members never author them) |
| `member-chats-with-a-released-agent` | unchanged; re-run in W1 | process | as today | a member's run by `agentRef` must not be touched |
| `always-allow-and-always-deny-are-remembered` | unchanged; re-run in W1 | process | as today | guards `catalog_publish.rs` (writers move); draft runs and Always saves keep working |
| `release-shows-the-diff-against-the-previous-version` | unchanged; re-run in W1 | process | as today | guards `catalog_publish.rs`; release route unchanged |
| `promotion-widens-a-default-key-but-not-a-narrowed-one`, `demotion-takes-catalog-writes-from-a-publisher-key`, `member-key-minted-with-catalog-write-cannot-write-the-catalog` | unchanged; re-run in W1 | process | as today | `PUT /v1/playbooks` now goes through the shared save; same status and message |

The three new scenarios guard `api/run_start.rs`, `api/catalog_save.rs` (new,
MD-2.1), `catalog.rs`, `catalog_publish.rs` and `catalog_resolve.rs`.

Sketch of each new scenario (all red class `product`):

- `inline-agent-run-saves-a-private-draft-and-releases-nothing`. `when`:
  `chat: { inline: agent-inline-probe, as: pub, text: "e2e-step:ip Hello" }`;
  `expect: { run: succeeded }`. `then`: `identity-request: { as: pub, credential:
  key, path: "/v1/agents/{{urn:agent-inline-probe}}/versions", status: 200,
  contains: "DRAFT", lacks: "RELEASED" }`; `identity-request: { as: other,
  credential: key, path: "/v1/agents/{{urn:agent-inline-probe}}", status: 404 }`;
  `identity-request: { as: other, credential: key, path: /v1/agents, status:
  200, lacks: "{{urn:agent-inline-probe}}" }`. Red at `then.0` (the run released
  the agent: `RELEASED` in the history). The run succeeding after the fix also
  proves the draft pin: without a release, today's floating pin fails.
- `member-cannot-publish-an-agent-through-a-run`. `when`: `chat: { inline:
  agent-inline-probe, as: member, text: "e2e-step:mi Hello", refused: true }`;
  `expect: { refused: true }`. `then`: `identity-request: { as: member,
  credential: key, path: "/v1/agents/{{urn:agent-inline-probe}}", status: 404 }`
  (not even the member's own draft); the same as `other`, 404. Red at `when.1`
  (202 and a finished run today). **No status and no `contains` on the
  refusal**, on purpose: in Phase 3 a member holds `agents:draft` and the same
  request is refused for another reason (the URN `local.vam.dev/e2e/...` is
  outside the member's namespace, decision 9: 400), and nothing is written
  either way, so the scenario holds unchanged through Phase 3. The Phase 2
  message (403, `catalog:write`) is pinned by a Rust API test instead
  (`run_inline_agent_from_a_member_is_403_and_writes_nothing`), which Phase 3
  wave 1 changes as an owner-approved spec change (see the note in Phase 3).
- `member-cannot-create-a-playbook-through-a-run`. `when`: `chat: { agent:
  agent-plain, inlinePlaybook: playbook-draft-probe, as: member, text:
  "e2e-step:mp Hello", refused: true }`; `expect: { refused: true, contains:
  "catalog:write" }`. `then`: `identity-request: { as: member, credential: key,
  path: "/v1/playbooks/{{urn:playbook-draft-probe}}", status: 404 }`; the same
  as `other`, 404; `chat: { agent: agent-plain, as: member, text: "e2e-step:mq
  Hello" }`; `expect: { run: succeeded }` (the member's plain chat is
  untouched). Red at `when.1`: today the playbook is released and the start
  answers 202, or 400 naming the playbook's `topic` input; never
  `catalog:write`. Wave 0 records which in the PR. This one holds for good
  (decision 6: playbooks stay on `catalog:write`).

**Wave 0 "scenarios"** writes the three scenario files `pending` with
`expectFail` at the steps above, the new world piece and these harness
additions in `crates/vam-e2e` (documented in `e2e/README.md`, "Roles and
drafts"):

- An `agent` piece with `deferred: true` and no `release` is not written at
  `given` (like a deferred playbook; the existing deferred agent pieces all
  have `release: true` and keep today's meaning, "released by
  `publish-skill`"). New piece `agent-inline-probe`: that, with the body of
  `agent-plain` (URN `urn:vam:agent:local.vam.dev/e2e/{{name}}`). Phase 3's
  "agent pieces deferred and written by `put-draft` as a member" reuses this.
- `chat` takes `inline: <agent piece>`: the piece's `spec` is sent as
  `inlineAgent`, with no `agentRef`; and `inlinePlaybook: <playbook piece>`:
  sent as `inlinePlaybook` (with `agent:` or `inline:`). Each URN's `{{name}}`
  is `<piece>-<as>-<scenario>-<runTag>`, the `put-draft` rule, so every
  identity writes its own URN. The run record's agent is the inline URN (so
  `expect: { runs }` and follow-ups find it).
- `{{urn:<piece>}}` in `identity-request` `path`, `contains` and `lacks`: the
  URN the latest `put-draft` or `chat inline | inlinePlaybook` step wrote (or
  tried to write) for that piece, path-encoded in `path`. None yet: class
  `scenario`.
- `check.rs` knows the new fields; `expect: { refused, contains }` already
  exists.

**Items** (each names the pending scenarios it turns green)

- [ ] `MD-2.0` — Wave 0 above. Files: `e2e/scenarios/` (3 new),
  `e2e/world/agent-inline-probe.yaml`, `crates/vam-e2e/src/{scenario.rs,engine.rs,identity.rs,world.rs,check.rs}`,
  `e2e/README.md`. **Accept:** `cargo run -p vam-e2e -- check` clean;
  `scripts/e2e/run.sh --scenarios
  inline-agent-run-saves-a-private-draft-and-releases-nothing,member-cannot-publish-an-agent-through-a-run,member-cannot-create-a-playbook-through-a-run`
  reports each red at its `expectFail` step, class `product`; `--smoke` green;
  `vam-e2e` unit tests for the URN expansion and the deferred-agent rule.
  `Decide: NOW — proof 3 (security floor R1), cost 1, line launch proof.`
- [ ] `MD-2.1` — **One save function per kind, shared by the PUT routes and the
  run route.** New `api/catalog_save.rs`: `save_agent_draft(store, scopes,
  owner_user_id, spec)` and `save_playbook_draft(..)`. Authority: `keys:admin`
  or `catalog:write` in the given (effective) scopes, refused with the PDP's
  exact error (`PermissionDenied("missing required scope 'catalog:write'")`,
  `api/auth.rs:80-97`); then `publish_agent_draft_with_owner`
  (`catalog_publish.rs:1018-1054`: permission defaults, validation, platform-URN
  refusal, owner check) or `publish_playbook_draft_with_owner`. `PUT
  /v1/agents` (`api/mod.rs:572-591`) and `PUT /v1/playbooks` (`:891-909`) call
  them with `auth.key.record.scopes` (effective since Phase 1). This is the
  function decision 5 names; Phase 3 adds restricted mode inside
  `save_agent_draft` only, and the run route inherits it. **Accept:** unit
  tests: `keys:admin` and `catalog:write` save, a member set is 403 with the
  same body as before, another user's URN is 403 "only the artifact owner";
  existing PUT tests and the Phase 1 scenarios unchanged.
  `Decide: NOW — proof 3, cost 1.`
- [ ] `MD-2.2` — **The run route saves drafts only and pins them.** In
  `start_run`:
  (a) **Before `resolve_place`** (`api/run_start.rs:256`), with the inline
  parts' URNs known: an inline agent with a `resourceId` needs the MD-2.1
  authority from `caller.scopes`; an inline playbook with a `resourceId` needs
  `catalog:write` (or `keys:admin`). Refused: 403, and no Space, thread, run or
  catalog row is created. An `agentRef` (or `playbookRef`) naming a different
  URN than the inline part: 400. Then `spec.agent_ref` becomes the inline URN at
  the draft slot (`{major: 1, prerelease: "draft"}`, `catalog.rs:43-45`),
  version and digest from the request dropped (open question 1), so the place
  kind (`:246-255`) is `Draft` and the run uses the agent's draft Space; the
  same for `spec.playbook_ref` (replaces `ensure_playbook_ref`,
  `run_context.rs:592-612`, for the inline case).
  (b) **After `inbox::admit` answers `Proceed`** (`:280-298`), before
  `bind_space_agent`: save through MD-2.1 (the owner check can still refuse:
  403, nothing in the catalog). The run's `agent` is the **saved** row (with
  permission defaults), not the request body, so the guest's inputs and the
  boot-time load agree. `:345-351` and `:382-388` go.
  (c) `record_resolved_refs` then pins the draft's digest (`pin_agent_ref`
  explicit-version arm, `catalog_resolve.rs:434-455`) and
  `run_resolve_context` (`:180-202`) gives `Draft` for the row's owner, the
  caller; session start loads the same pin (`manager.rs:1217-1228`).
  (d) Open question 2 (default taken): an inline agent or playbook with no
  `resourceId` is 400 naming `resourceId`, before anything is created.
  Files: `api/run_start.rs`, `api/catalog_save.rs`, `run_context.rs`.
  **Accept:** API tests in `tests/api_tests.rs`: publisher inline run → 202,
  draft row only, stored `agentRef` is `-draft` with the draft digest, a second
  user's `GET` is 404, `GET …/versions` has no release; a second inline run
  updates the same draft; `agentRef 1.0.0` + same-URN inline runs the draft;
  different URN → 400; `run_inline_agent_from_a_member_is_403_and_writes_nothing`
  (403 body names `catalog:write`; no agent row, no Space, no run); member
  inline playbook → 403 naming `catalog:write`, no playbook row; publisher
  inline playbook → draft only, run's `playbookRef` pinned `-draft`; inline
  without `resourceId` → 400; busy workspace with inline parts → 400 and no
  draft saved (extends `:3909`). Turns green the three scenarios.
  `Decide: NOW — proof 3 (R1), cost 2, line launch proof.`
- [ ] `MD-2.3` — **Release writers test-only.** `put_agent`,
  `put_agent_with_owner`, `put_playbook`, `put_playbook_with_owner`,
  `put_skill`, `put_skill_with_owner`, `put_policy`, `put_policy_with_owner`,
  `put_platform_policy` (`catalog.rs:763-784`, `:947-970`, `:1078-1101`,
  `:1115-1165`) and `put_agent|playbook|skill|policy_released_with_owner`
  (`catalog_publish.rs:1099`, `:1214`, `:1315`, `:1432`) behind
  `#[cfg(any(test, feature = "test-support"))]`. `crates/vam-orchestrator/Cargo.toml`:
  feature `test-support = []` (never in `default`) and the self
  dev-dependency `vam-orchestrator = { path = ".", features = ["test-support"] }`,
  the pattern of `brokers/vam-web/Cargo.toml:38-42`, so the 17 integration-test
  calls compile unchanged. **Accept:** `cargo check -p vam-orchestrator --lib
  --bins` builds without the feature (no production caller can exist);
  `scripts/ci/local.sh --fast`; the PR lists the grep of callers.
  `Decide: NOW — proof 3, cost 1.`
- [ ] `MD-2.4` — **Test fallout.** Tests that relied on the run route's
  release:
  - `tests/api_tests.rs` `idempotency_run_create_is_per_user` (`:1586`),
    `workspace_prefix_isolates_users_and_rejects_guest_override` (`:1655`),
    `spaces_are_created_once_per_key_and_renamed_in_place` (`:1790`): Alice and
    Bob are **members** (`two_member_keys`, `:1433-1460`) posting inline agents
    with a `resourceId`, i.e. they exercise R1. Their intent is per-user
    isolation, so mint them as publishers (a `two_publisher_keys` helper);
    the assertions do not change.
  - Fixtures with `agentRef 1.0.0` plus a same-URN inline agent
    (`create_run_body` `:2099-2125`, `sample_run_body` `:3261-3292`; their
    callers `:2051-2096`, `:2195-2530`, `:3374-4340`; `process_mode_run_api.rs:101-125`;
    `allowed_models_tests.rs:79-108`): they stay valid (the route pins the
    draft). Any assertion on the stored `agentRef` version `1.0.0`, a
    released-kind Space, or another user seeing the agent changes to the draft
    pin. The two `playbook_activation_offers_…` runs (`:2323-2385`) save the
    same URN twice with different bodies: expected to pass (same owner).
  - `hermetic_tests.rs:3176-3196` (inline, no `agentRef`): now pins the draft;
    the budget assertion is unchanged.
  - Anything else that breaks is reported, not edited.
  **Accept:** `scripts/ci/local.sh --fast`; the re-run scenario list green.
  `Decide: NOW — part of MD-2.2, cost 0.`
- [ ] `MD-2.5` — **`vam run --file` says what vamd says.** Remove the two
  pre-PUTs whose answers are discarded (`crates/vam-cli/src/cmd/run.rs:357-374`):
  the run route saves both drafts now, so they are a second write that hides
  the refusal. On a 4xx from `POST /v1/runs` for a request carrying inline
  parts, print vamd's message and one hint line ("`agent.json` and
  `playbook.json` are saved as your draft, which needs `catalog:write`; run a
  released agent with `vam run <agent> -p …`"). Exit code as today for an API
  error. **Accept:** `crates/vam-cli/tests/cli_tests.rs` (httpmock): a
  directory with `agent.json` posts exactly one `POST /v1/runs` with
  `inlineAgent` and no `PUT`; a 403 answer prints the message and the hint and
  exits non-zero; `--dry-run` unchanged. `cargo test -p vam-cli`.
  `Decide: NOW — the CLI must not hide R1's refusal, cost 0.`
- [ ] `MD-2.6` — **Docs lines** (the full pass is Phase 4):
  `orchestrator/session.md` (`POST /v1/runs`, near `:36`): an inline agent or
  playbook with a `resourceId` is saved as the caller's draft (`catalog:write`)
  and the run pins it; nothing is released; no `resourceId` is 400.
  `getting-started/cli.md` `--file` (`:178-189`): `agent.json` next to the run
  file updates your draft and runs it. `authoring/playbook-tutorial.md:71`
  ("needs the agent to be published first") made accurate.
  `docs/api/openapi.yaml:5479-5482`: descriptions on `inlineAgent` and
  `inlinePlaybook`. `proves:` on `orchestrator/user-isolation.md` gains the
  three scenarios. **Accept:** `cargo run -p vam-e2e -- check`; `pnpm build` in
  `docs/` if available.
  `Decide: NOW — docs say what ships, cost 0.`

**Waves** (owner's matrix: Sonnet 5.5 high or medium, or Haiku 5.5 high)

| Wave | Items | Turns green | Repo | Model | Budget | Depends on | Actions | Status | PR |
|---|---|---|---|---|---|---|---|---|---|
| 0 | `MD-2.0` (3 scenarios, 1 world piece, `chat inline / inlinePlaybook`, deferred agent rule, `{{urn:..}}`, README) | — (3 red, right reason, class `product`) | vam | Sonnet 5.5 medium: new step fields and a world-piece rule across five `vam-e2e` modules; Haiku high would follow the pattern but must also prove red for the right reason | 90 min | — | gate: `rust.yml` fast jobs and smoke; no VM path (`skip-vm-lane`) | [ ] | |
| 1 | `MD-2.1`, `MD-2.2`, `MD-2.3`, `MD-2.4` | `inline-agent-run-saves-a-private-draft-and-releases-nothing`, `member-cannot-publish-an-agent-through-a-run`, `member-cannot-create-a-playbook-through-a-run` (re-run list stays green) | vam | Sonnet 5.5 high: who may write the catalog through the run route, the ordering of refusal before any write, and the pin the boot loads (security boundary); the orchestrator reads `catalog_save.rs` and the `start_run` diff before merging | 150 min | 0 | gate: full `rust.yml`; no guest or VMM change, boot reads the same pin (`skip-vm-lane`) | [ ] | |
| 2 | `MD-2.5`, `MD-2.6` | — (CLI tests; the three stay green) | vam | Haiku 5.5 high: one CLI function with httpmock tests and doc lines, existing patterns | 60 min | 1 | gate: `rust.yml` fast jobs and `docs` | [ ] | |

**Open questions** (each with a default)

1. A request with both `agentRef` and an inline agent: **default: the inline
   part wins.** Same URN → the run pins the draft just saved (the caller's
   version and digest are ignored); a different URN → 400. Keeps every
   example `run.json` + `agent.json`, `bench-coldstart.sh` and the test
   fixtures working. Same rule for `playbookRef` and an inline playbook.
2. An inline agent with **no** `resourceId` fails at session start today
   (above). **Default: refuse it at `POST /v1/runs` with 400 naming
   `inlineAgent.resourceId`, before anything is created; an inline playbook
   with no `resourceId` (silently ignored today) likewise.** This amends
   design §4.2 ("unchanged"), and Phase 3's wave 2 item "inline agent without
   a row run by a member" then has nothing to cover (every inline agent has a
   row). Alternative: leave both as they are and keep that Phase 3 item.
3. Order: authority before place resolution (a 403 leaves no Space or
   thread); the save after admission (a busy refusal saves nothing).
   **Default: yes.**
4. Message for a member's inline playbook: **default:** "missing required scope
   'catalog:write': an inline playbook is saved as your draft; name a released
   playbook in playbookRef". The scenario checks only `catalog:write`.
5. **Default:** writers test-only through a `test-support` feature (vam-web
   pattern), not by rewriting 17 integration-test calls to draft + release.
6. A `vam run --file` rerun after the upgrade lands in the agent's **draft**
   Space (a default Space is per owner, agent and kind, `spaces.rs:245`), so a thread
   label used before starts fresh. **Default: accept**, one line in the
   `cli.md` change.
7. The quickstart and Jira tutorial use `vam run --file` with the example
   `agent.json`: a member following them now gets 403. **Default:** the docs
   lines say these need a publisher (or the bootstrap key); Phase 4 owns the
   member path.

**Out of scope here:** member draft agents (Phase 3), the ReleasedOnly mode
(Phase 3), any change to the release routes or the marketplace import.

**Phase close:** unit suites on the merged base; release; team checklist out
(the next phase does not wait for it; playbook §6).

**Team checklist — Phase 2**

Setup: deploy vamd from the base branch. Have a publisher and a member, each
with a CLI login or key, and one released agent the member can chat with. In
an empty directory `t/`, write `agent.json`: `{"resourceId":{"urn":"urn:vam:agent:local.vam.dev/team/inline-check"},"displayName":"Inline check","role":"Tester","systemInstructions":"Answer briefly.","allowAnyPlaybook":true}`.

1. As the publisher: `vam run --file t -p "hello" --follow`. **See:** the run
   starts and replies.
2. As the publisher: `vam agent versions urn:vam:agent:local.vam.dev/team/inline-check`.
   **See:** a draft only, no release.
3. As the member: `vam agent ls`. **See:** `inline-check` is not listed.
4. As the member: `vam run --file t -p "hello"`. **See:** an error naming
   `catalog:write`; no run starts; `vam agent ls` still does not list it.
5. As the member: `POST /v1/runs` with curl carrying an `inlinePlaybook`
   (any playbook body with a new URN). **See:** 403 naming `catalog:write`.
6. As the member: `vam run <released agent> -p "hello"`. **See:** it replies,
   as before.
7. As the publisher: `vam agent release urn:vam:agent:local.vam.dev/team/inline-check --note check`.
   **See:** released; the member now lists it.

Report: "Phase 2 pass", or the step number with the error text.

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
**From Phase 2 (breakdown 2026-11-17):** `member-cannot-publish-an-agent-through-a-run`
stays unchanged here (its URN is outside the member namespace, so the
request is still refused, 400 instead of 403, and nothing is written; the
scenario asserts no status). The Rust test
`run_inline_agent_from_a_member_is_403_and_writes_nothing` changes in wave 1
as an owner-approved spec change: outside the namespace 400, inside it the
member's private draft. If Phase 2 open question 2 is taken (inline agent
without `resourceId` is 400), wave 2's "inline agent without a row run by a
member" item is dropped.

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
| 2026-11-17 | 2 | Phase 2 breakdown drafted (3 new scenarios, 3 waves); waiting for owner approval |
