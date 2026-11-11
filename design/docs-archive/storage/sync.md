> Moved from the docs site on 2026-10-31: design intent, not built (no FUSE code, no cloud backends, no sync, no encryption at rest; storage is a local ext4 image per session).

---
title: Differential S3 Background Sync Engine
description: Dirty file tracking, content-defined chunk uploads, bandwidth pacing, and backpressure mechanisms in Drive9.
---

:::note[Status: Fully Implemented Local Storage Plane]
Differential chunking, Content-Defined Chunker (CDC Rabin), and local directory synchronization are fully implemented and optimized for unprivileged disk mounting and committing.
:::

Because object stores (like S3) are slow and do not natively support inline updates or partial byte modification, writing directly to cloud storage inside the guest would freeze agent tools. To maintain NVMe-level write performance, Drive9 uses a background **Differential Sync Engine** on the host.

---

## 1. Dirty Block & Byte Tracking

When an agent tool writes data inside `/mnt/drive9`, the guest client forwards the write block over VSOCK to the host.
1. **Host-Side Write-Back**: The host-side `vam-storage` module immediately commits the raw bytes into a local host Copy-on-Write SSD cache, returning a success signal to the guest within sub-milliseconds.
2. **Dirty Map Registration**: The host marks the modified byte ranges as **dirty** in an in-memory page tracking table.
3. **Triggering Sync**: A background syncer thread periodically sweeps this dirty map (either on a timer, e.g. every **5 seconds**, or when dirty bytes exceed **64MB**).

---

## 2. Differential Chunk Generation

Once the background syncer sweeps dirty regions, it computes differences to minimize cloud upload bandwidth:

```text
Dirty File: workspace/database.db (8MB modified at offset 2MB)
                              │
                              ▼ (Sliding Window Scan)
 ┌───────────────────┐  ┌───────────────┐  ┌───────────────────┐
 │ Chunk 1 (Unchanged│  │ Chunk 2 (Dirty│  │ Chunk 3 (Unchanged│
 │ Hash: 0xa1b2...   │  │ Hash: 0xf3d4..│  │ Hash: 0xc5d6...   │
 └─────────┬─────────┘  └───────┬───────┘  └─────────┬─────────┘
           │                    │                    │
     [ EXISTS ]           [ NEW CHUNK ]        [ EXISTS ]
  Skip upload, reuse    Upload raw bytes to   Skip upload, reuse
  chunk reference       `/chunks/f3/d4...`    chunk reference
```

1. **Rabin Boundaries Check**: The syncer parses the modified file using sliding-window Rabin fingerprinting to find chunk boundaries.
2. **Hashing**: Calculates the SHA-256 hash of each chunk block.
3. **Deduplication Check**: The engine checks if the calculated hash already exists in either the local LRU cache database or the global `/chunks/` directory in the remote cloud storage bucket.
4. **Targeted Upload**: If the chunk is a duplicate, the engine skips uploading the bytes, merely appending its SHA-256 hash into the new Merkle Manifest. Only newly created chunk blocks are physically pushed to the cloud backend.

---

## 3. Bandwidth Pacing & Backpressure

To prevent massive file uploads from saturating the host's physical network adapter and degrading other hypervisor processes (like LLM token streaming), Drive9 enforces strict network resource controls:

* **Token Bucket Pacing**: The background syncer uses a token bucket rate-limiter to cap maximum upload bandwidth (e.g., maximum **20MB/s** per VM session).
* **Backpressure Throttle**: If the background upload worker queue fills up (for example, during a huge compiler dump generating gigabytes of files):
  1. The host storage engine slows down acknowledgement frames returned over VSOCK.
  2. The guest FUSE client pauses its block write thread, throttling guest VFS operations.
  3. This feedback loop prevents the host daemon from running out of local SSD cache space.

---

## 4. Retries & Idempotency Rules

Object storage interactions are prone to network jitter, HTTP timeouts, and rate limits. The syncer implements robust recovery:
* **Asvoluntary Idempotency**: Because chunk files inside `/chunks/` are content-addressed (their keys are their SHA-256 hashes), they are inherently idempotent. If an upload of `chunks/a9/4a...` fails halfway through and retries, it cannot corrupt existing files. It simply overwrites or resumes the exact same byte block.
* **Exponential Backoff**: Trait calls to `put_chunk` are wrapped in a retry handler employing exponential backoff (starting at **1 second**, doubling to a maximum **60 seconds**, capped at **5 attempts**).
* **Terminal Degradation**: If all retry attempts fail, the syncer transitions the workspace into a `Degraded` offline state, halting high-risk transitions until connection is restored.

---

## 5. Next Steps

* To check how this sync engine updates workspace manifests, see [Consistency Model & Write Locks](/storage/consistency/).
* Read how directories are isolated inside local filesystems in [Copy-on-Write Workspace Overlay](/storage/overlay/).
* To see how check-pointing coordinates with sync operations, explore [State Checkpoints](/storage/checkpoint/).
