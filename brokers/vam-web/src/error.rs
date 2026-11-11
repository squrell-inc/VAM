//! Typed provider errors with stable, model-facing codes.

use std::time::Duration;
use thiserror::Error;

/// Failure of a search provider call. Messages never contain a secret or a
/// provider response body.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ProviderError {
    /// The request arguments are not acceptable.
    #[error("{0}")]
    InvalidQuery(String),
    /// The invoke carried no usable key for this provider.
    #[error("the search provider key is not configured for this broker")]
    MissingSecret,
    /// The provider rejected the key (HTTP 401 or 403).
    #[error("the search provider rejected the configured key")]
    ProviderAuth,
    /// The provider kept answering 429 after the allowed retries.
    #[error("the search provider is rate limiting requests")]
    RateLimited { retry_after: Option<Duration> },
    /// The invoke deadline passed.
    #[error("the search provider did not answer in time")]
    Timeout,
    /// Any other non-success HTTP status or transport failure.
    #[error("the search provider failed (status {status})")]
    Upstream { status: u16 },
    /// A 2xx answer that does not have the expected shape.
    #[error("the search provider returned an unexpected response")]
    BadResponse,
    /// Mock provider fixture problem.
    #[error("mock fixture error: {0}")]
    Fixture(String),
}

impl ProviderError {
    /// Stable code the model sees before the colon.
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidQuery(_) => "invalid_query",
            Self::MissingSecret => "provider_not_configured",
            Self::ProviderAuth => "provider_auth",
            Self::RateLimited { .. } => "rate_limited",
            Self::Timeout => "timeout",
            Self::Upstream { .. } | Self::BadResponse => "provider_error",
            Self::Fixture(_) => "fixture_error",
        }
    }

    /// Whether trying again later may succeed.
    pub fn retryable(&self) -> bool {
        matches!(
            self,
            Self::RateLimited { .. } | Self::Timeout | Self::Upstream { .. }
        )
    }

    /// `code: what to do`, the text returned to the model.
    pub fn model_message(&self) -> String {
        let advice = match self {
            Self::InvalidQuery(detail) => detail.clone(),
            Self::MissingSecret => {
                "web search is not set up on this host; tell the user an operator must configure the search key"
                    .to_string()
            }
            Self::ProviderAuth => {
                "the host's search key was refused; tell the user an operator must check it"
                    .to_string()
            }
            Self::RateLimited { .. } => {
                "wait a moment and try once more, or answer from what you already have".to_string()
            }
            Self::Timeout => "try a shorter or simpler query".to_string(),
            Self::Upstream { status } => {
                format!("the search service answered {status}; try again later")
            }
            Self::BadResponse => "try again later".to_string(),
            Self::Fixture(detail) => detail.clone(),
        };
        format!("{}: {}", self.code(), advice)
    }
}

/// Failure of a `web_fetch` call. Messages are composed here, never copied
/// from the remote server, and carry no secret.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum FetchError {
    /// The URL is not an acceptable http(s) address.
    #[error("{0}")]
    InvalidUrl(String),
    /// The URL is longer than the allowed maximum.
    #[error("the URL is too long")]
    UrlTooLong,
    /// The security guard refused the address or a redirect.
    #[error("{0}")]
    Blocked(String),
    /// robots.txt of the site disallows the path.
    #[error("robots.txt disallows fetching this path")]
    BlockedByRobots,
    /// The raw body is larger than the limit.
    #[error("the page is larger than the limit")]
    TooLarge,
    /// The call deadline passed.
    #[error("the site did not answer in time")]
    Timeout,
    /// The server sent a type this tool does not read.
    #[error("unsupported content type {0}")]
    UnsupportedContentType(String),
    /// HTTP 429.
    #[error("the site is rate limiting requests")]
    RateLimited,
    /// Any other failure to get the page: status, DNS, connection, TLS,
    /// redirect problems, an empty page.
    #[error("{detail}")]
    NotAccessible { detail: String, retryable: bool },
}

impl FetchError {
    /// A failure that retrying cannot fix.
    pub fn not_accessible(detail: impl Into<String>) -> Self {
        Self::NotAccessible {
            detail: detail.into(),
            retryable: false,
        }
    }

    /// A failure that may pass (server error, connection trouble).
    pub fn temporarily_not_accessible(detail: impl Into<String>) -> Self {
        Self::NotAccessible {
            detail: detail.into(),
            retryable: true,
        }
    }

    /// Stable code the model sees before the colon.
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidUrl(_) => "invalid_url",
            Self::UrlTooLong => "url_too_long",
            Self::Blocked(_) => "blocked",
            Self::BlockedByRobots => "blocked_by_robots",
            Self::TooLarge => "too_large",
            Self::Timeout => "timeout",
            Self::UnsupportedContentType(_) => "unsupported_content_type",
            Self::RateLimited => "rate_limited",
            Self::NotAccessible { .. } => "not_accessible",
        }
    }

    /// Whether trying again later may succeed.
    pub fn retryable(&self) -> bool {
        match self {
            Self::Timeout | Self::RateLimited => true,
            Self::NotAccessible { retryable, .. } => *retryable,
            _ => false,
        }
    }

    /// Wire code for `ErrorStatus`.
    pub fn vam_code(&self) -> vam_spec::v1::VamErrorCode {
        use vam_spec::v1::VamErrorCode as C;
        match self {
            Self::InvalidUrl(_) | Self::UrlTooLong | Self::UnsupportedContentType(_) => {
                C::InvalidArgument
            }
            Self::Blocked(_) | Self::BlockedByRobots => C::PermissionDenied,
            Self::TooLarge | Self::RateLimited => C::ResourceExhausted,
            Self::Timeout => C::DeadlineExceeded,
            Self::NotAccessible { .. } => C::Unavailable,
        }
    }

    /// `code: what to do`, the text returned to the model.
    pub fn model_message(&self) -> String {
        let advice = match self {
            Self::InvalidUrl(detail) => format!(
                "{detail}; give a full http or https URL without a user name or password, on port 80 or 443"
            ),
            Self::UrlTooLong => format!(
                "the URL is longer than {} characters; use a shorter URL",
                crate::tools::MAX_URL_CHARS
            ),
            Self::Blocked(detail) => format!(
                "{detail}; this address cannot be fetched, do not retry it, use another source"
            ),
            Self::BlockedByRobots => {
                "the site's robots.txt does not allow automated fetching of this path; use another source or tell the user".to_string()
            }
            Self::TooLarge => {
                "the page is larger than 5 MiB; look for a smaller page or a summary of it"
                    .to_string()
            }
            Self::Timeout => {
                "the site was too slow (20 second limit); try once more or use another source"
                    .to_string()
            }
            Self::UnsupportedContentType(kind) => format!(
                "the page is {kind}; web_fetch reads only HTML and plain text (PDF is not supported yet), use another source"
            ),
            Self::RateLimited => {
                "the site is rate limiting requests; wait a moment, or use another source"
                    .to_string()
            }
            Self::NotAccessible { detail, .. } => detail.clone(),
        };
        format!("{}: {}", self.code(), advice)
    }
}
