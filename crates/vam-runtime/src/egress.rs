use prost::bytes::Bytes;
use prost::Message;
use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, UnixListener};
use tokio::sync::{mpsc, oneshot};

use vam_spec::v1::{EgressRequest, EgressResponse, VsockEnvelope};
use vam_vsock::frame::{Frame, FrameType};

pub struct EgressClient {
    request_tx: mpsc::Sender<Frame>,
    pending: Arc<Mutex<HashMap<u32, oneshot::Sender<VsockEnvelope>>>>,
    next_cid: AtomicU32,
}

impl EgressClient {
    pub fn new<S>(stream: S) -> Self
    where
        S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
    {
        let (mut reader, mut writer) = tokio::io::split(stream);
        let (request_tx, mut request_rx) = mpsc::channel::<Frame>(128);
        let pending = Arc::new(Mutex::new(
            HashMap::<u32, oneshot::Sender<VsockEnvelope>>::new(),
        ));
        let pending_clone = pending.clone();

        tokio::spawn(async move {
            while let Some(frame) = request_rx.recv().await {
                if let Err(e) = frame.write_to(&mut writer).await {
                    eprintln!("[guest-egress] writer error: {}", e);
                    break;
                }
            }
        });

        tokio::spawn(async move {
            while let Ok(frame) = Frame::read_from(&mut reader).await {
                {
                    if frame.frame_type != FrameType::StreamEgress {
                        continue;
                    }
                    if let Ok(envelope) = VsockEnvelope::decode(frame.payload.as_ref()) {
                        let mut map = pending_clone.lock().unwrap();
                        if let Some(tx) = map.remove(&frame.correlation_id) {
                            let _ = tx.send(envelope);
                        }
                    }
                }
            }
        });

        Self {
            request_tx,
            pending,
            next_cid: AtomicU32::new(1),
        }
    }

    pub async fn request(&self, req: EgressRequest) -> Result<EgressResponse, String> {
        let start = std::time::Instant::now();
        let method = req.method.clone();
        let cid = self.next_cid.fetch_add(1, Ordering::SeqCst);
        let mut payload = Vec::new();
        req.encode(&mut payload).map_err(|e| e.to_string())?;

        let envelope = VsockEnvelope {
            method: "Egress".to_string(),
            correlation_id: cid,
            payload,
            error: None,
            trace_context: None,
        };

        let mut env_bytes = Vec::new();
        envelope.encode(&mut env_bytes).map_err(|e| e.to_string())?;

        let frame = Frame {
            frame_type: FrameType::StreamEgress,
            correlation_id: cid,
            payload: Bytes::from(env_bytes),
        };

        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(cid, tx);

        self.request_tx
            .send(frame)
            .await
            .map_err(|_| "Vsock channel closed".to_string())?;

        let res_envelope = rx.await.map_err(|_| "Vsock response dropped".to_string())?;
        if let Some(err) = res_envelope.error {
            return Err(err.message);
        }

        let resp =
            EgressResponse::decode(res_envelope.payload.as_slice()).map_err(|e| e.to_string())?;
        let mut attrs = std::collections::HashMap::new();
        attrs.insert("http.method".to_string(), method);
        attrs.insert("http.status_code".to_string(), resp.status_code.to_string());
        crate::telemetry::global_telemetry().record_span(
            "egress.request",
            start.elapsed(),
            None,
            attrs,
        );
        Ok(resp)
    }
}

#[derive(Debug)]
pub struct ParsedHttpRequest {
    pub method: String,
    pub url: String,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

pub fn parse_http_request(bytes: &[u8]) -> Result<ParsedHttpRequest, String> {
    let delimiter = b"\r\n\r\n";
    let mut split_pos = None;
    for i in 0..=bytes.len().saturating_sub(delimiter.len()) {
        if &bytes[i..i + delimiter.len()] == delimiter {
            split_pos = Some(i);
            break;
        }
    }

    let header_part_len = split_pos.ok_or_else(|| "Incomplete headers".to_string())?;
    let header_bytes = &bytes[..header_part_len];
    let body = bytes[header_part_len + delimiter.len()..].to_vec();

    let header_str = std::str::from_utf8(header_bytes).map_err(|e| e.to_string())?;
    let mut lines = header_str.lines();

    let request_line = lines.next().ok_or_else(|| "Empty request".to_string())?;
    let mut req_parts = request_line.split_whitespace();
    let method = req_parts
        .next()
        .ok_or_else(|| "Missing method".to_string())?
        .to_string();
    let path = req_parts
        .next()
        .ok_or_else(|| "Missing path".to_string())?
        .to_string();

    let mut headers = HashMap::new();
    for line in lines {
        if let Some(pos) = line.find(':') {
            let key = line[..pos].trim().to_string();
            let val = line[pos + 1..].trim().to_string();
            headers.insert(key, val);
        }
    }

    let url = if let Some(target) = headers
        .get("X-Target-Url")
        .or_else(|| headers.get("x-target-url"))
    {
        target.clone()
    } else if path.starts_with("http://") || path.starts_with("https://") {
        path
    } else if let Some(host) = headers.get("Host").or_else(|| headers.get("host")) {
        format!("http://{}{}", host, path)
    } else {
        return Err("Missing Host header or absolute URI".to_string());
    };

    headers.remove("X-Target-Url");
    headers.remove("x-target-url");
    headers.remove("Host");
    headers.remove("host");

    Ok(ParsedHttpRequest {
        method,
        url,
        headers,
        body,
    })
}

pub fn required_request_len(bytes: &[u8]) -> Option<usize> {
    let delimiter = b"\r\n\r\n";
    let header_end = bytes
        .windows(delimiter.len())
        .position(|w| w == delimiter)?;
    let header_end = header_end + delimiter.len();

    let header_str = std::str::from_utf8(&bytes[..header_end - delimiter.len()]).ok()?;
    let mut content_length: usize = 0;
    for line in header_str.lines().skip(1) {
        if let Some(pos) = line.find(':') {
            let key = line[..pos].trim();
            if key.eq_ignore_ascii_case("content-length") {
                content_length = line[pos + 1..].trim().parse().ok()?;
                break;
            }
        }
    }

    Some(header_end + content_length)
}

pub fn format_http_response(resp: &EgressResponse) -> Vec<u8> {
    let mut out = Vec::new();
    let status_text = match resp.status_code {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        301 => "Moved Permanently",
        302 => "Found",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        _ => "Unknown",
    };
    out.extend_from_slice(format!("HTTP/1.1 {} {}\r\n", resp.status_code, status_text).as_bytes());

    // The host may have rewritten the body (secret scrub), so upstream framing
    // headers are never trusted: the length comes from the body we write.
    for (k, v) in &resp.headers {
        if k.eq_ignore_ascii_case("content-length")
            || k.eq_ignore_ascii_case("transfer-encoding")
            || k.eq_ignore_ascii_case("connection")
        {
            continue;
        }
        out.extend_from_slice(format!("{}: {}\r\n", k, v).as_bytes());
    }
    out.extend_from_slice(format!("Content-Length: {}\r\n", resp.body.len()).as_bytes());
    out.extend_from_slice(b"Connection: close\r\n");
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(&resp.body);
    out
}

async fn serve_proxy_connection<Sock>(mut socket: Sock, client: Arc<EgressClient>, _peer: &str)
where
    Sock: AsyncRead + AsyncWrite + Unpin,
{
    let mut buf = vec![0u8; 65536];
    let mut read_len = 0;
    loop {
        if read_len >= buf.len() {
            buf.resize(buf.len() * 2, 0);
        }
        match socket.read(&mut buf[read_len..]).await {
            Ok(0) => break,
            Ok(n) => {
                read_len += n;
                let Some(required) = required_request_len(&buf[..read_len]) else {
                    continue;
                };
                if read_len < required {
                    continue;
                }
                match parse_http_request(&buf[..read_len]) {
                    Ok(parsed) => {
                        eprintln!(
                            "[guest-proxy] Request received: {} {}",
                            parsed.method, parsed.url
                        );
                        if parsed.method.to_uppercase() == "CONNECT" {
                            let body_msg = b"CONNECT tunneling is supported directly through the network plane (TAP/NAT), but secret injection is only available via standard structured HTTP/HTTPS RPC requests.";
                            let mut resp_headers = HashMap::new();
                            resp_headers
                                .insert("Content-Type".to_string(), "text/plain".to_string());
                            resp_headers.insert("Connection".to_string(), "close".to_string());
                            let resp = EgressResponse {
                                status_code: 501,
                                headers: resp_headers,
                                body: body_msg.to_vec(),
                                error: None,
                            };
                            let resp_bytes = format_http_response(&resp);
                            let _ = socket.write_all(&resp_bytes).await;
                            break;
                        }

                        let req = EgressRequest {
                            method: parsed.method,
                            url: parsed.url,
                            headers: parsed.headers,
                            body: parsed.body,
                        };

                        match client.request(req).await {
                            Ok(resp) => {
                                eprintln!(
                                    "[guest-proxy] Response from host: HTTP {}",
                                    resp.status_code
                                );
                                let resp_bytes = format_http_response(&resp);
                                let _ = socket.write_all(&resp_bytes).await;
                            }
                            Err(err_msg) => {
                                eprintln!("[guest-proxy] Host Egress Broker error: {}", err_msg);
                                let resp = EgressResponse {
                                    status_code: 502,
                                    headers: HashMap::new(),
                                    body: format!("Bad Gateway: {}", err_msg).into_bytes(),
                                    error: None,
                                };
                                let resp_bytes = format_http_response(&resp);
                                let _ = socket.write_all(&resp_bytes).await;
                            }
                        }
                        break;
                    }
                    Err(e) => {
                        eprintln!("[guest-proxy] HTTP parse failed: {e}");
                    }
                }
            }
            Err(_) => break,
        }
    }
}

/// Default (microVM) proxy address.
pub const VM_PROXY_ADDR: &str = "127.0.0.1:3128";
/// Default (microVM) proxy unix socket.
pub const VM_PROXY_SOCK: &str = "/tmp/vam-egress.sock";

/// Where one guest's egress proxy listens. A process guest gets its own
/// endpoint so that parallel guests do not collide, and it is loopback only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EgressEndpoint {
    /// Loopback TCP address (port 0 = pick a free one), or none. A non-loopback
    /// address is never bound.
    pub tcp: Option<std::net::SocketAddr>,
    /// Unix socket path.
    pub unix: std::path::PathBuf,
    /// When `tcp` cannot be bound, try a free port on loopback before serving the
    /// unix socket only. The microVM guest sets this (its fixed port can be
    /// taken); a process guest does not.
    pub ephemeral_fallback: bool,
}

impl EgressEndpoint {
    /// The microVM endpoint: `127.0.0.1:3128` and `/tmp/vam-egress.sock`.
    #[cfg(not(feature = "process-guest"))]
    pub fn vm_default() -> Self {
        Self {
            tcp: VM_PROXY_ADDR.parse().ok(),
            unix: std::path::PathBuf::from(VM_PROXY_SOCK),
            ephemeral_fallback: true,
        }
    }

    /// A per-guest endpoint. `sock` must be absolute; `addr`, if given, must be
    /// a loopback `ip:port`. Anything else (`0.0.0.0`, `::`, a hostname) is
    /// refused, so a process guest can never listen beyond loopback.
    pub fn per_guest(sock: &str, addr: Option<&str>) -> Result<Self, String> {
        let unix = std::path::PathBuf::from(sock);
        if !unix.is_absolute() {
            return Err(format!("egress socket path {sock:?} must be absolute"));
        }
        let tcp = match addr {
            None => None,
            Some(a) => {
                let sa: std::net::SocketAddr = a
                    .parse()
                    .map_err(|e| format!("egress address {a:?} is not an ip:port: {e}"))?;
                if !sa.ip().is_loopback() {
                    return Err(format!("egress address {a} is not a loopback address"));
                }
                Some(sa)
            }
        };
        Ok(Self {
            tcp,
            unix,
            ephemeral_fallback: false,
        })
    }
}

/// What a started proxy actually listens on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundEgress {
    pub tcp: Option<std::net::SocketAddr>,
    pub unix: Option<std::path::PathBuf>,
}

static ADVERTISED_PROXY_URL: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Sets the proxy URL tool processes get by default (once; later calls are ignored).
pub fn advertise_proxy_url(url: String) {
    let _ = ADVERTISED_PROXY_URL.set(url);
}

/// The proxy URL tool processes get unless `HTTP_PROXY` says otherwise.
pub fn proxy_url() -> String {
    ADVERTISED_PROXY_URL
        .get()
        .cloned()
        .unwrap_or_else(|| format!("http://{VM_PROXY_ADDR}"))
}

/// microVM entry point. Serves `proxy_addr` (loopback only; anything else becomes
/// `127.0.0.1:3128`), falls back to a free loopback port when that is taken, and
/// always serves `/tmp/vam-egress.sock`. It advertises the port it got to tool
/// processes. There is no wildcard bind: with no loopback at all, only the unix
/// socket serves.
#[cfg(not(feature = "process-guest"))]
pub async fn start_guest_proxy<S>(proxy_addr: &str, vsock_stream: S) -> Result<BoundEgress, String>
where
    S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
{
    let mut endpoint = EgressEndpoint::vm_default();
    match proxy_addr.parse::<std::net::SocketAddr>() {
        Ok(addr) if addr.ip().is_loopback() => endpoint.tcp = Some(addr),
        _ => {
            eprintln!("[guest-proxy] {proxy_addr} is not a loopback ip:port; using {VM_PROXY_ADDR}")
        }
    }
    let bound = start_guest_proxy_with(&endpoint, vsock_stream).await?;
    if let Some(addr) = bound.tcp {
        advertise_proxy_url(format!("http://{addr}"));
    }
    Ok(bound)
}

/// Starts the proxy on an explicit endpoint. It never binds a non-loopback
/// address and has no wildcard fallback: if the TCP bind fails (and the free
/// loopback port, where the endpoint allows one, fails too), only the unix socket
/// serves.
pub async fn start_guest_proxy_with<S>(
    endpoint: &EgressEndpoint,
    vsock_stream: S,
) -> Result<BoundEgress, String>
where
    S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
{
    start_proxy_binding_with(endpoint, vsock_stream, TcpListener::bind).await
}

/// The one bind path. `bind` is the only thing that opens a TCP listener, so a
/// test can make any address fail.
async fn start_proxy_binding_with<S, B, F>(
    endpoint: &EgressEndpoint,
    vsock_stream: S,
    bind: B,
) -> Result<BoundEgress, String>
where
    S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
    B: Fn(std::net::SocketAddr) -> F,
    F: std::future::Future<Output = std::io::Result<TcpListener>>,
{
    let tcp = bind_loopback(endpoint, &bind).await;
    serve_guest_proxy(tcp, &endpoint.unix, vsock_stream).await
}

async fn bind_loopback<B, F>(endpoint: &EgressEndpoint, bind: &B) -> std::io::Result<TcpListener>
where
    B: Fn(std::net::SocketAddr) -> F,
    F: std::future::Future<Output = std::io::Result<TcpListener>>,
{
    let Some(preferred) = endpoint.tcp else {
        return Err(std::io::Error::other("tcp listener not requested"));
    };
    let mut last = if preferred.ip().is_loopback() {
        match bind(preferred).await {
            Ok(listener) => return Ok(listener),
            Err(e) => {
                eprintln!("[guest-proxy] bind {preferred} failed: {e}");
                e
            }
        }
    } else {
        eprintln!("[guest-proxy] {preferred} is not a loopback address; not binding it");
        std::io::Error::other(format!("{preferred} is not a loopback address"))
    };
    if endpoint.ephemeral_fallback {
        let ip: std::net::IpAddr = if preferred.is_ipv6() && preferred.ip().is_loopback() {
            std::net::Ipv6Addr::LOCALHOST.into()
        } else {
            std::net::Ipv4Addr::LOCALHOST.into()
        };
        let any_port = std::net::SocketAddr::new(ip, 0);
        if any_port != preferred {
            match bind(any_port).await {
                Ok(listener) => return Ok(listener),
                Err(e) => {
                    eprintln!("[guest-proxy] bind {any_port} failed: {e}");
                    last = e;
                }
            }
        }
    }
    Err(last)
}

async fn serve_guest_proxy<S>(
    tcp: std::io::Result<TcpListener>,
    sock_path: &std::path::Path,
    vsock_stream: S,
) -> Result<BoundEgress, String>
where
    S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
{
    let client = Arc::new(EgressClient::new(vsock_stream));

    #[cfg(unix)]
    let (unix_ok, unix_err, unix_listener) = {
        let _ = std::fs::remove_file(sock_path);
        let unix = UnixListener::bind(sock_path);
        if unix.is_ok() {
            let _ = std::fs::set_permissions(sock_path, std::fs::Permissions::from_mode(0o666));
        }
        let ok = unix.is_ok();
        let err = unix
            .as_ref()
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        let listener = unix.ok();
        (ok, err, listener)
    };
    #[cfg(not(unix))]
    let (unix_ok, unix_err, _unix_listener): (bool, String, Option<()>) = (
        false,
        "unix sockets not supported on this platform".to_string(),
        None,
    );

    let tcp_ok = tcp.is_ok();
    let tcp_err = tcp
        .as_ref()
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
    let bound = BoundEgress {
        tcp: tcp.as_ref().ok().and_then(|l| l.local_addr().ok()),
        unix: unix_ok.then(|| sock_path.to_path_buf()),
    };
    eprintln!("[guest-proxy] listeners tcp_ok={tcp_ok} unix_ok={unix_ok} tcp_err={tcp_err} unix_err={unix_err}");

    if !tcp_ok && !unix_ok {
        return Err(format!("no proxy listener: tcp={tcp_err} unix={unix_err}"));
    }

    if let Ok(listener) = tcp {
        let client_clone = client.clone();
        tokio::spawn(async move {
            loop {
                if let Ok((socket, peer)) = listener.accept().await {
                    let client_clone = client_clone.clone();
                    tokio::spawn(async move {
                        serve_proxy_connection(socket, client_clone, &peer.to_string()).await;
                    });
                }
            }
        });
    }

    #[cfg(unix)]
    if let Some(listener) = unix_listener {
        let client_clone = client.clone();
        tokio::spawn(async move {
            loop {
                if let Ok((socket, _)) = listener.accept().await {
                    let client_clone = client_clone.clone();
                    tokio::spawn(async move {
                        serve_proxy_connection(socket, client_clone, "unix").await;
                    });
                }
            }
        });
    }

    Ok(bound)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_http_request() {
        let req_bytes = b"GET /v1/charges HTTP/1.1\r\nHost: api.stripe.com\r\nAuthorization: Bearer test_token\r\n\r\n";
        let parsed = parse_http_request(req_bytes).unwrap();
        assert_eq!(parsed.method, "GET");
        assert_eq!(parsed.url, "http://api.stripe.com/v1/charges");
        assert!(!parsed.headers.contains_key("Host"));
        assert!(!parsed.headers.contains_key("host"));
        assert_eq!(
            parsed.headers.get("Authorization").unwrap(),
            "Bearer test_token"
        );
        assert!(parsed.body.is_empty());

        let target_req = b"GET /search HTTP/1.1\r\nHost: vamsec_jira_base_url\r\nX-Target-Url: vamsec_jira_base_url/rest/api/3/search\r\nAuthorization: vamsec_jira_auth_token\r\n\r\n";
        let parsed_target = parse_http_request(target_req).unwrap();
        assert_eq!(parsed_target.url, "vamsec_jira_base_url/rest/api/3/search");
        assert!(!parsed_target.headers.contains_key("X-Target-Url"));
        assert!(!parsed_target.headers.contains_key("Host"));
        assert_eq!(
            parsed_target.headers.get("Authorization").unwrap(),
            "vamsec_jira_auth_token"
        );
    }

    #[test]
    fn test_required_request_len_without_body() {
        let req = b"GET / HTTP/1.1\r\nHost: example.com\r\n\r\n";
        assert_eq!(required_request_len(req), Some(req.len()));
    }

    #[test]
    fn test_required_request_len_with_body() {
        let body = b"{\"fields\":{\"summary\":\"test\"}}";
        let header_str = format!(
            "POST /issue HTTP/1.1\r\nHost: vamsec_jira_base_url\r\nX-Target-Url: vamsec_jira_base_url/rest/api/3/issue\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        let header_bytes = header_str.into_bytes();
        let total_len = header_bytes.len() + body.len();
        let mut req = header_bytes.clone();
        req.extend_from_slice(body);

        assert_eq!(
            required_request_len(&req[..header_bytes.len() + 10]),
            Some(total_len)
        );
        let parsed = parse_http_request(&req).unwrap();
        assert_eq!(parsed.body, body);
        assert_eq!(parsed.method, "POST");
    }

    #[test]
    fn test_required_request_len_split_body() {
        let body = b"0123456789abcdef";
        let headers = format!(
            "POST / HTTP/1.1\r\nHost: example.com\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        let header_bytes = headers.into_bytes();
        let total_len = header_bytes.len() + body.len();

        assert_eq!(required_request_len(&header_bytes), Some(total_len));
        assert_eq!(
            required_request_len(&header_bytes[..header_bytes.len() - 2]),
            None
        );

        let mut partial_req = header_bytes.clone();
        partial_req.extend_from_slice(&body[..4]);
        assert_eq!(required_request_len(&partial_req), Some(total_len));
    }

    #[test]
    fn test_format_http_response() {
        let mut headers = HashMap::new();
        headers.insert("Content-Type".to_string(), "application/json".to_string());
        let resp = EgressResponse {
            status_code: 200,
            headers,
            body: b"{\"success\":true}".to_vec(),
            error: None,
        };
        let formatted = format_http_response(&resp);
        let formatted_str = std::str::from_utf8(&formatted).unwrap();
        assert!(formatted_str.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(formatted_str.contains("Content-Type: application/json\r\n"));
        assert!(formatted_str.contains("Content-Length: 16\r\n"));
        assert!(formatted_str.contains("Connection: close\r\n"));
        assert!(formatted_str.ends_with("\r\n\r\n{\"success\":true}"));
    }

    #[test]
    fn test_format_http_response_ignores_upstream_framing() {
        let mut headers = HashMap::new();
        headers.insert("CONTENT-LENGTH".to_string(), "999".to_string());
        headers.insert("Transfer-Encoding".to_string(), "chunked".to_string());
        headers.insert("connection".to_string(), "keep-alive".to_string());
        headers.insert("content-type".to_string(), "text/plain".to_string());
        let resp = EgressResponse {
            status_code: 200,
            headers,
            body: b"hello".to_vec(),
            error: None,
        };
        let formatted = format_http_response(&resp);
        let (head, body) = {
            let split = formatted.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
            (
                std::str::from_utf8(&formatted[..split])
                    .unwrap()
                    .to_string(),
                &formatted[split + 4..],
            )
        };
        let lower = head.to_ascii_lowercase();
        assert_eq!(lower.matches("content-length:").count(), 1, "{head}");
        assert_eq!(lower.matches("connection:").count(), 1, "{head}");
        assert!(!lower.contains("transfer-encoding"), "{head}");
        assert!(head.contains("Content-Length: 5"), "{head}");
        assert!(head.contains("Connection: close"), "{head}");
        assert!(head.contains("content-type: text/plain"), "{head}");
        assert_eq!(body, b"hello");
    }

    #[test]
    fn per_guest_endpoint_refuses_non_loopback_and_relative() {
        for bad in [
            "0.0.0.0:3128",
            "[::]:3128",
            "10.0.0.5:80",
            "localhost:80",
            "3128",
        ] {
            assert!(
                EgressEndpoint::per_guest("/tmp/x.sock", Some(bad)).is_err(),
                "{bad} must be refused"
            );
        }
        assert!(EgressEndpoint::per_guest("relative.sock", None).is_err());
        let ok = EgressEndpoint::per_guest("/tmp/x.sock", Some("127.0.0.1:0")).unwrap();
        assert!(ok.tcp.unwrap().ip().is_loopback());
        let v6 = EgressEndpoint::per_guest("/tmp/x.sock", Some("[::1]:0")).unwrap();
        assert!(v6.tcp.unwrap().ip().is_loopback());
        assert_eq!(
            EgressEndpoint::per_guest("/tmp/x.sock", None).unwrap().tcp,
            None
        );
    }

    #[cfg(not(feature = "process-guest"))]
    #[test]
    fn vm_default_endpoint_is_the_legacy_one() {
        let d = EgressEndpoint::vm_default();
        assert_eq!(d.tcp.unwrap().to_string(), "127.0.0.1:3128");
        assert_eq!(d.unix, std::path::PathBuf::from("/tmp/vam-egress.sock"));
        assert_eq!(proxy_url(), "http://127.0.0.1:3128");
    }

    #[tokio::test]
    async fn two_guests_get_distinct_loopback_endpoints() {
        let dir = tempfile::tempdir().unwrap();
        let mut bound = Vec::new();
        for n in 0..2 {
            let ep = EgressEndpoint::per_guest(
                dir.path()
                    .join(format!("egress-{n}.sock"))
                    .to_str()
                    .unwrap(),
                Some("127.0.0.1:0"),
            )
            .unwrap();
            let (a, _b) = tokio::io::duplex(1024);
            let got = start_guest_proxy_with(&ep, a).await.unwrap();
            assert!(got.tcp.unwrap().ip().is_loopback(), "never a wildcard bind");
            assert_ne!(got.tcp.unwrap().port(), 0);
            assert_eq!(got.unix.as_deref(), Some(ep.unix.as_path()));
            bound.push(got);
        }
        assert_ne!(bound[0].tcp, bound[1].tcp);
        assert_ne!(bound[0].unix, bound[1].unix);
    }

    #[tokio::test]
    async fn busy_tcp_port_does_not_fall_back_to_a_wildcard_listener() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let busy = blocker.local_addr().unwrap();
        let ep = EgressEndpoint::per_guest(
            dir.path().join("egress.sock").to_str().unwrap(),
            Some(&busy.to_string()),
        )
        .unwrap();
        let (a, _b) = tokio::io::duplex(1024);
        let got = start_guest_proxy_with(&ep, a).await.unwrap();
        assert_eq!(
            got.tcp, None,
            "tcp stays unbound; only the unix socket serves"
        );
        assert!(got.unix.is_some());
    }

    /// A microVM-style endpoint (free loopback port allowed) on a temp socket.
    fn vm_style_endpoint(dir: &std::path::Path, preferred: &str) -> EgressEndpoint {
        EgressEndpoint {
            tcp: Some(preferred.parse().unwrap()),
            unix: dir.join("vm-egress.sock"),
            ephemeral_fallback: true,
        }
    }

    /// A binder that records every address asked for and fails the ones `fail` names.
    fn recording_binder(
        asked: Arc<Mutex<Vec<std::net::SocketAddr>>>,
        fail: fn(&std::net::SocketAddr) -> Option<std::io::ErrorKind>,
    ) -> impl Fn(
        std::net::SocketAddr,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = std::io::Result<TcpListener>> + Send>,
    > {
        move |addr| {
            asked.lock().unwrap().push(addr);
            Box::pin(async move {
                match fail(&addr) {
                    Some(kind) => Err(std::io::Error::from(kind)),
                    None => TcpListener::bind(addr).await,
                }
            })
        }
    }

    #[tokio::test]
    async fn vm_endpoint_with_a_busy_loopback_port_binds_a_free_loopback_port() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let busy = blocker.local_addr().unwrap();
        let ep = vm_style_endpoint(dir.path(), &busy.to_string());
        let (a, _b) = tokio::io::duplex(1024);
        let got = start_guest_proxy_with(&ep, a).await.unwrap();
        let tcp = got.tcp.expect("a free loopback port is bound");
        assert!(tcp.ip().is_loopback(), "never a wildcard bind: {tcp}");
        assert_ne!(tcp.port(), busy.port());
        assert_ne!(tcp.port(), 0);
        assert_eq!(got.unix.as_deref(), Some(ep.unix.as_path()));
    }

    #[tokio::test]
    async fn vm_endpoint_whose_loopback_address_cannot_be_bound_never_asks_for_a_wildcard() {
        // EADDRNOTAVAIL is what the kernel says when `lo` is down or the address is
        // not on the host. Injected, so the test does not depend on the host.
        let dir = tempfile::tempdir().unwrap();
        let ep = vm_style_endpoint(dir.path(), "127.0.0.1:3128");
        let asked = Arc::new(Mutex::new(Vec::new()));
        let binder = recording_binder(asked.clone(), |a| {
            (a.port() == 3128).then_some(std::io::ErrorKind::AddrNotAvailable)
        });
        let (a, _b) = tokio::io::duplex(1024);
        let got = start_proxy_binding_with(&ep, a, binder).await.unwrap();
        let tcp = got.tcp.expect("the free loopback port is bound");
        assert!(tcp.ip().is_loopback() && tcp.port() != 3128, "{tcp}");
        let asked = asked.lock().unwrap();
        assert!(
            asked.iter().all(|a| a.ip().is_loopback()),
            "only loopback addresses were bound: {asked:?}"
        );
        assert_eq!(asked.len(), 2, "the preferred address, then a free port");
    }

    #[tokio::test]
    async fn vm_endpoint_with_no_loopback_at_all_serves_the_unix_socket_only() {
        let dir = tempfile::tempdir().unwrap();
        let ep = vm_style_endpoint(dir.path(), "127.0.0.1:3128");
        let asked = Arc::new(Mutex::new(Vec::new()));
        let binder = recording_binder(asked.clone(), |_| {
            Some(std::io::ErrorKind::AddrNotAvailable)
        });
        let (a, _b) = tokio::io::duplex(1024);
        let got = start_proxy_binding_with(&ep, a, binder).await.unwrap();
        assert_eq!(got.tcp, None);
        assert_eq!(got.unix.as_deref(), Some(ep.unix.as_path()));
        assert!(asked.lock().unwrap().iter().all(|a| a.ip().is_loopback()));
    }

    #[tokio::test]
    async fn a_non_loopback_preferred_address_is_never_bound() {
        let dir = tempfile::tempdir().unwrap();
        for preferred in ["0.0.0.0:3128", "[::]:3128", "10.0.0.5:3128"] {
            let asked = Arc::new(Mutex::new(Vec::new()));
            let ep = vm_style_endpoint(dir.path(), preferred);
            let (a, _b) = tokio::io::duplex(1024);
            let got = start_proxy_binding_with(&ep, a, recording_binder(asked.clone(), |_| None))
                .await
                .unwrap();
            assert!(
                got.tcp.unwrap().ip().is_loopback(),
                "{preferred} must not be bound"
            );
            assert!(
                asked.lock().unwrap().iter().all(|a| a.ip().is_loopback()),
                "{preferred}: {:?}",
                asked.lock().unwrap()
            );
            // With no fallback allowed, a non-loopback address leaves only the socket.
            let strict = EgressEndpoint {
                ephemeral_fallback: false,
                ..ep
            };
            let (a, _b) = tokio::io::duplex(1024);
            let got = start_guest_proxy_with(&strict, a).await.unwrap();
            assert_eq!(got.tcp, None, "{preferred}");
        }
    }

    #[cfg(not(feature = "process-guest"))]
    #[test]
    fn the_microvm_endpoint_allows_a_free_loopback_port_and_the_process_one_does_not() {
        assert!(EgressEndpoint::vm_default().ephemeral_fallback);
        assert!(
            !EgressEndpoint::per_guest("/tmp/x.sock", Some("127.0.0.1:0"))
                .unwrap()
                .ephemeral_fallback
        );
    }
}
