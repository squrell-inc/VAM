# Workstream: Workspace Files UI

```yaml
status: done
slug: workspace-files-ui
current_phase: done
design_docs:
  - docs/src/content/docs/orchestrator/workspace-files.md
```

## Goal

Read-only Files UI over Drive9 HEAD via the existing Workspace Files API (list + download). Operators can browse committed workspace files and save/open them from the VAM app. No upload/write in this workstream.

## Out of scope

- Upload / write / mutate workspace files
- Live mid-run reads (only committed HEAD)
- OpenAPI generation for these routes
- Binary editors / in-app hex viewers
- New `ConnectError` variant for 413 (Forbidden + message is enough)

## Locked decisions

Do not re-litigate without editing this list.

1. Default workspace guest id when omitted: `assistant` (CLI / Tauri `resolve_workspace_id`). UI Phase 2+ always passes the active chat workspace guest suffix via `cleanWorkspaceSuffix(activeWorkspaceId)` — never rely on the omitted default for the Files panel.
2. Commands: `list_workspace_files` and `get_workspace_file` (raw bytes). Phase 3 adds `save_workspace_file` (get bytes + native save dialog + disk write).
3. Path normalize in Rust (mirror CLI): prepend `workspace/` when missing; reject empty, absolute, and `..` before the HTTP call.
4. URL: encode each path segment with existing `encode_path_segment`; keep `/` unencoded.
5. Auth: existing `execute_authed` Bearer flow (`sessions:write` on the server).
6. 413: map explicitly in `handle_http_error` to `ConnectError::Forbidden` with a clear message (reuse existing kind; no new enum variant this phase).
7. List returns JSON `String` (same pattern as `list_secrets`). Download returns `Vec<u8>` over IPC.

## Status legend

- `[ ]` not started
- `[~]` in progress
- `[x]` done (code + tests, or merged)

## Phases

### Phase 1 — Tauri + TypeScript types

**Goal:** Wire the VAM app Tauri/HTTP bridge and TS types so later UI phases only call `invoke`. No React panel.  
**Depends on:** none (API already shipped in vam)  
**Plan:** `.cursor/plans/workspace_files_ui_p1_1a30e57d.plan.md`  
**Exit:** `list_workspace_files` + `get_workspace_file` commands registered; TS types exist; path normalize + 413 unit tests pass; `cargo check` in `src-tauri`.

- [x] Workstream tracker + README row
- [x] `authenticated_get_bytes` + 413 → Forbidden in `vamd.rs`
- [x] `list_workspace_files` + `get_workspace_file` helpers + path normalize + tests
- [x] Tauri commands + `generate_handler`
- [x] `WorkspaceFileEntry` + `ListWorkspaceFilesResponse` in `vam.ts`

### Phase 2 — List panel

**Goal:** Right-panel Files list for the **active chat workspace** (guest suffix).  
**Depends on:** Phase 1  
**Plan:** `.cursor/plans/workspace_files_ui_p2_878eeb66.plan.md`  
**Exit:** User can open Files from ChatArea (FileText beside Palette) and see path/size rows from `list_workspace_files` for the active workspace.

- [x] Files right-panel list UI (`WorkspaceFilesDrawer`)
- [x] Hook wiring (`invoke` only; no new HTTP)

### Phase 3 — Save / open

**Goal:** Download a selected file via `get_workspace_file` and write with a save dialog (or open).  
**Depends on:** Phase 2  
**Plan:** local Cursor plan (`workspace_files_ui_p3`, not in the repo)  
**Exit:** User can save a listed file to disk from the app.

- [x] Save dialog / open flow
- [x] Error surfacing for 404 / 413

### Phase 4 — Preview + panel polish

**Goal:** Text preview modal (MD/JSON/plain) from the Files drawer; close right panel when left activity leaves chat. Keep Files open across workspace switches.  
**Depends on:** Phase 3  
**Plan:** local Cursor plan (`workspace_files_ui_p4`, not in the repo)  
**Exit:** Preview works for text files with size/truncate guards; Files/Settings dismiss when leaving chat; Files stays open on workspace switch.

- [x] Truncation / empty UX (list truncated banner + empty HEAD from Phase 2/3)
- [x] In-app text preview modal (MD via ChatMarkdown, JSON pretty, plain `<pre>`; binary refuse; 2 MiB / 3000 lines / 512 KiB guards)
- [x] Close `rightPanel` when `sidebarView !== "chat"`

## Explicitly later

Work that must not be pulled into this tracker.

- Upload / write APIs
- Live mid-run workspace reads
- Binary editors
- New ConnectError kind for payload-too-large

## Progress log

| Date | Phase | What landed |
|---|---|---|
| 2026-09-25 | — | Tracker opened |
| 2026-09-25 | 1 | Phase 1 done: Tauri `list_workspace_files` / `get_workspace_file`, path normalize + 413 tests, TS types |
| 2026-09-25 | 2 | Phase 2 done: ChatArea FileText → right panel `files`; `WorkspaceFilesDrawer` lists HEAD via `list_workspace_files` for active guest suffix; locked decision #1 amended |
| 2026-09-25 | 3 | Phase 3 done: Tauri `save_workspace_file` (get bytes + blocking_save_file + fs write); Selected footer Save… with saving/error/success; 404/413 via formatErrorMessage |
| 2026-09-25 | 4 | Phase 4 done: Preview modal (get_workspace_file → MD/JSON/plain); sidebar dismiss off-chat; Files stays open across workspace switches |
