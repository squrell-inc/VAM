# Workstream: Agent experience (Epic 1 — create and release)

```yaml
status: active
slug: agent-experience
current_phase: 7
design_docs:
  - design/agent-experience.md
```

## Goal

Anyone in the app, and any developer at the CLI, can create an agent, attach
skills and playbooks, set permissions with toggles, test it, and release it
as a numbered version that the whole org can run. Nobody types a digest, a
URN or a prerelease string. Releasing a second, third or tenth version
works. This is Epic 1 of the problem-led roadmap in
`design/agent-experience.md` Part I. Epics 2–6 are listed there and are not
tracked here.

Phases touch both `squrell-inc/VAM` (`vamd`, CLI) and the app repo
(UI). Each phase says which.

**Milestones:** one per phase (P1–P7). Issue IDs use the local prefix `AX-<phase>.<n>`.
**Branches:** `feat/agent-experience` (vam), `feature/agent-experience`
(the VAM app). Issue PRs target these. The epic merges to `main` when it closes.

## Out of scope

- Epics E2–E6 (packaging and lockfile, governance console, outcomes,
  triggers, Squrell marketplace). See the design doc for their order.
- Semver prereleases (`-rc.N`) and collaborators on a draft.
- A new policy engine. Cedar stays the engine; Phase 5 changes what people
  author (rules as data), per design Part IV.
- E8 (egress and secrets, design Part V) and org guardrails (E3). Both
  have their own planning sessions.

## Locked decisions

Do not re-litigate without editing this list.

1. **Plan from problems.** Every epic and story cites the market problems
   (P1–P10) in `design/agent-experience.md` Part I that it serves.
2. **A draft is a private, mutable working copy with no version.** Only its
   owner sees it or runs it. There is one per artifact. Release does not
   consume it. In the app, `vamd` stores it scoped to the owner. For a
   developer, the files on disk are the draft.
3. **A version exists only on release.** It is semver, immutable, chosen at
   release, with a bump suggested by the system. It must be greater than the
   latest release. Releasing the same version with the same digest is a
   no-op.
4. **Authors never type a digest, a URN or a prerelease string.** `vamd`
   computes digests and freezes every reference at release. References in a
   draft may float.
5. **A released agent references only released dependencies.** Release
   fails with an actionable message otherwise. The app offers to release
   them together.
6. **The URN domain is the origin.** It is the org instance's domain
   (configured in `vamd`), the Squrell marketplace domain, or `*.local`.
   Releasing at the org domain makes the artifact visible to the whole org.
   The app never hardcodes the domain.
7. **Permissions are edited as toggles by default.** Cedar is generated from
   them. The generated Cedar stays readable in an advanced view for admins. Refined by
   decision 11: toggles edit rules stored as data.
8. **Two front doors, one artifact.** App and CLI call the same `vamd` API
   (`developer-experience` decision 1).
9. **Getting started first (2026-10-10).** Every epic ships its
   getting-started slice (design doc, "Epics, in order") before any later
   stories. E1 as a whole is that slice for E1, with Phase 5 limited as
   noted there.
10. **One planning session per epic, one build session per issue.** See the
    design doc, "How we build it".
11. **Permissions are data; Cedar is the engine (2026-10-11).** A rule is
    `(target, allow | ask | deny)`. Scopes: platform floor, org, source
    (skill or MCP server), agent, run. Within a scope the most specific rule
    wins; across scopes the strictest wins. Rules compile to a fixed
    platform policy set plus per-scope attributes on `Tool` entities. Design
    Part IV §IV.2–IV.3.
12. **Agent rules live in the agent and release with it.** No separate
    policy release. The app stops auto-releasing policies. Presets are
    copied in, not referenced.
13. **Defaults for a new agent:** tools by side-effect class (read-only and
    mutable local allow; mutable external and unspecified ask), and
    internet allow.
14. **"Always allow / deny" from an approval prompt writes an agent rule
    into the draft**, for the owner on a draft run only. On a released
    version, approvals are once only. Not offered when a stricter scope
    decides. Release shows permission changes; loosening is at least a
    minor bump.
15. **Skills ship no Cedar.** A skill may mark its own tools
    `approvalRequired` (tighten-only). MCP rules target the server, not the
    user's connection.
16. **The egress broker evaluates `EgressRequest` in Cedar.** The platform
    IP floor and per-secret destinations stay hard limits outside Cedar.
17. **Hand-written Cedar remains** for org guardrails (E3, forbid-only) and
    `advanced` agent attachments. Both are frozen at release, and the
    runtime enforces the pin and digest (DX-6A). Released legacy agents
    keep their exact behaviour through a compatibility path; drafts migrate
    on open (§IV.11).
18. **The effective-permissions view and `vam policy explain` ship in the
    getting-started slice.**
19. **Placement:** Phase 5 is "Permissions as data". E8 (egress and
    secrets: TLS-terminating proxy with a per-session CA, inject by
    destination, upstream enterprise proxy) is a separate epic that runs
    in parallel. Org guardrails are E3.
20. **Tightening bump (2026-10-13; resolves §IV.14 #1).** allow → ask is a
    minor bump. Any → deny is a major bump (a revoked capability, §II.4).
    Loosening is at least minor.
21. **No remembered approvals for members (§IV.14 #2).** On a released
    version approvals are once only. "Always allow for me" stays deferred.
22. **MCP rule targets are origin plus path, normalised (§IV.14 #3).**
23. **Name collisions (§IV.14 #4).** The existing visible-name collision
    check is enough for Phase 5; release fails on a collision. DX-68 fixes
    it properly later.
24. **"Ask" for hosts is out of the slice (§IV.14 #5).** Host rules are
    allow or deny.
25. **Presets are Squrell-only at first (§IV.14 #6).** Org-authored presets
    belong to E3.
26. **Source rules on an org-scoped MCP connection are edited by the
    connection's owner (§IV.14 #7).** They can only tighten.
27. **"Always" rules on a draft are surfaced only on the permissions page
    (5.6).** No per-draft banner or counter in the slice.

## Status legend

- `[ ]` not started
- `[~]` in progress
- `[x]` done (code + tests, or merged)

## Phases

### Done phases (archived)

Full sections and their progress-log rows: [archive/agent-experience.md](archive/agent-experience.md).

| Phase | Name | Done |
|---|---|---|
| 0 | Capture | 2026-10-10 |
| 1 | Release works more than once | 2026-10-10 |
| 2 | Versions are visible | 2026-10-11 |
| 3 | The server owns pins | 2026-10-12 |
| 4 | Compose in the app | 2026-10-12 |
| 5 | Permissions as data | 2026-10-14 |
| 6 | Guided create, test, release | 2026-10-14 |

### Phase 7 — Origin from the URN domain

**Goal:** Where an artifact comes from is explicit, and the org registry is
real.
**Depends on:** Phase 0
**Plan:** breakdown approved 2026-10-16 (registry domain `vam.squrell.dev`; other defaults accepted). Both waves merged and released to `main` 2026-10-16; **released, checklist out**.
**Repos:** vam, the VAM app
**Exit:** `vamd` exposes its registry domain. The app builds URNs from it
and the user's namespace, and shows an origin badge (Org, Squrell, Local).

- [x] `vamd` config and API for the instance's registry domain.
- [x] the VAM app stops hardcoding `local.vam.dev/default` (`useVamCatalog.ts`).
- [x] An origin badge, derived from the domain, on every catalog list.
- [x] Document the domain table in `docs/src/content/docs/authoring/`.

<!-- Phase-breakdown agent, 2026-10-16. Each wave session plans its wave
before building. Design: design/agent-experience.md §II.6 (domain table),
§II.1 "URNs are hardcoded", open question 1 (marketplace domain). -->

Code map (2026-10-16). The app constant moved in Phase 6: it is
`NEW_AGENT_URN_PREFIX` in the VAM app `src/components/vam/guided/newAgent.ts`,
used by `useVamCatalog.ts:362`; agents are the only artifact the app
creates with a URN. In vam, `vam_spec::validation::is_local_skill_registry`
(`crates/vam-spec/src/validation.rs:3482`) hardcodes `local.vam.dev` with
the comment "until E1 Phase 7", and skill admission
(`skill_package_admit.rs:278`) and skill-ref resolution
(`skill_package_resolve.rs:81`, called from `manager.rs:1032`) reject any
other registry. So setting a real org domain without AX-7.3 makes every
org-domain skill unpublishable. `DaemonConfig` (`daemon.rs`) has no domain
field; `GET /v1/me` (`api/me.rs`, proto `GetMeResponse`,
`orchestrator.proto:514`) is readable by every authenticated caller and the
app already holds its result as `MeInfo`. Release visibility does not
depend on the domain today (a release is org-visible because it is
released), so decision 6's "visible to the whole org" already holds and
needs no change. Today nothing stops a caller from creating an artifact
under any domain, which would let anyone forge an origin badge.

**Items**

- [x] `AX-7.1` — **Registry config in `vamd`.** Add `registry_domain`
      (`VAMD_REGISTRY_DOMAIN` / `--registry-domain`, default
      `local.vam.dev` so dev nodes, tests and examples are unchanged) and
      optional `marketplace_domain` (`VAMD_MARKETPLACE_DOMAIN`, default
      empty, never hardcoded) to `DaemonConfig`. Validate at startup:
      lowercase host of slug segments as `parse_skill_urn` accepts; refuse
      to start on an invalid value. Put a pure, typed classifier in
      `vam-spec` (e.g. `RegistryDomains` + `fn origin(&self, urn) ->
      Origin { Org, Squrell, Local, Other }`, `thiserror`, no unwrap):
      `*.local` and `local.vam.dev` are Local; the configured registry
      domain is Org unless it is itself local; the marketplace domain,
      when set, is Squrell; anything else is Other. Make it reachable from
      the API state and the session manager (thread it, or follow the
      `brokers::install_egress_max_bytes` global pattern; the wave plan
      picks). Files: `crates/vam-orchestrator/src/daemon.rs`,
      `src/bin/vamd.rs`, `src/api/mod.rs` (`AppState`),
      `crates/vam-spec/src/validation.rs` (or a new `origin.rs`).
      **Accept:** unit tests for config parsing (default, valid, invalid)
      and for every `Origin` branch, including the unconfigured node
      (registry = `local.vam.dev` → Local).
      **Test:** `cargo test -p vam-spec`, `cargo test -p vam-orchestrator
      --lib daemon`, `cargo check --workspace`.
- [x] `AX-7.2` — **Expose it on the API.** Add `registry_domain`,
      `default_namespace` and `marketplace_domain` (empty when unset) to
      `GetMeResponse` (additive proto fields 5–7, so the baseline
      conformance test passes) and to `docs/api/openapi.yaml`.
      `default_namespace` is the caller's username, lowercased; if that is
      not a valid URN segment, `default`. Files: `proto/vam/v1/orchestrator.proto`,
      `crates/vam-orchestrator/src/api/me.rs`, `docs/api/openapi.yaml`.
      **Accept:** an API test: a node started with a registry domain
      returns it and the namespace for an admin and for a member key; an
      unconfigured node returns `local.vam.dev`.
      **Test:** `cargo test -p vam-spec --test conformance`,
      `cargo test -p vam-orchestrator --test api_tests me_`.
- [x] `AX-7.3` — **Admission follows the domain.** (a) Skill admission and
      skill-ref resolution accept the configured registry domain as well as
      the local registries (replace the hardcoded `local.vam.dev` in
      `is_local_skill_registry` with a domain-aware check; keep the
      "remote pull is not supported" error for every other domain).
      (b) A caller create or publish of any catalog artifact (agent,
      playbook, skill, skill package, policy) under the configured
      marketplace domain is refused, in the same place and style as
      `catalog_tier::reject_caller_platform_urn`, so nobody can forge a
      Squrell badge. Files: `crates/vam-spec/src/validation.rs`,
      `crates/vam-orchestrator/src/skill_package_admit.rs`,
      `skill_package_resolve.rs`, `catalog_tier.rs`, the create/publish
      handlers that already call `reject_caller_platform_urn`.
      **Accept:** tests: a skill at the org domain publishes and an agent
      referencing it runs its resolution; a skill at another domain is
      still refused with the same message; a create under the marketplace
      domain fails with an actionable 400; with no marketplace domain set,
      nothing extra is refused.
      **Test:** `cargo test -p vam-spec`, `cargo test -p vam-orchestrator
      skill_package`, `cargo test -p vam-orchestrator --test api_tests`.
- [x] `AX-7.4` — **Docs: the domain table.** In
      `docs/src/content/docs/authoring/model.md` (the URN section, lines
      ~31–43), add the table from design §II.6 (Org / Squrell / Local,
      who sees a release, how each is configured), the namespace default,
      and replace "Origin badges and a configured org domain are not part
      of authoring yet". State that the marketplace domain is not yet
      chosen. Add `VAMD_REGISTRY_DOMAIN` and `VAMD_MARKETPLACE_DOMAIN` rows
      to `docs/src/content/docs/orchestrator/daemon.md`. No new page, so no
      sidebar change. **Accept:** `pnpm build` in `docs/` passes; the
      table names no hub hostname as final.
      **Test:** `cd docs && pnpm build`.
- [x] `AX-7.5` — **The app builds URNs from `vamd`.** Add the three fields
      to `MeInfo` (Tauri `src-tauri/src/vamd.rs` with `serde` aliases for
      camel and snake case, and the TS type in `src/types/vam.ts`).
      Replace `NEW_AGENT_URN_PREFIX` with
      `newAgentUrn(name, registry)` →
      `urn:vam:agent:<registryDomain>/<defaultNamespace>/<slug>`; the
      collision check is unchanged. If `vamd` returns no registry domain
      (an older node), New agent shows "Update vamd to create agents"
      instead of guessing a domain. Update the tests and the `p6-*`
      harness expectations that assert `local.vam.dev/default`
      (`e2e/lib/p6.ts`, `e2e/lib/p6.test.ts`, `docs/e2e-mac.md`, and the
      unit tests that use that literal only as fixture data may stay).
      **Accept:** no `local.vam.dev` literal left in the VAM app `src/`
      outside tests (except the run URN, see open questions); unit tests
      for `newAgentUrn` with an org domain, a local domain and a missing
      domain. **Test:** `pnpm test`, `pnpm exec tsc --noEmit`,
      `cargo test --manifest-path src-tauri/Cargo.toml`.
- [x] `AX-7.6` — **Origin badge on every catalog list.** A pure helper
      `urnOrigin(urn, registry)` → `Org | Squrell | Local | Other`, same
      rules as AX-7.1 (the Local rule is the fixed platform rule:
      `*.local` and `local.vam.dev`; the org and marketplace domains come
      only from `MeInfo`). Render a small badge beside the existing
      Platform / Draft / version labels in `AgentsCatalogList.tsx`,
      `SkillsCatalogList.tsx`, `PlaybooksCatalogList.tsx`,
      `PoliciesCatalogList.tsx`, and in the matching inspector headers.
      The Platform tier label stays; it answers a different question.
      MCP connections, secrets, users and hosted brokers have no URN
      domain and get no badge. **Accept:** unit tests for `urnOrigin`
      covering each branch, an unset marketplace domain, and a node whose
      registry is `local.vam.dev` (→ Local, not Org); each of the four
      lists shows the badge (test id per row).
      **Test:** `pnpm test`, `pnpm exec tsc --noEmit`.

**Waves** (model: Sonnet low by default, medium second, high rarely with a reason)

| Wave | Items | Repo | Model | Depends on | Status | PR |
|---|---|---|---|---|---|---|
| 1 | `AX-7.1`, `AX-7.2`, `AX-7.3`, `AX-7.4` | vam | Sonnet medium — new config and API fields (proto + OpenAPI), crosses vam-spec and vam-orchestrator, changes admission | — | [x] | — |
| 2 | `AX-7.5`, `AX-7.6` | The VAM app | Sonnet low — one app, follows the existing `MeInfo` and list-label patterns | 1 (merged and deployed `vamd` field names) | [x] | app |

**Open questions** (each with a default)

1. **Registry domain for the team instance:** `vam.squrell.dev` or bare
   `squrell.dev`? **Owner decision 2026-10-16: `vam.squrell.dev`** (breakdown default was `squrell.dev`). A URN domain is permanent
   identity and names the organisation, not the host that serves it; the
   instance can move (another host, region or cloud) without renaming
   every artifact. It does not collide with the marketplace, whose domain
   is compared exactly. Set as `VAMD_REGISTRY_DOMAIN` on the team
   instance after Wave 1 lands.
2. **Marketplace domain:** stays unset (`VAMD_MARKETPLACE_DOMAIN` empty),
   so no row shows Squrell until E6 picks it. `hub.squrell.cloud` stays a
   placeholder in docs only. **Default: unset.**
3. **Existing `local.vam.dev/default/*` and `local.vam.dev/vam/*` rows on
   the team instance:** URNs are identity and pins reference them, so
   **default: no rename or migration in Phase 7**; they show as Local. New
   agents use the org domain. A rename tool, if wanted, is later (E2).
4. **Where `vamd` exposes the domain:** **default: additive fields on
   `GET /v1/me`** (any authenticated caller, already in the app). Not
   `GET /v1/node`, which members may get 403 on.
5. **Who classifies origin:** **default: `vamd` owns the rule (AX-7.1) and
   the app mirrors it from `MeInfo` (AX-7.6).** A per-row `origin` field on
   every catalog list response is the alternative; it is a larger proto
   change and is not needed while there are three domains.
6. **Namespace:** **default: the username, lowercased, else `default`;
   not enforced** (a caller may still create under another namespace, as
   today). Team namespaces arrive with Apps/Spaces (design open question 3).
7. **Foreign domains** (not org, not local, not marketplace): **default:
   still accepted for agents, playbooks and policies, badged Other**;
   skills keep today's local-only rule plus the org domain. Refusing them
   is an org-policy choice for E3.
8. **Internal URNs** (`urn:vam:run|token|attempt|lease|node:local.vam.dev/…`
   in `api/mod.rs`, `events.rs`, vam-telemetry `egress.rs`, the VAM app
   `useVamChat.ts:1354`): not catalog artifacts and have no badge.
   **Default: out of scope**, unchanged.
9. **CLI:** **default: no new command in Phase 7.** The CLI already takes
   explicit URNs; `vam init` (E2) will read `registryDomain` from
   `/v1/me`.

**Phase close:** unit suites green (vam CI on; the VAM app 667/667 on
the merged base). Released 2026-10-16; **team checklist out**.

**Team checklist — Phase 7 (origin from the URN domain)**

Setup (ops, once):
- Deploy vamd from vam `main` (latest) with
  `VAMD_REGISTRY_DOMAIN=vam.squrell.dev`. Leave `VAMD_MARKETPLACE_DOMAIN` unset.
  Set it once: URNs created under it are permanent.
- Then install the app from the VAM app `main` (latest).
  (Order matters: the new app on an old vamd disables New agent.)

Steps (one admin account, one member account):
1. Sign in as admin. Run `curl -s -H "Authorization: Bearer <token>"
   https://vam.squrell.dev/v1/me`. **See:** `"registryDomain":"vam.squrell.dev"`
   and a `defaultNamespace` (your username, lowercase). No `marketplaceDomain`.
2. Agents → New agent, name it `Origin Check`, create. **See:** the draft's URN
   is `urn:vam:agent:vam.squrell.dev/<your-namespace>/origin-check`.
3. Agents list. **See:** `Origin Check` has a blue **Org** badge, also in its
   inspector header.
4. Same list. **See:** agents made before this release (`local.vam.dev/...`) and
   platform examples (`examples.local`) show a grey **Local** badge.
5. Open Skills, Playbooks and Policies. **See:** every row has an origin badge.
6. Release `Origin Check` as 1.0.0. Sign in as the member. **See:** the member
   sees it in Agents with the **Org** badge.
7. CLI: `vam skill publish` a skill whose URN starts
   `urn:vam:skill:vam.squrell.dev/`. **See:** accepted. Change its URN domain to
   `hub.vam.dev` and publish again. **See:** refused, "not a local catalog
   registry".
8. Docs → Authoring → Model. **See:** a domain table (Org, Squrell, Local).

Report: "Phase 7 pass", or the step number with a screenshot or the error text.

## Explicitly later

- E2: `vam.json`/`vam.lock`, `vam init|add|publish`, eject and adopt
  (`developer-experience` Phases 3–4, 6).
- E6: Squrell marketplace publish, signing, trust tiers
  (`developer-experience` Phase 11).
- Semver prereleases and shared drafts.
- E8 (egress and secrets): design doc Part V. Its planning session opens
  its own tracker. Runs in parallel with E1 from Phase 4.
- Org guardrails: E3 (design §IV.10).
- E7 (machine and build): design doc Part III. Its planning session opens
  its own tracker. E7 Phase 1 (standard bases) may start alongside E1.

## Progress log

| Date | Phase | What landed |
|---|---|---|
| 2026-10-16 | 7 | **Released to `main`**: merge commits; bases level. Deploy vamd first with `VAMD_REGISTRY_DOMAIN=vam.squrell.dev`, then the app. Phase stays open until the team's manual check passes. |
| 2026-10-16 | 7 | **Wave 2 merged** (Sonnet low): `MeInfo` carries `registryDomain`/`defaultNamespace`/`marketplaceDomain`; `newAgentUrn(name, me)` builds `urn:vam:agent:<domain>/<namespace>/<slug>` (New agent disabled with "Update vamd to create agents" on an older vamd); `urnOrigin` + `OriginBadge` on the Agents, Skills, Playbooks and Policies lists and inspectors; p6 harness expectations updated. Orchestrator re-ran on the PR head: 667/667 app tests, tsc, e2e typecheck, 45 Tauri tests. All Phase 7 items done; **phase close waits on the owner's manual check** (team instance needs `VAMD_REGISTRY_DOMAIN=vam.squrell.dev`). |
| 2026-10-16 | 7 | **Wave 1 merged** (Sonnet medium): `VAMD_REGISTRY_DOMAIN` / `VAMD_MARKETPLACE_DOMAIN` (process-global `vam_orchestrator::registry`), `vam_spec::origin` (`Origin`, `RegistryDomains`), `/v1/me` adds `registryDomain`, `defaultNamespace`, `marketplaceDomain` (omitted from JSON when unset), skill admission accepts the org domain, the marketplace domain is refused for callers, domain table in `authoring/model.md`. CI green. Wave 2 started. |
| 2026-10-16 | 7 | **Phase 7 breakdown approved.** AX-7.1–7.6 in two waves (W1 vam, Sonnet medium; W2 the VAM app, Sonnet low). Owner chose `vam.squrell.dev` as the registry domain; other open-question defaults accepted. Wave 1 started. |
