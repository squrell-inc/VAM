# Workstream: Agent access (per-agent visibility, Add and Request, agent admins)

```yaml
status: active
slug: agent-access
current_phase: 0
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

Proposed, waiting for the owner (design §2.4; each has a recommended answer there): A1 scope agents
only; A2 amend `connectors.md` D4 and D27; A3 install stays node-wide, grant on top; A4 agent admins
as a table, not a `UserRole`; A5 defaults (no policy row = `(org, everyone)`, installs
`(org, approval)`); A6 first releaser seeded as admin; A7 revoke stops future scheduled and
triggered starts; A8 403 `PermissionDenied` with message prefix `agent_access_required:`; A9
manifest carries `access.default` only, no admins. Phase 0 moves the answers into this list.

## Status legend

- `[ ]` not started
- `[~]` in progress
- `[x]` done (code + tests, or merged)

## Phases

### Done phases

Done phase sections are deleted; git history keeps them.

| Phase | Name | Done |
|---|---|---|

### Phase 0 — Owner decisions

**Goal:** A1–A9 answered and locked; D4 and D27 amended in `design/connectors.md`.  
**Depends on:** none  
**Plan:** not needed (docs only)  
**Exit:** Locked decisions list holds A1–A9 as answered; `design/agent-access.md` §2.4 trimmed to
the answers; `connectors.md` D4 and D27 rows edited.

- [ ] Owner answers A1–A9
- [ ] Lock them here; amend `connectors.md` D4, D27

### Phase 1 — Storage, the rule, enforcement at agent admission

**Goal:** the three tables, the one rule (design §2.3) in the store's agent `*_visible` reads, and
every admission site refusing Hidden (404) and not-granted (403).  
**Depends on:** Phase 0  
**Plan:** not drafted  
**Exit:** design §3 scenarios active and green (member cannot see or run; request, approve, run;
add open and run; revoked scheduled run refused; existing agents unchanged on upgrade);
`cargo test -p vam-orchestrator` green.

- [ ] Wave 0: scenarios pending, red for the right reason
- [ ] Schema and store API (`agent_access_policy`, `agent_admins`, `agent_access`)
- [ ] Rule in `get_agent_record_visible`, `get_agent_at_version_visible`, `list_agent_records_visible`
- [ ] Version history, outcomes, release suggestion checked; Space bind moved after admission
- [ ] `AgentAccessRequired` → 403; Hidden → existing 404 text

### Phase 2 — Routes, OpenAPI, proto, notifications

**Goal:** the HTTP API of design §4, `access.requested` and `access.decided` notifications, the
contract frozen for the frontend.  
**Depends on:** Phase 1  
**Plan:** not drafted  
**Exit:** routes in `docs/api/openapi.yaml` and `orchestrator.proto`, baseline regenerated; docs
page `orchestrator/agent-access.md` (`status: shipped`, `proves` its scenarios); notification
scenarios green; frontend JSON shapes (design §6) marked frozen.

- [ ] Routes and `GET /v1/me` additions
- [ ] Notifications kinds and producers
- [ ] OpenAPI, proto, baseline, docs page

### Phase 3 — CLI

**Goal:** `vam agent browse|add|request|leave|admins|access-policy`, `vam access
mine|requests|approve|deny|revoke` (design §5).  
**Depends on:** Phase 2  
**Plan:** not drafted  
**Exit:** CLI scenarios green; `list_paged` handles paths with a query string; hints for
`agent_access_required`; CLI docs updated.

- [ ] Commands, hints, `list_paged` fix, docs

### Phase 4 — Squrell-App

**Goal:** browse with status, request dialog, my requests, admin queue, 403 mapping (design §6).  
**Depends on:** Phase 2 contract frozen (may start against mocks before Phase 2 ships)  
**Plan:** not drafted  
**Exit:** app tests green against mocks, then against a node with Phase 2; errors on these paths
are shown, not swallowed into `[]`.

- [ ] `services/agentAccess.ts` and hooks; Marketplace Agents tab on `GET /v1/catalog/agents`
- [ ] Request dialog, my requests, admin queue, `openAgentChat` gate

### Phase 5 — Marketplace manifest and hardening

**Goal:** `access.default` in `marketplace.json` applied on first install; the pitfalls of design §7
covered by scenarios.  
**Depends on:** Phase 2  
**Plan:** not drafted  
**Exit:** hardening scenarios green (name leaks, standalone refs, draft runs, credential confusion,
yanked version, admin vs catalog scopes, demoted owner); vam-marketplace schema documents the field.

- [ ] Manifest field and install copy
- [ ] Hardening scenarios

## Explicitly later

- Per-group gates for connectors, org MCP connections and models.
- Groups; expiring grants; request reminders; email and push for `access.*`.
- Primary agent's delegate list (README backlog) uses this rule when it is built.

## Progress log

| Date | Phase | What landed (end with the scoreboard: scenarios green, red, fix cycles, wall time) |
|---|---|---|
| 2026-11-26 | — | Tracker and `design/agent-access.md` opened on `feat/agent-access` |
