//! Deterministic fixture-backed provider for hermetic tests and demos.
//!
//! For a query, reads `<dir>/<key>.json` where `key` is the first 16 hex
//! characters of the SHA-256 of the trimmed, lowercased query; otherwise
//! `<dir>/default.json`. A fixture is
//! `{"results":[{"title","url","snippet","published"?}]}`. The mock needs no
//! secret and makes no network call.

use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use super::{Hit, ResolvedSecrets, SearchProvider, SearchQuery};
use crate::error::ProviderError;

/// Fixture directory provider.
pub struct MockProvider {
    dir: PathBuf,
}

#[derive(Debug, Deserialize)]
struct Fixture {
    results: Vec<FixtureHit>,
}

#[derive(Debug, Deserialize)]
struct FixtureHit {
    title: String,
    url: String,
    #[serde(default)]
    snippet: String,
    published: Option<String>,
}

impl MockProvider {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// File stem used for a query.
    pub fn fixture_key(query: &str) -> String {
        let digest = Sha256::digest(query.trim().to_lowercase().as_bytes());
        digest.iter().take(8).map(|b| format!("{b:02x}")).collect()
    }
}

#[async_trait]
impl SearchProvider for MockProvider {
    fn name(&self) -> &'static str {
        "mock"
    }

    async fn search(
        &self,
        query: &SearchQuery,
        _secrets: &ResolvedSecrets,
        _deadline: Duration,
    ) -> Result<Vec<Hit>, ProviderError> {
        let specific = self
            .dir
            .join(format!("{}.json", Self::fixture_key(&query.query)));
        let path = if specific.is_file() {
            specific
        } else {
            self.dir.join("default.json")
        };
        let bytes = tokio::fs::read(&path).await.map_err(|_| {
            ProviderError::Fixture(format!(
                "no fixture for this query and no default.json in {}",
                self.dir.display()
            ))
        })?;
        let fixture: Fixture = serde_json::from_slice(&bytes)
            .map_err(|e| ProviderError::Fixture(format!("{}: {e}", path.display())))?;
        Ok(fixture
            .results
            .into_iter()
            .take(query.count as usize)
            .map(|r| Hit {
                title: r.title,
                url: r.url,
                published: r.published,
                snippet: r.snippet,
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(text: &str, count: u8) -> SearchQuery {
        SearchQuery {
            query: text.to_string(),
            recency: None,
            include_domains: vec![],
            exclude_domains: vec![],
            count,
        }
    }

    #[tokio::test]
    async fn default_and_specific_fixtures_are_deterministic() {
        let dir = tempfile::tempdir().expect("tmp");
        std::fs::write(
            dir.path().join("default.json"),
            r#"{"results":[{"title":"A","url":"https://a.example/","snippet":"sa"},{"title":"B","url":"https://b.example/","snippet":"sb","published":"2026-01-02"}]}"#,
        )
        .expect("write");
        let key = MockProvider::fixture_key("  Special Query ");
        std::fs::write(
            dir.path().join(format!("{key}.json")),
            r#"{"results":[{"title":"S","url":"https://s.example/","snippet":"ss"}]}"#,
        )
        .expect("write");
        let provider = MockProvider::new(dir.path().to_path_buf());
        let secrets = ResolvedSecrets::default();
        let d = Duration::from_secs(1);

        let first = provider
            .search(&q("anything", 10), &secrets, d)
            .await
            .expect("ok");
        let second = provider
            .search(&q("anything", 10), &secrets, d)
            .await
            .expect("ok");
        assert_eq!(first, second);
        assert_eq!(first.len(), 2);
        assert_eq!(first[1].published.as_deref(), Some("2026-01-02"));

        let limited = provider
            .search(&q("anything", 1), &secrets, d)
            .await
            .expect("ok");
        assert_eq!(limited.len(), 1);

        let special = provider
            .search(&q("special query", 10), &secrets, d)
            .await
            .expect("ok");
        assert_eq!(special[0].title, "S");
    }

    #[tokio::test]
    async fn missing_fixture_is_typed() {
        let dir = tempfile::tempdir().expect("tmp");
        let provider = MockProvider::new(dir.path().to_path_buf());
        let err = provider
            .search(
                &q("x", 3),
                &ResolvedSecrets::default(),
                Duration::from_secs(1),
            )
            .await
            .expect_err("no fixture");
        assert_eq!(err.code(), "fixture_error");
    }
}
