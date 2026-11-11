---
title: Build Architecture
description: The pipeline, worker topology, and trust boundaries of the image build subsystem.
status: planned
---

## The End-to-End Build-to-Admission Pipeline

The VAM image supply chain enforces strict unidirectional progression of states from raw source recipe to runtime guest execution. Every stage is mathematically verifiable and isolated.

```text
  ┌────────────────────────────────────────────────────────┐
  │ 1. Parse & Plan                                        │
  │    - Lex and parse Vamfile to AST                      │
  │    - Resolve source tags to immutable SHA-256 digests  │
  │    - Generate a directed execution tree               │
  └──────────────────────────┬─────────────────────────────┘
                             │
                             ▼
  ┌────────────────────────────────────────────────────────┐
  │ 2. Isolated Stage Execution                            │
  │    - Execute build commands in a rootless staging dir  │
  │    - Bind read-only base directories                   │
  │    - Record layer file-state deltas                    │
  └──────────────────────────┬─────────────────────────────┘
                             │
                             ▼
  ┌────────────────────────────────────────────────────────┐
  │ 3. RootFS Assembly & Inject                            │
  │    - Format raw ext4 block device (mke2fs -d)          │
  │    - Inject static musl-compiled `vam-runtime`         │
  │    - Perform guest OS sanity checks                    │
  └──────────────────────────┬─────────────────────────────┘
                             │
                             ▼
  ┌────────────────────────────────────────────────────────┐
  │ 4. Supply-Chain Attestation                            │
  │    - Generate CycloneDX SBOM & SLSA Provenance         │
  │    - Record inputs, hashes, and tool versions          │
  └──────────────────────────┬─────────────────────────────┘
                             │
                             ▼
  ┌────────────────────────────────────────────────────────┐
  │ 5. Registry Distribution                               │
  │    - Package rootfs and metadata as an OCI artifact    │
  │    - Cryptographically sign artifact using Cosign/KMS  │
  │    - Push to centralized, OCI-compliant registry       │
  └──────────────────────────┬─────────────────────────────┘
                             │
                             ▼
  ┌────────────────────────────────────────────────────────┐
  │ 6. Host Admission & Run                                │
  │    - `vamd` pulls and verifies signatures & hashes     │
  │    - Reject unapproved, modified, or expired digests   │
  │    - Boot Firecracker with fail-closed guarantee       │
  └────────────────────────────────────────────────────────┘
```

---

## Isolated Worker Topology

VAM builds are never executed directly on production host machines running active virtual machines. Because the build process requires fetching packages from the public internet, compiling raw sources, and running post-installation hook scripts, the execution environment operates under an untrusted security posture.

* **Dedicated Build Nodes**: Build workers run on physically or logically isolated systems with strict egress network constraints.
* **No Host Mutation**: Build steps are executed inside a temporary directory structures or throwaway container sandboxes. The local storage of the host run node remains completely unaltered.
* **Ephemeral Workspaces**: When a build step finishes, its intermediate files are packaged, cache states are updated, and the sandbox is destroyed.

---

## Caches, Registries, & Content Addressability

VAM leverages a content-addressed system across the entire pipeline to guarantee speed, safety, and correctness:

### Step Cache (Content-Addressed)
Each directive in the `Vamfile` is assigned a unique cache key based on:
1. The preceding stage's filesystem hash.
2. The exact text of the current command.
3. Any environment variables declared.
4. Resolved hashes of copied local/remote assets.

If a cache hit is detected locally or in the remote build cache, the step is skipped, and the pre-computed filesystem delta is applied directly.

### Central Registries (OCI & ORAS)
Images are packaged as non-container OCI artifacts and distributed via standard container registries.
* Rather than inventing a custom distribution protocol, VAM uses ORAS (OCI Registry as Storage) library interfaces to push and pull rootfs layers and metadata files.
* Since tags (e.g., `latest`, `v1.0-alpine`) can be overwritten, the host admission controller **strictly rejects mutable tag references** in release mode, enforcing references by immutable SHA-256 digest (e.g., `image@sha256:abcd...`).

---

## Trust Boundaries & Isolation Layers

To protect the integrity of the build server and final VM artifacts, the architecture defines three strict security boundaries:

| Boundary | Vector Protected | Mitigation Mechanism |
|---|---|---|
| **Build Input** | Malicious recipes or untrusted third-party packages | Parse-time syntax schema constraints; mandatory signature pinning for base roots and binaries. |
| **Build Execution** | Rogue compilation scripts or package hook exploits | Execution confined to unprivileged user-namespaces or nested microVMs; strict deny-by-default egress filters. |
| **Admission Check** | Tampered registry artifacts or tag-switching attacks | `vamd` performs multi-signature key verification and re-hashes local assets before handing them to `vam-core`. |

---

## Responsibility Ownership

The modular responsibilities of the build system are separated to maintain a cohesive rust library API:

* **`recipe` & `source`**: responsible for understanding "what to build." They resolve mutable references (like tag strings or package names) into precise, cryptographic digests.
* **`planner` & `executor`**: responsible for "how to run the steps." They coordinate cache state, manage sandbox mount/directory scopes, and execute commands safely.
* **`rootfs` & `injector`**: responsible for "how to format and prepare the disk." They create the physical filesystem block, write files deterministically, and embed the critical host-guest interfaces.
* **`optimizer` & `metadata`**: responsible for "how to pack, size, and certify." They minimize raw size, zero unused space, and record SBOM and provenance chains.
* **`registry` & `policy`**: responsible for "how to ship and audit." They handle the ORAS transport layer and provide signature verification rules for host runtimes.
