---
title: Agent loop
description: How the guest builds the instructions, calls the model, applies the harness switches and keeps the context small.
status: shipped
proves:
  - harness-switches-print-on-the-receipt
  - named-profile-prints-on-the-receipt-with-its-own-digest
  - repeated-identical-call-is-caught-and-observed
  - todo-is-recited-each-turn-when-enabled
  - compaction-keeps-an-early-correction
  - follow-up-after-compaction
  - invalid-call-is-resampled-once-before-a-typed-error
  - tool-descriptions-carry-one-example-when-enabled
  - crates/vam-runtime/src/skill_index.rs::generate_is_sorted_unique_and_escaped
---

The loop runs inside the guest for each prompt task (`run_react_loop`, `crates/vam-runtime/src/agent.rs:36`).
It calls the model through the host on vsock port 53 (`agent.rs:82`), runs the tools the model asks for,
and stops when the model gives a final answer (or, when the run asks for an explicit claim, calls `task_complete`, `agent.rs:748`). The host decides what the agent
may do; see [Tools](/runtime/tools/).

## Instructions

The system text is built from layers:

1. The embedded template `templates/SYSTEM.md`, or the workspace file `.vam/SYSTEM.md` when it exists
   (`src/instruction.rs:9,66`).
2. The agent's own instruction text, with `{{inputs.name}}` placeholders filled from the run inputs
   (`agent.rs:51-58`).
3. Machine facts and tool guidance (`src/instruction.rs:134,187`).
4. The skill catalog and, for a bound playbook, its procedure (`agent.rs:88-96`).

## Skills

A skill is a folder under `/mnt/drive9/skills/<skill-id>/` whose entry file is `SKILLS.md`
(`src/skills.rs:148`; the sample is `crates/vam-runtime/SKILLS.md`). The model calls `load_skill` to read one.
For a large documentation tree the runtime writes an `INDEX.md` (`src/skill_index.rs`) and tells the model to
`grep` it before it globs (`src/instruction.rs:413`). MCP connections are preloaded: their tools are offered on
the first turn.

## Harness switches

A run may send `inputs.harness`. The host resolves it once, when the run is created, into a record with a
profile, a version, a digest and the overrides; the guest reads the switches
(`crates/vam-spec/src/harness.rs:1-30`, `Harness::from_inputs` at `agent.rs:43`). The receipt prints the profile,
its digest and any overrides. A key the host does not know is refused, and a switch whose lever is not built
accepts only its default.

| Profile | Differs from `base` | Source |
|---|---|---|
| `base` | nothing: the loop as it was | `harness.rs:53,415` |
| `open-strong` | thinking from the model catalog, one resample on an invalid call, a tool example line, repair budget 3, loop detection on, todo recitation on | `harness.rs:419-428` |
| `open-small` | `open-strong` with repair budget 2 and compaction at 60 percent | `harness.rs:431-436` |

Only `base` applies when a run names no profile. The model catalog entry names the profile that belongs to
a model ([Inference](/runtime/inference/)); the catalog never applies it on its own.

| Switch | What it does | Proof |
|---|---|---|
| `loop.detect` | `Detector` (`src/loop_detect.rs:45`) warns after five identical calls and stops on the next; trips on three alternating cycles and on three identical errors | `repeated-identical-call-is-caught-and-observed` |
| `context.reciteTodo` | the todo list is repeated to the model each turn | `todo-is-recited-each-turn-when-enabled` |
| `calls.repairBudget` | this many invalid calls in a row to one tool fail the task; 0 means unlimited | `agent.rs:734`; not proved |
| `adapter.resampleOnInvalid` | one resample at a higher temperature for an invalid call, labelled `resample` (`src/resample.rs:26`) | `invalid-call-is-resampled-once-before-a-typed-error` |
| `tools.examples` | one `Example:` line on each built-in tool description | `tool-descriptions-carry-one-example-when-enabled` |
| `human.jig` | offers the `show` tool ([Jig](/runtime/jig/)) | `progress-shows-while-the-task-runs` |

The receipt printing is proved by `harness-switches-print-on-the-receipt` and, for a named profile,
`named-profile-prints-on-the-receipt-with-its-own-digest`.

## Compaction

The loop estimates the context from the last usage report (`src/compaction.rs:43`). At 70 percent of the model's
window (`trigger_percent`, 60 for `open-small`) it asks the model for a summary of the older turns (`agent.rs:1855`) and keeps the last three rounds, the current user message when it fits, and
never starts the kept tail on a tool result (`src/compaction.rs:65`; unit tests in the same file). Without a known
window only a byte guard triggers. Proved by `compaction-keeps-an-early-correction` and `follow-up-after-compaction`.

## Not proved

- `calls.repairBudget`: no test or scenario drives a run to the repair-budget failure.
