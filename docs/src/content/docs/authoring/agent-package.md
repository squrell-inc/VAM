---
title: Agent Package Layout
description: Canonical file tree for an agent package, who may write each instruction layer, and why a Vamfile stays outside the package.
---

Use-case authors start at [Authoring model](/authoring/model/). The catalog you publish today is `agent.json`, skill directories, `playbook.json`, and `policy.cedar`. This page is the on-disk layout a later packaging phase will scaffold.

An **agent package** is the directory a developer commits for one agent. `vam init` will scaffold this tree and `vam validate` will check it. Those commands are later phases. This page is the layout they follow. `vam.json` and `vam.lock` are not required to publish or run an agent.

The checked-in examples under `examples/` predate this layout. Use the tree below.

---

## File tree

```text
my-agent/
  vam.json            identity, deps by range, capability grants   [commit]
  vam.lock            resolved versions + digests + capabilities    [commit, generated]
  agent.json          role, instructions, skill/playbook refs       [commit]
  policy.cedar        this agent's own rules (optional)             [commit]
  playbooks/*.json    procedures owned by this agent (optional)     [commit]
  skills/<id>/        only skills authored in this repo             [commit]
  .vam/               resolved cache, materialized payloads         [gitignore]
```

| Path | What it holds | Git |
|---|---|---|
| `vam.json` | Package identity, dependency ranges, and capability grants | Commit |
| `vam.lock` | Resolved versions, digests, and capabilities. Generated. | Commit |
| `agent.json` | Role, instructions, and skill / playbook references. An `AgentSpec`. | Commit |
| `policy.cedar` | Rules that belong to this agent. Optional. | Commit |
| `playbooks/*.json` | Procedures this agent owns. Optional. | Commit |
| `skills/<id>/` | A skill whose source this repository authors. | Commit |
| `.vam/` | Resolved cache and materialized payloads. | Gitignore |

`agent.json` is the published spec. See [Agent Specifications](/authoring/agent-spec/). `vam.json` and `vam.lock` are the project manifest and lockfile; their field schema is a later phase. This page locks where those files sit.

---

## A consumed skill is a reference

A skill this package **uses** is a reference (a dependency range or a skill URN). It is never a directory in this tree.

`skills/` holds only skills this repository authors. Installing someone else's skill changes the reference and the lockfile. Their files land in `.vam/`, which is gitignored, and are materialized when a session starts. Third-party code stays out of the developer's Git history.

---

## Instruction layers

Three layers. One owner each. No package may cross them.

| Layer | File | Owner | Writable by a package? |
|---|---|---|---|
| Platform rules | `workspace/.vam/SYSTEM.md` | VAM | **no** |
| Agent purpose | `agent.json` `systemInstructions` | agent author | yes |
| Skill usage | `SKILLS.md` `system_extension` | skill author | yes, on `load_skill` |

A playbook run adds a run-scoped layer the packages do not own. The host renders the procedure and the guest inserts it at `{{playbook.procedure}}` in `SYSTEM.md`.

**No package may write `SYSTEM.md`.** Two mechanisms already enforce that:

1. **Write protection.** Guest workspace tools refuse the path. `is_protected_write_path` in `crates/vam-runtime/src/primitives.rs` treats `.vam/SYSTEM.md` as protected, together with `.vam/workspace.json` and `.vam/threads/`. See [Runtime Primitives](/runtime/tools/).
2. **Automatic overwrite.** On workspace materialize, `seed_system_instruction_template` in `crates/vam-storage/src/instruction_template.rs` rewrites `workspace/.vam/SYSTEM.md` when the file is missing or when its frontmatter `template_version` is lower than `SHIPPED_TEMPLATE_VERSION`. That constant is **11**. The guest embeds the same template (`DEFAULT_TEMPLATE_VERSION` in `crates/vam-runtime/src/instruction.rs`, also 11). A direct edit of the workspace file disappears on the next platform upgrade that bumps the shipped version.

Platform habits belong in `crates/vam-runtime/templates/SYSTEM.md`. Agent-specific purpose belongs in `systemInstructions`. Skill-specific usage belongs in the skill's `system_extension`, injected when the agent calls `load_skill`. See [System Instruction Template](/runtime/agent/).

If a package needs to add to the platform layer, the slot is an explicit `systemInstructions.append`. Opening `SYSTEM.md` is not that slot. The append field is not part of this layout decision.

---

## `Vamfile` is not part of the package

A `Vamfile` answers what is in the machine image. The agent package answers what the agent does. Those are different axes. The standard layout has no `Vamfile`.

The `Vamfile` is the Phase 9 Tier 2 escape hatch: custom kernels, unusual system libraries, compliance-mandated base images. See [Vamfile Language](/build/vamfile/). Choosing a custom image remains the agent author's call. A marketplace skill still must not require that rebuild. Putting a `Vamfile` in every agent package would make every agent author a build engineer and would spend the cold-start advantage of the shared golden image.

---

## Related pages

- [Agent Specifications](/authoring/agent-spec/) — `agent.json` / `AgentSpec`
- [Skills & Frontmatter](/authoring/skill-manifest/) — skill manifests and `SKILLS.md`
- [Playbooks](/authoring/playbook-spec/) — playbook files
- [System Instruction Template](/runtime/agent/) — how `SYSTEM.md` is seeded and rendered
- [Vamfile Language](/build/vamfile/) — image recipes, outside this package
