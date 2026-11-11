//! The `web_fetch` engine: our own fetch on the host (no vendor extract API).
//!
//! `NetworkFetcher` does the real thing: every hop is validated and DNS-pinned
//! with the shared SSRF guard, redirects are followed by hand, robots.txt is
//! honoured, the body is streamed under a hard cap, and the page becomes
//! markdown. `FixtureFetcher` serves pages from files for hermetic runs.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::Semaphore;
use url::Url;

use crate::error::FetchError;

pub mod artifact;
pub mod content;
pub mod fixture;
pub mod network;
pub mod preview;
pub mod robots;
pub mod rules;

pub use fixture::FixtureFetcher;
pub use network::NetworkFetcher;

/// Product token in the User-Agent and in robots.txt matching.
pub const USER_AGENT_PRODUCT: &str = "VAM-WebFetch";
/// Where the User-Agent points by default: the Web broker docs page (`--docs-url` overrides it).
pub const DEFAULT_DOCS_URL: &str = "https://docs.squrell.dev/hosted/web/";
/// Raw body cap, bytes (after any content decoding).
pub const MAX_RAW_BYTES: usize = 5 * 1024 * 1024;
/// Whole call, all hops and the body.
pub const TOTAL_TIMEOUT: Duration = Duration::from_secs(20);
/// TCP connect.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Conversions that may run at the same time (each can hold tens of MiB).
pub const MAX_CONCURRENT_CONVERSIONS: usize = 4;

/// A fetched, converted page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page {
    /// The URL the model asked for, normalized.
    pub requested_url: String,
    /// The URL that answered, after same-host redirects.
    pub final_url: Url,
    pub title: String,
    pub markdown: String,
    pub content_type: String,
    pub raw_bytes: usize,
    pub retrieved_at: String,
}

/// What a fetch produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchOutcome {
    Page(Box<Page>),
    /// The site redirects to another host. Not followed; the model may call
    /// `web_fetch` again with `redirect_to`.
    Redirected {
        requested_url: String,
        final_url: String,
        redirect_to: String,
    },
}

/// Source of pages for `web_fetch`.
#[async_trait]
pub trait PageFetcher: Send + Sync {
    /// Fetch `url` within `timeout` (capped at [`TOTAL_TIMEOUT`]).
    async fn fetch(&self, url: &str, timeout: Duration) -> Result<FetchOutcome, FetchError>;
}

/// Run the CPU-bound conversion on a blocking thread, a few at a time.
pub(crate) async fn convert_blocking(
    permits: &Arc<Semaphore>,
    body: Vec<u8>,
    content_type: Option<String>,
    base: Url,
) -> Result<content::Converted, FetchError> {
    let permit = permits
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| FetchError::not_accessible("the fetch engine is shutting down"))?;
    // The permit moves into the thread: if the call times out and stops
    // waiting, the work still counts against the limit until it ends.
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        content::convert(&body, content_type.as_deref(), &base)
    })
    .await
    .map_err(|_| FetchError::not_accessible("the page could not be read"))?
}
