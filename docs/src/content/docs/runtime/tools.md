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

## Not proved

- The confinement of tool processes (Landlock, uid 1000, limits).
- Calls to a live MCP server and the token refresh against a real provider.

## Design notes

- `vamd` is not the OAuth client because it has no browser and no loopback listener. Keeping the
  authorization code in the app means the host holds only tokens, and the app holds none after the hand-off.
- Static header auth stays as a permanent mode: not every server speaks OAuth, and a pasted token is the
  only option for those.
