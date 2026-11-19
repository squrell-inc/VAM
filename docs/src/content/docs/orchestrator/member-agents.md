---
title: Member agents
description: How a member saves, edits and runs their own draft agents from released parts, the rules vamd enforces, its exact error messages, and what a client must do.
status: shipped
proves:
  - member-builds-and-runs-own-draft-agent-from-a-released-skill
  - member-draft-agent-refuses-a-skill-that-is-not-released
  - demoted-publishers-draft-agent-stops-using-their-draft-skill
  - member-draft-agent-is-private-and-in-the-members-namespace
  - member-draft-agent-uses-a-connector-skill-through-the-members-connection
  - promotion-widens-a-default-key-but-not-a-narrowed-one
  - demotion-takes-catalog-writes-from-a-publisher-key
  - member-key-minted-with-catalog-write-cannot-write-the-catalog
  - a-run-with-an-inline-agent-is-refused-and-writes-nothing
  - a-run-with-an-inline-playbook-is-refused-and-writes-nothing
  - a-publisher-saves-a-draft-agent-and-runs-it-by-ref
  - member-lists-skills-and-sees-only-released-rows
  - crates/vam-orchestrator/tests/catalog_read_authorize_tests.rs::skills_list_is_released_only_and_drafts_need_catalog_write
  - crates/vam-orchestrator/tests/catalog_read_authorize_tests.rs::skills_list_pages_with_a_token_and_refuses_a_bad_page_size
  - crates/vam-orchestrator/src/paging_store_tests.rs::skills_list_packages_and_manifests_and_a_released_package_hides_its_manifest
  - crates/vam-orchestrator/tests/member_draft_agents_tests.rs
  - crates/vam-orchestrator/tests/catalog_read_authorize_tests.rs::member_put_own_agent_draft_is_200_outside_namespace_is_400
  - crates/vam-orchestrator/tests/workspace_pin_tests.rs::member_can_pin_own_draft
  - crates/vam-orchestrator/tests/key_scope_mode_tests.rs
  - crates/vam-orchestrator/tests/me_tests.rs
  - crates/vam-orchestrator/tests/hsp_grants.rs::a_members_grant_naming_a_draft_playbook_is_refused_and_a_publishers_is_not
  - crates/vam-orchestrator/src/member_draft.rs::each_refused_field_is_a_400_naming_it
  - crates/vam-orchestrator/src/member_draft.rs::namespace_new_inside_ok_outside_400_non_slug_username_uses_default
  - crates/vam-orchestrator/src/member_draft.rs::an_email_username_uses_the_namespace_me_reports_not_the_generated_id
  - crates/vam-orchestrator/tests/member_draft_agents_tests.rs::an_email_member_saves_in_the_namespace_me_reports_and_not_the_user_id
  - crates/vam-orchestrator/src/member_draft.rs::a_urn_in_my_namespace_owned_by_a_publisher_is_403
  - crates/vam-orchestrator/src/member_draft.rs::the_cap_counts_drafts_only_and_refuses_a_new_row_not_an_edit
  - crates/vam-orchestrator/src/member_draft.rs::cap_parses_a_positive_number_else_the_default
  - crates/vam-orchestrator/src/member_draft.rs::always_allow_cannot_add_a_skill_playbook_or_policy_ref
  - crates/vam-orchestrator/src/catalog_resolve.rs::member_owned_draft_run_pins_the_latest_release_not_the_owners_draft
  - crates/vam-orchestrator/src/catalog_resolve.rs::a_members_explicit_draft_playbook_is_refused_even_on_their_own_draft
  - crates/vam-orchestrator/src/catalog_resolve.rs::released_only_refuses_an_explicit_draft_ref_even_for_the_owner_and_an_operator
  - crates/vam-orchestrator/src/api/composition.rs::explain_check_and_run_agree_before_and_after_a_demotion
  - crates/vam-orchestrator/src/permissions_view.rs::a_members_own_draft_is_released_only_and_stays_an_owner_draft_run
---

A member can build an agent of their own and run it. The agent is a draft that only the member
owns. It is made from parts a publisher has already released. This page is the rules, the exact
messages `vamd` sends, and what a client (such as the VAM app) has to do. For the roles and their
scopes, read [Users, keys and ownership](/orchestrator/user-isolation/#roles-and-default-scopes).
For the catalog in general, read [Catalog](/orchestrator/catalog/).

## What a member can do

A member can:

- save and edit their own draft agents (`PUT /v1/agents`);
- attach released skills, playbooks and policies to them;
- set permissions on the draft (`PUT /v1/agents/{urn}/permissions`);
- run and chat on the draft, and start a released playbook with it;
- use a connector skill through their own connection (the org admin enables the connector, the
  member connects it).

A member cannot:

- author skills, playbooks, policies or skill packages;
- release anything, an agent included;
- see, attach or pin anyone else's draft;
- overwrite a row another user owns.

Proved by `member-builds-and-runs-own-draft-agent-from-a-released-skill`,
`member-draft-agent-uses-a-connector-skill-through-the-members-connection` and
`member-draft-agent-is-private-and-in-the-members-namespace`.

## Scopes

The scope `agents:draft` opens two routes: `PUT /v1/agents` and `PUT /v1/agents/{urn}/permissions`.
It opens nothing else. Every role has it by default: member, publisher, org admin and the
bootstrap key (`keys.rs:41-95`). The other `catalog:write` routes keep their own check, and
`agents:draft` does not satisfy it (`closed_routes_stay_403_for_a_member_key` in
`member_draft_agents_tests.rs`).

The member rules apply when the caller's effective scopes hold `agents:draft` and not
`catalog:write` (`api/catalog_auth.rs:46-57`). A publisher key narrowed to `agents:draft` is
restricted too (`member_draft_agents_tests.rs`). A caller with `catalog:write` or `keys:admin`
saves as before, with no member rules.

The effective scopes follow the live role of the key's owner:

- A key minted with no scope list follows the role up and down.
- A key minted with a list is capped by the role and never widened past the list.
- The bootstrap key and a key whose owner is the operator keep their stored list.
- `GET /v1/me` shows the effective scopes.
- Release needs `catalog:write`, `publish:grant` and ownership of the row
  (`api/catalog_release.rs:43-45`).

Proved by `promotion-widens-a-default-key-but-not-a-narrowed-one`,
`demotion-takes-catalog-writes-from-a-publisher-key`,
`member-key-minted-with-catalog-write-cannot-write-the-catalog`, `key_scope_mode_tests.rs` and
`me_tests.rs`.

## Saving

`PUT /v1/agents` takes `{"spec": {…}}`. `PUT /v1/agents/{urn}/permissions` takes the permissions
object. Both need an `Idempotency-Key`. In member mode, `vamd` checks these in order
(`catalog_publish.rs`, `publish_agent_draft_restricted`):

1. **Refused fields.** The save is a 400 if the spec has `capabilityPolicy`, `hitlPolicy`,
   `instructionArtifact`, `extends`, `overlay` or a non-empty `completionPolicy.evaluatorRefs`
   (`member_draft.rs:48-75`; test `each_refused_field_is_a_400_naming_it`).
2. **Shape.** The spec must validate, as for any agent.
3. **Released refs.** Every ref in `skillRefs`, `defaultPlaybookRef`, `allowedPlaybookRefs`,
   `permissions.advanced`, `telemetryPolicy.policyRef` and `completionPolicy.policyRef` must
   resolve to a release (`member_draft.rs:79-136`). One message covers every cause: a URN that
   does not exist, someone else's draft, your own draft, a `-draft` version, and a version with
   no release. It tells a member nothing about other people's rows.
4. **Namespace.** A new URN must be under
   `urn:vam:agent:<registryDomain>/<defaultNamespace>/`. Both values are on `GET /v1/me`. The
   check runs when the caller owns no row of that URN, so an edit of your own draft skips it
   (`namespace_new_inside_ok_outside_400_non_slug_username_uses_default`, and
   `member_put_own_agent_draft_is_200_outside_namespace_is_400`).
5. **Owner.** A URN another user owns is a 403
   (`a_urn_in_my_namespace_owned_by_a_publisher_is_403`).
6. **Cap.** A member keeps at most `VAMD_MEMBER_DRAFT_AGENTS_MAX` agent drafts (default 50, see
   [Host daemon](/orchestrator/overview/)). A new URN past the cap is a 409. An edit is not
   counted (`the_cap_counts_drafts_only_and_refuses_a_new_row_not_an_edit`).

The save writes the member's draft row and never releases. "Always allow" and "always deny" on a
run of your own draft save one tool or host rule without these checks. They change only
`tools.rules` and `internet.rules`: they cannot add a skill, playbook or policy ref, and every
other field of the draft is written back unchanged.

## Error messages

`vamd` answers with `{"code": …, "message": …, "retryable": false}`. Show `message` as it is. The
`<…>` parts are filled in by `vamd`.

| Status | `message` | Cause | Code |
|---|---|---|---|
| 400 | `agent field '<field>' is not available to an agent saved by a member; remove it` | A refused field in the spec | `member_draft.rs:142-145` |
| 400 | `<kind> '<urn>' has no released version; an agent saved by a member may use released skills, playbooks and policies only` | A ref that is not a release. `<kind>` is `skill`, `playbook` or `policy` | `catalog_resolve.rs:282-288` |
| 400 | `a new agent saved by a member must be under urn:vam:agent:<registryDomain>/<defaultNamespace>/ (your namespace); got '<urn>'` | A new URN outside the member's namespace | `member_draft.rs:169-175` |
| 403 | `only the artifact owner may version this agent` | The URN belongs to another user | `owner.rs:709` (member), `owner.rs:642` (publisher) |
| 403 | `missing required scope 'catalog:write' or 'agents:draft'` | The key has neither scope, on `PUT /v1/agents` or its permissions route | `api/catalog_auth.rs:53-55` |
| 403 | `missing required scope 'catalog:write'` | A member on a skill, playbook, policy, package or release route | `api/auth.rs:92-95` |
| 403 | `missing required scope 'publish:grant'` | Release without `publish:grant` | `api/catalog_auth.rs:74-76` |
| 404 | `no draft of agent '<urn>' for this user` | Running or pinning a draft the caller does not own or that does not exist | `catalog.rs:51` |
| 409 | `a member may keep at most <n> draft agents; edit one of your drafts` | A new draft past the cap | `member_draft.rs:196-199` |
| 400 | `<kind> '<urn>': drafts are not available to this agent` | At run start: an explicit `-draft` ref on a released-only run | `catalog_resolve.rs:677-678`, `:771-772` |
| 400 | `'<urn>': drafts are not available to this agent` | At run start: a recorded draft pin on a released-only run, for instance after a demotion | `catalog_resolve.rs:271-273` |
| 400 | `inlineAgent is not accepted: save the agent with PUT /v1/agents and run it by agentRef (your draft is version {major: 1, prerelease: "draft"})` | `inlineAgent` on `POST /v1/runs` | `api/run_start.rs:219-222` |
| 400 | `inlinePlaybook is not accepted: save the playbook with PUT /v1/playbooks and run it by playbookRef (your draft is version {major: 1, prerelease: "draft"})` | `inlinePlaybook` on `POST /v1/runs` | `api/run_start.rs:226-229` |

A missing `Idempotency-Key` on a write is a 400 with
`Idempotency-Key header is required for mutating requests` (`api/error.rs:84-88`).

## Running it

Save the agent, then start a run with `agentRef` at the draft version. Do not send `inlineAgent`
or `inlinePlaybook`; both are a 400 (`a-run-with-an-inline-agent-is-refused-and-writes-nothing`,
`a-run-with-an-inline-playbook-is-refused-and-writes-nothing`).

```json
{
  "spec": {
    "runId": { "urn": "urn:vam:run:<registryDomain>/<defaultNamespace>/first-run" },
    "agentRef": {
      "resourceId": { "urn": "urn:vam:agent:<registryDomain>/<defaultNamespace>/my-agent" },
      "version": { "major": 1, "minor": 0, "patch": 0, "prerelease": "draft" }
    },
    "inputs": { "fields": { "prompt": { "stringValue": "Hello" } } },
    "workspaceRefUri": "volume://my-workspace",
    "idempotencyKey": "<a new UUID per run>"
  }
}
```

A workspace remembers the agent version of its first run. To chat on the draft, keep using the
same workspace (`member_can_pin_own_draft`). A member who names someone else's draft gets the
404 in the table.

The run uses released parts only. The rule follows the live role of the **agent row's owner**,
not of the caller, so an org admin who runs a member's draft gets the same versions the member
gets (`catalog_resolve.rs:213-234`):

- A floating ref is the latest release. The owner's own draft of a skill, playbook or policy is
  never taken.
- An `X.Y.Z` ref is that release, or a 400 if there is none.
- A `-draft` ref is a 400. This holds for a member's explicit `-draft` ref on a run or a run
  grant, even on their own draft
  (`a_members_explicit_draft_playbook_is_refused_even_on_their_own_draft`,
  `a_members_grant_naming_a_draft_playbook_is_refused_and_a_publishers_is_not`).

`member_owned_draft_run_pins_the_latest_release_not_the_owners_draft` and
`released_only_refuses_an_explicit_draft_ref_even_for_the_owner_and_an_operator` prove the
resolver. A publisher's draft run still uses the publisher's own drafts
(`a-publisher-saves-a-draft-agent-and-runs-it-by-ref`).

A demoted publisher's draft agents become member drafts. The next start is a 400 that names the
ref, and a re-save is a 400 too
(`demoted-publishers-draft-agent-stops-using-their-draft-skill`).

To preview, call `GET /v1/agents/{urn}/permissions/effective`,
`GET /v1/agents/{urn}/permissions/explain` and `POST /v1/agent-checks`. They resolve the same
versions the run pins (`explain_check_and_run_agree_before_and_after_a_demotion`,
`a_members_own_draft_is_released_only_and_stays_an_owner_draft_run`).

## Visibility

A draft agent is read by its owner and by a caller with `users:admin`. Another member and a
publisher get a 404, the same as for a missing row, and the other member's list does not show it
(`member-draft-agent-is-private-and-in-the-members-namespace`).

## Listing skills

`GET /v1/skills` lists the skills an agent's `skillRefs` can name, for a skill picker
(`api/mod.rs:849`, store `list_skill_records_page` in `catalog.rs`):

- **Released rows by default**, for every caller: skill packages and skill manifests. A manifest
  whose URN has a released package is left out, because a ref resolves to the package. Each record
  has `channel`, `version`, `ownerUserId`, `catalogTier` and `source` (`package` or `manifest`).
- **`includeDrafts=true`** adds the caller's own drafts. It needs `catalog:write` in the effective
  scopes (`api/catalog_auth.rs:34`); a member gets 403 `missing required scope 'catalog:write'`. Any
  value other than `true` or `false`, an empty one included, is a 400
  `includeDrafts must be true or false`.
- **Another user's draft is never listed**, an org admin's list included.
- **Paging** as on [HTTP API](/orchestrator/api/#paging): `pageSize`, `pageToken`, newest first.

Proved by `member-lists-skills-and-sees-only-released-rows`,
`skills_list_is_released_only_and_drafts_need_catalog_write`,
`skills_list_pages_with_a_token_and_refuses_a_bad_page_size` and
`skills_list_packages_and_manifests_and_a_released_package_hides_its_manifest`.

## Known limitation: shared namespaces

The namespace is `defaultNamespace` from `GET /v1/me`, made from the login name only, never from
the generated user id (see [Model](/authoring/model/)). Two different login names can still give
the same namespace, and then the members share it:

- Email names with the same local part: `ada@one.example`, `ada@two.example`, `ada+work@one.example`
  and the plain name `ada` all give `ada`.
- A name that does not make a valid URN segment gives `default`, so all of those users share it:
  a name that starts or ends with `.`, `-` or `_`, a `+` without an `@`, or an email whose local
  part is empty or starts with `+`.

`Alice` and `alice` no longer collide: login names are unique ignoring letter case, so the second
is refused when the user is created.

Users who share a namespace are not blocked by the namespace check. The owner check still stops an
overwrite with a 403. But a 200 for a free name against a 403 for a taken one tells a member
that the name is taken. This is not fixed.

## Building a client

The points below are for the frontend. Each one names the call.

1. **Show `message` as it is** on a 400, 403 or 409. It names the ref, the field, the namespace
   or the scope.
2. **Decide the controls from `GET /v1/me` `scopes`, not from `role`.** "Create agent" and edit
   need `agents:draft`. Skill, playbook and policy authoring need `catalog:write`. "Release"
   needs `catalog:write` and `publish:grant`. Read `/v1/me` again after a role change or a 403.
3. **Build the new agent URN** as `urn:vam:agent:<registryDomain>/<defaultNamespace>/<name>` from
   `GET /v1/me`.
4. **Attach parts only by ref**, in `skillRefs`, `defaultPlaybookRef`, `allowedPlaybookRefs` and
   `permissions.advanced`, to released versions. Let the picker offer rows whose `channel` is
   released (`GET /v1/playbooks`, `GET /v1/policies`). For skills, call `GET /v1/skills`: it lists
   released skills only (see [Listing skills](#listing-skills)), so offer every row it returns. Each
   ref is an object with the URN in `resourceId.urn`: write it alone to float, as
   `{"resourceId": {"urn": "<urn>"}}`, or pin a release with a `version` object, as
   `{"resourceId": {"urn": "<urn>"}, "version": {"major": 1, "minor": 2, "patch": 0}}`. Never write a
   `-draft` version.
5. **Never `PUT` a skill, playbook or policy to attach it.** A member gets
   `missing required scope 'catalog:write'`. A publisher gets
   `only the artifact owner may version this skill`.
6. **Hide the refused fields** from a member: `capabilityPolicy`, `hitlPolicy`,
   `instructionArtifact`, `extends`, `overlay` and `completionPolicy.evaluatorRefs`.
7. **Save with `PUT /v1/agents`, then run with `POST /v1/runs`** and `agentRef` at the draft
   version. Never send `inlineAgent` or `inlinePlaybook`.
8. **On a 409**, say the member reached the cap and offer to edit one of their drafts. There is
   no delete.
9. **Preview** with the effective permissions, explain and agent-check routes.
10. **Send an `Idempotency-Key`** on every write.

## Design notes

- The rules key on a scope, not a role check, so narrowing a key holds.
- The released-only rule keys on the agent row's owner, not the caller, so a run and its
  preview agree whoever starts it.
- One message covers every ref that is not a release, so a member learns nothing about other
  users' rows.
- The namespace check comes before the owner check, so a foreign URN outside the member's
  namespace answers like a free one.
- Rejected: giving members `catalog:write` and filtering by kind (one missed route would be a
  privilege escalation); comparing stored scopes with role sets to find a "default" key (breaks
  when a role's set changes); rewriting key rows on promotion (misses other paths and races
  with authentication); running an inline agent from the request (boot loads the agent by URN
  from the catalog, so it could boot another row); refusing floating refs for members (floating
  means "follow the latest release", and the run records the pin).

## Not proved here

- The exact order of checks in a save is read from `publish_agent_draft_restricted`; each check
  has its own test, but no test fixes the order of all six.
- The `default` namespace shared by non-slug names is read from the code
  (`vam_spec::default_namespace`); the email case is driven by
  `an_email_member_saves_in_the_namespace_me_reports_and_not_the_user_id`.
