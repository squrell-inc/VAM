use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum NotifyError {
    #[error("another vam-notify-broker already holds {}", path.display())]
    Locked { path: PathBuf },
    #[error("{}: {message}", path.display())]
    Io { path: PathBuf, message: String },
    #[error("delivery log: {0}")]
    Store(String),
}

impl NotifyError {
    pub fn io(path: &Path, e: std::io::Error) -> Self {
        Self::Io {
            path: path.to_path_buf(),
            message: e.to_string(),
        }
    }
}
