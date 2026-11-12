---
title: Host API
description: The HTTP API of vamd, its auth, rate limits, paging, errors and the OpenAPI file.
status: shipped
proves:
  - vamd-refuses-plain-http-by-default
  - api-rate-limits-a-flood
  - node-reports-the-isolation-it-runs
  - oversize-response-with-a-secret-never-reaches-the-guest
  - member-changes-own-password-and-signs-in-with-the-new-one
  - password-change-signs-out-every-session-and-keeps-api-keys
  - wrong-current-password-is-refused-and-attempts-are-limited
  - api-key-cannot-change-a-password
  - new-password-equal-to-current-is-refused
  - password-change-needs-an-idempotency-key
  - crates/vam-orchestrator/tests/rate_limit_api_tests.rs
  - crates/vam-orchestrator/src/paging_store_tests.rs::sessions_page_newest_first_and_a_new_session_does_not_shift_the_next_page
  - crates/vam-orchestrator/src/paging_store_tests.rs::runs_page_newest_first_by_rowid_cursor
  - crates/vam-orchestrator/tests/api_tests.rs
  - crates/vam-orchestrator/tests/mcp_credentials_stay_host_side.rs::the_secrets_api_cannot_write_the_connection_kind
  - crates/vam-orchestrator/tests/mcp_credentials_stay_host_side.rs::connection_credential_cannot_be_rewritten_as_egress
  - crates/vam-network/src/broker.rs::a_connection_bound_destination_is_secret_bound_with_or_without_a_placeholder
---

`vamd` serves a JSON API under `/v1`, over TLS by default (see [Host daemon](/orchestrator/overview/)).
The routes are in `crates/vam-orchestrator/src/api/mod.rs:95-210`. The complete list, with
request and response shapes, is the OpenAPI file the node serves.

## OpenAPI and `/docs`

`GET /openapi.yaml` returns the file built into the binary (`api/mod.rs:75,95`; source
`docs/api/openapi.yaml`). `GET /docs` serves a page that renders it with a pinned Scalar script,
`@scalar/api-reference@1.74.0` from jsDelivr (`api/mod.rs:96,286`). `/openapi.yaml` needs no key
(`api/auth.rs:133`).

## Route groups

| Group | Routes |
|---|---|
| Identity | `/v1/login`, `/v1/login/refresh`, `/v1/logout`, `/v1/me`, `/v1/keys`, `/v1/users` |
| Catalog | `/v1/agents`, `/v1/skills`, `/v1/playbooks`, `/v1/policies`, `/v1/agent-checks` |
| Sessions | `/v1/sessions`, and `pause`, `resume`, `terminate`, `snapshots` under `/v1/sessions/{id}` |
| Runs | `/v1/runs`, and `messages`, `stop`, `cancel`, `events` under `/v1/runs/{id}` |
| Approvals | `/v1/approvals`, `/v1/approvals/{id}/decide` |
| Secrets | `/v1/secrets`, `/v1/org/secrets` |
| Node | `/v1/health`, `/v1/node` |

The run routes sit behind one wildcard, `/v1/runs/*rest` (`api/mod.rs:187`), and are told apart
by their last segment (`api/mod.rs:1612-1670`).

`GET /v1/keys` lists the caller's `http` keys (`keys:admin` lists every `http` key). `?holder=broker`
lists the grants issued to brokers and `?holder=all` both; a grant is listed to its owner only, for
every role, and any other `holder` is a 400 (`list_keys`, `api/mod.rs:420-440`).

## Health and node status

`GET /v1/health` and `GET /v1/node` report the node's capacity, draining flag and the isolation it
runs under (`api/mod.rs:446-481`). Proved by `node-reports-the-isolation-it-runs`.

## Auth

Calls carry a bearer: an API key or a session token from `/v1/login`. Calls that create or change
something need an `Idempotency-Key` header (`require_idempotency_key`, `api/auth.rs`). A
cross-user read is a 404, the same as a missing row. See
[Users, keys and ownership](/orchestrator/user-isolation/).

## Rate limits

Limits are on by default (`api/rate_limit.rs:41-62`):

| Limit | Per second | Burst |
|---|---|---|
| Any call, per key | 30 | 100 |
| Creating runs, per key | 2 | 20 |
| Auth attempts, per address | 5 | 20 |

A refused call is a 429 with a reason header (`rate_limit.rs:28,340`). `VAMD_RATE_LIMIT=off`
turns the limits off; the `VAMD_RATE_LIMIT_*` variables change a value, and zero or a non-number
keeps the default (`daemon.rs:260-290`). Proved by `api-rate-limits-a-flood` and
`rate_limit_api_tests.rs`.

## Paging

List calls take `pageSize` (default 50, at most 1000) and `pageToken` (`api/paging.rs:14-50`).
Sessions, runs and approvals page newest first by a cursor, so a new row does not shift the next
page (tests `sessions_page_newest_first_and_a_new_session_does_not_shift_the_next_page` and
`runs_page_newest_first_by_rowid_cursor`). A bad `pageSize` is a 400.

## Errors

Each error type maps to one status in `api/error.rs:40-234`: 400 bad request, 401 and 403 auth,
404 missing or foreign row, 409 conflict, 412 failed precondition, 502 upstream failure, 503
node cannot take the call, 504 a stop still settling.

## Change your own password

`POST /v1/me/password` lets the signed-in user set a new password for themselves. Any role may
call it, the operator included. Who may change which password is in
[Users and roles](/getting-started/users-and-roles/#who-can-change-a-password); the CLI is
[`vam passwd`](/getting-started/cli/#change-your-password-vam-passwd).

- **Auth.** It needs a password session (`vam login`). An API key is a 403, even the user's own
  (proved by `api-key-cannot-change-a-password`). The call needs an `Idempotency-Key` header, like
  every write (`password-change-needs-an-idempotency-key`).
- **Request.** `{"currentPassword": "…", "newPassword": "…"}`. Both are write-only: the node
  stores the new one as argon2id and never logs or returns either.
- **Response.** `200` with `{"user": {…}, "reloginRequired": true, "passwordUpdatedAt": "…"}`.
  `passwordUpdatedAt` is Unix seconds written as a JSON string, as `createdAt` is. A real answer,
  secrets and ids shortened:

  ```json
  {"user":{"userId":"admin","username":"admin","role":"org_admin","hasPassword":true,"createdAt":"1794…"},"reloginRequired":true,"passwordUpdatedAt":"1794…"}
  ```

- **Sessions end, keys stay.** Every session of the user is revoked, the calling one too, so the
  token you called with answers 401 afterwards. API keys are not touched
  (`password-change-signs-out-every-session-and-keeps-api-keys`). Sign in again with the new
  password (`member-changes-own-password-and-signs-in-with-the-new-one`).
- **Five wrong tries.** A wrong current password counts against a limit of five per user. One try
  comes back every 60 seconds. The limit is on even with `VAMD_RATE_LIMIT=off`. A correct password
  and a refused new password do not count (`wrong-current-password-is-refused-and-attempts-are-limited`).
  The count lives in the node's memory, so a restart clears it (not proved by a test).

| Status | `code` in the body | `message` | Cause |
|---|---|---|---|
| 400 | `InvalidArgument` | `Idempotency-Key header is required for mutating requests` | No key sent |
| 400 | `InvalidArgument` | `currentPassword is required` | Empty current password |
| 400 | `InvalidArgument` | `new password must differ from the current password` | Same password again (`new-password-equal-to-current-is-refused`) |
| 400 | `InvalidArgument` | `password must be non-empty`, `password exceeds 1024 bytes` | New password breaks a rule |
| 401 | `Unauthenticated` | | No bearer, or a revoked or expired session |
| 403 | `PermissionDenied` | `changing a password needs a password session (vam login); API keys cannot change a password` | The caller used an API key |
| 403 | `PermissionDenied` | `current password is incorrect` | Wrong current password; counts as one of the five |
| 429 | `RESOURCE_EXHAUSTED` | `too many wrong password attempts; retry after Ns` | The five tries are used |

The 429 carries `Retry-After` (seconds) and `X-VAM-Rate-Limited-Reason: password-attempt-rate`.
Its `code` is spelled `RESOURCE_EXHAUSTED`, like every other 429; the other codes are the error
type's name.

## Secret kinds

`PUT /v1/secrets` and `PUT /v1/org/secrets` write two kinds: `1` inference and `2` egress. A list
may also show `3`, connection: the credential of an MCP connection, written only by
`/v1/mcp/connections`. The secrets API refuses to write it, and a row of that kind cannot be
rewritten as egress (`SecretKind::from_proto`, `secrets.rs`). Proved by
`the_secrets_api_cannot_write_the_connection_kind` and
`connection_credential_cannot_be_rewritten_as_egress`.

## A response too big to scrub

A response from a host that holds a session secret is read through a scrub buffer (8 MiB by
default, `vam-network/src/proxy/mitm.rs:52`). A response over the buffer is refused with a 502
before any of its bytes reach the guest (`mitm.rs:38`). Proved by
`oversize-response-with-a-secret-never-reaches-the-guest`, which uses a 64 KiB buffer so the
bodies stay small. It proves the host proxy, not the guest's lack of a route.

A host that a connection binding is pinned to (a connector token the gate swaps in, for example
`graph.microsoft.com`) counts as holding a secret, with or without a placeholder in the request
(`BrokerGate::prepare_with_decision`). The unit test
`a_connection_bound_destination_is_secret_bound_with_or_without_a_placeholder` covers the flag; the
refusal itself is proved by the scenario above for a static binding; for a connection-bound host
only the flag is tested.

## Outcomes per agent version

`GET /v1/agents/{urn}/outcomes` lists the versions that have receipts; `?version=X.Y.Z|draft&k=N`
returns one aggregate: `runs` (succeeded, failed and timed out), `successes`, `failed`,
`timedOut`, the excluded `cancelled`, `expired` and `rejected`, `successRate`, `passK {k, value}`
(`C(successes, k) / C(runs, k)`, default k 5, `value` absent with a `note` when runs are fewer
than k), `tokensPerSuccess`, `turnsPerSuccess`, `approvalsPerRun`, the cost at the model maker's
list price (`costPerSuccessMicroUsd`, `priceSource`, `catalogVersion`, or `costMissing` naming
unpriced models) and `effects` summed over journaled receipts (the sums are absent, never 0,
when none is journaled). It reads the stored receipts as they are; nothing is rebuilt. Auth is the
`versions` route's: the draft is for its owner or a `users:admin` caller, a 404 for anyone else.
The answer holds no run or user id. The wire shape is in `proto/vam/v1/outcomes.proto`.

`GET /v1/agents/{urn}/outcomes/diff?version=V[&k=N]` compares V with the release before it and
returns `{base, candidate, changes}`. `candidate` is V's aggregate (`draft` is the runs at the
current draft digest, the content a release would freeze). `base` is the highest release below V
(for `draft`, the latest release) and is absent for a first release. `changes` holds
`{base, candidate, delta}` for `successRate`, `passK` (its value), `costPerSuccessMicroUsd`,
`tokensPerSuccess` and `approvalsPerRun`, each absent when either side lacks the value, plus
`duplicates` with whichever sides have it. Auth and visibility are the outcomes route's. The
release endpoint is unchanged.

## Not proved here

The status map in Errors is read from `api/error.rs`; `api_tests.rs` covers many codes but not
each one.
