# Skill list (`GET /v1/skills`)

Proposal for open work. Tracker: `.cursor/workstreams/skill-list.md`. What the node does today is in
`docs/src/content/docs/orchestrator/member-agents.md` and `orchestrator/api.md`.

## Problem

Members save their own draft agents from released parts (`orchestrator/member-agents`, "Building a
client", point 4). The app's skill picker has to offer the released skills a member may attach, but
vamd has no skill list:

- `crates/vam-orchestrator/src/api/mod.rs` routes `/v1/skills` to `put(put_skill)` only; a `GET` is
  a 405 today.
- `GET /v1/skills/{urn}` reads one URN, so a client must already know it.
- The skill-package list (`GET /v1/skills/packages`) lists packages, not the manifests an agent's
  `skillRefs` name, and is the wrong shape for a picker.

Playbooks and policies already list (`list_playbooks`, `list_policies` in `api/mod.rs`, store
`list_playbook_records_visible`, `list_policies_visible` in `catalog.rs`). Both are unpaged
(`pagination: None`) and return the caller's own drafts to anyone.

## Proposal

`GET /v1/skills?pageSize=&pageToken=&includeDrafts=` returns `ListSkillsResponse { records:
SkillRecord[], pagination }`.

- **Auth.** `require_catalog_read`, the gate every catalog list uses (`api/catalog_auth.rs:12`).
- **Default: released rows only**, for every caller. A picker for a member's agent needs exactly
  these, and the default never leaks the caller's own unreleased work into a picker either.
- **`includeDrafts=true` adds the caller's own drafts.** It needs `catalog:write` (or `keys:admin`)
  in the caller's **effective** scopes, the test `require_agent_draft_write` uses for its `Full`
  mode (`api/catalog_auth.rs:46-57`). Without it the request is a 403
  `missing required scope 'catalog:write'`, the message the other skill routes send to a member.
  Any other value than `true` or `false` is a 400.
- **Never another user's draft**, not even for `users:admin`. Skills are authored only by
  `catalog:write` callers; an admin reads a foreign draft by URN if needed. This is the playbook
  and policy rule (`catalog_visible_where_sql`, `catalog.rs:341`), not the agent list's
  `include_foreign_drafts`.
- **One record per stored row** (each release, plus the caller's draft when asked), the shape of
  `ListPlaybooksResponse.records`. `SkillRecord` mirrors `PlaybookRecord`: `spec`, `ownerUserId`,
  `channel`, `catalogTier`, `tier`, `readOnly`, `origin`, `version`.
- **Paging** follows `orchestrator/api` "Paging": `pageSize` 1..1000, default 50, larger clamped,
  negative or non-number a 400; `pageToken` is the opaque `nextPageToken` (`api/paging.rs`), kind
  `skills`. Newest first by the row's SQLite `rowid` keyset (`rowid < cursor`), the runs rule
  (`list_runs_page_for_owner`, `catalog.rs:2241`). A draft edit is an `UPDATE` in place
  (`upsert_spec_draft`, `catalog_publish.rs:183`), so its `rowid` and place do not move; a new
  release is a new row on top, so it never shifts the next page.

## Rejected

- **Own drafts by default for `catalog:write`** (the playbook list's behaviour). A publisher
  building a member-facing picker would offer drafts no member can attach; opt-in keeps the
  default safe for every caller.
- **Ignoring `includeDrafts` for a member** instead of a 403. A silent downgrade hides a client
  bug; the docs already tell a client to decide controls from `GET /v1/me` scopes.
- **One row per URN (latest release).** A picker may pin `X.Y.Z`; the playbook list returns every
  row and a client groups them. Kept the same.
- **Paging playbooks and policies in the same change.** Out of scope; their unpaged responses stay.

## Evidence to keep

- `api/mod.rs` route table: `/v1/policies` and `/v1/playbooks` have `.get(list_…)`, `/v1/skills`
  has `put` only.
- `api/paging.rs` already holds `PageQuery`, `token`, `int_cursor`.
- `proto/vam/v1/orchestrator.proto` has `PlaybookRecord` and `ListPlaybooksResponse`; no skill
  list message exists.
