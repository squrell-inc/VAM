//! Search provider interface. Phase 1 ships Brave and a mock.

use std::collections::HashMap;
use std::fmt;
use std::time::Duration;

use async_trait::async_trait;

use crate::error::ProviderError;

pub mod brave;
pub mod mock;

/// Longest final query string sent to a provider.
pub const MAX_QUERY_CHARS: usize = 400;
/// Largest result count a caller may ask for.
pub const MAX_COUNT: u8 = 20;
/// Result count when the caller gives none.
pub const DEFAULT_COUNT: u8 = 10;

/// How recent results must be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recency {
    Day,
    Week,
    Month,
    Year,
}

/// A validated search request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchQuery {
    pub query: String,
    pub recency: Option<Recency>,
    pub include_domains: Vec<String>,
    pub exclude_domains: Vec<String>,
    pub count: u8,
}

/// One search result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub title: String,
    pub url: String,
    /// Page date as the provider reports it.
    pub published: Option<String>,
    pub snippet: String,
}

/// Secrets the host resolved for one invoke. The `Debug` output never shows
/// a value.
#[derive(Clone, Default)]
pub struct ResolvedSecrets(HashMap<String, String>);

impl ResolvedSecrets {
    pub fn new(map: HashMap<String, String>) -> Self {
        Self(map)
    }

    /// A non-empty value for `logical_name`.
    pub fn get(&self, logical_name: &str) -> Option<&str> {
        self.0
            .get(logical_name)
            .map(String::as_str)
            .filter(|v| !v.is_empty())
    }
}

impl fmt::Debug for ResolvedSecrets {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut names: Vec<&String> = self.0.keys().collect();
        names.sort();
        f.debug_struct("ResolvedSecrets")
            .field("names", &names)
            .finish()
    }
}

/// A search backend. Implementations take their credentials only from the
/// per-invoke `secrets`.
#[async_trait]
pub trait SearchProvider: Send + Sync {
    /// Short provider name for logs and docs.
    fn name(&self) -> &'static str;

    /// Run a search within `deadline`.
    async fn search(
        &self,
        query: &SearchQuery,
        secrets: &ResolvedSecrets,
        deadline: Duration,
    ) -> Result<Vec<Hit>, ProviderError>;
}

/// Replace every occurrence of each non-empty secret in `text` with `***`.
pub fn redact(text: &str, secrets: &ResolvedSecrets) -> String {
    let mut out = text.to_string();
    for value in secrets.0.values() {
        if !value.is_empty() {
            out = out.replace(value.as_str(), "***");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_hides_values() {
        let mut map = HashMap::new();
        map.insert("brave-api-key".to_string(), "s3cret".to_string());
        let secrets = ResolvedSecrets::new(map);
        let shown = format!("{secrets:?}");
        assert!(shown.contains("brave-api-key"));
        assert!(!shown.contains("s3cret"));
    }

    #[test]
    fn redact_replaces_value() {
        let mut map = HashMap::new();
        map.insert("k".to_string(), "abc123".to_string());
        let secrets = ResolvedSecrets::new(map);
        assert_eq!(redact("key=abc123 end abc123", &secrets), "key=*** end ***");
    }

    #[test]
    fn empty_secret_is_absent() {
        let mut map = HashMap::new();
        map.insert("k".to_string(), String::new());
        assert!(ResolvedSecrets::new(map).get("k").is_none());
    }
}
