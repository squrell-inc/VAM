---
title: Storage
description: Where an agent's files live, how workspaces are kept apart, how a finished turn is saved, and what is not built.
status: shipped
proves:
  - agent-workspaces-are-isolated
  - workspace-matches-conversation-after-vm-stop
  - crates/vam-storage/src/gc.rs::gc_preserves_shared_chunk_when_one_prefixed_workspace_removed
  - crates/vam-storage/src/gc.rs::gc_drops_chunk_when_no_manifest_references_remain
  - crates/vam-storage/src/log_quota.rs::usage_is_measured_once_then_counted
  - crates/vam-storage/src/cas.rs::test_chunk_store_lifecycle
  - crates/vam-storage/src/manifest.rs::test_merkle_manifest_determinism
---

Storage is the `vam-storage` crate, called Drive9. It holds each workspace on the node's local disk. A microVM sees its workspace as an ext4 disk mounted at `/mnt/drive9`; a process-mode guest works in a directory.

## What is built

| Part | What it does | Code |
|---|---|---|
| ext4 images | Builds an ext4 image from a workspace directory and overlays files onto it. | `crates/vam-storage/src/disk.rs:104`, `:149` |
| Directory backend | `LocalDirBackend` stores objects under a root directory. It is the only backend. | `crates/vam-storage/src/backend.rs:45-62` |
| Chunks (CAS) | Files are cut into chunks and stored under their SHA-256 digest, so identical chunks are stored once. | `crates/vam-storage/src/cas.rs:7`, `:68` |
| Manifests | A manifest lists a workspace's files by chunk hash. Its own hash is deterministic and names the workspace state. | `crates/vam-storage/src/manifest.rs:23-66` |
| Owner prefixes | A workspace id carries the owner and the agent, so two agents never share a workspace. A guest cannot choose the prefix. | `crates/vam-storage/src/owner_prefix.rs` |
| Garbage collection | Removes a workspace tree, then deletes chunks no manifest still names. | `crates/vam-storage/src/gc.rs:17`, `:71`, `:105` |
| Thread log quota | Limits one thread to 256 MiB and one workspace to 1 GiB, with a 1 MiB reserve. | `crates/vam-storage/src/log_quota.rs:17-21` |
| Turn commits | Saves workspace and conversation together after each round. See [Turn commits](/storage/turn-commits/). | `crates/vam-orchestrator/src/turn_commit.rs` |

## What a turn commit guarantees

Storage is not transactional on every write. The guarantee is per finished turn: after a stop, files and conversation both return to the newest complete turn commit. The `workspace-matches-conversation-after-vm-stop` scenario checks this on a microVM.

## Agents are kept apart

Each agent gets its own workspace id. The `agent-workspaces-are-isolated` scenario runs two agents and checks that neither sees the other's files.

## Cleanup

`gc_preserves_shared_chunk_when_one_prefixed_workspace_removed` shows a chunk shared by two owners survives when one workspace is removed. `gc_drops_chunk_when_no_manifest_references_remain` shows it is deleted when none is left. `usage_is_measured_once_then_counted` covers the log quota's usage count.

## Not built

* No FUSE layer. No crate mounts a FUSE file system in the guest; the guest sees an ext4 disk or a directory.
* No cloud object-store backends, and no background sync to one.
* No encryption of workspace data at rest.
