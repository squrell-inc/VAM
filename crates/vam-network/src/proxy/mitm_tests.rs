//! TLS termination tests: session CA toward the guest, upstream test CA toward the origin.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use http_body_util::BodyExt;
use hyper::body::{Body, Frame, Incoming};
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, IsCa, KeyPair, KeyUsagePurpose,
    PKCS_ECDSA_P256_SHA256,
};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName};
use rustls::RootCertStore;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio::net::TcpListener;
use tokio_rustls::{TlsAcceptor, TlsConnector};

use super::mitm::{BodyLimits, TerminateConfig};
use super::serve_proxy_with_upstream;
use crate::connection::{ConnectionRefusal, WriteTicket};
use crate::gate::{EgressGate, GateDecision, GateError, GateMode, PreparedEgress};
use crate::tls_ca::SessionCa;
use crate::upstream::UpstreamClient;
use std::collections::VecDeque;
use std::pin::Pin;
use std::task::{Context, Poll};
use vam_spec::v1::{EgressRequest, EgressResponse};

const HOST: &str = "api.example.test";
const PLACEHOLDER: &str = "vamsec_token";
const SECRET: &str = "super-secret-token-e8";
const READ_TIMEOUT: Duration = Duration::from_secs(15);

struct Seen {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

struct Origin {
    addr: SocketAddr,
    ca_pem: String,
    seen: Arc<Mutex<Vec<Seen>>>,
    tls_accepts: Arc<AtomicUsize>,
}

struct TermGate {
    addr: SocketAddr,
    /// Whether the destination holds a session secret binding.
    secret_bound: bool,
    refusals: Mutex<Vec<(String, u16, String)>>,
    prepares: AtomicUsize,
    finishes: AtomicUsize,
    recorded: AtomicU64,
    urls: Mutex<Vec<String>>,
    /// What the gate's journal ticket saw, in order (a write only).
    ticket_log: Arc<Mutex<Vec<String>>>,
    /// The ticket's `commit_send` refuses (the journal cannot be written).
    refuse_commit: Arc<AtomicBool>,
}

impl TermGate {
    fn new(addr: SocketAddr) -> Self {
        Self {
            addr,
            secret_bound: false,
            refusals: Mutex::new(Vec::new()),
            prepares: AtomicUsize::new(0),
            finishes: AtomicUsize::new(0),
            recorded: AtomicU64::new(0),
            urls: Mutex::new(Vec::new()),
            ticket_log: Arc::new(Mutex::new(Vec::new())),
            refuse_commit: Arc::new(AtomicBool::new(false)),
        }
    }
}

struct LogTicket {
    log: Arc<Mutex<Vec<String>>>,
    refuse: Arc<AtomicBool>,
}

impl WriteTicket for LogTicket {
    fn commit_send(&mut self) -> Result<(), ConnectionRefusal> {
        if self.refuse.load(Ordering::SeqCst) {
            self.log.lock().unwrap().push("commit-refused".into());
            return Err(ConnectionRefusal {
                text: "effect_journal_unavailable: not sent".into(),
                rule: "tool:t effect journal".into(),
            });
        }
        self.log.lock().unwrap().push("commit".into());
        Ok(())
    }

    fn answered(&mut self, status: u16) {
        self.log.lock().unwrap().push(format!("answered:{status}"));
    }
}

impl Drop for LogTicket {
    fn drop(&mut self) {
        self.log.lock().unwrap().push("dropped".into());
    }
}

#[async_trait]
impl EgressGate for TermGate {
    async fn check_destination(
        &self,
        _host: &str,
        _port: u16,
        _scheme: &str,
    ) -> Result<GateDecision, GateError> {
        Ok(GateDecision {
            addr: self.addr,
            mode: GateMode::Terminate,
        })
    }

    fn response_refused(&self, host: &str, port: u16, reason: &str) {
        self.refusals
            .lock()
            .unwrap()
            .push((host.to_string(), port, reason.to_string()));
    }

    async fn prepare_request(
        &self,
        mut request: EgressRequest,
    ) -> Result<PreparedEgress, GateError> {
        self.prepares.fetch_add(1, Ordering::SeqCst);
        self.urls.lock().unwrap().push(request.url.clone());
        for value in request.headers.values_mut() {
            *value = value.replace(PLACEHOLDER, SECRET);
        }
        if let Ok(text) = std::str::from_utf8(&request.body) {
            if text.contains(PLACEHOLDER) {
                request.body = text.replace(PLACEHOLDER, SECRET).into_bytes();
            }
        }
        // A write comes with a journal ticket (CN-4.7); a read does not.
        let ticket = (!request.method.eq_ignore_ascii_case("GET")).then(|| {
            Box::new(LogTicket {
                log: Arc::clone(&self.ticket_log),
                refuse: Arc::clone(&self.refuse_commit),
            }) as Box<dyn WriteTicket>
        });
        Ok(PreparedEgress {
            request,
            secret_bound: self.secret_bound,
            ticket,
        })
    }

    async fn finish(&self, mut response: EgressResponse) -> Result<EgressResponse, GateError> {
        self.finishes.fetch_add(1, Ordering::SeqCst);
        for value in response.headers.values_mut() {
            *value = value.replace(SECRET, PLACEHOLDER);
        }
        if let Ok(text) = std::str::from_utf8(&response.body) {
            if text.contains(SECRET) {
                response.body = text.replace(SECRET, PLACEHOLDER).into_bytes();
            }
        }
        Ok(response)
    }

    fn record_bytes(&self, n: u64) -> Result<(), GateError> {
        self.recorded.fetch_add(n, Ordering::SeqCst);
        Ok(())
    }
}

enum LeafKind {
    Valid { host: &'static str },
    Expired { host: &'static str },
    Mismatch,
}

fn mint_origin(kind: LeafKind) -> (String, TlsAcceptor) {
    let now = time::OffsetDateTime::now_utc();
    let ca_key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).expect("ca key");
    let mut ca_params = CertificateParams::default();
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, "upstream test CA");
    ca_params.distinguished_name = dn;
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    ca_params.not_before = now - time::Duration::days(30);
    ca_params.not_after = now + time::Duration::days(30);
    let ca_cert = ca_params.self_signed(&ca_key).expect("ca cert");

    let (leaf_host, not_before, not_after) = match kind {
        LeafKind::Valid { host } => (
            host,
            now - time::Duration::hours(1),
            now + time::Duration::days(7),
        ),
        LeafKind::Expired { host } => (
            host,
            now - time::Duration::days(10),
            now - time::Duration::days(1),
        ),
        LeafKind::Mismatch => (
            "wrong.example.test",
            now - time::Duration::hours(1),
            now + time::Duration::days(7),
        ),
    };
    let leaf_key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).expect("leaf key");
    let mut leaf_params = CertificateParams::new(vec![leaf_host.to_string()]).expect("leaf params");
    leaf_params.is_ca = IsCa::ExplicitNoCa;
    leaf_params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    leaf_params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
    leaf_params.not_before = not_before;
    leaf_params.not_after = not_after;
    let leaf = leaf_params
        .signed_by(&leaf_key, &ca_cert, &ca_key)
        .expect("leaf cert");

    let cert = CertificateDer::from(leaf.der().to_vec());
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(leaf_key.serialize_der()));
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let mut server = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("versions")
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)
        .expect("origin cert");
    server.alpn_protocols = vec![b"http/1.1".to_vec()];
    (ca_cert.pem(), TlsAcceptor::from(Arc::new(server)))
}

async fn spawn_origin(kind: LeafKind) -> Origin {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (ca_pem, acceptor) = mint_origin(kind);
    let seen = Arc::new(Mutex::new(Vec::new()));
    let tls_accepts = Arc::new(AtomicUsize::new(0));
    let seen_task = Arc::clone(&seen);
    let accepts_task = Arc::clone(&tls_accepts);
    tokio::spawn(async move {
        loop {
            let Ok((sock, _)) = listener.accept().await else {
                break;
            };
            accepts_task.fetch_add(1, Ordering::SeqCst);
            let acceptor = acceptor.clone();
            let seen = Arc::clone(&seen_task);
            tokio::spawn(async move {
                let Ok(tls) = acceptor.accept(sock).await else {
                    return;
                };
                let io = TokioIo::new(tls);
                let service = service_fn(move |request| {
                    let seen = Arc::clone(&seen);
                    async move {
                        Ok::<_, std::convert::Infallible>(origin_response(request, seen).await)
                    }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .keep_alive(true)
                    .serve_connection(io, service)
                    .await;
            });
        }
    });
    Origin {
        addr,
        ca_pem,
        seen,
        tls_accepts,
    }
}

async fn origin_response(
    request: Request<Incoming>,
    seen: Arc<Mutex<Vec<Seen>>>,
) -> Response<OriginBody> {
    let method = request.method().to_string();
    let path = request.uri().path().to_string();
    let headers: Vec<(String, String)> = request
        .headers()
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_string(),
                String::from_utf8_lossy(value.as_bytes()).into_owned(),
            )
        })
        .collect();
    let body = request
        .into_body()
        .collect()
        .await
        .map(|collected| collected.to_bytes())
        .unwrap_or_default();
    seen.lock().unwrap().push(Seen {
        method: method.clone(),
        path: path.clone(),
        headers: headers.clone(),
        body: body.to_vec(),
    });

    if method.eq_ignore_ascii_case("HEAD") {
        // `/head-big` declares a length over the scrub buffer, as a HEAD may.
        let declared = if path == "/head-big" { "5000" } else { "0" };
        return Response::builder()
            .status(StatusCode::OK)
            .header(hyper::header::CONTENT_LENGTH, declared)
            .body(OriginBody::whole(Bytes::new()))
            .unwrap();
    }

    // `/sized/N` and `/chunked/N`: N bytes of filler with the secret as the last bytes, and the
    // secret in a header. `/chunked/N` has no Content-Length.
    let sized = path
        .strip_prefix("/sized/")
        .map(|n| (n, false))
        .or_else(|| path.strip_prefix("/chunked/").map(|n| (n, true)));
    if let Some((n, chunked)) = sized {
        let n: usize = n.parse().unwrap();
        let mut payload = vec![b'a'; n - SECRET.len()];
        payload.extend_from_slice(SECRET.as_bytes());
        let body = if chunked {
            OriginBody::chunked(payload)
        } else {
            OriginBody::whole(Bytes::from(payload))
        };
        return Response::builder()
            .status(StatusCode::OK)
            .header("x-leak", SECRET)
            .body(body)
            .unwrap();
    }

    let (status, payload, location) = match path.as_str() {
        "/big" => (
            StatusCode::OK,
            Bytes::from(vec![0xABu8; 5 * 1024 * 1024]),
            None,
        ),
        "/stream" => (StatusCode::OK, Bytes::from(vec![0x11u8; 4096]), None),
        "/echo-auth" => {
            let auth = header_all(&headers, "authorization");
            (StatusCode::OK, Bytes::from(auth), None)
        }
        "/redirect" => (
            StatusCode::FOUND,
            Bytes::from_static(b"stay"),
            Some("https://evil.example/nope"),
        ),
        "/patch" => (StatusCode::OK, Bytes::from_static(b"ok-patch"), None),
        _ => (StatusCode::OK, Bytes::from_static(b"upstream-ok"), None),
    };
    let mut builder = Response::builder().status(status);
    if let Some(location) = location {
        builder = builder.header(hyper::header::LOCATION, location);
    }
    builder.body(OriginBody::whole(payload)).unwrap()
}

/// A response body from the test origin: one known-size chunk, or several with no size.
struct OriginBody {
    frames: VecDeque<Bytes>,
    exact: Option<u64>,
}

impl OriginBody {
    fn whole(bytes: Bytes) -> Self {
        let exact = Some(bytes.len() as u64);
        Self {
            frames: VecDeque::from([bytes]),
            exact,
        }
    }

    fn chunked(payload: Vec<u8>) -> Self {
        let frames = payload.chunks(700).map(Bytes::copy_from_slice).collect();
        Self {
            frames,
            exact: None,
        }
    }
}

impl Body for OriginBody {
    type Data = Bytes;
    type Error = std::convert::Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        match self.frames.pop_front() {
            Some(bytes) if !bytes.is_empty() => Poll::Ready(Some(Ok(Frame::data(bytes)))),
            _ => Poll::Ready(None),
        }
    }

    fn size_hint(&self) -> hyper::body::SizeHint {
        match self.exact {
            Some(len) => hyper::body::SizeHint::with_exact(len),
            None => hyper::body::SizeHint::default(),
        }
    }
}

fn header_all(headers: &[(String, String)], name: &str) -> String {
    headers
        .iter()
        .filter(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
        .collect::<Vec<_>>()
        .join(",")
}

fn trust_session(pem: &str) -> rustls::ClientConfig {
    let mut roots = RootCertStore::empty();
    let cert = CertificateDer::from_pem_slice(pem.as_bytes()).expect("session ca pem");
    roots.add(cert).expect("session ca root");
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let mut config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    config
}

struct Tunnel {
    tls: tokio_rustls::client::TlsStream<DuplexStream>,
    gate: Arc<TermGate>,
    origin: Origin,
}

async fn open_tunnel(kind: LeafKind, extra_root: bool, sni: &str, limits: BodyLimits) -> Tunnel {
    open_tunnel_with(kind, extra_root, sni, limits, false).await
}

/// `secret_bound`: the gate reports that the destination holds a session secret.
async fn open_tunnel_with(
    kind: LeafKind,
    extra_root: bool,
    sni: &str,
    limits: BodyLimits,
    secret_bound: bool,
) -> Tunnel {
    let origin = spawn_origin(kind).await;
    let upstream = if extra_root {
        UpstreamClient::new(Some(&origin.ca_pem)).expect("extra root")
    } else {
        UpstreamClient::new(None).expect("default roots")
    };
    let session = SessionCa::mint("e8-1-4").expect("session ca");
    let session_pem = session.ca_cert_pem();
    let mut gate = TermGate::new(origin.addr);
    gate.secret_bound = secret_bound;
    let gate = Arc::new(gate);
    let (client, server) = tokio::io::duplex(1024 * 1024);
    let gate_task = Arc::clone(&gate) as Arc<dyn EgressGate>;
    let ca = Arc::new(session);
    let config = TerminateConfig {
        offer_h2: false,
        limits,
    };
    tokio::spawn(async move {
        if let Err(err) =
            serve_proxy_with_upstream(server, gate_task, Some(ca), Arc::new(upstream), config).await
        {
            panic!("serve_proxy failed: {err}");
        }
    });

    let mut client = client;
    let connect = format!("CONNECT {HOST}:443 HTTP/1.1\r\nHost: {HOST}:443\r\n\r\n");
    client.write_all(connect.as_bytes()).await.unwrap();
    let (status, head, _) = read_http(&mut client, READ_TIMEOUT).await;
    assert_eq!(status, 200, "{head}");

    let connector = TlsConnector::from(Arc::new(trust_session(&session_pem)));
    let name = ServerName::try_from(sni).expect("sni").to_owned();
    let tls = tokio::time::timeout(READ_TIMEOUT, connector.connect(name, client))
        .await
        .expect("tls handshake timed out")
        .expect("tls to the proxy");
    Tunnel { tls, gate, origin }
}

async fn exchange(tunnel: &mut Tunnel, request: &str) -> (u16, String, Vec<u8>) {
    tunnel.tls.write_all(request.as_bytes()).await.unwrap();
    tunnel.tls.flush().await.unwrap();
    read_http(&mut tunnel.tls, READ_TIMEOUT).await
}

async fn read_http<R>(io: &mut R, timeout: Duration) -> (u16, String, Vec<u8>)
where
    R: AsyncRead + Unpin,
{
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let n = tokio::time::timeout(timeout, io.read(&mut byte))
            .await
            .expect("timed out waiting for a response")
            .expect("read response");
        assert_eq!(n, 1, "eof before response headers");
        buf.push(byte[0]);
        if buf.ends_with(b"\r\n\r\n") {
            break;
        }
        assert!(buf.len() < 64 * 1024, "response head grew without an end");
    }
    let head = String::from_utf8(buf).expect("response head is utf-8");
    let status = head
        .split_whitespace()
        .nth(1)
        .unwrap_or("0")
        .parse::<u16>()
        .expect("status code");
    let body = if let Some(len) = content_length_of(&head) {
        let mut body = vec![0u8; len];
        if len > 0 {
            tokio::time::timeout(timeout, io.read_exact(&mut body))
                .await
                .expect("timed out waiting for a response body")
                .expect("read response body");
        }
        body
    } else if is_chunked(&head) {
        read_chunked(io, timeout).await
    } else {
        Vec::new()
    };
    (status, head, body)
}

fn content_length_of(head: &str) -> Option<usize> {
    for line in head.split("\r\n") {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.eq_ignore_ascii_case("content-length") {
            return value.trim().parse().ok();
        }
    }
    None
}

fn is_chunked(head: &str) -> bool {
    head.split("\r\n").any(|line| {
        line.split_once(':').is_some_and(|(name, value)| {
            name.eq_ignore_ascii_case("transfer-encoding")
                && value
                    .split(',')
                    .any(|token| token.trim().eq_ignore_ascii_case("chunked"))
        })
    })
}

async fn read_chunked<R>(io: &mut R, timeout: Duration) -> Vec<u8>
where
    R: AsyncRead + Unpin,
{
    let mut body = Vec::new();
    loop {
        let mut line = Vec::new();
        loop {
            let mut byte = [0u8; 1];
            tokio::time::timeout(timeout, io.read_exact(&mut byte))
                .await
                .expect("chunk size")
                .unwrap();
            line.push(byte[0]);
            if line.ends_with(b"\r\n") {
                break;
            }
        }
        let text = std::str::from_utf8(&line).unwrap().trim();
        let size = usize::from_str_radix(text.split(';').next().unwrap_or(text), 16).unwrap();
        if size == 0 {
            let mut crlf = [0u8; 2];
            io.read_exact(&mut crlf).await.unwrap();
            break;
        }
        let mut chunk = vec![0u8; size];
        tokio::time::timeout(timeout, io.read_exact(&mut chunk))
            .await
            .expect("chunk")
            .unwrap();
        body.extend_from_slice(&chunk);
        let mut crlf = [0u8; 2];
        io.read_exact(&mut crlf).await.unwrap();
    }
    body
}

fn default_limits() -> BodyLimits {
    BodyLimits::default()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn connect_then_tls_get_returns_the_upstream_body() {
    let mut tunnel =
        open_tunnel(LeafKind::Valid { host: HOST }, true, HOST, default_limits()).await;
    let (status, head, body) = exchange(
        &mut tunnel,
        &format!("GET /hello HTTP/1.1\r\nHost: {HOST}\r\nConnection: close\r\n\r\n"),
    )
    .await;
    assert_eq!(status, 200, "{head}");
    assert_eq!(body, b"upstream-ok");
    let urls = tunnel.gate.urls.lock().unwrap().clone();
    assert_eq!(urls, vec![format!("https://{HOST}:443/hello")]);
    assert!(tunnel.gate.finishes.load(Ordering::SeqCst) >= 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn prepare_substitutes_authorization_and_finish_scrubs_the_echo() {
    let mut tunnel =
        open_tunnel(LeafKind::Valid { host: HOST }, true, HOST, default_limits()).await;
    let request = format!(
        "GET /echo-auth HTTP/1.1\r\nHost: {HOST}\r\nAuthorization: Bearer {PLACEHOLDER}\r\nConnection: close\r\n\r\n"
    );
    let (status, head, body) = exchange(&mut tunnel, &request).await;
    assert_eq!(status, 200, "{head}");
    let body = String::from_utf8(body).expect("utf-8 body");
    assert!(
        !body.contains(SECRET),
        "guest saw the injected secret: {body}"
    );
    assert!(
        body.contains(PLACEHOLDER),
        "scrubbed body should keep the placeholder: {body}"
    );
    let seen = tunnel.origin.seen.lock().unwrap();
    let auth = header_all(&seen[0].headers, "authorization");
    assert!(
        auth.contains(SECRET),
        "upstream did not see the substituted secret: {auth}"
    );
    assert!(
        !auth.contains(PLACEHOLDER),
        "placeholder reached the origin: {auth}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unknown_issuer_is_502() {
    let mut tunnel = open_tunnel(
        LeafKind::Valid { host: HOST },
        false,
        HOST,
        default_limits(),
    )
    .await;
    let (status, head, body) = exchange(
        &mut tunnel,
        &format!("GET / HTTP/1.1\r\nHost: {HOST}\r\nConnection: close\r\n\r\n"),
    )
    .await;
    assert_eq!(status, 502, "{head}");
    assert_eq!(body, b"unknown issuer");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hostname_mismatch_is_502() {
    let mut tunnel = open_tunnel(LeafKind::Mismatch, true, HOST, default_limits()).await;
    let (status, head, body) = exchange(
        &mut tunnel,
        &format!("GET / HTTP/1.1\r\nHost: {HOST}\r\nConnection: close\r\n\r\n"),
    )
    .await;
    assert_eq!(status, 502, "{head}");
    assert_eq!(body, b"hostname mismatch");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn expired_upstream_certificate_is_502() {
    let mut tunnel = open_tunnel(
        LeafKind::Expired { host: HOST },
        true,
        HOST,
        default_limits(),
    )
    .await;
    let (status, head, body) = exchange(
        &mut tunnel,
        &format!("GET / HTTP/1.1\r\nHost: {HOST}\r\nConnection: close\r\n\r\n"),
    )
    .await;
    assert_eq!(status, 502, "{head}");
    assert_eq!(body, b"certificate expired");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn host_header_mismatch_is_421_and_does_not_dial() {
    let mut tunnel =
        open_tunnel(LeafKind::Valid { host: HOST }, true, HOST, default_limits()).await;
    let (status, head, body) = exchange(
        &mut tunnel,
        "GET /secret HTTP/1.1\r\nHost: evil.example.test\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert_eq!(status, 421, "{head}");
    assert_eq!(body, b"host does not match connect target");
    assert_eq!(tunnel.gate.prepares.load(Ordering::SeqCst), 0);
    assert_eq!(tunnel.origin.tls_accepts.load(Ordering::SeqCst), 0);
    assert!(tunnel.origin.seen.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sni_mismatch_is_421() {
    let mut tunnel = open_tunnel(
        LeafKind::Valid { host: HOST },
        true,
        "evil.example.test",
        default_limits(),
    )
    .await;
    let (status, head, _) = exchange(
        &mut tunnel,
        &format!("GET / HTTP/1.1\r\nHost: {HOST}\r\nConnection: close\r\n\r\n"),
    )
    .await;
    assert_eq!(status, 421, "{head}");
    assert_eq!(tunnel.origin.tls_accepts.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn five_megabyte_response_is_delivered() {
    let mut tunnel =
        open_tunnel(LeafKind::Valid { host: HOST }, true, HOST, default_limits()).await;
    let (status, head, body) = exchange(
        &mut tunnel,
        &format!("GET /big HTTP/1.1\r\nHost: {HOST}\r\nConnection: close\r\n\r\n"),
    )
    .await;
    assert_eq!(status, 200, "{head}");
    assert_eq!(body.len(), 5 * 1024 * 1024);
    assert!(body.iter().all(|byte| *byte == 0xAB));
    assert!(tunnel.gate.finishes.load(Ordering::SeqCst) >= 1);
    assert!(tunnel.gate.recorded.load(Ordering::SeqCst) >= body.len() as u64);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn response_over_the_scrub_buffer_streams_unscrubbed() {
    let limits = BodyLimits {
        scrub_buffer: 1024,
        ..BodyLimits::default()
    };
    let mut tunnel = open_tunnel(LeafKind::Valid { host: HOST }, true, HOST, limits).await;
    let (status, head, body) = exchange(
        &mut tunnel,
        &format!("GET /stream HTTP/1.1\r\nHost: {HOST}\r\nConnection: close\r\n\r\n"),
    )
    .await;
    assert_eq!(status, 200, "{head}");
    assert_eq!(body, vec![0x11u8; 4096]);
    assert_eq!(
        tunnel.gate.finishes.load(Ordering::SeqCst),
        0,
        "a body over the scrub buffer must stream without finish"
    );
    assert!(tunnel.gate.recorded.load(Ordering::SeqCst) >= 4096);
}

fn small_buffer() -> BodyLimits {
    BodyLimits {
        scrub_buffer: 1024,
        ..BodyLimits::default()
    }
}

async fn get(tunnel: &mut Tunnel, path: &str, extra: &str) -> (u16, String, Vec<u8>) {
    exchange(
        tunnel,
        &format!("GET {path} HTTP/1.1\r\nHost: {HOST}\r\n{extra}Connection: close\r\n\r\n"),
    )
    .await
}

fn assert_refused(tunnel: &Tunnel, status: u16, head: &str, body: &[u8]) {
    assert_eq!(status, 502, "{head}");
    assert!(
        !head.contains(SECRET) && !head.contains("x-leak"),
        "an upstream header reached the guest: {head}"
    );
    let text = String::from_utf8_lossy(body);
    assert!(text.contains("too large to scrub"), "{text}");
    assert!(!text.contains(SECRET), "guest saw the secret: {text}");
    assert_eq!(
        tunnel.gate.finishes.load(Ordering::SeqCst),
        0,
        "a refused body is never read out for finish"
    );
    assert_eq!(
        *tunnel.gate.refusals.lock().unwrap(),
        vec![(HOST.to_string(), 443, "oversize_with_secret".to_string())]
    );
}

const AUTH: &str = "Authorization: Bearer vamsec_token\r\n";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn oversize_from_a_secret_bound_host_is_refused_by_content_length() {
    let mut tunnel = open_tunnel_with(
        LeafKind::Valid { host: HOST },
        true,
        HOST,
        small_buffer(),
        true,
    )
    .await;
    let (status, head, body) = get(&mut tunnel, "/sized/4096", AUTH).await;
    assert_refused(&tunnel, status, &head, &body);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn oversize_chunked_from_a_secret_bound_host_is_refused_before_any_byte_is_sent() {
    let mut tunnel = open_tunnel_with(
        LeafKind::Valid { host: HOST },
        true,
        HOST,
        small_buffer(),
        true,
    )
    .await;
    let (status, head, body) = get(&mut tunnel, "/chunked/4096", AUTH).await;
    assert_refused(&tunnel, status, &head, &body);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn oversize_without_a_placeholder_is_refused_when_the_host_holds_a_secret() {
    // The request carries no placeholder; the origin may still echo a secret it was sent earlier.
    for path in ["/sized/4096", "/chunked/4096"] {
        let mut tunnel = open_tunnel_with(
            LeafKind::Valid { host: HOST },
            true,
            HOST,
            small_buffer(),
            true,
        )
        .await;
        let (status, head, body) = get(&mut tunnel, path, "").await;
        assert_refused(&tunnel, status, &head, &body);
        assert_eq!(tunnel.gate.prepares.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn oversize_from_a_host_without_a_binding_still_streams() {
    for path in ["/sized/4096", "/chunked/4096"] {
        let mut tunnel = open_tunnel_with(
            LeafKind::Valid { host: HOST },
            true,
            HOST,
            small_buffer(),
            false,
        )
        .await;
        let (status, head, body) = get(&mut tunnel, path, AUTH).await;
        assert_eq!(status, 200, "{head}");
        assert_eq!(body.len(), 4096, "{path}");
        assert_eq!(tunnel.gate.finishes.load(Ordering::SeqCst), 0);
        assert!(tunnel.gate.refusals.lock().unwrap().is_empty());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_body_exactly_at_the_scrub_buffer_is_scrubbed_and_one_byte_over_is_refused() {
    let mut tunnel = open_tunnel_with(
        LeafKind::Valid { host: HOST },
        true,
        HOST,
        small_buffer(),
        true,
    )
    .await;
    let (status, head, body) = get(&mut tunnel, "/sized/1024", AUTH).await;
    assert_eq!(status, 200, "{head}");
    assert_eq!(body.len(), 1024 - SECRET.len() + PLACEHOLDER.len());
    assert!(!String::from_utf8_lossy(&body).contains(SECRET));
    assert!(String::from_utf8_lossy(&body).ends_with(PLACEHOLDER));
    assert!(!head.contains(SECRET), "{head}");
    assert_eq!(tunnel.gate.finishes.load(Ordering::SeqCst), 1);
    assert!(tunnel.gate.refusals.lock().unwrap().is_empty());

    let mut over = open_tunnel_with(
        LeafKind::Valid { host: HOST },
        true,
        HOST,
        small_buffer(),
        true,
    )
    .await;
    let (status, head, body) = get(&mut over, "/sized/1025", AUTH).await;
    assert_refused(&over, status, &head, &body);
    let mut chunked = open_tunnel_with(
        LeafKind::Valid { host: HOST },
        true,
        HOST,
        small_buffer(),
        true,
    )
    .await;
    let (status, head, body) = get(&mut chunked, "/chunked/1024", AUTH).await;
    assert_eq!(status, 200, "{head}");
    assert!(!String::from_utf8_lossy(&body).contains(SECRET));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn head_with_a_declared_length_over_the_buffer_is_not_refused() {
    // A HEAD response has no body to leak, and `finish` still scrubs its headers.
    let mut tunnel = open_tunnel_with(
        LeafKind::Valid { host: HOST },
        true,
        HOST,
        small_buffer(),
        true,
    )
    .await;
    let (status, head, body) = exchange(
        &mut tunnel,
        &format!("HEAD /head-big HTTP/1.1\r\nHost: {HOST}\r\nConnection: close\r\n\r\n"),
    )
    .await;
    assert_eq!(status, 200, "{head}");
    assert!(body.is_empty());
    assert_eq!(tunnel.gate.finishes.load(Ordering::SeqCst), 1);
    assert!(tunnel.gate.refusals.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn patch_and_head_are_forwarded_unchanged() {
    let mut tunnel =
        open_tunnel(LeafKind::Valid { host: HOST }, true, HOST, default_limits()).await;
    let patch = format!(
        "PATCH /patch HTTP/1.1\r\nHost: {HOST}\r\nContent-Length: 7\r\nConnection: keep-alive\r\n\r\nchanged"
    );
    let (status, head, body) = exchange(&mut tunnel, &patch).await;
    assert_eq!(status, 200, "{head}");
    assert_eq!(body, b"ok-patch");

    let head_req = format!("HEAD /head HTTP/1.1\r\nHost: {HOST}\r\nConnection: close\r\n\r\n");
    let (status, head, body) = exchange(&mut tunnel, &head_req).await;
    assert_eq!(status, 200, "{head}");
    assert!(body.is_empty());

    let seen = tunnel.origin.seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].method, "PATCH");
    assert_eq!(seen[0].path, "/patch");
    assert_eq!(seen[0].body, b"changed");
    assert_eq!(seen[1].method, "HEAD");
    assert_eq!(seen[1].path, "/head");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn redirects_are_not_followed() {
    let mut tunnel =
        open_tunnel(LeafKind::Valid { host: HOST }, true, HOST, default_limits()).await;
    let (status, head, body) = exchange(
        &mut tunnel,
        &format!("GET /redirect HTTP/1.1\r\nHost: {HOST}\r\nConnection: close\r\n\r\n"),
    )
    .await;
    assert_eq!(status, 302, "{head}");
    assert_eq!(body, b"stay");
    assert!(head
        .to_ascii_lowercase()
        .contains("location: https://evil.example/nope"));
    assert_eq!(tunnel.origin.seen.lock().unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn repeated_headers_reach_the_origin() {
    let mut tunnel =
        open_tunnel(LeafKind::Valid { host: HOST }, true, HOST, default_limits()).await;
    let request = format!(
        "GET /hdr HTTP/1.1\r\nHost: {HOST}\r\nX-Trace: one\r\nX-Trace: two\r\nConnection: close\r\n\r\n"
    );
    let (status, head, _) = exchange(&mut tunnel, &request).await;
    assert_eq!(status, 200, "{head}");
    let seen = tunnel.origin.seen.lock().unwrap();
    let traces: Vec<_> = seen[0]
        .headers
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("x-trace"))
        .map(|(_, value)| value.as_str())
        .collect();
    assert_eq!(traces, ["one", "two"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn keep_alive_reuses_the_pinned_upstream_connection() {
    let mut tunnel =
        open_tunnel(LeafKind::Valid { host: HOST }, true, HOST, default_limits()).await;
    let first = format!("GET /one HTTP/1.1\r\nHost: {HOST}\r\nConnection: keep-alive\r\n\r\n");
    let (status, head, body) = exchange(&mut tunnel, &first).await;
    assert_eq!(status, 200, "{head}");
    assert_eq!(body, b"upstream-ok");
    let second = format!("GET /two HTTP/1.1\r\nHost: {HOST}\r\nConnection: close\r\n\r\n");
    let (status, head, body) = exchange(&mut tunnel, &second).await;
    assert_eq!(status, 200, "{head}");
    assert_eq!(body, b"upstream-ok");
    assert_eq!(
        tunnel.origin.tls_accepts.load(Ordering::SeqCst),
        1,
        "second request opened another upstream TLS connection"
    );
    let seen = tunnel.origin.seen.lock().unwrap();
    assert_eq!(seen[0].path, "/one");
    assert_eq!(seen[1].path, "/two");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_journaled_write_is_committed_before_it_is_sent_and_answered_with_the_status() {
    let mut tunnel =
        open_tunnel(LeafKind::Valid { host: HOST }, true, HOST, default_limits()).await;
    let (status, head, _body) = exchange(
        &mut tunnel,
        &format!(
            "POST /write HTTP/1.1\r\nHost: {HOST}\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}"
        ),
    )
    .await;
    assert_eq!(status, 200, "{head}");
    assert_eq!(tunnel.origin.seen.lock().unwrap().len(), 1);
    assert_eq!(
        *tunnel.gate.ticket_log.lock().unwrap(),
        vec!["commit", "answered:200", "dropped"]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_write_whose_commit_fails_never_reaches_the_origin() {
    let mut tunnel =
        open_tunnel(LeafKind::Valid { host: HOST }, true, HOST, default_limits()).await;
    tunnel.gate.refuse_commit.store(true, Ordering::SeqCst);
    let (status, head, body) = exchange(
        &mut tunnel,
        &format!(
            "POST /write HTTP/1.1\r\nHost: {HOST}\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}"
        ),
    )
    .await;
    assert_eq!(status, 403, "{head}");
    assert!(String::from_utf8_lossy(&body).contains("effect_journal_unavailable"));
    assert!(tunnel.origin.seen.lock().unwrap().is_empty());
    assert_eq!(
        *tunnel.gate.ticket_log.lock().unwrap(),
        vec!["commit-refused", "dropped"]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_read_carries_no_ticket() {
    let mut tunnel =
        open_tunnel(LeafKind::Valid { host: HOST }, true, HOST, default_limits()).await;
    let (status, head, _body) = exchange(
        &mut tunnel,
        &format!("GET /hello HTTP/1.1\r\nHost: {HOST}\r\nConnection: close\r\n\r\n"),
    )
    .await;
    assert_eq!(status, 200, "{head}");
    assert!(tunnel.gate.ticket_log.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_journaled_write_to_a_secret_bound_host_keeps_both_rules() {
    // Journal (CN-4.7) and oversize rule together: the write is committed before it is sent and
    // answered with the origin's status, and a response too large to scrub is still refused.
    let mut tunnel = open_tunnel_with(
        LeafKind::Valid { host: HOST },
        true,
        HOST,
        small_buffer(),
        true,
    )
    .await;
    let (status, head, body) = exchange(
        &mut tunnel,
        &format!(
            "POST /sized/4096 HTTP/1.1\r\nHost: {HOST}\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}"
        ),
    )
    .await;
    assert_refused(&tunnel, status, &head, &body);
    assert_eq!(
        *tunnel.gate.ticket_log.lock().unwrap(),
        vec!["commit", "answered:200", "dropped"]
    );
}
