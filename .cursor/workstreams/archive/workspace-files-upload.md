# Workstream: Workspace Files Upload

```yaml
status: done
slug: workspace-files-upload
current_phase: done
design_docs:
  - docs/src/content/docs/orchestrator/workspace-files-upload.md
```

## Goal

Owner upload of one file into committed Drive9 HEAD under `workspace/`, so the next session materializes it. Design, storage helper, vamd PUT with an idle-workspace guard, and CLI. No the VAM app UI.

## Out of scope

- The VAM app Files panel upload
- Guest `write_file` and live mid-run disk writes
- FileRef / HSP attachments
- Admin cross-tenant write
- Multi-file upload in one request

## Locked decisions

Do not re-litigate without editing this list.

1. Idle only: reject upload when `find_active_session_on_workspace` finds a non-terminal session (`Terminated` and `Failed` are terminal). HTTP **409** via `OrchestratorError::AgentRunning`. No 423.
2. HEAD merge: upsert one path (chunk bytes, new manifest, `write_head`). Existing HEAD copies sibling files and sets `parent_manifest_hash` to the previous HEAD. Missing HEAD creates a manifest that contains only the uploaded file.
3. Write deny: refuse every path under `workspace/.vam/` and the path `workspace/.vam` itself. Stricter than the read deny-list. Do not reuse `is_deny_listed` as the write check. Write refusals are **400**, not the read API's **404**.
4. Same jail as read: only `workspace/...`. Owner prefix via `resolve_session_workspace_id`. The directory path `workspace` alone is rejected.
5. Auth: Bearer + `sessions:write` (`PutWorkspaceFile`). MVP matches list and download.
6. Size: 32 MiB max (`MAX_DOWNLOAD_BYTES`) → `FileTooLarge` / HTTP **413**.
7. Request body is raw `application/octet-stream`. Success is **200** JSON camelCase: `workspaceId` (guest suffix), `path`, `head`, `sizeBytes`. New manifest entries use mode `0o644`.
8. Guest `write_file` stays for mid-run writes. Upload is next-session materialize only.
9. FileRef / HSP attachments stay a separate path.

## Status legend

- `[ ]` not started
- `[~]` in progress
- `[x]` done (code + tests, or merged)

## Phases

### Phase 0 — Design of record

**Goal:** Publish the upload contract and record locked decisions.  
**Depends on:** none  
**Plan:** `.cursor/plans/workspace_files_upload_5565f6cb.plan.md`  
**Exit:** Doc + sidebar entry exist; locked decisions recorded in this tracker.

- [x] `docs/src/content/docs/orchestrator/workspace-files-upload.md`
- [x] Link from Workspace Files API + Starlight sidebar
- [x] This tracker and README row

### Phase 1 — Storage helper

**Goal:** Upsert one file into committed Drive9 HEAD under the `workspace/` jail.  
**Depends on:** Phase 0  
**Plan:** `.cursor/plans/workspace_files_upload_5565f6cb.plan.md`  
**Exit:** `cargo test -p vam-storage` green for the new write path.

- [x] `put_workspace_file_to_head` in `vam-storage` (reuse path normalize / jail; separate write deny)
- [x] Size check (32 MiB) → `FileTooLarge`
- [x] CAS chunk + merge into current HEAD manifest + `write_head` (create HEAD when missing)
- [x] Unit tests: jail, deny-list, size, upsert, missing HEAD

### Phase 2 — HTTP API

**Goal:** Owner upload on vamd with idle-workspace enforcement.  
**Depends on:** Phase 1  
**Plan:** `.cursor/plans/workspace_files_upload_5565f6cb.plan.md`  
**Exit:** `cargo test -p vam-orchestrator` covers the cases below.

- [x] `PUT /v1/workspaces/:id/files/*path` (raw body, JSON 200)
- [x] Auth Bearer + `sessions:write`; owner resolve via `resolve_session_workspace_id`
- [x] Busy guard → 409
- [x] Map jail / deny / size like the read API (write deny is 400)
- [x] Integration tests: happy path, cross-owner isolation, traversal, `.vam` deny, busy workspace, oversized body

### Phase 3 — CLI

**Goal:** `vam workspace put` mirrors the PUT API.  
**Depends on:** Phase 2  
**Plan:** `.cursor/plans/workspace_files_upload_5565f6cb.plan.md`  
**Exit:** `cargo test -p vam-cli` green; CLI docs updated.

- [x] `vam workspace put <remote-path> <local-file> [-w workspace]`
- [x] Same path normalize / auth as `ls` / `cat`
- [x] Surface busy / 413 / jail errors
- [x] CLI tests + `docs/src/content/docs/getting-started/cli.md` snippet

## Explicitly later

Work that must not be pulled into this tracker.

- The VAM app upload UI
- Live mid-run reads or writes through this API
- Guest `write_file` changes
- FileRef / HSP attachment changes

## Progress log

| Date | Phase | What landed |
|---|---|---|
| 2026-09-25 | 0 | Design doc, sidebar entry, tracker with locked decisions |
| 2026-09-25 | 1 | `put_workspace_file_to_head` plus jail, `.vam` deny, size, upsert, and missing-HEAD tests |
| 2026-09-27 | 2 | `PUT /v1/workspaces/:id/files/*path` with owner resolve, idle 409, and integration tests |
| 2026-09-27 | 3 | `vam workspace put`, upload error hints, CLI tests, and cli.md snippet |
