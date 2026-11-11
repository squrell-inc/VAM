# Workstream: Journey debug capture

```yaml
status: done
slug: journey-debug-capture
current_phase: done
design_docs:
  - docs/src/content/docs/telemetry/event-catalog.md
  - docs/src/content/docs/telemetry/privacy.md
```

## Goal

Host persists **redacted** tool args and LLM **outcomes** (not prompts) on run outbox events under `VAMD_TELEMETRY_CAPTURE`, and the VAM app journey sheet shows previews from `GET /v1/runs/{id}/events`.

## Out of scope

- Full prompt / message-list capture
- Interactive guest SSH / shell
- Debug bodies on trajectory replay or evidence packs
- GraphQL or new journey API

## Locked decisions

Do not re-litigate without editing this list.

1. LLM: **outcome only** (assistant text and/or tool-call JSON). No full prompt.
2. Tool: redacted args on `vam.v1.tool.invoked`; tool results stay on completed/failed (existing StreamLogs path).
3. Reuse `VAMD_TELEMETRY_CAPTURE` (`none` / `digest` / `redacted-full`).
4. Redact **before** outbox and CAS (T21). Keep `args_digest`. Trajectory replay stays digest-only.
5. Large bodies → Drive9 CAS; events get digest + preview (~4 KiB cap for debugger readability).
6. Journey reads fields from existing `GET /v1/runs/{id}/events`.

## Status legend

- `[ ]` not started
- `[~]` in progress
- `[x]` done (code + tests, or merged)

## Phases

### Phase 0 — Tracker and catalog

**Goal:** Workstream tracker + telemetry docs for new payload fields.  
**Depends on:** none  
**Plan:** `.cursor/plans/journey_debug_capture_707a16b0.plan.md`  
**Exit:** README row, event-catalog + privacy updated.

- [x] Copy tracker from `_template.md`
- [x] README row
- [x] `event-catalog.md` ToolInvoked + InferenceCompleted fields
- [x] `privacy.md` capture tier notes (4 KiB preview, PEP args)

### Phase 1 — VAM tool args

**Goal:** PEP `tool.invoked` carries redacted args CAS + preview when capture allows.  
**Depends on:** Phase 0  
**Plan:** same  
**Exit:** Tests for preview/digest/none + redaction before persist.

- [x] Wire `TelemetryCapture` + `ChunkStore` into PEP emit path
- [x] `emit_tool_invoked` CAS + `args_preview` / `args_cas`
- [x] Bump `CAPTURE_PREVIEW_MAX_BYTES` toward 4096
- [x] Tests

### Phase 2 — VAM inference outcome

**Goal:** `vam.v1.inference.completed` on **run_id** with redacted outcome fields.  
**Depends on:** Phase 1  
**Plan:** same  
**Exit:** Callback passes outcome only; outbox keyed by run.

- [x] `find_run_id_for_session` for outbox key
- [x] Extend `InferenceCompletionCallback` with outcome text + tool_calls JSON
- [x] Redact, CAS, preview in orchestrator callback
- [x] Tests

### Phase 3 — the VAM app journey

**Goal:** Journey sheet expands tool invoked args and inference outcome.  
**Depends on:** Phase 2 (on node under test)  
**Plan:** same  
**Exit:** `journeyMapper` + `JourneyTimeline` + tests.

- [x] `args_preview` on tool invoked rows
- [x] Inference completed / guest inference span expandable body
- [x] `journeyMapper.test.ts`

## Explicitly later

- Fetch full CAS blob by digest in UI when preview is truncated
- Fancy span waterfalls

## Progress log

| Date | Phase | What landed |
|---|---|---|
| 2026-09-17 | — | Tracker opened |
| 2026-09-17 | 0 | Catalog + privacy docs |
| 2026-09-17 | 1–3 | PEP args, inference outcome, journey UI |
| 2026-09-17 | 1 | Fix PEP tests: pass `script_digest` for catalogued `glob` |
