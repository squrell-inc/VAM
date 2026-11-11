//! A loopback CONNECT proxy that only dials public addresses on port 443.
//!
//! `git` fetches a marketplace over https. Its own DNS lookup and connect cannot be checked by
//! vamd: a name can answer with a public address when vamd looks and an internal one when git
//! connects (DNS rebinding), and a redirect can point anywhere. So `git` is told to use this proxy
//! (`http.proxy`). For every `CONNECT host:port` the proxy:
//!
//! 1. refuses any port but 443 and any malformed target;
//! 2. resolves the host itself and refuses it if **any** answer is private, loopback, link-local,
//!    ULA, CGNAT, multicast, documentation or a cloud metadata address (an IP literal is classified
//!    the same way, in every spelling the resolver accepts);
//! 3. dials one of the addresses it just checked (never resolves a second time), and only then
//!    answers `200` and splices bytes. The TLS handshake runs end to end between `git` and the
//!    marketplace host, so the certificate is still checked against the real name.
//!
//! The tunnel has an idle timeout, a total deadline and a byte cap. It listens on 127.0.0.1 and
//! starts once per process.
//!
//! **Only vamd's own `git` may use it.** The port is reachable by every local process (in a
//! process-guest test node that includes a guest tool, which could otherwise tunnel to any public
//! host and skip its egress policy). So each `git` call leases a fresh random token (256 bits) that
//! git sends as `Proxy-Authorization: Basic` on its first CONNECT (`http.proxyAuthMethod=basic`);
//! the proxy answers `407` before it parses the target or resolves anything unless the token is a
//! live lease (compared in constant time), and the lease ends when the call does. The token is
//! scrubbed from every error text and never logged.

use std::io::{self, Read, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use vam_security::ssrf_guard::{HostResolver, SystemResolver};

use super::url::{is_refused_ip, HTTPS_PORT};

const HEADER_CAP: usize = 8 * 1024;
const HEADER_TIMEOUT: Duration = Duration::from_secs(10);
const DIAL_TIMEOUT: Duration = Duration::from_secs(10);
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const TUNNEL_DEADLINE: Duration = Duration::from_secs(10 * 60);
/// Bytes one tunnel may carry in both directions. A marketplace fetch is small; a host that keeps
/// sending is cut off long before the disk cap matters.
const TUNNEL_BYTES: u64 = 256 * 1024 * 1024;
const MAX_TUNNELS: usize = 16;

/// What the proxy allows. Production: port 443, every address the SSRF classifier accepts.
#[derive(Clone)]
pub struct GuardPolicy {
    pub port: u16,
    pub refuse_ip: fn(IpAddr) -> bool,
    pub resolver: Arc<dyn HostResolver + Send + Sync>,
}

impl GuardPolicy {
    pub fn production() -> Self {
        Self {
            port: HTTPS_PORT,
            refuse_ip: is_refused_ip,
            resolver: Arc::new(SystemResolver),
        }
    }
}

/// The user name git sends to the proxy (the token is the password).
const PROXY_USER: &str = "vam";

/// The tokens of the `git` calls running now.
type Tokens = Arc<Mutex<Vec<String>>>;

/// A running proxy.
pub struct GuardProxy {
    addr: SocketAddr,
    tokens: Tokens,
}

/// Permission for one `git` call to use the proxy. Dropping it revokes the token.
pub struct ProxyLease {
    url: String,
    token: String,
    tokens: Tokens,
}

impl ProxyLease {
    /// `http://vam:<token>@127.0.0.1:<port>`, the value of git's `http.proxy`. Secret: never log it.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The token, for scrubbing it out of text.
    pub fn token(&self) -> &str {
        &self.token
    }
}

impl Drop for ProxyLease {
    fn drop(&mut self) {
        let mut t = self.tokens.lock().unwrap_or_else(|e| e.into_inner());
        t.retain(|x| x != &self.token);
    }
}

/// 256 random bits as hex.
fn new_token() -> String {
    use rand::RngCore;
    let mut b = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut b);
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Equality that does not stop at the first difference.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = (a.len() ^ b.len()) as u8;
    for i in 0..a.len().max(b.len()) {
        diff |= a.get(i).copied().unwrap_or(0) ^ b.get(i).copied().unwrap_or(0);
    }
    diff == 0
}

/// True when the request head carries `Proxy-Authorization: Basic <vam:token>` for a live lease.
fn authorized(head: &str, tokens: &Tokens) -> bool {
    use base64::Engine;
    let Some(value) = head.lines().skip(1).find_map(|l| {
        let (name, value) = l.split_once(':')?;
        name.trim()
            .eq_ignore_ascii_case("proxy-authorization")
            .then(|| value.trim().to_string())
    }) else {
        return false;
    };
    let Some(b64) = value
        .strip_prefix("Basic ")
        .or_else(|| value.strip_prefix("basic "))
    else {
        return false;
    };
    let Ok(raw) = base64::engine::general_purpose::STANDARD.decode(b64.trim()) else {
        return false;
    };
    let Some(token) = raw.strip_prefix(format!("{PROXY_USER}:").as_bytes()) else {
        return false;
    };
    let live = tokens.lock().unwrap_or_else(|e| e.into_inner());
    // Compare against every live token without an early exit.
    live.iter()
        .fold(false, |ok, t| ct_eq(token, t.as_bytes()) | ok)
}

impl GuardProxy {
    /// Binds 127.0.0.1 on a free port and serves from a background thread for the life of the
    /// process.
    pub fn start(policy: GuardPolicy) -> io::Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        let addr = listener.local_addr()?;
        let live = Arc::new(AtomicUsize::new(0));
        let tokens: Tokens = Arc::new(Mutex::new(Vec::new()));
        let serve_tokens = tokens.clone();
        std::thread::Builder::new()
            .name("marketplace-guard".into())
            .spawn(move || {
                for conn in listener.incoming() {
                    let Ok(client) = conn else { continue };
                    if live.fetch_add(1, Ordering::SeqCst) >= MAX_TUNNELS {
                        live.fetch_sub(1, Ordering::SeqCst);
                        let _ = reply(&client, "503 Service Unavailable");
                        continue;
                    }
                    let (policy, live, tokens) =
                        (policy.clone(), live.clone(), serve_tokens.clone());
                    let spawned = std::thread::Builder::new()
                        .name("marketplace-guard-conn".into())
                        .spawn({
                            let live = live.clone();
                            move || {
                                let _ = serve(client, &policy, &tokens);
                                live.fetch_sub(1, Ordering::SeqCst);
                            }
                        });
                    if spawned.is_err() {
                        live.fetch_sub(1, Ordering::SeqCst);
                    }
                }
            })?;
        Ok(Self { addr, tokens })
    }

    /// Permission for one `git` call; revoked when the lease is dropped.
    pub fn lease(&self) -> ProxyLease {
        let token = new_token();
        self.tokens
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(token.clone());
        ProxyLease {
            url: format!("http://{PROXY_USER}:{token}@{}", self.addr),
            token,
            tokens: self.tokens.clone(),
        }
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }
}

static PROXY: OnceLock<Result<GuardProxy, String>> = OnceLock::new();

/// The process-wide proxy (production policy), started on first use.
pub fn shared() -> Result<&'static GuardProxy, String> {
    PROXY
        .get_or_init(|| {
            GuardProxy::start(GuardPolicy::production()).map_err(|e| format!("guard proxy: {e}"))
        })
        .as_ref()
        .map_err(Clone::clone)
}

fn reply(mut s: &TcpStream, status: &str) -> io::Result<()> {
    write!(
        s,
        "HTTP/1.1 {status}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n"
    )?;
    s.flush()
}

/// Reads the request head (up to the blank line), at most `HEADER_CAP` bytes.
fn read_head(client: &mut TcpStream) -> io::Result<Vec<u8>> {
    client.set_read_timeout(Some(HEADER_TIMEOUT))?;
    let mut head = Vec::with_capacity(256);
    let mut byte = [0u8; 1];
    while head.len() < HEADER_CAP {
        if client.read(&mut byte)? == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        head.push(byte[0]);
        if head.ends_with(b"\r\n\r\n") {
            return Ok(head);
        }
    }
    Err(io::Error::new(io::ErrorKind::InvalidData, "head too large"))
}

/// `host:port` of a CONNECT target; the host is a DNS name or an IP literal (IPv6 in brackets).
fn parse_authority(target: &str) -> Option<(String, u16)> {
    let (host, port) = if let Some(rest) = target.strip_prefix('[') {
        let (h, p) = rest.split_once("]:")?;
        (h, p)
    } else {
        target.rsplit_once(':')?
    };
    let port: u16 = port.parse().ok()?;
    let ok = !host.is_empty()
        && host.len() <= 253
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".-:".contains(&b));
    ok.then(|| (host.to_ascii_lowercase(), port))
}

/// Resolves `host` and returns the addresses to dial, or the reason it must not be dialled.
pub fn allowed_addrs(
    host: &str,
    port: u16,
    policy: &GuardPolicy,
) -> Result<Vec<SocketAddr>, String> {
    if port != policy.port {
        return Err(format!("port {port} is not allowed (only {})", policy.port));
    }
    let addrs = match host.parse::<IpAddr>() {
        Ok(ip) => vec![SocketAddr::new(ip, port)],
        Err(_) => policy
            .resolver
            .lookup(host, port)
            .map_err(|_| format!("`{host}` could not be resolved"))?,
    };
    if addrs.is_empty() {
        return Err(format!("`{host}` did not resolve to any address"));
    }
    if let Some(a) = addrs.iter().find(|a| (policy.refuse_ip)(a.ip())) {
        return Err(format!(
            "`{host}` resolves to {}, a refused address",
            a.ip()
        ));
    }
    Ok(addrs)
}

fn serve(mut client: TcpStream, policy: &GuardPolicy, tokens: &Tokens) -> io::Result<()> {
    let head = read_head(&mut client)?;
    let text = String::from_utf8_lossy(&head);
    // Before anything is parsed, resolved or dialled.
    if !authorized(&text, tokens) {
        client.write_all(
            b"HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: Basic realm=\"vamd\"\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
        )?;
        return client.flush();
    }
    let line = text.lines().next().unwrap_or_default();
    let mut parts = line.split(' ');
    let (method, target, version) = (parts.next(), parts.next(), parts.next());
    if method != Some("CONNECT") || !version.is_some_and(|v| v.starts_with("HTTP/1.")) {
        return reply(&client, "405 Method Not Allowed");
    }
    let Some((host, port)) = target.and_then(parse_authority) else {
        return reply(&client, "400 Bad Request");
    };
    let addrs = match allowed_addrs(&host, port, policy) {
        Ok(a) => a,
        Err(why) => {
            tracing::warn!(%host, port, "marketplace guard refused a target: {why}");
            return reply(&client, "403 Forbidden");
        }
    };
    // Dial one of the addresses that were just checked; nothing is resolved again.
    let upstream = addrs
        .iter()
        .find_map(|a| TcpStream::connect_timeout(a, DIAL_TIMEOUT).ok());
    let Some(upstream) = upstream else {
        return reply(&client, "502 Bad Gateway");
    };
    client.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")?;
    client.flush()?;
    tunnel(client, upstream)
}

fn tunnel(client: TcpStream, upstream: TcpStream) -> io::Result<()> {
    let deadline = Instant::now() + TUNNEL_DEADLINE;
    let moved = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let (c2, u2) = (client.try_clone()?, upstream.try_clone()?);
    let up = {
        let moved = moved.clone();
        std::thread::spawn(move || pump(client, u2, deadline, &moved))
    };
    pump(upstream, c2, deadline, &moved);
    let _ = up.join();
    Ok(())
}

/// Copies `from` to `to` until EOF, an idle timeout, the deadline or the byte cap; then closes the
/// write side of `to` and the read side of `from`.
fn pump(
    mut from: TcpStream,
    mut to: TcpStream,
    deadline: Instant,
    moved: &std::sync::atomic::AtomicU64,
) {
    let _ = from.set_read_timeout(Some(IDLE_TIMEOUT));
    let _ = to.set_write_timeout(Some(IDLE_TIMEOUT));
    let mut buf = [0u8; 16 * 1024];
    loop {
        if Instant::now() >= deadline {
            break;
        }
        match from.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if moved.fetch_add(n as u64, Ordering::SeqCst) + n as u64 > TUNNEL_BYTES {
                    break;
                }
                if to.write_all(&buf[..n]).is_err() {
                    break;
                }
            }
        }
    }
    let _ = to.shutdown(Shutdown::Both);
    let _ = from.shutdown(Shutdown::Both);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    struct Fixed(Vec<IpAddr>);
    impl HostResolver for Fixed {
        fn lookup(&self, _host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
            Ok(self.0.iter().map(|ip| SocketAddr::new(*ip, port)).collect())
        }
    }

    fn policy(answer: &str, allow_all: bool) -> GuardPolicy {
        fn none(_: IpAddr) -> bool {
            false
        }
        GuardPolicy {
            port: HTTPS_PORT,
            refuse_ip: if allow_all { none } else { is_refused_ip },
            resolver: Arc::new(Fixed(vec![answer.parse().unwrap()])),
        }
    }

    /// Sends a request head to a proxy and returns the status line of the answer.
    fn status_of(proxy: &GuardProxy, head: &str) -> String {
        let mut s = TcpStream::connect(proxy.addr()).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        s.write_all(head.as_bytes()).unwrap();
        let mut out = Vec::new();
        let mut buf = [0u8; 512];
        while let Ok(n) = s.read(&mut buf) {
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
            if out.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        String::from_utf8_lossy(&out)
            .lines()
            .next()
            .unwrap_or_default()
            .to_string()
    }

    #[test]
    fn authorities_are_parsed_strictly() {
        assert_eq!(
            parse_authority("GitHub.com:443"),
            Some(("github.com".to_string(), 443))
        );
        assert_eq!(parse_authority("[::1]:443"), Some(("::1".to_string(), 443)));
        for bad in [
            "github.com",
            "github.com:",
            "github.com:99999",
            ":443",
            "a b:443",
            "a/b:443",
            "a@b:443",
        ] {
            assert_eq!(parse_authority(bad), None, "{bad}");
        }
    }

    #[test]
    fn only_port_443_and_public_answers_pass() {
        let p = policy("140.82.112.3", false);
        assert!(allowed_addrs("github.com", 443, &p).is_ok());
        assert!(allowed_addrs("github.com", 22, &p).is_err());
        assert!(allowed_addrs("github.com", 8443, &p).is_err());
        for private in [
            "127.0.0.1",
            "10.0.0.7",
            "169.254.169.254",
            "::1",
            "fd00::1",
            "100.100.100.200",
        ] {
            let p = policy(private, false);
            let err = allowed_addrs("rebind.example.com", 443, &p).unwrap_err();
            assert!(err.contains("refused address"), "{private}: {err}");
        }
        // An IP literal is classified without a lookup, in the spellings the resolver would accept.
        let any = policy("140.82.112.3", false);
        for literal in [
            "127.0.0.1",
            "10.1.1.1",
            "169.254.169.254",
            "::1",
            "::ffff:10.0.0.1",
        ] {
            assert!(allowed_addrs(literal, 443, &any).is_err(), "{literal}");
        }
        assert!(allowed_addrs("8.8.8.8", 443, &any).is_ok());
    }

    /// A request line plus the `Proxy-Authorization` header of a lease.
    fn authed(lease: &ProxyLease, line: &str) -> String {
        use base64::Engine;
        let cred =
            base64::engine::general_purpose::STANDARD.encode(format!("vam:{}", lease.token()));
        format!("{line}\r\nProxy-Authorization: Basic {cred}\r\n\r\n")
    }

    #[test]
    fn requests_other_than_connect_and_bad_targets_are_refused() {
        let proxy = GuardProxy::start(policy("140.82.112.3", false)).unwrap();
        let l = proxy.lease();
        let st = |line: &str| status_of(&proxy, &authed(&l, line));
        assert!(st("GET http://github.com/ HTTP/1.1").contains("405"));
        assert!(st("CONNECT github.com HTTP/1.1").contains("400"));
        assert!(st("CONNECT github.com:22 HTTP/1.1").contains("403"));
        assert!(st("CONNECT 127.0.0.1:443 HTTP/1.1").contains("403"));
        assert!(st("CONNECT [::1]:443 HTTP/1.1").contains("403"));
    }

    #[test]
    fn a_rebinding_name_is_refused_at_connect_time() {
        let proxy = GuardProxy::start(policy("10.0.0.5", false)).unwrap();
        let l = proxy.lease();
        let st = status_of(
            &proxy,
            &authed(&l, "CONNECT looks-public.example.com:443 HTTP/1.1"),
        );
        assert!(st.contains("403"), "{st}");
    }

    /// An "echo" target on loopback whose accepts are counted, and a policy that allows only it.
    fn counting_target() -> (u16, Arc<AtomicUsize>, GuardPolicy) {
        let echo = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = echo.local_addr().unwrap().port();
        let accepts = Arc::new(AtomicUsize::new(0));
        let n = accepts.clone();
        std::thread::spawn(move || {
            while let Ok((mut c, _)) = echo.accept() {
                n.fetch_add(1, Ordering::SeqCst);
                let mut buf = [0u8; 64];
                if let Ok(k) = c.read(&mut buf) {
                    let _ = c.write_all(&buf[..k]);
                }
            }
        });
        let mut p = policy("127.0.0.1", true);
        p.port = port;
        (port, accepts, p)
    }

    #[test]
    fn a_request_without_the_lease_token_gets_407_and_nothing_is_dialled() {
        let (port, accepts, p) = counting_target();
        let proxy = GuardProxy::start(p).unwrap();
        let line = format!("CONNECT echo.example.com:{port} HTTP/1.1");
        // No header at all.
        let st = status_of(&proxy, &format!("{line}\r\n\r\n"));
        assert!(st.contains("407"), "{st}");
        // A header with the wrong secret, the wrong user, a different scheme, and garbage.
        use base64::Engine;
        let b64 = |t: &str| base64::engine::general_purpose::STANDARD.encode(t);
        let real = proxy.lease();
        for value in [
            format!("Basic {}", b64(&format!("vam:{}", "0".repeat(64)))),
            format!("Basic {}", b64(&format!("vam:{}x", real.token()))),
            format!("Basic {}", b64(&format!("vam:{}", &real.token()[..63]))),
            format!("Basic {}", b64(&format!("other:{}", real.token()))),
            format!("Basic {}", b64(real.token())),
            format!("Bearer {}", real.token()),
            "Basic !!!not-base64".to_string(),
            "Basic ".to_string(),
        ] {
            let st = status_of(
                &proxy,
                &format!("{line}\r\nProxy-Authorization: {value}\r\n\r\n"),
            );
            assert!(st.contains("407"), "{value}: {st}");
        }
        // Even a refused target is not looked at before the token is: no 403, only 407.
        let st = status_of(&proxy, "CONNECT 127.0.0.1:22 HTTP/1.1\r\n\r\n");
        assert!(st.contains("407"), "{st}");
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(accepts.load(Ordering::SeqCst), 0, "the target was dialled");
    }

    #[test]
    fn a_revoked_lease_stops_working_and_leases_are_independent() {
        let (port, accepts, p) = counting_target();
        let proxy = GuardProxy::start(p).unwrap();
        let line = format!("CONNECT echo.example.com:{port} HTTP/1.1");
        let (a, b) = (proxy.lease(), proxy.lease());
        assert_ne!(a.token(), b.token());
        assert_eq!(a.token().len(), 64);
        let a_head = authed(&a, &line);
        drop(a);
        let st = status_of(&proxy, &a_head);
        assert!(st.contains("407"), "a dropped lease is revoked: {st}");
        assert_eq!(accepts.load(Ordering::SeqCst), 0);
        // The other lease still works (status_of stops at the 200 head).
        let st = status_of(&proxy, &authed(&b, &line));
        assert!(st.contains("200"), "{st}");
    }

    #[test]
    fn the_token_is_never_in_a_url_the_log_or_an_error_by_accident() {
        let proxy = GuardProxy::start(policy("140.82.112.3", false)).unwrap();
        let l = proxy.lease();
        // The URL carries the token (git needs it); nothing else formats it.
        assert!(l.url().starts_with("http://vam:") && l.url().contains(l.token()));
        assert!(l.url().ends_with(&proxy.addr().to_string()));
        assert!(ct_eq(b"abc", b"abc") && !ct_eq(b"abc", b"abd") && !ct_eq(b"abc", b"ab"));
    }

    #[test]
    fn an_allowed_target_is_tunnelled_with_the_right_token() {
        // The "marketplace host" is a loopback echo server; the test policy lets loopback through
        // on its port and nothing else, so this exercises the splice and not the policy.
        let (port, accepts, p) = counting_target();
        let proxy = GuardProxy::start(p).unwrap();
        let lease = proxy.lease();
        let mut s = TcpStream::connect(proxy.addr()).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        s.write_all(
            authed(&lease, &format!("CONNECT echo.example.com:{port} HTTP/1.1")).as_bytes(),
        )
        .unwrap();
        let mut head = Vec::new();
        let mut b = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            assert_eq!(s.read(&mut b).unwrap(), 1);
            head.push(b[0]);
        }
        assert!(String::from_utf8_lossy(&head).starts_with("HTTP/1.1 200"));
        s.write_all(b"ping").unwrap();
        let mut got = [0u8; 4];
        s.read_exact(&mut got).unwrap();
        assert_eq!(&got, b"ping");
        assert_eq!(accepts.load(Ordering::SeqCst), 1);
    }
}
