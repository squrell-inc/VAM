---
title: Schedule Broker
description: The first-party schedule broker (vam.schedule) lets an agent start a run later or on a repeat, as the user who approved it, with the clock in the broker and the authority in vamd.
status: shipped
proves:
  - scheduled-message-runs-at-its-time
  - deleted-schedule-never-fires-again
  - missed-fire-runs-once-after-the-broker-restarts
  - schedule-is-saved-only-after-the-user-approves
  - schedule-set-to-skip-does-not-wait-for-a-busy-workspace
  - schedule-for-a-new-workspace-runs-there
  - another-users-schedules-are-invisible
  - skipped-schedule-is-told-once-a-day
  - brokers/vam-schedule/tests/fire_loop.rs
  - brokers/vam-schedule/tests/log_redaction.rs
  - brokers/vam-schedule/src/when.rs::floor_cases
  - brokers/vam-schedule/src/when.rs::berlin_spring_forward
  - brokers/vam-schedule/src/fire.rs::catch_up_latest_fires_the_newest_missed_time_and_folds_the_rest
  - brokers/vam-schedule/src/manage.rs::delete_with_the_gateway_down_marks_deleting_bumps_the_version_and_drains_later
---

`vam.schedule` is a first-party HSP broker (package `vam-schedule`, binary `vam-schedule-broker`, source in `brokers/vam-schedule`). It gives an agent six tools to make a run start by itself at a time, with no one in the chat. It is built on [broker-triggered runs](/hosted/triggered-runs/): the broker keeps the clock and asks `vamd` to start each run with a run grant.

## Tools

| Tool | Side effect | What it does |
|---|---|---|
| `schedule_create` | mutable external | Makes a schedule. Arguments: `title` (required, up to 200 characters), `when` (required), `run` (required), `catchUp`, `clientRequestId`. Answers the schedule id, the next fire time and where it will run. |
| `schedule_list` | read only | Lists the user's schedules: id, title, when, next fire, state, target, expiry and last outcome. No message text. Arguments: `enabledOnly`, `limit` (1 to 200, default 50). |
| `schedule_get` | read only | Shows one schedule and its last 10 fires. `NOT_FOUND` for an id that is not the user's. Argument: `scheduleId`. |
| `schedule_update` | mutable external | Changes `title`, `when`, `enabled` (false pauses, true resumes) or `catchUp`. Passing `run` replaces what each fire runs: the user approves it again and the old grant is withdrawn. |
| `schedule_delete` | mutable external | Deletes a schedule. Its grant is withdrawn first. |
| `schedule_run_now` | mutable external | Starts the schedule's run once, now, with its stored grant. The user is asked again. The schedule's next fire does not change. |

Any key a tool does not list is refused, and so is any key that names an identity (`user`, `owner`, `userId`, `agentUrn` and similar). The user and agent come from the host-stamped caller context. A call without them is `INVALID_ARGUMENT`.

### `run`

`run` is what each fire does. The user approves exactly this request once, and every fire repeats it. It has the fields of a run grant request: `agentUrn`, `workspace`, `thread`, `message`, `playbookRef`, `inputs`, `onBusy` and `expiresAt`. See [Broker-Triggered Runs](/hosted/triggered-runs/) for what each means and how a Space and thread are chosen. The message must stand alone: the run has none of the chat.

* `onBusy` is `QUEUE` (default) or `SKIP`. With `QUEUE`, a fire on a busy Space waits for the running task and starts after it. With `SKIP`, it starts nothing.
* `expiresAt` ends the grant. The default is 90 days and the cap is 365. A schedule whose grant ends at or before its first fire is refused.

### `when`

Exactly one of `once` or `cron`.

| Form | Fields |
|---|---|
| `once.in` | An ISO-8601 duration counted from when the schedule is saved: `PT30M`, `PT2H`, `P1D`, `P1W`, `P1DT2H`. Weeks, days, hours, minutes and seconds only. Must be greater than zero. No timezone. |
| `once.at` | A local wall time such as `2026-11-02T08:00` with a `timezone` (an IANA name such as `Europe/Berlin`), or an instant with an offset such as `2026-11-02T08:00:00Z`. |
| `cron` | `expression`: five fields (minute, hour, day of month, month, day of week). `timezone`: an IANA name, required. |

* Every time is stored as a UTC instant. A time in the past is refused.
* In a zone, a wall time that does not exist (spring forward) fires once, at the first valid minute after the gap. A wall time that happens twice (fall back) fires once, at the first. Tests pin this for `Europe/Berlin` and `America/New_York`.
* **Cron floor.** The broker refuses a cron whose fires are closer than the minimum interval, 15 minutes by default (`--min-interval`). It checks every fire in the 31 days after the first fire. A refusal is `INVALID_ARGUMENT`, as is any bad `when`.

## Catch-up

The broker may be stopped, or the host asleep, when a fire is due. `catchUp` says what to do.

| Value | Behaviour |
|---|---|
| `LATEST` (default) | If the newest missed time is inside the grace window, fire it once. Older missed times in the backlog are recorded as one `missed` entry with a count. |
| `SKIP` | Fire nothing for missed times. Record one `missed` entry and move to the next future time. |

The grace window is 24 hours for a one-off. For a cron it is half the interval to the next fire, at most one hour. A fire that is one minute late or less is a normal fire, not a catch-up, under either value. A backlog older than the window is recorded as `missed` and nothing starts. There is never more than one catch-up fire per schedule.

A catch-up fire keeps its own nominal `fire_at`. A fire that reached `vamd` just before a crash is therefore answered `DUPLICATE` when it is sent again, and does not start a second run.

## Firing

One loop sleeps until the earliest due time, for at most 30 seconds, and wakes early on a create or update. It starts at most four fires at once. It writes the outcome of a fire and moves the schedule on in one transaction.

**Idempotency key.** The key of a scheduled fire is the hex SHA-256 of `vam.schedule`, the schedule id, the nominal fire time in UTC and the kind. Every retry of one fire uses the same key. A run-now key also hashes the id of the calling run, so a retried tool call is deduped and two deliberate presses start two runs. `trigger_ref` is the schedule id. `not_after` is the next nominal time for a cron, and 24 hours after `fire_at` for a one-off or a run-now.

| Answer from `vamd` | The broker |
|---|---|
| `ACCEPTED`, `QUEUED`, `DUPLICATE` | Records the run id and moves on. |
| `BUSY` with `SKIPPED` or `ALREADY_QUEUED` | Records `skipped_busy` and moves on. No retry. |
| `BUSY` with `ACTIVE_RUN` | Retries with the same key until `not_after`, then records `skipped_busy`. |
| `RATE_LIMITED`, `UNAVAILABLE`, or no answer | Retries with the same key: exponential backoff with full jitter, from 2 seconds up to 5 minutes, never sooner than `retry_after_ms`. The backoff is stored, so it survives a restart. At `not_after` it gives up with `failed_unavailable`. |
| `DENIED` `REVOKED` or `EXPIRED` | The schedule becomes `revoked` or `expired` and is disabled. |
| `DENIED` for any other reason | The schedule becomes `needs_attention` with the reason and is disabled. The grant is kept: fix the cause and resume with `schedule_update`. |

A one-off is `completed` after an accepted, queued or duplicate fire, and its grant is withdrawn. A one-off whose fire is skipped or gives up also ends `completed`. A `QUEUED` fire is discarded if its grant is revoked, so the grant of a queued one-off is kept until `not_after`.

When a fire is skipped on a busy Space, `vamd` tells the owner once per grant and day.

### States

`active`, `needs_attention`, `revoked`, `expired`, `completed` and `deleting`. A revoked or expired schedule can only be repaired by an `update` with a new `run`. A completed or deleting one cannot be changed.

## Who sees a schedule

The broker keeps its store by the user id and agent urn that `vamd` stamps on each call. A user sees only their own schedules. An id that is not theirs answers `NOT_FOUND`, the same as a missing one. Only the owner, through the agent that made the schedule, may update, delete or run it.

The schedule id is `sch_` and 26 hex characters derived from the grant id. A retried create with the same `clientRequestId` returns the first schedule and withdraws the extra grant it received.

## Deleting and revoking

* **`schedule_delete`** calls `RevokeRunGrant` first. If `vamd` answers, the row is removed. If it cannot be reached, the schedule becomes `deleting`: it never fires again, its grant is queued for withdrawal, and the loop retries once a minute and removes the row when it succeeds. The tool answers `deleted` or `deleting`.
* **The user** can revoke the grant without the broker: `vam grants revoke <id>`. The next fire is `DENIED REVOKED` and the schedule shows `revoked`.
* **An update with `run`**, and the end of a one-off, withdraw the old grant the same way.
* **The operator** can disable the broker's approval, which closes its gateway socket. Every fire then fails and nothing starts.

## What a run shows

The run's source and receipt come from `vamd`. The surface is `schedule`, set by the operator-approved manifest, not by the broker. The receipt names the user, the surface, the schedule id (the trigger ref), the grant id, the broker and the nominal fire time. See [Broker-Triggered Runs](/hosted/triggered-runs/).

A run the schedule started cannot create, update or run-now another schedule. `schedule_create`, `schedule_update` and `schedule_run_now` are loop-guarded in the manifest.

## Running the broker

Run it on the `vamd` host, as the same user as `vamd`. The broker's socket has mode `0600` and `vamd` dials it. On a Mac, `vamd` runs inside Lima: run the script inside the Lima VM. The broker needs no secrets.

```bash
scripts/run-schedule-broker.sh             # build, start, register, restart with the gateway socket
scripts/run-schedule-broker.sh --dry-run   # print the registration request and exit
scripts/run-schedule-broker.sh --foreground
scripts/run-schedule-broker.sh --stop
```

The script needs `jq` and `curl`. It does this:

1. Finds or builds `vam-schedule-broker`.
2. Starts it with `--socket` and `--state-dir`.
3. Registers its manifest (`vam-schedule-broker manifest` prints it) with `PUT /v1/hosted/brokers`.
4. Reads `gatewaySocket` from `GET /v1/hosted/brokers/vam.schedule`. `vamd` opens the gateway only after the registration.
5. Restarts the broker with `--gateway-socket`.
6. Checks that all six tools are listed.

It reads these environment variables:

| Variable | Default |
|---|---|
| `VAMD_URL` | `http://127.0.0.1:8080` |
| `VAMD_ADMIN_KEY` | `$VAM_API_KEY`. An API key with `brokers:admin`. |
| `VAM_SCHEDULE_SOCKET` | `/tmp/vam-schedule.sock` |
| `VAM_SCHEDULE_BIN` | built from this repository if not found |
| `VAM_SCHEDULE_STATE_DIR` | `/tmp/vam-schedule-broker` (database, lock, pid and log) |

The binary takes `--socket`, `--state-dir`, `--gateway-socket` and `--min-interval` (a whole number and `s`, `m` or `h`; default `15m`). `RUST_LOG` sets the log filter.

* **No gateway socket, no fires.** Without `--gateway-socket` the tools work but the fire loop does not run. Due times are caught up when the broker is next started with the flag.
* **One process per store.** The state directory is mode `0700` and holds an exclusive lock. A second broker on the same directory exits.
* **The manifest** is first party tier, needs no secrets or egress domains, requires the user and agent identity, and declares `schedule_create` and `schedule_update` as grant tools, with 60 requests per minute, 5000 per day and 50 active grants per user.
* **Per agent.** After registering, allow the schedule tools on each agent that may schedule work. The script prints a reminder. The user's approval of `schedule_create` is what issues the grant.

Logs carry schedule ids and outcome names. They never carry the token or the message, even at trace level.

## Not built yet

* Schedules that wait for an event, and event-source brokers.
* Schedules owned by a team.
* An editor in the app. The app lists and revokes grants. Editing is through the agent.
* Sub-minute schedules, and cron with seconds or years.

## Design notes

* **The broker owns the clock and `vamd` stores no schedules.** A clock is standing work: a timer, a catch-up policy and retries. It belongs in a broker with its own release cycle. `vamd` holds only the grant, which is a record of what the user allowed, and it can refuse every fire by revoking that.
* **No user credential is stored.** The broker holds a token that works only on its own gateway socket and only for the one run the user approved. It has no API key and no way to run anything else. A stolen database gives a thief the right to ask for runs the user already approved, and nothing else.
* **The approval covers every fire.** The user approves the whole request once, so the model fills `run` completely and the broker cannot change it.
* **Retries never double-run.** The key comes from the schedule and the nominal time, not from the time of the call, so a crash and a retry send the same key and `vamd` answers with the run it already has.
