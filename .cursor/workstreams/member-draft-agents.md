# Workstream: Member draft agents (members author their own agents from released parts)

```yaml
status: active
slug: member-draft-agents
current_phase: 2 (approved 2026-11-18, wave 0 next)
base_branch: feat/member-draft-agents
design_docs:
  - design/member-draft-agents.md
  - docs/src/content/docs/orchestrator/user-isolation.md
  - docs/src/content/docs/orchestrator/catalog.md
  - docs/src/content/docs/getting-started/users-and-roles.md
```

## Goal

A member creates, edits and runs their own draft agents, built only from
released skills, released playbooks, released policies and connectors the org
admin enabled and the member connected; a member's agent uses released parts
only, at save and at run. API keys authorize by the user's live role, so
promotion and demotion take effect on keys as they do on sessions. The run route
never writes a released catalog row. Publishers keep today's rights; members
still cannot release, cannot author skills, playbooks, policies or skill
packages, cannot see or attach anyone's drafts, and cannot overwrite another
user's row.

## Out of scope

- Members releasing or sharing agents (a publish request flow).
- Deleting drafts (nobody can today).
- The VAM app's screens (the frontend team builds on the Phase 4 handoff).
- Honouring `resourceRequirements` (not honoured for anyone today).
- `extends` / `overlay` merge at run time (not wired for anyone today).
- Renaming decision 9's branches in `isolation-verify-closeout` (a separate
  old-repo cleanup task).

## Locked decisions

Do not re-litigate without editing this list. Locked on the owner's approval of
this capture; the open questions of Phase 1 may still adjust 2 and 3 before
wave 0 starts. Rationale and evidence: `design/member-draft-agents.md` §4.

1. **Effective API-key scopes.** At authentication: the bootstrap key and any
   key whose owner's live role is Operator keep their stored scopes; a key with
   `scope_mode = role` gets `default_scopes_for_role(live role)`; a key with
   `scope_mode = explicit` gets `stored ∩ default_scopes_for_role(live role)`.
   The result replaces `record.scopes` on the `AuthenticatedKey`; the stored list
   stays on the row and in key listings. `GET /v1/me` reports effective scopes.
2. **`api_keys.scope_mode`.** `role` when minted with no scope list, `explicit`
   otherwise. Migration: `role` where the stored list equals, as a set, the
   defaults of the owner's current role; every other existing row `explicit`.
   Mint keeps an above-role explicit scope on the row (it is capped at
   authentication).
3. Session tokens (already live), grant holders and brokers (never HTTP
   bearers; the gateway already reads the live role) do not change.
4. Release requires `publish:grant` in the **effective** scopes (or
   `keys:admin`), so a key narrowed without it cannot release and a revoked
   publisher cannot either. A queued message re-derives its caller's scopes at
   start.
5. **The run route never writes the catalog.** *Amended 2026-11-18 by the
   product owner:* option B. `POST /v1/runs` refuses any `inlineAgent` or
   `inlinePlaybook` (any content, with or without a `resourceId`) with 400 and a
   message to save it first (`PUT /v1/agents`, `PUT /v1/playbooks`) and run it
   by `agentRef` / `playbookRef`; nothing is written. `vam run --file` saves the
   draft, shows any refusal, then runs the draft by ref (`-draft`). The
   draft-and-release writers (`put_*_with_owner`, `put_*_released_with_owner`)
   are test-only. (Was: an inline agent is saved as the caller's draft through
   the `PUT /v1/agents` function and the run pins it; an inline playbook is
   saved as a draft by a caller with `catalog:write`.)
6. **`agents:draft`** is a new scope in the member, publisher, org admin and
   bootstrap defaults. Restricted mode = the caller holds `agents:draft` and not
   `catalog:write`. Restricted mode may use `PUT /v1/agents` and
   `PUT /v1/agents/{urn}/permissions` on its own drafts (and Always allow/deny on
   its own draft runs, as today). Every other `catalog:write` route stays closed
   to it: skills, skill packages, playbooks, policies, migrate, delete, release,
   release suggestion, hosted brokers.
7. **Released only, at save and at run.** A restricted save refuses (400) any
   skill, playbook or policy ref that does not resolve to an existing release,
   with the same message for a missing URN and someone else's draft. A run whose
   agent row is owned by a user whose live role does not author (Member)
   resolves in a `ReleasedOnly` mode: floating = latest release, `X.Y.Z` = that release or an
   error (no "kept as-is"), `X.Y.Z-draft` = error. Recording and session start
   use the same mode; `permissions.advanced` refs are recorded and loaded by pin.
   *Amended 2026-11-18 (follows decision 5, option B):* the clause "and an
   inline agent with no row run by such a caller" is dropped; no run carries an
   inline agent.
8. A restricted save refuses `capabilityPolicy`, `hitlPolicy`,
   `instructionArtifact`, `extends`, `overlay` and `completionPolicy.evaluatorRefs`.
9. A restricted save may create a new agent URN only under
   `<registry domain>/<default_namespace(username)>/`; editing an own draft
   skips this. At most `VAMD_MEMBER_DRAFT_AGENTS_MAX` (default 50) agent drafts
   per member.
10. A member may pin their own draft agent on a workspace; foreign drafts stay
    404. Drafts are visible to their owner and to `users:admin` only.
11. Amends `isolation-verify-closeout` locked decisions 3 and 6 (amendment lines
    there point here): **3** members list/get released catalog without
    `catalog:write` and create/edit their own draft agents with `agents:draft`;
    skills, playbooks, policies, packages and every release stay on
    `catalog:write` / `publish:grant`. **6** members pin released versions or
    their own draft agent, never a foreign draft.

## Status legend

- `[ ]` not started
- `[~]` in progress
- `[x]` done (code + tests, or merged)

## Phases

### Done phases

Done phase sections are deleted; git history keeps them.

| Phase | Name | Done |
|---|---|---|
| 1 | API keys authorize by the live role | 2026-11-17: #22 (scenarios), #24 (scope_mode, effective scopes, release on effective `publish:grant`), #25 (queued start, docs). 3 scenarios active and green. |

### Phase 2 — The run route never writes the catalog

**Goal:** `POST /v1/runs` refuses any inline agent or inline playbook with 400
and writes nothing; agents and playbooks are saved only through their PUT
routes and run by ref. `vam run --file` saves the draft, shows any refusal and
runs the draft by ref. No production path but the release routes and the
marketplace import can write a released row.  
**Depends on:** none in code; runs after Phase 1 in this lane (both touch `api/mod.rs`, so never in parallel)  
**Plan:** approved by the owner 2026-11-18 (option B)  
**Exit:** the three scenarios below `active` and green, the re-run list green;
`vam run --file <dir>` saves and runs a publisher's draft and shows a member
the PUT's refusal (CLI tests and the team checklist; the `cli` scenario step
has no host or key, so it cannot prove this).

- [x] Wave 0: scenarios and harness
- [ ] Wave 1: run route refuses inline parts; writers test-only; CLI and
  bench save-then-run; test fallout; proto, OpenAPI and docs lines (one PR)

**Benchmark delta:** none. A security floor (design §6 R1), no benchmark line.
`scripts/bench-coldstart.sh` changes its requests, not what it measures.

**CI (owner, 2026-11-17):** fast local checks in the session
(`scripts/ci/local.sh --fast` and the wave's scenarios), the PR opens ready,
GitHub Actions is the gate.

**What the code does today** (checked 2026-11-17/18 on the base, Phase 1 merged)

- `start_run` (`crates/vam-orchestrator/src/api/run_start.rs:216-601`) is the
  only reader of `inline_agent` / `inline_playbook` in `src/`. An inline agent
  with a `resourceId` calls `put_agent_with_owner` (`:345-351`; `catalog.rs:767-784`:
  draft **and** release at 1.0.0); an inline playbook calls
  `put_playbook_with_owner` (`:382-388`; `catalog.rs:951-970`). The only gate is
  `runs:write` (`api/mod.rs:1360`). Other inline branches: `:338-342` (unbound
  workspace), `:377` (message names `inline_agent`), `:437` →
  `run_context::ensure_playbook_ref` (`run_context.rs:590-612`), and
  `has_inline` for a busy workspace (`:283`, `api/inbox.rs:74`, `:471`).
- Those two lines are the **only production callers** of the draft-and-release
  writers (`put_*_with_owner`, `put_*_released_with_owner`,
  `put_agent|playbook|skill|policy`, `put_platform_policy`). Every other caller
  is a `#[cfg(test)]` module or one of **17 calls in 6 integration-test files**
  (`tests/api_tests.rs`, `hermetic_tests.rs`, `legacy_permissions_tests.rs`,
  `agent_check_tests.rs`, `playbook_examples_tests.rs`, `process_mode_run_api.rs`),
  which link the library without `cfg(test)`, so hiding the writers needs a
  feature. The marketplace import writes its rows in its own transaction; the
  release routes use `release_*_with_meta`; vamd seeds nothing at boot.
- Who sends inline parts: `vam run --file <dir | run.json>`
  (`crates/vam-cli/src/cmd/run.rs`: `load_run_inputs` `:2080-2172` reads
  `agent.json` / `playbook.json` next to the run file; `start_from_path`
  `:1836-1852`; `start_run` `:350-412` PUTs both and **discards the answers**
  `:357-374`, then posts the run with both inline) and
  `scripts/bench-coldstart.sh` (PUT `:295`, inline agent in the run `:319`,
  `POST /v1/runs` `:333`). The trigger gateway, the queue starter and the e2e
  harness never send inline parts. No docs page mentions them.
- An inline agent **without** a `resourceId` already fails for everyone (202,
  then session start: "the run records no agent", `manager.rs:313-319`); an
  inline playbook without one is silently ignored.

**Scenario delta** (design/scenarios.md §9; touched scenarios found from
`guards` on `catalog_publish.rs`, `api/mod.rs`, `api/catalog_auth.rs`,
`keys.rs` and `given: org-*`; no scenario guards `api/run_start.rs`,
`catalog.rs` or `cmd/run.rs`)

| Scenario | Change | Layer | Given | Why |
|---|---|---|---|---|
| `a-run-with-an-inline-agent-is-refused-and-writes-nothing` | new, `pending` | process | `org-admin` as `admin`, `org-publisher` as `pub`, `org-member` as `member`, `agent-inline-probe` (new) | R1: member and publisher both 400, no row for anyone |
| `a-run-with-an-inline-playbook-is-refused-and-writes-nothing` | new, `pending` | process | `org-admin` as `admin`, `org-publisher` as `pub`, `org-member` as `member`, `agent-plain`, `playbook-draft-probe` | R1 for playbooks; a plain chat is untouched |
| `a-publisher-saves-a-draft-agent-and-runs-it-by-ref` | new, **`active` guard** | process | `org-admin` as `admin`, `org-publisher` as `pub`, `org-member` as `member`, `agent-inline-probe` | the save-then-run path `vam run --file` moves onto; draft stays private |
| `member-chats-with-a-released-agent`, `always-allow-and-always-deny-are-remembered`, `release-shows-the-diff-against-the-previous-version`, `promotion-widens-a-default-key-but-not-a-narrowed-one`, `demotion-takes-catalog-writes-from-a-publisher-key`, `member-key-minted-with-catalog-write-cannot-write-the-catalog` | unchanged; re-run in W1 | process | as today | run by `agentRef`, draft runs, Always saves and release must not move (`catalog_publish.rs` writers move) |

The three new scenarios guard `api/run_start.rs`, `catalog.rs`,
`catalog_publish.rs` and `api/inbox.rs`. Sketches (red class `product`):

- `a-run-with-an-inline-agent-is-refused-and-writes-nothing`. `when`:
  `chat: { inline: agent-inline-probe, as: member, text: "e2e-step:im Hello", refused: true }`;
  `expect: { refused: true, status: 400, contains: "PUT /v1/agents" }`; the same
  `as: pub` (`e2e-step:ip`). `then`: `identity-request` as `admin`, `credential: key`,
  `path: "/v1/agents/{{urn:agent-inline-probe:pub}}"`, `status: 404`; the same
  for `{{urn:agent-inline-probe:member}}`; `pub` and `member` each get 404 on
  their own URN. (The admin holds `users:admin` and sees every draft, so 404
  means no row.) **Red at `when.0`:** a member's inline run is 202 today.
- `a-run-with-an-inline-playbook-is-refused-and-writes-nothing`. `when`:
  `chat: { agent: agent-plain, inlinePlaybook: playbook-draft-probe, as: pub, text: "e2e-step:pp Hello", refused: true }`;
  `expect: { refused: true, status: 400, contains: "PUT /v1/playbooks" }`; the
  same `as: member`. `then`: `admin` gets 404 on
  `/v1/playbooks/{{urn:playbook-draft-probe:pub}}` and `…:member}}`;
  `chat: { agent: agent-plain, as: member, text: "e2e-step:mq Hello" }`,
  `expect: { run: succeeded }`. **Red at `when.0` or `when.1`:** today the
  playbook is released and the start answers 202, or 400 naming the `topic`
  input, never the `PUT /v1/playbooks` message; wave 0 records which.
- `a-publisher-saves-a-draft-agent-and-runs-it-by-ref`. `when`:
  `put-draft: { piece: agent-inline-probe, as: pub, status: 200 }`;
  `chat: { agent: agent-inline-probe, draft: true, as: pub, text: "e2e-step:sd Hello" }`;
  `expect: { run: succeeded }`. `then`: `pub` `GET
  /v1/agents/{{urn:agent-inline-probe:pub}}/versions` 200, `lacks: "releasedAt"`;
  `member` `GET` the agent 404; `member` `GET /v1/agents` 200, `lacks:
  "{{urn:agent-inline-probe:pub}}"`. **Green on today's code** (saving by PUT
  and running the draft by ref already work): it lands `active` in wave 0 as a
  guard, proves the new harness lookups, and must stay green through wave 1.

**Wave 0 harness additions** (`crates/vam-e2e`, documented in `e2e/README.md`
"Roles and drafts"; everything else reuses `put-draft`, `chat {draft, as}`,
`identity-request` and `expect {refused, contains}`):

1. An `agent` piece with `deferred: true` and no `release` is not written at
   `given` (`engine.rs:589-600`, today only playbooks skip). Existing deferred
   agent pieces all have `release: true` and keep their meaning. New piece
   `e2e/world/agent-inline-probe.yaml`: that, with the body of `agent-plain`.
2. `chat` takes `inline: <agent piece>` (the piece's `spec` sent as
   `inlineAgent`, no `agentRef`) and `inlinePlaybook: <playbook piece>` (sent
   as `inlinePlaybook`, with `agent:` or `inline:`). The URN's `{{name}}` is
   `<piece>-<as>-<scenario>-<runTag>`, the `put-draft` rule. `check.rs`
   refuses either without `refused: true` (vamd accepts neither any more).
3. `expect: { refused: true, status: <n> }`: the refusal's exact status
   (today any 4xx passes).
4. `put-draft` records the URN it wrote per `(piece, as)`; `chat { agent: <deferred piece>, as }`
   runs that URN (class `scenario` if no `put-draft` wrote one).
5. `{{urn:<piece>:<as>}}` in `identity-request` `path` (path-encoded),
   `contains` and `lacks`: the URN a `put-draft` or `chat inline | inlinePlaybook`
   step as `<as>` wrote or tried. None: class `scenario`.

**Items** (each names the scenarios it turns green)

- [x] `MD-2.0` — Wave 0 above. Files: `e2e/scenarios/` (3 new),
  `e2e/world/agent-inline-probe.yaml`,
  `crates/vam-e2e/src/{scenario.rs,engine.rs,identity.rs,world.rs,check.rs}`,
  `e2e/README.md`. **Accept:** `cargo run -p vam-e2e -- check` clean;
  `scripts/e2e/run.sh --scenarios a-run-with-an-inline-agent-is-refused-and-writes-nothing,a-run-with-an-inline-playbook-is-refused-and-writes-nothing`
  red at the `expectFail` steps above, class `product`;
  `--scenarios a-publisher-saves-a-draft-agent-and-runs-it-by-ref` green;
  `--smoke` green; `vam-e2e` unit tests for the deferred-agent rule, the URN
  lookup and `{{urn:…}}` expansion. `Decide: NOW — proof 3 (security floor R1), cost 1, line launch proof.`
- [ ] `MD-2.1` — **The run route refuses inline parts.** At the top of
  `start_run` (`api/run_start.rs:216`), before `resolve_place` (`:256`) and
  admission (`:280-298`): `req.inline_agent.is_some()` → 400
  (`InvalidArgument`) "inlineAgent is not accepted: save the agent with PUT
  /v1/agents and run it by agentRef (your draft is version {major: 1,
  prerelease: \"draft\"})"; `req.inline_playbook.is_some()` → 400, the same
  naming `PUT /v1/playbooks` and `playbookRef`. Any content, with or without
  `resourceId`. Nothing is created (no Space, thread, run, inbox message or
  catalog row). Remove the inline branches `:338-352`, `:382-388`, the
  `inline_agent` wording at `:377`, the inline arm of `ensure_playbook_ref`
  (`run_context.rs:590-612`, `:437`) and `has_inline` (`:283`,
  `api/inbox.rs:74`, `:471`; the busy-workspace refusal keeps its
  `playbookRef` arm). **Accept:** API tests in `tests/api_tests.rs`:
  inline agent with and without `resourceId`, as a member and as the
  bootstrap key → 400 naming `PUT /v1/agents`, no agent row, no Space, no run;
  inline playbook → 400 naming `PUT /v1/playbooks`, no playbook row; a busy
  workspace with an inline agent → 400 (the `:3909` test, now the inline
  message), no inbox message. Turns green the two `pending` scenarios.
  `Decide: NOW — proof 3 (R1), cost 1, line launch proof.`
- [ ] `MD-2.2` — **Release writers test-only.** `put_agent`,
  `put_agent_with_owner`, `put_playbook`, `put_playbook_with_owner`,
  `put_skill`, `put_skill_with_owner`, `put_policy`, `put_policy_with_owner`,
  `put_platform_policy` (`catalog.rs:763-784`, `:947-970`, `:1078-1101`,
  `:1115-1165`) and `put_agent|playbook|skill|policy_released_with_owner`
  (`catalog_publish.rs:1099`, `:1214`, `:1315`, `:1432`) behind
  `#[cfg(any(test, feature = "test-support"))]`;
  `crates/vam-orchestrator/Cargo.toml` gains `test-support = []` (never in
  `default`) and the self dev-dependency with that feature, the pattern of
  `brokers/vam-web/Cargo.toml:38-42`. **Accept:** `cargo check -p vam-orchestrator --lib --bins`
  without the feature (no production caller can exist); the PR lists the grep
  of callers. `Decide: NOW — proof 3, cost 1.`
- [ ] `MD-2.3` — **`vam run --file` saves, then runs by ref.** In
  `crates/vam-cli/src/cmd/run.rs` `start_run` (`:350-412`): with a playbook,
  `PUT /v1/playbooks` first (the agent may name it), then with an agent `PUT
  /v1/agents`; a non-2xx answer prints vamd's message plus one hint line
  ("`agent.json` is saved as your draft, which needs `catalog:write`; run a
  released agent with `vam run <agent> -p …`") and exits as any API error, with
  no `POST /v1/runs`. Then set `spec.agent_ref` (and `spec.playbook_ref`) to the
  saved URN at `{major: 1, prerelease: "draft"}`, no digest, and post the run
  with no inline parts. `agent.json` / `playbook.json` without
  `resourceId.urn`, or a `run.json` `agentRef` naming another URN than
  `agent.json`: exit 2 before any request. `load_run_inputs` (`:2080-2172`)
  and `start_from_path` (`:1836-1852`) keep their signatures; `dry_run_file`
  (`:1801-1830`) is unchanged. **Accept:** `crates/vam-cli/tests/cli_tests.rs`
  (httpmock): a directory with `agent.json` sends `PUT /v1/agents`, then one
  `POST /v1/runs` whose body has no `inlineAgent` and `agentRef.version.prerelease
  == "draft"`; with `playbook.json` the playbook PUT comes first and
  `playbookRef` is pinned the same; a 403 on the PUT prints the message and the
  hint, exits non-zero, sends no run; URN mismatch exits 2 with no request;
  `--dry-run` unchanged. `cargo test -p vam-cli`.
  `Decide: NOW — the CLI must not hide the refusal, cost 1.`
- [ ] `MD-2.4` — **`scripts/bench-coldstart.sh` save-then-run.** Check the PUT's
  status (`:295`, `-sf` already fails on 4xx; keep it), drop `inlineAgent` from
  the run body (`:319`) and pin `agentRef` to the draft. **Accept:**
  `bash -n scripts/bench-coldstart.sh`; the run body built by the script has no
  `inlineAgent` (`grep`). `Decide: NOW — cost 0.`
- [ ] `MD-2.5` — **Test fallout: inline runs become save-then-run.**
  - `tests/api_tests.rs` fixtures `create_run_body` (`:2099-2125`) and
    `sample_run_body` (`:3261-3292`) drop `inlineAgent` and pin `agentRef`
    `{major: 1, prerelease: "draft"}`; their callers (`:2195`, `:2223`, `:2260`,
    `:2341`, `:2373`, `:2401`, `:2518`, `:3374`, `:3423`, `:4110`, `:4182`,
    `:4235`, `:4260`, `:4330`) save first with `put_test_agent` (`:3295`) and
    the same key. Direct inline bodies `:2068`, `:2347-2348`, `:2381-2382`
    (the two `playbook_activation_offers_…` runs save the agent twice, same
    owner), `hermetic_tests.rs:3188`, `process_mode_run_api.rs:119`,
    `allowed_models_tests.rs:100`: the same change.
  - `idempotency_run_create_is_per_user` (`:1586`),
    `workspace_prefix_isolates_users_and_rejects_guest_override` (`:1655`),
    `spaces_are_created_once_per_key_and_renamed_in_place` (`:1790`) run as
    **members** (`two_member_keys`, `:1433-1460`) through `create_run_body`. A
    member cannot run a publisher's draft, so the agent is saved and released
    once in setup with a publisher (or the bootstrap) key, and Alice and Bob
    stay members and run `agentRef 1.0.0`. Their per-user assertions do not
    change.
  - Assertions on a stored `agentRef` of `1.0.0`, a released-kind Space or
    another user seeing the agent change to the draft pin; anything else that
    breaks is reported, not edited.
  **Accept:** `scripts/ci/local.sh --fast`; the re-run list green.
  `Decide: NOW — part of MD-2.1, cost 1.`
- [ ] `MD-2.6` — **Proto, OpenAPI and docs lines** (the full pass is Phase 4).
  `proto/vam/v1/orchestrator.proto:855-856`: comments say `inline_agent` and
  `inline_playbook` are refused (400); field numbers kept, not reserved or
  renumbered. `docs/api/openapi.yaml:5479-5482`: the same in their
  descriptions. `orchestrator/session.md` (`POST /v1/runs`, `:36`) and
  `orchestrator/api.md` (runs row, `:43`): an inline agent or playbook is 400;
  save it with the PUT route and run it by ref. `getting-started/cli.md`
  `--file` (`:177-190`) and `getting-started/quickstart.md:83`: `agent.json` next to
  the run file is saved as your draft (needs a publisher) and the draft is run;
  a rerun starts a new draft Space. `authoring/playbook-tutorial.md:71` made
  accurate. `proves:` on `orchestrator/user-isolation.md` gains the three
  scenarios. **Accept:** `cargo run -p vam-e2e -- check`; `cargo test -p vam-spec --test conformance`
  passes against `proto/vam/v1/baseline/vam_descriptor.bin` (comments only; if
  it flags them, regenerate with `crates/vam-spec/examples/generate_baseline.rs`
  and say so in the PR); `pnpm build`
  in `docs/` if available. `Decide: NOW — docs say what ships, cost 0.`

**Waves** (owner's matrix: Sonnet 5.5 high or medium, or Haiku 5.5 high)

| Wave | Items | Turns green | Repo | Model | Budget | Depends on | Actions | Status | PR |
|---|---|---|---|---|---|---|---|---|---|
| 0 | `MD-2.0` (3 scenarios, 1 world piece, 5 harness additions, README) | `a-publisher-saves-a-draft-agent-and-runs-it-by-ref` (guard); the two refusal scenarios red, right reason, class `product` | vam | Sonnet 5.5 medium: new step fields and lookups across five `vam-e2e` modules, and proving red for the right reason | 90 min | — | gate: `rust.yml` fast jobs and smoke; no VM path (`skip-vm-lane`) | [ ] | |
| 1 | `MD-2.1`–`MD-2.6` | `a-run-with-an-inline-agent-is-refused-and-writes-nothing`, `a-run-with-an-inline-playbook-is-refused-and-writes-nothing` (guard and re-run list stay green) | vam | Sonnet 5.5 high: who may write the catalog (refusal before any write, writers out of production builds) is a security boundary; the orchestrator reads the `start_run` diff, `Cargo.toml` and the CLI change before merging | 180 min | 0 | gate: full `rust.yml` and `docs`; no guest or VMM change (`skip-vm-lane`) | [ ] | |

**Open questions** (each with a default)

1. Status and wording: **default 400** (`InvalidArgument`) with the MD-2.1
   messages; the scenarios check only the status and `PUT /v1/agents` /
   `PUT /v1/playbooks`.
2. A CLI from before the upgrade still PUTs, then sends inline parts: its run
   is refused 400 after the draft was saved. **Default: accept**; the message
   says how to run by ref, and the release notes say to update the CLI.
3. `vam run --file` saves the playbook before the agent, and exits 2 on a
   missing `resourceId` or a URN mismatch between `run.json` and `agent.json`.
   **Default: yes.**
4. A rerun after the upgrade lands in the agent's **draft** Space (a default
   Space is per owner, agent and kind, `spaces.rs:245`), so a thread label used
   before starts fresh. **Default: accept**, one line in `cli.md`.
5. The quickstart and tutorials use `vam run --file`: a member now gets the
   PUT's 403. **Default:** the docs lines say these need a publisher (or the
   bootstrap key); Phase 4 owns the member path.

**Out of scope here:** member draft agents (Phase 3), the ReleasedOnly mode
(Phase 3), any change to the PUT routes, the release routes or the marketplace
import.

**Phase close:** unit suites on the merged base; release; team checklist out
(the next phase does not wait for it; playbook §6).

**Team checklist — Phase 2**

Setup: deploy vamd from the base branch, with a CLI built from it. Have a
publisher and a member, each with a CLI login or key, and one released agent
the member can chat with. In an empty directory `t/`, write `agent.json`:
`{"resourceId":{"urn":"urn:vam:agent:local.vam.dev/team/inline-check"},"displayName":"Inline check","role":"Tester","systemInstructions":"Answer briefly.","allowAnyPlaybook":true}`.

1. As the publisher: `vam run --file t -p "hello" --follow`. **See:** the run
   starts and replies.
2. As the publisher: `vam agent versions urn:vam:agent:local.vam.dev/team/inline-check`.
   **See:** a draft only, no release.
3. As the member: `vam agent ls`. **See:** `inline-check` is not listed.
4. As the member: `vam run --file t -p "hello"`. **See:** an error naming
   `catalog:write` and the hint line; no run starts.
5. As the publisher: `POST /v1/runs` with curl carrying `inlineAgent` (the
   body of `agent.json`). **See:** 400 telling you to `PUT /v1/agents` first;
   nothing new in `vam agent versions`.
6. As the member: the same with an `inlinePlaybook` (any playbook body with a
   new URN). **See:** 400 naming `PUT /v1/playbooks`.
7. As the member: `vam run <released agent> -p "hello"`. **See:** it replies,
   as before.
8. As the publisher: `vam agent release urn:vam:agent:local.vam.dev/team/inline-check --note check`.
   **See:** released; the member now lists it.

Report: "Phase 2 pass", or the step number with the error text.

### Phase 3 — Members create, edit and run their own draft agents from released parts

**Goal:** A member saves, edits and chats with their own draft agents whose
skills, playbooks and policies are released; the save refuses anything else
with a message naming the ref; the run resolves released-only even for a former
publisher's old drafts; drafts stay private and in the member's namespace.  
**Depends on:** Phase 1, Phase 2  
**Plan:** not drafted (scenario delta and wave sketch below)  
**Exit:** the five scenarios below `active` and green;
`catalog_read_authorize_tests::member_put_agent_is_403` replaced by the
owner-approved spec change (member PUT of an own draft is 200).  
**From Phase 2 (option B, 2026-11-18):** the run route refuses inline
agents, so a member's agent is only ever saved through `PUT /v1/agents`; the
restricted-mode validator has two callers (that route and the permissions PUT),
and wave 2 has no inline case.

- [ ] Wave 0: scenarios and harness (`publish-skill` takes `as` and
  `release: false`; agent pieces deferred and written by `put-draft` as a
  member; `chat {draft: true, as}` refused form if missing)
- [ ] Wave 1: `agents:draft`, restricted mode on the two routes, the save
  validator (refs, refused fields, namespace, cap)
- [ ] Wave 2: `ResolveMode::ReleasedOnly` in `run_resolve_context`; advanced
  refs recorded and loaded by pin; explain and check routes use the mode

**Scenario delta**

| Scenario | Change | Layer | Given | Why |
|---|---|---|---|---|
| `member-builds-and-runs-own-draft-agent-from-a-released-skill` | new | process | `org-admin`, `org-member`, a released skill piece, a deferred member agent piece (new) | the product: save 200, draft chat succeeds with the skill's tool; then a playbook `put-draft` and a `publish-skill` as the member are refused (red at the save: 403 today) |
| `member-draft-agent-refuses-a-skill-that-is-not-released` | new | process | `org-admin`, `org-member`, a draft-only skill piece | decision 7 at save: 400 naming the skill (red: 403 scope today) |
| `members-draft-run-takes-the-release-not-an-old-draft` | new | process | `org-admin`, `org-publisher`, a skill released then re-drafted with a different tool result | decision 7 at run: after demotion the floating ref runs the release (red: the owner's draft is used today) |
| `member-draft-agent-is-private-and-in-the-members-namespace` | new | process | `org-admin`, `org-member` as two aliases, `org-publisher`, `agent-plain` | R9, R10: another member gets 404; a URN outside the namespace is 400; another user's URN is 403 "only the artifact owner" (red at the first save) |
| `member-chats-with-a-released-agent` | unchanged; re-run | process | as today | members keep consuming released agents |

**Wave sketch**

| Wave | Items | Model | Why |
|---|---|---|---|
| 0 | scenarios, harness | Sonnet low | harness pattern |
| 1 | scope in `keys.rs` sets; `api/mod.rs:576`, `api/permissions.rs:155`; validator; namespace; cap; spec change to `member_put_agent_is_403` | Sonnet high | catalog write authority for a new class of user (security boundary) |
| 2 | `catalog_resolve.rs:180-202`, `:561-640`, `record_resolved_refs` advanced refs, `security.rs:398-470`, `permissions_view.rs:640-660`, `api/composition.rs:102` | Sonnet high | run-time resolution and session start must agree (security boundary) |

### Phase 4 — Docs and frontend handoff

**Goal:** The docs say what members, publishers and keys can do; the frontend
team has the handoff (`design/member-draft-agents.md` §9) on the docs site.  
**Depends on:** Phase 3  
**Plan:** not drafted  
**Exit:** pages updated, `vam-e2e check` and `pnpm build` green.

- [ ] `getting-started/users-and-roles.md`, `orchestrator/user-isolation.md`,
  `orchestrator/catalog.md`, `security/permissions.md`, `orchestrator/api.md`
  (member section with the handoff: show vamd's message, attach by refs only,
  never re-PUT a skill, show or hide create/release/author from `GET /v1/me`
  scopes, the member URN namespace, draft pins)

**Scenario delta:** none (docs only; `proves:` lists the scenarios of Phases
1–3). **Wave sketch:** one wave, Haiku 5.5 high (docs only), 45 min, actions
`docs` only.

## Explicitly later

- A member asks a publisher to release their agent (request and review flow).
- Draft deletion and archive.
- Capping `resourceRequirements` for members once the host honours it.
- Member access to `extends` / `overlay` once the run-time merge is wired.
- Renaming `isolation-verify-closeout` decision 9's branch names (old-repo
  cleanup).

## Progress log

| Date | Phase | What landed (end with the scoreboard: scenarios green, red, fix cycles, wall time) |
|---|---|---|
| 2026-11-17 | — | Tracker opened from the owner's member-authoring brief; design `design/member-draft-agents.md`; Phase 1 breakdown drafted |
| 2026-11-17 | 1 | **Phase 1 closed.** Merged #22, #24, #25 into `feat/member-draft-agents` (Actions green on each). Scoreboard: 3 scenarios green, 0 red; 0 scenario fix cycles. Next: Phase 2 breakdown (the run route never writes released rows). |
| 2026-11-17 | 2 | Phase 2 breakdown drafted (3 new scenarios, 3 waves); waiting for owner approval |
| 2026-11-18 | 2 | Owner chose option B (the run route refuses inline agents and playbooks; `vam run --file` saves then runs by ref) and two waves. Decisions 5 and 7 amended; Phase 2 rewritten (3 scenarios: 2 pending, 1 guard; waves 0 and 1). Plan approved; wave 0 next. |
| 2026-11-18 | 2 W0 | Wave 0 on `feat/member-draft-agents-scenarios-inline`: 3 scenarios (2 `pending`, 1 active guard), world piece `agent-inline-probe`, harness: deferred agent not written at `given`, `chat` `inline`/`inlinePlaybook`/`inputs` (with `refused`), `expect` `status`, `put-draft` URN per identity, `{{urn:<piece>:<as>}}`. Scoreboard: 1 green (guard), 2 red (right reason, class `product`, both at `when.0`: 202 today), 2 fix cycles (request shape; playbook inputs so the red is the inline rule), ~50 min. Local smoke: 9 Microsoft-mock scenarios fail the same on the base (container sandbox denies the guest's python under the `--out` path); Actions is the gate. |
