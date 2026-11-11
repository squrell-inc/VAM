//! Private copies of a running session's workspace image (turn commits).
//!
//! A turn commit of a microVM session is a copy of the guest's ext4 image taken
//! while the guest holds its filesystem frozen (see `vam-runtime`'s `freeze`).
//! [`snapshot_image`] makes that copy:
//!
//! * a reflink (`FICLONE`) when the volume supports it: copy-on-write, so the
//!   cost does not grow with the image;
//! * otherwise a hole-preserving copy ([`crate::sparse`]) that must finish within
//!   a deadline, so a copy never outlives the guest's thaw. A copy that passes
//!   its deadline is deleted and refused.
//!
//! The copy is a new inode, created `O_EXCL` with mode 0600 and never given the
//! source's permissions or owner. It is never staged into a jail: no writable
//! inode is shared with a VMM (isolation-truth W4b). The host reads it later
//! with the safe reader ([`crate::image_read`]); the guest never sees it.

use crate::error::{io_at, Drive9Error};
use std::fs::{File, OpenOptions};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::time::{Duration, Instant};

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;

/// Largest apparent image size a turn copy is taken of.
pub const MAX_IMAGE_APPARENT_BYTES: u64 = 16 * GIB;
/// Free space that must remain after a non-reflink copy.
pub const FREE_SPACE_MARGIN_BYTES: u64 = 512 * MIB;
/// A non-reflink copy that is not done by then is deleted and refused. The
/// guest's thaw watchdog fires at 125 s; the copy must be gone well before.
pub const COPY_DEADLINE: Duration = Duration::from_secs(100);

/// `FICLONE` is `_IOW(0x94, 9, int)` from `<linux/fs.h>`.
#[cfg(target_os = "linux")]
const FICLONE: libc::c_ulong = 0x4004_9409;

/// How a copy was made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotMethod {
    Reflink,
    SparseCopy,
}

impl SnapshotMethod {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Reflink => "reflink",
            Self::SparseCopy => "sparse-copy",
        }
    }
}

/// What a copy cost.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotReport {
    pub method: SnapshotMethod,
    /// Apparent size of the image.
    pub apparent_bytes: u64,
    /// Bytes the volume allocated for the source (what a non-reflink copy
    /// writes); a reflink shares them.
    pub allocated_bytes: u64,
    pub elapsed_ms: u128,
}

impl SnapshotReport {
    /// Allocated size in MiB, as the console line prints it.
    pub fn copied_mib(&self) -> u64 {
        match self.method {
            SnapshotMethod::Reflink => 0,
            SnapshotMethod::SparseCopy => self.allocated_bytes / MIB,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SnapshotLimits {
    pub max_apparent_bytes: u64,
    pub free_space_margin: u64,
    pub deadline: Duration,
    /// Try `FICLONE` first. Tests turn it off to reach the copy path.
    pub try_reflink: bool,
    /// Free bytes to assume instead of asking the volume (tests).
    pub free_space_override: Option<u64>,
}

impl Default for SnapshotLimits {
    fn default() -> Self {
        Self {
            max_apparent_bytes: MAX_IMAGE_APPARENT_BYTES,
            free_space_margin: FREE_SPACE_MARGIN_BYTES,
            deadline: COPY_DEADLINE,
            try_reflink: true,
            free_space_override: None,
        }
    }
}

/// Free bytes an unprivileged writer can use on the volume holding `path`.
fn free_bytes(path: &Path) -> std::io::Result<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    // SAFETY: `c` is a valid NUL-terminated path and `stat` is a plain-old-data
    // out parameter that statvfs fills in on success.
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(c.as_ptr(), &mut stat) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok((stat.f_bavail as u64).saturating_mul(stat.f_frsize as u64))
}

/// Tries to reflink `src` into `dst`. `Ok(true)` when it worked, `Ok(false)`
/// when the volume (or the pair of files) cannot, `Err` for anything else.
#[cfg(target_os = "linux")]
fn try_ficlone(src: &File, dst: &File) -> std::io::Result<bool> {
    // SAFETY: FICLONE takes the source descriptor as its argument; no memory is
    // touched.
    let rc = unsafe { libc::ioctl(dst.as_raw_fd(), FICLONE as _, src.as_raw_fd()) };
    if rc == 0 {
        return Ok(true);
    }
    let err = std::io::Error::last_os_error();
    match err.raw_os_error() {
        Some(libc::EOPNOTSUPP) | Some(libc::EXDEV) | Some(libc::ENOTTY) | Some(libc::EINVAL) => {
            Ok(false)
        }
        _ => Err(err),
    }
}

#[cfg(not(target_os = "linux"))]
fn try_ficlone(_src: &File, _dst: &File) -> std::io::Result<bool> {
    Ok(false)
}

/// Makes a private copy of the image at `src` as the new file `dst`.
///
/// `dst` must not exist (`O_EXCL`); it is created 0600 and owned by the caller.
/// On any error nothing is left at `dst`. Blocking: callers run it on a
/// blocking thread while the guest waits.
pub fn snapshot_image(
    src: &Path,
    dst: &Path,
    limits: &SnapshotLimits,
) -> Result<SnapshotReport, Drive9Error> {
    let started = Instant::now();
    let mut input = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(src)
        .map_err(|e| Drive9Error::from(e).at("snapshot.open_source", src))?;
    let meta = io_at(input.metadata(), "snapshot.stat_source", src)?;
    if !meta.is_file() {
        return Err(Drive9Error::Layout(
            "the image to copy is not a regular file".into(),
        ));
    }
    let apparent = meta.len();
    if apparent > limits.max_apparent_bytes {
        return Err(Drive9Error::FileTooLarge {
            size: apparent,
            max: limits.max_apparent_bytes,
        });
    }
    let allocated = meta.blocks().saturating_mul(512);

    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(dst)
        .map_err(|e| Drive9Error::from(e).at("snapshot.create_copy", dst))?;

    let result = copy_into(&mut input, &mut output, dst, apparent, allocated, limits);
    match result {
        Ok(method) => {
            // The deadline binds the whole operation, not only the loop: a copy that
            // finished late is as bad as one that did not finish.
            if method == SnapshotMethod::SparseCopy && started.elapsed() > limits.deadline {
                drop(output);
                let _ = std::fs::remove_file(dst);
                return Err(Drive9Error::DeadlineExceeded(format!(
                    "finished after {} ms",
                    started.elapsed().as_millis()
                )));
            }
            Ok(SnapshotReport {
                method,
                apparent_bytes: apparent,
                allocated_bytes: allocated,
                elapsed_ms: started.elapsed().as_millis(),
            })
        }
        Err(err) => {
            drop(output);
            let _ = std::fs::remove_file(dst);
            Err(err)
        }
    }
}

fn copy_into(
    input: &mut File,
    output: &mut File,
    dst: &Path,
    apparent: u64,
    allocated: u64,
    limits: &SnapshotLimits,
) -> Result<SnapshotMethod, Drive9Error> {
    if limits.try_reflink && io_at(try_ficlone(input, output), "snapshot.reflink", dst)? {
        return Ok(SnapshotMethod::Reflink);
    }
    // A copy writes the source's allocated bytes: there must be room for them and
    // for a margin the node keeps for everything else.
    let parent = dst.parent().unwrap_or(dst);
    let free = match limits.free_space_override {
        Some(free) => free,
        None => io_at(free_bytes(parent), "snapshot.free_space", parent)?,
    };
    let needed = allocated.saturating_add(limits.free_space_margin);
    if free < needed {
        return Err(Drive9Error::InsufficientSpace { needed, free });
    }
    let deadline = Instant::now() + limits.deadline;
    match crate::sparse::copy_sparse_into(input, output, apparent, Some(deadline)) {
        Ok(()) => Ok(SnapshotMethod::SparseCopy),
        Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
            Err(Drive9Error::DeadlineExceeded(e.to_string()))
        }
        Err(e) => Err(Drive9Error::from(e).at("snapshot.copy", dst)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Seek, SeekFrom, Write};
    use std::os::unix::fs::PermissionsExt;

    fn sparse_image(path: &Path, len: u64) {
        let mut f = File::create(path).expect("create");
        f.set_len(len).expect("len");
        f.write_all(b"head data").expect("write");
        f.seek(SeekFrom::Start(len / 2 + 5)).expect("seek");
        f.write_all(b"middle data").expect("write");
        f.seek(SeekFrom::Start(len - 4)).expect("seek");
        f.write_all(b"tail").expect("write");
    }

    fn copy_limits() -> SnapshotLimits {
        SnapshotLimits {
            try_reflink: false,
            ..SnapshotLimits::default()
        }
    }

    #[test]
    fn the_ficlone_number_matches_linux_fs_h() {
        // _IOW(0x94, 9, int): direction write (1), size 4, type 0x94, nr 9.
        assert_eq!(FICLONE, (1 << 30) | (4 << 16) | (0x94 << 8) | 9);
    }

    #[test]
    fn a_copy_is_a_private_0600_file_with_its_own_inode_and_equal_bytes() {
        let dir = tempfile::tempdir().expect("dir");
        let src = dir.path().join("drive9.ext4");
        let dst = dir.path().join("0001.ext4");
        sparse_image(&src, 8 * MIB);
        // A source with permissions a copy must not inherit.
        std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o666)).expect("chmod");
        let report = snapshot_image(&src, &dst, &copy_limits()).expect("snapshot");
        assert_eq!(report.method, SnapshotMethod::SparseCopy);
        assert_eq!(report.apparent_bytes, 8 * MIB);
        let (a, b) = (
            std::fs::metadata(&src).expect("src"),
            std::fs::metadata(&dst).expect("dst"),
        );
        assert_ne!(a.ino(), b.ino(), "the copy must be a new inode");
        assert_eq!(b.permissions().mode() & 0o777, 0o600);
        assert_eq!(b.nlink(), 1);
        assert_eq!(
            std::fs::read(&src).expect("read"),
            std::fs::read(&dst).expect("read")
        );
    }

    #[test]
    fn a_sparse_source_stays_sparse() {
        let dir = tempfile::tempdir().expect("dir");
        let src = dir.path().join("a.ext4");
        let dst = dir.path().join("b.ext4");
        sparse_image(&src, 64 * MIB);
        snapshot_image(&src, &dst, &copy_limits()).expect("snapshot");
        let allocated = std::fs::metadata(&dst).expect("meta").blocks() * 512;
        assert!(allocated < 8 * MIB, "the copy allocated {allocated} bytes");
    }

    #[test]
    fn the_method_is_reported_and_reflink_is_tried_first() {
        let dir = tempfile::tempdir().expect("dir");
        let src = dir.path().join("a.ext4");
        sparse_image(&src, 4 * MIB);
        let copied = snapshot_image(&src, &dir.path().join("c.ext4"), &copy_limits()).expect("c");
        assert_eq!(copied.method.as_str(), "sparse-copy");
        // With reflink allowed the volume decides; either way the bytes are equal.
        let tried = snapshot_image(&src, &dir.path().join("r.ext4"), &SnapshotLimits::default())
            .expect("r");
        assert!(matches!(
            tried.method,
            SnapshotMethod::Reflink | SnapshotMethod::SparseCopy
        ));
        assert_eq!(
            std::fs::read(&src).expect("src"),
            std::fs::read(dir.path().join("r.ext4")).expect("dst")
        );
    }

    #[test]
    fn a_copy_past_its_deadline_leaves_no_file() {
        let dir = tempfile::tempdir().expect("dir");
        let src = dir.path().join("a.ext4");
        let dst = dir.path().join("b.ext4");
        // Dense data, so the loop has blocks to copy.
        std::fs::write(&src, vec![7u8; 4 * MIB as usize]).expect("write");
        let limits = SnapshotLimits {
            deadline: Duration::ZERO,
            ..copy_limits()
        };
        let err = snapshot_image(&src, &dst, &limits).expect_err("late");
        assert!(matches!(err, Drive9Error::DeadlineExceeded(_)), "{err:?}");
        assert!(!dst.exists(), "a partial copy must not stay");
    }

    #[test]
    fn too_little_free_space_refuses_before_copying() {
        let dir = tempfile::tempdir().expect("dir");
        let src = dir.path().join("a.ext4");
        let dst = dir.path().join("b.ext4");
        std::fs::write(&src, vec![7u8; MIB as usize]).expect("write");
        let limits = SnapshotLimits {
            free_space_override: Some(FREE_SPACE_MARGIN_BYTES + MIB / 2),
            ..copy_limits()
        };
        let err = snapshot_image(&src, &dst, &limits).expect_err("full");
        assert!(
            matches!(err, Drive9Error::InsufficientSpace { .. }),
            "{err:?}"
        );
        assert!(!dst.exists());
        // Enough room for the allocated bytes plus the margin passes.
        let ok = SnapshotLimits {
            free_space_override: Some(FREE_SPACE_MARGIN_BYTES + 64 * MIB),
            ..copy_limits()
        };
        snapshot_image(&src, &dst, &ok).expect("room");
    }

    #[test]
    fn an_image_over_the_size_cap_is_refused_without_a_file() {
        let dir = tempfile::tempdir().expect("dir");
        let src = dir.path().join("a.ext4");
        let dst = dir.path().join("b.ext4");
        let f = File::create(&src).expect("create");
        // A sparse file whose apparent size is over the (lowered) cap.
        f.set_len(2 * MIB).expect("len");
        let limits = SnapshotLimits {
            max_apparent_bytes: MIB,
            ..copy_limits()
        };
        let err = snapshot_image(&src, &dst, &limits).expect_err("big");
        assert!(matches!(err, Drive9Error::FileTooLarge { .. }), "{err:?}");
        assert!(!dst.exists());
        assert_eq!(MAX_IMAGE_APPARENT_BYTES, 16 * GIB);
    }

    #[test]
    fn an_existing_destination_or_a_missing_or_linked_source_is_refused() {
        let dir = tempfile::tempdir().expect("dir");
        let src = dir.path().join("a.ext4");
        let dst = dir.path().join("b.ext4");
        sparse_image(&src, MIB);
        std::fs::write(&dst, b"mine").expect("pre-existing");
        assert!(snapshot_image(&src, &dst, &copy_limits()).is_err());
        assert_eq!(std::fs::read(&dst).expect("kept"), b"mine");
        let missing = dir.path().join("none.ext4");
        assert!(snapshot_image(&missing, &dir.path().join("c"), &copy_limits()).is_err());
        let link = dir.path().join("link.ext4");
        std::os::unix::fs::symlink(&src, &link).expect("symlink");
        assert!(snapshot_image(&link, &dir.path().join("d"), &copy_limits()).is_err());
        assert!(!dir.path().join("d").exists());
    }
}
