---
title: Guest runtime
description: "What runs inside the guest: PID 1 init, the port-52 server, the agent loop, tool execution and the guest egress proxy."
status: shipped
proves:
  - guest-proxy-never-binds-wildcard
  - vm-boots-and-registers-over-vsock
  - crates/vam-runtime/tests/process_guest.rs
---

`vam-runtime` is the program that runs inside the guest. In a microVM it is PID 1. In process mode the
same code runs as a host process (the `process-guest` feature), so scenarios can run without a VM.
The host side is in [Host daemon](/orchestrator/overview/) and [Isolation](/architecture/isolation/).

## What the guest does

| Step | Where | What happens |
|---|---|---|
| Mounts | `src/bin/guest_init/pid1.rs:92` | `/proc`, `/sys`, `/dev`, `/dev/pts`, tmpfs on `/tmp`, `/work` and `/home/guest`; the data drive `/dev/vdb` on `/mnt/drive9` with `workspace`, `tools`, `skills` and `checkpoints` under it. The snapshot-baking VM skips the data drives. |
| Bind | `src/bin/vam-runtime.rs:271` | Binds vsock port 52 before it registers with the host. |
| Register | `src/bin/vam-runtime.rs:296` | Dials the host (CID 2, port 52) up to 20 times with a growing back-off; halts if none succeeds. |
| Egress | `src/bin/vam-runtime.rs:85` | Dials the host on port 54 and starts the guest proxy (below). |
| Serve | `src/bin/vam-runtime.rs:442` | Accepts host connections and answers `Ping`, `ExecuteTool`, `ExecuteTask`, `CancelTask`, `Shutdown` and `ResumeFromSnapshot`. |

`ExecuteTask` carries a lease. The guest refuses a request with no lease, a stale lifecycle
generation, an expired lease or a lease it has already seen (`RuntimeError`, `src/lib.rs:66`). A prompt
task runs the agent loop ([Agent loop](/runtime/agent/)). Before a shutdown reply the guest flushes
its disks, because the host stops the VM right after (`src/bin/vam-runtime.rs:386`).

## Ports

| Port | Direction | Used for |
|---|---|---|
| 52 | host to guest, and guest to host | control: tasks, tool calls, policy checks (`src/pep.rs:48`), checkpoints, logs |
| 53 | guest to host | inference through the host broker (`src/agent.rs:82`) |
| 54 | guest to host | the egress broker behind the guest proxy |

## The guest egress proxy

`start_guest_proxy` (`src/egress.rs:419`) listens on `127.0.0.1:3128` and on the Unix socket
`/tmp/vam-egress.sock` (`src/egress.rs:333,335`) and forwards over vsock to the host. A preferred
address that is not loopback (`0.0.0.0`, `::`, a host address) is refused. If the port is taken it falls
back to a free loopback port. Proved by `guest-proxy-never-binds-wildcard` and the unit tests in
`src/egress.rs`. What the host does with the traffic is in [Network](/network/overview/).

## Errors and invariants

- A panic in the guest is logged and the guest halts (`src/bin/vam-runtime.rs:215`); `async_main`
  returning is also a halt.
- Lease errors are the `RuntimeError` variants above. A tool or task failure becomes a typed result.
- The guest runs tool processes as uid 1000. How far that confines them is not proved; see
  [Tools](/runtime/tools/).

## Proved by

| Claim | Proof |
|---|---|
| The VM boots and registers over port 52 | `vm-boots-and-registers-over-vsock` (lane) |
| The guest proxy binds loopback only | `guest-proxy-never-binds-wildcard` (lane) |
| The guest program registers, answers RPCs and exits on shutdown in process mode | `crates/vam-runtime/tests/process_guest.rs` (built with the `process-guest` feature) |

## Not built

- Mounting the workspace through FUSE (`design/fuse.md`): the data drive is a plain ext4 image.
- A kernel egress filter in the guest: egress control is the proxy and the missing route, see
  [Network](/network/overview/).
