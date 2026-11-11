//! Per-session CA for guest-trusted TLS termination (E8-P1).
//!
//! The CA private key and the shared leaf key stay in process memory. They are
//! not logged or written to disk. [`Debug`] output redacts them. On drop, the
//! PKCS#8 copies retained by `rcgen` are zeroized.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::net::IpAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use rcgen::{
    BasicConstraints, Certificate, CertificateParams, DistinguishedName, DnType, Ia5String, IsCa,
    KeyPair, KeyUsagePurpose, SanType, SerialNumber, PKCS_ECDSA_P256_SHA256,
};
use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer, ServerName};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::{CertifiedKey, SigningKey};
use zeroize::Zeroize;

use crate::error::VamNetworkError;

/// Clock skew subtracted from `not_before` so a guest whose clock is slightly behind still trusts the CA.
const SKEW_MINUTES: i64 = 5;
/// Default session lifetime. The CA is valid for this long plus [`CA_MARGIN_HOURS`].
const SESSION_LIFETIME_HOURS: i64 = 24;
/// Extra validity past the session lifetime so a leaf minted near the end still chains.
const CA_MARGIN_HOURS: i64 = 1;
/// Leaf certificates are valid for at most this long, and never past the CA.
const LEAF_LIFETIME_HOURS: i64 = 24;
/// Bound on distinct leaf names cached for one session.
const LEAF_CACHE_CAPACITY: usize = 1000;
/// Characters of the session id embedded in the CA subject.
const SHORT_SESSION_ID_LEN: usize = 8;

/// Session-scoped certificate authority used to mint MITM leaf certificates.
///
/// One ECDSA P-256 key pair signs every leaf. Leaves are cached per normalized
/// host name.
pub struct SessionCa {
    inner: Arc<SessionCaInner>,
}

struct SessionCaInner {
    session_id: String,
    subject_cn: String,
    not_before: time::OffsetDateTime,
    not_after: time::OffsetDateTime,
    ca_key: KeyPair,
    ca_cert: Certificate,
    ca_cert_pem: String,
    leaf_key: KeyPair,
    leaf_signing_key: Arc<dyn SigningKey>,
    next_serial: AtomicU64,
    cache: Mutex<LruCache<Arc<CertifiedKey>>>,
}

impl Drop for SessionCaInner {
    fn drop(&mut self) {
        // `rcgen::KeyPair` retains a PKCS#8 copy and implements `Zeroize` for that buffer.
        // The aws-lc key object is released with the pair immediately afterwards.
        self.ca_key.zeroize();
        self.leaf_key.zeroize();
    }
}

impl fmt::Debug for SessionCa {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let cached = lock(&self.inner.cache).len();
        f.debug_struct("SessionCa")
            .field("session_id", &self.inner.session_id)
            .field("subject", &self.inner.subject_cn)
            .field("cached_leaves", &cached)
            .field("private_key", &"<redacted>")
            .finish()
    }
}

impl SessionCa {
    /// Create a new CA for `session_id`.
    ///
    /// The subject CN is `VAM session CA <short session id>`, where the short id
    /// is the first eight characters. Each call generates fresh keys, so two
    /// mints for the same id do not share key material.
    pub fn mint(session_id: &str) -> Result<Self, VamNetworkError> {
        let session_id = session_id.trim().to_owned();
        let short_id = short_session_id(&session_id)?;
        let subject_cn = format!("VAM session CA {short_id}");

        let ca_key = ZeroizeKey::generate()?;
        let leaf_key = ZeroizeKey::generate()?;
        let (not_before, not_after) = ca_validity_window()?;

        let mut params = CertificateParams::default();
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, subject_cn.clone());
        params.distinguished_name = dn;
        // Path length 0: this CA signs end-entity leaves only. Name constraints are unset.
        params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        params.not_before = not_before;
        params.not_after = not_after;
        params.serial_number = Some(SerialNumber::from(1_u64));

        let ca_cert = params
            .self_signed(ca_key.get()?)
            .map_err(|err| ca_err("CA certificate", err))?;
        let ca_cert_pem = ca_cert.pem();
        let leaf_signing_key = signing_key_from_pair(leaf_key.get()?)?;

        Ok(Self {
            inner: Arc::new(SessionCaInner {
                session_id,
                subject_cn,
                not_before,
                not_after,
                ca_key: ca_key.into_key()?,
                ca_cert,
                ca_cert_pem,
                leaf_key: leaf_key.into_key()?,
                leaf_signing_key,
                next_serial: AtomicU64::new(2),
                cache: Mutex::new(LruCache::new(LEAF_CACHE_CAPACITY)),
            }),
        })
    }

    /// PEM-encoded trust anchor installed in the guest root store at boot.
    pub fn ca_cert_pem(&self) -> String {
        self.inner.ca_cert_pem.clone()
    }

    /// Dynamic server certificate resolver for rustls on the guest-facing side.
    ///
    /// On each ClientHello the SNI name selects a cached leaf. When the client
    /// omits SNI, this resolver returns `None`. Call [`Self::leaf_for`] with the
    /// CONNECT target and present that leaf, or use [`Self::server_cert_resolver_for`]
    /// which does that itself.
    pub fn server_cert_resolver(&self) -> Arc<dyn ResolvesServerCert> {
        Arc::new(SessionCertResolver {
            inner: Arc::clone(&self.inner),
            fallback_host: None,
        })
    }

    /// Resolver for one accepted CONNECT.
    ///
    /// Prefers the ClientHello SNI name. When the client omits SNI, presents the
    /// leaf for `connect_host`.
    pub fn server_cert_resolver_for(
        &self,
        connect_host: &str,
    ) -> Result<Arc<dyn ResolvesServerCert>, VamNetworkError> {
        let name = normalize_host(connect_host)?;
        self.inner.leaf_for_name(&name)?;
        Ok(Arc::new(SessionCertResolver {
            inner: Arc::clone(&self.inner),
            fallback_host: Some(name.cache_key()),
        }))
    }

    /// Leaf certificate for `host`, signed by this session CA.
    ///
    /// `host` is a DNS name or an IP literal (brackets around IPv6 are accepted).
    /// A repeated call for the same normalized name returns the cached leaf.
    /// E8-1.4 uses this as the certificate when the client sends no SNI.
    pub fn leaf_for(&self, host: &str) -> Result<Arc<CertifiedKey>, VamNetworkError> {
        let name = normalize_host(host)?;
        self.inner.leaf_for_name(&name)
    }
}

/// Holds a [`KeyPair`] and zeroizes its PKCS#8 buffer if dropped before the key is moved out.
struct ZeroizeKey(Option<KeyPair>);

impl ZeroizeKey {
    fn generate() -> Result<Self, VamNetworkError> {
        KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256)
            .map(|key| Self(Some(key)))
            .map_err(|err| ca_err("ECDSA P-256 key", err))
    }

    fn get(&self) -> Result<&KeyPair, VamNetworkError> {
        self.0
            .as_ref()
            .ok_or_else(|| internal("session CA: key already moved"))
    }

    fn into_key(mut self) -> Result<KeyPair, VamNetworkError> {
        self.0
            .take()
            .ok_or_else(|| internal("session CA: key already moved"))
    }
}

impl Drop for ZeroizeKey {
    fn drop(&mut self) {
        if let Some(key) = self.0.as_mut() {
            key.zeroize();
        }
    }
}

struct SessionCertResolver {
    inner: Arc<SessionCaInner>,
    fallback_host: Option<String>,
}

impl fmt::Debug for SessionCertResolver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionCertResolver")
            .field("session_id", &self.inner.session_id)
            .field("fallback_host", &self.fallback_host)
            .field("private_key", &"<redacted>")
            .finish()
    }
}

impl ResolvesServerCert for SessionCertResolver {
    fn resolve(&self, client_hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        let host = match client_hello.server_name() {
            Some(sni) => sni,
            None => self.fallback_host.as_deref()?,
        };
        match self.inner.leaf_for(host) {
            Ok(key) => Some(key),
            Err(err) => {
                tracing::debug!(error = %err, "session CA refused a ClientHello");
                None
            }
        }
    }
}

impl SessionCaInner {
    fn leaf_for(&self, host: &str) -> Result<Arc<CertifiedKey>, VamNetworkError> {
        let name = normalize_host(host)?;
        self.leaf_for_name(&name)
    }

    fn leaf_for_name(&self, name: &LeafName) -> Result<Arc<CertifiedKey>, VamNetworkError> {
        let cache_key = name.cache_key();
        {
            let mut cache = lock(&self.cache);
            if let Some(hit) = cache.get(&cache_key) {
                return Ok(Arc::clone(hit));
            }
        }

        let minted = self.mint_leaf(name)?;

        let mut cache = lock(&self.cache);
        if let Some(existing) = cache.get(&cache_key) {
            return Ok(Arc::clone(existing));
        }
        cache.insert(cache_key, Arc::clone(&minted));
        Ok(minted)
    }

    fn mint_leaf(&self, name: &LeafName) -> Result<Arc<CertifiedKey>, VamNetworkError> {
        let now = time::OffsetDateTime::now_utc();
        let (not_before, not_after) = leaf_validity(self.not_before, self.not_after, now)?;
        let serial = self.next_serial.fetch_add(1, Ordering::Relaxed);

        let mut params = CertificateParams::default();
        let mut dn = DistinguishedName::new();
        let cn = name.cache_key();
        if cn.len() <= 64 {
            dn.push(DnType::CommonName, cn);
        } else {
            dn.push(DnType::CommonName, "VAM leaf");
        }
        params.distinguished_name = dn;
        params.subject_alt_names = vec![name.san()?];
        params.is_ca = IsCa::ExplicitNoCa;
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
        params.use_authority_key_identifier_extension = true;
        params.not_before = not_before;
        params.not_after = not_after;
        params.serial_number = Some(SerialNumber::from(serial));

        let leaf = params
            .signed_by(&self.leaf_key, &self.ca_cert, &self.ca_key)
            .map_err(|err| ca_err("leaf certificate", err))?;
        let cert = rustls::pki_types::CertificateDer::from(leaf.der().to_vec());
        let certified = CertifiedKey::new(vec![cert], Arc::clone(&self.leaf_signing_key));
        certified.keys_match().map_err(|err| {
            internal(format!(
                "session CA: leaf key does not match certificate: {err}"
            ))
        })?;
        Ok(Arc::new(certified))
    }
}

enum LeafName {
    Dns(String),
    Ip(IpAddr),
}

impl LeafName {
    fn cache_key(&self) -> String {
        match self {
            LeafName::Dns(name) => name.clone(),
            LeafName::Ip(ip) => ip.to_string(),
        }
    }

    fn san(&self) -> Result<SanType, VamNetworkError> {
        match self {
            LeafName::Dns(name) => {
                let ia5 =
                    Ia5String::try_from(name.as_str()).map_err(|err| ca_err("DNS name", err))?;
                Ok(SanType::DnsName(ia5))
            }
            LeafName::Ip(ip) => Ok(SanType::IpAddress(*ip)),
        }
    }
}

fn normalize_host(host: &str) -> Result<LeafName, VamNetworkError> {
    let trimmed = host.trim();
    if trimmed.is_empty() {
        return Err(invalid_host(host));
    }
    let unbracketed = if let Some(rest) = trimmed.strip_prefix('[') {
        rest.strip_suffix(']').ok_or_else(|| invalid_host(host))?
    } else {
        trimmed
    };
    let unbracketed = unbracketed.trim_end_matches('.');
    if unbracketed.is_empty() {
        return Err(invalid_host(host));
    }
    if let Ok(ip) = unbracketed.parse::<IpAddr>() {
        return Ok(LeafName::Ip(ip));
    }
    let dns = unbracketed.to_ascii_lowercase();
    let parsed = ServerName::try_from(dns.as_str()).map(|name| name.to_owned());
    match parsed {
        Ok(ServerName::DnsName(_)) => Ok(LeafName::Dns(dns)),
        Ok(ServerName::IpAddress(ip)) => Ok(LeafName::Ip(std::net::IpAddr::from(ip))),
        Ok(_) | Err(_) => Err(invalid_host(host)),
    }
}

fn short_session_id(session_id: &str) -> Result<String, VamNetworkError> {
    if session_id.is_empty() || session_id.chars().any(|c| c.is_control()) {
        return Err(internal(
            "session CA: session id must be non-empty and must not contain control characters",
        ));
    }
    Ok(session_id.chars().take(SHORT_SESSION_ID_LEN).collect())
}

fn ca_validity_window() -> Result<(time::OffsetDateTime, time::OffsetDateTime), VamNetworkError> {
    let now = time::OffsetDateTime::now_utc();
    let not_before = checked_sub(now, time::Duration::minutes(SKEW_MINUTES))?;
    let lifetime = checked_add(now, time::Duration::hours(SESSION_LIFETIME_HOURS))?;
    let not_after = checked_add(lifetime, time::Duration::hours(CA_MARGIN_HOURS))?;
    Ok((not_before, not_after))
}

fn leaf_validity(
    ca_not_before: time::OffsetDateTime,
    ca_not_after: time::OffsetDateTime,
    now: time::OffsetDateTime,
) -> Result<(time::OffsetDateTime, time::OffsetDateTime), VamNetworkError> {
    let mut not_before = checked_sub(now, time::Duration::minutes(SKEW_MINUTES))?;
    if not_before < ca_not_before {
        not_before = ca_not_before;
    }
    let mut not_after = checked_add(now, time::Duration::hours(LEAF_LIFETIME_HOURS))?;
    if not_after > ca_not_after {
        not_after = ca_not_after;
    }
    if not_after <= not_before {
        return Err(internal(
            "session CA: certificate authority is outside its validity window",
        ));
    }
    Ok((not_before, not_after))
}

fn checked_add(
    base: time::OffsetDateTime,
    duration: time::Duration,
) -> Result<time::OffsetDateTime, VamNetworkError> {
    base.checked_add(duration)
        .ok_or_else(|| internal("session CA: validity window overflow"))
}

fn checked_sub(
    base: time::OffsetDateTime,
    duration: time::Duration,
) -> Result<time::OffsetDateTime, VamNetworkError> {
    base.checked_sub(duration)
        .ok_or_else(|| internal("session CA: validity window overflow"))
}

fn signing_key_from_pair(pair: &KeyPair) -> Result<Arc<dyn SigningKey>, VamNetworkError> {
    let mut der = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(pair.serialize_der()));
    let provider = rustls::crypto::aws_lc_rs::default_provider();
    let key = provider
        .key_provider
        .load_private_key(der.clone_key())
        .map_err(|err| internal(format!("session CA: rustls rejected the leaf key: {err}")))?;
    der.zeroize();
    Ok(key)
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn internal(message: impl Into<String>) -> VamNetworkError {
    VamNetworkError::Internal(message.into())
}

fn ca_err(context: &str, err: impl fmt::Display) -> VamNetworkError {
    internal(format!("session CA: {context}: {err}"))
}

fn invalid_host(host: &str) -> VamNetworkError {
    let shown: String = host
        .chars()
        .take(253)
        .map(|c| if c.is_control() { '?' } else { c })
        .collect();
    internal(format!("session CA: invalid host '{shown}'"))
}

/// Bounded map that drops the least recently used entry past `cap`.
struct LruCache<V> {
    cap: usize,
    map: HashMap<String, V>,
    order: VecDeque<String>,
}

impl<V> LruCache<V> {
    fn new(cap: usize) -> Self {
        Self {
            cap: cap.max(1),
            map: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    fn len(&self) -> usize {
        self.map.len()
    }

    fn get(&mut self, key: &str) -> Option<&V> {
        if self.map.contains_key(key) {
            self.touch(key);
            self.map.get(key)
        } else {
            None
        }
    }

    fn insert(&mut self, key: String, value: V) {
        if self.map.contains_key(&key) {
            self.map.insert(key.clone(), value);
            self.touch(&key);
            return;
        }
        while self.map.len() >= self.cap {
            let Some(old) = self.order.pop_front() else {
                break;
            };
            self.map.remove(&old);
        }
        self.order.push_back(key.clone());
        self.map.insert(key, value);
    }

    fn touch(&mut self, key: &str) {
        if let Some(pos) = self.order.iter().position(|existing| existing == key) {
            if let Some(existing) = self.order.remove(pos) {
                self.order.push_back(existing);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::IpAddr;
    use std::sync::Arc;

    use rustls::client::danger::ServerCertVerifier;
    use rustls::client::WebPkiServerVerifier;
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
    use rustls::sign::CertifiedKey;
    use rustls::RootCertStore;
    use sha2::{Digest, Sha256};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio_rustls::{TlsAcceptor, TlsConnector};

    use super::*;

    fn fingerprint(der: &[u8]) -> [u8; 32] {
        Sha256::digest(der).into()
    }

    fn parse_pem(pem: &str) -> CertificateDer<'static> {
        CertificateDer::from_pem_slice(pem.as_bytes()).expect("PEM parses")
    }

    fn verifier(ca_pem: &str) -> Arc<WebPkiServerVerifier> {
        let mut roots = RootCertStore::empty();
        roots
            .add(parse_pem(ca_pem))
            .expect("session CA is a trust anchor");
        WebPkiServerVerifier::builder(Arc::new(roots))
            .build()
            .expect("verifier")
    }

    fn verify(
        verifier: &WebPkiServerVerifier,
        leaf: &CertifiedKey,
        name: &ServerName<'_>,
    ) -> Result<(), rustls::Error> {
        let end = leaf.end_entity_cert().expect("leaf");
        verifier
            .verify_server_cert(end, &[], name, &[], UnixTime::now())
            .map(|_| ())
    }

    fn dns_name(host: &str) -> ServerName<'static> {
        ServerName::try_from(host).expect("dns name").to_owned()
    }

    fn assert_send_sync<T: Send + Sync>() {}

    /// PKCS#8 standard base64, used to prove the private key is absent from text output.
    fn b64(data: &[u8]) -> String {
        const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        let mut i = 0;
        while i + 3 <= data.len() {
            let n =
                (u32::from(data[i]) << 16) | (u32::from(data[i + 1]) << 8) | u32::from(data[i + 2]);
            out.push(TABLE[((n >> 18) & 63) as usize] as char);
            out.push(TABLE[((n >> 12) & 63) as usize] as char);
            out.push(TABLE[((n >> 6) & 63) as usize] as char);
            out.push(TABLE[(n & 63) as usize] as char);
            i += 3;
        }
        if i < data.len() {
            let b0 = data[i];
            let b1 = if i + 1 < data.len() { data[i + 1] } else { 0 };
            let n = (u32::from(b0) << 16) | (u32::from(b1) << 8);
            out.push(TABLE[((n >> 18) & 63) as usize] as char);
            out.push(TABLE[((n >> 12) & 63) as usize] as char);
            if i + 1 < data.len() {
                out.push(TABLE[((n >> 6) & 63) as usize] as char);
            }
        }
        out
    }

    fn assert_redacts(text: &str, pkcs8: &[u8], public_der: &[u8]) {
        assert!(!text.contains("BEGIN PRIVATE"));
        assert!(!text.contains("PRIVATE KEY"));
        assert!(!text.contains("serialized_der"));
        let encoded = b64(pkcs8);
        assert!(!text.contains(&encoded), "PKCS#8 base64 appeared in output");
        let window = 8;
        let mut saw_unique = false;
        if pkcs8.len() >= window {
            for start in 0..=pkcs8.len() - window {
                let slice = &pkcs8[start..start + window];
                if public_der
                    .windows(window)
                    .any(|candidate| candidate == slice)
                {
                    continue;
                }
                saw_unique = true;
                let decimal = slice
                    .iter()
                    .map(|byte| byte.to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                assert!(
                    !text.contains(&decimal),
                    "private key bytes leaked into output: {decimal}"
                );
                let hex: String = slice.iter().map(|byte| format!("{byte:02x}")).collect();
                assert!(!text.to_ascii_lowercase().contains(&hex));
            }
        }
        assert!(
            saw_unique,
            "PKCS#8 had no bytes distinct from the public cert"
        );
    }

    #[test]
    fn session_ca_is_send_sync() {
        assert_send_sync::<SessionCa>();
    }

    #[test]
    fn mint_parses_and_two_calls_differ() {
        let first = SessionCa::mint("sessid12-aaaa").expect("mint");
        let second = SessionCa::mint("sessid12-aaaa").expect("mint");

        let first_pem = first.ca_cert_pem();
        let second_pem = second.ca_cert_pem();
        assert!(first_pem.contains("BEGIN CERTIFICATE"));
        assert!(!first_pem.contains("PRIVATE KEY"));
        let first_der = parse_pem(&first_pem);
        let second_der = parse_pem(&second_pem);
        assert_eq!(first_der.as_ref(), first.inner.ca_cert.der().as_ref());
        assert_ne!(first_der.as_ref(), second_der.as_ref());

        let cn = first
            .inner
            .ca_cert
            .params()
            .distinguished_name
            .get(&DnType::CommonName);
        assert_eq!(
            cn,
            Some(&rcgen::DnValue::Utf8String(
                "VAM session CA sessid12".to_owned()
            ))
        );

        // CA:true, pathLenConstraint 0, inside the basic constraints extension value.
        let constraints: &[u8] = &[0x30, 0x06, 0x01, 0x01, 0xff, 0x02, 0x01, 0x00];
        assert!(
            first_der
                .windows(constraints.len())
                .any(|window| window == constraints),
            "CA certificate is missing pathlen:0 basic constraints"
        );

        assert_ne!(
            first.inner.ca_key.public_key_der(),
            second.inner.ca_key.public_key_der()
        );
        assert_ne!(
            first.inner.leaf_key.public_key_der(),
            second.inner.leaf_key.public_key_der()
        );
        assert_ne!(
            fingerprint(first.inner.ca_cert.der()),
            fingerprint(second.inner.ca_cert.der())
        );
    }

    #[test]
    fn empty_session_id_is_rejected() {
        let err = SessionCa::mint("   ").expect_err("empty id");
        let message = err.to_string();
        assert!(message.contains("session id"));
    }

    #[test]
    fn leaf_verifies_for_its_name_only() {
        let ca = SessionCa::mint("leaf-test").expect("mint");
        let verifier = verifier(&ca.ca_cert_pem());
        let leaf = ca.leaf_for("api.example.com").expect("leaf");

        verify(&verifier, &leaf, &dns_name("api.example.com")).expect("name matches");
        let mismatch = verify(&verifier, &leaf, &dns_name("other.example.com"));
        assert!(mismatch.is_err(), "leaf verified for a different name");
    }

    #[test]
    fn ip_literal_leaf_has_an_ip_san() {
        let ca = SessionCa::mint("ip-test").expect("mint");
        let verifier = verifier(&ca.ca_cert_pem());
        let leaf = ca.leaf_for("192.0.2.10").expect("ip leaf");
        let ip: IpAddr = "192.0.2.10".parse().expect("ip");
        verify(&verifier, &leaf, &ServerName::from(ip)).expect("IP SAN verifies");

        let as_dns = verify(&verifier, &leaf, &dns_name("api.example.com"));
        assert!(as_dns.is_err(), "IP leaf verified for a DNS name");

        let bracketed = ca.leaf_for("[2001:db8::1]").expect("ipv6");
        let again = ca.leaf_for("2001:db8::1").expect("ipv6 canonical");
        assert!(Arc::ptr_eq(&bracketed, &again));
        let v6: IpAddr = "2001:db8::1".parse().expect("v6");
        verify(&verifier, &bracketed, &ServerName::from(v6)).expect("IPv6 SAN");
    }

    #[test]
    fn second_request_returns_the_cached_leaf() {
        let ca = SessionCa::mint("cache-test").expect("mint");
        let first = ca.leaf_for("API.Example.COM.").expect("first");
        let second = ca.leaf_for("api.example.com").expect("second");
        assert!(Arc::ptr_eq(&first, &second));

        let other = ca.leaf_for("cdn.example.com").expect("other");
        assert!(!Arc::ptr_eq(&first, &other));
    }

    #[test]
    fn debug_redacts_private_keys() {
        let ca = SessionCa::mint("debug-session").expect("mint");
        let debug = format!("{ca:?}");
        assert!(debug.contains("debug-se"));
        assert!(debug.contains("<redacted>"));
        assert_redacts(
            &debug,
            ca.inner.ca_key.serialized_der(),
            ca.inner.ca_cert.der().as_ref(),
        );
        assert_redacts(
            &debug,
            ca.inner.leaf_key.serialized_der(),
            ca.inner.ca_cert.der().as_ref(),
        );

        let resolver = ca.server_cert_resolver();
        let resolver_debug = format!("{resolver:?}");
        assert!(resolver_debug.contains("<redacted>"));
        assert_redacts(
            &resolver_debug,
            ca.inner.ca_key.serialized_der(),
            ca.inner.ca_cert.der().as_ref(),
        );
        assert_redacts(
            &resolver_debug,
            ca.inner.leaf_key.serialized_der(),
            ca.inner.ca_cert.der().as_ref(),
        );

        let leaf = ca.leaf_for("api.example.com").expect("leaf");
        assert_redacts(
            &format!("{leaf:?}"),
            ca.inner.leaf_key.serialized_der(),
            leaf.cert[0].as_ref(),
        );
        assert_redacts(
            &ca.ca_cert_pem(),
            ca.inner.ca_key.serialized_der(),
            ca.inner.ca_cert.der().as_ref(),
        );
    }

    #[test]
    fn lru_evicts_least_recently_used() {
        let mut cache = LruCache::new(2);
        cache.insert("a".into(), 1);
        cache.insert("b".into(), 2);
        assert_eq!(cache.get("a").copied(), Some(1));
        cache.insert("c".into(), 3);
        assert_eq!(cache.get("b"), None);
        assert_eq!(cache.get("a").copied(), Some(1));
        assert_eq!(cache.get("c").copied(), Some(3));

        cache.insert("a".into(), 9);
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.get("a").copied(), Some(9));
    }

    fn client_config(ca_pem: &str) -> rustls::ClientConfig {
        let mut roots = RootCertStore::empty();
        roots.add(parse_pem(ca_pem)).expect("root");
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .expect("versions")
            .with_root_certificates(roots)
            .with_no_client_auth()
    }

    fn server_config(resolver: Arc<dyn ResolvesServerCert>) -> Arc<rustls::ServerConfig> {
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        Arc::new(
            rustls::ServerConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .expect("versions")
                .with_no_client_auth()
                .with_cert_resolver(resolver),
        )
    }

    async fn tls_ping(
        resolver: Arc<dyn ResolvesServerCert>,
        ca_pem: &str,
        server_name: ServerName<'static>,
        enable_sni: bool,
    ) -> std::io::Result<Vec<u8>> {
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let server_config = server_config(resolver);
        let mut client_config = client_config(ca_pem);
        client_config.enable_sni = enable_sni;
        let client_config = Arc::new(client_config);

        let server = tokio::spawn(async move {
            let acceptor = TlsAcceptor::from(server_config);
            let mut tls = acceptor.accept(server_io).await?;
            let mut buf = [0_u8; 4];
            tls.read_exact(&mut buf).await?;
            if &buf != b"ping" {
                return Err(std::io::Error::other("expected ping"));
            }
            tls.write_all(b"pong").await?;
            tls.flush().await?;
            Ok::<(), std::io::Error>(())
        });

        let client_result = async {
            let connector = TlsConnector::from(client_config);
            let mut tls = connector.connect(server_name, client_io).await?;
            tls.write_all(b"ping").await?;
            tls.flush().await?;
            let mut buf = [0_u8; 4];
            tls.read_exact(&mut buf).await?;
            if &buf != b"pong" {
                return Err(std::io::Error::other("expected pong"));
            }
            tls.get_ref()
                .1
                .peer_certificates()
                .and_then(|chain| chain.first())
                .map(|cert| cert.as_ref().to_vec())
                .ok_or_else(|| std::io::Error::other("missing peer certificate"))
        }
        .await;

        let server_result = server.await;
        match (client_result, server_result) {
            (Ok(peer), Ok(Ok(()))) => Ok(peer),
            (Err(err), _) => Err(err),
            (_, Ok(Err(err))) => Err(err),
            (_, Err(err)) => Err(std::io::Error::other(err.to_string())),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn handshake_over_duplex_trusts_the_session_ca() {
        let ca = SessionCa::mint("handshake").expect("mint");
        let pem = ca.ca_cert_pem();
        let peer = tls_ping(
            ca.server_cert_resolver(),
            &pem,
            dns_name("api.example.com"),
            true,
        )
        .await
        .expect("handshake");

        let cached = ca.leaf_for("api.example.com").expect("cached");
        let again = ca.leaf_for("api.example.com").expect("again");
        assert!(Arc::ptr_eq(&cached, &again));
        assert_eq!(peer.as_slice(), cached.cert[0].as_ref());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn handshake_without_sni_uses_connect_host_leaf() {
        let ca = SessionCa::mint("no-sni").expect("mint");
        let pem = ca.ca_cert_pem();
        let resolver = ca
            .server_cert_resolver_for("api.example.com")
            .expect("resolver");
        let peer = tls_ping(resolver, &pem, dns_name("api.example.com"), false)
            .await
            .expect("no-sni handshake");
        let leaf = ca.leaf_for("api.example.com").expect("leaf");
        assert_eq!(peer.as_slice(), leaf.cert[0].as_ref());

        let ip = "192.0.2.10";
        let ip_resolver = ca.server_cert_resolver_for(ip).expect("ip resolver");
        let ip_name = ServerName::from(ip.parse::<IpAddr>().expect("ip")).to_owned();
        let ip_peer = tls_ping(ip_resolver, &pem, ip_name, true)
            .await
            .expect("ip handshake sends no SNI");
        let ip_leaf = ca.leaf_for(ip).expect("ip leaf");
        assert_eq!(ip_peer.as_slice(), ip_leaf.cert[0].as_ref());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn handshake_without_sni_or_fallback_fails() {
        let ca = SessionCa::mint("no-fallback").expect("mint");
        let err = tls_ping(
            ca.server_cert_resolver(),
            &ca.ca_cert_pem(),
            dns_name("api.example.com"),
            false,
        )
        .await
        .expect_err("missing SNI and fallback");
        let _ = err;
    }
}
