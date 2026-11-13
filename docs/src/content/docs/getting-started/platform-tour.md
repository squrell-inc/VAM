---
title: Platform Tour
description: End-to-end session flow, platform terminology, and plane division.
---

:::caution[Alpha Stability Warning]
VAM is currently in **Alpha**. The platform subsystems, data flows, and interfaces described on this page represent planned architectural specifications.
:::

To provide a clear, unified mental model of the Virtual Agentic Machine before diving into complex crate modules, this page conducts an end-to-end tour of VAM's operations, terminology, and core planes.

---

## 1. High-Level Concept: Control vs. Execution Planes

VAM maintains a strict division of labor between two distinct operating planes to guarantee both node security and runtime performance:

### The Host Control Plane (Trusted)
* **What runs here**: The system daemon (`vamd` via `vam-orchestrator`), virtualization helpers (`vam-core`), security policies (`vam-security`), storage synchronization (`vam-storage`), host-side networking and firewalls (`vam-network`), and telemetry exporters (`vam-telemetry`).
* **Authority**: The host owns all absolute decision-making power. It holds encryption keys, registers network leases, evaluates authorization rules, and executes the physical microVM life cycle.

### The Guest Execution Plane (Untrusted)
* **What runs here**: The in-VM guest agent engine (`vam-runtime`) executing inside an isolated Linux microVM.
* **Authority**: The guest has zero authority. It cannot make direct internet requests, read host configuration, or access raw storage credentials. It serves as an unprivileged container for running agent instructions, playbooks, and scripting tools.

---

## 2. End-to-End Session Journey

To understand how VAM operates, let's trace the lifecycle of a single agent request:

```text
 1. Request Admission     2. Prepare Host & VM       3. Handshake & Init
 ┌──────────────────────┐  ┌───────────────────────┐  ┌────────────────────────┐
 │ - Client sends gRPC  │  │ - Setup TAP network   │  │ - VMM boots guest      │
 │ - Validate client JWT│  │ - Prepare Drive9 FS   │  │ - Guest PID 1 launches │
 │ - Reserve budget/vCPU│─►│ - Hardlink kernel/fs  │─►│ - Handshake over vsock │
 └──────────────────────┘  │ - Start Jailer sandbox│  │   port 52              │
                           └───────────────────────┘  └───────────┬────────────┘
                                                                  │
 6. Teardown & GC         5. Execute Playbook         4. Load Workspace
 ┌──────────────────────┐  ┌───────────────────────┐  │
 │ - Terminate VM       │  │ - Stream stdout/logs  │  │ - Mount /mnt/drive9    │
 │ - Sync final storage │  │ - Intercept raw tools │  │ - Load active SKILLS   │
 │ - Sign final audits  │◄─│ - Broker safe APIs    │◄─┼ - Ready for agent tasks│
 │ - Free TAP/IP/Memory │  │ - Evaluate Cedar     │  │                        │
 └──────────────────────┘  └───────────────────────┘  └────────────────────────┘
```

---

## 3. Canonical Platform Vocabulary

VAM uses consistent, precise terms across all documentation and crates. Every contributor must respect this vocabulary:

* **Node**: A physical bare-metal or nested-virtualization machine running the `vamd` system daemon.
* **Session**: The primary runtime isolation unit. A session represents one isolated environment, consisting of exactly one Firecracker microVM, its network namespace, and its dedicated Drive9 workspace.
* **MicroVM**: The lightweight virtual machine instance spawned by Firecracker, running a custom-built Linux kernel.
* **Host / Guest**: The physical node running the hypervisor versus the virtualized Linux operating system running inside the microVM.
* **Workspace**: The persistent directory tree allocated to an agent session.
* **Drive9**: VAM's custom virtual storage backend that mounts `/mnt/drive9` inside the guest using FUSE, while asynchronously syncing block deltas to remote cloud object stores.
* **Playbook**: A declarative, versioned blueprint representing a Directed Acyclic Graph (DAG) of steps, conditions, retry rules, and checkpoints that an Agent executes to accomplish a complex goal.
* **Workflow**: The live execution instance of a Playbook, coordinating the scheduling, variables, state transitions, and step outcomes.
* **Agent**: The active, stateful cognitive entity or LLM-driven reasoning loop running inside the guest VM userspace. An Agent is dedicated to a specific domain problem, and its capabilities are defined by its role and assigned Skill Set.
* **Skill**: A specialized, versioned, and self-contained package of instructions, domain knowledge, tool declarations, constraints, and examples located in storage.
* **Skill Set**: The complete, curated collection of Skills mounted into the Agent's workspace (e.g. at `/mnt/drive9/skills/`), which the Agent selectively resolves and invokes.
* **Tool / MCP Server**: Local scripts (Node.js, Python, Bash) or persistent Model Context Protocol servers run by the guest runtime to execute tasks.
* **HITL (Human-in-the-Loop)**: A security framework that pauses guest tool execution to seek authenticated approval from a human operator.
* **Checkpoint**: A lightweight file snapshot of the workspace filesystem state taken during execution.
* **Lifecycle Event**: A standardized CloudEvents 1.0 packet detailing operational transitions (e.g., `agent.started`, `tool.invoked`).
* **Audit Record**: A canonical, hash-chained, and signed security log entry recording a policy decision.

---

## 4. Why Host Brokering is Non-Negotiable

A traditional VM provides isolation but leaves security inside the guest up to the guest's OS. VAM rejects this model because a compromised agent runtime can bypass guest-side kernel controls. 

VAM implements **Host-Brokered Services** as a structural boundary:

* **Brokered Inference**: LLM keys are never present inside the VM. The guest runtime requests inference over `AF_VSOCK Port 52`, and the host appends credentials, sends the request to the provider, and streams raw tokens back.
* **Zero-Trust Secret Injection**: Credential variables like `$STRIPE_API_KEY` are held strictly on the host. When a tool makes an outbound connection, a host proxy intercepts the traffic, injects the key, and strips it from returning payloads.
* **Host-Only Telemetry Export**: The guest cannot talk directly to Datadog, Prometheus, or an OpenTelemetry Collector. The host-side pipeline (`vam-telemetry`) validates, redacts, and exports. See [Telemetry](/telemetry/overview/).

---

## 5. Workspace as living files

The Drive9 workspace under `/mnt/drive9/workspace/` is not just a scratch pad — the agent is expected to **maintain** durable files across runs on the same volume:

* **`PLAN.md`** — living plan (goals, status, next steps); host seeds a stub when missing.
* **`notes/`** — research dumps and durable notes the agent creates with `write_file`.
* **`.vam/host-artifacts/`** — large host tool payloads written by the guest after M4 execute.

Follow-up runs materialize the same volume HEAD. The orchestrator replays a bounded thread window plus `touched_paths` and hints the agent to `read_file` / `glob` `PLAN.md` and `notes/` when relevant — chat replay alone is not the source of truth for long research.

---

## 6. Map Your Contribution Persona

Depending on your engineering focus, you can jump directly to relevant architectural sections:

* **Integration & Client SDK Developers**
  - Read [Spec Overview](/spec/overview/) to understand Protocol Buffer wire schemas.
  - Read [Host Control API](/orchestrator/api/) to hook up external client calls.
* **Host Orchestration & Daemon Contributors**
  - Read [Orchestrator Overview](/orchestrator/overview/) and [Daemon Process Model](/orchestrator/overview/).
  - Read [Warm-Pool Snapshot Manager](/architecture/cold-start/) and [Admission Control](/orchestrator/overview/).
* **Security & Infrastructure Engineers**
  - Read [Security Overview](/security/overview/) and [Threat Model](/architecture/isolation/).
  - Read [Permissions](/security/permissions/) and [Network](/network/overview/).
* **Storage & Virtualization Contributors**
  - Read [Storage Overview](/storage/overview/).
  - Read [Core Overview](/core/overview/) and [AWS Jailer Sandbox](/core/overview/).
* **In-VM Guest Engine & Agent Runtime Developers**
  - Read [Runtime Overview](/runtime/overview/) and [PID 1 Guest Init](/runtime/overview/).
  - Read [Subprocess Supervisor](/runtime/tools/) and [Workflow DAG Interpreter](/runtime/workflow/).

---

## Next Steps

* Review the runnable components and maturity stages in [Project Status](/getting-started/project-status/).
* Learn about the Cargo tree structure and prerequisites in [Workspace & Documentation](/architecture/overview/).
