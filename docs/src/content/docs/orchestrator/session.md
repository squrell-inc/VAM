---
title: Sessions and runs
description: What a session, a run, a thread and a workspace are on the host, and how stop, terminate and cancel differ.
status: shipped
proves:
  - thread-history-lists-tool-results
  - steer-changes-the-running-task
  - queued-message-runs-after-the-task
  - stop-ends-a-running-task-cleanly
  - stop-denies-a-pending-approval
  - oversize-response-with-a-secret-never-reaches-the-guest
  - crates/vam-orchestrator/tests/api_tests.rs
  - crates/vam-orchestrator/tests/workspace_pin_tests.rs
---

A **session** is the environment a task runs in: a VM, or a process in process mode. A **run**
is one task in a session. A **thread** is the conversation a run belongs to, and a **workspace**
is the files that the thread keeps between runs. The host stores all four in SQLite
(`crates/vam-orchestrator/src/workflow` store) and every row carries its owner.

## Session calls

| Call | Route | Code |
|---|---|---|
| Create, list | `POST`, `GET /v1/sessions` | `api/mod.rs:171` |
| Read | `GET /v1/sessions/{id}` | `api/mod.rs:175` |
| Pause, resume | `POST /v1/sessions/{id}/pause`, `resume` | `api/mod.rs:968,995` |
| Terminate | `POST /v1/sessions/{id}/terminate` | `api/mod.rs:1037` |
| Snapshot, restore | `POST /v1/sessions/{id}/snapshots`, `POST /v1/sessions/restore` | `api/mod.rs:1124,1172` |

Each call checks that the session belongs to the caller; another user's session is a 404.
Mutating calls need an `Idempotency-Key`. The general behaviour is proved by `api_tests.rs`.

## Runs

`POST /v1/runs` starts a run (`api/mod.rs:1208`). A run is over when its state is one of
`SUCCEEDED`, `FAILED`, `CANCELLED`, `TIMED_OUT`, `EXPIRED` or `REJECTED` (`receipt.rs:27-34`).
Runs page newest first (see [Host API](/orchestrator/api/)). The thread's history, with tool
results, is read back through the thread routes; proved by `thread-history-lists-tool-results`.

## A workspace pins its agent version

A workspace remembers the agent version of its first run, and later runs on it use that version
(`api/mod.rs:1292-1296`, `catalog_resolve.rs`). Proved by `workspace_pin_tests.rs`.

## Stop, terminate and cancel

| Call | What it ends | What it does to the work |
|---|---|---|
| `POST /v1/runs/{id}/stop` | One live task | Files roll back to the last finished step. A pending approval is rejected "stopped by user". Unsent steers and queued messages are discarded and returned. The run ends `FAILED`, reason `stopped` (`api/stop.rs:1-24`). |
| `POST /v1/runs/{id}/cancel` | A run by id | Sets the run `CANCELLED`, discards its inbox, releases its reservation and terminates its session (`api/mod.rs:1720-1735`). |
| `POST /v1/sessions/{id}/terminate` | The whole session | Ends the session and its VM or process. |

Proved: stop by `stop-ends-a-running-task-cleanly` and `stop-denies-a-pending-approval`. Cancel
and terminate are read from the code and covered by `api_tests.rs` only in part.

## Talking to a running task

`POST /v1/runs/{id}/messages` sends a steer or a queued message, and `POST /v1/runs` that names
the thread of a live task does the same (`api/inbox.rs:1-8`, `api/mod.rs:1614`). The rules, the
caps and the outbox are on [Inbox and Outbox](/runtime/inbox-outbox/). Proved by
`steer-changes-the-running-task` and `queued-message-runs-after-the-task`.

## Responses the proxy refuses

If a run's tool calls an origin that holds a session secret and the response is over the scrub
buffer, the proxy answers the guest with a 502 and none of the body reaches it. See
[Host API](/orchestrator/api/); proved by
`oversize-response-with-a-secret-never-reaches-the-guest`.
