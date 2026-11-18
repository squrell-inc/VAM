# Workstream: Release artifacts (version, tag, build, publish, install)

```yaml
status: active
slug: release-artifacts
current_phase: A (restructured 2026-11-18; waiting for the owner's approval of the breakdown)
base_branch: main (cross-cutting; Phase A from feat/release-artifacts-a, Phase B from feat/release-artifacts-b, one release PR each)
design_docs:
  - design/release-artifacts.md
  - docs/src/content/docs/getting-started/installation.md
```

## Goal

A published VAM release is one version for the whole machine, built by one manually started,
owner-approved workflow from one green commit on `main`, published under an immutable `v*` tag
with `SHA256SUMS`, build-provenance attestations and `cargo auditable` binaries. For `v0.1.0`: a
node installs from the x86_64 node bundle (Linux server or WSL2) without a Rust toolchain or a
git checkout, brokers included; `vam` installs on Linux and macOS with one command and upgrades
itself with another. Lima (aarch64 node) and native Windows come in `v0.2.0`.

## Out of scope

- crates.io, Homebrew, winget, apt.
- In-place node upgrade (`vam node upgrade`) with state migration and rollback.
- A `vamd` container image (design §5.2).
- Publishing `rootfs.ext4`, Firecracker or the guest kernel (the node builds the image from the
  shipped recipe and fetches the rest from pins; a published image waits for a licence review).
- The Vamfile builder itself (`agent-experience` E7 Phase 3); this workstream only keeps the
  shipped recipe inside its grammar.
- Catalog item versions (agents, skills, playbooks have their own semver).
- In `v0.1.0` (owner, 2026-11-18): the aarch64 node bundle and the Lima page, and native Windows
  (`vam.exe`, `install.ps1`, `vam dock` on Windows). They are listed under "Next release: v0.2.0
  (Lima and Windows)" below, not broken down.
- CycloneDX SBOM files and their attestation, and a proto descriptor asset ("Explicitly later").

## Locked decisions

Do not re-litigate without editing this list. **Locked 2026-11-17 (owner approved the plan).
Decisions 3, 5, 7, 8, 10, 11, 14 and 15 changed 2026-11-18 (owner), each marked.**

1. One semver version for the workspace in `[workspace.package]`; every crate inherits it
   (`vam-firecracker` included). Tags are `v<version>`. Pre-release label `-rc.N` only.
2. The version is chosen at release time, never guessed ahead: a prepare PR
   (`scripts/release/prepare.sh <v>`, title `release: prepare v<v>`) sets it just before the
   release, and its merge is the commit that is tagged. No post-release bump PR. Pre-1.0: a minor
   may break; a patch only fixes. The workflow never pushes to `main`.
3. A build is a release build only when the release workflow sets `VAM_BUILD_CHANNEL=release`.
   Every other build reports the nearest `v*` tag plus build metadata (`0.3.0+dev.14 (<sha>)`),
   never a future version. Build info is read from git only in the binary crates' build scripts
   (`vam-cli`, `vam-orchestrator` for `vamd`, the three brokers), through one shared helper in
   `vam-spec` that takes the values as arguments, with `VAM_GIT_SHA` / `VAM_COMMIT_DATE`
   overrides; no git-reading `vam-spec/build.rs`. `vam`, `vamd`, each broker and `GET /v1/node`
   report version and commit the same way; `vam node status` shows the node's and the CLI's.
   **Changed 2026-11-18 (owner):** was "one build-info source in `crates/vam-spec/build.rs`";
   every crate depends on `vam-spec`, so that would rebuild most of the workspace on each commit.
4. Releases start only by `workflow_dispatch` (`version`, `commit`, `dry_run` default `true`)
   from `main`, and publish only after the owner approves the protected `release` environment
   (owner as required reviewer, `main` only, admin bypass off). No tag-on-merge.
5. Guards in `validate`: commit on `main`'s first-parent history; the `rust.yml` push run on that
   SHA (or a `workflow_dispatch` run of `rust.yml` on `main` at that SHA) succeeded; `Cargo.toml`
   version equals the input; tag and release absent; version greater than every `v*` tag;
   `cargo deny check advisories` passes; inputs reach shell only through `env:`. Builds may run
   beside `validate`; the publish job `needs` both. **Changed 2026-11-18 (owner):** guard 7 no
   longer re-runs `check-release-gate.sh` (`rust.yml` `checks` already runs it on every push to
   `main`, `rust.yml:121-123`, and guard 3 requires that run green); a fast advisories check
   takes its place, because new advisories appear after the push run.
6. Tags are immutable: immutable releases on; a `v*` ruleset restricts updates and deletions with
   no bypass. A tag is never moved, deleted or reused; a failed publish takes the next number.
7. The publish job creates a draft release targeting the exact commit, uploads every asset,
   attests, verifies, then publishes; GitHub creates the tag when the draft is published (A.W6
   confirms on GitHub's documentation and the rc.1 run shows it). A failure before publish leaves
   no tag: the job deletes its draft and the same number may run again. Nothing is public before
   the owner's approval. **Changed 2026-11-18 (owner):** no public scratch-repository rehearsal
   and no owner gate for one; no dry run before every publish (the publish run stops at the
   environment approval with the same summary; `dry_run: true` stays available but is not a
   checklist step).
8. Native GitHub-hosted standard runners per target; `cargo-zigbuild` only for the Linux glibc
   floor; no `cross`, no `cargo-dist`. No caches in release builds. Actions pinned by full SHA.
   **Changed 2026-11-18 (owner):** the build and assemble jobs live in a reusable workflow
   (`release-build.yml`, `workflow_call`) that `release.yml` calls and that also runs build-only
   (no publish job, no secrets) on release-path PRs and weekly, in place of branch dry runs. If
   the A.W4 spike shows zigbuild cannot build `aws-lc-sys` and bundled SQLite at glibc 2.34
   under `cargo auditable`, the Linux builds run in an AlmaLinux 9 container pinned by digest on
   the same runner instead.
9. Build jobs have read-only permissions and no secrets; the publish job holds only
   `GITHUB_TOKEN` with `contents: write`, `id-token: write`, `attestations: write`.
10. Provenance: `SHA256SUMS`, GitHub build-provenance attestations, the immutable release
    attestation, `cargo auditable` binaries. No long-lived signing key. **Changed 2026-11-18
    (owner):** the CycloneDX SBOM files and the SBOM attestation are deferred to later.
11. Shipped in `v0.1.0`: the x86_64 Linux gnu node bundle (`vamd`, `vam`, `vam-web-broker`,
    `vam-schedule-broker`, `vam-notify-broker`, `vam-runtime`, the standard base recipe); `vam`
    alone for Linux x86_64/aarch64 and macOS arm64/x86_64; the guest init as static musl, built
    exactly as `build-rootfs.sh` builds it; `install.sh`; `SHA256SUMS`. Not shipped:
    `hsp-echo-broker`, `vam-e2e`, `verify_chain`. **Changed 2026-11-18 (owner):** no proto
    descriptor asset (no consumer); the aarch64 node bundle and every Windows asset (`vam.exe`,
    `install.ps1`) move to `v0.2.0`.
12. No `CHANGELOG.md`: release notes are generated from the first-parent `release:` merges and
    their deploy notes, and reviewed at the approval gate.
13. No image is shipped. The node bundle carries the standard base recipe (`share/vam/base/`)
    and the attested `vam-runtime`; `build-rootfs.sh --runtime PATH` injects it as `/sbin/init`
    without compiling. The recipe stays inside the planned Vamfile grammar and becomes the first
    standard Vamfile when E7 Phase 3 lands (`vam image build` replaces `build-rootfs.sh`; the
    bundle layout does not change).
14. Brokers run as their own systemd units, installed by `vam node init --broker <names>` and
    registered through the API with the first-run admin key (or `VAM_API_KEY` on a re-run); a
    re-run without a key prints how to register. Their credentials stay in the vault, never in a
    unit or its environment. **Changed 2026-11-18 (owner):** no `vamd --register-broker`
    one-shot; the broker version in `RegisterResponse` is deferred.
15. `vam dock` puts `vam` on the user's `PATH` without admin rights (per-user directory, marked
    profile lines on Linux and macOS); `vam upgrade` checks the hash and the attestation before
    replacing itself and never touches `vamd`. The CLI never calls the network unasked.
    **Changed 2026-11-18 (owner):** the Windows part (the user `Path`, renaming a running
    `vam.exe` aside) moves to `v0.2.0`.

## Status legend

- `[ ]` not started
- `[~]` in progress
- `[x]` done (code + tests, or merged)

## Execution plan (all phases)

Drafted 2026-11-17; **restructured 2026-11-18** by the phase-breakdown agent on the owner's
decision after an independent review (seven phases became A, B and Final; one rc per milestone;
Lima and Windows moved to `v0.2.0`). The owner asked for the plan of every phase in one
pass, with parallel work wherever it is safe. That overrides "one phase per breakdown" and "one
wave at a time" (playbook §3) for this workstream only. The lane conflict rule still holds
(playbook §11.2): two lanes never build in the same file at the same time. Phase B and Final are
re-validated by the orchestrator against the code at their start.

### Branches

Every base is `feat/…`, because `rust.yml` treats a PR into `main` from `feat/*` as a release PR
and forces the microVM lane (`rust.yml:69-77`, `578-595`).

| Base | Cut from | When | Carries | Release PR into `main` |
|---|---|---|---|---|
| `feat/release-artifacts-a` | `main` | now | Phase A | `release: release-artifacts A`; then the prepare PR `0.1.0-rc.1` and the publish |
| `feat/release-artifacts-b` | `feat/release-artifacts-a`, right after A.W0 merges (about 2 h in) | day one, own cloud session | Phase B | opens only after rc.1 is published; the B base takes `main` first, so the PR shows Phase B only |
| none | — | — | Final: the prepare PR `release/prepare-v0.1.0` and one docs PR into `main` | — |

- **Why B starts from the A base.** B.W0 then adds its runner fields on top of A.W0's, and no
  runner file is edited on two bases at once. The A base holds only A.W0 (and perhaps W2 or W3)
  at that point; B's PR into `main` opens after A's release anyway.
- **Wave PRs.** Branch `ra-a-w<W>` or `ra-b-w<W>`, worktree under `.claude/worktrees/`, PR into
  the phase base, title `RA-A W<W>: <summary>` (or `RA-B`), ready for review (Actions is the
  gate). Merge commit only (`gh pr merge --merge --match-head-commit <sha>`), never squash
  (playbook §4).
- **Release PRs.** Merge commit, on the owner's word. No wave merges into a base while that base's
  release PR is open.
- **Between the bases.** Before a B wave that follows an A wave in the file table below, the
  orchestrator merges the A base into the B base (merge, never rebase; no conflict expected,
  since the files were never edited on both). After the A release merges, the B base takes
  `main` instead and the A base is deleted.
- **Prepare PRs.** Branch `release/prepare-v<v>` from `main`, title `release: prepare v<v>`, only
  `Cargo.toml` and `Cargo.lock`, merge commit on the owner's word. It is not a `feat/` branch, so
  the PR does not force the lane; the push run on `main` after the merge does (push runs always run
  the lane), and guard 3 reads that run. Open a prepare PR only when no other PR into `main` is
  open, so the prepare merge is the head that gets tagged.
- **Dispatch.** A publish runs only from `main` (guard 1): one dispatch per release, after the
  prepare PR's push run is green. Build changes are proven before that by the build-only
  `release-build.yml` runs on their own PRs (decision 8).

### Parallel lanes

A lane is one background agent at a time, each wave in its own worktree. **Recommended: one cloud
session per lane** (its own container, target directory and disk), which lifts the cap of two
building lanes that a shared `CARGO_TARGET_DIR` sets in one container (working agreement). Peak:
four sessions (A1, A2, A3, B1) for the first few hours; B2 joins when A.W5 merges.

```text
 A base (now)
 A1  A.W0 ─► A.W1 ────────────► A.W5 ─► A.W7 ──┐
 A2  A.W2 ──────────► A.W4 ──────────► A.W6 ────┼─► A release ─► prepare rc.1 ─► publish rc.1 ──┐
 A3  A.W3 ──┘ (A.W4 needs A.W2 and A.W3)        │                                                │
 O   owner: design §9 one-time settings ────────┘ (before the rc.1 publish)                      │
                                                                                                  ▼
 B base (from the A base after A.W0; own cloud session)                                (B takes main)
 B1  B.W0 ─► B.W1* ─► B.W2* ─────────────────┐
 B2          B.W4* (after A.W5) ──────────────┼─► B.W3* (after A.W6) ─► B.W5 ─► B release ─► prepare rc.2 ─► publish rc.2
                                                                                                  │
 rcs settled (owner's word) ─► Final: prepare 0.1.0 ─► publish v0.1.0 ─► upgrade rc.2 → v0.1.0 recorded ─► docs PR

 * the B base first takes the A base by merge (file table)
```

- A.W1, A.W2 and A.W3 run together from the start: build info, the release scripts and
  workflow, and the guest build share no file (A.W2 owns `rust.yml` and `scripts/ci/local.sh`
  for the whole phase, and wires in the paths A.W3 adds).
- B needs no published release until its close: `vam dock` and `vam upgrade` are proven against a
  fixture release server; the real upgrade between two published releases is recorded in Final.
- In one shared container (two building lanes), A.W3 runs after A.W0 in lane A1 and B waits for
  a free lane: about 3-4 h more on the critical path.

**Files that serialise waves** (a later wave starts after the earlier one's PR is merged; across
bases, the B base takes the A base first):

| File | Order |
|---|---|
| `.github/workflows/rust.yml`, `scripts/ci/local.sh` | A.W2 only (it also adds `scripts/release/build-guest.sh` to the lane regex, `rust.yml:84`, and cache key, `rust.yml:310`) |
| `.github/workflows/release.yml`, `release-build.yml`, `scripts/release/check-workflow.sh`, `scripts/release/package.sh` | A.W2 ─► A.W4 ─► A.W6 ─► B.W3 |
| `scripts/build-rootfs.sh`, `scripts/e2e/firecracker-assets.sh`, `crates/vam-runtime/tests/guest_build_guards.rs` | A.W3 only |
| `crates/vam-e2e/src/{cli.rs,scenario.rs}`, `e2e/README.md`, `scripts/e2e/run.sh` | A.W0 ─► B.W0 ─► B.W2 (`run.sh` only) |
| `scripts/e2e/check-release-gate.sh` | A.W2 ─► B.W2 |
| `crates/vam-cli/src/main.rs` | A.W1 ─► B.W1 |
| `crates/vam-cli/src/cmd/node.rs`, `node_install.rs`, `node_init.rs` | A.W1 (`node.rs`) ─► A.W5 ─► B.W4 |
| `Cargo.lock` | A.W2 (only if an advisory needs `cargo update`) ─► B.W1, B.W2 (only if they add a crate); prepare PRs only while no PR with a lock change is open against `main` |
| `docs/.../getting-started/releases.md`, `cli.md` | A.W7 ─► B.W5 |

### Owner gates

1. Approve this restructured breakdown and the changed decisions (3, 5, 7, 8, 10, 11, 14, 15);
   answer or accept the open-question defaults.
2. Approve each wave plan after the orchestrator validates it (standing rule).
3. Before the rc.1 publish: the one-time settings of design §9 (environment `release`, immutable
   releases, `v*` ruleset, SHA-pinned actions policy).
4. Each release PR into `main`: A, then B (merge commit).
5. Each prepare PR: `0.1.0-rc.1`, `0.1.0-rc.2`, any fix rc, `0.1.0`.
6. Each publish: dispatch once, read the summary at the `release` approval (guards, asset table,
   `SHA256SUMS`, notes), approve.
7. The team checklist at the A and B closes.
8. Final: the word that the rcs have settled, after the rc check on a server, WSL2 and a laptop
   CLI on Linux and macOS.
9. As always: a model step-up, a third fix cycle, any change to a locked decision.

### Release candidates (one per milestone)

| Release | Published after | Contains |
|---|---|---|
| `v0.1.0-rc.1` | Phase A close | The x86_64 node bundle with the kit (`vamd`, `vam`, three brokers, `vam-runtime`, `share/vam/base/`), so a node installs with Docker and no Rust or checkout; `vam` for Linux x86_64 and aarch64 and macOS arm64 and x86_64; the guest `vam-runtime` (musl); `SHA256SUMS`, notes, attestations |
| `v0.1.0-rc.2` | Phase B close | + `install.sh`; `vam dock`, `vam undock`, `vam upgrade`; `vam node init --broker`. The first rc a later `vam upgrade` can start from |
| `v0.1.0` | Final | prepare `0.1.0`; notes cover both rcs; `vam upgrade` from rc.2 recorded |

A broken rc is followed by the next number (decision 6): a fix rc.3 means the Final upgrade is
recorded from rc.3.

### Estimated wall time

Agent and CI time per wave: its budget plus about 30 minutes for the Actions run (lane included
where the wave touches a VM path), review and merge. A release PR with the lane about 45 min; a
prepare PR and its push run about 45 min; one publish run (about 20-25 min, design §6.3) with
verification about 1 h. Owner waits are not counted.

| Phase | Waves (budget + 30 min each) | Release steps | With the lanes |
|---|---|---|---|
| A | W0 1.75 h, W1 3.5 h, W2 3.5 h, W3 2 h, W4 3.5 h, W5 2.5 h, W6 3.5 h, W7 1.5 h (21.75 h serial) | release PR, prepare rc.1, publish and verify: 2.5 h | rc.1 at about 13 h (A.W2 ─► A.W4 ─► A.W6 is 10.5 h) |
| B | W0 2 h, W1 2.5 h, W2 3.5 h, W3 2.5 h, W4 3 h, W5 1.5 h (15 h serial), from about 2 h in | release PR, prepare rc.2, publish and verify: 2.5 h | rc.2 at about 17 h (B.W3 starts when A.W6 merges at 10.5 h; B.W5 ends at 14.5 h) |
| Final | docs PR 1 h | prepare `0.1.0`, publish, verify, upgrade recorded: 2.5 h | about 3.5 h after the owner's word |

- **Critical path:** about **21 hours** of agent and CI time (A.W2 ─► A.W4 ─► A.W6 ─► B.W3 ─►
  B.W5 ─► B release ─► rc.2 ─► Final), against 47 h in the 2026-11-17 plan. The owner's target
  was about 23 h. The lever is A.W6: if its plan turn splits it, add about an hour.
- **Serial:** about **45 hours** (A 24.25 h, B 17.5 h, Final 3.5 h), against 88 h before.
- **Calendar:** at 8-10 hours of orchestrated work a day with gates answered the same day, rc.1 on
  day 2, rc.2 on day 2 or 3, `v0.1.0` after the owner's settle window (Final open question 1),
  about one week from approval.

## Phases

### Done phases

Done phase sections are deleted; git history keeps them.

| Phase | Name | Done |
|---|---|---|

### Phase A — rc.1: a node and a CLI from a release, no Rust

**Goal:** a person downloads `v0.1.0-rc.1`, verifies it, installs a node from the x86_64 node
bundle on a host with Docker and no Rust or checkout (Linux server or WSL2), and runs `vam` on
Linux or macOS against it; every binary names its version, and a dev build never claims to be a
release.  
**Depends on:** none  
**Plan:** drafted 2026-11-18 (restructure of old Phases 0, 1, 2 and the kit items of old Phase 3),
waiting for the owner  
**Exit:** the phase's scenarios are active and green on Actions; `vam`, `vamd` and the three
brokers print `<name> <version>[+dev.N] (<sha>[+dirty][ <date>])`; `GET /v1/node` and
`vam node status` show `build`; `prepare.sh` sets the version; `cargo deny check` passes with
advisories and bans; the guard script refuses each bad input in its own test; a `release-build.yml`
PR run builds every `v0.1.0` asset with the highest `GLIBC_` symbol ≤ 2.34; `v0.1.0-rc.1` is
published, `gh release verify` and `gh attestation verify` pass on every asset, the tag points at
the dispatched commit, the ruleset refuses deleting or moving it, and a second dispatch of the
same version fails at guard 5; on a clean host, `sudo ./vam node init` from the extracted bundle
builds the base from the kit and starts `vamd` (team checklist); the docs lead with the release.

- [ ] Build info read in the binary crates; `vam`, `vamd`, the brokers, `GET /v1/node` and
      `vam node status` report it; `vam-firecracker` inherits the workspace version
- [ ] `scripts/release/prepare.sh`; `deny.toml` advisories and bans
- [ ] Guard script and tests; `release.yml` skeleton; workflow lint and `actionlint`; `rust.yml`
      keeps push runs on `main`
- [ ] `build-guest.sh` shared with `build-rootfs.sh`; `build-rootfs.sh --runtime PATH`
- [ ] Reusable `release-build.yml`: build matrix, kit in the node bundle, glibc check; build-only
      on release-path PRs and weekly
- [ ] `vam node init --kit DIR`
- [ ] Assemble (`SHA256SUMS`, notes, scans) and the publish job behind the `release` environment
- [ ] Docs: `getting-started/releases.md`, `installation.md` leads with the release
- [ ] `v0.1.0-rc.1` published and verified; design §9 amended where the run proved it wrong

<!-- Breakdown 2026-11-18 (phase-breakdown agent, Opus 5.5 high), restructured from the
2026-11-17 breakdowns of old Phases 0-3. -->

#### Code map (2026-11-17, rechecked 2026-11-18)

Version identity:

- `crates/vam-cli/build.rs:13-25` makes `VAM_GIT_SHA` (short SHA, `+dirty`, `unknown`), with no
  environment override; `main.rs:9` prints `CARGO_PKG_VERSION (VAM_GIT_SHA)`. No other crate has a
  build script that reads git; `vam-orchestrator` and the brokers have none.
- `crates/vam-spec/build.rs:1-64` compiles the protos and nothing else. A `cargo:rustc-env` from a
  build script reaches only its own package (lib and bins), so each binary crate reads git in its
  own `build.rs`. In `vam-orchestrator` that also rebuilds the library on each commit; nothing
  depends on `vam-orchestrator` (brokers never do, E10 decision 22), so the rebuild stops there.
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
- `hsp.rs:489` is a test (`client_register_and_invoke_round_trip`). In production `vamd` sends
  `broker_version: String::new()` in `RegisterRequest` (`api/mod.rs:1902`), and the brokers answer
  a `RegisterResponse` with no version (`hosted_services.proto:198-207`): deferred (decision 14).
- `crates/vam-cli/tests/cli_tests.rs:2682` asserts `vam --version` contains `CARGO_PKG_VERSION`. It
  breaks between a prepare PR and its tag (the nearest tag is then older than `Cargo.toml`).
- `crates/vam-firecracker/Cargo.toml:3` says `version = "0.1.0"`: equal today, so
  `version.workspace = true` changes no lock entry.

Release scripts and workflow:

- `deny.toml` has `[licenses]` only. `check-release-gate.sh:11-34` runs manifests, the
  vamd/guest/notify build checks, the tree check and the lane check; `rust.yml:121-123` runs it on
  every push to `main` and every PR into `main`.
- No `release.yml`, no `scripts/release/`. `rust.yml` cancels an older run of the same ref
  (`rust.yml:38-40`, `cancel-in-progress: true`): a second merge into `main` soon after a prepare
  merge cancels the prepare SHA's push run, and guard 3 then refuses it. `paths-ignore`
  (`rust.yml:4-10`): a docs-only merge has no push run at all.
- The push run's conclusion includes the microVM lane: `firecracker` has no `continue-on-error`
  (`rust.yml:252-257`), and push runs always set `vm=true` (`rust.yml:69-71`). So guard 3 also
  requires a green lane.
- The lane's `changes` regex (`rust.yml:84`) and asset cache key (`rust.yml:310`) already cover
  `scripts/build-rootfs*.sh` and `scripts/base/`; `scripts/release/build-guest.sh` must be added.
- Secret scan today: `export-public.sh:39-81` (`gitleaks dir` with `.gitleaks.toml`; refuses
  `*.ext4`, `vmlinux*`, `.env`, `*.log`).
- Release notes need the "Deploy note" sections of `release:` PR bodies: GitHub API reads (public
  repository).
- `gh release verify` and `verify-asset` check the release attestation that GitHub signs **when the
  release is published**. Before publishing, only the artifact attestations and the hashes can be
  checked.
- `releases/latest` (API and download URLs) ignores pre-releases: while only rcs exist, there is
  no "latest".

Guest build and the kit:

- The guest build sits inside `build-rootfs.sh:42-57` (musl C compiler fallback,
  `--release --target x86_64-unknown-linux-musl`, `+crt-static`); `build-rootfs-lima.sh` is the
  aarch64 copy. `guest_build_guards.rs:17-40` requires `--bin vam-runtime` in every
  `scripts/build-rootfs*.sh` and at least two such scripts: moving the cargo call into
  `build-guest.sh` moves this guard with it.
- `build-rootfs.sh` takes no `--runtime` (its argument loop, 27-33, rejects anything else); it
  stages under `<script dir>/../target/rootfs-staging` (18-20) and reads the recipe from
  `<script dir>/base/` (72, 84-91), so from `share/vam/base/` both land inside the bundle; every
  step calls `sudo` (a root host without `sudo` fails).
- `step_base` (`node_install.rs:333-398`): `--base`, else a checkout (`find_checkout`, 171-189,
  needs `scripts/build-rootfs.sh`); **refuses to build under `--root`** (378-380); runs
  `bash scripts/build-rootfs.sh` with `VAM_ROOTFS_OUT` (380-387); the stamp hashes checkout files
  (191-225).
- The vamd unit's `ExecStart` is the `vamd` beside `vam` (`node_install.rs:139`, `find_vamd`
  160-169): a re-run from a newer bundle repoints it.
- e2e `cli` step (`crates/vam-e2e/src/cli.rs`, `scenario.rs:272-293`): runs `VAM_E2E_CLI` only,
  with an empty environment (no host, no key), literal `contains`, no timeout; `root: same` reuses
  the tree (`cli.rs:159`). `run.sh:61-82` builds `vam`, `vamd` and the schedule and notify
  brokers, not `vam-web-broker`.

#### Scenario delta

| Scenario | Change | Layer | Given | Why |
|---|---|---|---|---|
| `cli-prints-its-version-and-commit` | new | process | none (`cli` steps, no tree) | `vam --version`, then `--version` of `vam-web-broker`, `vam-schedule-broker`, `vam-notify-broker`: each prints `<name> <version> (<commit>…)` matching `^<name> \d+\.\d+\.\d+(-rc\.\d+)?(\+dev(\.\d+)?)? \([0-9a-f]{7,}(\+dirty)?( \d{4}-\d{2}-\d{2})?\)$`, with the version and commit `vam` printed; the version contains `+dev` and no output says `release` (folds in the old `dev-build-does-not-claim-a-release`). Red at `when.0`: `vam --version` has no `+dev`, class `product` |
| `vamd-prints-its-version-without-starting` | new | process | none; fixture tree `version/bad-config` (an `.env` whose half-set TLS pair would stop a start) | `vamd --version` run in the tree: exit 0 within the timeout, one line `vamd <version> (<commit>)` matching the same pattern, tree unchanged (no state, no TLS, no key). Red: `vamd` has no `--version`; it reads the `.env` and exits non-zero ("cannot serve the API") |
| `node-status-shows-the-node-and-cli-versions` | new | process | org-admin | `GET /v1/node` has `status.build.version` containing `+dev`, `status.build.commit` present and `status.build.channel: dev`; `vam node status`, signed in as org-admin, prints `node: vamd <version> (<commit>) dev` and `cli: vam <version> (<commit>)`, no `minor versions differ`, and never `release`. Red at `when.0`: `status.build` absent |
| `node-init-from-a-release-bundle-needs-no-checkout` | new | process | none; fixture `node-init/bundle` (a ready host plus a bundle: a stub `vam-runtime`, `share/vam/base/` with `manifest.json` and a stub builder that writes a small image and a manifest naming the `--runtime` digest it got; `vam` copied in by `copyCli`) | `vam node init --root {{root}} --no-systemd --vamd {{vamd}}` (`cwd: root`) with the kit beside the copied `vam` and no checkout anywhere: exit 0; `done base: built from the kit`; the base manifest under the tree names the bundle runtime's sha256; the output never says `checkout`. Red: today it stops at the base step (no checkout found, or "refusing to build the base image under --root"), class `product` |
| `node-init-from-a-newer-bundle-keeps-the-state` | new | process | none; `node-init/bundle` then `node-init/bundle-newer` (another stub runtime and kit version) | `--no-start` init from the first bundle, then from the newer one on the same tree (`root: same`): the unit's `ExecStart` names the newer `vamd`; no new key printed (`already has an admin key`); `state.db` and `tls/ca.pem` unchanged (`files`); the base is rebuilt because the runtime changed. Red: as above |
| `vm-boots-and-registers-over-vsock`, `skill-uses-python-pandas-and-git-on-the-standard-base` | changed: `guards` add `vam:scripts/release/build-guest.sh` (made by A.W3, `then` unchanged) | firecracker | unchanged | the guest they boot is built by `build-guest.sh` from now on |
| `node-reports-the-standard-base-it-boots` | changed: `guards` add `vam:scripts/release/build-guest.sh` and `vam:scripts/e2e/firecracker-assets.sh` (A.W3) | firecracker | unchanged | the lane builds its image with `build-guest.sh` then `build-rootfs.sh --runtime`, so this proves a prebuilt, injected runtime boots |

Unchanged, regression set: `node-init-run-twice-changes-nothing-and-shows-the-key-once` (guards
`bin/vamd.rs`; `--init` must behave as today), `node-init-check-names-what-is-missing`, and the
marketplace scenarios (the node version they check against becomes `0.1.0+dev`).

Notes for wave 0:

- Runner additions to the `cli` step, only those Phase A uses, each with a `vam-e2e` unit test and
  a line in `e2e/README.md`: `bin` (`vam` by default; `vamd`, `vam-web-broker`,
  `vam-schedule-broker`, `vam-notify-broker` from the paths `run.sh` exports; `run.sh` builds
  `-p vam-web` and exports `VAM_E2E_WEB_BROKER`), `cwd: root`, `matches` (regexes over the
  output), `remember: { as, after }` (keeps a printed word as `{{<as>}}` for later steps; unlike
  `capture`, it may be printed), `as: <org alias>` (passes `--host`, `--token` and `VAM_CA_FILE` as
  `via: cli` does, `notifications.rs:535-563`), `timeout` (default 30 s; a hang is `product`),
  `copyCli: <path in tree>` (run a copy of `vam` placed in the tree) and
  `files: { <path>: { contains, absent, count, unchanged } }` (assertions on files in the tree;
  `unchanged` compares with the file before the step). Phase B adds `home: tree`, `env` and the
  `release-server` piece in B.W0.
- Fixtures under `e2e/world/fixtures/version/` and `e2e/world/fixtures/node-init/` only; the stub
  builder is shell and needs no Docker.
- Red proofs: `scripts/e2e/run.sh --scenarios <the five>` in the session; each fails at the step
  named above, class `product`.

What replaces scenarios for the workflow work:

| Proof | Shows | Runs |
|---|---|---|
| Guard tests (`scripts/release/test/guards.test.sh`) | each bad input refused by its own case with its reason: dispatched from a non-`main` ref, commit not on `main`'s first-parent history, CI run missing, red or cancelled (fixture API answers), `Cargo.toml` version not the input, tag exists, release (draft included) exists, version not greater than every tag, an advisory reported (fixture `cargo deny` output), malformed version, malformed commit; a good input passes | `local.sh --fast`, `rust.yml` `checks` |
| Workflow lint (`scripts/release/check-workflow.sh`) and `actionlint` | `release.yml` has `workflow_dispatch` only; `release-build.yml` has only `workflow_call`, `pull_request` and `schedule`; every external `uses:` pinned by a 40-hex SHA (local `./` workflows exempt); no `actions/cache` or `rust-cache`; no `${{ inputs.` or `github.event.inputs` inside `run:`; top-level `permissions: {}`; build and assemble jobs read-only with no `secrets.`; `persist-credentials: false` on every checkout; `publish` is the only job with write permissions, has `environment: release` and `needs` `validate` and the build workflow | same |
| glibc check | the highest `GLIBC_` symbol of every gnu binary is 2.34 or lower | each Linux build job |
| Scans | `gitleaks dir` over the extracted archives; no `vam_sk_`, `/home/runner`, `/Users/runner`; no refused file names | `assemble` |
| Build-only PR runs | `release-build.yml` on its own PRs builds every `v0.1.0` asset and assembles them | Actions, on A.W4 and A.W6 PRs |
| `v0.1.0-rc.1` verified | `gh release verify v0.1.0-rc.1`; `gh release verify-asset` and `gh attestation verify -R squrell-inc/VAM --signer-workflow squrell-inc/VAM/.github/workflows/release.yml` per asset; the tag at the dispatched commit; deleting or moving the tag refused; a second dispatch of `0.1.0-rc.1` fails at guard 5 | owner and orchestrator; recorded |

#### Benchmark delta

P3, install half (machine-base): minutes from a downloaded bundle to a signed-in node with no Rust
and no checkout, base build included; stopwatch on the team checklist (target under 30 minutes).
Launch proof "the install" moves from "from source" to "from a release", and the node and the CLI
name the version they run. P9 (supply chain) gains its first artifacts (`cargo auditable`
binaries, attestations), with no line yet.

#### Items

- [ ] **RA-A.1 Build info, read in the binary crates.**
  - **Scope.** The rules are pure functions in one std-only file,
    `crates/vam-spec/build_info_logic.rs`: channel `release` only when
    `VAM_BUILD_CHANNEL=release`, the version then `CARGO_PKG_VERSION` exactly; otherwise
    `git describe --tags --long --match 'v[0-9]*'` gives `<tag without v>+dev.<N>` (`+dev.0` at
    the tag itself), and no tag gives `<Cargo.toml version>+dev`. Commit: `git rev-parse
    --short=7 HEAD` plus `+dirty`, `unknown` outside a checkout. Date: `git show -s --format=%cs
    HEAD`. Overrides `VAM_GIT_SHA` and `VAM_COMMIT_DATE`; `rerun-if-env-changed` for all four.
    Each binary crate's `build.rs` (`vam-cli` rewritten; `vam-orchestrator`, `vam-web`,
    `vam-schedule`, `vam-notify` new) `include!`s that file, runs git and exports `VAM_VERSION`,
    `VAM_GIT_SHA`, `VAM_COMMIT_DATE`, `VAM_BUILD_CHANNEL`. `vam-spec` gives
    `BuildInfo { version, commit, commit_date, channel }` built from those values as arguments,
    `BuildInfo::line(name)` (`<name> <version> (<commit>[ <date>])`, the date on release builds
    only, open question 5), and a `build_info!()` macro that reads the `env!` values in the
    calling crate. `vam-spec/build.rs` stays proto-only. `main.rs:9` uses the macro;
    `cli_tests.rs:2682` compares with `build_info!().version`.
  - **Files.** `crates/vam-spec/build_info_logic.rs` (new), `crates/vam-spec/src/build_info.rs`
    (new, includes the logic file with `#[path]` for unit tests), `crates/vam-spec/src/lib.rs`,
    `crates/vam-cli/build.rs`, `crates/vam-orchestrator/build.rs` (new),
    `brokers/{vam-web,vam-schedule,vam-notify}/build.rs` (new), `crates/vam-cli/src/main.rs`,
    `crates/vam-cli/tests/cli_tests.rs`.
  - **Acceptance.** Unit tests for each row of design §3.3's table, `+dev.0` at a tag, both
    overrides and `+dirty`. `vam --version` prints `vam 0.1.0+dev (<sha>)` here. The PR records
    what `local.sh --fast` rebuilds after one empty commit (expected: the five binary crates, not
    `vam-spec` or the crates under it). This resolves the old "build info in `vam-spec`" question
    (decision 3).
  - **Tests.** `cargo test -p vam-spec build_info`, `cargo test -p vam-cli --test cli_tests`.
  - **Turns green** (with RA-A.2): `cli-prints-its-version-and-commit`.
  - `Decide: NOW — locked decision 3 (not scored)`

- [ ] **RA-A.2 Every binary prints it.**
  - **Scope.** `vamd --version` (and `-V`) handled first in `main`, before `dotenvy::dotenv()`
    (`vamd.rs:157`): prints `BuildInfo::line("vamd")`, exits 0, reads no env file, opens nothing.
    The three brokers: `version` from the macro in their `#[command]`. `vamd`'s `main` builds its
    `BuildInfo` and hands it to the daemon state; the marketplace `node_version` and MCP
    `clientInfo` read it there. `vam_core::VERSION` and the OCSF product version stay
    `CARGO_PKG_VERSION` (library crates read no git). `crates/vam-firecracker/Cargo.toml` takes
    `version.workspace = true`.
  - **Files.** `crates/vam-orchestrator/src/bin/vamd.rs`, `crates/vam-orchestrator/src/daemon.rs`
    (state), `brokers/*/src/bin/*.rs`, `crates/vam-orchestrator/src/marketplace/mod.rs`,
    `crates/vam-orchestrator/src/mcp_client.rs`, `crates/vam-firecracker/Cargo.toml`.
  - **Acceptance.** `vamd --version` in a directory whose `.env` would stop a start exits 0 and
    creates nothing. A unit test: `NodeRequirement::parse(">=0.1.0")` matches `0.1.0+dev`,
    `0.1.0+dev.14` and `0.1.0-rc.1`. The marketplace scenarios stay green.
  - **Tests.** `cargo test -p vam-spec marketplace`, `cargo test -p vam-orchestrator --lib`
    (marketplace, daemon), the two scenarios.
  - **Turns green:** `cli-prints-its-version-and-commit`, `vamd-prints-its-version-without-starting`.
  - `Decide: NOW — locked decision 3 (not scored)`

- [ ] **RA-A.3 The node reports its build; `vam node status` shows both.**
  - **Scope.** Proto `NodeStatus`: `NodeBuild build = 16;` with `version`, `commit`,
    `commit_date`, `channel`. `get_node` fills it from the daemon's `BuildInfo` (an API test
    passes a fixed one). OpenAPI `NodeStatus` gains `build` and the three fields it already lacks
    (`isolation`, `signingKeys`, `base`). `vam node status` prints first
    `node: vamd <version> (<commit>) <channel>` and `cli: vam <version> (<commit>)`, then
    `warning: the node runs <x.y>, this vam is <a.b>: minor versions differ` when major or minor
    differ (open question 4); a node with no `build` prints `node: version unknown (older vamd)`.
    `--json` prints the body only, as today.
  - **Files.** `proto/vam/v1/orchestrator.proto`, `crates/vam-orchestrator/src/api/mod.rs`
    (`get_node` only), `docs/api/openapi.yaml`, `crates/vam-orchestrator/tests/api_tests.rs`,
    `crates/vam-cli/src/cmd/node.rs`.
  - **Acceptance.** Unit tests of the two lines and the warning from recorded bodies (same version,
    minor differs, patch differs, no `build`); the API test; `examples_json` passes.
  - **Tests.** `cargo test -p vam-cli node`, `cargo test -p vam-orchestrator --test api_tests node`,
    `cargo test -p vam-spec --test examples_json`, the scenario.
  - **Turns green:** `node-status-shows-the-node-and-cli-versions`.
  - `Decide: NOW — locked decision 3 (not scored)`

- [ ] **RA-A.4 `scripts/release/prepare.sh <version>`.**
  - **Scope.** `scripts/release/lib.sh`, shared with the guards (RA-A.6): parse `X.Y.Z[-rc.N]`,
    compare by semver (an rc sorts below its final), list `v*` tags. `prepare.sh` refuses a
    malformed version, a version not greater than every `v*` tag and a dirty tree; sets
    `[workspace.package] version` (`Cargo.toml:33`); refreshes the workspace entries in
    `Cargo.lock` (`cargo update --workspace --offline`) and checks that nothing else in the lock
    changed; prints the PR title `release: prepare v<v>` and the `gh pr create` line. Tests in
    `scripts/release/test/` on throwaway repositories made in a temp dir (no network).
    `scripts/release/test/run.sh` runs every `scripts/release/test/*.test.sh` by glob (so later
    waves add a test file without editing it); `local.sh --fast` and the `rust.yml` `checks` job
    call it.
  - **Files.** `scripts/release/{lib.sh,prepare.sh}`, `scripts/release/test/{run.sh,prepare.test.sh}`.
  - **Acceptance.** A case each: bad version, lower than a tag, equal to a tag, an rc after its
    final (refused), a final after its rc (accepted), a dirty tree, a diff of `Cargo.toml` and the
    workspace lock entries only. A version below the current `Cargo.toml` but above every tag is
    accepted: the first prepare sets `0.1.0-rc.1` over the never-released `0.1.0`.
  - **Tests.** `scripts/release/test/run.sh`.
  - `Decide: NOW — locked decision 2 (not scored)`

- [ ] **RA-A.5 `deny.toml` advisories and bans.**
  - **Scope.** `[advisories]` (RustSec database, `yanked = "deny"`, each `ignore` with id, reason
    and a review date), `[bans]` (deny `openssl` and `openssl-sys`, `wildcards = "deny"`,
    `multiple-versions = "warn"`: 34 duplicates today), `[sources]` (crates.io only,
    `unknown-git = "deny"`). `check-release-gate.sh` runs `cargo deny check` and fails with an
    install hint when `cargo-deny` is missing; `rust.yml` `checks` installs it at a pinned version.
  - **Files.** `deny.toml`, `scripts/e2e/check-release-gate.sh`, `Cargo.lock` (only if an advisory
    has a fixed patch release).
  - **Acceptance.** `cargo deny check` exits 0 on the base; every `ignore` is listed in the PR body
    for the owner.
  - **Tests.** `cargo deny check`, `scripts/e2e/check-release-gate.sh`.
  - `Decide: NOW — locked decision 5 (guard 7) and design §5.4 (not scored)`

- [ ] **RA-A.6 The guard script and its tests.**
  - **Scope.** `scripts/release/guards.sh`: bash, git and jq; inputs only from the environment
    (`VERSION`, `COMMIT`, `DRY_RUN`, `REF`, `REPO`); one function per guard (design §4.2), each
    failing with its reason; the GitHub API through `gh api`, or a fixture directory
    (`GUARDS_API_FIXTURES`) in tests. Guard 3 accepts the `rust.yml` run for event `push`, or a
    `workflow_dispatch` run of `rust.yml` on `main`, at that exact SHA with conclusion `success`,
    and names the run (or the dispatch command) otherwise. Guard 6 uses `lib.sh`. Guard 7 runs
    `cargo deny check advisories` (a fixture output in tests). Every guard runs on a dry run too;
    there is no dry run from another ref (build changes are proven by `release-build.yml` PR
    runs). Tests build a fixture repository with first-parent merges, a side commit, tags and
    fixture API answers.
  - **Files.** `scripts/release/guards.sh`, `scripts/release/test/guards.test.sh`,
    `scripts/release/test/fixtures/`.
  - **Acceptance.** Every case in the proof table above fails with its own reason; a good input
    passes; the suite runs in under 10 seconds.
  - **Tests.** `scripts/release/test/run.sh`.
  - `Decide: NOW — locked decision 5 (not scored)`

- [ ] **RA-A.7 `release.yml` skeleton, the workflow lint, `rust.yml` fixes.**
  - **Scope.** `.github/workflows/release.yml`: `on: workflow_dispatch` only (`version`, `commit`,
    `dry_run` default `true`), top-level `permissions: {}`, `concurrency: { group: release,
    cancel-in-progress: false }`. Job `validate` (`contents: read`, `actions: read`): checkout with
    full history and `persist-credentials: false`; inputs reach shell only through `env:`;
    `guards.sh` with a pinned `cargo-deny` binary checked by sha256 (no compile, so `validate`
    takes a few minutes); a summary table of the guards. No build jobs yet. Every action pinned by
    full SHA with a version comment; `.github/dependabot.yml` for `github-actions`.
    `check-workflow.sh` and a pinned `actionlint` binary (checked by sha256) run from
    `scripts/release/test/run.sh`. `rust.yml`: `cancel-in-progress: ${{ github.event_name ==
    'pull_request' }}` so every merge into `main` keeps a complete push run; `checks` runs
    `scripts/release/test/run.sh` and installs `cargo-deny`; the lane's `changes` regex
    (`rust.yml:84`) and cache key (`rust.yml:310`) add `scripts/release/build-guest.sh` (the file
    A.W3 adds; naming it early keeps A.W3 off `rust.yml`). `local.sh --fast` runs the release
    tests.
  - **Files.** `.github/workflows/release.yml` (new), `.github/dependabot.yml` (new),
    `scripts/release/check-workflow.sh` (new), `.github/workflows/rust.yml`, `scripts/ci/local.sh`.
  - **Acceptance.** The lint passes on `release.yml` and fails on a copy with each forbidden line
    (test fixtures); `actionlint` is clean; once on `main`, a dispatch runs `validate` only.
  - **Tests.** `scripts/release/test/run.sh`; `actionlint .github/workflows/*.yml`.
  - `Decide: NOW — locked decisions 4, 5, 8, 9 (not scored)`

- [ ] **RA-A.8 One way to build the guest init.**
  - **Scope.** `scripts/release/build-guest.sh [--target x86_64|aarch64] [--out PATH]` with the
    flags of `build-rootfs.sh:42-57` exactly (tracker open question 9: no `release-guest`
    profile). `build-rootfs.sh` and `build-rootfs-lima.sh` call it. `guest_build_guards.rs` checks
    `build-guest.sh` for `--bin vam-runtime` and no features, and that every
    `scripts/build-rootfs*.sh` calls `build-guest.sh`. `firecracker-assets.sh`'s stamp hashes it.
    The guards-only scenario edits above.
  - **Files.** `scripts/release/build-guest.sh` (new), `scripts/build-rootfs.sh`,
    `scripts/build-rootfs-lima.sh`, `crates/vam-runtime/tests/guest_build_guards.rs`,
    `scripts/e2e/firecracker-assets.sh`, three `e2e/scenarios/*.yaml` (`guards` only).
  - **Acceptance.** The lane on the PR boots the guest built this way, every lane scenario green.
  - **Tests.** `cargo test -p vam-runtime --test guest_build_guards`; the lane.
  - `Decide: NOW — locked decision 11 ("built exactly as build-rootfs.sh builds it") (not scored)`

- [ ] **RA-A.9 `build-rootfs.sh --runtime PATH`; the recipe stays a Vamfile.**
  - **Scope.** Inject the given binary (checked: a static ELF for the target architecture) without
    cargo or `musl-gcc`; the recipe directory relative to the script, so it runs from
    `share/vam/base/`; staging in `mktemp -d`; `sudo` only when not root.
    `scripts/base/check-recipe.sh`: the `Dockerfile` uses only `FROM`, `RUN`, `COPY`, `ENV`,
    `ARG`, `WORKDIR`, `LABEL` and the syntax directive (`design/agent-experience.md` III.2), run
    through `scripts/release/test/recipe.test.sh` (picked up by the glob; no workflow edit).
    `firecracker-assets.sh` builds the lane image with `build-guest.sh` then `build-rootfs.sh
    --runtime`.
  - **Files.** `scripts/build-rootfs.sh`, `scripts/base/check-recipe.sh` (new),
    `scripts/release/test/recipe.test.sh` (new), `scripts/e2e/firecracker-assets.sh`.
  - **Acceptance.** The lane boots the `--runtime` image; the recipe check fails on a fixture
    Dockerfile with a refused instruction.
  - **Tests.** `cargo test -p vam-runtime --test guest_build_guards`; the recipe test; the lane.
  - `Decide: NOW — locked decision 13 (not scored)`

- [ ] **RA-A.10 The build matrix, the kit, the reusable workflow.**
  - **Scope.** First a short local spike (about 10 minutes): `cargo zigbuild --target
    x86_64-unknown-linux-gnu.2.34` of `vamd` and the notify broker (`aws-lc-sys`, bundled
    SQLite), wrapped by `cargo auditable`, and `objdump -T` of the result (zig from PyPI's pinned
    `ziglang` if ziglang.org is not reachable from the session). If it fails, the Linux jobs build
    in an AlmaLinux 9 container pinned by digest (decision 8); the PR says which.
    `scripts/release/build.sh <target>`: `cargo +1.97.0 auditable build --release --locked`,
    `CARGO_INCREMENTAL=0`, `SOURCE_DATE_EPOCH` = the commit time, `--remap-path-prefix` for the
    checkout and `$CARGO_HOME`, `VAM_BUILD_CHANNEL` from the input, `VAM_GIT_SHA` and
    `VAM_COMMIT_DATE` from the commit; the glibc check on every gnu binary; `cargo audit bin` on
    each binary; `vam --version` from the result. `scripts/release/package.sh`: archive names and
    contents as design §5.1, the node bundle layout per open question 9, with `share/vam/base/`
    (`build-rootfs.sh`, `base/Dockerfile`, `base/pins`, `base/resolve-pins.sh`, `manifest.json`
    with name `vam/base:2`, version and each file's sha256) and `vam-runtime`.
    `.github/workflows/release-build.yml`: `on: workflow_call` (inputs `version`, `commit`,
    `channel`), `pull_request` with `paths` (`scripts/release/**`,
    `.github/workflows/release*.yml`, `scripts/build-rootfs.sh`, `scripts/base/**`, `Cargo.lock`)
    and a weekly `schedule`; PR and weekly runs use the `Cargo.toml` version and `channel: dev`.
    Jobs (`permissions: contents: read`, no secrets, no caches, checkout through `env:`):
    `node-x86_64` (`ubuntu-24.04`: `vamd`, `vam`, three brokers, the guest via `build-guest.sh`,
    the standalone `vam-runtime-<v>-x86_64-unknown-linux-musl`), `cli` (`ubuntu-24.04` x86_64
    gnu, `ubuntu-24.04-arm` aarch64 gnu, `macos-15` arm64, `macos-15-intel` x86_64). Each uploads
    a workflow artifact. `release.yml` calls it beside `validate`. Tools by
    `cargo install --locked --version` (open question 10). The lint learns the new file.
  - **Files.** `scripts/release/{build.sh,package.sh}` (new), `.github/workflows/release-build.yml`
    (new), `.github/workflows/release.yml`, `scripts/release/check-workflow.sh`.
  - **Acceptance.** The PR's own `release-build.yml` run builds every `v0.1.0` asset; the highest
    `GLIBC_` per binary is in the PR body; `vam --version` from each archive prints
    `<v>+dev… (<sha>)` in its job.
  - **Tests.** The spike; `scripts/release/build.sh x86_64-unknown-linux-gnu` locally once (cold,
    about 10 minutes); the PR run.
  - `Decide: NOW — locked decisions 8, 11, 13 (not scored)`

- [ ] **RA-A.11 `vam node init --kit DIR`.**
  - **Scope.** Default: the `share/vam/base/` beside the running `vam` when it holds
    `manifest.json`; precedence per open question 7. Checks the kit's files against
    `manifest.json`; runs the kit's `build-rootfs.sh --runtime <bundle>/vam-runtime` with
    `VAM_ROOTFS_OUT`; allowed under `--root` for a kit only (open question 8); the stamp is the
    sha256 of the kit manifest and the runtime, so the same bundle reports `ok (unchanged)` and a
    newer one rebuilds. `--check` names a kit file that does not match its manifest.
  - **Files.** `crates/vam-cli/src/cmd/{node.rs,node_install.rs,node_init.rs}`.
  - **Acceptance.** Unit tests of the precedence and the stamp; the two scenarios.
  - **Tests.** `cargo test -p vam-cli node_install`; the two bundle scenarios.
  - **Turns green:** `node-init-from-a-release-bundle-needs-no-checkout`,
    `node-init-from-a-newer-bundle-keeps-the-state`.
  - `Decide: NOW — locked decision 13 (not scored)`

- [ ] **RA-A.12 `assemble`.**
  - **Scope.** A job in `release-build.yml` (so PR runs prove it): download the build artifacts;
    `SHA256SUMS`; `scripts/release/scan.sh` (gitleaks over the extracted archives with
    `.gitleaks.toml`, the string scans, the refused names; one list shared with
    `export-public.sh`); `scripts/release/notes.sh` (first-parent `release:` merges since the
    previous tag per open question 11, the "Deploy note" sections of their PR bodies verbatim, the
    asset table, the verify commands); a job summary (asset table with sizes and sha256, notes).
    One `release-<v>` workflow artifact holds everything. `assemble` reads PR bodies with
    `pull-requests: read`, nothing more.
  - **Files.** `scripts/release/{scan.sh,notes.sh}` (new), `scripts/export-public.sh` (the shared
    list), `.github/workflows/release-build.yml`, `scripts/release/test/` (notes and scan cases on
    fixtures).
  - **Acceptance.** The PR run is green and its summary shows the table and the notes; a planted
    `vam_sk_` string and a planted `.env` in a test archive fail the scan.
  - **Tests.** `scripts/release/test/run.sh`; the PR run.
  - `Decide: NOW — locked decisions 10, 12 (not scored)`

- [ ] **RA-A.13 The `publish` job.**
  - **Scope.** First, confirm on GitHub's documentation (REST "create a release", `gh release
    create`) that a draft release does not create its tag until it is published; record the
    source in the PR. Job in `release.yml`: `environment: release`; `permissions: contents:
    write, id-token: write, attestations: write`; `needs: [validate, build]`; runs only when
    `dry_run` is false. Downloads the `release-<v>` artifact; checks guard 5 again (tag and
    release absent); `gh release create v<v> --target <commit> --draft --notes-file
    RELEASE_NOTES.md` (`--prerelease` for `-rc`, `--latest` for a final); uploads every asset;
    attests build provenance for every asset and `SHA256SUMS` (`actions/attest-build-provenance`,
    pinned by SHA); downloads the draft's assets, compares them with `SHA256SUMS` and runs
    `gh attestation verify` on each; checks right before publishing that no `v<v>` tag exists;
    publishes; then `gh release verify` and `gh release verify-asset` per asset. A failure before
    publish deletes the draft and says the same number may run again; a failure after publish
    says "do not reuse this number; prepare the next" (decision 6). The run's summary (commit,
    guards with results, asset table, notes) is what the owner reads at the approval. Workflow
    lint extended for this job.
  - **Files.** `.github/workflows/release.yml`, `scripts/release/{publish.sh,verify.sh}` (new),
    `scripts/release/check-workflow.sh`, `scripts/release/test/`.
  - **Acceptance.** The lint cases; `publish.sh` and `verify.sh` against fixture `gh` answers
    (draft deleted on a failure before publish). The live proof is RA-A.15.
  - **Tests.** `scripts/release/test/run.sh`.
  - `Decide: NOW — locked decisions 6, 7, 9, 10 (not scored)`

- [ ] **RA-A.14 Docs.** New `getting-started/releases.md` (sidebar in `docs/astro.config.mjs`;
  which archive for which machine, verify, install a node from the bundle, the rc's own URLs per
  open question 13); `installation.md` leads with the release (the source build second);
  `windows-wsl.md` (the Linux `vam` and node bundle inside WSL2); `cli.md` (`vam --version`, the
  two `node status` lines, the warning); `orchestrator/api.md` (`status.build`);
  `orchestrator/overview.md` (`vamd --version`); `project-status.md` links the releases page;
  `macos-lima.md` gets the macOS CLI from the release, the node still from source. `proves` names
  the Phase A scenarios. `Decide: NOW — proof 2 (honest docs at the phase close), cost 0`

- [ ] **RA-A.15 `v0.1.0-rc.1`.** Release PR `release: release-artifacts A` merged; prepare PR
  `0.1.0-rc.1` merged into `main`; its push run green (lane included); one publish dispatch; the
  owner reads the summary and approves; every check of the proof table; negatives; design §9
  corrected where the run proved it wrong (a `design/` edit in the close PR). Orchestrator and
  owner. `Decide: NOW — the phase exit`

#### Waves

| Wave | Items | Turns green | Repo | Model | Budget | Depends on | Status | PR |
|---|---|---|---|---|---|---|---|---|
| 0 | scenarios (+ the runner `cli` fields Phase A uses) | — (pending, red for the right reason) | vam | Sonnet low | 75 min | — (lane A1) | [ ] | |
| 1 | RA-A.1, RA-A.2, RA-A.3 | `cli-prints-its-version-and-commit`, `vamd-prints-its-version-without-starting`, `node-status-shows-the-node-and-cli-versions` | vam | Sonnet medium: crosses `vam-spec`, `vam-cli`, `vam-orchestrator` and three brokers, and adds a proto field and an API shape | 180 min | 0 (lane A1) | [ ] | |
| 2 | RA-A.4, RA-A.5, RA-A.6, RA-A.7 | — (release script tests, guard tests, workflow lint, `cargo deny check`) | vam | Sonnet high: the guards decide what can be published, and the workflow's permissions are the release boundary | 180 min | — (lane A2) | [ ] | |
| 3 | RA-A.8, RA-A.9 | — (the lane boots the guest built by `build-guest.sh` and injected with `--runtime`) | vam | Sonnet low | 90 min | — (lane A3) | [ ] | |
| 4 | RA-A.10 | — (the PR's `release-build.yml` run builds every `v0.1.0` asset; glibc ≤ 2.34) | vam | Sonnet medium: four runner OSes, zigbuild with C dependencies and path remapping the plan cannot fully specify | 180 min | 2, 3 (lane A2) | [ ] | |
| 5 | RA-A.11 | `node-init-from-a-release-bundle-needs-no-checkout`, `node-init-from-a-newer-bundle-keeps-the-state` | vam | Sonnet medium: precedence and stamp rules the plan cannot fully specify | 120 min | 0, 1 (`node.rs`), 3 (`--runtime`) (lane A1) | [ ] | |
| 6 | RA-A.12, RA-A.13 | — (PR run: `SHA256SUMS`, notes, scans; publish lint and fixture tests) | vam | Sonnet high: write permissions, tag creation and attestation | 180 min | 4 (lane A2) | [ ] | |
| 7 | RA-A.14 | — | vam | Sonnet low | 60 min | 1, 5; beside 6 (lane A1) | [ ] | |
| close | RA-A.15 | — (`v0.1.0-rc.1` published and verified) | vam | orchestrator and owner | 150 min | A release merged; prepare PR; the owner's §9 settings | [ ] | |

Lane (the microVM lane runs on the PR): W0 yes (`crates/vam-e2e`, `e2e/`), W1 yes
(`vam-orchestrator`, `proto/`), W2 yes (`rust.yml`, `scripts/e2e/`; `skip-vm-lane` is fine),
W3 yes (`build-rootfs*.sh`, `vam-runtime`, `scripts/e2e/`); W4, W5, W6, W7 no. If W6's plan turn
finds it does not fit one context, it splits off `assemble` (RA-A.12) as W6a, medium, before the
publish job.

#### Open questions (new; dropped from the 2026-11-17 breakdowns: dry runs from another ref,
which CI run guard 3 accepts, `rust.yml` cancelling on `main`, the proto file list, the scratch
repository, release wall time, the Windows ones; all decided by the 2026-11-18 restructure)

1. **A dev build exactly at a tag.** Default: `0.1.0-rc.1+dev.0`, never the bare version.
2. **Shallow checkouts.** `actions/checkout` fetches one commit and no tags. Default: fall back to
   `Cargo.toml` + `+dev` (no `.N`); still never a release. `release.yml` fetches full history.
3. **Where `build` sits.** Default: `status.build` in `NodeStatus` (field 16), since `GET /v1/node`
   answers `{ status }`.
4. **Mismatch warning.** Default: warn when major or minor differ (pre-1.0 a minor may break);
   silent on patch, rc and `+dev` differences.
5. **The commit date.** Default: shown on release builds only, as in design §3.3's table; dev
   builds print `(<sha>)`.
6. **Advisories found by `cargo deny`.** Default: `cargo update -p` where a fixed patch release
   exists; otherwise an `ignore` with reason and review date, listed for the owner.
7. **Where the base comes from.** Default: `--base` > `--kit` > the kit beside `vam` >
   `--checkout` or a checkout found from the working directory. A bundle install never looks for a
   checkout.
8. **Building under `--root`.** Default: only from a kit; the kit's builder writes only
   `VAM_ROOTFS_OUT`.
9. **Node bundle layout.** Default: one top directory `vam-node-<v>-<target>/` with the binaries at
   its top (`sudo ./vam node init` works from it; `find_vamd` looks beside `vam`,
   `node_install.rs:160-169`), `share/vam/base/`, `LICENSE-*`, `NOTICE`, `README`, `assets.pins`.
10. **Tool installs.** Default: `cargo-auditable`, `cargo-zigbuild`, `cargo-audit` by
    `cargo install --locked --version`, zig by a pinned release checked by sha256, `cargo-deny`
    and `actionlint` as pinned binaries checked by sha256; no third-party install action.
11. **Notes range.** Default: an rc lists the `release:` merges since the previous tag of any kind;
    a final lists them since the previous final (for `v0.1.0`: all of them, grouped by rc).
12. **`actionlint` in sessions.** Default: required in CI (pinned binary); in `local.sh --fast` it
    runs when present and prints a skip line otherwise.
13. **No "latest" during the rcs.** Default: accept; rcs are pre-releases (decision 1), and the docs
    give tag-specific URLs until `v0.1.0`.
14. **Who dispatches.** Default: the owner dispatches every publish; the orchestrator may start an
    optional dry run when its token has `actions: write`.

#### Phase close

Scenarios active and green on Actions; unit suites on the merged base; the lane green with the
`--runtime` image. Release PR `release: release-artifacts A` (deploy note: the owner's §9 settings
must be in place before the first publish; a node installed from source keeps working), then
RA-A.15.

**Team checklist — Phase A** (owner)

Setup: `v0.1.0-rc.1` published; a fresh Ubuntu 24.04 x86_64 host with KVM and Docker, no Rust, no
checkout; a Mac; a Windows 11 PC with WSL2.

1. `gh release verify v0.1.0-rc.1`. **See:** verified.
2. On the host: download the node bundle and `SHA256SUMS`; `sha256sum -c SHA256SUMS
   --ignore-missing`; `gh attestation verify <bundle> -R squrell-inc/VAM`. **See:** OK; verified.
3. Extract; `./vam --version`, `./vamd --version`, `./vam-web-broker --version`,
   `./vam-schedule-broker --version`, `./vam-notify-broker --version`. **See:** each prints
   `0.1.0-rc.1 (<sha> <date>)`, no `+dev`, the same line after each name. In an empty directory,
   `vamd --version` creates no file.
4. `sudo ./vam node init --check`. **See:** one line per check; the kit found.
5. `sudo ./vam node init`, with a stopwatch. **See:** the base built from the kit (no checkout, no
   cargo), the admin key once, `vamd` active. Write down the minutes.
6. On the Mac: extract the macOS `vam` with `tar` in a terminal; `vam config set-host`, `set-ca`,
   `set-key`; `vam node status`. **See:** `node: vamd 0.1.0-rc.1 (…) release` and
   `cli: vam 0.1.0-rc.1 (…)`; no warning.
7. A chat. **See:** it boots and answers.
8. In WSL Ubuntu on the Windows PC: steps 2-5 with the same Linux assets, then the Linux `vam`
   inside WSL against it. **See:** the same.
9. `git push --delete origin v0.1.0-rc.1`. **See:** refused. Dispatch `0.1.0-rc.1` again. **See:**
   `validate` fails at guard 5.
10. In a throwaway clone: `scripts/release/prepare.sh banana`. **See:** refused, the format named.
    `scripts/release/prepare.sh 0.2.0`. **See:** `git diff --stat` lists `Cargo.toml` and
    `Cargo.lock` only. Delete the clone.

Report: "Phase A pass", or the step number with the error text.

### Phase B — rc.2: `vam dock`, `vam upgrade`, install script, brokers

**Goal:** one command installs `vam` on the user's `PATH` on Linux and macOS, one command upgrades
it to a newer release after verifying it, and a node installs and registers the brokers it is
asked for.  
**Depends on:** none to build (dock and upgrade are proven against a fixture release server); the
B base is cut from the A base after A.W0; the close needs Phase A released (rc.1)  
**Plan:** drafted 2026-11-18 (restructure of old Phase 5 without Windows, plus the slimmed broker
items of old Phase 3), waiting for the owner. Runs in its own cloud session from day one; the
orchestrator re-validates it against the code at its start  
**Exit:** scenarios green; `install.sh` is a release asset and works on Linux and macOS from a
clean user account (`install-smoke` green); `vam upgrade` reaches the newer release from a
fixture release server and refuses a tampered download; `vam node init --broker web,schedule`
installs the units and registers both brokers through the API; `v0.1.0-rc.2` published and
verified. The real upgrade between two published releases is recorded in Final (no earlier
release holds `vam upgrade`).

- [ ] `vam dock` / `vam undock` (`--dir`, `--no-modify-path`) on Linux and macOS (design §5.7)
- [ ] `vam upgrade` (`--check`, `--version`, `--pre`): GitHub API, hash and attestation check,
      self-replace, refuses a downgrade without `--version` and inside a node bundle
- [ ] `install.sh` asset (pick archive, check `SHA256SUMS`, run `vam dock`)
- [ ] `vam node init --broker web,schedule,notify`: units, registration through the API,
      `--check` names a missing broker binary
- [ ] Docs: `getting-started/releases.md`, `getting-started/cli.md`, `hosted/overview.md`
- [ ] `v0.1.0-rc.2` published and verified

<!-- Breakdown 2026-11-18 (phase-breakdown agent, Opus 5.5 high), restructured from the
2026-11-17 breakdowns of old Phases 3 and 5. Draft: re-validate at phase start. Also answers
tracker open question 13 (below, open question 1). -->

#### Code map (2026-11-17, rechecked 2026-11-18)

- No `dock`, `undock` or `upgrade`; the `Commands` enum is `main.rs:33-145`. `vam-cli` already has
  `reqwest` (rustls), `sha2`, `tar`, `flate2` (`crates/vam-cli/Cargo.toml:19-36`): downloading,
  hashing and extracting a `.tar.gz` need no new crate (no Windows `.zip` in this phase).
- `releases/latest` ignores pre-releases: until `v0.1.0`, `…/releases/latest/download/install.sh`
  answers 404 and "the latest release" has no rc.
- The e2e `cli` step runs one shared `vam` (`VAM_E2E_CLI`): `vam upgrade` and `vam dock` must act on
  a copy inside the scratch tree (`copyCli`, A.W0) with `HOME` inside the tree (`home: tree`,
  B.W0).
- Brokers. `vamd` dials a broker per call (`host_tools.rs:3347`). Registration is an operator
  approval stored by `PUT /v1/hosted/brokers` with a `brokers:admin` key (`api/mod.rs:1866-1924`).
  `vam node init` holds the admin key only on its first run (it relays it from `vamd --init`,
  `node_install.rs:401-446`); a re-run needs `VAM_API_KEY` or the signed-in config (global
  `--host` / `--token`, `main.rs:18-25`). `vamd` opens each broker's gateway socket at start
  (`gateway.rs:172,273`); the schedule broker needs `--gateway-socket` (the two-step start of
  `run-schedule-broker.sh:126-147`). `vam-web-broker` has no `manifest` subcommand (its manifest is
  `scripts/web-broker/manifest.json`); schedule and notify print theirs
  (`vam-schedule-broker.rs:43-46`, `vam-notify-broker.rs:46-49`).
- After A.W6, `release-build.yml` holds `assemble`; `install.sh` is one more asset there.

#### Scenario delta

| Scenario | Change | Layer | Given | Why |
|---|---|---|---|---|
| `dock-puts-vam-on-the-path-once` | new | process | none; fixture `dock/home` (a fake home with `.profile` and `.zshrc`) | with `home: tree` and a `PATH` without the directory: `vam dock --dir {{root}}/home/.local/bin` copies `vam` there and adds one marked line to `.profile` and `.zshrc`; a second run prints `unchanged` and each file holds the line once (`files` `count: 1`); `vam dock --no-modify-path` prints the line and changes no file; `vam undock` removes the binary and both lines. Red: `dock` is not a command |
| `upgrade-refuses-a-download-whose-hash-does-not-match` | new | process | `release-server` (new piece) with overlay `release-server-bad-sum` (one wrong `SHA256SUMS` line) | the tree's copy of `vam` runs `vam upgrade --version v9.9.9`: exit non-zero, the output names the hash mismatch, the copy is byte-for-byte unchanged. Red: `upgrade` is not a command |
| `upgrade-replaces-vam-with-the-newer-release` | new | process | `release-server` | `vam --version` and `vam node status` make no request to the server (request count 0: the CLI never calls the network unasked); `vam upgrade --check` names `v9.9.9` and changes nothing; `vam upgrade --pre` replaces the copy (the fixture archive holds a `vam` that prints `vam 9.9.9 (fixture)`), the output says the attestation was not checked because `gh` is absent; the replaced file then prints `vam 9.9.9 (fixture)` (the downgrade refusal and "up to date" are unit tests). Red: `upgrade` is not a command |
| `node-init-installs-and-registers-the-brokers-asked-for` | new | process | org-admin (the running node the CLI registers with); fixture `node-init/ready` with the brokers `run.sh` builds beside the copied `vam` | `vam node init --root {{root}} --no-systemd --broker web,schedule --vamd {{vamd}}` with `as: org-admin` (the key a re-run is given): writes `vam-broker@web` and `vam-broker@schedule` units and their argument files (no secret in them), registers both through the API (`done broker vam.web: registered`), and `GET /v1/hosted/brokers` lists `vam.web` and `vam.schedule`; notify is left alone; a second run reports both `ok (unchanged)`; a run without a key writes nothing new and prints the command that registers each broker. Red: `--broker` unknown (usage error) |
| `node-init-check-names-what-is-missing` | changed: `args` add `--broker web`; `contains` adds `missing broker vam-web-broker` (the bare tree has none) | process | unchanged | the check names a requested broker binary; a spec change approved with this breakdown |

Notes for wave 0: runner additions `home: tree` (`HOME` inside the tree) and `env` (extra
variables) on the `cli` step, and the `release-server` piece (`crates/vam-e2e/src/release_server.rs`):
the runner serves, on loopback, a GitHub-like `releases` API (latest, by tag, the list with
pre-releases) and the assets of a fixture directory, and counts requests. The built `vam` reaches
it only through the test seam of open question 3. Each with a `vam-e2e` unit test and a line in
`e2e/README.md`. Red proofs with `scripts/e2e/run.sh --scenarios …`.

#### Benchmark delta

P3, install half, for the CLI: one command and the minutes from nothing to `vam` on the `PATH` on
Linux and macOS (team checklist).

#### Items

- [ ] **RA-B.1 `vam dock` and `vam undock`.** Per design §5.7, Linux and macOS only: `~/.local/bin`
  by default; marked lines (`# added by vam dock`) in the listed login profiles (never system
  files); `--dir`, `--no-modify-path`; idempotent; `undock` removes only what `dock` wrote. On
  another OS, `dock` says it is not supported yet. Also adds the `upgrade` subcommand as a stub
  ("not built yet") so B.W2 does not touch `main.rs`. Files:
  `crates/vam-cli/src/{main.rs,cmd/mod.rs,cmd/dock.rs}`. Tests: unit tests on temp homes; the
  scenario. Turns green: `dock-puts-vam-on-the-path-once`. `Decide: NOW — locked decision 15 (not
  scored)`
- [ ] **RA-B.2 `vam upgrade`.** `--check` (reads only), `--version vX` (pins, may downgrade),
  `--pre` (allows an rc). Reads the GitHub releases API (with `GH_TOKEN` or `GITHUB_TOKEN` when set,
  never stored); picks the `.tar.gz` for the target `vam` was built for; downloads it and
  `SHA256SUMS`; checks the hash; then `gh attestation verify <archive> -R squrell-inc/VAM
  --signer-workflow squrell-inc/VAM/.github/workflows/release.yml` when `gh` is on the `PATH`, else
  one line saying it was not checked and the command to check it (open question 1); extracts to a
  sibling temp file; renames it over the running file. Refuses a downgrade without `--version`;
  refuses inside a node bundle (`vamd` beside `vam`, open question 6); never touches `vamd`. Test
  seam per open question 3. Files: `crates/vam-cli/src/cmd/upgrade.rs` (new),
  `crates/vam-cli/Cargo.toml` (the seam feature), `scripts/e2e/run.sh` (builds the e2e `vam` with
  it), `scripts/e2e/check-release-gate.sh` (the seam is absent from the default tree). Tests:
  `cargo test -p vam-cli upgrade`; the two scenarios. Turns green:
  `upgrade-refuses-a-download-whose-hash-does-not-match`, `upgrade-replaces-vam-with-the-newer-release`.
  `Decide: NOW — locked decision 15 (not scored)`
- [ ] **RA-B.3 `install.sh`.** A release asset made in `assemble` and listed in `SHA256SUMS`: pick
  the archive for the OS and CPU (Linux x86_64/aarch64, macOS arm64/x86_64); `--version vX`
  (required while no final exists, open question 2); download the archive and `SHA256SUMS`;
  check; extract to a temp directory; run `vam dock`; print each step first. `release-build.yml`:
  an `install-smoke` job on `ubuntu-24.04` and `macos-15` that runs the script against the run's
  own artifacts (a `--from DIR` mode), then `vam --version` and `vam undock`. Files:
  `scripts/release/install.sh` (new), `scripts/release/package.sh`,
  `.github/workflows/release-build.yml`, `scripts/release/check-workflow.sh`. Tests: `shellcheck`;
  the PR run. `Decide: NOW — locked decision 15 (not scored)`
- [ ] **RA-B.4 `vam node init --broker web,schedule,notify`.** `vam-web-broker manifest` prints its
  manifest (the crate becomes the one source; `run-web-broker.sh` reads it). Per open question 7:
  one template unit, an argument file per broker (no secrets), sockets 0600 under the state
  directory, started after `vamd`; registration through `PUT /v1/hosted/brokers` with the
  broker's manifest, using the admin key the first run holds, else `VAM_API_KEY` or the signed-in
  config; without a key, the units are written and the exact registration command is printed per
  broker (exit 0). `--check --broker` names a missing broker binary beside `vam`; a re-run is
  idempotent and never removes a broker it was not asked about. Files:
  `crates/vam-cli/src/cmd/{node.rs,node_install.rs,node_init.rs}`,
  `brokers/vam-web/src/bin/vam-web-broker.rs`, `scripts/run-web-broker.sh`. Tests:
  `cargo test -p vam-cli node`; the scenarios. Turns green:
  `node-init-installs-and-registers-the-brokers-asked-for`, `node-init-check-names-what-is-missing`
  (changed). `Decide: NOW — locked decision 14 (not scored)`
- [ ] **RA-B.5 Docs.** `getting-started/releases.md` (the `install.sh` one-liner, with the
  tag-specific URL until `v0.1.0`), `getting-started/cli.md` (`dock`, `undock`, `upgrade`); macOS
  quarantine for a browser download (open question 5); `hosted/overview.md` (installing brokers
  with `--broker`, registering on a re-run). `Decide: NOW — the tracker's item`
- [ ] **RA-B.6 `v0.1.0-rc.2`.** After rc.1, the B base takes `main`; release PR
  `release: release-artifacts B` merged; prepare PR `0.1.0-rc.2`; one publish dispatch; approval;
  verification as in RA-A.15. Orchestrator and owner. `Decide: NOW — the phase exit`

#### Waves

| Wave | Items | Turns green | Repo | Model | Budget | Depends on | Status | PR |
|---|---|---|---|---|---|---|---|---|
| 0 | scenarios (+ `home: tree`, `env`, the `release-server` piece) | — (pending, red for the right reason) | vam | Sonnet low | 90 min | A.W0 (the B base is cut after it) (lane B1) | [ ] | |
| 1 | RA-B.1 | `dock-puts-vam-on-the-path-once` | vam | Sonnet medium: a new top-level command that edits the user's login files on two OSes, idempotently | 120 min | 0; A.W1 (`main.rs`) (lane B1) | [ ] | |
| 2 | RA-B.2 | `upgrade-refuses-a-download-whose-hash-does-not-match`, `upgrade-replaces-vam-with-the-newer-release` | vam | Sonnet high: it replaces the running binary, and the checks must come before the replace | 180 min | 1; A.W2 (`check-release-gate.sh`) (lane B1) | [ ] | |
| 3 | RA-B.3 | — (`install-smoke` green on Linux and macOS in the PR's `release-build.yml` run) | vam | Sonnet medium: a script that downloads and runs a binary on users' machines | 120 min | 1; A.W6 (workflow order) (lane B2) | [ ] | |
| 4 | RA-B.4 | `node-init-installs-and-registers-the-brokers-asked-for`, `node-init-check-names-what-is-missing` | vam | Sonnet medium: crosses `vam-cli` and a broker, systemd units and API registration | 150 min | 0; A.W5 (node files) (lane B2) | [ ] | |
| 5 | RA-B.5 | — | vam | Sonnet low | 60 min | 2, 3, 4; A.W7 (`releases.md`) | [ ] | |
| close | RA-B.6 | — (`v0.1.0-rc.2` published and verified) | vam | orchestrator and owner | 150 min | rc.1 published; B base took `main` | [ ] | |

B.W2 and B.W4 may run side by side (different files). Lane: W0 yes (`crates/vam-e2e`, `e2e/`), W2
yes (`scripts/e2e/`); W1, W3, W4, W5 no.

#### Open questions (new; dropped: Windows dependencies, `aws-lc` on Windows, the broker-version
field, offline broker registration; all moved or decided by the 2026-11-18 restructure)

1. **Tracker open question 13, answered here.** Default: no in-process Sigstore verifier (it adds a
   trust-root fetch and a large dependency tree to a small CLI); the hash is always checked;
   `gh attestation verify` runs when `gh` is present; otherwise one line says the attestation was
   not checked and how to check it. Backlog trigger: "a user without `gh` asks, or the `cosign`
   bundle for `SHA256SUMS` lands".
2. **Before the first final.** Default: `install.sh` without `--version` stops with "no final
   release yet; pass --version v0.1.0-rc.N"; `vam upgrade` without `--pre` says the same.
3. **Test seam.** Default: a `vam-cli` cargo feature `upgrade-seam` that honours
   `VAM_UPGRADE_API_URL` for loopback URLs only; `run.sh` builds the e2e `vam` with it; the release
   gate's default-tree check refuses it, as it refuses `process-guest`.
4. **`PATH` edits.** Default: tracker open question 14 (marked lines in the login profiles only;
   `--no-modify-path` prints the line).
5. **Unsigned binaries.** Default: the docs explain macOS quarantine (a browser download; `curl`
   sets none); code signing is later.
6. **`vam upgrade` inside a node bundle.** Default: refused when `vamd` sits beside `vam` ("upgrade
   the node from its bundle"), so a node's CLI never drifts from its bundle unseen.
7. **Broker units.** Default: `vam-broker@.service`, an argument file `/etc/vam/broker-<name>.env`
   (no secrets), `User=root` like `vamd`, `After=` and `Requires=vamd.service`; socket
   `/var/lib/vam/brokers/<name>.sock` (0600); state `/var/lib/vam/brokers/<name>/`.
8. **Removing a broker.** Default: not in this phase.
9. **Registering without a key.** Default: write the units, print one `vam` (or `curl`) command
   per broker that registers it with an admin key, exit 0; the first run never needs it.

#### Phase close

Scenarios active and green; `install-smoke` green. Release PR `release: release-artifacts B`
after rc.1, then RA-B.6. Deploy note: `--broker` is opt-in; a node installed earlier keeps working.

**Team checklist — Phase B**

Setup: rc.2; a Linux and a macOS user account without `vam`; the node from the Phase A checklist.

1. Linux: `curl -fsSL https://github.com/squrell-inc/VAM/releases/download/v0.1.0-rc.2/install.sh
   | sh -s -- --version v0.1.0-rc.2`. **See:** each step printed first, the hash checked,
   `vam dock` done; a new terminal runs `vam --version`. Write down the minutes.
2. Run it again. **See:** `unchanged`.
3. macOS (zsh): the same. **See:** `~/.zprofile` holds one marked line.
4. `vam upgrade --check --pre`. **See:** up to date; nothing changed.
5. On the node: extract the rc.2 node bundle in another directory; `sudo ./vam node init --broker
   web,schedule`. **See:** no new key; the units written; the registration command printed (a
   re-run holds no key). Then `sudo VAM_API_KEY=<admin key> ./vam node init --broker
   web,schedule`. **See:** `vam-broker@web` and `vam-broker@schedule` active, both registered;
   earlier threads still listed; `vam node status` says `0.1.0-rc.2`.
6. `vam secrets set web/brave-api-key …`, then a chat that searches the web. **See:** an answer
   with sources.
7. Inside the bundle directory: `./vam upgrade`. **See:** refused, "upgrade the node from its
   bundle".
8. `vam undock` on Linux and macOS. **See:** the binary and the marked lines are gone.

Report: "Phase B pass", or the step number with the error text.

### Final — `v0.1.0`

**Goal:** the first final release, once the release candidates have settled.  
**Depends on:** Phases A and B closed (rc.1 and rc.2); the owner's word that the rcs have settled  
**Plan:** drafted 2026-11-18, waiting for the owner; re-validated at its start  
**Exit:** prepare PR `0.1.0` merged; `v0.1.0` published (not a pre-release) and verified with
`gh release verify` and `gh attestation verify`; `vam upgrade` from rc.2 (or the last fix rc)
reaches `v0.1.0` on Linux and macOS, recorded: the first upgrade between two published releases
that hold `vam upgrade`; the docs' releases page names `v0.1.0`.

- [ ] Owner's rc check: a server, WSL2 (the Linux assets), a laptop CLI on Linux and macOS, all on
      the last rc
- [ ] Prepare PR, one dispatch, approve, verify; release notes cover every rc
- [ ] `vam upgrade` from the last rc to `v0.1.0`, recorded
- [ ] Docs: releases page and `project-status.md`

#### Scenario delta

None new. Every scenario of Phases A and B is active, and the `rust.yml` push run on the prepare
commit (guard 3) is green with all of them and the lane. What replaces scenarios: design §9
"Every release" run end to end, and the recorded `vam upgrade` from the last rc.

#### Items

- [ ] **RA-F.1 The owner's rc check** on the last rc: a server, WSL2 with the Linux assets, and a
  laptop CLI on Linux and macOS (the Phase A and B checklists, shortened).
  `Decide: NOW — the tracker's item`
- [ ] **RA-F.2 `v0.1.0`.** Prepare PR `0.1.0` merged; one publish dispatch (`--latest`, not a
  pre-release); owner approves; `gh release verify v0.1.0`, `gh attestation verify` on every
  asset; `curl -fsSL https://github.com/squrell-inc/VAM/releases/latest/download/install.sh | sh`
  now works without `--version`; `vam upgrade` (no `--pre`) from rc.2 (or the last fix rc)
  reaches `v0.1.0` on Linux and macOS, recorded in the progress log. The notes cover every
  `release:` merge, grouped by rc (Phase A open question 11). `Decide: NOW — the phase exit`
- [ ] **RA-F.3 Docs.** `getting-started/releases.md` names `v0.1.0` and drops the rc-only URL
  form; `project-status.md` links the release. A docs-only PR into `main` after the publish.
  `Decide: NOW — the tracker's item`

#### Waves

| Wave | Items | Turns green | Repo | Model | Budget | Depends on | Status | PR |
|---|---|---|---|---|---|---|---|---|
| 1 | RA-F.3 | — | vam | Sonnet low | 45 min | `v0.1.0` published | [ ] | |
| close | RA-F.1, RA-F.2 | — (`v0.1.0` published and verified; upgrade recorded) | vam | owner and orchestrator | 150 min | Phases A and B closed; the owner's word | [ ] | |

Lane: no wave touches a VM path (the prepare PR's push run runs the lane anyway).

#### Open questions (new)

1. **When the rcs have "settled".** Default: the last rc passes RA-F.1 and no fix rc was needed for
   three working days of the owner's use; the owner can say so earlier.
2. **A fix found in the rc check.** Default: a fix PR into `main`, then the next rc (never straight
   to the final), then RA-F.1 again on what changed; the Final upgrade is then recorded from that
   rc.

#### Phase close

The owner decides whether `v0.2.0` (below) runs in this workstream or a new one. If a new
one, mark this workstream done and delete the tracker and its README row (playbook §7).

**Team checklist — Final:** design §9 "Every release", steps 1-7, for `v0.1.0`; then the platform
list of RA-F.1 on the final; then `vam upgrade` from the last rc on Linux and macOS. Report:
"Final pass", or the step number with the error text.

### Next release: v0.2.0 (Lima and Windows)

Owner, 2026-11-18: a separate later release brings Lima and Windows. **Not broken down**; the
items and facts below are kept from the 2026-11-17 breakdowns of old Phases 3 and 4 so they are
not lost. It is `v0.2.0`: a minor, because it adds features (decision 2 unchanged: a patch only
fixes). It gets its own rcs (`v0.2.0-rc.N`) and its own breakdown when the owner starts it.

Until then, for `v0.1.0`: Windows users run the Linux `vam` and the x86_64 node bundle inside
WSL2 (that path works today with the Linux assets); macOS gets the CLI (arm64 and x86_64) pointed
at a remote Linux node; Lima stays build-from-source as today.

**Exit:** `vam-node-<v>-aarch64-unknown-linux-gnu.tar.gz` published; inside a Lima VM on an M3 or
newer Mac, `sudo ./vam node init` from it builds the standard base, fetches the pinned aarch64
Firecracker and kernel and starts `vamd`, recorded; `getting-started/macos-lima.md` leads with
it; `vam.exe` and `install.ps1` are release assets; `vam dock` and `vam upgrade` work on Windows.

Items (not yet planned):

- [ ] aarch64 pins: the Bun aarch64 hash in `scripts/base/pins` (new `_AARCH64` keys, x86_64 keys
      unchanged), the `Dockerfile` picks the zip by `TARGETARCH`, `resolve-pins.sh` checks the
      Debian index lists `linux/arm64`; aarch64 Firecracker and kernel in `scripts/assets.pins`
- [ ] `check_platform` accepts aarch64 Linux; `vam node init` picks per-architecture pins and names
- [ ] `node-aarch64` job on `ubuntu-24.04-arm` (zigbuild `aarch64-unknown-linux-gnu.2.34`,
      `build-guest.sh --target aarch64`, the same kit); asset `vam-runtime-<v>-aarch64-unknown-linux-musl`
- [ ] `getting-started/macos-lima.md` leads with the release; `build-rootfs-lima.sh` and
      `start-vamd-lima.sh` stay one more release with a deprecation line; a recorded Lima run
- [ ] Windows: `#[cfg(unix)]` on the `node init` / `node install` modules (`node init` says Linux
      only, use WSL); a `cli (windows)` job in `rust.yml`; the `vam.exe` asset on `windows-2025`
- [ ] `install.ps1`; `vam dock` on Windows (the user `Path` in `HKCU\Environment` and a
      settings-changed broadcast, `winreg` and `windows-sys` under `cfg(windows)`); `vam upgrade`
      renames a running `vam.exe` aside and reads the `.zip`

Facts found 2026-11-17:

- `check_platform` accepts x86_64 Linux only (`node_init.rs:228-240`). The CLI embeds
  `scripts/assets.pins` (`node_init.rs:24`), x86_64 values only; the Firecracker URL and entry
  names are built with `x86_64` (`node_install.rs:270,287,290`).
- The Debian pin is already multi-architecture: `DEBIAN_DIGEST` is a manifest-list digest
  (`scripts/base/pins:8-11`). Bun is x64-only: `BUN_SHA256` is the `bun-linux-x64-baseline.zip`
  hash and the `Dockerfile` downloads that zip (`scripts/base/Dockerfile:43-45`).
- The aarch64 kernel: default the same `vmlinux-6.1.155` from the Firecracker CI bucket's
  `aarch64/` prefix. Whether `ubuntu-24.04-arm` runners expose `/dev/kvm` is unknown; default a
  recorded Lima run as the boot proof. `guest_build_guards.rs:39` requires at least two
  `build-rootfs*.sh` scripts: retiring the Lima script changes that test.
- Windows: `cmd/mod.rs:16-17` include `node_init` and `node_install` always; both import
  `std::os::unix::fs` (`node_init.rs:13`, `node_install.rs:12`); `config.rs:69-71` is already
  `cfg(unix)`. If `aws-lc-sys` needs NASM on MSVC, install a pinned NASM in the job; do not switch
  TLS backends.

## Open questions

Each has a default; see design §10 for the reasoning.

1. First release number — decided (changed 2026-11-18): rc per milestone: `v0.1.0-rc.1` after
   Phase A, `v0.1.0-rc.2` after Phase B, `v0.1.0` after the owner says the rcs have settled.
2. Pre-1.0 policy — minor may break; notes carry "Upgrade notes".
3. Restrict `v*` tag creation — no; updates and deletions restricted; no scratch-repository test
   before `v0.1.0` (later).
4. glibc floor — 2.34 via `cargo-zigbuild`; AlmaLinux 9 container if the A.W4 spike fails.
5. macOS Intel CLI — yes while `macos-15-intel` exists.
6. Windows CLI — decided (2026-11-18): native `vam.exe` in `v0.2.0`; for `v0.1.0`, the
   Linux `vam` inside WSL2.
7. CI-green guard — the `rust.yml` push run (or a dispatch run on `main`) on the exact SHA; the
   release re-runs only `cargo deny check advisories`.
8. Changelog file — none.
9. Guest init profile — as `build-rootfs.sh` builds it today.
10. Version bump — a prepare PR at release time; no post-release PR.
11. Who dispatches — owner dispatches and approves every publish; dry runs are optional.
12. Brokers installed by default — none; `--broker` names them.
13. `vam upgrade` attestation check — hash always, `gh attestation verify` when `gh` exists; no
    in-process Sigstore verifier (Phase B open question 1).
14. `PATH` edits — marked lines in the user's login profiles only; `--no-modify-path` opts out.
15. Where build info is read — resolved (2026-11-18): the binary crates' build scripts, through a
    shared helper in `vam-spec`; not a git-reading `vam-spec/build.rs`.

## Explicitly later

- Lima and Windows: `v0.2.0` (section above).
- Published `rootfs.ext4` with a source archive, after a licence review.
- CycloneDX SBOM files and their attestation; a proto descriptor asset, if a consumer appears.
- The broker's version in `RegisterResponse`; `vam node status` listing brokers with versions.
- Restricting `v*` tag creation to the workflow.
- `cosign` bundle for `SHA256SUMS`; SLSA Build L3; rebuild check.
- `vam node upgrade`, `vam node init --from-release`, package managers, release branches.

## Progress log

| Date | Phase | What landed (end with the scoreboard: scenarios green, red, fix cycles, wall time) |
|---|---|---|
| 2026-11-17 | — | Tracker opened with design `design/release-artifacts.md`; plan proposed, waiting for the owner's approval. Scoreboard: no scenarios yet, 0 fix cycles |
| 2026-11-17 | — | Owner review: version chosen at release time by a prepare PR (no post-release bump, dev builds `+dev.N`); Phase 3 gains broker install and `build-rootfs.sh --runtime`; recipe kept inside the Vamfile grammar (decision 13); new Phase 4 (aarch64 node for Lima) and Phase 5 (`vam dock`, `vam upgrade`); walkthrough in design §11. Still waiting for approval |
| 2026-11-17 | 0-6 | **Breakdowns of all phases drafted** (phase-breakdown agent, Opus 5.5 high; the owner asked for the whole plan in one pass with parallel lanes; Phases 3-6 are drafts re-validated at phase start), waiting for the owner. New "Execution plan": one `feat/` base per phase, P1.W1-W2 ride the P0 release so `release.yml` reaches `main` early, lanes A-F with a file-order table, 12 owner gates, rc.1-rc.4 then `v0.1.0`, about 47 h on the critical path (about 88 h serial). Scenario delta: P0 4 new; P1 3 guards-only; P2 none (lint, scratch rehearsal, verified rc.1); P3 4 new, 2 changed; P4 none new (arm64 run of three `node-init-*`, Lima run); P5 3 new; P6 none. Waves: P0 4 (low, medium, medium, low), P1 4 + close (high, low, medium, medium), P2 1 + close (high), P3 8 (low ×3, medium ×3, high, low), P4 3 + close (low, medium, low), P5 5 + close (low, medium, high, medium, low), P6 1 + close (low). Found in the code: `hsp.rs:489` is a test and `Register` carries no broker version back (RA-3.5); the Debian pin is already multi-arch; `rust.yml` cancels `main` push runs; guard 7 makes a run about 40 min; no `latest` release during the rcs; `node init` cannot register brokers without a key on a re-run (RA-3.6). Scoreboard: 0 green, 0 red (not written), 0 fix cycles |
| 2026-11-18 | A, B, Final | **Plan restructured on the owner's decision** after an independent review (phase-breakdown agent, Opus 5.5 high), waiting for the owner's approval. Review findings: rc.1 was not installable (no kit until old Phase 3); guard 7 repeated the release gate the push run already ran (`rust.yml:121-123`); a dry run before each publish duplicated the approval gate; each of seven phases carried its own release overhead. Now: **Phase A** (rc.1: version identity, release scripts and workflow, `build-guest.sh` and `--runtime`, reusable `release-build.yml` with the kit, `vam node init --kit`, publish; 8 waves: low, medium, high, low, medium, medium, high, low), **Phase B** (rc.2: `vam dock`, `vam upgrade`, `install.sh`, `--broker` via the API; 6 waves: low, medium, high, medium, medium, low; own cloud session from day one), **Final** (`v0.1.0`, upgrade rc.2 to `v0.1.0` recorded), and **`v0.2.0`** (Lima and Windows, a minor; not broken down). Decisions changed: 3 (build info read in binary crates only), 5 (guard 7 is now `cargo deny check advisories`; builds beside `validate`), 7 (draft targets the commit, tag at publish; no scratch rehearsal, no routine dry run), 8 (reusable build workflow; AlmaLinux 9 fallback), 10 (SBOM files deferred), 11 (no proto asset; aarch64 and Windows to `v0.2.0`), 14 (brokers registered through the API), 15 (Windows to `v0.2.0`). Scenario delta: A 5 new + 3 guards-only; B 4 new + 1 changed; Final none. Estimate: about 21 h on the critical path (was 47 h), about 45 h serial (was 88 h). Scoreboard: 0 green, 0 red (not written), 0 fix cycles |
