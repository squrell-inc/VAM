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
anything is public. A user puts `vam` on their `PATH` with one command and later upgrades it with
another (Phase 5). §11 walks through the whole thing from the developer's side and the user's side,
per platform.

Out of scope for the workstream: publishing crates to crates.io, package managers (Homebrew,
winget, apt), upgrading a running node in place (`vam node upgrade`), hosting the docs site, a
container image for `vamd` (see §5.2), and versioning catalog items (agents, skills, playbooks),
which have their own semver (`docs/src/content/docs/orchestrator/catalog.md`).

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

**The next version is chosen when releasing, never guessed in advance.** Nobody knows after
`v0.2.0` whether the next release is `0.2.1` or `0.3.0`; that is decided by what merged, at the
moment the owner decides to release.

- **Prepare PR, at release time.** The owner (or a session) runs `scripts/release/prepare.sh
  0.3.0`, which sets `[workspace.package] version` and refreshes the workspace entries in
  `Cargo.lock`, and opens a one-line PR `release: prepare v0.3.0`. Its merge into `main` is the
  commit that gets tagged (its `rust.yml` push run is the CI-green proof, guard 3). So the tagged
  source says `0.3.0` inside, and a build from the tag's source archive reports `0.3.0`. For an rc,
  the prepare PR sets `0.3.0-rc.1`; the final needs its own prepare PR.
- **No post-release PR.** Between releases `Cargo.toml` simply still names the last release.
- **Release build**: only the release workflow sets `VAM_BUILD_CHANNEL=release`, and it refuses
  unless `Cargo.toml` at the commit equals the version asked for.
- **Dev build** (local, CI, `cargo install --git`, any build between releases): the version is the
  nearest `v*` tag plus semver *build metadata* saying how far past it the build is
  (`git describe`), so it never claims to be a release and never names a future version:

  | Build | `vam --version` / `vamd --version` |
  |---|---|
  | release workflow at `v0.3.0` | `vam 0.3.0 (3f9c2ab 2026-11-20)` |
  | dev build, 14 merges after `v0.3.0` | `vam 0.3.0+dev.14 (9be41d0)` or `(9be41d0+dirty)` |
  | dev build before any tag exists | `vam 0.1.0+dev (9be41d0)` (the `Cargo.toml` version) |
  | outside a checkout, no override | `vam 0.3.0+dev (unknown)` |

  Build metadata (`+…`) is ignored by semver ordering, so a dev build compares equal to the
  release it grew from: the marketplace `requires_node` check treats `0.3.0+dev.14` as `0.3.0`.
- One source of build info: `crates/vam-spec/build.rs` (every binary already depends on
  `vam-spec`) exports `VAM_VERSION`, `VAM_GIT_SHA`, `VAM_COMMIT_DATE`, `VAM_BUILD_CHANNEL`;
  `vam_spec::build_info()` returns them. Environment overrides `VAM_GIT_SHA` and
  `VAM_COMMIT_DATE` make a tarball build honest. `crates/vam-cli/build.rs` is folded into it.
  The commit date (not the build time) keeps the string reproducible (`SOURCE_DATE_EPOCH`).
- Reported by: `vam --version` (the CLI's own version), `vamd --version` (new; the daemon's
  version, prints and exits without reading config), `--version` on each broker,
  `GET /v1/node` (new `build: { version, commit, channel }`, read by any signed-in user), and
  `vam node status`, which prints the node's version beside the CLI's and warns when their
  minor versions differ. The hard-coded `broker_version: "0.1.0"` (`hsp.rs:489`),
  `vam_core::VERSION` and the marketplace `node_version` use the same source.

Rejected: (a) `main` carries the *next* version with a post-release bump PR: it guesses minor vs
patch before anything merged. (b) The workflow stamps the version only inside its build without
committing it: no extra PR, but the tagged source still says the old version, so a build from the
tag's source archive reports the wrong number.

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
asset table and the verify commands, §5.4). The notes are part of the draft the owner reviews at
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
4. `Cargo.toml` at `commit` has `version == inputs.version` exactly (the prepare PR, §3.3, is
   merged at or before `commit`).
5. Tag `v<version>` does not exist locally or on the remote, and no release (draft included)
   uses it. An immutable release burns its tag name for good (§4.3), so a failed publish means the
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
| `vam-node-<v>-x86_64-unknown-linux-gnu.tar.gz` | `vamd`, `vam`, `vam-web-broker`, `vam-schedule-broker`, `vam-notify-broker`, `vam-runtime` (guest init), `share/vam/base/` (the standard base recipe, §5.3), `scripts/assets.pins`, `LICENSE-*`, `NOTICE`, `README` | x86_64 gnu, glibc ≥ 2.34 | `ubuntu-24.04` |
| `vam-node-<v>-aarch64-unknown-linux-gnu.tar.gz` | the same set for arm64 Linux, incl. an aarch64 `vam-runtime` (Phase 4: Lima on Apple Silicon, arm64 servers) | aarch64 gnu | `ubuntu-24.04-arm` |
| `install.sh`, `install.ps1` | download the right `vam` for the machine, check it against `SHA256SUMS`, run `vam self install` (Phase 5) | — | assemble |
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

- **`vamd` and the brokers: x86_64 Linux first, aarch64 Linux in Phase 4.** Today nodes are
  x86_64 only (`node_init.rs:228`) and the pinned Firecracker and kernel are x86_64
  (`assets.pins`). The Mac path is a Lima VM, which on Apple Silicon is aarch64 Linux, so Phase 4
  adds an aarch64 node bundle with an aarch64 standard base (aarch64 Debian digest and Bun pin),
  aarch64 Firecracker and kernel pins, and `check_platform` accepting aarch64 Linux.
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
  the **standard base recipe** and the released `vam-runtime`, and `vam node init` builds the base on
  the host from pinned inputs (digest-pinned Debian image, snapshot apt, sha256-pinned Bun). A
  published image with a source archive is a later phase after a licence review (§8).
- **No Firecracker, jailer or guest kernel.** `vam node init` already fetches them from upstream
  and checks the pinned sha256 (`assets.pins`); re-hosting the kernel brings the GPL-2.0 source
  duty with it.
- **No `vamd` container image on GHCR.** `vamd` needs `/dev/kvm`, root, cgroup v2, the jailer and
  systemd on the host; a container would run privileged with host mounts and add a second install
  path to test, for no isolation gain. Revisit if a hosted control plane without local VMs appears.
- **No docs-site bundle.** The docs site is built by `docs.yml`; hosting is separate.

### 5.3 The standard base recipe and runtime injection

No image is shipped; a **recipe** and the **guest init** are, and the node builds the image:

```text
 node bundle                               on the node (vam node init)
 share/vam/base/
   Dockerfile + pins  ──── recipe ───────► docker build (pinned Debian, snapshot apt, pinned Bun)
   build-rootfs.sh                         export → staging tree
   manifest.json                           copy vam-runtime → /sbin/init (last, so nothing in
 vam-runtime (musl, ─────── binary ──────►   the image can replace PID 1)
   built and attested in CI)               mkfs.ext4 → /var/lib/vam/rootfs.ext4 + .base.json
```

- `share/vam/base/` holds `scripts/base/` (Dockerfile, pins, resolve-pins), `build-rootfs.sh`,
  and a `manifest.json` naming the version and the sha256 of each file.
- **Runtime injection without compiling.** `build-rootfs.sh` already injects `vam-runtime` as
  `/sbin/init` (`build-rootfs.sh:222-227`), but compiles it first (Rust plus `musl-gcc`). It
  gains `--runtime PATH`: inject this binary instead of building one. From a release, the node
  injects the `vam-runtime` that CI built and attested, so a node needs Docker but **no Rust and
  no `musl-gcc`**.
- `vam node init` gains `--kit DIR` (default: the `share/vam/base/` beside the running `vam`) and
  passes `--runtime` the bundle's `vam-runtime`. `--checkout DIR` stays for source installs.
  The base built this way reports the same `.packages.txt` as the lane's base at the same commit.
- **The recipe is the first standard Vamfile.** The planned Vamfile (`design/agent-experience.md`
  Part III, E7 Phase 3; `docs/src/content/docs/build/vamfile.md`, `status: planned`) uses
  Docker's grammar plus VAM directives (`RUNTIME`, `PACKAGES`, `TEST`), with runtime injection
  done by the builder. The shipped `Dockerfile` is written to stay inside that grammar (no
  instruction the Vamfile v1 parser will refuse), and the manifest names it `vam/base:2`. When
  E7 Phase 3 lands, the file is renamed `Vamfile`, `vam node init` calls `vam image build` on it
  instead of `build-rootfs.sh`, and the bundle layout (recipe + runtime) does not change. The
  release workflow never needs to know which builder the node uses.
- `vam` (the CLI) does not go into the image. It drives the build: `vam node init` today,
  `vam image build` once the Vamfile builder exists.

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
- **SBOM** (software bill of materials): the parts list of a binary, every crate and its
  version, licence and source hash, so a question such as "does `v0.3.0` contain the vulnerable
  version of crate X?" is answered by reading a file instead of rebuilding. Two forms: binaries
  built with `cargo auditable` carry the list inside the binary (a few KB, read by
  `cargo audit bin`, Grype, Trivy, so a binary found on a server can be scanned on its own), and
  one CycloneDX JSON file per archive (`*.cdx.json`, from `cargo cyclonedx`), attested so it
  cannot be swapped.
- **Release gate additions**: `deny.toml` gains `[advisories]` and `[bans]`; the release runs
  `cargo deny check` (advisories, bans, licenses, sources) at the commit and `cargo audit bin` on
  the built binaries.

### 5.5 Install and docs

A new page `getting-started/releases.md` (sidebar under Getting started): which archive for which
machine, download, verify (`sha256sum -c SHA256SUMS --ignore-missing`, `gh attestation verify`),
install (`vam` on `PATH`; on a node, extract the node bundle and run `sudo ./vam node init`). The
pages that today say "build from source" (`installation.md`, `macos-lima.md`, `windows-wsl.md`,
`cli.md`, `project-status.md`) lead with the release and keep the source build as the second path.

### 5.6 Brokers on a node

Every first-party broker ships as its own binary in the node bundle: `vam-web-broker`,
`vam-schedule-broker`, `vam-notify-broker`. `hsp-echo-broker` (a test fixture in
`vam-orchestrator`), `vam-e2e` and `verify_chain` are not shipped.

A broker is a separate process that `vamd` dials on a Unix socket, registered with
`PUT /v1/hosted/brokers` (`docs/src/content/docs/hosted/overview.md`). Today a script starts and
registers each one (`scripts/run-web-broker.sh`, `scripts/run-schedule-broker.sh`), and
`vam node init` installs none, so a release user would get binaries nothing runs. Phase 3 adds:

- `vam node init --broker web,schedule,notify` (default: none, so a plain install changes nothing
  it does not today): one systemd unit per broker (`vam-broker@<name>.service`, the same user as
  `vamd`, socket mode 0600 under the state directory), started after `vamd`, then registered with
  the broker's manifest. A broker's credentials (for example the Brave key for `vam.web`) go into
  the vault with `vam secrets set`, never into the unit or its environment, as today.
- `vam node status` lists the registered brokers with their versions (each broker reports
  `build_info()` in `Register`).
- `--check` lists a requested broker binary that is missing beside `vam`.

### 5.7 `vam` on the `PATH`, and upgrading it (Phase 5)

Today a user copies `vam` somewhere by hand. Phase 5 adds two CLI commands and two install
scripts:

- **`vam self install`**: copies the running binary to a per-user directory and puts that
  directory on the user's `PATH`, idempotently (a second run prints `unchanged`):

  | OS | Directory | How it reaches `PATH` |
  |---|---|---|
  | Linux | `~/.local/bin` | already on `PATH` on most distributions; otherwise one marked line in `~/.profile` (and `~/.bashrc` / `~/.zshrc` if present) |
  | macOS | `~/.local/bin` | one marked line in `~/.zprofile` (zsh is the default shell) and `~/.bash_profile` if present |
  | Windows | `%LOCALAPPDATA%\Programs\vam\bin` | the user `Path` in `HKCU\Environment`, then a settings-changed broadcast so new terminals see it |

  No `sudo` or administrator rights. `--dir DIR` picks another directory; `vam self uninstall`
  removes the binary and the marked lines. A node install (`sudo ./vam node init`) still runs
  `vamd` from the bundle directory it was extracted to.
- **`install.sh` / `install.ps1`** (release assets): pick the right archive for the OS and CPU,
  download it with `SHA256SUMS`, check the hash, run `vam self install`. One line on the releases
  page: `curl -fsSL https://github.com/squrell-inc/VAM/releases/latest/download/install.sh | sh`
  (and the PowerShell equivalent). The scripts print what they will do and accept
  `--version v0.3.0`.
- **`vam self update`** (`--check` only reports; `--version vX` pins; `--pre` allows an rc):
  reads the latest published release from the GitHub API, downloads the archive for this
  platform and `SHA256SUMS`, checks the hash, verifies the release's GitHub attestation (in
  process with a Sigstore verifier, or by calling `gh attestation verify` when `gh` is present;
  decided at the Phase 5 breakdown, open question 13), then replaces itself (on Windows a running
  `.exe` cannot be overwritten, so it is renamed aside first). It never touches `vamd`.
  It refuses a downgrade without `--version`.
- `vam --version` and `vam node status` mention when a newer release exists only if
  `vam self update --check` was run; the CLI never calls the network unasked.
- **Upgrading a node** (`vamd`, brokers, base image, state migrations, restart) is a different and
  riskier job: out of scope here (§8). Until then the node upgrade is: read the release's upgrade
  notes, extract the new node bundle, and run `sudo ./vam node init` from it. The unit's
  `ExecStart` points at the `vamd` beside the running `vam` (`node_install.rs:139,160`), so the
  re-run points the unit at the new bundle and keeps `/var/lib/vam`. Phase 3 proves that re-run
  from a newer bundle with a scenario.

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
| 0 | Version identity | `vam`, `vamd` and each broker print the same version line; `GET /v1/node` reports `build`; dev builds say `+dev`; `scripts/release/prepare.sh` sets the version; `vam-firecracker` inherits the workspace version; `deny.toml` checks advisories and bans; scenarios green |
| 1 | Release workflow, dry run | `release.yml` validates, builds the Linux, macOS and guest assets, writes `SHA256SUMS`, SBOMs and notes, scans, and uploads workflow artifacts; a dry run on `main` is green; guards refuse each bad input in a test |
| 2 | Publish | `release` environment, immutable releases and the `v*` ruleset set by the owner; publish job creates the draft, attests, verifies and publishes; `v0.1.0-rc.1` published and verified, then `v0.1.0` |
| 3 | Install a node from a release | `vam` builds for Windows; `build-rootfs.sh --runtime`; `vam node init` installs from an extracted x86_64 node bundle without Rust or a checkout, and installs and registers the brokers asked for; docs lead with the release; the next minor ships with these |
| 4 | aarch64 node (Lima on Apple Silicon) | aarch64 standard base recipe and pins, aarch64 Firecracker and kernel pins, `vam node init` accepts aarch64 Linux, `vam-node-…-aarch64` bundle; the Mac page installs the node in Lima from the release with no compile; lane or Lima proof once |
| 5 | `vam` on the `PATH` and `vam self update` | `vam self install` / `uninstall` on Linux, macOS, Windows; `install.sh` / `install.ps1` assets; `vam self update` verifies hash and attestation and replaces itself; scenarios green |
| later | see §8 | — |

Phase 4 depends on Phase 3; Phase 5 depends on Phase 2 only and can run beside 3 and 4.

Scenarios the phases are expected to add (names final at the phase breakdown):

- Phase 0: `cli-prints-its-version-and-commit`, `vamd-prints-its-version-without-starting`,
  `node-status-shows-the-node-and-cli-versions` (and warns on a minor mismatch),
  `dev-build-does-not-claim-a-release`.
- Phase 3: `node-init-from-a-release-bundle-needs-no-checkout` (process layer, `--root` seam,
  `--kit` from a fixture bundle, a fixture `vam-runtime` injected as `/sbin/init`),
  `node-init-installs-and-registers-the-brokers-asked-for`,
  `node-init-from-a-newer-bundle-keeps-the-state`, and a change to
  `node-init-check-names-what-is-missing` if the check lists the recipe or a broker.
- Phase 4: `node-reports-the-standard-base-it-boots` gains an aarch64 lane variant (or a Lima
  proof recorded at the phase close if no arm64 KVM runner is available).
- Phase 5: `self-install-puts-vam-on-the-path-once` (`--dir` and a fake home as the seam),
  `self-update-refuses-a-download-whose-hash-does-not-match`,
  `self-update-replaces-itself-with-the-newer-release` (fixture release server).

## 8. Explicitly later

- A published `rootfs.ext4` with a source archive, after a licence review.
- `cosign` bundle for `SHA256SUMS`; SLSA Build L3 via a reusable workflow; rebuild check.
- `vam node upgrade` (in-place node upgrade with state migration and rollback);
  `vam node init --from-release v0.3.0` (download and verify the bundle itself).
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

1. Decide the number from what merged since the last tag: only fixes → patch; anything else →
   minor (pre-1.0); `-rc.1` first when unsure.
2. `scripts/release/prepare.sh <version>` → PR `release: prepare v<version>` → merge into `main`;
   `rust.yml` on that merge is green.
3. Actions → `release` → Run workflow from `main`: `version`, `commit` (or blank for the head),
   `dry_run: true`. Read the summary: guards, asset table, notes.
4. Run again with `dry_run: false`. Review the pending deployment: asset table, `SHA256SUMS`,
   notes. Approve.
5. On the published release: `gh release verify v<v>`, and download one archive and run
   `gh attestation verify` on it. `vam --version` from it prints `<v> (<sha> <date>)`.
6. Nothing to do on `main` afterwards: there is no post-release PR.
7. If the publish failed after the tag was created: do not delete or reuse it; prepare the next
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
10. **Version bump.** Default: a prepare PR at release time (§3.3); no post-release PR; the
    workflow never pushes to `main`.
11. **Who dispatches.** Default: owner only (environment reviewer); anyone with write access can
    start a dry run.
12. **Brokers installed by default.** Default: none; `--broker` names them. Alternative: all
    three, since the binaries are in the bundle anyway.
13. **How `vam self update` verifies the attestation.** Default: hash check always, plus
    `gh attestation verify` when `gh` is installed, with a warning when it is not; an in-process
    Sigstore verifier (`sigstore` crate, larger binary) is decided at the Phase 5 breakdown.
14. **`PATH` edits on Linux and macOS.** Default: one marked line in the login profile files
    listed in §5.7, never system files; `--no-modify-path` prints the line instead.

## 11. Walkthrough

### 11.1 Developer: merging is not releasing

```text
 feature branch ──waves──► release PR into main ──merge──► main      (Actions is the merge gate)
                                                            │  nothing is published; main can
                                                            │  take many release PRs per version
                                                            ▼
 Owner, when ready:  decide the number ► prepare PR "release: prepare v0.3.0" ► merge
                     Actions ▸ release ▸ Run workflow (version 0.3.0, commit, dry_run=true)
        ▼
   validate   commit is a merge on main · rust.yml green on that SHA · Cargo.toml = 0.3.0
              tag unused · 0.3.0 > every tag · release gate
        ▼
   build      (parallel, read-only, no secrets, no cache) node bundles · vam ×6 platforms ·
              vam-runtime · proto
        ▼
   assemble   SHA256SUMS · SBOMs · notes from the release: merges · secret scans
        ▼
   dry_run=true  ──► stop; artifacts stay on the run for inspection
   dry_run=false ──► waits in environment "release" ──► owner approves
                     draft release + tag v0.3.0 at the commit ► upload ► attest ► verify ► publish
                     the tag can never move or be deleted
        ▼
 main keeps building "0.3.0+dev.N" until the next prepare PR
```

### 11.2 User: run a node, connect from anywhere

```text
 server (x86_64 Linux + KVM; aarch64 from Phase 4)
   download vam-node-<v>-<arch>.tar.gz ► sha256sum -c SHA256SUMS / gh attestation verify ► tar -x
   sudo ./vam node init --check                  # what the host lacks, changes nothing
   sudo ./vam node init --broker web,schedule    # host check · /var/lib/vam · Firecracker,
                                                 # jailer, kernel (pinned) · base image from the
                                                 # recipe + vam-runtime · vamd.env · systemd units
                                                 # (vamd, brokers) · first admin key, shown once
 laptop (Linux, macOS, Windows)
   install.sh / install.ps1  (or download + vam self install)   # vam on PATH
   vam config set-host https://my-node:<port> · vam login
   vam node status     # node vamd 0.3.0 (3f9c2ab) release · brokers · cli vam 0.3.0
   vam run …           # everything goes over the node's HTTP API
   vam self update     # later releases of the CLI; the node is upgraded from its bundle
```

### 11.3 Platforms

```text
                       where vamd runs                                     vam CLI
 Linux x86_64 server   native, x86_64 node bundle                (P3)      native
 Linux arm64 server    native, aarch64 node bundle               (P4)      native
 Windows 11 + WSL2     WSL Ubuntu (x86_64) with nested KVM:      (P3)      vam.exe (P3) or the
                       the same x86_64 node bundle                         Linux vam inside WSL
 Mac, M3 or newer      Lima VM (aarch64 Linux, nested KVM):      (P4)      macOS arm64 vam
                       aarch64 node bundle inside Lima
 Mac, M1/M2 or Intel   no node locally (no nested virtualization           macOS arm64 / x86_64
                       for Lima); use a remote Linux node                  vam, pointed at it
```

- **Windows + WSL2:** inside the WSL Ubuntu distribution, extract the x86_64 node bundle and run
  `sudo ./vam node init` (`/dev/kvm` comes from WSL2 nested virtualization;
  `getting-started/windows-wsl.md`). On Windows, `vam.exe` points at `https://localhost:<port>`
  (WSL2 forwards localhost), or the Linux `vam` is used inside WSL. `vam node init` on Windows
  says it is Linux only.
- **Mac + Lima** (Apple Silicon M3 or newer, `getting-started/macos-lima.md`): `limactl start` with `scripts/lima-macos.yaml`; inside the VM, extract the
  aarch64 node bundle and run `sudo ./vam node init`; on the Mac, the macOS `vam` points at the
  forwarded port. Until Phase 4 the Mac path stays as today (`start-vamd-lima.sh` compiles `vamd`
  in Lima with the minimal image).
