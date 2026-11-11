//! TLS for the vamd API (IT-1.6).
//!
//! The API key travels on this connection, so vamd serves TLS by default:
//!
//! - an operator certificate from `VAMD_TLS_CERT` / `VAMD_TLS_KEY`, used as given; or
//! - a self-signed certificate minted on first start and kept in `<state dir>/tls/`
//!   (`ca.pem` is the certificate clients trust, `server.key` the key at 0600,
//!   `meta.json` the SAN set and expiry). It is reused across restarts.
//!
//! `--insecure-dev` serves plain HTTP instead (loud warning; the node reports TLS off).
//! Every failure to set up TLS stops vamd: it never falls back to plain HTTP and
//! never replaces stored material it cannot read.
//!
//! The key is never logged; the SHA-256 fingerprint and the trust-file path are.

use std::collections::BTreeSet;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::net::{IpAddr, SocketAddr};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum_server::tls_rustls::RustlsConfig;
use rcgen::{
    CertificateParams, DistinguishedName, DnType, Ia5String, IsCa, KeyPair, KeyUsagePurpose,
    SanType, PKCS_ECDSA_P256_SHA256,
};
use rustls::client::{ServerCertVerified, ServerCertVerifier};
use rustls::{Certificate, ClientConfig, ClientConnection, Connection, PrivateKey, ServerConfig};
use rustls::{ServerConnection, ServerName};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::OrchestratorError;

/// Subdirectory of the state dir that holds the generated certificate.
pub const TLS_DIR: &str = "tls";
/// The certificate clients trust (public).
pub const CA_FILE: &str = "ca.pem";
const KEY_FILE: &str = "server.key";
const META_FILE: &str = "meta.json";

/// A generated certificate is valid for two years.
pub const VALIDITY_DAYS: i64 = 730;
/// A generated certificate with less than this left is replaced at start.
pub const RENEW_BEFORE_DAYS: i64 = 30;
/// Clock skew allowed for a client whose clock is slightly behind.
const SKEW_SECS: i64 = 300;
const DAY_SECS: i64 = 86_400;

/// Why vamd cannot serve the API as configured. Every variant stops vamd.
#[derive(Debug, thiserror::Error)]
pub enum TlsError {
    #[error(
        "{set} is set but {missing} is not: VAMD_TLS_CERT and VAMD_TLS_KEY must be set together"
    )]
    HalfSet {
        set: &'static str,
        missing: &'static str,
    },
    #[error("--insecure-dev (VAMD_INSECURE_DEV) serves plain HTTP, but VAMD_TLS_CERT / VAMD_TLS_KEY are set: unset one side")]
    Conflict,
    #[error("cannot read {what} {path}: {source}")]
    Read {
        what: &'static str,
        path: PathBuf,
        source: io::Error,
    },
    #[error("{what} {path} holds no {expected}")]
    NoItem {
        what: &'static str,
        path: PathBuf,
        expected: &'static str,
    },
    #[error("the TLS certificate and key do not work together: {0}")]
    Mismatch(String),
    #[error("invalid VAMD_TLS_SANS entry '{0}': use a DNS name or an IP address")]
    BadSan(String),
    #[error("the stored node certificate in {dir} is unusable ({reason}); delete that directory to mint a new one")]
    Stored { dir: PathBuf, reason: String },
    #[error("cannot create the node certificate: {0}")]
    Generate(String),
    #[error("cannot write {path}: {source}")]
    Write { path: PathBuf, source: io::Error },
}

/// What `vamd` decided from its config.
pub struct TlsSettings<'a> {
    pub cert: Option<&'a Path>,
    pub key: Option<&'a Path>,
    pub insecure_dev: bool,
    /// `VAMD_TLS_SANS`: extra DNS names or IPs for the self-signed certificate.
    pub extra_sans: &'a [String],
    pub listen: SocketAddr,
    /// The state dir (the parent of the database); the certificate lives in `<dir>/tls/`.
    pub state_dir: &'a Path,
}

/// How the API is served.
pub enum Transport {
    /// Plain HTTP (`--insecure-dev` only).
    Plain,
    Tls(TlsServing),
}

impl Transport {
    /// True when the API is served over TLS. This is what `GET /v1/node` reports.
    pub fn is_tls(&self) -> bool {
        matches!(self, Transport::Tls(_))
    }

    /// Logs the transport: the source, the fingerprint and the trust-file path, never the key.
    pub fn log(&self, listen: SocketAddr) {
        match self {
            Transport::Plain => tracing::warn!(
                "INSECURE DEV MODE: the API on {listen} is served over plain HTTP; the API key travels unencrypted. Dev hosts only; the node reports TLS off"
            ),
            Transport::Tls(t) => {
                for note in &t.notes {
                    tracing::warn!("{note}");
                }
                match &t.source {
                    CertSource::Operator { cert } => tracing::info!(
                        fingerprint_sha256 = %t.fingerprint,
                        "API TLS: operator certificate {}",
                        cert.display()
                    ),
                    CertSource::SelfSigned {
                        regenerated,
                        created,
                        ca_path,
                    } => {
                        let trust = format!(
                            "trust it on clients with `vam config set-ca {}` or VAM_CA_FILE",
                            ca_path.display()
                        );
                        if *regenerated {
                            tracing::warn!(
                                fingerprint_sha256 = %t.fingerprint,
                                "API TLS: the self-signed certificate was REGENERATED (SAN change or under {RENEW_BEFORE_DAYS} days left); clients must trust the new one: {trust}"
                            );
                        } else if *created {
                            tracing::info!(
                                fingerprint_sha256 = %t.fingerprint,
                                "API TLS: generated a self-signed certificate at {}; {trust}",
                                ca_path.display()
                            );
                        } else {
                            tracing::info!(
                                fingerprint_sha256 = %t.fingerprint,
                                "API TLS: reusing the self-signed certificate at {}; {trust}",
                                ca_path.display()
                            );
                        }
                        if listen.ip().is_unspecified() {
                            tracing::info!(
                                "API TLS: listening on all interfaces; for remote clients set VAMD_TLS_SANS=<host name or IP> or use an operator certificate"
                            );
                        }
                    }
                }
            }
        }
    }
}

/// The certificate, its fingerprint and where it came from.
pub struct TlsServing {
    pub config: RustlsConfig,
    /// SHA-256 of the leaf certificate (DER), as `AB:CD:…`.
    pub fingerprint: String,
    pub source: CertSource,
    /// Warnings to log once tracing is up (for example a key file that was too open).
    pub notes: Vec<String>,
}

impl fmt::Debug for TlsServing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TlsServing")
            .field("fingerprint", &self.fingerprint)
            .field("source", &self.source)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone)]
pub enum CertSource {
    Operator {
        cert: PathBuf,
    },
    SelfSigned {
        /// Replaced a stored certificate (SAN change, or close to expiry).
        regenerated: bool,
        /// Minted just now (first start or regenerated).
        created: bool,
        ca_path: PathBuf,
    },
}

#[derive(Serialize, Deserialize)]
struct Meta {
    sans: Vec<String>,
    not_after: i64,
}

/// Decides how the API is served. Call before any state is opened.
pub fn resolve(s: &TlsSettings<'_>) -> Result<Transport, TlsError> {
    resolve_at(s, unix_now())
}

/// [`resolve`] with an explicit clock (for tests).
pub fn resolve_at(s: &TlsSettings<'_>, now: i64) -> Result<Transport, TlsError> {
    match (s.cert, s.key) {
        (Some(_), Some(_)) if s.insecure_dev => return Err(TlsError::Conflict),
        (Some(_), None) | (None, Some(_)) if s.insecure_dev => return Err(TlsError::Conflict),
        (Some(_), None) => {
            return Err(TlsError::HalfSet {
                set: "VAMD_TLS_CERT",
                missing: "VAMD_TLS_KEY",
            })
        }
        (None, Some(_)) => {
            return Err(TlsError::HalfSet {
                set: "VAMD_TLS_KEY",
                missing: "VAMD_TLS_CERT",
            })
        }
        _ => {}
    }
    if s.insecure_dev {
        return Ok(Transport::Plain);
    }
    match (s.cert, s.key) {
        (Some(cert), Some(key)) => operator(cert, key),
        _ => self_signed(s, now),
    }
}

fn operator(cert: &Path, key: &Path) -> Result<Transport, TlsError> {
    let certs = read_certs(cert, "TLS certificate")?;
    let key_der = read_key(key, "TLS key")?;
    let fingerprint = fingerprint(&certs[0]);
    let config = build_config(certs, key_der)?;
    Ok(Transport::Tls(TlsServing {
        config: RustlsConfig::from_config(config),
        fingerprint,
        source: CertSource::Operator {
            cert: cert.to_path_buf(),
        },
        notes: Vec::new(),
    }))
}

fn self_signed(s: &TlsSettings<'_>, now: i64) -> Result<Transport, TlsError> {
    let dir = s.state_dir.join(TLS_DIR);
    let sans = san_set(s.listen, s.extra_sans)?;
    let ca_path = dir.join(CA_FILE);
    let key_path = dir.join(KEY_FILE);
    let meta_path = dir.join(META_FILE);
    let present = [&ca_path, &key_path, &meta_path]
        .iter()
        .filter(|p| p.exists())
        .count();
    let stored = |reason: String| TlsError::Stored {
        dir: dir.clone(),
        reason,
    };

    let mut notes = Vec::new();
    let mut regenerated = false;
    let mut created = false;
    if present == 0 {
        generate(&dir, &sans, now)?;
        created = true;
    } else if present < 3 {
        return Err(stored(
            "some of ca.pem, server.key and meta.json are missing".into(),
        ));
    } else {
        let raw = fs::read(&meta_path).map_err(|e| stored(format!("meta.json: {e}")))?;
        let meta: Meta =
            serde_json::from_slice(&raw).map_err(|e| stored(format!("meta.json: {e}")))?;
        let renew = meta.not_after - now < RENEW_BEFORE_DAYS * DAY_SECS;
        if meta.sans != sans || renew {
            generate(&dir, &sans, now)?;
            created = true;
            regenerated = true;
        } else {
            let mode = fs::metadata(&key_path)
                .map_err(|e| stored(format!("server.key: {e}")))?
                .permissions()
                .mode();
            if mode & 0o077 != 0 {
                fs::set_permissions(&key_path, fs::Permissions::from_mode(0o600))
                    .map_err(|e| stored(format!("server.key permissions: {e}")))?;
                notes.push(format!(
                    "API TLS: {} was readable by others (mode {:o}); set to 0600",
                    key_path.display(),
                    mode & 0o777
                ));
            }
        }
    }

    let load = || -> Result<(Vec<Vec<u8>>, Vec<u8>), TlsError> {
        Ok((
            read_certs(&ca_path, "node certificate")?,
            read_key(&key_path, "node key")?,
        ))
    };
    let (certs, key_der) = load().map_err(|e| stored(e.to_string()))?;
    let fingerprint = fingerprint(&certs[0]);
    let config = build_config(certs, key_der).map_err(|e| stored(e.to_string()))?;
    Ok(Transport::Tls(TlsServing {
        config: RustlsConfig::from_config(config),
        fingerprint,
        source: CertSource::SelfSigned {
            regenerated,
            created,
            ca_path,
        },
        notes,
    }))
}

/// The names the certificate covers, as sorted `dns:` / `ip:` strings.
fn san_set(listen: SocketAddr, extra: &[String]) -> Result<Vec<String>, TlsError> {
    let mut set: BTreeSet<String> = BTreeSet::new();
    set.insert("dns:localhost".into());
    set.insert("ip:127.0.0.1".into());
    set.insert("ip:::1".into());
    if !listen.ip().is_unspecified() {
        set.insert(format!("ip:{}", listen.ip()));
    }
    for raw in extra {
        let e = raw.trim();
        if let Ok(ip) = e.parse::<IpAddr>() {
            set.insert(format!("ip:{ip}"));
        } else if !e.is_empty()
            && Ia5String::try_from(e).is_ok()
            && !e.contains(char::is_whitespace)
        {
            set.insert(format!("dns:{}", e.to_ascii_lowercase()));
        } else {
            return Err(TlsError::BadSan(raw.clone()));
        }
    }
    Ok(set.into_iter().collect())
}

fn generate(dir: &Path, sans: &[String], now: i64) -> Result<(), TlsError> {
    let gen = |e: &dyn fmt::Display| TlsError::Generate(e.to_string());
    let mut params = CertificateParams::default();
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, "vamd node");
    params.distinguished_name = dn;
    let mut names = Vec::new();
    for s in sans {
        if let Some(ip) = s.strip_prefix("ip:") {
            names.push(SanType::IpAddress(ip.parse().map_err(|e| gen(&e))?));
        } else if let Some(dns) = s.strip_prefix("dns:") {
            names.push(SanType::DnsName(
                Ia5String::try_from(dns).map_err(|e| gen(&e))?,
            ));
        }
    }
    params.subject_alt_names = names;
    params.is_ca = IsCa::ExplicitNoCa;
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
    let not_after = now + VALIDITY_DAYS * DAY_SECS;
    params.not_before =
        time::OffsetDateTime::from_unix_timestamp(now - SKEW_SECS).map_err(|e| gen(&e))?;
    params.not_after = time::OffsetDateTime::from_unix_timestamp(not_after).map_err(|e| gen(&e))?;
    let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).map_err(|e| gen(&e))?;
    let cert = params.self_signed(&key).map_err(|e| gen(&e))?;
    let meta = serde_json::to_vec_pretty(&Meta {
        sans: sans.to_vec(),
        not_after,
    })
    .map_err(|e| gen(&e))?;

    fs::create_dir_all(dir).map_err(|e| TlsError::Write {
        path: dir.to_path_buf(),
        source: e,
    })?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).map_err(|e| TlsError::Write {
        path: dir.to_path_buf(),
        source: e,
    })?;
    // The key first, with its mode set at creation; the certificate and meta after.
    write_atomic(&dir.join(KEY_FILE), key.serialize_pem().as_bytes(), 0o600)?;
    write_atomic(&dir.join(CA_FILE), cert.pem().as_bytes(), 0o644)?;
    write_atomic(&dir.join(META_FILE), &meta, 0o644)?;
    Ok(())
}

/// Writes `bytes` to a temp file created with `mode`, then renames it over `path`.
fn write_atomic(path: &Path, bytes: &[u8], mode: u32) -> Result<(), TlsError> {
    let werr = |e: io::Error| TlsError::Write {
        path: path.to_path_buf(),
        source: e,
    };
    let tmp = path.with_extension("tmp");
    let _ = fs::remove_file(&tmp);
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(&tmp)
        .map_err(werr)?;
    f.write_all(bytes).map_err(werr)?;
    f.sync_all().map_err(werr)?;
    drop(f);
    fs::rename(&tmp, path).map_err(werr)
}

fn read_certs(path: &Path, what: &'static str) -> Result<Vec<Vec<u8>>, TlsError> {
    let items = read_items(path, what)?;
    let certs: Vec<Vec<u8>> = items
        .into_iter()
        .filter_map(|i| match i {
            rustls_pemfile::Item::X509Certificate(c) => Some(c),
            _ => None,
        })
        .collect();
    if certs.is_empty() {
        return Err(TlsError::NoItem {
            what,
            path: path.to_path_buf(),
            expected: "PEM certificate",
        });
    }
    Ok(certs)
}

fn read_key(path: &Path, what: &'static str) -> Result<Vec<u8>, TlsError> {
    read_items(path, what)?
        .into_iter()
        .find_map(|i| match i {
            rustls_pemfile::Item::PKCS8Key(k)
            | rustls_pemfile::Item::RSAKey(k)
            | rustls_pemfile::Item::ECKey(k) => Some(k),
            _ => None,
        })
        .ok_or_else(|| TlsError::NoItem {
            what,
            path: path.to_path_buf(),
            expected: "PEM private key",
        })
}

fn read_items(path: &Path, what: &'static str) -> Result<Vec<rustls_pemfile::Item>, TlsError> {
    let raw = fs::read(path).map_err(|e| TlsError::Read {
        what,
        path: path.to_path_buf(),
        source: e,
    })?;
    rustls_pemfile::read_all(&mut raw.as_slice()).map_err(|e| TlsError::Read {
        what,
        path: path.to_path_buf(),
        source: e,
    })
}

fn fingerprint(cert_der: &[u8]) -> String {
    Sha256::digest(cert_der)
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

/// The exact server config that will be served, proven by an in-memory handshake
/// (rustls 0.21 does not check that the key matches the certificate).
fn build_config(certs: Vec<Vec<u8>>, key: Vec<u8>) -> Result<Arc<ServerConfig>, TlsError> {
    let mut cfg = ServerConfig::builder()
        .with_safe_defaults()
        .with_no_client_auth()
        .with_single_cert(
            certs.into_iter().map(Certificate).collect(),
            PrivateKey(key),
        )
        .map_err(|e| TlsError::Mismatch(e.to_string()))?;
    cfg.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    let cfg = Arc::new(cfg);
    handshake_check(cfg.clone())?;
    Ok(cfg)
}

struct AcceptAnyName;

impl ServerCertVerifier for AcceptAnyName {
    // The chain and name are not the question here; the handshake signature is
    // (the trait's default `verify_tls1x_signature` checks it against the certificate's key).
    fn verify_server_cert(
        &self,
        _end_entity: &Certificate,
        _intermediates: &[Certificate],
        _server_name: &ServerName,
        _scts: &mut dyn Iterator<Item = &[u8]>,
        _ocsp_response: &[u8],
        _now: std::time::SystemTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }
}

fn handshake_check(server_cfg: Arc<ServerConfig>) -> Result<(), TlsError> {
    let mismatch = |e: &dyn fmt::Display| TlsError::Mismatch(e.to_string());
    let client_cfg = ClientConfig::builder()
        .with_safe_defaults()
        .with_custom_certificate_verifier(Arc::new(AcceptAnyName))
        .with_no_client_auth();
    let name = ServerName::try_from("localhost").map_err(|e| mismatch(&e))?;
    let mut client: Connection = ClientConnection::new(Arc::new(client_cfg), name)
        .map_err(|e| mismatch(&e))?
        .into();
    let mut server: Connection = ServerConnection::new(server_cfg)
        .map_err(|e| mismatch(&e))?
        .into();
    for _ in 0..16 {
        let a = pump(&mut client, &mut server).map_err(|e| mismatch(&e))?;
        let b = pump(&mut server, &mut client).map_err(|e| mismatch(&e))?;
        if !client.is_handshaking() && !server.is_handshaking() {
            return Ok(());
        }
        if !a && !b {
            break;
        }
    }
    Err(TlsError::Mismatch(
        "the test handshake did not complete".into(),
    ))
}

/// Moves what `from` wants to send into `to`. True when bytes moved.
fn pump(from: &mut Connection, to: &mut Connection) -> Result<bool, rustls::Error> {
    let mut buf = Vec::new();
    while from.wants_write() {
        from.write_tls(&mut buf)
            .map_err(|e| rustls::Error::General(e.to_string()))?;
    }
    let moved = !buf.is_empty();
    let mut rd = buf.as_slice();
    while !rd.is_empty() {
        to.read_tls(&mut rd)
            .map_err(|e| rustls::Error::General(e.to_string()))?;
        to.process_new_packets()?;
    }
    Ok(moved)
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Serves `app` over TLS on an already-bound listener.
pub async fn serve_app(
    app: axum::Router,
    listener: tokio::net::TcpListener,
    tls: TlsServing,
) -> Result<(), OrchestratorError> {
    let std_listener = listener.into_std().map_err(OrchestratorError::Io)?;
    axum_server::from_tcp_rustls(std_listener, tls.config)
        .serve(app.into_make_service_with_connect_info::<SocketAddr>())
        .await
        .map_err(|e| OrchestratorError::Core(format!("HTTPS server error: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tempfile::TempDir;

    fn addr() -> SocketAddr {
        "127.0.0.1:8080".parse().unwrap()
    }

    fn settings<'a>(dir: &'a Path, extra: &'a [String]) -> TlsSettings<'a> {
        TlsSettings {
            cert: None,
            key: None,
            insecure_dev: false,
            extra_sans: extra,
            listen: addr(),
            state_dir: dir,
        }
    }

    fn tls(t: Transport) -> TlsServing {
        match t {
            Transport::Tls(t) => t,
            Transport::Plain => panic!("expected TLS"),
        }
    }

    fn err(r: Result<Transport, TlsError>) -> TlsError {
        match r {
            Err(e) => e,
            Ok(_) => panic!("expected an error"),
        }
    }

    fn ca_of(t: &TlsServing) -> PathBuf {
        match &t.source {
            CertSource::SelfSigned { ca_path, .. } => ca_path.clone(),
            CertSource::Operator { .. } => panic!("not self-signed"),
        }
    }

    #[test]
    fn a_self_signed_certificate_is_made_then_reused_after_a_restart() {
        let d = TempDir::new().unwrap();
        let first = tls(resolve(&settings(d.path(), &[])).unwrap());
        assert!(matches!(
            first.source,
            CertSource::SelfSigned {
                created: true,
                regenerated: false,
                ..
            }
        ));
        let pem1 = fs::read(ca_of(&first)).unwrap();
        let key1 = fs::read(d.path().join("tls/server.key")).unwrap();
        // A restart: the same state dir again.
        let second = tls(resolve(&settings(d.path(), &[])).unwrap());
        assert!(matches!(
            second.source,
            CertSource::SelfSigned {
                created: false,
                regenerated: false,
                ..
            }
        ));
        assert_eq!(first.fingerprint, second.fingerprint);
        assert_eq!(pem1, fs::read(ca_of(&second)).unwrap());
        assert_eq!(key1, fs::read(d.path().join("tls/server.key")).unwrap());
    }

    #[test]
    fn the_key_is_0600_and_the_dir_0700() {
        let d = TempDir::new().unwrap();
        resolve(&settings(d.path(), &[])).unwrap();
        let mode = |p: &str| fs::metadata(d.path().join(p)).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode("tls/server.key"), 0o600);
        assert_eq!(mode("tls"), 0o700);
        assert_eq!(mode("tls/ca.pem"), 0o644);
        // No temp file is left behind.
        let names: Vec<_> = fs::read_dir(d.path().join("tls"))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names.len(), 3, "{names:?}");
    }

    #[test]
    fn a_key_that_was_too_open_is_tightened_with_a_note() {
        let d = TempDir::new().unwrap();
        resolve(&settings(d.path(), &[])).unwrap();
        let key = d.path().join("tls/server.key");
        fs::set_permissions(&key, fs::Permissions::from_mode(0o644)).unwrap();
        let t = tls(resolve(&settings(d.path(), &[])).unwrap());
        assert_eq!(
            fs::metadata(&key).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(t.notes.len(), 1);
    }

    #[test]
    fn the_certificate_covers_the_loopback_names_and_valid_extra_names() {
        let a = san_set("0.0.0.0:8080".parse().unwrap(), &[]).unwrap();
        assert_eq!(a, vec!["dns:localhost", "ip:127.0.0.1", "ip:::1"]);
        let b = san_set(
            "10.1.2.3:8080".parse().unwrap(),
            &["Node.Example".to_string(), "192.0.2.9".to_string()],
        )
        .unwrap();
        assert!(b.contains(&"ip:10.1.2.3".to_string()));
        assert!(b.contains(&"dns:node.example".to_string()));
        assert!(b.contains(&"ip:192.0.2.9".to_string()));
        assert!(matches!(
            san_set(addr(), &["not a name".to_string()]),
            Err(TlsError::BadSan(_))
        ));
    }

    #[test]
    fn a_changed_san_set_regenerates_the_certificate() {
        let d = TempDir::new().unwrap();
        let first = tls(resolve(&settings(d.path(), &[])).unwrap());
        let extra = vec!["node.example".to_string()];
        let second = tls(resolve(&settings(d.path(), &extra)).unwrap());
        assert!(matches!(
            second.source,
            CertSource::SelfSigned {
                regenerated: true,
                ..
            }
        ));
        assert_ne!(first.fingerprint, second.fingerprint);
    }

    #[test]
    fn a_certificate_with_under_30_days_left_is_regenerated() {
        let d = TempDir::new().unwrap();
        let now = 1_800_000_000;
        let first = tls(resolve_at(&settings(d.path(), &[]), now).unwrap());
        // 29 days before the 2-year expiry.
        let late = now + (VALIDITY_DAYS - 29) * DAY_SECS;
        let second = tls(resolve_at(&settings(d.path(), &[]), late).unwrap());
        assert!(matches!(
            second.source,
            CertSource::SelfSigned {
                regenerated: true,
                ..
            }
        ));
        assert_ne!(first.fingerprint, second.fingerprint);
        // 31 days before: kept.
        let d2 = TempDir::new().unwrap();
        let a = tls(resolve_at(&settings(d2.path(), &[]), now).unwrap());
        let early = now + (VALIDITY_DAYS - 31) * DAY_SECS;
        let b = tls(resolve_at(&settings(d2.path(), &[]), early).unwrap());
        assert_eq!(a.fingerprint, b.fingerprint);
    }

    #[test]
    fn stored_material_that_cannot_be_used_stops_vamd_and_is_kept() {
        let d = TempDir::new().unwrap();
        resolve(&settings(d.path(), &[])).unwrap();
        let tls_dir = d.path().join("tls");
        // Half of the stored set is gone.
        fs::remove_file(tls_dir.join("server.key")).unwrap();
        assert!(matches!(
            err(resolve(&settings(d.path(), &[]))),
            TlsError::Stored { .. }
        ));
        assert!(
            !tls_dir.join("server.key").exists(),
            "nothing was re-minted"
        );
        // Garbage in the certificate.
        let d = TempDir::new().unwrap();
        resolve(&settings(d.path(), &[])).unwrap();
        fs::write(d.path().join("tls/ca.pem"), b"not pem").unwrap();
        let e = err(resolve(&settings(d.path(), &[])));
        assert!(matches!(e, TlsError::Stored { .. }), "{e}");
        assert!(e.to_string().contains("delete that directory"));
        assert_eq!(fs::read(d.path().join("tls/ca.pem")).unwrap(), b"not pem");
    }

    fn operator_pair(dir: &Path) -> (PathBuf, PathBuf) {
        // A pair from a second state dir, so the operator's files are outside the node's.
        let src = TempDir::new().unwrap();
        resolve(&settings(src.path(), &[])).unwrap();
        let (c, k) = (dir.join("op-cert.pem"), dir.join("op-key.pem"));
        fs::copy(src.path().join("tls/ca.pem"), &c).unwrap();
        fs::copy(src.path().join("tls/server.key"), &k).unwrap();
        (c, k)
    }

    #[test]
    fn an_operator_pair_is_served_and_nothing_is_written_to_the_state_dir() {
        let ops = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        let (c, k) = operator_pair(ops.path());
        let mut s = settings(state.path(), &[]);
        s.cert = Some(&c);
        s.key = Some(&k);
        let t = tls(resolve(&s).unwrap());
        assert!(matches!(t.source, CertSource::Operator { .. }));
        assert_eq!(t.fingerprint.split(':').count(), 32);
        assert!(!state.path().join("tls").exists());
    }

    #[test]
    fn a_half_set_operator_pair_fails() {
        let ops = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        let (c, k) = operator_pair(ops.path());
        let mut s = settings(state.path(), &[]);
        s.cert = Some(&c);
        assert!(matches!(
            err(resolve(&s)),
            TlsError::HalfSet {
                set: "VAMD_TLS_CERT",
                ..
            }
        ));
        s.cert = None;
        s.key = Some(&k);
        assert!(matches!(
            err(resolve(&s)),
            TlsError::HalfSet {
                set: "VAMD_TLS_KEY",
                ..
            }
        ));
    }

    #[test]
    fn an_unreadable_or_garbage_operator_pair_fails() {
        let ops = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        let (c, k) = operator_pair(ops.path());
        let mut s = settings(state.path(), &[]);
        let missing = ops.path().join("nope.pem");
        s.cert = Some(&missing);
        s.key = Some(&k);
        assert!(matches!(err(resolve(&s)), TlsError::Read { .. }));
        let junk = ops.path().join("junk.pem");
        fs::write(&junk, b"junk").unwrap();
        s.cert = Some(&junk);
        assert!(matches!(err(resolve(&s)), TlsError::NoItem { .. }));
        s.cert = Some(&c);
        s.key = Some(&junk);
        assert!(matches!(err(resolve(&s)), TlsError::NoItem { .. }));
    }

    #[test]
    fn a_certificate_with_another_nodes_key_fails_the_handshake_self_check() {
        let ops = TempDir::new().unwrap();
        let other = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        let (c, _k) = operator_pair(ops.path());
        let (_c2, k2) = operator_pair(other.path());
        let mut s = settings(state.path(), &[]);
        s.cert = Some(&c);
        s.key = Some(&k2);
        assert!(matches!(err(resolve(&s)), TlsError::Mismatch(_)));
    }

    #[test]
    fn insecure_dev_serves_plain_http_and_reports_tls_off() {
        let d = TempDir::new().unwrap();
        let mut s = settings(d.path(), &[]);
        s.insecure_dev = true;
        let t = resolve(&s).unwrap();
        assert!(!t.is_tls());
        assert!(!d.path().join("tls").exists(), "no certificate is made");
        let t = resolve(&settings(d.path(), &[])).unwrap();
        assert!(t.is_tls());
    }

    #[test]
    fn insecure_dev_with_an_operator_pair_is_refused() {
        let ops = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        let (c, k) = operator_pair(ops.path());
        let mut s = settings(state.path(), &[]);
        s.insecure_dev = true;
        s.cert = Some(&c);
        s.key = Some(&k);
        assert!(matches!(err(resolve(&s)), TlsError::Conflict));
        s.key = None;
        assert!(matches!(err(resolve(&s)), TlsError::Conflict));
    }

    async fn serve(d: &Path) -> (u16, PathBuf, tokio::task::JoinHandle<()>) {
        let t = tls(resolve(&settings(d, &[])).unwrap());
        let ca = ca_of(&t);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app =
            axum::Router::new().route("/v1/x", axum::routing::get(|| async { "secret-answer" }));
        let h = tokio::spawn(async move {
            let _ = serve_app(app, listener, t).await;
        });
        (port, ca, h)
    }

    #[tokio::test]
    async fn https_with_the_node_certificate_works_and_without_it_fails() {
        let d = TempDir::new().unwrap();
        let (port, ca, h) = serve(d.path()).await;
        let pem = fs::read(&ca).unwrap();
        let trusting = reqwest::Client::builder()
            .add_root_certificate(reqwest::Certificate::from_pem(&pem).unwrap())
            .build()
            .unwrap();
        for host in ["127.0.0.1", "localhost"] {
            let r = trusting
                .get(format!("https://{host}:{port}/v1/x"))
                .send()
                .await
                .unwrap();
            assert_eq!(r.text().await.unwrap(), "secret-answer", "{host}");
        }
        let bare = reqwest::Client::new();
        assert!(bare
            .get(format!("https://127.0.0.1:{port}/v1/x"))
            .send()
            .await
            .is_err());
        h.abort();
    }

    #[tokio::test]
    async fn plain_http_to_the_tls_port_gets_no_api_answer() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let d = TempDir::new().unwrap();
        let (port, _ca, h) = serve(d.path()).await;
        let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        s.write_all(b"GET /v1/x HTTP/1.1\r\nHost: a\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut got = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut got)).await;
        let text = String::from_utf8_lossy(&got);
        assert!(!text.starts_with("HTTP/"), "{text}");
        assert!(!text.contains("secret-answer"), "{text}");
        h.abort();
    }

    #[tokio::test]
    async fn a_restart_serves_the_same_certificate() {
        let d = TempDir::new().unwrap();
        let (port1, ca, h) = serve(d.path()).await;
        h.abort();
        let pem1 = fs::read(&ca).unwrap();
        let (port2, ca2, h2) = serve(d.path()).await;
        assert_eq!(pem1, fs::read(&ca2).unwrap());
        // A client holding the first run's trust file talks to the second run.
        let c = reqwest::Client::builder()
            .add_root_certificate(reqwest::Certificate::from_pem(&pem1).unwrap())
            .build()
            .unwrap();
        let r = c
            .get(format!("https://127.0.0.1:{port2}/v1/x"))
            .send()
            .await
            .unwrap();
        assert!(r.status().is_success());
        let _ = port1;
        h2.abort();
    }
}
