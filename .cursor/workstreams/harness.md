# Workstream: Harness (H): levers, profiles, and the balance with the model

```yaml
status: active          # active | paused | done
slug: harness
current_phase: H1 (exit met 2026-10-29; release to main on the owner's word; checklist out)
base_branch: feat/harness
design_docs:
  - design/harness.md
  - design/benchmark.md
```

## Goal

Every lever the machine controls over the model is inventoried, switchable
per run, recorded on the run and the receipt, measured on the benchmark, and
pinned as a **harness profile per model class** (`frontier`, `open-strong`,
`open-small`, `mock`). Model and harness are a pair: the product is a measured
profile per model, not one harness. Done when H0 to H3 are closed with
evidence-backed profiles published with their benchmark rows, and H4
produces one accepted profile PR offline.

## Out of scope

- A second agent loop. Switches live in `run_react_agent_loop_with_trace` and
  its helpers.
- Run-time self-modifying harness (decision 8).
- RL or fine-tuning of models.
- Multi-agent orchestration: fan-out, delegation, background agents (E10
  Phase 5, E5, `agent-runtime.md` §X.2). The only sub-agent shape here is the
  fresh-context helper (L5).
- Levers that need control of the inference server (guided decoding, strict
  schemas, server-side parsers, logit masking) until a self-hosted row exists.
- Shipping any lever on by default without a benchmark row (decision 3).

## Locked decisions

**Confirmed by the owner 2026-10-26** (copied from `design/harness.md`). Do not re-litigate without editing this list.

1. **Every lever is a named switch with a default.** It lives in one
   `harness` struct on the run input, read where the loop builds its tool
   list, prompt and model request. The effective configuration is recorded
   on the run and printed on the receipt.
2. **Profiles are per model class, pinned in the catalog, versioned.** A run
   names its profile; a benchmark row names it too. Changing a profile is a
   release with a benchmark delta.
3. **A lever becomes a default only with a row.** No lever ships on by
   default without an ablation row on the benchmark showing its effect for
   the model class it is on for.
4. **`machine off` is the minimal functional loop, not a handicap.** Same
   tools, same Cedar, same caps and overflow protection; no playbook
   tracking, no ranking, no recitation, no summary compaction beyond what
   prevents overflow; the procedure as prose. (`benchmark.md` decision 2.)
5. **Errors are honest.** An invalid tool call never becomes `{}`. The model
   receives a typed, actionable error naming the field, and the repair counts
   in the metrics.
6. **The harness never loosens security.** Ranking, masking and deferral are
   presentation; Cedar remains the gate on every call; a masked tool is still
   denied if called. (E10 locked decision 1.)
7. **Model-specific handling lives in the catalog entry**, never in the loop:
   parser, thinking mode, `tool_choice`, strict or guided decoding, context
   budget, price.
8. **No self-modifying harness at run time.** Recursive harnessing runs
   offline against the benchmark and produces a proposed profile change for
   review. Runtime behaviour is what the pinned profile says.

B0 also works under `design/benchmark.md` locked decisions (proposed) 1-9,
in particular 3 (versioned task set), 4 (no scores from the mock), 5 (cost
per success) and 7 (the benchmark lane is the scenario runner).

## Coordination points

- **Receipt field for the effective harness profile.** E12 Phase 1 is on
  `main`  and `feat/harness` has taken it . Its
  receipt has **no harness or profile field**: none in
  `proto/vam/v1/receipt.proto` (`RunReceipt` fields 1-12) and none in
  `crates/vam-orchestrator/src/receipt.rs` `build` (lines 675-786). H0 adds
  it (item H-0.3): an addition to E12's receipt schema, one optional
  object, so older verifiers are unaffected (they read the emitted JSON).
  `gather` already parses the stored `RunSpec` (`receipt.rs:291-298`), so
  the line reads the effective configuration from there. The orchestrator
  tells the E12 orchestrator when the H-0.3 PR merges.
- **`crates/vam-runtime/src/agent.rs`.** E10 Phase 3 (steer) holds its build
  waves until H0 merges into `main` (`phase-playbook.md` §11.2). The
  orchestrator tells the owner when H0's release PR merges, so the E10
  orchestrator can take `main` and start building.

- **Receipt schema addition (for the E12 lane).** Wave 1 adds an optional
  `harness {profile, version, digest, overrides?}` object to the receipt
  (`receipt.proto` field 13, `receipt.rs` `build`); no existing field
  changes, receipts without it still verify. This lane does not edit
  `design/run-receipt.md` or `run-receipt.md`; the E12 orchestrator adds the
  line there when it next touches the schema.

## Status legend

- `[ ]` not started
- `[~]` in progress
- `[x]` done (code + tests, or merged)

## Phases

### Done phases

Earlier phase detail is in git history.

| Phase | Name | Done |
|---|---|---|
| H0 + B0 | Switches and honesty; baseline numbers (baseline 0.1.0, run `55cb5b`, 40/40) | 2026-10-28 |

### Phase H1 — Open-model adapters

**Goal:** open models run first-class through Vercel AI Gateway.  
**Depends on:** H0  
**Plan:** breakdown drafted 2026-10-28 (background agent, Opus 5.5 high), awaiting owner approval.  
**Exit (old):** ~~U1 and `close-prep` run on GLM-5.x and a small Qwen through the
gateway with 0 silent `{}` calls.~~ (U1 needs a real Notion connection;
`close-prep` is a B1 task with no world yet; H0 already made `{}` impossible.)  
**Exit (approved by the owner 2026-10-28):** the baseline task set
0.1.0 runs on the three gateway open-weight models with every row
publishable, invalid calls and repairs recorded, and the two default profiles
present as data. Follow-up, not this breakdown: the H1 row in
`design/harness.md` §6 gets the same line once the owner approves (design
docs left untouched here).

**Code map** (base, checked for this breakdown)

- Catalog: `crates/vam-inference/src/catalog.rs`. `ModelCatalogEntry` has
  prices, `context_window`, `supports_tools`, cache fields; **no adapter
  fields and no profile name**. `direct_entry_for_gateway` (`:268`) maps
  `maker/name` to a direct entry only when its provider is `anthropic` or
  `openai`; `gateway_entry` (`:280`) copies its prices; `gateway_price_source`
  (`:301`) prints `<provider> list price`. No Z.ai, DeepSeek or Alibaba entry
  exists, so the three open ids resolve at price 0 (`resolve`, `:331`) and the
  bench refuses them (`vam-e2e` `bench.rs:106` requires `gateway_entry`).
  Callers: `run_context.rs:64`, `vam-e2e` `usage.rs:174,190`, `bench.rs:106,153`.
- OpenAI-compatible stream: the **live path is the inline loop in
  `OpenAiProvider::infer`** (`provider.rs:726-830`), not `parse_sse_line`
  (`:536`, a second parser used by tests). Both read only `delta.content` and
  `delta.tool_calls`; `reasoning` / `reasoning_content` are silently dropped.
  `build_openai_body` (`:941`) sends `temperature` and `max_tokens` from
  `ModelPreference`, never `tool_choice` or an effort field. Anthropic drops
  thinking deltas (`:1362`) and must never send `tool_choice` or `temperature`
  (`:1064`, current Claude models reject them).
- Wire: `InferenceResponseChunk` (`proto/vam/v1/inference.proto:58`) has no
  reasoning field; `ChatMessage` has none; `ContextHint.reason` is a free
  string. The mock (`mock_script.rs`) builds chunks directly, so it never
  passes through the OpenAI parser.
- Broker: `InferenceBrokerServer` (`vam-inference` `broker.rs`) is one per
  session, built in `vam-orchestrator` `brokers.rs:833`; it resolves the entry
  per request (`broker.rs:378`) but does not know the run's harness. The
  thread recorder already reads the run's stored harness per request
  (`thread_recorder.rs:143`, `session_recites_todo`): the pattern to copy.
- Harness: `vam-spec` `harness.rs`. Profile `base` is two constants
  (`PROFILE_BASE`, `BASE_VERSION`) plus `Harness::default()`; `digest()` and
  `record()` hard-code `base`; `resolve()` always starts from `Default`. A
  caller's `profile` key is refused as unknown. `rule_for` makes every
  `adapter.*` and `tools.examples` `NotBuilt` (test
  `levers_not_built_accept_only_their_default`). Host resolution:
  `run_context.rs:426` `resolve_harness` (not `api/`).
- Guest: invalid calls are caught in `checked_tool_args` (`agent.rs:1342`,
  used at `:633`); main requests at temperature 0.1 (`:1651`); the tool list
  is built in `rebuild_tools_list` (`:2725`); builtin descriptions in
  `primitives.rs:635` `builtin_tool_specs`. A resample re-sends a request
  without the discarded assistant message, which the recorder logs today as
  `rollback{prefix_mismatch}` (`thread_recorder.rs:15`).
- Not touched: `host_tools.rs`, `crates/vam-orchestrator/src/api/`.

**Scenario delta**

| Scenario | Change | Layer | Given | Why |
|---|---|---|---|---|
| thinking-model-tool-calls-are-not-dropped | new | process (mock form) + `model` block (real form on the bench lane) | org-admin, agent-with-builtins | design §7: a reply with reasoning and a tool call runs the tool, no invalid call, reasoning size on `inference.completed`; mock reply gains `reasoning`; red at the first `then` (reasoning dropped, no field) |
| reasoning-is-recorded-and-never-replayed-when-stripped | new | process | org-admin, agent-plain | "never dropped silently": with `adapter.thinking: strip` the next request does not carry the reasoning (mock `anyContains` a marker answers `leaked` vs `clean`) and the run records it; with the default the request is byte-identical to today; red at the record step |
| invalid-call-is-resampled-once-before-a-typed-error | new | process | org-admin, agent-with-builtins | `adapter.resampleOnInvalid: 1`: an unparsable call is resampled (mock condition `resampled: true` gives a valid call) and the tool runs with no typed error in the thread, one resample recorded; a second chat whose resample is also bad gets today's typed error (one resample, never a loop); red: switch refused (`NotBuilt`) |
| tool-descriptions-carry-one-example-when-enabled | new | process | org-admin, agent-with-builtins | `tools.examples`: two chats, default and on; a mock condition `toolDescribed {tool, contains}` answers whether `read_file`'s description has its example; red: switch refused |
| named-profile-prints-on-the-receipt-with-its-own-digest | new | process | org-admin, agent-plain | decision 2: a run naming `open-small` gets profile `open-small`, its version and a digest unlike `base`, no overrides; one naming `open-small` plus `loop.detect: false` shows that override; red at the first `receipt` step (`profile` refused) |
| invalid-tool-arguments-return-a-typed-error, harness-switches-print-on-the-receipt | unchanged, must stay green | process | — | proof that defaults are unchanged: resample off, profile `base` when none is named |
| plain-chat-gets-a-reply | guards add `vam-inference` `provider.rs`, `broker.rs`, `catalog.rs`; steps unchanged | process | — | nothing guards the inference crate today |
| vllm-model-runs-with-its-catalog-parser | deferred (not written) | — | — | with server-side parsers, strict and guided: needs a self-hosted row |

**Event field names (owner, 2026-10-28, wave 0 approval):** `inference.completed` carries `data.reasoningBytes` (size of the reasoning the model returned, recorded under every `adapter.thinking` mode) and `data.resamples` (resamples made for that inference). Waves 2 and 3 adopt these names; any other name is a spec change. The thinking scenario also has a real-model file on the bench lane (`0.0.0-lane`, k 1, unscored), so the delta has six new files.

Also must stay green: the six scenarios guarding `agent.rs`, the seven
guarding `thread_recorder.rs`, and the four task scenarios of set `0.1.0`.
The real form of the thinking scenario has task `thinking-tool-call` in the
unpublishable lane set `0.0.0-lane` (Q8): a check, not a score.

**Benchmark delta**

| Line (`benchmark.md` §1, §3) | What H1 does to it |
|---|---|
| P12 first-try tool success: `invalidCalls`, `repairsPerSuccess` | first open-model numbers; resamples counted as invalid calls (H-1.6) |
| P8 `loopDetections` | recorded on open rows (on under the profiles, off under `base`) |
| P5 pass^k, cost per success | first open-model rows at the maker's list price |
| P10 self-hosting | unchanged: no self-hosted row (deferred) |

Baseline for the open models: **no row yet**. Task set `0.1.0`
(first-try-tool-use, playbook-match, playbook-no-match, playbook-injected),
`machine on` only, k = 5, step budget 180 s, through Vercel AI Gateway.
Non-regression for existing rows: H1 changes no default, so Claude Sonnet
5.5 and Claude Haiku 5.5 (run `55cb5b`, 40/40) are not re-run on paid models;
proof is `--bench --mock -k 2` on each merged wave head, compared to run
`8c6684` (8/8), plus smoke.

Paid run (wave 5, **owner's word**, within the owner's approved budget, on a KVM host microVM lane):

- Models (Q1): `zai/glm-5`, `deepseek/deepseek-v4-pro`, `alibaba/qwen3.6-27b`
  (defaults; ids, prices and windows checked in wave 1 before data lands).
- Profiles (Q9): each open model under **both** `base` and its catalog
  profile (`open-strong` for GLM and DeepSeek, `open-small` for Qwen): the
  first pair of rows showing what the profile does, which H2 needs anyway.
  Fallback if the owner wants it cheaper: `base` only.
- Runs: smoke k = 1 per model under its profile (12), then 4 tasks x 5 x 3
  models x 2 profiles = 120, plus the thinking check k = 1 per model (3).
- Cost: kept within the owner's approved budget, with a per-model cap;
  stop on any cap. Token counts from the run `55cb5b` (about 58k tokens per
  model per task set at k = 1) are the basis, doubled for more turns on open
  models, no cache discount.
- Results PR: open rows added to `bench/results/0.1.0.json` (Claude rows of
  `55cb5b` kept; each row names its run tag and profile, H-1.8) and a report
  like `0.1.0-report.md` with the key grep (no `sk-` / `vck_`).

**Items**

- [x] `H-1.1` — **Scenarios (wave 0).** The five new scenarios as `pending`
      with `expectFail`, the guard additions on `plain-chat-gets-a-reply`,
      and the test-only mock fields they need (`mock_script.rs`): reply
      `reasoning` (emitted as reasoning by the mock), conditions `resampled`
      (the request is labelled a resample) and `toolDescribed {tool,
      contains}`; task `thinking-tool-call` in `0.0.0-lane`. *Accept:*
      `cargo run -p vam-e2e -- check` clean; each new scenario red at the step
      named in the delta, class `product`; smoke unchanged. *Test:* `cargo
      test -p vam-inference mock_script`, `-p vam-e2e`, `scripts/e2e/run.sh
      --scenarios <the five>`, `--smoke`. *Turns green:* none.
      `Decide: NOW — proof 2 (the phase's scenarios), cost 1 (test-only mock fields), line P12 via the exit`
- [x] `H-1.2` — **Levers unlocked and profiles as data.** `vam-spec`
      `harness.rs`: `adapter.thinking` `drop` (default) | `strip` | `keep` |
      `off` | `catalog` (use the entry's mode); `adapter.toolChoice` `auto`
      (default, not sent) | `required` | `none`; `adapter.resampleOnInvalid`
      0-1; `adapter.effort` `default` (not sent) | `low` | `medium` | `high`;
      `tools.examples`. `parser`, `strict`, `guided` stay `NotBuilt`. A
      profile table next to `base` (data, Q3): `base` 0.1.0 unchanged (same
      digest), `open-strong` 0.1.0 and `open-small` 0.1.0 with every switch at
      a stated value (Q4), each `evidence: pending`; `digest()` and `record()`
      take the profile; a test that every profile value passes `rule_for`.
      vamd `resolve_harness`: a caller may name `profile` (unknown name: 400
      naming `profile`; `version`, `digest`, `switches` still refused); switches
      on top are overrides against that profile. A pending profile is never
      applied implicitly (decision 3, Q2): no name means `base`. *Accept:*
      `base` digest equals `55cb5b`'s (`sha256:…`); unit tests per new
      value and refusal; the H0 checklist step 5 (`{"profile":"x"}` gives 400)
      still holds. *Test:* `cargo test -p vam-spec harness`, `-p
      vam-orchestrator run_context`. *Turns green:*
      named-profile-prints-on-the-receipt-with-its-own-digest.
      `Decide: NOW — proof 3 (decision 2; the exit names the profiles; benchmark rows must name a profile), cost 1 (files of H-0.2), line P5 harness disclosure`
- [x] `H-1.3` — **Catalog entries for the three open models.** *(Code merged in: adapter block, profile name, any-maker derivation, maker-named price source, fixture tests. GLM-5 and Qwen3.6-27B landed in (catalog `2026-v2`, owner-approved prices); DeepSeek V4 Pro is open: the makers' pages were unreachable from the cloud session; they land once verified.)* Direct entries
      for the Z.ai, DeepSeek and Alibaba models (maker list price, cache price
      where listed, context window, `supports_tools`), with a new `adapter`
      block (Q5: reasoning field name, thinking mode `strip` | `keep` |
      `off`, how to turn thinking off, `tool_choice` and effort support and
      their wire names) and a profile name; `direct_entry_for_gateway` covers
      the makers that have direct entries (not a fixed list of two);
      `gateway_price_source` names the maker (`zai list price, catalog …`);
      catalog version bumped. Ids, prices and windows checked on Vercel's
      model list and the makers' price pages in the wave before the data is
      committed (Q1). Existing entries unchanged. *Accept:* unit tests: each
      open id resolves through `provider: vercel` at the maker's price, its
      price source names the maker, Claude and OpenAI gateway entries
      byte-identical; bench gate accepts the ids. *Test:* `cargo test -p
      vam-inference catalog`, `-p vam-e2e bench`. *Turns green:* none alone.
      `Decide: NOW — proof 3 (no open row without entries; cost per success must be real), cost 1, line P5 cost per success`
- [x] `H-1.4` — **Reasoning content on the OpenAI-compatible path.** Both
      parsers read `reasoning` and `reasoning_content` deltas into a
      host-only `InferenceResponseChunk.reasoning` field (one optional proto
      field; the broker never forwards it to the guest in H1). The broker
      reads the run's stored harness per request (pattern of
      `session_recites_todo`) and applies the effective mode with the
      entry's adapter: `drop` and `strip` both keep reasoning out of the
      model context; `drop` records its size on `inference.completed`
      (`reasoningTokens`, provider-reported, else estimated), `strip` also
      keeps the text in the thread log off the main lane; `off` sends the
      entry's disable parameter; `keep` per Q6. Tool-call markup found inside
      reasoning is recorded and counted, never silently lost; the entry of a
      model that does this has thinking `off`. The mock emits reply
      `reasoning` through the same broker step. Anthropic thinking stays as
      today. *Accept:* SSE fixture tests per field name, per mode; default
      mode leaves requests and guest-visible chunks byte-identical (test);
      no reasoning reaches the guest. *Test:* `cargo test -p vam-inference`,
      `-p vam-orchestrator brokers`, scenarios. *Turns green:*
      reasoning-is-recorded-and-never-replayed-when-stripped,
      thinking-model-tool-calls-are-not-dropped (mock form).
      `Decide: NOW — proof 3 (a thinking model's call lost in reasoning is the silent failure decision 5 forbids; exit rows), cost 1 (one host-only proto field; same pattern as H-0.6), line P12 invalidCalls`
- [x] `H-1.5` — **`tool_choice` and effort passthrough.** `build_openai_body`
      sends `tool_choice` when the run's value is not `auto` and the entry
      supports it, and the entry's effort field when the value is not
      `default`; unsupported means not sent and recorded on
      `inference.completed`. Anthropic body unchanged. *Accept:* body unit
      tests per value and per support flag; defaults byte-identical. *Test:*
      `cargo test -p vam-inference provider`. *Turns green:* none (unit proof;
      no profile sets them in H1, Q7).
      `Decide: NOW — proof 2 (H2 grid levers; decision 7), cost 0 (same files and broker policy as H-1.4), line P12 via H2`
- [x] `H-1.6` — **One resample on an invalid call.** In the tool round, with
      `adapter.resampleOnInvalid: 1`, an invalid call (H0's typed-error cases)
      is not shown to the model; the same request is sent once more at a
      higher temperature (Q10), labelled a resample (Q11). A valid resample
      runs as normal; an invalid one gets today's typed error. The host
      recorder accepts the label only when the run's harness has the switch,
      keeps the discarded call as a `resample` record (no
      `rollback{prefix_mismatch}`), and `summary.json` / the results file
      count resamples as invalid calls plus a `resamples` field. Off: no
      change (unit test). *Accept:* guest unit tests (valid, invalid twice,
      off); recorder tests (accepted, refused label, switch off). *Test:*
      `cargo test -p vam-runtime -p vam-orchestrator thread_recorder -p
      vam-e2e`, scenarios. *Turns green:*
      invalid-call-is-resampled-once-before-a-typed-error.
      `Decide: NOW — proof 2 (W lever for open-small; P12), cost 1 (agent.rs and recorder patterns from H0), line P12 repairsPerSuccess`
- [x] `H-1.7` — **One example per built-in tool description.** With
      `tools.examples`, each builtin (`primitives.rs` `builtin_tool_specs`) and
      guest intrinsic (`task_complete`, playbook tools) gets one short example
      call appended to its description where the tool list is built; MCP and
      host tools unchanged (Q12). Off: descriptions byte-identical (cache
      prefix unchanged). *Accept:* unit test per tool (example valid against
      its own schema); off identical. *Test:* `cargo test -p vam-runtime`,
      scenarios. *Turns green:* tool-descriptions-carry-one-example-when-enabled.
      `Decide: NOW — proof 2 (L2 lever for weak models; P12), cost 0 (agent.rs already in wave 3), line P12 first-try tool success`
- [x] `H-1.8` — **Bench: profiles and open rows.** `vam-e2e run --bench
      --profile <name>` sends `inputs.harness.profile`; each row carries its
      profile, version, digest and run tag; a results file for one task-set
      version can hold rows of several runs (the Claude rows of `55cb5b` stay);
      the gateway gate accepts the open ids; runbook steps for the open models
      in `e2e/README.md`. *Accept:* runner tests (profile in rows, merge keeps
      old rows, refusal of an unknown profile); `--bench --mock -k 2` 8/8.
      *Test:* `cargo test -p vam-e2e` with vamd unreachable, `scripts/e2e/run.sh
      --bench --mock -k 2`. *Turns green:* none (runner proof).
      `Decide: NOW — proof 3 (the exit's rows need it), cost 1, line P5`
- [x] `H-1.9` — **Open-model rows (no code).** On the owner's word, on a
      KVM host: the paid run above; results PR with `bench/results/0.1.0.json`
      and the report. *Accept:* every open row publishable, no cost-missing
      row, invalid calls and repairs on every row, losses shown.
      `Decide: NOW — proof 3 (the exit), cost 1 (one paid session), line P5, P12`

**Waves**

| Wave | Items | Turns green | Repo | Model | Budget | Depends on | Status | PR |
|---|---|---|---|---|---|---|---|---|
| 0 | H-1.1 | — (red, right reason) | vam | Sonnet low | 60 min | — | [x] | — |
| 1 | H-1.2, H-1.3 | named-profile-prints-on-the-receipt-with-its-own-digest | vam | Sonnet medium — a schema change read by vamd and the guest, plus catalog data across two crates | 120 min | 0 | [x] | (entries open) |
| 2 | H-1.4, H-1.5 | reasoning-is-recorded-and-never-replayed-when-stripped, thinking-model-tool-calls-are-not-dropped (mock) | vam | Sonnet medium — a proto field, the broker reading the run's harness, two stream parsers | 120 min | 1 | [x] | — |
| 3 | H-1.6, H-1.7 | invalid-call-is-resampled-once-before-a-typed-error, tool-descriptions-carry-one-example-when-enabled | vam | Sonnet medium — the loop's tool round plus the host recorder | 120 min | 2 | [x] | — |
| 4 | H-1.8 | — (runner tests; mock) | vam | Sonnet low | 60 min | 1 | [x] | — |
| 5 | H-1.9 | — (numbers) | vam | owner on a KVM host; Sonnet low for the results PR | 60 min | 2, 3, 4, owner's word | [x] | (DeepSeek open) |

Branches `h-1/wave-N`, PRs into `feat/harness`. **Wave 3 holds every
`agent.rs` and `instruction.rs`/`primitives.rs` change**: it takes `main`
first, does not build while an E10 Phase 3 wave is building in `agent.rs`
(conflict rule, `phase-playbook.md` §11.2), and on merge the orchestrator tells
the E10 Phase 3 orchestrator the PR number. Waves 1, 2 and 4 do not touch
`agent.rs`. Wave 4 may run before 2 and 3 (it needs only wave 1).

**Open questions** (each has a default)

1. **Model ids, prices, windows** (not verifiable from this session:
   `ai-gateway.vercel.sh` is outside its network). **Default:**
   `zai/glm-5` (Z.ai list 1.00 / 3.20 USD per M, about 200K window),
   `deepseek/deepseek-v4-pro` (1M window; reports differ, 0.435 / 0.87 or
   1.74 / 3.48), `alibaba/qwen3.6-27b` (open weights; price unknown, cap
   assumes up to 0.40 / 2.40). Wave 1 checks each on Vercel's list and the
   maker's page and uses a newer GLM-5.x if listed; the owner confirms before
   the data is committed. Qwen3.5-Flash is not open-weight, so not the
   small row.
2. **Catalog profile applied implicitly?** Design §4 says the entry's profile
   applies; decision 3 says no lever on without a row. **Default:** the entry
   names its profile, but a pending profile applies only when a run or the
   bench names it; H2 publishes rows and flips it.
3. **Where profiles live.** **Default:** as data in `vam-spec` next to
   `base` (one definition for vamd and guest); the catalog entry pins the
   name and version (decision 2). Own `urn:vam:profile` artifacts stay later.
4. **Profile values.** **Default:** `open-strong` 0.1.0 = `base` plus
   `adapter.thinking: catalog`, `adapter.resampleOnInvalid: 1`,
   `tools.examples: true`, `calls.repairBudget: 3`, `loop.detect: true`,
   `context.reciteTodo: true`; `open-small` 0.1.0 = `open-strong` with
   `calls.repairBudget: 2` and `context.compaction.triggerPercent: 60`. Every
   other switch at its `base` value, written out in full.
5. **Adapter fields on the entry.** **Default:** `reasoningField`,
   `thinking` (`strip` for GLM-5 and DeepSeek V4, `off` for the Qwen, which
   can emit tool calls inside thinking), `thinkingOff` (the request fragment),
   `toolChoice` (supported or not), `effortField` (name or none); the
   fragments are checked with one k = 1 call per model in wave 2.
6. **`keep` (reasoning sent back).** Some APIs require reasoning returned in
   the same turn's tool round. **Default:** `keep` accepted but behaves as
   `strip` plus a note on the event, unless wave 2's check shows an H1 model
   fails without it; then the broker re-attaches reasoning by assistant
   message digest (host only, no guest change).
7. **`tool_choice: required` in a profile.** **Default:** no profile sets it
   in H1 (it can block a plain text reply); an H2 ablation value only.
8. **Where the thinking scenario's real form lives.** **Default:** task
   `thinking-tool-call` in the unpublishable lane set `0.0.0-lane`; task set
   `0.1.0` stays pinned.
9. **Profiles in the paid run.** **Default:** `base` and the catalog profile
   for each model. Alternative: `base` only (about half the cost).
10. **Resample temperature.** **Default:** 0.7 for the one resample (the main
    request stays 0.1, `sampling` stays fixed).
11. **How a resample is labelled.** **Default:** `ContextHint.reason =
    "resample"` (existing string field, no proto change); a new lane value
    only if the wave finds the reason field ambiguous.
12. **Which tools get examples.** **Default:** builtins and guest intrinsics;
    MCP and host tool descriptions belong to their servers.
13. **`drop` stays the default** (decision 1): model-facing behaviour is
    identical to today; the only change is the size line on
    `inference.completed`, so it is no longer silent.

**Phase close:** unit suites on the merged base; all five scenarios active
and green; benchmark delta recorded from wave 5; release on the owner's word;
team checklist out.

**Phase close (H1, 2026-10-29)**

- Scenarios: all seven H1 scenarios `active` and green on the merged base (local
  runs; CI was unavailable). No `needs: firecracker` scenario changed;
  the microVM lane ran the paid rows on a KVM host.
- Benchmark delta recorded (progress log, waves 5): 24 open rows; Sonnet and
  Haiku rows unchanged.
- Known: `keep` not exercised live (DeepSeek returned no reasoning through the
  gateway); the guest musl build compiled on the macOS rootfs rebuild; `vam-vsock`
  does not build natively on macOS (E0004), outside this phase.
- Backlog for H2: a "thinking on" adapter value and a gateway check for
  reasoning pass-through; suppress `reasoning_missing` when a session never saw
  reasoning; open rows on the Benchmark Results docs page.

**Team checklist — Phase H1** (about 10 minutes)

Setup: deploy vamd from `main` after the H1 release, then the guest rootfs from
the same build (new resample and examples code). `vam` CLI from the same build.
The node vault holds the Vercel AI Gateway key as `vercel/api-key`.

1. Run a short chat on model `zai/glm-5` (provider `vercel`). **See:** a reply;
   `vam receipt show <run>` names `base v0.1.0`.
2. The same on `alibaba/qwen3.6-27b`. **See:** a reply.
3. The same on `deepseek/deepseek-v4-pro`. **See:** a reply.
4. Start a run with `inputs.harness` = `{"profile":"open-small"}`. **See:** the
   receipt's Harness section says `open-small v0.1.0`, a digest different from
   step 1's, and no overrides.
5. Start a run with `{"profile":"open-small","loop":{"detect":false}}`. **See:**
   `open-small` with `loop.detect=false` under overrides.
6. Start a run with `{"profile":"x"}`. **See:** HTTP 400 naming `profile`.
7. Start a run with `{"adapter":{"parser":"hermes"}}`. **See:** HTTP 400 naming
   `adapter.parser` as not built (parsers wait for a self-hosted row).
8. Start a run on `zai/glm-5` with `{"tools":{"examples":true}}` and ask it to
   save a note. **See:** the note is saved; no tool call with `{}` arguments.
9. Open `bench/results/0.1.0.json` on `main`. **See:** 32 rows: 8 Claude (run
   `55cb5b`), 24 open-model rows, each naming its profile and a maker price.

Report: "Phase H1 pass", or the step number with a screenshot or the error text.

### Phase H2 — Ablation and first profiles

**Goal:** the `design/harness.md` §3 grid is run; profiles are
evidence-backed.  
**Depends on:** H1, B1  
**Plan:** not drafted  
**Exit:** profiles `frontier`, `open-strong`, `open-small` published with
rows; the `close-prep` walkthrough live.

### Phase H3 — Ranking, deferral, escalation

**Goal:** the levers the grid asked for.  
**Depends on:** H2  
**Plan:** not drafted  
**Exit:** rows re-run; profiles bumped with deltas.

### Phase H4 — Recursive harnessing, offline

**Goal:** profiles improve themselves under review.  
**Depends on:** H2, B3  
**Plan:** not drafted  
**Exit:** one accepted profile PR produced by the loop.

## Explicitly later

- Self-hosted vLLM or Ollama rows (owner, 2026-10-26: gateway only for the
  launch table) and the server-side levers they unlock.
- B1 to B3 beyond what B0 shares with H0 (own tracker when B1 is planned).
- Community profiles as `urn:vam:profile` artifacts (`design/harness.md`
  open question 5).

## Progress log

| Date | Phase | What landed (end with the scoreboard: scenarios green, red, fix cycles, wall time) |
|---|---|---|
| 2026-10-26 | — | Tracker opened; base `feat/harness` cut from `main`. Locked decisions copied from `design/harness.md`, proposed until the owner confirms. H0 + B0 breakdown started (background, Opus 5.5 high). |
| 2026-10-28 | — | **H0 + B0 released to `main`** (merge commit) on the owner's word, validated without CI (CI unavailable): release head code identical to's validated head. Base fast-forwarded to `main`. Deploy note: rebuild the guest rootfs. Next: H1 breakdown on the owner's word. |
| 2026-10-28 | H1 | **Breakdown drafted** (background agent, Opus 5.5 high), awaiting owner approval. Base. Scenario delta: five new process scenarios (`thinking-model-tool-calls-are-not-dropped` with a mock form and a real form in `0.0.0-lane`, `reasoning-is-recorded-and-never-replayed-when-stripped`, `invalid-call-is-resampled-once-before-a-typed-error`, `tool-descriptions-carry-one-example-when-enabled`, `named-profile-prints-on-the-receipt-with-its-own-digest`), a guard addition on `plain-chat-gets-a-reply`, `vllm-model-runs-with-its-catalog-parser` deferred. Items H-1.1 to H-1.9 in six waves: W0 low scenarios, W1 medium levers + profiles + catalog, W2 medium reasoning, `tool_choice`, effort (host), W3 medium resample + examples (all `agent.rs` work; takes `main`, tells E10 Phase 3), W4 low bench profiles, W5 paid open rows (owner's word; about 3 USD, cap 10). Found in the code: the live OpenAI stream is the inline loop in `infer`, not `parse_sse_line`; the broker does not know the run's harness; `digest()`/`record()` hard-code `base`; the bench refuses ids `gateway_entry` does not cover; a resample would log `rollback{prefix_mismatch}` without a label. Proposed exit replaces U1 and `close-prep` with the 0.1.0 task set on three open models. 13 open questions with defaults (ids and prices unverified from this session). Scoreboard: 0 green, 0 red (scenarios not yet written), 0 fix cycles. |
| 2026-10-28 | H1 | **Breakdown approved** by the owner with recommendations 1-3 and every open-question default: (1) wave 1 checks each model id and price on the maker's own page before any catalog data is committed; (2) wave 3 takes `main` first and does not build while an E10 Phase 3 wave builds; (3) paid run under both `base` and each model's catalog profile, about 3 USD, cap 10 USD total. New exit line approved. Wave 0 started (Sonnet low). Scoreboard: 0 green, 0 red, 0 fix cycles. |
| 2026-10-28 | H1 W0 | **Wave 0 merged** (merge commit; Sonnet 5.5 low): six pending scenarios (the five of the delta plus the real-model form `thinking-model-tool-call-runs-on-the-bench`, task `thinking-tool-call` in `0.0.0-lane`), inference-crate guards on `plain-chat-gets-a-reply`, test-only mock fields `reasoning`, `resampled` (inert until a request label exists), `toolDescribed`. Orchestrator re-ran: `vam-e2e check` 51 scenarios ok; mock_script tests 33 pass; fmt clean; the six red for the right reason, class product (thinking x2 at then.1; strip at when.3; resample at when.0; examples at when.2; profile at when.0); smoke 37 pass, 0 fail, 6 red right reason; mock task set 0.1.0 at k = 2 on the merged head 8/8 (same as run `8c6684`). Merged without CI (Actions has no credits; test-only change fully checked locally). Profile digest-differs check dropped: the runner has no not-equal matcher. Scoreboard: 0 green, 6 red (right reason), 0 fix cycles, about 25 min. |
| 2026-10-28 | H1 W1 | **Wave 1 plan approved** by the owner (H-1.2, H-1.3; Sonnet 5.5 medium): profiles `base`/`open-strong`/`open-small` as data in `vam-spec` (Q4 values, `evidence: pending`), a run may name a profile, five switches accepted and recorded ahead of their behaviour (waves 2-3; unreleased base, profiles never implicit). DeepSeek V4 needs reasoning passed back with tools: wave 2 builds `keep` for real (host-side re-attach; Q6 fallback taken), DeepSeek entry `thinking: keep`. Model data: this session's network blocked the makers' pages; owner is allowing docs.z.ai, api-docs.deepseek.com, alibabacloud.com, vercel.com. Only entries verified on the maker's page and Vercel's list land; the rest follow. |
| 2026-10-28 | H1 W1 | **Wave 1 merged** (merge commit; Sonnet 5.5 medium). Profiles `base`/`open-strong`/`open-small` as data (`base` digest unchanged, pinned by test); a run may name `profile` (unknown name 400 naming `profile`; `version`/`digest`/`switches` still refused; no name means `base`); thinking, toolChoice, resampleOnInvalid, effort, tools.examples accepted and recorded (behaviour in waves 2-3; doc table in `harness.md`); catalog `adapter` block, `profile` name, any-maker gateway derivation. **H-1.3 entries not landed:** maker pages and vercel.com unreachable all session (owner allowing the domains). Spec change approved by the orchestrator: three pending scenarios' `expectFail.step` moved to the behaviour step (resample when.0→when.1, strip when.3→then.0, examples when.2→then.0); no `then` edited. Orchestrator re-ran: vam-spec harness 25, vam-inference catalog 18, vam-e2e bench 13+3, orchestrator run_context 57 and receipt 26 pass; check 51 ok; fmt clean; clippy warnings in changed crates pre-date the PR; smoke 38 pass, 0 fail, 5 red right reason; mock task set 0.1.0 k = 2 8/8 (as `8c6684`). Merged without CI (no Actions credits). Scoreboard: 1 green (named-profile-prints-on-the-receipt-with-its-own-digest), 5 red right reason, 0 fix cycles, about 50 min. |
| 2026-10-28 | H1 W2 | **Wave 2 plan approved** by the owner (H-1.4, H-1.5 plus two H-1.3 entries; Sonnet 5.5 medium). Reasoning parsed on both OpenAI-compatible paths into a host-only chunk field (guest frames unchanged); broker reads the run's adapter per request; one wire plan per mode (`drop` byte-identical, `strip` text to a host-only `reasoning` log lane, `keep` re-attached on the host by assistant-message digest, `off` sends the entry's fragment, `catalog` = entry mode); `inference.completed` gains `reasoningBytes`, markup count, mode, adapter notes; `tool_choice`/effort only when set and supported. Bench-lane thinking scenario flips active too (its mock form passes). Catalog data, owner decision 2026-10-28: GLM-5 at the owner-provided Z.ai price (1.00 / 0.20 cached / 3.20 USD per M, 200K) and Qwen3.6-27B at Alibaba's international price (0.60 / 3.60, 256K tier), source recorded per entry; DeepSeek V4 Pro held until its price is read on DeepSeek's page. Risk accepted: `keep` loses its store on a broker restart (DeepSeek would 400 that round). |
| 2026-10-28 | H1 W2 | **Wave 2 merged** (merge commit; Sonnet 5.5 medium; 1 fix cycle). Reasoning read on both OpenAI-compatible paths into a host-only chunk field, cleared before any guest frame (orchestrator read the broker loop and its test); modes drop (byte-identical), strip (host `reasoning` log lane), keep (host re-attach by assistant digest), off (entry fragment), catalog; `inference.completed` gains `reasoningBytes` (only when reasoning came back), `reasoningToolMarkup`, `reasoningMode`, `adapterNotes`; `tool_choice` and effort only when set and supported. Catalog `2026-v2`: GLM-5 and Qwen3.6-27B (owner-approved prices, sources in comments); DeepSeek held. **Fix cycle 1 (owner-approved spec change):** the reasoning scenarios asserted `present: true` and passed vacuously once the field was always present; now exact sizes (19, 25), the strip check moved to the strip run, `then` asserts the follow-up is `clean`; the field is absent without reasoning; shown red with recording disabled. Orchestrator re-ran: vam-inference 130, vam-spec examples/conformance/harness, orchestrator events 13, thread_recorder 43, brokers 18, run_context 57, receipt 26, lineage 5, vam-e2e pass; check 51 ok; fmt clean; smoke 41 pass, 0 fail, 2 red right reason; mock task set 0.1.0 k = 2 8/8. Merged without CI (no Actions credits). Scoreboard: 3 green (thinking x2, strip), 2 red right reason, 1 fix cycle, about 2 h 40 min. |
| 2026-10-28 | H1 W3 | **Wave 3 plan approved** by the owner (H-1.6, H-1.7; Sonnet 5.5 medium). Resample: guest-side discard and one resend at temperature 0.7 labelled `ContextHint{Main, "resample"}` (no proto change for the label); recorder accepts the label only when the run's switch is on, records `rollback{reason:"resample"}`; `inference.completed` `resamples` on the resample's own inference; counted as an invalid call (not a repair) in `summary.json` and bench rows. Examples: `\nExample: name({json})` on builtins, `task_complete` and playbook tools, each checked against its own schema; off is byte-identical. agent.rs: four small hunks, clear of E10 Phase 3's regions (closest 7 lines from its steer insertion). **Spec change (owner-approved):** resample scenario `data.resamples: "1"` (int64 as JSON string); hopeless mock gives up after the typed error. Risk: guest musl build not compiled in the cloud session; first compile is the owner's rootfs rebuild before the paid run. |
| 2026-10-28 | H1 W3 | **Wave 3 merged** (merge commit; Sonnet 5.5 medium; 0 fix cycles). One resample on an invalid call (guest `resample.rs`, temperature 0.7, label `ContextHint.reason = "resample"`; if the resend fails the original reply stands), recorder `rollback{reason:"resample"}` only with the switch on, `inference.completed` `resamples` (field 19), counted as invalid calls (not repairs) in `summary.json` and bench rows; one `Example:` line on nine builtins, `task_complete` and three playbook tools, each checked against its schema; off byte-identical. agent.rs: four small hunks, clear of E10 Phase 3's regions. Spec change (owner-approved): `resamples: "1"`; hopeless mock gives up after the typed error. Each green scenario shown red with its feature disabled. Orchestrator re-ran: vam-runtime 259+14+3, vam-inference 131, orchestrator thread_recorder 46, events 14, brokers 18, run_context 57, receipt 26, vam-e2e and vam-spec harness pass; check 51 ok; fmt clean; smoke 43 pass, 0 fail, 0 red; mock task set 0.1.0 k = 2 8/8. Guest musl build not compiled here (first compile: the owner's rootfs rebuild). Merged without CI (no Actions credits). E10 Phase 3 told the PR number via the owner. Scoreboard: all 7 phase scenarios green (H1 scenario set complete), 0 red, 0 fix cycles, about 1 h 50 min. |
| 2026-10-28 | H1 W4 | **Wave 4 plan approved** by the owner (H-1.8; Sonnet 5.5 low). `--profile <name>` (bench only; unknown name refused before any trial; unset = today, `base`); rows gain run tag, profile, version, digest, overrides; results merged per task-set version by (task, model, mode, profile), schema 2 with a `runs` list, 55cb5b rows byte-identical; README runbook for GLM-5 and Qwen3.6-27B (k = 1 smoke, then `base` and catalog profile at k = 5). Owner added `--max-usd`: the runner stops starting trials past the cap (GLM 2, Qwen 1, 10 USD total); a missing cost stops the run. |
| 2026-10-28 | H1 W4 | **Wave 4 merged** (merge commit; Sonnet 5.5 low; 1 fix cycle). `--profile` (bench only, unknown refused before any trial), rows carry run tag, profile, version, digest, overrides; publish merges by (task, model, mode, profile) into schema 2 with `runs`, 55cb5b rows byte-identical (test reads the real file); `--max-usd` stops starting trials past the cap or on a missing cost (exit 3, never published); `vam-e2e bench-merge` for results copied out of Lima (refuses unpublishable); README steps 11-15 (GLM-5 and Qwen3.6-27B: k = 1 smoke, then `base` and catalog profile at k = 5 with caps). **Fix cycle 1 (orchestrator review):** receipt-less rows were profile `unknown`, so two profiles' rows collided on merge; now the requested profile; and step 13 had no merge command. Orchestrator re-ran: vam-e2e all suites pass; check 51 ok; fmt clean; smoke 43/43; mock 0.1.0 k = 2 8/8 under `base` (digest…`) and under `open-small` (digest…`, every row incl. receipt-less); unknown profile exit 2; bench-merge refuses a mock result. Docs site build not run (no node_modules; one sentence). Merged without CI (no Actions credits). Scoreboard: 7/7 phase scenarios green, 0 red, 1 fix cycle, about 1 h 40 min. |
| 2026-10-28 | H1 W5 | **Paid run approved** by the owner within a budget: GLM-5 and Qwen3.6-27B, k = 1 smoke then k = 5 under `base` and the catalog profile, after a rootfs rebuild (first guest musl compile of waves 2-3). A macOS lane session was started from this orchestrator; it also reads DeepSeek's price page and Vercel's list (read-only) for the DeepSeek entry. |
| 2026-10-28 | H1 W5 | **Open-model rows recorded** (merge commit), on a KVM host after a rootfs rebuild (waves 2-3 guest code compiled first try); mock k = 2 8/8; smokes 4/4 each. k = 5, task set 0.1.0: GLM-5 `base` 20/20, `open-strong` 20/20; Qwen3.6-27B `base` 20/20, `open-small` 20/20; every row publishable, maker list price (catalog 2026-v2), no cost missing; 55cb5b Claude rows unchanged (orchestrator re-checked); key grep clean. Cost per success (µUSD; first-try / injected / match / no-match): GLM base 2,027 / 7,779 / 53,702 / 3,076; GLM open-strong 2,052 / 5,620 / 53,973 / 3,275; Qwen base 1,775 / 3,256 / 26,748 / 2,047; Qwen open-small 1,832 / 2,783 / 21,539 / 2,155. **Benchmark delta (§11):** before: no open rows; after: 16 rows on two open models. P12 invalid calls 0, repairs 0, resamples 0, loop detections 0 on every row: **finding** — the 0.1.0 tasks never produce an invalid call, so the H1 levers have nothing to act on; their effect needs B1's longer tasks. Profiles changed cost only (Qwen open-small 19% cheaper on playbook-match, within one run's noise). Sonnet/Haiku rows did not regress (unchanged; mock 8/8 on every merged head). DeepSeek V4 Pro read on its page: `deepseek/deepseek-v4-pro`, 1M context, per M off-peak/peak: cache hit 0.022/0.044, miss 0.66/1.32, output 1.98/3.96 USD (peak 01:00-04:00 and 06:00-10:00 UTC weekdays). Also seen: `vam-vsock` fails to build on macOS (E0004 non-exhaustive match); the macOS lane used the Lima-built `bench-merge`. |
| 2026-10-29 | H1 | **DeepSeek V4 Pro entry merged** (merge commit; Sonnet 5.5 low): `deepseek/deepseek-v4-pro` at DeepSeek's peak list price (owner decision: peak, cost never understated; 1.32 in / 0.044 cache hit / 3.96 out USD per M, 1M context), thinking `keep`, `tool_choice` unsupported, profile `open-strong`; catalog `2026-v3`; README step 15 has the DeepSeek runs. The same PR fixes a base failure the orchestrator caused: changed the published 0.1.0.json and two wave 4 tests read it as a schema-1 fixture (merged without running the vam-e2e tests); they now read a frozen pre- fixture, and a new test checks the live file still holds the 55cb5b rows byte-identical. Orchestrator re-ran: vam-e2e all suites, vam-inference 131, check, fmt, smoke 43/43, mock 0.1.0 k = 2 8/8. Rule taken: run the vam-e2e tests on every merge that touches `bench/results`. DeepSeek paid run approved by the owner within a budget (smoke, then `base` and `open-strong` at k = 5). |
| 2026-10-29 | H1 W5 | **DeepSeek V4 Pro rows recorded** (merge commit), on a KVM host. Smoke (run `c95766`) stopped on `reasoning_missing`; read-only diagnosis: `reasoningBytes` absent on every DeepSeek round, tool-call ids and names match, no 400 — DeepSeek returned no reasoning through the gateway, so the note is expected and `keep` was not exercised live; owner said continue. k = 5: `base` (run `77e1f2`) 20/20, `open-strong` (run `cbcfa4`) 20/20, every row publishable at DeepSeek's peak list price (catalog 2026-v3). Cost per success µUSD (first-try / injected / match / no-match): base 1,149 / 2,543 / 10,051 / 664; open-strong 716 / 1,707 / 9,267 / 480. 119 rounds, reasoning absent on all. Orchestrator: 24 earlier rows byte-identical, vam-e2e tests pass on the branch, key grep clean. **H1 exit met:** task set 0.1.0 on the three gateway open models (GLM-5, Qwen3.6-27B, DeepSeek V4 Pro), 24 open rows all publishable, invalid calls, repairs, resamples and loop detections recorded on every row, profiles `open-strong` and `open-small` present as data. **Benchmark delta:** before no open rows, after 24; P12 lines did not move (0 invalid calls everywhere): finding, the 0.1.0 tasks never break a call, H1's levers need B1's longer tasks. Scoreboard: 7/7 phase scenarios active and green (local; CI unavailable), 0 red, fix cycles: W2 1, W4 1, base fix in. |
