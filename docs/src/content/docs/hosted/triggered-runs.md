---
title: Broker-Triggered Runs
description: How a hosted broker asks vamd to start a run later, as the user who approved it, with a run grant that vamd issues, stores, checks and lets the user revoke.
status: shipped
proves:
  - trigger-starts-a-run-as-the-grant-owner
  - grant-token-is-refused-over-http
  - revoked-grant-starts-nothing
  - repeated-fire-with-the-same-key-starts-one-run
  - trigger-on-a-busy-workspace-runs-after-the-task
  - triggered-run-cannot-arm-another-trigger
  - triggered-run-resumes-like-a-chat-run
  - expiring-grant-reaches-the-owner
  - crates/vam-orchestrator/tests/hsp_grants.rs
  - crates/vam-orchestrator/src/credentials.rs::grant_checks_on_its_own_broker_only
  - crates/vam-orchestrator/src/gateway.rs::only_core_first_party_and_operator_tiers_may_request_runs
---

A broker can ask `vamd` to start a run at a later time, with nobody in the chat. The broker keeps the clock or the event source. `vamd` keeps the authority: it starts the run, and it starts it as the user who approved the request. The reference user of this is the [schedule broker](/hosted/schedule/).

## The idea

1. An agent calls a broker tool that is allowed to hold a grant, and puts the run it wants in the `run` argument.
2. The user approves that tool call, as for any tool call that asks.
3. `vamd` issues a **run grant** for the exact request, stores a record of it, and hands the broker a bearer token with the call.
4. Later the broker calls `RequestRunTrigger` on its own gateway socket with that token.
5. `vamd` checks the grant and starts the run as the grant's owner, in the Space and thread the grant names.

The broker never boots a VM, never creates a session and never says who the user is. It holds a token. Everything the token stands for is in `vamd`'s record, which the broker cannot edit.

## Run grants

### Declaring the permission

A broker that may ask for runs declares `run_request_permission` in its manifest (see [Hosted Brokers](/hosted/overview/)).

| Field | Meaning |
|---|---|
| `grant_tools` | Tools whose `Invoke` may carry a `RunGrant`. Must be non-empty and a subset of the broker's tools. |
| `loop_guarded_tools` | Tools refused inside a run a broker started. Must be a subset of the tools and contain every grant tool. |
| `surface` | The receipt surface for these runs: 3 to 16 lowercase letters, not `cli`, `app`, `api` or `system`. |
| `max_requests_per_minute`, `max_requests_per_day` | Gateway rate limits per broker. 0 means 60 and 1440. |
| `max_active_grants_per_user` | Live grants per user and broker. 0 means 50. |

A limit of 0 means the node default, never unlimited. The operator approves the declaration with the same subset gate as secrets and egress domains: a presented manifest may not add the permission, add grant tools, drop a loop-guarded tool, change the surface or raise a limit.

Only core, first party and operator brokers may declare it, receive grants and call the gateway. A third party manifest that declares it is refused.

### What a grant tool asks for

A grant tool puts a `RunGrantRequest` in its `run` argument.

| Field | Meaning |
|---|---|
| `agent_urn` | The agent to run. Empty means the calling agent. Another agent must be one the user may run. |
| `workspace` | `SAME` (default, the calling run's Space), `NEW` (make a Space now, with an optional `name`) or `ID` (an `sp_` id the user owns, bound to that agent). |
| `thread` | `NEW` (default, make a thread now), `SAME` (only with workspace `SAME`) or `ID` (a `th_` id of that Space). |
| `message` | The user turn the run starts with, at most 32 KiB. Required unless there is a `playbook_ref`. |
| `playbook_ref`, `inputs` | A playbook to start instead of a message. Inputs are checked against the playbook's input schema. |
| `on_busy` | `QUEUE` (default) or `SKIP`. |
| `expires_at` | Empty means 90 days from now. Capped at 365 days. A time in the past is refused. |

In JSON the short enum names and the full proto names are both accepted. Unknown keys are refused. A failed check is a typed tool error the agent can repair (for example `space_not_found`, `thread_needs_space`, `message_too_large`, `grant_limit`), and nothing is created.

`vamd` resolves the request once, at issuance, to a Space id and a thread id, and binds them. `RequestRunTriggerRequest` carries no target and no prompt.

### Approving, listing, revoking

* **Approving.** The approval is the user's decision on the grant tool's call. A denied call issues nothing and the grant does not exist.
* **Listing.** `GET /v1/keys?holder=broker`, or `vam grants list` (the same as `vam keys list --grants`). Each row shows the holder, the target Space and thread labels, a preview of the bound message, how it was issued, its expiry and when it was last used. A user sees only their own grants, for every role.
* **Revoking.** `DELETE /v1/keys/{id}`, or `vam grants revoke <id>` (the same as `vam keys revoke <id>`). A fire a revoked grant left in the queue ends with it. The broker can also revoke its own grant with `RevokeRunGrant`.
* **Deactivating a user** revokes the user's grants.
* **Disabling the broker's approval** closes its gateway socket, so every fire fails and nothing starts.

### Expiry

A grant always has an expiry: 90 days by default, 365 at most. A grant that has expired answers `DENIED EXPIRED`. Seven days before a grant expires, `vamd` tells its owner once (a `grant.expiring` notification and event). Renewing is a new approval: the broker's tool is called again with a `run`, which issues a new grant.

### Storage

A grant is one kind of credential: a row in the `api_keys` table with holder `broker:<id>`, scope `runs:start` and an expiry. The token is `vam_rg_<grant_id>.<secret>`. The grant id is `rg_` plus 26 base32 characters. The secret is 32 random bytes from the operating system's random source. `vamd` stores only the SHA-256 of the secret and shows the token once, to the broker.

The row also holds the owner, the source run, the tool, the bound request (agent, Space id, thread id, message, playbook pin, inputs, `on_busy`) and the trigger ref the broker used on its first accepted fire. The `GrantTarget` the broker receives carries a `request_digest`, the SHA-256 of that request in canonical JSON.

The credential check refuses a grant in these cases, and answers them all alike on HTTP:

* The token is presented as an HTTP bearer, whole or as its bare secret. HTTP accepts only holder `http`.
* The grant is presented on another broker's socket.
* The row is revoked, or has no expiry or a past one.

## The gateway

`vamd` serves `HostedTriggerGateway` on one Unix socket per approved broker that declares the permission. The socket sits in a directory with mode `0700` and has mode `0600`. By default the directory is `hsp/gw` next to the state database (override with `VAMD_HSP_GATEWAY_DIR`), and the file is named after a hash of the broker id. `GET /v1/hosted/brokers/{id}` reports the path as `gatewaySocket`. The socket is never reachable from the guest, which has vsock only.

**The calling broker is the socket the call arrived on.** No request field names a broker. If the approval records a peer uid, `vamd` also checks the connecting process's uid.

### `RequestRunTrigger`

| Request field | Meaning |
|---|---|
| `run_grant` | The token. |
| `idempotency_key` | 1 to 128 characters, no control characters. The same on every retry of one fire. |
| `fire_at` | The nominal time of the fire. |
| `trigger_ref` | The broker's label for what fired. Pinned on the grant by the first accepted or queued fire. |
| `kind` | `SCHEDULED`, `RUN_NOW` or `EVENT`. |
| `not_after` | A queued fire that has not started by this time ends `EXPIRED`. |
| `trace_context` | Optional trace context. |

The response carries `outcome`, the `run_id`, `space_id` and `thread_id`, a `retry_after_ms`, a `busy_reason` or `deny_reason`, and an `error` composed by `vamd` (never text from the broker).

| Outcome | Meaning |
|---|---|
| `ACCEPTED` | A run started. |
| `QUEUED` | The Space is busy. The fire waits for the running task and then starts as its own run. |
| `DUPLICATE` | The idempotency key was already used. The answer carries the earlier run. |
| `BUSY` | Nothing started. See below. |
| `RATE_LIMITED` | The broker's limit is reached. `retry_after_ms` says when. |
| `UNAVAILABLE` | The node cannot start the run now. Retry. |
| `DENIED` | The grant is not usable. `deny_reason` says why. |

| `busy_reason` | Meaning |
|---|---|
| `SKIPPED` | The Space is busy and the grant says `SKIP`. |
| `ALREADY_QUEUED` | A fire from this grant is already queued. One per grant. |
| `ACTIVE_RUN` | The Space's task is parked, ending or being stopped. Retry after 2 seconds. |

| `deny_reason` | Meaning |
|---|---|
| `NOT_PERMITTED` | The broker is not approved, has the wrong tier or has no run permission, the peer uid differs, or the grant's user may no longer start runs. |
| `INVALID_REQUEST` | Unknown token, a bad idempotency key, or the grant's request can no longer be started. |
| `WRONG_BROKER` | The grant belongs to another broker. |
| `REVOKED`, `EXPIRED` | The grant was revoked, or has expired. |
| `TOOL_NOT_APPROVED` | The grant's tool is no longer in the approved `grant_tools`. |
| `OWNER_DISABLED` | The grant's user is deactivated. |
| `AGENT_UNAVAILABLE` | The agent or playbook is not available to the user any more. |
| `TARGET_GONE` | The Space or thread no longer exists for the user. |
| `TRIGGER_REF_MISMATCH` | The trigger ref differs from the one pinned on the grant. |

### What `vamd` checks on a fire

In this order. The first failure is the answer.

1. The socket's broker is approved and enabled, has an allowed tier and declares the permission. The peer uid matches if one is recorded.
2. The token parses and its secret hash matches a row in constant time.
3. The row's holder is this broker.
4. The grant is not revoked and has not expired.
5. The grant's tool is still an approved grant tool.
6. The owner exists, is active and may still start runs.
7. The idempotency key is valid, and the trigger ref equals the pinned one if there is one.
8. The broker's rate limit. The windows are in memory, so a restart of `vamd` resets them.
9. Dedupe. The key is `hsp:<broker_id>:<grant_id>:<idempotency_key>`, looked up among the owner's run keys. It covers runs that started and runs that are queued.
10. The Space and the thread exist and belong to the owner.
11. The Space's busy state: `SKIP` answers `BUSY SKIPPED`, a queued fire from the grant answers `BUSY ALREADY_QUEUED`, a task that is not live answers `BUSY ACTIVE_RUN`, otherwise the fire is queued.
12. The run starts through the same path as `POST /v1/runs`: the agent is loaded for the owner, secrets and model are admitted, the playbook and its inputs are checked again, and capacity is checked.

On `ACCEPTED` or `QUEUED`, `vamd` pins the trigger ref and writes a `trigger.fired` event on the run with the grant id, trigger ref and fire kind. Every answer is counted in `vam_hsp_trigger_total` by broker and result. A queued fire is checked again when it is about to start: a revoked or expired grant, a disabled broker or a user who may no longer start runs ends it with a message instead of a run.

A trigger **never steers**. A person's message to a running task can steer it. A fire always waits for the task and starts as its own run, and it keeps its playbook and inputs.

### `RevokeRunGrant`

The broker sends the token as proof that it holds the grant. `vamd` revokes the grant and discards a queued fire of that grant. It is idempotent: a grant that is already revoked answers success to the broker that holds it.

## Space and thread ids

A Space is the unit of workspace: its files and its busy state. A thread is a conversation inside a Space. Their ids are opaque: `sp_` and `th_` followed by 26 base32 characters. Names are labels, and a rename changes no id.

A grant binds ids, never names. If the Space is deleted and another is made with the same name, the grant for the old one answers `DENIED TARGET_GONE`. A foreign or missing id gets the same answer, so a user cannot probe for ids.

`workspace NEW` and `thread NEW` create their rows when the grant is issued, not when the run starts. If anything fails after that, the rows and the grant are removed again. `RunSpec` carries `space_id` and `thread_id`, and `RunStatus` carries them too.

## Where the run shows its source

* `CallerContext.run_source` is empty for a person and `broker:<id>` for a triggered run. A broker therefore sees that a call comes from a triggered run.
* `RunStatus.source` is absent for a person. For a trigger it holds `kind` (`trigger`), `broker_id`, `grant_id`, `trigger_ref`, `fire_at` and `fire_kind` (`scheduled` or `run_now`).
* The receipt records the requesting user, the surface from the approved manifest (not from the broker's request) and a `requestTrigger` block with the broker id, grant id, trigger ref, fire time and fire kind. `vam receipt` prints it as one line: user, surface, trigger ref, grant, broker and fire time. See [Run Receipt](/security/run-receipt/).

The run's owner and principal come from the grant, which comes from the run that issued it.

## Stopping loops

| Loop | Guard |
|---|---|
| A triggered run creates another trigger | In a run a broker started, `vamd` refuses the call of any `loop_guarded_tools` entry with `loop_guarded: this tool cannot be used in a run a trigger started`. It reads the run's recorded source, not anything the guest says. |
| The broker fires twice for one event | The idempotency key is deduped by `vamd`. |
| A job is still running at the next fire | The fire queues once per grant, or is skipped, as the grant says. |
| A broker retry storm | Per-broker rate limits. |
| A broker acts for a user | The owner comes only from the grant record, which is bound to the receiving broker. |
| A user holds too many grants | `max_active_grants_per_user`. |

## Not built yet

* Resuming a waiting task from a broker event (`wait_for`, `DeliverEvent`). The `event_context` field of the trigger request is reserved.
* Event-source brokers. `FireKind` has `EVENT`, but no broker sends it.
* Broker runs on demand, outside a grant.
* Third party brokers with run permission. They need vsock isolation, which is not built.

## Non-goals

* Cron, schedule storage or missed-fire handling inside `vamd`. A queued fire's `not_after` is a deadline for that fire, not a schedule.
* A broker that holds a long-lived `runs:write` API key and calls `POST /v1/runs` itself.
* A broker that says which user, agent or owner a run is for.
* A target or prompt in `RequestRunTrigger`.

## Design notes

* **The broker never asserts identity.** The broker already sees the host-stamped `CallerContext` on every call. If it could send those fields back to start a run, every approved broker could start runs as any user it had ever seen. A grant keeps the storage in the broker and the identity in `vamd`.
* **The grant binds the whole run request.** The agent, Space, thread, message, playbook, inputs and busy policy are fixed when the user approves. The broker can choose when to fire, not what runs. Changing any of it is a new approval and a new grant.
* **A busy fire queues and never steers.** Putting an unattended message into a task that a person is watching would change that task. Queuing it as its own run keeps each run's record and receipt whole. One queued fire per grant stops a slow task from building a backlog.
* **One credential type.** Grants share hashing, listing, revoking and expiry with API keys. The holder says where a secret is good, so a token stolen from a broker is useless over HTTP and on any other broker's socket.
* **The socket names the broker.** A per-broker socket gives an identity that needs no extra secret to store or rotate.
