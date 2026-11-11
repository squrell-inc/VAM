---
title: Bun Assistant Tutorial (Workspace & Multi-Turn)
description: Step-by-step tutorial running the Bun assistant for workspace file manipulation, multi-turn conversations, and thread persistence.
---

This tutorial demonstrates how to run the **Bun Assistant** (`examples/bun-assistant`) to create and edit files inside the persistent `/mnt/drive9/workspace/` volume across multiple conversation turns.

---

## Prerequisites

1. Ensure the `vamd` orchestrator is running (`cargo run -p vam-orchestrator`).
2. Ensure an LLM API key is stored (`vam secrets set openrouter/api-key --kind inference ...`).
3. *(Optional)* Publish the skill package to the catalog: `vam skill publish examples/bun-assistant/skills/assistant-tools`. Unpublished runs still work via seed fallback from `examples/bun-assistant/skills/`.

---

## Step 1: Launch Turn 1 (Create File)

Start a run with an inline prompt instructing the assistant to write a project configuration file:

```bash
vam run --file ./examples/bun-assistant \
  --prompt "Create a project config file at /mnt/drive9/workspace/app-config.json with environment 'staging' and port 8080." \
  --follow
```

### Execution Trace:
```text
[vam] workspaceId=assistant threadRef=main sessionId=sess-run-01982a...
[vam] Booting isolated Firecracker microVM...
[vam] Agent invoked tool 'write_file' for path '/mnt/drive9/workspace/app-config.json'
[vam] File written successfully (68 bytes).
[vam] Run COMPLETED in 5.2s.
[vam] Final Answer: Created /mnt/drive9/workspace/app-config.json with staging configuration.
```

---

## Step 2: Launch Turn 2 (Multi-Turn Conversation on Same Thread)

In follow-up prompts on the same workspace and thread (`main`), the assistant automatically remembers previous turns and workspace files:

```bash
vam run --file ./examples/bun-assistant \
  --prompt "Read app-config.json and add a new database connection block with host 'db.internal.acme.local'." \
  --follow
```

### What Happens Behind the Scenes:
1. VAM boots a fresh microVM and mounts the same `volume://assistant` Drive9 volume.
2. VAM replays previous turns from `.vam/threads/main/timeline.jsonl` into the agent's context.
3. The agent reads the existing `app-config.json`, modifies the content to include the database connection block, writes the updated file, and finishes.

---

## Step 3: Inspecting Conversation History

The conversation timeline is preserved as immutable CloudEvents records inside the workspace volume:

```text
/mnt/drive9/workspace/.vam/
├── workspace.json                  # Bound Agent URN and metadata
└── threads/
    └── main/
        ├── thread.json             # Thread state and revision counter
        └── timeline.jsonl          # CloudEvents JSONL turn history
```

---

## Next Steps

- Explore [Playbook DAGs](/authoring/playbook-spec/) to automate complex multi-step tasks.
- Check [Writing In-VM Tools](/tools/writing-tools/) to author custom guest tools.
