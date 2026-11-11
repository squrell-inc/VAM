> Moved from the docs site on 2026-10-30: design intent, not built.

---
title: Design Principles
description: Core architectural tenants guiding the design and development of VAM.
---

:::caution[Alpha Stability Warning]
VAM engineering principles serve as the binding contract for current prototyping and future Rust development.
:::

VAM's architecture is shaped around ten non-negotiable system engineering principles. Every crate, contract, and interface must conform fully to these rules.

---

## 1. Contract-First Interfaces
* **The Rule**: Protocol Buffer definitions (`proto/`) serve as the single source of truth for all communication boundaries.
* **The Consequence**: Host components, the in-VM guest runtime, and external multi-language client SDKs are decoupled from the start. We do not write ad-hoc JSON parsers or loose socket protocols. This ensures type safety, backward-compatibility, and seamless service delivery.
* **Detailed Section**: [Spec Overview](/spec/overview/).

## 2. Untrusted Guest, Host-Enforced Authority
* **The Rule**: The guest microVM is treated as completely untrusted. Security, storage, and policy decisions must reside on the host.
* **The Consequence**: If an LLM-generated script executes arbitrary code and escalates its privileges inside the VM, it remains isolated. It has no access to credentials, cannot alter storage sync rules, and cannot modify firewall structures. The host enforces the security floor.
* **Detailed Section**: [Security Overview](/security/overview/).

## 3. One MicroVM per Isolated Session
* **The Rule**: Every individual agent session (or playbook execution) is assigned a dedicated, isolated Firecracker microVM.
* **The Consequence**: VAM avoids multi-tenant container pooling leaks or shared Unix namespace escapes. The microVM serves as a strict hardware-virtualized sandbox, with memory, CPU, and disk buffers cleared upon session termination.
* **Detailed Section**: [Host Control plane Daemon](/orchestrator/overview/).

## 4. Explicit Lifecycle & Idempotent Reconciliation
* **The Rule**: Every session state transition (Create, Boot, Run, Pause, Resume, Checkpoint, Terminate) must be explicit, trackable, and idempotent.
* **The Consequence**: If the node orchestrator daemon (`vamd`) crashes, restarts, or loses power, a continuous reconciliation engine identifies orphaned VMs or incomplete state transfers, cleanly restoring or reaping them without host state pollution.
* **Detailed Section**: [Warm-Pool Snapshot Manager](/orchestrator/pool/).

## 5. Host-Brokered Ingress, Egress, & Secrets
* **The Rule**: Secrets, LLM credentials, and database connections are stored strictly on the host. Guest requests are brokered over virtual sockets (`AF_VSOCK`).
* **The Consequence**: Guest processes never see raw cloud credentials or provider API tokens in memory or filesystem directories. Snapshots written to disk contain zero sensitive keys, preventing cloning leaks or unauthorized exfiltration.
* **Detailed Section**: [Host-Brokered Inference](/runtime/inference/) and [Zero-Trust Secret Injection Proxy](/security/secrets-proxy/).

## 6. Isolated-By-Default Networking
* **The Rule**: Guest VMs boot with zero direct external internet access. Direct networking is strictly opt-in, isolated, and rate-limited.
* **The Consequence**: Inter-VM scanning is structurally impossible. Standard tools communicate solely through host-brokered vsock proxies, while approved outbound traffic goes through the host egress proxy and DNS sinkholes. Every session runs the brokered network mode: the guest has no route and the host proxy makes every connection.
* **Detailed Section**: [Network Architecture](/network/architecture/).

## 7. Provider-Neutral Workspace Storage
* **The Rule**: The virtual filesystem layout (Drive9) must support local directories and major cloud object stores (S3, GCS, Azure Blob) behind a clean, abstract interface.
* **The Consequence**: VAM is not locked into any single cloud vendor. Developers can test Drive9 locally using regular folders, while enterprise deployments can seamlessly leverage robust, immutable remote cloud backends.
* **Detailed Section**: [ObjectStoreBackend Trait](/storage/backend-interface/).

## 8. Bounded Resources & Backpressure
* **The Rule**: Every queue, socket channel, memory buffer, disk spool, and CPU pool must enforce strict maximum boundaries.
* **The Consequence**: A slow external consumer reading telemetry, a massive file-generator loop, or an aggressive metrics stream will hit a backpressure ceiling or force-eviction trigger, preventing the host daemon from running out of memory.
* **Detailed Section**: [Telemetry Overview](/telemetry/overview/) and [Admission Control & Queuing](/orchestrator/admission/).

## 9. Observable but Privacy-Preserving Execution
* **The Rule**: Telemetry metrics, CloudEvents, and audit records must be collected continuously, but sanitized on the host before export or local storage.
* **The Consequence**: Operators get high-resolution OpenTelemetry traces, latency breakdowns, and system metrics without leaking customer PII, raw agent prompt strings, or terminal outputs to external APM databases.
* **Detailed Section**: [Telemetry Privacy & Redaction](/telemetry/privacy/) and [Cryptographic Audit Ledger](/telemetry/audit-ledger/).

## 10. Honest Alpha Guarantees
* **The Rule**: Documentation, diagrams, and planned code blocks must clearly state their prototype status, and avoid claiming unsupported certifications, performance guarantees, or absolute compliance.
* **The Consequence**: Readers can easily audit the current repository state versus identical design targets, promoting high-fidelity system engineering and setting clear, actionable benchmarks for contributors.
* **Detailed Section**: [Project Status](/getting-started/project-status/).

## 11. Immutable Supply Chain & Verified Admission
* **The Rule**: Guest root filesystems must be bit-for-bit reproducible, cryptographically signed, and verified via fail-closed admission policies before boot.
* **The Consequence**: No mutable, unverified, or tampered OS image can be run in the platform. VAM guarantees that the code running inside any microVM matches an approved, audited build manifest.
* **Detailed Section**: [Images & Build Overview](/build/overview/).

## 12. Single-Agent Identity & Composable Skill Sets
* **The Rule**: Each VAM session is restricted to exactly one active, stateful Agent identity. Cognitive capability is composed via dynamic, versioned Skill Sets rather than top-level, multi-agent orchestration.
* **The Consequence**: This keeps the physical boundary of the VM aligned 1:1 with agent responsibility. If an agent needs to perform multi-step work, it executes as a single cognitive worker, selectively activating and using the tools from its mounted Skill Set. Sub-agent semantics or nested delegation are explicitly deferred as future platform layers.
* **Detailed Section**: [Agent Identity](/runtime/agent/).
