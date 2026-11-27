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
**Depends on:** Phase 2 contract frozen, i.e. design §6 marked frozen at Phase 2 breakdown approval
(mocks until Phase 2 is in the base, then a real node)  
**Plan:** not drafted  
**Exit:** app tests green against mocks, then against a node with Phase 2; errors on these paths
are shown, not swallowed into `[]`.

- [ ] `services/agentAccess.ts` and hooks; Marketplace Agents tab on `GET /v1/catalog/agents`
- [ ] Request dialog, my requests, admin queue, `openAgentChat` gate

### Phase 5 — Marketplace manifest and hardening

**Goal:** `access.default` in `marketplace.json` applied on first install; the pitfalls of design §7
covered by scenarios.  
**Depends on:** Phase 2 (A9 answered by its breakdown); the vam-marketplace part after the VAM release
that carries the field (see Execution plan, lane C)  
**Plan:** not drafted  
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
