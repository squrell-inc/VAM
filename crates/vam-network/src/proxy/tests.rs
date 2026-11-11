//! Proxy tests run over in-memory duplexes and localhost TCP only.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio::net::TcpListener;

use super::serve_proxy;
use crate::gate::{EgressGate, GateDecision, GateError, GateMode, PreparedEgress};
use vam_spec::v1::{EgressRequest, EgressResponse};

const TEST_TIMEOUT: Duration = Duration::from_secs(10);

enum Route {
    Allow(SocketAddr),
    Deny(String),
    Resolution,
}

struct FakeGate {
    routes: Mutex<HashMap<String, Route>>,
    recorded: AtomicU64,
    quota: AtomicU64,
    checks: Mutex<Vec<(String, u16, String)>>,
}

impl FakeGate {
    fn new() -> Self {
        Self {
            routes: Mutex::new(HashMap::new()),
            recorded: AtomicU64::new(0),
            quota: AtomicU64::new(0),
            checks: Mutex::new(Vec::new()),
        }
    }

    fn allow(&self, host: &str, addr: SocketAddr) {
        self.routes
            .lock()
            .unwrap()
            .insert(host.to_string(), Route::Allow(addr));
    }

    fn deny(&self, host: &str, reason: &str) {
        self.routes
            .lock()
            .unwrap()
            .insert(host.to_string(), Route::Deny(reason.to_string()));
    }

    fn resolution_failure(&self, host: &str) {
        self.routes
            .lock()
            .unwrap()
            .insert(host.to_string(), Route::Resolution);
    }

    fn set_quota(&self, quota: u64) {
        self.quota.store(quota, Ordering::SeqCst);
    }

    fn recorded(&self) -> u64 {
        self.recorded.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl EgressGate for FakeGate {
    async fn check_destination(
        &self,
        host: &str,
        port: u16,
        scheme: &str,
    ) -> Result<GateDecision, GateError> {
        self.checks
            .lock()
            .unwrap()
            .push((host.to_string(), port, scheme.to_string()));
        let route = self
            .routes
            .lock()
            .unwrap()
            .get(host)
            .map(|route| match route {
                Route::Allow(addr) => Route::Allow(*addr),
                Route::Deny(reason) => Route::Deny(reason.clone()),
                Route::Resolution => Route::Resolution,
            });
        match route {
            Some(Route::Allow(addr)) => Ok(GateDecision {
                addr,
                mode: GateMode::Passthrough,
            }),
            Some(Route::Deny(reason)) => Err(GateError::Denied(reason)),
            Some(Route::Resolution) => Err(GateError::Resolution("unresolvable".to_string())),
            None => Err(GateError::Denied("host denied".to_string())),
        }
    }

    async fn prepare_request(&self, request: EgressRequest) -> Result<PreparedEgress, GateError> {
        Ok(PreparedEgress {
            request,
            secret_bound: false,
            ticket: None,
        })
    }

    async fn finish(&self, response: EgressResponse) -> Result<EgressResponse, GateError> {
        Ok(response)
    }

    fn record_bytes(&self, n: u64) -> Result<(), GateError> {
        let quota = self.quota.load(Ordering::SeqCst);
        let mut current = self.recorded.load(Ordering::SeqCst);
        loop {
            let next = current.saturating_add(n);
            if quota > 0 && next > quota {
                return Err(GateError::QuotaExceeded);
            }
            match self.recorded.compare_exchange_weak(
                current,
                next,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => return Ok(()),
                Err(actual) => current = actual,
            }
        }
    }
}

fn share(gate: &Arc<FakeGate>) -> Arc<dyn EgressGate> {
    Arc::clone(gate) as Arc<dyn EgressGate>
}

fn spawn_proxy(gate: Arc<dyn EgressGate>, buf: usize) -> DuplexStream {
    let (client, server) = tokio::io::duplex(buf);
    tokio::spawn(async move {
        if let Err(err) = serve_proxy(server, gate, None).await {
            panic!("serve_proxy failed: {err}");
        }
    });
    client
}

async fn spawn_echo() -> (SocketAddr, Arc<AtomicUsize>) {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let accepts = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&accepts);
    tokio::spawn(async move {
        while let Ok((sock, _)) = listener.accept().await {
            counter.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                let (mut reader, mut writer) = sock.into_split();
                let _ = tokio::io::copy(&mut reader, &mut writer).await;
            });
        }
    });
    (addr, accepts)
}

async fn spawn_http() -> SocketAddr {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut collected = Vec::new();
                let mut tmp = [0u8; 1024];
                loop {
                    let n = match sock.read(&mut tmp).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => n,
                    };
                    collected.extend_from_slice(&tmp[..n]);
                    if collected.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                let text = String::from_utf8_lossy(&collected);
                let path = text.split_whitespace().nth(1).unwrap_or("/");
                let body = path.as_bytes();
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = sock.write_all(response.as_bytes()).await;
                let _ = sock.write_all(body).await;
            });
        }
    });
    addr
}

async fn read_http_message<R>(io: &mut R) -> (u16, String, Vec<u8>)
where
    R: AsyncRead + Unpin,
{
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let n = tokio::time::timeout(TEST_TIMEOUT, io.read(&mut byte))
            .await
            .expect("timed out waiting for a proxy response")
            .expect("read proxy response");
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
    let mut body = Vec::new();
    if let Some(len) = content_length_of(&head) {
        body.resize(len, 0);
        if len > 0 {
            tokio::time::timeout(TEST_TIMEOUT, io.read_exact(&mut body))
                .await
                .expect("timed out waiting for a response body")
                .expect("read response body");
        }
    }
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

fn assert_proxy_agent(head: &str) {
    let found = head.split("\r\n").any(|line| {
        line.split_once(':').is_some_and(|(name, value)| {
            name.eq_ignore_ascii_case("proxy-agent") && value.trim() == "vam"
        })
    });
    assert!(found, "response missing Proxy-Agent: vam:\n{head}");
}

#[tokio::test]
async fn connect_tunnels_bytes_both_ways_via_pinned_addr() {
    let (addr, accepts) = spawn_echo().await;
    let gate = Arc::new(FakeGate::new());
    gate.allow("pinned.invalid", addr);
    let mut client = spawn_proxy(share(&gate), 64 * 1024);

    client
        .write_all(b"CONNECT pinned.invalid:80 HTTP/1.1\r\nHost: pinned.invalid:80\r\n\r\n")
        .await
        .unwrap();
    let (status, head, body) = read_http_message(&mut client).await;
    assert_eq!(status, 200, "{head}");
    assert!(head.starts_with("HTTP/1.1 200 Connection Established\r\n"));
    assert_proxy_agent(&head);
    assert!(body.is_empty());

    client.write_all(b"hello-from-guest").await.unwrap();
    let mut got = [0u8; 16];
    tokio::time::timeout(TEST_TIMEOUT, client.read_exact(&mut got))
        .await
        .expect("timed out waiting for echoed bytes")
        .unwrap();
    assert_eq!(&got, b"hello-from-guest");
    assert_eq!(accepts.load(Ordering::SeqCst), 1);
    assert_eq!(gate.recorded(), 32);
    let checks = gate.checks.lock().unwrap().clone();
    assert_eq!(
        checks,
        vec![("pinned.invalid".to_string(), 80, "connect".to_string())]
    );
}

#[tokio::test]
async fn connect_denied_host_is_403_and_dials_nothing() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let gate = Arc::new(FakeGate::new());
    gate.deny("127.0.0.1", "host denied");
    let mut client = spawn_proxy(share(&gate), 64 * 1024);

    let request = format!(
        "CONNECT 127.0.0.1:{} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n",
        addr.port(),
        addr.port()
    );
    client.write_all(request.as_bytes()).await.unwrap();
    let (status, head, body) = read_http_message(&mut client).await;
    assert_eq!(status, 403, "{head}");
    assert_proxy_agent(&head);
    assert_eq!(body, b"host denied");
    assert_eq!(gate.recorded(), 0);

    let accepted = tokio::time::timeout(Duration::from_millis(150), listener.accept()).await;
    assert!(accepted.is_err(), "denied CONNECT dialled the target");
}

#[tokio::test]
async fn connect_resolution_error_is_502() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let gate = Arc::new(FakeGate::new());
    gate.resolution_failure("127.0.0.1");
    let mut client = spawn_proxy(share(&gate), 64 * 1024);

    let request = format!(
        "CONNECT 127.0.0.1:{} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n",
        addr.port(),
        addr.port()
    );
    client.write_all(request.as_bytes()).await.unwrap();
    let (status, head, body) = read_http_message(&mut client).await;
    assert_eq!(status, 502, "{head}");
    assert_proxy_agent(&head);
    assert_eq!(body, b"name resolution failed");
    let accepted = tokio::time::timeout(Duration::from_millis(150), listener.accept()).await;
    assert!(accepted.is_err(), "resolution failure still dialled");
}

#[tokio::test]
async fn connect_quota_closes_tunnel_mid_stream() {
    let (addr, _) = spawn_echo().await;
    let gate = Arc::new(FakeGate::new());
    gate.allow("pinned.invalid", addr);
    let quota = 32 * 1024;
    gate.set_quota(quota);
    let mut client = spawn_proxy(share(&gate), 8 * 1024);

    client
        .write_all(b"CONNECT pinned.invalid:443 HTTP/1.1\r\nHost: pinned.invalid:443\r\n\r\n")
        .await
        .unwrap();
    let (status, head, _) = read_http_message(&mut client).await;
    assert_eq!(status, 200, "{head}");

    let payload = vec![0x5Au8; 256 * 1024];
    let (mut reader, mut writer) = tokio::io::split(client);
    let sending = payload.clone();
    let write = async move {
        let _ = writer.write_all(&sending).await;
        let _ = writer.shutdown().await;
    };
    let read = async move {
        let mut got = Vec::new();
        let mut tmp = [0u8; 8192];
        loop {
            match reader.read(&mut tmp).await {
                Ok(0) | Err(_) => break,
                Ok(n) => got.extend_from_slice(&tmp[..n]),
            }
        }
        got
    };
    let ((), got) = tokio::time::timeout(TEST_TIMEOUT, async { tokio::join!(write, read) })
        .await
        .expect("quota tunnel timed out");
    assert!(
        !got.is_empty(),
        "tunnel closed before any byte was delivered"
    );
    assert!(
        got.len() < payload.len(),
        "quota did not close the tunnel (got {} of {})",
        got.len(),
        payload.len()
    );
    let recorded = gate.recorded();
    assert!(recorded > 0);
    assert!(recorded <= quota, "recorded {recorded} above quota {quota}");
}

#[tokio::test]
async fn connect_transfers_five_megabytes_both_ways() {
    let (addr, accepts) = spawn_echo().await;
    let gate = Arc::new(FakeGate::new());
    gate.allow("pinned.invalid", addr);
    let mut client = spawn_proxy(share(&gate), 1024 * 1024);

    client
        .write_all(b"CONNECT pinned.invalid:80 HTTP/1.1\r\nHost: pinned.invalid:80\r\n\r\n")
        .await
        .unwrap();
    let (status, head, _) = read_http_message(&mut client).await;
    assert_eq!(status, 200, "{head}");

    let payload = vec![0xABu8; 5 * 1024 * 1024];
    let (mut reader, mut writer) = tokio::io::split(client);
    let sending = payload.clone();
    let write = async move {
        writer.write_all(&sending).await.unwrap();
        writer.shutdown().await.unwrap();
    };
    let read = async move {
        let mut got = Vec::new();
        reader.read_to_end(&mut got).await.unwrap();
        got
    };
    let ((), got) = tokio::time::timeout(TEST_TIMEOUT, async { tokio::join!(write, read) })
        .await
        .expect("5 MiB tunnel timed out");
    assert_eq!(got, payload);
    assert_eq!(accepts.load(Ordering::SeqCst), 1);
    assert_eq!(gate.recorded(), (payload.len() as u64) * 2);
}

#[tokio::test]
async fn plain_http_keep_alive_serves_two_requests() {
    let origin = spawn_http().await;
    let gate = Arc::new(FakeGate::new());
    gate.allow("allowed.test", origin);
    let mut client = spawn_proxy(share(&gate), 64 * 1024);

    let first = b"GET http://allowed.test/one HTTP/1.1\r\nHost: allowed.test\r\nConnection: keep-alive\r\n\r\n";
    client.write_all(first).await.unwrap();
    let (status, head, body) = read_http_message(&mut client).await;
    assert_eq!(status, 200, "{head}");
    assert_proxy_agent(&head);
    assert_eq!(body, b"/one");

    let second = b"GET http://allowed.test/two HTTP/1.1\r\nHost: allowed.test\r\nConnection: keep-alive\r\n\r\n";
    client.write_all(second).await.unwrap();
    let (status, head, body) = read_http_message(&mut client).await;
    assert_eq!(status, 200, "{head}");
    assert_eq!(body, b"/two");

    let checks = gate.checks.lock().unwrap().clone();
    assert_eq!(
        checks,
        vec![
            ("allowed.test".to_string(), 80, "http".to_string()),
            ("allowed.test".to_string(), 80, "http".to_string()),
        ]
    );
}

#[tokio::test]
async fn malformed_request_line_is_400() {
    let gate = Arc::new(FakeGate::new());
    let mut client = spawn_proxy(share(&gate), 64 * 1024);
    client.write_all(b"HELLO\r\n\r\n").await.unwrap();
    let (status, head, body) = read_http_message(&mut client).await;
    assert_eq!(status, 400, "{head}");
    assert_proxy_agent(&head);
    assert_eq!(body, b"malformed request line");
    assert!(gate.checks.lock().unwrap().is_empty());
}

#[tokio::test]
async fn overlong_header_is_431() {
    let gate = Arc::new(FakeGate::new());
    let mut client = spawn_proxy(share(&gate), 128 * 1024);
    let mut request = b"GET http://allowed.test/ HTTP/1.1\r\nX-Pad: ".to_vec();
    request.extend(std::iter::repeat_n(b'a', 20 * 1024));
    request.extend_from_slice(b"\r\n\r\n");
    client.write_all(&request).await.unwrap();
    let (status, head, body) = read_http_message(&mut client).await;
    assert_eq!(status, 431, "{head}");
    assert_proxy_agent(&head);
    assert_eq!(body, b"request header fields too large");
}

#[tokio::test]
async fn request_body_over_cap_is_413() {
    let gate = Arc::new(FakeGate::new());
    gate.allow("allowed.test", SocketAddr::from((Ipv4Addr::LOCALHOST, 9)));
    let mut client = spawn_proxy(share(&gate), 64 * 1024);
    let request = format!(
        "POST http://allowed.test/upload HTTP/1.1\r\nHost: allowed.test\r\nContent-Length: {}\r\n\r\n",
        super::MAX_REQUEST_BODY + 1
    );
    client.write_all(request.as_bytes()).await.unwrap();
    let (status, head, body) = read_http_message(&mut client).await;
    assert_eq!(status, 413, "{head}");
    assert_proxy_agent(&head);
    assert_eq!(body, b"request body too large");
}
