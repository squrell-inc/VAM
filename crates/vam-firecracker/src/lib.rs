// Vendored fork of upstream Firecracker code: diverging further from upstream costs more
// than it buys, so style and complexity lints are allowed here. Correctness lints are fixed.
#![allow(
    clippy::redundant_static_lifetimes,
    clippy::bind_instead_of_map,
    clippy::needless_borrow,
    clippy::inherent_to_string,
    clippy::single_char_add_str,
    clippy::unnecessary_to_owned,
    clippy::to_string_trait_impl,
    clippy::unnecessary_lazy_evaluations,
    clippy::doc_lazy_continuation,
    clippy::doc_overindented_list_items,
    clippy::crate_in_macro_def
)]

pub mod agent;
pub mod events;
pub mod firecracker;
pub mod instance;
pub mod jailer;
pub mod models;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("IO: {0}")]
    IO(#[from] std::io::Error),
    #[error("Agent: {0}")]
    Agent(String),
    #[error("Configuraion: {0}")]
    Configuration(String),
    #[error("Event: {0}")]
    Event(String),
    #[error("Instance: {0}")]
    Instance(String),
    #[error("{0}")]
    FeatureNone(String),
}

pub type Result<T> = std::result::Result<T, crate::Error>;

#[macro_export]
macro_rules! missing_rt {
    () => {
        Err(crate::Error::FeatureNone(
            r#"Must enable one of these features: "_rt-std", "_rt-tokio", "_rt-async-std""#.into(),
        ))
    };
}

#[macro_export]
macro_rules! missing_rt_panic {
    () => {
        panic!(r#"Must enable one of these features: "_rt-std", "_rt-tokio", "_rt-async-std""#)
    };
}
