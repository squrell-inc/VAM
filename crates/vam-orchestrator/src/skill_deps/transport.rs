//! Fetching bytes from a mirror (VI-1.5). Two transports: `HttpTransport` (https only, every hop
//! pinned and allowlisted, strict address policy, no proxy, no cookies, no automatic redirects)
//! and `FileTransport` (a contained local directory). A lockfile URL never reaches either.

use super::{refused, unavailable, DepsError, Limits};
use reqwest::header::{ACCEPT, ACCEPT_ENCODING, CONTENT_ENCODING, CONTENT_LENGTH, LOCATION};
use reqwest::StatusCode;
use std::io::{Read, Write};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use url::Url;
use vam_security::ssrf_guard::{
    is_restricted_ip_strict, HostResolver, PinError, PinnedUrl, SystemResolver,
};
use vam_security::tls_roots::BundledRoots;

/// Where one resource lives, built by this module's callers from validated parts only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Locator {
    Http(Url),
    File(PathBuf),
}

/// What a fetch found.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Fetched {
    /// The body was written to the sink; this many bytes.
    Body(u64),
    /// The mirror has no such resource (HTTP 404, or no such file).
    NotFound,
}

pub(crate) trait Transport: Send + Sync {
    /// Stream the resource into `out`, refusing more than `cap` bytes. `deadline` bounds the
    /// whole `resolve` call.
    fn get(
        &self,
        loc: &Locator,
        accept_json: bool,
        cap: u64,
        deadline: Instant,
        out: &mut dyn Write,
    ) -> Result<Fetched, DepsError>;
}

const CHUNK: usize = 64 * 1024;

fn over_cap(cap: u64) -> DepsError {
    refused(format!(
        "the package mirror sent more than {} bytes for one file; the limit is {cap} bytes",
        cap
    ))
}

fn check_deadline(deadline: Instant) -> Result<(), DepsError> {
    if Instant::now() >= deadline {
        return Err(unavailable(
            "fetching packages took longer than the time limit; try again",
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// file://
// ---------------------------------------------------------------------------------------------

/// Reads regular files under one canonical root. A symlink, or a path that resolves outside the
/// root, is refused.
pub(crate) struct FileTransport {
    root: PathBuf,
}

impl FileTransport {
    pub(crate) fn new(root: PathBuf) -> FileTransport {
        FileTransport { root }
    }

    fn contained(&self, path: &Path) -> Result<Option<PathBuf>, DepsError> {
        let meta = match std::fs::symlink_metadata(path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(DepsError::Io(e)),
        };
        if !meta.file_type().is_file() {
            return Err(refused(
                "the package mirror holds a link or a non-file where a package file belongs",
            ));
        }
        let canon = match std::fs::canonicalize(path) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(DepsError::Io(e)),
        };
        if !canon.starts_with(&self.root) {
            return Err(refused(
                "a package mirror file resolves outside the mirror directory",
            ));
        }
        Ok(Some(canon))
    }
}

impl Transport for FileTransport {
    fn get(
        &self,
        loc: &Locator,
        _accept_json: bool,
        cap: u64,
        deadline: Instant,
        out: &mut dyn Write,
    ) -> Result<Fetched, DepsError> {
        let Locator::File(path) = loc else {
            return Err(refused("internal: a file mirror was asked for a URL"));
        };
        let Some(canon) = self.contained(path)? else {
            return Ok(Fetched::NotFound);
        };
        let mut file = std::fs::File::open(canon)?;
        if file.metadata()?.len() > cap {
            return Err(over_cap(cap));
        }
        let mut buf = vec![0u8; CHUNK];
        let mut total = 0u64;
        loop {
            check_deadline(deadline)?;
            let n = file.read(&mut buf)?;
            if n == 0 {
                break;
            }
            total += n as u64;
            if total > cap {
                return Err(over_cap(cap));
            }
            out.write_all(&buf[..n])?;
        }
        Ok(Fetched::Body(total))
    }
}

// ---------------------------------------------------------------------------------------------
// https
// ---------------------------------------------------------------------------------------------

type Restricted = dyn Fn(IpAddr) -> bool + Send + Sync;

/// Guarded HTTPS: a fresh client per hop, pinned to the validated addresses.
pub(crate) struct HttpTransport {
    allowlist: Vec<String>,
    resolver: Arc<dyn HostResolver + Send + Sync>,
    restricted: Arc<Restricted>,
    /// Only a test build can set this (see `for_tests`); production is always false.
    allow_http: bool,
    connect_timeout: Duration,
    request_timeout: Duration,
    max_redirects: usize,
}

impl HttpTransport {
    /// Production: system resolver, strict address policy, https only.
    pub(crate) fn new(hosts: Vec<String>, limits: &Limits) -> HttpTransport {
        HttpTransport {
            allowlist: hosts.into_iter().map(|h| h.to_ascii_lowercase()).collect(),
            resolver: Arc::new(SystemResolver),
            restricted: Arc::new(is_restricted_ip_strict),
            allow_http: false,
            connect_timeout: limits.connect_timeout,
            request_timeout: limits.request_timeout,
            max_redirects: limits.max_redirects,
        }
    }

    fn check_url(&self, url: &Url, redirect: bool) -> Result<String, DepsError> {
        let scheme_ok = url.scheme() == "https" || (self.allow_http && url.scheme() == "http");
        if !scheme_ok {
            return Err(refused(if redirect {
                "a package mirror redirect left https; only https is allowed".to_string()
            } else {
                format!("package mirror URL '{}' must be https", redact(url))
            }));
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(refused("a package mirror URL must not carry credentials"));
        }
        let host = url
            .host_str()
            .map(|h| h.to_ascii_lowercase())
            .unwrap_or_default();
        if !self.allowlist.contains(&host) {
            return Err(refused(if redirect {
                format!("package mirror redirect to '{host}' is not allowed")
            } else {
                format!("the package mirror URL host '{host}' is not allowed")
            }));
        }
        Ok(host)
    }

    fn client_for(
        &self,
        pinned: &PinnedUrl,
        remaining: Duration,
    ) -> Result<reqwest::blocking::Client, DepsError> {
        // `no_proxy`: an environment proxy would resolve the destination itself, bypassing the
        // pin. No cookie store and no redirect following: redirects are walked by hand.
        let builder = reqwest::blocking::Client::builder()
            .bundled_roots()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(self.connect_timeout)
            .timeout(self.request_timeout.min(remaining))
            .user_agent("vamd-package-fetch/1");
        pinned
            .apply_to_client_builder(builder)
            .build()
            .map_err(|e| unavailable(format!("could not build the package mirror client: {e}")))
    }
}

/// A URL for messages: no credentials, no query.
fn redact(url: &Url) -> String {
    let mut u = url.clone();
    let _ = u.set_username("");
    let _ = u.set_password(None);
    u.set_query(None);
    u.set_fragment(None);
    u.to_string()
}

impl Transport for HttpTransport {
    fn get(
        &self,
        loc: &Locator,
        accept_json: bool,
        cap: u64,
        deadline: Instant,
        out: &mut dyn Write,
    ) -> Result<Fetched, DepsError> {
        let Locator::Http(start) = loc else {
            return Err(refused(
                "internal: an https mirror was asked for a file path",
            ));
        };
        let mut url = start.clone();
        let mut hops = 0usize;
        loop {
            check_deadline(deadline)?;
            let host = self.check_url(&url, hops > 0)?;
            let pinned = PinnedUrl::validate_and_pin_with_policy(
                url.as_str(),
                &self.allowlist,
                &*self.resolver,
                &*self.restricted,
            )
            .map_err(|e| match e {
                PinError::Rejected(_) => refused(format!(
                    "the package mirror host '{host}' is not a public address"
                )),
                PinError::Resolution(_) => unavailable(format!(
                    "the package mirror host '{host}' could not be resolved"
                )),
            })?;
            let request_start = Instant::now();
            let remaining = deadline.saturating_duration_since(request_start);
            let client = self.client_for(&pinned, remaining)?;
            let accept = if accept_json {
                "application/vnd.pypi.simple.v1+json"
            } else {
                "*/*"
            };
            let resp = client
                .get(url.as_str())
                .header(ACCEPT, accept)
                .header(ACCEPT_ENCODING, "identity")
                .send()
                .map_err(|e| {
                    if e.is_timeout() {
                        unavailable(format!("the package mirror '{host}' timed out"))
                    } else {
                        unavailable(format!("the package mirror '{host}' could not be reached"))
                    }
                })?;
            let status = resp.status();
            if matches!(
                status,
                StatusCode::MOVED_PERMANENTLY
                    | StatusCode::FOUND
                    | StatusCode::SEE_OTHER
                    | StatusCode::TEMPORARY_REDIRECT
                    | StatusCode::PERMANENT_REDIRECT
            ) {
                hops += 1;
                if hops > self.max_redirects {
                    return Err(refused(format!(
                        "the package mirror redirected more than {} times",
                        self.max_redirects
                    )));
                }
                let location = resp
                    .headers()
                    .get(LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .ok_or_else(|| {
                        refused("the package mirror sent a redirect without a Location")
                    })?;
                let mut next = url
                    .join(location)
                    .map_err(|_| refused("the package mirror sent an unreadable redirect"))?;
                next.set_fragment(None);
                url = next;
                continue;
            }
            if status == StatusCode::NOT_FOUND {
                return Ok(Fetched::NotFound);
            }
            if !status.is_success() {
                return Err(unavailable(format!(
                    "the package mirror '{host}' answered HTTP {}; try again later",
                    status.as_u16()
                )));
            }
            if let Some(enc) = resp.headers().get(CONTENT_ENCODING) {
                let enc = enc.to_str().unwrap_or("?").trim().to_ascii_lowercase();
                if !enc.is_empty() && enc != "identity" {
                    return Err(refused(format!(
                        "the package mirror sent Content-Encoding '{enc}'; only identity is accepted"
                    )));
                }
            }
            if let Some(len) = resp
                .headers()
                .get(CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
            {
                if len > cap {
                    return Err(over_cap(cap));
                }
            }
            let body_deadline = deadline.min(request_start + self.request_timeout);
            return stream_body(resp, cap, body_deadline, out, &host);
        }
    }
}

/// Copies the body, counting bytes as they arrive (a header can lie or be absent).
fn stream_body(
    mut resp: reqwest::blocking::Response,
    cap: u64,
    deadline: Instant,
    out: &mut dyn Write,
    host: &str,
) -> Result<Fetched, DepsError> {
    let mut buf = vec![0u8; CHUNK];
    let mut total = 0u64;
    loop {
        if Instant::now() >= deadline {
            return Err(unavailable(format!(
                "the package mirror '{host}' took too long to send the file; try again"
            )));
        }
        let n = resp.read(&mut buf).map_err(|_| {
            unavailable(format!(
                "the package mirror '{host}' stopped sending data or timed out"
            ))
        })?;
        if n == 0 {
            break;
        }
        total += n as u64;
        if total > cap {
            return Err(over_cap(cap));
        }
        out.write_all(&buf[..n])?;
    }
    Ok(Fetched::Body(total))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skill_deps::testutil::{Reply, TestServer};
    use std::net::{Ipv4Addr, SocketAddr};

    /// Maps fixed test names to addresses without touching DNS.
    struct FixedResolver(Vec<(&'static str, IpAddr)>);

    impl HostResolver for FixedResolver {
        fn lookup(&self, host: &str, port: u16) -> std::io::Result<Vec<SocketAddr>> {
            self.0
                .iter()
                .filter(|(h, _)| *h == host)
                .map(|(_, ip)| Ok(SocketAddr::new(*ip, port)))
                .collect::<std::io::Result<Vec<_>>>()
                .and_then(|v| {
                    if v.is_empty() {
                        Err(std::io::Error::new(
                            std::io::ErrorKind::NotFound,
                            "no such host",
                        ))
                    } else {
                        Ok(v)
                    }
                })
        }
    }

    impl HttpTransport {
        /// The only way to reach a loopback server: an injected resolver and address policy,
        /// plain http allowed. Exists in test builds only; no env var or flag does this.
        pub(crate) fn for_tests(
            resolver: impl HostResolver + Send + Sync + 'static,
            restricted: impl Fn(IpAddr) -> bool + Send + Sync + 'static,
            allow_http: bool,
            hosts: &[&str],
            limits: &Limits,
        ) -> HttpTransport {
            let mut t = HttpTransport::new(hosts.iter().map(|h| h.to_string()).collect(), limits);
            t.resolver = Arc::new(resolver);
            t.restricted = Arc::new(restricted);
            t.allow_http = allow_http;
            t
        }
    }

    const LOOP: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

    /// Loopback is allowed here; every other restricted address stays restricted.
    fn only_loopback_allowed(ip: IpAddr) -> bool {
        ip != LOOP && is_restricted_ip_strict(ip)
    }

    fn short_limits() -> Limits {
        Limits {
            connect_timeout: Duration::from_millis(500),
            request_timeout: Duration::from_millis(800),
            ..Limits::default()
        }
    }

    fn transport(
        hosts: &[&str],
        names: Vec<(&'static str, IpAddr)>,
        limits: &Limits,
    ) -> HttpTransport {
        HttpTransport::for_tests(
            FixedResolver(names),
            only_loopback_allowed,
            true,
            hosts,
            limits,
        )
    }

    fn far() -> Instant {
        Instant::now() + Duration::from_secs(30)
    }

    fn url(server: &TestServer, host: &str, path: &str) -> Locator {
        Locator::Http(Url::parse(&format!("http://{host}:{}{path}", server.port())).expect("url"))
    }

    fn fetch(t: &HttpTransport, loc: &Locator, cap: u64) -> Result<(Fetched, Vec<u8>), DepsError> {
        let mut out = Vec::new();
        let r = t.get(loc, false, cap, far(), &mut out)?;
        Ok((r, out))
    }

    fn refusal(r: Result<(Fetched, Vec<u8>), DepsError>) -> String {
        match r {
            Err(DepsError::Refused(m)) => m,
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn plain_get_streams_the_body() {
        let server = TestServer::start(|_| Reply::ok(b"hello".to_vec()));
        let t = transport(
            &["mirror.test"],
            vec![("mirror.test", LOOP)],
            &short_limits(),
        );
        let (r, body) = fetch(&t, &url(&server, "mirror.test", "/x"), 100).expect("fetch");
        assert_eq!((r, body.as_slice()), (Fetched::Body(5), &b"hello"[..]));
        let seen = server.requests();
        assert_eq!(seen.len(), 1);
        assert!(
            seen[0]
                .head
                .to_ascii_lowercase()
                .contains("accept-encoding: identity"),
            "{}",
            seen[0].head
        );
    }

    #[test]
    fn json_accept_header_is_sent_for_indexes() {
        let server = TestServer::start(|_| Reply::ok(b"{}".to_vec()));
        let t = transport(
            &["mirror.test"],
            vec![("mirror.test", LOOP)],
            &short_limits(),
        );
        let mut out = Vec::new();
        t.get(
            &url(&server, "mirror.test", "/i/"),
            true,
            100,
            far(),
            &mut out,
        )
        .expect("fetch");
        assert!(server.requests()[0]
            .head
            .contains("application/vnd.pypi.simple.v1+json"));
    }

    #[test]
    fn index_url_loopback_refused() {
        // The strict policy refuses loopback, with no test seam relaxation.
        let server = TestServer::start(|_| Reply::ok(b"x".to_vec()));
        let t = HttpTransport::for_tests(
            FixedResolver(vec![("mirror.test", LOOP)]),
            is_restricted_ip_strict,
            true,
            &["mirror.test"],
            &short_limits(),
        );
        let m = refusal(fetch(&t, &url(&server, "mirror.test", "/x"), 100));
        assert!(m.contains("'mirror.test' is not a public address"), "{m}");
        assert_eq!(server.requests().len(), 0, "never connected");
    }

    #[test]
    fn pinned_address_is_used_not_dns() {
        // `mirror.test` resolves nowhere on this machine; the request succeeds only because the
        // validated address is pinned into the client, and the resolver ran once for the hop.
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct Counting(Arc<AtomicUsize>);
        impl HostResolver for Counting {
            fn lookup(&self, _host: &str, port: u16) -> std::io::Result<Vec<SocketAddr>> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(vec![SocketAddr::new(LOOP, port)])
            }
        }
        let lookups = Arc::new(AtomicUsize::new(0));
        let server = TestServer::start(|_| Reply::ok(b"pinned".to_vec()));
        let t = HttpTransport::for_tests(
            Counting(lookups.clone()),
            only_loopback_allowed,
            true,
            &["mirror.test"],
            &short_limits(),
        );
        let (r, body) = fetch(&t, &url(&server, "mirror.test", "/x"), 100).expect("fetch");
        assert_eq!((r, body.as_slice()), (Fetched::Body(6), &b"pinned"[..]));
        assert_eq!(lookups.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn production_transport_refuses_plain_http() {
        let t = HttpTransport::new(vec!["mirror.test".into()], &short_limits());
        let m = refusal(fetch(
            &t,
            &Locator::Http(Url::parse("http://mirror.test/x").expect("url")),
            100,
        ));
        assert!(m.contains("must be https"), "{m}");
    }

    #[test]
    fn index_url_off_host_refused() {
        let t = transport(
            &["mirror.test"],
            vec![("mirror.test", LOOP)],
            &short_limits(),
        );
        let m = refusal(fetch(
            &t,
            &Locator::Http(Url::parse("http://evil.test:1/x.whl").expect("url")),
            100,
        ));
        assert!(m.contains("host 'evil.test' is not allowed"), "{m}");
        let m = refusal(fetch(
            &t,
            &Locator::Http(Url::parse("http://169.254.169.254/latest/meta-data").expect("url")),
            100,
        ));
        assert!(m.contains("host '169.254.169.254' is not allowed"), "{m}");
    }

    #[test]
    fn credentials_in_a_url_are_refused() {
        let t = transport(
            &["mirror.test"],
            vec![("mirror.test", LOOP)],
            &short_limits(),
        );
        let m = refusal(fetch(
            &t,
            &Locator::Http(Url::parse("http://user:pw@mirror.test:1/x").expect("url")),
            100,
        ));
        assert!(m.contains("credentials"), "{m}");
    }

    #[test]
    fn redirect_to_an_allowlisted_host_is_followed() {
        let server = TestServer::start(|req| {
            if req.path == "/start" {
                Reply::redirect(302, "/final")
            } else {
                Reply::ok(b"done".to_vec())
            }
        });
        let t = transport(
            &["mirror.test"],
            vec![("mirror.test", LOOP)],
            &short_limits(),
        );
        let (r, body) = fetch(&t, &url(&server, "mirror.test", "/start"), 100).expect("fetch");
        assert_eq!((r, body.as_slice()), (Fetched::Body(4), &b"done"[..]));
        assert_eq!(server.requests().len(), 2);
    }

    #[test]
    fn redirect_off_host_refused() {
        let server = TestServer::start(|_| Reply::redirect(302, "http://other.test:9/x"));
        let t = transport(
            &["mirror.test"],
            vec![("mirror.test", LOOP), ("other.test", LOOP)],
            &short_limits(),
        );
        let m = refusal(fetch(&t, &url(&server, "mirror.test", "/x"), 100));
        assert!(
            m.contains("package mirror redirect to 'other.test' is not allowed"),
            "{m}"
        );
        assert_eq!(
            server.requests().len(),
            1,
            "the off-host target was never contacted"
        );
    }

    #[test]
    fn redirect_to_the_metadata_address_refused() {
        // The metadata IP as a host name that is allowlisted and resolves there: the strict pin
        // still refuses it. As a literal it fails the allowlist.
        let server = TestServer::start(|_| Reply::redirect(307, "http://metadata.test/latest"));
        let t = transport(
            &["mirror.test", "metadata.test"],
            vec![
                ("mirror.test", LOOP),
                (
                    "metadata.test",
                    IpAddr::V4(Ipv4Addr::new(169, 254, 169, 254)),
                ),
            ],
            &short_limits(),
        );
        let m = refusal(fetch(&t, &url(&server, "mirror.test", "/x"), 100));
        assert!(m.contains("'metadata.test' is not a public address"), "{m}");
        let server = TestServer::start(|_| Reply::redirect(307, "http://169.254.169.254/latest"));
        let m = refusal(fetch(&t, &url(&server, "mirror.test", "/x"), 100));
        assert!(
            m.contains("redirect to '169.254.169.254' is not allowed"),
            "{m}"
        );
    }

    #[test]
    fn redirect_from_https_to_http_refused() {
        // Without a TLS server, assert the downgrade rule on the policy check itself.
        let t = transport(
            &["mirror.test"],
            vec![("mirror.test", LOOP)],
            &short_limits(),
        );
        let down = Url::parse("http://mirror.test/x").expect("url");
        let prod = HttpTransport::new(vec!["mirror.test".into()], &short_limits());
        match prod.check_url(&down, true) {
            Err(DepsError::Refused(m)) => assert!(m.contains("redirect left https"), "{m}"),
            other => panic!("{other:?}"),
        }
        // A test transport that allows http for loopback still sees an https mirror as https.
        assert!(t
            .check_url(&Url::parse("https://mirror.test/x").expect("url"), true)
            .is_ok());
    }

    #[test]
    fn redirect_loop_stops_at_the_fourth_hop() {
        let server = TestServer::start(|req| {
            let n: usize = req.path.trim_start_matches("/hop").parse().unwrap_or(0);
            Reply::redirect(302, &format!("/hop{}", n + 1))
        });
        let t = transport(
            &["mirror.test"],
            vec![("mirror.test", LOOP)],
            &short_limits(),
        );
        let m = refusal(fetch(&t, &url(&server, "mirror.test", "/hop0"), 100));
        assert!(m.contains("redirected more than 3 times"), "{m}");
        assert_eq!(
            server.requests().len(),
            4,
            "the initial request and three followed hops"
        );
    }

    #[test]
    fn redirect_without_location_is_refused() {
        let server = TestServer::start(|_| Reply::status(302, &[], b""));
        let t = transport(
            &["mirror.test"],
            vec![("mirror.test", LOOP)],
            &short_limits(),
        );
        assert!(refusal(fetch(&t, &url(&server, "mirror.test", "/x"), 100))
            .contains("without a Location"));
    }

    #[test]
    fn body_over_cap() {
        // An honest oversized Content-Length is refused from the header; with no Content-Length at
        // all (chunked) the streaming counter does it. A header that understates the body cannot
        // enlarge it: the HTTP framing stops at the stated length.
        let big = vec![b'x'; 5000];
        let server = TestServer::start(move |_| Reply::ok(big.clone()));
        let t = transport(
            &["mirror.test"],
            vec![("mirror.test", LOOP)],
            &short_limits(),
        );
        let m = refusal(fetch(&t, &url(&server, "mirror.test", "/x"), 1000));
        assert!(m.contains("more than"), "{m}");
        let server = TestServer::start(|_| Reply::chunked(vec![b'z'; 5000]));
        let m = refusal(fetch(&t, &url(&server, "mirror.test", "/x"), 1000));
        assert!(
            m.contains("more than"),
            "counter enforced with no Content-Length: {m}"
        );
    }

    #[test]
    fn slow_body_times_out() {
        let server =
            TestServer::start(|_| Reply::drip(b"abcdefghij".to_vec(), Duration::from_millis(300)));
        let t = transport(
            &["mirror.test"],
            vec![("mirror.test", LOOP)],
            &short_limits(),
        );
        let started = Instant::now();
        match fetch(&t, &url(&server, "mirror.test", "/x"), 1000) {
            Err(DepsError::Unavailable(m)) => assert!(
                m.contains("took too long")
                    || m.contains("timed out")
                    || m.contains("stopped sending"),
                "{m}"
            ),
            other => panic!("expected Unavailable, got {other:?}"),
        }
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn idle_stall_times_out() {
        let server = TestServer::start(|_| Reply::stall_after(b"abc".to_vec()));
        let t = transport(
            &["mirror.test"],
            vec![("mirror.test", LOOP)],
            &short_limits(),
        );
        assert!(matches!(
            fetch(&t, &url(&server, "mirror.test", "/x"), 1000),
            Err(DepsError::Unavailable(_))
        ));
    }

    #[test]
    fn deadline_between_reads_is_unavailable() {
        let server = TestServer::start(|_| Reply::ok(b"x".to_vec()));
        let t = transport(
            &["mirror.test"],
            vec![("mirror.test", LOOP)],
            &short_limits(),
        );
        let mut out = Vec::new();
        let r = t.get(
            &url(&server, "mirror.test", "/x"),
            false,
            10,
            Instant::now(),
            &mut out,
        );
        assert!(matches!(r, Err(DepsError::Unavailable(m)) if m.contains("time limit")));
        assert_eq!(server.requests().len(), 0);
    }

    #[test]
    fn content_encoding_refused() {
        // `br` is not decoded by this build, so the header is visible and refused. (If the
        // workspace feature set adds gzip, reqwest would decode that one itself; the byte cap
        // still bounds it.)
        let server =
            TestServer::start(|_| Reply::status(200, &[("Content-Encoding", "br")], b"data"));
        let t = transport(
            &["mirror.test"],
            vec![("mirror.test", LOOP)],
            &short_limits(),
        );
        let m = refusal(fetch(&t, &url(&server, "mirror.test", "/x"), 100));
        assert!(m.contains("Content-Encoding 'br'"), "{m}");
        let server =
            TestServer::start(|_| Reply::status(200, &[("Content-Encoding", "identity")], b"ok"));
        assert!(fetch(&t, &url(&server, "mirror.test", "/x"), 100).is_ok());
    }

    #[test]
    fn not_found_and_server_errors() {
        let t = transport(
            &["mirror.test"],
            vec![("mirror.test", LOOP)],
            &short_limits(),
        );
        let server = TestServer::start(|_| Reply::status(404, &[], b"no"));
        assert_eq!(
            fetch(&t, &url(&server, "mirror.test", "/x"), 100)
                .expect("fetch")
                .0,
            Fetched::NotFound
        );
        for code in [429u16, 500, 502, 503] {
            let server = TestServer::start(move |_| Reply::status(code, &[], b"busy"));
            match fetch(&t, &url(&server, "mirror.test", "/x"), 100) {
                Err(DepsError::Unavailable(m)) => assert!(m.contains(&code.to_string()), "{m}"),
                other => panic!("{code}: {other:?}"),
            }
        }
    }

    #[test]
    fn dns_failure_and_connect_failure_are_unavailable() {
        let t = transport(&["nowhere.test"], vec![], &short_limits());
        match fetch(
            &t,
            &Locator::Http(Url::parse("http://nowhere.test:9/x").expect("url")),
            100,
        ) {
            Err(DepsError::Unavailable(m)) => assert!(m.contains("could not be resolved"), "{m}"),
            other => panic!("{other:?}"),
        }
        // Nothing listens on this port (bound then dropped).
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
            l.local_addr().expect("addr").port()
        };
        let t = transport(
            &["mirror.test"],
            vec![("mirror.test", LOOP)],
            &short_limits(),
        );
        let loc = Locator::Http(Url::parse(&format!("http://mirror.test:{port}/x")).expect("url"));
        assert!(
            matches!(fetch(&t, &loc, 100), Err(DepsError::Unavailable(m)) if m.contains("could not be reached"))
        );
    }

    #[test]
    fn connect_timeout_is_unavailable() {
        // 192.0.2.1 (TEST-NET-1) is restricted by the strict policy, so allow it for this probe.
        let t = HttpTransport::for_tests(
            FixedResolver(vec![("slow.test", IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)))]),
            |_| false,
            true,
            &["slow.test"],
            &Limits {
                connect_timeout: Duration::from_millis(200),
                request_timeout: Duration::from_millis(400),
                ..Limits::default()
            },
        );
        let started = Instant::now();
        let r = fetch(
            &t,
            &Locator::Http(Url::parse("http://slow.test/x").expect("url")),
            100,
        );
        assert!(matches!(r, Err(DepsError::Unavailable(_))), "{r:?}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    // ---- file transport ----

    fn file_root() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = std::fs::canonicalize(dir.path()).expect("canon");
        (dir, root)
    }

    #[test]
    fn file_transport_reads_files_under_the_root() {
        let (_g, root) = file_root();
        std::fs::write(root.join("a.txt"), b"abc").expect("write");
        let t = FileTransport::new(root.clone());
        let mut out = Vec::new();
        let r = t
            .get(
                &Locator::File(root.join("a.txt")),
                false,
                10,
                far(),
                &mut out,
            )
            .expect("get");
        assert_eq!((r, out.as_slice()), (Fetched::Body(3), &b"abc"[..]));
        let mut out = Vec::new();
        assert_eq!(
            t.get(
                &Locator::File(root.join("missing")),
                false,
                10,
                far(),
                &mut out
            )
            .expect("get"),
            Fetched::NotFound
        );
    }

    #[test]
    fn file_mirror_escape_refused() {
        let (_g, root) = file_root();
        let (_g2, outside) = file_root();
        std::fs::write(outside.join("secret"), b"top secret").expect("write");
        std::fs::write(root.join("real"), b"x").expect("write");
        // A symlink to a file outside the root.
        std::os::unix::fs::symlink(outside.join("secret"), root.join("link")).expect("link");
        // A directory symlink pointing out, with a real file behind it.
        std::os::unix::fs::symlink(&outside, root.join("dirlink")).expect("link");
        // A `..` in the path.
        let t = FileTransport::new(root.clone());
        for path in [
            root.join("link"),
            root.join("dirlink/secret"),
            root.join("../")
                .join(outside.file_name().expect("name"))
                .join("secret"),
        ] {
            let mut out = Vec::new();
            let r = t.get(&Locator::File(path.clone()), false, 100, far(), &mut out);
            assert!(
                matches!(r, Err(DepsError::Refused(_))),
                "{}: {r:?}",
                path.display()
            );
            assert!(out.is_empty(), "nothing leaked from {}", path.display());
        }
        // A directory is not a file.
        std::fs::create_dir(root.join("d")).expect("mkdir");
        let mut out = Vec::new();
        assert!(matches!(
            t.get(&Locator::File(root.join("d")), false, 100, far(), &mut out),
            Err(DepsError::Refused(_))
        ));
    }

    #[test]
    fn file_transport_enforces_the_cap() {
        let (_g, root) = file_root();
        std::fs::write(root.join("big"), vec![0u8; 5000]).expect("write");
        let t = FileTransport::new(root.clone());
        let mut out = Vec::new();
        let r = t.get(
            &Locator::File(root.join("big")),
            false,
            1000,
            far(),
            &mut out,
        );
        assert!(matches!(r, Err(DepsError::Refused(m)) if m.contains("more than")));
    }
}
