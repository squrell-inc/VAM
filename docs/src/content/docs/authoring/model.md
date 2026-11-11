---
title: Authoring model
description: The concepts a use-case author needs to write agents, skills, playbooks, and policies that match the catalog VAM actually runs.
---

This page is the map for people who write use cases: agents, skills, playbooks, and the policies those agents run under. Field-by-field pages are linked at the end. What follows is the model the catalog enforces today.

Two doors write the same artifacts. A developer uses `agent.json`, `manifest.json`, `playbook.json`, and `policy.cedar` with the `vam` CLI. The app calls the same `vamd` API. A released version is identical either way.

`vam.json` and `vam.lock` are a later packaging phase. Do not invent them for a use case. Publish the spec files below.

---

## The five artifacts

| Artifact | File you write | URN kind | What it is |
|---|---|---|---|
| Agent | `agent.json` | `agent` | Who the model is, which skills and playbooks it may use, which Cedar policies bind it, and its model and budget settings |
| Skill | `manifest.json` plus `SKILLS.md` | `skill` | Tools the model can call, plus the prose `load_skill` injects |
| Playbook | `playbook.json` | `playbook` | A host-tracked procedure: steps, gates, guards, and the skills it needs |
| Policy | `policy.cedar` | `policy` | Cedar that allows or denies tools, files, and egress. The agent points at it |
| Run | `run.json` or `vam run` flags | `run` | One turn: which agent (and optional playbook) to run, on which workspace, with which inputs |

A skill is a bundle of tools. An agent does not become able to call those tools until it lists the skill in `skillRefs` and a released run freezes that reference. A playbook does not run unless the run names it and the agent allows it.

---

## URNs

```text
urn:vam:<kind>:<domain>/<namespace>/<name>
```

Example: `urn:vam:agent:local.vam.dev/vam/jira-assistant`.

You choose the kind, domain, namespace, and name. You do not choose a digest, and you do not put `-draft` or `-rc` in the URN.

The domain says where an artifact comes from. A node knows three:

| Origin | Domain | Who sees a release | How it is configured |
|---|---|---|---|
| **Org** | The node's registry domain | Everyone in the org | `VAMD_REGISTRY_DOMAIN` on `vamd`. Read it from `registryDomain` on `GET /v1/me`. |
| **Squrell** | The marketplace domain | Everyone in the org, once an admin installs it | `hub.squrell.cloud` by default; `VAMD_MARKETPLACE_DOMAIN` overrides it (staging). `GET /v1/me` always returns it as `marketplaceDomain`. Only the catalog import writes it, and only from the preconfigured Squrell marketplace. |
| **Local** | `local.vam.dev`, and any host ending in `.local` | This node only | Fixed. Nothing to configure. |

Any other domain is **Other**: it is accepted for agents, playbooks and policies, and has no special meaning. Domains are compared exactly, so a subdomain of the registry domain is Other.

A node that has not set `VAMD_REGISTRY_DOMAIN` uses `local.vam.dev`, so everything on it is Local. The domain is part of an artifact's identity: changing it later does not rename existing artifacts.

The namespace defaults to your username, lowercased. If that is not a valid URN segment, it is `default`. `GET /v1/me` returns it as `defaultNamespace`. It is a default, not a rule: you may still choose another namespace.

Write use cases on a **local** domain, or on your org's registry domain. `local.vam.dev` is the one the examples use. Restrictions that are enforced:

- `examples.local` is reserved for platform seeds. Publishing an agent, skill, playbook, or policy there is refused.
- The marketplace domain and the `publisherDomain` of every marketplace an org admin has added are reserved. Creating, publishing, releasing or deleting a catalog artifact under them is refused, so nobody can forge a Squrell origin or an installed row. A row a marketplace installed (tier `marketplace`, `readOnly: true`, with an `origin`) is read-only for everyone, org admins included; only an admin installing or updating a listing writes it. A marketplace with an installed listing cannot be removed (disable it to hide its listings).
- A skill package you publish, and any agent `skillRefs` entry, must use a local registry or the node's registry domain. A domain such as `hub.vam.dev` is rejected with `not a local catalog registry` or `remote pull is not supported`. The one addition is resolving what a marketplace installed: a ref under the marketplace domain or an added marketplace's `publisherDomain` resolves, but you still cannot publish there.

A release is visible to the org because it is released. The draft stays private to its owner. The app shows an origin badge from the domain.

---

## Draft and release

Each artifact has **one draft**. The draft is private to its owner. Other users do not see it in the catalog and cannot run it. Saving replaces that draft. The draft has no version number.

**Release** snapshots the current draft as an immutable semver (`X.Y.Z`). Releasing does not delete the draft. The owner keeps editing, and the next release snapshots the draft again.

| You do | Result |
|---|---|
| `put` / `skill publish` | Saves your draft |
| `release` with no `--version` on a terminal | Prints a suggested version and asks |
| `release --version X.Y.Z` | Required when stdin is not a terminal. The error names the suggestion |
| `suggest-version` | Prints the suggestion and does not release |
| `versions` | Your draft first, then releases, newest first |
| `--note` | Optional changelog, at most 500 characters |

The suggestion compares the draft with the latest release. The highest matching rule wins.

| Kind | Major | Minor | Patch |
|---|---|---|---|
| Agent | Removed a skill or playbook; turned `allowAnyPlaybook` off; changed the capability-policy URN; changed `extends` | Added a skill or playbook; turned `allowAnyPlaybook` on | Instructions, role, display name, description, model or context settings; an updated pin on a skill or playbook that was already attached |
| Playbook | Removed a step; removed a gate option; added a required input | Added a step; added an optional input | Display name, description, step instructions |
| Skill | Removed a tool or a declared capability | Added a tool or a declared capability | Description or skill instructions |
| Policy | — | Any Cedar change (the classifier cannot read Cedar, so every edit is a minor bump) | — |

When nothing is released yet, the suggestion is `1.0.0`. A skill package uses the version already written in `manifest.json` when that version is greater than `0.0.0`. You may type a higher version than the suggestion. These releases are rejected:

- A version lower than the latest release.
- A version that already exists with different content. The error names the next free version.
- A higher version when the draft matches the latest release. Edit the draft first.
- A dependency that exists only as a draft. Release that dependency first. The error names it.

Releasing the same version with the same content is a no-op.

---

## References you write

Every link between artifacts is a URN: `skillRefs`, `defaultPlaybookRef`, `allowedPlaybookRefs`, policy refs, playbook `requires.skills`, and `extends`.

| What you write | What happens |
|---|---|
| URN only | The reference floats. This is what you should write |
| URN plus `version` | That exact release. Release still records a digest |
| URN plus `version` plus `digest` | Accepted, but do not write the digest. The server records it |

A digest without a version is rejected.

Resolution:

- **Draft run.** A floating reference uses the agent owner's draft of that dependency when one exists. Otherwise it uses the latest release. Another user's draft is never used.
- **Release.** Every reference is frozen to a version and a digest inside the released spec. A draft-only dependency cannot be frozen, so release fails.
- **A run of a released version** uses those frozen pins.

Frozen `version` and `digest` show up on the released spec, in `vam <kind> versions`, and on the run record. Do not copy them back into a draft. Edit the URN-only form and release again.

---

## How an agent is composed

```text
agent.json
  skillRefs[]            skills whose tools this agent may call
  defaultPlaybookRef     optional procedure used when a run omits a playbook
  allowedPlaybookRefs[]  playbooks a run may select
  allowAnyPlaybook       when true, a run may name any playbook
  capabilityPolicy       Cedar for tools, files, and egress
  hitlPolicy             tools that wait for a person
  extends + overlay      optional delta on another agent
```

Playbook admission is fail-closed. If `allowedPlaybookRefs` is empty and `allowAnyPlaybook` is false, a run cannot attach a playbook. `allowAnyPlaybook: true` is an explicit escape hatch. The composition check does not list every playbook in that case; it says the agent allows any playbook.

`extends` inherits another agent. It does not call that agent. The child leaves `systemInstructions` and `skillRefs` empty and puts the delta in `overlay`: append or replace instructions, add or remove skills, grant or revoke capability tokens. Chains are one parent at a time, up to 8 hops. Release freezes `extends` the same way as any other reference. See [Agent Specifications](/authoring/agent-spec/#5-extending-an-upstream-agent-extends).

### What the guest is told

The guest prompt lists only tools the host advertises for that run. A tool from a skill the agent did not attach is not in the prompt, and the guest cannot call it. If the host cannot list advertised tools, the allowlist is empty.

On a playbook run the host also injects the procedure. Each task step says how to finish it (`run_step` for a bound step, the named tool plus `step_complete` for assisted, the agent's tools plus `step_complete` for agentic). A gate tells the model to wait. The model does not invent a step outcome. See [Playbooks](/authoring/playbook-spec/#what-the-agent-sees).

`load_skill` appends that skill's `SKILLS.md` body to the prompt. It does not grant tools. Tools come from the attached skill package.

### Host-owned run inputs

These keys are sent by the host. They are stripped before a playbook `inputSchema` is checked, including when the schema sets `additionalProperties: false`:

`prompt`, `model`, `model_provider`, `max_iterations`, `system_instructions`, `agent_role`, `agent_display_name`, `playbook_procedure`.

Declare playbook inputs for the procedure (`project_key`, `pr_url`, and so on). Do not declare `prompt` or `max_iterations` as playbook fields. The chat message and the iteration budget stay with the run.

The declared `preferredModels[].provider` is the provider the host sends. An entry `{"provider": "vercel", "modelName": "anthropic/claude-haiku-5.5"}` routes through the Vercel AI Gateway.

---

## Skills

A publishable skill directory has:

| File | Role |
|---|---|
| `manifest.json` | Identity, tools, side effects, secrets, declared capabilities |
| `SKILLS.md` | Prose injected by `load_skill`. Frontmatter `id` and `version` must match the manifest |
| `execute.js` or another entrypoint | Unprivileged program. `localExec.entrypoint` is relative to the directory that contains `manifest.json` |

Publish the directory. `vam skill publish <dir>` stores one draft package. `vam skill put manifest.json` registers JSON metadata only and does not upload the scripts.

### Tools

Each tool has a `name`, a `description`, an `inputSchemaJson` (JSON Schema draft-07, as a string), a side effect, and one execution provider:

| Provider | Where it runs |
|---|---|
| `localExec` | Inside the guest. `command` is `bun`, `node`, `python3`, or `bash`. The process is uid 1000 and cannot write the skill mount |
| `hostExec` | On the host, after policy. Credentials stay on the host. `adapterId` names the adapter |
| `builtin` | A runtime primitive (`read_file`, `glob`, `grep`, `run`, `todo_write`, `load_skill`). Do not reuse these names for a skill tool |
| `mcpServer` | An MCP server launched inside the guest |

`sideEffect` is one of:

- `SIDE_EFFECT_CLASS_READ_ONLY` — no mutation
- `SIDE_EFFECT_CLASS_MUTABLE_LOCAL` — writes inside the workspace
- `SIDE_EFFECT_CLASS_MUTABLE_EXTERNAL` — changes something outside the VM

Playbook guards and human-approval policy read this class. Set it on every tool.

`requiredSecrets` names vault keys (`jira/base-url`). Values never appear in the manifest. `scope` is `SCOPE_ORG`, `SCOPE_USER`, or `SCOPE_EITHER` (the default). A required secret that is missing fails admission.

### Capability tokens

`capabilities` on a skill, and `requires.capabilities` on a playbook, are plain tokens:

```text
tool:<name>
egress:<host>
secret:<name>
fs:<path-or-mode>
```

Examples: `tool:jira.search`, `egress:example.com`, `secret:jira/auth-header`, `fs:workspace:write`. No whitespace. The prefix and the remainder are both required.

These tokens are what a person reviews when a skill is attached, and what `vam agent check` prints. They are also what an `extends` overlay grants and revokes. They are **not** the allowlist. Cedar on the agent's `capabilityPolicy` still decides the call. A token is not checked against that Cedar yet. Declare the tokens so reviewers can see what the skill claims. Put the allow and deny rules in `policy.cedar`.

A skill may also carry a `policy` string. That Cedar is stored and not enforced. Do not rely on it. Ship allow and deny rules as a policy artifact the agent references.

### Procedures shipped inside a skill

`playbooks` on the manifest is a list of paths relative to the manifest directory, such as `./playbooks/sprint-cleanup.json`. Those files publish with the package. They are not a scheduler, and they do not attach themselves to an agent. The agent still lists the playbook URN in `allowedPlaybookRefs` or `defaultPlaybookRef`.

---

## Playbooks

A playbook is host-tracked. The host records step state, opens gates, denies guarded tool calls until `allowedWhen` holds, and checks evidence before a non-failed outcome. Prose the host cannot check belongs in `SKILLS.md`, not in a playbook.

`requires` says what the procedure needs. It does not schedule steps.

```json
"requires": {
  "skills": [
    { "resourceId": { "urn": "urn:vam:skill:local.vam.dev/vam/jira-integration" } }
  ],
  "capabilities": ["tool:jira.search"]
}
```

Write skill URNs with no version. They resolve with the same draft and release rules as every other reference.

### Composition check

`vam agent check <agent-urn>` asks whether the agent's attached skills cover each playbook it names. It is informational: a missing skill still exits 0, and it does not change release or run admission.

```bash
vam agent check urn:vam:agent:local.vam.dev/vam/jira-assistant --draft
vam agent check urn:vam:agent:local.vam.dev/vam/jira-assistant --version 1.0.0
```

With no flag it checks the latest release, or your draft when nothing is released. The table lists each playbook, each required skill, whether that skill is on the agent, and the capability tokens. Attach a missing skill by adding its URN to `skillRefs`, then `put` the agent again.

`allowAnyPlaybook: true` prints `Allows any playbook (not listed).` The check still reports skills the agent attaches.

---

## Policies

Permissions are Cedar, referenced from the agent. There is no toggle editor in the spec yet.

| Agent field | What you attach |
|---|---|
| `capabilityPolicy.policyRef` | Cedar for tools, filesystem paths, and egress |
| `capabilityPolicy.securityProfileName` | A label for that profile |
| `hitlPolicy.policyRef` | Cedar for human approval |
| `hitlPolicy.monitoredTools` | Tool names that pause for a decision, such as `jira_transition_issue` |
| `telemetryPolicy` | PII redaction and audit mode |

```bash
vam policy put examples/bun-assistant/policy.cedar \
  --urn urn:vam:policy:local.vam.dev/vam/assistant-egress-only
vam policy release urn:vam:policy:local.vam.dev/vam/assistant-egress-only
```

On a playbook run, Cedar is evaluated first. A matching guard can deny after that. A guard never allows a call Cedar denied. Guards are deny-only and live on the playbook, not in Cedar. When the run has step rows, Cedar can read `context.playbook` (`id`, `state`, `outcome`) on `ExecuteTool`.

A policy is its own artifact: one draft, versions on release, the same suggestion rules. Any Cedar edit suggests a minor bump.

---

## A use case, end to end

1. Write the skill directory. Declare tools, side effects, secrets, and capability tokens. Put operational rules in `SKILLS.md`.
2. `vam skill publish <skill-dir>`, then `vam skill release <skill-urn>`.
3. Write `playbook.json` with steps, gates, guards, and `requires` pointing at that skill URN.
4. `vam playbook put <file>`, then `vam playbook release <playbook-urn>`.
5. Write Cedar and `vam policy put` / `vam policy release`.
6. Write `agent.json`: instructions, `skillRefs`, `allowedPlaybookRefs` or `defaultPlaybookRef`, and the policy refs. All URNs, no versions.
7. `vam agent put <file>`.
8. `vam agent check <agent-urn> --draft` and fix any skill the playbook requires that the agent does not attach.
9. Run the draft. A floating reference uses your draft of each dependency.
10. `vam agent release <agent-urn>`. The release freezes every reference. Members run that version, not your draft.

```bash
vam skill publish examples/jira-assistant/skills/jira-integration
vam skill release urn:vam:skill:local.vam.dev/vam/jira-integration

vam playbook put examples/jira-assistant/playbook.json
vam playbook release urn:vam:playbook:local.vam.dev/vam/jira-sprint-backlog-cleanup

vam agent put examples/jira-assistant/agent.json
vam agent check urn:vam:agent:local.vam.dev/vam/jira-assistant --draft
vam agent release urn:vam:agent:local.vam.dev/vam/jira-assistant
```

Omit `--version` on a terminal to accept the suggestion. In CI, pass `--version` from `vam <kind> suggest-version`.

In-flight sessions do not reload a package. Start a new run to pick up a publish or a release.

---

## Where each detail lives

- [Agent Specifications (`agent.json`)](/authoring/agent-spec/) — fields, models, `extends`
- [Skills and frontmatter](/authoring/skill-manifest/) — tool schema, executors, `INDEX.md`
- [Publish a skill package](/authoring/publish-skill/) — Mac and Lima publish path
- [Playbooks (`playbook.json`)](/authoring/playbook-spec/) — steps, gates, guards, evidence
- [Playbook tutorial](/authoring/playbook-tutorial/) — init, explain, test, decide a gate
- [Run specifications (`run.json`)](/authoring/run-spec/) — workspace, thread, budgets
- [CLI reference](/getting-started/cli/) — command list
