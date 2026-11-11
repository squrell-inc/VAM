# Bun Assistant Example

This is an end-to-end example of a Virtual Agentic Machine (VAM) agent running inside a Firecracker microVM.

The agent uses a ReAct execution loop to converse with an LLM via the host-brokered secure Inference plane.

**Runtime primitives** (always available, implemented in Rust inside `vam-runtime`):

- `read_file`, `write_file`, `edit_file`, `delete_file`, `glob`, `grep`, `run`, `todo_write`

**Skill tool** (Bun script under `/mnt/drive9/skills/assistant-tools/`):

- `calculator` — safe mathematical calculation

Use `glob` to list workspace files, `write_file` for notes, `todo_write` for todos, and `calculator` for math.

## Quick start

On Amazon Linux 2023, install a musl cross linker before `./scripts/build-rootfs.sh` (there is no `musl-gcc` RPM; see repo root README — musl.cc `x86_64-linux-musl-cross`).

Rebuild the guest rootfs after pulling runtime changes (the microVM runs the binary baked into `target/rootfs.ext4`, not just host `vamd`):

```bash
cargo build -p vam-orchestrator -p vam-runtime -p vam-cli
./scripts/build-rootfs.sh
./target/debug/vamd   # in one terminal
```

Start a run (CLI auto-loads sibling `agent.json` and refreshes the catalog):

```bash
# Optional: publish the skill package to vamd (draft channel)
vam skill publish examples/bun-assistant/skills/assistant-tools

vam run --file examples/bun-assistant/run.json --prompt "Calculate 23*45, write result.txt with write_file, glob *.txt" --follow
```

If you skip publish, skills still load from this example directory via workspace seed fallback on first materialization (`examples/bun-assistant/skills/`). `vam run --file` does not auto-publish.

Workspaces named `assistant`, `assistant2`, `assistant3`, etc. seed skills from this example directory on first materialization.

## Troubleshooting stale tools or weak instructions

If a run tries to call removed tools such as `write_note` or `list_workspace`, or the model says it cannot access the filesystem:

1. **Rebuild the guest rootfs** — `./scripts/build-rootfs.sh` (required after guest code changes; `cargo build -p vam-runtime` alone does not update the microVM image).
2. **Rebuild `vam-runtime`** — `cargo build -p vam-runtime` (guest binary must match host orchestrator).
3. **Refresh the agent catalog** — `vam agent put examples/bun-assistant/agent.json`, or use `vam run --file examples/bun-assistant/run.json` (inline agent is loaded automatically).
4. **Reset the workspace snapshot** — switch to a fresh volume (e.g. `volume://assistant-v2` in `run.json`) or clear the Drive9 HEAD for the workspace so the guest mounts the current skill manifest.
5. **Check vamd traces** — look for `📋 [System Prompt]` and `🔧 [Tools Available]` lines; you should see 8+ tools including `write_file` and `glob`.

Replay and Drive9 HEAD reuse snapshots across follow-up runs on the same thread; stale manifests persist until the workspace is reset.
