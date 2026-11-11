//! First-party HSP schedule broker (`vam.schedule`).
//!
//! Runs as its own process and talks to `vamd` over HSP (tools) and the trigger gateway (fires).
//! This wave is the process: flags, UDS serve, single-instance lock, the SQLite store and the
//! gateway client. The tools still refuse every call, so the end-to-end scenarios for the broker
//! fail at the step that needs it, not while arranging.

pub mod broker;
pub mod clock;
pub mod config;
pub mod error;
pub mod fire;
pub mod gateway;
pub mod grant;
pub mod idempotency;
pub mod manage;
pub mod manifest;
pub mod ops;
pub mod serve;
pub mod state;
pub mod store;
pub mod tools;
pub mod when;

pub use broker::ScheduleBroker;
pub use clock::{Clock, SystemClock, TestClock};
pub use error::ScheduleError;
pub use fire::{Gateway, Scheduler};
pub use when::{When, WhenError};
