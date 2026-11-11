> Moved from the docs site on 2026-10-31: design intent, not built (no FUSE code, no cloud backends, no sync, no encryption at rest; storage is a local ext4 image per session).

---
title: FUSE Frontend Driver
description: The POSIX-compliant user-space filesystem driver inside the guest microVM.
---

:::note[Status: Fully Implemented Local Storage Plane]
The VSOCK FS frames concurrent protocol client and server are fully designed and implemented. Today, because the guest kernel lacks native FUSE support, VAM mounts a host-compiled loopback ext4 disk image to `/mnt/drive9`. The FUSE driver acts as a forward-compatibleuserspace client waiting for the FUSE-enabled kernel.
:::

To provide a seamless experience for autonomous AI agents, VAM requires that `/mnt/drive9` exposes a standard, POSIX-compliant local filesystem interface. Rather than rewriting agent tool libraries to speak cloud APIs, VAM runs a high-performance, user-space FUSE (Filesystem in Userspace) driver inside the guest.

---

## 1. Supported POSIX Operations

The FUSE client linked into the guest `vam-runtime` implements the standard Linux FUSE kernel API. It translates virtual file system (VFS) operations into asynchronous VSOCK packets.

The table below lists the primary POSIX operations supported inside the guest:

| POSIX Operation | Guest FUSE Role | Host Coordination |
|---|---|---|
| `open` / `release` | Allocates local file-handles and tracks references. | Verifies file existence and locks path references. |
| `read` | Satisfies reads from in-VM page cache or vsock block request. | Fetches block from host LRU or object store. |
| `write` | Writes data to in-VM buffers and queues vsock send. | Records written bytes to CoW overlay and marks dirty. |
| `mkdir` / `rmdir` | Modifies active guest metadata tree. | Writes directory entry metadata blocks. |
| `unlink` / `rename` | Detaches directory node or updates path index. | Updates active manifest layout indices. |
| `fsync` | Blocks guest calling thread until write queue clears. | Flushes local dirty blocks to host storage media. |

---

## 2. Directory Metadata Cache

Making a vsock roundtrip to the host for every single file metadata request (e.g., checking if a file exists, fetching file sizes) causes unacceptable latency during heavy operations like compiling code or resolving nested dependencies.

To prevent this performance bottleneck, VAM's guest FUSE driver implements a strict **metadata caching policy**:
* **Negative Caching**: Caches file non-existence results (for 5 seconds) to speed up compiler checks searching for header files.
* **Positive Caching**: Caches file attributes (like sizes, ownership, permission bits) inside guest memory.
* **Coordinated Invalidation**: When the host modifies a file (e.g., pulling a new skill or synchronizing workspace updates), the host-side storage daemon sends a metadata invalidation frame over VSOCK, forcing the guest FUSE driver to immediately purge its metadata cache for affected paths.

---

## 3. Unsupported POSIX Edge Cases

Because Drive9 synchronizes to cloud object storage backends (which are inherently non-POSIX systems), certain advanced Linux filesystem operations are unsupported or behave differently:

:::danger[Filesystem Limitations]
1. **Hard Links**: Standard hard links (`link(2)`) are **unsupported** across different directories because cloud object store buckets do not have a concept of shared inode references. Symbolic links (`symlink(2)`) are fully supported.
2. **Standard Advisory Locking**: Operations like `flock(2)` or `fcntl(2)` locks are local to the guest microVM. They prevent concurrent processes *inside the same VM* from modifying a file but **do not** block writers on sibling host nodes or other cloned VM instances.
3. **mmap Write-Back**: Files mapped with write access (`mmap(2)` with `MAP_SHARED`) are not continuously synchronized to VSOCK in real time. The guest FUSE driver only captures updates when the mapping is flushed via `msync(2)` or when the file handle is formally closed.
:::

---

## 4. Next Steps

* To understand how the guest FUSE client translates calls to host-side storage, see [Storage Architecture](/storage/architecture/).
* To see how host-side storage interfaces with multi-cloud targets, read [ObjectStoreBackend Trait](/storage/backend-interface/).
* Explore how workspace mutations are tracked and written to persistent backends in [Differential S3 Background Sync Engine](/storage/sync/).
