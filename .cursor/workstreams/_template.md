# Workstream: <title>

```yaml
status: active          # active | paused | done
slug: <kebab-case>
current_phase: 0
design_docs:
  - docs/src/content/docs/<path>/
```

## Goal

One paragraph: what "done" means for this workstream.

## Out of scope

- …

## Locked decisions

Do not re-litigate without editing this list.

1. …

## Status legend

- `[ ]` not started
- `[~]` in progress
- `[x]` done (code + tests, or merged)

## Phases

### Done phases (archived)

Full sections: [archive/<slug>.md](archive/<slug>.md).

| Phase | Name | Done |
|---|---|---|

### Phase 0 — <name>

**Goal:**  
**Depends on:** none  
**Plan:** not drafted | approved YYYY-MM-DD  
**Exit:** …

- [ ] …

<!-- Filled by the phase-breakdown agent (background, Opus high), approved by the
owner before any wave starts. Each wave session plans its wave before building. See .cursor/workstreams/phase-playbook.md §2–§3. -->

**Scenario delta** (first output of the breakdown; design/scenarios.md §9)

| Scenario | Change | Layer | Given | Why |
|---|---|---|---|---|
| `<scenario-name>` | new / changed: … / retired: … | process | `<world piece>` | … |

**Items** (each names the pending scenarios it turns green)

- [ ] `<PREFIX>-0.1` — scope; files; acceptance check; test commands

**Waves** (model: Sonnet low by default, medium second, high rarely with a reason)

| Wave | Items | Turns green | Repo | Model | Budget | Depends on | Status | PR |
|---|---|---|---|---|---|---|---|---|
| 0 | scenarios | — (pending, red for the right reason) | vam | Sonnet low | 45 min | — | [ ] | |
| 1 | `<PREFIX>-0.1` | `<scenario-name>` | vam | Sonnet low | 60 min | 0 | [ ] | |

**Open questions** (each with a default)

- …

**Phase close:** unit suites on the merged base; release; team checklist out
(the next phase does not wait for it; playbook §6).

**Team checklist — Phase 0**

Setup: what to deploy, in which order, env vars.

1. Do … **See:** …

Report: "Phase 0 pass", or the step number with a screenshot or the error text.

### Phase 1 — <name>

**Goal:**  
**Depends on:** Phase 0  
**Plan:** not drafted  
**Exit:** …

- [ ] …

## Explicitly later

Work that must not be pulled into this tracker.

- …

## Progress log

| Date | Phase | What landed (end with the scoreboard: scenarios green, red, fix cycles, wall time) |
|---|---|---|
| YYYY-MM-DD | — | Tracker opened |
