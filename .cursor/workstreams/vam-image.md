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
own manifest and golden snapshot (baked lazily, evicted under a disk budget);
`requiresRuntime` is checked per image; a skill's hashed Python or JS lockfile is fetched on
the host, cached by content hash and mounted read-only (DX-101, 101A, 101B, 102, 105).  
**Depends on:** none (stays out of the hot files: works in `golden.rs`, `node_base.rs`,
`skill_package_resolve.rs`, `scripts/base/`, `vam-cli` node init, `vam-runtime` exec).  
**Plan:** not drafted  
**Exit:** `vam/python:1` and `vam/node:1` build on the CI lane and boot; a skill with
`polars` in a hashed lockfile releases and its tool imports it on `vam/base:2`; a second agent
with the same lockfile hits the cache; a source-only package is refused at release; `GET
/v1/node` lists every image with its runtimes; golden restore median on the default image
within 2x of ~217 ms (`machine-base` escalation rule).

- [ ] Curated recipes `vam/python`, `vam/node` (FROM the `vam/base:2` tree), pins,
  manifests; `vam node init --image`; lane `base` input accepts them
- [ ] Several images per node: image store, manifest map in `node_base.rs`,
  `status.images[]`; `VAM_ROOTFS_PATH` stays the default
- [ ] Golden per (image, vCPU, memory): lazy bake on first use, LRU eviction under
  `VAMD_GOLDEN_MAX_BYTES`, default and in-use keys never evicted
- [ ] `requiresRuntime` per image: release passes if any held image satisfies; refusal names
  the images
- [ ] Skill packages from lockfiles: fetch-only installers (uv wheels-only with hashes, bun
  with scripts off), package mirrors via the SSRF guard, content-addressed cache, linked into
  the staged skill; `PYTHONPATH` for the skill's Python tools in the guest
- [ ] Docs: authoring page for skill lockfiles; node page for images and the golden budget

**Scenario delta** (draft from the design doc; the breakdown re-derives it from `guards` and
`given`)

| Scenario | Change | Layer | Given | Why |
|---|---|---|---|---|
| `skill-imports-a-package-from-its-hashed-lockfile` | new | process | `org-admin`, `package-mirror` (new), `skill-with-python-lockfile` (new) | DX-101/102 exit |
| `skill-with-a-source-only-package-is-refused-at-release` | new | process | `org-admin`, `package-mirror`, `skill-with-sdist-lockfile` (new) | decision 6 |
| `second-agent-with-the-same-lockfile-reuses-the-cached-packages` | new | process | `org-admin`, `package-mirror`, `skill-with-python-lockfile` | DX-102 cache |
| `node-reports-each-image-it-holds` | new | firecracker | `org-admin`, node with `vam/base:2` and `vam/node:1` | several images per node |
| `skill-needing-a-runtime-the-base-lacks-is-refused-at-install` | changed: refusal names every held image | process | as today | `requiresRuntime` per image |
| `skill-uses-node-and-npm-packages-on-the-node-base` | new | firecracker | node defaulting to `vam/node:1`, `skill-with-js-lockfile` (new) | curated base boots, JS dep resolves |

**Open questions** (each with a default; design doc section 7)

- Golden disk budget: default 8 GiB, measured in the breakdown.
- JS lockfiles: `bun.lock` and `package-lock.json`; refuse yarn and pnpm.
- Skills drive size: 512 MiB per skill's packages, 2 GiB per drive, refused at release above.

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
