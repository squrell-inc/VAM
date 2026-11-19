# Workstream: Connectors (how VAM reaches the systems people work in)

```yaml
status: active
slug: connectors
current_phase: 5 released to main 2026-11-19 (VAM #52, 4f24c41; Squrell-App #12, bb4ee15), CN-5.7 (the team's Box real-tenant run) is a later manual check that does not block the next phase (playbook §6); 4 build done 2026-11-09 (8 scenarios active and green; waves 0–7 merged), real-tenant wave 9 and lane B wait for the owner; 2 and 3 released to main 2026-11-09, checklists out; Phase 1 released, checklist out
design_docs:
  - design/connectors.md
  - docs/src/content/docs/runtime/tools.md
  - `docs/src/content/docs/hosted/triggered-runs.md`
  - design/jig.md
```

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

#### Phase 1 breakdown (phase-breakdown agent, 2026-11-07; approved by the owner 2026-11-07)

Written against `feat/connectors` (vam) and `feature/connectors` (the VAM app),
both level with `main` (vam). No product code, build or test was
run. Where the code and the design differ, the code is the fact.

**Code map (today).**

- MCP connections: `crates/vam-orchestrator/src/api/mcp_connections.rs`
  (`PUT/GET /v1/mcp/connections`, `…/:id/refresh`, `…/:id/oauth/tokens`;
  scope `secrets:write`; `sideEffects`, `askTools`, `acceptReadOnlyHints`
  are owner-set per request). An OAuth connect with no tokens skips
  `tools/list`; token upsert does not snapshot either.
- `crates/vam-orchestrator/src/mcp_registry.rs`: `validate_connection_id`
  (`[a-z0-9_-]{1,64}`, nothing reserved), guest names `{conn}__{vendor}`
  (`plan_mcp_tools`), `MCP_SEEDED_SIDE_EFFECT = 3` (an unclassified tool is
  external), `seed_mcp_skills` seeds every visible connection with tools for
  the session owner (user row beats org row); no enablement concept.
- `crates/vam-orchestrator/src/mcp_oauth_refresh.rs` `exchange()` always
  sends `resource` (line ~259); Entra must not (locked 11).
- `crates/vam-orchestrator/src/secrets.rs`: `SecretKind::Connection`
  (DX-139), written only by `mcp_registry`, refused by the public secrets
  API, never a guest binding.
- Node setting pattern: `api/allowed_models.rs`, `api/node_notify.rs` +
  `node_notify.rs` (one-row table, `initialize_schema` called from
  `workflow.rs` store open; there is no migrations directory). Org-admin
  authority: `state.pdp.authorize(&auth, "<Action>", USERS_ADMIN_SCOPE)`
  as in `api/org_secrets_admin.rs`; `keys:admin` (operator) passes every
  check, `ORG_ADMIN_SCOPES` holds `users:admin` (`keys.rs`).
- Rule targets: `session_targets.rs` maps MCP tools to
  `mcp_server_id(server_url)`, so a fixed connector URL gives the same
  target on every node.
- Config: `daemon.rs` env-or-flag pattern (`VAMD_MARKETPLACE_DOMAIN` /
  `--marketplace-domain`). A default `vamd` refuses to start with any
  `VAMD_PROCESS_*` variable (`bin/vamd.rs` `reject_process_env`), checked by
  `scripts/e2e/check-vamd-build.sh`.
- `mcp_client.rs` refuses loopback (`PinnedUrl`); no scenario today calls an
  MCP tool, and the runner has no mock provider.
- CLI: `crates/vam-cli/src/cmd/mcp.rs`, `cmd/node.rs` (notify/models
  pattern). Spec data precedent: `spec/jig/*.json` embedded by
  `crates/vam-spec/src/jig.rs`.
- The VAM app: `oauthDiscovery.ts` enforces the RFC 8414 issuer match (~line
  690) and S256 in metadata; Entra `/organizations` fails both, so the Entra
  profile skips discovery through the `discoveredMetadata` option of
  `startOAuthAuthorizeFlow` (`oauthPkceExchange.ts` ~372).
  `buildOAuthAuthorizeUrl` and `exchangeOAuthCodeForTokens` add `resource`
  whenever given and pass `prompt` through. `executeOAuthConnectAndUpsert`
  targets MCP routes only. `OrgSettingsPage.tsx` gates the page on
  `hasAdminScope` (users:admin) and the Notifications place on
  `brokers:admin`. Connections view: `Sidebar.tsx` →
  `McpConnectionsCatalogList.tsx` / `McpConnectionInspector.tsx`. Tauri:
  `src-tauri/src/vamd.rs` builders, `lib.rs` `generate_handler!`. No UI
  scenario layer exists (`design/scenarios.md` §5): app behaviour is proven
  by unit tests, the team checklist and wave 7.

##### Scenario delta

All process mode, mock model. C and D need two new runner-hosted mock
providers and a process-only `vamd` seam (open question 10); nothing here
needs the app or a real Microsoft tenant.

| Scenario | Change | Layer | Given | Why |
|---|---|---|---|---|
| A `connectors-reach-members-only-after-an-org-admin-enables-them` | new | process | `org-member`, **new** `org-administrator` (kind `org`, role `OrgAdmin`: holds `users:admin`, not the operator key) | Locked 3 and the exit. The member lists connectors: `linear` reads "ask your admin"; the member's `PUT` is refused (403) and nothing changes; the org administrator enables it; the member's list reads "connect"; disabled again, it reads "ask your admin". Red at the first list (`/v1/connectors` 404), class `product` |
| B `microsoft-365-waits-for-the-admin-consent` | new | process | `org-member`, `org-administrator` | Consent half of the outcome. Enabled without consent, the member sees "waiting for admin approval"; a consent with a tenant that is not a GUID (or `organizations`) is refused; with tenant T the member sees "connect", and the listing shows profile `entra`, the node's client id, tenant T, `offline_access` in the scopes and no `resource`. Red at the first list (404) |
| C `microsoft-365-connection-stays-in-the-consented-tenant` | new | process (mock provider) | `org-member`, `org-administrator`, **new** `mock-microsoft-login` | Tenant rule (locked 11) and status. Tokens from another tenant are refused (409) and the tile stays "connect"; tokens from T give "connected as <upn>"; `check` refreshes at the mock's `/T/` token endpoint and the mock's log shows no `resource`, no client secret, the configured client id; the mock revokes, `check` gives "needs reconnect". No token value in any listing. Red at the enable step (404) |
| D `linear-reads-run-without-asking-and-writes-ask` | new | process (mock provider) | `org-member`, `org-administrator`, **new** `mock-linear-server`, an agent piece with by-side-effect defaults (reuse one if it fits) | The Linear half of the outcome. Admin enables; the member connects as the app would (fixed id `linear`, tokens); `linear__list_issues` runs with no approval and the mock saw the bearer; `linear__save_issue` parks on an approval; the vendor's unvetted tool is not offered. Admin disables: in a new chat a `linear__list_issues` call is refused as an unknown tool; re-enabled, it runs again without reconnecting. Red at the enable step (404) |
| E `connector-id-cannot-be-taken-by-a-hand-added-server` | new | process | `org-member` | Fixed ids stay meaningful on every node. An OAuth-mode `PUT /v1/mcp/connections` with id `linear` and another URL is refused; with the right URL while `linear` is not enabled, refused ("ask your admin"). Red: both are expected to answer 2xx today (an OAuth connect without tokens skips `tools/list`); wave 0 confirms |

**New world pieces (3):** `org-administrator`; `mock-microsoft-login` and
`mock-linear-server` (kind `mock-provider`, new runner module
`crates/vam-e2e/src/connectors.rs`, std-only loopback HTTP like
`mcp_client.rs` `mock`): the first serves `/{tenant}/oauth2/v2.0/token`,
logs form field names and non-secret values (never tokens), and can revoke;
the second is a minimal streamable-HTTP MCP server with `list_issues`,
`get_issue`, `save_issue`, `save_comment` and one unvetted tool, requiring
the runner-made bearer. Token values are runner-made, kept in memory, never
printed. The runner starts both with `vamd` and passes their addresses in
`VAMD_PROCESS_CONNECTOR_ENDPOINTS` plus a test client id in
`VAMD_CONNECTOR_MICROSOFT_365_CLIENT_ID`.

**New steps (contract with W2–W4, written into `e2e/README.md` "Connectors"):**
`connector-node: { id, enabled, consent: { tenant }, as, refused }` →
`PUT /v1/connectors/{id}/node`; `connector-connect: { id, tenant, upn, as,
refused }` → as the app would (MCP routes for `linear`, `PUT
/v1/connectors/{id}/connection` for `microsoft-365`); `connector-check:
{ id, as }`; `mock-provider: { piece, revoke: true }`; `expect: { connectors:
{ id, as, fields } }` over `GET /v1/connectors`; `expect: { mock-requests:
{ piece, path, has, lacks } }`.

**Existing scenarios whose `then` changes: none.** Checked from `guards`
and `given`: `guest-cannot-use-an-mcp-token-placeholder` (guards
`mcp_registry.rs`, `secrets.rs`) and `broker-cannot-be-given-an-mcp-credential`
use connection id `e2e-mcp`, which is not a connector id, so seeding and
connect are unchanged for them. **Guards-only additions:** add
`vam:crates/vam-orchestrator/src/connectors.rs` and
`vam:crates/vam-orchestrator/src/api/connectors.rs` to both, so a change to
connector storage reruns the credential-leak checks.

##### Benchmark delta

| Line (`design/benchmark.md` §1, P6) | Baseline on this base | Measured at phase close by | Expected after |
|---|---|---|---|
| **new:** minutes and clicks from "Enable" to "Connected" for a member with no instructions | Linear: no Enable exists; a member must be told the URL and type id and URL in Connections → Add, choose OAuth, sign in, then mark read tools by hand or every read asks (a tester times it once on the current base). Microsoft 365: not possible (no Entra profile; discovery fails the issuer and S256 checks) | Team checklist (Linear, team instance) and wave 7 (Microsoft 365, test tenant): a stopwatch from the admin's Enable click to the member's tile reading "Connected as", clicks counted by the tester, member given no steps | Linear: under 2 minutes, admin 1–2 clicks, member about 3 clicks plus sign-in; Microsoft 365: the same plus the admin consent screen |
| P6 tasks through brokered connections with 0 secret bytes in guest output (regression) | as today | `secret-absent` in D and in the two guards-only scenarios | unchanged, 0 |

The orchestrator adds the new line to the P6 row of `design/benchmark.md`
§1 in the breakdown commit (docs only). The design's §10 line runs to "the
first successful mail read on a real tenant"; that half needs Graph calls
and is measured at Phase 4. No paid model is used; the line is human-timed.

##### Items

Shared invariants (every item holds them; the orchestrator checks in review):

- **K1** A connector names no skill, playbook or agent; per-SaaS variation is
  definition data (locked 4, 5).
- **K2** Nothing reaches a member before the node enables it: no Connect, no
  token write, no seeded tool (locked 3).
- **K3** Every connector credential is a `connection`-kind vault row; no token
  value in any response, log or error (locked 7; mcp-oauth implementer
  constraints).
- **K4** Entra: no `resource` on authorize, token or refresh; never
  `prompt=consent` on user sign-in; tenant from the consent record, never
  `common`/`organizations` for a member token; the token endpoint is the
  consent tenant's (locked 11).

**CN-1.1 Connector definition in `vam-spec`.**
- **Scope.** Serde types for design §4 (`urn`, `version`, `displayName`,
  `publisher`, `description`, `icon`; `provider` = `oauth2 { profile:
  entra|discover|google|slack, clientId?, clientSecret: none|squrell_relay,
  scopes, orgConsent: none|admin_required }` or `api_key`; `connection
  { holder, multiAccount, label }`; `bindings?`; `mcp? { url, tools[{ remote,
  sideEffect, default? }] }`; optional `events` accepted as a reserved list,
  unused, §6.6). `validate_connector`: urn shape; exactly one of `bindings`
  and `mcp`; `entra` needs `orgConsent: admin_required` and `offline_access`;
  version one allows `holder: user`, `multiAccount: false` only; unique tool
  names, closed `sideEffect` and `default` sets; exact binding hosts; unknown
  fields refused except `events`. Built-in data `spec/connectors/microsoft-365.json`
  (client id = a documented placeholder sentinel) and `spec/connectors/linear.json`
  (design §4.2 tool list), embedded like `spec/jig/`.
- **Files.** `crates/vam-spec/src/connector.rs` (new), `crates/vam-spec/src/lib.rs`
  (one `pub mod`), `spec/connectors/*.json` (new).
- **Acceptance.** Both built-ins validate; each rule has a failing-input test;
  a definition with `events: [...]` validates; JSON round trip is stable.
- **Tests.** `scripts/ci/local.sh --fast` (`cargo test -p vam-spec`).
- **Turns green.** None alone (unit tests; scoreboard flat, expected).
- **Decide.** `Decide: NOW — proof 2 (new P6 line: no tile without a definition), cost 1 (one wave, a vam-spec module and data, no API)`

**CN-1.2 Node enablement and the connector list.**
- **Scope.** `connectors.rs`: built-in registry from CN-1.1 with node
  overrides: client id from `VAMD_CONNECTOR_MICROSOFT_365_CLIENT_ID` /
  `--connector-client-id microsoft-365=<id>` (`daemon.rs`); the sentinel
  means `configured: false`. Table `node_enabled_connectors` (connector id,
  enabled, consent tenant id, consent recorded at/by, updated at/by), schema
  init in `workflow.rs`. `GET /v1/connectors` (`node:read`): each built-in
  with its public definition, node state (`enabled`, `configured`,
  `consent`) and the caller's state (`askAdmin | waitingForConsent | connect
  | connected | needsReconnect`, label). `PUT /v1/connectors/{id}/node`
  (`users:admin`, idempotency key, `{ enabled, consent?: { tenantId } }`):
  unknown id 404, `configured: false` 409, consent on an `orgConsent: none`
  connector 400, tenant must be a GUID. Process-only seam (under
  `process-guest`): `VAMD_PROCESS_CONNECTOR_ENDPOINTS` maps the Entra
  authority host and the Linear MCP URL to loopback, and lets the MCP client
  and refresh reach those loopback URLs only; marker added to
  `check-vamd-build.sh`. CLI half of CN-1.5. OpenAPI.
- **Files.** `crates/vam-orchestrator/src/connectors.rs` (new),
  `api/connectors.rs` (new), `api/mod.rs` (one merge), `workflow.rs` (one
  init line), `daemon.rs`, `bin/vamd.rs` (seam read), `mcp_client.rs` /
  `mcp_oauth_refresh.rs` (seam hook only, `cfg(feature = "process-guest")`),
  `scripts/e2e/check-vamd-build.sh`, `docs/api/openapi.yaml`,
  `crates/vam-cli/src/cmd/connectors.rs` (new) and `cmd/mod.rs`.
- **Acceptance.** Member `PUT` 403 and state unchanged; org admin (not
  operator) `PUT` 200; listing never carries a vault id or token; default
  `vamd` refuses `VAMD_PROCESS_CONNECTOR_ENDPOINTS`; unit tests for each
  refusal.
- **Tests.** `--fast` while building; `--scenarios
  connectors-reach-members-only-after-an-org-admin-enables-them,microsoft-365-waits-for-the-admin-consent`;
  `--full` and `--release` (release gate) before ready.
- **Turns green.** A, B.
- **Decide.** `Decide: SLICE — proof 2 (P6), cost 2 (new API and table) → slice: list, enable and consent only; no per-tool Allow/Ask/Block editor, no hub listing`

**CN-1.3 MCP connector (Linear).**
- **Scope.** When a `PUT /v1/mcp/connections` id equals an MCP connector's
  id: refused unless enabled (K2) and unless the URL equals the definition's;
  `sideEffects`/`askTools` come from the definition (request values ignored
  for connector rows); `tools/list` runs after token upsert for connector
  rows and keeps only vetted tools. A hand-added row with a connector id and
  another URL is refused. `seed_mcp_skills` skips connector rows while the
  connector is disabled. The caller's state in `GET /v1/connectors` reads the
  row (`connected`, `needs_reauth` → `needsReconnect`). Unit test: a
  trigger-started session (the grant's user as owner, `gateway.rs` path)
  seeds that user's `linear` tools (design §6.6).
- **Files.** `api/mcp_connections.rs`, `mcp_registry.rs`, `connectors.rs`,
  `crates/vam-cli/src/cmd/mcp.rs` (refusal message names `vam connectors`).
- **Acceptance.** Read tools seed as `READ_ONLY`, writes as external with
  `ask`; an unvetted tool is not seeded; existing hand-added rows (`e2e-mcp`)
  unchanged; guest names `linear__<tool>` on every node.
- **Tests.** `--scenarios linear-reads-run-without-asking-and-writes-ask,connector-id-cannot-be-taken-by-a-hand-added-server,guest-cannot-use-an-mcp-token-placeholder,broker-cannot-be-given-an-mcp-credential`; `--full`.
- **Turns green.** D, E.
- **Decide.** `Decide: NOW — proof 2 (P6; read tools stop asking), cost 1 (existing routes and registry, new tests, no new surface)`

**CN-1.4 REST connector connection (Microsoft 365).**
- **Scope.** Table `connector_connections` (owner user, connector id,
  status, label, tenant id, token endpoint, scopes, expires at, updated at).
  `PUT /v1/connectors/{id}/connection` (`secrets:write`, idempotency key;
  access and refresh tokens, `idToken`): refused unless enabled with consent;
  `tid` claim must equal the consent tenant (409), label from
  `preferred_username`; tokens written as `connection`-kind rows
  (`connector/<id>/oauth/*`), id token not stored; token endpoint fixed to
  `https://login.microsoftonline.com/<consent tenant>/oauth2/v2.0/token`.
  `DELETE …/connection` removes rows and bindings.
  `POST …/connection/check`: one refresh through `mcp_oauth_refresh.rs`
  with an Entra profile that omits `resource` and sends the definition's
  scopes; success → `connected`, `invalid_grant` → `needsReconnect`. CLI
  half of CN-1.5 (`disconnect`, `check`).
- **Files.** `connectors.rs`, `api/connectors.rs`, `mcp_oauth_refresh.rs`
  (profile flag), `mcp_registry.rs` (`McpOauthMetadata` profile field if the
  wave reuses it), `secrets.rs` (comment: second writer of `connection`
  kind), `crates/vam-cli/src/cmd/connectors.rs`, `docs/api/openapi.yaml`.
- **Acceptance.** Unit tests on the loopback mock AS: the Entra form has no
  `resource`; the MCP form still has it; a wrong-tenant token writes nothing;
  responses and errors carry no token text; the public secrets API cannot
  read or overwrite the rows.
- **Tests.** `--scenarios microsoft-365-connection-stays-in-the-consented-tenant` plus the two guards-only scenarios; `--full`.
- **Turns green.** C.
- **Decide.** `Decide: SLICE — proof 2 (P6 for Microsoft 365), cost 2 (new route, a second writer of connection-kind rows) → slice: store, tenant check, one explicit check refresh; per-audience tokens, bindings and refresh-before-swap stay in Phase 2`

**CN-1.5 CLI parity (DX-1).**
- **Scope.** `vam connectors list` (id, name, node state, my state, label),
  `enable <id> [--tenant <guid>]`, `disable <id>` (W2); `disconnect <id>`,
  `check <id>` (W4). No CLI sign-in: the app stays the OAuth client
  (`docs/src/content/docs/runtime/tools.md`, CLI deferred).
- **Files.** `crates/vam-cli/src/cmd/connectors.rs`, `cmd/mod.rs`.
- **Acceptance.** Unit tests on the line formatting and request shapes, as
  in `cmd/node.rs`; a member's `enable` prints the 403 text.
- **Tests.** `cargo test -p vam-cli` through `--fast`.
- **Turns green.** None (unit).
- **Decide.** `Decide: BACKLOG — proof 1 (DX-1 parity, no line), cost 0 → carried by W2 and W4, which touch the routes`

**CN-1.6 the VAM app: commands, types, Org settings "Connectors".**
- **Scope.** Tauri builders `list_connectors`, `put_connector_node`,
  `put_connector_connection`, `delete_connector_connection`,
  `check_connector_connection` (registered in `lib.rs`); `src/types/vamConnectors.ts`
  (parse, the five tile states). New place `OrgSettingsConnectorsWorkspace.tsx`
  in `OrgSettingsPage.tsx`, shown on `hasAdminScope` (users:admin, not
  `canEditNodeNotify`): per connector Enable/Disable, "not configured on this
  node" when `configured: false`, and for `orgConsent: admin_required`
  "Record Microsoft admin consent": opens
  `https://login.microsoftonline.com/organizations/v2.0/adminconsent` with
  the node's client id, scope `https://graph.microsoft.com/.default`, state
  from `oauthPendingFlow.ts`, catches `tenant` and `admin_consent` on
  `squrell://oauth`, then `PUT …/node` with the tenant; manual tenant field
  as fallback.
- **Files.** `src-tauri/src/vamd.rs`, `src-tauri/src/lib.rs`,
  `src/types/vamConnectors.ts` (+ test), `src/components/vam/OrgSettingsPage.tsx`,
  `src/components/vam/OrgSettingsConnectorsWorkspace.tsx` (new),
  `src/types/vam.ts` (`AdminWorkspaceView`).
- **Acceptance.** Unit tests: state mapping; consent URL has no `prompt`, no
  `resource`; unknown or expired `state` refused; a member never sees the
  place; builders carry the idempotency key.
- **Tests.** `CI=1 pnpm test`, `pnpm exec tsc --noEmit`,
  `cargo test --manifest-path src-tauri/Cargo.toml`.
- **Turns green.** None (unit; team checklist steps 2 and 6).
- **Decide.** `Decide: NOW — proof 2 (P6: the Enable click), cost 1 (one app wave, the Notifications place as pattern)`

**CN-1.7 the VAM app: Connections tiles and the Entra profile.**
- **Scope.** `ConnectorTiles.tsx` above the MCP list in the Connections view
  (`Sidebar.tsx`), the §3.1 states, Connect / Reconnect / Disconnect, no URL,
  client id or token shown. `discover` (Linear): the existing flow with the
  fixed id and URL from the listing, then `executeOAuthConnectAndUpsert`.
  `entra` (Microsoft 365): new `oauthEntraProfile.ts` builds
  `discoveredMetadata` from the consent tenant (authorize and token endpoints
  under `/<tenant>/oauth2/v2.0/`, client id from the listing, scopes from the
  definition with `offline_access`, method `manual`), so `oauthDiscovery.ts`
  is not called; `buildOAuthAuthorizeUrl` and `exchangeOAuthCodeForTokens`
  get no `resource` and no `prompt` for this profile; after exchange, decode
  the id token and require `tid` = consent tenant and `iss` =
  `https://login.microsoftonline.com/<tid>/v2.0` before `put_connector_connection`;
  tokens dropped from memory after the call.
- **Files.** `src/components/vam/ConnectorTiles.tsx` (new),
  `src/components/vam/oauthEntraProfile.ts` (new, + test),
  `src/components/vam/oauthPkceExchange.ts` (profile option only),
  `src/components/vam/Sidebar.tsx`, `src/components/vam/hooks/useVamInspectors.ts`,
  `src/components/vam/McpConnectionsCatalogList.tsx` (hide the connector's
  row or mark it "managed by connector").
- **Acceptance.** Unit tests: Entra authorize URL and token body never hold
  `resource` or `prompt=consent`; a `tid` mismatch refuses before any `vamd`
  call; the MCP path still sends `resource`; tile states from fixtures.
- **Tests.** as CN-1.6.
- **Turns green.** None (unit; team checklist steps 3–8).
- **Decide.** `Decide: NOW — proof 2 (P6: the Connect click), cost 1 (one app wave; the discoveredMetadata seam exists)`

**CN-1.8 Real-tenant check (last).**
- **Scope.** Needs the owner items (Entra registration, publisher
  verification) and a test tenant. Commit the real public client id into
  `spec/connectors/microsoft-365.json` (not a secret); on the test tenant run
  checklist steps 1, 6 and 7 for Microsoft 365, record the P6 minutes and clicks,
  and confirm design §6.1 items 2, 3, 4, 6 (issuer, `resource`, consent,
  90-day refresh platform) and that the admin-consent redirect to
  `squrell://oauth` works. Fix only what the tenant shows; a fix is one PR in
  the repo it touches.
- **Files.** `spec/connectors/microsoft-365.json`; others only if a fix is
  needed.
- **Acceptance.** The exit's wave 7 clause; findings in the progress log.
- **Tests.** `--scenarios` for A–E after the id change; the manual run.
- **Turns green.** None new (exit evidence).
- **Decide.** `Decide: NOW — proof 2 (P6 measured on Microsoft 365; §6.1 is research until a tenant confirms it), cost 1 (one wave, mostly manual; blocked on owner items)`

##### Waves

One wave at a time; branches `cn-1/wave-N` into `feat/connectors` (vam) or
`feature/connectors` (the VAM app). vam-spec and data land first, vam API and CLI
before the VAM app, the real-tenant check last.

| Wave | Items | Turns green | Repo | Model | Budget | Depends on | Actions | Status | PR |
|---|---|---|---|---|---|---|---|---|---|
| 0 | scenarios: 5 files, 3 world pieces, runner module `connectors.rs` with the two mock providers, steps, README | — (5 red, right reason, class `product`) | vam | Sonnet low | 90 min | — | fast (`vam-e2e`, `e2e/`, `scripts/e2e/`: `skip-vm-lane`) | [x] | — |
| 1 | CN-1.1 | — (unit; flat, expected) | vam | Sonnet low | 60 min | 0 | none (`vam-spec`, `spec/` only: `[skip ci]`) | [x] | — |
| 2 | CN-1.2, CN-1.5 (list, enable, disable) | A, B | vam | Sonnet medium: new API and table, a new `users:admin` authority, and a process-only seam the release gate must still reject; orchestrator reads the `PUT` handler and the seam's `cfg` | 120 min | 1 | fast (`skip-vm-lane`; `--release` run in session) | [x] | — |
| 3 | CN-1.3 | D, E | vam | Sonnet medium: changes the connect path, seeding and the side-effect source that decides what asks | 120 min | 2 | fast (`skip-vm-lane`) | [x] | — |
| 4 | CN-1.4, CN-1.5 (disconnect, check) | C | vam | Sonnet high: a second writer of `connection`-kind vault rows, the refresh token exchange and the tenant rule; orchestrator reads `connectors.rs`, `mcp_oauth_refresh.rs` and the route before merge | 120 min | 3 | fast (`skip-vm-lane`) | [x] | — |
| 5 | CN-1.6 | — (unit) | The VAM app | Sonnet medium: admin consent through the deep-link state store and five new Tauri commands | 90 min | 4 | none (no CI) | [x] | — |
| 6 | CN-1.7 | — (unit) | The VAM app | Sonnet medium: the app's OAuth client drops the issuer check for a tenant check; `vamd` repeats the tenant check, so not high; orchestrator reads `oauthEntraProfile.ts` | 120 min | 5 | none (no CI) | [x] | — |
| 7 | CN-1.8 | — (exit evidence) | vam (the VAM app fix PR only if the tenant shows one) | Sonnet medium: live vendor behaviour, unknown fixes | 90 min | 6 and the owner items | none (`spec/` only); fast if a vamd fix | [~] code merged; manual run pending | — |

Scoreboard expectation: 0 green until wave 2 (2), 4 after wave 3, 5
after wave 4; flat in waves 1, 5, 6, 7 (stated so it is not flagged).

**Files per wave.** 0: `e2e/scenarios/*.yaml` (5), `e2e/world/*.yaml` (3),
`crates/vam-e2e/src/connectors.rs`, dispatch lines in `scenario.rs`,
`engine.rs`, `world.rs`, `vamd.rs` (env), `e2e/README.md`. 1:
`crates/vam-spec/src/{connector.rs,lib.rs}`, `spec/connectors/*.json`. 2:
`crates/vam-orchestrator/src/{connectors.rs,api/connectors.rs,api/mod.rs,workflow.rs,daemon.rs,bin/vamd.rs}`,
seam hooks in `mcp_client.rs` / `mcp_oauth_refresh.rs`,
`scripts/e2e/check-vamd-build.sh`, `docs/api/openapi.yaml`,
`crates/vam-cli/src/cmd/{connectors.rs,mod.rs}`. 3:
`api/mcp_connections.rs`, `mcp_registry.rs`, `connectors.rs`,
`crates/vam-cli/src/cmd/mcp.rs`. 4: `connectors.rs`, `api/connectors.rs`,
`mcp_oauth_refresh.rs`, `mcp_registry.rs`, `secrets.rs` (comment),
`crates/vam-cli/src/cmd/connectors.rs`, `docs/api/openapi.yaml`. 5:
`src-tauri/src/{vamd.rs,lib.rs}`, `src/types/{vam.ts,vamConnectors.ts}`,
`src/components/vam/{OrgSettingsPage.tsx,OrgSettingsConnectorsWorkspace.tsx}`.
6: `src/components/vam/{ConnectorTiles.tsx,oauthEntraProfile.ts,oauthPkceExchange.ts,Sidebar.tsx,McpConnectionsCatalogList.tsx,hooks/useVamInspectors.ts}`.
7: `spec/connectors/microsoft-365.json`.

**Main update 2026-11-07, checked against this breakdown.**
Connectors are in 0.1.0 as a second product lane (0.1.0 launch scope rows
18–21, beside rows 10 E12 Phase 3 and 12 E4 slice); the lane conflict rule
applies: before each wave, check that no other lane is building in its files
(likely overlaps: the VAM app `OrgSettingsPage.tsx`, `Sidebar.tsx`, `lib.rs`,
`vamd.rs`; vam `api/mod.rs` routes). The `docs/src/content/docs/runtime/tools.md`
amendment puts a connector profile's client id ahead of DCR, which matches
CN-1.3/1.4/1.7. It does **not** exempt connectors from "send `resource` on
authorize, token and refresh" (that doc, lines ~117 and ~174): still open,
see open question 12. The egress envelope is Phase 2; Phase 1 needs no
change for it beyond keeping the definition open (CN-1.1 already does).

**Phase rubric.** `Decide: SLICE — proof 3 (in 0.1.0 since 2026-11-07: the 0.1.0 launch plan, which rows 19–21 and the real-tenant launch demo need; plus the new P6 line Enable → Connected), cost 2 (one phase: new API, two tables, both repos) → slice: two built-in connectors, connect and status only; token use (Phase 2), cards (Phase 3), hub, events, Teams and Google later`

##### Open questions (each with a default)

1. **Where the consent tenant id comes from.** Default: the `tenant`
   parameter of Microsoft's admin-consent redirect, caught by the app; a
   manual tenant-id field as fallback. `vamd` accepts a GUID only (no
   `common`, `organizations`, or domain names).
2. **What a member sees for a connector the node has not enabled.** Default:
   the tile is listed with "Ask your admin", name, icon and description only;
   no Connect, no scopes or client id; every connection write is refused.
3. **Linear's vetted list as data.** Default: yes, in
   `spec/connectors/linear.json` (design §4.2: two reads, two writes with
   `ask`). Vendor tools outside the list are not seeded; a new vendor tool
   needs a new definition version, not a member's drift approval.
4. **The client id placeholder.** Default: a sentinel in
   `spec/connectors/microsoft-365.json`; the node overrides it with
   `VAMD_CONNECTOR_MICROSOFT_365_CLIENT_ID` or
   `--connector-client-id microsoft-365=<id>` (`daemon.rs`); with the
   sentinel the connector lists as "not configured" and `PUT …/node` answers
   409, so a fake id never reaches Microsoft. W7 commits the real id.
5. **How the app connects an MCP connector.** Default: the existing
   `/v1/mcp/connections` routes with id = connector id (the app's flow is
   reused); `PUT /v1/connectors/{id}/connection` for REST connectors only.
   Alternative: one connector route for both kinds.
6. **An existing hand-added `linear` row on a node.** Default: if its URL
   equals the definition's it becomes the connector's connection (vetted
   classes applied at its next refresh); otherwise it keeps working until
   disconnected and the tile reads "Needs reconnect"; new hand-added rows
   with a connector id are refused.
7. **Disable.** Default: keeps consent and members' connections, stops
   seeding and every token write; re-enable restores without reconnecting.
8. **Tenant check strength in `vamd`.** Default: `vamd` reads the `tid`
   claim of the id token the app passes (no signature check in Phase 1) and
   fixes the token endpoint to the consent tenant, so a token from another
   tenant fails at the first `check` or Phase 2 refresh. Alternative: verify
   the id token against Entra's keys (a new `vamd` egress host).
9. **Per-tool Allow / Ask / Block on the admin side (§3.1).** Default: not in
   Phase 1; tools follow the vetted defaults and agent rules.
10. **Process-mode mock providers.** Default: `VAMD_PROCESS_CONNECTOR_ENDPOINTS`
    in `process-guest` builds only (a default `vamd` refuses any
    `VAMD_PROCESS_*`), mapping the Entra authority and the Linear URL to the
    runner's loopback mocks and allowing loopback for those URLs only.
    Alternative: no tool call in D (it then proves listing and refusals, not
    "reads run without asking").
11. **"Needs reconnect" for Microsoft 365 in Phase 1.** Default: set only by
    an explicit `check` (tile button, `vam connectors check`); no background
    refresh (mcp-oauth design keeps refresh reactive).
12. **`resource` for Entra vs `docs/src/content/docs/runtime/tools.md`.** That doc
   still says to send `resource` on authorize, token and refresh; locked
   decision 11 says never for Entra, and the 2026-11-07 amendment did not
   cover it. Default: locked decision 11 wins for connectors whose profile is
   `entra`; the owner adds one line to the amendment note.

**Phase close:** unit suites on the merged base; release; team checklist out
(the next phase does not wait for it; playbook §6).

**Team checklist — Phase 1** (written 2026-11-08 from the merged waves 0–6;
Linear steps run now, Microsoft 365 steps 6–7 need the real client id from
wave 7)

Setup: deploy `vamd` from `feat/connectors`, then the app from
`feature/connectors` (vamd first: the app needs `/v1/connectors`). Until
wave 7 commits the real client id, Microsoft 365 shows "not configured on
this node" unless `VAMD_CONNECTOR_MICROSOFT_365_CLIENT_ID` is set on `vamd`
(skip steps 6–7 if it is not). Two accounts: an org admin (role org admin,
not the operator key) and a member.

1. As the member, open Connections. **See:** Microsoft 365 and Linear tiles
   reading "Ask your admin", no Connect button; no `linear` row to edit in
   the MCP list.
2. As the admin, Org settings → Connectors → Linear → Enable (start the
   stopwatch at this click). **See:** "Connector enabled."
3. As the member, Linear → Connect, sign in to Linear. **See:** "Connected
   as <email>"; stop the stopwatch, note minutes and the member's clicks.
4. As the member, ask an agent to list your Linear issues. **See:** the
   answer with no approval card.
5. Ask it to create a Linear issue. **See:** an approval card; deny it.
6. As the admin, Microsoft 365 → Enable, then "Record Microsoft admin
   consent", approve on Microsoft's page. **See:** "Admin consent recorded
   for tenant <GUID>".
7. As the member, Microsoft 365 → Connect, sign in with a work account of
   that tenant. **See:** "Connected as <upn>", no consent prompt for the
   member.
8. As the admin, disable Linear. As the member, start a new chat. **See:**
   the tile reads "Ask your admin"; the agent has no Linear tools.
9. As the member, `vam connectors list`. **See:** the same states as the
   tiles.

Report: "Phase 1 pass", or the step number with a screenshot or the error
text, plus the minutes and clicks from steps 2–3 (P6, Linear).

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

#### Phase 2 breakdown (phase-breakdown agent, 2026-11-08; awaiting owner approval)

Written against `claude/funny-cannon-of6lha`, level with `origin/main` and
`origin/feat/connectors` ; the VAM app read. No product
code, build or test was run. Where the code and the design differ, the code is
the fact.

**Code map (today).**

- Egress provider: `crates/vam-network/src/broker.rs` `BrokerGate`
  (`prepare_with_decision` ~line 505 runs `phase1` (URL placeholders), the
  destination check, then `phase2`; `finish` scrubs the response). One gate
  per session serves both port 54 (legacy `EgressRequest` frames, the guest
  proxy's `X-Target-Url` path) and port 55 (MITM `proxy/mitm.rs` through the
  `EgressGate` trait, `gate.rs`), so a check in `prepare` covers both.
  Placeholder swap: `crates/vam-network/src/secrets.rs`
  `phase1_substitute_url_secrets` / `phase2_inject_secrets` over a static
  `Vec<SecretBinding>` (`proto/vam/v1/network.proto`: `binding_id`,
  `placeholder`, `secret_ref`, `allowed_destinations`, `inject_at`; no
  method or path). `SecretProvider::get_secret` is **sync**; refresh needs an
  async seam.
- Session egress bindings: built once at session boot in
  `crates/vam-orchestrator/src/brokers.rs` ~line 1003 from
  `SecretsVault::session_egress_bindings_for_caller` (`secrets.rs` ~753:
  `egress`-kind rows only, convention placeholders `vamsec_<id>`), into an
  `Arc<Mutex<Vec<SecretBinding>>>`; `VaultSecretProvider` (`secrets.rs`
  ~1279) decrypts per request and audits `secret.accessed`. A connection made
  after boot is invisible to the session today.
- Connections: `crates/vam-orchestrator/src/connectors.rs` (Phase 1). Two
  `connection`-kind rows per user and connector,
  `connector/<id>/oauth/access-token` (pinned to the binding host,
  `graph.microsoft.com`) and `.../refresh-token` (pinned to the login host),
  plus `connector_connections` (status `connected | needs_reconnect`,
  `expires_at`, tenant, token endpoint fixed to the consent tenant).
  `check_connection` refreshes through `mcp_oauth_refresh.rs`
  `refresh_access_token_entra` (blocking reqwest, no `resource`, rotated
  refresh token stored first). The definition's binding is data:
  `spec/connectors/microsoft-365.json` `bindings: [{ name: graph,
  placeholder: vamsec_m365_graph, value: access_token, inject: bearer, hosts:
  [graph.microsoft.com] }]` (`crates/vam-spec/src/connector.rs` `Binding`).
- Tool calls: every guest tool call is authorized on the host first (port 52
  `RequestToolAuthorization`, `security.rs` `handle_tool_intent_request`
  ~1092, with `call_id` and the script digest); the guest runs calls one at a
  time (`vam-runtime/src/agent.rs`, no parallel dispatch). The host catalog is
  `session.rs` `ToolCatalogEntry` / `parse_skills_catalog` (hand-parsed
  `manifest.json`: command, entrypoint, digest, side effect, skill name,
  `approval_required`). F8 (`docs/.../security/agent-autonomy-and-governance.md`):
  the egress gate cannot tell which guest code sent a request; the PEP intent
  is the only per-call fact the host has.
- Reauth park: `security.rs` `park_for_reauth` (~1924) parks a **host MCP**
  call at the intent (approval kind `reauth`, `locked_reason`
  `reauth:<scope>:<owner>:<conn id>`, run PAUSED `waiting_for: REAUTH`,
  outbox event `vam.v1.connection.needs_reauth` (`events.rs:48`));
  `api/reauth.rs` `resolve` / `resume_parked` decide it `RECONNECTED` (no
  grant) when the MCP row is connected again; `workflow.rs` ~1351 accepts only
  `RECONNECTED` or `REJECTED`. Nothing parks for connector connections or skill
  tools.
- Notifications: `notifications.rs` `Kind` (11 kinds, `Kind::ALL`), templates
  and producers; `notification_prefs.rs`; `brokers/vam-notify` is generic over
  kinds (adapter carries `kind` as metadata). No `connection.needs_reauth`
  kind. The VAM app lists the kinds in `src/types/vamNotifySettings.ts`
  `NOTIFICATION_KINDS` and maps them in `vamNotifications.ts` `cardKindFor`
  (unknown kinds get no card).
- Skill admission: `skill_package_admit.rs` `load_and_validate_manifest`
  (`json_to_proto("vam.v1.SkillManifest")`, strict, then `validate()` and
  `skill_manifest_warnings`); `proto/vam/v1/skills.proto` `ToolSpec` uses
  fields 1–13 (next free **14**), `SkillManifest` 1–11 (next free **12**);
  `crates/vam-spec/src/validation.rs` `impl Validate for ToolSpec` (~2711)
  and `for SkillManifest` (~2959). Generated proto code is built into
  `OUT_DIR` by `crates/vam-spec/build.rs` (not committed); baselines in
  `proto/vam/v1/baseline/skill_manifest_*.{json,bin}`.
- Process mode: the guest has its own egress proxy (`VAM_EGRESS_PROXY_SOCK`,
  `process_vmm.rs` ~286) and skills mount; local_exec tools run as host
  subprocesses with `HTTP_PROXY` set to the guest proxy (`vam-runtime/src/exec.rs`
  ~572). No scenario makes a guest egress call today and no world skill has
  files (`vam-e2e/src/forms.rs` `skill_archive` packs `SKILLS.md` and
  `manifest.json` only). The seam `VAMD_PROCESS_CONNECTOR_ENDPOINTS`
  (`process_mode.rs`) knows `microsoft-login` and `linear-server` and refuses
  unknown keys. Process mode cannot prove the MITM proxy's TLS CA
  (`design/scenarios.md` §6): port-55 behaviour is unit-tested
  (`proxy/mitm_tests.rs`) and runs on the lane at the release PR.
- The VAM app: a reauth park shows as a generic `ActionCard` (approval buttons and
  the raw `lockedReason`); no "connect to continue" card; the Connections
  tiles (`ConnectorTiles.tsx`, Phase 1) own the Entra and Linear connect flows.

**Shared invariants** (every item holds them; the orchestrator checks in review):

- **K1–K4 from Phase 1 hold** (no skill named by a connector; nothing before
  enable; connection-kind rows, no token in any response, log or error; Entra
  without `resource`, consent-tenant endpoint).
- **K5** The guest only ever holds the definition's placeholder
  (`vamsec_m365_graph`). The access token is swapped in on the host; the
  refresh token is never a binding and is sent only to the login host.
  Connection rows never enter `session_egress_bindings_for_caller` (locked 7:
  they are not guest egress bindings; the connection binding is a separate
  host-side source).
- **K6** A connection token is attached only to a request inside the shape
  the calling tool declared (binding, exact host, method, path prefix); every
  other request carrying the placeholder is refused before any upstream
  connection, with fixed text and an `egress.decided` DENY naming the tool.
  A tool with no declared shape never gets a connection token.
- **K7** A park or refusal sends nothing: no request reaches Graph from a
  parked call, and a reconnect is not a permission (the call is decided again).

##### Scenario delta

All process mode, mock model. They need one new runner-hosted mock
(`mock-microsoft-graph`) that shares token state with `mock-microsoft-login`,
a skill piece with files (a probe script), and one more key in the
process-only seam (open question 11).

| Scenario | Change | Layer | Given | Why |
|---|---|---|---|---|
| A `skill-reads-microsoft-graph-as-the-member-without-holding-a-token` | new | process (mock providers) | `org-member`, `org-administrator`, `mock-microsoft-login`, **new** `mock-microsoft-graph`, **new** `skill-m365-probe`, **new** `agent-with-m365-probe` | The phase goal. Admin enables Microsoft 365 with consent T; the member connects. The member asks; `m365_probe_search` (READ_ONLY, declared `GET graph.microsoft.com /v1.0/me/messages`) sends `Authorization: vamsec_m365_graph` through the guest proxy. The mock saw `GET /v1.0/me/messages` with `bearer=current`; the thread holds the two mock subjects; `secret-absent` for the access token in thread, events, receipt and workspace. Red: after W1, at the `mock-requests` expect (nothing swaps the placeholder, so the egress gate refuses or the mock sees `bearer=placeholder`), class `product` |
| B `tool-declared-as-a-read-cannot-send-mail` | new | process (mock providers) | as A | K6 and launch row 19. The same read tool, told by its argument to `POST /v1.0/me/sendMail` and then `GET /v1.0/users`, gets a refusal naming the declared shape for each; `m365_probe_undeclared` (no `egress`) gets the same refusal on a `GET /v1.0/me/messages`; the mock saw **no** POST and no request without a matching shape; an `egress.decided` DENY per refusal. Then `m365_probe_send` (MUTABLE_EXTERNAL, `approvalRequired`, declared `POST /v1.0/me/sendMail`) parks on an approval; approved, the mock saw one POST with `bearer=current` (the envelope is per tool, not a blanket block). Red: at the first refusal expect, class `product` |
| C `expired-microsoft-token-is-refreshed-before-the-call` | new | process (mock providers) | as A | Refresh before swap. The member connects with `expiresIn: 0`. One search: the login mock logged one refresh at `/T/oauth2/v2.0/token` with no `resource` and no `client_secret`; Graph saw `bearer=current` (the refreshed token), never `bearer=stale`. A second search inside the new lifetime adds no refresh (count stays 1). Red: at the login `mock-requests` count, class `product` |
| D `revoked-microsoft-connection-parks-the-call-and-tells-the-member` | new | process (mock providers, Slack mock) | as A, plus `slack-bot-token`, `slack-app-token`, `notify-broker` | Reauth park and the new kind; the P6/P13 revoke line on the mock. Connected with `expiresIn: 0`, the login mock revokes. The member asks: the run parks (`waiting_for: REAUTH`, "Reconnect Microsoft 365 to continue"); Graph saw nothing; the tile reads `needsReconnect`; one `connection.needs_reauth` notification (state open) and one Slack DM to the linked contact within 60 s. A second ask while still revoked adds no second notification. The member reconnects (`connector-connect`): the parked call is decided again and runs, the run succeeds, Graph saw exactly one GET, the notification is resolved. Red: at the `waiting_for` expect (today the call runs and fails at egress), class `product` |
| E `skill-needing-microsoft-365-asks-the-member-to-connect-first` | new | process (mock providers) | as A | `requiresConnector` and "connect to continue". Enabled and consented, the member has not connected: the search parks with "Connect Microsoft 365 to continue" and nothing is sent; the member connects; the run resumes and succeeds without a new chat (the connection made after the session started is used). Disabled by the admin, a new chat's search is refused with "ask your admin" and does not park. Red: at the park expect, class `product` |

**New world pieces (3) and a fixture:** `mock-microsoft-graph` (kind
`mock-provider`, provider `microsoft-graph`, in `crates/vam-e2e/src/connectors.rs`):
serves `GET /v1.0/me/messages` (two fixed messages), `POST /v1.0/me/sendMail`
(202) and 404 for anything else; logs method, path and
`bearer=current|stale|placeholder|none` by comparing with the login mock's
latest issued token, never a value. `skill-m365-probe` (kind `skill`, new
`files:` field packed by `forms.rs skill_archive`; fixture
`e2e/world/fixtures/m365-probe/probe.py`, python3 stdlib, calls the guest
proxy with `X-Target-Url`, prints status and body; `requiresConnector:
urn:vam:connector:hub.squrell.cloud/squrell/microsoft-365`; tools
`m365_probe_search`, `m365_probe_send`, `m365_probe_undeclared`).
`agent-with-m365-probe` (allows `load_skill` and the skill's tools; send asks
through its own `approvalRequired`).

**Runner additions (contract with W2–W4, written into `e2e/README.md`
"Connectors"):** `connector-connect` gains `expiresIn`; `mock-requests` gains
`method` and matches the graph mock's `bearer=` tag; the seam key
`microsoft-graph` is held out of the runner's `VAMD_PROCESS_CONNECTOR_ENDPOINTS`
until W2 makes `vamd` accept it (Phase 1 did the same with the whole seam);
until then A–E reach `when` and are red at their first expect.

**Existing scenarios whose `then` changes: none.** Checked from `guards` and
`given`: the egress and secret scenarios (`guest-cannot-use-an-mcp-token-placeholder`,
`secret-never-reaches-guest-encoded`, `skill-cannot-send-a-secret-to-an-unpinned-host`,
`broker-cannot-be-given-an-mcp-credential`) use static egress rows or MCP
rows, which this phase leaves on the existing path; the five Phase 1 connector
scenarios make no guest egress call; the approval and notification scenarios
guarding `security.rs` and `notifications.rs` use tools without
`requiresConnector`. **Guards-only additions:** add
`vam:crates/vam-orchestrator/src/connection_egress.rs` and
`vam:crates/vam-network/src/broker.rs` to `guest-cannot-use-an-mcp-token-placeholder`
and `secret-never-reaches-guest-encoded`, so a change to the token source
reruns the leak checks.

##### Benchmark delta

| Line (`design/benchmark.md` §1) | Baseline on this base | Measured at phase close by | Expected after |
|---|---|---|---|
| P6 tasks completed through brokered connections with 0 secret bytes in guest output | No task exists: a skill cannot reach Graph as the user (no connection binding) | `secret-absent` in A–D, plus the two guards-only scenarios | A–D pass, 0 secret bytes |
| P9 malicious-skill tasks that fail safely (100%): **new task** "a read-declared tool tries to send" | For static egress secrets today, any tool holding a host-pinned placeholder can POST to that host (F8). For connection bindings: nothing to send with | B on every PR | 100% refused, mock saw 0 POSTs from the read tool |
| P6/P13 (design §10 line, to be added): forced revoke parks, the user reconnects, the run resumes with zero duplicate effects | Not possible (no park for connector connections) | D on the mock (one GET after reconnect); the seven-day unattended half is Phase 4 on a real tenant | park, one notification, resume, 1 request |
| P7 decisions the user learned about within a minute | No signal for a broken connection | D: Slack DM `within: 60s` | 1 DM, under a minute |

All lines are mock-measured by scenarios; no paid model and no live tenant.
**Baseline: to run on owner's word** for any real-model or real-tenant
number (none is needed for this phase's exit). The orchestrator adds the
design §10 P6/P13 line and the P9 task to `design/benchmark.md` §1 on
approval (docs only).

##### Items

**CN-2.1 Skill manifest scaffold (proto, validation, unit tests).**
- **Scope.** `SkillManifest.requires_connector` (string, a connector URN; JSON
  `requiresConnector`) and `ToolSpec.egress` (repeated `ToolEgress { string
  binding; string host; repeated string methods; repeated string
  path_prefixes; }`; JSON `egress`). Field numbers: **the next free numbers at
  rebase** (today `ToolSpec` 14, `SkillManifest` 12; if Phase 3's `ToolSpec.ui`
  lands first it holds 14 and this takes 15). Validation lives in a new module
  `crates/vam-spec/src/skill_egress.rs`; `validation.rs` gets one call line in
  each of the two impls. Rules: `requires_connector` matches the connector URN
  shape (reuse `connector.rs`); a tool with `egress` needs a `local_exec`
  provider and a skill-level `requires_connector`; `binding` non-empty; `host`
  an exact lowercase DNS name (no scheme, port, wildcard or IP); `methods`
  non-empty, from `GET HEAD POST PUT PATCH DELETE`, no duplicates; each path
  prefix starts with `/`, has no `..` segment, `//`, `?`, `#` or `%`; a warning
  (`skill_manifest_warnings`) when a READ_ONLY tool declares a method other
  than GET or HEAD. No orchestrator edit; `skill_package_admit.rs` untouched
  (Phase 3's file).
- **Files.** `proto/vam/v1/skills.proto`, `crates/vam-spec/src/skill_egress.rs`
  (new), `crates/vam-spec/src/lib.rs` (one `pub mod`),
  `crates/vam-spec/src/validation.rs` (two call lines); baselines only if the
  `--fast` protos step demands, additive.
- **Acceptance.** Each rule has a failing-input unit test; a manifest with both
  fields round-trips through `json_to_proto`; an existing manifest without
  them validates unchanged; the probe skill's manifest (W0 fixture) validates.
- **Tests.** `scripts/ci/local.sh --fast`.
- **Turns green.** None alone (scoreboard flat, expected); after it A–E fail
  at their first expect instead of at publish.
- **Decide.** `Decide: NOW — proof 3 (launch row 19: "a tool declared as a read cannot send"; P9), cost 1 (one small wave, two fields, one module)`

**CN-2.2 Connection bindings from the connector.**
- **Scope.** New `crates/vam-orchestrator/src/connection_egress.rs`: for the
  session owner, each enabled connector with `bindings` and a `connected`
  connection yields a host-side binding keyed by the definition's placeholder
  (hosts and inject from the definition; value = the binding's access-token
  row, open question 4). Resolved **per request**, so a connection made after
  the session booted is used (E). vam-network: an async
  `ConnectionTokenSource` trait (new `crates/vam-network/src/connection.rs`)
  that `BrokerGate::prepare_with_decision` consults for placeholders the
  static list does not hold, after the destination check and before phase 2;
  `with_connection_tokens` builder; the minted token joins the response scrub.
  `brokers.rs` installs the source at session boot. Static egress bindings,
  `session_egress_bindings_for_caller` and `VaultSecretProvider` unchanged
  (K5). Audit `secret.accessed` with the binding name, never the value.
  Process-only seam: `VAMD_PROCESS_CONNECTOR_ENDPOINTS` key `microsoft-graph`
  maps `https://graph.microsoft.com` to the loopback mock in the gate's
  upstream (process-guest builds only; `check-vamd-build.sh` marker unchanged).
- **Files.** `crates/vam-network/src/{connection.rs (new),broker.rs,lib.rs}`,
  `crates/vam-orchestrator/src/{connection_egress.rs (new),brokers.rs,process_mode.rs,lib.rs}`,
  `connectors.rs` (read accessors only).
- **Acceptance.** Unit tests: the placeholder to a non-binding host is refused
  (confused deputy); a refresh-token row is never returned; no token in an
  error, log or `Debug`; a disabled connector or a `needs_reconnect`
  connection attaches nothing; the MITM path (`mitm_tests.rs`) swaps through
  the same source.
- **Tests.** `--fast`; `--scenarios skill-reads-microsoft-graph-as-the-member-without-holding-a-token,guest-cannot-use-an-mcp-token-placeholder,secret-never-reaches-guest-encoded`; `--full`, `--release` (seam) before ready.
- **Turns green.** A (with CN-2.3).
- **Decide.** `Decide: NOW — proof 3 (the phase goal; P6 tasks through connections), cost 2 (a new trait across vam-network and the orchestrator, a seam key)`

**CN-2.3 Per-call egress envelope.**
- **Scope.** The host catalog entry (`session.rs ToolCatalogEntry`,
  `parse_skills_catalog`, the `session_tool_catalog` row) carries the tool's
  `egress` shapes and the skill's `requires_connector`. When
  `handle_tool_intent_request` authorizes a call (or replays nothing), it
  records the session's **active call window** in `connection_egress.rs`:
  call id, tool, shapes, until now + tool timeout + 30 s; replaced by the next
  intent, cleared at turn commit and session end (open question 1). The token
  source attaches only when the placeholder's binding equals a shape's binding,
  the request host equals the shape's host exactly, the method is listed and
  the normalized path (one percent-decode, dot segments and `//` refused, query
  ignored, case-sensitive) starts with a prefix on a segment boundary; else
  `egress_outside_declared_shape` (fixed text naming the tool and the shape,
  no request upstream, DENY event with rule `tool:<name> egress`). No active
  window, or a window whose tool declares no shape: refused the same way.
- **Files.** `connection_egress.rs`, `security.rs` (one record call at
  AUTHORIZED), `session.rs` (two catalog fields and their parse),
  `workflow.rs` (catalog row columns, if stored), vam-network `connection.rs`
  (method and path in the trait call).
- **Acceptance.** Unit tests: read tool POST refused; prefix `/v1.0/me/messages`
  does not admit `/v1.0/me/messagesX` or `/v1.0/me/messages/../sendMail` or
  `%2e%2e`; host with port or different case; expired window; a send tool's
  window admits its POST; nothing is attached for a tool of another skill.
- **Tests.** `--scenarios tool-declared-as-a-read-cannot-send-mail,skill-reads-microsoft-graph-as-the-member-without-holding-a-token`; `--full`.
- **Turns green.** B (and A with CN-2.2).
- **Decide.** `Decide: NOW — proof 3 (P1 override applies: an undeclared send is a guarded-call violation; owner decision 2026-11-07, question 8), cost 2 (PEP record, catalog fields, path normalization)`

**CN-2.4 Refresh before swap.**
- **Scope.** The token source reads `expires_at`; expired or within 120 s, it
  refreshes through `refresh_access_token_entra` at the consent-tenant
  endpoint (K4) in `spawn_blocking`, **single flight per (owner, connector)**,
  rotated refresh token written first, then the access token and
  `expires_at`. At the intent of a tool whose skill requires the connector,
  the same refresh runs if the token expires within the tool's timeout, so a
  failure parks before anything runs (CN-2.5). A refusal at swap time
  (`invalid_grant` or another 4xx) moves the connection to `needs_reconnect`
  and refuses the request with fixed `connection_needs_reconnect`; a network
  or 5xx failure refuses with `connection_unavailable` and changes no status
  (open question 6).
- **Files.** `connection_egress.rs`, `connectors.rs` (share the check's
  refresh-and-store path), `mcp_oauth_refresh.rs` (no change expected; reuse).
- **Acceptance.** Unit tests on the loopback mock AS: two concurrent swaps make
  one refresh; the form has no `resource` or secret; a rotated refresh token
  survives a failed access-token write; no token text in errors.
- **Tests.** `--scenarios expired-microsoft-token-is-refreshed-before-the-call,microsoft-365-connection-stays-in-the-consented-tenant`; `--full`.
- **Turns green.** C.
- **Decide.** `Decide: NOW — proof 2 (P6/P13: a scheduled run must not fail hourly when the token expires), cost 1 (reuses the Phase 1 refresh; one single-flight map)`

**CN-2.5 Parks: connect to continue, reconnect to continue.**
- **Scope.** In the intent path, before the policy decision (as
  `park_for_reauth` does), for a tool whose skill has `requires_connector`:
  unknown URN or connector not enabled → Denied, fixed "ask your admin to
  enable <name>" (no park; open question 7); no connection → park "Connect
  <name> to continue"; `needs_reconnect`, or the intent refresh of CN-2.4
  fails → park "Reconnect <name> to continue" and emit the existing
  `vam.v1.connection.needs_reauth` outbox event. Park key
  `reauth:user:<owner>:connector:<id>` (`connector:` cannot collide with an
  MCP id, which has no colon). `api/reauth.rs` gains a key-based resolve and
  resume, called after `PUT /v1/connectors/{id}/connection` succeeds; the
  call is decided again on resume (K7). A swap-time failure inside a running
  tool refuses the request (CN-2.4), it does not park mid-call; the next
  intent parks.
- **Files.** `security.rs` (park branch), `hitl.rs` (key helper),
  `api/reauth.rs`, `api/connectors.rs` (resume after connect),
  `connection_egress.rs`.
- **Acceptance.** Unit tests: park before decision (a deny rule is not
  revealed); a park on another user's connector is not resolved; REJECTED
  declines; a stop rejects the park; disabled connector refuses without a park.
- **Tests.** `--scenarios skill-needing-microsoft-365-asks-the-member-to-connect-first,revoked-microsoft-connection-parks-the-call-and-tells-the-member,approval-suspends-and-resumes`; `--full`.
- **Turns green.** E; D with CN-2.6.
- **Decide.** `Decide: NOW — proof 2 (P6: a member who has not connected is asked, not failed; P13 revoke line), cost 1 (the MCP park and resume exist; a second key form)`

**CN-2.6 `connection.needs_reauth` notification kind.**
- **Scope.** `Kind::ConnectionNeedsReauth` (`connection.needs_reauth`,
  severity `action`, action `open` with target `{ kind: connector, id }`) in
  `notifications.rs`; fixed templates over the connector's display name; to
  the connection's owner; **one open row per transition** to needs-reconnect,
  resolved when the connection is connected again. Producers: connector
  connections moving to `needs_reconnect` (check, swap refresh, intent
  refresh) and Linear connector rows on their existing `needs_reauth`
  transition (open question 9). Prefs and node kind defaults
  (`notification_prefs.rs`, `node_notify.rs` if it lists kinds) default
  `immediate`; `vam-notify` needs no change (kind is metadata; the Slack text
  is title, body and **Open in VAM**). OpenAPI kind list; CLI half of CN-2.8.
- **Files.** `notifications.rs`, `notification_prefs.rs`, `connectors.rs` and
  `connection_egress.rs` (producer calls), `mcp_registry.rs` or
  `host_tools.rs` (Linear transition, one call), `docs/api/openapi.yaml`.
- **Acceptance.** Unit tests: one row for two failures; resolved on connect;
  never to another user; Slack payload carries no token or URL with a token.
- **Tests.** `--scenarios revoked-microsoft-connection-parks-the-call-and-tells-the-member,quiet-hours-hold-slack-until-morning,notifications-are-never-shown-to-another-user`; `--full`.
- **Turns green.** D (with CN-2.4, CN-2.5).
- **Decide.** `Decide: NOW — proof 2 (P7: learned within a minute; P2: the park is in the list), cost 1 (one kind on an existing record and channel)`

**CN-2.7 the VAM app: reconnect notification and connect-to-continue card.**
- **Scope.** `connection.needs_reauth` in `NOTIFICATION_KINDS` and a
  `cardKindFor` plan "connection" with **Reconnect** (runs the tile's existing
  connect flow for that connector). In the thread, a pending approval of kind
  `reauth` renders "Connect / Reconnect <name> to continue" with **Connect**
  and **Decline** (no Approve, no Always, no raw `lockedReason`), for MCP and
  connector parks; a successful connect needs no decide call (vamd resumes).
- **Files.** `src/types/{vamNotifications.ts,vamNotifySettings.ts,vam.ts}` (+
  tests), `src/components/vam/{NotificationsPanel.tsx,ChatArea.tsx,ConnectorTiles.tsx}`,
  `src/components/vam/ConnectToContinueCard.tsx` (new), `ActionCard.tsx` (route
  reauth parks away).
- **Acceptance.** Unit tests: kind mapping, card plan from fixtures, Decline
  sends REJECTED with a reason, no token or client id rendered.
- **Tests.** `CI=1 pnpm test`, `pnpm exec tsc --noEmit`, `cargo test --manifest-path src-tauri/Cargo.toml`.
- **Turns green.** None (unit; team checklist).
- **Decide.** `Decide: NOW — proof 2 (P6: a non-developer reconnects from the message), cost 1 (one app wave; the tile flow exists)`

**CN-2.8 CLI parity (DX-1).**
- **Scope.** `vam connectors list` already shows `needsReconnect`; the run
  list shows `waiting_for: reauth` with the connector name; `vam notifications`
  lists the new kind.
- **Files.** `crates/vam-cli/src/cmd/{connectors.rs,runs.rs or notifications.rs}` only if output needs it.
- **Acceptance.** CLI unit tests on line formatting.
- **Tests.** `--fast`.
- **Turns green.** None.
- **Decide.** `Decide: BACKLOG — proof 1 (DX-1 parity, no line), cost 0 → carried by W4, which touches the routes`

##### Shared surfaces (Phase 3 lane runs at the same time)

Connectors Phase 3 ("Cards from skills": `ToolSpec.ui`, the `List`
primitive in `vam/1`, jig sources in skill packages checked at publish) runs
as a parallel lane in another session. Conflict rule
(`phase-playbook.md` §11.2): two lanes never build in the same file at the
same time; the later lane holds its build waves (breakdown and scenarios may
proceed). Before each wave the orchestrator checks the Phase 3 tracker rows
and open PRs for the files below.

- `proto/vam/v1/skills.proto` (`ToolSpec`, `SkillManifest`): **both phases.**
  Phase 2 edits it only in **W1**.
- `crates/vam-spec/src/validation.rs` (`impl Validate for ToolSpec` /
  `SkillManifest`): **both phases.** Phase 2 edits it only in **W1**, two
  call lines; its rules live in `skill_egress.rs`. Phase 3 is asked to do the
  same (its own module, one call line).
- `proto/vam/v1/baseline/skill_manifest_*`: W1 only, and only if the protos
  check demands it; additive.
- Generated proto code: built into `OUT_DIR`, not committed; no conflict
  beyond the `.proto`.
- `crates/vam-orchestrator/src/session.rs` (`ToolCatalogEntry`,
  `parse_skills_catalog`): **likely both** (Phase 3 needs `ui` on the host
  catalog to attach cards). Phase 2: **W2**.
- `crates/vam-orchestrator/src/security.rs` and `hitl.rs` (intent and
  suspend): **likely both** (Phase 3's richer approval card on a suspended
  call). Phase 2: **W2** (one record call), **W4** (park branch).
- `crates/vam-e2e/src/{scenario.rs,engine.rs,world.rs,forms.rs}`,
  `e2e/README.md`: **both wave 0s** (Phase 3 also packs jig sources into skill
  archives). Phase 2: **W0**: additive fields at the end, the `files:` field on
  skill pieces is shared; whichever lands first, the other reuses it.
- `docs/api/openapi.yaml`: additive in W2/W4; rebase.
- The VAM app `src/types/vam.ts`, `ActionCard.tsx`, `ChatArea.tsx`: **likely
  both** (Phase 3 renders cards in the thread). Phase 2: **W5**.
- Phase 3 only (Phase 2 does not edit): `spec/jig/catalog.json`,
  `crates/vam-spec/src/jig*`, the `vam/1` host and renderer code,
  `skill_package_admit.rs`, `brokers/vam-notify` adapters.
- Phase 2 only: `crates/vam-network/**`, `connection_egress.rs`,
  `connectors.rs`, `api/connectors.rs`, `api/reauth.rs`, `notifications.rs`,
  `process_mode.rs`, `brokers.rs`.

**Coordination point.** Whichever lane's proto and validation wave merges
first, the other rebases onto it before its own proto wave; neither reorders
or renumbers the other's fields; each takes the next free numbers at rebase.
Phase 2's W1 is kept to proto fields, validation and unit tests (60 min) so
the hold window is short and Phase 3 can rebase on it.

##### Waves

One wave at a time; branches `cn-2/wave-N` into `feat/connectors` (vam) or
`feature/connectors` (the VAM app). Wave rule for every row with a shared file:
**HOLD: shared file `<path>`** if a Phase 3 build wave is open in it when the
wave is due; else go.

| Wave | Items | Turns green | Repo | Model | Budget | Depends on | Actions | Shared surfaces | Status | PR |
|---|---|---|---|---|---|---|---|---|---|---|
| 0 | scenarios: 5 files, 3 world pieces, probe fixture, `files:` on skill pieces, graph mock in `vam-e2e/src/connectors.rs`, `expiresIn`, `method`, README, guards-only edits | — (5 red, right reason, class `product`) | vam | Sonnet low: follows Phase 1 wave 0 (mock providers, steps) | 90 min | — | fast (`vam-e2e`, `e2e/`: `skip-vm-lane`) | `vam-e2e` `scenario.rs`, `engine.rs`, `world.rs`, `forms.rs`, `e2e/README.md`: additive; scenarios may proceed under the conflict rule, rebase on Phase 3's wave 0 if it lands first | [x] | — |
| 1 | CN-2.1 | — (unit; flat, expected) | vam | Sonnet low: two proto fields, one validation module, unit tests on an existing pattern | 60 min | 0 | fast (`proto/` is a VM path: `skip-vm-lane`) | **HOLD: shared file `proto/vam/v1/skills.proto`** and **`crates/vam-spec/src/validation.rs`** if a Phase 3 build wave is open in them; else go. Next free field numbers at rebase | [x] | — |
| 2 | CN-2.2, CN-2.3 | A, B | vam | **Sonnet high**: the security boundary (secrets and egress): this wave builds where the user's token is attached and the envelope that decides it, across vam-network and the PEP; a mistake leaks a bearer or lets a read send. Built together so no build ever attaches a connection token without the envelope. Orchestrator reads `connection.rs`, `connection_egress.rs`, the `prepare_with_decision` change and the path normalizer before merge | 150 min | 1 | fast (`skip-vm-lane`; `--release` run in session for the seam). The port-55 path is unit-tested here and runs on the lane once in the release PR | **HOLD: shared file `session.rs`** or **`security.rs`** if a Phase 3 build wave is open in them; else go | [x] | — |
| 3 | CN-2.4 | C | vam | **Sonnet high**: secrets plus concurrency (single-flight refresh, rotated refresh token ordering) | 120 min | 2 | fast (`skip-vm-lane`) | none shared | [x] | — |
| 4 | CN-2.5, CN-2.6, CN-2.8 | D, E | vam | Sonnet medium: run state (park and resume) and a new notification kind across the PEP, reauth, connectors and notifications. Not high: the park runs before the decision and changes none, reusing the MCP park; rerun at high if it fails twice. Orchestrator reads the park branch in `security.rs` | 150 min | 3 | fast (`skip-vm-lane`) | **HOLD: shared file `security.rs`** / **`hitl.rs`** if a Phase 3 build wave is open in them; `openapi.yaml` additive | [x] | — |
| 5 | CN-2.7 | — (unit) | The VAM app | Sonnet medium: crosses the notifications panel, the thread cards and the tile connect flow, and changes how a reauth approval is decided | 120 min | 4 | none (the VAM app unit CI only) | **HOLD: shared file `src/types/vam.ts`**, **`ActionCard.tsx`** or **`ChatArea.tsx`** if a Phase 3 app wave is open in them; else go | [x] | app |

**Runner switches from wave 0 (owner, 2026-11-08; part of the named waves' acceptance).** Both are `false` on `feat/connectors` after; the wave that adds the behaviour flips its switch in the same PR, else its scenarios stay red for a runner reason:

- `PASS_SKILL_CONNECTOR_FIELDS` (`crates/vam-e2e/src/forms.rs`): published skill manifests keep `requiresConnector` and per-tool `egress`. **Flipped in wave 1** (CN-2.1, once vam-spec admits the fields).
- `PASS_GRAPH_ENDPOINT` (`crates/vam-e2e/src/connectors.rs`): the `microsoft-graph` key in `VAMD_PROCESS_CONNECTOR_ENDPOINTS`. **Flipped in wave 2** (once vamd accepts the key).

**Fixed observables (owner, 2026-11-08):** B's refusals show the text "declared shape"; D and E observe the park as `run: paused` + `approval: pending`. Wave 4 checks that a reauth park lists as a pending approval; if not, a one-line spec change approved by the orchestrator, never an edited `then`. D tests "no second notification" with an explicit `connector-check`, not a second chat (a second parked run would also resume on reconnect).

Scoreboard expectation: 0 green after W0 and W1 (flat in W1, stated so it is
not flagged); A, B after W2; C after W3; D, E after W4 (all 5); flat in W5.

**Files per wave.** 0: `e2e/scenarios/*.yaml` (5), `e2e/world/*.yaml` (3),
`e2e/world/fixtures/m365-probe/probe.py`, `crates/vam-e2e/src/{connectors.rs,forms.rs,world.rs,scenario.rs,engine.rs}`,
`e2e/README.md`, two guards-only edits. 1: `proto/vam/v1/skills.proto`,
`crates/vam-spec/src/{skill_egress.rs,lib.rs,validation.rs}`. 2:
`crates/vam-network/src/{connection.rs,broker.rs,lib.rs}`,
`crates/vam-orchestrator/src/{connection_egress.rs,brokers.rs,process_mode.rs,security.rs,session.rs,workflow.rs,lib.rs}`,
`docs/api/openapi.yaml` (if needed). 3: `connection_egress.rs`,
`connectors.rs`. 4: `security.rs`, `hitl.rs`, `api/reauth.rs`,
`api/connectors.rs`, `notifications.rs`, `notification_prefs.rs`,
`connection_egress.rs`, one Linear transition call, `docs/api/openapi.yaml`,
`crates/vam-cli/src/cmd/*` if needed. 5: the VAM app files of CN-2.7.

**Phase rubric.** `Decide: NOW — proof 3 (launch row 19: "a skill reaches Graph as the user without holding a token, and a tool declared as a read cannot send"; rows 21 and the launch demo need it; P1 override for the envelope), cost 2 (one phase: a new egress seam across two crates, the PEP, a notification kind, one app wave)`

##### Open questions (each with a default)

1. **How a request is tied to a tool call.** Default: the host's active call
   window from the PEP intent (one per session; the guest runs calls one at a
   time today), replaced at the next intent, cleared at turn commit, session
   end or tool timeout + 30 s. Residual risk, recorded as a finding: a
   background process left by a read tool could ride a later, approved send
   window. Alternative: a per-call placeholder minted at the intent and passed
   to the tool's environment (stronger, but a guest change, a rootfs rebuild
   and a different skill code form than design §4.1). When parallel tool
   calls arrive (launch row 13, H3), tools with connection shapes serialize.
2. **Method rules by side-effect class.** Default: no hard rule; a READ_ONLY
   tool may declare POST for a read endpoint (Graph `findMeetingTimes` is a
   POST that reads), publish warns, and the envelope still pins it to the
   declared path, so it cannot reach `sendMail`.
3. **Path matching.** Default: prefix on a segment boundary, one
   percent-decode, dot segments and `//` refused, query and fragment ignored,
   case-sensitive, host exact (no port, no subdomain).
4. **Per-audience token rows.** Default: version one has one binding
   (`graph`), so the existing `connector/microsoft-365/oauth/access-token` row
   is its token; a connector with a second binding gets
   `connector/<id>/oauth/<binding>/access-token`, minted from the same refresh
   token with that audience's scopes. No migration in this phase.
5. **Refresh timing.** Default: refresh at swap when within 120 s of expiry,
   and at the intent when the token expires within the tool's timeout; no
   background scheduler.
6. **A refresh that fails inside a running tool.** Default: the request is
   refused with fixed text, the connection becomes `needs_reconnect`, the
   notification is raised, and the next intent parks; no mid-call park.
7. **`requiresConnector` when the connector is not enabled or not consented.**
   Default: refused with "ask your admin to enable <name>", no park (the member
   cannot fix it); no notification to the admin in this phase.
8. **Static egress secrets.** Default: the envelope applies to connection
   bindings only (scope of question 8); static `egress` rows keep today's
   host pinning, and F8 for them goes to the backlog as a finding.
9. **Producers of `connection.needs_reauth`.** Default: connector-backed
   connections (Microsoft 365, and Linear connector rows on their existing
   `needs_reauth` transition); hand-added MCP rows keep the outbox event only.
   One open row per transition, resolved on reconnect.
10. **Where `requiresConnector` is checked.** Default: shape at publish in
    vam-spec; resolution at the intent (unknown URN → refused). No edit to
    `skill_package_admit.rs`, which stays Phase 3's.
11. **Mock Graph in process mode.** Default: the seam gains key
    `microsoft-graph` (plain http loopback standing in for
    `https://graph.microsoft.com` in the egress gate's upstream, process-guest
    builds only; a default `vamd` still refuses any `VAMD_PROCESS_*`); the
    runner holds the key until W2.
12. **Probe tool language.** Default: python3 stdlib, calling the guest proxy
    with `X-Target-Url` (no TLS CA needed in process mode). Alternative: `sh`
    and `curl`. Wave 0 confirms python3 is on the guest tool path in CI.
13. **Locked decision 7 wording.** It says connector credentials are "never
    guest egress bindings", while design §10 says "bindings from the connector
    become session egress bindings". Default: both hold as K5 states (the
    guest gets only a placeholder; the host-side source is not an egress vault
    row); the owner may add one clarifying line to locked 7.

**Phase close:** unit suites on the merged base; release (the release PR runs
the lane once, which covers the port-55 path); team checklist out (the next
phase does not wait for it; playbook §6).

#### Phase 2 close

**Scoreboard:** 5 scenarios added, 5 `active` and green; waves 0–5, 0 fix cycles; about 13 h wall time on 2026-11-08 (two container restarts, one disk-full incident). GitHub Actions: only the fast jobs triggered by `skip-vm-lane` on ready PRs; no microVM or benchmark lane.

**Findings and backlog (recorded, not fixed):**
- A background process left by a read tool can ride a later call window of the same session (fix: per-call placeholder minted at the intent; needs a guest change). `Decide: BACKLOG — proof P1, cost 2, line F8`.
- vamd should expose a structured reauth approval kind and connector id; the app parses `lockedReason` today and falls back to the old card if it cannot. `Decide: BACKLOG — proof P6, cost 1`.
- Port 55 does not merge repeated `Authorization` lines; large port-55 responses stream unscrubbed (pre-existing); legacy-policy sessions cannot reach Graph; a declared prefix `/` admits the whole host.
- `expires_at = None` is never refreshed at swap (the app always sends `expiresIn`); the connection lock is held up to ~10 s during a refresh.
- Unit-test gaps covered only by scenarios: the park precedes the policy decision; a two-owner resume.

**Team checklist — Phase 2** (draft; handed over at release)

Setup: deploy vamd from `main` (after the release), then the app from `main`. The node already has `microsoft-365` enabled with admin consent from the Phase 1 checklist; a test skill that calls Graph (the Phase 4 `m365-mail` skill is not there yet, so use the probe skill from `e2e/world/skill-m365-probe.yaml` published to the node) and an agent that allows it.

1. As a member who has **not** connected Microsoft 365, ask the agent to read your mail. You see a card "Connect Microsoft 365 to continue" with Connect and Decline only.
2. Click Connect and finish the Microsoft sign-in. The run continues on its own and shows your recent mail subjects. No approve click.
3. Ask the agent to send a mail using the read tool. The tool reports the request is outside its declared shape; nothing is sent (check Sent Items).
4. Ask it to send with the send tool. You get an approval card; approve it; the mail arrives.
5. In the Entra portal, revoke the app's sessions for your user (or remove its consent), then ask the agent to read mail again. The run pauses with "Reconnect Microsoft 365 to continue", a "Reconnect" notification appears in the app, and a Slack DM arrives (if Slack notify is linked).
6. Click Reconnect from the notification and sign in. The paused run continues and the notification clears.
7. Leave the connection idle for over an hour, then ask the agent to read mail. It works without a reconnect (the token was renewed).
8. As an admin, disable `microsoft-365` in Org settings, then ask the agent to read mail as the member. The answer says to ask your admin; no card. Re-enable it afterwards.

Report: "Phase 2 pass", or the step number with a screenshot or the error text.

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

#### Phase 3 breakdown (phase-breakdown agent, 2026-11-08; approved by the owner 2026-11-08)

Written against `feat/connectors` (vam) and `feature/connectors`
(the VAM app), both level with `main`. No product code, build or test
was run. Phase 2 has no breakdown yet; its files below are inferred from
`design/connectors.md` §10 and must be re-checked when it lands. Where code and
design differ, the code is the fact.

**Code map (today).**

- Jig: `spec/jig/{catalog.json,ir.schema.json,ir.d.ts,builtins/*.json}` embedded
  by `crates/vam-spec/src/jig.rs` (tests in `src/jig/tests.rs`). The IR is JSON
  (flat `nodes` map, `children` ids, `$data` / `$state` bindings); there is **no
  `.jig` text parser**. `check_ir` already resolves every `$data` pointer
  against the IR's `inputSchema` (`resolve_schema`), so "fits the tool's output
  schema" is that check with the tool's schema as `inputSchema`. `check_data` /
  `check_result` work for built-ins only. No `List`; `catalog_digest()` hashes
  every `spec/jig` file and the VAM app pins that digest
  (`src/jig/specDigest.ts`, test fails on any change).
- `proto/vam/v1/skills.proto` `ToolSpec` uses fields 1–13 (`output_schema_json`
  = 8, optional). Manifest checks: `impl Validate for ToolSpec` and `for
  SkillManifest` in `crates/vam-spec/src/validation.rs`. Generated code is in
  `OUT_DIR` (not checked in); no manifest JSON schema file; OpenAPI
  `ToolSpec` at `docs/api/openapi.yaml` ~4423.
- Publish: `POST /v1/skills/packages` → `skill_package_admit.rs`
  (`load_and_validate_manifest` parses `manifest.json` with `json_to_proto`,
  then `SKILLS.md`); the tarball is stored by digest (`skill_packages.rs`).
  Manifest-relative paths exist for `playbooks` but nothing reads them.
- Outbox: guest `show` → port-52 `PostOutbox` → `crates/vam-orchestrator/src/outbox.rs`
  re-checks (built-in Show jigs only, `human.jig`, 16 KiB, 60/min) and emits
  `outbox.card {jig, data, card_id?}`. Host-run tool results come back in
  `host_tools.rs` (`emit_host_tool_completed`); guest-run results reach the
  host first as tool messages in `thread_recorder.rs`.
- HITL: `ApprovalRequestClaims` (`hitl.proto`, fields 1–20) has
  `amendable_schema`; `HITL_DECISION_AMENDED` exists; `hitl.rs`
  `validate_amended_args` limits keys to the schema; tool-call suspends use
  `default_amendable_schema` (every arg a string); resume runs
  `amended_args_json` when set (`api/mod.rs` `approved_tool_args_json`); the
  receipt says `approved-amended`; `vam approve decide --amended-args` exists.
  No scenario amends a tool call; the runner has no amend step.
- Channels: `notify_delivery.rs` `renders_natively` sends `jig_ir` to Slack
  only when every node is in the Slack set (no `List`) and the only action is a
  submit (NT-2.10); `notify_gateway.rs` never takes AMENDED from a channel.
- e2e: `kind: skill` pieces are packed by `crates/vam-e2e/src/forms.rs`
  `skill_archive` (manifest + `SKILLS.md` only) and published and released.
  Host adapter `echo` returns its arguments as JSON, so a test tool's result is
  whatever the mock model's call carries. `WorldPiece` is `deny_unknown_fields`.
- The VAM app: `src/jig/{check.ts,paint.ts,JigRenderer.tsx,registry.ts}` mirror the
  checker and paint IR; `src/components/vam/outbox.ts` accepts `vam.progress`
  cards only; `hooks/chatInbox.ts` `reduceOutboxEvents` rebuilds cards from run
  events; `ActionCard.tsx` paints `vam.approve`; an amend path exists for the
  start-playbook card (`StartPlaybookCard.tsx`, `useVamChat.ts` `handleAmendAction`).

##### Scenario delta

All process mode, mock model. One new test skill whose tools are host `echo`
tools (no Microsoft, no new adapter): `mail_list` (`ui.show: mail_list`),
`mail_send` (`approvalRequired`, `ui.ask: send_approval`),
`calendar_find_slots` (`ui.show: slot_list`), `calendar_book`
(`approvalRequired`, `ui.ask: book_approval`), each with an `outputSchemaJson`,
and its jig files in the package.

| Scenario | Change | Layer | Given | Why |
|---|---|---|---|---|
| A `mail-list-result-shows-as-the-skills-card` | new | process | `org-admin`, **new** `skill-mail-calendar-cards`, **new** `agent-with-mail-calendar-cards` | The phase's first half. The skill publishes; the agent calls `mail_list` (three messages); the run's events carry `outbox.card` with the skill's jig, the checked IR (a `List`) and the tool result as data; the model made no `show` call. A second call whose result breaks the card's schema adds no card, a `card.skipped` event names the tool, and the tool result still reaches the model. Red at when.0 (`publish-skill`: `ui` is an unknown field today), class `product` |
| B `send-asks-with-the-skills-card-and-an-edit-runs-as-amended` | new | process | as A | The phase's second half. `mail_send` waits; the pending approval carries the card with `to`, `subject`, `body` bound from the args; an amend of a field the card does not offer is refused (400) and the approval stays pending; an amend of `subject` decides AMENDED, the tool result holds the edited subject and not the draft, the receipt reads `approved-amended`. Red at when.0 today; after wave 3, at the card fields of the pending approval |
| C `booking-asks-with-the-skills-card-and-approve-books-it-unchanged` | new | process | as A | Calendar half. `calendar_find_slots` shows the slot card; `calendar_book` waits with its card; Approve with no edit decides APPROVED and the tool result holds the original args. Red as B |
| D `a-skill-whose-card-does-not-fit-its-tool-is-refused-at-publish` | new | process | `org-admin`, as A, **new** `skill-with-cards-that-do-not-fit` (several pieces or one piece per case) | Publish-time check (Decision CN-3.3). The good skill publishes; refused (400, the message names tool, file and JSON pointer): a `ui.show` jig file that is missing, one with an unknown primitive, one binding `/messages/0/sender` that the tool's output schema lacks, and an ask card whose submit names a field the tool's input schema lacks. Red at when.0 (the good skill is refused today) |

**New world pieces (3):** `skill-mail-calendar-cards`, `agent-with-mail-calendar-cards`
(rules allow `skill:<name>/*`), `skill-with-cards-that-do-not-fit`.
`WorldPiece` gains `files: { "<path>": <json> }`, packed by `skill_archive`
next to the manifest.

**New steps (contract with waves 3–5, written into `e2e/README.md` "Cards"):**
`publish-skill: { piece, refused, contains }` (publishes and releases a skill
piece in `when`, so a refusal is `product`); `approve: { amend: { <field>:
<value> }, refused }` → decide AMENDED with `amendedArgs`; `expect: { approval:
pending, tool, fields: { card.jig, card.data.<path> } }` uses the existing
`fields` matcher; `expect: { event: outbox.card | card.skipped, fields }` and
`expect: { thread: { role: tool, contains } }` exist.

**Existing scenarios whose `then` changes: none.** Checked from `guards` and
`given`: no scenario publishes a skill with `ui` or amends a tool call.
**Guards-only additions:** `progress-shows-while-the-task-runs` gains
`vam:crates/vam-orchestrator/src/outbox.rs`;
`simple-jigs-render-in-slack-and-the-rest-open-in-vam` gains
`vam:crates/vam-spec/src/jig.rs` and `vam:crates/vam-orchestrator/src/notify_delivery.rs`
(its `jig/**` glob misses `jig.rs`); `approval-suspends-and-resumes` already
guards `security.rs`, add `vam:crates/vam-orchestrator/src/hitl.rs`.

##### Benchmark delta

| Line (`design/benchmark.md` §1, P14) | Baseline on this base | Measured at phase close by | Expected after |
|---|---|---|---|
| **new P14 task:** model turns and output tokens to show a mailbox and send one mail with one correction, card vs chat | Chat only: the model restates the list in prose, the person types the correction, the model calls send again (structurally 2 extra turns plus the list tokens). A scored baseline needs a real model on two models: **owner's word** (§10); not run | the same task with the test skill (mock world), scored on two models on the owner's word; in CI, scenario B records the structural count (one `mail_send` call, decided AMENDED, no second send) | card path: 0 list tokens from the model, 0 extra turns for the correction; chat path unchanged |
| P14 "turns and tokens to collect a structured input, jig vs chat" (existing; purchase-requisition gate) | unchanged | not touched by this phase (gate jigs are E11) | unchanged, written down as a finding |

The orchestrator adds the new task line to the P14 row of `design/benchmark.md`
§1 when the owner approves (docs only). The mock model proves the path, not
model behaviour; no paid endpoint is used before the owner says so.

##### Items

Shared invariants (every item holds them; the orchestrator checks in review):

- **J1** Closed rendering: only catalog primitives; every string from a tool
  result or an argument is shown as text (jig §3); no URL opens from a card.
- **J2** The host checks every IR and every datum before it is stored or sent;
  the app checks again before painting; a card that fails shows nothing and
  never changes the tool result or the run.
- **J3** A submit is never trusted: `vamd` validates amended args against the
  approval's amendable schema (jig decision 14); a card can edit only the
  fields it offers; channels never AMEND.
- **J4** The model does nothing extra: the card is named by the tool, not
  chosen by the model.

**CN-3.1 `List` and the item binding in `vam/1`.**
- **Scope.** Catalog `List`: props `items` (new prop type `list`: a `$data`
  pointer to an array), `empty` (text when empty); exactly one child, the item
  template. Inside a `List` subtree a new binding `{ "$item": "/<ptr>" }`
  resolves against the array's `items` schema; `$item` outside a `List` and a
  `List` inside a `List` are errors (one level in this slice). Display only, no
  selection (the picker is E11). Paint cap: at most 100 items, the rest as "and
  N more". `ir.schema.json` and `ir.d.ts` gain the binding. Pure helpers for
  later waves in `jig.rs`: `check_card(ir, schema)` (the IR checked with the
  given schema as `inputSchema`), `check_ask_card(ir, tool_input_schema)`
  (every submit `with` name is a top-level input property of a fitting type,
  `decision` reserved and set to `approve` or `deny` by the buttons),
  `bind_args(ir, args)` (a state variable named like an argument starts with
  its value).
- **Files.** `spec/jig/{catalog.json,ir.schema.json,ir.d.ts}`,
  `crates/vam-spec/src/jig.rs`, `crates/vam-spec/src/jig/tests.rs`.
- **Acceptance.** A `List` over `/messages` with `$item` text checks; each
  error case has a failing-input test with its JSON pointer; built-ins still
  check; the digest test is updated; `List` is absent from the Slack set, so
  such a card is not native (asserted in wave 5).
- **Tests.** `scripts/ci/local.sh --fast` (`cargo test -p vam-spec`).
- **Turns green.** None (unit; flat, expected).
- **Decide.** `Decide: NOW — proof 2 (P14: a mail list cannot be a card without it), cost 1 (one wave, vam-spec and spec data, no API)`

**CN-3.2 `ToolSpec.ui` and `ApprovalRequestClaims.card` (held for Phase 2).**
- **Scope.** `message ToolUi { string show = 1; string ask = 2; }` and `ToolUi
  ui` on `ToolSpec` at the next free field number after Phase 2's merge (JSON
  `ui: { show, ask }`). `message ApprovalCard { string jig = 1;
  google.protobuf.Struct ir = 2; google.protobuf.Struct data = 3; }` and
  `ApprovalCard card` on `ApprovalRequestClaims` (next free, 21 today).
  Validation in `impl Validate for ToolSpec`: names match
  `[a-z][a-z0-9_]{0,63}`; `show` needs a non-empty `output_schema_json`;
  `ask` is allowed on any tool (agent rules can ask any tool). OpenAPI for both.
  Nothing reads the fields yet.
- **Files.** `proto/vam/v1/skills.proto`, `proto/vam/v1/hitl.proto`,
  `crates/vam-spec/src/validation.rs`, `crates/vam-spec/src/encoding.rs`
  (JSON name test), `crates/vam-spec/tests/examples_json.rs` (round trip),
  `docs/api/openapi.yaml`.
- **Acceptance.** A manifest with `ui` round-trips JSON ↔ proto; each rule has
  a failing test; existing manifests unchanged; field numbers do not collide
  with Phase 2's.
- **Tests.** `--fast`; `--scenarios` A–D (they move from when.0 to their card
  step); `--full` before ready.
- **Turns green.** None (A–D move to a later step; recorded as movement).
- **Decide.** `Decide: SLICE — proof 2 (P14), cost 2 (two proto fields) → slice: two names and one card field; no jig catalog kind, no component refs`

**CN-3.3 Jig files in the skill package, checked at publish.**
- **Scope.** At admission, for each tool with `ui`, read `jigs/<name>.json`
  from the package (manifest-relative, fixed folder, 64 KiB cap), refuse with
  400 naming tool, file and JSON pointer when: the file is missing or not an
  IR; `check_ir` fails; for `show`, `check_card` against the tool's
  `outputSchemaJson` fails; for `ask`, `check_card` against the tool's
  `inputSchemaJson` or `check_ask_card` fails. The checked IRs (with
  `inputSchema` set from the tool's schema and `jig` set to
  `<skill name>.<jig name>`) are kept with the package record so a run never
  unpacks the tarball for a card (the wave picks a column on the package row
  or a stored sidecar).
- **Files.** `crates/vam-orchestrator/src/skill_package_admit.rs`,
  `skill_packages.rs`, `catalog_resolve.rs` or the session catalog build
  (expose the IRs per tool), `crates/vam-spec/src/jig.rs` (helpers from
  CN-3.1, no new rules).
- **Acceptance.** Unit tests per refusal; a package without `ui` publishes as
  before (digest unchanged); the stored IR is the checked one.
- **Tests.** `--scenarios a-skill-whose-card-does-not-fit-its-tool-is-refused-at-publish`; `--full`.
- **Turns green.** D.
- **Decide.** `Decide: SLICE — proof 2 (P14: a bad card is refused at publish with a path, not a blank card at run time), cost 1 (admission already unpacks the package; the checks exist) → slice: JSON IR files only; the `.jig` text form and its compiler stay with E11 Phase 4`

**CN-3.4 The host attaches the show card on a tool result.**
- **Scope.** One function in `outbox.rs`: for a successful call of a tool with
  `ui.show`, parse the result as JSON, cap 16 KiB, `check_data` against the
  tool's output schema, count against the run's outbox rate, then emit
  `outbox.card { jig, ir, data, tool, callId, skill, source: "tool" }`. Any
  failure emits `card.skipped { tool, callId, reason }` (no data) and leaves the
  tool result alone. Called from the host-run tool path (`host_tools.rs`, after
  the adapter returns) and, for guest-run tools (Phase 4's code tools), from
  the thread recorder when it first records the tool message for that call
  (unit test; no scenario runs a guest code tool). Not gated on `human.jig`
  (open question 3).
- **Files.** `crates/vam-orchestrator/src/outbox.rs`, `host_tools.rs` (call
  site), `thread_recorder.rs` (call site), `events.rs` (`card.skipped`),
  `session.rs` (`ToolCatalogEntry` carries the tool's card refs).
- **Acceptance.** Unit tests: a fitting result emits one card with the stored
  IR; an oversize or misfit result emits `card.skipped` only; a tool without
  `ui` emits nothing; a second identical card does not dedupe a different call.
- **Tests.** `--scenarios mail-list-result-shows-as-the-skills-card,progress-shows-while-the-task-runs`; `--full`.
- **Turns green.** A.
- **Decide.** `Decide: NOW — proof 2 (P14: the list shows as a card with no model work), cost 1 (existing outbox event and checks; one new call site per path)`

**CN-3.5 Ask with the skill's card; an edited submit runs as AMENDED.**
- **Scope.** When a call of a tool with `ui.ask` suspends (its
  `approvalRequired` or an agent rule `ask`), the approval stores and serves
  `card { jig, ir: bind_args(ir, args), data: args }`, and its amendable schema
  is the card's submit `with` fields with their types from the tool's input
  schema (not `default_amendable_schema`). Decide: APPROVED runs the requested
  args; AMENDED merges the amended fields over them, re-digests and runs that;
  a field outside the card is 400 and the approval stays pending; REJECTED as
  today. Always allow / Always deny keep today's rules (shown outside the card
  by the app when offered). Channels: the approval renders as today
  (`vam.approve` buttons and Open in VAM; NT-2.10), no edit from Slack; unit
  test that a `List` IR is not native.
- **Files.** `crates/vam-orchestrator/src/security.rs` (suspend sites),
  `hitl.rs` (card amendable schema, `card_json` column via the schema init),
  `api/mod.rs` (`stored_to_approval_request`, decide), `notify_delivery.rs`
  (test only, unless the card leaks into the rendering).
- **Acceptance.** Unit tests: card served with bound state; amended field
  types from the input schema; refusal leaves the row pending; the resumed call
  runs the merged args with a fresh digest; no card on a tool without `ui.ask`.
- **Tests.** `--scenarios send-asks-with-the-skills-card-and-an-edit-runs-as-amended,booking-asks-with-the-skills-card-and-approve-books-it-unchanged,approval-suspends-and-resumes,simple-jigs-render-in-slack-and-the-rest-open-in-vam`; `--full`.
- **Turns green.** B, C.
- **Decide.** `Decide: NOW — proof 3 (the 0.1.0 walkthrough: send and book ask with a card), cost 1 (AMENDED, amendable schema and resume exist; new card column)`

**CN-3.6 the VAM app: `List`, tool cards in the thread, the approval card.**
- **Scope.** (a) Vendor the new `spec/jig` and its digest; `List` and `$item`
  in `check.ts`, `paint.ts`, `JigRenderer.tsx` (same rules and cap as vam).
  (b) `outbox.ts`: a `skill` card item from `outbox.card` with `source: "tool"`
  (paints the inline IR after its own `checkIr` + data check; keyed by
  `callId`), shown in the thread under the tool call, rebuilt on reload by
  `reduceOutboxEvents`; `card.skipped` shows nothing. (c) `ActionCard.tsx`:
  an approval with `card` paints it with `JigRenderer` (state from the bound
  IR); its submit maps to APPROVED (no field changed), AMENDED with
  `amendedArgs` (changed fields only) or REJECTED; Always buttons stay outside
  the card when offered. Types in `src/types/vam.ts`.
- **Files.** (a) `src/jig/{spec/**,specDigest.ts,check.ts,paint.ts,JigRenderer.tsx,types.ts}` (+ tests);
  (b, c) `src/components/vam/{outbox.ts,ActionCard.tsx,ChatArea.tsx}`,
  `hooks/{chatInbox.ts,useVamChat.ts}`, `src/types/vam.ts` (+ tests,
  fixtures from scenarios A–C's payloads).
- **Acceptance.** Unit tests: a `List` of three rows paints three; 150 items
  paint 100 and "and 50 more"; markup in a subject paints as text; a card
  failing its schema paints nothing; an unchanged submit sends APPROVED, an
  edited one AMENDED with only the edited keys.
- **Tests.** `CI=1 pnpm test`, `pnpm exec tsc --noEmit`,
  `cargo test --manifest-path src-tauri/Cargo.toml`.
- **Turns green.** None (unit; team checklist).
- **Decide.** `Decide: NOW — proof 2 (P14: the card is what the person sees), cost 1 per wave (two app waves on the existing renderer and amend path)`

**Not in the slice.** `Link`: `Decide: CUT — proof 1 (no line needs it; mail and calendar cards prove P14 without it), cost 2 (a URL from tool data opens from a card: a new trust rule, jig §3 has no @OpenUrl) → Phase 4 decides if "Open in Outlook" is needed`. The `.jig` text form: `Decide: BACKLOG — proof 1, cost 2 (a parser) → E11 Phase 4`.

##### Waves

One wave at a time; branches `cn-3/wave-N` into `feat/connectors` (vam) or
`feature/connectors` (the VAM app). Waves 0–2 touch no file Phase 2 is expected to
touch and start after approval; wave 3 is the held one and is kept small.

| Wave | Items | Turns green | Repo | Model | Budget | Depends on | Actions | Status | PR |
|---|---|---|---|---|---|---|---|---|---|
| 0 | scenarios: 4 files, 3 world pieces, `files:` on skill pieces, steps `publish-skill` and `approve.amend`, README "Cards", guards-only edits | — (4 red, right reason, class `product`, at when.0) | vam | Sonnet low | 90 min | — (check Phase 2's lane is not open in the runner files first) | fast (`vam-e2e`, `e2e/`: `skip-vm-lane`) | [x] | — |
| 1 | CN-3.1 | — (unit; flat, expected) | vam | Sonnet medium: changes the jig IR schema (a new binding form) that both repos check | 90 min | 0 | none (`vam-spec`, `spec/` only: `[skip ci]`) | [x] | — |
| 2 | CN-3.6 (a) | — (unit) | The VAM app | Sonnet low | 60 min | 1 | none (no CI) | [x] | — |
| 3 | CN-3.2 | — (A–D move to their card step) | vam | Sonnet medium: two proto fields and manifest validation | 45 min | 1; **held until Phase 2's wave touching `skills.proto` / `validation.rs` has merged into `feat/connectors`**, then merge the base into the branch before starting | fast (`proto/`: `skip-vm-lane`) | [x] | — |
| 4 | CN-3.3, CN-3.4 | D, A | vam | Sonnet medium: crosses admission, the package store, the host tool path, the recorder and events | 120 min | 3 | fast (`skip-vm-lane`) | [x] | — |
| 5 | CN-3.5 | B, C | vam | Sonnet medium: changes which arguments an approved external write runs with; orchestrator reads the amendable schema, the decide handler and the resume path before merge | 120 min | 4 | fast (`skip-vm-lane`) | [x] | — |
| 6 | CN-3.6 (b, c) | — (unit) | The VAM app | Sonnet medium: new event and claims shapes and the decide path | 120 min | 2, 5 | none (no CI) | [x] | — |

Scoreboard expectation: 0 green until wave 4 (A, D), 4 after wave 5; flat in
waves 1, 2, 6 (unit), movement without green in wave 3 (stated so it is not
flagged).

**Lane overlap (Phase 2 runs now; `phase-playbook.md` §11.2 conflict rule).**
Before each wave the orchestrator checks Phase 2's open PRs and wave rows for
these paths; a Phase 3 wave never starts while a Phase 2 wave is building in
one of its files.

| File | Phase 2 (expected) | Phase 3 wave | Rule |
|---|---|---|---|
| `proto/vam/v1/skills.proto` (`ToolSpec`, `SkillManifest`) | egress envelope on `ToolSpec`, `requiresConnector` on `SkillManifest` | 3 only | **held**; take field numbers after Phase 2's |
| `crates/vam-spec/src/validation.rs` (`impl Validate for ToolSpec` / `SkillManifest`) | envelope and `requiresConnector` checks | 3 only | **held** |
| `crates/vam-spec/src/encoding.rs`, `crates/vam-spec/tests/examples_json.rs` | likely (JSON name and round-trip tests) | 3 only | **held** |
| `docs/api/openapi.yaml` (`ToolSpec`) | likely | 3 only | **held** |
| `proto/vam/v1/hitl.proto` | possible ("connect to continue" park) | 3 only | held with 3 |
| Generated proto code | not checked in (`OUT_DIR`) | — | none |
| Manifest JSON schema | none exists | — | none |
| `crates/vam-orchestrator/src/skill_package_admit.rs` | possible (`requiresConnector` at publish) | 4 | check before 4 |
| `host_tools.rs`, `security.rs`, `hitl.rs`, `api/mod.rs` | possible (egress per call, reauth / connect park) | 4, 5 | check before each |
| e2e runner `crates/vam-e2e/src/{scenario.rs,engine.rs,world.rs,forms.rs}`, `e2e/README.md` | likely in Phase 2's wave 0 | 0 | check before 0; hold if open |
| `e2e/world/` | its own new pieces | 0: three new files | no shared piece; neither lane edits the other's |
| The VAM app `ActionCard.tsx`, `src/types/vam.ts` | possible (a "connect to continue" card) | 6 | check before 6 |
| This tracker | its own section | — | each lane edits only its phase section, `current_phase` in one line, and appends log rows |

**Files per wave.** 0: `e2e/scenarios/*.yaml` (4 new; 3 guards-only edits),
`e2e/world/*.yaml` (3), `crates/vam-e2e/src/{world.rs,forms.rs,scenario.rs,engine.rs}`,
`e2e/README.md`. 1: `spec/jig/{catalog.json,ir.schema.json,ir.d.ts}`,
`crates/vam-spec/src/{jig.rs,jig/tests.rs}`. 2: the VAM app `src/jig/**`.
3: `proto/vam/v1/{skills.proto,hitl.proto}`,
`crates/vam-spec/src/{validation.rs,encoding.rs}`, `crates/vam-spec/tests/examples_json.rs`,
`docs/api/openapi.yaml`. 4: `crates/vam-orchestrator/src/{skill_package_admit.rs,skill_packages.rs,catalog_resolve.rs,session.rs,outbox.rs,host_tools.rs,thread_recorder.rs,events.rs}`.
5: `crates/vam-orchestrator/src/{security.rs,hitl.rs,api/mod.rs,notify_delivery.rs}`.
6: the VAM app `src/components/vam/{outbox.ts,ActionCard.tsx,ChatArea.tsx,hooks/chatInbox.ts,hooks/useVamChat.ts}`, `src/types/vam.ts`.

**Phase rubric.** `Decide: SLICE — proof 3 (0.1.0 row 20, which row 21's walkthrough step 4 needs: sending and booking ask with a card; plus the new P14 task line), cost 2 (two proto fields, one IR primitive, both repos) → slice: mail and calendar cards only (`ToolSpec.ui`, `List`, JSON jigs checked at publish, AMENDED from the card); `.jig` text, `Link`, `Query`, App pages, component libraries and a jig catalog kind later`

##### Open questions (each with a default)

1. **Form of a skill's jig.** Default: JSON IR files `jigs/<name>.json` in the
   package, named by `ui.show` / `ui.ask`, checked at publish (CN-3.3); the
   `.jig` text form of `design/connectors.md` §8 waits for E11 Phase 4's parser.
2. **Card name on events and approvals.** Default: `<skill name>.<jig name>`
   (e.g. `m365-mail.mail_list`) with the skill URN beside it; the `vam.`
   prefix stays reserved for built-ins.
3. **Tool cards and the `human.jig` switch.** Default: tool cards attach
   whatever the switch says (the host attaches them; the switch governs what
   the model is offered).
4. **Guest-run tools.** Default: in the slice, through the thread recorder's
   first sight of the tool message (CN-3.4), so Phase 4's code tools get cards;
   proven by a unit test, since no scenario runs a guest code tool.
5. **A result that does not fit or is over 16 KiB.** Default: no card, a
   `card.skipped {tool, callId, reason}` event, the tool result unchanged.
6. **Prefill of an ask card.** Default: a state variable named like a tool
   argument starts with that argument's value; others keep the IR's defaults.
7. **What an ask card may edit.** Default: only its submit `with` fields, each
   a top-level input property; `decision` reserved (`approve` | `deny`);
   unchanged → APPROVED, changed → AMENDED with only the changed keys, deny →
   REJECTED.
8. **Slack and other channels.** Default: an approval with a skill card
   renders as today (`vam.approve` buttons and Open in VAM); edits only in the
   app; a `List` card falls back to Open in VAM wherever a jig would go native.
9. **`Link`.** Default: cut from the slice (see Not in the slice).
10. **Selection in `List`.** Default: display only; the picker is E11.
11. **Reload.** Default: cards come back from the run's `outbox.card` events
    (the app already reduces run events); the thread log keeps the tool result
    as today.
12. **Proto field numbers.** Default: wave 3 takes the next free numbers on
    `ToolSpec` and `ApprovalRequestClaims` after Phase 2's proto wave merges,
    and says them in its PR.

**Phase close:** unit suites on the merged base; release with Phase 2 if the
owner wants one release for both lanes; team checklist out.

**Phase 3 close (2026-11-08, orchestrator).** All four Phase 3 scenarios are
`active` and green (D, A from wave 4; B, C from wave 5b); smoke on
`feat/connectors` at 5b's head: 113/113 pass, 0 red. The VAM app unit suites
green on `feature/connectors` (839/839, tsc clean). Waves 0–6 merged (wave 5
split into 5a/5b around the Phase 2 lane's `security.rs` wave); 2 fix cycles
(W3 runner wait, W4 session-map pruning); GitHub Actions used: none. Benchmark P14:
scored baseline still needs the owner's word for paid models (not run).

**Team checklist — Phase 3** (written 2026-11-08 from scenarios A–D)

Setup: deploy `vamd` from `feat/connectors`, then the app from
`feature/connectors` (vamd first: the app reads `outbox.card` with
`source: "tool"` and `ApprovalRequestClaims.card`). One member account. A
skill directory for the test skill: `manifest.json` from the tools in
`e2e/world/skill-mail-calendar-cards.yaml` (`body.spec`, with your own URN)
plus `jigs/` copied from `e2e/world/fixtures/mail-calendar-cards/jigs/`
(the four good files) and a `SKILLS.md`; and an agent that may use it. The
tools use the built-in `echo` host adapter, so results are the arguments the
agent passes.

1. `vam catalog skill publish <dir>`. **See:** published.
2. Copy `jigs/unknown_primitive.json` over `jigs/mail_list.json` and publish
   again. **See:** refused, naming `mail_list`, the file and `Carousel`.
   Restore the file.
3. In the app, ask the agent: "Call mail_list with messages: one from Ana
   'Lunch', one from Raj 'Budget', one from Lee 'Trip'." **See:** a card in
   the thread with three rows (from, subject), not only text.
4. Reload the app. **See:** the same card is still there.
5. Ask: "Send a mail to ana@example.com, subject 'Hi', body 'Hello'."
   **See:** an approval card from the skill, the subject field filled with
   "Hi", and a "What will run" section listing `to`, `subject`, `body`.
6. Change the subject to "Hello Ana" and approve. **See:** "What will run"
   marks subject "(edited)"; the run finishes and the tool result shows
   "Hello Ana", not "Hi".
7. `vam receipt <run>` (or the run's receipt in the app). **See:** the send
   line resolved `approved-amended`.
8. Ask the agent to find slots (give two) and book the first. **See:** a
   slot card, then an approval card; approve unchanged. **See:** the booking
   runs with the first slot.
9. On a Slack-linked account, trigger step 5 again. **See:** Slack shows the
   usual Approve/Deny and Open in VAM (no card, no edit in Slack).

Report: "Phase 3 pass", or the step number with a screenshot or the error
text.

Open after the close: a ready-made test skill directory (manifest, jigs,
SKILLS.md) would make the setup a single command; Phase 4's `m365-mail`
skill replaces it with real mail and calendar.

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

#### Phase 4 breakdown (phase-breakdown agent, 2026-11-09; approved by the owner 2026-11-09)

Written against `claude/sweet-allen-fnc5re`, level with `main` and
`feat/connectors` ; the VAM app read;
`squrell-inc/vam-marketplace` is empty (no commit). No product code, build or
test was run. Where the code and the design differ, the code is the fact.

**Code map (today).**

- Admission of Squrell content is impossible today. `VAMD_MARKETPLACE_DOMAIN`
  (`daemon.rs`) is empty by default; `/v1/me` then omits `marketplaceDomain`
  and the app's Squrell badge never shows (the VAM app `components/vam/origin.ts`).
  `catalog_tier.rs`: tiers `user | platform` under a SQLite `CHECK`
  (`CATALOG_TIER_CHECK_SQL`; `platform` = `examples.local` seeding only);
  `reject_caller_platform_urn_with` refuses `examples.local` and the
  marketplace domain to callers. `vam-spec/src/origin.rs`
  `RegistryDomains::admits_skill_registry` admits only local registries and the
  org's registry domain (used by `skill_package_admit.rs:280` and
  `skill_package_resolve.rs:81`): a skill under `hub.squrell.cloud` can be
  neither admitted nor resolved. Agents and playbooks under any other domain
  can be published by callers (no reservation).
- Built-in connectors: `vam-spec/src/connector.rs` `builtin_connectors()` over
  two `include_str!` constants (a fixed list). `connection_egress.rs` reads
  bindings from it.
- Catalog and publish: `catalog_publish.rs` (draft, release, released digest,
  owner per row), `skill_package_admit.rs` (unpack, manifest, `SKILLS.md`
  frontmatter, owner-namespaced blob), `skill_cards` checks jig files at
  publish. No import path; no origin columns beyond the URN domain.
- CLI: `vam catalog {agent,playbook,policy,skill}` talk to vamd; no `import`,
  no offline `validate`. `vam-cli` depends on vam-spec and vam-security, not
  on the orchestrator, so offline checks must live in vam-spec.
- Git: vamd has no Git code; it has `reqwest` (blocking, rustls), `tar`,
  `flate2`. Only `vam-cli/build.rs` runs `git`.
- Egress envelope (`vam-network/src/connection.rs` `prefix_admits`): a path
  prefix on a segment boundary; `/` admits the whole host. `ToolEgress`
  (`skills.proto`) fields 1–4, next free **5**. Consequence for writes: a
  `POST /v1.0/me/events` prefix also admits `POST /v1.0/me/events/{id}/forward`
  (sends mail), and a `POST /v1.0/me/messages` (draft) prefix admits
  `POST /v1.0/me/messages/{id}/send`.
- Effect journal: `effect_path.rs` journals host-mediated calls only
  (`is_journaled` = `command == "host"`); in-VM code tools are out of scope
  on purpose. So a `m365-mail` send interrupted after Graph got it has no
  journal row. `ToolIntentResponse` already has `DISPOSITION_REPLAYED`.
- Guest: the rootfs is `busybox:musl` plus Bun (`scripts/build-rootfs.sh`);
  **no python3**. The Phase 2 probe tool is python3 and runs only in process
  mode. CI has no Bun.
- Schedules: `brokers/vam-schedule` takes `playbookRef` and `inputs`;
  `grants.rs` binds the playbook to the grant; trigger-started sessions run as
  the grant owner (Phase 1 W3). Scenarios: `scheduled-message-runs-at-its-time`
  (`wait-scheduled-run`), `task-killed-mid-effect-finishes-once` (`kill-guest`).
- e2e runner: piece kinds `org, agent, playbook, secret, mcp-connection,
  skill, broker, workspace-seed, mock-provider`; `files:` on skill pieces; no
  Git piece or marketplace step. Mock Graph serves `GET /v1.0/me/messages`
  and `POST /v1.0/me/sendMail`, 404 for the rest.
- The VAM app: Org settings places (`OrgSettingsPage.tsx`,
  `OrgSettingsConnectorsWorkspace.tsx` is the Phase 1 pattern), Tauri commands
  in `src-tauri/src/vamd.rs` + `lib.rs`.
- Repos: `squrell-inc/VAM` is **private**; `squrell-inc/vam-marketplace` is
  public and empty.

**Shared invariants** (every item holds them; the orchestrator checks in review):

- **K1–K7 from Phases 1 and 2 hold** (no token in the guest, envelope per
  call, parks send nothing).
- **M1** Nothing from a marketplace reaches the catalog before an org admin
  installs it; nothing installs by itself (refresh only reads).
- **M2** Installed rows are read-only for everyone: no draft, put, publish,
  release or delete by a caller on an installed URN. Only the import path
  writes them.
- **M3** Content under `hub.squrell.cloud` is admitted only from the
  preconfigured Squrell marketplace URL. An added marketplace cannot write
  under the Squrell domain, the org's registry domain, `local.vam.dev`,
  `*.local`, `examples.local`, or another trusted marketplace's domain.
- **M4** Non-Squrell listings install skills, playbooks and agents only; a
  connector in one is refused (D25).
- **M5** What installs is exactly the tag's commit: the commit SHA and each
  package digest are recorded; a moved tag is refused, never followed.
- **M6** Fetching a repo runs no code from it (no hooks, no submodules, no
  LFS, no filters) and reaches only `https://` hosts on public addresses.

##### Scenario delta

All process mode, mock model. Marketplace scenarios use a runner-made **bare
Git repo** served as `file://` (new piece kind `git-marketplace`: the runner
copies a fixture directory, `git init`, commits, tags; step `git-tag` adds or
moves a tag). vamd reaches it through a process-only seam key (as Phase 1 did
for the mock login), so the default vamd still accepts only `https://`.
Microsoft scenarios reuse `mock-microsoft-login` and `mock-microsoft-graph`
(gains: `$search`, message by id, `calendarView`, `findMeetingTimes`, draft
create, `sendMail` with an optional hold, `POST /me/events`, all logged with
`bearer=` and method).

| Scenario | Change | Layer | Given | Why |
|---|---|---|---|---|
| `admin-installs-the-squrell-listing-at-its-tag` | new | process | `org-admin`, `org-member`, **new** `marketplace-squrell` (built from `e2e/world/fixtures/marketplace-squrell/`) | D23, §9.1. The Squrell marketplace is listed preconfigured with badge `squrell`; its listing `microsoft-365` shows what it installs and can reach (tools, side effects, hosts, connector). The admin installs it: three released rows under `hub.squrell.cloud/squrell/…` with origin (marketplace, ref, commit); a member's install is 403; a caller's publish or put on an installed URN is refused; the member can chat with the installed agent. Red at the marketplace list step (no route), class `product` |
| `listing-from-an-added-marketplace-installs-as-unverified-within-its-limits` | new | process | `org-admin`, `org-member`, `marketplace-squrell`, **new** `marketplace-community` (fixture with four listings) | D25, M3, M4; P9. The admin adds a second URL (a member cannot). Its good listing shows `Unverified` and a capability summary that names a `/` prefix as "any path on <host>"; installs under the marketplace's own domain. Refused with a reason: a listing with a connector; one whose artifact URN is under `hub.squrell.cloud`; one under the org's registry domain. A broken index in one marketplace shows an error for that one only. Red as above |
| `newer-tag-shows-update-available-until-the-admin-updates` | new | process | as the first | Update path and M5. After an install at `microsoft-365-v1.0.0`, `git-tag` adds `v1.1.0` and the index names it: refresh shows `updateAvailable`, the catalog is unchanged; the admin updates and the new release is the one resolved. A re-pointed `v1.0.0` is refused at refresh and at install. Red as above |
| `executive-assistant-triages-sends-and-books-with-approval` | new | process (mock providers) | as the first, plus `mock-microsoft-login`, `mock-microsoft-graph` | The §3.3 walkthrough on mocks. Installed listing, connector enabled, member connected. The agent searches (the skill's mail card), reads one message, finds slots (slot card); the draft tool, told to send its own draft, is refused ("declared shape") and Graph saw no `…/send`; `mail_send` asks with the skill's card, approved, Graph saw one `POST /v1.0/me/sendMail` `bearer=current`; `calendar_create_event` asks, approved, Graph saw one `POST /v1.0/me/events`. Receipt: two `approved-once` lines; `secret-absent`. Red at when.0 (install), then at the first tool step |
| `morning-triage-runs-on-schedule-as-the-member-who-approved-it` | new | process (mock providers) | as above, plus `schedule-broker` | P7, the scheduled half. The member asks the assistant to run the morning triage every weekday; `schedule_create` asks; approved (one grant). `wait-scheduled-run`: a run starts by itself with the `morning-triage` playbook, reads Graph as the member (`bearer=current`), shows the mail card, sends nothing; the receipt names surface `schedule` and the grant. Red at when.0 |
| `scheduled-triage-parks-on-a-revoked-connection-and-resumes-once` | new | process (mock providers) | as above | P6/P13 revoke line, unattended. Connected with `expiresIn: 0`; the login mock revokes; the scheduled run parks (`waiting_for: REAUTH`), one `connection.needs_reauth` notification, Graph saw nothing; the member reconnects; the run resumes and succeeds; Graph saw exactly one search. Red at when.0 |
| `send-interrupted-mid-call-is-not-sent-twice` | new | process (mock providers) | as the fourth | P13 zero duplicate effects. Graph holds `sendMail`; `kill-guest` during the hold; the run resumes on a new guest; Graph saw one `POST /v1.0/me/sendMail`; the resumed call is not sent again (replayed, or asked when its answer was lost); the receipt says so. Red at when.0 |

**Existing scenarios whose `then` changes: none.** Checked from `guards` and
`given`: no scenario asserts `marketplaceDomain` (default option of question
1 makes it present), publishes under `hub.squrell.cloud`, or uses a write
egress shape. **Guards-only additions:** the vam-spec package-check module
(CN-4.2) on `a-skill-whose-card-does-not-fit-its-tool-is-refused-at-publish`;
`vam:crates/vam-orchestrator/src/effect_path.rs` on
`task-killed-mid-effect-finishes-once` and
`effect-is-not-repeated-after-the-guest-crashes`; `vam:crates/vam-spec/src/connector.rs`
on `connectors-reach-members-only-after-an-org-admin-enables-them`.

##### Benchmark delta

| Line (`design/benchmark.md` §1) | Baseline on this base | Measured at phase close by | Expected after |
|---|---|---|---|
| **new P6 line:** minutes and clicks from the admin's Install/Enable to a member's first successful mail read, real tenant, member given no instructions | Not possible (no listing, no mail skill). Phase 1's Enable → Connected is still out with the team | CN-4.10, by hand on the owner's test tenant, timed | first number recorded |
| **new P6/P13 line:** a scheduled Microsoft 365 run survives seven days unattended with zero reconnect prompts; on a forced revoke it parks, the user reconnects, it resumes with zero duplicate effects | Revoke-park-resume proven interactively on mocks (Phase 2 scenario D); no scheduled Microsoft run exists | mock half: the last three scenarios on every PR; real half: benchmark lane B (seven days, **owner's word**) | 0 reconnect prompts in 7 days; 1 send per approved send |
| P6 tasks through brokered connections, 0 secret bytes in guest output | Probe skill only | `secret-absent` in the Microsoft scenarios | 0 bytes |
| P9 malicious-skill tasks that fail safely: **new tasks** "a listing from an added marketplace ships a connector or takes a Squrell or org name" and "a create tool forwards or sends" | Not possible / admitted by prefix today | the second and fourth scenarios | 100% refused |

All lines in CI are mock-measured. **Baseline: to run on the owner's word**
for the real-tenant and paid-model numbers (none is needed for the build
waves). The orchestrator adds the two new lines to `design/benchmark.md` §1 on
approval (docs only).

##### Items

**CN-4.1 Built-in connectors from every file in `spec/connectors/`.**
- **Scope.** `vam-spec/build.rs` lists `spec/connectors/*.json` and generates
  the embedded table (`include_str!` per file, sorted); `builtin_connectors()`
  reads it. Duplicate id or URN fails the build test; a file that does not
  validate fails `builtins_validate`. No behaviour change.
- **Files.** `crates/vam-spec/{build.rs,src/connector.rs}`.
- **Acceptance.** A unit test adds a temp file in a copy and sees it listed
  (or the build script's own test); `GET /v1/connectors` unchanged.
- **Tests.** `scripts/ci/local.sh --fast`.
- **Turns green.** None (prepares Phase 5).
- **Decide.** `Decide: BACKLOG — proof 1 (prepares D26; no line), cost 0 → carried by wave 1, which already touches vam-spec`

**CN-4.2 Package checks and the marketplace index in vam-spec; `vam catalog validate`.**
- **Scope.** One offline check per artifact kind in a new vam-spec module
  (`package.rs`): detect the kind of a directory (skill, agent, playbook;
  `connector` reserved for Phase 5 and refused), check it as vamd's publish
  does (manifest, `SKILLS.md` frontmatter, jig files against the tool schemas,
  egress shapes, `requiresConnector` shape), and compute its **capability
  summary** (tools with side effect and approval, egress host / methods /
  paths with `/` named "any path", connector needed, agent rules, playbook
  tools). vamd's skill admission calls the same check (logic moved, not
  copied). New `marketplace.rs`: `marketplace.json` schema (`name`,
  `publisherDomain`, `listings[{id, displayName, description, connector?,
  artifacts[], ref, requiresNode}]`) and its validation. CLI `vam catalog
  validate <dir>` (one artifact, or a marketplace root: every listing) runs
  offline, no vamd; exit code and one line per finding.
- **Files.** `crates/vam-spec/src/{package.rs,marketplace.rs,lib.rs}` (new),
  `crates/vam-orchestrator/src/skill_package_admit.rs` (call the shared
  check), `crates/vam-cli/src/cmd/catalog.rs`, `crates/vam-cli/src/main.rs`.
- **Acceptance.** Unit tests per rule and per kind; the Phase 3 bad-card
  fixtures fail offline with the same message as at publish; a good
  marketplace fixture validates.
- **Tests.** `--fast`; `--scenarios a-skill-whose-card-does-not-fit-its-tool-is-refused-at-publish,mail-list-result-shows-as-the-skills-card`.
- **Turns green.** None (scaffold the later waves share).
- **Decide.** `Decide: NOW — proof 3 (launch row 21: import from Git; marketplace CI cannot validate without it), cost 1 (one wave, logic moved, one CLI command)`

**CN-4.3 Exact paths in the egress envelope.**
- **Scope.** `ToolEgress.paths = 5` (repeated, exact paths after the same
  normalization as prefixes). A shape needs at least one of `pathPrefixes` or
  `paths`. The envelope admits a request when its path equals a declared
  exact path or matches a prefix. Publish warning (and capability-summary
  flag) when a method other than GET or HEAD is declared by prefix. OpenAPI and
  the proto baseline additive.
- **Files.** `proto/vam/v1/skills.proto`, `crates/vam-spec/src/skill_egress.rs`,
  `crates/vam-network/src/connection.rs`,
  `crates/vam-orchestrator/src/connection_egress.rs` (shape copy), `docs/api/openapi.yaml`.
- **Acceptance.** Unit tests: exact `/v1.0/me/events` refuses
  `/v1.0/me/events/x/forward`, `/v1.0/me/events/`, `%2F` forms; prefixes
  unchanged; existing manifests validate unchanged.
- **Tests.** `--fast`; `--scenarios tool-declared-as-a-read-cannot-send-mail,skill-reads-microsoft-graph-as-the-member-without-holding-a-token`.
- **Turns green.** None alone; the refusal step of the fourth scenario.
- **Decide.** `Decide: NOW — proof 3 (P1 override: a declared create can forward or send by prefix), cost 1 (one field, one matcher)`

**CN-4.4 Trusted marketplaces on the node.**
- **Scope.** Table `node_marketplaces` (id, url, display name,
  `publisherDomain`, trust `squrell | unverified`, preconfigured, enabled,
  last checked, last error, index digest, tags seen). The Squrell entry
  `https://github.com/squrell-inc/vam-marketplace` is seeded at boot (can be
  disabled, not removed). `GET /v1/marketplaces` (users:admin),
  `POST` (add a URL; reads the index once; the admin confirms its
  `publisherDomain`, checked against M3), `PUT` enable/disable, `DELETE`
  (added ones), `POST /{id}/refresh`. Refresh per marketplace on its own timer
  (question 7) and on demand; one marketplace's failure never touches another.
  Fetch (question 4): `git ls-remote` for tags, a shallow fetch of the default
  branch for `marketplace.json`, hardened per M6, size and time capped, into a
  per-marketplace cache under the node state dir. Process-only seam key maps
  the Squrell URL to a `file://` bare repo (process-guest builds only; the
  release gate proves it absent). CLI `vam marketplace ls|add|disable|rm|refresh`.
- **Files.** `crates/vam-orchestrator/src/marketplace/{mod.rs,fetch.rs,store.rs}`
  (new), `api/marketplaces.rs` (new), `api/mod.rs` (routes), `daemon.rs`,
  `process_mode.rs`, `crates/vam-cli/src/cmd/marketplace.rs` (new),
  `docs/api/openapi.yaml`, `docs/src/content/docs/orchestrator/daemon.md`.
- **Acceptance.** Unit tests: `http://`, `ssh://`, `file://` (default build),
  private and loopback addresses refused; a repo with hooks, submodules or a
  symlinked `marketplace.json` runs nothing and reads nothing outside;
  oversize and timeout fail that marketplace only; a member gets 403; a domain
  clash is refused.
- **Tests.** `--fast`; `--scenarios admin-installs-the-squrell-listing-at-its-tag,listing-from-an-added-marketplace-installs-as-unverified-within-its-limits`; `--release` for the seam.
- **Turns green.** None alone (the first three move past their list steps).
- **Decide.** `Decide: SLICE — proof 3 (D23; launch row 21), cost 2 (new table, API, a fetcher of admin-supplied URLs) → slice: Git over https only, index and tags only, no mirrors UI; rest backlog (OCI hub §9.3)`

**CN-4.5 Listing import at a tag; Squrell and Unverified admission; update available.**
- **Scope.** `POST /v1/marketplaces/{id}/listings/{listing}/install` and
  `vam catalog import <git url> --path <dir> --ref <tag>` reach the **same**
  import function (the URL must be a trusted marketplace on the node).
  Resolve the tag to a commit (M5), read each artifact at that commit, run the
  CN-4.2 check, apply the admission rules of question 1 (M3, M4), write the
  released rows in one transaction (skills, then playbooks, then agents) with
  owner `marketplace:<id>`, tier `marketplace`, and origin columns
  (marketplace, listing, ref, commit, package digest); record the install.
  `requiresNode` checked against the vamd version. Read-only (M2) enforced in
  the publish, put, release and delete paths. Refresh computes
  `updateAvailable` (the index's `ref` differs from the installed one and its
  tag exists); `POST …/update` imports the new tag the same way; old releases
  stay resolvable for pinned agents. The import code is generic over artifact
  kind (a kind table), so Phase 5 adds `connector` as a row. `GET
  /v1/marketplaces/{id}/listings` returns listings with install state and the
  capability summary. `/v1/me` and catalog rows carry origin and badge.
- **Files.** `marketplace/{import.rs (new),mod.rs}`, `catalog_tier.rs`,
  `catalog_publish.rs`, `catalog.rs`, `skill_package_admit.rs`,
  `skill_package_resolve.rs`, `crates/vam-spec/src/origin.rs`,
  `api/marketplaces.rs`, `api/mod.rs`, `workflow.rs` (migration),
  `crates/vam-cli/src/cmd/catalog.rs`, `docs/api/openapi.yaml`,
  `docs/src/content/docs/authoring/model.md` (domain table).
- **Acceptance.** Unit tests: every M2–M5 refusal; a failed artifact leaves no
  row (one transaction); an agent pinned to 1.0.0 still resolves after the
  update; a caller cannot publish under an added marketplace's domain; the
  migration keeps existing rows `user` / `platform`.
- **Tests.** `--scenarios admin-installs-the-squrell-listing-at-its-tag,listing-from-an-added-marketplace-installs-as-unverified-within-its-limits,newer-tag-shows-update-available-until-the-admin-updates,member-chats-with-a-released-agent`; `--full`, `--release`.
- **Turns green.** The first three.
- **Decide.** `Decide: SLICE — proof 3 (D23–D25; launch row 21), cost 2 (catalog admission, a migration) → slice: install and update at a tag, read-only rows; fork, uninstall and policy-driven auto-update backlog`

**CN-4.6 The Microsoft 365 listing: `m365-mail`, Executive assistant, morning triage.**
- **Scope.** Authored in `e2e/world/fixtures/marketplace-squrell/` (the same
  layout as the marketplace repo; question 3). `listings/microsoft-365/skills/m365-mail`:
  Bun TypeScript tools (question 11), each with one narrow shape:
  `mail_search` GET prefix `/v1.0/me/messages`, `mail_read` GET prefix
  `/v1.0/me/messages`, `calendar_list` GET prefix `/v1.0/me/calendarView`,
  `calendar_find_slots` POST exact `/v1.0/me/findMeetingTimes` (READ_ONLY;
  the publish warning is expected), `mail_draft` POST exact `/v1.0/me/messages`,
  `mail_send` POST exact `/v1.0/me/sendMail` (MUTABLE_EXTERNAL,
  `approvalRequired`, `ui.ask`), `calendar_create_event` POST exact
  `/v1.0/me/events` (MUTABLE_EXTERNAL, `approvalRequired`, `ui.ask`,
  `effect.class` IDEMPOTENT with a Graph `transactionId` derived from the
  arguments); `ui.show` cards from the Phase 3 jigs; `SKILLS.md` prose (when
  to use each tool, never send without the card, Graph limits: one mailbox,
  serial calls). `agents/executive-assistant`: permissions mode, `load_skill`
  and reads allow, `mail_draft` allow (question 10), send and create ask,
  `schedule_list`/`get` allow, `schedule_create`/`update`/`delete` ask.
  `playbooks/morning-triage`: inputs (lookback hours), reads only, ends with
  the mail card and a short summary. `marketplace.json`. Bun pinned in CI and
  `scripts/ci/local.sh` for process-mode tools. Checks that triggered runs use
  the grant owner's connection (fix in `brokers.rs` / `connection_egress.rs`
  only if the scenario shows otherwise).
- **Files.** `e2e/world/fixtures/marketplace-squrell/**`,
  `.github/workflows/rust.yml` and `scripts/ci/local.sh` (Bun),
  possibly `connection_egress.rs` / `brokers.rs`.
- **Acceptance.** `vam catalog validate e2e/world/fixtures/marketplace-squrell`
  passes; unit tests of the TS tools against a recorded Graph response (Bun
  test); no tool declares `/`.
- **Tests.** `--scenarios executive-assistant-triages-sends-and-books-with-approval,morning-triage-runs-on-schedule-as-the-member-who-approved-it,scheduled-triage-parks-on-a-revoked-connection-and-resumes-once`; `--full`.
- **Turns green.** The fourth, fifth and sixth.
- **Decide.** `Decide: NOW — proof 3 (launch row 21: the demo), cost 1 (content on existing machinery, one CI step)`

**CN-4.7 Connection writes are journaled.**
- **Scope.** A code tool whose shapes include a method other than GET or HEAD
  is journaled like a host call (`effect_path.rs`): `intended` at the intent,
  `sent` at the envelope's attach of a write request (before the request
  leaves; a failed write blocks it, R1), settled with the tool's answer at the
  tool result. A resumed identical call is `REPLAYED` from the row, or asks
  when the row is `sent` without an answer (the existing path). The effect key
  stays on the host; no guest change (question 12). Receipt lines name the
  state.
- **Files.** `effect_path.rs`, `connection_egress.rs`, `security.rs` (the
  journaled test), `host_tools.rs` or `thread_recorder.rs` (settle),
  `receipt.rs` if a line changes.
- **Acceptance.** Unit tests: attach without an `intended` row refuses; two
  write requests in one call are both recorded; READ_ONLY tools unchanged; a
  replay sends nothing.
- **Tests.** `--scenarios send-interrupted-mid-call-is-not-sent-twice,task-killed-mid-effect-finishes-once,effect-is-not-repeated-after-the-guest-crashes`; `--full`.
- **Turns green.** The seventh.
- **Decide.** `Decide: NOW — proof 2 (P13 zero duplicate effects; the P6/P13 line), cost 1 (existing journal, one new call site)`

**CN-4.8 the VAM app: Org settings Marketplace page and origin badges.**
- **Scope.** Org settings "Marketplace" (users:admin), like the Connectors
  place: trusted marketplaces with badge (Squrell / Unverified), last checked,
  error, Refresh, Add URL (shows the index name and `publisherDomain` to
  confirm), Disable, Remove (added only); per marketplace its listings with
  description, what each installs and can reach (the capability summary), the
  connector it needs with a link to Connectors when not enabled, Install (a
  confirm dialog with the summary, always for Unverified), Installed version,
  "Update available" and Update. Catalog lists show the origin badge and no
  edit actions on installed rows. Tauri commands registered in `lib.rs`.
- **Files.** `src/components/vam/OrgSettingsMarketplaceWorkspace.tsx` (new),
  `OrgSettingsPage.tsx`, `origin.ts`, `src/types/vamMarketplace.ts` (new) and
  tests, `src-tauri/src/{vamd.rs,lib.rs}`.
- **Acceptance.** Unit tests on fixtures: badge per trust, summary renders
  every host and the "any path" flag, Install disabled for a member, update
  state.
- **Tests.** `CI=1 pnpm test`, `pnpm exec tsc --noEmit`, `cargo test --manifest-path src-tauri/Cargo.toml`.
- **Turns green.** None (unit; team checklist).
- **Decide.** `Decide: NOW — proof 2 (P6: a non-developer admin installs without the CLI), cost 1 (one app place on a known pattern)`

**CN-4.9 Bootstrap `squrell-inc/vam-marketplace`.**
- **Scope.** First commit on `main` (the repo has none, so there is no base to
  open a PR against: **owner's word** for the first push, or the owner makes
  it): `README.md` (what it is, how a node trusts it, how to add a listing,
  release by tag), `LICENSE` (question 2), `marketplace.json`,
  `listings/microsoft-365/` copied from the vam fixture at the wave's vam
  commit, `CODEOWNERS`, `.github/workflows/validate.yml`: on every PR and tag,
  build `vam-cli` at a pinned vam commit (question 8), run `vam catalog
  validate .`, check every listing's `ref` tag exists and points at a commit
  that validates. Tag `microsoft-365-v1.0.0`. In vam: a `SOURCE` stamp in
  the fixture (marketplace tag and tree hash) and a `--fast` check that the
  fixture still matches its stamp (question 3).
- **Files.** vam-marketplace: all of the above. vam:
  `e2e/world/fixtures/marketplace-squrell/SOURCE`,
  `scripts/marketplace/check-snapshot.sh`, `scripts/ci/local.sh` (one line).
- **Acceptance.** Marketplace CI green on the first PR after `main`; the tag
  validates; vam `--fast` passes the stamp check.
- **Tests.** The marketplace repo's own workflow; vam `--fast`.
- **Turns green.** None (marketplace CI).
- **Decide.** `Decide: NOW — proof 3 (D23: the URL every node trusts), cost 3 (a new repo) → kept to one small wave: no site, no hub`

**CN-4.10 Real-tenant run and the two benchmark lines (last).**
- **Scope.** On the owner's test tenant, a node deployed from the base (Firecracker
  mode, so the Bun tools run in the VM through port 55 for the first time; see
  risks), real model on the owner's word. Admin opens Marketplace, installs
  Microsoft 365 from `github.com/squrell-inc/vam-marketplace` at
  `microsoft-365-v1.0.0`, enables the connector and consents (Phase 1), a
  member connects and runs the walkthrough: triage, find a slot, draft, send
  and book with cards, receipts verified offline. P6 timed (minutes, clicks
  from Install to the first mail read). The member approves a weekday morning
  triage. **Benchmark lane B** (seven days, zero reconnect prompts; then a
  forced revoke in Entra, reconnect, resume, zero duplicates): **owner's word**.
- **Files.** Runbook in the tracker's Phase close block; fixes, if any, as one
  fix wave.
- **Acceptance.** The walkthrough passes; P6 number recorded; lane B recorded
  or deferred by the owner.
- **Tests.** The team checklist steps; `vam receipt verify` on each run.
- **Turns green.** None (real tenant).
- **Decide.** `Decide: NOW — proof 3 (launch row 21: the demo on a real tenant; the two new lines), cost 1 (runbook and the owner's time)`

##### Waves

One wave at a time; branches `cn-4/wave-N` into `feat/connectors` (vam) or
`feature/connectors` (the VAM app). The bases are behind `main` by the release
merge commits: wave 0 merges `main` in first (or the owner fast-forwards them).
Wave 1 is the scaffold later waves share (vam-spec package, index and kind
table).

| Wave | Items | Turns green | Repo | Model | Budget | Depends on | actions | Status | PR |
|---|---|---|---|---|---|---|---|---|---|
| 0 | scenarios: 7 files, `git-marketplace` piece and `git-tag` step, marketplace steps (`marketplace-add`, `-refresh`, `-install`, `-update`), `marketplace-community` fixture, `marketplace-squrell` skeleton (index only), mock Graph endpoints and hold, README "Marketplace", runner switch `PASS_MARKETPLACE_SEAM` off, guards-only edits | — (7 red, right reason, class `product`) | vam | Sonnet low | 120 min | — | fast (`vam-e2e`, `e2e/`: `skip-vm-lane`) | [x] | — |
| 1 | CN-4.1, CN-4.2 | — (unit; flat, expected) | vam | Sonnet medium: moves the publish check out of vamd into vam-spec for the CLI and vamd, adds the index schema and the kind table the later waves share | 120 min | 0 | fast (`skip-vm-lane`) | [x] | — |
| 2 | CN-4.3 | — (unit) | vam | **Sonnet high**: the egress envelope (security boundary); a mistake lets a create send | 90 min | 1 | fast (`proto/`: `skip-vm-lane`) | [x] | — |
| 3 | CN-4.4 | — (first three move past their list steps) | vam | **Sonnet high**: vamd fetching admin-supplied Git URLs (untrusted input, a subprocess, SSRF). Orchestrator reads the fetcher's hardening and URL checks before merge | 150 min | 1 | fast (`skip-vm-lane`; `--release` in session for the seam) | [x] | — |
| 4 | CN-4.5 | first, second, third | vam | **Sonnet high**: catalog admission and trust (question 1, M2–M5). Orchestrator reads the admission rules, the read-only checks and the migration | 150 min | 3 | fast (`skip-vm-lane`) | [ ] | |
| 5 | CN-4.6 | fourth, fifth, sixth | vam | Sonnet medium: the first real skill code (Bun) across four artifact formats, a CI step, and a check of triggered runs | 150 min | 2, 4 | fast (workflow edit adds Bun; `skip-vm-lane`) | [ ] | |
| 6 | CN-4.7 | seventh | vam | **Sonnet high**: the effect journal on the egress path (R1 ordering, replay) | 120 min | 5 | fast (`skip-vm-lane`) | [ ] | |
| 7 | CN-4.8 | — (unit) | The VAM app | Sonnet medium: a new place on new API shapes, two trust badges, install confirm | 150 min | 4 | none (the VAM app unit only) | [ ] | |
| 8 | CN-4.9 | — (marketplace CI green) | vam-marketplace (+ small vam PR) | Sonnet low | 90 min | 5, 6 | marketplace: its own CI (public repo); vam: none (`[skip ci]`). first push to `main` covered by the owner's standing approval (2026-11-09) | [ ] | |
| 9 | CN-4.10 | — (real tenant; P6 line) | — (runbook; fixes as a fix wave) | Sonnet low (session supports the owner) | 60 min + owner's time | 7, 8; release or a deploy from the bases | none (the release PR runs the microVM lane once) | [ ] | |
| B | benchmark lane: seven-day run and forced revoke | — (P6/P13 line) | — | — | 7 days | 9 | lane — **owner's word** | [ ] | |

Scoreboard expectation: 0 green after waves 0–3 (1–3 flat, stated so they are
not flagged; wave 3 moves the first three to their install step); 3 after
wave 4; 6 after wave 5; 7 after wave 6; flat in 7–9.

**Phase rubric.** `Decide: SLICE — proof 3 (launch row 21: the demo on a real tenant; D23–D25), cost 3 (a new repo, an app page, a new trust boundary for fetched content) → slice: Git over https at pinned tags, read-only installs, three artifact kinds; fork, uninstall, auto-update policy, mirrors UI and connectors from marketplaces (Phase 5) later`


##### Phase 5 folded into Phase 4 (owner, 2026-11-09)

Relayed by the connectors design session on the owner's request; the orchestrator checked the owner's message in that session's transcript before acting. Wave 4 was in progress and runs unchanged; the rest is replanned.

- **Connectors in listings (new CN-4.11, wave 4b).** A listing folder may contain a connector file (the `spec/connectors/*.json` format). The import of CN-4.5 imports it like the other artifacts: validated with the wave 1 package checks (`ArtifactKind::Connector` becomes supported for import), Squrell-reserved ids (`microsoft-365`, `linear`) and unknown profiles refused, admitted read-only with its origin, and returned by `GET /v1/connectors` beside the built-ins (the built-ins win on a clash, which the reserved-id rule already prevents). Community connector files are limited to: remote MCP with discovery (as `linear`), API-key providers, and the existing OAuth profiles with the publisher's own public client id. Out of scope: bring-your-own OAuth client, a generic OAuth profile (small follow-up later), and the larger trust model of the old Phase 5 section (qualified connection ids, approved snapshots as a separate record, `vam connector init/test`). M4 changes: a non-Squrell listing may carry a connector within these limits. Still: nothing reaches a member before the admin enables it (D4), and enabling a community connector shows "Unverified".
  - Files (guide): `vam-spec` `package.rs` (connector kind supported for import, community limits), vam-orchestrator `marketplace/` import, the connectors listing (`connectors.rs` / `GET /v1/connectors` merge), node enablement for an installed connector id, `e2e/` new scenario and a community fixture listing with a connector.
  - Acceptance: an installed community connector appears for the admin with `origin` and badge `unverified`, can be enabled, and a member connects through the existing MCP flow; a listing whose connector reuses `microsoft-365`/`linear`, or names an unknown profile, or asks for a client secret, is refused with a reason; a connector update that changes publisher, client id, hosts, scopes or MCP server sets the connector back to not enabled (needs Enable again).
  - Turns green: new pending scenario `community-connector-appears-unverified-and-connects` (written as the wave's first commit, red for the right reason, then green).
  - Decide: `Decide: NOW — proof 2 (D26: developers ship connectors without a Squrell release), cost 2 (import path exists; MCP discovery profile exists)`. Model Sonnet high (a new trust boundary: third-party connectors).
- **CN-4.8 (wave 7) grows:** the install summary also shows a connector's publisher, client id, hosts, scopes and MCP server; an update that changes any of them shows as a change and needs Enable again.
- **Wave 8 merged into wave 5:** CN-4.6 and CN-4.9 are one wave. The Microsoft 365 listing is written straight into `squrell-inc/vam-marketplace` with its bootstrap (README, Apache-2.0 `LICENSE`, `marketplace.json`, `listings/microsoft-365/`, per-listing tag, CI that validates every package with a pinned vam CLI). vam's e2e fixture `marketplace-squrell` becomes a vendored copy stamped with the marketplace commit (the `SOURCE` stamp and `--fast` check of CN-4.9), replacing the provisional placeholders.
- **Waves 6 (journaled writes) and 9 (real tenant) unchanged.** Lane B still needs the owner's word.

**Waves after the fold** (supersedes rows 4–9 of the table above for order and scope):

| Wave | Items | Turns green | Repo | Model | Depends on | actions | Status | PR |
|---|---|---|---|---|---|---|---|---|
| 4 | CN-4.5 | first, second, third | vam | Sonnet high | 3 | fast (`skip-vm-lane`) | [x] | — |
| 4b | CN-4.11 (+ its pending scenario) | `community-connector-appears-unverified-and-connects` | vam | Sonnet high: third-party connectors | 4 | fast (`skip-vm-lane`) | [x] | — |
| 5 | CN-4.6 + CN-4.9 | fourth, fifth, sixth | vam-marketplace (first push to `main`) + vam (vendored fixture, stamp check, Bun in CI) | Sonnet medium | 2, 4 | marketplace: its own CI; vam: fast | [x] |; vam-marketplace |
| 6 | CN-4.7 | seventh | vam | Sonnet high | 5 | fast | [x] | — |
| 7 | CN-4.8 (+ connector summary and change → Enable again) | — (unit) | The VAM app | Sonnet medium | 4b | none | [x] | — |
| 9 | CN-4.10 | — (real tenant) | — | Sonnet low + owner | 6, 7 | none (release PR runs the lane) | [ ] | |
| B | lane | P6/P13 | — | — | 9 | lane, **owner's word** | [ ] | |

Scoreboard expectation after the fold: 8 scenarios; 3 green after wave 4, 4 after 4b, 7 after 5, 8 after 6.

##### Open questions (each with a default)

1. **Answered (owner, 2026-11-09): (a).** **REQUIRED owner decision — admitting Squrell content (`catalog_tier.rs`,
   `VAMD_MARKETPLACE_DOMAIN`).** Today the domain is unset by default, callers
   may never publish under it, skills resolve only from local registries and
   the org's domain, and tiers are `user | platform`. So nothing under
   `hub.squrell.cloud/squrell/…` can be admitted on any node. Options:
   - **(a) Default.** `VAMD_MARKETPLACE_DOMAIN` defaults to
     `hub.squrell.cloud` (the variable still overrides it, for staging). New
     tier `marketplace` (the `CHECK` becomes `user | platform | marketplace`;
     SQLite needs a table rebuild in the migration). Only the import path
     writes rows under the marketplace domain, and only from the marketplace
     entry flagged `squrell` (the preconfigured
     `https://github.com/squrell-inc/vam-marketplace`), never from one an admin
     added. `admits_skill_registry` admits the marketplace domain and each
     trusted marketplace's `publisherDomain` **for resolving installed rows**,
     never for a caller's publish. `reject_caller_platform_urn_with` keeps
     refusing the marketplace domain and also refuses every trusted
     marketplace's domain. Effect: `/v1/me` always returns `marketplaceDomain`
     and the app shows the Squrell badge on every node.
   - (b) Same changes, but the domain stays unset by default and each
     deployment sets `VAMD_MARKETPLACE_DOMAIN=hub.squrell.cloud`. A node
     without it cannot install the Squrell listing and shows no Squrell badge.
   - (c) No marketplace domain: import Squrell content into the org's own
     domain. Breaks the stable URN of §9.3 and the connector URN already under
     `hub.squrell.cloud`, and lets org publishers edit Squrell content. Not
     recommended.
2. **Answered (owner, 2026-11-09): Apache-2.0.** **REQUIRED owner decision — the marketplace repo's licence.** Default
   **Apache-2.0** (permissive, with a patent grant; fits D13: installed content
   is readable anyway; `vam` itself declares `MIT OR Apache-2.0`).
   Alternatives: MIT; CC-BY-4.0 for prose with Apache-2.0 for code; no licence
   (all rights reserved, visible but not reusable).
3. **Where the listing is authored, and how the other copy is checked.**
   Default: authored in vam (`e2e/world/fixtures/marketplace-squrell/`) while
   this phase builds it, because the scenarios that prove it run there. From
   wave 8 the marketplace repo is the source (D24); vam keeps a copy as a test
   fixture with a `SOURCE` stamp (tag and tree hash), and a `--fast` check
   refuses a fixture that differs from its stamp. A change goes to the
   marketplace first; a script refreshes the vam copy and its stamp.
4. **How vamd fetches Git.** Default: the `git` binary with a locked-down
   environment (no system or global config, empty `HOME`, no prompts,
   `protocol.allow=never` except `https`, hooks path empty, no submodules or
   LFS, shallow, size and time caps), after resolving the host and refusing
   private, loopback and link-local addresses (residual: DNS rebinding between
   the check and git's own lookup, recorded). Alternatives: an HTTPS tarball
   of the tag through `reqwest` and the SSRF guard (no subprocess, but
   host-specific URLs); a Rust Git library (a large new dependency).
5. **`git` on the node, and vamd's outbound network.** Default: `git` is a
   documented vamd runtime dependency; without it the Marketplace page says so
   and nothing else breaks. vamd reaches the marketplace hosts directly from
   the host (not through guest egress). Air-gapped nodes add a mirror URL.
6. **Tag pinning.** Default: `marketplace.json` is read at the default branch
   head; artifacts only at the listing's tag; the install records the commit
   SHA and each package digest; a tag whose commit changes is refused (never
   followed); no branch refs.
7. **"Refreshed on its own".** Default: each marketplace is checked every 6
   hours and when an admin clicks Refresh, with its own last-checked time and
   error; refresh only reads (updates are shown, never applied).
8. **Marketplace CI and the private `vam` repo.** Default: the workflow checks
   out vam at a pinned commit with a read-only deploy key (repository secret)
   and builds `vam-cli`, cached; pull requests from forks skip validation with
   a notice and a maintainer re-runs. Alternative: vam's release attaches a
   `vam` CLI binary somewhere public and CI downloads it by version.
9. **The domain of an added marketplace.** Default: its `marketplace.json`
   declares `publisherDomain`; the admin confirms it when adding; it must not
   be any domain M3 names; once added, callers cannot publish under it; adding
   is refused if org rows already exist under it.
10. **Draft permission and exact paths.** Default: `mail_draft` allowed (a
    draft reaches no one), send and create ask; all write tools use exact
    paths (CN-4.3). Without CN-4.3, drop `mail_draft` and accept that the
    create tool's prefix admits event forward, cancel and decline.
11. **Tool language.** Default: Bun TypeScript (the rootfs has Bun and no
    python3; design §4.1 shows `.ts`); CI and `local.sh` install a pinned Bun
    for process mode.
12. **Idempotency for send and create.** Default: the host journal (CN-4.7)
    is the guard and needs no guest change; events also carry a Graph
    `transactionId` derived from the arguments; `sendMail` has no Graph-side
    key, so a send whose answer was lost asks the member instead of sending
    again.
13. **Who sees the Marketplace page.** Default: org admins (`users:admin`).
    Members see installed artifacts in the catalog with their badge.
14. **Install and the connector.** Default: installing a listing does not
    enable its connector; the page links to Connectors (Phase 1 flow) when
    the connector is not enabled or not consented.
15. **Uninstall and fork.** Default: not in this phase. Disabling a
    marketplace hides its listings and keeps installs.
16. **Real-tenant run.** Default: a node deployed from the bases in
    Firecracker mode, a real model on the owner's word, the owner's test
    tenant (publisher verification not needed for the owner's own
    tenant, per Phase 1).
17. **Phase 2 findings.** Default: none blocks this phase or the seven-day
    run. The `/` prefix and legacy-policy findings are handled by narrow
    shapes (CN-4.6) and a permissions-mode agent. The background-process ride
    is a security finding, not a reliability one (Squrell's tools leave no
    process); it matters more for Unverified skills and stays in the backlog.
    `expires_at = None` does not occur for Entra (the app always sends
    `expiresIn`, and a refresh writes it). The ~10 s connection lock and the
    port-55 items do not affect a once-a-day run.

**Risks.**

- The real-tenant wave is the first end-to-end run of a code tool in a
  Firecracker guest through port 55 (MITM TLS) with Bun's `fetch`: process
  mode cannot prove it (`design/scenarios.md` §6). If Bun does not trust the
  guest CA or ignore the proxy settings, wave 9 needs a fix wave. Suggested:
  a short smoke at the start of wave 9 with one read tool before the
  walkthrough.
- Phase 2's team checklist uses the python3 probe skill on a real node; on a
  Firecracker node there is no python3, so that step fails for a reason
  outside the product. Tell the team (process mode only), or give them a Bun
  probe.
- The bases are behind `main`; wave 0 merges `main` first.

**Phase close:** all seven scenarios `active` and green; the VAM app unit suites on
the merged base; marketplace CI green on its tag; the walkthrough on the real
tenant with the P6 number; lane B on the owner's word; release (the release PR
runs the microVM lane once); team checklist out.


#### Phase 4 close (build)

**Scoreboard:** 8 scenarios added (7 planned + 1 from the Phase 5 fold), **8 `active` and green**; waves 0–7 incl. 4b merged (; `squrell-inc/vam-marketplace`); 3 fix cycles (W0 fmt, W3 guard auth, none after); about 9 h wall time on 2026-11-09. GitHub Actions: **0 minutes** (every wave proven in the session, `[skip ci]`). Smoke on `feat/connectors` after W6: 121/121; the VAM app 893/893, tsc clean, src-tauri 59/59.

**Open owner items before wave 9:**
1. Create tag `microsoft-365-v1.0.0` in `squrell-inc/vam-marketplace` (the session's git proxy cuts tag pushes).
2. (No longer needed: `squrell-inc/VAM` is public and the marketplace workflow clones it over HTTPS.)
3. Deploy: release to `main` (vam first: the app reads the new marketplace, origin and connector fields), or deploy vamd and the app from the bases. The release PR runs the microVM lane once — the first time the Bun tools run in a Firecracker guest through port 55 (recommendation 3 of the breakdown: start wave 9 with a one-tool smoke).
4. Lane B (seven days unattended, then a forced revoke): owner's word.

**Backlog recorded this phase:** nodes behind a corporate HTTPS proxy cannot fetch marketplaces (`Decide: BACKLOG — proof 1, cost 1`); community connectors beyond remote MCP with discovery (API-key connection kind; installed REST bindings in the egress gate) (`Decide: BACKLOG — proof 2, cost 2`); the receipt's `effects` section still says "not journaled" (E12 Phase 3); a multi-write tool call is protected as a whole.

**Wave 9 runbook (the owner's test tenant, owner at the keyboard, real model on the owner's word):**
1. Node deployed in Firecracker mode from `main` (or the bases); vamd can reach `github.com` and `graph.microsoft.com`; `git` installed (`VAMD_GIT_BIN` if not on `PATH`).
2. Admin: Org settings → Marketplace. The Squrell marketplace is listed with the Squrell badge; Refresh; the "Microsoft 365" listing shows its tools, hosts and paths. **Start the P6 timer.**
3. Install (confirm dialog). Agents, playbooks and skills show the Squrell badge, read-only.
4. Connectors → Microsoft 365: Enable and admin consent (Phase 1 flow, if not already).
5. Member (no instructions): Connections → Connect Microsoft 365; open the Executive assistant; "what's in my inbox today?" → the mail card. **Stop the P6 timer** at the first mail read; record minutes and clicks.
6. "Find 30 minutes with <colleague> tomorrow and draft a reply" → slot card, draft saved (no approval).
7. "Send it" → the send approval card with every recipient; approve; the mail arrives (Sent Items shows one copy). "Book it" → the booking card; approve; one event.
8. "Run the morning triage every weekday at 8:30" → schedule approval; approve; next morning the run happens by itself, sends nothing, shows the mail card.
9. `vam receipt verify` on each run: send and book lines `approved-once`, `secret-absent`.
10. Lane B (owner's word): leave it seven days; count reconnect prompts (expect 0); then revoke the app's sessions in Entra; the next scheduled run parks with "Reconnect Microsoft 365"; reconnect; it resumes with no duplicate send.

**Team checklist — Phase 4** (handed over at release; at most 10 steps)

Setup: deploy vamd from `main` (after the release), then the app from `main`. The tag `microsoft-365-v1.0.0` exists in `squrell-inc/vam-marketplace`. One admin and one member account; the admin has enabled Microsoft 365 with consent (Phase 1 checklist).

1. As the member, open Org settings: there is no Marketplace item. As the admin: Marketplace lists "Squrell" with the Squrell badge.
2. Refresh it, open "Microsoft 365": you see seven tools, host `graph.microsoft.com` with its paths, and that send and booking ask first.
3. Install it. Agents, Skills and Playbooks show the three items with the Squrell badge and no edit or release buttons.
4. Add `https://github.com/squrell-inc/vam-marketplace` again: it is refused as already trusted. Add an internal address (e.g. `https://localhost/x`): it is refused with a reason.
5. As the member, connect Microsoft 365 and ask the Executive assistant for today's mail: a mail card appears with no approval.
6. Ask it to draft and send a short reply to yourself: one approval card listing the recipient; approve; exactly one mail arrives.
7. Ask it to book a 15-minute meeting tomorrow: a booking card; approve; one event appears.
8. Ask it to run the morning triage on weekdays: approve the schedule; the next run appears by itself and sends nothing.
9. Run `vam receipt verify <run>` on the send run: it passes and shows one approved send.

Report: "Phase 4 pass", or the step number with a screenshot or the error text.

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

#### Phase 5 breakdown (phase-breakdown agent, 2026-11-18; approved by the owner 2026-11-18)

Written against `docs/connectors-org-supplied-client` (one docs commit ahead of
`origin/main`, which carries the Phase 4 release); `squrell-inc/vam-marketplace`
branch `box-connector` read; the VAM app not in this checkout (named from the
Phase 1, 2 and 4 file lists). No product code, build or test was run. Where the
code and the design differ, the code is the fact.

**Code map (today, checked 2026-11-18).**

- Connector file: `crates/vam-spec/src/connector.rs` `OAuth2Provider { profile,
  client_id, client_secret: ClientSecret { None, SqurellRelay }, scopes,
  org_consent }` under `deny_unknown_fields`: a file with `client` fails to
  parse. `validate_connector` has no client rule.
- Marketplace check: `crates/vam-spec/src/connector_listing.rs`
  `check_marketplace_connector` and `parse_marketplace_connector` refuse any
  `clientSecret` but `none` (text "asks for a client secret"), any profile but
  `discover`, any `bindings`. The capability summary is
  `vam-spec/src/package.rs` `ConnectorSummary { publisher, provider, profile,
  client_id?, client_secret, org_consent, scopes, mcp_url, mcp_host, hosts, tools }`;
  the import's `summary_digest` (`marketplace/import.rs`) hashes it, so a new
  summary field makes an update that changes it disable the connector
  (`reset_connector`) with no further code.
- Enable: `PUT /v1/connectors/{id}/node` (`api/connectors.rs` `put_node`,
  `users:admin`, idempotency key) → `connectors.rs` `PutNodeRequest { enabled,
  consent }` (`deny_unknown_fields`) → `plan_put` → table
  `node_enabled_connectors`. A disable keeps the recorded consent (Phase 1 Q7).
  `list_entry` shows the public definition only when enabled and swaps in
  `effective_client_id` (node env override `VAMD_CONNECTOR_<ID>_CLIENT_ID`, else
  the file's). `is_configured` refuses Enable for a placeholder client id.
- Member connect for an MCP connector: the app runs discovery, DCR and PKCE,
  then `PUT /v1/mcp/connections` with `oauth { resource, authorizationServer,
  tokenEndpoint, clientId, registrationMethod: dcr|cimd|manual }`
  (`api/mcp_connections.rs` `ConnectMcpOauthRequest`), then
  `PUT …/oauth/tokens` with `{ accessToken, refreshToken, clientSecret? }`.
  `connectors::gate_mcp_write` pins the URL, tools and risk classes to the
  connector and refuses org scope, but **the token endpoint and client id on
  the row are whatever the app sent**. A `clientSecret` in the upsert becomes a
  user-scope `connection` row `mcp_oauth_client_secret_binding_id(id)`
  (`mcp_registry.rs:779`): today that secret passes through the app.
- Refresh: MCP rows refresh on a 401 (`host_tools.rs` ~3095): refresh token and
  the optional client secret read at the row's own scope and owner, then
  `mcp_oauth_refresh.rs` `RefreshProfile::Mcp` (`resource` on every refresh,
  `client_secret_post` when given; DNS-pinned token endpoint, no redirects).
  One gate per (store, scope, owner, connection) (`lock_mcp_credentials`,
  `McpRefreshGate` with a generation counter) so concurrent calls share one
  refresh. Revoke on disconnect reads the per-row secret too
  (`mcp_oauth_refresh.rs` ~524). No early refresh for MCP rows.
- REST connectors (`microsoft-365`) refresh in `connectors.rs` `check_connection`
  with the Entra profile; untouched by this phase.
- Vault: `put_secret_with_owner(binding, value, SecretKind::Connection,
  &[host])` pins a row to hosts. `GET /v1/secrets` (`api/mod.rs`
  `list_secrets`) lists every org-scope row's metadata (binding id, kind, an
  8-hex fingerprint, destinations) to any caller with `secrets:write`;
  `DELETE /v1/secrets/*id` shows no kind check in the handler (DX-139 refuses
  writes of the `connection` kind; the wave confirms delete).
- Marketplace reads: the index (`marketplace.json`) at the remote's default
  branch head (`fetch.rs` `HEAD_REF`), artifacts only at the tag the index
  names (M5); `vam catalog import --ref` must equal that tag. Box's URN is
  under `hub.squrell.cloud`, so only the preconfigured Squrell entry may install
  it (M3). The `box-connector` branch has no tag and `main`'s index does not
  name Box: **no node can import the Box listing today** (open question 4).
- e2e: mock providers `microsoft-login`, `microsoft-graph`, `linear-server`
  (`crates/vam-e2e/src/connectors.rs`); the `mcp:<https url>` seam key
  (`PASS_MCP_SERVER_SEAM`) maps an installed connector's MCP URL to a mock;
  fixture `marketplace-community-connectors` (Bugwatch and three refused
  listings). No mock serves OAuth metadata, an authorization code or a
  confidential client. `expect: { runs: { count } }` and `chat { wait: false }`
  exist.
- The VAM app: Org settings Connectors workspace
  (`OrgSettingsConnectorsWorkspace.tsx`), tiles (`ConnectorTiles.tsx`), the
  MCP OAuth connect flow (discovery, DCR, PKCE, code exchange, token hand-off),
  Tauri commands in `src-tauri/src/{vamd.rs,lib.rs}`, Marketplace summary
  (CN-4.8).

**Shared invariants** (every item holds them; the orchestrator checks in review):

- **K1–K7 and M1–M6 hold.**
- **O1** The org client secret is written only by an org admin
  (`users:admin`), only through `PUT /v1/connectors/{id}/node`, as one
  org-scope `connection` row (`connector/<id>/oauth/client-secret`) pinned to
  the token endpoint host fixed at Enable. No route, list, log, event, error,
  receipt or app response returns it; it is never a guest binding and never a
  broker's; `/v1/secrets` neither lists, overwrites nor deletes it.
- **O2** The secret is sent to one place: the token endpoint (and the
  revocation endpoint when it is on the same host) pinned at Enable. Never to a
  URL a member's request, the app or a provider redirect names.
- **O3** For an `org` client, the client id, authorization endpoint, token
  endpoint and revocation endpoint a member's connection uses come from the
  node row, never from the request.
- **O4** A refresh token is spent once: concurrent calls on one connection
  share one refresh, and the rotated refresh token is stored before the access
  token is used.
- **O5** A connector with `clientSecret: "org_vault"` is never connected with
  tokens the app exchanged (the app cannot hold the secret): the app-path token
  upsert is refused for it.

##### Scenario delta

All process mode, mock model. One new runner-hosted provider,
`mock-filebox-server` (kind `mock-provider`, provider `filebox`, a fictional
Box-like service): a streamable-HTTP MCP server at `https://mcp.filebox.example/mcp`
(`search_files`, `read_file`; bearer required) that publishes RFC 9728
protected-resource metadata pointing at `https://auth.filebox.example`, which
publishes RFC 8414 metadata (authorize, token, revocation, S256, no
registration endpoint). The authorize endpoint mints a code for the registered
client id, redirect URI and S256 challenge (the runner plays the browser); the
token endpoint is a confidential client (`client_secret_post`), checks code,
verifier and redirect URI, issues short access tokens and **single-use rotating
refresh tokens** (a reused one answers `invalid_grant`). It logs field names,
`grant_type`, `client_id`, and `client-secret=current|old|wrong|none` and
`refresh=fresh|reused`, never a value. Steps: `mock-provider: { piece, expire:
true }` (the current access token is refused from now on), `holdToken:
true|false`, `rotateSecret: true` (the provider's current secret changes; the
old one is refused). Both seam origins (`mcp:` and a new `oauth:<https
origin>` key) map to the mock; the `oauth:` key is held out until wave 2 makes
vamd accept it. The fixture `marketplace-community-connectors` gains two
listings: `filebox-connector` (installs: `discover`, `client: org`,
`clientSecret: org_vault`) and `org-secret-without-org-client` (refused).
Runner steps: `connector-node` gains `client: { clientId, clientSecret: {
piece, which: current|wrong|rotated } }` (values from the mock, scrubbed);
`connector-connect` for a connector whose entry says `client: org` plays the
app: reads the pinned authorize endpoint and client id from `GET
/v1/connectors`, makes a PKCE pair, gets a code from the mock, and posts code,
verifier and redirect URI to the node's exchange route (open question 3).

| Scenario | Change | Layer | Given | Why |
|---|---|---|---|---|
| `listing-connector-with-an-admin-supplied-client-installs-and-waits-for-its-client` | new | process | `org-admin`, `org-member`, `marketplace-community-connectors` | D28, the marketplace half. The admin adds and refreshes; `filebox-connector`'s summary shows `client: org`, `clientSecret: org_vault` and text "admin-supplied client (with a secret)"; installed, the connector reads `askAdmin` for the member, Unverified, not enabled; `org-secret-without-org-client` is refused with a reason containing `client`; Bugwatch and the three Phase 4 refusals are unchanged. Red at the listing expect (the file does not parse: `client` is unknown), class `product` |
| `admin-supplied-client-is-set-only-by-an-org-admin-and-never-read-back` | new | process (mock providers) | as above, plus `org-administrator`, `mock-filebox-server` | O1, O3. Enable without a client is refused (409, "client id"); without a secret refused (400, "secret"); a member's Enable with a client is 403; a client on Bugwatch (publisher client) is refused (400). The org administrator enables with id and secret: the entry shows `provider.client: org`, `node.client.clientId`, `node.client.secretSet: true`, the pinned `authorizeEndpoint` and `tokenEndpoint` (host `auth.filebox.example`), `node.client.updatedBy`; `secret-absent` for the secret in `GET /v1/connectors` (admin and member), `GET /v1/secrets` (admin and member), events; a `DELETE /v1/secrets/connector/filebox-connector/oauth/client-secret` is refused. Disable then `{ enabled: true }` alone: `secretSet` still true (open question 1). Red at the first `connector-node` with `client` (unknown field, 400), class `product` |
| `admin-supplied-client-lets-members-connect-without-the-secret-leaving-the-node` | new | process (mock providers) | as above, plus `agent-plain` | D29, the phase goal. Enabled with the client; the member connects through the exchange route: the mock's token endpoint saw one `grant_type=authorization_code` with `client_id=<admin's>`, `client-secret=current`, `code_verifier` and `redirect_uri`; the tile reads `connected`. A read tool runs (`filebox__search_files`); the MCP mock saw the bearer. The app-path token upsert for this connector is refused (400, "exchange"); an MCP connect naming another token endpoint or client id is refused; an exchange with a redirect URI outside the app's is refused and the mock saw no second exchange. `secret-absent` for the client secret and the member's tokens in thread, events, receipt, workspace and every connector and MCP listing. Red at `connector-connect` (no exchange route: 404, `product`) |
| `rotating-refresh-token-is-spent-once-when-two-tasks-call-at-once` | new | process (mock providers) | as the third | O4 and Box's single-use refresh tokens. Connected; `expire: true`; `holdToken: true`; two chats by the member start with `wait: false` on two threads, each calling the read tool; the MCP mock saw two refused calls; `holdToken: false`; both runs succeed (`runs: { count: 2 }` succeeded); the token endpoint saw **one** `grant_type=refresh_token` with `client-secret=current` and `refresh=reused` count 0. A second `expire` and one more call: one more refresh, `refresh=fresh` (the rotated token was stored). Red at `connector-connect` until the exchange exists, then at the refresh count if the org secret is not sent (`client-secret=none`, `invalid_client`) |
| `admin-corrects-the-client-secret-and-members-stay-connected` | new | process (mock providers) | as the third | Replace and rotation. Enabled with `which: wrong`: the member's connect is refused with text naming the provider's refusal of "the client id or secret your admin entered", nothing stored (tile `connect`); the admin re-saves with `current` (enabled stays on): the connect works. `rotateSecret: true` at the provider, the admin saves `rotated`; `expire: true`; the next call refreshes with `client-secret=current` (the new one), the member is **not** asked to reconnect. The admin changes the client id: the member's tile reads `needsReconnect` (tokens belong to the old client). Red at `connector-connect` (404), class `product` |

**Existing scenarios whose `then` changes: none.** Checked from `guards` and
`given`: `community-connector-appears-unverified-and-connects` still sees
`connector-with-a-secret` (asks for `squrell_relay`) refused with "secret";
Bugwatch's summary gains fields but its expects read only existing ones; the
Linear and Microsoft 365 scenarios use no `client` and keep their refresh
shapes (Entra: no `client_secret`, asserted by
`expired-microsoft-token-is-refreshed-before-the-call`). **Guards-only
additions:** `vam:crates/vam-spec/src/connector_listing.rs` on
`community-connector-appears-unverified-and-connects` and
`listing-from-an-added-marketplace-installs-as-unverified-within-its-limits`;
`vam:crates/vam-orchestrator/src/host_tools.rs` on
`linear-reads-run-without-asking-and-writes-ask` (the refresh path changes);
`vam:crates/vam-orchestrator/src/api/mcp_connections.rs` on
`guest-cannot-use-an-mcp-token-placeholder`.

##### Benchmark delta

| Line (`design/benchmark.md` §1) | Baseline on this base | Measured by | Expected after |
|---|---|---|---|
| P9 **new task** "the org client secret is read back by a member, the app or the guest" | Not possible (no org secret) | `secret-absent` in the second and third scenarios | 0 bytes, 100% |
| P6 minutes and clicks from the admin's Enable (client id and secret) to a member's first Box read, real tenant | Not possible | CN-5.7, by hand, team, owner's word | first number recorded |

Mock-measured in CI; the real-tenant line only on the owner's word. The
orchestrator adds the P9 task to `design/benchmark.md` §1 on approval (docs only).

##### Items

**CN-5.1 `provider.client` and `clientSecret: "org_vault"` in vam-spec, the marketplace check and the summary.**
- **Scope.** `OAuth2Provider.client: ClientKind { Publisher, Org }`
  (`#[serde(default)]` = `publisher`, so every existing file parses unchanged);
  `ClientSecret::OrgVault` (`org_vault`). `validate_connector`: `org_vault`
  only with `client: org`; `client: org` with a `clientId` in the file is
  refused (the admin supplies it); `org` with profile `entra` refused
  (Squrell's client, locked 11). `check_marketplace_connector` and
  `parse_marketplace_connector`: allow `client: org` with `clientSecret: none |
  org_vault` (still `discover` + MCP only; `squrell_relay` still refused with
  the same text). `ConnectorSummary` gains `client` and the summary text "admin-supplied
  client" / "admin-supplied client (with a secret)", so the import's
  digest turns a change of either into "Enable again". `vam catalog validate`
  passes the Box `connector.json` from `box-connector` (copied as a vam-spec test fixture).
- **Files.** `crates/vam-spec/src/{connector.rs,connector_listing.rs,package.rs}`,
  `crates/vam-orchestrator/src/marketplace/summary.rs` (text only, if the text
  lives there), unit tests; JSON schema or docs table for the connector file if one exists
  (`docs/src/content/docs/authoring/` connector page, `status` frontmatter).
- **Acceptance.** Unit tests per rule (each refusal names the field); the
  built-ins and Bugwatch round-trip byte-stable; an update fixture that flips
  `client` sets the connector back to not enabled (import unit test).
- **Tests.** `cargo test -p vam-spec`, `cargo test -p vam-orchestrator marketplace`, `scripts/ci/local.sh --fast`.
- **Turns green.** `listing-connector-with-an-admin-supplied-client-installs-and-waits-for-its-client`.
- **Decide.** `Decide: NOW — proof 3 (D28, D32: Box cannot be described today), cost 1 (an enum, two rules, one summary field)`

**CN-5.2 Enable with the admin's client.**
- **Scope.** `PutNodeRequest.client: Option<Option<ClientRequest { client_id,
  client_secret? }>>` (omitted keeps; `null` clears, only with `enabled:
  false`). Allowed only for `client: org` connectors (400 otherwise). Enabling
  an `org` connector needs a stored or given client id (409 "client id"), and
  for `org_vault` a stored or given secret (400 "secret"). On a body with a
  client, `vamd` discovers the provider's endpoints from the connector's MCP
  URL (RFC 9728 protected resource, then RFC 8414 / OIDC metadata) through the
  SSRF guard (https, public DNS names, no redirects, size and time caps),
  requires S256, and pins `authorizeEndpoint`, `tokenEndpoint`,
  `revocationEndpoint?`, `issuer` on the node row (migration: columns on
  `node_enabled_connectors`, or a `node_connector_clients` table); a failed
  discovery refuses the save and writes nothing. The secret becomes the
  org-scope `connection` row `connector/<id>/oauth/client-secret` pinned to the
  token endpoint host (written last, after the row validates; replaced in
  place on a new secret). A new secret alone keeps members' connections; a new
  client id marks every member's connection to the connector `needs_reauth`.
  Disable keeps the client (open question 1); `client: null` deletes the row and marks
  connections `needs_reauth`. `list_entry`: for an `org` connector, `node.client
  { clientId, secretSet, authorizeEndpoint, tokenEndpoint, updatedAt }` (plus
  `updatedBy` for admins) once set; `provider.clientId` shows the admin's id
  when enabled; never the secret. `GET /v1/secrets` omits `connector/*/oauth/*`
  org rows and `DELETE /v1/secrets/…` refuses them. `is_configured` is true for
  an `org` connector only when its client is stored. OpenAPI for the new body
  and entry fields.
- **Files.** `crates/vam-orchestrator/src/{connectors.rs,api/connectors.rs,secrets.rs,api/mod.rs}`,
  a migration in the store, the discovery helper (new, reusing
  `vam-security` `ssrf_guard` or the MCP client's pinned fetch), `docs/api/openapi.yaml`,
  `crates/vam-orchestrator/tests/` (route tests).
- **Acceptance.** Route tests: every refusal writes nothing; the secret's
  value is absent from every response body and the audit log of the call; the
  row's kind is `connection`, scope `org`, destinations = the token host; a
  member's `PUT` with `client` is 403; re-enable keeps `secretSet`; client-id
  change flips members' rows.
- **Tests.** `cargo test -p vam-orchestrator connectors`, `--scenarios admin-supplied-client-is-set-only-by-an-org-admin-and-never-read-back`, `--fast`.
- **Turns green.** `admin-supplied-client-is-set-only-by-an-org-admin-and-never-read-back`.
- **Decide.** `Decide: NOW — proof 3 (D28), cost 2 (a body field, a migration, discovery on the host, the vault row)`

**CN-5.3 The node exchanges the code.**
- **Scope.** New route (open question 3: `POST
  /v1/connectors/{id}/connection/oauth/code`), the caller's own scope
  (`secrets:write`, user scope only), idempotency key, body `{ code,
  codeVerifier, redirectUri }` (`deny_unknown_fields`; no endpoint or client
  field). Refused unless the connector is `client: org` with `org_vault`,
  enabled, and its client stored (409/400 with reasons). `redirectUri` must be one of the
  app's (open question 6). `vamd` posts `authorization_code` with `client_id`,
  `client_secret` (`client_secret_post`), `code_verifier`, `redirect_uri` and
  the RFC 8707 `resource` to the pinned token endpoint (same DNS-pinned client
  as refresh, the secret only after the host matches the row's pinned
  destination), then creates or replaces the caller's MCP connection row from
  the connector (URL, tools) and the node row (client id, token and revocation
  endpoints; registration method `manual`), stores tokens as today (rotated
  refresh first), and returns the caller's list entry. A provider
  `invalid_client` answers 409 "the provider refused the client id or secret
  your admin entered" and writes nothing; `invalid_grant` answers 400. For an
  `org` connector, `PUT /v1/mcp/connections` refuses an `oauth` block whose
  token endpoint or client id differ from the node's, and
  `PUT …/oauth/tokens` refuses for `org_vault` (400, "use the code exchange")
  and refuses `clientSecret` for any connector-backed row. OpenAPI; the
  `runtime/tools.md` OAuth section says the node exchanges the code for an
  admin-supplied client with a secret (D29, page `status` kept `shipped` with a
  `proves` line for the new scenarios).
- **Files.** `crates/vam-orchestrator/src/{api/connectors.rs,connectors.rs,api/mcp_connections.rs,mcp_oauth_refresh.rs (token request),mcp_registry.rs}`,
  `docs/api/openapi.yaml`, `docs/src/content/docs/runtime/tools.md`, route tests.
- **Acceptance.** Route tests against a loopback token server: form fields and
  order of writes; nothing written on any refusal; a body with `tokenEndpoint`
  or `clientId` is 400; responses and errors hold no token or secret.
- **Tests.** `cargo test -p vam-orchestrator`, `--scenarios admin-supplied-client-lets-members-connect-without-the-secret-leaving-the-node`, `--fast`.
- **Turns green.** `admin-supplied-client-lets-members-connect-without-the-secret-leaving-the-node` (with CN-5.4).
- **Decide.** `Decide: NOW — proof 3 (D29: the secret cannot pass through the app), cost 2 (one route on existing token code)`

**CN-5.4 Refresh and revoke with the org secret.**
- **Scope.** For a connector-backed row whose connector is `client: org`, the
  401-refresh (`host_tools.rs`) and the disconnect revoke
  (`mcp_oauth_refresh.rs`) read the org-scope secret row (not a per-row one)
  and use the node's pinned token endpoint and client id; a row whose stored
  endpoint or client id differs from the node's is marked `needs_reauth`
  without a request. `RefreshProfile::Mcp` keeps `resource`. Single flight:
  the existing per-connection gate; verify and test that the second waiter
  sees the new generation and does not refresh, and that the rotated refresh
  token is stored before the gate is released (O4). A reused-token
  `invalid_grant` marks `needs_reauth` (existing path). A replaced secret is
  read on the next refresh (no cache). `invalid_client` on refresh marks
  `needs_reauth` with `lastRefreshError` naming the client (open question 8).
- **Files.** `crates/vam-orchestrator/src/{host_tools.rs,mcp_oauth_refresh.rs,mcp_registry.rs,connectors.rs}`, unit tests.
- **Acceptance.** Unit test: two concurrent refreshes against a single-use
  token server send one request; the secret is read at org scope; the Entra
  and plain-MCP shapes are unchanged (existing tests stay green).
- **Tests.** `cargo test -p vam-orchestrator mcp_oauth host_tools`, `--scenarios rotating-refresh-token-is-spent-once-when-two-tasks-call-at-once,admin-corrects-the-client-secret-and-members-stay-connected`, `--fast`.
- **Turns green.** `rotating-refresh-token-is-spent-once-when-two-tasks-call-at-once`, `admin-corrects-the-client-secret-and-members-stay-connected`.
- **Decide.** `Decide: NOW — proof 3 (Box refresh tokens are single-use: without it every member reconnects hourly), cost 1 (a secret source and an endpoint source on an existing path)`

**CN-5.5 the VAM app.**
- **Scope.** Org settings → Connectors, an `org` connector: Enable asks for
  the client id and, for `org_vault`, the secret (password field, never
  prefilled, never stored by the app, never logged; sent once in the `PUT`
  body); shows `secretSet`, the pinned endpoints and "Replace secret" /
  "Change client id" (with the warning that members reconnect). The Marketplace
  and Connectors summaries show "admin-supplied client" (and "with a secret").
  Member connect for an `org` connector: no discovery or registration; the
  authorize URL from the entry's `node.client.authorizeEndpoint` and client id,
  PKCE S256, redirect `squrell://oauth` (or loopback); for `org_vault` it
  posts code, verifier and redirect URI to the node's exchange route and never
  exchanges itself; an `org` client without a secret keeps the app exchange
  (Phase 6). The node's 409 text for a refused client is shown to the member
  as "ask your admin".
- **Files.** `src/components/vam/{OrgSettingsConnectorsWorkspace.tsx,ConnectorTiles.tsx}`,
  the MCP OAuth flow module, `src/types/` connector types, Marketplace summary
  (CN-4.8 place), `src-tauri/src/{vamd.rs,lib.rs}`, tests.
- **Acceptance.** Unit tests on fixtures: the secret field is write-only and
  absent from state after save; connect builds the authorize URL from the
  entry only; no DCR request for `org`; the exchange call carries no secret.
- **Tests.** `CI=1 pnpm test`, `pnpm exec tsc --noEmit`, `cargo test --manifest-path src-tauri/Cargo.toml`.
- **Turns green.** None (unit; team checklist).
- **Decide.** `Decide: NOW — proof 3 (no admin can enter a client without it), cost 2 (two places on known patterns, one flow change)`

**CN-5.6 Squrell marketplace index read from a named branch (staging nodes only).**
- **Scope.** Open question 4's default: `VAMD_SQURELL_MARKETPLACE_INDEX_REF`
  (a branch name, `[A-Za-z0-9._/-]`, no `..`; unset = the default branch head)
  makes the preconfigured Squrell entry read `marketplace.json` at that
  branch's head. Artifacts are still installed only at the tag the index
  names, pinned and never followed (M5 unchanged); the entry shows `indexRef`
  in `GET /v1/marketplaces` and vamd logs it at start. Added marketplaces
  cannot set it.
- **Files.** `crates/vam-orchestrator/src/marketplace/{mod.rs,fetch.rs,store.rs}`,
  `crates/vam-orchestrator/src/daemon.rs`, `docs/src/content/docs/orchestrator/overview.md` (env table), unit tests.
- **Acceptance.** Unit tests: index from the named branch, artifacts from the
  tag only, a bad name refused at start; unset behaves as today (Phase 4
  marketplace scenarios unchanged).
- **Tests.** `cargo test -p vam-orchestrator marketplace`, `--fast`.
- **Turns green.** None (team test path).
- **Decide.** `Decide: NOW — proof 2 (D32: the team tests Box before release), cost 1 (one ref in the fetch)`

**CN-5.7 The Box listing ready to tag, and the team's run.**
- **Scope.** On `vam-marketplace` `box-connector`: tool names checked
  against the live `tools/list` and fixed in `connector.json`, the skill, the
  playbook and the agent; `requiresNode` set to the first node version with
  Phase 5; CI's pinned vam commit moved to the Phase 5 merge so `vam catalog
  validate` passes; `docs/listing-box.md` step 2 rewritten for the import path
  of CN-5.6 (owner pushes tag `box-v1.0.0-rc.1` on the branch commit). Team
  run on a real Box tenant, owner's word: Enable with the Box integration's id
  and secret, member connects, the Box assistant answers from a file, a second
  refresh after an hour works, disconnect revokes, `secret-absent` checked by
  hand in the API listings. P6 number recorded.
- **Files.** vam-marketplace: `listings/box/**`, `.github/workflows/validate.yml`,
  `docs/listing-box.md`. vam: Phase 5 close block (runbook, team checklist).
- **Acceptance.** Marketplace CI green on the branch; the team's run passes or
  its failures land as one fix wave.
- **Tests.** Marketplace CI; the runbook steps.
- **Turns green.** None (real tenant).
- **Decide.** `Decide: NOW — proof 3 (exit criterion), cost 1 (data and a runbook)`

##### Waves

One wave at a time; branches `cn-5/wave-N` into the base (open question 9).
Wave 1 is the scaffold later waves share (the connector file shape and the
summary).

| Wave | Items | Turns green | Repo | Model | Budget | Depends on | actions | Status | PR |
|---|---|---|---|---|---|---|---|---|---|
| 0 | scenarios: 5 files, `mock-filebox-server` (MCP, metadata, authorize, confidential token endpoint, single-use refresh, hold, rotate, expire), two fixture listings, `connector-node.client`, org-client `connector-connect`, `oauth:` seam key held out, README "Connectors" additions, guards-only edits | — (5 red, right reason, class `product`) | vam | Sonnet low | 120 min | — | fast (`vam-e2e`, `e2e/`: `skip-vm-lane`) | [x] | #33 |
| 1 | CN-5.1, CN-5.6 | first | vam | Sonnet medium: changes the connector file schema and the marketplace trust check (a community connector may now ask for a secret) and touches the marketplace fetch | 90 min | 0 | fast (`skip-vm-lane`) | [x] | #36 |
| 2 | CN-5.2 | second | vam | **Sonnet high**: writes a client secret to the vault from an API body, a migration, and host-side discovery of untrusted endpoints (SSRF). Orchestrator reads the vault write, the redaction and the discovery guard before merge | 150 min | 1 | fast (`skip-vm-lane`) | [x] | #39 |
| 3 | CN-5.3, CN-5.4 | third, fourth, fifth | vam | **Sonnet high**: a new route that sends the org secret, the refresh gate for single-use tokens (concurrency), and refusals on the MCP routes (auth). Orchestrator reads where the secret is read and sent, and the gate test | 180 min | 2 | fast (`skip-vm-lane`) | [x] | #44 |
| 4 | CN-5.5 | — (unit) | The VAM app | **Sonnet high**: the admin's secret passes through the app once and the sign-in flow changes (no registration, no app-side exchange for this case); a slip logs or keeps the secret | 150 min | 3 | none (app unit only) | [x] | Squrell-App #11 |
| 5 | CN-5.7 | — (marketplace CI; real tenant) | vam-marketplace (+ vam close block) | Sonnet low (session supports the team) | 60 min + team's time | 4; a deploy of vamd and the app from the base | marketplace: its own CI; vam: none | [ ] | |

Scoreboard expectation: 0 green after wave 0; 1 after wave 1; 2 after wave 2;
5 after wave 3; flat in 4–5 (stated so they are not flagged).

**Phase rubric.** `Decide: SLICE — proof 3 (D28, D29, D32: Box and every no-registration provider), cost 3 (secret handling, a new route, both repos) → slice: remote MCP with discovery only; client secret post only (no private-key JWT); no admin notification on a refused client; no early refresh for MCP rows; fixed endpoints and company address in Phase 6`

##### Open questions (each with a default)

1. **Disable and re-enable: keep or clear the client and secret.** Default
   **keep** (as consent is kept, Phase 1 Q7): a disable stops members at once
   (`askAdmin`, calls refused) but keeps the client id, the pinned endpoints
   and the vault row; `{ enabled: true }` restores them and members'
   connections work again without a reconnect. Clearing is explicit:
   `client: null` with `enabled: false` deletes the row and marks members'
   connections `needs_reauth`. Alternative: clear on disable (an admin must
   re-enter the secret after any pause).
2. **Does the code exchange for a secret-less org client also move to vamd?**
   Default **no** (D29 as written): `client: org` with `clientSecret: none`
   (Zendesk, Salesforce in Phase 6) keeps the app's exchange; the app uses the
   node's client id and pinned endpoints, and `PUT /v1/mcp/connections` refuses
   other values (O3). The node route refuses connectors without `org_vault`.
   Revisit in Phase 6: one path for every org client would also stop the app
   naming the token endpoint for REST connectors.
3. **Route names.** Default: Enable stays `PUT /v1/connectors/{id}/node` with
   a `client: { clientId, clientSecret? }` field; the exchange is `POST
   /v1/connectors/{id}/connection/oauth/code` with `{ code, codeVerifier,
   redirectUri }`, answering the caller's list entry (beside the REST
   connection routes, since the node, not the request, decides the client).
   Alternatives: `POST /v1/mcp/connections/{id}/oauth/exchange` (MCP route
   family, but the id would have to be a connector); a separate `PUT
   /v1/connectors/{id}/node/client` for the secret (cleaner audit, a second
   admin call).
4. **How the team imports the unreleased Box listing.** The index is read at
   the default branch head and artifacts only at tags; the Box URN can only
   come from the Squrell entry (M3); so the `box-connector` branch cannot be
   installed today. Default **(a) CN-5.6**: a staging node sets
   `VAMD_SQURELL_MARKETPLACE_INDEX_REF=box-connector`, the owner pushes tag
   `box-v1.0.0-rc.1` on the branch commit and the branch index names it;
   nothing reaches `main` before Phase 5 is released (locked 16). (b) Zero
   code: a `process-guest` vamd with `VAMD_PROCESS_MARKETPLACE_URLS` mapping the
   Squrell URL to a local bare clone whose default branch is `box-connector`
   (works, since Box tools run in vamd, but the team runs a test build). (c)
   Merge into `main` gated by `requiresNode`: breaks locked 16.
5. **Where the endpoints come from.** Default: `vamd` discovers them at
   Enable from the connector's MCP URL (RFC 9728, then RFC 8414 or OIDC),
   requires https on public names, S256 and a token endpoint, and pins them;
   they change only when the admin saves the client again (re-discovery). The
   admin sees the token host before saving. Alternative: the app discovers and
   sends them at Enable (no host fetch, but the endpoint the secret goes to
   would be request data).
6. **Redirect URIs the exchange accepts.** Default: `squrell://oauth` and
   `http://127.0.0.1:<port>/<path>` only (the app's; the admin registers them
   at the provider, per `docs/listing-box.md`), else 400. A node setting for
   more is later.
7. **Changing the client id.** Default: members' connections to the connector
   are marked `needs_reauth` (their tokens belong to the old client); a new
   secret alone keeps them.
8. **A refused client.** Default: at the exchange, 409 with "the provider
   refused the client id or secret your admin entered", nothing written; at a
   refresh, `invalid_client` marks the connection `needs_reauth` with that
   reason (the existing reconnect notification). An admin notification for a
   broken client is backlog (`Decide: BACKLOG — proof 1, cost 1`).
9. **Base branch.** Default: `feat/connectors` re-cut from `main` after this
   docs branch merges (it was deleted at the Phase 4 release); waves 0–4 merge
   there, released to `main` at the phase close.
10. **Unverified connectors with an org secret.** Default: allowed from any
    trusted marketplace (D28 says marketplace connectors may use it): the secret is the
    org's own, entered by its admin, sent only to the token host the admin
    sees at Enable; the summary says "admin-supplied client (with a secret)"
    beside the Unverified badge. Alternative: Squrell marketplace only.
11. **The org secret row in `/v1/secrets`.** Default: not listed and not
    deletable there (members hold `secrets:write` and would see its
    fingerprint and id); the connector entry's `secretSet` and `updatedAt` are
    the only view.
12. **Box facts not yet checked** (tool names, PKCE S256 in Box's metadata,
    any `scope` value, Box AI enabled). Default: the team checks them on the
    live server in wave 5 before the tag; the scenarios do not depend on them.

**Risks.**

- A refresh that Box accepts but whose rotated token `vamd` fails to store
  spends the member's only refresh token: the member reconnects. The store
  order (refresh token first) and the gate keep the window small; recorded,
  not fixed.
- Discovery at Enable is the node's first fetch of a provider metadata URL
  named by a connector file: it must use the SSRF guard and cap sizes, or a
  community connector could point the node at an internal address.
- The app wave is the first time an admin types a secret into the app; the
  orchestrator checks the app's logs and state stores in review.

**Phase close:** all five scenarios `active` and green; the VAM app unit
suites on the merged base; Box marketplace CI green on its branch; the team's
real-tenant run on the owner's word with the P6 number; release (the release PR
runs the microVM lane once); team checklist out; tag `box-v1.0.0` and the
listing merged into `vam-marketplace` `main` after the release.

#### Phase 5 close

**Released:** 2026-11-19. VAM #52 (`feat/connectors` into `main`, merge 4f24c41) and Squrell-App #12 (merge bb4ee15). Before the release, six rounds of CodeRabbit findings on #52 were fixed in #68, #70, #71, #73 and #74, with main merged in by #72 (axum 0.8 route syntax fixed there). CN-5.7 stays open for the team's Box run; the next phase does not wait for it (playbook §6).

**Findings and backlog (recorded, not fixed):**
- The admin cannot choose how the client secret is sent to the token endpoint (`client_secret_basic` or `client_secret_post`); discovery decides (CodeRabbit on #52, thread 4619996438). `Decide: BACKLOG — proof 1 (no provider needs it yet; revisit when Box or Zendesk does), cost 1`.
- A member reconnecting a working connection reads `needs_reauth` between the exchange commit and tool adoption. `Decide: BACKLOG — proof 1, cost 1`.
- A first-time tools-pending failure shows `needsReconnect` but opens no new notification. `Decide: BACKLOG — proof P6, cost 1`.
- The app token-upsert route (`PUT .../oauth/tokens`) still commits `connected` before adoption (the app holds the tokens and can retry). `Decide: BACKLOG — proof 1, cost 1`.
- The refresh-versus-admin-save races are proved by unit tests only; the filebox e2e mock cannot hold a refresh across an admin save. `Decide: BACKLOG — proof 1, cost 1`.
- The app chose a new `Idempotency-Key` on every call, so its automatic repeat of a code exchange after a renewed login did not reuse the key the node's single-spend check keys on. `Decide: SLICE — proof 1, cost 0 → fixed in Squrell-App #13, merged 2026-11-19 (5941b3a)`.

### Phase 6 — Company address and REST from marketplaces (Zendesk)

**Goal:** a connector reaches a per-company host and a marketplace connector
may be REST with skills (design §13, D30, D31).
**Depends on:** Phase 5.
**Status:** breakdown approved 2026-11-19 (owner: recommendations 1 and 2 and every open-question default as written); wave 0 started.
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

- [ ] CN-6.1 The connector file: `profile: oauth` with fixed `endpoints`, one `tenant` parameter, marketplace REST bindings, templated skill egress hosts, the capability summary
- [ ] CN-6.2 Enable with a company address: one DNS label, filled once into every host; the admin's client pinned to the filled endpoints
- [ ] CN-6.3 The node exchanges the code for every admin-supplied client (secret or not) and keeps a REST connection for `profile: oauth`; refresh at the filled token endpoint
- [ ] CN-6.4 The egress swap for installed connectors: filled bindings and shapes, per-host token rows, unique placeholders
- [ ] CN-6.5 A skill learns its company host without a guest change
- [ ] CN-6.6 the VAM app: the company address at Enable, the filled hosts, one connect path for admin-supplied clients
- [ ] CN-6.7 The Zendesk listing on a `vam-marketplace` branch, and the team's run on a real Zendesk instance
- [ ] CN-6.8 (candidate, backlog) Salesforce `instance_url` checked against the company address

#### Phase 6 breakdown (phase-breakdown agent, 2026-11-19; awaiting owner approval)

Written against `docs/connectors-p6-breakdown`, cut from `origin/feat/connectors`
at 5d020be (equal to `main`, which carries the Phase 5 release).
`squrell-inc/vam-marketplace` read with `git ls-remote` (branches `main`,
`box-connector`; no Zendesk branch yet). The VAM app is not in this checkout
(named from the Phase 5 file lists). No product code, build or test was run.
Where the code and the design differ, the code is the fact (table at the end).

**Code map (today, checked 2026-11-19).**

- Connector file: `crates/vam-spec/src/connector.rs` `OAuth2Provider { profile,
  client_id, client, client_secret, scopes, org_consent }` under
  `deny_unknown_fields` (63-77); `OAuthProfile` is `entra | discover | google |
  slack` (79-86): a file with `profile: oauth`, `endpoints` or `tenant` does not
  parse. `validate_connector` (267-366) wants exact lowercase binding hosts
  (`valid_host` 259-265, used at 342-347) and an `https` MCP URL; nothing fills
  a parameter. `Binding { name, placeholder, value: access_token, inject:
  bearer|header|query, hosts }` (170-192).
- Marketplace check: `connector_listing.rs` `check_marketplace_connector`
  refuses every profile but `discover` (229-239) and any `bindings` (264-266,
  "remote MCP only"), and needs an MCP server whose host is a full public DNS
  name (`check_mcp_server_url` 110-165, which also refuses internal suffixes).
  `parse_marketplace_connector` (293-330) names a bad profile before parsing.
  Summary: `vam-spec/src/package.rs` `ConnectorSummary` (541-567), whose
  `hosts` is "the MCP server's" only; `summarize_connector` (712-) knows no REST
  hosts. A change of the summary's digest resets Enable on update
  (`marketplace/import.rs` `reset_connector`, 864-).
- Skill egress: `vam-spec/src/skill_egress.rs` requires `egress[].host` to be an
  exact DNS name (`valid_dns_host` 45-49, used at 80-89): a skill cannot write
  `{subdomain}.zendesk.com`. Shapes are parsed host-side at session boot from
  the materialized skills (`manager.rs` 1411-1414 →
  `connection_egress::parse_skill_shapes` 78-125).
- Enable: `PUT /v1/connectors/{id}/node` (`api/connectors.rs` 32, `put_node`)
  → `PutNodeRequest { enabled, consent, client }` (`connectors.rs` 378-393,
  `deny_unknown_fields`) → `plan_put` (431-490). `consent.tenantId` is the
  Entra tenant GUID (`parse_tenant_guid` 410-421): the word "tenant" is already
  on the wire. For `client: org` the handler discovers endpoints from
  `def.mcp.url` (`api/connectors.rs` 181-190, `connector_discovery::discover`)
  and pins them in `node_connector_clients` (`connector_client.rs` `NodeClient`,
  `plan_client` 321-387). No path pins endpoints from the file.
- `list_entry` (`connectors.rs` 536-590) copies the raw definition's
  `provider`, `bindings` and `mcp` into an enabled entry: a template would show
  unfilled.
- The org client is MCP-only: `org_connector` returns `None` without an MCP
  policy (`connector_client.rs` 938-963). The exchange route `POST
  /v1/connectors/{id}/connection/oauth/code` (`api/connectors.rs` 440-)
  refuses any connector without `org_vault` (459-467, "signs in with the app's
  own exchange") and writes an MCP row (`connector_exchange.rs`); `code_form`
  always adds `resource` and, for `client_secret_post`, the secret
  (`mcp_oauth_refresh.rs` 525-543); `exchange_authorization_code` requires a
  secret (549-560). The app-path token upsert is refused only for `org_vault`
  (`api/mcp_connections.rs` 1101).
- REST connections are Entra-only: `entra_provider` (`connectors.rs` 737-754);
  `PUT /v1/connectors/{id}/connection` takes app-exchanged tokens plus an id
  token whose `tid` must equal the consent (`plan_connection` 814-870); the
  token host is hard-coded `login.microsoftonline.com` (863, 1140); the access
  token is pinned to the first host of the first binding only (850-857,
  1146-1152); refresh is `EntraRefresher(tenant, client_id, scope,
  refresh_token)` (974-982) and `RefreshProfile` has `Mcp` and `Entra` only
  (`mcp_oauth_refresh.rs` 116-123). Single flight per (owner, connector):
  `lock_connection` (`connectors.rs` 1211-) and `ensure_fresh`
  (`connection_egress.rs` 267-305).
- The egress swap knows built-in connectors only: `builtin_bindings()`
  (`connection_egress.rs` 44-60), used by `SessionConnectionSource::new` (339;
  built at `brokers.rs` 1087); `token_for` finds the definition in
  `builtin_connectors()` (407-410), `connector_id_of` too (586-595). Only
  `inject: bearer` with `value: access_token` attaches (419-423). The intent
  park, by contrast, reads every entry (`connector_park.rs` 70) and uses the
  Entra refresher (155-156). A marketplace REST connector's binding would be
  parked on, then never attached.
- Guest tools: `vam-runtime/src/exec.rs` clears the environment and sets a
  fixed list (543-605); `LocalExecSpec.env_vars` is parsed
  (`vam-runtime/src/skills.rs` 380-409) and never applied. A skill has no way
  to learn a per-company host today.
- e2e: mock providers `microsoft-login`, `microsoft-graph`, `linear-server`,
  `filebox` (`crates/vam-e2e/src/connectors.rs`); seam keys `mcp:<url>`,
  `oauth:<origin>` and the fixed `microsoft-graph` (`process_mode.rs` 150-180),
  the Graph key served by the egress gate (`brokers.rs` 1133-1138, process-guest
  only). No key maps an arbitrary REST host. Fixture
  `marketplace-community-connectors`: six listings, all MCP.

**Shared invariants** (every item holds them; the orchestrator checks in review):

- **K1–K7, M1–M6 and O1–O5 hold.** The Entra path is unchanged (K4): no
  `resource`, the consent tenant, the id-token check.
- **T1** The company address is set only by an org admin (`users:admin`),
  only through `PUT /v1/connectors/{id}/node`, as one DNS label under the suffix
  the connector file fixes. It is filled in once, at that save; afterwards every
  host the node uses (sign-in endpoints, MCP URL, binding hosts, the hosts of
  tool shapes) is exact. No member request, token response, provider answer or
  skill changes a host.
- **T2** A binding's access token is attached only to a request whose host is
  one of that connector's filled binding hosts, inside the calling tool's filled
  shape, for an enabled connector, from the member's own `connection` row pinned
  to that host.
- **T3** Codes and refresh tokens go only to the token endpoint filled at
  Enable. A connection made under another address or client is never refreshed
  (`needs_reconnect`, no request).
- **T4** A marketplace binding's placeholder is `vamsec_<connector id>_<binding
  name>`: unique on the node by construction; no installed connector can claim
  a built-in placeholder.
- **T5** The company address is not a secret and may appear in entries, events
  and receipts; tokens never do.

##### Scenario delta

All process mode, mock model. One new runner-hosted provider,
`mock-ticketdesk-server` (kind `mock-provider`, provider `ticketdesk`, a
fictional Zendesk-like service) on one loopback listener for any
`<company>.ticketdesk.example` (the scenarios use `acme` and `globex`): fixed
OAuth endpoints and **no metadata** (`GET /oauth/authorizations/new` mints a
code for a client id starting `e2e-ticketdesk-client`, the redirect URI and an
S256 challenge, 302; `POST /oauth/tokens` is a public client: it refuses a
`client_secret`, checks single-use codes with verifier and redirect URI, and
issues access tokens and rotating refresh tokens), and a REST API (`GET
/api/v2/search.json`, `GET /api/v2/tickets/{id}.json`, `POST
/api/v2/tickets.json`) that needs the current bearer for **that** company. It
logs field names, `host=<company>`, `grant_type`, `client_id`,
`redirect_uri`, `client-secret=none|present`, `resource=none|present`,
`refresh=fresh|reused`, `method=` and
`bearer=current|stale|placeholder|unknown|none`, never a value. Step
`mock-provider: { piece, shortTokens: true }` makes new access tokens live
60 s (inside the node's 120 s swap window, so every call refreshes first).
Seam: `oauth:https://acme.ticketdesk.example` (and `globex`), an existing key;
a new `rest:<https origin>` key for the API, held out until wave 4 makes vamd
accept it (as `oauth:` was in Phase 5). The fixture
`marketplace-community-connectors` gains `ticketdesk-connector`: connector
`ticketdesk` (`profile: oauth`, `endpoints` on
`https://{subdomain}.ticketdesk.example`, `tenant { name: subdomain, suffix:
ticketdesk.example, label: "Ticketdesk address" }`, `client: org`,
`clientSecret: none`, binding `api` / `vamsec_ticketdesk_api` on
`{subdomain}.ticketdesk.example`), a skill `ticketdesk-tickets` with a read
tool (`GET`, prefixes `/api/v2/search.json` and `/api/v2/tickets/`) and a
create tool (`POST` exact `/api/v2/tickets.json`, approval required) whose
shapes name `{subdomain}.ticketdesk.example`, and an agent. Two refused
listings: `company-address-outside-its-suffix` (a binding on
`{subdomain}.elsewhere.example`) and `rest-connector-with-a-foreign-placeholder`
(placeholder `vamsec_m365_graph`). Runner steps: `connector-node` gains
`tenant: { subdomain }`; `connector-connect` with a ticketdesk `piece` plays
the app (reads `node.client.authorizeEndpoint` and `node.client.clientId`, PKCE,
gets a code, posts `{ code, codeVerifier, redirectUri }` to the node's exchange
route); `via: tokens` sends tokens to `PUT /v1/connectors/{id}/connection`.

| Scenario | Change | Layer | Given | Why | Red point (today) |
|---|---|---|---|---|---|
| `rest-listing-with-a-company-address-installs-and-shows-its-hosts` | new | process | `org-admin`, `org-member`, `marketplace-community-connectors` | D31, the marketplace half. The listing's summary shows `profile: oauth`, `client: org`, "admin-supplied client", the company-address label and suffix, the binding host `{subdomain}.ticketdesk.example` and the skill's egress entries; a member cannot install; the admin installs (connector `askAdmin`, Unverified, not enabled; skill and agent installed read-only); the two refusals name `suffix` and `placeholder`; Bugwatch, Filebox and the four earlier refusals read as before | the `listings` expect on `ticketdesk-connector` (its summary is an `error`: profile `oauth` is unknown), class `product` |
| `company-address-is-entered-once-at-enable-and-every-host-is-filled` | new | process (mock providers) | as above, plus `org-administrator`, `mock-ticketdesk-server` | D30, T1. Enable without an address is 409 ("company address"); `acme.evil`, `-acme`, `a_b` and 64 letters are 400 ("one DNS label"); a client secret is 400 ("secret"); a member's Enable is 403. The administrator enables with `tenant { subdomain: acme }` and the client id: admin and member entries show `node.tenant.subdomain: acme`, `node.tenant.hosts.0: acme.ticketdesk.example`, `bindings.0.hosts.0`, and `node.client.authorizeEndpoint` and `tokenEndpoint` on `https://acme.ticketdesk.example/`; no field holds `{subdomain}`; the mock saw no request (no discovery). Disable, then `{ enabled: true }`: the address is kept | `marketplace-install` (refused: the connector does not parse), class `product` |
| `member-connects-a-company-address-connector-through-the-node-without-a-secret` | new | process (mock providers) | as the second | Phase 5 open question 2 (open question 1 below), T3. The member connects: the token endpoint at `host=acme` saw one `grant_type=authorization_code` with `client_id=e2e-ticketdesk-client`, `code_verifier`, `redirect_uri=squrell://oauth`, `client-secret=none`, `resource=none`; the tile reads `connected`. `via: tokens` is refused (400, "exchange"). The admin changes the address to `globex` (saving the client again): the member's tile reads `needsReconnect` and the mock saw nothing at `host=globex`. `secret-absent` for the tokens in every listing and event | `marketplace-install`; after waves 1–2, `connector-connect` (the route answers 400 "signs in with the app's own exchange"), class `product` |
| `skill-from-the-listing-reaches-only-the-company-host-inside-its-shape` | new | process (mock providers) | as the second (the agent comes from the listing) | D31, T2, the phase exit on a mock. The member chats with the installed agent: the read tool's request reached `host=acme` with `bearer=current` and no `bearer=placeholder`; the thread holds the mock ticket's subject; a call the agent aims at `globex.ticketdesk.example` and one at a path outside the shape are refused (the tool message holds `declared shape`) and the mock saw nothing at `host=globex`; `secret-absent` in thread, events, receipt and workspace | `marketplace-install`; after wave 3, the read tool (the swap answers "unknown connector"), class `product` |
| `company-address-token-is-refreshed-at-the-filled-endpoint-before-the-call` | new | process (mock providers) | as the fourth | Rotating refresh for a REST connector that is not Entra. `shortTokens: true`; connect; two tool calls: the token endpoint at `host=acme` saw two `grant_type=refresh_token` with `client_id`, `client-secret=none`, `resource=none`, `refresh=fresh`, and `refresh=reused` count 0 (the rotated token was stored before the access token was used); the API saw `bearer=current` both times | `marketplace-install`; then the first tool call, class `product` |

**Existing scenarios whose `then` changes: none.** Checked from `guards`
and `given`: the Filebox scenarios keep `org_vault` and the exchange with the
secret; `community-connector-appears-unverified-and-connects` still sees
`unknown-profile-connector` (profile `generic`) refused with "profile" and
`connector-with-a-secret` with "secret"; the Microsoft 365 scenarios keep the
Entra connection route and refresh shape. **Guards-only additions:**
`vam:crates/vam-orchestrator/src/connection_egress.rs` on
`skill-reads-microsoft-graph-as-the-member-without-holding-a-token` and
`tool-declared-as-a-read-cannot-send-mail`;
`vam:crates/vam-orchestrator/src/connector_exchange.rs` and
`vam:crates/vam-orchestrator/src/mcp_oauth_refresh.rs` on
`admin-supplied-client-lets-members-connect-without-the-secret-leaving-the-node`;
`vam:crates/vam-orchestrator/src/connectors.rs` on
`expired-microsoft-token-is-refreshed-before-the-call` and
`microsoft-365-connection-stays-in-the-consented-tenant`;
`vam:crates/vam-spec/src/skill_egress.rs` on
`listing-from-an-added-marketplace-installs-as-unverified-within-its-limits`.

##### Benchmark delta

| Line (`design/benchmark.md` §1) | Baseline on this base | Measured by | Expected after |
|---|---|---|---|
| P6 **new task** "a marketplace REST skill reads a company-address service as the member", 0 secret bytes in guest output | Not possible (no marketplace REST connector) | `secret-absent` in the fourth and fifth scenarios | 0 bytes, 100% |
| P9 **new task** "a marketplace skill aims its connection at another company's host or outside its shape" fails safely | Not possible | the fourth scenario | 100% refused; the provider saw nothing |
| P6 minutes and clicks from the admin's Enable (address and client id) to a member's first Zendesk read, real instance | Not possible | CN-6.7, by hand, team, owner's word | first number recorded |

Mock-measured in CI; the real-instance line only on the owner's word. The
orchestrator adds the two new tasks to `design/benchmark.md` §1 on approval
(docs only).

##### Items

**CN-6.1 The connector file: `profile: oauth`, `endpoints`, `tenant`, marketplace REST bindings, templated shape hosts, the summary.**
- **Scope.** `OAuthProfile::Oauth` (`oauth`) with `OAuth2Provider.endpoints:
  Option<Endpoints { authorize, token, revoke? }>` (required with `oauth`,
  refused with every other profile; `https`, port 443, no query, fragment or
  user info). `Connector.tenant: Option<TenantParam { name: "subdomain",
  suffix, label }>` (one parameter; `name` must be `subdomain`). `{subdomain}`
  may appear only as the whole first label of a host, and a templated host must
  be exactly `{subdomain}.<suffix>`: in `endpoints`, `mcp.url` and binding
  hosts; a template without `tenant`, or `tenant` with no template, is refused.
  The suffix: at least two labels, the DNS rules of `check_mcp_server_url`
  (`connector_listing.rs` 141-163), no internal suffix. Phase 6 slice: `oauth`
  needs `client: org` and `clientSecret: none` (open question 6).
  `check_marketplace_connector` and `parse_marketplace_connector`: allow
  `oauth`; allow `bindings` (1 to 8, each with 1 to 4 hosts, `value:
  access_token`, `inject: bearer`, placeholder exactly `vamsec_<id>_<name>` with
  `name` of `[a-z0-9_]{1,32}`, T4); MCP connectors stay `discover`; every
  binding or endpoint host is exact or `{subdomain}.<suffix>`; the MCP URL is
  checked on its filled form with a sample label. Skill egress
  (`skill_egress.rs`): `egress[].host` may be `{subdomain}.<dns name>` (syntax
  only; the node fills or drops it, CN-6.4); the marketplace import refuses a
  listing whose skill's templated host is not its connector's
  `{subdomain}.<suffix>`. `ConnectorSummary` gains `bindings [{ name, hosts }]`,
  `endpoints`, `tenant { label, suffix }`, `hosts` = the MCP host or the binding
  hosts (templated), and the text "company address: <label> (…<suffix>)"; the
  digest turns any change into "Enable again". The `import.rs` module doc
  (17-19) is corrected. Docs: the connector file authoring page, `status` kept.
- **Files.** `crates/vam-spec/src/{connector.rs,connector_listing.rs,package.rs,skill_egress.rs}`,
  `crates/vam-orchestrator/src/marketplace/{import.rs,summary.rs}`, unit tests,
  the connector authoring docs page.
- **Acceptance.** Unit tests per rule (each refusal names its field); the
  built-ins, Bugwatch and Filebox round-trip byte-stable; a Zendesk-shaped file
  (a vam-spec test fixture) validates; `{subdomain}` in a middle label, a
  one-label suffix, `*.` hosts, a built-in placeholder and `oauth` with
  `org_vault` are refused.
- **Tests.** `cargo test -p vam-spec`, `cargo test -p vam-orchestrator marketplace`, `scripts/ci/local.sh --fast --base origin/feat/connectors`.
- **Turns green.** `rest-listing-with-a-company-address-installs-and-shows-its-hosts`.
- **Decide.** `Decide: SLICE — proof 3 (D30, D31, D32: Zendesk cannot be described today), cost 2 (connector schema, marketplace trust rule, skill egress host form) → slice: oauth with an org client and no secret, bearer access-token bindings, one subdomain parameter; header/query inject, publisher client ids on oauth and secrets with fixed endpoints: backlog, trigger "a listing that needs one"`

**CN-6.2 Enable with a company address.**
- **Scope.** `PutNodeRequest.tenant: Option<Option<TenantRequest { subdomain
  }>>` (omitted keeps; `null` clears, only with `enabled: false`; 400 on a
  connector without `tenant`). Enabling a connector with `tenant` needs a
  stored or given address (409 "company address"). The subdomain is trimmed and
  lowercased, then must be one DNS label: 1-63 of `[a-z0-9-]`, no leading or
  trailing `-`, no `xn--` prefix (400 "one DNS label"; open question 3). Stored
  on the node row (migration: `tenant_subdomain` on `node_enabled_connectors`).
  One pure function `resolve(def, node) -> Connector` fills every
  `{subdomain}` and is the only way code reads a templated definition:
  `list_entry` (filled `bindings`, `mcp`, `provider.endpoints`, plus
  `node.tenant { subdomain, hosts }`), `policy_of` and `gate_mcp_write` (the MCP
  URL), Enable's discovery URL. A templated definition with no address
  resolves to nothing usable (no MCP policy, no binding). For `profile: oauth`
  with `client: org`, Enable with `client: { clientId }` pins `authorize`,
  `token` and `revoke` from the **filled file endpoints** (no discovery, no
  fetch; `issuer` = the token endpoint's origin); `org_connector` and
  `OrgConnector` stop requiring an MCP policy (a REST variant). A new address
  must come with the client saved again in the same body (409 otherwise: the
  client was registered in the old instance) and marks members' connections
  (REST and MCP) `needs_reconnect`; disable keeps the address (as consent,
  Phase 1 Q7). OpenAPI for the body and entry fields.
- **Files.** `crates/vam-orchestrator/src/{connectors.rs,connector_client.rs,api/connectors.rs}`,
  `connector_discovery.rs` (call site only), the store migration,
  `docs/api/openapi.yaml`, route tests.
- **Acceptance.** Route tests: each refusal writes nothing; the entry never
  holds `{subdomain}` once an address is set; a member sees the filled hosts
  once enabled; an address change flips members' rows and needs the client; no
  network request at Enable for `oauth`; the Filebox (discovery) path
  unchanged.
- **Tests.** `cargo test -p vam-orchestrator connectors`, `--scenarios company-address-is-entered-once-at-enable-and-every-host-is-filled`, `--fast`.
- **Turns green.** `company-address-is-entered-once-at-enable-and-every-host-is-filled`.
- **Decide.** `Decide: SLICE — proof 3 (D30), cost 2 (a body field, a migration, one resolver on every read) → slice: one subdomain per node, set at Enable; per-member addresses and a second parameter: cut, trigger "a provider whose members sit in different instances"`

**CN-6.3 The node exchanges the code for every admin-supplied client, and keeps REST connections for `profile: oauth`.**
- **Scope.** Open question 1's default. The exchange route accepts any
  enabled `client: org` connector with its client saved, secret or not (the
  `keeps_secret` filter at `api/connectors.rs` 459-467 goes). Without a secret,
  `exchange_authorization_code` takes `Option<&SecretString>` and sends no
  `client_secret` and no Authorization header; `resource` is sent for MCP
  connectors only (a `CodeGrant` profile mirroring `RefreshProfile`, so no code
  path can add `resource` to an `oauth` request). For an MCP connector the
  route writes the MCP row as today; for `profile: oauth` it writes the REST
  connection: the `connector_connections` row (the address it was made under,
  the filled token endpoint, no label: open question 9), the refresh token
  pinned to the token host, the access token pinned to **every** filled binding
  host. `PUT /v1/connectors/{id}/connection` stays Entra-only and refuses an
  `oauth` connector (400 "use the code exchange"); `PUT …/oauth/tokens` and an
  `oauth` block on `PUT /v1/mcp/connections` are refused for every `client:
  org` connector (O5 widened). Refresh: `RefreshProfile::Oauth { scope }` (no
  `resource`, no secret; `scope` only if the file lists scopes), sent to the
  node client's filled token endpoint with its client id; the refresher becomes
  a target the caller builds (Entra or Oauth), never a URL from a member's row;
  `refresh_connection` and `rest_connection_state` compare the row's address
  and token endpoint with the node's (else `needs_reconnect`, no request; T3);
  `POST …/connection/check` works for `oauth`. Disconnect deletes the rows and
  revokes only when the file declares `endpoints.revoke` (RFC 7009, no secret;
  open question 10). `runtime/tools.md` (OAuth section): the node also
  exchanges for an admin-supplied client without a secret.
- **Files.** `crates/vam-orchestrator/src/{api/connectors.rs,connector_exchange.rs,connector_client.rs,connectors.rs,mcp_oauth_refresh.rs,api/mcp_connections.rs}`,
  `docs/api/openapi.yaml`, `docs/src/content/docs/runtime/tools.md`, route and
  unit tests.
- **Acceptance.** Form tests: no `client_secret`, no Basic header and no
  `resource` for `oauth`; the Filebox form byte-identical; the Entra refresh
  form unchanged. Route tests against a loopback token server: nothing written
  on any refusal; the rotated refresh token stored before the access token; a
  row made under `acme` is never refreshed once the address is `globex`.
- **Tests.** `cargo test -p vam-orchestrator`, `--scenarios member-connects-a-company-address-connector-through-the-node-without-a-secret,admin-supplied-client-lets-members-connect-without-the-secret-leaving-the-node`, `--fast`.
- **Turns green.** `member-connects-a-company-address-connector-through-the-node-without-a-secret`.
- **Decide.** `Decide: SLICE — proof 3 (the exit needs a member connected, and the node cannot bind app-held tokens to the address without an id token), cost 2 (a widened route and a refresh profile) → slice: one exchange path for org clients, no account label, revoke only when RFC 7009 is declared`

**CN-6.4 The egress swap for installed connectors.**
- **Scope.** `SessionConnectionSource` is built from every enabled connector
  (built-in and installed), resolved with the node's address at session boot
  (`brokers.rs` 1087): bindings with filled hosts. `token_for` and
  `connector_id_of` read the session's resolved set, not
  `builtin_connectors()`; a binding whose connector has no address resolves to
  nothing (its placeholder stays reserved, so the request is refused, not
  forwarded). The boot step that records shapes fills a templated shape host
  from the required connector's address and drops a shape whose suffix is not
  that connector's. The access-token row check accepts any filled binding host
  (CN-6.3 pins the row to all of them). Refresh before swap and the intent park
  use the CN-6.3 target (`connector_park.rs` 155-156); single flight unchanged.
  A placeholder claimed by two resolved bindings refuses the swap for both (the
  backstop for T4). Process-guest seam: vamd accepts `rest:<https origin>` keys
  and the gate answers those origins from loopback (as the Graph key,
  `brokers.rs` 1133-1138); the runner turns `PASS_REST_SEAM` on.
- **Files.** `crates/vam-orchestrator/src/{connection_egress.rs,brokers.rs,connector_park.rs,manager.rs (shape record),process_mode.rs}`,
  `crates/vam-e2e/src/connectors.rs` (switch on), unit tests.
- **Acceptance.** Unit tests: an installed binding attaches only on its filled
  host and inside the filled shape; another company's host and an unfilled
  template are refused before any token read; a disabled installed connector
  attaches nothing; the built-in Graph tests unchanged; two requests at once
  make one refresh for an `oauth` connection.
- **Tests.** `cargo test -p vam-orchestrator connection_egress connector_park process_mode`, `--scenarios skill-from-the-listing-reaches-only-the-company-host-inside-its-shape,company-address-token-is-refreshed-at-the-filled-endpoint-before-the-call,skill-reads-microsoft-graph-as-the-member-without-holding-a-token,tool-declared-as-a-read-cannot-send-mail`, `--fast`.
- **Turns green.** `skill-from-the-listing-reaches-only-the-company-host-inside-its-shape`, `company-address-token-is-refreshed-at-the-filled-endpoint-before-the-call` (with CN-6.5).
- **Decide.** `Decide: NOW — proof 3 (P1, P6: without it a marketplace binding is parked on and never attached; T2 is the security floor), cost 1 (files Phase 2 built, new tests, no new surface)`

**CN-6.5 A skill learns its company host.**
- **Scope.** Open question 7's default: when a session's skills are
  materialized (`manager.rs`, inside `vam_storage::with_skills_writable`,
  before the skills digest), the node writes `.vam/connector.json` into each
  skill directory whose manifest's `requiresConnector` names an enabled
  connector with an address: `{ "connector": "<id>", "hosts": { "<binding>":
  "<filled host>" } }`. Not a secret (T5); no guest code changes (the tool
  reads a file next to its manifest). The e2e fixture skill and the Zendesk
  skill read it. The skill authoring docs page says where the file is.
- **Files.** `crates/vam-orchestrator/src/manager.rs` (or a small module it
  calls), unit tests, the skill authoring docs page.
- **Acceptance.** Unit test: the file appears only for skills that require a
  connector with an address, holds no token or vault id, and a skill without
  `requiresConnector` is unchanged; the microVM lane green (the skills drive
  changes).
- **Tests.** `cargo test -p vam-orchestrator manager`, the fourth scenario, `--fast`; the lane on the PR.
- **Turns green.** with CN-6.4.
- **Decide.** `Decide: NOW — proof 3 (a skill cannot address the company's host otherwise), cost 1 (one host-written file at boot, no guest code)`

**CN-6.6 the VAM app.**
- **Scope.** Org settings → Connectors, a connector with `tenant`: Enable asks
  for the company address as `[ acme ].<suffix>` under the file's label (the
  suffix fixed), lowercases it and checks one DNS label before sending; for
  `client: org` with `clientSecret: none`, the client id only (no secret
  field). After save the card shows `node.tenant.hosts` and the filled
  endpoints ("Squrell will reach acme.zendesk.com"); "Change address" warns that
  members reconnect and asks for the client id again. The Marketplace summary
  shows "company address: <label> (…<suffix>)" and the templated hosts. Member
  connect: for **every** `client: org` connector the app builds the authorize
  URL from `node.client.authorizeEndpoint` and the client id, PKCE S256, and
  posts code, verifier and redirect URI to the node's exchange route (the
  app-side exchange for org clients goes; the `Idempotency-Key` reuse from
  Squrell-App #13 stays).
- **Files.** `src/components/vam/{OrgSettingsConnectorsWorkspace.tsx,ConnectorTiles.tsx}`,
  the MCP OAuth flow module, `src/types/` connector types, the Marketplace
  summary, `src-tauri/src/{vamd.rs,lib.rs}`, tests.
- **Acceptance.** Unit tests on fixtures: the address field refuses a dot or an
  underscore; the authorize URL comes from the entry only; the app sends no
  token request for any `client: org` connector.
- **Tests.** `CI=1 pnpm test`, `pnpm exec tsc --noEmit`, `cargo test --manifest-path src-tauri/Cargo.toml`.
- **Turns green.** None (unit; team checklist).
- **Decide.** `Decide: SLICE — proof 3 (no admin can enter an address without it), cost 2 (two screens and a flow change) → slice: address, client id, filled hosts, one connect path; a live preview of the hosts before save: backlog`

**CN-6.7 The Zendesk listing, and the team's run.**
- **Scope.** On `squrell-inc/vam-marketplace`, branch `zendesk-connector`
  (from `main`): `listings/zendesk/` with `connector.json` (`profile: oauth`;
  `endpoints.authorize`
  `https://{subdomain}.zendesk.com/oauth/authorizations/new`,
  `endpoints.token` `https://{subdomain}.zendesk.com/oauth/tokens`; `tenant {
  name: subdomain, suffix: zendesk.com, label: "Zendesk subdomain" }`;
  `client: org`, `clientSecret: none`; scopes from Zendesk's docs, read first;
  binding `api` / `vamsec_zendesk_api` on `{subdomain}.zendesk.com`), a skill
  `zendesk-tickets` (Bun TypeScript on the m365 pattern, reads
  `.vam/connector.json`: search tickets, show a ticket, create a ticket with
  approval by exact path; open question 8), a support assistant agent, Bun
  tests, and `docs/listing-zendesk.md` (create the OAuth client in Admin Center
  with PKCE and the redirect `squrell://oauth`, Enable, connect). `requiresNode`
  = the first node version with Phase 6; CI's pinned vam commit moved to the
  Phase 6 merge so `vam catalog validate` passes; the owner tags
  `zendesk-v1.0.0-rc.1`; a staging node imports it with
  `VAMD_SQURELL_MARKETPLACE_INDEX_REF=zendesk-connector` (CN-5.6). Team run on a
  real Zendesk instance, owner's word: Enable with the subdomain and client id,
  member connects, the assistant answers from a ticket, a refresh after expiry
  works, another subdomain is refused, P6 number recorded. Zendesk facts (PKCE
  for a local client, refresh-token rotation and lifetimes, scope names) are
  checked against Zendesk's primary docs before the tag (design §13.2 is
  research).
- **Files.** vam-marketplace: `listings/zendesk/**`, `marketplace.json` (the
  branch index), `.github/workflows/validate.yml`, `docs/listing-zendesk.md`.
  vam: the Phase 6 close block (runbook, team checklist).
- **Acceptance.** Marketplace CI green on the branch; the team's run passes or
  its failures land as one fix wave.
- **Tests.** Marketplace CI; the runbook steps.
- **Turns green.** None (real instance).
- **Decide.** `Decide: NOW — proof 3 (exit criterion), cost 1 (data, a skill on the m365 pattern, a runbook)`

**CN-6.8 (candidate) Salesforce `instance_url`.**
- **Scope (no wave).** Salesforce returns `instance_url` in the token
  response, and REST goes there. Within D30 it fits as the company address:
  `tenant { suffix: my.salesforce.com }` with the admin's My Domain label; at
  the exchange and every refresh the node requires `instance_url` to equal
  `https://<filled host>`, else it refuses and stores nothing. No per-member
  host, no host learned from a token response. Sandboxes
  (`*.sandbox.my.salesforce.com`) need a second suffix: cut.
- **Decide.** `Decide: BACKLOG — proof 1 (D32 names Zendesk; no Salesforce listing is planned), cost 1 (one check on files this phase touches); trigger "a Salesforce listing is scheduled"`

##### Waves

One wave at a time; branches `cn-6/wave-N` into `feat/connectors` (open
question 14). Wave 1 is the scaffold later waves share (the connector file and
the summary). The VM-path flag comes from the files each wave touches
(`vam-runtime`, `vam-core`, `vam-vsock`, `vam-storage`, `vam-firecracker`, or
what the guest reads from the skills drive): only wave 4 changes what a guest
reads.

| Wave | Items | Turns green | Repo | Model | Budget | Depends on | CI | VM path | Status | PR |
|---|---|---|---|---|---|---|---|---|---|---|
| 0 | scenarios: 5 files, `mock-ticketdesk-server` (fixed endpoints, public-client token endpoint, rotating refresh, REST API per company, `shortTokens`), the listing fixture (connector, skill, agent) and two refused listings, `connector-node.tenant`, ticketdesk `connector-connect` and `via: tokens`, the `rest:` seam key held out, README "Connectors" additions, guards-only edits | — (5 red, right reason, class `product`) | vam | Sonnet low | 120 min | — | `CI: scenarios=rest-listing-with-a-company-address-installs-and-shows-its-hosts,company-address-is-entered-once-at-enable-and-every-host-is-filled,member-connects-a-company-address-connector-through-the-node-without-a-secret,skill-from-the-listing-reaches-only-the-company-host-inside-its-shape,company-address-token-is-refreshed-at-the-filled-endpoint-before-the-call lane=no release-build=no` | no | [ ] | |
| 1 | CN-6.1 | first | vam | Sonnet medium: changes the connector file schema, the marketplace trust check (a community connector may now have tokens attached to guest requests) and the skill egress rules in `vam-spec` | 120 min | 0 | `CI: scenarios=rest-listing-with-a-company-address-installs-and-shows-its-hosts,listing-connector-with-an-admin-supplied-client-installs-and-waits-for-its-client,community-connector-appears-unverified-and-connects,listing-from-an-added-marketplace-installs-as-unverified-within-its-limits tests=workspace lane=no release-build=no` | no | [ ] | |
| 2 | CN-6.2 | second | vam | Sonnet medium: a new API body field, a migration, and one resolver on every read of a definition across three modules | 120 min | 1 | `CI: scenarios=company-address-is-entered-once-at-enable-and-every-host-is-filled,admin-supplied-client-is-set-only-by-an-org-admin-and-never-read-back,connectors-reach-members-only-after-an-org-admin-enables-them lane=no release-build=no` | no | [ ] | |
| 3 | CN-6.3 | third | vam | **Sonnet high**: the code exchange and refresh for a new profile (where codes and refresh tokens are sent, T3) and refusals on three connection routes (auth). Orchestrator reads the token forms and the endpoint source before merge | 150 min | 2 | `CI: scenarios=member-connects-a-company-address-connector-through-the-node-without-a-secret,admin-supplied-client-lets-members-connect-without-the-secret-leaving-the-node,rotating-refresh-token-is-spent-once-when-two-tasks-call-at-once,expired-microsoft-token-is-refreshed-before-the-call,microsoft-365-connection-stays-in-the-consented-tenant lane=no release-build=no` | no | [ ] | |
| 4 | CN-6.4, CN-6.5 | fourth, fifth | vam | **Sonnet high**: the egress swap that attaches a member's token to guest requests, now for marketplace connectors (T2, T4), and a host-written file in the skills drive. Orchestrator reads `token_for`, the shape fill and the placeholder check | 180 min | 3 | `CI: scenarios=skill-from-the-listing-reaches-only-the-company-host-inside-its-shape,company-address-token-is-refreshed-at-the-filled-endpoint-before-the-call,skill-reads-microsoft-graph-as-the-member-without-holding-a-token,tool-declared-as-a-read-cannot-send-mail,skill-needing-microsoft-365-asks-the-member-to-connect-first lane=yes release-build=no` | **yes** (skills drive) | [ ] | |
| 5 | CN-6.6 | — (unit) | The VAM app (`squrell-inc/Squrell-App`, base `feature/connectors`, re-cut from the app's `main` if gone) | Sonnet medium: two admin screens and the sign-in flow of every admin-supplied client change; no secret passes through the app here | 120 min | 4 | none (app unit only) | no | [ ] | |
| 6 | CN-6.7 | — (marketplace CI; real instance) | vam-marketplace (+ vam close block) | Sonnet low (the session supports the team) | 60 min + the team's time | 5; a deploy of vamd and the app from the base | marketplace: its own CI; vam: none | no | [ ] | |

Budget: 870 min of agent time (14.5 h) plus the team's run. Scoreboard
expectation: 0 green after wave 0; 1 after wave 1; 2 after wave 2; 3 after
wave 3; 5 after wave 4; flat in 5–6 (stated so they are not flagged).

**Phase rubric.** `Decide: SLICE — proof 3 (D30, D31, D32: the exit), cost 3 (a new sign-in profile, marketplace REST trust, the swap for installed connectors, three repos) → slice: oauth with an org client and no secret; one subdomain at Enable; bearer access-token bindings; the node's exchange for org clients; a host-written hosts file; reads plus one exact-path create in the Zendesk skill. Backlog: secrets with fixed endpoints (with the admin's token auth method), header/query inject, account labels, path templates in shapes, Salesforce instance_url (CN-6.8)`

##### Open questions (each with a default)

1. **Phase 5 open question 2 (deferred here): does the code exchange for a
   secret-less org client move to `vamd`?** Default **yes, for every
   `client: org` connector, MCP or REST** (CN-6.3). Reasons from the code:
   (a) the only app-token route for REST, `PUT /v1/connectors/{id}/connection`,
   trusts tokens because an Entra id token's `tid` equals the recorded consent
   (`connectors.rs` 814-870); Zendesk returns no id token, so the node could not
   tell tokens from `acme.zendesk.com` from tokens of any other instance, and T3
   would rest on the app. An exchange at the endpoint the node filled is the
   proof. (b) Everything the exchange needs exists: pinned endpoints on the
   node row, single-spend idempotency (`claim_code_exchange`), the redirect-URI
   check (`check_code_exchange`), the DNS-pinned, no-redirect token client; the
   change is the `keeps_secret` filter (`api/connectors.rs` 459-467) and a form
   without a secret. (c) Refresh is the node's anyway (`refresh_connection`),
   so the app never needs the token endpoint. (d) One app path for every org
   client removes the request-borne token endpoint and client id that O3
   polices on `PUT /v1/mcp/connections`. Cost: `vamd` sees a public client's
   single-use code and verifier for a moment (`runtime/tools.md` amended
   again). Alternative: keep D29 as written (the app exchanges without a
   secret) and add an `oauth` variant of `PUT …/connection`, with no way to bind
   the tokens to the address. Taking the default **edits locked decision 13**
   on approval: "the node exchanges the code for every admin-supplied client;
   without a secret it sends none".
2. **Names.** File: top-level `tenant: { name: "subdomain", suffix, label }`
   and `provider.endpoints { authorize, token, revoke? }` (design §13.3).
   Wire: `PUT …/node` body `tenant: { subdomain }`; entry `node.tenant {
   subdomain, hosts }`. `consent.tenantId` (Entra) is another object and keeps
   its name. Alternative: `companyAddress` on the wire, to avoid "tenant" twice.
3. **Tenant validation.** Default: the admin's value is trimmed and
   lowercased, then one DNS label: 1-63 of `[a-z0-9-]`, no leading or trailing
   `-`, no `xn--` prefix (no look-alikes), else 400. The suffix is fixed per
   connector by the file (at least two labels, public DNS rules, no internal
   suffix, not editable at Enable); `{subdomain}` only as the whole first label;
   every templated host is exactly `{subdomain}.<suffix>`. No public-suffix
   list: the admin sees the filled hosts before members use them, and the
   two-label minimum stops `{subdomain}.com`. The node does not probe the filled
   host at Enable (Zendesk publishes no metadata; the first member sign-in is
   the check).
4. **Changing the address.** Default: the client id must be saved again in the
   same body (a client lives in one instance); every member's connection reads
   `needs_reconnect`; an old refresh token is never sent to the new host.
   Disable keeps the address.
5. **Salesforce `instance_url`.** Default **defer** (CN-6.8, backlog). When it
   comes, it is a check (the token response's `instance_url` must equal the
   filled My Domain host), never a host learned from a response.
6. **Secrets with fixed endpoints, and the admin's token auth method (Phase 5
   close backlog, CodeRabbit thread 4619996438 on #52).** Default: **does not
   land here.** Phase 6's `oauth` profile allows `clientSecret: none` only
   (Zendesk and Salesforce need none), so no auth method is chosen; discovery
   keeps deciding for `discover` (Box). Trigger: the first fixed-endpoint
   provider that needs a secret; then the method is declared in the file
   (`endpoints.tokenAuthMethod`), not chosen by the admin.
   `Decide: BACKLOG — proof 1, cost 1` (unchanged).
7. **How a skill learns its company host.** Nothing carries a per-node value
   to a tool today (`exec.rs` clears the environment; `env_vars` is never
   applied). Default **(a)** CN-6.5: the node writes `.vam/connector.json` next
   to the skill's manifest at boot (host-side, no guest code; the lane runs).
   (b) The gate fills a literal `{subdomain}` in the guest's target URL before
   the envelope screens (no file, but a rewrite inside the security gate).
   (c) The runtime applies a host-supplied environment variable (a guest
   change, cost 3).
8. **Zendesk writes.** Exact `paths` cannot name `/api/v2/tickets/{id}.json`,
   and a write by prefix publishes with a warning (`skill_egress.rs` 15-18).
   Default: the first Zendesk skill reads (search, show) and creates a ticket
   (`POST` exact `/api/v2/tickets.json`); updates and comments wait for one
   `{id}` segment in exact paths (`Decide: BACKLOG — proof 1, cost 1`, trigger
   "the team's run asks for ticket updates").
9. **Account label for an `oauth` connection.** Default: none in Phase 6 (the
   tile says "Connected"); a label read from a declared path with the member's
   token is backlog (`Decide: BACKLOG — proof 1, cost 1`).
10. **Revoke on disconnect.** Default: only when the file declares
    `endpoints.revoke` (RFC 7009, no secret). Zendesk's token delete API is not
    RFC 7009, so the rows are deleted and the grant expires at Zendesk.
11. **Placeholders.** Default: a marketplace binding's placeholder must be
    `vamsec_<connector id>_<binding name>` (T4), unique by construction since
    the import already refuses a taken connector id; any other value is refused
    at install.
12. **MCP URL templating.** Default: in scope through the resolver (CN-6.2),
    proved by unit tests only (no listing uses it yet). Alternative: refuse
    `{subdomain}` in `mcp.url` until a provider needs it.
13. **Where the Zendesk listing lives.** Default: `vam-marketplace` branch
    `zendesk-connector` under Squrell's URN (as Box), imported on a staging
    node through CN-5.6's index ref; merged into `main` and tagged
    `zendesk-v1.0.0` after the Phase 6 release (locked 16's pattern).
14. **Base branch.** Default: `feat/connectors` (fast-forwarded to `main` at
    5d020be); waves 0–4 merge there and the app wave into the app's
    `feature/connectors`; released to `main` at the phase close.

**Risks.**

- Zendesk's OAuth facts (PKCE for local clients, refresh rotation, scopes) are
  research (design §13.2); wave 6 checks them before the tag. The scenarios do
  not depend on them.
- The swap's definition set moves from a compiled list to the store (CN-6.4):
  a slip there attaches a token for a connector the admin disabled. The
  orchestrator reads the enabled and address checks in `token_for` before merge.
- A per-node address makes the skills digest per node (CN-6.5), so golden
  snapshots keyed on it are shared less. Recorded, not fixed.

**Design vs code** (the code is the fact; the last column says what moves).

| Design says | Code does | Moves |
|---|---|---|
| §4 `McpServer.url` "may contain {tenant}" | No templating: `validate_connector` wants a plain `https` URL (`connector.rs` 351-354), the marketplace a full DNS name (`connector_listing.rs` 110-165). §13.1 admits it; §4 names the parameter `{tenant}`, §13.3 and D30 `{subdomain}` | design §4 to `{subdomain}` (docs, at the close) |
| §4 `OAuth2Provider.profile` without `oauth`; `clientSecret: none \| squrell_relay` | `client` and `org_vault` exist (`connector.rs` 65-122); no `oauth`, no `endpoints`, `deny_unknown_fields` (63-64) | code (CN-6.1); design §4 refreshed at the close |
| §5 the swap "finds the session owner's connection for that connector" | Built-in connectors only: `builtin_bindings()` (`connection_egress.rs` 44), `token_for` (407), `connector_id_of` (586-595), built at `brokers.rs` 1087 | code (CN-6.4) |
| §4 `inject: "bearer" \| "header" \| "query"`; §5 Zendesk in "Swap" mode | Only `bearer` with `access_token` attaches (`connection_egress.rs` 419-423) | the slice; header/query backlog |
| §4 binding `hosts: string[]` | The access token is pinned to the first host of the first binding (`connectors.rs` 850-857, 1146-1152) | code (CN-6.3) |
| §13.3 "after Enable every host is exact … token routing unchanged"; D29 "without a secret the app keeps exchanging" | REST connections are Entra-only (`connectors.rs` 737-754, 814-870; token host fixed at 863 and 1140; `RefreshProfile` Mcp/Entra, `mcp_oauth_refresh.rs` 116-123); the org client is MCP-only (`connector_client.rs` 938-963; discovery from `def.mcp.url`, `api/connectors.rs` 181-190); the exchange always sends `resource` and needs a secret (`mcp_oauth_refresh.rs` 525-560) | code (CN-6.2, CN-6.3); locked 13 edited if open question 1's default is taken |
| §13.3 `{subdomain}` in "endpoints, `mcp.url` and binding hosts" | Skills name hosts too, and `egress[].host` must be an exact DNS name (`skill_egress.rs` 45-49, 80-89): a Zendesk skill cannot declare its host. The design is silent | design §13.3 adds shape hosts; code (CN-6.1, CN-6.4) |
| (silent) how skill code learns the host | No per-node value reaches a tool: `exec.rs` 543-605; `env_vars` parsed (`skills.rs` 380-409), never applied | design gains a line (open question 7); code (CN-6.5) |
| §11 Q8 exact paths for writes | An exact path has no segment template, so `/api/v2/tickets/{id}.json` cannot be a declared write (`skill_egress.rs` 32-43) | open question 8 (backlog) |
| `marketplace/import.rs` module doc (17-19): a marketplace connector is "remote MCP with discovery, no secret" | Since Phase 5 `org_vault` is allowed (`connector_listing.rs` 11, 303-313) | doc comment fixed in wave 1 |
| `ConnectorSummary.hosts` "the MCP server's" (`package.rs` 564-565) | Right today, wrong once REST listings exist | code (CN-6.1) |

**Phase close:** all five scenarios `active` and green; the VAM app unit
suites on the merged base; Zendesk marketplace CI green on its branch; the
team's real-instance run on the owner's word with the P6 number; release (the
release PR runs the microVM lane once); team checklist out; tag
`zendesk-v1.0.0` and the listing merged into `vam-marketplace` `main` after the
release.

### Phase 7 — Developer kit and API keys

**Goal:** a third-party developer builds, checks and tests a connector without
a Squrell release; API-token systems can be connected.
**Depends on:** Phase 6.
**Status:** defined 2026-11-18 (owner); not started.
**Scope:** `vam connector init`, `validate`, `test` (mock provider world
piece, also used by marketplace CI); the `api_key` connection flow (admin- or
member-entered key, `connection` row, host-side inject only).
**Exit:** a connector built from nothing passes `validate` and `test` and
installs from a test marketplace.

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
| 2026-11-07 | — | Tracker opened from `design/connectors.md` (design session, branch `claude/elegant-hamilton-xg79jo`). |
| 2026-11-07 | 0 | **Phase 0 done.** DX-139 merged to `main` as : reserved `connection` vault kind, broker, HTTP-adapter and notify-token refusals; three process-mode scenarios; Actions 5/5 green incl. the microVM lane. Microsoft paperwork carried as owner items. Next: Phase 1 breakdown from `main`. |
| 2026-11-07 | 1 | Breakdown drafted (5 scenarios, CN-1.1–1.8, waves 0–7), awaiting owner approval. Then checked against (connectors in 0.1.0, rows 18–21; egress envelope in Phase 2; OAuth amendment applied): phase rubric proof 2 → 3, Phase 2 dependency met, open question 12 added. No wave or scope change. |
| 2026-11-07 | 1 | **Breakdown approved** (owner) with recommendation 1 and open question 12's default; landed on `feat/connectors`. Wave 0 started. |
| 2026-11-07 | 1 | **Standing approval for Phase 1 (owner):** the orchestrator auto-approves its own wave recommendations and runs waves 0–7 without waiting; only very tricky or high-risk decisions go to the owner, with a notification. Push failures on signing: try the GitHub (HTTPS/API) route, else keep local commits and continue. Actions only for the microVM or benchmark lane; everything else runs in the session. Wave 0 plan approved with: no mock-endpoint env until wave 2 (flag flipped there), guards-only edits deferred if `check` rejects missing paths, `refused` = 400/403/409, `connectors` tag. |
| 2026-11-07 | 1 | **Wave 0 merged** (squash into `feat/connectors`, `[skip ci]`). Scoreboard: 5 scenarios red (right reason, class product, at when.0), 0 red wrong reason, 99 others pass; 0 fix cycles; two container restarts mid-build (work survived on disk; parallelism capped at 2 jobs after). Known local failure, not from this wave: `hsp_uds::uds_socket_acl_deny_fails_fast` fails when the session runs as root. Remote branch delete refused by the git proxy (`cn-1-w0` left on origin). |
| 2026-11-07 | 1 | **Wave 1 merged**: connector definition and built-ins in vam-spec, placeholder client id `PLACEHOLDER-squrell-entra-client-id`, plus a rule `mcp.url` must be `https://`. Scoreboard: 0 scenarios moved (unit wave, expected), 5 red right reason, 0 fix cycles; orchestrator re-ran vam-spec tests (274 pass) and fmt. Wave 2 started. |
| 2026-11-08 | 1 | **Wave 2 merged**: `GET /v1/connectors`, `PUT /v1/connectors/{id}/node` (users:admin), `node_enabled_connectors`, client-id config, process-only endpoint seam (release gate proves the default vamd lacks it), CLI list/enable/disable. Spec addition (orchestrator, standing approval): `consent: null` clears the consent record; runner cleanup restores node state because scenarios share one vamd. `expectFail` of C and D moved to when.1 (no `then` change). Scoreboard: A and B green (active); C, D red right reason at when.1, E at when.0; 1 fix cycle (scenario isolation). Disk ran out mid-wave (target 28 GB): `cargo clean` per the ladder, 29 GB freed. Third container restart during a full run; known local failures recorded: `hsp_uds` ACL test (root), `effect_journal` flaky under load. Wave 3 started. |
| 2026-11-08 | 1 | **Wave 3 merged**: Linear as an MCP connector (gate on enable and definition URL, vetted tools at the definition's classes, disabled rows not seeded, caller state from the row, trigger-started sessions seed the grant user's tools). Spec change (orchestrator, observation only): D observes `run: failed` + `mock.unmatched` `reason: tool_not_offered` instead of an "unknown tool" message, since the mock model cannot call an unoffered tool. Scoreboard: D and E green (active); A, B green; C red right reason at when.1; 1 fix cycle. Wave 4 started. |
| 2026-11-08 | 1 | **Wave 4 merged**: Microsoft 365 connection (tenant gate before write, connection-kind vault rows, fixed consent-tenant token endpoint, Entra refresh with no `resource` and no secret, check/disconnect, CLI). Scoreboard: C green (active): **all 5 Phase 1 scenarios active and green**; `--fast` 104/104; workspace suite green but the known root-only `hsp_uds`; release gate PASS; 0 fix cycles. Fourth container restart, again at the smoke step of a `--full` run; the split run had memory under 1 GB, so memory is not the cause. Note for wave 7: real id tokens may lack `preferred_username` (no `profile` scope); label falls back to `upn`/`email`. vam side of Phase 1 done; wave 5 (the VAM app) started. |
| 2026-11-08 | 1 | **Wave 5 merged**: Org settings "Connectors" (users:admin), Enable/Disable, Microsoft admin consent via `adminconsent` (no `prompt`/`resource`, state checked, tenant GUID) with manual fallback, five Tauri commands. Orchestrator re-run: pnpm test 805/805, tsc clean, src-tauri 58/58 (GTK/WebKit dev packages installed in the session). Consent actions show after Enable (listing carries `provider` only once enabled). 0 fix cycles. Wave 6 started. |
| 2026-11-08 | 1 | **Wave 6 merged**: member Connections tiles (§3.1 states, Connect/Reconnect/Disconnect, nothing secret shown), Linear through the existing MCP OAuth flow with the fixed id and URL, Microsoft 365 through the Entra profile (discovery skipped, no `resource`/`prompt`, `tid` and `iss` checked before vamd, tokens cleared). Orchestrator re-run: pnpm test 818/818, tsc clean, src-tauri 58/58. 0 fix cycles. **Waves 0–6 done; all 5 scenarios active and green.** Wave 7 (real tenant) blocked on the owner items. Team checklist written (Linear steps runnable now). |
| 2026-11-08 | 1 | **Owner registered the Squrell Entra app** (multi-tenant, public client, `squrell://oauth` + `http://localhost`, 8 delegated Graph scopes incl. `profile`, `email`; admin consent not granted in the portal so the app's flow is tested). **Wave 7 code merged**: real client id `082509e4-…` in `microsoft-365.json`, scopes + `profile`/`email`; tests for an unconfigured node set the placeholder explicitly (no unit test can reach Microsoft). 5/5 connector scenarios pass, workspace lib tests green. Pending: owner's manual run of checklist steps 1, 6, 7 on the owner's test tenant and the P6 numbers. Publisher verification still open (needed for other orgs' tenants, not for this test). |
| 2026-11-08 | 1 | **Released to `main`** on the owner's word: (merge commit; Actions in full once: checks, workspace tests, smoke scenarios, microVM lane all green) and (merge commit; test green). Bases fast-forwarded to `main`. **Phase 1 scoreboard:** 5 scenarios added, 5 active and green; waves 0–7 (wave 7 code only), 3 fix cycles in total (W2 scenario isolation, W3 observation spec change, W7 test config); Actions used only by the release PR. Status: released, checklist out. Open: team checklist (incl. Microsoft steps 6–7 on the owner's test tenant, P6 minutes and clicks), publisher verification, owner's one-line note in `docs/src/content/docs/runtime/tools.md` (Q12). A checklist failure becomes a fix wave on `feat/connectors` / `feature/connectors`. |
| 2026-11-08 | 2 | **Breakdown drafted** (phase-breakdown agent,), awaiting owner approval: 5 scenarios (A–E), CN-2.1–2.8, waves 0–5 (W2 and W3 Sonnet high: token attach, envelope, refresh), 13 open questions with defaults. Shared surfaces with the Phase 3 lane named (`skills.proto`, `validation.rs`, `session.rs`, `security.rs`/`hitl.rs`, the e2e runner, app thread cards); all Phase 2 proto and validation edits in the small W1 scaffold, HOLD rule per wave, next free field numbers at rebase. |
| 2026-11-08 | 2 | **Breakdown approved** (owner) with recommendation 1 (open question 1's default; the background-process residual risk recorded as a finding) and recommendation 2 (clarifying line under locked decision 7); other defaults accepted. Wave 0 started. |
| 2026-11-08 | 2 | **Wave 0 merged** (squash into `feat/connectors`, `[skip ci]` + `skip-vm-lane`): 5 scenarios pending, mock Graph, probe skill (`files:` on skill pieces), runner switches `PASS_SKILL_CONNECTOR_FIELDS` / `PASS_GRAPH_ENDPOINT` (false). Orchestrator re-run: `--fast` all steps pass, 109 scenarios: 104 pass, 0 fail, 5 red right reason (A, B, C at when.4; D at when.6; E at when.2; class product); vam-e2e unit 243/243. Wave session `--full`: only the known root-only `hsp_uds` fails. Scoreboard: 0 green, 5 red right reason, 0 fix cycles, ~100 min. Local note: as root, `LOCAL_CI_OUT` must be `mkdir -m 755` (tool user reads fixtures). Phase 3 wave 0 released to start. Wave 1 next. |
| 2026-11-08 | 2 | **Wave 1 plan approved** (owner) with the orchestrator's recommendations: empty `path_prefixes` refused (whole host = "/"); `..Default::default()` at each `ToolSpec` literal (about 50 sites in 20 files) so Phase 3's `ToolSpec.ui` needs no second sweep; budget 90 min; new `ToolEgress` message and `pub(crate)` URN/host checks in `connector.rs` accepted. **Standing approval for Phase 2 (owner, 2026-11-08):** the orchestrator approves its own wave recommendations and runs all waves without waiting; only very tricky or high-risk decisions go to the owner, with a notification. Signing push failures: try the GitHub route, else keep local commits and continue. Actions only for the microVM (Firecracker) or benchmark lane when required; everything else runs in the session. |
| 2026-11-08 | 2 | **Wave 1 merged** (squash, `skip-vm-lane`): `ToolEgress`, `ToolSpec.egress = 14`, `SkillManifest.requires_connector = 12`, rules in `crates/vam-spec/src/skill_egress.rs` (empty `path_prefixes` refused), switch `PASS_SKILL_CONNECTOR_FIELDS` on. No `ToolSpec` literal needed a change; five `SkillManifest` literals got `..Default::default()`. Orchestrator re-run on the merged state with Phase 3 W0 : `--fast` all pass, 113 scenarios: 104 pass, 9 red right reason (Phase 2 A–E unchanged; Phase 3 four at when.0); workspace 3958 pass, 1 known root-only `hsp_uds`. Scoreboard: 0 green (scaffold, expected), 5 red right reason, 0 fix cycles. Two container restarts; the wave session and the orchestrator ran full builds at once and filled the disk (0 MB free): ladder steps 1–3 (incremental, 70 old test binaries, `cargo clean` 23 GB), 30 GB free after. **Rule from here:** one full build at a time in the session; the orchestrator re-runs only after the wave session reports. Phase 3 told: `skills.proto`/`validation.rs` free, take `ToolSpec.ui = 15`. Wave 2 next. |
| 2026-11-08 | 2 | **Wave 2 merged** (squash, `skip-vm-lane`): connection bindings and the per-call egress envelope. New `vam-network/src/connection.rs` (pure `Envelope::screen` before DNS: one placeholder, only as the exact `Authorization` value; https, port 443, no userinfo, canonical URL; host exact; method; path decoded once per segment, dot segments / encoded slash / double encoding refused; binding and `requiresConnector` URN) and `vam-orchestrator/src/connection_egress.rs` (token only from the member's own connection-kind row, connector enabled, `Connected`, refresh token present, host in the binding; window re-checked at attach). Call window opened on an AUTHORIZED port-52 intent, cleared on the next intent, `CommitTurn`, session end, the next inference request, or tool timeout + 30 s. Shapes in side table `session_tool_egress` (not `ToolCatalogEntry`). reqwest redirects off. Test redirect `UpstreamRedirect` compiled only with `vam-network/process-guest`; release gate PASS (marker absent from the default vamd). `PASS_GRAPH_ENDPOINT` on. **A and B active and green.** Spec changes (orchestrator, observation only): `egress.decided` written under the run id; B reads `data.disposition`; one mock rule for the post-approval user message (class scenario fix). Orchestrator read `connection.rs`, `connection_egress.rs` attach/token_for, the gate wiring; re-run (merged with Phase 3 W1): `--fast` 113 scenarios: 106 pass, 7 red right reason (C at when.4, D at when.6, E at when.2, Phase 3 four at when.0); vam-network (process-guest) + vam-orchestrator lib 1606 pass, 0 fail; release gate PASS. Wave session: workspace 3991 pass, 1 known root-only `hsp_uds`. **Findings (recorded, not fixed):** a background process left by a read tool can ride a later window of the same session (fix: per-call placeholder, needs a guest change); port 55 does not merge repeated `Authorization` lines; large port-55 responses stream unscrubbed (pre-existing); legacy-policy sessions cannot reach Graph; prefix `/` admits the whole host. Scoreboard: 2 green (A, B), 3 red right reason, 0 fix cycles, ~180 min. |
| 2026-11-08 | 2 | **Wave 3 merged** (squash, `skip-vm-lane`): refresh before swap. `attach` refreshes when `expires_at <= now + 120 s`, after the envelope, destination and connection-state checks and before the header is set, then re-checks the call window. Single flight on `connectors::lock_connection` (owner + connector, shared with check/connect/disconnect), expiry re-read under the lock, guard moved into `spawn_blocking`. Write order: rotated refresh token, access token, row; a failed refresh-token write means the new access token is never used. `invalid_grant`/4xx → `needs_reconnect` + fixed text `connection_needs_reconnect`; network/5xx/429 → `connection_unavailable`, no status change. Tenant: recorded tenant and the fixed consent-tenant endpoint checked before the refresh token is read; no id_token parsing (accepted). `check_connection` now wraps a shared `refresh_connection`. `ensure_fresh(horizon)` shipped for wave 4's intent call (moved there). **C active and green.** Known limits: `expires_at = None` is never refreshed at swap (the app always sends `expiresIn` for Entra, `oauthEntraProfile.ts:142`); the lock is held up to ~10 s during a refresh. Orchestrator read the refresh path; re-run (merged with Phase 3 W3): `--fast` 113 scenarios: 107 pass, 6 red right reason (D at when.6, E at when.2, Phase 3 four), tenant scenario green; vam-orchestrator lib 1499 pass. Wave session: workspace green but the known root-only `hsp_uds`. Scoreboard: 3 green (A, B, C), 2 red right reason, 0 fix cycles, ~150 min. |
| 2026-11-08 | 2 | **Wave 4 merged** (squash, `skip-vm-lane`): new `connector_park.rs` (gate: not enabled / no consent / unknown URN → refuse "ask your admin", no park (Q7); no connection → park "Connect <name> to continue"; `needs_reconnect` → park "Reconnect <name> to continue"); async precheck in the `brokers.rs` port-52 arm calls `ensure_fresh(horizon = 60 s default tool timeout)` before the intent — `NeedsReconnect` parks, `Unavailable` proceeds (the swap refuses `connection_unavailable`; refinement of the "a failure there parks" line). `security.rs`: `park_for_reauth` split into shared `suspend_for_reauth`; gated intent entry, gate after `replay_from_journal`, before the policy decision. Park key `reauth_key(User, owner, "connector:<id>")` (no `hitl.rs` change); `put_connection` writes, releases the connection lock, then resumes only the caller's parks for that connector (`api/reauth.rs` `resolve_key`/`resume_key`). `connection.needs_reauth` notification raised once per transition in `refresh_connection` (Linear via `mcp_registry.rs`), attached to the parked run, owner only, resolved on connect/disconnect. CLI: `vam approve ls` and `vam notifications` show a `vam connectors connect <id>` hint; run-list `waiting_for` is JSON only (no CLI renderer exists). Spec changes (orchestrator, mock rules only, no `then`): D and E use `afterTool:` (the post-resume message is the continuation prompt), E one reply rule after the refusal. **D and E active and green: all 5 Phase 2 scenarios active and green.** Orchestrator read the resume scope (`put_connection` → `park_key(owner from auth, id)`) and lock release; re-run (merged with Phase 3 W5a): `--fast` 113 scenarios: 111 pass, 2 red right reason (Phase 3); vam-orchestrator lib 1533 pass; vam-cli 214 pass. Wave session: workspace green but the known root-only `hsp_uds`. Test gaps (scenario-covered only): no unit test that the park precedes the policy decision; the two-owner resume unit test only covers the empty case. Phase 3 told (5b may take `security.rs`; keep parks before the Cedar decision). Scoreboard: 5 green, 0 red, 0 fix cycles, ~170 min. |
| 2026-11-08 | 2 | **Wave 5 merged** (squash): `ConnectToContinueCard` for reauth parks (detected from `claims.lockedReason` `reauth:` + the "Connect|Reconnect <name> to continue" message; anything else falls back to the old card; `lockedReason` never rendered), Connect reuses the tile flow (`connectorConnect.ts` `runConnectorFlow`, no decide call), Decline sends REJECTED with a reason; `connection.needs_reauth` notification row with Reconnect (`CardKind` "connection", not a decision kind), `NOTIFICATION_KINDS` entry; disabled button + "Open Connections" when the connector is missing or not ready. Orchestrator re-run: `CI=1 pnpm test` 852/852, `tsc` clean; no src-tauri/e2e change. Gap: no component-level tests for the cards (pure helpers covered). **Phase 2 build done:** all 5 scenarios active and green; 0 fix cycles. Next: owner's manual check (5 steps in the PR body), release on the owner's word, team checklist out. |
| 2026-11-08 | 3 | Breakdown drafted (phase-breakdown agent): sliced to mail and calendar cards; 4 scenarios (A–D), CN-3.1–3.6, waves 0–6; publish-time checks of JSON jig files in the slice, `.jig` text and `Link` out; wave 3 (`skills.proto`, `hitl.proto`, `validation.rs`, OpenAPI) held until Phase 2's wave in those files merges; waves 0–2 can run first. |
| 2026-11-08 | 3 | **Breakdown approved** (owner) with recommendation 1 (every shared proto, validation and OpenAPI edit in wave 3, held until Phase 2's CN-2.1 merges; field numbers after Phase 2's) and recommendation 2 (re-check the lane overlap against Phase 2's breakdown); all open-question defaults accepted. **Lane check against:** Phase 2's wave 0 (started ~13:15) builds in the same e2e runner files (`vam-e2e` `scenario.rs`, `engine.rs`, `world.rs`, `forms.rs`, `e2e/README.md`) and also adds `files:` on skill world pieces. Agreed with the Phase 2 orchestrator: Phase 2's wave 0 owns `files:`; Phase 3 wave 0 waits for it to merge, merges the base and adds only `publish-skill` and `approve.amend`. Phase 2 confirms none of its waves touch `crates/vam-spec/src/jig*`, `spec/jig/` or the app's `JigRenderer`, so waves 1–2 are clear. Other holds confirmed: wave 3 after CN-2.1; wave 5 vs Phase 2 W2/W4 (`security.rs`, `hitl.rs`); wave 6 vs Phase 2 W5 (`ActionCard.tsx`, `vam.ts`, `ChatArea.tsx`). |
| 2026-11-08 | 3 | **Standing approval for Phase 3 (owner):** the orchestrator auto-approves its own wave recommendations and runs all waves without waiting; only very tricky or high-risk decisions go to the owner, with a notification. Signing push failures: try the GitHub route, else keep local commits and continue. Actions only for the microVM or benchmark lane. |
| 2026-11-08 | 3 | **Wave 0 merged** (`[skip ci]`): 4 scenarios pending, world pieces (test skill with echo mail/calendar tools, 7 JSON jig fixtures, 4 bad-card pieces), runner steps `publish-skill` and `approve.amend`, `deferred` world pieces (the agent too: vamd refuses with 409 to release an agent whose skill is unreleased), `PLANNED_EVENTS` (`card.skipped`; wave 4 moves it to `KNOWN_EVENTS`), README "Cards", guards-only edits on 3 scenarios. Orchestrator re-run: fmt, protos, manifests, check, workspace tests green but the known root-only `hsp_uds`; smoke 113: 104 pass, 9 red right reason (4 Phase 3 + 5 Phase 2), 0 fail. Scoreboard: 4 red right reason at when.0 class product (publish rejects `ui`: `unrecognized field name 'ui'`), 0 green, 0 fix cycles. **Local tip:** from a worktree run scenarios with a short `--out` (e.g. `/tmp/cn3/e2e`); the default output under `.claude/worktrees/…/target/e2e` makes the HSP gateway socket path 108 bytes (limit 107) and 24 scenarios fail at given, class harness. Unverified until waves 3–4: receipt line indexes and `outbox.card` field names in A–C. Next: wave 1 (CN-3.1, `List`). |
| 2026-11-08 | 3 | **Wave 1 merged** (`[skip ci]`): `List` (display only, one item template, no nesting, paint cap 100) and `{"$item": "/ptr"}` in `vam/1`; `check_card`, `check_ask_card` (`decision` reserved: string default "", never in `with`, `set.decision` approve/deny), `bind_args` (never sets `decision`). Catalog digest…1372`. Slack: `List` is outside `SLACK_PRIMITIVES`, so List cards fall back to Open in VAM (no change). Orchestrator re-run: vam-spec 266 unit + integration green; workspace green but `hsp_uds` (root); smoke 104 pass, 9 red right reason, 0 fail. Scoreboard flat (unit wave), 0 fix cycles. |
| 2026-11-08 | 3 | **Wave 2 merged** (`[skip ci]`): vendored `spec/jig` (byte-identical), pinned digest…`, `List`/`$item` in check, paint, renderer (100 then "and N more"; text only, no raw HTML). Orchestrator re-run: pnpm test 824/824, tsc clean; src-tauri unchanged. 0 fix cycles. |
| 2026-11-08 | 3 | **Wave 3 merged** (`[skip ci]`; started after Phase 2's CN-2.1): `ToolUi ui = 15` on `ToolSpec`, `ApprovalCard card = 21` on `ApprovalRequestClaims`, `ui` name rules (`[a-z][a-z0-9_]{0,63}`, `show` needs an output schema), OpenAPI, round-trip tests. Runner fix (wave 0 code): an `event` expectation now fails class product once the run has stayed PAUSED for the settle time instead of running to the step budget (class harness). Scenario movement (expectFail moved, no `then` change): D then.0 (bad skill publishes 200), A then.1 (no `outbox.card`), B when.3 (approval has no `card`), C when.3 (no `outbox.card`, parked); each red right reason alone and in batch. Orchestrator re-run: workspace green but `hsp_uds`; smoke 104 pass, 9 red right reason, 0 fail. Scoreboard: 0 green, 4 moved; 1 fix cycle (runner wait). Disk note: `scripts/e2e/run.sh` run directly builds into the worktree's own `target/` (12 GB here); set `CARGO_TARGET_DIR=/home/user/vam/target` for direct runs. Next: wave 4 (CN-3.3, CN-3.4). |
| 2026-11-08 | 3 | **Wave 4 merged** (`[skip ci]`): publish checks each tool's `jigs/<name>.json` (regular file, 64 KiB, `check_card` / `check_ask_card`; 400 names tool, file, JSON pointer); cards are checked again once when the session's tool catalog is seeded and kept in a host-side per-session map (orchestrator-required: a run never re-reads `materialized/skills`; test: editing a jig after seed changes nothing), pruned at session end (`forget_session` from `remove_thread_recorder`; orchestrator-required). Show card: host-run tools after `fit_response_to_frame` (the card sees the redacted result), guest-run tools from the thread recorder (host/builtin skipped); `outbox.card {jig, ir, data, tool, callId, skill, source: tool}` or `card.skipped {tool, callId, reason}`; `card.skipped` now a known event. Spec changes (orchestrator): `tool:` dropped from A's and C's `outbox.card` lines (`fields: data.tool` pins it); the `$item` refusal names the List's `$data` pointer so D's `contains: /messages` holds. Accepted risks (PR body): in-memory dedupe may double a card after a vamd restart; guest-run results carry guest trust; tool cards share the 60/min outbox budget; a failed guest tool emits `card.skipped`. Orchestrator re-run: workspace green but `hsp_uds`; smoke 113: 108 pass, 5 red right reason, 0 fail. **Scoreboard: D and A green (active)**; C moved to when.4 (ask card); B at when.3; 1 fix cycle (session-map pruning). Next: wave 5 (CN-3.5), shares `security.rs`/`hitl.rs`/`hitl.proto` with Phase 2 W4: whichever is ready first builds first. |
| 2026-11-08 | 3 | **Wave 5 split; 5a merged** (`[skip ci]`). Phase 2's W4 plan was approved first and builds in `security.rs`, so wave 5 is split: 5a (no `security.rs`) stores the ask card (`card_json` on `hitl_pending` and `approvals`, `register_hitl_suspend_card`), serves `claims.card`, builds the amendable schema from the card's submit fields typed from the tool input schema (`additionalProperties: false`, never `decision`), and decides a card AMENDED before the transaction: a key outside the card or `decision` is 400 with the row pending and no grant; otherwise requested args with only the submit fields replaced, canonical, granted and resumed on the merged digest. Inert in a real run until 5b (nothing registers a card yet). Orchestrator read the decide branch, `merge_card_amend` and the schema builder; re-run: workspace green but `hsp_uds`; smoke 109 pass, 4 red right reason (B when.3, C when.4, 2 Phase 2), 0 fail. 0 fix cycles. **5b** (the hook at the Cedar suspend site in `security.rs`, property-4 tests, B and C active, receipt indices) starts when Phase 2's W4 merges. |
| 2026-11-08 | 3 | **Wave 6 merged** (`[skip ci]`): tool cards in the thread from `outbox.card` `source: tool` (app `checkIr` + object/64 KiB gate, dedupe by `callId`, rebuilt on reload by a bounded fetch: newest 20 finished runs, lazy, once per mount, silent on failure — orchestrator-required), `card.skipped` shows nothing; approval card painted with `JigRenderer`, unchanged submit → APPROVED, edited → AMENDED with only changed submit fields, deny → REJECTED, 400 keeps the card with the error; a "What will run" summary always lists the tool and every requested arg and marks edits (orchestrator-required: a card can never hide e.g. the recipient). Cards placed in event order (the thread has no tool-call rows). Orchestrator re-run: pnpm test 839/839, tsc clean; src-tauri unchanged. 0 fix cycles. |
| 2026-11-08 | 3 | **Wave 5b merged** (after the Phase 2 lane's W4): the Cedar suspend site in `security.rs` asks with the skill's card (`skill_cards::ask_approval_card` from the session map, `bind_args`); connector/reauth parks stay before the decision and never carry a card. Property-4 tests. Spec changes (orchestrator): receipt index `lines.0` in B and C (allowed calls are not receipt lines; B still asserts `approved-amended`, C `approved-once`); mock rules `afterTool:` after resume. Two container restarts during the wave; work survived on disk; orchestrator verified: `--fast` incl. smoke **113/113 pass, 0 red**; B, C, approval-suspends-and-resumes, simple-jigs, and Phase 2's two park scenarios pass alone; workspace green but `hsp_uds` (root) and one transient `vam-vsock` loopback AddrInUse from two concurrent runs (passes 3/3 alone). **Scoreboard: B and C green; all 4 Phase 3 scenarios active and green.** |
| 2026-11-08 | 3 | **Phase 3 closed in the session**: waves 0–6 merged, 4 scenarios active and green, team checklist written (above). Release to `main` waits for the owner; one release for both lanes proposed. |
| 2026-11-09 | 2, 3 | **Release PR opened** on the owner's word: Phases 2 and 3 together, vam `feat/connectors` → `main` (this commit carries no `[skip ci]` so the release gate and the microVM lane run once), then the VAM app `feature/connectors` → `main`. Deploy: vamd first, then the app; no new env var. |
| 2026-11-09 | 2, 3 | **Released to `main`** on the owner's word, both lanes together: (merge commit; Actions in full once: checks incl. release gate, workspace tests, smoke, microVM lane all green) and (merge commit; test green; 852/852 and tsc clean locally). closed unmerged: its head (the base's last commit) carried `[skip ci]`, so GitHub ran nothing; added one release-record commit without it. Status: **released, checklists out** (Team checklists — Phase 2 and Phase 3). Bases `feat/connectors` / `feature/connectors` are behind `main` by the release merge commits until fast-forwarded (owner's word for the direct push) or merged in by the next phase. |
| 2026-11-09 | — | Owner decisions D23–D27 recorded in `design/connectors.md` (design session): a temporary Git marketplace in the public `squrell-inc/vam-marketplace` repo with trusted URLs per node; code in `vam`, data in marketplace repos; 0.1.0 trust (non-Squrell listings install skills, playbooks and agents only); Phase 5 (installable connectors) added; the org's own catalog unchanged. Phase 4 scope updated. |
| 2026-11-09 | 4 | **Breakdown drafted** (phase-breakdown agent,), awaiting owner approval: 7 scenarios (process, mock model, mock Graph, runner-made `file://` Git repos), CN-4.1–4.10, waves 0–9 plus benchmark lane B (seven days, owner's word); waves 2, 3, 4, 6 Sonnet high (envelope exact paths, Git fetch of admin-supplied URLs, catalog admission, effect journal on the egress path). Required owner decisions: Squrell admission (`catalog_tier.rs`, `VAMD_MARKETPLACE_DOMAIN`; default: domain `hub.squrell.cloud` by default, tier `marketplace`, writes only from the preconfigured Squrell URL) and the marketplace licence (default Apache-2.0). Findings from the code map: write tools need exact paths (a prefix admits `…/events/{id}/forward` and `…/messages/{id}/send`); in-VM tools are not journaled; the rootfs has Bun but no python3; `squrell-inc/VAM` is private (marketplace CI needs a deploy key). |
| 2026-11-09 | 4 | **Breakdown approved** (owner) with recommendations 1 and 2, open question 1 (a) (`VAMD_MARKETPLACE_DOMAIN` defaults to `hub.squrell.cloud`, tier `marketplace`, writes only from the preconfigured Squrell URL), open question 2 Apache-2.0, other defaults accepted. **Standing approval for Phase 4 (owner):** the orchestrator approves its own wave recommendations and runs all waves without waiting (the first push to the marketplace `main` included); only very tricky or high-risk decisions go to the owner, with a notification. Signing push failures: GitHub route, else local commits and continue. Actions only for the microVM or benchmark lane when required; everything else runs in the session. The seven-day lane B still needs the owner's word. Bases level with `main` (`feat/connectors` fast-forwarded). Wave 0 next. |
| 2026-11-09 | 4 | **Wave 0 merged** (squash, `[skip ci]`, `skip-vm-lane`): 7 scenarios pending; runner `git-marketplace` piece (deterministic commits, git with no config/hooks), steps `git-tag`, `marketplace-add/-refresh/-install/-update`, `catalog-write`, expects `marketplaces`/`listings`/`catalog`, `chat.agent` takes a URN, `*` path glob, Graph mock routes + `sendMail` hold; fixtures `marketplace-squrell` (index + provisional placeholder skill/agent/playbook, replaced in wave 5), `marketplace-community` (4 listings), overlays; route shapes for `/v1/marketplaces` in `e2e/README.md`; switch `PASS_MARKETPLACE_SEAM` off. Orchestrator decisions: placeholders now (A); marketplace scenarios must be order-independent on one shared vamd, wave 4 adds a process-only cleanup of installs (B); the exact-path refusal is proven with a test-only overreach skill (`skill-m365-overreach`, deferred, published by step; green with wave 2), never by a product tool that sends when told (C); refresh at when.0, install at when.1. Orchestrator re-run: `--fast` 120 scenarios: 113 pass, 0 fail, 7 red right reason (when.0, class product), 0 wrong reason. Wave session `--full`: only the known root-only `hsp_uds`. Scoreboard: 0 green, 7 red right reason, 1 fix cycle (fmt), ~50 min. Wave 1 next. |
| 2026-11-09 | 4 | **Wave 1 merged** (squash, `[skip ci]`, `skip-vm-lane`): CN-4.1 `build.rs` embeds every `spec/connectors/*.json` (`BUILTIN_CONNECTOR_FILES`, sorted by file name: `GET /v1/connectors` now lists Linear before Microsoft 365, no consumer depends on order), `connectors_from_files` refuses invalid/duplicate. CN-4.2 `vam-spec/src/package.rs` (`ArtifactKind` incl. `Connector` detected and refused, `check_dir`, `CheckedPackage`, `CapabilitySummary`/`ToolSummary`/`EgressSummary`/`PathReach{exact, anyPath}`, `read_package_file` no symlinks, 1 MiB), `marketplace.rs` (`MarketplaceIndex`, `parse_index`, `check_git_ref` refuses leading `-`, `..`, commit ids; `NodeRequirement`; `check_marketplace_root`); vamd publish calls the shared checks (messages unchanged; registry-domain admission stays in vamd); `vam catalog validate <dir> [--strict] [--kind] [--json]` offline, exit 6. Package layout: skill dir (`SKILLS.md`, `manifest.json`, entry points, `jigs/`), `agent.json`/`playbook.json` `{spec}`. Fixture edit: community `notes-search` gained `requiresConnector`. Note for CN-4.6: a playbook with no bound tool warns (fails `--strict`). Orchestrator re-run: `--fast` 120 scenarios: 113 pass, 7 red right reason, 0 fail; workspace 121 targets ok, only the known root-only `hsp_uds`. Disk: ladder steps 1–2 (incremental, 43 older test binaries), 9.5 → 15 GB free. Scoreboard flat (unit wave, expected), 0 fix cycles. Wave 2 next. |
| 2026-11-09 | 4 | **Wave 2 merged** (squash, `[skip ci]`, `skip-vm-lane`): `ToolEgress.paths = 5` (exact paths); `connection.rs` `exact_admits` (same decoded segments, request trailing slash refused, malformed declared path admits nothing); publish validation (`egress_path`, no `{id}` templates, no `? # %`), `WRITE_METHOD_PREFIX_WARNING` (warning, not refusal); summary `PathReach.exact`, `writesByPrefix`; OpenAPI `ToolEgress`. Proof: unit tests refuse `/forward`, `/cancel`, `/send`, `%2F`, dot segments, double encoding; a temporary scenario with `skill-m365-overreach` passed (draft created, send refused, Graph saw no `/send`) and failed as a negative control with `pathPrefixes`. Orchestrator read `exact_admits` and the screen change; re-run: `--fast` 113 pass, 7 red right reason, 0 fail; workspace 121 ok, only `hsp_uds` (root). Note for wave 5: m365 writes use exact `/v1.0/me/sendMail`, `/v1.0/me/events`, `/v1.0/me/messages`; reads keep GET prefixes. Scoreboard flat (unit wave; overreach step goes green with the scenario once waves 3–5 land), 0 fix cycles. Wave 3 next. |
| 2026-11-09 | 4 | **Wave 3 merged** (squash, `[skip ci]`, `skip-vm-lane`): trusted marketplaces on the node. `marketplace/` in vam-orchestrator: `url.rs` (`check_url`: https, 443, no userinfo/query/IP literal/internal suffix; `is_refused_ip` strict + Azure wire server), `guard.rs` (loopback CONNECT proxy: dials only checked public addresses on 443, re-checked at connect, idle/total/byte caps; **per-call 256-bit lease token, 407 before any lookup** — orchestrator-required fix: the first version accepted any local process, letting a process-mode guest tunnel past its egress policy), `fetch.rs` (git with cleared env, `protocol.allow=never` + https only, `http.followRedirects=false`, fsck, no hooks/submodules/LFS, bare repo read raw with `cat-file`, depth 1, HEAD + listing tags only, `check_git_ref` before use, `--`, watchdog, 128 MiB cache cap; `file://` only in test/process-guest), `store.rs` (`node_marketplaces`, `node_marketplace_tags`; Squrell seeded), pins written once, moved tag 409; refresh every 6 h (`VAMD_MARKETPLACE_REFRESH_SECS`), off in process-guest; `VAMD_GIT_BIN`. Routes `/v1/marketplaces…` users:admin only (members 403 incl. list); `vam marketplace ls|add|enable|disable|rm|refresh|listings`; seam `VAMD_PROCESS_MARKETPLACE_URLS`, `PASS_MARKETPLACE_SEAM` on. M3 on add against a fixed reserved list. Left for CN-4.5: `VAMD_MARKETPLACE_DOMAIN` default, tier `marketplace`, admission, origin columns, install/update, uninstall on marketplace delete. For CN-4.5: `cached_tag_commit`, `Fetcher::export_dir`, `lock_marketplace`. Spec changes (orchestrator): `expectFail` positions moved (no `then`). Orchestrator read url/guard/fetch config; re-run: `--fast` 113 pass, 7 red right reason (install steps: when.4, when.6, when.2, when.1 ×4), 0 fail; workspace 121 ok, only `hsp_uds`; release gate PASS. Note: parallel `local.sh` runs sharing a `LOCAL_CI_OUT` get 401s; use a fresh dir. Finding: nodes behind a corporate HTTPS proxy cannot fetch (the guard dials directly) — `Decide: BACKLOG — proof 1, cost 1`. Scoreboard: 0 green, 7 moved, 1 fix cycle (guard auth), ~75 min. Wave 4 next. |
| 2026-11-09 | 4, 5 | **Phase 5 folded into Phase 4** (owner, relayed by the connectors design session; owner's message checked in that session's transcript). New CN-4.11 connectors in listings (wave 4b, Sonnet high; one new pending scenario `community-connector-appears-unverified-and-connects`); CN-4.8 shows a connector's publisher, client id, hosts, scopes, MCP server and requires Enable again when an update changes them; wave 8 merged into wave 5 (listing written straight into `squrell-inc/vam-marketplace`, vam fixture vendored with a stamp); waves 6 and 9 unchanged. Out: bring-your-own client, generic OAuth profile, the old Phase 5 trust model. `design/connectors.md` D25, D26 and §10 rows 4–5 amended. Wave 4 continues unchanged. |
| 2026-11-09 | 4 | **Wave 4 merged** (squash, `[skip ci]`, `skip-vm-lane`): one import for install and update (`marketplace/import.rs`, generic per-artifact loop), `installs.rs` (`node_marketplace_installs`, `catalog_origins` side table, owner system user `marketplace:<id>` hidden from `/v1/users`, process-only reset `/v1/process/marketplace-reset` proven absent by the release gate). Q1 (a): `VAMD_MARKETPLACE_DOMAIN` defaults to `hub.squrell.cloud`; migration `catalog_tier_marketplace` rebuilds CHECK tables to `user | platform | marketplace`. M2 `reject_caller_installed_urn` at every write sink (draft, put, publish, release, delete, owner reassign): installed URN 409, URN under any trusted `publisherDomain` 400. M3 artifact URN domain = marketplace `publisherDomain`, re-checked against reserved domains for added marketplaces; only the preconfigured Squrell row writes the marketplace domain. M4 listing connector only from Squrell (4b changes this). M5 `cached_tag_commit`, moved tag 409. Resolve-only admission `admits_installed_skill_registry`. Agents must carry `permissions`. Update: changed agent/playbook → next patch; changed skill needs a new manifest version; `capabilitiesChanged` per artifact (summary digest). Marketplace delete refused while installs exist; no uninstall. New `DELETE` routes (409 installed, 405 otherwise). `vam catalog import`. API: `tier`, `readOnly`, `origin {marketplace, listing, ref, commit, packageDigest}` on catalog records; listings `installed`, `updateAvailable`. Orchestrator read the M2 guard and the import admission; re-run: `--fast` 120: 116 pass, 4 red right reason, 0 fail; workspace 121 ok, only `hsp_uds`; release gate PASS. **Scoreboard: 3 green (install, unverified limits, update available; active)**; the four Microsoft scenarios past install (expectFail when.10, when.5, when.6, when.6). 0 fix cycles. Wave 4b next. |
| 2026-11-09 | 4 | **Wave 4b merged** (squash, `[skip ci]`, `skip-vm-lane`): connectors in listings. `vam-spec/src/connector_listing.rs` (community limits: profile `discover` only, no secret, `orgConsent: none`, no `bindings`, `mcp` required with an https public DNS URL, 1–64 plain tool names, ids not Squrell's or a built-in's, no lookalike names), `package.rs` connector kind (`connector.json`, `ConnectorSummary`); `marketplace/connector_store.rs` (`connector_releases`), import arm with id-clash refusal, not enabled on install, an update that changes the capability summary sets it back to not enabled and marks members' OAuth rows `needs_reauth` in the same transaction; M4 now: a listing's `connector` must be one it ships, only Squrell may name built-ins. `GET /v1/connectors` serves installed entries with `source`, `trust`, `origin`, `capabilities`; gate as Linear (enabled, own URL, vetted tools). New scenario `community-connector-appears-unverified-and-connects` (pending first, red at when.0 class product, now active and green); fixture `marketplace-community-connectors`; runner switch `PASS_MCP_SERVER_SEAM`, process-only seam key `mcp:<url>`. **Scope narrowed (orchestrator, owner told, default unless the owner says otherwise):** API-key providers (no node flow stores a member's API key) and existing OAuth profiles with a publisher client id (`entra` needs REST bindings = egress to publisher-chosen hosts; `google`/`slack` need a secret) are refused and recorded as a follow-up: `Decide: BACKLOG — proof 2 (D26 breadth), cost 2 (installed bindings in the egress gate; API-key connection kind)`. Orchestrator read the gate and admission rules; re-run: `--fast` 121: 117 pass, 4 red right reason, 0 fail; workspace 121 ok, only `hsp_uds`; release gate PASS. **Scoreboard: 4 of 8 green.** 0 fix cycles. Wave 5 next. |
| 2026-11-09 | 4 | **Wave 5 merged** (squash, `[skip ci]`, `skip-vm-lane`) and **`squrell-inc/vam-marketplace` bootstrapped**: first commit on `main` (README, Apache-2.0 LICENSE, `marketplace.json`, CODEOWNERS, `.github/workflows/validate.yml` building vam-cli at pinned with a read-only deploy key `VAM_DEPLOY_KEY`, Bun 1.4.4, `vam catalog validate . --json` filtered strict, per-listing tag check, Bun tests), listing `microsoft-365`: `m365-mail` (Bun TS; reads GET prefixes `/v1.0/me/messages`, `/v1.0/me/calendarView`; writes exact `/v1.0/me/findMeetingTimes` (READ_ONLY, accepted warning), `/v1.0/me/messages` (draft, allowed), `/v1.0/me/sendMail` (ask, `send_approval`), `/v1.0/me/events` (ask, `book_approval`, IDEMPOTENT, Graph `transactionId` = sha256 of canonical args)), Executive assistant (permissions mode, default deny; reads, draft, `step_complete`, `start_playbook`, schedule list/get allow; send, book, schedule create/update/delete ask), morning triage (one step bound to `mail_search`). The wave session's worktree could not write the marketplace checkout; the orchestrator made the commit (tree = the vendored fixture's) and pushed `main`. **Tag `microsoft-365-v1.0.0` not pushed: the session's git proxy cuts tag pushes (5 tries); owner asked to create it, and to add the `VAM_DEPLOY_KEY` deploy key + secret.** vam: fixture vendored with `SOURCE` (commit + tree), `scripts/marketplace/check-snapshot.sh` in `--fast`; Bun pinned in `rust.yml` and `local.sh` (installed as a real file in `/usr/local/bin`: tool processes drop privileges and cannot follow a symlink into `/root`); `exec.rs` sets `VAM_EGRESS_ENDPOINT` for tools; harness `wait-scheduled-run` lists as the schedule creator. Spec changes (orchestrator-approved): mock inputs in scenarios 5–6; scenario 4 receipt index `lines.1` → `lines.0` (each run has its own receipt; only asked/denied calls are lines; observation only). Triggered runs used the grant owner's connection with no product change. Orchestrator reviewed the tool code and shapes; re-run: `--fast` 121: 120 pass, 0 fail, **1 red wrong reason: `send-interrupted-mid-call-is-not-sent-twice` passes early (its `then` is too weak)** — wave 6 tightens it first; workspace 121 ok, only `hsp_uds`; release gate PASS. **Scoreboard: 7 of 8 green** (walkthrough, scheduled triage, revoke-resume active). 0 fix cycles. Wave 6 next. |
| 2026-11-09 | 4 | **Wave 6 merged** (squash, `[skip ci]`, `skip-vm-lane`): connection writes journaled. `vam-network` `WriteTicket` (reserve before the token is read, `commit_send` immediately before dispatch in `broker.rs` and the port-55 MITM path, `answered(status)`), `effect_path.rs` `reserve_write`/`WriteCall` (RAII: dropped after send without an answer → `unknown`), `session_tool_egress.effect_class` (from the skill manifest), lease generation on `record_intent`, `settle_orphaned_sends` after a vamd crash. Crash points: journal write fails → nothing sent (`effect_journal_unavailable`); answer recorded → `REPLAYED` before the PDP, model told "not sent again"; answer lost/5xx → `unknown`, non-idempotent retry asks, idempotent (`calendar_create_event`, Graph `transactionId`) resent under its key; 4xx → `failed`, replayed as failed. No token or body in the journal. Limits: a multi-write call is protected as a whole (a crash between two writes misses the second, never duplicates); a write without a call id is refused; the receipt's `effects` section still reads "not journaled" (E12 Phase 3; the proof here is the tool-call lines + `effect.replayed`). Spec change (orchestrator-approved, per the Scenario delta): `send-interrupted-mid-call-is-not-sent-twice` asserts `effect.replayed` for `mail_send`, "not sent again", receipt one `approved-once` line; it was flaky on the base (one run saw two `sendMail` — a real duplicate). Orchestrator read the commit-before-send sites; re-run: `--fast` **121/121 pass**; the interrupted-send and two effect scenarios 3/3 three times alone; workspace 121 ok, only `hsp_uds`; release gate PASS. **Scoreboard: all 8 Phase 4 scenarios active and green.** 0 fix cycles. Wave 7 (the VAM app) next. |
| 2026-11-09 | 4 | **Wave 7 merged** (squash, `[skip ci]`): Org settings Marketplace place (users:admin; members see no item, deep link locked), trusted marketplaces with Squrell/Unverified badges, Add (confirm URL and domain), Refresh, Enable/Disable, Remove (409 reason shown); listings with the capability summary (hosts, paths, any-path chip, prefix-write warning, connector publisher/client id/scopes/MCP server); Install and Update always confirm, capability changes shown before and `capabilitiesChanged` after (needs Enable again); Connectors place shows trust, origin, capabilities before Enable; installed rows read-only with badge (operators too). Eight Tauri commands, all writes with `Idempotency-Key`. Listing text rendered as plain text (no raw HTML). Orchestrator re-run: `CI=1 pnpm test` 893/893, `tsc` clean, src-tauri 59/59. **Phase 4 build done:** 8 of 8 scenarios active and green; Phase 4 close block written (scoreboard, owner items, wave 9 runbook, team checklist). Wave 9 and lane B wait for the owner. |
| 2026-11-09 | 4 | **Release PR opened** on the owner's word: vam `feat/connectors` → `main` (this commit carries no `[skip ci]`, so the release gate, workspace tests, scenarios and the microVM lane run once on Actions; the lane is the first run of the Bun tools in a Firecracker guest), then. Before it: `main` merged into `feat/connectors` by (merge commit; conflicts in the egress code resolved keeping both sides: main's `secret_bound` and oversize refusal, Phase 4's journal ticket and exact paths; clippy 1.97 fixes; orchestrator re-run: 125/125 scenarios, release gate PASS, workspace green but `hsp_uds`). Deploy: vamd first, then the app; no new required env var (`VAMD_MARKETPLACE_DOMAIN` now defaults to `hub.squrell.cloud`; `git` must be on the node or `VAMD_GIT_BIN` set). |
| 2026-11-18 | 5 | **Breakdown drafted** (phase-breakdown agent), awaiting owner approval: 5 scenarios (process, mock model, new `mock-filebox-server` confidential-client MCP provider), CN-5.1–5.7, waves 0–5 (W2, W3 and the app W4 Sonnet high: the org secret in the vault, the node-side code exchange and single-use refresh, the secret in the app); 12 open questions with defaults (disable keeps the secret; secret-less org clients keep the app exchange; `POST /v1/connectors/{id}/connection/oauth/code`; Box imported on a staging node via a Squrell index branch, CN-5.6). |
| 2026-11-18 | 5 | **Breakdown approved** (owner, defaults accepted; orchestrator runs the waves and asks the owner only on high-risk calls). **Wave 0 merged** (#33, squash into `feat/connectors`). Scoreboard: 5 scenarios red (right reason, class product, all at when.2: the filebox install is refused until CN-5.1), 0 red wrong reason, 22 connector scenarios green, Actions green; 0 fix cycles; ~49 min. Notes for later waves: the connector id is `filebox` (vault id `connector/filebox/oauth/client-secret`), the capability-summary field is `client`, members see `node.client.authorizeEndpoint` and only admins `updatedBy`, the exchange route is `POST /v1/connectors/{id}/connection/oauth/code`. Disk hit 0 mid-wave (ad-hoc test builds, duplicate target dir); 11G free after cleanup. |
| 2026-11-18 | 5 | **Wave 1 merged** (#36, squash into `feat/connectors`). Scoreboard: 1 green (`listing-connector-with-an-admin-supplied-client-installs-and-waits-for-its-client`), 4 red right reason (scenario 2 at when.4, 3–5 at when.3: Enable with `client` is 400), 12 other connector/marketplace scenarios green, Actions green; 1 fix cycle (orchestrator). **Scenario spec fix (orchestrator):** wave 0's listing scenario read `provider.*` as a member before Enable, contradicting the tested rule that members see no definition until Enable; the client checks now read the admin's `capabilities` (commit 47c351f). Decisions later waves match: `ClientKind {Publisher, Org}` with `client` skipped when publisher; `ClientSecret::OrgVault`; summary `client`/`clientText` absent for publisher clients (app treats absent as publisher), `clientSecret` always present; `squrell_index_ref` applies only to the preconfigured entry, a missing branch is a failed read. Disk: fell to 2 GB in the wave (wave's local.sh did not run); orchestrator ran ladder step 1 (`target/debug/incremental`, 11 GB), 14 GB free. ~50 min. |
| 2026-11-18 | 5 | **Wave 2 merged** (#39, squash into `feat/connectors`). Scoreboard: 2 green (+`admin-supplied-client-is-set-only-by-an-org-admin-and-never-read-back`), 3 red right reason at when.4 (the exchange route 404; the app-path token upsert not yet refused), 24 other connector/marketplace scenarios green, Actions green; 2 fix cycles (orchestrator): (1) security review: a stored secret was kept when the admin saved a different client id without a secret; now a new client id needs its own secret (400), unit and route tests updated (c4e4e58); (2) clippy `large_enum_variant` on `ClientWrite` (boxed `NodeClient`), which `local.sh --fast` does not catch: later wave briefs run `cargo clippy -p <crate> --all-targets -- -D warnings`. Reviewed: the only secret writer is `connector_client.rs` `put_node_with_client` (sealed before the transaction, org-scope `connection` row, destination = token host); discovery `connector_discovery.rs` pins public addresses per fetch (`PinnedUrl`, `is_restricted_ip_strict`), no redirects or proxy, size and time caps, issuer match, S256. The wave also hid the row from `/v1/org/secrets` (`api/org_secrets_admin.rs`), which the plan missed. For wave 3: read the secret with `decrypt_secret_at(Org, ORG_OWNER_USER_ID, client_secret_binding_id(id))` (never `decrypt_secret_for_caller`) and check the request host against the row's destination; endpoints from `node_client(id)`; revocation is pinned only when on the token host. ~1.5 h. |
| 2026-11-18 | 5 | **Wave 3 merged** (#44, squash into `feat/connectors`). Scoreboard: **5 of 5 Phase 5 scenarios green**; 20 connector/marketplace/linear/microsoft-365/mcp scenarios green; Actions green; 0 product fix cycles, 3 on fixtures. Orchestrator approved two `when`/fixture changes (no `then` edited): the filebox tool `read_file` shadowed a builtin (renamed `get_file_content`); the rotating scenario uses two agent aliases because one member's chats with one agent queue (AR-3.8), with the mock answering a refused call on hold release. Security review: the secret is read only in `connector_client.rs` `org_client_secret` (host = pinned token host, row kind/scope/single destination checked, org scope only), sent only by `mcp_oauth_refresh.rs` `pinned_token_client` (https, one exact host, `PinnedUrl`, no redirects/proxy); the loopback seam is `cfg(test)`/`process-guest` only; single-use refresh serialized by the existing `McpRefreshGate`, with the code exchange taking the same gate. **For wave 4 (app):** route `POST /v1/connectors/{id}/connection/oauth/code`, `Idempotency-Key`, body `{code, codeVerifier, redirectUri}` only; redirect `squrell://oauth` or `http://127.0.0.1:<port>/<path>`; 200 = list entry `state: connected`; 400 bad body/refused code, 403 not enabled, 404 unknown, 409 no client or provider refused the client (show "ask your admin"), 502 provider down/no refresh token; authorize URL from `node.client.authorizeEndpoint` and `clientId`; no app-side exchange for `org_vault`. **Backlog:** marketplace import should refuse a connector whose vetted tool names shadow a builtin (it imports but can never connect); check the Box tool names for this in CN-5.7. ~1 h. |
| 2026-11-18 | 5 | **Wave 4 merged** (squrell-inc/Squrell-App #11, squash into `feature/connectors`, base recreated from app `main` 0e47c90). Orchestrator re-ran at the PR head: `CI=1 yarn test` 1170 pass / 1 fail / 2 skipped, the one failure (`AgentCreationCycle.test.tsx` MCP selection) also fails on the untouched base 0e47c90 (pre-existing, not this wave); `tsc --noEmit` clean; agent ran Playwright 16/16. Review: the admin secret lives only in an uncontrolled password input and a request object cleared in `finally`, never in state, storage, URL, logs or toasts; members never send one; the new Tauri command `exchange_connector_code` does not log and encodes the path segment. Not verified here: `cargo test` for `src-tauri` (gdk/webkit missing in the container) and the real `squrell://` deep-link return — both go to the CN-5.7 Box run on a desktop build. **Open for the owner:** (1) web deployments cannot connect an `org_vault` connector: the node accepts only `squrell://oauth` and `http://127.0.0.1:<port>/…` as redirect URIs (open question 6) and the web app's origin is neither; (2) on desktop a 400 and a 403 both arrive as `ConnectError::Forbidden`, so the app tells them apart by message text (heuristic; a structured error code from vamd would fix it). **Backlog:** `CONNECTOR_MANAGED_MCP_IDS` in the app is hard-coded to `["linear"]`, so marketplace connectors' MCP rows (e.g. `filebox`, `box`) still show in the generic MCP list; no "remove client" button (`client: null` with `enabled: false` is supported by the body builder). Plan corrections: the app uses yarn (not pnpm) and has no `e2e:typecheck` script; component paths are `components/org/` and `components/mcp/`. ~20 min agent + review. |
| 2026-11-18 | 5 | **Release review of #52 fixed** (branch `cn-5-review` into `feat/connectors`). Each finding has a failing-before test: (1) the Enable write re-checks the saved client (id, token endpoint, `secret_set`, `updated_at`) and the connector definition inside its transaction and answers 409 "the client changed while saving; save again" when another admin's save moved either; (2) the exchange commit re-reads the client, the enable and the connector's server and stores nothing (409 "sign in again") for a client replaced or a connector turned off mid-exchange; (3) the member's row, the refresh token and the access token are one transaction (`commit_connector_connection`), sealed first, the gate generation bumped after; a failed save after the spent code says to sign in again; (4) tool adoption is conditional on the credential generation the write left (the gate is kept alive by a `CredentialGeneration` handle: a gate with no holder is dropped, and its counter with it) and never upserts a missing row; (5) **decision: no path re-adopted tools** (the 401 refresh only refreshes tokens, the tools-refresh route treats a vetted tool added to an empty pin as drift needing approval), so a sign-in whose tools cannot be read is stored but left `needs_reauth` with the fixed reason `TOOLS_PENDING_REASON` (calls fail fast, the notification stays open, parked runs stay parked) and `POST /v1/mcp/connections/{id}/refresh` adopts the vetted tools (the same first pin the connect makes), puts the row back to `connected`, resolves the notification and resumes the runs, in that order; the app's own token upsert takes the same path; (6) saving a corrected secret for the same client clears `needs_reauth` only on rows whose reason is the refused admin client (`CLIENT_REFUSED_REASON`), resolves their notification and resumes their runs; a revoked grant still needs a sign-in; new scenario `admin-corrects-a-refused-client-secret-and-the-member-is-not-asked-to-reconnect` (the existing correction scenario never reached a refused refresh); (7) a moved connector's old row and tokens go in the same commit as the new grant, after it; (8) **decision: pin the token authentication method at Enable** (`client_secret_post` when listed, else `client_secret_basic` when listed or omitted, per RFC 8414 §2; neither is a 409), column `token_auth_method` on `node_connector_clients` (migration default `client_secret_post`), used by the exchange, refresh and revoke (HTTP Basic carries the form-urlencoded id and secret, the form has no `client_secret`); (9) the protected-resource `resource` must be a string naming the identifier its well-known URL was built from (the root fallback is judged against the root identifier); (10) enabling with `Keep` needs `secret_set` for an `org_vault` connector; (11) the saved id is compared trimmed; (12) a named-branch read and a pinned-tag check ask only for tags, never `HEAD` or the index branch. Minors fixed: the issuer is compared and stored exactly as named (RFC 8414), the secret's destination check and its ciphertext come from one read, the node body's parse error is fixed for every failure, the OpenAPI and `runtime/tools.md` sentences name their proofs (or say not proved), the runner rejects a second filebox piece, a non-filebox alias for a filebox secret, `expire` with `revoke`, and a refused-connect `contains` it ignored, keeps every rotated secret in the leak set, refuses a refresh on a revoked mock and judges a held refresh on the secret current when released, and `secret-absent` waits for the run before it collects values. The runner now clears a connector's client before its marketplace is removed (after that the clear was a 404, so every scenario left its client and vault secret on the vamd) and records a client only when the node accepted it. Declined (reasons in the PR): the discovery deadline on every fetch, per-grant access tokens in the filebox mock, a retry record for a failed client clear (it is reported instead), and extra assertions in the three existing scenarios (no scenario was edited). |
| 2026-11-19 | 5 | **Released to `main`** (owner): VAM #52 merged as 4f24c41, Squrell-App #12 as bb4ee15, after CodeRabbit rounds 1–6 were fixed (#68, #70, #71, #73, #74; main merged in by #72). Remaining CodeRabbit item (admin choice of the token auth method) and the known trade-offs logged under "Phase 5 close"; CN-5.7 (Box real tenant, team) does not block Phase 6. |
| 2026-11-19 | 6 | **Breakdown drafted** (phase-breakdown agent), awaiting owner approval: 5 scenarios (process, mock model, new `mock-ticketdesk-server` fixed-endpoint public-client REST provider per company), CN-6.1–6.8 (6.8 Salesforce `instance_url` backlog), waves 0–6 (W3 and W4 Sonnet high: the exchange and refresh for `profile: oauth`, the egress swap for installed connectors; W4 runs the microVM lane), 14 open questions with defaults (the node exchanges the code for every admin-supplied client, editing locked 13 on approval; the token auth method stays backlog). |
| 2026-11-19 | 6 | **Breakdown approved** (owner): the node exchanges for every org client (locked decision 13 amended), Salesforce `instance_url` deferred (CN-6.8 backlog), all open-question defaults accepted (subdomain one DNS label, no token auth method this phase, reads plus ticket create). Wave 0 started. |
