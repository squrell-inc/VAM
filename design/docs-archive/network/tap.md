> Moved from the docs site on 2026-10-30: design intent, not built (brokered sessions use no TAP, NAT, DNS filter or traffic control).

---
title: TAP Interface Allocation
description: Provisions virtual TAP interfaces, manages local namespaces, and handles clean teardowns.
---

:::note[Status: Implemented]
VAM host-side networking and security mechanisms are fully implemented, verified, and integrated within the `vam-network` and `vam-orchestrator` crates.
:::

To connect a microVM's virtual ethernet adapter (`virtio-net`) to the host network stack, `vam-network` must dynamically create and configure virtual TAP (Terminal Access Point) interfaces. **TAP allocation applies only to Restricted Direct sessions** — Isolated and Brokered Egress skip TAP/netns setup entirely on the session create hot path (Phase 3).

:::note[Default sessions have no TAP]
Brokered and Isolated sessions do not record a `tap_device` in SQLite, do not call `KernelNetAdapter` TAP/netns methods, and do not join a jailer `--netns`. Firecracker is not configured with `PutGuestNetworkInterfaceByID` for these modes today.
:::

To maintain strict multi-tenant isolation, Restricted Direct allocation follows a rigid process model.

---

## 1. Linux Interface Naming Constraints (`IFNAMSIZ`)

In the Linux kernel, network interface names are strictly limited to **16 bytes** (including the null terminator), meaning a valid interface label can contain at most **15 characters**.

Because VAM's global session IDs are long, unique UUIDs (e.g. `sess_7e2c906a-a82f-4882-938b-d72b1239aa82`), they cannot be used directly as interface names. `vam-network` implements a deterministic hashing convention:

```rust
// status: Implemented
fn generate_tap_name(session_id: &str) -> String {
    use sha2::{Sha256, Digest};
    let mut hasher = Sha256::new();
    hasher.update(session_id.as_bytes());
    let hash = hasher.finalize();
    // Prefix "tap_" (4 chars) + first 10 hex characters of SHA256 = 14 characters
    format!("tap_{:.10}", format!("{:x}", hash))
}
```

This guarantees:
* **Uniqueness**: Prevents name collisions across concurrent active sessions.
* **Kernel Safety**: Fits safely within the 15-character `IFNAMSIZ` boundary.
* **Durable Mapping**: The host database durably stores this mapped TAP name alongside the session ID for reconciliation.

---

## 2. Locally Administered MAC Addresses

Just like interface names, every virtual network card requires a unique Media Access Control (MAC) address. VAM generates **locally administered unicast MAC addresses** (where the second hex digit of the first octet is `2`, `6`, `A`, or `E`) to ensure they do not collide with physical hardware NICs on the host LAN:

```text
       Octet 1      Octet 2      Octet 3      Octet 4      Octet 5      Octet 6
     ┌──────────┐ ┌──────────┐ ┌──────────┐ ┌──────────┐ ┌──────────┐ ┌──────────┐
     │   02     │ │   00     │ │   VA     │ │   M0     │ │   xx     │ │   xx     │
     └────┬─────┘ └──────────┘ └──────────┘ └──────────┘ └────┬─────┘ └────┬─────┘
          │                                                   └─────┬──────┘
  Locally Administered                                       Randomized or
   Unicast Identifier                                        Lease-Derived Hex
```

The MAC address is mapped to the session lease in the database, preventing MAC hijacking or ARP table corruption on virtual bridge networks.

---

## 3. Dynamic Namespace Setup Sequence

When `vamctl` requests a session with direct restricted network access, the network supervisor performs the following setup operations inside the host kernel:

```text
 1. Create Netns         2. Create TAP Device     3. Create veth Pair
 ┌────────────────────┐  ┌─────────────────────┐  ┌────────────────────────┐
 │ - ip netns add     │  │ - ip link add name  │  │ - ip link add veth_host│
 │   netns_vam_abc12  │─►│   tap_vam_abc12     │─►│   type veth peer name  │
 └────────────────────┘  │   type tap          │  │   veth_peer            │
                         └─────────────────────┘  └───────────┬────────────┘
                                                              │
 6. Move TAP & Peer      5. Configure Up State    4. Attach veth_host
 ┌────────────────────┐  ┌─────────────────────┐  │
 │ - ip link set tap  │  │ - ip link set dev   │  │ - Bind veth_host to    │
 │   netns netns_vam  │◄─│   veth_host up      │◄─┼ - Host routing table  │
 │ - Move veth_peer   │  │                     │  │   or bridge            │
 └────────────────────┘  └─────────────────────┘  └────────────────────────┘
```

:::warning[Namespace Boundary Rule]
To ensure strict security, **the TAP device and the guest-facing `veth_peer` interface must be physically moved into the guest's dedicated network namespace (`netns`)** before the guest kernel starts booting. They must not exist in the root host namespace during execution.
:::

---

## 4. Teardown and Resource Cleanup

When a session completes or is abruptly terminated:
1. **API Shutdown**: `vamd` sends `SIGKILL` to the target Firecracker process to halt traffic.
2. **Namespace Destruction**: The network manager issues `ip netns delete netns_vam_<id>`.
3. **Automatic Reaping**: Deleting the namespace triggers the Linux kernel to **automatically delete all virtual interfaces inside it** (including the TAP device and the peer veth interfaces).
4. **Host Sweep**: Any corresponding `veth_host` interfaces remaining in the host root namespace are deleted, and the lease is marked as `RELEASED` in IPAM.

---

## Next Steps

- Explore how IP addresses are allocated to these interfaces in [IPAM Address Management](/network/ipam/).
- See how traffic is secured inside namespaces using [nftables NAT & Isolation](/network/nat/).
- Review the core network invariants in [Network Invariants](/network/invariants/).
