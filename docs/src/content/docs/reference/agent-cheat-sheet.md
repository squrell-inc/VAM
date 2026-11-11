---
title: AI Agent & Developer Cheatsheet
description: Compact, machine-readable cheat sheet optimized for AI coding assistants and developers.
---

This cheatsheet provides a prompt-ready summary of VAM primitives, JSON schemas, URN conventions, in-VM tool execution patterns, and CLI syntax.

---

## 1. Core Primitives & URN Formats

| Entity | URN Scheme | Example |
|---|---|---|
| **Agent** | `urn:vam:agent:<domain>/<name>` | `urn:vam:agent:local.vam.dev/vam/jira-assistant` |
| **Skill** | `urn:vam:skill:<domain>/<name>` | `urn:vam:skill:local.vam.dev/vam/jira-integration` |
| **Policy** | `urn:vam:policy:<domain>/<name>` | `urn:vam:policy:local.vam.dev/vam/jira-egress-only` |
| **Playbook**| `urn:vam:playbook:<domain>/<name>` | `urn:vam:playbook:local.vam.dev/vam/sprint-dag` |
| **Workspace** | `volume://<workspace-id>` | `volume://assistant` |
| **Thread** | String identifier | `main`, `feature-login`, `sprint-24` |
| **Approval** | `urn:vam:approval:<uuid>` | `urn:vam:approval:0198a2e1-4821-7291-a1b2-c98129e01234` |

---

## 2. Specification Skeletons

### `agent.json` (`vam.v1.AgentSpec`)
```json
{
  "resourceId": { "urn": "urn:vam:agent:local.vam.dev/vam/my-assistant" },
  "displayName": "My Assistant",
  "description": "Autonomous task assistant",
  "role": "You are a specialized assistant.",
  "systemInstructions": "Call tools as needed to complete the user goal.",
  "modelRequirements": {
    "minContextTokens": 64000,
    "requiresToolCalling": true,
    "requiresStructuredOutput": true,
    "requiredModalities": ["text"]
  },
  "preferredModels": [
    { "provider": "openrouter", "modelName": "openai/gpt-4o-mini", "temperature": 0.1, "maxCompletionTokens": 4096 },
    { "provider": "anthropic", "modelName": "claude-3-5-sonnet-latest", "temperature": 0.1, "maxCompletionTokens": 4096 }
  ],
  "contextPolicy": { "maxTokenCount": 64000, "maxHistoryTurns": 20, "compaction": "COMPACTION_POLICY_SUMMARIZE_MIDDLE" },
  "reasoningPolicy": { "maxIterations": 20, "stepTimeout": "180s", "termination": "TERMINATION_POLICY_EXPLICIT_CLAIM" },
  "skillRefs": [
    {
      "resourceId": { "urn": "urn:vam:skill:local.vam.dev/vam/my-skill" }
    }
  ],
  "resourceRequirements": { "vcpuCount": 2, "memoryMib": 2048, "storageGib": 10, "networkMode": "NETWORK_MODE_RESTRICTED_EGRESS" },
  "allowAnyPlaybook": true
}
```

### `run.json` (`vam.v1.RunSpec`)
```json
{
  "agentRef": {
    "resourceId": { "urn": "urn:vam:agent:local.vam.dev/vam/my-assistant" },
    "version": { "major": 1, "minor": 0, "patch": 0 }
  },
  "workspaceRefUri": "volume://assistant",
  "threadRef": "main",
  "inputs": {
    "prompt": "Find issue VAM-123 and update status to In Progress"
  },
  "maxDuration": "1800s",
  "maxFinancialBudget": { "currency": "USD", "micros": 1000000 }
}
```

### `SKILL.md` (`vam.v1.SkillManifest`)
```markdown
---
id: "urn:vam:skill:local.vam.dev/vam/my-skill"
name: "My Custom Skill"
version: "1.0.0"
description: "Provides custom tool capabilities"
tools:
  - name: "custom_query"
    description: "Queries data"
    input_schema:
      type: "object"
      properties:
        query: { type: "string", description: "Search query" }
      required: ["query"]
    exec:
      command: "bun"
      entrypoint: "./execute.js"
      default_args: ["query"]
    required_secrets: ["api/token"]
    side_effect: "READ_ONLY"
    timeout: "30s"
---

# Operational Guidelines
1. Always validate arguments before calling custom_query.
```

---

## 3. In-VM Tool Protocol (`execute.js`)

```javascript
#!/usr/bin/env bun
import { readFileSync } from "fs";

async function main() {
  const action = process.argv[2];
  let input = {};
  try {
    const raw = readFileSync(0, "utf-8").trim();
    if (raw) input = JSON.parse(raw);
  } catch (err) {
    console.error(JSON.stringify({ error: `Invalid JSON stdin: ${err.message}` }));
    process.exit(1);
  }

  if (action === "query") {
    // Return JSON output to stdout
    console.log(JSON.stringify({ success: true, result: `Processed ${input.query}` }));
    process.exit(0);
  } else {
    console.error(JSON.stringify({ error: `Unknown action: ${action}` }));
    process.exit(1);
  }
}
main();
```

---

## 4. Cedar Security Policy Templates (`policy.cedar`)

```cedar
// 1. Safe read-only tools are allowed automatically
permit (
    principal == Agent::"urn:vam:agent:local.vam.dev/vam/my-assistant",
    action == Action::"ExecuteTool",
    resource == Tool::"custom_query"
);

// 2. Mutating tools require explicit human approval (HITL)
permit (
    principal == Agent::"urn:vam:agent:local.vam.dev/vam/my-assistant",
    action == Action::"ExecuteTool",
    resource == Tool::"custom_mutate"
)
when {
    context has hitl_approved && context.hitl_approved == true
};

// 3. Workspace file access boundaries
permit (
    principal,
    action == Action::"ReadFile",
    resource
)
when {
    context has path &&
    (context.path like "/mnt/drive9/workspace/*" || context.path like "/mnt/drive9/skills/*")
};

permit (
    principal,
    action == Action::"WriteFile",
    resource
)
when {
    context has path && context.path like "/mnt/drive9/workspace/*" &&
    !(context.path like "/mnt/drive9/skills/*")
};
```

---

## 5. CLI Command Reference

```bash
# 1. Manage Runs
vam run --file examples/jira-assistant/run.json --follow
vam run ls
vam run get <RUN_ID>
vam run cancel <RUN_ID>

# 2. Human-in-the-Loop Approvals
vam approve ls
vam approve get <APPROVAL_URN>
vam approve decide <APPROVAL_URN> --decision approved|rejected|amended [--amended-args JSON]

# 3. Host Secrets Vault
vam secrets set openrouter/api-key --kind inference --from-env OPENROUTER_API_KEY
vam secrets set vercel/api-key --kind inference --from-env AI_GATEWAY_API_KEY
vam secrets set jira/base-url --kind egress --value "https://acme.atlassian.net" --allowed-destination "*.atlassian.net"
vam secrets set jira/auth-header --kind egress --from-env JIRA_AUTH_HEADER --allowed-destination "*.atlassian.net"
vam secrets list

# 4. Catalog Management
vam agent put examples/jira-assistant/agent.json
vam agent get urn:vam:agent:local.vam.dev/vam/jira-assistant
vam skill publish examples/jira-assistant/skills/jira-integration
vam skill put examples/jira-assistant/skills/jira-integration/manifest.json
vam playbook put examples/jira-assistant/playbook.json

# 5. Node Diagnostics
vam health
vam node status
```
