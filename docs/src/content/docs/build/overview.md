---
title: Images & Build Overview
description: Role and boundaries of the image builder and Vamfile pipeline.
status: planned
---

## Crate Role & Supply-Chain Boundary

The `vam-build` crate is VAM's offline, build-time supply chain subsystem. Unlike the other crates in the workspace that run on active hypervisor nodes (such as `vam-core` and `vam-orchestrator`), `vam-build` operates entirely at compilation time to produce immutable, hardened virtual machine root filesystems (`rootfs.ext4`).

By creating a dedicated build plane, VAM enforces a critical security and operational boundary:
* **Separation of Concerns**: Building guest filesystem images is decoupled from executing virtual machine sessions. Production nodes running the `vamd` control daemon do not require compile tools, package managers, or image builders.
* **Hermetic Execution**: Image compilation executes in dedicated, isolated build environments—never on production virtualization hosts.
* **Immutable Output**: Builds yield write-once, cryptographically signed block devices. Any runtime change inside a running guest session is confined to ephemeral overlays (see [CoW Workspace Overlay](/storage/overview/)), preserving the integrity of the base image.

```text
  [ Vamfile Recipe + Context ]
               │
               ▼
┌──────────────────────────────────────────┐
│      Build Plane (`vam-build`)           │
├──────────────────────────────────────────┤
│  - Parse & Plan Recipe Steps             │
│  - Execute Package Installing (Rootless) │
│  - Assemble ext4 block device            │
│  - Inject `vam-runtime` & FUSE drivers   │
│  - Optimise, Resize, & Sparsify          │
│  - Generate SBOM & SLSA Provenance       │
│  - Package & Sign OCI Artifact           │
└──────────────────┬───────────────────────┘
                   │
                   ▼ (OCI Registry / ORAS)
┌──────────────────────────────────────────┐
│     Verification Plane (`vamd`)          │
├──────────────────────────────────────────┤
│  - Validate signature & trust root       │
│  - Verify SBOM and CVE scan status       │
│  - Enforce Fail-Closed Admission Policy  │
└──────────────────┬───────────────────────┘
                   │ (Approved Local Cache)
                   ▼
┌──────────────────────────────────────────┐
│     Virtualisation Plane (`vam-core`)    │
├──────────────────────────────────────────┤
│  - Hardlink rootfs to secure chroot      │
│  - Spawn Firecracker VMM                 │
└──────────────────────────────────────────┘
```

---

## The Core Artifact: Guest RootFS

The target of `vam-build` is a clean, minimal `rootfs.ext4` image file. This artifact contains:
1. **The Base OS**: A stripped-down Linux distribution (Alpine, Ubuntu, or Red Hat UBI).
2. **System Dependencies**: Runtimes, compilers, or utilities required by agent scripts (Node.js, Bun, Python).
3. **The Guest Runtime**: The static, compiled `vam-runtime` binary injected into `/usr/local/bin/vam-runtime` to act as guest PID 1.
4. **Virtual Filesystem Drivers**: Standard Linux FUSE libraries allowing the guest runtime to mount the Drive9 workspace.

---

## Planned Module Tree

The planned module structure inside `crates/vam-build/src/` is organized as follows:

```text
crates/vam-build/
├── src/
│   ├── recipe/          # Vamfile lexer, parser, AST builder, and schema validator
│   ├── source/          # Base rootfs archives, contexts, and digest resolution
│   ├── planner/         # Execution graph compiler and content-addressed cache keys
│   ├── executor/        # Rootless directory, loopback, and future VM builders
│   ├── rootfs/          # ext4 disk image allocation, formatting, and mke2fs -d
│   ├── injector/        # static `vam-runtime` and FUSE driver asset injector
│   ├── optimizer/       # filesystem shrinking, zeroing, and sparsification
│   ├── metadata/        # OCI manifest, SPDX/CycloneDX SBOM, and SLSA provenance
│   ├── registry/        # OCI artifact packager, push/pull ORAS transport
│   ├── policy/          # Cosign signature verification and host admission checks
│   └── error.rs         # VamBuildError mappings and verification failures
└── SKILLS.md            # Invariants for RootFS image building
```

---

## System Consumers

The artifacts compiled by `vam-build` are consumed across the platform:
* **Central Registries**: Distribution hubs that store, index, and version image bundles as standard OCI artifacts.
* **Admission Controllers**: The security gate inside the host daemon (`vamd`) that validates signatures, hashes, and compliance reports before authorizing execution.
* **`vam-core`**: The host virtualization library that takes the verified local `rootfs.ext4` path, locks it inside the Jailer sandbox, and boots Firecracker.

---

## Next Steps

* Study the complete parse-to-admission pipeline in [Build Architecture](/build/architecture/).
* Learn about the declarative syntax of recipes in [Vamfile Language](/build/vamfile/).
* Examine our compilation CLI commands in [`vam build` Command](/build/cli/).
