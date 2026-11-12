---
title: Architecture overview
description: The planes, the crates as built, the path of one run, process mode versus VM mode, and the repository layout.
status: shipped
proves:
  - plain-chat-gets-a-reply
  - crates/vam-orchestrator/tests/process_mode_bin.rs
  - node-reports-the-isolation-it-runs
---

VAM runs agents on one Linux host. A host daemon, `vamd`, owns state and the API. Each session runs
its agent in a guest: a Firecracker microVM on a real node, an ordinary host process on a test node.
The host keeps the model keys, the secrets, the approvals and the receipts. The guest holds only the
agent loop and its tools.

## The crates

The Cargo workspace has 14 members (`Cargo.toml:3-18`). `crates/vam-build/` is a directory with one
file, `SKILLS.md`; it is not a crate, and the [image-building pages](/build/overview/) describe a
planned design.

| Crate | Binary | What it is |
|---|---|---|
| `vam-spec` | | The protobuf contracts in `proto/vam/v1/`, compiled to Rust types |
| `vam-core` | | The `Vmm` trait, the Firecracker and process VMMs, jail staging |
| `vam-firecracker` | | A vendored fork of the Firecracker client and jailer launcher |
| `vam-vsock` | | The vsock frame format (64 KiB payload cap) |
| `vam-orchestrator` | `vamd` | The host daemon: API, state database, sessions, runs, catalog, receipts |
| `vam-runtime` | `vam-runtime` | The guest: PID 1 init, the agent loop, tool execution, the guest egress proxy |
| `vam-security` | | Permissions, the Cedar policy engine, approval signing |
| `vam-network` | | The host egress gate and proxy, secret injection, the per-session CA |
| `vam-storage` | | Per-session workspaces, turn commits, the thread log |
| `vam-inference` | | Model providers, the catalog, usage and cost, the mock model |
| `vam-telemetry` | | Events, the outbox, OTLP, CloudEvents, the audit chain |
| `vam-cli` | `vam` | The command-line client for the HTTP API |
| `vam-e2e` | `vam-e2e` | The scenario runner and the docs lint |
| `brokers/vam-web` | | The `vam.web` hosted broker |

## One run, from the CLI to a receipt

1. `vam run` posts to `POST /v1/runs` (`crates/vam-orchestrator/src/api/mod.rs:178`, handler
   `create_run`, `:1201`).
2. `vamd` starts or reuses a session. In VM mode it restores or boots a microVM
   ([Cold start](/architecture/cold-start/)); in process mode it starts the guest binary. The node
   records the isolation the session started under (`api/mod.rs:3657-3680`).
3. The guest registers over vsock, takes the run and runs the agent loop in `vam-runtime`.
4. Model calls go to the host. The guest never holds a provider key. Tool calls are checked against
   the agent's permissions on the host, and a call that needs an approval parks the run until a
   person decides.
5. A tool that reaches the internet goes through the guest proxy to the host proxy, which applies the
   agent's internet rules and substitutes secrets.
6. Events are recorded as the run goes. At the end `vamd` writes a signed receipt that names the
   isolation the run had ([Run receipt](/security/run-receipt/)).

What keeps step 3 and step 5 apart from the host is on [Isolation](/architecture/isolation/).

## Process mode and VM mode

`vam_orchestrator::guest_mode()` (`crates/vam-orchestrator/src/lib.rs:91`) returns `"process"` only
when the binary was built with the `process-guest` feature and process mode is active. Every other
build returns `"vm"`.

| | VM mode | Process mode |
|---|---|---|
| Built as | the default `vamd` | `--features process-guest`, a test build |
| Guest | a Firecracker microVM | a host process |
| Isolation line | `firecracker microVM, jailer on` | `process guest (no isolation)` |
| Model | any configured provider | the mock model only |
| Needs | KVM, root for the jailer, Firecracker, kernel and rootfs | nothing special |

The scenarios run on a process-mode node, except the ten that need a microVM
(`scripts/e2e/run.sh --smoke` and `--firecracker`; see [Testing](/architecture/testing/)). A VM node
that lacks `VAMD_FIRECRACKER_BIN`, `VAM_ROOTFS_PATH`, `VAM_KERNEL_PATH` or (with the jailer on)
`VAMD_JAILER_BIN` refuses to start and names each one
(`crates/vam-orchestrator/src/daemon.rs::vm_node_names_every_missing_variable`).

Process mode must never ship. `scripts/e2e/check-release-gate.sh` builds the default `vamd` and
guest, fails if either contains process mode, and fails if the default dependency tree enables
`process-guest` or `uds-transport`. Its last lines on this base:

```text
$ scripts/e2e/check-release-gate.sh
…
check-guest-build: PASS
ok:   the default workspace dependency tree enables neither feature
release-gate: PASS
```

## Names that matter

- The secrets master key is read from `VAMD_SECRETS_MASTER_KEY` (`crates/vam-orchestrator/src/secrets.rs:17`).
  The value is 64 hex characters. If the variable is not set, the node reads `secrets.key` beside the
  state database, or creates it there with a random 32-byte key and mode 0600
  (`load_or_create_master_key`, `secrets.rs:1149-1190`).
- A skill package is described by `SKILLS.md`. The code reads that name and no other
  (`crates/vam-orchestrator/src/skill_package_admit.rs:233-238`).
- An agent with no `internet` setting is allowed to reach the internet.
  See [Isolation](/architecture/isolation/#what-is-not-proved).

## Repository layout

| Path | Holds |
|---|---|
| `crates/`, `brokers/` | the Rust workspace |
| `proto/vam/v1/` | the contracts |
| `e2e/` | scenarios (`e2e/scenarios/`), world pieces, the docs register and baseline |
| `scripts/e2e/` | the runner, the release gate, the lane's asset script |
| `examples/` | worked agents, skills and brokers |
| `docs/` | this site (Astro Starlight) |
| `design/` | design proposals for open or in-progress work |
| `third_party/` | `ext4-view`, vendored with one patch |

## Not built

- `vam-build`, the image builder: there is no crate.
- Cloud storage backends and a FUSE frontend for workspaces: the code has the local backend only.
