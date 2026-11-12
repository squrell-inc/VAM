# Workstream: Run receipt (E12)

```yaml
status: active          # active | paused | done
slug: run-receipt
current_phase: 3
base_branch: feat/run-receipt
design_docs:
  - design/run-receipt.md
```

## Goal

Every run leaves one document behind: what the host observed, who decided
what, which effects happened, what it cost, and a node signature that anyone
can verify later without `vamd`. Two trust tiers, visibly separated (host facts
signed; model-written text a labelled appendix included by digest). One
canonical JSON (`GET /v1/runs/{id}/receipt`), rendered by the CLI, the app and
`vam.notify` without adding facts. Done when the 0.1.0 demo ends with a signed
receipt and the security page can point at it.

## Out of scope

- New observation: the receipt reads what the PEP, HITL store, metering,
  Drive9 and the journal already record.
- Compliance framework mapping inside the receipt (a docs page at most).
- Raw tool arguments, results or secrets (even placeholders) in the receipt.
- A procedure section for plain chat runs (header, pins, tool calls and spend only).
- Any renderer that invents fields.

## Locked decisions

Do not re-litigate without editing this list. Copied from
`design/run-receipt.md` "Locked decisions" when the tracker opened (2026-10-25).

1. **Two trust tiers, visibly separated.** Host-observed facts are the receipt;
   model-written text is a labelled appendix ("written by the model, not
   verified by the host"). The renderer never mixes them.
2. **Every pin is on the page.** Agent, playbook, skill, policy and image
   versions with digests; the model and broker endpoint; workspace HEAD before
   and after.
3. **Denials and asks are first-class lines**, with the deciding rule and scope.
4. **Approval provenance.** Who decided, from which surface, when, and the
   validated response; later `args_within` calls reference the gate.
5. **Effect accounting is honest.** Until the effect journal (E10 Phase 4)
   exists, the section says "effects not journaled on this version", never zeros.
6. **Signed by the node, verifiable offline.** Ed25519 node key (`vam-security`);
   `vam receipt verify <file>` checks the signature and evidence-pack digests
   with no daemon. The signature covers the host tier; the model appendix is
   included by digest.
7. **One canonical JSON, three renderers.** No renderer adds facts.
8. **Receipts are immutable and retained with the run.** Produced when a run
   reaches a terminal state, and again when a parked run resumes and ends (one
   receipt per run, superseding partials, lifecycle generations listed).
9. **No compliance language.** Fields are named for what was observed.

## Status legend

- `[ ]` not started
- `[~]` in progress
- `[x]` done (code + tests, or merged)

## Phases

### Done phases

Full sections: not kept; see the progress log and git history.

| Phase | Name | Done |
|---|---|---|

### Phase 1 — Canonical receipt (released to `main` 2026-10-26 checklist out)

`Decide: NOW — proof 3 (demo receipt, security page), cost 2 (one phase, data exists)`

**Goal:** one document, two tiers, signed.  
**Depends on:** E10 Phase 2 (run record and context shape; released to `main`
2026-10-25). Reuses E13 Phase 1 node isolation facts and Firecracker lane (on `main`).  
**Plan:** approved 2026-10-26 (owner), all open-question defaults accepted and Q10 decided 2026-10-26: option A (the constant key is fixed in Wave 1 on the base; it reaches `main` with the E12 release, no separate hotfix). Q12 is mandatory, not optional  
**Exit:** Scenario: a playbook run with a denial, an ask and a gate produces a
receipt whose host tier matches the scenario's expectations line by line, and
`vam receipt verify` passes offline and fails on one changed byte.

- [x] `GET /v1/runs/{id}/receipt` (canonical JSON: `receipt`, `model_appendix`, `signature`)
- [x] Run record gains model, endpoint, workspace HEADs and isolation facts
- [x] PEP decisions stored per run with the deciding rule and scope; gate provenance on steps
- [x] Ed25519 node signature over the canonical host tier
- [x] `vam receipt show` and `vam receipt verify` (offline)
- [x] Receipt in the evidence pack, with the "effects not journaled" line

**Breakdown** (2026-10-25, phase-breakdown agent; playbook §3 step 2, §11;
`design/scenarios.md` §8–9). Approved by the owner before any wave starts.

**Code map** (`feat/run-receipt`). **Already built, reused:**
the run's workspace HEADs (`run_workspace_heads`, `workflow.rs:554`; written by
`record_run_head_before`/`_after`, `session_end.rs:357`, `:379`, `:684`; shown as
`finalOutputs.workspace`, `api/mod.rs:2485`); the run result is published only
after the end commit (`publish_run_result`, `session_end.rs:336`), so
`finalize_run_session` (`api/mod.rs:2858`) sees both HEADs when it builds the
run attestation (`:3097`, with `workspace` and the thread-log head). The node's
isolation line comes from one function, `isolation::describe` (`isolation.rs:53`),
served by `GET /v1/node` (`api/mod.rs:130`, `:435`; proto `NodeIsolation`:
`vmm`, `jailer`, `apiTls`, `summary`). Pins: `RunSpec.resolved_refs`
(`catalog_resolve.rs:128`, recorded at `api/mod.rs:1340`); the model is pinned into
`inputs.model` (`allowed_models.rs:224`). Steps: `run_playbook_steps`
(`workflow.rs:471`, with `outcome`, `evidence_refs_json`, `agent_summary`,
`started_at`/`finished_at`). Metering: `run_metering` (`workflow.rs:489`:
iteration, prompt and completion tokens). The session's tool rows already hold
the deciding scope and rule (`session_tool_targets`, `session_targets.rs:432`,
struct `:97`). Gates and tool approvals share the `approvals` table and
`POST /v1/approvals/{id}/decide` (`api/mod.rs:1730`, store `workflow.rs:1095`).
Evidence pack: `assemble_evidence_pack` (`evidence.rs:29`), offline
`verify_evidence_pack` (`vam-telemetry/src/evidence.rs:179`), CLI
`vam evidence export|verify|receipt` (`vam-cli/src/cmd/evidence.rs:12`).
Domain-separated Ed25519 signing exists (`ApprovalSigner::sign`,
`vam-security/src/lib.rs:136`; `verify`, `:158`). Self-signed TLS key custody is the
precedent for a key file (`tls.rs:256`, written `0600` at `:394`).

**Gaps the code shows (some contradict the design):**

- **No node signing key exists.** Every production signer uses the constant
  `SigningKey::from_bytes(&[42u8; 32])`: the session security context
  (`brokers.rs:541`, key id `host-key-<session>`, `security.rs:630`) and the run
  attestation (`api/mod.rs:3131`). The same context mints and checks the
  capability tokens the guest sends back with `ExecuteHostTool` over vsock
  (`host_tools.rs:456`, `:3144`; the expected key id is read from the token
  itself, `:3160`). The key is in the public source, so a guest that knows it can
  likely mint a token for a host tool call (claims are visible in tokens it
  already received). Security floor (launch-0.1.0 §3 override): fixed in Wave 1.
- **Nothing verifies a signature today.** `verify_evidence_pack` checks hashes,
  the audit chain (no key) and unwraps the DSSE; it never checks the attestation
  signature.
- **`classify_tools` is not on the decision path** (`vam-security/src/advertise.rs:258`
  serves explain and migrate). The PEP path (`security.rs:851`, decision at
  `:1000`–`:1295`) emits `policy.decided` with tool, disposition, args digest and
  path only (`trajectory.rs:247`); a guard denial is a reason string
  (`security.rs:1711`). "Stored, not recomputed" means copying the session row's
  `deciding_scope`/`deciding_rule` (or the guard, path-limb or permissions-row
  reason) into a per-run row at decision time; "Always" rewrites the session row
  mid-run, so recomputing later would be wrong.
- **The run record has no start or end time**; `RunStatus.started_at` is
  `now_ts()` on every read (`api/mod.rs:2514`).
- **Approvals carry no decision time and no surface** (`UPDATE approvals`,
  `workflow.rs:1278`: decision, `reviewer_principal`, response). No client sends a
  surface hint.
- **Isolation facts are node-current, not per run**, and the node reports no
  egress path or "no secrets in guest" fact; kernel and rootfs SHA-256 exist only
  inside the golden-snapshot key (`golden.rs:209`).
- **No run is ever deleted** (no `DELETE FROM runs`), so "deleted with the run"
  has no hook yet.
- **`serde_json` has `preserve_order` on in this workspace** (`indexmap` in its
  `Cargo.lock` deps): canonical JSON must sort keys itself.
- **The Firecracker lane runs on every PR of this phase**: the `changes` job
  (`.github/workflows/rust.yml:56`) selects it for any change under
  `crates/vam-orchestrator/`, `e2e/` or `proto/`.
- Harness: the runner speaks HTTP (`crates/vam-e2e/src/api.rs`) and never runs
  the CLI; `scripts/e2e/run.sh:56` already builds `vam-cli` in process mode.
  Dotted `fields` paths take array indices (`engine.rs:1586`); there is no
  "absent" matcher (`engine.rs:1604`).

**Scenario delta**

| Scenario | Change | Layer | Given | Why |
|---|---|---|---|---|
| receipt-shows-denial-before-approval | new | process | org-admin, agent-with-guarded-tool, playbook-with-guard-and-gate (new piece) | the phase's exit: denial, ask and gate in one run; the host tier matches line by line |
| receipt-names-who-decided-and-from-where | new | process | org-admin, org-member (as member), agent-plain, playbook-with-gate | gate provenance: who, surface, when, the validated response; the requester |
| receipt-separates-host-facts-from-model-text | new | process | org-admin, agent-plain, playbook-one-step | two tiers: pins, model, isolation and steps in the host tier; the model's text only in the labelled appendix, by digest |
| receipt-verifies-offline-and-fails-on-tamper | new | process | org-admin, agent-plain | the exit's second half: `vam receipt verify` passes with no daemon and fails on one changed byte; a plain chat has no procedure section |
| receipt-states-effects-not-journaled-before-journal | new (retired by Phase 3) | process | org-admin, agent-with-builtins | a run that wrote a file says "effects not journaled on this version", never zeros; the receipt is in the evidence pack and the pack verifies |
| receipt-names-the-isolation-the-run-had | new, `needs: firecracker` | firecracker | org-admin, agent-plain | the isolation line on a real microVM (`firecracker microVM, jailer on`, image digests); the process scenarios prove `process guest (no isolation)` |
| gate-decided-finishes-the-run | changed: guards add `crates/vam-orchestrator/src/workflow.rs` (the decide path now writes provenance); steps unchanged, stays `active` | process | org-admin, agent-plain, playbook-with-gate | must keep passing |
| approval-suspends-and-resumes | changed: guards add `crates/vam-orchestrator/src/security.rs` (a per-run decision row beside every PEP decision); steps unchanged, stays `active` | process | org-admin, agent-with-guarded-tool | must keep passing; `always-allow-and-always-deny-are-remembered` already guards `security.rs` |

What each new scenario does, and where it is red today (all class `product`,
at the first receipt step: `GET /v1/runs/{id}/receipt` answers 404):

- **receipt-shows-denial-before-approval.** New piece
  `playbook-with-guard-and-gate`: `prepare` → gate `confirm` (go/stop) →
  `finish`, guard `{ tools: [write_file], allowed_when: ["confirm:go"] }`. Mock:
  in `prepare` the agent calls `write_file` (denied by the guard), then completes
  the step; the gate is decided `go`; in `finish` it calls `write_file` (asks,
  agent rule `builtin:write_file = ask`), `approve`, then completes. `then`:
  `expect: { receipt: { fields } }` with `receipt.toolCalls.counts` (1 denied,
  1 asked), `receipt.toolCalls.lines.0` (write_file, denied, decided by the
  playbook guard, rule naming `confirm:go`), `lines.1` (write_file, asked,
  scope `agent`, rule `builtin:write_file`, approved once), and
  `receipt.procedure.steps.1` (confirm, decided, option `go`).
- **receipt-names-who-decided-and-from-where.** The member starts the gate
  playbook and decides the gate (`as: member`). `then`: `receipt.requestedBy`
  contains the member, `procedure.steps.1.gate.decidedBy` contains the member,
  `gate.surface: api` (the runner sends no surface hint), `gate.decidedAt`
  present, `gate.response.option: go`.
- **receipt-separates-host-facts-from-model-text.** `then`:
  `receipt.pins.agent` and `pins.playbook` with version and digest present,
  `receipt.model.0.name: mock-model`, `receipt.isolation.summary: "process guest
  (no isolation)"`, `receipt.workspace.headBefore` present, step `summarise`
  outcome `done`, `receipt.procedure.steps.0.summary` absent,
  `modelAppendix.label: "written by the model, not verified by the host"`,
  `modelAppendix.finalReply` contains `summarised`, `receipt.modelAppendixDigest`
  present.
- **receipt-verifies-offline-and-fails-on-tamper.** One plain chat. `then`:
  `receipt.procedure` absent, `signature.keyId` present; `verify-receipt: {}`
  passes; `verify-receipt: { tamper: true }` fails as it should.
- **receipt-states-effects-not-journaled-before-journal.** The agent writes
  `note.txt`. `then`: `receipt.effects.journaled: false`,
  `receipt.effects.statement: "effects not journaled on this version"`,
  `receipt.effects.duplicates` absent; `verify-receipt: { pack: true }` passes
  and the pack holds `receipt.json`.
- **receipt-names-the-isolation-the-run-had.** One chat on the microVM lane.
  `then`: `receipt.isolation.summary: "firecracker microVM, jailer on"`,
  `receipt.isolation.jailer: true`, `receipt.image.rootfsDigest` present. Not
  runnable in the cloud: Wave 0 lands it `pending` and lints it.

**Runner additions** (Wave 0, `crates/vam-e2e`, `scripts/e2e/run.sh`,
`e2e/README.md`):

- `expect: { receipt: { fields } }` reads `GET /v1/runs/{latest}/receipt` and
  matches dotted camelCase paths over `{receipt, modelAppendix, signature}` (as
  `node` does). A non-2xx answer is `product`.
- Matchers `{ absent: true }` and `{ present: true }` for any `fields` family.
- Step `verify-receipt: { tamper, pack }` writes the latest run's receipt (or,
  with `pack: true`, its evidence pack from `GET /v1/runs/{id}/evidence`) under
  the scenario's out directory and runs the built `vam` binary with no host and
  no key (`vam receipt verify <file>` or `vam evidence verify <dir>`); it passes
  on exit 0. `tamper: true` flips one byte inside the signed `receipt` object and
  passes only on a non-zero exit that names the signature. `run.sh` exports
  `VAM_E2E_CLI` (the `vam` it builds); a missing binary is `env`.

**Benchmark delta** (§11). Line moved: **P2 receipt completeness** ("decisions
with a named rule and scope (100%); verify passes", `benchmark.md` §1). Baseline
on the current base, by construction and without a run: not measurable; no
receipt exists, `policy.decided` names no rule or scope, and nothing verifies a
signature. After: measured on the mock by `receipt-shows-denial-before-approval`
(every decision line has a rule and scope) and
`receipt-verifies-offline-and-fails-on-tamper` (verify passes). No task added;
B1 (`close-prep`, `runaway`) consumes the receipt as the scorer's input. P1
guarded-call violations becomes readable from the receipt but does not move. Cost
per success stays tokens only (no estimate). No paid baseline (§10).

**Items**

- [x] `RR-1.1` — **Harness additions (Wave 0).** The runner additions above;
      engine self-tests for each (receipt fields against a canned body,
      absent/present, verify pass, tamper detected, missing CLI is `env`);
      `check.rs` accepts the new step; `e2e/README.md` documents them. A
      `skip-vm-lane` label check in the `changes` job of
      `.github/workflows/rust.yml` (owner, 2026-10-26: mandatory) that sets
      `vm=false` for a PR carrying the label; documented in `e2e/README.md`
      "The microVM lane". **Accept:** `cargo test -p
      vam-e2e`; `cargo run -p vam-e2e -- check` clean; `--smoke` still green.
      **Test:** `cargo test -p vam-e2e`, `scripts/e2e/run.sh --smoke`.
      `Decide: NOW — proof 3 (the exit cannot be shown without it), cost 1 (one wave, tests, no product surface)`
- [x] `RR-1.2` — **Scenario delta (Wave 0).** The six new scenarios `pending`
      with `expectFail` at their first receipt step, world piece
      `playbook-with-guard-and-gate`, the guards change to the two existing
      scenarios. Confirm in a run that the denial-ask-gate flow reaches the
      receipt step (a failure earlier is a product finding to report, not to
      paper over), and that `step_complete` is allowed under the agent's
      `default: deny` (else the new piece pairs with its own agent piece). The
      `fields` paths written here are the schema names `RR-1.3` uses; changing
      one later is a spec change. **Accept:** `scripts/e2e/run.sh --scenarios
      receipt-shows-denial-before-approval,receipt-names-who-decided-and-from-where,receipt-separates-host-facts-from-model-text,receipt-verifies-offline-and-fails-on-tamper,receipt-states-effects-not-journaled-before-journal`
      each fails at the declared step, class `product`; the firecracker one is red
      on the Wave 0 PR's lane run (lane run 1). **Test:** that command, `vam-e2e check`.
      `Decide: NOW — proof 3 (spec of the exit), cost 0`
- [x] `RR-1.3` — **Receipt schema, canonical form, sign and verify (scaffold).**
      `proto/vam/v1/receipt.proto`: `RunReceipt` (header, requestedBy, pins, model,
      workspace, isolation, image, procedure, toolCalls, effects, spend,
      modelAppendixDigest), `ModelAppendix`, `ReceiptSignature` (algorithm, keyId,
      publicKey, receiptDigest, value), `ReceiptDocument` (the three). Only the
      fields Phase 1 fills; effects is `{journaled: false, statement}`. A pure
      module (vam-spec `receipt`, signing in vam-security): canonical bytes
      (parse the emitted JSON, sort keys recursively, compact, integers only),
      digest, `sign_receipt` (domain `vam.receipt.v1`, key id bound in the
      signed bytes), `verify_receipt(json_bytes, trusted_keys)` that never
      round-trips through proto (a newer field must not break an old verifier).
      **Accept:** golden vector (fixed key → fixed signature); one changed byte in
      `receipt` fails; a changed `modelAppendix` fails by digest; reordered keys and
      whitespace still verify; wrong key id fails. **Test:** `cargo test -p
      vam-spec receipt`, `cargo test -p vam-security receipt`.
      `Decide: SLICE — proof 3 (demo receipt; P2 "verify passes"), cost 2 (new proto) → slice: Phase 1 sections only; app, notify and journal fields wait for Phases 2 and 3`
- [x] `RR-1.4` — **Node signing key; the constant key retired.** Generated on
      first start under the vamd state dir (`signing/node-ed25519.key`, `0600`,
      atomic write as `tls.rs:394`); key id:<first 16 hex of
      sha256(public key)>`; public keys of `signing/retired/*` loaded too.
      `GET /v1/node` gains `signingKeys` (`keyId`, `publicKey`, `state`
      current|retired); `vam node keys` prints them as a file `verify` can take.
      The run attestation is signed with the node key (`api/mod.rs:3131`); the
      session context (`brokers.rs:541`) gets a per-session key derived from the
      node secret (stable across a vamd restart, never in source). **Accept:**
      key survives restart with the same id; file mode `0600`; a capability token
      signed with the old constant is refused; `/v1/node` lists the key; a retired
      key is listed as retired. **Test:** `cargo test -p vam-orchestrator
      signing`, `cargo test -p vam-orchestrator host_tools` (token tests),
      `scripts/e2e/run.sh --smoke`.
      `Decide: NOW — proof 3 (security floor: a public signing key on the capability-token path; a receipt signed with it proves nothing), cost 1 (one wave, files named, no new surface beyond one node field)`
- [x] `RR-1.5` — **Run record facts.** `runs` gains `created_at`, `ended_at`
      (set at create and at the terminal publish); `RunStatus.started_at` and
      `finished_at` report them (bug at `api/mod.rs:2514`). A per-run models table
      filled from the broker's inference callback (`brokers.rs:~700`: model,
      provider, endpoint host from the catalog entry, call count), so fallbacks
      show. Isolation facts recorded per run and lifecycle generation at session
      start (`isolation::describe` plus kernel and rootfs SHA-256 in VM mode from
      the golden hash cache, `golden.rs:209`; none in process mode). Requester:
      owner user and the `X-Vam-Surface` hint on `create_run`. Workspace HEADs:
      reuse `run_workspace_heads`. **Accept:** unit tests per table; a resumed run
      keeps its first start time; a process run records `process` and no image.
      **Test:** `cargo test -p vam-orchestrator run_facts`, `--smoke`.
      `Decide: SLICE — proof 3 (locked decision 2, every pin on the page), cost 2 (schema) → slice: record only what the receipt prints; no new RunStatus fields beyond the two timestamps`
- [x] `RR-1.6` — **Decisions per run; gate and approval provenance.** A per-run
      decision table written beside `emit_policy_decided` (`security.rs:1286`):
      sequence, time, tool, target, side-effect class, disposition
      (allowed|asked|denied), decided by (cedar rule | playbook guard | path limb
      | missing permissions row | start-playbook/run-step gate), deciding scope
      and rule (from the session row at that moment, or the guard/path reason;
      legacy mode: matching policy ids), args digest, approval request id. No
      paths, previews or arguments. It never changes a disposition. `approvals`
      gains `decided_at`, `decided_surface`, `decided_by_user_id`; the decide
      handler (`api/mod.rs:1730`) reads `X-Vam-Surface` (`cli`, `app`; anything
      else `api`); the CLI sends `cli` on every request. **Accept:** unit tests:
      a guard denial, an ask then approve, an "Always allow" mid-run keeps the
      earlier row's rule; gate decision stores time, surface, decider; existing
      approval and always-allow scenarios green. **Test:** `cargo test -p
      vam-orchestrator decisions`, `cargo test -p vam-orchestrator hitl`,
      `scripts/e2e/run.sh --scenarios approval-suspends-and-resumes,always-allow-and-always-deny-are-remembered,gate-decided-finishes-the-run`.
      `Decide: SLICE — proof 3 (P2 line: every decision with a named rule and scope), cost 2 (new table, columns, header) → slice: store at decision time, surface from a client hint (cli/api); Slack surface in Phase 2`
- [x] `RR-1.7` — **Assemble, issue, store, serve.** A receipt module in
      vam-orchestrator builds the host tier from RR-1.5/1.6 rows, `resolved_refs`,
      steps (gate provenance; `args_within` calls name the gate step whose output
      constrained them), metering (tokens in/out, turns used, approvals count; no
      cost), the effects statement, and the model appendix (step
      `agent_summary`, final reply) by digest; signs it with the node key.
      Issued once at a terminal state on every terminal path
      (`finalize_run_session` after the attestation, `cancel_run_by_id`
      `api/mod.rs:1607`, HITL expiry `workflow.rs:1433`, resume failures
      `api/mod.rs:~1885`) and by a startup reconcile for terminal runs with none.
      Insert-only `run_receipts` (run_id, generation, document, digest, key_id,
      created_at). `GET /v1/runs/{id}/receipt` (`RunPathKind::Receipt`,
      `api/mod.rs:1507`), same auth as run status; 409 while the run is not
      terminal. Plain chat runs: no procedure section. If an event is added, it
      joins `KNOWN_EVENTS` (`vam-e2e/src/events.rs:7`). **Accept:** unit tests
      (one receipt per run, immutable, re-issue is a no-op, reconcile fills a gap,
      no raw argument in the document); the scenarios below green; the
      firecracker scenario green on this PR's lane run (lane run 2).
      **Test:** `cargo test -p vam-orchestrator receipt`, `scripts/e2e/run.sh
      --scenarios receipt-shows-denial-before-approval,receipt-names-who-decided-and-from-where,receipt-separates-host-facts-from-model-text`.
      Turns green: those three and `receipt-names-the-isolation-the-run-had`.
      `Decide: SLICE — proof 3 (demo receipt, security page), cost 2 (new API) → slice: one GET, issued at terminal only; partial receipts stay backlog`
- [x] `RR-1.10` — **Gate references on constrained calls (carried from Wave 3, owner 2026-10-26).**
      A tool call whose arguments were constrained by `args_within` names the gate
      that constrained it (`toolCalls.lines[].gateStep`), per locked decision 4 and
      open question 13 (unit tests only, no scenario). **Accept:** unit tests in
      `receipt.rs`. `Decide: NOW — proof 3 (locked decision 4), cost 0 (one wave, files already touched)`
- [x] `RR-1.11` — **Refuse a decision on a finished run (carried from Wave 3, owner 2026-10-26).**
      `decide_approval` can set an already-terminal run back to RUNNING; with
      immutable receipts that leaves a signed receipt for a running run. Refuse a
      decision on a terminal run with 409 (state checked in the same transaction),
      with a unit test and an `api_tests` case. **Accept:** existing approval and gate
      scenarios stay green. `Decide: NOW — proof 2 (closes a confirmed defect), cost 1 (one wave, tests)`
- [x] `RR-1.8` — **`vam receipt show` and `vam receipt verify`.** `show <run>`
      renders the canonical JSON as the design §1.2 markdown (no field the JSON
      lacks; the appendix under its label; "effects not journaled on this
      version"); `--json` prints the document. `verify <file>` is offline (no
      host, no token): signature, model appendix digest, key id; prints key id and
      "key not pinned" unless `--trusted-keys <file>` (from `vam node keys`) is
      given, where a mismatch fails. `vam evidence receipt` becomes an alias of
      `show` with a one-line notice. Docs: a receipt page (fields, verify, trust)
      the security page can link. **Accept:** renderer golden test; verify exit
      codes; offline with `VAM_HOST` unset. **Test:** `cargo test -p vam-cli receipt`.
      Turns green: `receipt-verifies-offline-and-fails-on-tamper`.
      `Decide: NOW — proof 3 (demo, security page "verifiable offline"), cost 1`
- [x] `RR-1.9` — **Receipt in the evidence pack.** `assemble_evidence_pack` adds
      `receipt.json` (the stored document, byte for byte); the manifest hashes it;
      `verify_evidence_pack` checks the receipt signature and that its run id and
      trajectory digest match the pack, and now also checks the attestation's
      signature against the node key the receipt names; a pack without
      `receipt.json` (older runs) still verifies. **Accept:** unit tests (tampered
      receipt fails the pack, old pack passes). **Test:** `cargo test -p
      vam-telemetry evidence`, `cargo test -p vam-orchestrator evidence`.
      Turns green: `receipt-states-effects-not-journaled-before-journal`.
      `Decide: NOW — proof 2 (P2 "verify passes" on the pack), cost 1`

**Waves** (model: Sonnet low by default, medium second, high rarely with a reason)

| Wave | Items | Turns green | Repo | Model | Budget | Depends on | Status | PR |
|---|---|---|---|---|---|---|---|---|
| 0 | `RR-1.1`, `RR-1.2` (scenarios) | — (red, right reason; lane run 1) | vam | Sonnet low | 60 min | — | [x] | — |
| 1 | `RR-1.3`, `RR-1.4` | — (unit tests: vectors, tamper, key custody, constant refused) | vam | Sonnet high — the signature format, canonical form and node key custody are what every receipt's trust rests on, and the wave replaces a public key on the guest-facing capability-token path | 120 min | 0 | [x] | — |
| 2 | `RR-1.5`, `RR-1.6` | — (existing approval, always-allow and gate scenarios stay green) | vam | Sonnet medium — new tables and migrations, a write beside every PEP decision that must not change one, a CLI header | 120 min | 1 | [x] | — |
| 3 | `RR-1.7` | receipt-shows-denial-before-approval, receipt-names-who-decided-and-from-where, receipt-separates-host-facts-from-model-text, receipt-names-the-isolation-the-run-had (lane run 2) | vam | Sonnet medium — new API; issuance on every terminal path and a startup reconcile | 120 min | 2 | [x] | — |
| 4 | `RR-1.8`, `RR-1.9`, `RR-1.10`, `RR-1.11` | receipt-verifies-offline-and-fails-on-tamper, receipt-states-effects-not-journaled-before-journal | vam | Sonnet medium — the offline verifier other people run, across vam-cli, vam-telemetry and the pack | 90 min | 3 | [x] | — |

Branches `e12-1/wave-N`. Wave 1 is the scaffold later waves share (proto types,
signer). The VAM app: nothing in Phase 1 (the receipt view is Phase 2); the guest
(`vam-runtime`) does not change, so no rootfs rebuild. Firecracker lane: two
planned runs (Wave 0 red, Wave 3 green); see open question 12.

**Open questions** (each has a default; the owner approves or changes them)

1. **Schema (design Q1).** **Default:** proto messages in
   `proto/vam/v1/receipt.proto`, proto JSON field names (camelCase, as
   `RunStatus`); the signed bytes are the canonical form of the emitted JSON
   (sorted keys, compact, integers only), and verify works on JSON, never via
   proto.
2. **Tool arguments and results (design Q2).** **Default (owner):** never raw;
   args digest and side-effect class only. The `path` that `policy.decided`
   carries is not copied.
3. **Key custody and rotation (design Q3).** **Default (owner):** receipts carry
   key id and public key; old receipts stay valid after rotation. Phase 1
   minimum: the key file in the state dir, retired public keys from
   `signing/retired/` published by `GET /v1/node` and `vam node keys`; rotation
   is "move the file, restart" (a rotate command is backlog). Node id stays
   `local` (as `/v1/node` today); a node name is backlog.
4. **Partial receipts (design Q4).** **Default (owner):** on demand only,
   backlog. The table keeps a `generation` column; Phase 1 always writes 1.
5. **Cost (design Q5).** **Default (owner):** tokens only, no estimate. The
   attestation keeps its `spend_micro`; the receipt does not print it.
6. **When it is made.** **Default (owner):** once per run at a terminal state,
   stored, and served by GET and `show`; never built on request. Recovery: a
   startup reconcile (and a GET on a terminal run that somehow has none) issues
   the one receipt through the same function. GET on a live or parked run: 409.
7. **Storage and retention.** **Default (owner):** `run_receipts` in vamd's
   SQLite next to the run, insert-only. No run deletion exists today, so the row
   lives as long as the run row; whoever adds run deletion deletes the receipt in
   the same transaction. Export copies it into the pack.
8. **Verify trust.** **Default:** an unpinned verify checks the embedded public
   key, prints the key id and "key not pinned", and exits 0;
   `--trusted-keys <file>` turns a key mismatch into a failure. (Unpinned only
   proves integrity; the team checklist pins.)
9. **Surface.** **Default:** an `X-Vam-Surface` hint from the authenticated
   client (`cli`, `app`), recorded as declared; anything else is `api`. The CLI
   sends it in Phase 1; the VAM app adds it with its receipt view in Phase 2, so app
   decisions read `api` until then. Slack comes from the notify broker (Phase 2).
10. **The constant key (finding).** **Default:** fixed in Wave 1 although it
    reaches beyond receipts: a per-session key derived from the node secret for
    capability tokens and the audit ledger; the node key for the attestation and
    the receipt. **Decided (owner, 2026-10-26): option A**, in Wave 1, released to
    `main` with E12; no separate hotfix (nothing runs in parallel).
11. **Isolation and image facts.** **Default:** recorded per run and lifecycle
    generation at session start (one line when all generations match); only what
    `/v1/node` reports plus kernel and rootfs digests in VM mode. The design
    sample's "egress via host proxy, no secrets in guest" is printed only once the
    node reports those facts (isolation-truth), never from assumptions.
12. **Firecracker lane budget.** The path filter runs the lane on all five PRs.
    **Decided (owner, 2026-10-26): mandatory.** Save lane minutes on every PR
    that does not need the lane. Wave 0 adds a `skip-vm-lane` label check to the `changes` job;
    a PR that should skip the lane is opened as a **draft** (draft PRs run no CI),
    labelled, then marked ready, so the first run already sees the label.
    the Wave 1, 2 and 4 PRs carry the label, so the lane runs on Wave 0 (red) and
    Wave 3 (green). A third run (a Wave 3 fix) is asked first. Alternative: accept
    the automatic runs.
13. **`args_within` naming the gate.** **Default:** unit tests in `RR-1.7`; no
    scenario in Phase 1 (`decide-gate` sends an option only); the B1 `close-prep`
    task covers it end to end.
14. **Model and endpoint.** **Default:** the set the broker actually served
    (model, provider, endpoint host, calls) from the inference callback; the host
    only, never a path or query.
15. **`vam evidence receipt`.** **Default:** an alias of `vam receipt show` with a
    one-line notice, removed after 0.1.0.

**Phase close:** all eight scenarios `active` (the six new) or unchanged and green
in CI; `receipt-names-the-isolation-the-run-had` green on the lane (run 2);
Benchmark delta recorded (P2 line before: not measurable; after: 100% and verify
passes, on the mock); release on the owner's word; team checklist out.

**Team checklist — Phase 1**

Setup (ops, once): deploy vamd from `main` after the release (or `feat/run-receipt`
at); no guest rootfs or app change. On first start vamd creates
`<state dir>/signing/` (0700, key files 0600); back it up with the database. An agent whose
`write_file` asks, and a playbook with a guard on `write_file` until its gate
`confirm` says `go` (the `playbook-with-guard-and-gate` world piece). `$VAMD`,
`$KEY` set.

Steps:
1. `curl -H "Authorization: Bearer $KEY" $VAMD/v1/node`. **See:** `signingKeys`
   with one current key id; the isolation summary.
2. `vam run <agent> --playbook <urn>`. **See:** the first `write_file` refused
   (guard), the run parked at the gate.
3. `vam approve ls`; `vam approve decide <gate id> --decision submitted --response '{"option":"go"}'`,
   then `vam approve decide <ask id> --decision approved`. **See:** the run succeeds.
4. `vam receipt show <run>`. **See:** sections in design order; the DENIED line
   (guard, `confirm:go`) before the ASK line (agent rule, approved once); the gate
   decided by you, from `cli`, with the time; isolation as the node reports it.
5. Same receipt. **See:** "effects not journaled on this version" (no zeros); the
   agent summary only under "written by the model, not verified by the host"; no
   cost figure.
6. `curl .../v1/runs/<a parked run>/receipt`. **See:** 409.
7. `vam receipt show <run> --json > r.json; vam node keys > keys.json`; stop vamd;
   `vam receipt verify r.json --trusted-keys keys.json`. **See:** OK, the key id
   of step 1.
8. Change one character inside `receipt` in `r.json`; verify again. **See:** fails,
   naming the signature.
9. Start vamd; `vam evidence export <run> --out pack && vam evidence verify pack`.
   **See:** OK; `pack/receipt.json` present.
10. Restart vamd; `vam receipt show <run> --json`. **See:** byte-identical to
    `r.json` (immutable).

Report: "Phase 1 pass", or the step number with the error text.

### Phase 2 — Surfaces

**Goal:** the same receipt everywhere.  
**Depends on:** Phase 1, E5 slice  
**Plan:** not drafted  
**Exit:** Scenario: a gate decided from the notify surface shows that surface in the receipt.

- [ ] App receipt view (built-in jig once E10 Phase 3 lands)
- [ ] Slack short form and gate decisions from Slack: built in the `notifications` tracker, slice 2 (NT-2.x); this phase's exit is met by `gate-decided-from-slack-names-slack-on-the-receipt` (owner, 2026-11-02; `docs/src/content/docs/orchestrator/notifications.md`)

**Coordination point with notifications (2026-11-02).** Phase 2's Slack items
(Slack short form, gate decisions from Slack with surface `slack`) now live in
`.cursor/workstreams/notifications.md` (slice 2, NT-2.x; base
`feat/notifications`). Notifications slice 1 adds one call in `issue_receipt`
(the `run.finished` record, after `insert_run_receipt`) and in
`issue_missing_receipts`; it does not touch the receipt builder, which stays this
lane's (Phase 3). This phase keeps the app receipt view.

**Coordination point with E5 (work-outside-chat, 2026-10-29).** A run a broker
trigger started carries its trigger on the receipt. The emitted receipt JSON is
flat today (`requestedBy`, `requestSurface` strings), so E5 Phase 1 wave 9
populates a flat key `requestTrigger {brokerId, grantId, triggerRef, fireAtMs,
fireKind}` beside them, and `requestSurface` is the manifest's
`run_request_permission.surface`. `receipt.proto` stays this lane's: when this
lane adds `ReceiptRequester.trigger = 3` / `ReceiptTrigger`, it maps to that
key. E5's scenario `trigger-starts-a-run-as-the-grant-owner` reads
`receipt.requestTrigger.*`. Tracker: `work-outside-chat.md` Phase 1.

### Phase 3 — Effects and outcomes (released to `main` 2026-11-10 checklist out)

`Decide: NOW — proof 3 (the U5 must-be-zero line reads "from the receipt's effects section", benchmark §3; launch row 10 unblocks row 12), cost 2 (one phase, one additive receipt field, one new read API)`

**Goal:** receipts count effects and feed outcomes.  
**Depends on:** Phase 1 (on `main`), E10 Phase 4 (the effect journal; on this base
since the `main` merge).  
**Plan:** breakdown drafted 2026-11-09 (phase-breakdown agent, Opus 5.5 high),
approved by the owner 2026-11-09; built and closed in base 2026-11-10. High-level only: each wave session plans its wave first.  
**Repos:** vam only. The agent page is the E4 slice (launch row 12), not this phase.  
**Exit:** the kill-mid-task row ends with a receipt reporting 0 duplicates and 0
unknown (on the mock, process mode: `receipt-counts-effects-after-a-crash`, and the
bench row's receipt cross-check on `task-killed-mid-effect-finishes-once`); the
aggregate per agent version is served by the API and `vam agent stats`, ready for
the agent page (E4). Wording change to the old exit ("the agent page shows the
aggregate"): see open question 16.

- [x] Effects section from the effect journal (retires the "not journaled" line)
  **Coordination point with E10 Phase 4 (2026-11-01):** E10 provides the journal (`effect_journal`, read through `effect_store::effects_for_run`) and does not edit `receipt.rs`; this lane owns the receipt field (`agent-runtime.md` Phase 4, Q16).
- [x] Receipts aggregated per agent version (E4 success, cost, pass^k)
- [x] Release diff comparing receipts of the previous version

**Breakdown** (2026-11-09, phase-breakdown agent; playbook §3 step 2, §11, §12).
Evaluated against `feat/run-receipt` (level with `main`:
E10 Phase 4, E12 Phase 1, notifications slice 1, connectors Phase 1).

**Code map** (facts the wave sessions build on):

- **The receipt today.** `EFFECTS_STATEMENT` (`receipt.rs:22`) is written
  unconditionally as `{journaled: false, statement}` (`receipt.rs:908-911`), tested
  by `plain_chat_has_no_procedure_and_effects_are_not_journaled` (`:1308`). The
  builder is a pure function of `Input` (`:255`), filled by `gather` (`:335`);
  `issue_receipt` (`:928`) signs once at a terminal state, insert-only
  `run_receipts` (`:43`, triggers forbid update and delete); `GENERATION` is always 1
  (`:24`). Built as `serde_json::Value` because prost JSON drops `false` and zero
  (`:10-12`); integers only (canonical form, `vam-spec/src/receipt.rs:46`).
  Verify works on the JSON (`vam-security/src/receipt.rs:174`), never through
  proto, so new fields do not break an old verifier and old receipts keep
  verifying: the stored bytes are never rebuilt. `RECEIPT_VERSION` is
  `vam.receipt.v1` (`vam-spec/src/receipt.rs:23`). Proto: `ReceiptEffects
  {journaled = 1, statement = 2}` (`receipt.proto:162-166`); the emitted
  `metering` object is `ReceiptSpend` in proto (`:168`). CLI renderer: `## Effects`
  prints only `statement` (`vam-cli/src/cmd/receipt.rs:438-443`); golden fixtures
  `vam-cli/tests/fixtures/receipt_full.{json,md}`.
- **The journal (E10's, read only).** `effect_journal` (`effect_store.rs:74-95`),
  one row per (run, call id); `EffectState` `intended|sent|confirmed|failed|unknown`
  (`:23`), `EffectClass` `read|idempotent|non_idempotent` (`effect_class.rs:11`);
  columns include `attempts`, `idempotency_key`, `args_digest`, `result_digest`,
  `detail`, `reconciled_by`, `parent_call_id`, `generation_sent`, `created_at` (ms).
  `effects_for_run(run) -> {rows, counts}` (`:619`), oldest first; counts are per
  state only. Reads are journaled too (the verify read is the read-class row
  `<call id>/verify`, `effect_reconcile.rs:348`).
- **What `reconciled_by` holds.** Only `verify` is ever written to a row:
  `effect_mark_confirmed(.., Some("verify"))` when the check finds the effect, and
  `effect_note_checked(.., "verify")` when it finds nothing (the row stays
  `unknown`, `effect_reconcile.rs:262-286`, `effect_store.rs:442`). The host re-send
  of a read or idempotent call (`by: host`) and the person allowing an identical
  call (`by: person`) are `effect.reconciled` **events** only
  (`effect_reconcile.rs:38-42`, `:238`, `:180`); the row's doc comment
  (`effect_store.rs:140`, "verify, model or person") is wider than the code
  (finding for E10, not edited here).
- **What a "duplicate" can be in the journal.** The key is
  `HMAC(K_effect, run, call id, args digest)` (`effect_key.rs:3-4`): one key per
  decided call. (1) The host re-sending one decided call shows as `attempts > 1`
  on its row; `effect_resend` allows that only for read and idempotent rows
  (`effect_store.rs:416-438`), so a non-idempotent row with `attempts > 1` should be
  impossible. (2) An identical call under a **new** call id (same tool and args
  digest) after an earlier one is `unknown` or confirmed only by a check asks the
  person first (`security.rs:2111-2148`, `effect_store.rs:460`); allowed, it is a
  second row with its own key. (3) A guest that sends no call id is journaled under
  `legacy-<tool>-<digest8>-<n>` (`effect_path.rs:183-196`), not stable across a
  resume. The journal cannot see traffic a guest process sends directly (design
  §V.1) or a vendor applying one request twice: the world's ledger stays the
  independent counter (E10 Q9).
- **Terminal-time states.** `sent` is written immediately before the send and
  `intended` before any send (owner decision R1, fail closed), so at a terminal
  state an `intended` row never left the host, and a `sent` row left with no answer
  recorded. A denial at decide is `failed` with `detail: denied` and `attempts: 0`
  (`security.rs:1771`).
- **Cost.** Receipts carry tokens only (Phase 1 Q5): `model[]` with `name`,
  `provider`, `promptTokens`, `completionTokens` per model (`receipt.rs:853-872`),
  `metering` totals (`:894-906`). No cache token split. `run_metering.spend_micro`
  (`workflow.rs:537`) is the broker's estimate at run time (catalog price with cache
  prices, `vam-inference/src/catalog.rs:71`); it is on the attestation, not the
  receipt. Prices: the compiled-in `ModelCatalog` (`catalog.rs:42-52`, version
  `catalog_version()` `:385`); `mock`, `vercel` and `openrouter` are transport, not
  makers (`:113`); `gateway_entry`/`gateway_price_source` (`:447`, `:470`) give a
  gateway id the maker's list price. The benchmark's rule (maker's list price,
  tokens only with no price, cache tokens at the input price) is implemented in
  `vam-e2e/src/usage.rs` (`bench_entry` `:176`, `price_source` `:196`,
  `estimate_micro` `:215`), a test crate the orchestrator cannot depend on.
- **Run to agent version.** `runs.agent_urn` is written at create
  (`catalog.rs:1859`; no index on it, `owner.rs:86` indexes only the owner); the
  version and digest are in `RunSpec.agent_ref` on `runs.spec_json` and are already
  on the signed receipt as `pins.agent {urn, version, digest}` (`receipt.rs:345`,
  `:828`). A draft run pins `X.Y.Z-draft` and the draft's digest at that moment
  (`catalog_resolve.rs:41`, `:185-189`); a draft keeps changing, so only the digest
  names its content.
- **Previous version.** `query_catalog_versions` lists the draft first, then
  releases newest first (`catalog.rs:546-586`); a release must be newer than the
  latest and differ from it (`catalog_publish.rs:476-490`). The release stores the
  **frozen** content's digest; the draft digest it was frozen from
  (`FrozenRelease.draft_digest`, `catalog_publish.rs:421`) is checked and dropped,
  not stored, so after a release nothing links draft runs to it. `POST
  /v1/agents/{urn}/release` (`api/catalog_release.rs:33`) returns the spec only;
  GETs under `/v1/agents/*` dispatch in `get_agent_wildcard` (`api/mod.rs:589`,
  `versions` at `:600`, suggestion at `:603`). CLI: `vam agent release` asks the
  suggestion, releases, prints the JSON (`vam-cli/src/cmd/catalog.rs:289-306`);
  `AgentCmd` (`:24`).
- **Bench.** Rows read receipts for the harness (`vam-e2e/src/bench.rs:242-305`)
  and carry `duplicateEffects`/`lostTasks` from the ledger and the run state
  (`:342-343`), `costPerSuccess`/`costMissing`/`priceSource` (`:321-335`).
- **Runner.** Receipt steps (`expect: {receipt}`, `verify-receipt`,
  `engine.rs:1557`, dispatch `:2150`); world agent pieces release as 1.0.0
  (`engine.rs:578-592`); `chat {draft: true|false}` pins the draft or 1.0.0
  (`:1035-1041`); `Status::Retired` exists (`scenario.rs:92`). No step edits a
  draft or releases an agent; there is no generic GET expectation.

**Scenario delta**

| Scenario | Change | Layer | Given | Why |
|---|---|---|---|---|
| receipt-lists-each-external-effect-once | new | process | org-admin, agent-with-effect-tools, effects-test-broker | the effects section: one line per host-mediated effect with class, state and key digest; reads counted, in-VM file writes not listed; replaces the retired line |
| receipt-counts-effects-after-a-crash | new | process | org-admin, agent-with-effect-tools, effects-test-broker | **the phase's exit**: guest killed mid-create, task resumes; receipt says 0 duplicates, 0 unknown; the ledger says 1 |
| receipt-shows-an-effect-settled-by-a-check | new | process | org-admin, agent-with-effect-tools, effects-test-broker | the state "reconciled": vamd restart leaves a create unknown, the declared check finds it |
| receipt-counts-an-unknown-effect-it-could-not-check | new | process | org-admin, agent-with-effect-tools, effects-test-broker | unknowns are counted, never hidden: no check exists, the retry is denied, the receipt says 1 unknown |
| agent-version-stats-sum-its-receipts | new | process | org-admin, agent-with-effect-tools, effects-test-broker | the aggregate per version: runs, successes, pass^k, tokens, effects; versions kept apart |
| release-shows-the-diff-against-the-previous-version | new | process | org-admin, agent-with-effect-tools, effects-test-broker | the release diff: before release (draft runs vs 1.0.0) and after (1.1.0 vs 1.0.0) |
| old-receipt-still-verifies | new, lands `active` (guard; open question 14) | process | org-admin | a receipt saved before this phase (`effects.journaled: false`) still verifies offline, pinned, and fails on tamper |
| receipt-states-effects-not-journaled-before-journal | **retired** (spec change for the owner): its `then` stops being true on purpose; its two halves move to `receipt-lists-each-external-effect-once` (the section) and `old-receipt-still-verifies` (the old form) | — | — | locked decision 5's "until the journal exists" ends; design §4 named it "retired by Phase 3" |

Existing scenarios whose `then` does not change and that the build waves re-run
because their guards are touched (`receipt.rs`, `api/mod.rs`, `host_tools.rs`):
the five Phase 1 receipt scenarios, `harness-switches-print-on-the-receipt`,
`named-profile-prints-on-the-receipt-with-its-own-digest`,
`trigger-starts-a-run-as-the-grant-owner`, `background-run-end-notifies-and-chat-turn-does-not`,
`gate-decided-from-slack-names-slack-on-the-receipt`, the four effect scenarios and
`task-killed-mid-effect-finishes-once`. **`task-killed-mid-effect-finishes-once` is a
bench task and is not edited** (benchmark locked decision 3: a phase adds tasks,
never edits a `then`); the exit is proven by the new crash scenario and by the bench
row's receipt cross-check (`RR-3.7`).

What each new scenario does, and where it is red today (class `product` unless
said):

- **receipt-lists-each-external-effect-once.** Mock `e2e-step:make`:
  `echo_create_ticket {title: Q4 plan}`, then `echo_find_ticket {title: Q4 plan}`,
  then `write_file note.txt`, then text. `then`: `run: succeeded`; `receipt.effects.journaled:
  true`, `receipt.effects.statement` absent, `receipt.effects.scope` present,
  `receipt.effects.duplicates: 0`, `receipt.effects.unknown: 0`,
  `receipt.effects.counts.reads: 1`, `lines.0.tool: echo_create_ticket`,
  `lines.0.class: non_idempotent`, `lines.0.state: confirmed`, `lines.0.attempts: 1`,
  `lines.0.keyDigest` and `lines.0.argsDigest` present, `lines.1` absent (the read is
  counted, the file write is a turn commit, not a journal row); `verify-receipt: {}`.
  **Red at the first receipt `then`** (`journaled: false`).
- **receipt-counts-effects-after-a-crash.** The `when` of
  `effect-is-not-repeated-after-the-guest-crashes` (create with `holdMs: 4000`,
  `expect {ledger count: 1}`, `kill-guest`). `then`: `run: succeeded`, `ledger
  {tool: echo_create_ticket, count: 1}`, `receipt.effects.duplicates: 0`,
  `receipt.effects.unknown: 0`, `receipt.effects.counts.identicalRepeats: 0`,
  `lines.0 {tool: echo_create_ticket, state: confirmed, attempts: 1}`, `lines.1`
  absent; `verify-receipt: {}`. **Red at `then.2`**.
- **receipt-shows-an-effect-settled-by-a-check.** The `when` of
  `unknown-effect-is-reconciled-by-a-verify-read` (restart during the hold). `then`:
  `run: succeeded`, ledger create 1 and find 1, `lines.0 {tool: echo_create_ticket,
  state: reconciled, checkedBy: verify, attempts: 1}`, `receipt.effects.unknown: 0`,
  `receipt.effects.duplicates: 0`, `receipt.effects.counts.reconciled: 1`. **Red at
  the first receipt `then`**.
- **receipt-counts-an-unknown-effect-it-could-not-check.** The `when` of
  `unknown-effect-without-a-verify-read-is-left-to-the-model` (restart during
  `echo_create_note`, the blind retry asks, `deny: latest`). `then`: `run:
  succeeded`, ledger note 1, `receipt.effects.unknown: 1`, `lines.0 {tool:
  echo_create_note, state: unknown}`, `lines.0.checkedBy` absent, `lines.1 {tool:
  echo_create_note, state: failed, attempts: 0}` (the denied retry never left),
  `receipt.effects.duplicates: 0`. **Red at the first receipt `then`**.
- **agent-version-stats-sum-its-receipts.** Two runs on the release (`chat
  {draft: false}`): `e2e-step:make` (one create) and, in a new thread,
  `e2e-step:hello` (text only); one run on the draft (`chat {draft: true}`) whose
  mock rule `{step: broken}` replies `fail: {status: 503}` (no fallback, so the run
  ends FAILED; wave 0 confirms the state, else it uses `stop` and the draft line
  asserts `cancelled: 1`). `then`: `agent-stats {piece: agent-with-effect-tools,
  version: 1.0.0, k: 2, fields: {runs: 2, successes: 2, failed: 0, passK.k: 2,
  passK.value: 1, approvalsPerRun: 0, tokensPerSuccess: present,
  costPerSuccessMicroUsd: absent, costMissing.0: mock-model,
  effects.journaledRuns: 2, effects.duplicates: 0, effects.unknown: 0,
  effects.confirmed: 1}}`; `agent-stats {version: draft, fields: {runs: 1,
  successes: 0, failed: 1}}`. **Red at `then.0`** (404: no route).
- **release-shows-the-diff-against-the-previous-version.** `chat {draft: false}`
  `e2e-step:make`; `edit-agent-draft {piece, set: {description: second edition}}`;
  `chat {draft: true, thread: new}` `e2e-step:make`; `expect {release-diff {piece,
  version: draft, fields: {base.version: 1.0.0, base.runs: 1, base.successes: 1,
  candidate.version: draft, candidate.runs: 1, candidate.successes: 1,
  candidate.effects.duplicates: 0}}}` (the diff at release time); `release-agent
  {piece, version: 1.1.0}`. `then`: `release-diff {version: 1.1.0, fields:
  {base.version: 1.0.0, base.runs: 1, candidate.version: 1.1.0, candidate.runs: 0,
  candidate.passK.value: absent}}`. **Red at the `when` diff `expect`** (404).
- **old-receipt-still-verifies.** A Phase 1 receipt and its node key list, saved by
  wave 0 from a run on the current base, as fixtures under
  `e2e/world/fixtures/receipts/` (public key only). `when`/`then`:
  `verify-receipt {file, trustedKeys}` passes; `verify-receipt {file, tamper: true}`
  fails naming the signature. Passes on the current base by design: it is the guard
  that the canonical form and signature domain do not change in this phase.

**Runner additions** (Wave 0; own module `crates/vam-e2e/src/outcomes.rs`,
registration lines only in `scenario.rs`, `engine.rs`, `check.rs`; `e2e/README.md`
section "Outcomes per agent version"):

- `expect: {agent-stats: {piece, version?, k?, fields}}` reads `GET
  /v1/agents/{urn}/outcomes[?version=&k=]` (`version`: `X.Y.Z` or `draft`); dotted
  camelCase `fields` with `present`/`absent`; a non-2xx is `product`.
- `expect: {release-diff: {piece, version, k?, fields}}` reads `GET
  /v1/agents/{urn}/outcomes/diff?version=`.
- `edit-agent-draft: {piece, set: {<dotted spec path>: value}}` reads the piece's
  draft, sets the paths, `PUT /v1/agents` (harness failure on non-2xx).
- `release-agent: {piece, version, refused?}` posts the release.
- `verify-receipt` gains `file` (a fixture path relative to the repo) and
  `trustedKeys` (passed as `--trusted-keys`).
- Numeric `fields` compare by value (`1` matches `1.0`), if not already so.

**Benchmark delta** (playbook §11)

- **Lines moved.** P2 receipt completeness (`benchmark.md` §1): the receipt now
  states its effects, with a count of unknowns; "verify passes" now also covers a
  receipt written before this phase. P13 kill-mid-task duplicates and lost tasks
  (§1, §3 "from the receipt's effects section", must be 0): the receipt becomes the
  declared source, the ledger the independent cross-check (E10 Q9). P5 reliability
  per agent version (pass^k, tokens and cost per success, approvals per run,
  duplicate effects must be 0): now served by the product per agent version (the
  bench table stays per task and model).
- **Tasks added:** none. Bench rows gain additive `receiptDuplicateEffects`,
  `receiptUnknownEffects` and `effectsMismatch` (`RR-3.7`); a mismatch between the
  receipt's count and the ledger's makes the row red and is recorded as a finding.
- **Baseline on the current base, by construction (no run needed):** every
  receipt says "effects not journaled on this version" (`receipt.rs:908-911`;
  `receipt-states-effects-not-journaled-before-journal` green); bench
  `duplicateEffects` 0 and `lostTasks` 0 on the mock come from the ledger and the
  run state only (`task-killed-mid-effect-finishes-once`, smoke); per-version
  outcomes are not measurable (no API; nothing groups receipts by version).
- **After (phase close, mock):** receipt duplicates 0 and unknown 0 on the crash
  scenario and on the bench task's mock row, equal to the ledger; per-version runs,
  successes, pass^k, tokens per success and duplicates read from the API; cost per
  success is tokens only on the mock (unpriced) and priced in unit tests. No paid
  run is proposed (§10): the real-model rows already exist from E10 Phase 4 if the
  owner ran them, and gain the cross-check columns on the next run.

**Items**

- [x] `RR-3.1` — **Runner additions and fixtures (Wave 0).** The runner additions
      above; engine self-tests for each (stats and diff against a canned body,
      draft edit, release, verify of a file with pinned keys, tamper of a file);
      the old-receipt fixture and keys file captured from a run on the current base
      (`receipt-states-effects-not-journaled-before-journal`'s receipt and `vam node
      keys`); `e2e/README.md` section. Files: `vam-e2e/src/{outcomes,scenario,engine,check}.rs`,
      `vam-e2e/tests/engine_selftests.rs`, `e2e/world/fixtures/receipts/`,
      `e2e/README.md`. **Accept:** `cargo test -p vam-e2e`; `cargo run -p vam-e2e
      -- check` clean; `--smoke` still green. **Test:** `cargo test -p vam-e2e`,
      `scripts/ci/local.sh --fast`.
      `Decide: NOW — proof 3 (the exit and the aggregate cannot be shown without them), cost 1 (one wave, tests, no product surface)`
- [x] `RR-3.2` — **Scenario files (Wave 0).** The six new scenarios `pending` with
      `expectFail` at the steps above; `old-receipt-still-verifies` `active`; the
      retirement of `receipt-states-effects-not-journaled-before-journal`
      (`status: retired`, once the owner approves the spec change) and the docs
      page's `proves` list updated to match (the docs lint reads it). The `fields`
      paths written here (`effects.duplicates`, `effects.unknown`, `effects.scope`,
      `effects.counts.*`, `effects.lines[].{tool,adapter,class,state,attempts,
      keyDigest,argsDigest,checkedBy}`, the outcomes and diff paths) are the schema
      `RR-3.3`, `RR-3.5` and `RR-3.6` use; changing one later is a spec change.
      Confirm in a run that each flow reaches its receipt or stats step (a failure
      earlier is a product finding to report, not to paper over), and how the
      `fail: {status: 503}` draft run ends. **Accept:** `scripts/e2e/run.sh
      --scenarios receipt-lists-each-external-effect-once,receipt-counts-effects-after-a-crash,receipt-shows-an-effect-settled-by-a-check,receipt-counts-an-unknown-effect-it-could-not-check,agent-version-stats-sum-its-receipts,release-shows-the-diff-against-the-previous-version`
      each red at its declared step, class `product`; `old-receipt-still-verifies`
      green. **Turns green:** `old-receipt-still-verifies` (guard).
      `Decide: NOW — proof 3 (spec of the exit), cost 0`
- [x] `RR-3.3` — **Effects section from the journal.** In `receipt.rs` (ours):
      `Input` gains the run's `effects_for_run` rows (read only; E10's file is not
      changed) and whether the run is journaled; the builder writes
      `effects {journaled: true, scope, duplicates, unknown, counts {confirmed,
      failed, reconciled, notSent, identicalRepeats, reads}, lines[]}` per open
      questions 1-4: one line per non-read row, oldest first (`seq`, `tool`,
      `adapter`, `class`, `state` `confirmed|failed|unknown|reconciled|notSent`,
      `attempts`, `keyDigest` = `sha256:` of the key, `argsDigest`, `resultDigest`
      when present, `checkedBy` when a check ran, `parentSeq` for a nested step,
      `repeatOf` for an identical repeat, `hostDerivedId: true` for a `legacy-` id,
      `at` in seconds); integers only. "Journaled" is false (the Phase 1 object,
      byte for byte) for a run created before the node first started with this
      version (a one-row marker in `receipt.rs`'s own schema init; open question 5).
      No call id, no `detail` text, no raw argument or result. `receipt.proto`:
      additive `ReceiptEffects` fields 3+ and `ReceiptEffectCounts`,
      `ReceiptEffectLine` (documentation of the JSON, as Phase 1), proto baseline
      updated (additive). `vam receipt show`: the effects section (counts line, then
      one line per effect; the old statement for old receipts); golden fixtures for
      both. Docs: `security/run-receipt.md` (the section, states, what "duplicate"
      and "unknown" mean, what is outside the journal; `proves` list). **Accept:**
      unit tests: a run with no rows says journaled with zeros (never the old line);
      a pre-marker run keeps the Phase 1 object; `sent` at terminal is unknown,
      `intended` is notSent; a non-idempotent `attempts: 2` counts one duplicate; an
      idempotent `attempts: 2` counts none; an identical repeat under a new call id
      counts in `identicalRepeats` with `repeatOf`; reads only counted; verify rows
      named by `checkedBy`; no raw argument (the Phase 1 test extended); an old
      stored receipt (the wave 0 fixture) still verifies with `vam receipt verify`;
      renderer goldens. **Test:** `cargo test -p vam-orchestrator --lib receipt`,
      `cargo test -p vam-cli receipt`, `cargo test -p vam-security receipt`,
      `scripts/ci/local.sh --scenarios receipt-lists-each-external-effect-once,receipt-counts-effects-after-a-crash,receipt-shows-an-effect-settled-by-a-check,receipt-counts-an-unknown-effect-it-could-not-check,old-receipt-still-verifies`
      plus the receipt scenarios listed under the delta. **Turns green:** the four
      receipt effect scenarios.
      `Decide: NOW — proof 3 (P13 must-be-zero line from its declared source; locked decision 5), cost 1 (one wave; the proto field is additive documentation of a JSON object already emitted)`
- [x] `RR-3.4` — **One price rule, shared.** Move the maker-list-price rule
      (`bench_entry`, `price_source`, `estimate_micro`, `vam-e2e/src/usage.rs:176-229`)
      into `vam-inference` (`ModelCatalog::list_price_entry`, `list_price_source`,
      `estimate_list_micro`); `vam-e2e` calls them, no change in bench output.
      **Accept:** existing bench unit tests unchanged and green (`bench.rs`
      price tests, `bench_publish`); vam-inference vectors (maker entry, gateway id at
      the maker's price, mock and unknown unpriced). **Test:** `cargo test -p
      vam-inference catalog`, `cargo test -p vam-e2e`.
      `Decide: NOW — proof 2 (P5/P8 cost per success with the published price rule), cost 1 (a move with tests)`
- [x] `RR-3.5` — **Outcomes per agent version (API).** New
      `vam-orchestrator/src/outcomes.rs` (pure aggregation over parsed receipts) and
      `api/outcomes.rs` (routes; one dispatch line in `get_agent_wildcard`). `GET
      /v1/agents/{urn}/outcomes` lists versions with runs (released by version label,
      newest first, plus `draft` for the caller's current draft digest);
      `?version=X.Y.Z|draft&k=N` returns one: `runs` (SUCCEEDED + FAILED +
      TIMED_OUT), `successes`, `failed`, `timedOut`, and excluded `cancelled`,
      `expired`, `rejected`; `successRate`; `passK {k, value}` (open question 8;
      `value` absent with `note` when runs < k); `tokensPerSuccess`, `turnsPerSuccess`;
      `costPerSuccessMicroUsd`, `priceSource`, `catalogVersion` or `costMissing`
      (open question 10); `approvalsPerRun`; `effects {journaledRuns,
      unjournaledRuns, confirmed, unknown, duplicates, identicalRepeats}` summed over
      journaled receipts only (absent, never 0, when none is journaled). Source: the
      stored signed receipts joined on `runs.agent_urn` (index
      `idx_runs_agent_urn` created in `outcomes.rs`'s schema init), grouped by
      `receipt.pins.agent`; floats allowed here (unsigned). Same auth as
      `versions` (catalog read; draft only for its owner or `users:admin`); no run
      or user ids in the answer. JSON built explicitly (zeros kept);
      `proto/vam/v1/outcomes.proto` documents it; `docs/api/openapi.yaml` stub.
      **Accept:** unit tests: pass^k estimator vectors (`n=3, s=2, k=2` → 1/3;
      `s=n` → 1; `n<k` → absent); denominator excludes cancelled; a pre-journal
      receipt counts for success and tokens but not effects; draft grouped by
      digest (an edited draft starts a new group); cost priced from receipt tokens
      at the maker's list price, absent when any model is unpriced; an `api_tests`
      case for auth (foreign draft 404). **Test:** `cargo test -p vam-orchestrator
      --lib outcomes`, `--test api_tests`, `scripts/ci/local.sh --scenarios
      agent-version-stats-sum-its-receipts`. **Turns green:**
      agent-version-stats-sum-its-receipts.
      `Decide: SLICE — proof 3 (launch row 12, the E4 slice, reads it; P5 line), cost 2 (new API) → slice: one read API over stored receipts, computed on read; a stored index table, time windows and per-model splits backlog, trigger "the outcomes call takes over 200 ms at 10k receipts" or "E4 asks for a window"`
- [x] `RR-3.6` — **Release diff and `vam agent stats`.** `GET
      /v1/agents/{urn}/outcomes/diff?version=V[&k=]` returns `{base, candidate,
      changes}`: `candidate` is V's aggregate (`draft` = runs at the current draft
      digest, the content a release would freeze), `base` is the release before V
      (for `draft`, the latest release) or absent for a first release; `changes` holds
      differences of `successRate`, `passK.value`, `costPerSuccessMicroUsd`,
      `tokensPerSuccess`, `approvalsPerRun` (absent when either side lacks the value)
      and both sides' duplicates. The release endpoint does not change (open question
      12). CLI (own file `vam-cli/src/cmd/outcomes.rs`, one `AgentCmd` variant):
      `vam agent stats <agent> [--version X.Y.Z|draft] [--k N] [--diff] [--json]`;
      `vam agent release` prints the draft-vs-previous diff on stderr before it
      releases (stdout stays the release JSON). Docs: `orchestrator/api.md` (two
      routes), `orchestrator/catalog.md` (stats and the release diff), no new page
      (a new page would need `docs/astro.config.mjs`). **Accept:** unit tests (base
      lookup skips drafts and picks the highest release below V; first release has
      no base); CLI goldens for stats, diff and the release preface; `vam agent
      release --json` output unchanged. **Test:** `cargo test -p vam-orchestrator
      --lib outcomes`, `cargo test -p vam-cli outcomes`, `scripts/ci/local.sh
      --scenarios release-shows-the-diff-against-the-previous-version,agent-version-stats-sum-its-receipts`.
      **Turns green:** release-shows-the-diff-against-the-previous-version.
      `Decide: NOW — proof 2 (P5 "getting better?" per release; feeds E4's release gate), cost 1 (files already touched in the phase; one route over RR-3.5)`
- [x] `RR-3.7` — **Bench cross-check from the receipt.** Bench rows read each
      trial's `receipt.effects.duplicates` and `unknown` (as they read the harness)
      and add `receiptDuplicateEffects`, `receiptUnknownEffects`, `effectsMismatch`
      (ledger extras ≠ receipt duplicates + identical repeats, or any unknown on a
      must-be-zero task); a mismatch makes the row red. Additive fields in the proof
      lane's `bench.rs` (coordination note to the harness tracker in the PR).
      **Accept:** bench unit tests (clean row; receipt 1 vs ledger 0 is red;
      unjournaled receipt reads "not journaled", never 0); `--bench --mock` on
      `task-killed-mid-effect-finishes-once` prints both receipt counts 0.
      **Test:** `cargo test -p vam-e2e bench`, `scripts/ci/local.sh --scenarios
      task-killed-mid-effect-finishes-once`.
      `Decide: NOW — proof 3 (benchmark §3 names the receipt as the source of a must-be-zero line), cost 1`

**Waves** (model: Sonnet low by default, medium second, high rarely with a reason)

| Wave | Items | Turns green | Repo | Model | Budget | Depends on | Actions | Status | PR |
|---|---|---|---|---|---|---|---|---|---|
| 0 | `RR-3.1`, `RR-3.2` (scenarios) | old-receipt-still-verifies (guard); six red, right reason | vam | Sonnet low | 60 min | — | none | [x] (disk 30 GB before) | — |
| 1 | `RR-3.3`, `RR-3.7` | receipt-lists-each-external-effect-once, receipt-counts-effects-after-a-crash, receipt-shows-an-effect-settled-by-a-check, receipt-counts-an-unknown-effect-it-could-not-check | vam | Sonnet medium — the signed host tier: the state mapping and duplicate rule must match the journal exactly, the canonical form must not move (old receipts verify), across vam-orchestrator, proto, vam-cli, vam-e2e and docs | 120 min | 0 | none | [x] (disk 9.1 GB before) | — |
| 2 | `RR-3.4`, `RR-3.5` | agent-version-stats-sum-its-receipts | vam | Sonnet medium — a new API and proto over three crates (vam-inference, vam-e2e, vam-orchestrator), auth on drafts | 120 min | 1 | none | [x] (disk 20 GB before) | — |
| 3 | `RR-3.6` | release-shows-the-diff-against-the-previous-version | vam | Sonnet low — one route over wave 2's aggregate, CLI flags and docs, following existing patterns | 90 min | 2 | none | [x] (disk 29 GB before) | — |

Branches `rr-3/wave-N` against `feat/run-receipt`; PR title `RR-3 W<n>: <summary>`,
body lists the items and says "Do not merge". Every PR opens as a **draft** and
stays one until `scripts/ci/local.sh --full` passes in the session (summary under
"Local CI"); `--fast` while building. `actions: none` on every wave: no wave needs a
microVM (all scenarios are process mode), so the last commit carries `[skip ci]`
(the paths select the lane: `crates/vam-orchestrator/`, `e2e/`, `proto/`); the
release PR into `main` runs full Actions once. The guest (`vam-runtime`) does not
change: no rootfs rebuild. The VAM app: nothing (the agent page is E4). Record `df -h
/home/user` in the wave row before each build.

**Open questions** (each has a default; the owner approves or changes them)

1. **What "duplicate" means on the receipt.** **Default:** two numbers, because
   the journal can honestly assert only the first. `duplicates` = sends beyond the
   first of one decided call of class non-idempotent (`attempts - 1` summed; 0 by
   construction, now observable). `counts.identicalRepeats` = non-idempotent rows
   sent with the same tool and args digest as an earlier sent, not failed row of
   the run (the model asked again, or the person allowed it after an unknown); each
   such line names `repeatOf`. Idempotent re-sends under one key are not
   duplicates. The journal cannot see direct guest egress or a vendor applying a
   request twice; the `scope` sentence says so, and the bench compares with the
   world's ledger. Alternative: one `duplicates` that adds both.
2. **Which rows are lines.** **Default:** every non-read row (idempotent and
   non-idempotent) is a line; reads (the verify read included) are counted only, as
   Phase 1 summarises read-only calls. Alternative: non-idempotent only (design
   decision 5's wording).
3. **States at a terminal run.** **Default:** `confirmed`; `reconciled` =
   confirmed by a check (`reconciled_by = verify`); `failed` (a denial shows
   `attempts: 0`); `unknown` = `unknown`, and `sent` with no answer recorded (counted
   in `unknown`); `notSent` = `intended` (never left the host). A checked-but-not-
   found row stays `unknown` with `checkedBy: verify`.
4. **Line fields.** **Default:** no call id (a provider- or guest-made string), no
   `detail` (may carry vendor text), no arguments or results (receipt Q2):
   `keyDigest` (sha256 of the key, so a vendor log can be matched without printing
   the key), `argsDigest`, `resultDigest`, `adapter`, `attempts`, `at`.
5. **Runs from before the journal.** **Default:** `receipt.rs` records, once, the
   time the node first started with this version; a run created before it keeps
   the Phase 1 object (`journaled: false`, statement), conservative (it never claims
   a journal it cannot show). Stored receipts are never rebuilt (decision 8).
6. **Receipt version.** **Default:** stays `vam.receipt.v1`; the change is
   additive and the signature domain and canonical form are unchanged.
7. **Aggregation source.** **Default:** the stored signed receipts (what the
   E4 slice is specified to read, launch row 12), joined on `runs.agent_urn` with a
   new index, computed on each read. Facts tables are rejected: they can change
   after the receipt (metering upserts) and old runs would need a second path.
8. **pass^k for production runs.** **Default:** the unbiased estimator the
   benchmark's pass^k approximates, `C(successes, k) / C(runs, k)`: the chance that
   k runs drawn from this version's runs all succeeded; k default 5 (the benchmark's
   default), `?k=` to change; absent with a note when runs < k. Order-free and uses
   every run. Alternative: 1 when the last k runs all succeeded, else 0.
9. **Success and the denominator.** **Default:** success = `SUCCEEDED`; runs =
   `SUCCEEDED + FAILED + TIMED_OUT`. `CANCELLED` (a person stopped it), `EXPIRED` (no
   one answered, or a fire past its deadline) and `REJECTED` (refused at admission)
   are listed but not counted against the agent.
10. **Cost per success.** **Default:** priced on read from each receipt's
    per-model tokens at the model maker's list price in the compiled-in catalog
    (benchmark §3; a gateway is transport), prompt tokens at the input price (the
    receipt has no cache split); integer micro-USD with `priceSource` and
    `catalogVersion`. If any model in the group has no price (the mock), the cost is
    absent and `costMissing` lists the models; tokens per success always shown. The
    receipt itself stays tokens only (Phase 1 Q5 unchanged); `spend_micro` is not
    used (a different price source).
11. **Draft runs.** **Default:** grouped by the draft digest; the `draft` group is
    the caller's current draft digest (the content a release would freeze). Older
    draft digests are not listed.
12. **Release diff placement.** **Default:** a separate GET; the release endpoint
    and its response do not change; `vam agent release` prints the diff on stderr
    before releasing. No snapshot is stored at release (the draft digest a release
    froze is not persisted, `catalog_publish.rs:421`); storing a release-time diff
    is backlog, trigger "E4 needs the diff as it was at release".
13. **Who may read outcomes.** **Default:** like `versions`: any catalog reader
    for released versions, the owner or `users:admin` for the draft; counts and
    ratios only, no run or user ids.
14. **`old-receipt-still-verifies` lands `active`.** It is a guard (it must pass
    before and after), an exception to "new scenarios land pending".
    **Default:** keep it as a scenario (the claim users rely on, end to end through
    the built `vam`). Alternative: a `vam-cli` fixture test only.
15. **Bench mismatch.** **Default:** a receipt-vs-ledger mismatch makes the row
    red and is written in the scoreboard as a finding (E10 Q9 cross-check).
16. **Exit wording.** **Default:** "the agent page shows the aggregate" moves to
    the E4 slice (launch row 12; the app is out of this phase per the owner's
    brief); this phase's exit is the API and `vam agent stats`.
17. **E10's Q16 window.** Closed by `RR-3.3`. The doc comment on
    `EffectRow.reconciled_by` (`effect_store.rs:140`) lists values the code never
    writes: reported to E10, not edited here.

**Recommendations for the owner**

- *(required)* Approve the retirement of
  `receipt-states-effects-not-journaled-before-journal` as a spec change: its
  `then` (`journaled: false`) becomes false for every new run by design; the old
  form stays proven by `old-receipt-still-verifies`.
- *(suggested)* Accept the two-number duplicate rule (Q1): one number the host
  guarantees (0 by construction) and one that shows identical repeats it cannot
  judge, so a reader never mistakes "the person allowed it again" for "the host
  repeated it".
- *(suggested)* Keep cost off the signed receipt and price on read (Q10): the
  receipt stays observation only and the price follows the published rule.

**Phase close (2026-11-10, orchestrator).** All seven Phase 3 scenarios `active` and
green on the base (the four effect scenarios, `agent-version-stats-sum-its-receipts`,
`release-shows-the-diff-against-the-previous-version`, the guard
`old-receipt-still-verifies`); `receipt-states-effects-not-journaled-before-journal`
`retired`; the receipt and effect scenarios under the delta still green. Waves:
 (4 PRs, 3 fix cycles). Exit met on the mock: the kill-mid-task
row ends with a receipt reporting 0 duplicates and 0 unknown, equal to the ledger
(`receipt-counts-effects-after-a-crash`; bench `--mock` on
`task-killed-mid-effect-finishes-once`); the aggregate per agent version is served by
the API and `vam agent stats` for the E4 slice.

**Benchmark delta, recorded.** Before: every receipt "effects not journaled on this
version"; bench duplicates and lost tasks from the ledger and run state only;
per-version outcomes not measurable. After (mock, process mode): receipt duplicates 0
and unknown 0 equal to the ledger (`effectsMismatch` false); per-version runs,
successes, pass^k, tokens per success, approvals per run and effects served by
`GET /v1/agents/{urn}/outcomes`; cost per success tokens only on the mock (unpriced),
priced at the maker's list price in unit tests. No paid run (owner's word needed).
Lines not moved: none; real-model rows gain the receipt cross-check columns on their
next run.

**Findings carried out of the phase.**

- Phase 1 gap: `runs.created_at` is NULL on the chat create path, so a chat run's
  receipt has no `startedAt` (Phase 3 uses `run_requests.created_at` for the
  journaled cut-off). Backlog for this lane.
- E10: `host_tools::effect_journal_tests::a_sent_or_unknown_call_answers_the_unknown_outcome_and_is_never_sent`
  failed once under the parallel workspace suite (Sent instead of Unknown); 5/5 alone.
  The `EffectRow.reconciled_by` doc comment lists values the code never writes.
- Harness: `connector-id-cannot-be-taken-by-a-hand-added-server` fails (class harness)
  when run alone: it needs mock providers that a batch starts.
- Sessions: long builds and long smoke runs restart the container (six
  restarts in this phase, one during clippy). Proof was run in resumable steps, smoke
  one scenario per run, builds at 2 jobs. Each wave fills about 20 GB of `target/`;
  `cargo clean` between waves (ladder step 3, three times).

**Team checklist — Phase 3 (effects and outcomes)**

Setup (ops, once): deploy `vamd` from vam `main` at the Phase 3 release commit (vamd
only; no app or rootfs change; no env var). On first start it creates its own
`receipt_effects_marker` row and an index on `runs.agent_urn`; runs created before that
start keep "effects not journaled on this version". Pick an agent that can create
something through a connected tool (for example a Linear issue) and has at least one
released version. Keep `vam node keys > keys.json`.

1. `vam receipt show <a run from before the upgrade>`: the Effects section says
   "effects not journaled on this version"; `vam receipt verify` on its `--json` passes.
2. Run the agent: "create one issue titled QA-<date>, then look it up". `vam receipt
   show <run>`: Effects lists one line (the create: non-idempotent, confirmed, 1
   attempt), 0 duplicates, 0 unknown, the lookup counted as 1 read; no arguments or
   results are printed.
3. `vam receipt show <run> --json > r.json; vam receipt verify r.json --trusted-keys
   keys.json` passes; change one character in `r.json`: verify fails naming the signature.
4. In the tool itself (Linear): exactly one issue QA-<date> exists.
5. `vam agent stats <agent>`: the released version with runs is listed. `vam agent stats
   <agent> --version <X.Y.Z>`: runs, successes, success rate, pass^k (or "fewer runs
   than k"), tokens per success, approvals per run, effects with 0 duplicates.
6. `curl -H "Authorization: Bearer <key>" "$VAM/v1/agents/<urn>/outcomes?version=<X.Y.Z>"`
   returns the same numbers as step 5.
7. Edit the agent's draft (any description change), run it once on the draft, then
   `vam agent stats <agent> --version draft --diff`: the base is the latest release, the
   candidate is the draft with 1 run.
8. `vam agent release <agent>` to the next version: the diff prints first (stderr), then
   the usual release JSON.
9. As a member who cannot see another member's draft-only agent: `curl
   ".../v1/agents/<that urn>/outcomes"` answers 404.

Report: "Phase 3 pass", or the step number with the error text.

## Explicitly later

- Partial receipts at each park (launch-0.1.0 backlog).
- Full Jig rendering of the receipt in Slack (a link covers the demo).

## Progress log

| Date | Phase | What landed (end with the scoreboard: scenarios green, red, fix cycles, wall time) |
|---|---|---|
| 2026-10-25 | — | Tracker opened on `feat/run-receipt` (cut from `main`). Phase 1 `Decide: NOW`. Breakdown agent started |
| 2026-10-25 | 1 | Phase 1 breakdown written: 6 new scenarios (1 `needs: firecracker`), 2 changed (guards only), items `RR-1.1`–`RR-1.9` in 5 waves; finding: production signers use a constant key (Wave 1). Scoreboard: 0 green, 0 red, 0 fix cycles, — |
| 2026-10-26 | 1 | Owner approved the Phase 1 plan and defaults; Q12 (`skip-vm-lane`) mandatory; Q10 (constant key) pending a plain summary. Wave 0 started (plan-only turn) |
| 2026-10-26 | 1 | Owner approved the Wave 0 plan; Q10 decided: constant key fixed in Wave 1 (option A). Wave 0 building |
| 2026-10-26 | 1 | Wave 0 merged : runner `expect: { receipt }`, `{present\|absent}` matchers (all fields families), `verify-receipt { tamper, pack }` via `VAM_E2E_CLI`; 6 receipt scenarios `pending` + world pieces `playbook-with-guard-and-gate`, `agent-with-guarded-write`; 2 scenarios guards only; `skip-vm-lane` label in the `changes` job (draft, label, ready). Verified: smoke 19 pass + 5 red (right reason); the 5 process receipt scenarios red at `then.1`, class `product` (`/receipt` 404: no route yet); CI green; Firecracker lane run 1 of 2 used. Scoreboard: 0 green, 6 red (right reason, by design), 0 fix cycles, about 45 min build |
| 2026-10-26 | 1 | Owner approved the Wave 1 plan: separate `session-master.key`; the `hmac` crate (RustCrypto) instead of a hand-written HMAC; hex strings for keys, signatures and digests. Finding: the guest never verifies signatures (passes no key), so the fix is host-only, no rootfs rebuild; the host also took the key id from the token (`host_tools.rs:3160`). Wave 1 building |
| 2026-10-26 | 1 | Wave 1 merged : receipt proto and canonical JSON (sorted keys, integers only), Ed25519 sign/verify bound to key id and domain `vam.receipt.v1` (golden vector checked independently), node key under `<state>/signing/` (0700/0600, fail closed on loose mode), per-session token keys by HMAC (`hmac` crate) from a separate session master, host no longer takes the key id from the token, `[42u8; 32]` refused, attestation signed by the node key, `signingKeys` on `GET /v1/node`, `vam node keys`. Fix cycle 1: three orchestrator integration suites spawned sessions without node keys (fail-closed worked); test setup now installs ephemeral keys. CI green, lane skipped by `skip-vm-lane`. Scoreboard: 0 green (none planned), 5 process + 1 lane receipt scenarios still red (right reason), 1 fix cycle, about 75 min |
| 2026-10-26 | 1 | Owner approved the Wave 2 plan: decision-write failure logs and counts (never denies; the receipt prints "N decisions not recorded"); `ended_at` = terminal state published (spec line); SQLite trigger sets `ended_at`. Surface is client-asserted (`X-Vam-Surface`), decider identity authenticated. Wave 2 building |
| 2026-10-26 | 1 | Wave 2 merged : `runs.created_at`/`ended_at` (trigger on terminal state; `RunStatus` start/finish real), `run_requests` (requester, surface, `decision_write_failures`), `run_isolation` per session (VM image digests), `run_models` (model, provider, endpoint host, calls, tokens), `run_decisions` from the same target-row read Cedar uses (rule and scope; `via_grant` on the allowed row after an approval), approvals gain `decided_at`/`decided_surface`/`decided_by_user_id`; CLI sends `X-Vam-Surface: cli`; read API `run_facts`, `run_decisions`, `approval_provenance`. Not recorded: denials for uncanonical arguments and a catalog storage error (known limit; not in the counter). VM digest path unit-tested only. CI green, lane skipped. Scoreboard: 0 green (none planned), 6 receipt scenarios red (right reason), 0 fix cycles, about 40 min |
| 2026-10-26 | 1 | Owner approved the Wave 3 plan with one change: GET never issues a receipt (a run is published terminal before finalize reads the head after and signs the attestation, so an inline issue could store an incomplete receipt). finalize issues last; cancel/expiry/resume-failure issue directly; 5 s sweeper and startup reconcile as catch-all; GET on a terminal run without a receipt answers a retryable 409 and the runner retries it within the step budget. 409 for live runs. Wave 3 building; this PR uses Firecracker lane run 2 of 2 |
| 2026-10-26 | 1 | Wave 3 lane result: Firecracker lane run 2 passed all 7 scenarios, `receipt-names-the-isolation-the-run-had` included; the job failed in the VM test step (lane-only `test_vamd_real_firecracker_e2e` spawned a session without node keys). Owner chose option B: one-line test fix pushed with `skip-vm-lane`; the release PR into `main` proves the VM tests. Owner added `RR-1.10` (gate references) and `RR-1.11` (refuse a decision on a finished run) to Wave 4 |
| 2026-10-26 | 1 | Wave 3 merged : `GET /v1/runs/{id}/receipt` (stored bytes; 409 live, 409 "not yet issued", 404 foreign, 500 no keys; GET never issues), receipt builder as JSON from Wave 1–2 facts (ask folded with its `via_grant` allow; "decisions not recorded" line), signed by the node key, insert-only `run_receipts` (abort triggers); issued last in `finalize_run_session` (head after and attestation trajectory digest in place), on cancel and expiry, plus a 5 s sweeper (10 s grace) and startup reconcile; runner retries a 409 within the step budget. Lane run 2: all 7 scenarios passed; VM test step failed on missing node keys in lane-only tests, fixed (test-only, `skip-vm-lane`), proven at the release PR (owner, option B). The fix push's CI never started (no runner assigned, twice: GitHub Actions blocked for the repo); merged under "CI and merging" on local proof: smoke 22 pass + 2 red right reason, receipt 14, route 3, runner 27/34. Scoreboard: 4 green (denial-before-approval, who-decided, separates-host-facts, isolation on the lane), 2 red (right reason, at `then.2`), 1 fix cycle, about 50 min build |
| 2026-10-26 | 1 | Owner approved the Wave 4 plan: (a) optional `attestation.envelope.json` in the pack, verified against the receipt's key (pre-node-key attestations reported unverifiable, not failing); (b) gate references also on allowed `args_within`-constrained calls via a nullable field on `run_decisions` written at the PEP (locked decision 4); (c) `vam evidence receipt` becomes the alias, old renderer removed. Wave 4 building |
| 2026-10-26 | 1 | Wave 4 merged : `vam receipt show` (markdown from stored fields only; model appendix last under its label; `--json` verbatim) and `vam receipt verify` (offline, `--trusted-keys` from `vam node keys`, unpinned warns, failures exit 9 naming the signature); pack gains optional hashed `receipt.json` and `attestation.envelope.json` (receipt signature, run id and trajectory digest checked; envelope must be signed by the receipt's key; `host-key-*` reported unverifiable); `gateStep` on denied, asked and allowed gate-constrained lines (`run_decisions.constraint_rule`); decisions on a finished run refused with 409 in the same transaction; `vam evidence receipt` is an alias; docs page `security/run-receipt.md`. GitHub Actions still assigned no runners; merged on the owner's word on local proof: fmt, receipt_cli 6, telemetry evidence 12, evidence_pack_test 5, api_tests 107, smoke 24/24. Scoreboard: 2 green (verifies-offline-and-fails-on-tamper, states-effects-not-journaled), 0 red, 0 fix cycles, about 55 min |
| 2026-10-26 | 1 | **Phase 1 closed in base.** Six receipt scenarios active and green (5 process in smoke, isolation on the Firecracker lane run 2); 2 changed scenarios green. Phase scoreboard: 6 green, 0 red, 2 fix cycles (W1 test setup, W3 lane-only test setup), 2 lane runs (W0 red by design, W3), 5 PRs. Security finding closed: the constant `[42u8; 32]` signer is gone; guest-forgeable capability tokens no longer verify. Known limits: denials for uncanonical arguments and catalog storage errors are not recorded (not in the not-recorded counter); VM tests (vamd, jailer) with the node-key fix and the full workspace suite on heads are proven at the release PR (Actions blocked). Team checklist written. Release to `main`: owner's word |
| 2026-10-26 | 1 | **Released to `main`** (merge commit) on the owner's word, after full CI green on the release head: checks and release gate, workspace tests, smoke, and the Firecracker lane (VM tests with the node-key fix, jailed variant included). Actions minutes had run out (3,000 included used); owner set a $5 Actions budget. `feat/run-receipt` fast-forwarded to `main`. Team checklist out |
| 2026-11-09 | 3 | Phase 3 breakdown written (phase-breakdown agent, Opus 5.5 high,): 6 new scenarios pending + 1 guard (`old-receipt-still-verifies`, active) + 1 retired (`receipt-states-effects-not-journaled-before-journal`, spec change for the owner); items `RR-3.1`–`RR-3.7` in 4 waves (W0 low, W1 medium, W2 medium, W3 low; `actions: none` each). Findings: only `verify` is ever written to `reconciled_by`; the draft digest a release froze is not stored; the list-price rule lives in a test crate. Owner approval pending. Scoreboard: 0 green, 0 red, 0 fix cycles, — |
| 2026-11-09 | 3 | **Owner approved the Phase 3 breakdown** (relayed by the Master Orchestrator): all three recommendations (retire `receipt-states-effects-not-journaled-before-journal` as an approved spec change; duplicates counted honestly with the bench ledger comparison; "the agent page shows the aggregate" moves to the E4 slice) and all defaults. Standing instruction: this orchestrator plans, builds, reviews and merges every wave into `feat/run-receipt` itself; escalates only the signed canonical form, the version string, old-receipt verification, other lanes' files, locked decisions, other `then` edits, a third fix cycle or paid runs. Wave 0 started. Disk 30 GB free |
| 2026-11-09 | 3 W0 | **Wave 0 merged** (Sonnet 5.5 low). Runner module `crates/vam-e2e/src/outcomes.rs` (`agent-stats`, `release-diff`, `edit-agent-draft`, `release-agent`, `verify-receipt {file, trustedKeys}`; numeric `fields` compare by value in `value_matches`, all families); 76 engine self-tests; old-receipt fixture from a real base run (public key only). Six scenarios `pending`, each red at its declared step, class `product` (lists-each `then.1`, crash `then.2`, settled `then.3`, unknown `then.2`, stats `then.0` 404, diff `when.3` 404); `old-receipt-still-verifies` active and green; `receipt-states-effects-not-journaled-before-journal` retired (owner-approved spec change; `then` untouched), docs `proves` and `docs-features.yaml` updated. Mock note: `afterTool` rules never match broker (hsp) tool results; `toolName` rules used instead. The `fail: {status: 503}` draft run ends FAILED. Finding: a long `--out` path breaks 28 scenarios (HSP gateway socket path over 107 bytes); use the default or a short one. Orchestrator re-run at the head: `--full` fmt, protos, manifests, check, both clippy and the workspace suite **all passed as root** (first time, with); the container restarted during smoke (26 passed, none failed, before the restart; third restart this phase, cause not visible inside). Smoke proof from the wave at the pre-merge head: 117 pass, 6 red right reason, 0 fail; the merge since adds only's test change. Merged on that evidence. Cross-cutting: into `main` (owner asked directly): `hsp_uds` ACL test skips as root, so local `--full` runs to the end. Scoreboard: 1 green (guard), 6 red (right reason, by design), 1 fix cycle (container restarts, no scenario change), about 1.5 h incl. three restarts |
| 2026-11-09 | 3 W1 | **Wave 1 merged** (Sonnet 5.5 medium). `receipt.rs`: effects section from `effects_for_run` (read only) per the approved defaults: one line per non-read row (`seq`, `tool`, `adapter`, `class`, `state` confirmed/reconciled/failed/unknown/notSent, `attempts`, `keyDigest` sha256 of the key, `argsDigest`, `resultDigest`, `checkedBy`, `parentSeq`, `repeatOf`, `hostDerivedId`, `at`), `duplicates`, `unknown`, disjoint `counts` (`identicalRepeats`, `reads`), a `scope` sentence; no call id, detail, argument or result. Runs created before the node first started with this version keep the Phase 1 object byte for byte (marker table in `receipt.rs`). `receipt.proto` additive documentation; `vam receipt show` renders effects (new golden); docs section. Bench rows add `receiptDuplicateEffects`, `receiptUnknownEffects`, `effectsMismatch` (any receipt unknown > 0 is a mismatch on every trial: accepted by the orchestrator, stricter than "must-be-zero tasks"). **Fix cycle 1:** all four scenarios read `journaled: false` because `runs.created_at` is NULL on the chat create path; creation time now falls back to `run_requests.created_at`. **Fix cycle 2 (orchestrator, test only):** `receipt_route_test` asserted the retired `journaled: false` on a new run; now asserts journaled with zeros. Signing, canonical form, `RECEIPT_VERSION`, E10's journal files untouched; `old-receipt-still-verifies` green. Orchestrator re-run at the head: `--full` fmt, protos, manifests, check, both clippy and the workspace suite pass (only the route test failed before cycle 2; it passes after); fast mode with smoke **121 pass, 0 fail, 2 red right reason** (stats and diff, waves 2-3). Bench `--mock` on `task-killed-mid-effect-finishes-once`: receipt duplicates 0, unknown 0, mismatch false. **Finding (Phase 1 gap, not fixed here):** `runs.created_at` is NULL for chat runs, so the receipt's `startedAt` is absent for them. Disk: wave runs filled the disk to 45 MB; ladder step 3 (`cargo clean`, 32.6 GB, no agent building) → 29 GB free. Fourth container restart (after `--full` finished). Scoreboard: 4 green, 2 red (right reason, by design), 2 fix cycles, about 2 h |
| 2026-11-10 | 3 W2 | **Wave 2 merged** (Sonnet 5.5 medium). The maker-list-price rule moved from `vam-e2e/src/usage.rs` into `vam-inference` (`list_price_entry`, `list_price_source`, `estimate_list_micro`; bench output unchanged). `outcomes.rs` (pure aggregation over parsed stored receipts, index `idx_runs_agent_urn`) and `api/outcomes.rs` (one dispatch line in `get_agent_wildcard`; `PermissionsQuery` gains optional `k`): `GET /v1/agents/{urn}/outcomes[?version=X.Y.Z|draft&k=N]` per the approved defaults; `outcomes.proto`, OpenAPI stub, api.md and catalog.md sections. `effects.confirmed` = confirmed + reconciled; effect sums cover every receipt of the version (cancelled included), absent when none is journaled. **Fix cycle 1 (review):** the listing (no `version`) counted released versions from receipts, not from the caller's visible catalog rows, so it answered 200 with counts where `versions` answers 404; now filtered to visible releases, 404 on no rows, api test added. Orchestrator re-run at the head: `--full` fmt, protos, manifests, check, both clippy, workspace 1566/1567: one E10 flake under the parallel suite (`host_tools::effect_journal_tests::a_sent_or_unknown_call_answers_the_unknown_outcome_and_is_never_sent`: Sent instead of Unknown) that passed 5/5 alone and its group 15/15; **finding for E10**. Smoke one scenario per run (workaround): 121 pass, 1 red right reason (release diff, wave 3), 1 harness fail only when run alone (`connector-id-cannot-be-taken-by-a-hand-added-server` needs the batch's mock providers; green in the full smoke at Wave 1's head). The 5 Microsoft-connector failures in the wave's own run did not reproduce. Disk: wave filled to 3.7 GB; ladder step 3 (`cargo clean`, 26.6 GB). Fifth container restart (after `--full` ended). Scoreboard: 1 green, 1 red (right reason, by design), 1 fix cycle, about 2.5 h |
| 2026-11-10 | 3 W3 | **Wave 3 merged** (Sonnet 5.5 low). `GET /v1/agents/{urn}/outcomes/diff?version=V[&k=]` (`{base, candidate, changes}`; base = highest visible release below V, latest release for `draft`, absent for a first release; same visibility as the outcomes route; release endpoint unchanged); `vam agent stats <agent> [--version] [--k] [--diff] [--json]` in `vam-cli/src/cmd/outcomes.rs`; `vam agent release` prints the draft-vs-previous diff on stderr (never blocks; stdout unchanged, golden); api.md, catalog.md, outcomes.proto. Finding: a refused Bash call (heredoc plus another command) silently dropped the wave's CLI wiring edits once; caught by the first CLI test run, redone. Orchestrator proof at the head, resumable per step after a sixth container restart (this one during clippy, not smoke, so restarts are not only), builds capped at 2 jobs: fmt, protos, manifests, check, both clippy, **workspace suite all green**, both outcome scenarios PASS; smoke one scenario per run 122 pass, 1 harness fail only when run alone (`connector-id-cannot-be-taken-by-a-hand-added-server`, as in W2). Scoreboard: 1 green, 0 red, 0 fix cycles, about 1.5 h |
| 2026-11-10 | 3 | **`main` merged into the base** (connectors Phase 4 release and its waves). Conflicts only in `crates/vam-e2e/src/{check,engine,scenario}.rs` (both lanes added step registrations; both kept; one dropped brace fixed in a follow-up commit). Proof at the head, resumable steps across two container restarts: fmt, protos, manifests, check, both clippy, **workspace suite green**, the 7 Phase 3 scenarios PASS; smoke one scenario per run 125/131: 4 connectors scenarios fail identically on `origin/main`  in this session, fake Graph sees 0 requests (pre-existing; the Master Orchestrator counts them as such; unsetting the standard proxy variables does not change it), `connector-id-cannot-be-taken-by-a-hand-added-server` fails only when run alone (harness, as in W2 and W3), `agent-preference-off-the-allowlist-does-not-block` failed once at node start (env, 5 ms) and passed on its own. Phase 3 receipt scenarios unchanged by the journaled connection writes |
| 2026-11-10 | 3 | **Phase 3 closed in base.** Seven scenarios active and green (four effect scenarios, outcomes per version, release diff, old-receipt guard); one retired (owner-approved). Exit met on the mock: kill-mid-task receipt 0 duplicates / 0 unknown equal to the ledger; outcomes served by the API and `vam agent stats`. Phase scoreboard: 7 green, 0 red, 3 fix cycles (W1 x2, W2 x1), 4 PRs, plus into `main` (owner's word). Benchmark delta recorded. Team checklist written. Release to `main`: PR opened for the Master Orchestrator |
| 2026-11-10 | 3 | **Released to `main`** (merge commit, merged by the Master Orchestrator) after full Actions green on the release head (run 42747589235: checks, clippy, workspace tests, smoke, docs and the Firecracker lane). Smoke green on the runner confirms the four connectors scenarios fail only in cloud sessions. `feat/run-receipt` fast-forwarded to `main`. Team checklist out (Phase 3 close block). Open items logged: E10 parallel-load flake connectors in cloud sessions chat-run receipts without `startedAt` (this lane's backlog), remote branch deletes on the owner's list. **Phase scoreboard:** 7 green, 0 red, 3 fix cycles, 5 PRs into the base and 1 release PR, plus into `main`; 9 container restarts, no work lost; about 13 h wall time |
