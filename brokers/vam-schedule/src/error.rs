//! The broker's error type.

use std::path::PathBuf;

/// Everything the store and the state directory can fail with.
#[derive(Debug, thiserror::Error)]
pub enum ScheduleError {
    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("another vam-schedule-broker already uses {}", path.display())]
    Locked { path: PathBuf },
    #[error("migration {version} failed: {message}")]
    Migration { version: u32, message: String },
    #[error("store version {found} is newer than this broker understands ({supported})")]
    StoreTooNew { found: u32, supported: u32 },
    #[error("invalid argument: {0}")]
    InvalidArgument(String),
}

impl ScheduleError {
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }
}
