> Moved from the docs site on 2026-10-31: design intent, not built (capacity barriers and queuing, no code).

---
title: Admission Control & Queuing
description: Capacity calculations, bounded scheduling queues, and tenant fairness policies in VAM.
---

:::caution[Alpha Stability Warning]
VAM orchestration features and subsystems (under the `vam-orchestrator` crate) are currently in **Alpha**. The host daemon process model, scheduling policies, and session routing layers are actively evolving, breaking changes are expected, and no production-readiness guarantees are provided at this stage. Do not deploy in untrusted production environments without custom hardening.
:::

When thousands of agent runs trigger simultaneously on a host, admitting all requests immediately will saturate host CPU caches, fragment RAM namespaces, and degrade existing active workflows. To maintain node stability, `vamd` implements a strict **Admission Control and Queuing** layer.

---

## 1. Capacity Calculations & Barriers (Decision O12)

Before admitting a request to the scheduling queue, `vamd` calculates the active **Available Node Capacity**:

```text
Available RAM = Total Host RAM - Scheduled RAM - Host Headroom (2GB)
Available vCPUs = Total Host Logical Cores - Scheduled cpusets - Host Headroom (2 Cores)
```

If a request's resource specifications (such as vCPUs or memory) exceed the current available capacity, `vamd` blocks direct allocation and places the request into the scheduling queue.

---

## 2. Bounded Scheduling Queues

To prevent memory exhaustion on the host control plane during massive API spikes (such as a swarm of agents launching concurrent subtasks):
* **Queue Bounds**: The scheduling queue is strictly bounded (default **maximum 1,000 pending requests**).
* **Early Rejection**: If a request arrives and the queue is completely saturated, the request is rejected immediately at the API gateway, returning a standard gRPC `RESOURCE_EXHAUSTED` (Overloaded) code to the client before allocating any local file paths, CIDs, or TAP interfaces.

---

## 3. Tenant Fairness and Priority Scheduling

To prevent a single aggressive client or "noisy tenant" from hogging the entire host scheduling queue:
* **Tenant Buckets**: The queue is segmented into tenant-specific buckets using the incoming principal ID (e.g., matching the authenticated user or organization identifier from the SPIFFE certificate).
* **Fair-Share Round-Robin**: When scheduling next slots, `vamd` uses a fair-share round-robin selector. It pulls one request from Tenant $A$, then one from Tenant $B$, preventing Tenant $A$'s 900-request swarm from starving Tenant $B$'s single high-priority command.
* **Priority Escalation**: Playbook steps can carry priority flags (e.g. `HIGH_PRIORITY` for interactive human overrides). The queue prioritizes these requests, executing them ahead of standard background batch workflows.

---

## 4. Next Steps

- Explore how we isolate physical CPU pins for admitted VMs in [Host Resource Scheduler](/orchestrator/scheduler/).
- To see the metrics logged for queue delays, review [Orchestrator Telemetry & SLOs](/orchestrator/telemetry/).
- Read about the complete error mapping profiles in [Orchestrator Errors](/orchestrator/errors/).
