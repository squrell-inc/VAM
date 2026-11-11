---
title: Jira Assistant Tutorial (HITL Demo)
description: Step-by-step tutorial running the autonomous Jira Assistant, configuring secrets, and completing a Human-in-the-Loop transition approval.
---

This tutorial guides you through configuring and running the **Jira Assistant** agent (`examples/jira-assistant`). You will store required API secrets in the host vault, launch an autonomous prompt to transition a ticket, and interact with the Human-in-the-Loop (HITL) approval gate.

---

## Prerequisites

Before starting:
1. Ensure the `vamd` orchestrator daemon is running (`cargo run -p vam-orchestrator`).
2. Ensure the `vam` CLI is installed and configured (`vam health`).
3. Have an LLM provider key ready (e.g. OpenRouter, OpenAI, Anthropic, or Gemini).
4. *(Optional)* Publish the Jira skill package: `vam skill publish examples/jira-assistant/skills/jira-integration`. The HITL demo still boots from the example seed if you skip this step.

---

## Step 1: Configure Host Vault Secrets

Store the LLM inference key and Jira Cloud credentials in the encrypted host vault. The credentials are encrypted on the host and never exposed to the guest microVM:

```bash
# 1. Store LLM API key for host-brokered inference
vam secrets set openrouter/api-key \
  --kind inference \
  --from-env OPENROUTER_API_KEY

# 2. Store Jira base URL (mock tenant origin)
vam secrets set jira/base-url \
  --kind egress \
  --value "https://acme.atlassian.net" \
  --allowed-destination "*.atlassian.net"

# 3. Store Jira basic auth header
vam secrets set jira/auth-header \
  --kind egress \
  --value "Basic bW9ja191c2VyOm1vY2tfdG9rZW4=" \
  --allowed-destination "*.atlassian.net"
```

Verify secrets are registered in the vault:

```bash
vam secrets list
```

---

## Step 2: Review the Agent & Policy Configuration

Inspect the files in `examples/jira-assistant/`:

- **[`agent.json`](/authoring/agent-spec/)**: Binds the `jira-integration` skill dependency and defines reasoning budgets.
- **[`run.json`](/authoring/run-spec/)**: Sets the prompt: `"Search for issue VAM-123, then transition it to In Progress."`.
- **[`policy.cedar`](/security/cedar-and-hitl/)**: Authorizes `jira_search_issues` automatically, but requires `context.hitl_approved == true` for `jira_transition_issue`.

---

## Step 3: Launch the Execution Run

In your first terminal, launch the run with the `--follow` flag:

```bash
vam run --file examples/jira-assistant/run.json --follow
```

### What You Will Observe in the Terminal:
```text
[vam] workspaceId=assistant threadRef=main sessionId=sess-run-019182...
[vam] Booting isolated Firecracker microVM...
[vam] Mounted skill: jira-integration (read-only at /mnt/drive9/skills/jira-integration)
[vam] Step 1: Agent calls 'jira_search_issues' for issue 'VAM-123'
[vam] Search completed: Found issue VAM-123 ('Implement zero-trust egress proxy', Status: 'Open')
[vam] Step 2: Agent attempts to call 'jira_transition_issue' to status 'In Progress'
[vam] Action requires human authorization (Cedar policy rule: hitl_approved == true)
[vam] Run SUSPENDED. Waiting for human approval decision...
```

---

## Step 4: Inspect & Approve the Transition

Open a second terminal window to inspect and resolve the pending approval.

### 1. List Outstanding Approvals
```bash
vam approve ls
```

Output:
```text
APPROVAL ID                                           AGENT                               ACTION                 STATUS
urn:vam:approval:0198a2e1-4821-7291-a1b2-c98129e01234  urn:vam:agent:jira-assistant        jira_transition_issue  PENDING
```

### 2. Inspect the Proposed Tool Arguments
```bash
vam approve get urn:vam:approval:0198a2e1-4821-7291-a1b2-c98129e01234
```

Output:
```json
{
  "approvalId": "urn:vam:approval:0198a2e1-4821-7291-a1b2-c98129e01234",
  "runId": "urn:vam:run:local.vam.dev/vam/jira-run-001",
  "toolName": "jira_transition_issue",
  "arguments": {
    "issue_key": "VAM-123",
    "transition_id": "In Progress"
  },
  "riskLevel": "MUTABLE_EXTERNAL",
  "status": "PENDING"
}
```

### 3. Submit Approval Decision
```bash
vam approve decide urn:vam:approval:0198a2e1-4821-7291-a1b2-c98129e01234 \
  --decision approved
```

---

## Step 5: Verify Completion

Return to the first terminal window. You will see:

```text
[vam] Human decision received: APPROVED.
[vam] Resuming microVM execution with context.hitl_approved: true...
[vam] Tool 'jira_transition_issue' executed successfully via zero-trust proxy.
[vam] Run COMPLETED in 12.4s.
[vam] Final Output: Issue VAM-123 has been transitioned to 'In Progress'.
```

---

## Next Steps

- Explore workspace file manipulation in the [Bun Assistant Tutorial](/tutorials/bun-assistant/).
- Learn about [Playbook DAGs](/authoring/playbook-spec/) to chain multi-step workflows.
