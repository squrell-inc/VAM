> Moved from the docs site on 2026-10-31: design intent, not built (no files or RAG broker; no triggered runs).

---
title: File and Knowledge References
description: FileRef and KnowledgeRef URN scheme, Drive9 boundary, structured output blocks, and signed URL rules.
---

:::caution[Runtime not implemented]
`FileRef` and `KnowledgeRef` are committed in `proto/vam/v1/common.proto`. HSP broker runtime, file APIs, and signed URL minting are **not implemented** yet.
:::

## Reference Types

HSP uses two control-plane reference types for attachments and knowledge corpus entries. They complement — but do not replace — the existing `ArtifactRef` in [common.proto](/spec/overview/).

### FileRef

Opaque identity for a control-plane file attachment (committed in `common.proto`):

```protobuf
message FileRef {
  ResourceId file_id = 1;     // urn:vam:file:<registry>/<namespace>/<name>
  Digest digest = 2;          // Content-addressed integrity
  string media_type = 3;
  int64 size_bytes = 4;
  ResourceMetadata metadata = 5;
}
```

### KnowledgeRef

Pointer to a knowledge corpus entry (chunk, document, or collection):

```protobuf
message KnowledgeRef {
  ResourceId knowledge_id = 1;  // urn:vam:knowledge:<registry>/<namespace>/<name>
  Digest digest = 2;            // Optional chunk digest
  string collection = 3;
  ResourceMetadata metadata = 4;
}
```

---

## URN Scheme

Both types use the standard three-segment `ResourceId` path enforced by semantic validation:

| Type | Pattern | Example |
|---|---|---|
| File | `urn:vam:file:<registry>/<namespace>/<name>` | `urn:vam:file:hub.vam.dev/acme/quarterly-report` |
| Knowledge | `urn:vam:knowledge:<registry>/<namespace>/<name>` | `urn:vam:knowledge:hub.vam.dev/acme/handbook-getting-started` |

Registry, namespace, and name segments must be lowercase slugs (hyphens and dots allowed). Two-segment paths such as `urn:vam:file:acme/<uuid>` **fail** `Validate for ResourceId`.

---

## Relationship to ArtifactRef

Today's `ArtifactRef` (committed in `proto/vam/v1/common.proto`):

```protobuf
message ArtifactRef {
  ResourceId artifact_id = 1;
  Digest digest = 2;
  string media_type = 3;
  int64 size_bytes = 4;
  string uri = 5;   // opaque retrieval hint, not an authority
}
```

| Message | Role | Trust boundary |
|---|---|---|
| `FileRef` / `KnowledgeRef` | Control-plane resource identity | API requests, hook inputs, run spec |
| `ArtifactRef` | Run output, checkpoint, evidence | Workflow status, evals, HITL |

`FileRef` may **convert to** `ArtifactRef` at run completion when a file becomes durable output. The `uri` field on `ArtifactRef` remains an opaque hint — for HSP, `on_response` mints a **separate** signed download URL for the API client, not stored in `ArtifactRef.uri` passed to the guest.

---

## Drive9 vs Control-Plane Attachments

| Store | Scope | Mutability | Guest access |
|---|---|---|---|
| **Drive9 workspace** | Agent scratchpad (`/mnt/drive9/workspace/`) | Guest read/write via Cedar-gated tools | Yes (normalized paths) |
| **Control-plane attachments** | `POST /v1/files`, run attachment refs | Host/broker managed | No direct access |

**Boundary rule:** User-uploaded files and knowledge corpus live in **broker backends** (local disk, operator-configured object store). Drive9 is the agent's working directory — not the attachment store.

Large tool payloads use the existing [host-artifact channel](/runtime/tools/) (`/mnt/drive9/workspace/.vam/host-artifacts/`) for guest-visible scratch copies. Control-plane originals stay host-side.

---

## Structured Output Blocks

HSP responses and API payloads use typed blocks instead of stuffing everything into a single string (defined in `hosted_services.proto`):

```protobuf
message OutputBlock {
  oneof block {
    TextBlock text = 1;
    FileRefBlock file_ref = 2;
  }
}

message TextBlock {
  string content = 1;
  repeated Citation citations = 2;
}

message FileRefBlock {
  FileRef file = 1;
  string download_hint = 2;   // Client-only; NOT passed to guest
}
```

| Block | Guest sees | API client sees |
|---|---|---|
| `text` | Truncated/redacted excerpt | Full text + citations |
| `file_ref` | Relative path or ref id only | `download_hint` (signed URL) |

This mirrors today's rule: host artifact chat shows **path not body** (`[Implemented]` in [Host-Provided Tools](/runtime/tools/)).

---

## on_response URL Minting

Download URLs are minted exclusively in the `on_response` hook:

```text
1. Agent completes run; output artifacts collected
2. files broker on_response: create signed, time-limited URL
3. vamd attaches URL to FileRefBlock.download_hint in HTTP response
4. Guest never receives the signed URL
```

Properties:

- **Time-limited** — default TTL configured per operator policy
- **Scoped** — bound to file digest and tenant
- **Non-embeddable in guest context** — prevents exfiltration via prompt or tool args

If the agent needs to reference a file during the run, it uses `files.get` (returns extracted text or a workspace-relative artifact path) — not a signed URL.

---

## API Flow

```text
Client                          vamd                         files broker
  │                               │                               │
  │ POST /v1/files (upload)       │                               │
  ├──────────────────────────────►│ HSP (store)                   │
  │                               ├──────────────────────────────►│
  │◄──────────────────────────────┤ FileRef                       │
  │                               │                               │
  │ POST /v1/runs { attachments } │                               │
  ├──────────────────────────────►│ on_request: ingest/extract    │
  │                               ├──────────────────────────────►│
  │                               │                               │
  │         ... agent run ...     │                               │
  │                               │                               │
  │ GET /v1/runs/{id}             │ on_response: mint URLs        │
  ├──────────────────────────────►├──────────────────────────────►│
  │◄──────────────────────────────┤ response_blocks + signed URLs  │
```

---

## Related

* [Lifecycle Hooks](/hosted/hooks/) — when `on_response` fires
* [First-Party Brokers](/hosted/first-party/) — files broker concrete design
* [Drive9 Storage](/architecture/drive9/) — workspace vs attachment boundary
* [Common Primitives](/spec/overview/) — `ArtifactRef`, `ResourceId`, `Digest`
