//! First-party HSP notification channel broker (`vam.notify`).
//!
//! Runs as its own process. `vamd` hands it a neutral rendering (`DeliverNotification`); an
//! adapter turns that into a channel message (Slack first). A person's click comes back through
//! the broker to `vamd`'s gateway, which decides; the broker never decides and never produces a
//! record (notifications locked decisions 2 and 6). Delivery, the Slack adapter, the delivery log
//! and the link and click relays are built against the fake transport; the live Slack transport
//! is wave 6.

pub mod adapter;
pub mod broker;
#[cfg(feature = "fake-transport")]
pub mod control;
pub mod error;
pub mod gateway;
pub mod hub;
pub mod live;
pub mod log;
pub mod manifest;
pub mod serve;
pub mod slack;
pub mod state;
pub mod transport;

pub use broker::NotifyBroker;
pub use error::NotifyError;
