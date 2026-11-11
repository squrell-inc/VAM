//! TLS-terminating `CONNECT` path.
//!
//! After `200 Connection Established` the proxy accepts TLS with the session
//! CA, reads HTTP/1.1 inside the tunnel, and opens a verified upstream TLS
//! connection to the gate's pinned address.
//!
//! Request and response bodies are streamed with caps ([`BodyLimits`]).
//! `EgressGate::finish` scrubs a fully buffered response. A response larger
//! than [`BodyLimits::scrub_buffer`] (default 8 MiB) cannot be scrubbed, and a
//! secret split across chunks needs a streaming scrubber, which is a follow-up.
//! What happens to it depends on the destination:
//!
//! * A destination that holds a secret binding in this session
//!   (`PreparedEgress::secret_bound`) fails closed. The response is refused with
//!   a `502` before a byte of it, its status or its headers reaches the guest,
//!   and the gate is told (`EgressGate::response_refused`, an `egress.decided`
//!   `DENY` with rule `oversize_with_secret`). This holds whether or not the
//!   request carried a placeholder, because an upstream can echo a secret it
//!   was sent earlier.
//! * Any other destination streams to the guest without scrubbing and emits an
//!   `egress.decision` event with `scrubbed=false`, so large downloads still work.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::{Body, Frame, Incoming};
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_rustls::TlsAcceptor;
use vam_spec::v1::{EgressRequest, EgressResponse};

use crate::error::VamNetworkError;
use crate::gate::{EgressGate, GateDecision, GateError, REFUSED_OVERSIZE_WITH_SECRET};
use crate::tls_ca::SessionCa;
use crate::upstream::{tls_failure_reason, UpstreamClient, UpstreamError};

/// Guest request bodies larger than this are rejected with 413.
pub(super) const DEFAULT_MAX_REQUEST_BODY: usize = 32 * 1024 * 1024;
/// Upstream response bodies larger than this are rejected with 502.
pub(super) const DEFAULT_MAX_RESPONSE_BODY: usize = 256 * 1024 * 1024;
/// Responses at or under this size are buffered so [`EgressGate::finish`] can
/// scrub them. A larger response from a destination that holds a session secret
/// is refused; from any other destination it streams unscrubbed.
pub(super) const DEFAULT_SCRUB_BUFFER: usize = 8 * 1024 * 1024;

const HEADER_IDLE: Duration = Duration::from_secs(30);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);

/// Caps for the terminating path. Defaults match the E8-1.4 limits.
#[derive(Debug, Clone, Copy)]
pub(crate) struct BodyLimits {
    pub max_request_body: usize,
    pub max_response_body: usize,
    pub scrub_buffer: usize,
}

impl Default for BodyLimits {
    fn default() -> Self {
        Self {
            max_request_body: DEFAULT_MAX_REQUEST_BODY,
            max_response_body: DEFAULT_MAX_RESPONSE_BODY,
            scrub_buffer: DEFAULT_SCRUB_BUFFER,
        }
    }
}

/// Guest-facing TLS options. HTTP/2 is advertised only when `offer_h2` is set,
/// and it is off by default. Negotiating `h2` closes the tunnel: this build
/// serves HTTP/1.1.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct TerminateConfig {
    pub offer_h2: bool,
    pub limits: BodyLimits,
}

/// ALPN protocols offered to the guest. `h2` is first only when the flag is on.
pub(super) fn alpn_protocols(offer_h2: bool) -> Vec<Vec<u8>> {
    if offer_h2 {
        vec![b"h2".to_vec(), b"http/1.1".to_vec()]
    } else {
        vec![b"http/1.1".to_vec()]
    }
}

/// TLS-terminating `CONNECT` path.
///
/// The caller has already written `200 Connection Established`. Bytes already
/// read past the proxy headers are still sitting on `stream` (a client may
/// pipeline the TLS ClientHello).
#[allow(clippy::too_many_arguments)]
pub(super) async fn serve_terminated<S>(
    stream: S,
    host: String,
    port: u16,
    decision: GateDecision,
    gate: Arc<dyn EgressGate>,
    ca: Option<Arc<SessionCa>>,
    upstream: Arc<UpstreamClient>,
    config: TerminateConfig,
) -> Result<(), VamNetworkError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let Some(ca) = ca else {
        return Err(VamNetworkError::Internal(
            "tls termination requires a session CA".to_string(),
        ));
    };
    // SNI selects the leaf. When the client omits SNI, `server_cert_resolver_for`
    // presents `leaf_for(connect_host)`.
    let resolver = ca.server_cert_resolver_for(&host)?;
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let mut server = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|err| VamNetworkError::Internal(format!("tls versions: {err}")))?
        .with_no_client_auth()
        .with_cert_resolver(resolver);
    server.alpn_protocols = alpn_protocols(config.offer_h2);
    let acceptor = TlsAcceptor::from(Arc::new(server));

    let tls = match tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(stream)).await {
        Ok(Ok(tls)) => tls,
        Ok(Err(err)) => {
            tracing::debug!(error = %err, "guest tls handshake failed");
            return Ok(());
        }
        Err(_) => {
            tracing::debug!("guest tls handshake timed out");
            return Ok(());
        }
    };

    if tls.get_ref().1.alpn_protocol() == Some(b"h2") {
        tracing::debug!("guest negotiated h2; the terminator serves HTTP/1.1 only");
        return Ok(());
    }

    let sni_matches = match tls.get_ref().1.server_name() {
        Some(sni) => same_host(&host, sni),
        None => true,
    };
    let ctx = Ctx {
        host,
        port,
        addr: decision.addr,
        gate,
        upstream,
        sni_matches,
        limits: config.limits,
    };
    let service = service_fn(move |request| {
        let ctx = ctx.clone();
        async move { Ok::<_, std::convert::Infallible>(handle_request(ctx, request).await) }
    });

    let io = TokioIo::new(tls);
    match hyper::server::conn::http1::Builder::new()
        .keep_alive(true)
        .serve_connection(io, service)
        .await
    {
        Ok(()) => Ok(()),
        Err(err) => {
            tracing::debug!(error = %err, "terminated egress connection closed");
            Ok(())
        }
    }
}

#[derive(Clone)]
struct Ctx {
    host: String,
    port: u16,
    addr: std::net::SocketAddr,
    gate: Arc<dyn EgressGate>,
    upstream: Arc<UpstreamClient>,
    sni_matches: bool,
    limits: BodyLimits,
}

async fn handle_request(ctx: Ctx, request: Request<Incoming>) -> Response<GuestBody> {
    if !ctx.sni_matches {
        return text_response(
            StatusCode::MISDIRECTED_REQUEST,
            "host does not match connect target",
            true,
        );
    }

    let method = request.method().as_str().to_string();
    let head = method.eq_ignore_ascii_case("HEAD");
    if let Some(response) = check_target(&ctx, &request) {
        return response;
    }

    let path = request_path(request.uri());
    let mut lines = header_lines(request.headers());
    match declared_length(request.headers()) {
        Ok(Some(len)) if len > ctx.limits.max_request_body => {
            return text_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                "request body too large",
                true,
            );
        }
        Err(()) => {
            return text_response(StatusCode::BAD_REQUEST, "malformed request line", true);
        }
        _ => {}
    }

    let body = match collect_request(&ctx, request.into_body()).await {
        Ok(body) => body,
        Err(fail) => return read_fail(fail),
    };

    let prepared_egress = match ctx
        .gate
        .prepare_request(EgressRequest {
            method: method.clone(),
            url: https_url(&ctx.host, ctx.port, &path),
            headers: first_header_values(&lines),
            body,
        })
        .await
    {
        Ok(prepared) => prepared,
        Err(err) => return gate_response(err),
    };
    let secret_bound = prepared_egress.secret_bound;
    let mut ticket = prepared_egress.ticket;
    let prepared = prepared_egress.request;
    if prepared.body.len() > ctx.limits.max_request_body {
        return text_response(
            StatusCode::PAYLOAD_TOO_LARGE,
            "request body too large",
            true,
        );
    }

    let path = path_from_url(&prepared.url).unwrap_or(path);
    if !path.starts_with('/') {
        return text_response(StatusCode::BAD_REQUEST, "malformed request line", true);
    }
    lines = merge_request_headers(lines, &prepared.headers);
    if !lines
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("host"))
    {
        lines.push(("host".to_string(), host_value(&ctx.host, ctx.port)));
    }

    let upstream_request =
        match build_upstream_request(&ctx, &prepared.method, &path, &lines, prepared.body) {
            Ok(request) => request,
            Err(()) => {
                return text_response(StatusCode::BAD_GATEWAY, "upstream request failed", true);
            }
        };

    // A journaled write is recorded as sent immediately before it leaves the host; a journal
    // that cannot be written sends nothing (R1). A ticket dropped after this point without an
    // answer (an upstream error, a timeout, this future cancelled) leaves the write unknown.
    if let Some(ticket) = ticket.as_mut() {
        if let Err(refusal) = ticket.commit_send() {
            return gate_response(GateError::Denied(refusal.text));
        }
    }
    let response = match tokio::time::timeout(
        HEADER_IDLE,
        ctx.upstream.send(&ctx.host, ctx.addr, upstream_request),
    )
    .await
    {
        Ok(Ok(response)) => response,
        Ok(Err(err)) => return upstream_response(&err),
        Err(_) => {
            tracing::debug!("upstream request timed out");
            return text_response(StatusCode::BAD_GATEWAY, "upstream request failed", true);
        }
    };
    if let Some(ticket) = ticket.as_mut() {
        ticket.answered(response.status().as_u16());
    }

    match read_upstream(&ctx, response, head, secret_bound).await {
        Ok(response) => response,
        Err(fail) => read_fail(fail),
    }
}

fn check_target(ctx: &Ctx, request: &Request<Incoming>) -> Option<Response<GuestBody>> {
    let hosts: Vec<String> = request
        .headers()
        .get_all(hyper::header::HOST)
        .iter()
        .map(|value| String::from_utf8_lossy(value.as_bytes()).into_owned())
        .collect();
    if hosts.len() > 1 {
        return Some(text_response(
            StatusCode::BAD_REQUEST,
            "malformed request line",
            true,
        ));
    }
    if let Some(host) = hosts.first() {
        if !host_header_matches(&ctx.host, ctx.port, host) {
            return Some(text_response(
                StatusCode::MISDIRECTED_REQUEST,
                "host does not match connect target",
                true,
            ));
        }
    }
    if let Some(uri_host) = request.uri().host() {
        let uri_port = request.uri().port_u16().unwrap_or(ctx.port);
        if !same_host(&ctx.host, uri_host) || uri_port != ctx.port {
            return Some(text_response(
                StatusCode::MISDIRECTED_REQUEST,
                "host does not match connect target",
                true,
            ));
        }
    }
    None
}

async fn collect_request(ctx: &Ctx, mut incoming: Incoming) -> Result<Vec<u8>, ReadFail> {
    let mut body = Vec::new();
    loop {
        let next = match tokio::time::timeout(HEADER_IDLE, incoming.frame()).await {
            Ok(next) => next,
            Err(_) => return Err(ReadFail::Timeout),
        };
        let Some(next) = next else {
            return Ok(body);
        };
        let frame = next.map_err(|err| {
            tracing::debug!(error = %err, "guest request body failed");
            ReadFail::Malformed
        })?;
        let Ok(data) = frame.into_data() else {
            continue;
        };
        if data.is_empty() {
            continue;
        }
        if body.len().saturating_add(data.len()) > ctx.limits.max_request_body {
            return Err(ReadFail::TooLarge);
        }
        ctx.gate
            .record_bytes(data.len() as u64)
            .map_err(ReadFail::Gate)?;
        body.extend_from_slice(&data);
    }
}

async fn read_upstream(
    ctx: &Ctx,
    response: hyper::Response<Incoming>,
    head: bool,
    secret_bound: bool,
) -> Result<Response<GuestBody>, ReadFail> {
    let status_code = response.status().as_u16();
    let lines = header_lines(response.headers());
    // A response with no body (HEAD, 204, 304) can declare a length it never sends.
    let bodyless = head || matches!(status_code, 204 | 304) || (100..200).contains(&status_code);
    if let Some(len) = content_length_header(response.headers()) {
        if len > ctx.limits.max_response_body {
            return Err(ReadFail::ResponseTooLarge);
        }
        if len > ctx.limits.scrub_buffer && !(secret_bound && bodyless) {
            if secret_bound {
                return Err(refuse_oversize(ctx, len));
            }
            note_unscrubbed(&ctx.host, ctx.port, len);
            let body = GuestBody::stream(
                Bytes::new(),
                response.into_body(),
                ctx.limits.max_response_body,
                Arc::clone(&ctx.gate),
            );
            let advertised = if head { Some(0) } else { Some(len as u64) };
            return Ok(forward_response(status_code, &lines, body, advertised));
        }
    }

    let mut incoming = response.into_body();
    let mut buffered = Vec::new();
    loop {
        let next = match tokio::time::timeout(HEADER_IDLE, incoming.frame()).await {
            Ok(next) => next,
            Err(_) => return Err(ReadFail::Timeout),
        };
        let Some(next) = next else {
            return finish_buffered(ctx, status_code, lines, buffered, head).await;
        };
        let frame = next.map_err(|err| {
            tracing::debug!(error = %err, "upstream response body failed");
            ReadFail::Upstream
        })?;
        let Ok(data) = frame.into_data() else {
            continue;
        };
        if data.is_empty() {
            continue;
        }
        let next_len = buffered.len().saturating_add(data.len());
        if next_len > ctx.limits.max_response_body {
            return Err(ReadFail::ResponseTooLarge);
        }
        ctx.gate
            .record_bytes(data.len() as u64)
            .map_err(ReadFail::Gate)?;
        if next_len > ctx.limits.scrub_buffer {
            if secret_bound {
                // Nothing has been sent to the guest yet: the head is built only after
                // the body is read or streaming is chosen. Dropping `incoming` ends the
                // upstream exchange.
                return Err(refuse_oversize(ctx, next_len));
            }
            note_unscrubbed(&ctx.host, ctx.port, next_len);
            let mut prefix = buffered;
            prefix.extend_from_slice(&data);
            let body = GuestBody::stream(
                Bytes::from(prefix),
                incoming,
                ctx.limits.max_response_body,
                Arc::clone(&ctx.gate),
            );
            return Ok(forward_response(status_code, &lines, body, None));
        }
        buffered.extend_from_slice(&data);
    }
}

async fn finish_buffered(
    ctx: &Ctx,
    status_code: u16,
    lines: Vec<(String, String)>,
    buffered: Vec<u8>,
    head: bool,
) -> Result<Response<GuestBody>, ReadFail> {
    let before = first_header_values(&lines);
    let finished = ctx
        .gate
        .finish(EgressResponse {
            status_code: u32::from(status_code),
            headers: before.clone(),
            body: buffered,
            error: None,
        })
        .await
        .map_err(ReadFail::Gate)?;
    let status = u16::try_from(finished.status_code).unwrap_or(StatusCode::BAD_GATEWAY.as_u16());
    let status = if (100..600).contains(&status) {
        status
    } else {
        StatusCode::BAD_GATEWAY.as_u16()
    };
    let headers = reconcile_response_headers(&lines, &before, &finished.headers);
    let advertised = if head {
        Some(0)
    } else {
        Some(finished.body.len() as u64)
    };
    let body = if head {
        GuestBody::full(Bytes::new())
    } else {
        GuestBody::full(Bytes::from(finished.body))
    };
    Ok(forward_response(status, &headers, body, advertised))
}

fn forward_response(
    status: u16,
    headers: &[(String, String)],
    body: GuestBody,
    content_length: Option<u64>,
) -> Response<GuestBody> {
    let code = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut builder = Response::builder().status(code);
    for (name, value) in headers {
        if super::is_hop_by_hop(name)
            || name.eq_ignore_ascii_case("content-length")
            || name.eq_ignore_ascii_case("transfer-encoding")
        {
            continue;
        }
        let Ok(name) = hyper::header::HeaderName::from_bytes(name.as_bytes()) else {
            continue;
        };
        let Ok(value) = hyper::header::HeaderValue::from_bytes(value.as_bytes()) else {
            continue;
        };
        builder = builder.header(name, value);
    }
    if let Some(len) = content_length {
        builder = builder.header(hyper::header::CONTENT_LENGTH, len);
    }
    builder
        .body(body)
        .unwrap_or_else(|_| text_response(StatusCode::BAD_GATEWAY, "upstream request failed", true))
}

fn build_upstream_request(
    ctx: &Ctx,
    method: &str,
    path: &str,
    headers: &[(String, String)],
    body: Vec<u8>,
) -> Result<Request<Full<Bytes>>, ()> {
    let uri = origin_uri(&ctx.host, ctx.port, path)?;
    let method = hyper::Method::from_bytes(method.as_bytes()).map_err(|_| ())?;
    let mut builder = Request::builder().method(method).uri(uri);
    for (name, value) in headers {
        if super::is_hop_by_hop(name) || name.eq_ignore_ascii_case("content-length") {
            continue;
        }
        let name = hyper::header::HeaderName::from_bytes(name.as_bytes()).map_err(|_| ())?;
        let value = hyper::header::HeaderValue::from_bytes(value.as_bytes()).map_err(|_| ())?;
        builder = builder.header(name, value);
    }
    builder.body(Full::new(Bytes::from(body))).map_err(|_| ())
}

/// The response from a secret-bound destination does not fit the scrub buffer.
fn refuse_oversize(ctx: &Ctx, response_bytes: usize) -> ReadFail {
    tracing::info!(
        target: "vam_network::egress",
        event = "egress.decision",
        scrubbed = false,
        refused = true,
        reason = REFUSED_OVERSIZE_WITH_SECRET,
        host = %ctx.host,
        port = ctx.port,
        response_bytes,
        "response from a secret-bound destination exceeds the scrub buffer and is refused"
    );
    ctx.gate
        .response_refused(&ctx.host, ctx.port, REFUSED_OVERSIZE_WITH_SECRET);
    ReadFail::OversizeWithSecret
}

fn note_unscrubbed(host: &str, port: u16, response_bytes: usize) {
    // `scrubbed=false` marks a body that bypassed `EgressGate::finish`.
    // A streaming scrubber that handles a secret split across chunks is a follow-up.
    tracing::info!(
        target: "vam_network::egress",
        event = "egress.decision",
        scrubbed = false,
        %host,
        port,
        response_bytes,
        "response exceeds the scrub buffer and streams unscrubbed"
    );
}

fn upstream_response(err: &UpstreamError) -> Response<GuestBody> {
    let reason = if err.is_tls() {
        err.guest_reason()
    } else if let Some(reason) = tls_failure_reason(err) {
        reason
    } else {
        err.guest_reason()
    };
    tracing::debug!(error = %err, reason, "upstream request failed");
    text_response(StatusCode::BAD_GATEWAY, reason, true)
}

fn gate_response(err: GateError) -> Response<GuestBody> {
    let (status, body) = match err {
        GateError::Denied(reason) => (StatusCode::FORBIDDEN, super::one_line(&reason, "denied")),
        GateError::QuotaExceeded => (StatusCode::FORBIDDEN, "quota exceeded".to_string()),
        GateError::Resolution(detail) => {
            tracing::debug!(error = %detail, "egress name resolution failed");
            (
                StatusCode::BAD_GATEWAY,
                "name resolution failed".to_string(),
            )
        }
        GateError::Internal(detail) => {
            tracing::debug!(error = %detail, "egress gate failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal error".to_string(),
            )
        }
    };
    text_response(status, &body, true)
}

fn read_fail(fail: ReadFail) -> Response<GuestBody> {
    match fail {
        ReadFail::TooLarge => text_response(
            StatusCode::PAYLOAD_TOO_LARGE,
            "request body too large",
            true,
        ),
        ReadFail::ResponseTooLarge => {
            text_response(StatusCode::BAD_GATEWAY, "upstream response too large", true)
        }
        ReadFail::OversizeWithSecret => text_response(
            StatusCode::BAD_GATEWAY,
            "response too large to scrub; refused because this host holds a secret",
            true,
        ),
        ReadFail::Timeout => text_response(StatusCode::REQUEST_TIMEOUT, "request timeout", true),
        ReadFail::Malformed => {
            text_response(StatusCode::BAD_REQUEST, "malformed request line", true)
        }
        ReadFail::Upstream => {
            text_response(StatusCode::BAD_GATEWAY, "upstream request failed", true)
        }
        ReadFail::Gate(err) => gate_response(err),
    }
}

enum ReadFail {
    TooLarge,
    ResponseTooLarge,
    /// Over the scrub buffer, from a destination that holds a session secret.
    OversizeWithSecret,
    Timeout,
    Malformed,
    Upstream,
    Gate(GateError),
}

fn text_response(status: StatusCode, body: &str, close: bool) -> Response<GuestBody> {
    let bytes = Bytes::from(body.to_string());
    let mut builder = Response::builder()
        .status(status)
        .header(hyper::header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .header(hyper::header::CONTENT_LENGTH, bytes.len())
        .header("proxy-agent", super::PROXY_AGENT);
    if close {
        builder = builder.header(hyper::header::CONNECTION, "close");
    }
    builder
        .body(GuestBody::full(bytes))
        .unwrap_or_else(|_| Response::new(GuestBody::full(Bytes::new())))
}

fn header_lines(headers: &hyper::HeaderMap) -> Vec<(String, String)> {
    let mut lines = Vec::new();
    for (name, value) in headers.iter() {
        if super::is_hop_by_hop(name.as_str()) {
            continue;
        }
        lines.push((
            name.as_str().to_string(),
            String::from_utf8_lossy(value.as_bytes()).into_owned(),
        ));
    }
    lines
}

/// One value per header name, the first one. Repeated lines stay on the
/// `Vec` that is forwarded upstream; they are not comma-joined into this map.
fn first_header_values(lines: &[(String, String)]) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for (name, value) in lines {
        if name.eq_ignore_ascii_case("content-length")
            || name.eq_ignore_ascii_case("transfer-encoding")
        {
            continue;
        }
        map.entry(name.clone()).or_insert_with(|| value.clone());
    }
    map
}

fn merge_request_headers(
    lines: Vec<(String, String)>,
    prepared: &HashMap<String, String>,
) -> Vec<(String, String)> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for (name, _) in &lines {
        *counts.entry(name.to_ascii_lowercase()).or_default() += 1;
    }
    let mut seen: HashMap<String, usize> = HashMap::new();
    let mut out = Vec::new();
    for (name, value) in lines {
        if lookup(prepared, &name).is_none() {
            continue;
        }
        let key = name.to_ascii_lowercase();
        let index = seen.entry(key.clone()).or_insert(0);
        *index += 1;
        let appearances = counts.get(&key).copied().unwrap_or(1);
        if appearances == 1 || *index == 1 {
            let updated = lookup(prepared, &name).unwrap_or(value);
            out.push((name, updated));
        } else {
            out.push((name, value));
        }
    }
    for (name, value) in prepared {
        if counts.keys().all(|key| !key.eq_ignore_ascii_case(name))
            && !super::is_hop_by_hop(name)
            && !name.eq_ignore_ascii_case("content-length")
        {
            out.push((name.clone(), value.clone()));
        }
    }
    out
}

fn reconcile_response_headers(
    lines: &[(String, String)],
    before: &HashMap<String, String>,
    finished: &HashMap<String, String>,
) -> Vec<(String, String)> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for (name, _) in lines {
        *counts.entry(name.to_ascii_lowercase()).or_default() += 1;
    }
    let mut seen: HashMap<String, usize> = HashMap::new();
    let mut out = Vec::new();
    for (name, value) in lines {
        if super::is_hop_by_hop(name)
            || name.eq_ignore_ascii_case("content-length")
            || name.eq_ignore_ascii_case("transfer-encoding")
        {
            continue;
        }
        let key = name.to_ascii_lowercase();
        let index = seen.entry(key.clone()).or_insert(0);
        *index += 1;
        let appearances = counts.get(&key).copied().unwrap_or(1);
        if appearances == 1 || *index == 1 {
            if let Some(updated) = lookup(finished, name) {
                out.push((name.clone(), updated));
                continue;
            }
        } else if let (Some(original), Some(updated)) =
            (lookup(before, name), lookup(finished, name))
        {
            if value == &original && original != updated {
                out.push((name.clone(), updated));
                continue;
            }
        }
        out.push((name.clone(), value.clone()));
    }
    for (name, value) in finished {
        if counts.keys().all(|key| !key.eq_ignore_ascii_case(name)) && !super::is_hop_by_hop(name) {
            out.push((name.clone(), value.clone()));
        }
    }
    out
}

fn lookup(headers: &HashMap<String, String>, name: &str) -> Option<String> {
    headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.clone())
}

fn declared_length(headers: &hyper::HeaderMap) -> Result<Option<usize>, ()> {
    let mut found = None;
    for value in headers.get_all(hyper::header::CONTENT_LENGTH) {
        let text = std::str::from_utf8(value.as_bytes()).map_err(|_| ())?;
        let len = text.trim().parse::<usize>().map_err(|_| ())?;
        if let Some(prev) = found {
            if prev != len {
                return Err(());
            }
        }
        found = Some(len);
    }
    Ok(found)
}

fn content_length_header(headers: &hyper::HeaderMap) -> Option<usize> {
    declared_length(headers).ok().flatten()
}

fn request_path(uri: &hyper::Uri) -> String {
    match uri.path_and_query().map(|pq| pq.as_str()) {
        Some(pq) if pq.starts_with('/') => pq.to_string(),
        Some(pq) if pq.starts_with('?') => format!("/{pq}"),
        Some(pq) if !pq.is_empty() => format!("/{pq}"),
        _ => "/".to_string(),
    }
}

fn path_from_url(url: &str) -> Option<String> {
    let uri: hyper::Uri = url.parse().ok()?;
    Some(request_path(&uri))
}

fn https_url(host: &str, port: u16, path: &str) -> String {
    let path = if path.is_empty() { "/" } else { path };
    format!("https://{}{path}", authority(host, port))
}

fn origin_uri(host: &str, port: u16, path: &str) -> Result<hyper::Uri, ()> {
    format!("https://{}{path}", authority(host, port))
        .parse()
        .map_err(|_| ())
}

fn authority(host: &str, port: u16) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

fn host_value(host: &str, port: u16) -> String {
    if port == 443 {
        if host.contains(':') && !host.starts_with('[') {
            format!("[{host}]")
        } else {
            host.to_string()
        }
    } else {
        authority(host, port)
    }
}

pub(super) fn same_host(left: &str, right: &str) -> bool {
    normalize_host(left) == normalize_host(right)
}

pub(super) fn host_header_matches(connect_host: &str, connect_port: u16, header: &str) -> bool {
    let Some((host, port)) = split_host_header(header) else {
        return false;
    };
    if let Some(port) = port {
        if port != connect_port {
            return false;
        }
    }
    same_host(connect_host, host)
}

fn normalize_host(host: &str) -> String {
    let trimmed = host.trim().trim_end_matches('.');
    let inner = trimmed
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or(trimmed);
    inner.to_ascii_lowercase()
}

fn split_host_header(header: &str) -> Option<(&str, Option<u16>)> {
    let header = header.trim();
    if header.is_empty() {
        return None;
    }
    if let Some(rest) = header.strip_prefix('[') {
        let end = rest.find(']')?;
        let host = &rest[..end];
        let after = &rest[end + 1..];
        if after.is_empty() {
            return Some((host, None));
        }
        let port = after.strip_prefix(':')?.parse().ok()?;
        return Some((host, Some(port)));
    }
    if let Some((host, port_text)) = header.rsplit_once(':') {
        if !host.is_empty() && !host.contains(':') {
            if let Ok(port) = port_text.parse::<u16>() {
                return Some((host, Some(port)));
            }
        }
    }
    Some((header, None))
}

/// Bytes written back to the guest. Buffered responses are one chunk.
/// Oversized responses yield a prefix, then the rest of the upstream body.
struct GuestBody {
    inner: GuestInner,
}

enum GuestInner {
    Full(Option<Bytes>),
    Stream {
        pending: Option<Bytes>,
        incoming: Incoming,
        counted: usize,
        max: usize,
        gate: Arc<dyn EgressGate>,
        idle: Pin<Box<tokio::time::Sleep>>,
    },
}

impl GuestBody {
    fn full(bytes: Bytes) -> Self {
        Self {
            inner: GuestInner::Full(Some(bytes)),
        }
    }

    fn stream(prefix: Bytes, incoming: Incoming, max: usize, gate: Arc<dyn EgressGate>) -> Self {
        let counted = prefix.len();
        let pending = if prefix.is_empty() {
            None
        } else {
            Some(prefix)
        };
        Self {
            inner: GuestInner::Stream {
                pending,
                incoming,
                counted,
                max,
                gate,
                idle: Box::pin(tokio::time::sleep(HEADER_IDLE)),
            },
        }
    }
}

impl Body for GuestBody {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();
        match &mut this.inner {
            GuestInner::Full(slot) => match slot.take() {
                Some(bytes) if !bytes.is_empty() => Poll::Ready(Some(Ok(Frame::data(bytes)))),
                _ => Poll::Ready(None),
            },
            GuestInner::Stream {
                pending,
                incoming,
                counted,
                max,
                gate,
                idle,
            } => {
                if let Some(bytes) = pending.take() {
                    idle.as_mut()
                        .reset(tokio::time::Instant::now() + HEADER_IDLE);
                    return Poll::Ready(Some(Ok(Frame::data(bytes))));
                }
                match Pin::new(incoming).poll_frame(cx) {
                    Poll::Pending => match idle.as_mut().poll(cx) {
                        Poll::Ready(()) => Poll::Ready(Some(Err(std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            "upstream response idle timeout",
                        )))),
                        Poll::Pending => Poll::Pending,
                    },
                    Poll::Ready(None) => Poll::Ready(None),
                    Poll::Ready(Some(Err(err))) => {
                        Poll::Ready(Some(Err(std::io::Error::other(err.to_string()))))
                    }
                    Poll::Ready(Some(Ok(frame))) => {
                        idle.as_mut()
                            .reset(tokio::time::Instant::now() + HEADER_IDLE);
                        match frame.into_data() {
                            Ok(data) => {
                                if !data.is_empty() {
                                    *counted = counted.saturating_add(data.len());
                                    if *counted > *max {
                                        return Poll::Ready(Some(Err(std::io::Error::new(
                                            std::io::ErrorKind::InvalidData,
                                            "upstream response too large",
                                        ))));
                                    }
                                    if let Err(err) = gate.record_bytes(data.len() as u64) {
                                        return Poll::Ready(Some(Err(std::io::Error::new(
                                            std::io::ErrorKind::ConnectionAborted,
                                            err.to_string(),
                                        ))));
                                    }
                                }
                                Poll::Ready(Some(Ok(Frame::data(data))))
                            }
                            Err(frame) => Poll::Ready(Some(Ok(frame))),
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_alpn_is_http1_and_h2_is_opt_in() {
        assert_eq!(alpn_protocols(false), vec![b"http/1.1".to_vec()]);
        assert_eq!(
            alpn_protocols(true),
            vec![b"h2".to_vec(), b"http/1.1".to_vec()]
        );
        assert!(!TerminateConfig::default().offer_h2);
        assert_eq!(
            TerminateConfig::default().limits.scrub_buffer,
            DEFAULT_SCRUB_BUFFER
        );
    }

    #[test]
    fn host_header_must_match_the_connect_target() {
        assert!(host_header_matches(
            "api.example.test",
            443,
            "API.Example.Test"
        ));
        assert!(host_header_matches(
            "api.example.test",
            443,
            "api.example.test:443"
        ));
        assert!(host_header_matches(
            "api.example.test.",
            443,
            "api.example.test"
        ));
        assert!(!host_header_matches(
            "api.example.test",
            443,
            "evil.example.test"
        ));
        assert!(!host_header_matches(
            "api.example.test",
            443,
            "api.example.test:8443"
        ));
        assert!(host_header_matches("2001:db8::1", 443, "[2001:db8::1]"));
        assert!(host_header_matches(
            "2001:db8::1",
            8443,
            "[2001:db8::1]:8443"
        ));
        assert!(!host_header_matches("2001:db8::1", 443, "[2001:db8::2]"));
    }

    #[test]
    fn repeated_headers_are_not_comma_joined() {
        let lines = vec![
            ("x-trace".to_string(), "one".to_string()),
            ("x-trace".to_string(), "two".to_string()),
            (
                "authorization".to_string(),
                "Bearer placeholder".to_string(),
            ),
        ];
        let mut prepared = first_header_values(&lines);
        assert_eq!(prepared.get("x-trace").map(String::as_str), Some("one"));
        if let Some(value) = prepared.get_mut("authorization") {
            *value = "Bearer secret".to_string();
        }
        let merged = merge_request_headers(lines, &prepared);
        let traces: Vec<_> = merged
            .iter()
            .filter(|(name, _)| name == "x-trace")
            .map(|(_, value)| value.as_str())
            .collect();
        assert_eq!(traces, ["one", "two"]);
        let auth = merged
            .iter()
            .find(|(name, _)| name == "authorization")
            .map(|(_, value)| value.as_str());
        assert_eq!(auth, Some("Bearer secret"));
    }
}
