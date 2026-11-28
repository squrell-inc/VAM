# VAM image: the machine an agent runs on

Status: **proposed** (2026-11-28). Lane 2 of the next-lanes plan. Tracker:
`.cursor/workstreams/vam-image.md` (slug `vam-image`, base branch `feat/vam-image`).
This doc narrows E7 (`design/agent-experience.md` Part III, lines 448-536) into four
phases, I1 to I4, and says which E7 pieces wait for a blocked client. Read the section
for the phase in hand only.

## What we are building

A **VAM image** is the root filesystem a run boots, plus the facts about it: its name and
version, the runtimes it provides, its VM size floor, and its digest. Today a node has
exactly one image and every VM gets the same size. After this lane:

1. **I1.** VAM ships more curated bases (`vam/python`, `vam/node`) next to `vam/base:2`. A
   node can hold several images, each with its own manifest and its own golden snapshot,
   baked when first needed and evicted when disk runs short. A skill brings its own
   language packages from its native lockfile; the host fetches them by hash, caches them by
   content hash and mounts them read-only. Nothing installs inside the guest.
2. **I2.** An image is a catalog artifact (`urn:vam:image:…`). An agent names one with
   `imageRef`; with none it gets the standard base. Each run boots the image its agent
   version pins, at a VM size inside the operator's caps, and the receipt records which
   image ran.
3. **I3.** **Vamfile Lite**: a declarative recipe (FROM a VAM base, pinned packages, a
   runtime, a smoke test, a VM floor; no `RUN`). The node builds it in a throwaway builder
   microVM whose only egress is the package mirrors. Recipes are contributed and installed
   as `vam-marketplace` listings, pinned like every other listing.
4. **I4.** The full Vamfile with `RUN`, only when a real client is blocked without it.

## Why

- **Every agent shares one machine.** The rootfs path comes from one environment variable
  (`crates/vam-orchestrator/src/golden.rs:118-124`, read at session start in
  `crates/vam-orchestrator/src/manager.rs:1919`). Every VM is 2 vCPU and 512 MiB
  (`manager.rs:1928`, `.with_limits(2, 512)`). A data agent that needs 2 GiB and a docs
  agent get the same box.
- **What an agent asks for is ignored.** `AgentSpec.resource_requirements`
  (`proto/vam/v1/agentic_spec.proto:42`, message at `proto/vam/v1/model_policy.proto:93-98`)
  is read nowhere in the orchestrator; the only Rust use is `None` in
  `crates/vam-spec/examples/generate_baseline.rs:81`. `AgentSpec` has no image field
  (`agentic_spec.proto:14-65`).
- **Skills cannot bring packages.** DX Phase 9 says a skill that cannot bring its own
  dependencies is not installable (`design/developer-experience.md` §19.1, line 1087;
  tracker `.cursor/workstreams/developer-experience.md` DX-101 to DX-105, lines 1198-1288).
  Nothing reads a skill's `package.json` (DX-101A). The base blocks `pip install` on purpose
  (`scripts/base/Dockerfile:51-59`), so a skill needing `polars` has no path today.
- **The parts already line up.** Golden snapshots are already stored per key, and the key
  hashes kernel, rootfs, root-drive layout tag, vCPU and memory
  (`golden.rs:247-330`; files under `<drive9>/golden/<key>/`, `golden.rs:359-366`), so
  several images can coexist on disk. Only the node's one rootfs is baked, at startup
  (`docs/src/content/docs/architecture/cold-start.md`, "Bake"). The base manifest
  `<image>.base.json` and the `requiresRuntime` check exist (`crates/vam-orchestrator/src/
  node_base.rs:1-20, 205-260`; `proto/vam/v1/skills.proto:180-182`). The receipt already has
  `ReceiptImage.rootfs_digest` (`proto/vam/v1/receipt.proto:112-115`).

## Name

"Environment" already means staging or prod: approved C2 (one App per environment) and DX
Phase 8 (`.cursor/workstreams/developer-experience.md:1174-1185`). This doc calls the machine
a **VAM image** everywhere: CLI (`vam image …`), API (`imageRef`, `status.images`), catalog
kind (`image`), docs. Proposed as Q12.

## Locked decisions (proposed, owner to confirm)

Do not re-litigate without editing this list. Decisions 1 to 3 restate locked decisions
of other trackers and are not new.

1. **PID 1 is always `vam-runtime`** (E7 rule 1, `agent-experience.md:454-457`;
   `machine-base` decision 4). Every image, curated or built, gets it injected last as
   `/sbin/init`.
2. **No package installs inside the guest, ever** (Q13). Packages enter an image at build
   time (E7 rule 2; `machine-base` decision 5, `.cursor/workstreams/machine-base.md:51`) or a
   skill's read-only mount before boot (DX locked decisions 9 and 9b,
   `developer-experience.md:53-63`; DX-105). The root drive stays read-only
   (`crates/vam-core/src/vmm.rs:27-33`), scratch stays tmpfs
   (`crates/vam-runtime/src/bin/guest_init/pid1.rs:116-128`), and nothing a run writes
   becomes part of a later run's machine.
3. **Agents own the machine; skills own their language packages** (DX 9a, DX-101C). A
   marketplace skill never requires an image rebuild; it declares `requiresRuntime`.
4. **The image is pinned in the agent version, not in the App.** `imageRef` is an
   `AgentSpec` field, so releasing a new image changes nothing until a new agent version
   names it. An App pins an agent version (C1), so it gets a fixed machine with no App field.
5. **No `imageRef` means the standard base** (E7 rule 3). An operator chooses which images
   a node holds, not what "no imageRef" means. One migration exception (§3.2): agent
   versions released before a node's I2 upgrade keep the node's I1 default image.
6. **Skill packages are fetched, never built, on the host.** The host runs no package code:
   Python is wheels only with `--require-hashes`, JS installs with scripts off, every file is
   checked against the lockfile's hash. A skill whose lockfile needs a source build or an
   install script is refused at release with the package named.
7. **Built images live on the node; recipes travel.** Git marketplaces carry recipes,
   locks and expected package lists, never ext4 files. Each node builds the image itself
   from pinned inputs. No OCI registry in this lane (section 2).
8. **Community recipes never build on the host.** VAM's own curated recipes keep building
   with Docker on the host through `vam node init`, as today (`design/release-artifacts.md`
   §5.3). Any other recipe builds only in the builder microVM (I3), whose egress is the
   package mirrors and nothing else.
9. **A node builds a recipe only from a trusted marketplace listing at its pinned tag, or
   from a draft a member with publish rights uploads.** Never from a bare URL (section 7).
10. **Vamfile Lite is a strict subset of the full Vamfile.** Every Lite file stays valid
    when I4 lands. `VM` is added to the full grammar too.
11. **VM size is chosen by the host, inside operator caps.** The image sets a floor, the
    deployment asks, the operator caps. Sizes are a small fixed set so golden snapshots stay
    bounded. See section 3.2 for how this meets DX locked decision 17.

## 1. What main has today

| Area | Today | Where |
|---|---|---|
| Rootfs | One per node, `VAM_ROOTFS_PATH`; a VM node refuses to start without it | `golden.rs:118-124`, `manager.rs:1919` |
| VM size | Fixed 2 vCPU, 512 MiB for runs and for the bake | `manager.rs:1928`; `GOLDEN_VCPU`, `GOLDEN_MEM_MIB` at `golden.rs:16-17` |
| Golden key | sha256 of kernel, rootfs, layout tag, vCPU, memory | `golden.rs:247-330` |
| Bake | Once at startup, for the one rootfs, before the API answers | `golden.rs:622` `warmup_golden_snapshot`; `bin/vamd.rs:739` |
| Base manifest | `<image>.base.json` beside the rootfs; one global base | `node_base.rs:67-73`, `static BASE: OnceLock` at `node_base.rs:143` |
| `requiresRuntime` | Checked at skill release against the node's one base | `node_base.rs:205-260`, `skills.proto:180-182` |
| Standard base | `vam/base:2`: Debian trixie-slim by digest, snapshot apt, Python 3.13 + 6 libs, git, curl, jq, Bun; 704 MiB (526 used); cold boot ~249 ms, golden restore ~217 ms median | `scripts/base/Dockerfile`; `machine-base.md:194-196, 594` |
| Image shipping | No `rootfs.ext4` is published (GPL source duty); the node builds the base from the bundled recipe | `design/release-artifacts.md` §5.2-5.3; `release-artifacts` decision 13 |
| Image in spec | No image field; `resource_requirements` unread | `agentic_spec.proto:14-65`, `:42` |
| Artifact kinds | skill, agent, playbook, connector; no image | `crates/vam-spec/src/package.rs:60-68` |
| Receipt | `ReceiptImage { kernel_digest, rootfs_digest }`, filled from the node's rootfs file at run start | `receipt.proto:112-115`; `api/mod.rs:3676-3690` |
| Skill staging | Each skill package unpacked into `<session>/catalog-skills` before the skills drive is built | `crates/vam-orchestrator/src/skill_package_resolve.rs:32-58`; `manager.rs:1250-1290` |
| Marketplace | Git repo, listings at immutable per-listing tags, moved tag refused, admin trusts by URL, Squrell trusted, others "Unverified" | `marketplace/mod.rs:1-23, 250`; `marketplace/store.rs:28-50` |
| Marketplace limits | 128 MiB bare cache per marketplace, 16 MiB and 2000 files per exported artifact | `marketplace/fetch.rs:123-135` |
| Build crate | `crates/vam-build` has only `SKILLS.md` (digest-pinned inputs, isolated builder, rootless, reproducible, no secrets in layers, fail-closed admission); `docs/…/build/*.md` all `status: planned` | `crates/vam-build/SKILLS.md` |

Two older texts disagree with what shipped and are corrected here: E7's examples use `apk`
(`agent-experience.md:476, 487`), but the base is Debian (`machine-base` decision 1), so
`PACKAGES` names `apt`. E7 Phase 1 and the build docs plan ORAS distribution of built
images (`docs/src/content/docs/build/architecture.md:84-86`); decision 7 replaces that.

## 2. Where built images live

A curated base is about 700 MiB of ext4. Three options:

| Option | How | Verdict |
|---|---|---|
| A. ext4 blobs in the marketplace Git repo | Commit the image, or Git LFS | **Rejected.** The node's marketplace fetch caps a cache at 128 MiB and an exported artifact at 16 MiB (`fetch.rs:129-133`); Git is the wrong store for 700 MiB binaries; and publishing an image carries the GPL source duty `release-artifacts` decided not to take on |
| B. OCI registry via ORAS | Build once in CI, push ext4 as an OCI artifact, nodes pull by digest (the plan in `docs/…/build/architecture.md:84-111`) | **Not now.** Same licence problem (the registry redistributes git, glibc, busybox). Adds a second distribution and trust channel beside the marketplace, a registry to run, and pull credentials. Revisit after the licence review `release-artifacts` §8 defers, or if node-side builds become too slow |
| C. Recipe in the marketplace, node builds | The listing holds the recipe, its lock and the expected package list. Each node builds from pinned inputs and checks the result against that list | **Chosen.** Same model as the standard base today (`release-artifacts.md` §5.3). The marketplace keeps its pinning, review and trust rules unchanged. Nothing large crosses Git |

Why C holds up: byte-identical ext4 is not achievable today (host ctimes leak, so
`.packages.txt` and `.base.json` are the contract, `release-artifacts.md:53`). So the pin is
on the **inputs** (recipe digest, base digest, snapshot timestamp, package hashes) and on
the **resolved package list**. A node that builds a different package list from the same
recipe refuses the image. The node-local ext4 digest is recorded per run in the receipt.

Cost of C: each node builds each image once (minutes) and stores it. Mitigations: lazy
golden bakes (I1), build on install rather than on first run (I3), and a node can build
images in the background after an admin enables a listing.

## 3. Phases

| Phase | Delivers | Depends on | Touches hot files |
|---|---|---|---|
| I1 Curated bases and skill packages | `vam/python`, `vam/node`; several images per node; golden per image and size, lazy, evicted; `requiresRuntime` per image; DX-101/102 lockfile packages | nothing | no (golden.rs, node_base.rs, skill_package_resolve.rs, scripts, vam-runtime exec) |
| I2 Image artifact and per-run choice | `image` artifact kind, `imageRef`, per-run rootfs and VM size, receipt names the image | Apps P4 merged; agent access in `main` | yes: catalog, agent spec proto, `manager.rs`, `run_start.rs` |
| I3 Vamfile Lite | Grammar, lock, builder microVM, TEST, SBOM, marketplace `image` listings, org-private drafts | I2 | catalog import (marketplace), new `vam-build` code |
| I4 Full Vamfile | `RUN`, multi-stage, `BUILD_EGRESS`, `SECRET`, app Machine step | I3 and a named client blocked by I1 to I3 | builder only |

E7 mapping: I1 finishes E7 Phase 1 and adds DX-101/102; I2 is E7 Phase 2; I3 is a subset of
E7 Phase 3 plus the SBOM and marketplace parts of Phase 5; I4 is the rest of Phase 3 and
Phase 4 (`agent-experience.md:524-536`).

### 3.1 I1 — Curated bases and skill packages

**Goal.** More agents run with no build: the node can hold `vam/python` and `vam/node`, a
skill brings its Python or JS packages from its lockfile, and cold start stays fast.

**Curated bases.** Two recipes beside `scripts/base/`, in the same Dockerfile-inside-Vamfile
grammar, `FROM` the `vam/base:2` tree so they share layers:

| Image | Adds to `vam/base:2` | Runtimes it reports |
|---|---|---|
| `vam/python:1` | scipy, scikit-learn, matplotlib, pyarrow, polars (Debian snapshot packages where they exist; wheels by hash otherwise), uv | python 3.13 |
| `vam/node:1` | Node.js 22 LTS (tarball, sha256 pinned), npm | node 22, bun 1.x |

The breakdown may adjust the lists and must justify each (as `machine-base` decision 2 did).
`vam node init --image vam/python,vam/node` builds them; the bundle ships the recipes
(`release-artifacts` decision 13 unchanged). The CI lane's `base` input accepts each.

**Several images per node.** Images live under `<state>/images/<name>@<version>/` as
`rootfs.ext4`, `rootfs.base.json`, `rootfs.packages.txt`. `node_base.rs` loads every
manifest into a map instead of one `OnceLock` (`node_base.rs:143`). `VAM_ROOTFS_PATH` stays
and names the default image, so an existing node changes nothing. `GET /v1/node` keeps
`status.base` (the default) and adds `status.images[]` (name, version, digest, verified,
runtimes). In I1 runs still boot the default image; choosing per run is I2. An operator who
wants `vam/node` for every run points `VAM_ROOTFS_PATH` at it, as they can today.

**Golden per image and size.** `golden.rs` functions take `(image, vcpu, mem)` instead of
reading the env and the constants. Startup bakes only the default image at the default size,
as today. Any other key is baked on first use: that run boots cold (~250 ms) and a
background bake follows, so the next run restores. Eviction is least recently used under
`VAMD_GOLDEN_MAX_BYTES` (each key costs about its memory size plus the sparse data disk),
never the default key and never a key a restore is using. I1 builds and unit-tests this;
the lane exercises it once I2 selects images.

**`requiresRuntime` per image.** At skill release the check passes when any image on the
node meets it, and the refusal names the images that would (`… needs node >=22; this
node's images: vam/base:2 (no node), vam/python:1 (no node)`). At agent release and at run
start, each skill is checked against the image the run will use (in I1, the default). The
machine-base scenario's message changes accordingly.

**Skill packages from lockfiles (DX-101, DX-101A, DX-102, DX-105).**

| Skill ships | Host does | Lands in the skill's mount |
|---|---|---|
| `requirements.txt` with `--hash` lines, or `pylock.toml` / `uv.lock` | `uv pip install --target … --require-hashes --only-binary :all: --python-version <image python> --python-platform <image platform>` | `<skill>/.vam/python/` |
| `package.json` + `package-lock.json` or `bun.lock` | `bun install --frozen-lockfile --ignore-scripts --production` in a staging dir | `<skill>/node_modules/` |

- **No package code runs on the host** (decision 6). Wheels are unpacked, not built; JS
  install scripts are off. Installers are pinned binaries shipped with the node. A sdist, a
  missing hash or a package that needs a postinstall step is refused at release, naming it.
- **When.** Resolved at skill release or marketplace install, so a run never waits on the
  network; re-checked at run start only as a cache lookup. Fail fast, as DX 9b asks.
- **Cache.** Content-addressed under `<state>/deps/<key>/`, key = sha256 of the lockfile,
  the ecosystem, the target image's runtime ABI (python minor, platform tag) and the
  installer version. Two skills or two agents with the same lockfile share one copy. The
  skill's package digest does not change: deps are derived, not authored.
- **Mount.** `skill_package_resolve.rs` (`resolve_one_skill_ref`, line 60) links the cached
  tree into the staged skill, so the existing read-only skills drive carries it. The guest
  sets `PYTHONPATH` to `<skill>/.vam/python` for that skill's Python tools (a small change in
  `vam-runtime` exec); Bun finds `node_modules` beside the tool without help.
- **Egress.** The host fetches only from the operator's package mirrors
  (`VAMD_PACKAGE_MIRRORS`, default PyPI's file host and the npm registry) through the shared
  SSRF guard. An air-gapped node points them at internal mirrors.
- **Collisions.** Per-skill directories give isolation, so DX-103 (Nix) stays unbuilt, as DX
  decision 9 says.

**Exit.** `vam/python` and `vam/node` build on the lane and boot; a skill that declares
`polars` in a hashed lockfile releases and its tool imports it on `vam/base:2`; the same
lockfile on a second agent hits the cache; a source-only package is refused at release; the
node reports every image it holds; golden restore median within 2x of ~217 ms on the default
image (the `machine-base` escalation rule).

**Scenario delta (I1)**

| Scenario | Change | Layer | Given | Why |
|---|---|---|---|---|
| `skill-imports-a-package-from-its-hashed-lockfile` | new | process | `org-admin`, `package-mirror` (new: fixture wheels and npm tarballs served offline), `skill-with-python-lockfile` (new) | DX-101/102 exit: the tool imports a vendored package with no network in the guest |
| `skill-with-a-source-only-package-is-refused-at-release` | new | process | `org-admin`, `package-mirror`, `skill-with-sdist-lockfile` (new) | decision 6: nothing is built on the host |
| `second-agent-with-the-same-lockfile-reuses-the-cached-packages` | new | process | `org-admin`, `package-mirror`, `skill-with-python-lockfile` | DX-102: content-addressed cache; the run reports `deps: cached` |
| `node-reports-each-image-it-holds` | new | firecracker | `org-admin`, node with `vam/base:2` and `vam/node:1` | `status.images[]` lists both with runtimes; `status.base` unchanged |
| `skill-needing-a-runtime-the-base-lacks-is-refused-at-install` | changed: refusal names every image the node holds and which would satisfy | process | as today | `requiresRuntime` is per image |
| `skill-uses-node-and-npm-packages-on-the-node-base` | new | firecracker | node whose default is `vam/node:1`, `skill-with-js-lockfile` (new) | the curated base boots and a JS dep resolves |
| `skill-uses-python-pandas-and-git-on-the-standard-base` | unchanged (guard) | firecracker | as today | the default base is untouched |

### 3.2 I2 — Image artifact and per-run choice

**Goal.** An agent version names its machine and VM size request; every run boots exactly
that, and the receipt says so.

**Image artifact.** `ArtifactKind::Image` joins `crates/vam-spec/src/package.rs:60-75`, URN
`urn:vam:image:<domain>/<ns>/<name>`, E1 lifecycle (draft, release, digest pinned by `vamd`).
In I2 an image package holds `image.json` (name, version, `from`, runtimes, VM floor) and,
for a curated base, the recipe reference. The node's curated images are registered as
released images under the Squrell domain. Authoring JSON accepts the short form
`"imageRef": "vam/python:1"`, resolved to the URN and pinned like a skill ref.

**`imageRef` on the agent.** New `AgentSpec` field `ResourceRef image_ref = 24`. It is part
of the agent version, so a released version always boots the same image (decision 4).
Agent release refuses an `imageRef` the node does not hold or cannot verify, and refuses a
skill whose `requiresRuntime` that image does not meet (the check moves from "the node's base"
to "this agent's image"). `extends` inherits the parent's `imageRef` unless the overlay sets
one.

**Migration from a nonstandard default.** In I1 an operator may point `VAM_ROOTFS_PATH` at
`vam/node` (§3.1). On the first I2 start, `vamd` records the image `VAM_ROOTFS_PATH` names
(name, version, digest) as the node's `legacy_default_image`. An agent version released
before that start with no `imageRef` keeps booting `legacy_default_image`, so it and every
App pinned to it keep their runtime; a version released after it with no `imageRef` gets the
standard base (decision 5). Agent release warns when a new version drops to the standard
base from a previous version that ran on `legacy_default_image`, naming the `imageRef` to
set. A node whose default was already the standard base sees no change.

**Per-run rootfs.** `manager.rs:1919` stops reading the env: the run's resolved image gives
the rootfs path and golden key. A run pinned to an image the node has since evicted from disk
refuses with a typed error naming the image (no silent fallback to the default).

**VM size.** Resolution, first match wins, then clamped:

1. The deployment's request. Until the environment profile of DX locked decision 17 exists,
   this is `AgentSpec.resource_requirements` (`vcpu_count`, `memory_mib`). When the profile
   lands, the profile is read first and the agent field becomes the fallback.
2. Otherwise the node default (2 vCPU, 512 MiB).
3. Raised to the image's floor (`VM min-memory=…` in its manifest).
4. Rounded up to the node's size classes (`VAMD_VM_SIZES`, default `2x512,2x1024,4x2048`)
   and capped by `VAMD_VM_MAX`. A request above the cap runs at the cap and the run
   records `vm.clamped: true`; an image floor above the cap refuses at release.

Size classes keep golden keys bounded to images x classes. `storage_gib` and
`network_mode` stay out of I2. **This touches DX locked decision 17**, which moves
`resource_requirements` out of `AgentSpec` into an environment profile
(`developer-experience.md:135-151`). Reading the field where it lives today, until the
profile exists, does not reverse that decision; the owner should confirm (open question 4).

**Receipt and status.** The run records image URN, version and package digest, plus the
node-local rootfs digest it booted and the VM size. `record_run_isolation_fact`
(`api/mod.rs:3676`) takes the run's image instead of hashing the node's file.
`ReceiptImage` gains `image_urn`, `image_version`, `vcpu`, `memory_mib` (new field numbers;
`rootfs_digest` keeps its meaning). `RunStatus` shows the image.

**Exit.** An agent with `imageRef: vam/node:1` runs on it and one without runs on
`vam/base:2` on the same node; both restore from their own golden snapshot on the second
run; the receipt names the image; a request above the cap is clamped and recorded; an App
pinned to an agent version keeps its image when a newer image is released; an agent version
released before I2 on a node whose default was `vam/node` still boots `vam/node`.

**Scenario delta (I2)**

| Scenario | Change | Layer | Given | Why |
|---|---|---|---|---|
| `agent-with-an-image-ref-runs-on-that-image` | new | firecracker | node with `vam/base:2` and `vam/node:1`, `agent-on-node-image` (new) | the exit: guest facts report node 22, second run `bootPath: restore` |
| `agent-without-an-image-ref-runs-on-the-standard-base` | new | firecracker | same node, `agent-with-builtins` | decision 5 holds with several images |
| `agent-naming-an-image-the-node-lacks-is-refused-at-release` | new | process | `org-admin`, `agent-on-missing-image` (new) | fail at release, not at run |
| `skill-needing-a-runtime-the-agents-image-lacks-is-refused` | new (replaces the node-wide check for agents) | process | `org-admin`, `skill-needs-java`, agent on `vam/base:2` | `requiresRuntime` is checked against the agent's image |
| `run-gets-the-vm-size-it-asked-for-within-the-cap` | new | firecracker | node with `VAMD_VM_MAX=2x1024`, agent asking 4 vCPU / 2048 | guest reports 1024 MiB, run records `vm.clamped: true` |
| `receipt-names-the-image-the-run-booted` | new | process | `org-admin`, `agent-on-node-image` | receipt carries URN, version and digest |
| `app-keeps-its-image-when-a-newer-image-is-released` | new | process | Apps P2 pieces, `agent-on-node-image` | decision 4: the image moves only with the agent version |
| `agent-released-before-i2-keeps-the-nodes-old-default-image` | new | firecracker | node with `VAM_ROOTFS_PATH` at `vam/node:1`, `agent-with-builtins` released before the upgrade | migration from a nonstandard default |

### 3.3 I3 — Vamfile Lite

**Goal.** An agent author (or a community contributor) gets a machine with system packages
the curated bases lack, without anyone running arbitrary build code.

**Grammar (`vamfile-lite/1`).** One stage. Every line is also valid full Vamfile.

```
# syntax=vamfile-lite/1
FROM vam/python:1                         # vam/* or released in this marketplace; digest in the lock
RUNTIME java@21                           # from VAM's maintained set (python, node, java)
PACKAGES apt postgresql-client poppler-utils
PACKAGES pip pdfplumber==0.11.4
PACKAGES npm @mermaid-js/mermaid-cli@11.4.0
COPY files/acme-root.pem /usr/local/share/ca-certificates/acme-root.pem
ENV PGCONNECT_TIMEOUT=10
LABEL org.opencontainers.image.source=https://github.com/acme/agent-images
VM min-vcpu=1 min-memory=1024
TEST pdftotext -v && psql --version && java -version
```

| Directive | Rule |
|---|---|
| `FROM` | Exactly one; must name a VAM curated image (`vam/*`) or an image already released in the same marketplace. No arbitrary OCI image |
| `RUNTIME` | `<name>@<major>` from VAM's maintained set; recorded in the manifest's runtimes, which is what `requiresRuntime` checks |
| `PACKAGES` | First word is the manager: `apt` (Debian, from `snapshot.debian.org` at the lock's timestamp; no third-party repos), `pip` (wheels only, exact versions), `npm` (exact versions, scripts off). Installs system-wide for the agent's machine |
| `COPY` | Source inside the recipe directory, regular files only, total under the marketplace's 16 MiB export cap; destination not under `/sbin`, `/etc/vam`, `/usr/lib/vam` or the mount points |
| `ENV`, `LABEL` | Literal values; the linter refuses values that match secret patterns (the `.gitleaks.toml` rules) |
| `VM` | Floor only (`min-vcpu`, `min-memory`); the host still picks the size (I2) |
| `TEST` | One or more shell commands run in a fresh VM on the built image with no network; any non-zero exit fails the build |
| Refused | `RUN`, `ARG`, `SECRET`, `BUILD_EGRESS`, `USER`, `WORKDIR`, multi-stage `AS`, `ENTRYPOINT`, `CMD`, `EXPOSE`, `VOLUME`, `HEALTHCHECK`, `ONBUILD`, `WARM` |

**Lock.** `vam image lock` resolves the recipe into `Vamfile.lock`: base digest, snapshot
timestamp, every apt package with version and sha256 from the signed `Packages` index, every
wheel URL and sha256 for the image's platform, every npm tarball with its integrity hash. A
build refuses a recipe without a lock, or a lock that does not match. Authors write versions;
hashes are generated, never hand-typed.

**Pipeline.**

```
recipe + lock -> check (grammar, lock matches, FROM per the rule above)
  -> builder microVM: root = copy of the FROM image; egress = package mirrors only
     apt-get install (Debian-signed packages; maintainer scripts run inside the VM)
     pip install --require-hashes --only-binary :all:  |  npm install --ignore-scripts
     RUNTIME install from VAM's pinned tarballs; COPY from a read-only input drive
  -> tree out over a second drive -> host: mkfs.ext4 -d (fixed UUID, hash seed, time)
  -> inject vam-runtime as /sbin/init (last) -> manifest, packages.txt, SBOM (SPDX)
  -> TEST in a fresh VM, no network -> package list equals the listing's expected list
  -> admitted: image released on this node, golden baked on first use
```

The builder VM is a Firecracker VM like any run (jailer, read-only kernel, no host
mounts), so `crates/vam-build/SKILLS.md` invariant 2 (no `RUN`-like steps on the host) holds
for apt maintainer scripts too. Egress uses the node's existing egress proxy with a fixed
allowlist: the snapshot mirror, the wheel host, the npm registry, or the operator's mirrors.
The author cannot add hosts in Lite.

**Who may build** (answers `agent-experience.md` open question 6, line 1050). An org admin,
by enabling an image listing from a marketplace the org trusts; or a member with publish
rights, by uploading a draft (`vam image build ./dir` sends the recipe, the node builds it as
an org-private draft, `vam image release` releases it). A per-node build queue of one, a
build timeout and a disk quota stop builds from crowding out runs.

**Marketplace listing kind.** A listing adds artifacts of kind `image`:

```
listings/<listing>/images/<name>/
  image.json        name, version, description, from, runtimes, vm floor
  Vamfile           the Lite recipe
  Vamfile.lock      generated by `vam image lock`
  packages.txt      the expected resolved package list (the contract)
  files/            COPY sources, if any
```

`marketplace.json` lists it as `{ "kind": "image", "path": "listings/<listing>/images/<name>" }`
with the listing's `ref` tag and a `requiresNode` at or above the first release with I3.
The node imports it like any artifact: at the listing's tag, moved tag refused
(`marketplace/mod.rs:19, 250`), package digest recorded (`marketplace/installs.rs:61`).

**Contribution flow.**

1. Write the recipe; run `vam image lock` and `vam image build --local` (Docker on the
   author's own machine, a preview only) to see the package list and run TEST.
2. Open a pull request on the marketplace repo with the four files.
3. Marketplace CI (`.github/workflows/validate.yml` in `vam-marketplace`) adds two steps
   after `vam catalog validate --strict`: `vam image check` (grammar, lock matches, FROM
   names a `vam/*` curated image or an image already released in the same marketplace, COPY limits, secret lint), then a Docker build of the recipe on the CI
   runner, TEST, and a diff of the resolved list against `packages.txt`. The runner is
   disposable, so a PR build is acceptable there; it is never a node. The SBOM is attached
   to the run for the reviewer.
4. A `CODEOWNERS` reviewer approves; the release workflow tags `<listing>-v<x.y.z>` as today.
5. On a node, an admin enables the listing; the node builds the image in the builder VM and
   admits it only if TEST passes and the package list equals `packages.txt`.

Images from an added ("Unverified") marketplace install the same way, labelled Unverified,
under the node's rules for non-Squrell content, like skills (`listing-from-an-added-
marketplace-installs-as-unverified-within-its-limits`). The guest is untrusted either way;
the build is what needs isolation, and the builder VM gives it.

**Exit.** A Lite listing at a pinned tag builds on a node and an agent with that
`imageRef` runs on it; a recipe with `RUN` or an unhashed package is refused before any
build; a build cannot reach a host outside the mirrors; a failing TEST leaves nothing
admitted; marketplace CI rejects a PR whose package list drifts.

**Scenario delta (I3)**

| Scenario | Change | Layer | Given | Why |
|---|---|---|---|---|
| `image-listing-builds-on-the-node-and-an-agent-runs-on-it` | new | firecracker | `org-admin`, `marketplace-with-image-listing` (new), `agent-on-listed-image` | the exit |
| `vamfile-lite-with-a-run-step-is-refused-before-building` | new | process | `org-admin`, `image-draft-with-run` (new) | Lite has no `RUN` |
| `vamfile-lite-with-an-unhashed-package-is-refused` | new | process | `org-admin`, `image-draft-without-lock` (new) | decision 6 and the lock rule |
| `image-build-cannot-reach-a-host-outside-the-mirrors` | new | firecracker | `image-draft-that-fetches` (new; a test-mirror apt package whose maintainer script, run inside the builder VM during the build, tries a non-mirror host and a mirror and writes both results into the image) | builder egress is mirrors only: the outside attempt fails and the proxy logs the deny |
| `image-whose-test-fails-is-not-admitted` | new | firecracker | `image-draft-failing-test` (new) | TEST gates admission |
| `image-listing-with-a-moved-tag-is-refused` | new | process | `marketplace-with-image-listing`, tag moved | same pin rule as other listings |
| `node-refuses-to-build-a-recipe-from-a-bare-url` | new | process | `org-admin` | decision 9 |

### 3.4 I4 — Full Vamfile (gated)

Only when a named client is blocked by I1 to I3 (Q14). Adds `RUN`, `ARG`, multi-stage
`AS`, `BUILD_EGRESS` (author-declared hosts, recorded for review), `SECRET` (build-time
mount only), `HARDEN`, and the app's Machine step (E7 Phase 4,
`agent-experience.md:514-522`). Same builder VM, same admission. Scenario delta when
planned: at least `vamfile-run-step-runs-only-in-the-builder-vm`,
`run-step-reaches-only-its-build-egress-hosts`, `build-secret-is-not-in-the-image`.

## 4. Why "build any Vamfile from a URL on the node" is rejected

The request: point the node at `https://…/Vamfile` and get an image. Refused, for these
reasons:

1. **A URL is not a version.** The content behind it can change between two nodes, or
   between build and audit. A marketplace tag is immutable and a moved tag is refused
   (`marketplace/mod.rs:250`); a URL has no such rule, so a receipt could not say what ran.
2. **Nobody reviewed it.** A listing passes `vam catalog validate`, marketplace CI and a
   `CODEOWNERS` approval before it is tagged. A URL skips all three.
3. **It spends the node on a stranger's job.** A build is minutes of CPU, gigabytes of disk
   and network to the mirrors. Anyone who can paste a URL could fill the node.
4. **Unpinned inputs mean open egress.** A recipe without a generated lock resolves
   packages at build time, which is exactly the run-time install DX 9b forbids, moved one
   step earlier.
5. **It already exists in a safer form.** "Install by URL" is what adding a marketplace by
   URL does: the admin trusts the repository once, and its listings install at pinned tags,
   labelled Unverified. Private recipes go through a draft uploaded by a member with publish
   rights.

## 5. Interplay with other lanes

- **Apps and Spaces (lane 1**, `design/vam-apps-spaces.md` "Amendment 2026-11-28",
  tracker `.cursor/workstreams/apps-spaces.md`). An App pins agent and version (C1); the image is inside the
  version, so the App gets a fixed machine with no App field and no Apps change. Staging and
  prod (C2) can run different images only by pinning different agent versions, which is the
  intended promotion path. When the environment profile (DX-17) arrives, it carries VM size,
  not the image.
- **Self-aware (lane 3**, `design/self-aware.md`, tracker
  `.cursor/workstreams/self-aware.md`). A refused `requiresRuntime` or a missing image is a typed denial
  with a fix ("pick `vam/node:1`", "ask an admin to enable listing X"); S1 owns the denial
  shape, this lane supplies the reason codes.
- **Agent access.** I2 changes agent release checks and `start_run`, which agent access
  also changes; I2 waits until agent access is in `main`.
- **Hot files (Q16).** I1 touches none of `run_start.rs`, `catalog_resolve.rs`, `manager.rs`
  or the agent spec proto. I2 touches all four (catalog kind, `image_ref` field,
  `manager.rs:1919-1928`, image resolution in `start_run`) and starts only after Apps P4 has
  merged and Apps hands the hot files over. I3 touches marketplace import and new
  `vam-build` code only.

## 6. Relation to existing decisions

| Decision | This doc |
|---|---|
| E7 rules 1-4 (`agent-experience.md:450-467`) | Kept |
| E7 grammar (`:469-490`) | Lite is a subset; `apk` becomes `apt`; `VM` added |
| E7 pipeline and tiers (`:492-512`) | Kept; distribution by node-side build (decision 7) instead of the registry the build docs planned |
| DX §19.5 "defer Tier 2 until a customer is blocked" (`developer-experience.md:1153-1160`) and DX-104 amended | I1, I2 build Tiers 0 and 1; I3 is a narrow Tier 2 with no `RUN`; I4 keeps the deferral (Q14) |
| DX 9, 9a, 9b; DX-101 to DX-105 | I1 builds DX-101, 101A, 101B (per image), 102, 105; DX-103 (Nix) stays unbuilt |
| DX 17 (deployment fields move to an environment profile) | I2 reads `resource_requirements` where it is today until the profile exists (open question 4) |
| `machine-base` 1, 2, 4, 5 | Kept; the "later: `vam/python`, `vam/node`" of decision 2 is I1 |
| `release-artifacts` 13 (no image shipped, node builds recipe) | Kept and extended to every image |

## 7. Open questions (each with a recommended answer)

1. **Q12. Call the machine a "VAM image"?** Recommended: yes; "environment" stays staging
   and prod.
2. **Q13. May agents install packages at run time and keep them in the workspace?**
   Recommended: no. Packages come from the image or from the host-materialized,
   hash-cached skill lockfile. The guest never installs.
3. **Q14. When do we build the full Vamfile (I4)?** Recommended: only when a real client is
   blocked by curated bases, skill packages and Vamfile Lite, named in the tracker.
4. **Owner question: VM size before the environment profile exists. Does I2 honouring
   `resourceRequirements` conflict with DX locked decision 17** (these `AgentSpec` fields
   move to an environment profile, `.cursor/workstreams/developer-experience.md:135`)?
   Recommended: I2 honours
   `AgentSpec.resource_requirements` (vCPU and memory only) inside operator caps; when DX-17's
   profile lands, the profile wins and the agent field is a fallback. Alternative: wait for the
   profile and ship I2 with images only.
5. **Which marketplaces may supply images?** Recommended: any marketplace the org trusts,
   with Unverified labelling, the same as skills. Alternative: Squrell only until I3 has run on
   a pilot.
6. **Where do curated images come from on a node?** Recommended: built by `vam node init`
   from bundled recipes, as `vam/base:2` is. Revisit publishing built images (OCI, ORAS) after
   the licence review.
7. **Golden disk budget.** Recommended default `VAMD_GOLDEN_MAX_BYTES` = 8 GiB (about four
   images at two sizes); the breakdown measures and adjusts.
8. **JS lockfiles.** Recommended: accept `bun.lock` and `package-lock.json` (Bun reads both
   with `--frozen-lockfile`); refuse `yarn.lock` and `pnpm-lock.yaml` until asked.
9. **Skills drive size.** Recommended: cap one skill's materialized packages at 512 MiB and
   the drive at 2 GiB, refuse at release above it; the breakdown checks current drive sizing.

## What we are not building

- Package installs inside the guest, or any workspace-persisted toolchain.
- Building recipes from a bare URL, or community recipes on the host with Docker.
- Publishing built ext4 images, or an OCI/ORAS registry, in this lane.
- Nix or a VAM dependency resolver (DX decision 9).
- Custom kernels, non-Debian bases, arbitrary OCI `FROM` images.
- An image field on the App.
- `WARM` snapshots (E7 "later"), image signing beyond the catalog digest (E7 Phase 5 with E6).
- macOS/Lima images (the standard base is x86_64 only, `release-artifacts.md:56`).
