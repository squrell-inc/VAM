---
title: Workspace Files Upload
description: Owner upload of one file into committed Drive9 HEAD under workspace/, for the next session only.
status: shipped
proves:
  - crates/vam-orchestrator/tests/workspace_files_tests.rs
---

Owner upload writes **one path** into committed Drive9 HEAD under the guest `workspace/` mount. The next session materializes that HEAD. Upload does not change a live guest disk, does not replace guest `write_file` during a run, and is not an HSP `FileRef` attachment.

List and download stay on [Workspace Files API](/orchestrator/workspace-files/). This page is the write contract.

## Route

| Method | Path | Auth |
|---|---|---|
| PUT | `/v1/workspaces/:id/files/*path` | Bearer with `sessions:write` (`PutWorkspaceFile`) |

`:id` is the **guest workspace suffix** (for example `assistant`). The host resolves `{owner_user_id}--{suffix}` with `resolve_session_workspace_id`, the same way as list and download. Callers cannot write another user's volume by embedding a foreign prefix in `:id`.

## Request

Upload accepts **any file type** (PNG, DOCX, PDF, text, etc.) as opaque bytes. The manifest path comes from the URL (`*path`), not from JSON `name`.

### Raw bytes (CLI default)

- `Content-Type: application/octet-stream`
- Body is the file bytes directly

### JSON + base64 (Postman-friendly)

- `Content-Type: application/json`
- Body (camelCase or snake_case for `media_type`):

```json
{
  "name": "pixel.png",
  "mediaType": "image/png",
  "base64": "<standard-base64>"
}
```

`name` and `mediaType` are optional metadata; only `base64` is required. The host decodes base64 and stores the bytes. Decoded size must be ≤ **32 MiB** (`413` if larger). Invalid JSON or base64 returns **400**.

- `*path` must normalize to a **file** under the workspace jail: `workspace/...` after component normalization. Shorthand paths are a CLI concern; the HTTP path is the manifest-relative path.
- The directory path `workspace` alone is rejected (**400**). Empty paths, absolute paths, and `..` are rejected (**400**).
- Bodies over **32 MiB** return **413** before chunking (`Drive9Error::FileTooLarge`). The cap matches download (`MAX_DOWNLOAD_BYTES`).

## Response

Success is **200** with camelCase JSON:

```json
{
  "workspaceId": "assistant",
  "path": "workspace/PLAN.md",
  "head": "<new-manifest-hash>",
  "sizeBytes": 120
}
```

`workspaceId` is the guest suffix, not the host-prefixed volume id. `head` is the manifest hash just committed. `path` is the normalized manifest path.

## HEAD merge

The host stores the body as content-addressed chunks, upserts that single path into a new manifest, then `write_head`.

- If HEAD exists, the new manifest copies every other file and replaces the matching path. `parent_manifest_hash` is the previous HEAD.
- If HEAD is missing, the new manifest contains only the uploaded file (`parent_manifest_hash` is null) and becomes HEAD. Read still returns **404** when HEAD is missing; upload is how an owner seeds that first snapshot.
- New entries use mode `0644` (`0o644`).

A later session materializes this HEAD into the guest. The running microVM disk is not updated.

## Idle workspace

Upload is allowed only when no non-terminal session holds the workspace. `find_active_session_on_workspace` treats `Terminated` and `Failed` as terminal, the same predicate as run create.

A live session returns **409** (`OrchestratorError::AgentRunning`, `FailedPrecondition`). There is no `423`.

## Jail and write deny

The path jail matches read: only `workspace/...`.

Write deny is stricter than the read deny-list. Upload refuses **every** path under `workspace/.vam/`, including paths read still serves (`workspace/.vam/host-artifacts/`, `workspace/.vam/agent/`). `workspace/.vam` itself is refused. Those refusals are **400** (jail / layout), not the read API's **404** hide.

Read continues to omit only:

- `workspace/.vam/threads/`
- `workspace/.vam/SYSTEM.md`
- `workspace/.vam/workspace.json`

## What this is not

| Path | Role |
|---|---|
| This PUT | Owner writes committed HEAD. Visible on the **next** session materialize. |
| Guest `write_file` | Agent writes the live guest disk during a run. |
| FileRef / HSP attachments | Separate attachment path. Not a workspace HEAD upsert. |

## CLI

Operators use the same auth and path rules as `vam workspace ls` / `cat`:

```bash
vam workspace put notes/a.md ./a.md
vam workspace put workspace/notes/a.md ./a.md -w assistant
```

`<remote-path>` may be shorthand (`notes/a.md`) or `workspace/...`. Default workspace is `assistant`. Busy (**409**), oversize (**413**), and jail (**400**) errors surface as command failures. Local files over 32 MiB are rejected before the request.

## Out of scope

- The VAM app Files panel upload
- Changing guest `write_file` or FileRef / HSP attachments
- Admin cross-tenant write
- Upload of more than one path per request
