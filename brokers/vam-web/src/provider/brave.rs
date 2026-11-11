//! Brave Search adapter.
//!
//! `GET https://api.search.brave.com/res/v1/web/search` with the key in the
//! `X-Subscription-Token` header. The key comes only from the per-invoke
//! `resolved_secrets` (logical name [`SECRET_NAME`]); this module never reads
//! the environment or the disk.

use std::time::{Duration, Instant};

use async_trait::async_trait;
use reqwest::header::{HeaderValue, ACCEPT, RETRY_AFTER};
use reqwest::StatusCode;
use serde::Deserialize;

use super::{
    redact, Hit, Recency, ResolvedSecrets, SearchProvider, SearchQuery, MAX_COUNT, MAX_QUERY_CHARS,
};
use crate::error::ProviderError;

/// Production endpoint.
pub const BRAVE_ENDPOINT: &str = "https://api.search.brave.com/res/v1/web/search";
/// Logical secret name the manifest binds to the vault key.
pub const SECRET_NAME: &str = "brave-api-key";
/// 429 answers retried at most this many times.
const MAX_RETRIES: u32 = 2;
/// Longest single wait for a `Retry-After`.
const MAX_RETRY_WAIT: Duration = Duration::from_secs(5);
/// Largest accepted response body.
const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;

/// Brave Search provider.
pub struct BraveProvider {
    client: reqwest::Client,
    endpoint: String,
}

impl BraveProvider {
    /// Provider for the production endpoint.
    pub fn new() -> Result<Self, ProviderError> {
        Self::with_endpoint(BRAVE_ENDPOINT)
    }

    /// Provider for another endpoint (tests point this at a local server).
    pub fn with_endpoint(endpoint: &str) -> Result<Self, ProviderError> {
        let client = reqwest::Client::builder()
            .user_agent("VAM-WebBroker/1")
            .build()
            .map_err(|_| ProviderError::Upstream { status: 0 })?;
        Ok(Self {
            client,
            endpoint: endpoint.to_string(),
        })
    }

    /// The `q` parameter: the query plus domain filters as search operators.
    pub fn build_q(query: &SearchQuery) -> Result<String, ProviderError> {
        let mut q = query.query.trim().to_string();
        match query.include_domains.as_slice() {
            [] => {}
            [one] => q.push_str(&format!(" site:{one}")),
            many => {
                let joined = many
                    .iter()
                    .map(|d| format!("site:{d}"))
                    .collect::<Vec<_>>()
                    .join(" OR ");
                q.push_str(&format!(" ({joined})"));
            }
        }
        for domain in &query.exclude_domains {
            q.push_str(&format!(" -site:{domain}"));
        }
        if q.chars().count() > MAX_QUERY_CHARS {
            return Err(ProviderError::InvalidQuery(format!(
                "the query with its domain filters is longer than {MAX_QUERY_CHARS} characters; shorten it or use fewer domain filters"
            )));
        }
        Ok(q)
    }

    fn freshness(recency: Recency) -> &'static str {
        match recency {
            Recency::Day => "pd",
            Recency::Week => "pw",
            Recency::Month => "pm",
            Recency::Year => "py",
        }
    }
}

#[derive(Debug, Deserialize)]
struct BraveResponse {
    web: Option<BraveWeb>,
}

#[derive(Debug, Deserialize)]
struct BraveWeb {
    #[serde(default)]
    results: Vec<BraveResult>,
}

#[derive(Debug, Deserialize)]
struct BraveResult {
    #[serde(default)]
    title: String,
    url: Option<String>,
    #[serde(default)]
    description: String,
    page_age: Option<String>,
    age: Option<String>,
}

fn parse_retry_after(value: Option<&HeaderValue>) -> Option<Duration> {
    let secs: u64 = value?.to_str().ok()?.trim().parse().ok()?;
    Some(Duration::from_secs(secs))
}

#[async_trait]
impl SearchProvider for BraveProvider {
    fn name(&self) -> &'static str {
        "brave"
    }

    async fn search(
        &self,
        query: &SearchQuery,
        secrets: &ResolvedSecrets,
        deadline: Duration,
    ) -> Result<Vec<Hit>, ProviderError> {
        let key = secrets
            .get(SECRET_NAME)
            .ok_or(ProviderError::MissingSecret)?;
        let mut token = HeaderValue::from_str(key).map_err(|_| ProviderError::ProviderAuth)?;
        token.set_sensitive(true);
        let q = Self::build_q(query)?;
        let count = query.count.clamp(1, MAX_COUNT).to_string();

        let started = Instant::now();
        let mut attempt = 0u32;
        loop {
            let remaining = deadline
                .checked_sub(started.elapsed())
                .filter(|d| !d.is_zero())
                .ok_or(ProviderError::Timeout)?;

            let mut params: Vec<(&str, String)> = vec![
                ("q", q.clone()),
                ("count", count.clone()),
                ("text_decorations", "false".to_string()),
            ];
            if let Some(recency) = query.recency {
                params.push(("freshness", Self::freshness(recency).to_string()));
            }

            let response = self
                .client
                .get(&self.endpoint)
                .header("X-Subscription-Token", token.clone())
                .header(ACCEPT, "application/json")
                .query(&params)
                .timeout(remaining)
                .send()
                .await
                .map_err(|e| {
                    if e.is_timeout() {
                        ProviderError::Timeout
                    } else {
                        ProviderError::Upstream { status: 0 }
                    }
                })?;

            let status = response.status();
            if status == StatusCode::TOO_MANY_REQUESTS {
                let retry_after = parse_retry_after(response.headers().get(RETRY_AFTER));
                let wait = retry_after.unwrap_or(Duration::from_secs(1));
                let left = deadline.saturating_sub(started.elapsed());
                if attempt >= MAX_RETRIES || wait > MAX_RETRY_WAIT || wait >= left {
                    return Err(ProviderError::RateLimited { retry_after });
                }
                attempt += 1;
                tokio::time::sleep(wait).await;
                continue;
            }
            if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
                return Err(ProviderError::ProviderAuth);
            }
            if !status.is_success() {
                return Err(ProviderError::Upstream {
                    status: status.as_u16(),
                });
            }

            let body = response.bytes().await.map_err(|e| {
                if e.is_timeout() {
                    ProviderError::Timeout
                } else {
                    ProviderError::Upstream { status: 0 }
                }
            })?;
            if body.len() > MAX_BODY_BYTES {
                return Err(ProviderError::BadResponse);
            }
            let parsed: BraveResponse =
                serde_json::from_slice(&body).map_err(|_| ProviderError::BadResponse)?;
            let hits = parsed
                .web
                .map(|w| w.results)
                .unwrap_or_default()
                .into_iter()
                .filter_map(|r| {
                    let url = r.url?;
                    // Defence in depth: nothing the provider sends may carry the key.
                    let clean = |s: String| redact(&s, secrets);
                    Some(Hit {
                        title: clean(r.title),
                        url: clean(url),
                        published: r.page_age.or(r.age),
                        snippet: clean(r.description),
                    })
                })
                .take(query.count as usize)
                .collect();
            return Ok(hits);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use httpmock::prelude::*;

    use super::*;

    fn secrets(key: &str) -> ResolvedSecrets {
        let mut map = HashMap::new();
        map.insert(SECRET_NAME.to_string(), key.to_string());
        ResolvedSecrets::new(map)
    }

    fn query() -> SearchQuery {
        SearchQuery {
            query: "rust async".to_string(),
            recency: Some(Recency::Week),
            include_domains: vec!["docs.rs".to_string()],
            exclude_domains: vec!["spam.example".to_string()],
            count: 5,
        }
    }

    const BODY: &str = r#"{"web":{"results":[
        {"title":"Async book","url":"https://rust-lang.github.io/async-book/","description":"Intro","page_age":"2026-09-01T00:00:00"},
        {"title":"No age","url":"https://example.com/a","description":"d","age":"3 days ago"},
        {"title":"No url","description":"skipped"}
    ]}}"#;

    #[test]
    fn q_has_site_operators() {
        let q = BraveProvider::build_q(&query()).expect("q");
        assert_eq!(q, "rust async site:docs.rs -site:spam.example");
        let mut multi = query();
        multi.include_domains = vec!["a.com".into(), "b.org".into()];
        multi.exclude_domains.clear();
        assert_eq!(
            BraveProvider::build_q(&multi).expect("q"),
            "rust async (site:a.com OR site:b.org)"
        );
    }

    #[test]
    fn q_too_long_is_invalid() {
        let mut long = query();
        long.query = "x".repeat(MAX_QUERY_CHARS);
        let err = BraveProvider::build_q(&long).expect_err("too long");
        assert_eq!(err.code(), "invalid_query");
    }

    #[tokio::test]
    async fn request_shape_and_mapping() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET)
                .path("/res/v1/web/search")
                .header("X-Subscription-Token", "k-123")
                .query_param("q", "rust async site:docs.rs -site:spam.example")
                .query_param("count", "5")
                .query_param("freshness", "pw")
                .query_param("text_decorations", "false");
            then.status(200)
                .header("content-type", "application/json")
                .body(BODY);
        });
        let provider = BraveProvider::with_endpoint(&server.url("/res/v1/web/search")).expect("p");
        let hits = provider
            .search(&query(), &secrets("k-123"), Duration::from_secs(5))
            .await
            .expect("hits");
        mock.assert();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].title, "Async book");
        assert_eq!(hits[0].published.as_deref(), Some("2026-09-01T00:00:00"));
        assert_eq!(hits[1].published.as_deref(), Some("3 days ago"));
    }

    #[tokio::test]
    async fn count_is_capped_at_twenty() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET).query_param("count", "20");
            then.status(200).body(r#"{"web":{"results":[]}}"#);
        });
        let provider = BraveProvider::with_endpoint(&server.url("/s")).expect("p");
        let mut q = query();
        q.count = 99;
        provider
            .search(&q, &secrets("k"), Duration::from_secs(5))
            .await
            .expect("ok");
        mock.assert();
    }

    #[tokio::test]
    async fn auth_failure_never_echoes_key() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET);
            then.status(401)
                .body(r#"{"error":"bad token super-secret-key"}"#);
        });
        let provider = BraveProvider::with_endpoint(&server.url("/s")).expect("p");
        let err = provider
            .search(
                &query(),
                &secrets("super-secret-key"),
                Duration::from_secs(5),
            )
            .await
            .expect_err("auth");
        assert_eq!(err, ProviderError::ProviderAuth);
        assert!(!err.model_message().contains("super-secret-key"));
        assert!(!format!("{err:?}").contains("super-secret-key"));
    }

    /// Minimal HTTP server answering each connection from `responses` in order.
    async fn sequence_server(responses: Vec<String>) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            for response in responses {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = vec![0u8; 8192];
                let mut seen = Vec::new();
                while !seen.windows(4).any(|w| w == b"\r\n\r\n") {
                    match socket.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => seen.extend_from_slice(&buf[..n]),
                    }
                }
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            }
        });
        format!("http://{addr}/s")
    }

    #[tokio::test]
    async fn rate_limit_then_success() {
        let limited = "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 0\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string();
        let ok = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            BODY.len(),
            BODY
        );
        let url = sequence_server(vec![limited, ok]).await;
        let provider = BraveProvider::with_endpoint(&url).expect("p");
        let hits = provider
            .search(&query(), &secrets("k"), Duration::from_secs(10))
            .await
            .expect("hits");
        assert_eq!(hits.len(), 2);
    }

    #[tokio::test]
    async fn rate_limit_exhausted_is_typed() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET);
            then.status(429).header("Retry-After", "0");
        });
        let provider = BraveProvider::with_endpoint(&server.url("/s")).expect("p");
        let err = provider
            .search(&query(), &secrets("k"), Duration::from_secs(10))
            .await
            .expect_err("limited");
        assert_eq!(err.code(), "rate_limited");
        // One try plus two retries.
        assert_eq!(mock.hits(), 3);
    }

    #[tokio::test]
    async fn bad_shape_is_provider_error() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET);
            then.status(200).body("not json");
        });
        let provider = BraveProvider::with_endpoint(&server.url("/s")).expect("p");
        let err = provider
            .search(&query(), &secrets("k"), Duration::from_secs(5))
            .await
            .expect_err("bad");
        assert_eq!(err, ProviderError::BadResponse);
    }
}
