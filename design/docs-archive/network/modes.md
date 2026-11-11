> Moved from the docs site on 2026-10-30: design intent, not built (brokered sessions use no TAP, NAT, DNS filter or traffic control).

---
title: Network Modes
description: The three core connectivity modes in VAM and their operational trade-offs.
---

:::note[Status: Implemented]
VAM host-side networking and security mechanisms are fully implemented, verified, and integrated within the `vam-network` and `vam-orchestrator` crates.
:::

To ensure robust security and prevent data exfiltration by unverified agent scripts, VAM classifies connectivity into three distinct operational modes. Rather than granting general, unfiltered network access to guest environments, networking is strictly scoped by safety-by-default rules.

---

## 1. Network Modes Matrix

| Mode | Direct Guest NIC? | Egress Path | Security Risk | Primary Use Case |
|---|---|---|---|---|
| **Isolated** (Default) | No | None | **Zero** | Highly sensitive data processing, local tool executions. |
| **Brokered Egress** (Preferred) | No | Host-side gRPC over vsock | **Very Low** | Inference calls, authenticated API connections. |
| **Restricted Direct** (Opt-In) | Yes (virtio-net) | Per-session netns + TAP + veth | **Medium** | Web scraping, package installs, external downloads. |

---

## 2. Isolated Mode (Default)

This is VAM's baseline, high-security state.
* **The Reality**: The microVM configuration contains **zero virtio-net hardware devices**. The guest Linux kernel is completely unaware of standard ethernet structures (aside from the standard loopback `lo` interface).
* **Hot path (Phase 3)**: `vamd` does **not** create TAP devices, network namespaces, or `tc` shaping for Isolated sessions. No per-session `sudo ip`/`tc` runs on the default create path.
* **Isolation Boundary**: The VM has no network sockets, no IP address, and no capacity to establish direct TCP/UDP packets. All data ingress and egress is strictly confined to the `AF_VSOCK` binary streams managed by `vam-core`.
* **Value**: Guarantees absolute safety. Even if an attacker executes arbitrary shellcode inside the guest, they cannot make contact with external domains or scan local host network interfaces.

---

## 3. Brokered Egress Mode (Preferred)

Brokered Egress is the **default network pool mode** at daemon startup and bridges the gap between isolation and external functionality by offloading network operations to the host.
* **The Path**: The guest VM has no direct network card. Instead, when an agent tool requests an LLM inference call or third-party API transaction, `vam-runtime` transmits the request across `AF_VSOCK` to the host control plane.
* **Hot path (Phase 3)**: Like Isolated, Brokered sessions skip TAP/netns/tc/IPAM on create. Egress is handled via host vsock listeners on ports **52** (guest control), **53** (inference), **54** (legacy egress RPC), and **55** (HTTP forward proxy). Firecracker is **not** configured with `PutGuestNetworkInterfaceByID` for these modes. Port 55 is bound in Phase 1; the guest dials it in Phase 2.
* **The Proxy**: The trusted host gate (see [Network Policy](/network/policy/)) checks the domain, port, and protected deny set, substitutes security credentials (using the [Secret Injection Proxy](/security/secrets-proxy/)), and either tunnels `CONNECT` or performs the external call on the guest's behalf.
* **Value**: Keeps raw credential secrets and direct socket handling out of untrusted guest RAM, eliminating the risk of key theft and direct network exfiltration.

---

## 4. Restricted Direct Egress Mode (Explicit Opt-In)

When direct TCP/UDP sockets are required (for example, a coding agent needs to execute `npm install` or query an un-proxied API):
* **The Hardware (future)**: `vamd` will provision a dedicated `virtio-net` device for the VM and link it to an isolated dynamic TAP interface inside a dedicated namespace on the host. **Current code does not attach virtio-net** — the TAP/netns path exists only for Restricted Direct and requires a filled TAP resource pool (Phase 4+).
* **The Controls**: Direct traffic is strictly regulated by host-side mechanisms:
  - **DNS Whitelisting**: Only DNS queries for explicitly allowed domains (e.g. `*.github.com`) are resolved.
  - **Raw IP Blocking**: Non-resolved IP connections or hard-coded outbound IP requests are blocked.
  - **Bandwidth Shaping**: Throughput is capped via Traffic Control (`tc`) token buckets to prevent DoS attacks.
* **How to Enable**: Restricted Direct Egress must be explicitly declared as an authorized parameter inside the playbook's execution contract (using Cedar policy validation).

---

## 5. Unsupported Mode: Unrestricted Direct Egress

:::danger[The Unfiltered Internet Anti-Pattern]
VAM does **not** support unrestricted direct egress inside guest environments.
* **Why**: Granting an autonomous LLM agent unfiltered access to raw IP sockets and public DNS namespaces creates extreme security exposure. A compromised agent can instantly participate in botnet traffic, download malicious secondary payloads, perform local network port scans, or exfiltrate sensitive files via raw TCP/UDP tunnels.
* **Enforcement**: If a playbook attempts to requests unrestricted egress, `vamd`'s admission controller rejects the request with a permission error.
:::

---

## Next Steps

- Explore the wire contracts that define these mode arguments in [Network Contracts](/network/contracts/).
- Understand how restricted TAP interfaces are allocated in [TAP Allocation](/network/tap/).
- Read about domain allowlisting policies in [DNS Domain Filtering](/network/dns/).
