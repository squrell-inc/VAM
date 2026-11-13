# AGENTS.md — VAM (Virtual Agentic Machine)

> **Canonical System Guide & Agent Operating Manual**  
> Read this file at the start of every session. It provides the authoritative architectural mental model, crate directory map, operational rules, and development workflows for AI agents and agentic IDEs working in this repository.

---

## 1. What is VAM?

**VAM** (Virtual Agentic Machine) is a Rust workspace for designing, orchestrating, and executing autonomous AI agents inside secure, hardware-isolated **Firecracker microVMs**.

Instead of executing untrusted AI agent code or generated scripts in shared container spaces, VAM boots microVMs with host-brokered services (Inference, Networking, Storage, Secrets) bridged across `AF_VSOCK`.

```
+-------------------------------------------------------------------------------+
| HOST SYSTEM (Trusted)                                                         |
|                                                                               |
|  +-------------------------------------------------------------------------+  |
|  | vamd / vam-orchestrator (Reconciliation Engine & Daemon)                |  |
|  +--------------------+-------------------+--------------------+-----------+  |
|                       |                   |                    |              |
|              +--------v-------+  +--------v-------+  +---------v---------+    |
|              | vam-firecracker|  |  vam-inference |  |  vam-network      |    |
|              | (VMM / Jailer) |  | (LLM Broker)   |  | (TAP / DNS proxy) |    |
|              +--------+-------+  +--------+-------+  +---------+---------+    |
|                       |                   |                    |              |
|                       |          +--------v--------------------+---------+    |
|                       |          | AF_VSOCK Host Broker (vam-vsock)      |    |
|                       +----+     +-------------------+-------------------+    |
|                            |                         |                        |
+----------------------------|-------------------------|------------------------+
                             | Virtual Hardware        | AF_VSOCK Multiplexing
+----------------------------v-------------------------v------------------------+
| GUEST MICROVM (Untrusted Agent Execution Sandbox)                             |
|                                                                               |
|  +-------------------------------------------------------------------------+  |
|  | /init (vam-runtime, PID 1)                                              |  |
|  | - Mounts pseudo-filesystems (/proc, /sys, /dev)                         |  |
|  | - Configures loopback & guest network                                   |  |
|  | - Connects to Host Broker over AF_VSOCK                                 |  |
|  | - Spawns & supervises untrusted agent workloads                         |  |
|  +-------------------------------------------------------------------------+  |
|                                                                               |
|  Untrusted Agent Workload (Claude, AutoGPT, Bun, Python, Shell commands)      |
+-------------------------------------------------------------------------------+
```

---

## 2. Workspace & Repository Layout

```
vam/
├── AGENTS.md                  # ← Canonical agent operating manual
├── README.md                  # Human onboarding & project overview
├── Cargo.toml                 # Workspace root manifest
├── rust-toolchain.toml         # Stable Rust channel definition
├── rootfs.ext4                # Guest microVM ext4 image, built locally by scripts/build-rootfs*.sh (git-ignored, not in the repo)
├── .cursor/rules/             # Agentic IDE scoped rules (*.mdc)
├── .cursor/workstreams/       # Phase trackers for active work (see section 2.1)
├── .cursor/skills/            # Agent procedures (phased-workstream)
├── design/                    # Design plans of record (see design/README.md)
├── proto/                     # Protocol Buffers / wire formats (vam/v1)
├── scripts/                   # Rootfs builds, Lima VM scripts, benchmarks
├── examples/                  # Example agent playbooks & manifests
├── crates/                    # 13 workspace crates (plus vam-build, not a member): the machine (see section 3)
├── brokers/                   # First-party HSP brokers, one crate each, own process (see section 3)
└── docs/                      # Astro Starlight documentation site (@vam/docs)
    └── src/content/docs/      # Docs pages: what the node does today
```

---

## 2.1 Where Active Work Is Tracked

**Read this before starting any implementation task.** Work in this repository
flows `design doc → phase tracker → plan → build`.

| Location | Contains | When to read |
|---|---|---|
| [`.cursor/workstreams/README.md`](.cursor/workstreams/README.md) | Index of every workstream with status and current phase | **First**, to find the relevant tracker |
| `.cursor/workstreams/<slug>.md` | Goal, out-of-scope, **locked decisions**, phases with checkboxes and exit criteria | **Every session** working on that slug |
| [`design/README.md`](design/README.md) | Index of design plans of record | When you need the rationale |
| `design/<slug>.md` | The reasoning: evidence, file/line citations, tradeoffs, rejected options | Once for context — read **only the section** for the phase in hand, these files are large |
| `.cursor/skills/phased-workstream/SKILL.md` | The procedure for running a workstream | When opening or advancing a workstream |
| [`design/scenarios.md`](design/scenarios.md), [`e2e/README.md`](e2e/README.md) | How phases are planned and proven test-first with scenarios | Before planning or building any phase |

Rules for agents:

- **Never re-litigate a "Locked decisions" entry** in a tracker. If it must
  change, say so explicitly and edit that list first.
- **One phase at a time.** Do not plan or implement an entire workstream in one
  pass. Respect each phase's `Depends on`.
- **Plan and build every phase test-first through scenarios.** The phase
  breakdown starts with a Scenario delta; wave 0 writes the new and changed
  scenarios (`e2e/scenarios/`) as `pending`, failing for the right reason;
  build waves turn named scenarios green and never edit a scenario's `then` to
  pass. Run them with `scripts/e2e/run.sh` (no KVM needed). Procedure:
  `phase-playbook.md` §3; rationale: `design/scenarios.md`; how-to:
  `e2e/README.md`.
- **Tick the checkbox and add a Progress log row** (with the scoreboard:
  scenarios green/red, fix cycles, wall time) when work lands.
- Do not create planning or tracking markdown in the repo root or in the
  Starlight docs site. Trackers live in `.cursor/workstreams/`, design plans in
  `design/`.

---

## 3. Crate Catalog & System Responsibilities

Every crate in `crates/` and `brokers/` is a workspace member governed by root `Cargo.toml`.
`crates/` holds the machine. `brokers/` holds first-party HSP brokers: each is its
own process, talks to `vamd` only over HSP, and depends on `vam-spec` and shared
libraries, never on `vam-orchestrator` (locked decision 22 in
`.cursor/workstreams/agent-runtime.md`).

| Crate | Type | Primary Role & Invariants |
|---|---|---|
| [`crates/vam-spec`](crates/vam-spec) | Library | **Core Data Contracts & Schemas**: Session manifests, agent specifications, state machine enums, error models, and wire-format serializations (`serde`). |
| [`crates/vam-core`](crates/vam-core) | Library | **VMM Supervisor & CID Management**: Context IDs allocation, Firecracker lifecycle states, process supervisor abstractions. |
| [`crates/vam-firecracker`](crates/vam-firecracker) | Library | **Hypervisor Integration**: Firecracker REST API client, Jailer process execution, drive attachment, boot configuration. |
| [`crates/vam-vsock`](crates/vam-vsock) | Library | **Host-Guest Protocol**: Bidirectional multiplexed communication over `AF_VSOCK` (port 52) between host and guest. |
| [`crates/vam-runtime`](crates/vam-runtime) | Binary / Lib | **Guest PID 1 Init Process**: Compiled to static musl (`x86_64-unknown-linux-musl`), boots guest, sets up cgroups/mounts, communicates with host via vsock, executes agent workloads. |
| [`crates/vam-storage`](crates/vam-storage) | Library | **Storage Engine & Drive9 Caching**: Ephemeral overlayfs, golden base snapshots, ext4 loopback devices, volume lifecycle. |
| [`crates/vam-network`](crates/vam-network) | Library | **Host Network & Egress**: TAP interfaces, IPAM, DNS proxy, strict security capability rules. |
| [`crates/vam-inference`](crates/vam-inference) | Library | **Host-Side Inference Broker**: LLM gateway (OpenAI, Anthropic, Ollama, etc.) with rate-limiting and token budgets. Guest requests completions over vsock without guest ever holding API keys. |
| [`crates/vam-security`](crates/vam-security) | Library | **Security & Policy Enforcement**: Ed25519 signatures, skill package verification, sandbox admission controllers, secret proxying. Shared SSRF guard (`ssrf_guard`) behind the non-default `ssrf-guard` feature, so the static guest build pulls no `reqwest`. |
| [`crates/vam-orchestrator`](crates/vam-orchestrator) | Daemon / Lib | **Node Daemon (`vamd`)**: Session reconciliation engine, state persistence, HITL (human-in-the-loop) approvals, event bus. |
| [`crates/vam-cli`](crates/vam-cli) | CLI Binary | **Developer CLI (`vam`)**: Commands for starting, stopping, inspecting sessions, managing rootfs images, and interacting with `vamd`. |
| [`crates/vam-e2e`](crates/vam-e2e) | Library / Binary | **E2E Scenario Catalog**: typed scenario and world-piece formats, `vam-e2e check` (catalog lint, runs nothing) and the runner seams. Never depends on `vam-orchestrator`, `vam-core` or `vam-runtime`. |
| [`crates/vam-build`](crates/vam-build) | Design/Lib | *(Planned/In-Design)* RootFS image build pipeline & OCI-to-ext4 converter. See `crates/vam-build/SKILLS.md`. |
| [`crates/vam-telemetry`](crates/vam-telemetry) | Library | **Host-Centric Observability**: Correlation spine, W3C traceparent/baggage validators, semantic conventions, zero-I/O pre-spool redaction. Docs: `docs/src/content/docs/telemetry/overview.md`. |
| [`brokers/vam-web`](brokers/vam-web) | Binary / Lib | **Web Broker (`vam-web-broker`)**: HSP broker exposing `web_search` (provider interface, Brave adapter, mock) and `web_fetch` (own guarded fetch: per-hop DNS pinning, robots.txt, HTML to markdown). Search key arrives per invoke from `vamd`, never from env or disk. |
| [`brokers/vam-schedule`](brokers/vam-schedule) | Binary / Lib | **Schedule Broker (`vam.schedule`)**: HSP broker that owns the clock (cron and once, IANA zones, catch-up) and fires runs through the trigger gateway with a run grant the user approved. Docs: `docs/src/content/docs/hosted/schedule.md`. |
| [`brokers/vam-notify`](brokers/vam-notify) | Binary / Lib | **Notify Broker (`vam.notify`)**: HSP broker that delivers notification records to channels (Slack first) and relays decisions back; keeps only delivery facts. Docs: `docs/src/content/docs/hosted/notify.md`. |

---

## 4. Key Architectural & Security Invariants

1. **Zero-Trust Guest Boundary**:
   - The microVM guest is completely untrusted.
   - **Never** expose host filesystem paths, host network interfaces, or raw cloud/LLM API credentials to the guest.
   - All external operations (model inference, network requests, secrets access, storage writes) **must** be mediated by host-side brokers over `AF_VSOCK`.
2. **Error Handling Conventions**:
   - **Library crates** (`vam-spec`, `vam-core`, `vam-storage`, etc.): Use `thiserror` to define explicit, typed error enums. **Never** call `.unwrap()` or `.expect()` in library code.
   - **Binaries & Daemons** (`vam-cli`, `vam-orchestrator` / `vamd`, `vam-runtime`): Use `anyhow::Result` for top-level error propagation and context.
3. **Guest Musl Target**:
   - `vam-runtime` runs inside the microVM and must compile against `x86_64-unknown-linux-musl`.
   - Host builds (`cargo build`) compile host binaries; updating the guest init requires `./scripts/build-rootfs.sh` or `./scripts/build-rootfs-lima.sh`.
4. **No Secrets in Code**:
   - Never hardcode or commit API keys, tokens, or `.env` files. Host-side brokers read credentials from local environment variables or secure keyrings.

---

## 5. Standard Development & Verification Workflows

### Cargo & Rust Checks
```bash
# Check compilation across all workspace members
cargo check --workspace

# Run tests for a specific crate
cargo test -p vam-spec
cargo test -p vam-storage
cargo test -p vam-network
cargo test -p vam-orchestrator

# Run all workspace tests (where hardware virtualization is not required)
cargo test --workspace

# Format and lint
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
```

**CI:** GitHub Actions (`.github/workflows/rust.yml`) runs `cargo fmt --all --check` on pushes to `main` and on pull requests. It also runs a `clippy` job (`cargo clippy --workspace --all-targets -- -D warnings`, default and `process-guest` features, toolchain pinned to 1.97.0 and kept equal to `CLIPPY_TOOLCHAIN` in `scripts/ci/local.sh`; a Rust bump is one PR that changes both and fixes new lints). Wave PRs stay drafts and run no Actions; `scripts/ci/local.sh --full` runs the same clippy gate in the session, and every wave runs it before its PR (`--fast` skips clippy). It also runs the process-mode smoke scenarios (`scripts/e2e/run.sh --smoke`) and, for PRs into `main`, the release gate (`scripts/e2e/check-release-gate.sh`). A docs-only PR skips `rust.yml` and runs `.github/workflows/docs.yml` instead (job `docs`: `cargo run -p vam-e2e -- check`, which lints each page's `status` and `proves` frontmatter, then `pnpm build`). Every docs page needs `status: shipped | planned | stale`; see `e2e/README.md`. Agents and contributors run `cargo check --workspace` locally before opening a PR.

**Optional local hook:** Run `./scripts/install-git-hooks.sh` once per clone to set `core.hooksPath` to `scripts/git-hooks/`; pre-commit then runs the same rustfmt check before each commit.

### Guest RootFS & macOS/Lima Emulation
```bash
# On Linux hosts (requires musl-gcc, docker, e2fsprogs):
./scripts/build-rootfs.sh

# On macOS hosts (via Lima VM with hardware KVM acceleration):
./scripts/build-rootfs-lima.sh
./scripts/start-vamd-lima.sh
```

### Documentation Site
```bash
cd docs
pnpm install
pnpm dev      # Local preview server at http://localhost:4321
pnpm build    # Build static site & validate all routing/links
```

---

## 6. Documentation Map (`docs/src/content/docs/`)

When working on a specific subsystem, refer to the corresponding documentation directory:

| Subsystem / Task | Read first | Then |
|---|---|---|
| Whole system, crate map, process vs VM mode | `docs/src/content/docs/architecture/overview.md` | `docs/src/content/docs/architecture/isolation.md`, `docs/src/content/docs/architecture/testing.md` |
| Isolation, jailer, Firecracker, vsock | `docs/src/content/docs/architecture/isolation.md` | `docs/src/content/docs/core/overview.md`, `docs/src/content/docs/runtime/overview.md` |
| Cold start, golden snapshot | `docs/src/content/docs/architecture/cold-start.md` | `docs/src/content/docs/core/overview.md` |
| `vam-core`, vendored SDK | `docs/src/content/docs/core/overview.md` | `docs/src/content/docs/architecture/isolation.md` |
| `vamd`: startup, config, restart | `docs/src/content/docs/orchestrator/overview.md` |  |
| HTTP API, auth, rate limits, paging | `docs/src/content/docs/orchestrator/api.md` |  |
| Sessions, runs, steer and stop | `docs/src/content/docs/orchestrator/session.md` | `docs/src/content/docs/runtime/inbox-outbox.md`, `docs/src/content/docs/storage/turn-commits.md` |
| Users, keys, ownership | `docs/src/content/docs/orchestrator/user-isolation.md` |  |
| Catalog: agents, skills, playbooks, versions | `docs/src/content/docs/orchestrator/catalog.md` | `docs/src/content/docs/authoring/agent-spec.md` |
| Guest runtime (`vam-runtime`) | `docs/src/content/docs/runtime/overview.md` | `docs/src/content/docs/runtime/agent.md`, `docs/src/content/docs/runtime/tools.md` |
| Agent loop, compaction | `docs/src/content/docs/runtime/agent.md` |  |
| Tools, host tools, MCP | `docs/src/content/docs/runtime/tools.md` |  |
| Inference (`vam-inference`), cost | `docs/src/content/docs/runtime/inference.md` |  |
| Playbooks, gates, approvals | `docs/src/content/docs/runtime/workflow.md` | `docs/src/content/docs/authoring/playbook-spec.md` |
| Permissions, Cedar | `docs/src/content/docs/security/permissions.md` |  |
| Secrets, egress, network | `docs/src/content/docs/security/secrets-proxy.md`, `docs/src/content/docs/network/overview.md` |  |
| Receipts, evidence | `docs/src/content/docs/security/run-receipt.md` |  |
| Storage, workspaces (`vam-storage`) | `docs/src/content/docs/storage/overview.md` | `docs/src/content/docs/storage/turn-commits.md` |
| Telemetry (`vam-telemetry`), audit, events | `docs/src/content/docs/telemetry/overview.md` | `docs/src/content/docs/telemetry/event-catalog.md` |
| Hosted brokers (HSP), `vam.web`, `vam.schedule`, `vam.notify`, triggers | `docs/src/content/docs/hosted/overview.md` | `docs/src/content/docs/hosted/web.md`, `hosted/schedule.md`, `hosted/notify.md`, `hosted/triggered-runs.md` |
| Contracts, proto | `docs/src/content/docs/spec/overview.md` | `proto/vam/v1/` |
| Tests and scenarios | `docs/src/content/docs/architecture/testing.md` | `e2e/README.md` |
| Image building (planned) | `docs/src/content/docs/build/overview.md` | `docs/src/content/docs/build/rootfs.md` |
| Agent / skill authoring | `docs/src/content/docs/authoring/agent-spec.md` | `docs/src/content/docs/authoring/run-spec.md`, `docs/src/content/docs/authoring/skill-manifest.md` |

Files in `design/` are proposals for open work, not behaviour; never cite them as what the node does.

---

## 7. What Agents Must NOT Do

- **Do NOT bypass VSOCK isolation**: Never introduce direct host file access or host socket binds from inside the guest.
- **Do NOT commit `.env` or credential files**: Keep tests hermetic with mock providers.
- **Do NOT create orphan markdown planning files in the repo root**: Phase trackers belong in `.cursor/workstreams/`, design plans of record in `design/`, and user-facing documentation under `docs/src/content/docs/`.
- **Do NOT ignore or silently contradict a tracker's "Locked decisions"**: See section 2.1. Edit the list explicitly if a decision must change.
- **Do NOT break Starlight docs navigation**: If you add, rename, or move pages in `docs/src/content/docs/`, update `docs/astro.config.mjs` sidebar configuration accordingly.
- **Do NOT add non-workspace crates or external monorepo tools** without explicit user instruction.
- **Do NOT add AI attribution to commits or pull requests**: commits are authored by the human maintainer's git identity and carry their `Signed-off-by` (DCO); no `Co-Authored-By` agent trailers, no session links, and no "Generated with" lines in commit messages or PR descriptions. The project discloses AI-assisted development once, in `CONTRIBUTING.md`.
