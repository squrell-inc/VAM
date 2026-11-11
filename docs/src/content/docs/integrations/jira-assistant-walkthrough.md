---
title: Jira Assistant Integration Walkthrough
description: End-to-end case study of building an autonomous Jira assistant with zero-trust egress and Human-in-the-Loop approval.
---

This case study walks through how the bundled **Jira Assistant** (`examples/jira-assistant`) was built, configured, and executed using VAM's zero-trust proxy and Cedar HITL security policies.

---

## Architecture of the Integration

The Jira integration consists of five core components:

```
examples/jira-assistant/
├── agent.json              # AgentSpec: role, LLMs, and cryptographic skill pins
├── policy.cedar            # Cedar rules: permits search, requires HITL for transition
├── run.json                # RunSpec: prompt targeting issue VAM-123
└── skills/
    └── jira-integration/
        ├── SKILLS.md       # SkillManifest: tool schemas and operational guidelines
        └── execute.js      # Tool executor script with in-guest proxy networking
```

---

## Step 1: Defining Tools in `SKILL.md`

In `SKILLS.md`, we declare four distinct tools:

```yaml
tools:
  - name: "jira_search_issues"
    description: "Queries Jira issues using JQL."
    input_schema:
      type: "object"
      properties:
        jql: { type: "string" }
      required: ["jql"]
    exec:
      command: "bun"
      entrypoint: "./execute.js"
      default_args: ["search"]
    side_effect: "READ_ONLY"

  - name: "jira_transition_issue"
    description: "Transitions a Jira issue to a new status."
    input_schema:
      type: "object"
      properties:
        issue_key: { type: "string" }
        transition_id: { type: "string" }
      required: ["issue_key", "transition_id"]
    exec:
      command: "bun"
      entrypoint: "./execute.js"
      default_args: ["transition"]
    side_effect: "MUTABLE_EXTERNAL"
```

Notice that `jira_search_issues` is classified as `READ_ONLY`, while `jira_transition_issue` is classified as `MUTABLE_EXTERNAL`.

---

## Step 2: In-VM Proxy Networking (`execute.js`)

In `execute.js`, the script interacts with Jira Cloud exclusively through the in-guest egress proxy:

```javascript
import { readFileSync } from "fs";
import { connect } from "node:net";

const JIRA_BASE_PLACEHOLDER = "vamsec_jira_base_url";
const JIRA_AUTH_PLACEHOLDER = "vamsec_jira_auth_token";
const PROXY_TCP = process.env.VAM_EGRESS_ENDPOINT || "http://127.0.0.1:3128";

async function jiraFetch(pathAndQuery, options = {}) {
  // 1. Construct target path using scheme-less placeholder
  const target = `${JIRA_BASE_PLACEHOLDER}${pathAndQuery.startsWith('/') ? pathAndQuery : `/${pathAndQuery}`}`;
  
  // 2. Set headers with X-Target-Url and auth token placeholder
  const headers = {
    "Authorization": JIRA_AUTH_PLACEHOLDER,
    "Accept": "application/json",
    "Content-Type": "application/json",
    "X-Target-Url": target,
    ...(options.headers || {})
  };

  // 3. Forward request through in-guest proxy socket
  const proxyUrl = new URL(PROXY_TCP);
  return await httpViaGuestProxy(
    options.method || "GET",
    proxyUrl.hostname,
    Number(proxyUrl.port || 80),
    pathAndQuery,
    headers,
    options.body
  );
}
```

The script never knows the real Jira tenant URL or API credential.

---

## Step 3: Cedar Security Policy (`policy.cedar`)

We write Cedar policies to permit read operations automatically, but gate state mutations behind human approval:

```cedar
// 1. Read-only search is allowed immediately
permit (
    principal == Agent::"urn:vam:agent:local.vam.dev/vam/jira-assistant",
    action == Action::"ExecuteTool",
    resource
)
when {
    [Tool::"jira_search_issues", Tool::"jira_add_comment"].contains(resource)
};

// 2. Transitions require explicit human approval (context.hitl_approved == true)
permit (
    principal == Agent::"urn:vam:agent:local.vam.dev/vam/jira-assistant",
    action == Action::"ExecuteTool",
    resource
)
when {
    [Tool::"jira_transition_issue", Tool::"jira_create_issue"].contains(resource) &&
    context has hitl_approved &&
    context.hitl_approved == true
};
```

---

## Step 4: Storing Vault Secrets

Before running the agent, store the mocked Jira credentials in the host secrets vault:

```bash
# Set base URL
vam secrets set jira/base-url \
  --kind egress \
  --value "https://acme.atlassian.net" \
  --allowed-destination "*.atlassian.net"

# Set authorization header
vam secrets set jira/auth-header \
  --kind egress \
  --value "Basic bW9ja191c2VyOm1vY2tfdG9rZW4=" \
  --allowed-destination "*.atlassian.net"
```

---

## Step 5: End-to-End Execution Trace

### 1. Launch Run
We start the execution with a prompt targeting issue **`VAM-123`**:

```bash
vam run --file examples/jira-assistant/run.json --follow
```

### 2. MicroVM Boot & Search Execution
1. Host boots microVM and mounts `jira-integration` skill read-only.
2. Agent invokes `jira_search_issues` for `VAM-123`.
3. Cedar policy evaluates `permit` $\rightarrow$ search runs through egress proxy and returns issue details.

### 3. Transition Interception & HITL Approval
1. Agent decides to call `jira_transition_issue` to move `VAM-123` to *In Progress*.
2. Cedar PDP evaluates policy: `context.hitl_approved` is `false`.
3. Host **Suspends** the run and registers an approval request.

### 4. Human Review & Resolution
In a second terminal:

```bash
# List pending approval
vam approve ls

# Inspect the request
vam approve get urn:vam:approval:0198a2e1-4821-7291-a1b2-c98129e01234

# Approve the transition
vam approve decide urn:vam:approval:0198a2e1-4821-7291-a1b2-c98129e01234 --decision approved
```

### 5. Resumption & Completion
The host resumes the guest microVM with `context.hitl_approved: true`. The tool executes `bun execute.js transition`, updates the ticket, and completes the run.

---

## Next Steps

- Learn how to author custom Cedar policies in [Cedar Policies & HITL](/authoring/agent-spec/).
- Check the [CLI Command Reference](/getting-started/cli/).
