---
title: Run Specifications (run.json)
description: Complete guide to authoring RunSpec files, managing workspaces and threads, and executing multi-turn agent runs.
---

Read [Authoring model](/authoring/model/) for which agent and playbook a run resolves, and which input keys the host owns. This page is the run file.

A **Run** in VAM represents a single execution turn of an agent or playbook against a persistent workspace volume.

The **`run.json`** file binds an agent, a durable storage volume, conversation thread, and input prompt. It maps to the **`vam.v1.RunSpec`** protobuf message (`proto/vam/v1/workflow.proto`).

---

## Annotated Example: `run.json`

Below is an annotated `run.json` based on the Jira assistant workflow:

```json
{
  "runId": {
    "urn": "urn:vam:run:local.vam.dev/vam/jira-assistant-transition-run-001"
  },
  "agentRef": {
    "resourceId": {
      "urn": "urn:vam:agent:local.vam.dev/vam/jira-assistant"
    },
    "version": {
      "major": 1,
      "minor": 0,
      "patch": 0
    }
  },
  "workspaceRefUri": "volume://assistant",
  "threadRef": "main",
  "inputs": {
    "prompt": "Find issue CGA-1298 and transition it to In Progress."
  },
  "maxDuration": "1800s",
  "maxFinancialBudget": {
    "currency": "USD",
    "micros": 1000000
  },
  "idempotencyKey": "9d8e3b12-4c5a-4b71-9f20-1a8c9e4210f0"
}
```

---

## The Four Core Identities

A common source of confusion in agent architectures is mixing session state with conversation history. VAM separates these concerns into four distinct identities:

```
┌─────────────────────────────────────────────────────────────────────────────┐
│ 1. WORKSPACE (volume://assistant)                                           │
│    • Durable Drive9 storage volume containing user files & conversation logs │
│    • Persists across all microVM boots and runs                              │
├─────────────────────────────────────────────────────────────────────────────┤
│ 2. THREAD (main, feature-auth, sprint-24)                                   │
│    • One conversation history stream on that volume                         │
│    • Stored in .vam/threads/<thread_id>/timeline.jsonl                      │
├─────────────────────────────────────────────────────────────────────────────┤
│ 3. SESSION (sess-run-0198a2...)                                             │
│    • An ephemeral Firecracker microVM instance spawned for ONE task turn    │
│    • Booted fresh, executes the prompt, commits disk changes, and terminates│
├─────────────────────────────────────────────────────────────────────────────┤
│ 4. RUN (run-0198a2...)                                                      │
│    • A single prompt/agent turn, recorded as an immutable timeline event    │
└─────────────────────────────────────────────────────────────────────────────┘
```

| Identity | What It Is | Where It Lives | Portable Across Hosts? |
|---|---|---|---|
| **Workspace** | Durable Drive9 disk volume (files + timelines) | `volume://<id>` | **Yes** (in Drive9 storage) |
| **Thread** | Distinct conversation history on a workspace | `.vam/threads/<id>/` | **Yes** (in volume `.vam/` directory) |
| **Session** | Ephemeral microVM writer process | Host SQLite + VM IDs | **No** (host-local disposable VM) |
| **Run** | One prompt execution turn | Host run catalog + volume timeline | **Yes** (inputs, turn records) |

:::note[Every Run Boots a Fresh MicroVM]
VAM does **not** keep long-running VMs alive between conversation turns. Each run starts a fresh, pristine microVM, mounts the durable Drive9 workspace volume, replays the bounded thread history, completes the step, commits disk changes, and destroys the VM.
:::

---

## Field Reference

### 1. Agent Reference (`agentRef`)
- **`resourceId.urn`**: The URN of the registered `AgentSpec` executing this task.
- **`version`**: Optional run selection. Set it to run one release of the agent. Omit it to run the agent by the rule in section 1b. Do not write a `digest`. The server records the frozen digest on the run.

### 1b. Resolving unversioned refs
A skill, playbook, policy, or agent `ResourceRef` that **omits `version`** resolves by run type:

- **Draft run.** The ref resolves to the agent owner's draft of that dependency if one exists. Otherwise it resolves to the latest release.
- **Release.** Every ref is frozen to a version and digest. A release that depends on a draft-only dependency fails, because a draft cannot be frozen. Publish the dependency first.

A ref with `version` names that release and stays on it. The resolved version and digest are recorded on `resolvedRefs` on the stored run. If nothing can be resolved, run create fails closed.

---

### 2. Storage & Thread Routing
- **`workspaceRefUri`**: URI of the durable Drive9 volume. Must begin with the `volume://` scheme (e.g. `"volume://assistant"`). The host prefixes the volume id from the caller's `owner_user_id`; guest-supplied ids are the suffix only.
- **`threadRef`**: The conversation thread identifier (defaults to `"main"`). You can fork or maintain independent conversation topics on the same workspace by specifying different thread names (e.g. `"threadRef": "sprint-retrospective"`). `threadRef` inherits the same host owner prefix as the workspace id.

---

### 3. Inputs & Prompt Parameterization
- **`inputs.prompt`**: The chat message for this turn. The host owns it. A playbook `inputSchema` must not require it, and a schema with `additionalProperties: false` still accepts it.
- **`inputs.<custom_key>`**: Values the playbook declares (`project_key`, `sprint_name`). Templates in step instructions read them as `{{inputs.project_key}}`.

These keys are also host-owned and are stripped before the playbook schema is checked: `model`, `model_provider`, `max_iterations`, `step_timeout_secs`, `system_instructions`, `agent_role`, `agent_display_name`, `playbook_procedure`. Do not add them to `inputSchema`. `model` and `model_provider` follow the agent's `preferredModels` entry. `max_iterations` and `step_timeout_secs` are the budgets `vamd` resolved for the run. `playbook_procedure` is the rendered procedure from [What the agent sees](/authoring/playbook-spec/#what-the-agent-sees).

---

### 4. Budgets & Idempotency
- **`maxDuration`**: Active time for the task. When the caller omits it, or sends a non-positive value, `vamd` stores 30 minutes (`"1800s"`). A positive caller value is kept. Parked time, including human waits and keep-warm, does not count. The next guest task receives only the budget that is left.
- **`maxFinancialBudget`**: Upper financial limit for LLM inference calls across the run:
  - `currency`: ISO currency code (`"USD"`).
  - `micros`: Budget in micro-units ($1.00 = `1000000` micros).
- **`idempotencyKey`**: UUID to prevent duplicate executions if a network retry occurs.

---

## Multi-Turn Conversations & Memory Replay

When you run follow-up prompts on the same workspace and thread:

```bash
# Turn 1: Initial task from a run file
vam run --file examples/bun-assistant/run.json --prompt "Create a README.md file in workspace" --follow

# Turn 2: same workspace and thread, no run file
vam run --workspace assistant --thread main -p "Now add a badge to the README you just created"
```

### What Happens Behind the Scenes:
1. **Turn 1 Completes**: VAM appends a turn record to `.vam/threads/main/timeline.jsonl` on the Drive9 volume, records a trajectory digest, and terminates the VM.
2. **Turn 2 Launches**: VAM boots a fresh microVM, mounts the same Drive9 volume (with the updated `README.md`), reads previous turns from `timeline.jsonl`, injects prior context into the prompt, and dispatches the task.

---

## Launching Runs via `vam` CLI

```bash
# Run from a run.json file
vam run --file examples/jira-assistant/run.json --follow

# Override prompt dynamically
vam run --file examples/jira-assistant/run.json --prompt "Summarize open P1 issues" --follow

# Run on a separate thread
vam run --file examples/jira-assistant/run.json --thread sprint-audit --prompt "Audit sprint 14" --follow
```

---

## Next Steps

- Learn how to define multi-step DAG workflows in [Playbook DAGs (`playbook.json`)](/authoring/playbook-spec/).
- Explore human approval workflows in [Cedar Policies & HITL](/authoring/agent-spec/).
