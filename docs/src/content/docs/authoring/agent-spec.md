---
title: Agent Specifications (agent.json)
description: Complete guide to authoring declarative agent configurations with AgentSpec in VAM.
---

Read [Authoring model](/authoring/model/) first. It is the map of drafts, releases, floating references, skills, playbooks, and policies. This page is the field reference for `agent.json`.

The **`agent.json`** file defines the identity, behavioral instructions, LLM preferences, execution budgets, and security boundaries for an autonomous agent in VAM.

It maps directly to the **`vam.v1.AgentSpec`** protobuf message (`proto/vam/v1/agentic_spec.proto`).

---

## Annotated Example: `agent.json`

Below is a complete, production-ready `agent.json` configuration based on the bundled Jira assistant:

```json
{
  "resourceId": {
    "urn": "urn:vam:agent:local.vam.dev/vam/jira-assistant"
  },
  "displayName": "Jira Assistant Agent",
  "description": "An autonomous agent designed to query sprint backlogs, update Jira issues, and coordinate deliverables.",
  "role": "You are a professional Agile Project Assistant and Jira Specialist.",
  "systemInstructions": "Your baseline mission is to maintain the hygiene of Jira project boards. Call load_skill with catalog id jira-integration before using Jira tools (jira_search_issues, jira_create_issue, jira_transition_issue, jira_add_comment). Always query current sprint/ticket states first to ensure latest context. Remain polite, professional, and clear. Avoid duplicate ticket creation by searching before calling create.",
  "modelRequirements": {
    "minContextTokens": 128000,
    "requiresToolCalling": true,
    "requiresStructuredOutput": true,
    "requiredModalities": ["text"]
  },
  "preferredModels": [
    {
      "provider": "openrouter",
      "modelName": "openai/gpt-4o-mini",
      "temperature": 0.1,
      "maxCompletionTokens": 4096
    },
    {
      "provider": "anthropic",
      "modelName": "claude-3-5-sonnet-latest",
      "temperature": 0.2,
      "maxCompletionTokens": 4096
    },
    {
      "provider": "google",
      "modelName": "gemini-2.5-pro",
      "temperature": 0.1,
      "maxCompletionTokens": 8192
    }
  ],
  "contextPolicy": {
    "maxTokenCount": 96000,
    "maxHistoryTurns": 30,
    "compaction": "COMPACTION_POLICY_SUMMARIZE_MIDDLE"
  },
  "reasoningPolicy": {
    "maxIterations": 50,
    "stepTimeout": "300s",
    "termination": "TERMINATION_POLICY_EXPLICIT_CLAIM"
  },
  "skillRefs": [
    {
      "resourceId": {
        "urn": "urn:vam:skill:local.vam.dev/vam/jira-integration"
      }
    }
  ],
  "capabilityPolicy": {
    "policyRef": {
      "resourceId": {
        "urn": "urn:vam:policy:local.vam.dev/vam/jira-egress-only"
      }
    },
    "securityProfileName": "jira-only-profile"
  },
  "hitlPolicy": {
    "policyRef": {
      "resourceId": {
        "urn": "urn:vam:policy:local.vam.dev/vam/high-risk-jira-actions"
      }
    },
    "monitoredTools": [
      "jira_delete_issue",
      "jira_transition_issue"
    ]
  },
  "telemetryPolicy": {
    "policyRef": {
      "resourceId": {
        "urn": "urn:vam:policy:local.vam.dev/vam/standard-telemetry-compliance"
      }
    },
    "piiRedactionEnabled": true,
    "auditLedgerMode": "AUDIT_LEDGER_MODE_HASH_CHAINED"
  },
  "resourceRequirements": {
    "vcpuCount": 2,
    "memoryMib": 2048,
    "storageGib": 10,
    "networkMode": "NETWORK_MODE_RESTRICTED_EGRESS"
  },
  "allowAnyPlaybook": true,
  "allowedPlaybookRefs": [],
  "metadata": {
    "revision": 1,
    "createdBy": {
      "urn": "urn:vam:user:local.vam.dev/vam/admin"
    },
    "labels": [
      { "key": "environment", "value": "production" },
      { "key": "domain", "value": "agile-ops" }
    ]
  }
}
```

---

## Authored References

Every reference in an authored spec is a URN: `skillRefs`, the policy refs, `defaultPlaybookRef`, `allowedPlaybookRefs`, and `extends`. You write the URN and let the server pin it.

| What you write | What it means |
|---|---|
| URN only | The ref floats. Draft runs resolve it. Release freezes it. |
| URN plus `version` | The ref names one version. Release still freezes it to a digest. |
| URN plus `version` plus `digest` | Accepted by validation, but do not write it. Digests are recorded by the server at release. |

A digest needs a version. A digest without one is rejected.

Resolution depends on what you run:

- **Draft run.** Each ref resolves to the agent owner's draft of the dependency if one exists. Otherwise it resolves to the latest release.
- **Release.** Every ref is frozen to a version and a digest in the released spec.
- **Release with a draft-only dependency.** The release fails. A draft cannot be frozen, so publish the dependency first.

Frozen pins appear in three places: the released spec, `vam agent versions`, and the run record.

:::note[Reading a frozen release]
A released spec shows `version` and `digest` on each ref. The server wrote them. They describe what that release runs, so do not copy them back into a draft. Edit the URN-only form and release again.
:::

---

## Section Breakdown

### 1. Identity & Prompts

| Field | Type | Description |
|---|---|---|
| `resourceId.urn` | `string` | Globally unique identifier, format: `urn:vam:agent:<domain>/<name>` |
| `displayName` | `string` | Short human-readable title |
| `role` | `string` | Professional role statement establishing the LLM's persona |
| `systemInstructions` | `string` | Baseline operational guidelines injected into the system prompt |

:::tip[Layered Prompts]
`systemInstructions` acts as **Layer 1** (the agent's baseline identity). When the agent dynamically loads skills via `load_skill`, each skill's markdown body is appended as **Layer 2** operational instructions.
:::

---

### 2. Model Requirements & Preferences

VAM decouples the agent's functional requirements from specific LLM providers:

- **`modelRequirements`**: Declares what capabilities the agent needs:
  - `minContextTokens`: Minimum context window size (e.g. `128000`).
  - `requiresToolCalling`: Must support JSON function/tool calling (`true`).
  - `requiresStructuredOutput`: Must support structured schema response parsing (`true`).
- **`preferredModels`**: Ordered list of models. The host inference broker tries models in sequence based on availability and configured secrets:
  - Providers: `"openrouter"`, `"vercel"`, `"anthropic"`, `"google"`, `"openai"`.
  - The declared `provider` is honoured, including for slash-style model ids: when a run uses a model that matches a `preferredModels` entry (or the host injects the first entry as the default), the host passes that entry's provider to the guest as the host-owned `model_provider` input. An agent declaring `{"provider": "vercel", "modelName": "anthropic/claude-haiku-5.5"}` routes through the Vercel AI Gateway rather than OpenRouter. Models with no declared provider keep the old inference (`mock-model` → `mock`, ids containing `/` → `openrouter`).
  - `temperature`: Recommended sampling temperature (e.g. `0.1` for deterministic coding/tool actions).
  - `maxCompletionTokens`: Maximum token output per generation turn.

---

### 3. Execution & Context Policies

Guardrails to prevent runaway token costs and infinite tool loops:

```json
"contextPolicy": {
  "maxTokenCount": 96000,
  "maxHistoryTurns": 30,
  "compaction": "COMPACTION_POLICY_SUMMARIZE_MIDDLE"
},
"reasoningPolicy": {
  "maxIterations": 50,
  "stepTimeout": "300s",
  "termination": "TERMINATION_POLICY_EXPLICIT_CLAIM"
}
```

- **`contextPolicy`**:
  - `maxTokenCount`: Triggers compaction when the conversation window reaches this limit.
  - `maxHistoryTurns`: Maximum previous interaction turns preserved in memory.
  - `compaction`: Strategy for handling overflow (`COMPACTION_POLICY_SUMMARIZE_MIDDLE` compresses middle turns while preserving initial instructions and latest outputs).
- **`reasoningPolicy`**: `vamd` resolves these once in `create_run` and writes them onto the stored run. Dispatch, resume, and active-time expiry read those stored values.
  - `maxIterations`: Turns per task. Unset or `0` uses the default of 50. A value above 200 stays valid on the agent and is clamped to 200 on the run. A caller may send `inputs.max_iterations` to lower the resolved budget.
  - `stepTimeout`: Time limit for one ReAct round. Default 5 minutes (`"300s"`). `vamd` writes `inputs.step_timeout_secs` from this field. A value sent by the caller is left unused.
  - `termination`: When the task counts as finished. `vamd` writes `inputs.termination` (`explicit_claim` or `output_satisfied`; removed when unset) onto the stored run, so a resume reuses it.
    - Unset: a model reply without tool calls ends the task.
    - `TERMINATION_POLICY_EXPLICIT_CLAIM`: the model is offered a `task_complete(summary)` tool. A reply without tool calls does not end the task: the model gets one neutral re-prompt, and a second text-only reply ends the run with `claimed: false`. A `task_complete` call ends it with `claimed: true`. `claimed` is in `vam run get` under `finalOutputs`. A bound playbook is still checked before the run ends. `task_complete` is handled inside the guest and has no side effect of its own.
    - `TERMINATION_POLICY_OUTPUT_SATISFIED`: the playbook output check decides, for a playbook that declares an output schema. Otherwise the run behaves as if termination were unset, and `vam agent check` prints a warning.

---

### 4. Skill Dependencies (`skillRefs`)

Agents declare their tool capabilities by skill URN. Do not add a version or a digest:

```json
"skillRefs": [
  {
    "resourceId": {
      "urn": "urn:vam:skill:local.vam.dev/vam/jira-integration"
    }
  }
]
```

A draft run resolves the skill to the agent owner's draft, or to the latest release if there is no draft. Release freezes the ref to a version and digest. At MicroVM boot time, the orchestrator verifies the frozen skill's digest and mounts the skill directory as a read-only filesystem at `/mnt/drive9/skills/<skill-id>/`.

The guest prompt lists only tools the host advertises for this run. A skill that is not in `skillRefs` does not contribute tools.

---

### 5. Playbooks the agent may run

```json
{
  "defaultPlaybookRef": {
    "resourceId": { "urn": "urn:vam:playbook:local.vam.dev/vam/jira-sprint-backlog-cleanup" }
  },
  "allowedPlaybookRefs": [
    {
      "resourceId": { "urn": "urn:vam:playbook:local.vam.dev/vam/jira-sprint-backlog-cleanup" }
    }
  ],
  "allowAnyPlaybook": false
}
```

Write URNs only. `defaultPlaybookRef` is the procedure a run uses when it does not name one. `allowedPlaybookRefs` is the set a run may select. If that list is empty and `allowAnyPlaybook` is false, no playbook can be attached. `allowAnyPlaybook: true` is the explicit escape hatch.

`vam agent check <urn> --draft` compares those playbooks' `requires.skills` with `skillRefs`. It reports a missing skill and exits 0. Release and run admission do not consult that report.

Removing a skill or a playbook, or turning `allowAnyPlaybook` off, suggests a major bump. Adding one suggests a minor bump.

---

### 6. Extending an upstream agent (`extends`)

`extends` inherits a pinned agent. It does not call that agent. The child is still one agent, with a delta on top of the parent definition.

```json
"extends": {
  "resourceId": { "urn": "urn:vam:agent:hub.vam.dev/acme/support-bot" }
},
"overlay": {
  "instructions": { "append": "Reply in Hindi or English. INR only." },
  "skills": {
    "add": [{ "resourceId": { "urn": "urn:vam:skill:hub.vam.dev/acme/internal-kb" } }],
    "remove": [{ "resourceId": { "urn": "urn:vam:skill:hub.vam.dev/acme/legacy-crm" } }]
  },
  "capabilities": {
    "grant": ["egress:example.com"],
    "revoke": ["egress:amazonaws.com"]
  }
}
```

`extends` takes an agent URN. A draft may float, and a draft run resolves it like any other ref. Release freezes it to a version and digest. The URN kind is `agent`. An extending agent leaves `systemInstructions` and `skillRefs` empty. The overlay is the only delta. `append` joins the parent instructions, a blank line, and the child text. `replace` keeps the child text. `remove` drops a skill by URN, then `add` runs. `grant` then `revoke` edit the capability set.

Chains are allowed, one parent at a time, up to 8 hops. Two rules reject a merge:

- Revoking a capability that a skill still in the effective set requires. Remove that skill, or leave the capability in place. This includes a skill the child itself added.
- Adding a skill URN the effective parent set already has at a different version or digest. Neither pin wins. Remove the parent skill in the same overlay and add the new pin to change it on purpose.

`vam-spec`'s `merge_agent_chain` applies the chain. Run admission still stores the child spec as written.

---

### 7. Permissions

New agents declare what they may do as data on `permissions`. A rule is a target plus `allow`, `ask`, or `deny`. Cedar is generated from these rules. Hand-written Cedar, when you still need it, is a policy reference on `permissions.advanced`.

`capabilityPolicy` and `hitlPolicy` are the legacy path. A spec may use those fields, or `permissions`, but not both.

```json
"permissions": {
  "tools": {
    "default": "bySideEffect",
    "rules": [
      { "target": "skill:jira/create_issue", "decision": "ask" },
      { "target": "mcp:mcp.atlassian.com/*", "decision": "allow" },
      { "target": "builtin:run", "decision": "deny" }
    ]
  },
  "internet": {
    "default": "allow",
    "rules": [ { "target": "*.pastebin.com", "decision": "deny" } ]
  },
  "web": { "enabled": true },
  "advanced": []
}
```

Publishing a draft that has neither `permissions` nor `capabilityPolicy` / `hitlPolicy` stores the new-agent defaults explicitly: `tools.default` is `bySideEffect` and `internet.default` is `allow`. A draft that already has `permissions` is stored as written, and a legacy draft is left unchanged until it is migrated. Released versions are not rewritten. Inside a `permissions` object, an omitted default stays unspecified and still resolves to those same values. `bySideEffect` allows read-only and mutable-local tools and asks for mutable-external or unspecified tools. Host rules are `allow` or `deny`; `ask` is rejected.

| Target | Form | Example |
|---|---|---|
| A skill tool | `skill:<skill name>/<tool>` | `skill:jira/create_issue` |
| A skill source | `skill:<skill name>/*` | `skill:jira/*` |
| An MCP tool | `mcp:<server>/<tool>` | `mcp:mcp.atlassian.com/v1/create_issue` |
| An MCP source | `mcp:<server>/*` | `mcp:mcp.atlassian.com/*` |
| A built-in tool | `builtin:<tool>` | `builtin:run` |
| Every tool | `tools:*` | |
| A host | a DNS name, or one leading wildcard label | `api.github.com`, `*.pastebin.com` |
| Every host | `internet:*` | |

The skill name is a lowercase slug. A tool token is 1–128 characters of ASCII letters, digits, `_`, `-`, and `.`, and it keeps the vendor's case (`createIssue` and `create_issue` are different tools).

**Canonical MCP server id.** The server in an `mcp:` target is origin plus path, normalised the same way the host normalises `McpConnection.url`:

* Scheme and host are lowercased. When no scheme is written, `https` is implied. Only `http` and `https` are accepted.
* The default port is dropped (`443` for https, `80` for http). Any other port is kept.
* A trailing slash is dropped. The path is case-sensitive. Empty segments, `.`, and `..` are rejected.
* Query (`?`), fragment (`#`), and userinfo (`@`) are rejected.
* `https` is omitted from the id. `http` is written out. `https://MCP.Atlassian.com:443/v1/` and `mcp.atlassian.com/v1` are both `mcp.atlassian.com/v1`.
* Two servers on one host differ by path: `mcp.atlassian.com/v1` and `mcp.atlassian.com/v2` are different.
* The last path segment of a target is the tool, not part of the server. `mcp:mcp.atlassian.com/v1/create_issue` is tool `create_issue` on server `mcp.atlassian.com/v1`.

A wildcard host is one leading `*` label only (`*.pastebin.com`, not `foo.*.com`). `*.example.com` matches `api.example.com` and `sub.api.example.com`, and does not match the apex `example.com`. Within one list, the most specific rule wins: an exact tool beats a source wildcard, which beats `tools:*`; an exact host beats a wildcard host (a longer suffix is more specific), which beats `internet:*`.

`permissions.web.enabled` switches the first-party web tools on. It is off when omitted, and agents on the legacy policy path never get it. Set it with `vam agent permissions web <agent> on|off`; see [Web broker](/hosted/web/).

`examples/permissions-assistant/agent.json` is a full agent that uses this block and no legacy policy.

### 8. Security and governance policies

Specs that have not moved to `permissions` still bind Cedar directly:

- **`capabilityPolicy`**: Binds the AWS Cedar authorization policy governing which tools and file paths the agent may access.
- **`hitlPolicy`**: Declares which high-risk actions (e.g. `jira_transition_issue`, `jira_delete_issue`) require explicit human sign-off before execution.
- **`telemetryPolicy`**: Enables automated PII/secret scrubbing and configures cryptographic audit logging (`AUDIT_LEDGER_MODE_HASH_CHAINED`).
- **`resourceRequirements`**: Specifies guest microVM hardware limits:
  - `vcpuCount`: Allocated virtual CPUs (e.g. `2`).
  - `memoryMib`: Guest RAM in MiB (e.g. `2048`).
  - `networkMode`: `NETWORK_MODE_RESTRICTED_EGRESS` restricts network access exclusively through the host security proxy.

---

## Registering an Agent with `vam` CLI

To register your `agent.json` in the VAM catalog:

```bash
vam agent put examples/jira-assistant/agent.json
```

That saves your draft. Retrieve it, check composition, and release:

```bash
vam agent get urn:vam:agent:local.vam.dev/vam/jira-assistant
vam agent check urn:vam:agent:local.vam.dev/vam/jira-assistant --draft
vam agent suggest-version urn:vam:agent:local.vam.dev/vam/jira-assistant
vam agent release urn:vam:agent:local.vam.dev/vam/jira-assistant --note "Sprint cleanup"
vam agent versions urn:vam:agent:local.vam.dev/vam/jira-assistant
```

On a terminal, `release` with no `--version` prints the suggestion and asks. Without a terminal, pass `--version`. The same commands exist for `vam playbook` and `vam policy`. Skill packages use `vam skill publish` and `vam skill release`. See [Authoring model](/authoring/model/#draft-and-release).

---

## Next Steps

- Learn how to define skills and tools in [Skills & Frontmatter (`SKILL.md`)](/authoring/skill-manifest/).
- Learn how to launch execution runs in [Run Specifications (`run.json`)](/authoring/run-spec/).
- Explore multi-step workflow graphs in [Playbook DAGs (`playbook.json`)](/authoring/playbook-spec/).
