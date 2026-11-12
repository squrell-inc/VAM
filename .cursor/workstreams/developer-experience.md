# Workstream: Developer experience

```yaml
status: active
slug: developer-experience
current_phase: 3
design_docs:
  - design/developer-experience.md
```

## Goal

A developer can find an agent, run it, change it, write their own skill, secure
it, ship it, and publish it — without hand-writing protobuf JSON, without
booting a microVM to test, and without copying other people's code into their
repo. Every stage is a place they can stop and still have something useful.

## Out of scope

- HSP brokers as marketplace items. HSP stays operator-installed and
  first-party. Revisit after Phase 10.
- The Tauri UI app. It lives in its own repo. This tracker only guarantees that
  every capability is reachable over the HTTP API so the UI can consume it.
- `vam-build` / Vamfile custom images. Escape hatch, not the path. See Phase 9.
- Rewriting the agent loop, telemetry, or isolation.

## Locked decisions

Do not re-litigate without editing this list.

1. **API first.** CLI and UI are both clients of `vamd`. No capability is
   UI-only. No capability is CLI-only.
2. **The CLI is also an LLM interface.** Every command gets `--json`, stable
   exit codes, no interactive prompt when stdin is not a TTY, and errors that
   state the fix.
3. **camelCase** is the canonical JSON casing for all VAM files. Examples get
   converted once.
4. **URNs are fully qualified** everywhere, including Cedar principals.
5. **`run.json` becomes optional.** `vam run <agent> --prompt` is the primary
   path. The file is kept for reproducible/batch runs only.
6. **Skills declare capabilities twice**: a structured list for humans and
   diffs, and a Cedar fragment for the PDP. Publish-time check proves the Cedar
   is a subset of the structured list.
   **Amended 2026-10-11** (`design/agent-experience.md` Part IV, decision 9):
   skills ship no Cedar. The structured list stays, for consent.
7. **A skill's Cedar fragment may only `permit` its own tools.** Enforced at
   publish. Skills may not `forbid`.
   **Amended 2026-10-11** (`design/agent-experience.md` Part IV, decision 9):
   skills ship no Cedar fragment. A skill may mark its own tools
   `approvalRequired` (tighten-only data), which can reach no other tool.
8. **Org and platform policy layers are `forbid`-only.** They cannot be
   escalated past, by Cedar's own semantics.
9. **Do not invent a VAM package manager.** Skills declare dependencies in
   their own ecosystem's native manifest + lockfile; VAM runs the standard
   installer host-side. Nix is not adopted: per-skill install directories give
   the collision isolation that was its main justification.
9a. **Agents own the machine; skills own their language packages.** Image
   customization (Vamfile, extra runtimes) is the agent author's call. A
   marketplace skill must never require the installer to rebuild an image; it
   declares `requiresRuntime` and install fails fast if unmet.
9b. **No network installs at run time.** Dependencies resolve on the host
   before boot. A guest that can `npm install` mid-run has arbitrary egress and
   non-reproducible runs.
9c. **Dev does not need isolation.** `vam dev` defaults to no VM and no
   container. Fidelity is bought in tiers, not paid for on every edit.
10. **Digest `""` means unpinned.** Stop using `sha256("")` as a sentinel.
11. **Lockfile records resolved capabilities**, not just versions. CI fails when
    resolved capabilities exceed granted ones.
12. Phases ship in order. A phase is done when its exit criteria pass, not when
    its code merges.
13. **A value that crosses the trust boundary gets exactly one canonicalisation
    implementation**, in a shared crate, called by both sides. Host and guest
    have already drifted once (DX-9). A second copy is a bug waiting on a
    calendar.
14. **A workaround in a skill is a platform bug report.** When a skill hardcodes
   a path, flattens a type, or reshapes an argument to get through, the fix
   belongs in the platform. Track it, do not normalise it.
15. **Canonicalize nested arguments; do not drop them.** The arguments digest
   is the binding between what was authorized and what runs. `vam-spec`'s
   `canonical_args_digest` is the only implementation, called by host and
   guest. Arrays, nested objects, null, and empty structs are part of the
   digest. Object keys are sorted. A non-finite number or a protobuf value
   with no kind fails the call. Silently omitting a value is not allowed:
   that would leave it unsigned. Rejecting nested objects instead would push
   skills back to flattening arguments into strings.
16. **A playbook is a dual-layer procedure, not a DAG (DX-80).** Revised
    2026-09-24 by the product owner, superseding an earlier prose-only ruling.

    **Amended 2026-10-09 by the product owner** (`docs/src/content/docs/runtime/workflow.md`): the
    host now tracks step state and rejects illegal transitions. The agent may
    still reorder available steps but may not skip a required step.
    Enforcement of effects stays on tools, now including deny-only playbook
    guards in the PEP. Prose-only playbooks are removed. Everything else below
    stands.

    **Layer 1 — prose, always present.** Every step carries an `instruction`
    in plain language. The agent may reorder, skip, or repeat steps. There is
    no scheduler.

    **Layer 2 — an optional tool binding.** A step *may* also name a tool, with
    a `mode` saying who supplies the arguments:

    | Mode | Tool | Arguments | Model call? |
    |---|---|---|---|
    | `bound` | author | author | no |
    | `assisted` | author | model fills from context | constrained |
    | `agentic` | model | model | free — today's prose-only step |

    `agentic` is the existing behaviour under a name, so this adds two modes
    beside it rather than replacing it. Both execution primitives already work
    in the guest: `ToolAction` at `vam-runtime.rs:488`, `PromptAction` at
    `:549`. Only playbook-level dispatch is missing.

    **Stop making `step_action` a `oneof`.** A step needs prose *and* an
    optional binding. The `oneof` forces a choice between them and is the one
    real blocker.

    **Still rejected — per-step `advisory` vs `enforced` (DX-81).** A tool that
    needs approval carries that itself through the HITL overlay, so the
    guarantee holds wherever it is called from. Restating it on the step is
    duplicate policy in a second place, and the two will drift. Enforcement
    lives on tools.

    **Still deferred — the workflow engine.** Do not grow `retry_policy`,
    `fallback_step_id`, `join_policy`, `required_successes`,
    `evaluation_policy`, `ConditionalAction`, or `ApprovalAction`. Each needs a
    scheduler; none has ever run. Keep `depends_on` as a rare ordering escape
    hatch, default to a linear list.

    **Context:** playbooks are stored but never executed today —
    `PlaybookStep.step_action` is read only at `vam-spec/src/validation.rs:390`,
    only for `Condition`, only to validate. `driver.rs:64` hardcodes one
    `PromptAction`. So this decides what to build, not what to replace.
    Rationale in full: `design/developer-experience.md` §24.
17. **These `AgentSpec` fields move to an environment profile (DX-17).**
    They are how a deployment runs, not why the agent exists:
    - `preferred_models` (`provider`, `model_name`, `temperature`, `top_p`,
      `max_completion_tokens`)
    - `model_requirements.max_cost_per_million_tokens`
    - `context_policy` (`max_token_count`, `max_history_turns`, `compaction`)
    - `reasoning_policy` (`max_iterations`, `step_timeout`, `termination`)
    - `resource_requirements.vcpu_count`
    - `resource_requirements.memory_mib`
    - `resource_requirements.storage_gib`
    - `resource_requirements.network_mode`

    The capability floor stays on the agent: `model_requirements` fields
    `min_context_tokens`, `requires_tool_calling`, `requires_structured_output`,
    and `required_modalities`. The profile picks a model that meets that floor.
    Purpose fields stay: identity, role, instructions, skill refs, playbook
    refs, and `capability_policy`.
18. **`hitlPolicy` joins the Cedar policy set as a generated overlay;
    `telemetryPolicy` stays its own ref (DX-18).** Do not collapse both into
    Cedar actions. `monitoredTools` stays structured. `derive_hitl_overlay`
    already emits `ExecuteTool` permit/forbid on `context.hitl_approved`.
    There is no new HITL action, and `HitlPolicyRef.policy_ref` is not a
    second Cedar document. `pii_redaction_enabled` and `audit_ledger_mode`
    are operational settings Cedar cannot express, so `telemetryPolicy` stays
    a separate ref. `capability_policy` and `completion_policy` are unchanged
    by this decision.
19. **The canonical agent package is the tree in DX-19.** A consumed skill
    is a reference, never a directory. `skills/` holds only skills this repo
    authors, so third-party code stays out of the developer's Git history.
    `.vam/` is the resolved cache and is gitignored. Documented in
    `docs/src/content/docs/authoring/agent-package.md`. `vam init` scaffolding
    is Phase 2 (DX-50); this decision locks the layout only.
19a. **Three instruction layers, one owner each (DX-19A).** No package may
    cross them, and no package may write `SYSTEM.md`:

    | Layer | File | Owner | Writable by a package? |
    |---|---|---|---|
    | Platform rules | `workspace/.vam/SYSTEM.md` | VAM | **no** |
    | Agent purpose | `agent.json` `systemInstructions` | agent author | yes |
    | Skill usage | `SKILLS.md` `system_extension` | skill author | yes, on `load_skill` |

    The guest write-protects the path in `is_protected_write_path`
    (`crates/vam-runtime/src/primitives.rs`, `.vam/SYSTEM.md` alongside
    `.vam/workspace.json` and `.vam/threads/`). The host overwrites the file
    when `SHIPPED_TEMPLATE_VERSION` (`crates/vam-storage/src/instruction_template.rs`,
    now **9**) exceeds the file's frontmatter `template_version`. If a real
    need appears, add an explicit `systemInstructions.append` slot instead of
    opening the file. DX-9A edits the platform layer's content; that is VAM
    exercising ownership.
20. **The MCP client is core VAM, remote-first (Phase 2.5).** VAM ships its
    own JSON-RPC client inside `vamd`; MCP is a platform ability, not
    marketplace supply. Three tiers, and they must not be conflated:

    | Tier | Where the server runs | Credential | Phase |
    |---|---|---|---|
    | **Remote** (Jira, Linear, Sentry) | vendor's HTTPS endpoint | per-user vault row | **2.5, core** |
    | Local / stdio | a host process | HSP broker holds it | 10 |
    | OAuth vendors | either | needs a subsystem that does not exist | 10 |

    Consequences that are binding:
    - **The `vamsec_` placeholder is never used for MCP.** It works only
      because *our* skill writes it verbatim for the egress proxy to
      substitute. A third-party server validates token format, or derives
      rather than sends. The host calls out; the guest never holds a
      credential either way.
    - **No third-party code in `vamd`.** A `HostToolAdapter` gets a live
      `&SecretsVault` (`host_tools.rs:43-54`). First-party clients there are
      fine; vendor binaries are not, and that is what HSP is for.
    - **MCP tools reach the guest as ordinary `host_exec` skill tools**
      (`adapter_id: mcp:<connection>:<tool>`). The guest stays MCP-unaware, so
      Cedar, HITL and telemetry need no MCP-specific cases.
    - **Tool lists are pinned at connect and re-approved on drift** — the
      tool-poisoning defence. Refresh uses `relate_approved_set` (the same
      check as `manifest_is_subset`). Identical is a no-op. A strict subset
      is stored. An added tool, or a changed name, description, or schema,
      is not applied until `vam mcp refresh --approve-drift`.
    - **Guest tool names are `{connection}__{vendor}` (DX-134).** `-` in the
      connection id becomes `_`. Vendor characters outside `[a-z0-9_]`
      become `_`. That string is the Cedar resource id. Over 64 characters,
      a reserved builtin, or two tools that land on one string rejects the
      connection. `tools/call` keeps the vendor name in
      `mcp:<connection>:<tool>`.
    - **A connection takes effect at the next session start (DX-133B).**
      The catalog and guest skill files are written once, during session
      setup. `vam mcp connect` says a restart is required. A live session is
      not re-seeded.
    - **MCP capabilities are inferred skill tokens (DX-136, DX-137).**
      `tool:<guest name>`, `egress:<connection host>`, and `secret:<binding>`
      when a token is stored. Origin is `inferred`. Grant is `prompt`. No
      Cedar `permit` is written from a vendor description. The default side
      effect is class 3 (`context.side_effect == 3`). An operator lowers one
      tool with `--side-effect TOOL=read`. Descriptions are stripped of
      control characters, capped at 512 characters, and shown on the HITL
      approval as the text the model sees.
      **Amended 2026-10-15 by `agent-runtime` (E10 Phase 0):** the
      512-character cap on what the model sees is lifted, and the server's
      `instructions` are read. Vendor text is fenced as untrusted data and
      scanned before use. The cap is lifted only once that fencing has
      landed. The HITL approval shows a capped preview with the full text
      on request. Everything else in this decision stands.

19b. **`Vamfile` is not part of the agent package (DX-19B).** It answers
    what is in the machine image, a different axis from what the agent does.
    It stays out of the standard layout. It is the Phase 9 Tier 2 escape hatch.
    *Amended 2026-10-10 by the product owner (`design/agent-experience.md`
    Part III):* still its own project, now producing a versioned **image
    artifact** that an agent references with `imageRef`. It is a supported,
    opt-in path (E7), not only an escape hatch.
    Image customization remains the agent author's call (item 9a). A
    marketplace skill still must not require an image rebuild.
20. **`extends` inherits a pinned agent. It does not call one (DX-14, DX-15).**
    `AgentSpec.extends` is one `ResourceRef` to the immediate parent. Version
    and digest are required. The URN kind is `agent`. The child does not
    invoke the parent. There is no delegation and no second agent running.
    `merge_agent_chain` in `vam-spec` is the only merge. Run admission does
    not apply it yet.

    **One parent. Chains, at most 8 hops.** Each spec names only its parent.
    The caller passes the chain root-first: the agent that does not extend,
    then each child. A chain longer than 8 hops fails with `extends_depth`.
    A spec whose `extends` URN equals its own URN fails validation with
    `extends_cycle`. A chain that repeats an agent URN fails merge with
    `extends_cycle`. A loop with no unextended root fails with
    `extends_unresolved`. A child whose `extends` URN is not the previous
    agent's URN fails with `extends_parent`.

    **Only three axes merge.** Identity stays the child's. Models, context,
    reasoning, resources, Cedar and HITL and telemetry and completion refs,
    and playbook refs are not inherited. Instructions, skills, and
    capabilities are.

    | Axis | Verb | Effect |
    |---|---|---|
    | instructions | `append` | parent text, a blank line, then the child text |
    | instructions | `replace` | the child text only |
    | skills | `add` | union the skill ref into the effective set |
    | skills | `remove` | drop that skill URN; version and digest on the remove ref are ignored |
    | capabilities | `grant` | add the token |
    | capabilities | `revoke` | remove the token |

    No instruction op, and no skill or capability ops, means inherit that
    axis. `overlay` without `extends` fails validation
    (`overlay_without_extends`). An extending agent does not also set
    `instruction_source` or `skill_refs`; those fail with
    `instruction_source_not_overlay` and `skill_refs_not_overlay`. Append
    and replace text must be non-empty (`instruction_empty`).

    **Instructions and artifacts.** Append and inherit need the parent's
    inline text. If the effective instructions are only an
    `instruction_artifact`, append and inherit fail with
    `instruction_artifact_opaque`. Replace does not read the parent body
    and succeeds. A chain of one artifact agent merges with no inline text.

    **Skills. The diamond is a pin conflict, not a second parent.** Skill
    identity is the URN. The pin is version plus digest. A missing version
    or digest is a different pin from a present one. `remove` runs before
    `add`.

    - Add of a URN already in the effective set at the **same** pin keeps
      one ref.
    - Add of a URN already in the effective set at a **different** pin
      fails with `skill_version_conflict`. Neither pin wins. This is "A
      extends B, and both reference skill S at different versions",
      including when the older pin was introduced further up the chain and
      the immediate parent does not restate it.
    - `remove` plus `add` of the same URN is an explicit replacement, not
      a conflict. That is how a child changes a skill's version.
    - `remove` of a URN that is not in the set is a no-op.
    - Two `add`s of one URN at different pins in the same overlay fail
      with `skill_version_conflict`.

    **Capabilities.** After the skill edit, the required set is the union
    of capabilities declared by the skills that remain. The caller passes
    those declarations, keyed by skill URN. A missing entry fails closed
    with `skill_capabilities_missing`. An empty list means the skill
    requires nothing. Then apply `grant`, then `revoke`.

    - The same token in `grant` and `revoke` on one overlay fails
      validation with `capability_grant_revoke_conflict`.
    - **A revoke of a capability that a remaining skill requires fails
      with `capability_revoke_required`.** The error names the token and
      the skill URN. That includes a skill this child just added. The
      revoke does not punch a hole under a mounted skill. Remove the
      skill, or do not revoke the token.
    - A capability required only by a skill this overlay removes is no
      longer required. Revoking it succeeds.
    - Revoke of a token nothing requires is a no-op, so a parent upgrade
      that dropped the token does not break the child.
    - Tokens use the `tool:` / `egress:` / `secret:` / `fs:` shape
      (`capability_shape`).

## Status legend

- `[ ]` not started
- `[~]` in progress
- `[x]` done (code + tests, or merged)

## Phase map

| # | Phase | Why it exists | Unblocks |
|---|---|---|---|
| 0 | Ground truth | Examples are wrong; new users copy them | trust in everything else |
| 1 | Schema for intent | Skills can't carry policy, agents can't extend | 2,4,5,6,7 |
| 2 | Run & CLI ergonomics | Daily friction, cheapest win | every later stage |
| 2.5 | **MCP as a core capability** | An agent that can't touch Jira or Linear isn't useful; remote MCP needs no new trust model | real agent work, 7, 10 |
| 3 | Project manifest + lock | No unit of packaging; Git churn | 5,6,10,11 |
| 4 | Local loop | Testing needs a VM boot | skill authoring |
| 5 | Layered policy | One agent = one policy today | security persona, marketplace |
| 6 | Install & customize | No way to adopt and adapt an agent | the stage most users live in |
| 7 | Playbooks | Least served marketplace artifact | marketplace demand side |
| 8 | Deployments | No named, rollback-able deployment | production use |
| 9 | Dependencies | Skills can't bring their own deps | installable skills at scale |
| 10 | MCP — local servers & OAuth | The hard tail: stdio servers and OAuth vendors | marketplace |
| 11 | Marketplace | — | — |

---

## Phases

### Phase 0 — Ground truth

**Goal:** Nothing in the repo teaches the wrong thing, and no skill needs a
workaround for a platform defect. A machine check keeps it that way.
**Depends on:** none
**Exit:** every file under `examples/` round-trips through its declared proto
message in CI; one JSON casing; one URN form; a skill can call `bun` by name
and pass an array argument with no workaround.

- [x] **DX-1** Fix `sqlite-assistant/run.json` and `chaos-assistant/run.json`.
  They use `resource_id`, `timeout_budget`, `cost_budget` — none are fields of
  `RunSpec` or `CreateRunRequest`. `prost-reflect` denies unknown fields by
  default, so both fail to parse. Rewrite to the `CreateRunRequest` shape used
  by `jira-assistant`.
- [x] **DX-2** CI test: walk `examples/**/*.json`, decode each through
  `vam_spec::encoding::json_to_proto` for its declared message. Fail the build
  on error. This one test prevents the whole class.
- [x] **DX-3** Convert all examples to camelCase. Skill manifests are currently
  snake_case, agents are camelCase. Both are legal protobuf JSON, which is why
  it drifted.
- [x] **DX-4** Normalize Cedar principals to fully-qualified URNs. Delete the
  duplicated rule blocks in `examples/jira-assistant/policy.cedar` that exist
  only to cover both the short and long form.
- [x] **DX-5** Replace `sha256("")` digests with absent/empty. Keep
  `EMPTY_SHA256_HEX` working for one release, then remove.
  Examples omit `digest` instead of pinning SHA-256 of `""`. Resolvers still
  treat a missing digest, an empty `hex_hash`, and `EMPTY_SHA256_HEX` as
  unpinned. The constant is documented as deprecated and stays for one release.
- [x] **DX-6** Decide and document: does policy resolution verify the declared
  `ResourceRef.digest`? Today `security.rs` uses only the URN and silently
  drops version and digest. Either enforce it or delete the fields.
  **Recommendation: enforce the digest. Do not delete the fields. Not
  implemented.**
  `capability_policy_urn` keeps only `policy_ref.resource_id.urn`.
  `resolve_session_policy` loads that URN from the catalog. The declared
  version and digest are discarded, so a later catalog write changes the Cedar
  that runs with no pin mismatch.
  `ResourceRef.digest` is already a real pin for skill packages and agent refs
  (`digest_missing_or_placeholder`, `lookup_and_validate_package`). Deleting it
  would drop that guarantee for every ref, not just policy. Policy should use
  the same rule the skill resolver already uses: a missing digest, an empty
  `hex_hash`, or the deprecated `sha256("")` sentinel means "follow the catalog
  row"; any other digest must equal the catalog digest or resolution fails.
  Version selection is the same hole and is not part of this decision. Do not
  implement enforcement here.
- [x] **DX-7** Doc fixes: scope the `hosted/first-party.md` "nothing is
  implemented" line to files/RAG only; either wire the `EgressRequest` PEP or
  correct `security/cedar.md` to say egress is enforced by the network layer
  and adapter allowlists.
  **Choice: correct the doc. Do not wire an `EgressRequest` PEP.** Guest HTTP
  is already enforced by the network egress broker (domain allowlist and the
  protected destination deny set). Hosted-service adapters enforce
  `egress_domains` before invoke. Cedar never evaluates `Action::"EgressRequest"`.

**Platform defects found while building the coding-assistant agent.** These
three were reported from real agent work and each is verified below. They are
why skills currently need workarounds; every one of those workarounds is a
thing an end user should never have had to write.

- [x] **DX-8** **`/usr/local/bin` is missing from the guest `PATH`.**
  `crates/vam-runtime/src/exec.rs:494` sets `PATH=/usr/bin:/bin`, but both
  `scripts/build-rootfs.sh:54-56` and `scripts/build-rootfs-lima.sh:90-92`
  install `bun` to `/usr/local/bin/bun`. Any skill that invokes `bun` by name
  gets `127: not found`, so skills hardcode the absolute path. Fix: add
  `/usr/local/bin` to the injected `PATH`. Then delete the hardcoded paths from
  `examples/` (coordinate with DX-3). Add a test asserting the `PATH` contains
  every directory the rootfs build writes executables into — the two are
  defined in different files today and nothing keeps them honest.
- [x] **DX-9** **The canonical args digest has two implementations and they
  disagree.** Guest: `crates/vam-runtime/src/pep.rs:11-61` handles
  `ListValue`. Host: `crates/vam-orchestrator/src/security.rs:728-750` drops it
  on the `_ => {}` arm. So any tool called with an array argument produces
  different digests on each side, `pep.rs:346` finds
  `claims.arguments_digest != expected_args_digest`, and the guest rejects the
  capability token the host just issued. That is the "token mismatch" being
  worked around with comma-separated strings.
  **Fix the cause, not the symptom:** delete both copies, put one
  `canonical_args_digest` in a shared crate, and have host and guest call it.
  Two hand-maintained copies of a security-critical canonicalisation function
  will drift again. Add a test that round-trips arrays, nested objects, null,
  and an empty struct through both call sites.
  **Note the security half of this, which the report does not cover:** the
  digest is the binding between what was authorised and what is executed. Any
  argument the digest silently drops is an argument the capability token does
  not cover, and the evidence pack records a digest that does not represent the
  real call. Both implementations drop `StructValue`, so nested objects are
  unsigned on *both* sides — no mismatch error, no protection either. Decide
  explicitly: canonicalise nested values, or reject argument shapes the digest
  cannot cover. Do not leave a third option where it silently passes.
- [x] **DX-9A** **Strengthen `crates/vam-runtime/templates/SYSTEM.md`** with the
  universal working habits: read before editing, prefer surgical edits, how to
  reach the egress proxy. Content only — the question of who *owns* which
  instruction layer is DX-19A and stays there.
  **Two traps to respect.** The template is embedded with `include_str!` in two
  crates (`vam-runtime/src/instruction.rs:9` and
  `vam-storage/src/instruction_template.rs:8`), and existing workspaces only
  pick up changes when `SHIPPED_TEMPLATE_VERSION`
  (`instruction_template.rs:9`, currently `8`) is greater than the version in
  the file's own frontmatter. So: bump the constant **and** the
  `template_version` in the file's frontmatter together, or the edit reaches
  nobody. Second, this file is overwritten on upgrade, so anything written here
  is platform-owned and will erase a developer's edits at that path — keep it
  to universal habits, nothing agent-specific.

---

### Phase 1 — Schema for intent

**Goal:** The protos can express the product as described. Small change, but
Phases 2, 4, 5, 6 and 7 are all blocked on it.
**Depends on:** Phase 0
**Answer first:** **DX-80** (rigid DAG vs soft procedure) is listed under
Phase 7, but DX-12 and DX-13 add fields to `PlaybookSpec` and to the skill
manifest that point at it. Adding fields to a message whose nature is still
undecided is how schemas get churned. Answer DX-80 as a written decision before
starting DX-12/DX-13 — decision only, no Phase 7 code.
**Exit:** an agent can extend another; a skill can ship a policy and a playbook;
a playbook can declare the skills it needs; all with round-trip tests.

- [x] **DX-10** `SkillManifest.capabilities` — structured list
  (`tool:…`, `egress:…`, `secret:…`, `fs:…`). Human-readable, diffable.
- [x] **DX-11** `SkillManifest.policy` — a Cedar fragment shipped by the skill
  author.
  **Not loaded from untrusted sources until DX-62.** The fragment is stored and
  round-trips, but scoped-permit validation does not exist yet. A skill
  fragment must not be loaded into the policy set from an untrusted source
  before that check rejects an unrestricted `permit`.
- [x] **DX-12** `SkillManifest.playbooks` — procedures shipped with the skill.
  Each path is relative to the directory that contains `manifest.json`.
- [x] **DX-13** `PlaybookSpec.requires` — skill refs and capability refs.
  Admission pins each declared skill ref. No scheduler and no per-step
  advisory/enforced flag.
- [x] **DX-14** `AgentSpec.extends` — a pinned upstream agent URN.
  **Decision: locked as item 20.** One `ResourceRef`. Version and digest are
  required. The URN kind is `agent`. Inheritance, not a call.
- [x] **DX-15** Overlay merge semantics for `extends`:
  `append` / `replace` for instructions, `add` / `remove` for skills,
  `grant` / `revoke` for capabilities. Write the conflict rules down; they are
  the hard part.
  **Decision: locked as item 20.** `merge_agent_chain` applies those verbs.
  Chains stop at 8 hops. A child that revokes a capability a remaining skill
  requires fails. Two pins of the same skill URN fail unless the child
  removed the earlier pin first.
- [x] **DX-16** Make skill entrypoints manifest-relative. Today the jira
  manifest hardcodes `/mnt/drive9/skills/jira-integration/execute.js`, which
  bakes in both the mount root and the directory name. A namespaced,
  installable package cannot assume either.
  Entrypoints are paths relative to the directory that contains
  `manifest.json` (`./execute.js`). Host digest calculation and guest
  execution share `resolve_manifest_relative_entrypoint`. A skill whose
  install directory was renamed still resolves; an absolute mount path does
  not.
- [x] **DX-17** Move deployment config out of `AgentSpec` into an
  environment-scoped profile: models, context policy, reasoning policy, vCPU,
  memory, network mode. An agent should describe purpose, not VM sizing. This
  is also what makes an agent portable between organizations.
  **Decision: locked as item 17.** Fields that leave: `preferred_models`,
  `model_requirements.max_cost_per_million_tokens`, `context_policy`,
  `reasoning_policy`, and `resource_requirements`
  (`vcpu_count`, `memory_mib`, `storage_gib`, `network_mode`).
  The capability floor stays: `min_context_tokens`, `requires_tool_calling`,
  `requires_structured_output`, `required_modalities`. Not implemented in
  this batch.
- [x] **DX-18** Decide: `hitlPolicy` and `telemetryPolicy` stay separate refs,
  or collapse into the policy set as distinct Cedar actions. HITL already
  compiles to Cedar via a generated overlay, so the collapse is mostly done.
  **Decision: locked as item 18.** HITL stays structured `monitoredTools`
  plus the existing `ExecuteTool` overlay. It does not become its own Cedar
  action, and `policy_ref` on that message is not a second policy document.
  `telemetryPolicy` stays a separate ref (`pii_redaction_enabled`,
  `audit_ledger_mode`). Not implemented in this batch.
- [x] **DX-19** **Define the canonical agent package layout.** One documented
  file tree that `vam init` scaffolds, `vam validate` checks, and the docs
  teach. Proposed:

  ```text
  my-agent/
    vam.json            identity, deps by range, capability grants   [commit]
    vam.lock            resolved versions + digests + capabilities    [commit, generated]
    agent.json          role, instructions, skill/playbook refs       [commit]
    policy.cedar        this agent's own rules (optional)             [commit]
    playbooks/*.json    procedures owned by this agent (optional)     [commit]
    skills/<id>/        only skills authored *in this repo*           [commit]
    .vam/               resolved cache, materialized payloads         [gitignore]
  ```

  Rule: a consumed skill is a **reference**, never a directory. `skills/` only
  holds skills this repo authors. That is what stops third-party code entering
  the developer's Git history.
  **Decision: locked as item 19.** Taught in
  `docs/src/content/docs/authoring/agent-package.md`. `vam init` scaffolding
  stays Phase 2 (DX-50). Not scaffolded in this batch.
- [x] **DX-19A** **Lock the instruction layering.** Three layers, one owner
  each, no package may cross them:

  | Layer | File | Owner | Writable by a package? |
  |---|---|---|---|
  | Platform rules | `workspace/.vam/SYSTEM.md` | VAM | **no** |
  | Agent purpose | `agent.json.systemInstructions` | agent author | yes |
  | Skill usage | `SKILLS.md` `system_extension` | skill author | yes, on `load_skill` |

  **Do not let agent or skill packages write `SYSTEM.md`.** Two reasons, both
  in code today: it is already write-protected from the guest
  (`is_protected_write_path`, `primitives.rs:139-148`), and it is
  **auto-overwritten** whenever `SHIPPED_TEMPLATE_VERSION`
  (`instruction_template.rs:9`, now 9) exceeds the file's frontmatter
  version. Any developer edit silently vanishes on the next platform upgrade.
  If a real need appears, add an explicit `systemInstructions.append` slot
  instead of opening the file.
  Related: **DX-9A** edits the *content* of the platform layer in Phase 0. That
  is consistent with this decision — VAM owns that file, so VAM may change it.
  DX-19A decides ownership; DX-9A exercises it.
  **Decision: locked as item 19a.** The table above is the rule. No package
  may write `SYSTEM.md`.
- [x] **DX-19B** **`Vamfile` is not part of the agent package.** It answers
  "what is in the machine image", which is a different axis from "what does
  the agent do". Keep it out of the standard layout; it is the Phase 9 Tier 2
  escape hatch. Conflating the two would make every agent author a build
  engineer and would spend the cold-start advantage.
  **Decision: locked as item 19b.**
- [x] **DX-16A** **Kill the `SKILLS.md` ↔ `manifest.json` duplication.** Today
  the `SKILLS.md` YAML frontmatter restates the entire manifest — tool names,
  `input_schema`, `exec`, `required_secrets`, `side_effect`, `timeout` — and
  nothing checks that the two agree. Pick one source of truth. Recommended:
  `manifest.json` is authoritative and machine-owned; `SKILLS.md` keeps only
  prose guidance for the model (its `system_extension` body, which is the part
  `load_skill` actually injects). Generate or validate the frontmatter from the
  manifest at publish.
  `manifest.json` is the machine contract. Example `SKILLS.md` files keep
  identity frontmatter plus the prose body. Publish rejects a frontmatter
  `tools` list whose names, schemas, exec, secrets, side effect, or timeout
  disagree with the manifest.

---

### Phase 2 — Run and CLI ergonomics

**Goal:** Running an agent takes one line and no file. The CLI is pleasant for
humans, parseable by agents, and scriptable in CI.
**Depends on:** Phase 0 (can start alongside Phase 1)
**Exit:** `vam run <agent> -p "..."` works with no `run.json` anywhere;
`--json` on every command; release verbs exist for all four artifact kinds.

**Run redesign**

- [x] **DX-20** `vam run <agent-urn-or-name> --prompt "..."` as the primary
  path. No file required. Resolves the agent from the catalog.
- [x] **DX-21** Short names resolve against installed agents, so
  `vam run jira-assistant -p "..."` works. URNs stay valid.
- [x] **DX-22** `vam run --file run.json` for reproducible and batch runs. The
  file stops being the default and becomes the CI artifact.
- [x] **DX-23** Unhide and fold in `run continue`:
  `vam run --workspace <ws> --thread <t> -p "..."`. It being hidden is evidence
  the file-centric model was already being worked around.
- [x] **DX-24** `vam run <agent> --playbook <urn> --input k=v` for playbook
  runs, matching `PlaybookSpec.input_schema`.
- [x] **DX-25** `vam run --dry-run` — resolve refs, evaluate policy, print the
  tools that would be allowed, denied, and HITL-gated. Run nothing. This is the
  single best "understand my agent" command and it needs no new engine work.

**CLI as an agent interface**

- [x] **DX-26** `--json` on every command, with a documented stable schema.
- [x] **DX-27** Stable exit codes. Never signal failure only in prose.
- [x] **DX-28** No interactive prompt when stdin is not a TTY. Every prompt gets
  a flag. No spinners or colour when not a TTY.
- [x] **DX-29** Rewrite errors to be actionable: what was wrong, what was
  expected, and the command that fixes it. An agent cannot infer intent from a
  raw HTTP 422.
- [x] **DX-30** Idempotency keys on every mutating command.

**Close the lifecycle gap**

- [x] **DX-31** `vam agent release`, `vam policy release`,
  `vam playbook release`, `vam playbook ls`. The HTTP routes already exist in
  `api/catalog_release.rs`; only the CLI surface is missing. Thin work, removes
  "how do I ship v2 of my agent?"

---

### Phase 2.5 — MCP as a core capability

**Goal:** A VAM agent can use remote MCP servers — Jira, Linear, Sentry — the
same way it uses any other tool, with per-user credentials and no new trust
model.
**Depends on:** Phase 2 (the CLI verbs land here)
**Exit:** `vam mcp connect linear` then an agent run that creates a Linear
issue, using the running user's own token, with the call visible in the
trajectory and denied correctly by a Cedar policy that forbids it.

**Why this is its own phase and not Phase 10.** Phase 10 framed MCP as
*marketplace supply* — import an ecosystem to fill an empty catalog. That
framing is wrong for the common case. Remote MCP servers are the ones people
actually want, they need no process hosting, no broker and no sandboxing, and
they are the difference between a demo agent and a useful one. This is a core
platform ability, so it sits early. What genuinely is hard — local stdio
servers and OAuth vendors — stays in Phase 10.

**Why it is cheap.** Every piece already exists; this is assembly, not
invention. `host_tools_http.rs` already resolves a per-caller vault secret,
attaches it as an `Authorization` header, makes the HTTPS call and returns the
result. A remote-MCP client is that same flow with a JSON-RPC body. Full
reasoning: `design/developer-experience.md` §26.

- [x] **DX-130** **Remote MCP client inside `vamd`.** JSON-RPC 2.0 over HTTPS:
  `initialize`, `tools/list`, `tools/call`. Model it on
  `host_tools_http.rs:137-214` — same secret resolution, same egress path, same
  `ssrf_guard`. First-party code, so the in-process concern that rules out
  hosting third-party servers does not apply: we write the client, we do not
  load the vendor's.
- [x] **DX-130A** **Feed the connection URL into the adapter allowlist.** The
  HTTP adapter pins its destination with
  `PinnedUrl::validate_and_pin(url, &allowlist)` where the allowlist comes from
  `ctx.config["allowlist"]` (`host_tools_http.rs:45-68`, `:188-213`). An MCP
  adapter has no skill author to populate that config, so it must be derived
  from the stored connection URL. Get this wrong in the lax direction and a
  user-supplied MCP URL becomes an unrestricted SSRF primitive running as the
  host daemon.
- [x] **DX-131** **Connection registry, per user.** One row per connection:
  `connection_id`, `url`, `secret_binding_id`, `tools_snapshot`,
  `owner_user_id`. CLI: `vam mcp connect|list|disconnect|refresh`.
- [x] **DX-132** **Bind the credential using the vault that already exists.**
  Key is `(scope, owner_user_id, binding_id)` (`secrets.rs:150-205`), so
  `--scope org` gives the team one shared token and `--scope user` gives each
  person their own, with no new storage. At call time
  `decrypt_secret_for_caller(ctx.caller_user_id, …)` already picks the right
  row — the same agent run by two people uses two different tokens with zero
  extra code.
- [x] **DX-133** **Seed a synthetic skill directory per connection — this is
  how schemas reach the guest.** *Corrected after tracing the tool lifecycle.*
  "Generate a manifest" was too vague and hid the real mechanism. The guest
  receives only tool **names** from the host over vsock
  (`ListAdvertisedTools`, `vam-runtime/src/pep.rs:98-158`); descriptions and
  JSON schemas are read from `manifest.json` **on the guest filesystem**
  (`vam-runtime/src/skills.rs:224-306`). A tool that exists only in the host
  database is therefore invisible to the model.
  `hsp_store.rs:366` `seed_hsp_skills` already solves exactly this: it writes
  `materialized/skills/hosted-<broker>/manifest.json` with
  `{name, description, input_schema_json, side_effect, host_exec.adapter_id}`.
  Mirror it as `seed_mcp_skills` writing `mcp-<connection>/`. Do not invent a
  second mechanism.
  This also explains where `adapter_id` lives: **not** in `ToolCatalogEntry`,
  which has only `tool_name`, `command`, `entrypoint`, `digest`, `side_effect`
  (`session.rs:670-676`). It is parsed back off disk at call time by
  `parse_host_exec_from_manifest` (`host_tools.rs:307-332`). The manifest file
  is the source of truth, not the database.
  **The guest must not be able to tell an MCP tool from a normal skill tool** —
  no new proto, no new guest code, and Cedar, HITL and telemetry all apply
  unchanged.
- [x] **DX-133A** **Scope seeding to the session owner.** `seed_hsp_skills`
  lists *all* enabled brokers because HSP is node-wide. MCP connections are
  **per user**, so copying that loop verbatim would materialize one user's Jira
  tools into another user's session — a cross-tenant leak. Filter by
  `session_owner_user_id` (already available at `manager.rs:712`, before the
  catalog seed at `manager.rs:809-834`) and include org-scope connections.
- [x] **DX-133B** **Decide when a new connection takes effect.** The tool
  catalog is seeded **once**, during session/VM setup
  (`manager.rs:809-834`). So `vam mcp connect` during a live session changes
  nothing until the session restarts. **Decision: say so, do not re-seed a
  live session.** Re-advertising would need a new guest path, which locked
  decision 20 forbids. `vam mcp connect`, `refresh`, and `disconnect` print
  that a restart is required (and `--json` sets `restartRequired`).
- [x] **DX-133C** **Write a `SKILLS.md` consistent with the generated
  manifest.** DX-16A (Phase 1) made `SKILLS.md` frontmatter authoritative
  against the manifest for tool names, required secrets and `side_effect`
  (`skill_frontmatter.rs:90-118`). Synthetic seeding bypasses the upload
  admission path, so confirm whether the guest-side loader also requires
  `SKILLS.md`; if it does, generate one and keep it in agreement or the skill
  fails to load.
- [x] **DX-134** **Namespace tool names — but a dot is illegal.**
  *Corrected: the original `jira.create_issue` spelling would have failed
  validation.* `is_tool_name` (`vam-spec/src/validation.rs:1975-1984`) accepts
  only `[a-z0-9_]`, must start with a lowercase letter or digit, and is capped
  at **64 characters**. Use `jira__create_issue` (double underscore as the
  namespace separator) or an equivalent legal scheme, and pick it deliberately
  because it becomes the Cedar resource id and is hard to change later.
  Three constraints to enforce, all of which real MCP servers will hit:
  - the 64-char cap — namespace prefix plus a verbose vendor tool name can
    exceed it; truncating silently would alias two tools onto one name, so
    fail loudly instead;
  - `reserved_builtin_tool_names()` (`validation.rs:1987`) — an MCP tool must
    never shadow `read_file` and friends;
  - session uniqueness, enforced by the `(session_id, tool_name)` primary key
    (`session.rs:941-944`), which would otherwise surface as an opaque
    `INSERT OR REPLACE` overwrite rather than an error.
- [x] **DX-135** **Pin the tool snapshot and detect drift — tool poisoning
  defence.** A remote server can change its advertised tools between calls.
  Snapshot at connect, re-compare on refresh, require explicit re-approval if
  the set grew. `manifest_is_subset` in `hsp_registry.rs` already implements
  exactly this comparison; lift the logic, do not rewrite it.
  Shipped rule: identical is a no-op, a strict subset is stored, and an added
  tool or a changed description or schema is not applied until
  `vam mcp refresh --approve-drift`. Comparison is `relate_approved_set`,
  which `manifest_is_subset` also calls.
- [x] **DX-136** Map MCP tools into the VAM capability model — qualified Cedar
  namespace, structured capability list. *(Moved here from Phase 10 DX-111; it
  is a prerequisite for remote MCP, not a marketplace concern.)* This is the
  real work and it is what makes the claim true rather than marketing.
- [x] **DX-137** Mark capabilities **declared** vs **inferred** in CLI and UI,
  defaulting inferred ones to prompt, never auto-grant. *(Was DX-113.)* An MCP
  `tools/list` description is vendor-supplied prose, not a capability
  declaration — treating it as one would overclaim exactly the credibility
  this is meant to build.
- [x] **DX-137A** **Choose the default `side_effect` for an MCP tool, and
  default it to `true`.** `side_effect` is the field the host feeds to Cedar as
  `context.side_effect` (`security.rs:440-455`), and HITL is reached by
  re-evaluating a denied call with `hitl_approved=true`
  (`security.rs:704-749`). So `side_effect` is the lever that decides whether a
  tool can be auto-run or must be approved. MCP has no trustworthy signal for
  this: tool annotations are optional and vendor-supplied. Defaulting to
  `false` would let a vendor's `delete_project` run unattended. Default to
  `true` and let an operator lower it per tool.
- [x] **DX-137B** **Treat the tool description as untrusted input, because it
  reaches the system prompt.** MCP descriptions are rendered into the guest's
  tools inventory and sent to the model every iteration
  (`vam-runtime/src/instruction.rs:60-68`, `agent.rs:241-246`). That makes a
  vendor-controlled description a direct prompt-injection channel into the
  system prompt. DX-135 catches a description that *changes*; nothing catches
  one that is hostile on day one. At minimum bound the length, strip control
  characters, and show the description at approval time so a human sees what
  the model will see.
- [x] **DX-138** Document the whole path in `docs/runtime/mcp.md`, including
  the tier table: remote (core, here), local stdio (Phase 10), OAuth
  (Phase 10). Today that file describes only a guest-side STDIO host.
- [x] **DX-139** Security floor: MCP connection credentials never reach guest
  egress or a broker. Found by code reading (2026-11-04): `mcp_registry`
  wrote `{id}/oauth/*`, `{id}/auth-header` and `{id}/header/*` as `kind=egress`
  rows pinned to the MCP or token host, so `session_egress_bindings_for_caller`
  gave each a `vamsec_` placeholder and put the host on the guest allowlist.
  Guest code could send `vamsec_<id>_oauth_access_token` to the MCP host, get
  the real bearer injected and call any tool, past Cedar, HITL and the pinned
  list; this broke locked decision 20. Fix: vault kind `connection`
  (reserved: not writable through the secrets API, not rewritable to another
  kind, reclassified from `egress` at start), excluded from every session
  egress binding; an HSP broker is never given a `connection` row and is given
  other rows only when their pinned hosts lie inside its approved
  `egressDomains` (`decrypt_secret_for_broker`). Scenarios
  `guest-cannot-use-an-mcp-token-placeholder` and
  `broker-cannot-be-given-an-mcp-credential`; Rust proof in
  `tests/mcp_credentials_stay_host_side.rs`.
  Decide: NOW (security floor, proof 3 by `phase-playbook.md` §11.1;
  cost 1).

**Team checklist — DX-139 (MCP credentials stay host-side; microVM check)**

Proven so far on the process-mode guest only. A process guest is a host process
with no VM, no vsock and no TLS interception, so this checklist is the first
look at the same behaviour in a real microVM. Run it on the Mac, in Lima.

Setup (once): from the checkout, `./scripts/build-rootfs-lima.sh`, then
`./scripts/start-vamd-lima.sh`. The vamd must be a build of the DX-139 PR (or
`main` after it merges). Sign in with the node key. Use any agent that can run
shell commands (`run`) and a real model.

1. Register an OAuth connection that needs no live server:
   `curl -sk -X PUT "$HOST/v1/mcp/connections" -H "Authorization: Bearer $KEY"
   -H "Idempotency-Key: dx139-a" -H "Content-Type: application/json" -d
   '{"connectionId":"dx139","scope":"org","url":"https://mcp.dx139.example/mcp",
   "authMode":"oauth","oauth":{"resource":"https://mcp.dx139.example/mcp",
   "authorizationServer":"https://auth.dx139.example","tokenEndpoint":
   "https://auth.dx139.example/token","clientId":"c","registrationMethod":"dcr"}}'`,
   then `curl -sk -X PUT "$HOST/v1/mcp/connections/dx139/oauth/tokens?scope=org"
   ... -H "Idempotency-Key: dx139-b" -d '{"accessToken":"dx139-access-value",
   "refreshToken":"dx139-refresh-value"}'`. **See:** both answer 200.
2. `vam secrets list`. **See:** `dx139/oauth/access-token` and
   `dx139/oauth/refresh-token` with kind `KIND_CONNECTION` (not egress).
3. Start a run and ask the agent: "Run this exact command and show me the
   output: `curl -s -i -x $HTTP_PROXY -H 'Authorization:
   vamsec_dx139_oauth_access_token' http://mcp.dx139.example/mcp`". **See:** the
   output says it could not resolve `mcp.dx139.example`. It does not say a
   secret placeholder was refused, and `dx139-access-value` appears nowhere in
   the thread.
4. Read the vamd log (the terminal where `start-vamd-lima.sh` runs) for the run's
   session. **See:** `Egress Broker session ...
   bindings=0 allowed_domains=[]` (no MCP host in the guest allowlist).
5. Upgrade check, on a node that already had an MCP connection before this
   change (skip on a fresh node): restart vamd on the new build and run
   `vam secrets list`. **See:** that connection's rows read `KIND_CONNECTION`.
6. Clean up: `curl -sk -X DELETE "$HOST/v1/mcp/connections/dx139?scope=org" -H
   "Authorization: Bearer $KEY" -H "Idempotency-Key: dx139-c"`.

Report: "DX-139 microVM pass", or the step number with the output or error text.

---

### Phase 3 — Project manifest and lockfile

**Goal:** A unit of packaging exists. Installing a skill no longer churns the
developer's repo.
**Depends on:** Phase 1
**Exit:** a project declares dependencies by range, resolves to a committed
lockfile, and contains no third-party payloads.

- [ ] **DX-40** `vam.json` — **the project file for VAM artifacts only.**
  Scope is deliberately narrow now that language packages are handled by each
  ecosystem's native manifest (DX-101). Two separate dependency layers, two
  separate tools:

  | Layer | Example | Managed by | Lockfile |
  |---|---|---|---|
  | VAM artifacts | a skill, a playbook, a policy | **`vam`** | `vam.lock` |
  | Language packages | `pandas`, `axios` | npm / pip / go | that ecosystem's own lock |

  `vam` is a package manager for skills, playbooks, policies and agents. It is
  **not** a package manager for libraries and must never grow into one.

  Contents:

  ```jsonc
  {
    "name": "jira-assistant",
    "namespace": "acme",           // publishes as urn:vam:agent:<registry>/acme/jira-assistant
    "version": "1.2.0",            // the one place this project's version lives
    "kind": "agent",               // agent | skill | playbook

    "agent": "./agent.json",       // where the spec files are
    "policies": ["./policy.cedar"],
    "playbooks": ["./playbooks/sprint-cleanup.json"],

    "dependencies": {              // VAM artifacts only, by range
      "skills":    { "acme/jira": "^3.0", "acme/slack": "^2.1" },
      "playbooks": { "acme/standup": "^1"  },
      "policies":  { "vam/base-readonly-fs": "^1" }
    },

    "grants": [                    // what installed skills are allowed to do
      "tool:acme/jira:search", "tool:acme/jira:transition",
      "egress:*.atlassian.net", "secret:jira/auth-header"
    ]
  }
  ```

  `grants` lives here, not in `agent.json`, because it is the install-time
  consent record and must be diffable against `vam.lock` in a pull request
  (DX-42, DX-75).
- [ ] **DX-40A** **Keep `vam.json` and `agent.json` separate.** `agent.json`
  is an `AgentSpec` proto message that gets published to the catalog — it is
  the artifact. `vam.json` is local project metadata that is never published:
  where the files are, which ranges you want, who publishes it. It also has to
  work for repos that have **no** `agent.json` at all, such as a skill repo or
  a playbook repo. One project file per repo, whatever the repo produces.
- [ ] **DX-40B** The registry is `vamd`'s existing catalog; the client is the
  `vam` CLI. No new storage layer — the catalog already has namespaced URNs,
  semver, digests, draft/released/yanked channels and owner scoping.
- [ ] **DX-40C** **One project releases exactly one artifact.** `vam.json` has
  one `kind` and one `version`; that is what `vam release` publishes. Release
  granularity follows **reuse**, not file count:

  | File in the project | Separately installable? | Why |
  |---|---|---|
  | the agent (`kind: agent`) | yes — it *is* the artifact | — |
  | its own `policy.cedar` | **no**, bundled at the project version | an implementation detail of that agent; nobody installs it alone |
  | its own `playbooks/*.json` | **no**, bundled | same |
  | a shared policy or playbook | yes — give it **its own project** | independently useful, so it needs its own version train |
  | an installed skill | yes — it is a `dependency`, by URN | someone else's release train |

  Rule of thumb: **local files ride along at the project version; referenced
  URNs have their own versions.** If you want something reusable, move it to
  its own repo/project and depend on it. Same model as npm — internal modules
  are not separately versioned, dependencies are.
- [ ] **DX-40D** Consequence for the examples: `examples/jira-assistant`
  currently authors an agent *and* a skill in one directory. Under DX-40C that
  is two projects. Either split it, or keep it as the motivating case for
  monorepo support — but do not let one `vam.json` publish two kinds.
- [ ] **DX-41** `vam.lock` — exact `urn@version+digest` for everything
  resolved. The orchestrator already computes this as
  `RunSpec.resolved_refs`; it just never hands it back in committable form.
- [ ] **DX-42** Record **resolved capabilities** in the lockfile, not just
  versions. This is what makes a dependency bump reviewable in a PR.
- [ ] **DX-43** Materialize payloads into the Drive9 skills mount at session
  start, gitignored. Third-party code never enters the repo.
- [ ] **DX-44** Version range resolution (`^2.1`) against the catalog.
- [ ] **DX-45** `vam.lock` is the input to run resolution, so a run is
  reproducible from the repo alone.
- [ ] **DX-46** **Plan to retire the `draft` channel, replacing it with real
  prerelease versions — but not yet.** Draft exists because there was no local
  testing: you had to push to the catalog to try anything. Once `vam dev`
  (DX-53) covers the inner loop, the remaining reasons to publish something
  unfinished are sharing with a teammate and staging — and semver prereleases
  do that better:

  | Need | Today | Proposed |
  |---|---|---|
  | test my own change | publish `1.0.0-draft` | `vam dev`, nothing published |
  | share a WIP | `1.0.0-draft` (mutable) | `1.3.0-rc.1` (immutable, ordered) |
  | ship it | release `1.0.0` | release `1.3.0` |

  Why prereleases are better: `draft` is a **mutable row** at a fixed slot, so
  you cannot have a draft of `2.1.0` while `2.0.0` is released — the draft slot
  is literally `1.0.0-draft`. A prerelease is immutable and orderable, so it
  behaves like every other version. The CLI's current rejection of prerelease
  strings on release would flip to allowing them.

  **Blocked, do not start:** `.cursor/workstreams/isolation-verify-closeout.md`
  is active at phase 4 and its locked decision 6 is *"Owners/publishers may pin
  draft (`1.0.0-draft`) or a released version"*, with shipped code
  depending on it. Revisit only after that workstream closes, and treat it as a
  migration with a deprecation window, not a removal.

  **Superseded 2026-10-10 by the product owner**
  (`.cursor/workstreams/agent-experience.md`, decisions 2–3): `draft` is
  **kept**, not retired. It becomes the owner's private working copy with no
  version, one per artifact, no longer stored at a fixed `1.0.0-draft` slot.
  Versions exist only on release. Semver prereleases are deferred. The
  implementation is agent-experience Phase 1. Do not build this item.

---

### Phase 4 — Local loop

**Goal:** Edit a skill, see it work, in seconds. No microVM boot.
**Depends on:** Phases 1, 3
**Exit:** a developer scaffolds a skill and iterates without `vamd` or Lima.

- [ ] **DX-50** `vam init agent|skill|playbook` — scaffold from templates.
  Kills copy-paste drift from examples, which is how the Phase 0 bugs spread.
- [ ] **DX-51** `vam validate` — run `vam-spec` validation and Cedar strict
  validation **locally**. Today both only run server-side at publish time, so
  the inner loop is edit → upload → read an HTTP error.
- [ ] **DX-52** Cross-file checks in `validate`, which nothing does today:
  every tool named in `policy.cedar` exists in a referenced skill manifest;
  every `required_secrets` entry has a binding; every `skillRef` resolves;
  every playbook `requires` is satisfied.
- [ ] **DX-53** **`vam dev`** — the most important unbuilt command here.
  **Three fidelity tiers, not one.** Isolation exists to protect the host from
  untrusted agent code; in dev the developer *wrote* that code, so isolation is
  genuinely optional and should not be paid for on every edit.

  | Tier | How | Speed | Catches | Use |
  |---|---|---|---|---|
  | **1 `vam dev`** (default) | no VM, no container: tool runs as a host process, Cedar in-process, host tools stubbed | ms | tool logic, manifest/schema errors, policy decisions | every edit |
  | **2 `vam dev --container`** | Docker, using the **same rootfs contents** as the golden base, mounted at the same paths | seconds | missing binaries, wrong paths, runtime version drift | before commit |
  | **3 `vam run`** | real `vamd` + Firecracker | ~minute | everything, incl. isolation | before publish |

- [ ] **DX-53A** Tier 1 harness: do **not** run `vam-runtime`. Run a host-side
  executor that speaks the same tool contract, so there is no VM and no
  container in the default loop.
- [ ] **DX-53B** Tier 2 container: run the **real `vam-runtime`** in Docker,
  talking to `vamd` over a Unix socket instead of vsock. This is already
  largely possible — `GenericStream` in `crates/vam-vsock/src/guest.rs:10`
  already has both `Vsock(VsockStream)` and `Unix(UnixStream)` variants, so the
  transport swap exists. Keeping the real runtime in the path is what makes
  Tier 2 trustworthy rather than an approximation.
- [ ] **DX-53C** **Build the golden base as an OCI image, then convert to
  `ext4`.** Docker dev and Firecracker prod then share one filesystem
  definition, so Tier 2 fidelity is high by construction instead of by
  maintenance. `docs/build/oci.md` already designs OCI packaging of ext4
  artifacts; this is the same idea run in the other direction, and it is the
  single highest-leverage item for making container-based dev honest.
- [ ] **DX-53D** Mount paths must match production exactly in Tier 2 —
  `/mnt/drive9/skills` read-only, `/mnt/drive9/workspace` writable. If dev uses
  host paths, every path-jail and filesystem policy test is meaningless.
- [ ] **DX-53E** macOS matters here: Docker Desktop works natively while
  Firecracker needs Lima. Tier 2 removes Lima from the inner loop for most
  developers, which is a large part of the ten-step setup problem.
- [ ] **DX-54** `vam doctor` — check KVM/Lima, rootfs, kernel, daemon
  reachability, auth, secrets, catalog. One command instead of four docs.
- [ ] **DX-55** Publish JSON Schema for all VAM files so editors autocomplete
  and lint them.
- [ ] **DX-56** `vam dev --watch` for a live reload loop.

---

### Phase 5 — Layered policy and capability grants

> **2026-10-11:** largely superseded by `design/agent-experience.md` Part IV
> (permissions as data, E1 Phase 5). Each item below carries its new home.

**Goal:** Skill authors say what they need; installers grant it; the org bounds
it. Security lives in one place, as originally intended.
**Depends on:** Phases 1, 3
**Exit:** five policy layers compose correctly; a skill cannot grant itself
anything beyond its own tools.

- [ ] **DX-60** `AgentSpec.policyRefs[]` replacing the single `policyRef`.
  Concatenate into one Cedar `PolicySet`. Cedar's forbid-overrides-permit gives
  sound composition with no new evaluation semantics.
  *(Superseded 2026-10-11 by `agent-experience` Part IV: an agent carries `permissions` (rules as data) plus optional `advanced` policy attachments, not `policyRefs[]`. E1 Phase 5.)*
- [ ] **DX-6A** **Carried from Phase 0.** DX-6 decided that policy
  `ResourceRef.digest` should be *enforced* and the field kept. The decision
  landed; the code did not. `security.rs` still extracts only the URN and drops
  version and digest, so a policy can be swapped under a pinned reference.
  Implement verification here, where policy resolution is already being
  reworked for `policyRefs[]`. Exit test: resolution fails closed when the
  fetched policy's digest does not match a pinned one.
  *(Moved 2026-10-11 to `agent-experience` Phase 5, item 5.3: pin and digest enforced for `advanced` attachments and the legacy compatibility path.)*
- [ ] **DX-61** Five layers, with rules about what each may emit:
  platform baseline (`forbid` only), org baseline (`forbid` only), skill
  request (`permit` only, own tools only), agent grant (selects which requests
  activate), run narrowing (`forbid` only).
  *(Superseded 2026-10-11 by `agent-experience` Part IV: scopes are platform floor, org, source, agent and run, with "strictest wins" across scopes. Skills request no permits.)*
- [ ] **DX-62** **Scoped-permit validation at publish time.** Every `permit` in
  a skill fragment must reference only tools that skill exports. This is the
  linchpin — without it, a published skill containing
  `permit(principal, action, resource)` owns the machine. Static, checkable,
  cheap. Build it before anything third-party is installable.
  *(Superseded 2026-10-11 by `agent-experience` Part IV: skills ship no Cedar, so no scoped-permit validator is needed. `SkillManifest.policy` is deprecated.)*
- [ ] **DX-63** Structured capability list ⊇ Cedar fragment, validated at
  publish. Humans read the structured list; the PDP reads Cedar.
  *(Superseded 2026-10-11 by `agent-experience` Part IV: moot without skill Cedar. The structured capability list stays for consent.)*
- [ ] **DX-64** Org baseline as a real artifact with its own URN, owned by the
  security team, applied to every agent in the org.
  *(Moved 2026-10-11 to E3 as org guardrails, `agent-experience` design §IV.10.)*
- [ ] **DX-65** `vam policy explain <tool> [--context k=v]` — print the
  decision and which policy matched. `DecisionOutcome` already carries
  `matching_policies`; expose it.
  *(Moved 2026-10-11 to `agent-experience` Phase 5, item 5.8, with `--host`.)*
- [ ] **DX-66** `vam policy test` — table-driven fixtures. The tests in
  `crates/vam-security/tests/pdp_tests.rs` are already this shape, just not
  available to users.
  *(Still open; listed as later in `agent-experience` Phase 5.)*
- [ ] **DX-67** Cedar templates for the repeated workspace read/write/size
  block, copy-pasted verbatim across all five example policies.
  *(Still open, advanced attachments only; listed as later in `agent-experience` Phase 5.)*
- [ ] **DX-68** Qualify Cedar tool resources with namespace —
  `Tool::"acme/slack:post"`. Today Cedar sees the unqualified `echo.ping` while
  the route is `hosted:echo-ts:echo.ping`. At a lakh of skills, names like
  `search` and `query` collide, and a policy permitting `Tool::"search"`
  silently authorizes a tool from a skill nobody trusted. Cheap now, breaking
  later.
  *(Still needed. `agent-experience` Phase 5 adds source-qualified targets as `Tool` attributes; renaming the Cedar resource id stays here.)*

---

### Phase 6 — Install and customize

**Goal:** The stage most users will live in permanently, and it does not exist
today. There is currently a cliff: run a bundled example, or build from
scratch.
**Depends on:** Phases 3, 5
**Exit:** install an agent, change it, and still track upstream.

- [ ] **DX-70** `vam install <urn>` — agent, skill, or playbook. Resolves
  transitively, writes the lockfile, materializes payloads.
- [ ] **DX-71** Capability consent on install — show the structured capability
  list and require an explicit grant.
- [ ] **DX-72** `vam fork <urn>` — create an overlay via `extends`, not a copy.
  The repo holds only the delta.
- [ ] **DX-73** `vam add skill|playbook <urn>` and `vam remove`.
- [ ] **DX-74** `vam upgrade` — re-resolve, show a **capability diff**, report
  overlay conflicts.
- [ ] **DX-75** CI check: fail when lockfile capabilities exceed `vam.json`
  grants. A capability expansion cannot merge without a human widening the
  grant. A version-only lockfile cannot show this; a capability set can,
  because Cedar makes capabilities explicit.
- [ ] **DX-76** `vam eject` and `vam adopt` — move a project between
  UI-managed and Git-managed without losing it. Without both, the UI is a trap
  and serious developers avoid it from day one.

---

### Phase 7 — Playbooks as a product

**Goal:** Make the artifact that delivers outcomes first-class. Skills are
ingredients; playbooks are recipes; users buy recipes.
**Depends on:** Phases 1, 6
**Exit:** a playbook installs standalone, pulls its skills with it, and can be
tested.

- [x] **DX-80** **Decide: rigid DAG or soft procedure.** `PlaybookSpec.steps`
  today is a strict DAG with `dependsOn`, `retryPolicy` and `approvalAction` —
  a workflow engine. The stated intent is softer: guidance the agent follows.
  These are different products and conflating them is a trap. Blocks
  everything else in this phase.
  **Decision: dual-layer procedure. Locked as item 16, revised 2026-09-24.**
  Prose on every step; an optional tool binding with mode `bound` /
  `assisted` / `agentic`. No scheduler, no per-step `advisory`/`enforced`.
  Supersedes an earlier prose-only ruling made in a previous session. Unblocks
  DX-12 and DX-13 as procedures with `requires`. Not implemented in this batch.
- [x] **DX-80A** **Reshape `PlaybookStep` for the dual layer.** *Filed under
  Phase 7 but built during Phase 1, which is where it belonged — it is a proto
  change and DX-13 edits the same message. Left here for history.* This is the
  schema change decision 16 requires and it gates DX-81A, DX-81B and DX-12/13.
  Replace the `oneof step_action` with: `instruction` (string, always present),
  `binding` (optional — `tool_name` plus `arguments`), and `mode`
  (`bound` | `assisted` | `agentic`). Keep `id`, `description`, `depends_on`.
  Leave the deferred workflow-engine fields in place and untouched.
  Exit: round-trip test through `json_to_proto`; a step with prose *and* a
  binding survives encode/decode; the Phase 0 examples test still passes.
- [ ] **DX-81** Recommended answer to DX-80: mark steps individually as
  `advisory` or `enforced`, so an approval gate is guaranteed while analysis
  steps stay adaptive.
  **Rejected by DX-80.** Do not add per-step `advisory` / `enforced`. An
  enforced step with an advisory predecessor has no defined join, and the
  approval guarantee is the HITL overlay on tools, not a playbook step.
  **Confirmed on review** — this rejection stands. Enforcement lives on tools.
- [x] **DX-81A** **Literal vs templated arguments.** `instruction_template` already interpolates
  `{{steps.x.output}}`. If a step's bound `arguments` allow the same, a
  "static" argument is not static and cannot be pre-approved. Validate and
  label at publish: literal args get a `canonical_args_digest` computed at
  publish time (reuse the Phase 0 function in `vam-spec`) and are inspectable
  before purchase; templated args go through the runtime PEP like any other
  call. Do not let a playbook claim determinism it does not have.
- [ ] **DX-81B** **Define what the model sees per mode**, or steps get executed twice. `bound`: prose is documentation —
  shown in traces and `vam playbook explain`, never injected. `assisted`:
  injected as argument-filling context only. `agentic`: injected as the
  instruction. Without this a bound step's prose can make the model redo the
  work the bound call already did.
- [ ] **DX-81C** Confirm in the proto comments that the deferred workflow-engine fields are inert:
  `RetryRule`, `JoinPolicy`/`QUORUM`, `required_successes`,
  `fallback_step_id`, `StepEvaluationPolicy`, and `ConditionalAction`'s CEL
  expression. Decision 16 already says not to grow them; nothing reads them
  today. Mark them reserved-for-later so the next reader does not assume a
  scheduler exists. The schema comment already concedes "Alpha permits at most
  one dependency per step."
- [ ] **DX-82** Standalone playbook install — installing a playbook resolves
  and installs its required skills transitively. This is the "install it and
  start using it" experience.
- [ ] **DX-83** `vam init playbook` template and authoring docs. Playbooks are
  currently the artifact nobody mentions despite having full catalog
  machinery.
- [ ] **DX-84** `vam playbook test` — run against a mock workspace with stubbed
  tools, assert the step sequence.
- [ ] **DX-85** Playbook ↔ skill compatibility checking: does the playbook's
  `requires` match the installed skill versions?
- [ ] **DX-86** Marketplace listing treats playbooks as a first-class artifact with
  outcome-based descriptions, not tool lists.

---

### Phase 8 — Deployments and environments

**Goal:** Someone who did not build the agent can run it in production, see
what version is live, and roll it back.
**Depends on:** Phases 2, 3
**Exit:** named environments with independent pins and one-command rollback.

- [ ] **DX-90** A named deployment concept in `vamd`. Today there is a catalog
  (definitions) and sessions/runs (executions), but nothing addressable in
  between. The closest primitive is a workspace-bound agent version, which
  surfaces only as an error message.
- [ ] **DX-91** Environments: `staging` runs 3.1, `prod` runs 3.0.
- [ ] **DX-92** `vam deploy <env>`, `vam promote <from> <to>`,
  `vam rollback <env>`.
- [ ] **DX-93** `vam status <env>` — what version, what capabilities, what
  health.
- [ ] **DX-94** Decide yank semantics: what happens to a **running** session
  pinned to a yanked artifact. The `yanked` channel exists in the schema; the
  behaviour is undefined. Note policy deliberately does not hot-reload into
  in-flight sessions.
- [ ] **DX-95** CI recipes: validate on PR, release on tag, deploy on merge.

---

### Phase 9 — Dependencies and the base image

**Goal:** A skill can bring what it needs. Without this, skills are patches,
not packages.
**Depends on:** Phase 3
**Exit:** a skill declaring `pandas` works on the shared base with no image
build.

- [ ] **DX-100** Stock the golden base properly — Node, Python, Bun, common
  CLIs. Covers roughly 90% of agents with zero build and preserves the cold
  start that justified Firecracker.
- [ ] **DX-101** **Use each ecosystem's native manifest. Do not invent a VAM
  dependency DSL.** *(Revised — supersedes the earlier proposal of a custom
  `dependencies` map.)* A skill ships the lockfile its own ecosystem already
  uses, and VAM detects it and runs the standard installer:

  | Skill ships | VAM runs | Installs into |
  |---|---|---|
  | `package.json` + `package-lock.json` | `npm ci` | `<skill>/node_modules` |
  | `requirements.txt` or `pyproject.toml` + lock | `pip install` / `uv sync` | `<skill>/.venv` |
  | `go.mod` + `go.sum` | `go mod download` | module cache |
  | `pom.xml` / `build.gradle` | `mvn` / `gradle` | local repo |

  Rationale: a custom DSL means reimplementing version resolution for every
  language forever, and skill authors already know their own tooling.
  Per-skill install directories also give collision isolation for free — two
  skills can depend on different versions of the same library with no shared
  namespace, which removes the main argument for a Nix backend (§19.3 of the
  design doc). Revisit Nix only if per-skill duplication becomes a real size
  or cold-start problem.
- [ ] **DX-101A** **Consume the `package.json` that already ships.** Every
  example skill has one, **nothing in the codebase reads it**, its
  `dependencies` are empty, and there is no install step anywhere in the guest.
  That is the concrete reason skills are not yet installable packages. Keep the
  file, make it authoritative for JS skills, and require a committed lockfile
  for reproducibility.
- [ ] **DX-101B** **Split language packages from system packages.** Language
  deps (DX-101) are per-skill and install cleanly. System-level things — a JVM,
  the Python interpreter itself, `libpq`, `imagemagick` — cannot be installed
  per skill and belong to the image. A skill therefore *declares* what it needs
  from the machine and VAM **checks** it at install:

  ```jsonc
  "requiresRuntime": { "java": ">=21", "python": ">=3.11" }
  ```

  This is a precondition check, not an installer — small surface, and it turns
  a mysterious runtime failure into "this skill needs Java 21; your base image
  has none. Add it to your Vamfile or pick the jvm base."
- [ ] **DX-101C** **Agents own their machine; skills must not.** The image is
  the agent author's call — Vamfile, extra runtimes, anything (DX-104). But a
  *marketplace* skill cannot require the installer to edit their image, or
  "install" stops meaning anything. Hence the split: skills bring language
  packages, agents bring the machine, and `requiresRuntime` is the contract
  between them.
- [ ] **DX-105** **No network installs at run time.** Dependencies resolve and
  materialize on the **host**, before boot, into the read-only skills mount.
  A guest that can `npm install` at run time has arbitrary egress, which
  defeats the egress policy and makes runs non-reproducible. This constraint
  should be written into the invariants, not just assumed.
- [ ] **DX-102** Host-side materialization of declared deps into the mount at
  session start. Content-addressed and cached.
- [ ] **DX-103** Spike a resolver backend. Nix is the candidate, and the real
  argument is not reproducibility — it is collision-free composition. With
  lakhs of skills from independent authors, two skills needing incompatible
  versions of the same library is a certainty; content-addressed store paths
  let both coexist. **Developers must never see Nix.** If it leaks into
  authoring, the approach failed.
- [ ] **DX-103A** **Resolve the `ENTRYPOINT` contradiction in the Vamfile
  spec.** `docs/build/vamfile.md` shows a worked example ending in
  `ENTRYPOINT ["/usr/local/bin/my-agent"]`, and `directives.md` documents
  `ENTRYPOINT` as a directive. But in VAM, PID 1 is always `vam-runtime` — it
  mounts pseudo-filesystems, configures networking, and connects to the host
  broker over vsock. A Vamfile that replaces the entrypoint produces a guest
  that never boots. Either drop `ENTRYPOINT` from the language, or redefine it
  as the workload `vam-runtime` supervises, and fix the example. Decide before
  anyone authors a Vamfile against the current spec.

  **Decided 2026-10-10 (`design/agent-experience.md` §III.1–III.2):**
  `ENTRYPOINT` and `CMD` are dropped. The builder always injects
  `vam-runtime` as `/sbin/init`. `SECURITY_POLICY` is renamed `HARDEN`. Docs
  under `docs/build/` are updated in E7 Phase 3.
- [ ] **DX-104** Position Vamfile explicitly as the Tier 2 escape hatch:
  custom kernels, unusual system libraries, compliance base images. Note that
  a custom-image agent is not portable across the marketplace. `vam-build` has
  21 specification documents and zero implementation; most may never need
  building.

  **Amended 2026-10-10 by the product owner:** superseded by E7
  (`design/agent-experience.md` Part III). Vamfile becomes a supported opt-in
  path with an app Machine step. Standard bases (Tier 0) still come first, so
  most agents never build.

---

### Phase 10 — MCP: local servers and OAuth

**Goal:** Cover the MCP tail that Phase 2.5 deliberately left out — servers
that must run as a local process, and vendors that only speak OAuth.
**Depends on:** Phase 2.5, Phases 5, 6
**Exit:** a stdio MCP server runs under an approved, confined broker, and one
OAuth vendor completes a full authorize → refresh → revoke cycle.

**Read this first.** Remote MCP moved to Phase 2.5 as a core capability. What
remains here is genuinely harder and should not be conflated with it: hosting
somebody else's process, and building an OAuth subsystem from zero.

- [ ] **DX-110** Implement the guest-side MCP host already designed in
  `docs/runtime/mcp.md` — STDIO JSON-RPC, lazy start, 300s idle reap.
  **Re-scoped: this unlocks uncredentialed servers only.** `mcp.md:85` already
  says to use guest MCP "when the tool server can run safely inside the VM with
  no host secrets". Almost every server worth importing needs credentials, so
  this tier covers filesystem, time, calculator — not GitHub or Slack. Ship it,
  but do not describe it as MCP support.
- [ ] **DX-110A** **MCP as an HSP broker — the tier that makes MCP useful.**
  *Revised: HSP, not the host tool adapter.* `HostToolAdapter` is an
  **in-process** Rust trait (`host_tools.rs:56-59`) handed a live
  `Option<&SecretsVault>` (`:43-54`). Third-party code there means arbitrary
  code in `vamd`'s address space beside every secret — a non-starter. HSP is
  already out-of-process gRPC over UDS (`hsp.rs:19-24`) and **already has the
  admission machinery MCP needs**: `require_enabled_approval`,
  `manifest_is_subset` (blocks tool poisoning), `approved_secrets` +
  `deny_extra_secret` (`hsp_registry.rs:71-81`), `assert_trust_transport`,
  `assert_identity_satisfied`. Reuse it; do not rebuild it.
  Shape: `vamd` ⟶ UDS ⟶ `vam-mcp-bridge` (first-party, one per server) ⟶
  STDIO ⟶ the third-party MCP server. The bridge holds the credential and
  passes it to the child's environment. The MCP server never learns VAM
  exists, and the guest still never holds a secret.
  Rationale and threat model: `design/developer-experience.md` §26.
- [ ] **DX-110D** **Broker process confinement — currently nothing.** `vamd`
  never spawns a broker; it only dials a UDS (`hsp.rs:281-300`), so user,
  namespace, seccomp, cgroup and filesystem confinement are entirely the
  operator's job and no code in the repo does any of it. Acceptable while HSP
  is operator-installed. **Becomes a blocker the moment MCP import is
  one-click**, because the person clicking is no longer the person who vetted
  the binary. Decide the confinement story before, not after.
- [ ] **DX-110E** **Keep `vam import mcp` two-step.** DX-112 reads like
  one-click install. One-click install of a host process holding a real
  credential is the failure mode: the broker keeps the token in memory for its
  lifetime, and `npx @vendor/server` runs arbitrary code at install time before
  any VAM policy applies. Generate the manifest and wrapper, then require
  explicit operator approval — `hsp_store.rs` and `hsp_registry.rs` already
  implement approval. Applies to **local** servers only; remote connections in
  Phase 2.5 install no process and need no operator step beyond binding a
  secret.
- [ ] **DX-110B** **Why placeholders cannot carry MCP auth — record this so it
  is not re-attempted.** The `vamsec_<id>_<hex>` placeholder
  (`vam-network/src/secrets.rs:46-58`) is substituted by the egress proxy and
  works because *the skill* writes it verbatim into a request. A third-party
  MCP server fails this three ways: it may validate the token's format locally
  and reject `vamsec_…` before any network call; it may *derive* rather than
  send the secret (OAuth refresh, HMAC, JWT), and a placeholder cannot be
  derived from; or it may use OAuth entirely. Document in `docs/runtime/mcp.md`.
- [ ] **DX-110C** **OAuth — open a separate design doc, do not build it here.**
  **There is no OAuth anywhere in the codebase** (zero matches across all
  crates). GitHub, Google, Slack, Notion are all OAuth, so the top MCP servers
  are unreachable without it. This is a subsystem, not a feature:
  authorization-code flow, redirect handling, refresh-token storage,
  expiry-driven refresh, per-user and per-org scoping, revocation. Extend the
  existing `org`/`user`/`either` scope model in
  `vam-orchestrator/src/secret_requirements.rs` rather than duplicating it.
  Leaving it inside Phase 10 hides a multi-week security-critical build inside
  an item that reads like an import script.
- [ ] **DX-112** `vam import mcp <server>` — wrap a **local/stdio** MCP server
  as an installable VAM skill. Remote servers use `vam mcp connect` (DX-131)
  and do not go through import at all.
- [ ] **DX-114** Import the top MCP servers as seed catalog content.

---

### Phase 11 — Marketplace

**Goal:** Third parties publish; users install safely.
**Depends on:** Phases 5, 6, 7, 10
**Exit:** an external developer claims a namespace, publishes a signed skill,
and someone else installs it unmodified.

- [ ] **DX-120** Namespace ownership with DNS-style verification. Note
  `catalog_tier.rs` currently identifies platform artifacts with
  `urn.contains(":examples.local/")` — a substring match on a hardcoded string
  that will not survive third-party namespaces.
- [ ] **DX-121** Publisher signing. `vam-security` has Ed25519 but it signs
  approval claims, not artifacts. Must exist **before** anything untrusted is
  installable — this is the don't-retrofit-it item.
- [ ] **DX-122** Signature verification at install.
- [ ] **DX-123** Trust tiers — verified publisher, community, unverified —
  driving default grant behaviour.
- [ ] **DX-124** Search and discovery, including an installed-skill index so
  the model can choose what to load without loading everything. Shares a design
  with `docs/src/content/docs/authoring/skill-manifest.md`, which is solving the same shape of
  problem for large skill docs.
- [ ] **DX-125** Agent-proposes / human-grants install flow. Reuse the existing
  HITL approval machinery.
- [ ] **DX-126** Publisher dashboard: installs, versions, capability-change
  review.

---

## Explicitly later

Work that must not be pulled into this tracker.

- HSP brokers as marketplace packages, broker URNs/versions/digests,
  sandboxed third-party brokers over vsock. HSP stays operator-installed.
- `vam-build` implementation beyond positioning it as an escape hatch.
- Tauri UI implementation. API parity only.
- Monorepo/workspace support in `vam.json`.
- **Agent-to-agent calls / sub-agent delegation.** `extends` is *inheritance*
  (an agent derives from another agent's definition). It is not *invocation*
  (an agent calling another agent as a tool). Delegation is a real and common
  need, but it raises its own questions — whose capabilities apply to the
  callee, how budgets and HITL propagate, how loops are prevented — and none of
  that is designed. Do not conflate it with `extends`. Open a separate design
  doc if it becomes a requirement.
- Rate limiting and the `App`/`Subject` identity model — tracked separately in
  `design/vam-apps-spaces.md` (supersedes
  an earlier App/Subject draft). Its decision C1 proposes that the App *is* the
  DX-90 deployment and the home of the DX-17 profile. Both add principals to
  the same Cedar evaluation and must be reconciled before Phase 5 ships.

## How to deep dive a phase

Same loop each time, so phases stay comparable:

1. **Read the slice.** Re-read the matching design-doc section, then read the
   current code for that area. Write down what already exists — Phase 0 found
   that release APIs existed for all four artifact kinds while only skills had
   CLI verbs.
2. **Write the contract first.** Proto or CLI signature, plus the error cases.
   Most of these phases are contract changes; the code is small once the
   contract is settled.
3. **Find the locked decision.** Every phase has one question that blocks the
   rest — DX-80 for playbooks, DX-62 for policy, DX-17 for agent shape. Answer
   it before splitting issues, and add it to the Locked decisions list.
4. **Split by contract surface**, not by crate. One issue per field, command,
   or validation rule. If an issue touches three crates that is fine; if it
   introduces two new concepts, split it.
5. **Name the exit test.** Each issue states the test that proves it. No test,
   not done.
6. **Check the journey.** Ask which developer stage the phase moves, and how
   many steps it removes. If the answer is none, it is not a DX phase.

## How to hand work to an agent

**Default to one or two items per prompt. Use judgement — some items are big
enough to stand alone, some are small enough to travel together.** A phase is a
planning unit, not a work order. Ten items in one prompt produces a large diff
that is hard to review and hard to roll back.

Group a batch by **the files it touches**, not by item number. Items that edit
the same files belong in one batch so they do not conflict; items that touch
different files can run in parallel.

Good batch shapes:

- a failing test plus the fix that makes it pass,
- one defect with its regression test,
- several small decisions written up together, no code.

Split further when an item is security-critical, changes a contract, or has a
decision embedded in it. Combine when items are trivial and touch the same
files — a docs-only fix does not need its own prompt.

State in every prompt: the item IDs, the batch's exit test, the verification
commands, and an explicit *stop here* so the agent does not wander into the
next batch.

## Progress log

| Date | Phase | What landed |
|---|---|---|
| 2026-09-24 | — | Tracker opened from `design/developer-experience.md` |
| 2026-09-24 | 0 | DX-1 and DX-2: `examples/**/*.json` decodes through `json_to_proto` in CI; `sqlite-assistant/run.json` and `chaos-assistant/run.json` rewritten to the `CreateRunRequest` shape. |
| 2026-09-24 | 0 | DX-8: guest tool `PATH` includes `/usr/local/bin`. A test checks that path against every executable directory `scripts/build-rootfs.sh` installs. |
| 2026-09-24 | 0 | DX-9: one `canonical_args_digest` in `vam-spec`, used by host and guest. Nested objects, arrays, null, and empty structs are canonicalized. Non-finite numbers and missing kinds fail closed. |
| 2026-09-24 | 0 | DX-3 and DX-4: example proto JSON uses camelCase field names. Cedar principals use fully-qualified agent URNs; the duplicated short-form blocks in `jira-assistant/policy.cedar` are gone. Run inputs and JSON Schema property names stay as the runtime reads them. Example manifests already invoke `bun` by name and do not encode arrays as comma-separated strings. |
| 2026-09-24 | 0 | DX-5: example refs omit `digest` instead of `sha256("")`. `EMPTY_SHA256_HEX` stays for one release and still means unpinned, as does an empty `hex_hash`. DX-6: enforce policy `ResourceRef.digest` (do not delete the field); not implemented. |
| 2026-09-24 | 0 | DX-9A: `SYSTEM.md` adds universal working habits (read before editing, surgical edits, egress proxy at `http://127.0.0.1:3128`). `SHIPPED_TEMPLATE_VERSION` and `template_version` are 9, so a workspace still on version 8 is refreshed. |
| 2026-09-24 | 0 | DX-7: `hosted/first-party.md` limits "not implemented" to the files and RAG broker designs. `security/cedar.md` no longer lists `Action::"EgressRequest"` as a Cedar PEP. Egress stays with the network layer and adapter `egress_domains` allowlists. No PEP was wired. |
| 2026-09-24 | 1 | DX-80, DX-17, DX-18 decided. No proto changes. Playbooks are soft guidance (DX-81 rejected). Environment profile takes `preferred_models`, `max_cost_per_million_tokens`, `context_policy`, `reasoning_policy`, and `resource_requirements` (`vcpu_count`, `memory_mib`, `storage_gib`, `network_mode`). HITL stays a generated `ExecuteTool` overlay; `telemetryPolicy` stays a separate ref. |
| 2026-09-24 | 7 | **DX-80 revised by the product owner, superseding the prose-only ruling above.** A playbook is a dual-layer procedure: prose on every step plus an *optional* tool binding with mode `bound` / `assisted` / `agentic`. The `oneof step_action` must go — a step needs prose *and* a binding. DX-81 (`advisory`/`enforced`) stays rejected; the scheduler stays deferred. New: DX-80A (reshape `PlaybookStep`), DX-81A (literal vs templated args), DX-81B (what the model sees per mode), DX-81C (mark inert fields). Found while exploring: playbooks are stored but never executed — no DAG walker exists, `driver.rs:64` hardcodes one `PromptAction`. Rationale: `design/developer-experience.md` §24. |
| 2026-09-24 | 1 | DX-10 and DX-11: `SkillManifest.capabilities` (`tool:`, `egress:`, `secret:`, `fs:`) and `SkillManifest.policy` (Cedar fragment). Shape is validated; scoped-permit check is TODO(DX-62). Until that exists, a skill fragment is unvalidated and must not be loaded from an untrusted source. |
| 2026-09-24 | 1 | DX-16 and DX-16A: skill `localExec.entrypoint` is manifest-relative (`./execute.js`) and resolves from a renamed install directory. `manifest.json` is the tool contract. `SKILLS.md` keeps the prose `load_skill` injects. Publish fails when frontmatter tools disagree with the manifest. |
| 2026-09-24 | 7 | DX-81A: a `bound` step's `binding.arguments` must be literal. Any `{{…}}` fails validation at `steps[i].binding.arguments.<field>` naming the field; `assisted`/`agentic` may template. `vam_spec::validation::bound_step_args_digest` returns the Phase 0 `canonical_args_digest` of literal args so the call is inspectable before it runs. |
| 2026-09-24 | 7 | DX-80A: `PlaybookStep` drops the `step_action` oneof. Every step has `instruction`; `binding` (`tool_name` + `arguments`) is optional; `mode` is `bound`, `assisted`, or `agentic`. Deferred workflow fields stay. A step with prose and a binding round-trips through `json_to_proto`. |
| 2026-09-24 | 1 | DX-19, DX-19A, DX-19B: canonical agent package layout in `docs/src/content/docs/authoring/agent-package.md`. A consumed skill is a reference; `skills/` holds only skills this repo authors. No package may write `SYSTEM.md` (guest write-protect plus overwrite when `SHIPPED_TEMPLATE_VERSION` 9 exceeds frontmatter). `Vamfile` stays out of the package; it is the Phase 9 escape hatch. `vam init` not scaffolded. |
| 2026-09-24 | 1 | DX-12 and DX-13: `SkillManifest.playbooks` (manifest-relative procedure paths) and `PlaybookSpec.requires` (skill refs and capability refs). A playbook that declares two skills pins both at run admission. No scheduler, no per-step advisory/enforced. |
| 2026-09-24 | 1 | DX-14 and DX-15: `AgentSpec.extends` is a pinned parent agent URN. `AgentOverlay` appends or replaces instructions, adds or removes skills, and grants or revokes capabilities. Chains are at most 8 hops. Revoking a capability a remaining skill requires fails. Two pins of one skill URN fail unless the child removed the earlier pin. Inheritance, not a call. Phase 1 exit is met; `current_phase` is 2. |
| 2026-09-24 | 2 | DX-31: `vam agent release`, `vam policy release`, `vam playbook release` (`--version X.Y.Z`, POST `/v1/<kind>/<urn>/release`) and `vam playbook ls`. `playbook ls` needed a new `GET /v1/playbooks` (`ListPlaybooksResponse`, own drafts plus released rows). All four round-tripped against a local vamd; HTTP `put` drafts are stamped 1.0.0, so that is the version to release. |
| 2026-09-24 | 2 | DX-26, DX-27, DX-28: global `--json` (one JSON document on stdout, NDJSON when streaming, `{"error":{code,exitCode,message,httpStatus}}` on failure). Stable exit codes 0–9 in `vam_cli::output::ExitCode`, mapped from HTTP status. Prompts fail with exit 2 and name their flag when stdin is not a TTY or `--json` is set. No spinners; help colour off when not a TTY or `NO_COLOR`. Schema and table in `getting-started/cli.md`. |
| 2026-09-24 | 2 | DX-20 and DX-21: `vam run <agent> -p "..."` starts a run with no `run.json`. A short name matches the last segment of an installed agent URN (`GET /v1/agents`); a URN is sent as `agentRef` with no version so the host pins the latest release. The workspace is `volume://<short-name>`. An unknown name exits 4 and names `vam agent ls`. Two matches exit 2 and list the URNs. `vam run start <path>` and `--file` still load a run file. |
| 2026-09-24 | 2 | DX-22 and DX-23: a run file is `vam run --file <path>` only. A positional path exits 2 and names that flag. `vam run --workspace <ws> --thread <t> -p "..."` continues that volume and thread with no agent and no run file (`--thread` defaults to `main`). `vam run continue` is not a command; it exits 2 and names the workspace form. No prompt without `-p`. |
| 2026-09-24 | 2 | DX-24 and DX-25: `vam run <agent> --playbook <urn> --input k=v` checks pairs against `PlaybookSpec.input_schema` and posts that playbook. `vam run --dry-run` resolves the agent, skills, and capability policy, prints allow / deny / HITL with Cedar `matchingPolicies`, and does not start a run. |
| 2026-09-24 | 2 | DX-29 and DX-30: CLI errors are three lines (`Wrong`, `Expected`, `Fix`) and do not print a raw HTTP status. Exit codes 0–9 stay. `--json` still includes `httpStatus`. Every mutating command accepts `--idempotency-key` (session create also accepts `--idempotency`). Omit the flag and `vam` mints a key. Phase 2 checkboxes are complete; `current_phase` is 3. |
| 2026-09-25 | 2.5,10 | **MCP promoted to a core capability and split by tier.** The earlier analysis was framed around *local* MCP servers, but the ones that matter (Jira, Linear, Sentry) are **remote** — an HTTPS endpoint with no process to host, so no broker and no sandboxing is involved. `host_tools_http.rs:137-214` already resolves a per-caller vault secret, attaches it as an `Authorization` header and makes the call; a remote-MCP client is that same flow with a JSON-RPC body. Created **Phase 2.5 — MCP as a core capability** (DX-130 to DX-138) sequenced right after the CLI work, moved DX-111 → DX-136 and DX-113 → DX-137 into it, and re-scoped Phase 10 to local/stdio servers and OAuth only. Added locked decision 20 (three tiers; no `vamsec_` placeholder for MCP; no third-party code in `vamd`; MCP tools surface as ordinary `host_exec` tools so the guest stays MCP-unaware; pin tool lists and re-approve on drift). Rationale: `design/developer-experience.md` §26. |
| 2026-09-25 | 2 | **Phase 2 verified complete — all 12 items across 7 PRs (-).** DX-81A (bound-step literal args) shipped as Batch 0; DX-31 release verbs; DX-26/27/28 `--json`, exit codes, non-TTY guard; DX-20/21 run by name; DX-22/23 `--file` and thread continue; DX-24/25 dry-run; DX-29/30 three-line errors and idempotency keys. Tracker reconciled after the reset: a local-only commit predating the Phase 2 branches was dropped and its one surviving note (DX-80A built during Phase 1) reapplied; its DX-81A "do this now" flag was discarded as obsolete since DX-81A is now done. `current_phase` set to **2.5**, not 3. |
| 2026-09-25 | 2.5 | **Phase 2.5 re-verified against the tool lifecycle; six gaps found and fixed before any code was written.** (1) **Schema delivery was missing entirely** — the guest gets only tool *names* over vsock (`pep.rs:98-158`); descriptions and JSON schemas come from `manifest.json` on the guest filesystem (`skills.rs:224-306`), so a host-only tool is invisible to the model. `seed_hsp_skills` (`hsp_store.rs:366`) already solves this; DX-133 now says mirror it. (2) `adapter_id` is **not** in `ToolCatalogEntry` (five fields only, `session.rs:670-676`) — it is parsed off disk by `parse_host_exec_from_manifest` (`host_tools.rs:307-332`), so the manifest file is the source of truth. (3) **Dots are illegal in tool names** — `is_tool_name` (`validation.rs:1975-1984`) allows `[a-z0-9_]` with a 64-char cap, so the planned `jira.create_issue` would have failed validation; DX-134 corrected, plus reserved-name and uniqueness checks. (4) Seeding is **node-wide in HSP but must be per-user for MCP** or one user's tools leak into another's session (DX-133A). (5) The catalog is seeded **once at session setup** (`manager.rs:809-834`), so connecting mid-session is a no-op until restart (DX-133B). (6) The adapter allowlist comes from `ctx.config["allowlist"]`, which has no skill author to populate it for MCP — left lax it is an SSRF primitive (DX-130A). Also added DX-133C (`SKILLS.md` consistency), DX-137A (default `side_effect` to true; it is the HITL lever via `security.rs:704-749`) and DX-137B (vendor descriptions reach the system prompt at `instruction.rs:60-68`, a prompt-injection channel). Phase 2.5 is now 15 items. |
| 2026-09-25 | 2.5 | DX-130 and DX-131. `mcp_client.rs`: blocking JSON-RPC 2.0 client (`initialize` + `notifications/initialized`, paginated `tools/list`, `tools/call`), JSON or SSE response bodies, `Mcp-Session-Id` carried. `McpClient::connect` requires `https` and goes through `PinnedUrl::validate_and_pin` + `apply_to_client_builder`, redirects off, `timeout_ms` honoured (default 30 s). Typed `McpClientError`: `UrlRejected`, `Transport`, `JsonRpc`, `Protocol`. `mcp_registry.rs`: `mcp_connections` keyed `(scope, owner_user_id, connection_id)` with the same org/user CHECK as `host_secrets`; `fetch_tools_snapshot` resolves the credential like `host_tools_http` and uses the secret's `allowed_destinations` as the SSRF allowlist when set. Org connections snapshot with the org secret only. vamd routes `PUT/GET /v1/mcp/connections`, `DELETE /v1/mcp/connections/:id`, `POST …/:id/refresh` (scope `secrets:write`; org writes need operator/org_admin). CLI `vam mcp connect|list|disconnect|refresh` with `--json`, three-line errors, `--idempotency-key`. No manifest generation. DX-130A (allowlist derived from the connection URL for the call-time adapter) still open. Tests: 12 orchestrator (encode/decode, error kinds, SSE, pagination, loopback/link-local rejection, registry round-trip) and 2 CLI, all against a loopback mock. |
| 2026-09-25 | 2.5 | DX-130A and DX-132. The MCP allowlist is the exact host of the stored connection URL (`connection_host` / `allowlist_from_connection_url`). `McpClient::connect` rejects an empty list and any `*` pattern before `PinnedUrl`, and redirects stay off (`Policy::none`). Reconnect to a different host is rejected. `vam mcp connect --scope org|user` with `--value`, `--from-env`, or `--file` writes the token through `put_secret` / `put_secret_with_owner` and stores `secret_binding_id` as `<connection>/auth-header`. Org writes still require `can_write_org_secrets()`. The token is not logged or returned. Tests: host A cannot call host B, redirect is not followed, two callers send their own tokens to a loopback mock. |
| 2026-09-25 | 2.5 | DX-133, DX-133A, DX-133B, DX-133C. `seed_mcp_skills` writes `mcp-<connection>/manifest.json` and `SKILLS.md` in the same shape as `seed_hsp_skills` (the guest loader skips a directory with no `SKILLS.md`). Called beside both HSP seeds in `manager.rs`, filtered to the session owner plus org connections; a user row shadows an org row with the same id. `parse_mcp_adapter_id` / `mcp_adapter_id` (`mcp:<connection>:<tool>`) and a host dispatch arm next to the hosted branch. No guest, proto, Cedar, HITL, or telemetry changes. DX-133B decision: do not re-seed a live session; connect/refresh/disconnect say a restart is required. Seeded `side_effect` is 3 until DX-137A adds a per-tool override. |
| 2026-09-25 | 2.5 | DX-134 and DX-135. Guest tool name is `{connection}__{vendor}` (`-` becomes `_`; other illegal vendor characters become `_`). That string is the catalog and Cedar id; the adapter id still carries the vendor name for `tools/call`. Over 64 characters, a reserved builtin (`read_file` and friends), or two tools that legalize to one name reject the connection with a three-line error that names the tool. `record_tool_catalog` rejects a duplicate `(session_id, tool_name)` before `INSERT OR REPLACE`. Refresh uses `relate_approved_set` (the same check as `manifest_is_subset`): identical is a no-op, a strict subset is stored and the removed names are returned, and an added tool or a changed description or schema is not written until `vam mcp refresh --approve-drift` (exit non-zero, diff in the error). A description-only change is drift. |
| 2026-09-25 | 2.5 | DX-136, DX-137, DX-137A, DX-137B, DX-138. MCP tools reuse skill capability tokens (`tool:`, `egress:`, `secret:`). Every token from `tools/list` is `inferred` and `prompt`; the guest manifest lists the tokens and does not contain a Cedar `permit`. Default side effect is class 3. `--side-effect TOOL=read` stores a per-tool override. Descriptions are stripped of control characters, capped at 512 characters, and appended to the HITL approval. `docs/src/content/docs/runtime/mcp.md` now leads with the three-tier table and says `vamsec_` is never used for MCP. Phase 2.5 items are checked. `current_phase` is 3. Exit gap: a live `vam mcp connect linear` plus a microVM run that creates a Linear issue was not executed. A loopback mock showed the caller's bearer token on `tools/call`. A Cedar `forbid` of `Tool::"linear__create_issue"` returned Denied and the audit chain recorded DENY. |
| 2026-11-04 | 2.5 | **DX-139 security floor: MCP credentials were guest-injectable.** Reproduced first at the vault and egress-gate level (`mcp_credentials_stay_host_side`: the real access token reached the MCP host, and the MCP and token hosts entered the allowlist), then fixed with the reserved `connection` secret kind and a broker-side destination rule. Two process-mode scenarios added and `active`: with the fix disabled both fail at their refusal assertion, with it both pass; smoke set 91/91 green. The guest scenario keys on the gate refusing a known placeholder over plain HTTP, because the process node has no network to show an injection. Not reproduced with a running microVM guest. Residual: a broker approved before this change keeps its approval; its next call is refused if it names a `connection` row or a row pinned outside its `egressDomains`. |
| 2026-11-07 | 2.5 | **DX-139 ready to merge.** `origin/main` (notifications Phase 2) merged; one conflict in `vam-e2e` `engine.rs` cleanup, both behaviours kept. Closed the follow-up the release opened: `vam.notify` read its Slack token refs with a plain `decrypt_secret`; `decrypt_secret_not_connection` now refuses a `connection` row (delivery fails the row `secret_not_permitted`, gateway answers PermissionDenied), tests added. Scoreboard on the merged tree: `cargo check --workspace --all-targets` ok; `cargo fmt --all --check` ok; `cargo test -p vam-orchestrator -p vam-network` 2014 passed, 1 failed (`hsp_uds::uds_socket_acl_deny_fails_fast`, a socket permission test that cannot deny root; this session ran as root); `scripts/e2e/run.sh --smoke` 98/98 green; notifications scenarios 15 green, 1 skipped (`badge-counts-what-waits-on-you` needs ui), 0 red. Proven on the process-mode guest only; microVM check is the team checklist above. Open: other plain `decrypt_secret` readers (`host_tools_http.rs`, `host_tools.rs`, `host_tools_fixtures.rs`) not audited for a skill naming a `connection` row. |
| 2026-11-07 | 2.5 | **DX-139: HTTP host adapter closed.** `host_tools_http.rs` decrypted any `*-auth-header` and `*-base-url` secret a skill named and sent it to the skill's allowlist with no check of kind or pins, so a static MCP connection's `{conn}/auth-header` was reachable by any published skill. Now `decrypt_secret_for_host_call` refuses a `connection` row before decryption and returns the row's pinned destinations; the final target host must be inside them (exact, `*.base` or `*`; an empty list allows nothing; a `base-url` with no pins is pinned to its own host). A refusal is a `secret_not_permitted` tool error before any request exists. Behaviour change: an `auth-header` secret with no `--allowed-destination` is no longer attached by `http_get`/`http_write` (the guest egress proxy already ignored such rows). **Audit of the other plain readers:** `host_tools.rs` `collect_secret_values` and the broker-binding audit loop only collect values for output redaction and send them nowhere (left as is; redacting a connection value is the safe direction). `host_tools.rs` MCP credential reads (`mcp_registry`, OAuth refresh) are the host's own use of its connection and read the connection's exact scope (intended). The `host_tools.rs` `decrypt_secret` at the pad-then-secret adapter is inside `#[cfg(test)]`. `host_tools_fixtures.rs` `secret_echo` and `secret_forms` print a secret to the guest by design and exist on test nodes only; they now use `decrypt_secret_not_connection` as well. No other production reader sends a vault value onward. Scenario `skill-cannot-send-a-secret-to-an-unpinned-host`: red with the destination check disabled, green with it. The scenario covers the pin rule; the `connection`-kind refusal is covered by unit tests (`host_tools_http.rs`) and `tests/mcp_credentials_stay_host_side.rs`, because a static MCP connection cannot be created without a live MCP server. Scoreboard: `cargo check --workspace --all-targets` ok; `cargo fmt --all --check` ok; `cargo test -p vam-orchestrator -p vam-network` 2020 passed, 1 failed (`hsp_uds::uds_socket_acl_deny_fails_fast`, a socket permission test that cannot deny root); `scripts/e2e/run.sh --smoke` 99/99; notifications scenarios 15 green, 1 skipped (`badge-counts-what-waits-on-you` needs ui), 0 red. Process-mode guest only; microVM check is the team checklist above. |
