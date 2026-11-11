# VAM Core Development Invariants

This file details the strict coding and operational rules that every agent or developer must adhere to when working inside `crates/vam-core/`. For detailed design rationales, refer to [Development Invariants](/core/invariants/) in the VAM documentation site.

---

## 1. Containment and Sandboxing (Jailer)
* **Never spawn raw Firecracker processes in production**. You must run all microVMs wrapped inside an AWS Jailer sandbox container.
* **Never reuse directories**. Target a completely unique VM ID folder (`/srv/jailer/firecracker/<vm-id>/`) for every instance to prevent socket lockups.
* **Always hardlink or bind-mount resources**. Symbolic links to the kernel (`vmlinux`) and rootfs image (`rootfs.ext4`) will break after the `chroot` jail barrier is applied.

## 2. API Sequencing and State Barriers
* **Enforce the Crossed Boot Barrier**. Send all device hardware config payloads (`PUT` requests to `/boot-source`, `/drives/*`, `/machine-config`, `/vsock`) *before* issuing `POST /actions { "action_type": "InstanceStart" }`. Config API endpoints become read-only once booted.
* **Validate configuration on the client**. Perform input sanity checks on path structures, vCPUs, and memory size before executing the process or binding sockets to save host-side IO operations.

## 3. Communication and Addressing
* **Pre-allocate Context IDs**. Claim a guest Context ID (CID) from the allocator pool *before* dispatching `PUT /vsock`. Overlapping CIDs crash the kernel-level VSOCK device initialization.
* **Lock Port 52 for Control Lines**. The gRPC and core stream bridges must utilize Guest Port `52` exclusively.

## 4. Snapshot Restoration Hazards
* **Always re-seed entropy on state load**. Pushing a fresh entropy packet over the vsock connection on VM resume is required to prevent cryptographic collision loops.
* **Force immediate time synchronization on resume**. Correct the guest OS clock instantly upon restore to prevent clock drift timeouts on external OAuth/API sessions.

## 5. Coding Standards and GC
* **Zero tolerance for panics**. Do not execute `.unwrap()`, `.expect()`, or raw `panic!` blocks in host-side library routines. Report all errors as an explicit `Result::Err(VamCoreError)`.
* **Enforce idempotent garbage collection**. Complete the full GC pipeline (unmounting dev directories, removing socket files, clearing jailer directories, releasing CID blocks) on every termination flow.
