//! Provider prompt caching through one mechanism (AR-2.11).
//!
//! The host builds a provider-neutral [`CachePlan`] per request from the
//! catalog entry and a host-derived [`CacheKey`]. Adapters only translate the
//! plan into their wire format. Nothing in the plan comes from the guest: the
//! request's messages, tools and `context_hint` are never read here.

use sha2::{Digest, Sha256};

use crate::catalog::ModelCatalogEntry;

/// How a provider (or gateway) takes the plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CacheMode {
    /// Explicit `cache_control` markers: a static one after tools and system
    /// and a moving one on the conversation.
    ExplicitBreakpoints,
    /// Automatic prefix caching steered by a `prompt_cache_key` (OpenAI).
    PromptKey,
    /// The gateway adds the markers when asked (Vercel `caching: "auto"`).
    GatewayAuto,
    /// The provider caches a byte-stable prefix by itself: no fields are sent.
    /// An unknown provider is `Implicit`.
    #[default]
    Implicit,
}

/// How long the static prefix is kept. The moving breakpoint is always short.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CacheRetention {
    /// Provider default: 5 minutes (Anthropic) or in-memory (OpenAI).
    #[default]
    Short,
    /// 1 hour (Anthropic) or `24h` (OpenAI, where the model allows it).
    Long,
}

/// A marker's time to live.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheTtl {
    Short,
    Long,
}

/// Opaque, host-derived cache key. Owner, agent digest and thread are hashed,
/// so no id appears in clear text at the provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheKey(String);

impl CacheKey {
    /// `vam1-` plus 40 hex characters of a domain-separated SHA-256 over the
    /// length-prefixed parts.
    pub fn derive(owner: &str, agent_digest: &str, thread: &str) -> Self {
        let mut h = Sha256::new();
        h.update(b"vam.cache-key.v1");
        for part in [owner, agent_digest, thread] {
            h.update((part.len() as u64).to_be_bytes());
            h.update(part.as_bytes());
        }
        let hex: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
        Self(format!("vam1-{}", &hex[..40]))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Resolves the key when a request arrives (the thread may not exist when the
/// broker is built). Set by `vamd` only.
pub type CacheKeySource = std::sync::Arc<dyn Fn() -> Option<CacheKey> + Send + Sync>;

/// The cache controls of one request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachePlan {
    pub mode: CacheMode,
    /// Static breakpoint after tools and system.
    pub static_ttl: CacheTtl,
    /// Moving breakpoint on the conversation.
    pub moving_ttl: CacheTtl,
    pub key: Option<CacheKey>,
    pub retention: CacheRetention,
}

impl CachePlan {
    /// No fields at all.
    pub fn implicit() -> Self {
        Self {
            mode: CacheMode::Implicit,
            static_ttl: CacheTtl::Short,
            moving_ttl: CacheTtl::Short,
            key: None,
            retention: CacheRetention::Short,
        }
    }

    /// The plan for a catalog entry and a host key. Reads nothing else.
    pub fn for_entry(entry: &ModelCatalogEntry, key: Option<CacheKey>) -> Self {
        Self {
            mode: entry.cache_mode,
            static_ttl: match entry.cache_retention {
                CacheRetention::Long => CacheTtl::Long,
                CacheRetention::Short => CacheTtl::Short,
            },
            moving_ttl: CacheTtl::Short,
            key,
            retention: entry.cache_retention,
        }
    }

    /// The same request without any cache field (the downgrade retry).
    pub fn downgraded(&self) -> Self {
        Self::implicit()
    }
}

/// Default mode for a provider and model id (dynamic gateway entries).
pub fn default_cache_mode(provider: &str, model: &str) -> CacheMode {
    match provider {
        "anthropic" => CacheMode::ExplicitBreakpoints,
        "openai" => CacheMode::PromptKey,
        "openrouter" if model.starts_with("anthropic/") => CacheMode::ExplicitBreakpoints,
        "vercel" if model.starts_with("anthropic/") || model.starts_with("minimax/") => {
            CacheMode::GatewayAuto
        }
        _ => CacheMode::Implicit,
    }
}

/// Prompt-cache tokens of one inference.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CacheTokens {
    pub read: i64,
    pub written: i64,
    /// The provider reported no cache usage: `read` is unknown, not zero.
    pub unknown: bool,
}

/// True when a provider's 400 body names a cache field, so one retry without
/// cache fields is worth trying.
pub fn is_cache_related_error(details: &str) -> bool {
    let d = details.to_ascii_lowercase();
    d.contains("cache_control")
        || d.contains("prompt_cache")
        || d.contains("provideroptions")
        || d.contains("caching")
}

static DOWNGRADED: std::sync::Mutex<Vec<(String, String)>> = std::sync::Mutex::new(Vec::new());

/// Records that a provider rejected the cache fields for a model: the request
/// was retried once without them and later requests skip them. Logged once.
pub fn note_downgrade(provider: &str, model: &str, detail: &str) {
    let mut seen = DOWNGRADED.lock().unwrap_or_else(|e| e.into_inner());
    let entry = (provider.to_string(), model.to_string());
    if !seen.contains(&entry) {
        let brief: String = detail.chars().take(200).collect();
        eprintln!(
            "[inference-broker] cache fields rejected by {provider}/{model}; retrying without them and sending none from now on: {brief}"
        );
        seen.push(entry);
    }
}

/// True once [`note_downgrade`] was called for this provider and model.
pub fn is_downgraded(provider: &str, model: &str) -> bool {
    let seen = DOWNGRADED.lock().unwrap_or_else(|e| e.into_inner());
    seen.iter().any(|(p, m)| p == provider && m == model)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_is_stable_hashed_and_separated() {
        let a = CacheKey::derive("alice@example.com", "abc123", "thread-1");
        assert_eq!(
            a,
            CacheKey::derive("alice@example.com", "abc123", "thread-1")
        );
        assert!(a.as_str().starts_with("vam1-"));
        assert_eq!(a.as_str().len(), 45);
        for clear in ["alice", "abc123", "thread-1"] {
            assert!(!a.as_str().contains(clear));
        }
        // Moving a boundary between parts changes the key.
        assert_ne!(
            a,
            CacheKey::derive("alice@example.comabc", "123", "thread-1")
        );
        assert_ne!(
            a,
            CacheKey::derive("alice@example.com", "abc123", "thread-2")
        );
    }

    #[test]
    fn default_modes() {
        assert_eq!(
            default_cache_mode("anthropic", "x"),
            CacheMode::ExplicitBreakpoints
        );
        assert_eq!(default_cache_mode("openai", "gpt-4o"), CacheMode::PromptKey);
        assert_eq!(
            default_cache_mode("openrouter", "anthropic/claude-x"),
            CacheMode::ExplicitBreakpoints
        );
        assert_eq!(
            default_cache_mode("openrouter", "openai/gpt-4o"),
            CacheMode::Implicit
        );
        assert_eq!(
            default_cache_mode("vercel", "minimax/m"),
            CacheMode::GatewayAuto
        );
        assert_eq!(
            default_cache_mode("vercel", "openai/gpt-4o"),
            CacheMode::Implicit
        );
        assert_eq!(default_cache_mode("ollama", "llama"), CacheMode::Implicit);
    }

    #[test]
    fn cache_error_detection() {
        assert!(is_cache_related_error(
            "Unknown field: prompt_cache_retention"
        ));
        assert!(is_cache_related_error(
            "providerOptions.gateway not allowed"
        ));
        assert!(!is_cache_related_error("invalid api key"));
    }
}
