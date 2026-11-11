//! E2E egress fixture: a fake HTTPS origin for the terminating proxy.
//!
//! Compiled only with the `process-guest` feature (test nodes), the way the host
//! tool fixtures are: a default `vamd` contains none of this. The origin serves
//! the name [`FIXTURE_HOST`] from a loopback port, with a certificate from a CA
//! made for this process. [`EgressFixture::upstream`] is the proxy's origin
//! client that trusts that CA.
//!
//! `GET /echo?size=N` answers with the secret it was sent in `Authorization`
//! (or, when the request carries none, the last one it saw), repeated to `N`
//! bytes, and again in the `x-echoed-auth` header. It is what a hostile or
//! careless upstream does: it hands an injected secret back. The proxy's
//! scrubbing is what the E2E scenario judges.

use std::convert::Infallible;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use http_body_util::Full;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, IsCa, KeyPair, KeyUsagePurpose,
    PKCS_ECDSA_P256_SHA256,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

use crate::error::VamNetworkError;
use crate::upstream::UpstreamClient;

/// The name the fixture answers to. Guests `CONNECT` to it on port 443.
pub const FIXTURE_HOST: &str = "oversize.e2e.test";
/// Scrub buffer the proxy uses against the fixture, so a body that is over it
/// is 64 KiB, not 8 MiB.
pub const FIXTURE_SCRUB_BUFFER: usize = 64 * 1024;

/// A running fixture origin and the client that trusts it.
pub struct EgressFixture {
    addr: SocketAddr,
    upstream: Arc<UpstreamClient>,
}

impl EgressFixture {
    /// Binds a loopback port and serves until the process ends.
    pub async fn start() -> Result<Arc<Self>, VamNetworkError> {
        let (ca_pem, acceptor) = mint()?;
        let upstream = UpstreamClient::new(Some(&ca_pem))
            .map_err(|err| VamNetworkError::Internal(format!("fixture upstream client: {err}")))?;
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .map_err(|err| VamNetworkError::Internal(format!("fixture bind: {err}")))?;
        let addr = listener
            .local_addr()
            .map_err(|err| VamNetworkError::Internal(format!("fixture addr: {err}")))?;
        let last_auth = Arc::new(Mutex::new(String::new()));
        tokio::spawn(async move {
            loop {
                let Ok((sock, _)) = listener.accept().await else {
                    break;
                };
                let acceptor = acceptor.clone();
                let last_auth = Arc::clone(&last_auth);
                tokio::spawn(async move {
                    let Ok(tls) = acceptor.accept(sock).await else {
                        return;
                    };
                    let service = service_fn(move |request| {
                        let last_auth = Arc::clone(&last_auth);
                        async move { Ok::<_, Infallible>(answer(&request, &last_auth)) }
                    });
                    let _ = hyper::server::conn::http1::Builder::new()
                        .keep_alive(true)
                        .serve_connection(TokioIo::new(tls), service)
                        .await;
                });
            }
        });
        Ok(Arc::new(Self {
            addr,
            upstream: Arc::new(upstream),
        }))
    }

    /// Where the proxy dials for [`FIXTURE_HOST`].
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The proxy's origin client, trusting the fixture's CA.
    pub fn upstream(&self) -> Arc<UpstreamClient> {
        Arc::clone(&self.upstream)
    }
}

fn answer(
    request: &Request<hyper::body::Incoming>,
    last_auth: &Mutex<String>,
) -> Response<Full<Bytes>> {
    let sent = request
        .headers()
        .get(hyper::header::AUTHORIZATION)
        .map(|value| String::from_utf8_lossy(value.as_bytes()).into_owned());
    let echoed = {
        let mut slot = last_auth
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(sent) = sent {
            *slot = sent;
        }
        slot.clone()
    };
    let size = request
        .uri()
        .query()
        .and_then(|query| {
            query
                .split('&')
                .find_map(|pair| pair.strip_prefix("size="))
                .and_then(|value| value.parse::<usize>().ok())
        })
        .unwrap_or(64)
        .min(4 * 1024 * 1024);
    let mut body = Vec::with_capacity(size);
    if !echoed.is_empty() {
        while body.len() < size {
            body.extend_from_slice(echoed.as_bytes());
            body.push(b'\n');
        }
        body.truncate(size);
    } else {
        body.resize(size, b'.');
    }
    let mut builder = Response::builder().status(StatusCode::OK);
    if let Ok(value) = hyper::header::HeaderValue::from_str(&echoed) {
        builder = builder.header("x-echoed-auth", value);
    }
    builder
        .body(Full::new(Bytes::from(body)))
        .unwrap_or_else(|_| Response::new(Full::new(Bytes::new())))
}

/// A throwaway CA and a leaf for [`FIXTURE_HOST`] signed by it.
fn mint() -> Result<(String, TlsAcceptor), VamNetworkError> {
    let fail = |what: &str, err: &dyn std::fmt::Display| {
        VamNetworkError::Internal(format!("fixture {what}: {err}"))
    };
    let now = time::OffsetDateTime::now_utc();
    let ca_key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).map_err(|e| fail("ca key", &e))?;
    let mut ca_params = CertificateParams::default();
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, "vam e2e fixture CA");
    ca_params.distinguished_name = dn;
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    ca_params.not_before = now - time::Duration::hours(1);
    ca_params.not_after = now + time::Duration::days(2);
    let ca_cert = ca_params
        .self_signed(&ca_key)
        .map_err(|e| fail("ca cert", &e))?;

    let leaf_key =
        KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).map_err(|e| fail("leaf key", &e))?;
    let mut leaf_params = CertificateParams::new(vec![FIXTURE_HOST.to_string()])
        .map_err(|e| fail("leaf params", &e))?;
    leaf_params.is_ca = IsCa::ExplicitNoCa;
    leaf_params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    leaf_params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
    leaf_params.not_before = now - time::Duration::hours(1);
    leaf_params.not_after = now + time::Duration::days(2);
    let leaf = leaf_params
        .signed_by(&leaf_key, &ca_cert, &ca_key)
        .map_err(|e| fail("leaf cert", &e))?;

    let cert = CertificateDer::from(leaf.der().to_vec());
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(leaf_key.serialize_der()));
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let mut server = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| fail("tls versions", &e))?
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)
        .map_err(|e| fail("origin cert", &e))?;
    server.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok((ca_cert.pem(), TlsAcceptor::from(Arc::new(server))))
}

/// What a guest sees when it asks the fixture origin through the proxy's port-55 socket.
pub struct Port55Reply {
    /// Status code of the response the proxy returned, or 0 when there was none.
    pub status: u16,
    /// Everything read back after the tunnel opened: the proxy's status line, headers and body.
    pub raw: Vec<u8>,
}

/// Plays the guest: opens the session's port-55 socket, `CONNECT`s to [`FIXTURE_HOST`], speaks TLS
/// to the proxy (any certificate is accepted; the scenario judges the proxy's scrubbing, not the
/// session CA) and asks `GET /echo?size=N`, with `authorization` as the `Authorization` header
/// when given. Blocking, with a 10 s timeout per read.
pub fn probe_port55(
    socket: &std::path::Path,
    size: usize,
    authorization: Option<&str>,
) -> Result<Port55Reply, String> {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    let timeout = Some(Duration::from_secs(10));
    let mut stream = UnixStream::connect(socket).map_err(|e| format!("connect {socket:?}: {e}"))?;
    stream
        .set_read_timeout(timeout)
        .map_err(|e| e.to_string())?;
    stream
        .set_write_timeout(timeout)
        .map_err(|e| e.to_string())?;
    stream
        .write_all(
            format!("CONNECT {FIXTURE_HOST}:443 HTTP/1.1\r\nHost: {FIXTURE_HOST}:443\r\n\r\n")
                .as_bytes(),
        )
        .map_err(|e| format!("CONNECT write: {e}"))?;
    // One byte at a time, so no TLS byte is read past the proxy's head.
    let mut head = Vec::new();
    let mut one = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        match stream.read(&mut one) {
            Ok(0) => break,
            Ok(_) => head.push(one[0]),
            Err(e) => return Err(format!("CONNECT read: {e}")),
        }
    }
    if !head.starts_with(b"HTTP/1.1 200") {
        return Err(format!(
            "CONNECT answered: {}",
            String::from_utf8_lossy(&head).lines().next().unwrap_or("")
        ));
    }

    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(Arc::clone(&provider))
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAny(provider)))
        .with_no_client_auth();
    let name = rustls::pki_types::ServerName::try_from(FIXTURE_HOST).map_err(|e| e.to_string())?;
    let conn = rustls::ClientConnection::new(Arc::new(config), name).map_err(|e| e.to_string())?;
    let mut tls = rustls::StreamOwned::new(conn, stream);
    let auth = authorization
        .map(|value| format!("Authorization: {value}\r\n"))
        .unwrap_or_default();
    tls.write_all(
        format!(
            "GET /echo?size={size} HTTP/1.1\r\nHost: {FIXTURE_HOST}\r\n{auth}Connection: close\r\n\r\n"
        )
        .as_bytes(),
    )
    .map_err(|e| format!("request write: {e}"))?;
    let mut raw = Vec::new();
    let mut chunk = [0u8; 16 * 1024];
    loop {
        match tls.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => raw.extend_from_slice(&chunk[..n]),
            // A close without close_notify, or a timeout, ends the read; what arrived is the reply.
            Err(_) => break,
        }
    }
    let status = raw
        .strip_prefix(b"HTTP/1.1 ")
        .and_then(|rest| rest.get(..3))
        .and_then(|code| std::str::from_utf8(code).ok())
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    Ok(Port55Reply { status, raw })
}

#[derive(Debug)]
struct AcceptAny(Arc<rustls::crypto::CryptoProvider>);

impl rustls::client::danger::ServerCertVerifier for AcceptAny {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}
