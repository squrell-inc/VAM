# Archive: Harness (H)

Closed phases moved from `../harness.md` (playbook §7).

### Phase H0 + B0 — Switches and honesty; baseline numbers

**Goal:** every lever in `design/harness.md` §1 has a named switch on one
`harness` run input with defaults equal to today's behaviour; the loop stops
lying (typed errors for invalid tool arguments, loop detection, todo
recitation); the effective configuration is on the run record and the
receipt; scenarios gain a `model` block and `summary.json` records usage
(B0: `scripts/bench/` folded into the scenario runner, first real numbers on
two models, `machine on` only).  
**Depends on:** E10 Phase 2 (done, on `main`); E9 Phase 1 (done).  
**Plan:** approved by the owner 2026-10-26 with the orchestrator's recommendations: Q2 `calls.validate` on, `loop.detect` and `context.reciteTodo` off; Q10 release to `main` after wave 3 (owner's word at that point); Q3, Q4, Q7, Q11 defaults; U2 to B1. Still on the owner's word at wave 6: the paid baseline (Sonnet 5.5 + Haiku 5.5, about $5, cap $10; Haiku is outside playbook §2), run on the owner's KVM host.  
**Exit:** all existing scenarios green with defaults; the new scenarios in
`design/harness.md` §7 and `design/benchmark.md` §6 that H0/B0 own are
`active` and green; a bench run prints its switches; a table with real
numbers on two models (owner's word for paid runs).

**Code map** (base, cited for the wave sessions)

- Loop: `run_react_agent_loop_with_trace`, `crates/vam-runtime/src/agent.rs:22`.
  Tool list built at `:82` (`rebuild_tools_list`, `:2546`), prompt at `:99`
  (`build_agent_system_prompt`, `:2476`), model request at `:459`
  (`InferenceRequest`), tool round at `:598`.
- The `{}` fallback: `agent.rs:603-608`. Unparsable arguments **and** valid
  JSON that is not an object (array, string) both become an empty `Struct`
  and go to the PEP and the tool.
- The old same-tool heuristic: `consecutive_same_tool_forces_finalize`,
  `agent.rs:1233-1237`, `#[cfg(test)]` only, always `false`; its test at
  `:2799`. No loop detection exists.
- Todo: `todo_write` in `crates/vam-runtime/src/primitives.rs:539` (result
  does not echo the items); `read_todo_plan` at `agent.rs:1532`, used only
  by compaction (`:409`). No recitation.
- Fixed constants that become switches: compaction trigger 70% and keep 3
  rounds (`crates/vam-runtime/src/compaction.rs:28,34`), clearing every 8
  rounds (`context_window.rs:17`), text result cap 16 KiB (`agent.rs:2207`),
  prompt template v11 (`instruction.rs:10`), sampling 0.1 and 4096 tokens
  (`agent.rs:1515-1517`, not in the §1 inventory; Q9).
- Host to guest: run inputs are a `Struct` on `RunSpec.inputs`
  (`proto/vam/v1/workflow.proto:216`). vamd writes host-owned fields once in
  `resolve_run_budgets` (`crates/vam-orchestrator/src/run_context.rs:333`,
  called at `api/mod.rs:1353`): `max_iterations`, `step_timeout_secs`,
  `termination` (from `reasoningPolicy`), `context_window`. They are listed
  in `HOST_INPUT_FIELDS` (`run_context.rs:31`) so a playbook input schema
  does not reject them. The spec is persisted at `api/mod.rs:1388`; resume
  reads the stored spec and never resolves again. The playbook travels as
  `playbook_procedure` (`run_context.rs:18`).
- Schema check today: host only, `validate_json_schema`
  (`crates/vam-orchestrator/src/hitl.rs:344`), a subset validator (type,
  enum, format, required, properties). No JSON-schema crate in the workspace.
  Builtin schemas with `required`: `primitives.rs:635-680`.
- Usage: `vam.v1.inference.completed` events carry tokens, cache tokens and
  `spendMicro` (`crates/vam-orchestrator/src/events.rs:274`). Prices in
  `crates/vam-inference/src/catalog.rs:119-175`.
- Runner: `crates/vam-e2e/src/scenario.rs` (`deny_unknown_fields`, no
  `model` block), `report.rs:162` `summary` (no usage), `main.rs` (no
  `--bench`). The mock takes `toolCall.args` as an object only
  (`crates/vam-inference/src/mock_script.rs:119-126, 399`).
- Old bench: `scripts/bench/run-bench.sh` drives a live vamd through the
  CLI; `score.sh` scores from events. `--mock` appends `mock-script:` and
  `e2e-step:` markers; broken against today's mock (handoff-e2e-scenarios
  "Known items"). U1 needs a real Notion MCP connection
  (`tasks/u1-notion-draft.json`); U2 needs the web broker.
- Process mode serves `mock-model` only (`crates/vam-orchestrator/src/process_mode.rs:1-16,
  205-218`; E9 locked decision S5). A real-model run needs a default (VM)
  vamd.

**Scenario delta**

| Scenario | Change | Layer | Given | Why |
|---|---|---|---|---|
| invalid-tool-arguments-return-a-typed-error | new | process | org-admin, agent-with-builtins | decision 5: an unparsable call and a call missing `path` each get a typed error naming the field, the tool does not run, the next valid call runs; red at the first `thread` step (today the tool runs with `{}` or the partial args) |
| repeated-identical-call-is-caught-and-observed | new | process | org-admin, agent-with-builtins | `loop.detect` on (through the run's `harness`): the mock repeats one identical `read_file` call; the thread gets the loop observation, then the run fails naming the loop; red at the observation step (today it runs to the iteration cap) |
| todo-is-recited-each-turn-when-enabled | new | process | org-admin, agent-with-todo-list (new piece: read, write, `todo_write`) | two chats: default, the mock never sees the plan; with `context.reciteTodo: true` it does (`anyContains`) and says so; red at the second chat's `thread` step |
| harness-switches-print-on-the-receipt | new | process | org-admin, agent-plain | decision 1 and the exit "a run prints its switches": one chat with defaults, one with an override; the receipt shows profile `base`, its version, digest and the override, and no override on the first; red at the first `receipt` step |
| bench-run-records-cost-per-success | new | runner (`cargo test -p vam-e2e bench`, recorded fixtures) | — | the folded bench writes pass^k, turns, tokens and cost per success and repairs per success per (task, model, mode) from run events and receipts; red: no `--bench` |
| bench-task-set-version-is-pinned-in-results | new | runner (same) | — | benchmark decision 3: the results file names the task-set version, the harness profile and overrides, k and the endpoint; a task whose version is not in the set is refused; red: no `--bench` |
| plain-chat-gets-a-reply, approval-suspends-and-resumes, compaction-keeps-an-early-correction, follow-up-after-compaction, follow-up-uses-earlier-tool-results, long-thread-resumes-after-approval | unchanged, must stay green | process | — | they guard `agent.rs` or `run_context.rs`; they are the proof that defaults reproduce today |
| receipt-separates-host-facts-from-model-text, receipt-verifies-offline-and-fails-on-tamper | guards add `crates/vam-orchestrator/src/receipt.rs`; steps and `then` unchanged | process | — | H-0.3 adds a signed field; nothing guards `receipt.rs` today |

The two `bench-*` rows are runner tests, not `e2e/scenarios/` files: the
runner cannot assert on its own `summary.json`. Wave 0 writes them as
`#[ignore = "pending: --bench"]` tests (so CI stays green) and shows them red
with `--ignored`, failing for the missing flag; wave 4 removes the `ignore`.
They keep the scenario names so B1's tracker can find them.

Owned elsewhere: `machine-off-runs-the-procedure-as-prose-with-same-tools`
and `bench-machine-off-uses-same-tools-and-model` (B1, with `machine off`;
see "Not in this phase"); `thinking-model-tool-calls-are-not-dropped` (H1,
`needs: model`); `vllm-model-runs-with-its-catalog-parser` (deferred to a
self-hosted row); `profile-change-without-rows-is-rejected-at-release`
(H2, profiles pinned); `bench-row-fails-red-on-duplicate-effect` (B2, needs
the effect journal, E10 Phase 4).

**Benchmark delta**

| Line (`benchmark.md` §1) | What H0 + B0 does to it |
|---|---|
| P5 pass^k, turns, tokens, cost per success | first real numbers (B0 exit); defaults only, so before = after for the code H0 adds (decision 3) |
| P12 first-try tool success; repairs per success | becomes measurable: invalid calls are typed errors (H-0.4), counted per run in `summary.json` and the results file (H-0.8) |
| P8 cost control | loop detections counted; with `loop.detect` off by default (Q2) the line does not move in `base`; H2 measures it |
| P2 receipt completeness | the receipt names the harness profile, version, digest and overrides (H-0.3) |

Tasks added (task set `0.1.0`, the first versioned set): U1 v2 first-try
tool use with a stand-in tool (no SaaS mock world exists; the Notion task
stays a manual check), U9 match, U9 no-match, U9 injected (ported from
`scripts/bench/tasks/`). U2 cited research is SLICE (H-0.11; default: B1).

Baseline plan. **Needs the owner's word before any paid run** (playbook
§10). No run happens in this breakdown.

- When: once, after wave 5, on the merged base, all switches at default.
  The defaults equal today except honest errors on invalid calls, so this
  row is the baseline of the current base; the phase close records it as
  both before and after, and the lines H0 makes measurable are written down
  as findings (playbook §11).
- Models: **Claude Sonnet 5.5** and **Claude Haiku 5.5**, both from the
  benchmarked tier (`agent-runtime.md` decision 8), endpoint
  `https://api.anthropic.com/v1/messages`, one key (`anthropic/api-key` in
  vamd's vault). Haiku is the small row and already the E2E model. The
  OpenAI row (`gpt-4o`, `api.openai.com`) joins in B1 (Q8).
- Tasks: U1 v2, U9 x3 (plus U2 only if H-0.11 is pulled in); `machine on`;
  k = 5.
- Runs: 4 tasks x 5 x 2 = 40 (50 with U2).
- Cost, from catalog prices and about 50k prompt and 1.5k completion
  tokens per run before caching: Sonnet about $0.12 per run, Haiku under
  $0.01; **about $5 in total, cap $10** with one smoke run at k = 1. U2
  adds a Brave key and about $1.
- Where: the microVM lane on the owner's KVM host (KVM; cloud sessions have no
  `/dev/kvm`), `scripts/e2e/run.sh --firecracker --bench --real --model
  claude-sonnet-5-5 --model claude-haiku-5-5 -k 5`. Process mode cannot
  serve a real model (S5; Q1).

**Items**

One prefix, `H-0.n`, for H0 and B0: one phase, one tracker, and B has no
tracker until B1. B0 items are marked (B0) so B1 can cite them.

- [x] `H-0.1` — **Scenarios and runner additions (wave 0).** The four
      process scenarios above as `pending` with `expectFail`, the two
      runner tests as ignored-pending (red with `--ignored`), the guard additions on the two receipt
      scenarios, and what the new scenarios need to parse: `harness:` on
      `chat` and `start-playbook` (`scenario.rs` `ChatFull`,
      `StartPlaybook`; the engine puts it into the run's `inputs.harness`;
      today's vamd ignores it, so the scenarios fail at the right step);
      mock `toolCall.rawArgs`, a string sent verbatim, for the unparsable
      case (`mock_script.rs` `MockToolCall`, test-only mock); world piece
      `agent-with-todo-list`. *Accept:* `cargo run -p vam-e2e -- check`
      clean; `scripts/e2e/run.sh --scenarios <the four>` reports each red
      for the right reason, class `product`; `--smoke` otherwise unchanged.
      *Test:* `cargo test -p vam-e2e -p vam-inference mock_script`, the
      runs above. *Turns green:* none.
      `Decide: NOW — proof 2 (the phase's scenarios), cost 1 (test-only runner and mock fields), line P12 and P5 via the phase exit`
- [x] `H-0.2` — **The `harness` run input (scaffold).** `vam-spec`
      `harness` module: one struct with every switch in `design/harness.md`
      §6.1 (`adapter`, `tools`, `calls`, `context`, `loop`, `process`,
      `reliability`, `human`), camelCase serde, `Default` = profile `base`
      v0.1.0 = today's values; `validate()` with a `thiserror` error naming
      the switch: unknown switch, a value for a lever not built on this
      version (anything but its default for rank, deferred, mask, examples,
      parallel, plan_first, verify_before_finish, escalate,
      reset_at_tokens, helper, journal, fallback, verify_reads, ask policy,
      jig, every `adapter.*`), and any `process.*` value but `true` (Q4);
      `effective(overrides)` and `digest()` (sha256 of the canonical JSON).
      vamd: a `resolve_harness` beside `resolve_run_budgets`
      (`run_context.rs:333`) reads a caller `inputs.harness` as overrides,
      refuses a bad one with 400 naming the switch, and writes the
      effective object back (host-owned; added to `HOST_INPUT_FIELDS`).
      Guest: parses `inputs.harness` once at the top of
      `run_react_agent_loop_with_trace`; missing means `Default`. Nothing
      reads a switch yet. Docs: a "Harness switches" section in
      `docs/src/content/docs/runtime/agent.md`. *Accept:* a unit test pins
      `Default` to today's constants (H-0.7 fills the rest); round-trip and
      refusal tests next to `resolve_run_budgets_tests`
      (`run_context.rs:2416`); a resumed run keeps its stored harness;
      `--smoke` green (defaults change nothing). *Test:* `cargo test -p
      vam-spec harness`, `-p vam-orchestrator run_context`, `-p vam-runtime`,
      `scripts/e2e/run.sh --smoke`. *Turns green:* none alone.
      `Decide: NOW — proof 3 (decision 1: no benchmark row without a disclosed harness), cost 1 (one wave; an object in the existing inputs Struct, no proto), line P2 harness disclosure`
- [x] `H-0.3` — **Effective configuration on the run record and the
      receipt.** Run record: the stored `RunSpec.inputs.harness` (H-0.2).
      Receipt: `receipt.proto` gains `ReceiptHarness harness = 13`
      {`profile`, `version`, `digest`, `overrides`}; `receipt.rs` `build`
      writes `harness` from the spec `gather` already parses; `vam receipt
      show` prints one line, e.g. `harness base v0.1.0 (overrides:
      loop.detect=true)` (decision 7 of E12: the fact is in the JSON, the
      renderer adds none); `docs/src/content/docs/security/run-receipt.md`
      gains the field. An addition to E12's schema: no existing field
      changes. *Accept:* receipt unit tests updated for the new key;
      verify still passes and a tamper of `harness` fails; the receipt
      scenarios stay green. *Test:* `cargo test -p vam-orchestrator
      receipt`, `-p vam-security receipt`, `-p vam-cli`, scenarios below
      plus `receipt-*`. *Turns green:* harness-switches-print-on-the-receipt.
      `Decide: NOW — proof 3 (P2 receipt completeness; benchmark §3 harness disclosure), cost 1 (gather already parses RunSpec; one optional proto field), line P2`
- [x] `H-0.4` — **Honest tool-call errors (decision 5).** Move the subset
      validator from `hitl.rs:344` into `vam-spec` with typed errors
      (`{tool, field, expected, got}`) and use it from `hitl.rs` and the
      guest. In the tool round (`agent.rs:598-608`): unparsable or
      non-object arguments are a typed error result ("arguments are not
      valid JSON", never `{}`), with or without `calls.validate`; with
      `calls.validate` (default **on**, Q2) a schema violation is a typed
      error naming the field and the fix. An invalid call does not reach
      the PEP or the tool. Unknown schema keywords pass, so every call valid
      today runs as today (MCP and host tools keep their host check).
      `calls.repairBudget` (default unlimited, today's behaviour): after N
      invalid calls in a row to one tool the task fails naming the tool.
      The error text starts with a stable prefix constant in `vam-spec`
      (H-0.8 counts from it). `task_complete` is checked the same way.
      *Accept:* unit tests per builtin schema (missing required, wrong
      type, unparsable, array), valid calls byte-identical to today; all
      scenarios guarding `agent.rs` green. *Test:* `cargo test -p vam-spec
      -p vam-runtime -p vam-orchestrator hitl`, scenarios. *Turns green:*
      invalid-tool-arguments-return-a-typed-error.
      `Decide: NOW — proof 3 (decision 5; H1 exit "0 silent {} calls"; P12 is unmeasurable while bad calls become {}), cost 1, line P12 first-try tool success`
- [x] `H-0.5` — **Loop detector (`loop.detect`).** Over the task's own
      calls in the tool round: the same tool with canonical-equal
      arguments N times in a row, an A-B-A-B alternation of M cycles, the
      same error result R times in a row (thresholds in the harness struct,
      defaults in Q3). First trip: one observation in the conversation
      naming the pattern and what to do instead. Next trip: the task fails,
      error naming the loop kind (code 10, aborted), with usage. Default
      off (Q2). Removes the test stub at `agent.rs:1233-1237`; its claim
      (the same tool with different arguments never stops a task) stays as
      a detector test. *Accept:* unit tests per pattern, a near-miss per
      pattern does not trip, off means no detector state; scenarios green.
      *Test:* `cargo test -p vam-runtime`, scenarios. *Turns green:*
      repeated-identical-call-is-caught-and-observed.
      `Decide: NOW — proof 2 (P8: a stuck run burns to the 50-turn cap today; P13), cost 1 (same function as H-0.4), line P8 and loop detections`
- [x] `H-0.6` — **Todo recitation (`context.reciteTodo`).** When on and
      `read_todo_plan` (`agent.rs:1532`) finds a plan, every model request
      carries it once, after the last message, as a short labelled
      user-role note (capped 4 KiB). It is not added to the stored
      conversation, so the cached prefix and the thread log are unchanged.
      **Changed 2026-10-26 (owner):** the host recorder
      (`thread_recorder.rs`) logs every inference request, so a request-only
      note would be logged and cause a `rollback{prefix_mismatch}` each turn.
      The guest labels the note and the host records it on its own `note`
      lane, outside the main context chain (audit keeps what the model saw;
      no rollbacks; cached prefix unchanged). Wave 3b, Sonnet medium.
      Default off (decision 3). *Accept:* off: the request is byte-identical
      to today (unit test); on: the note is last and not in the next
      turn-commit. *Test:* `cargo test -p vam-runtime`, scenarios. *Turns
      green:* todo-is-recited-each-turn-when-enabled.
      `Decide: NOW — proof 2 (a W lever the H2 grid needs switchable; P12), cost 0 (agent.rs already in this phase, read_todo_plan exists), line P12 on open-small (H2)`
- [x] `H-0.7` — **Present levers read from the struct.** Compaction trigger
      and keep rounds, clearing interval, text result cap, reminders,
      prompt variant (`v11` only), procedure render variant (today's only)
      move from constants to switches, read where the constants are read
      today. *Accept:* the `Default` test from H-0.2 now covers each
      constant; a changed threshold changes the unit-tested trigger;
      scenarios green. *Test:* `cargo test -p vam-runtime -p vam-spec`,
      `--smoke`. *Turns green:* none (proof is all green).
      `Decide: NOW — proof 2 (H2 sweeps these; no attribution without switches), cost 0 (files already touched), line P5 via H2`
- [x] `H-0.8` — **Usage and harness in `summary.json` (B0, shared with
      H0).** `report.rs` `summary`: per scenario `usage` {calls,
      promptTokens, completionTokens, cachedTokens, cacheCreationTokens,
      spendMicro, turns, estimated} from the runs' `inference.completed`
      events and receipt metering; `harness` {profile, version, overrides}
      from the receipt; `toolCalls` {invalid, repairs} and
      `loopDetections` counted from the thread by the `vam-spec` prefixes
      (host-recorded, no proto; Q6). On every run, not only the bench.
      Mock usage is marked `estimated`. *Accept:* report unit tests with
      recorded events; `--smoke` writes the fields. *Test:* `cargo test -p
      vam-e2e`. *Turns green:* none alone (feeds the next item).
      `Decide: NOW — proof 2 (P5, P12 need per-run usage), cost 1, line P5 tokens and cost per success`
- [x] `H-0.9` — **`model` block and `--bench` (B0).** `scenario.rs`:
      optional `model` {task, version, k, modes: [on], timeout}. `vam-e2e
      run --bench` selects scenarios with a `model` block and runs k trials
      per `--model`; `--mock` uses the scenario's `mock` block and marks the
      file `mock: true`, never publishable (benchmark decision 4); a
      non-mock model is refused without `--real` (as `run-bench.sh:76-83`).
      A trial passes when every `then` holds. For a real model the engine
      leaves out the `e2e-step:` markers, and the node is the microVM lane:
      the key comes from the runner's environment into vamd's vault, never
      into the out dir or `summary.json`. Output
      `bench/results/<task-set-version>.json`: one row per (task, model,
      mode) with endpoint host, profile, version, overrides, k, pass^k,
      pass@k, pass rate and its standard error (the noise band, Q7), turns,
      tokens, cost and wall time per success, invalid calls, repairs per
      success, loop detections, cost-missing flag. *Accept:* the two runner
      tests; a `--bench --mock` run on the ported tasks writes a valid
      file; refusal tests. *Test:* `cargo test -p vam-e2e`,
      `scripts/e2e/run.sh --bench --mock`. *Turns green:*
      bench-run-records-cost-per-success, bench-task-set-version-is-pinned-in-results.
      `Decide: SLICE — proof 3 (benchmark decision 7: no table without the lane; P5), cost 2 (results schema, a runner lane with a key) → slice: --bench, --mock in process mode, real models on the microVM lane only, cost 1. Rest: a real-model process node (Q1) and the CI lane (B3), trigger "owner amends S5" or B3`
- [x] `H-0.10` — **Tasks as scenarios (B0).** U1 v2 (first-try tool use on
      a stand-in tool; the Notion version stays a manual spot check), U9
      match, no-match, injected, each with a `model` block, its existing
      mock script inline, its `then` from the `score.sh` pass rule, and
      world pieces for the U9 playbooks (from `scripts/bench/playbooks/`).
      Task set `0.1.0` listed in one file the runner checks. They test
      existing behaviour, so they land `active` and green on the mock.
      *Accept:* `check` clean; `--smoke` and `--bench --mock` green.
      *Test:* `scripts/e2e/run.sh --scenarios <the four>`. *Turns green:*
      the four tasks (mock).
      `Decide: NOW — proof 2 (the baseline tasks), cost 1 (ports), line P5, P12 (U1), P4 (U9)`
- [ ] `H-0.11` — **U2 cited research as a task (B0).** Needs a
      `web-broker-mock` world piece and the runner starting the broker
      (`scripts/run-web-broker.sh --mock`), plus a Brave key for a real run.
      `Decide: SLICE — proof 2 (P5 row), cost 2 (a broker world piece and process in the runner) → default: B1, where business tasks add world pieces anyway. Pull in only if wave 5 has room`
- [x] `H-0.12` — **Retire the old bench; docs; noise band (B0).** Remove
      `scripts/bench/run-bench.sh`, `score.sh` and `test.sh` (this replaces
      the broken `--mock` live path), keep fixtures the tasks use; CI and
      `.gitignore` references updated. Docs: `runtime/evals.md` "Harness
      benchmark" section rewritten for `--bench`, and a results page (sidebar
      in `docs/astro.config.mjs`) saying what is measured (U1 v2, U9, machine
      on, two models, k = 5, profile `base`) and what is not (machine off,
      business tasks, open models, the isolation lane, any mock number), and
      the noise band. *Accept:* `pnpm build` in `docs`; no reference to the
      removed scripts. *Test:* docs build, `--smoke`.
      `Decide: NOW — proof 2 (the B0 exit names the docs page; benchmark decision 4), cost 1, line P5 publication`
- [x] `H-0.14` — **Real-model runs on the microVM lane (B0; added
      2026-10-27, owner).** Wave 4 refuses a real model on every lane, so
      the baseline cannot run. `--bench --real` runs when the node reports
      `guestMode: vm` (process mode stays mock-only, E9 S5). The provider
      key stays in vamd's own secret store on the owner's KVM host; the runner
      never reads, passes or writes it, and only checks that the node can
      serve the model (refused with a clear message otherwise). *Accept:*
      refusal tests per gate; a mock-model `--bench` on the advisory
      microVM lane in CI; no key in any output. *Test:* `cargo test -p
      vam-e2e`, CI firecracker lane. *Turns green:* none (unit and lane
      proof; the first real run is wave 6's k = 1 smoke).
      `Decide: NOW — proof 3 (B0 exit: real numbers), cost 1 (gates in the runner, no new trust path), line P5`
- [x] `H-0.15` — **Bench models through Vercel AI Gateway (B0; added
      2026-10-27, owner).** The owner holds a Vercel AI Gateway key, not an
      Anthropic one. Today the gateway is reached only through a run's
      `provider: vercel` model preference (`vam-inference` `catalog.rs`
      `resolve`, prices 0), and the static catalog has one gateway entry
      (`openai/gpt-4o`, price 0), so the bench cannot run Claude through it
      and would report $0 cost. Preferred: reuse the existing preference
      path (the runner sets the preference; gates check `vercel/api-key`);
      fallback: static gateway entries for Claude Sonnet 5.5 and Haiku 5.5.
      Either way cost per success must be real (catalog prices or the
      gateway's reported cost), and the runbook switches to `vercel/api-key`.
      *Accept:* gate and cost unit tests; mock smoke green; runbook updated.
      `Decide: NOW — proof 3 (B0 exit: real numbers; the owner's only key), cost 1, line P5`
- [x] `H-0.16` — **Fresh agent, workspace and thread per bench trial (B0 fix; added 2026-10-27).** Under `--bench` every trial of a scenario reused the scenario's names (`run_cmd.rs:711` repeats the scenario k times per model; `engine.rs:925` names the thread `t1` again; world-piece names are per scenario run), so all trials of a task, both models, continued one conversation (owner's KVM host diagnosis of run `d40996`). Fix: a trial-unique suffix for world pieces, workspace and thread; `summary.json` top-level `mock` from the run, not hard-coded (`report.rs:186`); one brief per trial, not overwritten. *Accept:* a runner test that k trials get distinct agent, workspace and thread ids and that trial 2 sees no message of trial 1; `--bench --mock` over the four tasks; smoke green. `Decide: NOW — proof 3 (B0 exit: the baseline is invalid without it), cost 1, line P5`
- [x] `H-0.17` — **A step budget for real-model bench runs (B0 fix; added 2026-10-27, owner chose option A).** The baseline re-run (run on the owner's KVM host, 35/40) lost all five Sonnet playbook-match trials to the runner's 30 s default step budget (`engine.rs:32`, set for the mock): each had started the playbook, written the draft and closed the step. Under `--bench --real` the default step budget becomes 180 s (a flag overrides it; mock and CI stay at 30 s); the budget in force is recorded in `bench-results.json`. Also the `run.sh` absolute-`CARGO_TARGET_DIR` fix from the backlog. Then the whole k = 5 baseline is re-run so every row shares one run. `Decide: NOW — proof 3 (B0 exit: a fair baseline), cost 0, line P5`
- [x] `H-0.13` — **Baseline run (B0, no code).** (Run `d40996` withdrawn 2026-10-27: trials shared one conversation. Valid baseline: run `55cb5b`.) On the owner's word, on
      the Mac: the command in the baseline plan; commit
      `bench/results/0.1.0.json` and the table on the docs page; the
      numbers go into the progress log. *Accept:* rows for both models, no
      cost-missing row, the losses shown.
      `Decide: NOW — proof 3 (B0 exit: real numbers; benchmark decision 4), cost 1 (about $5, one session), line P5`

**Not in this phase**

- `machine off` rendering (§6.1 says "off rendering in H0"). H0 adds the
  `process.*` switches with only `true` accepted (H-0.2). Rendering the
  procedure as prose, and its scenario, move to B1, which is the first
  consumer, after §8 Q1 is answered.
  `Decide: SLICE — proof 2 (P4 gap), cost 2 (host playbook admission, finish check and prompt across vamd and guest) → slice in H0: the switch, refused off, cost 0. Rest: B1, trigger "B1 planned"`

**Waves**

| Wave | Items | Turns green | Repo | Model | Budget | Depends on | Status | PR |
|---|---|---|---|---|---|---|---|---|
| 0 | H-0.1 | — (red, right reason) | vam | Sonnet low | 60 min | — | [x] | — |
| 1 | H-0.2, H-0.3 | harness-switches-print-on-the-receipt | vam | Sonnet medium — a new run-input schema read by vamd and the guest, plus a receipt proto field (rubric: two crates, schema) | 120 min | 0 | [x] | — |
| 2 | H-0.4, H-0.5 | invalid-tool-arguments-return-a-typed-error, repeated-identical-call-is-caught-and-observed | vam | Sonnet medium — changes the loop's tool path and stop conditions; the validator moves to vam-spec for host and guest | 120 min | 1 | [x] | — |
| 3a | H-0.7 | — (proof is all green) | vam | Sonnet low | 60 min | 2 | [x] | — |
| 3b | H-0.6 | todo-is-recited-each-turn-when-enabled | vam | Sonnet medium — the host thread recorder gains a note lane (`thread_recorder.rs`, request hint), a run-record path, not only the guest | 90 min | 3a | [x] | — |
| 4 | H-0.8, H-0.9 | bench-run-records-cost-per-success, bench-task-set-version-is-pinned-in-results | vam | Sonnet medium — a new results schema and a runner lane that starts a real-model node with a key | 120 min | 3 | [x] | — |
| 5a | H-0.10, H-0.12 | the ported tasks on the mock | vam | Sonnet low | 90 min | 4 | [x] | — |
| 5b | H-0.14 | — (refusal tests; mock run on the advisory microVM lane) | vam | Sonnet medium — gates around real-model runs; Sonnet high if the key must pass through the runner (secrets boundary) | 90 min | 5a | [x] | — |
| 5c | H-0.15 | — (gate and cost tests; mock) | vam | Sonnet medium — may thread a provider preference through the run input and the inference broker | 60 min | 5b | [x] | — |
| 5d | H-0.16 | — (runner test: trials isolated) | vam | Sonnet low | 60 min | 5c | [x] | — |
| 5e | H-0.17 | — (runner tests; mock) | vam | Sonnet low | 45 min | 5d | [x] | — |
| 6 | H-0.13 | — (numbers) | vam | owner on the Mac; Sonnet low for the results PR | 45 min | 5e, owner's word | [x] | — |

Branches `h-0/wave-N`, PRs into `feat/harness`. Wave 1 is the scaffold the
later waves share (the struct). Waves 1 to 3 hold every `agent.rs` change;
waves 4 to 6 do not touch it, so H0 can be released to `main` after wave 3
and E10 Phase 3 can start building (Q10).

**Open questions** (each has a default)

1. **Real model in process mode.** `benchmark.md` says the scored lane runs
   in the process-mode guest with a real endpoint; E9 locked decision S5 and
   `process_mode.rs` allow only `mock-model`. **Default:** keep S5; B0 real
   runs use the microVM lane on the owner's KVM host. Amending S5 (a bench-only
   node that serves real models, no provider-key import beyond the run's
   key) is a separate owner decision, needed before B3.
2. **Which new levers are on by default.** Design §1 marks `calls.validate`
   and `loop.detect` "default on"; decision 3 and §2 step 3 say defaults
   reproduce today. **Default:** `calls.validate` on (decision 5 makes the
   `{}` coercion a defect, not a lever; valid calls are unchanged);
   `loop.detect` and `context.reciteTodo` off until an H2 row.
3. **Loop thresholds** (design §8 Q3). **Default:** 5 identical calls in a
   row (Gemini CLI), 3 A-B cycles and 3 identical errors in a row
   (OpenHands `StuckDetector`); data in the struct, tuned per class in H2.
4. **`process.*` off and decision 6.** Turning guards, gates or evidence
   off would loosen host checks. **Default:** H0 accepts only `true`;
   `machine off` in B1 keeps guards and gates and changes only tracking and
   rendering (decision 4).
5. **Who may override switches.** Design §4: "for the bench only".
   **Default:** any caller with `runs:write` on their own run, printed on
   the receipt as overrides; agents name profiles from H1.
6. **Where repairs and loop detections are counted.** **Default:** from the
   host-recorded thread by stable prefixes, no proto change; a guest
   counter on `TaskOutcome` only if B1 needs them on the receipt.
7. **Noise band** (`benchmark.md` §7 Q1). **Default:** standard error of
   the pass rate over k, printed per row.
8. **The bench model pair.** **Default:** Claude Sonnet 5.5 and Claude
   Haiku 5.5 on the Anthropic endpoint (one key, about $5). Alternative:
   Sonnet 5.5 and `gpt-4o` (about $9, two keys).
9. **Sampling is not in the §1 inventory** (`agent.rs:1515-1517`:
   temperature 0.1, 4096 output tokens). **Default:** record both in the
   effective configuration as fixed; a switch in H1 with `adapter.effort`.
10. **Release after wave 3.** **Default:** yes, on the owner's word, so E10
    Phase 3 builds while B0 finishes (`launch-0.1.0.md` §4 conflict rule).
11. **`machine off` rendering** (design §8 Q1). **Default:** decided in B1;
    H0 only adds the switch.

**Phase close:** unit suites on the merged base; release; team checklist out
(the next phase does not wait for it; playbook §6).

**Team checklist — Phase H0 + B0** (final, 2026-10-28; about 10 minutes)

Setup: deploy vamd from `main` after the H0 + B0 release, then the guest rootfs from the same
build (the guest sends a new thread-log lane; an older guest still works). `vam` CLI from the
same build. No env var.

1. Run any chat with your agent. **See:** `vam receipt show <run>` has a "Harness" section:
   `base v0.1.0`, a digest, and no overrides.
2. Start a run with `inputs.harness` = `{"loop":{"detect":true}}`. **See:** that run's receipt
   lists `loop.detect=true` under overrides.
3. Start a run with `{"tools":{"rank":true}}`. **See:** HTTP 400; the error names
   `tools.rank` as not built on this version. Nothing runs.
4. Start a run with `{"context":{"compaction":{"triggerPercent":90}}}`. **See:** HTTP 400;
   the error names `context.compaction.triggerPercent` and the range 10 to 75.
5. Start a run with `{"profile":"x"}` inside `inputs.harness`. **See:** HTTP 400 naming
   `profile` (a caller cannot send its own harness record).
6. Run `vam receipt verify` on the receipt from step 2. **See:** it verifies. Change one
   character of its `harness` block and verify again. **See:** it fails.
7. Ask an agent with file tools to save a short note. **See:** in the run's thread there is
   never a tool call with empty arguments `{}`; if the model sent bad arguments, you see an
   error starting `invalid arguments for` and then a corrected call.
8. Open the docs page Runtime → Benchmark Results. **See:** the 0.1.0 table: eight rows
   (four tasks, Claude Sonnet 5.5 and Claude Haiku 5.5), `k = 5`, run `55cb5b`, 40/40, cost at
   Anthropic's list price, and the note on the two withdrawn runs.

Report: "Phase H0 + B0 pass", or the step number with a screenshot or the error text.

## Progress log (Phase H0 + B0)

| Date | Phase | What landed |
|---|---|---|
| 2026-10-26 | H0 + B0 | **Breakdown drafted** (background agent, Opus 5.5 high), awaiting owner approval. Base (E12 Phase 1 merged in). Scenario delta: four new process scenarios (`invalid-tool-arguments-return-a-typed-error`, `repeated-identical-call-is-caught-and-observed`, `todo-is-recited-each-turn-when-enabled`, `harness-switches-print-on-the-receipt`), two runner tests (`bench-run-records-cost-per-success`, `bench-task-set-version-is-pinned-in-results`), two receipt scenarios gain a guard. Items H-0.1 to H-0.13 in seven waves: W0 low scenarios, W1 medium harness input and receipt line, W2 medium honest errors and loop detector, W3 low recitation and present levers, W4 medium usage and `--bench`, W5 low tasks, retire old bench, docs, W6 baseline on the Mac (owner's word, about $5 on Sonnet 5.5 and Haiku 5.5). `machine off` rendering and U2 sliced to B1. Found in the code: the receipt has no harness field; non-object JSON arguments also become `{}`; process mode cannot serve a real model (S5), so real bench runs need the microVM lane; U1 needs a real Notion connection; sampling is fixed and not in the inventory. 11 open questions with defaults. Scoreboard: 0 green, 0 red (scenarios not yet written), 0 fix cycles. |
| 2026-10-26 | H0 + B0 | **Plan approved; locked decisions 1-8 confirmed** by the owner, with the orchestrator's recommendations (see Plan line). Wave 0 started (plan only). Scoreboard: 0 green, 0 red, 0 fix cycles. |
| 2026-10-26 | H0 + B0 W0 | **Wave 0 merged** (merge commit; background agent, Sonnet 5.5 low). Owner approved the plan with three orchestrator corrections (pinned typed-error phrases `invalid arguments for <tool>: not valid JSON` / `missing required field '<f>'`, since `read_file` already answers "requires 'path'"; todo scenario across new threads, since `anyContains` sees the `todo_write` args; loop scenario without an unnamed event) and receipt fields `harness.profile`, `.version`, `.digest`, `.overrides`. Landed: four pending scenarios, world piece `agent-with-todo-list`, runner `harness:` on `chat`/`start-playbook` into `inputs.harness` (absent when unset, unit-tested), mock `toolCall.rawArgs` (verbatim; with `args` refused), `run.completed` added to known events, two ignored bench runner tests (red with `--ignored`: no `--bench`), `receipt.rs` guard on the two receipt scenarios. Found: a run that hits the iteration cap without a playbook ends SUCCEEDED; the loop scenario's `run.completed` `error` path is unverified until wave 2 (a difference there is a spec change for the orchestrator); bench tests pin flag and field names wave 4 may need to change (spec change line in that PR). Nit for wave 1: `put_harness` took `part_text`'s doc comment in `engine.rs`. Orchestrator re-ran at the head: `vam-e2e check` 35 scenarios / 12 world pieces; the four red (right reason) at when.1, when.1, then.0, when.5, class product; smoke 24 pass, 4 red (right reason), 0 fail, no leftovers. Merged under the 2026-10-25 rule (test-only, proven locally) with CI long jobs still running; watched afterwards. Scoreboard: 0 turned green, 4 red (right reason), 0 fix cycles, about 30 min. |
| 2026-10-26 | H0 + B0 W1 | **Wave 1 plan approved** by the owner (H-0.2, H-0.3; Sonnet 5.5 medium, branch `h-0/wave-1`): non-default values for present levers refused until H-0.7; `loop.detect`, `calls.validate`/`repairBudget`, `context.reciteTodo` accepted but inert until waves 2-3 (no release before wave 3); receipt carries only `harness {profile, version, digest, overrides?}`, full switch tree on the run record; integers only in the canonical form (`triggerPercent`, temperature in milli); `repairBudget: 0` = unlimited. E12's own files (`design/run-receipt.md`, its tracker) are not edited by this lane; the schema addition is recorded under Coordination points. Building. |
| 2026-10-26 | H0 + B0 W1 | **Wave 1 merged** (merge commit; background agent, Sonnet 5.5 medium). Landed: `vam_spec::harness` (every §6.1 switch, `Default` = profile `base` 0.1.0 = today's values, integers only in the canonical form, `resolve`/`record`/digest = sha256 of canonical `{profile, version, switches}`; refusals typed and naming the switch: unknown, wrong type, not built, fixed (sampling), `process.*` not `true`, non-default present levers until H-0.7; `loop.detect`, `calls.validate`/`repairBudget` (0 = unlimited), `context.reciteTodo` accepted but inert until waves 2-3); vamd `resolve_harness` at the single run-creation site beside `resolve_run_budgets`, 400 naming the switch, caller `profile`/`version`/`digest`/`switches` refused, every run gets the `base` record, resume never re-resolves; `harness` in `HOST_INPUT_FIELDS` (stripped only for the playbook schema check, record reaches the guest); guest parses once in `agent.rs` (one binding, no behaviour change); receipt `harness {profile, version, digest, overrides?}` (proto field 13, old receipts verify, tamper fails); `vam receipt show` `## Harness` section; docs in `runtime/agent.md` and `security/run-receipt.md`; doc-comment nits fixed (`engine.rs`, `receipt.rs`). **`harness-switches-print-on-the-receipt` active and green.** Orchestrator read the input-resolution and receipt code and re-ran at the head: vam-spec harness 18, vam-orchestrator harness 9, vam-security receipt 21, fmt ok; three receipt scenarios pass; smoke 25 pass, 3 red (right reason), 0 fail. CI green (checks, workspace tests, smoke) before merge; Firecracker (advisory) still running, watched. Scoreboard: 1 turned green, 3 red (right reason), 0 fix cycles, about 75 min. |
| 2026-10-26 | H0 + B0 W2 | **Wave 2 plan approved** by the owner (H-0.4, H-0.5; Sonnet 5.5 medium, branch `h-0/wave-2`): guest-side subset validator `vam_spec::call_check` (no new dep; `filepath` alias kept; null = absent; integral float = integer); decision 5 over the brief: `calls.validate: false` skips schema checks only, unparsable or non-object arguments are always a typed error; `task_complete` type-checked only; empty-string arguments = `{}`; **host `hitl.rs` / `host_tools.rs` validation untouched** (MCP argument check is the security path and next to IT Phase 2's files; unifying host and guest checkers is backlog: `Decide: BACKLOG — proof 1, cost 2, line none (refactor)`); detector 5 identical / 3 A-B cycles / 3 identical errors, observe once then stop with `loop_detected:`, state not kept across suspend/resume (accepted for H0, revisit in H2); `repair_budget_exhausted` when a budget is set; Q6 prefixes `invalid arguments for ` (tool) and `Loop detected:` (user). `run.completed` `error` path traced in code (`api/mod.rs` `result_json.error`). Building. |
| 2026-10-26 | H0 + B0 W2 | **Wave 2 merged** (merge commit; background agent, Sonnet 5.5 medium). Landed as approved: `vam_spec::call_check` (guest only; host `hitl.rs`/`host_tools.rs` untouched), `checked_tool_args` in `agent.rs` before the PEP (a rejected call never reaches Cedar, approval or the tool; never `{}`), `loop_detect.rs` (created only with `loop.detect`; warn once with `Loop detected:`, then `loop_detected: <kind>: …`, code 10, run FAILED), `repair_budget_exhausted:` (code 3) when a budget is set, the `consecutive_same_tool_forces_finalize` stub removed. Live `run.completed` payload confirmed: `{"error":"loop_detected: identical_calls: read_file called with identical arguments 5 times in a row; the task was stopped"}`; no spec change. **`invalid-tool-arguments-return-a-typed-error` and `repeated-identical-call-is-caught-and-observed` active and green.** Orchestrator read the `agent.rs` diff and re-ran at the head: vam-spec call_check 12, vam-runtime lib 239, fmt ok, `check` 35/12; the two new plus the six scenarios guarding `agent.rs` 8/8; smoke 27 pass, 1 red (right reason), 0 fail. CI green (checks, workspace tests, smoke) before merge; Firecracker (advisory) running, watched. Scoreboard: 2 turned green, 1 red (right reason), 0 fix cycles, about 60 min. |
| 2026-10-26 | H0 + B0 W3 | **Wave 3 split** by the owner after plan review. The plan's claim that a request-only recitation note stays out of the thread log was wrong: `thread_recorder.rs` records from each inference request and appends `rollback{prefix_mismatch}` on divergence. Wave 3a = H-0.7 (present levers: `Rule::Range`/`Only`; trigger 10-95, keepRounds 1-50, everyRounds 1-200, resultCap 1024-16384, `promptVariant` only `v11`, `renderVariant` only `base`, `reminders` = the three `environment_guidance` bullets; guest only), building on Sonnet low. Wave 3b = H-0.6 with a host `note` lane (option 1; rejected: host silently drops the note, weakens P2; system-prompt recitation, busts the cache on each todo change), Sonnet medium, planned after 3a. |
| 2026-10-26 | H0 + B0 W3a | **Wave 3a merged** (merge commit; background agent, Sonnet 5.5 low). H-0.7 as approved, guest only: `Rule::Range`/`Only`, `OutOfRange`/`OnlyValue`, `NotWired` removed; compaction `decide`/`plan_cut`, clearing cadence, result cap (through `execute_host_tool_call`'s callers) and reminders (`build_system_prompt_with`) read from the struct; `Default` test pinned to the real constants. **Review fix:** `triggerPercent` narrowed from the approved 10-95 to **10-75**, since `cap_round_results` holds a round to 15% of the window plus a 4,096-token reply, so above ~75% the summary request can overflow (owner informed); H2 must tie the round cap to the trigger before widening. `ask_playbook_finish` keeps its fixed 16 KiB cap (parsed, not shown to the model). **CI did not run on the final head** (marking ready cancelled the push run; the ready run skipped all jobs; a hand re-run would start the Firecracker lane): merged under the 2026-10-25 rule (guest-only, not security-sensitive) on local proof; wave 3b's PR carries this code through full CI. Orchestrator re-ran: vam-spec 191 (+5, +6), vam-runtime lib 244, vam-orchestrator lib 1056, vam-e2e all, `cargo check --workspace` ok, fmt ok; six scenarios guarding `agent.rs` pass; todo scenario red (right reason) at when.5; smoke 27 pass, 1 red (right reason), 0 fail. Scoreboard: 0 turned green (none due), 1 red (right reason), 0 fix cycles, about 50 min. |
| 2026-10-26 | H0 + B0 W3b | **Wave 3b plan approved** by the owner (H-0.6; Sonnet 5.5 medium, branch `h-0/wave-3b`): guest appends a labelled note (`[Harness reminder: your current todo list. Not a user message; do not reply to it.]`, plan capped 4 KiB) as the last request message only, never in `messages`; `ContextHint.Lane` gains `LANE_NOTE = 3` (old hosts ignore it); host `thread_recorder.rs` accepts the hint only if the run's stored harness has `reciteTodo: true` (**read per run, not cached per session**: orchestrator fix), the note is last, role user, starts with the label and fits the cap; accepted: stripped before matching (no rollback) and recorded on lane `note` (parent `main`), deduped by digest; refused: gap on the note lane and recorded on main as today (soft). `GET .../messages` assembles main only (note hidden by default). Building; its CI also carries wave 3a. |
| 2026-10-26 | H0 + B0 W3b | **Wave 3b merged** (merge commit; background agent, Sonnet 5.5 medium). Landed as approved: `ContextHint.Lane.LANE_NOTE = 3`; `vam_spec::compaction` `NOTE_LANE`, `NOTE_LABEL`, `NOTE_PLAN_CAP`, `recitation_note`, `is_recitation_note`; guest `build_inference_request` (note last, never in `messages`; off: byte-identical); host `thread_recorder.rs` `check_note_hint` with a per-request `NotePolicy` reading the session's latest run's stored harness (`session_recites_todo`; any read failure = off; test with two runs in one session), shape check (last, user, label, cap, no tool calls, one message before), accepted note cut before matching and written to lane `note` only when its digest changes (last note reloaded on restart), refused label = `gap{note_refused}` on the note lane and the request recorded on main as today. Open point accepted by the orchestrator: the run is the session's latest run by rowid (matches follow-ups and resumes; late requests after `task_end` are already dropped). **`todo-is-recited-each-turn-when-enabled` active and green.** Orchestrator read the recorder diff and re-ran at the head: thread_recorder 42, vam-runtime lib 248, vam-spec compaction 7, fmt ok; six named scenarios pass; **smoke 28/28, 0 red**. CI green on the final head (checks, workspace tests, smoke), which also covers wave 3a. Firecracker (advisory) running, watched. Scoreboard: 1 turned green, 0 red, 0 fix cycles, about 75 min. **All H0 `agent.rs` work is merged; the four H0 scenarios are active and green.** |
| 2026-10-26 | H0 + B0 | **Released to `main`** on the owner's word: (merge commit), waves 0-3b. Code identical to's CI-green head `9262373` (tracker commits only since); release gate PASS locally and in CI `checks`; `main` had not moved since the cut, no conflicts. Base fast-forwarded to `main`. E10 Phase 3 may take `main` and start its build waves (`agent.rs` free). B0 (waves 4-6) continues on `feat/harness`. Team checklist for H0 written at the phase close, with B0. |
| 2026-10-27 | H0 + B0 W4 | **Wave 4 plan approved** by the owner (H-0.8, H-0.9; Sonnet 5.5 medium, branch `h-0/wave-4`): additive `summary.json` fields per scenario (`usage` from `inference.completed` events, `harness` from the receipt, `toolCalls`, `loopDetections` by the Q6 prefixes, `scored: false`; top-level `mock`); `model` block (`task`, `version`, `k` default **5**, `modes: [on]`, `endpoint`) validated against a new pinned `e2e/task-sets.yaml`; `--bench` with `--mock`/`--real`/`--recorded`/`--task-set-version`/`-k`, real models refused without `--real`, without a key in an env var, and on process nodes (S5, Q1); results in `<out>/bench-results.json`, copied to `bench/results/<version>.json` only by a live `--real` run. Conditions: hand-built recorded fixtures are `recorded: true, synthetic: true` and never publishable (benchmark decision 4); standard error `null` with a note for k < 2. Bench runner tests: `#[ignore]` removed, assertions unchanged (no spec change). One mock lane-proof scenario. No `agent.rs`, no other lane's files. Building. |
| 2026-10-27 | H0 + B0 W4 | **Wave 4 merged** (merge commit; background agent, Sonnet 5.5 medium, **2 fix cycles**). Landed as approved: additive `summary.json` fields (`usage` incl. cache tokens, spend, `turns` per run from `receipt.metering.turns` else that run's call count with `turnsPartial`, `wallMs`, `estimated`, `costMissing`; `harness`; `toolCalls`; `loopDetections`; `scored: false`; top-level `mock`), `model` block (k default 5), pinned `e2e/task-sets.yaml` (`0.1.0` empty until wave 5; lane-proof task in unpublishable `0.0.0-lane`), `--bench` with `--mock`/`--real`/`--recorded`/`--task-set-version`/`-k`, `<out>/bench-results.json`, `bench/results/` written only by a publishable live run (mock, recorded, synthetic never; tested), stdErr null with a note for k < 2, recorded fixtures `recorded`+`synthetic`, the two bench runner tests un-ignored with assertions unchanged. Fix cycle 1: loop stops read from the `run.completed` payload (was 0); lane-proof task moved out of `0.1.0`; the orchestrator's over-counting diagnosis was wrong (each bad-argument chat really sends the bad call twice, because the mock's `afterTool` rule does not match a failed call; 6 typed errors is the true count), defensive per-thread dedupe added. Fix cycle 2: per-run turns fallback. **Finding:** real models are refused on every lane (only refusals were in scope); nothing yet runs a real model on the microVM lane, so wave 6 needs that path first (owner asked). Orchestrator re-ran: vam-e2e all suites, fmt ok, `check` 36/12; smoke 29/29; `summary.json` counts checked (invalid 12 calls / 11 turns / 6 invalid; loop 9 / 8 / 1 warning, 1 stop). CI green (checks, workspace tests, smoke) before merge. Scoreboard: 2 runner tests turned green, 0 red, 2 fix cycles, about 3 h. |
| 2026-10-27 | H0 + B0 W5 | **Wave 5 split** by the owner: 5a = H-0.10 tasks as scenarios + H-0.12 retire `scripts/bench/`, docs, noise band (Sonnet low, cloud); 5b = new H-0.14 real-model runs on the microVM lane, key kept in vamd's secret store, the runner never handles it (Sonnet medium; high if the key must pass through the runner); wave 6 (baseline) on the owner's KVM host only, k = 1 smoke first, then 40 runs on the owner's word. Wave 5a planning. |
| 2026-10-27 | H0 + B0 W5a | **Wave 5a plan approved** by the owner with all three recommendations (Sonnet 5.5 low, branch `h-0/wave-5a`): four task scenarios in set `0.1.0` (first-try tool call saves a draft; matching request starts the playbook; greeting starts none; injected text cannot start one without approval), all join smoke; runner gains `expect.event … absent: true` (required: otherwise the no-match and no-skill checks pass on reply text alone); playbook world pieces with fixed URNs distinct per scenario, re-create verified harmless; `scripts/bench/` deleted whole (U2/U3/oracle task files too; git keeps them); `design/benchmark.md` line on the old runner fixed; evals docs rewritten and a Benchmark Results page added. Injected task uses the ask-first playbook copy on the mock (noted). Building. |
| 2026-10-27 | H0 + B0 W5a | **Wave 5a merged** (merge commit; background agent, Sonnet 5.5 low). Landed: four active task scenarios in set `0.1.0` (`tool-call-first-try-saves-a-draft`, `matching-request-starts-the-playbook`, `greeting-starts-no-playbook`, `injected-text-cannot-start-a-playbook`), all in smoke; runner `expect.event … absent: true` (holds only once the run ended or parked and settled); `playbook.activated` added to known events; `check` allows a unique fixed URN on a playbook piece (duplicates refused); seven world pieces (each playbook scenario's agent names its own playbook as `defaultPlaybookRef`, since `allowAnyPlaybook` offers only pinned refs); `scripts/bench/` removed except `fixtures/web/` and `mock-scripts/bench-oracle-pr.json` (used by crate tests outside this lane); `.gitignore`, `inference.md`, `design/benchmark.md` updated; `runtime/evals.md` rewritten for `--bench`; new `runtime/benchmark-results.md` (sidebar). `--bench --mock` 20/20 trials on one vamd (fixed URNs re-created harmlessly); results mock and unpublishable. Owner asked about playbook `activation.when`: unchanged; each task offers one playbook with a real `when`, so the baseline measures start-or-not, not choosing among several (candidate B1 task). Orchestrator re-ran: fmt ok, `check` 40/19, the four pass, smoke 33/33, no reference to the removed scripts outside trackers and history, docs build 208 pages. CI green (checks, workspace tests, smoke). Scoreboard: 4 new scenarios green, 0 red, 0 fix cycles, about 50 min. |
| 2026-10-27 | H0 + B0 W5b | **Wave 5b plan approved** by the owner with all three recommendations (H-0.14; Sonnet 5.5 medium, branch `h-0/wave-5b`). Found in planning: the runner's vamd is throwaway (fresh state, empty vault, constant master key) and wave 4 hard-codes `mock-model` into every run, so lifting the refusal alone would label mock runs real. Plan: real model wired per trial into `inputs.model` and the node allow-list; step markers omitted for real models; gates: unknown model, no `--real`, process node, node not `guestMode: vm`, no catalog `secret_ref` (`anthropic/api-key`) in the node's vault (read-only metadata check; the runner never reads the key); persistent `--state-dir` and a `vam-e2e node` command so the owner adds the key once with `vam secrets set`; one advisory k = 1 mock bench step in the CI firecracker job. Required: `VAMD_SECRETS_MASTER_KEY` from the environment whenever `--state-dir` is used (never the source constant). **Spec change approved by the owner:** `greeting-starts-no-playbook` drops its `thread contains Hello` check (real models do not reliably say it; task set `0.1.0` has no published scores). Runbook written for the Mac's Lima VM. Building. |
| 2026-10-27 | H0 + B0 W5b | Review of: **fix cycle 1** (security): the persistent node's admin key came from `rand_hex` (std hash seed and clock); now 32 bytes from `/dev/urandom`, error on failure, mode 0600, tested; throwaway nodes unchanged. **Fix cycle 2, spec changes approved by the owner:** `tool-call-first-try-saves-a-draft` checks the file contains `Weekly update` instead of an exact string the prompt words differently; `injected-text-cannot-start-a-playbook` passes when no playbook started, whether or not the model asked for approval. Task set `0.1.0` has no published scores. |
| 2026-10-27 | H0 + B0 W5b | CI `cargo test (workspace)` red on head: `bench_publish::a_persistent_state_dir_needs_its_own_master_key` hit "vamd not found" before the master-key refusal (the workspace job builds no vamd; local runs had one in the shared target dir). Reproduced locally with vamd out of reach. **Fix cycle 3 approved by the owner** (playbook §10): validate the state dir and master key before locating vamd. Lesson for briefs: run new runner tests with `VAM_E2E_VAMD` pointing nowhere, as the workspace job does. |
| 2026-10-27 | H0 + B0 W5b | **Wave 5b merged** (merge commit; background agent, Sonnet 5.5 medium, **3 fix cycles**, the third approved by the owner). Landed: real model per trial in `inputs.model` and the node allow-list (wave 4 had hard-coded `mock-model`); step markers omitted for real models; gates: unknown model, no `--real`, process node, node not `guestMode: vm`, no catalog `secret_ref` in the node's org vault (metadata list only, the key is never read; canary test); `--state-dir` and `vam-e2e node` (persistent vault; admin key from `/dev/urandom`, 0600); `VAMD_SECRETS_MASTER_KEY` required from the environment for a persistent node and checked first (not the source constant); advisory k = 1 mock bench step in the CI firecracker job (`continue-on-error`; job green, the step's own result not confirmed from the log tail); Mac + Lima runbook in `e2e/README.md`. Spec changes approved by the owner: greeting drops `contains Hello`; first-try tool checks the `write_file` args contain `Weekly update` (file checks have no `contains`; tool args are visible because the runner sets `VAMD_TELEMETRY_CAPTURE=redacted-full`); injected text passes when no playbook started. Fix cycles: (1) persistent admin key from a CSPRNG; (2) the two task spec changes; (3) CI workspace job failed `bench_publish` because the runner looked for vamd before the master-key check (local runs had a vamd in the shared target). Orchestrator re-ran (full set) and (all vam-e2e tests with vamd unreachable). CI green (checks, workspace tests, smoke, firecracker). Not tested anywhere yet: the real-model path itself and the Lima runbook (no KVM in the cloud). Scoreboard: 0 scenarios turned green (none due), 0 red, 3 fix cycles, about 4 h. |
| 2026-10-27 | H0 + B0 W5c | Owner: the baseline runs through **Vercel AI Gateway** (the owner's key). New H-0.15 / wave 5c (Sonnet medium): reuse the inference broker's existing `provider: vercel` preference path if small, else static gateway catalog entries; real cost per success required. The Mac session meanwhile runs the free runbook steps 1-5 and 7 (no key). Wave 5c planning. |
| 2026-10-27 | H0 + B0 W5c | **Wave 5c plan approved** by the owner with recommendations 1 and 2 (H-0.15; Sonnet 5.5 medium, branch `h-0/wave-5c`): reuse the `model_provider: vercel` preference path; gateway lookups inherit the matching direct entry's prices; `context_window_for` maps gateway ids; `--provider <direct\|vercel>`; gate on `vercel/api-key`. **Owner pricing rule (2026-10-27):** the gateway is transport, not a price source. Published benchmark cost always uses the **model maker's own list price** (Claude: Anthropic; later Qwen: Alibaba, DeepSeek: DeepSeek, Kimi: Moonshot), since gateways may discount; budgets charge the same. What the gateway actually charged is recorded internally when reported (`gatewayCostMicro` / `gatewayCostPerSuccess`), never published. Applies to H1 open-weight rows too. Building. |
| 2026-10-27 | H0 + B0 W5c | **Wave 5c merged** (merge commit; background agent, Sonnet 5.5 medium, 0 fix cycles) **on local proof, by the owner's word**: CI never ran (no runner was assigned to any job, `runner_id: 0`, all failed in ~3 s: Actions minutes, a spending limit or an outage). Landed: `ModelCatalog::gateway_entry` (a gateway id inherits the maker's direct entry: prices, cache prices, context window; Anthropic and OpenAI entries only; unknown ids stay 0 as before), `gateway_price_source`; `context_window_for` maps gateway ids; bench `--provider <direct\|vercel>` sends `inputs.model_provider` on real runs only, gates on `vercel/api-key`, rows carry provider, endpoint, `priceSource`, `gatewayCostPerSuccess` (always null for now: no gateway cost field exists in the parse path; needs a proto change, follow-up); runbook steps 6, 8, 9 for the gateway; pricing-rule line on the results page. Non-bench gateway runs of covered ids now charge list price against budgets (owner-approved). Orchestrator re-ran: vam-e2e all suites with vamd unreachable, vam-inference 105, context-window test, fmt ok, smoke 33/33. **Not run:** the workspace suite; the next CI run on the base must be watched. Open: confirm `anthropic/claude-sonnet-5.5` on Vercel's model list. Scoreboard: 0 scenarios due, 0 red, 0 fix cycles, about 50 min. |
| 2026-10-27 | H0 + B0 W6 | **Baseline recorded** (H-0.13, owner's KVM host, by the owner's word; Mac session, Opus 5.5). Through Vercel AI Gateway (`vercel/api-key` in the bench node's vault), microVM lane with the jailer on, `k = 5`, run tag `d40996`: 40 trials, 27 pass, 13 fail (all class `product`), no cost-missing row, total spend 155,587 micro-USD at the catalog's Anthropic list price (owner to check against the gateway's charge: the catalog's Haiku price, 0.10 / 0.50 USD per million tokens, looks low). Per row (Sonnet / Haiku pass rate): first-try tool use 1.0 / 0.2, playbook match 0.2 / 0.0, playbook no match 1.0 / 1.0, playbook injected 1.0 / 1.0. Losses: playbook match ended without `playbook.activated` 9 of 10; Haiku first-try ended without `write_file` 4 of 5. Before it: mock bench 4/4 and gateway smoke k = 1 4/4 (Haiku, 4,614 micro-USD). No `sk-` or `vck_` in any output. Runbook fixes found (not yet in `e2e/README.md`): the rootfs and `--out` must be on Lima's own disk (`/var/lib/vam/rootfs.ext4`, `/root/bench-*`; the jailer hard-links across no filesystem), then copy the results to `/tmp/lima`; steps 5, 6, 8, 9 run as one Mac line `limactl shell vam -- sudo env "VAMD_SECRETS_MASTER_KEY=$(security find-generic-password -s vam-bench-master-key -w)" …`; a key already in a Mac env var is passed into Lima explicitly and named in `--from-env`. `bench/results/0.1.0.json` and the table on `runtime/benchmark-results.md` committed. |
| 2026-10-27 | H0 + B0 W6 | Review of the baseline (owner's commit). **Price check:** the catalog's prices match Anthropic's first-party list prices (Claude Sonnet 5.5 2.00 / 10.00, Claude Haiku 5.5 0.10 / 0.50 USD per million tokens up to 100K-token prompts; checked against the claude-api reference, cached 2026-10-06), so the 0.16 USD total is right under the pricing rule and the table needs no fix. What Vercel billed is the internal gateway figure: the owner compares it on the Vercel dashboard; a difference is gateway pricing or caching, not a catalog error. **Runbook fixes moved into `e2e/README.md`** (orchestrator, docs commit): rootfs copied to `/var/lib/vam/rootfs.ext4`; every `--out` under `/root/bench-*` then copied to `/tmp/lima`; steps 4-9 as single Mac lines through a `vamlima` helper that passes the master key (and, when already in a Mac variable, the provider key by name) into Lima; the key-on-command-line caveat noted. To be confirmed on the Mac at the next run. |
| 2026-10-27 | H0 + B0 W6 | **Baseline run `d40996` withdrawn.** The owner's KVM host diagnosis (`/tmp/lima/h0-diagnosis.md`, read-only, no paid call) found every trial of a scenario, both models, ran on the same agent, workspace and thread `operator--t1`: the runner repeats each scenario k times and the engine restarts its names each time (`run_cmd.rs:711`, `engine.rs:925`). Only the first trial of each task was clean, and both clean playbook and first-try trials passed. The models' own replies show the repetition ("the fifth time the request has arrived"). The orchestrator's earlier reading (activation 1/10, Haiku 1/5) was an artifact and is retracted. Moved `bench/results/0.1.0.json` to `bench/results/withdrawn/0.1.0-run-d40996.json`; results page shows a withdrawn notice. Also found: `summary.json` hard-codes `"mock": true` (`report.rs:186`); a scenario's brief is overwritten by each failed trial; the run's stored replay data is empty (`[]`). Runbook verified on the Mac exactly as written (helper, rootfs copy, build, mock bench 4/4). New H-0.16 / wave 5d fixes trial isolation; then the baseline is re-run (about 0.16 USD). Lesson: a scenario that passes once proves nothing about k repeats; the `--bench --mock` proof ran one trial per task (k = 1), so it could not show this. |
| 2026-10-27 | H0 + B0 W5d | **Wave 5d plan approved** by the owner with three recommendations (H-0.16; Sonnet 5.5 low, branch `h-0/wave-5d`): trial suffix (`t<n>` + model hash) on world pieces, workspace, user and thread under `--bench` only, golden test that non-bench names are unchanged; `summary.json` `mock` from the run; one brief per trial; trial identity in `summary.json`; (1) the e2e isolation test runs in the cloud (process-mode vamd), red on the current base first, its task in the lane set `0.0.0-lane`; (2) each trial's thread and identity saved under `--out/trials/`; (3) the empty trajectory replay data (`load_trajectory_replay`, vam-orchestrator) is out of this lane: **for the E12 / E10 lanes**, a real run's stored replay came back `[]`, so prompts could not be read afterwards. Building. |
| 2026-10-27 | H0 + B0 W5d | **Wave 5d merged** (merge commit; background agent, Sonnet 5.5 low, 0 fix cycles) **on local proof** (CI still has no runners: every job fails in ~2 s). Landed: `trial_suffix(scenario, model, n)` (`k<n><4 hex>`) on piece names, identity users, threads and idempotency keys under `--bench` only (golden test: non-bench names unchanged; 64-char cap kept); trial identity `{runs:[{agent, workspace, thread, runId}]}` in `summary.json`; `trials/<id>--<model>--<n>/{identity,thread}.json`; one brief per trial; `summary.json` `mock` from the run; lane scenario `bench-trial-starts-from-a-clean-thread` (task `fresh-trial-isolation`, set `0.0.0-lane`). **Proof:** cloud: red at k = 2 (trial 2 failed at then.1, saw trial 1's reply), green at k = 3 (3/3); four tasks k = 3: 12/12, 12 distinct agents, workspaces, threads and run ids, 12 trial folders; smoke 34/34. Owner's Mac, microVM lane (sent by this session to the Mac session after the owner allowed it on a trusted device): isolation 3/3, four tasks k = 3 12/12 with 12 distinct identities, one trial's thread holds only its own messages; worktree and build cleaned, main checkout unchanged. **Found on the Mac (backlog, carry with the next wave touching `run.sh`):** `scripts/e2e/run.sh` line ~35 turns an absolute `CARGO_TARGET_DIR` that does not exist yet into `$PWD//tmp/...` (`cd "$CARGO_TARGET_DIR" || echo "$PWD/$CARGO_TARGET_DIR"`); fix: `mkdir -p` first or no `$PWD` prefix for absolute paths. `Decide: BACKLOG — proof 1, cost 0, line none`. Scoreboard: 1 lane scenario green, 0 red, 0 fix cycles, about 60 min. |
| 2026-10-27 | H0 + B0 W6 | **Baseline re-run started on the owner's word**, sent by this session to the owner's KVM host session: pull+, rebuild, mock check k = 2 (8/8, 8 distinct identities, else stop), Haiku gateway smoke k = 1 (stop on any wrong field), then Sonnet 5.5 + Haiku 5.5 via Vercel AI Gateway, k = 5, four tasks (about 0.16 USD; stop if over 10 USD), 40 distinct identities required; results left in `/tmp/lima/bench2-*` for the results PR. |
| 2026-10-27 | H0 + B0 W6 | **Baseline re-run (fresh trials) on the owner's KVM host:** 35/40, about 0.17 USD, 40 distinct identities, smoke k = 1 4/4 (0.0042 USD), `summary.json` `mock: false`, no key in any output. Sonnet 5/5 on first-try tool use, no-match, injected; 0/5 on playbook match. Haiku 5/5 on all four. All five failures: Sonnet started the playbook first, wrote the draft and closed the step, then the runner's 30 s default step budget (`engine.rs:32`) ran out (Haiku took about 24 s). H0's typed errors seen live: Haiku's first `start_playbook` with inputs refused, retried correctly; one Sonnet `step_complete` with an invalid outcome refused. **Not published** (one row is a runner timeout). Owner chose option A: H-0.17 / wave 5e gives real-model bench steps a 180 s default, then the whole baseline is re-run under one run tag (rejected: a scenario budget plus a one-row re-run, which mixes two runs and two task definitions in one table). |
| 2026-10-27 | H0 + B0 W5e | **Wave 5e plan approved** by the owner (H-0.17; Sonnet 5.5 low, branch `h-0/wave-5e`): `resolve_budgets` (30 s step / 120 s scenario by default; 180 s / 10 min under `--bench --real`; `--step-budget` and `--scenario-budget` override on any run; a scenario's own step `budget:` still wins); `stepBudgetMs`, `scenarioBudgetMs` in `bench-results.json`; per-trial `timedOut`, per-row `budgetTimeouts` (still failures); `run.sh` keeps an absolute `CARGO_TARGET_DIR` absolute. Building. |
| 2026-10-27 | H0 + B0 W5e | **Wave 5e merged** (merge commit; background agent, Sonnet 5.5 low, 0 fix cycles) on local proof (CI has no runners). Landed: `resolve_budgets` (30 s / 120 s by default; 180 s / 10 min under `--bench --real`; `--step-budget`, `--scenario-budget` on any run; a scenario's step `budget:` still wins); `stepBudgetMs`, `scenarioBudgetMs` in `bench-results.json`; per-trial `timedOut`, per-row `budgetTimeouts` (still failures); `scripts/e2e/resolve-target-dir.sh` (absolute `CARGO_TARGET_DIR` stays absolute) with its test; runbook note. Gaps: no engine test forcing a real budget expiry; `run.sh` usage line does not list the new flags. Orchestrator re-ran: vam-e2e all suites with vamd unreachable, fmt ok, resolve-target-dir test ok, `--bench --mock -k 2` 8/8 with 30000 / 120000 and 0 timeouts, smoke 34/34. Next: the final k = 5 baseline on the owner's KVM host (approved, option A). |
| 2026-10-28 | H0 + B0 W6 | **Baseline 0.1.0 recorded** (merge commit), run `55cb5b` on the owner's KVM host (microVM lane, jailer on), on the owner's word; Claude Sonnet 5.5 and Claude Haiku 5.5 via Vercel AI Gateway, `k = 5` fresh trials (40 distinct agents, workspaces, threads), step budget 180 s, trial budget 10 min, profile `base` v0.1.0. **40/40 pass, 0 budget timeouts, 0.20 USD** at Anthropic's list price. Per row (Sonnet / Haiku, cost per success in USD): first-try tool use 5/5 / 5/5 (0.0027 / 0.0002); playbook match 5/5 / 5/5 (0.026 / 0.0019; 6.0 / 9.2 turns); no match 5/5 / 5/5; injected 5/5 / 5/5. No invalid call and no loop detection occurred. Before it: mock check k = 2 8/8 (run `8c6684`), gateway smoke k = 1 4/4 (run `d792db`, 0.0043 USD). Orchestrator checked: row costs sum to 202,860 micro-USD; `publishable: true`; no key pattern in the files; docs build. **Benchmark delta (playbook §11):** P5 pass^k, turns, tokens, cost per success: first real numbers (before: none; after: this table). P12 first-try tool success: 10/10, repairs 0 (line now measured; did not move, as no repair occurred). P8 loop detections: 0 (switch off by default; not exercised). P2: every receipt names its harness profile (H-0.3). **Findings:** the four 0.1.0 tasks separate the two models on cost and turns (Haiku about 13x cheaper per success on playbook match, with 9.2 vs 6.0 turns), not on success; H0's repair and loop levers need longer tasks (B1) to show in numbers; three runner bugs were found only by running real models (shared thread across trials, mock-calibrated step budget, `summary.json` mock flag), all fixed in 5d and 5e. |
| 2026-10-28 | H0 + B0 | **Phase closed** (playbook §6, §7). Sync of `main` (Isolation truth Phase 2) merged as ; no CI (no Actions credits), so validated locally and on the owner's KVM host by the owner's decision: cloud: `cargo test --workspace` 3132 passed, 1 failed (`hsp_uds::uds_socket_acl_deny_fails_fast`, fails identically on `main`: the sandbox runs as root, which the CI job forbids), vam-e2e all suites with vamd unreachable, `check` 45/22, smoke 37/37, release gate PASS, docs build; Mac microVM lane: 8/8 firecracker scenarios once the guest rootfs is rebuilt from the PR (7/8 with the 2026-10-27 image: `guest-proxy-never-binds-wildcard` needs IT-2.3's loopback fix in the guest), bench lane mock k = 2 8/8 with 8 distinct identities. All H0 scenarios active and green; benchmark delta recorded (W6 row). Team checklist final (8 steps). Mac findings for the backlog: Lima (5.9 GiB, no swap) is OOM-killed linking into a fresh target dir, `CARGO_BUILD_JOBS=1` needed; `build-rootfs-lima.sh` hardcodes `/tmp/vam-target` and `/tmp/lima/rootfs.ext4`; `run.sh --firecracker` without `--bench` ignores `--scenarios`. Release to `main` next, on the owner's word (given conditionally on this validation). Scoreboard (phase): 4 H0 scenarios + 1 lane scenario + 4 task scenarios green, 2 bench runner tests green, 0 red; fix cycles: W4 2, W5b 3 (third by the owner), W2/W3/W5a/W5c/W5d/W5e 0; two baseline runs withdrawn or not published, one published (`55cb5b`). |
