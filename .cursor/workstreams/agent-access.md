# Workstream: Agent access (per-agent visibility, Add and Request, agent admins)

```yaml
status: active
slug: agent-access
current_phase: 2
branch: feat/agent-access
design_docs:
  - design/agent-access.md
  - docs/src/content/docs/orchestrator/catalog.md
```

## Goal

Marketplace and catalog agents can be hidden by default. A member browses the agents they may ask
for, **Adds** an open agent (granted at once) or **Requests** an approval agent; each agent has its
own agent admins who approve or deny, with org admins implicitly admins of every agent. An agent is
usable (run, session start, Space binding, run grants, scheduled and triggered runs) only once
granted. Built in `vamd`; served over HTTP, the CLI and Squrell-App, with notifications to admins
and requesters.

## Out of scope

- Gating playbooks, skills, policies (they inherit from the agent; design §2.1).
- Gating connectors, org MCP connections, models, secrets, brokers (stay org-admin; backlog).
- Any change to HSP brokers.
- Groups, expiring grants, email or push delivery.
- Changing how marketplaces are trusted or installed (`/v1/marketplaces` stays `users:admin`).

## Locked decisions

Do not re-litigate without editing this list.

1. Access is enforced in `vamd` (`crates/vam-orchestrator`), never in an HSP broker.
2. Access is checked once, at agent admission; never per dependency.

Owner-approved 2026-11-27 (design §2.4, recommended answers):

3. A1: gate agents only; dependencies inherit; connectors, org MCP, models, secrets and brokers stay org-admin.
4. A2: `design/connectors.md` D4 and D27 amended (done in Phase 0): admin installs, then each agent's policy decides who sees and uses it; org releases are visible to the whole org unless their access policy restricts it.
5. A3: install stays node-wide (`/v1/marketplaces/…/install`, `users:admin`); the per-user grant sits on top.
6. A4: agent admins are rows in `agent_admins`, not a `UserRole`; org admins are implicitly admins of every agent.
7. A5: no policy row means `(org, everyone)`; new marketplace installs default `(org, approval)`; the org's own releases default `(org, everyone)`.
8. A6: the user who first releases a URN is seeded as its admin; installs have none (org admins only).
9. A7: a revoked grant stops future scheduled and triggered starts (checked at each `start_run`); a running session is not killed.
10. A8: visible-but-not-granted returns 403 `PermissionDenied` with message prefix `agent_access_required:`; no new `VamErrorCode`.
11. A9: the marketplace manifest carries `access.default {visibility, mode}` only, no admins.

## Status legend

- `[ ]` not started
- `[~]` in progress
- `[x]` done (code + tests, or merged)

## Phases

### Done phases

Done phase sections are deleted; git history keeps them.

| Phase | Name | Done |
|---|---|---|
| 0 | Owner decisions | 2026-11-27 |
| 1 | Storage, the rule, enforcement at agent admission (closed in base: PR #144; contracts frozen below) | 2026-11-27 |

### Phase 2 — Routes, OpenAPI, proto, notifications

**Goal:** the HTTP API of design §4, `access.requested` and `access.decided` notifications, the
contract frozen for the frontend.  
**Depends on:** Phase 1 (done)  
**Plan:** approved 2026-11-27, below  
**Exit:** routes in `docs/api/openapi.yaml` and `orchestrator.proto`, baseline regenerated; docs
page `orchestrator/agent-access.md` (`status: shipped`, `proves` its scenarios); notification
scenarios green; frontend JSON shapes (design §6) marked frozen.

- [ ] Routes and `GET /v1/me` additions
- [ ] Notifications kinds and producers
- [ ] OpenAPI, proto, baseline, docs page

**Plan (breakdown approved by Roshan, 2026-11-27).**

Read-only breakdown by the Opus breakdown agent, 2026-11-27, against `feat/agent-access` @ e2bca3d
(Phase 1 merged, PR #144). Format: `phase-playbook.md` §3 step 2 and §11, same as
`p1-breakdown.md`. Line cites checked on that head. Item prefix `AA-2.n`. Roshan approved it and all six
recommended answers (Q1-Q6) on 2026-11-27. The frozen contracts are in design §6.

**Goal (reworded).** The remaining design §4 routes are `GET /v1/catalog/agents`,
`DELETE /v1/agents/{urn}/access`, `GET /v1/me/access`, `GET /v1/access-requests`,
`GET`/`PUT /v1/agents/{urn}/admins` and the `/v1/me` additions. On top of them: Idempotency-Key
replay on all six access writes, the four Phase 1 writes included (Phase 1 backlog), and
`access.requested` and `access.decided` with their producers. Then OpenAPI, proto, baseline, the docs
page and the catalog.md amendment. Design §6 is frozen with its "Amendments frozen by Phase 2".

**Phase rubric.** `Decide: NOW — proof 3 (owner goal; P1 override: browse, admins or lists leak a
hidden agent), cost 2 (six routes, one table, two notification kinds, proto)`. `lane: no` for every
wave (no guest or VM path).

#### 1. Code map and outline

##### 1.1 What Phase 1 shipped (verified)

| Piece | Where |
|---|---|
| Tables + live index | `agent_access.rs:20-53` |
| Rule `decide_access` / `agent_access_on` | `agent_access.rs:200-262`; admin rule `is_agent_or_org_admin` `:299-309` (never `pdp.authorize`) |
| SQL predicate (usable) | `usable_predicate_sql` `agent_access.rs:315-324`, used by `GET /v1/agents` `catalog.rs:1077-1112` |
| Store writes | `request_agent_access` (one lock) `:631-655`, `decide_access_request` `:665-694`, `revoke_access` `:698-728`, `put_agent_access_policy` `:573-594`, `seed_releaser_on` `:432-444`, install default `:469-489` |
| Routes | `api/agent_access.rs` (`stand` `:98-112`, policy `:114-139`, access `:194-239`, decide `:295-320`, revoke `:324-357`, `request_json` `:394-425`) |
| Dispatch | router `api/mod.rs:174-184`; POST wildcard `:625-647`; PUT wildcard `api/agent_access.rs:75-88`; GET wildcard `api/mod.rs:649-698`; DELETE wildcard `:720-726` → `refuse_catalog_delete` `:701-718` |
| Errors | Hidden = `catalog_not_found("agents", urn)` `catalog.rs:521`; Visible = `AgentAccessRequired` (403) |

##### 1.2 Where each new route plugs in

| Route | Dispatch | Reads | Hidden | Not admin |
|---|---|---|---|---|
| `GET /v1/catalog/agents` | new static route beside `/v1/catalog/reassign-owner` (`api/mod.rs:153`) | new SQL over `agents` released rows, latest per URN (ordering of `latest_released_where_sql` `catalog.rs:331-333`) | not listed | n/a |
| `DELETE /v1/agents/{urn}/access` | `delete_agent_wildcard` `api/mod.rs:720-726`, before `refuse_catalog_delete`; add `HeaderMap` | caller's live row (`read_live_row` `agent_access.rs:395`) | 404 missing-URN text when no own row | n/a |
| `GET /v1/me/access` | new static route (beside `/v1/me/contacts`, `api/contacts.rs:24`) | `agent_access WHERE user_id = caller` | `agentDisplayName` omitted | n/a |
| `GET /v1/access-requests` | new route beside `api/mod.rs:181-184` | `agent_access` joined to `agent_admins` (org admin: all) | only the caller's agents | empty list, 200 |
| `GET …/admins` | GET wildcard, after the permissions match `api/mod.rs:655-657`, before the record read `:679` | `agent_admins` ⋈ `users` | 404 missing-URN text | 403 |
| `PUT …/admins` | `put_agent_wildcard` `api/agent_access.rs:82-87` | same | 404 | 403 |
| `GET /v1/me` | `me::get_me` `api/me.rs:29-61`; `GetMeResponse` `orchestrator.proto:704-718` | `agent_admins`, pending count | — | — |

##### 1.3 Notifications machinery (what W2 extends)

`Kind` is a closed enum at `notifications.rs:49-62`, with `ALL` at `:64-77`, `severity` at `:103-116`,
`is_decision` at `:119-124`, the verb at `:126-133` and `render` at `:148-176`. `record` upserts by
`(user_id, kind, subject_key)`: an open row counts up, a resolved row gives up its subject
(`:325-393`), and delivery rows are written in the same transaction (`:388`). `resolve` works by
approval id for three kinds (`:878-901`). The refusal for a decision kind is at `:1283-1288`. Slack:
`DECISION_KINDS_SQL` at `notify_delivery.rs:87` and `on_resolved_at` at `:350-365`, and rendering
without an approval row gives no buttons (`:1505-1515`). The per-kind default comes from
`notification_prefs.rs:66-72` (new kinds default on). The list API filters by `kind`, and an
unknown kind returns empty (`api/notifications.rs:79-86`). The docs kinds table is at
`docs/.../orchestrator/notifications.md:48-56,87`.

##### 1.4 Idempotency today

`require_idempotency_key` (`api/auth.rs:270-278`) only checks that the header is present. Real
replay exists in three places, each scoped to `owner_user_id`, each mapping key to resource id and
answering the resource's *current* state: `space_create_keys` (`spaces.rs:200-204,307-340`),
`runs.idempotency_key` (`catalog.rs:798`, `api/run_start.rs:291-297`) and sessions
(`api/mod.rs:1089-1104`). None stores a response or compares the body. Phase 1 left all four access
writes with `_idempotency` and a TODO. The e2e runner mints a fresh key per write (`vam-e2e/src/api.rs:143`),
so W0 must add a fixed-key option.

##### 1.5 Items

**AA-2.1 Browse** (W1, sec). Store: `browse_agents(caller, org_admin, after_urn, limit)` in
`agent_access.rs`. It reads one row per URN: the latest `released` row by (major, minor, patch).
Filter is `urn > ?after` AND (`org_admin` OR `listable_predicate_sql`), ordered by `urn`,
`LIMIT n+1`. Add `listable_predicate_sql(urn_col, user_param)` = `NOT EXISTS (policy with
visibility='restricted') OR usable_predicate_sql(…)`, with a unit test that mirrors `decide_access`
for every policy × caller (pattern `sql_predicate_matches_rule`, `agent_access.rs:1131`). For each
row:
- `visibility`/`mode` come from the policy, default `org`/`everyone`.
- `access` follows design §6 and its amendments: the caller's latest row by `ORDER BY state IN ('pending','approved') DESC, requested_at DESC, rowid DESC` (the one-second `requested_at` can tie after a withdraw and a re-request).
  Usable → `approved`; `withdrawn` or no row → `none`.
- `canAdmin` = `is_agent_or_org_admin`.
- `displayName`, `description` and `latestVersion` come from the decoded spec.

Route `GET /v1/catalog/agents`: `require_catalog_read`, `PageQuery`, cursor kind `catalog-agents`,
one part = the URN. Accept:
- a member: Hidden not listed, Visible listed with `none`/`pending`/`denied`/`revoked`, Usable
  `approved`;
- an org admin and an agent admin see restricted rows with `canAdmin: true`;
- `pageSize=1` over 3 agents returns each once;
- a token of another list answers 400;
- a token whose URN is a hidden agent returns the same rows as the unedited token.

`Decide: NOW — proof 3, cost 1`.

**AA-2.2 Lists** (W1, sec).
- `GET /v1/me/access?pageSize&pageToken`: the caller's rows, all states, newest first, keyset
  `(requested_at, request_id)` (`pair_cursor`, `api/paging.rs:88-101`), kind `my-access`. Answer
  `{requests, pagination}`. Leave out `agentDisplayName` when the rule gives Hidden for this caller
  now.
- `GET /v1/access-requests?state&adminOf&pageSize&pageToken`:
  - `state` defaults to `pending`; allowed values are the five states or `all`, anything else 400.
  - `adminOf` may be absent or `me`, anything else 400.
  - Rows: every row for an org admin, else a JOIN on `agent_admins` for the caller.
  - Same keyset, kind `access-requests`. A caller who administers nothing gets `{requests: []}`
    with 200.
- Add a shared helper, `admin_scope_sql`, reused by AA-2.7's count.

Accept: a publisher sees only their own agent's requests, an org admin sees all, a member sees `[]`;
a bad `state` answers 400; paging works. `Decide: NOW — proof 3, cost 1`.

**AA-2.3 Admins** (W1, sec).
- `GET …/admins`: `stand` (`api/agent_access.rs:98`); a caller who isn't admin gets 403
  `only an agent admin or org admin may see the admins of '<urn>'`. Answer
  `{admins: [{userId, username, addedBy, addedAt, deactivated?}]}` by `added_at`.
- `PUT …/admins {userIds: [...]}`:
  - `stand`; not admin → 403 with the policy route's text (`:127-129`).
  - 1..=50 distinct ids allowed, `[]` too (Q5). Each must be an existing, active `users` row, else
    400 `user '<id>' cannot be an agent admin` (the same text for unknown and deactivated).
  - One lock: delete rows not in the set, insert new rows with `added_by` = caller, keep kept rows.
  - Answer = the GET shape.
- Dispatch both by suffix `/admins`.

Accept:
- a key with only `catalog:write`, `publish:grant` or `keys:admin` isn't admin (use
  `org-member-with-a-catalog-key`);
- Hidden answers the missing-URN text byte for byte, on GET and PUT;
- removing yourself works; an empty list leaves org admins deciding (decide still 200 for
  `users:admin`).

`Decide: NOW — proof 3, cost 1`.

**AA-2.4 Withdraw or leave** (W1, sec). `DELETE /v1/agents/{urn}/access`, dispatched in
`delete_agent_wildcard` before `refuse_catalog_delete`, with `HeaderMap`, `require_catalog_read` and
the key. Store `withdraw_access(urn, user)` runs in one lock:
- With a live row (pending or approved): set `withdrawn`, `decided_by` = self, `decided_at` = now,
  answer 204 with no body.
- Without one: for a missing or Hidden agent, the missing-URN text (row-2 helper). Otherwise 404
  `you have no access request for agent '<urn>'`.

Leaving is checked at the next admission (A7). W2 clears `access.requested` inside this lock.
Accept: withdraw, then decide answers 409; leave, then chat answers 403; leave again answers 404;
re-Add after leave works; the `marketplaces_tests.rs:514-549` DELETE cases stay green.
`Decide: NOW — proof 3, cost 0`.

**AA-2.5 Idempotent replay** (W1, sec).

> **W1 plan review (Opus, 2026-11-27) overrides the bullets below where they differ.** Order per
> write: key (`require_replay_key`: 1..=255 bytes of 0x21-0x7E) → parse body (a 400 is never stored)
> → fingerprint (length-prefixed fields) → one lock and transaction → prune rows older than 24 h →
> precheck (Hidden 404, §2.3 admin 403, via `_on` reads) → lookup → state checks, write, render,
> insert → commit. So a retry replays the first answer, never one to a caller who lost the right
> to ask. Every store call in the lock is an `_on(&Connection)` variant. `DELETE …/access` has no
> rule precheck (keyed on the caller's own row). `PUT …/admins`: only new ids must exist and be
> active, 0..=50 distinct non-empty strings. Browse state ties break on `rowid DESC`.
- Table (`agent_access::initialize_schema`): `agent_access_replays(owner_user_id, idempotency_key,
  fingerprint, status INTEGER, body TEXT, created_at INTEGER, PRIMARY KEY(owner_user_id,
  idempotency_key))`.
- Helper `with_replay(owner, key, fingerprint, |tx| -> Result<(StatusCode, String)>)` holds one
  `with_access_connection` (`agent_access.rs:552-561`) and an `unchecked_transaction` (pattern
  `catalog.rs:2199`):
  - same fingerprint: return the stored status and body;
  - other fingerprint: 409 `Idempotency-Key was already used for a different request`;
  - no row: run the closure; on 2xx insert the row and commit; on error roll back and store nothing.
- Fingerprint = sha256 over the method, the route template, the decoded URN, userId or requestId,
  and the body re-serialized from parsed JSON.
- A key over 255 chars answers 400. Rows older than 24 h are pruned on insert.
- Wrap all six writes: PUT policy, POST access, DELETE access, decide, revoke, PUT admins. Each
  route's checks, write and response rendering move inside the closure. The 403/404 checks may stay
  in front: they write nothing, and a refused call is never stored.
- **Builder caution:** `std::sync::Mutex` isn't reentrant. `request_json` calls
  `store.get_agent_admitted` and `store.get_user` (`api/agent_access.rs:398-409`), which lock the
  same mutex, so calling it inside the closure deadlocks. Add `request_json_on(&Connection, row)`
  and connection-level user and agent reads.

Accept (unit and API tests per route):
- same key twice gives the same status and body, and exactly one state change or row;
- the same key from another user takes the normal path, so no replay crosses users;
- the same key with another body answers 409;
- a 400 first call followed by the corrected body under the same key is processed;
- two threads with the same key make one write.

`Decide: NOW — proof 3 (a replay across users would leak another user's answer), cost 1`.

**AA-2.6 Notifications** (W2). In `notifications.rs`:
- `Kind::AccessRequested` (`access.requested`, `Action`, `is_decision`, verb `decide`) and
  `Kind::AccessDecided` (`access.decided`, `Info`, verb `open`). `ALL` grows to 14.
- `render`: "<username> asks to use <agent>" and "Your access to <agent> was <approved|denied|revoked>".
  The request `message` is never used.

Producers, each on the fact's transaction (inside AA-2.5's closure, so a replay emits nothing):
- `record_access_requested(tx, row)` when `request_agent_access` inserts `Pending`.
  - Recipients: active admins (`agent_admins ⋈ users WHERE deactivated_at IS NULL`); with none,
    active users with role `org_admin` or `operator`.
  - `subject_key` = request id, `agent_urn` set.
  - `action = {verb: decide, target: {kind: access_request, id}, api: "POST /v1/access-requests/<id>/decide"}`.
- `resolve_access_requested(tx, request_id, reason, acted)` closes every open row of that subject
  and calls `notify_delivery::on_resolved(Resolved::Notification(id))` per row. Decide calls it with
  `acted` (by the decider, surface `api`). Withdraw calls it with `withdrawn`, a new
  `resolvedReason`.
- `record_access_decided(tx, row)` to the requester on decide (approved or denied) and on revoke (Q3).

Also:
- add `'access.requested'` to `DECISION_KINDS_SQL` (`notify_delivery.rs:87`);
- the refusal at `notifications.rs:1283-1288` names `POST /v1/access-requests/<subject>/decide` for
  this kind;
- no Slack buttons (Q6).

Accept:
- recipients are admins only; with no active admin, org admins and the operator; never members or
  deactivated users;
- a decide closes every admin's copy;
- a withdraw closes them with `withdrawn`;
- a replayed request leaves `count` at 1;
- the title and body never hold the message.

`Decide: NOW — proof 3, cost 1`.

**AA-2.7 `/v1/me` additions** (W2). `GetMeResponse` gains `repeated string admin_of_agents = 8;`
and `int32 pending_access_requests = 9;` (`orchestrator.proto:704-718`), filled in `api/me.rs:49-58`:
- `adminOfAgents` = the caller's `agent_admins` URNs that still have a released `agents` row, sorted;
- `pendingAccessRequests` = the count of `pending` rows under AA-2.2's admin scope.

Both are absent when empty (Q2). `Decide: NOW — proof 2, cost 0`.

**AA-2.8 Contracts** (W2).
- `proto/vam/v1/orchestrator.proto`: `AgentAccessPolicy`, `AccessRequest`, `AccessState` (browse
  stub), `CatalogAgent`, `ListCatalogAgentsResponse`, `ListAccessRequestsResponse`, `AgentAdmin`,
  `ListAgentAdminsResponse`, `PutAgentAdminsRequest`, `PutAgentAccessPolicyRequest`/`Response`,
  `RequestAgentAccessRequest`/`Response`, `DecideAccessRequestRequest`, `RevokeAgentAccessRequest`.
- `docs/api/openapi.yaml`: the 10 access routes (Phase 1's 4 and Phase 2's 6), the `/v1/me` fields,
  the error bodies (403 `agent_access_required:`, Hidden 404) and the replay 409.
- Regenerate the baseline: `cargo run -p vam-spec --example generate_baseline`.
- Guards: in `crates/vam-spec/tests/openapi_yaml.rs`, a new `openapi_has_agent_access_routes`. In
  `api/agent_access.rs`, a unit test that each hand-built JSON parses with
  `vam_spec::encoding::json_to_proto` into its message. Handlers keep hand-built JSON (Phase 1
  precedent; booleans such as `canAdmin` must be present).

`Decide: NOW — proof 2, cost 1`.

**AA-2.9 Docs** (W2).
- New page `docs/src/content/docs/orchestrator/agent-access.md` (`status: shipped`; `proves`: N1–N8,
  Phase 1's S1–S7 and the test files). Sidebar entry after "Member agents" (`docs/astro.config.mjs:153`).
- Amend `orchestrator/catalog.md`: routes table, an access paragraph, the link.
- `orchestrator/notifications.md`: the two kinds rows, the clear rule, `withdrawn`.
- `orchestrator/api.md` Auth: replay on access writes.
- `e2e/docs-features.yaml`: an agent-access row (dev page `pending_page: true` until Phase 3's CLI
  docs).
- Check with `cargo run -p vam-e2e -- check`.

`Decide: NOW — proof 2, cost 0`.

**The phase session at approval (docs only, not a wave):** add the amendments to design
§6, mark it **Frozen (2026-11-xx, Phase 2 breakdown)**, and correct §4's table (`/v1/me/access`
paged, admins shape, DELETE 204). Commit with the breakdown.

#### 3. Scenario delta (wave 0)

All scenarios are `process`, mock model, `status: pending`. Pieces exist: `org-admin` (the bootstrap
key, operator, an admin of world agents by A6), `org-administrator` (OrgAdmin role), `org-publisher`,
`org-member`, `org-member-email` (as a second member), `org-member-with-a-catalog-key`, `agent-plain`,
`agent-inline-probe`, `marketplace-squrell`. `P` = `identity-request`.

**The node is shared across scenarios** (`e2e/README.md:363-367`). So assert by `{{acr}}` and by the
per-scenario publisher's agent. Never count an org admin's rows or `pendingAccessRequests` for the
operator.

| Scenario | Given | Steps (when → then) | Red (W0) | Green |
|---|---|---|---|---|
| N1 `a-member-browses-the-agents-they-may-ask-for` | admin, member, agent-plain, agent-inline-probe | when: plain {org, approval}; probe {restricted, approval}; member `P get /v1/catalog/agents` 200 contains plain, lacks probe; member requests plain 202 (`acr`); admin deny with reason "not now" 200. then: member browse contains `"denied"` and `not now`; admin browse contains probe; member `?pageSize=1` has `pagination.nextPageToken` present; `?pageToken=bogus` 400 | when.2 (404: no route) | W1 |
| N2 `an-agent-admin-sees-only-requests-for-agents-they-administer` | admin, pub, member, agent-plain, agent-inline-probe | when: `put-draft {as: pub}` probe; pub release 1.0.0; pub policy {org, approval}; admin plain {org, approval}; member requests plain (`acr1`) and probe (`acr2`), both 202. then: pub `get /v1/access-requests?state=pending&adminOf=me` contains `{{acr2}}`, lacks `{{acr1}}`; admin contains both; member 200 lacks both; member `get /v1/me/access` contains both; `?state=bogus` 400 | then.0 (404) | W1 |
| N3 `an-agent-admin-adds-a-second-admin-who-can-then-decide` | admin, pub, member, member2 (`org-member-email`), ckey, agent-inline-probe | when: put-draft as pub, release; pub policy {org, approval}; member `get …/admins` 403; ckey `put …/admins` 403; pub `put …/admins {userIds: [{pub.userId}, {member2.userId}]}` 200; pub put with id `nobody` 400; member requests 202 (`acr`). then: member2 decides approve 200; pub `get …/admins` contains `{member2.userId}` | when.3 (404, not 403) | W1 |
| N4 `a-member-withdraws-a-request-and-leaves-an-agent` | admin, member, agent-plain | when: {org, approval}; member requests 202 (`acr`); member `delete …/access` 204; admin decide `{{acr}}` 409; admin sets {org, open}; member Add 200; member chat succeeds; member `delete …/access` 204. then: member chat refused 403 `agent_access_required:`; member delete again 404; member `get /v1/me/access` contains `withdrawn`; member Add 200 again | when.2 (403: catalog:write) | W1 |
| N5 `hidden-agents-stay-hidden-on-every-new-access-route` | admin, member, agent-plain | when: admin {restricted, approval}. then: member browse lacks urn; `get …/admins` 404 with `fields.message` = `agents '<urn>' not found in catalog`, and the same template on `…/e2e/no-such-agent/admins`; `put …/admins` 404 same; `delete …/access` 404 same; `get /v1/me/access` lacks urn; `get /v1/access-requests` lacks urn | then.0 (404: no route) | W1 |
| N6 `a-retried-access-write-replays-the-first-answer` | admin, member, agent-plain | when: admin policy {org, approval} with `idempotencyKey: pol`, twice 200; member Add key `req` 202 (`acr`); again key `req` 202 with `request.requestId: {{acr}}`; key `req` with body `{message: other}` 409 contains `different request`; admin decide key `dec` 200, again 200 `request.state: approved`; admin revoke key `rev` 200, again 200 `request.state: revoked`. then: member `get /v1/me/access` has `requests.0.requestId: {{acr}}` and `requests.1: absent` | when.3 (409) | W1 |
| N7 `an-access-request-notifies-the-agents-admins-and-the-decision-notifies-the-member` | admin, pub, oadm (`org-administrator`), member, agent-inline-probe | when: put-draft as pub, release, policy {org, approval}; member requests with message `secret-words-e2e` 202 (`acr`); pub `get /v1/notifications?kind=access.requested&state=open` contains `{{acr}}`, lacks `secret-words-e2e`; oadm lacks `{{acr}}` (the agent has an admin); member lacks `{{acr}}`; pub `get /v1/me` has `pendingAccessRequests: 1` and contains the urn; pub decide approve 200. then: pub open `access.requested` lacks `{{acr}}`; member `?kind=access.decided` contains `{{acr}}`; pub `get /v1/me` has `pendingAccessRequests: absent` | when.4 (contains fails) | W2 |
| N8 `a-request-for-an-agent-with-no-admin-goes-to-the-org-admins` | admin, oadm, member, marketplace-squrell | when: refresh, install as admin; member requests executive-assistant (key `ea`) 202 (`acr`); again key `ea` 202. then: oadm open `access.requested` contains `{{acr}}`; admin (operator) contains `{{acr}}`; member `delete …/access` 204; oadm open `access.requested` lacks `{{acr}}` | then.0 | W2 |

No existing scenario changes. Phase 1's S1–S7 and its 10 changed marketplace scenarios re-run in W1,
because its policy, request, decide and revoke routes are re-wrapped. Their `then` steps stay as
they are.

**Guards** for N1–N8: `agent_access.rs`, `api/agent_access.rs`, `api/mod.rs`, `api/paging.rs`;
plus `notifications.rs`, `notify_delivery.rs` and `api/me.rs` for N7 and N8, and
`marketplace/import.rs` for N8.

**W0 runner addition** (`crates/vam-e2e`, documented in `e2e/README.md` "Roles and drafts"):
`identity-request` gains `idempotencyKey: <string>`. The runner sends the header as
`e2e-<runTag>-<scenario>-<string>` (unique per run on a shared node, same within a scenario).
Without the field it mints a fresh key as today (`api.rs:143`). `check.rs` lints it. W0 also
confirms that `{alias.userId}` expands inside `body` arrays (N3).

**Benchmark delta** (proposed, mock-measured): "hidden agents listed by browse to a non-admin: 0"
(N1, N5) and "repeated side effects on a retried access write: 0" (N6). On approval the orchestrator
adds both to `design/benchmark.md` §1.

#### 4. Waves

| Wave | Items | Turns green | Model | Budget | Depends | CI line | lane |
|---|---|---|---|---|---|---|---|
| 0 | N1–N8 + runner `idempotencyKey` | — (red, right reason) | Sonnet 5.5 low | 60 min | — | `CI: scenarios=<N1..N8>` | no |
| W1 plan review | read-only, before W1 builds (security wave) | — | Opus 5.5 high | 20 min | 0 | — | — |
| 1 (sec) | AA-2.1, 2.2, 2.3, 2.4, 2.5 | N1–N6 | Sonnet 5.5 high (hidden-safe lists, admin rule, replay scoped per user) | 150 min | 0 | `CI: scenarios=<N1..N6>,<P1 S1..S7>,<10 marketplace>` | no |
| 2 | AA-2.6, 2.7, 2.8, 2.9 | N7, N8 | Sonnet 5.5 medium (proto, baseline, closed `Kind` enum) | 120 min | 1 | `CI: scenarios=<N1..N8> tests=workspace` (proto forces it anyway) | no |
| review | phase close, read-only | — | Opus 5.5 high | 30 min | 2 | — | — |
| fix | only if the review finds something | — | Sonnet 5.5 medium | 45 min | review | as W2 | no |

Build waves run one at a time. Only the two Opus reviews read in parallel with nothing.

**Files per wave.**
- W0: `e2e/scenarios/<N1..N8>.yaml` (new), `crates/vam-e2e/src/identity.rs`, `api.rs`, `check.rs`,
  `e2e/README.md`. No product code.
- W1: `agent_access.rs` (browse SQL, listable predicate, lists, admins, withdraw, replay table and
  helper, `_on` readers), `api/agent_access.rs` (handlers, `request_json_on`, replay wrap on all six
  writes), `api/mod.rs` (routes, GET and DELETE wildcard dispatch), `api/paging.rs` (if a URN cursor
  helper is needed), tests in `tests/api_tests.rs` or module tests, scenario `status: active` flips.
- W2: `notifications.rs`, `notify_delivery.rs`, `agent_access.rs` (producer calls),
  `api/agent_access.rs`, `api/me.rs`, `proto/vam/v1/orchestrator.proto`,
  `proto/vam/v1/baseline/vam_descriptor.bin`, `docs/api/openapi.yaml`,
  `crates/vam-spec/tests/openapi_yaml.rs`, the docs pages (`orchestrator/agent-access.md` new,
  `catalog.md`, `notifications.md`, `api.md`), `docs/astro.config.mjs`, `e2e/docs-features.yaml`,
  N7/N8 flips.

**Exit checks** (per wave, in its worktree; low-disk env from working-agreement "Running Rust tests"):
- W0: `cargo test -p vam-e2e`, `cargo run -p vam-e2e -- check`, `scripts/e2e/run.sh --scenarios <N1..N8>`.
  Each must fail at its Red step, class `product` (`expectFail`). Then
  `scripts/ci/local.sh --fast --base origin/feat/agent-access`.
- W1: `cargo test -p vam-orchestrator --lib agent_access`, `cargo test -p vam-orchestrator`
  (incl. `--test api_tests`, `marketplaces_tests`),
  `scripts/e2e/run.sh --scenarios <N1..N6>,<S1..S7>,<10 marketplace>`, then `local.sh --fast`.
- W2: `cargo test -p vam-spec` (conformance, openapi_yaml), `cargo test --workspace`,
  `cargo run -p vam-e2e -- check`, `pnpm build` in `docs/`, `scripts/e2e/run.sh --scenarios <N1..N8>`,
  then `local.sh --fast`. Phase exit: N1–N8 `active` and green in Actions, and design §6 is marked
  frozen.

**Stop-after-W1 shortcut.** After W1 the base has every route and the replay, with N1–N6 green. The
owner can try browse, queue, admins, withdraw and retries with `curl`. Notifications, `/v1/me`
badges, OpenAPI and docs wait for W2. Conditions hold: routes and JSON are final (design §6 amendments), and
W2 only adds producers inside closures W1 already built. Not a separate release.

**Opus reviews.**
- *W1 plan* (security wave, before build) checks:
  - every new route answers Hidden with the missing-URN text;
  - the admin check is `is_agent_or_org_admin`, never `pdp.authorize` or a scope;
  - browse and the lists filter in SQL on every page;
  - the replay is keyed by the caller's user id, so no answer crosses users.
- *Phase close* (after W2), the tracker's list: admin writes and lists use the §2.3 admin rule;
  browse and `/v1/me` leak no Hidden agent; `access.requested` reaches admins only (or active org
  admins when there are none); a replay emits no second notification; OpenAPI, proto and the
  baseline match the handlers. Findings go to one fix wave.

#### 5. Risks, conflicts, questions

**Conflicts between the design and the code** (beyond the issues table)
1. Design §4 shows `GET /v1/me/access → {requests}` unpaged, and gives no admins JSON in §6.
   The design §6 amendments add both.
2. Design §4: "`access.requested` … `is_decision`, cleared by the decide". In code, decision kinds
   are approval-bound in three places (`notifications.rs:878-901,1283-1288`, `notify_delivery.rs:87`).
   AA-2.6 extends each.
3. Design §4: "`api: /v1/access-requests/{id}/decide`". Existing actions carry the method, as in
   `"GET /v1/connectors"` (`notifications.rs:518`). Use `"POST /v1/access-requests/<id>/decide"`.
4. Cite drift: `GET /v1/agents` is now `api/mod.rs:809-826` (design says 796-813), the agents
   wildcard is `:174-180`, and `/openapi.yaml` is included at `api/mod.rs:89`. Notification and
   paging cites hold.

**Risks**
- W1 is five items in about five files. If its plan exceeds one context, split it into W1a
  (AA-2.1 to 2.4) and W1b (AA-2.5), still serial and both high.
- Suffix parsing (`/admins`, `/access`) on an agent whose last URN segment is `admins` or `access`
  would mis-parse; the same holds today for `/versions`. Match only when the head parses as an agent
  URN, else fall through.
- Leaks outside Phase 2 remain on the Phase 5 list:
  - `DELETE /v1/agents/{hidden installed urn}` by a `catalog:write` holder answers 409, not 405
    (`api/mod.rs:711`, `require_not_installed`);
  - `PUT /v1/agents` and `POST …/release` (Phase 1 backlog).
- On a large org, fanning out to every org admin writes one row each. That's bounded by users and
  accepted.
- `access.decided` defaults to external delivery (`notification_prefs.rs:66-72`), so members with
  Slack linked get a DM. That's intended.
- Rendering the answer inside the lock (AA-2.5) is the one deadlock trap (std `Mutex` isn't
  reentrant); the builder brief names it.
- `PUT …/admins` replaces the whole list, so two admins editing at once can lose one edit. Phase 3's
  `admins add|rm` does read-modify-write; accepted, no ETag in Phase 2.

**Open questions for Roshan**: section 5 at the top (Q1–Q6), recommended answers first.

### Phase 3 — CLI

**Goal:** `vam agent browse|add|request|leave|admins|access-policy`, `vam access
mine|requests|approve|deny|revoke` (design §5).  
**Depends on:** Phase 2  
**Plan:** approved 2026-11-27, below  
**Exit:** CLI scenarios green; `list_paged` handles paths with a query string; hints for
`agent_access_required`; CLI docs updated.

- [ ] Commands, hints, `list_paged` fix, docs

### Phase 4 — Squrell-App

**Goal:** browse with status, request dialog, my requests, admin queue, 403 mapping (design §6).  
**Depends on:** Phase 2 contract frozen, i.e. design §6 marked frozen at Phase 2 breakdown approval
(mocks until Phase 2 is in the base, then a real node)  
**Plan:** approved 2026-11-27, below  
**Exit:** app tests green against mocks, then against a node with Phase 2; errors on these paths
are shown, not swallowed into `[]`.

- [ ] `services/agentAccess.ts` and hooks; Marketplace Agents tab on `GET /v1/catalog/agents`
- [ ] Request dialog, my requests, admin queue, `openAgentChat` gate

### Phase 5 — Marketplace manifest and hardening

**Goal:** `access.default` in `marketplace.json` applied on first install; the pitfalls of design §7
covered by scenarios.  
**Depends on:** Phase 2 (A9 answered by its breakdown); the vam-marketplace part after the VAM release
that carries the field (see Execution plan, lane C)  
**Plan:** approved 2026-11-27, below  
**Exit:** hardening scenarios green (name leaks, standalone refs, draft runs, credential confusion,
yanked version, admin vs catalog scopes, demoted owner); vam-marketplace schema documents the field.

- [ ] Manifest field and install copy
- [ ] Hardening scenarios

## Execution plan

Owner's questions, 2026-11-27. Follows `phase-playbook.md` §1–§3, §11.2, §13 and
`working-agreement.md`; where the owner's preference and a rule differ, the rule holds and the gap is
listed last. The breakdown of each phase still decides its items and waves.

**Session shape**

- **One phase session per phase** (orchestrator, Sonnet medium; playbook §1, §9 start message): starts
  the breakdown agent, relays it, runs each wave as a background agent in a worktree, verifies and
  merges into `feat/agent-access`, closes the phase, hands off. It never reads code or writes product
  code (§1, §13.2); any read it needs is a background agent's 15-line report.
- **Phase 0 has no session**: whichever session gets the owner's answers makes the docs edits.
- **Waves: the floor, not more.** A code phase needs wave 0 (scenarios, merges first) and at least one
  build wave (AGENTS.md §2.1; playbook §3 step 2; `phased-workstream/SKILL.md` Stage C). One build wave
  is fine when the items fit one context. Build waves run one at a time (§3 step 2: "one wave at a
  time"), so parallel background agents inside a phase are read lanes only (code map, reviewers).
- **Approvals.** The owner approves each breakdown. Wave plans: the owner may delegate them to the
  phase session (precedent: `connectors.md` progress log 2026-11-20, "owner's delegation"), except the
  security waves marked (sec), whose plan relay goes to the owner.
- **Suggested waves.** P1: W0 · W1 tables, store API, rule · W2 (sec) admission sites, 403/404
  mapping, Space bind after admission, policy and Add routes · W3 (sec) request, decide with the admin
  check, revoke, A5 install default. P2: W0 · W1 (sec) browse, lists, admins, `/v1/me`, admin check · W2 notifications, OpenAPI,
  proto, baseline, docs page. P3: W0 · W1. P4: W0 Playwright specs on mocks · W1 service, hooks,
  browse tab · W2 dialog, queues, `openAgentChat` gate. P5: W0 · W1 manifest field · W2 (sec)
  hardening fixes · W3 vam-marketplace.
- **Input for the Phase 1 breakdown.** Process scenarios act only through the API and CLI (`Step`,
  `crates/vam-e2e/src/scenario.rs:162`; `e2e/README.md` "Write one"), so design §3's "store API used by
  tests" cannot set up a request, approval or revoke. Phase 1 ships the write routes of design §4
  (access-policy, access, decide, revoke) at their final paths and §6 JSON; Phase 2 keeps the rest.

**Reviews per phase**

| Phase | Plan review | Code review | Security review | Phase close | Skipped on purpose |
|---|---|---|---|---|---|
| 0 | Owner answers A1–A9 | — | — | `docs` check green | all: docs only |
| 1 | Owner: breakdown, W2 and W3 plans | Session per PR (playbook §4: CI green, reads the risky lines in `catalog.rs`, `run_start.rs`, `grants.rs`, `api/error.rs`); CodeRabbit | Opus review agent after W3: every design §3 site goes through the rule; Hidden equals a missing URN byte for byte; admin check is the §2.3 rule, never `pdp.authorize` or a scope; Space bound only after admission; no backfill on upgrade. Findings: one fix wave | Scenarios `active`, green; team checklist; owner check (playbook §6) | Separate design review (the design and A1–A9 are that review) |
| 2 | Owner: breakdown, W1 plan, design §6 marked frozen | As P1, plus OpenAPI, proto and baseline match the handlers (`crates/vam-spec/tests/conformance.rs`) | Opus review agent on W1: admins write and lists use the §2.3 admin rule; browse and `/v1/me` leak no Hidden agent; `access.requested` reaches admins only | As P1 | — |
| 3 | Owner: breakdown; wave plans delegated | Session per PR; CodeRabbit | None: the CLI holds no authority, vamd enforces | Scenarios green; checklist | Opus review, security review |
| 4 | Owner: breakdown; wave plans delegated | Session runs the app's local gate (playbook §4: `CI=1 pnpm test`, `tsc --noEmit`, `pnpm e2e:typecheck`); Playwright desktop and mobile (Squrell-App `.cursorrules`) | None: the app hides nothing vamd does not | Owner's manual check of the chat flow (working-agreement "Rules that bit us"); then against a Phase 2 node | Opus review |
| 5 | Owner: breakdown, W2 plan, A9 | As P1; vam-marketplace PR reviewed by `CODEOWNERS` | Final Opus review agent over the whole feature (design §7 list): the one end-to-end security review before the last release | As P1 | — |

**Model and effort.** Cost is context size times turns (§13): Opus only in short agents with a fresh
context, high effort where a miss is a security hole (effort is 3–8% of cost), Sonnet low elsewhere.
A wave failing its checks twice steps one row up (§2); Opus never builds; Fable 5.2 is not used.

| Phase | Activity | Model | Effort | Why |
|---|---|---|---|---|
| all | Phase session (orchestrator) | Sonnet 5.5 | medium | Rule (§13.1); long-lived, so the model price multiplies every re-read |
| all | Breakdown (outline, scenario delta, waves) | Opus 5.5 | high | Rule (§2); about 5 minutes on a fresh context, cheap despite the tier; plan quality sets every wave's cost |
| all | "Where is X" for a breakdown or review | Haiku 5.5 if the owner allows (§1), else Sonnet | low | Read-only and short; the caller re-checks the result |
| all | Wave 0 scenarios and e2e steps | Sonnet 5.5 | low | Rule default; follows existing YAML and `Step` patterns |
| all, 0 | Tracker, docs pages; Phase 0's three edits | Sonnet 5.5 | low | Rule; tracker lines by the session itself |
| 1 | W1 tables, store API, rule | Sonnet 5.5 | medium | New migration and store API under every catalog read (§2) |
| 1 | W2 (sec) admission sites, errors, routes | Sonnet 5.5 | high | Auth boundary (§2); a missed site leaks or admits |
| 1 | W3 (sec) request, decide, revoke | Sonnet 5.5 | high | Decide is the admin check (design §4: never `pdp.authorize`) |
| 1, 2, 5 | Review agent | Opus 5.5 | high | Read-only, short, fresh; two Opus readers found the connectors R1 Critical (`connectors.md` 2026-11-21) |
| 2 | W1 (sec) browse, lists, admins, `/v1/me` | Sonnet 5.5 | high | Admin writes and Hidden-safe lists are the trust boundary |
| 2 | W2 notifications, OpenAPI, proto, baseline, docs | Sonnet 5.5 | medium | Proto, baseline and a closed `Kind` enum: new API surface (§2) |
| 3 | W1 commands, `list_paged` fix, hints | Sonnet 5.5 | low | One crate, templates `cmd/approve.rs`, `cmd/notifications.rs` |
| 4 | W0 specs, W1 service, hooks, browse tab | Sonnet 5.5 | low | One app area, existing patterns |
| 4 | W2 dialog, queues, `openAgentChat` gate | Sonnet 5.5 | medium | Chat flow; services that swallow errors into `[]` (design §6) |
| 5 | W1 manifest field (`vam-spec`, import, install) | Sonnet 5.5 | medium | Core crate (forces workspace tests) and the install path |
| 5 | W2 (sec) hardening fixes | Sonnet 5.5 | high | Name leaks are the security floor (§11.1 override) |
| 5 | W3 vam-marketplace schema doc, CI pin | Sonnet 5.5 | low | Docs and one pin |

**Hand-off between phase sessions.** The closing session leaves:

1. Tracker: items and waves ticked with PRs; phase status (`closed in base` or `released, checklist
   out`); `current_phase` and the README row; a progress-log row with the scoreboard.
2. Branch: `feat/agent-access` head green on Actions; no open wave PRs; only the main checkout in
   `git worktree list`; the release PR's state written (merged, open, held).
3. Frozen contracts named, with the commit: P1 tables, rule outcomes, error texts (prefix
   `agent_access_required:`, Hidden = missing-URN text); P2 routes and design §6 JSON; P5 the field.
4. The next phase's breakdown started and committed (`docs(agent-access): Phase <n> breakdown`,
   playbook §6), so the next session opens at the owner's approval; known issues and backlog lines.
   The session then stops (or at about 300K context, §13.4); archived when its release merges.

**Dependencies and lanes**

```
0 ─► 1 ─► 2 ─┬─► 3 ─► 5 ─► VAM release ─► 5 in vam-marketplace
             └─► 4  (Squrell-App; mocks from Phase 2 breakdown approval)
```

| Lane | Repo, branch | Runs | Why |
|---|---|---|---|
| A | VAM, `feat/agent-access` | 0 → 1 → 2 → 3 → 5, serial | 1 and 2 share `api/mod.rs`, `api/error.rs`, `catalog.rs`, `docs/api/openapi.yaml` (and 2 alone `orchestrator.proto`, `vam_descriptor.bin`, `notifications.rs`); one orchestrator per epic (playbook §9); 3 is one small wave, so 5 after 3 costs about a day |
| B | Squrell-App, `feature/agent-access` (as `feature/connectors`) | 4, from Phase 2 breakdown approval | No shared file; no CI, the local gate decides; app release after the VAM release it needs (vamd first, playbook §8) |
| C | vam-marketplace, PR into `main` | 5's schema doc and the `VAM_COMMIT` pin, after the VAM release | `Listing` is `deny_unknown_fields` (`crates/vam-spec/src/marketplace.rs:55`); its CI builds `vam-cli` at the pinned ref (vam-marketplace `.github/workflows/validate.yml:27`) |

- **3 beside 5** (owner's option, §11.2): shared only `crates/vam-e2e/src/scenario.rs` and this
  tracker; 3 owns `crates/vam-cli`, 5 owns `vam-spec`, `marketplace/`, server fixes, `openapi.yaml`
  (listing schema at `requiresNode`). Cost: a second orchestrator on one base (playbook §9) and queued
  releases (working-agreement rule 6).
- **Scenarios for every phase up front: no.** Each wave 0 writes its own 3–8 (AGENTS.md §2.1 "one phase
  at a time"); Phase 3–5 scenarios now would guess shapes Phase 2 has not frozen.

**Before the owner answers A1–A9**

| May start now (read-only) | Waits |
|---|---|
| Re-check design §1 citations on `main` (written 2026-11-26) | Phase 1 breakdown: A1, A3–A8 set the tables, rule, defaults and error text |
| Read-only map of Squrell-App services that return `[]` on error | Wave 0 files: `then` lines encode A5 (upgrade), A7 (revoked scheduled run), A8 (403 shape) |
| Scenario ids from design §3 | Product code; the D4 and D27 edits (A2) |
| "Accept all recommended" from the owner unblocks Phase 1 | A9 blocks only Phase 5; answer it at that breakdown |

**Phase 1 shortcut: yes as a stop inside Phase 1, no as its own phase.** After W2 the base has the
tables, the rule, enforcement and open Add (`PUT …/access-policy`, `POST /v1/agents/{urn}/access` →
200); scenarios cannot-see-or-run, adds-open-and-runs and existing-agents-unchanged are green, and the
owner can try it with `curl` before W3 adds approval. The design holds: the rule is the same, approval
mode answers 403 until W3. Conditions: A5's `(org, approval)` install default waits for W3 (else a new
install has no request path), and the routes use their final paths and JSON. A separate release is not
worth it: an extra close, checklist and full-suite release PR for files W3 edits again.

**Owner preferences versus the rules (the rules hold until the owner edits them)**

- The phase session builds only through wave agents; it never writes product code (playbook §1).
- Wave 0 plus one build wave is the floor; no parallel build waves in a phase (playbook §3 step 2).
- The owner approves each wave plan unless delegated (playbook §3 step 4; working-agreement "Roles").
- Haiku is not on the model list; the owner says yes first (playbook §1, §10; working-agreement).
- Two phase sessions at once on this epic conflicts with "One orchestrator per epic" (playbook §9),
  though §11.2 allows lanes.

## Explicitly later

- Per-group gates for connectors, org MCP connections and models.
- Groups; expiring grants; request reminders; email and push for `access.*`.
- Primary agent's delegate list (README backlog) uses this rule when it is built.

## Progress log

| Date | Phase | What landed (end with the scoreboard: scenarios green, red, fix cycles, wall time) |
|---|---|---|
| 2026-11-26 | — | Tracker and `design/agent-access.md` opened on `feat/agent-access` |
| 2026-11-27 | 0 | A1–A9 approved by the owner with the recommended answers and locked (decisions 3–11); `connectors.md` D4 and D27 amended; design §2.4 marked approved. Docs only: no scenarios. |
| 2026-11-27 | 1 | Tables, rule, enforcement at every admission site, Space bound after admission, and the write routes (access-policy, access, decide, revoke) landed via PR #144. Frozen: tables per design §2.2; rule outcomes per §2.3; Hidden = the route's own missing-URN text; Visible = 403 `agent_access_required: request access to '<urn>'`; A5 default `(org, approval)` on first marketplace install only; A6 first releaser seeded; admitted sessions keep running after a revoke (A7). Opus review: no Critical or High; found and fixed a missing check on `POST /v1/runs/{id}/messages`. Scoreboard: 7 new scenarios green, 10 adjusted, `cargo test -p vam-orchestrator` green, Actions green on 1edebe5, 1 fix wave. Backlog: Idempotency-Key replay on the four write routes (Phase 2); `PUT /v1/agents` and `POST …/release` name leaks (Phase 5). |
| 2026-11-27 | 2 W1 | Browse, `/v1/me/access`, `/v1/access-requests`, admins GET/PUT, DELETE access and Idempotency-Key replay on all six access writes (the four Phase 1 writes included) landed on `feat/agent-access-p2`, with the Opus plan review's 10 changes. N1–N6 `active` and green; N7, N8 pending (W2). Scoreboard: N1–N6, P1 S1–S7 and the 10 marketplace scenarios 23/23 green, `cargo test -p vam-orchestrator` green, 0 fix cycles. |
