//! The network fetcher: guarded, pinned, redirect-by-hand, robots-aware,
//! size- and time-capped.

#[cfg(any(test, feature = "test-support"))]
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant as StdInstant, SystemTime};

use async_trait::async_trait;
use reqwest::header::{ACCEPT, ACCEPT_LANGUAGE, CONTENT_TYPE, LOCATION};
use tokio::sync::Semaphore;
use tokio::time::{timeout_at, Instant};
use url::Url;
use vam_security::ssrf_guard::{
    is_restricted_ip_strict, HostResolver, PinError, PinnedUrl, SystemResolver,
};

use super::artifact::rfc3339_utc;
use super::content::{kind_for_mime, mime_essence};
use super::robots::{Robots, RobotsCache, MAX_ROBOTS_BYTES, TTL_FOUND, TTL_UNREACHABLE};
use super::rules::{
    check_url, classify_redirect, literal_ip_restricted, parse_request_url, RedirectKind,
    MAX_REDIRECTS,
};
use super::{
    convert_blocking, FetchOutcome, Page, PageFetcher, CONNECT_TIMEOUT, DEFAULT_DOCS_URL,
    MAX_CONCURRENT_CONVERSIONS, MAX_RAW_BYTES, TOTAL_TIMEOUT, USER_AGENT_PRODUCT,
};
use crate::error::FetchError;

/// robots.txt gets this long (inside the call deadline).
const ROBOTS_TIMEOUT: Duration = Duration::from_secs(5);
/// A `Location` header longer than this is not followed.
const MAX_LOCATION_BYTES: usize = 8 * 1024;

/// How names are resolved. Production uses the system resolver; unit tests
/// swap in a fake resolver and a connect address, which is the only way the
/// local test servers are reachable. None of this exists outside `cfg(test)` or the off-by-default `test-support` feature
/// and no CLI flag can set it.
#[derive(Clone)]
pub(crate) struct Env {
    pub resolver: Arc<dyn HostResolver + Send + Sync>,
    #[cfg(any(test, feature = "test-support"))]
    pub connect_ip: Option<Arc<dyn Fn(IpAddr) -> IpAddr + Send + Sync>>,
    #[cfg(any(test, feature = "test-support"))]
    pub any_port: bool,
}

impl Env {
    pub fn system() -> Self {
        Self {
            resolver: Arc::new(SystemResolver),
            #[cfg(any(test, feature = "test-support"))]
            connect_ip: None,
            #[cfg(any(test, feature = "test-support"))]
            any_port: false,
        }
    }

    fn any_port(&self) -> bool {
        #[cfg(any(test, feature = "test-support"))]
        {
            self.any_port
        }
        #[cfg(not(any(test, feature = "test-support")))]
        {
            false
        }
    }

    fn apply_pin(
        &self,
        builder: reqwest::ClientBuilder,
        pin: &PinnedUrl,
    ) -> reqwest::ClientBuilder {
        #[cfg(any(test, feature = "test-support"))]
        if let Some(map) = &self.connect_ip {
            let addrs: Vec<std::net::SocketAddr> = pin
                .resolved_ips
                .iter()
                .map(|ip| std::net::SocketAddr::new(map(*ip), pin.original_port))
                .collect();
            return builder.resolve_to_addrs(&pin.original_host, &addrs);
        }
        pin.apply_to_async_client_builder(builder)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Want {
    Page,
    Robots,
}

enum Hop {
    /// A 301, 302, 303, 307 or 308 with its `Location`.
    Redirect(Option<String>),
    /// Any other non-2xx status.
    Status(u16),
    Ok {
        content_type: Option<String>,
        body: Vec<u8>,
    },
}

/// A failed hop; `host_unreachable` marks a DNS or connect failure.
struct HopError {
    err: FetchError,
    host_unreachable: bool,
}

impl From<FetchError> for HopError {
    fn from(err: FetchError) -> Self {
        Self {
            err,
            host_unreachable: false,
        }
    }
}

/// Fetches pages from the open web.
pub struct NetworkFetcher {
    env: Env,
    user_agent: String,
    robots: RobotsCache,
    permits: Arc<Semaphore>,
}

impl NetworkFetcher {
    /// `docs_url` goes into the User-Agent so site owners can find out who
    /// fetches: `VAM-WebFetch/1 (+<docs_url>)`.
    pub fn new(docs_url: &str) -> Self {
        Self::with_env(Env::system(), docs_url)
    }

    /// Test seam for integration tests (own process): a fake resolver, every
    /// validated address dialled at `connect_ip`, any port allowed. Exists
    /// only with the off-by-default `test-support` feature, which the
    /// shipped binary is never built with.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn for_tests(
        resolver: Arc<dyn HostResolver + Send + Sync>,
        connect_ip: IpAddr,
        docs_url: &str,
    ) -> Self {
        let env = Env {
            resolver,
            connect_ip: Some(Arc::new(move |_| connect_ip)),
            any_port: true,
        };
        Self::with_env(env, docs_url)
    }

    pub(crate) fn with_env(env: Env, docs_url: &str) -> Self {
        let mut user_agent = format!("{USER_AGENT_PRODUCT}/1 (+{})", docs_url.trim());
        if reqwest::header::HeaderValue::from_str(&user_agent).is_err() {
            user_agent = format!("{USER_AGENT_PRODUCT}/1 (+{DEFAULT_DOCS_URL})");
        }
        Self {
            env,
            user_agent,
            robots: RobotsCache::default(),
            permits: Arc::new(Semaphore::new(MAX_CONCURRENT_CONVERSIONS)),
        }
    }

    pub fn user_agent(&self) -> &str {
        &self.user_agent
    }

    /// Validate and pin one hop with the strict classifier.
    async fn pin(&self, url: &Url, deadline: Instant) -> Result<PinnedUrl, HopError> {
        let resolver = self.env.resolver.clone();
        let target = url.to_string();
        let task = tokio::task::spawn_blocking(move || {
            PinnedUrl::validate_and_pin_with_policy(
                &target,
                &[],
                &*resolver,
                &is_restricted_ip_strict,
            )
        });
        match timeout_at(deadline, task).await {
            Err(_) => Err(FetchError::Timeout.into()),
            Ok(Err(_)) => {
                Err(FetchError::not_accessible("the host name could not be checked").into())
            }
            Ok(Ok(Err(PinError::Rejected(_)))) => Err(FetchError::Blocked(
                "the address is private, local or otherwise restricted".to_string(),
            )
            .into()),
            Ok(Ok(Err(PinError::Resolution(_)))) => Err(HopError {
                err: FetchError::not_accessible(
                    "the host name could not be resolved; check the URL or use another source",
                ),
                host_unreachable: true,
            }),
            Ok(Ok(Ok(pin))) => Ok(pin),
        }
    }

    /// One validated, pinned GET. Never follows a redirect.
    async fn hop(&self, url: &Url, want: Want, deadline: Instant) -> Result<Hop, HopError> {
        let pin = self.pin(url, deadline).await?;
        let builder = reqwest::Client::builder()
            .user_agent(self.user_agent.clone())
            // Ignore HTTP(S)_PROXY and friends: a proxy resolves names itself
            // and would bypass the pinned addresses. An explicit operator
            // proxy option is a later item.
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(CONNECT_TIMEOUT)
            .pool_max_idle_per_host(0);
        let client = self
            .env
            .apply_pin(builder, &pin)
            .build()
            .map_err(|_| FetchError::not_accessible("the HTTP client could not be built"))?;
        let accept = match want {
            Want::Page => "text/html,application/xhtml+xml,text/plain;q=0.9,text/markdown;q=0.9",
            Want::Robots => "text/plain,*/*;q=0.1",
        };
        let request = client
            .get(url.as_str())
            .header(ACCEPT, accept)
            .header(ACCEPT_LANGUAGE, "en;q=0.9, *;q=0.5");
        let mut response = timeout_at(deadline, request.send())
            .await
            .map_err(|_| FetchError::Timeout)?
            .map_err(|e| HopError {
                err: map_transport_error(&e),
                host_unreachable: e.is_connect(),
            })?;

        let status = response.status();
        if status.is_redirection() {
            return Ok(match status.as_u16() {
                301 | 302 | 303 | 307 | 308 => Hop::Redirect(
                    response
                        .headers()
                        .get(LOCATION)
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_string),
                ),
                other => Hop::Status(other),
            });
        }
        if !status.is_success() {
            return Ok(Hop::Status(status.as_u16()));
        }

        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let limit = match want {
            Want::Page => MAX_RAW_BYTES,
            Want::Robots => MAX_ROBOTS_BYTES,
        };
        if want == Want::Page {
            if let Some(ct) = &content_type {
                let mime = mime_essence(ct);
                if !mime.is_empty() && kind_for_mime(&mime).is_none() {
                    return Err(
                        FetchError::UnsupportedContentType(super::content::describe_mime(&mime))
                            .into(),
                    );
                }
            }
            if response.content_length().is_some_and(|n| n > limit as u64) {
                return Err(FetchError::TooLarge.into());
            }
        }

        // Stream and stop at the cap: an endless or compressed-huge body is
        // never buffered. The cap counts decoded bytes.
        let mut body: Vec<u8> = Vec::new();
        loop {
            let next = timeout_at(deadline, response.chunk())
                .await
                .map_err(|_| FetchError::Timeout)?
                .map_err(|e| map_body_error(&e))?;
            let Some(chunk) = next else { break };
            if body.len() + chunk.len() > limit {
                match want {
                    Want::Page => return Err(FetchError::TooLarge.into()),
                    Want::Robots => {
                        body.extend_from_slice(&chunk[..limit - body.len()]);
                        break;
                    }
                }
            }
            body.extend_from_slice(&chunk);
        }
        Ok(Hop::Ok { content_type, body })
    }

    /// The robots.txt verdict for `url`'s origin (cached).
    async fn robots_allows(&self, url: &Url, deadline: Instant) -> Result<bool, FetchError> {
        let origin = format!(
            "{}://{}:{}",
            url.scheme(),
            url.host_str().unwrap_or(""),
            url.port_or_known_default().unwrap_or(0)
        );
        let rules = match self.robots.get(&origin, StdInstant::now()) {
            Some(rules) => rules,
            None => {
                let (rules, ttl) = self.load_robots(url, deadline).await?;
                let rules = Arc::new(rules);
                self.robots
                    .put(&origin, rules.clone(), ttl, StdInstant::now());
                rules
            }
        };
        let mut path = url.path().to_string();
        if let Some(q) = url.query() {
            path.push('?');
            path.push_str(q);
        }
        Ok(rules.allows(&path))
    }

    /// Fetch and parse robots.txt through the same guard. A 4xx means no
    /// rules; anything else that fails means "disallow" (RFC 9309), except
    /// a policy refusal or DNS failure, which are the page's own errors.
    async fn load_robots(
        &self,
        page: &Url,
        deadline: Instant,
    ) -> Result<(Robots, Duration), FetchError> {
        let robots_deadline = deadline.min(Instant::now() + ROBOTS_TIMEOUT);
        let mut current = page.clone();
        current.set_path("/robots.txt");
        current.set_query(None);
        current.set_fragment(None);
        let unreachable = || (Robots::disallow_all(), TTL_UNREACHABLE);
        let mut hops = 0;
        loop {
            match self.hop(&current, Want::Robots, robots_deadline).await {
                Ok(Hop::Ok { body, .. }) => {
                    let text = String::from_utf8_lossy(&body);
                    return Ok((Robots::parse(&text, USER_AGENT_PRODUCT), TTL_FOUND));
                }
                Ok(Hop::Status(code)) if (400..500).contains(&code) && code != 429 => {
                    return Ok((Robots::allow_all(), TTL_FOUND));
                }
                Ok(Hop::Status(_)) => return Ok(unreachable()),
                Ok(Hop::Redirect(location)) => {
                    let next = match resolve_location(&current, location) {
                        Ok(next) if check_url(&next, self.env.any_port()).is_ok() => next,
                        _ => return Ok(unreachable()),
                    };
                    if literal_ip_restricted(&next) {
                        return Err(FetchError::Blocked(
                            "robots.txt redirects to a private or restricted address".to_string(),
                        ));
                    }
                    hops += 1;
                    if classify_redirect(&current, &next) != RedirectKind::SameHost
                        || hops > MAX_REDIRECTS
                    {
                        return Ok(unreachable());
                    }
                    current = next;
                }
                Err(h) => {
                    if h.host_unreachable || matches!(h.err, FetchError::Blocked(_)) {
                        return Err(h.err);
                    }
                    if matches!(h.err, FetchError::Timeout) && Instant::now() >= deadline {
                        return Err(FetchError::Timeout);
                    }
                    return Ok(unreachable());
                }
            }
        }
    }
}

#[async_trait]
impl PageFetcher for NetworkFetcher {
    async fn fetch(&self, url: &str, timeout: Duration) -> Result<FetchOutcome, FetchError> {
        let deadline = Instant::now() + timeout.min(TOTAL_TIMEOUT);
        let requested = parse_request_url(url, self.env.any_port())?;
        let mut current = requested.clone();
        let mut hops = 0usize;
        let (content_type, body) = loop {
            if !self.robots_allows(&current, deadline).await? {
                return Err(FetchError::BlockedByRobots);
            }
            match self
                .hop(&current, Want::Page, deadline)
                .await
                .map_err(|h| h.err)?
            {
                Hop::Redirect(location) => {
                    let next = resolve_location(&current, location)?;
                    check_url(&next, self.env.any_port()).map_err(|_| {
                        FetchError::not_accessible(
                            "the page redirects to an address web_fetch cannot read; use another source",
                        )
                    })?;
                    if literal_ip_restricted(&next) {
                        return Err(FetchError::Blocked(
                            "the page redirects to a private or restricted address".to_string(),
                        ));
                    }
                    match classify_redirect(&current, &next) {
                        RedirectKind::CrossHost => {
                            return Ok(FetchOutcome::Redirected {
                                requested_url: requested.to_string(),
                                final_url: current.to_string(),
                                redirect_to: next.to_string(),
                            });
                        }
                        RedirectKind::Downgrade => {
                            return Err(FetchError::Blocked(
                                "the page redirects from https to http".to_string(),
                            ));
                        }
                        RedirectKind::SameHost => {}
                    }
                    hops += 1;
                    if hops > MAX_REDIRECTS {
                        return Err(FetchError::not_accessible(
                            "the site redirected too many times; use another source",
                        ));
                    }
                    current = next;
                }
                Hop::Status(code) => return Err(status_error(code)),
                Hop::Ok { content_type, body } => break (content_type, body),
            }
        };

        let raw_bytes = body.len();
        let converted = timeout_at(
            deadline,
            convert_blocking(&self.permits, body, content_type.clone(), current.clone()),
        )
        .await
        .map_err(|_| FetchError::Timeout)??;
        Ok(FetchOutcome::Page(Box::new(Page {
            requested_url: requested.to_string(),
            final_url: current,
            title: converted.title,
            markdown: converted.markdown,
            content_type: content_type
                .as_deref()
                .map(mime_essence)
                .filter(|m| !m.is_empty())
                .unwrap_or_else(|| "text/html".to_string()),
            raw_bytes,
            retrieved_at: rfc3339_utc(SystemTime::now()),
        })))
    }
}

/// Resolve a `Location` header against the URL that sent it.
fn resolve_location(current: &Url, location: Option<String>) -> Result<Url, FetchError> {
    let Some(location) = location else {
        return Err(FetchError::not_accessible(
            "the site redirected without saying where; use another source",
        ));
    };
    if location.len() > MAX_LOCATION_BYTES {
        return Err(FetchError::not_accessible(
            "the site redirected to an address that is too long; use another source",
        ));
    }
    current.join(location.trim()).map_err(|_| {
        FetchError::not_accessible("the site redirected to an invalid address; use another source")
    })
}

fn status_error(code: u16) -> FetchError {
    match code {
        429 => FetchError::RateLimited,
        401 | 403 => FetchError::not_accessible(format!(
            "the site refused access (HTTP {code}: login, paywall or bot protection); use another source"
        )),
        404 | 410 => FetchError::not_accessible(format!(
            "the page was not found (HTTP {code}); check the URL or use another source"
        )),
        408 | 500..=599 => FetchError::temporarily_not_accessible(format!(
            "the site had a server error (HTTP {code}); try again later or use another source"
        )),
        _ => FetchError::not_accessible(format!(
            "the site answered HTTP {code}; use another source"
        )),
    }
}

fn map_transport_error(e: &reqwest::Error) -> FetchError {
    if e.is_timeout() {
        FetchError::Timeout
    } else if e.is_connect() {
        FetchError::temporarily_not_accessible(
            "could not connect to the site (it may be down, or its certificate is invalid); try again later or use another source",
        )
    } else {
        FetchError::temporarily_not_accessible(
            "the connection to the site failed; try again later or use another source",
        )
    }
}

fn map_body_error(e: &reqwest::Error) -> FetchError {
    if e.is_timeout() {
        FetchError::Timeout
    } else if e.is_decode() {
        FetchError::not_accessible("the page body could not be decoded; use another source")
    } else {
        FetchError::temporarily_not_accessible(
            "the connection dropped while reading the page; try again later",
        )
    }
}

#[cfg(test)]
mod tests;
