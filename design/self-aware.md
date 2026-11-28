# Self-aware: denials that carry their fix, and one-click agent upgrade

**Status:** Proposed (lane 3 of the next lanes, 2026-11-28). Every decision below is "proposed, owner to confirm" unless it quotes an owner-approved answer.
**Date:** 2026-11-28
**Owner:** maintainer
**Repos:** `squrell-inc/VAM`, Squrell-App
**Tracker:** `.cursor/workstreams/self-aware.md`
**Branches:** `fix/typed-denials` (S1), `fix/space-upgrade` (S2); S3 and S4 get their branches when they start.
**Related lanes:** Apps and Spaces (`design/vam-apps-spaces.md` "Amendment 2026-11-28", tracker `.cursor/workstreams/apps-spaces.md`, phases P0-P7); VAM image (`design/vam-image.md`, `.cursor/workstreams/vam-image.md`, I1-I4); Long work (`design/long-work.md`, `.cursor/workstreams/agent-runtime.md` Phase 5, 5a-5e).

## Decision record

**Context.** When VAM says no, the person who hit the refusal usually cannot tell
what would make it yes, or who can change it. When an agent gets better, a Space
that already ran it never sees the new version. Both are the same gap: the host
knows the fix and does not say it.

1. A Cedar deny carries a diagnostic string and the matching policy ids, nothing
   typed (`crates/vam-security/src/pdp.rs:13-21`). The host turns it into a
   `ToolIntentResponse` with `code: 7` and `details: vec![]`
   (`crates/vam-orchestrator/src/security.rs:1962-1976`); the four other
   `PERMISSION_DENIED` sites in that file do the same (`:1093`, `:1190`,
   `:1278`, `:1512`). `ErrorStatus.details` (`repeated google.protobuf.Any`)
   exists and is unused (`proto/vam/v1/common.proto:172-177`).
2. The model sees `Error: Tool 'x' was denied by security policy: <diag>`
   (`crates/vam-runtime/src/agent.rs:2486-2496`). It cannot tell the user who
   could allow the call.
3. An egress refusal is a string: `Access to host '{host}' is blocked by active
   network policy (rule: {rule})` (`crates/vam-network/src/broker.rs:1253-1255`);
   an exceeded byte quota is another string (`broker.rs:1288-1290`).
4. A missing secret is a 412 at admission
   (`crates/vam-orchestrator/src/secret_requirements.rs:226-252`,
   `api/error.rs:230-241`). The CLI prints a `Fix:` line, found by matching the
   message text (`crates/vam-cli/src/http_error.rs:143-150`).
5. A connector that is enabled but not connected parks the call and emits
   `connection.needs_reauth`; Squrell renders `ConnectToContinueCard`
   (`Squrell-App/src/components/chat/ConnectToContinueCard.tsx`). That is the one
   refusal today with a working button. A connector that is not enabled, or not
   known, is a dead end: "ask your admin to enable {name}"
   (`crates/vam-orchestrator/src/connector_park.rs:114`, `:133`). No request
   reaches the admin.
6. On `feat/agent-access` a visible but not granted agent answers 403
   `PermissionDenied` with the message prefix `agent_access_required:` and has a
   request flow (`design/agent-access.md` on that branch, §2.3 and §4, decision
   A8). The CLI matches the prefix (`http_error.rs:246` on that branch).
7. The fix machinery that exists is scattered: Always allow / Always deny writes
   a rule on the agent's draft, only on a draft run by the draft's owner
   (`api/decide.rs:70-107`, `session_targets.rs:636-662`); the permissions
   explain route names the deciding scope and rule
   (`GET /v1/agents/{urn}/permissions/explain?tool=|host=`,
   `api/permissions.rs:100-118`, `permissions_view.rs:118-145`); the CLI turns
   message text into Wrong / Expected / Fix lines (`http_error.rs`).

On versions:

8. The `spaces` row holds `agent_urn` and `app_id`, no version
   (`crates/vam-orchestrator/src/spaces.rs:172-185`); `bind_space_agent` sets the
   URN on the first run only (`spaces.rs:376-389`). The version pin lives in the
   workspace: `WorkspaceMeta.agent_version` in `.vam/workspace.json`
   (`crates/vam-storage/src/vam_dir.rs:114-125`), written at session start by
   `prepare_thread_context` (`api/mod.rs:3401-3428`). A different version is
   refused (`vam_dir.rs:316-328`).
9. `apply_workspace_version_pin` fills an unversioned run from the pin and
   refuses a run that names another version
   (`catalog_resolve.rs:58-114`, called at `api/run_start.rs:353-357`) with 409
   `agent version 'x' does not match workspace-bound version 'y'`
   (`api/error.rs:190-201`).
10. Squrell-App sends released runs with no version
    (`Squrell-App/src/types/vam.ts:1915-1938`), so the first run pins the latest
    release and the Space stays there forever. The owner question is open:
    `.cursor/workstreams/archive/connectors-phases-1-7.md:3403` ("Should an
    installed listing update move the workspace version pins…", `Decide: OWNER`).
11. Catalog tables allow a `yanked` channel (`catalog.rs:30-31`); nothing writes
    it, and DX-94 ("decide yank semantics", `developer-experience.md:1190-1193`)
    is open.
12. There is no way to change a Space's version: `api/spaces.rs:22-27` routes
    list, create, get and rename only. The CLI has a fix hint for the agent
    mismatch (`http_error.rs:126-136`) but none for the version mismatch.

**Decision (proposed).** Four phases:

| Phase | What | Base branch |
|---|---|---|
| S1 | Typed denials: every refusal carries a `Denial` with a reason and a host-written `SuggestedFix`; CLI and Squrell show a fix card whose button calls an existing write route or sends a request to the right admin; the model gets one "Suggested fix" line | `fix/typed-denials` |
| S2 | Space upgrade: GET Space shows pinned and latest version; `POST /v1/spaces/{id}/agent-version` re-pins with rollback; "Agent updated, use vX?" in Squrell and `vam space upgrade`; minimal yank (Q11) | `fix/space-upgrade` |
| S3 | Agent-proposed change card (Q15): the agent proposes a change from a closed list; a human approves; the host applies it with the approver's rights | later, after agent access is in `main` |
| S4 | App version follow and bulk upgrade: exact pin or follow latest, canary, rollback | later, inside or after Apps P2 (on `feat/apps-spaces`) |

## Audience

| Who | What changes for them |
|---|---|
| Member in Squrell | A refusal shows what would fix it and a button (connect, set secret, request access, ask an admin). A Space shows "Agent updated, use v1.3.0?" |
| CLI user | `Fix:` lines come from the error body, not from message matching; `vam space upgrade` |
| Agent owner, org admin | Requests from members arrive as notifications with the change already filled in |
| The model | One host-written "Suggested fix" line in a denied observation, so it can tell the user what to ask for |
| Integrators | A typed `denial` object on HTTP errors; `agentVersion` / `latestVersion` on Spaces |

## What we are building

### S1. Typed denials

**The `Denial` message** (new, `proto/vam/v1/common.proto`, packed into
`ErrorStatus.details` as `type.googleapis.com/vam.v1.Denial`):

| Field | Type | Meaning |
|---|---|---|
| `reason` | enum `DenialReason` | closed list below |
| `subject` | string | what was refused, as the host names it: a tool name from the host catalog, a normalized host, a binding id, a connector id, an agent URN, a version label |
| `fix` | `SuggestedFix` (optional) | absent when nothing can change it (the platform IP floor, a tampered skill) |
| `text` | string | one host-written sentence for people, from a fixed template per `(reason, fix.change, fix.who)` |
| `decision_ref` | string | the id of the stored decision row this denial came from (empty when nothing is stored, e.g. a 412 before a run); `POST /v1/fix-requests` resolves it |

`DenialReason`:

| Value | Raised at (today's code) | Usual fix |
|---|---|---|
| `POLICY_RULE` | Cedar deny, missing permissions row (`security.rs:1470-1520`, `:1962-1976`) | change the agent's permission rule (agent owner), or Always allow on the owner's own draft run |
| `EGRESS_RULE` | `broker.rs:513`, `:566` | add the host to the agent's internet rules (agent owner); none for the platform IP floor |
| `SECRET_MISSING` | `secret_requirements.rs:226-252` (412) | set the secret (self for user scope, org admin for org scope) |
| `CONNECTOR_NOT_CONNECTED` | the park in `connector_park.rs` | connect (self). Behaviour unchanged: still a park, the denial rides on the park's notification |
| `CONNECTOR_DISABLED` | `connector_park.rs:114`, `:133` | enable the connector (org admin) |
| `AGENT_NOT_GRANTED` | 403 `agent_access_required:` (agent access) | request access (agent admin) through `POST /v1/agents/{urn}/access` |
| `VERSION_STALE` | 409 version mismatch (`api/error.rs:190-201`); a yanked pin (S2) | move the Space to another version (self, S2 route) |
| `QUOTA` | inbox full (`api/error.rs:169-176`), egress byte quota (`broker.rs:1288-1290`), App quotas later (Apps P6) | text only in S1; who is the App owner or org admin |

`SuggestedFix`:

| Field | Meaning |
|---|---|
| `change` | closed enum: `ALLOW_TOOL`, `ALLOW_HOST`, `SET_SECRET`, `CONNECT_CONNECTOR`, `ENABLE_CONNECTOR`, `REQUEST_AGENT_ACCESS`, `SET_SPACE_VERSION`, `RAISE_LIMIT` |
| `target` | the identifier the change acts on (agent URN plus tool or host; binding id plus scope; connector id; Space id plus version) |
| `who` | `SELF`, `AGENT_OWNER`, `ORG_ADMIN` (`APP_OWNER` added by Apps) |
| `route` | `{method, path}` of the existing write route that applies it, for API callers |
| `caller_can_apply` | whether the caller holds the rights now (the host checked) |

**Where it goes.**

- Tool intents: every `PERMISSION_DENIED` `ToolIntentResponse` in `security.rs`
  packs a `Denial`. The deciding scope and rule come from the same decision the
  host already records (`run_facts::NewRunDecision`, `deciding_scope`,
  `deciding_rule`), so `who` follows the deciding scope: `agent` → `AGENT_OWNER`
  (or `SELF` when the caller owns the draft and the run is a draft run, the
  `plan_remember` rule, `session_targets.rs:636-662`); `host` (catalog,
  tamper, reused call id) → no fix.
- HTTP errors: `ApiError` (`api/error.rs:9-29`) gains an optional `denial`, and
  the JSON body gains `"denial": {...}` next to `code`, `message`, `retryable`.
  The message text is unchanged, so old clients and the `agent_access_required:`
  prefix (decision A8) keep working. No new `VamErrorCode`.
- Egress: the proxy's refusal carries the denial in the `egress.decided` event's
  data, and the refusal body the guest's HTTP client gets adds the one-line fix.
  The model sees it only if the tool prints the body.
- Run records: the typed denial is stored with the decision row and sent on
  `vam.v1.policy.decided` and `vam.v1.egress.decided`, so Squrell can draw the
  fix card from the run's events.
- Connector park: the `connection.needs_reauth` action is already a fix; S1
  sets `Denial{CONNECTOR_NOT_CONNECTED}` on it so clients draw one card type.

**The model's line.** `denied_observation` (`agent.rs:2486-2496`) appends one
line when a `Denial` with a fix is present:

```text
Error: Tool 'jira_create_issue' was denied by security policy: <diag>
Suggested fix (for the user, not for you): the agent's owner can allow jira_create_issue for this agent.
```

The line is built in the guest from the typed fields and a fixed template table
compiled into the guest, not from `Denial.text` or from any string a tool or
the model wrote. It never names a route, never tells the model to retry, to try
another tool or host, or to ask for wider rights. The model may repeat it to the
user. It names `who` and the change; it does not quote policy text beyond what
`<diag>` already carries.

**Fix cards (CLI and Squrell).**

| `who` and `caller_can_apply` | CLI | Squrell |
|---|---|---|
| `SELF`, true | `Fix: <exact command>` (e.g. `vam secrets set GITHUB_TOKEN --from-env <VAR>`) | button that calls the write route (set secret opens the existing secret form; connect opens the connect flow) |
| `AGENT_OWNER` or `ORG_ADMIN`, caller holds the right | as above | button |
| someone else | `Ask: <who> to <change>; vam fix request <id>` | "Ask <who>" button |

Clients choose the action from `change` (closed enum) and fill it from
`target`. They never call an arbitrary path from the body: `route` is checked
against the client's own list for that `change`, and a mismatch shows text only.

**Requests to the right person.** For `REQUEST_AGENT_ACCESS` the button calls
the agent access route. For every other fix that the caller cannot apply, the
button calls a new `POST /v1/fix-requests {denial}`. The denial carries a
reference to the stored decision row (new `Denial.decision_ref`, the id of
the row S1 stores the denial with); reason and subject alone cannot name the
recipient, since two agents with different owners can share a tool or a host.
The host resolves the reference, checks that the requester is entitled to it
(the run's caller or owner; otherwise 404, same text as a missing row),
recovers the agent, scope and target from the stored row, re-derives the fix
from those (it never trusts the client's `fix`), and sends a `fix.requested`
notification (new `Kind`, `notifications.rs:49-62`) to that agent's admins or
the org admins. The admin's
notification carries the same typed fix, so their card has the button. No
queue, no decision state in S1 (S3 adds one). One request per (requester,
change, target) per 24 hours; a repeat answers 200 with the existing request.

**The CLI** reads `denial` first and falls back to today's message matching
(`http_error.rs`) for servers without it. It adds the missing hint for the
version mismatch: `Fix: vam space upgrade <space> --to <bound or requested>`.

### S2. Space upgrade

**Where the pin lives.** The Space row becomes the authority for a Space's
version. `spaces` gains `agent_version TEXT` and `previous_agent_version TEXT`
(both nullable). A row with `agent_version` NULL takes the workspace file's pin
on its next read (lazy backfill), which keeps every existing Space on its
version. `apply_workspace_version_pin` reads the row first. At session start,
when the row's version differs from `workspace.json`, the host rewrites
`agent_version` and `agent_digest` in the file through a new
`vam_storage::repin_workspace_json`; from then on the file's pin means "last
version run here", not the authority. The refusal at `vam_dir.rs:316-328`
stays for every other path. This matches the comment at `run_start.rs:333` ("the Space
row holds the bound agent") and the Apps plan's "App pin is authoritative"
(AS-2.6, `design/vam-apps-spaces.md:599-601`).

Rejected: writing a new `workspace.json` from the API route. The workspace file
is read from the committed Drive9 head (`api/mod.rs:3635-3643`), so a re-pin
from a route would need a host commit outside a run, under the writer lock, for
one field.

**The seam.** One store function, `set_space_agent_version(owner, space_id,
version) -> SpaceVersionChange` (the name and shape the Apps plan already uses,
`design/vam-apps-spaces.md` §5 "Seam shared with lane 3"), holds every check
below. The route and the CLI call it. S2 builds and owns it. Apps P4 (AS-2.6,
lane 1) calls it to route a shared App's Spaces to the App pin (Q10), so App
pin moves and personal re-pins share one write path.

**Routes.**

`GET /v1/spaces/{id}` and `GET /v1/spaces` add:

| Field | Meaning |
|---|---|
| `agentVersion` | the pinned label (`1.2.0`, `1.0.0-draft`), empty before the first run |
| `previousAgentVersion` | the pin before the last change, for rollback |
| `latestVersion` | the newest released, not yanked version the caller may run; empty for a draft pin |
| `updateAvailable` | `latestVersion` is newer than `agentVersion` |
| `pinnedVersionYanked` | the pin was yanked (Q11) |

These are new fields on `vam.v1.Space` (`proto/vam/v1/orchestrator.proto:1176-1184`).

`POST /v1/spaces/{id}/agent-version {version: "1.3.0" | "latest", rollback?: true}`,
with `Idempotency-Key`:

| Check | Refusal |
|---|---|
| Caller owns the Space (`space_for_route`, as `get_space` does) | 404, same text as a missing Space |
| Same agent URN; switching agents is not an upgrade | 400 |
| Target is released and not yanked, or the caller's own draft (isolation-verify-closeout decision 6 and its 2026-11-17 amendment) | 404 for a foreign draft; 409 for a yanked version |
| The caller may run the agent (agent access, once in `main`) | 403 `agent_access_required:` with its denial |
| Nothing live, queued or parked on the Space (`workspace_busy` and `busy_run`, `api/inbox.rs:87`, `:145-148`, which cover the live session and the parked reservation, plus queued runs) | 409 `space_busy: run '<id>'` with a denial whose fix is "wait or stop that run" |
| Thread compatibility (below) | not a refusal; the answer lists affected threads |

The busy check and the pin write are one serialized step: they run under
the same per-Space guard that run admission (`inbox::admit`,
`api/inbox.rs:407`) holds, so no run can be admitted between the check and
the write and a Space is never re-pinned while a run is starting.

On success: `previous_agent_version := agent_version`, `agent_version :=
target`, one audit record (`vam.v1.space.agent_version_changed`, through the
audit ledger, `vam-telemetry/src/audit.rs:92`), and an `agent.updated`
notification (new `Kind`, severity `info`) to the Space owner so other surfaces
(Slack, a second app window) refresh. The answer is the Space view plus
`threadsStartingFresh: [threadId]`. `rollback: true` swaps the two pins.

**Thread compatibility.** A thread's log records what the model saw: the agent
digest at task start (`thread_recorder.rs:710-740`) and the tools by digest per
exchange (`thread_recorder.rs:1085-1089`; `ThreadContext.tools_sha256`,
`vam-spec/src/thread_context.rs:86-88`). Replaying that history under a version
whose tools changed is the risk: a history that calls a tool absent from the
new request is refused by some model providers and misleads the model on
others, and a changed input schema invites wrong calls. The check, run inside
`set_space_agent_version`:

1. Collect the tool names the thread's log called (tool calls in the log's
   assistant messages).
2. Compare with the new version's host tool catalog (skill tools plus built-ins).
3. A thread that called a tool the new version lacks, or whose input schema
   digest changed, is marked `fresh_start`.

The next message to a `fresh_start` thread runs in a new thread linked by
`ThreadMeta.parent_thread` (`vam_dir.rs:144-157` already has the field), with
no carried context in S2. Squrell shows "This conversation used tools the new
version no longer has. It continues in a new conversation" with a link back.
Threads that pass the check continue as they are. The log is never rewritten.
Rollback clears the marks it set.

**Squrell.** The Space view shows "Agent updated, use v1.3.0?" when
`updateAvailable`; one click calls the route; a busy Space shows "after the
current task". A `pinnedVersionYanked` Space shows a blocking prompt: the
composer is disabled until the user upgrades (Q11). The release note of the new
version is shown when the catalog has one.

**CLI.** `vam space show <space>` prints pinned, latest and previous;
`vam space upgrade <space> [--to <version>|--latest] [--rollback]`. The
version-mismatch `Fix:` from S1 points here.

**Minimal yank (Q11, proposed).** `POST /v1/agents/{urn}/versions/{v}/yank
{reason}` sets the row's channel to `yanked`, for the agent's publisher (and,
after agent access, its agent admins and org admins). A new run on a yanked
version is refused 409 with `Denial{VERSION_STALE, fix: SET_SPACE_VERSION
latest}`. A running session finishes on it (answers DX-94; policy already does
not hot-reload into running sessions). A draft cannot be yanked. If no other
release exists the fix is `who: AGENT_OWNER`, "release a new version". The
latest-release lookup must skip yanked rows; S2 checks it does.

### S3. Agent-proposed change card (Q15, after agent access is in `main`)

The agent can propose one change from a closed list. A person approves it. The
host applies it with the approver's rights. Nothing changes without a person.

**How the agent asks.** A built-in tool `propose_change {change, target,
reason}` with a closed JSON schema. The host validates it and opens an approval
of a new kind `change_proposal` (next to `tool_call`, `gate`, `question`,
`hitl.rs:15-23`), drawn with a new built-in jig `vam.change_proposal`
(`design/jig.md`). The run waits as for `ask_user`.

**Closed list (v1).**

| `change` | Host validation | Applied by |
|---|---|---|
| `CONNECT_CONNECTOR` | connector is enabled and the run's skill needs it | the member's own connect flow; the parked call resumes |
| `SET_SECRET` | binding is declared by a skill of the run | opens the secret form; the agent never sees the value |
| `ALLOW_TOOL` | tool is in the agent's catalog | the draft permissions write (`PUT /v1/agents/{urn}/permissions`); takes effect where that write takes effect today (next release, or a draft run) |
| `ALLOW_HOST` | host is declared in a skill's egress of this agent; never an arbitrary host | same write |
| `ENABLE_CONNECTOR` | connector is installed on the node | org admin route `PUT /v1/connectors/{id}/node` |

**The card.** Fixed text from a host template ("Allow this agent to reach
api.example.com"), the target as data, and the agent's `reason` shown below,
labelled "The agent's reason", plain text, at most 280 characters, no links. The
card names the risk class of the change.

**Who approves.** The run's approver. If they lack the right, the card offers
"Send to <who>": the proposal goes to the admin queue that agent access built
(`GET /v1/access-requests`, `POST /v1/access-requests/{id}/decide`), extended
with a `kind`. The admin's approval applies it with the admin's rights. Cedar
still gates the next call: an applied change is a normal write, not a pass.

**Limits.** At most 3 proposals per run; one per (change, target) per thread
per 24 hours after a refusal; a proposal cannot be decided from a notification
surface (the gateway passes no standing-rule rights today, `decide.rs:70-74`),
only in the app or the CLI.

**Threat model (prompt injection).** The attacker controls text the agent read
(a web page, an email, a tool result) and makes the agent propose a change.

| Attack | Defence |
|---|---|
| Propose egress to the attacker's host | `ALLOW_HOST` accepts only hosts already declared in the agent's skills |
| Persuasive reason text | fixed card text; the reason is labelled as the agent's, capped, rendered as plain text |
| Many proposals until one is clicked | per-run cap and per-target cooldown |
| Proposal applies with more rights than the approver | applied through the approver's own route call; no host privilege |
| Approve from a phishing-like notification | not decidable from notification surfaces |
| Agent writes settings directly | no guest-side write path exists or is added; only the host applies |

### S4. App version follow and bulk upgrade (inside or after Apps P2)

Under C1 an App holds a pinned agent and version. S4 adds to the App row:

| Field | Meaning |
|---|---|
| `version_mode` | `exact` (default) or `follow` (latest release, skipping yanked) |
| `previous_agent_version` | for rollback |
| `canary` | optional: a list of Space refs, or a share of Spaces, that get a new version first |

One write upgrades every Space of a shared App (Q10); live runs finish on the
old version; the next run uses the new pin. With C2 (one App per environment)
the usual canary is staging first, then prod; the in-App canary is for prod
Apps with many Spaces. Rollback restores `previous_agent_version`. `follow`
moves on release; a yank moves `follow` Apps back to the newest unyanked
release and blocks `exact` Apps with the S2 prompt to the App owner.
Turning on `follow` is the human approval for those later automatic moves:
it is the App owner's explicit opt-in, revocable by setting `exact`, and
the App owner is notified on each move (`agent.updated`).
Unlike the personal re-pin, an App pin move does not wait for live runs (it
cannot wait on many Spaces): each live run finishes on the spec it was
resolved with, and the S2 thread check runs per Space at its next run. A personal
user's "upgrade all my Spaces on this agent" is a CLI loop over the S2 seam,
not a new route. S4's items are planned with Apps P2 and P4 so that lane owns
`run_start.rs` and `catalog_resolve.rs` while S4 builds (Q16).

## Why

- The cheapest fix is the one the host already knows. Every refusal site listed
  above has the reason and the subject in hand; S1 only stops throwing them
  away.
- A typed object beats message matching: `http_error.rs` breaks whenever a
  message is reworded, and Squrell cannot match at all.
- One button per refusal turns "ask your admin" into a request the admin can
  act on.
- The version pin was right for safety (a run does not change under a user) and
  wrong as a permanent state. An explicit, owner-only, reversible re-pin keeps
  the safety and ends the silent freeze.

## Locked decisions this touches

| Decision | Effect |
|---|---|
| isolation-verify-closeout 5 ("Workspace pins agent URN + version at create time") | **Owner question; proposed change:** the pin is set at the first run and may be changed later only by the Space owner through `set_space_agent_version`. The tracker's "Explicitly later: Workspace migrates from one agent version to another in place" moves into S2. The owner confirms before S2 is broken down |
| isolation-verify-closeout 6 (who may pin draft) | kept: the re-pin target follows the same rule |
| agent-access A8 (no new `VamErrorCode`, `agent_access_required:` prefix) | kept: `denial` is an extra body field; the prefix stays |
| agent-runtime 23 (conversation lives in the workspace log) | kept: the log is never rewritten; a `fresh_start` thread is a new thread |
| decide.rs standing-rule rule (only app and CLI set standing rules) | kept, and reused for S3 proposals |

## Phases

### S1 — Typed denials

**Depends on:** none to start. `AGENT_NOT_GRANTED` is wired when agent access
is in `main`; until then it is reserved.
**Exit:** every refusal site in the table above returns a `Denial`; the model's
observation carries the fix line; the CLI prints fixes from `denial`; Squrell
draws a fix card from the error body and from run events; a member's "ask your
admin" sends a `fix.requested` notification the admin can act on.

Scenario delta (proposed; the breakdown confirms against `guards` and `given`):

| Scenario | Change | Layer | Given | Why |
|---|---|---|---|---|
| `denied-tool-tells-the-model-who-can-allow-it` | new | process | `org-member`, `agent-with-guarded-tool` | the observation's fix line |
| `egress-refuses-an-ungranted-host` | changed: the `egress.decided` deny carries a denial naming the agent owner | process | `org-admin`, `agent-with-shell-and-no-internet` | egress denial typed |
| `missing-secret-refusal-names-the-secret-to-set` | new | process | `org-member`, `agent-with-shell-and-secret-skill` | 412 body with `SET_SECRET` |
| `member-asks-an-org-admin-to-enable-a-connector` | new | process | `org-member`, `org-admin`, a disabled connector | dead end becomes a request |
| `cli-prints-the-fix-from-the-error-body` | new | process | `org-member` | CLI reads `denial` |
| `fix-line-ignores-text-the-tool-wrote` | new | process | `agent-with-guarded-tool` | a tool output that says "Suggested fix" adds nothing |

### S2 — Space upgrade

**Depends on:** agent access in `main` (S2 touches `run_start.rs` and
`catalog_resolve.rs`); the owner's answer on Q10 and on the decision change
above. Holds the hot-file lock for those two files while it builds (Q16).
**Exit:** a Space shows its pinned and latest version; its owner moves it to
the latest release or back with one call; the next run uses the new pin; a
busy Space refuses; affected threads continue in a new thread; a yanked version
refuses new runs and prompts the upgrade.

| Scenario | Change | Layer | Given | Why |
|---|---|---|---|---|
| `space-shows-a-newer-released-version` | new | process | `org-member`, `org-publisher`, an agent with two releases | read side |
| `owner-moves-a-space-to-the-latest-version-and-the-next-run-uses-it` | new | process | as above | the exit |
| `space-upgrade-waits-for-the-live-task` | new | process | as above, a running task | busy refusal |
| `space-upgrade-can-be-rolled-back` | new | process | as above | rollback |
| `member-cannot-move-another-members-space` | new | process | two members | owner check |
| `thread-that-used-a-removed-tool-continues-in-a-new-thread` | new | process | an agent whose second release drops a tool | thread check |
| `yanked-version-refuses-new-runs-and-asks-to-upgrade` | new | process | as above, one yanked | Q11 |

### S3 — Agent-proposed change card

**Depends on:** agent access in `main` (its admin queue); S1 (the change kinds
and fix routes); the owner's answer on Q15.
**Exit:** an agent can propose each change on the closed list; a person sees a
fixed-text card and approves or refuses; the host applies it with the
approver's rights or sends it to the admin queue; invalid, over-limit and
notification-surface decisions are refused.

| Scenario | Change | Layer | Given | Why |
|---|---|---|---|---|
| `agent-proposes-connecting-and-the-member-approves` | new | process | `org-member`, connector skill | happy path |
| `proposal-for-an-undeclared-host-is-refused` | new | process | `agent-with-egress-probe` | injection defence |
| `proposal-the-member-cannot-apply-goes-to-the-admin` | new | process | `org-member`, `org-admin` | routing |
| `proposals-stop-at-the-run-limit` | new | process | `org-member` | rate limit |
| `proposal-cannot-be-approved-from-a-notification` | new | process | `notify-broker` | surface rule |

### S4 — App version follow and bulk upgrade

**Depends on:** Apps P2 (App pin; S4 is built inside or after it); S2. The
App-pin routing through the seam lands with Apps P4 (AS-2.6).
**Exit:** a shared App in `follow` mode moves on release; one write upgrades
every Space; live runs finish on the old version; canary Spaces move first;
rollback restores the previous pin.

| Scenario | Change | Layer | Given | Why |
|---|---|---|---|---|
| `app-following-latest-moves-on-release` | new | process | a shared App | follow |
| `app-upgrade-leaves-live-runs-on-the-old-version` | new | process | a shared App, a running task | Q10 |
| `canary-spaces-get-the-new-version-first` | new | process | a shared App with three Spaces | canary |
| `app-rollback-restores-the-previous-pin` | new | process | a shared App | rollback |

## What we are not building

- No guest-side settings writes. The guest never changes a policy, secret,
  connection or pin; it can only propose (S3), and the host applies.
- No auto-apply without a human, at any phase, for any change kind. An App
  in `follow` mode (S4) is not an exception: the owner's opt-in to `follow`
  is that approval, revocable, with a notice on each move.
- No fix text from the model or from tools. Fix text comes from host templates
  and host identifiers only.
- No new `VamErrorCode`s.
- No hot reload of policy into a running session.
- No per-message version switch (isolation-verify-closeout, explicitly later).
- No automatic upgrade of personal Spaces (Q10); no "update available"
  notification on every release (the banner is read from GET Space).
- No carried conversation into a `fresh_start` thread in S2.
- No general change-request queue in S1 (S3 extends the agent access queue).

## Open questions (recommended answer in bold)

1. **Q10.** Personal Spaces stay pinned and show "Agent updated, use vX?" with
   one click; shared App Spaces follow the App's pin. **Yes, as proposed.** S2
   builds the personal half; S4 the App half.
2. **Q11.** Minimal yank: a yanked version refuses new runs; its Spaces get a
   forced upgrade prompt; running sessions finish. **Yes; it settles DX-94; build
   it as the last S2 items.**
3. **Q15.** Agent-proposed settings changes. **Yes, after agent access is in
   `main`; human approves; host applies with the approver's rights; closed list
   as in S3.**
4. Where the version pin lives. **The Space row, with lazy backfill from
   `workspace.json`** (rejected option above).
5. Should `latestVersion` follow agent access (hide releases the caller cannot
   run)? **Yes; until agent access is in `main` it is the latest release.**
6. Carry context into a `fresh_start` thread? **Not in S2. If users ask, a later
   item adds a text-only digest of the old thread as a host context block.**
7. Should S2 notify on every new release (`agent.update_available`)? **No; the
   banner is enough. Revisit with S4 for App owners.**
8. Should `POST /v1/fix-requests` be rate-limited per org as well as per user?
   **Per user and target only in S1; add an org cap if a pilot sees noise.**
9. Should a denial expose the deciding rule text to members? **Only what the
   explain route already shows the caller; the model gets `who` and the change,
   not the rule.**
10. **Owner question: may S2 change isolation-verify-closeout locked decision 5**
    ("Workspace pins agent URN + version at create time")? **Yes: the pin is set at
    the first run and only the Space owner moves it, through
    `set_space_agent_version`; answer before S2 is broken down.**

## Implementer constraints

- Fix text: host templates keyed by `(reason, change, who)`; `subject` and
  `target` are host identifiers. An egress host comes from the guest's request:
  normalize it (`broker.rs` `normalize_host`), cap it at 253 bytes, render it as
  data, never as a link.
- Hidden agents stay 404 with no denial (agent access §2.3).
- `fix.requested` and `POST /v1/fix-requests` resolve the denial's
  `decision_ref`, check the requester's entitlement, and re-derive the fix
  and the recipients on the host from the stored row; a client-sent `fix`
  is ignored.
- The guest's template table and the host's change kinds are one proto enum;
  regenerate the descriptor baseline (`cargo run -p vam-spec --example
  generate_baseline`).
- Hot files (Q16): S1 touches `security.rs`, `agent.rs`, `api/error.rs`,
  `connector_park.rs`, `broker.rs`, `http_error.rs`, `common.proto`; none of the
  four hot files. S2 touches `run_start.rs` and `catalog_resolve.rs` and takes
  the lock for them; S4 runs inside the Apps lane's lock.
- Docs: `orchestrator/api.md` (error body), a new `orchestrator/denials.md`, and
  the Spaces section for upgrade, each with `status` and `proves` frontmatter.
