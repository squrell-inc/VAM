//! Guest-facing HTTP proxy front-end (E8-P1).
//!
//! One byte stream speaks HTTP/1.1 forward-proxy: `CONNECT host:port` and
//! absolute-form requests such as `GET http://host/path HTTP/1.1`. The gate
//! decides whether a `CONNECT` is passthrough or TLS termination.

mod connect;
#[cfg(feature = "process-guest")]
pub mod fixture;
mod mitm;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use vam_spec::v1::{EgressRequest, EgressResponse};

use crate::error::VamNetworkError;
use crate::gate::{EgressGate, GateDecision, GateError, GateMode};
use crate::tls_ca::SessionCa;
use crate::upstream::UpstreamClient;

use connect::PassthroughError;

const PROXY_AGENT: &str = "vam";
/// Request head limit, including the request line. Larger heads get `431`.
const MAX_HEADER_BYTES: usize = 16 * 1024;
/// Overall deadline for reading one request head.
const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(30);
/// Idle gap allowed while reading a body.
const BODY_READ_TIMEOUT: Duration = Duration::from_secs(30);
/// Above the legacy 64 KiB broker cap. Larger request bodies get `413`.
const MAX_REQUEST_BODY: usize = 32 * 1024 * 1024;
/// Larger upstream responses get `502` rather than being buffered whole.
const MAX_RESPONSE_BODY: usize = 32 * 1024 * 1024;

/// Serve the egress proxy protocol on `stream` until the guest disconnects.
///
/// Plain HTTP requests on one connection are kept alive. `CONNECT` takes over
/// the rest of the stream. `ca` is only used when the gate selects TLS
/// termination.
pub async fn serve_proxy<S>(
    stream: S,
    gate: Arc<dyn EgressGate>,
    ca: Option<Arc<SessionCa>>,
) -> Result<(), VamNetworkError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    serve_proxy_inner(stream, gate, ca, None, mitm::TerminateConfig::default()).await
}

/// [`serve_proxy`] with a caller-supplied origin client and terminator limits.
///
/// Tests pass an [`UpstreamClient`] that trusts an extra CA, and a
/// [`mitm::TerminateConfig`] with a smaller scrub buffer.
#[cfg(any(test, feature = "process-guest"))]
pub(crate) async fn serve_proxy_with_upstream<S>(
    stream: S,
    gate: Arc<dyn EgressGate>,
    ca: Option<Arc<SessionCa>>,
    upstream: Arc<UpstreamClient>,
    terminate: mitm::TerminateConfig,
) -> Result<(), VamNetworkError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    serve_proxy_inner(stream, gate, ca, Some(upstream), terminate).await
}

/// [`serve_proxy`] against the E2E fixture origin, with the fixture's small
/// scrub buffer. Test nodes only (`process-guest`).
#[cfg(feature = "process-guest")]
pub async fn serve_proxy_with_fixture<S>(
    stream: S,
    gate: Arc<dyn EgressGate>,
    ca: Option<Arc<SessionCa>>,
    fixture: &fixture::EgressFixture,
) -> Result<(), VamNetworkError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let terminate = mitm::TerminateConfig {
        offer_h2: false,
        limits: mitm::BodyLimits {
            scrub_buffer: fixture::FIXTURE_SCRUB_BUFFER,
            ..mitm::BodyLimits::default()
        },
    };
    serve_proxy_with_upstream(stream, gate, ca, fixture.upstream(), terminate).await
}

async fn serve_proxy_inner<S>(
    stream: S,
    gate: Arc<dyn EgressGate>,
    ca: Option<Arc<SessionCa>>,
    upstream: Option<Arc<UpstreamClient>>,
    terminate: mitm::TerminateConfig,
) -> Result<(), VamNetworkError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let mut conn = ProxyConn::new(stream);
    loop {
        let request = match conn.read_request().await {
            Ok(request) => request,
            Err(fail) => return write_fail(conn.into_inner(), fail).await,
        };
        if let RequestTarget::Connect { host, port } = request.target {
            return handle_connect(conn, host, port, gate, ca, upstream, terminate).await;
        }
        let keep_alive = request.keep_alive;
        if let Err(fail) = forward_http(&mut conn, &request, &gate).await {
            return write_fail(conn.into_inner(), fail).await;
        }
        if !keep_alive {
            return Ok(());
        }
    }
}

async fn handle_connect<S>(
    conn: ProxyConn<S>,
    host: String,
    port: u16,
    gate: Arc<dyn EgressGate>,
    ca: Option<Arc<SessionCa>>,
    upstream: Option<Arc<UpstreamClient>>,
    terminate: mitm::TerminateConfig,
) -> Result<(), VamNetworkError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let decision = match gate.check_destination(&host, port, "connect").await {
        Ok(decision) => decision,
        Err(err) => return write_fail(conn.into_inner(), gate_fail(err)).await,
    };
    let mut stream = conn.into_prefixed();
    match decision.mode {
        GateMode::Passthrough => {
            match connect::tunnel_passthrough(&mut stream, decision.addr, gate).await {
                Ok(()) => Ok(()),
                Err(PassthroughError::Dial) => {
                    write_fail(
                        &mut stream,
                        ProxyFail::Upstream("upstream connection failed"),
                    )
                    .await
                }
                Err(PassthroughError::Io(err)) => map_client_io(err),
            }
        }
        GateMode::Terminate => {
            // 200 is written here so early ClientHello bytes stay buffered on
            // `stream` for `serve_terminated`.
            if let Err(PassthroughError::Io(err)) = connect::write_established(&mut stream).await {
                return map_client_io(err);
            }
            let upstream = match upstream {
                Some(client) => client,
                None => Arc::new(UpstreamClient::new(None)?),
            };
            mitm::serve_terminated(stream, host, port, decision, gate, ca, upstream, terminate)
                .await
        }
    }
}

async fn forward_http<S>(
    conn: &mut ProxyConn<S>,
    request: &ParsedRequest,
    gate: &Arc<dyn EgressGate>,
) -> Result<(), ProxyFail>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let RequestTarget::Http { host, port, url } = &request.target else {
        return Err(ProxyFail::Malformed);
    };
    let decision = gate
        .check_destination(host, *port, "http")
        .await
        .map_err(gate_fail)?;
    let body = conn.read_body(&request.method, &request.headers).await?;
    let prepared = gate
        .prepare(EgressRequest {
            method: request.method.clone(),
            url: url.clone(),
            headers: egress_headers(&request.headers),
            body,
        })
        .await
        .map_err(gate_fail)?;
    if prepared.body.len() > MAX_REQUEST_BODY {
        return Err(ProxyFail::BodyTooLarge);
    }
    let upstream = dispatch_upstream(&decision, prepared).await?;
    let finished = gate.finish(upstream).await.map_err(gate_fail)?;
    if finished.body.len() > MAX_RESPONSE_BODY {
        return Err(ProxyFail::Upstream("upstream response too large"));
    }
    write_forwarded(&mut conn.io, &finished, request.keep_alive, &request.method)
        .await
        .map_err(ProxyFail::Io)?;
    Ok(())
}

async fn dispatch_upstream(
    decision: &GateDecision,
    prepared: EgressRequest,
) -> Result<EgressResponse, ProxyFail> {
    let uri = upstream_uri(&prepared.url)?;
    let set_host = !prepared
        .headers
        .keys()
        .any(|name| name.eq_ignore_ascii_case("host"));
    let method = hyper::Method::from_bytes(prepared.method.as_bytes()).map_err(|_| {
        tracing::debug!("egress prepare returned a method that is not a valid token");
        ProxyFail::Upstream("upstream request failed")
    })?;
    let mut builder = hyper::Request::builder().method(method).uri(uri);
    for (name, value) in &prepared.headers {
        if is_end_to_end(name) && header_safe(name, value) {
            builder = builder.header(name, value);
        }
    }
    let request = builder
        .body(Full::new(Bytes::from(prepared.body)))
        .map_err(|err| {
            tracing::debug!(error = %err, "egress upstream request was not valid HTTP");
            ProxyFail::Upstream("upstream request failed")
        })?;

    let response = connect::send_pinned(decision.addr, set_host, request)
        .await
        .map_err(|err| {
            tracing::debug!(error = %err, "egress upstream request failed");
            if err.is_connect() {
                ProxyFail::Upstream("upstream connection failed")
            } else {
                ProxyFail::Upstream("upstream request failed")
            }
        })?;

    let status = response.status().as_u16();
    let mut headers = HashMap::new();
    for (name, value) in response.headers().iter() {
        if !is_end_to_end(name.as_str()) {
            continue;
        }
        let value = String::from_utf8_lossy(value.as_bytes()).into_owned();
        headers
            .entry(name.as_str().to_string())
            .and_modify(|existing: &mut String| {
                existing.push_str(", ");
                existing.push_str(&value);
            })
            .or_insert(value);
    }
    let mut body = Vec::new();
    let mut incoming = response.into_body();
    while let Some(next) = incoming.frame().await {
        let frame = next.map_err(|err| {
            tracing::debug!(error = %err, "egress upstream body failed");
            ProxyFail::Upstream("upstream request failed")
        })?;
        let Ok(data) = frame.into_data() else {
            continue;
        };
        if body.len().saturating_add(data.len()) > MAX_RESPONSE_BODY {
            return Err(ProxyFail::Upstream("upstream response too large"));
        }
        body.extend_from_slice(&data);
    }
    Ok(EgressResponse {
        status_code: u32::from(status),
        headers,
        body,
        error: None,
    })
}

fn upstream_uri(url: &str) -> Result<hyper::Uri, ProxyFail> {
    let uri: hyper::Uri = url.parse().map_err(|_| {
        tracing::debug!("egress prepare returned a URL that is not an http URI");
        ProxyFail::Upstream("upstream request failed")
    })?;
    if uri.scheme_str() != Some("http") || uri.host().is_none() {
        return Err(ProxyFail::Upstream("upstream request failed"));
    }
    if !uri.path().is_empty() && !uri.path().starts_with('?') {
        return Ok(uri);
    }
    let authority = uri
        .authority()
        .map(|authority| authority.as_str())
        .unwrap_or("");
    let path = match uri.path_and_query().map(|pq| pq.as_str()) {
        Some(pq) if pq.starts_with('?') => format!("/{pq}"),
        Some(pq) if !pq.is_empty() => pq.to_string(),
        _ => "/".to_string(),
    };
    format!("http://{authority}{path}")
        .parse()
        .map_err(|_| ProxyFail::Upstream("upstream request failed"))
}

struct ProxyConn<S> {
    io: S,
    buf: Vec<u8>,
}

impl<S> ProxyConn<S> {
    fn new(io: S) -> Self {
        Self {
            io,
            buf: Vec::new(),
        }
    }

    fn into_inner(self) -> S {
        self.io
    }

    fn into_prefixed(self) -> Prefixed<S> {
        Prefixed {
            pending: self.buf,
            pending_pos: 0,
            inner: self.io,
        }
    }
}

impl<S> ProxyConn<S>
where
    S: AsyncRead + Unpin,
{
    async fn read_request(&mut self) -> Result<ParsedRequest, ProxyFail> {
        let head = self.read_head().await?;
        parse_head(&head)
    }

    async fn read_head(&mut self) -> Result<Vec<u8>, ProxyFail> {
        let deadline = tokio::time::Instant::now() + HEADER_READ_TIMEOUT;
        loop {
            match scan_head(&self.buf) {
                HeadScan::Done(end) => {
                    let mut rest = self.buf.split_off(end);
                    std::mem::swap(&mut self.buf, &mut rest);
                    return Ok(rest);
                }
                HeadScan::Malformed => return Err(ProxyFail::Malformed),
                HeadScan::TooLong => return Err(ProxyFail::HeadersTooLong),
                HeadScan::NeedMore => {
                    if tokio::time::Instant::now() >= deadline {
                        return Err(ProxyFail::Timeout);
                    }
                    self.fill_until(deadline).await?;
                }
            }
        }
    }

    async fn read_body(
        &mut self,
        method: &str,
        headers: &[(String, String)],
    ) -> Result<Vec<u8>, ProxyFail> {
        if method.eq_ignore_ascii_case("GET") || method.eq_ignore_ascii_case("HEAD") {
            if is_chunked(headers)? || matches!(content_length(headers)?, Some(len) if len > 0) {
                return Err(ProxyFail::Malformed);
            }
            return Ok(Vec::new());
        }
        if is_chunked(headers)? {
            return self.read_chunked().await;
        }
        match content_length(headers)? {
            Some(len) if len > MAX_REQUEST_BODY => Err(ProxyFail::BodyTooLarge),
            Some(len) => self.read_exact_bytes(len).await,
            None => Ok(Vec::new()),
        }
    }

    async fn read_chunked(&mut self) -> Result<Vec<u8>, ProxyFail> {
        let mut body = Vec::new();
        loop {
            let line = self.read_crlf_line(256).await?;
            let size_hex = line.split(|b| *b == b';').next().unwrap_or(&[]);
            let size_text = std::str::from_utf8(size_hex)
                .map_err(|_| ProxyFail::Malformed)?
                .trim();
            if size_text.is_empty() {
                return Err(ProxyFail::Malformed);
            }
            let size = usize::from_str_radix(size_text, 16).map_err(|_| ProxyFail::Malformed)?;
            if size == 0 {
                loop {
                    let trailer = self.read_crlf_line(MAX_HEADER_BYTES).await?;
                    if trailer.is_empty() {
                        break;
                    }
                }
                return Ok(body);
            }
            if body.len().saturating_add(size) > MAX_REQUEST_BODY {
                return Err(ProxyFail::BodyTooLarge);
            }
            body.extend_from_slice(&self.read_exact_bytes(size).await?);
            let crlf = self.read_exact_bytes(2).await?;
            if crlf.as_slice() != b"\r\n" {
                return Err(ProxyFail::Malformed);
            }
        }
    }

    async fn read_crlf_line(&mut self, max: usize) -> Result<Vec<u8>, ProxyFail> {
        loop {
            if let Some(pos) = find_slice(&self.buf, b"\r\n") {
                if pos > max {
                    return Err(ProxyFail::Malformed);
                }
                let mut rest = self.buf.split_off(pos);
                std::mem::swap(&mut self.buf, &mut rest);
                // `rest` is the line. Drop the CRLF from the front of the remainder.
                if self.buf.len() >= 2 {
                    self.buf.drain(..2);
                }
                return Ok(rest);
            }
            if self.buf.len() > max {
                return Err(ProxyFail::Malformed);
            }
            self.fill_body().await?;
        }
    }

    async fn read_exact_bytes(&mut self, n: usize) -> Result<Vec<u8>, ProxyFail> {
        while self.buf.len() < n {
            self.fill_body().await?;
        }
        let rest = self.buf.split_off(n);
        let body = std::mem::take(&mut self.buf);
        self.buf = rest;
        Ok(body)
    }

    async fn fill_until(&mut self, deadline: tokio::time::Instant) -> Result<(), ProxyFail> {
        if tokio::time::Instant::now() >= deadline {
            return Err(ProxyFail::Timeout);
        }
        let mut tmp = [0u8; 8192];
        let n = tokio::time::timeout_at(deadline, self.io.read(&mut tmp))
            .await
            .map_err(|_| ProxyFail::Timeout)?
            .map_err(ProxyFail::Io)?;
        if n == 0 {
            return if self.buf.is_empty() {
                Err(ProxyFail::Eof)
            } else {
                Err(ProxyFail::Malformed)
            };
        }
        self.buf.extend_from_slice(&tmp[..n]);
        Ok(())
    }

    async fn fill_body(&mut self) -> Result<(), ProxyFail> {
        let mut tmp = [0u8; 8192];
        let n = tokio::time::timeout(BODY_READ_TIMEOUT, self.io.read(&mut tmp))
            .await
            .map_err(|_| ProxyFail::Timeout)?
            .map_err(ProxyFail::Io)?;
        if n == 0 {
            return Err(ProxyFail::Malformed);
        }
        self.buf.extend_from_slice(&tmp[..n]);
        Ok(())
    }
}

struct ParsedRequest {
    method: String,
    keep_alive: bool,
    headers: Vec<(String, String)>,
    target: RequestTarget,
}

enum RequestTarget {
    Connect {
        host: String,
        port: u16,
    },
    Http {
        host: String,
        port: u16,
        url: String,
    },
}

enum HeadScan {
    NeedMore,
    Done(usize),
    Malformed,
    TooLong,
}

fn scan_head(buf: &[u8]) -> HeadScan {
    let window = buf.len().min(MAX_HEADER_BYTES);
    let scan = &buf[..window];
    if let Some(pos) = find_slice(scan, b"\r\n\r\n") {
        return HeadScan::Done(pos + 4);
    }
    if has_bare_lf(scan) {
        return HeadScan::Malformed;
    }
    if buf.len() >= MAX_HEADER_BYTES {
        HeadScan::TooLong
    } else {
        HeadScan::NeedMore
    }
}

fn parse_head(head: &[u8]) -> Result<ParsedRequest, ProxyFail> {
    let text = std::str::from_utf8(head).map_err(|_| ProxyFail::Malformed)?;
    let mut lines = text.split("\r\n");
    let request_line = lines.next().unwrap_or("");
    if request_line.is_empty() {
        return Err(ProxyFail::Malformed);
    }
    let mut parts = request_line.split(' ');
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("");
    let version = parts.next().unwrap_or("");
    if method.is_empty() || target.is_empty() || version.is_empty() || parts.next().is_some() {
        return Err(ProxyFail::Malformed);
    }
    if !is_token(method) {
        return Err(ProxyFail::Malformed);
    }
    let http10 = match version {
        "HTTP/1.1" => false,
        "HTTP/1.0" => true,
        other if other.starts_with("HTTP/") => return Err(ProxyFail::Version),
        _ => return Err(ProxyFail::Malformed),
    };

    let mut headers = Vec::new();
    let mut saw_close = false;
    let mut saw_keep_alive = false;
    for line in lines {
        if line.is_empty() {
            continue;
        }
        if line.starts_with(' ') || line.starts_with('\t') {
            return Err(ProxyFail::Malformed);
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(ProxyFail::Malformed);
        };
        if !is_token(name) {
            return Err(ProxyFail::Malformed);
        }
        let value = value.trim();
        if value.bytes().any(|b| b == 0 || b == b'\r' || b == b'\n') {
            return Err(ProxyFail::Malformed);
        }
        if name.eq_ignore_ascii_case("connection") || name.eq_ignore_ascii_case("proxy-connection")
        {
            for token in value.split(',') {
                let token = token.trim();
                if token.eq_ignore_ascii_case("close") {
                    saw_close = true;
                } else if token.eq_ignore_ascii_case("keep-alive") {
                    saw_keep_alive = true;
                }
            }
        }
        headers.push((name.to_string(), value.to_string()));
    }

    let keep_alive = if saw_close {
        false
    } else if http10 {
        saw_keep_alive
    } else {
        true
    };
    let target = if method.eq_ignore_ascii_case("CONNECT") {
        let (host, port) = parse_connect_authority(target)?;
        RequestTarget::Connect { host, port }
    } else {
        let (host, port, url) = parse_http_target(target)?;
        RequestTarget::Http { host, port, url }
    };
    Ok(ParsedRequest {
        method: method.to_string(),
        keep_alive,
        headers,
        target,
    })
}

fn parse_connect_authority(target: &str) -> Result<(String, u16), ProxyFail> {
    if target.starts_with('[') {
        let end = target.find(']').ok_or(ProxyFail::Malformed)?;
        let host = &target[1..end];
        let rest = target.get(end + 1..).ok_or(ProxyFail::Malformed)?;
        let port = rest
            .strip_prefix(':')
            .ok_or(ProxyFail::Malformed)?
            .parse::<u16>()
            .map_err(|_| ProxyFail::Malformed)?;
        if host.is_empty() || port == 0 {
            return Err(ProxyFail::Malformed);
        }
        return Ok((host.to_string(), port));
    }
    let (host, port_text) = target.rsplit_once(':').ok_or(ProxyFail::Malformed)?;
    if host.is_empty() || host.contains(':') {
        return Err(ProxyFail::Malformed);
    }
    let port = port_text.parse::<u16>().map_err(|_| ProxyFail::Malformed)?;
    if port == 0 {
        return Err(ProxyFail::Malformed);
    }
    Ok((host.to_string(), port))
}

fn parse_http_target(target: &str) -> Result<(String, u16, String), ProxyFail> {
    if !target.starts_with("http://") {
        return Err(ProxyFail::Malformed);
    }
    let uri: hyper::Uri = target.parse().map_err(|_| ProxyFail::Malformed)?;
    if uri.scheme_str() != Some("http") {
        return Err(ProxyFail::Malformed);
    }
    let host = uri.host().ok_or(ProxyFail::Malformed)?;
    if host.is_empty() {
        return Err(ProxyFail::Malformed);
    }
    let port = uri.port_u16().unwrap_or(80);
    if port == 0 {
        return Err(ProxyFail::Malformed);
    }
    Ok((host.to_string(), port, target.to_string()))
}

fn content_length(headers: &[(String, String)]) -> Result<Option<usize>, ProxyFail> {
    let mut found = None;
    for (name, value) in headers {
        if !name.eq_ignore_ascii_case("content-length") {
            continue;
        }
        let len = value.parse::<usize>().map_err(|_| ProxyFail::Malformed)?;
        if let Some(prev) = found {
            if prev != len {
                return Err(ProxyFail::Malformed);
            }
        }
        found = Some(len);
    }
    Ok(found)
}

fn is_chunked(headers: &[(String, String)]) -> Result<bool, ProxyFail> {
    let mut chunked = false;
    for (name, value) in headers {
        if !name.eq_ignore_ascii_case("transfer-encoding") {
            continue;
        }
        for token in value.split(',') {
            if token.trim().eq_ignore_ascii_case("chunked") {
                chunked = true;
            } else if !token.trim().is_empty() {
                return Err(ProxyFail::Malformed);
            }
        }
    }
    Ok(chunked)
}

fn egress_headers(headers: &[(String, String)]) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for (name, value) in headers {
        if !is_end_to_end(name) || name.eq_ignore_ascii_case("content-length") {
            continue;
        }
        map.entry(name.clone())
            .and_modify(|existing: &mut String| {
                existing.push_str(", ");
                existing.push_str(value);
            })
            .or_insert_with(|| value.clone());
    }
    map
}

fn is_end_to_end(name: &str) -> bool {
    !is_hop_by_hop(name)
}

pub(crate) fn is_hop_by_hop(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "proxy-connection"
            | "proxy-agent"
    )
}

fn header_safe(name: &str, value: &str) -> bool {
    is_token(name) && !value.bytes().any(|b| b == 0 || b == b'\r' || b == b'\n')
}

fn is_token(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|b| {
            matches!(
                b,
                b'!'
                    | b'#'
                    | b'$'
                    | b'%'
                    | b'&'
                    | b'\''
                    | b'*'
                    | b'+'
                    | b'-'
                    | b'.'
                    | b'^'
                    | b'_'
                    | b'`'
                    | b'|'
                    | b'~'
                    | b'0'..=b'9'
                    | b'A'..=b'Z'
                    | b'a'..=b'z'
            )
        })
}

fn find_slice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len())
        .position(|window| window == needle)
}

fn has_bare_lf(buf: &[u8]) -> bool {
    buf.iter()
        .enumerate()
        .any(|(index, byte)| *byte == b'\n' && (index == 0 || buf[index - 1] != b'\r'))
}

#[derive(Debug)]
enum ProxyFail {
    Eof,
    Malformed,
    HeadersTooLong,
    Timeout,
    BodyTooLarge,
    Version,
    Denied(String),
    Resolution,
    Quota,
    Upstream(&'static str),
    Internal,
    Io(std::io::Error),
}

fn gate_fail(err: GateError) -> ProxyFail {
    match err {
        GateError::Denied(reason) => ProxyFail::Denied(reason),
        GateError::QuotaExceeded => ProxyFail::Quota,
        GateError::Resolution(detail) => {
            tracing::debug!(error = %detail, "egress name resolution failed");
            ProxyFail::Resolution
        }
        GateError::Internal(detail) => {
            tracing::debug!(error = %detail, "egress gate failed");
            ProxyFail::Internal
        }
    }
}

impl ProxyFail {
    fn response(&self) -> Option<(u16, String)> {
        let pair = match self {
            Self::Malformed => (400, "malformed request line".to_string()),
            Self::HeadersTooLong => (431, "request header fields too large".to_string()),
            Self::Timeout => (408, "request timeout".to_string()),
            Self::BodyTooLarge => (413, "request body too large".to_string()),
            Self::Version => (505, "HTTP version not supported".to_string()),
            Self::Denied(reason) => (403, one_line(reason, "denied")),
            Self::Resolution => (502, "name resolution failed".to_string()),
            Self::Quota => (403, "quota exceeded".to_string()),
            Self::Upstream(reason) => (502, (*reason).to_string()),
            Self::Internal => (500, "internal error".to_string()),
            Self::Eof | Self::Io(_) => return None,
        };
        Some(pair)
    }
}

fn one_line(raw: &str, fallback: &str) -> String {
    let mut out = String::new();
    for ch in raw.chars() {
        if ch == '\n' || ch == '\r' {
            break;
        }
        if ch.is_control() {
            continue;
        }
        out.push(ch);
        if out.len() >= 200 {
            break;
        }
    }
    let out = out.trim();
    if out.is_empty() {
        fallback.to_string()
    } else {
        out.to_string()
    }
}

async fn write_fail<S>(mut io: S, fail: ProxyFail) -> Result<(), VamNetworkError>
where
    S: AsyncWrite + Unpin,
{
    if let Some((status, body)) = fail.response() {
        return match write_http_error(&mut io, status, &body).await {
            Ok(()) => Ok(()),
            Err(err) => map_client_io(err),
        };
    }
    match fail {
        ProxyFail::Io(err) => map_client_io(err),
        ProxyFail::Eof => Ok(()),
        _ => Ok(()),
    }
}

fn map_client_io(err: std::io::Error) -> Result<(), VamNetworkError> {
    if is_disconnect(&err) {
        Ok(())
    } else {
        Err(err.into())
    }
}

fn is_disconnect(err: &std::io::Error) -> bool {
    matches!(
        err.kind(),
        std::io::ErrorKind::BrokenPipe
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::UnexpectedEof
            | std::io::ErrorKind::NotConnected
    )
}

async fn write_http_error<S>(io: &mut S, status: u16, body: &str) -> std::io::Result<()>
where
    S: AsyncWrite + Unpin,
{
    let bytes = body.as_bytes();
    let head = format!(
        "HTTP/1.1 {status} {phrase}\r\nProxy-Agent: {PROXY_AGENT}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n",
        phrase = reason_phrase(status),
        len = bytes.len(),
    );
    io.write_all(head.as_bytes()).await?;
    io.write_all(bytes).await?;
    io.flush().await?;
    Ok(())
}

async fn write_forwarded<S>(
    io: &mut S,
    response: &EgressResponse,
    keep_alive: bool,
    method: &str,
) -> std::io::Result<()>
where
    S: AsyncWrite + Unpin,
{
    let code = u16::try_from(response.status_code).unwrap_or(502);
    let code = if (100..600).contains(&code) {
        code
    } else {
        502
    };
    let mut head = format!(
        "HTTP/1.1 {code} {phrase}\r\nProxy-Agent: {PROXY_AGENT}\r\n",
        phrase = reason_phrase(code),
    );
    for (name, value) in &response.headers {
        if is_hop_by_hop(name)
            || name.eq_ignore_ascii_case("content-length")
            || !header_safe(name, value)
        {
            continue;
        }
        head.push_str(name);
        head.push_str(": ");
        head.push_str(value);
        head.push_str("\r\n");
    }
    let connection = if keep_alive { "keep-alive" } else { "close" };
    head.push_str(&format!(
        "Content-Length: {}\r\nConnection: {connection}\r\n\r\n",
        response.body.len()
    ));
    io.write_all(head.as_bytes()).await?;
    if !method.eq_ignore_ascii_case("HEAD") && !response.body.is_empty() {
        io.write_all(&response.body).await?;
    }
    io.flush().await?;
    Ok(())
}

fn reason_phrase(status: u16) -> &'static str {
    hyper::StatusCode::from_u16(status)
        .ok()
        .and_then(|code| code.canonical_reason())
        .unwrap_or("Error")
}

/// Inbound bytes already pulled off the guest socket, then the live socket.
struct Prefixed<S> {
    pending: Vec<u8>,
    pending_pos: usize,
    inner: S,
}

impl<S> AsyncRead for Prefixed<S>
where
    S: AsyncRead + Unpin,
{
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let this = self.get_mut();
        if this.pending_pos < this.pending.len() {
            let rest = &this.pending[this.pending_pos..];
            let n = rest.len().min(buf.remaining());
            buf.put_slice(&rest[..n]);
            this.pending_pos += n;
            return std::task::Poll::Ready(Ok(()));
        }
        std::pin::Pin::new(&mut this.inner).poll_read(cx, buf)
    }
}

impl<S> AsyncWrite for Prefixed<S>
where
    S: AsyncWrite + Unpin,
{
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod mitm_tests;
#[cfg(test)]
mod tests;
