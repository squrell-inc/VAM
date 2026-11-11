> Moved from the docs site on 2026-10-30: design intent, not built (brokered sessions use no TAP, NAT, DNS filter or traffic control).

---
title: nftables NAT & Isolation
description: Host-level routing, conntrack mapping, forwarding tables, and inter-VM isolation rules.
---

:::note[Status: Implemented]
VAM host-side networking and security mechanisms are fully implemented, verified, and integrated within the `vam-network` and `vam-orchestrator` crates.
:::

VAM relies on **`nftables`** (the modern Linux kernel packet classification framework) to handle stateful connections, Network Address Translation (NAT), port forwarding, and strict inter-VM isolation rules.

---

## 1. Dedicated `nftables` Table Architecture

VAM enforces a clean separation of concerns on the host network. Rather than flushing or corrupting host-global firewall configurations, `vam-network` operates strictly within a dedicated, isolated nftables table.

```text
 ┌────────────────────────────────────────────────────────┐
 │            nftables table inet vam_firewall            │
 ├────────────────────────────────────────────────────────┤
 │                                                        │
 │  ┌──────────────────────────────────────────────────┐  │
 │  │ chain input { type filter hook input ... }       │  │
 │  │ - Drop direct incoming packets to host          │  │
 │  └──────────────────────────────────────────────────┘  │
 │                                                        │
 │  ┌──────────────────────────────────────────────────┐  │
 │  │ chain forward { type filter hook forward ... }   │  │
 │  │ - Deny inter-VM cross-session scanning (default)  │  │
 │  │ - Forward only to whitelisted internet routers   │  │
 │  └──────────────────────────────────────────────────┘  │
 │                                                        │
 │  ┌──────────────────────────────────────────────────┐  │
 │  │ chain postrouting { type nat hook postrouting }  │  │
 │  │ - MASQUERADE private guest IPs (172.20.0.0/16)    │  │
 │  └──────────────────────────────────────────────────┘  │
 └────────────────────────────────────────────────────────┘
```

### Core Chains & Hooks
* **`input`**: Intercepts packets directed to the physical host itself. VAM drops all un-brokered network connections coming from guest namespaces to protect host daemon services.
* **`forward`**: Evaluates packets transiting across network namespaces. By default, forwarding is set to a strict **deny** policy.
* **`postrouting`**: Applies NAT (MASQUERADE) to outgoing packets, mapping the private `/30` guest IPs to the host's primary physical network interface (e.g., `eth0`) for internet transit.

---

## 2. Stateful Connection Tracking (`conntrack`)

nftables integrates deeply with the Linux kernel **Connection Tracking (`conntrack`)** engine. VAM leverages this stateful check to improve performance:
* **The Rule**: `ct state established,related accept` is placed at the top of every forwarding chain.
* **The Result**: Once a packet survives the initial DNS evaluation barrier and establishes a valid TCP handshake, subsequent packets belonging to that exact same connection bypass the slow filtering rules entirely, delivering near-native throughput speeds.

---

## 3. Strict Inter-VM Tenant Isolation

To enforce multi-tenant isolation, VAM completely forbids guest-to-guest network visibility by default.

```text
               GUEST VM A (Session A)
                         │
                         ▼ [ Egress Packets ]
 ┌────────────────────────────────────────────────────────┐
 │           Host Routing Table & Forward Chain           │
 ├────────────────────────────────────────────────────────┤
 │ Is Destination IP inside `172.20.0.0/16`?              │
 │       ├─────────────────────────┐                      │
 │       │ (YES)                   │ (NO)                 │
 │       ▼                         ▼                      │
 │ Are Sessions authorized?  Forward packet to Gateway    │
 │       ├───────────┐             with MASQUERADE NAT.   │
 │       │ (YES)     │ (NO)                               │
 │       ▼           ▼                                    │
 │    Forward     DROP PACKET!                            │
 │    Allowed.    Emit Security Event.                    │
 └───────┬───────────┬────────────────────────────────────┘
         │           │
         ▼           ▼
     GUEST VM B     [ Blackhole / Drop ]
    (Session B)
```

:::warning[Inter-VM Isolation Rule]
Under no circumstances may Guest VM $A$ establish direct network connections to Guest VM $B$ unless a bilateral **Agent-to-Agent (A2A) lease** has been explicitly authorized via Cedar. In Alpha, all inter-VM IP forwarding is **blackholed** inside nftables to prevent tenant discovery scans.
:::

---

## 4. Port Forwarding (DNAT) Safety

VAM enforces a strict **No Inbound Connections** policy by default.
* **The Policy**: Guest microVMs are entirely hidden from the outside world. No external internet user can ping or connect to a service running inside the guest.
* **Opt-In DNAT**: If an active playbook explicitly requires exposing an in-VM service (e.g., exposing a web app built by an agent for testing), `vam-network` configures a precise Destination NAT (DNAT) rule mapping a randomized high-port on the host to the guest's port. This rule is automatically torn down the instant the session is terminated.

---

## Next Steps

- Explore how policy rules are Compiled in [Network Policy Compilation](/network/policy/).
- Understand how namespaces are setup in [TAP Interface Allocation](/network/tap/).
- Learn about the complete setup and teardown sequence in [Network Lifecycle](/network/lifecycle/).
