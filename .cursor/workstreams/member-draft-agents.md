# Workstream: Member draft agents (members author their own agents from released parts)

```yaml
status: active
slug: member-draft-agents
current_phase: 4 (breakdown drafted 2026-11-18, waiting for owner approval)
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
| 3 | Members create, edit and run their own draft agents from released parts | 2026-11-18: #38 (plan approved), #42 (scenarios), #45 (`agents:draft`, restricted save: refused fields, released-only refs, namespace, owner, 50-draft cap), #49 (`ReleasedOnly` at record, boot, explain and check; a Member never runs draft parts). 5 scenarios active and green in Actions; team checklist in git history. |

### Phase 4 — Docs and frontend handoff

**Goal:** The docs say what members, publishers and keys can do with draft
agents, and the frontend team has the handoff (`design/member-draft-agents.md`
§9) on the docs site, in one place, with vamd's exact messages.  
**Depends on:** Phase 3 (closed)  
**Plan:** breakdown drafted 2026-11-18, waiting for owner approval  
**Exit:** every stale mention in the table below fixed; the new page
`orchestrator/member-agents` in the sidebar; every page touched has `status:`
and `proves:` and has left `e2e/docs-baseline.txt` (29 → 24 lines);
`cargo run -p vam-e2e -- check` clean (features 104 → 107); `pnpm build` green;
the `docs` check green in Actions.

**What the docs say today** (checked 2026-11-18 on `feat/member-draft-agents`
at 2a08368; pages under `docs/src/content/docs/`)

| Where | Says today | Now true (proof) |
|---|---|---|
| `getting-started/users-and-roles.md:65-66` | publisher "Write catalog and release"; member "Run released agents … No catalog write." | a member saves and runs their own draft agents from released parts (`agents:draft`); skills, playbooks, policies, packages and release stay on `catalog:write` / `publish:grant` (`member-builds-and-runs-own-draft-agent-from-a-released-skill`) |
| `getting-started/users-and-roles.md:22`, `:137` | "Role and scope tables stay on Control plane security" (links `orchestrator/user-isolation`) | that page has no role or scope table; MD-4.2 adds it |
| `getting-started/users-and-roles.md:68` | keys follow the live role; "Releasing an agent needs `publish:grant` on the key" | right, but release also needs `catalog:write` and ownership (`api/catalog_release.rs:43-45`); no word on `agents:draft` |
| `orchestrator/user-isolation.md:43` | `GET /v1/me` returns "`userId`, `username`, `role` and the bearer's scopes" | it also returns `registryDomain`, `defaultNamespace`, `marketplaceDomain`; the scopes are the effective ones (`api/me.rs:23-52`) |
| `orchestrator/user-isolation.md:28-33` | `local` is the implicit owner; roles set scopes | `local` is a member, so its draft agents run on released parts only (Phase 3 open question 2) |
| `orchestrator/catalog.md:18-35` | routes with no scopes; "needs the owner and a live `publish:grant`" | agent PUT takes `catalog:write` or `agents:draft` (member rules); release takes `catalog:write`, `publish:grant` and ownership; members own draft agents |
| `orchestrator/session.md:41-52` | inline refusal (right); "a workspace pins its agent version" | add: a member may pin their own draft, a foreign draft is 404 (`tests/workspace_pin_tests.rs::member_can_pin_own_draft`); a member's draft run uses released parts only |
| `security/permissions.md:44-54` | `permissions set` replaces the owner's draft | a member may set permissions on their own draft; `advanced` refs must be released; `capabilityPolicy` / `hitlPolicy` are refused for members; member drafts get the same defaults as any agent (design §8 Q7) |
| `orchestrator/api.md:41`, `:89-93` | Catalog group with no scope note; 409 is "conflict" | link the member rules; 409 also answers the member draft cap |
| `orchestrator/overview.md:77-87` | settings table | no `VAMD_MEMBER_DRAFT_AGENTS_MAX` row (`bin/vamd.rs:272-276`, default 50, a non-positive or non-number value keeps 50) |
| `authoring/model.md:50` | namespace "is a default, not a rule: you may still choose another namespace" | for a member's **new** agent it is the rule (400 outside it); an edit of an own draft skips it |
| `authoring/model.md:111`, `authoring/agent-spec.md:135`, `:225`, `:276`, `authoring/run-spec.md:94`, `authoring/playbook-spec.md:223` | "Draft run: a floating ref uses the agent owner's draft when one exists" | only when the agent row's owner authors (publisher and up, the bootstrap key) by live role; a member-owned draft run uses the latest release, an `X.Y.Z` with no release and any `-draft` ref are refused (`demoted-publishers-draft-agent-stops-using-their-draft-skill`, `catalog_resolve.rs::member_owned_draft_run_pins_the_latest_release_not_the_owners_draft`) |
| `authoring/run-spec.md:85-97` | `agentRef` reference; no word on inline parts | `inlineAgent` / `inlinePlaybook` are refused with 400; save, then run by `agentRef` at `1.0.0-draft` |
| `getting-started/cli.md:189` | `vam run --file` saving `agent.json` "needs `catalog:write`: a publisher or the bootstrap key" | `agent.json` needs `agents:draft` (member rules: namespace, released parts) or `catalog:write`; `playbook.json` needs `catalog:write`; the examples' URNs (`local.vam.dev/vam/…`) are outside a member's namespace |
| `getting-started/cli.md:428` | "Empty scope lists use the owner's role defaults" | an empty list follows the live role; a list is capped by it |
| `docs/api/openapi.yaml:3923-3926` (`MeResponse.scopes`) | "session role defaults, or the API key's minted set" | the effective scopes (live role) |
| `docs/api/openapi.yaml:4094` (`CreateApiKeyRequest.scopes`) | "Empty = owner's role defaults" | empty follows the live role; a list is capped by it |
| `docs/api/openapi.yaml:1431-1465` (`PUT /v1/agents`) | no auth note, no 400/403/409 | `catalog:write` or `agents:draft`; member rules; 400, 403, 409 |
| `examples/bun-assistant/README.md:48` | `vam run --file` "(inline agent is loaded automatically)" | `--file` saves `agent.json` as your draft, then runs it by ref |
| `crates/vam-cli/src/cmd/run.rs:416` (code, not docs) | the save-refused hint: "`{file}` is saved as your draft, which needs catalog:write" for both files | `agent.json`: `agents:draft` (your namespace, released parts) or `catalog:write`; `playbook.json`: `catalog:write` (open question 2) |

**Stale in `design/member-draft-agents.md`** (not edited: the design doc is
retired at the workstream close, its user-facing facts and a short "Design
notes" move into MD-4.1): the header "proposal for owner approval"; §4.2
bullets saving inline parts as drafts (amendment note above them); §4.3 "the
inline agent of Phase 2" as a validator caller; §4.4 "an inline agent with no
row"; §5 last sentence; §6 R1 ("draft only through the PUT path", now refused)
and R8 ("members can already do this today with an inline agent", no longer
possible); §7 "12 of them" (15 sites, 13 closed, 2 open); §8 Q9 (403 for an
inline agent; option B made every inline part a 400); §9 lacks save-then-run,
the 409 cap and the Member `-draft` refusal (amended decision 7). No docs page
says "members cannot write the catalog" outside `users-and-roles.md:66`, and no
docs page shows an inline agent on `POST /v1/runs`.

**Scenario delta:** none. Docs only: no behaviour changes, so no scenario is
added, changed or retired, and there is no wave 0. Pages name existing `active`
scenarios in `proves:`: the five member-draft ones
(`member-builds-and-runs-own-draft-agent-from-a-released-skill`,
`member-draft-agent-refuses-a-skill-that-is-not-released`,
`demoted-publishers-draft-agent-stops-using-their-draft-skill`,
`member-draft-agent-is-private-and-in-the-members-namespace`,
`member-draft-agent-uses-a-connector-skill-through-the-members-connection`),
Phase 1's (`promotion-widens-a-default-key-but-not-a-narrowed-one`,
`demotion-takes-catalog-writes-from-a-publisher-key`,
`member-key-minted-with-catalog-write-cannot-write-the-catalog`) and Phase 2's
(`a-run-with-an-inline-agent-is-refused-and-writes-nothing`,
`a-run-with-an-inline-playbook-is-refused-and-writes-nothing`,
`a-publisher-saves-a-draft-agent-and-runs-it-by-ref`), plus the tests below.
**Benchmark delta:** none.

**Proofs to cite** (all resolve today; `path::fn` has a `#[test]`):
`crates/vam-orchestrator/tests/member_draft_agents_tests.rs`,
`crates/vam-orchestrator/tests/catalog_read_authorize_tests.rs::member_put_own_agent_draft_is_200_outside_namespace_is_400`,
`crates/vam-orchestrator/tests/workspace_pin_tests.rs::member_can_pin_own_draft`,
`crates/vam-orchestrator/tests/key_scope_mode_tests.rs`,
`crates/vam-orchestrator/tests/me_tests.rs`,
`crates/vam-orchestrator/tests/hsp_grants.rs::a_members_grant_naming_a_draft_playbook_is_refused_and_a_publishers_is_not`,
`crates/vam-orchestrator/src/member_draft.rs::{each_refused_field_is_a_400_naming_it, namespace_new_inside_ok_outside_400_non_slug_username_uses_default, a_urn_in_my_namespace_owned_by_a_publisher_is_403, the_cap_counts_drafts_only_and_refuses_a_new_row_not_an_edit, cap_parses_a_positive_number_else_the_default}`,
`crates/vam-orchestrator/src/catalog_resolve.rs::{member_owned_draft_run_pins_the_latest_release_not_the_owners_draft, a_members_explicit_draft_playbook_is_refused_even_on_their_own_draft, released_only_refuses_an_explicit_draft_ref_even_for_the_owner_and_an_operator}`,
`crates/vam-orchestrator/src/api/composition.rs::explain_check_and_run_agree_before_and_after_a_demotion`,
`crates/vam-orchestrator/src/permissions_view.rs::a_members_own_draft_is_released_only_and_stays_an_owner_draft_run`
(one `proves:` entry per function; the brace lists are shorthand here).

**Items**

- [ ] `MD-4.1` — **New page `orchestrator/member-agents.md` ("Member agents"),
  the frontend handoff in it.** Sidebar: `docs/astro.config.mjs:151`, one
  entry `{ label: 'Member agents', slug: 'orchestrator/member-agents' }` after
  Catalog. Frontmatter `status: shipped`, `proves:` the 11 scenarios above and
  the tests the page cites. Sections, each security sentence naming its proof:
  1. **What a member can do.** Save and edit own draft agents, attach released
     skills, playbooks and policies, chat on the draft, start a released
     playbook on it, use a connector skill through their own connection
     (admin enables, member connects). Cannot: author skills, playbooks,
     policies or packages, release, see or attach anyone's draft, overwrite
     another user's row.
  2. **Scopes.** `agents:draft` is in the member, publisher, org admin and
     bootstrap defaults (`keys.rs:41-95`). Member rules apply when the caller
     holds `agents:draft` and not `catalog:write` (so also a publisher key
     narrowed to it: `member_draft_agents_tests.rs`). Role-mode keys (minted
     with no list) get the live role's defaults; explicit keys get their list
     capped by the live role; the bootstrap key and Operator keys keep their
     list; `GET /v1/me` shows the effective set; release needs `catalog:write`,
     `publish:grant` and ownership.
  3. **Saving** (`PUT /v1/agents`, `PUT /v1/agents/{urn}/permissions`): refused
     fields (`capabilityPolicy`, `hitlPolicy`, `instructionArtifact`, `extends`,
     `overlay`, `completionPolicy.evaluatorRefs`); every ref in `skillRefs`,
     `defaultPlaybookRef`, `allowedPlaybookRefs`, `permissions.advanced`,
     `telemetryPolicy.policyRef`, `completionPolicy.policyRef` must resolve to
     a release, one message for a missing URN, a foreign draft, an own draft,
     `-draft` and a version with no release; a new URN under
     `urn:vam:agent:<registryDomain>/<defaultNamespace>/`; the owner check;
     at most `VAMD_MEMBER_DRAFT_AGENTS_MAX` (default 50) agent drafts, a new
     URN past it is 409, an edit is not. Always allow / deny on an own draft run
     still saves tool rules.
  4. **Message table** (status, exact `message`, cause), copied from the code:
     400 `agent field '<field>' is not available to an agent saved by a member; remove it`
     (`member_draft.rs:142-149`); 400 `<kind> '<urn>' has no released version; an agent saved by a member may use released skills, playbooks and policies only`
     (`catalog_resolve.rs:282-288`); 400 `a new agent saved by a member must be under urn:vam:agent:<domain>/<ns>/ (your namespace); got '<urn>'`
     (`member_draft.rs:169-175`); 403 `only the artifact owner may version this agent`
     (`owner.rs:642`); 403 `missing required scope 'catalog:write' or 'agents:draft'`
     (`api/catalog_auth.rs:53-55`); 403 `missing required scope 'catalog:write'`
     (playbook, skill, policy, package and release routes) and
     `missing required scope 'publish:grant'` (`api/catalog_auth.rs:75`);
     409 `a member may keep at most <n> draft agents; edit one of your drafts`
     (`member_draft.rs:196-199`); at run start 400 `<kind> '<urn>': drafts are not available to this agent`
     (`catalog_resolve.rs:678`, `:772`) or the "has no released version" one;
     400 `inlineAgent is not accepted: …` / `inlinePlaybook is not accepted: …`
     (`api/run_start.rs:216-232`).
  5. **Running it.** Save, then `POST /v1/runs` with `agentRef` at
     `{major: 1, prerelease: "draft"}`; inline parts are 400; chat on the draft
     by pinning the workspace to it. The run resolves released parts only,
     keyed on the agent row's owner's live role (so an org admin running a
     member's draft gets the same pins): floating = latest release, `X.Y.Z` =
     that release or 400, `-draft` = 400. A Member's explicit `-draft` ref on a
     run or a run grant is refused even for their own draft
     (`hsp_grants.rs::…`, `catalog_resolve.rs::a_members_explicit_draft_playbook_…`).
     A demoted publisher's draft agents become member drafts: the next start
     is 400 naming the ref and a re-save is 400. `GET /v1/agents/{urn}/permissions/effective`,
     `…/permissions/explain` and `POST /v1/agent-checks` resolve the same
     versions the run pins (`composition.rs::explain_check_and_run_agree_…`,
     `permissions_view.rs::a_members_own_draft_…`).
  6. **Visibility.** Drafts list and read for their owner and `users:admin`
     only; another member and a publisher get 404.
  7. **Known limitation: shared namespaces.** Usernames that slug to the same
     namespace (`Alice` and `alice`, both allowed: usernames are
     case-sensitive), and usernames that are not slugs (a leading or trailing
     `.`, `-` or `_`), which all fall back to `default`, share one namespace.
     The owner check still blocks an overwrite (403), but 200 against 403
     tells a member that a name is taken. Not fixed; `user-identity` Phase 2
     (email usernames) touches the same function (`design/user-identity.md`
     line 242).
  8. **Building a client** (the handoff, design §9 plus Phase 2 and 3): (a)
     show vamd's `message` verbatim on 400, 403 and 409; it names the ref,
     field, namespace or scope; (b) decide controls from `GET /v1/me` `scopes`,
     not `role`: "Create agent" and edit from `agents:draft`, skill, playbook
     and policy authoring from `catalog:write`, "Release" from `catalog:write`
     plus `publish:grant`; re-read `/v1/me` after a role change or a 403;
     (c) a new agent URN is `urn:vam:agent:<registryDomain>/<defaultNamespace>/<name>`
     from `/v1/me`; (d) attach skills, playbooks and policies only by ref
     (`skillRefs`, `defaultPlaybookRef`, `allowedPlaybookRefs`,
     `permissions.advanced`) to released versions: pickers offer rows whose
     `channel` is released, write the URN only (floating) or `X.Y.Z`, never
     `-draft`; (e) never re-PUT a skill, playbook or policy to attach it (a
     member gets 403, a publisher "only the artifact owner may version this
     skill"); (f) hide the refused fields from members; (g) save with
     `PUT /v1/agents`, then run by `agentRef` at `1.0.0-draft`; never send
     `inlineAgent` or `inlinePlaybook`; (h) on 409 say the member reached the
     cap and offer to edit a draft (there is no delete); (i) preview with the
     effective, explain and check routes; (j) every write carries an
     `Idempotency-Key`.
  9. **Design notes** (short, so the design doc can be deleted): a scope, not
     a role check, so key narrowing holds; released-only keyed on the agent
     row's owner, not the caller; one message per ref kind, no existence
     oracle; namespace before owner check, so a foreign URN outside the
     namespace answers like a free one; rejected options (design §7) in one
     line each.
  10. **Not proved here** (anything only read from code).
  Commands with output only if run against a process-mode node (style rule);
  otherwise request bodies and the message table, no pasted answers.
  **Accept:** `vam-e2e check` clean, `pnpm build` green, the page in the
  sidebar. `Decide: NOW — proof 3 (launch proof "a member builds their own agent" needs its client), cost 1.`
- [ ] `MD-4.2` — **Roles, scopes and keys.** `getting-started/users-and-roles.md`:
  profile rows for publisher and member (`:65-66`), the key paragraph (`:68`:
  `agents:draft`, role-mode vs explicit keys with one example each, release
  needs `catalog:write` + `publish:grant` + ownership), the two "Control plane
  security" links (`:22`, `:137`) point at the new table, a link to
  `orchestrator/member-agents`. `orchestrator/user-isolation.md`: a role →
  default scopes table from `keys.rs:41-95` (operator, org admin, publisher,
  member), the `GET /v1/me` fields (`:43`), `local` is a member so its drafts
  run on released parts, the bootstrap key always authors, a link. Both keep
  their `proves:` and add `member-builds-and-runs-own-draft-agent-from-a-released-skill`,
  `crates/vam-orchestrator/tests/key_scope_mode_tests.rs`.
  **Accept:** `vam-e2e check` clean. `Decide: NOW — proof 2, cost 0.`
- [ ] `MD-4.3` — **Catalog, sessions, permissions, API, daemon.**
  `orchestrator/catalog.md` (routes table gains a "Who writes" column; the
  drafts paragraph says members own draft agents; release needs
  `catalog:write`, `publish:grant`, ownership; link), `orchestrator/session.md`
  (member pins own draft, foreign 404; member draft runs on released parts;
  link), `security/permissions.md` (member permissions on own draft,
  `advanced` released only, legacy fields refused, same defaults),
  `orchestrator/api.md` (Catalog row and Errors 409 note, link),
  `orchestrator/overview.md` (the `VAMD_MEMBER_DRAFT_AGENTS_MAX` row). Each
  adds the proofs it cites to `proves:` (e.g. `workspace_pin_tests.rs::member_can_pin_own_draft`,
  `member_draft.rs::cap_parses_a_positive_number_else_the_default`,
  `member-draft-agent-is-private-and-in-the-members-namespace`).
  **Accept:** `vam-e2e check` clean. `Decide: NOW — proof 2, cost 0.`
- [ ] `MD-4.4` — **Authoring and CLI pages; each leaves the docs baseline.**
  `authoring/model.md` (`:50` namespace, `:111` resolution),
  `authoring/agent-spec.md` (`:135`, `:225`, `:276`; a member note under
  "Registering an Agent", `:357-375`), `authoring/run-spec.md` (`:94`; inline
  refusal and an `agentRef` `1.0.0-draft` example in §1),
  `authoring/playbook-spec.md` (`:223`), `getting-started/cli.md` (`:189`,
  `:428`; one line in §5: a member may `vam agent put` and
  `vam agent permissions set` an agent in their namespace). The five pages
  have no frontmatter `status` today (their only failure); each gets
  `status: shipped` and a `proves:` naming at least one proof of every
  `e2e/docs-features.yaml` row that lists it, plus the member proofs it cites,
  and its line in `e2e/docs-baseline.txt` is deleted. Minimum per page (from
  the register): **agent-spec** `crates/vam-orchestrator/tests/api_tests.rs`,
  `always-allow-and-always-deny-are-remembered`,
  `crates/vam-security/src/permissions.rs::default_deny_internet_allows_only_listed_hosts`;
  **model** `crates/vam-orchestrator/tests/api_tests.rs`; **run-spec**
  `plain-chat-gets-a-reply`; **playbook-spec**
  `crates/vam-cli/tests/playbook_put_cli.rs`, `matching-request-starts-the-playbook`,
  `gate-decided-finishes-the-run`; **cli** `node-reports-the-isolation-it-runs`,
  `crates/vam-orchestrator/tests/api_tests.rs`,
  `crates/vam-orchestrator/tests/login_tests.rs`,
  `member-changes-own-password-and-signs-in-with-the-new-one`,
  `crates/vam-orchestrator/src/paging_store_tests.rs::runs_page_newest_first_by_rowid_cursor`,
  `crates/vam-cli/tests/cli_tests.rs`, `crates/vam-cli/tests/agent_check_cli.rs`,
  `plain-chat-gets-a-reply`, `crates/vam-orchestrator/tests/evidence_pack_test.rs`,
  `crates/vam-orchestrator/tests/workspace_files_tests.rs`,
  `crates/vam-orchestrator/tests/trajectory_replay_test.rs`, and
  `a-publisher-saves-a-draft-agent-and-runs-it-by-ref` (MD-4.6 lists cli as a
  dev page). Checked: with only the five baseline lines removed, `check` fails
  on the missing `status` and on these rows, nothing else. The wave reads each
  page whole; anything wrong outside this workstream is one sentence fixed or
  listed in the PR for the orchestrator, not rewritten (open question 1).
  **Accept:** `vam-e2e check` clean with 24 baseline lines.
  `Decide: NOW — proof 2 (stale security-relevant claims), cost 1.`
- [ ] `MD-4.5` — **OpenAPI descriptions** (`docs/api/openapi.yaml`, text only,
  no new path or schema): `PUT /v1/agents` (`:1431-1465`) says who may call
  it, the member rules in one paragraph with a link, and adds 400, 403 and 409
  responses (`ApiError`); `MeResponse.scopes` (`:3923-3926`) and
  `CreateApiKeyRequest.scopes` (`:4094`) say the scopes follow the live role.
  **Accept:** `cargo test -p vam-spec --test openapi_yaml`.
  `Decide: NOW — proof 2 (the frontend reads it), cost 0.`
- [ ] `MD-4.6` — **Feature register.** Three rows in `e2e/docs-features.yaml`
  (rule: a feature gets its row when its lane reaches the base):
  `member-draft-agents` (area "Agents and specs"; surface `agents:draft`,
  `PUT /v1/agents` and `PUT /v1/agents/{urn}/permissions` as a member,
  `VAMD_MEMBER_DRAFT_AGENTS_MAX`; dev and arch `orchestrator/member-agents`,
  arch also `orchestrator/catalog`; proves the five member-draft scenarios and
  `tests/member_draft_agents_tests.rs`); `api-keys-follow-the-live-role`
  ("Auth and users"; dev `getting-started/users-and-roles`, arch
  `orchestrator/user-isolation`; proves the three Phase 1 scenarios and
  `tests/key_scope_mode_tests.rs`); `run-route-refuses-inline-agents-and-playbooks`
  ("Agents and specs"; dev `getting-started/cli`, arch `orchestrator/session`;
  proves the three Phase 2 scenarios). **Accept:** `check` prints
  `features: 107`. `Decide: NOW — proof 1, cost 0.`
- [ ] `MD-4.7` — **Stale mentions outside the site.**
  `examples/bun-assistant/README.md:48`; and, if open question 2 is taken, the
  hint in `crates/vam-cli/src/cmd/run.rs:412-422` per file (`agent.json`:
  "needs agents:draft (an agent in your namespace from released parts) or
  catalog:write"; `playbook.json`: "needs catalog:write") with one unit test.
  **Accept:** `cargo test -p vam-cli`; `scripts/ci/local.sh --fast` when the
  hint changes. `Decide: NOW — proof 1, cost 0.`

**Waves** (owner prefers as few as possible: one build wave, no wave 0)

| Wave | Items | Turns green | Repo | Model | Budget | Depends on | Actions | Status | PR |
|---|---|---|---|---|---|---|---|---|---|
| 1 | `MD-4.1` … `MD-4.7` | — (no scenario delta; `vam-e2e check` clean with 24 baseline lines and 107 features, `pnpm build` green) | vam | Sonnet 5.5 medium: about 15 files in three areas (site, OpenAPI, feature register, one CLI string); every security sentence names a proof and every message is copied exactly from the code; no behaviour change | 120 min | — | `docs` (`vam-e2e check`, `pnpm build`); with the CLI hint also `rust.yml` fast jobs and smoke (`skip-vm-lane`) | [ ] | |

Wave checks: `cargo run -p vam-e2e -- check`; `cargo test -p vam-e2e --test docs_check`
(`repo_docs_are_honest`); `cargo test -p vam-spec --test openapi_yaml`;
`cd docs && pnpm install && pnpm build`; with the CLI hint,
`cargo test -p vam-cli` and `scripts/ci/local.sh --fast`.

**Risks**

- **De-baselining five big pages** (`cli.md` 545 lines, `agent-spec.md` 383)
  marks them `shipped`, a claim about the whole page. The wave reads them whole
  and lists, not rewrites, what is wrong outside this workstream.
- **Message drift.** The page quotes vamd's messages; a later wording change
  makes it stale. Each message names its code line so a grep finds it.

**Open questions** (each with a default)

1. Five pages the edits touch are in `e2e/docs-baseline.txt` (no `status`).
   **Default: they leave the baseline** with `status: shipped` and the
   `proves:` above (the owner asked for frontmatter on every page touched);
   other faults found there are listed in the PR, not fixed.
2. The CLI save-refused hint (`run.rs:416`) tells a member that `agent.json`
   needs `catalog:write`. **Default: fix it in this wave** (one string per
   file, one unit test); the PR then runs `rust.yml` fast jobs as well as
   `docs`. Not taken: the hint stays wrong until a later CLI change, and the
   wave is docs only.
3. Where the handoff lives. **Default: a new page `orchestrator/member-agents`**
   (rules, messages, client guide and design notes in one URL; other pages
   link to it) rather than a section of `orchestrator/catalog.md`.
4. OpenAPI. **Default: descriptions only** (MD-4.5). The draft permissions
   `GET`/`PUT /v1/agents/{urn}/permissions` paths are missing from the OpenAPI
   file altogether; adding them is a follow-up (a new path, not this phase).
5. Model. **Default: Sonnet 5.5 medium** (the earlier sketch said Haiku 5.5
   high, 45 min; the stale list and the feature register make it larger).

**Out of scope here:** sanitizing the old-repo branch names of
`isolation-verify-closeout` decision 9 (`feat/improving-the-vam`), deferred by
the owner (follow-up, see Explicitly later); any behaviour change, including
the namespace collision (documented, not fixed); the VAM app's screens.

**Phase close and workstream close** (orchestrator): the wave merged and
`docs` green in Actions; team checklist below out. Then the workstream is done:
delete `design/member-draft-agents.md` and its `design/README.md` row (facts
moved to `orchestrator/member-agents`); repoint the amendment lines in
`.cursor/workstreams/isolation-verify-closeout.md:30` and `:35` at
`/orchestrator/member-agents/`; move "Explicitly later" rows to the
`.cursor/workstreams/README.md` backlog; delete this tracker and its README row.

**Team checklist — Phase 4**

Setup: the docs site built from the base branch (`cd docs && pnpm build && pnpm preview`)
or the published site; the frontend lead.

1. Open Orchestrator → Member agents. **See:** the rules, the message table,
   "Building a client" and "Known limitation".
2. For each "Building a client" point, name the API call the app makes.
   **See:** every point maps to a route on the page or in `GET /docs`.
3. Open Users and roles. **See:** a member can save their own draft agents; the
   scope table names `agents:draft`.

Report: "Phase 4 pass", or the step number with what is missing.

## Explicitly later

- A member asks a publisher to release their agent (request and review flow).
- Draft deletion and archive.
- Capping `resourceRequirements` for members once the host honours it.
- Member access to `extends` / `overlay` once the run-time merge is wired.
- Renaming `isolation-verify-closeout` decision 9's branch names (old-repo
  cleanup, `feat/improving-the-vam`; deferred by the owner 2026-11-18).
- Member namespaces that collide (`Alice` / `alice`, non-slug usernames on
  `default`): documented in Phase 4, fixed with `user-identity` Phase 2's
  namespace derivation (`design/user-identity.md` line 242).
- `GET` / `PUT /v1/agents/{urn}/permissions` in `docs/api/openapi.yaml` (the
  paths are missing today).

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
| 2026-11-18 | 3 W2 | Wave 2 on `feat/member-draft-agents-released-runs`: `ResolveMode::ReleasedOnly` follows the live role (`owner_authors`, `draft_mode_for_owner`, `caller_ref_mode` in `catalog_resolve.rs`; bootstrap owner always authors; missing users row is a Member): a draft run whose agent row's owner (the draft row, not a release row) does not author is released-only in record, session start, explain and check; a Member's caller-supplied refs (run `playbook_ref`, grant playbook) are released-only, a released agent keeps its frozen refs. `permissions.advanced` pins recorded in every mode and loaded by pin; boot refuses a recorded draft pin once released-only (`require_no_draft_pins`, `load_advanced_policy`, legacy pinned policy, `resolve_one_skill_ref`); no-pin policy loads (advanced and legacy) take the latest release outside Draft mode; the permissions view uses `run_resolve_context` and keys `owner_draft_run` on a draft. Test fixtures promoted to Publisher: `store_with_alice_skill` (3 tests) and `foreign_draft_playbook_pin_is_rejected_for_bob_not_alice_or_operator`. Scoreboard: demotion scenario green (flipped `active`), other 4 member-draft and the 11 re-run scenarios green (16/16, the Microsoft-mock ones too); 0 scenario fix cycles, 1 test-fixture fix cycle; unit and integration tests green; ~3h. |
| 2026-11-18 | 3 | **Phase 3 closed.** Merged #42 (wave 0), #45 (wave 1), #49 (wave 2) into `feat/member-draft-agents` (plan #38). Scoreboard: all 5 scenarios active and green in Actions, 0 red; the re-run list green; 2 scenario fix cycles (W0, W1), 1 test-fixture cycle (W2); ~5h across the three waves. Next: Phase 4 (docs and frontend handoff). |
| 2026-11-18 | 4 | Phase 4 breakdown drafted: no scenario delta, one docs wave (MD-4.1 to MD-4.7: new page `orchestrator/member-agents` with the frontend handoff, roles and keys, catalog, sessions, permissions, five pages leave the docs baseline, OpenAPI descriptions, three feature-register rows, the CLI hint); waiting for owner approval |
