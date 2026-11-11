# Isolation verify closeout

Brainstorm of record after live UI verify on Lima `vamd` (`feat/improving-the-vam`) plus the VAM app `feature/user-admin-ui`.

Tracker: `.cursor/workstreams/isolation-verify-closeout.md`.

Do **not** merge this into `main`. Implement later, one phase at a time, after a Plan pick.

## What we verified

Isolation is **always on**. Every durable row has `owner_user_id`. There is no `--isolation` flag.

How to run it on a Mac:

1. Checkout vam `feat/improving-the-vam`.
2. Start Lima vamd: `./scripts/start-vamd-lima.sh`.
3. Optional first admin: `VAMD_BOOTSTRAP_ADMIN=admin` and `VAMD_BOOTSTRAP_ADMIN_PASSWORD=…` (no UI, no email).
4. Point the VAM app at `http://127.0.0.1:8080`.
5. `GET /v1/health` is `SERVING`. `GET /v1/me` without a bearer is `401` (expected).

Operator login is the **bootstrap API key**. The operator row has no password. There is one operator. `operator` and `local` user seeds are required (roles + implicit owner). They are not sample catalog content.

Product tenants do **not** want boot-seeded `examples/*` agents, playbooks, skills, or Cedar policies. Those rows showed up in Org settings as policies with no matching agents, and as duplicate catalog chips. Stop seeding them. Keep `operator` / `local` users.

Members set their own ENVs with **user-scope secrets** (`PUT /v1/secrets`), not org vault rows. That HTTP already exists. The member UI still needs a place to edit them.

## Bugs found in verify

### 1. Members cannot list the catalog they are allowed to run

`GET /v1/agents` (and GET-by-URN, policies, skills) authorize `PutCatalog` + `catalog:write`. Members do not have that scope. They get `403 missing required scope 'catalog:write'`.

Intended: members **read** own drafts? No — members do not author. Members **read released** agents and **run** them. Publishers/org admin keep `catalog:write` for PUT and release.

Visibility rows already exist (`list_agent_records_visible`). The gate is wrong, not the filter.

### 2. Chat pins `1.0.0` and misses draft (and unpublished) agents

Screenshot error on publisher chat:

```text
agents 'urn:vam:agent:examples.local/vam/hrms-agent' 1.0.0 not found in catalog
```

Cause: the VAM app `feature/user-admin-ui` hardcodes CreateRun `agentRef.version = 1.0.0` in `useVamChat.ts`. Draft catalog rows are `1.0.0-draft`. Host `load_agent_for_run` looks up the exact version. Released `1.0.0` is missing until publish. The publisher (and later members) cannot talk to the agent they just saved.

`fix/create-run-draft-catalog-pin` in the VAM app is a **sidebar-channel** workaround (`1.0.0-draft` vs unversioned latest released). That is not the product. Do not treat it as the workspace pin.

Host today: pinned version → `get_agent_at_version`; no version → **latest released only**. A workspace that pins draft must send the draft version, and the host must load that draft for the owner (not silently rewrite to released).

## Locked product (this brainstorm)

### Workspace pins the agent version

Not a dropdown on every chat message.

- A workspace belongs to **one agent + one catalog version** (draft `1.0.0-draft`, or a released `1.0.0` / `1.1.0` / …).
- Choose that pin **when creating the workspace**.
- Show a **label** on the workspace row (and chat header): `draft` or `1.0.0`.
- Each send uses that pin. Threads in the workspace inherit it.
- One agent can have several workspaces (draft sandbox vs released production).
- Who may pin draft: owner / publisher of that URN. Members only see and pin **released** versions they can read.
- If there is only one version, create can default to it. If there are a draft and one or more releases, the create dialog must pick.

This is simpler than picking version while typing, and it keeps Drive9 data for that pin in one volume.

### Composer picks the model, not the agent version

The input box should **not** switch draft vs released.

It **should** let the user pick a model for this run, from the **operator allowlist**.

- Operator (admin setting) publishes the list of models this node may use.
- Chat shows that list in the composer. The send sets `inputs.model`.
- Agent `preferredModels` is a default, not the allowlist.
- The VAM app today picks `preferredModels` then falls back to `openai/gpt-4o-mini`. There is no operator list and no composer control.

## Remaining work (club later)

vam (`feat/improving-the-vam`, not `main`):

1. Catalog **GET/list** for any authenticated caller with run rights. Keep PUT/release on `catalog:write` / `publish:grant`. Members see released only.
2. Stop boot-seeding `examples.local` catalog rows (Cedar policies today; agents/skills if present). Keep user seeds.
3. CreateRun honors a **workspace pin**, including owner draft. Do not resolve a draft pin to latest released.
4. Operator **allowed-models** store + HTTP. CreateRun rejects a model outside the list.

The VAM app (`feature/user-admin-ui`):

5. Stop hardcoding `1.0.0` on CreateRun.
6. Create-workspace dialog: agent version select; workspace list/header label.
7. Composer model dropdown from operator allowlist.
8. Hide agent authoring from members. Members consume released catalog.
9. Member ENVs UI → user-scope `PUT /v1/secrets`.
10. Collapse duplicate draft/released catalog rows in lists.
11. Starlight isolation how-to: new `getting-started/users-and-roles.md` plus Lima / quickstart / installation links (Phase 6).

Starlight: isolation **init how-to** is missing on the docs site (see below). Examples are not a product seed.

## Docs site gap — initialize user isolation

The live site does not tell a new operator how to turn isolation on for people. Facts exist, but they are split and dense:

- Env names sit in `orchestrator/daemon.md` and a short block in `getting-started/installation.md`.
- Roles and scopes sit in `orchestrator/security.md` (Phase 3 table).
- Architecture dump sits in `orchestrator/user-isolation.md`.
- Lima (`getting-started/macos-lima.md`) only shows the **bootstrap API key**. It never creates an admin user, never explains operator vs org admin, never names publisher/member.
- `start-vamd-lima.sh` does not forward `VAMD_BOOTSTRAP_ADMIN` / `VAMD_BOOTSTRAP_ADMIN_PASSWORD`.

People need one getting-started recipe: which env creates the first admin, how the operator works, what each profile is for, and which later docs to open.

### What the how-to must say (locked content)

**Create the first loginable admin** (no UI, no email):

```bash
export VAMD_BOOTSTRAP_ADMIN=admin
export VAMD_BOOTSTRAP_ADMIN_PASSWORD='…'
# daemon start, or:
vamd --bootstrap-admin admin   # still needs VAMD_BOOTSTRAP_ADMIN_PASSWORD
```

That user is **`org_admin`** unless the username is the seeded `operator`. Same username again is idempotent. A second distinct bootstrap admin is refused. Then `vam login --username admin` (or the VAM app password sign-in).

**Operator** is not that admin:

- Seeded user `operator`. There is one.
- Signs in with the **bootstrap API key** (`VAMD_BOOTSTRAP_API_KEY`, or the key printed on first start). No password by default.
- Node owner: `keys:admin`, `brokers:admin`, plus user admin. Use it for the daemon, broker install, and CI. Do not use it as a tenant login in the VAM app day to day.

**Four profiles** (exactly one role per user):

| Profile | Who | When to use |
|---|---|---|
| `operator` | Bootstrap key / seeded `operator` | Host, keys, brokers. Not a tenant persona. |
| `org_admin` | First `VAMD_BOOTSTRAP_ADMIN` user, later tenant admins | Create users, reset passwords, grant publish, org secrets, reassign catalog. The VAM app Org settings. |
| `publisher` | Member who received `publish:grant` | Write catalog and **release**. Draft + released workspaces. |
| `member` | Default, including seeded `local` | Run **released** agents. Own sessions, runs, user-scope secrets. No catalog write. |

`admin` in conversation means **`org_admin`**, not operator.

### Which Starlight page to update (do not dump this into one wall)

| Job | Page | Why |
|---|---|---|
| **New how-to** | `docs/src/content/docs/getting-started/users-and-roles.md` (**new**) | Init isolation: envs, operator vs admin, four profiles, when to use each. Add to `docs/astro.config.mjs` under Getting Started. |
| Lima path | `getting-started/macos-lima.md` | After CLI setup: pass bootstrap-admin env (or document script change); operator key vs `vam login` as org admin. Link the new page. |
| 5-minute path | `getting-started/quickstart.md` | One short step after daemon start + link. Do not paste the roles table. |
| Env commands | `getting-started/installation.md` | Keep the two env vars and `--bootstrap-admin`. Replace the dense paragraph with a pointer to the new page. |
| CLI verbs | `getting-started/cli.md` | `vam login` / keys already live here. Point at the new page for *which user*. |
| Env catalog | `orchestrator/daemon.md` | Keep the `VAMD_*` table. No tutorial. |
| Scope truth | `orchestrator/security.md` | Keep the roles/scopes table. Link from the new page. |
| Invariants | `orchestrator/user-isolation.md` | Architecture only. Do **not** turn this into the onboarding recipe. |

Later, optionally teach `start-vamd-lima.sh` to pass through `VAMD_BOOTSTRAP_ADMIN` and `VAMD_BOOTSTRAP_ADMIN_PASSWORD` so the Lima page can show a copy-paste that actually works.

## Out of scope

- Per-message agent-version dropdown.
- Multiple operators.
- Password login for the operator row.
- Merging vam or the VAM app to `main`.
- Implementing in this capture.

## Verify when built

1. Publisher creates a workspace pinned to draft and chats. No `1.0.0 not found`.
2. Publisher (or member) creates a second workspace pinned to released `1.0.0` after publish. Chat uses that pin. Label shows `1.0.0`.
3. Member `GET /v1/agents` is 200 with released rows only. No `catalog:write`.
4. Member cannot pin draft. Member can set a user-scope secret and a run uses it.
5. Composer lists only operator-allowed models. A send with a listed model is what the run uses.
6. Fresh vamd has `operator` + `local` users and **no** `examples.local` catalog rows.
7. Docs site: a new Getting Started page plus Lima/quickstart links let someone create `org_admin`, sign in as operator vs admin, and pick publisher vs member without reading `user-isolation.md`.
