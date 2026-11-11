---
title: Playbooks, gates and approvals
description: How a playbook starts, how a gate and a tool approval pause a run, what a decision can be, and how a paused run survives a restart.
status: shipped
proves:
  - matching-request-starts-the-playbook
  - greeting-starts-no-playbook
  - injected-text-cannot-start-a-playbook
  - gate-decided-finishes-the-run
  - gate-offers-its-options-and-stop-ends-the-run
  - approval-suspends-and-resumes
  - always-allow-and-always-deny-are-remembered
  - released-agent-approvals-are-once-only
  - parked-approval-survives-a-restart
  - question-waits-for-an-answer-and-resumes
  - crates/vam-cli/tests/playbook_test_cli.rs
  - crates/vam-orchestrator/src/daemon.rs::tool_hitl_timeout_parser_uses_one_hour_default
---

The host owns the playbook and every pause. The guest runs one bounded task at a time under a lease
([Guest runtime](/runtime/overview/)); it cannot advance a playbook or decide an approval itself.
The spec fields are in the developer docs, [Playbook spec](/authoring/playbook-spec/).

## Starting a playbook

An agent can have playbooks attached. `playbook_activation.rs` (`build_attached` line 43, `plan_start_playbook`
line 183) decides whether one starts. A playbook whose activation mode is not `auto` is not started by the model
alone; it asks first (`playbook_activation.rs:310-312`). The model offers `start_playbook` only while no playbook
is bound (`crates/vam-runtime/src/agent.rs:96`).

| Request | Result | Proof |
|---|---|---|
| matches the playbook's `when` | the playbook starts | `matching-request-starts-the-playbook` |
| a greeting | no playbook starts | `greeting-starts-no-playbook` |
| instruction-like text inside a note | no playbook starts without approval | `injected-text-cannot-start-a-playbook` |

## Gates

A gate step stops the run and waits for a person. The gate offers its options, and the response is checked
against the schema captured when the gate opened (`crates/vam-orchestrator/src/hitl.rs:400`). A gate with no
`timeout` waits 72 hours; publish refuses a timeout above 30 days (`workflow.rs:49-51`). A decided gate finishes
the run (`gate-decided-finishes-the-run`); stopping the run ends it (`gate-offers-its-options-and-stop-ends-the-run`).

## Tool approvals

When the policy answers "ask", the call becomes an approval and the run is `PAUSED`. The routes are
`GET /v1/approvals`, `GET /v1/approvals/:id` and `POST /v1/approvals/:id/decide` (`api/mod.rs:190-192`). A
decision is `APPROVED`, `REJECTED`, `AMENDED` (approve with changed arguments) or `SUBMITTED` (an answer to a gate
or a question) (`api/mod.rs:1922-1927`). The decide request can also carry `remember`: `ALWAYS_ALLOW` records an
approval and a standing rule, `ALWAYS_DENY` a rejection and a deny rule (`api/mod.rs:1910-1912`); a decision that
disagrees with `remember` is refused. The commands and the two-pass model are on
[Approvals](/security/hitl-lifecycle/).

| Behaviour | Proof |
|---|---|
| The call waits, then runs once approved | `approval-suspends-and-resumes` |
| Always allow and always deny are remembered | `always-allow-and-always-deny-are-remembered` |
| A released agent's approvals are once-only | `released-agent-approvals-are-once-only` |
| A question waits for an answer and refuses one that breaks its schema | `question-waits-for-an-answer-and-resumes` |

## Timeout

A tool approval that nobody decides times out. The limit is `VAMD_TOOL_HITL_TIMEOUT_SECS` (or `--tool-hitl-timeout-secs`), one hour by default (`crates/vam-orchestrator/src/daemon.rs:192-196`; 0 gives the default). Proved by the unit test `tool_hitl_timeout_parser_uses_one_hour_default`.

## Pause, checkpoint and resume

On a pause the guest saves a continuation checkpoint and the host keeps it in SQLite. The checkpoint is at most
4 MiB, uploaded in chunks of at most 32 KiB on one port-52 connection (`crates/vam-runtime/src/pep.rs:15-22`);
the host checks a SHA-256 digest before it stores the bytes, and the guest checks it again on resume
(`hitl_checkpoint.rs`). A decision for an earlier generation is refused. A parked approval survives a restart of
`vamd` (`parked-approval-survives-a-restart`).

## Approval tokens

`vam-security` signs approval claims with Ed25519 under a domain name for each message type, so bytes signed for
one kind cannot be replayed as another (`crates/vam-security/src/lib.rs:122`). A reviewer token is single use:
the host stores the consumed token id in the approval row, which is unique (`workflow.rs:468`), so a restart does
not make a used token valid again. The signer and the replay rule have no test under `tests/`, so they are not proved.

## Playbook tests

`POST /v1/playbook-tests` (`api/mod.rs:159`, `playbook_test.rs:122`) runs a playbook against a scenario and
aggregates trials. The CLI front end is `vam playbook test` (`crates/vam-cli/src/cmd/catalog.rs:413`), covered by `crates/vam-cli/tests/playbook_test_cli.rs`.
