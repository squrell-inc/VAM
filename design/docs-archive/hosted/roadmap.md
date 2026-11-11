> Moved from the docs site on 2026-10-31: design intent, not built (no files or RAG broker; no triggered runs).

---
title: Design Rationale and Roadmap
description: Product vs platform framing, HSP phasing, rejected alternatives, marketplace end state, and open questions.
---

:::note[Kernel hooks landed; first-party brokers remain]
The HSP **proto contract** is committed (Phase 0). Kernel transport, manifest admission, catalog merge, Invoke, lifecycle hooks, and trust-tier hardening are implemented through workstream Phase 6. First-party files/RAG brokers and marketplace isolation remain later deliverables (see product phasing below).
:::

## Product vs Platform

HSP separates **product features** (files, RAG, DLP) from the **platform integration layer** (register, invoke, hooks, manifests). VAM ships first-party brokers as product. Operators and marketplace vendors ship additional brokers on the same contract as platform.

Without HSP, every new credentialed capability becomes an in-process Rust adapter — coupling release cycles, expanding `vamd` attack surface, and blocking a third-party ecosystem.

---

## Phasing

| Phase | Deliverable | Depends on |
|---|---|---|
| **0** | `hosted_services.proto` + `FileRef`/`KnowledgeRef` + baseline fixtures | — `[x]` |
| **1** | Generic `hosted:` `HostToolAdapter` shim + HSP client in `vamd` + UDS transport | Phase 0 |
| **2** | `vam.files` broker (hooks + `files.*` tools) | Phase 1 |
| **3** | `vam.rag` broker (auto-retrieve hook + `knowledge.*` tools) | Phase 2 |
| **4** | Capability manifests + trust tier enforcement | Phase 1 |
| **5** | Broker signing, distribution registry, third-party VM isolation | `vam-build`, Phase 4 |

Phases 1–4 deliver enterprise file + knowledge workflows. Phase 5 opens the marketplace. Phase 0 (proto contract) is committed.

---

## Rejected Alternatives

### In-process Rust adapters for every service

**Rejected.** Works for `echo` and `http_get` (`[Implemented]`), but every new integration requires `vamd` rebuild, shared memory with vendor logic, and monolithic release trains. HSP keeps `vamd` thin.

### Guest MCP for credentialed services

**Rejected.** MCP inside the guest would expose credential handles to untrusted RAM. [Guest MCP is unimplemented](/runtime/tools/) by design. Host-brokered tools and HSP keep secrets on the host.

### Per-service vsock ports

**Rejected.** Multiplying vsock ports breaks the single Port 52 control plane model and complicates guest init. HSP uses host-local transport (UDS) with vsock reserved for future isolated broker VMs — not guest-facing ports.

### Agent-generated download URLs

**Rejected.** Letting the agent mint or embed signed URLs enables exfiltration via prompt or tool output. URLs are minted only in `on_response` by the files broker, returned to the API client — never guest context. See [File and Knowledge References](/hosted/resources/).

### Drive9 as attachment store

**Rejected.** Drive9 is the agent scratchpad inside the guest trust boundary. User uploads and knowledge corpus are control-plane resources managed by brokers. Conflating the two breaks retention, DLP, and cross-run attachment semantics.

---

## Marketplace End State

Third-party brokers run in **plain Firecracker microVMs** with a dedicated drive — the same isolation model as agent workloads, but executing broker code instead of ReAct agents:

```text
  vamd ── HSP over vsock ──► broker microVM
                               ├── broker process
                               ├── local backend drive
                               └── no guest agent workload
```

Trust tier **third-party** enforces VM boundary. Manifest signing and admission gate broker images before registration.

Open dependencies for marketplace:

- [`vam-build`](/build/overview/) image pipeline for broker rootfs
- Signed OCI admission ([Signing & Admission Policy](/build/signing/))
- vsock HSP connector in `vamd` (transport swap from UDS)

---

## Open Questions

### Credential custody for vendor-run brokers

When a third-party broker runs in its own microVM, who holds vault keys?

- **Option A:** `vamd` still resolves secrets; passes per-invoke over vsock HSP (extends current model)
- **Option B:** Broker VM gets scoped secret proxy with TTL tokens (extends [Secret Injection Proxy](/security/secrets-proxy/))

Leaning A for consistency with today's host adapter model. Needs threat-model review for vsock HSP channel.

### Local catalog vs distribution registry

Skill packages today use local draft/release channels in `vamd` with digest verification but **no signature verification** in [`skill_package_admit.rs`](https://github.com/squrell-inc/VAM/blob/main/crates/vam-orchestrator/src/skill_package_admit.rs) — digest and channel only.

Broker distribution likely needs stronger guarantees:

- Signed broker images (cosign/minisign)
- Operator-approved registry mirror
- Revocation list for compromised broker versions

Relationship to skill package admission is unresolved: shared registry or separate broker catalog?

### vam-build prerequisite

Marketplace broker VMs need reproducible, signed rootfs images. The [`vam-build`](/build/overview/) crate and [RootFS Assembly](/build/rootfs/) pipeline are design-phase / script-based today. Phase 5 marketplace work is blocked on hardened image supply chain.

### Runtime-spec vs operated infrastructure

Two deployment stories coexist in discussion:

| Model | Who runs brokers | Config surface |
|---|---|---|
| **Runtime-spec** | Declared in agent/run spec; `vamd` ensures brokers available | `run.json`, capability manifest refs |
| **Operated infrastructure** | Operator deploys broker fleet independently; `vamd` discovers via registration | Helm/k8s, systemd, config files |

Initial first-party brokers assume **operated infrastructure** (co-located with `vamd`). Runtime-spec broker selection is an open product question for multi-tenant SaaS.

---

## Related

* [Hosted Services Overview](/hosted/overview/) — motivation and non-goals
* [HSP Architecture](/hosted/architecture/) — component diagram and lifecycle
* [Capability Manifests](/hosted/capability-manifest/) — trust tiers
* [Broker-Triggered Runs](/hosted/triggered-runs/) — scheduled and event-driven run design
* [Project Status](/getting-started/project-status/) — workspace maturity matrix
