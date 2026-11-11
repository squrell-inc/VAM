# Apps and Spaces: who a run is for, and where it lives

**Status:** Proposed (replaces the model in `design/vam-subjects.md` §4–§7, §10); amended 2026-10-29 (run grants and opaque Space and thread ids for the E5 slice)
**Date:** 2026-10-09
**Owner:** maintainer
**Repos:** `squrell-inc/VAM`

## Decision record

**Context.** VAM has one caller shape: a `User` with API keys, owning
`{owner}--{suffix}` workspaces (`vam-storage/src/owner_prefix.rs`). That
breaks in four places:

1. A website using one key puts every visitor in one workspace with one writer
   lock, and every visitor runs with the developer's full authority and
   secrets (`vam-subjects.md` §2, Facts 1–2).
2. Nothing is ever cleaned up (Fact 3).
3. "Which agent runs here" is a storage detail (`WorkspaceMeta` in
   `.vam/workspace.json`) surfaced only as `AgentWorkspaceMismatch` /
   `WorkspaceNotBound` errors. There is no named, deployed agent (DX-90) and
   no home for the planned "primary agent" or default workspace/thread.
4. Brokers (`design/hsp-triggers.md`) need to act for someone without
   inventing identity.

`vam-subjects.md` proposed `App` and `Subject` as new identities. Review found
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

## Audience and positioning

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

- Prior art (E2B, Modal, Vercel, Daytona, Fly, Cloudflare; `vam-subjects.md`
  §3) converges on: a thin developer-owned principal, developer-named
  instances with get-or-create, opaque handles with no PII, and per-account
  quotas. All kept here.
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
- Usage rollup and spend ceilings (`vam-subjects.md` §9).
- Space-scoped secrets / customers' own credentials (`vam-subjects.md` §12).
- Read-only App-level shared mount (`vam-subjects.md` §6.1).
- Broker development (see `design/hsp-triggers.md`).

## Amendment 2026-10-29: run grants and opaque ids for the E5 slice

Written by a design session against `main`. It is the plan of
record for the parts of this design that E5 Phase 1 ("grants, gateway and
ids") builds; the phases are in `design/hsp-triggers.md` § "Phases (E5
getting-started slice)". Apps, shared Spaces, App authority and retention
stay as designed and stay cut from 0.1.0 (`design/launch-0.1.0.md` §2). Where
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
  `design/hsp-triggers.md` amendment §4.
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
(`hsp-triggers.md` amendment §7).

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
`hsp-triggers.md` amendment §3):
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
- **Grants bind ids, never names** (`hsp-triggers.md` amendment §4): a
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
for the primary agent is now `design/primary-agent.md`. Apps are cut from
0.1.0 (`design/launch-0.1.0.md` §2), so the primary agent is designed
without them. Superseded in this doc, for the primary agent only:

- Problem 3's "no home for the planned 'primary agent'" (line 21): the home
  is `users.primary_agent_urn` (NULL = the node's default primary),
  `primary-agent.md` §3.
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
  `vam-subjects.md`): the default workspace is absorbed by Spaces (amendment
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

One internal entry point, shared with `design/hsp-triggers.md`:

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
  (`vam-subjects.md` §5.1).
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

### What changed from `vam-subjects.md`

| `vam-subjects.md` | This design | Why |
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
> §2–§5). The plan and its scenarios are in `design/hsp-triggers.md` § "Phases
> (E5 getting-started slice)". Everything else below stays as designed and
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
