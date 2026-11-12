# Agent experience: problem-led roadmap and the authoring model

Status: **proposed**. Tracker: `.cursor/workstreams/agent-experience.md` (E1).
E7 and later epics open their own trackers from their planning session.

## What we are building

A way of planning VAM from the problems teams face when adopting agents,
rather than from the parts VAM already has. This document does two things:

1. **Part I** lists the problems VAM exists to solve. Each one names
   who feels it, what VAM already has, and what is missing. Every later epic
   and story must cite the problems it serves. A story that serves none of
   them waits.
2. **Part II** settles the first epic: how an agent gets created, composed,
   tested and released, by someone in the app and by a developer in Git. The
   product owner confirmed this model on 2026-10-10.
3. **Part III** settles E7: how the agent's machine gets the packages it
   needs, through standard base images and a `Vamfile`.
4. **Part IV** settles how permissions compose: permissions are data, Cedar
   is the engine. It replaces E1 Phase 5 and moves org guardrails to E3.
5. **Part V** sets out E8, egress and secrets: a TLS-terminating egress
   proxy, injecting secrets by destination, and chaining to an enterprise
   proxy. Confirmed by the product owner on 2026-10-11.

**Getting started comes first.** The goal is a VAM that people can start
using, not one polished in every direction. Every epic has a **getting
started** slice: the smallest set of stories that makes the epic useful to a
first organization. Slices are built before any epic's later stories.

The product goal in one line: **agents can act safely, and VAM makes building
and running them easy enough to be the default way an organization does it.**

## Why

Phases 0–2.5 of `developer-experience`, HSP, MCP OAuth and playbooks have
landed. The platform is strong, but people building on it get stuck on how
its storage works:

- An agent has no visible version. After its first release an agent cannot
  be released again (see §II.1).
- Releasing a skill whose draft is at `0.1.1` fails.
- Nobody can say what a "draft" is.
- Authors are asked to put SHA-256 digests in JSON files.
- Nobody knows how to connect skills or playbooks to an agent. Playbooks
  cannot be attached in the app at all.

These come from one root cause: **authors work directly with the catalog's
storage model**. URNs, version triplets, prerelease strings, digests and
Cedar references are how the catalog stores things, not how people think
about building an agent.

---

# Part I — The problems

Fourteen problems, in rough order of how often they block adoption
(P12 and P13 added 2026-10-15, see `design/agent-runtime.md`; P14 the same day, see `design/jig.md`). Statuses
reflect `main`. Each problem has at least one measured line in
`design/benchmark.md` §1; a phase that serves a problem reports that line
before and after.

| # | Problem, as a user would say it | Who feels it |
|---|---|---|
| P1 | "We can't let an agent touch real systems." | Security, platform teams |
| P2 | "We don't know what our agents did, or who allowed it." | Security, compliance, managers |
| P3 | "Building a useful agent takes an engineer a week." | Everyone who isn't a platform engineer |
| P4 | "Our experts' processes live in their heads." | Team leads, domain experts |
| P5 | "Agents are unreliable. They skip steps and we can't tell if they're getting better." | Anyone running agents in production |
| P6 | "The agent needs our SaaS tools, but we won't hand it our keys." | Every user, IT |
| P7 | "Real work doesn't happen in a chat window." | Operations, support, engineering |
| P8 | "AI spend is unpredictable." | Finance, team leads |
| P9 | "Third-party agents and skills are a supply-chain risk." | Security |
| P10 | "We can't send this data to someone else's cloud." | Regulated industries |
| P11 | "The agent's machine doesn't have the tools our work needs." | Developers, anyone whose agent needs git, Python, Java |
| P12 | "We connected the tool and it still doesn't know how to use it, and it can't look anything up." | Everyone who connects an MCP or skill |
| P13 | "It can't do real work: it forgets, I can't talk to it while it works, long tasks die, and a crash can break things." | Anyone using agents for more than a one-shot question |
| P14 | "Real work needs forms, pickers and status, not a chat transcript." | Anyone whose agent asks for structured input or reports state; builders who repeat UI per agent |

### P1 — Safe action

**Already there:** Firecracker microVM per run. The guest is untrusted. Cedar
decides on every tool call. Egress goes through a proxy with
`vamsec_` placeholders, so the guest never holds a secret. HITL overlays and
playbook gates and guards are in place. MCP tool lists are pinned, and drift
is re-approved.
**Missing:** an org baseline policy as a real artifact (DX-64). A capability
diff on upgrade (DX-74). `vam policy explain` (DX-65). Permissions that a
non-expert can read (§II.5).
Part IV (permissions as data) and E8 (Part V) close most of this.

### P2 — Governance and audit

**Already there:** the telemetry kernel, journey timeline, audit events, run
metering, and HITL with parked runs.
**Missing:** an org-wide inventory of agents: who released what, which
version runs where, and which capabilities each holds. An approvals inbox
covering every run, not just one chat. A review queue for capability
changes for the security admin (`developer-experience.md` §14, rule 4).

### P3 — Creating an agent is hard

**Already there:** the app's Create agent dialog (name, description,
instructions), attaching skills in the agent inspector, and the CLI's
`agent`, `skill`, `playbook` and `policy` publish and release commands.
**Missing:** a working version and release model, composition that never
asks for a digest, attaching playbooks in the app, permissions as toggles,
and testing before release. **This is Epic 1 (Part II).**

### P4 — Encode and share expertise

The Oracle PR example: a developer who knows the organization's PR process
packages it as an agent with a playbook, and the whole organization runs it.
**Already there:** playbooks with host-enforced steps, gates, guards and
evidence. `vam playbook init/explain/test` with pass^k. `extends`
merge semantics.
**Missing:** a project file and lockfile (`vam.json`/`vam.lock`, DX-40/41).
`vam init agent`, `vam add` and `vam publish` (DX-50/73). An org registry
where a release is visible to everyone (§II.6). Customizing an installed
agent through `extends` in the app.

### P5 — Reliability and outcomes

**Already there:** step state tracked by the host, evidence checks, and
finish feedback before a playbook run succeeds. `vam playbook test` with
pass^k.
**Missing:** outcome metrics per agent *version*: success rate, steps
skipped or denied, approvals hit, cost per successful run. Evaluations as a
release gate: a release that lowers pass^k is flagged. Rolling back to a
previous version.

### P6 — Access to SaaS without handing over keys

**Already there:** a remote MCP client in `vamd`, MCP OAuth, a per-user
vault, the egress proxy, and HSP brokers.
**Missing:** prompting the user to connect an agent's services when they
install it. Connections the whole org can share, owned by an admin.
Local/stdio MCP behind HSP (DX-110A).
Standard HTTPS clients cannot use the egress proxy today: the guest proxy
refuses `CONNECT` (`vam-runtime/src/egress.rs:269`), so skills rewrite their
requests. **E8 (Part V).**

### P7 — Work outside chat

**Already there:** parking and resuming runs on a fresh VM, `on_paused`
notifications, and playbook gates. Designs exist for broker-triggered runs
(`hsp-triggers.md`) and Apps/Spaces (`vam-apps-spaces.md`).
**Missing:** the triggers themselves (webhook, schedule, event). Delivering
approvals and results where people already work (Slack, email). Agents
running in the background on behalf of a team.

### P8 — Cost control

**Already there:** inference broker budgets, run-level metering, and the
operator's model allowlist.
**Missing:** budgets per agent and per team, visible in the app. Cost per
outcome (shared with P5). Spend alerts.

### P9 — Supply chain

**Already there:** Ed25519 in `vam-security`, skill capability declarations,
skill Cedar that may only `permit` its own tools, and MCP drift
re-approval.
**Missing:** publisher signing and verification (DX-121/122), namespace
ownership (DX-120), trust tiers (DX-123), and the Squrell marketplace
review flow (§II.6).

### P10 — Self-hosting and data residency

**Already there:** `vamd` runs on the organization's own hosts. The
inference broker supports several providers, including Ollama.
**Missing:** a documented single-org deployment story. Per-org model and
provider policy, and data-residency guidance. Mostly packaging and docs.

### P11 — The machine lacks tools

**Already there:** a guest image built by `scripts/build-rootfs.sh` that holds
busybox, Bun and `vam-runtime`, nothing else. A fully specified `Vamfile`
language in `docs/build/` (21 pages). `crates/vam-build` holds only
`SKILLS.md`.
**Missing:** Python, git, Java and common CLIs in the base. A way for an
agent author to add system packages. A builder. An image an agent can point
to and pin. **This is E7 (Part III).**

## Epics, in order

Each epic names the problems it serves. E1 and E7 are broken into phases.
The others get their phases from their own planning session (see "How we
build it").

| Epic | Serves | Phases | Getting started slice | Later |
|---|---|---|---|---|
| **E1 Create and release** | P3, P4, P1 | 7 | All of E1, with Phase 5 limited to the getting-started slice of Part IV §IV.12 (allow/ask/deny rules, defaults, "always allow", effective view, `explain`, egress in Cedar). **Parts II and IV.** | Advanced Cedar attachment, presets, egress "ask", run narrowing |
| E2 Developer packaging and org registry | P4, P9 | ~4 | `vam init`, `vam.json` holding the version, `vam release` from a folder, install from the org catalog | Ranges and `vam.lock` resolution, `vam dev` tiers, eject and adopt, `extends` in the app |
| E3 Governance console | P1, P2 | ~5 | Agent inventory (who released what, which version, what it can do), one approvals inbox, and **org guardrails**: org-scope deny/ask rules for tools and hosts, read by the Part IV resolver (§IV.10) | Capability-diff review queue, hand-written forbid-only Cedar guardrails, allowlist-only internet mode, guardrail release and audit |
| E4 Outcomes | P5, P8 | ~4 | Success, failure and cost per agent version on the agent page | Evaluations gate releases, rollback, budgets per agent and team |
| E5 Work outside chat | P7, P6 | ~5 | Built right after E10 Phase 6: Credentials (AS-1.1, AS-1.2), the trigger gateway with resume delivery, task waits on events (`wait_for`), `vam.schedule` and `vam.github` brokers (`hsp-triggers.md` amendment, `agent-runtime.md` §IX.4), and "connect your services" on install | Slack and email (`vam.notify`), more event sources, Apps and Spaces |
| E6 Squrell marketplace | P9, P4 | ~4 | A curated Squrell catalog of first-party skills that an org admin installs | Community publishing, signing, trust tiers, review |
| **E7 Machine and build** | P11, P3, P4 | 5 | Standard bases (Phase 1), image artifact (Phase 2), Vamfile with the core command set (Phase 3), the app's Machine step (Phase 4). **Part III.** | Snapshots, SBOM, signing, marketplace portability |
| **E8 Egress and secrets** | P6, P1, P3 | 5 | Phases 1–3: TLS-terminating egress proxy with a per-session CA, standard HTTPS clients work unchanged in the guest, secrets injected by destination. **Part V.** | Upstream enterprise proxy (Netskope, Zscaler) with identity headers; egress audit and "ask" for hosts |
| **E10 Agent runtime** | P12, P13, P5 | 7 | Phases 0–3: first-try tool use, web, follow-up context, steering. **`design/agent-runtime.md`.** | Never twice (effect journal), long work, learns; later code mode and child-VM sub-agents |
| **E11 Jig** | P14, P4, P7 | 7 | A basic version in E10 Phase 3 (built-in jigs for every chat card), then Phases 1–3: tool transactions, rich forms in playbook gates, shippable jig packages. **`design/jig.md`.** | Agent-written jigs, registry and marketplace (micro-apps), Slack/email, A2UI adapter |
| **E12 Run receipt** | P2, P5, P1 | 3 | Phase 1: one canonical, two-tier (host-observed vs model-written), Ed25519-signed receipt per run from data the host already records; `vam receipt show` and offline `verify`. **`design/run-receipt.md`.** | Receipt in the app and `vam.notify` (with E5); effects section and per-version outcomes (with E10 Phase 4 and E4) |

**Build order (revised 2026-10-21, one phase at a time):** E10 Phase 2
(in progress) and Phase 3, then **E12 Run receipt** Phase 1
(`design/run-receipt.md`), then E5's getting-started slice (triggers, task
waits, scheduler and GitHub brokers, `vam.notify` for Slack approvals;
`hsp-triggers.md` amendment) with E12 Phase 2, then E10 Phase 4 (effect
journal) with E12 Phase 3, then **E4's getting-started slice** (success,
cost and pass^k per agent version, per model, on the agent page, fed by
receipts), then E8 Phases 2–3, E7 Phase 1, E10 Phases 5–6, E11 Jig
Phases 1–3, E2, the rest of E7, E3, the rest of E5, the rest of E4, E6.

Why this order: the goal is a governed run, on the operator's own hardware
and model, that comes back with a verifiable receipt. The receipt needs
host-tracked playbooks (built), triggers and approvals where people work
(E5), effects that are never repeated (E10 Phase 4), and outcome numbers
per version and per model (E4). Those move ahead of web polish, the image
builder and Jig packages.

**Three standing tracks run beside this order**, planned as hotfix-sized
phases that do not wait for an epic boundary:

- **B, the benchmark** (`design/benchmark.md`): B0 baseline now, B1 with
  E12 Phase 1, B2 with E5 and E10 Phase 4, B3 with isolation-truth Phase 1.
  Every phase below names the benchmark lines it moves (see "How we build
  it").
- **H, the harness** (`design/harness.md`, added 2026-10-26): H0 switches
  and honesty with B0 (one wave); H1 open-model adapters; H2 the ablation
  grid and the first profiles with B1; H3 and H4 as the grid directs. Every
  benchmark row names its harness profile.
- **IT, isolation truth** (`design/isolation-truth.md`): Phase 1 (CI boots
  a microVM, jailer and TLS on by default) now; Phase 2 (known leaks) and
  Phase 3 (honest docs, clippy gate) before 0.1.0.

**What 0.1.0 ships** and what is cut from it, with the decision rubric
every proposal uses from now on: `.cursor/workstreams/phase-playbook.md` §11.1.

**Order of 2026-10-15 (superseded):** H0, H1, E1 Phase 7, E10 Phases 0–3,
E8 Phases 2–3, E7 Phase 1, E10 Phases 4–6, E5's getting-started slice, E11
Jig Phases 1–3, E2, the rest of E7, E3, the rest of E5, E4, E6. H0, H1, E1
Phase 7 and E10 Phases 0–1 are done under that order.

**Earlier order of the getting-started slices:** E1 and E7 Phase 1 in parallel,
then E2, the rest of E7, E3, E5, E4, E6. **E8 runs in parallel with E1**
from E1 Phase 4 onward. It touches `vam-network`, `vam-runtime` and the
rootfs, which E1 does not, apart from one call site in the egress broker
(see §V.4).

The existing trackers keep their own scope. `developer-experience` Phases
3–11 hold most of the mechanisms behind E2, E6 and E7. This document only
sets the order in which they are pulled.

## How we build it

- **One planning session per epic.** It owns the epic's design section and
  tracker, drafts each phase's Linear issues one phase at a time, and runs
  the phase-exit check. It writes no product code.
- **One build session per Linear issue.** It starts from the issue text
  alone, works on one branch, opens one PR and drives it to green.
- **Phases are 3–6 issues, in waves.** Issues in a wave run in parallel and
  are split by file so they don't conflict. A UI change gets its own
  the VAM app issue, blocked by the `vam` issue that lands its API contract.
- **Hardware checks are explicit issues.** Cloud sessions cannot boot
  Firecracker. A phase exit that needs a real microVM gets a verify issue
  run on a Lima or KVM host, until the CI Firecracker lane
  (`isolation-truth.md` Phase 1) makes it a scenario marked
  `needs: firecracker`.
- **Every phase shows the problem it solved with a number (added
  2026-10-21).** A phase plan carries a **benchmark delta** next to its
  scenario delta: the lines in `benchmark.md` §1 it moves, the tasks it
  adds, and the baseline on the current base. The phase close records
  before and after on the same models in the scoreboard. A line that did
  not move is written down as a finding. A problem with no line gets its
  line first. Scenarios prove behaviour with the scripted model; the
  benchmark proves outcomes with real ones; a phase needs both.
- **Every proposal is scored before it is planned (added 2026-10-21).**
  Waves, items, backlog rows and new designs carry a one-line decision
  from the rubric in `phase-playbook.md` §11.1: NOW, SLICE, BACKLOG or CUT,
  with the proof and cost scores and the line it moves.
- **The repo is the source of truth.** Trackers hold the work queue; issue
  IDs are local to each tracker.

---

# Part II — Epic 1: create and release

## II.1 What is broken today

**An agent can be released once.** Agent, playbook and policy drafts are
always stored at a fixed slot, `1.0.0-draft`
(`crates/vam-orchestrator/src/catalog_publish.rs`
`publish_agent_draft_with_owner`, constants at `catalog.rs:32-34`). Release
looks for a draft at the requested version (`release_spec_row`). So
releasing `1.0.0` a second time conflicts on digest, and `1.1.0` finds no
draft. The app always sends `1.0.0` (`buildReleaseCatalogRequest`,
`the VAM app/src/types/vam.ts`). `AgentInspector` calls `onRelease(urn)` with no
version.

**Skill release sends the draft's prerelease.** Skill packages take their
version from the manifest, for example `0.1.1-draft`. `SkillInspector`
passes the row's version, `prerelease: "draft"` included, to release.
`api/skill_packages.rs` rejects that with "release version must not include
a prerelease suffix".

**Digests are typed by hand.** The app writes `EMPTY_SHA256_HEX` when a
package has no digest (`resourceRefFromSkillPackage`). The examples carry
pins written by hand.

**URNs are hardcoded.** The app creates every agent as
`urn:vam:agent:local.vam.dev/default/<slug>` (`useVamCatalog.ts`), so the
domain says nothing about where the agent comes from.

**Playbooks cannot be attached in the app.** `AgentInspector` only displays
`defaultPlaybookRef`/`allowAnyPlaybook`. There is no playbook catalog list.

## II.2 The model, as confirmed

There are two front doors to one artifact. A developer works in Git with
the CLI. Anyone else works in the app. Both call the same `vamd` API
(`developer-experience` decision 1) and produce the same released artifact.

```
            draft (private, mutable, no version)
                 │  release as X.Y.Z
                 ▼
  released X.Y.Z (immutable, visible to the whole org)
                 │  publish to marketplace (E6, later)
                 ▼
  Squrell marketplace listing (public, community)
```

## II.3 Draft

- **A draft is the owner's working copy.** No other user sees it in the
  catalog or can run it. Its owner may run it.
- **A draft has no version number.** Editing it never asks for one. Saving
  replaces it. The revision counter increments for the owner's own
  history.
- **There is one draft per artifact.** Releasing does not delete it. The
  owner keeps editing after a release, and the next release snapshots the
  draft again.
- **Where it lives.** In the app, the draft is stored by `vamd` and scoped to
  its owner, because a run needs it on the host to boot. For a developer,
  the files on disk *are* the draft. `vam dev`/`vam run` use them without
  publishing. Pushing a draft to `vamd` is optional and only needed to try
  it in the app.
- **A workspace or chat pins "Draft"** as a choice next to the released
  versions. That is the owner only, as isolation decision 6 already
  requires.

## II.4 Versions and release

- **A version exists only once released.** It is semver, immutable, and
  chosen at release.
- **The system suggests the bump.** Patch for instruction or text changes.
  Minor for an added skill, playbook or capability. Major for a removed
  skill, a revoked capability or a narrower input. The author can override
  it. The new version must be greater than the latest release.
- **Release carries a short note.** It becomes the version's changelog.
- **Releasing the same version with the same content is a no-op.** Different
  content at an existing version is a conflict that names the next free
  version.
- **The release fails with a fix if a dependency is draft-only.** For
  example: "skill `jira` has no released version: release it first, or
  release both together". The app offers to release both together.
- **The version is shown everywhere.** That includes the catalog lists, the
  inspector (with a version history), the workspace pin and the run header.

## II.5 Composition with no digests

- **Authors never type a digest, a URN or a prerelease string.** These appear
  only in resolved output: the released spec, `vam.lock`, the run record and
  the audit trail.
- **References in a draft may float.** No version means the latest released
  version, or the owner's own draft of a dependency they own. A range such
  as `^1.2` is allowed once E2 lands (DX-44).
- **Release freezes every reference** to an exact `version + digest`
  computed by `vamd`. That is the pin a member's run uses.
- **Skills are attached from the catalog by name.** Attaching one shows its
  declared capabilities (DX-10) as plain-language consent.
- **Playbooks are attached from the catalog by name.** If the agent lacks a
  skill or capability the playbook `requires` (DX-13), the app says so and
  offers to add it.
- **Permissions are toggles.** The default editor is a list of capabilities
  (tools, egress hosts, secrets, workspace access) and "ask before" (HITL)
  switches. Cedar is generated from them. The generated Cedar stays
  readable in an advanced view for admins.
  **Refined by Part IV:** permissions are stored as data (allow, ask, deny
  rules) and compiled to Cedar by the host.

## II.6 Where an artifact comes from: the URN domain

`urn:vam:<kind>:<domain>/<namespace>/<name>`. The domain says where an
artifact comes from. Nothing else is needed.

| Domain | Meaning | Who sees a release |
|---|---|---|
| The org instance's domain, set in `vamd` config | Built inside this organization | Everyone in the org |
| The Squrell marketplace domain (proposed: `hub.squrell.cloud`) | Published by the community | Anyone who installs it |
| `*.local` (`is_local_skill_registry`) | Examples and local development | This node only |

- **The app stops hardcoding `local.vam.dev`.** `vamd` exposes its registry
  domain. The namespace defaults to the user, or to a team once teams
  exist.
- **The domain shows as an origin badge** in the app: Org, Squrell, Local.
- **Releasing at the org domain makes the artifact visible to the whole
  org.** This is the "org marketplace". It needs no separate store; it is
  the catalog that already exists (DX-40B).
- **Squrell marketplace artifacts are installed into the org by an admin,
  after capability consent.** They keep their original URN so their origin
  stays visible. Org policy may forbid foreign domains. The flow is E6.
- **Publishing from the org to Squrell is a separate, later step.** It starts
  from a released version and needs namespace ownership and signing
  (DX-120/121). Whether someone who is not a developer may do it is an open
  question.

## II.7 The two journeys

**In the app**
1. **New agent:** a name and what it is for. The domain and namespace come
   from the instance and the user.
2. **Instructions:** the system instructions.
3. **Skills:** browse the org catalog (and Squrell, once E6 lands) and add.
   Review what each skill can do.
4. **Playbooks:** attach. Fix any missing requirements in one click.
5. **Permissions:** toggles, plus "ask before" switches.
6. **Test:** chat with the draft. The journey timeline shows every tool
   call and policy decision.
7. **Release:** pick the suggested version and add a note. The org can now
   run it.

**As a developer (the Oracle PR agent)**
1. `vam init agent oracle-pr` scaffolds `vam.json` (name, version) and an
   `agent.json` with references that float.
2. `vam add skill github` and `vam add playbook pr-review` write `vam.lock`.
3. `vam dev` / `vam playbook test` run the draft from disk.
4. `vam release` resolves and pins references, then releases at the org
   domain.
5. Later: `vam publish --marketplace` submits a released version to Squrell.

Steps 1, 2 and 4 depend on E2 (DX-40/41/50/73). Epic 1 makes the same flow
work today through the existing `vam agent publish`/`release` commands.

---

# Part III — E7: machine and build

## III.1 The rules

The `Vamfile` uses Docker's grammar with VAM's rules:

1. **PID 1 is always `vam-runtime`.** An image can add things but cannot
   replace init. The builder injects `vam-runtime` as `/sbin/init` whatever
   the image says. This resolves DX-103A.
2. **Packages install at build time, never at run time.** A running agent
   still cannot install anything over the network (`developer-experience`
   decision 9b).
3. **An image is a versioned artifact.** `urn:vam:image:<domain>/<ns>/<name>`
   follows the E1 model: a private draft, versions on release, a digest pinned
   by `vamd`. An agent points to an image with `imageRef`. With no
   `imageRef`, it gets the standard VAM base.
4. **A Vamfile is its own project.** It produces the image, and agents
   reference it (decision 19b stands). Skills still never require an image
   rebuild. They declare `requiresRuntime`, and an image provides it
   (decisions 9a and DX-101B).

## III.2 Commands

| | Commands | Why |
|---|---|---|
| Keep from Docker | `FROM`, `RUN`, `COPY`, `ENV`, `ARG`, `WORKDIR`, `LABEL`, multi-stage `AS` | Developers already know them |
| Drop | `ENTRYPOINT`, `CMD`, `EXPOSE`, `VOLUME`, `HEALTHCHECK`, `ONBUILD` | `vam-runtime` owns boot, Drive9 owns mounts, and nothing listens for inbound connections |
| Add | `RUNTIME python@3.12` | Installs a runtime from VAM's maintained set. This is what satisfies a skill's `requiresRuntime` |
| Add | `PACKAGES apk git jq postgresql-client` | System packages, declared, pinned and cached. The app generates this line |
| Add | `TEST <cmd>` | A smoke check in the built image. The build fails if it fails |
| Add | `BUILD_EGRESS <host>` | The hosts `RUN` may download from, recorded for supply-chain review |
| Keep, renamed | `SECRET` (build-time mount only), `HARDEN cis-level-1` (was `SECURITY_POLICY`) | Already specified. Secrets are never baked into the image. The new name avoids confusion with Cedar policy |
| Later | `WARM <cmd>` | Runs a command, then snapshots the warmed machine for fast boot |

Example, for the Oracle PR agent:

```
FROM vam/base:2
RUNTIME java@21
PACKAGES apk git gh openssh-client
COPY ./tools/ojdbc.jar /opt/oracle/
TEST git --version && java -version
```

## III.3 The build pipeline

```
Vamfile -> parse -> pin FROM to a digest -> run stages in a builder microVM
        (RUN may reach only BUILD_EGRESS hosts; layers cached by digest)
        -> OCI layers -> ext4 rootfs -> inject vam-runtime as /sbin/init
        -> TEST -> digest + SBOM -> catalog draft -> release
        -> (later) Firecracker snapshot per image
```

`RUN` executes untrusted code, so builds run in a throwaway builder microVM,
never through Docker on the host. VAM's own standard bases are built by the
same pipeline (DX-53C), so there is one way to build an image.

Three tiers keep most people away from builds:

| Tier | What | Covers |
|---|---|---|
| 0 | Standard bases: `vam/base`, `vam/python`, `vam/node`, `vam/jvm` | Most agents, with no build |
| 1 | Skills bring their own npm or pip packages (DX-101) | Language libraries |
| 2 | Vamfile | Everything else |

## III.4 The two journeys

- **In the app:** the New agent flow gains a **Machine** step. The user picks
  a base (Standard, Python, Node, Java) and ticks extra packages. The app
  writes the Vamfile, builds it and shows the build log. They never see the
  Vamfile unless they open the advanced view.
- **As a developer:** write a `Vamfile` in its own project, then run
  `vam image build`. `vam dev` uses the result. `vam image release`
  publishes it, and the agent's `imageRef` points to it.

## III.5 Phases

| # | Phase | Delivers | Depends on |
|---|---|---|---|
| 1 | Standard bases built as OCI images and converted to ext4. A `requiresRuntime` check at install | Missing core packages fixed with no Vamfile | nothing |
| 2 | Image artifact kind, `imageRef` on the agent, draft and release | Images versioned and pinned like everything else | E1 Phases 1 and 3 |
| 3 | Vamfile v1 parser and builder microVM: the keep set plus `RUNTIME`, `PACKAGES`, `TEST`, `BUILD_EGRESS`. Layer cache, ext4 conversion, runtime injection | `vam image build` works | Phase 2 |
| 4 | App Machine step (base, package checklist, live build log) and the CLI `vam image build`/`release` commands | Both journeys work | Phase 3 |
| 5 | Snapshots per image, SBOM, signing, marketplace portability | Fast boot, trusted images | Phase 4, E6 |

Phases 1–4 are the getting-started slice. Phase 1 can start now, in
parallel with E1.

---

# Part IV — Permissions as data

Settled with the product owner on 2026-10-11. This part replaces E1 Phase 5
("Permissions as toggles") and supersedes the policy-stack items in
`developer-experience` Phase 5 (DX-60…68). §IV.12 is the new phase; the
`developer-experience` tracker marks each superseded item.

## IV.1 The problem

In the product owner's words:

1. **Composing.** A common policy such as "allow all tools and all internet"
   should combine with a specific one such as "this tool needs approval",
   which may be written for one skill. Today that means writing a third
   policy that merges the two.
2. **Releasing.** Not every rule should need its own release.

What the code does today (verified on `feat/agent-experience`, 2026-10-11):

- **One policy per agent.** `AgentSpec` carries a single
  `capability_policy` and a single `hitl_policy`
  (`proto/vam/v1/agentic_spec.proto:34-39`).
  `resolve_session_policy` (`crates/vam-orchestrator/src/security.rs`)
  loads the capability policy by URN and **ignores the pinned version and
  digest** (DX-6A). It string-concatenates that policy, a HITL overlay
  generated from `monitored_tools`
  (`vam-security/src/advertise.rs` `hitl_execute_overlay`) and a playbook
  overlay, then compiles one `PolicySet`. `HitlPolicyRef.policy_ref` is
  never loaded.
- **Skills' Cedar is never enforced.** `SkillManifest.policy` and
  `capabilities` exist (`proto/vam/v1/skills.proto`) but nothing checks them
  (DX-62).
- **MCP connections belong to a user, not an agent.** A session exposes every
  connection its owner can see (`list_visible_mcp_connections(owner)`), and
  the guest sees tools as `<connection_id>_<tool>`. An agent author cannot
  list those tools in a policy ahead of time.
- **The per-tool decision already lives with the source.**
  `McpConnection.side_effects` stores a `SideEffectClass` per vendor tool,
  defaulting to mutable external; only an operator may lower it
  (`mcp_registry.rs`). `ToolSpec.side_effect` exists for skill tools, and
  it reaches Cedar as `context.side_effect`.
- **Egress is not Cedar.** The broker allowlist is the union of secrets'
  `allowed_destinations` (`vam-network/src/broker.rs:441-459`,
  `vam-orchestrator/src/brokers.rs`). An agent with no secrets cannot
  reach the internet at all. The schema's `EgressRequest` action is never
  evaluated.
- **Policies are released silently.** The app releases an agent's attached
  policy whenever the agent is released ("Released with <agent> vX.Y.Z").

The root cause is that **Cedar text is the storage format for decisions
that are really data**: "this tool: allow, ask or deny". Every author has to
write, release and attach a policy artifact, which suits a security team
and almost nobody else.

## IV.2 The model: permissions are data, Cedar is the engine

**A rule** is `(target, decision)`. A decision is `allow`, `ask` or `deny`.

| Target | Form | Example |
|---|---|---|
| A skill tool | `skill:<skill name>/<tool>` | `skill:jira/create_issue` |
| An MCP tool | `mcp:<server>/<tool>` | `mcp:mcp.atlassian.com/create_issue` |
| A built-in tool | `builtin:<tool>` | `builtin:run` |
| A whole source | a trailing `*` | `mcp:mcp.atlassian.com/*`, `skill:jira/*` |
| Every tool | `tools:*` | |
| A host | a host pattern | `*.pastebin.com`, `api.github.com` |
| Every host | `internet:*` | |

**MCP rules name the server, not the connection.** Connections are per user
and their ids are chosen by the user, so a rule keyed by the author's
connection id means nothing for a member who runs the released agent. The
host maps each guest tool name to its target at session start. It already
knows each tool's source when it builds the advertised list.

**Scopes.** A rule lives where the thing it governs lives:

| Scope | Stored on | Set by | May |
|---|---|---|---|
| Platform floor | Code | VAM | deny only |
| Org | Org guardrail record (E3) | Security team | deny or ask only |
| Source | Skill manifest; MCP connection row | Skill author; whoever connects the server | ask only, for its own tools |
| Agent | The agent spec, released with it | Agent author | allow, ask, deny |
| Run | Run request (later) | Caller | deny or ask only |

**Resolution.**

1. **Within one scope, the most specific rule wins.** `tools:*: allow` plus
   `skill:jira/create_issue: ask` means everything is allowed except that
   one tool, which asks. No merged third policy.
2. **Across scopes, the strictest wins.** `deny` > `ask` > `allow`. The
   platform, org, source and run scopes can only tighten.
3. **Nothing permitted means denied**, as Cedar does today.

Specificity is resolved in Rust, inside each scope, by one shared
implementation in `vam-security` (`developer-experience` decision 13). The
across-scope rule is left to Cedar's forbid-overrides-permit.

**Defaults for a new agent** (product owner, 2026-10-11):

- `tools:*` is `bySideEffect`: read-only and mutable local are `allow`;
  mutable external and unspecified are `ask`. MCP tools default to mutable
  external, so a new MCP tool asks until someone decides.
- `internet:*` is `allow`.

**Agent JSON** (illustrative; the proto lands in Phase 5):

```jsonc
"permissions": {
  "tools": {
    "default": "bySideEffect",          // allow | ask | deny | bySideEffect
    "rules": [
      { "target": "skill:jira/create_issue", "decision": "ask" },
      { "target": "mcp:mcp.atlassian.com/*", "decision": "allow" },
      { "target": "builtin:run", "decision": "deny" }
    ]
  },
  "internet": {
    "default": "allow",                  // allow | deny
    "rules": [ { "target": "*.pastebin.com", "decision": "deny" } ]
  },
  "advanced": []                         // optional hand-written Cedar (§IV.6)
}
```

## IV.3 How rules compile to Cedar

The host puts each scope's resolved decision on the `Tool` entity as an
attribute, and a small **fixed platform policy set** decides:

```cedar
permit (principal, action == Action::"ExecuteTool", resource)
  when { resource.agent_rule == "allow" || resource.agent_rule == "ask" };

forbid (principal, action == Action::"ExecuteTool", resource)
  when { resource.org_rule == "deny" || resource.source_rule == "deny" ||
         resource.agent_rule == "deny" };

forbid (principal, action == Action::"ExecuteTool", resource)
  when { resource.org_rule == "ask" || resource.source_rule == "ask" ||
         resource.agent_rule == "ask" }
  unless { context has hitl_approved && context.hitl_approved == true };
```

- **The two-pass HITL evaluation does not change.** With `hitl_approved`
  false and then true, the host still gets Allow, Deny or Suspend.
- **The playbook overlay stays** and is appended as today.
- **The Cedar resource id does not change in Phase 5.** It stays the guest
  tool name, and the target is an attribute. Qualifying the resource id
  itself remains DX-68.
- **Schema:** `Tool` gains required `agent_rule`, `source_rule`, `org_rule`
  and `target` attributes. The host always sets all of them, with `"none"`
  when a scope has no rule. A missing attribute would make Cedar skip a
  `forbid`, so they must never be absent. The engine passes entities per
  request; today it passes none.
- **Hosts** compile to `EgressRequest` policies, for example
  `forbid (principal, action == Action::"EgressRequest", resource)
  when { context.host like "*.pastebin.com" };`, plus one `permit` when
  `internet:*` is `allow`. This keeps hand-written guardrails in the same
  language.

## IV.4 Egress in Cedar

The broker evaluates `EgressRequest` for every request. Order:

| # | Check | Where | Changeable by |
|---|---|---|---|
| 1 | Platform floor: private ranges, metadata, loopback, the host. Checked after DNS, which defeats rebinding (`vam-network/src/policy.rs` `is_ip_denied`) | Rust | nobody |
| 2 | Org host rules (E3) | Cedar | security team |
| 3 | Agent host rules, `internet:*` | Cedar | agent author |
| 4 | A secret is substituted only for its own `allowed_destinations` | Rust | the secret's binding |
| 5 | Forward through the enterprise proxy, if configured (E8) | Rust | org config |

Step 3 replaces the secret-union allowlist. An agent with no secrets and
the default `internet:*: allow` can now reach the public internet. "Ask" for
hosts is later (§IV.9). Suspending mid-request needs the approvals path in
the broker.

## IV.5 Skills and MCP sources

- **Skills ship data, never Cedar.** `SkillManifest.policy` is deprecated
  and ignored. The structured `capabilities` list stays for consent.
- **A skill may mark its own tools `approvalRequired`.** This is a new
  `ToolSpec` field. It becomes a source rule of `ask` for that tool only.
  Data can reach no tool but the skill's own, so it cannot escalate or
  disable another skill's tools. DX-62's scoped-permit validator is not
  needed.
- **MCP sources** take the stored side-effect class as the input to
  `bySideEffect`. A connection may also carry `ask` source rules. For an
  org connection, an admin sets them. For a user connection, the user does.

## IV.6 Shared policies, presets and hand-written Cedar

- **Presets** ("Read-only", "Open internet", "Ask before writes") are
  starting rule sets. They are **copied** into the agent, never referenced,
  so they need no release. Squrell ships the first ones.
- **Hand-written Cedar stays** in two places:
  - **Org guardrails** (E3): forbid-only, released as an org artifact.
  - **Agent `advanced` attachments**: a released policy artifact the agent
    references. It is added to the same policy set, frozen to
    `version + digest` at release (Phase 3), and **enforced at session
    start**, failing closed on a digest mismatch (DX-6A). It must not
    hard-code an agent principal; publish rejects
    `principal == Agent::"…"` in a policy marked shareable.
- **The app stops auto-releasing policies.** Inline rules release with the
  agent. An advanced attachment that only exists as a draft fails release
  with the Phase 3 message, and the app offers to release both together.

## IV.7 Saving decisions as you go

When a HITL prompt appears:

| Who, which run | Options | Effect |
|---|---|---|
| The owner, on a draft run | Allow once, Always allow, Always deny | "Always" writes an agent rule for that target into the draft |
| Anyone, on a released version | Allow once, Deny | Nothing is saved |

- "Always allow" is **an agent rule, not a user preference**. When the
  agent is released, the tool is allowed for everyone who runs that
  version.
- **It is offered only when it can take effect.** If a skill's
  `approvalRequired` or an org rule forces `ask`, the prompt offers only
  *Allow once* and names the rule.
- **A personal agent is a draft that is never released.** Its owner keeps
  running it, and their "always" choices build up in it.

## IV.8 Release, versioning and pins

- **Release shows what changed** in permissions: "Now allowed without
  asking: `jira_create_issue` (was: ask)", "Now denied: `*.pastebin.com`".
- **Loosening is at least a minor bump** (§II.4: an added capability). Is
  tightening major? Open question 1.
- **Pins.** Release freezes every reference, `advanced` policy refs
  included, to `version + digest` (Phase 3, AX-3.3). The runtime loads
  exactly that version and checks the digest. This closes DX-6A for every
  policy that is still referenced.
- **Inline rules are part of the agent's digest**, so they need no pin of
  their own.

## IV.9 App and CLI

**App (agent → Permissions):**
- A row per tool, grouped by source, with Allow / Ask / Deny. Defaults are
  marked "default (read-only)".
- Internet: "Any site" or "Only these", plus a block list.
- **An effective-permissions view** (allowed, asks, denied) built on
  `classify_tools`, with each row naming the rule that decided it. A rule
  from a skill or the org shows as locked, with its reason.
- A preset picker on new agents.
- An "Advanced Cedar" panel, read-only, listing attachments. *(Later.)*

**CLI:**
```
$ vam policy explain jira-assistant jira_create_issue
ASK   jira_create_issue  (skill:jira/create_issue)
  agent  allow   skill:jira/*  (agent v1.3.0)
  source ask     approvalRequired  (skill jira v2.1.0)     <- decides
  org    none

$ vam policy explain jira-assistant --host pastebin.com
DENY  pastebin.com   agent rule *.pastebin.com at agent v1.3.0
```
`vam agent permissions get|set` edits a draft's rules (JSON in and out, per
`developer-experience` decision 2).

## IV.10 What goes to E3

**Org guardrails** are an org-scope rule set: deny or ask by target, by
side-effect class ("ask for anything mutable external") or by host. They
live in one org record, read by the same resolver. The governance console
owns their editor, release and audit trail. Later: hand-written forbid-only
Cedar guardrails, and an allowlist-only internet mode. The principals added
by Apps/Spaces (`design/vam-apps-spaces.md`) must be reconciled before org
guardrails ship (`developer-experience` §17.1 Q5).

## IV.11 Migration

Released versions are immutable, so nothing in them is rewritten.

**Released agents (frozen refs).** Once AX-3.3 lands, a released agent
holds `capability_policy.policy_ref` frozen to `version + digest`, plus
`hitl_policy.monitored_tools` (and a `hitl_policy.policy_ref`, possibly
frozen, that was never loaded). They run on a **compatibility path** that
keeps today's behaviour exactly:

- The frozen capability policy loads as an `advanced` attachment **at its
  pinned version and digest**. A mismatch fails closed (DX-6A). A
  pre-Phase-3 ref with no digest loads the latest release, as today, with a
  warning in the run record.
- `monitored_tools` become agent `ask` rules for those tools.
- **No new defaults are applied.** Tools follow the legacy Cedar alone,
  with no `bySideEffect` and no `internet:*`.
- Egress keeps the secret-union allowlist, compiled into `EgressRequest`
  permits for those destinations.
- `hitl_policy.policy_ref` is ignored, as it is today.

**Drafts.** When an owner opens a legacy draft (app banner or
`vam agent migrate`):

- `monitored_tools` become `ask` rules.
- The capability policy is converted if every statement is one of the
  known shapes: a permit over a tool list, the same gated by
  `hitl_approved`, or a forbid over a tool list. It becomes agent rules,
  with `tools:*: deny` as the default so the meaning is unchanged.
  Anything else stays an `advanced` attachment, and the banner says so.
- The next release carries `permissions` and no `capability_policy`. The
  release dialog's diff shows any change in behaviour.

**Proto.** `AgentSpec` gains `AgentPermissions permissions`.
`capability_policy` and `hitl_policy` are deprecated, read only on the
compatibility path, and rejected on new drafts once the app writes
`permissions`. `telemetry_policy` and `completion_policy` are out of scope.

## IV.12 Phases (E1 Phase 5)

E1 Phase 5 is rewritten as **Permissions as data**. It depends on Phase 3
(frozen refs) and Phase 4 (attach skills by name).

**Getting-started slice (all of these):**

| # | Item | Repo |
|---|---|---|
| 5.1 | `AgentPermissions` in `vam-spec` (proto + JSON), the target grammar, and the resolver in `vam-security` (specificity per scope, `bySideEffect`) | vam |
| 5.2 | Fixed platform policy set, schema `Tool` attributes, entities per request; `classify_tools` returns the deciding scope and rule | vam |
| 5.3 | Session assembly from `permissions`; the compatibility path with pin and digest enforcement (DX-6A); the egress broker calls Cedar `EgressRequest` in place of the secret-union allowlist | vam |
| 5.4 | `ToolSpec.approvalRequired` as a source rule; `SkillManifest.policy` deprecated | vam |
| 5.5 | "Always allow / Always deny" on a draft run writes the agent rule; API plus the approval prompt in the app | vam, the VAM app |
| 5.6 | Permissions page with toggles, presets, effective view, migration banner | The VAM app |
| 5.7 | Release diff for permissions and the bump rule; app stops auto-releasing policies | vam, the VAM app |
| 5.8 | `vam policy explain` (tool and `--host`) and `vam agent permissions` | vam |
| 5.9 | Verify on a KVM host: open internet with no secrets, the HITL suspend, "always allow" then release, a legacy released agent unchanged | — |

**Later (not in the slice):** read-only advanced Cedar view; org presets as
artifacts; "ask" for hosts; run narrowing; `vam policy test` (DX-66); Cedar
templates for advanced policies (DX-67); qualified Cedar resource ids
(DX-68).

## IV.13 Decisions

1. **Permissions are data. Cedar is the engine.** Authors of normal agents
   never write Cedar.
2. **A rule is `(target, allow | ask | deny)`.** Scopes: platform floor,
   org, source, agent, run.
3. **Within a scope the most specific rule wins. Across scopes the
   strictest wins.** One resolver in `vam-security`.
4. **Rules compile to a fixed platform policy set plus per-scope attributes
   on `Tool` entities.** Host rules compile to `EgressRequest` policies.
5. **Agent rules live in the agent and release with it.** No separate
   policy release. The app stops auto-releasing policies.
6. **Defaults:** tools by side-effect class (read-only and mutable local
   allow; mutable external and unspecified ask), and `internet:*` allow.
7. **"Always allow / deny" from an approval prompt writes an agent rule into
   the draft**, for the owner on a draft run only. On a released version,
   approvals are once only. It is not offered when a stricter scope
   decides.
8. **Release shows permission changes. Loosening is at least a minor bump.**
9. **Skills ship no Cedar.** A skill may mark its own tools
   `approvalRequired` (tighten-only). This amends `developer-experience`
   decisions 6 and 7.
10. **MCP rules target the server, not the user's connection.**
11. **The broker evaluates egress in Cedar.** The platform IP floor and
    per-secret destinations stay as hard limits outside it.
12. **Hand-written Cedar remains** for org guardrails (E3, forbid-only) and
    `advanced` agent attachments. Both are frozen at release, and the
    runtime enforces the pin (DX-6A).
13. **Presets are copied in, not referenced.**
14. **Released legacy agents keep their exact behaviour** through a
    compatibility path. Drafts migrate on open.
15. **The effective-permissions view and `vam policy explain` ship with the
    getting-started slice.** Adding a rule can change the result, so users
    must see the result.
16. **Placement:** E1 Phase 5 is this part. Egress engine and secrets are E8
    (Part V). Org guardrails are E3.

## IV.14 Open questions

1. Is tightening (allow → ask, or anything → deny) a major bump, as §II.4
   reads ("a revoked capability"), or a minor one?
2. Should a member be able to remember an approval for themselves on a
   released agent ("always allow for me")? Deferred; the product owner chose
   "once" for now.
3. MCP server identity for targets: host only, or origin plus path (two MCP
   servers on one host)? Proposal: origin plus path, normalised.
4. Guest tool names collide across sources until DX-68. Is the existing
   visible-name collision check enough for Phase 5?
5. "Ask" for hosts: how does a suspended HTTP request behave (timeouts,
   retries by the client)?
6. Can an org author presets as shareable artifacts, or are they
   Squrell-only at first?
7. Who may edit source rules on an org-scoped MCP connection: any admin, or
   the connection's owner?

---

# Part V — E8: egress and secrets

Settled with the product owner on 2026-10-11. E8 runs in parallel with E1.

## V.1 The problem

- **Standard HTTPS clients do not work.** The guest proxy on
  `127.0.0.1:3128` turns plain HTTP into `EgressRequest` messages over
  vsock, and the host substitutes `vamsec_` placeholders. HTTPS `CONNECT` is
  refused with a 501 (`vam-runtime/src/egress.rs:269`), because a tunnel
  hides the headers the host must rewrite. Bun and Node skills have to call
  the proxy explicitly with `X-Target-Url` (`vam-runtime/src/exec.rs:535`).
- **SDKs that check a key's format** (`sk-…`, `ghp_…`) reject a
  placeholder.
- **Enterprises already run a secure web gateway** (Netskope, Zscaler,
  Prisma) with URL categories, DLP and logging. VAM should not copy those.

## V.2 The model

1. **A TLS-terminating egress proxy on the host.**
   - A CA is minted per session; its key never leaves the host. The CA
     certificate is sent to the guest at boot and added to the trust store,
     with `SSL_CERT_FILE`, `NODE_EXTRA_CA_CERTS` and `REQUESTS_CA_BUNDLE`
     set.
   - The guest uses ordinary `HTTPS_PROXY`. Later, the TAP device routes
     port 443 to the proxy transparently.
   - The host opens the TLS connection, evaluates `EgressRequest` (§IV.4),
     substitutes placeholders in headers and the URL (and the body where a
     binding says so), then opens a fresh TLS connection upstream. It
     verifies the upstream against standard roots, plus the org CA when one
     is configured.
2. **`CONNECT` passthrough** for non-HTTP traffic (ssh, postgres) and for
   clients that pin certificates. Cedar checks `host:port`. No secret is
   injected.
3. **Inject by destination.** A binding may say "requests to
   `api.github.com` get `Authorization: Bearer {secret}`". The skill then
   sends no credential at all. Placeholders stay for SDKs that refuse to
   start without a key. The security is the same, since anything in the VM
   could already use the placeholder.
4. **Upstream enterprise proxy.** An org setting `egress.upstreamProxy`
   (URL, auth, CA bundle) chains all outbound traffic through the corporate
   gateway. VAM adds identity headers (default `X-VAM-Agent`, `X-VAM-Run`,
   `X-VAM-User`, names configurable) or uses proxy auth, so the gateway's
   logs say which agent and run made a call. VAM handles per-agent intent;
   the gateway handles company-wide hygiene.

## V.3 Phases

| Phase | Goal | Getting started |
|---|---|---|
| 1 | Host TLS-terminating proxy and per-session CA in `vam-network`; `CONNECT` passthrough with `host:port` check | yes |
| 2 | Guest wiring: CA in the trust store at boot; `HTTPS_PROXY` for every runtime, Bun and Node included; `X-Target-Url` kept as deprecated compatibility; rootfs update | yes |
| 3 | Inject-by-destination bindings: `SecretBinding` attach mode, vault and app UI, `secret:` capability with its destination | yes |
| 4 | Upstream enterprise proxy with identity headers; docs for Netskope and Zscaler | later |
| 5 | Egress audit events with decision attribution (telemetry), "ask" for hosts, transparent port-443 routing | later |

Each phase that changes the guest ends with a KVM verify issue: curl,
Python `requests`, Node `fetch` and a Go binary reach an HTTPS API, with a
secret injected and the guest never seeing it.

## V.4 Running in parallel with E1

- E1 Phase 5 (item 5.3) changes one call site in `vam-network/src/broker.rs`
  `handle_egress`: the allowlist check becomes a Cedar `EgressRequest`
  decision. E8 Phase 1 moves that call into the new proxy path. Whichever
  lands second rebases onto the other. No other file is shared.
- E8 needs nothing else from E1, and E1 needs nothing from E8: with the
  current proxy, the new egress rules apply to plain HTTP and to the
  structured requests skills already make.
- `docs/src/content/docs/network/policy.md` is stale on egress and is
  rewritten in E8 Phase 1.

## V.5 Open questions

1. Transparent port-443 routing versus `HTTPS_PROXY` only. Proposal:
   environment first, transparent in Phase 5.
2. Runtimes with their own trust store (the Java keystore). Add the CA at
   boot with `keytool`, or document it?
3. Which hosts get passthrough by default (known certificate-pinning
   clients), and who may add to the list: agent author or org only?
4. **Guest proxy wildcard bind** (found by E9, 2026-10-18). The guest's
   egress proxy (`crates/vam-runtime/src/egress.rs`, `start_guest_proxy`)
   binds `127.0.0.1:3128` and, if that port is taken, falls back to
   `0.0.0.0:3128`, exposing it on the guest's TAP interface. Low risk
   today (the proxy only forwards to the host broker, which enforces
   policy), but it should never bind a non-loopback address. Proposal:
   fix in **Phase 2** (guest wiring, which rewrites the proxy advertisement
   anyway): fail closed to the unix socket, or retry another loopback port
   and advertise it; add a test that an occupied loopback port never yields
   a non-loopback listener. E9's process-guest build already compiles the
   fallback out.

---

## What we are not building

- No semver prereleases (`1.3.0-rc.1`) yet. A private draft covers the
  inner loop. Revisit if teams need to share work in progress before
  release.
- No collaborators on a draft. One owner per artifact, as today.
- No Squrell marketplace backend, signing or review in Epic 1.
- No new storage layer. Drafts and versions stay in the existing catalog
  tables.
- No new policy engine. Cedar stays the engine. Part IV changes what people
  author (rules as data), not how decisions are evaluated.

## Open questions

1. The Squrell marketplace domain. `hub.squrell.cloud` is a placeholder.
2. Can someone who is not a developer publish to the Squrell marketplace,
   or does that need a publisher role? (Product owner: "to think about".)
3. Does a team namespace arrive with Apps/Spaces (`vam-apps-spaces.md`), or
   before?
4. When the version bump is suggested, does a changed playbook count as
   minor or as patch?
5. E7: which package manager the bases use (Alpine `apk` matches today's
   musl rootfs; Debian `apt` has wider package coverage). `PACKAGES` takes
   the manager explicitly, so this can change later.
6. E7: who may build images. Building runs untrusted `RUN` code inside a
   builder VM, but it still spends host capacity. Publisher role or
   everyone?
