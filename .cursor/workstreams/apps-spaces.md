# Workstream: Apps and Spaces: deployed agents, shared Spaces, integrator kit

```yaml
status: active          # active | paused | done
slug: apps-spaces
current_phase: 0 (built, PRs 161 and 164 await merge)
base_branch: feat/apps-spaces
design_docs:
  - design/vam-apps-spaces.md   # read "Amendment 2026-11-28" first; it supersedes the phase list
  - design/self-aware.md        # S2 builds the set_space_agent_version seam P4 calls; S4 with P2
```

## Goal

A developer can deploy an agent as an App (one agent, one pinned version,
one App per environment), give their backend an App key, and give each of
their users, channels or tickets a private Space by an opaque ref, without
VAM learning who those people are. Runs for an App go through the one
`start_run`, on the App's pin, under the owner's agent access and the App's
forbid-only limits and secret bindings; idle Spaces are reaped and quotas
refuse with typed reasons. Done when P0 to P7 are closed (TypeScript SDK,
example app, docs page and Squrell-App Apps screens shipped) and the pilot
(one internal app, one client) has started on it. Personal use shows no
change at any point.

## Out of scope

- End-user approvals in shared Apps (v1 routes approvals to the App owner).
- App keys held in a browser; the short-lived-token widget is later.
- An image field on an App (the image is pinned inside the agent version,
  lane 2 I2).
- Environment promotion (`vam promote`, DX-92): staging and prod are two
  Apps.
- SDKs other than TypeScript before the pilot.
- Long work (E10 Phase 5) and harness H2-H4: held until the pilot.
- Org accounts; Space-scoped secrets; usage rollup and spend ceilings
  (design § What we are not building).

## Locked decisions

Do not re-litigate without editing this list. Source: `design/vam-apps-spaces.md`
§ Decision summary as replaced by the 2026-11-28 amendment §3.2, and
`work-outside-chat` locked decision 10 (ids are the identity), which this
tracker does not contradict.

**Apps-Spaces decision summary** (the design's locked summary as restated by
amendment §3.2; C1-C3 approved 2026-11-28; parts marked *proposed change* are
the amendment's changes and wait for the owner)

1. **Concepts.** Account, App, Space, Thread, Credential.
2. **App (C1).** The deployed agent: one agent URN and one pinned version;
   kind `personal` or `shared`; kind only changes defaults.
3. **One App per environment (C2).** Staging and prod are two Apps with
   separate Spaces, keys, secrets and quotas. "Environment" means
   staging/prod only.
4. **Shared App runs always name a Space (C3).** No shared default Space.
5. **Space.** A place, not an identity: one Drive9 workspace stored under its
   `sp_…` id; owner, agent and `app_id` are row columns. *Proposed change:* a shared-App Space
   is found by id or get-or-created by an opaque caller ref whose SHA-256 is
   stored as an idempotency key in `space_create_keys`; no `ref` column; the
   ref is never stored, logged or returned.
6. **Personal Apps.** One per `(owner, agent)`, created on first run; its
   draft and released default Spaces are today's default Spaces. The primary
   agent stays `users.primary_agent_urn` (amendment 2026-11-01).
7. **App pin authoritative (AS-2.6)** for shared-App Spaces;
   `AgentWorkspaceMismatch` stays as a consistency check. P4 routes App Spaces
   to the App pin by calling lane 3's `set_space_agent_version` (S2), where
   the `spaces` row is the authority and `workspace.json` is "last version run
   here", rewritten at the next session start (proposed, with Q10).
8. **Credentials.** One type on `api_keys` (shipped). App keys: holder
   `http`, `target_kind = 'app'`; never issued before the bearer path narrows
   them to their App.
9. **Holder.** `http` or `broker:<id>`; refused on any other transport.
10. **Authority.** Owner ∩ App limit; the App limit is forbid-only; personal
    Apps have none. Space is never a Cedar principal.
11. **Secrets.** Personal App: all owner secrets. Shared App: listed vault
    keys only (none at all between P4 and P5, AS-4.5).
12. **Target resolution.** Request → Credential → default → error; a request
    may only narrow a Credential.
13. **Retention.** `keep` (personal default) or `idle_ttl` capped at 30 days;
    per-Space `keep`.
14. **Existing state.** Nothing is moved or renamed (`work-outside-chat`
    decision 10). *Proposed change:* P2 only fills NULL `spaces.app_id`; a
    NULL `app_id` reads as the personal App of `(owner_user_id, agent_urn)`.

**Owner approvals 2026-11-28** (15-18, 20; 19 follows from agent-access A7,
approved 2026-11-27)

15. **App owner** is the developer who creates it; org admins
    (`users:admin`) can reassign it. Reassignment moves the App's Spaces and
    their idempotency rows in one transaction (that mechanism is proposed).
16. **Approvals** in shared Apps go to the App owner (and org admins) in v1,
    or the App uses read-only playbooks.
17. **App keys are backend-only in v1.**
18. **Integrator kit in TypeScript first.**
19. **Agent access applies to Apps** (agent-access A7): every App run passes
    `start_run`, so the App owner's access is checked at each start; creating
    or reassigning an App needs access; a revoke stops the next run and does
    not kill a live one.
20. **Long work stays design-only until the first pilot; harness H2-H4 held
    until pilots give real tasks.**

**Proposals (owner has not answered; not locked)**

- **Q10** Shared-App Spaces follow the App's pin (one write moves all; live
  runs finish on the old version); personal Spaces stay pinned with an
  "Agent updated, use vX?" prompt. Seam `set_space_agent_version` shared
  with lane 3 S2/S4 (`design/self-aware.md`, `.cursor/workstreams/self-aware.md`).
- **Q11** Minimal yank: a yanked version refuses new runs; Spaces and Apps on
  it get a forced upgrade prompt (settles DX-94).
- **Q12** The machine idea is called "VAM image"; "environment" stays
  staging/prod (`design/vam-image.md`, `.cursor/workstreams/vam-image.md`).
- **Q16** Lanes run in parallel on separate branches; one lane holds a hot
  file (`run_start.rs`, `catalog_resolve.rs`, `manager.rs`, agent spec proto)
  at a time. Apps P4 holds `run_start.rs` and `catalog_resolve.rs`; VAM
  image I2 starts after P4 merges and agent access is in `main`; self-aware
  S3 waits for agent access in `main`; S4 runs inside or after P2.
- **Owner question (P2):** bearer-path narrowing ships with App key issuance.
  The bearer path (`keys.rs:704-760`) ignores `target_kind` and `expires_at`
  for `http` keys; today's `main` mints no such key, so nothing is exposed
  until AS-2.7 (design amendment §1, §9 question 7).
- The App carries the DX-17 environment profile once DX-17 is implemented
  (design amendment 2026-11-28 §6).

## Status legend

- `[ ]` not started
- `[~]` in progress
- `[x]` done (code + tests, or merged)

## Phases

P1 (one Credential type) shipped in E5 (`work-outside-chat`). Phase scope
and scenario names: design amendment 2026-11-28 §7.

### Done phases

Done phase sections are deleted; git history keeps them.

| Phase | Name | Done |
|---|---|---|
| 1 | One Credential type (AS-1.1 to AS-1.4) | E5, `work-outside-chat` Phase 1 |

### Phase 0 — Model frozen

**Goal:** The amendment is merged, the Cedar `App` entity is proven, the
protos are additive and the scenario list for P2-P7 is recorded.  
**Depends on:** none (owner answers given 2026-11-28)  
**Plan:** approved 2026-11-28 (breakdown below; owner answered "approved" to Q1-Q10)  
**Exit:** amendment merged; `cargo test -p vam-security` proves a forbid-only
App policy denies a tool its owner may use; AS-0.4 policy audit written;
`cargo test -p vam-spec` passes with the additive protos.

- [x] Design amendment 2026-11-28 merged (PRs 153 and 157)
- [x] AS-0.1 Proto (PR 161): `App`, `Space.app_id`, `RunSpec.app_id`, `RunStatus.app_id`, `CallerContext.app_id = 11`
- [x] AS-0.3 Cedar `App` entity and forbid-only tests (PR 164; `evaluate_in_app`)
- [x] AS-0.4 Policy audit (`principal ==` vs `in`) (PR 164, design amendment 2026-11-28 audit section)
- [x] Scenario names for P2-P7 confirmed (list in the Phase 0 breakdown, Scenario delta)

**Frozen numbers (W1):** `Space.app_id = 8`, `RunSpec.app_id = 14`, `RunStatus.app_id = 24`, `CallerContext.app_id = 11`, `App` fields 1-8. Self-aware S2 starts new `Space` fields at 9.

#### Phase 0 breakdown (approved 2026-11-28)

Read-only breakdown, 2026-11-28, against `feat/apps-spaces` @ `f1c1431`. Format: `phase-playbook.md`
§3 step 2, as `agent-access/p3-breakdown.md`. Item prefix `AS-0.n`. `Decide: NOW (proof: tracker
exit; cost 1: two crates plus proto)`. `lane: no` for every wave: no guest or VM path changes
behaviour, although `proto/` matches the full-tier lane filter (`scripts/ci/plan.sh:38`) on the
release PR.

##### Scenario delta

**P0 turns no scenario green** (design §7 P0: "PDP and proto unit tests only"). So **no wave 0**:
there is nothing to write as `pending`. P0's scenario work is to record the names below in the
tracker. Existing scenarios that guard files P0 touches are re-run in W1:
`space-and-thread-can-be-renamed-in-place` (guards `api/spaces.rs`),
`broker-cannot-be-given-an-mcp-credential` (guards `caller_context.rs`) and
`trigger-starts-a-run-as-the-grant-owner` (CallerContext stamping).

**Names for P2 to P7** (design `:1050-1197`; all `process` unless marked). World pieces that exist:
`org-admin`, `org-administrator`, `org-publisher`, `org-member`, `org-member-email`, `agent-plain`,
`agent-with-guarded-tool`, `agent-with-shell-and-secret-skill`, `agent-with-egress-probe`. New
pieces (none exists, so no clash): `shared-app`, and the variants below.

| Phase | Scenario | Given (world pieces) | Flag |
|---|---|---|---|
| P2 | `a-developer-creates-a-shared-app-pinned-to-a-released-version` | `org-publisher`, `agent-plain` | — |
| P2 | `an-app-cannot-be-created-for-an-agent-the-owner-may-not-use` | `org-member`, `agent-plain` + access-policy step | needs agent-access runner steps (not in `main`; P2 already depends on it) |
| P2 | `an-app-key-cannot-reach-the-owners-other-agents-or-keys` | `shared-app` (new) | — |
| P2 | `a-revoked-app-key-starts-nothing` | `shared-app` | — |
| P2 | `an-org-admin-reassigns-an-app-and-the-new-owner-gets-its-runs` | `org-admin`, `shared-app` | — |
| P2 | `agent-workspaces-are-isolated` | existing | unchanged, must stay green (file exists) |
| P3 | `the-same-ref-returns-the-same-space` | `shared-app` | — |
| P3 | `two-refs-get-two-private-spaces` | `shared-app` | — |
| P3 | `an-app-key-cannot-open-another-apps-space` | two `shared-app` (aliases via `as:`) | — |
| P3 | `an-app-key-without-spaces-create-cannot-make-a-space` | `shared-app` | — |
| P4 | `an-app-key-run-lands-in-the-named-space-on-the-apps-pin` | `shared-app` | — |
| P4 | `a-shared-app-run-without-a-space-is-refused` | `shared-app` | — |
| P4 | `a-request-cannot-widen-an-app-key` | `shared-app` | — |
| P4 | `an-app-run-is-refused-after-the-owners-access-is-revoked` | `shared-app`, access-policy step | agent-access steps |
| P4 | `moving-the-app-pin-moves-every-space-and-a-live-run-finishes-on-the-old-version` | `shared-app`, two releases | pending Q10; 79 chars (allowed) |
| P4 | `an-app-approval-reaches-the-app-owner` | `shared-app`, `agent-with-guarded-tool` | — |
| P4 | `a-shared-app-run-reads-no-owner-secrets-before-bindings` | `shared-app`, `agent-with-shell-and-secret-skill` | — |
| P5 | `an-app-limit-denies-a-tool-its-owner-may-use` | `shared-app-with-a-tool-limit` (new) | — |
| P5 | `an-app-limit-denies-an-egress-host-its-owner-may-reach` | `shared-app`, `agent-with-egress-probe` | egress PEP principal note (issue 7) |
| P5 | `a-shared-app-reads-only-its-bound-secrets` | `shared-app`, `agent-with-shell-and-secret-skill` | — |
| P5 | `a-personal-app-keeps-all-owner-secrets` | `agent-with-shell-and-secret-skill` | — |
| P6 | `a-space-read-shows-when-its-history-expires` | `shared-app-with-idle-ttl` (new) | proposed (Q9) |
| P6 | `an-app-key-keeps-its-own-space` | `shared-app-with-idle-ttl` | proposed (Q9) |
| P6 | `an-app-reports-runs-tokens-and-cost-per-space` | `shared-app`, two Spaces | proposed (Q9) |
| P6 | `an-idle-shared-space-is-reaped-within-its-ttl` | `shared-app-with-idle-ttl` | — |
| P6 | `a-kept-space-is-never-reaped` | `shared-app` | — |
| P6 | `a-space-with-a-parked-run-is-not-reaped` | `shared-app`, `agent-with-guarded-tool` | — |
| P6 | `deleting-a-space-erases-it-at-once` | `shared-app` | — |
| P6 | `an-app-over-its-run-quota-is-refused-with-a-typed-reason` | `shared-app-with-a-run-quota` (new) | the typed reason comes from S1's `QUOTA` |
| P7 | `the-sdk-creates-a-space-runs-and-follows-events` | `shared-app` | — |
| P7 | `the-sdk-answers-an-approval-and-the-run-resumes` | `shared-app`, `agent-with-guarded-tool` | — |
| P7 | `the-example-app-gives-two-visitors-two-private-spaces` | `shared-app` | — |
| P7 | `the-sdk-reads-a-spaces-usage-and-expiry` | `shared-app-with-idle-ttl` | proposed (AS-7.5) |
| P7 | `an-owner-creates-an-app-and-issues-a-key-in-the-app` | Squrell-App | `needs: ui` (exists, `scenario.rs:81-85`) |
| P7 | `the-apps-screen-shows-each-apps-spaces-and-pin` | Squrell-App | `needs: ui` |

Notes for P2's wave 0: `TAGS` has no `apps` (`scenario.rs:7-24`), so either use
`workspace`/`members`/`permissions` or add `apps` (a one-line `vam-e2e` change). A piece cannot
take parameters (only `as:`), which is why the variants are separate pieces.

##### Items

**AS-0.1 Additive proto** (W1).
- `proto/vam/v1/orchestrator.proto`: after `ListSpacesResponse` (`:1204`), a section "Apps" with
  `message App { string app_id = 1; string name = 2; string kind = 3; string agent_urn = 4;
  string agent_version = 5; string owner_user_id = 6; google.protobuf.Timestamp created_at = 7;
  google.protobuf.Timestamp deactivated_at = 8; }`. `kind` holds `personal` or `shared`.
  `deactivated_at` is absent while the App is active. `Space`: `string app_id = 8; // app_…;
  empty until Apps fill it (a NULL row reads as the personal App)`.
- `proto/vam/v1/workflow.proto`: `RunSpec` `string app_id = 14; // host-resolved; ignored until
  Apps resolution, never trusted from a client` (after `:232`); `RunStatus` `string app_id = 24;`
  (after `:365`).
- `proto/vam/v1/hosted_services.proto`: `CallerContext` `string app_id = 11; // host-stamped;
  empty for a personal run` (after `:140`).
- Literals: `crates/vam-orchestrator/src/caller_context.rs:77-88` (`app_id: String::new()`, plus a
  unit test that a stamped context has an empty `app_id`); `crates/vam-orchestrator/src/api/spaces.rs:33-43`;
  `crates/vam-spec/examples/generate_baseline.rs:192` (do **not** run it).
- New `crates/vam-spec/tests/app_contract.rs`: the four `app_id` numbers and `App` 1 to 8 are
  locked (pattern from `encoding.rs:574-607`); `App` JSON round-trips through `json_to_proto` /
  `proto_to_json` with `"kind":"shared"`; `Space`/`RunStatus` with an empty `app_id` print no
  `appId`; `RunSpec` JSON with `appId` parses (documents the inert input).
- Accept: `cargo test -p vam-spec`, `cargo check --workspace`, the three guarded scenarios green.

**AS-0.3 Cedar App entity** (W2).
- `crates/vam-security/schema/vam.cedarschema`: `entity App;` and `entity Agent in [App];`
  (`User` unchanged).
- `crates/vam-security/src/engine.rs`: `pub fn evaluate_in_app(&self, req: &EvaluationRequest,
  app_id: &str) -> DecisionOutcome` on `CedarPolicyEngine` and `ArcSwapPolicyEngine`. It shares the
  body of `evaluate` through a private `evaluate_with(req, Option<&str>)`, and `request_entities`
  gains the principal entity (`Agent` uid with parent `App`) plus the `App` entity, built as JSON.
  Deny with a diagnostic when the principal type is not `Agent`, or `app_id` fails `app_` plus 26
  lowercase base32 characters (the `spaces::mint_id` alphabet, `spaces.rs:31-52`). `evaluate`'s
  behaviour is byte-identical, and an empty Tool list still uses `Entities::empty()` when no App
  is given. Docs comment: forbid-only App limits are written `forbid (principal in App::"<id>",
  …)`. Validating that a limit is forbid-only is P5 (AS-5.1).
- `crates/vam-security/tests/pdp_tests.rs`: the four tests in Outcomes, and a strict-validation
  test showing that `forbid (principal in App::"app_…", …)` compiles against the default schema.
  The wave checks whether Cedar 4.12 (`Cargo.lock`) needs the parent entity present, and keeps
  the `App` entity in the store either way.
- Accept: `cargo test -p vam-security`, `cargo test -p vam-orchestrator security::` (the sample
  policies still compile against the schema, `security.rs:4315-4325`), `cargo check --workspace`.

**AS-0.4 Policy audit** (W2). A new subsection "AS-0.4 Policy audit (2026-11-28)" at the end of
amendment 2026-11-28 in `design/vam-apps-spaces.md`, built from issue 7: the list with file:line,
the verdict "no meaning change", the App-limit shape rule (forbid-only, `principal in App::"<id>"`,
never `==`), and the three P5 notes (egress principal, legacy converter, `App::` in agent
policies). It also notes the P5 alternative of a fourth Tool attribute `app_rule` beside
`agent_rule/source_rule/org_rule` (`pdp.rs:96-104`). That is a breaking schema change for every
`ToolEntity::new`, so the hierarchy stays the plan.

**AS-0.5 Scenario names and tracker** (close, phase session, docs-only PR). Copy the table above into
the P2 to P7 sections of the tracker, and record the numbers and the stale-line fixes.

##### Waves

| Wave | Items | Turns green | Repo | Model | Budget | Depends on | CI | lane |
|---|---|---|---|---|---|---|---|---|
| 0 | none: no scenario turns green in P0 | — | — | — | — | — | — | — |
| 1 | AS-0.1 | none (re-runs 3 guarded scenarios) | vam | Sonnet 5.5 medium (new proto and API schema across `vam-spec` and `vam-orchestrator`) | 60 min | approval | `CI: scenarios=space-and-thread-can-be-renamed-in-place,broker-cannot-be-given-an-mcp-credential,trigger-starts-a-run-as-the-grant-owner tests=workspace` | no |
| 2 | AS-0.3, AS-0.4 | none | vam | Sonnet 5.5 high (the Cedar policy boundary; plan goes to the owner) | 90 min | 1 | `CI: tests=workspace` (`vam-security` forces it anyway) | no |
| review | phase close, read-only, W2 diff | — | — | Opus 5.5 high (Q8) | 20 min | 2 | — | — |
| close | AS-0.5, ticks, progress row | — | vam | phase session (Sonnet medium) | 20 min | review | docs-only (runs `docs.yml`) | no |

W1 and W2 touch different crates, but they run one at a time (playbook §3). W1 goes first so the
numbers are in the branch before S1 or I1 reach their proto items. Each PR targets
`feat/apps-spaces`, is titled `AS-0 W<n>: <summary>`, and says "Do not merge". Commits follow
AGENTS.md:256: the maintainer's identity with `git commit -s`, and no `Co-Authored-By`, session
links or "Generated with" lines.

**Files per wave.**
- W1: `proto/vam/v1/{orchestrator,workflow,hosted_services}.proto`, `crates/vam-orchestrator/src/caller_context.rs`,
  `crates/vam-orchestrator/src/api/spaces.rs`, `crates/vam-spec/examples/generate_baseline.rs`,
  `crates/vam-spec/tests/app_contract.rs` (new).
- W2: `crates/vam-security/schema/vam.cedarschema`, `crates/vam-security/src/engine.rs`,
  `crates/vam-security/tests/pdp_tests.rs`, `design/vam-apps-spaces.md` (audit section).
- Close: `.cursor/workstreams/apps-spaces.md`, `.cursor/workstreams/README.md` row,
  `design/vam-apps-spaces.md` (stale lines).

##### Lane collisions (wave 1 siblings)

| Lane | Proto files and numbers it may take | Other shared files | P0's position |
|---|---|---|---|
| S1 typed denials (`/mnt/project-files/self-aware/s1-breakdown.md` SA-1.1 to 1.5) | `common.proto`: new `Denial`, `DenialReason`, `SuggestedFix`, `FixChange`, `FixWho` (new messages, so no numbers on existing ones); packed into the existing `ErrorStatus.details` (`common.proto:172-177`); `events.proto`: `denial` on `PolicyDecidedPayload` and `EgressDecidedPayload` (their next free numbers); **regenerates `baseline/vam_descriptor.bin`** | `security.rs`, `api/mod.rs`, `api/error.rs`, `docs/api/openapi.yaml`, `vam-runtime/src/agent.rs`, possibly `encoding.rs` lock tests | P0 touches none of these files, does not run `generate_baseline`, and puts its lock test in a new file. It edits one line of `generate_baseline.rs` (a literal), which S1 runs but is not planned to edit. No overlap. |
| I1 curated bases (`.cursor/workstreams/vam-image.md` Phase 1) | `orchestrator.proto` `NodeStatus` (next free 17, for `status.images[]`) and `NodeBase`/new image message (`:81-133`); maybe `skills.proto` `SkillManifest` (next free 14, lockfile) | `golden.rs`, `node_base.rs`, `skill_package_resolve.rs`, `scripts/base/`, `vam-runtime` exec, `vam-cli` node init | Same file `orchestrator.proto`, different messages about 1,000 lines apart, no shared numbers, no name clash (`App` vs image names). W1 lands first. I1 rebases on a clean text merge. |
| S2 (later, not wave 1) | new fields on `vam.v1.Space` (`design/self-aware.md:252`) | `run_start.rs`, `catalog_resolve.rs` | P0 takes `Space` 8; S2 starts at 9 (Q10). |
| Agent access (when released to `main`) | `orchestrator.proto` `GetMeResponse` (`:704-718`) and new access messages; regenerates the baseline | `openapi.yaml` | No shared message or number. P0 leaves the baseline alone, so merging `main` later has no binary conflict. |

##### Test commands

```bash
cargo test -p vam-spec                       # conformance (one-way vs baseline), examples_json, openapi_yaml, app_contract; builds generate_baseline
cargo test -p vam-spec --test conformance    # the only proto drift check
cargo test -p vam-spec --test examples_json  # the CI `checks` job step
cargo test -p vam-security                   # pdp_tests incl. the App forbid tests
cargo test -p vam-orchestrator caller_context api::spaces security::
cargo test -p vam-schedule                   # CallerContext consumer (uses ..Default)
cargo check --workspace
cargo clippy --workspace --all-targets -- -D warnings
scripts/e2e/run.sh --scenarios space-and-thread-can-be-renamed-in-place,broker-cannot-be-given-an-mcp-credential,trigger-starts-a-run-as-the-grant-owner   # W1
scripts/ci/local.sh --fast --base origin/feat/apps-spaces
```

There is no OpenAPI drift check beyond `openapi_yaml.rs`'s named paths, no `buf`/proto lint, and
no generated docs. `cargo run -p vam-e2e -- check` runs only on the close PR (via `docs.yml`).

##### Risks

- **Inert `appId` input (Q4).** A client can send it and get no error. Nothing reads it before P4,
  and P4 must treat it as a request, never as a fact. Write this into P4's AS-4.3 acceptance.
- **Cedar dangling parents.** If 4.12 refuses a parent that is not in the store, the `App` entity
  is added (planned anyway). Strict validation of `principal in App::` on an action whose
  principals are `[Agent, User]` is possible for `Agent`, so it is accepted. W2 proves both in
  tests.
- **An unused `evaluate_in_app` until P5.** No fail-open risk: nothing calls it, and `evaluate` is
  unchanged. The risk moves to P5, where the PEP must call it at every site: tool (`security.rs:952,
  :1529`), path limb (`:1009`), advertise (`advertise.rs:289-340`), and the egress set (issue 7).
  P5's deny matrix (AS-5.4) is the proof.
- **The schema now accepts `App::` in agent policies** (issue 7). It is inert until P5, and P5
  refuses it at publish (Q6).
- **Text merge in `orchestrator.proto` with I1 and agent access.** Different hunks. W1 is small
  and lands early.
- **Generated module churn.** Editing any `vam.v1` file regenerates the single module that also
  holds `agentic_spec.proto`'s types. They are unchanged, and nothing generated is committed. The
  baseline is not regenerated.
- **Tracker size.** The scenario tables add about 3K tokens to `apps-spaces.md`, still under the
  40K rule (AGENTS.md §2.1).

##### Tracker, progress log and design edits at close

- Tracker P0: tick all five boxes. Set `Plan: approved <date>` and mark P0 done. AS-0.1 line gets
  the numbers (`Space.app_id = 8`, `RunSpec.app_id = 14`, `RunStatus.app_id = 24`,
  `CallerContext.app_id = 11`, `App` 1 to 8). Paste the scenario table into P2 to P7.
  `current_phase: 2 (blocked: agent access not in main)`, and the README row to match. Then
  delete the P0 section per playbook §7, leaving a "Done phases" row.
- Progress log row: `2026-MM-DD | 0 | AS-0.1 proto (PR …), AS-0.3 Cedar App entity and AS-0.4
  audit (PR …), scenario names P2-P7 recorded. Scoreboard: 0 green (none planned), 0 red, <n> fix
  cycles, <wall time>`.
- Design `vam-apps-spaces.md`: the AS-0.4 section (W2). §7 P0 AS-0.1 gets the concrete numbers
  (`:1026-1030`). §1 "Not on main" (`:807-809`) drops "a Cedar `App` entity", since the schema
  has it and the PEP does not yet. Amendment 2026-10-29 §4 `:238-239` points at
  `encoding.rs:574-607` and `vam-spec/tests/app_contract.rs` instead of `triggered-runs.md`. The
  §7 P2 table's Given "restricted agent" becomes `agent-plain` plus an access-policy step. One
  line under §2 says the original AS-0.2 `identity_requirement` flags are deferred to P4 (Q5).
- Coordinator note (Q10), so the S1, S2 and I1 sessions see the reserved numbers.

##### What must not be touched

`crates/vam-orchestrator/src/api/run_start.rs`, `catalog_resolve.rs`, `manager.rs`,
`proto/vam/v1/agentic_spec.proto`, `proto/vam/v1/baseline/*`, `docs/api/openapi.yaml`,
`docs/src/content/docs/**`, `crates/vam-orchestrator/src/security.rs`, `api/mod.rs`,
`common.proto`, `events.proto`. If a wave finds it needs one of these, it stops and the phase
session asks the owner.

### Phase 2 — Apps

**Goal:** Personal and shared Apps with a pinned version, and App keys.  
**Depends on:** Phase 0; agent access released to `main`  
**Plan:** not drafted  
**Exit:** `/v1/apps` create (access checked), list, get, deactivate,
reassign; personal App on first run with no visible change; App keys issued
and narrowed to their App by the bearer path; `agent-workspaces-are-isolated`
stays green.

- [ ] AS-2.1 `apps` table and store
- [ ] AS-2.2 Personal App on run start; fill `spaces.app_id`
- [ ] AS-2.5 Shared App API, reassignment, `vam apps`
- [ ] AS-2.7 App keys and bearer-path narrowing

### Phase 3 — Shared Spaces by ref

**Goal:** Get-or-create a shared-App Space by an opaque caller ref.  
**Depends on:** Phase 2  
**Plan:** not drafted  
**Exit:** the same ref returns the same Space; another App's Space is
unreachable; only the ref's hash is stored.

- [ ] AS-3.4 `POST /v1/apps/{id}/spaces {ref}`, list and get by App
- [ ] AS-3.5 `last_active_at` for App Spaces
- [ ] `POST /v1/spaces` refuses `app:` idempotency keys

### Phase 4 — One `start_run` resolution

**Goal:** App, Space and thread resolved once in `start_run`, on the App's pin.  
**Depends on:** Phases 2 and 3; lane 3 S2 (`set_space_agent_version`); holds `run_start.rs` and `catalog_resolve.rs` (Q16)  
**Plan:** not drafted  
**Exit:** resolution table proven for HTTP and the gateway; a shared-App run
without a Space is refused; the App pin is authoritative; the old request
shape is unchanged; a shared-App run resolves no owner secrets until
Phase 5 lands.

- [ ] AS-4.1 Resolution in `start_run`
- [ ] AS-2.6 App pin authoritative; `set_space_agent_version` seam with lane 3
- [ ] AS-4.3 Narrow, never widen
- [ ] AS-4.4 Stamp `app_id`; filter runs by App and Space
- [ ] AS-4.5 Shared-App runs resolve zero owner secrets until Phase 5

### Phase 5 — App limits and secret bindings

**Goal:** A shared App can only take authority away from its owner.  
**Depends on:** Phases 0 and 4  
**Plan:** not drafted  
**Hand-off from P0 (Opus review, PR 164):** (1) the engine does not itself enforce forbid-only: with the schema change, catalog publish accepts `permit (principal in App::...)` and `forbid ... unless { principal in App::... }`, which `evaluate_in_app` would honour. P5 must reject any `App` reference outside a plain forbid scope (and `unless`/`!` forms) at publish and when composing a run's policy set, before wiring `evaluate_in_app`. (2) `security.rs:643` holds `Arc<dyn PolicyDecisionPoint>`, so the inherent method is unreachable there: P5 adds a defaulted trait method or a downcast. (3) Egress checks use the fixed principal `Agent::"egress"`; compile App egress limits per run set.  
**Exit:** a shared-App run is denied a tool, a secret and an egress host its
owner could use.

- [ ] AS-5.1 App limit in the PEP
- [ ] AS-5.2 Secret bindings
- [ ] AS-5.3 Layer order
- [ ] AS-5.4 Deny matrix

### Phase 6 — Quotas and retention

**Goal:** Idle Spaces are reaped, quotas refuse with typed reasons.  
**Depends on:** Phases 3 and 4  
**Plan:** not drafted  
**Exit:** idle Spaces reaped within the 30-day cap; quotas refuse with typed
reasons; erasure immediate.

- [ ] AS-6.1 Retention policy and `spaces.keep`
- [ ] AS-6.2 Reaper (skips locked and parked)
- [ ] AS-6.3 Erasure and deactivation
- [ ] AS-6.4 Quotas

### Phase 7 — Integrator kit and Apps screens

**Goal:** An outside developer can build on an App from TypeScript and manage it in Squrell-App.  
**Depends on:** Phase 4 (Phases 5 and 6 for the docs' limits section)  
**Plan:** not drafted  
**Exit:** TS SDK published (create Space, start run, follow
`/v1/runs/{id}/events`, answer approvals); example app runs against a
process-mode vamd; docs page live; Squrell-App Apps screens shipped.

- [ ] AS-7.1 TypeScript SDK
- [ ] AS-7.2 Example app
- [ ] AS-7.3 Docs page
- [ ] AS-7.4 Squrell-App Apps screens

## Explicitly later

Work that must not be pulled into this tracker.

- Pilot (one internal app, one client): follows Phase 7.
- End-user approvals; browser widget with short-lived tokens.
- Other-language SDKs.
- Self-aware lane items (lane 3, `.cursor/workstreams/self-aware.md`: typed
  denials, Space upgrade banner, bulk upgrade); P4 only calls the shared
  `set_space_agent_version` seam that S2 builds.
- VAM image lane items (lane 2, `.cursor/workstreams/vam-image.md`).
- Long work (`.cursor/workstreams/agent-runtime.md` Phase 5,
  `design/long-work.md`): waits for the pilot.

## Progress log

| Date | Phase | What landed (end with the scoreboard: scenarios green, red, fix cycles, wall time) |
|---|---|---|
| 2026-11-28 | — | Tracker opened; design amendment 2026-11-28 written (C1-C3 and owner decisions approved). Scoreboard: none yet |
| 2026-11-28 | 0 | Phase 0 built: W1 additive proto (PR 161), W2 Cedar App entity and audit (PR 164, Opus review GO, nothing blocking), both into `feat/apps-spaces`, awaiting owner merge. Frozen numbers: Space 8, RunSpec 14, RunStatus 24, CallerContext 11, App 1-8. Scoreboard: 0 scenarios turned green (none planned), 3 guarded scenarios green, 0 red, 0 fix cycles, about 3 h wall time |
