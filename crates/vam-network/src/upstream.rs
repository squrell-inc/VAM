//! Verified HTTPS client for the TLS-terminating egress proxy.
//!
//! Each origin connection is a fresh TLS session. The certificate chain is
//! checked against the webpki roots, the platform trust store, and optional
//! extra PEM roots (the org CA hook). The hostname check uses the `CONNECT`
//! host, while the TCP dial uses the gate's pinned [`SocketAddr`]. Redirects
//! are not followed. Idle connections are pooled per `(host, pinned address)`.

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use http_body_util::Full;
use hyper::body::Incoming;
use hyper_util::client::legacy::connect::{Connected, Connection};
use hyper_util::rt::{TokioExecutor, TokioIo};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName};
use rustls::ClientConfig;
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tower_service::Service;

use crate::error::VamNetworkError;

const POOL_IDLE: Duration = Duration::from_secs(90);
const POOL_MAX_IDLE_PER_HOST: usize = 8;

/// Why an origin request did not produce an HTTP response.
#[derive(Debug)]
pub struct UpstreamError {
    kind: UpstreamFailure,
    source: Box<dyn std::error::Error + Send + Sync>,
}

#[derive(Debug)]
enum UpstreamFailure {
    Connect,
    Tls(&'static str),
    Protocol,
}

impl UpstreamError {
    /// Short reason safe to return to the guest. TLS failures use one of
    /// `certificate expired`, `unknown issuer`, `hostname mismatch`, or
    /// `upstream tls failed`.
    pub fn guest_reason(&self) -> &'static str {
        match self.kind {
            UpstreamFailure::Connect => "upstream connection failed",
            UpstreamFailure::Tls(reason) => reason,
            UpstreamFailure::Protocol => "upstream request failed",
        }
    }

    pub fn is_tls(&self) -> bool {
        matches!(self.kind, UpstreamFailure::Tls(_))
    }
}

impl fmt::Display for UpstreamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.guest_reason(), self.source)
    }
}

impl std::error::Error for UpstreamError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}

/// Outbound HTTPS client used by the terminating proxy.
///
/// `extra_roots_pem` is a PEM bundle of additional trust anchors. It is how an
/// org CA is added later; tests pass an rcgen CA the same way.
pub struct UpstreamClient {
    tls: Arc<ClientConfig>,
    pools: Mutex<HashMap<PoolKey, PooledClient>>,
}

type PooledClient = hyper_util::client::legacy::Client<PinnedTlsConnector, Full<bytes::Bytes>>;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct PoolKey {
    host: String,
    addr: SocketAddr,
}

impl UpstreamClient {
    /// Build a client. `extra_roots_pem` appends PEM certificates to the trust store.
    pub fn new(extra_roots_pem: Option<&str>) -> Result<Self, VamNetworkError> {
        let tls = Arc::new(client_config(extra_roots_pem)?);
        Ok(Self {
            tls,
            pools: Mutex::new(HashMap::new()),
        })
    }

    /// Send one HTTP/1.1 request to `addr`, verifying TLS against `host`.
    ///
    /// The request URI is not used to choose the socket. Redirects are left
    /// as ordinary responses for the caller to return.
    pub async fn send(
        &self,
        host: &str,
        addr: SocketAddr,
        request: hyper::Request<Full<bytes::Bytes>>,
    ) -> Result<hyper::Response<Incoming>, UpstreamError> {
        let client = self.client_for(host, addr).map_err(|err| UpstreamError {
            kind: UpstreamFailure::Protocol,
            source: Box::new(err),
        })?;
        match client.request(request).await {
            Ok(response) => Ok(response),
            Err(err) => Err(map_client_error(err)),
        }
    }

    fn client_for(&self, host: &str, addr: SocketAddr) -> Result<PooledClient, VamNetworkError> {
        let (pool_host, server_name) = server_name(host)?;
        let key = PoolKey {
            host: pool_host,
            addr,
        };
        let mut pools = match self.pools.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(client) = pools.get(&key) {
            return Ok(client.clone());
        }
        let connector = PinnedTlsConnector {
            addr,
            server_name,
            tls: Arc::clone(&self.tls),
        };
        let client = hyper_util::client::legacy::Client::builder(TokioExecutor::new())
            .pool_idle_timeout(POOL_IDLE)
            .pool_max_idle_per_host(POOL_MAX_IDLE_PER_HOST)
            .set_host(false)
            .build(connector);
        pools.insert(key, client.clone());
        Ok(client)
    }
}

fn client_config(extra_roots_pem: Option<&str>) -> Result<ClientConfig, VamNetworkError> {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

    let native = rustls_native_certs::load_native_certs();
    for err in &native.errors {
        tracing::debug!(error = %err, "skipped a platform trust anchor");
    }
    for cert in native.certs {
        if let Err(err) = roots.add(cert) {
            tracing::debug!(error = %err, "platform trust anchor was not added");
        }
    }

    if let Some(pem) = extra_roots_pem.map(str::trim).filter(|pem| !pem.is_empty()) {
        let mut added = 0usize;
        for item in CertificateDer::pem_slice_iter(pem.as_bytes()) {
            let cert = item.map_err(|err| {
                VamNetworkError::Internal(format!("upstream trust root PEM: {err}"))
            })?;
            roots.add(cert).map_err(|err| {
                VamNetworkError::Internal(format!("upstream trust root rejected: {err}"))
            })?;
            added += 1;
        }
        if added == 0 {
            return Err(VamNetworkError::Internal(
                "upstream trust root PEM contained no certificates".to_string(),
            ));
        }
    }

    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let mut config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|err| VamNetworkError::Internal(format!("upstream tls versions: {err}")))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    // The origin speaks the same HTTP version as the guest-facing terminator.
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(config)
}

fn server_name(host: &str) -> Result<(String, ServerName<'static>), VamNetworkError> {
    let trimmed = host.trim().trim_end_matches('.');
    let name = if let Some(inner) = trimmed.strip_prefix('[') {
        inner.strip_suffix(']').unwrap_or(inner)
    } else {
        trimmed
    };
    if name.is_empty() {
        return Err(VamNetworkError::Internal(
            "upstream host is empty".to_string(),
        ));
    }
    let owned = name.to_ascii_lowercase();
    let server_name = ServerName::try_from(owned.clone()).map_err(|_| {
        VamNetworkError::Internal("upstream host is not a DNS name or IP address".to_string())
    })?;
    Ok((owned, server_name))
}

fn map_client_error(err: hyper_util::client::legacy::Error) -> UpstreamError {
    let kind = if let Some(reason) = tls_failure_reason(&err) {
        UpstreamFailure::Tls(reason)
    } else if err.is_connect() {
        UpstreamFailure::Connect
    } else {
        UpstreamFailure::Protocol
    };
    UpstreamError {
        kind,
        source: Box::new(err),
    }
}

/// Walk `err` for a rustls certificate failure and map it to a guest-facing reason.
pub(crate) fn tls_failure_reason(err: &(dyn std::error::Error + 'static)) -> Option<&'static str> {
    let mut current = Some(err);
    let mut hops = 0;
    while let Some(err) = current {
        if hops > 8 {
            break;
        }
        hops += 1;
        if let Some(reason) = rustls_reason(err) {
            return Some(reason);
        }
        if let Some(io_err) = err.downcast_ref::<std::io::Error>() {
            if let Some(inner) = io_err.get_ref() {
                if let Some(reason) = rustls_reason(inner) {
                    return Some(reason);
                }
            }
        }
        current = err.source();
    }
    None
}

fn rustls_reason(err: &(dyn std::error::Error + 'static)) -> Option<&'static str> {
    if let Some(tls) = err.downcast_ref::<rustls::Error>() {
        return Some(classify_rustls(tls));
    }
    let text = err.to_string().to_ascii_lowercase();
    if text.contains("not valid for name") || text.contains("not valid for the name") {
        Some("hostname mismatch")
    } else if text.contains("unknown issuer") || text.contains("unknownissuer") {
        Some("unknown issuer")
    } else if text.contains("certificate expired") || text.contains("cert expired") {
        Some("certificate expired")
    } else {
        None
    }
}

fn classify_rustls(err: &rustls::Error) -> &'static str {
    match err {
        rustls::Error::InvalidCertificate(cert) => classify_cert(cert),
        other => {
            let text = other.to_string().to_ascii_lowercase();
            if text.contains("not valid for") {
                "hostname mismatch"
            } else if text.contains("unknown issuer") {
                "unknown issuer"
            } else if text.contains("expired") {
                "certificate expired"
            } else {
                "upstream tls failed"
            }
        }
    }
}

fn classify_cert(err: &rustls::CertificateError) -> &'static str {
    match err {
        rustls::CertificateError::Expired => "certificate expired",
        rustls::CertificateError::UnknownIssuer => "unknown issuer",
        rustls::CertificateError::NotValidForName => "hostname mismatch",
        other => {
            let text = other.to_string().to_ascii_lowercase();
            if text.contains("not valid for") || text.contains("hostname") {
                "hostname mismatch"
            } else if text.contains("expired") {
                "certificate expired"
            } else if text.contains("unknown issuer") || text.contains("issuer") {
                "unknown issuer"
            } else {
                "upstream tls failed"
            }
        }
    }
}

/// Dials one pinned address and verifies the certificate for a fixed name.
///
/// hyper pools by the request URI. One [`UpstreamClient`] cache entry holds
/// the client for a single `(host, addr)` pair, so an idle connection is never
/// reused against a different pin.
#[derive(Clone)]
struct PinnedTlsConnector {
    addr: SocketAddr,
    server_name: ServerName<'static>,
    tls: Arc<ClientConfig>,
}

impl fmt::Debug for PinnedTlsConnector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PinnedTlsConnector")
            .field("addr", &self.addr)
            .field("server_name", &self.server_name)
            .finish()
    }
}

impl Service<hyper::Uri> for PinnedTlsConnector {
    type Response = VerifiedTls;
    type Error = std::io::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _dst: hyper::Uri) -> Self::Future {
        let addr = self.addr;
        let server_name = self.server_name.clone();
        let tls = Arc::clone(&self.tls);
        Box::pin(async move {
            let stream = TcpStream::connect(addr).await?;
            let _ = stream.set_nodelay(true);
            let connector = TlsConnector::from(tls);
            let tls = connector.connect(server_name, stream).await?;
            Ok(VerifiedTls {
                io: TokioIo::new(tls),
            })
        })
    }
}

/// TLS stream that hyper's pool can reuse. ALPN is HTTP/1.1, so the
/// connection metadata stays on the HTTP/1 path.
struct VerifiedTls {
    io: TokioIo<tokio_rustls::client::TlsStream<TcpStream>>,
}

impl hyper::rt::Read for VerifiedTls {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: hyper::rt::ReadBufCursor<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        Pin::new(&mut self.get_mut().io).poll_read(cx, buf)
    }
}

impl hyper::rt::Write for VerifiedTls {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        Pin::new(&mut self.get_mut().io).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), std::io::Error>> {
        Pin::new(&mut self.get_mut().io).poll_flush(cx)
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        Pin::new(&mut self.get_mut().io).poll_shutdown(cx)
    }
}

impl Connection for VerifiedTls {
    fn connected(&self) -> Connected {
        Connected::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_certificate_failures() {
        assert_eq!(
            classify_rustls(&rustls::Error::InvalidCertificate(
                rustls::CertificateError::Expired
            )),
            "certificate expired"
        );
        assert_eq!(
            classify_rustls(&rustls::Error::InvalidCertificate(
                rustls::CertificateError::UnknownIssuer
            )),
            "unknown issuer"
        );
        assert_eq!(
            classify_rustls(&rustls::Error::InvalidCertificate(
                rustls::CertificateError::NotValidForName
            )),
            "hostname mismatch"
        );
    }

    #[test]
    fn empty_extra_roots_build_and_garbage_pem_is_rejected() {
        UpstreamClient::new(None).expect("webpki and platform roots");
        UpstreamClient::new(Some("   ")).expect("blank pem is ignored");
        let err = match UpstreamClient::new(Some("not a certificate")) {
            Err(err) => err,
            Ok(_) => panic!("garbage pem was accepted"),
        };
        let message = err.to_string();
        assert!(
            message.contains("PEM") || message.contains("certificate"),
            "{message}"
        );
    }
}
