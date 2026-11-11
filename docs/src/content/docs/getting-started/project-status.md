---
title: Project Status
description: Real-time maturity assessment, status matrix, and development rules.
---

:::caution[Alpha Stability Warning]
VAM is an active design and prototyping project in **Alpha**. The repository is currently a scaffold: many of the submodules, traits, and RPC interfaces described throughout these pages represent *planned designs* rather than complete, production-ready Rust code.
:::

To ensure clear expectation management for contributors, users, and security auditors, this page maintains an explicit status matrix of the VAM workspace, separating what is currently runnable from what is planned.

---

## 1. Project Maturity Matrix

| System Component | Current Repository State | Planned Target Design |
|---|---|---|
| **Cargo Workspace** | Runnable `vam-core`, `vam-spec`, `vam-security`, `vam-storage`, `vam-runtime`, and `vam-orchestrator` libraries. | Extend to network, telemetry, and image build crates. |
| **`vam-core`** | Fully runnable. Hardened VMM process supervisor, type-safe configuration builder, vsock CID allocator, state transition tracking, and hardware-gated integration tests. | AWS Jailer production chroot strategy, cgroup v2 boundary validation. |
| **`vam-runtime`** | Fully runnable. Guest PID 1 init binary (`vam-runtime`), Port 52 VSOCK server, unprivileged subprocess execution supervisor with env-scrubbing, **M1 runtime primitives** plus **`load_skill` (M3)**, **M4 host-provided tools** (`host_exec` → `ExecuteHostTool` on Port 52 with token verify + redaction), **M5 guest `.vam/agent/observations.jsonl`** (observational only), **M6 durable work product** (`PLAN.md` / `notes/` convention, host-seeded `PLAN.md` stub, intra-run tool-output compaction to file pointers, host artifact chat = path not body), **M7 host PEP dual-eval** (guest FS jails advisory; host Cedar `ReadFile`/`WriteFile` authoritative on normalized paths), **Drive9-seeded `SYSTEM.md` instruction template (v6)** with Level-1 skills catalog and Cedar-filtered live tool inventory, progressive skill loading from `/mnt/drive9/skills/*/`, and unprivileged Bun tool executor (glibc baseline, from the standard base). The guest agent coordinates ReAct loop over vsock with brokered host-side inference. **M2 honest capability**: guest `ListAdvertisedTools` filter aligns prompt inventory and `InferenceRequest.tools` with host Cedar advertise dry-run; unloaded skill tools stay out of `tools[]` until `load_skill`. Rebuild guest changes via `scripts/build-rootfs.sh`. | Subprocess memory bounds and real Drive9 FUSE mount setup. |
| **`vam-orchestrator`** | Fully runnable. Durable SQLite-backed session/workflow state, startup reconciliation, `vamd` daemon with **HTTP/JSON developer API** (`127.0.0.1:8080`), hashed API keys, **encrypted host secret vault**, **session tool catalog seeding (builtins + skills + host_exec)**, **local skill packages** (`vam skill publish` / draft & release channels, session catalog overlay; examples still seed from `examples/*/skills/` when unpublished), **HostToolAdapter registry** (`echo`/`fail`/`artifact` test adapters), Port 52 `ExecuteHostTool` handler with capability token + replay verification, **M5 host trajectory outbox** (PEP events → digest on volume timeline), **M7 Cedar FS dual-eval** (`examples/*/policy.cedar` are samples on disk, not product catalog) and HITL overlay from `hitlPolicy.monitoredTools`, catalog store, session pause/resume, and async single-task run dispatch. **Cold-start Phases 0–4 shipped:** TAP skip on Isolated/Brokered, Drive9 image templates, Tier 1 golden snapshot bake/restore (~115 ms VM-ready on a dedicated host; ~150 to 250 ms whole session start measured on the CI lane). **User isolation Phases 1–10 shipped:** `owner_user_id` stamps, roles + `vam login`, host-prefixed workspaces, org/user vault, versioned catalog publish, org-admin users/keys/org-secrets HTTP, two-user coverage-map + evidence tests, single-user parity on implicit `local`, `GET /v1/me` identity read, `GET /v1/node` login probe 200 for member / org admin / operator (see [User isolation](/orchestrator/user-isolation/)). | Tier 2 paused standbys, UFFD mem_backend, full playbook DAG scheduler, gRPC/UDS listener. |
| **`vam-cli`** | Runnable HTTP client binary (`vam`): config, health, keys, secrets, catalog, sessions, runs, approvals. | OIDC login, MCP adapter. |
| **`vam-security`** | Runnable Cedar PDP with schema-validated policies, Ed25519 signed-envelope and replay-store primitives, `ReadFile`/`WriteFile` path limbs in `vam.cedarschema`. | SPIFFE/SPIRE, key management, and PII redaction integrations. |
| **`vam-storage`** | Fully runnable. Core Drive9 content-addressed storage (CAS) engine, SHA-256 chunking, Merkle Manifest v1, local-only backends, CoW materialization cache, workspace-scoped **drive9.ext4 templates** (Phase 2), unprivileged ext4 disk generation, vsock FS frames client/server protocol. | FUSE driver (blocked on guest kernel), single-writer cloud leases, and multi-cloud adapters. |
| **`vam-network`** | Fully runnable. Local IPv4/IPv6 subnet IPAM pools with 30s quarantine conntrack-leak prevention, lock-free ArcSwap policy routing container, Zero-Trust secret proxy with post-resolve IP pinning and redirect blocking, guest HTTP egress proxy over vsock streams, Linux iproute2 namespaces, and capabilities-gated graceful fallback. **Phase 3:** default Isolated/Brokered create skips TAP (pool capacity 0). | Production Hickory DNS service. |
| **`vam-telemetry`** | Fully runnable library crate. **Phases 0–3 shipped:** W3C `traceparent`/baggage validators, semconv contracts & low-cardinality label enforcement (Decision T20), zero-I/O pre-spool redactor, `TraceContext` in protobuf RPCs, `vamd` structured `tracing` subscriber, run span tree, metrics registry with broker SLIs, lossy ring buffer / disk-spool queues, tail-sampling policy, `workflow_outbox` broadcast bus, `TelemetryStreamService.StreamEvents` cursor replay & live stream, hash-chained partitioned audit ledger with Ed25519 checkpoints and fail-closed session abort, spool retention manager, trace context propagation across HSP RPCs (`Invoke`, hooks), reference broker instrumentation in Rust (`hsp-echo-broker`) and TypeScript (`hsp-echo-ts`), and trust-tier routing isolation. Phase tracker: `.cursor/workstreams/archive/telemetry.md`. | Guest plane (`StreamLogs` over vsock), hypervisor metrics scraper, signed outcome attestations, SIEM OCSF mapping. Guest never exports OTLP. |
| **`vam-build`** | Fully runnable script-based toolchain. The rootless ext4 assembly script (`scripts/build-rootfs.sh`) builds the standard base `vam/base:2` (Debian trixie-slim, Python 3.13 with requests, pydantic, pandas, numpy, python-dateutil and openpyxl, git, curl, jq, busybox, Bun) and injects `vam-runtime` as `/sbin/init`. `--minimal` keeps the old busybox image for one release. `vam node init` drives it. | Vamfile DSL parser, full native Rust `vam-build` crate, runtime/FUSE injector, OCI/ORAS publisher. |
| **Proto Contracts** | Fully runnable and validated, including `orchestrator.proto` (Vamd HTTP service messages), additive DAG joins, execution leases, HITL bindings, task RPCs, evaluation provenance, and descriptor compatibility checks. | Frozen compatibility baseline for `vam.v1`, extending to generate future multi-language client SDKs. |
| **Orchestrator / HITL** | Tool-intercept pause/decide on single-task runs: CloudEvents outbox, Cedar two-pass Suspend, `GET /v1/approvals`, same-session resume after decide. | Playbook `ApprovalAction`, OpenUI, `GET /v1/artifacts`, VM replacement after long pause. |
| **Docs Site** | Fully runnable, highly comprehensive, and up-to-date architectural specifications. | Continuous development reference that evolves alongside active Rust implementation. |
| **Hosted Services (HSP)** | **Phases 0–6 kernel complete:** proto + codegen, UDS gRPC client/`hsp-echo-broker`, SQLite capability manifests + `HspRegistry` admit gate, `hosted:` catalog merge, `ExecuteHostTool` → `Invoke` (vault inject/redact), lifecycle hooks (`HspHookDispatcher`), and trust-tier ↔ transport / unbound-secret / invoke egress hardening. Design docs under `docs/src/content/docs/hosted/`. | First-party files/RAG brokers, `POST /v1/files`, the VAM app download UI, vsock marketplace connector. |

---

## 2. What Can Be Run Today?

Currently, the workspace supports verification and documentation development tools:

### Rust Validation
You can run standard Cargo check commands and verify code and contract compliance through our test suite:
```bash
cargo check
cargo build
cargo test --all
```
*(Note: Running `cargo test --all` executes active unit and conformance tests for `vam-spec`, ensuring wire compatibility against the frozen descriptor baseline and semantic validations).*

### Live Agent E2E Execution Test
You can run our live ReAct agent integration test inside a real Firecracker VM using unprivileged Bun tools and brokered OpenAI/OpenRouter inference:
```bash
# Set OPENROUTER_API_KEY inside your .env file
VAM_LIVE_INFERENCE=1 cargo test --test e2e_tests test_bun_agent_live_openrouter -- --ignored --nocapture
```

### Astro Starlight Docs Site
The Astro Starlight documentation site is fully operational and serves as the source of truth for the platform's architectural blueprint. To run the documentation site locally:
```bash
cd docs
pnpm install
pnpm dev
```

---

## 3. How to Interpret Code & Design Samples

Throughout this documentation, you will encounter numerous Rust, Protobuf, and JSON code samples detailing the APIs, traits, and configurations of different crates. These must be interpreted under the following rules:

* **Planned Code Markers**: Most code samples are labeled with comments like `// status: Planned` or wrapped in text clarifying their prototyping status. These represent the target API designs that contributors must implement.
* **No Implied Guarantees**: Under no circumstances must any design specification be interpreted as an active guarantee of platform performance, network latency (e.g. "sub-millisecond stream forwarding"), storage durability, SOC2/HIPAA compliance, or public cloud service compatibility. These are design objectives, not verified performance benchmarks.
* **API Invariants**: While the code does not exist yet, the *invariants* listed in each crate's `SKILLS.md` and `invariants.md` pages are strictly binding. Any new code implementation must conform fully to those rules.

---

## 4. Documentation Synchronization Rule

To prevent the architectural specification from drifting away from the physical implementation as development progresses, VAM enforces a **Synchronous Update Rule**:

> **Rule**: Any pull request that modifies, renames, or implements a planned crate, module, or configuration file must update the corresponding Starlight documentation page (`docs/src/content/docs/`), this Project Status page, and the root `README.md` in the same commit.

---

## Next Steps

* [Instruction Template](/runtime/agent/) — Drive9-seeded `SYSTEM.md` rendering (Implemented).
* [Runtime Primitives](/runtime/tools/) — M1 filesystem, search, shell, and todo tools (Implemented).
* To understand how the physical repository is organized on disk, see [Workspace & Documentation](/architecture/overview/).
* Dive into the core architectural principles in [Architecture Overview](/architecture/overview/).
