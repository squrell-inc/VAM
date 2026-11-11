# Launch 0.1.0 readiness: what is done on `main`, what is missing

Status: **audit** (2026-11-10). Read-only inventory of `main`
(the merge, E12 Phase 3). Written from the code, the scenarios, CI and
the trackers, not from the plan. Companion to `launch-0.1.0.md` (§1 proofs,
§2 scope, §4 lanes). Row 14 (`feat/machine-base`) and the bug-fix lane
 are in progress and were not audited as code.

## Summary (one screen)

**Verdict: not ready to launch today.** The machine itself is in good shape.
143 of 145 scenarios are active, and the release gate, manifest check and
catalog lint all pass. Three things block launch: the install does not
exist on `main`, the demo and the benchmark table are only partly built,
and the front pages of the docs site promise things the code does not do.

| Launch proof (`launch-0.1.0.md` §1) | State | Why |
|---|---|---|
| Live demo: scheduled `close-prep` run, one denial, gate decided from Slack a day later, five effects posted once, signed receipt | **Partly met** | Every piece exists and is green in process mode: schedule, denial, gate from Slack, effect journal and signed receipt. But there is no `close-prep` procedure or world, and the pieces have never run together in one run. Slack has never been used for real (fake transport only). The substitute demo, morning triage on Microsoft 365, has never touched a real tenant. |
| The table: two frontier and two open-weight models, `machine on` vs `off`, pass^k and cost per success | **Partly met** | Five models were benched on real endpoints: Claude Sonnet and Haiku 5.5, GLM-5, Qwen3.6-27B and DeepSeek V4 Pro. All runs used four short tasks with the machine on. `machine off` does not exist and the business tasks were never written (B1 not started). |
| The security page: every claim names its scenario, CI boots a VM, jailer and TLS on by default | **Mostly met** | `architecture/isolation.md` names a scenario or test per layer. Jailer and TLS are on by default. CI boots a microVM (11 scenarios). But the lane is advisory, and `security/overview.md` claims seccomp, SPIFFE and PII masking modules that do not exist. |
| The install: `vam node init` on one host, app or CLI signs in, standard base has Python and git | **Not met on `main`** | `vam node init`, `vamd --init` and the Python and git base are all on `feat/machine-base` (row 14, in progress). The quickstart on `main` has commands that fail. |
| The honest page: what VAM is not for, where open models still lose | **Not met** | `runtime/benchmark-results.md` honestly lists what was not measured. No page says what VAM is not for. The open models did not lose on these four tasks (all 20/20), so the data to show where they lose does not exist yet. |

**Must fix before launch** (details in §4.1, items M1 to M7):

1. **Docs front pages make false claims** (security, landing, quickstart, install, README, project status). Size M, docs pass.
2. **CI on `main` is red.** `egress-refuses-an-ungranted-host` failed in the smoke job: an ALLOW egress event appeared on a run whose agent has no internet. The scenario passes 3 of 3 when run alone. CI also loses the failure brief to an artifact path bug. Size S+M, bug-fix lane. Escalated to the orchestrator 2026-11-10.
3. **The install proof** (row 14). Size L, in progress.
4. **The demo has never run end to end**, on any procedure, with real Slack or a real tenant. The Microsoft 365 path also lacks the marketplace tag and publisher verification. Size M to L, connectors and product lanes plus the owner.
5. **The benchmark table needs B1 or an explicit re-scope by the owner.** Size L, proof lane.
6. **Two open bugs touch the "never twice" and receipt promises.**: a run reports "Sent." while the mock Graph saw 0 requests.: an effect is recorded as Sent where it should be Unknown. Size M each, bug-fix lane, in progress.
7. **The honest page** (what VAM is not for, where open models lose) does not exist. Size S for the page; the second half needs B1 data.

## 1. Checks run (2026-11-10, `main`)

| Check | Result |
|---|---|
| `cargo run -p vam-e2e -- check` | **pass**: 145 scenarios, 65 world pieces; features 98, proven 87, with a developer page 48 |
| `scripts/e2e/check-release-gate.sh` | **pass**: guest build, notify build, default tree, manifests |
| `scripts/e2e/check-manifests.sh` | **pass** |
| GitHub Actions, Rust, push of to `main` (run 42749491149) | **fail**. Passed: clippy, checks, cargo test (workspace), firecracker (microVM, advisory). Failed: scenarios (smoke), 130 of 131 pass, `egress-refuses-an-ungranted-host` failed at `then.5`, class product. The artifact upload then failed on a `:` in `drive9/catalog/skills/marketplace:m-…`, so the brief was lost. |
| Same scenario run alone in this session, three times | pass 3/3 (about 7.3 s each) |
| GitHub Actions, Docs, same push | pass |
| Previous `main` pushes | Rust green on each |

**Scenarios by status:** 145 in total.

- 143 active: 132 run in process mode and 11 on the Firecracker microVM lane.
- 1 pending: `badge-counts-what-waits-on-you`, which needs a UI runner.
- 1 retired: `receipt-states-effects-not-journaled-before-journal`, retired with the owner's approval in E12 Phase 3. `old-receipt-still-verifies` still proves the old form.

The smoke set is every process scenario except one tagged `slow`, which is 131 scenarios.

**Scenarios by tag** (one scenario can carry several tags; counts are of active scenarios unless noted):

| Tag | Count |
|---|---|
| chat | 76, plus 1 retired |
| approval | 33, plus 1 pending |
| connectors | 22 |
| triggers | 20 |
| notifications | 17, plus 1 pending |
| egress | 17 |
| members | 14 |
| context | 14 |
| playbook | 13 |
| gate | 10 |
| schedule | 9 |
| workspace | 9 |
| permissions | 5 |
| release | 4 |
| slow | 1 |

**Microvm-lane scenarios (11):**

- `vm-boots-and-registers-over-vsock`
- `vm-restores-from-the-golden-snapshot`
- `vm-run-waits-at-a-gate-and-finishes`
- `vm-task-killed-mid-effect-finishes-once`
- `workspace-matches-conversation-after-vm-stop`
- `jailer-confines-the-vmm-process`
- `node-reports-the-isolation-it-runs`
- `receipt-names-the-isolation-the-run-had`
- `guest-has-no-route-off-the-vm`
- `guest-proxy-never-binds-wildcard`
- `vault-secret-is-not-in-the-guest`

**Docs pages by status** (73 pages under `docs/src/content/docs/`):

| Status | Pages |
|---|---|
| shipped | 31 |
| planned | 11, all of `build/*` |
| stale | 0 |
| no status | 31 |

Every page with no status, plus `README.md`, is grandfathered in `e2e/docs-baseline.txt`, so the docs lint does not read it. That set includes the landing page, all of `getting-started/` and `security/overview.md`. The 31 `shipped` pages hold up well. The false claims sit almost entirely in the exempt pages, which are the ones a visitor reads first.

**Where things are proven, in one line.** Almost everything is proven in process mode with the mock model. Eleven behaviours are proven on the microVM lane: boot, snapshot restore, jailer, isolation line, no route, loopback proxy, vault, gate and kill mid-effect. Only one thing has ever run against a real external service: the benchmark, four short tasks on five models through Vercel AI Gateway, on the owner's KVM host. Nothing else has: no real Slack, no real Microsoft 365 or Entra tenant, no real Linear workspace. No team checklist (the manual pass on a deployed node) is recorded as passed in any tracker for E5, notifications, E12 Phase 3 or connectors.

## 2. Inventory by area

Columns:

- **Proven** says where a behaviour has been shown to work: *process* (a process-mode scenario on every PR, mock model), *VM* (the Firecracker microVM lane), *real* (a real external service), *unit* (unit or integration tests only) or *none*.
- **Docs** gives the page and its `status`. "no status" means the page is grandfathered in `e2e/docs-baseline.txt` and is not linted.

### 2.1 Install and run a node

| Capability | What it does | Evidence | Proven | Docs | Issues |
|---|---|---|---|---|---|
| `vamd` start and restart | The daemon starts and keeps sessions, parked approvals and work across a restart. | `follow-up-continues-after-vamd-restart`, `work-survives-vamd-restart-mid-task`, `parked-approval-survives-a-restart` | process | `orchestrator/overview.md`, shipped, true | — |
| TLS by default | Serves HTTPS with a self-signed certificate unless `--insecure-dev` is passed. | IT Phase 2; `daemon.rs`; `tests/tls_refusals.rs`; `vamd-refuses-plain-http-by-default` | process | overview and api, shipped, true. `getting-started/installation.md` still shows an `http://` host. | — |
| `vam node init`, `vamd --init`, standard base with Python and git | — | **Not on `main`.** `vam node` has only `status`, `keys` and `notify`. | none (row 14) | `build/templates.md`, planned | on `feat/machine-base` |
| Rootfs build | `scripts/build-rootfs.sh` and `build-rootfs-lima.sh` build the busybox, musl and Bun guest. | used by the CI lane (`scripts/e2e/firecracker-assets.sh`) | VM (indirect) | `build/rootfs.md` says **planned** although the script ships | — |
| One-host install docs | Quickstart, installation, macOS with Lima, Windows with WSL. | `getting-started/*` | none | no status; **false in places** (§3) | — |
| Sign-in | `vam login` / `logout`, `GET /v1/me`, `GET /v1/node` login probe. | user isolation phases; `api_tests.rs` | process | `orchestrator/user-isolation.md`, shipped | — |

### 2.2 Isolation and security

| Capability | What it does | Evidence | Proven | Docs | Issues |
|---|---|---|---|---|---|
| Firecracker microVM per session | The guest boots, registers over vsock and restores from the golden snapshot (about 225 ms session start on the CI lane). | `vm-boots-and-registers-over-vsock`, `vm-restores-from-the-golden-snapshot`, `workspace-matches-conversation-after-vm-stop` | VM; also real (bench on Lima) | `architecture/isolation.md`, `cold-start.md`, shipped, true | lane is advisory |
| Jailer on by default | On unless `--no-jailer` or `VAMD_USE_JAILER=0/false/off/no`; a typo keeps it on. | IT Phase 1; `daemon.rs`; `crates/vam-core/tests/jailed.rs`; `jailer-confines-the-vmm-process` | VM | isolation, `core/overview.md`, shipped, true | — |
| Isolation line | The node and every receipt state the isolation the run had. | `node-reports-the-isolation-it-runs`, `receipt-names-the-isolation-the-run-had` | VM | true | — |
| No route off the VM; loopback-only guest proxy | The guest has no default route, and its proxy binds 127.0.0.1 only. | IT Phase 2; `guest-has-no-route-off-the-vm`, `guest-proxy-never-binds-wildcard` | VM | `network/overview.md`, shipped, true | — |
| Host egress gate | Denies hosts that were not granted, and the cloud metadata address. An agent with **no `internet` setting resolves to allow**, which is a design choice. | `vam-network/src/broker.rs`; `vam-security/src/permissions.rs:277-279`; `egress-refuses-an-ungranted-host` and 16 other `egress` scenarios | process | `security/permissions.md` and isolation.md state the open default honestly. isolation.md still calls the refusal scenario "pending" (it is active). | **CI red** on this scenario (§1) |
| Secret vault and proxy substitution | Secrets are swapped in at the host proxy and scrubbed from responses, including encoded and oversize ones. | `vault-secret-is-not-in-the-guest` (VM); `secret-never-reaches-guest-encoded`, `oversize-response-with-a-secret-never-reaches-the-guest`; `mitm_tests.rs` | VM and process | `security/secrets-proxy.md`, shipped, true | no oversize **connector** response scenario |
| Connection credential kind | Connector and MCP tokens stay host-side, never on guest egress and never handed to brokers. Secrets go only to pinned hosts. |; `broker-cannot-be-given-an-mcp-credential`, `guest-cannot-use-an-mcp-token-placeholder`, `skill-cannot-send-a-secret-to-an-unpinned-host`; `tests/mcp_credentials_stay_host_side.rs` | process | `runtime/tools.md`, `orchestrator/api.md`. The "Secret kinds" line in api.md is stale: connectors also write this kind. | microVM repeat is still a team checklist item |
| Per-call egress envelope | A connector tool declared as a read cannot send: exact host, method and path per tool. |; `vam-network/src/connection.rs`; `tool-declared-as-a-read-cannot-send-mail` | process | api.md mentions only the flag | — |
| API rate limits | Floods are throttled per key; run creation and auth have their own budgets. | IT Phase 2; `rate_limit.rs`; `api-rate-limits-a-flood` | process | api.md, true | — |
| Cedar permissions and approvals | Allow, ask or deny per tool and host. Remembered decisions. Injected text cannot start a playbook. | `always-allow-and-always-deny-are-remembered`, `released-agent-approvals-are-once-only`, `injected-text-cannot-start-a-playbook`; `pdp_tests.rs` | process | `security/permissions.md`, shipped, true | — |
| Frame cap and token replay | 64 KiB control frame cap; forged or replayed host-tool tokens are refused. | `host-tool-output-fits-frame`; `adversarial_vsock_tests.rs` | process and unit | isolation.md says its scenarios run on the lane, but this one is a process scenario | — |
| SSRF guard, telemetry redaction | DNS pinning and restricted IPs (web broker); pre-spool redaction. | `ssrf_guard.rs`; `vam-telemetry` tests | unit | network and telemetry pages | no scenario |
| Tool confinement (Landlock, drop to uid 1000) | — | `vam-runtime/src/landlock.rs` | none | isolation.md admits it is not proved | silently skipped on kernels without Landlock |
| Security overview page | — | — | — | `security/overview.md`, no status: **false** (SPIFFE, PII masking, signed HITL tokens, custom seccomp) | §3 |

### 2.3 Agent runtime

| Capability | What it does | Evidence | Proven | Docs | Issues |
|---|---|---|---|---|---|
| Agent loop and tools | Chat, tool calls and typed errors for bad arguments. | E10 Phase 0; `plain-chat-gets-a-reply`, `tool-call-first-try-saves-a-draft`, `invalid-tool-arguments-return-a-typed-error` | process; real (bench task `first-try-tool-use`) | `runtime/agent.md`, `tools.md`, shipped | — |
| Steer, stop, queue | Corrections are followed mid-task, stop is clean, and follow-ups queue behind the running task. | E10 Phase 3; `steer-changes-the-running-task`, `correction-sent-mid-task-is-followed`, `stop-ends-a-running-task-cleanly`, `queued-message-runs-after-the-task` | process | `orchestrator/session.md`, shipped | — |
| Compaction, context caching | Summary compaction keeps early corrections; one cache plan per provider. | E10 Phase 2; `compaction-keeps-an-early-correction`, `follow-up-after-compaction` | process (compaction); unit (caching) | agent.md, inference.md | — |
| Effect journal ("never twice") | External effects are journaled and are not repeated after a guest crash or a kill mid-call. Unknown effects are settled by a verify read. | E10 Phase 4; `effect-is-not-repeated-after-the-guest-crashes`, `send-interrupted-mid-call-is-not-sent-twice`, `unknown-effect-is-reconciled-by-a-verify-read`; `vm-task-killed-mid-effect-finishes-once` | process and VM | `security/run-receipt.md`, shipped | **** (Sent vs Unknown under load) |
| Resume after restart or crash | A cold resume keeps the agent's identity and fails closed. One resume per decision. |; `decision-on-a-warm-session-resumes-once`, `long-thread-resumes-after-approval` | process | session.md | — |
| Harness switches and profiles | Loop detector, recitation, examples and resample are switchable and printed on the receipt with a digest. Profiles: `base`, `open-strong`, `open-small`. | H0, H1; `repeated-identical-call-is-caught-and-observed`, `harness-switches-print-on-the-receipt`, `named-profile-prints-on-the-receipt-with-its-own-digest` | process; real (bench) | agent.md, shipped | open profiles still say `evidence: "pending"` in `vam-spec/src/harness.rs` although bench rows exist |

### 2.4 Models and inference

| Capability | What it does | Evidence | Proven | Docs | Issues |
|---|---|---|---|---|---|
| Providers | OpenAI-compatible (OpenAI, OpenRouter, Vercel), Anthropic, and the mock. | `vam-inference/src/provider.rs` | real for Vercel only; unit for the rest | `runtime/inference.md`, shipped. The quickstart names Gemini, which has no provider. | — |
| Gateway routing | Slash ids go to the gateway; unknown bare ids are refused with a fix. |; `gateway-model-id-routes-to-the-gateway`, `unknown-slash-model-uses-the-node-default-gateway`, `unknown-bare-model-is-refused-with-a-fix` | process; real | true | — |
| Catalog and prices | Capabilities and the maker's list price per model; cost per success. | H1 | unit; real (bench) | true | — |
| Thinking models | Reasoning is recorded, never replayed when stripped, and kept for DeepSeek. | H1; `thinking-model-tool-calls-are-not-dropped`, `reasoning-is-recorded-and-never-replayed-when-stripped` | process; real (DeepSeek bench) | true | — |
| Resample, `tool_choice`, effort | One resample of an invalid call, then a typed error; options sent only where supported. | `invalid-call-is-resampled-once-before-a-typed-error`; `provider.rs` | process (resample); unit | agent.md | 0 resamples seen on real runs (the tasks are too easy) |
| Model allowlist and fallback | The model is a preference, not a gate. A caller off the allowlist is refused. Fallback stays inside the allowlist. | E10 Phase 4; `caller-model-off-the-allowlist-is-refused`, `provider-fallback-stays-in-the-allowlist` | process | inference.md; fallback is **undocumented** | — |

### 2.5 Catalog and releases

| Capability | What it does | Evidence | Proven | Docs | Issues |
|---|---|---|---|---|---|
| Drafts and releases | A private draft becomes an immutable semver release (agents, skills, playbooks, policies); the system suggests the next version. | agent-experience Phases 1 to 7; `vam agent/skill/playbook/policy put/release/versions/suggest-version`; `api_tests.rs` | unit/API; `member-chats-with-a-released-agent` (process) | `orchestrator/catalog.md`, shipped | no scenario walks the release flow itself |
| Release diff and `vam agent stats` | Per version: runs, success, pass^k, tokens and cost per success, approvals, effects. Release prints the diff first. | E12 Phase 3; `GET /v1/agents/{urn}/outcomes[/diff]`; `agent-version-stats-sum-its-receipts`, `release-shows-the-diff-against-the-previous-version` | process (mock: cost unpriced) | catalog.md, shipped (scenarios not in its `proves`) | — |
| E4 slice: pass^k and cost on the agent page | The app shows the numbers above. | API is ready; the page is a VAM app task | none here | — | not built |
| Git marketplace install | An org admin installs or updates a listing at its tag as read-only rows with an origin. Non-Squrell listings show "Unverified". | connectors Phase 4; `vam marketplace`, `/v1/marketplaces*`; `admin-installs-the-squrell-listing-at-its-tag`, `newer-tag-shows-update-available-until-the-admin-updates` | process, **vendored fixture only** | `orchestrator/overview.md` (env vars only); no page | the real `squrell-inc/vam-marketplace` has **no tags**, so an install at `microsoft-365-v1.0.0` would fail today |
| Install from any Git URL, `vam.json` version | — | no code | none | agent-package.md correctly says "later" | §2 SLICE row never done |
| `SKILL.md` read alongside `SKILLS.md` | — | only `SKILLS.md` is read (`vam-runtime/src/skills.rs`, `skill_package_admit.rs`) | none | `authoring/skill-manifest.md` is titled "(SKILL.md)", which misleads | §2 build-now row never done |
| Org inventory list | — | no route | none | — | §2 SLICE row never done |
| Origin from the URN domain | Org, Squrell or Local. | AX-7; `vam-spec/src/origin.rs` | unit | `authoring/model.md`, no status | — |

### 2.6 Skills, playbooks and approvals

| Capability | What it does | Evidence | Proven | Docs | Issues |
|---|---|---|---|---|---|
| Playbook runs | A matching request starts the playbook with inputs; a greeting or injected text does not. | `matching-request-starts-the-playbook`, `playbook-run-starts-with-inputs`, `greeting-starts-no-playbook`, `injected-text-cannot-start-a-playbook`; `vam playbook init/explain/test --trials` | process; real (bench, three tasks) | `runtime/workflow.md`, shipped | — |
| Gates | The run parks, shows options, and is decided, stopped or timed out. | `gate-decided-finishes-the-run`, `gate-offers-its-options-and-stop-ends-the-run`, `gate-left-open-shows-until-it-times-out`; `vm-run-waits-at-a-gate-and-finishes` | process and VM | workflow.md | — |
| Approvals and questions | Suspend and resume once, survive a restart, stop denies a pending one; a question waits; a rejected question ends the run within three turns. | `approval-suspends-and-resumes`, `parked-approval-survives-a-restart`, `question-waits-for-an-answer-and-resumes`, `rejected-question-ends-the-run-within-three-turns` | process | workflow.md; `security/hitl-lifecycle.md`, no status | — |
| Permission presets and effective view | Lists presets, prints the decided table, explains a tool or host. | `/v1/permission-presets`, `/v1/agents/{urn}/permissions/effective`; `vam agent permissions --effective`, `vam policy explain`; `permissions_view_tests.rs` | unit/API only | permissions.md, shipped | no scenario; app side not verified here |
| Skill cards (`ToolSpec.ui`) | A skill ships show and ask cards for its tools, checked at publish; an amended approval runs as amended. | connectors Phase 3; `mail-list-result-shows-as-the-skills-card`, `send-asks-with-the-skills-card-and-an-edit-runs-as-amended`, `a-skill-whose-card-does-not-fit-its-tool-is-refused-at-publish` | process | `runtime/jig.md` covers built-in cards only; skill-manifest.md omits `ui`, `egress` and `requiresConnector` | — |

### 2.7 Receipts

| Capability | What it does | Evidence | Proven | Docs | Issues |
|---|---|---|---|---|---|
| Signed receipt | An Ed25519 receipt per run that separates host facts from model text. `GET /v1/runs/{id}/receipt`. | E12 Phase 1; `receipt-separates-host-facts-from-model-text`; `receipt_route_test.rs` | process | `security/run-receipt.md`, shipped, true | chat runs have no `startedAt` (null `runs.created_at`) |
| Show and verify offline | `vam receipt show` and `vam receipt verify --trusted-keys`; tampering fails; old receipts still verify. | `receipt-verifies-offline-and-fails-on-tamper`, `old-receipt-still-verifies`; `crates/vam-cli/tests/receipt_cli.rs` | process | true | — |
| Isolation line | See 2.2. | `receipt-names-the-isolation-the-run-had` | VM | true | — |
| Who decided, and from where | Each approval and gate names the person and the surface (app, CLI, Slack); a denial shows before an approval. | `receipt-names-who-decided-and-from-where`, `receipt-shows-denial-before-approval`, `gate-decided-from-slack-names-slack-on-the-receipt` | process (Slack faked) | true (scenarios not in `proves`) | — |
| Effects section | Each external effect is counted once, including after a crash, with duplicates and unknowns. | E12 Phase 3; `receipt-lists-each-external-effect-once`, `receipt-counts-effects-after-a-crash`, `receipt-counts-an-unknown-effect-it-could-not-check` | process; VM for kill mid-effect | true | — |
| Receipt in the app and in Slack (E12 Phase 2) | — | the app receipt view is unchecked in `run-receipt.md` | none | — | — |

### 2.8 Triggers and schedule

| Capability | What it does | Evidence | Proven | Docs | Issues |
|---|---|---|---|---|---|
| Run grants and trigger gateway | A broker holding a grant starts runs as the grant's owner over its own socket. HTTP refuses grant tokens. A triggered run cannot arm another trigger. One run per key. | E5 Phase 1; `grants.rs`, `gateway.rs`; `trigger-starts-a-run-as-the-grant-owner`, `grant-token-is-refused-over-http`, `triggered-run-cannot-arm-another-trigger`, `repeated-fire-with-the-same-key-starts-one-run` | process | `hosted/overview.md`, shipped | — |
| Space and thread ids | Opaque `sp_…` and `th_…` ids; names are labels and can be renamed. |; `vam space`, `vam thread`; `names-are-only-labels`, `thread-is-reached-by-its-id` | process | **no page** | — |
| `vam.schedule` broker | Once, relative or cron in a time zone. Saved only after the user approves. Catch-up after a broker restart. Skip on a busy workspace. Revoke on delete. | E5 Phase 2; `brokers/vam-schedule`; `scheduled-message-runs-at-its-time`, `schedule-is-saved-only-after-the-user-approves`, `missed-fire-runs-once-after-the-broker-restarts`, `deleted-schedule-never-fires-again`, `another-users-schedules-are-invisible` | process | **no page**; AGENTS.md §3 omits `vam-schedule` and `vam-notify` | — |
| `wait_for` (on CI) | — | no code; `work-outside-chat.md` puts it in a later phase | none | — | `launch-0.1.0.md` §2 still lists it as a 0.1.0 slice |

### 2.9 Notifications

| Capability | What it does | Evidence | Proven | Docs | Issues |
|---|---|---|---|---|---|
| Record, list, CLI, agent tool | One record per user per approval, gate, question, background run end, grant expiry or schedule skip. Never shown to another user. `vam notifications`; the read-only `notifications_list` tool. | notifications Phase 1; `/v1/notifications*`; `approval-in-another-thread-reaches-the-list`, `notifications-are-never-shown-to-another-user`, `agent-reads-what-waits-without-acting` | process | **no page**; routes missing from OpenAPI | — |
| Badge | Unread count in the app. | `badge-counts-what-waits-on-you` | **pending** (no UI runner) | — | — |
| Slack channel and deciding from Slack | Slack DMs with buttons, quiet hours, rate limits and a linked, verified contact. A decision in the app updates the message. A gate decided in Slack resumes the run. | notifications Phase 2; `brokers/vam-notify`; `gate-decided-from-slack-names-slack-on-the-receipt`, `decision-in-the-app-updates-the-slack-message`, `slack-decision-needs-the-handle-and-the-linked-account`, `unverified-contact-receives-nothing` | process with the `fake-transport` feature; **never against a real Slack workspace** (`notifications.md`: "Live Slack not yet run") | **no page** (no operator setup docs at all) | — |

### 2.10 Connectors and marketplace

| Capability | What it does | Evidence | Proven | Docs | Issues |
|---|---|---|---|---|---|
| Built-in connectors: `microsoft-365`, `linear` | Connector definitions with validation. | connectors Phase 1; `vam-spec/src/connector.rs`, `spec/connectors/*.json` | unit | **no page** (`docs-features.yaml`: `pending_page: true`) | Microsoft 365 client id is a placeholder until set by env |
| Admin enable and consent | An org admin enables a connector for the node with the tenant's GUID; members see it only after that. | `connectors-reach-members-only-after-an-org-admin-enables-them`, `microsoft-365-waits-for-the-admin-consent` | process (mock Microsoft login) | none | real-tenant check CN-1.8 still `[ ]` |
| Member connect | Tokens stored; the token's tenant must equal the consented tenant; check and refresh. | `microsoft-365-connection-stays-in-the-consented-tenant` | process | none | — |
| Linear as an MCP connector | Fixed id with vetted read and write risk; reads run, writes ask; the id cannot be taken by a hand-added server. | `linear-reads-run-without-asking-and-writes-ask`, `connector-id-cannot-be-taken-by-a-hand-added-server` | process (mock MCP) | none | no real Linear run |
| Connection behind skills | The host swaps in the token per audience and refreshes it before expiry. A revoked connection parks the call and tells the member. A skill that needs a connector asks the member to connect first. | connectors Phase 2; `expired-microsoft-token-is-refreshed-before-the-call`, `revoked-microsoft-connection-parks-the-call-and-tells-the-member`, `skill-reads-microsoft-graph-as-the-member-without-holding-a-token` | process; microVM lane once at release | none | — |
| Microsoft 365 in use | The `m365-mail` skill, the Executive assistant starter agent, and the morning triage playbook on `vam.schedule`. | connectors Phase 4; `executive-assistant-triages-sends-and-books-with-approval`, `morning-triage-runs-on-schedule-as-the-member-who-approved-it`, `scheduled-triage-parks-on-a-revoked-connection-and-resumes-once` | process on GitHub runners only | none | ****: in cloud sessions the run says "Sent." while the Graph mock saw 0 requests |
| Git marketplace | Trusted marketplace URLs per node; listing import at a tag; "Unverified" badge; "update available"; community connectors (remote MCP only) as listings. |; `listing-from-an-added-marketplace-installs-as-unverified-within-its-limits`, `community-connector-appears-unverified-and-connects` | process, **fixture repo only** | none | real repo has no tag; deploy key unconfirmed; Microsoft publisher verification `[ ]` |

### 2.11 CLI (`vam`)

Commands that exist on `main`:

| Group | Commands |
|---|---|
| Setup and identity | `config`, `login`, `logout`, `health`, `users`, `keys` |
| Node | `node` (`status`, `keys`, `notify`) |
| Connectors and credentials | `connectors`, `marketplace`, `grants`, `secrets`, `mcp` |
| Catalog | `agent` (including `stats` and `permissions`), `playbook` (including `init`, `explain`, `test`), `policy`, `skill`, `catalog` (`validate`, `import`) |
| Sessions and runs | `session`, `run` (`start`, `send`, `stop`, `answer`, `events`, `trajectory`, `compare` and more), `approve` |
| Notifications | `notifications` (`show`, `read`, `contacts`, `link`, `unlink`, `prefs`) |
| Receipts and evidence | `evidence`, `receipt` (`show`, `verify`) |
| Workspaces | `workspace`, `space`, `thread` |

- **Proven:** scenarios drive the CLI and API; `crates/vam-cli/tests`.
- **Docs:** `getting-started/cli.md` (no status) claims to be a "complete reference" but has no section for `connectors`, `marketplace`, `mcp`, `grants`, `receipt`, `notifications`, `space` or `thread`.
- **Help text:** `vam node` help says `vam secrets put`; the subcommand is `set`.

### 2.12 HTTP API

About 85 to 110 route paths in `crates/vam-orchestrator/src/api/`.

- **Auth:** bearer tokens, `Idempotency-Key` on writes, and 404 for another user's objects. True to `orchestrator/api.md` (shipped).
- **Rate limits:** match the doc.
- **Paging:** matches, except that an oversize page size is clamped rather than refused.
- **Gaps:**
  - The route-group table in api.md is stale. It omits connectors, marketplaces, mcp, notifications, contacts, spaces, workspaces, hosted, `node/allowed-models` and `node/notify`.
  - `docs/api/openapi.yaml` lacks `/v1/notifications*`, `/v1/me/contacts*`, `/v1/me/notification-prefs`, `/v1/node/notify`, `/v1/agent-checks` and `/v1/playbook-tests`.

### 2.13 App-facing surfaces (consumed by the VAM app)

- **Contracts the app reads:**
  - proto `ToolSpec.ui` (15) and `ToolSpec.egress` (14), and `ApprovalRequestClaims.card` (21);
  - outbox cards;
  - connector states;
  - marketplace and origin fields;
  - `/v1/notifications/summary`;
  - the vendored `spec/jig`, pinned by digest.
- **App PRs named in trackers:** (work outside chat) (notifications) (connectors Phase 1) (Phases 2 and 3) (Marketplace page) (Phase 4). I could not check's merge state from this session.
- **Proof:** app unit tests and team checklists, none recorded as passed. The only UI scenario (`badge-counts-what-waits-on-you`) is pending, for lack of a UI runner.
- **Not built on the app side:** the receipt view (E12 Phase 2) and the agent page numbers (E4 slice).

### 2.14 Benchmark

| Capability | Evidence | Proven | Docs | Issues |
|---|---|---|---|---|
| B0 bench lane: task set 0.1.0 (four short tasks), machine on, k = 5, pass^k, cost per success at list price |; `bench/results/0.1.0.json`, `0.1.0-report.md`. Claude Sonnet 5.5 and Haiku 5.5, 40/40, run `55cb5b`, 0.20 USD. | real (Vercel gateway, microVM in Lima, jailer on) | `runtime/benchmark-results.md`, shipped, honest about what is not measured | one withdrawn run kept in `withdrawn/` |
| Open-weight rows |; GLM-5, Qwen3.6-27B and DeepSeek V4 Pro, all 20/20 (`0.1.0-open-report.md`) | real | in a report, not in the page's table | 0 invalid calls and 0 resamples: these tasks do not separate the models or exercise the levers |
| `bench.yml` on Actions |; dispatch-only, KVM runner, gateway secret | never run (no results on `main`) | — | — |
| Task sets 0.2.0 and 0.3.0 | `e2e/task-sets.yaml` | defined, never scored | — | — |
| **B1**: `machine off`, business tasks (`close-prep`, `invoice-approval`, `runaway`, `hostile-skill`), the on vs off table | `machine off` is refused by the catalog lint (`crates/vam-e2e/src/check.rs`); no tasks or worlds | **not started** | page line 37 implies B1 exists | blocks the table proof and H2 |

### 2.15 Docs

- **31 shipped pages:** truthful, each with `proves:` entries. Exceptions:
  - isolation.md has two stale lines;
  - api.md has a stale route table and secret-kind line;
  - jig.md does not cover skill cards.
- **11 `build/*` pages:** marked planned, though the Vamfile work was cut (`launch-0.1.0.md` §2). They should be `stale` or archived. `build/rootfs.md` describes a script that ships.
- **31 pages with no status:** the landing page, getting-started, security overview, authoring, tools and tutorials. These hold the false claims listed in §3.
- **Shipped features with no page at all:**
  - Slack notifications and operator setup;
  - the schedule broker;
  - Microsoft 365 and Linear connectors;
  - the Git marketplace;
  - spaces and thread ids.
- **Workstream index:** seven rows in `.cursor/workstreams/README.md` are stale:
  - `work-outside-chat` and `notifications` both say Phase 2 is next, but both shipped;
  - `run-receipt`'s Phase 2 is no longer waiting on the E5 slice;
  - `agent-runtime` omits Phase 4;
  - `harness` says H1 is next, but H1 shipped;
  - `connectors` says only Phase 1 is released, but Phases 2 to 4 are too;
  - `e2e-scenarios` lists only Phase 1.

## 3. Where the docs or README claim what is not shipped

High severity means a launch visitor reads it: the README, landing page,
install or security pages.

| File | Claim | Reality on `main` | Severity |
|---|---|---|---|
| `security/overview.md` (no status, in sidebar) | SPIFFE attestation, PII regex and entropy masking, signed HITL tokens, custom seccomp BPF filters through the jailer; `spiffe/ pii/ seccomp/` modules | None exist in `crates/vam-security/src`. `architecture/isolation.md` says only Firecracker's built-in seccomp filter applies. The two pages contradict each other. | high |
| `index.mdx` (landing hero) | "zero-trust egress" | An agent with no `internet` setting resolves to allow (documented in permissions.md). The hero overclaims. | high |
| `README.md` | "an active design and prototyping scaffold"; a Starlight `:::caution` block that renders as text on GitHub; "planned native Rust `vam-build`" | Receipts, connectors, notifications, schedule broker, jailer and TLS defaults all ship. `vam-build` was cut. | high |
| `getting-started/project-status.md` | "repository is currently a scaffold", "code does not exist yet", "`vam-build`: fully runnable", "~115 ms VM-ready", roadmap lists Vamfile and OCI publisher | Out of date on almost every row. cold-start.md (shipped) says about 225 ms session start. Shipped features are absent. Tells readers to put an API key in `.env`. | high |
| `getting-started/quickstart.md`, `installation.md` | `cargo run -p vam-orchestrator` | The crate has two binaries and no `default-run`, so cargo refuses. Needs `--bin vamd`. | high |
| quickstart.md | Sample output `[vam] Booting isolated Firecracker microVM...`, `Run COMPLETED in 14.2s` | Made up. The CLI prints `[vam] run state:` and `step {id}:`. | high |
| installation.md | `vam keys create --role "admin"`; `vam health` prints "Warm Pool: 4 ready microVMs… 0.1.0-alpha" | There is no `--role` flag (`--scope`). `health` prints the raw JSON. There is no warm pool. | high |
| `getting-started/windows-wsl.md` | `./scripts/fetch-guest-kernel.sh`, `./scripts/start-vamd-wsl.sh` | Neither script exists. | high |
| `getting-started/introduction.md` | "sub-millisecond checkpoints… clone, rollback instantly"; "CPU, memory and bandwidth quotas at the hypervisor"; "not a chat UI, no web interfaces" | No checkpoint or clone feature. No Firecracker rate limiter is used. The VAM app ships. | medium |
| `getting-started/platform-tour.md` | gRPC, JWT validation, TAP setup per session; Drive9 over FUSE syncing to cloud stores | The API is HTTP/JSON. TAP is skipped by default. No FUSE (storage overview says so). No cloud backends. | medium |
| `authoring/model.md`, `skill-manifest.md`, `tools/tool-runtimes.md` | `localExec` runs `bun`, `node`, `python3` or `bash` | The guest has busybox and Bun only. Python arrives with row 14. | medium |
| `build/*` (11 pages, planned) | Vamfile, `vam build` | Cut from 0.1.0. Should be `stale`. | medium |
| `runtime/benchmark-results.md` line 37 | "the business tasks in B1 do" | B1 is not started. | low |
| `architecture/isolation.md` | the egress refusal scenario is "pending"; "scenarios named here run on the microVM lane" | It is active. `host-tool-output-fits-frame` runs in process mode. | low |
| `installation.md` | `host = "http://127.0.0.1:8080"` | TLS is the default. | low |

**Root cause.** `e2e/docs-baseline.txt` exempts 31 pages and the README from
the `status` and `proves` lint. The lint works; the baseline hides exactly
the pages a visitor sees first.

## 4. Gap list

Size: S is about one wave or less, M one phase or a few waves, L more than one phase or blocked on outside work.

### 4.1 Must fix before launch

Each item is a broken promise, a security issue, a data-loss risk, or a reason the launch demo cannot run.

| # | Gap | Size | Files or areas | Best lane |
|---|---|---|---|---|
| M1 | **Docs front pages make false claims** (§3, high rows): the security overview, landing hero, README, project status, quickstart, installation and the WSL page. Fix: rewrite or mark `stale`, drop them from `docs-baseline.txt` so the lint covers them, and paste real CLI output. | M | `docs/src/content/docs/{index.mdx,security/overview.md,getting-started/*}`, `README.md`, `e2e/docs-baseline.txt` | Truth (isolation-truth Phase 4, the docs pass), or a docs wave after row 14 so the install pages describe `vam node init` |
| M2 | **CI on `main` is red.** `egress-refuses-an-ungranted-host` sees an ALLOW event under full smoke load but passes alone. Until it is understood, an ALLOW on a no-internet agent's run may be a real attribution or isolation bug. Also fix the artifact upload, which fails on `:` in the catalog path and loses every smoke failure brief. | S (artifact) + M (egress) | `.github/workflows/rust.yml` (upload paths); `crates/vam-network/src/broker.rs`, `crates/vam-orchestrator/src/brokers.rs`, `events.rs`; the scenario's `then` only by spec change | Bug-fix lane |
| M3 | **The install** is not on `main`: `vam node init`, self-signed TLS on one host, a base with Python and git, one-host docs. | L (in progress) | rootfs scripts, `vam-cli` node, `vamd` | Row 14 (`feat/machine-base`) |
| M4 | **The demo has never run end to end.** Either write `close-prep` (procedure, world, five effects) or have the owner name morning triage on Microsoft 365 as the launch demo. Then, for the chosen one: one live run with a real model, a real Slack workspace and, for Microsoft 365, a real tenant (connectors Phase 4 wave 9, CN-1.8). Owner items: tag `microsoft-365-v1.0.0` on `squrell-inc/vam-marketplace`, its deploy key, and Microsoft publisher verification (longest lead; needed for other tenants only). | M to L | `examples/`, `e2e/world/`, connectors and notifications team checklists | Product lane (close-prep) or connectors lane (Microsoft 365), plus the owner |
| M5 | **Benchmark table proof.** B1 (`machine off`, the four business tasks, the on vs off table) is not started. Today's table shows five models at 100% on four easy tasks, which proves the lane but not the claim. Either build B1, or have the owner re-scope the proof to B0 and say so on the page. | L (B1), or S (re-scope plus honest page) | `crates/vam-e2e` (mode `off`), `e2e/task-sets.yaml`, `bench/results`, `runtime/benchmark-results.md` | Proof lane (B1 + H2), owner's word for the paid runs |
| M6 | ** touch the receipt promise.**: a run reports "Sent." while the Graph mock saw nothing, in cloud sessions (suspected proxy variables reaching the guest or Bun). If a proxy 2xx can be read as a send, a receipt can claim an effect that did not happen.: under load a sent-or-unknown call is journaled as Sent instead of Unknown. | M each | `crates/vam-orchestrator` effect journal, `crates/vam-e2e/src/connectors.rs`, process-guest env | Bug-fix lane (in progress) |
| M7 | **The honest page** (launch proof 5): what VAM is not for, and where open models still lose. It needs B1 data for the second half. The first half is a short page that can replace the wrong "not a chat UI" section. | S (page) + data from M5 | `docs/src/content/docs/getting-started/introduction.md` or a new page | Docs pass |

### 4.2 Should fix (cheap and visible)

| # | Gap | Size | Files or areas | Best lane |
|---|---|---|---|---|
| S1 | Pages for shipped features: Slack notifications with operator setup, the schedule broker, Microsoft 365 and Linear connectors, the Git marketplace, spaces and thread ids. | M | `docs/src/content/docs/{hosted,orchestrator}/`, `astro.config.mjs` | Docs pass, with each lane's owner reviewing |
| S2 | api.md route table and "Secret kinds" line; OpenAPI notification, contact, node-notify, agent-check and playbook-test routes; the missing cli.md sections; the `vam secrets put` help string. | S | `orchestrator/api.md`, `docs/api/openapi.yaml`, `getting-started/cli.md`, `crates/vam-cli/src/cmd/node.rs` | Docs pass (help string: bug-fix lane) |
| S3 | Re-status `build/*` as `stale` (Vamfile cut) and `build/rootfs.md` as shipped. Fix the two stale isolation.md lines and the benchmark page's B1 line. Limit `localExec` interpreters to what the base really has. | S | `docs/src/content/docs/{build,architecture,runtime,authoring,tools}/` | Docs pass |
| S4 | Make the microVM lane block releases: either a required check on PRs into `main`, or the release gate requires a green lane run for the head commit. Today it is advisory, path-filtered and skippable by label. | S | `.github/workflows/rust.yml`, `scripts/e2e/check-release-gate.sh` | Truth lane |
| S5 | Read `SKILL.md` alongside `SKILLS.md` (a §2 build-now row, never done; community skills use `SKILL.md`). | S | `vam-runtime/src/skills.rs`, `vam-orchestrator` skill admission | Any lane touching skills (connectors) |
| S6 | E4 slice: pass^k and cost per version on the agent page. The API exists (`/v1/agents/{urn}/outcomes`). | S | The VAM app | Product lane (app wave) |
| S7 | Run and record the team checklists on a deployed node: E5 Phases 1 and 2, notifications 1 and 2, E12 Phase 3, connectors 1 to 4. None is recorded as passed. Include one live Slack run on a test workspace. | M (ops) | trackers' checklists | Each lane, owner at the keyboard for Slack and the tenant |
| S8 | Update the seven stale rows in `.cursor/workstreams/README.md` and the trackers' current-phase lines. Add `vam-schedule` and `vam-notify` to AGENTS.md §3. | S | `.cursor/workstreams/`, `AGENTS.md` | Orchestrator |
| S9 | Resolve the `wait_for` conflict: `launch-0.1.0.md` §2 keeps it in 0.1.0, but the E5 tracker defers it. The owner decides; the default is out of 0.1.0, editing the §2 row. | S | `design/launch-0.1.0.md` | Owner, then orchestrator |
| S10 | Open profiles say `evidence: "pending"` though bench rows exist. Update them or explain why they stay pending. Document provider fallback in inference.md. | S | `vam-spec/src/harness.rs`, `runtime/inference.md` | Proof lane |
| S11 | Chat runs have no `startedAt` on the receipt (null `runs.created_at`). | S | `vam-orchestrator` run store | Bug-fix lane |

### 4.3 After launch

| # | Gap | Size | Best lane |
|---|---|---|---|
| A1 | Install from any Git URL with `vam.json` holding the version (§2 SLICE) | M | Product |
| A2 | Org inventory read-only list (§2 SLICE) | S to M | Product |
| A3 | A UI runner so `badge-counts-what-waits-on-you` and app flows can be scenarios | M | E9 scenarios |
| A4 | Scenarios for what is only unit-tested: permission presets and effective view, context caching, `tool_choice`, SSRF guard, redaction, an oversize connector response. Prove or remove the Landlock claim. | M | Truth |
| A5 | `wait_for` on CI, `vam.github`, resume grants (later E5 phase) | L | Product |
| A6 | B2, B3, scoring task sets 0.2.0 and 0.3.0, a first `bench.yml` run on Actions, an OpenAI frontier row | L | Proof |
| A7 | Receipt in the app and in Slack (E12 Phase 2 app view) | M | Product |
| A8 | Tool masking for self-hosted vLLM (§2 SLICE, conditional on B1 showing repairs dominate) | M | Proof |
| A9 | (a long smoke run takes down the cloud container) | S to M | Bug-fix lane (in progress) |

## 5. §2 items marked SLICE or BACKLOG that were never done

| Item (`launch-0.1.0.md` §2) | Bucket | State |
|---|---|---|
| `SKILL.md` read alongside `SKILLS.md` | Build now | not done (S5) |
| E4 slice: pass^k and cost on the agent page | Build now | API done; app page not done (S6) |
| E7 Phase 1 and the install slice | Build now | row 14, in progress (M3) |
| Benchmark B1 | Build now | not started (M5) |
| Permissions presets and effective view | Build now | API and CLI done; no scenario; app side not verified |
| E8 egress Phase 2, guest proxy fail-closed | SLICE (keep) | done via isolation-truth (loopback-only proxy, no route) |
| E10 Phase 5: `wait_for` on CI | SLICE (keep) | not done; deferred by the E5 tracker (S9) |
| E3: org inventory read-only list | SLICE (keep) | not done (A2) |
| E2: `vam.json` version, install from a Git URL | SLICE (keep) | not done (A1); the Git marketplace covers installs from trusted listings |
| E11: basic Jig in E10 Phase 3 | SLICE (keep) | done (built-in cards; skill cards in connectors Phase 3) |
| Constrained decoding on the vLLM path | SLICE (conditional) | not done; its trigger (B1 data) does not exist |
| Origin badges beyond Org, Squrell, Local | BACKLOG | not done (as intended) |
| `vam policy test`, Cedar templates | BACKLOG | not done (`vam policy explain` exists) |
| Partial receipts at each park | BACKLOG | not done (`receipt.rs`: "partial receipts are backlog") |
| A2UI export of built-in jigs | BACKLOG | not done |
| Version-bump suggestion refinements | BACKLOG | not verified; `suggest-version` exists |

## 6. Method and limits

- **What was read:** `main` (the code, `e2e/scenarios/`, `.github/workflows/`, trackers, docs frontmatter, `bench/results/`), plus GitHub Actions runs on `main` and open issues.
- **What was run:** the three checks in §1, and the failing egress scenario three times in process mode.
- **What was not run:**
  - the full smoke set, which says can take down a cloud container;
  - the microVM lane, which needs KVM;
  - any real model or service.
- **Limit:** the clone's history starts at so agent-experience Phases 0 to 7 were checked through their tracker, not through git.
- **Not audited:** row 14 and the bug-fix lane's open branches.
