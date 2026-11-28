# Agent access: per-agent visibility, Add and Request, agent admins

Proposal for open work, 2026-11-26; built in Phases 0-5 on `feat/agent-access`, release pending. Decisions A1–A9 were **approved by the owner on
2026-11-27 with the recommended answers** (§2.4). Tracker: `.cursor/workstreams/agent-access.md`. Branch: `feat/agent-access`. What the
node does today is in `docs/src/content/docs/orchestrator/catalog.md`, `orchestrator/member-agents.md`
and `orchestrator/user-isolation.md`.

Read only the section for the phase in hand: §1–§2 for everyone, then §3 (Phase 1), §4 (Phase 2),
§5 (Phase 3), §6 (Phase 4), §7 (Phase 5).

---

## 1. Problem and evidence

Goal: an agent can be hidden from members by default; a member **Adds** an open agent (granted at
once) or **Requests** an approval agent; each agent has its own **agent admins** who approve or
deny; an agent is usable (run, session start, Space binding, grants) only once granted; org admins
are implicitly admins of every agent.

It is built in `vamd` (`crates/vam-orchestrator`), not in an HSP broker. Brokers (`brokers/vam-web`,
`vam-notify`, `vam-schedule`) are called by vamd over HSP; they have no user API, users table or
ACLs, and only receive the `user_id` vamd stamps into the caller context. Access is a property of
the catalog, which only vamd holds.

| Fact today | Where |
|---|---|
| Catalog visibility is one predicate: released rows for everyone, drafts for their owner | `catalog.rs:367-369` (`catalog_visible_where_sql`), `catalog.rs:56-72` (`ensure_draft_visible`) |
| `GET /v1/agents` lists every released agent plus own drafts; `users:admin` sees foreign drafts too | `api/mod.rs:796-813`, `catalog.rs:934-958` |
| Every agent read goes through three store calls: `get_agent_record_visible`, `get_agent_at_version_visible`, `list_agent_records_visible` (`include_foreign_drafts` = `users:admin` at API sites) | `catalog.rs:821-898` |
| Run admission loads the agent once, via `load_agent_for_run` | `catalog_resolve.rs:43-55`, called at `api/run_start.rs:359-376` |
| Triggered and scheduled runs go through the same `start_run` | `gateway.rs:24-25,709` |
| Legacy `POST /v1/sessions` loads the agent through `load_agent_for_run` too | `manager.rs:1119` (`handle_create_session`), `manager.rs:1236` |
| `run_start` binds an unbound Space to the requested URN **before** it loads the agent | `api/run_start.rs:333-350` (`bind_space_agent`) then `:359` |
| `POST /v1/spaces {agentUrn}` checks `get_agent_visible` | `api/spaces.rs:146-149` |
| Run grants check `get_agent_visible` for a URN other than the calling agent, non-leaky refusal `agent_not_found: …` | `grants.rs:130-132,230-243` |
| Composition and permissions views load via the same visible calls | `api/composition.rs:72-76`, `permissions_view.rs:634-660`, `api/permissions.rs:68` |
| Version history: released rows for every reader | `catalog.rs:575-615` (`query_catalog_versions`) |
| Agent sub-routes are one wildcard; `/versions`, `/outcomes`, release suggestion, `/permissions` are parsed from `rest` | `api/mod.rs:174-179,620-683` |
| Inline agents in a run are refused, so the catalog is the only source | scenario `a-run-with-an-inline-agent-is-refused-and-writes-nothing` |
| Dependency refs resolve by owner, never by caller visibility of the agent; one uniform message for any non-release | `catalog_resolve.rs:382-391` (`resolve_ref_for_owner`), `:282-289` (`not_released`) |
| Release freezes every dependency to version and digest | `catalog_freeze.rs:1-17,63-110` |
| Roles: `Operator`, `OrgAdmin`, `Publisher`, `Member`; no reviewer role | `users.rs:73-83` |
| Nearest approver hook: `HitlApprover::{Owner, Reviewer}`, `Reviewer` typed but never constructed | `hitl.rs:34-42` |
| Authorization is scope-only (`keys:admin` or the named scope), no per-resource ACL | `api/auth.rs:79-96` |
| Error body is `{code, message, retryable}`, `code` = `VamErrorCode` name (`PermissionDenied`, `NotFound`…); no custom codes | `api/error.rs:17-28`, `proto/vam/v1/common.proto:151-169` |
| `/v1/marketplaces` routes all need `users:admin` | `api/marketplaces.rs:1-12,72,140,227` |
| Installed rows are released rows owned by `marketplace:<id>`, so visible to the whole org | `marketplace/installs.rs:20-25` |
| Notifications: one row per `(user_id, kind, subject_key)`, fields `agent_urn`, `action {verb, target:{kind,id}, api}`; kinds are a closed enum | `notifications.rs:49-96,180-237` |
| Paging: `pageSize` 1..1000, opaque `pageToken` | `api/paging.rs:1-75` |
| Decisions this changes | `design/connectors.md` D4 (line 26), D27 (line 47, §9.2) |

---

## 2. Model (proposed)

### 2.1 Scope: gate agents only (A1)

Only agents carry access. Playbooks, skills and policies inherit from the agent that uses them: a
release freezes them by digest (`catalog_freeze.rs`), and `resolve_ref_for_owner` never checks the
caller against a dependency. **Check once, at agent admission, never per dependency.** Connectors,
org MCP connections, models, secrets and brokers stay org-admin gated; per-group gates are backlog.

Consequence to accept: a playbook or skill a hidden agent uses still shows in `GET /v1/playbooks` /
`GET /v1/skills` (released rows). Its name is not secret; the agent is.

### 2.2 Tables

```sql
CREATE TABLE agent_access_policy (
  urn          TEXT PRIMARY KEY,
  visibility   TEXT NOT NULL CHECK (visibility IN ('org','restricted')),
  access_mode  TEXT NOT NULL CHECK (access_mode IN ('everyone','open','approval')),
  updated_by   TEXT NOT NULL, updated_at INTEGER NOT NULL
);
CREATE TABLE agent_admins (
  urn TEXT NOT NULL, user_id TEXT NOT NULL REFERENCES users(user_id),
  added_by TEXT NOT NULL, added_at INTEGER NOT NULL, PRIMARY KEY (urn, user_id)
);
CREATE TABLE agent_access (
  request_id   TEXT PRIMARY KEY,            -- minted id, e.g. 'acr_…'
  urn TEXT NOT NULL, user_id TEXT NOT NULL REFERENCES users(user_id),
  state        TEXT NOT NULL CHECK (state IN ('pending','approved','denied','revoked','withdrawn')),
  message TEXT, requested_at INTEGER NOT NULL,
  decided_by TEXT, decided_at INTEGER, reason TEXT
);
CREATE UNIQUE INDEX agent_access_live ON agent_access(urn, user_id)
  WHERE state IN ('pending','approved');
```

`agent_access` keeps history (one live row per user and agent, older rows stay for audit). Policy
is per URN, not per version.

**No policy row = `(org, everyone)`**, today's behaviour. Upgrade needs no backfill and nothing
disappears (A5).

### 2.3 The one rule

`agent_access(store, urn, caller) -> Hidden | Visible { state } | Usable`:

1. **Usable** if any: `access_mode = everyone` (and visibility `org`); caller holds `users:admin`
   (org admin, the same flag as `include_foreign_drafts`); caller is in `agent_admins` for the URN;
   caller owns the agent's draft row (`agent_draft_owner`, `catalog.rs:851`) and the row read is
   that draft; caller has an `approved` row.
2. Else **Visible** if `visibility = org` (state = the caller's live row, or none).
3. Else **Hidden**.

| Outcome | List `GET /v1/agents` | Browse `GET /v1/catalog/agents` | Any admission site |
|---|---|---|---|
| Usable | listed | listed, `access.state = approved` | allowed |
| Visible | **not listed** | listed with state | 403 `PermissionDenied`, message `agent_access_required: …` |
| Hidden | not listed | not listed | 404 with the existing not-found text |

Hidden must be indistinguishable from "no such agent": same status, same message as a missing URN
(the `not_released` / `grants.rs` style). The 403 machine marker is a message prefix, as grants
already do (`grants.rs:130`), because the error envelope has no custom codes (A8).

### 2.4 Decisions (approved by the owner, 2026-11-27)

| # | Question | Answer (locked) |
|---|---|---|
| A1 | Gate agents only; dependencies inherit; connectors, org MCP, models, secrets, brokers stay org-admin | Yes (§2.1) |
| A2 | Amend `connectors.md` D4 (admin enables marketplace items) and D27 (org releases visible to the whole org) | Amend both: D4 "admin installs, then each agent's policy decides who sees and uses it"; D27 "visible to the whole org unless its access policy restricts it" |
| A3 | Install stays node-wide; per-user grant sits on top | Yes; install (`/v1/marketplaces/…/install`) is unchanged and `users:admin` only |
| A4 | Agent admins are rows in `agent_admins`, not a new `UserRole` | Yes: roles are node-wide, admin is per agent; org admins are implicit |
| A5 | Defaults: no row = `(org, everyone)`; new marketplace installs default `(org, approval)`; the org's own releases default `(org, everyone)` | Yes (keeps D27 for org releases; "hidden by default" applies to installs) |
| A6 | Who becomes an agent admin first | The user who first releases a URN is seeded as its admin; installs have none (org admins only) |
| A7 | Does a revoked grant stop scheduled and triggered runs | Yes, checked at each start (they all pass `start_run`); a running session is not killed |
| A8 | 403 shape for visible-but-not-granted | `PermissionDenied`, message prefix `agent_access_required:`; no new `VamErrorCode` |
| A9 | Marketplace manifest field | `access.default {visibility, mode}` only; **no `admins`**: user ids are node-local, a public index cannot name them |

### 2.5 Rejected

- **Gate in an HSP broker.** Brokers have no users, no catalog and no API; the run is admitted
  before any broker is called.
- **Per-dependency checks** (playbook, skill, policy). Releases freeze dependencies by digest; a
  check per ref adds a failure mode with no protection, since a hidden agent's parts are named in
  public lists anyway.
- **A new `UserRole::AgentAdmin`.** One role per user cannot express "admin of agent X only".
- **Reusing `HitlApprover::Reviewer`.** It is a Cedar principal for run-time tool approvals; access
  requests are control-plane records with no run. Keep the shape (decide + reason), not the type.
- **Only two modes (`open|approval`).** Without `everyone`, existing agents would drop out of
  `GET /v1/agents` for every member on upgrade until each one is Added.
- **Cedar policy per agent.** The PDP is scope-only today (`api/auth.rs:79`); a table query at one
  site is smaller and testable.

---

## 3. Phase 1: storage, the rule, enforcement

**Where to enforce.** Every agent admission already calls one of three store functions
(`get_agent_record_visible`, `get_agent_at_version_visible`, `list_agent_records_visible`,
`catalog.rs:821-958`). Put the rule there (list: join to `agent_access_policy`, `agent_admins`,
`agent_access` in SQL; get: one helper after `ensure_draft_visible`). That covers, by construction:

| Site | Call |
|---|---|
| `GET /v1/agents`, `GET /v1/agents/{urn}` | `api/mod.rs:796`, `:636-683` |
| run start, standalone run with a bound Space, workspace version pin | `api/run_start.rs:359-376` |
| triggered and scheduled runs | `gateway.rs:709` → `start_run` |
| legacy session create | `manager.rs:1236` |
| `POST /v1/spaces` | `api/spaces.rs:146-149` |
| run grants for another agent | `grants.rs:237` |
| composition check, permissions view | `api/composition.rs:72-76`, `permissions_view.rs:634` |

Not covered by those calls, handle explicitly:

- **Version history** (`query_catalog_versions`, `catalog.rs:575`): add the rule for `agents`.
- **Outcomes and release suggestion** under the agent wildcard (`api/mod.rs:650-662`): check
  before answering.
- **Space bind before admission** (`api/run_start.rs:333-350`): move the agent check before
  `bind_space_agent`, or a refused run leaves a Space bound to a hidden URN.
- **`include_foreign_drafts`**: org admins stay Usable; grants and manager pass `false` and the
  session owner, so the owner's grant decides.

New error variant `OrchestratorError::AgentAccessRequired { urn }` → 403 (`api/error.rs`).
`AgentHidden` maps to the existing `NotFound` text.

**Scenario delta (draft for the breakdown).** World pieces exist: `org-admin`, `org-publisher`,
`org-member`.

| Scenario | Layer | Proves |
|---|---|---|
| `a-member-cannot-see-or-run-a-restricted-agent` | process | list lacks it, GET 404, run 404, `/versions` 404, Space create 404 |
| `a-member-requests-an-agent-an-admin-approves-and-the-member-runs-it` | process | 403 `agent_access_required` before; request; approve; run 200 |
| `a-member-adds-an-open-agent-and-runs-it-at-once` | process | Add → approved → run |
| `a-revoked-member-cannot-start-a-scheduled-run` | process | grant-fired run refused after revoke |
| `existing-agents-stay-usable-after-upgrade` | process | no policy row, member lists and runs as before |

Phase 1 writes rows through a store API used by tests; routes come in Phase 2. If the scenarios
need HTTP to set up, Phase 1 may ship `PUT …/access-policy` and the decide route early.

---

## 4. Phase 2: routes, OpenAPI, proto, notifications

All under `/v1`; writes need `Idempotency-Key`; errors as `api/error.rs`. Agent sub-routes are
parsed inside the `/v1/agents/{*rest}` wildcard (`api/mod.rs:174`), like `/versions`.

| Method, path | Who | Answer |
|---|---|---|
| `GET /v1/catalog/agents?pageSize&pageToken` | any `catalog:read` | browse stubs (§6) for Visible and Usable agents |
| `POST /v1/agents/{urn}/access {message?}` | caller | `open`: 200 approved; `approval`: 202 pending; 409 if a live row exists; 404 if Hidden |
| `DELETE /v1/agents/{urn}/access` | caller | withdraw pending or leave approved; 204, no body |
| `GET /v1/me/access?pageSize&pageToken` | caller | own rows, all states, paged |
| `GET /v1/access-requests?state=pending&adminOf=me&pageSize&pageToken` | agent admin, org admin | requests for agents the caller administers |
| `POST /v1/access-requests/{id}/decide {decision: approve\|deny, reason}` | agent admin, org admin | 200; `reason` required on deny (400); 409 if not pending |
| `POST /v1/agents/{urn}/access/{userId}/revoke {reason}` | agent admin, org admin | 200 |
| `GET`, `PUT /v1/agents/{urn}/admins` | read: admins; write: admins, org admin | `{admins: [{userId, username, addedBy, addedAt, deactivated?}]}` (§6) |
| `PUT /v1/agents/{urn}/access-policy {visibility, mode}` | agent admin, org admin | 200 |
| `GET /v1/me` | caller | adds `adminOfAgents: string[]`, `pendingAccessRequests: number` |

Who may administer is the rule in §2.3 for "admin", never `catalog:write`, `publish:grant` or
`keys:admin` alone (`keys:admin` passes every scope check in `pdp.authorize`, `api/auth.rs:85`; the
admin check must not use `pdp.authorize`).

**Notifications.** Two new `Kind`s in `notifications.rs:49`: `access.requested` (to each agent admin
and, when the agent has none, to org admins; severity `action`, `is_decision`, cleared by the
decide) and `access.decided` (to the requester, `info`). `agent_urn` set; `action =
{verb, target: {kind: "access_request", id}, api: "/v1/access-requests/{id}/decide"}`. Delivery to
Slack follows through `vam-notify` with no broker change. There is no `relatedUrn` field; use
`agent_urn`.

**Docs and contracts.** `docs/api/openapi.yaml` (hand-written, served at `/openapi.yaml`,
`api/mod.rs:88`); messages in `proto/vam/v1/orchestrator.proto`; regenerate the baseline with
`cargo run -p vam-spec --example generate_baseline` (`proto/vam/v1/baseline/vam_descriptor.bin`,
checked by `crates/vam-spec/tests/conformance.rs`). Docs page `orchestrator/agent-access.md` with
`status` and `proves` frontmatter; amend `orchestrator/catalog.md`.

---

## 5. Phase 3: CLI

`vam marketplace add` already means "trust a Git marketplace" (`crates/vam-cli/src/cmd/marketplace.rs:18`),
so access lives under `vam agent` (`main.rs:88`, `cmd/catalog.rs`) and a new `vam access`:

- `vam agent browse [--all]`, `vam agent add <urn>`, `vam agent request <urn> [-m msg]`,
  `vam agent leave <urn>`
- `vam agent admins ls|add|rm <urn> [user]`, `vam agent access-policy <urn> --visibility --mode`
- `vam access mine`, `vam access requests [--all]`, `vam access approve <id>`,
  `vam access deny <id> --reason`, `vam access revoke <urn> <user> --reason`

Templates: `cmd/approve.rs`, `cmd/notifications.rs`. Writes through `post_raw_json` (mints
`Idempotency-Key`, `client.rs:237`). **`list_paged` appends `?pageToken=`** (`client.rs:560`), so it
breaks on `/v1/access-requests?state=pending`; fix it to use `&` when the path has a query. Hints in
`http_error.rs` (403 `agent_access_required` → "run `vam agent request <urn>`"). Exit codes are
already 403→3, 404→4, 409→5 (`output.rs:110-114`).

---

## 6. Phase 4: Squrell-App, and the frontend contract

**Frozen (2026-11-27, Phase 2 breakdown approved by Roshan).** The shapes below, with the amendments at the end of this section, are the contract; changing one needs the owner. Phase 4 can start against mocks. Today (Squrell-App `main`):
`services/apiClient.ts` is hand-rolled, no react-query or generated client, polling only;
`MarketplaceAgentsList` renders `GET /v1/agents` (`services/agents.ts:564` `fetchLiveAgents`);
`openAgentChat` (`context/WorkspaceChatContext.tsx:1171`) has no access gate; `MeInfo`
(`types/vam.ts:23`), `hasAdminScope` (`types/vam.ts:59`); `HitlActionCard`, `NotificationsPanel`
exist; many services swallow errors and return `[]`, which would show a 403 as "no agents".

Needed: `services/agentAccess.ts` + hooks; Marketplace Agents tab reads `GET /v1/catalog/agents`;
status chip `hidden | available | pending | denied | approved`; request dialog; "My requests";
admin queue; `openAgentChat` and run errors map 403 `agent_access_required:` to the request dialog.

**Stable JSON shapes** (camelCase, as every vamd JSON response):

```jsonc
// GET /v1/catalog/agents
{ "agents": [ {
    "urn": "urn:vam:agent:…", "displayName": "…", "description": "…",
    "catalogTier": "user|platform|marketplace", "latestVersion": "1.2.0",
    "visibility": "org|restricted", "mode": "everyone|open|approval",
    "access": { "state": "none|pending|approved|denied|revoked",
                "requestId": "acr_…", "decidedAt": "RFC3339", "reason": "…" },
    "canAdmin": false } ],
  "pagination": { "nextPageToken": "…" } }

// POST /v1/agents/{urn}/access  -> 200 | 202
{ "request": AccessRequest }

// AccessRequest (GET /v1/me/access -> {requests:[…]}, GET /v1/access-requests -> {requests:[…], pagination})
{ "requestId": "acr_…", "urn": "…", "agentDisplayName": "…",
  "userId": "…", "username": "…", "state": "pending|approved|denied|revoked|withdrawn",
  "message": "…", "requestedAt": "RFC3339",
  "decidedBy": "…", "decidedAt": "RFC3339", "reason": "…" }

// POST /v1/access-requests/{id}/decide  {"decision":"approve|deny","reason":"…"} -> {"request": AccessRequest}

// GET /v1/me additions
{ "adminOfAgents": ["urn:…"], "pendingAccessRequests": 3 }

// 403 at any admission site
{ "code": "PermissionDenied", "message": "agent_access_required: request access to 'urn:…'",
  "retryable": false }
```

UI `status` from `access.state`: `none` → `available`, `approved` or `mode=everyone` →
`approved`; a 404 for a URN the app knew → `hidden`.

**Amendments frozen by Phase 2 (2026-11-27).** Where they differ from the sketches above, these win.

- Conventions: camelCase, RFC3339 UTC with seconds. Unset optionals are omitted. Booleans and
  `access.state` are always present. `pagination.nextPageToken` is `""` on the last page.
- **Browse** `GET /v1/catalog/agents?pageSize&pageToken` returns released agents only, sorted by URN.
  `access.state` is one of `none|pending|approved|denied|revoked`. An agent usable for any reason
  shows `approved`; `withdrawn` or no row shows `none`. `requestId`, `decidedAt` and `reason` appear
  only when a row backs the state.
- **AccessRequest**: as above; `agentDisplayName` is omitted when the agent is now hidden from the
  reader.
- `GET /v1/me/access` returns `{requests, pagination}` (paged, newest first).
  `GET /v1/access-requests?state=pending|approved|denied|revoked|withdrawn|all&adminOf=me` returns
  `{requests, pagination}`, newest first.
- `DELETE /v1/agents/{urn}/access` returns 204 with no body.
- **Admins**: `GET /v1/agents/{urn}/admins` returns
  `{admins: [{userId, username, addedBy, addedAt, deactivated?}]}`; `PUT` with
  `{userIds: [...]}` replaces the list and returns the same shape.
- **`/v1/me`**: `adminOfAgents` (explicit admin rows only) and `pendingAccessRequests` (requests the
  caller can decide). Both are absent when empty or zero.
- **Replay**: the same `Idempotency-Key` and the same request return the first 2xx answer byte for
  byte. The same key with another request returns 409 `Conflict`, `Idempotency-Key was already used
  for a different request`.
- **Notifications**: `access.requested` (action, decision, subject = request id,
  `action.target = {kind: access_request, id}`, `api: "POST /v1/access-requests/<id>/decide"`) and
  `access.decided` (info). `resolvedReason` gains `withdrawn`.
- Unchanged from Phase 1: the 403 body, Hidden = the route's missing-URN text, the policy and
  request bodies.

---

## 7. Phase 5: marketplace manifest and hardening

**Manifest.** Optional per-listing `access: { default: { visibility, mode } }` in
`vam-marketplace/marketplace.json`, parsed with `requiresNode` (`marketplace/import.rs:156`),
copied into `agent_access_policy` on first install only; a later update never overwrites a policy
an admin changed. No `admins` (A9).

**Pitfalls to test:**

- Name leaks: list, `/versions`, outcomes, release suggestion, composition, permissions view, and
  error text (a Hidden answer must equal a missing URN's, byte for byte).
- Standalone `playbookRef` with a bound Space: the agent is still admitted first.
- Draft runs: an owner's draft stays Usable to its owner; a foreign `-draft` pin stays NotFound.
- Credential confusion: the rule reads the caller's `user_id` and `users:admin`, never
  `keys:admin` or `catalog:write` alone; API keys of a user carry that user's access.
- A grant pinned to a version that is later yanked or superseded: access is per URN, not version.
- Per-agent admin vs `catalog:write`/`publish:grant`/`keys:admin`: none of these implies agent
  admin; an agent admin needs no catalog scope to decide.
- Demoted or deactivated owner or admin: admin rows survive a role change; a deactivated user's
  rows are ignored by the rule and listed for cleanup.
- Space bound before admission (§3).

## 8. Backlog

- Per-group gates for connectors, org MCP connections and models.
- Groups (grant an agent to a group, not one user).
- Expiring grants; request reminders; email and push for `access.*` (notifications slice 3).
- Primary agent (`.cursor/workstreams/README.md` backlog): its delegate list must use this rule.
