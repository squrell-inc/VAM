# Workstream: Isolation verify closeout

```yaml
status: active
slug: isolation-verify-closeout
current_phase: 4
design_docs:
  - design/isolation-verify-closeout.md
  - docs/src/content/docs/orchestrator/user-isolation.md
```

## Goal

After isolation UI verify: members can read and run **released** agents; publishers can run **draft or released** by pinning the version on the **workspace**; chat can pick an **operator-allowlisted** model. Docs site teaches how to init users (admin env, operator, four profiles). No example catalog seeds. No merge to `main`.

## Out of scope

- Per-message draft/released dropdown
- Multiple operators / operator password login
- Merge vam or the VAM app to `main`
- Coding in Phase 0 (capture only)

## Locked decisions

Do not re-litigate without editing this list.

1. Isolation is always on (`owner_user_id`). No `--isolation` flag.
2. Operator signs in with the bootstrap API key. One operator. `operator` + `local` user seeds stay. Example catalog seeds go.
3. Members do not need `catalog:write` to **list/get** released catalog. PUT/release stay write/publish scopes.
4. Members set ENVs with user-scope `PUT /v1/secrets`.
5. **Workspace** pins agent URN + version at create time. Label on the workspace. Chat sends that pin. Not a composer version dropdown.
6. Owners/publishers may pin draft (`1.0.0-draft`) or a released version. Members pin released only.
   *Amended 2026-10-10 by the product owner:* "draft" means the owner's private working copy, whatever slot it is stored in. The fixed `1.0.0-draft` slot is replaced in `agent-experience` Phase 1, which must keep this pin behaviour working.
7. Composer dropdown is **models** from the operator allowlist. Agent `preferredModels` is the default only.
8. The VAM app must not hardcode CreateRun `agentRef` to `1.0.0`.
9. Implement later, one phase per plan, on `feat/improving-the-vam` + `feature/user-admin-ui` (not `main`).
10. Docs site how-to lives under **Getting Started**, not inside `user-isolation.md`. New page `getting-started/users-and-roles.md`. Lima + quickstart + installation link it. `security.md` / `daemon.md` / `user-isolation.md` stay source of truth for scopes, env names, and invariants.

## Status legend

- `[ ]` not started
- `[~]` in progress
- `[x]` done (code + tests, or merged)

## Phases

### Phase 0 — Capture this brainstorm

**Goal:** Design doc + tracker so later plans do not reconstruct from chat.  
**Depends on:** none  
**Plan:** this capture  
**Exit:** README row, design doc, user-isolation "not enforced" points here.

- [x] `design/isolation-verify-closeout.md`
- [x] Tracker + README row
- [x] `user-isolation.md` remaining-work pointer
- [x] Stop. No product code in this phase.

### Phase 1 — Catalog GET without `catalog:write`

**Goal:** Authenticated runners can list/get visible catalog. Members see released only.  
**Depends on:** Phase 0  
**Exit:** Member `GET /v1/agents` is 200; PUT still 403 without `catalog:write`.

- [x] Split list/get authorize from `PutCatalog` / `catalog:write`
- [x] Keep visibility filter (`list_*_visible` / published-vs-private)
- [x] Tests: member 200 released; foreign draft 404; publisher PUT 200

### Phase 2 — Stop example catalog seeds

**Goal:** Fresh vamd has no `examples.local` catalog rows. User seeds stay.  
**Depends on:** Phase 0  
**Plan:** not drafted  
**Exit:** Boot has `operator` + `local` only; no example Cedar/agent seeds.

- [x] Remove boot seed of example policies (and any example agents/skills)
- [x] Starlight: examples are samples in `examples/`, not product catalog
- [x] Tests: empty catalog policies on boot aside from what product still requires

### Phase 3 — Workspace pins agent version

**Goal:** Create workspace chooses draft or a released version. Label shows it. CreateRun uses that pin. Host loads owner draft when pinned.  
**Depends on:** Phase 1  
**Plan:** not drafted  
**Exit:** Publisher draft chat works; released pin works; no hardcoded `1.0.0`.

- [x] Host: CreateRun / workspace record stores agent URN + version; draft pin loads owner draft
- [ ] the VAM app: remove hardcoded `1.0.0` in `useVamChat.ts`
- [ ] the VAM app: create-workspace version select + workspace/chat label
- [x] Do not ship a per-message version dropdown
- [~] Tests: draft pin 200; missing released `1.0.0` stays 404 if unpublished; member cannot pin draft

### Phase 4 — Operator allowed models + composer picker

**Goal:** Operator sets the model list. Chat picks one. Run uses that model.  
**Depends on:** Phase 3  
**Plan:** not drafted  
**Exit:** Composer lists allowlist only; CreateRun rejects unknown models.

- [x] vam: persist operator allowlist + HTTP
- [x] vam: CreateRun `inputs.model` must be on the list
- [ ] the VAM app: composer model dropdown; default from agent `preferredModels` if allowed
- [x] Tests: allowlist miss is 400/403; listed model is recorded on the run

### Phase 5 — Member surfaces

**Goal:** Members consume released agents, set their own secrets, and do not see authoring chrome or duplicate catalog rows.  
**Depends on:** Phase 1  
**Plan:** not drafted  
**Exit:** Member UI has no put-agent; secrets form hits user-scope vault; lists are one row per URN with channel chips, not clones.

- [ ] Hide authoring from members
- [ ] Member ENVs → `PUT /v1/secrets` (user scope)
- [ ] Collapse draft/released duplicate list rows

### Phase 6 — Starlight isolation how-to

**Goal:** Docs site teaches how to initialize users: admin env, operator key, four profiles, which page is which.  
**Depends on:** Phase 0  
**Plan:** not drafted  
**Exit:** New Getting Started page in the sidebar; Lima and quickstart link it; `user-isolation.md` is not the tutorial.

- [x] Add `docs/src/content/docs/getting-started/users-and-roles.md` (envs, operator, org_admin / publisher / member, when to use each)
- [x] Register the page in `docs/astro.config.mjs` Getting Started
- [x] Lima + quickstart + installation: short steps + link (no role-table paste)
- [x] `cli.md` points at the new page for *which user*
- [x] Do not turn `user-isolation.md` into onboarding
- [x] Optional same phase: `start-vamd-lima.sh` forwards `VAMD_BOOTSTRAP_ADMIN` / `VAMD_BOOTSTRAP_ADMIN_PASSWORD`

## Explicitly later

- Per-message agent version switch
- Workspace migrates from one agent version to another in place
- Multi-operator / OIDC
- Merge to `main`

## Progress log

| Date | Phase | What landed |
|---|---|---|
| 2026-09-18 | 0 | Tracker opened from isolation UI verify + publisher `1.0.0 not found` brainstorm |
| 2026-09-18 | 0 | Docs-site gap: isolation init how-to + which Starlight pages to update |
| 2026-09-18 | 6 | Starlight `users-and-roles.md`; Lima/quickstart/installation/cli link it; Lima script forwards bootstrap-admin env |
| 2026-09-18 | 4 | vam: operator allowlist persist + HTTP; CreateRun rejects off-list models |
| 2026-09-18 | 1 | Catalog GET without `catalog:write` merged via  |
| 2026-09-18 | 2 | Stop example catalog seeds: no `examples.local` rows on fresh open; user seeds stay |
| 2026-09-18 | 3 | Host workspace/CreateRun pin: draft pin loads owner draft; unpublished `1.0.0` 404; member cannot pin foreign draft |
