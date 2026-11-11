---
title: Catalog
description: Agents, skills, skill packages, playbooks and policies as catalog entries, with drafts, releases and the checks the host runs.
status: shipped
proves:
  - released-agent-approvals-are-once-only
  - member-chats-with-a-released-agent
  - crates/vam-orchestrator/tests/agent_check_tests.rs
  - crates/vam-orchestrator/tests/allowed_models_tests.rs
  - crates/vam-orchestrator/tests/api_tests.rs
---

The catalog holds the things a run is made from: agents, skills, skill packages, playbooks and
policies. Each entry is named by a URN. The host stores them in SQLite, stamped with their owner.

## Routes

| Entry | Write and list | Release |
|---|---|---|
| Agent | `PUT`, `GET /v1/agents` | `api/catalog_release.rs` |
| Skill | `PUT /v1/skills`, reads by URN | `catalog_release::skill_release_route` (`api/mod.rs:169`) |
| Playbook | `PUT`, `GET /v1/playbooks` | `catalog_release::playbook_release_route` (`api/mod.rs:163`) |
| Policy | `PUT`, `GET /v1/policies` | `catalog_release::policy_release_route` (`api/mod.rs:156`) |

Routes: `api/mod.rs:142-169`.

## Drafts and releases

An owner keeps one draft of an entry. A **release** snapshots that draft at a version and needs
the owner and a live `publish:grant` (publisher, org admin or operator). After the first write,
only the owner can change an entry; another user's write is a 403. A caller reads their own
drafts and every released version; an org admin reading with `users:admin` also sees other
people's drafts. A run that names an agent without a version uses the latest release. A member
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
is refused (`catalog_tier.rs:42`). Entries are stored with a tier, `user` or `platform`
(`catalog_tier.rs:12`).

## Skill packages

A skill package is admitted only if it holds `SKILLS.md` at the archive root, or inside a single
top-level directory, and the file starts with YAML frontmatter and has a non-empty body
(`skill_package_admit.rs:233-243,291-304`). The package also needs `manifest.json`
(`skill_package_admit.rs:49`).

## Agent check

`POST /v1/agent-checks` checks that an agent's parts fit together before it runs
(`api/mod.rs:160`, `api/composition.rs`). Proved by `agent_check_tests.rs` and the CLI test
`crates/vam-cli/tests/agent_check_cli.rs`.

## Allowed models

The operator can set a list of allowed models with `GET` and `PUT /v1/node/allowed-models`
(`api/allowed_models.rs`). A model a caller names that is off a non-empty list is refused with a
400 that names the allowed models. Proved by `allowed_models_tests.rs`.

## Not proved here

Reserved-namespace refusal and tiers are read from `catalog_tier.rs`; `api_tests.rs` covers the
release and ownership rules in part.
