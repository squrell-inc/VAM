# Workstream: Member draft agents (members author their own agents from released parts)

```yaml
status: active
slug: member-draft-agents
current_phase: 3 (approved 2026-11-18, wave 0 next)
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
5. **The run route never writes the catalog.** *Amended 2026-11-18 by the
   product owner:* option B. `POST /v1/runs` refuses any `inlineAgent` or
   `inlinePlaybook` (any content, with or without a `resourceId`) with 400 and a
   message to save it first (`PUT /v1/agents`, `PUT /v1/playbooks`) and run it
   by `agentRef` / `playbookRef`; nothing is written. `vam run --file` saves the
   draft, shows any refusal, then runs the draft by ref (`-draft`). The
   draft-and-release writers (`put_*_with_owner`, `put_*_released_with_owner`)
   are test-only. (Was: an inline agent is saved as the caller's draft through
   the `PUT /v1/agents` function and the run pins it; an inline playbook is
   saved as a draft by a caller with `catalog:write`.)
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
   agent row is owned by a user whose live role does not author (Member)
   resolves in a `ReleasedOnly` mode: floating = latest release, `X.Y.Z` = that release or an
   error (no "kept as-is"), `X.Y.Z-draft` = error. Recording and session start
   use the same mode; `permissions.advanced` refs are recorded and loaded by pin.
   *Amended 2026-11-18 (follows decision 5, option B):* the clause "and an
   inline agent with no row run by such a caller" is dropped; no run carries an
   inline agent.
   *Amended 2026-11-18 by the product owner (Phase 3 open question 1):* a
   Member's explicit `-draft` ref (on a run, or on a run grant) to a playbook,
   skill or policy is refused even when the member owns that draft; a Member
   never runs draft parts.
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
| 2 | The run route never writes the catalog | 2026-11-18: #26 (plan, option B), #27 (scenarios), #35 (refusal, writers test-only, CLI and bench save-then-run, docs). 2 scenarios active and green, 1 guard. |

### Phase 3 — Members create, edit and run their own draft agents from released parts

**Goal:** A member saves, edits and chats with their own draft agents whose
skills, playbooks and policies are released; the save refuses anything else
with a message naming the ref; the run resolves released-only even for a former
publisher's old drafts; drafts stay private and in the member's namespace.  
**Depends on:** Phase 1, Phase 2  
**Plan:** approved by the owner 2026-11-18 (open-question defaults accepted)  
**Exit:** the five new scenarios below `active` and green, the re-run list
green; `catalog_read_authorize_tests::member_put_agent_is_403` replaced by the
owner-approved spec change (a member's PUT of an own draft in their namespace is
200, outside it 400).

- [x] Wave 0: scenarios and harness
- [x] Wave 1: `agents:draft`, restricted mode on the two routes, the save
  validator (refs, refused fields, namespace, cap), key backfill fix, test fallout
- [ ] Wave 2: `ResolveMode::ReleasedOnly` at record and at session start;
  advanced refs recorded and loaded by pin; explain and check routes use the mode

**Benchmark delta:** none moved; this phase is the launch proof "a member builds
their own agent" (`member-builds-and-runs-own-draft-agent-from-a-released-skill`).

**CI (owner, 2026-11-17):** fast local checks in the session
(`scripts/ci/local.sh --fast` and the wave's scenarios), the PR opens ready,
GitHub Actions is the gate. No wave touches a guest or VMM path (`skip-vm-lane`).

**What the code does today** (checked 2026-11-18 on
`feat/member-draft-agents-phase2-close`, Phases 1 and 2 merged; paths under
`crates/vam-orchestrator/src/`)

- **Scope check.** `ControlPlanePdp::authorize` (`api/auth.rs:80-96`) passes on
  `keys:admin` or the one named scope, read from the effective scopes Phase 1
  put on `auth.key.record.scopes` (`keys.rs:197-211` `effective_scopes`).
  `MEMBER_SCOPES` (`keys.rs:76-84`) has no catalog scope; there is no
  `agents:draft` anywhere.
- **`catalog:write` call sites (15).** Opened in restricted mode (decision 6):
  `PUT /v1/agents` (`api/mod.rs:572-591`, authorize `:579`) and
  `PUT /v1/agents/{urn}/permissions` (`api/permissions.rs:144-172`, authorize
  `:155`). **Stay closed (13):** migrate `api/permissions_migrate.rs:35`; delete
  (always refused) `api/mod.rs:675`; `PUT /v1/policies` `api/mod.rs:801`;
  `PUT /v1/playbooks` `:898`; `PUT /v1/skills` `:920`; `POST /v1/skills/packages`
  `api/skill_packages.rs:42` and its release `:185`; release of agents,
  playbooks, skills, policies `api/catalog_release.rs:43`, `:63`, `:85`, `:106`;
  hosted brokers list and get `api/mod.rs:1960`, `:1979`. The release suggestion
  (`api/release_suggestion.rs:35-47`) is a read gated by
  `require_release_authorization` (`publish:grant`), so it stays closed too.
  (Design §2.2 and §7 say "12"; the count is 15 sites, 13 closed.)
- **Save path.** `put_agent` → `publish_agent_draft_with_owner`
  (`catalog_publish.rs:1022-1058`): permission defaults
  (`apply_publish_permission_defaults` `:845`), `spec.validate()`, platform-URN
  refusal, then `upsert_spec_draft` (`:182-245`) under the store lock:
  installed-URN refusal, `owner::require_matching_urn_owner` (`owner.rs:612-646`,
  403 "only the artifact owner may version this agent"), upsert. No ref check, no
  namespace rule, no cap. The permissions PUT loads the caller's draft (foreign =
  404) and writes through `write_draft_permissions` (`catalog_publish.rs:911-931`)
  into the same function. Always allow / deny (`session_targets.rs:636-700`,
  `save_remembered_rule`) writes tool rules through `update_draft_permissions`
  with no scope check (unchanged; it adds no refs).
- **Namespace.** `vam_spec::default_namespace(username)` (`vam-spec/src/origin.rs:166-173`)
  is only reported by `GET /v1/me` (`api/me.rs:47`), with `registryDomain`.
- **Resolution.** `record_resolved_refs` (`catalog_resolve.rs:128-171`, called
  synchronously in `start_run`, `api/run_start.rs:436`, so a resolution error is
  the start's 400) pins `playbook_ref`, `skill_refs`, default/allowed playbooks,
  overlay skills, playbook `requires` and the four legacy policy refs; **not**
  `permissions.advanced`. `run_resolve_context` (`:180-202`) returns
  `(agent row owner, Draft)` for a `-draft` agent pin, else `(caller, Release)`.
  `resolve_inner` (`:561-647`): the "kept as-is" branch `:614-627` keeps a
  versioned ref with a digest the catalog does not hold, labelled Released; an
  explicit `-draft` resolves the owner's draft in both modes, and any owner's when
  `include_foreign_drafts` (`users:admin` caller). Floating refs take the owner's
  draft first in Draft mode (`resolve_skill_pin` `:676-683`,
  `resolve_table_pin` `:755-763`).
- **Session start.** Skill staging (`manager.rs:1232-1250`) takes the owner and
  mode from `run_resolve_context`; recorded skill pins replace `skill_refs` first
  (`overlay_resolved_refs` `catalog_resolve.rs:323-341`). A versioned pin is
  looked up without an owner (`skill_package_resolve.rs:115-145`). Legacy policy
  loads recorded pins (`policy_legacy.rs:315-331`). `load_advanced_policy`
  (`security.rs:398-478`) uses a recorded pin when there is one, else
  `get_policy_visible(urn, resolve_owner)` (`:464`), which can be the owner's
  draft. `SessionAgent` (`policy_legacy.rs:30-40`, built `:83-135`) carries
  `draft_run` and `resolve_owner` but no mode.
- **Explain and check.** `permissions_view.rs` `load_agent` (`:632-665`)
  hard-codes `ResolveMode::Draft` for a draft (`:660`), and `provenance_for`
  (`:932-937`) derives "owner draft run" from `mode == Draft`.
  `api/composition.rs:102` already calls `run_resolve_context`.
- **Scheduled and triggered runs.** The gateway builds the run with an
  unversioned `agent_ref` (`gateway.rs:858-868`); a draft-bound Space applies
  the `-draft` pin (`catalog_resolve.rs:66-117`), so `record_resolved_refs`
  decides the mode: no gateway change. Grant validation pins a `playbookRef` in
  `Release` mode for the grant owner (`grants.rs:348-357`).
- **Connectors.** Nothing on `AgentSpec` names a connector. A skill's
  `requiresConnector` is checked at tool-call time for the **session owner**
  (`connector_park.rs:60-110`: node enabled with consent, then the owner's own
  connection, else park); MCP connections are the session owner's visible rows
  while the node has the connector enabled (`mcp_registry.rs:1868-1900`). So a
  member "attaches a connector" by attaching a released skill that requires it,
  and their own draft run uses their own connection: no code change.
- **Visibility.** Drafts list and get only for their owner and `users:admin`
  (`api/mod.rs:649`, list `:775-789`); a foreign draft pin is 404
  (`catalog_resolve.rs:43-55`, `tests/workspace_pin_tests.rs:291-322`). A member
  pinning their own draft needs no code change.
- **Phase 1 backfill hazard (found here).** `backfill_scope_mode`
  (`keys.rs:761-813`, one-shot on the first start with the column) marks a key
  `role` only when its stored list equals the owner's role defaults **now**.
  Phase 1 is not on `main` yet (`origin/main...HEAD` = 0/8), so every existing
  node runs this backfill for the first time with Phase 3's sets: a member's
  default key minted before the upgrade (no `agents:draft`) would become
  `explicit` and never gain `agents:draft`, nor `catalog:write` on promotion.
  MD-3.1 fixes the comparison.

**Scenario delta** (design/scenarios.md §9; touched scenarios found from
`guards` on `api/mod.rs`, `api/permissions.rs`, `catalog_publish.rs`,
`catalog_resolve.rs`, `keys.rs`, `owner.rs`, `security.rs`,
`session_targets.rs`, `connectors.rs` and `given: org-*`)

| Scenario | Change | Layer | Given | Why |
|---|---|---|---|---|
| `member-builds-and-runs-own-draft-agent-from-a-released-skill` | new, `pending` | process | `org-admin`, `org-member` as `member`, `skill-host-echo` (new), `agent-member-draft-with-host-echo` (new), `playbook-one-step`, `playbook-draft-probe`, `skill-draft-probe` (new) | **the owner's symptom**: save 200, draft chat runs the released skill's tool, a released playbook runs on the draft; member still cannot author a playbook or skill or release |
| `member-draft-agent-refuses-a-skill-that-is-not-released` | new, `pending` | process | `org-admin` as `admin`, `org-publisher` as `pub`, `org-member` as `member`, `skill-draft-probe`, `agent-member-draft-with-draft-probe` (new) | decision 7 at save, R6: missing URN and someone's draft get the same 400; nothing written |
| `demoted-publishers-draft-agent-stops-using-their-draft-skill` | new, `pending` | process | `org-admin`, `org-publisher` as `pub`, `skill-draft-probe`, `agent-member-draft-with-draft-probe` | decision 7 at run, R6, R13: after demotion the same draft run is refused (no release to pin) and a re-save is 400 |
| `member-draft-agent-is-private-and-in-the-members-namespace` | new, `pending` | process | `org-admin` as `admin`, `org-member` as `m1` and as `m2`, `org-publisher` as `pub`, `skill-host-echo`, `agent-member-draft-with-host-echo`, `agent-inline-probe` | decisions 9, 10, R9, R10: own draft pin works; another member and a publisher get 404, the admin 200; outside the namespace and another member's URN are 400, nothing overwritten |
| `member-draft-agent-uses-a-connector-skill-through-the-members-connection` | new, `pending` | process | `org-member`, `org-administrator`, `mock-microsoft-login`, `mock-microsoft-graph`, `skill-m365-probe`, `agent-member-draft-with-m365-probe` (new) | owner intent "connectors enabled by the admin, connected by the member": the draft run parks until the member connects, then uses the member's connection |
| `member-chats-with-a-released-agent`, `always-allow-and-always-deny-are-remembered`, `released-agent-approvals-are-once-only`, `a-publisher-saves-a-draft-agent-and-runs-it-by-ref`, `promotion-widens-a-default-key-but-not-a-narrowed-one`, `demotion-takes-catalog-writes-from-a-publisher-key`, `member-key-minted-with-catalog-write-cannot-write-the-catalog`, `skill-needing-microsoft-365-asks-the-member-to-connect-first`, `trigger-starts-a-run-as-the-grant-owner`, `morning-triage-runs-on-schedule-as-the-member-who-approved-it`, `release-shows-the-diff-against-the-previous-version` | unchanged; re-run in W1 and W2 | process | as today | released agents, publisher draft runs, Always on a draft, key scopes, connectors and grant-started runs must not move |

No existing scenario's `then` changes: none asserts that a member's agent PUT is
403 (the Phase 1 probes write a **playbook** on purpose, and members keep 403
there), and `org-member-with-a-narrow-key` is an explicit list that stays
without `agents:draft`. The only spec change is the Rust test below.

Sketches (all red class `product`; the five guard `api/mod.rs`,
`api/permissions.rs`, `catalog_publish.rs`, `keys.rs`, plus
`catalog_resolve.rs`, `security.rs` and `permissions_view.rs` for the run
scenarios, `owner.rs` for the namespace one, `connector_park.rs` for the
connector one):

- `member-builds-and-runs-own-draft-agent-from-a-released-skill`. `when`:
  `put-draft: { piece: agent-member-draft-with-host-echo, as: member, status: 200 }`;
  `chat: { agent: agent-member-draft-with-host-echo, draft: true, as: member, text: "e2e-step:me Echo" }`;
  `expect: { run: succeeded }`; `expect: { thread: { role: tool, contains: host-echo-ok } }`;
  `start-playbook: { playbook: playbook-one-step, agent: agent-member-draft-with-host-echo, draft: true, as: member, label: mp, inputs: { topic: e2e } }`;
  `expect: { run: succeeded }`. `then`: `identity-request: { as: member, credential: key, path: /v1/me, status: 200, contains: "agents:draft", lacks: "catalog:write" }`;
  `put-draft: { piece: playbook-draft-probe, as: member, status: 403, contains: "catalog:write" }`;
  `publish-skill: { piece: skill-draft-probe, as: member, refused: true, contains: "catalog:write" }`;
  `identity-request: { as: member, credential: key, method: post, path: "/v1/agents/{{urn:agent-member-draft-with-host-echo:member}}/release", status: 403 }`.
  The agent piece's skill ref is **floating** (follows the latest release).
  **Red at `when.0`:** 403 "missing required scope 'catalog:write'" today.
- `member-draft-agent-refuses-a-skill-that-is-not-released`. `when`:
  `put-draft: { piece: agent-member-draft-with-draft-probe, as: member, status: 400, contains: "released" }`
  (the skill URN exists nowhere yet); `publish-skill: { piece: skill-draft-probe, as: pub, release: false }`;
  `put-draft: { piece: agent-member-draft-with-draft-probe, as: member, status: 400, contains: "released" }`
  (now a publisher's draft: same answer). `then`: `identity-request: { as: admin, credential: key, path: "/v1/agents/{{urn:agent-member-draft-with-draft-probe:member}}", status: 404 }`.
  **Red at `when.0`:** 403 today.
- `demoted-publishers-draft-agent-stops-using-their-draft-skill`. `when`:
  `publish-skill: { piece: skill-draft-probe, as: pub, release: false }`;
  `put-draft: { piece: agent-member-draft-with-draft-probe, as: pub, status: 200 }`;
  `chat: { agent: agent-member-draft-with-draft-probe, draft: true, as: pub, text: "e2e-step:pd Hello" }`;
  `expect: { run: succeeded }` (a publisher's draft run takes their draft skill);
  `set-role: { as: pub, role: member }`. `then`:
  `chat: { agent: agent-member-draft-with-draft-probe, draft: true, as: pub, text: "e2e-step:md Hello", refused: true }`;
  `expect: { refused: true, status: 400, contains: "released" }`;
  `put-draft: { piece: agent-member-draft-with-draft-probe, as: pub, status: 400, contains: "released" }`.
  **Red at `then.0`:** the start is 202 and the run uses the owner's draft today.
  `then.0` turns green in wave 2, `then.2` in wave 1.
- `member-draft-agent-is-private-and-in-the-members-namespace`. `when`:
  `put-draft: { piece: agent-member-draft-with-host-echo, as: m1, status: 200 }`;
  `chat: { agent: agent-member-draft-with-host-echo, draft: true, as: m1, text: "e2e-step:pin Hello" }`;
  `expect: { run: succeeded }`;
  `put-draft: { piece: agent-inline-probe, as: m1, status: 400, contains: "namespace" }`
  (its URN is under `local.vam.dev/e2e/`);
  `put-draft: { piece: agent-member-draft-with-host-echo, as: m2, urnOf: m1, status: 400, contains: "namespace" }`.
  `then`: `m2` GET `/v1/agents/{{urn:agent-member-draft-with-host-echo:m1}}` 404;
  `m2` GET `/v1/agents` 200 `lacks` that URN; `pub` GET it 404; `admin` GET it 200;
  `m1` GET `…/versions` 200, `lacks: "releasedAt"`, and `contains: "\"revision\":1"`
  (or the field wave 0 finds that shows m2 changed nothing). **Red at `when.0`.**
- `member-draft-agent-uses-a-connector-skill-through-the-members-connection`.
  `when`: `connector-node: { id: microsoft-365, enabled: true, consent: { tenant: 11111111-1111-4111-8111-111111111111 }, as: org-administrator }`;
  `put-draft: { piece: agent-member-draft-with-m365-probe, as: org-member, status: 200 }`;
  `chat: { agent: agent-member-draft-with-m365-probe, draft: true, as: org-member, text: "e2e-step:ask Search my mail", wait: false }`;
  `expect: { run: paused }`; `expect: { approval: pending, tool: m365_probe_search }`;
  `expect: { mock-requests: { piece: mock-microsoft-graph, path: "*", count: 0 } }`;
  `connector-connect: { id: microsoft-365, piece: mock-microsoft-login, tenant: 11111111-1111-4111-8111-111111111111, upn: member@e2e.example, as: org-member }`.
  `then`: `expect: { run: succeeded }`; `expect: { mock-requests: { piece: mock-microsoft-graph, path: /v1.0/me/messages, method: GET, count: 1, has: [bearer=current] } }`.
  Mock script copied from `skill-needing-microsoft-365-asks-the-member-to-connect-first`.
  **Red at `when.1`:** 403 today.

**Wave 0 harness additions** (`crates/vam-e2e`, documented in `e2e/README.md`
"Roles and drafts"; everything else reuses `put-draft`, `chat {agent, draft,
as, refused}`, `set-role`, `identity-request`, `connector-*`):

1. `{{ns}}` in the body of a piece that `put-draft` (or `chat inline`) writes:
   the writer's `defaultNamespace` from `GET /v1/me` (cached per identity; the
   bootstrap key's own when `as` is omitted), the value the frontend handoff
   uses. `check` refuses `{{ns}}` in a piece that is written at `given`.
2. `put-draft` takes `urnOf: <as>`: the body's URN is the one an earlier
   `put-draft` of the same piece as `<as>` wrote (class `scenario` if none); the
   step records it for its own identity too.
3. `publish-skill` takes `as` (publish and release as that identity; the
   bootstrap key when omitted, as today) and `release: false` (publish the
   package as a draft only). `refused` keeps its meaning.
4. `start-playbook: { agent: <deferred agent piece>, as }` runs the URN a
   `put-draft` as `as` wrote, the `chat` rule (`engine.rs:1004-1034` `agent_of`
   does not look up drafts today).
5. New world pieces: `skill-host-echo` (released at `given`; one tool
   `host_echo`, `host_exec` adapter `echo`, the tool of `skill-host-fixtures`
   without the secret), `skill-draft-probe` (`deferred`, same body);
   `agent-member-draft-with-host-echo`, `agent-member-draft-with-draft-probe`,
   `agent-member-draft-with-m365-probe` (each `deferred`, no `release`, URN
   `urn:vam:agent:local.vam.dev/{{ns}}/{{name}}`; floating skill ref to
   `skill-host-echo` / `skill-draft-probe`, pinned `1.0.0` ref to
   `skill-m365-probe`; `allowAnyPlaybook: true`; permissions as
   `agent-with-host-fixtures` / `agent-with-m365-probe`).

**Items** (each names the scenarios it turns green)

- [x] `MD-3.0` — Wave 0 above. Files: `e2e/scenarios/` (5 new),
  `e2e/world/` (5 new pieces),
  `crates/vam-e2e/src/{engine.rs,identity.rs,scenario.rs,check.rs}`,
  `e2e/README.md`. **Accept:** `cargo run -p vam-e2e -- check` clean;
  `scripts/e2e/run.sh --scenarios member-builds-and-runs-own-draft-agent-from-a-released-skill,member-draft-agent-refuses-a-skill-that-is-not-released,demoted-publishers-draft-agent-stops-using-their-draft-skill,member-draft-agent-is-private-and-in-the-members-namespace,member-draft-agent-uses-a-connector-skill-through-the-members-connection`
  red at the `expectFail` steps above, class `product` (the demotion one must
  pass its `when`, proving a publisher's draft run takes the draft skill);
  `--smoke` green; `vam-e2e` unit tests for `{{ns}}`, `urnOf`, `publish-skill`
  `as`/`release`, the `start-playbook` lookup. If the `host_echo` adapter is not
  offered to a member's run, swap the piece for another released-skill tool and
  say so in the PR (the scenario's behaviour does not change).
  `Decide: NOW — proof 3, cost 1, line launch proof "a member builds their own agent".`
- [x] `MD-3.1` — **`agents:draft` scope, and the backfill keeps default keys
  default.** `pub const AGENTS_DRAFT_SCOPE: &str = "agents:draft"` in
  `keys.rs`, added to `BOOTSTRAP_SCOPES`, `ORG_ADMIN_SCOPES`, `PUBLISHER_SCOPES`,
  `MEMBER_SCOPES` (`keys.rs:33-84`). `backfill_scope_mode` (`keys.rs:761-813`):
  a key is `role` when its stored set equals the owner's role defaults **with or
  without** `agents:draft` (the pre-Phase-3 sets), so nodes upgrading straight
  from `main` keep members' default keys following the role. The bootstrap row
  keeps its stored list (it holds `catalog:write`, so it is never restricted).
  **Accept:** unit tests: each role's defaults hold `agents:draft`; a member key
  stored with the old member set backfills to `role` and its effective scopes
  gain `agents:draft`; a publisher key stored with the old member set stays
  `explicit`; `GET /v1/me` with a member default key lists `agents:draft`
  (`tests/me_tests.rs`). `Decide: NOW — proof 3, cost 1.`
- [x] `MD-3.2` — **Restricted mode on the two routes.** One helper (e.g.
  `api/catalog_auth.rs` `require_agent_draft_write(auth) -> Result<AgentWriteMode,_>`):
  `keys:admin` or `catalog:write` → `Full` (today's path, unchanged);
  `agents:draft` → `Restricted`; neither → 403 "missing required scope
  'catalog:write' or 'agents:draft'". `put_agent` (`api/mod.rs:579`) and
  `put_draft_permissions` (`api/permissions.rs:155`) call it instead of
  `authorize(…"catalog:write")`; `Restricted` goes through MD-3.3. The 13 closed
  sites listed above are not touched. **Accept:** API tests
  (`tests/catalog_read_authorize_tests.rs` or a new
  `tests/member_draft_agents_tests.rs`): with a member key, each closed route
  answers 403 naming `catalog:write` (or `publish:grant` for release), a table
  test over all 13; a publisher key and the bootstrap key keep 200 on both open
  routes with no member rule applied (a publisher may still save a draft-skill
  ref and a URN outside their namespace); a narrowed key with neither scope is
  403 on both. Turns green (with MD-3.3, MD-3.4): `member-builds-and-runs-own-draft-agent-from-a-released-skill`,
  `member-draft-agent-uses-a-connector-skill-through-the-members-connection`.
  `Decide: NOW — proof 3 (catalog write authority), cost 1.`
- [x] `MD-3.3` — **The save validator (refs and refused fields).** New
  `member_draft.rs` (or in `catalog_publish.rs`):
  `validate_restricted_agent(store, spec) -> Result<(), OrchestratorError>`, run
  on the full spec that would be stored (after
  `apply_publish_permission_defaults`, and for the permissions PUT after the new
  permissions are applied). (a) Refused fields, 400 naming the JSON field:
  `capabilityPolicy`, `hitlPolicy`, `instructionArtifact`, `extends`, `overlay`,
  `completionPolicy.evaluatorRefs` (decision 8). (b) Every ref resolves through
  `resolve_ref_for_owner(…, ResolveMode::ReleasedOnly)`. This item adds the
  variant (`catalog_resolve.rs:206-214`) and its resolver rules, so save and run
  share one definition: floating = latest release only (no owner draft:
  `resolve_skill_pin` `:676-683`, `resolve_table_pin` `:755-763`); `X.Y.Z` =
  that release or error, the "kept as-is" branch (`:614-627`) is an error;
  `X.Y.Z-draft` = error "drafts are not available to this agent";
  `include_foreign_drafts` is ignored. Refs checked:
  `skillRefs`, `defaultPlaybookRef`, `allowedPlaybookRefs`,
  `permissions.advanced`, `telemetryPolicy.policyRef`,
  `completionPolicy.policyRef`. Any failure, whatever the cause (missing URN,
  foreign draft, own draft, explicit `-draft`, a version with no release, a
  digest mismatch) maps to one 400: "<kind> '<urn>' has no released version; an
  agent saved by a member may use released skills, playbooks and policies only"
  (no existence oracle). Files: the new module, `catalog_resolve.rs` (variant
  and rules, with resolver unit tests per rule), `api/mod.rs:572-591`,
  `api/permissions.rs:144-172`, `catalog_publish.rs:911-931` (the permissions
  writer takes the mode). **Accept:** unit tests for each refused field, each
  ref kind, and the five failure causes giving byte-identical messages; a
  released skill floating and pinned `X.Y.Z` passes; the permissions PUT of an
  own draft with a draft `advanced` policy is 400 and leaves the draft; Always
  allow on a member's own draft run still saves (`session_targets.rs` test).
  Turns green: `member-draft-agent-refuses-a-skill-that-is-not-released`, and
  `then.2` of `demoted-publishers-draft-agent-stops-using-their-draft-skill`.
  `Decide: NOW — proof 3 (R6, R12), cost 1.`
- [x] `MD-3.4` — **Namespace, ownership and cap, under the store lock.** A
  restricted writer next to `publish_agent_draft_with_owner`
  (`catalog_publish.rs:1022-1058`; `upsert_spec_draft` `:182-245`) that, holding
  the connection lock: if the caller owns any row of the URN → edit (skip
  namespace and cap; the owner check passes); otherwise the URN must be
  `urn:vam:agent:<registry domain>/<default_namespace(username)>/<name>`
  (`registry::registry_domains()`, the user's `username` from `users`) or 400
  "a new agent saved by a member must be under urn:vam:agent:<domain>/<ns>/
  (your namespace)"; then the owner check (a URN in the member's namespace
  owned by someone else stays 403 "only the artifact owner may version this
  agent"); then a new URN needs fewer than `VAMD_MEMBER_DRAFT_AGENTS_MAX`
  (default 50, read once at start like `daemon.rs:112` `VAMD_EGRESS_MAX_BYTES`)
  agent drafts owned by the caller, else 409 naming the limit. The namespace
  check comes before the owner check for a URN the caller does not own, so a
  foreign URN outside the namespace answers like a free one (no oracle). Files:
  `catalog_publish.rs`, `owner.rs` (a `caller_owns_any_row` helper beside
  `require_matching_urn_owner` `:612-646`), `daemon.rs` / `api/mod.rs`
  `AppState` for the limit. **Accept:** unit tests: new URN in namespace 200,
  outside 400, a username that is not a slug uses `default`; another member's
  URN 400 and unchanged; a URN in the member's namespace owned by a publisher
  403; editing an own draft outside the namespace (a former publisher's) 200
  when its refs are released; the cap counts drafts only and refuses the 51st
  new URN but not an edit; two concurrent creates at the limit leave at most the
  limit. Turns green: `member-draft-agent-is-private-and-in-the-members-namespace`.
  `Decide: NOW — proof 3 (R10, R11), cost 1.`
- [ ] `MD-3.5` — **`ResolveMode::ReleasedOnly` at record.** In
  `catalog_resolve.rs` (the variant and its rules landed in MD-3.3): `run_resolve_context`
  (`:180-202`) returns `(owner, ReleasedOnly)` for a `-draft` agent pin whose
  row owner's live role is Member (`users.role`; an owner with no users row is
  Member, fail closed), else `(owner, Draft)` as today. One helper
  (`draft_mode_for_owner(store, owner)`) shared with `permissions_view.rs`.
  `record_resolved_refs` (`:128-171`) also
  pins `permissions.advanced` refs (every mode; the same `resolve_ref_by_urn`).
  **Accept:** unit tests: a Member-owned draft run pins the latest release of a
  floating skill even when the owner has a draft; `X.Y.Z` with no release, a
  kept-as-is digest and `-draft` each fail, also for a `users:admin` caller
  running the member's draft; a publisher-owned draft run is unchanged; a
  demoted publisher's draft run flips to ReleasedOnly; advanced refs appear in
  `resolved_refs`; existing tests whose draft owner was created with
  `create_user` (a Member) promote that owner to Publisher, nothing else
  changes. Turns green: `then.0` of `demoted-publishers-draft-agent-stops-using-their-draft-skill`.
  `Decide: NOW — proof 3 (R6, R13), cost 1.`
- [ ] `MD-3.6` — **Session start, explain and check agree.**
  `SessionAgent` (`policy_legacy.rs:30-40`, `:83-135`) carries the mode from
  `run_resolve_context`; `load_advanced_policy` (`security.rs:398-478`) uses the
  recorded pin (now always present for a recorded run) and, with no pin in a
  non-Draft mode, loads the latest release only (`get_policy`, never
  `get_policy_visible`, `:464`); `resolve_one_skill_ref`
  (`skill_package_resolve.rs:55-145`) refuses a `-draft` pin in ReleasedOnly;
  `permissions_view.rs` `load_agent` (`:655-663`) uses `draft_mode_for_owner`
  for the draft's owner and `provenance_for` (`:932-937`) keys "owner draft
  run" on "a draft" (not `mode == Draft`), so Always stays offered on a
  member's own draft; `api/composition.rs:102` follows `run_resolve_context`
  with no edit (check it). **Accept:** unit tests: a member's draft with a
  floating advanced policy loads the recorded release at boot, never the
  owner's draft; a boot whose mode turned ReleasedOnly after the run was
  recorded with a draft pin fails closed; `GET …/permissions/effective` and
  `POST /v1/agents/check` for a member's draft show the released skill version
  the run pins; `owner_draft_run` stays true for a member's own draft.
  `tests/permissions_view_tests.rs`, `tests/agent_check_tests.rs`,
  `security.rs` tests. If a test asserts the "not pinned by digest" warning on a
  recorded run, that is the decision-7 spec change: one line in the PR.
  `Decide: NOW — proof 3 (R7, R17), cost 1.`
- [x] `MD-3.7` — **Rust test spec change and fallout (wave 1).**
  `tests/catalog_read_authorize_tests.rs:205-212` `member_put_agent_is_403`
  becomes `member_put_own_agent_draft_is_200_outside_namespace_is_400`: the
  member (`username` "member") PUTs `urn:vam:agent:local.vam.dev/member/member-write`
  → 200 and `…/acme/member-write` → 400 (spec change, owner-approved product
  change: decisions 6 and 9). `member_put_policy_is_403` and every other member
  403 stay. `tests/workspace_pin_tests.rs` gains `member_can_pin_own_draft` next
  to `member_cannot_pin_foreign_draft` (`:291-322`, unchanged). `keys.rs` unit
  tests on role sets gain `agents:draft`. Tests built on the scope constants
  (`me_tests.rs`, `key_scope_mode_tests.rs`, `bootstrap_principal_tests.rs`,
  `isolation_coverage_map_tests.rs:561`) follow the constants with no edit;
  anything else that breaks is reported, not edited. **Accept:**
  `scripts/ci/local.sh --fast`; the re-run list green.
  `Decide: NOW — part of MD-3.2, cost 0.`

**Waves** (owner's matrix: Sonnet 5.5 high or medium, or Haiku 5.5 high; as few
as the work allows. One build wave was weighed and rejected: the save path and
the run-time resolver each touch five or more files with their own test
fallout, and one context would not hold both plus the orchestrator's security
read.)

| Wave | Items | Turns green | Repo | Model | Budget | Depends on | Actions | Status | PR |
|---|---|---|---|---|---|---|---|---|---|
| 0 | `MD-3.0` (5 scenarios, 5 world pieces, 4 harness additions, README) | — (5 red, right reason, class `product`) | vam | Sonnet 5.5 medium: new step fields across four `vam-e2e` modules and proving each red at the right step | 90 min | — | gate: `rust.yml` fast jobs and smoke (`skip-vm-lane`) | [ ] | |
| 1 | `MD-3.1`, `MD-3.2`, `MD-3.3`, `MD-3.4`, `MD-3.7` | `member-builds-and-runs-own-draft-agent-from-a-released-skill`, `member-draft-agent-refuses-a-skill-that-is-not-released`, `member-draft-agent-is-private-and-in-the-members-namespace`, `member-draft-agent-uses-a-connector-skill-through-the-members-connection` (re-run list stays green) | vam | Sonnet 5.5 high: catalog write authority for a new class of user, a key-migration rule and a lock-held namespace and cap check (security boundary); the orchestrator reads the auth helper, the validator and the restricted writer before merging | 180 min | 0 | gate: full `rust.yml` (`skip-vm-lane`) | [ ] | |
| 2 | `MD-3.5`, `MD-3.6` | `demoted-publishers-draft-agent-stops-using-their-draft-skill` (the other four and the re-run list stay green) | vam | Sonnet 5.5 high: run-time resolution, session start and the explain routes must agree, and a mismatch is a policy bypass (security boundary); the orchestrator reads `run_resolve_context`, `resolve_inner` and `load_advanced_policy` | 150 min | 1 | gate: full `rust.yml` (`skip-vm-lane`) | [ ] | |

**Risks**

- **Mode drift between record and boot.** The mode is derived twice (run start
  and session start) from the live role; a promotion or demotion in between can
  change it. Boot then fails closed (a draft pin refused in ReleasedOnly) or
  takes recorded released pins (promotion). Accepted; no mode is stored on the
  run.
- **Test fallout in wave 2.** Unit tests create draft owners with
  `create_user` (role Member: `users.rs:216-222`); their draft runs flip to
  ReleasedOnly. The fix is to promote the owner in the fixture; a wave that
  edits an assertion instead is reported.
- **Backfill.** Without MD-3.1's comparison fix, the release that ships Phases
  1–3 to `main` freezes every member's existing default key without
  `agents:draft`.
- **Permission defaults (R8).** A member's draft gets open internet and
  side-effect tool rules like any agent (design §8 Q7, default same).

**Open questions** (each with a default)

1. **A member running a *released* agent with an explicit `-draft` ref** (a
   `playbookRef` on the run, or on a grant: `grants.rs:348-357`) resolves in
   `Release` mode, which still loads the caller's own draft, so a former
   publisher can attach their old draft playbook to a released agent's run.
   **Default: close it** — `run_resolve_context` returns ReleasedOnly also for
   a non-draft run whose caller's live role is Member, and grant validation uses
   the grant owner's mode; MD-3.5 takes one more branch. Taking it amends
   decision 7 by one clause ("and any run started by a Member"); the orchestrator
   edits the locked list first if approved.
2. The legacy single-user owner `local` has role Member (`users.rs:209`), so
   its draft runs become ReleasedOnly. **Default: yes** (follow the role
   strictly); tests that meant an author use the operator or a publisher.
3. Cap answer: **default 409** naming the limit and "edit one of your drafts".
4. Validator wording and status: **default 400** with one message per kind (no
   existence oracle); the scenarios check only `released` / `namespace`.
5. Record `permissions.advanced` pins in every mode, not only ReleasedOnly.
   **Default: every mode** (decision 7 says "recorded and loaded by pin"; one
   rule).

**Out of scope here:** docs and the frontend handoff (Phase 4), a member
publish-request flow, draft deletion, `extends` / `overlay` for members.

**Phase close:** unit suites on the merged base; team checklist out (the next
phase does not wait for it; playbook §6).

**Team checklist — Phase 3**

Setup: deploy vamd from the base branch over an existing node with at least one
member who has an API key minted before the upgrade; the bootstrap key; one
publisher; a released skill (with a tool) and a released playbook.

1. With the member's **old** key, `GET /v1/me`. **See:** `agents:draft` listed,
   no `catalog:write`.
2. As the member, `PUT /v1/agents` an agent at
   `urn:vam:agent:<registryDomain>/<defaultNamespace>/my-agent` (both from
   `GET /v1/me`) whose `skillRefs` names the released skill with no version.
   **See:** 200.
3. Chat with it pinned to the draft (`-draft`). **See:** the skill's tool runs.
   Start the released playbook on it. **See:** the run succeeds.
4. As the member, PUT an agent naming a skill that is only the publisher's
   draft. **See:** 400 naming the skill; the same text for a skill URN that does
   not exist.
5. As the member, PUT an agent at `…/someone-else/x`. **See:** 400 naming your
   namespace. `PUT /v1/playbooks`, `POST /v1/skills/packages` and
   `POST /v1/agents/<your urn>/release`. **See:** 403 each.
6. As another member, `GET /v1/agents/<first member's urn>`. **See:** 404. As the
   org admin. **See:** 200.
7. Promote a test user, save a draft skill and a draft agent that names it with
   no version, chat (works), demote them, chat again. **See:** the start is
   refused (400) naming the skill.

Report: "Phase 3 pass", or the step number with the error text.

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
| 2026-11-18 | 2 | Owner chose option B (the run route refuses inline agents and playbooks; `vam run --file` saves then runs by ref) and two waves. Decisions 5 and 7 amended; Phase 2 rewritten (3 scenarios: 2 pending, 1 guard; waves 0 and 1). Plan approved; wave 0 next. |
| 2026-11-18 | 2 | **Phase 2 closed.** Merged #27, #35 (plan #26). Scoreboard: 2 scenarios green, 1 guard green, 0 red; 5 fix cycles across both waves. Next: Phase 3 breakdown (members create, edit and run their own draft agents from released parts). |
| 2026-11-18 | 3 | Phase 3 breakdown drafted (5 new scenarios, waves 0–2); waiting for owner approval |
| 2026-11-18 | 3 | Owner approved the Phase 3 breakdown with the open-question defaults (close a Member's explicit `-draft` refs, decision 7 amended; `local` is released-only; cap answers 409; one 400 message per kind; advanced policy pins recorded in every mode). |
| 2026-11-18 | 3 W0 | Wave 0 on `feat/member-draft-agents-scenarios-member-drafts`: 5 scenarios `pending`, 5 world pieces, harness `{{ns}}`, `put-draft urnOf`, `publish-skill as`/`release: false`, `start-playbook` on a draft. Scoreboard: 0 green, 5 red (right reason, class `product`): four at the member's first save (403 `catalog:write`), the demotion one at `then.1` (the start is accepted and uses the draft skill). Publisher-swapped copies (deleted) confirmed the rest of each scenario passes. 1 fix cycle (playbook step tool on the agent piece), ~1h. Local smoke: the 9 Microsoft-mock sandbox failures as before. |
| 2026-11-18 | 3 W1 | Wave 1 on `feat/member-draft-agents-member-save`: `agents:draft` in every role default, backfill keeps pre-scope default keys `role`, `require_agent_draft_write` on `PUT /v1/agents` and the agent permissions PUT (13 other `catalog:write` routes unchanged, table test), restricted save (`member_draft.rs`, `publish_agent_draft_restricted`: refused fields, released-only refs through `ResolveMode::ReleasedOnly`, namespace, owner with empty owner as `local`, cap 409 under the store lock), release routes authorize before reading the body. Scoreboard: 3 scenarios green locally (builds-and-runs, refuses-unreleased, private-and-namespace), the connector one `active` and red only at the Microsoft-mock request count (sandbox, as its sibling), demotion still `pending` (red at `then.1`, wave 2); re-run list green; 1 fix cycle (release auth order), ~1h. |
