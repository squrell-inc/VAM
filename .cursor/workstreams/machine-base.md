# Workstream: Machine base (E7 Phase 1 + install slice)

```yaml
status: active
slug: machine-base
current_phase: 1
design_docs:
  - design/agent-experience.md   # Part III, §III.1 to §III.5
  - design/benchmark.md          # §2 the `toolchain` task (P11)
  - docs/src/content/docs/getting-started/installation.md
```

## Goal

A fresh Linux host with KVM goes from nothing to a signed-in VAM node with one
command (`vam node init`), and the default microVM base has Python 3 (with a small
curated library set), git, curl and jq, so the `toolchain` benchmark task passes.
The base is built from a pinned Debian slim image at build time only, converted to
ext4, with `vam-runtime` injected as `/sbin/init`. A skill that declares a runtime
the node's base lacks is refused at install, naming the base and the runtime.
The cost (image size, cold boot, golden-restore session start) is measured before
and after on the CI microVM lane.

## Out of scope

- The Vamfile and builder microVM (E7 Phases 3 to 5; on the 0.1.0 cut list).
- Image artifacts as catalog kinds, `imageRef` (E7 Phase 2).
- The app's Machine step; SBOM signing; extra bases (`vam/python`, `vam/node`, `vam/jvm`).
- Run-time package installs; Tier 1 skills vendoring their own packages (DX-101).
- Other lanes' files: `receipt.rs` (E12 Phase 3), catalog import and marketplace (connectors Phase 4).

## Locked decisions

Do not re-litigate without editing this list.

1. **Debian slim (glibc) base, not Alpine** (owner, 2026-11-09). PyPI wheels must
   just work. `vam-runtime` stays a static musl binary injected as `/sbin/init`.
   (Settles design/agent-experience.md open question 5 for the standard base.)
2. **One default base for 0.1.0, not four** (owner, 2026-11-09): Python 3 with a small
   curated library set (requests, pydantic, pandas, numpy, python-dateutil, openpyxl;
   the breakdown may adjust and must justify), pip present to read installed packages
   but no run-time installs, git, curl, jq, ca-certificates, Bun kept. Named per
   design (e.g. `vam/base:2`) with a version and digest recorded. Later: `vam/python`,
   `vam/node`, `vam/jvm`; Tier 1 is after launch.
3. **Measure and record the cost** (owner, 2026-11-09): image size, cold boot and
   golden-restore session start, before and after, on the CI microVM lane. Baseline is
   about 225 ms median (isolation-truth W3b). A regression beyond
   2x is an escalation to the owner.
4. **PID 1 is always `vam-runtime`** (design III.1 rule 1). The image can add things,
   never replace init; the build injects it as `/sbin/init` whatever the image says.
5. **Packages install at build time, never at run time** (design III.1 rule 2).

## Escalate to the owner (standing instruction, 2026-11-09)

A change to the guest boot path or PID 1 behaviour; anything weakening the jailer or
isolation; a cold-start regression over 2x; another lane's files; a locked decision;
editing a scenario's `then`; a third fix cycle; any paid model run. Everything else:
run every wave to the end without waiting.

## Status legend

- `[ ]` not started
- `[~]` in progress
- `[x]` done (code + tests, or merged)

## Phases

### Done phases (archived)

| Phase | Name | Done |
|---|---|---|

### Phase 1 — Standard base and `vam node init`

**Goal:** the default base (locked decision 2) built reproducibly and booted by the CI
lane; `requiresRuntime` checked at install and release; `vam node init` and its docs;
the `toolchain` scenario green on the microVM lane; the cost recorded.  
**Depends on:** none  
**Plan:** breakdown 2026-11-09; closed in base 2026-11-10 (W0-W5 merged; release PR into `main` open; team checklist out)  
**Exit:** scenarios active and green; cold-start numbers in this tracker and in
the 0.1.0 launch plan; team checklist written; one release PR into `main`.

#### Code map (what the breakdown found, 2026-11-09, base)

- **Image today.** `scripts/build-rootfs.sh` exports `busybox:musl`, copies the static
  `vam-runtime` to `/sbin/init`, adds Bun (musl baseline, `latest` unless `BUN_VERSION`), the
  musl loader and `libstdc++`/`libgcc_s` from `alpine:latest`, writes `passwd`/`group`
  (`guest` 1000:100), makes the mount points, then a fixed 256M ext4 with `mke2fs -d`.
  `build-rootfs-lima.sh` is the aarch64 copy. Two guards read the scripts as text:
  `crates/vam-runtime/tests/guest_build_guards.rs` (every `build-rootfs*.sh` builds
  `--bin vam-runtime`, no `process-guest`, at least two scripts) and
  `exec.rs::guest_path_covers_rootfs_executable_dirs` (every `chmod +x "$STAGING_DIR/..."`
  directory is on the guest `PATH` `/usr/local/bin:/usr/bin:/bin`).
- **What the guest assumes of the image.** `guest_init/pid1.rs` brings up loopback with
  `/bin/busybox ifconfig`, falling back to `/bin/busybox ip`, `/sbin/ifconfig`,
  `/bin/ifconfig`: Debian slim has none of them. It mounts tmpfs on `/tmp`, `/work`,
  `/home/guest` and expects `/proc /sys /dev /mnt/drive9` to exist in the read-only image
  (`vam-core/src/vmm.rs` attaches the root drive read-only). Tools run as 1000:100 with
  `HOME=/home/guest`, `TMPDIR=/tmp` and `HTTPS_PROXY`/`https_proxy` set (`exec.rs`).
  `instruction.rs` tells the model the guest is "Minimal Linux (musl/busybox userland)".
  Lane scenarios use `nc`, `awk`, `timeout` in shell commands
  (`guest-has-no-route-off-the-vm`).
- **CI lane.** `scripts/e2e/firecracker-assets.sh` pins Firecracker v1.16.1, jailer and
  kernel `vmlinux-6.1.155` by sha256 and builds the rootfs with `VAM_ROOTFS_SKIP_BUN=1`; its
  stamp hashes `build-rootfs.sh`, `crates/vam-runtime` and `Cargo.lock`. `rust.yml`: the
  `changes` regex names `scripts/build-rootfs\.sh$`; the assets cache key hashes the same
  files; the Summary step already prints `HONEST_COLD_BOOT_LATENCY`,
  `COLD_BOOT_TO_REGISTERED_LATENCY` and the restore median from the `tier="restore"` SLO
  lines. No input chooses the base.
- **Golden snapshot.** `golden.rs` keys the snapshot on sha256(kernel + rootfs), read in
  full once per file (`HashCache`); a bigger rootfs makes the first hash and the bake
  slower, not each session. `image_fits_golden_disk` limits the session data image, not
  the root drive.
- **Node, TLS, key.** `vamd` keeps state beside `VAMD_DB_PATH` (default
  `/var/lib/vam/state.db`) and mints a self-signed `tls/ca.pem` there on first start.
  `bootstrap_api_key_if_empty` mints the first key and **`vamd.rs:583` writes the secret to
  the log** (`tracing::info!("Bootstrap API key (shown once): …")`): under systemd that is
  journald. Nothing parses that line (the e2e runner and Lima scripts pass
  `VAMD_BOOTSTRAP_API_KEY`). The jailer preflight is `vam_core::jail::JailConfig::preflight`
  (uid/gid 65400, cgroup v2, binary ownership, base dir), called from `vamd.rs`.
  `daemon.rs::vm_node_names_every_missing_variable` lists the variables a VM node needs.
  `vam-cli` does not depend on `vam-core`.
- **`requiresRuntime`.** Exists only in design text (DX-101B, decision 9a). `SkillManifest`
  ends at field 12 (`requires_connector`). Install paths: `POST /v1/skills/packages`
  (`skill_package_admit.rs`) and `PUT /v1/skills` (`api/mod.rs::put_skill`). Release paths:
  `api/skill_packages.rs` (`…/release`) and `api/catalog_release.rs::release_skill_wildcard`.
  `GET /v1/node` (`api/mod.rs::get_node`, proto `NodeStatus`, last field 14) has no base;
  `vam node status` prints the isolation line and models.
- **Scenarios.** `needs: firecracker` scenarios run on the lane by selection; the runner
  has `publish-skill { piece, refused, contains }` (publish then release 1.0.0), skill
  pieces with binary `files:`, `expect: { node: { fields } }`, and runs the built CLI for
  `verify-receipt` (`VAM_E2E_CLI`). It has no generic CLI step. The cloud session has no
  KVM, so firecracker scenarios are proved only on the lane.
- **Docs.** `getting-started/installation.md` has no `status` and is in
  `e2e/docs-baseline.txt`; `e2e/docs-features.yaml` names it as the `dev` page of three
  rows.

#### Scenario delta

| Scenario | Change | Layer | Given | Why |
|---|---|---|---|---|
| skill-uses-python-pandas-and-git-on-the-standard-base | new | firecracker | org-admin, agent-with-toolchain-skill (new: `agent-with-shell` rules plus a skill piece whose `files:` carry a CSV, a Python script and a small git bundle) | the `toolchain` task (P11) and the phase exit. The agent loads the skill and runs one `run` call: `python3` reads the CSV with pandas and prints an assembled total token; `git clone` of the bundle into `/tmp` and `git log -1` print the fixture commit subject. Red today at the first `then` thread check (`python3: not found` on the busybox image), class `product` |
| node-reports-the-standard-base-it-boots | new | firecracker | org-admin | the install proof: `GET /v1/node` names `status.base.name: vam/base:2`, `status.base.verified: true` and a `python` and a `git` entry in `status.base.runtimes`, then a chat boots on that base (`run: succeeded`). Red: field absent |
| skill-needing-a-runtime-the-base-lacks-is-refused-at-install | new | process | org-admin, skill-needs-python (`requiresRuntime: { python: ">=3.11" }`), skill-needs-java (`requiresRuntime: { java: ">=21" }`, deferred) | `when`: the node reports `status.base.name: vam/base:2` (the process node is started with a fixture base manifest); `then`: the Python skill publishes and releases, the Java skill is refused with 4xx containing `vam/base:2` and `java`. Red at `when.0` (field absent) |
| node-init-check-names-what-is-missing | new | process | none (a bare-host fixture tree under `e2e/world/fixtures/`) | `vam node init --check --root <copy of the bare tree>` exits non-zero and names each missing thing with its fix: `/dev/kvm`, cgroup v2, `firecracker`, `jailer`, the guest kernel, the base image; and writes nothing under the root. Red: the CLI has no `init` |
| node-init-run-twice-changes-nothing-and-shows-the-key-once | new | process | none (a ready-host fixture tree) | first `vam node init --root <tree> --no-systemd` exits 0, prints the admin key once and the `vam config set-host` / `set-ca` lines; the second run exits 0, reports every step unchanged, prints no key and says the node already has one; the key is in no file under the tree's log or state paths. Red: the CLI has no `init` |
| vm-boots-and-registers-over-vsock | changed: `guards` only (adds the base build files) | firecracker | unchanged | runs on the standard base from wave 1; `then` unchanged |
| guest-has-no-route-off-the-vm | changed: `guards` only (adds the base build files) | firecracker | unchanged | its shell command needs `nc`, `awk` and `timeout` in the new image; `then` unchanged |

Notes for wave 0:

- Five new scenarios, two guards-only edits; nothing retired. The other nine lane
  scenarios are the regression set for the new image and need no edit.
- The two `node-init-*` scenarios need one runner addition: a `cli` step
  (`cli: { args, root: { from: <fixture dir> }, exit: ok|fail, contains: [...], absent: [...] }`)
  that copies the fixture tree to a temp dir, substitutes `{{root}}` and `{{vamd}}` (the
  built `vamd`, which `vam node init` calls for `--init` and `--preflight`), runs
  `VAM_E2E_CLI` and judges exit and output; `absent` also searches the copied tree's files.
  A non-matching run is `product`; a missing binary is `env`. It reuses `run_cli`.
- The process node gets `VAM_BASE_MANIFEST=<fixture base manifest>` from the runner
  (`crates/vam-e2e/src/vamd.rs`); today's `vamd` ignores it, so wave 0 can add it.
- The toolchain skill piece declares no `requiresRuntime` in wave 0: today's manifest
  parser may refuse an unknown field, which would make the scenario red for the wrong
  reason. Wave 2 may add it (a world-piece edit, not a `then` edit).
- The toolchain scenario carries `model: { task: toolchain, version: <next free task-set
  version>, k: 5 }`, and wave 0 adds that version to `e2e/task-sets.yaml` (the previous set
  plus `toolchain`).
- Red proof: the three process scenarios with `scripts/e2e/run.sh --scenarios …` in the
  session. The two firecracker ones are linted by `vam-e2e check` only; wave 1's lane run is
  their first run (the toolchain one goes green there; the base report stays red until
  wave 2).

#### Benchmark delta

No paid run in this phase (playbook §10): every number below comes from the mock model, the
CI lane or a stopwatch.

| Line (`benchmark.md` §1, launch §1) | Change | Before (current base) | After (phase close) | How it is measured |
|---|---|---|---|---|
| P11 machine lacks tools: tasks needing Python or git that pass on the standard base | adds the `toolchain` task (new task-set version) | 0 of 1: the busybox image has no `python3` or `git`; the scenario is red at its first thread check | 1 of 1 on the mock (lane) | `skill-uses-python-pandas-and-git-on-the-standard-base` on the lane, plus one `--bench --mock --scenarios <it> -k 1` trial in the lane's mock-bench step. The two-model real baseline is the owner's word (paid) and belongs to the B1/B2 grid |
| P3 creating an agent is hard: time from empty to working (install half) | adds "minutes from a bare host to a signed-in node" | not measurable: no single command; today's page lists about ten manual steps (build, assets, variables, jailer, TLS, key) | minutes on the team checklist, one fresh host, base build included; target under 30 (open question 15) | stopwatch on the team checklist; written in the progress log |
| Launch proof "the install" | moved | no `vam node init`; base has no Python or git | `vam node init` then sign-in; `vam node status` shows `vam/base:2` with python and git | the two `node-init-*` scenarios and `node-reports-the-standard-base-it-boots` |
| Cost (locked decision 3): image size | new number | busybox image: 256 MiB ext4 file, used blocks recorded by wave 5 | standard base: expected 600 to 800 MiB file (content-sized) | lane Summary: file bytes and `dumpe2fs` used blocks, base name and digest |
| Cost: cold boot | before and after | recorded by wave 5 on the minimal base (same commit) | recorded on the standard base | `HONEST_COLD_BOOT_LATENCY` and `COLD_BOOT_TO_REGISTERED_LATENCY` from the lane's VM tests (already in the Summary) |
| Cost: golden-restore session start | before and after | about 225 ms median, n=15 (isolation-truth W3b, 2026-10-24); re-measured by wave 5 on the minimal base | recorded on the standard base; more than 2x the same-commit before is an owner escalation | the Summary's restore median from the `tier="restore"` lines (n printed) |
| Cost: golden bake and first rootfs hash | new number | recorded by wave 5 | recorded | `golden bake timing:` and `golden key: …ms cached=false` lines, already in `vamd.log` and the Summary |

#### Cost (locked decision 3)

Measured on the CI microVM lane, all runs on GitHub-hosted ubuntu-latest.

| Run | Base | Commit | Image (used) | Honest cold boot | Cold boot to registered (jailed) | Daemon real boot to running | Golden bake | Golden restore median (n) | Scenarios |
|---|---|---|---|---|---|---|---|---|---|
| 42751788600 | minimal `vam/base:1` | | 256 MiB (29 MiB) | 28.9 ms | 247.5 ms (251.8) | 425.5 / 421.4 ms | registered 787 ms, snapshot 4274 ms | ~220 ms (12) | 11 pass; toolchain fails (negative control) |
| 42756578260 | standard `vam/base:2` | | 704 MiB (526 MiB) | 26.0 ms | 169.4 ms (180.8) | 282.5 / 282.8 ms | registered 663 ms, snapshot 6320 ms | ~150 ms (9) | 13/13 |
| 42724833562 | standard `vam/base:2` | (W1) | 704 MiB (526 MiB) | 35.3 ms | 248.6 ms (256.5) | 424.9 / 419.1 ms | registered 625 ms, snapshot 3717 ms | ~217 ms (12) | 11 + toolchain green |

**Conclusion: no regression.** The standard base costs disk (+448 MiB file, +497 MiB used), not time. Boot and restore are within runner variance and far under the 2x escalation line. The previous baseline was ~225 ms (isolation-truth W3b). P11 `toolchain` line: the mock run passes on the standard base and fails on minimal (the negative control).

**How CI records the numbers.** Wave 1 adds a `base: standard | minimal` input to
`rust.yml`'s `workflow_dispatch` (passed to `firecracker-assets.sh` as `VAM_BASE`) and four
Summary lines (base name, digest, image bytes, used bytes). Wave 5 dispatches the lane twice
on the same commit, once per base, and copies both Summary tables into this tracker (a
"Cost" table under this section) and into the 0.1.0 launch plan (the snapshot row's
"about 225 ms" sentence). Same commit, same runner class, so the only change is the image.
A regression beyond 2x on cold boot or restore stops the wave and goes to the owner.

#### Items

Every build wave runs `scripts/ci/local.sh --fast` while building and `--full` before its
PR (shared `CARGO_TARGET_DIR=/home/user/vam/target`), plus its own scenarios with
`scripts/e2e/run.sh --scenarios …`. A wave that touches guest code or `vamd` strings also
runs `scripts/e2e/check-release-gate.sh` locally.

- [x] **MB-1.1 The standard base image, built at build time.**
  - **Scope.** `scripts/base/Dockerfile`: `FROM debian:<release>-slim@sha256:<digest>`
    (open question 1; pinned by digest at wave time), apt sources pointed at
    `snapshot.debian.org` at a pinned timestamp so the package set is reproducible,
    `apt-get install --no-install-recommends` of `python3`, `python3-pip`,
    `python3-requests`, `python3-pydantic`, `python3-pandas`, `python3-numpy`,
    `python3-dateutil`, `python3-openpyxl`, `git`, `curl`, `jq`, `ca-certificates`,
    `busybox-static` (PID 1's loopback bring-up calls `/bin/busybox`) and `netcat-openbsd`
    (`nc` for the lane scenarios); `python3 -m compileall` at build (the root is read-only,
    so no `__pycache__` at run time); apt lists, caches, docs and man pages removed. Bun:
    the glibc baseline build, pinned by version and sha256, never `latest`
    (`/usr/local/bin/bun`, so the `exec.rs` PATH guard still finds a `chmod +x` line
    there). `scripts/build-rootfs.sh` gains `VAM_BASE=standard` (default) and
    `VAM_BASE=minimal` (today's busybox image, one release, prints a deprecation line).
    Standard path: `docker build`, `docker create` + `docker export` into staging,
    `passwd`/`group` merged (Debian's system users kept, `guest` 1000:100 added),
    `vam-runtime` injected as `/sbin/init` whatever the image holds (locked decision 4),
    the mount points made, ext4 content-sized (open question 7) with `mke2fs -d`, a fixed
    UUID and hash seed and `E2FSPROGS_FAKE_TIME`. Beside the image it writes
    `<image>.base.json` (`name: vam/base:2`, `version`, `from` digest, snapshot timestamp,
    image sha256 and bytes, `runtimes: { python, git, bun, curl, jq }` with versions read
    from the built tree) and `<image>.packages.txt` (SBOM-lite: `dpkg-query -W` plus
    `pip list --format=freeze`). The `debugfs` checks add `/usr/bin/python3`,
    `/usr/bin/git`, `/bin/busybox`. The minimal path writes a manifest too
    (`vam/base:1`, `runtimes: { bun }`). `build-rootfs-lima.sh`: open question 12.
    - Library set (locked decision 2 allows adjusting with a reason): kept as given.
      `requests` (HTTP through the proxy), `pandas` + `numpy` (tables; the `toolchain`
      task), `python-dateutil` (dates; pandas pulls it anyway), `openpyxl` (xlsx, which
      the business tasks such as `close-prep` read), `pydantic` (structured output). All
      six are Debian packages, so the build reaches no PyPI and `pip` is there to list
      packages only; Debian's PEP 668 marker also refuses `pip install` at run time.
  - **Files.** `scripts/build-rootfs.sh`, `scripts/base/Dockerfile` (new),
    `scripts/base/pins` (new: Debian digest, snapshot timestamp, Bun version and sha256),
    `scripts/build-rootfs-lima.sh` (per open question 12).
  - **Acceptance.** The standard image builds on `ubuntu-latest` (the lane) and on a
    Debian or Ubuntu host with Docker; the manifest and package list sit beside it; the
    minimal flag still builds today's image; `guest_build_guards` and the PATH guard
    pass; two builds on the same pins give the same package list (a byte-identical image
    is a target, checked once by hand and written down, not a gate).
  - **Tests.** `cargo test -p vam-runtime --test guest_build_guards`,
    `cargo test -p vam-runtime guest_path_covers_rootfs_executable_dirs`; the build itself
    runs on the lane (the cloud session may have no Docker or KVM).
  - **Turns green** (with MB-1.2): `skill-uses-python-pandas-and-git-on-the-standard-base`.
  - `Decide: NOW — proof 3 (launch proof "the install": the standard base has Python and git; P11), cost 1 (one wave, scripts only, no API, no guest code)`

- [x] **MB-1.2 The CI lane boots the standard base and prints its cost.**
  - **Scope.** `firecracker-assets.sh` builds the image `VAM_BASE` names (default
    standard, with Bun: open question 5); its stamp hashes `scripts/base/**` too; it
    exports the manifest path. `rust.yml`: the `changes` regex adds `scripts/base/` and
    every `scripts/build-rootfs*.sh`; the assets cache key adds `scripts/base/**`; a
    `workflow_dispatch` input `base: standard | minimal`; the Summary prints base name,
    digest, image bytes and used bytes; the Docker build image is removed after the build
    (runner disk).
  - **Files.** `scripts/e2e/firecracker-assets.sh`, `.github/workflows/rust.yml`.
  - **Acceptance.** One lane run on the PR: every existing lane scenario green on the
    standard base, the toolchain scenario green, the Summary shows the four base lines.
  - **Tests.** The lane (`actions: lane`); `scripts/e2e/firecracker-assets.sh --check`
    locally where Docker exists.
  - **Turns green:** `skill-uses-python-pandas-and-git-on-the-standard-base`; keeps
    `vm-boots-and-registers-over-vsock`, `guest-has-no-route-off-the-vm` and the other
    lane scenarios green.
  - `Decide: NOW — proof 3 (security page: CI boots the image users get; P11), cost 1 (files the wave already touches, CI only)`

- [x] **MB-1.3 The model is told what the base has.**
  - **Scope.** `instruction.rs` facts: the OS line from `/etc/os-release` `PRETTY_NAME`
    (falling back to today's text when absent) instead of the fixed "musl/busybox"
    sentence; list `python3`, `git`, `curl`, `jq` when present, the way Bun is listed
    today; the longer prose at line ~478 says the same. Not the boot path and not PID 1.
  - **Files.** `crates/vam-runtime/src/instruction.rs` (and its unit tests).
  - **Acceptance.** Unit tests with a temp root for both images; the minimal image's
    facts change only in the OS line.
  - **Tests.** `cargo test -p vam-runtime instruction`; `scripts/e2e/check-release-gate.sh`.
  - **Turns green:** none alone (it serves the real-model `toolchain` runs).
  - `Decide: NOW — proof 2 (P11 real-model line: a model told "busybox" does not reach for python3), cost 0 (rides wave 1, one file)`

- [x] **MB-1.4 `requiresRuntime` in the skill manifest.**
  - **Scope.** `proto/vam/v1/skills.proto` `SkillManifest`:
    `map<string, string> requires_runtime = 13;` (JSON `requiresRuntime`). `vam-spec`
    validation: keys `[a-z][a-z0-9-]*`, values `""` or `*` (any version), `>=X[.Y[.Z]]` or
    `=X[.Y[.Z]]` (open question 9); a small version compare in `vam-spec` (no new crate).
    Round-trip in `examples_json`; `docs/api/openapi.yaml` if it lists `SkillManifest`.
  - **Files.** `proto/vam/v1/skills.proto`, `crates/vam-spec/src/validation.rs`,
    `crates/vam-spec/src/runtime_requirement.rs` (new), `crates/vam-spec/tests/examples_json.rs`,
    `docs/api/openapi.yaml`.
  - **Acceptance.** A bad key or value is refused at validation with the field named; an
    absent field changes nothing (digests of existing packages unchanged).
  - **Tests.** `cargo test -p vam-spec`; `scripts/ci/local.sh --fast` (protos).
  - **Turns green** (with MB-1.5, MB-1.6): `skill-needing-a-runtime-the-base-lacks-is-refused-at-install`.
  - `Decide: SLICE — proof 3 (launch proof "the install"; developer-experience decision 9a: install fails fast), cost 2 (a proto field) → slice: a name-to-requirement map with ">=" and "=" only, checked at install and release. Ranges, per-tool runtimes and the Vamfile RUNTIME line: backlog, trigger "E7 Phase 3 (Vamfile)"`

- [x] **MB-1.5 The node knows its base and reports it.** (code and unit tests landed in W2; `node-reports-the-standard-base-it-boots` is active and passed on the lane, run 42756578260, 13/13)
  - **Scope.** `vamd` reads the base manifest from `VAM_BASE_MANIFEST`, else
    `<VAM_ROOTFS_PATH without .ext4>.base.json`. On a VM node it compares the manifest's
    digest with the rootfs sha256 the golden key already computes (`golden.rs`
    `HashCache`; no second full read). Missing or mismatched: one startup line naming the
    file, and the node reports `verified: false` with no runtimes (open question 8). A
    process node reports the manifest it was given, unverified. Proto
    `orchestrator.proto` `NodeStatus`: `NodeBase base = 15;` with `name`, `version`,
    `digest`, `verified`, `repeated NodeRuntime runtimes` (`name`, `version`).
    `GET /v1/node` fills it; `vam node status` prints
    `base: vam/base:2 (sha256:<12 hex>, verified) python 3.x, git 2.x, bun 1.x, …`.
  - **Files.** `proto/vam/v1/orchestrator.proto`, `crates/vam-orchestrator/src/node_base.rs`
    (new), `api/mod.rs` (`get_node` only), `golden.rs` (a read accessor, no behaviour
    change), `daemon.rs` (the variable), `bin/vamd.rs` (load at start),
    `crates/vam-cli/src/cmd/node.rs`.
  - **Acceptance.** Unit tests: manifest beside the image, override variable, digest
    mismatch, missing file, process node; the CLI line from a recorded body.
  - **Tests.** `cargo test -p vam-orchestrator node_base`, `cargo test -p vam-cli node`;
    `--scenarios skill-needing-a-runtime-the-base-lacks-is-refused-at-install`; the lane.
  - **Turns green:** `node-reports-the-standard-base-it-boots` (lane) and the `when` of
    the refusal scenario.
  - `Decide: NOW — proof 3 (launch proof "the install"; the refusal message needs it), cost 1 (a field beside isolation in a handler that exists; counted in MB-1.4's slice)`

- [x] **MB-1.6 Install and release refuse a skill whose runtime the base lacks.**
  - **Scope.** One function, `node_base::check_requires_runtime(&SkillManifest)`, called
    at both install paths (`skill_package_admit.rs` after the manifest is validated;
    `api/mod.rs::put_skill`) and both release paths (`api/skill_packages.rs` release,
    `api/catalog_release.rs::release_skill_wildcard`). A 400 whose text names the skill,
    the runtime, the requirement and the base:
    `skill <urn> needs java >=21; this node's base vam/base:2 (sha256:<12 hex>) has no java`,
    or `… needs python >=3.14; vam/base:2 has python 3.13.5`. A skill with no
    `requiresRuntime` is never affected. The marketplace import (connectors Phase 4) goes
    through admission and inherits the check without this phase touching it.
  - **Files.** `crates/vam-orchestrator/src/node_base.rs`, one call each in
    `skill_package_admit.rs`, `api/mod.rs`, `api/skill_packages.rs`,
    `api/catalog_release.rs`.
  - **Acceptance.** Unit tests: runtime missing, version too old, satisfied, no field,
    unverified base refuses any requirement, a draft admitted before the check is refused
    at release.
  - **Tests.** `cargo test -p vam-orchestrator requires_runtime`;
    `--scenarios skill-needing-a-runtime-the-base-lacks-is-refused-at-install`.
  - **Turns green:** `skill-needing-a-runtime-the-base-lacks-is-refused-at-install`.
  - `Decide: NOW — proof 3 (launch proof; P9-adjacent: a skill fails at install, not mid-run), cost 1 (four one-line calls and one module; counted in MB-1.4's slice)`

- [x] **MB-1.7 The first admin key is minted by `vamd --init`, shown once, never logged.**
  - **Scope.** A one-shot `vamd --init`: creates the state dir (0700), the DB, the TLS
    material (`tls::resolve`, self-signed unless an operator pair is set) and the signing
    keys; mints the first key only when none exists and prints it to **stdout** once with
    "save it now; it is not shown again"; when a key exists it prints "this node already
    has an admin key; none shown" and exits 0. Daemon start stops writing the secret to
    the log (`vamd.rs:583`): with no key and no `VAMD_BOOTSTRAP_API_KEY` it mints nothing
    and logs "no API key yet: run `vam node init` (or `vamd --init`)" (open question 10).
    `VAMD_BOOTSTRAP_API_KEY` keeps working (the e2e runner and Lima scripts use it).
  - **Files.** `crates/vam-orchestrator/src/bin/vamd.rs`, `daemon.rs` (the flag),
    `api/mod.rs` (`bootstrap_api_key_if_empty` only), a test under
    `crates/vam-orchestrator/tests/`.
  - **Acceptance.** A binary test runs `vamd --init` twice on a temp state dir: the first
    stdout has one key, the second none; neither stderr nor any file under the state dir
    contains the secret; a daemon started on an empty DB logs no secret.
  - **Tests.** `cargo test -p vam-orchestrator --test <new test>`;
    `scripts/e2e/check-release-gate.sh`.
  - **Turns green:** part of `node-init-run-twice-changes-nothing-and-shows-the-key-once`.
  - `Decide: NOW — proof 3 (security floor: a secret in the daemon log, journald under systemd), cost 1`

- [x] **MB-1.8 `vam node init --check`.**
  - **Scope.** `vam node init --check [--root DIR]` needs no host or key (`main.rs`
    dispatches it before config is resolved). One line per check, `ok | missing | wrong`,
    each with its fix, exit 1 if any is not ok, nothing written: x86_64 Linux; `/dev/kvm`
    read-write; cgroup v2 (`cgroup.controllers`); `firecracker` and `jailer` present at
    the pinned version; the guest kernel present with the pinned sha256; the base image
    and its manifest present with a matching digest; uid/gid 65400 not used by an
    existing account; the jailer base on the same filesystem as the assets (files enter a
    jail by hard link); state dir ownership and mode; Docker (only when the base is to
    be built); systemd; the listen port free. The jailer-specific rules come from a new
    one-shot `vamd --preflight` that prints `JailConfig::preflight` and the missing-variable
    check as JSON (open question 11), so `vam-cli` takes no `vam-core` dependency and the
    rules exist once. The pins (Firecracker, jailer, kernel URLs and sha256) move into one
    file that `firecracker-assets.sh` sources and the CLI embeds. `--check` never offers
    `--no-jailer` as a fix.
  - **Files.** `crates/vam-cli/src/cmd/node.rs`, `crates/vam-cli/src/cmd/node_init.rs`
    (new), `crates/vam-cli/src/main.rs`, `crates/vam-orchestrator/src/bin/vamd.rs`
    (`--preflight`), `scripts/e2e/firecracker-assets.sh`, `scripts/assets.pins` (new).
  - **Acceptance.** Unit tests per check on temp roots; `--check` on a bare tree lists
    every missing item and leaves the tree byte-identical.
  - **Tests.** `cargo test -p vam-cli node_init`;
    `--scenarios node-init-check-names-what-is-missing`.
  - **Scenario contract (wave 0).** `--root DIR` is a test seam this wave implements: `--check`
    and `init` look up `/dev/kvm`, the cgroup mount, the asset, state, `/etc/vam` and systemd
    unit directories under DIR, and never need real root or the real `/dev`. `--vamd PATH` names
    the `vamd` for `--init` and `--preflight`. Line format: `missing <check>` or `wrong <check>`,
    detail, `fix: <how>`; checks `kvm`, `cgroup-v2`, `firecracker`, `jailer`, `kernel`,
    `base-image`; `init` prints `admin key: <key>` and "save it now; it is not shown again"
    once, then `ok (unchanged)` per step and "already has an admin key" on a rerun.
  - **Turns green:** `node-init-check-names-what-is-missing`.
  - `Decide: NOW — proof 3 (launch proof "the install"), cost 1 (CLI and a vamd one-shot; no API)`

- [x] **MB-1.9 `vam node init` installs the node, idempotently.**
  - **Scope.** As root (or under `--root` for tests and packaging): run the checks and
    stop on a missing item it cannot fix itself; fetch the pinned Firecracker, jailer and
    kernel (sha256-verified) into `/var/lib/vam/assets`; put the base in place (build with
    `scripts/build-rootfs.sh` from the checkout by default, or `--base PATH|URL` with
    `--base-sha256`, verified against its manifest: open question 4); write
    `/etc/vam/vamd.env` (0600: `VAMD_FIRECRACKER_BIN`, `VAMD_JAILER_BIN`,
    `VAM_KERNEL_PATH`, `VAM_ROOTFS_PATH`, `VAMD_DB_PATH`, `VAMD_TLS_SANS` from
    `--host-name` or the primary address); write `/etc/systemd/system/vamd.service`
    (root, `EnvironmentFile`, `Restart=on-failure`); run `vamd --init` (TLS, key shown
    once); `systemctl enable --now vamd`; wait for `/v1/health` over TLS with the node's
    CA; write the invoking user's CLI config (host, CA, key; `SUDO_USER`-aware); print the
    sign-in block for other machines (`vam config set-host https://<host>:8080`,
    `vam config set-ca <copied ca.pem>`, `vam config set-key --from-env VAM_API_KEY`,
    `vam node status`). Each step compares first and prints `ok (unchanged)`: the key is
    never minted twice, TLS is never rewritten, the unit and env file are rewritten (and
    vamd restarted) only when their content differs, the base is rebuilt only when its
    stamp differs. `--no-systemd` and `--no-start` for containers and the scenario.
  - **Files.** `crates/vam-cli/src/cmd/node_init.rs` (and its templates).
  - **Acceptance.** Unit tests per step on temp roots, including a second run with no
    change and a run after the unit file was edited by hand.
  - **Tests.** `cargo test -p vam-cli node_init`;
    `--scenarios node-init-run-twice-changes-nothing-and-shows-the-key-once`.
  - **Turns green:** `node-init-run-twice-changes-nothing-and-shows-the-key-once`.
  - `Decide: NOW — proof 3 (launch proof "the install"; P3 minutes), cost 1 (one wave, CLI only, uses the vamd one-shots)`

- [x] **MB-1.10 The installation page, rewritten and honest.**
  - **Scope.** Rewrite `getting-started/installation.md`: requirements (x86_64 Linux,
    KVM, cgroup v2, root, Docker for the base build, a Rust toolchain to build `vam` and
    `vamd`); the steps (clone, build, `sudo vam node init --check`, `sudo vam node init`,
    sign in); what `vam/base:2` holds and that nothing installs at run time;
    `requiresRuntime` in one paragraph with a link; the `VAM_BASE=minimal` fallback for
    this release; troubleshooting by check name. Frontmatter `status: shipped` and
    `proves:` naming the four process and lane scenarios above; the page leaves
    `e2e/docs-baseline.txt`. `e2e/docs-features.yaml`: rows for `vam node init`, the
    standard base and `requiresRuntime`, each with its proofs. Small fixes:
    `users-and-roles.md` ("the key printed on first start"), `project-status.md`
    (`vam-runtime` and `vam-build` rows still say busybox and Bun musl),
    `authoring/skill-manifest.md` (the field). No new page, so `docs/astro.config.mjs` is
    unchanged; if one is added, its sidebar entry is added too.
  - **Files.** `docs/src/content/docs/getting-started/{installation,users-and-roles,project-status}.md`,
    `docs/src/content/docs/authoring/skill-manifest.md`, `e2e/docs-baseline.txt`,
    `e2e/docs-features.yaml`.
  - **Acceptance.** `vam-e2e check` clean; `pnpm build` clean; every `shipped` claim
    names an active scenario or test.
  - **Tests.** `cargo run -p vam-e2e -- check`; `cd docs && pnpm build`.
  - **Turns green:** none (docs).
  - `Decide: NOW — proof 3 (launch proofs "the install" and "the honest page"), cost 0`

- [x] **MB-1.11 The cost and the `toolchain` line, recorded.**
  - **Scope.** Add the toolchain scenario to the lane's mock-bench step. Dispatch the lane
    twice on one commit (`base: minimal`, `base: standard`); copy image size, both
    cold-boot numbers, the restore median with its n, the bake time and the first rootfs
    hash time into a "Cost" table in this tracker and into the 0.1.0 launch plan;
    write the P11 mock result. More than 2x on cold boot or restore: stop and escalate.
  - **Files.** `.github/workflows/rust.yml` (mock-bench step), this tracker,
    the 0.1.0 launch plan.
  - **Acceptance.** Both runs linked from the progress log; the table filled.
  - **Tests.** The two lane runs.
  - **Turns green:** none new (records).
  - `Decide: NOW — proof 2 (P11 line; locked decision 3), cost 0`

#### Waves

One wave at a time. `actions`: `none` = `[skip ci]`; `fast` = label `skip-vm-lane`, the
fast jobs run once (any change under `crates/vam-orchestrator/`, `crates/vam-e2e/`,
`scripts/e2e/`, `proto/` or `Cargo.lock` matches the lane filter); `lane` = one full run,
the microVM lane included.

| Wave | Items | Turns green | Repo | Model | actions | Budget | Depends on | Files touched | Status | PR |
|---|---|---|---|---|---|---|---|---|---|---|
| 0 | scenarios | — (process three red at the step the phase adds; firecracker two linted) | vam | Sonnet low | fast | 60 min | — | `e2e/scenarios/` (5 new, 2 guards-only), `e2e/world/` (toolchain agent and skill, two `requiresRuntime` skills), `e2e/world/fixtures/{toolchain,base,host-bare,host-ready}/`, `crates/vam-e2e/src/{scenario.rs,engine.rs,check.rs,vamd.rs}` (`cli` step, `VAM_BASE_MANIFEST` on the process node), `e2e/README.md`, `e2e/task-sets.yaml` | [x] | — |
| 1 | MB-1.1, MB-1.2, MB-1.3 | skill-uses-python-pandas-and-git-on-the-standard-base; all existing lane scenarios stay green | vam | Sonnet medium: a new image build path, the CI lane and the guest's glibc assumptions, which the plan cannot fully specify | lane | 120 min | 0 | `scripts/build-rootfs.sh`, `scripts/build-rootfs-lima.sh`, `scripts/base/` (new), `scripts/e2e/firecracker-assets.sh`, `.github/workflows/rust.yml`, `crates/vam-runtime/src/instruction.rs` | [x] | — |
| 2 | MB-1.4, MB-1.5, MB-1.6 | skill-needing-a-runtime-the-base-lacks-is-refused-at-install, node-reports-the-standard-base-it-boots | vam | Sonnet medium: two proto fields across `vam-spec`, `vam-orchestrator` and `vam-cli` | lane (proves the base report on the real image) | 120 min | 1 (manifest format) | `proto/vam/v1/{skills,orchestrator}.proto`, `crates/vam-spec/src/{validation.rs,runtime_requirement.rs}`, `crates/vam-spec/tests/examples_json.rs`, `docs/api/openapi.yaml`, `crates/vam-orchestrator/src/{node_base.rs,golden.rs,daemon.rs,bin/vamd.rs,skill_package_admit.rs,api/mod.rs,api/skill_packages.rs,api/catalog_release.rs}`, `crates/vam-cli/src/cmd/node.rs` | [x] | — |
| 3 | MB-1.7, MB-1.8 | node-init-check-names-what-is-missing | vam | Sonnet medium: crosses `vam-cli` and `vamd`, and changes how the first key is shown (the orchestrator reads that code before merging) | fast | 120 min | 2 (`--check` reads the base manifest) | `crates/vam-orchestrator/src/{bin/vamd.rs,daemon.rs,api/mod.rs}`, `crates/vam-orchestrator/tests/` (new), `crates/vam-cli/src/{main.rs,cmd/node.rs,cmd/node_init.rs}`, `scripts/assets.pins` (new), `scripts/e2e/firecracker-assets.sh` | [x] | — |
| 4 | MB-1.9 | node-init-run-twice-changes-nothing-and-shows-the-key-once | vam | Sonnet medium: system install steps and idempotency the plan cannot fully specify | fast | 120 min | 3 | `crates/vam-cli/src/cmd/node_init.rs` and its templates | [x] | no PR (merged) |
| 5 | MB-1.10, MB-1.11 | — (all five active; numbers recorded) | vam | Sonnet low: docs, tracker and recording | lane (two dispatch runs, one per base; doubles as the release-candidate lane run) | 90 min | 1 to 4 | `docs/src/content/docs/getting-started/{installation,users-and-roles,project-status}.md`, `docs/src/content/docs/authoring/skill-manifest.md`, `e2e/docs-baseline.txt`, `e2e/docs-features.yaml`, `.github/workflows/rust.yml` (mock-bench step), this tracker, the 0.1.0 launch plan | [x] | — |

Decide lines per wave: W0 `NOW — proof 3, cost 1`; W1 `NOW — proof 3, cost 1`; W2
`SLICE — proof 3, cost 2 → slice as MB-1.4`; W3 `NOW — proof 3 (security floor and install), cost 1`;
W4 `NOW — proof 3, cost 1`; W5 `NOW — proof 3, cost 0`.

**Owner-escalation triggers, by wave** (standing instruction above):

- **W1, the guest boot path.** The image under PID 1 changes from busybox/musl to Debian
  glibc. No `pid1.rs` or boot code is planned to change: `busybox-static` keeps
  `/bin/busybox ifconfig` working. If the guest does not register on the new image and
  the fix needs a change in `vam-runtime`'s boot or PID 1 code, the wave stops and
  escalates. A cold-boot or restore regression over 2x seen on this lane run also stops it.
- **W2, other lanes' files.** `skill_package_admit.rs` is in connectors Phase 4's file list
  ("check before 4") and `api/mod.rs` in Phase 5's; `skills.proto` field 13 and
  `orchestrator.proto` field 15 must not collide with another lane's open branch. The
  orchestrator checks with the connectors lane before W2 starts (launch §4 conflict
  rule: the later lane holds). W2 edits no catalog import or marketplace code and not
  `receipt.rs`.
- **W3, isolation and keys.** `vamd --preflight` reads the jailer rules and never relaxes
  them; `vam node init` never suggests `--no-jailer`. The daemon's first-key behaviour
  changes (open question 10): the orchestrator reads that diff as security code.
- **W5.** A cost regression over 2x goes to the owner before the numbers are published.
- **Locked decisions.** None is touched. Locked decision 1 relies on `vam-runtime`
  staying a static musl binary; W1 keeps it so.

#### Risks (glibc base under a musl PID 1)

1. **Loopback.** PID 1 needs `/bin/busybox` (or `ifconfig`): covered by `busybox-static`;
   a wrong path fails every boot, so W1's first lane run is the check.
2. **Applets the scenarios and agents use.** Busybox gave `nc`, `awk`, `timeout`, `wget`,
   `vi`. Debian slim has `mawk` and coreutils `timeout`; `nc` comes from `netcat-openbsd`.
   `wget` and the rest are dropped (agents use `curl`); `/bin/sh` becomes `dash`, which
   `exec.rs`'s `sh -c` handles.
3. **Users.** `passwd`/`group` must keep Debian's system users and add `guest` 1000:100;
   gid 100 is Debian's `users`, which matches.
4. **Bun and the musl libs.** The musl loader and `libstdc++` copies from Alpine go away
   with the glibc Bun; the minimal path keeps them.
5. **Read-only root.** No `__pycache__`, no `/var/tmp` writes, no `apt`, `dpkg` or `pip`
   writes at run time; `git` needs a writable `HOME` (tmpfs, fine).
6. **Size and disk.** A larger image lengthens the first rootfs hash and the bake (once
   per node start) and fills the runner's disk (assets cache, Docker layers). The root
   drive is hard-linked into each jail, so per-session cost is disk-free; it must sit on
   the same filesystem as the jailer base, which `--check` tests.
7. **Version coupling.** The image carries `vam-runtime`; an image from another commit
   can speak an older vsock protocol, hence open question 4's default.

#### Open questions (default in bold)

1. Debian release for `vam/base:2`: **trixie-slim (Debian 13: Python 3.13, pandas 2.2,
   pydantic 2)**, or bookworm-slim (Python 3.11, pydantic 1.10).
2. Python libraries from **Debian packages pinned through `snapshot.debian.org`** (no PyPI
   at build), or pip wheels with hashes. The six libraries are kept as given; **no
   additions** (PyYAML is the first candidate if the owner wants one).
3. Build method: **Docker on the build host with a pinned Dockerfile** (what the lane and
   most hosts have; today's script already needs it), or `mmdebstrap` (Debian and Ubuntu
   hosts only). Design III.3's builder microVM stays Phase 3; Phase 1 builds only VAM's
   own Dockerfile, never a user's `RUN`.
4. The base on a fresh host: **built from the checkout by `vam node init`** (the image
   carries `vam-runtime`, which must match `vamd`; no release-asset pipeline exists), with
   `--base PATH|URL --base-sha256` accepted. A released, signed base artifact is backlog,
   trigger "the first tagged binary release".
5. Bun: **the glibc baseline build, pinned by version and sha256, kept in the base and in
   the lane image** (the lane boots what users get; `VAM_ROOTFS_SKIP_BUN` stays for local
   builds).
6. Busybox: **`busybox-static` installed as `/bin/busybox` with no applet links, plus
   `netcat-openbsd`**, so PID 1 needs no change.
7. Image size: **content-sized, plus 25% headroom, rounded up to 64 MiB** (expected 600 to
   800 MiB), instead of the fixed 256M.
8. Base manifest: **`<image>.base.json` beside the image, `VAM_BASE_MANIFEST` overrides;
   on a VM node a missing or mismatched manifest means no runtimes** (skills with
   `requiresRuntime` are refused, others unaffected).
9. Requirement syntax: **`*` or empty, `>=X[.Y[.Z]]`, `=X[.Y[.Z]]` only.**
10. Daemon start with no key: **mints nothing and logs the hint; the key comes only from
    `vamd --init`, `vam node init` or `VAMD_BOOTSTRAP_API_KEY`**. Alternative: keep minting
    but print the secret only when stderr is a terminal.
11. Jailer checks in `--check`: **through a `vamd --preflight` one-shot** (no `vam-core`
    dependency in `vam-cli`, one copy of the rules).
12. Lima (aarch64): **the same Dockerfile with `--platform linux/arm64`, checked by hand,
    no CI**; if it does not fit wave 1's budget, Lima stays on `minimal` for 0.1.0 with a
    docs note.
13. Task-set version for `toolchain`: **the next free version (the latest set plus
    `toolchain`)**, chosen in wave 0.
14. The `minimal` fallback: **kept for 0.1.0 only, removed in the next release**; its
    manifest says `vam/base:1`, runtimes `bun`.
15. P3 target: **under 30 minutes from a bare host to signed in, base build included**,
    recorded at the team checklist, not a gate.

**Phase close.** All five new scenarios `active` and green (three process ones in CI on
every PR, two firecracker ones on the lane); every existing lane scenario green on the
standard base; the Cost table filled here and in the 0.1.0 launch plan with no
regression over 2x (or the owner's word on one); the team checklist run once on a fresh
host and its minutes logged; one release PR into `main`.

#### Team checklist (phase close)

On a fresh x86_64 Linux host with KVM (for example Ubuntu 24.04 on a metal or nested-virtualization instance), Docker and a Rust toolchain. Start a stopwatch (P3).

1. Setup: clone the repo; `cargo build --release -p vam-cli -p vam-orchestrator`.
2. `sudo target/release/vam node init --check`: every missing item is named with its fix. Fix them.
3. `sudo target/release/vam node init`: the admin key is shown once; the sign-in lines are printed. `journalctl -u vamd | grep vam_` finds no key.
4. Run `sudo target/release/vam node init` again: every step says unchanged; no key is shown.
5. On another machine: `vam config set-host …`, `vam config set-ca …`, `vam config set-key …`; `vam node status` shows `base: vam/base:2 (…, verified)` with python and git.
6. Publish the toolchain skill and agent (the files are `e2e/world/skill-toolchain.yaml`, `e2e/world/agent-with-toolchain-skill.yaml` and `e2e/world/fixtures/toolchain/`) and run it, asking it to total the ledger and read the repository log: the pandas total and the cloned commit subject come back. Stop the stopwatch.
7. Publish a skill with `requiresRuntime: { java: ">=21" }`: refused, naming `vam/base:2` and `java`.
8. Report: minutes from step 1 to step 6 (target under 30), the image size and restore median from the release PR's lane Summary, and anything that needed a manual fix; write the minutes in the progress log.

### Phase 2+ — later (stubs)

- Phase 2: image artifact kind and `imageRef` (agent-experience Phase 2).
- Phases 3 to 5: Vamfile, Machine step, snapshots/SBOM signing. Cut from 0.1.0.

## Progress log

| Date | Wave | What | Scenarios green / red | Fix cycles | Wall time |
|---|---|---|---|---|---|
| 2026-11-09 | W0 (merged) | 5 scenarios pending, `cli` step in `vam-e2e`, `VAM_BASE_MANIFEST` on the process node, fixtures. Disk before 30 GB, after 20 GB. Two container restarts killed `local.sh --full` in smoke both times after `decision-on-a-warm-session-resumes-once`; smoke run one scenario at a time instead: 109 pass + 3 red (right reason), 0 fail; the crash is an existing issue in one long `run.sh --smoke` process (reported to the Master) | 0 green (wave 0), 3 red right reason + 2 firecracker linted | 0 | ~2.5 h (two restarts) |
| 2026-11-10 | W1 (merged) | Standard base `vam/base:2` (Debian trixie-slim by digest, snapshot apt, Python + 6 libs, git, curl, jq, busybox-static, Bun bun-v1.4.4 pinned), `vam-runtime` as `/sbin/init`, manifest + package list beside the image, lane `base` input, guest facts. Lane run 42724833562 (standard): 11 lane scenarios pass, toolchain green; image 704 MiB (526 MiB used); cold boot to registered 249 ms; golden restore median ~217 ms (n=12, baseline ~225 ms): no regression. Local: workspace 124 ok; smoke one at a time 116 pass + 3 red right reason; `connector-id-cannot-be-taken-by-a-hand-added-server` fails alone (no mock-provider piece in its `given`; passes beside a mock scenario: existing run-order dependency). First agent blocked on an environment prompt and was restarted offline; another restart killed the second agent's `--full` | 1 green (toolchain, lane), lane 11/11 | 0 | ~2 h |
| 2026-11-10 | W2 (merged) | Finished by the orchestrator after a restart killed the agent before its PR. On top of W1 + `main` (connectors Phase 4, merged via with additive vam-e2e conflicts). `local.sh --full --scenarios <refusal>` exit 0 (124 ok); smoke one at a time 125 pass, 2 red right reason (`node-init-*`), 1 known run-order FAIL alone (`connector-id-…`). Connectors-lane files: 3 added lines, as approved | 1 green (refusal, process) | 0 | ~1.5 h (restarts) |
| 2026-11-10 | W3 (merged) | `vamd --init` mints the first admin key, shown once on stdout, DB keeps its sha256; daemon start logs only the key id and mints nothing without a key (WARN, starts); startup `Config` log redacted (it also leaked the key); `vamd --preflight [--root]` (`--root` refused elsewhere); `vam node init --check`; `scripts/assets.pins`. Security code read by the orchestrator; tests assert the secret is absent from tracing, stderr, argv and files. Agent killed by a restart after pushing; orchestrator finished the PR. `local.sh --full --scenarios node-init-check…` exit 0 (125 ok); smoke one at a time 126 pass, 1 red right reason (run-twice, W4), 1 known run-order FAIL alone | 1 green (node-init check) | 0 | ~1.5 h (restarts) |
| 2026-11-10 | W4 (merged no PR: the agent died before opening one and the orchestrator merged the proven branch directly) | `vam node init`: checks, state dir 0700, assets by pin, base built from the checkout or `--base` + sha256, `/etc/vam/vamd.env` 0600 (no secrets), systemd unit (root, ProtectSystem=full, ProtectHome=yes, no PrivateTmp, no --no-jailer/--insecure-dev; test), `vamd --init` key relayed once to stdout, sign-in lines; compare-then-write so a second run is `ok (unchanged)`. Code read by the orchestrator. `local.sh --full --scenarios node-init-*` exit 0 (125 ok, both PASS); smoke one at a time 127 pass + 1 known run-order FAIL alone | 1 green (run twice) | 0 | ~1.5 h (restarts) |
| 2026-11-10 | W5 | Install page rewritten around `vam node init` (status shipped, five proofs), key-on-first-start text fixed, project-status and skill-manifest (`requiresRuntime`) updated, three feature rows, `node-reports-the-standard-base-it-boots` active (lane run 42756578260, base=standard, 13/13). Cost table recorded (runs 42751788600 minimal, 42756578260 and 42724833562 standard): no regression; mock-bench step adds the toolchain scenario on the standard base. Team checklist written (not yet run on a fresh host) | 5 of 5 new scenarios active | 0 | docs only |
