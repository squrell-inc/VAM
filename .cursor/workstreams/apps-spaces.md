# Workstream: Apps and Spaces: deployed agents, shared Spaces, integrator kit

```yaml
status: active          # active | paused | done
slug: apps-spaces
current_phase: 0 (plan not drafted)
base_branch: feat/apps-spaces
design_docs:
  - design/vam-apps-spaces.md   # read "Amendment 2026-11-28" first; it supersedes the phase list
  - design/self-aware.md        # S2 builds the set_space_agent_version seam P4 calls; S4 with P2
```

## Goal

A developer can deploy an agent as an App (one agent, one pinned version,
one App per environment), give their backend an App key, and give each of
their users, channels or tickets a private Space by an opaque ref, without
VAM learning who those people are. Runs for an App go through the one
`start_run`, on the App's pin, under the owner's agent access and the App's
forbid-only limits and secret bindings; idle Spaces are reaped and quotas
refuse with typed reasons. Done when P0 to P7 are closed (TypeScript SDK,
example app, docs page and Squrell-App Apps screens shipped) and the pilot
(one internal app, one client) has started on it. Personal use shows no
change at any point.

## Out of scope

- End-user approvals in shared Apps (v1 routes approvals to the App owner).
- App keys held in a browser; the short-lived-token widget is later.
- An image field on an App (the image is pinned inside the agent version,
  lane 2 I2).
- Environment promotion (`vam promote`, DX-92): staging and prod are two
  Apps.
- SDKs other than TypeScript before the pilot.
- Long work (E10 Phase 5) and harness H2-H4: held until the pilot.
- Org accounts; Space-scoped secrets; usage rollup and spend ceilings
  (design § What we are not building).

## Locked decisions

Do not re-litigate without editing this list. Source: `design/vam-apps-spaces.md`
§ Decision summary as replaced by the 2026-11-28 amendment §3.2, and
`work-outside-chat` locked decision 10 (ids are the identity), which this
tracker does not contradict.

**Apps-Spaces decision summary** (the design's locked summary as restated by
amendment §3.2; C1-C3 approved 2026-11-28; parts marked *proposed change* are
the amendment's changes and wait for the owner)

1. **Concepts.** Account, App, Space, Thread, Credential.
2. **App (C1).** The deployed agent: one agent URN and one pinned version;
   kind `personal` or `shared`; kind only changes defaults.
3. **One App per environment (C2).** Staging and prod are two Apps with
   separate Spaces, keys, secrets and quotas. "Environment" means
   staging/prod only.
4. **Shared App runs always name a Space (C3).** No shared default Space.
5. **Space.** A place, not an identity: one Drive9 workspace stored under its
   `sp_…` id; owner, agent and `app_id` are row columns. *Proposed change:* a shared-App Space
   is found by id or get-or-created by an opaque caller ref whose SHA-256 is
   stored as an idempotency key in `space_create_keys`; no `ref` column; the
   ref is never stored, logged or returned.
6. **Personal Apps.** One per `(owner, agent)`, created on first run; its
   draft and released default Spaces are today's default Spaces. The primary
   agent stays `users.primary_agent_urn` (amendment 2026-11-01).
7. **App pin authoritative (AS-2.6)** for shared-App Spaces;
   `AgentWorkspaceMismatch` stays as a consistency check. P4 routes App Spaces
   to the App pin by calling lane 3's `set_space_agent_version` (S2), where
   the `spaces` row is the authority and `workspace.json` is "last version run
   here", rewritten at the next session start (proposed, with Q10).
8. **Credentials.** One type on `api_keys` (shipped). App keys: holder
   `http`, `target_kind = 'app'`; never issued before the bearer path narrows
   them to their App.
9. **Holder.** `http` or `broker:<id>`; refused on any other transport.
10. **Authority.** Owner ∩ App limit; the App limit is forbid-only; personal
    Apps have none. Space is never a Cedar principal.
11. **Secrets.** Personal App: all owner secrets. Shared App: listed vault
    keys only.
12. **Target resolution.** Request → Credential → default → error; a request
    may only narrow a Credential.
13. **Retention.** `keep` (personal default) or `idle_ttl` capped at 30 days;
    per-Space `keep`.
14. **Existing state.** Nothing is moved or renamed (`work-outside-chat`
    decision 10). *Proposed change:* P2 only fills NULL `spaces.app_id`; a
    NULL `app_id` reads as the personal App of `(owner_user_id, agent_urn)`.

**Owner approvals 2026-11-28** (15-18, 20; 19 follows from agent-access A7,
approved 2026-11-27)

15. **App owner** is the developer who creates it; org admins
    (`users:admin`) can reassign it. Reassignment moves the App's Spaces and
    their idempotency rows in one transaction (that mechanism is proposed).
16. **Approvals** in shared Apps go to the App owner (and org admins) in v1,
    or the App uses read-only playbooks.
17. **App keys are backend-only in v1.**
18. **Integrator kit in TypeScript first.**
19. **Agent access applies to Apps** (agent-access A7): every App run passes
    `start_run`, so the App owner's access is checked at each start; creating
    or reassigning an App needs access; a revoke stops the next run and does
    not kill a live one.
20. **Long work stays design-only until the first pilot; harness H2-H4 held
    until pilots give real tasks.**

**Proposals (owner has not answered; not locked)**

- **Q10** Shared-App Spaces follow the App's pin (one write moves all; live
  runs finish on the old version); personal Spaces stay pinned with an
  "Agent updated, use vX?" prompt. Seam `set_space_agent_version` shared
  with lane 3 S2/S4 (`design/self-aware.md`, `.cursor/workstreams/self-aware.md`).
- **Q11** Minimal yank: a yanked version refuses new runs; Spaces and Apps on
  it get a forced upgrade prompt (settles DX-94).
- **Q12** The machine idea is called "VAM image"; "environment" stays
  staging/prod (`design/vam-image.md`, `.cursor/workstreams/vam-image.md`).
- **Q16** Lanes run in parallel on separate branches; one lane holds a hot
  file (`run_start.rs`, `catalog_resolve.rs`, `manager.rs`, agent spec proto)
  at a time. Apps P4 holds `run_start.rs` and `catalog_resolve.rs`; VAM
  image I2 starts after P4 merges and agent access is in `main`; self-aware
  S3 waits for agent access in `main`; S4 runs inside or after P2.
- **Owner question (P2):** bearer-path narrowing ships with App key issuance.
  The bearer path (`keys.rs:704-760`) ignores `target_kind` and `expires_at`
  for `http` keys; today's `main` mints no such key, so nothing is exposed
  until AS-2.7 (design amendment §1, §9 question 7).
- The App carries the DX-17 environment profile once DX-17 is implemented
  (design amendment 2026-11-28 §6).

## Status legend

- `[ ]` not started
- `[~]` in progress
- `[x]` done (code + tests, or merged)

## Phases

P1 (one Credential type) shipped in E5 (`work-outside-chat`). Phase scope
and scenario names: design amendment 2026-11-28 §7.

### Done phases

Done phase sections are deleted; git history keeps them.

| Phase | Name | Done |
|---|---|---|
| 1 | One Credential type (AS-1.1 to AS-1.4) | E5, `work-outside-chat` Phase 1 |

### Phase 0 — Model frozen

**Goal:** The amendment is merged, the Cedar `App` entity is proven, the
protos are additive and the scenario list for P2-P7 is recorded.  
**Depends on:** none (owner answers given 2026-11-28)  
**Plan:** not drafted  
**Exit:** amendment merged; `cargo test -p vam-security` proves a forbid-only
App policy denies a tool its owner may use; AS-0.4 policy audit written;
`cargo test -p vam-spec` passes with the additive protos.

- [ ] Design amendment 2026-11-28 merged
- [ ] AS-0.1 Proto: `App`, `Space.app_id`, `RunSpec.app_id`, `RunStatus.app_id`, `CallerContext.app_id = 11`
- [ ] AS-0.3 Cedar `App` entity and one forbid-only test
- [ ] AS-0.4 Policy audit (`principal ==` vs `in`)
- [ ] Scenario names for P2-P7 confirmed

### Phase 2 — Apps

**Goal:** Personal and shared Apps with a pinned version, and App keys.  
**Depends on:** Phase 0; agent access released to `main`  
**Plan:** not drafted  
**Exit:** `/v1/apps` create (access checked), list, get, deactivate,
reassign; personal App on first run with no visible change; App keys issued
and narrowed to their App by the bearer path; `agent-workspaces-are-isolated`
stays green.

- [ ] AS-2.1 `apps` table and store
- [ ] AS-2.2 Personal App on run start; fill `spaces.app_id`
- [ ] AS-2.5 Shared App API, reassignment, `vam apps`
- [ ] AS-2.7 App keys and bearer-path narrowing

### Phase 3 — Shared Spaces by ref

**Goal:** Get-or-create a shared-App Space by an opaque caller ref.  
**Depends on:** Phase 2  
**Plan:** not drafted  
**Exit:** the same ref returns the same Space; another App's Space is
unreachable; only the ref's hash is stored.

- [ ] AS-3.4 `POST /v1/apps/{id}/spaces {ref}`, list and get by App
- [ ] AS-3.5 `last_active_at` for App Spaces
- [ ] `POST /v1/spaces` refuses `app:` idempotency keys

### Phase 4 — One `start_run` resolution

**Goal:** App, Space and thread resolved once in `start_run`, on the App's pin.  
**Depends on:** Phases 2 and 3; holds `run_start.rs` and `catalog_resolve.rs` (Q16)  
**Plan:** not drafted  
**Exit:** resolution table proven for HTTP and the gateway; a shared-App run
without a Space is refused; the App pin is authoritative; the old request
shape is unchanged.

- [ ] AS-4.1 Resolution in `start_run`
- [ ] AS-2.6 App pin authoritative; `set_space_agent_version` seam with lane 3
- [ ] AS-4.3 Narrow, never widen
- [ ] AS-4.4 Stamp `app_id`; filter runs by App and Space

### Phase 5 — App limits and secret bindings

**Goal:** A shared App can only take authority away from its owner.  
**Depends on:** Phases 0 and 4  
**Plan:** not drafted  
**Exit:** a shared-App run is denied a tool, a secret and an egress host its
owner could use.

- [ ] AS-5.1 App limit in the PEP
- [ ] AS-5.2 Secret bindings
- [ ] AS-5.3 Layer order
- [ ] AS-5.4 Deny matrix

### Phase 6 — Quotas and retention

**Goal:** Idle Spaces are reaped, quotas refuse with typed reasons.  
**Depends on:** Phases 3 and 4  
**Plan:** not drafted  
**Exit:** idle Spaces reaped within the 30-day cap; quotas refuse with typed
reasons; erasure immediate.

- [ ] AS-6.1 Retention policy and `spaces.keep`
- [ ] AS-6.2 Reaper (skips locked and parked)
- [ ] AS-6.3 Erasure and deactivation
- [ ] AS-6.4 Quotas

### Phase 7 — Integrator kit and Apps screens

**Goal:** An outside developer can build on an App from TypeScript and manage it in Squrell-App.  
**Depends on:** Phase 4 (Phases 5 and 6 for the docs' limits section)  
**Plan:** not drafted  
**Exit:** TS SDK published (create Space, start run, follow
`/v1/runs/{id}/events`, answer approvals); example app runs against a
process-mode vamd; docs page live; Squrell-App Apps screens shipped.

- [ ] AS-7.1 TypeScript SDK
- [ ] AS-7.2 Example app
- [ ] AS-7.3 Docs page
- [ ] AS-7.4 Squrell-App Apps screens

## Explicitly later

Work that must not be pulled into this tracker.

- Pilot (one internal app, one client): follows Phase 7.
- End-user approvals; browser widget with short-lived tokens.
- Other-language SDKs.
- Self-aware lane items (lane 3, `.cursor/workstreams/self-aware.md`: typed
  denials, Space upgrade banner, bulk upgrade); P4 only calls the shared
  `set_space_agent_version` seam that S2 builds.
- VAM image lane items (lane 2, `.cursor/workstreams/vam-image.md`).
- Long work (`.cursor/workstreams/agent-runtime.md` Phase 5,
  `design/long-work.md`): waits for the pilot.

## Progress log

| Date | Phase | What landed (end with the scoreboard: scenarios green, red, fix cycles, wall time) |
|---|---|---|
| 2026-11-28 | — | Tracker opened; design amendment 2026-11-28 written (C1-C3 and owner decisions approved). Scoreboard: none yet |
