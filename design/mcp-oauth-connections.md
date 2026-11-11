# MCP OAuth connections

**Status:** Proposed
**Date:** 2026-10-07
**Owner:** maintainer
**Repos:** `squrell-inc/VAM` and the app repo

## ADR decision record

**Context.** MCP connections today authenticate only with a pasted
`Authorization` header token stored as a vault Egress secret. Many remote MCP
servers now act as OAuth 2.1 resource servers and expect the client to obtain
tokens through discovery, client registration, and an authorization-code + PKCE
flow. Pasted tokens expire, cannot be refreshed, and push users into copying
long-lived credentials around. Separately, the app UI already lets users enter
multiple headers, but the backend `ConnectMcpRequest` is Authorization-centric
and silently drops the extra headers.

**Decision.** Add OAuth as a second connection auth mode alongside static
headers. the VAM app acts as the OAuth user-agent: it performs discovery,
DCR-first client registration (CIMD fallback), PKCE, opens the browser, receives
the `squrell://oauth` callback, and exchanges the authorization code. It then
hands the access and refresh tokens to `vamd`, which stores them in the secrets
vault, attaches the access token as `Bearer` on every MCP HTTP request, and owns
refresh-on-401 with a single retry. Static header/token auth stays as a
permanent fallback. Multi-header connect is fixed first.

**Consequences.**
- Users can connect OAuth-protected MCP servers without handling raw tokens.
- `vamd` never runs a browser and never sees the authorization code; it does
  hold the refresh token and must talk to the authorization server's token
  endpoint.
- Public-client (DCR/CIMD) registration means no client secret ships in the
  app; PKCE is mandatory.
- Connection records grow an `authMode` and OAuth metadata; disconnect must
  clean up more vault bindings.
- Two repos must ship in a coordinated order (see dependency graph).

## What we are building

1. **Multi-header static connect that actually works.** `PUT
   /v1/mcp/connections` accepts a `headers[]` list, persists each as its own
   vault binding, and keeps `secretValue` as shorthand for a single
   `Authorization` header.
2. **An OAuth connection mode.** A connection is either `static` (headers) or
   `oauth`. OAuth connections store access token, refresh token, expiry, and
   the authorization-server metadata needed to refresh.
3. **App-side OAuth flow.** the VAM app discovers the authorization server via
   RFC 9728 Protected Resource Metadata, registers a client (DCR first, CIMD
   fallback), runs authorization code + PKCE (S256) with an RFC 8707
   `resource` indicator, catches the `squrell://oauth` deep link, exchanges the
   code, and upserts the tokens to `vamd`.
4. **vamd refresh-on-401.** When a tool call returns HTTP 401, `vamd` uses the
   stored refresh token, rotates the access binding (and the refresh binding if
   the AS rotates it), and retries the call once.
5. **Status and reauth UX.** Connections expose a `needs_reauth` state when
   refresh fails, and the app shows a reconnect CTA.

## Why

- Paste-a-token is fragile: tokens expire silently and break tool calls with
  no recovery path.
- The MCP authorization spec standardizes HTTP transports on OAuth 2.1, so
  more servers will require it over time. See the
  [MCP authorization specification](https://modelcontextprotocol.io/specification/2026-07-28/basic/authorization).
- Keeping tokens in the VAM secrets vault, with refresh owned by the component
  that makes the calls, means agents keep working without user interaction
  until a refresh genuinely fails.
- The multi-header gap is a live bug: users enter headers in the UI that never
  reach the server.

## What we are not building

- No browser, loopback listener, or authorization-code handling in `vamd`.
- No confidential OAuth client with a shipped client secret in the app.
- No token passthrough: tokens obtained for an MCP server are never forwarded
  to other services, and `vamd` never accepts a token minted for a different
  audience.
- No removal of static header/token auth; it remains a supported mode
  indefinitely.
- No org-wide OAuth sharing model in this milestone (deferred, see open
  questions).
- No generic OAuth broker for non-MCP egress. *(Amended 2026-11-07: connector bindings get one; see the note under the decision summary.)*
- No proactive background refresh scheduler for MCP connections *(amended 2026-11-07: connector bindings refresh before swap)*; refresh is reactive on 401 (an
  `expires_at` pre-check may be added inside V2 if trivial, but is not
  required).
- No new Cedar actions unless an implementer finds an existing action cannot
  cover token upsert; raise it before adding one.

## Decision summary (locked)

> **Amended for connector bindings (owner, 2026-11-07; `design/connectors.md` §12, Phase 2).** For a connection created from a connector, `vamd` mints per-audience access tokens and refreshes them before they are swapped into a request (proactive, not only on 401); the client id comes from the connector's profile ahead of DCR (Squrell's own registrations, e.g. Entra, which has no DCR). These three rows and bullets below still hold for plain MCP connections. "No token passthrough" holds for both: a token goes only to its audience.


| Topic | Decision |
| --- | --- |
| Goal | OAuth for MCP connections; tokens land in the VAM secrets vault; tool calls use them. |
| Refresh owner | `vamd`, triggered by HTTP 401 on a tool call. Refresh token stored in vault. Retry once. Connector bindings: also before swap (amended 2026-11-07). |
| Callback | Custom URL scheme `squrell://oauth` handled by the Tauri app. |
| Code exchange | Performed by the VAM app. App hands access + refresh tokens to `vamd`. |
| Client registration | A connector profile's client id first (amended 2026-11-07), then DCR, then CIMD when the AS advertises support and DCR is unavailable. |
| Fallback | Static header/token auth kept forever. |
| Early fix | Close the multi-header connect gap before any OAuth work. |

## Target architecture

### Roles

**The VAM app (Tauri) — OAuth user-agent and client**
- Probes the MCP server, reads `WWW-Authenticate` / Protected Resource
  Metadata, fetches AS metadata (RFC 8414 / OIDC discovery).
- Registers the client (DCR, else CIMD, else asks user for a pre-registered
  client id).
- Generates PKCE verifier/challenge (S256) and `state`; opens the system
  browser.
- Handles `squrell://oauth?code=…&state=…`, validates `state`, exchanges the
  code at the token endpoint with `resource` set to the MCP server URL.
- Calls `vamd` to upsert the connection (auth mode + OAuth metadata) and the
  tokens. Discards tokens from app memory afterwards.

**vamd — token vault, refresher, MCP HTTP client**
- Persists connection `authMode` and OAuth metadata.
- Stores access token, refresh token, and (if any) client credentials as vault
  Egress secrets bound to the connection.
- `invoke_mcp_tool → resolve_mcp_headers → McpClient` attaches
  `Authorization: Bearer <access>` for OAuth connections, or the configured
  headers for static connections.
- On 401: refresh, rotate bindings, retry once; on refresh failure, mark the
  connection `needs_reauth` and surface a structured error.

**Authorization server / MCP server** — external; unchanged.

### Hotspots

- vam: `mcp_registry.rs`, `api/mcp_connections.rs`, `mcp_client.rs`,
  `host_tools.rs`, `secrets.rs`.
- The VAM app: `McpConnectionInspector`, `types/vam.ts`, `useVamInspectors`, Tauri
  `connect_mcp` command, Tauri deep-link configuration.

## High-level sequences

### Connect with OAuth

```text
User          the VAM app (Tauri)              Browser / AS             MCP server        vamd
 |  Connect ->     |                            |                       |                |
 |                 |-- unauthenticated probe ---------------------------->|                |
 |                 |<-- 401 + WWW-Authenticate (resource_metadata) ------|                |
 |                 |-- GET protected-resource metadata (RFC 9728) ------>|                |
 |                 |-- GET AS metadata ------------>|                    |                |
 |                 |-- DCR register (or CIMD) ----->|                    |                |
 |                 |<-- client_id ------------------|                    |                |
 |                 |-- open authorize URL (PKCE S256, state, resource) ->|                |
 |  sign in/consent ------------------------------->|                    |                |
 |                 |<-- squrell://oauth?code&state -|                    |                |
 |                 |-- token exchange (code, verifier, resource) ->|     |                |
 |                 |<-- access, refresh, expires_in -|                   |                |
 |                 |-- PUT connection (authMode=oauth, metadata) ------------------------->|
 |                 |-- PUT tokens (access, refresh, expires_at) -------------------------->| vault
 |                 |-- snapshot / list tools ---------------------------------------------->|
 |                 |                                                     |<-- Bearer ----|
 |<- connected ----|                                                                       |
```

### Tool call with refresh

```text
Agent loop -> vamd invoke_mcp_tool
  resolve_mcp_headers: read access binding from vault -> Authorization: Bearer
  McpClient -> MCP server
    200 -> return result
    401 -> (single-flight per connection)
           read refresh binding + token_endpoint + client_id
           POST token endpoint grant_type=refresh_token, resource=<mcp url>
             ok   -> write new access (+ new refresh if rotated), update expires_at
                     retry original call once -> return result or error
             fail -> mark connection needs_reauth, return auth error to agent
```

## Data model additions (conceptual)

**Connection record**
- `authMode`: `static` | `oauth` (default `static`; existing rows migrate as
  `static`).
- `headerBindings`: list of `{ name, bindingId }` for static mode.
- `oauth` (only when `authMode = oauth`):
  - `resource` — canonical MCP server URI used as RFC 8707 resource.
  - `authorizationServer` — issuer URL.
  - `tokenEndpoint`, `revocationEndpoint` (optional).
  - `clientId` and `registrationMethod`: `dcr` | `cimd` | `manual`.
  - `scopes` granted.
  - `expiresAt` for the current access token.
  - `status`: `connected` | `needs_reauth`.
  - `lastRefreshAt`, `lastRefreshError` (sanitized, no token material).

**Vault bindings (Egress secrets, host-pinned to the MCP server / AS host)**
- Existing: `{connectionId}/auth-header`, `{connectionId}/header/{name}`.
- New:
  - `{connectionId}/oauth/access-token` — pinned to the MCP server host.
  - `{connectionId}/oauth/refresh-token` — pinned to the token endpoint host.
  - `{connectionId}/oauth/client-secret` — only if DCR returns one; pinned to
    the token endpoint host. Normally absent (public client).

`clientId` is not secret and lives in connection metadata, not the vault.

## PR bases

- vam: all implementation PRs target `feat/mcp-oauth`, not `main` and not
  `feat/improving-the-vam`.
  Marker: `[repo=squrell-inc/VAM] [branch=feat/mcp-oauth]`
- The VAM app: all implementation PRs target `feat/mcp-oauth`, cut from the current
  admin UI line (`feature/user-admin-ui`) so connection inspector work is
  present.
  Marker: `[repo=the app repo] [branch=feat/mcp-oauth]`

## Phases

Each phase is one small story.

### vam (`squrell-inc/VAM`)

**V0 — Wire multi-header connect API**
- `ConnectMcpRequest` accepts `headers: [{ name, value }]`.
- Each header becomes a vault binding `{connectionId}/header/{name}`;
  `headerBindings` persisted on the connection.
- `secretValue` remains shorthand for a single `Authorization` header
  (`{connectionId}/auth-header`).
- `resolve_mcp_headers` emits all bound headers. Reject reserved/hop-by-hop
  header names and duplicates.
- Tests: multi-header round trip; shorthand compatibility; old rows still work.

**V1 — authMode, OAuth metadata, token upsert API**
- Add `authMode` and `oauth` metadata to the connection model and API.
- New endpoint to upsert tokens for a connection (access, refresh,
  `expires_in`/`expires_at`), writing vault bindings; returns no token
  material.
- For OAuth connections, `resolve_mcp_headers` emits `Authorization: Bearer`
  from the access binding.
- No browser, no code exchange in `vamd`.

**V2 — Refresh-on-401**
- In the `invoke_mcp_tool` / `McpClient` path, on 401 for an OAuth
  connection: refresh via token endpoint with `resource`, rotate access
  binding, rotate refresh binding if a new one is returned, update
  `expiresAt`, retry once.
- Single-flight refresh per connection to avoid concurrent refresh races and
  refresh-token reuse.
- On failure set `status = needs_reauth` and return a structured auth error.
- Static connections: 401 is returned as-is (no refresh).

**V3 — Discovery helpers (optional) and DCR placement**
- Decision: **the app performs discovery, DCR, and code exchange; `vamd` only
  stores results.** This matches the locked callback decision.
- Optional: a read-only helper that probes the MCP URL and returns the parsed
  `WWW-Authenticate` / Protected Resource Metadata, if the app cannot reach
  the server directly (e.g. vamd-only network egress). Ship only if A2 proves
  it necessary.
- **V3.1 outcome:** gate skipped. No probe route. The connect sequence and the `vamd` MCP API show the app reaching the MCP host for discovery; this repo has no evidence that vamd-only egress is required. Reopen only if a later story documents that wall.
- A vamd-side proxy for confidential registration is out of scope by default;
  add only if a target AS mandates a client secret and the team accepts
  storing it in the vault.

**V4 — Disconnect/revoke and status**
- Disconnect deletes all OAuth bindings and metadata; best-effort RFC 7009
  revocation of refresh token when `revocationEndpoint` is known.
- Expose `status` (`connected` / `needs_reauth`), `expiresAt`, and sanitized
  `lastRefreshError` in connection list/snapshot responses.
- Switching a connection between `static` and `oauth` cleans up the old
  mode's bindings.

### the VAM app (the app repo)

**A0 — Align connect request with multi-header backend** (depends V0)
- `types/vam.ts` and Tauri `connect_mcp` send `headers[]`; keep the single
  token shorthand path.
- Inspector shows saved header names (never values).

**A1 — Auth mode UI and deep-link handler** (depends V1 for save)
- `McpConnectionInspector`: choose Static headers vs OAuth.
- Register `squrell://oauth` in Tauri; route callbacks to a pending-flow store
  keyed by `state`; reject unknown or expired `state`.

**A2 — OAuth connect flow** (depends A1, V1)
- Discovery: probe → RFC 9728 metadata → AS metadata.
- Registration: DCR first; CIMD if DCR unsupported and AS advertises it;
  otherwise prompt for a manual client id.
- PKCE S256, `state`, `resource` on authorize and token requests; open system
  browser; exchange code; call vamd connect + token upsert; refresh snapshot.
- Abort cleanly if the AS does not support S256.

**A3 — Connection status UX** (depends V2, V4)
- Show connected / expired / needs reauth; "Reconnect with OAuth" CTA reruns
  A2 against the existing connection.
- Map vamd structured auth errors from tool calls to the reauth CTA.

**A4 — CIMD fallback hosting and static path** (depends A2)
- Decide and configure the client metadata document URL (HTTPS, stable,
  Squrell-hosted) used as `client_id` for CIMD.
- Confirm static header/token path remains first-class in the UI.

## Cross-repo dependency graph

```text
V0 ──► A0
V0 ──► V1 ──► A1 ──► A2 ──► A4
        │             ▲
        └─────────────┘   (A2 needs V1 token upsert)
V1 ──► V2 ──┐
V1 ──► V4 ──┴──► A3
V3 (optional) ··► A2   (only if app cannot reach MCP server directly)
```

- V0 and A0 can ship as an independent bugfix slice before any OAuth work.
- V2 and V4 can proceed in parallel with A1/A2.

## Issue index

### P0 — Multi-header
- [V0.1 Accept headers[] on PUT /v1/mcp/connections]
- V0.2 Tests for multi-header connect, resolve, and disconnect (blocked by V0.1)
- [A0.1 App sends headers[] and displays saved bindings] (blocked by V0.1)


### V1 — vamd OAuth data model
- V1.1 authMode + OAuth metadata (blocked by V0.1)
- V1.2 Token upsert API (blocked by V1.1)
- V1.3 OAuth status on list/snapshot (blocked by V1.1)
- V1.4 Tests for metadata + upsert (blocked by V1.1)
- V1.5 MCP connections OpenAPI + runtime docs (blocked by V1.2)

### V2 — Refresh-on-401
- V2.1 Refresh on 401 + retry once (blocked by V1.2)
- V2.2 Concurrent refresh lock + rotation (blocked by V2.1)
- V2.3 needs_reauth on refresh failure (blocked by V2.1)

### V3 — Discovery helpers (optional)
- V3.1 Optional vamd PRM probe (skipped: app owns discovery; no vamd probe)

### V4 — Lifecycle
- V4.1 Disconnect + best-effort revoke (blocked by V1.2)
- V4.2 OAuth status for reconnect UX (blocked by V1.3)

### A1 — App shell for OAuth
- A1.1 Auth-mode UI static vs OAuth (blocked by A0.1)
- A1.2 squrell://oauth deep-link + state

### A2 — Full OAuth connect
- A2.1 Discovery + DCR-first (blocked by A1.1)
- A2.2 PKCE + browser + code exchange (blocked by A2.1)
- A2.3 Hand tokens to vamd (blocked by A2.1)
- A2.4 CIMD fallback (blocked by A2.1)

### A3 — Connection status UX
- A3.1 Surface OAuth status in inspector/list (blocked by V4.2)
- A3.2 Reconnect with OAuth CTA (blocked by A3.1)
- A3.3 Map vamd auth errors to reauth CTA (blocked by A3.1)

### A4 — CIMD hosting + static path
- A4.1 Host Squrell CIMD client metadata document (blocked by A2.4)
- A4.2 Wire production CIMD URL into app defaults (blocked by A4.1)
- A4.3 Keep static header/token path first-class (blocked by A1.1)

## Open questions (deferred)

- **Org vs user OAuth sharing.** Is an OAuth connection per user, or can an
  org admin connect once for all members? Affects vault scoping and Cedar.
- **First providers to test.** Pick 2–3 real OAuth MCP servers (one with DCR,
  one CIMD-only if available) as acceptance targets for A2/V2.
- **Session restart.** Does a running agent session need restart to pick up a
  newly connected OAuth MCP server's tools, as static connect does today?
- **Proactive refresh.** Whether to refresh on `expiresAt` before a call, in
  addition to 401.
- **Scope step-up.** Handling `insufficient_scope` 403 challenges (likely
  routed to the reauth CTA with expanded scopes).
- **CLI MCP OAuth.** Deferred until after A2. Prefer a thin later `vam mcp oauth init` that opens/prints a handoff (or app deep link) and polls connection status — like `gh` waiting — without CLI DCR or authorization-code exchange (app remains the OAuth client).

## Implementer constraints

- Never log, trace, or return token values, authorization codes, PKCE
  verifiers, or client secrets. Sanitize AS error bodies before persisting.
- Tokens live only in the VAM secrets vault as Egress secrets; no tokens in
  connection metadata, the database outside the vault, app local storage, or
  config files.
- Egress host pinning: access token pinned to the MCP server host; refresh
  token and client secret pinned to the token endpoint host.
- PKCE S256 is required; refuse to proceed if unsupported. Validate `state` on
  every callback.
- Always send the RFC 8707 `resource` parameter set to the canonical MCP server
  URI on authorize, token, and refresh requests.
- No token passthrough / confused deputy: only send a token to the resource it
  was issued for; never forward an inbound token upstream.
- Bearer token goes in the `Authorization` header on every MCP HTTP request,
  never in query strings.
- Refresh is retry-once and single-flight per connection; no retry loops.
- Static header auth must keep working unchanged for existing connections.
- Do not commit `graft/` or other local scratch artifacts.
- vam: add focused tests and run `cargo test` for modified crates plus
  `cargo check --workspace`.
- The VAM app: add focused tests for request shaping and deep-link `state`
  handling; run the app's lint/typecheck.
- Update `docs/api/openapi.yaml` and Host Control API docs when MCP connection
  routes/fields change (per story ACs); V1.5 finishes the MCP connections OpenAPI
  package and rewrites `runtime/mcp.md`.
- CLI OAuth is deferred until after A2; do not add CLI DCR or code exchange. If a CLI path is added later, keep it to poll/handoff only.
