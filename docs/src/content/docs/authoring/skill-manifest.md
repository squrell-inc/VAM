---
title: Skills & Frontmatter (SKILL.md)
description: Complete guide to authoring skills, defining tool schemas, configuring unprivileged executors, and writing operational guardrails.
---

Read [Authoring model](/authoring/model/) for drafts, releases, capability tokens, and how an agent attaches a skill. This page is the skill package itself.

A **Skill** in VAM is a self-contained bundle of capabilities consisting of:
1. **Tool Specifications**: Machine-readable JSON Schemas that the LLM uses for function calling.
2. **Execution Contracts**: Unprivileged script commands (`bun`, `node`, `python3`) executed inside the guest microVM.
3. **Operational Guidelines**: Secondary prompt instructions (Layer 2) injected into the agent's context when the skill is loaded.

Skills map to the **`vam.v1.SkillManifest`** protobuf message (`proto/vam/v1/skills.proto`). Author the machine contract in **`manifest.json`**. **`SKILLS.md`** keeps the prose body `load_skill` injects, plus optional `id` / `version` frontmatter that must match the manifest.

---

## Annotated Example

`manifest.json` owns the tools. `localExec.entrypoint` is relative to that file's directory:

```json
{
  "name": "jira_search_issues",
  "description": "Queries Jira issues using Jira Query Language (JQL).",
  "inputSchemaJson": "{\"type\":\"object\",\"properties\":{\"jql\":{\"type\":\"string\"}},\"required\":[\"jql\"]}",
  "localExec": {
    "command": "bun",
    "entrypoint": "./execute.js",
    "defaultArgs": ["search"]
  },
  "sideEffect": "SIDE_EFFECT_CLASS_READ_ONLY",
  "approvalRequired": false
}
```

`SKILLS.md` is the prose injected when the skill is loaded:

```markdown
---
id: "urn:vam:skill:local.vam.dev/vam/jira-integration"
name: "Jira Integration Skill"
version: "1.0.0"
description: "Provides capabilities to search, create, transition, and comment on Jira issues"
---

# Jira Integration Operational Guidelines

When using this skill to manage sprint boards, follow these operational rules:

1. **Query Before Action**: Always check the issue status via search before initiating a transition.
2. **Comment Format**: Comments added to tickets must contain a clear automated notification prefix:
   - Example: `[VAM Agent Notification] Sprint Backlog Cleanup: ...`
3. **No Duplicate Issues**: Before creating a task or bug, search Jira using a JQL summary filter to confirm an identical issue does not already exist.
4. **Error Handling**: If a transition fails due to permissions, log the failure and request manual human approval.
```

Publish checks `id` and `version` against `manifest.json`. A frontmatter `tools` list is rejected when it disagrees with the manifest.

A skill may ship procedures in `playbooks`. Each entry is a path relative to the directory that contains `manifest.json`, for example `./playbooks/sprint-cleanup.json`. Absolute paths and `..` are rejected. Those files are procedures the skill publishes, not a scheduler.

---

## The Dual-Layer Architecture

VAM uses a two-layer design to separate machine tool calling from human operational guidance:

```
┌─────────────────────────────────────────────────────────────────────────────┐
│ LAYER 1: manifest.json (Machine Specification)                              │
│   • Exposed to the LLM as function definitions (OpenAI / Anthropic tools)   │
│   • Configures in-VM subprocess invocation (bun ./execute.js ...)           │
│   • Declares required host vault secrets and side-effect categories         │
├─────────────────────────────────────────────────────────────────────────────┤
│ LAYER 2: SKILLS.md BODY (Human & Operational Guidelines)                    │
│   • Dynamically injected into the agent's prompt when load_skill is called  │
│   • Establishes best practices, guardrails, comment templates, and safety   │
└─────────────────────────────────────────────────────────────────────────────┘
```

---

## Manifest Field Reference

These fields live in `manifest.json`. Tool fields are `ToolSpec`. `SKILLS.md` does not restate them.

### Skill Metadata

| Field | Type | Description |
|---|---|---|
| `resourceId.urn` | `string` | Unique URN: `urn:vam:skill:<domain>/<namespace>/<name>`. Local use cases use `local.vam.dev`. `examples.local` is reserved |
| `version` | object | `{ "major", "minor", "patch" }` on the manifest. Frontmatter in `SKILLS.md` must match it. The catalog draft is not this number; release chooses the published version |
| `description` | `string` | Brief description used for skill discovery |
| `systemExtension` | `string` | Short tool-oriented instructions stored on the manifest. The longer operational guide stays in the `SKILLS.md` body |
| `capabilities` | `string[]` | Consent tokens: `tool:`, `egress:`, `secret:`, or `fs:` plus a non-empty remainder. Shown by `vam agent check`. Not a substitute for Cedar |
| `playbooks` | `string[]` | Paths relative to this file, such as `./playbooks/sprint-cleanup.json`. Published with the package. The agent still has to allow the playbook |
| `requiresRuntime` | `map<string,string>` | Runtimes the skill's scripts need from the node's base image, such as `{ "python": ">=3.12", "git": "*" }`. See [`requiresRuntime`](#requiresruntime) |
| `policy` | `string` | Optional Cedar fragment. Stored, not enforced. Put allow and deny rules in a policy artifact the agent references |

#### `requiresRuntime`

A map from runtime name (`python`, `git`, `bun`, `curl`, `jq`) to a requirement. The syntax is `>=X[.Y[.Z]]` (at least), `=X[.Y[.Z]]` (exactly) or `*` (any version, same as empty). Nothing else is accepted. The node compares it with the runtimes its base image reports (`GET /v1/node`, `vam node status`) when the package is **installed** and again at **release**, and refuses a skill the base cannot run. The message names the skill, the requirement, the base and what it has, for example `skill urn:vam:skill:acme/x/y needs java >=21; this node's base vam/base:2 has no java`. A skill without the field is never affected. See [Installation](/getting-started/installation/#the-standard-base-vambase2).

---

### Tool Specification (`tools[]`)

Each item in the `tools` list represents a discrete capability exposed to the LLM. In `manifest.json` the keys are camelCase: `inputSchemaJson`, `outputSchemaJson`, `localExec`, `hostExec`, `sideEffect`, `requiredSecrets`, `defaultArgs`.

#### 1. Schema Definition
- **`name`**: Tool identifier invoked by the LLM (e.g. `jira_search_issues`). Must be alphanumeric with underscores.
- **`description`**: Clear, unambiguous description explaining when and why the LLM should call this tool.
- **`input_schema`**: Standard **JSON Schema (draft-07)** defining the tool arguments, types, descriptions, and required fields.
- **`output_schema`** *(Optional)*: JSON Schema defining expected return structure.

#### 2. Subprocess Execution (`exec`)
- **`command`**: The runtime interpreter (e.g. `"bun"`, `"node"`, `"python3"`, `"bash"`).
- **`entrypoint`**: Path relative to the skill directory that contains `manifest.json` (e.g. `"./execute.js"`). The runtime joins it onto the install directory, so the package does not name the mount root or its own directory.
- **`default_args`**: Static CLI arguments passed to the script before runtime args (e.g. `["search"]`).

#### 3. Security & Safety Attributes
- **`required_secrets`**: List of logical secret keys requested from the host vault (e.g. `["jira/base-url", "jira/auth-header"]`).
- **`side_effect`**: Categorization for security policy evaluation:
  - `READ_ONLY`: Observational queries with no mutation (e.g. `jira_search_issues`, `read_file`).
  - `MUTABLE_LOCAL`: Mutates local workspace files inside the microVM.
  - `MUTABLE_EXTERNAL`: Mutates third-party state outside the VM (e.g. `jira_transition_issue`, `jira_create_issue`). Triggers HITL review when enforced by Cedar policies.
- **`approvalRequired`** *(optional, default `false`)*: Source rule of `ask` for **this tool only**. JSON name `approvalRequired` (`approval_required` on the protobuf field). A skill can set it only on tools that skill declares; it has no syntax to name another skill's tool, and it cannot turn the rule off for anyone else (tighten-only). The session catalog records `skill_name` (the manifest `name`) and `approval_required` from each tool.
- **`timeout`**: Maximum execution duration before the runner kills the subprocess (e.g. `"30s"`, `"15s"`).

### Deprecated: `policy`

`SkillManifest.policy` is a Cedar fragment skills used to ship. It is **deprecated and ignored**. Publish still accepts it and stores it, and returns this warning:

```text
`policy` is ignored; use `approvalRequired` on tools
```

Nothing enforces that Cedar. Mark tools with `approvalRequired` instead. The structured `capabilities` list is unchanged and stays the consent surface.

---

## Filesystem Layout Inside the MicroVM

When an agent is booted with a skill dependency, VAM mounts the skill directory as a **read-only** volume inside the microVM:

```text
/mnt/drive9/skills/<install-directory>/
├── manifest.json     # Tool contract; entrypoints are relative to this file
├── SKILLS.md         # Prose injected by load_skill
├── package.json      # Dependencies (if Node/Bun)
└── execute.js        # Subprocess tool executor (uid=1000, gid=100)
```

The script runs with dropped privileges (`uid=1000`, `gid=100`) and cannot modify files in `/mnt/drive9/skills/`.

---

## Large documentation index (`INDEX.md`)

Skill packages may include thousands of files under `docs/`. Broad `glob` / `grep` results are capped at 200 hits, so large corpora need a compact name-to-path index.

Place **`INDEX.md` at the skill package root** (next to `SKILLS.md`):

```text
/mnt/drive9/skills/<skill-id>/
├── SKILLS.md
├── INDEX.md              # optional compact lookup table
├── manifest.json
└── docs/                 # extra reference files; not auto-injected
```

Packages without `INDEX.md` load unchanged. The published tar.gz is unpacked as-is, so the index is preserved with the rest of the tree.

### Entry format

`INDEX.md` is UTF-8 text:

1. Comment header lines starting with `#` (ignored by parsers).
2. One **tab-separated** row per document: `name<TAB>relative_path`.
3. `relative_path` is relative to the skill package root. Absolute paths and `..` segments are rejected.
4. **Deterministic generation**: unique names, then sort rows by name (UTF-8 byte / Unicode code point order).
5. **Duplicate names**: the first path keeps the original name; later rows become `name (2)`, `name (3)`, and so on.
6. **Escaping**: TAB, newline, carriage return, and backslash in a field are written as `\t`, `\n`, `\r`, `\\`.

```text
# VAM skill documentation index
po_headers_all	docs/tables/po_headers_all.md
invoice_lines	docs/tables/invoice_lines.md
```

### SKILLS.md guidance

Keep the body short. Tell the agent to grep the index by logical name, then read the absolute guest path — do not glob the docs tree:

```markdown
Large docs: grep INDEX.md by table or document name with path `/mnt/drive9/skills/<skill-id>`, then read_file `/mnt/drive9/skills/<skill-id>/<relative-path-from-index>`. Do not glob the docs tree.
```

A resolved row `po_headers_all	docs/tables/po_headers_all.md` is read as `/mnt/drive9/skills/<skill-id>/docs/tables/po_headers_all.md`. Do not copy the corpus into the workspace.

---

## Declared capabilities

Add the tokens a reviewer should see when this skill is attached:

```json
"capabilities": [
  "tool:jira.search",
  "tool:jira.write",
  "egress:atlassian.net",
  "secret:jira/auth-header"
]
```

Each token is `tool:`, `egress:`, `secret:`, or `fs:` plus a remainder (`fs:workspace:write` is valid). Whitespace is rejected. Removing a token suggests a major version bump. Adding one suggests a minor bump.

The token is a claim. The agent's Cedar policy still allows or denies the call.

---

## Registering a Skill with `vam` CLI

Publish a skill directory as the owner's **one draft** (recommended):

```bash
vam skill publish examples/jira-assistant/skills/jira-integration
```

This uploads a tar.gz of the directory, validates `SKILLS.md` + `manifest.json`, computes the digest on the host, and stores the blob under `{VAM_DRIVE9_ROOT}/catalog/skills/{owner}/` (owner-namespaced). Publishing again replaces that draft. It does not create a second version.

Promote the draft to a released version. On a terminal, omit `--version` to accept the suggestion:

```bash
vam skill suggest-version urn:vam:skill:local.vam.dev/vam/jira-integration
vam skill release urn:vam:skill:local.vam.dev/vam/jira-integration --note "Search, create, transition, comment"
```

List and inspect package rows. The draft is not a semver, so fetch it by channel:

```bash
vam skill ls
vam skill versions urn:vam:skill:local.vam.dev/vam/jira-integration
vam skill get urn:vam:skill:local.vam.dev/vam/jira-integration --channel draft
vam skill get urn:vam:skill:local.vam.dev/vam/jira-integration --channel released --version 1.0.0
```

The legacy metadata catalog (`PUT /v1/skills`) remains available for JSON-only registration:

```bash
vam skill put examples/jira-assistant/skills/jira-integration/manifest.json
```

Retrieve metadata by URN:

```bash
vam skill get urn:vam:skill:local.vam.dev/vam/jira-integration
```

See [Publish a Skill Package](/authoring/publish-skill/) for the Lima-ready publish → run recipe.

---

## Next Steps

- Learn how to implement tool executors in [Writing In-VM Tools (`execute.js`)](/authoring/agent-spec/).
- Learn how to configure execution runs in [Run Specifications (`run.json`)](/authoring/run-spec/).
- Understand how to enforce approvals in [Cedar Policies & HITL](/authoring/playbook-spec/).
