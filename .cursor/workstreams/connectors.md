# Workstream: Connectors (how VAM reaches the systems people work in)

```yaml
status: active
slug: connectors
current_phase: 4 build done 2026-11-09 (8 scenarios active and green; waves 0–7 merged), real-tenant wave 9 and lane B wait for the owner; 2 and 3 released to main 2026-11-09, checklists out; Phase 1 released, checklist out
design_docs:
  - design/connectors.md
  - design/mcp-oauth-connections.md
  - design/hsp-triggers.md
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
  (`design/mcp-oauth-connections.md`, CLI deferred).
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
`vamd.rs`; vam `api/mod.rs` routes). The `mcp-oauth-connections.md`
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
12. **`resource` for Entra vs `design/mcp-oauth-connections.md`.** That doc
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
`design/mcp-oauth-connections.md` (design §12) was approved and applied
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

### Phase 5 — Installable connectors

**Goal:** developers ship connectors from any trusted marketplace without a
Squrell release (design D26, §10 Phase 5 row).
**Depends on:** Phase 4 (marketplace import)
**Status: folded into Phase 4 (owner, 2026-11-09).** Connectors ship as files in marketplace listings (CN-4.11). What follows stays later: bring-your-own OAuth client and a generic OAuth profile (small follow-up), and the rest of this scope.
**Plan:** not drafted
**Scope:** connector as a catalog kind (`urn:vam:connector`); installed and
built-in connectors merged; qualified connection ids and Squrell-reserved
ids; the admin's approved capability snapshot (publisher, OAuth app, hosts,
scopes, MCP server) with re-approval when an upgrade widens it; a generic
OAuth profile with bring-your-own client (vault-held secret); `vam connector
init`, `validate`, `test`. Rubric: cost 3 (new trust boundary).

## Explicitly later

- Hub, OCI distribution, entitlements, metering from receipts (design §9.3).
- Verified publishers (namespace proof, signing) for marketplace content.
- Event subscriptions and the first event source (design §6.6).
- Teams relay, Google secret relay, GitHub, databases.

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
| 2026-11-08 | 1 | **Released to `main`** on the owner's word: (merge commit; Actions in full once: checks, workspace tests, smoke scenarios, microVM lane all green) and (merge commit; test green). Bases fast-forwarded to `main`. **Phase 1 scoreboard:** 5 scenarios added, 5 active and green; waves 0–7 (wave 7 code only), 3 fix cycles in total (W2 scenario isolation, W3 observation spec change, W7 test config); Actions used only by the release PR. Status: released, checklist out. Open: team checklist (incl. Microsoft steps 6–7 on the owner's test tenant, P6 minutes and clicks), publisher verification, owner's one-line note in `design/mcp-oauth-connections.md` (Q12). A checklist failure becomes a fix wave on `feat/connectors` / `feature/connectors`. |
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
