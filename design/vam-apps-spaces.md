# Apps and Spaces: who a run is for, and where it lives

**Status:** Proposed (replaces an earlier App/Subject draft); amended 2026-10-29 (run grants and opaque Space and thread ids for the E5 slice)
**Date:** 2026-10-09
**Owner:** maintainer
**Repos:** `squrell-inc/VAM`

## Decision record

**Context.** VAM has one caller shape: a `User` with API keys, owning
`{owner}--{suffix}` workspaces (`vam-storage/src/owner_prefix.rs`). That
breaks in four places:

1. A website using one key puts every visitor in one workspace with one writer
   lock, and every visitor runs with the developer's full authority and
   secrets.
2. Nothing is ever cleaned up (Fact 3).
3. "Which agent runs here" is a storage detail (`WorkspaceMeta` in
   `.vam/workspace.json`) surfaced only as `AgentWorkspaceMismatch` /
   `WorkspaceNotBound` errors. There is no named, deployed agent (DX-90) and
   no home for the planned "primary agent" or default workspace/thread.
4. Brokers (`docs/src/content/docs/hosted/triggered-runs.md`) need to act for someone without
   inventing identity.

An earlier draft proposed `App` and `Subject` as new identities. Review found
that a Subject never logs in, never calls VAM, and has no role: it is a place,
not an identity. Modelling it as a principal invites identity features VAM must
not have, and ties it to "a visitor" when brokers need channels, tickets, and
schedules too.

**Decision.** Five concepts, one rule each:

| Piece | Plain meaning | Example |
| --- | --- | --- |
| **Account** | Who owns things, pays, and approves | Alice |
| **App** | An agent set up for use, with its own rules and limits | "Alice's assistant", "Support bot" |
| **Space** | One private place where work and chat history live | Alice's `assistant` workspace; one Slack channel |
| **Thread** | One conversation inside a Space | `main`, `daily-report` |
| **Credential** | A key that lets something other than a logged-in person ask for work | API key; website key; broker run grant |

An Account owns Apps. An App has Spaces. A Space has Threads. A Credential says
who may ask, and for which App.

> Personal use stays zero-config. Sharing is just a second kind of App.

## Audience

Two groups, one model:

- **People using VAM themselves** (CLI, chat app, scheduler). They never need
  to know Apps exist: a personal App is created the first time they run an
  agent, and the "primary agent" is their default App.
- **Developers exposing an agent to others** (website, Slack bot, Mast). They
  create a shared App, give their backend or broker a Credential, and each of
  their users, channels, or tickets gets a Space. VAM never learns who those
  people are.

## What we are building

1. **One Credential type** covering API keys, App keys, and broker run grants,
   built by extending `api_keys`.
2. **Apps**: personal (auto-created) and shared; Account default App.
3. **Spaces**: one Drive9 workspace each, found by `(app, ref)`; existing
   workspaces become Spaces with unchanged IDs.
4. **One way to start a run**: `start_run` resolves App → Space → Thread the
   same way for HTTP and the HSP gateway.
5. **App authority**: a forbid-only policy layer and secret bindings.
6. **Retention and limits**: idle reaping (≤ 30 days), erasure, per-App
   quotas.

## Why

- A place keyed by an opaque `ref` is all VAM needs to isolate work. The ref
  can be anything, which is what brokers need.
- One Credential type means one hashing, revoke, list, and audit path
  (`keys.rs`) instead of three.
- Making the App the owner of the agent pin turns today's runtime binding
  check into structure, and gives DX-90 its "deployed agent".

## What we are not building

- Spaces as identities: no login, roles, or Cedar principal for a Space.
- Storing who a Space is for. `ref` is opaque, never interpreted.
- Org accounts (Account = User for now; `tenant_id` unchanged).
- Per-App HITL approver; Apps with more than one agent.
- Environment promotion and rollback (DX Phase 8).
- Usage rollup and spend ceilings.
- Space-scoped secrets / customers' own credentials.
- Read-only App-level shared mount.
- Broker development (see `docs/src/content/docs/hosted/triggered-runs.md`).

## Amendment 2026-10-29: run grants and opaque ids for the E5 slice

Written by a design session against `main`. It is the plan of
record for the parts of this design that E5 Phase 1 ("grants, gateway and
ids") builds; the phases are in the `work-outside-chat` tracker. Apps, shared Spaces, App authority and retention
stay as designed and stay cut from 0.1.0 (0.1.0 launch scope). Where
`main` contradicts an earlier section, `main` is the fact. Locked rows this
touches are in §6, as proposals for the owner.

### 1. What `main` has today

- **No Credential columns.** `api_keys` is `key_id, name, sha256_secret,
  principal, scopes, created_at, last_used_at, revoked_at` plus
  `owner_user_id` (`crates/vam-orchestrator/src/keys.rs:142-152, 199-208`).
  Bearer auth looks the hash up and refuses a revoked key
  (`keys.rs:423-470`); there is no holder, target or expiry. Secrets come
  from `mint_api_key_secret`, which is not a CSPRNG (`keys.rs:175-189`).
- **No Spaces, Apps or thread ids.** A workspace id is
  `{owner}--{agent_key}--{name}`, or the legacy `{owner}--{name}` kept for
  the agent it is bound to (`run_context.rs:128-156`,
  `vam-storage/src/owner_prefix.rs:80-124`, HF-2). A thread is a caller-chosen
  name (`RunSpec.thread_ref = 9`, `proto/vam/v1/workflow.proto:223`;
  `RunStatus.thread_ref = 16`, `:355`), checked by `sanitize_thread_id`
  (`vam_dir.rs:210-231`), stored owner-prefixed (`run_context.rs:158-165`)
  and used as its directory `.vam/threads/<prefixed name>/`
  (`api/mod.rs:3465`, `vam_dir.rs:233-239, 359-389`). `ThreadMeta` holds
  `thread_id` (that directory key), agent, parent, fork turn, log info
  (`vam_dir.rs:144-157`). The thread API is
  `GET /v1/workspaces/:id/threads[/:thread[/messages]]`
  (`api/threads.rs:41-48`). There is no `GET /v1/workspaces` list.
- **The rules this design keeps** are in code: one workspace holds many
  threads; each run names one thread; one live task per workspace
  (`api/inbox.rs:58-91`); a run for another thread on a busy workspace is
  queued (`api/inbox.rs:388-410`, E10 Phase 3 owner decision D). A queued
  run is started later from its stored request with the workspace re-resolved
  from its **name** (`inbox.rs:205-214`).

### 2. Credentials: run grants first

E5 Phase 1 builds AS-1.1 to AS-1.4 with one non-`http` holder in use, the
broker run grant. User API keys keep working unchanged (NULL columns read as
holder `http`, target Account).

- **AS-1.1** `api_keys` gains nullable `holder`, `target_kind`, `target_id`,
  `constraints_json`, `issued_via`, `external_ref`, `expires_at`, and, for
  provenance, `source_run_id`, `tool_name`. The grant's values are in
  ``docs/src/content/docs/hosted/triggered-runs.md`.
- **AS-1.2** `check_credential(secret, transport)`: hash, holder matches the
  transport (`http`, or `broker:<id>` on that broker's gateway socket), not
  revoked, not expired; returns owner, target, permissions, constraints.
  HTTP bearer auth refuses every non-`http` holder with 401.
- **AS-1.3** List and revoke through the existing `/v1/keys` routes
  (`api/mod.rs:102-103`) with `?holder=` and the new columns in the
  response; `vam keys list --grants`, `vam keys revoke`. Owner-scoped.
- **AS-1.4** Tests as designed, plus: a grant token on HTTP is 401; a grant
  presented on another broker's socket is `DENIED WRONG_BROKER`.
- New grant and id secrets use `rand::rngs::OsRng`. Moving user API keys
  off `mint_api_key_secret` is a separate security item, not in this slice.

The "Scheduler grant" row of the Credentials table reads, from this
amendment: holder `broker:vam.schedule`, target one Space, constraints the
bound thread and the approved run request (agent, message, playbook,
inputs, on-busy), permission `runs:start`, `issued_via invoke`, expiry
always set.

**Open question 4 (grant expiry): answered.** Default 90 days, cap 365; a
one-off schedule's grant at most its fire time + 24 h; renewal is a re-bind
the user approves; `grant.expiring` is emitted 7 days ahead
(`docs/src/content/docs/hosted/triggered-runs.md`).

### 3. Spaces before Apps

A grant must bind a place that cannot be swapped by reusing a name. E5
therefore needs Space ids now, while Apps are cut. Interim model: **the
personal App is implicit as `(owner_user_id, agent_urn)`**, which is exactly
the key AS-2.2 would give it.

```sql
CREATE TABLE spaces (
  space_id TEXT PRIMARY KEY,          -- "sp_" + 26 chars base32, CSPRNG; opaque
  app_id TEXT,                        -- NULL until Apps ship; AS-2.4 backfill fills it
  owner_user_id TEXT NOT NULL,
  agent_urn TEXT,                     -- the bound agent; NULL until the first run binds it
  ref TEXT NOT NULL,                  -- the label: today's workspace name ("assistant")
  workspace_id TEXT NOT NULL UNIQUE,  -- the Drive9 workspace, unchanged
  default_thread_id TEXT,
  threads_indexed_at INTEGER,         -- when the thread backfill last read HEAD
  created_via TEXT NOT NULL CHECK(created_via IN ('run','backfill','grant')),
  created_by_grant_id TEXT,
  created_at INTEGER NOT NULL,
  last_active_at INTEGER NOT NULL
);
CREATE UNIQUE INDEX spaces_owner_agent_ref ON spaces(owner_user_id, IFNULL(agent_urn, ''), ref);
```

- **Workspace ids never change.** `spaces.workspace_id` points at the
  existing Drive9 workspace; a legacy `{owner}--{name}` workspace and an
  agent-scoped `{owner}--{agent_key}--{name}` one with the same name are two
  Spaces with the same `ref` and different agents, which the unique index
  allows (this is HF-2's split, now visible as two ids).
- **A Space created by a grant** (`workspace NEW`) gets its row and id at
  issuance; its workspace id follows today's scheme
  `{owner}--{agent_key}--{label}`, and Drive9 materialises it at its first
  run as for any new workspace. The shared-App scheme
  `{owner}--{app_id}--{space_id}` stays for shared Apps (not in E5).
- **Labels, cursor pagination, `keep`, retention and erasure** of this
  design's Phase 3 and 6 are not in E5.

### 4. Thread ids

```sql
CREATE TABLE threads (
  thread_id TEXT PRIMARY KEY,         -- "th_" + 26 chars base32, CSPRNG; opaque
  space_id TEXT NOT NULL REFERENCES spaces(space_id),
  name TEXT NOT NULL,                 -- the label, unprefixed ("main", "daily-jira")
  dir TEXT NOT NULL,                  -- key under .vam/threads/: legacy "{owner}--main", new "th_…"
  state TEXT NOT NULL DEFAULT 'live' CHECK(state IN ('reserved','live')),
  created_via TEXT NOT NULL CHECK(created_via IN ('run','backfill','grant')),
  created_by_grant_id TEXT,
  created_at INTEGER NOT NULL,
  UNIQUE(space_id, name),
  UNIQUE(space_id, dir)
);
```

- **Relation, unchanged:** Space 1:N threads; thread 1:N runs (a task is a
  run, E10 locked decision 2); at most one live task per Space; a run for
  another thread queues (owner decision D). A thread still has no lock of its
  own (E10 decision 12).
- **Name is a label, unique per Space.** Same charset as today
  (`sanitize_thread_id`). Unique so that name lookups stay unambiguous for
  the CLI and old clients; relaxing it is a later question.
- **Directory.** A thread created after this lands uses its id as its
  directory (`.vam/threads/th_…/`); legacy threads keep their directory
  (never moved or renamed, as for workspaces). Everything below the API keeps
  working on the directory key: `RunSpec.thread_ref` after resolution holds
  `dir`, as it holds the prefixed name today, so `thread_recorder`,
  `thread_context`, `ensure_thread_json` and the thread log are unchanged.
- **`thread.json`** (`ThreadMeta`, `vam_dir.rs:144-157`) gains `id` (`th_…`)
  and `name` with serde defaults; the existing `thread_id` field keeps
  meaning "directory key" for compatibility. The id travels with the
  workspace (Drive9 versions it), so a moved or forked workspace keeps its
  thread ids; SQLite stays an index (E10 locked decision 23).
- **`state = reserved`**: a thread created by a grant (`thread NEW`) exists
  as a row before its first run writes the directory.

**Proto** (`proto/vam/v1/workflow.proto`; field numbers in
`docs/src/content/docs/hosted/triggered-runs.md`):
- `RunSpec.space_id = 12`, `RunSpec.thread_id = 13`. The comment on
  `thread_ref = 9` (`:223`) becomes "thread **name** (or, after resolution,
  its directory key); prefer `thread_id`".
- `RunStatus.space_id = 20`, `RunStatus.thread_id = 21` beside
  `thread_ref = 16` (`:355`) and `workspace_id = 17`, which keep their
  values.
- `proto/vam/v1/orchestrator.proto`: `Space {space_id, name, workspace_id,
  agent_urn, created_at, last_active_at, created_by_grant_id}`, `Thread
  {thread_id, space_id, name, created_at, created_by_grant_id}`,
  `ListSpacesRequest {page_size, page_token, agent_urn}` /
  `ListSpacesResponse {spaces, next_page_token}`.
- `hosted_services.proto` `CallerContext.space_id = 8`, `thread_id = 9`
  (host-stamped); `workspace_handle = 6` and `thread_handle = 7` keep today's
  values. AS-0.2's `space_ref` is dropped (the id serves brokers that route
  replies); `app_id` takes 11 when Apps ship.

**Code** (for the breakdown's map):
- `run_context.rs`: `resolve_run_workspace_id` (`:128-156`) and
  `resolve_thread_ref` (`:158-165`) gain an id path: id given → load the row,
  check the owner and agent, use its `workspace_id` / `dir`; name given →
  today's resolution, then get-or-create the row. Both given → must agree,
  else `InvalidContract`.
- `api/mod.rs::create_run` (`:1238-1256`, moving into `run_start.rs`):
  resolve ids before `inbox::admit`; stamp `RunStatus` ids in
  `build_run_status` (`:2607`).
- `inbox.rs::request_from_resolved` (`:205-226`): the queued request
  carries `space_id` and `thread_id`, so a queued run cannot land in another
  place if a name is reused.
- `vam-storage/src/vam_dir.rs`: `ThreadMeta.id`, `ThreadMeta.name`;
  `ensure_thread_json` (`:359-389`) writes them for new threads and fills
  them in for legacy ones at the thread's next commit.
- `api/threads.rs`: `:id` accepts `sp_…`, a workspace id or a name; `:thread`
  accepts `th_…` or a name; responses add `spaceId`, `threadId`, `name`.
  `api/workspace_files.rs` accepts `sp_…` for `:id`.
- New `spaces.rs` (store, backfill), routes `GET /v1/spaces`,
  `GET /v1/spaces/:id`.
- CLI (`crates/vam-cli/src/cmd/run.rs:29-66`, `thread.rs`, `workspace.rs`):
  `--workspace` and `--thread` take a name or an id; `vam run` prints
  `space sp_… (assistant) · thread th_… (main)`; `vam thread list` shows id
  and name.
- **App contract** (the VAM app, E5 Phase 2 item WO-2.6): read `spaceId`,
  `threadId` and `name` from `RunStatus` and the thread list; send ids when
  it holds them, names otherwise; show names, never `--` ids (HF-3's
  per-agent default workspace keeps working by name).

### 5. Migration window and backfill

- **Every API accepts an id or a name** from the day ids land: `RunSpec`,
  the thread and workspace-file routes, the CLI flags. An id always wins;
  both must agree. **Names stay accepted** as a lookup for people and old
  clients; what changes is what the host **stores**: grants, queued
  requests, run records and `RunStatus` carry ids. Ending name lookups is a
  later owner decision, not part of E5.
- **Grants bind ids, never names** (`docs/src/content/docs/hosted/triggered-runs.md`): a
  deleted and recreated workspace with the same name is a different Space,
  and a grant for the old one answers `DENIED TARGET_GONE`.
- **Backfill** (the AS-3.2 pattern; only adds rows, safe to rerun):
  - Spaces: at `vamd` start, one row per existing Drive9 workspace that has
    none: `workspace_id` unchanged, `owner_user_id` from the prefix, `ref`
    from the last `--` segment (`owner_prefix_suffix`,
    `owner_prefix.rs:68-73`), `agent_urn` from `WorkspaceMeta` when present,
    `created_via backfill`. Also lazily on first touch, for a workspace
    created while the pass ran.
  - Threads: per Space, on first touch (a run, a thread read, a grant), read
    HEAD's `.vam/threads/*/thread.json`; one row per directory, `dir`
    unchanged, `name` = the directory without the `{owner}--` prefix, a new
    id; `threads_indexed_at` set. The id is written into that
    `thread.json` at the thread's next commit by the host, never by a
    separate write into HEAD.
  - Nothing is renamed, moved or rewritten.

### 6. Locked decisions this agrees with or touches

| Decision | Effect |
| --- | --- |
| Apps-Spaces "Concepts" | Agrees. A thread gains an id; no new concept. |
| Apps-Spaces "Space: a place … found by `(app_id, ref)`" | **Proposal P2 (owner to confirm): Spaces before Apps.** Until Apps ship, a Space is found by `(owner_user_id, agent_urn, ref)`, which is the personal App `(owner, agent)` plus `ref`; `app_id` is NULL and AS-2.4 fills it. No change to the locked row's meaning; it changes when the App column is filled. |
| Apps-Spaces "Personal Apps auto-created per `(owner, agent)`" | Not built in E5 (Apps are cut from 0.1.0); covered by P2. |
| Apps-Spaces "Existing workspaces keep their IDs. Backfill only adds rows." | Agrees (§5). |
| Apps-Spaces "Credentials: one type, extending `api_keys`" and "Holder" | Agrees (§2). |
| Apps-Spaces "Target resolution … a request may only narrow" | Agrees: for a trigger, step 2 (the Credential) decides and the request names no target. |
| E10 decision 2 (thread holds context, task is the unit) | Agrees: each fire is one task on one thread. |
| E10 decision 4 (sub-agents in the same VM first) | Untouched: a schedule for another agent is a separate task the owner approved, run as the owner, with its own budget; not delegation. |
| E10 decision 12 (files and conversation roll back together) | Agrees: one live task per Space stays; a fire queues. |
| E10 decision 23 (the thread log lives in the workspace) | Agrees: the log stays under `.vam/threads/<dir>/`; new directories are ids, legacy ones keep their names; SQLite holds only the index. |
| E10 Phase 3 owner decision D (another thread queues) | Agrees, and triggers use the same queue. |

**Open question 5 (Space concurrency): answered** by E10 Phase 3 owner
decision D and the trigger design: a second message or a fire for a busy
Space is queued, not `BUSY`; integrators who want parallel work use separate
Spaces.

## Amendment 2026-11-01: the primary agent without Apps

Written by a design session against `main`. The plan of record
for the primary agent is now the primary-agent proposal (backlog in `.cursor/workstreams/README.md`). Apps are cut from
0.1.0 (0.1.0 launch scope), so the primary agent is designed
without them. Superseded in this doc, for the primary agent only:

- Problem 3's "no home for the planned 'primary agent'" (line 21): the home
  is `users.primary_agent_urn` (NULL = the node's default primary),
  the primary-agent proposal
- "the 'primary agent' is their default App" (Audience, line 52).
- The second sentence of the locked row "Personal Apps … Account default
  App = primary agent." **Proposal P-B, owner to confirm**; the first
  sentence is untouched. When Apps ship, an Account's default App is the
  personal App of its primary agent, derived from the user's column, not
  stored twice.
- "Starting a run", column 3 for the App row ("Account default App"): a run
  request keeps naming its agent; the app and the CLI read the primary from
  `GET /v1/me` and name it.
- The row "Alice changes her primary agent" (§ Scenarios): she sets
  `PUT /v1/me/primary-agent`; runs keep naming their agent.
- "Absorbs primary agent and default workspace" (§ What changed from
  the App/Subject draft): the default workspace is absorbed by Spaces (amendment
  2026-10-29); the primary agent is not absorbed by Apps.
- `users.default_app_id` (§ Data model) is not added for the primary.

Everything else in this doc stands.

## Decision summary (locked)

| Topic | Decision |
| --- | --- |
| Concepts | Account, App, Space, Thread, Credential. |
| Space | A place, not an identity. One Drive9 workspace. Found by `(app_id, ref)`; `ref` opaque. |
| App | Pins exactly one agent. Kind `personal` or `shared`; kind only changes defaults. |
| Personal Apps | Auto-created per `(owner, agent)` on first run. Account default App = primary agent. |
| Credentials | One type, extending `api_keys` with nullable columns. Login sessions stay separate. |
| Holder | `http` or `broker:<id>`. A credential is refused on any other transport or broker. |
| Authority | Owner ∩ App limit. App limit is forbid-only; personal Apps have none. |
| Secrets | Personal App: all owner secrets (today). Shared App: listed vault keys only. |
| Existing workspaces | Keep their IDs. Backfill only adds rows. |
| Target resolution | Request → Credential → default → error (§ Target model). A request may only narrow a Credential. |
| Retention | `keep` (personal default) or `idle_ttl` capped at 30 days; per-Space `keep` override. |

## Target model

### Roles

**vamd: the authority**
- Creates personal Apps and backfills existing workspaces.
- Resolves every run's App, Space, and Thread; enforces Credential targets.
- Applies the App limit and secret bindings on every tool, file, and egress
  decision.
- Reaps idle Spaces and enforces per-App quotas.

**Account owner (CLI / app)**
- Picks a default App; creates shared Apps; issues and revokes Credentials.

**Integrator (website backend or broker)**
- Holds a Credential for a shared App; chooses `ref`s; maps them to its own
  users. VAM never sees that mapping.

### Starting a run

One internal entry point, shared with `docs/src/content/docs/hosted/triggered-runs.md`:

```text
start_run(caller, target { app, space_ref, thread }, prompt, inputs)
```

| Field | 1. Request | 2. Credential | 3. Default | 4. Otherwise |
| --- | --- | --- | --- | --- |
| App | `target.app` | Credential's App | Account default App | error |
| Space | `target.space_ref` | Credential's fixed Space | App default Space (`assistant` for personal Apps) | error for shared Apps |
| Thread | `target.thread` | — | Space default Thread | `main` |

Backward compatibility: `agentRef` + `workspaceRefUri` + `threadRef` map to the
personal App for `(owner, agent)`, Space ref = workspace suffix, same thread.

A missing Space is created on first use for personal Apps (as workspaces are
today), and only with `spaces:create` for shared Apps.

### Authority

- **App limit** is a forbid-only Cedar layer, consistent with DX locked
  decision 8 (org and platform layers are forbid-only). It can only take
  authority away.
- **Cedar shape:** add an `App` entity; the run principal is a member of it,
  so App policies use `principal in Vam::App::"…"`. Space is passed as
  context (`space_id`), not an entity. Needs the `principal ==` vs `in` audit
 .
- **CallerContext** gains `app_id`, `space_id`, `space_ref` (host-stamped).
  A broker can route replies from `space_ref` without its own lookup table.
- **HITL** still routes to the App owner.

### Credentials

| Field | Meaning |
| --- | --- |
| `holder` | `http` or `broker:<id>` |
| `owner` | The Account |
| `target` | Account, one App, or one Space |
| `permissions` | Today's scopes plus `runs:start`, `spaces:create`, `spaces:read` |
| `constraints` | Optional fixed Space, captured thread |
| `issued_via` | `user` (app/CLI) or `invoke` (approved tool call) |
| `external_ref` | Holder's opaque label, display only |
| lifecycle | `created_at`, `last_used_at`, `expires_at`, `revoked_at` |

| Today / planned | As a Credential |
| --- | --- |
| User API key | holder `http`, target Account, today's scopes |
| Website key | holder `http`, target shared App, `runs:start` + `spaces:create` |
| Scheduler grant | holder `broker:vam.schedule`, target one Space, constraints the bound thread and approved run request, `runs:start`, via `invoke`, expiry always set (amended 2026-10-29, §2 of the amendment) |

### Scenarios

| Scenario | Credential | App | Space | Result |
| --- | --- | --- | --- | --- |
| Alice chats | Login session | Her default App | `assistant` | Same as today |
| "Summarise Jira daily at 6 PM" | Scheduler grant | Alice's App | Where she created it | Runs as Alice at 6 PM; queued behind her task if she is mid-run (amended 2026-10-29) |
| Customers @mention a Slack bot | Bot grant with `spaces:create` | Support bot (shared) | Per channel or per person, broker's choice | Private history each; App's limited rights and secrets |
| Website visitors (Mast) | Website key, or grant held by `mast.ui` | Site App (`idle_ttl`) | Hash of visitor cookie | Isolated; idle Spaces reaped ≤ 30 days |
| Alice revokes the scheduler | — | — | — | Next broker request `DENIED` |
| Alice changes her primary agent | — | New default App | — | Runs that name no App go to the new agent |

### What changed from the App/Subject draft

| App/Subject draft | This design | Why |
| --- | --- | --- |
| Subject is an identity | Space is a place | Subjects never authenticate |
| Subject = visitor | `ref` = anything | Brokers need channels, tickets, schedules |
| App only for multi-tenant products | Every agent use is an App | Absorbs primary agent and default workspace |
| Separate App key | One Credential type | One hashing / revoke / audit path |
| `Agent in Subject in App in User` | `App` entity; Space as context | Policies are per App |
| Secrets not addressed | App secret bindings | Closes the secrets half of Fact 2 |
| Subject state ephemeral / durable | App retention + per-Space `keep` | Policy belongs to the product |

Kept: opaque refs, get-or-create by ref, labels, cursor pagination, 30-day
cap, cascade delete and erasure, typed rate-limit reasons.

## Data model (conceptual)

```sql
CREATE TABLE apps (
  app_id TEXT PRIMARY KEY,           -- server-generated
  owner_user_id TEXT NOT NULL,
  name TEXT NOT NULL,
  kind TEXT NOT NULL CHECK(kind IN ('personal','shared')),
  agent_urn TEXT NOT NULL,
  agent_version TEXT,
  default_space_ref TEXT,
  limit_policy_id TEXT,              -- forbid-only layer; NULL for personal
  secret_bindings_json TEXT,         -- NULL = all owner secrets (personal)
  retention_json TEXT,
  quotas_json TEXT,
  created_at INTEGER NOT NULL,
  deactivated_at INTEGER,
  UNIQUE(owner_user_id, name)
);

CREATE TABLE spaces (
  space_id TEXT PRIMARY KEY,         -- server-generated
  app_id TEXT NOT NULL REFERENCES apps(app_id),
  ref TEXT NOT NULL,                 -- opaque, caller-chosen
  workspace_id TEXT NOT NULL UNIQUE, -- the Drive9 workspace
  default_thread TEXT,
  keep INTEGER NOT NULL DEFAULT 0,
  created_at INTEGER NOT NULL,
  last_active_at INTEGER NOT NULL,
  UNIQUE(app_id, ref)
);
-- space_labels(space_id, key, value)
-- users.default_app_id
-- api_keys + holder, target_kind, target_id, constraints_json,
--            issued_via, external_ref, expires_at   (all nullable)
```

**Workspace IDs.** `spaces.workspace_id` is the source of truth. Existing
workspaces keep their IDs; new shared-App Spaces use
`{owner}--{app_id}--{space_id}` (all server-generated; `apply_owner_prefix`
already rejects `--` in guest suffixes, so no collision). Playbook workspace
reservations (`run_reservations`, PB-1.5) stay keyed by `workspace_id` and
need no change.

## Phases

> **2026-10-29:** E5 Phase 1 builds AS-1.1 to AS-1.4 (grants only), the id
> parts of AS-0.1 and AS-0.2, and AS-3.1 and AS-3.2 without Apps (amendment
> §2–§5). The plan and its scenarios are in the `work-outside-chat` tracker. Everything else below stays as designed and
> cut from 0.1.0.

IDs (`AS-x.y`) are local and become Linear issues; each must be independently
reviewable.

| Phase | Goal | Depends on |
| --- | --- | --- |
| 0 | Model frozen | Open questions C1–C3 answered |
| 1 | One Credential type | — (does not need C1–C3; unblocks `hsp-triggers` Phase 2) |
| 2 | Apps | 0 |
| 3 | Spaces | 2 |
| 4 | One way to start a run | 2, 3, `hsp-triggers` TR-1.1 |
| 5 | App authority | 0, 4 |
| 6 | Retention and limits | 3 |

### Phase 0 — Model frozen

**Exit:** protos committed and validated; Cedar approach proven in PDP tests;
`cargo test -p vam-spec` and `cargo test -p vam-security` pass.

- **AS-0.1 Proto records and run target.** `App`, `Space`, `Credential`
  messages for API responses; `RunTarget { app, space_ref, thread,
  thread_mode }` on `CreateRunRequest` (shared with `hsp-triggers` TR-0.2).
  Additive only. Files: `proto/vam/v1/orchestrator.proto`,
  `proto/vam/v1/common.proto`, baseline fixtures. Tests: `cargo test -p
  vam-spec`.
- **AS-0.2 CallerContext fields.** `app_id`, `space_id`, `space_ref` and
  matching `identity_requirement` flags. Files: `hosted_services.proto`,
  `crates/vam-orchestrator/src/caller_context.rs`. Tests: caller context unit
  tests.
- **AS-0.3 Cedar `App` entity.** Add `App`; prove one forbid-only App policy
  denies a tool its owner may use. Files:
  `crates/vam-security/schema/vam.cedarschema`. Tests:
  `crates/vam-security/tests/pdp_tests.rs`.
- **AS-0.4 Policy audit.** List every policy using `principal ==` and whether
  its meaning changes. Output: a section in this doc. Blocked by AS-0.3.

### Phase 1 — One Credential type

Independent of Phase 0: `target_kind` values `app` / `space` are stored but
not used until Phases 2–4.

**Exit:** all existing auth tests pass unchanged; holder, target, expiry, and
revoke tests pass.

- **AS-1.1 Extend `api_keys`.** Nullable `holder`, `target_kind`,
  `target_id`, `constraints_json`, `issued_via`, `external_ref`,
  `expires_at`; NULL reads as holder `http`, target Account. Files:
  `crates/vam-orchestrator/src/keys.rs`. Tests: `tests/login_tests.rs`,
  `tests/api_tests.rs` unchanged.
- **AS-1.2 `check_credential`.** One helper: hash compare, holder matches
  transport, not revoked or expired; returns owner, target, permissions. HTTP
  bearer auth uses it; `broker:*` credentials refused over HTTP. Files:
  `keys.rs`, `api/auth.rs`. Blocked by AS-1.1.
- **AS-1.3 List and revoke.** API and `vam keys` show holder, target,
  `issued_via`, `last_used_at`; owner-scoped. Files: `api/mod.rs`,
  `crates/vam-cli/src/cmd/keys.rs`. Blocked by AS-1.1.
- **AS-1.4 Tests.** Legacy key unchanged; wrong transport, expired, revoked
  denied; other owner cannot list or revoke. Blocked by AS-1.2, AS-1.3.

### Phase 2 — Apps

**Exit:** existing run flows create a personal App; default App selectable;
backfill idempotent.

- **AS-2.1 `apps` table and store.** Files: new
  `crates/vam-orchestrator/src/apps.rs`, `workflow.rs` schema init.
- **AS-2.2 Personal App on run start.** Get or create for `(owner, agent)`.
  Files: `api/mod.rs` (or `run_start.rs` once TR-1.1 lands). Blocked by
  AS-2.1.
- **AS-2.3 Default App.** `users.default_app_id`; `PUT /v1/me/default-app`;
  `vam apps default <name>`. Files: `users.rs`, `api/me.rs`, new
  `crates/vam-cli/src/cmd/apps.rs`. Blocked by AS-2.1.
- **AS-2.4 Backfill.** One personal App per `(owner, bound agent)` from
  existing workspace metadata; safe to rerun. Files: `apps.rs`,
  `bin/vamd.rs` startup. Blocked by AS-2.1.
- **AS-2.5 Shared App API.** `/v1/apps` create, list, get, deactivate; `vam
  apps`. Owner-scoped. Blocked by AS-2.1.
- **AS-2.6 App pin is authoritative.** Keep `AgentWorkspaceMismatch` as a
  consistency check. Files: `catalog_resolve.rs`, `api/mod.rs`. Blocked by
  AS-2.2.

### Phase 3 — Spaces

**Exit:** every existing workspace is a Space with an unchanged ID; shared-App
Spaces get-or-create by ref; a caller cannot reach another App's Space.

- **AS-3.1 `spaces` and `space_labels` tables.** Files: new
  `crates/vam-orchestrator/src/spaces.rs`. Blocked by AS-2.1.
- **AS-3.2 Backfill.** One Space per existing workspace, `ref` = suffix,
  `workspace_id` unchanged. Blocked by AS-3.1, AS-2.4.
- **AS-3.3 New shared-App Spaces.** `{owner}--{app_id}--{space_id}`;
  materialised like today's first run. Files: `spaces.rs`, `manager.rs`.
  Blocked by AS-3.1.
- **AS-3.4 Space API.** `POST /v1/apps/:id/spaces` get-or-create by `ref`;
  list with cursor pagination and label filter; get; delete. Blocked by
  AS-3.1.
- **AS-3.5 Activity.** `last_active_at` updated on run start. Blocked by
  AS-3.1.

### Phase 4 — One way to start a run

**Exit:** the resolution table is covered by tests for HTTP and the gateway;
the old request shape still works.

- **AS-4.1 Resolution in `start_run`.** Implement the table. Files:
  `run_start.rs`. Blocked by TR-1.1, AS-2.2, AS-3.1.
- **AS-4.2 Legacy mapping.** `agentRef` + `workspaceRefUri` + `threadRef` →
  personal App + Space + Thread. Blocked by AS-4.1.
- **AS-4.3 Narrow, never widen.** Credential targets bound the request;
  shared Apps require a Space ref and `spaces:create` to create one. Blocked
  by AS-4.1, AS-1.2.
- **AS-4.4 Stamp and expose.** `app_id` / `space_id` on the run record,
  `CallerContext`, and `RunStatus`; filter `GET /v1/runs` by App and Space.
  Blocked by AS-4.1, AS-0.2.

### Phase 5 — App authority

**Exit:** a shared App run is denied a tool, a secret, and an egress host its
owner could use.

- **AS-5.1 App limit in the PEP.** Evaluate the forbid-only layer on tool,
  file, and egress decisions. Files: `security.rs`. Blocked by AS-0.3, AS-4.4.
- **AS-5.2 Secret bindings.** Shared Apps resolve listed keys only, for host
  tools, HSP `Invoke`, and MCP. Files: `secrets.rs`, `caller_context.rs`,
  `host_tools.rs`, `mcp_registry.rs`. Blocked by AS-4.4.
- **AS-5.3 Layer order.** Document where the App layer sits relative to DX
  Phase 5 layers and playbook guards (PB-2.2). Blocked by AS-5.1.
- **AS-5.4 Deny matrix tests.** Blocked by AS-5.1, AS-5.2.

### Phase 6 — Retention and limits

**Exit:** idle Spaces reaped within the 30-day cap; quotas refuse with typed
reasons; erasure is immediate.

- **AS-6.1 Retention policy.** App `keep` | `idle_ttl` (≤ 30 days);
  per-Space `keep`. Files: `apps.rs`, `spaces.rs`. Blocked by AS-3.1.
- **AS-6.2 Reaper.** On the existing 60 s sweeper in `bin/vamd.rs`: skip
  locked workspaces and workspaces reserved by a parked run
  (`run_reservations`); `remove_workspace_tree`; delete row; audit event.
  Files: `bin/vamd.rs`, `vam-storage/src/gc.rs`. Blocked by AS-6.1.
- **AS-6.3 Erasure and deactivation.** Delete Space = immediate; deactivate
  App = 30-day grace, then cascade. Blocked by AS-6.2.
- **AS-6.4 Quotas.** Per-App max concurrent runs and Space-create rate; typed
  reason in HTTP 429 and HSP `RATE_LIMITED`. Files: `run_start.rs`,
  `spaces.rs`. Blocked by AS-4.1.

## Open questions

> 2026-10-29: questions 4 and 5 are answered in the amendment (§2, §6). C1–C3
> stay open; E5 does not need them.

Answer C1–C3 before Phase 0.

1. **C1 — Is the App the DX "deployed agent" (DX-90) and the home of the
   DX-17 profile** (models, context policy, VM sizing)? Draft: yes, one
   concept.
2. **C2 — One App per environment?** Draft: yes; staging and prod are two
   Apps with separate Spaces, secrets, and quotas.
3. **C3 — Must callers of a shared App always name a Space?** Draft: yes;
   otherwise outside callers share one Space (the original Fact 1 bug).
4. **Grant expiry default** for broker Credentials: none, or N days?
5. **Space concurrency:** one writer per Space means two messages in one
   Slack channel Space give `BUSY`. Draft: the integrator chooses finer refs;
   revisit if it hurts.

## PR base

`feat/apps-spaces` cut from `main`. Phase 4 builds on `start_run` from `hsp-triggers` TR-1.1.

## Issue index

Not created yet.

## Implementer constraints

- Users who never touch Apps must see no behaviour change. Every phase keeps
  existing API, CLI, and hermetic tests passing unchanged.
- Never rename or move an existing workspace. Backfill only adds rows.
- `ref` is opaque: never parse it, log it at info level, or store anything
  about what it refers to.
- App limits are forbid-only. An App can never grant what its owner lacks.
- Credential secrets are stored as SHA-256 only; tokens are returned once.
- Space is never a Cedar principal.
- Never expose host paths or real secrets to the guest.
- Add focused tests per issue; run `cargo test -p vam-spec`,
  `cargo test -p vam-security`, `cargo test -p vam-orchestrator`,
  `cargo check --workspace`, and `cargo fmt --all --check`.
- If an issue changes a public contract, update the Starlight docs and
  `docs/astro.config.mjs` when pages move.

## Amendment 2026-11-28: Apps on today's main

Written by a design session against `main` (branch `docs/next-lanes`, cut
from `origin/main` on 2026-11-28). It is the plan of record for the Apps and
Spaces lane (tracker `.cursor/workstreams/apps-spaces.md`, branch
`feat/apps-spaces`). Where `main` contradicts an earlier section, `main` is
the fact. Superseded parts are listed in §2; everything not listed there
stands. Decisions are in §3: rows marked **approved 2026-11-28** were
answered by the owner; rows marked **proposal** are recommendations the
owner has not answered.

The other lanes cut on the same day: lane 2 VAM image
(`design/vam-image.md`, tracker `.cursor/workstreams/vam-image.md`, phases
I1-I4), lane 3 Self-aware (`design/self-aware.md`, tracker
`.cursor/workstreams/self-aware.md`, phases S1-S4) and lane 4 Long work
(`design/long-work.md`, tracker `.cursor/workstreams/agent-runtime.md`
"Phase 5 — Long work (U4)", slices 5a-5e).

### 1. What `main` has now

**Spaces and threads** (`crates/vam-orchestrator/src/spaces.rs`, built by
E5 under `work-outside-chat` locked decision 10, which already replaced this
doc's row "Existing workspaces keep their IDs. Backfill only adds rows."):

- A Space is stored in Drive9 under its `sp_…` id alone, with no owner or
  agent prefix; owner and bound agent are columns of the row
  (`spaces.rs:1-10`). Ids are `sp_`/`th_` + 26 base32 characters from
  `OsRng` (`spaces.rs:31-52`).
- The `spaces` table has `space_id, app_id, owner_user_id, agent_urn, label,
  is_default, default_key, default_thread_id, created_via,
  created_by_grant_id, created_at, last_active_at` (`spaces.rs:172-185`).
  **`app_id` exists and is always NULL**: no code writes it. There is **no
  `ref` column**: `label` is a display string, never a key, a directory or a
  lookup (`spaces.rs:4-6`, `:343-357`).
- Default Spaces: one per `(owner, agent, default_key)` with
  `default_key IN ('released','draft')`, enforced by the partial unique index
  `idx_spaces_default` and created on first use by `default_space`
  (`spaces.rs:179, 186-187, 245-274`). This pair is the implicit personal App
  of amendment 2026-10-29 §3, split by draft and released.
- Idempotent create: `space_create_keys(owner_user_id, idempotency_key,
  space_id)` (`spaces.rs:199-204`); `create_space_once` returns the first
  call's Space for a repeated key under the connection lock
  (`spaces.rs:309-341`). `POST /v1/spaces {agentUrn, label}` requires an
  `Idempotency-Key` header and checks the agent with `get_agent_visible`
  first (`api/spaces.rs:126-155`; header read at `api/auth.rs:270-278`,
  any non-empty string).
- Every read by id goes through `get_space(owner, space_id)`, which answers
  "not found" for a foreign id (`spaces.rs:224-231`); routes accept only an
  `sp_…` id (`space_for_route`, `spaces.rs:499-511`).
- The `Space` proto has no `app_id` (`proto/vam/v1/orchestrator.proto:1176-1184`);
  `RunSpec.space_id = 12`, `thread_id = 13` (`workflow.proto:231-232`).

**Credentials** (`keys.rs`, `credentials.rs`, built by E5 as AS-1.1 to
AS-1.4): `api_keys` has the nullable `holder, target_kind, target_id,
constraints_json, issued_via, external_ref, expires_at, source_run_id,
tool_name` (`keys.rs:239-248, 321-329`). `check_credential` checks hash,
holder against transport, revoke and expiry (`credentials.rs:134-203`).
Grants use `target_kind = 'space'` (`grants.rs:565`). **The HTTP bearer path
refuses non-`http` holders and revoked keys only** (`keys.rs:704-760`,
holder check at `:736-740`): it reads neither `target_kind` nor `expires_at`
for an `http` key. An `http` key with `target_kind = 'app'` minted today
would act with its owner's full authority. App keys must not be issued
before the bearer path narrows them (P2). **Today's `main` is not exposed**
(checked 2026-11-28): the only `http` mint path,
`create_api_key_owned_with_mode` (`keys.rs:444-490`), writes neither
`target_kind` nor `expires_at`; the rows that carry a target or an expiry
are grants (`insert_grant_credential` refuses a non-`broker:` holder,
`credentials.rs:239-247`) and action handles (`mint_action_handle`, holder
`broker:<id>`, `credentials.rs:356-397`), both refused by the bearer path's
holder check. The gap opens only when a route mints an `http` key with a
target or an expiry, which is AS-2.7. Owner question 7 (§9).

**Run start** (`api/run_start.rs`): `start_run(state, caller, req, opts)`
(`:236`) is the one entry for `POST /v1/runs`, the inbox starter and the
gateway (`work-outside-chat` locked decision 8). Order: refuse inline parts;
idempotency lookup; `resolve_place` picks a Space id (owner checked) or the
agent's default Space and a thread id or a new thread (`:273-285`;
`spaces.rs:535-560`); inbox admission (`:294-312`); the bound agent from
`workspace.json` and from the `spaces` row must equal the requested URN, else
`AgentWorkspaceMismatch`, and an unbound non-default Space is bound
(`:314-351`); `apply_workspace_version_pin` (`:353-357`); `load_agent_for_run`
(`:359-376`).

**Version pin** lives in the workspace, not in SQLite:
`WorkspaceMeta.agent_version: Option<WorkspaceAgentVersion>`
(`vam-storage/src/vam_dir.rs:115-125`; the type at `:47-104`, where any two
draft pins match and released pins compare every field, `:93-103`). It is
written once, at the first run, by `ensure_workspace_json`, which refuses a
different later version (`vam_dir.rs:292-330`, called from
`api/mod.rs:3401-3428`). `apply_workspace_version_pin`
(`catalog_resolve.rs:58-114`) requires a run's version to match the pin or
fills it in. **There is no call that changes a Space's pin**, and `yanked`
is a catalog channel value with no behaviour (`catalog.rs:31`; DX-94 open).

**Not on main:** `apps` table, App keys, a Cedar `App` entity
(`vam-security/schema/vam.cedarschema:1-14` has `Agent, User, Tool, Host,
Path, Workspace`), agent access (on `feat/agent-access`, Phase 5).

### 2. What is stale in this doc, and the replacement

| Earlier text | Status | Replacement |
| --- | --- | --- |
| Workspace id scheme `{owner}--{app_id}--{space_id}` (§ Data model "Workspace IDs", line 509; amendment 2026-10-29 §3, line 195; AS-3.3, line 612) | **Superseded** | A shared-App Space is an ordinary `sp_…` Space with `app_id` set. No new directory scheme. |
| `spaces.ref` column and `UNIQUE(app_id, ref)` (§ Data model, line 495; amendment 2026-10-29 §3, lines 175, 182) | **Superseded** | No `ref` column. Get-or-create by caller ref reuses the idempotency-key path (§4). Only a hash of the ref is stored. |
| `spaces.keep`, `default_thread`, `space_labels` (§ Data model) | Deferred to P6 | `keep` added in P6. Labels stay the single `label` column. |
| AS-2.4 and AS-3.2 "Backfill … from workspace metadata" | **Superseded** | Spaces already exist as rows. P2 fills `spaces.app_id` (§4.1). |
| `RunTarget { app, space_ref, thread }` and `CallerContext.space_ref` (AS-0.1, AS-0.2, § Starting a run) | **Superseded** | A run names `RunSpec.space_id` (exists) and, new, `app_id`. The ref is never returned or stamped: brokers route by `space_id` (amendment 2026-10-29 §4 already dropped `space_ref`). |
| Phase table and Phases 0-6 (§ Phases) | **Superseded** by §7 | P1 shipped in E5. Phases renumbered by goal, not by number. |
| Open questions C1-C3 (§ Open questions) | **Answered** in §3 | — |
| Decision summary (locked), lines 360-372 | **Replaced** by §3.2 | Rows kept unless §3.2 marks them changed. |

### 3. Decisions

#### 3.1 Owner answers (approved 2026-11-28)

| # | Question | Answer |
| --- | --- | --- |
| C1 | What is an App? | The deployed agent: one agent URN and one pinned version (DX-90's "deployed agent"). |
| C2 | One App per environment? | Yes. Staging and prod are two Apps, with separate Spaces, keys, secrets and quotas. "Environment" means staging/prod only. |
| C3 | Must callers of a shared App name a Space? | Yes. A shared-App run without a Space is refused; there is no shared default Space. |
| D1 | Who owns an App? | The developer who creates it. Org admins (`users:admin`) can reassign it. |
| D2 | Who answers approvals in a shared App? | The App owner (and org admins) in v1, or the App uses read-only playbooks. End-user approvals are later. |
| D3 | Where may App keys live? | Backend only in v1. A browser widget later uses short-lived tokens minted by the integrator's backend. |
| D4 | First integrator kit language | TypeScript. |
| D5 | Long work, harness H2-H4 | Long work stays design-only until the first pilot; H2-H4 held until pilots give real tasks. |

#### 3.2 Decision summary (replaces lines 360-372)

Rows that cite C1-C3 or D1-D4 are approved 2026-11-28. Rows marked
**Changed** are proposals, owner to confirm. Every other row is the locked
summary of lines 360-372, unchanged in meaning.

| Topic | Decision |
| --- | --- |
| Concepts | Account, App, Space, Thread, Credential. Unchanged. |
| App | The deployed agent: one agent URN, one pinned version, kind `personal` or `shared` (C1). One App per environment (C2). Owner = creator; org admins reassign (D1). |
| Space | A place, not an identity. One Drive9 workspace stored under its `sp_…` id. Owner and agent are row columns; `app_id` names its App. **Changed:** a shared-App Space is found by id, or get-or-created by an opaque caller ref whose hash is stored as an idempotency key (§4.2); there is no `ref` column. |
| Shared App runs | Always name a Space (C3). |
| Personal Apps | One per `(owner, agent)`, created on first run (unchanged). Its draft and released default Spaces are today's `default_space` rows. The "default App = primary agent" sentence stays as amended 2026-11-01. |
| Version pin | **Changed:** the App pin is authoritative for shared-App Spaces (AS-2.6). Personal Spaces keep their own pin (proposal Q10, §5). |
| Credentials | One type on `api_keys` (shipped). App keys: holder `http`, `target_kind = 'app'`, backend only (D3). |
| Holder | `http` or `broker:<id>` (shipped). |
| Authority | Owner ∩ App limit; App limit forbid-only; personal Apps have none. The owner's agent access is checked at every App run start (§6). |
| Approvals | Routed to the App owner in v1 (D2). |
| Secrets | Personal App: all owner secrets. Shared App: listed vault keys only. Unchanged. |
| Existing workspaces | Superseded by `work-outside-chat` decision 10 (ids are the identity, no backfill). P2 fills `app_id` on existing rows; nothing is moved. |
| Target resolution | Request → Credential → default → error. A request may only narrow a Credential. Unchanged in meaning; §7 P4 restates it on `start_run`. |
| Retention | `keep` (personal default) or `idle_ttl` capped at 30 days; per-Space `keep`. Unchanged. |
| Integrator kit | TypeScript first (D4). |

### 4. Data model on today's main

#### 4.1 `apps` and the `app_id` column

```sql
CREATE TABLE apps (
  app_id TEXT PRIMARY KEY,           -- "app_" + 26 base32, spaces::mint_id
  owner_user_id TEXT NOT NULL,
  name TEXT NOT NULL,                -- a label, like Space labels
  kind TEXT NOT NULL CHECK(kind IN ('personal','shared')),
  agent_urn TEXT NOT NULL,
  agent_version TEXT,                -- "X.Y.Z"; NULL only for a personal App (§5)
  limit_policy_id TEXT,              -- P5
  secret_bindings_json TEXT,         -- P5; NULL = all owner secrets (personal)
  retention_json TEXT, quotas_json TEXT,  -- P6
  created_at INTEGER NOT NULL,
  deactivated_at INTEGER
);
CREATE UNIQUE INDEX apps_personal ON apps(owner_user_id, agent_urn) WHERE kind = 'personal';
```

- `UNIQUE(owner_user_id, name)` from § Data model is dropped: names are
  labels (work-outside-chat decision 10).
- `spaces.app_id` (exists) is filled when a run resolves a Space: the
  personal App's id for `(owner, agent)`, or the shared App's id. A row with
  NULL `app_id` reads as the personal App of its `(owner_user_id,
  agent_urn)`, so filling is lazy and never required for a run to work. A
  startup pass may fill the rest; it only writes NULL columns and is safe to
  rerun.
- **Spaces with no agent yet.** Session creation with a label and a run
  with no `agent_ref` make default Spaces with NULL `agent_urn`
  (`api/mod.rs:1058-1069`; `run_start.rs:260-284` through `resolve_place`,
  `spaces.rs:540-545`). Such a Space keeps `app_id` NULL until the first
  run binds an agent to it (`run_start.rs:316-351`: the workspace file, and
  the row for non-default Spaces); that run fills `app_id` with the
  personal App of the bound agent. A run with no agent leaves it NULL.
  Reads (`GET /v1/spaces`, `Space.app_id`) return such a Space with an
  empty `app_id` and never fail on it; the startup pass skips it.
- App keys are `api_keys` rows: `holder 'http'`, `target_kind 'app'`,
  `target_id` the App id, scopes `runs:start`, `spaces:create`,
  `spaces:read`. Grants keep `target_kind 'space'`.

#### 4.2 Get-or-create a shared-App Space by caller ref

`POST /v1/apps/{id}/spaces {ref, label?}` returns the same Space for the same
`(app, ref)`. It reuses `create_space_once` (`spaces.rs:309-341`) unchanged
in shape:

- `owner_user_id` = the App's owner; `idempotency_key` =
  `app:{app_id}:` + hex SHA-256 of the ref bytes. The ref itself is never
  stored, logged or returned (Implementer constraints: `ref` is opaque).
- `POST /v1/spaces` refuses an `Idempotency-Key` that starts with `app:`
  (400), so a header key cannot collide with a derived one.
- The new row has `app_id` set and `created_via 'api'`. The label defaults
  to `untitled`; the integrator may set a label it is willing to show the
  owner (never required).
- Requires `spaces:create` on the credential (C3, AS-4.3). With only
  `spaces:read`, an unknown ref is `not found`, a known ref returns its Space.
- Owner reassignment (D1) moves `apps.owner_user_id`, every
  `spaces.owner_user_id` with that `app_id`, and their `space_create_keys`
  rows in one transaction. Workspace directories do not move (they are
  `sp_…` only).

Rejected: a new `app_space_refs` table. It duplicates `space_create_keys`
and its lock-protected get-or-create.

### 5. Version pin

- **App pin authoritative (AS-2.6, kept).** A shared App holds
  `agent_version`. `start_run` for an App Space resolves the version from
  the App row and passes it where `apply_workspace_version_pin` now reads
  `WorkspaceMeta` (`catalog_resolve.rs:58-114`): a request naming another
  version is `AgentWorkspaceVersionMismatch`; a request naming none gets the
  App's. `AgentWorkspaceMismatch` stays as a consistency check
  (`run_start.rs:314-351`).
- **Proposal Q10, owner to confirm.** Shared-App Spaces follow the App pin:
  one write to `apps.agent_version` moves every Space of the App; a live run
  finishes on the version it started with (the spec is resolved at start).
  Personal Spaces stay pinned and show "Agent updated, use vX?" with one
  click. A personal App's `agent_version` is NULL: each personal Space keeps
  its own pin, set at its first run (in `workspace.json` today, in the
  `spaces` row after lane 3 S2).
- **Seam shared with lane 3 (S2, S4; `design/self-aware.md` §S2 "The
  seam").** One store call, `set_space_agent_version(owner, space_id,
  version)`, built and owned by lane 3 S2 (Space upgrade API and banner).
  S2 makes the `spaces` row the authority for a Space's pin (new
  `spaces.agent_version`, lazily backfilled from the file);
  `WorkspaceMeta.agent_version` in `workspace.json` becomes "last version
  run here" and is rewritten from the row at the next session start (today
  `ensure_workspace_json` refuses a changed pin, `vam_dir.rs:316-327`; S2
  adds the session-start rewrite). Apps P4 (AS-2.6) calls the seam to route
  an App Space to the App pin, so an App pin move reaches each Space through
  the same function. Lane 3 S4 (App version follow and bulk upgrade) lands
  inside or after Apps P2. Whoever lands first owns `catalog_resolve.rs` for
  that change (Q16).
- **Proposal Q11 (yank), owner to confirm; settles DX-94.** A yanked version
  refuses new runs with a typed denial; Spaces pinned to it, and Apps pinned
  to it, get a forced upgrade prompt; running sessions are not killed. For a
  shared App the owner moves the App pin; until then its runs are refused.

### 6. Agent access and VAM image interplay

**Agent access** (`git show origin/feat/agent-access:design/agent-access.md`;
decisions A1-A9 approved 2026-11-27):

- **App runs pass through `start_run`, so the owner's access is checked.**
  The rule sits in the catalog visibility calls that every admission makes,
  including run start (`agent-access.md:156-158`, citing
  `api/run_start.rs:359-376`) and triggered runs (`:159`). An App key
  authenticates as its owner, and "API keys of a user carry that user's
  access" (`agent-access.md:341-342`), so an App run is admitted only if the App
  owner may use the agent.
- **Creating an App needs access.** `POST /v1/apps` makes the same check as
  `POST /v1/spaces` (`agent-access.md:161`): Hidden is 404, Visible is 403
  `agent_access_required:` (`:96-116`). Reassigning an App checks the new
  owner the same way.
- **Revoke stops the next run (A7, `agent-access.md:127`).** Access is
  checked at each start; a live session is not killed. An App whose owner
  loses access refuses every new run until access is restored or the App is
  reassigned.
- **Check before writing.** The agent check must run before any Space bind
  or create (`agent-access.md:170-171`); the App path keeps that order and
  checks before `create_space_once` too.
- Agent access is per URN, not per version (`agent-access.md:90-91`), so an
  App pin change needs no new access check.

**VAM image** (lane 2, `design/vam-image.md`): the image will be pinned
inside the agent version (`imageRef`, lane 2 I2). An App pins the agent version, so it pins the image
through it. **An App has no image field.** Moving an App's pin is also how
its image changes. Where VM sizing lives (DX locked decision 17, environment
profile; C1 originally proposed the App as its home) is not decided here:
**proposal**, owner to confirm, that the App carries the DX-17 profile once
DX-17 is implemented, and until then lane 2 I2 reads the agent's
`resourceRequirements`. That interim read is itself an owner question
against DX locked decision 17 (`design/vam-image.md` §7 question 4).

### 7. Phases (replace § Phases)

P1 (one Credential type) shipped in E5 and is not repeated. IDs stay
`AS-<phase>.<n>`. Each phase is broken down in its own thread; the items
below are the scope, not the plan. Hot files (Q16, proposal): P4 holds
`api/run_start.rs` and `catalog_resolve.rs`; lane 3 S2 and lane 2 I2 must
not hold them at the same time. Lane 2 I2 starts only after P4 has merged
and agent access is in `main` (`design/vam-image.md` §5); lane 3 S3 waits
for agent access in `main`; lane 3 S4 runs inside or after P2
(`design/self-aware.md` §S4).

| Phase | Goal | Depends on | Exit |
| --- | --- | --- | --- |
| P0 | Model frozen: this amendment, Cedar `App` entity, scenario list | Owner answers (done 2026-11-28) | Amendment merged; `cargo test -p vam-security` proves a forbid-only App policy denies a tool its owner may use; policy audit (AS-0.4) section written; scenario names below copied to the tracker |
| P2 | Apps: personal and shared, pinned version, App keys | P0; agent access in `main` | `POST/GET /v1/apps`, deactivate, reassign; personal App on first run with no visible change; App keys issued and narrowed by the bearer path |
| P3 | Shared Spaces by ref | P2 | Same ref same Space; another App's Space unreachable; ref stored only as a hash |
| P4 | One `start_run` resolution | P2, P3, lane 3 S2 (`set_space_agent_version`); holds `run_start.rs` and `catalog_resolve.rs` | Resolution table proven for HTTP and the gateway; App pin authoritative; old request shape unchanged; a shared-App run resolves no owner secrets until P5 |
| P5 | App limits and secret bindings | P0 (Cedar), P4 | A shared-App run is denied a tool, a secret and an egress host its owner could use |
| P6 | Quotas and retention | P3, P4 | Idle Spaces reaped within the 30-day cap; quotas refuse with typed reasons; erasure immediate |
| P7 | Integrator kit and Apps screens | P4 (P5, P6 for the docs' limits section) | TS SDK published; example app runs against a process-mode vamd; docs page live; Squrell-App Apps screens shipped |

After P7: **pilot** (one internal app, one client). Not a build phase; Long
work (`design/long-work.md` §9 lists what the pilot must measure) and
harness H2-H4 wait for it (D5).

#### P0 — Model frozen

- **AS-0.1** Proto, additive: `App {app_id, name, kind, agent_urn,
  agent_version, owner_user_id, created_at, deactivated_at}`,
  `Space.app_id`, `RunSpec.app_id` and `RunStatus.app_id` at the next free
  field numbers, `CallerContext.app_id = 11` (amendment 2026-10-29 §4).
  No `RunTarget`, no `space_ref`.
- **AS-0.3** Cedar `App` entity; the run principal is `in` it; one
  forbid-only policy test.
- **AS-0.4** Policy audit (`principal ==` vs `in`), as a section in this doc.
- Scenario names for P2-P7 copied into the tracker.

**Scenario delta:** none turns green (PDP and proto unit tests only).

#### P2 — Apps

- **AS-2.1** `apps` table and store (`apps.rs`).
- **AS-2.2** Personal App get-or-create for `(owner, agent)` in
  `start_run`; fill `spaces.app_id` (§4.1).
- **AS-2.5** `/v1/apps` create (agent access checked), list, get,
  deactivate; `PUT /v1/apps/{id}/owner` for org admins; `vam apps`.
- **AS-2.7** App keys: `POST /v1/apps/{id}/keys`; the bearer path reads
  `target_kind = 'app'` and limits the key to that App's routes and runs,
  and honours `expires_at` for `http` keys. Shipped together with issuance.
- **AS-2.6** moves to P4 (it changes `start_run`).

| Scenario | Change | Layer | Given | Why |
|---|---|---|---|---|
| `a-developer-creates-a-shared-app-pinned-to-a-released-version` | new | process | `org-publisher`, `agent-plain` | P2 exit |
| `an-app-cannot-be-created-for-an-agent-the-owner-may-not-use` | new | process | `org-member`, restricted agent (agent access world piece) | §6 |
| `an-app-key-cannot-reach-the-owners-other-agents-or-keys` | new | process | `shared-app` (new world piece) | AS-2.7 narrowing |
| `a-revoked-app-key-starts-nothing` | new | process | `shared-app` | revoke path |
| `an-org-admin-reassigns-an-app-and-the-new-owner-gets-its-runs` | new | process | `org-admin`, `shared-app` | D1 |
| `agent-workspaces-are-isolated` | unchanged, must stay green | process | existing | personal runs see no change |

#### P3 — Shared Spaces by ref

- **AS-3.4** `POST /v1/apps/{id}/spaces {ref}` get-or-create (§4.2); list
  and get by App with cursor pagination; delete moves to P6.
- **AS-3.5** `last_active_at` on run start (exists as `touch_space`,
  `spaces.rs:418`); keep it for App Spaces.
- `POST /v1/spaces` refuses `app:` keys.

| Scenario | Change | Layer | Given | Why |
|---|---|---|---|---|
| `the-same-ref-returns-the-same-space` | new | process | `shared-app` | get-or-create |
| `two-refs-get-two-private-spaces` | new | process | `shared-app` | isolation |
| `an-app-key-cannot-open-another-apps-space` | new | process | two `shared-app`s | P3 exit |
| `an-app-key-without-spaces-create-cannot-make-a-space` | new | process | `shared-app` | AS-4.3 |

#### P4 — One `start_run` resolution

- **AS-4.1** Resolution in `start_run`: App from request, then credential,
  then the personal App of the named agent; Space from `RunSpec.space_id`,
  then the credential's fixed Space, then the personal default Space, else
  refused for a shared App (C3); thread as today.
- **AS-2.6** App pin authoritative (§5); calls lane 3's
  `set_space_agent_version` seam (S2) to route App Spaces to the App pin.
- **AS-4.3** Narrow, never widen.
- **AS-4.4** Stamp `app_id` on the run record, `RunStatus`, `CallerContext`;
  filter `GET /v1/runs` by App and Space.
- Approvals of a shared-App run list for the App owner (D2): the run's
  owner is the App owner, so the existing approval list applies; verify.
- **AS-4.5 No owner secrets before P5.** A shared-App run resolves zero
  owner secrets (an empty binding list) until P5's secret bindings and App
  limit land; P5 replaces the empty list with the App's bindings. Personal
  Apps keep all owner secrets.

| Scenario | Change | Layer | Given | Why |
|---|---|---|---|---|
| `an-app-key-run-lands-in-the-named-space-on-the-apps-pin` | new | process | `shared-app` | P4 exit |
| `a-shared-app-run-without-a-space-is-refused` | new | process | `shared-app` | C3 |
| `a-request-cannot-widen-an-app-key` | new | process | `shared-app` | AS-4.3 |
| `an-app-run-is-refused-after-the-owners-access-is-revoked` | new | process | `shared-app`, restricted agent | A7 |
| `moving-the-app-pin-moves-every-space-and-a-live-run-finishes-on-the-old-version` | new (pending Q10) | process | `shared-app`, two released versions | §5 |
| `an-app-approval-reaches-the-app-owner` | new | process | `shared-app` with `agent-with-guarded-tool` | D2 |
| `a-shared-app-run-reads-no-owner-secrets-before-bindings` | new | process | `shared-app`, `agent-with-shell-and-secret-skill` | AS-4.5 |

#### P5 — App limits and secret bindings

- **AS-5.1** App limit in the PEP (tool, file, egress). **AS-5.2** Secret
  bindings for host tools, HSP `Invoke` and MCP. **AS-5.3** Layer order.
  **AS-5.4** Deny matrix. Scope as in § Phases (Phase 5).

| Scenario | Change | Layer | Given | Why |
|---|---|---|---|---|
| `an-app-limit-denies-a-tool-its-owner-may-use` | new | process | `shared-app` with limit | P5 exit |
| `an-app-limit-denies-an-egress-host-its-owner-may-reach` | new | process | `shared-app`, `agent-with-egress-probe` | P5 exit |
| `a-shared-app-reads-only-its-bound-secrets` | new | process | `shared-app`, `agent-with-shell-and-secret-skill` | AS-5.2 |
| `a-personal-app-keeps-all-owner-secrets` | new | process | `agent-with-shell-and-secret-skill` | no change for personal use |

#### P6 — Quotas and retention

- **AS-6.1** to **AS-6.4** as in § Phases (Phase 6), with `spaces.keep`
  added here and the reaper deleting the `space_create_keys` row with the
  Space.

| Scenario | Change | Layer | Given | Why |
|---|---|---|---|---|
| `an-idle-shared-space-is-reaped-within-its-ttl` | new | process | `shared-app` with `idle_ttl` | P6 exit |
| `a-kept-space-is-never-reaped` | new | process | `shared-app` | per-Space `keep` |
| `a-space-with-a-parked-run-is-not-reaped` | new | process | `shared-app`, `agent-with-guarded-tool` | AS-6.2 |
| `deleting-a-space-erases-it-at-once` | new | process | `shared-app` | AS-6.3 |
| `an-app-over-its-run-quota-is-refused-with-a-typed-reason` | new | process | `shared-app` with quota | AS-6.4 |

#### P7 — Integrator kit and Apps screens

- **AS-7.1** TS SDK: create a Space by ref, start a run, follow
  `/v1/runs/{id}/events` (`api/mod.rs:1481-1484`), answer approvals
  (`/v1/approvals/{id}/decide`, `api/mod.rs:226-228`). Types generated from
  `docs/api/openapi.yaml`. Backend only (D3): the SDK refuses to run in a
  browser.
- **AS-7.2** Example app: a small web backend where each visitor gets a
  Space by a hash of their session; one App key in the backend.
- **AS-7.3** Docs page (`docs/src/content/docs/integrations/`, sidebar in
  `docs/astro.config.mjs`): Apps, App keys, Spaces by ref, approvals,
  limits.
- **AS-7.4** Squrell-App Apps screens: list Apps, create (agent and version
  picker), keys (issue once, revoke), Spaces per App, pin and upgrade.

| Scenario | Change | Layer | Given | Why |
|---|---|---|---|---|
| `the-sdk-creates-a-space-runs-and-follows-events` | new | process | `shared-app` | AS-7.1 |
| `the-sdk-answers-an-approval-and-the-run-resumes` | new | process | `shared-app`, `agent-with-guarded-tool` | AS-7.1 |
| `the-example-app-gives-two-visitors-two-private-spaces` | new | process | `shared-app` | AS-7.2 |
| `an-owner-creates-an-app-and-issues-a-key-in-the-app` | new | ui | Squrell-App | AS-7.4 |
| `the-apps-screen-shows-each-apps-spaces-and-pin` | new | ui | Squrell-App | AS-7.4 |

### 8. What we are not building (additions)

- End-user approvals in shared Apps (D2); a browser-held App key (D3).
- Any image field on an App (§6).
- A `ref` column, ref lookup without the App, or a ref in any response.
- Environment promotion between Apps (`vam promote`, DX-92): staging and prod
  are two Apps; moving prod's pin is a manual pin write.
- SDKs other than TypeScript before the pilot.

### 9. Open questions (each with a recommended answer)

1. **Q10, Q11** — proposals in §5. Recommended: as written.
2. **Does an App key's Space create count against the owner's or the App's
   quota?** Recommended: the App's (P6), so one site cannot exhaust the
   owner's personal use.
3. **Where does the TS SDK live?** Recommended: its own repo, with types
   generated from `docs/api/openapi.yaml`, so the VAM repo stays Rust-only
   (AGENTS.md §7: no external monorepo tools without instruction). The P7
   process scenarios drive the same HTTP calls the SDK makes; the SDK's own
   tests run against a process-mode vamd.
4. **Can the e2e runner drive the example app?** Recommended: no new step
   type in P7; the scenarios assert the HTTP behaviour, and the example app
   has its own smoke test.
5. **Personal App rows: create on first run, or keep them implicit?**
   Recommended: create on first run (the locked row), since `spaces.app_id`
   and App-filtered run lists need an id; the NULL-reads-as-personal rule
   keeps runs working while rows fill.
6. **The DX-17 profile on the App** — proposal in §6. Recommended: yes,
   after DX-17 is implemented; not in P2.
7. **Owner question: does P2 ship bearer-path narrowing in the same change
   as App key issuance?** The HTTP bearer path (`keys.rs:704-760`) checks
   holder and revoke only; it ignores `target_kind` and `expires_at` for
   `http` keys. No `http` key with a target or expiry can be minted on
   today's `main` (§1), so nothing is exposed now. Recommended: yes; AS-2.7
   makes the bearer path enforce `target_kind = 'app'` (App routes and runs
   only) and `expires_at` for `http` keys, with tests, before or in the same
   PR as `POST /v1/apps/{id}/keys`; no App key is issued from a build
   without it.
