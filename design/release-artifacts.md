# Release artifacts: version, tag, build and publish VAM

Status: **proposal, 2026-11-17**, waiting for the owner's approval. Nothing here is built.
Tracker: `.cursor/workstreams/release-artifacts.md`. Base branch: `main` (cross-cutting;
each phase lands as its own release PR).

Read only the section for the phase in hand.

---

## 1. Goal

A person who wants VAM downloads a published release, verifies it, and runs it without a Rust
toolchain. Each release is one version for the whole machine (`vamd`, `vam`, the brokers, the
guest init, the proto descriptor), built by one workflow from one commit on `main`, and published
under a tag that can never move. The owner starts every release by hand and approves it before
anything is public.

Out of scope for the workstream: publishing crates to crates.io, package managers (Homebrew,
winget, apt), a `vam self-update` command, hosting the docs site, a container image for `vamd`
(see §6.6), and versioning catalog items (agents, skills, playbooks), which have their own semver
(`docs/src/content/docs/orchestrator/catalog.md`).

## 2. What exists today (evidence)

| Fact | Where |
|---|---|
| One workspace version `0.1.0`; every crate inherits it except `vam-firecracker` (own `version = "0.1.0"`) | `Cargo.toml:33`, `crates/vam-firecracker/Cargo.toml:3` |
| No git tags, no release workflow, no `CHANGELOG`. Releases are PRs from a feature branch into `main`, merged with a merge commit and titled `release: <epic> <phases> (#N)` | `git tag` (empty); `git log --merges origin/main`; `phase-playbook.md` §8; `working-agreement.md` "Standing approvals" |
| `vam --version` prints `CARGO_PKG_VERSION (VAM_GIT_SHA)`; `VAM_GIT_SHA` comes from `git rev-parse --short HEAD` (+`+dirty`) and is `unknown` outside a checkout, with no environment override | `crates/vam-cli/src/main.rs:9`, `crates/vam-cli/build.rs` |
| `vamd` has no `--version`; `/v1/health` returns only a serving status; `GET /v1/node` has no version | `crates/vam-orchestrator/src/bin/vamd.rs`; `crates/vam-orchestrator/src/api/mod.rs:121,162` |
| Version strings elsewhere: `vam_core::VERSION`, the marketplace `node_version`, MCP `clientInfo`, OCSF (all `CARGO_PKG_VERSION`); a hard-coded `broker_version: "0.1.0"` | `crates/vam-core/src/lib.rs:32`, `crates/vam-orchestrator/src/marketplace/mod.rs:106`, `mcp_client.rs:372`, `crates/vam-telemetry/src/ocsf.rs:30`, `crates/vam-orchestrator/src/hsp.rs:489` |
| Binaries: `vam`, `vamd`, `hsp-echo-broker` (test broker), `vam-runtime`, `vam-web-broker`, `vam-schedule-broker`, `vam-notify-broker`, `vam-e2e`, `verify_chain` | `[[bin]]` sections of each crate |
| Every binary crate depends on `vam-spec`, which already has a `build.rs` (proto compile, descriptor set to `$OUT_DIR/vam_descriptor.bin`) | `crates/vam-spec/build.rs:40-45`; `crates/vam-cli/Cargo.toml:35`, `brokers/*/Cargo.toml` |
| CI: `rust.yml` (fmt, clippy pinned to 1.97.0, workspace tests, smoke scenarios, microVM lane, release gate, `release lane (microVM)` required on release PRs), `docs.yml`, `dco.yml`, `bench.yml`. All `ubuntu-latest`, `permissions: contents: read`, no `environment:`, actions pinned by tag, not SHA | `.github/workflows/rust.yml:578`; `scripts/e2e/check-release-gate.sh` |
| The repository is **public** (GitHub search, 2026-11-17); `dco.yml` runs only on `squrell-inc/VAM` ("the private predecessor does not require sign-off"). Owner, 2026-11-17: Actions on standard GitHub-hosted runners is free for this repo and is the merge gate | `.github/workflows/dco.yml:15-16` |
| Toolchain: `rust-toolchain.toml` floats on `stable`; clippy pins 1.97.0 (`CLIPPY_TOOLCHAIN` in `scripts/ci/local.sh`) | `rust-toolchain.toml` |
| `deny.toml` checks licences only (no `[advisories]`, no `[bans]`) | `deny.toml` |
| Guest init is a static musl build in `build-rootfs.sh` (`--release --target x86_64-unknown-linux-musl`, `+crt-static`). The `release-guest` profile (`opt-level=z`, LTO, strip) exists but no script uses it | `scripts/build-rootfs.sh:44-56`, `Cargo.toml:20-26` |
| Standard base: `debian:trixie-slim` by digest, apt from `snapshot.debian.org`, Python, git, curl, jq, busybox-static, Bun pinned by sha256; fixed UUID and hash seed, fake time; host ctimes still leak, so `.packages.txt` and `.base.json` are the contract, not image bytes | `scripts/base/Dockerfile`, `scripts/base/pins`, `scripts/build-rootfs.sh:244` |
| Firecracker `v1.16.1` and kernel `vmlinux-6.1.155` (x86_64) pinned by sha256 and fetched from upstream by `vam node init` | `scripts/assets.pins` |
| Install today is from source: `git clone`, `cargo build --release -p vam-cli -p vam-orchestrator`, `sudo vam node init`, which builds the base from the checkout or takes `--base PATH|URL --base-sha256` | `docs/src/content/docs/getting-started/installation.md:39` |
| Nodes run on x86_64 Linux only (`check_platform`); macOS uses an aarch64 Lima VM with a minimal busybox rootfs (the standard base is x64-only), and the page notes a zip of aarch64 `vamd` and assets "is not implemented" | `crates/vam-cli/src/cmd/node_init.rs:228`; `getting-started/macos-lima.md:162`; `scripts/build-rootfs-lima.sh:9-13` |
| Windows: `vam` is built from source (in WSL, copied to `/mnt/c`). It does **not** compile for Windows: `node_install.rs` and `node_init.rs` import `std::os::unix::fs` without a `cfg`, and `cmd/mod.rs` includes both always | `crates/vam-cli/src/cmd/node_install.rs:12`, `node_init.rs:13`, `cmd/mod.rs:16-17`; `getting-started/windows-wsl.md:47` |
| C code in every target's tree: `aws-lc-sys` (via `reqwest 0.13`/rustls 0.23 from `vam-telemetry`, and `rcgen`), `ring` (rustls 0.21), bundled SQLite (`vamd`, notify and schedule brokers). No OpenSSL anywhere | `Cargo.lock`; `cargo tree -p vam-cli --target x86_64-pc-windows-msvc` |
| Secret scanning: `gitleaks` runs only in `scripts/export-public.sh` (with the tree's `.gitleaks.toml`), which also refuses `*.ext4`, `vmlinux*`, `.env` and logs in an export | `scripts/export-public.sh`, `.gitleaks.toml` |
| Local cold release builds (4 cores): `vam-cli` 4 min 11 s; `vamd` and the three brokers 5 min 58 s after the CLI | measured 2026-11-17 |

Precedent: `squrell-inc/vam-marketplace` releases a listing with a manual `workflow_dispatch`
that validates, waits in a protected `release` environment (owner as required reviewer, `main`
only, admin bypass off), then creates the tag with the workflow token and never moves or deletes
a tag. This plan reuses that shape (from the owner's description; that repository was not readable
in this session). Nodes refuse a moved marketplace tag; VAM release tags are treated the same way.

## 3. Versioning

### 3.1 One version for the machine

**One semver version for the whole workspace**, in `[workspace.package] version` in `Cargo.toml`.
Every crate inherits it (`vam-firecracker` moves to `version.workspace = true`). A tag is
`v<version>`. One version because the pieces ship and are tested together: the release gate,
the microVM lane and the scenarios prove a commit, not a crate, and the wire contracts
(`proto/vam/v1`, HSP) carry their own `v1` namespace for compatibility. Per-crate versions would
only matter for crates.io, which is out of scope.

Rejected: per-crate versions (release-plz style). More bookkeeping, no consumer, and a broker at
`0.4.2` beside `vamd` at `0.7.0` tells an operator nothing about whether they work together.

### 3.2 Semver rules before 1.0

- `0.MINOR.PATCH`. **Minor** for any release from `main` that adds features or changes behaviour
  (pre-1.0, a minor may break the API, the CLI or the on-disk state; the notes say so).
  **Patch** for fixes only, cut from `main` like any other release (no release branches yet).
- Pre-release label: `-rc.N` only (`v0.2.0-rc.1`). Published as a GitHub pre-release. No
  `-alpha`/`-beta`. Pre-releases sort below the final under semver, so `v0.2.0-rc.2 < v0.2.0`.
- 1.0 is a separate decision (stable API and state compatibility promise); not in this plan.

### 3.3 Where the version lives and how dev builds differ

- `Cargo.toml` on `main` carries the **next** release's version (after `v0.2.0` ships, the first
  post-release PR sets `0.3.0`). The release workflow refuses to tag a commit whose `Cargo.toml`
  version differs from the version asked for.
- A build is a **release build** only when the release workflow sets `VAM_BUILD_CHANNEL=release`.
  Every other build (local, CI, `cargo install --git`) reports a dev version, so a binary built
  from `main` between releases never claims to be a release:

  | Build | `vam --version` / `vamd --version` |
  |---|---|
  | release workflow | `vam 0.2.0 (3f9c2ab 2026-11-20)` |
  | any other build in a checkout | `vam 0.2.0-dev (3f9c2ab)` or `(3f9c2ab+dirty)` |
  | outside a checkout, no override | `vam 0.2.0-dev (unknown)` |

- One source of build info: `crates/vam-spec/build.rs` (every binary already depends on
  `vam-spec`) exports `VAM_VERSION`, `VAM_GIT_SHA`, `VAM_COMMIT_DATE`, `VAM_BUILD_CHANNEL`;
  `vam_spec::build_info()` returns them. Environment overrides `VAM_GIT_SHA` and
  `VAM_COMMIT_DATE` make a tarball build honest. `crates/vam-cli/build.rs` is folded into it.
  The commit date (not the build time) keeps the string reproducible (`SOURCE_DATE_EPOCH`).
- Reported by: `vam --version`, `vamd --version` (new; prints and exits without reading config),
  `--version` on each broker, `GET /v1/node` (new `build: { version, commit, channel }`, read by
  any signed-in user), `vam node status`. The hard-coded `broker_version: "0.1.0"`
  (`hsp.rs:489`) and `vam_core::VERSION` use the same source. The marketplace `node_version`
  keeps the plain semver (without `-dev`) so `requires_node` ranges still match on dev builds.

### 3.4 Changelog and release notes

No `CHANGELOG.md`. The record already exists: every change reaches `main` through a
`release: …` merge commit whose PR body carries a deploy note (`phase-playbook.md` §8). The
release workflow writes the notes from the first-parent merges between the previous tag and the
commit:

```text
git log --first-parent --merges --format='%s' v0.1.0..<sha>   # "release: ... (#N)" lines
```

grouped as **Releases** (the `release:` titles, each linking its PR), **Upgrade notes** (the
"Deploy note" sections of those PR bodies, verbatim), and **Artifacts and verification** (the
asset table and the verify commands, §6.4). The notes are part of the draft the owner reviews at
the approval gate. The docs page `getting-started/project-status.md` links the releases page.

## 4. Release trigger

### 4.1 Recommendation: manual dispatch behind a protected environment

`.github/workflows/release.yml`, `on: workflow_dispatch` only, inputs:

| Input | Default | Meaning |
|---|---|---|
| `version` | (required) | `0.2.0` or `0.2.0-rc.1`; regex `^[0-9]+\.[0-9]+\.[0-9]+(-rc\.[0-9]+)?$` |
| `commit` | head of `main` | full 40-hex SHA to release |
| `dry_run` | `true` | build, check and upload as workflow artifacts; never touch tags or releases |

Jobs:

```text
validate (read-only) ──► build matrix (read-only, no secrets, no caches) ──► assemble (SHA256SUMS, SBOM, notes, scans)
                                                                                   │ dry_run=true: stop here
                                                                                   ▼
                                     publish (environment: release; owner approves) ──► draft release + tag at <commit>
                                                                                         ► upload assets ► attest ► verify ► publish
```

Rejected: **tag-on-merge** (tag every `release:` merge into `main`, or push a tag to trigger the
build). It makes merging and publishing the same act: hotfix and design-cleanup releases
(`release: design/ cleanup (#7)`) would ship binaries; a pushed tag exists before anything is
built or checked, so a failed build leaves a tag behind; and the tag would be created by a person,
not the gated workflow. Manual dispatch keeps the marketplace's shape: nothing public until the
owner approves the exact artifacts.

### 4.2 Guards (all in `validate`, each failing with the reason)

1. The actor dispatched from `main` (`github.ref == refs/heads/main`), so the workflow file that
   runs is the reviewed one.
2. `commit` is on `main`'s first-parent history (`git merge-base --is-ancestor` and present in
   `git rev-list --first-parent origin/main`), so it is a merge into `main`, not a commit from
   inside a feature branch.
3. CI is green on that exact SHA: the `rust.yml` run for the `push` to `main` at `commit`
   concluded `success` (Actions API, `head_sha` filter). If it is missing or red, the release
   fails and names the run; the owner re-runs `rust.yml` for that SHA, never the release.
4. `Cargo.toml` at `commit` has `version == inputs.version` (plain part; `-rc.N` allowed only as
   an input suffix over the same `Cargo.toml` version).
5. Tag `v<version>` does not exist locally or on the remote, and no release (draft included)
   uses it. An immutable release burns its tag name for good (§6.5), so a failed publish means the
   next number.
6. `version` is greater, by semver, than every existing `v*` tag (a final may follow its own rc).
7. The release gate passes at `commit` (`scripts/e2e/check-release-gate.sh`; cheap, no KVM).
8. Inputs reach shell only through `env:`, never `${{ inputs.* }}` inside `run:`.

### 4.3 Tag protection

- Repository setting **immutable releases** on (GA since 2025-10-28): once published, a release's
  tag cannot move or be deleted, its assets cannot change, and GitHub signs a release attestation.
- A repository **ruleset on `refs/tags/v*`**: restrict updates, restrict deletions, block force
  pushes; **no bypass actors, admins included**. This covers the window before publish, and tags
  without a release.
- Creation: the workflow creates the tag through `gh release create v<version> --target <commit>
  --draft` with `GITHUB_TOKEN` (`contents: write`). Whether a "restrict creations" rule can let
  only the workflow create `v*` tags is unproven: community reports say the GitHub Actions app
  cannot be a bypass actor. Phase 2 tests it in a scratch repository first (open question 3);
  the default is to leave creation unrestricted and accept that a maintainer with write access
  could push a stray `v*` tag, which the next release's guard 5 refuses and the immutable-release
  check below exposes.
- A node or user trusts a tag only if it has a **published immutable release** whose attestations
  verify (`gh release verify v<version>`); a bare tag means nothing.

## 5. Artifacts and targets

### 5.1 Matrix (native GitHub-hosted runners)

| Asset | Contents | Target | Runner |
|---|---|---|---|
| `vam-node-<v>-x86_64-unknown-linux-gnu.tar.gz` | `vamd`, `vam`, `vam-web-broker`, `vam-schedule-broker`, `vam-notify-broker`, `vam-runtime` (guest init), `share/vam/base/` (the base build kit, §5.3), `scripts/assets.pins`, `LICENSE-*`, `NOTICE`, `README` | x86_64 gnu, glibc ≥ 2.34 | `ubuntu-24.04` |
| `vam-<v>-x86_64-unknown-linux-gnu.tar.gz` | `vam` | x86_64 gnu | `ubuntu-24.04` |
| `vam-<v>-aarch64-unknown-linux-gnu.tar.gz` | `vam` | aarch64 gnu | `ubuntu-24.04-arm` |
| `vam-<v>-aarch64-apple-darwin.tar.gz` | `vam` | macOS arm64 | `macos-15` |
| `vam-<v>-x86_64-apple-darwin.tar.gz` | `vam` | macOS x86_64 | `macos-15-intel` (last Intel image; retires 2027) |
| `vam-<v>-x86_64-pc-windows-msvc.zip` | `vam.exe` | Windows x86_64 | `windows-2025` (Phase 3, after the CLI compiles there) |
| `vam-runtime-<v>-x86_64-unknown-linux-musl` | the guest init alone | static musl | `ubuntu-24.04` |
| `vam-proto-<v>.tar.gz` | `proto/vam/v1/*.proto` + `vam.binpb` (FileDescriptorSet with imports) | any | `ubuntu-24.04` |
| `SHA256SUMS` | one line per asset above | — | publish job |
| `*.cdx.json` | CycloneDX SBOM per archive | — | build jobs |
| `RELEASE_NOTES.md` | the notes (§3.4) | — | assemble |

- **`vamd` and the brokers: x86_64 Linux only** for now. Nodes need x86_64 KVM, and the pinned
  Firecracker and kernel are x86_64 (`node_init.rs:228`, `assets.pins`). An aarch64 node bundle
  (for the Lima path) waits for an aarch64 standard base, kernel and Firecracker pins (§8, later).
- **gnu, not musl, for host binaries**, built for glibc 2.34 (the oldest documented host,
  Amazon Linux 2023; Ubuntu 22.04 has 2.35) with `cargo-zigbuild --target
  x86_64-unknown-linux-gnu.2.34`. The assemble job checks the highest `GLIBC_` symbol version
  with `objdump -T`. musl for a daemon brings a slower allocator and a musl C toolchain for
  aws-lc, ring and SQLite, for no gain on a host that already runs systemd and Docker.
- **Guest init**: built the way `build-rootfs.sh` builds it today (same flags), from one shared
  script, so the released `vam-runtime` is the one the lane proved. Moving both to the
  `release-guest` profile is a separate change proven on the lane (open question 9).
- **Native runners for every target.** The tree compiles C (aws-lc, ring, SQLite) on every
  target, which is where `cross` (qemu) and macOS-from-Linux (Apple SDK) hurt. Standard runners,
  macOS and arm64 included, are free for this public repository. `cargo-zigbuild` is used only on
  Linux for the glibc floor.
- **Rejected: `cargo-dist`.** It generates a tag-push-driven `release.yml` with mutable action
  tags; fitting it to an environment-gated dispatch with immutable tags means patching generated
  YAML on every upgrade, and its 2026 maintenance could not be confirmed. A hand-written workflow
  of about 250 lines is easier to audit.

### 5.2 What is not published

- **No `rootfs.ext4`** in phase 1–3. It redistributes GPL and LGPL binaries (git, busybox,
  bash, coreutils, glibc), so a published image needs the corresponding source alongside or a
  three-year written offer (Debian's guidance for image vendors), plus licence texts. Debian makes
  no offer we can pass on, and pointing at mirrors is contested. Instead the node bundle carries
  the **base build kit** and the released `vam-runtime`, and `vam node init` builds the base on
  the host from pinned inputs (digest-pinned Debian image, snapshot apt, sha256-pinned Bun). A
  published image with a source archive is a later phase after a licence review (§8).
- **No Firecracker, jailer or guest kernel.** `vam node init` already fetches them from upstream
  and checks the pinned sha256 (`assets.pins`); re-hosting the kernel brings the GPL-2.0 source
  duty with it.
- **No `vamd` container image on GHCR.** `vamd` needs `/dev/kvm`, root, cgroup v2, the jailer and
  systemd on the host; a container would run privileged with host mounts and add a second install
  path to test, for no isolation gain. Revisit if a hosted control plane without local VMs appears.
- **No docs-site bundle.** The docs site is built by `docs.yml`; hosting is separate.

### 5.3 The base build kit

`share/vam/base/` in the node bundle: `scripts/base/` (Dockerfile, pins, resolve-pins),
`build-rootfs.sh`, and a `manifest.json` naming the version and the sha256 of each file.
`vam node init` gains `--kit DIR` (default: the `share/vam/base/` beside the running `vam`), so a
node installs from an extracted bundle with Docker but **without a Rust toolchain or a git
checkout**: `build-rootfs.sh --runtime <path>` takes the released `vam-runtime` instead of
compiling one. `--checkout DIR` stays for source installs. The base built this way must report the
same `.packages.txt` as the lane's base at the same commit (same pins).

### 5.4 Checksums, SBOM and provenance

- **`SHA256SUMS`**, one file for every asset, in `sha256sum` format.
- **GitHub artifact attestations** (`actions/attest`, build provenance) for every asset and
  `SHA256SUMS`, and an SBOM attestation per archive. Free for public repositories, signed through
  Sigstore public-good, verified with `gh attestation verify <file> -R squrell-inc/VAM
  --signer-workflow squrell-inc/VAM/.github/workflows/release.yml`, and offline with
  `gh attestation download` plus `gh attestation trusted-root`. Provenance from a hosted runner is
  SLSA Build L2; L3 needs the build in a reusable workflow (later).
- **Immutable release attestation**: GitHub signs the release itself
  (`gh release verify v<v>`, `gh release verify-asset v<v> <file>`).
- **No long-lived signing key.** Keyless signing has no key to protect or rotate; the public
  repository means the Sigstore identity (repo, workflow, ref) in the public log reveals nothing.
  A `cosign` bundle for `SHA256SUMS` (for people without `gh`) is a later addition.
- **SBOM**: binaries built with `cargo auditable` (dependency list embedded, readable by
  `cargo audit bin`, Grype, Trivy); one CycloneDX file per archive from `cargo cyclonedx`.
- **Release gate additions**: `deny.toml` gains `[advisories]` and `[bans]`; the release runs
  `cargo deny check` (advisories, bans, licenses, sources) at the commit and `cargo audit bin` on
  the built binaries.

### 5.5 Install and docs

A new page `getting-started/releases.md` (sidebar under Getting started): which archive for which
machine, download, verify (`sha256sum -c SHA256SUMS --ignore-missing`, `gh attestation verify`),
install (`vam` on `PATH`; on a node, extract the node bundle and run `sudo ./vam node init`). The
pages that today say "build from source" (`installation.md`, `macos-lima.md`, `windows-wsl.md`,
`cli.md`, `project-status.md`) lead with the release and keep the source build as the second path.

## 6. Build infrastructure

### 6.1 Toolchain and reproducibility

- Release builds use the toolchain pinned for clippy (`1.97.0`, `CLIPPY_TOOLCHAIN`), passed as
  `cargo +1.97.0`, so a Rust bump stays the one PR it is today. `rust-toolchain.toml` is not
  changed for development.
- `--locked`, `CARGO_INCREMENTAL=0`, `SOURCE_DATE_EPOCH` = the commit's time,
  `--remap-path-prefix` for the checkout and `$CARGO_HOME`, `strip` as in the release profile.
- The promise is **same inputs, same versions** (lockfile, toolchain, pins, `packages.txt`), not
  bit-identical binaries. A rebuild check (build twice, compare hashes) is a later phase.

### 6.2 Caching

**No caches in release builds** (no `Swatinem/rust-cache`, no `actions/cache`). PR and branch
runs write caches the release would read, which is a cache-poisoning path into signed artifacts.
Builds are cold; on free standard runners that costs wall time only.

### 6.3 Cost

Standard GitHub-hosted runners, macOS and arm64 included, are free for this public repository
(owner, 2026-11-17), so Actions minutes are not a constraint. A release is about 70 runner-minutes
of cold builds spread over six parallel jobs, about **20 minutes wall time** (longest leg: the
node bundle with zigbuild, or the Windows CLI). The owner's approval wait is extra. Only a larger
or self-hosted runner would cost money; this plan uses none. For reference, the same release on
private-repository pricing would be about US$1.35 (US$0.79 without the macOS Intel CLI).

### 6.4 Keeping secrets out

- Build jobs: `permissions: contents: read`, no secrets in the environment, `persist-credentials:
  false`. The only token in the publish job is `GITHUB_TOKEN` with `contents: write`,
  `id-token: write`, `attestations: write`. No repository or environment secret is needed.
- `assemble` runs `gitleaks dir` with the tree's `.gitleaks.toml` over the extracted archives,
  greps binaries for `vam_sk_`, `/home/runner` and other runner paths (after the path remap), and
  refuses any `.env`, `*.ext4`, `vmlinux*` or log file, the same list `export-public.sh` refuses.
- All third-party actions pinned by full commit SHA in `release.yml` (and Dependabot for action
  bumps). harden-runner is optional and later.

## 7. Phases

Each phase is one release PR into `main`, planned and built through the phased-workstream
procedure. Phases 1 and 2 are infrastructure: their proof is the workflow's own checks and a
recorded run, plus product scenarios where the behaviour is visible through the CLI or API.

| Phase | Name | Exit |
|---|---|---|
| 0 | Version identity | `vam`, `vamd` and each broker print the same version line; `GET /v1/node` reports `build`; dev builds say `-dev`; `vam-firecracker` inherits the workspace version; `deny.toml` checks advisories and bans; scenarios green |
| 1 | Release workflow, dry run | `release.yml` validates, builds the Linux, macOS and guest assets, writes `SHA256SUMS`, SBOMs and notes, scans, and uploads workflow artifacts; a dry run on `main` is green; guards refuse each bad input in a test |
| 2 | Publish | `release` environment, immutable releases and the `v*` ruleset set by the owner; publish job creates the draft, attests, verifies and publishes; `v0.1.0-rc.1` published and verified, then `v0.1.0` |
| 3 | Install from a release | `vam` builds for Windows; `vam node init` installs from an extracted node bundle without Rust or a checkout; docs lead with the release; the next minor ships with these |
| later | see §8 | — |

Scenarios the phases are expected to add (names final at the phase breakdown):

- Phase 0: `cli-prints-its-version-and-commit`, `vamd-prints-its-version-without-starting`,
  `node-reports-the-version-it-runs` (CLI and `vamd` agree), `dev-build-does-not-claim-a-release`.
- Phase 3: `node-init-from-a-release-bundle-needs-no-checkout` (process layer, `--root` seam,
  `--kit` from a fixture bundle), and a change to `node-init-check-names-what-is-missing` if the
  check lists the kit.

## 8. Explicitly later

- aarch64 node bundle and Lima zip (needs an arm64 standard base, kernel and Firecracker pins).
- A published `rootfs.ext4` with a source archive, after a licence review.
- `cosign` bundle for `SHA256SUMS`; SLSA Build L3 via a reusable workflow; rebuild check.
- `vam` verifying downloads itself (`vam node init --from-release v0.2.0`), self-update.
- Package managers (Homebrew tap, winget, apt repository).
- Release branches for patching an older minor.

## 9. Owner checklist

### One-time setup (before Phase 2's first publish)

1. Settings → Environments → new `release`: required reviewer = owner, "prevent self-review"
   off (the owner both dispatches and approves), deployment branches = `main` only,
   "allow administrators to bypass" **off**.
2. Settings → General → Releases: **enable immutable releases**.
3. Settings → Rules → new tag ruleset `release tags`, target `v*`: restrict updates, restrict
   deletions, block force pushes; bypass list empty. (Creation per open question 3.)
4. Settings → Actions → General: "Require actions to be pinned to a full-length commit SHA" on
   (org or repo policy), if not already.

### Every release

1. The release PR (or PRs) for the cycle are merged into `main`; `rust.yml` on that merge is green.
2. `Cargo.toml` on `main` carries the version to release (bumped by the cycle's first PR).
3. Actions → `release` → Run workflow from `main`: `version`, `commit` (or blank for the head),
   `dry_run: true`. Read the summary: guards, asset table, notes.
4. Run again with `dry_run: false`. Review the pending deployment: asset table, `SHA256SUMS`,
   notes. Approve.
5. On the published release: `gh release verify v<v>`, and download one archive and run
   `gh attestation verify` on it. `vam --version` from it prints `<v> (<sha> <date>)`.
6. Open the post-release PR: `Cargo.toml` to the next minor.
7. If the publish failed after the tag was created: do not delete or reuse it; release the next
   patch (or `-rc.N+1`).

## 10. Open questions (each with a default)

1. **First release number.** Default: `v0.1.0-rc.1` to prove the publish path (a mistake burns
   only an rc), then `v0.1.0` from the same commit.
2. **Pre-1.0 policy.** Default: minor may break; every release note has "Upgrade notes".
3. **Restrict `v*` tag creation.** Default: no creation restriction; updates and deletions
   restricted with no bypass. Phase 2 tests in a scratch repository whether the workflow token can
   create under a creation restriction; if a GitHub App is needed, that is a later change.
4. **glibc floor.** Default: 2.34 (Amazon Linux 2023) via `cargo-zigbuild`. Alternative: fully
   static musl host binaries.
5. **macOS Intel CLI.** Default: build it while `macos-15-intel` exists (free), drop it then.
6. **Windows CLI.** Default: native `vam.exe` in Phase 3, after gating the unix-only `node init`
   modules (`node init` reports "Linux only" on Windows); WSL stays the node path.
7. **CI-green guard.** Default: require the `rust.yml` push run on the exact SHA; the release does
   not re-run the workspace suite (it re-runs only the cheap release gate and `cargo deny`).
8. **Changelog file.** Default: none; GitHub release notes generated from `release:` merges.
9. **Guest init profile.** Default: release builds the guest exactly as `build-rootfs.sh` does;
   moving both to `release-guest` is a separate lane-proven change.
10. **Version bump.** Default: a manual post-release PR sets the next minor. The workflow does not
    push to `main`.
11. **Who dispatches.** Default: owner only (environment reviewer); anyone with write access can
    start a dry run.
