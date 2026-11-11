# VAM Storage & Drive9 Development Invariants

This file details the strict coding and operational rules that every agent or developer must adhere to when working inside `crates/vam-storage/`. For detailed design rationales, refer to [Storage Invariants](/storage/invariants/) in the VAM documentation site.

---

## 1. Cloud-Agnostic & Provider Neutrality
* **Never depend on provider-specific properties**. All metadata, manifests, and sync loops must map strictly to the abstract interfaces defined in `ObjectStoreBackend`. Do not utilize provider-specific headers, object tags, or multipart ETag strings in the core filesystem.
* **Maintain portable object layouts**. Store content-addressed file chunks under global `/chunks/` paths, and workspace-specific manifests under `/workspaces/<workspace-id>/manifests/`.

## 2. Host-Guest Boundaries & Security
* **Never inject cloud storage credentials into the guest VM**. Keep all access keys, secret tokens, and managed identity configurations securely on the trusted host side. Guest VFS read/write operations must be routed entirely over VSOCK port 52 storage control lines.
* **Always calculate SHA-256 chunk hashes on the host**. Do not trust or accept hashes declared or specified by the guest FUSE client. This prevents cache poisoning attacks from corrupted guest files.

## 3. Atomic Commits & Durability Barriers
* **Enforce single-writer leases**. Limit write operations to exactly one active lease per workspace, verified via a short-TTL lock file in cloud storage.
* **Verify checkpoint barriers on HITL pauses**. Before acknowledging a guest HITL pause request or signing a `HitlToken`, flush all dirty chunks to the remote cloud store and commit the workspace HEAD manifest to a `CHECKPOINTED` state.

## 4. Cache & Garbage Collection Safety
* **Always write-verify chunks before cache admission**. Recalculate SHA-256 hashes of chunks upon local SSD cache admission and eviction to instantly catch bit-rot or local corruption.
* **Apply GC safety windows to unreferenced chunks**. Chunks flagged for deletion inside `/chunks/` must first be tagged with a tombstone, and must remain completely unreferenced for a minimum **GC Safety Window of 24 hours** before physical removal.

## 5. Coding Standards & Error Handling
* **Zero tolerance for panics**. Report all errors using `thiserror` mapping to the spec's `VamErrorCode` payload. Never use `.unwrap()`, `.expect()`, or raw `panic!` blocks in storage libraries.
* **Default to safe, degraded offline states**. During temporary cloud outages, serve existing reads from the local LRU disk cache and buffer write blocks inside host SSD CoW files. Block and queue high-consequence state transitions (like pauses) until connection is restored.
