---
title: Network
description: How a session reaches the internet through the host proxy, what the gate checks, and what the crate holds that sessions do not use.
status: shipped
proves:
  - guest-has-no-route-off-the-vm
  - guest-proxy-never-binds-wildcard
  - crates/vam-network/src/broker.rs::platform_ip_floor_runs_before_and_without_the_authorizer
  - crates/vam-network/src/broker.rs::test_mixed_public_and_private_resolution_is_denied
  - crates/vam-network/src/broker.rs::test_check_destination_port_allowlist
  - crates/vam-network/src/broker.rs::connect_mode_terminates_443_unless_host_is_pinned
  - crates/vam-network/src/broker.rs::record_bytes_refuses_past_cap_and_zero_is_unlimited
  - crates/vam-network/src/policy.rs::test_is_ip_denied_hardened_ranges
  - crates/vam-security/src/ssrf_guard.rs::test_is_restricted_ip
  - crates/vam-security/src/ssrf_guard.rs::policy_pin_keeps_every_address_and_fails_closed_on_any_restricted_one
  - crates/vam-security/src/permissions.rs::default_deny_internet_allows_only_listed_hosts
  - crates/vam-orchestrator/src/daemon.rs::passthrough_hosts_default_empty_and_normalize
  - crates/vam-orchestrator/src/daemon.rs::egress_max_bytes_defaults_to_one_gib_and_zero_disables
---

Every session runs in brokered mode. The guest has no route off the VM, and the host proxy makes
each connection after a gate has checked it. This page covers `vam-network` and where `vamd` wires it.

Scenarios named here run on the microVM lane (`scripts/e2e/run.sh --firecracker`). They are `active`.

## The path of one request

1. The guest's tools talk to a proxy on `127.0.0.1:3128` inside the VM. It never binds a wildcard
   address (`guest-proxy-never-binds-wildcard`; `crates/vam-runtime/src/egress.rs`).
2. The proxy sends the request over vsock to the host. Port 55 carries the HTTP and `CONNECT` proxy
   (`vam_network::proxy::serve_proxy`); port 54 carries the older broker protocol. Both share one
   `BrokerGate` per session (`crates/vam-orchestrator/src/brokers.rs:1056-1085`).
3. The gate resolves the name, checks the addresses, asks the session's authorizer, checks the port
   and returns one pinned address to dial (`BrokerGate::check_destination_inner`,
   `crates/vam-network/src/broker.rs:363`).
4. The host dials that address. For HTTPS it terminates TLS with the session CA so a secret
   placeholder can be swapped (see [Secrets](/security/secrets-proxy/)).
5. Each decision becomes an `egress.decided` event, and a refusal also an OCSF block event
   (`brokers.rs`, decision callback).

The guest has no default route, so nothing leaves except through this path. Proved by
`guest-has-no-route-off-the-vm`.

## What the gate checks

| Check | Rule | Proved by |
|---|---|---|
| IP floor | Loopback, RFC 1918, link-local (cloud metadata), CGNAT, unspecified, broadcast, multicast and the IPv6 equivalents are refused. IPv4-mapped and NAT64 addresses are judged by the IPv4 address inside. One refused answer refuses the whole name. | `policy.rs::test_is_ip_denied_hardened_ranges`, `broker.rs::test_mixed_public_and_private_resolution_is_denied`, `ssrf_guard.rs::test_is_restricted_ip` |
| Floor before rules | The floor runs first and an allow rule cannot widen it. | `broker.rs::platform_ip_floor_runs_before_and_without_the_authorizer` |
| Host rule | The authorizer built from the agent's `internet` rules decides. **With no `internet` setting the answer is allow.** | `permissions.rs::default_deny_internet_allows_only_listed_hosts`; see [Permissions](/security/permissions/) |
| Port | Ports 80 and 443 unless the policy lists others (`policy.rs:130-134`). | `broker.rs::test_check_destination_port_allowlist` |
| Pinned address | The first resolved address is the one dialled, so a second lookup cannot change it. | `ssrf_guard.rs::policy_pin_keeps_every_address_and_fails_closed_on_any_restricted_one` |

The floor code is `policy.rs:14-60` (`get_protected_deny_set`, `is_ip_denied`). A name that does not
resolve is refused and the event names the host rule if one also refused it (`broker.rs:371-386`).

The scenario `egress-refuses-an-ungranted-host` is written for the host-rule and metadata case. It is
`pending`, so this page does not rely on it.

## Settings

| Setting | Effect | Proved by |
|---|---|---|
| `VAMD_EGRESS_PASSTHROUGH_HOSTS` | Comma-separated hosts tunnelled on port 443 without TLS termination. Empty by default. A `CONNECT` to any other port is a passthrough tunnel too (`broker.rs:1017-1028`). | `daemon.rs::passthrough_hosts_default_empty_and_normalize`, `broker.rs::connect_mode_terminates_443_unless_host_is_pinned` |
| `VAMD_EGRESS_MAX_BYTES` | Per-session cap on bytes in both directions. Default 1 GiB, `0` turns it off (`daemon.rs:167`). | `daemon.rs::egress_max_bytes_defaults_to_one_gib_and_zero_disables`, `broker.rs::record_bytes_refuses_past_cap_and_zero_is_unlimited` |

Both are also command-line flags (`--egress-passthrough-hosts`, `--egress-max-bytes`,
`daemon.rs:85-92`). A passthrough host skips secret substitution because the host cannot see the
traffic.

## The session CA

Each session gets its own certificate authority (`crates/vam-network/src/tls_ca.rs`). The session
owner or an operator can fetch its certificate at `GET /v1/sessions/:id/egress-ca`; the private key
stays in memory. Proved by the `mitm_tests.rs` tests listed on [Secrets](/security/secrets-proxy/).

## Why there is no kernel filter

Sessions run brokered mode, so there is no guest packet path to filter, and VAM ships no kernel
packet filter for egress. Reopen the question as its own design if one of three things appears:

* a direct-route network mode for traffic the proxy cannot terminate;
* the userspace proxy becoming the bottleneck at many VMs per host;
* kernel-enforced egress being required as an attestable control.

The first answer then would be an nftables set that a DNS proxy fills.

## Code in the crate that sessions do not use

* `NetResourcePool::lease` skips TAP and network namespace setup for the default modes
  (`crates/vam-network/src/pool.rs:5-16`, `:52-56`). A restricted direct mode has no pre-provisioned
  leases, so it returns `NotProvisioned`.
* `LinuxKernelNetAdapter`, the TAP name and address helpers in `ipam.rs`, and the policy compiler
  are built for that direct mode.
* `SessionDnsProxy` (`dns.rs`) has no caller in `vam-orchestrator`.

The earlier pages for modes, TAP, NAT, DNS filtering and traffic control describe this unused
design. They now live in `design/docs-archive/network/` and are design intent, not behaviour.

## Not built yet

* Direct-route network modes, NAT rules and traffic shaping.
* Domain filtering at a DNS proxy.
* Kernel-level egress filtering.
