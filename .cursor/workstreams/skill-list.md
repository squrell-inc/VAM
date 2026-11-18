# Workstream: Skill list (`GET /v1/skills`)

```yaml
status: active
slug: skill-list
current_phase: 0
design_docs:
  - design/skill-list.md
  - docs/src/content/docs/orchestrator/member-agents.md
```

## Goal

A paged `GET /v1/skills` so the app's skill picker can offer the released skills a member may attach
to their draft agent. Released rows by default; the caller's own drafts only on request and only with
`catalog:write`; never another user's drafts; paging as `orchestrator/api` "Paging" says.

## Out of scope

- Paging the playbook and policy lists.
- Filters beyond `includeDrafts` (search, tier, tag).
- Changing the skill-package list.

## Locked decisions

Do not re-litigate without editing this list.

1. Route `GET /v1/skills`, gated by `require_catalog_read` like every catalog list.
2. Default answer: released rows only, for every caller.
3. `includeDrafts=true` adds the caller's own drafts; it needs `catalog:write` or `keys:admin` in the
   effective scopes, else 403 `missing required scope 'catalog:write'`. A value other than `true` or
   `false` is a 400.
4. Another user's draft is never listed, `users:admin` included.
5. Response `ListSkillsResponse { records: SkillRecord[], pagination }`; `SkillRecord` mirrors
   `PlaybookRecord` plus `source` (`package` | `manifest`). One record per stored row of
   `skill_packages` **and** `skills`; a `skills` manifest whose URN has a released package is left
   out. *Changed 2026-11-18 in wave 1 (was: `skills` table only): a packaged skill lives only in
   `skill_packages`, and a skill ref resolves to the package first (`catalog_resolve.rs:800-840`),
   so a `skills`-only list was empty for the scenario's packaged skill.*
6. Paging: `pageSize` / `pageToken` from `api/paging.rs`, token kind `skills`, newest first by the
   keyset `(created_at, source, rowid)` over both tables. *Changed with decision 5 (was: `rowid`
   alone, which collides across two tables).*

## Status legend

- `[ ]` not started
- `[~]` in progress
- `[x]` done (code + tests, or merged)

## Phases

### Done phases

Done phase sections are deleted; git history keeps them.

| Phase | Name | Done |
|---|---|---|

### Phase 0 — List skills

**Goal:** the route, its OpenAPI entry, and the member-agents page naming it.  
**Depends on:** member-draft-agents (closed; base `feat/member-draft-agents`)  
**Plan:** approved 2026-11-18 (owner brief)  
**Exit:** `member-lists-skills-and-sees-only-released-rows` active and green; unit tests for paging,
visibility and the `includeDrafts` gate; `cargo test -p vam-orchestrator` and `-p vam-spec` green.

- [x] Wave 0 — scenario pending, red for the right reason
- [x] Wave 1 — route, proto, OpenAPI, docs

**Scenario delta**

| Scenario | Change | Layer | Given | Why |
|---|---|---|---|---|
| `member-lists-skills-and-sees-only-released-rows` | new | process | `org-admin`, `org-publisher`, `org-member`, `skill-host-echo`, `skill-draft-probe` | A member's picker lists released skills only; a publisher's own draft appears only on request; nobody sees another's draft |

**Items**

- [x] `SL-0.1` — proto `SkillRecord`, `ListSkillsRequest`, `ListSkillsResponse`
  (`proto/vam/v1/orchestrator.proto`).
- [x] `SL-0.2` — store `list_skill_records_page` (`catalog.rs`), skill origins
  (`marketplace/installs.rs`), handler `list_skills` and route (`api/mod.rs`), the `includeDrafts`
  gate (`api/catalog_auth.rs`). Unit tests: released only by default, own draft with the flag,
  foreign draft never, member flag is 403, pages do not repeat or skip.
- [x] `SL-0.3` — `docs/api/openapi.yaml` `GET /v1/skills`, `SkillRecord`, `ListSkillsResponse`;
  `orchestrator/member-agents` names the route; `orchestrator/api` "Paging" lists skills.

**Waves**

| Wave | Items | Turns green | Repo | Model | Budget | Depends on | Status | PR |
|---|---|---|---|---|---|---|---|---|
| 0 | scenarios | — (pending, red for the right reason) | vam | Sonnet low | 30 min | — | [x] | |
| 1 | `SL-0.1`–`SL-0.3` | `member-lists-skills-and-sees-only-released-rows` | vam | Sonnet medium (adds an API) | 90 min | 0 | [x] | |

**Open questions** (each with a default)

- Should playbooks and policies move to the same default and paging? Default: no, separate work.

## Explicitly later

- Paged playbook and policy lists.

## Progress log

| Date | Phase | What landed (end with the scoreboard: scenarios green, red, fix cycles, wall time) |
|---|---|---|
| 2026-11-18 | — | Tracker opened |
| 2026-11-18 | 0 | Wave 0: `member-lists-skills-and-sees-only-released-rows` pending, red for the right reason at `then.0` (405, no `GET /v1/skills`). Scoreboard: 0 green, 1 red (right reason), 0 fix cycles, ~10 min. |
| 2026-11-18 | 0 | Wave 1: `GET /v1/skills` (store `list_skill_records_page`, `includeDrafts` gate, paging), proto `SkillRecord` / `ListSkillsResponse`, OpenAPI, `member-agents` "Listing skills", `api` "Paging", `catalog` routes. Fix cycle 1 moved the scenario from 405 at `then.0` to an empty list at `then.0` (`skills` table only); cycle 2 listed packages too (decisions 5 and 6 changed) and turned it green. Scoreboard: 1 green, 0 red, 2 fix cycles, ~60 min. |
