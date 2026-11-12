---
title: Cold start and the golden snapshot
description: How a new session gets a running microVM, what is measured, and what is not built.
status: shipped
proves:
  - vm-restores-from-the-golden-snapshot
  - crates/vam-orchestrator/src/golden.rs::bake_waits_for_the_real_guest
---

A session needs a running microVM. VAM gets one of two ways. On a node with snapshot restore on
(the default), it restores a **golden snapshot** baked at startup. Otherwise, or when a restore is
not possible, it boots cold.

The scenario `vm-restores-from-the-golden-snapshot` shows the restore path on the microVM lane: two
sessions in a row both emit `session.booted` with `data.bootPath: restore`, and the second run
succeeds. Process mode has no snapshots (`process_mode.rs`: snapshots off), so this page is about
the microVM node only.

## Bake

At startup the node boots one VM with the node's kernel and rootfs (2 vCPU, 512 MiB:
`GOLDEN_VCPU`, `GOLDEN_MEM_MIB`, `golden.rs:15-16`), waits until the real guest has registered and
says it is ready, pauses it and writes the snapshot (`bake_into`, `golden.rs:828`; the unit test
`bake_waits_for_the_real_guest` and `bake_does_not_snapshot_a_guest_that_never_becomes_ready` cover
the wait). The files live under `<drive9 root>/golden/<key>/`:

| File | What it is |
|---|---|
| `vm_state.bin`, `vm_mem.bin` | Firecracker's VM state and memory |
| `disk.ext4` | The empty 1 GiB ext4 data drive the bake booted against (`GOLDEN_DISK_BYTES`) |
| `key.json` | The key and what it was made from |

The node bakes before its API answers.

## The key

`compute_golden_key` (`golden.rs:263`) hashes the kernel, the rootfs, the root-drive layout tag
(`ROOT_DRIVE_LAYOUT`), the vCPU count and the memory size. Change the kernel or rootfs and the key
changes, so an old snapshot is never restored against a new image. A jailed node mixes in a
`jailed-flat-v1` tag: a jailed snapshot records in-jail file names and cannot restore on an unjailed
node, nor the reverse.

Hashing the kernel and rootfs on every start is slow, so the hashes are cached per file, keyed by
size and modification time. A file changed within the last two seconds is never cached
(`HashCache`, `golden.rs:195-245`).

## Restore

`resolve_restore_candidate` (`golden.rs:444`) decides for each new session. It returns a restore
candidate, or the reason to boot cold (`ColdBootReason`, `golden.rs:378`):

- `VAM_SNAPSHOT_RESTORE=0` (restore disabled);
- a storage profile other than `default`;
- the session image is missing, empty or larger than 1 GiB;
- no complete snapshot for the current key;
- on a jailed node, golden files the jailed VMM could write (`check_golden_files_for_jail`: the memory
  and state files must be readable and not writable by the jail uid).

On restore, Firecracker loads the snapshot, the session's own image replaces the data drive and the
guest remounts it (`image_fits_golden_disk`, `golden.rs:426`).

## What was measured

| Figure | What it measured | Source |
|---|---|---|
| About 225 ms | Session start with golden restore, median of 15, on the CI lane | CI microVM lane, measured 2026-10-24 |
| About 60 to 120 ms | The VM itself becoming ready, same runs | same line |

These are lane figures, on shared CI hardware, and one measurement day. They are not a promise. The
50 ms target for session start is not met. The node also prints the time its golden key took and
whether the hashes were cached (`[orchestrator] golden key: <n>ms cached=<bool>`,
`golden.rs:461-466`), and with `VAM_TIMING=1` the node logs the phases of a start (`timing.rs:1`).

## Not built

- No demand-paged memory (UFFD): `vam-core` and `vamd` never set a UFFD memory backend (the vendored Firecracker client has the type; nothing uses it).
- No pool of paused, ready sessions: each session restores on demand.
