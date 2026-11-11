---
title: Publish a Skill Package (Local)
description: Lima-ready recipe to publish a skill directory to the orchestrator catalog, inspect draft/release rows, and run an example agent.
---

This page is a copy-paste workflow for **local skill packages**: publish a skill folder to `vamd`, inspect catalog rows, promote the draft to a **released** version, then start a run. The catalog rules (one draft, suggested version, floating references) are in [Authoring model](/authoring/model/). For manifest fields, see [Skills and frontmatter](/authoring/skill-manifest/).

---

## 1. Skill folder layout

Each publishable skill directory needs three pieces:

| File | Role |
|---|---|
| **`SKILLS.md`** | Frontmatter with `id` (URN), `version`, `name`, `description`. The body is the prose `load_skill` injects. A frontmatter `tools` list that disagrees with the manifest is rejected |
| **`manifest.json`** | Same URN and semver as `SKILLS.md`; tools, side effects, secrets, and `capabilities` |
| **`execute.js`** (or `.py`) | Unprivileged tool executor. `localExec.entrypoint` is relative to this directory |

The URN in both files must match what the agent references in `agent.json` `skillRefs`, for example `urn:vam:skill:local.vam.dev/vam/assistant-tools`.

---

## 2. Mac + Lima topology

On Apple Silicon, **`vam` runs on the Mac** and talks to **`vamd` inside the Lima VM** (`VAM_HOST`, default `http://127.0.0.1:8080`).

- The repo is bind-mounted **read-only** into Lima — that is where `vam skill publish <path>` reads the **source folder**.
- Published blobs land in the VM's catalog store at `{VAM_DRIVE9_ROOT}/catalog/skills/{owner}/` (owner-namespaced; not on the Mac repo mount).

See [macOS + Lima](/getting-started/macos-lima/) for VM setup.

---

## 3. Publish, inspect, release

From the repository root (Mac CLI → Lima `vamd`):

```bash
# Upload the directory as the owner's one draft (tar.gz). Publishing again replaces it.
vam skill publish examples/bun-assistant/skills/assistant-tools

# List package rows in the catalog
vam skill ls

# Fetch the draft. The draft has no semver, so do not pass --version.
vam skill get urn:vam:skill:local.vam.dev/vam/assistant-tools --channel draft

# Suggested next version, then release. Caller must own the package and hold publish:grant.
# On a terminal, omit --version to accept the suggestion.
vam skill suggest-version urn:vam:skill:local.vam.dev/vam/assistant-tools
vam skill release urn:vam:skill:local.vam.dev/vam/assistant-tools --note "First assistant tools"

# Released rows are addressed by version
vam skill versions urn:vam:skill:local.vam.dev/vam/assistant-tools
vam skill get urn:vam:skill:local.vam.dev/vam/assistant-tools --channel released --version 1.0.0
```

**Draft vs release:** there is one draft, private to the owner. Another user's draft never appears in `vam skill ls` or `vam skill get`. **Release** snapshots that draft as an immutable `X.Y.Z` and makes it visible to the org. Edit the files and publish again to change the draft; release a higher version to ship it. Releasing unchanged content at a new version is rejected. **In-flight sessions do not hot-reload** — start a new run to pick up a published package.

Legacy JSON-only metadata registration (no tarball) remains available:

```bash
vam skill put examples/bun-assistant/skills/assistant-tools/manifest.json
```

`vam skill put` accepts **`manifest.json` only**, not `SKILLS.md`.

---

## 4. Run an example agent

After publishing (optional — see seed fallback below):

```bash
vam run --file examples/bun-assistant --prompt "Calculate 23*45 and write the result to result.txt" --follow
```

`vam run --file` does **not** auto-publish skills.

---

## 5. Seed fallback (unpublished examples)

Example workspaces under `examples/<name>/skills/` are still copied into the session tool catalog on first materialization even when you skip publish. That lets `vam run --file` work out of the box for bun, jira, sqlite, and chaos examples.

Use **`vam skill publish`** when you want the orchestrator catalog (draft/release rows, digest pinning, and overlay resolution) instead of the workspace seed alone.

---

## Next steps

- [CLI Reference — Catalog](/getting-started/cli/#5-catalog-management-vam-agent-vam-playbook-vam-skill)
- [Skills & Frontmatter](/authoring/skill-manifest/)
- [Bun Assistant Tutorial](/tutorials/bun-assistant/)
