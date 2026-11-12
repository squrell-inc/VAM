---
title: Isolation
description: What separates a session from the host, what each layer is proved by, and what is not isolated.
status: shipped
proves:
  - vm-boots-and-registers-over-vsock
  - jailer-confines-the-vmm-process
  - node-reports-the-isolation-it-runs
  - receipt-names-the-isolation-the-run-had
  - guest-has-no-route-off-the-vm
  - guest-proxy-never-binds-wildcard
  - host-tool-output-fits-frame
  - vault-secret-is-not-in-the-guest
  - crates/vam-core/tests/jailed.rs
  - crates/vam-orchestrator/tests/adversarial_vsock_tests.rs
  - crates/vam-vsock/src/frame.rs::test_frame_oversized_payload
  - crates/vam-security/src/permissions.rs::j1_defaults_allow_read_only_ask_mutable_external_and_open_internet
---

This page says what keeps a session away from the host. Each layer names the scenario or test that
shows it. A layer with no proof is listed under [What is not proved](#what-is-not-proved).

Scenarios named here run on the microVM lane (`scripts/e2e/run.sh --firecracker`, needs `/dev/kvm`
and root). The lane is a separate CI job. Every scenario named here has `status: active` in `e2e/scenarios/`.

## The layers

| Layer | What it is | Where in code | Proved by |
|---|---|---|---|
| MicroVM per session | One Firecracker microVM per session. The guest registers with the host over vsock before it gets work. | `crates/vam-core/src/vmm.rs` | `vm-boots-and-registers-over-vsock` |
| Jailer | Firecracker runs under the jailer: chrooted, as uid and gid 65400, in its own PID namespace, under a seccomp filter (Firecracker's built-in one). | uid/gid `jail.rs:23-24`, chroot base `jail.rs:25`, cgroup v2 `jail.rs:26`, own PID namespace `vmm.rs:555` | `jailer-confines-the-vmm-process`, `crates/vam-core/tests/jailed.rs` |
| Control channel | The host and guest talk over a vsock stream. A frame carries at most 64 KiB. | `crates/vam-vsock/src/frame.rs:6` | `crates/vam-vsock/src/frame.rs::test_frame_oversized_payload`, `host-tool-output-fits-frame` |
| Tool authorization | A guest cannot run a host tool by forging or replaying a capability token. | `crates/vam-orchestrator/src/host_tools.rs` | `crates/vam-orchestrator/tests/adversarial_vsock_tests.rs` |
| No route off the VM | The guest's routing tables have no default route. Only the host proxy reaches out. | `crates/vam-network/src/` | `guest-has-no-route-off-the-vm` |
| Loopback guest proxy | The proxy in the guest listens on `127.0.0.1:3128` and never on a wildcard address. | `crates/vam-runtime/src/egress.rs:333`, bind path `:437-484` | `guest-proxy-never-binds-wildcard` |
| Secrets stay on the host | A vault secret is substituted at the host proxy. It is not in the guest's environment, boot arguments or workspace. | `crates/vam-network/src/proxy/mitm.rs` | `vault-secret-is-not-in-the-guest` |
| The isolation line | The node and the run receipt report what the node ran with. The line is built from the running config, never a constant. | `crates/vam-orchestrator/src/isolation.rs:55-68` | `node-reports-the-isolation-it-runs`, `receipt-names-the-isolation-the-run-had` |

### The jailer

The jailer is on unless `VAMD_USE_JAILER` is `0`, `false`, `off` or `no`, or `--no-jailer` is passed
(`crates/vam-orchestrator/src/daemon.rs:112-131`). Any other value keeps it on, so a typo does not
switch the confinement off. A node with the jailer off reports `firecracker microVM, jailer off`.

`jail.rs` hard-links the kernel, root filesystem and per-VM drives into the jail. A shared file
(kernel, rootfs) must not be writable by the VMM. A per-VM file is chowned to the jail uid.
`vmm.rs` does not pass `--no-seccomp` to Firecracker anywhere, so Firecracker's own filter applies;
the scenario reads the filter state from the live process.

### The control channel frame

A frame is an 11-byte header (magic `VA`, one type byte, a 4-byte big-endian payload length, a 4-byte
correlation id) and the payload (`crates/vam-vsock/src/frame.rs:5-6,47-71`). The reader rejects a
wrong magic, an unknown type and a length over 64 KiB before it allocates the payload; the writer
refuses to send one. Larger data is split into chunks (`frame.rs:218`). The frame types are stdin,
stdout, stderr, control gRPC, storage, egress and inference streams.

A host tool's reply is held to 60 KiB (`HOST_TOOL_FRAME_BUDGET_BYTES`, `host_tools.rs:36`) so it
always fits one frame. A 200 KB reply is cut with a truncation note and the run carries on
(`host-tool-output-fits-frame`).

### The isolation line

`describe` (`isolation.rs:55-68`) returns one of three summaries:

| Node | Summary |
|---|---|
| microVM, jailer on | `firecracker microVM, jailer on` |
| microVM, jailer off | `firecracker microVM, jailer off` |
| process mode | `process guest (no isolation)` |

The same function feeds `GET /v1/node` (`api/mod.rs:474`). When a run's session starts, the node records
the line in the `run_isolation` table (`api/mod.rs:3657-3680`, `run_facts.rs:190`), and the receipt reads
it from there. Unit tests in `isolation.rs` show that process mode reports no isolation whatever the jailer flag says.

## Process mode isolates nothing

A `vamd` built with the `process-guest` feature runs each guest as an ordinary host process
(`crates/vam-orchestrator/src/process_mode.rs:1-20`). It is a test build. It starts only with the
mock model, loopback listeners, no provider-key import and no jailer. The release gate fails the
build if a default `vamd` or guest contains it (`scripts/e2e/check-release-gate.sh`). See
[Architecture overview](/architecture/overview/#process-mode-and-vm-mode).

## What is not proved

- **Tool confinement.** In the guest, a tool process gets a Landlock filesystem policy and then
  drops to uid 1000, gid 100 (`crates/vam-runtime/src/exec.rs:634,700,711`). If the kernel lacks
  Landlock the policy is skipped and the tool still runs. No scenario checks that a tool cannot
  read or write outside its roots. Treat tool confinement as not proved.
- **Egress to an ungranted host.** The scenario `egress-refuses-an-ungranted-host` is `pending`. This
  page does not cite it. The host proxy and gate exist (`crates/vam-network/src/gate.rs`) and have
  unit tests, but there is no passing end-to-end scenario for a refusal yet.
- **An agent with no `internet` setting.** It resolves to **allow**, not deny
  (`crates/vam-security/src/permissions.rs:277-279`: `HostDefault::Allow | HostDefault::Unspecified`
  both give `"allow"`). The default preset test is
  `j1_defaults_allow_read_only_ask_mutable_external_and_open_internet`. To close the internet for an
  agent, set its default to deny and list the hosts it may reach.

## What is not built

- **No eBPF.** There is no kernel-level egress filter. The only network controls are the absent
  guest route, the guest proxy and the host proxy and gate.

## Design notes

- Egress is brokered, not filtered. A session's guest has no network route; the host proxy makes every
  connection and applies the gate. With no packet path from the guest to the network, there is nothing for a kernel
  filter to inspect, and the claim "the guest cannot reach the network" is stronger than "a filter blocks some packets".
- A kernel filter was removed for that reason, along with its stub and doc claims.
- It would be worth reopening if a network mode with a direct guest route appears, if the userspace proxy
  becomes the bottleneck at many VMs per host, or if kernel-enforced egress is required as an attestable control.
  The first step then would be nftables with an address set filled by the DNS proxy, then eBPF for scale.

## Related

- [Architecture overview](/architecture/overview/): planes, crates, one run end to end.
- [Cold start](/architecture/cold-start/): golden snapshots and the jail.
- [Testing](/architecture/testing/): how these scenarios run.
- [Run receipt](/security/run-receipt/): the isolation line a run carries.
