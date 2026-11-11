# Launch 0.1.0: what ships, what waits, and how a session decides

Status: **proposed** (2026-10-21). Owner decides the cut list. The rubric
in §3 is referenced from `phase-playbook.md` §11 and applies to every
proposal in every tracker from now on.

## 1. What 0.1.0 is

The first VAM an outsider installs. One Linux host with KVM, one command,
one model endpoint (a frontier API key or a local vLLM), and inside an
hour they have run a governed procedure and hold a receipt they can
verify. It is sold on one line:

> A governed run, on your hardware, on your model, that comes back with a
> receipt. Measured.

**Launch proof (all required):**

| Proof | Backed by |
|---|---|
| The live demo: a scheduled `close-prep` run, one denial, a gate decided from Slack a day later, five effects posted once, a signed receipt | playbooks (built), E12 Phase 1, E5 slice, E10 Phase 4 |
| The table: the same task on two frontier and two open-weight models, `machine on` vs `machine off`, pass^k and cost per success | benchmark B0, B1 |
| The security page: every isolation claim names its scenario; CI boots a VM; jailer and TLS on by default | isolation-truth Phase 1, Phase 3 |
| The install: `vam node init` on one host, the app or CLI signs in, the standard base has Python and git | E7 Phase 1, an install slice (new, small) |
| The honest page: what VAM is not for, where open models still lose | benchmark decision 4, docs pass |

**Not required for 0.1.0:** a marketplace, a protocol, an image builder
language, Jig packages, Mast, multi-agent, computer use.

## 2. The 0.1.0 scope, decided with the rubric

Every candidate scored with §3. Proof is how much the item moves a launch
proof or a benchmark line; Cost is waves plus risk plus new surface.

### Build now (high proof, low cost)

| Item | Proof moved | Cost |
|---|---|---|
| E12 Run receipt Phase 1 | demo, security page | 1 phase, data exists |
| Benchmark B0 and B1 | the table | 2 small phases |
| Isolation-truth Phase 1 | security page, receipt isolation line | 1 phase, CI and defaults |
| E5 slice: schedule trigger, in two phases (grants, gateway and ids; then `vam.schedule`). `vam.notify` Slack approve is built in E12 Phase 2 (amended 2026-10-29) | demo | 2 phases, designed (`hsp-triggers.md` § Phases, `schedule-broker.md`) |
| E10 Phase 4 effect journal | demo, table (duplicates 0) | 1 phase, designed |
| E4 slice: pass^k and cost per version on the agent page | table, P5 | 1 small phase, fed by receipts |
| E7 Phase 1: standard base with Python and git | install, `toolchain` task | 1 phase, no Vamfile |
| Install slice: `vam node init`, TLS self-signed, one-host docs | install | 1 wave |
| `SKILL.md` read alongside `SKILLS.md` | community adoption | 1 wave |
| Permissions presets and effective view (Part IV §IV.9, already in E1 scope) | security page | app waves |

### Slice it (high proof, high cost): take only the smallest piece that moves the line

| Item | Keep for 0.1.0 | Defer |
|---|---|---|
| E8 egress (Part V) | Phase 2 guest proxy fail-closed (folded into isolation-truth) | TLS-terminating proxy, inject-by-destination, upstream enterprise proxy (Phases 1, 3, 4) |
| E10 Phase 5 long work | `wait_for` on CI as part of the E5 slice | sub-agents, memory broker |
| E3 governance console | org inventory read-only list (who released what, which version) | guardrail editor, capability-diff queue |
| E2 packaging | `vam.json` holding the version, install from a Git URL | ranges, `vam.lock` resolution, eject and adopt |
| E11 Jig | the basic version in E10 Phase 3 (built-in cards) | tool transactions, packages, agent-written jigs |
| Constrained decoding for self-hosted models (`agent-runtime.md` §X.4) | tool masking on the vLLM path if B1 shows repairs dominate open-model cost | escalation ladder, safe replay |

### Backlog (low proof, low cost): never start a wave for these; carry into a wave that already touches the files

- Origin badges beyond Org, Squrell, Local.
- `vam policy test` and Cedar templates (DX-66, DX-67).
- Partial receipts at each park.
- A2UI export of built-in jigs.
- Version-bump suggestion refinements (changed playbook: minor or patch).

### Cut from 0.1.0 (low proof, high cost): the minimal-value, great-effort list

| Item | Why it adds little at launch | Effort |
|---|---|---|
| Vamfile language and builder microVM (E7 Phases 3–5) | Standard bases cover most agents; no launch proof needs a custom image; 21 doc pages already written | 3 phases, a new untrusted-build path |
| Jig packages, registry, agent-written jigs (E11 Phases 3–5) | Built-in cards cover the demo; packages are a marketplace feature | 3 phases plus publish and consent flows |
| Mast (`mast-framework.md`) | A different product for a different buyer; needs Apps, Subjects, TTL reaping first | two new repos |
| Child-VM sub-agents and in-task multi-agent delegation (asking another agent as a separate approved task is rows 16 and 17) | Locked out of E10 until Phase 5; no launch proof needs it | large, security-sensitive |
| Marketplace backend beyond the Git marketplace: the OCI hub, signing portal, namespace ownership, trust tiers, entitlements, revenue share (E6, DX-120–123) | The 0.1.0 marketplace is the Git marketplace in row 21 (owner, 2026-11-09): signed-by-origin releases installed read-only, an "Unverified" badge and an admin-approved capability summary for non-Squrell listings. The hub replaces Git as the source later without changing the app page | 4 phases plus review operations |
| Apps, Spaces, Subjects, Credentials beyond broker run grants | Multi-tenant and B2B2C; 0.1.0 is single-org. (Proposed 2026-10-29, owner to confirm: opaque Space and thread ids without Apps move into E5 Phase 1, because grants must bind ids, not names; `vam-apps-spaces.md` amendment §3, §6 P2) | large identity migration |
| Driven-mode playbooks (playbooks Phase 4) | Conditional on adherence data that B1 produces | unknown until measured |
| Transparent port-443 routing, enterprise proxy identity headers (E8 Phases 4–5) | Position-A buyers ask, but after the first install, not before | network and rootfs work |
| `vam.memory` broker and "learns" (E10 Phase 6) | No launch proof; the benchmark measures it later (U7) | 1 phase plus storage |
| eBPF filter | Host proxy already enforces egress; build only after the isolation-truth decision | kernel work |
| Snapshot `WARM` and a paused standby pool | A session start with golden restore takes about 225 ms on the CI lane (median, n=15; the VM itself is ready in about 60-120 ms; measured 2026-10-24, isolation-truth W3b), which is enough for the demo. Re-measured on the standard base `vam/base:2` (2026-11-10, machine-base MB-1.11, GitHub-hosted ubuntu-latest): golden restore median ~150 ms (n=9, run 42756578260) and ~217 ms (n=12, run 42724833562), against ~220 ms (n=12) on the minimal base (run 42751788600); cold boot to registered 169 to 249 ms against 248 ms. The 0.1.0 conclusion still holds, and runner variance between runs is large (the same commit differs by more than the base does) | VMM work |
| Full Jig rendering in Slack | A link into the app covers the demo | per-surface mapping |

The owner may move any row. The table exists so that the move is explicit.

## 3. The rubric: build now, slice, backlog, or cut

Any session proposing an item (a wave, a phase item, a backlog row, a new
design) answers five questions in the tracker, then scores two axes.

**Five questions**

1. Which problem (P1–P14) does it serve, and which benchmark line
   (`benchmark.md` §1) or launch proof (§1 above) does it move?
2. What is the smallest slice that moves that line?
3. How many waves, which model, and what new surface (API, proto, guest,
   policy, docs) does the slice add?
4. Which locked decision does it touch? (Touching one means stop and ask;
   playbook §10.)
5. Is it reversible? Could it ship behind a flag or as a docs-only
   change first?

**Two axes, 0 to 3 each**

| Proof (value) | Meaning |
|---|---|
| 3 | A launch proof cannot be made true without it, or it moves a must-be-zero line |
| 2 | It moves a published benchmark line or closes a confirmed defect |
| 1 | It serves a problem with no line yet, or improves a line already green |
| 0 | No problem, no line, no proof |

| Cost | Meaning |
|---|---|
| 0 | One wave, files already touched this phase, no new surface |
| 1 | One wave, new tests, no new surface |
| 2 | One phase, or a new API or proto field |
| 3 | More than one phase, a new guest path, a new trust boundary, or a new repo |

**Decision**

| | Cost 0–1 | Cost 2–3 |
|---|---|---|
| **Proof 3–2** | **NOW**: plan it in the current or next phase | **SLICE**: find the Cost 0–1 piece that moves the line; plan that; backlog the rest with a trigger ("after B1 shows X") |
| **Proof 1–0** | **BACKLOG**: never its own wave; carried by a wave that already touches the files; reviewed each phase plan | **CUT** for this release; recorded with the number that would change the decision |

**Two overrides**

- **Security floor always NOW.** A finding that weakens P1 (a secret
  path, a bypass, a panic in the daemon) is Proof 3 whatever the cost
  (E10 locked decision 1).
- **A locked decision is never scored.** Changing one is a conversation
  with the owner, then an edit to the list.

**How it is written in a tracker**

```
Decide: SLICE — proof 3 (demo: gate decided from Slack), cost 3 (E5 full) →
slice: schedule trigger + notify approve only, cost 1. Rest: backlog, trigger "after 0.1.0".
```

One line per item, next to the item. The phase-breakdown agent fills it;
the orchestrator checks it; the owner reads it.

## 4. Lanes and blockers to 0.1.0 (added 2026-10-26)

Three lanes run at once, one orchestrator session each in its own worktree
(`working-agreement.md`). A fourth only when review time allows. State on
2026-10-26: E10 Phase 2, isolation-truth Phase 1 and E12 Phase 1 are on
`main`. Only the phases 0.1.0 needs are listed; later phases are planned
when these close.

| # | Lane | Phase | Blocked by | Why | Unblocks |
|---|---|---|---|---|---|
| 1 | Proof | **H0 + B0** (one wave): switches, honest errors, loop detector, recitation, scenario `model` block, usage in `summary.json` | nothing | small, `vam-runtime`, `vam-spec`, `vam-e2e`, `scripts/bench` | E10 Phase 3 build waves (shares `agent.rs`), H1, B1 |
| 2 | Truth | **IT Phase 2**: redaction of encoded secrets, frame cap, loopback-only guest proxy, API rate limits, daemon panics, EC2 defaults | nothing | `host_tools.rs`, `egress.rs`, `daemon.rs`, API | IT Phase 3 |
| 3 | Product | **E10 Phase 3** steer: breakdown and wave 0 scenarios now; build waves after H0 merges and the base takes `main` | H0 (build waves only) | both edit `agent.rs` | E5 slice (`wait_for` needs the inbox), E10 Phase 4 |
| 4 | Proof | **H1** gateway adapters: catalog entries, thinking handling, `tool_choice`, resample, effort, examples, two default profiles | H0 | `vam-inference`, catalog | H2 |
| 5 | Truth | **IT Phase 3**: honest docs, clippy gate, eBPF decision, build pages behind a banner | IT Phase 2 | docs, CI | security page of the launch |
| 6a | Product | **E5 Phase 1, grants, gateway and ids** (amended 2026-10-29): run grants as Credentials, the trigger gateway (one socket per broker), shared start path, triggered runs queue through the inbox, Space and thread ids, a test broker (`hsp-triggers.md` § Phases). `wait_for`, resume grants, `vam.github` and "connect your services" move to a later E5 phase | E10 Phase 3 (on `main`) | builds on the inbox queue and the run start E10 Phase 3 reshaped | 6b, E12 Phase 2 |
| 6b | Product | **E5 Phase 2, `vam.schedule`** (`schedule-broker.md`): a Rust broker in `brokers/`, six tools, cron and once with time zones, catch-up, revoke on delete; the TypeScript prototype is retired | 6a | needs grants and the gateway | the demo's scheduled run, B2 |
| 7 | Product | **E12 Phase 2, reshaped 2026-11-01 (P-D, `design/notifications.md`)**: a notification record with many surfaces (app badge and list, `notifications_list` tool, then Slack as the first external channel); the receipt in the app and in Slack; gate decisions record their surface | 6a | deciding a gate from Slack is a reverse call through the gateway and its broker credentials | the live demo (with 6b) |
| 8 | Proof | **B1 + H2**: `machine off`, business tasks and worlds, cost per success, the ablation grid, first profiles, the `close-prep` challenge page. Tasks and worlds may be written after H0; scored runs need E12 Phase 1 (on `main`) and the owner's word (paid models) | H1; owner's word for the paid runs | receipts are the scorer's input; the grid costs money | H3, the launch table |
| 9 | Product | **E10 Phase 4** effect journal, resume after crash, provider fallback | E10 Phase 3 | journal keys ride the thread and inbox | E12 Phase 3, B2 (duplicates and lost tasks rows) |
| 10 | Product | **E12 Phase 3**: effects section on the receipt; receipts aggregated per agent version | E10 Phase 4 | the journal | E4 slice |
| 11 | Proof | **B2**: `pr-process`, `candidate-onboarding`, `sfdc-change`; parked cost; U3, U4, U5 scored | 6b, E10 Phase 4, B1; `pr-process` also the later E5 phase (`wait_for`, `vam.github`) (amended 2026-10-29) | triggers and the journal exist | launch table rows |
| 12 | Product | **E4 slice**: success, cost and pass^k per agent version on the agent page | E12 Phase 3 | reads receipts | the table in the product |
| 13 | Proof | **H3**: ranking and deferred tools, parallel calls, fresh-context helper, verify-before-finish, escalation, fallback; only the levers with a row | H2 | the grid decides | profile bumps |
| 14 | Product | **E7 Phase 1** standard base with Python and git, plus the install slice (`vam node init`, TLS self-signed, one-host docs) | nothing technically; scheduled after the lane frees | rootfs and scripts | `toolchain` task, the install proof |
| 15 | Proof | **B3** public lane: GitHub Action with a model secret, community tasks and rows, Firecracker lane adds isolation facts | B1, IT Phase 1 (done) | | community |
| 16 | Product | **Primary agent Phase 1** (`primary-agent.md`): `vam.delegate` with `delegate_run` and `delegation_status`, report back, `agent:` permission target, receipt chain, CLI | E10 Phase 4 (row 9) and notifications slice 1 (row 7, reshaped by P-D); breakdown and wave 0 may run earlier | shares `gateway.rs`, the receipt builder, the PEP and the inbox | row 17; the delegation benchmark line |
| 17 | Product | **Primary agent Phase 2**: a primary per user, the shipped assistant, app cards, `delegation_steer` and `delegation_stop` | row 16 | needs delegation | the product's home screen |
| 18 | Product (second lane) | **Connectors Phase 1** (`design/connectors.md` §10): connector definition, `microsoft-365` and `linear` built in, admin enable and consent, member tiles, Linear working as an MCP connector, the Entra profile in the app's OAuth flow | nothing (Phase 0, the `connection` vault kind, is on `main`); its last wave, the real-tenant check, needs the Squrell Entra app | the first thing a non-developer tester can do: connect the tools they use | rows 19, 20 |
| 19 | Product (second lane) | **Connectors Phase 2**: the connection behind skills (per-audience tokens, refresh before swap, reauth park, `requiresConnector`) **plus the per-call egress envelope** for connection bindings (host, method, path prefix per declared tool; owner 2026-11-07) | row 18; the `mcp-oauth-connections.md` amendment (applied 2026-11-07) | a skill reaches Graph as the user without holding a token, and a tool declared as a read cannot send | row 21 |
| 20 | Product (second lane, parallel with 19) | **Connectors Phase 3, sliced**: cards from skills for the mail and calendar tools only (`ToolSpec.ui`, the `List` primitive) | row 18 | different files from row 19 | row 21 |
| 21 | Product (second lane) | **Connectors Phase 4, with Phase 5 folded in (owner, 2026-11-09)**: Microsoft 365 in use (the `m365-mail` skill, the Executive assistant starter agent, the morning triage playbook on `vam.schedule`) enabled from the **Git marketplace** (`design/connectors.md` D23 to D27, §9.1): trusted marketplace URLs per node, `squrell-inc/vam-marketplace`, listing import at a tag, the Org settings Marketplace page, "update available"; and connectors as installable marketplace artifacts (`urn:vam:connector`, D26) so agents and their connectors install in one click | rows 19, 20; Microsoft publisher verification (owner, longest lead time) | the launch demo on a real tenant, installed from the marketplace instead of hand-built | the launch |

Rows 1 to 3 start today. Rows 4 and 5 follow their lane without waiting
on the others. Row 8 is the first paid step. Row 14 can slot into any lane
that frees up, since nothing blocks it. Rows 16 and 17 (the primary agent) are outside 0.1.0. Rows 18 to 21
(connectors) are **in 0.1.0** (owner, 2026-11-07: testers on the 0.1.0
build could not make a useful agent because connecting the tools they use
was too hard for a non-developer; MCP rarely exists for those tools) and run
as a second product lane beside rows 10 and 12. E10 Phases 5 and 6, E8
Phases 2 and 3, E11 and E2 are outside 0.1.0 and are planned later.

**Conflict rule.** Two lanes never build in the same file at the same
time. The known pair is H0 and E10 Phase 3 in `agent.rs`; H0 goes first.
If a second pair appears in a breakdown, the later lane holds its build
waves and does breakdown and scenarios meanwhile.

## 5. Open questions

1. Is the install slice its own wave in the E7 Phase 1 plan, or a
   release task?
2. Does 0.1.0 require the Firecracker lane green in CI (isolation-truth
   Phase 1) or only jailer and TLS on by default with the lane as 0.1.1?
3. Which two open-weight models are in the launch table?
4. Does the launch table need the `machine off` column for all four
   tasks, or two?

## What we are not building

- No launch date in this document. The proofs decide.
- No feature added to 0.1.0 that has no row in §2.
