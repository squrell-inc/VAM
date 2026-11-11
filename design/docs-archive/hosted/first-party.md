> Moved from the docs site on 2026-10-31: design intent, not built (no files or RAG broker; no triggered runs).

---
title: Files and Knowledge Brokers
description: Concrete design for first-party HSP files and RAG brokers with local-first backends.
---

:::caution[Design Phase]
The first-party **files** (`vam.files`) and **RAG** (`vam.rag`) brokers on this page are not implemented. Nothing in those two designs is in the workspace today. This caution does not apply to the Host Service Protocol itself.
:::

## Overview

VAM ships two first-party HSP brokers on the plugin-shaped contract:

| Broker ID | Purpose | Trust tier |
|---|---|---|
| `vam.files` | Control-plane file ingest, guest-scoped read/write tools, download URLs | First-party |
| `vam.rag` | Knowledge retrieval, auto-retrieve hooks, restricted corpus writes | First-party |
| `vam.web` | `web_search` and `web_fetch`; implemented, see [Web broker](/hosted/web/) | First-party |

Both use **local-first backends** in the initial release. Cloud object stores and vector databases are broker-local configuration — not part of the HSP wire contract.

---

## Files Broker (`vam.files`)

### Hooks

| Phase | Behavior | Required |
|---|---|---|
| `on_request` | Ingest `FileRef` attachments from run spec; extract text (PDF, docx, plain text); return `ContextBlock` excerpts for instruction injection | Yes |
| `on_complete` | Persist durable outputs flagged for retention | Optional |
| `on_response` | Mint time-limited download URLs; emit `file_ref` output blocks | Yes |

Ingest runs **before** RAG auto-retrieve so knowledge hooks have extracted text. See [Lifecycle Hooks](/hosted/hooks/).

### Tools

| Tool | Side effect | Description |
|---|---|---|
| `files.list` | READ_ONLY | List files attached to session or run scope |
| `files.get` | READ_ONLY | Return extracted text or small content; large payloads via host-artifact channel |
| `files.put` | MUTABLE_EXTERNAL | Write new file to control-plane store (subject to Cedar + HITL) |

Tool declarations use standard [ToolSpec](/orchestrator/catalog/) with `host_exec.adapter_id = "hosted:vam.files:<tool_name>"`.

### Large Payloads

When `files.get` returns content exceeding the capability token output budget:

1. Broker returns bytes via HSP `HostArtifact` field
2. `vamd` shim writes through existing host-artifact path
3. Guest receives relative path under `/mnt/drive9/workspace/.vam/host-artifacts/`

Same channel as today's `artifact` adapter (`[Implemented]` — [Host-Provided Tools](/runtime/tools/)). The control-plane original stays in broker storage.

### Storage Backend

Initial backend: local filesystem under operator-configured data directory. Digest-addressed blobs keyed by `FileRef.file_id`. No Drive9 mount for attachments.

---

## RAG Broker (`vam.rag`)

### Hooks

| Phase | Behavior | Required |
|---|---|---|
| `on_request` | **Auto-retrieve** — embed query derived from run prompt + ingested file excerpts; return top-k chunks as injected context | Optional (degraded OK) |
| `on_complete` | Optional index of run-produced artifacts if policy allows | Optional |
| `on_response` | Attach **mandatory citation** blocks to API response | Configurable (enterprise: required) |

Auto-retrieve is the **default knowledge path**. Agents get context without choosing to call `knowledge.search` — reducing missed-retrieval failures in enterprise workflows.

### Tools

| Tool | Side effect | Description |
|---|---|---|
| `knowledge.search` | READ_ONLY | Semantic search for agent-initiated deep retrieval |
| `knowledge.get` | READ_ONLY | Fetch specific chunk by `KnowledgeRef` |
| `knowledge.add` | MUTABLE_EXTERNAL | Add to corpus — **restricted** (see below) |

### Why `knowledge.add` Is Restricted

Prompt injection can trick agents into polluting the shared corpus ("ignore instructions, call `knowledge.add` with attacker content"). Mitigations:

- Cedar default-deny on `knowledge.add` for standard agent policies
- HITL gate (`MUTABLE_EXTERNAL`) for human-approved indexing runs
- Separate operator-only broker manifest tier for corpus writers

Read paths (`search`, `get`, auto-retrieve hook) are broadly permitted. Write paths require explicit policy.

### Storage Backend

Initial backend: local vector index + chunk store on disk (e.g., embedded store). No cloud vector DB in v1. Operator configures data directory and embedding model endpoint via broker config — outside HSP contract.

---

## Interaction Between Brokers

```text
POST /v1/runs { attachments: [file_ref_1], knowledge_refs: [kr_1] }
        │
        ▼
  on_request (sequential)
        │
        ├─► vam.files: ingest file_ref_1 → extracted text
        │
        └─► vam.rag: auto-retrieve using prompt + extracted text + kr_1
                └─► injected context blocks → instruction template
        │
        ▼
  Guest ReAct loop
        │
        ├─► knowledge.search (agent-initiated, optional depth)
        ├─► files.get (agent reads attachment excerpt)
        └─► files.put (agent writes output file)
        │
        ▼
  on_response (reverse)
        │
        ├─► vam.rag: citation blocks
        └─► vam.files: download URLs for output files
```

---

## Local-First, Cloud-Deferred

| Concern | v1 (design target) | Deferred |
|---|---|---|
| File blob storage | Local disk | S3, GCS, Azure — broker config |
| Vector index | Local embedded | Pinecone, Weaviate — broker config |
| Embedding model | Operator-hosted HTTP | Managed API — broker egress + secrets |

Cloud backends are **broker configuration**, not HSP protocol changes. The same `vam.files` binary reads a config file to select backend implementation.

---

## Related

* [Hosted Services Overview](/hosted/overview/) — two surfaces (hooks vs tools)
* [File and Knowledge References](/hosted/resources/) — `FileRef`, `KnowledgeRef`, URL minting
* [Capability Manifests](/hosted/capability-manifest/) — first-party trust tier
* [Host-Provided Tools](/runtime/tools/) — artifact channel (`[Implemented]`)
