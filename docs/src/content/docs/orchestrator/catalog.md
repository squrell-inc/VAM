---
title: Catalog
description: Agents, skills, skill packages, playbooks and policies as catalog entries, with drafts, releases and the checks the host runs.
status: shipped
proves:
  - released-agent-approvals-are-once-only
  - member-draft-agent-is-private-and-in-the-members-namespace
  - member-lists-skills-and-sees-only-released-rows
  - crates/vam-orchestrator/tests/member_draft_agents_tests.rs
  - member-chats-with-a-released-agent
  - crates/vam-orchestrator/tests/agent_check_tests.rs
  - crates/vam-orchestrator/tests/allowed_models_tests.rs
  - crates/vam-orchestrator/tests/api_tests.rs
---

The catalog holds the things a run is made from: agents, skills, skill packages, playbooks and
policies. Each entry is named by a URN. The host stores them in SQLite, stamped with their owner.

## Routes

| Entry | Write and list | Who writes | Release |
|---|---|---|---|
| Agent | `PUT`, `GET /v1/agents` | `catalog:write`, or `agents:draft` under the [member rules](/orchestrator/member-agents/) | `api/catalog_release.rs` |
| Skill | `PUT`, `GET /v1/skills` (paged, released only unless `includeDrafts=true`; see [Listing skills](/orchestrator/member-agents/#listing-skills)) | `catalog:write` | `catalog_release::skill_release_route` (`api/mod.rs:197`) |
| Playbook | `PUT`, `GET /v1/playbooks` | `catalog:write` | `catalog_release::playbook_release_route` (`api/mod.rs:190`) |
| Policy | `PUT`, `GET /v1/policies` | `catalog:write` | `catalog_release::policy_release_route` (`api/mod.rs:182`) |

Routes: `api/mod.rs:167-197`.

## Drafts and releases

An owner keeps one draft of an entry. A **release** snapshots that draft at a version and needs
`catalog:write`, a live `publish:grant` (publisher, org admin or operator) and ownership of the row. After the first write,
only the owner can change an entry; another user's write is a 403. A caller reads their own
drafts and every released version; an org admin reading with `users:admin` also sees other
people's drafts. A member owns the draft agents they save, and only the owner and a caller with `users:admin` can read them (`member-draft-agent-is-private-and-in-the-members-namespace`). The rules for those drafts are on [Member agents](/orchestrator/member-agents/). A run that names an agent without a version uses the latest release. A member
can chat with a released agent (`member-chats-with-a-released-agent`), and a released agent's
approvals are once-only (`released-agent-approvals-are-once-only`).

## Outcomes per version

What a version's runs added up to is one read away: `GET /v1/agents/{urn}/outcomes` (see the API
page). Runs are grouped by the version their receipt pins; a draft's runs are grouped by the
digest of the draft content, so an edit to the draft starts a new group, and the `draft` entry
is the content a release would freeze. Cost uses the model maker's list price, so a gateway
never sets the price; an unpriced model (the mock) leaves the cost out and is named.

`vam agent stats <agent> [--version X.Y.Z|draft] [--k N] [--diff] [--json]` prints these numbers;
without `--version` it lists the versions that have runs, and `--diff` compares the version (the
draft by default) with the release before it. `vam agent release` prints that draft-versus-previous
diff on stderr before it releases; stdout stays the release JSON, and a failure to fetch the diff
only warns.

## Platform entries

URNs in the `examples.local` namespace are reserved for platform artifacts; a user write to one
is refused (`catalog_tier.rs:46`). Entries are stored with a tier: `user`, `platform`, or
`marketplace` for rows a trusted marketplace import wrote (`catalog_tier.rs:21-28`).

## Skill packages

A skill package is admitted only if it holds `SKILLS.md` (or the community `SKILL.md`) at the
archive root, or inside a single top-level directory (`skill_package_admit.rs:226-249`), and the
file starts with YAML frontmatter and has a non-empty body (`skill_package_admit.rs:274-280`,
checked by `vam_spec::package::check_skills_md`). The package also needs `manifest.json`
(`skill_package_admit.rs:251-272`).

## Agent check

`POST /v1/agent-checks` checks that an agent's parts fit together before it runs
(`api/mod.rs:186`, `api/composition.rs`). Proved by `agent_check_tests.rs` and the CLI test
`crates/vam-cli/tests/agent_check_cli.rs`.

## Allowed models

The operator can set a list of allowed models with `GET` and `PUT /v1/node/allowed-models`
(`api/allowed_models.rs`). A model a caller names that is off a non-empty list is refused with a
400 that names the allowed models. Proved by `allowed_models_tests.rs`.

## Not proved here

Reserved-namespace refusal and tiers are read from `catalog_tier.rs`; `api_tests.rs` covers the
release and ownership rules in part.
