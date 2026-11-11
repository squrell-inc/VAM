> Moved from the docs site on 2026-10-30: design intent, not built (brokered sessions use no TAP, NAT, DNS filter or traffic control).

---
title: Traffic Control & Bandwidth Shaping
description: Enforcing network QoS, token bucket queueing disciplines, and shaping boundaries.
---

:::note[Status: Implemented]
VAM host-side networking and security mechanisms are fully implemented, verified, and integrated within the `vam-network` and `vam-orchestrator` crates.
:::

To guarantee host platform stability, enforce strict multi-tenant Quality of Service (QoS), and prevent a single rogue guest VM from monopolizing the host's physical network bandwidth, `vam-network` implements host-level **Traffic Control (`tc`) bandwidth shaping**.

---

## 1. Linux Traffic Control (`tc`) Architecture

VAM manages bandwidth constraints by manipulating Linux kernel **Queueing Disciplines (`qdisc`)** attached to the virtual interfaces.

```text
                        GUEST VM (virtio-net NIC)
                                    │
                                    ▼ [ Packet Egress ]
 ┌─────────────────────────────────────────────────────────────────┐
 │     SESSION NAMESPACE: TAP Interface (tap_vam_abc12)            │
 │                                                                 │
 │   ┌──────────────────────┐                                      │
 │   │  Ingress Policing    │ ──► Drops packets exceeding           │
 │   │  (IFB Device / tc)   │     configured burst ceilings        │
 │   └──────────┬───────────┘                                      │
 └──────────────┼──────────────────────────────────────────────────┘
                │
                ▼ [ Namespace Transit ]
 ┌─────────────────────────────────────────────────────────────────┐
 │     HOST ROOT NAMESPACE: Virtual Ethernet interface (veth_host) │
 │                                                                 │
 │   ┌──────────────────────┐                                      │
 │   │  Egress Classing     │ ──► HTB (Hierarchical Token Bucket)  │
 │   │  (tc qdisc HTB)      │     limits overall host-outbound rate│
 │   └──────────┬───────────┘                                      │
 └──────────────┼──────────────────────────────────────────────────┘
                │
                ▼ [ Outbound Gateway ]
         [ Physical NIC ] ──► Internet
```

### Queueing Disciplines Selection
* **Egress (Host-Outbound)**: Managed using a **Hierarchical Token Bucket (`HTB`)** qdisc. HTB provides extremely precise, stateful rate-limiting, allowing operators to set sustained bandwidth limits alongside controlled burst rates.
* **Ingress (Host-Inbound / Guest-Egress)**: Managed using an **Intermediate Functional Block (`IFB`)** device paired with an ingress filter, enabling multi-tenant traffic shaping on ingress queues that do not natively support standard classful qdiscs.

---

## 2. Bandwidth Profiles & Token Buckets

Every network-enabled session is assigned a **Bandwidth Profile** (defined inside the [Network Policy](/network/contracts/)). The shaper translates these profiles into precise token bucket parameters:

* **Sustained Rate (`rate`)**: The maximum long-term bandwidth allowed (e.g. `50 Mbps`).
* **Burst Size (`burst`)**: The maximum amount of bytes that can be transmitted at physical wire speed in a single burst (e.g. `5 MB`). Burst buckets allow rapid web-page loads without sacrificing long-term safety.
* **Packet Rate Limit (`pps`)**: An additional hard filter enforcing maximum packets-per-second to block high-frequency UDP or TCP SYN flood denial-of-service attempts.

---

## 3. Host-Level `tc` vs. Firecracker Device Rate Limits

VAM enforces bandwidth limits at two distinct, independent layers to guarantee maximum platform safety:

| Layer | Enforcement Location | Mechanism | Primary Responsibility |
|---|---|---|---|
| **Tier 1: Device Limits** | Inside VMM Process (`vam-core`) | Firecracker VirtIO Rate Limiters | **Mechanism Guardrail**: Prevents a guest from saturating the virtio ring buffers or crashing the VMM spawner. |
| **Tier 2: Policy Limits** | Host Kernel (`vam-network`) | Linux `tc qdisc` & IFB | **Scheduling Policy**: Enforces Cedar tenant SLA limits, bandwidth shaping, and node-wide resource scheduling. |

By separating these layers, `vam-network` can dynamically adjust tenant bandwidth allocations on the fly without needing to reboot the guest VM or modify Firecracker's static hardware descriptors.

---

## Next Steps

- Explore the performance metrics collected during shaping in [Network Telemetry](/network/telemetry/).
- Read about the complete setup and teardown sequence in [Network Lifecycle](/network/lifecycle/).
- Examine the non-negotiable system requirements in [Network Invariants](/network/invariants/).
