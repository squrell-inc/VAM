# Workstream: Release artifacts (version, tag, build, publish)

```yaml
status: active
slug: release-artifacts
current_phase: 0 (plan proposed 2026-11-17; waiting for the owner's approval)
base_branch: main (cross-cutting; one release PR per phase)
design_docs:
  - design/release-artifacts.md
  - docs/src/content/docs/getting-started/installation.md
```

## Goal

A published VAM release is one version for the whole machine, built by one manually started,
owner-approved workflow from one green commit on `main`, published under an immutable `v*` tag
with `SHA256SUMS`, SBOMs and GitHub attestations. A user installs `vam` on Linux, macOS or
Windows, and a node from the x86_64 node bundle, without a Rust toolchain or a git checkout.

## Out of scope

- crates.io, Homebrew, winget, apt; `vam self-update`.
- A `vamd` container image (design §5.2).
- Publishing `rootfs.ext4`, Firecracker or the guest kernel (built or fetched on the host from
  pins; a published image waits for a licence review, design §8).
- An aarch64 node bundle (needs arm64 base, kernel and Firecracker pins).
- Catalog item versions (agents, skills, playbooks have their own semver).

## Locked decisions

Do not re-litigate without editing this list. **Proposed 2026-11-17; locked when the owner
approves the plan.**

1. One semver version for the workspace in `[workspace.package]`; every crate inherits it
   (`vam-firecracker` included). Tags are `v<version>`. Pre-release label `-rc.N` only.
2. Pre-1.0: a minor may break; a patch only fixes. `Cargo.toml` on `main` carries the next
   release's version; a post-release PR bumps it. The workflow never pushes to `main`.
3. A build is a release build only when the release workflow sets `VAM_BUILD_CHANNEL=release`;
   every other build prints `<version>-dev (<sha>)`. One build-info source in
   `crates/vam-spec/build.rs`, with `VAM_GIT_SHA` / `VAM_COMMIT_DATE` overrides. `vam`, `vamd`,
   each broker and `GET /v1/node` report the same version and commit.
4. Releases start only by `workflow_dispatch` (`version`, `commit`, `dry_run` default `true`)
   from `main`, and publish only after the owner approves the protected `release` environment
   (owner as required reviewer, `main` only, admin bypass off). No tag-on-merge.
5. Guards before any build: commit on `main`'s first-parent history; the `rust.yml` push run on
   that SHA succeeded; `Cargo.toml` version matches; tag and release absent; version greater than
   every `v*` tag; release gate passes; inputs reach shell only through `env:`.
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
11. `vamd` and the brokers ship for x86_64 Linux gnu only; `vam` for Linux x86_64/aarch64, macOS
    arm64/x86_64 and (Phase 3) Windows x86_64; the guest init as static x86_64 musl, built exactly
    as `build-rootfs.sh` builds it.
12. No `CHANGELOG.md`: release notes are generated from the first-parent `release:` merges and
    their deploy notes, and reviewed at the approval gate.

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
dev build never claims to be a release.  
**Depends on:** none  
**Plan:** not drafted (breakdown after the owner approves this tracker)  
**Exit:** the phase's scenarios are active and green on Actions; `vam --version`,
`vamd --version`, `vam-web-broker --version`, `vam-schedule-broker --version`,
`vam-notify-broker --version` print `<name> <version>[-dev] (<sha>[+dirty][ <date>])`;
`GET /v1/node` and `vam node status` show `build.version`, `build.commit`, `build.channel`;
`cargo deny check` passes with advisories and bans.

- [ ] Build info in `crates/vam-spec/build.rs` (`VAM_VERSION`, `VAM_GIT_SHA`, `VAM_COMMIT_DATE`,
      `VAM_BUILD_CHANNEL`, env overrides); `vam_spec::build_info()`; fold `crates/vam-cli/build.rs`
- [ ] `vamd --version` (no config read, no side effects); `--version` on the three brokers
- [ ] `GET /v1/node` `build` object (proto, OpenAPI, shape test); `vam node status` prints it
- [ ] `hsp.rs:489` `broker_version` and `vam_core::VERSION` from build info; `vam-firecracker`
      inherits the workspace version; marketplace `node_version` stays plain semver
- [ ] `deny.toml` `[advisories]` and `[bans]`; `cargo deny check` in the release gate
- [ ] Docs: `getting-started/cli.md`, `orchestrator/api.md` (node), `orchestrator/overview.md`

Expected scenarios (final names at the breakdown): `cli-prints-its-version-and-commit`,
`vamd-prints-its-version-without-starting`, `node-reports-the-version-it-runs`,
`dev-build-does-not-claim-a-release`.

### Phase 1 — Release workflow, dry run

**Goal:** `release.yml` validates inputs and builds, checks and packages every phase-1 asset
without publishing anything.  
**Depends on:** Phase 0  
**Plan:** not drafted  
**Exit:** a `dry_run: true` dispatch on `main` is green and uploads the assets as workflow
artifacts (node bundle x86_64; `vam` for Linux x86_64/aarch64 and macOS arm64/x86_64; guest
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
- [ ] `release.yml` added to `rust.yml`'s VM-path filter only if it touches lane paths (it should not)

No product scenario: the proof is the guard tests and the recorded dry-run (design §7).

### Phase 2 — Publish

**Goal:** the first immutable, attested release, `v0.1.0-rc.1`, then `v0.1.0`.  
**Depends on:** Phase 1; the owner's one-time setup (design §9)  
**Plan:** not drafted  
**Exit:** `gh release verify v0.1.0` and `gh attestation verify` on every asset pass; the tag
points at the dispatched commit; deleting or moving the tag is refused by the ruleset; a second
dispatch with the same version fails at the guard.

- [ ] Scratch-repository test: can `GITHUB_TOKEN` create a `v*` tag under a "restrict creations"
      ruleset (decides open question 3)
- [ ] `publish` job in environment `release`: draft release at the commit, upload, `actions/attest`
      (provenance and SBOM), `gh release verify-asset` per asset, publish (pre-release for `-rc`)
- [ ] Job summary: asset table, guards, notes; the approval page shows them
- [ ] `v0.1.0-rc.1` published and verified; `v0.1.0` published and verified
- [ ] Owner checklist run end to end and amended where it was wrong

### Phase 3 — Install from a release

**Goal:** people install from the release, not from source.  
**Depends on:** Phase 2  
**Plan:** not drafted  
**Exit:** `vam.exe` builds and runs `vam --version`, `vam login` and `vam health` on
`windows-2025`; `vam node init` installs a node from an extracted node bundle on a host with
Docker but no Rust and no checkout; the next minor ships with the Windows asset; the docs lead
with the release.

- [ ] Gate the unix-only `node init` / `node install` modules (`node init` says Linux only
      elsewhere); `windows-2025` job in `release.yml`
- [ ] Node bundle carries `share/vam/base/` (base build kit + manifest) and `vam-runtime`;
      `vam node init --kit DIR` (default beside the binary); `build-rootfs.sh --runtime PATH`
- [ ] Docs: new `getting-started/releases.md` (sidebar), `installation.md`, `macos-lima.md`,
      `windows-wsl.md`, `cli.md`, `project-status.md`

Expected scenarios: `node-init-from-a-release-bundle-needs-no-checkout`; a change to
`node-init-check-names-what-is-missing` if the check lists the kit.

## Open questions

Each has a default; see design §10 for the reasoning.

1. First release number — `v0.1.0-rc.1`, then `v0.1.0`.
2. Pre-1.0 policy — minor may break; notes carry "Upgrade notes".
3. Restrict `v*` tag creation — no; updates and deletions restricted; Phase 2 tests the token.
4. glibc floor — 2.34 via `cargo-zigbuild`.
5. macOS Intel CLI — yes while `macos-15-intel` exists.
6. Windows CLI — native `vam.exe` in Phase 3; WSL stays the node path.
7. CI-green guard — the `rust.yml` push run on the exact SHA; no suite re-run in the release.
8. Changelog file — none.
9. Guest init profile — as `build-rootfs.sh` builds it today.
10. Version bump — manual post-release PR to the next minor.
11. Who dispatches — owner approves every publish; any writer may dry-run.

## Explicitly later

- aarch64 node bundle and Lima zip.
- Published `rootfs.ext4` with a source archive, after a licence review.
- `cosign` bundle for `SHA256SUMS`; SLSA Build L3 via a reusable workflow; rebuild check.
- `vam node init --from-release`, self-update, package managers, release branches.

## Progress log

| Date | Phase | What landed (end with the scoreboard: scenarios green, red, fix cycles, wall time) |
|---|---|---|
| 2026-11-17 | — | Tracker opened with design `design/release-artifacts.md`; plan proposed, waiting for the owner's approval. Scoreboard: no scenarios yet, 0 fix cycles |
