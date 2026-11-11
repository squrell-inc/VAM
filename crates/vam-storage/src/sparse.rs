//! Sparse-aware file copy.
//!
//! A workspace image is a sparse file: a 1 GiB default image holds a few MiB of
//! data. `std::fs::copy` on a filesystem without reflink can write every hole
//! out as zeros, so each clone costs the full apparent size (this is what
//! filled the CI runner's quota). [`copy_sparse`] copies only the data
//! extents the filesystem reports and skips all-zero blocks inside them, so a
//! copy stays as sparse as its source.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::time::Instant;

const BLOCK: usize = 64 * 1024;

fn seek_whence(file: &File, offset: u64, whence: libc::c_int) -> io::Result<Option<u64>> {
    // SAFETY: plain lseek on an open descriptor; no memory is touched.
    let pos = unsafe { libc::lseek(file.as_raw_fd(), offset as libc::off_t, whence) };
    if pos >= 0 {
        return Ok(Some(pos as u64));
    }
    let err = io::Error::last_os_error();
    match err.raw_os_error() {
        // No data (or no hole) at or after `offset`.
        Some(libc::ENXIO) => Ok(None),
        _ => Err(err),
    }
}

/// Copies `[start, end)` of `input` to the same offsets of `output`, leaving
/// all-zero blocks as holes. A copy still running at `deadline` stops with
/// [`io::ErrorKind::TimedOut`].
fn copy_range(
    input: &mut File,
    output: &mut File,
    start: u64,
    end: u64,
    deadline: Option<Instant>,
) -> io::Result<()> {
    input.seek(SeekFrom::Start(start))?;
    output.seek(SeekFrom::Start(start))?;
    let mut buf = vec![0u8; BLOCK];
    let mut left = end - start;
    while left > 0 {
        if deadline.is_some_and(|d| Instant::now() >= d) {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "sparse copy passed its deadline",
            ));
        }
        let want = left.min(BLOCK as u64) as usize;
        let n = input.read(&mut buf[..want])?;
        if n == 0 {
            // The source shrank under us; the length was set up front.
            break;
        }
        if buf[..n].iter().all(|b| *b == 0) {
            output.seek(SeekFrom::Current(n as i64))?;
        } else {
            output.write_all(&buf[..n])?;
        }
        left -= n as u64;
    }
    Ok(())
}

/// Copies `src` to `dst` (replacing it), keeping holes. Returns the apparent
/// length. The destination gets the source's permissions.
pub fn copy_sparse(src: &Path, dst: &Path) -> io::Result<u64> {
    let mut input = File::open(src)?;
    let meta = input.metadata()?;
    if !meta.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "sparse copy source is not a regular file",
        ));
    }
    let len = meta.len();
    let mut output = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(dst)?;
    copy_sparse_into(&mut input, &mut output, len, None)?;
    output.set_permissions(meta.permissions())?;
    Ok(len)
}

/// Copies the first `len` bytes of `input` into the empty `output`, keeping
/// holes. Permissions are not touched. A copy still running at `deadline` stops
/// with [`io::ErrorKind::TimedOut`]; the caller removes the partial file.
pub fn copy_sparse_into(
    input: &mut File,
    output: &mut File,
    len: u64,
    deadline: Option<Instant>,
) -> io::Result<()> {
    // The whole length up front: everything not written stays a hole.
    output.set_len(len)?;
    let mut pos = 0u64;
    while pos < len {
        let Some(data) = seek_whence(input, pos, libc::SEEK_DATA)? else {
            break;
        };
        if data >= len {
            break;
        }
        let hole = seek_whence(input, data, libc::SEEK_HOLE)?
            .unwrap_or(len)
            .min(len);
        copy_range(input, output, data, hole, deadline)?;
        pos = hole;
    }
    output.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    fn allocated(path: &Path) -> u64 {
        std::fs::metadata(path).expect("meta").blocks() * 512
    }

    #[test]
    fn a_sparse_file_stays_sparse_and_keeps_its_bytes() {
        let dir = tempfile::tempdir().expect("dir");
        let src = dir.path().join("src.img");
        let dst = dir.path().join("dst.img");
        let len: u64 = 8 * 1024 * 1024;
        {
            let mut f = File::create(&src).expect("create");
            f.set_len(len).expect("len");
            f.write_all(b"head data").expect("write");
            f.seek(SeekFrom::Start(len / 2 + 5)).expect("seek");
            f.write_all(b"middle data").expect("write");
            f.seek(SeekFrom::Start(len - 4)).expect("seek");
            f.write_all(b"tail").expect("write");
        }
        let copied = copy_sparse(&src, &dst).expect("copy");
        assert_eq!(copied, len);
        let a = std::fs::read(&src).expect("read src");
        let b = std::fs::read(&dst).expect("read dst");
        assert_eq!(a.len(), b.len());
        assert!(a == b, "copied bytes differ");
        assert!(
            allocated(&dst) <= allocated(&src) + 64 * 1024,
            "copy allocated {} against {}",
            allocated(&dst),
            allocated(&src)
        );
        assert!(allocated(&dst) < len / 2, "the copy is not sparse");
    }

    #[test]
    fn an_existing_destination_is_replaced_and_an_empty_file_copies() {
        let dir = tempfile::tempdir().expect("dir");
        let src = dir.path().join("empty");
        let dst = dir.path().join("dst");
        File::create(&src).expect("create");
        std::fs::write(&dst, b"old old old").expect("old");
        assert_eq!(copy_sparse(&src, &dst).expect("copy"), 0);
        assert_eq!(std::fs::metadata(&dst).expect("meta").len(), 0);
    }

    #[test]
    fn a_missing_source_or_a_directory_is_an_error() {
        let dir = tempfile::tempdir().expect("dir");
        assert!(copy_sparse(&dir.path().join("none"), &dir.path().join("d")).is_err());
        assert!(copy_sparse(dir.path(), &dir.path().join("d")).is_err());
    }
}
