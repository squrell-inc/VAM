# Run receipt: the signed record of what an agent did

Status: **proposed** (2026-10-21). Epic **E12 Run receipt**. No tracker yet;
E12 opens `.cursor/workstreams/run-receipt.md` when its first phase is
planned. Placement: see `agent-experience.md` "Build order" and §5 below.

## What we are building

A **run receipt** is the one document a run leaves behind: what the host
observed, who decided what, which effects happened, what it cost, and a
signature that lets someone verify it later without `vamd`. Every other part
of VAM (microVM, brokers, Cedar, playbooks, the effect journal, telemetry)
exists to make the receipt true.

Today `vam evidence receipt <run>` prints playbook step state from
`GET /v1/runs/{id}` (`crates/vam-cli/src/cmd/evidence.rs`
`format_playbook_receipt`), and `vam evidence export` / `verify` write and
check an evidence directory offline. That is the seed. This design turns it
into a complete, two-tier, signed receipt with one renderer for the CLI, the
app and `vam.notify` surfaces.

## Why

- **P2 ("we don't know what our agents did, or who allowed it")** and
  **P5 ("agents are unreliable and we can't tell if they're getting
  better")** are the two problems `agent-experience.md` Part I marks as
  least served. Both are answered by a receipt that a controller or auditor
  reads without opening the app.
- The data already exists on the host: `RunStatus.step_statuses`,
  `resolved_refs`, HITL decisions, PEP decisions with side-effect class,
  run-level metering (H1), the telemetry journey, and the evidence pack.
  Nothing new is observed; it is assembled, separated by trust tier, signed
  and rendered.

## Locked decisions (proposed)

Do not re-litigate without editing this list.

1. **Two trust tiers, visibly separated.** Everything the host observed
   (steps, outcomes, evidence, policy decisions, gate decisions, effects,
   spend, pins, isolation facts) is the receipt. Anything the model wrote
   (summaries, step notes) is a labelled appendix: "written by the model,
   not verified by the host". The renderer never mixes them.
2. **Every pin is on the page.** Agent, playbook, skill, policy and image
   versions with digests from `resolved_refs`; the model and the broker
   endpoint it ran on; the workspace HEAD before and after.
3. **Denials and asks are first-class lines.** A tool call denied by Cedar
   or a playbook guard, and a call that waited for approval, are shown
   with the rule and scope that decided (`classify_tools` already names
   them). The denial before an approval is the line a reviewer reads
   first.
4. **Approval provenance.** For every gate or tool approval: who decided,
   from which surface (app, CLI, Slack, email), when, and the validated
   response (the selected subset). Later calls constrained by
   `args_within` reference the gate that constrained them.
5. **Effect accounting is honest.** The effects section lists
   non-idempotent effects with their journal state (`confirmed`,
   `failed`, `unknown`) and explicit counts of duplicates and unknowns.
   Until the effect journal (E10 Phase 4) exists, the section says
   "effects not journaled on this version" rather than printing zeros.
6. **Signed by the node, verifiable offline.** `vamd` signs the canonical
   receipt JSON with the node's Ed25519 key (`vam-security`). `vam receipt
   verify <file>` checks the signature and the digests of the evidence
   pack with no daemon. The signature covers the host tier only; the
   model appendix is included by digest.
7. **One canonical JSON, three renderers.** `GET /v1/runs/{id}/receipt`
   returns the canonical document. The CLI prints markdown, the app draws
   it (as a built-in jig once E11's basic version lands), and `vam.notify`
   posts a short form with a link. No renderer adds facts.
8. **Receipts are immutable and retained with the run.** A receipt is
   produced when a run reaches a terminal state and again when a parked
   run resumes and ends (one receipt per run, superseding partials, with
   the lifecycle generations listed). Retention follows the run record.
9. **No compliance language.** Fields are named for what was observed
   (`denied by guard`, `decided by`, `evidence: oracle.post_journal`), not
   for a framework. Mapping to OWASP ACS or audit standards is a docs
   page, not a receipt field.

## 1. The document

### 1.1 Sections, in order

| Section | Source today | Tier |
|---|---|---|
| Header: run id, state, started, ended, active vs parked time, lifecycle generations | run record, `run_checkpoints` | host |
| Who and what: requester and surface (user, schedule, webhook), agent and playbook refs with version and digest, skills, policy attachments, image, model and endpoint, workspace HEAD before and after, isolation facts (VMM, jailer, egress path, "no secrets in guest") | `resolved_refs`, session record, inference broker, Drive9, `daemon` config | host |
| Procedure: each step with state, outcome, time, evidence refs, gate decision (who, surface, when, response) | `RunStatus.step_statuses`, HITL store | host |
| Tool calls: counts allowed, asked, denied; each denial and ask with the deciding rule and scope; read-only calls summarised | PEP decisions (`security.rs`), `classify_tools` | host |
| Effects: non-idempotent effects with journal state; duplicates and unknowns | effect journal (E10 Phase 4); until then a stated absence | host |
| Spend: tokens in and out, estimated cost, turns used of budget, approvals count | run-level metering (H1), inference broker | host |
| Model appendix: final summary and per-step summaries | agent output | model, labelled |
| Attestation: node id, key id, receipt digest, verify command | `vam-security` Ed25519 | host |

### 1.2 Sample (CLI markdown)

```
VAM RUN RECEIPT                                   run_01J9X4K7M2Q8
Status      SUCCEEDED                          2026-10-20 06:12
Duration    active 14m 02s · parked 1d 03h 41m · 2 VM lifecycles

WHO AND WHAT
Requested by   priya@acme.example (member) via schedule vam.schedule/month-end-prep
Agent          urn:vam:agent:vam.acme.example/finance/close-prep  v1.3.0  sha256:7f3a…c219
Playbook       urn:vam:playbook:vam.acme.example/finance/month-end-prep  v2.0.1  sha256:0b91…e4d7
Skills         oracle-fusion v1.4.2  sha256:…  ·  salesforce v2.1.0  sha256:…
Model          glm-5.1 via inference broker (host: vllm.acme.internal), fallbacks: none
Workspace      acme--close-prep--oct-2026  (Drive9 HEAD 3e1c…  →  9a72…)
Isolation      firecracker microVM, jailer on, egress via host proxy, no secrets in guest

PROCEDURE  (host-tracked; 5 of 5 required steps completed)
 1 pull_trial_balance      completed  done        06:00:14   evidence: oracle.query_gl ✔ (1 call)
 2 flag_variances          completed  done        06:03:40   output: 7 variances > 2%
 3 approve_adjustments     completed  approved    next day   GATE  decided by ravi@acme.example
                                                             from Slack #close-oct, 2026-10-20 05:58
                                                             selected 5 of 7 proposed adjustments
 4 post_adjustments        completed  done        06:09:51   evidence: oracle.post_journal ✔ (5 calls)
                                                             guard: args_within steps.approve.selected ✔
 5 notify_controller       completed  done        06:11:37   evidence: vam.notify.send ✔

TOOL CALLS  (19 total · 17 allowed · 1 asked · 1 denied)
 06:04:02  oracle.post_journal      DENIED   guard "no external mutation before approve_adjustments:approved"
 06:04:05  salesforce.update_opp    ASK      agent rule skill:salesforce/update_opp = ask → approved by priya (once)
 06:09:51  oracle.post_journal ×5   ALLOWED  agent rule skill:oracle-fusion/* = allow (agent v1.3.0)
 … 12 read-only calls allowed by side-effect default

EFFECTS  (journal; non-idempotent effects at-most-once)
 5 journals posted: JE-88412 … JE-88416   confirmed   0 duplicates   0 unknown

SPEND
 tokens 184,310 in / 9,842 out  ·  est. cost $0.37  ·  budget 50 turns, used 23  ·  2 approvals

AGENT SUMMARY  (written by the model, not verified by the host)
 "Pulled October trial balance, found 7 variances over 2%, proposed adjustments;
  5 approved and posted; 2 deferred to November per Ravi's note."

ATTESTATION
 Signed by vamd node acme-vam-01 ( key id 4c7e…)   receipt sha256: f1d0…88ab
 Verify: vam receipt verify run_01J9X4K7M2Q8
```

The isolation line prints what the node actually ran. A process-mode guest
prints `process guest (no isolation)`; a jailer that is off prints
`jailer off`. The receipt never claims more than the node did.

### 1.3 Canonical JSON

`GET /v1/runs/{id}/receipt` returns `{ "receipt": {...}, "model_appendix":
{...}, "signature": {...} }`. `receipt` is canonicalised (sorted keys, no
insignificant whitespace) before signing. Field names follow the proto
conventions of `RunStatus`. The exact schema is open question 1.

## 2. What exists and what is missing

| Piece | Exists | Missing |
|---|---|---|
| Step state, outcomes, evidence refs | `step_statuses`, `evidence_refs` | gate decision provenance (surface, response) on the step |
| Pins | `resolved_refs` | model and endpoint on the run record; workspace HEAD before and after; isolation facts |
| PEP decisions | logged per call with side-effect class | a per-run query returning each decision with the deciding rule and scope (`classify_tools` output stored, not recomputed) |
| Approvals | HITL store, `vam approve` | surface of the decision (app, CLI, Slack) once `vam.notify` lands (E5) |
| Effects | none | effect journal (E10 Phase 4) |
| Spend | run-level metering (H1) | estimated cost from the catalog price, if present |
| Signing | `vam-security` Ed25519 signs approval claims | signing the receipt; node key id in the document |
| Export and verify | `vam evidence export` / `verify` | receipt inside the evidence pack; `vam receipt verify` as the public name |
| Renderers | CLI markdown step receipt | canonical JSON; app view; notify short form |

## 3. Phases

| Phase | Outcome | Contents | Depends on | Exit |
|---|---|---|---|---|
| **1 Canonical receipt** | One document, two tiers, signed | `GET /v1/runs/{id}/receipt`; run record gains model, endpoint, workspace HEADs, isolation facts; PEP decisions stored per run with deciding rule and scope; gate provenance on steps; Ed25519 signature; `vam receipt show` and `verify`; receipt in the evidence pack; the "effects not journaled" line | E10 Phase 2 (run record and context shape) | Scenario: a playbook run with a denial, an ask and a gate produces a receipt whose host tier matches the scenario's expectations line by line, and `vam receipt verify` passes offline and fails on one changed byte |
| **2 Surfaces** | The same receipt everywhere; every decision names its surface | App receipt view (built-in jig); the run-finished notification links the receipt, delivered through the notification module's channels (`docs/src/content/docs/orchestrator/notifications.md`, slice 2, Slack first); gate decisions taken from a channel are recorded with their surface from the handle `vamd` issued | Phase 1, notifications slice 2 | Scenario: a gate decided from Slack shows `slack` on the receipt |
| **3 Effects and outcomes** | Receipts count effects and feed outcomes | Effects section from the effect journal; receipts aggregated per agent version into E4's success, cost and pass^k; a release diff that compares receipts of the previous version | Phase 1, E10 Phase 4 | Kill-mid-task benchmark run ends with a receipt reporting 0 duplicates and 0 unknown; the agent page shows the aggregate |

Phase 1 is the getting-started slice and is small: it assembles data that
exists, adds four fields to the run record, and signs.

## 4. Scenarios

Named by behaviour, per `design/scenarios.md`:

- `receipt-separates-host-facts-from-model-text`
- `receipt-shows-denial-before-approval`
- `receipt-names-who-decided-and-from-where`
- `receipt-verifies-offline-and-fails-on-tamper`
- `receipt-states-effects-not-journaled-before-journal` (retired by Phase 3)
- `receipt-counts-effects-after-crash` (Phase 3, with E10 Phase 4)

## 5. Placement

After E10 Phase 3 (so the run record has its final shape and chat cards
are jigs), before the E5 slice. E5 then adds the notify surface (Phase 2)
and E10 Phase 4 adds effects (Phase 3). See `agent-experience.md` "Build
order (revised 2026-10-21)".

## 6. Open questions

1. Exact canonical JSON schema and the field naming (proto-derived or a
   standalone JSON Schema in `vam-spec`)?
2. Does the receipt include tool arguments and results? Proposal: digests
   and side-effect class only, with the redacted journey as the evidence
   pack's detail; never raw arguments in the receipt.
3. Node key custody and rotation: who holds the Ed25519 key, and does a
   rotated key invalidate old receipts (proposal: no; receipts carry the
   key id and the node publishes retired public keys).
4. Partial receipts for a run that is parked for days: on demand only, or
   emitted at each park?
5. Cost estimate source when the catalog has no price: omit the line, or
   print tokens only? Proposal: tokens only, no estimate.

## What we are not building

- No new observation. The receipt reads what the PEP, HITL store, metering,
  journal and Drive9 already record.
- No compliance framework mapping inside the receipt.
- No raw tool arguments, results, or secrets (even placeholders) in the
  receipt.
- No receipt for plain chat runs beyond the header, pins, tool calls and
  spend sections; the procedure section appears only for playbook runs.
- No third renderer that invents fields; every surface draws the canonical
  JSON.
