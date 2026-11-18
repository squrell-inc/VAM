//! Bundled Mozilla CA roots for host-side reqwest clients.
//!
//! reqwest 0.11's `rustls-tls` feature trusted the webpki-roots bundle and nothing else. reqwest
//! 0.13's `rustls` feature trusts the platform store instead. Every host client goes through
//! [`BundledRoots::bundled_roots`] so it keeps trusting exactly the Mozilla bundle (plus any root
//! the caller adds), whatever CA store the host machine has or lacks.

use std::sync::OnceLock;

/// The Mozilla root bundle as reqwest certificates, parsed once per process.
pub fn mozilla_roots() -> &'static [reqwest::Certificate] {
    static ROOTS: OnceLock<Vec<reqwest::Certificate>> = OnceLock::new();
    ROOTS.get_or_init(|| {
        webpki_root_certs::TLS_SERVER_ROOT_CERTS
            .iter()
            .filter_map(|der| reqwest::Certificate::from_der(der.as_ref()).ok())
            .collect()
    })
}

/// Restricts a reqwest client builder to the bundled Mozilla roots. Roots the caller adds with
/// `tls_certs_merge` before or after are trusted as well.
pub trait BundledRoots: Sized {
    /// Trust the bundled Mozilla roots only (no platform store).
    fn bundled_roots(self) -> Self;
}

impl BundledRoots for reqwest::ClientBuilder {
    fn bundled_roots(self) -> Self {
        self.tls_certs_only(mozilla_roots().iter().cloned())
    }
}

impl BundledRoots for reqwest::blocking::ClientBuilder {
    fn bundled_roots(self) -> Self {
        self.tls_certs_only(mozilla_roots().iter().cloned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_bundled_root_parses() {
        assert_eq!(
            mozilla_roots().len(),
            webpki_root_certs::TLS_SERVER_ROOT_CERTS.len()
        );
        assert!(!mozilla_roots().is_empty());
    }

    #[test]
    fn a_builder_with_bundled_roots_builds() {
        assert!(reqwest::Client::builder().bundled_roots().build().is_ok());
    }
}
