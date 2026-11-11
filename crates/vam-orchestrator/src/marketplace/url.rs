//! The URL of a marketplace, and which addresses vamd may reach to read it.
//!
//! The URL is typed by an org admin, so it is untrusted input to a subprocess that opens a network
//! connection from the node. Two layers:
//!
//! 1. [`check_url`] (syntax, no network): `https` only, a real DNS name, no user info, no port other
//!    than 443, no query or fragment, a plain path. It returns the canonical form that is stored and
//!    handed to `git`.
//! 2. [`check_host_public`] (one DNS lookup): every address the name resolves to must be public. The
//!    authoritative check is made again at connect time by the guard proxy ([`super::guard`]), which
//!    dials the address it checked, so a name that answers differently the second time (DNS
//!    rebinding) still cannot reach an internal host.

use std::net::{IpAddr, Ipv4Addr};
use vam_security::ssrf_guard::{is_restricted_ip_strict, HostResolver, SystemResolver};

/// The longest URL accepted.
pub const MAX_URL_LEN: usize = 512;
/// The only port a marketplace may be served on.
pub const HTTPS_PORT: u16 = 443;

/// A marketplace URL that passed [`check_url`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarketplaceUrl {
    /// `https://<host><path>`: lowercase host, no port, no trailing slash. Stored and given to git.
    pub canonical: String,
    pub host: String,
    /// The key two URLs of the same repository share: lowercase, without `.git`.
    pub dedup_key: String,
}

/// Why a URL (or a host) is refused. The text is shown to the admin.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UrlError {
    #[error("marketplace URL {0}")]
    Bad(String),
    #[error("marketplace host `{host}` {why}")]
    Host { host: String, why: String },
}

fn bad(why: &str) -> UrlError {
    UrlError::Bad(why.to_string())
}

/// Host names that name this machine or a private network. A fast refusal with a clear message;
/// the address check is the real gate.
const INTERNAL_SUFFIXES: &[&str] = &[
    ".localhost",
    ".local",
    ".localdomain",
    ".internal",
    ".intranet",
    ".corp",
    ".home",
    ".lan",
    ".private",
    ".svc",
    ".home.arpa",
];

fn check_host_name(host: &str) -> Result<(), UrlError> {
    let why = |w: &str| UrlError::Host {
        host: host.to_string(),
        why: w.to_string(),
    };
    if host.len() > 253 || host.ends_with('.') {
        return Err(why("is not a valid host name"));
    }
    let labels: Vec<&str> = host.split('.').collect();
    if labels.len() < 2 {
        return Err(why("must be a full domain name, not a bare name"));
    }
    for l in &labels {
        let ok = !l.is_empty()
            && l.len() <= 63
            && !l.starts_with('-')
            && !l.ends_with('-')
            && l.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
        if !ok {
            return Err(why(
                "is not a valid host name (lowercase letters, digits and '-')",
            ));
        }
    }
    // The last label of a real name is never all digits (that is an IPv4 address in some form).
    if labels
        .last()
        .is_some_and(|t| t.bytes().all(|b| b.is_ascii_digit()))
    {
        return Err(why("looks like an IP address; use a host name"));
    }
    let dotted = format!(".{host}");
    if host == "localhost" || INTERNAL_SUFFIXES.iter().any(|s| dotted.ends_with(s)) {
        return Err(why("names this machine or a private network"));
    }
    Ok(())
}

fn check_path(path: &str) -> Result<(), UrlError> {
    if path.len() <= 1 {
        return Err(bad("must name a repository (a path after the host)"));
    }
    if path.len() > 300 {
        return Err(bad("path is too long"));
    }
    if !path
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"/._~-".contains(&b))
    {
        return Err(bad(
            "path may hold only letters, digits and '/', '.', '_', '~', '-'",
        ));
    }
    for seg in path.trim_start_matches('/').split('/') {
        if seg.is_empty() || seg == "." || seg == ".." || seg.starts_with('-') {
            return Err(bad("path has an empty, '.', '..' or '-' segment"));
        }
    }
    Ok(())
}

/// Syntax check of a marketplace URL; no network. See the module docs for what it refuses.
pub fn check_url(raw: &str) -> Result<MarketplaceUrl, UrlError> {
    if raw.is_empty() || raw.len() > MAX_URL_LEN {
        return Err(bad("must be 1 to 512 characters"));
    }
    if raw.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err(bad("must not hold spaces or control characters"));
    }
    let u = url::Url::parse(raw).map_err(|_| bad("is not a valid URL"))?;
    if u.scheme() != "https" {
        return Err(bad("must start with https:// (no other scheme is fetched)"));
    }
    if !u.username().is_empty() || u.password().is_some() {
        return Err(bad("must not hold a user name or password"));
    }
    if u.query().is_some() || u.fragment().is_some() {
        return Err(bad("must not hold a query or a fragment"));
    }
    // `port()` is None for the default port, so Some(_) here is never 443.
    if u.port().is_some() {
        return Err(bad("must use the standard https port (443)"));
    }
    let host = match u.host() {
        Some(url::Host::Domain(d)) => d.to_string(),
        Some(_) => return Err(bad("must use a host name, not an IP address")),
        None => return Err(bad("has no host")),
    };
    check_host_name(&host)?;
    let path = u.path().trim_end_matches('/').to_string();
    check_path(&path)?;
    let canonical = format!("https://{host}{path}");
    let dedup_key = format!(
        "https://{host}{}",
        path.trim_end_matches(".git").to_ascii_lowercase()
    );
    Ok(MarketplaceUrl {
        canonical,
        host,
        dedup_key,
    })
}

/// True for an address a node must never dial for a marketplace: everything the SSRF guard's strict
/// classifier refuses (private, loopback, link-local, ULA, CGNAT, multicast, documentation, and the
/// IPv6 forms that embed an IPv4 address), plus the Azure wire server.
pub fn is_refused_ip(ip: IpAddr) -> bool {
    if is_restricted_ip_strict(ip) {
        return true;
    }
    // Azure's wire server / metadata helper 168.63.129.16 is a public-looking address.
    matches!(ip, IpAddr::V4(v4) if v4 == Ipv4Addr::new(168, 63, 129, 16))
}

/// Resolves `host` and refuses it unless every address is public. The authoritative check is the
/// guard proxy's, at connect time; this one gives the admin a clear error before `git` starts.
pub fn check_host_public(host: &str, resolver: &dyn HostResolver) -> Result<(), UrlError> {
    let addrs = resolver
        .lookup(host, HTTPS_PORT)
        .map_err(|_| UrlError::Host {
            host: host.to_string(),
            why: "could not be resolved".into(),
        })?;
    if addrs.is_empty() {
        return Err(UrlError::Host {
            host: host.to_string(),
            why: "did not resolve to any address".into(),
        });
    }
    if let Some(a) = addrs.iter().find(|a| is_refused_ip(a.ip())) {
        return Err(UrlError::Host {
            host: host.to_string(),
            why: format!(
                "resolves to {}, which is not a public address (private, loopback, link-local and metadata addresses are refused)",
                a.ip()
            ),
        });
    }
    Ok(())
}

/// [`check_host_public`] through the system resolver.
pub fn check_host_public_system(host: &str) -> Result<(), UrlError> {
    check_host_public(host, &SystemResolver)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    struct Fixed(Vec<IpAddr>);
    impl HostResolver for Fixed {
        fn lookup(&self, _host: &str, port: u16) -> std::io::Result<Vec<SocketAddr>> {
            Ok(self.0.iter().map(|ip| SocketAddr::new(*ip, port)).collect())
        }
    }

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn a_plain_https_url_is_canonicalised() {
        let u = check_url("https://GitHub.com:443/Squrell-Inc/Vam-Marketplace.git/").unwrap();
        assert_eq!(
            u.canonical,
            "https://github.com/Squrell-Inc/Vam-Marketplace.git"
        );
        assert_eq!(u.host, "github.com");
        assert_eq!(
            u.dedup_key,
            "https://github.com/squrell-inc/vam-marketplace"
        );
        assert_eq!(
            check_url("https://github.com/squrell-inc/vam-marketplace")
                .unwrap()
                .dedup_key,
            u.dedup_key,
            "with and without .git is one repository"
        );
    }

    #[test]
    fn other_schemes_are_refused() {
        for raw in [
            "http://github.com/a/b",
            "ssh://git@github.com/a/b",
            "git://github.com/a/b",
            "file:///srv/repo",
            "ext::sh -c id",
            "git@github.com:a/b.git",
            "/srv/repo",
            "ftp://github.com/a/b",
            "HTTP://github.com/a/b",
        ] {
            assert!(check_url(raw).is_err(), "{raw}");
        }
    }

    #[test]
    fn user_info_ports_queries_and_fragments_are_refused() {
        for raw in [
            "https://user@github.com/a/b",
            "https://user:pw@github.com/a/b",
            "https://:pw@github.com/a/b",
            "https://github.com:8443/a/b",
            "https://github.com:80/a/b",
            "https://github.com/a/b?x=1",
            "https://github.com/a/b#frag",
            "https://github.com/a b",
            "https://github.com/a\nb",
        ] {
            assert!(check_url(raw).is_err(), "{raw}");
        }
        // An explicit 443 is the default port and is accepted.
        assert!(check_url("https://github.com:443/a/b").is_ok());
    }

    #[test]
    fn ip_literals_in_every_spelling_are_refused() {
        for raw in [
            "https://127.0.0.1/a/b",
            "https://10.0.0.1/a/b",
            "https://169.254.169.254/latest/meta-data",
            "https://[::1]/a/b",
            "https://[::ffff:127.0.0.1]/a/b",
            "https://[fd00:ec2::254]/a/b",
            "https://2130706433/a/b",
            "https://0x7f.1/a/b",
            "https://0177.0.0.1/a/b",
            "https://8.8.8.8/a/b",
            "https://1.2.3.4.5/a/b",
        ] {
            assert!(check_url(raw).is_err(), "{raw}");
        }
    }

    #[test]
    fn internal_names_and_bare_names_are_refused() {
        for raw in [
            "https://localhost/a/b",
            "https://app.localhost/a/b",
            "https://git/a/b",
            "https://git.corp/a/b",
            "https://gitea.internal/a/b",
            "https://nas.local/a/b",
            "https://host.svc/a/b",
            "https://router.home.arpa/a/b",
            "https://github.com./a/b",
        ] {
            assert!(check_url(raw).is_err(), "{raw}");
        }
    }

    #[test]
    fn paths_must_be_plain() {
        for raw in [
            "https://github.com",
            "https://github.com/",
            "https://github.com/a/b%2Fc",
            "https://github.com/a/b%2fc",
            "https://github.com/a/b%00c",
            "https://github.com/a/-b",
            "https://github.com/a//b",
            "https://github.com/a/b;c",
            "https://github.com/a/b'c",
        ] {
            assert!(check_url(raw).is_err(), "{raw}");
        }
        // Dot segments (also percent-encoded) are folded by the URL parser before we see them.
        for raw in [
            "https://github.com/a/../b/c",
            "https://github.com/a/%2e%2e/b/c",
        ] {
            assert_eq!(check_url(raw).unwrap().canonical, "https://github.com/b/c");
        }
    }

    #[test]
    fn restricted_addresses_are_refused_in_every_form() {
        for a in [
            "127.0.0.1",
            "127.1.2.3",
            "10.1.2.3",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.0.1",
            "169.254.169.254",
            "169.254.0.1",
            "100.100.100.200",
            "100.64.0.1",
            "0.0.0.0",
            "224.0.0.1",
            "255.255.255.255",
            "240.0.0.1",
            "168.63.129.16",
            "::1",
            "::",
            "fe80::1",
            "fd00:ec2::254",
            "fc00::1",
            "ff02::1",
            "::ffff:10.0.0.1",
            "::ffff:127.0.0.1",
            "::ffff:169.254.169.254",
            "64:ff9b::7f00:1",
            "2002:7f00:1::1",
            "2001:0:4136:e378:8000:63bf:3fff:fdd2",
            "2001:db8::1",
        ] {
            assert!(is_refused_ip(ip(a)), "{a} must be refused");
        }
        for a in ["140.82.112.3", "8.8.8.8", "2606:4700:4700::1111", "1.1.1.1"] {
            assert!(!is_refused_ip(ip(a)), "{a} is public");
        }
    }

    #[test]
    fn one_private_answer_among_public_ones_refuses_the_host() {
        let mixed = Fixed(vec![ip("140.82.112.3"), ip("10.0.0.5")]);
        let err = check_host_public("evil.example.com", &mixed).unwrap_err();
        assert!(err.to_string().contains("10.0.0.5"), "{err}");
        let public = Fixed(vec![ip("140.82.112.3"), ip("2606:4700:4700::1111")]);
        assert!(check_host_public("good.example.com", &public).is_ok());
        let metadata = Fixed(vec![ip("169.254.169.254")]);
        assert!(check_host_public("meta.example.com", &metadata).is_err());
        let none = Fixed(vec![]);
        assert!(check_host_public("none.example.com", &none).is_err());
    }
}
