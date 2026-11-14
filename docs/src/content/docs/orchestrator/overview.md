---
title: Host daemon
description: What vamd does at start, which settings change it, and what it does on a restart.
status: shipped
proves:
  - vamd-refuses-plain-http-by-default
  - generated-id-and-email-login-survive-a-restart
  - crates/vam-orchestrator/tests/vamd_username_clash.rs
  - plain-chat-gets-a-reply
  - follow-up-continues-after-vamd-restart
  - work-survives-vamd-restart-mid-task
  - crates/vam-orchestrator/tests/tls_refusals.rs
  - crates/vam-orchestrator/src/ttl.rs::bare_session_is_idle_only_after_the_ttl
  - crates/vam-orchestrator/src/ttl.rs::sweep_ends_idle_sessions_once_and_survives_a_restart
  - crates/vam-orchestrator/src/daemon.rs::vm_node_names_every_missing_variable
  - crates/vam-orchestrator/src/daemon.rs::session_idle_ttl_default_off_and_invalid
---

`vamd` is the one host process of a node. It serves the HTTP API, keeps the durable state in
SQLite, starts and ends the sessions, and signs the receipts. The code is in
`crates/vam-orchestrator`; the binary is `src/bin/vamd.rs`.

## Start order

| Step | What happens | Code |
|---|---|---|
| 1 | The API transport is chosen. TLS is the default. A bad or half-set certificate pair stops `vamd`; it never falls back to plain HTTP. | `bin/vamd.rs:84-106`, `tls.rs` |
| 2 | The node's signing keys are loaded or created. A key file that is loose or unreadable stops `vamd`. | `bin/vamd.rs:109-119` |
| 3 | Startup reconciliation runs against the state database. | `bin/vamd.rs:447`, `reconcile.rs:107` |
| 4 | Receipts missing for finished runs are issued. | `bin/vamd.rs:452` |
| 5 | The session manager loop, the HTTP server, the telemetry gRPC server and the sweepers start. | `bin/vamd.rs:559-704` |

## State and listen address

The state database is set by `VAMD_DB_PATH` (`daemon.rs:364`) and the listen address by `VAMD_LISTEN`.
A node that starts cleanly answers a plain chat (`plain-chat-gets-a-reply`).

## TLS

The API is served over TLS. With no certificate given, `vamd` mints a self-signed one on first
start and keeps it under the state directory (`tls.rs:6`). `VAMD_TLS_CERT` and `VAMD_TLS_KEY` give
your own pair; `VAMD_TLS_SANS` adds names to the self-signed one (`daemon.rs:110`). Plain HTTP
needs `--insecure-dev` or `VAMD_INSECURE_DEV=1` (`daemon.rs:149-154`). Proved by
`vamd-refuses-plain-http-by-default` and `tls_refusals.rs`.

## VM nodes need three paths

A VM node refuses to start unless `VAMD_FIRECRACKER_BIN` and `VAM_ROOTFS_PATH` are set, and
`VAMD_JAILER_BIN` while the jailer is on (`daemon.rs:242-244`). The error names every missing
variable (test `vm_node_names_every_missing_variable`). A process-mode node does not need them.

## Readiness and draining

`GET /v1/health` reports capacity and `draining` (`api/mod.rs:446-471`). On SIGTERM or Ctrl-C
`vamd` sets `draining`, after which mutating requests are refused and readiness is false
(`bin/vamd.rs:731-752`, `api/mod.rs:298,341`).

## Restart

State lives in SQLite, so a restart keeps sessions, runs, approvals and threads. Startup
reconciliation matches the stored sessions to the processes found on the host (`reconcile.rs`).
A follow-up on a thread continues after a restart, and a task that was running when `vamd`
restarted survives: scenarios `follow-up-continues-after-vamd-restart` and
`work-survives-vamd-restart-mid-task`.

## Offline user commands

Two flags change a user from the host. Run them with `vamd` stopped:

- `vamd --reset-password <username>` sets a password (the new one comes from `VAMD_RESET_PASSWORD`; it is never logged). It takes a user id or a login name in any case.
- `vamd --rename-user <user_id> <new_username>` renames a user, checks the new name by the login-name rules, prints the old and new name and exits. It is the fix when two usernames differ only in letter case: until then `vamd` refuses to serve or `--init`, lists the clashing users with their ids, and exits 1. It works on a database that has clashes. See [Users and roles](/getting-started/users-and-roles/).

## Idle sessions

A session with no live run, no undecided approval and no activity for
`VAMD_SESSION_IDLE_TTL_SECS` is ended through the normal terminate path. The default is 86400
(24 h); `off` turns the sweep off with a warning; `0` or a non-number falls back to 24 h with a
warning (`daemon.rs:206-232`, `ttl.rs:1-19`). The clock is read from stored times, so a restart
does not reset it (test `sweep_ends_idle_sessions_once_and_survives_a_restart`).

## Example seeds

`VAMD_EXAMPLES_DIR` (older name `VAM_EXAMPLES_DIR`) names the directory the node seeds example
agents from (`manager.rs:176-180`).

## Settings that matter

| Setting | Effect | Code |
|---|---|---|
| `VAMD_SECRETS_MASTER_KEY` | Master key for stored secrets. | `secrets.rs:17` |
| `VAMD_RATE_LIMIT` and `VAMD_RATE_LIMIT_*` | API rate limits, on by default. See [Host API](/orchestrator/api/). | `daemon.rs:260-266` |
| `VAMD_SESSION_IDLE_TTL_SECS` | Idle session limit. | `daemon.rs:206` |
| `VAMD_MARKETPLACE_DOMAIN` / `--marketplace-domain` | The Squrell marketplace domain. Default `hub.squrell.cloud`; set it only on a staging node. Callers cannot create, publish, release or delete artifacts under it; only the catalog import writes it, from the preconfigured Squrell marketplace. Always returned as `marketplaceDomain` on `GET /v1/me`. | `daemon.rs:109`, `daemon.rs:299-310` |
| `VAMD_GIT_BIN` | Absolute path of the `git` binary vamd uses to read Git marketplaces (default: the first `git` on `PATH`). `git` is needed by marketplaces only: without it `GET /v1/marketplaces` reports `gitAvailable: false`, every read fails with a clear `lastError`, and nothing else is affected. vamd runs it with a cleared environment and an empty `HOME`, `GIT_CONFIG_NOSYSTEM=1`, `GIT_CONFIG_GLOBAL=/dev/null`, `GIT_TERMINAL_PROMPT=0`, hooks disabled, https only through a loopback guard proxy that dials public addresses on port 443 only, redirects off, no submodules, and it never checks anything out. The cache lives under `<state dir>/marketplaces/`. | `marketplace/mod.rs:67`, `marketplace/mod.rs:111` |
| `VAMD_MARKETPLACE_REFRESH_SECS` | How often each enabled marketplace is read on its own. Default 21600 (six hours); at least 300; `0` or `off` for on-demand only. Each marketplace keeps its own last-checked time and error. A read only reads: it never installs. | `marketplace/mod.rs:68`, `marketplace/mod.rs:82` |

Process-guest builds (test only) add `VAMD_PROCESS_*` seams: `VAMD_PROCESS_CONNECTOR_ENDPOINTS` maps
connector endpoints to local mocks, and `VAMD_PROCESS_MARKETPLACE_URLS` maps a marketplace's https URL to a
local `file://` bare repository (`process_mode.rs:30-36`). A default `vamd` refuses every `VAMD_PROCESS_*`
variable and has no `file://` transport; the release gate checks both.

## Not proved here

Draining, and the refusal of mutating requests while draining, are read from the code; no
scenario or test drives them.
