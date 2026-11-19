# Release artifacts: version, tag, build and publish VAM

Status: **plan of record, approved by the owner 2026-11-17; restructured by the owner
2026-11-18** (Phases A, B and Final; Lima and Windows in `v0.2.0`). Nothing here is built
yet. Tracker: `.cursor/workstreams/release-artifacts.md`. Base branch: `main` (cross-cutting;
Phase A lands from `feat/release-artifacts-a`, Phase B from `feat/release-artifacts-b`, one
release PR each).

Read only the section for the phase in hand.

---

## 1. Goal

A person who wants VAM downloads a published release, verifies it, and runs it without a Rust
toolchain. Each release is one version for the whole machine (`vamd`, `vam`, the brokers, the
guest init), built by one workflow from one commit on `main`, and published under a tag that can
never move. The owner starts every release by hand and approves it before anything is public. A
user installs a node from the x86_64 node bundle (Linux server or WSL2) without Rust or a
checkout (Phase A), puts `vam` on their `PATH` on Linux or macOS with one command and later
upgrades it with another (Phase B). §11 walks through the whole thing from the developer's side
and the user's side, per platform.

Out of scope for the workstream: publishing crates to crates.io, package managers (Homebrew,
winget, apt), upgrading a running node in place (`vam node upgrade`), hosting the docs site, a
container image for `vamd` (see §5.2), and versioning catalog items (agents, skills, playbooks),
which have their own semver (`docs/src/content/docs/orchestrator/catalog.md`).

Out of scope for `v0.1.0` (owner, 2026-11-18), planned for `v0.2.0` (§7): the aarch64
node bundle and the Lima page, and native Windows (`vam.exe`, `install.ps1`, `vam dock` on
Windows). For `v0.1.0`, Windows users run the Linux `vam` and the x86_64 node bundle inside
WSL2, macOS gets the CLI pointed at a remote Linux node, and Lima stays build-from-source as
today.

## 2. What exists today (evidence)

| Fact | Where |
|---|---|
| One workspace version `0.1.0`; every crate inherits it except `vam-firecracker` (own `version = "0.1.0"`) | `Cargo.toml:33`, `crates/vam-firecracker/Cargo.toml:3` |
| No git tags, no release workflow, no `CHANGELOG`. Releases are PRs from a feature branch into `main`, merged with a merge commit and titled `release: <epic> <phases> (#N)` | `git tag` (empty); `git log --merges origin/main`; `phase-playbook.md` §8; `working-agreement.md` "Standing approvals" |
| `vam --version` prints `CARGO_PKG_VERSION (VAM_GIT_SHA)`; `VAM_GIT_SHA` comes from `git rev-parse --short HEAD` (+`+dirty`) and is `unknown` outside a checkout, with no environment override | `crates/vam-cli/src/main.rs:9`, `crates/vam-cli/build.rs` |
| `vamd` has no `--version`; `/v1/health` returns only a serving status; `GET /v1/node` has no version | `crates/vam-orchestrator/src/bin/vamd.rs`; `crates/vam-orchestrator/src/api/mod.rs:121,162` |
| Version strings elsewhere: `vam_core::VERSION`, the marketplace `node_version`, MCP `clientInfo`, OCSF (all `CARGO_PKG_VERSION`); `broker_version` is empty in production (`vamd` sends `String::new()` in `Register`, `api/mod.rs:1902`; the `"0.1.0"` at `hsp.rs:489` is a test) | `crates/vam-core/src/lib.rs:32`, `crates/vam-orchestrator/src/marketplace/mod.rs:106`, `mcp_client.rs:372`, `crates/vam-telemetry/src/ocsf.rs:30`, `crates/vam-orchestrator/src/hsp.rs:489` |
| Binaries: `vam`, `vamd`, `hsp-echo-broker` (test broker), `vam-runtime`, `vam-web-broker`, `vam-schedule-broker`, `vam-notify-broker`, `vam-e2e`, `verify_chain` | `[[bin]]` sections of each crate |
| Every binary crate depends on `vam-spec`, which already has a `build.rs` (proto compile, descriptor set to `$OUT_DIR/vam_descriptor.bin`) | `crates/vam-spec/build.rs:40-45`; `crates/vam-cli/Cargo.toml:35`, `brokers/*/Cargo.toml` |
| CI: `rust.yml` (fmt, clippy pinned to 1.97.0, workspace tests, smoke scenarios, microVM lane, release gate, `release lane (microVM)` required on release PRs), `docs.yml`, `dco.yml`, `bench.yml`. All `ubuntu-latest`, `permissions: contents: read`, no `environment:`, actions pinned by tag, not SHA | `.github/workflows/rust.yml:578`; `scripts/e2e/check-release-gate.sh` |
| `rust.yml` `checks` runs `check-release-gate.sh` on every push to `main` and on every PR into `main`; push runs always run the microVM lane | `.github/workflows/rust.yml:121-123`, `rust.yml:69-71` |
| `rust.yml` cancels an older run of the same ref, `main` included (`cancel-in-progress: true`) | `.github/workflows/rust.yml:38-40` |
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
`v0.2.0` whether `v0.2.0` is `0.2.1` or `0.3.0`; that is decided by what merged, at the
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
- **Build info is read from git only in the binary crates' build scripts** (changed
  2026-11-18, owner): `vam-cli`, `vam-orchestrator` (for `vamd`) and the three brokers. Each
  `build.rs` includes one small std-only file that runs `git describe` and `git rev-parse` and
  exports `VAM_VERSION`, `VAM_GIT_SHA`, `VAM_COMMIT_DATE`, `VAM_BUILD_CHANNEL`. `vam-spec`
  holds only the pure part: a `BuildInfo` type built from those values as arguments, its
  version rules and its printed line, and a `build_info!()` macro that reads the `env!` values in
  the calling crate. A git-reading `vam-spec/build.rs` was rejected: every crate depends on
  `vam-spec`, so every commit would rebuild most of the workspace. In `vam-orchestrator` a build
  script reaches the library too, so a commit rebuilds that crate (a leaf: nothing depends on
  it), not `vam-spec` or the crates under it. Environment overrides `VAM_GIT_SHA` and
  `VAM_COMMIT_DATE` make a tarball build honest. The commit date (not the build time) keeps the
  string reproducible (`SOURCE_DATE_EPOCH`).
- Reported by: `vam --version` (the CLI's own version), `vamd --version` (new; the daemon's
  version, prints and exits without reading config), `--version` on each broker,
  `GET /v1/node` (new `build: { version, commit, channel }`, read by any signed-in user, filled
  from the `BuildInfo` that `vamd`'s `main` hands to the daemon), and `vam node status`, which
  prints the node's version beside the CLI's and warns when their minor versions differ. The
  marketplace `node_version` and the MCP `clientInfo` take the daemon's `BuildInfo`;
  `vam_core::VERSION` and the OCSF product version stay `CARGO_PKG_VERSION` (library crates read
  no git). The broker's own version back to `vamd` needs a new `RegisterResponse` field: later
  (§8).

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
validate (read-only: guards, cargo deny advisories) ─────────────────────────────┐
build matrix ──► assemble (SHA256SUMS, notes, scans)                             │
  (reusable workflow release-build.yml: read-only, no secrets, no caches)        │
                                                         │ dry_run=true: stop here
                                                         ▼                       ▼
                         publish (needs validate and assemble; environment: release; owner approves)
                           ► draft release targeting <commit> ► upload assets ► attest ► verify assets
                           ► publish (GitHub creates the tag v<version> now) ► gh release verify
```

- **Builds run beside `validate`** (changed 2026-11-18, owner): they are read-only, hold no
  secrets and publish nothing, so they need not wait; the publish job `needs` both.
- **The build and assemble jobs live in a reusable workflow**, `.github/workflows/release-build.yml`
  (`on: workflow_call`), called by `release.yml`. The same file also runs on `pull_request` when
  release paths change (`scripts/release/**`, `.github/workflows/release*.yml`,
  `scripts/build-rootfs.sh`, `scripts/base/**`, `Cargo.lock`) and on a weekly `schedule`, build
  only: no publish job, no secrets, `channel: dev`. So a change to the build is proven on its own
  PR, and drift (a new toolchain, a dependency) shows up within a week, without branch dry runs.
- **No dry run before every publish** (changed 2026-11-18, owner). The publish run stops at the
  environment approval with the same summary a dry run gives (guards, asset table, `SHA256SUMS`,
  notes), so the owner reads it there. `dry_run: true` stays available, but it is not a checklist
  step.

Rejected: **tag-on-merge** (tag every `release:` merge into `main`, or push a tag to trigger the
build). It makes merging and publishing the same act: hotfix and design-cleanup releases
(`release: design/ cleanup (#7)`) would ship binaries; a pushed tag exists before anything is
built or checked, so a failed build leaves a tag behind; and the tag would be created by a person,
not the gated workflow. Manual dispatch keeps the marketplace's shape: nothing public until the
owner approves the exact artifacts.

### 4.2 Guards (all in `validate`, each failing with the reason; changed 2026-11-18, owner)

1. The actor dispatched from `main` (`github.ref == refs/heads/main`), so the workflow file that
   runs is the reviewed one.
2. `commit` is on `main`'s first-parent history (`git merge-base --is-ancestor` and present in
   `git rev-list --first-parent origin/main`), so it is a merge into `main`, not a commit from
   inside a feature branch.
3. CI is green on that exact SHA: the `rust.yml` run for the `push` to `main` at `commit`, or a
   `workflow_dispatch` run of `rust.yml` on `main` at that SHA, concluded `success` (Actions
   API, `head_sha` filter). That run includes the release gate (`rust.yml:121-123`) and the
   microVM lane. If it is missing, cancelled or red, the release fails and names the run; the
   owner re-runs `rust.yml` for that SHA, never the release.
4. `Cargo.toml` at `commit` has `version == inputs.version` exactly (the prepare PR, §3.3, is
   merged at or before `commit`).
5. Tag `v<version>` does not exist locally or on the remote, and no release (draft included)
   uses it. An immutable release burns its tag name for good (§4.3), so a failed publish means the
   next number.
6. `version` is greater, by semver, than every existing `v*` tag (a final may follow its own rc).
7. No known advisory at `commit`: `cargo deny check advisories` (fast, compiles nothing). New
   advisories appear after the push run, so this one runs again at release time. The release
   gate itself is not re-run (it was guard 7 until 2026-11-18): guard 3 already requires the
   push run that ran it.
8. Inputs reach shell only through `env:`, never `${{ inputs.* }}` inside `run:`.

### 4.3 Tag protection

- Repository setting **immutable releases** on (GA since 2025-10-28): once published, a release's
  tag cannot move or be deleted, its assets cannot change, and GitHub signs a release attestation.
- A repository **ruleset on `refs/tags/v*`**: restrict updates, restrict deletions, block force
  pushes; **no bypass actors, admins included**. This covers the window before publish, and tags
  without a release.
- Creation (changed 2026-11-18, owner): the publish job creates a **draft release targeting the
  commit** (`gh release create v<version> --target <commit> --draft`) with `GITHUB_TOKEN`
  (`contents: write`). The tag is expected to be created by GitHub only when the draft is
  published; A.W6 confirms this on GitHub's documentation and the rc.1 run shows it. So a
  failure before publish leaves a draft and no tag: the job deletes its draft, and the same
  number can be dispatched again after the fix. A failure after publish burns the number
  (decision 6). Right before publishing, the job checks again that no `v<version>` tag exists
  (a stray push would otherwise carry the release).
- Restricting creation: whether a "restrict creations" rule can let only the workflow create
  `v*` tags is unproven (community reports say the GitHub Actions app cannot be a bypass
  actor). There is no scratch-repository rehearsal before `v0.1.0` (removed 2026-11-18, owner);
  the default is to leave creation unrestricted and accept that a maintainer with write access
  could push a stray `v*` tag, which guard 5 and the check before publish refuse, and the
  immutable-release check below exposes. Testing the rule is later (§8).
- A node or user trusts a tag only if it has a **published immutable release** whose attestations
  verify (`gh release verify v<version>`); a bare tag means nothing.

## 5. Artifacts and targets

### 5.1 Matrix (native GitHub-hosted runners)

Shipped in `v0.1.0` (decision 11, changed 2026-11-18, owner):

| Asset | Contents | Target | Runner | Phase |
|---|---|---|---|---|
| `vam-node-<v>-x86_64-unknown-linux-gnu.tar.gz` | `vamd`, `vam`, `vam-web-broker`, `vam-schedule-broker`, `vam-notify-broker`, `vam-runtime` (guest init), `share/vam/base/` (the standard base recipe, §5.3), `scripts/assets.pins`, `LICENSE-*`, `NOTICE`, `README` | x86_64 gnu, glibc ≥ 2.34 | `ubuntu-24.04` | A |
| `vam-<v>-x86_64-unknown-linux-gnu.tar.gz` | `vam` | x86_64 gnu | `ubuntu-24.04` | A |
| `vam-<v>-aarch64-unknown-linux-gnu.tar.gz` | `vam` | aarch64 gnu | `ubuntu-24.04-arm` | A |
| `vam-<v>-aarch64-apple-darwin.tar.gz` | `vam` | macOS arm64 | `macos-15` | A |
| `vam-<v>-x86_64-apple-darwin.tar.gz` | `vam` | macOS x86_64 | `macos-15-intel` (last Intel image; retires 2027) | A |
| `vam-runtime-<v>-x86_64-unknown-linux-musl` | the guest init alone | static musl | `ubuntu-24.04` | A |
| `SHA256SUMS` | one line per asset | — | assemble | A |
| `RELEASE_NOTES.md` | the notes (§3.4) | — | assemble | A |
| `install.sh` | downloads the right `vam` for the machine, checks it against `SHA256SUMS`, runs `vam dock` | — | assemble | B |

`v0.2.0` (Lima and Windows, §7) adds:

| Asset | Contents | Target | Runner |
|---|---|---|---|
| `vam-node-<v>-aarch64-unknown-linux-gnu.tar.gz` | the node set for arm64 Linux, incl. an aarch64 `vam-runtime` (Lima on Apple Silicon, arm64 servers) | aarch64 gnu | `ubuntu-24.04-arm` |
| `vam-runtime-<v>-aarch64-unknown-linux-musl` | the aarch64 guest init alone | static musl | `ubuntu-24.04-arm` |
| `vam-<v>-x86_64-pc-windows-msvc.zip` | `vam.exe` | Windows x86_64 | `windows-2025` (after the CLI compiles there) |
| `install.ps1` | the PowerShell `install.sh` | — | assemble |

Not shipped (changed 2026-11-18, owner): the proto descriptor archive (`vam.binpb` and the
`.proto` files; no consumer), and the CycloneDX `*.cdx.json` files (later, §5.4).

- **`vamd` and the brokers: x86_64 Linux in `v0.1.0`, aarch64 Linux in `v0.2.0`.** Today
  nodes are x86_64 only (`node_init.rs:228`) and the pinned Firecracker and kernel are x86_64
  (`assets.pins`). The Mac path is a Lima VM, which on Apple Silicon is aarch64 Linux, so the next
  release adds an aarch64 node bundle with an aarch64 standard base (the Debian pin is already a
  multi-arch index digest, `scripts/base/pins:8-11`; Bun needs an aarch64 pin, and the
  Dockerfile's hard-coded x64 zip becomes per-architecture), aarch64 Firecracker and kernel pins,
  and `check_platform` accepting aarch64 Linux.
- **gnu, not musl, for host binaries**, built for glibc 2.34 (the oldest documented host,
  Amazon Linux 2023; Ubuntu 22.04 has 2.35) with `cargo-zigbuild --target
  x86_64-unknown-linux-gnu.2.34`, wrapped by `cargo auditable`. Each build job checks the
  highest `GLIBC_` symbol version with `objdump -T`. A.W4 starts with a short local spike of
  zigbuild with `aws-lc-sys` and bundled SQLite under `cargo auditable`; if it fails, the
  fallback is to build inside an AlmaLinux 9 container (glibc 2.34) pinned by digest on the same
  runner. musl for a daemon brings a slower allocator and a musl C toolchain for aws-lc, ring and
  SQLite, for no gain on a host that already runs systemd and Docker.
- **Guest init**: built the way `build-rootfs.sh` builds it today (same flags), from one shared
  script, so the released `vam-runtime` is the one the lane proved. Moving both to the
  `release-guest` profile is a separate change proven on the lane (open question 9).
- **Native runners for every target.** The tree compiles C (aws-lc, ring, SQLite) on every
  target, which is where `cross` (qemu) and macOS-from-Linux (Apple SDK) hurt. Standard runners,
  macOS and arm64 included, are free for this public repository. `cargo-zigbuild` (or the
  AlmaLinux 9 fallback) is used only on Linux for the glibc floor.
- **Rejected: `cargo-dist`.** It generates a tag-push-driven `release.yml` with mutable action
  tags; fitting it to an environment-gated dispatch with immutable tags means patching generated
  YAML on every upgrade, and its 2026 maintenance could not be confirmed. A hand-written workflow
  of about 250 lines is easier to audit.

### 5.2 What is not published

- **No `rootfs.ext4`** in this workstream. It redistributes GPL and LGPL binaries (git, busybox,
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
  `SHA256SUMS`. Free for public repositories, signed through
  Sigstore public-good, verified with `gh attestation verify <file> -R squrell-inc/VAM
  --signer-workflow squrell-inc/VAM/.github/workflows/release.yml`, and offline with
  `gh attestation download` plus `gh attestation trusted-root`. Provenance from a hosted runner is
  SLSA Build L2. The builds already sit in a reusable workflow (§4.1), but the attestation is
  signed by `release.yml`'s publish job; L3 is later.
- **Immutable release attestation**: GitHub signs the release itself
  (`gh release verify v<v>`, `gh release verify-asset v<v> <file>`).
- **No long-lived signing key.** Keyless signing has no key to protect or rotate; the public
  repository means the Sigstore identity (repo, workflow, ref) in the public log reveals nothing.
  A `cosign` bundle for `SHA256SUMS` (for people without `gh`) is a later addition.
- **SBOM** (software bill of materials): the parts list of a binary, every crate and its
  version, licence and source hash, so a question such as "does `v0.3.0` contain the vulnerable
  version of crate X?" is answered by reading the binary instead of rebuilding. In `v0.1.0`,
  binaries are built with `cargo auditable` and carry the list inside the binary (a few KB, read
  by `cargo audit bin`, Grype, Trivy, so a binary found on a server can be scanned on its own).
  A separate CycloneDX JSON file per archive and its SBOM attestation are deferred (changed
  2026-11-18, owner; §8): nothing reads them yet, and the embedded list answers the same
  question.
- **Release gate additions**: `deny.toml` gains `[advisories]` and `[bans]`; `rust.yml`'s release
  gate runs `cargo deny check` (advisories, bans, licenses, sources) on every push to `main`; the
  release runs `cargo deny check advisories` again in `validate` (guard 7) and `cargo audit bin`
  on the built binaries.

### 5.5 Install and docs

A new page `getting-started/releases.md` (sidebar under Getting started): which archive for which
machine, download, verify (`sha256sum -c SHA256SUMS --ignore-missing`, `gh attestation verify`),
install (`vam` on `PATH`; on a node, extract the node bundle and run `sudo ./vam node init`). The
pages that today say "build from source" (`installation.md`, `windows-wsl.md` (the Linux assets
inside WSL2), `cli.md`, `project-status.md`) lead with the release and keep the source build as
the second path (Phase A). `macos-lima.md` gets the macOS CLI from the release and keeps the node
build-from-source until `v0.2.0`.

### 5.6 Brokers on a node

Every first-party broker ships as its own binary in the node bundle: `vam-web-broker`,
`vam-schedule-broker`, `vam-notify-broker`. `hsp-echo-broker` (a test fixture in
`vam-orchestrator`), `vam-e2e` and `verify_chain` are not shipped.

A broker is a separate process that `vamd` dials on a Unix socket, registered with
`PUT /v1/hosted/brokers` (`docs/src/content/docs/hosted/overview.md`). Today a script starts and
registers each one (`scripts/run-web-broker.sh`, `scripts/run-schedule-broker.sh`), and
`vam node init` installs none, so a release user would get binaries nothing runs. Phase B adds:

- `vam node init --broker web,schedule,notify` (default: none, so a plain install changes nothing
  it does not today): one systemd unit per broker (`vam-broker@<name>.service`, the same user as
  `vamd`, socket mode 0600 under the state directory), started after `vamd`, then registered
  **through the API** (`PUT /v1/hosted/brokers`) with the broker's manifest (changed 2026-11-18,
  owner). On the first run `vam node init` holds the admin key it just minted and uses it; on a
  re-run it uses `VAM_API_KEY` (or the signed-in `vam` config) when given. A re-run without a key
  writes the units and prints the exact command that registers each broker. No new offline
  registration path (`vamd` gains no one-shot flag for it). A broker's credentials (for example
  the Brave key for `vam.web`) go into the vault with `vam secrets set`, never into the unit or
  its environment, as today.
- `--check` lists a requested broker binary that is missing beside `vam`.
- Later (§8): the broker's own version back to `vamd` (a new `RegisterResponse` field, since
  `Register` goes from `vamd` to the broker) and `vam node status` listing brokers with versions.

### 5.7 `vam` on the `PATH`, and upgrading it (Phase B; Windows in `v0.2.0`)

Names: `vam dock` (the CLI docks on this machine: on the `PATH`, ready), `vam undock` (it leaves,
nothing behind), `vam upgrade` (pairs with the later `vam node upgrade`). Owner, 2026-11-17.

Today a user copies `vam` somewhere by hand. Phase B adds the CLI commands and `install.sh` for
Linux and macOS; the Windows parts (marked below) come in `v0.2.0` (owner, 2026-11-18):

- **`vam dock`**: copies the running binary to a per-user directory and puts that
  directory on the user's `PATH`, idempotently (a second run prints `unchanged`):

  | OS | Directory | How it reaches `PATH` |
  |---|---|---|
  | Linux | `~/.local/bin` | already on `PATH` on most distributions; otherwise one marked line in `~/.profile` (and `~/.bashrc` / `~/.zshrc` if present) |
  | macOS | `~/.local/bin` | one marked line in `~/.zprofile` (zsh is the default shell) and `~/.bash_profile` if present |
  | Windows (`v0.2.0`) | `%LOCALAPPDATA%\Programs\vam\bin` | the user `Path` in `HKCU\Environment`, then a settings-changed broadcast so new terminals see it |

  No `sudo` or administrator rights. `--dir DIR` picks another directory; `vam undock`
  removes the binary and the marked lines. A node install (`sudo ./vam node init`) still runs
  `vamd` from the bundle directory it was extracted to.
- **`install.sh`** (release asset; `install.ps1` in `v0.2.0`): pick the right archive
  for the OS and CPU, download it with `SHA256SUMS`, check the hash, run `vam dock`. One line on the releases
  page: `curl -fsSL https://github.com/squrell-inc/VAM/releases/latest/download/install.sh | sh`
  once a final exists. GitHub's `releases/latest` ignores pre-releases, so during the rc period the
  page names the rc's own URL and `vam upgrade` needs `--pre`. The script prints what it will do
  and accepts `--version v0.3.0`.
- **`vam upgrade`** (`--check` only reports; `--version vX` pins; `--pre` allows an rc):
  reads the latest published release from the GitHub API, downloads the archive for this
  platform and `SHA256SUMS`, checks the hash, verifies the release's GitHub attestation (in
  process with a Sigstore verifier, or by calling `gh attestation verify` when `gh` is present;
  decided: `gh` when present, open question 13), then replaces itself (`v0.2.0`: on Windows a
  running `.exe` cannot be overwritten, so it is renamed aside first). It never touches `vamd`.
  It refuses a downgrade without `--version`.
- `vam --version` and `vam node status` mention when a newer release exists only if
  `vam upgrade --check` was run; the CLI never calls the network unasked.
- **Upgrading a node** (`vamd`, brokers, base image, state migrations, restart) is a different and
  riskier job: out of scope here (§8). Until then the node upgrade is: read the release's upgrade
  notes, extract the new node bundle, and run `sudo ./vam node init` from it. The unit's
  `ExecStart` points at the `vamd` beside the running `vam` (`node_install.rs:139,160`), so the
  re-run points the unit at the new bundle and keeps `/var/lib/vam`. Phase A proves that re-run
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
(owner, 2026-11-17), so Actions minutes are not a constraint. A `v0.1.0` release is about
55 runner-minutes of cold builds spread over five parallel jobs (the x86_64 node bundle and four
CLIs), about **20–25 minutes wall time** (estimate, changed 2026-11-18): `validate` takes a few
minutes and runs beside the builds, since it no longer re-runs the release gate (guard 7); the
longest leg is the node bundle with zigbuild (about 12–15 minutes cold plus tool installs);
assemble and publish add about 5–8 minutes. The owner's approval wait is extra. Only a larger or
self-hosted runner would cost money; this plan uses none. For reference, the same release on
private-repository pricing would be about US$1.20 (the earlier US$1.35 included the Windows
CLI).

### 6.4 Keeping secrets out

- Build jobs: `permissions: contents: read`, no secrets in the environment, `persist-credentials:
  false`. The only token in the publish job is `GITHUB_TOKEN` with `contents: write`,
  `id-token: write`, `attestations: write`. No repository or environment secret is needed.
- `assemble` runs `gitleaks dir` with the tree's `.gitleaks.toml` over the extracted archives,
  greps binaries for `vam_sk_`, `/home/runner` and other runner paths (after the path remap), and
  refuses any `.env`, `*.ext4`, `vmlinux*` or log file, the same list `export-public.sh` refuses.
- All third-party actions pinned by full commit SHA in `release.yml` and `release-build.yml`
  (and Dependabot for action bumps). harden-runner is optional and later.

## 7. Phases

Restructured 2026-11-18 (owner, after an independent review found seven phases over-split: rc.1
was not installable, guard 7 repeated the push run, dry runs duplicated the approval gate, and
each phase carried its own release overhead). Each phase is one release PR into `main`, planned
and built through the phased-workstream procedure. Workflow work is proven by its own checks,
build-only PR runs of `release-build.yml` and the recorded publish, plus product scenarios where
the behaviour is visible through the CLI or API.

| Phase | Name | Exit |
|---|---|---|
| A | rc.1: a node and a CLI from a release, no Rust | `vam`, `vamd` and each broker print the same version line; `GET /v1/node` reports `build`; dev builds say `+dev`; `prepare.sh`; `deny.toml` advisories and bans; guards and `release.yml` with the reusable build workflow; `build-rootfs.sh --runtime`; the kit in the x86_64 node bundle; `vam node init --kit`; publish job; `v0.1.0-rc.1` published and verified; a node installs from it on a host with Docker and no Rust or checkout; docs lead with the release |
| B | rc.2: `vam dock`, `vam upgrade`, install script, brokers | `vam dock` / `vam undock` on Linux and macOS; `install.sh` asset; `vam upgrade` verifies hash and attestation and replaces itself; `vam node init --broker` installs and registers brokers through the API; `v0.1.0-rc.2` published and verified |
| Final | `v0.1.0` | the owner says the rcs have settled; prepare PR `0.1.0`, dispatch, publish, verify; docs PR; `vam upgrade` from rc.2 (or the last fix rc) to `v0.1.0` recorded, the first upgrade between two published releases that hold `vam upgrade` |
| `v0.2.0` | Lima and Windows (after `v0.1.0`) | aarch64 node bundle and pins, the Lima page; `vam.exe`, `install.ps1`, `vam dock` on Windows. A minor, because it adds features (decision 2: a patch only fixes) |
| later | see §8 | — |

Phase B depends on Phase A only for its release (its PR opens after rc.1) and for files it shares
with A waves (tracker "Execution plan"); it can start the first day in a separate session.

**Release candidates per milestone** (changed 2026-11-18, owner): `v0.1.0-rc.1` after Phase A,
`v0.1.0-rc.2` after Phase B, `v0.1.0` after the owner says the rcs have settled. Each rc is
immutable like a final: a broken rc is followed by the next rc (a fix rc.3 shifts nothing else).

Scenarios the phases are expected to add (names final at the phase breakdown):

- Phase A: `cli-prints-its-version-and-commit`, `vamd-prints-its-version-without-starting`,
  `node-status-shows-the-node-and-cli-versions` (and warns on a minor mismatch; the dev-build
  checks fold into the first and the third), `node-init-from-a-release-bundle-needs-no-checkout`
  (process layer, `--root` seam, `--kit` from a fixture bundle, a fixture `vam-runtime` injected
  as `/sbin/init`), `node-init-from-a-newer-bundle-keeps-the-state`.
- Phase B: `dock-puts-vam-on-the-path-once` (`--dir` and a fake home as the seam),
  `upgrade-refuses-a-download-whose-hash-does-not-match`,
  `upgrade-replaces-vam-with-the-newer-release` (fixture release server),
  `node-init-installs-and-registers-the-brokers-asked-for`, and a change to
  `node-init-check-names-what-is-missing` (it names a requested broker binary).
- `v0.2.0`: `node-reports-the-standard-base-it-boots` gains an aarch64 lane variant (or a
  Lima proof recorded at its close if no arm64 KVM runner is available).

## 8. Explicitly later

- Lima and Windows: `v0.2.0` (§7, tracker "Next release: v0.2.0 (Lima and Windows)").
- A published `rootfs.ext4` with a source archive, after a licence review.
- CycloneDX SBOM files per archive and their SBOM attestation (deferred 2026-11-18, owner).
- A proto descriptor asset (`vam.binpb` and the `.proto` files), if a consumer appears.
- The broker's version back to `vamd` (a `RegisterResponse` field) and `vam node status` listing
  brokers with versions.
- Restricting `v*` tag creation to the workflow (test whether the token can create under the
  rule).
- `cosign` bundle for `SHA256SUMS`; SLSA Build L3 (signing inside the reusable workflow); rebuild
  check.
- `vam node upgrade` (in-place node upgrade with state migration and rollback);
  `vam node init --from-release v0.3.0` (download and verify the bundle itself).
- Package managers (Homebrew tap, winget, apt repository).
- Release branches for patching an older minor.

## 9. Owner checklist

### One-time setup (before Phase A's first publish)

1. Settings → Environments → new `release`: required reviewer = owner, "prevent self-review"
   off (the owner both dispatches and approves), deployment branches = `main` only,
   "allow administrators to bypass" **off**. (Changed 2026-11-19 by the rc.1 run: the owner turned
   it on while they are the only contributor; turn it off when a second maintainer joins.)
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
3. Actions → `release` → Run workflow from `main` once: `version`, `commit` = the prepare
   PR's merge SHA (changed 2026-11-19: a blank `commit` takes `main`'s head, and for rc.1 that was
   a later hotfix merge, so the tag is not on the prepare merge), `dry_run: false` (changed
   2026-11-18: no dry run first; `dry_run: true` stays available but is not a step).
4. The run stops at the `release` environment. Read the summary on the pending deployment:
   guards, asset table, `SHA256SUMS`, notes. Approve (or reject: nothing is public, no tag).
5. On the published release: `gh release verify v<v>`, and download one archive and run
   `gh attestation verify` on it. `vam --version` from it prints `<v> (<sha> <date>)`.
6. Nothing to do on `main` afterwards: there is no post-release PR.
7. If the run failed before publish: there is no tag and the job deleted its draft; fix, then
   dispatch the same number again. (Corrected 2026-11-19 by the rc.1 run: if it failed before the
   draft's id was recorded, the cleanup cannot find the draft. Delete any draft for that tag by
   hand first, since `publish.sh check` refuses a tag that any release, draft or not, already
   uses.) If it failed after publish: do not delete or reuse the tag;
   prepare the next patch (or `-rc.N+1`).

## 10. Open questions (each with a default)

1. **First release number.** Decided (owner, 2026-11-17; changed 2026-11-18): rc per milestone:
   `v0.1.0-rc.1` after Phase A, `v0.1.0-rc.2` after Phase B, `v0.1.0` after the owner says the
   rcs have settled.
2. **Pre-1.0 policy.** Default: minor may break; every release note has "Upgrade notes".
3. **Restrict `v*` tag creation.** Default: no creation restriction; updates and deletions
   restricted with no bypass. No scratch-repository test before `v0.1.0` (removed 2026-11-18);
   testing the rule is later (§8).
4. **glibc floor.** Default: 2.34 (Amazon Linux 2023) via `cargo-zigbuild`; fallback an
   AlmaLinux 9 container pinned by digest if the A.W4 spike fails. Alternative: fully static
   musl host binaries.
5. **macOS Intel CLI.** Default: build it while `macos-15-intel` exists (free), drop it then.
6. **Windows CLI.** Decided (owner, 2026-11-18): native `vam.exe` in `v0.2.0`, after
   gating the unix-only `node init` modules; for `v0.1.0`, Windows users run the Linux `vam` and
   the node inside WSL2.
7. **CI-green guard.** Default: require the `rust.yml` push run (or a `workflow_dispatch` run on
   `main`) on the exact SHA; the release does not re-run the workspace suite or the release gate,
   only `cargo deny check advisories`.
8. **Changelog file.** Default: none; GitHub release notes generated from `release:` merges.
9. **Guest init profile.** Default: release builds the guest exactly as `build-rootfs.sh` does;
   moving both to `release-guest` is a separate lane-proven change.
10. **Version bump.** Default: a prepare PR at release time (§3.3); no post-release PR; the
    workflow never pushes to `main`.
11. **Who dispatches.** Default: owner only (environment reviewer); anyone with write access can
    start a dry run, which is optional.
12. **Brokers installed by default.** Default: none; `--broker` names them. Alternative: all
    three, since the binaries are in the bundle anyway.
13. **How `vam upgrade` verifies the attestation.** Default: hash check always, plus
    `gh attestation verify` when `gh` is installed, with a line saying how to check when it is
    not; no in-process Sigstore verifier in `v0.1.0`.
14. **`PATH` edits on Linux and macOS.** Default: one marked line in the login profile files
    listed in §5.7, never system files; `--no-modify-path` prints the line instead.
15. **Where build info is read.** Resolved (owner, 2026-11-18): in the binary crates' build
    scripts only, through a shared helper in `vam-spec` that takes the values as arguments
    (§3.3); not a git-reading `vam-spec/build.rs`.

## 11. Walkthrough

### 11.1 Developer: merging is not releasing

```text
 feature branch ──waves──► release PR into main ──merge──► main      (Actions is the merge gate)
                                                            │  nothing is published; main can
                                                            │  take many release PRs per version
                                                            ▼
 Owner, when ready:  decide the number ► prepare PR "release: prepare v0.3.0" ► merge
                     Actions ▸ release ▸ Run workflow once (version 0.3.0, commit, dry_run=false)
        ▼
   validate   commit is a merge on main · rust.yml green on that SHA (release gate and lane
   (beside    included) · Cargo.toml = 0.3.0 · tag unused · 0.3.0 > every tag ·
   build)     cargo deny check advisories
   build      (reusable release-build.yml; parallel, read-only, no secrets, no cache)
              x86_64 node bundle with the kit · vam ×4 platforms · vam-runtime
        ▼
   assemble   SHA256SUMS · notes from the release: merges · secret scans
        ▼
   publish    waits in environment "release": the owner reads the summary and approves
              draft release targeting the commit ► upload ► attest ► verify assets
              ► publish (the tag v0.3.0 is created now) ► gh release verify
              the tag can never move or be deleted
   (dry_run=true stops before publish; optional, not a checklist step)
        ▼
 main keeps building "0.3.0+dev.N" until the next prepare PR
```

### 11.2 User: run a node, connect from anywhere

```text
 server (x86_64 Linux + KVM, or WSL2; aarch64 in v0.2.0)
   download vam-node-<v>-<arch>.tar.gz ► sha256sum -c SHA256SUMS / gh attestation verify ► tar -x
   sudo ./vam node init --check                  # what the host lacks, changes nothing
   sudo ./vam node init --broker web,schedule    # host check · /var/lib/vam · Firecracker,
                                                 # jailer, kernel (pinned) · base image from the
                                                 # recipe + vam-runtime · vamd.env · systemd units
                                                 # (vamd, brokers) · first admin key, shown once
 laptop (Linux, macOS; on Windows, inside WSL2; native Windows in v0.2.0)
   install.sh  (or download + vam dock)                 # vam on PATH
   vam config set-host https://my-node:<port> · vam login
   vam node status     # node vamd 0.3.0 (3f9c2ab) release · cli vam 0.3.0
   vam run …           # everything goes over the node's HTTP API
   vam upgrade     # later releases of the CLI; the node is upgraded from its bundle
```

### 11.3 Platforms

```text
                       where vamd runs                                     vam CLI
 Linux x86_64 server   native, x86_64 node bundle                (A)       native (A)
 Linux arm64 server    remote x86_64 node in v0.1.0; aarch64     (v0.2.0)  native (A)
                       node bundle in v0.2.0
 Windows 11 + WSL2     WSL Ubuntu (x86_64) with nested KVM:      (A)       the Linux vam inside WSL (A);
                       the same x86_64 node bundle                         vam.exe in v0.2.0
 Mac, M3 or newer      remote Linux node, or Lima built from     (v0.2.0)  macOS arm64 vam (A)
                       source as today; aarch64 node bundle
                       inside Lima in v0.2.0
 Mac, M1/M2 or Intel   no node locally (no nested virtualization           macOS arm64 / x86_64
                       for Lima); use a remote Linux node                  vam (A), pointed at it
```

- **Windows + WSL2 (`v0.1.0`):** inside the WSL Ubuntu distribution, download the Linux `vam` and
  the x86_64 node bundle, extract it and run `sudo ./vam node init` (`/dev/kvm` comes from WSL2
  nested virtualization; `getting-started/windows-wsl.md`). This path works with the Linux assets
  as they are. The CLI is the Linux `vam` inside WSL. Native `vam.exe` (pointing at
  `https://localhost:<port>`, which WSL2 forwards) comes in `v0.2.0`, where
  `vam node init` on Windows says it is Linux only.
- **Mac (`v0.1.0`):** the macOS `vam` (arm64 or x86_64) points at a remote Linux node. On an M3
  or newer, the Lima path stays as today (`start-vamd-lima.sh` compiles `vamd` in Lima with the
  minimal image; `getting-started/macos-lima.md`).
- **Mac + Lima (`v0.2.0`):** `limactl start` with `scripts/lima-macos.yaml`; inside the VM,
  extract the aarch64 node bundle and run `sudo ./vam node init`; on the Mac, the macOS `vam`
  points at the forwarded port.
