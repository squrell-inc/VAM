---
title: Turn Commits & Thread Log
status: shipped
proves:
  - workspace-matches-conversation-after-vm-stop
  - follow-up-continues-after-stop
  - thread-history-lists-tool-results
  - crates/vam-orchestrator/tests/thread_api_tests.rs
description: "How a workspace and its conversation stay in step: turn commits, the stop rule, kept files, the log size limit, and the thread log v1 format."
---

A follow-up message must find the files and the conversation where the last task left them, even after a stop or a `vamd` restart. Two things make that true: a **turn commit** pairs a workspace snapshot with a position in the conversation, and the conversation itself lives in an append-only **thread log** inside the workspace.

---

## 1. Turn commits

After a round whose tools may write files, the guest asks the host for a commit (`CommitTurn`). A turn commit has two halves, and it is **complete** only when both are recorded:

1. **Manifest.** While the guest waits, the host snapshots the workspace (the host `.vam/` directory is left out) and records the manifest hash.
2. **Boundary.** The position in the thread log right after the round's tool results. The host records it when the guest's next model request carries a tool-result tail whose digest equals the one `CommitTurn` announced. A guest cannot complete a commit by saying so; only the host seeing the matching tail does.

### 1a. How a microVM takes a turn commit

A microVM guest writes its own disk image, so the host cannot read its files while it runs. The commit is a private copy of that image, taken at a moment when the image is a complete filesystem:

1. **Freeze (guest).** Before asking, the guest runs `syncfs` on `/mnt/drive9`, then `FIFREEZE` (`_IOWR('X', 119, int)`). Nothing is written to the workspace disk until the thaw. The ioctl is a no-op when `/mnt/drive9` is not an ext4 mount, so process guests are unchanged. If the freeze fails, that round's commit is skipped and the reason is logged on the console.
2. **Copy (host).** The guest sends `CommitTurn` (the wire format does not change) and waits. The host copies `sessions/<id>/drive9.ext4` to `sessions/<id>/turns/<round>.ext4`: a reflink (`FICLONE`) when the volume supports it, otherwise a hole-preserving copy that must finish within 100 s. A copy that is late is deleted and refused with code 13, so a copy never outlives the guest's thaw. The copy is a new file (`O_EXCL`, mode 0600, owned by the node) next to the live image and is never given to a jail or a VM.
3. **Thaw (guest).** The guest thaws (`FITHAW`) on the host's reply, on an error, on a timeout, or if the call is dropped. A plain thread thaws after 125 s even if the async runtime is stuck (`CommitTurn` has a 120 s timeout).

The host keeps the newest complete copy and at most two copies waiting for their boundary; older copies, copies of failed commits and leftovers of earlier runs are removed at the next snapshot. A copy that cannot be made (no space, the 16 GiB image-size cap, a deadline) fails the commit with `storage_commit_failed` and the usual event, naming no host path.

**Cost model.** Without reflinks the copy reads and writes the image's allocated bytes: about 0.3 s per 100 MB on a local SSD, while the guest is frozen. It checks free space first (the image's allocated size plus 512 MiB). With reflinks (XFS with `reflink=1`, btrfs) the copy is a metadata operation and does not grow with the workspace. **Recommendation for nodes that run microVMs: put the Drive9 root on a reflink-capable filesystem.** `cargo test -p vam-storage --test turn_commit_cost -- --ignored --nocapture` prints the method and the cost on the machine it runs on.

**Lazy extraction.** The copy is not read into a manifest when it is taken. The commit's manifest hash is the token `image:<round>`, and the commit completes when the boundary arrives, as for process guests. A stop (and a park or an end that needs it) reads the newest complete copy with the safe image reader and stores the real manifest hash back. Reading costs time proportional to the workspace, so it is paid once, at the stop, not on every round. If that copy cannot be read, the commit is marked `failed`, `vam.v1.workspace.commit_failed` is raised, and the stop falls back to the previous complete commit (to the run's start if there is none). An I/O error or a limit hit while reading fails the settle, and the whole session tree is kept.

Limits: one commit per round, at most 256 per run, never more than the model requests the host recorded, and none once the session is stopping or sealed. A commit that cannot be stored fails with `storage_commit_failed` and the host emits `vam.v1.workspace.commit_failed` (see [Failed commits](#4-failed-commits-and-kept)).

---

## 2. The stop rule

Work is never discarded. A stop rolls **files and conversation** back to the newest complete turn commit of the stopped session:

1. The VM is torn down (VM only), so nothing writes while the host settles.
2. The conversation seals: the host writes "Not run" results for tool calls that never finished, a `rollback` record **only when** a file-writing result finished after the last complete commit, and `task_end` with outcome `stopped`.
3. One settle sets the workspace HEAD to the last complete commit's files plus the host `.vam/`. On the image backend the files come from the commit's copy (see [1a](#1a-how-a-microvm-takes-a-turn-commit)); the host's `.vam/` is never in the guest image (it is set aside while the image is built), so it is composed from the session tree. A stop does not refresh the golden template: the live image holds the work that was rolled back.
4. The run ends `FAILED` with reason `stopped`. The stop endpoint waits up to 60 s, else answers `504 stop_pending`.

A new workspace stopped **before any turn commit** keeps only its conversation: there is no commit to roll files back to, so HEAD holds no files from that task (what the task wrote may be copied to `kept/`, below).

The stop logs where its time went: `stop settled: wait_ms=… prepare_end_ms=… resolve_ms=… scan_ms=…` (`resolve_ms` is the read of the copy, `scan_ms` the best-effort scan of the live image for the rolled-back list; a scan that fails logs and yields an empty list, it never blocks the rollback).

### Rolled-back files

Files the stopped task wrote after the last complete commit are not in HEAD. They are copied to `kept/<id>/rolled-back/` (under the node's `drive9/kept` directory), up to a **64 MiB** cap per stop; files over the cap are skipped and counted. They are for a person to recover, not part of the agent's workspace, and the guest never sees them. The session tree is removed only after HEAD reads back; sweeps that park a paused or warm session never reclaim a tree.

### `vamd` restart

On startup `vamd` settles every session tree it finds without a live run (`settle_dead_trees`). A microVM session's VMM may still be dying: the settle first waits up to about 5 s for the recorded process to be gone (while its start time still matches), then reads the surviving turn copies. A run that was still running ends `FAILED` with reason `vamd_restarted`, the log gets `task_end` with outcome `interrupted`, and the workspace rolls back to the last complete turn commit. Other trees commit as they are. A tree whose commit fails is kept, not removed.

---

## 3. Workspace HEAD before and after

A run ends in two phases (`prepare_end`, then `finish_end`). The `commit` record in the thread log and `finalOutputs.workspace` carry the workspace HEAD before and after the run and a `files_hash`, so a reader can tell that the files and the conversation belong together. The terminal run state is published only after the settle.

---

## 4. Failed commits and `kept/`

If the end-of-task commit or a turn commit cannot be written, the host:

* keeps the session tree under `kept/<id>` and records a `workspace_kept` row;
* emits `vam.v1.workspace.commit_failed` with the kept id and an error that names no host path;
* ends the run with `storage_commit_failed`.

The work stays on the node until a person recovers or deletes it. Nothing is discarded to make room.

---

## 5. Log size limit

The thread log and the prompt store count against one quota (`LogQuota`):

| Scope | Limit |
|---|---|
| One thread | 256 MiB |
| One workspace (all thread logs and the prompt store) | 1 GiB |
| Reserve for control records | 1 MiB |

The reserve is kept so a full log can still record a stop, a rollback or a `task_end`; message records and prompts cannot use it. When a limit is hit the broker refuses the request with `thread_log_quota_exceeded` before the provider is called. Three failed log writes in a row refuse with `thread_log_unavailable` and the run ends `storage_commit_failed`.

This is separate from the **context guard**: the live context assembled for one request is refused above 16 MiB (`MAX_CONTEXT_BYTES`, error `context_too_large`). Compaction normally keeps a thread far below it (see [Context](/runtime/agent/)).

---

## 6. Thread log v1

```text
.vam/threads/<thread>/log/<first_seq:08>.jsonl
```

* **Segments.** Named by the first `seq` they hold; a segment is sealed once a later one exists and is never rewritten. Rotation at 4 MiB; a record larger than that gets a segment of its own. A record above 16 MiB is not written; a `gap` record takes its place.
* **Lines.** One JSON object per line, newline-terminated. A line is committed only when its newline is on disk. Readers skip a torn last line, unknown kinds and unknown fields inside `v: 1`, and refuse `v: 2`. The writer repairs a torn tail before the next append.
* **Header.** Every record has `v, seq, id, prev, ts, run, lane, agent, parent_lane, round, src, kind, body_sha256` and a `body`. `prev` is the digest of the previous record's header. The chain covers the canonical header, which includes `body_sha256`, not the raw line, so a body can later be removed (`redact`) without breaking the chain. The thread read API reports `integrity` (`ok` or `broken_at:<seq>`) and serves records up to a break.
* **Lanes.** `main` is the conversation. `summary` holds the exchange that produced a compaction summary; it is never part of the model's context. A lane may name a `parent_lane`.
* **Kinds.**

| Kind | Meaning |
|---|---|
| `task_start`, `task_end` | A run's range in the log. `task_end` carries the outcome (for example `stopped`, `interrupted`). |
| `message` | One chat message as the model saw it. |
| `prompt_change` | The system prompt or tool definitions changed (digests). |
| `suspend` | The task parked for an approval. |
| `compaction` | Summary message, `keep_from`, tokens before and after, reason. |
| `rollback` | Records between `to` and this record are no longer part of the conversation. |
| `commit` | Workspace HEAD before and after, `files_hash`. |
| `fork`, `redact`, `gap` | Branching, body removal, an unwritten oversized record. |

* **Typed parts.** A message's content is a list of parts: `text`, `image` (a `ref`), `tool_use` (id, name, the model's `arguments` string byte-exact), `tool_result` (`tool_use_id` and nested parts), `reasoning` (text and signature) and `opaque` (provider-specific items kept verbatim). A reader skips a part type it does not know. Assistant records also carry the usage the host measured (`prompt`, `completion`, `cached`, `cache_write`, and flags `cached_unknown` and `prefix_stable`).
* **Prompt store.** System prompts and tool definitions are not written into the log. The log keeps SHA-256 digests (`system_sha256`, `tools_sha256` on `task_start` and `prompt_change`); the bytes live in `<drive9_root>/workspaces/<id>/prompts/`, outside the workspace tree and manifest, so they are never versioned, listed by a files API or reachable by the guest. Each blob is re-hashed when read.

### What each view shows

`GET /v1/workspaces/:id/threads/:thread/messages` (see [Host Control API](/orchestrator/api/)):

* `view=all` returns every record of every lane, including rolled-back records and records older than a compaction. It is the audit trail.
* `view=context` returns what the model sees next: the latest `compaction`'s summary, then the kept tail from `keep_from`, then later messages, on the `main` lane. Records strictly between a `rollback.to` and that rollback are left out, and an earlier summary is replaced by the newer one. A rollback never reaches below the latest compaction, so older messages are not brought back.

The VAM app chat history is read with `view=all`, hiding rollbacks the same way, so a finished run renders from the log.
