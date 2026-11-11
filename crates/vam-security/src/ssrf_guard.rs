use std::io;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::str::FromStr;
use url::Url;

/// Checks if an IP address is a private, loopback, link-local, or otherwise restricted address.
pub fn is_restricted_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ipv4) => {
            // Loopback: 127.0.0.0/8
            if ipv4.is_loopback() {
                return true;
            }
            // Private ranges (RFC 1918)
            // 10.0.0.0/8
            // 172.16.0.0/12
            // 192.168.0.0/16
            if ipv4.is_private() {
                return true;
            }
            // Link-local / IMDS: 169.254.0.0/16
            if ipv4.is_link_local() {
                return true;
            }
            // Unspecified: 0.0.0.0
            if ipv4.is_unspecified() {
                return true;
            }
            // Documentation (RFC 5737)
            // 192.0.2.0/24, 198.51.100.0/24, 203.0.113.0/24
            if ipv4.is_documentation() {
                return true;
            }
            // Broadcast: 255.255.255.255
            if ipv4.is_broadcast() {
                return true;
            }
            // Multicast: 224.0.0.0/4
            if ipv4.is_multicast() {
                return true;
            }

            false
        }
        IpAddr::V6(ipv6) => {
            // Loopback: ::1
            if ipv6.is_loopback() {
                return true;
            }
            // Unspecified: ::
            if ipv6.is_unspecified() {
                return true;
            }
            // IPv4-mapped/compatible IPv6 addresses can reach the same private
            // IPv4 targets; classify the embedded address before public pinning.
            if let Some(ipv4) = ipv6.to_ipv4() {
                return is_restricted_ip(IpAddr::V4(ipv4));
            }
            // Link-local: fe80::/10
            // Since std::net::Ipv6Addr::is_unicast_link_local() is stable
            if (ipv6.segments()[0] & 0xffc0) == 0xfe80 {
                return true;
            }
            // Unique local address (ULA): fc00::/7
            if (ipv6.segments()[0] & 0xfe00) == 0xfc00 {
                return true;
            }
            // Multicast: ff00::/8
            if ipv6.is_multicast() {
                return true;
            }

            false
        }
    }
}

/// Stricter classifier for fetching arbitrary public web pages.
///
/// A superset of [`is_restricted_ip`]. It also refuses the shared address
/// space (CGNAT, which holds some cloud metadata endpoints such as
/// `100.100.100.200`), `0.0.0.0/8`, `192.0.0.0/24`, benchmarking
/// (`198.18.0.0/15`), reserved `240.0.0.0/4`, and the IPv6 transition and
/// special ranges (NAT64, 6to4, Teredo, discard, documentation, site-local),
/// re-classifying any IPv4 address they embed. Callers that reach
/// operator-configured private hosts (vamd's MCP and OAuth clients) keep
/// using [`is_restricted_ip`].
pub fn is_restricted_ip_strict(ip: IpAddr) -> bool {
    if is_restricted_ip(ip) {
        return true;
    }
    match ip {
        IpAddr::V4(v4) => strict_restricted_v4(v4),
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4() {
                return is_restricted_ip_strict(IpAddr::V4(v4));
            }
            let s = v6.segments();
            let embedded = |hi: u16, lo: u16| {
                is_restricted_ip_strict(IpAddr::V4(std::net::Ipv4Addr::new(
                    (hi >> 8) as u8,
                    hi as u8,
                    (lo >> 8) as u8,
                    lo as u8,
                )))
            };
            // NAT64 well-known prefix 64:ff9b::/96 embeds an IPv4 address.
            if s[0] == 0x0064 && s[1] == 0xff9b && s[2..6].iter().all(|x| *x == 0) {
                return embedded(s[6], s[7]);
            }
            // NAT64 local-use prefix 64:ff9b:1::/48.
            if s[0] == 0x0064 && s[1] == 0xff9b && s[2] == 0x0001 {
                return true;
            }
            // IPv4-translated ::ffff:0:a.b.c.d (RFC 2765).
            if s[..4].iter().all(|x| *x == 0) && s[4] == 0xffff && s[5] == 0 {
                return embedded(s[6], s[7]);
            }
            // 6to4 2002::/16 embeds an IPv4 address in bits 16..48.
            if s[0] == 0x2002 {
                return embedded(s[1], s[2]);
            }
            // Teredo 2001:0::/32.
            if s[0] == 0x2001 && s[1] == 0 {
                return true;
            }
            // Documentation 2001:db8::/32.
            if s[0] == 0x2001 && s[1] == 0x0db8 {
                return true;
            }
            // Deprecated site-local fec0::/10.
            if (s[0] & 0xffc0) == 0xfec0 {
                return true;
            }
            // Discard-only 100::/64.
            if s[0] == 0x0100 && s[1] == 0 && s[2] == 0 && s[3] == 0 {
                return true;
            }
            false
        }
    }
}

fn strict_restricted_v4(ip: std::net::Ipv4Addr) -> bool {
    let o = ip.octets();
    // "This network" 0.0.0.0/8.
    o[0] == 0
        // Shared address space 100.64.0.0/10 (CGNAT, some metadata services).
        || (o[0] == 100 && (o[1] & 0xc0) == 64)
        // IETF protocol assignments 192.0.0.0/24.
        || (o[0] == 192 && o[1] == 0 && o[2] == 0)
        // Benchmarking 198.18.0.0/15.
        || (o[0] == 198 && (o[1] & 0xfe) == 18)
        // Reserved 240.0.0.0/4.
        || o[0] >= 240
}

/// Parses wildcard glob patterns like `*.atlassian.net`, `registry.npmjs.org` into a matches checker.
pub fn host_matches_pattern(host: &str, pattern: &str) -> bool {
    let host = host.to_lowercase();
    let pattern = pattern.to_lowercase();

    if pattern == "*" {
        return true;
    }

    if let Some(suffix) = pattern.strip_prefix("*.") {
        if host == suffix {
            return true;
        }
        if host.ends_with(&format!(".{}", suffix)) {
            return true;
        }
        return false;
    }

    host == pattern
}

/// Why a URL could not be validated and pinned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PinError {
    /// The URL or its resolved addresses violate policy (bad scheme, no host,
    /// allowlist miss, restricted IP). Retrying cannot succeed.
    Rejected(String),
    /// DNS resolution failed or returned no addresses. This may be a
    /// transient resolver outage, so callers may retry later.
    Resolution(String),
}

impl std::fmt::Display for PinError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rejected(msg) | Self::Resolution(msg) => f.write_str(msg),
        }
    }
}

impl From<PinError> for String {
    fn from(err: PinError) -> Self {
        match err {
            PinError::Rejected(msg) | PinError::Resolution(msg) => msg,
        }
    }
}

/// Turns a host name into socket addresses. Production uses [`SystemResolver`];
/// unit tests inject fixed answers so SSRF policy checks never depend on live DNS.
pub trait HostResolver {
    fn lookup(&self, host: &str, port: u16) -> io::Result<Vec<SocketAddr>>;
}

/// Resolves through the operating system (`getaddrinfo`).
pub struct SystemResolver;

impl HostResolver for SystemResolver {
    fn lookup(&self, host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
        format!("{}:{}", host, port)
            .to_socket_addrs()
            .map(Iterator::collect)
    }
}

/// Resolves a host name to its target IP addresses, ensuring that ALL resolved IPs are non-restricted.
/// Returns the first valid public IP address or an error.
pub fn resolve_and_classify_host(host: &str, port: u16) -> Result<IpAddr, String> {
    resolve_and_classify_host_typed(host, port).map_err(String::from)
}

/// Like [`resolve_and_classify_host`], but distinguishes policy rejections
/// from resolution failures.
pub fn resolve_and_classify_host_typed(host: &str, port: u16) -> Result<IpAddr, PinError> {
    resolve_and_classify_host_with(host, port, &SystemResolver)
}

fn resolve_and_classify_host_with(
    host: &str,
    port: u16,
    resolver: &dyn HostResolver,
) -> Result<IpAddr, PinError> {
    resolve_and_classify_all(host, port, resolver, &is_restricted_ip).map(|ips| ips[0])
}

/// Resolves `host` and returns every address, failing closed if any of them
/// is restricted under `restricted`. The list is never empty.
fn resolve_and_classify_all(
    host: &str,
    port: u16,
    resolver: &dyn HostResolver,
    restricted: &dyn Fn(IpAddr) -> bool,
) -> Result<Vec<IpAddr>, PinError> {
    // Treat raw IP addresses first. `Url::host_str` keeps the brackets of an
    // IPv6 literal, so strip them before parsing.
    let bare = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    if let Ok(ip) = IpAddr::from_str(bare) {
        if restricted(ip) {
            return Err(PinError::Rejected(format!(
                "Restricted IP address target: {}",
                ip
            )));
        }
        return Ok(vec![ip]);
    }

    // Resolve hostname to socket addresses
    let addrs = match resolver.lookup(host, port) {
        Ok(addrs) => addrs,
        Err(e) => {
            return Err(PinError::Resolution(format!(
                "Failed to resolve hostname '{}': {}",
                host, e
            )))
        }
    };

    let mut resolved_ips = Vec::new();
    for addr in addrs {
        let ip = addr.ip();
        if restricted(ip) {
            return Err(PinError::Rejected(format!(
                "SSRF Violation: Resolved IP address is restricted: {}",
                ip
            )));
        }
        if !resolved_ips.contains(&ip) {
            resolved_ips.push(ip);
        }
    }

    if resolved_ips.is_empty() {
        return Err(PinError::Resolution(format!(
            "No IP addresses found for hostname '{}'",
            host
        )));
    }

    Ok(resolved_ips)
}

/// Validates a URL against an allowlist and dynamic host/IP rules,
/// resolving the host and returning a Pinned connection configuration.
pub struct PinnedUrl {
    pub resolved_ip: IpAddr,
    /// Every validated address of the host (the first is `resolved_ip`).
    pub resolved_ips: Vec<IpAddr>,
    pub original_host: String,
    pub original_port: u16,
    pub final_url: Url,
}

impl PinnedUrl {
    pub fn validate_and_pin(url_str: &str, allowlist: &[String]) -> Result<Self, String> {
        Self::validate_and_pin_typed(url_str, allowlist).map_err(String::from)
    }

    /// Like [`PinnedUrl::validate_and_pin`], but distinguishes policy
    /// rejections from (possibly transient) DNS resolution failures.
    pub fn validate_and_pin_typed(url_str: &str, allowlist: &[String]) -> Result<Self, PinError> {
        Self::validate_and_pin_with(url_str, allowlist, &SystemResolver)
    }

    fn validate_and_pin_with(
        url_str: &str,
        allowlist: &[String],
        resolver: &dyn HostResolver,
    ) -> Result<Self, PinError> {
        Self::validate_and_pin_with_policy(url_str, allowlist, resolver, &is_restricted_ip)
    }

    /// Like [`PinnedUrl::validate_and_pin_typed`], with an injected resolver
    /// and address classifier (for example [`is_restricted_ip_strict`]).
    /// Every resolved address must be unrestricted under `restricted`.
    pub fn validate_and_pin_with_policy(
        url_str: &str,
        allowlist: &[String],
        resolver: &dyn HostResolver,
        restricted: &dyn Fn(IpAddr) -> bool,
    ) -> Result<Self, PinError> {
        let url =
            Url::parse(url_str).map_err(|e| PinError::Rejected(format!("Invalid URL: {}", e)))?;

        let scheme = url.scheme();
        if scheme != "http" && scheme != "https" {
            return Err(PinError::Rejected(format!(
                "Unsupported URL scheme: {}",
                scheme
            )));
        }

        let host_str = match url.host_str() {
            Some(h) => h,
            None => return Err(PinError::Rejected("URL has no host name".to_string())),
        };

        // 1. Enforce allowlist if provided
        if !allowlist.is_empty() {
            let mut matches = false;
            for pattern in allowlist {
                if host_matches_pattern(host_str, pattern) {
                    matches = true;
                    break;
                }
            }
            if !matches {
                return Err(PinError::Rejected(format!(
                    "SSRF Block: Host '{}' does not match the allowed domains",
                    host_str
                )));
            }
        }

        // Determine port
        let port = url.port_or_known_default().unwrap_or(80);

        // 2. Resolve DNS and Classify IPs
        let resolved_ips = resolve_and_classify_all(host_str, port, resolver, restricted)?;
        let resolved_ip = resolved_ips[0];

        Ok(PinnedUrl {
            resolved_ip,
            resolved_ips,
            original_host: host_str.to_string(),
            original_port: port,
            final_url: url,
        })
    }

    /// Configures a reqwest client builder with a pinned DNS resolution entry
    /// to completely defeat DNS rebinding attacks.
    pub fn apply_to_client_builder(
        &self,
        mut builder: reqwest::blocking::ClientBuilder,
    ) -> reqwest::blocking::ClientBuilder {
        let pin_addr = SocketAddr::new(self.resolved_ip, self.original_port);
        builder = builder.resolve(&self.original_host, pin_addr);
        builder
    }

    /// Async twin of [`PinnedUrl::apply_to_client_builder`]. Pins the host to
    /// every validated address (the client tries them in turn), so a v4-only
    /// or v6-only machine still connects while nothing outside the validated
    /// set can be dialled. Use a fresh client per hop.
    pub fn apply_to_async_client_builder(
        &self,
        builder: reqwest::ClientBuilder,
    ) -> reqwest::ClientBuilder {
        let addrs: Vec<SocketAddr> = self
            .resolved_ips
            .iter()
            .map(|ip| SocketAddr::new(*ip, self.original_port))
            .collect();
        builder.resolve_to_addrs(&self.original_host, &addrs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_restricted_ip() {
        // Restricted IPv4
        assert!(is_restricted_ip(IpAddr::from_str("127.0.0.1").unwrap()));
        assert!(is_restricted_ip(
            IpAddr::from_str("169.254.169.254").unwrap()
        ));
        assert!(is_restricted_ip(IpAddr::from_str("10.0.0.1").unwrap()));
        assert!(is_restricted_ip(IpAddr::from_str("172.16.50.4").unwrap()));
        assert!(is_restricted_ip(IpAddr::from_str("192.168.1.100").unwrap()));
        assert!(is_restricted_ip(IpAddr::from_str("0.0.0.0").unwrap()));

        // Restricted IPv6
        assert!(is_restricted_ip(IpAddr::from_str("::1").unwrap()));
        assert!(is_restricted_ip(
            IpAddr::from_str("::ffff:127.0.0.1").unwrap()
        ));
        assert!(is_restricted_ip(
            IpAddr::from_str("::ffff:169.254.169.254").unwrap()
        ));
        assert!(is_restricted_ip(
            IpAddr::from_str("::ffff:10.0.0.1").unwrap()
        ));
        assert!(is_restricted_ip(IpAddr::from_str("::").unwrap()));
        assert!(is_restricted_ip(IpAddr::from_str("fe80::1").unwrap()));
        assert!(is_restricted_ip(IpAddr::from_str("fc00::1").unwrap()));

        // Public IPv4 (Unrestricted)
        assert!(!is_restricted_ip(IpAddr::from_str("8.8.8.8").unwrap()));
        assert!(!is_restricted_ip(IpAddr::from_str("1.1.1.1").unwrap()));
        assert!(!is_restricted_ip(IpAddr::from_str("104.244.42.1").unwrap())); // twitter.com
    }

    #[test]
    fn typed_pin_errors_distinguish_policy_from_resolution() {
        assert!(matches!(
            PinnedUrl::validate_and_pin_typed("https://127.0.0.1/token", &[]),
            Err(PinError::Rejected(_))
        ));
        assert!(matches!(
            PinnedUrl::validate_and_pin_typed("ftp://example.com/", &[]),
            Err(PinError::Rejected(_))
        ));
        assert!(matches!(
            PinnedUrl::validate_and_pin_typed(
                "https://auth.example.com/token",
                &["other.example.com".to_string()]
            ),
            Err(PinError::Rejected(_))
        ));
        // Unresolvable host: the resolver fails, which is a resolution error
        // rather than a policy rejection. FixedResolver keeps this off live DNS.
        assert!(matches!(
            PinnedUrl::validate_and_pin_with(
                "https://auth.vam-test.invalid/token",
                &[],
                &FixedResolver
            ),
            Err(PinError::Resolution(_))
        ));
    }

    /// Answers only the hosts these tests expect. Any other name fails to
    /// resolve, so the suite never depends on the network or on live DNS.
    struct FixedResolver;

    impl HostResolver for FixedResolver {
        fn lookup(&self, host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
            match host {
                // Public documentation-style address, not in any restricted range.
                "jira.atlassian.net" => Ok(vec![SocketAddr::new(
                    IpAddr::from_str("93.184.216.34").unwrap(),
                    port,
                )]),
                _ => Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "host not in FixedResolver",
                )),
            }
        }
    }

    #[test]
    fn test_host_matches_pattern() {
        assert!(host_matches_pattern(
            "jira.atlassian.net",
            "*.atlassian.net"
        ));
        assert!(host_matches_pattern("atlassian.net", "*.atlassian.net"));
        assert!(host_matches_pattern(
            "sub.jira.atlassian.net",
            "*.atlassian.net"
        ));
        assert!(host_matches_pattern(
            "registry.npmjs.org",
            "registry.npmjs.org"
        ));
        assert!(!host_matches_pattern("other-site.com", "*.atlassian.net"));
        assert!(!host_matches_pattern(
            "atlassian.net.evil.com",
            "*.atlassian.net"
        ));
    }

    fn ip(s: &str) -> IpAddr {
        IpAddr::from_str(s).unwrap()
    }

    #[test]
    fn strict_classifier_is_a_superset_with_extra_ranges() {
        for restricted in [
            "127.0.0.1",
            "10.1.2.3",
            "169.254.169.254",
            "100.64.0.1",
            "100.100.100.200",
            "100.127.255.255",
            "0.1.2.3",
            "192.0.0.8",
            "198.18.0.1",
            "198.19.255.255",
            "240.0.0.1",
            "255.255.255.254",
            "::1",
            "::ffff:127.0.0.1",
            "::ffff:100.100.100.200",
            "::ffff:0:10.0.0.1",
            "64:ff9b::7f00:1",
            "64:ff9b::a9fe:a9fe",
            "64:ff9b:1::1",
            "2002:7f00:1::1",
            "2002:a9fe:a9fe::1",
            "2001:0:4136:e378:8000:63bf:3fff:fdd2",
            "2001:db8::1",
            "fec0::1",
            "100::1",
            "fe80::1",
            "fd00::1",
        ] {
            assert!(is_restricted_ip_strict(ip(restricted)), "{restricted}");
        }
        for public in [
            "8.8.8.8",
            "93.184.216.34",
            "100.63.255.255",
            "100.128.0.1",
            "198.17.0.1",
            "198.20.0.1",
            "64:ff9b::808:808",
            "2002:808:808::1",
            "2606:4700:4700::1111",
        ] {
            assert!(!is_restricted_ip_strict(ip(public)), "{public}");
        }
        // The base classifier is unchanged for vamd's callers.
        assert!(!is_restricted_ip(ip("100.64.0.1")));
    }

    /// Answers a fixed list per host.
    struct ListResolver(Vec<IpAddr>);

    impl HostResolver for ListResolver {
        fn lookup(&self, _host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
            Ok(self.0.iter().map(|i| SocketAddr::new(*i, port)).collect())
        }
    }

    #[test]
    fn policy_pin_keeps_every_address_and_fails_closed_on_any_restricted_one() {
        let ok = PinnedUrl::validate_and_pin_with_policy(
            "http://multi.test/x",
            &[],
            &ListResolver(vec![ip("93.184.216.34"), ip("2606:4700:4700::1111")]),
            &is_restricted_ip_strict,
        )
        .unwrap();
        assert_eq!(ok.resolved_ip, ip("93.184.216.34"));
        assert_eq!(ok.resolved_ips.len(), 2);
        // One public and one CGNAT answer: strict refuses, the base policy allows.
        let mixed = ListResolver(vec![ip("93.184.216.34"), ip("100.100.100.200")]);
        assert!(matches!(
            PinnedUrl::validate_and_pin_with_policy(
                "http://multi.test/x",
                &[],
                &mixed,
                &is_restricted_ip_strict
            ),
            Err(PinError::Rejected(_))
        ));
        assert!(PinnedUrl::validate_and_pin_with("http://multi.test/x", &[], &mixed).is_ok());
    }

    #[test]
    fn bracketed_ip_literals_are_classified_without_the_resolver() {
        let never = ListResolver(vec![]);
        for url in [
            "http://[::1]/",
            "http://[::ffff:7f00:1]/",
            "http://[::ffff:127.0.0.1]/",
            "http://2130706433/",
            "http://0177.0.0.1/",
            "http://0x7f.1/",
        ] {
            assert!(
                matches!(
                    PinnedUrl::validate_and_pin_with_policy(
                        url,
                        &[],
                        &never,
                        &is_restricted_ip_strict
                    ),
                    Err(PinError::Rejected(_))
                ),
                "{url}"
            );
        }
        let public = PinnedUrl::validate_and_pin_with_policy(
            "http://[2606:4700:4700::1111]/",
            &[],
            &never,
            &is_restricted_ip_strict,
        )
        .unwrap();
        assert_eq!(public.resolved_ips, vec![ip("2606:4700:4700::1111")]);
    }

    #[test]
    fn async_pin_helper_builds_a_client() {
        let pin = PinnedUrl::validate_and_pin_with_policy(
            "https://multi.test/x",
            &[],
            &ListResolver(vec![ip("93.184.216.34")]),
            &is_restricted_ip_strict,
        )
        .unwrap();
        assert!(pin
            .apply_to_async_client_builder(reqwest::Client::builder())
            .build()
            .is_ok());
    }

    #[test]
    fn test_pinned_url_validation() {
        let allowlist = vec![
            "*.atlassian.net".to_string(),
            "registry.npmjs.org".to_string(),
        ];

        // Valid public URL in allowlist
        let pin = PinnedUrl::validate_and_pin_with(
            "https://jira.atlassian.net/rest/api/3/myself",
            &allowlist,
            &FixedResolver,
        );
        assert!(pin.is_ok());
        let p = pin.unwrap();
        assert_eq!(p.original_host, "jira.atlassian.net");
        assert_eq!(p.original_port, 443);
        assert_eq!(p.resolved_ip, IpAddr::from_str("93.184.216.34").unwrap());

        // SSRF blocked (private IP)
        let pin_private =
            PinnedUrl::validate_and_pin_with("https://10.0.0.1/rest", &allowlist, &FixedResolver);
        assert!(pin_private.is_err());

        // IPv4-mapped IPv6 must not bypass the private-IP check even when
        // the URL's exact host is allowlisted (as for an OAuth token endpoint).
        // These are IP literals, so the system resolver parses them without DNS.
        // Require a policy rejection so a resolution failure cannot satisfy the check.
        for mapped in [
            "https://[::ffff:127.0.0.1]/token",
            "https://[::ffff:10.0.0.1]/token",
            "https://[::ffff:169.254.169.254]/token",
        ] {
            let host = Url::parse(mapped).unwrap().host_str().unwrap().to_string();
            assert!(matches!(
                PinnedUrl::validate_and_pin_typed(mapped, &[host]),
                Err(PinError::Rejected(_))
            ));
        }

        // Domain blocked (not in allowlist)
        let pin_blocked =
            PinnedUrl::validate_and_pin_with("https://google.com/", &allowlist, &FixedResolver);
        assert!(pin_blocked.is_err());
    }
}
