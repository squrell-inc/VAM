# Squrell marketplace for VAM

The marketplace that every VAM node trusts by default. It is a plain Git repository: an index
(`marketplace.json`) and, for each listing, the skills, agents and playbooks it installs. An org
admin enables a listing in VAM's Org settings; the node imports its artifacts at the listing's tag
and admits them read-only, marked as Squrell content.

| Listing | Installs |
|---|---|
| `microsoft-365` | the `m365-mail` skill (search, read, draft, send, calendar, free times), the Executive assistant agent, the Morning triage playbook. Needs the Microsoft 365 connector built into VAM. |

Licence: Apache-2.0 (see `LICENSE`).

## How a node trusts it

The Squrell node ships with this URL preconfigured: `https://github.com/squrell-inc/vam-marketplace`.
Nothing is installed until an admin enables a listing, and enabling a listing does not enable its
connector: the admin does that in Org settings, Connectors, and each member signs in for themselves.
Nothing from this repository runs on a node outside the guest, and every tool declares exactly which
host, methods and paths it may reach through the member's connection; the node enforces that.

An admin can add other marketplaces by URL. Those show as "Unverified" and are limited by the node's
rules for non-Squrell content. This repository is the only one whose content is installed under the
`hub.squrell.cloud/squrell/` names.

## Layout

```
marketplace.json                      the index: name, publisherDomain, listings
listings/<listing>/skills/<name>/     SKILLS.md, manifest.json, tool entry points (Bun TypeScript), jigs/ cards
listings/<listing>/agents/<name>/     agent.json
listings/<listing>/playbooks/<name>/  playbook.json
tests/<skill>/                        Bun tests for a skill's tools, against recorded service answers
```

## Add a listing

1. Create `listings/<id>/` with its artifacts. Skills, agents and playbooks use the package layout of
   `vam catalog validate` (the same check the node runs when it imports them).
2. Add the listing to `marketplace.json`: `id`, `displayName`, `description`, the `connector` it needs
   (a URN, optional), its `artifacts` (each `kind` and `path`), a `ref` (the release tag, see below)
   and `requiresNode`.
3. Check it with a `vam` CLI: `vam catalog validate . --strict`.
4. Open a pull request. CI validates the whole repository (see below).

## Release a listing

A release is a per-listing tag named `<listing id>-v<major>.<minor>.<patch>` (`microsoft-365-v1.0.0`)
on the commit that holds exactly what the listing should install. The index at that commit names the
tag in the listing's `ref`. Nodes import a listing only at its `ref`; a branch is never installed,
and a tag that is moved is refused by nodes that already recorded it. Publish a new version by
changing the content and `ref` in a pull request, merging it, then tagging the merge commit.

## CI

`.github/workflows/validate.yml` runs on every pull request and tag push. It builds `vam-cli` at a
pinned ref of the public VAM repository, then:

- runs `vam catalog validate . --strict` on the repository. The one accepted warning is
  `calendar_find_slots` being READ_ONLY while it POSTs (Graph's `findMeetingTimes` is a POST that only
  suggests times); any other finding fails the run;
- for each listing, checks that its `ref` tag exists, that the index at the tag names that tag, and
  that the repository at the tag validates (a `ref` whose tag is not created yet is noted, not an error,
  on a branch or pull request; on a tag push it is an error);
- runs the Bun tests for the skills (`bun test tests`).

The workflow needs no secrets, so it also runs on pull requests from forks.

## Mirror for an air-gapped node

The node reads this repository over HTTPS. To run without reaching GitHub, mirror it
(`git clone --mirror https://github.com/squrell-inc/vam-marketplace`, then push the mirror to an
internal Git server that serves HTTPS), keep the tags, and in Org settings, Marketplace, add the
mirror's URL. A marketplace added by URL shows as "Unverified" and is subject to the node's rules for
non-Squrell content; the content and tags are identical, so listings install the same way.

## Contributing

Changes to `microsoft-365` go through a pull request reviewed by the owners in `CODEOWNERS`. Do not
push tags that are not on a merged commit.
