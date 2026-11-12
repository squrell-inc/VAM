---
title: Notify Broker
status: shipped
proves:
  - decision-in-the-app-updates-the-slack-message
  - simple-jigs-render-in-slack-and-the-rest-open-in-vam
  - slack-decision-needs-the-handle-and-the-linked-account
  - gate-decided-from-slack-names-slack-on-the-receipt
  - quiet-hours-hold-slack-until-morning
  - unverified-contact-receives-nothing
  - brokers/vam-notify/tests/broker_flow.rs
  - brokers/vam-notify/tests/serve_uds.rs
description: The first-party notification channel broker (vam.notify) delivers vamd's notification records to Slack, relays button clicks back, and keeps only delivery facts.
---

`vam.notify` is a first-party HSP broker (package `vam-notify`, binary `vam-notify-broker`, source in `brokers/vam-notify`). It carries [notification records](/orchestrator/notifications/) from `vamd` to a chat channel and carries a person's click back. Slack is the only adapter today.

The broker exports **no tools**. It is a channel, not something an agent calls. It never writes a notification record and never decides anything: `vamd` does both.

## How a message travels

```
record written  ->  vamd creates a delivery row  ->  DeliverNotification  ->  broker  ->  Slack DM
Slack button    ->  broker (Socket Mode)         ->  ActOnNotification   ->  vamd decides
```

1. `vamd` writes a record. In the same transaction it creates a delivery row for each channel that is enabled, has an approved broker, and has a verified contact for the user, if the user's preferences allow that kind.
2. A worker in `vamd` sends due rows over the broker's HSP socket with `DeliverNotification`. The call carries a **neutral rendering** (title, body, actions, optional jig, open link, CLI line), the contact's address, the bot token for this one call, and, for a decision kind, a one-use action handle.
3. The broker's adapter turns the rendering into a channel message and answers `SENT`, `DUPLICATE`, `RETRY`, `PERMANENT_FAILURE` or `UNSUPPORTED`.
4. When the person presses a button, the broker relays it to `vamd` on its gateway socket with `ActOnNotification`. `vamd` runs its checks and decides through the same code as `POST /v1/approvals/{id}/decide`.
5. When the record is resolved from anywhere, `vamd` queues an `update`. The broker edits the original message to say who decided, where and when, and removes the buttons. Unsent deliveries for that record are suppressed.

## The adapter interface

Each channel is a `ChannelAdapter` in the broker. Nothing in `vamd` knows a channel's format.

| Part | Does |
|---|---|
| `capabilities` | Lists the jig primitives the channel draws natively, the largest select, whether decisions can come back, and whether a verified contact is needed. |
| `deliver` | Sends or edits one message from a rendering, a binding (address, ids, an optional original message) and per-call credentials. Returns an outcome, an error code, a retry hint, the vendor message id and the actions the message really carries. |
| `receive` | Turns a channel event into a relay: a **decision** (a button or form submit) or a **link code** (a direct message to the bot). |
| `reply` | Tells the person something short in the channel, such as a link result. |
| `connect` | Opens the live connection with the app-level token, used once and dropped. |

A relay has no surface field and names no user. The broker sends the delivery id, the Slack account (`team/user`) and the action id; `vamd` supplies everything else from the handle it issued.

## The Slack adapter

* **Direct messages only.** `conversations.open`, then `chat.postMessage`; `chat.update` for an edit.
* **Socket Mode.** The broker keeps a live connection to Slack using the app-level token, fetched from `vamd` on start and on every reconnect, with back-off from 2 s to 60 s. There is no inbound HTTP endpoint and nothing to sign. The broker drops events from a workspace other than the pinned one (the node's linked workspace).
* **Message layout.** A header with the title, the body, the agent's text fenced and labelled, native blocks for a jig, then buttons, then an "Open in VAM" link (when the operator set an app URL) or "In a terminal: `<command>`".
* **Errors.** Slack rate limits and server errors are `RETRY`. Codes that cannot get better (`user_not_found`, `account_inactive`, `invalid_auth`, `missing_scope`, a bad blocks payload and similar) are `PERMANENT_FAILURE`.
* **Limits.** Header 150 characters, a text block 3000, a button label 75, at most 25 buttons.

## Provenance

* The title and body are written by `vamd` from a fixed template. Agent-written text (a tool's argument preview, a question, a gate prompt) travels in a separate `agent_text` field of at most 1 KiB, with control characters removed and any run of three backticks defused.
* The adapter shows agent text inside a code fence under a label: "written by <agent>, not verified by the host", or "from the playbook" for a gate.
* The tool approval buttons are **Approve** and **Deny** only. There is no "always allow" or "always deny": a standing rule is set in the app or the CLI.
* An update shows "Decided by <user> in <surface> at <time>" and no agent text.

## Jigs in channels

`vamd` offers the jig for a decision only when the **whole** jig renders natively, and the broker checks again.

| Primitives drawn natively | `Stack`, `Text`, `Badge`, `KeyValue`, `Button`, `Input`, `TextArea`, `Select`, `MultiSelect`, `Toggle`, `DateInput` |
|---|---|
| Not drawn | `NumberInput`, `FileInput`, `ProgressBar` |
| Other limits | A select with more than 100 options. A button whose action is not a submit. |

What each kind shows:

| Record | In Slack |
|---|---|
| Tool approval | Approve and Deny, plus the tool name. |
| Question | The question as agent text and a form with Send; choices become a select. |
| Gate with options | One button per option. |
| Gate with a response schema that renders natively | The generated form and Submit. |
| Gate that asks for a file, or any jig that does not render natively | Title, body and "Open in VAM" only. No fields and no buttons. A form submit is offered only when its form is drawn. |

Inputs come back as `vam_in:`-prefixed form fields and become the gate's response. Decide it in the app or CLI when the message has no buttons.

## Deciding from Slack

A button press is accepted only after `vamd` has checked all of these, in order. The broker is the socket a call arrives on, so no request field names it.

1. The handle parses and its hash matches. It belongs to this broker's socket, is not expired and not used. A used or revoked handle answers `ALREADY_RESOLVED` when its record is closed, else `INVALID`.
2. The record is still open. If not, the answer says who decided it, from which surface, and when.
3. The Slack account is a verified, unrevoked contact of the handle's user, and the handle's own contact. An account linked to a different user answers `INVALID`.
4. The user is active.
5. The decision is one the message offered. `AMENDED` and any remember word are never accepted, and a gate or question response must validate.
6. The broker is within its rate limit: `maxActionsPerMinute` in the manifest, where 0 means 60.
7. `vamd` decides as the handle's user with **the surface stored on the handle**, which comes from the operator-approved manifest (`slack`). Remembering is not allowed from here.

The handle is bound to the record, the approval, the user, the contact, the channel, the offered decisions and the surface. It expires with the approval (seven days when the approval has no deadline). It is used up by the decision's own transaction, and any decision from anywhere revokes every handle for the record. Because the surface comes from the handle, the receipt shows `slack` and the linked user as the decider.

When a click fails, the broker tells the person in the channel, privately where Slack allows: not linked, already decided by someone, expired, not offered, or "try again in the app".

Linking works through the same path: a direct message to the bot with a link code becomes `LinkChannelAccount`, and the person is told the result. A node accepts one Slack workspace; an account in another workspace is refused and does not use up the code.

## Operator configuration

An operator turns Slack on in four steps.

1. **Create a Slack app** with a bot token and an app-level token, and store each as an **org-scope** vault secret. Tokens are never sent to the settings API.
2. **Register the broker** with the manifest the binary prints, using `PUT /v1/hosted/brokers` and a key with `brokers:admin`:

   ```bash
   vam-notify-broker manifest
   vam-notify-broker --socket /run/vam/notify.sock --state-dir /var/lib/vam-notify \
     --gateway-socket <path vamd reports for the broker>
   ```

   The manifest declares trust tier first-party, no secret bindings, no tools, the egress domains `slack.com`, `wss-primary.slack.com` and `wss-backup.slack.com`, and `notificationChannel {channel: "slack", surface: "slack", maxActionsPerMinute: 60}`. Run the broker as the same user as `vamd`. When registration is approved, `vamd` opens a gateway socket for it and reports the path in the broker record; pass it as `--gateway-socket`.
3. **Set the node's notification settings:**

   ```bash
   vam node notify set --slack on \
     --bot-token-ref notify/slack/bot-token --app-token-ref notify/slack/app-token \
     --app-url https://vam.example.com --kind run.finished=off
   vam node notify          # show
   ```

   This is `PUT /v1/node/notify`, which needs `brokers:admin` and an idempotency key. Values are vault secret **names**. Each must exist as an org secret; enabling Slack needs both. `appUrl` must be an `https` URL without credentials. `kindDefaults` give the org's default per kind (`immediate` or `off`); a user's own setting wins.
4. **Users link their accounts** (see [Contacts](/orchestrator/notifications/#contacts)).

Gateway access is limited. A call is refused unless the broker has an enabled approval, its trust tier is core or first-party, its manifest declares a notification channel, and, when the approval records a peer uid, the caller's uid matches. The channel config, with the tokens, is handed out only when the node enables the channel, and only for that call. When the broker starts or reconnects, `vamd` revokes its open handles and queues an edit of each still-open sent decision that carries the buttons and a new handle.

A broker started with `--fake-transport` exists only in test builds. A normal build contains no fake transport and rejects the flag.

## Delivery facts and decision facts

Each side keeps only what it needs.

| In `vamd` (decision facts and delivery intent) | In the broker (delivery facts) |
|---|---|
| The record, its approval, who decided from which surface. | A delivery log with one table: delivery id, notification id, channel, Slack channel id, message timestamp, sent time, attempt count, last vendor status. |
| Delivery rows: which record goes to which contact over which channel, the state (`pending`, `sent`, `held`, `failed`, `suppressed`), the hold reason, attempts, last error. | Action handles and the actions each message offered, **in memory only**. A restart drops them, and `vamd` re-mints them as edits. |
| Contacts (user to verified account), hashed link codes, hashed handles. | No address, no token, no handle, no link code and no agent text in the log. Every value is checked before it is written and may not look like a Slack token or a VAM handle. |
| The vault secrets and the node settings. | The tokens only for the length of one call. |

`vamd` does not store a Slack message id. When it wants an edit it names the original delivery id and the broker maps it to the message.

`vamd` retries a delivery after 30 s, 2 min, 10 min, 30 min and 1 h, then hourly, and marks it `failed` 24 hours after it was created. It sends at least once, and the delivery id is the broker's idempotency key: a repeat answers `DUPLICATE`. The state directory is mode 0700 with a lock, so one process owns it. Metrics: `vam_notify_deliveries_total{channel,state}` and `vam_notify_gateway_total{rpc,result}`.

## Planned

* Email and mobile push adapters.
* Digests.
* `notify_send`, a tool for an agent to message its user.

## Design notes

* Brokers never write records. The broker has no authority of its own: a click is a claim that `vamd` checks against a handle it issued, and the surface comes from the manifest the operator approved.
* Decision facts stay in `vamd`, delivery facts in the broker, so the broker's store can be read and dropped without losing a decision.
* `vamd` builds one neutral rendering. An adapter decides how to draw it, so a new channel does not change `vamd`.
* A jig is shown in a channel only if all of it can be drawn. Half a form would let a person decide on less than the app shows.
* Secrets are per call and handles live in memory. A restart costs a re-minted button, not a stored secret.
