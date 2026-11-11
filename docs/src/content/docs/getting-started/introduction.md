---
title: Introduction
description: What the Virtual Agentic Machine (VAM) is and why it exists.
---

:::caution[Alpha Stability Warning]
VAM is currently in **Alpha**. The architectural definitions, API contracts, in-VM runtime, security controls, and storage mechanisms are design specifications undergoing active prototyping. No production-readiness, security certifications, or API stability guarantees are provided at this stage.
:::

**VAM** (Virtual Agentic Machine) is a Rust-based, contract-first workspace designed to build secure, isolated, and highly observable infrastructure for running autonomous AI agents. 

In VAM, each isolated microVM represents exactly one purpose-built **Agent** dedicated to solving a specific domain problem. Rather than relying on top-level, multi-persona "Assistant" orchestrations, an agent's capabilities are composed directly from specialized, versioned **Skills** (such as database development or business analysis). This ensures a clean design where the physical boundary of the VM aligns perfectly with the cognitive boundary of a single agent.

---

## The Core Problem: Untrusted Agency

Autonomous AI agents are inherently high-risk workloads. To accomplish real-world tasks, they must:
* Execute dynamic, model-generated code (Bash, Python, Node.js).
* Install unverified third-party dependencies from public repositories.
* Interact with local filesystems, external APIs, and internal networks.

If an agent is subjected to a prompt injection attack, or if a generated script runs a destructive command (such as `rm -rf /` or unauthorized data exfiltration), standard container boundaries (`Docker`, `LXC`) present a massive attack surface. Shared host kernels and namespaces are vulnerable to escape, and traditional container engines do not natively support sub-millisecond file checkpoints, fine-grained system call filtering, or host-brokered secure environments.

---

## The VAM Solution: MicroVM Sandboxing

VAM addresses these risks by wrapping every agent session in an isolated **Firecracker microVM**. This architectural choice delivers clear security and operational boundaries:

* **Hardware-Level Isolation**: Each session runs on its own virtualized Linux kernel backed by KVM. There is no shared user/kernel space with the host or other tenant sessions.
* **Host-Controlled, Guest-Executed Model**: The guest VM (`vam-runtime`) is entirely untrusted. All high-privileged resources—such as cloud storage keys, external API credentials, and network firewall policies—are maintained strictly on the host and brokered over isolated virtual sockets (`AF_VSOCK`).
* **Sub-Millisecond Checkpoints**: Leverages Copy-on-Write (CoW) overlay block devices and memory state serialization to pause, clone, resume, or rollback agent environments instantly.
* **Deterministic Guardrails**: CPU, memory, and bandwidth quotas are enforced directly at the hypervisor boundary, protecting the physical host from noisy neighbors or run-away loops.

---

## What VAM Is Not (Yet)

To set correct expectations, it is vital to understand VAM's current boundaries:
* **Not a Chat UI**: VAM is low-level, developer-facing system infrastructure. It does not provide web interfaces, prompt playgrounds, or user-facing agent frameworks.
* **Not a Hosted SaaS**: It is an open-source technology stack intended to be deployed on bare-metal or nested-virtualization nodes.
* **Not a Kubernetes Replacement**: VAM is node-local. It does not manage multi-host consensus, fleet scheduling, or distributed networking; those are the responsibility of a separate, upstream control plane.
* **Not Yet Runnable in Production**: The repository is in an active prototyping phase. See [Project Status](/getting-started/project-status/) for details on what can be run today.

---

## Who is This For?

* **Platform & Infrastructure Engineers** building secure, serverless agent execution services.
* **Runtime & Rust Contributors** implementing microVM sandboxes, virtual filesystems (Drive9), or network filtering engines.
* **SDK & Agent Framework Authors** integrating client-side workflows with VAM's contract-first host daemon APIs.
* **Security Engineers & Compliance Reviewers** auditing sandbox isolation, AWS Cedar access policies, and cryptographic logs.

---

## Reading Path

To navigate the documentation effectively, we recommend the following path:
1. **[Platform Tour](/getting-started/platform-tour/)**: A high-level overview of VAM's end-to-end components and terminology.
2. **[Project Status](/getting-started/project-status/)**: An honest review of what is implemented today and what is currently planned.
3. **[Workspace & Documentation](/architecture/overview/)**: How to explore the repository layout and run local builds.
4. **[Architecture Overview](/architecture/overview/)**: Deep dive into the host control plane, guest runtime, and isolated service planes.

