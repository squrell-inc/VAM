//! How the CLI reaches vamd: TLS by default, the node's own certificate as a trusted
//! root, a warning before an API key goes out unencrypted, and guided errors when the
//! scheme or the certificate is the problem.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use base64::Engine as _;
use sha2::{Digest, Sha256};

use crate::config::ConfigFile;

/// The CA file clients trust besides the built-in roots: `VAM_CA_FILE`, else `ca_file` in the config.
pub fn ca_file() -> Option<PathBuf> {
    let file = ConfigFile::load().unwrap_or_default();
    pick_ca_file(std::env::var("VAM_CA_FILE").ok(), file.ca_file)
}

fn pick_ca_file(env: Option<String>, file: Option<String>) -> Option<PathBuf> {
    env.filter(|s| !s.trim().is_empty())
        .or_else(|| file.filter(|s| !s.trim().is_empty()))
        .map(PathBuf::from)
}

/// A client builder that trusts the configured CA file (if any) as well as the built-in roots.
pub fn client_builder() -> Result<reqwest::ClientBuilder> {
    let mut b = reqwest::Client::builder();
    if let Some(path) = ca_file() {
        let pem = std::fs::read(&path).with_context(|| format!("read CA file {path:?}"))?;
        parse_pem_cert(&pem).with_context(|| format!("CA file {path:?}"))?;
        let cert = reqwest::Certificate::from_pem(&pem)
            .with_context(|| format!("CA file {path:?} is not a PEM certificate"))?;
        b = b.add_root_certificate(cert);
    }
    Ok(b)
}

/// The DER bytes of the first certificate in `pem`.
pub fn parse_pem_cert(pem: &[u8]) -> Result<Vec<u8>> {
    let text = String::from_utf8_lossy(pem);
    let body = text
        .split("-----BEGIN CERTIFICATE-----")
        .nth(1)
        .and_then(|rest| rest.split("-----END CERTIFICATE-----").next())
        .context("no PEM certificate in the file")?;
    let b64: String = body.chars().filter(|c| !c.is_whitespace()).collect();
    base64::engine::general_purpose::STANDARD
        .decode(b64)
        .context("the PEM certificate is not valid base64")
}

/// SHA-256 of the certificate, as `AB:CD:…` (the form vamd logs).
pub fn fingerprint(der: &[u8]) -> String {
    Sha256::digest(der)
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

/// The host part of `base` and whether it is plain `http://`.
fn scheme_and_host(base: &str) -> (Option<String>, Option<String>) {
    let (scheme, rest) = match base.split_once("://") {
        Some((s, r)) => (s.to_ascii_lowercase(), r),
        None => return (None, None),
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let authority = authority.rsplit('@').next().unwrap_or("");
    let host = if let Some(v6) = authority.strip_prefix('[') {
        v6.split(']').next().unwrap_or("").to_string()
    } else {
        authority.split(':').next().unwrap_or("").to_string()
    };
    (Some(scheme), Some(host))
}

/// True for `localhost`, 127.0.0.0/8 and `::1`.
pub fn is_loopback_host(host: &str) -> bool {
    let h = host.trim().trim_end_matches('.').to_ascii_lowercase();
    if h == "localhost" || h.ends_with(".localhost") {
        return true;
    }
    match h.parse::<std::net::IpAddr>() {
        Ok(ip) => ip.is_loopback(),
        Err(_) => false,
    }
}

/// The warning for sending a secret over plain `http://` to a host that is not loopback.
pub fn cleartext_warning(base: &str, what: &str) -> Option<String> {
    match scheme_and_host(base) {
        (Some(s), Some(host)) if s == "http" && !host.is_empty() && !is_loopback_host(&host) => {
            Some(format!(
                "sending the {what} unencrypted to {host}; use https://"
            ))
        }
        _ => None,
    }
}

static WARNED: AtomicBool = AtomicBool::new(false);

/// Prints [`cleartext_warning`] on stderr, once per process. Sending is still allowed
/// (a node started with `--insecure-dev` serves plain HTTP).
pub fn warn_cleartext(base: &str, what: &str) {
    if let Some(w) = cleartext_warning(base, what) {
        if !WARNED.swap(true, Ordering::SeqCst) {
            eprintln!("vam: warning: {w}");
        }
    }
}

fn error_text(e: &(dyn std::error::Error + 'static)) -> String {
    let mut s = e.to_string();
    let mut src = e.source();
    while let Some(c) = src {
        s.push_str(": ");
        s.push_str(&c.to_string());
        src = c.source();
    }
    s.to_ascii_lowercase()
}

fn is_certificate_error(text: &str) -> bool {
    text.contains("certificate") || text.contains("unknownissuer") || text.contains("invalid peer")
}

/// One probe of `<scheme>://<same host:port>/v1/health`. It carries NO credentials: no
/// `Authorization` header, no cookies, no body. Returns the HTTP status, or the transport
/// error text. `scheme` is `https` in use; the tests use `http` to read the request.
async fn probe(base: &str, scheme: &str, client: &reqwest::Client) -> Result<u16, String> {
    let rest = base.split_once("://").map(|(_, r)| r).unwrap_or(base);
    let authority = rest.split('/').next().unwrap_or(rest);
    let url = format!("{scheme}://{authority}/v1/health");
    client
        .get(url)
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .map(|r| r.status().as_u16())
        .map_err(|e| error_text(&e))
}

/// Turns a failed request into a guided error when the scheme or the certificate is the
/// cause; otherwise returns `err` unchanged. `base` is the host the CLI used.
pub async fn explain(err: anyhow::Error, base: &str) -> anyhow::Error {
    let req_err = err.chain().find_map(|c| c.downcast_ref::<reqwest::Error>());
    let Some(req_err) = req_err else {
        return err;
    };
    if req_err.is_timeout() {
        return err;
    }
    let text = error_text(req_err);
    let (scheme, host) = scheme_and_host(base);
    let host = host.unwrap_or_default();
    match scheme.as_deref() {
        Some("http") if !req_err.is_connect() => {
            // The node answered the connection but not as plain HTTP. Ask once, without
            // any credentials, whether it speaks TLS.
            let Ok(client) = client_builder().and_then(|b| Ok(b.build()?)) else {
                return err;
            };
            let tls_base = base.replacen("http://", "https://", 1);
            let speaks_tls = match probe(base, "https", &client).await {
                Ok(_) => true,
                Err(t) => is_certificate_error(&t),
            };
            if speaks_tls {
                return crate::output::fail(
                    crate::output::ExitCode::Unreachable,
                    format!(
                        "{host} did not answer plain HTTP, but it answers TLS: this node serves HTTPS (the default). Use {tls_base} (`vam config set-host {tls_base}` or --host); if it is not trusted yet, run `vam config set-ca <ca.pem>`. A node started with --insecure-dev serves plain HTTP.\n  cause: {err:#}"
                    ),
                );
            }
            err
        }
        Some("https") if is_certificate_error(&text) => {
            let hint = match ca_file() {
                Some(p) => format!(
                    "the CA file {p:?} does not match this node's certificate; check VAM_CA_FILE and `vam config show`"
                ),
                None => "its certificate is not trusted. A node with no operator certificate makes a self-signed one: copy `<state dir>/tls/ca.pem` from the node and run `vam config set-ca <path>` (or set VAM_CA_FILE), then compare the fingerprint with the one vamd logged".to_string(),
            };
            crate::output::fail(
                crate::output::ExitCode::Unreachable,
                format!("cannot verify the TLS certificate of {host}: {hint}\n  cause: {err:#}"),
            )
        }
        _ => err,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    #[test]
    fn loopback_hosts_are_classified() {
        for h in [
            "localhost",
            "LOCALHOST",
            "127.0.0.1",
            "127.9.9.9",
            "::1",
            "foo.localhost",
        ] {
            assert!(is_loopback_host(h), "{h}");
        }
        for h in [
            "10.0.0.5",
            "192.168.1.1",
            "example.com",
            "localhost.example.com",
            "0.0.0.0",
            "2001:db8::1",
            "128.0.0.1",
            "",
        ] {
            assert!(!is_loopback_host(h), "{h}");
        }
    }

    #[test]
    fn the_cleartext_warning_fires_only_for_http_to_a_non_loopback_host() {
        let w = cleartext_warning("http://node.example:8080", "API key").unwrap();
        assert_eq!(
            w,
            "sending the API key unencrypted to node.example; use https://"
        );
        assert!(cleartext_warning("http://10.1.2.3:8080/", "API key").is_some());
        assert!(cleartext_warning("http://user@node.example", "API key").is_some());
        assert!(cleartext_warning("http://127.0.0.1:8080", "API key").is_none());
        assert!(cleartext_warning("http://localhost:8080", "API key").is_none());
        assert!(cleartext_warning("http://[::1]:8080", "API key").is_none());
        assert!(cleartext_warning("https://node.example", "API key").is_none());
        assert!(cleartext_warning("node.example", "API key").is_none());
        // A look-alike user part does not make a remote host loopback.
        assert!(cleartext_warning("http://127.0.0.1@evil.example/", "API key").is_some());
    }

    #[test]
    fn the_ca_file_comes_from_env_before_the_config_file() {
        assert_eq!(
            pick_ca_file(Some("/e.pem".into()), Some("/f.pem".into())),
            Some(PathBuf::from("/e.pem"))
        );
        assert_eq!(
            pick_ca_file(None, Some("/f.pem".into())),
            Some(PathBuf::from("/f.pem"))
        );
        assert_eq!(
            pick_ca_file(Some("  ".into()), Some("/f.pem".into())),
            Some(PathBuf::from("/f.pem"))
        );
        assert_eq!(pick_ca_file(None, None), None);
    }

    #[test]
    fn a_pem_certificate_parses_and_has_a_fingerprint() {
        let pem = "-----BEGIN CERTIFICATE-----\nAQID\n-----END CERTIFICATE-----\n";
        let der = parse_pem_cert(pem.as_bytes()).unwrap();
        assert_eq!(der, vec![1, 2, 3]);
        assert_eq!(fingerprint(&der).split(':').count(), 32);
        assert!(parse_pem_cert(b"nope").is_err());
    }

    #[tokio::test]
    async fn the_https_hint_probe_sends_no_authorization_or_cookie() {
        // A recording listener. The probe's own request builder is the code under test;
        // `http` as the scheme lets this test read the request line and headers.
        let seen: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let s2 = seen.clone();
        let h = std::thread::spawn(move || {
            let (mut c, _) = l.accept().unwrap();
            let mut buf = [0u8; 4096];
            let n = c.read(&mut buf).unwrap();
            *s2.lock().unwrap() = String::from_utf8_lossy(&buf[..n]).to_string();
            c.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n")
                .unwrap();
        });
        // A client that WOULD send credentials if the probe used its defaults.
        let client = reqwest::Client::builder()
            .default_headers({
                let mut h = reqwest::header::HeaderMap::new();
                h.insert("x-default", "1".parse().unwrap());
                h
            })
            .build()
            .unwrap();
        let status = probe(&format!("http://{addr}/ignored/path"), "http", &client)
            .await
            .unwrap();
        h.join().unwrap();
        assert_eq!(status, 200);
        let req = seen.lock().unwrap().to_ascii_lowercase();
        assert!(req.starts_with("get /v1/health "), "{req}");
        assert!(!req.contains("authorization"), "{req}");
        assert!(!req.contains("cookie"), "{req}");
        assert!(!req.contains("bearer"), "{req}");
    }

    #[tokio::test]
    async fn an_http_host_against_a_node_that_drops_plain_http_is_not_misreported_when_tls_is_absent(
    ) {
        // A listener that closes every connection: it does not speak TLS either, so the
        // original error stays.
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        std::thread::spawn(move || {
            for c in l.incoming() {
                drop(c)
            }
        });
        let base = format!("http://{addr}");
        let c = reqwest::Client::new();
        let e = c.get(format!("{base}/v1/health")).send().await.unwrap_err();
        let original = anyhow::Error::new(e).context("GET request failed");
        let out = explain(original, &base).await;
        assert!(format!("{out:#}").contains("GET request failed"), "{out:#}");
        assert!(!format!("{out:#}").contains("serves HTTPS"), "{out:#}");
    }
}
