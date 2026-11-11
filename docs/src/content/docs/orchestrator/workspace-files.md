---
title: Workspace Files API
description: Owner-scoped HTTP list and download of committed Drive9 workspace HEAD files.
status: shipped
proves:
  - crates/vam-orchestrator/tests/workspace_files_tests.rs
---

The workspace files API exposes **committed Drive9 HEAD** content under the guest `workspace/` mount. It does not read live VM disks, mid-run writes, or HSP `FileRef` signed URLs.

## Routes

| Method | Path | Auth |
|---|---|---|
| GET | `/v1/workspaces/:id/files` | Bearer with `sessions:write` |
| GET | `/v1/workspaces/:id/files/*path` | Bearer with `sessions:write` |

`:id` is the **guest workspace suffix** (for example `assistant`). The host applies `{owner_user_id}--` the same way as sessions and runs; callers cannot browse another user's volume by embedding a foreign prefix in `:id`.

## List response

JSON uses camelCase:

```json
{
  "workspaceId": "assistant",
  "head": "<manifest-hash>",
  "files": [{ "path": "workspace/PLAN.md", "sizeBytes": 120, "mode": 420 }],
  "truncated": false
}
```

Paths are manifest-relative and always start with `workspace/`. Only committed HEAD is visible; empty or missing HEAD returns **404**.

## Download

- Path must normalize to `workspace` or `workspace/...` after component normalization.
- Response is raw bytes: `Content-Type: application/octet-stream` (never extension sniffing).
- `Content-Disposition: attachment; filename="<basename>"` uses the final path segment only (sanitized).
- Manifest `size_bytes` over **32 MiB** returns **413** before chunk assembly.
- Jail violations (traversal, paths outside `workspace/`) return **400**.
- Missing, denied, or non-HEAD files return **404** (no separate forbidden signal for deny-listed paths).

## CLI

List and download via `vam` (same auth as sessions: Bearer with `sessions:write`):

```bash
# List committed HEAD files (default workspace: assistant)
vam workspace ls
vam workspace ls assistant

# Machine-readable list (API JSON body)
vam workspace ls --json

# Download (shorthand or full workspace/ path)
vam workspace cat notes/a.md
vam workspace cat workspace/notes/a.md -o ./a.md

# With --json, -o is required; stdout is metadata only
vam workspace cat notes/a.md -o ./a.md --json
```

Human `ls` prints `path` and size per line; if the list is truncated it warns on stderr. `cat` writes raw bytes to stdout or `-o`. Empty, absolute, and `..` paths are rejected with exit `2` (usage).

## Filtering and deny-list

HEAD manifests include the full materialized tree (`skills/`, `tools/`, etc.). Listing and download **hard-filter** to `workspace/` only.

These paths are omitted from lists and treated as **404** on download:

- `workspace/.vam/threads/` (prefix)
- `workspace/.vam/SYSTEM.md`
- `workspace/.vam/workspace.json`

`workspace/.vam/host-artifacts/` and `workspace/.vam/agent/` remain readable in MVP.

## Limits

| Limit | Value |
|---|---|
| Max download size | 32 MiB |
| Max listed files | 10,000 (`truncated: true` when capped) |

## Deferred (out of scope)

- The VAM app Files panel
- Live mid-run reads, Range/streaming
- Owner upload into HEAD: [Workspace Files Upload](/orchestrator/workspace-files-upload/)
- Admin cross-tenant browse
- Per-download audit events and dedicated rate limits
