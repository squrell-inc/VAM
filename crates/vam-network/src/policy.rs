use crate::error::VamNetworkError;
use arc_swap::ArcSwap;
use ipnet::IpNet;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use vam_spec::v1::{NetworkMode, NetworkPolicy, NetworkPolicySnapshot};

/// The immutable, non-negotiable Protected Destination Deny Set.
///
/// Blocks loopback, RFC1918, link-local (including metadata endpoints),
/// CGNAT (`100.64.0.0/10`), the unspecified `0.0.0.0/8` block, limited
/// broadcast, and IPv6 loopback / unspecified / link-local / ULA.
/// Multicast is rejected in [`is_ip_denied`] via `IpAddr::is_multicast`.
pub fn get_protected_deny_set() -> Vec<IpNet> {
    vec![
        "127.0.0.0/8".parse().unwrap(),
        "10.0.0.0/8".parse().unwrap(),
        "172.16.0.0/12".parse().unwrap(),
        "192.168.0.0/16".parse().unwrap(),
        "169.254.0.0/16".parse().unwrap(),
        "0.0.0.0/8".parse().unwrap(),
        "100.64.0.0/10".parse().unwrap(),
        "255.255.255.255/32".parse().unwrap(),
        "::1/128".parse().unwrap(),
        "::/128".parse().unwrap(),
        "fe80::/10".parse().unwrap(),
        "fc00::/7".parse().unwrap(),
    ]
}

/// Verifies whether a destination IP address falls within the protected deny set.
///
/// IPv4-mapped IPv6 (`::ffff:a.b.c.d`) is classified as the embedded IPv4
/// address, matching `ssrf_guard::is_restricted_ip`, so `::ffff:127.0.0.1`
/// cannot bypass `127.0.0.0/8`.
pub fn is_ip_denied(ip: &IpAddr) -> bool {
    if let IpAddr::V6(v6) = ip {
        if let Some(v4) = v6.to_ipv4() {
            return is_ip_denied(&IpAddr::V4(v4));
        }
        // NAT64 well-known prefix 64:ff9b::/96 carries the IPv4 destination in
        // the low 32 bits, so classify that embedded address.
        let octets = v6.octets();
        if octets[..12] == [0, 0x64, 0xff, 0x9b, 0, 0, 0, 0, 0, 0, 0, 0] {
            let embedded = Ipv4Addr::new(octets[12], octets[13], octets[14], octets[15]);
            return is_ip_denied(&IpAddr::V4(embedded));
        }
    }
    if ip.is_loopback() || ip.is_multicast() || ip.is_unspecified() {
        return true;
    }
    if let IpAddr::V4(v4) = ip {
        if v4.is_broadcast() {
            return true;
        }
    }
    get_protected_deny_set().iter().any(|net| net.contains(ip))
}

/// Parses an IP literal host, accepting the brackets a URL puts around an
/// IPv6 address (`[::ffff:127.0.0.1]`) as well as the bare form.
pub(crate) fn parse_ip_literal(host: &str) -> Option<IpAddr> {
    let inner = host
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or(host);
    inner.parse().ok()
}

/// Allow or deny one host and port.
///
/// Used by sessions with no [`crate::gate::EgressAuthorizer`]. With one, the host
/// decision comes from the authorizer and only [`allow_port`] applies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny(String),
}

/// Host allowlist plus port check.
///
/// When `allowed_ports` is empty, only 80 and 443 are permitted. Otherwise the
/// port must fall in one of the configured TCP (or unspecified / `any`) ranges.
pub fn allow_destination(snapshot: &NetworkPolicySnapshot, host: &str, port: u16) -> Decision {
    let Some(policy) = snapshot.policy.as_ref() else {
        return Decision::Deny(format!(
            "Access to host '{}' is blocked by active network policy",
            host
        ));
    };

    let host_allowed = !policy.allowed_domains.is_empty()
        && policy
            .allowed_domains
            .iter()
            .any(|pattern| pattern == "*" || match_domain(pattern, host));
    if !host_allowed {
        return Decision::Deny(format!(
            "Access to host '{}' is blocked by active network policy",
            host
        ));
    }

    if !port_allowed(policy, port) {
        return Decision::Deny(format!(
            "Access to host '{}' port {} is blocked by active network policy",
            host, port
        ));
    }

    Decision::Allow
}

/// Port check alone, for sessions whose host decision comes from an
/// [`crate::gate::EgressAuthorizer`]. No policy snapshot denies.
pub fn allow_port(snapshot: &NetworkPolicySnapshot, host: &str, port: u16) -> Decision {
    match snapshot.policy.as_ref() {
        Some(policy) if port_allowed(policy, port) => Decision::Allow,
        Some(_) => Decision::Deny(format!(
            "Access to host '{}' port {} is blocked by active network policy",
            host, port
        )),
        None => Decision::Deny(format!(
            "Access to host '{}' is blocked by active network policy",
            host
        )),
    }
}

fn port_allowed(policy: &NetworkPolicy, port: u16) -> bool {
    if policy.allowed_ports.is_empty() {
        return port == 80 || port == 443;
    }
    let port = u32::from(port);
    policy.allowed_ports.iter().any(|range| {
        let protocol = range.protocol.trim();
        if !protocol.is_empty()
            && !protocol.eq_ignore_ascii_case("tcp")
            && !protocol.eq_ignore_ascii_case("any")
        {
            return false;
        }
        port >= range.from_port && port <= range.to_port
    })
}

/// Performs safe, wildcard-aware domain matching following Decision N16:
/// - `*.example.com` matches `api.example.com` and `sub.api.example.com`
/// - `*.example.com` does NOT match the apex `example.com`
pub fn match_domain(pattern: &str, target: &str) -> bool {
    let pattern = pattern.to_lowercase();
    let target = target.to_lowercase();
    if pattern == target {
        return true;
    }
    if let Some(suffix) = pattern.strip_prefix("*.") {
        // "example.com"
        if target.ends_with(suffix) {
            // Ensure target is a strict subdomain
            if target.len() > suffix.len() {
                let prefix_len = target.len() - suffix.len();
                if target.as_bytes()[prefix_len - 1] == b'.' {
                    return true;
                }
            }
        }
    }
    false
}

/// Compiles a user-supplied NetworkPolicy into an immutable NetworkPolicySnapshot.
pub struct NetworkPolicyCompiler;

impl NetworkPolicyCompiler {
    pub fn compile(
        session_id: &str,
        generation: u64,
        policy: NetworkPolicy,
    ) -> Result<NetworkPolicySnapshot, VamNetworkError> {
        // Enforce that if mode is Brokered or Isolated, direct egress rules (like allowed_domains/CIDRs) are empty or ignored
        let compiled_policy = match NetworkMode::try_from(policy.mode) {
            Ok(NetworkMode::Isolated) | Ok(NetworkMode::Brokered) => NetworkPolicy {
                mode: policy.mode,
                max_egress_bps: policy.max_egress_bps,
                max_ingress_bps: policy.max_ingress_bps,
                max_packet_rate_pps: policy.max_packet_rate_pps,
                allowed_domains: vec![],
                allowed_cidr_destinations: vec![],
                allowed_ports: vec![],
            },
            _ => policy,
        };

        // Always validate allowed_cidr_destinations to make sure they don't overlap with the Protected Deny Set
        for cidr in &compiled_policy.allowed_cidr_destinations {
            let parsed: IpNet = cidr.parse().map_err(|e| {
                VamNetworkError::Serialization(format!(
                    "Invalid allowed CIDR destination '{}': {}",
                    cidr, e
                ))
            })?;
            // If the configured allowed CIDR overlaps with any protected network, reject it
            for deny_net in get_protected_deny_set() {
                if deny_net.contains(&parsed.addr()) || parsed.contains(&deny_net.addr()) {
                    return Err(VamNetworkError::EgressForbidden {
                        reason: format!(
                            "Allowed CIDR '{}' violates protected destination deny rule against '{}'",
                            cidr, deny_net
                        ),
                    });
                }
            }
        }

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        Ok(NetworkPolicySnapshot {
            session_id: session_id.to_string(),
            policy_generation: generation,
            compiled_at_timestamp: now,
            policy: Some(compiled_policy),
            resolved_allowed_ips: vec![],
        })
    }
}

/// Thread-safe, lock-free container for holding and swapping the active policy snapshot (Decision N20).
pub struct ActivePolicyContainer {
    snapshot: ArcSwap<NetworkPolicySnapshot>,
}

impl ActivePolicyContainer {
    pub fn new(initial: NetworkPolicySnapshot) -> Self {
        Self {
            snapshot: ArcSwap::from_pointee(initial),
        }
    }

    pub fn load(&self) -> Arc<NetworkPolicySnapshot> {
        self.snapshot.load_full()
    }

    /// Swaps the current snapshot with a newer generation.
    /// Rejects the update if the incoming generation is less than or equal to the current generation.
    pub fn swap(&self, new_snap: NetworkPolicySnapshot) -> Result<(), VamNetworkError> {
        let current = self.load();
        if new_snap.policy_generation <= current.policy_generation {
            return Err(VamNetworkError::EgressForbidden {
                reason: format!(
                    "Outdated policy generation: current is {}, received {}",
                    current.policy_generation, new_snap.policy_generation
                ),
            });
        }
        self.snapshot.store(Arc::new(new_snap));
        Ok(())
    }
}

/// HeaderFilter strips Proxy-*, Host override, and hop-by-hop HTTP headers before dispatch.
pub struct HeaderFilter;

impl HeaderFilter {
    pub const STRIPPED_HEADERS: &'static [&'static str] = &[
        "connection",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "proxy-connection",
        "te",
        "trailer",
        "trailers",
        "transfer-encoding",
        "upgrade",
        "host",
        "x-target-url",
        "content-length",
    ];

    pub fn sanitize_headers(
        headers: std::collections::HashMap<String, String>,
    ) -> std::collections::HashMap<String, String> {
        let mut clean = std::collections::HashMap::new();
        for (k, v) in headers {
            let k_lower = k.to_lowercase();
            if k_lower.starts_with("proxy-") || Self::STRIPPED_HEADERS.contains(&k_lower.as_str()) {
                continue;
            }
            clean.insert(k, v);
        }
        clean
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_ip_denied() {
        assert!(is_ip_denied(&"127.0.0.1".parse().unwrap()));
        assert!(is_ip_denied(&"10.0.0.5".parse().unwrap()));
        assert!(is_ip_denied(&"172.16.50.2".parse().unwrap()));
        assert!(is_ip_denied(&"169.254.169.254".parse().unwrap())); // Metadata endpoint
        assert!(is_ip_denied(&"ff02::1".parse().unwrap())); // Multicast

        // Allowed IPs
        assert!(!is_ip_denied(&"8.8.8.8".parse().unwrap()));
        assert!(!is_ip_denied(&"140.82.112.5".parse().unwrap()));
    }

    #[test]
    fn test_match_domain_wildcard_decision_n16() {
        assert!(match_domain("*.example.com", "api.example.com"));
        assert!(match_domain("*.example.com", "sub.api.example.com"));
        assert!(!match_domain("*.example.com", "example.com")); // Apex must fail!
        assert!(!match_domain("*.example.com", "fake-example.com"));
        assert!(match_domain("example.com", "example.com"));
    }

    #[test]
    fn test_policy_compiler_and_monotonicity_n20() {
        let raw_policy = NetworkPolicy {
            mode: NetworkMode::RestrictedEgress as i32,
            max_egress_bps: 10_000_000,
            max_ingress_bps: 10_000_000,
            max_packet_rate_pps: 1000,
            allowed_domains: vec!["*.github.com".to_string()],
            allowed_cidr_destinations: vec!["140.82.112.0/20".to_string()],
            allowed_ports: vec![],
        };

        let snap1 = NetworkPolicyCompiler::compile("sess-1", 1, raw_policy.clone()).unwrap();
        assert_eq!(snap1.policy_generation, 1);

        let active_policy = ActivePolicyContainer::new(snap1);

        // Attempting to swap with same generation fails
        let snap2_stale = NetworkPolicyCompiler::compile("sess-1", 1, raw_policy.clone()).unwrap();
        assert!(active_policy.swap(snap2_stale).is_err());

        // Attempting to swap with older generation fails
        let snap2_stale_old =
            NetworkPolicyCompiler::compile("sess-1", 0, raw_policy.clone()).unwrap();
        assert!(active_policy.swap(snap2_stale_old).is_err());

        // Successful swap with newer generation
        let snap2_good = NetworkPolicyCompiler::compile("sess-1", 2, raw_policy.clone()).unwrap();
        active_policy.swap(snap2_good).unwrap();
        assert_eq!(active_policy.load().policy_generation, 2);
    }

    #[test]
    fn test_sanitize_headers_strips_proxy_metadata() {
        let mut headers = std::collections::HashMap::new();
        headers.insert("Authorization".to_string(), "Bearer token".to_string());
        headers.insert(
            "X-Target-Url".to_string(),
            "vamsec_jira_base_url/rest/api/3/search".to_string(),
        );
        headers.insert("Content-Length".to_string(), "42".to_string());
        headers.insert("Host".to_string(), "vamsec_jira_base_url".to_string());

        let clean = HeaderFilter::sanitize_headers(headers);
        assert_eq!(clean.len(), 1);
        assert_eq!(clean.get("Authorization").unwrap(), "Bearer token");
    }

    #[test]
    fn test_policy_compiler_rejects_denied_cidr() {
        let bad_policy = NetworkPolicy {
            mode: NetworkMode::RestrictedEgress as i32,
            max_egress_bps: 0,
            max_ingress_bps: 0,
            max_packet_rate_pps: 0,
            allowed_domains: vec![],
            allowed_cidr_destinations: vec!["169.254.0.0/16".to_string()], // Violates metadata
            allowed_ports: vec![],
        };

        let res = NetworkPolicyCompiler::compile("sess-1", 1, bad_policy);
        assert!(res.is_err());
    }

    #[test]
    fn test_is_ip_denied_hardened_ranges() {
        // IPv4-mapped IPv6 must be classified as the embedded address.
        assert!(is_ip_denied(&"::ffff:127.0.0.1".parse().unwrap()));
        assert!(is_ip_denied(&"::ffff:10.0.0.1".parse().unwrap()));
        assert!(is_ip_denied(&"::ffff:169.254.169.254".parse().unwrap()));
        assert!(is_ip_denied(&"::ffff:0.0.0.1".parse().unwrap()));
        assert!(is_ip_denied(&"::ffff:100.64.0.1".parse().unwrap()));
        assert!(is_ip_denied(&"::ffff:255.255.255.255".parse().unwrap()));
        assert!(!is_ip_denied(&"::ffff:8.8.8.8".parse().unwrap()));

        // NAT64 well-known prefix 64:ff9b::/96 embeds the IPv4 destination.
        assert!(is_ip_denied(&"64:ff9b::7f00:1".parse().unwrap()));
        assert!(is_ip_denied(&"64:ff9b::a9fe:a9fe".parse().unwrap()));
        assert!(is_ip_denied(&"64:ff9b::a00:1".parse().unwrap()));
        assert!(!is_ip_denied(&"64:ff9b::808:808".parse().unwrap()));

        // 0.0.0.0/8, not only the unspecified address.
        assert!(is_ip_denied(&"0.0.0.0".parse().unwrap()));
        assert!(is_ip_denied(&"0.1.2.3".parse().unwrap()));
        assert!(is_ip_denied(&"0.255.255.255".parse().unwrap()));
        assert!(!is_ip_denied(&"1.0.0.0".parse().unwrap()));

        // Limited broadcast.
        assert!(is_ip_denied(&"255.255.255.255".parse().unwrap()));

        // CGNAT 100.64.0.0/10.
        assert!(is_ip_denied(&"100.64.0.0".parse().unwrap()));
        assert!(is_ip_denied(&"100.64.0.1".parse().unwrap()));
        assert!(is_ip_denied(&"100.127.255.255".parse().unwrap()));
        assert!(!is_ip_denied(&"100.63.255.255".parse().unwrap()));
        assert!(!is_ip_denied(&"100.128.0.0".parse().unwrap()));

        // Unspecified IPv6.
        assert!(is_ip_denied(&"::".parse().unwrap()));
    }

    fn snapshot(
        domains: Vec<String>,
        ports: Vec<vam_spec::v1::AllowedPortRange>,
    ) -> NetworkPolicySnapshot {
        NetworkPolicySnapshot {
            session_id: "sess-ports".to_string(),
            policy_generation: 1,
            compiled_at_timestamp: 0,
            policy: Some(NetworkPolicy {
                mode: NetworkMode::Brokered as i32,
                max_egress_bps: 0,
                max_ingress_bps: 0,
                max_packet_rate_pps: 0,
                allowed_domains: domains,
                allowed_cidr_destinations: vec![],
                allowed_ports: ports,
            }),
            resolved_allowed_ips: vec![],
        }
    }

    #[test]
    fn test_allow_destination_ports() {
        let open = snapshot(vec!["api.example.com".to_string()], vec![]);
        assert_eq!(
            allow_destination(&open, "api.example.com", 443),
            Decision::Allow
        );
        assert_eq!(
            allow_destination(&open, "api.example.com", 80),
            Decision::Allow
        );
        assert!(matches!(
            allow_destination(&open, "api.example.com", 8443),
            Decision::Deny(_)
        ));
        assert!(matches!(
            allow_destination(&open, "other.example.com", 443),
            Decision::Deny(_)
        ));

        let custom = snapshot(
            vec!["api.example.com".to_string()],
            vec![vam_spec::v1::AllowedPortRange {
                from_port: 8443,
                to_port: 8443,
                protocol: "tcp".to_string(),
            }],
        );
        assert!(matches!(
            allow_destination(&custom, "api.example.com", 443),
            Decision::Deny(_)
        ));
        assert_eq!(
            allow_destination(&custom, "api.example.com", 8443),
            Decision::Allow
        );

        let udp_only = snapshot(
            vec!["*".to_string()],
            vec![vam_spec::v1::AllowedPortRange {
                from_port: 443,
                to_port: 443,
                protocol: "udp".to_string(),
            }],
        );
        assert!(matches!(
            allow_destination(&udp_only, "api.example.com", 443),
            Decision::Deny(_)
        ));
    }
}
