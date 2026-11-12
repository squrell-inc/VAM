# The primary agent: one agent to ask, who gets the work done by the others

Status: **approved by the owner 2026-11-01** (proposals P-A, P-B, P-C, P-D and every default in §13 and §14 accepted; not in 0.1.0; placed after E10 Phase 4 and notifications slice 1). Written 2026-11-01. Written by a design session against `main`
at (E5 Phase 2, the schedule broker). Where `main` disagrees
with an earlier design, `main` is the fact. Locked rows this touches are in
§9 as proposals for the owner; nothing in a tracker is edited by this doc.
Tracker slug proposed: `primary-agent`, item prefix `PA`.

The owner's vision, in their words: "one primary agent for every user by
default, or they can switch that primary agent to some other agent they have
drafted; its role is to help with other things; the next agenda is how that
primary agent can interact with these agents that the user has access to and
can fulfil the jobs."

## Decision record

| Topic | Decision (proposed) |
| --- | --- |
| Shape | **Router.** The primary agent asks another agent by starting a **separate run of that agent, as the user**, through the grant machine E5 built. A first-party broker `vam.delegate` (in `brokers/`, own process, HSP only) exposes one tool, `delegate_run`; `vamd` mints a single-use grant on the approved call; the broker fires it at once (`RUN_NOW`). |
| Rejected for now | Composition through `extends` (runtime merge not wired; one run with everyone's permissions). In-loop sub-agents (E10 Phases 4 and 5; the parent's permissions, not the target's). A `vamd` built-in that starts runs from inside a run (a new run-start path and provenance kind in the kernel; no operator switch). |
| Return path | **Report back.** When the asked run ends, `vamd` queues a host-written **report** in the asking thread: run id, outcome, output (capped, fenced), approvals and denials, receipt digest. It starts as the primary's next task (never a steer). The primary does not wait; `wait_for` stays deferred. This is not `DeliverEvent`. |
| Place | Never the asking Space. Default: the target agent's **default Space** for this user (new `PLACE_MODE_DEFAULT`), a **new thread**. `SAME` and self are refused. |
| Depth and fan-out | Depth one: only a run a **person** started may delegate (the loop guard, read as "any run a person did not start"). At most **4** delegations per task. |
| Permissions | New targets `agent:<urn>` and `agents:*`; decisions allow, ask, deny; default **ask** (mutable external). The asked run uses the **target agent's own** spec, permissions, model and budget, as the user. |
| Receipts | A chain of three: the asking run lists its delegations; the asked run says "requested by agent … run … via grant …"; the report run names the asked run's receipt digest. |
| User model | `users.primary_agent_urn` (NULL = the node's default primary). No Apps. The default is a Squrell-shipped **released** assistant the operator installs; a user with `catalog:write` may copy it to a personal draft and switch to it, or switch to any agent they may run. |
| Scope | Two phases. Not in 0.1.0 by the rubric. Product lane, after E12 Phase 2 and E10 Phase 4 (§11). |

## What we are building

A person opens VAM and talks to one agent, their **primary agent**. It
helps directly (answers, web, files in its own workspace, schedules) and,
when a job belongs to another agent the person may run, it asks that agent
to do it. The person approves the ask once (or has a rule that allows it);
the other agent works in its own workspace under its own rules, as the
person; when it ends, the result comes back into the conversation where the
person asked, and the primary agent tells them what happened. Every step is
on a receipt.

Phase 1 builds the machine (any agent can delegate; CLI and receipts).
Phase 2 makes it the product (every user has a primary; the app opens on it
and shows asks and reports).

## Why

- **P4 encode and share expertise** (`design/agent-experience.md:110-121`):
  an org packages a procedure as an agent with a playbook. Today a person
  must know that agent exists and open it. With a primary agent, the
  expertise is reachable from the one place people already talk.
- **P13 can't do real work** (`design/benchmark.md` §1): real jobs span
  several agents (a researcher, a writer, a close-prep agent). Today the
  person is the router.
- **P7 work outside chat**: the asked run happens outside the person's chat,
  parks at no cost while it waits for them, and comes back with a receipt.

The machine is already most of the way there. `RunGrantRequest.agent_urn`
may name another agent the owner may run (`grants.rs:233-245`; default 8 at
`design/hsp-triggers.md:510`), and the schedule broker's `schedule_create`
with `agentUrn` and `once in PT10S` can start another agent's run from a
chat today (`brokers/vam-schedule/src/tools.rs:48`). What is missing is the
return path, a safe default place, a per-agent permission, a tool shaped for
asking, and the guard rails this doc adds.

## 1. What `main` has today

| Piece | On `main` | Where |
| --- | --- | --- |
| Grant issuance on an approved broker tool call | Yes: loop guard, decode `run`, resolve agent, Space, thread, playbook, expiry, per-user cap; mint after preflight; a guard undoes on failure | `crates/vam-orchestrator/src/grants.rs:186-438` (check), `:503-600` (mint), `:462-500` (guard); called from `host_tools.rs:2863-2878`, `:2940-2952` |
| Another agent as the target | Yes, if the owner may run it (`get_agent_visible`) and the Space is `NEW` or bound to it; `SAME` with another agent is refused (`space_agent_mismatch`) | `grants.rs:233-245`, `:276-286`; `catalog.rs:789` |
| Gateway fire, typed outcomes, dedupe per grant and key, start as the owner | Yes | `gateway.rs:496-737`; spec key `hsp:<broker>:<grant>:<key>` at `:583` |
| Fire inside an Invoke (the broker calls the gateway while `vamd` waits on the Invoke) | Yes: `schedule_run_now` | `brokers/vam-schedule/src/broker.rs:93`, `manage.rs:55-120` |
| Busy Space → queued task, never a steer; one queued fire per grant; `not_after` | Yes | `api/inbox.rs:395`, `:463-475`; re-check at start `gateway.rs:883-918` |
| Shared start path, run source | `start_run`, `RunSource::{Person, Trigger}` | `api/run_start.rs:18-40`, `:216` |
| Loop guard | Refuses `loop_guarded_tools` when `runs.source` starts with `broker:` | `grants.rs:192-217` |
| A queued task built by `vamd` from a run's own spec with new text | Yes: steers left at a task's end become a queued task | `inbox.rs:351-420` (`merged_task`) |
| Default Space per (owner, agent, draft or released) | Yes, created on first use | `spaces.rs:243-275`; `resolve_place` `:535-560` |
| Receipt with trigger facts, issued when a run ends, swept if missed | Yes: `requestTrigger {brokerId, grantId, triggerRef, fireAtMs, fireKind}` | `receipt.rs:806-818`; `issue_receipt` `:923-961`; sweeper `:975-986` |
| Thread log `task_start` body | `{prompt, model, agent_digest, resumed, workspace}` on the main lane | `thread_recorder.rs:663-678` |
| Permission targets | `skill:`, `mcp:`, `builtin:`, `tools:*`, hosts, `internet:*`; broker tools map to `builtin:<tool>`; one row per tool, frozen at session start | `vam-spec/src/permissions.rs:179-219`; `session_targets.rs:165-167`; `security.rs:723-743` |
| "Always" offered only on the owner's draft run | Yes | `session_targets.rs:24-29` |
| Who may write agents | `catalog:write` is a publisher scope; members have none | `keys.rs:59-76` |
| A per-user default agent | **No.** `/v1/me` returns identity, scopes, domains and `defaultNamespace` only | `api/me.rs` |
| Model when the agent names none | Resolved from the node allowlist and default, so an assistant with no preference runs | `api/mod.rs:1345-1360`; `allowed_models.rs:371` |
| `extends` at run time | **No**: "stay out until the runtime merge is wired" | `catalog_resolve.rs:123` |
| Effect journal | **No** (E10 Phase 4) | `.cursor/workstreams/agent-runtime.md:3296-3307` |

## 2. Shapes compared

| | A. Router through grants (adopted) | B. Composition (`extends`) | C. In-loop sub-agents | D. `vamd` built-in `delegate_run` |
| --- | --- | --- | --- | --- |
| What runs | A separate run of the target agent, as the user | One run of a merged agent | A second loop in the parent's VM | A separate run, started by `vamd` directly |
| Whose rules | The target's own | The union of all parents' | The parent's (E10 decision 4) | The target's own |
| Exists on `main` | Issuance, gateway, queue, receipt facts, loop guard, start path | Proto, validation, freeze; **no runtime merge** (`catalog_resolve.rs:123`); one parent per chain (`agentic_spec.proto:52-57`) | Designed only (`design/agent-runtime.md:325`; Phase 5) | `start_run` |
| Missing | Report back, `agent:` target, place default, single use, discovery | Runtime merge, many parents, draft parents | Effect journal (Phase 4), `spawn_agent` (Phase 5), loading another agent's spec, child VMs (later) | A new run source and provenance, its own loop guard and dedupe, an operator switch |
| Locked rows | None changed; agrees with `hsp-triggers.md:510` (default 8) | "Inheritance, not a call" holds, but this is becoming an agent, not asking it; E1 21 (approvals once only on released) | Decision 4 gives the parent's permissions, so the target's deny rules and asks would not apply | Decision 15 fits (who and whether is `vamd`'s), but it re-proves in the kernel what the gateway already proves |

**Why A.** It reuses only approved, tested machinery (grant issuance,
socket identity, dedupe, queue, start path, receipt facts, `vam grants`),
keeps every locked row, and gives the operator one switch: disabling the
`vam.delegate` approval closes its gateway socket and no agent can ask
another (`design/hsp-triggers.md` amendment §5). The broker holds no vendor
and no clock; it is the degenerate "when = now" broker, exactly as
`schedule_run_now` is, and the rule "the broker decides when; the grant
decides who, what and where" (`.cursor/workstreams/work-outside-chat.md:75-80`,
decision 9) holds.

**Why B waits.** Its runtime merge is not wired, it allows one parent per
chain, and it would collapse every agent's tools and permissions into one
run: the largest blast radius, and not "asking" an agent but becoming it.

**Why C waits.** It needs E10 Phase 4's journal and Phase 5's `spawn_agent`,
and decision 4 gives sub-agents the parent's permissions and workspace, so
an asked agent's own deny rules, asks and files would not apply.
`spawn_agent` stays as designed, for splitting one agent's work, not for
asking another agent.

**Why not D now.** Considered seriously: one process fewer. But it adds a
second way to start a run from inside a run, with its own provenance kind,
loop guard and dedupe, in `vamd`, and no off switch short of a release. If A
proves the shape, D is a later optimisation behind the same tool name and
the same receipts.

## 3. The user model

**One primary per user.** `users.primary_agent_urn TEXT NULL`. NULL means
the node's default primary, an operator setting
`node_operator_settings.default_primary_agent_urn` beside the allowed-models
row (`allowed_models.rs:41-57`), written with the same operator scope as
`PUT /v1/node/allowed-models` (`api/allowed_models.rs:18-44`). If neither is
set, there is no primary and the app keeps today's agent list. No Apps, no
org accounts.

**The default primary is a released agent, not a per-user draft.** Evidence:
members hold no `catalog:write` (`keys.rs:70-76`), so a per-user
auto-created draft would either fail for members or give every member the
power to write their own agent's permissions, bypassing the org's review of
released agents (P2). So:

- Squrell ships an **assistant** agent package with VAM. The operator
  releases it into the node's catalog and sets it as the node default (a
  script in Phase 2; `vam node init` later, with the install slice,
  the 0.1.0 launch plan). Its permissions are the org's to set
  on release: `agents:*: ask` by default; an org may release it with
  `agent:<urn>: allow` for agents it trusts.
- A user with `catalog:write` may **copy** it into a personal draft in their
  `defaultNamespace` and switch to it. A copy, not `extends`, because the
  runtime merge is not wired; the draft records the template version it
  came from. "A personal agent is a draft that is never released"
  (`design/agent-experience.md:762-763`) then holds, and "Always allow" on a
  delegation builds up in it.
- A member runs the released assistant. Each delegation asks "Allow once"
  unless the released version's rules allow it (`design/agent-experience.md`
  §IV.7: on a released version nothing is saved).

**Switching.** `PUT /v1/me/primary-agent {agentUrn}` (empty clears to the
node default), validated with `get_agent_visible(urn, owner)`
(`catalog.rs:789`): any released agent the user may run, or any draft they
own. `GET /v1/me` gains `primaryAgent {urn, label, source: user | node,
stale}`; `stale` means the stored agent is no longer runnable and the node
default is used. Old threads stay with the agent they belong to.

**No server-side "no agent means primary".** `POST /v1/runs` keeps naming
its agent. The app and the CLI read the primary from `/v1/me` and name it
(`agent-experience` E1 decision 8: app and CLI call the same API). No run
API changes meaning.

**What the app opens.** The primary's default Space (`spaces.rs:243-275`)
and its most recent thread. The other agents stay one tap away.

**What the primary sees of the others.** The agents the user may run appear
**inside the delegate tool**, not as one tool each: at session start, for a
tool listed in `delegate_tools` (§8), `vamd` fills the `run.agentUrn`
property's `oneOf` with up to 20 agents the owner may run, the primary
itself excluded, ranked by the owner's most recent runs. Each entry carries
`display_name`, `description` (`agentic_spec.proto:16-17`, cut to 280
characters) and up to three playbook `when` lines (`workflow.proto:205`),
fenced as untrusted text, since other publishers wrote them. The model can
only name an agent it may run; an off-list URN is a schema error it
repairs. More than 20 agents waits for harness L2's ranking and deferred
tools (`design/harness.md:120-128`, `tools.rank`, `tools.deferred`; lane 13).

## 4. Delegation: the call

**The tool** (`vam.delegate`, side effect `MUTABLE_EXTERNAL`):

```jsonc
delegate_run {
  "title":  "Market scan for Q4",          // ≤ 80 chars; labels the asked thread
  "run": {
    "agentUrn":    "urn:vam:agent:…/researcher",   // required; oneOf filled by vamd
    "message":     "…",                    // ≤ 32 KiB; stands alone: the asked run has none of this chat
    "playbookRef": { "urn": "…" },         // optional; one of the target's playbooks
    "inputs":      { },                    // with playbookRef only
    "workspace":   { "mode": "DEFAULT" },  // DEFAULT (default) | NEW {name} | ID {id}; SAME refused
    "thread":      { "mode": "NEW" }       // NEW (default, labelled from title) | ID {id} of that Space
  }
}
```

`onBusy` and `expiresAt` are not in the schema: `vamd` fixes them for a
delegation (`QUEUE`; 24 hours). The result to the model: `{childRunId,
agent {urn, label}, space {id, label}, thread {id, label}, state: started |
queued, note}`, where `note` says "Tell the user you asked <label>. End your
turn; the result arrives in this thread as a report."

**Issuance rules** (`grants.rs`, for a tool in the approved
`delegate_tools`), on top of today's checks:

1. **Only from a person's task.** The loop guard refuses the call in any
   run whose `runs.source` is not NULL: a triggered run, an asked run, a
   report run. So depth is one, a report turn cannot ask onward, and no
   delegation happens without a person's message (proposal P-A, §9).
2. **Never the asking Space, never itself.** `workspace SAME`, an empty
   `agentUrn` and the calling agent's own URN are refused with a typed
   error (`delegate_same_space`, `delegate_self`). Why: one live task per
   workspace (`design/agent-runtime.md:955-962`) means a child queued behind
   its parent in the parent's Space would serialize behind it, write into
   its files, and deadlock outright the day the parent can wait
   (`wait_for`). `SAME` with another agent is already refused
   (`grants.rs:276-286`); this closes the self case and makes the rule
   explicit.
3. **Place.** `DEFAULT` resolves at issuance, as `resolve_place` does for a
   run with no Space id (`spaces.rs:535-548`): the owner's default Space of
   the target agent (draft or released side), created on first use. It is
   bound to that agent, so `hsp-triggers.md:510` (default 8) holds. Why not
   "the most recent Space": the default Space is what the user sees when
   they open that agent directly (HF-3), and it is already one row per
   (owner, agent, side). `NEW` makes a Space once (as for schedules); `ID`
   names an owned Space bound to the target. Thread `ID` continues an
   earlier asked thread ("ask the researcher to go deeper"), so the asked
   agent keeps its context (E10 decision 2).
4. **Fan-out.** At most **4** delegations per task, counted from grants
   whose `source_run_id` is the calling run (`grants.rs:564` writes it).
   The number mirrors E10 decision 5's four live sub-agents. Several calls
   in one turn are allowed; two to the same agent's default Space run one
   after the other (the second queues).
5. **Single use, bounded life.** The grant expires after 24 hours (it must
   outlive a queued fire, which the queue starter re-checks,
   `gateway.rs:883-918`). The gateway answers `DUPLICATE` with the first run
   for any later fire of a delegate grant, whatever its key. `vamd` revokes
   it when the asked run ends. A user who revokes it before the asked run
   starts cancels the delegation (`vam grants revoke`).
6. **The guard never undoes a used grant.** If the broker fails after its
   fire was accepted, the issuance guard must not delete the new thread a
   run already uses (`grants.rs:477-490` deletes created rows today): undo
   only when the grant has no recorded fire (`last_used_at`, written at
   gateway step 13).
7. **Explicit playbook start is allowed.** A delegation is a trigger grant,
   where E10 decision 18 keeps explicit starts; the approval card shows the
   playbook and inputs, so the person approves exactly that. With no
   playbook, the asked agent matches one as in chat (its own ask card).

**The broker** (`brokers/vam-delegate`, lib and bin `vam-delegate-broker`,
the `vam-schedule` serve pattern, no store). On `delegate_run` it requires
the `RunGrant` on the Invoke and fires `RequestRunTrigger` with `kind
RUN_NOW`, `idempotency_key = hex(sha256("vam.delegate\n" + grant_id))`,
`trigger_ref = grant_id`, `not_after = now + 24 h`. It retries
`RATE_LIMITED`, `UNAVAILABLE` and `BUSY ACTIVE_RUN` with the same key for at
most 30 seconds, maps `ACCEPTED`, `QUEUED` and `DUPLICATE` to the result
above, and every `DENIED` reason to a typed tool error the model can
explain ("that agent cannot run now"). It keeps nothing after the Invoke.

**Crash and retry** (E10 decision 11). Today a crashed VM ends its task; no
host call is re-sent, so no duplicate child. When E10 Phase 4's journal
lands, `delegate_run` is a mutable-external host call like any other; its
verify read is "the delegate grants of this run" (`GET
/v1/keys?holder=broker`, filtered by source run), which the Phase 4
breakdown can use.

## 5. The return path: report back

**Name.** This slice is **report back**, not `DeliverEvent`. `DeliverEvent`
(`design/hsp-triggers.md` 2026-10-15 amendment item 2) is a broker
delivering a vendor event to a task that **waits** (`wait_for`) under a
**resume grant**. Report back has no broker, no grant and no waiting task:
`vamd` itself turns the end of a run it started into a message for the
thread that asked. What the two share is the inbox entry: a **host-written
queued message with a source**. Report back builds that entry;
`DeliverEvent` reuses it later. This is the narrowest result-delivery slice
the primary needs, and the only one pulled forward (Appendix B).

**When.** After the asked run is terminal and its receipt is issued
(`issue_receipt`, `receipt.rs:923-961`, which the finalizer calls last), so
the report can name the receipt digest. The receipt sweeper
(`receipt.rs:975-986`) also reports any ended delegation not yet reported,
so a `vamd` restart loses none. Idempotent by the asked run id.

**The report** (host facts; the output is model-written and labelled):

```jsonc
{
  "childRunId": "urn:vam:run:…", "grantId": "rg_…", "requestDigest": "sha256:…",
  "agent": { "urn": "…", "version": "1.2.0", "label": "Researcher" },
  "place": { "spaceId": "sp_…", "threadId": "th_…" },
  "outcome": "SUCCEEDED",                 // the run's terminal state
  "reason": "",                           // e.g. "stopped by you", "its workspace stayed busy for 24 h"
  "approvals": { "asked": 1, "denied": 0,
                 "items": [ { "what": "skill:jira/create_issue", "decision": "approved-once",
                              "by": "alice", "surface": "app" } ] },   // at most 10 items
  "output": "…", "outputTruncated": false, // the run's final text, at most 8 KiB
  "receiptDigest": "sha256:…",
  "endedAt": 1730000000
}
```

The model sees it as one fenced user message, the same fence as web and MCP
text (E10 Phase 0):

```
<vam:report run="run_…" agent="Researcher" outcome="succeeded" receipt="sha256:…">
Asked: "Market scan for Q4". Approvals: 1 asked (approved by you in the app), 0 denied.
Output (written by Researcher, not verified by the host):
…
</vam:report>
```

**Where it lands.** In the asking Space and thread (ids from the asking
run's resolved spec), as a **queued task on that thread**, built the way
`merged_task` builds one (`inbox.rs:351-420`): the asking run's agent ref
and place, the report text as the prompt, the owner as caller, and a new
source `report:<childRunId>`. On the thread log it is on the **main lane**:
the task's `task_start` record gains `source: {kind: "report", childRunId,
agentUrn, outcome, receiptDigest}` (`thread_recorder.rs:663-678`), and the
fenced text is that task's user message. No new lane: the model must see it
in context, compaction and replay treat it as any turn, and the app tells a
report from a person's words by `task_start.source`.

| The asking thread is | What happens |
| --- | --- |
| Idle | The report task starts now: the primary wakes, reads it, answers the person. |
| Busy (the person is talking to the primary, or another task holds the Space) | Queued behind the live task, as a queued message is today (`api/inbox.rs:463`). **Never a steer**: a report is not the person's words (the same rule as a trigger, `work-outside-chat.md:67-74`, decision 7). |
| Parked (waiting for an approval or an answer) | Queued behind it like any task; the app shows the report card at once (Phase 2). |
| Already holding other reports that have not started | They become **one** task, texts joined in arrival order (≤ 32 KiB), so a fan-out of three wakes the primary once. |
| Stopped by the person | A Stop discards the person's pending messages, never a report: the report still runs after. |
| Gone: owner deactivated, or the asking agent no longer runnable for them | Not delivered. `delegation.report_undelivered {reason}` on the asked run's outbox; the result stays in the asked agent's thread, where the person can read it. |

**The person sees** (Phase 2, the VAM app): in the primary's thread, the ask as
a card ("Asked Researcher: Market scan for Q4 · working · open"), live from
the asked run's status; "waiting for your approval · open" while the asked
run is parked on an ask; then a **report card** (outcome, output preview,
approvals, receipt link) followed by the primary's reply. In the asked
agent's thread, a header "Started by Assistant from <thread label>" links
back. In Phase 1 (CLI): `vam run events` prints `delegation.started` and
`delegation.reported`; `vam receipt show` prints the chain (§7).

**Cost.** While the asked run works, the primary holds no VM (its task
ended after the call). Parked cost of waiting for a delegation: 0. The
report wakes the primary once per batch.

**Security.** The asked run's output may carry injected text (a web page it
read). It is fenced and labelled, and the report turn **cannot delegate**
(§4 rule 1): an injection in a report cannot make the primary ask a third
agent to act. The person's next message can.

**Synchronous waiting stays deferred.** A primary that calls
`wait_for(delegation)` and continues in the same task needs E10 Phase 5
(`wait_until`, parking) and the later E5 phase (resume grants). Report back
covers the product need without it.

## 6. Permissions

**New targets** (`design/agent-experience.md` Part IV;
`vam-spec/src/permissions.rs`):

| Target | Form | Specificity | Example |
| --- | --- | --- | --- |
| One agent | `agent:<urn>` | exact (like a tool) | `agent:urn:vam:agent:vam.acme.example/finance/close-prep` |
| Every agent | `agents:*` | source wildcard | |

Decisions `allow | ask | deny`. Default: `delegate_run` is mutable
external, so `tools:* bySideEffect` makes every delegation **ask**
(`vam-security/src/permissions.rs:496-516`). Within a scope the most
specific rule wins (`agent:` over `agents:*` over `tools:*`); across scopes
the strictest wins (§IV.2).

**Per call, frozen at session start.** Today a tool has one target row,
frozen when the session starts (`security.rs:723-727`). A delegation's
target depends on the call's `run.agentUrn`. So at session start `vamd`
stores, beside the delegate tool's row (target `agents:*`), the `agent:`
rules of every scope that has them; at the call, the PEP picks the most
specific row for the named agent and stamps `agent:<urn>` on the decision
and on the approval. The Cedar resource stays the guest tool name and the
target is an attribute (§IV.3), so the fixed policy set does not change.

**Saving.** On the owner's draft primary, the approval offers "Always allow"
and "Always deny", which write `agent:<urn>` into the draft
(`session_targets.rs:24-29`; §IV.7). On a released primary: "Allow once"
only.

**Org.** The org scope (E3, not built) may set `agent:<urn>: deny` or `ask`;
the target kind is valid there from the start. Until E3, the operator has
the broker switch (all delegation on or off), and the org's release of the
assistant carries its rules.

**The asked run's own power.** The gateway starts it as the owner, loading
the **target** agent for the owner (`gateway.rs` step 12): its spec,
permissions, model (`resolve_run_model`), budget and Space. The primary's
"allow" for a delegation never becomes an allow inside the asked run; the
asked agent's own asks park and go to the person. Its `runs.source` is
`broker:vam.delegate`, so the loop guard also stops it creating schedules
(`brokers/vam-schedule/src/manifest.rs:12`).

## 7. Receipts

Receipts are immutable and issued when a run ends (`design/run-receipt.md`
locked decision 8). The asking run ends right after it delegates, before the
asked run finishes, so its receipt cannot carry the asked run's digest. The
chain therefore has three links, all host tier:

```
ASKING RUN (the primary)
DELEGATIONS (2)
 Researcher   urn:…/researcher   run run_01…  started   grant rg_7f… (vam.delegate)  request sha256:0b91…
 Writer       urn:…/writer       run run_01…  queued    grant rg_2c… (vam.delegate)  request sha256:e4d7…

ASKED RUN
Requested by   alice (member), surface delegate
               by agent urn:…/assistant v1.0.0, run run_01… via grant rg_7f… (vam.delegate)

REPORT RUN (the primary's next task)
Requested by   alice (member), surface delegate — report of run run_01… (Researcher)
REPORTS
 run run_01…  Researcher  SUCCEEDED  receipt sha256:f1d0…88ab   verify: vam receipt verify run_01…
```

"By agent" and "run" come from the grant row (`source_run_id`), never from
the broker. The fields are requested from the E12 lane (Appendix E):
additive JSON like `requestTrigger` (`receipt.rs:806-818`).

## 8. Contract (additive)

```protobuf
// hosted_services.proto
message RunRequestPermission {            // :100
  // … fields 1–6 unchanged
  repeated string delegate_tools = 7;     // ⊆ grant_tools ∩ loop_guarded_tools. A grant from these is a
                                          // delegation: single use, never the calling Space or agent,
                                          // reported back. Core and first-party tiers only.
}
enum PlaceMode {                          // :347
  // … 0–3 unchanged
  PLACE_MODE_DEFAULT = 4;                 // the target agent's default Space for the owner
}

// workflow.proto
message RunSource {                       // kind gains "report"
  // … fields 1–6 unchanged
  string child_run_id = 7;                // kind "report": the run this reports
  string requested_by_run_id = 8;         // kind "trigger" from a delegate tool: the asking run
}
```

Manifest of `vam.delegate`:

```json
{
  "brokerId": "vam.delegate",
  "trustTier": "TRUST_TIER_FIRST_PARTY",
  "secretBindings": [], "egressDomains": [],
  "identityRequirement": { "userId": true, "agent": true },
  "runRequestPermission": {
    "grantTools": ["delegate_run"],
    "loopGuardedTools": ["delegate_run"],
    "delegateTools": ["delegate_run"],
    "surface": "delegate",
    "maxRequestsPerMinute": 60, "maxRequestsPerDay": 2000,
    "maxActiveGrantsPerUser": 20
  },
  "tools": [ { "toolName": "delegate_run", "sideEffect": "SIDE_EFFECT_CLASS_MUTABLE_EXTERNAL" } ]
}
```

Outbox events: `delegation.started {childRunId, agentUrn, grantId, state}`
on the asking run; `delegation.reported {childRunId, outcome,
receiptDigest, reportRunId}` and `delegation.report_undelivered {reason}` on
the asked run. API (Phase 2): `GET /v1/me` `primaryAgent`; `PUT
/v1/me/primary-agent`; `GET|PUT /v1/node/primary-agent` (operator).

## 9. Locked decisions this agrees with or touches

| Decision | Effect |
| --- | --- |
| E10 1 (security first) | Agrees: delegation goes through the host, Cedar and an approval; the report turn cannot delegate. |
| E10 2 (the thread holds context; a task is the unit) | Agrees: the asked run is its own task with its own budget; a report is a task on the asking thread; thread `ID` continues the asked agent's context. |
| E10 3 (a mid-run message steers) | Agrees: it governs a person's messages. A report is not one and queues, as a trigger does (WOC 7). |
| E10 4 (sub-agents in the same VM first, with the parent's permissions) | **Untouched.** Delegation is not a sub-agent: a separate task as the owner under the target's rules, as `hsp-triggers.md:510` and `vam-apps-spaces.md` amendment 2026-10-29 §6 already read it. |
| E10 5 (four live sub-agents, depth 1) | Agrees in spirit: four delegations per task, depth one. |
| E10 11 (decide, persist, act) | Agrees: no host call is re-sent today; Phase 4 journals `delegate_run` like any mutable-external call. |
| E10 15 (layer rule) | Agrees: the broker holds "when = now"; `vamd` holds who, whether and the report. |
| E10 18 (the model picks the playbook in chat) | Agrees: a delegation is a trigger grant, where explicit starts stay; the approval card shows the playbook. |
| E10 22 (brokers in `brokers/`) | Agrees: `brokers/vam-delegate`. |
| E10 23 (the thread log lives in the workspace) | Agrees: the report is a main-lane message of a task; `task_start` gains a `source` key. |
| One live task per workspace (`design/agent-runtime.md:955-962`) | Agrees, and is why `SAME` and self are refused. |
| WOC 3, 4, 5, 8, 9, 11, 12, 13, 14 | Agree unchanged. The 24-hour expiry is within decision 13. |
| WOC 6 (loop guard "inside a broker-started run") | **Proposal P-A (owner to confirm).** Read it as "inside any run a person did not start" (`runs.source` not NULL). Today the two are the same set; the report source makes them differ. Stricter, so P1-safe. |
| WOC 7 (busy → queue, never a steer) | Agrees, and extends to reports. |
| WOC 10 (ids; a request with no Space id uses the default Space) | Agrees: `DEFAULT` is that rule, resolved at issuance. |
| Apps-Spaces locked row "Personal Apps … Account default App = primary agent" | **Proposal P-B (owner to confirm).** Its second sentence is superseded: the primary is `users.primary_agent_urn`. When Apps ship, an Account's default App is the personal App of its primary (derived, not stored twice). Recorded as a marked amendment in `vam-apps-spaces.md`. |
| the 0.1.0 launch plan cut row "Child-VM sub-agents and multi-agent delegation" | **Proposal P-C (owner to confirm).** Split it: in-task delegation (sub-agents, child VMs) stays cut and locked to E10 Phase 5; asking another agent as a separate approved task is this design's own row, also after 0.1.0 (Appendix C). |
| Run receipt locked decisions 1, 4, 8 (proposed there) | Agree: host tier only, approval provenance kept, immutable receipts (hence the three-link chain). |

## 10. Locked decisions (proposed)

1. **One primary per user.** `users.primary_agent_urn`, NULL = the node
   default; no Apps; the run API keeps naming its agent.
2. **The default primary is released.** Squrell ships it, the operator
   releases it; a user with `catalog:write` may copy it to a draft. No
   per-user auto-created drafts.
3. **Asking is a separate task as the user**, under the target agent's own
   spec, permissions, model, budget and Space. Never the primary's power,
   never the asking Space, never itself.
4. **Through the grant machine.** `vam.delegate` in `brokers/`; single-use
   grants from an approved call; fired at once; declared by
   `delegate_tools`.
5. **Depth one, and only from a person's task.** No delegation in a run a
   person did not start (triggered, asked, report).
6. **Results come back as a report**: a host-written queued task on the
   asking thread, never a steer; the primary does not wait.
7. **`agent:<urn>` and `agents:*`** are permission targets, default ask,
   resolved per call from rules frozen at session start.
8. **The receipt chain has three links** (asking run, asked run, report
   run), host facts only.
9. **At most 4 delegations per task.**

## 11. Scope and placement

**Rubric** (`phase-playbook.md` §11.1;, the five questions:

1. Serves P13 and P4. No benchmark line fits, so its line comes first
   (Appendix D): "delegated jobs that complete and report back".
2. The smallest slice that moves it: Phase 1 (machine and CLI).
3. Phase 1: one phase, Sonnet low to medium waves; new surface: one broker
   crate, two proto fields and an enum value, one permission target kind,
   one `vamd` delivery path. Phase 2: one phase, vam and the VAM app.
4. Touches WOC 6 (P-A), an Apps-Spaces row (P-B) and the 0.1.0 cut row
   (P-C): proposals for the owner, not scored.
5. Reversible: the operator disables the broker approval and nothing
   delegates; no existing API changes meaning.

**Decide:** `SLICE — proof 2 (P13 gets its delegation line first, PA-1.8; no
0.1.0 launch proof needs it, the 0.1.0 launch plan "Not required: multi-agent"),
cost 3 (two phases, a new broker, a new permission target kind, a new vamd
delivery path, the VAM app) → slice: Phase 1 (cost 2). Phase 2 after Phase 1
shows the line. Not in 0.1.0 unless the owner names "ask the assistant and
it gets another agent to do it" a launch proof.`

**Where it sits.** Product lane. E5 Phases 1 and 2 are on `main`. Phase 1
shares files with **E12 Phase 2** (row 7: `gateway.rs`, the receipt builder,
the requester block) and with **E10 Phase 4** (row 9: `host_tools.rs` and
the inbox, which the journal keys ride). The conflict rule says two lanes
never build in one file at once (0.1.0 launch scope). Default: the
breakdown and wave 0 (scenarios, red) may run while row 7 builds; build
waves start after rows 7 and 9 merge (0.1.0 first). If the owner wants it
sooner: after row 7 and before row 9, which delays E10 Phase 4 by one phase.
Phase 2 follows Phase 1; its the VAM app waves do not conflict with the vam
lanes.

## 12. Phases

| Phase | Outcome | Depends on |
| --- | --- | --- |
| **Phase 1: ask and report back** | Any agent with the delegate tool can ask another agent the user may run; the user approves (or a rule decides); the asked run works in its own Space under its own rules; the result comes back as a report task in the asking thread; receipts chain; the CLI shows it | E5 Phases 1 and 2 (on `main`); E12 Phase 2 and E10 Phase 4 for shared files |
| **Phase 2: the primary agent** | Every user has a primary (the node default or their choice); the app opens on it and shows asks, waits and reports; the shipped assistant | Phase 1 |
| Later (not planned) | `wait_for` a delegation in the same task; in-loop delegation behind the journal; child VMs; delegation from scheduled runs; file hand-off; progress relay | E10 Phases 4 and 5; the later E5 phase |

### Phase 1: ask and report back

**Outcome.** A person asks their agent for a job that belongs to another
agent; after one approval the other agent does it in its own workspace, as
the person, under its own rules; when it ends, the asking conversation gets
a report and the agent answers from it, with no VM held while waiting.
Neither the asked agent nor the report turn can ask anyone. Receipts show
who asked whom.

**Phase Decide:** `SLICE — proof 2 (the delegation line, added first by
PA-1.8), cost 2 (proto fields, a broker on the vam-schedule pattern, a vamd
delivery path) → slice: terminal reports only, CLI only, ≤ 20 agents in the
tool. Rest: Phase 2 (app, primary), later (wait_for, progress relay).`

**Benchmark delta.** New line (Appendix D), baseline 0 by construction (no
delegation exists). After: the `route-the-ask` task passes on the mock; paid
runs on two models on the owner's word (`phase-playbook.md` §10). Parked
cost while a delegation runs: 0, checked by
`asked-agent-runs-in-its-own-space-and-reports-back`.

**Contents**

- **PA-1.1 Contract.** `RunRequestPermission.delegate_tools = 7`,
  `PLACE_MODE_DEFAULT = 4`, `RunSource.child_run_id = 7`,
  `requested_by_run_id = 8`, kind `report`; validation in `vam-spec`
  (⊆ grant tools ∩ loop-guarded tools; core and first-party only; the
  subset gate in `hsp_registry.rs`); the receipt request to the E12 lane;
  Starlight `hosted/delegation.md` (new, sidebar in `docs/astro.config.mjs`)
  and a section in `hosted/triggered-runs.md`.
  `Decide: SLICE — proof 2 (no item runs without it), cost 2 (proto) → slice:
  the fields this phase reads.`
- **PA-1.2 Permission targets.** `Target::Agent {urn}` and `AgentsAll` in
  `vam-spec/src/permissions.rs` (parse, display, specificity); resolution in
  `vam-security`; session start freezes `agent:` rules beside the delegate
  tool's row; the PEP resolves per call and stamps `agent:<urn>`; "Always
  allow/deny" writes it on the owner's draft; the permissions view lists it.
  Deny by rule is proven by unit tests.
  `Decide: NOW — proof 3 (P1: no delegation without a decision), cost 1.`
- **PA-1.3 Issuance rules for delegate tools.** §4 rules 1 to 6 in
  `grants.rs` and the gateway: the loop guard on any non-person source
  (P-A); `SAME` and self refused; `DEFAULT` resolved; fan-out cap 4;
  24-hour single use (`DUPLICATE` on a second fire); revoke at the asked
  run's end; the guard never undoes a used grant.
  `Decide: NOW — proof 3 (P1 and the deadlock trap), cost 1.`
- **PA-1.4 The `vam.delegate` broker.** `brokers/vam-delegate`, manifest,
  `delegate_run` schema and argument hygiene, the fire inside the Invoke
  with retries ≤ 30 s, outcome mapping, the operator script, e2e world
  piece `delegate-broker`.
  `Decide: NOW — proof 2 (the line), cost 1 (new crate on an existing
  pattern, no store).`
- **PA-1.5 Report back.** On terminal plus receipt (finalizer and sweeper):
  build the report, queue a host-written task on the asking thread (the
  `merged_task` pattern), merge pending reports of one thread, keep reports
  through a Stop, source `report:<id>`, `task_start.source`, the fence;
  outbox events; the undelivered path; idempotent by asked run id.
  `Decide: SLICE — proof 2 (no report, no line), cost 2 (a new vamd path into
  the inbox) → slice: terminal reports only; no progress relay, no
  wait_for.`
- **PA-1.6 Agents in the tool.** The `run.agentUrn` `oneOf` at session
  start: ≤ 20 agents the owner may run, self excluded, recent first,
  description and `when` lines cut and fenced.
  `Decide: NOW — proof 2 (wrong-agent picks are half the line), cost 1.`
- **PA-1.7 Receipts and CLI.** Delegations on the asking receipt; "by agent
  … run … via grant" on the asked receipt; reports with digests on the
  report receipt; `vam receipt show` lines; `vam run events` lines; `vam
  grants list` shows delegate grants with their state.
  `Decide: SLICE — proof 2 (P2 receipt completeness), cost 2 (receipt fields
  through the E12 lane) → slice: host-tier facts only.`
- **PA-1.8 The line and its task.** Benchmark task `route-the-ask` (a
  primary and five agents; three asks, each matching one agent's
  description or playbook `when`; scored: the right agent ran, the report
  arrived, the answer uses it), mock first; the line request to the
  benchmark lane. Lands in wave 0.
  `Decide: NOW — proof 2 (playbook §11: a problem with no line gets its line
  first), cost 1.`

**Exit.** All Phase 1 scenarios green and `active` in process mode; every
existing scenario green unchanged (an agent without the delegate tool sees
no change; `triggered-run-cannot-arm-another-trigger` stays green after
P-A); `route-the-ask` passes on the mock; `scripts/ci/local.sh --full`; the
Firecracker lane once on a KVM host (the start path and the inbox
moved).

**Scenarios** (all `needs: process`)

| Scenario | Change | Given | Behaviour |
| --- | --- | --- | --- |
| `asked-agent-runs-in-its-own-space-and-reports-back` | new | org-admin, agent-that-delegates, agent-asked-for-help, delegate-broker | The user asks and approves; the asked run starts in the asked agent's default Space in a new thread, as the user; the asking task has ended (no session) while it works; when it ends, a report task runs in the asking thread with outcome, output and receipt digest; the three receipts chain |
| `delegation-asks-first-and-a-denial-starts-nothing` | new | same | The call asks; on deny no grant, no run, no Space or thread is listed |
| `runs-a-person-did-not-start-cannot-delegate` | new | same | The asked run and the report turn each call `delegate_run`; the host refuses both; no grant is listed |
| `delegation-into-the-asking-workspace-is-refused` | new | same | `workspace SAME` and the agent's own URN are refused with a typed error the model can read; no run |
| `report-waits-for-the-task-in-the-asking-thread` | new | same, agent-with-shell | The asking thread is busy with a slow task when the asked run ends; the report is queued (not a steer), starts after the task, and the person's task is unchanged |
| `delegated-run-follows-the-asked-agents-own-rules` | new | same, an asked agent with an ask-first tool | The asked run's own ask parks it although the delegation was allowed; after approval it finishes; the report lists the approval with who and where |
| `two-agents-asked-in-one-turn-report-in-one-task` | new | same, a second asked agent | Two delegations in one turn; both run; their reports start one task in the asking thread; a fifth delegation in one task is refused |
| `asked-agent-list-shows-only-agents-the-user-may-run` | new | org-admin, org-member, agent-that-delegates, another user's draft | The member's delegate tool lists released agents and none of the admin's drafts; naming a draft is a schema error |

Unchanged, re-run because their guards move: `triggered-run-cannot-arm-another-trigger`,
`trigger-on-a-busy-workspace-runs-after-the-task`,
`queued-message-runs-after-the-task` (`guards` on `grants.rs`, `gateway.rs`,
`api/inbox.rs`).

### Phase 2: the primary agent

**Outcome.** A new user signs in and lands in a conversation with their
primary agent, the node's assistant. A user who drafts their own agent can
make it their primary. In the app, an ask shows as a card that follows the
asked run, a wait for approval shows where the person asked, and the report
arrives as a card followed by the primary's answer.

**Phase Decide:** `SLICE — proof 2 (the line becomes visible to users), cost
2 (the VAM app views, one column, one setting) → slice: primary selection, the
shipped assistant, ask and report cards. Rest: backlog (per-agent toggles
beyond the permissions view; reports in Slack and email with E12 Phase 2's
vam.notify).`

**Benchmark delta.** No line moves (the delegation line moved in Phase 1);
recorded as such. `route-the-ask` starts from the user's primary.

**Contents**

- **PA-2.1 The primary on the user record.** `users.primary_agent_urn`,
  `node_operator_settings.default_primary_agent_urn`, `GET /v1/me`
  `primaryAgent`, `PUT /v1/me/primary-agent`, `GET|PUT
  /v1/node/primary-agent`, the stale fallback; CLI `vam primary show|set`,
  and a CLI run that names no agent uses the primary (client side).
  `Decide: BACKLOG — proof 1 (no line), cost 1 → carried by PA-2.3's first
  wave, which touches /v1/me and the app's home.`
- **PA-2.2 The shipped assistant.** The package (instructions for helping
  and asking, `agents:* ask`, web, schedule and outbox tools, no shell), a
  script that releases it and sets the node default, a copy-to-draft action
  for users with `catalog:write`.
  `Decide: BACKLOG — proof 1, cost 1 → carried with PA-2.1.`
- **PA-2.3 App surfaces** (repo the app repo). Home opens the
  primary's default Space and last thread; ask, wait and report cards (§5);
  the "Started by" header in the asked thread; the primary picker in
  settings; `agent:` rules in the permissions view.
  `Decide: SLICE — proof 2 (the line in the product), cost 2 (new views) →
  slice: cards, picker, header.`

**Exit.** Phase 2 scenarios green and `active`; Phase 1 scenarios still
green; the VAM app smoke with the new cards; one manual run on a KVM host.

**Scenarios** (vam, `needs: process`; the VAM app checks in its own smoke)

| Scenario | Change | Given | Behaviour |
| --- | --- | --- | --- |
| `new-user-starts-with-the-node-primary-agent` | new | org-admin, org-member, shipped-assistant | With the node default set, a member's `/v1/me` names it with source `node`; a CLI run that names no agent goes to it |
| `user-switches-primary-to-an-agent-they-may-run` | new | org-admin, agent-plain | The user sets their own draft as primary; `/v1/me` names it with source `user`; their earlier threads stay with the old agent |
| `primary-cannot-be-an-agent-the-user-may-not-run` | new | org-admin, org-member, another user's draft | Setting another user's draft is refused as not found; the primary is unchanged |
| `unavailable-primary-falls-back-to-the-node-default` | new | the first row's pieces, plus a draft that is then deleted | After the chosen draft is gone, `/v1/me` names the node default with `stale: true` |

## 13. Open questions, each with a default

| # | Question | Default |
| --- | --- | --- |
| 1 | Does a report wake the primary (a model turn), or wait for the person's next message? | Wakes it; pending reports of one thread start as one task. |
| 2 | May a report turn ask another agent (chains)? | No (loop guard). The person's next message can. |
| 3 | Fan-out cap | 4 per task. |
| 4 | Output cap in the report | 8 KiB; the rest through the link to the asked thread. |
| 5 | Place of the asked run | The target's default Space, a new thread; `NEW` or `ID` when the model names one. |
| 6 | How long a delegation waits for a busy target | 24 h, then `EXPIRED` and reported. |
| 7 | Default primary | The released Squrell assistant, set by the operator; members get no drafts; publishers may copy it. |
| 8 | Do the asked run's files reach the primary? | No. The report names the asked thread; files stay in the asked Space. File hand-off is later. |
| 9 | The person switches primary while reports are pending | Reports go to the thread that asked, run by the agent that asked. |
| 10 | More than 20 agents | Recent first; the rest wait for harness ranking and deferred tools (lane 13). |
| 11 | A manifest field, or a hard-coded broker id, marks delegation | The manifest field `delegate_tools`, core and first-party tiers only. |
| 12 | Reports by Slack or email too | After E12 Phase 2 (`vam.notify` delivers outbox messages); not in these phases. |

## 14. Owner review, 2026-11-01: steer, stop, status, notifications, limits

Added after the owner read the design. These amend §4, §5, §12 and §13;
the phase breakdown absorbs them as items. Proposals are marked.

### 14.1 Steer, stop and status of an asked run

Today the user can cancel before the run starts (`vam grants revoke`) and,
while it runs, open the asked thread and steer or stop it directly, since
it is their run and the E10 Phase 3 inbox works on any run they own. What
was missing is doing it through the primary. The delegate broker gains
three tools, all reusing surfaces that exist on `main` (run status, the
inbox, stop). Authorisation for all three: the run's grant was issued from
the calling thread (`grants.source_run_id` resolves to a task of this
thread) and the owner is this user; anything else is `NOT_FOUND`.

| Tool | Side effect | Does | Phase |
|---|---|---|---|
| `delegation_status` (`run_id`) | `READ_ONLY` | one object: state, current step and playbook step, what it waits on (`approval`, `gate`, `budget`, `queue`, none) with the approval or gate id, spend so far, the last event in one line, the asked thread id | 1 |
| `delegation_steer` (`run_id`, `text`) | `MUTABLE_EXTERNAL`, ask by default | posts `text` into the asked thread through the inbox as the user (a steer if the task runs, queued otherwise), fenced as "from <primary> on your behalf", 4 KiB cap; refused for a run that is not this user's or has ended | 2 |
| `delegation_stop` (`run_id`) | `MUTABLE_EXTERNAL`, ask by default | stops the asked run as the user; the report still arrives with `reason: "stopped by you"` | 2 |

**Rule (locked, proposed):** the primary never approves a tool call,
decides a gate, or supplies a credential for an asked run. Approvals stay
with a person (`run-receipt.md` decision 4). `delegation_status` returns
the approval or gate id so the person can decide; it is never a decision.

A steer sent by the primary is text the primary composed, possibly from
content it read; the ask-by-default and the fence are the mitigation, as
for `delegate_run` itself (P-A keeps the asked run from acting on it as a
delegation).

### 14.2 Notifications: one record, many surfaces

The design leaned on `vam.notify` and treated it as Slack delivery. The
owner's case shows the gap: an asked run parks on an approval while the
user talks to the primary, and nobody tells them. Proposal **P-D**: a
notification module in `vamd`, designed in a small `design/notifications.md`
and built in E12 Phase 2's slot, with Slack as its first external channel
rather than its purpose:

- **One record per event and user**: id, kind, severity, run id, thread id,
  Space id, created, read_at, acted_at, the link that resolves it.
  Producers: approval pending, gate opened, run finished, **report
  arrived** (§5), grant expiring (`hsp-triggers.md` §7 q2), budget hit,
  delegation stopped or expired.
- **Consumers**: the app (badge, list, mark read), mobile push and web
  (same list through the API), Slack and email as channels with per-user
  preferences, and **agents**: a `notifications_list` tool (read-only) so
  the primary can answer "anything waiting on me?" and name the child.
- **Acted-on clears everywhere**: a gate decided in Slack clears the app
  badge; marking read is per user, not per channel.

The primary agent's Phase 1 depends on the module's first slice (the
record, the API list and mark-read, the app badge, the `notifications_list`
tool), not on Slack. §12 Phase 1 and the lane rows in Appendix C change
accordingly: row 16 is blocked by "notifications slice 1" instead of
E12 Phase 2 as a whole.

### 14.3 "What is going on?"

That is `delegation_status` in Phase 1. The primary reads it and answers in
its own words; when the child waits on an approval or gate, the answer
carries the id and the asked thread so the user can decide or look.

### 14.4 Limits

What bounds how many agents a primary can run and how they interact.
Existing limits are cited; the two new ones are proposals.

| Limit | Value | Where |
|---|---|---|
| Depth | one: a run a person did not start cannot delegate | loop guard, P-A |
| Fan-out per task | 4 | §4 item 4 |
| **Concurrent asked runs per user, across all threads** | **new: 4 running; further asks queue in order and the status says `queue`** | grant issuance counts live asked runs with `source_run_id` owned by the user (`grants.rs`) |
| One live task per workspace | every child in its own Space, so N children is N VMs | E10 locked decision 12 |
| VMs per node | the node's session capacity; a child beyond it queues at start | vamd reconciliation; `agent-runtime.md` §V.5 |
| Active grants per user and broker | 50 by default | manifest `max_active_grants_per_user` |
| Broker rate | per minute and per day | manifest `max_requests_per_minute/day`, enforced by the gateway |
| Per-run budgets | turns, duration, spend from the target agent's own spec | the child's run, metered on its own receipt |
| Org guardrail | `agents:*` deny or ask | permissions as data, org scope |
| **Thread spend** | **new: the primary's thread shows the sum of its delegations' spend** | an E4 item; open question 13 |

The interaction surface is closed: the primary can start (`delegate_run`),
read (`delegation_status`, `notifications_list`), steer and stop (Phase 2);
a child can only report back; children never address each other or the
primary. Nothing here changes locked decisions 4 or 12.

### 14.5 Changes to the phases

- Phase 1 adds `delegation_status` and the dependency on notifications
  slice 1 (P-D); scenario `status-of-an-asked-run-names-what-it-waits-on`.
- Phase 2 adds `delegation_steer` and `delegation_stop`, with the app's
  Stop and "tell it" on the ask card; scenarios
  `steer-through-the-primary-reaches-the-asked-thread` and
  `stop-through-the-primary-still-reports-back`.
- Open question 13 (new): is thread spend an E4 line or shown from receipts
  directly? Default: E4.

## What we are not building

- **Several agents in one VM** or in one run (composition, in-loop
  delegation). `spawn_agent` stays E10 Phase 5; child VMs later.
- **Agent-to-agent chat.** The asked agent gets one message and returns one
  report; it cannot ask the primary a question mid-run (its questions go to
  the person).
- **Recursion.** Depth one: an asked run, a triggered run and a report turn
  cannot delegate.
- **Apps, org accounts, shared Spaces.** The primary is a column on the user.
- **Autonomous delegation.** No delegation without a person's message in
  that task: none from schedules, events or reports.
- **Synchronous waiting** (`wait_for` a delegation) and progress relay.
- **Cross-user delegation.** The asked run always runs as the person who
  asked, never as another user.
- **A planner** that builds task graphs. The model asks; the person approves.

---

## Appendix A: amendment applied to `design/vam-apps-spaces.md`

A marked block, "Amendment 2026-11-01: the primary agent without Apps", is
added to that doc with this one, after its 2026-10-29 amendment. It names
which of its sentences about the primary are superseded (the gap at line 21,
line 52, the second sentence of the "Personal Apps" locked row as proposal
P-B, the default-App step of "Starting a run" for the primary, the scenario
row "Alice changes her primary agent", "Absorbs primary agent" in "What
changed from `vam-subjects.md`", and `users.default_app_id` in the data
model) and points here. Nothing else in that doc changes.

## Appendix B: proposed text for `.cursor/workstreams/work-outside-chat.md` "Explicitly later"

Not applied (a design session does not edit trackers). Replace the bullet at
line 1226 with:

> - Resume grants, `wait_for`, `DeliverEvent`, `vam.github` (later E5
>   phase). The primary agent does not need them: its result path is
>   **report back**, a host-written queued message `vamd` puts in the
>   asking thread when a run it started for another agent ends
>   (`design/primary-agent.md` §5), built in the `primary-agent` lane.
>   `DeliverEvent` will reuse that inbox entry.

And add after the "On-demand broker runs" bullet (line 1217):

> - Delegation by the primary agent is **not** option B: it uses a per-call
>   grant minted on an approved `delegate_run` call, so locked decisions 5
>   and 9 hold unchanged. It reads decision 6 as "any run a person did not
>   start" (`design/primary-agent.md` §9, P-A, owner to confirm).

## Appendix C: proposed rows for the 0.1.0 launch plan

Not applied. §4, after row 15 (outside 0.1.0 unless the owner moves them):

| # | Lane | Phase | Blocked by | Why | Unblocks |
|---|---|---|---|---|---|
| 16 | Product | **Primary agent Phase 1** (`primary-agent.md`): `vam.delegate`, report back, `agent:` permission target, receipt chain, CLI | E12 Phase 2 (row 7) and E10 Phase 4 (row 9) for shared files; breakdown and wave 0 may run earlier | shares `gateway.rs`, the receipt builder, the PEP and the inbox | row 17; the delegation benchmark line |
| 17 | Product | **Primary agent Phase 2**: a primary per user, the shipped assistant, app cards | row 16 | needs delegation | the product's home screen |

§2: the cut row "Child-VM sub-agents and multi-agent delegation" becomes
"Child-VM sub-agents and in-task multi-agent delegation (asking another
agent as a separate approved task is rows 16 and 17)" (P-C).

## Appendix D: proposed benchmark line (request to the benchmark lane)

`design/benchmark.md` §1, row P13, add: "delegated jobs that complete and
report back into the asking thread (pass^k); wrong-agent picks per 100 asks;
parked cost while a delegation runs (must be 0)", shown by "the
`route-the-ask` task". Not applied (that doc belongs to the benchmark lane).

## Appendix E: receipt fields (request to the E12 lane)

Additive JSON beside `requestTrigger` (`receipt.rs:806-818`); proto in
`receipt.proto` by that lane:

- `requestDelegation {byRunId, byAgentUrn, byAgentVersion}` on an asked run
  (from the grant's `source_run_id`).
- `delegations [{agentUrn, runId, grantId, requestDigest, answer}]` on an
  asking run (from its delegate grants; `answer` at issue time).
- `reports [{childRunId, agentUrn, outcome, receiptDigest}]` on a report
  run.
