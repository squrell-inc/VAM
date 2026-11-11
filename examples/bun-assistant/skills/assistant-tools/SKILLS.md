---
id: "urn:vam:skill:local.vam.dev/vam/assistant-tools"
name: Assistant Tools
version: "1.0.0"
description: Offline calculator skill; filesystem and todo tools are runtime builtins.
---

# Assistant Tools Skill

This skill exposes the `calculator` Bun tool for safe mathematical evaluation.

Use `calculator` for math expressions like `23 * 45 + 122`.

Filesystem operations (`read_file`, `write_file`, `glob`, etc.) and todos are provided by **runtime primitives** — not this skill.

Extra reference files under this skill directory (if any) are on the read-only mount — use `read_file` / `grep`; they are not auto-injected.
