---
title: Host API
description: The HTTP API of vamd, its auth, rate limits, paging, errors and the OpenAPI file.
status: shipped
proves:
  - vamd-refuses-plain-http-by-default
  - api-rate-limits-a-flood
  - node-reports-the-isolation-it-runs
  - oversize-response-with-a-secret-never-reaches-the-guest
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
