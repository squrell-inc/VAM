# Improving the VAM

## What we are building

VAM agents should be able to investigate large, read-only skill documentation
corpora for the full configured turn budget without losing the path back to
source material.

This milestone is one implementation phase with four independently reviewable
issues:

1. Remove the three-consecutive-same-tool force-answer heuristic. The configured
   `max_iterations` remains the agent-loop turn cap, including its existing hard
   upper bound.
2. Stamp canonical `/mnt/drive9/skills/<skill-id>/...` paths on `read_file` and
   `grep` results and retain a usable `read_file` pointer when old tool output is
   compacted.
3. Define a compact root `INDEX.md` contract for large skill `docs/` trees.
   `SKILLS.md` should tell the agent to grep that index by logical document or
   table name before reading the resolved absolute path.
4. Add only three concise default `SYSTEM.md` reminders: grep the index first,
   re-read the stamped path after truncation, and write generated work into the
   workspace.

The complete skill package remains on the read-only Drive9 skills mount. Only
the `SKILLS.md` body is injected by `load_skill`; extra documentation is opened
on demand with built-in file tools.

## Why

Large corpora can contain thousands of files. Broad `glob` and `grep` results
are capped at 200, repeated calls to the same tool currently trigger forced
finalization after three rounds, and compaction can replace an old skill read
with a message that says the output is not on disk. Together these behaviors
can stop a valid investigation before the configured iteration budget is
exhausted.

A small searchable index plus durable guest paths fixes discovery and recovery
without spending prompt tokens on the corpus itself.

## What we are not building

- No documentation corpus, index dump, or large file listing in `SYSTEM.md`.
- No database-backed documentation search service.
- No HSP or new Cedar action for reading skill documentation.
- No increase to the 200-result `glob` or `grep` limits.
- No increase to the `max_iterations` hard cap.
- No writable skills mount and no host filesystem path exposure.
- No copy of the full skill corpus into the workspace.

An `INDEX.md` workspace copy is not required. If implementation evidence shows
it is useful, only the compact index may be copied under a namespaced VAM path;
the mounted source remains authoritative.

## PR base

All implementation PRs target `feat/improving-the-vam`, not `main`.

Repository and branch marker:
`[repo=squrell-inc/VAM] [branch=feat/improving-the-vam]`

## Work items

1. Remove three-same-tool force-answer
2. Preserve skills-mount re-read paths through compaction
3. Add a searchable index contract for large skill docs
4. Add concise large-doc navigation bullets to SYSTEM.md

Item 4 is blocked by items 2 and 3 because its guidance must describe
capabilities that exist. The other three issues have no implementation
dependency on each other.

## Implementer constraints

- Keep `max_iterations` as the authoritative loop cap; do not replace the
  removed heuristic with another repetition limit.
- Preserve HITL checkpoint compatibility when retiring same-tool state.
- Emit guest-visible skills-mount paths only. Never leak host paths.
- Keep `/mnt/drive9/skills/` read-only and preserve workspace/path-jail checks.
- Existing skill packages without `INDEX.md` must continue to load.
- VAM skill manifests are named `SKILLS.md`, not `SKILL.md`.
- Keep default prompt additions to three short bullets; do not inject corpus
  content.
- Add focused tests and run `cargo test -p vam-runtime` plus
  `cargo check --workspace`. Run focused tests for any additional modified
  crate.
- Update Starlight authoring or runtime documentation if the implemented index
  or output format becomes a public contract.
