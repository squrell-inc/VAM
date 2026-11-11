//! Tests that must change process-wide environment variables. They live in
//! their own integration-test binary (their own process) so they cannot leak
//! into the unit tests, which run in another process.
//!
//! The fetch seam used here (`NetworkFetcher::for_tests`) exists only with the
//! off-by-default `test-support` feature, which this package enables for its
//! own tests through a dev-dependency. The shipped `vam-web-broker` is not
//! built with it.

use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use vam_security::ssrf_guard::HostResolver;
use vam_web::fetch::{FetchOutcome, NetworkFetcher, PageFetcher};
use vam_web::provider::brave::BraveProvider;
use vam_web::provider::{ResolvedSecrets, SearchProvider, SearchQuery};
use vam_web::ProviderError;

struct PublicResolver;

impl HostResolver for PublicResolver {
    fn lookup(&self, _host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
        Ok(vec![SocketAddr::new(
            "93.184.216.34".parse().unwrap(),
            port,
        )])
    }
}

/// Answers 404 for robots.txt and a small page for everything else.
async fn start_server() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                let Ok(n) = sock.read(&mut buf).await else {
                    return;
                };
                let head = String::from_utf8_lossy(&buf[..n]).to_string();
                let reply = if head.contains("GET /robots.txt") {
                    "HTTP/1.1 404 X\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
                } else {
                    let body = "<html><body><p>through the pin</p></body></html>";
                    format!(
                        "HTTP/1.1 200 X\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                };
                let _ = sock.write_all(reply.as_bytes()).await;
                let _ = sock.shutdown().await;
            });
        }
    });
    port
}

#[tokio::test]
async fn fetch_ignores_proxy_environment_variables() {
    // A dead proxy everywhere in the environment, and no NO_PROXY: if the
    // fetch client honoured proxy variables it would dial 127.0.0.1:9 (and a
    // real proxy would resolve names itself, bypassing the pinned addresses).
    for key in [
        "HTTP_PROXY",
        "http_proxy",
        "HTTPS_PROXY",
        "https_proxy",
        "ALL_PROXY",
        "all_proxy",
    ] {
        std::env::set_var(key, "http://127.0.0.1:9");
    }
    for key in ["NO_PROXY", "no_proxy"] {
        std::env::remove_var(key);
    }
    let port = start_server().await;
    let fetcher = NetworkFetcher::for_tests(
        Arc::new(PublicResolver),
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        "https://docs.test/web",
    );
    let outcome = fetcher
        .fetch(&format!("http://a.test:{port}/p"), Duration::from_secs(10))
        .await
        .expect("fetched through the pinned address, not the proxy");
    match outcome {
        FetchOutcome::Page(page) => assert!(page.markdown.contains("through the pin")),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn brave_ignores_the_search_key_environment_variable() {
    std::env::set_var("BRAVE_API_KEY", "from-env");
    let provider = BraveProvider::with_endpoint("http://127.0.0.1:9/s").expect("provider");
    let query = SearchQuery {
        query: "rust".to_string(),
        recency: None,
        include_domains: vec![],
        exclude_domains: vec![],
        count: 5,
    };
    let err = provider
        .search(&query, &ResolvedSecrets::default(), Duration::from_secs(1))
        .await
        .expect_err("no key");
    assert_eq!(err, ProviderError::MissingSecret);
}
