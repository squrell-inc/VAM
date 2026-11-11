---
title: Playbooks (playbook.json)
description: Author a host-tracked playbook with steps, outcomes, and preconditions. Prose-only procedures belong in skills.
---

Read [Authoring model](/authoring/model/) for how a playbook attaches to an agent, how `requires` resolves, and `vam agent check`. This page is the playbook file.

A **Playbook** in VAM is a **host-tracked** procedure: inputs, outputs, and steps the host can verify as the run progresses. Playbooks map to the **`vam.v1.PlaybookSpec`** protobuf message (`proto/vam/v1/workflow.proto`).

## Playbooks vs skills

- **Host-tracked playbooks** declare steps, outcomes, and preconditions. The host records step state (`step_complete`), runs bound steps (`run_step`), opens gates, denies guarded tool calls until `allowedWhen` holds, and checks evidence before a non-failed outcome.
- **Prose-only procedures belong in skills**, not as playbooks. Put guidance that the host cannot track in a skill’s `SKILLS.md` (or equivalent skill prose). VAM does not treat a list of instructions with no host tracking as a playbook.

---

## Annotated Example: `playbook.json`

Below is a trimmed excerpt of `examples/jira-assistant/playbook.json` (sprint backlog cleanup): a task step, a human gate, a follow-on task, and a deny-only guard. The host opens gates when they are available and evaluates guards on every tool call. `argsWithin` is enforced on every tool call that matches the guard (PB-2.3).

```json
{
  "resourceId": {
    "urn": "urn:vam:playbook:local.vam.dev/vam/jira-sprint-backlog-cleanup"
  },
  "displayName": "Sprint Backlog Cleanup",
  "description": "Finds stale issues, asks for approval, transitions approved issues, and writes a report.",
  "activation": {
    "when": "Use when the user wants to clean up stale issues in the current sprint."
  },
  "steps": [
    {
      "id": "query-stale-issues",
      "kind": "STEP_KIND_TASK",
      "mode": "STEP_MODE_AGENTIC",
      "instruction": "Search for stale issues in project '{{inputs.project_key}}' and sprint '{{inputs.sprint_name}}' using jira_search_issues.",
      "evidence": {
        "tool": "jira_search_issues",
        "result": "success"
      },
      "outputSchema": {
        "type": "object",
        "properties": {
          "issues": { "type": "array" }
        },
        "required": ["issues"]
      }
    },
    {
      "id": "request-approver-review",
      "kind": "STEP_KIND_GATE",
      "after": ["query-stale-issues"],
      "question": "Approve moving these stale issues back to the backlog?",
      "choicesFrom": "steps.query-stale-issues.output.issues",
      "timeout": "2592000s",
      "options": [
        { "label": "approved" },
        {
          "label": "rejected",
          "terminal": "OUTCOME_TERMINAL_SUCCEEDED"
        }
      ]
    },
    {
      "id": "transition-stale-issues",
      "kind": "STEP_KIND_TASK",
      "mode": "STEP_MODE_AGENTIC",
      "after": ["request-approver-review:approved"],
      "instruction": "For each selected issue, call jira_transition_issue and jira_add_comment."
    }
  ],
  "guards": [
    {
      "tools": ["jira_transition_issue"],
      "sideEffects": ["SIDE_EFFECT_CLASS_MUTABLE_EXTERNAL"],
      "allowedWhen": ["request-approver-review:approved"],
      "argsWithin": [
        {
          "argument": "issue_key",
          "within": "steps.request-approver-review.output.selected[*].key"
        }
      ]
    }
  ],
  "inputSchema": {
    "type": "object",
    "properties": {
      "project_key": { "type": "string" },
      "sprint_name": { "type": "string" },
      "cutoff_days": { "type": "integer" }
    },
    "required": ["project_key", "sprint_name", "cutoff_days"]
  }
}
```

See the full example under [`examples/jira-assistant/playbook.json`](https://github.com/squrell-inc/VAM/blob/main/examples/jira-assistant/playbook.json).

---

## Activation: when the model starts a playbook

`activation` tells the model when to start the playbook. In chat nobody picks a playbook: the agent lists its attached playbooks with their `when` text, and the model calls `start_playbook(urn, inputs)` when a request matches.

```json
"activation": {
  "when": "Use when the user wants to draft a pull request.",
  "examples": ["Please draft a pull request that fixes the login bug."],
  "mode": "ACTIVATION_MODE_ASK"
}
```

| Field | Rule |
|---|---|
| `when` | **Required to publish (release).** One or two sentences, at most 280 characters. A draft can be saved without it, and a release without it is refused with `playbook.activation.when is required: ... Add it and publish again.` There is no fallback to the description or the name. |
| `examples` | Optional sample requests. At most 5, each at most 200 characters. Need a `when`. |
| `mode` | `ACTIVATION_MODE_ASK` (default): the user sees an ask card ("Start *Draft a PR*?") with the inputs prefilled, and confirms, edits or declines. `ACTIVATION_MODE_AUTO`: starts without asking, for playbooks that only read. |

`auto` is refused when a step binds a mutating builtin tool (anything but `read_file`, `glob`, `grep`, `load_skill`). At start time the host also asks when a bound or assisted step uses a tool the session does not know to be read-only, and, once the run has seen untrusted text (a web page or an MCP result), when any step lets the model choose its tool.

**What the host checks on `start_playbook`:** the playbook is the agent's default or one of its allowed playbooks (`allowAnyPlaybook` does not widen this), the task has no playbook yet (a task runs one), and `inputs` satisfy `inputSchema`. A playbook with no input schema takes no inputs: call it with `inputs` omitted or `{}`. Starting never grants a permission; Cedar still decides every call. A refusal comes back as a message the model can act on.

**Explicit starts do not match.** The CLI `--playbook`, the API (`playbookRef` in the run), and trigger grants bind the named playbook when the run is created, as before. A run that names none stays a plain run (the default playbook is never bound on its own), and the model may start one with `start_playbook`.

## Step shape

| Field | Required | Role |
|---|---|---|
| `id` | yes | Unique within the playbook |
| `kind` | yes* | `STEP_KIND_TASK` or `STEP_KIND_GATE` (*unspecified migrates to task) |
| `instruction` | yes for tasks | Prose for the agent |
| `mode` | yes for tasks | Who supplies the tool and arguments |
| `binding` | no | `toolName` plus `arguments` (forbidden on gates) |
| `outcomes` | no | Declared outcome labels; a task with none has implicit `done` |
| `after` / `afterAny` | no | Preconditions on prior steps / outcomes |
| `optional` | no | Default `false`; required steps block run completion |
| `outputSchema` | no | JSON Schema for the step’s recorded output |
| `evidence` | no | Host-observed tool call required before a non-failed outcome |

### Modes (task steps)

| Mode | Tool | Arguments |
|---|---|---|
| `STEP_MODE_BOUND` | author | author (literal args) |
| `STEP_MODE_ASSISTED` | author | model fills from context |
| `STEP_MODE_AGENTIC` | model | model |

`bound` and `assisted` require `binding.toolName`. Templated binding args make a step `assisted`. On a playbook run the guest can call `run_step` with `step_id` only. The host executes a bound step's publish-time arguments through the PEP. `run_step` rejects assisted steps and any binding whose arguments contain `{{...}}`.

### What the agent sees

The host renders this procedure into the guest prompt. The header says to complete available steps in any order, that the host tracks each step, and that each step names the tool that completes it. Every task step includes `mode: bound`, `mode: assisted`, or `mode: agentic`, then one line that says how to finish it:

- **bound** — `Complete it by calling run_step with step_id "<id>". The host runs the bound tool.`
- **assisted** — call the tool named on the step, filling its arguments from context, then `step_complete` with that `step_id` and an outcome. Do not call `run_step`. `run_step` rejects assisted steps; `step_complete` is what records the outcome.
- **agentic** — do the step with the tools you have, then `step_complete` with that `step_id` and an outcome describing what you did. Never invent results. If you cannot do the step with your tools, say so in the outcome.
- **gate** — `Wait for the human decision on the gate approval. Do not call run_step or step_complete for it.` Gates do not print a mode line.

When a finish attempt is still missing steps, the host observation starts with `Playbook finish is incomplete. Do not end the task yet.`, lists each missing step with that same action line, and ends with: `For each missing step: call run_step if it is bound, do the work and call step_complete if it is agentic, and wait for the human decision if it is a gate. Never invent a step's outcome.`

### Preconditions

- `after: ["B", "C:approved"]` — all must hold. A bare id means “completed with any outcome”; `id:outcome` means that outcome.
- `afterAny: ["C", "D"]` — at least one must hold (merge exclusive branches).
- Both may be set; both must hold. Steps with neither are available at the start.

### Outcomes

- Each entry: `{ label, description?, terminal? }`.
- `terminal: OUTCOME_TERMINAL_SUCCEEDED | OUTCOME_TERMINAL_FAILED` ends the run when that outcome is recorded; remaining steps become `skipped`.
- Every task step implicitly allows `failed` (honest inability to complete the step).

---

## Gates and guards

Publish validation accepts well-formed **gates** and **guards**. The host opens a gate when its preconditions hold (PB-2.1). The step is `open` (reported as `STEP_STATE_PAUSED`), so `step_complete` cannot record it. A human posts `SUBMITTED` with a response that matches the gate schema, or `REJECTED` when `rejected` is an allowed outcome. That response is the step output. `choicesFrom` (`steps.<id>.output.<path>`) fills the schema enum from the preceding step's output at open time. `timeout` defaults to 72h and cannot exceed 30 days. `onTimeout` records that outcome and resumes; with no `onTimeout` the run ends `EXPIRED` and the workspace reservation is released. `keepWarm` parks the VM for the gate duration, or 5 minutes when unset, on the existing HITL keep-warm and resume path.

On every tool call the PEP evaluates Cedar first, then each matching guard. Either side can deny. A guard never grants a call Cedar denied. A guard matches when the tool name is listed or the call's side-effect class is listed. `allowedWhen` uses the same syntax as `after` and is checked against the run's current step rows. Until it holds, the denial names the blocking step and the required outcome (for example, blocked until step `approve` is `approved`). A non-failed `step_complete` is accepted only when required evidence was observed while the step was available.

### Gates (`STEP_KIND_GATE`)

- No `binding`. Use `question` (template) plus either `options` (≥ 2 branch labels) or `responseSchema`.
- Optional: `choicesFrom` (`steps.<id>.output.<path>` of a preceding step), `timeout` (max 30 days; default at runtime is 72h), `onTimeout`, `keepWarm`.
- Example: option `rejected` with `terminal: OUTCOME_TERMINAL_SUCCEEDED` is a clean stop without later tasks.
- Decide with `vam approve decide <approval> --decision submitted --response '{...}'`. Tool-call approvals still use `approved`, `rejected`, and `amended`.

### Guards (`guards[]` on the playbook)

Deny-only rules. Shape:

- Target: `tools: [...]` and/or `sideEffects: [SIDE_EFFECT_CLASS_MUTABLE_EXTERNAL, ...]`
- `allowedWhen`: same syntax as `after` — every entry must hold before the target is allowed
- Optional `argsWithin`: tool argument path must be a member of a step output path (for example `issue_key` within `steps.request-approver-review.output.selected[*].key`)

Playbooks never grant permissions; Cedar and guards can only deny. When the run has step rows, Cedar sees them as `context.playbook` (`id`, `state`, `outcome`) on `ExecuteTool`. Org policies can read that attribute. Guards themselves stay outside Cedar.

### Evidence

`evidence: { tool, result }` is accepted at publish. The host accepts a non-failed outcome only when this run's trajectory has a matching successful call (`tool`, `result`) recorded while the step was available. A call from before the step was available does not count. The matching trajectory refs are stored on the step and returned as `RunStatus.step_statuses[].evidence_refs`. The implicit task outcome `failed` does not require evidence.

### Step-tracking-only warning

Publish accepts a playbook that has no gate, no guard, no step with a tool binding and no step with evidence, but it warns. Such a playbook only records step state; the host does not limit tool use or what counts as completion. `vam playbook put` prints the warning to stderr after a successful save, so stdout stays the saved spec. `vam playbook explain` lists it under `warnings`. Adding any one of those four enforcement features clears the warning. The warning is guidance only: it never blocks a publish.

`run_step(step_id)` is advertised only on playbook runs, the same way as `step_complete`. It executes the publish-time bound arguments through the PEP (Cedar and guards still apply) and records the step outcome from the result. Guest-supplied arguments are not used. A Cedar deny does not run the tool and does not complete the step as success.

---

## Requires

A playbook may declare skills and capabilities the procedure needs. `requires` is a dependency list, not a scheduler.

```json
"requires": {
  "skills": [
    { "resourceId": { "urn": "urn:vam:skill:local.vam.dev/vam/jira-integration" } }
  ],
  "capabilities": ["tool:jira.search", "tool:jira.write"]
}
```

Write each skill as a URN. Leave `version` and `digest` off. On a draft run the host resolves that URN to the agent owner's draft of the skill when one exists, otherwise to the latest release. Release of the playbook freezes the URN to a version and a digest. A skill that exists only as a draft blocks that release.

`capabilities` uses the same tokens as a skill manifest: `tool:`, `egress:`, `secret:`, or `fs:` plus a non-empty remainder. They describe what the procedure claims to need. They do not grant the call.

`vam agent check <agent-urn>` reports each required skill and whether the agent attaches it, plus these tokens. A missing skill is a report, not a failed command. Add the URN to the agent's `skillRefs` and publish the agent again. The resolved pins of an admitted run are stored on `RunSpec.resolvedRefs`.

---

## Retired fields

| Field | Status |
|---|---|
| `dependsOn` | Retired ordering. Publish migrates bare ids into `after` and clears `dependsOn`. Author new playbooks with `after` / `afterAny`. |
| `retryPolicy`, `fallbackStepId`, `joinPolicy`, `requiredSuccesses`, `evaluationPolicy` | Rejected at publish when set. |

---

## Registering and Running Playbooks

### Register Playbook

```bash
vam playbook put examples/jira-assistant/playbook.json
```

### Execute Run with a Playbook

In `run.json`, specify `playbookRef` instead of a static prompt:

```json
{
  "agentRef": {
    "resourceId": { "urn": "urn:vam:agent:local.vam.dev/vam/jira-assistant" }
  },
  "playbookRef": {
    "resourceId": { "urn": "urn:vam:playbook:local.vam.dev/vam/jira-sprint-backlog-cleanup" }
  },
  "workspaceRefUri": "volume://assistant",
  "inputs": {
    "project_key": "OPS",
    "sprint_name": "Sprint 42",
    "cutoff_days": 14
  }
}
```

Launch the run:

```bash
vam run --file run.json --follow
```

On playbook runs the guest records progress with the host built-in `step_complete`, or calls `run_step` for a bound step. The rendered procedure names which of those calls completes each step (see [What the agent sees](#what-the-agent-sees)). `RunStatus.step_statuses` shows each step’s state, outcome, and evidence refs.

Before the guest commits success, the host checks required steps, open gates, and the playbook `outputSchema`. Missing items come back as an observation that repeats each missing step's completion action, and the guest keeps going, counting each turn against `max_iterations`. A run that is still incomplete when the loop stops is `FAILED`. The result keeps `error`, `missing_steps`, and `answer`, and adds `open_gates` and `schema_error`. A complete run whose output matches `outputSchema` is `SUCCEEDED`. An open gate cannot succeed; when no task step is still available the host parks the run instead. Runs with no playbook are unchanged. Step `outputSchema` is still checked on `step_complete`, not at finish.

---

## Next Steps

- Explore in-VM tool executors in [Writing In-VM Tools (`execute.js`)](/authoring/agent-spec/).
- Learn about skill manifests and prose guidance in [Skill manifests](/authoring/skill-manifest/).
- Review [CLI Command Reference](/getting-started/cli/).
