---
title: Users, keys and ownership
description: Users and roles, login, API keys, owner stamps on every row, and what one user cannot see of another's.
status: shipped
proves:
  - member-chats-with-a-released-agent
  - crates/vam-orchestrator/tests/api_tests.rs
  - crates/vam-orchestrator/src/secrets.rs::import_env_secrets_are_org_scope_and_ignore_user_rows
  - crates/vam-orchestrator/tests/login_tests.rs
  - crates/vam-orchestrator/tests/me_tests.rs
  - crates/vam-orchestrator/tests/node_login_probe_tests.rs
  - crates/vam-orchestrator/tests/isolation_coverage_map_tests.rs
---

Every durable row on the host carries an `owner_user_id`. Reads and writes filter on it, so one
code path serves one user and many. This page says how users, roles and keys work and what
the owner filter covers.

## Users and roles

The database starts with two users: `operator` (the bootstrap key) and `local` (the implicit
owner when there is one user). Each user has one role: `operator`, `org_admin`, `publisher` or
`member`; member is the default. A role sets the scopes of the user's sessions and default keys.
The first admin is created from the command line, with `vamd --bootstrap-admin` or
`VAMD_BOOTSTRAP_ADMIN_PASSWORD`; there is no sign-up route and no OIDC. For the steps, read
[Users and roles](/getting-started/users-and-roles/).

## Login and keys

| Call | Route | Notes |
|---|---|---|
| Login | `POST /v1/login`, `/v1/login/refresh`, `/v1/logout` | Username and password give a short session token and a refresh token (`api/mod.rs:98-100`). |
| Who am I | `GET /v1/me` | Any authenticated caller gets `userId`, `username`, `role` and the bearer's scopes. A deactivated user or revoked key is a 401 (`api/me.rs`). |
| API keys | `POST`, `GET /v1/keys`, `DELETE /v1/keys/{id}` | Keys bind to a user when minted. List and revoke are per user; `keys:admin` sees every `http` key. A grant issued to a broker (`?holder=broker\|all`) is listed and revoked by its owner only, and an HTTP bearer never accepts one (401). |
| Users | `POST`, `GET /v1/users`, `deactivate`, `password`, `keys` under `/v1/users/{id}` | Scope `users:admin`. A password reset revokes session tokens and keeps API keys; deactivate revokes both. |
| Publish grant | `DELETE /v1/users/{id}/publish-grant` | Moves a publisher back to member. |
| Reassign | `POST /v1/catalog/reassign-owner` | Moves catalog ownership; released versions stay released. |

Routes: `api/mod.rs:98-126`. Proved by `login_tests.rs`, `me_tests.rs` and
`node_login_probe_tests.rs`.

## What the owner filter covers

Sessions, runs, events, approvals, trajectories and API keys are listed and read by owner. A
get for another user's row is a 404, the same as a missing row, and pause, resume, terminate and
cancel answer the same way. Idempotency keys are unique per user. Workspace ids are built by the
host as `{owner}--{agent}--{name}`; the guest supplies only the name and cannot choose the
prefix. Secrets are stored by `(scope, owner_user_id, binding_id)` (`secrets.rs`), and there is
no fallback from a user-scope secret to an org-scope one. The two-user walk in
`isolation_coverage_map_tests.rs` checks that Alice cannot list, read or change Bob's rows.

Secrets imported from the environment at start (`VAMD_IMPORT_ENV_SECRETS`, `process_mode.rs:28`)
are org-scope, and user rows are ignored (test `import_env_secrets_are_org_scope_and_ignore_user_rows`).

## Not covered

- Approvals have one approver, the run's owner. There are no reviewer roles or delegation.
- Session egress for in-process adapters decrypts org-scope secrets only.
