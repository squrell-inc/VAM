> Moved from the docs site on 2026-10-31: design intent, not built (NUMA cpuset scheduling, no code).

---
title: Host Resource Scheduler
description: CPU pinning, NUMA-aware memory allocations, cgroups v2 cpusets, and no-overcommit policies in VAM.
---

:::caution[Alpha Stability Warning]
VAM orchestration features and subsystems (under the `vam-orchestrator` crate) are currently in **Alpha**. The host daemon process model, scheduling policies, and session routing layers are actively evolving, breaking changes are expected, and no production-readiness guarantees are provided at this stage. Do not deploy in untrusted production environments without custom hardening.
:::

In high-concurrency bare-metal deployments, multiple running microVMs can interfere with each other if resource boundaries are not strictly isolated. If three VM instances attempt heavy compilation tasks on the same physical CPU core, cache thrashing and context switching will cause severe performance degradation.

To prevent this, `vamd` implements a **NUMA-Aware Host Resource Scheduler**.

---

## 1. Durable Isolation via cgroups v2 `cpuset` (Decision O11)

While tools like `taskset` can configure CPU affinity for a process on the fly, they are non-hierarchical, easily bypassed, and do not restrict RAM allocation nodes. 

`vamd` utilizes **Linux cgroups v2 `cpuset`** as the absolute authority for resource isolation:

```text
Host: cgroups v2 root (`/sys/fs/cgroup/`)
└── vam/                           # Base VAM resource group
    └── session_abc123/            # Dedicated Session Cgroup
        ├── cpuset.cpus = "0-3"    # Pins guest vCPUs to physical Core 0,1,2,3
        ├── cpuset.mems = "0"      # Restricts RAM allocations to NUMA Node 0
        ├── cpu.max = "200000 100000" # Caps CPU usage to maximum 2 vCPUs
        └── memory.max = "1073741824" # Sets hard RAM limit to 1GB
```

When spawning a guest, the scheduler creates a unique session folder inside the host's unified cgroup tree, writes the Firecracker PID to `cgroup.procs`, and configures the `cpuset` and `memory` controllers before kernel execution begins.

---

## 2. NUMA-Aware Placement

Modern multi-socket host servers segment memory and CPU cores into physical **NUMA (Non-Uniform Memory Access) Nodes**. Accessing memory from Node 0 while executing instructions on CPU Node 1 adds massive hardware-level latency overhead.

The scheduler discovers host architecture at startup (parsing `/sys/devices/system/node/`) and applies the **NUMA Affinity Rule**:
* **Affinity Pairing**: When selecting CPU pins for a microVM session, the scheduler identifies which physical RAM socket is physically closest. It forces `cpuset.mems` and `cpuset.cpus` to target the *same* physical NUMA node.
* **Locality Benefit**: This alignment ensures that guest memory accesses never cross the slow inter-socket QPI/UPI links, preserving sub-millisecond execution speeds.

---

## 3. Strict No-Overcommit Alpha Policy (Decision O12)

Because autonomous agents execute arbitrary, heavy compile and run tasks, resource spikes can easily crash a host node if memory is overcommitted.

In the alpha stage, `vamd` enforces a strict **No-Overcommit Policy**:
* **RAM Reservation**: When a session requests 1GB of memory, `vamd` physically subtracts 1GB from its available host scheduling budget.
* **Host Headroom**: The scheduler reserves a fixed minimum headroom (default **2GB** or **10% of total host RAM**) exclusively for the parent `vamd` daemon and system kernel processes.
* **Fail-Closed Admission**: If a request arrives and scheduled memory allocations exceed the host's capacity (subtracting the headroom), the request is rejected immediately at the gateway, preventing the host kernel from triggering the OOM (Out-of-Memory) Killer.

---

## 4. Next Steps

- Explore how capacity checks are queued before rejection in [Admission Control & Queuing](/orchestrator/admission/).
- Read about how we coordinate snapshot templates and standby VMs in [Warm-Pool Snapshot Manager](/architecture/cold-start/).
- To see the metrics logged for resource pressure, visit [Orchestrator Telemetry & SLOs](/orchestrator/telemetry/).
