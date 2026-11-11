//! The state directory: mode 0700, one process per store (an exclusive `flock`).

use std::fs::{File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};

use crate::error::NotifyError;

pub const DB_FILE: &str = "vam-notify.db";
pub const LOCK_FILE: &str = "vam-notify.lock";

/// The opened state directory. Holding it holds the lock.
#[derive(Debug)]
pub struct StateLock {
    dir: PathBuf,
    _lock: File,
}

impl StateLock {
    /// Create the directory (0700), tighten an existing one, take the exclusive lock.
    pub fn open(dir: &Path) -> Result<Self, NotifyError> {
        std::fs::create_dir_all(dir).map_err(|e| NotifyError::io(dir, e))?;
        set_mode(dir, 0o700)?;
        let lock_path = dir.join(LOCK_FILE);
        let file = open_private(&lock_path)?;
        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Err(NotifyError::Locked { path: dir.into() }),
            Err(TryLockError::Error(e)) => return Err(NotifyError::io(&lock_path, e)),
        }
        Ok(Self {
            dir: dir.to_path_buf(),
            _lock: file,
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn db_path(&self) -> PathBuf {
        self.dir.join(DB_FILE)
    }
}

fn open_private(path: &Path) -> Result<File, NotifyError> {
    let mut opts = OpenOptions::new();
    opts.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let file = opts.open(path).map_err(|e| NotifyError::io(path, e))?;
    set_mode(path, 0o600)?;
    Ok(file)
}

fn set_mode(path: &Path, mode: u32) -> Result<(), NotifyError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
            .map_err(|e| NotifyError::io(path, e))?;
    }
    #[cfg(not(unix))]
    let _ = (path, mode);
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn dir_is_0700_and_second_holder_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("state");
        let first = StateLock::open(&dir).unwrap();
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        assert!(matches!(
            StateLock::open(&dir),
            Err(NotifyError::Locked { .. })
        ));
        drop(first);
        StateLock::open(&dir).unwrap();
    }
}
