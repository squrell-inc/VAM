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
  - crates/vam-orchestrator/tests/workspace_pin_tests.rs::member_can_pin_own_draft
  - member-builds-and-runs-own-draft-agent-from-a-released-skill
  - a-run-with-an-inline-agent-is-refused-and-writes-nothing
  - a-run-with-an-inline-playbook-is-refused-and-writes-nothing
  - a-publisher-saves-a-draft-agent-and-runs-it-by-ref
---

A **session** is the environment a task runs in: a VM, or a process in process mode. A **run**
is one task in a session. A **thread** is the conversation a run belongs to, and a **workspace**
is the files that the thread keeps between runs. The host stores all four in SQLite
(`crates/vam-orchestrator/src/workflow` store) and every row carries its owner.

## Session calls

| Call | Route | Code |
|---|---|---|
| Create, list | `POST`, `GET /v1/sessions` | `api/mod.rs:199` |
| Read | `GET /v1/sessions/{id}` | `api/mod.rs:210` |
| Pause, resume | `POST /v1/sessions/{id}/pause`, `resume` | `api/mod.rs:1122,1149` |
| Terminate | `POST /v1/sessions/{id}/terminate` | `api/mod.rs:1191` |
| Snapshot, restore | `POST /v1/sessions/{id}/snapshots`, `POST /v1/sessions/restore` | `api/mod.rs:1278,1326` |

Each call checks that the session belongs to the caller; another user's session is a 404.
Mutating calls need an `Idempotency-Key`. The general behaviour is proved by `api_tests.rs`.

## Runs

`POST /v1/runs` starts a run (`api/mod.rs:1362`, `api/run_start.rs:236`). A run is over when its state is one of
`SUCCEEDED`, `FAILED`, `CANCELLED`, `TIMED_OUT`, `EXPIRED` or `REJECTED` (`receipt.rs:33-40`).
Runs page newest first (see [Host API](/orchestrator/api/)). The thread's history, with tool
results, is read back through the thread routes; proved by `thread-history-lists-tool-results`.

A run names its agent and playbook by `agentRef` and `playbookRef`. A request that carries an
`inlineAgent` or an `inlinePlaybook`, with any content, is refused with 400 before anything is
written: save the agent with `PUT /v1/agents` or the playbook with `PUT /v1/playbooks` first, then
run it by reference (your draft is version `1.0.0-draft`). The run route never writes the
catalog. Proved by `a-run-with-an-inline-agent-is-refused-and-writes-nothing`,
`a-run-with-an-inline-playbook-is-refused-and-writes-nothing` and
`a-publisher-saves-a-draft-agent-and-runs-it-by-ref`.

## A workspace pins its agent version

A workspace remembers the agent version of its first run, and later runs on it use that version
(`api/run_start.rs:353-357`, `catalog_resolve.rs:58`). A member may pin their own draft; another user's draft is a 404 (`member_can_pin_own_draft`). A draft run of a member's agent uses released parts only; see [Member agents](/orchestrator/member-agents/#running-it). Proved by `workspace_pin_tests.rs` and `member-builds-and-runs-own-draft-agent-from-a-released-skill`.

## Stop, terminate and cancel

| Call | What it ends | What it does to the work |
|---|---|---|
| `POST /v1/runs/{id}/stop` | One live task | Files roll back to the last finished step. A pending approval is rejected "stopped by user". Unsent steers and queued messages are discarded and returned. The run ends `FAILED`, reason `stopped` (`api/stop.rs:1-24`). |
| `POST /v1/runs/{id}/cancel` | A run by id | Sets the run `CANCELLED`, discards its inbox, releases its reservation and terminates its session (`api/mod.rs:1514-1541`). |
| `POST /v1/sessions/{id}/terminate` | The whole session | Ends the session and its VM or process. |

Proved: stop by `stop-ends-a-running-task-cleanly` and `stop-denies-a-pending-approval`. Cancel
and terminate are read from the code and covered by `api_tests.rs` only in part.

## Talking to a running task

`POST /v1/runs/{id}/messages` sends a steer or a queued message, and `POST /v1/runs` that names
the thread of a live task does the same (`api/inbox.rs:1-8`, `api/run_start.rs:293-315`). The rules, the
caps and the outbox are on [Inbox and Outbox](/runtime/inbox-outbox/). Proved by
`steer-changes-the-running-task` and `queued-message-runs-after-the-task`.

## Responses the proxy refuses

If a run's tool calls an origin that holds a session secret and the response is over the scrub
buffer, the proxy answers the guest with a 502 and none of the body reaches it. See
[Host API](/orchestrator/api/); proved by
`oversize-response-with-a-secret-never-reaches-the-guest`.
