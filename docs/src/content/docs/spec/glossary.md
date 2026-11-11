---
title: Glossary
description: Shared vocabulary, platform concepts, and VAM-specific terminology.
---

:::caution[Alpha Stability Warning]
VAM specifications and contracts (under the `vam.v1` package) are currently in **Alpha**. The schema is actively evolving, breaking changes are expected, and no forward/backward compatibility guarantees are provided at this stage.
:::

To ensure clear, unambiguous communication across design documents, schemas, and codebases, this page defines the canonical terminology used throughout the VAM (Virtual Agentic Machine) ecosystem.

---

## 1. Domain Entities

### Agent
The core purpose-built, stateful AI worker running inside the guest microVM. An Agent is defined by its professional role, core instructions, model preferences, memory limits, and assigned Skill Set. It manages short-term context memory, plans step execution, and dispatches tools.

### Skill
A specialized, self-contained, and versioned package of domain knowledge, system instruction extensions, local or MCP tools, constraints, and few-shot examples.

### Skill Set
The complete curated collection of Skills mounted into the Agent's workspace (typically under `/mnt/drive9/skills/`), which the Agent selectively resolves and invokes.

### Playbook
A declarative, versioned blueprint representing a Directed Acyclic Graph (DAG) of steps, conditions, retry rules, and checkpoints that an Agent executes to solve a domain problem.

### Workflow (or Run)
A live execution instance of a Playbook inside a VAM session. It coordinates the step execution sequence, schedules parallel steps, and manages state variables and outcomes.

### VAM (Virtual Agentic Machine)
The primary physical and logical isolation unit; a hardware-virtualized Firecracker microVM with persistent Drive9 storage. Default Isolated/Brokered sessions use **vsock-only** networking (no TAP). Restricted Direct adds a dedicated TAP when pool capacity allows. Exactly one Agent runs per VAM session.

### Tool
A declared function or command that an agent can execute to interact with its environment (e.g., file edits, database queries). A tool is defined by a unique name, description, and an input schema represented in JSON Schema format.

### Runtime Primitive (Builtin Tool)
An always-on tool implemented in Rust inside `vam-runtime` (not a skill script). Milestone 1 ships eight primitives: `read_file`, `write_file`, `edit_file`, `delete_file`, `glob`, `grep`, `run`, and `todo_write`. Reserved names cannot appear in skill manifests.

### Trajectory
The complete historical trace of a **Run**, including the chronological sequence of system inputs, agent prompts, internal reasoning, tool invocations, results, and metric streams.

---

## 2. Infrastructure & Runtime Components

### `vamd` (VAM Daemon / `vam-core`)
The high-level orchestrator process running on the host system. `vamd` is responsible for launching Firecracker microVMs, establishing `AF_VSOCK` communication, monitoring metrics, applying Cedar access control policies, handling Human-in-the-Loop interventions, and persisting Drive9 storage state.

### `vam-runtime`
The core engine process running inside the guest microVM. The `vam-runtime` connects to the host via VSOCK, registers itself, receives tool execution orders, manages local sandboxed execution (e.g. Bun/Python environments), captures stderr/stdout, and streams logs back to the host.

### Drive9
A high-performance, purpose-built storage virtualization layer optimized for agent workloads. Drive9 allows host and guest to share/sync layered directories, keeping agent progress isolated and checkpointable.

### `AF_VSOCK` (Virtual Sockets)
The Linux kernel-level socket family designed for lightning-fast host-to-guest communications. It bypasses host network interfaces entirely, operating instead via memory-copy over hypervisor-managed rings, ensuring absolute VM boundary safety.

### CID (Context Identifier)
A unique 32-bit integer identifier used in `AF_VSOCK` addressing. In the VAM CID model, the host is statically fixed to CID `2`, while guests are dynamically assigned unique CIDs by the host orchestrator.
