---
title: Core Overview
description: What the vam-core host crate does, its modules, how a microVM is started and torn down, and what is proved.
status: shipped
proves:
  - jailer-confines-the-vmm-process
  - crates/vam-core/tests/jailed.rs
  - crates/vam-core/src/jail.rs::refuses_uid_or_gid_zero
  - crates/vam-core/src/jail.rs::refuses_a_node_that_is_not_root_and_names_the_opt_out
  - crates/vam-core/src/state.rs::test_invalid_transitions
  - crates/vam-core/src/cid.rs::test_cid_allocation_lifecycle
  - crates/vam-orchestrator/src/daemon.rs::vm_node_names_every_missing_variable
---

`vam-core` is the host-side crate that starts, controls and stops one Firecracker microVM. It does not
route sessions or pick which VM serves a request: that is [`vamd`](/orchestrator/overview/). For what
the jailer and the VM keep away from the host, see [Isolation](/architecture/isolation/).

## Modules

| Module | What it holds | Where |
|---|---|---|
| `config` | `MicroVmConfigBuilder` and `MicroVmConfig`: kernel, rootfs, data drives, vCPUs, memory, guest CID, boot arguments. Defaults are 1 vCPU and 512 MiB. | `crates/vam-core/src/config.rs:53-63` |
| `vmm` | The `Vmm` trait and its implementations `FirecrackerVmm` and `MockVmm`. | `vmm.rs:75`, `:340`, `:163` |
| `process_vmm` | `ProcessVmm`, which runs the guest as a plain host process. Built only with the `process-guest` feature, for tests. | `process_vmm.rs:161`, `Cargo.toml` |
| `state` | `VmState` and `VmStateMachine`. | `state.rs:5-61` |
| `cid` | `CidAllocator`: guest vsock context ids. | `cid.rs:7` |
| `jail` | Jailer settings, preflight refusals, staging files into the jail, and teardown. | `jail.rs` |

The crate talks to Firecracker through the vendored SDK in `crates/vam-firecracker`. VAM uses it to
spawn the jailer, call Firecracker's REST API over a Unix socket, and pause, resume and snapshot.

## The state machine

A VM moves `Uninitialized`, `Configured`, `Booting`, `Running`; from `Running` it can go to `Paused` and
back. `Booting`, `Running` and `Paused` can go to `Halted`, and `Halted` goes to `Reaped`. Any state may
go straight to `Reaped` for forced cleanup. Every other move is an error (`state.rs:17-37`).

## The `Vmm` trait

The trait covers `spawn`, `configure_hardware`, `start_instance`, `pause`, `resume`, `create_snapshot`,
`stage_snapshot_restore`, `load_snapshot`, `patch_data_drive_path`, `patch_vsock_uds_path`, `shutdown`,
`get_pid`, `detach`, `adopt` and `cleanup_after_exit` (`vmm.rs:75-160`). Code above it works on the
trait, so a test can use `MockVmm`.

## Context ids

`CidAllocator::default()` hands out CIDs 3 to 65535 and never 0, 1 or 2 (`cid.rs:19`). A range that
starts below 3 panics (`cid.rs:23`). Running out returns `CidExhausted`.

## The jail

Settings (`JailConfig`, `jail.rs:113`): uid and gid 65400, base directory `/srv/jailer`, cgroup v2
(`jail.rs:23-27`). The jailer runs Firecracker in a new PID namespace (`vmm.rs:557`). Files enter the
jail by hard link, not copy, so the kernel, rootfs and snapshot must be on the same filesystem as the
base directory.

Before a VM starts, preflight refuses with a named reason (`JailRefusal`, `jail.rs:137`):

- the node is not root;
- the jail uid or gid is 0;
- the cgroup version is not 1 or 2, or its mount is missing;
- the Firecracker or jailer binary is missing, not executable, or writable by group or others;
- the base directory is unusable, or an asset is on another filesystem or has unsafe permissions.

The error text names the opt-out (`--no-jailer` or `VAMD_USE_JAILER=0`) so the operator knows how to run
unconfined on purpose. A VM node that lacks a binary or path variable also refuses to start and names
each one (`crates/vam-orchestrator/src/daemon.rs`).

At teardown the jail is removed and stale jails left by a dead `vamd` are swept (`sweep_stale_jails`,
`jail.rs:690`).

## Proved by

| Claim | Proof |
|---|---|
| A jailed VM is confined and torn down; a jailed restore stages the golden files read-only | `jailer-confines-the-vmm-process`, `crates/vam-core/tests/jailed.rs` (lane) |
| Preflight refuses uid 0, a non-root node, bad binaries | `jail.rs::refuses_uid_or_gid_zero`, `jail.rs::refuses_a_node_that_is_not_root_and_names_the_opt_out` |
| A VM node names every missing variable | `daemon.rs::vm_node_names_every_missing_variable` |
| Invalid state moves fail | `state.rs::test_invalid_transitions` |
| CIDs are allocated and released | `cid.rs::test_cid_allocation_lifecycle` |

## Not built

- **CPU and memory limits.** The jailer is given no cgroup resource limit. A guest is bounded by its
  vCPU and memory size only.
- **I/O rate limits.** The Firecracker rate limiter is always `None` (`vmm.rs:35,841,1079`). The old
  design is archived in `design/docs-archive/core/ratelimit.md`.
