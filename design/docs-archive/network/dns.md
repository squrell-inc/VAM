> Moved from the docs site on 2026-10-30: design intent, not built (brokered sessions use no TAP, NAT, DNS filter or traffic control).

---
title: DNS Domain Filtering
description: Per-session DNS resolvers, wildcard matching rules, CNAME closures, and DNS rebinding defenses.
---

:::note[Status: Implemented]
VAM host-side networking and security mechanisms are fully implemented, verified, and integrated within the `vam-network` and `vam-orchestrator` crates.
:::

Because domain-based whitelisting is a primary requirement for Restricted Direct Egress, VAM implements a highly secure, **per-session DNS proxy and filtering resolver**. Rather than attempting deep packet inspection of encrypted TLS handshakes, VAM regulates outbound connections by controlling the guest's name resolution channel.

---

## 1. Per-Session DNS Resolver Binding

When a Restricted Direct Egress session boots:
1. **Nameserver Pinning**: The guest's `/etc/resolv.conf` is configured to point solely to the host gateway IP (e.g. `172.20.0.1`).
2. **Local Bind**: `vam-network` spawns an isolated DNS proxy listener inside the session's network namespace (`netns`), binding it to port `53` on the gateway interface.
3. **No Bypass**: All direct external DNS traffic (UDP/TCP port `53`), DNS-over-TLS (DoT, port `853`), and DNS-over-HTTPS (DoH, port `443` to non-whitelisted IPs) is **denied by default** at the host. The guest *must* use VAM's local DNS proxy.

---

## 2. Wildcard Semantics & CNAME Closure

When the DNS proxy intercepts a resolution query:
* **The Check**: It evaluates the requested domain against the session's compiled `allowed_domains` list (e.g. `["*.github.com", "api.anthropic.com"]`).
* **Wildcard Semantics (Decision N16)**: Wildcards are explicitly matched. A rule for `*.example.com` matches `api.example.com` and `web.example.com`, but does **not** match the apex domain `example.com` unless `example.com` is explicitly added to the allowlist.
* **CNAME Closure Protection**: If a whitelisted domain resolves to a CNAME chain (e.g., `api.anthropic.com` points to `anthropic.map.fastly.net`), VAM's proxy automatically resolves and whitelists the entire chain. If it did not, subsequent TCP packets directed to Fastly's IPs would be blocked at the L3 filter.

---

## 3. Dynamic IP Allowlist Commits

Once a domain query is successfully authorized by the DNS proxy:

```text
 1. Guest requests resolution
    "api.openai.com"
          │
          ▼
 2. DNS Proxy validates domain
    Is "api.openai.com" in allowlist?
          ├─────────────────────────┐
          │ (YES)                   │ (NO)
          ▼                         ▼
 3. Resolve IP via upstream  3. Return DNS `REFUSED`
    Received: "140.82.112.4"        to Guest.
          │
          ▼
 4. Commit IP to BPF Map
    allowed_destinations += 140.82.112.4
    Set map entry TTL = 300s (DNS TTL)
          │
          ▼
 5. Return success IP to Guest.
    Guest can now establish TCP connection.
```

This **DNS-to-IP Binding pattern** ensures that:
* **Dynamic Safety**: Direct outbound connections to hard-coded IP addresses (bypassing DNS nameserver queries) are blocked because they have no preceding DNS commit entry inside the BPF map.

---

## 4. DNS Rebinding & Private Address Defenses

To prevent malicious guest scripts from exploiting local host-network services via DNS tricks:
* **IP Rebinding Protection**: VAM's proxy inspects all incoming upstream DNS answers. If an external whitelisted domain attempts to resolve to a private local IP address (e.g. `127.0.0.1`, `192.168.1.50`, or the AWS metadata endpoint `169.254.169.254`), the DNS proxy **drops the answer** and returns `REFUSED`.
* **TTL Pinning**: The proxy clamps maximum DNS TTL values to **300 seconds** to ensure that resolved IP entries inside BPF maps expire quickly and do not linger as permanent egress holes.

---

## Next Steps

- Review the high-level security architecture in [Network Security Model](/network/security/).
- See how DNS errors are classified in [Network Error Handling](/network/errors/).
