# Workstream: Release artifacts (version, tag, build, publish, install)

```yaml
status: active
slug: release-artifacts
current_phase: 0 (plan approved 2026-11-17; phase breakdowns next)
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

## Phases

### Done phases

Done phase sections are deleted; git history keeps them.

| Phase | Name | Done |
|---|---|---|

### Phase 0 — Version identity

**Goal:** every binary and the node say which version and commit they are, the same way, and a
dev build never claims to be a release or names a future version.  
**Depends on:** none  
**Plan:** not drafted (breakdown after the owner approves this tracker)  
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

Expected scenarios (final names at the breakdown): `cli-prints-its-version-and-commit`,
`vamd-prints-its-version-without-starting`, `node-status-shows-the-node-and-cli-versions`,
`dev-build-does-not-claim-a-release`.

### Phase 1 — Release workflow, dry run

**Goal:** `release.yml` validates inputs and builds, checks and packages every phase-1 asset
without publishing anything.  
**Depends on:** Phase 0  
**Plan:** not drafted  
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

No product scenario: the proof is the guard tests and the recorded dry run (design §7).

### Phase 2 — Publish

**Goal:** the first immutable, attested release, `v0.1.0-rc.1`.  
**Depends on:** Phase 1; the owner's one-time setup (design §9)  
**Plan:** not drafted  
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

### Phase 3 — Install a node from a release

**Goal:** a node is installed from the x86_64 node bundle (Linux server or WSL2) with its brokers,
without Rust or a checkout; `vam.exe` exists.  
**Depends on:** Phase 2  
**Plan:** not drafted  
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

Expected scenarios: `node-init-from-a-release-bundle-needs-no-checkout`,
`node-init-installs-and-registers-the-brokers-asked-for`,
`node-init-from-a-newer-bundle-keeps-the-state`; a change to
`node-init-check-names-what-is-missing` if the check lists the recipe or a broker.

### Phase 4 — aarch64 node (Lima on Apple Silicon)

**Goal:** a Mac (M3 or newer) runs a node in Lima from the release, with the standard base, and no
compile.  
**Depends on:** Phase 3  
**Plan:** not drafted  
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

### Phase 5 — `vam` on the `PATH` and `vam upgrade`

**Goal:** one command installs `vam` on the user's `PATH` on Linux, macOS and Windows, and one
command upgrades it to a newer release after verifying it.  
**Depends on:** Phase 2 (can run beside Phases 3 and 4)  
**Plan:** not drafted  
**Exit:** scenarios green; `install.sh` and `install.ps1` are release assets and work on the
three OSes from a clean user account; `vam upgrade` from the previous release reaches the
newest one and refuses a tampered download.

- [ ] `vam dock` / `vam undock` (`--dir`, `--no-modify-path`), per OS (design §5.7)
- [ ] `install.sh` / `install.ps1` assets (pick archive, check `SHA256SUMS`, run `vam dock`)
- [ ] `vam upgrade` (`--check`, `--version`, `--pre`): GitHub API, hash and attestation check,
      self-replace (rename aside on Windows), refuses a downgrade without `--version`
- [ ] Docs: `getting-started/releases.md`, `getting-started/cli.md`

Expected scenarios: `dock-puts-vam-on-the-path-once`,
`upgrade-refuses-a-download-whose-hash-does-not-match`,
`upgrade-replaces-vam-with-the-newer-release`.

### Phase 6 — Final `v0.1.0`

**Goal:** the first final release, once the release candidates have settled.  
**Depends on:** Phases 2–5 closed, each with its rc; the owner's word that the rcs have settled  
**Plan:** not drafted  
**Exit:** prepare PR `0.1.0` merged; `v0.1.0` published (not a pre-release) and verified with
`gh release verify` and `gh attestation verify`; `vam upgrade` from the last rc reaches `v0.1.0`;
the docs' releases page names `v0.1.0`.

- [ ] Owner's rc check: a server, WSL2, Lima (M3+), a laptop CLI on each OS, all on the last rc
- [ ] Prepare PR, dispatch, approve, verify; release notes cover every rc since nothing
- [ ] Docs: releases page and `project-status.md`

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
