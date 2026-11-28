# Workstream: VAM image (curated bases, imageRef, Vamfile Lite)

```yaml
status: active
slug: vam-image
current_phase: 1
base_branch: feat/vam-image
design_docs:
  - design/vam-image.md                 # the plan of record for this tracker
  - design/agent-experience.md          # Part III (E7), lines 448-536
  - design/developer-experience.md      # §19 (lines 1075-1160)
  - .cursor/workstreams/developer-experience.md   # Phase 9, DX-101 to DX-105 (lines 1198-1288)
  - docs/src/content/docs/build/        # all status: planned; updated in I3
```

## Goal

An agent runs on the machine it needs, and nothing installs inside the guest. A node holds
several VAM images (`vam/base:2`, `vam/python`, `vam/node`), each with its own manifest and
golden snapshot; a skill brings its Python or JS packages from a hashed lockfile, fetched
and cached on the host; an agent version names its image with `imageRef` and gets a VM size
inside the operator's caps; the receipt records the image a run booted. Then Vamfile Lite
(declarative, no `RUN`) lets authors and the community add system packages through
`vam-marketplace` listings, built by each node in a builder microVM whose only egress is the
package mirrors. The full Vamfile with `RUN` waits for a client who is blocked without it.

## Out of scope

- Package installs inside the guest; anything a run writes becoming part of a machine.
- Building a recipe from a bare URL; community recipes built with Docker on the host.
- Publishing built ext4 images; an OCI or ORAS registry (licence review first,
  `design/release-artifacts.md` §5.2).
- Nix or any VAM dependency resolver (DX locked decision 9; DX-103 stays unbuilt).
- An image field on the App; environment profiles (DX-17, DX Phase 8).
- Custom kernels, non-Debian bases, arbitrary OCI `FROM`; macOS/Lima images.
- `WARM` snapshots; image signing beyond the catalog digest (E7 Phase 5 with E6).
- Other lanes' files while they own them: `run_start.rs`, `catalog_resolve.rs`,
  `manager.rs`, the agent spec proto (Apps lane through P4), denial shapes (self-aware S1).

## Locked decisions

Do not re-litigate without editing this list. All **proposed, owner to confirm**
(2026-11-28) except where they restate another tracker's locked decision. Rationale:
`design/vam-image.md` "Locked decisions".

1. **PID 1 is always `vam-runtime`**, injected last as `/sbin/init` in every image
   (restates `machine-base` decision 4).
2. **No package installs inside the guest, ever** (Q13). Packages enter at image build time
   or through a skill's read-only mount before boot (restates `machine-base` decision 5 and
   DX decisions 9 and 9b).
3. **Agents own the machine; skills own their language packages** (restates DX 9a). A
   marketplace skill never requires an image rebuild; it declares `requiresRuntime`.
4. **The image is pinned in the agent version** (`AgentSpec.image_ref`), not in the App.
5. **No `imageRef` means the standard base.** Operators choose which images a node holds,
   not what "no imageRef" means. Migration: agent versions released before a node's I2
   upgrade keep the node's I1 default image (`legacy_default_image`, design §3.2).
6. **Skill packages are fetched, never built, on the host.** Wheels only with hashes, JS
   with scripts off, every file checked against the lockfile; a source build or install
   script is refused at release, naming the package.
7. **Built images live on the node; recipes travel.** Marketplaces carry recipe, lock and
   expected package list; each node builds and checks the resolved list. No registry.
8. **Community recipes never build on the host.** VAM's curated recipes keep building with
   Docker through `vam node init` (as `release-artifacts` decision 13); any other recipe
   builds only in the builder microVM, egress limited to the package mirrors.
9. **A node builds only from a trusted marketplace listing at its pinned tag, or from a
   draft uploaded by a member with publish rights.** Never from a bare URL.
10. **Vamfile Lite is a strict subset of the full Vamfile**; `VM` joins the full grammar.
11. **The host picks the VM size**: deployment request, raised to the image floor, rounded to
    the node's size classes, capped by the operator. Until DX-17's environment profile exists
    the request is read from `AgentSpec.resource_requirements` (vCPU, memory). This reads a
    field DX locked decision 17 moves later; it does not reverse 17 (owner to confirm, design
    open question 4).
12. **The machine is called a "VAM image"** (Q12); "environment" stays staging and prod.
13. **The full Vamfile with `RUN` is built only when a real client is blocked** by I1 to I3
    (Q14), named in this tracker.

## Status legend

- `[ ]` not started
- `[~]` in progress
- `[x]` done (code + tests, or merged)

## Phases

### Done phases

Done phase sections are deleted; git history keeps them.

| Phase | Name | Done |
|---|---|---|

### Phase 1 — Curated bases and skill packages (I1)

**Goal:** the node can hold `vam/python` and `vam/node` beside `vam/base:2`, each with its
own manifest and golden snapshot (baked in the background when the image is installed, if
design I1 question 10 is confirmed; evicted under a disk budget); `requiresRuntime` names
every held image; a skill's hashed Python or JS lockfile is fetched on the host, cached by
content hash and mounted with the skill (DX-101, 101A, 101B, 102, 105).  
**Depends on:** none. Touches none of the hot files (`run_start.rs`, `catalog_resolve.rs`,
`manager.rs`, agent spec proto): `golden.rs` grows image-parameterised `_for` variants and
keeps today's public functions as default-image wrappers, so `manager.rs:1469, 1739, 1919`
compile unchanged.  
**Plan:** breakdown drafted 2026-11-28 (below), owner to approve. Recommended as two phases,
**I1b skill packages first** (waves 0-2), then **I1a images and golden** (waves 3-5); see
open question 1.  
**Exit (I1b):** a skill with a fully hashed lockfile publishes and its Python tool imports
the package on `vam/base:2`, and a Bun tool imports an npm package; a second skill with the
same lockfile is served from the cache; a source-only package is refused when published,
naming it. **Exit (I1a):** `vam/python:1` and `vam/node:1` build (node on every lane run,
python on the lane's `base: python` dispatch) and boot; `GET /v1/node` lists every image with
its runtimes; the installed `vam/node:1` reports its golden snapshot baked with no run
started (only if question 10 is confirmed); golden restore median on the default image within
2x of ~217 ms (`machine-base` escalation rule).

- [x] VI-1.4, VI-1.5: lockfile reading and fetch-only, hash-checked package fetch into a
  content-addressed cache, through the operator's mirrors (security wave)
- [ ] VI-1.6, VI-1.7: packages resolved at publish, recorded on the package, linked into the
  staged skill; derived trees kept out of turn commits; `PYTHONPATH` in the guest
- [ ] VI-1.1: several images per node, `status.images[]`, refusals name every image
- [ ] VI-1.2: curated recipes `vam/python`, `vam/node`, pins, `vam node init --image`, lane
- [ ] VI-1.3: golden per (image, vCPU, memory), background bake queue, LRU eviction under
  `VAMD_GOLDEN_MAX_BYTES`
- [ ] VI-1.8: docs (skill lockfiles authoring page; node images, golden budget, mirrors)

#### Code map (checked on `f1c1431`)

- **Golden.** `golden.rs:118-124` reads `VAM_ROOTFS_PATH`; `resolve_restore_candidate`
  (`:451-495`) and `warmup_real_golden` (`:647-722`) read the env and `GOLDEN_VCPU`/`GOLDEN_MEM_MIB`
  (`:16-17`); the bake hard-codes the size (`:880`, `:1032-1034`) and its vm id
  `golden-bake-vm-<pid>` (`:690`), so a second concurrent bake would reuse the jail and vsock
  paths (`vam_core::jail::vsock_uds_path(&vm_id)`). `compute_golden_key` (`:263`) already takes
  rootfs, vCPU and memory. Callers outside `golden.rs`: `manager.rs:1469, 1739, 1919`,
  `preflight.rs:114`, `node_base.rs:154`, `bin/vamd.rs:585, 739`, `api/mod.rs:3680`
  (`image_digests`), `tests/hermetic_tests.rs:1278-1301` (constants). No golden directory is
  ever removed (only `discard_partial_bake`, `:607`): every rebuilt rootfs leaves its old key.
- **Bake timing and safety.** Startup awaits the bake before "Phase 4: Ready" and before
  `SessionManager` exists (`bin/vamd.rs:739-747`). The bake VM has no network device
  (`golden.rs:653` drops `net_pool`); vsock is Firecracker's per-VM Unix socket, so the fixed
  guest CID 3 (`:18`) inside a bake VM cannot collide on the host. Every key is baked on CID 3
  and a restore reserves it (`manager.rs:1490-1505`), so only one golden restore runs at a time
  node-wide (existing; matters in I2, noted there).
- **Base.** One `static BASE: OnceLock<LoadedBase>` (`node_base.rs:143`), loaded from the env
  and awaited at startup (`bin/vamd.rs:681-688`). `check_requires_runtime`/`check_against`
  (`:207-263`) and `check_interpreters` (`:287-327`) are reached from `api/mod.rs:1020`
  (`PUT /v1/skills`), `skill_package_admit.rs:44` (upload), `check_draft_skill`
  (`api/catalog_release.rs:91`) and `check_draft_package` (`api/skill_packages.rs:206`).
  Marketplace import never checks `requiresRuntime` (no `node_base` call under `marketplace/`).
  `NodeStatus.base = 15` (`proto/vam/v1/orchestrator.proto:102, 120-128`); `docs/api/openapi.yaml:4450`
  `NodeStatus` lacks `base`, `isolation` and `signingKeys` (drift). The proto baseline check
  only requires existing fields to stay (`crates/vam-spec/tests/conformance.rs:25-70`): an
  added field needs no baseline regeneration.
- **Skills are not a separate drive.** The guest mounts `/dev/vdc` read-only at
  `/mnt/drive9/skills` only if present (`vam-runtime/src/bin/guest_init/pid1.rs:150-159`); the
  host never attaches one. Skills are copied into the one data image
  (`vam-storage/src/workspace.rs:706-719`, `copy_dir_all` `:299-321`, which does not follow a
  symlinked directory), read-only only by ownership (`pid1.rs:171`). That image is
  content-sized up to the 1 GiB quota set in `manager.rs:1307-1311` and must fit
  `GOLDEN_DISK_BYTES` = 1 GiB for a restore (`golden.rs:19, 433-447`). The turn commit walks
  the whole image, skills included (`vam-storage/src/manifest.rs:142-197`,
  `workspace.rs:846-915`), and `skills_digest_from_dir` hashes every skill file on each
  materialize and commit (`workspace.rs:328, 745-749, 903-911`).
- **Guest exec.** `exec.rs:543-556` clears the env and sets `PATH` only; `resolve_dynamic`
  (`:86-200`) returns the interpreter and argv, not the skill directory; `node` resolves to
  `/usr/local/bin/node` (`:154`). Root is read-only (`vam-core/src/vmm.rs:24-38`), home and
  `/tmp` are tmpfs (`pid1.rs:114-129`): Python cannot write `__pycache__` beside a skill.
- **Lockfiles.** Nothing reads `requirements.txt`, `pylock.toml` or `package.json`; example
  skills ship `package.json` with empty `dependencies` (`examples/*/skills/*/package.json`).
  Upload is `admit_and_publish_skill_package_owned` (`skill_package_admit.rs:27-60`), release
  is `api/skill_packages.rs:190-215`, run staging is `resolve_one_skill_ref`
  (`skill_package_resolve.rs:60-140`). A skill's `.vam/connector.json` already lives beside its
  manifest (`docs/.../authoring/skill-manifest.md:254`).
- **Egress.** `is_restricted_ip` refuses private and loopback (`vam-security/src/ssrf_guard.rs:7-60`);
  callers pin with `PinnedUrl::validate_and_pin_with_policy` (`connector_discovery.rs:400-409`).
  Process test nodes reach loopback stand-ins only through `VAMD_PROCESS_*` seams
  (`process_mode.rs:25-48, 456-464`).
- **Recipes and lane.** `scripts/base/{Dockerfile,pins,resolve-pins.sh,check-recipe.sh}`;
  `build-rootfs.sh` accepts only `standard|minimal` (`:52-55`), reads facts for a fixed runtime
  list (`:120-131`), fails fast on `UNRESOLVED` pins (`:100-104`); the base Dockerfile deletes
  its snapshot apt config at the end (`:62-66`). `firecracker-assets.sh:30-33` and the lane's
  `base` input (`.github/workflows/rust.yml:30-37`) take `standard|minimal`; the lane resolves
  unresolved pins itself (`rust.yml:381-384`) and is short of disk ("Free runner disk").
  `vam node init` builds one base from a kit or checkout (`vam-cli/src/cmd/node_install.rs:478-578`)
  and writes `VAM_ROOTFS_PATH` into the env file (`:114-131`).
- **E2E.** One vamd per run, no per-scenario node env (`crates/vam-e2e/src/scenario.rs:28-50`);
  a process node gets its base manifest from a fixture (`run_cmd.rs:478-505`). So images and
  mirrors for scenarios are run-level fixtures, and "a node whose default is `vam/node:1`"
  cannot share a run with the standard-base scenarios.

#### Scenario delta

Re-derived from `guards`/`given` and the code map. Changes from the design's 6+1:
`skill-uses-node-and-npm-packages-on-the-node-base` (firecracker, default `vam/node:1`) is
replaced by a process scenario with a Bun tool on the standard base, because a shared run
cannot change the default image; `node-reports-each-image-it-holds` moves to process (fixture
manifests, like today's base fixture), and the firecracker proof that a curated image boots
is the install-time bake (conditional on Q10); the source-only refusal happens when the
package is published (uploaded), because draft runs need their packages too and
`publish-skill { refused }` judges the upload.

Run-level fixtures (W0): `e2e/world/fixtures/package-mirror/` (a `file://` mirror: PEP 691
JSON index and files for a tiny pure-Python wheel, an sdist-only project, an npm tarball),
passed to every node as `VAMD_PACKAGE_MIRRORS`; `e2e/world/fixtures/images/` (manifests for
`vam/python:1` and `vam/node:1`, no Java in any) passed to a process node as
`VAMD_IMAGES_DIR` (the runner already passes `VAM_BASE_MANIFEST` the same way). Unknown env
is ignored until the waves read it.

| # | Scenario | Change | Layer | Given | Red at W0 (class `product`) | Green by |
|---|---|---|---|---|---|---|
| S1 | `skill-imports-a-package-from-its-hashed-lockfile` | new | process | `org-admin`, `agent-with-python-lockfile-skill` (new), `skill-with-python-lockfile` (new: `requirements.txt`, every line hashed, python3 tool) | `then`: tool result lacks the marker the package computes (import fails) | W2 |
| S2 | `skill-imports-an-npm-package-from-its-lockfile` | new | process | `org-admin`, `agent-with-js-lockfile-skill` (new), `skill-with-js-lockfile` (new: `package.json` + `package-lock.json` v3, bun tool) | `then`: tool result lacks the marker (module not found) | W2 |
| S3 | `skill-with-a-source-only-package-is-refused-when-published` | new | process | `org-admin`, `skill-with-sdist-lockfile` (new, `deferred`) | `then.1`: `publish-skill { refused: true, contains: [<project name>, source] }` gets 2xx | W2 |
| S4 | `second-agent-with-the-same-lockfile-reuses-the-cached-packages` | new | process | `org-admin`, `skill-with-python-lockfile`, `skill-with-the-same-python-lockfile` (new), two agent pieces | `then`: second package's record lacks `packages.source: cache` (`api-call contains`); both tools import | W2 |
| S5 | `node-reports-each-image-it-holds` | new | process | `org-admin` | `when`: `status.images` absent; expects each image's name, runtimes (node on `vam/node:1`), the default flagged, `status.base` unchanged | W3 |
| S6 | `skill-needing-a-runtime-the-base-lacks-is-refused-at-install` | changed: `contains` adds `vam/node:1` (the refusal names every held image) | process | as today | `then.2`: refusal names only the base | W3 |
| S7 | `an-installed-image-is-baked-before-its-first-run` | new, **only if Q10 is confirmed** (else `an-installed-image-boots-before-it-is-reported-ready`, the same bake used as a boot check, snapshot discarded) | firecracker | `org-admin`; lane holds `vam/node:1` in `VAMD_IMAGES_DIR` | `when`: `status.images[name=vam/node:1]` absent; expects `verified: true`, node runtime present, golden `baked` within the step budget, no run started | W5 |
| — | `skill-uses-python-pandas-and-git-on-the-standard-base`, `node-reports-the-standard-base-it-boots` | unchanged (guards gain `golden.rs`, `node_base.rs`, `scripts/images/**`) | firecracker | as today | stay green | — |

Unit tests stand in for: every lockfile refusal (no hash, URL/git/path dep, install script,
yanked format), wheel tag choice, archive escapes, SSRF and redirect refusals, cache key
inputs, LRU order, bake queue serialisation.

#### Wave 0 "scenarios" (Sonnet low, 75 min, merges first)

S1-S7 as `pending` with `expectFail`, the new pieces, the two run-level fixtures and the
runner lines that pass them (`run_cmd.rs` `node_env`, beside `BASE_MANIFEST_FIXTURE`; the lane
side of S7 arrives in W4), README entries. Gate: `cargo run -p vam-e2e -- check`;
`scripts/e2e/run.sh --scenarios S1..S6` each red at the named step, class `product`; S7 is
checked by the lane in W4 (CI line `lane=yes`). If split (Q1), W0 lands S1-S4 with I1b and
S5-S7 with I1a. Risks: `{{urn:piece}}` inside an `api-call` path may need a one-line runner
extension; the wheel and tarball fixtures are committed small binaries whose hashes the
lockfiles carry; S2 needs Bun at `/usr/local/bin/bun` on the process lane (the lane installs
it, `rust.yml:209-213`).

#### Items

**I1b, skill packages**

- **VI-1.4 Lockfile reading** (new module `crates/vam-orchestrator/src/skill_deps/`, pure,
  no I/O). Python: `requirements.txt` where every requirement is `name==version` with
  `--hash=sha256:` (pip-compile / `uv export --generate-hashes`), and PEP 751 `pylock.toml`;
  JS: `package.json` + `package-lock.json` v2/v3 (production entries only). Refused, naming the
  package: a requirement without a hash, a URL, git, path or editable requirement, an sdist-only
  match, `hasInstallScript: true`, `uv.lock`, `bun.lock(b)`, `yarn.lock`, `pnpm-lock.yaml`
  (each with the export hint), a lockfile next to a shipped `node_modules/`, a `package.json`
  with non-empty `dependencies` and no lock (DX-101A). Markers: a small evaluator
  (`python_version`, `sys_platform`, `platform_machine`, `implementation_name`, `extra`).
  Wheel tags from the target image: `cp3XX`/`abi3`/`none`, `manylinux_2_N` with N at most the
  image's glibc, `x86_64`; `linux_x86_64` and musllinux refused. Target ABI from the default
  image's manifest (python minor from `runtimes.python`; glibc from a new manifest fact, else
  2.41 for `vam/base:2`). Tests: unit tests per rule.
- **VI-1.5 Fetch and cache (security).** `VAMD_PACKAGE_MIRRORS` (`pypi=<simple index>`,
  `npm=<registry>`; default `https://pypi.org/simple` with `files.pythonhosted.org` as its file
  host, `https://registry.npmjs.org`; `file://` directories for air-gapped nodes and tests).
  The lockfile chooses what (name, version, hash), the operator chooses where: URLs inside a
  lockfile are never fetched; a wheel is found in the mirror's JSON index by its hash, an npm
  tarball at `<registry>/<name>/-/<file>.tgz` and checked against `integrity`. HTTP through
  `PinnedUrl` with the strict policy, no redirect off the mirror hosts, no proxy env,
  timeouts, per-file and per-skill byte caps. Unpack only after the hash matches: wheels
  (zip) unpacked into `python/` (purelib/platlib; `.data/scripts` dropped), tarballs under
  `node_modules/<path>`; refuse absolute paths, `..`, links leaving the tree, device files;
  strip setuid; file-count cap. Cache `<drive9 root>/deps/<key>/`, key = sha256(lockfile
  bytes, ecosystem, target ABI, fetcher version), built in a temp dir and renamed into place,
  with a `deps.json` (files, bytes, packages). No package code runs (decision 6): no
  installer binary is shipped or executed. Tests: unit tests with a `file://` mirror and a
  local HTTP test seam; refusal texts.
- **VI-1.6 Admission and record.** Upload (`skill_package_admit.rs`, after
  `check_requires_runtime`) resolves the lockfile, so drafts and draft runs have their packages
  and a bad lockfile fails fast; release (`api/skill_packages.rs:206`) checks the cache and
  re-resolves on a miss; marketplace import resolves the same way at its one skill call site
  (`marketplace/import.rs:300-318`) and gains the `requiresRuntime` check it lacks. The package
  record gains `packages { ecosystem, count, bytes, cacheKey, source: mirror|cache }`
  (`proto/vam/v1/skills.proto` `SkillPackageRecord`, not the agent spec proto; OpenAPI).
  Caps (Q5) refused at upload.
- **VI-1.7 Mount and guest.** `resolve_one_skill_ref` hard-links the cached tree into the
  staged skill (`<skill>/.vam/python/`, `<skill>/node_modules/`; copy if another filesystem)
  and writes `<skill>/.vam/deps.key`; a cache miss at run start re-fetches (Q11); the agent's
  total is checked against the per-agent cap before staging. `vam-storage`: `copy_dir_all`
  hard-links where it can; `skills_digest_from_dir` skips the derived trees and reads
  `deps.key` instead; the turn commit (`build_from_image`, `build_from_dir`) skips
  `skills/*/.vam/python/**` and `skills/*/node_modules/**` (host-authoritative, re-staged
  every run). Guest `exec.rs`: `resolve_dynamic` also returns the skill directory; a python
  tool gets `PYTHONPATH=<skill>/.vam/python` when it exists and
  `PYTHONPYCACHEPREFIX=/tmp/pycache`; Bun and Node find `node_modules` beside the tool.
  Tests: unit tests in all three crates; S1, S2, S4; `lane=yes` (guest change).

**I1a, images and golden**

- **VI-1.1 Image store and report.** `node_base.rs`: the `OnceLock<LoadedBase>` becomes a
  default image plus a list of held images read from `VAMD_IMAGES_DIR` (default
  `/var/lib/vam/images`, one directory per image holding `rootfs.ext4`, `rootfs.base.json`,
  `rootfs.packages.txt`; identity from the manifest; an entry equal to the default by digest is
  the default). Extra images are hashed and verified off the startup path (status `verifying`
  until done). `check_against`/`check_interpreters` keep their signatures and callers: the
  **default image decides** (runs boot only the default in I1, `manager.rs:1919`), and the
  refusal names every held image and whether it would satisfy. Proto: `message NodeImage`
  (name, version, digest, verified, runtimes, `default`, `golden` state) and
  `NodeStatus.images = 17`; OpenAPI adds `images` and the missing `base`, `isolation`,
  `signingKeys`. Tests: unit; S5, S6.
- **VI-1.2 Curated recipes, node init, lane.** `scripts/images/python/Dockerfile` and
  `scripts/images/node/Dockerfile` (`FROM` the locally built `vam/base:2` build image,
  re-adding the snapshot apt config at the same pins); `build-rootfs.sh --image base|python|node`
  with per-image facts (python, node, bun, glibc) and name; pins `NODE_VERSION`,
  `NODE_SHA256` (official `linux-x64` tarball, installed to `/usr/local`, so `node` is at
  `/usr/local/bin/node`) in a pins file that `resolve-pins.sh` fills (unresolved from this
  sandbox: no route to nodejs.org or Debian; the lane resolves and prints them). `vam/python:1`
  adds `python3-scipy`, `python3-sklearn`, `python3-matplotlib` from the same snapshot,
  `MPLBACKEND=Agg`, bytecode compiled, the pip block kept (Q8); `vam/node:1` adds Node 24 LTS
  and npm with a global `npmrc` that keeps installs offline (checked to fail, like pip).
  `check-recipe.sh` runs on each recipe. `firecracker-assets.sh`: `VAM_BASE` also takes
  `python|node`; `VAM_EXTRA_IMAGES=node` builds `vam/node:1` into an images dir and exports
  `VAMD_IMAGES_DIR`; `rust.yml` `base` choices add `python`, `node`, the cache key adds
  `scripts/images/**`, the default lane builds the node image as an extra. `vam node init
  --image vam/python,vam/node`: builds each from the kit or checkout into
  `/var/lib/vam/images/<name>@<version>/`, `--check` reports each, env file gains
  `VAMD_IMAGES_DIR`; the bundle kit (`scripts/release/package.sh`, kit `manifest.json`)
  carries the recipes (`release-artifacts` decision 13 unchanged). Tests: CLI unit tests,
  existing `node-init-*` scenarios stay green, lane builds the node image, dispatch
  `base: python` boots.
- **VI-1.3 Golden per image and size (concurrency).** `golden.rs`: `GoldenTarget { rootfs,
  vcpu, mem_mib }`; `resolve_restore_candidate_for`, `warmup_golden_for`, `bake_into` sized by
  the target; `resolve_rootfs_path`, `resolve_restore_candidate`, `restore_from_golden`,
  `warmup_golden_snapshot`, `image_digests` and the constants keep their signatures as the
  default-image wrappers. A serial background bake queue (one bake at a time, started in
  `bin/vamd.rs` after Ready; the startup bake and soak stay as today) bakes each held image at
  the default size when it is found (Q10); vm id `golden-bake-vm-<pid>-<key8>` (unique jail,
  vsock and log paths; the lane's diagnostics prefix kept); no network device; a bake waits
  while the node is at its session cap. Eviction: least recently used under
  `VAMD_GOLDEN_MAX_BYTES` (default 8 GiB, allocated bytes), never the default key, never a key
  being baked; recency from a `last_used` stamp the restore lookup touches; keys of rootfs
  files the node no longer holds go first (the leftover pile today). `status.images[].golden`:
  `queued|baking|baked|failed|evicted|over_budget`. Tests: unit tests for queue, eviction
  order, wrappers equal to today; hermetic tests unchanged; S7; lane diagnostics show the
  default restore median.
- **VI-1.8 Docs.** New `docs/.../authoring/skill-packages.md` (lockfiles, refusals, caps,
  cache; sidebar in `docs/astro.config.mjs`), `skill-manifest.md`, `orchestrator/overview.md`
  (`VAMD_IMAGES_DIR`, `VAMD_GOLDEN_MAX_BYTES`, `VAMD_PACKAGE_MIRRORS`),
  `architecture/cold-start.md` (golden per image), `runtime/tools.md` (`PYTHONPATH`),
  `docs-features.yaml` rows with `proves`. Split across W2 and W3/W5.

#### Waves

Builds are serial. Each wave's `CI:` line lists its scenario ids.

| Wave | Items | Turns green | Repo | Model | Budget | Depends on | CI | Status | PR |
|---|---|---|---|---|---|---|---|---|---|
| 0 | scenarios, fixtures | — (S1-S6 red; S7 on the lane in W4) | vam | Sonnet low | 75 min | approval | `CI: scenarios=<S1..S6 ids>` | [x] | |
| 1 (sec) | VI-1.4, VI-1.5 | — (no scenario can go green: a lockfile is read only from `skill_package_admit.rs`, W2; S1-S4 stay red/pending, W1 is proven by unit tests on the W0 fixtures) | vam | Sonnet high: egress, SSRF pinning and archive extraction are the security floor | 120 min | 0 | `CI: tests=workspace` | [x] | |
| 2 | VI-1.6, VI-1.7, VI-1.8 (packages), upload refusal (S3) | S1, S2, S3, S4 | vam | Sonnet medium: three crates (orchestrator, storage, guest) and a proto field | 120 min | 1 | `CI: scenarios=<S1,S2,S3,S4> lane=yes` | [ ] | |
| 3 | VI-1.1, VI-1.8 (images) | S5, S6 | vam | Sonnet medium: new proto field and API | 90 min | 0 | `CI: scenarios=<S5,S6>` | [ ] | |
| 4 | VI-1.2 | — (lane builds `vam/node:1`; S7 still red for its golden field) | vam | Sonnet medium: scripts, CLI and CI together | 120 min | 3 | `CI: lane=yes` | [ ] | |
| 5 (concurrency) | VI-1.3, VI-1.8 (golden) | S7 | vam | Sonnet high: VMM lifecycle and a background queue beside live runs | 120 min | 4 | `CI: scenarios=<S7> lane=yes` | [ ] | |

Waves 0-2 are I1b, waves 3-5 are I1a (Q1). No wave edits a hot file.

#### Open questions (each with a default)

1. **Split the phase?** Default: yes. I1b (skill packages, W0-W2) first: it has value at once
   on the standard base. I1a (images, golden, W3-W5) next; its extra images only serve runs
   once I2 lands, which waits for Apps P4.
2. **Q10, bake at install, in I1?** Default: yes, for the default size when the node finds an
   image (startup or `vam node init`); it is also the proof that the image boots. Release and
   App-create triggers stay in I2.
3. **Golden budget.** Default `VAMD_GOLDEN_MAX_BYTES` 8 GiB of allocated bytes (a key at
   512 MiB costs about its memory file plus a few MiB of sparse disk, so about 14 keys); the
   default key is kept even over budget, with a warning.
4. **Lockfile formats.** Default: `requirements.txt` fully hashed and `pylock.toml`;
   `package-lock.json` v2/v3. `uv.lock` and `bun.lock` refused with an export hint
   (`uv export --format pylock.toml`, `npm install --package-lock-only`); yarn and pnpm refused.
   The design's `bun.lock` waits for a request.
5. **Caps.** Default: 384 MiB of packages per skill, 512 MiB per agent, refused at upload or
   staging. The design's 2 GiB drive cannot fit: skills share the 1 GiB data image that a
   restore needs (raising it is `manager.rs`, stop-and-ask).
6. **Mirrors.** Default: public PyPI and npm, public addresses only (strict SSRF policy);
   `file://` mirrors for air-gapped nodes. An HTTP mirror on a private address is refused
   (stop-and-ask).
7. **Node version.** Default: Node 24 LTS (maintained to April 2028) instead of the design's 22
   (end of life April 2027); the pin is confirmed when resolved.
8. **Python libraries in `vam/python:1`.** Default: Debian `scipy`, `scikit-learn`,
   `matplotlib` on top of the base's pandas and numpy. Drop `pyarrow` and `polars` (not in
   trixie; a skill lockfile brings them, which the I1b exit proves with polars) and `uv` (no
   installs in the guest, Q13).
9. **Installer.** Default: VAM's own fetch-and-unpack in Rust, not shipped `uv`/`bun`
   binaries as the design says: they make their own HTTP calls (no SSRF pinning) and would be
   two more pinned binaries in the kit. DX-101's `npm ci`/`.venv` table is superseded.
10. **`requiresRuntime` with several images in I1.** Default: the default image decides
    (runs boot only it); the refusal names every image. "Any held image passes" (design
    §3.1) waits for I2, where the agent's image decides.
11. **When packages resolve.** Default: at upload (draft) and marketplace import, checked at
    release; a run start that finds the cache empty re-fetches (only after a wipe; no cache
    GC in I1).
12. **Lane.** Default: every lane run builds `vam/node:1` as an extra image; `vam/python:1`
    builds on the `base: python` dispatch and in the release build check (runner disk).
13. **Bakes beside live runs.** Default: one at a time, deferred while the node is at its
    session cap.

#### Stop and ask (do not do inside a wave)

- Raising the 1 GiB session data image or `GOLDEN_DISK_BYTES`, or attaching a separate
  read-only packages drive (`/dev/vdc`): `manager.rs` and the golden drive layout. I2 or later.
- A run-start `requiresRuntime` check, per-run image choice, or leases that protect a key a
  run is restoring from eviction: `run_start.rs`, `catalog_resolve.rs`, `manager.rs`. I2.
- Any edit to the agent spec proto, or a check in the agent release arm of
  `api/catalog_release.rs` (agent access edits that path): the per-agent cap is enforced at
  staging instead.
- Allowing private-address HTTP mirrors (relaxing the SSRF policy).
- Editing `marketplace/import.rs` while agent access (AA-5.3 edits the same file) is unmerged:
  coordinate the merge order; if it conflicts, defer that call site and rely on the run-start
  re-fetch.
- Any change to locked decisions 2, 6 or 8 beyond the installer wording in Q9.

### Phase 2 — Image artifact and per-run choice (I2)

**Goal:** `image` artifact kind and `urn:vam:image:…`; `AgentSpec.image_ref`; each run boots
its agent version's image at a host-chosen VM size inside operator caps; agent release checks
`requiresRuntime` against the agent's image; the receipt and `RunStatus` name the image.  
**Depends on:** Phase 1; Apps and Spaces Phase 4 merged (one `start_run` resolution) and the
hot files handed over (Q16); agent access released to `main`.  
**Plan:** not drafted  
**Exit:** on one node, an agent with `imageRef: vam/node:1` runs on it and one without runs
on `vam/base:2`, each restoring from its own golden snapshot on the second run; the receipt
names the image; a request above the cap runs at the cap and records `vm.clamped: true`; an
App pinned to an agent version keeps its image when a newer image is released; an agent version
released before I2 keeps the node's old default image.

- [ ] `ArtifactKind::Image`, image package (`image.json`), curated images registered as
  released under the Squrell domain, short-form `imageRef` resolution
- [ ] `ResourceRef image_ref` on `AgentSpec`; inherited through `extends`; release refuses a
  missing image or an unmet `requiresRuntime`
- [ ] Per-run rootfs and golden key in `manager.rs`; typed refusal for an image no longer held
- [ ] VM size resolution: request, image floor, size classes (`VAMD_VM_SIZES`), cap
  (`VAMD_VM_MAX`)
- [ ] Receipt and run status: image URN, version, package digest, node rootfs digest, VM size
- [ ] `legacy_default_image` recorded on the first I2 start; pre-I2 agent versions without
  `imageRef` boot it; release warns on a drop to the standard base

**Scenario delta** (draft)

| Scenario | Change | Layer | Given | Why |
|---|---|---|---|---|
| `agent-with-an-image-ref-runs-on-that-image` | new | firecracker | two-image node, `agent-on-node-image` (new) | the exit |
| `agent-without-an-image-ref-runs-on-the-standard-base` | new | firecracker | two-image node, `agent-with-builtins` | decision 5 |
| `agent-naming-an-image-the-node-lacks-is-refused-at-release` | new | process | `org-admin`, `agent-on-missing-image` (new) | fail at release |
| `skill-needing-a-runtime-the-agents-image-lacks-is-refused` | new | process | `org-admin`, `skill-needs-java` | check moves to the agent's image |
| `run-gets-the-vm-size-it-asked-for-within-the-cap` | new | firecracker | node with `VAMD_VM_MAX=2x1024` | decision 11 |
| `receipt-names-the-image-the-run-booted` | new | process | `org-admin`, `agent-on-node-image` | receipt |
| `app-keeps-its-image-when-a-newer-image-is-released` | new | process | Apps P2 pieces | decision 4 |
| `agent-released-before-i2-keeps-the-nodes-old-default-image` | new | firecracker | node defaulting to `vam/node:1`, pre-I2 `agent-with-builtins` | decision 5 migration |

**Open questions** (each with a default)

- Owner question: does honouring `resource_requirements` conflict with DX locked
  decision 17 (fields move to an environment profile)? Default: honour it (vCPU, memory)
  within caps; the profile wins once it exists (design open question 4).

### Phase 3 — Vamfile Lite (I3)

**Goal:** a declarative recipe (FROM a `vam/*` image or one released in the same
marketplace, `RUNTIME`, `PACKAGES apt|pip|npm` exact
and hashed through `Vamfile.lock`, `COPY`, `ENV`, `LABEL`, `VM` floor, `TEST`; no `RUN`)
builds in a builder microVM with egress limited to the package mirrors, produces ext4 with
`vam-runtime` injected, an SBOM and a package list, and is admitted only if TEST passes and
the list matches. Contributed and installed as `vam-marketplace` listings of kind `image`,
pinned by tag; org-private drafts by members with publish rights.  
**Depends on:** Phase 2.  
**Plan:** not drafted  
**Exit:** a Lite listing at a pinned tag builds on a node and an agent with that `imageRef`
runs on it; a recipe with `RUN` or an unhashed package is refused before any build; a build
cannot reach a host outside the mirrors; a failing TEST admits nothing; marketplace CI rejects
a PR whose resolved package list drifts from `packages.txt`.

- [ ] Grammar parser and linter (`vam image check`), `vam image lock`
- [ ] Builder microVM (jailed Firecracker, mirrors-only egress, output drive), host ext4
  conversion with fixed UUID, seed and time, runtime injection, SBOM
- [ ] TEST in a fresh VM without network; package-list comparison; admission
- [ ] Marketplace `image` artifact kind (import at tag, digest recorded); org-private drafts
  (`vam image build`, `vam image release`); per-node build queue, timeout, disk quota
- [ ] `vam-marketplace` CI steps (check, Docker preview build on the runner, TEST, list
  diff) and contribution docs (in that repository)
- [ ] `docs/…/build/` pages updated to Lite (drop `ENTRYPOINT`, `apk` to `apt`)

**Scenario delta** (draft)

| Scenario | Change | Layer | Given | Why |
|---|---|---|---|---|
| `image-listing-builds-on-the-node-and-an-agent-runs-on-it` | new | firecracker | `marketplace-with-image-listing` (new) | the exit |
| `vamfile-lite-with-a-run-step-is-refused-before-building` | new | process | `image-draft-with-run` (new) | no `RUN` in Lite |
| `vamfile-lite-with-an-unhashed-package-is-refused` | new | process | `image-draft-without-lock` (new) | lock rule |
| `image-build-cannot-reach-a-host-outside-the-mirrors` | new | firecracker | `image-draft-that-fetches` (new; probes from inside the builder VM during the build) | builder egress |
| `image-whose-test-fails-is-not-admitted` | new | firecracker | `image-draft-failing-test` (new) | TEST gates admission |
| `image-listing-with-a-moved-tag-is-refused` | new | process | `marketplace-with-image-listing` | pin rule |
| `node-refuses-to-build-a-recipe-from-a-bare-url` | new | process | `org-admin` | decision 9 |

**Open questions** (each with a default)

- Marketplaces that may supply images: any the org trusts, labelled Unverified unless
  Squrell (design open question 5).

### Phase 4 — Full Vamfile (I4, gated)

**Goal:** `RUN`, `ARG`, multi-stage `AS`, `BUILD_EGRESS`, `SECRET` (build-time mount only),
`HARDEN`, and the app's Machine step (E7 Phase 4), on the same builder and admission.  
**Depends on:** Phase 3, and a named client blocked by Phases 1 to 3 (decision 13).  
**Plan:** not drafted  
**Exit:** to be set when the client is named; at least `vamfile-run-step-runs-only-in-the-builder-vm`,
`run-step-reaches-only-its-build-egress-hosts`, `build-secret-is-not-in-the-image`.

- [ ] Blocked client named here, with what Phases 1 to 3 could not do

## Explicitly later

- Publishing built images (OCI/ORAS) after a licence review.
- `WARM` snapshots per image; image signing with E6.
- `storage_gib` and `network_mode` from `resource_requirements`.
- An image picker in Squrell-App beyond the Machine step.

## Progress log

| Date | Phase | What landed (end with the scoreboard: scenarios green, red, fix cycles, wall time) |
|---|---|---|
| 2026-11-28 | — | Tracker opened from `design/vam-image.md` (proposed; owner to confirm decisions and Q12-Q14) |
| 2026-11-28 | 1 W0 | Scenarios S1-S7, pieces, package-mirror and image fixtures, runner env landed. S6 is a new sibling `skill-refusal-names-every-image-the-node-holds` (the active original is cited as docs proof). Scoreboard: S1-S6 red at the named step, class product (0 green, 6 red right reason), S7 pending for the lane; 0 fix cycles |
| 2026-11-28 | 1 W1 | VI-1.4 lockfile reading and VI-1.5 verified fetch and cache landed as `crates/vam-orchestrator/src/skill_deps/` (pure parsers for requirements.txt, pylock.toml, package-lock.json v2/v3; `VAMD_PACKAGE_MIRRORS` https or `file://` only; guarded HTTP with per-hop pin, allowlist, no proxy; link-free wheel and tgz unpackers; content-addressed cache with atomic publish). Owner decisions applied: S3 and the upload refusal move to W2; yanked files and `http://` mirrors refused; W0 runner builds the mirror URL with `Url::from_directory_path` (comma written `%2C` by hand, since the URL crate leaves it raw). Scoreboard: 0 scenarios green (none can turn green in W1), S1-S4 stay red/pending, 162 unit tests green, 0 fix cycles beyond the first test run |

> Coordination note (2026-11-28): Apps P0 claims proto fields Space.app_id=8, RunSpec.app_id=14, RunStatus.app_id=24, CallerContext.app_id=11 and a new App message after ListSpacesResponse in orchestrator.proto. I1 adds only NodeStatus.images=17 (check NodeStatus numbers are still free at build time).
