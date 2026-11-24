# Workstream: Connectors (how VAM reaches the systems people work in)

```yaml
status: active
slug: connectors
current_phase: 7 (developer kit and API keys) released to main 2026-11-21 (VAM #139, 5f1de31); wave 7 (marketplace CI) done; wave 6 (app API-key Connect) waits for the next app release; 6 released 2026-11-20 (VAM #112)
design_docs:
  - design/connectors.md
  - docs/src/content/docs/runtime/tools.md
  - `docs/src/content/docs/hosted/triggered-runs.md`
  - design/jig.md
```

> **Archive.** Phase breakdowns (Phases 1–7), the Phase 2, 4 and 5 closes and the older progress-log rows live in
> `.cursor/workstreams/archive/connectors-phases-1-7.md`. Read it only when a task needs that detail (cost rule, `phase-playbook.md` §13).


## Goal

An org admin enables a connector once and records the provider's admin
consent; each member connects their own account from a tile; agents use it
under the agent's rules, with reads running freely and writes asking with a
card. Version one ends when the walkthrough in `design/connectors.md` §3.3
runs on a real Microsoft 365 tenant: connect, triage the inbox, find a slot,
send and book with approval, a scheduled morning triage, every run with a
receipt. Linear works the same way as an MCP connector.

## Out of scope

- The marketplace hub, OCI pull, entitlements, listings and reverse lookup
  (design §9; after version one).
- Teams as a channel and the Squrell relay service (D22).
- Google, Slack and GitHub connectors; databases (D12, parked).
- Inbound events that start runs (design §6.6; later).
- Community or third-party connectors and brokers.

## Locked decisions

Do not re-litigate without editing this list. Source: `design/connectors.md`
decision record (D1–D22), agreed with the owner 2026-11-07.

1. One `vamd` node per customer organization (D1).
2. Squrell owns the OAuth app registrations; customers never see a client
   id. The org admin consents once; each user connects their own account (D3).
   **Amended 2026-11-18 (owner, D28):** where Squrell has no app and the
   provider has no dynamic registration (Box, Salesforce, Zendesk), a
   connector may say the org admin supplies the client id, and the secret
   when one is needed.
3. Nothing reaches a member before the node's admin enables it (D4).
   Enabling is an org-admin decision (`users:admin`), not operator-only.
4. A connector is connectivity only: how a credential is obtained, whose it
   is, how the node reaches the system, how it is attached; for MCP the server
   configuration plus a pinned, vetted tool list. It names no skill, playbook
   or agent (D8).
5. The MCP engine stays in VAM core; per-SaaS variation is connector data
   (D9).
6. Dependencies point downward: agent → playbook → skill → connector. A skill
   declares `requiresConnector`. Jigs live inside the skill that owns the tool
   (D10, D19).
7. Connector credentials are `connection`-kind vault rows (DX-139): never
   guest egress bindings, never handed to a broker unasked.
   Clarified 2026-11-08 (owner, Phase 2 approval): a connector's bindings do
   become session egress bindings on the host, but the guest only ever holds a
   placeholder; the real token is attached by the host egress provider and
   never enters the guest.
8. Customers pay for inference with their own keys; Squrell artifacts never
   send data off the node (D14, D15).
9. One scope set per connector in version one; writes are governed by tool
   defaults (`ask`) and agent rules (D20).
10. Version one ships `microsoft-365` (REST, Squrell public Entra client) and
    `linear` (remote MCP) built into `vamd`; skills, playbook and starter agent
    are imported from Git (D21).
11. Entra: no dynamic registration; never send the RFC 8707 `resource`
    parameter; never send `prompt=consent` on user sign-in; validate the
    token's tenant instead of the templated issuer (design §6.1).
12. Admin-supplied client (owner, 2026-11-18, D28): `provider.client:
    "org"`; `clientSecret: "org_vault"` only with it. The secret is an
    org-scope `connection` vault row pinned to the token endpoint host, written
    only by an org admin, never returned, never sent to the app, never a guest
    binding.
13. Code exchange on the node when a secret is involved (owner, 2026-11-18,
    D29): the app runs the browser sign-in with PKCE and hands code, verifier
    and redirect URI to `vamd`, which exchanges at the token endpoint pinned at
    Enable. Without a secret the app exchanges as today.
    **Amended (owner, 2026-11-19, Phase 6 open question 1):** the node
    exchanges for every `client: org` connector, MCP or REST, with or without
    a secret; a secret-less client sends no secret. The app no longer
    exchanges for org clients (CN-6.3, CN-6.6).
14. Company address (owner, 2026-11-18, D30): one `{subdomain}` parameter
    under a fixed suffix, entered once at Enable, one DNS label, filled in at
    Enable; afterwards every host is exact.
15. Marketplace connectors may declare REST bindings (owner, 2026-11-18, D31),
    exact hosts, used only inside a tool's declared egress shape.
16. First proofs (owner, 2026-11-18, D32): Box for Phase 5, Zendesk for
    Phase 6. The Box listing lives on a `vam-marketplace` branch until Phase 5
    is released; the team builds and tests it.

## Status legend

- `[ ]` not started
- `[~]` in progress
- `[x]` done (code + tests, or merged)

## Phases

Sizes and outcomes: `design/connectors.md` §10.

### Phase 0 — Security floor and paperwork

**Goal:** close the MCP credential gap; start the Microsoft side.
**Depends on:** none
**Plan:** done outside this tracker (DX-139 in `developer-experience.md`)
**Status:** **done 2026-11-07** (owner). The code is on `main`; the Microsoft
paperwork is carried as owner items (below) that only the real-tenant waves
need.

- [x] DX-139: `connection` vault kind, broker and HTTP-adapter refusals,
  notify token refusal. merged to `main` 2026-11-07
  All five Actions checks green, the microVM lane included.

**Owner items carried forward** (not engineering; needed before the
real-tenant wave of Phase 1 and for Phase 4):

- [x] Squrell Entra app registration (multi-tenant, Mobile and desktop
  platform, `squrell://oauth` and `http://127.0.0.1`, public client, delegated
  Graph scopes per design §4.1). Record the client id for the built-in
  `microsoft-365` connector.
- [ ] Publisher verification for the Entra app: owner item.

### Phase 1 — Connectors on the node

**Goal:** an admin enables a connector and records consent; members see
tiles, connect, and see status. Linear works fully as an MCP connector;
Microsoft 365 connects (its tools come in Phase 2).
**Depends on:** Phase 0 (done); the real-tenant wave (wave 7) also needs
the owner items above
**Status:** **released to `main` 2026-11-08, checklist out**. Done when the team reports a pass; the real-tenant run (wave 7's manual half) moved into the team checklist steps 6–7 on the owner's word.
**Plan:** approved 2026-11-07 (owner) with recommendation 1 (required: the process-only mock-endpoint seam, `VAMD_PROCESS_CONNECTOR_ENDPOINTS`, is refused in release builds like the other `VAMD_PROCESS_*` settings; the orchestrator checks it in wave 2) and open question 12's default (locked 11 wins for `entra` profiles; owner adds one line to the amendment note). Other open questions: defaults accepted.
**Exit:** the five scenarios below `active` and green in CI (process mode,
mock model, runner-hosted mock providers); the VAM app unit suites green on the
merged `feature/connectors`; wave 7 on a real test tenant: an org admin enables
`microsoft-365` and records consent, a member's tile reads "Connected as
<upn>", and one `check` refresh succeeds at the consent tenant's token
endpoint with no `resource`; the P6 line "Enable → Connected" recorded for
Linear (team instance) and Microsoft 365 (test tenant).

Scope from design §10 (the breakdown refines it):

- Connector definition in `vam-spec` (design §4; leave room for an optional
  `events` field, §6.6).
- `microsoft-365` and `linear` built into `vamd`; `GET /v1/connectors`
  merged with node enablement.
- `node_enabled_connectors` with the consent record; `PUT` for org admins;
  CLI parity (DX-1).
- MCP connectors: fixed connection id (stable tool names on every node) and
  vetted risk classes, so read tools stop asking.
- The VAM app: Org settings "Connectors" place; daily Connections tiles with the
  states in design §3.1; the Entra profile in the OAuth flow.
- The Squrell Entra client id may not exist yet: a placeholder in config;
  the real-tenant check is the last wave.

- [x] CN-1.1 Connector definition in `vam-spec` and the two built-in definitions as data
- [x] CN-1.2 Node enablement: `node_enabled_connectors`, consent record, `GET /v1/connectors`, `PUT` for `users:admin`, client-id config, process-mode endpoint seam
- [x] CN-1.3 MCP connector (Linear): fixed id, vetted risk classes, reserved id, enablement gate on seeding
- [x] CN-1.4 REST connector connection (Microsoft 365): token store, tenant check, Entra refresh without `resource`, `check`
- [x] CN-1.5 CLI parity: `vam connectors` (carried by W2 and W4)
- [x] CN-1.6 the VAM app: Tauri commands, types, Org settings "Connectors" place with admin consent
- [x] CN-1.7 the VAM app: Connections tiles and the Entra profile in the OAuth flow
- [ ] CN-1.8 Real-tenant check and the real client id (last; owner items first)

### Phase 2 — The connection behind skills

**Goal:** a skill calls Microsoft Graph as the user without holding a token.
**Depends on:** Phase 1. The three-line amendment to
`docs/src/content/docs/runtime/tools.md` (design §12) was approved and applied
2026-11-07. Scope now includes the per-call egress envelope for
connection bindings (owner, 2026-11-07; design §10, question 8, finding F8).
**Plan:** approved 2026-11-08 (owner) with recommendation 1 (required: open question 1's default, the host's active call window, with its residual risk recorded as a finding: a background process left by a read tool could ride a later, approved send window) and recommendation 2 (the clarifying line under locked decision 7). Other open questions: defaults accepted. Wave 0 started.
**Status:** **build done 2026-11-08** — waves 0–5 merged; all five scenarios `active` and green; the VAM app unit suites green on the merged `feature/connectors` (852/852, tsc clean). Awaiting the owner's manual check of the app (wave 5 PR body) and the release decision. The P9 line ("a read-declared tool tries to send", refused in 100% of runs) is proven with the mock model by scenario B; the real-model run needs the owner's word (benchmark lane).
**Exit:** the five scenarios below `active` and green in CI (process mode, mock
model, runner-hosted mock login and mock Graph); the VAM app unit suites green on the
merged `feature/connectors`; the existing connector, egress and notification
scenarios still green; the P9 task "a read-declared tool tries to send" refused
in 100% of runs.

Scope from design §10 (the breakdown refines it): account-kind connection
(refresh token pinned to the login host, per-audience access token); connector
bindings become session egress bindings; refresh before swap; reauth park for
egress calls; `connection.needs_reauth` notification kind (app and, through
`vam.notify`, a Slack DM); `requiresConnector` on skills and a "connect to
continue" park; the **per-call egress envelope** for connection bindings.

- [x] CN-2.1 Skill manifest scaffold: `requiresConnector` and per-tool `egress` in `skills.proto`, validation, unit tests
- [x] CN-2.2 Connection bindings from the connector, resolved at request time on the host
- [x] CN-2.3 Per-call egress envelope: the token is attached only inside the calling tool's declared shape
- [x] CN-2.4 Refresh before swap (single flight, consent-tenant endpoint, no `resource`)
- [x] CN-2.5 Parks: "connect to continue" and "reconnect to continue", resumed by a connect — also calls `ensure_fresh(horizon = tool timeout)` at the intent (moved from CN-2.4, orchestrator 2026-11-08; a failure there parks)
- [x] CN-2.6 `connection.needs_reauth` notification kind (app list, Slack DM)
- [x] CN-2.7 the VAM app: the reconnect notification and the connect-to-continue card
- [x] CN-2.8 CLI parity (carried by W4)

### Phase 3 — Cards from skills

**Goal:** tool results render as cards; send and book ask with a richer card.
**Depends on:** none (may run as a parallel lane with Phase 2)
**Plan:** drafted 2026-11-08 by the phase-breakdown agent; awaiting owner approval.
**Lane:** runs beside Phase 2 (0.1.0 launch scope). Phase 3 is the later lane: wave 0 and the waves that touch no shared file run now; wave 3 (the only one in `skills.proto` / `validation.rs`) and everything after it wait for Phase 2's wave in those files to merge into `feat/connectors` (block "Lane overlap" below).
**Exit:** the four scenarios below `active` and green in CI (process mode, mock model, a test skill in the e2e world, no Microsoft); the VAM app unit suites green on the merged `feature/connectors`; the P14 line below recorded.

Scope, sliced on the owner's word to the mail and calendar tools: `ToolSpec.ui` (`show`, `ask`), the `List` primitive in `vam/1`, and what a mail-list result needs to render as a card and a send or book to ask with the skill's card, args bound in, an edited submit recorded as AMENDED. Out: `Query`, App pages, component libraries, a jig catalog kind (D19), the `.jig` text form, `Link`, the real `m365-mail` skill (Phase 4).

- [x] CN-3.1 `List` primitive and the item binding in `vam/1` (spec + vam-spec checker)
- [x] CN-3.2 `ToolSpec.ui` and `ApprovalRequestClaims.card` (proto, manifest validation) — held for Phase 2
- [x] CN-3.3 Jig files in the skill package, checked at publish
- [x] CN-3.4 The host attaches the show card on a tool result
- [x] CN-3.5 Ask with the skill's card; an edited submit runs as AMENDED
- [x] CN-3.6 the VAM app: `List` in the renderer; tool cards in the thread; the approval card with edits

### Phase 4 — Microsoft 365 in use

**Goal:** the walkthrough in design §3.3 on a real tenant, with Microsoft 365
enabled from the marketplace.
**Depends on:** Phases 2 and 3 (both released)
**Plan:** drafted 2026-11-09 by the phase-breakdown agent; **approved by the owner 2026-11-09** with recommendations 1 (CN-4.3 exact paths) and 2 (CN-4.7 journal connection writes); open question 1 answered (a), open question 2 answered Apache-2.0; other defaults accepted.
**Exit:** the seven scenarios below `active` and green in CI (process mode, mock model, mock login and Graph, runner-made Git repos); the VAM app unit suites green on the merged `feature/connectors`; `squrell-inc/vam-marketplace` CI green on `microsoft-365-v1.0.0`; the walkthrough on the owner's test tenant with the P6 number recorded; the seven-day lane on the owner's word.
**Scope:** design §10 Phase 4 row and §9.1 (the Git marketplace, D23–D25):
trusted marketplace URLs per node, index reading, listing import at a tag,
Squrell-domain admission, "update available", the Org settings Marketplace
page; `squrell-inc/vam-marketplace` (public, created by the owner) bootstrapped
with CI; the `m365-mail` skill, the Executive assistant, the morning triage
playbook. Prepare Phase 5: load built-in connectors from every file in
`spec/connectors/`; keep the import generic across artifact kinds.

- [x] CN-4.1 Built-in connectors from every file in `spec/connectors/`
- [x] CN-4.2 Package checks and the marketplace index in vam-spec; `vam catalog validate`
- [x] CN-4.3 Exact paths in the egress envelope
- [x] CN-4.4 Trusted marketplaces on the node (list, fetch, refresh)
- [x] CN-4.5 Listing import at a tag; Squrell and Unverified admission; update available
- [x] CN-4.6 The Microsoft 365 listing: `m365-mail`, Executive assistant, morning triage
- [x] CN-4.7 Connection writes are journaled (no duplicate send or booking)
- [x] CN-4.8 the VAM app: Org settings Marketplace page and origin badges
- [x] CN-4.9 Bootstrap `squrell-inc/vam-marketplace` (merged into wave 5 with CN-4.6, owner 2026-11-09)
- [ ] CN-4.10 Real-tenant run and the two benchmark lines
- [x] CN-4.11 Connectors in listings (Phase 5 folded in, owner 2026-11-09)

### Phase 5 — Admin-supplied client (Box)

**Goal:** an org admin enables a connector whose OAuth client they registered
with the provider, entering the client id and, when asked, the secret; members
connect from the tile; `vamd` exchanges the code with the vault secret and
refreshes rotating tokens safely. Marketplace connectors may use it (design
§13, D28, D29).
**Depends on:** Phase 4 (marketplace import, released).
**Status:** **released to `main` 2026-11-19** (VAM #52, merge 4f24c41; Squrell-App #12, merge bb4ee15). CN-5.7, the team's run on a real Box tenant from the `vam-marketplace` branch `box-connector`, is a later manual check: the next phase does not wait for it (playbook §6).
**Plan:** drafted 2026-11-18 by the phase-breakdown agent (below); approved by the owner 2026-11-18 with the recommended defaults.
**Scope:** `provider.client` and `clientSecret: "org_vault"` in `vam-spec`
and the marketplace check; Enable takes `{ clientId, clientSecret? }` (org
admin only; secret write-only, org-scope `connection` row); the app's connect
flow uses the node's client id and skips registration; a node route that takes
code, verifier and redirect URI and exchanges with the secret; refresh with the
org secret (`RefreshProfile::Mcp`), single flight for single-use refresh
tokens; the capability summary shows "admin-supplied client"; the admin can
replace the secret; disable and re-enable keep or clear it (open question).
**Exit:** scenarios green on a mock provider (no secret in any listing, app
response or guest binding; rotation under concurrent calls); the Box listing
from the `vam-marketplace` branch `box-connector` (listing files and a
team test guide in `docs/listing-box.md`, pushed 2026-11-18) connects and runs on a real Box tenant
(team, on the owner's word).
**Rubric:** cost 3 (secret handling, a new route, both repos).

- [x] CN-5.1 `provider.client` and `clientSecret: "org_vault"` in vam-spec, the marketplace check and the capability summary
- [x] CN-5.2 Enable with the admin's client: endpoints pinned at Enable, the secret as an org-scope `connection` row, replace, disable keeps
- [x] CN-5.3 The node exchanges the code: one route takes code, verifier and redirect URI and connects the member
- [x] CN-5.4 Refresh and revoke with the org secret at the pinned token endpoint; single-use refresh tokens spent once
- [x] CN-5.5 the VAM app: client fields at Enable, "admin-supplied client" in the summary, connect without registration
- [x] CN-5.6 Squrell marketplace index read from a named branch (staging nodes only), for the team's Box test
- [ ] CN-5.7 The Box listing ready to tag, and the team's run on a real Box tenant

### Phase 6 — Company address and REST from marketplaces (Zendesk)

**Goal:** a connector reaches a per-company host and a marketplace connector
may be REST with skills (design §13, D30, D31).
**Depends on:** Phase 5.
**Status:** **build done 2026-11-20**: waves 0–5 merged (VAM #93, #95, #98, #103, #106; Squrell-App #14), all five Phase 6 scenarios `active` and green. **VAM released to `main` 2026-11-20** (#112, f016617). The app release (Squrell-App #15) is held for the owner and merges only after the VAM one. CN-6.7: the Zendesk listing is on vam-marketplace `zendesk-connector` (52b0ed0); its tag and the real-instance run are the team's.
**Plan:** drafted 2026-11-19 by the phase-breakdown agent (below).
**Scope:** `profile: "oauth"` with fixed `endpoints` for providers without
metadata; one `tenant` parameter (`{subdomain}`, fixed suffix) entered at
Enable and filled into endpoints, MCP URL and binding hosts; marketplace REST
bindings with exact hosts and per-tool egress shapes; the admin sees the filled
hosts. Salesforce `instance_url` pinning is a candidate item (open question).
**Exit:** a Zendesk REST listing (connector plus skill) works on a mock and,
on the owner's word, a real Zendesk instance.
**Rubric:** cost 3 (a new sign-in profile, marketplace REST trust, the egress
swap for installed connectors, three repos).

- [x] CN-6.1 The connector file: `profile: oauth` with fixed `endpoints`, one `tenant` parameter, marketplace REST bindings, templated skill egress hosts, the capability summary (#95)
- [x] CN-6.2 Enable with a company address: one DNS label, filled once into every host; the admin's client pinned to the filled endpoints (#98)
- [x] CN-6.3 The node exchanges the code for every admin-supplied client (secret or not) and keeps a REST connection for `profile: oauth`; refresh at the filled token endpoint (#103)
- [x] CN-6.4 The egress swap for installed connectors: filled bindings and shapes, per-host token rows, unique placeholders (#106)
- [x] CN-6.5 A skill learns its company host without a guest change (#106)
- [x] CN-6.6 the VAM app: the company address at Enable, the filled hosts, one connect path for admin-supplied clients (Squrell-App #14)
- [~] CN-6.7 The Zendesk listing on a `vam-marketplace` branch, and the team's run on a real Zendesk instance (listing on `zendesk-connector` 52b0ed0; tag and real-instance run: team). Stated exception (orchestrator's decision, release review of #112): staging may import a pre-release tag `zendesk-v1.0.0-rc.N` from the `zendesk-connector` branch, because a staging import needs a tag; `zendesk-v1.0.0` itself is tagged only after the team's real-instance run passes.
- [ ] CN-6.8 (candidate, backlog) Salesforce `instance_url` checked against the company address

#### Phase 6 close

**Release:** VAM done: #112 (`release: connectors Phase 6`; `feat/connectors` with `main` merged in once by #110 after `v0.1.0-rc.2`) merged into `main` as f016617 on 2026-11-20, after three review rounds merged into `feat/connectors` (#113, #116, #117). The app release is still pending: Squrell-App #15 (`feature/connectors` into `main`), held for the owner. **Deploy order: vamd before the app.** Between waves 3 and 5 the node refuses app-held tokens for org clients, and only the wave 5 app connects through the node exchange; an old app against a new node cannot connect an MCP org client with no secret. A new app against an old node has no exchange for REST connectors.

**Scoreboard:** 5 of 5 Phase 6 scenarios green; the Phase 4–5 connector, Microsoft 365 and refresh regressions green; 2 fix cycles in all (wave 2: the runner's no-run `secret-absent` passed vacuously; wave 4: the time-window refresh dedupe replaced by a per-call mark).

**Team checklist (CN-6.7, before the Zendesk tag):** verify the Zendesk facts this session could not reach (scope names, token lifetimes and refresh rotation, PKCE for a secret-less client, the `squrell://oauth` redirect, request and response shapes, revoke) and fix `scopes` or endpoints if needed; set `requiresNode`; move the marketplace CI pin to the Phase 6 merge on `main`; the owner pushes a pre-release tag `zendesk-v1.0.0-rc.N` (stated exception, orchestrator's decision: a staging import needs a tag, so `zendesk-v1.0.0` itself is not tagged before the run); staging import of that tag from the `zendesk-connector` branch with `VAMD_SQURELL_MARKETPLACE_INDEX_REF=zendesk-connector`; the real-instance run (enable the connector (with its company address) before starting the agent's session; Enable, connect, answer from a ticket, create a ticket with `ticket_create` after approving it and check the ticket exists in Zendesk, refresh after expiry, another subdomain refused, Enable-to-Connected time); only after it passes the owner tags `zendesk-v1.0.0`. Failures land as one fix wave.

**Findings and backlog (recorded, not fixed):**
- Salesforce `instance_url` pinning (CN-6.8). `Decide: BACKLOG — proof 1, cost 1`.
- How the client secret is sent to the token endpoint (Phase 5 backlog) stays open: the `oauth` profile has no secret. `Decide: BACKLOG — proof 1, cost 1`.
- ~~Skills from the seed directory get no `.vam/connector.json`; only catalog skills do.~~ Fixed in the release review of #112: a seed skill that needs an enabled company-address connector is copied into the session's staged tree with its hint (the shared seed tree is never written).
- A skill without `requiresConnector` may ship its own `.vam` hint; it is inert (egress checks still apply). `Decide: BACKLOG — proof 1, cost 1`.
- The skills digest (golden snapshot key) now varies by node address. `Decide: BACKLOG — proof P-cold-start, cost 1`.
- The `ticket_create` write path has no scenario; the real-instance run covers it. `Decide: BACKLOG — proof 1, cost 1`.
- `docs/api/openapi.yaml` does not describe the summary's `tenant`, `bindings` and `endpoints`. `Decide: NOW at the next docs pass — proof 1, cost 1`.
- App: an address-only Enable form for a tenant connector with the publisher's client, and a live host preview before save. `Decide: BACKLOG — proof 1, cost 2`.
- From the #112 review (orchestrator's decisions):
  - Stale-URL MCP rows are dropped only for marketplace-installed and tenant-declaring connectors; hand-added rows under a built-in id on another URL keep working (locked Phase 1 Q6). Widening it would reopen Q6. `Decide: BACKLOG — proof 1, cost 1`.
  - A transient failure after the code may have left still answers a retryable 502, and a same-key retry answers "already submitted" (which tells the member to sign in again). A code is single-use, so this is confusing, not unsafe; the pinned "unreachable provider is retryable" behaviour is kept. `Decide: BACKLOG — proof 1, cost 1`.
  - The egress swap carries no credential generation: a re-sign-in in the middle of one call can shorten that call's token horizon. A fix threads a generation through the broker and precheck. `Decide: BACKLOG — proof 1, cost 2`.
  - Warm in-place resume after an admin enables a company-address connector post-boot: the skills drive has no `.vam/connector.json`, so the Ticketdesk/Zendesk tool reports "no company host" (fails closed; egress is authorised at call time). Options: rebuild the drive on a connector change, give the tool a second host source, or cold-start the session when connector state changes. Trigger: a team reports a resumed run that says no company host. `Decide: BACKLOG — proof 1, cost 2`.
  - Round 3 minors (orchestrator's rule: backlog), each `Decide: BACKLOG — proof 1, cost 1`:
    - openapi: limit the metadata-discovery text of a `client` save to MCP connectors (fixed-endpoint REST takes its pins from the filled file); DELETE answers the caller's updated entry, not always `connect`.
    - Ticketdesk mock: accept the access token only with the `Bearer` scheme.
    - Harness absence check: an empty or invalid 200 listing body fails the check instead of counting as read.
    - Mutation responses (`entry_for`): resolve the template against the node row the response shows, as GET does.
    - Startup park recovery: a test that restarts vamd and sees the run resume (the startup pass and its selection are unit-tested; no vamd-restart harness yet).
- **High priority (after the release):** static-connect self-race in `api/mcp_connections.rs` `connect`: the header credential is written to the vault (579-597) before the guarded row write (666-676), so when one member makes two first-time static connects of the same connection at once, the accepted row can hold the header value of the request that got a 409 (and a connect that fails after the write has already replaced a working header). Fix: stage the header credentials into `upsert_mcp_connection_guarded`'s transaction, as #117 did for the deletes. `Decide: NOW — Phase 7 wave 1 (CN-7.1)`.

### Phase 7 — Developer kit and API keys

**Goal:** a third-party developer builds, checks and tests a connector without
a Squrell release; API-token systems can be connected.
**Depends on:** Phase 6.
**Status:** VAM released to `main` 2026-11-21 (#139, 5f1de31). Wave 6 (CN-7.8, the app) rides with the Squrell-App release; wave 7 (CN-7.9, marketplace CI) follows this release. See the Phase 7 close.
**Plan:** drafted 2026-11-20 by the phase-breakdown agent (below).
**Scope:** `vam connector init`, `validate`, `test` (mock provider world
piece, also used by marketplace CI); the `api_key` connection flow
(member-entered key, `connection` row, host-side inject only).
**Scope narrowed (parent orchestrator, owner's delegation, 2026-11-20):** the
key is entered by the member only. A key the admin enters for everyone is an
org-held connection, which stays under "Explicitly later"; `connection.holder`
stays `user` and each member's calls run as their own account (locked
decision 2).
**Exit:** a connector built from nothing passes `validate` and `test` and
installs from a test marketplace.
**Rubric:** cost 3 (a CLI kit that runs the egress gate's rules offline, a new
credential kind on the swap, three repos).

- [x] CN-7.1 Static connect stages its header credentials in the guarded row write (Phase 6 release-review finding)
- [x] CN-7.2 The connector file and the marketplace check: `api_key` with one field, `value: api_key`, REST or MCP
- [x] CN-7.3 One listing check for `validate` and import (the import-only rules move to `vam-spec`; a skill's binding and host must be its connector's)
- [x] CN-7.4 `vam connector init` and `vam connector validate`
- [x] CN-7.5 `vam connector test`: the tools run against the declared mock through the egress gate's own rules, offline
- [x] CN-7.6 The API-key connection for REST connectors: enter, swap, park, replace, disconnect
- [x] CN-7.7 The API-key connection for MCP connectors (the node writes the static row)
- [ ] CN-7.8 the VAM app: an API key at Connect, no client at Enable, the summary's "API key"
- [ ] CN-7.9 vam-marketplace CI runs `vam connector validate` and `test` (team)

#### Phase 7 close

**Release:** VAM released 2026-11-21: #139 (`release: connectors Phase 7`, `feat/connectors` into `main`) merged as 5f1de31 on the owner's word, full CI green on the first run (smoke 187/187 under the 25-minute budget; the release build and install-smoke on Linux and macOS). It carries waves 0–5 (#122, #124, #126, #128, #130, #132), the review fixes (#135), the tracker (#136) and `main` merged in once after the #134 hotfix (#137, full CI). The R1 hotfix itself reached `main` first as #134 (dc84121). Not in this release: wave 6 (CN-7.8, the member's API-key Connect in the app) waits for Squrell-App #15 and ships with the app release; wave 7 (CN-7.9, vam-marketplace CI) pins `VAM_COMMIT` to this release and follows it.

**Scoreboard:** 5 of 5 Phase 7 scenarios `active` and green; the 40 connector-tagged and 15 MCP, API-key and OAuth-token scenarios green on #137; smoke 187 of 187. Fix cycles: W3 1, W4 1, W5 1 (scenario `when`, mock and CLI-test fixes, no `then` edited); review round: #134 3 (CodeRabbit round 1, the token pin, the revoke pin), #135 1 (unquoted openapi descriptions); #137 0 code fixes (one smoke re-run: the 15-minute budget). Agent wall time about 4.4 h for waves 1–5 (73, 20, 37, 81 and 54 min, from the progress rows), plus about 5 h for the review round and the merge, much of it rebuilds after the shared target filled the disk.

**Locked decisions honoured:** decision 7 (an API key is never a guest egress secret: it is a `connection` row read only by the host swap, pinned to the filled hosts); decision 22 (no broker change).

**Team checklist (CN-7.9, after this release):** pin `VAM_COMMIT` in vam-marketplace `validate.yml` to the release merge on `main`; run `vam connector validate --strict` and `vam connector test` on every listing root in CI; fix the Squrell root's one `validate` warning (`calendar_find_slots` READ_ONLY declares POST) before `--strict` gates it.

**Findings and backlog:** recorded in the Phase 7 backlog lines above (review round R5, R6, R10, R12; the smoke budget; the shared cargo target filling the disk; workspace pins on listing update, `Decide: OWNER`). The smoke budget is raised in the close-out PR (below).

## Explicitly later

- Hub, OCI distribution, entitlements, metering from receipts (design §9.3).
- Verified publishers (namespace proof, signing) for marketplace content.
- Event subscriptions and the first event source (design §6.6).
- Teams relay, Google secret relay, GitHub, databases.
- The connector as its own catalog kind with drafts; qualified connection ids;
  org-held connections (service accounts). (From the old Phase 5 scope.)

## Progress log

| Date | Phase | What landed |
|---|---|---|
| 2026-11-20 | 7 | **Breakdown written** (phase-breakdown agent, branch `docs/cn-7-breakdown` from `feat/connectors` f016617), awaiting approval: 5 scenarios (process, mock model; a new `provider: declared` mock built from a publisher's test file), CN-7.1–7.9, waves 0–7 (wave 1 starts with CN-7.1, the static-connect self-race; W1, W3, W4 Sonnet high; W3 runs the microVM lane because the gate's screening code moves to `vam-spec`; W6 Squrell-App, W7 vam-marketplace), 14 open questions with defaults (member-entered key only, narrowing the stated scope; bearer inject only; `vam connector test` offline in the CLI). Folded backlog: Ticketdesk Bearer-only and the no-run absence check (W0), `entry_for` and the openapi items (W4). |
| 2026-11-20 | 7 | **Breakdown approved** by the parent orchestrator (owner's delegation); #118 merged into `feat/connectors` (ff6fcc2). Decisions: member-entered key only (scope narrowed, org-held keys stay later); bearer **and** `inject: header` with a required `headerName` (refused names listed) in wave 4, Basic auth to backlog; no provider call at connect, a declared `verify` to backlog; W6 waits for Squrell-App #15; W3, W4, W5 on Sonnet high. Wave 0 started (branch `cn-7/wave-0`). |
| 2026-11-20 | 7 | **Wave 0 landed** (#122, 8813e6e; decisions PR #119, 8b9c8fa): 5 scenarios pending and red for the right reason (class `product`), the `provider: declared` mock and `vam_spec::connector_kit`, the three listings, the overlay, the `connector-kit` trees, the world pieces and the README contract words; folded Ticketdesk Bearer-only and the no-run absence check. Scoreboard: 0 green / 5 red (right reason), 0 fix cycles. |
| 2026-11-20 | 7 | **Wave 1 landed** (#124, merge 1777e50; CN-7.1 b40ee27, CN-7.2 d69bd77). CN-7.1: a static connect seals its headers and the guarded row write stores them in its own transaction after the check; the route race test and the failed-listing test fail on 8813e6e and pass with the fix (shown in #124). Behaviour change: a connect replay with the same Idempotency-Key and a new value writes the new value. CN-7.2: `api_key` with one field, `value: api_key`, REST or MCP; the marketplace check accepts it; the summary adds `fields` only for `api_key`; `PUT …/node` refuses client and consent. The three key scenarios' `expectFail.step` moved from `when.2` to `when.5` (no `then` edited). Scoreboard: 5 existing connector scenarios green, 3 Phase 7 scenarios red at the key step (right reason), 0 fix cycles, about 73 min agent wall time. |
| 2026-11-20 | 7 | **Wave 2 landed** (#126, merge 4b57fd1; CN-7.3 d7861e9, CN-7.4 fa234e4). CN-7.3: the three import-only listing rules and the duplicate-URN check moved into `vam_spec::marketplace::check_listing(listing_id, …)` with the same words and order (import refuses on the first error); new import refusal: a tool's egress binding and host must be its required connector's (shipped or built in), otherwise an offline warning; offline warning for a connector listing without `tests/connector-test.json`; parity test over the e2e fixtures. CN-7.4: `connector` alias, `vam connector init` (api_key, oauth, discover; Bun or Python; refuses a non-empty directory) and `validate` (`--strict`, `--json`, exit 6); `test` registered only. Kit scenarios' `expectFail.step` moved to `when.2` and `when.4` (no `then` edited). Scoreboard: 4 existing scenarios green, 2 kit scenarios red at `vam connector test` (right reason), 0 fix cycles, about 20 min agent wall time plus a full local re-run after the shared target filled the disk (`cargo clean`). Backlog: the Squrell root's READ_ONLY-declares-POST warning. |
| 2026-11-20 | 7 | **Wave 3 landed** (#128; move 29823ef, route matcher 30bdc58, `vam connector test` c373bb8). The egress envelope's pure screening moved verbatim from `vam-network/src/connection.rs` to `vam-spec/src/connection_envelope.rs`, re-exported so no gate call site changed (orchestrator read it with `--color-moved`; one non-move line, `reqwest::Url::parse` to `url::Url::parse`, the same crate version; the moved tests 19 before and after). `vam connector test` runs each case's tool with a cleared environment against a loopback stub that screens with `Envelope::screen`, swaps in a generated credential, answers from the declared mock and checks that the credential reaches no output or file. Scoreboard: both kit scenarios green (`active`), the four egress and secret scenarios and the microVM lane green, 1 fix cycle (two wave 2 CLI tests that pinned the old `test` stub), about 37 min agent wall time. |
| 2026-11-20 | 7 | **Wave 4 landed** (#130; CN-7.6 37a4291, named header inject 5acdaa2, docs and openapi f214c10, scenario `when` fix 1424ace). A member enters an API key (`PUT …/connection {apiKey}`): one transaction re-checks the plan, writes the vault row `connector/<id>/api-key` pinned to the filled binding hosts and the `connected` row; `token_for` reads the key with the pin check, attaches it after `Envelope::screen` (bearer, or `set_header` into one named header from the connector file), audits `secret.accessed` and scrubs it. Named header: a fixed denylist (Host, Authorization, Cookie, Proxy-*, hop-by-hop) and RFC 9110 tokens only; the guest cannot pick or move the header (refuse in the screen, overwrite every case variant at the gate). The pinned-host scenario makes a member Space before the post-update chat (`create-space` gained `as`), because a Space keeps the agent version of its first run; the approver accepted this `when` change and the two `afterTool` mock rules. Scoreboard: both API-key REST scenarios green (`active`), the wave's other seven and the Space-rename scenario green, 1 fix cycle (the scenario `when`), about 81 min agent wall time plus orchestrator review. Backlog: workspace pins on listing update (owner), startup recovery key-pin check. |
| 2026-11-20 | 7 | **Wave 5 landed** (#132; CN-7.7 7ac6928, scenario mock fix bbb6fbb). A member enters an API key for an MCP connector on the same route: the key is staged and sealed, the tool listing holds it in memory only, vetted tools and the connector policy apply, and `upsert_mcp_connection_guarded` writes the static row and its header in one transaction after the guard (CN-7.1); a failed listing, a refused guard or drift writes no key. `PUT /v1/mcp/connections` and `…/oauth/tokens` refuse an API-key connector id; state reads `connected` from the static row and its header; disconnect deletes both. An MCP connector cannot ask for a named header (`mcp` holds `url` and `tools` only; approver's choice (b), pinned by tests). The approver accepted the mock fix (tool names as the guest sees them, `-` written as `_`). Scoreboard: the MCP API-key scenario green (`active`), the five connector and API-key scenarios green, 1 fix cycle (the mock), about 54 min agent wall time. Backlog: a named header for MCP, stale header rows on replace, the post-transaction policy write. |
| 2026-11-21 | 7 | **Review round.** Two Opus reviewers read waves 1–5; findings R1–R13 went to the orchestrator. **R1 (Critical)**: a static MCP connect could name any vault row as its header binding, and the resolver sent it to the connection's URL. Fixed on `main` as hotfix #134 (`fix/mcp-secret-binding`; 5623777, 6424602, 71375fd): a connection uses only its own binding ids, and every MCP credential is read in one checked read (kind `connection`, pinned to the exact host it is sent to). CodeRabbit round 1 added: cleanup deletes only `connection` rows; foreign bindings refused before any token write; the OAuth refresh reads the refresh token and client secret pinned to the current token-endpoint host; the disconnect revoke sends only to a revocation host pinned on the row (otherwise local delete only). Round 2 and 3: no actionable comments. #134 is green and waits for the owner's word. **R2, R3, R4, R9, R11 and docs R7, R8** merged into `feat/connectors` as #135 (`fix/cn-7-review`, merge 4f6a9d6): an api_key connect replaces the OAuth row an update left behind (409 for any other row); `headerName` refuses every header the egress filter strips (one list in `vam-spec`); where the key goes is part of the summary digest; a disconnect fails and keeps the row while the key survives; `vam connector test` exits 6 when no case ran; the kit, header-inject and connect-route docs. R2 and R9 tests fail before the fix and pass after. **Backlog:** R5, R6 (new `then` lines, approved for a later wave), R10, R12; R13 was already there. Scoreboard: all Phase 7 scenarios still green (35 connector-tagged, 182 and 186 smoke locally, Actions green on both PRs); 3 fix cycles on #134 (CodeRabbit round 1, the token pin, the revoke pin), 1 on #135 (unquoted openapi descriptions found by `local.sh`); about 5 h agent wall time, with three `cargo clean`s after the shared target filled the disk. Next: merge `main` into `feat/connectors` once #134 lands, then the Phase 7 release PR. |
| 2026-11-21 | 7 | **Release prepared.** The owner gave the word; #134 merged into `main` (dc84121). `main` merged into `feat/connectors` once (#137, merge 54bfd08, full CI): CN-7.1's staged connect path now goes through #134's own-id and pinned-host rules (`resolve_mcp_headers_staged` takes the connection's target; non-staged bindings use the pinned decrypt); the CN-7.6 REST `token_for` and the CN-7.7 MCP key connect already met them. 0 code fix cycles; smoke passed on its one re-run after the 15-minute budget cut the first attempt (849 s of 900 s on the green run). Phase 7 close written; the smoke budget raised to 25m in the close-out PR; release PR into `main` next. |
| 2026-11-21 | 7 | **Released to `main`** on the owner's word: #139 merged as 5f1de31. Before it: the close-out #138 (tracker close; smoke whole-run budget 15 to 25 minutes, whose own run used 965 s and would have timed out under the old budget). CodeRabbit skipped #139 (110 files, over its 100-file limit); the orchestrator accepted the review round, #134's three CodeRabbit rounds and #135 as its review. Full CI green on the first run. Open: wave 6 (CN-7.8) with the app release behind Squrell-App #15; wave 7 (CN-7.9) pins vam-marketplace CI to 5f1de31. |
| 2026-11-21 | 7 | **Wave 7 landed** (vam-marketplace #8, merge c0ab323; #9 into `zendesk-connector`). Marketplace CI now builds `vam` at the Phase 7 release (5f1de31) and runs `vam connector validate` (strict, the one accepted `calendar_find_slots` finding) and `vam connector test` next to `vam catalog validate`; the remote MCP listings notion, wetrack and welead ship an empty test file (checked; nothing runs). **cornerstone is excluded** from the gate (`EXCLUDED_LISTINGS`, printed every run): the Phase 7 node refuses to import `cornerstone-v0.1.0` (CN-7.3, 54 errors; see the backlog; the owner was told). microsoft-365 has no connector test (its connector is built in; backlog). At a pre-Phase-7 tag the missing-test-file warning is accepted (a tag cannot change); the current tree still must pass it. #9 merges `main` into `zendesk-connector` (pin 5f1de31, no listing change) and adds a Zendesk connector test from the listing's declared shapes (three cases pass; the key never reached a tool; shape and token handling only, the real-instance facts stay with the team). 1 fix cycle (#8's tag check, which a local check had missed because it dropped the workflow's findings filter). No tags pushed; the owner tags.
