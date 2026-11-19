---
title: Tools
description: The built-in tools, how a bad call is answered, custom tools run in the guest, host tools and their size bound, and MCP connections.
status: shipped
proves:
  - thread-history-lists-tool-results
  - tool-call-first-try-saves-a-draft
  - invalid-tool-arguments-return-a-typed-error
  - host-tool-output-fits-frame
  - progress-shows-while-the-task-runs
  - question-waits-for-an-answer-and-resumes
  - crates/vam-orchestrator/tests/mcp_connections_tests.rs
  - skill-cannot-send-a-secret-to-an-unpinned-host
  - guest-cannot-use-an-mcp-token-placeholder
  - broker-cannot-be-given-an-mcp-credential
  - crates/vam-orchestrator/tests/mcp_credentials_stay_host_side.rs
  - admin-supplied-client-lets-members-connect-without-the-secret-leaving-the-node
  - rotating-refresh-token-is-spent-once-when-two-tasks-call-at-once
  - admin-corrects-the-client-secret-and-members-stay-connected
---

A tool call goes from the model to the guest loop, then to the host for a policy check, then runs either
in the guest or on the host. The policy is in [Permissions](/security/permissions/).

## Built-in tools

These run in the guest, against the workspace (`crates/vam-runtime/src/primitives.rs:268-274,640-680`):

| Tool | Does |
|---|---|
| `read_file`, `write_file`, `edit_file`, `delete_file` | file access under the workspace; writes to protected paths are refused (`primitives.rs:139,220`) |
| `glob`, `grep` | search file names and file contents |
| `run` | run a shell command through the process supervisor (below) |
| `todo_write` | keep the todo list in `.vam/agent/todos.json` |
| `load_skill` | read a skill ([Agent loop](/runtime/agent/)) |

The loop adds `task_complete` when a run asks for an explicit claim (`agent.rs:748`) and `start_playbook` while a
playbook may be started. Three more come from the talk-back path: `post_message`, `show` (only with the `human.jig`
switch) and `ask_user` ([Jig](/runtime/jig/), [Inbox and outbox](/runtime/inbox-outbox/)). A call that names none
of these goes to the host or to MCP.

Proved by `thread-history-lists-tool-results` (results appear in the thread) and
`tool-call-first-try-saves-a-draft`.

## A bad call

The guest checks the arguments against the tool's schema before anything runs (`crates/vam-spec/src/call_check.rs`).
Unparsable or non-object arguments are not turned into `{}`; a schema violation becomes a tool result that starts
`invalid arguments for` and names the field and the fix. Proved by `invalid-tool-arguments-return-a-typed-error`.
With the `adapter.resampleOnInvalid` switch the loop asks the model once more first.

## Custom tools and `run`

A skill can carry its own tools, and `run` starts shell commands. Both go through `execute_tool_subprocess`
(`src/exec.rs:336`). Before the command starts, in the child process (`src/exec.rs:630-711`), the guest:

1. clears the environment (`exec.rs:543`) and starts a new session;
2. sets limits for CPU time, address space, processes, open files, file size and core size;
3. applies a Landlock file-access policy (`src/landlock.rs`), which is built only for x86_64 Linux and falls back
   when the guest kernel does not support it;
4. drops to gid 100 and uid 1000 (`exec.rs:708,711`).

None of this is proved by a test or scenario. The page does not claim that a tool cannot read or write outside
the workspace, only that these calls are made.

## Host tools

Some tools run on the host and return their result over the port-52 channel. One reply must fit a vsock frame,
so everything a host tool returns (text, file names, files, the error text) stays under
`HOST_TOOL_FRAME_BUDGET_BYTES`, 60 KiB (`crates/vam-orchestrator/src/host_tools.rs:37`). A larger payload is
written to the workspace under `.vam/host-artifacts/` and the result names the file (`src/instruction.rs:326`).
Proved by `host-tool-output-fits-frame`.

### Secrets in an HTTP host tool

The `http_get` and `http_write` adapters attach a stored credential (an `auth-header` secret, or a
`base-url` that resolves a relative path) on the host, so the guest never holds it
(`crates/vam-orchestrator/src/host_tools_http.rs`). A secret is sent only to a host its vault row is
pinned to (`--allowed-destination`: an exact host, a `*.base` pattern, or `*`). A row with no pinned
destination is sent nowhere, so every `auth-header` secret an HTTP tool uses needs a pin. A
`base-url` with no pin is pinned to the host its own value names. A refusal is a
`secret_not_permitted` tool error, before any request exists. Proved by
`skill-cannot-send-a-secret-to-an-unpinned-host`. The adapter never reads an MCP connection
credential (vault kind `connection`), whatever a skill names; that is covered by the unit tests in
`host_tools_http.rs` and `mcp_credentials_stay_host_side.rs`.

## MCP connections

A remote MCP server is connected per user with `vam mcp connect`, `update`, `list`, `disconnect` and `refresh`
(`crates/vam-cli/src/cmd/mcp.rs`), or through `/v1/mcp/connections` (`api/mod.rs:196`). The host stores the
connection and its OAuth tokens, and refreshes them (`mcp_registry.rs`, `mcp_oauth_refresh.rs`). The tools of a
connection are offered to the model on the first turn. The test file covers what the host stores: the saved
connection, the token upsert and that a listing shows no secret
(`crates/vam-orchestrator/tests/mcp_connections_tests.rs`). Calls to a live MCP server are not proved.

### OAuth

A connection uses one of two auth modes: `static` (the default) or `oauth`.

- **Static.** One or more headers are stored per connection (`secretValue` is shorthand for a single
  `Authorization` header). Values are write-only: a listing never returns them.
- **OAuth.** The app is the OAuth client. It discovers the authorization server, registers a client
  (dynamic client registration), runs the authorization-code flow with PKCE (S256) and sends an RFC 8707
  `resource` for the MCP server, then hands the tokens to `vamd` with
  `PUT /v1/mcp/connections/:id/oauth/tokens`. `vamd` never opens a browser and never sees the
  authorization code. The discovery and browser steps live in the app, not in this repository.
  The one exception is a connector whose client the org admin supplied, with a secret
  (`clientSecret: org_vault`, below): there the node exchanges the code.

### The connector file for a company address (REST, `profile: "oauth"`)

A provider that gives every company its own host (`<company>.example.com`) and publishes no discovery metadata
is described in the connector file, not discovered. `provider.profile: "oauth"` carries fixed
`provider.endpoints { authorize, token, revoke? }` (`https`, port 443 at most, no query, fragment or user info) and
needs `client: "org"` with `clientSecret: "none"` (a secret with fixed endpoints is not available yet). A
top-level `tenant { name: "subdomain", suffix, label }` names the one parameter the admin will enter at Enable:
`{subdomain}` may stand only for the whole first label of a host, and every templated host (in `endpoints`,
`mcp.url` and `bindings[].hosts`) is exactly `{subdomain}.<suffix>`, where the suffix has at least two labels on a
public DNS name. A file with a template and no `tenant`, or the reverse, is refused. A marketplace connector may
declare 1 to 8 REST `bindings` (1 to 4 hosts each, `value: access_token`, `inject: bearer`) whose placeholder is
exactly `vamsec_<connector id>_<binding name>`; a skill's `egress[].host` may be `{subdomain}.<dns name>` and a
listing is refused when that is not the suffix of the connector it ships. The listing's capability summary shows
the admin the sign-in endpoints, the bindings, the company address (label and suffix) and the templated hosts.
Entering the address at Enable, signing in and attaching the member's token to these hosts are not yet built.

### The admin's client: the node exchanges the code

A connector with `provider.client: "org"` and `clientSecret: "org_vault"` uses the OAuth client its org admin
registered with the provider. The client id, the pinned authorization, token and revocation endpoints and the
secret are saved at Enable (`PUT /v1/connectors/{id}/node`); the secret is an org-scope `connection` vault row
pinned to the token endpoint's host. Discovery at Enable also pins how the secret authenticates
(RFC 6749 §2.3.1): `client_secret_post` when the provider's metadata lists it, else `client_secret_basic` when it
lists that or says nothing (RFC 8414 §2 makes it the default), and the save is refused (409) when it lists neither.
The method is used by the code exchange, the refresh and the revoke alike. The app cannot hold
that secret, so it does not exchange the code. It runs the browser sign-in with PKCE against the pinned
authorization endpoint and the admin's client id, then posts what it got to
`POST /v1/connectors/{id}/connection/oauth/code` with `{ code, codeVerifier, redirectUri }` (no endpoint and no
client field; any other field is a 400). `redirectUri` must be the app's own, `squrell://oauth` or
`http://127.0.0.1:<port>/<path>` with an explicit port other than 80 (a loopback URI with no port, or with the
default port 80, is refused). `vamd` posts the code, the verifier, the redirect URI, the client id, the
`resource` and the secret (in the body, or as HTTP Basic, as pinned) to the pinned token endpoint, over the same
DNS-pinned public address, no redirects and no proxy as a refresh (`mcp_oauth_refresh.rs`). The secret is read at
org scope only when that host is the one it is pinned to (`connector_client.rs`, `org_client_secret`). Once the
provider accepts the code, the member's connection (the connector and the node's client: client id and endpoints
are the node's), the refresh token and the access token are one transaction, which first checks that the admin's
client, the enable and the connector's server are still what the code was exchanged for. The tools are listed
next, and the member's connector entry is the answer.

The local checks (body, redirect URI, code and verifier syntax, the connector, the enable, the saved client and
secret) run before the request and write nothing; the enable is checked again once the member's credential gate is
held, before the secret is read (a connector turned off while the request waited is a 403 and nothing is sent).
Just before the code is sent, the node records the exchange under
the member, the connector and the request's `Idempotency-Key` (`connector_code_exchanges`, the key's SHA-256, kept a
week): a request repeated with the same key, because the first answer was lost, never presents the code to the
provider again (RFC 6749 §4.1.2: a provider refuses a reused code and may revoke the tokens the first exchange
issued). It is a 409 that is not retryable, whatever became of the first request; the app reads the connection
state (`GET /v1/connectors`) and starts a new sign-in, with a new key, if it is not connected. After the provider has accepted the code it cannot be used again,
so what can still fail says so: a client replaced (even replaced and then put back, judged by the client's
revision), or the connector turned off, while the provider was answering is a 409 and nothing is stored; a failure to save is a 502 that replaces nothing and says to sign in again; tools that
cannot be read leave the sign-in stored but the connection `needsReconnect`, with the notification open and parked
runs parked, until `POST /v1/mcp/connections/{id}/refresh?scope=user` lists them (the member's row is user-scoped;
an org admin who leaves out `scope` addresses the org scope and gets a 404). A listing that fails is recorded only while
the row is still the sign-in it was for (its URL and OAuth state, checked in the transaction that writes the mark):
when another refresh already adopted the tools, or an admin save marked the row, the failure is a 409 and the row is
left as it is. That recovery runs only while the connector is enabled: on a connector the admin turned off it is a
409 before the provider is contacted, the adoption checks the enable again in its commit, and the sign-in stays
pending until the connector is on again. The route renews the stored access token first when its recorded expiry has
passed; when the provider sent no `expires_in` there is none, so a token the MCP server refuses (HTTP 401) while the
tools are listed is refreshed once, under the credential gate, and the listing is tried once more with the renewed
token (never more than once per request). A provider that refuses the client
(`invalid_client`) is a 409 saying the client id or secret the admin entered was refused; a refused or
spent code is a 400; an unreachable provider is a 502 the app may retry; an answer with no refresh token is a 502 that is not
retryable: the provider must issue a refresh token (offline access) for the admin's client before the member signs
in again. When the admin saves a
corrected secret for the same client, a member whose refresh the provider had refused for the client
(`invalid_client`) is connected again; a revoked grant still needs a sign-in. A sign-in whose tools were still
pending when the provider refused the client goes back to pending, not to connected: the tools-refresh route then
renews its token with the corrected secret and adopts the tools, and only that connects it. A save that corrects the secret and
turns the connector off connects no one again: those members stay parked until the admin enables it again with the
same client. Turning the connector off and on again with no secret saved in between is not a correction (the
provider refused the secret that is still saved), so those members stay parked; the node keeps a marker on the
client for a secret saved while the connector was off, and only the enable that follows such a save clears it. No code, verifier,
secret or token is in any answer or error. For such a connector the app-path routes are closed:
`PUT /v1/mcp/connections/:id/oauth/tokens` answers 400 (use the code exchange), a connect that names another
token endpoint or client id is refused, and a `clientSecret` is refused for any connector-backed row. A refresh
and the revoke on disconnect use the node's endpoints, client id and the org secret, read at that moment (a
replaced secret is used by the next refresh); the revoke authenticates the way the provider's
`revocation_endpoint_auth_methods_supported` says (the same choice as for the token endpoint), with
`client_secret_basic` when the provider omits the field (RFC 8414 §2's default), and as the token endpoint does when
the list has nothing this node can use or the client was saved before the field was read. The revoke picks the
node's client by the grant's client id and token endpoint, not by the connector's current URL, so the row a
connector update left on its previous server (revoked once its replacement commits, or on disconnect) is revoked
with the org secret too when its grant is the admin's current client; a row whose client id or endpoint differs from the node's is
marked `needs_reauth` without a request, and `invalid_client` on a refresh marks it `needs_reauth` naming the
admin's client, unless the admin saved the client again while the provider was answering: then the refusal was
of a secret no longer saved and nothing is recorded for it. When that save only replaced the secret of the same
client (the client id and token endpoint unchanged, so the member's grant still belongs to the current client), the
next call refreshes with the saved secret (a tools-pending sign-in stays pending, and the tools-refresh route uses
it); a save that changed the client id or the token endpoint has already marked the row `needs_reauth`, as below,
and the member signs in again. A refresh the
provider accepts after the admin changed the client id or token endpoint stores nothing and leaves the member
`needs_reauth`; one accepted after a secret-only save is stored. A corrected secret reconnects a member
automatically only while their grant still belongs to the current client: the refusal was of this client id and
token endpoint, and no save changed either since. A save that changes the client id or the token endpoint replaces
the recoverable mark with "the admin changed the client", so the row stays `needs_reauth` and the member signs in
again, even if the admin later returns to the earlier client (A, then B, then A) and corrects its secret. A
parked row is not refreshed at all, and a refresh never clears a mark a save left. If the provider rotated the
refresh token and the node then fails to store the access token, the rotated refresh token is still kept (the
next call refreshes with it): it is written first and the later writes run in a savepoint, so this holds only while
the outer transaction still commits. If the whole transaction rolls back (the disk is full, an I/O error) after the
provider spent the old refresh token, neither token is stored and the member may need to sign in again. The client and its secret are read at one revision: an admin save between the two
reads sends nothing. Providers whose refresh tokens are single-use are safe: the refresh gate below makes
concurrent calls spend the token once.

`vamd` stores the access and refresh tokens as `connection` vault rows, each pinned to one host
(`mcp_registry.rs`, `store_mcp_oauth_tokens`): the access token to the MCP server host, the refresh token to the token
endpoint host. The host sends the access token as `Authorization: Bearer` on each MCP request and sends
a token only to its own audience; there is no token passthrough.

When an MCP call returns HTTP 401, `vamd` refreshes once and retries the call once. Concurrent calls on the same
connection share one refresh (a per-connection gate), so a refresh token is not spent twice. The
token endpoint request goes to a DNS-pinned public IP, with no redirects, and sends `resource` again
(`mcp_oauth_refresh.rs`). A definitive failure such as `invalid_grant` marks the connection `needs_reauth`: later
calls fail at once with an `mcp needs_reauth:` error, without contacting the MCP server, until the user
reconnects. A network failure records `lastRefreshError` but does not mark `needs_reauth`. There is no
background refresh for plain MCP connections.

Not built yet: refresh before expiry for plain MCP connections (only connector bindings refresh early).

### MCP credentials are host-only

Every row a connection writes (`{id}/auth-header`, `{id}/header/*`, `{id}/oauth/*`) has the vault
kind `connection`. The kind is reserved: the secrets API cannot write it, and a row cannot be
rewritten as another kind. Only the host uses these rows to call the MCP server. They are never
guest egress bindings: no `vamsec_` placeholder exists for them, and the MCP host and token endpoint
host do not enter the session's allowed egress domains because of them. An HSP broker manifest that
names one is refused at the call. Rows an earlier build stored as `egress` are reclassified when
`vamd` starts (`SecretsVault::initialize_connection_secret_kinds`).

A skill cannot call an MCP tool from guest code. A guest that guesses the placeholder a convention
would give (`vamsec_<id>_oauth_access_token`) sends a string no binding matches, and the MCP host is
not on its allowlist unless something else puts it there; the only path is the tool call, which goes
through the per-tool permission decision. Proved by `guest-cannot-use-an-mcp-token-placeholder` (the
placeholder is not substituted) and `broker-cannot-be-given-an-mcp-credential` (a broker whose
manifest names the token gets nothing), plus the tests in `mcp_credentials_stay_host_side.rs`.

### What proves the admin's client

- The secret is in no connector, MCP or secrets listing (admin, member or operator), no exchange or save answer,
  no thread, event, receipt or workspace file the scenarios read: `admin-supplied-client-is-set-only-by-an-org-admin-and-never-read-back`
  and `admin-supplied-client-lets-members-connect-without-the-secret-leaving-the-node` (`secret-absent`), and the route
  tests in `api/connectors.rs` (`assert_no_value` over every answer and listing).
- It is sent only to the pinned token host and the revocation endpoint on that host, never to another host or a row
  not pinned to exactly that host: `connector_client.rs` (`the_secret_is_read_only_for_the_pinned_token_host`,
  `a_row_not_pinned_to_exactly_the_token_host_is_never_read`) and `mcp_oauth_refresh.rs` (the exchange, refresh and
  revoke tests, `the_exchange_never_leaves_the_public_https_rules`, the `client_secret_basic` wire test).
- A refresh token is spent once under overlapping calls, and the rotated one is stored first:
  `rotating-refresh-token-is-spent-once-when-two-tasks-call-at-once` and the gate tests in `host_tools.rs`.
- A corrected or rotated secret keeps members connected, and a changed client id does not:
  `admin-corrects-the-client-secret-and-members-stay-connected`, and for a client the provider refused,
  `a_corrected_secret_for_the_same_client_brings_back_members_a_refused_client_parked` and
  `a_corrected_secret_saved_with_a_disable_keeps_members_parked_until_the_enable` and
  `a_refused_client_on_a_pending_sign_in_goes_back_to_pending_after_a_corrected_secret` in `api/connectors.rs`, and
  `a_plain_disable_and_enable_leaves_a_refused_client_parked` in `connector_client.rs`.
- The commit rules (one transaction, a client replaced mid-exchange stores nothing, a moved connector's old row goes
  only after the new grant, a disconnect during `tools/list` is not undone, a stored sign-in whose tools cannot be read
  is retried by the tools-refresh route): `api/connectors.rs` route tests
  `an_exchange_for_a_client_the_admin_replaced_meanwhile_stores_nothing`,
  `a_client_changed_and_changed_back_while_the_provider_answers_stores_nothing` (the commit's revision check, with
  the provider's answer held),
  `a_save_that_fails_after_the_code_was_spent_replaces_nothing_and_says_to_sign_in_again`,
  `a_row_of_a_moved_connector_is_replaced_only_once_the_new_grant_is_valid`,
  `a_disconnect_while_tools_are_listed_does_not_bring_the_row_back` and
  `a_sign_in_whose_tools_cannot_be_read_stays_retryable_and_is_not_announced`,
  `a_failed_listing_never_marks_a_row_another_request_already_moved_on`,
  `a_pending_sign_in_is_not_recovered_while_the_connector_is_off` and
  `a_pending_sign_in_whose_token_the_server_refuses_is_refreshed_once_and_adopted`; two admins' saves racing is
  `a_save_planned_before_another_admins_save_commits_nothing` in `connector_client.rs`.

## Not proved

- The confinement of tool processes (Landlock, uid 1000, limits).
- That the DNS pin, the no-redirect rule and the no-proxy rule hold against a real resolver and network: the tests use
  loopback stand-ins for the hosts, and the real-tenant run (CN-5.7) is the check.
- That a refresh token the provider spends but `vamd` fails to store is recoverable: the member signs in again.
- Calls to a live MCP server and the token refresh against a real provider (the exchange and refresh with the admin's client are proved against a mock provider).

## Design notes

- `vamd` is not the OAuth client because it has no browser and no loopback listener. Keeping the
  authorization code in the app means the host holds only tokens, and the app holds none after the hand-off.
  Where the admin supplied a client with a secret, the secret cannot pass through the app, so the app hands
  `vamd` the code and `vamd` makes the exchange.
- Static header auth stays as a permanent mode: not every server speaks OAuth, and a pasted token is the
  only option for those.
