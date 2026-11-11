use std::path::PathBuf;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum Drive9Error {
    #[error("workspace path '{0}' is not a directory")]
    WorkspaceNotDirectory(PathBuf),

    #[error("checkpoint name '{0}' is invalid")]
    InvalidCheckpointName(String),

    #[error("path sandbox violation: '{path}' tried to escape root '{root}'")]
    SandboxViolation { path: PathBuf, root: PathBuf },

    #[error("path contains invalid characters (NUL byte or control characters)")]
    InvalidPathCharacters(PathBuf),

    #[error("storage I/O failed: {0}")]
    Io(#[from] std::io::Error),

    #[error("storage I/O failed during {step} at {path}: {source}")]
    IoAt {
        step: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("required image tool is missing: {0}")]
    MissingImageTool(String),

    #[error("SQLite database error: {0}")]
    Database(#[from] rusqlite::Error),

    #[error("serialization/deserialization failed: {0}")]
    Serialization(String),

    #[error("checksum verification failed: expected {expected}, found {actual}")]
    ChecksumMismatch { expected: String, actual: String },

    #[error("disk quota exceeded: limit is {limit} bytes, required {required} bytes")]
    QuotaExceeded { limit: u64, required: u64 },

    #[error(
        "concurrent write conflict: manifest was updated from version {expected:?} to {actual:?}"
    )]
    ConcurrentWriteConflict {
        expected: Option<String>,
        actual: String,
    },

    #[error("workspace layout error: {0}")]
    Layout(String),

    #[error("connection closed")]
    ConnectionClosed,

    #[error("remote error: code {code}, message: {message}")]
    RemoteError { code: i32, message: String },

    #[error("not found: {0}")]
    NotFound(String),

    #[error("file size {size} bytes exceeds maximum {max} bytes")]
    FileTooLarge { size: u64, max: u64 },

    /// The guest-written disk image is corrupt, inconsistent or uses a feature
    /// the reader does not support.
    #[error("guest disk image rejected: {0}")]
    ImageCorrupt(String),

    /// Reading the guest-written disk image hit a bound (entries, bytes, depth, time).
    #[error("guest disk image over a read limit: {0}")]
    ImageLimit(String),

    /// There is not enough free space on the volume for a copy of the image.
    #[error("not enough free space for a turn copy: need {needed} bytes, {free} free")]
    InsufficientSpace { needed: u64, free: u64 },

    /// A turn copy of the image ran past its deadline and was removed.
    #[error("turn copy of the image passed its deadline: {0}")]
    DeadlineExceeded(String),

    /// A manifest holds a file path that is not a plain relative path.
    #[error("manifest path '{0}' is not a plain relative path")]
    InvalidManifestPath(String),
}

impl Drive9Error {
    /// Names the step and path of a plain I/O failure; other errors pass through.
    pub fn at(self, step: &'static str, path: &std::path::Path) -> Self {
        match self {
            Drive9Error::Io(source) => Drive9Error::IoAt {
                step,
                path: path.to_path_buf(),
                source,
            },
            other => other,
        }
    }
}

/// `result` with the step and path named when it is an I/O failure.
pub(crate) fn io_at<T>(
    result: std::io::Result<T>,
    step: &'static str,
    path: &std::path::Path,
) -> Result<T, Drive9Error> {
    result.map_err(|source| Drive9Error::IoAt {
        step,
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_io_failure_names_its_step_and_path_and_other_errors_pass_through() {
        let io = Drive9Error::Io(std::io::Error::from_raw_os_error(122));
        let named = io.at("materialize.clone_cache_file", std::path::Path::new("/s/f"));
        let text = named.to_string();
        assert!(text.contains("materialize.clone_cache_file") && text.contains("/s/f"));
        assert!(matches!(named, Drive9Error::IoAt { .. }));
        let other = Drive9Error::Layout("x".into()).at("step", std::path::Path::new("/p"));
        assert!(matches!(other, Drive9Error::Layout(_)));
    }
}
