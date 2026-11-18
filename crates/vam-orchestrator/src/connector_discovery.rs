//! Host-side discovery of a connector's OAuth endpoints (CN-5.2).
//!
//! When an org admin saves an admin-supplied client, `vamd` reads the provider's metadata from the
//! connector's MCP URL (RFC 9728 protected-resource metadata, then RFC 8414 / OpenID Connect
//! authorization-server metadata) and pins the endpoints on the node row. This is the node's first
//! fetch of a URL a connector file (possibly a community one) names, so it is guarded:
//!
//! - every URL is `https`, a DNS name (no IP literal), no user info, no fragment, port 443 only
//!   ([`check_endpoint`]);
//! - every fetch resolves the host once, requires every address to be public under the strict
//!   classifier (`ssrf_guard::is_restricted_ip_strict`), and dials only that pinned address
//!   (`PinnedUrl`), so a name that answers differently later cannot reach an internal host;
//! - no redirects, no environment proxy, 8 s per request and 20 s for the whole discovery, at most
//!   64 KiB per answer, JSON only;
//! - the result is checked before it is used: the protected-resource document names the resource
//!   its well-known URL was built from (RFC 9728 §3.3), the authorization server's `issuer` is the
//!   one asked for, S256 is offered, a connector that keeps a secret finds a token-endpoint
//!   authentication method it can use (pinned, see [`Discovered::token_auth_method`]), and the
//!   endpoints pass [`check_endpoint`].
//!
//! Errors carry fixed text and a host name, never a response body. The secret is not known here.
//!
//! Test seam: in a node started with `VAMD_PROCESS_CONNECTOR_ENDPOINTS` (feature `process-guest`),
//! URLs it names reach the runner's loopback mock and nothing else changes.

use std::io::Read;
use std::time::{Duration, Instant};

use serde_json::Value;
use url::{Host, Url};
use vam_security::ssrf_guard::{is_restricted_ip_strict, PinError, PinnedUrl, SystemResolver};

use crate::connector_client::TokenAuthMethod;

const MAX_BODY_BYTES: u64 = 64 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(8);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(4);
const TOTAL_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_URL_LEN: usize = 512;

/// What discovery pins: the endpoints a member's sign-in and the node's exchange use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Discovered {
    pub issuer: String,
    pub authorize_endpoint: String,
    pub token_endpoint: String,
    /// Only when it is on the token endpoint's host (the secret is sent there and nowhere else).
    pub revocation_endpoint: Option<String>,
    /// How the org's client secret authenticates at the token and revocation endpoints: POST when
    /// the provider lists it, else Basic when it lists it or says nothing (RFC 8414 §2: an omitted
    /// `token_endpoint_auth_methods_supported` means `client_secret_basic`). Without a secret there
    /// is nothing to authenticate and this is the default, unused.
    pub token_auth_method: TokenAuthMethod,
}

/// Why discovery failed. `Unreachable` may heal on a retry; `Unsuitable` is the provider's (or the
/// connector's) metadata being unusable for this node.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DiscoveryError {
    #[error("{0}")]
    Unreachable(String),
    #[error("{0}")]
    Unsuitable(String),
}

fn unsuitable(why: impl Into<String>) -> DiscoveryError {
    DiscoveryError::Unsuitable(why.into())
}

/// One JSON `GET`. The production implementation is [`GuardedFetcher`]; tests inject answers.
pub trait MetadataFetcher {
    /// `Unreachable` for a network failure, a non-200 answer or an answer that is not JSON.
    fn get_json(&self, url: &Url) -> Result<Value, DiscoveryError>;
    /// Whether `url`'s host resolves only to public addresses (no request is made).
    fn check_public_host(&self, url: &Url) -> Result<(), DiscoveryError>;
}

/// The syntax rules for every URL discovery fetches or pins.
pub fn check_endpoint(raw: &str, what: &str) -> Result<Url, DiscoveryError> {
    let bad = |why: &str| unsuitable(format!("the {what} {why}"));
    if raw.is_empty() || raw.len() > MAX_URL_LEN {
        return Err(bad("is missing or too long"));
    }
    if raw.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err(bad("holds spaces or control characters"));
    }
    let u = Url::parse(raw).map_err(|_| bad("is not a valid URL"))?;
    if u.scheme() != "https" {
        return Err(bad("must be an https URL"));
    }
    if !u.username().is_empty() || u.password().is_some() {
        return Err(bad("must not hold a user name or password"));
    }
    if u.fragment().is_some() {
        return Err(bad("must not hold a fragment"));
    }
    match u.host() {
        Some(Host::Domain(d)) if d.contains('.') && !d.ends_with('.') => {}
        Some(Host::Domain(_)) => return Err(bad("must name a full domain")),
        _ => return Err(bad("must use a host name, not an IP address")),
    }
    if u.port().is_some_and(|p| p != 443) {
        return Err(bad("must use port 443"));
    }
    Ok(u)
}

/// The host of a checked endpoint (lowercase, no port).
pub fn host_of(raw: &str) -> Option<String> {
    Url::parse(raw)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
}

/// Discovers the endpoints for `mcp_url`. `keeps_secret`: the connector keeps a client secret, so
/// the provider must take it by `client_secret_post` or `client_secret_basic` (and one is pinned).
pub fn discover(
    mcp_url: &str,
    keeps_secret: bool,
    fetcher: &dyn MetadataFetcher,
) -> Result<Discovered, DiscoveryError> {
    let started = Instant::now();
    let mcp = check_endpoint(mcp_url, "connector's MCP URL")?;
    let issuer = protected_resource_issuer(&mcp, fetcher, started)?;
    let meta = authorization_server_metadata(&issuer, fetcher, started)?;
    let found = pin_endpoints(&issuer, &meta, keeps_secret)?;
    // Fail now, with the secret not yet stored, if the token host is not a public one.
    let token = check_endpoint(&found.token_endpoint, "token endpoint")?;
    fetcher.check_public_host(&token)?;
    Ok(found)
}

fn out_of_time(started: Instant) -> Result<(), DiscoveryError> {
    if started.elapsed() > TOTAL_TIMEOUT {
        return Err(DiscoveryError::Unreachable(
            "reading the provider's metadata took too long".into(),
        ));
    }
    Ok(())
}

fn well_known(base: &Url, suffix: &str, path: &str) -> Result<Url, DiscoveryError> {
    let mut u = base.clone();
    u.set_query(None);
    let path = path.trim_end_matches('/');
    u.set_path(&format!("/.well-known/{suffix}{path}"));
    Ok(u)
}

/// RFC 9728: the protected-resource metadata names the authorization server. Tries the path
/// suffixed form first (`/.well-known/oauth-protected-resource/mcp`, with the MCP URL's query if it
/// has one), then the root (no query). Each candidate
/// carries the resource identifier its URL was built from, and the document must name exactly that
/// one (RFC 9728 §3.3): a document for another path, another server or no resource at all is not
/// this server's metadata. The comparison is exact: `/mcp/` is not `/mcp`. The one spelling
/// difference accepted is an empty path against `/` on an identifier that is only an origin, which
/// RFC 3986 §6.2.3 makes the same URI for `https` (`https://h` and `https://h/`).
fn protected_resource_issuer(
    mcp: &Url,
    fetcher: &dyn MetadataFetcher,
    started: Instant,
) -> Result<String, DiscoveryError> {
    // The query is part of the resource identifier (RFC 9728 §3.1): the resource-specific
    // well-known URL keeps it after the inserted path, and the document must name it.
    let identifier = mcp.as_str().to_string();
    let mut specific = well_known(mcp, "oauth-protected-resource", mcp.path())?;
    specific.set_query(mcp.query());
    let root_identifier = format!("{}://{}", mcp.scheme(), mcp.host_str().unwrap_or_default());
    let mut candidates = vec![(specific, identifier)];
    if !mcp.path().trim_end_matches('/').is_empty() {
        candidates.push((
            well_known(mcp, "oauth-protected-resource", "")?,
            root_identifier,
        ));
    }
    let mut last = DiscoveryError::Unreachable(format!(
        "{} publishes no OAuth protected-resource metadata",
        mcp.host_str().unwrap_or("the MCP server")
    ));
    for (url, expected) in candidates {
        out_of_time(started)?;
        let doc = match fetcher.get_json(&url) {
            Ok(d) => d,
            Err(e) => {
                last = e;
                continue;
            }
        };
        let named = doc.get("resource").and_then(Value::as_str);
        if !named.is_some_and(|r| same_resource(r, &expected)) {
            return Err(unsuitable(
                "the MCP server's protected-resource metadata does not name this server as its resource",
            ));
        }
        let first = doc
            .get("authorization_servers")
            .and_then(Value::as_array)
            .and_then(|a| a.iter().filter_map(Value::as_str).next())
            .ok_or_else(|| unsuitable("the MCP server names no authorization server"))?;
        let issuer = check_endpoint(first, "authorization server")?;
        if issuer.query().is_some() {
            return Err(unsuitable("the authorization server must not hold a query"));
        }
        // The identifier as the provider named it, not as a URL parser spells it: RFC 8414 compares
        // the metadata's `issuer` to it exactly (a trailing `/` is part of a tenant's identifier).
        return Ok(first.to_string());
    }
    Err(last)
}

/// RFC 9728 §3.3: the metadata's `resource` must be identical to the identifier the well-known URL
/// was built from. Only an origin may be spelled with or without its `/` path (RFC 3986 §6.2.3).
fn same_resource(named: &str, expected: &str) -> bool {
    if named == expected {
        return true;
    }
    let origin_only = |s: &str| {
        Url::parse(s)
            .is_ok_and(|u| u.path() == "/" && u.query().is_none() && u.fragment().is_none())
    };
    // With the path exactly `/`, each side is the origin with or without that one `/`.
    origin_only(named)
        && origin_only(expected)
        && named.trim_end_matches('/') == expected.trim_end_matches('/')
}

/// RFC 8414, then OpenID Connect discovery, in the order the MCP authorization spec gives.
fn authorization_server_metadata(
    issuer: &str,
    fetcher: &dyn MetadataFetcher,
    started: Instant,
) -> Result<Value, DiscoveryError> {
    let base = check_endpoint(issuer, "authorization server")?;
    let path = base.path().trim_end_matches('/').to_string();
    let mut candidates = vec![
        well_known(&base, "oauth-authorization-server", &path)?,
        well_known(&base, "openid-configuration", &path)?,
    ];
    if !path.is_empty() {
        let mut oidc = base.clone();
        oidc.set_path(&format!("{path}/.well-known/openid-configuration"));
        candidates.push(oidc);
    }
    let mut last = DiscoveryError::Unreachable(format!(
        "{} publishes no authorization-server metadata",
        base.host_str().unwrap_or("the authorization server")
    ));
    for url in candidates {
        out_of_time(started)?;
        match fetcher.get_json(&url) {
            Ok(doc) => {
                let said = doc.get("issuer").and_then(Value::as_str).unwrap_or("");
                if said != issuer {
                    last =
                        unsuitable("the authorization server's metadata names a different issuer");
                    continue;
                }
                return Ok(doc);
            }
            // A usable-looking answer that was refused is the more useful reason to show.
            Err(e) if !matches!(last, DiscoveryError::Unsuitable(_)) => last = e,
            Err(_) => {}
        }
    }
    Err(last)
}

/// The token-endpoint authentication method a connector that keeps a secret will use (RFC 8414 §2).
fn choose_auth_method(meta: &Value) -> Result<TokenAuthMethod, DiscoveryError> {
    let refuse = || {
        unsuitable(
            "the provider's token endpoint accepts neither client_secret_post nor client_secret_basic",
        )
    };
    match meta.get("token_endpoint_auth_methods_supported") {
        // Omitted: RFC 8414 says `client_secret_basic`.
        None | Some(Value::Null) => Ok(TokenAuthMethod::ClientSecretBasic),
        Some(Value::Array(listed)) => {
            let listed: Vec<&str> = listed.iter().filter_map(Value::as_str).collect();
            if listed.contains(&"client_secret_post") {
                Ok(TokenAuthMethod::ClientSecretPost)
            } else if listed.contains(&"client_secret_basic") {
                Ok(TokenAuthMethod::ClientSecretBasic)
            } else {
                Err(refuse())
            }
        }
        Some(_) => Err(refuse()),
    }
}

fn pin_endpoints(
    issuer: &str,
    meta: &Value,
    keeps_secret: bool,
) -> Result<Discovered, DiscoveryError> {
    let strings = |key: &str| -> Option<Vec<&str>> {
        meta.get(key)
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).collect())
    };
    if !strings("code_challenge_methods_supported").is_some_and(|m| m.contains(&"S256")) {
        return Err(unsuitable(
            "the provider does not offer PKCE with S256, which every sign-in here requires",
        ));
    }
    let token_auth_method = if keeps_secret {
        choose_auth_method(meta)?
    } else {
        TokenAuthMethod::default()
    };
    let field = |key: &str, what: &str| -> Result<Url, DiscoveryError> {
        let raw = meta.get(key).and_then(Value::as_str).unwrap_or("");
        check_endpoint(raw, what)
    };
    let authorize = field("authorization_endpoint", "authorization endpoint")?;
    let token = field("token_endpoint", "token endpoint")?;
    let revocation = match meta.get("revocation_endpoint").and_then(Value::as_str) {
        Some(raw) => check_endpoint(raw, "revocation endpoint")
            .ok()
            .filter(|r| r.host_str() == token.host_str()),
        None => None,
    };
    Ok(Discovered {
        issuer: issuer.to_string(),
        authorize_endpoint: authorize.to_string(),
        token_endpoint: token.to_string(),
        revocation_endpoint: revocation.map(|r| r.to_string()),
        token_auth_method,
    })
}

/// The production fetcher: pinned to public addresses, no redirects, no proxy, bounded.
pub struct GuardedFetcher;

fn classify(e: PinError) -> DiscoveryError {
    match e {
        PinError::Rejected(_) => unsuitable("the provider's host is not a public address"),
        PinError::Resolution(_) => {
            DiscoveryError::Unreachable("the provider's host could not be resolved".into())
        }
    }
}

fn client(pinned: Option<&PinnedUrl>) -> Result<reqwest::blocking::Client, DiscoveryError> {
    let mut b = reqwest::blocking::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .connect_timeout(CONNECT_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        // A proxy resolves the destination itself and would bypass the pin.
        .no_proxy();
    if let Some(p) = pinned {
        b = p.apply_to_client_builder(b);
    }
    b.build()
        .map_err(|_| DiscoveryError::Unreachable("could not start an HTTP client".into()))
}

fn pin(url: &Url) -> Result<PinnedUrl, DiscoveryError> {
    let host = url.host_str().unwrap_or_default().to_string();
    PinnedUrl::validate_and_pin_with_policy(
        url.as_str(),
        &[host],
        &SystemResolver,
        &is_restricted_ip_strict,
    )
    .map_err(classify)
}

/// Unit-test answers for [`GuardedFetcher`], by exact URL, so a route test reads no network.
/// `None` is an unreachable provider. Tests use hosts of their own so they never share an answer.
#[cfg(test)]
pub(crate) mod test_seam {
    use serde_json::Value;
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    static ANSWERS: Mutex<BTreeMap<String, Option<Value>>> = Mutex::new(BTreeMap::new());

    pub(crate) fn answer(url: &str, v: Option<Value>) {
        ANSWERS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(url.to_string(), v);
    }

    pub(crate) fn lookup(url: &str) -> Option<Option<Value>> {
        ANSWERS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(url)
            .cloned()
    }

    pub(crate) fn knows_host(host: &str) -> bool {
        ANSWERS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .any(|u| u.starts_with(&format!("https://{host}/")))
    }
}

impl MetadataFetcher for GuardedFetcher {
    fn check_public_host(&self, url: &Url) -> Result<(), DiscoveryError> {
        #[cfg(test)]
        if test_seam::knows_host(url.host_str().unwrap_or_default()) {
            return Ok(());
        }
        #[cfg(feature = "process-guest")]
        if crate::process_mode::map_connector_url(url.as_str()).is_some() {
            return Ok(());
        }
        pin(url).map(|_| ())
    }

    fn get_json(&self, url: &Url) -> Result<Value, DiscoveryError> {
        let host = url.host_str().unwrap_or("the provider").to_string();
        let unreachable = |why: &str| DiscoveryError::Unreachable(format!("{host}: {why}"));
        #[cfg(test)]
        if let Some(answer) = test_seam::lookup(url.as_str()) {
            return answer.ok_or_else(|| unreachable("the request failed"));
        }
        // Test-node seam: a URL the node was told about reaches the runner's loopback mock.
        #[cfg(feature = "process-guest")]
        let seam = crate::process_mode::map_connector_url(url.as_str());
        #[cfg(not(feature = "process-guest"))]
        let seam: Option<Url> = None;
        let (http, target) = match seam {
            Some(mapped) => (client(None)?, mapped),
            None => {
                let pinned = pin(url)?;
                (client(Some(&pinned))?, pinned.final_url.clone())
            }
        };
        let response = http
            .get(target)
            .header("accept", "application/json")
            .send()
            .map_err(|_| unreachable("the request failed"))?;
        if response.status().as_u16() != 200 {
            return Err(unreachable("the provider did not answer with metadata"));
        }
        let mut body = Vec::new();
        response
            .take(MAX_BODY_BYTES + 1)
            .read_to_end(&mut body)
            .map_err(|_| unreachable("the answer could not be read"))?;
        if body.len() as u64 > MAX_BODY_BYTES {
            return Err(unreachable("the answer is too large"));
        }
        serde_json::from_slice(&body).map_err(|_| unreachable("the answer is not JSON"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::cell::RefCell;
    use std::collections::BTreeMap;

    /// Answers by exact URL and records every request.
    struct Fake {
        answers: BTreeMap<String, Value>,
        asked: RefCell<Vec<String>>,
        private_hosts: Vec<&'static str>,
    }

    impl Fake {
        fn new(answers: &[(&str, Value)]) -> Self {
            Self {
                answers: answers
                    .iter()
                    .map(|(u, v)| (u.to_string(), v.clone()))
                    .collect(),
                asked: RefCell::new(vec![]),
                private_hosts: vec![],
            }
        }
    }

    impl MetadataFetcher for Fake {
        fn get_json(&self, url: &Url) -> Result<Value, DiscoveryError> {
            self.asked.borrow_mut().push(url.to_string());
            self.answers
                .get(url.as_str())
                .cloned()
                .ok_or_else(|| DiscoveryError::Unreachable("no answer".into()))
        }
        fn check_public_host(&self, url: &Url) -> Result<(), DiscoveryError> {
            if self.private_hosts.contains(&url.host_str().unwrap_or("")) {
                return Err(unsuitable("the provider's host is not a public address"));
            }
            Ok(())
        }
    }

    const MCP: &str = "https://mcp.box.example/mcp";

    fn prm() -> Value {
        json!({ "resource": MCP, "authorization_servers": ["https://auth.box.example"] })
    }

    fn asm() -> Value {
        json!({
            "issuer": "https://auth.box.example",
            "authorization_endpoint": "https://auth.box.example/authorize",
            "token_endpoint": "https://auth.box.example/token",
            "revocation_endpoint": "https://auth.box.example/revoke",
            "code_challenge_methods_supported": ["S256"],
            "token_endpoint_auth_methods_supported": ["client_secret_post"]
        })
    }

    fn fake() -> Fake {
        Fake::new(&[
            (
                "https://mcp.box.example/.well-known/oauth-protected-resource/mcp",
                prm(),
            ),
            (
                "https://auth.box.example/.well-known/oauth-authorization-server",
                asm(),
            ),
        ])
    }

    fn with(mut f: Fake, url: &str, v: Value) -> Fake {
        f.answers.insert(url.to_string(), v);
        f
    }

    const AS_URL: &str = "https://auth.box.example/.well-known/oauth-authorization-server";

    #[test]
    fn discovery_reads_the_resource_then_the_server_and_pins_the_endpoints() {
        let f = fake();
        let d = discover(MCP, true, &f).unwrap();
        assert_eq!(d.issuer, "https://auth.box.example");
        assert_eq!(d.authorize_endpoint, "https://auth.box.example/authorize");
        assert_eq!(d.token_endpoint, "https://auth.box.example/token");
        assert_eq!(
            d.revocation_endpoint.as_deref(),
            Some("https://auth.box.example/revoke")
        );
        // Two fetches, both https, no query, nothing else.
        assert_eq!(f.asked.borrow().len(), 2);
    }

    #[test]
    fn the_root_metadata_is_the_fallback_for_a_resource_with_a_path() {
        // The root document names the root identifier its URL was built from, not the MCP URL.
        let mut root = prm();
        root["resource"] = json!("https://mcp.box.example");
        let f = Fake::new(&[
            (
                "https://mcp.box.example/.well-known/oauth-protected-resource",
                root,
            ),
            (AS_URL, asm()),
        ]);
        assert!(discover(MCP, true, &f).is_ok());
        assert_eq!(f.asked.borrow().len(), 3);
    }

    const PRM_URL: &str = "https://mcp.box.example/.well-known/oauth-protected-resource/mcp";
    const PRM_ROOT_URL: &str = "https://mcp.box.example/.well-known/oauth-protected-resource";

    #[test]
    fn the_protected_resource_must_name_the_resource_its_url_was_built_from() {
        // RFC 9728 §3.3: a missing or non-string `resource`, another path on the same host, another
        // server, and (for the root fallback) the MCP URL instead of the root identifier are refused.
        for resource in [
            None,
            Some(json!(42)),
            Some(json!(null)),
            Some(json!("https://mcp.box.example/other")),
            Some(json!("https://mcp.box.example")),
            Some(json!("https://other.example/mcp")),
            Some(json!("http://mcp.box.example/mcp")),
        ] {
            let mut doc = prm();
            match &resource {
                Some(v) => doc["resource"] = v.clone(),
                None => {
                    doc.as_object_mut().unwrap().remove("resource");
                }
            }
            let f = with(fake(), PRM_URL, doc);
            let e = discover(MCP, true, &f).unwrap_err();
            assert!(
                matches!(e, DiscoveryError::Unsuitable(_)),
                "{resource:?}: {e}"
            );
            assert!(e.to_string().contains("resource"), "{e}");
            // The authorization server's metadata was never asked for.
            assert_eq!(f.asked.borrow().len(), 1, "{resource:?}");
        }
        // Exactly the identifier passes.
        let mut doc = prm();
        doc["resource"] = json!(MCP);
        let f = with(fake(), PRM_URL, doc);
        assert!(discover(MCP, true, &f).is_ok());
        // The root fallback is checked against the identifier it was built from. An origin is the
        // same URI with or without its `/` path (RFC 3986 §6.2.3); a doubled slash is not.
        for (resource, ok) in [
            ("https://mcp.box.example", true),
            ("https://mcp.box.example/", true),
            ("https://mcp.box.example//", false),
            (MCP, false),
            ("https://other.example", false),
        ] {
            let mut root = prm();
            root["resource"] = json!(resource);
            let f = Fake::new(&[(PRM_ROOT_URL, root), (AS_URL, asm())]);
            assert_eq!(discover(MCP, true, &f).is_ok(), ok, "{resource}");
        }
    }

    #[test]
    fn a_resource_that_differs_by_a_trailing_slash_is_refused() {
        // RFC 9728 §3.3: the identifiers must be identical, so a trailing slash on either side is
        // another resource, not a spelling of this one.
        for resource in [
            "https://mcp.box.example/mcp/",
            "https://mcp.box.example/mcp//",
            "https://mcp.box.example/mcp///",
        ] {
            let mut doc = prm();
            doc["resource"] = json!(resource);
            let f = with(fake(), PRM_URL, doc);
            let e = discover(MCP, true, &f).unwrap_err();
            assert!(
                matches!(e, DiscoveryError::Unsuitable(_)),
                "{resource}: {e}"
            );
            assert_eq!(f.asked.borrow().len(), 1, "{resource}");
        }
        // A connector configured with the trailing slash must be named with it.
        let with_slash = "https://mcp.box.example/mcp/";
        let f = with(fake(), PRM_URL, prm());
        assert!(
            matches!(
                discover(with_slash, true, &f),
                Err(DiscoveryError::Unsuitable(_))
            ),
            "{MCP} named for {with_slash}"
        );
        let mut doc = prm();
        doc["resource"] = json!(with_slash);
        let f = with(fake(), PRM_URL, doc);
        assert!(discover(with_slash, true, &f).is_ok());
    }

    #[test]
    fn a_query_is_part_of_the_resource_identifier() {
        // RFC 9728 §3.1: the resource-specific lookup keeps the query, and the document names it.
        let with_query = "https://mcp.box.example/mcp?tenant=blue";
        let url = "https://mcp.box.example/.well-known/oauth-protected-resource/mcp?tenant=blue";
        let mut doc = prm();
        doc["resource"] = json!(with_query);
        let f = Fake::new(&[(url, doc), (AS_URL, asm())]);
        assert!(discover(with_query, true, &f).is_ok());
        assert_eq!(f.asked.borrow()[0], url);
        // The identifier without its query, or with another one, is another resource.
        for named in [
            MCP,
            "https://mcp.box.example/mcp?tenant=red",
            "https://mcp.box.example/mcp?tenant=blue&x=1",
        ] {
            let mut doc = prm();
            doc["resource"] = json!(named);
            let f = Fake::new(&[(url, doc), (AS_URL, asm())]);
            assert!(
                matches!(
                    discover(with_query, true, &f),
                    Err(DiscoveryError::Unsuitable(_))
                ),
                "{named}"
            );
        }
        // The origin fallback is unchanged: no query, the root identifier.
        let mut root = prm();
        root["resource"] = json!("https://mcp.box.example");
        let f = Fake::new(&[(PRM_ROOT_URL, root), (AS_URL, asm())]);
        assert!(discover(with_query, true, &f).is_ok());
        assert_eq!(f.asked.borrow()[1], PRM_ROOT_URL);
    }

    #[test]
    fn openid_configuration_is_the_fallback_for_the_server() {
        let f = Fake::new(&[
            (
                "https://mcp.box.example/.well-known/oauth-protected-resource/mcp",
                prm(),
            ),
            (
                "https://auth.box.example/.well-known/openid-configuration",
                asm(),
            ),
        ]);
        assert!(discover(MCP, true, &f).is_ok());
    }

    #[test]
    fn a_missing_s256_is_refused() {
        for methods in [json!(["plain"]), json!([])] {
            let mut meta = asm();
            meta["code_challenge_methods_supported"] = methods;
            let f = with(fake(), AS_URL, meta);
            let e = discover(MCP, true, &f).unwrap_err();
            assert!(e.to_string().contains("S256"), "{e}");
        }
        let mut meta = asm();
        meta.as_object_mut()
            .unwrap()
            .remove("code_challenge_methods_supported");
        let f = with(fake(), AS_URL, meta);
        assert!(discover(MCP, true, &f).is_err());
    }

    #[test]
    fn the_token_authentication_method_is_chosen_and_pinned_for_a_connector_that_keeps_a_secret() {
        use TokenAuthMethod::{ClientSecretBasic as Basic, ClientSecretPost as Post};
        let method = |listed: Option<Value>, keeps: bool| {
            let mut meta = asm();
            match listed {
                Some(v) => meta["token_endpoint_auth_methods_supported"] = v,
                None => {
                    meta.as_object_mut()
                        .unwrap()
                        .remove("token_endpoint_auth_methods_supported");
                }
            }
            discover(MCP, keeps, &with(fake(), AS_URL, meta)).map(|d| d.token_auth_method)
        };
        // POST is preferred when listed, alone or beside Basic.
        assert_eq!(
            method(Some(json!(["client_secret_post"])), true).unwrap(),
            Post
        );
        assert_eq!(
            method(
                Some(json!(["client_secret_basic", "client_secret_post"])),
                true
            )
            .unwrap(),
            Post
        );
        // Basic when it is the one listed, or when the provider says nothing (RFC 8414 §2).
        assert_eq!(
            method(Some(json!(["client_secret_basic"])), true).unwrap(),
            Basic
        );
        assert_eq!(method(None, true).unwrap(), Basic);
        // Neither is a refusal that names both.
        for listed in [
            json!(["private_key_jwt"]),
            json!([]),
            json!("client_secret_post"),
        ] {
            let e = method(Some(listed), true).unwrap_err();
            assert!(matches!(e, DiscoveryError::Unsuitable(_)), "{e}");
            assert!(e.to_string().contains("client_secret_basic"), "{e}");
        }
        // No secret kept: nothing to authenticate, whatever the provider lists or omits.
        for listed in [
            None,
            Some(json!(["private_key_jwt"])),
            Some(json!(["client_secret_basic"])),
        ] {
            assert_eq!(method(listed, false).unwrap(), Post);
        }
    }

    #[test]
    fn a_different_issuer_in_the_metadata_is_refused() {
        let mut meta = asm();
        meta["issuer"] = json!("https://evil.example");
        let f = with(fake(), AS_URL, meta);
        let e = discover(MCP, true, &f).unwrap_err();
        assert!(e.to_string().contains("different issuer"), "{e}");
    }

    #[test]
    fn the_issuer_is_compared_exactly_and_stored_as_named() {
        // A tenant issuer ends in `/`: that is part of its identifier. A document that drops it
        // names another identifier and is refused (RFC 8414 §3.3).
        let tenant = "https://auth.box.example/tenant/";
        let as_url = "https://auth.box.example/.well-known/oauth-authorization-server/tenant";
        let mut doc = prm();
        doc["authorization_servers"] = json!([tenant]);
        let with_tenant = |issuer: &str| {
            let mut meta = asm();
            meta["issuer"] = json!(issuer);
            with(with(fake(), PRM_URL, doc.clone()), as_url, meta)
        };
        let e = discover(MCP, true, &with_tenant("https://auth.box.example/tenant")).unwrap_err();
        assert!(e.to_string().contains("different issuer"), "{e}");
        let found = discover(MCP, true, &with_tenant(tenant)).unwrap();
        assert_eq!(found.issuer, tenant);
        // An origin-only issuer is stored without the slash a URL parser would add.
        assert_eq!(
            discover(MCP, true, &fake()).unwrap().issuer,
            "https://auth.box.example"
        );
    }

    #[test]
    fn a_resource_on_another_server_is_refused() {
        let mut doc = prm();
        doc["resource"] = json!("https://other.example/mcp");
        let f = with(
            fake(),
            "https://mcp.box.example/.well-known/oauth-protected-resource/mcp",
            doc,
        );
        assert!(matches!(
            discover(MCP, true, &f),
            Err(DiscoveryError::Unsuitable(_))
        ));
    }

    #[test]
    fn endpoints_must_be_public_looking_https_names() {
        for bad in [
            "http://auth.box.example/token",
            "https://127.0.0.1/token",
            "https://10.0.0.5/token",
            "https://[::1]/token",
            "https://localhost/token",
            "https://user:pw@auth.box.example/token",
            "https://auth.box.example:8443/token",
            "https://auth.box.example/token#frag",
            "https://auth.box.example/to ken",
            "ftp://auth.box.example/token",
            "",
        ] {
            assert!(check_endpoint(bad, "token endpoint").is_err(), "{bad:?}");
            let mut meta = asm();
            meta["token_endpoint"] = json!(bad);
            let f = with(fake(), AS_URL, meta);
            assert!(discover(MCP, true, &f).is_err(), "{bad:?}");
        }
        assert!(check_endpoint("https://auth.box.example:443/token", "x").is_ok());
        // An authorization server named by an IP address never gets a request.
        let mut doc = prm();
        doc["authorization_servers"] = json!(["https://169.254.169.254"]);
        let f = with(
            fake(),
            "https://mcp.box.example/.well-known/oauth-protected-resource/mcp",
            doc,
        );
        assert!(discover(MCP, true, &f).is_err());
        assert!(f.asked.borrow().iter().all(|u| !u.contains("169.254")));
    }

    #[test]
    fn a_connector_url_that_is_not_a_public_https_name_makes_no_request() {
        let f = fake();
        for bad in ["http://mcp.box.example/mcp", "https://192.168.0.1/mcp"] {
            assert!(discover(bad, true, &f).is_err(), "{bad}");
        }
        assert!(f.asked.borrow().is_empty());
    }

    #[test]
    fn a_revocation_endpoint_on_another_host_is_not_pinned() {
        let mut meta = asm();
        meta["revocation_endpoint"] = json!("https://elsewhere.example/revoke");
        let f = with(fake(), AS_URL, meta);
        assert_eq!(discover(MCP, true, &f).unwrap().revocation_endpoint, None);
    }

    #[test]
    fn a_token_host_that_is_not_public_is_refused_before_anything_is_stored() {
        let mut f = fake();
        f.private_hosts = vec!["auth.box.example"];
        let e = discover(MCP, true, &f).unwrap_err();
        assert!(e.to_string().contains("not a public address"), "{e}");
    }

    #[test]
    fn the_guarded_fetcher_refuses_a_host_that_resolves_to_a_private_address() {
        // `localhost` fails the syntax rule, an IP literal is a private address under the pin.
        let u = Url::parse("https://127.0.0.1/.well-known/x").unwrap();
        assert!(matches!(
            GuardedFetcher.get_json(&u),
            Err(DiscoveryError::Unsuitable(_))
        ));
        assert!(matches!(
            GuardedFetcher.check_public_host(&u),
            Err(DiscoveryError::Unsuitable(_))
        ));
    }

    #[test]
    fn errors_name_a_host_never_a_body() {
        let f = with(
            fake(),
            AS_URL,
            json!({ "issuer": "https://auth.box.example", "secret": "s3cr3t-value" }),
        );
        let e = discover(MCP, true, &f).unwrap_err().to_string();
        assert!(!e.contains("s3cr3t-value"), "{e}");
    }
}
