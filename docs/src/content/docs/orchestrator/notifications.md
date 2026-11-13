---
title: Notifications
status: shipped
proves:
  - notifications-are-never-shown-to-another-user
  - decision-anywhere-clears-the-notification
  - approval-in-another-thread-reaches-the-list
  - restarted-approval-keeps-one-notification
  - agent-reads-what-waits-without-acting
  - background-run-end-notifies-and-chat-turn-does-not
  - gate-left-open-shows-until-it-times-out
  - expiring-grant-reaches-the-owner
  - skipped-schedule-is-told-once-a-day
  - revoked-microsoft-connection-parks-the-call-and-tells-the-member
  - gate-decided-from-slack-names-slack-on-the-receipt
  - quiet-hours-hold-slack-until-morning
  - unverified-contact-receives-nothing
description: The per-user notification record that vamd writes when something needs a person or something they left running is done, with its API, CLI, agent tool, preferences and contacts.
---

When something needs a user, or something they left running has finished, `vamd` writes one **notification record** for that user. The record says what happened, which run, thread and Space it belongs to, and what the user can do about it. The API, the CLI and an agent tool all read the same record. Channels such as Slack are fed from it too: see [Notify Broker](/hosted/notify/).

A record is a projection of a fact `vamd` already stores. It is written in the same transaction as that fact (an approval row, a receipt, a grant claim), so the two exist together or not at all. It is not a second event bus: nothing is published on the event bus for it, and run timelines still read the run outbox. The only extra signal is the counter `vam_notifications_total{kind}`.

## The record

One row per event and recipient user, in the workflow store beside `approvals`.

| Field | Meaning |
|---|---|
| `id` | `nt_` plus 26 random characters. A foreign id answers "not found". |
| `kind`, `severity` | What happened (below). Severity is `action` (the user must do something), `warn` (something failed or is about to) or `info` (done, nothing to do). |
| `runId`, `spaceId`, `threadId`, `approvalId`, `grantId`, `receiptDigest` | Where it happened. Ids only, never names. Each is present only when it applies. |
| `agentUrn`, `agentLabel`, `via` | Which agent, and how the run came to be (a trigger or a delegation). |
| `title`, `body` | Written by the host from a fixed template per kind: tool name, step id, outcome, expiry. At most 120 and 500 characters. |
| `action` | `{verb, target, api}`. The verb is `decide`, `answer`, `open` or `renew`. The target is `{kind, id}`. Each surface builds its own link from it. |
| `count` | How many times the same subject recurred while the record was open. |
| `createdAt`, `updatedAt`, `firstShownAt`, `readAt` | Times in unix seconds. `readAt` is per user, not per channel. |
| `actedAt`, `actedByUserId`, `actedSurface` | Who closed it and from where (`app`, `cli`, `api`, `slack`). |
| `resolvedAt`, `resolvedReason` | Closed: `acted`, `expired`, `timed_out`, `stopped` or `run_ended`. |

Two trust tiers apply, as in the [run receipt](/security/run-receipt/). The row holds host facts only. Text an agent wrote (a tool's argument preview, a question, a gate prompt) is never copied into the row. `GET /v1/notifications/{id}` attaches it at read time as `agentText {label, text}`, at most 500 characters, labelled "written by <agent>, not verified by the host" (or "from the playbook" for a gate). Show it fenced.

## Kinds and when `vamd` writes one

| Kind | Severity | Written when | Action | Goes to |
|---|---|---|---|---|
| `approval.pending` | action | A run parks on a tool approval. | `decide` | The approval's approver (the run owner today). |
| `question.asked` | action | A run parks on `ask_user`. | `answer` | The approver. |
| `gate.opened` | action | A playbook gate opens. | `decide` | The approver. |
| `run.finished` | info, or warn when the run failed | A run ends and its receipt is stored (see below). | `open` the receipt | The run owner. |
| `grant.expiring` | warn | The grant sweeper claims a grant seven days before it expires. | `renew` | The grant owner. |
| `budget.hit` | warn | The active-duration budget stops a run, or the spend budget refuses a model call. One per run. | `open` the receipt | The run owner. |
| `trigger.fired` | info | A trigger fire was queued behind a live task. | `open` | The grant owner. |
| `trigger.skipped` | warn | A fire was skipped: workspace busy, Space or thread gone, agent unavailable, grant expired. | `open` | The grant owner. |
| `connection.needs_reauth` | action | A connector connection moves to needs-reconnect. | `open` the connector | The connection owner. |

`approval.pending`, `question.asked` and `gate.opened` are the **decision kinds**. A `reauth` park (a run waiting for a connection, not for a decision) writes none; it gets `connection.needs_reauth` instead.

`run.finished` is not written for every run. A chat turn that succeeds without waiting leaves none. A run gets one when any of these holds:

- a trigger started it;
- it waited on a person at least once;
- it ended `FAILED`, `TIMED_OUT`, `EXPIRED` or `REJECTED`.

A run the user stopped, a run stopped by the duration budget (it has `budget.hit`), an asked (delegated) run and a report run get none. The record carries the receipt digest. If the node cannot sign the receipt, the sweeper writes the record without a digest after a short delay and fills the digest later.

### One record per subject

Each record has a subject key, unique per user and kind. The key is the run, tool and argument digest for an approval; the run and step for a gate; the run for `run.finished` and `budget.hit`; the grant for `grant.expiring`; the grant and the UTC day for `trigger.*`; the connector for `connection.needs_reauth`.

If the same subject recurs while its record is open, the record's `count` goes up and its text is refreshed. A resolved record never reopens: the next occurrence gets a new record. So a parked approval keeps one record across a vamd restart, and two skipped fires on one day are one record with `count` 2. A repeated call after a denial has a different argument digest or a new run, so it gets its own record.

### Not built yet

The kinds `report.arrived`, `delegation.stopped` and `delegation.expired` exist in the kind list and the preference tables, and the functions that write them exist, but no code path calls them yet. No such record appears today.

## Clearing: acted on anywhere clears everywhere

A decision kind is closed only by deciding the approval it names, from any surface. All surfaces go through the same decision code, so a gate decided from Slack clears the record for the app and the CLI, and a gate decided in the app updates the Slack message.

| Kind | Closed by | `resolvedReason` |
|---|---|---|
| Decision kinds | A decision from any surface (`actedSurface` says which). | `acted` |
| Decision kinds | The approval expires, the gate times out, or the run is stopped. Nobody acted. | `expired`, `timed_out`, `stopped` |
| Decision kinds | The run ended with the record still open. | `run_ended` |
| `grant.expiring` | The grant is renewed or revoked, or it expires. | `acted` or `expired` |
| `connection.needs_reauth` | The user connects the connector again. | `acted` |
| `run.finished`, `budget.hit`, `trigger.*` | `POST /v1/notifications/{id}/acted`. | `acted` |

Read and acted are different. Reading marks the record seen for that user and does not close it. An unresolved `action` record stays in the badge count even when read. A decision kind can never be marked acted through the notification API: that answers `409` with `decide_required` and the approval endpoint to use. No surface can clear a pending approval without deciding it.

## API

All routes answer only the calling user's rows. They need the scope `notifications:read` (reads) or `notifications:write` (marks). Members, publishers, org admins and the operator key have both by default. Keys that existed before notifications and hold `approvals:write` received both scopes once, at upgrade.

| Route | Does |
|---|---|
| `GET /v1/notifications` | One page of records. Query: `state=open` (unresolved only; default is all), `unread=true`, `kind=`, `pageSize` (default 50, at most 100), `pageToken`. Order: unresolved `action` records first (oldest first, so the longest wait is on top), then unread by newest, then read by newest. Sets `firstShownAt`. |
| `GET /v1/notifications/summary` | `{action, warn, unread, etag}` for a badge. `action` counts unresolved action records. `warn` counts unresolved, unread warnings. Sends `ETag` and answers `304` to a matching `If-None-Match`. |
| `GET /v1/notifications/{id}` | One record. For a decision kind it adds `agentText` and the approval as `GET /v1/approvals/{id}` returns it. |
| `POST /v1/notifications/{id}/read` | Marks one record read. Idempotent. |
| `POST /v1/notifications/read` | Body `{ids}` or `{before}` (unix seconds). Marks the caller's records read and returns `{updated}`. An empty body is refused. |
| `POST /v1/notifications/{id}/acted` | Closes a non-decision record. The surface comes from the `X-Vam-Surface` header. |
| `GET`, `PUT /v1/me/notification-prefs` | The caller's preferences (below). |
| `GET`, `POST /v1/me/contacts`, `DELETE` or `POST /v1/me/contacts/{id}/unlink` | The caller's channel contacts (below). |
| `GET`, `PUT /v1/node/notify` | The operator's channel settings (see [Notify Broker](/hosted/notify/)). |

Deciding stays at `POST /v1/approvals/{id}/decide`. The notification API never decides.

The surface of a decision is the client's hint in `X-Vam-Surface`: `cli` and `app` are kept, anything else is `api`. A client cannot claim `slack`. Only a decision relayed through the notification gateway, with a handle `vamd` issued, is recorded as `slack`.

Records are kept for 30 days (`info`) or 90 days (`warn`) after they were last read or resolved, and at most 2000 rows per user. An unresolved `action` record is never pruned.

## CLI

```bash
vam notifications                       # open records, oldest wait first
vam notifications --unread --kind approval.pending
vam notifications --all                 # include resolved records
vam notifications show <id>             # one record, with agent text labelled
vam notifications read <id>             # mark one read
vam notifications read --all            # mark every record read
```

The list prints the decide command for a decision kind (`vam approve decide <approval-id> --decision approved|rejected`) and the connect command for `connection.needs_reauth`. Every request carries `X-Vam-Surface: cli`, so a decision from the CLI names the CLI.

Preferences and contacts also live under this command:

```bash
vam notifications prefs                              # show, with the mode each kind has now
vam notifications prefs --quiet 22:00-07:00 --tz Europe/Berlin
vam notifications prefs --no-quiet
vam notifications prefs --kind run.finished --on     # or --off
vam notifications prefs --kind question.asked --always    # or --not-always
vam notifications contacts
vam notifications link slack [--account <team>/<user>]
vam notifications unlink slack
```

An app or any other client draws a badge from `/summary` and a list from `GET /v1/notifications`, and can open a decision kind with the approval the record carries.

## The `notifications_list` agent tool

`notifications_list` is a read-only host tool. It lets an agent answer "is anything waiting on me?" and never act on the answer.

* **Opt-in.** The agent needs a permission rule that names exactly `builtin:notifications_list` and allows it. A default rule (`allow`, `by_side_effect`, `ask`, `tools:*`) does not opt in.
* **Arguments.** `kind` (a string or a list of strings) and `limit` (1 to 20, default 10).
* **Result.** Records that are unresolved and unread, in list order, with host fields only: id, kind, severity, title, body, ids, `via`, creation time, and an `action` with the verb, a link target and the CLI line. It never carries `agentText`. The result is capped at 4 KiB; whole records are dropped to fit and `truncated` says so.
* **Side effects.** None. It does not mark a record read or shown, and there is no tool to mark, decide or answer.
* **Who.** The run and its owner come from the leased run, never from the arguments. A run that a trigger, a delegation or a report started is refused: only a run a person started may read the list.

## Scope: who sees what

* A record belongs to one user. The list, summary, read, acted and by-id routes filter by the authenticated user. Another member sees none of it and gets "not found" for a foreign id.
* Records are not shared with an org inbox. There is no cross-user view.
* The recipient of a decision kind is the approval's approver. Today that is always the run owner.
* The agent tool reads the run owner's records and nobody else's.
* Contacts are the caller's own. Records and receipts keep the user id, never a channel address.
* Operator settings need `brokers:admin`. Anyone with `node:read` can see which adapters are on and the org's kind defaults. Only the operator sees the vault secret names.

## The receipt effect

A decision is recorded with who made it and from which surface, on the approval row: `decided_by_user_id` and `decided_surface`. The [run receipt](/security/run-receipt/) prints them as `approvedBy` and `surface` on the approval or gate line. So a gate decided from Slack names `slack` on the receipt, with the user who linked that Slack account. The record's `actedSurface` is a copy, taken in the same transaction. A close that no person made (expiry, timeout, stop) has a `resolvedReason` and no acting surface.

## Notification preferences and quiet hours

Preferences decide which records go to **channels**. The list in the app and CLI always shows every record.

`PUT /v1/me/notification-prefs` replaces the whole document:

```json
{
  "kinds": { "run.finished": "immediate", "trigger.skipped": "off" },
  "quietHours": { "start": "22:00", "end": "07:00", "tz": "Europe/Berlin" },
  "always": ["question.asked"]
}
```

`GET` returns the same document plus `effective`: the mode each kind has now.

* **Kind mode.** `immediate` (or `on`) or `off`. A kind not listed follows the operator's default for the node, then the built-in default. Unknown kinds and modes are refused.
* **Built-in defaults.** The decision kinds, `trigger.skipped`, `grant.expiring`, `budget.hit`, `connection.needs_reauth` and the delegation kinds go out. `run.finished` goes out only as a warning (the effective mode shows `warn`). `trigger.fired` does not go out.
* **Quiet hours.** `HH:MM` in whole minutes and an IANA time zone. The window is `[start, end)` and crosses midnight when `start` is after `end`. Start and end may not be the same minute.
* **During quiet hours** every channel delivery is held, approvals included, except kinds in `always`. Held deliveries go out when the window ends, as one message when there are several. The record is in the list at once.
* **Rate.** At most 30 immediate posts per user and channel per hour. The rest are held and folded into one "and N more" message.

There are no digests yet.

## Contacts

A contact is the verified account a user has on a channel. It is never a login field.

1. The user starts a link (`vam notifications link slack` or `POST /v1/me/contacts {channel}`). `vamd` returns a ten-character code, once, and keeps only its hash. The code lives ten minutes and works once. The user may name the account it is for (`address`); then only that account can verify.
2. The user sends the code from that Slack account to the VAM bot. The channel broker relays it to `vamd`, which checks it against the hash. Five wrong tries on one contact, or from one account in a window, lock it out.
3. The account is now the user's verified contact. A channel account is verified for one user only, and a node accepts one Slack workspace.

Only a verified, unrevoked contact receives channel messages. Starting a link needs the adapter to be enabled by the operator, and a user with a verified contact on a channel must unlink it first. Unlinking revokes the contact and every open decision handle minted for it, so an unlinked account cannot decide from a message it already holds.

## Planned

* Email, mobile push and digests as further channels. The node's channel list holds `slack` only.
* `notify_send`, a tool for an agent to message its user. Only the read-only `notifications_list` exists.
* The three delegation kinds above, when the report-back path calls them.

## Design notes

* One record per event and user, written by `vamd` in the transaction of the fact it reports. It is a projection, not a second event bus.
* Brokers never write records. A channel broker delivers a rendering `vamd` built and relays a person's click back; the record is `vamd`'s.
* Decision facts live in `vamd` (the record, the approval, who decided from which surface). Delivery facts live in the broker (see [Notify Broker](/hosted/notify/)).
* Agent text never enters a row. It is attached at read time and labelled, so the store holds no conversation bytes and every surface shows the same warning.
* Agents read and never act. The tool lists, and a person decides.
* A decision kind is cleared only by deciding. Reading is not acting, so nothing hides a pending approval except an answer to it.
