---
title: Cedar Security Policies & HITL
description: Authoring AWS Cedar access control policies in VAM to govern tool execution, file access, and Human-in-the-Loop gating.
---

VAM uses **AWS Cedar** as its Policy Decision Point (PDP) engine. Cedar evaluates whether an executing agent is authorized to invoke a tool, read or write a file, or perform state-mutating actions.

---

## Annotated Example: `policy.cedar`

Below is a complete Cedar security policy based on the Jira assistant example:

```cedar
// ==============================================================================
// VAM Cedar Security Policy: Jira Assistant
// URN: urn:vam:policy:local.vam.dev/vam/jira-egress-only
// ==============================================================================

// 1. Safe, read-only tools are allowed automatically
permit (
    principal == Agent::"urn:vam:agent:local.vam.dev/vam/jira-assistant",
    action == Action::"ExecuteTool",
    resource
)
when {
    [
        Tool::"jira_search_issues",
        Tool::"jira_add_comment",
        Tool::"read_file",
        Tool::"glob",
        Tool::"grep",
        Tool::"load_skill"
    ].contains(resource)
};

// 2. Mutating tools require explicit human approval (context.hitl_approved == true)
permit (
    principal == Agent::"urn:vam:agent:local.vam.dev/vam/jira-assistant",
    action == Action::"ExecuteTool",
    resource
)
when {
    [
        Tool::"jira_create_issue",
        Tool::"jira_transition_issue"
    ].contains(resource) &&
    context has hitl_approved &&
    context.hitl_approved == true
};

// 3. Workspace read permissions
permit (
    principal,
    action == Action::"ReadFile",
    resource
)
when {
    context has path &&
    (context.path like "/mnt/drive9/workspace/*" ||
     context.path like "/mnt/drive9/skills/*")
};

// 4. Workspace write permissions (bounded by size limit)
permit (
    principal,
    action == Action::"WriteFile",
    resource
)
when {
    context has path &&
    context.path like "/mnt/drive9/workspace/*" &&
    !(context.path like "/mnt/drive9/skills/*") &&
    context has file_size_bytes &&
    context.file_size_bytes <= 10485760 // Max 10 MiB per file write
};

// 5. Explicitly forbid modifying skill definitions
forbid (
    principal,
    action == Action::"WriteFile",
    resource
)
when {
    context has path && context.path like "/mnt/drive9/skills/*"
};
```

---

## Anatomy of Cedar Rules in VAM

Every Cedar policy statement contains four core entities:

```
permit | forbid (
    principal == <Agent URN>,
    action == <Action Type>,
    resource == <Target Tool or Path>
)
when {
    <Boolean Condition Expression>
};
```

### 1. Principals
Identifies the executing agent by its unique URN:
```cedar
principal == Agent::"urn:vam:agent:local.vam.dev/vam/jira-assistant"
```

### 2. Actions
VAM standardizes three action types for policy evaluation:

| Action | Evaluated When | Context Fields Available |
|---|---|---|
| `Action::"ExecuteTool"` | Agent calls a skill tool or host tool | `context.hitl_approved`, `context.arguments_digest`, `context.playbook` (step `id`, `state`, `outcome` when the run has a playbook) |
| `Action::"ReadFile"` | Agent reads a file inside the microVM | `context.path` |
| `Action::"WriteFile"` | Agent creates or updates a file | `context.path`, `context.file_size_bytes` |

### 3. Resources
- For `ExecuteTool`: Represents the tool name (e.g. `Tool::"jira_search_issues"`, `Tool::"jira_transition_issue"`).
- For `ReadFile` / `WriteFile`: Represents the filesystem resource or wildcard.

---

## Enforcing Human-in-the-Loop (HITL)

To protect production systems from unintended state mutations, mutating tools must require `context.hitl_approved == true`:

```cedar
permit (
    principal == Agent::"urn:vam:agent:local.vam.dev/vam/jira-assistant",
    action == Action::"ExecuteTool",
    resource == Tool::"jira_transition_issue"
)
when {
    context has hitl_approved &&
    context.hitl_approved == true
};
```

### How the PDP Evaluates This Rule:
1. When the agent first calls `jira_transition_issue` on issue `VAM-123`, the initial execution context has `context.hitl_approved = false`.
2. The Cedar engine evaluates the rule: since `context.hitl_approved` is `false`, the permit does **not** match.
3. The Policy Enforcement Point (PEP) identifies that this tool is listed in `hitlPolicy.monitoredTools`, transitions the run to **`Suspended`**, and creates a pending approval claim.
4. Once a human approves the request via `vam approve decide`, the host resumes execution with `context.hitl_approved = true`. The permit rule now evaluates to `true`, and the tool executes.

---

## Filesystem Sandbox Rules

Cedar policies also enforce storage isolation:

- **Path Restrictions**: Allow writing only under `/mnt/drive9/workspace/*`.
- **Immutable Skills**: Explicit `forbid` rules prevent an agent from modifying `/mnt/drive9/skills/*`.
- **File Size Caps**: Prevent disk exhaustion by capping single write operations (e.g. `context.file_size_bytes <= 10485760` for 10 MiB).

---

## Next Steps

- Learn how human approvals are reviewed and resolved in [HITL Approval Lifecycle](/security/hitl-lifecycle/).
- Understand how credentials are managed in [Secrets Vault Management](/security/secret-management/).
