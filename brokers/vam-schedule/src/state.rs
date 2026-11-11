//! The state directory: mode 0700, one process per store (an exclusive `flock`).

use std::fs::{File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};

use crate::error::ScheduleError;

pub const DB_FILE: &str = "vam-schedule.db";
pub const LOCK_FILE: &str = "vam-schedule.lock";

/// The opened state directory. Holding it holds the lock; dropping it (or the process ending)
/// releases the lock.
#[derive(Debug)]
pub struct StateLock {
    dir: PathBuf,
    _lock: File,
}

impl StateLock {
    /// Create the directory (0700, parents included), tighten an existing one to 0700, and take
    /// the exclusive lock on `vam-schedule.lock`. A second holder gets [`ScheduleError::Locked`].
    pub fn open(dir: &Path) -> Result<Self, ScheduleError> {
        std::fs::create_dir_all(dir).map_err(|e| ScheduleError::io(dir, e))?;
        set_mode(dir, 0o700)?;
        let lock_path = dir.join(LOCK_FILE);
        let file = open_private(&lock_path)?;
        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                return Err(ScheduleError::Locked { path: dir.into() })
            }
            Err(TryLockError::Error(e)) => return Err(ScheduleError::io(&lock_path, e)),
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

/// Open (creating at 0600) a file for read and write, and force 0600 on an existing one.
pub(crate) fn open_private(path: &Path) -> Result<File, ScheduleError> {
    let mut opts = OpenOptions::new();
    opts.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let file = opts.open(path).map_err(|e| ScheduleError::io(path, e))?;
    set_mode(path, 0o600)?;
    Ok(file)
}

#[cfg(unix)]
pub(crate) fn set_mode(path: &Path, mode: u32) -> Result<(), ScheduleError> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .map_err(|e| ScheduleError::io(path, e))
}

#[cfg(not(unix))]
pub(crate) fn set_mode(_path: &Path, _mode: u32) -> Result<(), ScheduleError> {
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn mode(p: &Path) -> u32 {
        std::fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn creates_nested_dir_0700_and_lock_0600() {
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join("a/state");
        let s = StateLock::open(&dir).unwrap();
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&dir.join(LOCK_FILE)), 0o600);
        assert_eq!(s.db_path(), dir.join(DB_FILE));
    }

    #[test]
    fn tightens_an_existing_loose_dir() {
        let t = tempfile::tempdir().unwrap();
        std::fs::set_permissions(t.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        let _s = StateLock::open(t.path()).unwrap();
        assert_eq!(mode(t.path()), 0o700);
    }

    #[test]
    fn second_holder_is_refused_until_the_first_drops() {
        let t = tempfile::tempdir().unwrap();
        let first = StateLock::open(t.path()).unwrap();
        let err = StateLock::open(t.path()).unwrap_err();
        assert!(matches!(err, ScheduleError::Locked { .. }), "{err}");
        assert!(err.to_string().contains("already uses"));
        drop(first);
        StateLock::open(t.path()).unwrap();
    }
}
