---
title: Inbox and Outbox
description: Talking to a running task and hearing back from it, steer, queue, stop, progress and questions.
status: shipped
proves:
  - steer-changes-the-running-task
  - stop-denies-a-pending-approval
  - correction-sent-mid-task-is-followed
  - follow-up-continues-after-stop
  - crates/vam-cli/tests/run_inbox_cli.rs
  - crates/vam-spec/tests/inbox_contracts.rs
---

A task no longer has to finish before you can say something to it, and it can tell
you what it is doing. Messages **to** the task go through the run's **inbox**.
Messages **from** the task go through the **outbox**.

## Sending to a running task

`POST /v1/runs/{id}/messages {text, mode?}` (CLI: `vam run send`). `POST /v1/runs`
naming the thread of a live task does the same.

| Mode | Behaviour | CLI |
|---|---|---|
| `STEER` (default) | The text is taken in at the task's next **delivery point** and becomes a user message in the same task. | `vam run send <run> "text"` |
| `QUEUE` | The text becomes the next task. The `202` names the run it will start (`inbox.runId`). | `vam run send <run> "text" --queue` |

A message for a workspace that a live or parked run holds, from **another thread**,
is queued the same way: it starts, as the person who sent it, when the run ahead of
it has ended by any end but a user stop (which discards it), including after a
vamd restart. A run that is finalizing, settling or being stopped answers a
retryable `409`.

**Delivery point.** The guest pulls its inbox at the start of each step, after a
tool result and before the next model call. A steer is therefore read after the
current tool call finishes, never in the middle of one. Steers a task ends without
taking in become one queued task, unless the thread log already holds them.

**Caps.** At most 20 undelivered messages per run (`429 inbox_full`), 32 KiB of
text per message. The message route has no run-creation rate limit.

A parked task (waiting on an approval or a question) keeps its steers until the
decision resumes it.

## Stopping

`POST /v1/runs/{id}/stop` (CLI: `vam run stop`) ends the live task cleanly:

- files roll back to the last finished step;
- a pending approval is rejected "stopped by user", a pending question is
  cancelled, and the task never resumes;
- messages that never reached the task (pending steers, queued messages) are
  **discarded**, each listed in `discarded[]` with its text so a client can put it
  back in the composer, and recorded as `inbox.discarded`;
- the run ends `FAILED`, reason `stopped`, error "stopped by user".

A run that had already ended answers `200` with its own end. While the workspace is
still settling the answer is `504 stop_pending`. See [Session](/orchestrator/session/)
for stop versus terminate versus cancel.

## Events

| Event | When |
|---|---|
| `inbox.received` | A message was accepted. |
| `inbox.delivered` | A steer reached the task. |
| `inbox.discarded` | A stop or cancel dropped it. |
| `inbox.started` | A queued message began its task. |
| `outbox.message` | The agent posted text (`post_message`). |
| `outbox.card` | The agent showed a [jig](/runtime/jig/) card (`show`). |

`vam run events <run>` prints outbox messages and cards as readable lines
(`agent: ...`, `card vam.progress: ...`) beside the JSON.

## Progress and questions

The agent has three conversation tools, described in [Jig](/runtime/jig/):
`post_message {text}`, `show {jig, data}` (for example `vam.progress`) and
`ask_user {jig, data}`. The host checks every call; cards need `human.jig` on the
run.

A wildcard `tools:*` deny does **not** remove these three tools. Only a rule that
names the tool exactly (`builtin:post_message`, `builtin:show`, `builtin:ask_user`
set to deny) or an org or source deny does. Each call still gets its
`policy.decided` event.

`ask_user` suspends the task as an approval of kind `question`. The run is
`PAUSED` and its status carries `waitingFor: INPUT` (`APPROVAL` for a tool
approval, `GATE` for a gate). Answer with
`POST /v1/approvals/{id}/decide` using `SUBMITTED` and `response`, or from the CLI:

```bash
vam run answer <approval> --json '{"answer":"Ada"}'
vam run answer <approval> --decline
```

A wrong-type answer is a 4xx and the question stays pending. `--decline` rejects it
and the task is told the user declined.
