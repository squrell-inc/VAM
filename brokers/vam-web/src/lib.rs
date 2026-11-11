//! First-party HSP web broker (`vam.web`).
//!
//! Runs as its own process and talks to `vamd` only over HSP. It exposes
//! `web_search` (through a [`provider::SearchProvider`]) and `web_fetch`
//! (the [`fetch`] engine: our own guarded fetch on the host). Provider keys
//! arrive per invoke in
//! `InvokeRequest.resolved_secrets`; the broker never reads them from the
//! environment or from disk.

pub mod broker;
pub mod error;
pub mod fetch;
pub mod provider;
pub mod serve;
pub mod tools;

pub use broker::WebBroker;
pub use error::{FetchError, ProviderError};
