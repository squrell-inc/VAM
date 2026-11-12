# Developer experience: artifacts, lifecycle, and the road to a marketplace

**Status:** brainstorming / living document. Not a plan of record yet.
**Scope:** how a developer authors, composes, versions, releases, and reuses the
VAM artifact set — HSP brokers, Cedar policies, `agent.json`, `run.json`, and
skill manifests — and what has to change before a package marketplace is viable.

Everything in §2–§5 is evidence-backed against the tree at the time of writing.
Proposals in §6–§8 are explicitly labelled as proposals.

**Part I (§1–§9)** audits what exists today.
**Part II (§10–§17)** works from the *intended* product model — stated in
session — and designs the artifact model, capability system, Git↔marketplace
marriage, and hosted-scale concerns needed to deliver it.
**Part III (§19–§23)** takes the developer's point of view: the
image/packaging decision, CLI vs UI vs API, and a stage-by-stage journey
roadmap from first evaluation to publishing on a marketplace.

**Tracker:** `.cursor/workstreams/developer-experience.md` holds the phased
roadmap and issue breakdown derived from this document.

**Two scope corrections made after Part III was written, which override any
statement to the contrary above:**

1. **HSP is not a marketplace target.** Brokers stay operator-installed and
   first-party. Broker URNs, versions, digests, signing, and sandboxed
   third-party brokers over vsock are all deferred. Read every HSP-as-package
   passage in §12, §15.4 and §16.1 as future thinking, not plan.
2. **The UI already exists**, as a Tauri app in a separate repository. §20.3
   should be read as *what the API must expose for that app to consume*, not as
   a proposal to build a UI. The binding requirement is API parity: no
   capability may be reachable from only one client.

---

## 1. Why this document exists

The five concepts in the title are each individually coherent. The confusion is
not inside any one of them — it is in the **seams between them**: the same
logical fact is restated in four files under three naming conventions, and
nothing checks that the restatements agree.

This document tries to (a) write down the real current flow so we stop
re-deriving it, (b) separate genuine architectural gaps from cosmetic ones, and
(c) sequence the fixes so the marketplace phase lands on a foundation that
already works.

---

## 2. The concept map

The developer-facing surface is **seven** artifacts, not five. Two of the
missing ones are a large part of the confusion.

| Artifact | File | Identity | Versioned? | Released? | Who authors it |
|---|---|---|---|---|---|
| Agent | `agent.json` | `urn:vam:agent:…` | yes | yes (catalog) | developer |
| Run | `run.json` | `urn:vam:run:…` | n/a — it is a request | no | developer |
| Skill | `skills/<id>/manifest.json` + payload | `urn:vam:skill:…` | yes | **yes, fully** | developer |
| Policy | `policy.cedar` | `urn:vam:policy:…` | yes (catalog row) | yes | developer |
| **Playbook** | `playbook.json` | `urn:vam:playbook:…` | yes | yes | developer |
| **HSP broker** | broker process + `CapabilityManifest` | `broker_id` (**not a URN**) | no | no | operator |
| **Workspace** | — | `{owner}--{suffix}` | pins an agent version | no | implicit |

Two observations that matter more than they look:

- **Playbook is a first-class catalog artifact** with its own URN, version,
  draft/release channel, and `PUT`/`GET` API — but it is absent from most
  mental models and from the list we started this session with. If it is real,
  it needs to be taught; if it is vestigial, it should be retired. Right now it
  is neither.
- **HSP brokers are the only artifact that is not a catalog resource.** They
  have a `broker_id`, not a URN; no version, no digest, no draft/released
  channel, no publish command. This is the single biggest structural mismatch
  in the system and it is exactly the thing a marketplace would need to ship
  (see §7).

### 2.1 How they actually connect

```
run.json ──agentRef(urn+version+digest)──► agent.json
                                              │
                    ┌─────────────────────────┼──────────────────────────┐
                    │                         │                          │
              skillRefs[]            capabilityPolicy.policyRef    defaultPlaybookRef
                    │                         │                          │
                    ▼                         ▼                          ▼
           skill manifest.json          policy.cedar               playbook.json
                    │                         │
      ┌─────────────┴──────────┐              │
      │                        │              │
  local_exec              host_exec           │
  (in guest)         adapter_id: "http_get"   │
                     adapter_id: "hosted:<broker_id>:<tool>"
                                │             │
                                ▼             │
                          HSP broker ◄────────┘
                       (registered out-of-band,
                        NOT referenced by URN)
```

The HSP edge is dashed in reality: an agent never names a broker. It names a
*synthetic skill* `hosted-<broker_id>` that `vamd` generates at broker
registration time. If the broker was not registered before the session started,
the skill simply does not exist and the agent fails at `load_skill`.

---

## 3. The current flow, honestly described

Ten steps, minimum, before a first run:

1. `cargo build -p vam-cli --release`, put `vam` on `PATH`
2. start `vamd` (Linux) or the Lima stack (Apple Silicon)
3. `vam config set-host …`
4. `vam config set-key --from-env …` or `vam login`
5. `vam health && vam node status`
6. hand-write 5–8 files: `agent.json`, `run.json`, `policy.cedar`,
   optionally `playbook.json`, `skills/<id>/manifest.json`, `SKILLS.md`,
   `execute.js`, `package.json`
7. `vam secrets set …` for every inference and egress binding
8. optionally publish to catalog: `vam agent put`, `vam policy put --urn …`,
   `vam skill publish`, `vam skill release --version X.Y.Z`
9. (HSP only) start the broker, then `PUT /v1/hosted/brokers` to register it —
   **before** creating the session
10. `vam run start …/run.json --follow`

There is no `vam init`, no `vam validate`, no `vam doctor`, no `vam build`, and
no `vam deploy`. Step 6 is a copy-paste-from-examples step, which is why the
examples' own inconsistencies (§4) propagate directly into user projects.

### 3.1 What is genuinely good and should not be rebuilt

It is worth being explicit, because the gap list below is long:

- **The catalog is already a real registry.** `crates/vam-orchestrator/src/catalog.rs`
  has per-artifact tables keyed by `(urn, major, minor, patch, prerelease)`,
  a `draft | released | yanked` channel constraint, SHA-256 digests, owner
  scoping, and a `user | platform` tier. `catalog_publish.rs` implements draft
  publish and release promotion with digest-conflict detection on re-release.
- **Release accepts caller-chosen semver.** `POST` on the release routes parses
  a `version` from the body and rejects prerelease suffixes
  (`api/catalog_release.rs:117-128`). The `1.0.0` constants in `catalog.rs` are
  convenience/test wrappers, not a product limitation.
- **Floating refs resolve and get pinned.** `catalog_resolve.rs` resolves
  unversioned skill/playbook/policy refs to latest released and records the
  resolved pin on the run. Agents stay pinned.
- **Skills have a complete lifecycle.** `publish` (tar.gz) → `release --version`
  → digest-addressed resolution → admission. This is the model the other
  artifacts should copy.
- **Cedar is real.** `cedar-policy 4.12`, an embedded schema at
  `crates/vam-security/schema/vam.cedarschema`, `ValidationMode::Strict`
  validation at publish time, fail-closed on every malformed input, and a
  `Shadow` mode that already exists.
- **HSP admission is rigorous.** `hsp_registry.rs` rejects unapproved brokers,
  extra tools, and extra secrets against the operator-approved manifest.

The foundation is much closer to a marketplace than the CLI surface suggests.
Most of what follows is about **exposing and reconciling** what already exists.

---

## 4. Confirmed defects and inconsistencies

These are verified, not suspected. They are listed first because several are
cheap to fix and are actively teaching users the wrong thing.

### 4.1 Two of the five example `run.json` files cannot parse — **bug**

`vam run start` loads a run file via `vam_spec::encoding::json_to_proto`, which
uses `prost_reflect::DynamicMessage::deserialize` with default options. The
default is `deny_unknown_fields: true`
(`prost-reflect-0.12.0/src/dynamic/serde/mod.rs:182`). The CLI tries
`CreateRunRequest` first, then `RunSpec`.

`RunSpec` (`proto/vam/v1/workflow.proto:100`) declares `run_id`, `agent_ref`,
`playbook_ref`, `inputs`, `max_duration`, `max_financial_budget`,
`workspace_ref_uri`, `idempotency_key`, `thread_ref`, `resolved_refs`.
`CreateRunRequest` declares only `spec`, `inline_agent`, `inline_playbook`,
`idempotency_key`.

But `examples/sqlite-assistant/run.json` and `examples/chaos-assistant/run.json`
are written as:

```json
{
  "resource_id":    { "urn": "urn:vam:run:sqlite-analysis-001" },
  "agent_ref":      { "resource_id": { "urn": "urn:vam:agent:sqlite-assistant" } },
  "inputs":         { ... },
  "timeout_budget": { "seconds": 300 },
  "cost_budget":    { "max_usd": "0.50" }
}
```

`resource_id`, `timeout_budget`, and `cost_budget` are **not fields of either
message**. Both parses fail. Meanwhile `examples/jira-assistant/run.json` uses
the correct shape (`spec` wrapper, `maxDuration`, `maxFinancialBudget`).

So we ship two mutually incompatible `run.json` dialects, one of which is not
merely deprecated but non-functional. A new developer has a 40% chance of
copying the broken one. **This should be fixed today, independent of any
larger redesign.**

### 4.2 The example digests are all the SHA-256 of the empty string

Every `digest.hexHash` in the example `agent.json` files is
… — `sha256("")`. This is not decoration: the orchestrator
treats it as a sentinel meaning *unpinned*, via
`skill_package_resolve::EMPTY_SHA256_HEX`, checked in `catalog_resolve.rs:164`,
`:298`, and `:370`.

The effect is that the examples teach a `digest` field that looks like
integrity verification but is a magic "don't verify me" value. Worse, the
policy digest in `capabilityPolicy.policyRef` is **never checked at all** —
`security.rs:208-217` extracts only the URN and resolves by name, discarding
the declared version and digest.

Two separate problems: a sentinel that looks like a real hash (should be `""`
or an explicit `"unpinned": true`), and a declared-vs-resolved pin that is
silently ignored for policies.

### 4.3 The Cedar principal URN is not normalized

`examples/jira-assistant/policy.cedar` contains every tool rule **twice** —
once for `Agent::"urn:vam:agent:jira-assistant"` and once for
`Agent::"urn:vam:agent:examples.local/vam/jira-assistant"`:

```cedar
permit (principal == Agent::"urn:vam:agent:jira-assistant", …)
permit (principal == Agent::"urn:vam:agent:examples.local/vam/jira-assistant", …)
```

This is a workaround, checked into an example, for an unresolved question about
whether the Cedar principal is the short name or the fully-qualified URN. It
doubles policy length and will silently drift. One canonical form must win, and
the PDP should reject the other rather than quietly never matching it.

### 4.4 Naming for one HSP capability spans six identifiers

For the echo example, the same thing is called:
`hsp-echo-ts` (npm package), `echo-ts` (broker id), `hosted-echo-ts`
(synthetic skill), `echo.ping` (tool), `hosted:echo-ts:echo.ping` (adapter id),
`urn:vam:skill:examples.local/vam/hosted-echo-ts` (catalog URN).

Cedar sees `echo.ping` — the *unqualified* name. Two brokers exporting a tool
of the same name are therefore indistinguishable to policy. That is a latent
security issue, not just an ergonomics one, and it gets sharply worse the
moment third-party brokers exist.

### 4.5 Documentation contradicts implementation

- `docs/…/hosted/first-party.md` says "No HSP code, proto contract, or broker
  runtime exists in the workspace today. Nothing on this page is implemented."
  The workspace contains `hosted_services.proto`, `HspClient`, `HspRegistry`,
  `hsp_store`, `hsp_hooks`, two echo brokers, and a TS example. The statement
  is true of `vam.files`/`vam.rag` only, but reads as global.
- `docs/…/security/cedar.md` presents `Action::"EgressRequest"` as the egress
  control. The action exists in the schema and the context fields exist in
  `EvaluationContext`, but no `EgressRequest` PEP was found in the orchestrator
  and no example policy uses it. Egress appears to be enforced by the network
  layer and adapter allowlists (`"allowlist": "*.atlassian.net"` in the skill
  manifest) instead. Either wire the PEP or correct the doc.

### 4.6 Lifecycle symmetry is broken across artifact types

| | put | get | ls | publish | release |
|---|---|---|---|---|---|
| skill | ✅ | ✅ | ✅ | ✅ | ✅ |
| agent | ✅ | ✅ | ✅ | ❌ | ❌ (API exists, no CLI) |
| policy | ✅ | ✅ | ✅ | ❌ | ❌ (API exists, no CLI) |
| playbook | ✅ | ✅ | ❌ | ❌ | ❌ (API exists, no CLI) |
| HSP broker | — | — | — | ❌ | ❌ |

The release HTTP routes exist for agent, playbook, skill, **and** policy
(`api/catalog_release.rs:15-28`). Only `skill` has CLI surface for it. So "how
do I release a v2 of my agent?" has a real answer at the API layer and no
answer at the CLI layer. This is the cheapest high-impact gap in the table.

---

## 5. The structural gaps

These are design gaps, not bugs.

### 5.1 There is no project manifest

Nothing ties a directory together. `vam run start` infers siblings by filename
convention (`agent.json`, `playbook.json` next to `run.json`). There is no file
that says "this project produces agent X at version Y, composed of skills A and
B, policies P and Q". Consequences: no single place to bump a version, no
reviewable dependency change, no lockfile, no reproducibility.

### 5.2 Policy composition does not exist

This is, in my view, **the most important gap** for the direction described.

The engine takes one `&str`:

```rust
pub fn new(schema_src: &str, policy_src: &str, mode: PolicyMode, generation: u64)
```

`AgentSpec.capability_policy` is a **single** `policy_ref`. One agent, one
policy document. The only composition that exists is
`security.rs:190-200`, which string-concatenates a generated HITL overlay onto
the base policy.

So "clubbing multiple policies" is currently done by **copy-pasting Cedar rules
between files**. There is no policy set, no `policyRefs[]`, no org→agent→run
layering, no Cedar templates or linked policies (both of which cedar-policy 4.12
supports and we do not use), and no import mechanism.

Note the irony: `agent.json` already references **four** distinct policies —
`capabilityPolicy`, `hitlPolicy`, `telemetryPolicy`, plus playbook refs — each
with its own URN, while the Cedar engine can only load one. The data model is
already ahead of the enforcement model.

This blocks the marketplace directly: an installed third-party skill will want
to ship its own least-privilege policy fragment, and there is nowhere to put it.

### 5.3 There is no "deployed agent"

There is a catalog (definitions) and there are sessions/runs (executions), but
no named, addressable deployment. A developer cannot ask "which version of
`support-bot` is live in staging?" or roll back. Workspaces can pin an agent
version (`error.rs`: *"agent version '{requested}' does not match
workspace-bound version '{bound}'"*), which is the closest primitive — but it
is a storage concept surfaced as an error message, not a deployment concept
surfaced as a command.

### 5.4 Two version axes are conflated

`1.0.0-draft` is used as the draft channel marker. That overloads semver
prerelease — which has real ordering semantics — to mean "unreleased working
copy". A draft is a *mutable pointer*; a version is *immutable*. Conflating
them means you cannot have a draft of `2.1.0` while `2.0.0` is released,
because the draft slot is literally the row at `1.0.0-draft`. And the CLI
release parser rejects prerelease strings outright, so a genuine `1.0.0-rc.1`
release is impossible.

### 5.5 HSP is operator-scoped, but the marketplace needs it developer-scoped

Broker registration is `PUT /v1/hosted/brokers` with an operator-approved
manifest, and the transport is UDS on the host. Docs are explicit that
HSP-over-vsock for marketplace brokers in isolated microVMs is future work.
Today a broker is a trusted host process with no version, no digest, no
signature, and no publish path. Every marketplace assumption — install an
untrusted broker, pin it, verify it, sandbox it — is unimplemented.

### 5.6 No local validation loop

`vam-spec` has real validation logic and the Cedar engine validates strictly —
but both only run **server-side at publish/admission time**. A developer's
inner loop is therefore: edit JSON → upload → read an HTTP error. There is no
`vam validate`, no policy linter, no "explain this deny", no schema for editor
completion. `PolicyMode::Shadow` exists in the engine and is not exposed as a
developer-facing dry-run.

---

## 6. Proposals

Ordered by (impact ÷ cost). Nothing here requires re-architecting.

### Tier 0 — do these regardless of direction

**P0. Fix the broken examples.** Rewrite `sqlite-assistant/run.json` and
`chaos-assistant/run.json` to the `CreateRunRequest` shape. Add a CI test that
round-trips every `examples/**/*.json` through `json_to_proto` for its declared
message type. This single test would have caught §4.1, and prevents the whole
class.

**P1. Pick one JSON casing and one URN form.** The examples currently mix
camelCase (`agent.json`, jira `run.json`) and snake_case (skill
`manifest.json`, other `run.json`s) — both are legal protobuf JSON, which is
exactly why it drifted. Choose camelCase (protobuf JSON canonical), convert all
examples, and say so in the authoring docs. Same for URNs: fully-qualified
everywhere, and delete the duplicated Cedar blocks in §4.3.

**P2. Stop using `sha256("")` as a sentinel.** Use an absent/empty `digest`
field for "unpinned" and reserve a real hash for a real pin. Then make policy
resolution actually verify the declared digest, or delete the field from
`CapabilityPolicyRef` so it stops implying a guarantee it does not provide.

**P3. Add the missing release CLI verbs.** The HTTP routes already exist:

```
vam agent release    <URN> --version X.Y.Z
vam policy release   <URN> --version X.Y.Z
vam playbook release <URN> --version X.Y.Z
vam playbook ls
```

This is thin client plumbing and it makes the release story uniform.

**P4. Reconcile the contradictory docs** in §4.5. Scope the `first-party.md`
disclaimer to files/RAG. Either implement the `EgressRequest` PEP or document
that egress is enforced by the network layer and adapter allowlists.

### Tier 1 — the DX unlock

**P5. `vam.json` — a project manifest.** One file that names the project and
declares what it produces and consumes:

```jsonc
{
  "name": "jira-assistant",
  "namespace": "examples.local/vam",
  "version": "1.2.0",
  "agent":    "./agent.json",
  "policies": ["./policy.cedar", "urn:vam:policy:vam.dev/base/readonly-fs"],
  "skills":   ["./skills/jira-integration", "urn:vam:skill:acme/slack@^2.1"],
  "playbooks":["./playbook.json"],
  "brokers":  ["urn:vam:broker:vam.dev/files@1"]
}
```

Version lives **here**, once, instead of being restated in five refs. This is
the anchor for every other proposal — `init`, `validate`, `build`, `publish`,
lockfiles, and the marketplace all need a unit of packaging, and right now
there isn't one.

**P6. `vam init`, `vam validate`, `vam doctor`.**
`init` scaffolds a correct project from a template (killing copy-paste drift).
`validate` runs the existing `vam-spec` + Cedar strict validation **locally**,
plus cross-file checks nothing does today: does every tool named in
`policy.cedar` exist in a referenced skill manifest? does every
`required_secrets` entry have a binding? is every `skillRef` resolvable?
`doctor` checks KVM/Lima, rootfs, kernel, daemon reachability, auth, secrets.

**P7. `vam.lock`.** Resolve every floating ref to `urn@version+digest` and
write it out. The orchestrator already computes exactly this
(`RunSpec.resolved_refs`) — it just never gives it back to the developer in a
committable form. Reproducibility and supply-chain review both fall out of this
for very little work.

**P8. Expose shadow mode as `vam policy explain`.** Given a policy, a tool, and
a context, print the decision and the matching policy IDs. `DecisionOutcome`
already carries `matching_policies`. Pair it with `vam policy test` running
table-driven fixtures — the tests in `crates/vam-security/tests/pdp_tests.rs`
are already exactly this shape, just not available to users.

### Tier 2 — the architectural changes

**P9. Policy sets: `policyRefs[]` instead of `policyRef`.** Replace the single
capability policy with an ordered list, concatenated into one Cedar
`PolicySet`. Cedar's forbid-overrides-permit already gives sound composition
semantics for free — a platform baseline can `forbid` and no downstream
fragment can re-`permit` it. This is the enabler for:
- an org baseline policy layered under every agent,
- a skill shipping its own least-privilege fragment,
- a run-level temporary narrowing.

Then use **Cedar templates** for the repetitive parts. The
`/mnt/drive9/workspace/*` read/write/size block is copy-pasted verbatim across
all five example policies; it should be one linked template with the workspace
path as a slot.

**P10. Make the Cedar resource broker-qualified for hosted tools.** Cedar sees
`Tool::"echo.ping"` while the route is `hosted:echo-ts:echo.ping`. Move to
`Tool::"hosted:echo-ts:echo.ping"` (or add a `broker_id` context field) before
third-party brokers make name collisions exploitable. Cheap now, breaking later.

**P11. Separate the draft pointer from the version.** Make `channel` the only
draft marker and let drafts carry their true target version, so `2.1.0-draft`
can coexist with released `2.0.0`. Allow real prerelease releases
(`1.0.0-rc.1`) through the CLI parser.

**P12. Promote HSP brokers to catalog artifacts.** Give them
`urn:vam:broker:…`, a semver, a digest, and draft/released channels, so
`agent.json` can declare `brokerRefs[]` and fail fast at admission with
"broker `X@1.2` not registered" instead of failing at `load_skill` with a
confusing missing-skill error. This is the prerequisite for §7.

---

## 7. The marketplace phase

The good news: **the registry substrate already exists.** Namespaced URNs,
semver, digests, draft/released/yanked, owner scoping, and a `user | platform`
tier are all in `catalog.rs` today. A marketplace is mostly *exposure* of this
plus distribution and trust.

What is actually missing, in dependency order:

1. **A unit of packaging.** Nothing to install today. → P5 (`vam.json`), P7 (lockfile).
2. **Namespace ownership.** `examples.local/vam` is a convention, not a claim.
   Need domain verification (DNS TXT, like crates.io/npm scopes) binding a
   namespace to an owner. Note `catalog_tier.rs` currently identifies platform
   artifacts with `urn.contains(":examples.local/")` — a substring match on a
   hardcoded string. That will not survive third-party namespaces.
3. **Signing and verification.** `vam-security` already has Ed25519, but it
   signs *approval claims*, not artifacts. Nothing signs a skill package, a
   policy, or an agent. Publisher keys, signed manifests, and verification at
   install need to exist **before** anything untrusted is installable — this is
   the "don't retrofit it" item.
4. **`vam install` / `vam pull` / dependency resolution.** No solver, no
   transitive resolution, no cache. Scope this deliberately: flat, exact-pinned
   dependencies are probably sufficient and avoid inheriting npm's problems.
5. **Policy shipped with packages.** Requires P9. A marketplace skill must be
   able to say "I need exactly these capabilities", and the installing user must
   be able to see and diff that. **A capability diff on install/upgrade — "this
   update additionally requests network egress to `*.foo.com`" — is the single
   highest-leverage trust feature we could ship**, and Cedar makes capabilities
   explicit and auditable, which supports it.
6. **Sandboxed third-party brokers.** Today a broker is a trusted host process.
   Marketplace brokers must run in isolated microVMs over vsock — acknowledged
   as future work in `hosted/architecture.md`, and gated on P12.
7. **Yanking and revocation.** The `yanked` channel exists in the schema; what
   happens to a *running* session pinned to a yanked artifact is undefined.
   Note that policy does not hot-reload into in-flight sessions by design, so
   revocation semantics need deciding explicitly.

**Sequencing view.** Items 1–3 are prerequisites and are mostly cheap. Item 5
is the highest-leverage trust feature. Item 6 is the expensive one and can be deferred by
launching the marketplace with **skills and policies only** — both are
declarative, already versioned, already digest-addressed, and do not require
new isolation work. Agents follow once P3/P5 land. Brokers come last.

---

## 8. Open questions

1. **Is `playbook` a supported concept or a legacy one?** It has full catalog
   machinery and near-zero presence in docs and mental models. Decide, then
   either teach it or deprecate it. Leaving it ambiguous costs us in every
   explanation of the artifact set.
2. **Should `run.json` exist as a file at all?** It is the only artifact that
   is a request rather than a resource. Most of its content is either
   agent-level config or a one-off prompt. `vam run <agent-urn> --prompt …`
   with flags may be the better primary interface, with the file reserved for
   reproducible batch runs. Worth noting `vam run continue` already exists as a
   *hidden* command — evidence the file-centric model is being worked around.
3. **Where does model selection belong?** `agent.json` has `preferredModels`;
   `run.json` has `inputs.model`. In the jira example the run specifies
   `nvidia/nemotron-3-super-120b-a12b:free`, which is not in the agent's
   preference list at all. Which wins, and is that an override or a violation?
   There is also an operator allowlist (`allowed_models.rs`) — a third input to
   the same decision.
4. **One policy language for all three concerns?** `capabilityPolicy`,
   `hitlPolicy`, and `telemetryPolicy` are separate refs with separate shapes,
   but HITL is already compiled into Cedar via an overlay. Should HITL and
   telemetry just be Cedar rules with distinct actions?
5. **Do we want `vam build`?** `vam-build` is documentation-only and the README
   calls the native image compiler "Planned Today". An agent is currently pure
   configuration over a shared rootfs, which is arguably the *right* model —
   there may be no artifact to build. Worth an explicit decision rather than a
   perpetual "planned".

---

## 9. Suggested near-term sequence

A concrete starting point, not a commitment:

1. P0 + P1 + P2 — fix and harmonize the examples, add the round-trip CI test.
   Small, removes actively-misleading material, unblocks everything else.
2. P3 + P4 — release verbs and doc reconciliation. Thin, makes the current
   system explainable.
3. P6 (`validate` first) — the local loop is what developers feel every day.
4. P5 + P7 — `vam.json` and lockfile. The packaging anchor.
5. P9 — policy sets. The real architectural unlock, and the marketplace gate.
6. Then re-evaluate the marketplace scope with §7 sequencing.

---
---

# Part II — Designing for the intended model

Part I audited what exists. Part II starts from the **product intent** and works
backward to the artifact model. The intent, as stated:

- **Agent** = *why this exists*. A role, a purpose, system instructions. Thin.
- **Skill** = *an installable package*. Any agent can install one and use it.
- **Policy** = *security, owned in one place*. Written by the skill author (what
  the skill needs) and by the org's security team (what the org permits).
- **Playbook** = *procedural knowledge*. May ship with an agent, with a skill,
  with both, or standalone. May span multiple skills.
- **Git** = where a developer's agent lives and is versioned.
- **Marketplace** = thousands of agents, lakhs of skills, installable by users.

This is a coherent product. The problem is that the schema cannot express most
of it.

---

## 10. The intended model vs. the encoded model

Four intents, checked against the protos:

| Intent | Encoded today? | Evidence |
|---|---|---|
| Agent is thin — role + instructions | ❌ inverted | `AgentSpec` is a kitchen sink: model prefs, context policy, reasoning policy, resource reqs, 4 policy refs, playbook refs, metadata. The jira example is ~200 lines for what the intent describes as a paragraph. |
| Skill authors ship policy | ❌ **impossible** | `SkillManifest` (`skills.proto:87-97`) has no policy field. |
| Playbooks ship with skills | ❌ **impossible** | No playbook ref on `SkillManifest`. |
| Playbooks span multiple skills | ❌ **impossible** | `PlaybookSpec` (`workflow.proto:86-97`) has `steps`, `input_schema`, `output_schema`, `metadata` — **no dependency refs of any kind**. |
| Install, then customize | ❌ **impossible** | No `extends` / `inherits` / `base_agent` field anywhere in `proto/vam/v1/`. A fork is a full copy. |

This is the root cause of the confusion this session set out to resolve. The
mental model is not wrong and it is not under-specified — it is simply **not
written down in the schema**, so every explanation has to reconstruct it from
scratch, and every example has to fake it (the jira agent's four policy URNs
point at policies that mostly do not exist, because the model they imply is not
implementable).

**The fix is not more documentation.** It is to encode the intent, then let the
docs describe something real.

### 10.1 One thing the current design already got right

`load_skill` — progressive disclosure, where a skill's instructions and tools
enter context only on demand — is the correct answer to a problem that becomes
existential at marketplace scale (§15.3). It should be kept and built on, not
replaced. The `SKILLS.md`-injected-on-load contract is genuinely good.

### 10.2 A positive surprise in the URN scheme

The proto comments already anticipate a registry host:

```
urn:vam:skill:hub.vam.dev/acme/oracle-sql      (skills.proto:88)
urn:vam:playbook:hub.vam.dev/acme/erp-monthly-close
```

So the shape is `urn:vam:<kind>:<registry-host>/<namespace>/<name>`, and
`squrell.cloud/acme/slack` drops in with no change. The examples'
`examples.local/vam/...` is the local-dev instance of the same scheme. **The
identity scheme is marketplace-ready.** That is a real head start.

---

## 11. Policy as capability request and grant

This is the most important design decision in Part II, because the stated
intent — *"policies are for the developers who build the skills, as well as the
organization's security team"* — describes **two different parties writing
policy about the same skill**. The current model (one agent, one policy
document, §5.2) cannot represent two parties at all.

### 11.1 The model

Borrow the mobile-permissions model, which solved exactly this:

> A skill does not **have** permissions. It **requests** them.
> The installer **grants** them. The org **bounds** them.

Five layers, with strict rules about what each may do:

| Layer | Author | May emit | Rationale |
|---|---|---|---|
| 1. Platform baseline | squrell.cloud | `forbid` only | Non-negotiable. Block writes to the skills mount, cloud metadata endpoints, etc. |
| 2. Org baseline | org security team | `forbid` only | Non-negotiable within the org. "No egress outside our allowlist." "Nothing `MUTABLE_EXTERNAL` without HITL." |
| 3. Skill request | skill author | `permit` only, **scoped to its own tools** | "To work, I need `slack.post` and egress to `slack.com`." Inert until granted. |
| 4. Agent grant | agent developer / installer | selects which requests activate | The consent step. |
| 5. Run narrowing | caller, per-invocation | `forbid` only | Temporary tightening. Optional. |

Cedar gives sound composition for free: **`forbid` overrides `permit`, and deny
is the default**. Layers 1, 2 and 5 can therefore never be escalated past by
anything downstream — which is precisely the property a security team needs.
This requires no new evaluation semantics, only a policy *set* instead of a
policy *string* (P9 in Part I).

### 11.2 The security linchpin: scoped permits

Layer 3 is where this design lives or dies. A skill shipping arbitrary Cedar is
a privilege-escalation primitive — nothing stops:

```cedar
permit (principal, action, resource);   // a malicious skill grants itself everything
```

**Admission rule, enforced at publish time:** every `permit` in a skill's policy
fragment must reference only tools that the skill itself exports in its
`SkillManifest.tools`. Anything broader is rejected at `vam skill publish`, not
at runtime. Layers 1 and 2 need no such rule because `forbid` cannot escalate.

This is a small, checkable, static rule, and it is what makes third-party skills
safe to install at all. It should be built before the marketplace, not after.

### 11.3 Humans cannot read Cedar

Cedar is the enforcement artifact. It is not a consent UI. So a skill declares
capabilities **twice**: once structurally (for humans and diffs), once in Cedar
(for the PDP), with publish-time validation that the Cedar fragment is a subset
of the structured declaration.

```jsonc
// in the skill manifest
"capabilities": [
  "tool:slack.post",
  "tool:slack.admin",
  "egress:slack.com",
  "secret:slack/bot-token",
  "fs:workspace:write"
]
```

The structured list is what appears in an install prompt, a PR diff, and an
admin review queue. The Cedar fragment is never shown to a user.

### 11.4 Consequence: collapse the three policy refs

`agent.json` currently carries `capabilityPolicy`, `hitlPolicy`, and
`telemetryPolicy` as three separate refs with three shapes — yet HITL is
*already* compiled into Cedar as a generated overlay (`security.rs:190-200`) and
evaluated as `context.hitl_approved`. The stated intent is that security lives
in **one place**. The intent argues for one policy set with distinct Cedar
actions, not three parallel mechanisms. Recommend collapsing.

---

## 12. Playbooks, taken seriously

The intent for playbooks is the least-served and, I suspect, the most
valuable.

### 12.1 Why a playbook shipped with a skill is a strong idea

A skill gives a model *tools*. It does not give it *competence*. Knowing that
`jira_search_issues`, `jira_transition_issue` and `jira_add_comment` exist does
not tell a model that a sprint cleanup means search → confirm → transition →
comment → report, in that order, with approval before mutation. That sequence is
real, reusable knowledge, and today it is smeared across `systemInstructions`
and a run prompt where it cannot be versioned or shared.

Shipping the procedure alongside the tools is the correct instinct. It should be
a first-class, separately versioned artifact that a skill can publish.

### 12.2 Skills are ingredients; playbooks are recipes

A playbook spanning multiple skills is a **composition artifact** — and it is
the one that delivers an outcome. Users do not want `slack@2.1` and `jira@3.0`;
they want "weekly sprint report posted to Slack every Friday."

Skills are ingredients, playbooks are recipes, the agent is the cook. Playbook
authoring, testing and discovery deserve investment well beyond their current
status as the artifact nobody mentions.

### 12.3 Required schema changes

```jsonc
// PlaybookSpec — add dependency declaration
"requires": {
  "skills": ["urn:vam:skill:squrell.cloud/acme/jira@^3.0",
             "urn:vam:skill:squrell.cloud/acme/slack@^2.1"],
  "capabilities": ["tool:jira.transition", "tool:slack.post"]
}

// SkillManifest — allow a skill to ship procedures and policy
"playbooks": ["./playbooks/sprint-cleanup.json"],
"policy":    "./policy.cedar",
"capabilities": [ ... ]
```

`requires` also makes playbooks **installable standalone**: installing a
playbook resolves and installs its skills transitively, which is the "install
and start using it" experience the intent describes.

### 12.4 An open design decision: DAG or procedure?

`PlaybookSpec.steps` is a strict DAG with `dependsOn`, `retryPolicy`,
`approvalAction` and templated instructions — a workflow engine. But the intent
describes something softer: "a set of instructions or steps" for the agent to
follow.

These are genuinely different products:

- **Rigid DAG** — deterministic, auditable, testable; brittle when reality
  deviates, and it fights the model's own planning.
- **Soft procedure** — guidance injected into context, model adapts; far more
  robust, but you cannot *guarantee* the approval step ran.

My read: **both are needed, and conflating them is a trap.** The soft procedure
is what most marketplace playbooks should be. The rigid DAG is what compliance
workflows need. If one artifact must serve both, mark steps individually as
`advisory` vs `enforced`, so the approval gate is enforced while the analysis
steps stay adaptive. Worth an explicit decision rather than drifting.

---

## 13. Marrying Git and the marketplace

The stated tension: *"when I introduce multiple skills into that agent, it
impacts the GitHub versioning of all of that story."*

That is the vendoring problem, and it is solved. The solution is a strict
three-way split, and VAM currently has none of the three.

### 13.1 Declare / resolve / materialise

| Layer | File | Hand-edited | Committed | Content |
|---|---|---|---|---|
| **Declare** | `vam.json` | ✅ | ✅ | Intent, with ranges: `"acme/slack": "^2.1"` |
| **Resolve** | `vam.lock` | ❌ generated | ✅ | Exact `urn@version+digest` **+ resolved capabilities** |
| **Materialise** | Drive9 skills mount | ❌ | ❌ gitignored | Actual payloads, fetched at session start |

The developer's repo **never contains another author's code.** Installing a
skill changes two committed files, and upgrading one changes a single lockfile
stanza. The "my repo churns when their skill releases" problem disappears
because their code was never in the repo.

### 13.2 Resolved capabilities in the lockfile

Record the **resolved capability set** in the lockfile, not just the version.
Then a routine dependency bump produces a reviewable diff:

```diff
 [[skill]]
 urn = "urn:vam:skill:squrell.cloud/acme/slack"
-version = "2.1.0"
-digest  = "sha256:9f2a…"
-capabilities = ["tool:slack.post", "egress:slack.com"]
+version = "2.2.0"
+digest  = "sha256:c41e…"
+capabilities = ["tool:slack.post", "tool:slack.admin",
+                "egress:slack.com", "egress:*.amazonaws.com"]
```

A reviewer sees, in a pull request, that a patch-level Slack upgrade started
asking for admin scope and AWS egress. A version-only
lockfile cannot show this. A lockfile that records capabilities can, because
Cedar already makes capabilities explicit.

Make it enforceable: `vam validate` (and therefore CI) **fails** when the
lockfile's resolved capabilities exceed the grants in `vam.json`. A capability
expansion cannot merge without a human explicitly widening the grant. This is
supply-chain protection, it is cheap once the capability model exists, and it should be a launch feature rather than a v2.

### 13.3 `extends`: install and customise without forking

The other half of the marriage. A user installs `acme/support-bot@3.2`, then
wants different instructions. Today that means copying ~200 lines of JSON and
permanently losing the upstream relationship.

Instead, make a fork an **overlay**:

```jsonc
{
  "name": "acme-support-bot-india",
  "extends": "urn:vam:agent:squrell.cloud/acme/support-bot@3.2",
  "systemInstructions": { "append": "Reply in Hindi or English. INR only." },
  "skills": { "add": ["urn:vam:skill:myorg/internal-kb@^1"] },
  "grants": { "revoke": ["egress:*.amazonaws.com"] }
}
```

The repo contains only the delta. `vam upgrade` re-resolves against
`acme/support-bot@3.3` and reports conflicts. This is what makes
"install thousands of predefined agents, then make them yours" actually work,
and it is unimplementable today.

### 13.4 Repo shapes worth supporting explicitly

- **Agent repo** — `vam.json` + `agent.json` + `vam.lock`. No skill payloads.
- **Skill repo** — manifest + payload + `policy.cedar` + playbooks + tests.
  Publishes on tag via CI.
- **Monorepo** — several of each, one lockfile. Needs workspace support; defer.

### 13.5 Git as a publishing source

`ResourceLocator` already exists on `SkillManifest` (`skills.proto:96`) as
"ordered native artifact locations/mirrors". A Git URL + tag is a legitimate
locator. That enables private/internal skills with no registry involvement,
and makes the registry an accelerator rather than a gatekeeper. Worth a look
before building bespoke distribution.

---

## 14. Developer experience vs. user experience

These are four personas, not two, and the failure mode is forcing one interface
on all of them.

| | Skill/playbook author | Agent developer | Org security admin | End user |
|---|---|---|---|---|
| Population | hundreds | thousands | one per org | everyone |
| Primary surface | Git + CI | Git **or** web | web console | chat / web |
| Sees JSON? | yes | yes | never | never |
| Sees Cedar? | writes fragments | rarely | **yes, reviews** | never |
| Cares about | publishing, compat | composition, reproducibility | blast radius, audit | outcome |
| Key artifact | skill + playbook | `vam.json` + `vam.lock` | org baseline policy | installed agent |
| Key ritual | `publish` → `release` | PR with capability diff | grant / deny review | install → prompt |
| Failure they fear | breaking consumers | silent behaviour change | data exfiltration | "it didn't work" |

**Design rules that follow:**

1. **One artifact model, three front-ends.** CLI, web console, and API must be
   equal citizens over the same catalog. Anything the console can do, the CLI
   must too — otherwise the Git persona is a second-class user of their own
   product.
2. **Eject must exist and be one command.** A user starts in the console, grows
   up, and needs `vam eject <agent>` to produce a `vam.json` + `vam.lock` they
   can commit. Without it, the console is a trap and sophisticated users avoid
   it from day one.
3. **Adopt is the inverse and matters just as much.** Point the console at a Git
   repo; it renders and runs. This is the Vercel model and it is why developers
   tolerate a dashboard.
4. **The security admin never writes an agent.** Their entire surface is the org
   baseline plus a review queue of capability diffs. Today they have neither.
5. **The end user must never see a URN.** "Slack" and "Jira", not
   `urn:vam:skill:squrell.cloud/acme/slack@2.1`. URNs are an infrastructure
   detail; display names and icons are the product.

---

## 15. What changes at hosted scale

Assume squrell.cloud with thousands of agents and lakhs of skills. Several
current decisions do not survive that.

### 15.1 Tool namespacing becomes existential

Cedar currently sees the **unqualified** tool name — `Tool::"echo.ping"` — while
the route is `hosted:echo-ts:echo.ping` (§4.4). At five example skills this is
untidy. At a lakh of skills, names like `search`, `query`, `run`, `get` collide
constantly, and a policy permitting `Tool::"search"` silently authorises a tool
from a skill the user never intended to trust.

Qualify the Cedar resource — `Tool::"acme/slack:post"` — **before** third-party
publishing exists. It is a cheap change now and a breaking one later.

### 15.2 Discovery: let the agent search, make the human grant

Nobody browses a lakh of skills. The natural interface is the agent itself
searching the catalog and proposing installs — which is powerful and obviously
dangerous.

The capability-grant model (§11) is what makes it safe: an agent may **search
and propose**; only a human may **grant**. Install becomes a HITL approval
showing the structured capability list. VAM is unusually well positioned here,
because an approval mechanism (`hitl.rs`, approval signing, `vam approve`)
already exists and can be reused directly.

### 15.3 Context budget forces progressive disclosure

Fifty installed skills cannot fit their tool definitions in a context window.
`load_skill` is already the right answer (§10.1). What is missing at scale is
the *index*: a compact, searchable manifest of what is installed and what it is
for, so the model can choose what to load without loading everything.

`design/improving-the-vam.md` is already solving the same shape of problem for
large skill documentation corpora (`INDEX.md`, grep-the-index-first). **The
same pattern generalises to the installed-skill set**, and the two efforts
should share a design rather than diverge.

### 15.4 Trust tiers and namespace ownership

`TrustTier` exists in `CapabilityManifest` for HSP brokers; skills have no
equivalent. At marketplace scale, verified-publisher / community / unverified
tiers drive default grant behaviour: a verified publisher's read-only skill
might auto-grant, while anything unverified requesting egress always prompts.

Namespace ownership needs DNS-style verification (crates.io / npm scopes).
Note that `catalog_tier.rs` currently identifies platform artifacts with
`urn.contains(":examples.local/")` — a substring match on a hardcoded string,
which will not survive third-party namespaces.

### 15.5 Skill payloads are not yet relocatable

The jira skill manifest hardcodes:

```json
"entrypoint": "/mnt/drive9/skills/jira-integration/execute.js"
```

That bakes in both the mount root **and** the skill's own local directory name.
An installable, namespaced package cannot assume either. Entrypoints should be
manifest-relative (`./execute.js`) and resolved by the host at mount time. Small
change; blocks installability until done.

---

## 16. Proposed artifact model

Consolidating Part II into a concrete shape.

```
vam.json          project: identity, dependencies (ranges), grants     [committed]
vam.lock          resolved: versions + digests + capabilities          [committed, generated]
agent.json        thin: role, instructions, skills, playbooks          [committed]
  └─ may "extends" an upstream agent URN
skills/<id>/      only if authoring a skill in this repo               [committed]
  ├─ manifest.json    tools, capabilities, playbooks, policy
  ├─ policy.cedar     scoped permits — validated ⊆ own tools at publish
  ├─ playbooks/       procedures shipped with the skill
  └─ SKILLS.md        the load_skill payload
playbooks/<id>.json   procedures, with "requires" skill refs           [committed]
policy/org.cedar      org baseline: forbid-only                        [committed, security team]
```

Deployment configuration — models, context policy, reasoning policy, vCPU,
memory, network mode — moves **out** of `agent.json` into an environment-scoped
profile. Rationale: the stated intent is that an agent is *why it exists*, and
"2 vCPU / 2048 MiB / `gpt-4o-mini`" is not why anything exists. It is also what
makes an agent portable across orgs — a marketplace agent cannot dictate the
installing org's model choice or VM sizing. This additionally resolves open
question §8.3: the agent expresses *requirements*, the deployment profile
chooses the *model*, the operator allowlist bounds it.

### 16.1 Resulting lifecycles

| Artifact | Versioned | Released | Installable | Composable |
|---|---|---|---|---|
| Skill | semver | ✅ | ✅ | into agents & playbooks |
| Playbook | semver | ✅ | ✅ | requires skills |
| Policy | semver | ✅ | ✅ | **layered** (§11) |
| Agent | semver | ✅ | ✅ | `extends` |
| HSP broker | semver | ✅ | operator-approved | via `brokerRefs` |
| Run | — | — | — | — |

Everything versions independently and composes by reference. That is the answer
to "releasing different versions of an agent versus a policy versus a skill":
they are four independent release trains, joined only by a lockfile.

---

## 17. Revised sequence

Part I's sequence still holds for §1–§4 (fix the broken examples, harmonise
casing and URNs, add release verbs, reconcile docs — all still worth doing
first and cheap). Part II changes what comes after.

**Phase A — make the intent expressible** *(schema work; unblocks everything)*
1. `SkillManifest`: add `capabilities`, `policy`, `playbooks`.
2. `PlaybookSpec`: add `requires`.
3. `AgentSpec`: add `extends`; move deployment config out.
4. `policyRefs[]` instead of `policyRef`; layered policy sets (P9).
5. Qualify Cedar tool resources with namespace (§15.1).

**Phase B — the developer loop**
6. `vam.json` + `vam.lock`, with capabilities recorded in the lock.
7. `vam init`, `vam validate` (incl. capability-vs-grant check), `vam doctor`.
8. Publish-time scoped-permit validation (§11.2) — the security linchpin.
9. `vam policy explain` / `vam policy test`.

**Phase C — marketplace**
10. Namespace ownership + publisher signing.
11. `vam install` / `vam upgrade`; capability diff on both.
12. Console with eject/adopt parity.
13. Agent-proposes / human-grants discovery (§15.2).
14. Trust tiers; sandboxed third-party HSP brokers last.

The ordering principle: **Phase A is mostly schema and is the cheapest thing on
this list, yet everything else is blocked on it.** Doing B or C first means
building tooling for a model the schema cannot represent.

### 17.1 Sharpest open questions after Part II

1. **Rigid DAG or soft procedure for playbooks (§12.4)?** Blocks playbook
   investment, and playbooks may be the main marketplace artifact.
2. **Can a skill's policy fragment ever `forbid`?** Permits are scopeable and
   safe; a `forbid` from a skill is unscopeable and lets one skill disable
   another. Current recommendation: no.
3. **Does `extends` allow removing an upstream skill, or only adding?** Removal
   is what users will want and is where upstream-upgrade merges get hard.
4. **Is `run.json` retained at all** once the console and `extends` exist? See
   §8.2 — the hidden `vam run continue` already suggests it is being routed
   around.
5. **Who owns the org baseline in a hosted multi-tenant world** — and how does
   it interact with the `App`/`Subject` identity work in
   `design/vam-apps-spaces.md`? These two designs need to be reconciled; both add
   principals to the same Cedar evaluation.

---
---

# Part III — The developer journey

Parts I and II were about artifacts. Part III is about **the human**: how a
developer starts, gets value, grows, ships, and distributes — and what the CLI
and UI each owe them at every stage.

---

## 19. The image question: core image, packages, Nix, Vamfile

Current state, verified:

- `crates/vam-build` contains **only** `SKILLS.md`. No implementation.
- `docs/…/build/` contains **21 documents** — `vamfile.md`, `directives.md`,
  `backends.md`, `oci.md`, `signing.md`, `supply-chain.md`, `reproducibility.md`,
  `templates.md`, `hardening.md`, `optimization.md`, and more. The Vamfile DSL is
  fully specified (`FROM`, `RUN`, `COPY`, multi-stage `AS`, syntax versioning).
- **Nix appears nowhere.** The only matches are the `nix` Rust crate and the
  word "Unix". It is an idea, not an exploration.

### 19.1 The tension to resolve

Custom images maximise flexibility and destroy three things: cold start,
marketplace portability, and the developer's time. If installing a marketplace
skill requires rebuilding your image because the skill needs `pandas`, then
skills are not packages — they are patches — and the entire Part II model
collapses.

**A skill that cannot bring its own dependencies is not installable.** That is
the requirement the image strategy must satisfy.

### 19.2 Proposed: three tiers, not one

| Tier | Mechanism | Who needs it | Cost |
|---|---|---|---|
| **0. Golden base** | Shared prebuilt rootfs + snapshot | ~90% | zero build, fastest boot |
| **1. Composed deps** | Skill declares deps; host materialises them into the mount at session start | ~9% | seconds, cached, content-addressed |
| **2. Custom image** | `Vamfile` → `rootfs.ext4` | ~1% | minutes; escape hatch |

Tier 1 is the missing middle, and it is where almost all real demand sits. It is
also the tier that makes the marketplace work.

### 19.3 Where Nix genuinely earns its place — and where it does not

The usual argument for Nix is reproducibility. That is *not* the strongest
argument here. The strongest argument is **collision-free composition**:

> With lakhs of skills from thousands of independent authors, two installed
> skills requiring incompatible versions of the same library is not an edge
> case — it is a certainty. Every conventional package manager resolves this
> with a shared mutable namespace and therefore with dependency hell. Nix store
> paths are content-addressed, so both versions coexist and both skills work.

Secondary benefits that fit VAM specifically: closures are content-addressed,
so they are digest-pinnable and drop straight into `vam.lock` and the capability
model (§13.2); derivation hashes give supply-chain provenance for free; and
binary caches make Tier 1 fast.

**The hard constraint: developers must never write Nix.** The learning curve
would kill adoption outright. A skill author writes:

```jsonc
"dependencies": { "python": "3.11", "pip": ["pandas>=2.0", "httpx"] }
```

Nix is the *resolver underneath*, an implementation detail of Tier 1, never a
developer-facing surface. If Nix leaks into the authoring experience, the
approach has failed.

### 19.4 Where Vamfile belongs

Vamfile is a good design solving a real but **rare** problem. Position it
explicitly as the Tier 2 escape hatch: custom kernels, unusual system
libraries, compliance-mandated base images.

Two cautions:

1. If Vamfile becomes the default path, every developer becomes a build
   engineer and the cold-start advantage is spent.
2. A custom image is **not portable across the marketplace** — a skill published
   against your bespoke image cannot be installed by anyone else. Tier 2 agents
   should be understood as private by nature.

**Cost note:** adopting the tiered model means much of the 21-document build
specification may never need implementing. That is a saving, not a loss.

### 19.5 Recommended decision

1. Ship Tier 0 properly first — a well-stocked golden base (Node, Python, Bun,
   common CLIs) covers most agents with zero build.
2. Build Tier 1 next, as `dependencies` in the skill manifest. Prototype Nix as
   the backend; keep the manifest field backend-agnostic so this stays
   reversible.
3. Defer Tier 2 / `vam-build` until a real customer is blocked. The spec is
   already written and will keep.

---

## 20. Three surfaces, one system

The stated rationale for the CLI — agents running commands, developers who
prefer terminals, and people building wrappers — is three distinct contracts.

### 20.1 The rule

**API first; CLI and UI are both clients.** The CLI is already an HTTP client
for `vamd` (`main.rs:7`), which is the right shape. The UI must be too. The
failure mode to avoid absolutely is privileged endpoints only the UI can reach —
it strands CLI users and makes the API a second-class citizen of its own product.

Corollary: **anything the UI can do, the CLI must be able to do**, and vice
versa. Feature parity is a product constraint, not an aspiration.

### 20.2 The CLI is an LLM interface, not only a human one

"The CLI helps agents quickly run commands" has real design consequences, and
they differ from human CLI design:

- `--json` on **every** command, with a stable documented schema.
- Stable, meaningful exit codes; never signal failure only in prose.
- Zero interactive prompts when stdin is not a TTY; every prompt has a flag.
- Idempotency keys on every mutating command (partly present already).
- **Errors written to be acted on**, not just read: state what was wrong, what
  was expected, and the command that fixes it. An agent cannot infer intent from
  a raw HTTP 422.
- Deterministic, greppable output; no spinners or colour when not a TTY.

This is a small amount of work that makes the CLI simultaneously the agent
interface, the CI interface, and the wrapper-builder interface.

### 20.3 What the UI owes, that the CLI cannot

The separate VAM app should not be a form-builder for JSON. Its distinct value:

- **Trace and timeline** — what the agent did, step by step, with tool calls,
  policy decisions, token and cost accounting. Reading a run is inherently
  visual.
- **The approval inbox** — HITL is a human workflow and belongs in a UI.
- **The capability review queue** — the security persona's entire surface:
  pending grants, capability diffs, org baseline.
- **Marketplace browse and install** — discovery is visual.
- **Cost and usage dashboards.**

Everything above is *read-heavy, human-judgement-heavy* work. That is where a UI
wins. Authoring, by contrast, should stay Git-first with the UI as an onramp.

### 20.4 Eject and adopt are the bridge

- `vam eject <agent>` — console-managed → `vam.json` + `vam.lock` in a repo.
- `vam adopt <repo>` — point the console at a Git repo; it renders, runs, and
  observes without taking ownership.

Without both, the console is a trap and sophisticated users will refuse it from
day one; with both, it is an onramp. This is the Vercel model and it is why
developers tolerate a dashboard.

---

## 21. The developer journey roadmap

The organising principle: **every stage must be a place a developer can stop and
still have something valuable.** No stage may require the next. Today there is a
cliff — you can run a bundled example, and then you are on your own.

Two hard rules:

- **Value before setup.** Nothing of value today happens before ten setup steps
  including KVM or Lima (§3). That is fatal for adoption.
- **The inner loop is the product.** Stage 3's edit→test cycle determines
  whether anyone stays. If it requires a microVM boot, it is already too slow.

---

### Stage 0 — Evaluate · *5 minutes, zero install*

> *"Does this do anything I need?"*

| | |
|---|---|
| **Does** | Opens squrell.cloud, picks a published agent, runs it in a playground against a sample workspace. |
| **UI** | Marketplace listing → "Try it" → chat. No signup until they want to keep something. |
| **CLI** | *(not yet — do not make the terminal the front door)* |
| **Value moment** | An agent does real work, and a panel shows **exactly which tools it used and which were denied**. |
| **Needs** | Hosted playground; ephemeral sandbox workspaces (see `design/vam-apps-spaces.md` — a Space is the primitive); published demo agents. |

The denial panel matters more than the success. It shows
the governance model in the first minute.

---

### Stage 1 — First real run · *15 minutes*

> *"Can it do my work, on my data?"*

| | |
|---|---|
| **Does** | Installs an agent, connects one credential, runs against something real. |
| **UI** | Install → capability consent screen → connect Jira/Slack → run. |
| **CLI** | `vam login` → `vam install acme/jira-assistant` → `vam secrets set …` → `vam run "clean up sprint 42"` |
| **Value moment** | Real output, plus an evidence trail they can export. |
| **Needs** | `vam install`; capability consent UX; secrets onboarding that does not require reading three docs; **`vam run <agent> --prompt` without a `run.json`** (§8.2). |

Note what is absent: no JSON authored, no policy written, no `run.json`, no
local daemon, no Lima. If any of those appear at Stage 1, Stage 1 is broken.

---

### Stage 2 — Make it yours · *1 hour*

> *"Almost right — I need it to behave differently."*

| | |
|---|---|
| **Does** | Changes instructions, adds a skill, tightens a permission. |
| **UI** | Edit instructions inline; "Add skill" from marketplace; toggle a granted capability. Saving creates a **fork by overlay**. |
| **CLI** | `vam fork acme/jira-assistant` → edit `agent.json` → `vam add skill myorg/internal-kb` → `vam run` |
| **Value moment** | A customised agent that still **tracks upstream** — `vam upgrade` shows what changed. |
| **Needs** | **`extends` (§13.3)** — the whole stage is blocked on it. Plus `vam add`, `vam fork`, `vam upgrade`. |

This stage is where most users will live permanently, and it does not exist
today. It is the single highest-leverage gap in the journey.

---

### Stage 3 — Author a skill · *1 day*

> *"No existing skill does this. I'll write one."*

| | |
|---|---|
| **Does** | Scaffolds a skill, writes a tool, declares capabilities, iterates. |
| **UI** | Web editor for simple cases; otherwise "open in your editor". |
| **CLI** | `vam init skill my-thing` → edit → `vam validate` → `vam dev` → iterate |
| **Value moment** | Edit → see the tool work, **in under five seconds**. |
| **Needs** | `vam init` templates; **`vam dev` — a local, microVM-free loop** with a mock host broker (there is no local/mock mode today); `vam validate` with cross-file checks; JSON Schema published for editor completion; `vam policy explain`. |

`vam dev` is the most important unbuilt command in this document. Authoring is
where developers are won or lost, and a loop measured in microVM boots loses
them. A guest-less local executor that stubs host tools and evaluates Cedar
in-process is enough — and Cedar already evaluates host-side, so this is
tractable.

---

### Stage 4 — Harden · *within the first week*

> *"I want to run this where it matters."*

| | |
|---|---|
| **Does** | Writes policy, adds tests, sets HITL on mutating tools, reviews evidence. |
| **UI** | Policy editor with live "what would this allow?"; approval inbox; evidence viewer. |
| **CLI** | `vam policy test` · `vam policy explain` · `vam validate --strict` · `vam evidence verify` |
| **Value moment** | A test proves a dangerous action is blocked — **and** that the intended one is not. |
| **Needs** | Layered policy sets (§11); `vam policy test` fixtures; scoped-permit validation; org baseline as a real artifact. |

This is where the security persona enters, and where the governance model
becomes tangible to the reviewer.

---

### Stage 5 — Ship and operate · *ongoing*

> *"It's live. Don't let me break it."*

| | |
|---|---|
| **Does** | Versions, releases, deploys to an environment, monitors, rolls back. |
| **UI** | Environments view; "staging runs 3.1, prod runs 3.0"; promote; rollback; cost and failure dashboards. |
| **CLI** | `vam release --version 3.1.0` · `vam deploy staging` · `vam promote staging prod` · `vam rollback prod` |
| **Value moment** | A bad release is rolled back in one command, and evidence shows exactly what it did while live. |
| **Needs** | **A named deployment concept (§5.3)** — absent today; environments; release CLI verbs for agents and policies (P3); CI recipes. |

---

### Stage 6 — Distribute · *when ready*

> *"Others should use this."*

| | |
|---|---|
| **Does** | Claims a namespace, publishes, supports consumers, ships upgrades. |
| **UI** | Publisher dashboard: installs, versions, issues, capability-change review. |
| **CLI** | `vam namespace claim` · `vam publish` · `vam release` · `vam yank` |
| **Value moment** | Someone else installs it and it works unmodified. |
| **Needs** | Namespace verification; publisher signing; capability diff on upgrade; deprecation and yank semantics. |

---

### 21.1 The journey as a single table

| Stage | Time | Artifact they touch | Blocking gap |
|---|---|---|---|
| 0 Evaluate | 5 min | none | hosted playground |
| 1 First run | 15 min | a prompt | `vam install`, promptable run |
| 2 Customise | 1 hr | `agent.json` overlay | **`extends`** |
| 3 Author | 1 day | skill manifest + code | **`vam dev`**, `vam validate` |
| 4 Harden | 1 week | `policy.cedar`, tests | **layered policy** |
| 5 Ship | ongoing | `vam.lock`, env | **deployments** |
| 6 Distribute | ongoing | published packages | namespaces, signing |

Read the right-hand column as the roadmap. Four of six blocking gaps are already
Phase A/B items in §17 — the journey view does not add work, it **orders** it by
where a human gets stuck.

---

## 22. Solving marketplace supply: adopt MCP

"Lakhs of skills" will not materialise from an empty registry. Every marketplace
faces this, and the only reliable solutions are to import an existing ecosystem
or to generate one.

**MCP is the existing ecosystem**, and the repository has already designed for
it: `docs/…/runtime/mcp.md` specifies local MCP servers spawned inside the guest
over STDIO JSON-RPC, lazily started and idle-reaped after 300s. It is marked
planned, not implemented.

### 22.1 Why this is the strongest available move

MCP servers are widely available, widely adopted, and **widely feared** — they
are arbitrary code with ambient credentials, and organisations are actively
struggling with whether to allow them. VAM's answer writes itself:

> *Run any MCP server inside a hardware-isolated microVM, with its capabilities
> declared, policy-enforced, human-approved, and fully audited.*

That is simultaneously a supply solution and a direct use of the governance
model (§11, §15.1). It converts VAM's strengths into an immediate
catalogue.

### 22.2 What it requires

1. Implement the designed guest-side MCP host.
2. **Map MCP tools into the VAM capability model** — an imported server's tools
   must land in the qualified Cedar namespace (§15.1) and produce a structured
   capability list (§11.3). This is the real work and it is what makes the
   proposition true rather than marketing.
3. `vam import mcp <server>` — wrap an MCP server as an installable VAM skill,
   with capabilities inferred from its tool list and confirmed by the installer.
4. Publish inferred capabilities so the consent screen is honest.

### 22.3 The honest caveat

An imported MCP server is only as trustworthy as the capability inference. Be
explicit in the UI about what was **declared** versus **inferred**, and default
inferred capabilities to "prompt" rather than "auto-grant". Overclaiming here
would undermine exactly the credibility this strategy is meant to build.

---

## 23. What I would do first

If the goal is a developer experience that holds together end to end, and given
that Part I's cleanup items (§9.1–2) are cheap and should happen regardless:

1. **Decide who the journey is designed for**: the developer building an agent,
   or also the platform engineer who approves agents for production (§14).
   Everything below inherits from it.
2. **`extends`** (§13.3). Unblocks Stage 2, where most users will live. Small
   schema change, disproportionate effect.
3. **`vam dev`** (Stage 3). The inner loop is the product; nothing else in the
   authoring experience matters until this is fast.
4. **Layered policy sets** (§11). Unblocks Stage 4, the security persona, and
   the marketplace simultaneously.
5. **Golden base image, properly stocked** (§19.5). Defer `vam-build`; most of
   its 21 documents may never need implementing.
6. **MCP import** (§22). Supply and governance in one move.
7. **Deployments and environments** (Stage 5). The last gap before VAM is
   operable in production by someone who did not build it.

Deliberately *not* first: the Vamfile build system, HSP marketplace brokers, and
the full registry. Each is real work that the journey does not yet reach.

---

## 24. Playbooks: the dual-layer proposal (answers DX-80)

Status: **accepted** 2026-09-24 by the product owner. Locked as tracker
decision 16, superseding an earlier prose-only ruling recorded in a previous
session by a different tool. That earlier ruling is void; it was not the
owner's intent.

### 24.0 What changed from the superseded ruling

| | Superseded ruling | Accepted |
|---|---|---|
| Step prose | only content of a step | still required on every step |
| Tool binding | banned | optional, with a mode |
| `advisory` / `enforced` | rejected | **still rejected** |
| Scheduler, retry, CEL | deferred | **still deferred** |

Only the tool-binding clause changed. One part of the superseded ruling was
right and is kept: per-step `advisory` vs `enforced` is redundant, because a
tool needing approval carries that through the HITL overlay wherever it is
called from. Restating it on the step is duplicate policy that will drift.

Its grounds for banning tool bindings — "a mixed graph has no defined join" —
did not survive review. That is an argument about graphs, and it does not
reach a bound call in a linear sequence, where there is no join. The actual
reasons to bind a tool are fewer inference round trips, reproducible runs, and
a playbook an installer can read before installing it. None were addressed.

The honest limit of this decision: binding adds no *safety*. Tool calls inside
the agent loop already pass Cedar and HITL. Binding buys determinism, cost
control, and inspectability — not new isolation.

### 24.1 The finding that changes the decision

**Playbooks are stored but never executed.** The catalog machinery is complete
— `put_playbook`, `get_playbook`, publish, release, version, digest
(`catalog.rs:663-700`). The schema is a full workflow engine: `depends_on`,
`RetryRule`, `JoinPolicy` with quorum, `fallback_step_id`,
`StepEvaluationPolicy`, and `ConditionalAction` carrying a CEL expression.

None of it runs. `PlaybookStep.step_action` is read in exactly one place,
`vam-spec/src/validation.rs:390`, and only for the `Condition` variant, and
only to validate it. There is no DAG walker, no step scheduler, no dispatch.
The orchestrator's one execution path, `driver.rs:64`, hardcodes a single
`PromptAction` and sends it to the guest. The only `depends_on` graph logic in
the orchestrator belongs to HSP hooks (`hsp_hooks.rs:441-524`), not playbooks.

So DX-80 is not "keep the DAG engine or replace it." Nothing is built. The
question is what to build, and the sunk cost is one proto message and its
validator. That makes the cheaper, softer design easy to choose.

### 24.2 What already exists and is worth keeping

The execution primitive for both layers is **already implemented in the guest**:

- `ToolAction` — `vam-runtime.rs:488` — direct tool call, no ReAct loop.
- `PromptAction` — `vam-runtime.rs:549` — hand the instruction to the agent loop.

So "call this tool directly" and "let the agent work it out" both already
execute. What is missing is a playbook-level layer that chooses between them
per step. That is exactly the proposal.

### 24.3 The one schema change that matters

Today the two are a **`oneof`**:

```proto
oneof step_action {
  PromptAction prompt_action = 4;
  ToolAction   tool_action   = 5;
  ...
}
```

`oneof` means a step is *either* prose *or* a tool call. The proposal needs a
step to be **prose and optionally a bound tool** — both at once. That is the
schema change: stop making them alternatives.

```proto
message PlaybookStep {
  string id          = 1;
  string description = 2;      // human/UI summary

  string instruction = 20;     // the verbal layer. Always present.
  StepBinding binding = 21;    // the tool layer. Optional.
  StepMode    mode    = 22;    // bound | assisted | agentic

  repeated string depends_on = 3;    // keep; see 24.6
}

message StepBinding {
  string tool_name = 1;
  google.protobuf.Struct arguments = 2;  // literal or templated; see 24.5
}
```

### 24.4 One axis, not two

An earlier draft of this section proposed a second axis, per-step `advisory`
vs `enforced`. **Withdrawn** — see 24.0. Enforcement stays on tools, where the
HITL overlay already guarantees it.

What remains is the axis the proposal actually described — **who supplies the
arguments:**

| Mode | Tool | Arguments | Model called? |
|---|---|---|---|
| `bound` | fixed by author | fixed by author | no |
| `assisted` | fixed by author | model fills from context | yes, constrained |
| `agentic` | model chooses | model chooses | yes, free |

The proposal named the two ends. `assisted` is the middle and it is the one
most real steps need: "run this exact tool, but work out the arguments from
what the previous step produced." Without it, authors are pushed to `agentic`
for anything non-constant, and the determinism benefit is lost.

Note that `agentic` is what locked decision 16 already allows — it is today's
prose-only step under a name. So this is not a replacement of decision 16. It
is two additional modes alongside the one it already permits.

### 24.5 Why `bound` is the governance win, and the hole to avoid

A `bound` step's arguments are known at authoring time. That means its
canonical args digest can be computed at **publish** time, using the
`canonical_args_digest` that Phase 0 just centralised in `vam-spec`. An installer
can see exactly what a playbook will do before running it, and the capability
check can happen before the run starts rather than mid-flight.

**The hole:** `instruction_template` already uses `{{steps.x.output}}`
interpolation. The moment `StepBinding.arguments` allows the same, a "static"
argument is no longer static and cannot be pre-approved. Do not let this blur.

Split it explicitly:

- **literal** arguments — no interpolation. Digest computable at publish.
  Pre-approvable. This is what makes a playbook inspectable.
- **templated** arguments — contain `{{…}}`. Digest computable only at call
  time. Must go through the normal runtime PEP like any other call.

Validate at publish and label the step accordingly. A playbook that claims to
be deterministic while interpolating prior output is exactly the kind of false
governance claim that is worse than no claim.

### 24.6 Keep `depends_on`, drop the workflow engine

Keep: `id`, `description`, `depends_on`. They are cheap, and ordering is real.

Defer until someone asks: `RetryRule`, `JoinPolicy` (especially `QUORUM`),
`required_successes`, `fallback_step_id`, `StepEvaluationPolicy`, and
`ConditionalAction`'s CEL expression. Each is a workflow-engine feature with a
scheduler behind it, none has ever run, and shipping a CEL evaluator to make a
recipe work is a large detour. Note the existing comment on `depends_on`
already says "Alpha permits at most one dependency per step" — the schema
author had already reached for linear.

Default to a **linear list**. Treat `depends_on` as the rare escape hatch.
Most playbooks are "do this, then this, then this."

### 24.7 The failure mode to design against

If a step carries both prose and a bound tool, and the prose is injected into
the model's context on a `bound` step, the model may redo the work the bound
call already performed, or argue with its result. Define it once:

- `bound` — `instruction` is **documentation**. Shown in traces, the UI, and
  `vam playbook explain`. Not injected as an instruction.
- `assisted` — `instruction` is injected as **argument-filling context** only.
- `agentic` — `instruction` is injected as the instruction.

### 24.8 Falls out for free

- **DX-13 `requires`** mostly derives itself. Every `bound` and `assisted`
  step names a tool, and each tool resolves to a skill. Only `agentic` steps
  need an explicit declaration. Derive what you can, require the rest.
- **A trust signal for the marketplace.** The ratio of bound/assisted to
  agentic steps is a real, computable inspectability measure. "This playbook is
  80% deterministic" is a claim that can be verified from the artifact, which
  is a better marketplace signal than a star rating.
- **`vam playbook explain`** becomes trivial and valuable: print each step, its
  mode, its tool, and its literal arguments. That is the "read the recipe
  before you buy it" experience.

### 24.9 What this costs

Proto: one message reshaped, two enums, one new message. Validator updates.
Executor: a linear walker that dispatches to the two guest paths that already
work. No scheduler, no CEL, no retry engine.

The risk is not technical. It is that `agentic` becomes the default because it
is the easiest to author, and playbooks end up as prose with extra steps. The
counter is tooling, not schema: make `vam playbook explain` show the
determinism ratio, and make `bound` the mode the templates and docs reach for
first.

---

## 25. What the phases do *not* deliver: MCP auth and playbook execution

Status: **brainstorm**, raised by the product owner after Phase 1. Two honest
gaps found by re-reading Phase 7 and Phase 10 against the code.

### 25.1 Short answer

No. Neither is complete as written.

| | Phases deliver | Missing |
|---|---|---|
| **Playbooks** | schema, install, template, compat check, listing | **the executor.** Nothing walks the steps |
| **MCP** | guest STDIO host, capability mapping, import, seed catalog | **authentication.** No credential path for real servers |

Both gaps have the same shape: the phase packages, tests and sells a thing
whose engine was never specified.

### 25.2 Playbooks: Phase 7 tests a runner that does not exist

Phase 7 is DX-82 install, DX-83 `vam init playbook`, DX-84 `vam playbook test`,
DX-85 compatibility, DX-86 marketplace listing. Read them together and the hole
is obvious: **DX-84 says "assert the step sequence" and DX-85 says "does the
playbook's `requires` match"** — both assume something executes steps. Nothing
does. As established in §24.1, `driver.rs:64` sends one hardcoded
`PromptAction` and there is no walker anywhere.

So Phase 7 as written produces an installable, testable, purchasable artifact
that cannot run. The executor needs to be a named item, and it is the largest
single piece of work in the phase.

### 25.3 The real playbook question: how does an agent *consume* one?

This is the part the tracker never asked, and it matters more than the walker.
There are two possible architectures and they produce different products.

**Option A — the orchestrator drives.** The host walks steps, and for each one
sends the guest either a `ToolAction` (bound) or a `PromptAction` (agentic).
The agent is a worker; the playbook is the program.

- Deterministic, inspectable, easy to trace and bill.
- Every step is a fresh host round trip, so the agent loses continuity between
  steps and cannot carry working context forward.
- Fights decision 16, which says the agent may reorder, skip, or repeat.
  An orchestrator that walks a list cannot let the agent reorder it.

**Option B — the agent drives.** The playbook is loaded into the guest as
context. The agent reads it, follows it, and calls tools itself through the
normal PEP path. Bound steps become pre-approved tool calls it may fire without
argument synthesis.

- Matches decision 16 exactly: reorder, skip, repeat are natural.
- Keeps continuity — one reasoning session, not N disconnected ones.
- Weaker guarantees. "Did it actually follow the playbook?" becomes an
  observation after the fact, not a property of the system.

**Recommendation: B, with A's evidence trail.** The agent drives, but every
step transition is emitted as a telemetry event, so conformance is *measured*
rather than enforced. This fits what VAM already is — the evidence pack and
trajectory replay machinery exists (`vam-telemetry/src/replay.rs`,
`evidence.rs`), and governance-by-audit is the model. A playbook
then ships with a conformance report: which steps ran, in what order, which
were skipped. That is a better marketplace artifact than a promise, and it
does not require building a workflow engine.

Concretely, the agent needs the playbook in its context in a form it can act
on. That is an instruction-layer question, and §19a already locked three
layers. A playbook is a **fourth, run-scoped layer** — it is not the platform,
not the agent's purpose, not a skill's usage notes. It is "what we are doing
this run." Decide where it sits before building either option.

### 25.4 MCP: the structural conflict

VAM's secret model is genuinely good and it is the reason MCP is hard.

**How it works today.** The guest never holds a credential. It receives a
placeholder — `vamsec_<binding_id>_<32 hex>`, minted per session in
`vam-network/src/secrets.rs:46-58` — injected as an environment variable
(`exec.rs:563-567`). The skill writes that placeholder into a URL or header.
The host egress proxy substitutes the real value on the way out. There is a
scope model already (`org` / `user` / `either`, `secret_requirements.rs`) and
output is scanned for vault values and replaced with `***`.

**Why MCP breaks it.** A third-party MCP server is a binary that expects a
*real* credential in its environment and makes its *own* network calls. It
knows nothing about VAM placeholders. Three failure modes, in increasing order
of nastiness:

1. **Format validation.** Many clients check a token looks like a token before
   using it. `vamsec_github_a3f9…` fails that check locally, before any
   network call, so substitution never gets a chance.
2. **Non-verbatim use.** Substitution only works if the credential appears
   literally in the outbound bytes. An OAuth refresh exchange, an HMAC
   signature, or a JWT assertion all *derive* from the secret. A placeholder
   cannot be derived from.
3. **OAuth entirely.** **There is no OAuth anywhere in the codebase** — zero
   matches across all crates. GitHub, Google, Slack, Notion — the servers worth
   importing — are OAuth. Static tokens are the exception now, not the rule.

**The doc already conceded this.** `docs/runtime/mcp.md:85` says to use guest
MCP "when the tool server can run safely inside the VM with no host secrets"
and host-provided tools otherwise. Follow that through: **every MCP server
worth importing needs credentials, so almost none of them qualify for the
guest path.** DX-110 implements the guest STDIO host, which unlocks only the
uncredentialed minority — filesystem, time, calculator. Those are not why
anyone wants MCP.

### 25.5 A way through, in order of cost

**Tier 1 — uncredentialed guest MCP.** What DX-110 already describes. Ship it,
but describe it honestly: it is the local-tools tier.

**Tier 2 — MCP over the host tool adapter.** The interesting move. The
`HostToolAdapter` path already exists and is marked implemented
(`docs/runtime/host-tools.md:19`) — it runs tools on the host, after the same
Cedar PEP, with vault access the guest never sees, and it already redacts
vault values from output. **Run the MCP server host-side as an adapter.** The
guest calls a tool; the host process holds the real credential and speaks MCP.
Zero-trust is preserved because the server is on the trusted side, and every
call still passes the PEP. This reuses machinery that is built rather than
inventing a credential path.
Cost: the MCP server is now trusted host code, so admission control and
sandboxing of *that* process becomes the security question. It moves the
problem rather than removing it, but it moves it somewhere there is already
policy machinery.

**Tier 3 — OAuth as a first-class subsystem.** Unavoidable for the top
servers, and it is a real subsystem, not a feature: authorization-code flow,
redirect handling, refresh-token storage, expiry-driven refresh, per-user and
per-org scoping, revocation. The `org`/`user`/`either` scope model in
`secret_requirements.rs` is the right foundation and should be extended rather
than duplicated. **This deserves its own workstream.** Folding it into a
marketplace-supply phase would hide a multi-week security-critical build
inside an item that reads like an import script.

### 25.6 What to do about it

1. Add a playbook executor item to Phase 7, and answer 25.3 (who drives)
   before writing it. It is a locked-decision-class question.
2. Decide where a playbook sits in the instruction layering (§19a) — the
   likely answer is a fourth, run-scoped layer.
3. Re-scope DX-110 to say plainly that it unlocks uncredentialed servers only.
4. Add the host-adapter MCP tier as the item that makes MCP actually useful.
5. Open a separate design doc for OAuth. Do not let it hide inside Phase 10.

---

## 26. MCP: how the credential gets there

Status: **accepted** — locked decision 20, Phase 2.5 and Phase 10.

> **Read 26.0 first.** Sections 26.1-26.5 were written assuming *local* MCP
> servers and reach a broker-shaped conclusion. That conclusion is correct for
> local servers and wrong for the common case. 26.0 sets the frame.

### 26.0 Two kinds of MCP server, and only one is hard

| | Remote (Jira, Linear, Sentry) | Local / stdio (`npx @vendor/server`) |
|---|---|---|
| Where it runs | the vendor's HTTPS endpoint | a process on your host |
| What VAM needs | **a client** | **a host for someone else's binary** |
| Credential | host attaches a header | broker's child environment |
| Sandboxing | not our problem | entirely our problem |
| Phase | **2.5, core** | 10 |

Almost everything people actually want is the left column. There is no process
to host, so there is no broker, no confinement question and no supply-chain
question — the vendor already runs the server.

**So remote MCP is one client inside `vamd`, and the code is already there in
another shape.** `host_tools_http.rs:137-214` resolves a per-caller vault
secret, attaches it as an `Authorization` header, calls out and returns the
result. A remote-MCP client is that flow with a JSON-RPC body:

```
guest calls jira.create_issue
  → orchestrator resolves the tool  (host_tools.rs:408)
  → decrypt_secret_for_caller(caller_user_id, "jira/auth-header")
  → POST {"method":"tools/call", ...}  with the header attached
  → result back to the guest
```

The earlier "no third-party code in `vamd`" warning still stands — it just does
not apply here. We write the client; we never load the vendor's binary.

Per-user credentials need nothing new either. The vault key is already
`(scope, owner_user_id, binding_id)` (`secrets.rs:150-205`), so `--scope user`
means the same agent run by two people transparently uses two tokens.

Tools reach the guest as ordinary `host_exec` skill tools with
`adapter_id: mcp:<connection>:<tool>`. **The guest cannot tell an MCP tool from
any other tool**, which is the whole point: no new proto, no runtime changes,
and Cedar, HITL and telemetry keep working with no MCP-specific cases.

The one real risk that survives is **tool poisoning** — a remote server can
change its advertised tools between calls. Pin the `tools/list` snapshot at
connect and require re-approval on drift; `manifest_is_subset`
(`hsp_registry.rs`) already implements that comparison.

Everything below concerns the local/stdio tier only.

---

### 26.1 (local tier) Stop trying to pass the secret to the server


The `vamsec_` placeholder exists for one shape of problem: **guest code that
builds an HTTP request**. The skill writes the placeholder into a header, the
host egress proxy swaps it for the real value on the way out, and the guest
never holds the credential. That works because the skill is *our* code and the
placeholder travels verbatim.

An MCP server is the opposite shape. It is third-party code that wants a real
credential in its own environment and calls the API itself. As recorded in
§25.4 it will validate the token's format, or derive from it rather than send
it, or both.

**So the answer is not to pass a secret into the guest at all.** Move the MCP
server to the host, where the real credential already lives. The guest keeps
receiving what it receives today: a tool name, arguments, and a result.

### 26.2 (local tier) Host tool adapter or HSP? HSP

These are two different extension models and only one can host foreign code.

| | `HostToolAdapter` | HSP broker |
|---|---|---|
| Location | **in-process**, compiled into `vamd` | **separate process** |
| Transport | Rust trait call (`host_tools.rs:56-59`) | gRPC over UDS (`hsp.rs:19-24`) |
| Vault access | direct `&SecretsVault` on the context | host resolves, sends `resolved_secrets` |
| Crash blast radius | takes `vamd` with it | contained |
| Can host third-party code | **no** | yes |

`HostToolContext` (`host_tools.rs:43-54`) hands an adapter a live
`Option<&SecretsVault>`. An adapter *is* trusted VAM code with the vault in
reach. Loading a third-party MCP server there means arbitrary code in the
orchestrator's address space next to every secret. That is not a tradeoff, it
is a non-starter.

HSP is already the out-of-process model, and — importantly — **it already has
the admission machinery MCP needs.** From `hsp_registry.rs`:

- `require_enabled_approval` — a broker must be explicitly approved before any
  call is forwarded.
- `manifest_is_subset` — a broker cannot present more tools than were
  approved. This is exactly the defence against MCP **tool poisoning**, where
  a server changes its tool list after you trusted it.
- `approved_secrets` + `deny_extra_secret()` (lines 71-81) — the broker
  receives only the secret bindings named in its approved manifest. Ask for one
  more and the call is denied.
- `assert_trust_transport` — trust tier and transport must match, fail-closed.
- `assert_identity_satisfied` — per-caller identity requirements.

An MCP server wrapped as a broker inherits all of it for free. Nothing in that
list has to be built for MCP; it has to be *reused*.

### 26.3 (local tier) How many local MCP servers sit beside the host

One broker process per MCP server. Each has its own `broker_id`, its own UDS
path, its own approval record and its own approved tool and secret lists.
`hsp_hooks.rs` already orders brokers by `depends_on` and already detects
cycles (`hsp_hooks.rs:441-524`), so multi-broker lifecycle is solved.

Practically, the shape is: `vamd` ⟶ UDS ⟶ `vam-mcp-bridge` (first-party, one
per server) ⟶ STDIO ⟶ the third-party MCP server. The bridge is ours and
small: translate HSP `Invoke` to MCP JSON-RPC, translate `ListTools` to MCP
`tools/list`, hold the credential, and hand it to the child process in its
environment. The third-party binary never learns VAM exists.

### 26.4 (local tier) Security threats, honestly

This does not make the problem vanish. It relocates it to where there is
policy machinery, and it swaps a *guest* risk for a *host* risk. That is a
real trade and it should be stated plainly.

**What genuinely improves**

- The guest still never sees a credential. The zero-trust boundary is intact.
- Every call still passes the Cedar PEP before it reaches a broker.
- Tool poisoning is already blocked by `manifest_is_subset`.
- Secret over-request is already blocked by `deny_extra_secret`.
- A broker crash no longer takes down the orchestrator.

**What gets worse, and is not currently mitigated**

1. **A real credential now sits in a host process's memory.** Previously it
   only appeared inside the egress proxy at substitution time. Now it lives for
   the process's lifetime. Compromise the broker, keep the token.
2. **`vamd` does not spawn or confine brokers.** It only dials a UDS
   (`hsp.rs:281-300`); there is no `Command::new` in the HSP path. Process
   confinement — user, namespace, seccomp, cgroup, filesystem — is entirely the
   operator's job today and nothing in the repo does it. A broker is an
   ordinary host process with ordinary host access.
3. **Supply chain.** The usual MCP install is `npx @vendor/server`, which pulls
   and runs arbitrary code at install time, before any VAM policy applies.
   Approval covers what the broker may *do* through HSP; it does not cover what
   the package did while installing.
4. **Confused deputy.** The broker acts for whichever session called it. If one
   broker process serves several sessions and does not partition state, data
   can cross sessions. `caller_context` exists and identity is asserted, but
   per-caller *isolation inside the broker* is the broker's responsibility.
5. **Exfiltration through results.** Broker output lands in agent context.
   Vault values are redacted from output (`host-tools.md:86`), but a
   compromised broker can return anything else it can reach.

**Why this is acceptable right now:** HSP is operator-installed and stays that
way — it is already in this tracker's "Explicitly later" list as explicitly not
a marketplace item. The trust model is "an administrator chose to run this
binary," which is the same trust model as any other daemon on the host. The
danger is only if MCP import later becomes a one-click user action; then
threats 1-3 escalate from acceptable to serious, because the person clicking is
no longer the person who vetted the code.

### 26.5 (local tier) The line not to cross

**Do not let `vam import mcp` install and run a broker without an operator
approving it.** DX-112 as written ("wrap an MCP server as an installable VAM
skill") reads like one-click install, and one-click install of a host process
holding a real credential is the failure mode. Keep import as a two-step act:
generate the broker manifest and the wrapper, then require explicit operator
approval — the machinery for which already exists in `hsp_store.rs` and
`hsp_registry.rs`.

If a one-click path is wanted later, it needs broker sandboxing (threat 2)
built first, not retrofitted.
