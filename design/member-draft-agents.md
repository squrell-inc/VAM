# Member draft agents: members author their own agents from released parts

Status: **proposal for owner approval, 2026-11-17** (Discovery: Stage A capture and
the Phase 1 breakdown). Tracker: `.cursor/workstreams/member-draft-agents.md`.
Base branch: `feat/member-draft-agents` (cut from `main`).

Read only the section for the phase in hand. Line numbers are on `main` at
2026-11-17.

---

## 1. Goal (owner, binding)

Members are the consumers the product is built for. A member can:

- create and edit **their own draft agents**;
- attach **released** skills, **released** playbooks, **released** policies and
  connectors (enabled by the org admin, connected by the member) to those drafts;
- run their own drafts (chat, try them with released playbooks);
- use any released agent (works today).

A member cannot: release anything (`publish:grant`), create or edit skills,
playbooks, policies or skill packages, see or attach anyone's drafts, or
overwrite another user's catalog row. Publishers keep today's rights. A member's
agent uses released skills only, at save time **and** at run time: an unversioned
ref on a member's draft run resolves to the latest release, never to a draft,
even one the member owns from a time they were a publisher.

Also in scope:

- API keys authorize by the user's **live** role, not the scopes frozen at mint.
- Security bug: the run route writes **and releases** catalog rows (inline agent
  or playbook with a `resourceId`), gated only by `runs:write`.

## 2. What exists today (evidence)

### 2.1 Scopes and keys

| Fact | Where |
|---|---|
| Role default scope sets; `MEMBER_SCOPES` has no `catalog:write` | `crates/vam-orchestrator/src/keys.rs:31-100` |
| `sanitize_scopes_for_role` keeps any requested scope except the two operator-only ones, so a **member key can be minted with `catalog:write`** (asserted by `keys.rs:762-768`) | `keys.rs:116-133` |
| Mint stores the sanitized list in `api_keys.scopes`; empty request = role defaults at mint time; the row does not record which | `keys.rs:328-363`; routes `api/mod.rs:402-450` (`POST /v1/keys`), `api/users_keys_admin.rs:64-110` (`POST /v1/users/{id}/keys`) |
| API-key authentication reads the stored scopes and the **live** role (role only feeds `AuthContext::role()`) | `keys.rs:589-643` (role at `:628-635`) |
| Session tokens use `default_scopes_for_role(user.role)` live | `session_tokens.rs:360-376` |
| The scope check reads `auth.key.record.scopes` only (`keys:admin` bypass) | `api/auth.rs:80-96` |
| `has_users_admin`, `has_keys_admin` read the same field | `api/auth.rs:43-60` |
| Catalog read accepts `runs:write` or `catalog:write` | `api/catalog_auth.rs:12-28` |
| Release checks the **live role** only (`role().has_publish_grant()`), not the key's scopes: a publisher key narrowed to `[catalog:write, runs:write]` can still release | `api/catalog_auth.rs:33-42`, `:44-54` |
| `GET /v1/me` returns `auth.key.record.scopes` | `api/me.rs:52` |
| Grant (`/v1/users/{id}/publish-grant`) only changes the role | `users.rs:408-413`, `api/publish_grant.rs:35-64` |
| A run caller copies the key's scopes; a queued message stores them and replays them at start (role re-read, scopes not) | `api/run_start.rs:76-91`, `inbox.rs:126-140`, `inbox.rs:146-200` (`to_caller`, scopes at `:169-173`) |
| Grant holders never authenticate as HTTP (`!record.is_http()` is 401) and the trigger gateway already uses `default_scopes_for_role(live role)` | `keys.rs:612-616`, `gateway.rs:580-585`, `gateway.rs:934-970` |
| Tests mint members with an explicit `catalog:write` | `tests/isolation_coverage_map_tests.rs:48-52` |
| The e2e harness mints every `org` piece key with no scope list (role defaults) | `crates/vam-e2e/src/engine.rs:655-730` |

### 2.2 Catalog writes

| Route | Gate | Where |
|---|---|---|
| `PUT /v1/agents` (draft) | `catalog:write` | `api/mod.rs:569-586` → `publish_agent_draft_with_owner` `catalog_publish.rs:1018-1054` (applies permission defaults, no ref check) |
| `PUT /v1/agents/{urn}/permissions` | `catalog:write`, own draft (foreign = 404) | `api/permissions.rs:144-171` (scope at `:155`) |
| `POST /v1/agents/{urn}/migrate` | `catalog:write` | `api/permissions_migrate.rs:35` |
| `DELETE /v1/agents|playbooks|skills/packages/{urn}` (always refused, 405/409) | `catalog:write` | `api/mod.rs:663-688` (`:672`) |
| `PUT /v1/policies` | `catalog:write` | `api/mod.rs:791-812` (`:798`) |
| `PUT /v1/playbooks` | `catalog:write` | `api/mod.rs:888-906` (`:895`) |
| `PUT /v1/skills` | `catalog:write` | `api/mod.rs:910-932` (`:917`) |
| `POST /v1/skills/packages`, `…/{urn}/release` | `catalog:write` (+ live publish for release) | `api/skill_packages.rs:42`, `:185` |
| `POST /v1/{agents|playbooks|skills|policies}/{urn}/release` | `catalog:write` + `require_release_authorization` (live role, owner, not installed); dependencies must be released (409) | `api/catalog_release.rs:43-44`, `:63-64`, `:85-86`, `:106-107`; freeze `catalog_freeze.rs:61-121` |
| `GET /v1/hosted/brokers[/…]` | `catalog:write` | `api/mod.rs:1957`, `:1976` |
| Reassign owner, marketplaces | `users:admin` | `api/catalog_owner.rs:38`, `api/marketplaces.rs:72-366` |
| Owner check on every draft upsert ("only the artifact owner may version this …") | first writer owns the URN; released row's owner wins | `owner.rs:612-646`, called at `catalog_publish.rs:195` |
| No namespace rule: any caller with `catalog:write` may claim any URN in the registry domain | — (`vam_spec::default_namespace` is only reported by `GET /v1/me`, `api/me.rs:47`) |
| "Always allow / deny" writes the agent's draft, own draft run only | `session_targets.rs:648-660`, `:720` |

### 2.3 The run route writes released rows (the bug)

`start_run` (`api/run_start.rs:216-601`):

- `:345-351`: an `inline_agent` with a `resource_id` calls
  `put_agent_with_owner` (`catalog.rs:767-784`), which saves a draft **and
  releases it** at the fixed catalog version.
- `:382-388`: an `inline_playbook` with a `resource_id` calls
  `put_playbook_with_owner` (`catalog.rs:951-970`), the same.

The gate is `runs:write`, which every member has. The release goes through
`release_agent` → `freeze_agent`, so unreleased dependencies are still refused
(409) and the owner check still holds. What is skipped is `publish:grant`: a
member publishes an agent (their instructions, their permissions, default open
internet) that every user then lists and may run, and claims its URN. A playbook
is worse: members may never author playbooks at all.

These two lines are the only production callers of any `put_*_with_owner` or
`put_*_released_with_owner` writer (the rest are in `#[cfg(test)]` modules).
Marketplace imports write released rows behind `users:admin`.

### 2.4 Ref resolution

| Fact | Where |
|---|---|
| `record_resolved_refs` pins `playbook_ref`, `skill_refs`, default/allowed playbooks, `overlay.skills.add`, playbook `requires` skills, the four legacy policy refs; **not** `permissions.advanced` | `catalog_resolve.rs:128-171`, `:460-503` |
| `run_resolve_context`: a draft run resolves for the **agent row's owner** in `ResolveMode::Draft` (floating refs take the owner's draft first); any other run is `Release` for the caller; session start calls the same function | `catalog_resolve.rs:180-202`; session start `manager.rs:1233-1250`, `policy_legacy.rs:106-110` |
| An explicit `X.Y.Z-draft` ref resolves to the owner's draft in **both** modes | `catalog_resolve.rs:268-288` (table), `:676-700` |
| Foreign or missing explicit draft is an error | `catalog_resolve.rs:604-613` |
| "Kept as-is": a versioned ref with a non-empty digest that the catalog does not hold is kept and labelled `Released` | `catalog_resolve.rs:614-627` |
| A floating `permissions.advanced` policy at session start loads `get_policy_visible(urn, resolve_owner)`, which can be that owner's draft | `security.rs:398-470` (`:464`) |
| Attached playbooks and `start_playbook` use only pins recorded at run start | `playbook_activation.rs:43-100`, `:200-220` |
| Members are blocked from draft pins only because they own no drafts; foreign draft pins are 404 | `catalog_resolve.rs:43-55`; `tests/workspace_pin_tests.rs:292-322` |
| An inline agent with no catalog row resolves as `Release` for the caller, so explicit `-draft` refs hit the caller's own drafts | `catalog_resolve.rs:180-191` |

### 2.5 Connectors

A connector reaches an agent through a released skill whose `requires_connector`
names it (`proto/vam/v1/skills.proto:176-177`); the connection is the member's
own. MCP connections are per user and offered to every run of that user
(`docs/src/content/docs/runtime/tools.md:84-91`). Nothing on the agent spec
names a connection, so "attach a connector" = attach a released skill.

### 2.6 Corrections to the brief's evidence

- `keys.rs:589-639` is where scopes are **read** at authentication; mint is
  `keys.rs:328-363` and the two routes above. Authentication already reads the
  live role (`keys.rs:628-635`); it only ignores it for scopes.
- The inline release does **not** skip the dependency check (it runs the same
  freeze). It skips `publish:grant`, and for playbooks the rule that members
  never author them.
- Two further holes the brief did not list: an org admin can mint a member key
  with `catalog:write` today (§2.1), and release ignores a key that was narrowed
  to exclude `publish:grant` (§2.1).

## 3. Root cause

Authority lives in two places that drift: scopes frozen on the API-key row at
mint, and the role read live, with each route picking one. The run route then
reuses the test convenience `put_*_with_owner` (draft + release in one call)
instead of the guarded draft and release paths, so `runs:write` alone reaches
the release writer.

## 4. Decisions

### 4.1 Q4 first: API keys authorize by the live role (Phase 1)

**Rule.** Each key row gets `scope_mode`: `role` (minted with no scope list) or
`explicit` (minted with a list). At authentication (`keys.rs:589-643`, the one
place an `AuthenticatedKey` is built from an `api_keys` row):

```
effective(key, live_role) =
  stored                                   if key is the bootstrap key or live_role = Operator
  default_scopes_for_role(live_role)       if scope_mode = role
  stored ∩ default_scopes_for_role(live_role)   if scope_mode = explicit
```

The result replaces `record.scopes` in the `AuthenticatedKey`; the stored list
stays on the row (key listings show it). Unknown owner = Member and deactivated
user = refused, as today.

**Why this rule.**

- *Gains on promote, loses on demote:* a default key follows the role both ways.
- *Narrowed keys stay narrowed:* an explicit key never exceeds its stored list,
  and never exceeds the live role. An explicit member key minted with
  `catalog:write` gains it on promotion (it was minted for it) and has no use of
  it while a member; that closes the hole in §2.1.
- *Operator and bootstrap unaffected:* exempt by key id and by role.
- *Grant holders and brokers unaffected:* they never pass HTTP authentication,
  and the gateway already uses the live role (`gateway.rs:580-585`, `:961-967`).
- *Session tokens unaffected:* already live.
- *Future scope additions (`agents:draft` in Phase 3) reach default keys
  automatically.* A rule that infers "default" by comparing the stored list with
  a role's set breaks the moment a role's set changes; a stored mode does not.

**Migration.** `ALTER TABLE api_keys ADD COLUMN scope_mode TEXT NOT NULL DEFAULT
'explicit'`, then set `role` where the stored list equals (as a set) the defaults
of the owner's **current** role. A publisher's key that equals the member set
stays explicit, so it stays narrow. A member's default key becomes `role` and
gains on promotion.

**Every reader of `key.record.scopes`, audited** (all downstream of
authentication, so all see effective scopes with no change of their own):

| Reader | Where | Effect |
|---|---|---|
| PDP scope check | `api/auth.rs:80-96` | live |
| `has_users_admin`, `has_keys_admin` | `api/auth.rs:43-60` | live; `keys:admin` only ever on operator keys |
| Catalog read | `api/catalog_auth.rs:12-28` | live |
| Release | `api/catalog_auth.rs:33-42` | **change:** require `publish:grant` in effective scopes (or `keys:admin`); effective ⊆ live role, so revocation still sticks and narrowing now holds |
| `GET /v1/me` | `api/me.rs:52` | reports effective scopes |
| Run caller | `api/run_start.rs:76-91` | live at request time |
| Queued message start | `inbox.rs:146-200` | **change:** re-derive from the key row (effective) or the role defaults for a session token, instead of replaying queue-time scopes; recompute `users_admin` |
| Connectors, MCP, secrets | `api/connectors.rs`, `api/mcp_connections.rs` | through PDP and `auth.role()`: live |
| Gateway grant caller, queued fire re-check | `gateway.rs:580-585`, `:934-970` | already live; unchanged |
| Key listing | `api/mod.rs:2262` (`to_proto_key`) | shows stored scopes; unchanged |

### 4.2 Q5: the run route never writes released rows (Phase 2)

> *Amended 2026-11-18 by the product owner (option B):* the run route refuses
> any inline agent or inline playbook with 400 and writes nothing; `vam run
> <dir>` saves the draft with `PUT /v1/agents` (and `PUT /v1/playbooks`), shows
> any refusal, then runs it by ref pinned `-draft`. The bullets below on saving
> inline parts as drafts, the "inline agent of Phase 2" caller in §4.3, the
> inline clause in §4.4 and the last sentence of §5 no longer apply. The
> writers still become test-only (through a `test-support` feature, since
> integration tests call them). Plan of record: the tracker's Phase 2 and
> locked decision 5.

- Inline agent with a `resource_id`: saved through the **same function as
  `PUT /v1/agents`** (authority, owner check, installed and platform refusals,
  and from Phase 3 the member rules), as the caller's **draft** only. The run
  then pins `agent_ref` to that draft (`-draft`), so load and boot use the row
  just saved, and the Space is a draft Space. A caller who may not save that
  draft gets 403 naming what is missing; nothing is written.
- Inline playbook with a `resource_id`: saved as the caller's draft only by a
  caller with `catalog:write`, and the run pins it `-draft`. Anyone else: 403
  ("members cannot create playbooks; name a released playbook in
  `playbookRef`").
- Inline agent with no `resource_id`: unchanged (never persisted). From Phase 3
  its refs resolve released-only when the caller may not author (§4.4).
- The draft-and-release convenience writers (`catalog.rs:763-784`, `:947-970`,
  `:1078-1100`, `:1115-1140`) and `put_*_released_with_owner`
  (`catalog_publish.rs:1099`, `:1214`, `:1315`, `:1432`) become test-only
  (`#[cfg(test)]` or a `test_support` module), so no production path can write a
  release except the four release routes and the marketplace import.
- `vam run <dir>` (`crates/vam-cli/src/cmd/run.rs:2090-2150`) keeps working for
  publishers: it now updates their draft and runs it.

### 4.3 Q1: how a member gets draft-agent write (Phase 3)

**Decision: a new scope `agents:draft`**, added to the member, publisher, org
admin and bootstrap default sets. Rejected: a role check (it bypasses key
narrowing: a member's runs-only CI key would gain catalog writes).

Restricted mode = the caller holds `agents:draft` and **not** `catalog:write`.
Since effective scopes follow the live role (Phase 1), this is "is a member" for
default keys, and it also restricts a narrowed publisher key.

| Route | Member (restricted mode) | Why |
|---|---|---|
| `PUT /v1/agents` (`api/mod.rs:576`) | **open**: own draft, rules below | the product |
| `PUT /v1/agents/{urn}/permissions` (`api/permissions.rs:155`) | **open**: own draft (foreign = 404 as today); `advanced` refs released only | toggles, internet, tool rules are how a member "connects" tools |
| Always allow / deny on own draft run (`session_targets.rs:648-660`) | open (unchanged, `approvals:write`) | same as editing permissions |
| `POST /v1/agents/{urn}/migrate` (`api/permissions_migrate.rs:35`) | closed | members cannot save legacy drafts, so nothing to migrate |
| `DELETE /v1/agents|playbooks|skills/packages/{urn}` (`api/mod.rs:672`) | closed (it refuses everyone anyway) | — |
| Release and release-suggestion (`api/catalog_release.rs`, `api/release_suggestion.rs`) | closed | `publish:grant` |
| `PUT /v1/skills`, `/v1/playbooks`, `/v1/policies`, skill packages and their release | closed | members never author them |
| `GET /v1/hosted/brokers` (`api/mod.rs:1957`, `:1976`) | closed | operator surface |
| Reassign owner, marketplaces | closed (`users:admin`) | — |
| Connectors, MCP connections | unchanged (per user, `runs:write` and the org admin's enable) | a connector is attached as a released skill (§2.5) |

**Rules for a restricted-mode save** (one validator, used by `PUT /v1/agents`,
the permissions PUT and the inline agent of Phase 2):

1. Every ref must resolve to an existing **release** (§4.4 mode
   `ReleasedOnly`): `skill_refs`, `default_playbook_ref`,
   `allowed_playbook_refs`, `permissions.advanced`, `telemetry_policy` and
   `completion_policy` policy refs. Otherwise 400:
   `skill 'urn:…' has no released version; an agent saved by a member may use
   released skills only`. A foreign draft and a missing URN get the same message
   (no existence oracle).
2. Refused fields (400, naming the field): `capability_policy`, `hitl_policy`
   (legacy Cedar path), `instruction_artifact`, `extends` and `overlay` (no
   runtime merge yet), `completion_policy.evaluator_refs` (not wired).
3. A **new** URN must sit in the caller's namespace:
   `urn:vam:agent:<registry domain>/<default_namespace(username)>/<name>`.
   Editing an existing own draft skips this (the owner check decides).
4. At most `VAMD_MEMBER_DRAFT_AGENTS_MAX` (default 50) agent drafts per owner.
5. Owner check, installed and platform refusals as today.

### 4.4 Q2: released-only refs, at save and at run (Phase 3)

At save: §4.3 rule 1 (fail fast, 400 names the ref).

At run (defence in depth): a new `ResolveMode::ReleasedOnly`:

| ref | `ReleasedOnly` |
|---|---|
| no version | latest release (never a draft, own or foreign) |
| `X.Y.Z` | that release; missing = error (no "kept as-is", `catalog_resolve.rs:614-627`) |
| `X.Y.Z-draft` | error: "drafts are not available to this agent" |

`run_resolve_context` (`catalog_resolve.rs:180-202`) returns it when the run is
a draft run and the **agent row's owner** does not author by live role (Member),
and when an inline agent with no row is run by a caller who may not author.
Because session start calls the same function (`manager.rs:1236`,
`policy_legacy.rs:109`), recording and boot agree. Keyed on the owner, not the
caller, so an org admin running a member's draft gets the same pins, and a
demoted publisher's old drafts are never used by their agents.

Also: `record_resolved_refs` records `permissions.advanced` pins, so
`load_advanced_policy` (`security.rs:398-470`) uses the recorded release instead
of the floating "visible" lookup; `permissions_view.rs:640-660` and
`api/composition.rs:102` use the same mode so the explain and check routes show
what the run will do.

**What a member's draft run pins:** floating refs pin the newest release at each
run start (recorded on the run and in its receipt); a pinned ref pins exactly
that release. **If a ref later has no release:** releases are never deleted
(`api/mod.rs:663-665`); a marketplace reset can remove installed rows. Then the
run is refused at start with the same 400 naming the ref; the saved draft is
untouched and the member edits it.

Playbook → skill refs: released playbooks carry frozen `requires`, resolved in
the same mode. Attached playbooks and `start_playbook` only use recorded pins
(§2.4), so they follow.

### 4.5 Q3: draft agent pin (Phase 3)

Amends `isolation-verify-closeout` locked decision 6: a member may pin **their
own** draft agent on a workspace; foreign drafts stay 404
(`catalog_resolve.rs:43-55`, `get_agent_at_version_visible`). No pin code
changes: members were blocked only by owning no drafts.

### 4.6 Locked-decision amendments

- `isolation-verify-closeout` #3: members list/get released catalog without
  `catalog:write` **and** create/edit their own draft agents with `agents:draft`;
  skills, playbooks, policies, packages and every release stay on
  `catalog:write` / `publish:grant`.
- `isolation-verify-closeout` #6: members pin released versions or their own
  draft agent; never a foreign draft.

## 5. Phases

| Phase | Name | Depends on | Why this order |
|---|---|---|---|
| 1 | API keys authorize by the live role | — | Phase 3's scope must follow promotion and demotion; closes two live holes (§2.6) |
| 2 | The run route never writes released rows | — | a live escalation (§2.3), smallest change; security floor, NOW |
| 3 | Members create, edit and run own draft agents, released parts only | 1, 2 | the product |
| 4 | Docs and frontend handoff | 3 | docs say what ships |

The brief proposed the inline fix as Phase 3. It is moved to Phase 2 because it
is exploitable today and independent of member drafts (playbook §11.1 security
floor). Phase 3 then reuses Phase 2's "save inline as draft" function.

## 6. Security analysis

| # | Risk | Mitigation |
|---|---|---|
| R1 | Run route releases a member's agent or playbook (§2.3) | Phase 2: draft only through the PUT path; release writers test-only |
| R2 | A demoted publisher keeps `catalog:write` on their keys | Phase 1 effective scopes |
| R3 | An org admin mints a member key with `catalog:write` | Phase 1: capped by the live role |
| R4 | A narrowed key without `publish:grant` can still release | Phase 1: release reads effective scopes |
| R5 | A queued message starts with queue-time scopes (e.g. `users:admin` after a demotion) | Phase 1: `to_caller` re-derives |
| R6 | A member's draft refs a draft skill/playbook/policy (own, from publisher days, or foreign) | Phase 3: save-time 400 and run-time `ReleasedOnly` keyed on the owner's live role |
| R7 | A floating advanced policy loads the owner's draft at session start | Phase 3: advanced refs recorded and loaded by pin |
| R8 | Member drafts default to open internet and side-effect tool rules (`vam-spec/src/package.rs:367-383`) | Same defaults as any agent (owner may choose otherwise, §8 Q7). Org and source rules can only tighten (`docs/…/security/permissions.md:27`); the IP floor holds; the run uses only the member's own secrets (no org fallback) and connections. Members can already do this today with an inline agent |
| R9 | A member's draft visible to others | Listing and get show drafts to the owner and to `users:admin` only (`api/mod.rs:772-788`, `:643-646`); other members and publishers get 404. Org admins seeing drafts is today's behaviour for publishers' drafts; documented |
| R10 | URN squatting (member claims a URN a publisher wants) | Phase 3 rule 3: new member URNs only in the member's namespace; org admin can reassign (`api/catalog_owner.rs`) |
| R11 | Draft flood | Phase 3 rule 4 (cap); request body limits as today |
| R12 | Legacy Cedar or unwired refs smuggled in (`capability_policy`, `hitl_policy`, `extends`, `instruction_artifact`, evaluators) | Phase 3 rule 2: refused for members |
| R13 | Demoted publisher's leftover drafts and releases | Releases stay released and owned (org admin may reassign); their draft skills/playbooks/policies stay private and uneditable (no `catalog:write`); their draft agents become member drafts and run `ReleasedOnly` |
| R14 | Scheduled or triggered runs of a member's draft | Grants already need the member's approval and run as the member (`grants.rs:237`); a grant fire has no version, so it runs a draft only on a draft-bound Space (`catalog_resolve.rs:66-117`); resolution is `ReleasedOnly`. No change |
| R15 | Cost: member sets large budgets | Iterations hard-capped (`run_context.rs:316-329`); model allowlist (`allowed_models`); `resource_requirements` is not honoured by the host today (a later phase that honours it must cap it for members) |
| R16 | Receipts and audit | The receipt records the `-draft` agent pin and every resolved (released) ref; the draft row carries `owner_user_id` |
| R17 | Explain and check routes disagree with the run | Phase 3: same mode in `permissions_view.rs` and `composition.rs` |

## 7. Rejected options

- **Role check instead of a scope** for member drafts: ignores key narrowing.
- **Give members `catalog:write` and filter by kind:** every `catalog:write`
  route (12 of them, §2.2) would need a member branch; one missed route is a
  privilege escalation.
- **Effective = stored ∩ live only:** a default member key never gains on
  promotion (the stored member set lacks `catalog:write`).
- **Infer "default key" by comparing the stored list with role sets, no
  column:** breaks when a role's set changes (Phase 3 adds `agents:draft`) and
  widens a publisher's deliberately member-sized key.
- **Rewrite key rows on promote and demote:** misses role changes made by other
  paths and races with authentication.
- **Inline agent never written, run from the request:** boot loads the agent
  by URN from the catalog (`manager.rs:1217-1228`), so an unsaved inline agent
  with a URN would boot another row.
- **Refuse member drafts with a floating skill ref** (force pins): floating =
  "follow the latest release" is what members want; the run records the pin.

## 8. Open questions (owner), with defaults

1. Move the inline fix to Phase 2 (before member drafts). **Default: yes.**
2. Migration: `role` mode iff the stored list equals the owner's current role
   defaults. **Default: yes.**
3. Mint keeps an above-role explicit scope on the row (capped at auth, shown in
   key listings, absent from `GET /v1/me`) rather than refusing it at mint.
   **Default: keep and cap.**
4. Scope name `agents:draft`. **Default: yes.**
5. New member URNs only in `<registry>/<default_namespace(username)>/`.
   **Default: yes** (username changes in `user-identity` Phase 2 do not touch
   existing drafts: owner check, not namespace).
6. Draft cap 50 per member. **Default: 50, env-configurable.**
7. Member drafts get the same permission defaults as any agent (open internet).
   **Default: same.**
8. Refuse `extends`, `overlay`, `instruction_artifact`, legacy policies and
   evaluator refs on member saves. **Default: refuse.**
9. Inline agent with a URN from a caller who may not save it: 403, not an
   unsaved run. **Default: 403.**
10. Org admins keep seeing members' drafts. **Default: yes.**

## 9. Frontend handoff (delivered into the docs in Phase 4)

- Show vamd's error `message` verbatim on 400/403/409: it names the ref, field
  or scope.
- Attach skills, playbooks and policies **by ref** on the agent
  (`skillRefs`, `defaultPlaybookRef`, `allowedPlaybookRefs`,
  `permissions.advanced`). Never re-PUT a skill, playbook or policy to attach it
  (a publisher gets "only the artifact owner may version this skill"; a member
  gets 403).
- Offer only released artifacts in a member's pickers (`channel` on list rows).
- Show or hide "Create agent" from `GET /v1/me` `scopes`: `agents:draft`
  (members and up); "Release" from `publish:grant`; skill, playbook and policy
  authoring from `catalog:write`. Re-read `GET /v1/me` after a role change; a
  session token and a default API key both follow the live role.
- A member's new agent URN: `urn:vam:agent:<registryDomain>/<defaultNamespace>/<name>`
  from `GET /v1/me`.
- Chat on one's own draft: pin the workspace to the draft (`-draft`), as
  publishers do.
