---
title: Agent access
description: Who may see and use each agent, how a member browses, adds or requests one, how admins decide, the notifications and the exact answers vamd gives.
status: shipped
proves:
  - a-member-cannot-see-or-run-a-restricted-agent
  - a-member-must-request-an-agent-the-admin-installed
  - a-member-requests-an-agent-an-admin-approves-and-the-member-runs-it
  - a-member-adds-an-open-agent-and-runs-it-at-once
  - a-publisher-who-releases-an-agent-decides-who-may-use-it
  - a-revoked-member-cannot-start-a-scheduled-run
  - existing-agents-stay-usable-after-upgrade
  - a-member-browses-the-agents-they-may-ask-for
  - an-agent-admin-sees-only-requests-for-agents-they-administer
  - an-agent-admin-adds-a-second-admin-who-can-then-decide
  - a-member-withdraws-a-request-and-leaves-an-agent
  - hidden-agents-stay-hidden-on-every-new-access-route
  - a-retried-access-write-replays-the-first-answer
  - an-access-request-notifies-the-agents-admins-and-the-decision-notifies-the-member
  - a-request-for-an-agent-with-no-admin-goes-to-the-org-admins
  - a-member-browses-requests-and-leaves-an-agent-from-the-cli
  - an-agent-admin-decides-requests-from-the-cli
  - an-agent-admin-names-other-admins-from-the-cli
  - the-cli-tells-a-member-how-to-ask-for-an-agent
  - a-marketplace-listing-sets-the-access-of-the-agents-it-installs
  - a-listing-update-keeps-the-access-an-admin-chose
  - a-hidden-agent-answers-like-a-missing-one-on-every-catalog-route
  - a-playbook-run-in-a-bound-space-is-admitted-as-the-spaces-agent
  - an-owners-draft-stays-usable-and-a-foreign-draft-pin-is-not-found
  - catalog-and-key-scopes-never-make-an-agent-admin
  - a-demoted-or-deactivated-admin-is-handled-by-the-rule
  - access-follows-the-agent-across-its-releases
  - crates/vam-orchestrator/src/agent_access.rs::sql_predicate_matches_rule
  - crates/vam-orchestrator/src/agent_access.rs::a_request_reaches_the_agents_admins_only_with_a_fixed_text
  - crates/vam-orchestrator/src/agent_access.rs::with_no_active_admin_the_org_admins_and_the_operator_are_told
  - crates/vam-orchestrator/src/agent_access.rs::a_replayed_request_notifies_once
  - crates/vam-orchestrator/src/api/agent_access.rs::handler_json_parses_as_its_proto_message
  - crates/vam-orchestrator/src/agent_access.rs::hidden_write_routes_tests::put_of_a_hidden_installed_agent_equals_a_missing_urn_in_both_modes
  - crates/vam-orchestrator/src/api/catalog_auth.rs::keys_admin_without_users_admin_is_hidden_on_a_restricted_agent
  - crates/vam-orchestrator/src/notify_delivery/tests.rs::an_access_request_names_the_command_that_approves_it_by_request_id
  - crates/vam-orchestrator/src/notifications_tool.rs::an_access_request_names_the_command_that_approves_it_by_request_id
  - crates/vam-spec/tests/openapi_yaml.rs::openapi_has_agent_access_routes
---

Every agent has an **access policy**. It decides which members can see the agent, and which can run it. Members browse the agents they may ask for, add one that is open, request one that needs approval, and leave one they no longer want. An admin of the agent decides requests. The whole feature is HTTP on the node; nothing here needs a guest or a VM. The `vam` commands for it are in the [CLI reference](/getting-started/cli/#agent-access-vam-agent-browse-vam-access).

## The rule

The policy is `(visibility, mode)` per agent URN, not per version.

| Visibility | Mode | Meaning |
|---|---|---|
| `org` | `everyone` | Every member may use it. This is what an agent has with no policy row, so agents that existed before access control behave as before (`existing-agents-stay-usable-after-upgrade`). |
| `org` or `restricted` | `open` | A member adds it and may use it at once. |
| `org` or `restricted` | `approval` | A member requests it; an admin decides. |

`restricted` with `everyone` is refused with a 400.

A caller is **usable** on an agent when any of these holds: the policy is `(org, everyone)`; the key carries `users:admin` (an org admin); the caller has an admin row for the agent; the caller owns the agent's draft; or the caller has an approved request. Otherwise the agent is **visible** when its visibility is `org`, and **hidden** when it is `restricted`.

| Outcome | Browse | Run, chat, Space, grant | Any other route naming the agent |
|---|---|---|---|
| Usable | listed, `access.state` `approved` | allowed | allowed |
| Visible | listed with the caller's state | 403 `agent_access_required: request access to '<urn>'` | as the route says |
| Hidden | not listed | 404 | 404 |

A hidden agent answers with exactly the text of a missing URN, `agents '<urn>' not found in catalog`, on every access route, so a member cannot tell a hidden agent from one that does not exist (`hidden-agents-stay-hidden-on-every-new-access-route`, `a-member-cannot-see-or-run-a-restricted-agent`).

Access is checked once, when an agent is admitted. A playbook, skill or policy the agent uses inherits it. Leaving or being revoked takes effect at the next admission: a session already admitted keeps running, and a scheduled or triggered run is checked each time it starts (`a-revoked-member-cannot-start-a-scheduled-run`).

### Who is an admin

An agent admin is a row in the agent's admin list. The user who first releases an agent is added as its admin (`a-publisher-who-releases-an-agent-decides-who-may-use-it`). An agent installed from a marketplace has none; the org admins decide its requests (`a-member-must-request-an-agent-the-admin-installed`). Org admins, meaning keys with `users:admin`, are admins of every agent without a row.

### What a marketplace install starts with

The first install of an agent writes its policy, and nothing writes it again. A listing in `marketplace.json` may carry the default in an optional `access` object:

```json
"access": { "default": { "visibility": "org", "mode": "open" } }
```

`visibility` is `org` or `restricted`; `mode` is `everyone`, `open` or `approval`. `restricted` with `everyone` is refused, as it is on the policy route, and so is any other key (a listing cannot name admins). Without `access`, the agent starts as `(org, approval)`. The default is applied to each agent of the listing the first time that agent is installed, including an agent a later update adds. An update never changes a policy, and an install from before this field keeps having no row. The node honors the field of any marketplace it trusts, and the admin can change the policy afterwards (`a-marketplace-listing-sets-the-access-of-the-agents-it-installs`, `a-listing-update-keeps-the-access-an-admin-chose`).

The admin check is that rule and nothing else. A key with only `catalog:write`, `publish:grant` or `keys:admin` is not an admin (`an-agent-admin-adds-a-second-admin-who-can-then-decide`).

## Routes

All routes are under `/v1` and need `catalog:read`. The shapes are in the [OpenAPI file](/orchestrator/api/) served at `/openapi.yaml`.

| Route | Who | Answer |
|---|---|---|
| `GET /v1/catalog/agents?pageSize&pageToken` | any caller | Released agents the caller may ask for or use, sorted by URN, each with `access.state` and `canAdmin`. Hidden agents are left out on every page. |
| `POST /v1/agents/{urn}/access {message?}` | caller | `open`: 200, `approved`. `approval`: 202, `pending`. 409 when the agent is already usable or already has a pending request. 404 when hidden. |
| `DELETE /v1/agents/{urn}/access` | caller | Withdraws a pending request or leaves an approved agent. 204, no body. 404 `you have no access request for agent '<urn>'` with no live row. |
| `GET /v1/me/access` | caller | The caller's own requests, every state, newest first, paged. |
| `GET /v1/access-requests?state&adminOf=me` | admin, org admin | Requests for the agents the caller administers (all for an org admin), `state` defaulting to `pending`. A caller who administers nothing gets an empty list. |
| `POST /v1/access-requests/{id}/decide {decision, reason}` | admin, org admin | `approve` or `deny`. `reason` is required on deny. 409 when the request is not pending. 404 for an unknown id and for a caller who cannot decide it. |
| `POST /v1/agents/{urn}/access/{userId}/revoke {reason}` | admin, org admin | Ends an approved access. 200. |
| `GET`, `PUT /v1/agents/{urn}/admins` | admin, org admin | `{admins: [...]}`. `PUT {userIds}` replaces the list (0 to 50 distinct active users) and answers the same shape. 403 for anyone else. |
| `PUT /v1/agents/{urn}/access-policy {visibility, mode}` | admin, org admin | 200 with the policy. |

Browse, the two request lists and the admins list are proved by `a-member-browses-the-agents-they-may-ask-for`, `an-agent-admin-sees-only-requests-for-agents-they-administer` and `an-agent-admin-adds-a-second-admin-who-can-then-decide`. Withdrawing is proved by `a-member-withdraws-a-request-and-leaves-an-agent`: after a withdraw the admin's decide is a 409, and the member can ask again.

`GET /v1/me` gains two fields, both left out when empty: `adminOfAgents`, the agents the caller administers by an explicit admin row (an org admin gets none from `users:admin`), and `pendingAccessRequests`, the number of pending requests the caller can decide.

## Retries

The six writes (policy, request or add, withdraw, decide, revoke, admins) replay. The same `Idempotency-Key` with the same request from the same user returns the first answer byte for byte, changes nothing again and sends no second notification. The same key with another request is a 409, `Idempotency-Key was already used for a different request`. Keys are 1 to 255 printable characters, are kept for 24 hours and are scoped to the caller, so one user's key never returns another user's answer (`a-retried-access-write-replays-the-first-answer`). A call that fails is not stored, so a corrected retry under the same key is processed.

## Notifications

Two [notification](/orchestrator/notifications/) kinds follow the requests.

| Kind | To | When |
|---|---|---|
| `access.requested` | The agent's active admins. With none, the active org admins and the operator. Never the requester. | A request is raised, or the admin list changes while it is pending. |
| `access.decided` | The member | An admin approves, denies or revokes. |

The title is fixed, `<username> asks to use <agent>`. The member's `message` is shown on the request in the queue and is never copied into a notification or a Slack post. One decision closes every admin's copy of `access.requested` as `acted`; a withdraw closes them as `withdrawn`. Slack gets a link and no buttons. The record's CLI line, in Slack and in the notifications tool, is `vam access approve <request id>`, the command that decides it. `PUT /v1/agents/{urn}/admins` brings the copies of the pending requests on that agent in line with the new list, in the same transaction: a removed admin's copy closes as `expired`, a new admin gets a copy, and with no active admin left the org admins and the operator get copies and any copy held by someone who is no longer a recipient closes. A holder who can no longer decide a copy (removed admin, demoted org admin) may clear it with `POST /v1/notifications/{id}/acted`; a holder who still can decide gets `decide_required`. A PUT that changes nothing, and a replayed PUT, emit nothing. When a denial or revoke leaves a restricted agent hidden from the member, `access.decided` names the agent by the last segment of its URN, as `/v1/me/access` leaves out its name. Proved by `an-access-request-notifies-the-agents-admins-and-the-decision-notifies-the-member` and `a-request-for-an-agent-with-no-admin-goes-to-the-org-admins`.

## Other routes that name a hidden agent

A hidden agent answers `POST /v1/agents/{urn}/release`, `GET /v1/agents/{urn}/release-suggestion` and `DELETE /v1/agents/{urn}` exactly as a missing URN does, for every caller the access rule hides it from. The caller's scopes are checked first, as for a missing URN, so a key without `publish:grant` is told so whether the agent is hidden or not. Then a release and a suggestion answer the route's own 404 (`agent '<urn>' not found`) and a delete answers the 405 (`cannot be deleted: the catalog keeps every release`). Inside the domain of a trusted marketplace all three, and `PUT /v1/agents`, answer the 400 that names the domain, the same for an agent that marketplace installed and for a URN that does not exist, so a hidden installed agent cannot be told from a missing one. A visible agent and an org admin keep today's answers: 409 for an installed agent, 403 `only the artifact owner may publish this agent` for a release by another user (`a-hidden-agent-answers-like-a-missing-one-on-every-catalog-route`). The one exception is a hidden installed agent whose marketplace has since been removed: a caller with `catalog:write` still gets the 409 on `PUT /v1/agents` there, because a save of a missing URN would succeed for that caller and there is no missing answer to give. A member (`agents:draft` without `catalog:write`) gets the same 400 a missing URN outside their namespace gets.

Two cases are accepted, not hidden:

- `PUT /v1/agents` on a URN that another user owns and that no marketplace installed answers 403 to a `catalog:write` holder. That caller can already write the catalog, and a save of a free URN succeeds, so the answer tells them the URN is taken. Owner decision, 2026-11-28.
- The released playbooks and skills a hidden agent uses still show in `GET /v1/playbooks` and `GET /v1/skills` under their own names. Only agents carry access; a playbook or skill inherits the access of the agent that uses it, and is checked once, at agent admission. Its name is not secret, the agent is.
