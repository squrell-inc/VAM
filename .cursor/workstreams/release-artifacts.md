# Workstream: Release artifacts (version, tag, build, publish, install)

```yaml
status: active
slug: release-artifacts
current_phase: 0 (plan approved 2026-11-17; breakdowns of all phases drafted 2026-11-17, waiting for the owner)
base_branch: main (cross-cutting; one release PR per phase)
design_docs:
  - design/release-artifacts.md
  - docs/src/content/docs/getting-started/installation.md
```

## Goal

A published VAM release is one version for the whole machine, built by one manually started,
owner-approved workflow from one green commit on `main`, published under an immutable `v*` tag
with `SHA256SUMS`, SBOMs and GitHub attestations. A user installs `vam` on Linux, macOS or
Windows with one command and upgrades it with another, and installs a node (x86_64 Linux, WSL2,
then aarch64 Linux for Lima on Apple Silicon) from a node bundle without a Rust toolchain or a
git checkout, brokers included.

## Out of scope

- crates.io, Homebrew, winget, apt.
- In-place node upgrade (`vam node upgrade`) with state migration and rollback.
- A `vamd` container image (design §5.2).
- Publishing `rootfs.ext4`, Firecracker or the guest kernel (the node builds the image from the
  shipped recipe and fetches the rest from pins; a published image waits for a licence review).
- The Vamfile builder itself (`agent-experience` E7 Phase 3); this workstream only keeps the
  shipped recipe inside its grammar.
- Catalog item versions (agents, skills, playbooks have their own semver).

## Locked decisions

Do not re-litigate without editing this list. **Locked 2026-11-17 (owner approved the plan).**

1. One semver version for the workspace in `[workspace.package]`; every crate inherits it
   (`vam-firecracker` included). Tags are `v<version>`. Pre-release label `-rc.N` only.
2. The version is chosen at release time, never guessed ahead: a prepare PR
   (`scripts/release/prepare.sh <v>`, title `release: prepare v<v>`) sets it just before the
   release, and its merge is the commit that is tagged. No post-release bump PR. Pre-1.0: a minor
   may break; a patch only fixes. The workflow never pushes to `main`.
3. A build is a release build only when the release workflow sets `VAM_BUILD_CHANNEL=release`.
   Every other build reports the nearest `v*` tag plus build metadata (`0.3.0+dev.14 (<sha>)`),
   never a future version. One build-info source in `crates/vam-spec/build.rs`, with
   `VAM_GIT_SHA` / `VAM_COMMIT_DATE` overrides. `vam`, `vamd`, each broker and `GET /v1/node`
   report version and commit the same way; `vam node status` shows the node's and the CLI's.
4. Releases start only by `workflow_dispatch` (`version`, `commit`, `dry_run` default `true`)
   from `main`, and publish only after the owner approves the protected `release` environment
   (owner as required reviewer, `main` only, admin bypass off). No tag-on-merge.
5. Guards before any build: commit on `main`'s first-parent history; the `rust.yml` push run on
   that SHA succeeded; `Cargo.toml` version equals the input; tag and release absent; version
   greater than every `v*` tag; release gate passes; inputs reach shell only through `env:`.
6. Tags are immutable: immutable releases on; a `v*` ruleset restricts updates and deletions with
   no bypass. A tag is never moved, deleted or reused; a failed publish takes the next number.
7. The workflow creates the tag via a draft release at the exact commit, uploads every asset,
   attests, verifies, then publishes. Nothing is public before the owner's approval.
8. Native GitHub-hosted standard runners per target; `cargo-zigbuild` only for the Linux glibc
   floor; no `cross`, no `cargo-dist`. No caches in release builds. Actions pinned by full SHA.
9. Build jobs have read-only permissions and no secrets; the publish job holds only
   `GITHUB_TOKEN` with `contents: write`, `id-token: write`, `attestations: write`.
10. Provenance: `SHA256SUMS`, GitHub artifact attestations (build provenance and CycloneDX SBOM),
    the immutable release attestation, `cargo auditable` binaries. No long-lived signing key.
11. Shipped: node bundles (`vamd`, `vam`, `vam-web-broker`, `vam-schedule-broker`,
    `vam-notify-broker`, `vam-runtime`, the standard base recipe) for x86_64 Linux gnu, and
    aarch64 Linux gnu from Phase 4; `vam` alone for Linux x86_64/aarch64, macOS arm64/x86_64 and
    (Phase 3) Windows x86_64; the guest init as static musl, built exactly as `build-rootfs.sh`
    builds it. Not shipped: `hsp-echo-broker`, `vam-e2e`, `verify_chain`.
12. No `CHANGELOG.md`: release notes are generated from the first-parent `release:` merges and
    their deploy notes, and reviewed at the approval gate.
13. No image is shipped. The node bundle carries the standard base recipe (`share/vam/base/`)
    and the attested `vam-runtime`; `build-rootfs.sh --runtime PATH` injects it as `/sbin/init`
    without compiling. The recipe stays inside the planned Vamfile grammar and becomes the first
    standard Vamfile when E7 Phase 3 lands (`vam image build` replaces `build-rootfs.sh`; the
    bundle layout does not change).
14. Brokers run as their own systemd units, installed and registered by
    `vam node init --broker <names>`; their credentials stay in the vault, never in a unit or its
    environment.
15. `vam dock` puts `vam` on the user's `PATH` without admin rights (per-user directory,
    marked profile lines on Linux/macOS, the user `Path` on Windows); `vam upgrade` checks the
    hash and the attestation before replacing itself and never touches `vamd`. The CLI never calls
    the network unasked.

## Status legend

- `[ ]` not started
- `[~]` in progress
- `[x]` done (code + tests, or merged)

## Execution plan (all phases)

Drafted 2026-11-17 by the phase-breakdown agent. The owner asked for the plan of every phase in
one pass, with parallel work wherever it is safe. That overrides "one phase per breakdown" and
"one wave at a time" (playbook §3) for this workstream only. The lane conflict rule still holds
(playbook §11.2): two lanes never build in the same file at the same time. The breakdowns of
Phases 3–6 are **drafts**: the orchestrator re-validates each against the code at its phase start
and brings any change to the owner before a wave starts.

### Branches

One base per phase. Every base is `feat/…`, because `rust.yml` treats a PR into `main` from
`feat/*` as a release PR and forces the microVM lane (rust.yml:69-77, 578-595).

| Base | Cut from | When | Carries | Release PR into `main` |
|---|---|---|---|---|
| `feat/release-artifacts-p0` | `main` | now | Phase 0, plus the early Phase 1 waves P1.W1 (guards, `release.yml` with `validate` only) and P1.W2 (`build-guest.sh`) | `release: release-artifacts P0 (+P1 W1-W2)`. After it, `release.yml` exists on `main`, so later Phase 1 waves can dispatch dry runs from their branches (Phase 1 open question 1) |
| `feat/release-artifacts-p1` | `main` | after the P0 release merges | Phase 1 W3, W4 | `release: release-artifacts P1`; then the recorded dry run from `main` |
| `feat/release-artifacts-p2` | `main` | after the P1 release and its dry run | Phase 2 | `release: release-artifacts P2`; then the prepare PR `0.1.0-rc.1` and the first publish |
| `feat/release-artifacts-p3` | `main` | after the P0 release (most Phase 3 waves need no published release) | Phase 3 | opens only after rc.1 is published |
| `feat/release-artifacts-p4` | `feat/release-artifacts-p3` | once P3.W2, P3.W3 and P3.W5 are merged there | Phase 4 | opens only after the P3 release merges |
| `feat/release-artifacts-p5` | `main` | after the P0 release (W0-W2 need no published release) | Phase 5 | opens only after rc.1 is published |
| none | — | — | Phase 6: the prepare PR `release/prepare-v0.1.0` and one docs PR into `main` | — |

- **Wave PRs.** Branch `ra-p<N>-w<W>`, worktree under `.claude/worktrees/`, PR into the phase
  base, title `RA-<N> W<W>: <summary>`, ready for review (Actions is the gate). Merge commit only
  (`gh pr merge --merge --match-head-commit <sha>`), never squash (playbook §4).
- **Release PRs.** Merge commit, on the owner's word. No wave merges into a base while that base's
  release PR is open (the PR would pick it up). P1.W1 and P1.W2 merge into the P0 base before the
  P0 release PR opens.
- **After each release merge.** Every open base takes `main` by merge, never rebase (working
  agreement). The base of the closed phase is deleted (its history is in `main`). The P4 base
  takes `main` after the P3 release, so its release PR shows Phase 4 only.
- **Prepare PRs.** Branch `release/prepare-v<v>` from `main`, title `release: prepare v<v>`, only
  `Cargo.toml` and `Cargo.lock`, merge commit on the owner's word. It is not a `feat/` branch, so
  the PR does not force the lane; the push run on `main` after the merge does (push runs always run
  the lane), and guard 3 reads that run. Open a prepare PR only when no other PR into `main` is
  open, so the prepare merge is the head that gets tagged.
- **Dispatch.** A publish runs only from `main` (guard 1). So Phase 1's recorded dry run runs after
  the P1 release merges, and Phase 2's publish after the P2 release and the rc.1 prepare PR.

### Parallel lanes

A lane is one background agent at a time, each wave in its own worktree. All worktrees share
`CARGO_TARGET_DIR=/home/user/vam/target` (working agreement), so concurrent cargo builds in one
container wait on Cargo's build-directory lock: run at most **two building lanes** at once there
(script, workflow and docs waves build little), or run lanes as separate cloud sessions.

```text
 P0 base (now)                         P1 base               P2 base                                   after rc.1
 A  P0.W0 ─► P0.W1 ─► P0.W2 ─┐
 B  P0.W3 ─► P1.W1 ──────────┼─► P0 release ─► P1.W3 ─► P1.W4 ─► P1 release ─► dry run ─► P2.W1 ─► P2 release ─► prepare rc.1 ─► publish rc.1
 C  P1.W2 ───────────────────┘       │                                          ▲                                              │
 O  owner: §9 settings, public scratch repository, RA-2.1 ──────────────────────┘ (before P2.W1 merges)                         │
                                     │                                                                                         ▼
 D (P3 base)                         └─► P3.W0 ─► P3.W1 ─► P3.W2 ─► P3.W3 ─► P3.W4 ─► P3.W5 ...... P3.W6* ─► P3.W7 ─► P3 release ─► rc.2
 E (P5 base)                         └─► P5.W0 ─► P5.W1 ─► P5.W2 .................................. P5.W3* ─► P5.W4 ─► P5 release ─► rc.3
 F (P4 base, from P3 base after P3.W5)                     P4.W1 ................................. P4.W2* ─► P4.W3 ─► Lima run ─► P4 release ─► rc.4
                                                                                                    (after the P3 release)
 all rcs settled (owner's word) ─► P6: prepare 0.1.0 ─► publish v0.1.0 ─► docs PR

 * edits release.yml: runs after P2.W1 is merged, in the order P3.W6 ─► P5.W3 ─► P4.W2 (one at a time)
```

- Lanes B and C run beside lane A from the start: the guard script, the workflow skeleton and the
  shared guest build need no build info (P1.W1 needs only P0.W3's `lib.sh`).
- Lane D: the Windows gating (P3.W1), `--runtime` (P3.W2), `--kit` (P3.W3) and the broker version
  and registration (P3.W4, P3.W5) need no published release; only the bundle packaging (P3.W6)
  waits for `release.yml` order, and the phase close waits for rc.1.
- Lane E: `vam dock` (P5.W1) and `vam upgrade` against a fixture release server (P5.W2) need no
  published release. The real upgrade is proven only from an rc that already holds `vam upgrade`
  to a later one (Phase 5 open question 2).
- Lane F: Phase 4's pins and per-architecture asset choice (P4.W1) start once the kit layout
  (P3.W3) and the `node_install.rs` changes of P3.W5 are merged into the P3 base.
- At most two building lanes at once: in practice A+B, then D+E, then F with whatever is left.

**Files that serialise waves** (a later wave starts after the earlier one's PR is merged into its
base; when the two sit on different bases, the later release PR merges `main` first and a wave
session resolves any conflict):

| File | Order |
|---|---|
| `.github/workflows/release.yml` | P1.W1 ─► P1.W3 ─► P1.W4 ─► P2.W1 ─► P3.W6 ─► P5.W3 ─► P4.W2 |
| `.github/workflows/rust.yml` | P0.W3 ─► P1.W1 ─► P3.W1 ─► P3.W2 ─► P4.W2 |
| `scripts/ci/local.sh` | P0.W3 ─► P1.W1 ─► P3.W2 |
| `crates/vam-cli/src/main.rs` | P0.W1 ─► P5.W1 |
| `crates/vam-cli/src/cmd/mod.rs` | P3.W1 ─► P5.W1 |
| `crates/vam-cli/src/cmd/node.rs` | P0.W2 ─► P3.W1 ─► P3.W4 ─► P3.W5 |
| `crates/vam-cli/src/cmd/node_install.rs`, `node_init.rs` | P3.W3 ─► P3.W5 ─► P4.W1 |
| `proto/vam/v1/orchestrator.proto` | P0.W2 ─► P3.W4 |
| `scripts/build-rootfs.sh`, `scripts/e2e/firecracker-assets.sh` | P1.W2 ─► P3.W2 ─► P4.W1 |
| `crates/vam-e2e/src/{cli.rs,scenario.rs}`, `e2e/README.md` | P0.W0 ─► P3.W0 ─► P5.W0 |
| `Cargo.lock` | P0.W3 (only if an advisory needs `cargo update`) ─► P5.W1 ─► P5.W2; prepare PRs only while no PR with a lock change is open against `main` |
| `docs/.../getting-started/cli.md`, `releases.md` | P0.W2 ─► P3.W7 ─► P5.W4 (whichever of P3.W7 and P5.W4 runs first creates `releases.md` and its sidebar entry) |

### Owner gates

1. Approve this breakdown (all phases) and answer or accept the open-question defaults.
2. Approve each wave plan after the orchestrator validates it (standing rule).
3. Any time before P2.W1 merges: the one-time settings of design §9 (environment `release`,
   immutable releases, `v*` ruleset, SHA-pinned actions policy), and a **public** scratch
   repository for RA-2.1 (artifact attestations on a private repository need GitHub Enterprise
   Cloud).
4. Each release PR into `main`: P0 (with P1.W1-W2), P1, P2, P3, P5, P4 (merge commit).
5. Each prepare PR (`0.1.0-rc.1` … `0.1.0`).
6. Dispatch: dry runs may be started by the orchestrator when its token can (`gh workflow run`),
   else by the owner; every publish is dispatched by the owner.
7. Approve each `release` deployment after reading the summary (guards, asset table,
   `SHA256SUMS`, notes): rc.1, rc.2, rc.3, rc.4, any fix rc, `v0.1.0`.
8. The team checklist at each phase close (Phases 0-5).
9. Phase 4: the recorded Lima run on an M3 or newer Mac.
10. Phase 6: the word that the rcs have settled, after the rc check on a server, WSL2, Lima and a
    laptop CLI on each OS.
11. Re-approve each drafted breakdown (Phases 3-6) after the orchestrator re-validates it.
12. As always: a model step-up, a third fix cycle, any change to a locked decision.

### Release candidates

| Release | Published after | Contains |
|---|---|---|
| `v0.1.0-rc.1` | Phase 2 close | Phases 0-2. The x86_64 node bundle (`vamd`, `vam`, three brokers, `vam-runtime`; no kit yet, so a node still needs a checkout or `--base`), `vam` for Linux x86_64 and aarch64 and macOS arm64 and x86_64, the guest `vam-runtime` (musl), `vam-proto`, `SHA256SUMS`, SBOMs, notes, attestations |
| `v0.1.0-rc.2` | Phase 3 close | + `vam.exe`; the kit (`share/vam/base/`) in the node bundle; `vam node init --kit`, `--broker`; `build-rootfs.sh --runtime`; broker versions in `vam node status`; the releases docs page |
| `v0.1.0-rc.3` | Phase 5 close | + `install.sh`, `install.ps1`; `vam dock`, `vam undock`, `vam upgrade`. The first rc a later `vam upgrade` can start from |
| `v0.1.0-rc.4` | Phase 4 close | + the aarch64 node bundle and guest init; the Lima page; the first real `vam upgrade` (rc.3 to rc.4), recorded |
| `v0.1.0` | Phase 6 | prepare `0.1.0`; notes cover every rc; `vam upgrade` from the last rc, recorded |

rc.2 to rc.4 follow the order the phases close; the numbers above assume P3, then P5, then P4. A
broken rc is followed by the next number (decision 6); fix rcs shift the numbers.

### Estimated wall time

Agent and CI time per wave: its budget plus about 30 minutes for the Actions run (lane included
where the wave touches a VM path), review and merge. A branch dry run of `release.yml` takes about
40 minutes (Phase 1 open question 8). Owner waits are not counted.

| Phase | Waves | Release and rc steps | Wall time with the lanes |
|---|---|---|---|
| 0 (+P1.W1, W2) | lane A 7 h; lanes B (5 h) and C (2 h) beside it | release PR 45 min | about 8 h |
| 1 | W3 5 h, W4 4.5 h (two branch dry runs each) | release PR, recorded dry run 1.5 h | about 11 h |
| 2 | W1 4 h (with the scratch rehearsal) | release PR, prepare PR, dry run, publish, verify 3.5 h | about 7.5 h + approval |
| 3 | W0-W5 about 14 h inside the Phase 1-2 window (lane D) | after rc.1: W6, W7, release, rc.2 about 8 h | about 8 h after rc.1 |
| 5 | W0-W2 about 9 h inside the Phase 1-2 window (lane E) | after rc.1: W3, W4, release, rc.3 about 11.5 h | about 11.5 h after rc.1, beside P3 |
| 4 | W1 2.5 h inside the P3 window | after rc.1: W2 (after P5.W3), W3, Lima run, release, rc.4 | about 16.5 h after rc.1 (the critical path) |
| 6 | docs 1 h | rc check (owner), prepare, publish, verify about 3 h | about 4 h + the settle window |

- **Whole workstream with the lanes:** about 47 hours of agent and CI time on the critical path
  (P0 8 h, P1 11 h, P2 7.5 h, rc.1 to rc.4 16.5 h, P6 4 h). One wave at a time it would be about
  88 hours; the lanes save about 40.
- **Calendar:** at 8-10 hours of orchestrated work a day and owner gates answered the same day,
  rc.4 in about 5-6 working days; `v0.1.0` after the owner's settle window (Phase 6 open
  question 1), about two weeks from approval.

## Phases

### Done phases

Done phase sections are deleted; git history keeps them.

| Phase | Name | Done |
|---|---|---|

### Phase 0 — Version identity

**Goal:** every binary and the node say which version and commit they are, the same way, and a
dev build never claims to be a release or names a future version.  
**Depends on:** none  
**Plan:** drafted 2026-11-17 (breakdown), waiting for the owner  
**Exit:** the phase's scenarios are active and green on Actions; `vam --version`,
`vamd --version`, `vam-web-broker --version`, `vam-schedule-broker --version`,
`vam-notify-broker --version` print `<name> <version>[+dev.N] (<sha>[+dirty][ <date>])`;
`GET /v1/node` and `vam node status` show `build.version`, `build.commit`, `build.channel`;
`scripts/release/prepare.sh` sets the version; `cargo deny check` passes with advisories and bans.

- [ ] Build info in `crates/vam-spec/build.rs` (`VAM_VERSION` from `git describe` of the nearest
      `v*` tag, else `Cargo.toml` + `+dev`; `VAM_GIT_SHA`, `VAM_COMMIT_DATE`,
      `VAM_BUILD_CHANNEL`, env overrides); `vam_spec::build_info()`; fold `crates/vam-cli/build.rs`
- [ ] `vamd --version` (no config read, no side effects); `--version` on the three brokers
- [ ] `GET /v1/node` `build` object (proto, OpenAPI, shape test); `vam node status` prints the
      node's and the CLI's version and warns on a minor mismatch
- [ ] `hsp.rs:489` `broker_version`, `vam_core::VERSION` and the marketplace `node_version` from
      build info; `vam-firecracker` inherits the workspace version
- [ ] `scripts/release/prepare.sh <version>` (workspace version + `Cargo.lock` entries, refuses a
      version not greater than the latest tag)
- [ ] `deny.toml` `[advisories]` and `[bans]`; `cargo deny check` in the release gate
- [ ] Docs: `getting-started/cli.md`, `orchestrator/api.md` (node), `orchestrator/overview.md`

<!-- Breakdown 2026-11-17 (phase-breakdown agent, Opus 5.5 high). -->

#### Code map (2026-11-17)

- `crates/vam-cli/build.rs:13-25` makes `VAM_GIT_SHA` (short SHA, `+dirty`, `unknown`), with no
  environment override; `main.rs:9` prints `CARGO_PKG_VERSION (VAM_GIT_SHA)`.
- `crates/vam-spec/build.rs:1-64` compiles the protos and nothing else. Every binary depends on
  `vam-spec`. A `cargo:rustc-env` from that build script reaches `vam-spec` only, so the build info
  must be read inside `vam-spec` (`env!` there) and exposed as a function or constant.
- Watching git from `vam-spec`'s build script makes every commit rebuild `vam-spec` and every crate
  above it; today only `vam-cli` rebuilds (open question 1).
- `vamd.rs:157` calls `dotenvy::dotenv()` first, then `one_shot_flag_error` (`daemon.rs:331`) and
  `DaemonConfig::parse()`. `--version` must be handled before line 157 to read no config.
- Brokers: clap `#[command(name, about)]` without `version` (`vam-web-broker.rs:23-28`,
  `vam-schedule-broker.rs:16-21`, `vam-notify-broker.rs:18-24`).
- `GET /v1/node` (`api/mod.rs:527-567`) fills `NodeStatus` (`orchestrator.proto:81-103`, last
  field 15 `base`). The OpenAPI `NodeStatus` (`docs/api/openapi.yaml:3952`) stops at `guestMode`:
  `isolation`, `signingKeys` and `base` are missing too.
- `vam node status` (`cmd/node.rs:112-129`) prints isolation, base and models, then the body.
- Other version strings: `vam_core::VERSION` (`vam-core/src/lib.rs:32`), marketplace
  `node_version` (`marketplace/mod.rs:106`), MCP `clientInfo` (`mcp_client.rs:372`), OCSF product
  (`vam-telemetry/src/ocsf.rs:30`). `NodeRequirement::matches` already drops `-pre` and `+build`
  (`vam-spec/src/marketplace.rs:294-300`), so `0.1.0+dev.14` and `0.1.0-rc.1` satisfy `>=0.1.0`.
- **`hsp.rs:489` is a test** (`client_register_and_invoke_round_trip`). In production `vamd` is the
  HSP client: it sends `broker_version: String::new()` in `RegisterRequest` (`api/mod.rs:1902`),
  and the brokers answer a `RegisterResponse` with no version (`hosted_services.proto:198-207`).
  The broker's own version needs a response field: RA-3.5, not this phase.
- `crates/vam-cli/tests/cli_tests.rs:2682` asserts `vam --version` contains `CARGO_PKG_VERSION`. It
  breaks between a prepare PR and its tag (the nearest tag is then older than `Cargo.toml`).
- `deny.toml` has `[licenses]` only. `check-release-gate.sh:11-34` runs manifests, the
  vamd/guest/notify build checks, the tree check and the lane check.
- `crates/vam-firecracker/Cargo.toml:3` says `version = "0.1.0"`: equal today, so
  `version.workspace = true` changes no lock entry.
- e2e `cli` step (`crates/vam-e2e/src/cli.rs`, `scenario.rs:272-293`): runs `VAM_E2E_CLI` only,
  with an empty environment (no host, no key), literal `contains`, no timeout. `run.sh:61-82`
  builds `vam`, `vamd` and the schedule and notify brokers, not `vam-web-broker`.

#### Scenario delta

| Scenario | Change | Layer | Given | Why |
|---|---|---|---|---|
| `cli-prints-its-version-and-commit` | new | process | none (`cli` steps, no tree) | `vam --version`, then `--version` of `vam-web-broker`, `vam-schedule-broker`, `vam-notify-broker`: each prints `<name> <version> (<commit>…)` matching `^<name> \d+\.\d+\.\d+(-rc\.\d+)?(\+dev(\.\d+)?)? \([0-9a-f]{7,}(\+dirty)?( \d{4}-\d{2}-\d{2})?\)$`, with the version and commit `vam` printed. Red at the first broker step: no `--version` (clap usage error), class `product` |
| `vamd-prints-its-version-without-starting` | new | process | none; fixture tree `version/bad-config` (an `.env` whose half-set TLS pair would stop a start) | `vamd --version` run in the tree: exit 0 within the timeout, one line `vamd <version> (<commit>)`, tree unchanged (no state, no TLS, no key). Red: `vamd` has no `--version`; it reads the `.env` and exits non-zero ("cannot serve the API") |
| `node-status-shows-the-node-and-cli-versions` | new | process | org-admin | `GET /v1/node` has `status.build.version`, `status.build.commit` present and `status.build.channel: dev`; `vam node status`, signed in as org-admin, prints `node: vamd <version> (<commit>) dev` and `cli: vam <version> (<commit>)`, and no `minor versions differ`. Red at `when.0`: `status.build` absent |
| `dev-build-does-not-claim-a-release` | new | process | org-admin | `vam --version` and `vamd --version` contain `+dev`; `GET /v1/node` says `status.build.channel: dev` and its version contains `+dev`; no output says `release`. Red at `when.0`: `vam --version` has no `+dev` |

Unchanged, regression set: `node-init-run-twice-changes-nothing-and-shows-the-key-once` (guards
`bin/vamd.rs`; `--init` must behave as today) and the marketplace scenarios (the node version they
check against becomes `0.1.0+dev`).

Notes for wave 0:

- Runner additions to the `cli` step, each with a `vam-e2e` unit test and a line in
  `e2e/README.md`. Phase 0 needs: `bin` (`vam` by default; `vamd`, `vam-web-broker`,
  `vam-schedule-broker`, `vam-notify-broker` from the paths `run.sh` exports; `run.sh` builds
  `-p vam-web` and exports `VAM_E2E_WEB_BROKER`), `cwd: root`, `matches` (regexes over the
  output), `remember: { as, after }` (keeps a printed word as `{{<as>}}` for later steps; unlike
  `capture`, it may be printed), `as: <org alias>` (passes `--host`, `--token` and `VAM_CA_FILE` as
  `via: cli` does, `notifications.rs:535-563`), `timeout` (default 30 s; a hang is `product`).
  To keep later phases off these files (Execution plan), wave 0 also adds the fields Phases 3 and 5
  need: `copyCli: <path in tree>` (run a copy of `vam` placed in the tree), `home: tree` (`HOME`
  inside the tree), `env` (extra variables), and `files: { <path>: { contains, absent, count, unchanged } }`
  (assertions on files in the tree; `unchanged` compares with the file before the step).
- Red proofs: `scripts/e2e/run.sh --scenarios <the four>` in the session; each fails at the step
  named above, class `product`.

#### Benchmark delta

No `design/benchmark.md` §1 line moves. Launch proof "the install": the node and the CLI name the
version they run (a support question becomes one command).

#### Items

- [ ] **RA-0.1 Build info, one source.**
  - **Scope.** `crates/vam-spec/build.rs` computes `VAM_VERSION`, `VAM_GIT_SHA`,
    `VAM_COMMIT_DATE`, `VAM_BUILD_CHANNEL`. Channel `release` only when `VAM_BUILD_CHANNEL=release`;
    the version is then `CARGO_PKG_VERSION` exactly. Otherwise `git describe --tags --long --match
    'v[0-9]*'` gives `<tag without v>+dev.<N>` (`+dev.0` at the tag itself), and no tag gives
    `<Cargo.toml version>+dev`. Commit: `git rev-parse --short=7 HEAD` plus `+dirty`, `unknown`
    outside a checkout. Date: `git show -s --format=%cs HEAD`. Overrides `VAM_GIT_SHA` and
    `VAM_COMMIT_DATE`; `rerun-if-env-changed` for all four. The rules are pure functions in
    `crates/vam-spec/build_info_logic.rs`, included by `build.rs` (`#[path]`) and by
    `src/build_info.rs`, so every row has a unit test without git. API: `vam_spec::build_info()`
    returns `BuildInfo { version, commit, commit_date, channel }`; `BuildInfo::line(name)` gives
    `<name> <version> (<commit>[ <date>])`, the date on release builds only (open question 6);
    `vam_spec::build_info::CLAP_VERSION` for clap. `crates/vam-cli/build.rs` is deleted;
    `main.rs:9` uses `CLAP_VERSION`; `cli_tests.rs:2682` compares with `build_info().version`.
  - **Files.** `crates/vam-spec/build.rs`, `crates/vam-spec/build_info_logic.rs` (new),
    `crates/vam-spec/src/build_info.rs` (new), `crates/vam-spec/src/lib.rs`,
    `crates/vam-cli/build.rs` (deleted), `crates/vam-cli/src/main.rs`,
    `crates/vam-cli/tests/cli_tests.rs`.
  - **Acceptance.** Unit tests for each row of design §3.3's table, `+dev.0` at a tag, both
    overrides and `+dirty`. `vam --version` prints `vam 0.1.0+dev (<sha>)` here. The PR records the
    extra rebuild after one commit (open question 1).
  - **Tests.** `cargo test -p vam-spec build_info`, `cargo test -p vam-cli --test cli_tests`.
  - **Turns green** (with RA-0.2): `cli-prints-its-version-and-commit`; the `vam` step of
    `dev-build-does-not-claim-a-release`.
  - `Decide: NOW — locked decision 3 (not scored)`

- [ ] **RA-0.2 Every binary prints it; one version everywhere.**
  - **Scope.** `vamd --version` (and `-V`) handled first in `main`, before `dotenvy::dotenv()`
    (`vamd.rs:157`): prints `BuildInfo::line("vamd")`, exits 0, reads no env file, opens nothing.
    The three brokers: `version = CLAP_VERSION` in their `#[command]`. `vam_core::VERSION`,
    marketplace `node_version`, MCP `clientInfo` and the OCSF product version read
    `build_info().version`. `crates/vam-firecracker/Cargo.toml` takes `version.workspace = true`.
    The tracker line about `hsp.rs:489` is a test fixture and stays; the broker's real version is
    RA-3.5.
  - **Files.** `crates/vam-orchestrator/src/bin/vamd.rs`, `brokers/*/src/bin/*.rs`,
    `crates/vam-core/src/lib.rs`, `crates/vam-orchestrator/src/marketplace/mod.rs`,
    `crates/vam-orchestrator/src/mcp_client.rs`, `crates/vam-telemetry/src/ocsf.rs`,
    `crates/vam-firecracker/Cargo.toml`.
  - **Acceptance.** `vamd --version` in a directory whose `.env` would stop a start exits 0 and
    creates nothing. A unit test: `NodeRequirement::parse(">=0.1.0")` matches `0.1.0+dev`,
    `0.1.0+dev.14` and `0.1.0-rc.1`. The marketplace scenarios stay green.
  - **Tests.** `cargo test -p vam-spec marketplace`, `cargo test -p vam-orchestrator --lib`
    (marketplace, daemon), `scripts/e2e/run.sh --scenarios
    cli-prints-its-version-and-commit,vamd-prints-its-version-without-starting`.
  - **Turns green:** `cli-prints-its-version-and-commit`, `vamd-prints-its-version-without-starting`.
  - `Decide: NOW — locked decision 3 (not scored)`

- [ ] **RA-0.3 The node reports its build; `vam node status` shows both.**
  - **Scope.** Proto `NodeStatus`: `NodeBuild build = 16;` with `version`, `commit`,
    `commit_date`, `channel`. `get_node` fills it from `build_info()`. OpenAPI `NodeStatus` gains
    `build` and the three fields it already lacks (`isolation`, `signingKeys`, `base`). An API test
    reads `status.build` (strings present, `channel: dev`). `vam node status` prints first
    `node: vamd <version> (<commit>) <channel>` and `cli: vam <version> (<commit>)`, then
    `warning: the node runs <x.y>, this vam is <a.b>: minor versions differ` when major or minor
    differ (open question 5); a node with no `build` prints `node: version unknown (older vamd)`.
    `--json` prints the body only, as today.
  - **Files.** `proto/vam/v1/orchestrator.proto`, `crates/vam-orchestrator/src/api/mod.rs`
    (`get_node` only), `docs/api/openapi.yaml`, `crates/vam-orchestrator/tests/api_tests.rs`,
    `crates/vam-cli/src/cmd/node.rs`.
  - **Acceptance.** Unit tests of the two lines and the warning from recorded bodies (same version,
    minor differs, patch differs, no `build`); the API test; `examples_json` passes.
  - **Tests.** `cargo test -p vam-cli node`, `cargo test -p vam-orchestrator --test api_tests node`,
    `cargo test -p vam-spec --test examples_json`, the two scenarios.
  - **Turns green:** `node-status-shows-the-node-and-cli-versions`,
    `dev-build-does-not-claim-a-release`.
  - `Decide: NOW — locked decision 3 (not scored)`

- [ ] **RA-0.4 `scripts/release/prepare.sh <version>`.**
  - **Scope.** `scripts/release/lib.sh`, shared with the guards (RA-1.1): parse
    `X.Y.Z[-rc.N]`, compare by semver (an rc sorts below its final), list `v*` tags.
    `prepare.sh` refuses a malformed version, a version not greater than every `v*` tag and a dirty
    tree; sets `[workspace.package] version` (`Cargo.toml:33`); refreshes the workspace entries in
    `Cargo.lock` (`cargo update --workspace --offline`) and checks that nothing else in the lock
    changed; prints the PR title `release: prepare v<v>` and the `gh pr create` line. Tests in
    `scripts/release/test/` on throwaway repositories made in a temp dir (no network), run by
    `scripts/release/test/run.sh`, which `local.sh --fast` and the `rust.yml` `checks` job call.
  - **Files.** `scripts/release/{lib.sh,prepare.sh}`, `scripts/release/test/{run.sh,prepare.test.sh}`,
    `scripts/ci/local.sh`, `.github/workflows/rust.yml` (`checks`).
  - **Acceptance.** A case each: bad version, lower than a tag, equal to a tag, an rc after its
    final (refused), a final after its rc (accepted), a dirty tree, a diff of `Cargo.toml` and the
    workspace lock entries only. A version below the current `Cargo.toml` but above every tag is
    accepted: the first prepare sets `0.1.0-rc.1` over the never-released `0.1.0`.
  - **Tests.** `scripts/release/test/run.sh`.
  - **Turns green:** none (script tests).
  - `Decide: NOW — locked decision 2 (not scored)`

- [ ] **RA-0.5 `deny.toml` advisories and bans; `cargo deny check` in the release gate.**
  - **Scope.** `[advisories]` (RustSec database, `yanked = "deny"`, each `ignore` with id, reason
    and a review date), `[bans]` (deny `openssl` and `openssl-sys`, `wildcards = "deny"`,
    `multiple-versions = "warn"`: 34 duplicates today), `[sources]` (crates.io only,
    `unknown-git = "deny"`). `check-release-gate.sh` runs `cargo deny check` and fails with an
    install hint when `cargo-deny` is missing; `rust.yml` `checks` installs it at a pinned version
    (`cargo install --locked --version …`).
  - **Files.** `deny.toml`, `scripts/e2e/check-release-gate.sh`, `.github/workflows/rust.yml`,
    `Cargo.lock` (only if an advisory has a fixed patch release).
  - **Acceptance.** `cargo deny check` exits 0 on the base; every `ignore` is listed in the PR body
    for the owner.
  - **Tests.** `cargo deny check`, `scripts/e2e/check-release-gate.sh`.
  - **Turns green:** none.
  - `Decide: NOW — locked decision 5 (guard 7) and design §5.4 (not scored)`

- [ ] **RA-0.6 Docs.** `getting-started/cli.md` (`vam --version`, the two `node status` lines, the
  warning), `orchestrator/api.md` (`status.build`), `orchestrator/overview.md` (`vamd --version`);
  `proves` names the Phase 0 scenarios. In wave 2.
  `Decide: NOW — proof 2 (honest docs at the phase close), cost 0`

#### Waves

| Wave | Items | Turns green | Repo | Model | Budget | Depends on | Status | PR |
|---|---|---|---|---|---|---|---|---|
| 0 | scenarios (+ runner `cli` fields) | — (pending, red for the right reason) | vam | Sonnet low | 90 min | — | [ ] | |
| 1 | RA-0.1, RA-0.2 | `cli-prints-its-version-and-commit`, `vamd-prints-its-version-without-starting` | vam | Sonnet medium: crosses `vam-spec`, `vam-cli`, `vam-orchestrator`, `vam-core`, `vam-telemetry` and three brokers | 120 min | 0 | [ ] | |
| 2 | RA-0.3, RA-0.6 | `node-status-shows-the-node-and-cli-versions`, `dev-build-does-not-claim-a-release` | vam | Sonnet medium: a proto field and an API shape | 120 min | 1 | [ ] | |
| 3 | RA-0.4, RA-0.5 | — (release script tests; `cargo deny check`) | vam | Sonnet low | 90 min | — (lane B, beside W0-W2) | [ ] | |

Lane (the microVM lane runs on the PR): W0 yes (`crates/vam-e2e`, `e2e/`), W1 yes
(`vam-orchestrator`, `vam-core`), W2 yes (`proto/`), W3 yes (`scripts/e2e/`; `skip-vm-lane` is
fine, no VM path changes).

#### Open questions (new; the tracker's 14 stand)

1. **Rebuild cost of build info in `vam-spec`.** Every commit then rebuilds `vam-spec` and all
   crates above it. Default: W1 measures one commit's extra rebuild in `local.sh --fast`; under
   3 minutes, keep decision 3 as written; over, the owner chooses: accept it, or move the git reads
   into a small leaf crate (an edit to decision 3).
2. **A dev build exactly at a tag.** Default: `0.1.0-rc.1+dev.0`, never the bare version.
3. **Shallow checkouts.** `actions/checkout` fetches one commit and no tags. Default: fall back to
   `Cargo.toml` + `+dev` (no `.N`); still never a release. `release.yml` fetches full history.
4. **Where `build` sits.** Default: `status.build` in `NodeStatus` (field 16), since `GET /v1/node`
   answers `{ status }`.
5. **Mismatch warning.** Default: warn when major or minor differ (pre-1.0 a minor may break);
   silent on patch, rc and `+dev` differences.
6. **The commit date.** Default: shown on release builds only, as in design §3.3's table; dev
   builds print `(<sha>)`.
7. **Advisories found by `cargo deny`.** Default: `cargo update -p` where a fixed patch release
   exists; otherwise an `ignore` with reason and review date, listed for the owner.

#### Phase close

Scenarios active and green on Actions; unit suites on the merged base. Release PR
`release: release-artifacts P0 (+P1 W1-W2)` (P1.W1 and P1.W2 merged first). Deploy note: none (no
variable; a `vamd` restart picks the version up). Nothing is published.

**Team checklist — Phase 0**

Setup: build from `main` after the P0 release (`cargo build --release -p vam-cli -p vam-orchestrator
-p vam-web -p vam-schedule -p vam-notify`), restart `vamd`.

1. `vam --version`. **See:** `vam 0.1.0+dev (<7 hex>)`.
2. In an empty directory, `vamd --version`. **See:** `vamd`, the same version and commit, nothing
   else; `ls -a` shows no new file.
3. `vam-web-broker --version`, `vam-schedule-broker --version`, `vam-notify-broker --version`.
   **See:** the same version and commit after each name.
4. `vam node status`. **See:** first lines `node: vamd … dev` and `cli: vam …`; no warning.
5. `curl` `GET /v1/node` with your key, `| jq .status.build`. **See:** `version`, `commit`,
   `channel: "dev"`.
6. In a scratch clone: `scripts/release/prepare.sh banana`. **See:** refused, the format named.
   `scripts/release/prepare.sh 0.1.1`. **See:** `git diff --stat` lists `Cargo.toml` and
   `Cargo.lock` only. Delete the clone.
7. `cargo deny check`. **See:** advisories, bans, licenses and sources ok.

Report: "Phase 0 pass", or the step number with the error text.

### Phase 1 — Release workflow, dry run

**Goal:** `release.yml` validates inputs and builds, checks and packages every phase-1 asset
without publishing anything.  
**Depends on:** Phase 0  
**Plan:** drafted 2026-11-17 (breakdown), waiting for the owner  
**Exit:** a `dry_run: true` dispatch on `main` is green and uploads the assets as workflow
artifacts (x86_64 node bundle; `vam` for Linux x86_64/aarch64 and macOS arm64/x86_64; guest
`vam-runtime`; proto descriptor; `SHA256SUMS`; SBOMs; notes); the guard script refuses each bad
input in its own test (wrong branch, commit off `main`, red CI, version mismatch, existing tag,
lower version, malformed input); the binaries' highest `GLIBC_` symbol is ≤ 2.34.

- [ ] `scripts/release/` guard script (pure shell, testable locally against a fixture repo) and
      its test, run by `scripts/ci/local.sh --fast`
- [ ] `scripts/release/build-guest.sh` shared with `build-rootfs.sh` (one way to build the init)
- [ ] `.github/workflows/release.yml`: `validate`, build matrix, `assemble` (SHA256SUMS,
      `cargo cyclonedx`, `cargo auditable`, notes from `release:` merges, glibc check, gitleaks
      and string scans), all actions pinned by SHA, no caches, no publish job yet
- [ ] Proto descriptor export (`vam.binpb` with imports) and `vam-proto-<v>.tar.gz`

<!-- Breakdown 2026-11-17 (phase-breakdown agent, Opus 5.5 high). W1 and W2 run early, on the P0
base (Execution plan). -->

#### Code map (2026-11-17)

- No `release.yml`, no `scripts/release/`. `rust.yml` cancels an older run of the same ref
  (`concurrency`, rust.yml:38-40, `cancel-in-progress: true`): a second merge into `main` soon
  after a prepare merge cancels the prepare SHA's push run, and guard 3 then refuses it.
  `paths-ignore` (rust.yml:4-10): a docs-only merge has no push run at all.
- The push run's conclusion includes the microVM lane: `firecracker` has no `continue-on-error`
  (rust.yml:252-257), and push runs always set `vm=true` (rust.yml:69-71). So guard 3 also
  requires a green lane.
- The guest build sits inside `build-rootfs.sh:42-57` (musl C compiler fallback,
  `--release --target x86_64-unknown-linux-musl`, `+crt-static`); `build-rootfs-lima.sh` is the
  aarch64 copy. `guest_build_guards.rs:17-40` requires `--bin vam-runtime` in every
  `scripts/build-rootfs*.sh` and at least two such scripts: moving the cargo call into
  `build-guest.sh` moves this guard with it. The lane's asset cache key hashes `build-rootfs.sh`
  (rust.yml:310); the `changes` regex is rust.yml:84.
- The release gate (guard 7) compiles `vamd`, `vam-runtime` and `vam-notify` twice each
  (`check-vamd-build.sh:29,45`, `check-guest-build.sh:26,41`, `check-notify-build.sh:21`): about
  10-15 minutes cold in `validate`, before any build (decision 5 keeps it there).
- Protos: `vam-spec/build.rs:13-32` lists 18 files; prost already writes a descriptor set with
  imports to `OUT_DIR` (build.rs:39-48).
- Secret scan today: `export-public.sh:39-81` (`gitleaks dir` with `.gitleaks.toml`; refuses
  `*.ext4`, `vmlinux*`, `.env`, `*.log`).
- Release notes need the "Deploy note" sections of `release:` PR bodies: GitHub API reads (public
  repository).

#### Scenario delta

No new product scenario: nothing a user sees through the CLI or the API changes. Guards-only edits
(made by W2), `then` unchanged:

| Scenario | Change | Layer | Given | Why |
|---|---|---|---|---|
| `vm-boots-and-registers-over-vsock` | changed: `guards` add `vam:scripts/release/build-guest.sh` | firecracker | unchanged | the guest it boots is built by `build-guest.sh` from now on |
| `node-reports-the-standard-base-it-boots` | changed: same `guards` line | firecracker | unchanged | same |
| `skill-uses-python-pandas-and-git-on-the-standard-base` | changed: same `guards` line | firecracker | unchanged | same |

What replaces scenarios here:

| Proof | Shows | Runs |
|---|---|---|
| Guard tests (`scripts/release/test/guards.test.sh`) | each bad input refused by its own case with its reason: dispatched from a non-`main` ref, commit not on `main`'s first-parent history, CI run missing, red or cancelled (fixture API answers), `Cargo.toml` version not the input, tag exists, release (draft included) exists, version not greater than every tag, malformed version, malformed commit; a good input passes | `local.sh --fast`, `rust.yml` `checks` |
| Workflow lint (`scripts/release/check-workflow.sh`) and `actionlint` | `release.yml` has `workflow_dispatch` only; every `uses:` pinned by a 40-hex SHA; no `actions/cache` or `rust-cache`; no `${{ inputs.` or `github.event.inputs` inside `run:`; top-level `permissions: {}`; build and assemble jobs read-only with no `secrets.`; `persist-credentials: false` on every checkout | same |
| glibc check | the highest `GLIBC_` symbol of every gnu binary is 2.34 or lower | `assemble` |
| Scans | `gitleaks dir` over the extracted archives; no `vam_sk_`, `/home/runner`, `/Users/runner`, `D:\a\`; no refused file names | `assemble` |
| Recorded dry run | a `dry_run: true` dispatch on `main` is green and uploads every phase-1 asset | Actions; run URL, asset table and job times in the progress log |

#### Benchmark delta

No §1 line moves. P9 (supply chain) gains its first artifacts (SBOMs, `cargo auditable`), with no
line yet.

#### Items

- [ ] **RA-1.1 The guard script and its tests.**
  - **Scope.** `scripts/release/guards.sh`: bash, git and jq; inputs only from the environment
    (`VERSION`, `COMMIT`, `DRY_RUN`, `REF`, `REPO`); one function per guard (design §4.2), each
    failing with its reason; the GitHub API through `gh api`, or a fixture directory
    (`GUARDS_API_FIXTURES`) in tests. Guard 3 accepts the `rust.yml` run for event `push` (or a
    `workflow_dispatch` run of `rust.yml` on `main`, open question 2) at that exact SHA with
    conclusion `success`, and names the run otherwise. Guard 6 uses `lib.sh` (RA-0.4). A dry run
    from another ref: open question 1. Tests build a fixture repository with first-parent merges, a
    side commit, tags and fixture API answers.
  - **Files.** `scripts/release/guards.sh`, `scripts/release/test/guards.test.sh`,
    `scripts/release/test/fixtures/`.
  - **Acceptance.** Every case in the proof table above fails with its own reason; a good input
    passes; the suite runs in under 10 seconds.
  - **Tests.** `scripts/release/test/run.sh`.
  - `Decide: NOW — locked decision 5 (not scored)`

- [ ] **RA-1.2 `release.yml` skeleton, the workflow lint, `rust.yml` fixes.**
  - **Scope.** `.github/workflows/release.yml`: `on: workflow_dispatch` only (`version`, `commit`,
    `dry_run` default `true`), top-level `permissions: {}`, `concurrency: { group: release,
    cancel-in-progress: false }`. Job `validate` (`contents: read`, `actions: read`): checkout with
    full history and `persist-credentials: false`; inputs reach shell only through `env:`;
    `guards.sh`; guard 7 (`check-release-gate.sh`, which runs `cargo deny check` after RA-0.5) on
    toolchain 1.97.0; a summary table of the guards. No build jobs yet. Every action pinned by full
    SHA with a version comment; `.github/dependabot.yml` for `github-actions` (design §6.4).
    `check-workflow.sh` and a pinned `actionlint` binary (checked by sha256) run from
    `scripts/release/test/run.sh`. `rust.yml`: `cancel-in-progress: ${{ github.event_name ==
    'pull_request' }}` (open question 3); the `changes` regex and the lane cache key add
    `scripts/release/build-guest.sh`.
  - **Files.** `.github/workflows/release.yml` (new), `.github/dependabot.yml` (new),
    `scripts/release/check-workflow.sh` (new), `scripts/release/test/run.sh`,
    `.github/workflows/rust.yml`, `scripts/ci/local.sh`.
  - **Acceptance.** The lint passes on `release.yml` and fails on a copy with each forbidden line
    (test fixtures); `actionlint` is clean; once on `main`, a dispatch runs `validate` only.
  - **Tests.** `scripts/release/test/run.sh`; `actionlint .github/workflows/*.yml`.
  - `Decide: NOW — locked decisions 4, 5, 8, 9 (not scored)`

- [ ] **RA-1.3 One way to build the guest init.**
  - **Scope.** `scripts/release/build-guest.sh [--target x86_64|aarch64] [--out PATH]` with the
    flags of `build-rootfs.sh:42-57` exactly (open question 9 of the tracker: no `release-guest`
    profile). `build-rootfs.sh` and `build-rootfs-lima.sh` call it. `guest_build_guards.rs` checks
    `build-guest.sh` for `--bin vam-runtime` and no features, and that every
    `scripts/build-rootfs*.sh` calls `build-guest.sh`. `firecracker-assets.sh`'s stamp hashes it.
    The three guards-only scenario edits above.
  - **Files.** `scripts/release/build-guest.sh` (new), `scripts/build-rootfs.sh`,
    `scripts/build-rootfs-lima.sh`, `crates/vam-runtime/tests/guest_build_guards.rs`,
    `scripts/e2e/firecracker-assets.sh`, three `e2e/scenarios/*.yaml` (`guards` only).
  - **Acceptance.** The lane on the PR boots the guest built this way, every lane scenario green.
  - **Tests.** `cargo test -p vam-runtime --test guest_build_guards`; the lane.
  - `Decide: NOW — locked decision 11 ("built exactly as build-rootfs.sh builds it") (not scored)`

- [ ] **RA-1.4 The build matrix.**
  - **Scope.** `scripts/release/build.sh <target>`: `cargo +1.97.0 auditable build --release
    --locked` (through `cargo zigbuild --target <triple>.2.34` for Linux gnu), `CARGO_INCREMENTAL=0`,
    `SOURCE_DATE_EPOCH` = the commit time, `--remap-path-prefix` for the checkout and
    `$CARGO_HOME`, `VAM_BUILD_CHANNEL=release`, `VAM_GIT_SHA` and `VAM_COMMIT_DATE` from the
    commit. `scripts/release/package.sh`: archive names and contents as design §5.1, the node
    bundle layout per open question 4, one `cargo cyclonedx` SBOM per archive. Jobs (each `needs:
    validate`, `permissions: contents: read`, no secrets, no caches, checkout of the input commit
    through `env:`): `node-x86_64` (`ubuntu-24.04`: `vamd`, `vam`, three brokers, guest via
    `build-guest.sh`, and the standalone `vam-runtime-<v>-x86_64-unknown-linux-musl`), `cli`
    (`ubuntu-24.04` x86_64 gnu, `ubuntu-24.04-arm` aarch64 gnu, `macos-15` arm64, `macos-15-intel`
    x86_64), `proto` (RA-1.6). Each uploads a workflow artifact. Tools by `cargo install --locked
    --version` (open question 7).
  - **Files.** `scripts/release/{build.sh,package.sh}` (new), `.github/workflows/release.yml`.
  - **Acceptance.** A branch dry run (open question 1) builds every asset; `vam --version` from
    each archive prints `<v> (<sha> <date>)` in its job.
  - **Tests.** `scripts/release/build.sh x86_64-unknown-linux-gnu` locally once (cold, about
    10 minutes); branch dry runs, at most three per wave.
  - `Decide: NOW — locked decisions 8, 11 (not scored)`

- [ ] **RA-1.5 `assemble`.**
  - **Scope.** Download the build artifacts; `SHA256SUMS`; the glibc check (`objdump -T`);
    `cargo audit bin` on each binary; `scripts/release/scan.sh` (gitleaks over the extracted
    archives with `.gitleaks.toml`, the string scans, the refused names; one list shared with
    `export-public.sh`); `scripts/release/notes.sh` (first-parent `release:` merges since the
    previous tag per open question 9, the "Deploy note" sections of their PR bodies verbatim, the
    asset table, the verify commands); a job summary (guards, asset table with sizes and sha256,
    notes). One `release-<v>` workflow artifact holds everything. `assemble` reads PR bodies with
    `pull-requests: read`, nothing more.
  - **Files.** `scripts/release/{scan.sh,notes.sh}` (new), `scripts/export-public.sh` (the shared
    list), `.github/workflows/release.yml`, `scripts/release/test/` (notes and scan cases on
    fixtures).
  - **Acceptance.** A branch dry run is green and its summary shows the table and the notes; a
    planted `vam_sk_` string and a planted `.env` in a test archive fail the scan.
  - **Tests.** `scripts/release/test/run.sh`; branch dry runs.
  - `Decide: NOW — locked decisions 10, 12 (not scored)`

- [ ] **RA-1.6 Proto descriptor export.** `scripts/release/proto.sh`: `vam.binpb`
  (FileDescriptorSet with imports) from one list of proto files read by both `vam-spec/build.rs`
  and the script (open question 5), plus the `.proto` files, as `vam-proto-<v>.tar.gz`; a check
  that the set decodes. In W3. `Decide: NOW — locked decision 11 and the tracker (not scored)`

- [ ] **RA-1.7 The recorded dry run on `main`.** After the P1 release merges: dispatch `version:
  0.1.0` (the `Cargo.toml` version; no tag exists, so guards 5 and 6 pass), `dry_run: true`.
  Record the run URL, the asset table, the highest `GLIBC_` per binary and each job's time in the
  progress log. Orchestrator (owner if the token cannot dispatch).
  `Decide: NOW — the phase exit`

#### Waves

| Wave | Items | Turns green | Repo | Model | Budget | Depends on | Status | PR |
|---|---|---|---|---|---|---|---|---|
| 1 | RA-1.1, RA-1.2 | — (guard tests and workflow lint pass) | vam | Sonnet high: the guards decide what can be published, and the workflow's permissions are the release boundary | 150 min | P0.W3 (`lib.sh`); lane B, P0 base | [ ] | |
| 2 | RA-1.3 | — (lane boots the guest from `build-guest.sh`) | vam | Sonnet low | 75 min | — (lane C, P0 base) | [ ] | |
| 3 | RA-1.4, RA-1.6 | — (branch dry run builds every phase-1 asset) | vam | Sonnet medium: four runner OSes, zigbuild and path remapping the plan cannot fully specify | 180 min | P0 release on `main` (P0.W1, W1, W2) | [ ] | |
| 4 | RA-1.5 | — (branch dry run: `SHA256SUMS`, SBOMs, notes, glibc, scans) | vam | Sonnet medium: same reason | 150 min | 3 | [ ] | |
| close | RA-1.7 | — (recorded dry run on `main`) | vam | orchestrator | 45 min | P1 release merged | [ ] | |

Lane: W1 yes (`rust.yml`), W2 yes (`build-rootfs*.sh`, `vam-runtime`, `scripts/e2e/`), W3 no, W4 no.

#### Open questions (new)

1. **Dry runs from another ref.** Without them, `release.yml` can be tried on Actions only after
   each change reaches `main`. Default: allowed only with `dry_run: true`; guards 1-3 print
   `skipped (dry run from <ref>)` and the summary's first line says `NOT RELEASABLE`; a publish
   always enforces every guard. Needs `release.yml` on `main` first (shipped by the P0 release).
2. **Which CI run guard 3 accepts.** Default: the `push` run, or a `workflow_dispatch` run of
   `rust.yml` on `main`, at that exact SHA with conclusion `success`; a cancelled or missing run is
   refused with its URL or with the dispatch command.
3. **`rust.yml` cancelling on `main`.** Default: never cancel a push run on `main`
   (`cancel-in-progress` for pull requests only), so every merge keeps a complete run.
4. **Node bundle layout.** Default: one top directory `vam-node-<v>-<target>/` with the binaries at
   its top (`sudo ./vam node init` works from it; `find_vamd` looks beside `vam`,
   `node_install.rs:160-169`), `LICENSE-*`, `NOTICE`, `README`, `assets.pins`; `share/vam/base/`
   joins in Phase 3.
5. **The proto file list.** Default: one list file under `proto/vam/v1/`, read by `build.rs` and
   `proto.sh`.
6. **`actionlint` in sessions.** Default: required in CI (pinned binary); in `local.sh --fast` it
   runs when present and prints a skip line otherwise.
7. **Tool installs.** Default: `cargo-auditable`, `cargo-cyclonedx`, `cargo-zigbuild`,
   `cargo-audit` by `cargo install --locked --version`, zig by a pinned release checked by sha256;
   no third-party install action (about 3 extra minutes, nothing unpinned).
8. **Release wall time.** Guard 7 compiles in `validate` before any build, so a run takes about
   35-45 minutes, not the 20 of design §6.3. Default: accept (decision 5).
9. **Notes range.** Default: an rc lists the `release:` merges since the previous tag of any kind;
   a final lists them since the previous final (for `v0.1.0`: all of them, grouped by rc).

#### Phase close

No scenario changes behaviour. Release PR `release: release-artifacts P1`, then RA-1.7. Deploy
note: none.

**Team checklist — Phase 1** (owner)

Setup: the recorded dry run on `main` (URL in the progress log).

1. Open the run's summary. **See:** guards 1-8 `ok`; an asset table with every phase-1 asset, its
   size and sha256; notes listing the `release:` merges.
2. On an Ubuntu 22.04 or Amazon Linux 2023 host, download the `vam-node-…-x86_64-…` artifact,
   extract it, `./vamd --version`, `./vam --version`. **See:** `0.1.0 (<sha> <date>)`, no `+dev`.
3. On a Mac, extract the macOS `vam` with `tar` in a terminal, `./vam --version`. **See:** the same
   line.
4. `sha256sum -c SHA256SUMS --ignore-missing` in the download directory. **See:** `OK` per file.
5. Dispatch again with `version: 9.9.9`. **See:** `validate` fails at guard 4 and names `0.1.0`.
6. Dispatch from a feature branch with `dry_run: false`. **See:** refused at guard 1.

Report: "Phase 1 pass", or the step number with the error text.

### Phase 2 — Publish

**Goal:** the first immutable, attested release, `v0.1.0-rc.1`.  
**Depends on:** Phase 1; the owner's one-time setup (design §9)  
**Plan:** drafted 2026-11-17 (breakdown), waiting for the owner  
**Exit:** `gh release verify v0.1.0-rc.1` and `gh attestation verify` on every asset pass; the tag
points at the dispatched commit; deleting or moving the tag is refused by the ruleset; a second
dispatch with the same version fails at the guard.

- [ ] Scratch-repository test: can `GITHUB_TOKEN` create a `v*` tag under a "restrict creations"
      ruleset (decides open question 3)
- [ ] `publish` job in environment `release`: draft release at the commit, upload, `actions/attest`
      (provenance and SBOM), `gh release verify-asset` per asset, publish (pre-release for `-rc`)
- [ ] Job summary: asset table, guards, notes; the approval page shows them
- [ ] `v0.1.0-rc.1` published and verified
- [ ] Owner checklist run end to end and amended where it was wrong

<!-- Breakdown 2026-11-17 (phase-breakdown agent, Opus 5.5 high). -->

#### Code map (2026-11-17)

- After Phase 1, `release.yml` has no job with write permission and no `environment`; the
  repository settings of design §9 (environment `release`, immutable releases, the `v*` ruleset)
  are the owner's.
- `gh release verify` and `gh release verify-asset` check the release attestation that GitHub signs
  **when the release is published**. Before publishing, only the artifact attestations
  (`gh attestation verify`) and the hashes can be checked. So "attest, verify, publish" (design
  §4.1) becomes: attest, verify artifact attestations and hashes on the draft, publish, then verify
  the release attestation (open question 1).
- `releases/latest` (API and download URLs) ignores pre-releases: while only rcs exist, there is
  no "latest" (matters for Phase 5's install one-liner and `vam upgrade`).

#### Scenario delta

None: the phase adds no behaviour a user sees through the CLI or the API. What replaces scenarios:

| Proof | Shows | Runs |
|---|---|---|
| Workflow lint, extended | `publish` is the only job with write permissions (`contents: write`, `id-token: write`, `attestations: write`); it has `environment: release`, `needs: assemble`, and runs only when `dry_run` is false; no secret other than `GITHUB_TOKEN` | `local.sh --fast`, `rust.yml` `checks` |
| Scratch rehearsal (RA-2.1) | whether the token can create a `v*` tag under "restrict creations"; a full publish of a toy asset; tag delete and move refused | the public scratch repository; recorded |
| `v0.1.0-rc.1` verified | `gh release verify v0.1.0-rc.1`; `gh release verify-asset` and `gh attestation verify -R squrell-inc/VAM --signer-workflow squrell-inc/VAM/.github/workflows/release.yml` per asset; the tag at the dispatched commit | owner and orchestrator; recorded |
| Negatives | deleting or moving the tag is refused; a second dispatch of `0.1.0-rc.1` fails at guard 5 | recorded |

#### Benchmark delta

No §1 line moves. Launch proof "the install" gains its first verifiable release.

#### Items

- [ ] **RA-2.1 Scratch-repository test (owner and orchestrator).** A public scratch repository with
  the same settings (environment `release`, immutable releases, `v*` ruleset with "restrict
  creations" on) and a copy of `release.yml` and `scripts/release/` reduced to one toy asset.
  (a) Can `GITHUB_TOKEN` create `v0.0.1-rc.1` through `gh release create --draft --target` under
  the creation rule? This decides tracker open question 3. (b) A full rehearsal of the publish job
  (draft, upload, attest, verify, publish, `gh release verify`). (c) Delete and move of the tag
  refused. Results in the progress log. Runs any time before W1 merges (lane O).
  `Decide: NOW — tracker open question 3 (not scored)`
- [ ] **RA-2.2 The `publish` job.**
  - **Scope.** `environment: release`; `permissions: contents: write, id-token: write,
    attestations: write`; `needs: assemble`; runs only when `dry_run` is false. Downloads the
    `release-<v>` artifact; checks guard 5 again right before creating (tag and release still
    absent); `gh release create v<v> --target <commit> --draft --notes-file RELEASE_NOTES.md`
    (`--prerelease` for `-rc`, `--latest` for a final); uploads every asset; attests build
    provenance for every asset and `SHA256SUMS` (`actions/attest-build-provenance`, pinned by SHA)
    and one SBOM attestation per archive (`actions/attest-sbom`); downloads the draft's assets,
    compares them with `SHA256SUMS` and runs `gh attestation verify` on each; publishes; then
    `gh release verify` and `gh release verify-asset` per asset. A failure after publish is
    reported in the summary with "do not reuse this number; prepare the next" (decision 6).
    Workflow lint extended for this job.
  - **Files.** `.github/workflows/release.yml`, `scripts/release/{publish.sh,verify.sh}` (new),
    `scripts/release/check-workflow.sh`, `scripts/release/test/`.
  - **Acceptance.** The lint cases; RA-2.1 (b) green with this job's exact steps.
  - **Tests.** `scripts/release/test/run.sh`; the scratch rehearsal.
  - `Decide: NOW — locked decisions 6, 7, 9, 10 (not scored)`
- [ ] **RA-2.3 Job summary and approval page.** The pending deployment shows the run's summary:
  the commit, guards 1-8 with results, the asset table (name, size, sha256), the notes. In W1.
  `Decide: NOW — locked decision 12 (notes reviewed at the gate)`
- [ ] **RA-2.4 `v0.1.0-rc.1`.** Prepare PR `0.1.0-rc.1` merged into `main`; its push run green
  (lane included); dry run; publish dispatch; owner approves; every check of the proof table;
  negatives. Orchestrator and owner. `Decide: NOW — the phase exit`
- [ ] **RA-2.5 Owner checklist amended.** Design §9 corrected where the run proved it wrong (a
  `design/` edit in the close PR). `Decide: NOW — the tracker's item`

#### Waves

| Wave | Items | Turns green | Repo | Model | Budget | Depends on | Status | PR |
|---|---|---|---|---|---|---|---|---|
| 1 | RA-2.2, RA-2.3 | — (lint; scratch rehearsal of the publish job green) | vam | Sonnet high: write permissions, tag creation and attestation | 150 min | Phase 1 closed; RA-2.1 (a) answered | [ ] | |
| close | RA-2.4, RA-2.5 | — (`v0.1.0-rc.1` published and verified) | vam | orchestrator and owner | 120 min | P2 release; prepare PR; owner settings | [ ] | |

RA-2.1 runs before W1 merges (owner and orchestrator, lane O). Lane: W1 no.

#### Open questions (new)

1. **Verify before publish.** Default: before publishing, compare the draft's assets with
   `SHA256SUMS` and run `gh attestation verify` on each; after publishing, `gh release verify` and
   `verify-asset` (the release attestation exists only then). A failure after publish is
   announced, and the next rc follows.
2. **The scratch repository.** Default: public (attestations on a private repository need GitHub
   Enterprise Cloud), named by the owner, archived after Phase 2 and reused for later workflow
   changes.
3. **No "latest" during the rcs.** Default: accept; rcs are pre-releases (decision 1), and the docs
   give tag-specific URLs until `v0.1.0`.
4. **Who dispatches.** Default: the orchestrator dispatches dry runs when its token has
   `actions: write`; the owner dispatches every publish.

#### Phase close

Release PR `release: release-artifacts P2` (deploy note: the owner's settings must be in place
before the first publish), then RA-2.4 and RA-2.5.

**Team checklist — Phase 2:** design §9 "Every release", steps 1-7, run end to end for
`v0.1.0-rc.1`, plus: `gh release verify v0.1.0-rc.1`; download the node bundle and run
`gh attestation verify` on it; try `git push --delete origin v0.1.0-rc.1` (refused); dispatch
`0.1.0-rc.1` again (fails at guard 5); `vam --version` from the macOS arm64 archive prints
`vam 0.1.0-rc.1 (<sha> <date>)`. Report: "Phase 2 pass", or the step number with the error text.

### Phase 3 — Install a node from a release

**Goal:** a node is installed from the x86_64 node bundle (Linux server or WSL2) with its brokers,
without Rust or a checkout; `vam.exe` exists.  
**Depends on:** Phase 2  
**Plan:** drafted 2026-11-17 (breakdown; draft: the orchestrator re-validates against the code at
phase start), waiting for the owner. Most waves may run before Phase 2 closes (Execution plan,
lane D); the phase closes after rc.1  
**Exit:** `vam.exe` builds and runs `vam --version`, `vam login` and `vam health` on
`windows-2025`; `vam node init` from an extracted node bundle on a host with Docker but no Rust
and no checkout builds the base from the recipe with the bundled `vam-runtime`, starts `vamd`,
and installs and registers the brokers named with `--broker`; re-running it from a newer bundle
keeps `/var/lib/vam`; the docs lead with the release; the next minor ships with these.

- [ ] Gate the unix-only `node init` / `node install` modules (`node init` says Linux only
      elsewhere); `windows-2025` job in `release.yml`
- [ ] `build-rootfs.sh --runtime PATH` (inject without compiling); recipe kept inside the Vamfile
      grammar (design §5.3)
- [ ] Node bundle carries `share/vam/base/` (recipe + manifest) and `vam-runtime`;
      `vam node init --kit DIR` (default beside the binary)
- [ ] `vam node init --broker web,schedule,notify`: one systemd unit per broker, registration,
      `--check` names a missing broker binary; brokers report `build_info()` in `Register`;
      `vam node status` lists them with versions
- [ ] Docs: new `getting-started/releases.md` (sidebar), `installation.md`, `windows-wsl.md`,
      `cli.md`, `project-status.md`, `hosted/overview.md` (installing brokers)

<!-- Breakdown 2026-11-17 (phase-breakdown agent, Opus 5.5 high). Draft: re-validate at phase start. -->

#### Code map (2026-11-17)

- Windows: `cmd/mod.rs:16-17` include `node_init` and `node_install` always; both import
  `std::os::unix::fs` (`node_init.rs:13`, `node_install.rs:12`); `config.rs:69-71` is already
  `cfg(unix)`. `node.rs:82-108` dispatches `init`.
- `step_base` (`node_install.rs:333-398`): `--base`, else a checkout (`find_checkout`, 171-189,
  needs `scripts/build-rootfs.sh`); **refuses to build under `--root`** (378-380); runs
  `bash scripts/build-rootfs.sh` with `VAM_ROOTFS_OUT` (380-387); the stamp hashes checkout files
  (191-225).
- `build-rootfs.sh` takes no `--runtime` (its argument loop, 27-33, rejects anything else); it
  stages under `<script dir>/../target/rootfs-staging` (18-20) and reads the recipe from
  `<script dir>/base/` (72, 84-91), so from `share/vam/base/` both land inside the bundle; every
  step calls `sudo` (a root host without `sudo` fails).
- The vamd unit's `ExecStart` is the `vamd` beside `vam` (`node_install.rs:139`, `find_vamd`
  160-169): a re-run from a newer bundle repoints it.
- Brokers. `vamd` dials a broker per call (`host_tools.rs:3347`). Registration is an operator
  approval stored by `PUT /v1/hosted/brokers` with a `brokers:admin` key (`api/mod.rs:1866-1924`).
  `vam node init` holds the admin key only on its first run (it relays it, `node_install.rs:401-446`),
  so a re-run cannot call the API. `Register` goes from `vamd` to the broker; `RegisterResponse`
  (`hosted_services.proto:204-207`) and `HostedBrokerRecord` (`orchestrator.proto:1104-1110`)
  carry no version. `vamd` opens each broker's gateway socket at start (`gateway.rs:172,273`); the
  schedule broker needs `--gateway-socket` (the two-step start of `run-schedule-broker.sh:126-147`).
  `vam-web-broker` has no `manifest` subcommand (its manifest is `scripts/web-broker/manifest.json`);
  schedule and notify print theirs (`vam-schedule-broker.rs:43-46`, `vam-notify-broker.rs:46-49`).
- `release.yml` (Phase 1) has no Windows job; the node bundle has no `share/`.

#### Scenario delta

| Scenario | Change | Layer | Given | Why |
|---|---|---|---|---|
| `node-init-from-a-release-bundle-needs-no-checkout` | new | process | none; fixture `node-init/bundle` (a ready host plus a bundle: a stub `vam-runtime`, `share/vam/base/` with `manifest.json` and a stub builder that writes a small image and a manifest naming the `--runtime` digest it got; `vam` copied in by `copyCli`) | `vam node init --root {{root}} --no-systemd --vamd {{vamd}}` (`cwd: root`) with the kit beside the copied `vam` and no checkout anywhere: exit 0; `done base: built from the kit`; the base manifest under the tree names the bundle runtime's sha256; the output never says `checkout`. Red: today it stops at the base step (no checkout found, or "refusing to build the base image under --root"), class `product` |
| `node-init-from-a-newer-bundle-keeps-the-state` | new | process | none; `node-init/bundle` then `node-init/bundle-newer` (another stub runtime and kit version) | `--no-start` init from the first bundle, then from the newer one on the same tree (`root: same`): the unit's `ExecStart` names the newer `vamd`; no new key printed (`already has an admin key`); `state.db` and `tls/ca.pem` unchanged (`files`); the base is rebuilt because the runtime changed. Red: as above |
| `node-init-installs-and-registers-the-brokers-asked-for` | new | process | none; `node-init/ready` (the brokers `run.sh` builds sit beside the copied `vam`) | `vam node init --root {{root}} --no-start --broker web,schedule --vamd {{vamd}}`: writes `vam-broker@web` and `vam-broker@schedule` units and their argument files (no secret in them), approves both (`done broker vam.web: approved`), leaves notify alone; a second run reports both `ok (unchanged)`. Red: `--broker` unknown (usage error) |
| `node-status-lists-the-brokers-with-their-versions` | new | process | org-admin, schedule-broker | `GET /v1/hosted/brokers` has `brokers.0.brokerVersion`; `vam node status` as org-admin prints `broker vam.schedule <version> (<commit>)`. Red at `when.0`: field absent |
| `node-init-check-names-what-is-missing` | changed: `args` add `--broker web`; `contains` adds `missing broker vam-web-broker` (the bare tree has none) | process | unchanged | the check names a requested broker binary; a spec change approved with this breakdown |
| `node-reports-the-standard-base-it-boots` | changed: `guards` add `vam:scripts/e2e/firecracker-assets.sh` | firecracker | unchanged | the lane now builds its image with `build-rootfs.sh --runtime` (RA-3.2), so this proves a prebuilt, injected runtime boots |

Notes for wave 0: fixtures under `e2e/world/fixtures/node-init/` only; the stub builder is shell
and needs no Docker; the `cli` fields come from P0.W0 (`copyCli`, `files`). Red proofs with
`scripts/e2e/run.sh --scenarios …`.

#### Benchmark delta

P3, install half (machine-base): minutes from a downloaded bundle to a signed-in node with no Rust
and no checkout, base build included; stopwatch on the team checklist (target under 30 minutes).
Launch proof "the install" moves from "from source" to "from a release".

#### Items

- [ ] **RA-3.1 `vam` builds for Windows.** `#[cfg(unix)]` on the two modules (`cmd/mod.rs:16-17`);
  on other systems `vam node init` exits non-zero with "runs on Linux only; on Windows use WSL
  (getting-started/windows-wsl)". `rust.yml`: an advisory `cli (windows)` job on `windows-2025`
  (`cargo build -p vam-cli`, `vam.exe --version`). Files: `crates/vam-cli/src/cmd/{mod.rs,node.rs}`,
  `.github/workflows/rust.yml`. Tests: `cargo test -p vam-cli`; the job.
  `Decide: NOW — tracker open question 6 (not scored)`
- [ ] **RA-3.2 `build-rootfs.sh --runtime PATH`; the recipe stays a Vamfile.** Inject the given
  binary (checked: a static ELF for the target architecture) without cargo or `musl-gcc`; the
  recipe directory relative to the script, so it runs from `share/vam/base/`; staging in
  `mktemp -d`; `sudo` only when not root. `scripts/base/check-recipe.sh`: the `Dockerfile` uses
  only `FROM`, `RUN`, `COPY`, `ENV`, `ARG`, `WORKDIR`, `LABEL` and the syntax directive
  (`design/agent-experience.md` III.2), run by `local.sh --fast` and `checks`.
  `firecracker-assets.sh` builds the lane image with `build-guest.sh` then `build-rootfs.sh
  --runtime`. Files: `scripts/build-rootfs.sh`, `scripts/base/check-recipe.sh` (new),
  `scripts/e2e/firecracker-assets.sh`, `scripts/ci/local.sh`, `.github/workflows/rust.yml`, the
  `guards` line above. Tests: `cargo test -p vam-runtime --test guest_build_guards`; the lane.
  `Decide: NOW — locked decision 13 (not scored)`
- [ ] **RA-3.3 The kit in the node bundle; `vam.exe`.** `package.sh` adds `share/vam/base/`
  (`build-rootfs.sh`, `base/Dockerfile`, `base/pins`, `base/resolve-pins.sh`, `manifest.json` with
  name `vam/base:2`, version and each file's sha256) and `vam-runtime` to the node bundle;
  `release.yml` gains the `windows-2025` job (`vam-<v>-x86_64-pc-windows-msvc.zip`; `vam.exe
  --version`, `config set-host`, a `health` that fails with the guided connection error).
  Files: `scripts/release/package.sh`, `.github/workflows/release.yml`. Tests: a branch dry run.
  `Decide: NOW — locked decisions 11, 13 (not scored)`
- [ ] **RA-3.4 `vam node init --kit DIR`.** Default: the `share/vam/base/` beside the running `vam`
  when it holds `manifest.json`; precedence per open question 2. Checks the kit's files against
  `manifest.json`; runs the kit's `build-rootfs.sh --runtime <bundle>/vam-runtime` with
  `VAM_ROOTFS_OUT`; allowed under `--root` for a kit only (open question 3); the stamp is the
  sha256 of the kit manifest and the runtime, so the same bundle reports `ok (unchanged)` and a
  newer one rebuilds. Files: `crates/vam-cli/src/cmd/{node.rs,node_install.rs,node_init.rs}`.
  Tests: `cargo test -p vam-cli node_install`; the two bundle scenarios. Turns green:
  `node-init-from-a-release-bundle-needs-no-checkout`, `node-init-from-a-newer-bundle-keeps-the-state`.
  `Decide: NOW — locked decision 13 (not scored)`
- [ ] **RA-3.5 Brokers report their version.** Proto `RegisterResponse.broker_version = 3`
  (`hosted_services.proto`) and `HostedBrokerRecord.broker_version = 6`; the three brokers fill it
  from `build_info()`; `vamd` stores it on the approval when `Register` succeeds (a store column)
  and returns it; `vam node status` prints `broker <id> <version>` from `GET /v1/hosted/brokers`.
  `vam-web-broker manifest` prints its manifest (the crate becomes the one source;
  `run-web-broker.sh` reads it). Files: protos, `crates/vam-orchestrator/src/{api/mod.rs,store…}`,
  `brokers/*/src/broker.rs`, `brokers/vam-web/src/bin/vam-web-broker.rs`, `scripts/run-web-broker.sh`,
  `crates/vam-cli/src/cmd/node.rs`, `docs/api/openapi.yaml`. Turns green:
  `node-status-lists-the-brokers-with-their-versions`. `Decide: NOW — locked decision 3 (not scored)`
- [ ] **RA-3.6 Approving a broker without a key.** One-shot `vamd --register-broker <manifest.json>
  --unix-path <socket> [--peer-uid N]`, root only, like `--init` (open question 1): the steps of the
  `PUT` handler (approval, `Register` when the socket answers, the version recorded), refused
  with `--root` and with any other one-shot flag; prints the gateway socket path it will open.
  Files: `crates/vam-orchestrator/src/bin/vamd.rs`, `daemon.rs` (`one_shot_flag_error`), a shared
  function with `api/mod.rs`. Tests: `cargo test -p vam-orchestrator` (approval written once,
  idempotent, refused flags). `Decide: NOW — locked decision 14 (not scored)`
- [ ] **RA-3.7 `vam node init --broker web,schedule,notify`.** Per open question 4: one template
  unit, an argument file per broker (no secrets), sockets 0600 under the state directory, started
  after `vamd`; the gateway path from RA-3.6's output; `--check --broker` names a missing broker
  binary beside `vam`; a re-run is idempotent and never removes a broker it was not asked about.
  Files: `crates/vam-cli/src/cmd/{node.rs,node_install.rs,node_init.rs}`. Tests:
  `cargo test -p vam-cli node`; the scenarios. Turns green:
  `node-init-installs-and-registers-the-brokers-asked-for`, `node-init-check-names-what-is-missing`
  (changed). `Decide: NOW — locked decision 14 (not scored)`
- [ ] **RA-3.8 Docs.** New `getting-started/releases.md` (sidebar in `docs/astro.config.mjs`;
  which archive for which machine, verify, install a node from the bundle, rc URLs per Phase 2 open
  question 3); `installation.md`, `windows-wsl.md`, `cli.md`, `project-status.md` lead with the
  release; `hosted/overview.md` (installing brokers). `Decide: NOW — the tracker's item`

#### Waves

| Wave | Items | Turns green | Repo | Model | Budget | Depends on | Status | PR |
|---|---|---|---|---|---|---|---|---|
| 0 | scenarios | — (pending, red for the right reason) | vam | Sonnet low | 90 min | P0.W0 (runner fields) | [ ] | |
| 1 | RA-3.1 | — (`cli (windows)` job green) | vam | Sonnet low | 60 min | P0.W2 (`node.rs`) | [ ] | |
| 2 | RA-3.2 | — (the lane boots the `--runtime` image) | vam | Sonnet low | 90 min | P1.W2 | [ ] | |
| 3 | RA-3.4 | `node-init-from-a-release-bundle-needs-no-checkout`, `node-init-from-a-newer-bundle-keeps-the-state` | vam | Sonnet medium: precedence and stamp rules the plan cannot fully specify | 120 min | 0, 2 | [ ] | |
| 4 | RA-3.5, RA-3.6 | `node-status-lists-the-brokers-with-their-versions` | vam | Sonnet high: RA-3.6 adds a second way to approve a broker, outside the API's auth | 180 min | 0, 1 (`node.rs`), P0.W2 (`orchestrator.proto`) | [ ] | |
| 5 | RA-3.7 | `node-init-installs-and-registers-the-brokers-asked-for`, `node-init-check-names-what-is-missing` | vam | Sonnet medium: crosses `vam-cli` and the `vamd` one-shot | 120 min | 3, 4 | [ ] | |
| 6 | RA-3.3 | — (branch dry run: kit and runtime in the bundle; the Windows zip) | vam | Sonnet medium: MSVC and aws-lc on a new runner | 120 min | 2; P2.W1 (`release.yml` order) | [ ] | |
| 7 | RA-3.8 | — | vam | Sonnet low | 75 min | 3-6 | [ ] | |

Lane: W0 yes, W1 yes (`rust.yml`), W2 yes, W4 yes (`vam-orchestrator`, `proto/`); W3, W5, W6,
W7 no.

#### Open questions (new)

1. **Approving a broker without a key.** Default: the root-only one-shot `vamd --register-broker`
   (the same trust as `vamd --init`, which already mints the first admin key offline).
   Alternative: `vam node init` uses the key it relays on the first run and `VAM_API_KEY` on later
   runs; no new path, but a re-run without the key cannot register a newer broker.
2. **Where the base comes from.** Default: `--base` > `--kit` > the kit beside `vam` >
   `--checkout` or a checkout found from the working directory. A bundle install never looks for a
   checkout.
3. **Building under `--root`.** Default: only from a kit; the kit's builder writes only
   `VAM_ROOTFS_OUT`.
4. **Broker units.** Default: `vam-broker@.service`, an argument file `/etc/vam/broker-<name>.env`
   (no secrets), `User=root` like `vamd`, `After=` and `Requires=vamd.service`; socket
   `/var/lib/vam/brokers/<name>.sock` (0600); state `/var/lib/vam/brokers/<name>/`.
5. **Removing a broker.** Default: not in this phase.
6. **aws-lc on Windows.** Default: if `aws-lc-sys` needs NASM on MSVC, install a pinned NASM in the
   job; do not switch TLS backends.
7. **`vam login` and `vam health` on Windows.** Default: CI proves `--version`, `config set-host`
   and the guided connection error; a real sign-in against a WSL node is a team-checklist step.

#### Phase close

Scenarios active and green; the lane green with the `--runtime` image. Release PR
`release: release-artifacts P3` after rc.1; then rc.2 (Execution plan). Deploy note: a node
installed from source keeps working; `--broker` is opt-in.

**Team checklist — Phase 3**

Setup: rc.2; a fresh Ubuntu 24.04 x86_64 host with KVM and Docker, no Rust, no checkout; a
Windows 11 PC with WSL2.

1. Download the node bundle and `SHA256SUMS`; `sha256sum -c SHA256SUMS --ignore-missing`;
   `gh attestation verify <bundle> -R squrell-inc/VAM`. **See:** OK; verified.
2. Extract; `sudo ./vam node init --check --broker web,schedule`. **See:** one line per check,
   brokers found.
3. `sudo ./vam node init --broker web,schedule`, with a stopwatch. **See:** the base built from the
   kit, the admin key once, `vamd`, `vam-broker@web` and `vam-broker@schedule` active. Write down
   the minutes.
4. `vam config set-host`, `set-ca`, `set-key`; `vam node status`. **See:** the node and cli lines;
   `broker vam.web …`, `broker vam.schedule …`.
5. `vam secrets set web/brave-api-key …`, then a chat that searches the web. **See:** an answer
   with sources.
6. Extract a newer bundle (a later dry run or rc) in another directory; `sudo ./vam node init
   --broker web,schedule` from it. **See:** no new key; earlier threads still listed.
7. In WSL Ubuntu: steps 1-4. **See:** the same.
8. On Windows: `vam.exe --version`, `vam.exe config set-host https://localhost:8080`, `set-ca`,
   `login`, `health`. **See:** the WSL node answers. `vam.exe node init`. **See:** "Linux only".

Report: "Phase 3 pass", or the step number with the error text.

### Phase 4 — aarch64 node (Lima on Apple Silicon)

**Goal:** a Mac (M3 or newer) runs a node in Lima from the release, with the standard base, and no
compile.  
**Depends on:** Phase 3  
**Plan:** drafted 2026-11-17 (breakdown; draft: the orchestrator re-validates against the code at
phase start), waiting for the owner. W1 may start once P3.W2, P3.W3 and P3.W5 are merged
(Execution plan, lane F)  
**Exit:** `vam-node-<v>-aarch64-unknown-linux-gnu.tar.gz` published; inside a Lima VM,
`sudo ./vam node init` from it builds the standard base (aarch64 Debian digest, aarch64 Bun pin),
fetches the pinned aarch64 Firecracker and kernel, starts `vamd`; the macOS `vam` reaches it;
`getting-started/macos-lima.md` leads with this path; proof recorded at the phase close (lane
variant if an arm64 KVM runner exists, else a recorded Lima run).

- [ ] aarch64 pins in `scripts/base/pins` (Debian digest, Bun) and `scripts/assets.pins`
      (Firecracker, kernel); the recipe builds for both architectures
- [ ] `check_platform` accepts aarch64 Linux; `vam node init` fetches per-architecture assets
- [ ] aarch64 node bundle job on `ubuntu-24.04-arm` (and aarch64 `vam-runtime` musl)
- [ ] Docs: `macos-lima.md`; retire `build-rootfs-lima.sh`'s minimal image as the default

<!-- Breakdown 2026-11-17 (phase-breakdown agent, Opus 5.5 high). Draft: re-validate at phase start. -->

#### Code map (2026-11-17)

- `check_platform` accepts x86_64 Linux only (`node_init.rs:228-240`). The CLI embeds
  `scripts/assets.pins` (`node_init.rs:24`), which holds x86_64 values only; the Firecracker URL
  and the entry names are built with `x86_64` (`node_install.rs:270,287,290`).
- **The Debian pin is already multi-architecture**: `DEBIAN_DIGEST` is a manifest-list digest
  (`scripts/base/pins:8-11`), so no new Debian digest is needed, only a check that the index lists
  `linux/arm64`. Bun is x64-only: `BUN_SHA256` is the `bun-linux-x64-baseline.zip` hash and the
  `Dockerfile` downloads that zip (`scripts/base/Dockerfile:43-45`).
- `build-guest.sh` (Phase 1) takes `--target`; `build-rootfs.sh` gains `--runtime` (Phase 3).
- `build-rootfs-lima.sh` builds the minimal aarch64 image and `start-vamd-lima.sh` compiles `vamd`
  in Lima. `guest_build_guards.rs:39` requires at least two `build-rootfs*.sh` scripts: retiring
  the Lima script changes that test.
- `firecracker-assets.sh` sources `assets.pins` by key name; the lane cache key hashes the file
  (rust.yml:310).
- Whether GitHub's `ubuntu-24.04-arm` runners expose `/dev/kvm` is unknown (open question 1).
  Process scenarios need no KVM.

#### Scenario delta

No new scenario file: the aarch64 path is the same behaviour on another machine.

| Scenario | Change | Layer | Given | Why |
|---|---|---|---|---|
| `node-init-check-names-what-is-missing`, `node-init-run-twice-changes-nothing-and-shows-the-key-once`, `node-init-from-a-release-bundle-needs-no-checkout` | unchanged files; also run by a new advisory `rust.yml` job on `ubuntu-24.04-arm` | process | unchanged | `check_platform` and the per-architecture asset choice on aarch64 |
| `node-reports-the-standard-base-it-boots` | an aarch64 lane variant only if an arm64 runner with `/dev/kvm` exists (open question 1); otherwise unchanged and proven by the recorded Lima run | firecracker | unchanged | the aarch64 base boots |

Other proof: unit tests for the per-architecture pins and names; a branch dry run that builds the
aarch64 bundle; the recorded Lima run on an M3 or newer Mac (RA-4.5).

#### Benchmark delta

P3, install half, on a Mac: minutes from `limactl start` to a signed-in node with no compile
(today the Lima path compiles `vamd` and uses the minimal image). Stopwatch on the team checklist.

#### Items

- [ ] **RA-4.1 aarch64 pins; the recipe builds for both.** `scripts/base/pins` gains the aarch64
  Bun hash (key naming per open question 2); the `Dockerfile` picks the zip by `TARGETARCH` (still
  inside the Vamfile keep set); `resolve-pins.sh` resolves both and checks the Debian index lists
  `linux/arm64`. `scripts/assets.pins` gains the aarch64 Firecracker hash and kernel URL and hash
  (open question 3). Files: `scripts/base/{pins,Dockerfile,resolve-pins.sh}`, `scripts/assets.pins`.
  Tests: `scripts/base/check-recipe.sh`; the x86_64 lane unchanged.
  `Decide: NOW — the tracker's item (Phase 4 exit)`
- [ ] **RA-4.2 The CLI on aarch64.** `check_platform` accepts aarch64 Linux; the asset step picks
  the pins and entry names for the running architecture (pure functions of an architecture
  argument, unit-tested for both). Files: `crates/vam-cli/src/cmd/{node_init.rs,node_install.rs}`.
  Tests: `cargo test -p vam-cli node_init node_install`. `Decide: NOW — the tracker's item`
- [ ] **RA-4.3 The aarch64 node bundle.** `release.yml` job `node-aarch64` on `ubuntu-24.04-arm`
  (zigbuild `aarch64-unknown-linux-gnu.2.34`, `build-guest.sh --target aarch64`, the same kit);
  asset `vam-runtime-<v>-aarch64-unknown-linux-musl`. `rust.yml`: an advisory job on
  `ubuntu-24.04-arm` running the three `node-init-*` scenarios. Files: `.github/workflows/release.yml`,
  `.github/workflows/rust.yml`, `scripts/release/{build.sh,package.sh}`. Tests: a branch dry run;
  the arm64 job. `Decide: NOW — locked decision 11 (not scored)`
- [ ] **RA-4.4 The Lima path from the release.** `getting-started/macos-lima.md` leads with:
  `limactl start` with `scripts/lima-macos.yaml`, inside the VM download and verify the aarch64
  bundle, `sudo ./vam node init`, the macOS `vam` pointed at the forwarded port.
  `build-rootfs-lima.sh` and `start-vamd-lima.sh` stay one more release with a deprecation line
  (open question 4); `guest_build_guards.rs` adjusted if a script goes. Tests:
  `cargo test -p vam-runtime --test guest_build_guards`. `Decide: NOW — the tracker's item`
- [ ] **RA-4.5 Recorded Lima run** (owner, M3 or newer, macOS 15 or newer): the Phase 4 exit,
  written in the progress log with times. `Decide: NOW — the phase exit`

#### Waves

| Wave | Items | Turns green | Repo | Model | Budget | Depends on | Status | PR |
|---|---|---|---|---|---|---|---|---|
| 1 | RA-4.1, RA-4.2 | — (unit tests; x86_64 lane unchanged) | vam | Sonnet low | 120 min | P3.W2, P3.W3, P3.W5 (`node_install.rs` order) | [ ] | |
| 2 | RA-4.3 | the three `node-init-*` scenarios on arm64 | vam | Sonnet medium: a new runner architecture and the aarch64 musl toolchain | 150 min | 1; P5.W3 (`release.yml` order) | [ ] | |
| 3 | RA-4.4 | — | vam | Sonnet low | 75 min | 2 | [ ] | |
| close | RA-4.5 | — (recorded Lima run) | vam | owner and orchestrator | 60 min | 3; P3 release merged | [ ] | |

Lane: W1 yes (`scripts/base/`, `assets.pins` through the cache key), W2 yes (`rust.yml`), W3 yes
if a `build-rootfs*.sh` changes, else no.

#### Open questions (new)

1. **arm64 KVM on hosted runners.** Default: assume none; the Lima run is the boot proof. If
   `ubuntu-24.04-arm` exposes `/dev/kvm`, an aarch64 lane variant becomes a follow-up wave.
2. **Pin key names.** Default: keep the x86_64 keys as they are and add `_AARCH64` keys, so the
   lane's cache key and `firecracker-assets.sh` do not churn.
3. **The aarch64 kernel.** Default: the same `vmlinux-6.1.155` from the Firecracker CI bucket's
   `aarch64/` prefix, hash taken by the resolve script at wave time.
4. **The minimal Lima image.** Default: kept one more release with a deprecation line, as
   `--minimal` was; the docs lead with the release path.
5. **Mac hardware.** Default: the page states M3 or newer and macOS 15 or newer (nested
   virtualization); older Macs use a remote node.

#### Phase close

Release PR `release: release-artifacts P4` after the P3 release; then the next rc.

**Team checklist — Phase 4**

Setup: an M3 or newer Mac with macOS 15 or newer and Lima; the rc with the aarch64 bundle.

1. `limactl start` as the page says. **See:** the VM is up; `ls /dev/kvm` inside works.
2. Inside the VM: download the aarch64 bundle and `SHA256SUMS`; verify. **See:** OK.
3. `sudo ./vam node init`. **See:** the base built for aarch64; Firecracker and the kernel fetched
   and verified; `vamd` active. Write down the minutes.
4. On the Mac: the macOS `vam` with `config set-host https://localhost:<forwarded port>`, `set-ca`,
   `login`, `node status`. **See:** `node: vamd <rc> (…) release`; `base: vam/base:2 (…, verified)`
   with python and git.
5. A chat. **See:** it boots and answers.

Report: "Phase 4 pass", or the step number with the error text.

### Phase 5 — `vam` on the `PATH` and `vam upgrade`

**Goal:** one command installs `vam` on the user's `PATH` on Linux, macOS and Windows, and one
command upgrades it to a newer release after verifying it.  
**Depends on:** Phase 2 (can run beside Phases 3 and 4)  
**Plan:** drafted 2026-11-17 (breakdown; draft: the orchestrator re-validates against the code at
phase start), waiting for the owner. W0-W2 may run before Phase 2 closes (Execution plan, lane E);
the phase closes after rc.1  
**Exit:** scenarios green; `install.sh` and `install.ps1` are release assets and work on the
three OSes from a clean user account; `vam upgrade` reaches the newer release from a fixture
release server and refuses a tampered download. The real upgrade from one published rc to the next
is shown at the next rc, and from the last rc to `v0.1.0` in Phase 6 (no earlier release holds
`vam upgrade`).

- [ ] `vam dock` / `vam undock` (`--dir`, `--no-modify-path`), per OS (design §5.7)
- [ ] `install.sh` / `install.ps1` assets (pick archive, check `SHA256SUMS`, run `vam dock`)
- [ ] `vam upgrade` (`--check`, `--version`, `--pre`): GitHub API, hash and attestation check,
      self-replace (rename aside on Windows), refuses a downgrade without `--version`
- [ ] Docs: `getting-started/releases.md`, `getting-started/cli.md`

<!-- Breakdown 2026-11-17 (phase-breakdown agent, Opus 5.5 high). Draft: re-validate at phase start.
This breakdown also answers tracker open question 13 (below, open question 1). -->

#### Code map (2026-11-17)

- No `dock`, `undock` or `upgrade`; the `Commands` enum is `main.rs:33-145`. `vam-cli` already has
  `reqwest` (rustls), `sha2`, `tar`, `flate2` (`crates/vam-cli/Cargo.toml:19-36`): downloading,
  hashing and extracting a `.tar.gz` need no new crate. A Windows `.zip` and the Windows `Path`
  edit need new `cfg(windows)` dependencies.
- `releases/latest` ignores pre-releases: until `v0.1.0`, `…/releases/latest/download/install.sh`
  answers 404 and "the latest release" has no rc (design §5.7 assumes a latest release exists).
- The first release that holds `vam upgrade` has no earlier release holding it, so "`vam upgrade`
  from the previous release" (this phase's exit) can only be shown from the next rc on (open
  question 2).
- The e2e `cli` step runs one shared `vam` (`VAM_E2E_CLI`): `vam upgrade` and `vam dock` must act on
  a copy inside the scratch tree (`copyCli`, P0.W0) with `HOME` inside the tree (`home: tree`).

#### Scenario delta

| Scenario | Change | Layer | Given | Why |
|---|---|---|---|---|
| `dock-puts-vam-on-the-path-once` | new | process | none; fixture `dock/home` (a fake home with `.profile` and `.zshrc`) | with `home: tree` and a `PATH` without the directory: `vam dock --dir {{root}}/home/.local/bin` copies `vam` there and adds one marked line to `.profile` and `.zshrc`; a second run prints `unchanged` and each file holds the line once (`files` `count: 1`); `vam dock --no-modify-path` prints the line and changes no file; `vam undock` removes the binary and both lines. Red: `dock` is not a command |
| `upgrade-refuses-a-download-whose-hash-does-not-match` | new | process | `release-server` (new piece) with overlay `release-server-bad-sum` (one wrong `SHA256SUMS` line) | the tree's copy of `vam` runs `vam upgrade --version v9.9.9`: exit non-zero, the output names the hash mismatch, the copy is byte-for-byte unchanged. Red: `upgrade` is not a command |
| `upgrade-replaces-vam-with-the-newer-release` | new | process | `release-server` | `vam --version` and `vam node status` make no request to the server (request count 0: the CLI never calls the network unasked); `vam upgrade --check` names `v9.9.9` and changes nothing; `vam upgrade --pre` replaces the copy (the fixture archive holds a `vam` that prints `vam 9.9.9 (fixture)`), the output says the attestation was not checked because `gh` is absent; the replaced file then prints `vam 9.9.9 (fixture)` (the downgrade refusal and "up to date" are unit tests). Red: `upgrade` is not a command |

Notes for wave 0: the `release-server` piece is new runner code (`crates/vam-e2e/src/release_server.rs`):
the runner serves, on loopback, a GitHub-like `releases` API (latest, by tag, the list with
pre-releases) and the assets of a fixture directory, and counts requests. The built `vam` reaches
it only through the test seam of open question 3. Red proofs with `scripts/e2e/run.sh --scenarios …`.

#### Benchmark delta

P3, install half, for the CLI: one command and the minutes from nothing to `vam` on the `PATH` on
each OS (team checklist).

#### Items

- [ ] **RA-5.1 `vam dock` and `vam undock`.** Per design §5.7: the per-user directory per OS;
  marked lines (`# added by vam dock`) in the listed login profiles on Linux and macOS (never system
  files); on Windows the user `Path` in `HKCU\Environment` and a settings-changed broadcast;
  `--dir`, `--no-modify-path`; idempotent; `undock` removes only what `dock` wrote. Also adds the
  `upgrade` subcommand as a stub ("not built yet") so W2 does not touch `main.rs`. Files:
  `crates/vam-cli/src/{main.rs,cmd/mod.rs,cmd/dock.rs}`, `crates/vam-cli/Cargo.toml`
  (`cfg(windows)` dependencies, open question 4), `Cargo.lock`. Tests: unit tests per OS on temp
  homes (the Windows part on the `cli (windows)` job); the scenario. Turns green:
  `dock-puts-vam-on-the-path-once`. `Decide: NOW — locked decision 15 (not scored)`
- [ ] **RA-5.2 `vam upgrade`.** `--check` (reads only), `--version vX` (pins, may downgrade),
  `--pre` (allows an rc). Reads the GitHub releases API (with `GH_TOKEN` or `GITHUB_TOKEN` when set,
  never stored); picks the archive for the target `vam` was built for; downloads it and
  `SHA256SUMS`; checks the hash; then `gh attestation verify <archive> -R squrell-inc/VAM
  --signer-workflow squrell-inc/VAM/.github/workflows/release.yml` when `gh` is on the `PATH`, else
  one line saying it was not checked and the command to check it (open question 1); extracts to a
  sibling temp file; replaces itself (Unix: rename over the running file; Windows: rename the
  running `vam.exe` to `vam.exe.old`, move the new one in, remove `.old` on the next run). Refuses a
  downgrade without `--version`; refuses inside a node bundle (`vamd` beside `vam`, open question
  6); never touches `vamd`. Test seam per open question 3. Files: `crates/vam-cli/src/cmd/upgrade.rs`
  (new), `crates/vam-cli/Cargo.toml`, `scripts/e2e/run.sh` (the seam feature),
  `scripts/e2e/check-release-gate.sh` (the seam is absent from the default tree). Tests:
  `cargo test -p vam-cli upgrade`; the two scenarios. Turns green:
  `upgrade-refuses-a-download-whose-hash-does-not-match`, `upgrade-replaces-vam-with-the-newer-release`.
  `Decide: NOW — locked decision 15 (not scored)`
- [ ] **RA-5.3 `install.sh` and `install.ps1`.** Release assets made in `assemble` and listed in
  `SHA256SUMS`: pick the archive for the OS and CPU; `--version vX` (required while no final
  exists, open question 2); download the archive and `SHA256SUMS`; check; extract to a temp
  directory; run `vam dock`; print each step first. `release.yml`: an `install-smoke` job on
  `ubuntu-24.04`, `macos-15` and `windows-2025` that runs the scripts against the run's own
  artifacts (a `--from DIR` mode), then `vam --version` and `vam undock`. Files:
  `scripts/release/{install.sh,install.ps1}` (new), `.github/workflows/release.yml`,
  `scripts/release/check-workflow.sh`. Tests: `shellcheck`, PSScriptAnalyzer in the job; a branch
  dry run. `Decide: NOW — locked decision 15 (not scored)`
- [ ] **RA-5.4 Docs.** `getting-started/releases.md` (the one-liners, with the tag-specific URL
  until `v0.1.0`), `getting-started/cli.md` (`dock`, `undock`, `upgrade`); unsigned binaries:
  macOS quarantine for a browser download, Windows SmartScreen (open question 5).
  `Decide: NOW — the tracker's item`
- [ ] **RA-5.5 The real upgrade.** At the rc after the one that ships `vam upgrade`: on Linux,
  macOS and Windows, `vam upgrade --pre` from the earlier rc reaches it; recorded in the progress
  log (Execution plan: rc.3 to rc.4). Then again in Phase 6. `Decide: NOW — the phase exit`

#### Waves

| Wave | Items | Turns green | Repo | Model | Budget | Depends on | Status | PR |
|---|---|---|---|---|---|---|---|---|
| 0 | scenarios (+ `release-server` piece) | — (pending, red for the right reason) | vam | Sonnet low | 120 min | P3.W0 (runner files order) | [ ] | |
| 1 | RA-5.1 | `dock-puts-vam-on-the-path-once` | vam | Sonnet medium: three OSes and new Windows dependencies | 150 min | 0; P0.W1 (`main.rs`), P3.W1 (`cmd/mod.rs`) | [ ] | |
| 2 | RA-5.2 | `upgrade-refuses-a-download-whose-hash-does-not-match`, `upgrade-replaces-vam-with-the-newer-release` | vam | Sonnet high: it replaces the running binary, and the checks must come before the replace | 180 min | 1 | [ ] | |
| 3 | RA-5.3 | — (`install-smoke` green on three OSes in a branch dry run) | vam | Sonnet medium: scripts that download and run a binary on users' machines | 150 min | 1; P3.W6 (`release.yml` order) | [ ] | |
| 4 | RA-5.4 | — | vam | Sonnet low | 60 min | 2, 3; P3.W7 (docs order) | [ ] | |
| close | RA-5.5 | — (recorded at the next rc) | vam | orchestrator and owner | 45 min | rc after the P5 rc | [ ] | |

W2 and W3 may run side by side (different files). Lane: W0 yes (`crates/vam-e2e`, `e2e/`), W2 yes
(`scripts/e2e/`); W1, W3, W4 no.

#### Open questions (new)

1. **Tracker open question 13, answered here.** Default: no in-process Sigstore verifier in this
   phase (it adds a trust-root fetch and a large dependency tree to a small CLI); the hash is
   always checked; `gh attestation verify` runs when `gh` is present; otherwise one line says the
   attestation was not checked and how to check it. Backlog trigger: "a user without `gh` asks, or
   the `cosign` bundle for `SHA256SUMS` lands".
2. **Before the first final.** Default: `install.sh` and `install.ps1` without `--version` stop
   with "no final release yet; pass --version v0.1.0-rc.N"; `vam upgrade` without `--pre` says the
   same. The real upgrade is recorded at the rc after the one that ships `vam upgrade` (RA-5.5).
3. **Test seam.** Default: a `vam-cli` cargo feature `upgrade-seam` that honours
   `VAM_UPGRADE_API_URL` for loopback URLs only; `run.sh` builds the e2e `vam` with it; the release
   gate's default-tree check refuses it, as it refuses `process-guest`.
4. **Windows dependencies.** Default: `winreg` for `HKCU\Environment` and `windows-sys` for the
   broadcast, `cfg(windows)` only; `zip` for the Windows archive in `vam upgrade`; licences pass
   `deny.toml`.
5. **Unsigned binaries.** Default: the docs explain macOS quarantine (a browser download; `curl`
   sets none) and Windows SmartScreen; code signing is later.
6. **`vam upgrade` inside a node bundle.** Default: refused when `vamd` sits beside `vam` ("upgrade
   the node from its bundle"), so a node's CLI never drifts from its bundle unseen.

#### Phase close

Scenarios active and green; `install-smoke` green. Release PR `release: release-artifacts P5`
after rc.1, then the next rc. Deploy note: none.

**Team checklist — Phase 5**

Setup: the rc with the install scripts; a Linux, a macOS and a Windows user account without `vam`.

1. Linux: `curl -fsSL https://github.com/squrell-inc/VAM/releases/download/<tag>/install.sh | sh
   -s -- --version <tag>`. **See:** each step printed first, the hash checked, `vam dock` done; a
   new terminal runs `vam --version`. Write down the minutes.
2. Run it again. **See:** `unchanged`.
3. macOS (zsh): the same. **See:** `~/.zprofile` holds one marked line.
4. Windows PowerShell: the `install.ps1` one-liner from the releases page, with the version.
   **See:** a new terminal runs `vam --version`.
5. `vam upgrade --check --pre`. **See:** up to date, or the newer rc named; nothing changed.
6. `vam undock` on each. **See:** the binary and the marked lines are gone.
7. On a node host, inside the bundle directory: `./vam upgrade`. **See:** refused, "upgrade the
   node from its bundle".

Report: "Phase 5 pass", or the step number with the error text.

### Phase 6 — Final `v0.1.0`

**Goal:** the first final release, once the release candidates have settled.  
**Depends on:** Phases 2–5 closed, each with its rc; the owner's word that the rcs have settled  
**Plan:** drafted 2026-11-17 (breakdown; draft: the orchestrator re-validates against the code at
phase start), waiting for the owner  
**Exit:** prepare PR `0.1.0` merged; `v0.1.0` published (not a pre-release) and verified with
`gh release verify` and `gh attestation verify`; `vam upgrade` from the last rc reaches `v0.1.0`;
the docs' releases page names `v0.1.0`.

- [ ] Owner's rc check: a server, WSL2, Lima (M3+), a laptop CLI on each OS, all on the last rc
- [ ] Prepare PR, dispatch, approve, verify; release notes cover every rc since nothing
- [ ] Docs: releases page and `project-status.md`

<!-- Breakdown 2026-11-17 (phase-breakdown agent, Opus 5.5 high). Draft: re-validate at phase start. -->

#### Scenario delta

None new. Every scenario of Phases 0-5 is active, and the `rust.yml` push run on the prepare commit
(guard 3) is green with all of them and the lane. What replaces scenarios: design §9 "Every
release" run end to end, and the recorded `vam upgrade` from the last rc.

#### Benchmark delta

None. Launch proof "the install" is met from a final release.

#### Items

- [ ] **RA-6.1 The owner's rc check** on the last rc: a server, WSL2, Lima (M3 or newer), and a
  laptop CLI on Linux, macOS and Windows (the Phase 3, 4 and 5 checklists, shortened).
  `Decide: NOW — the tracker's item`
- [ ] **RA-6.2 `v0.1.0`.** Prepare PR `0.1.0` merged; dry run; publish dispatch (`--latest`, not a
  pre-release); owner approves; `gh release verify v0.1.0`, `gh attestation verify` on every
  asset; `curl -fsSL https://github.com/squrell-inc/VAM/releases/latest/download/install.sh | sh`
  now works without `--version`; `vam upgrade` (no `--pre`) from the last rc reaches `v0.1.0` on
  three OSes. The notes cover every `release:` merge, grouped by rc (Phase 1 open question 9).
  `Decide: NOW — the phase exit`
- [ ] **RA-6.3 Docs.** `getting-started/releases.md` names `v0.1.0` and drops the rc-only URL
  form; `project-status.md` links the release. A docs-only PR into `main` after the publish.
  `Decide: NOW — the tracker's item`

#### Waves

| Wave | Items | Turns green | Repo | Model | Budget | Depends on | Status | PR |
|---|---|---|---|---|---|---|---|---|
| 1 | RA-6.3 | — | vam | Sonnet low | 45 min | `v0.1.0` published | [ ] | |
| close | RA-6.1, RA-6.2 | — (`v0.1.0` published and verified) | vam | owner and orchestrator | 180 min | Phases 2-5 closed; the owner's word | [ ] | |

Lane: no wave touches a VM path (the prepare PR's push run runs the lane anyway).

#### Open questions (new)

1. **When the rcs have "settled".** Default: the last rc passes RA-6.1 and no fix rc was needed for
   three working days of the owner's use; the owner can say so earlier.
2. **A fix found in the rc check.** Default: a fix PR into `main`, then the next rc (never straight
   to the final), then RA-6.1 again on what changed.

#### Phase close

Mark the workstream done; delete the tracker and its README row (playbook §7).

**Team checklist — Phase 6:** design §9 "Every release", steps 1-7, for `v0.1.0`; then the
platform list of RA-6.1 on the final; then `vam upgrade` from the last rc on Linux, macOS and
Windows. Report: "Phase 6 pass", or the step number with the error text.

**Release candidates first** (owner, 2026-11-17): every phase close from Phase 2 on publishes the
next `v0.1.0-rc.N`; no final until Phase 6.

## Open questions

Each has a default; see design §10 for the reasoning.

1. First release number — decided: `v0.1.0-rc.N` per phase close from Phase 2; `v0.1.0` in Phase 6.
2. Pre-1.0 policy — minor may break; notes carry "Upgrade notes".
3. Restrict `v*` tag creation — no; updates and deletions restricted; Phase 2 tests the token.
4. glibc floor — 2.34 via `cargo-zigbuild`.
5. macOS Intel CLI — yes while `macos-15-intel` exists.
6. Windows CLI — native `vam.exe` in Phase 3; WSL stays the node path.
7. CI-green guard — the `rust.yml` push run on the exact SHA; no suite re-run in the release.
8. Changelog file — none.
9. Guest init profile — as `build-rootfs.sh` builds it today.
10. Version bump — a prepare PR at release time; no post-release PR.
11. Who dispatches — owner approves every publish; any writer may dry-run.
12. Brokers installed by default — none; `--broker` names them.
13. `vam upgrade` attestation check — hash always, `gh attestation verify` when `gh` exists;
    in-process Sigstore decided at the Phase 5 breakdown.
14. `PATH` edits — marked lines in the user's login profiles only; `--no-modify-path` opts out.

## Explicitly later

- Published `rootfs.ext4` with a source archive, after a licence review.
- `cosign` bundle for `SHA256SUMS`; SLSA Build L3 via a reusable workflow; rebuild check.
- `vam node upgrade`, `vam node init --from-release`, package managers, release branches.

## Progress log

| Date | Phase | What landed (end with the scoreboard: scenarios green, red, fix cycles, wall time) |
|---|---|---|
| 2026-11-17 | — | Tracker opened with design `design/release-artifacts.md`; plan proposed, waiting for the owner's approval. Scoreboard: no scenarios yet, 0 fix cycles |
| 2026-11-17 | — | Owner review: version chosen at release time by a prepare PR (no post-release bump, dev builds `+dev.N`); Phase 3 gains broker install and `build-rootfs.sh --runtime`; recipe kept inside the Vamfile grammar (decision 13); new Phase 4 (aarch64 node for Lima) and Phase 5 (`vam dock`, `vam upgrade`); walkthrough in design §11. Still waiting for approval |
| 2026-11-17 | 0-6 | **Breakdowns of all phases drafted** (phase-breakdown agent, Opus 5.5 high; the owner asked for the whole plan in one pass with parallel lanes; Phases 3-6 are drafts re-validated at phase start), waiting for the owner. New "Execution plan": one `feat/` base per phase, P1.W1-W2 ride the P0 release so `release.yml` reaches `main` early, lanes A-F with a file-order table, 12 owner gates, rc.1-rc.4 then `v0.1.0`, about 47 h on the critical path (about 88 h serial). Scenario delta: P0 4 new; P1 3 guards-only; P2 none (lint, scratch rehearsal, verified rc.1); P3 4 new, 2 changed; P4 none new (arm64 run of three `node-init-*`, Lima run); P5 3 new; P6 none. Waves: P0 4 (low, medium, medium, low), P1 4 + close (high, low, medium, medium), P2 1 + close (high), P3 8 (low ×3, medium ×3, high, low), P4 3 + close (low, medium, low), P5 5 + close (low, medium, high, medium, low), P6 1 + close (low). Found in the code: `hsp.rs:489` is a test and `Register` carries no broker version back (RA-3.5); the Debian pin is already multi-arch; `rust.yml` cancels `main` push runs; guard 7 makes a run about 40 min; no `latest` release during the rcs; `node init` cannot register brokers without a key on a re-run (RA-3.6). Scoreboard: 0 green, 0 red (not written), 0 fix cycles |
