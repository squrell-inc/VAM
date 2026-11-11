//! URL rules for `web_fetch`: scheme, userinfo, ports, length and the
//! redirect decision. Pure functions, no I/O.

use url::{Host, Url};
use vam_security::ssrf_guard::is_restricted_ip_strict;

use crate::error::FetchError;
use crate::tools::MAX_URL_CHARS;

/// Redirect hops followed before giving up.
pub const MAX_REDIRECTS: usize = 5;

/// Parse and check a URL the model gave. `any_port` is for tests only.
pub fn parse_request_url(raw: &str, any_port: bool) -> Result<Url, FetchError> {
    let raw = raw.trim();
    if raw.chars().count() > MAX_URL_CHARS {
        return Err(FetchError::UrlTooLong);
    }
    let url = Url::parse(raw)
        .map_err(|_| FetchError::InvalidUrl("the URL could not be parsed".to_string()))?;
    check_url(&url, any_port)?;
    Ok(url)
}

/// Scheme, host, userinfo and port rules, applied to the first URL and to
/// every redirect target.
pub fn check_url(url: &Url, any_port: bool) -> Result<(), FetchError> {
    if url.as_str().chars().count() > MAX_URL_CHARS {
        return Err(FetchError::UrlTooLong);
    }
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err(FetchError::InvalidUrl(
            "only http and https URLs are supported".to_string(),
        ));
    }
    if url.host().is_none() {
        return Err(FetchError::InvalidUrl(
            "the URL has no host name".to_string(),
        ));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(FetchError::InvalidUrl(
            "the URL contains a user name or password".to_string(),
        ));
    }
    let port = url.port_or_known_default().unwrap_or(0);
    if !any_port && port != 80 && port != 443 {
        return Err(FetchError::InvalidUrl(format!(
            "port {port} is not allowed (only 80 and 443)"
        )));
    }
    Ok(())
}

/// Host with case, a trailing dot and one leading `www.` removed.
pub fn host_key(url: &Url) -> String {
    let host = url.host_str().unwrap_or("").to_ascii_lowercase();
    let host = host.trim_end_matches('.');
    host.strip_prefix("www.").unwrap_or(host).to_string()
}

/// What to do with a redirect from `from` to `to`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedirectKind {
    /// Same host or its `www.` variant over http(s): follow.
    SameHost,
    /// Another host: return `redirected` to the model.
    CrossHost,
    /// Same host but https to http: refuse.
    Downgrade,
}

pub fn classify_redirect(from: &Url, to: &Url) -> RedirectKind {
    if host_key(from) != host_key(to) {
        RedirectKind::CrossHost
    } else if from.scheme() == "https" && to.scheme() == "http" {
        RedirectKind::Downgrade
    } else {
        RedirectKind::SameHost
    }
}

/// True when the URL names an IP address (in any notation `Url` normalizes,
/// such as `2130706433` or `[::ffff:7f00:1]`) that the strict policy refuses.
pub fn literal_ip_restricted(url: &Url) -> bool {
    match url.host() {
        Some(Host::Ipv4(ip)) => is_restricted_ip_strict(ip.into()),
        Some(Host::Ipv6(ip)) => is_restricted_ip_strict(ip.into()),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(u: &str) -> Url {
        parse_request_url(u, false).expect("ok")
    }

    #[test]
    fn accepts_plain_http_and_https() {
        ok("http://example.com/a?b=1#frag");
        ok("https://example.com:443/");
        ok("http://example.com:80/");
    }

    #[test]
    fn rejects_bad_urls_with_typed_errors() {
        let long = format!("https://example.com/{}", "a".repeat(2040));
        assert_eq!(parse_request_url(&long, false), Err(FetchError::UrlTooLong));
        for bad in [
            "ftp://example.com/",
            "file:///etc/passwd",
            "javascript:alert(1)",
            "https://user:pw@example.com/",
            "https://user@example.com/",
            "http://example.com:8080/",
            "https://example.com:8443/",
            "not a url",
            "",
        ] {
            assert!(
                matches!(
                    parse_request_url(bad, false),
                    Err(FetchError::InvalidUrl(_))
                ),
                "{bad}"
            );
        }
        assert!(parse_request_url("http://example.com:8080/", true).is_ok());
    }

    #[test]
    fn exact_length_limit() {
        let base = "https://example.com/";
        let at = format!("{base}{}", "a".repeat(2048 - base.len()));
        assert!(parse_request_url(&at, false).is_ok());
        let over = format!("{at}a");
        assert_eq!(parse_request_url(&over, false), Err(FetchError::UrlTooLong));
    }

    #[test]
    fn redirect_classification() {
        let u = |s: &str| Url::parse(s).expect("url");
        assert_eq!(
            classify_redirect(&u("http://a.test/"), &u("https://a.test/x")),
            RedirectKind::SameHost
        );
        assert_eq!(
            classify_redirect(&u("http://a.test/"), &u("http://www.a.test/")),
            RedirectKind::SameHost
        );
        assert_eq!(
            classify_redirect(&u("https://www.a.test/"), &u("https://A.TEST./")),
            RedirectKind::SameHost
        );
        assert_eq!(
            classify_redirect(&u("https://a.test/"), &u("http://a.test/")),
            RedirectKind::Downgrade
        );
        assert_eq!(
            classify_redirect(&u("https://a.test/"), &u("https://b.test/")),
            RedirectKind::CrossHost
        );
        // A look-alike suffix is another host.
        assert_eq!(
            classify_redirect(&u("https://a.test/"), &u("https://a.test.evil.example/")),
            RedirectKind::CrossHost
        );
        assert_eq!(
            classify_redirect(&u("https://a.test/"), &u("https://sub.a.test/")),
            RedirectKind::CrossHost
        );
    }

    #[test]
    fn literal_ip_notations_are_caught() {
        for restricted in [
            "http://127.0.0.1/",
            "http://2130706433/",
            "http://0177.0.0.1/",
            "http://0x7f.1/",
            "http://0x7f000001/",
            "http://127.1/",
            "http://169.254.169.254/latest/meta-data/",
            "http://100.100.100.200/",
            "http://[::1]/",
            "http://[::ffff:7f00:1]/",
            "http://[::ffff:127.0.0.1]/",
            "http://[::ffff:a9fe:a9fe]/",
            "http://[fe80::1]/",
            "http://[fd00::1]/",
            "http://10.0.0.1/",
            "http://192.168.0.1/",
            "http://0.0.0.0/",
        ] {
            let url = Url::parse(restricted).expect("url");
            assert!(literal_ip_restricted(&url), "{restricted}");
        }
        for public in ["http://93.184.216.34/", "http://[2606:4700:4700::1111]/"] {
            assert!(!literal_ip_restricted(&Url::parse(public).expect("url")));
        }
        assert!(!literal_ip_restricted(
            &Url::parse("http://a.test/").expect("url")
        ));
    }
}
