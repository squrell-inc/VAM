# Workstream: Agent access (per-agent visibility, Add and Request, agent admins)

```yaml
status: active
slug: agent-access
current_phase: 5
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

- [x] Routes and `GET /v1/me` additions
- [x] Notifications kinds and producers
- [x] OpenAPI, proto, baseline, docs page

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

- [x] Commands, hints, `list_paged` fix, docs

Hand-off: Phase 3 done 2026-11-28. Wave PRs #147-#149 merged into `feat/agent-access-p3`; the Opus close review ran and its findings are fixed. CLI scenarios `a-member-browses-requests-and-leaves-an-agent-from-the-cli`, `an-agent-admin-decides-requests-from-the-cli`, `an-agent-admin-names-other-admins-from-the-cli` and `the-cli-tells-a-member-how-to-ask-for-an-agent` are green in CI. CI's `CI:` line takes scenario ids, not labels like C1.

**Approved 2026-11-28 by Roshan with all recommended answers Q1 to Q9** (command names as design §5; short names resolve through browse; `requests` pending by default with `--state`, `--agent`, `--all` = every page; users by id or username plus `vam users ls`; no interactive confirm; table or global `--json`; docs in `getting-started/cli.md`; one short Opus high close review; commits as Roshan with `git commit -s`, no AI trailers).


Read-only breakdown by the Opus breakdown agent, 2026-11-28, against `feat/agent-access-p3` @ 91f470d.
Format: `phase-playbook.md` §3 step 2, as `p2-breakdown.md`. Item prefix `AA-3.n`.

**Phase rubric.** `Decide: NOW (proof 2: owner goal, tracker exit; cost 1: one crate plus the e2e
runner and docs)`. `lane: no` for every wave (no guest or VM path).

#### 1. Code map

| Piece | Where |
|---|---|
| Command tree | `main.rs:29-151` (`Agent` `:88-91`, dispatch `:213`); help guard `every_vam_command_named_in_help_exists` `main.rs:304-330` |
| `vam agent` | `cmd/catalog.rs:24-108` (`AgentCmd`), `run_agent` `:326-412`; the file is 1214 lines, so access code goes in a new `cmd/access.rs` |
| Templates | `cmd/approve.rs` (list, decide, key), `cmd/notifications.rs` (tables, decide line), `cmd/marketplace.rs` (small writes) |
| Client | `get` `client.rs:57-71`, `delete` `:128-144`, `put_raw_json` `:174-197`, `post_raw_json` `:237-266` (202 is success), `list_paged` `:544-571`, `merge_pages` `:515-531` |
| Errors | `http_error.rs` `hint` `:72-225`, `command_for` `:280-331`, `list_command` `:337-351`; exit codes `output.rs:43-119`; `--json` failure document `output.rs:144-160` |
| Tests | `crates/vam-cli/tests/cli_tests.rs` (httpmock, `PROCESS_STATE_LOCK`), module `flag_tests` |
| Server facts used | browse JSON `api/agent_access.rs:643-670`; lists `:673-758`; decide `:404-461`; revoke `:465-504`; admins `:509-569`; replay 409 `agent_access.rs:1182-1184` |

#### 2. Items

**AA-3.1 `list_paged` with a query** (W1). `client.rs`: next-page URL uses `&` when `path` contains
`?`; token percent-encoded with `encode_path_segment`. Tests: httpmock asserts
`/v1/access-requests?state=all&pageToken=t` and `/v1/approvals?pageToken=t`; `list_paged_follows_the_token_only_with_all` stays green.

**AA-3.2 Member commands** (W1). New `cmd/access.rs`, wired from `AgentCmd`:
- `browse [--all] [--page-size n]`: table `URN  NAME  VERSION  MODE  STATE  ADMIN` (`ADMIN` = `canAdmin`), with `reason` under a denied or revoked row.
- `add <agent> [-m msg] [--idempotency-key k]` and `request <agent> [-m msg] [--idempotency-key k]`:
  POST `{message?}`; print `request <id> approved` or `request <id> pending: an admin of <urn> decides`
  (the scenarios remember the id after `request `).
- `leave <agent> [--idempotency-key k]`: DELETE; prints `left <urn>`; `--json` local document (Q6).
- `access-policy <agent> --visibility <org|restricted> --mode <everyone|open|approval> [--idempotency-key k]`: both flags required (vamd requires both, `api/agent_access.rs:211`).
- `<agent>`: URN, or a short name through browse (Q2).
- `vam access mine [--all] [--page-size n]`: table `REQUEST  AGENT  STATE  REQUESTED  REASON`.
Tests: flag parsing, the 200/202 wording, short-name resolution (unique, none, ambiguous), control
characters stripped. Accept: C1 green.

**AA-3.3 Admins** (W2). `vam agent admins ls <agent>`: table `USER ID  USERNAME  ADDED BY  ADDED AT`,
`(deactivated)` marked. `add|rm <agent> <user> [--idempotency-key k]`: read-modify-write and the
resolution order of issue 2; no-op exits 0 with a stderr note; removing the last admin warns that org
admins now decide. `vam users ls` (Q4). Tests: resolution by id, by username in any case, from the
list, from requests, from `/v1/users`, none (exit 4); unchanged set sends no PUT; PUT body keeps the
order. Accept: C3 green.

**AA-3.4 `vam access` admin side** (W2). `requests [--state s] [--agent urn] [--all] [--page-size n]`
(path `/v1/access-requests?state=s&adminOf=me&pageSize=n`): table `REQUEST  AGENT  USER  STATE
REQUESTED  MESSAGE`. `approve <id> [--reason r]`, `deny <id> --reason r`, `revoke <agent> <user>
--reason r`, each with `--idempotency-key`. Print `request <id> approved|denied|revoked`. Tests:
bodies, required reasons (exit 2, no request), revoke resolution over two pages. Accept: C2 green.

**AA-3.5 Hints and helpers** (W1). In `http_error.rs`, before the generic 403:
`agent_access_required:` gives `vam agent request <urn>`; the admin 403 texts; the replay 409
(`different request`) before the `idempotency` branch; 404 on `/v1/agents/<urn>/access|admins|...`
gives `vam agent browse`; `/v1/access-requests` 404 and 409 give `vam access requests [--state all]`;
`/v1/agents/<urn>/access` 409 gives `vam access mine`; revoke 404 gives the approved list. `list_command`
learns `access-requests`. `run.rs` short-name fallback (issue 6). `notifications.rs` decide line for
`access.requested` (issue 14). Tests per hint, a byte-equal test for a hidden and a missing 404, and a
test that every `vam ...` command a new hint names parses through the CLI tree (as `main.rs:304-330`).
Accept: C4 green.

**AA-3.6 Docs** (W2). `getting-started/cli.md`: section "Agent access (`vam agent browse`,
`vam access`)" with one example per command, the exit codes they use and the user-naming rule; add
the new writes to the mutating list (`cli.md:86`) and `agent leave` to the local outputs (`cli.md:53`);
add C1 to C4 to its `proves`. `orchestrator/agent-access.md`: one line linking the CLI section, C1 to
C4 in `proves`. `e2e/docs-features.yaml`: row `agent-access` `dev: [getting-started/cli]`, C1 to C4 in
`proves`, surface gains the CLI commands, `pending_page` removed. Check with `cargo run -p vam-e2e --
check` and `pnpm build`.

#### 3. Scenario delta (wave 0)

All four are `process`, mock model, `status: pending`, layer CLI through `cli` steps with
`as: <alias>` and `root: { from: version/empty }` (as `node-status-shows-the-node-and-cli-versions`).
World pieces exist: `org-admin` (bootstrap key, org admin), `org-publisher`, `org-member`,
`org-member-email` (second member), `agent-plain`, `agent-inline-probe` (deferred; `put-draft` as
`pub`). The node is shared, so assert by `{{acr}}` and the per-scenario agent, never by counts; a
fixed `--idempotency-key` is safe because replays are per user and the users are new each run.
Below, `P` = `{{urn:agent-inline-probe:pub}}`, `A` = `{{urn:agent-plain}}`; `cli m [...]` = a `cli`
step as `member`.

| Scenario | Given | Steps (when, then) | Red (W0) | Green |
|---|---|---|---|---|
| C1 `a-member-browses-requests-and-leaves-an-agent-from-the-cli` | admin, pub, member, agent-plain, agent-inline-probe | when: admin policy A {org, approval} (identity-request); put-draft P as pub, release 1.0.0, pub policy P {org, open}; `cli m [agent, browse]` ok contains A and `none`; `cli m [agent, request, A, -m, please]` ok contains `pending`, remember `acr` after `request `; `cli m [agent, add, P]` ok contains `approved`. then: `cli m [access, mine, --page-size, "1", --all]` ok contains `{{acr}}` and P (two pages on a path with a query); `cli m [agent, leave, A]` ok; `cli m [--json, access, mine]` contains `withdrawn`; `cli m [agent, leave, A]` exitCode 4 contains `vam access mine`; `cli m [agent, request, P]` exitCode 5 contains `already usable` | when.4 (`browse` is no subcommand: exit 2) | W1 |
| C2 `an-agent-admin-decides-requests-from-the-cli` | admin, pub, member, member2, agent-inline-probe | when: put-draft P as pub, release; `cli pub [agent, access-policy, P, --visibility, org, --mode, approval]` ok; member requests (202, `acr1`), member2 requests (202, `acr2`) by identity-request; `cli pub [notifications, --kind, access.requested]` contains `vam access approve {{acr1}}`; `cli pub [access, requests]` contains both ids; `cli pub [access, approve, {{acr1}}, --idempotency-key, a1]` ok contains `approved`; `cli pub [access, deny, {{acr2}}]` exitCode 2 contains `--reason`. then: the same approve with key `a1` ok contains `approved` (replay); `cli pub [access, deny, {{acr2}}, --reason, "not now", --idempotency-key, a1]` exitCode 5 contains `different request`, `new key`; deny with reason ok; `cli pub [access, approve, {{acr2}}]` exitCode 5 contains `not pending`, `vam access requests`; `cli pub [access, approve, acr_nope]` exitCode 4 contains `vam access requests`; `cli member2 [agent, browse]` contains `denied`, `not now`; `cli pub [access, revoke, P, "{member.username}", --reason, done]` ok contains `revoked`; `cli m [access, mine]` contains `revoked`; revoke without `--reason` exitCode 2 | when.2 (`access-policy` is no subcommand) | W2 |
| C3 `an-agent-admin-names-other-admins-from-the-cli` | admin, pub, member, member2, agent-inline-probe | when: put-draft P as pub, release; `cli pub [agent, admins, ls, P]` ok contains `{pub.username}`; `cli pub [agent, admins, add, P, "{member2.userId}"]` ok contains `{member2.username}`; `cli pub [agent, admins, add, P, nobody-e2e]` exitCode 4 contains `user id`; `cli admin [agent, admins, add, P, "{member.username}"]` ok (org admin, via `/v1/users`); `cli member2 [agent, admins, rm, P, "{pub.username}"]` ok (from the list). then: `cli m [--json, agent, admins, ls, P]` contains `{member.userId}`, `{member2.userId}`, absent `{pub.userId}`; `cli pub [agent, admins, ls, P]` exitCode 3 contains `only an agent admin`; `cli m [agent, admins, rm, P, "{member2.userId}", --idempotency-key, r1]` ok; the same again ok (no change) | when.2 (`admins` is no subcommand) | W2 |
| C4 `the-cli-tells-a-member-how-to-ask-for-an-agent` | admin, pub, member, agent-plain, agent-inline-probe | when: admin policy A {org, approval}; put-draft P as pub, release, pub policy P {restricted, approval}. then: `cli m [run, A, -p, "e2e-step:hi Hello"]` exitCode 3 contains `agent_access_required`, `vam agent request A`; `cli m [--json, run, A, -p, "e2e-step:hi Hello"]` contains `"exitCode":3`, `"httpStatus":403`; `cli m [agent, request, P]` exitCode 4 contains `not found in catalog`, `vam agent browse`, absent `agent_access_required`; `cli m [agent, request, urn:vam:agent:local.vam.dev/e2e/no-such-agent]` exitCode 4 contains `vam agent browse` | then.0 (exit 3 holds, but the text says `vam login`: class `product`) | W1 |

Mock: C4 carries the `hi` rule (`agent-plain`'s reply) in case a start slips through. No existing
scenario changes; none guards a `vam-cli` file today (grep), and the one `via: cli` scenario
(`decision-anywhere-clears-the-notification`) re-runs in W1 because `client.rs` changes.

**Guards** for C1 to C4: `crates/vam-cli/src/main.rs`, `cmd/catalog.rs`, `client.rs`,
`http_error.rs`; plus `cmd/run.rs` (C4) and `cmd/notifications.rs` (C2). W1 adds `cmd/access.rs` once
it exists (a guard must name a real file).

**W0 runner additions** (`crates/vam-e2e/src/cli.rs`, `scenario.rs`, `check.rs`; documented in
`e2e/README.md` "Receipt expectations and offline verify", the `cli` paragraph): `exitCode: <0..=255>`
(exact; with it `exit` defaults to `fail`, and `check` refuses `exit: ok` with a non-zero code), and
the placeholder expansion of issue 16. `check` refuses a placeholder whose piece or alias is not in
`given`. Unit tests in `cli.rs` and `check.rs`.

#### 4. Waves

| Wave | Items | Turns green | Model | Budget | Depends | CI line | lane |
|---|---|---|---|---|---|---|---|
| 0 | C1 to C4 + runner `exitCode`, placeholders | none (red, right reason) | Sonnet 5.5 low | 60 min | approval | `CI: scenarios=<C1..C4>` | no |
| 1 | AA-3.1, 3.2, 3.5 | C1, C4 | Sonnet 5.5 low (one crate, existing patterns) | 90 min | 0 | `CI: scenarios=<C1..C4>,decision-anywhere-clears-the-notification` | no |
| 2 | AA-3.3, 3.4, 3.6 | C2, C3 | Sonnet 5.5 medium (user resolution and read-modify-write are logic the plan cannot fully specify) | 120 min | 1 | `CI: scenarios=<C1..C4>` (docs check runs in `checks`) | no |
| review | phase close, read-only | n/a | Opus 5.5 high | 20 min | 2 | n/a | n/a |
| fix | only if the review finds something | per finding | Sonnet 5.5 low | 45 min | review | as W2 | no |

Build waves run one at a time; no dedicated security wave (the CLI decides nothing; vamd enforces).

**Files per wave.**
- W0: `e2e/scenarios/<C1..C4>.yaml` (new), `crates/vam-e2e/src/cli.rs`, `scenario.rs`, `check.rs`,
  `e2e/README.md`. No product code.
- W1: `crates/vam-cli/src/client.rs`, `cmd/access.rs` (new), `cmd/mod.rs`, `cmd/catalog.rs`
  (variants delegate), `main.rs` (the `Access` group), `http_error.rs`, `cmd/run.rs`,
  `cmd/notifications.rs`, `tests/cli_tests.rs`; C1 and C4 flipped to `active`.
- W2: `cmd/access.rs`, `cmd/users.rs`, `main.rs`, `tests/cli_tests.rs`,
  `docs/src/content/docs/getting-started/cli.md`, `docs/src/content/docs/orchestrator/agent-access.md`,
  `e2e/docs-features.yaml`; C2 and C3 flipped.

**Exit checks** (each wave in its worktree; low-disk settings from working-agreement "Running Rust
tests"):
- W0: `cargo test -p vam-e2e`, `cargo run -p vam-e2e -- check`, `scripts/e2e/run.sh --scenarios
  <C1..C4>`: each fails at its Red step, class `product` (`expectFail`). Then `scripts/ci/local.sh
  --fast --base origin/feat/agent-access`.
- W1: `cargo test -p vam-cli`, `cargo clippy -p vam-cli --all-targets -- -D warnings`,
  `scripts/e2e/run.sh --scenarios <C1..C4>,decision-anywhere-clears-the-notification` (C1, C4 green;
  C2, C3 still red at their Red step), `local.sh --fast`.
- W2: as W1 plus `cargo run -p vam-e2e -- check`, `pnpm build` in `docs/`; C1 to C4 green. Phase exit:
  C1 to C4 `active` and green in Actions.

**How the wave plans run.** The phase session starts each wave as a background agent in its own
worktree with a pointer brief (tracker path, wave number, base, the facts above). Its first turn is
plan only; the phase session checks it against this breakdown and approves it (delegated: no wave is
a security wave). The agent builds, runs the exit checks and `local.sh --fast`, opens the PR with the
wave's `CI:` line and "Local CI" summary, and reports in at most 15 lines. The phase session verifies
CI and merges. Two failed check cycles stop the wave; the rerun moves one row up (playbook §2).

**Phase-close review (Q8).** One read-only Opus 5.5 high agent, about 20 minutes, after W2, on the
phase diff only. Checklist: no hint, fallback or table shows anything vamd did not return to that
caller (short-name and user resolution read only routes the caller may call; hidden and missing print
the same text); read-modify-write sends exactly the intended set and never drops a deactivated admin
by accident; every write sends the user's key when given and a fresh one otherwise, and a key is
never reused across two different commands; user text is stripped of control characters; no file
outside the allowed list changed. Findings go to one fix wave.

**PR and commit rules.** Every PR targets `feat/agent-access` (wave PRs onto the phase branch
`feat/agent-access-p3`, which reaches `feat/agent-access` in one PR as #145 did for Phase 2); never
`main`. Commits use `git commit -s` with the maintainer identity (Q9). No `Co-Authored-By`, session
links or "Generated with" lines in commits or PR bodies (AGENTS.md:256). Wave PR titles
`AA-3 W<n>: <summary>`, body lists the items and says "Do not merge".

#### 5. Docs and tracker updates

- Docs: AA-3.6 (W2). `e2e/README.md` for the runner fields (W0).
- Tracker at approval (phase session, docs only): paste section 2 to 4 under Phase 3, record Q1 to Q9,
  set Phase 3 `Plan: approved <date>`, update the reviews row for Phase 3 if Q8 holds, and set
  `current_phase: 3` with the README row once Phase 2 is closed.
- Per wave: tick items, wave row with PR, a progress-log row with the scoreboard.
- Phase close: checkbox `[x]`, progress row, hand-off to the coordinator; Phase 5's breakdown is the
  next session's job.

#### 6. Risks

- **Read-modify-write races.** Two admins changing the list at once can lose one change (no ETag;
  accepted in Phase 2). The CLI re-reads before each PUT, so the window is one round trip.
- **User resolution by name** reads only routes the caller may call; an agent admin who is not an org
  admin can add a never-seen user by id only. A server lookup route is backlog (Q4).
- **Shared node.** `browse --all` walks every agent on the node; the scenarios avoid it and use
  `access mine --page-size 1 --all`, which is per user.
- **Remembered ids from human output.** C1 and C2 remember `{{acr}}` after `request `; that line
  format is part of AA-3.2 and must not change without changing the scenarios.
- **W0 shares `crates/vam-e2e/src/scenario.rs` with Phase 5** (tracker lane note). Phase 5 runs after
  Phase 3, so there is no overlap; Phase 5 rebases on the merged runner.
- **Help guard.** New help texts that name `vam ...` commands must resolve (`main.rs:304-330`); the
  hint test of AA-3.5 extends the same idea to hints.

#### 7. What must NOT be touched

Phase 4 builds on the frozen design §6 in parallel, and Phase 5 owns the server fixes. So Phase 3
changes no file under `crates/vam-orchestrator/`, `crates/vam-spec/`, `proto/` (and the baseline),
`docs/api/openapi.yaml`, and no vamd route, error text or JSON shape. It does not edit design §6 or a
locked decision. If a wave finds it needs a server change, it stops and the phase session asks the
owner. Server items noticed here, for the Phase 5 or backlog list: the `access.requested` CLI line
says `vam notifications show` (`notify_delivery.rs:1506-1509`); no username lookup for agent admins;
no `?agent=` filter on `GET /v1/access-requests`.


### Phase 4 — Squrell-App

**Goal:** browse with status, request dialog, my requests, admin queue, 403 mapping (design §6).  
**Depends on:** Phase 2 contract frozen, i.e. design §6 marked frozen at Phase 2 breakdown approval
(mocks until Phase 2 is in the base, then a real node)  
**Plan:** approved 2026-11-27, below  
**Exit:** app tests green against mocks, then against a node with Phase 2; errors on these paths
are shown, not swallowed into `[]`.

- [x] `services/agentAccess.ts` and hooks; Marketplace Agents tab on `GET /v1/catalog/agents`
- [x] Request dialog, my requests, admin queue, `openAgentChat` gate

Hand-off: Phase 4 done. Squrell-App PRs #18 to #22 merged into `feature/agent-access`; release PR #23 into `main` is held until vamd is deployed. See `p4-breakdown.md` in the project files (`agent-access/p4-breakdown.md`).

### Phase 5 — Marketplace manifest and hardening

**Goal:** `access.default` in `marketplace.json` applied on first install; the pitfalls of design §7
covered by scenarios.  
**Depends on:** Phase 2 (A9 answered by its breakdown); the vam-marketplace part after the VAM release
that carries the field (see Execution plan, lane C)  
**Plan:** breakdown approved by the owner 2026-11-28 (all nine recommendations, Q1-Q9), detail below  
**Exit:** hardening scenarios green (name leaks, standalone refs, draft runs, credential confusion,
yanked version, admin vs catalog scopes, demoted owner); vam-marketplace schema documents the field.

- [x] W0 scenarios H1 to H8 and the `marketplace-access` piece
- [x] W1 manifest field and install copy (AA-5.1 to 5.5)
- [x] W2 (sec) hardening fixes (AA-5.6 to 5.9)
- [x] Close: Opus whole-feature review, fix wave, records (AA-5.10), release in order
- [ ] W3 vam-marketplace docs and pin, after the VAM release and vamd deploy

#### Phase 5 breakdown (approved 2026-11-28)

Owner answers: Q1 A9 reconfirmed (listing-level `access.default`, no admins); Q2 accept `PUT` telling a
`catalog:write` holder a foreign URN is taken; Q3 Hidden answers exactly what a missing URN gets on that
route; Q4 honor the field from any marketplace; Q5 `deactivated` flag on admins is the cleanup list; Q6 fix
the CLI hint now, username lookup and `?agent=` to backlog; Q7 already-passing proofs land `active`; Q8
vam-marketplace gets docs and pin only; Q9 fix app nit N6 with the `src-tauri` cargo test before PR 23.
Full text: the owner's page and `p5-breakdown.md`. Detail follows.


Read-only breakdown by the Opus breakdown agent, 2026-11-28. Item prefix `AA-5.n`. Format: playbook §3.

#### A. Verified citations and drift notes

- **Install default (P1 W3, landed):** `agent_access.rs:498-517`; call `import.rs:1053-1057` sits
  before `insert_release`, so the released-row check sees the pre-write state. Tests
  `import_tests.rs:567` (first install only) and `:631` (keeps an existing row);
  `agent_access.rs:1910`.
- **Install vs update:** `import.rs:171-195` (`Mode::Install` refuses an installed listing,
  `Mode::Update` refuses a missing one); the policy write does not look at the mode.
- **requiresNode precedent:** spec check `marketplace.rs:201-203`; refresh `mod.rs:388-420`; import
  `import.rs:156-165`; view `mod.rs:913`; OpenAPI `openapi.yaml:7543`.
- **§7 name leaks, current state:** list and read through `get_agent_record_visible`
  (`catalog.rs:972-990`); `/versions` keeps only an own draft and refuses with the no-versions text
  (`catalog.rs:710-733`); outcomes same text (`api/outcomes.rs:38-51`); release-suggestion
  (`api/release_suggestion.rs:36-46`, residual in issue 8); composition (`api/composition.rs:70-77`);
  permissions view (`permissions_view.rs:645`; the no-version path reads the caller's own draft only,
  `catalog_publish.rs:851-871`, and `PUT .../permissions` answers the no-draft 404 before the body,
  `api/permissions.rs:157-159`).
- **Standalone `playbookRef`:** with no `agentRef`, admission reads the Space row's agent, then the
  workspace file's (`api/run_start.rs:240-277`), before any Space or binding (`:305-308`); with no
  Space id the run uses the agent-less default Space (`spaces.rs:245-273`). Covered in code.
- **Space bound after admission (P1 fix):** admission `run_start.rs:308`, bind `:393`. Proved by S1
  (`POST /v1/spaces` 404) and S2 (`GET /v1/spaces` lacks the URN after a refused run).
- **Credential confusion:** `has_users_admin` reads only `users:admin` (`api/auth.rs:44-50`);
  `has_keys_admin` is used only by the keys routes (`api/mod.rs:492,514`) and the catalog write and
  publish checks (`api/catalog_auth.rs:59-91`), never by the rule. Admin check
  `agent_access.rs:311-338` (never `pdp.authorize` or a scope).
- **Deactivated:** recipients skip deactivated admins and fall back to org admins
  (`notifications.rs:585-614`); `PUT .../admins` accepts a kept deactivated id
  (`agent_access.rs:1082-1105`).
- **Drift:** P1's `owner.rs:405-408` is now `:410-412`. P2 review "409 vs 405" misses the 400 case
  (issue 7). Tracker Phase 5 "Plan approved" has no plan. `docs-features.yaml:886-907` row has no P5
  proofs.

#### B. Scenario delta

New world piece `marketplace-access` (`kind: git-marketplace`, added by an admin, domain
`access.community.example`, fixture `e2e/world/fixtures/marketplace-access/`): three listings of one
agent each, agent shape copied from `marketplace-community` `helpful-notes`. `open-notes`
`{org, open}`, `quiet-notes` `{restricted, approval}`, `kept-notes` `{org, open}`; tags
`<id>-v1.0.0`. Overlay `marketplace-access-update/`: `kept-notes` index says `{org, everyone}`, ref
`kept-notes-v1.1.0`, agent description changed. No other scenario uses these URNs. W0 documents the
piece in `e2e/README.md` (Marketplace).

| Scenario | Change | Layer | Given | Steps (short) | Status at W0, red step | Green by |
|---|---|---|---|---|---|---|
| H1 `a-marketplace-listing-sets-the-access-of-the-agents-it-installs` | new | process | org-admin, org-member, marketplace-access | add, refresh, `expect listings` `access.default.mode: open`; install open-notes and quiet-notes; member browse lists open-notes, lacks quiet-notes; member `POST .../access` 200 `approved`, chat succeeds; member `GET` quiet-notes 404 exact missing text | pending, `when.1` refresh (unknown field), product | W1 |
| H2 `a-listing-update-keeps-the-access-an-admin-chose` | new | process | org-admin, org-member, marketplace-access | add, refresh, install kept-notes; admin policy `{org, approval}`; `git-tag` overlay; refresh; update; `expect listings` installed ref v1.1.0; member `POST .../access` 202 `pending` | pending, `when.1`, product | W1 |
| H3 `a-hidden-agent-answers-like-a-missing-one-on-every-catalog-route` | new | process | org-admin, org-publisher as pub, org-publisher as pub2, org-member, agent-inline-probe, marketplace-access | pub releases and restricts; for member and pub2, pairs of hidden URN and `.../no-such-agent` with exact `fields.message` and status: `/versions`, `/outcomes`, `/release-suggestion`, `/permissions/effective?version=1.0.0`, `POST /v1/agent-checks`; then pub2 release 1.1.0 (404 pair), DELETE (405 pair); then admin installs quiet-notes and pub2 release, release-suggestion, DELETE, PUT on it vs `urn:vam:agent:access.community.example/x/no-such-agent` (400 pair) | pending, first release step, product | W2 |
| H4 `a-playbook-run-in-a-bound-space-is-admitted-as-the-spaces-agent` | new | process | org-admin, org-member, agent-plain, playbook-one-step | approval policy, request, approve, `POST /v1/spaces` remember id, revoke; `POST /v1/runs {playbookRef, spaceId}` 403 `agent_access_required:`; restrict; same 404 missing text | active | W0 |
| H5 `an-owners-draft-stays-usable-and-a-foreign-draft-pin-is-not-found` | new | process | org-admin, org-member as m1, org-member-email as m2, agent-member-draft-with-host-echo, skill-host-echo | m1 `put-draft`; admin policy `{restricted, approval}` on it; m1 chat `draft: true` succeeds; m2 `POST /v1/runs` pinned `1.0.0-draft` 404, equal to the same pin on a missing URN | active | W0 |
| H6 `catalog-and-key-scopes-never-make-an-agent-admin` | new | process | org-admin, org-publisher as pub, org-member-with-a-narrow-key as nk, org-member, agent-plain | admin approval policy, admins `[nk]`; member requests; pub: policy 403, admins GET and PUT 403, decide 404, chat 403; nk (no catalog scope) decides 200 and sets policy 200; member `login`, then `credential: session` read 200 (session carries the user's access) | active | W0 |
| H7 `a-demoted-or-deactivated-admin-is-handled-by-the-rule` | new | process | org-admin, org-publisher as pub, org-member, org-member-email as m2, agent-inline-probe | pub releases (seeded admin), approval policy; `set-role pub member`; member requests; pub decides 200, release 1.1.0 403 `publish:grant`; admin deactivates pub; `GET .../admins` `deactivated: true`; m2 requests; admin's `/v1/notifications?kind=access.requested` holds it | active | W0 |
| H8 `access-follows-the-agent-across-its-releases` | new | process | org-admin, org-publisher as pub, org-member, agent-inline-probe | pub releases 1.0.0, approval policy, member approved; pub releases 1.1.0; member runs pinned 1.0.0 and latest; revoke; both 403 | active | W0 |

Unit tests that stand in for scenarios: bad `access` values (vam-spec); `keys:admin` alone (AA-5.9);
hidden vs missing per route, org admin and Visible unchanged (AA-5.6).

#### C. Waves, items, gates, models

Gates (every wave, in its own worktree, builds never in parallel):
`cargo fmt --all --check`; `cargo test -p <crates touched>`; `cargo run -p vam-e2e -- check`;
`scripts/e2e/run.sh --scenarios <ids>`; `scripts/ci/local.sh --fast --base origin/feat/agent-access`;
`cd docs && pnpm build` when a page changes. Clippy only through `local.sh --full` or Actions:
`cargo +1.97.0 clippy --workspace --all-targets -- -D warnings`, and again with
`--features vam-runtime/process-guest,vam-orchestrator/process-guest` (AA-5.4 is
`cfg(feature = "process-guest")`). PR body `CI:` line lists the scenario ids (not H-numbers; the P3
lesson).

**W0 scenarios** (Sonnet low, ~60 min). H1 to H8, the `marketplace-access` fixture and overlay, README
piece entry. Gate: `vam-e2e check`; `run.sh --scenarios` all eight (H1, H2, H3 red at the named step,
class `product`; H4 to H8 green). Risks: `{{urn:<piece>}}` may cover agent pieces only (README
"Roles and drafts"), so H4 may need the playbook URN literal or a one-line runner extension; the
RunSpec JSON for `POST /v1/runs` (`agentRef.version.prerelease`, `playbookRef`) must match proto
names. `CI: scenarios=<eight ids>`.

**W1 manifest field** (Sonnet medium: `vam-spec` forces workspace tests; the install path). Turns
green H1, H2.
- AA-5.1 `vam-spec`: `Listing.access: Option<ListingAccess>` (`serde(default,
  skip_serializing_if)`), `ListingAccess { default: AccessDefault }` and `AccessDefault { visibility,
  mode }`, all `deny_unknown_fields`, lowercase enums; `listing_problems` refuses
  `(restricted, everyone)` with the route's wording; module doc example. Unit tests: absent, each
  valid pair, the refused pair, unknown value, `admins` key.
- AA-5.2 `marketplace/store.rs` `ListingState.access`; copy at `mod.rs:325-336`; `listings_view_with`
  adds `access`; OpenAPI property on the listing item.
- AA-5.3 `write_install_default_on` takes the pair (absent = `(org, approval)`); `import.rs` passes
  `listing.access`. Tests next to `import_tests.rs:567-640`: default applied; absent field; update
  never overwrites; agent added by an update gets it; pre-upgrade install keeps no row.
- AA-5.4 reset seam (`installs.rs:332-385`): delete `agent_access_policy` and `agent_admins` rows of
  the marketplace-tier agent URNs it deletes, same transaction; update the doc comment.
- AA-5.5 docs: `orchestrator/agent-access.md:61` (install default, the field, first install only);
  `getting-started/cli.md:443` one clause (validate checks `access.default`).
- Gate: `cargo test -p vam-spec`, `cargo test --workspace` (forced), `run.sh --scenarios` H1, H2,
  `a-member-must-request-an-agent-the-admin-installed`, `admin-installs-the-squrell-listing-at-its-tag`,
  `newer-tag-shows-update-available-until-the-admin-updates`. `CI: scenarios=... tests=workspace`.
- Risks: refresh reads the index at the head, the tag may differ; an index typo now fails the whole
  read (same as `requiresNode` today).

**W2 (sec) hardening** (Sonnet high: name leaks are the security floor). Turns green H3. Plan goes to
the owner.
- AA-5.6 one helper in `catalog_tier.rs`: split `reject_caller_installed_urn` into the installed check
  and `reject_reserved_domain`; `hidden_answer(conn, urn, noun, missing)` runs the domain check then
  returns the route's own missing error. Apply when the rule says Hidden for the caller (pass
  `has_users_admin`; org admins are never Hidden): release (`api/catalog_release.rs:33-52`, before
  `require_release_authorization`); release-suggestion agent arm (`release_suggestion.rs:36-47`);
  `refuse_catalog_delete` for agents (`api/mod.rs:711-726`, missing answer = the 405); PUT full and
  restricted (`catalog_publish.rs:1055-1150`, installed case only; plumb the flag from
  `api/mod.rs:595-622`). Unit tests: per route, hidden and missing give equal status, code and message;
  Visible and org admin keep 409 and 403.
- AA-5.7 `access.requested` CLI line: `vam access approve <request id>` in `notify_delivery.rs:1506-1509`
  and `notifications_tool.rs:162-166`, with a test each.
- AA-5.8 docs: rewrite "Not proved here" (`agent-access.md:100-102`) to the accepted PUT case and the
  §2.1 playbook and skill names; add H1 to H8 to `proves`; `docs-features.yaml` row: P5 proofs and
  `marketplace.json access.default` in `surface`.
- AA-5.9 unit test: `keys:admin` without `users:admin` is Hidden on a restricted agent.
- Gate: `cargo test -p vam-orchestrator`, `vam-e2e check`, `pnpm build`, `run.sh --scenarios` H3 plus
  all 27 agent access scenarios and the marketplace scenarios listed above. Risks: the PUT path holds
  the store lock, so the rule must use the `_on(connection)` variants; `noun` and texts must come from
  the existing constructors, never new strings.

**W3 vam-marketplace** (Sonnet low, after the VAM release and the vamd deploy). README "Add a listing"
step 2, the field table in `docs/reference-listing-notion.md:115-122`, `VAM_COMMIT` = the VAM release
merge commit (`validate.yml:27`). Gate: that repo's CI (builds `vam-cli` at the pin) green; review by
`@squrell-inc/marketplace` (`CODEOWNERS`).

**Phase close** (Opus high, read-only, ~30 min): the whole feature against design §7, issues 6 to 9,
release order. One fix wave (Sonnet medium) if needed. AA-5.10 records: tracker (Phase 2 to 5 closed,
done table, frozen P5 field with commit; PR #146 merged or closed), `design/agent-access.md` header and
`design/README.md:25` "built", `.cursor/workstreams/README.md:24`; delete the tracker and its README
row once the release merges (playbook §7).

| Wave | Items | Turns green | Model | Budget | Depends on |
|---|---|---|---|---|---|
| 0 | scenarios, piece | H4 to H8 (H1 to H3 red) | Sonnet low | 60 min | approval |
| 1 | AA-5.1 to 5.5 | H1, H2 | Sonnet medium | 90 min | 0 |
| 2 (sec) | AA-5.6 to 5.9 | H3 | Sonnet high | 90 min | 1 (H3 uses the piece and AA-5.4) |
| close | review, fix wave, AA-5.10 | all 8 active | Opus high, Sonnet medium | 60 min | 2 |
| 3 | vam-marketplace | none (CI) | Sonnet low | 30 min | VAM release, vamd deployed |

Adjusted from the tracker: W0 lands five already-passing proofs `active` (Q7); W2 depends on W1, not
only W0; W3 also waits for the vamd deploy.

#### D. Risks

- **Whole-index failure on older nodes.** Any `access` key in a live index stops every older node
  from reading it (issue 1). `requiresNode` cannot help: parsing fails first.
- **Scenario order.** Until AA-5.4 lands, a marketplace URN's policy carries across scenarios in one
  run; W0 must not change the policy of a URN another scenario installs.
- **Accepted residuals.** PUT tells a `catalog:write` holder a foreign URN exists (Q2); a hidden
  installed URN whose marketplace was removed equals a missing URN only because the same code runs.
- **Squrell-App** release still needs the `src-tauri` cargo test on a Tauri machine.
- **Public repos.** VAM and vam-marketplace are public; land W2 before the release so `main` never
  carries the "Not proved here" leak text.

#### E. What must not be touched

No change to the rule (`decide_access`, `agent_access.rs:212-232`), the frozen error texts (prefix
`agent_access_required:`, the existing not-found constructors), the design §6 JSON, the routes of
Phase 2, `orchestrator.proto` or the baseline, HSP brokers, or how marketplaces are trusted and
installed (`users:admin`, A3). No scenario `then` is edited; no locked decision changes. The vendored
`marketplace-squrell` fixture stays as is. Squrell-App is not touched in this phase except N6 if Q9
is accepted (its own lane).

#### F. Release order

1. W0 to W2 and the close fix wave merged into `feat/agent-access`; Actions green; tracker closed.
2. Owner's word: one release PR `feat/agent-access` into VAM `main` (full CI, release gate, merge
   commit). Re-check `git rev-list --left-right --count origin/main...origin/feat/agent-access` first.
3. Deploy vamd (staging, then production). Staging may try a branch index carrying `access` with
   `VAMD_SQURELL_MARKETPLACE_INDEX_REF` (`orchestrator/overview.md:104`).
4. Squrell-App: N6 and the `src-tauri` cargo test, then merge PR #23 (a push to `main` deploys the app).
5. W3: vam-marketplace PR (docs, pin), reviewed by `CODEOWNERS`.
6. Only once every node that reads the Squrell index runs the release may a live listing carry
   `access` (an owner decision per listing).
7. Team checklist from H1 to H8; archive the tracker.

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
| 3 | Owner: breakdown; wave plans delegated | Session per PR; CodeRabbit | Short Opus high read-only review at phase close (approved 2026-11-28): hints and fallbacks leak nothing vamd did not return; read-modify-write sends the intended set | Scenarios green; checklist | Dedicated security build wave |
| 4 | Owner: breakdown; wave plans delegated | Session runs the app's local gate (playbook §4: `CI=1 yarn test`, `tsc --noEmit`, `yarn e2e:typecheck`); Playwright desktop and mobile (Squrell-App `.cursorrules`) | None: the app hides nothing vamd does not | Owner's manual check of the chat flow (working-agreement "Rules that bit us"); then against a Phase 2 node | Opus review |
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
| 2026-11-27 | 2 W2 | `access.requested` (admins, else org admins and the operator; fixed title, the member's message never copied; decide clears every copy as `acted`, withdraw as `withdrawn`) and `access.decided` (approve, deny, revoke) written inside the replay transaction, so a replay emits nothing; `/v1/me` `adminOfAgents` and `pendingAccessRequests`; proto messages and baseline; OpenAPI for all 10 access routes; the Agent access docs page, catalog, notifications and api amendments, sidebar and feature row. N7, N8 `active`. An admin added after a request was raised gets no notification (queue and badge still show it). Scoreboard: N1–N8, P1 S1–S7 and the 10 marketplace scenarios 25/25 green (N1–N8 8/8), `cargo test --workspace` green, `vam-spec` green, `vam-e2e check` ok, `pnpm build` ok, clippy and fmt clean, 0 fix cycles; `local.sh --fast` stops at `install.test.sh` (environmental as root). Phase not closed: the phase-close review is next. |
| 2026-11-27 | 2 fix wave | Opus security review F1-F4 fixed. `PUT …/admins` now moves the open `access.requested` copies of the pending requests on the same transaction (removed admin's copy closes `expired`, new admin gets one, no admin left falls back to the org admins and the operator); `POST /v1/notifications/{id}/acted` clears a copy its holder can no longer decide; the requester is never a recipient; `access.decided` names a now-Hidden agent by its URN tail; OpenAPI gains the 403 (no `catalog:read`) on the access reads and writes and the full revoke 404 text. The W2 caveat "a late admin gets no notification" is gone. Scoreboard: N1–N8, P1 S1–S7 and the 10 marketplace scenarios 25/25 green, `cargo test -p vam-orchestrator` 2742 passed 0 failed, `vam-spec` green, `vam-e2e check` ok, `pnpm build` ok, clippy and fmt clean, 0 fix cycles, scenarios 4m44s. Phase not closed. |
| 2026-11-28 | 3 W2 | AA-3.3 (`vam agent admins ls\|add\|rm`, `vam users ls`), AA-3.4 (`vam access requests\|approve\|deny\|revoke`) and AA-3.6 (CLI docs, feature row without `pending_page`) on `feat/agent-access-p3-w2`. C2 and C3 `active`; C1 to C4 active. `cargo test -p vam-cli`, clippy, `vam-e2e check` and `pnpm build` green locally; scenarios run in Actions. |
| 2026-11-28 | 3 W0-W1 | Scenarios C1-C4 (pending) and the runner `exitCode` and placeholders (#147); paging fix, member commands and hints (#148). Note: the CI line takes scenario ids, not C1-C4, so #148 ran only `decision-anywhere-clears-the-notification`; the four CLI scenarios first run on #149 with their real ids. |
| 2026-11-28 | 3 review, fix wave and close | Opus close review of the merged Phase 3 diff; findings fixed on `feat/agent-access-p3-fix`: exact user id beats any username, `/v1/users` read only for org admins and only 403/404 swallowed, admin list re-read right before the PUT, hint text and URN lists cleaned of control and format characters, a short-name locked run fails with 403 like the URN path and points at `vam access mine` for a pending request, path segments encode UTF-8 bytes and dots. Phase 3 closed. |
| 2026-11-28 | 4 | Squrell-App PRs 18-22 merged into `feature/agent-access`: browse with status chips, request dialog, My requests, admin review queue with revoke, `openAgentChat` gate, 403 mapping, errors shown not swallowed, Tauri commands. Release PR 23 into `main` held until vamd is deployed. Open: app nit N6 and the `src-tauri` cargo test (gate PR 23); N4, N5, N7 to the app backlog. |
| 2026-11-28 | 5 | W0 scenarios H1-H8 (#152), W1 `access.default` in `marketplace.json` with first-install-only application and the reset-seam fix (#154), W2 hidden-equals-missing on release, release suggestion, DELETE and PUT, `vam access approve` hint, docs (#155), then the Opus whole-feature review (0 Critical, 1 Important, 9 Nits; `p5-close-security-review.md`) and its fix wave on `feat/agent-access-p5-fix` (restricted `PUT` on a hidden orphan agent answers the missing-URN 400, Slack fallback escape, records). Owner answers Q1-Q9 as recommended. Scoreboard: 8 new scenarios active and green (27 agent access scenarios in all), Actions green on #154 and #155, 0 fix cycles in the waves, 1 review fix wave. Frozen: `listing.access.default {visibility, mode}`, no admins. Accepted residuals: `PUT` by a `catalog:write` holder shows a foreign or installed URN exists. Backlog: install binds `ref` but not `access` (N3); older nodes reject an index carrying `access` until all upgrade (no live listing may carry it before that). Open: W3 (vam-marketplace docs and pin) after the VAM release and vamd deploy. |
