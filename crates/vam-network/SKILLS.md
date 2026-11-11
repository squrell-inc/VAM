# Developer Invariants — `vam-network`

These rules are non-negotiable invariants for any developer or agent modifying
the host-side network infrastructure. They are mirrored from the full
architectural specifications in `/network/invariants/`.

---

## 1. Safety & Default Isolation
* **Baseline Isolation**: MicroVMs must boot with **zero network devices** by default. Direct network access is strictly opt-in.
* **Brokered Preference**: Outbound AI tool requests, LLM inference calls, and API integrations should always prefer **Host-Brokered Egress** (via `AF_VSOCK` proxies). Direct socket transit is an explicit, opt-in restricted mode.
* **No Unfiltered Internet**: Under no circumstances may a configuration or mode grant guest environments unrestricted direct internet access.

---

## 2. Structural & Namespace Isolation
* **Dedicated Namespaces**: Every network-enabled session must be isolated inside a **dedicated Linux Network Namespace (`netns`)**. Bridge networks shared across different sessions are strictly forbidden.
* **Kernel Name Safety**: Network interface names must be deterministically hashed from the session ID and must fit safely within the **15-character `IFNAMSIZ` limit** (including prefix).
* **MAC Integrity**: MAC addresses must be locally administered unicast addresses derived from lease identity and checked for collisions.

---

## 3. Filtering & Control Posture
* **Brokered First**: Every session runs the brokered network mode: the guest has no route and the host proxy makes every connection. There is no kernel-level egress filter.
* **No Bypass Sockets**: All nameserver resolutions must be pinned to VAM's local per-session DNS proxy. Direct outgoing TCP/UDP connections to port `53` or other public resolvers must be blackholed.
* **DNS IP-Binding**: Traffic to a destination IP is only permitted if it was preceded by an authorized DNS query matching the session's allowlist.
* **Atomic Transactions**: Rule updates and policy modifications must be compiled, written to shadow maps, and swapped using atomic transaction pointers (**Generation IDs**) to prevent partial security windows.

---

## 4. Protected Targets & Fail-Closed
* **Protected Blocks**: Every policy compiler run must append the **Protected Destination Deny Set** (blocking localhost, local RFC1918 private classes, link-local, multicast, and the instance metadata endpoint `169.254.169.254`).
* **Fail-Closed State**: If any network hook, or DNS resolver proxy fails to initialize or encounters a panic, the entire bootstrap sequence must fail instantly, keeping interfaces closed.

---

## 5. Telemetry & Cleanup
* **Privacy Baseline**: Network telemetry systems must **never record, log, or inspect raw packet payloads**.
* **State-First Order**: Leases must be committed to the durable SQLite database *before* creating physical namespaces or virtual interfaces.
* **Quarantine Requirement**: Released IP subnets and MAC addresses must enter a **30-second quarantine buffer** to allow conntrack and stale packets to expire before re-allocation.
