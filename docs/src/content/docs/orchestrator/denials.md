---
title: Denials and fixes
status: shipped
proves:
  - denied-tool-tells-the-model-who-can-allow-it
  - fix-line-ignores-text-the-tool-wrote
  - egress-refuses-an-ungranted-host
  - missing-secret-refusal-names-the-secret-to-set
  - member-asks-an-org-admin-to-enable-a-connector
  - member-cannot-file-a-fix-request-for-someone-elses-denial
  - cli-prints-the-fix-from-the-error-body
description: How a refusal says what would fix it and who can do it, the Denial object, where it appears, the line the model reads, and how a member asks someone else to apply the fix.
---

A refusal in VAM tells you two things: what would fix it, and who can do it. The host writes both. Nothing a tool or the model wrote is ever part of them.

The refusal's own message text does not change. The fix rides beside it as a typed `Denial`.

## What a denial is

A denial is a refusal that has a reason, a subject and, when someone can change something, a fix. These refusals carry one:

- a tool call refused by policy;
- an egress request to a host the agent may not reach;
- a run that needs a secret nobody has set;
- a tool that needs a connector that is off for the organization, or not connected;
- a run pinned to a version the Space is not bound to;
- a run whose inbox is full.

## The Denial JSON

```json
{
  "reason": "POLICY_RULE",
  "subject": "jira_create_issue",
  "fix": {
    "change": "ALLOW_TOOL",
    "target": "urn:vam:agent:hub.example.dev/acme/helper#jira_create_issue",
    "who": "AGENT_OWNER",
    "route": { "method": "PUT", "path": "/v1/agents/urn:vam:agent:hub.example.dev/acme/helper/permissions" },
    "callerCanApply": false
  },
  "text": "The agent's owner can allow jira_create_issue for this agent.",
  "decisionRef": "dnl_..."
}
```

| Field | Meaning |
|---|---|
| `reason` | Why: `POLICY_RULE`, `EGRESS_RULE`, `SECRET_MISSING`, `CONNECTOR_NOT_CONNECTED`, `CONNECTOR_DISABLED`, `VERSION_STALE`, `QUOTA`. `AGENT_NOT_GRANTED` and `SPACE_BUSY` are reserved. |
| `subject` | A host identifier: the tool, host, secret binding, connector or version. At most 253 bytes, no control characters. |
| `fix` | Absent when nobody can change anything. |
| `text` | One host sentence. Always present. |
| `decisionRef` | `dnl_` and 26 characters. Present only when the caller cannot apply the fix, so a request can be sent. |

### Changes

`fix.change` is one of `ALLOW_TOOL`, `ALLOW_HOST`, `ENABLE_CONNECTOR`, `CONNECT_CONNECTOR`, `SET_SECRET`. `SET_SPACE_VERSION`, `REQUEST_AGENT_ACCESS` and `STOP_RUN` are reserved.

### Who

`fix.who` is `SELF`, `AGENT_OWNER` or `ORG_ADMIN`.

`fix.route` is the existing write route that applies the change, for API callers. It is absent when there is no single route.

### `callerCanApply` is a hint

`callerCanApply` says whether the person who got the refusal can apply the fix. It is for drawing a button. It grants nothing. The route you call still checks your rights.

## Which refusal gives which fix

| Refusal | Reason | Fix | Who |
|---|---|---|---|
| Tool: no rule names it and the default is deny (own agent, caller owns it) | `POLICY_RULE` | `ALLOW_TOOL` | `SELF` |
| Tool: same, caller does not own the agent | `POLICY_RULE` | `ALLOW_TOOL` | `AGENT_OWNER` |
| Tool: the agent's owner denied it by name | `POLICY_RULE` | none, text only | |
| Tool: an org-scope rule denies it | `POLICY_RULE` | none, text only (names the org admins) | |
| Tool: an agent installed from a publisher | `POLICY_RULE` | none, text names the publisher | |
| Tool: a legacy policy, a built-in or skill rule, a path limit | `POLICY_RULE` | none, text only | |
| Tool: needs a connector the server does not have | `CONNECTOR_DISABLED` | none, text says to ask an org admin | |
| Tool: connector is off for the organization | `CONNECTOR_DISABLED` | `ENABLE_CONNECTOR` | `ORG_ADMIN` |
| Run parked on a connector connection | `CONNECTOR_NOT_CONNECTED` | `CONNECT_CONNECTOR` | `SELF` |
| Egress: host not granted by the agent's rules | `EGRESS_RULE` | `ALLOW_HOST` | `AGENT_OWNER` |
| Egress: an explicit host deny, an installed agent, the IP floor, the port list, a secret destination | `EGRESS_RULE` | none | |
| Secret missing, user scope | `SECRET_MISSING` | `SET_SECRET` | `SELF` |
| Secret missing, org scope | `SECRET_MISSING` | `SET_SECRET` | `ORG_ADMIN` |
| Version mismatch (409) | `VERSION_STALE` | none yet; the text says to run on the bound version | |
| Run inbox full (429) | `QUOTA` | none; the text names the org admin | |

An explicit deny is a choice, so it gets text only. Only a default deny gets `ALLOW_TOOL` or `ALLOW_HOST`. The `VERSION_STALE` fix has no change until Space upgrade ships.

## Where denials appear

- **Tool responses.** The denied tool message the model reads ends with the fix line (below).
- **`policy.decided`.** The event carries `denial`.
- **`egress.decided`.** A deny carries `denial`. The proxy's refusal body adds one fixed phrase when the owner can allow the host. Each session stores at most 64 host rows.
- **HTTP error bodies.** The body gains `denial` (the first) and `denials` (the list). Most errors have neither. The 412 for missing secrets has one denial per secret.
- **`connection.needs_reauth`.** The notification carries a `denial` with `CONNECT_CONNECTOR`. See [Notifications](/orchestrator/notifications/).

## The model's line

The denied tool message may end with one line:

```
Suggested fix (for the user, not for you): the agent's owner can allow jira_create_issue for this agent.
```

The guest builds it from a fixed table keyed by `(reason, change, who)`, and the host `subject`. Safety rules:

- It never reads `text`, `route`, `target` or `decisionRef`.
- Text a tool wrote, or a tool name the model made up, never reaches it.
- A subject that is not a plain identifier (`A-Za-z0-9._-`, up to 128 bytes) gives no line.
- No fix, an unknown value or a combination not in the table gives no line.
- It never names a route and never tells the model to retry, try another tool, or ask for wider rights.

## Fix requests

A member who cannot apply a fix can ask the person who can. `decisionRef` is the key.

`POST /v1/fix-requests` with `{"decisionRef": "dnl_..."}`. It needs scope `runs:write` and an `Idempotency-Key`. The host reads the stored denial and works out the change, target and recipients itself. Anything else in the body is ignored.

| Status | Meaning |
|---|---|
| 201 | Filed. The body has `id`, `change`, `target`, `who`, `createdAt`, `asked` and `existing: false`. |
| 200 | The same request was filed in the last 24 hours. Nobody is told again. `existing: true`. |
| 404 | `denial not found`. One answer for every bad reference: unknown, someone else's, nothing to ask, or older than 30 days. |
| 409 | Nobody can be asked right now (installed agent, owner gone). |
| 429 | More than 10 new `ALLOW_HOST` requests for one agent from you in 24 hours. |

You can only file a request for your own denial. A reference older than 30 days answers 404.

Recipients are worked out at request time. The agent's owner, or the active org admins, are told. You are never a recipient, and a deactivated user is skipped. A request asks for `ALLOW_TOOL`, `ALLOW_HOST`, `ENABLE_CONNECTOR` or `SET_SECRET` only.

Each recipient gets a `fix.requested` notification. It is not a decision: the admin closes it with `acted`. See [Notifications](/orchestrator/notifications/).

## In the CLI

When the error body has a denial, `vam` prints it before falling back to message matching:

```
Wrong: ...
Expected: An org admin can enable microsoft-365 for this organization.
Ask: an org admin to enable microsoft-365; vam fix request dnl_...
```

- `Fix:` is a command you can run yourself.
- `Ask:` is a fix someone else applies. It ends with the `vam fix request` command.

`vam fix request <decisionRef>` calls the route above and prints `Asked <who>. Request <id>.` A repeat prints `Already asked; nobody was notified again.` `--json` prints the body. A reference that is not `dnl_` plus 26 letters and digits exits 2 without a request.

See also [Host API](/orchestrator/api/#errors).
