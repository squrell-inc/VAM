//! Safe, read-only walker over a guest-written ext4 image.
//!
//! The guest owns its data drive and can write any bytes to the raw block device,
//! so the image is hostile input: directory entries can carry any name, sizes can
//! lie, and the tree can loop. This module is the only code that reads such an
//! image on the host. It is a pure-Rust reader (`ext4-view`), never a C parser
//! and never a shell command, and it follows three rules:
//!
//! 1. **No host path is built from an image name** except through [`TreeWriter`],
//!    which only joins components that passed [`valid_component`], creates every
//!    directory and file itself (`create_new`, `O_NOFOLLOW`) and never creates a
//!    symlink or a device.
//! 2. **Everything is bounded** by [`ImageLimits`]: entries, apparent bytes, one
//!    file, depth, and wall-clock time. Hard links count once per name.
//! 3. **Only plain data comes out**: regular files and directories. Symlinks,
//!    devices, fifos and sockets are never materialised; they are reported.
//!
//! Top-level `.vam` (the host's conversation, composed in separately) and
//! `lost+found` are skipped without being read.

use std::cell::Cell;
use std::collections::HashSet;
use std::io::{self, Read, Write};
use std::os::unix::fs::{FileExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use ext4_view::{Ext4, Ext4Error, Ext4Read, FileType};
use thiserror::Error;

use crate::error::Drive9Error;

/// Top-level names never read from an image.
const RESERVED_TOP_LEVEL: [&str; 2] = [".vam", "lost+found"];
/// How many skipped or dropped entries a report lists (the counts stay exact).
const MAX_LISTED: usize = 10_000;
const READ_CHUNK: usize = 1024 * 1024;
const WRITE_BLOCK: usize = 64 * 1024;

/// Why an image could not be read.
#[derive(Debug, Error)]
pub enum ImageReadError {
    #[error("the image is corrupt or inconsistent: {0}")]
    Corrupt(String),
    #[error("the image uses a feature this reader does not support: {0}")]
    Unsupported(String),
    #[error("the image exceeds a read limit: {0}")]
    Limit(String),
    #[error("reading the image failed: {0}")]
    Io(#[from] io::Error),
}

impl From<ImageReadError> for Drive9Error {
    fn from(e: ImageReadError) -> Self {
        match e {
            ImageReadError::Corrupt(s) | ImageReadError::Unsupported(s) => {
                Drive9Error::ImageCorrupt(s)
            }
            ImageReadError::Limit(s) => Drive9Error::ImageLimit(s),
            ImageReadError::Io(e) => Drive9Error::Io(e),
        }
    }
}

/// Maps an I/O error raised while reading file content to a typed storage
/// error (`InvalidData` is a corrupt image, `TimedOut` a limit).
pub fn content_io_error(e: io::Error) -> Drive9Error {
    match e.kind() {
        io::ErrorKind::InvalidData | io::ErrorKind::UnexpectedEof => {
            Drive9Error::ImageCorrupt(e.to_string())
        }
        io::ErrorKind::TimedOut => Drive9Error::ImageLimit(e.to_string()),
        _ => Drive9Error::Io(e),
    }
}

/// Bounds on one walk of an image.
#[derive(Debug, Clone)]
pub struct ImageLimits {
    /// Directory entries visited (every name, hard links included).
    pub max_entries: u64,
    /// Sum of the apparent sizes of the regular files met, per name.
    pub max_total_bytes: u64,
    /// One regular file's apparent size.
    pub max_file_bytes: u64,
    /// Directory depth below the root.
    pub max_depth: usize,
    /// Wall-clock time for the whole walk.
    pub timeout: Duration,
}

impl Default for ImageLimits {
    fn default() -> Self {
        Self {
            max_entries: 500_000,
            max_total_bytes: 4 * 1024 * 1024 * 1024,
            max_file_bytes: 1024 * 1024 * 1024,
            max_depth: 128,
            timeout: Duration::from_secs(600),
        }
    }
}

impl ImageLimits {
    /// The defaults, each overridable by an environment variable:
    /// `VAM_IMAGE_MAX_ENTRIES`, `VAM_IMAGE_MAX_BYTES`, `VAM_IMAGE_MAX_FILE_BYTES`,
    /// `VAM_IMAGE_MAX_DEPTH`, `VAM_IMAGE_TIMEOUT_SECS`.
    pub fn from_env() -> Self {
        fn var(name: &str) -> Option<u64> {
            std::env::var(name).ok()?.trim().parse().ok()
        }
        let mut l = Self::default();
        if let Some(v) = var("VAM_IMAGE_MAX_ENTRIES") {
            l.max_entries = v;
        }
        if let Some(v) = var("VAM_IMAGE_MAX_BYTES") {
            l.max_total_bytes = v;
        }
        if let Some(v) = var("VAM_IMAGE_MAX_FILE_BYTES") {
            l.max_file_bytes = v;
        }
        if let Some(v) = var("VAM_IMAGE_MAX_DEPTH") {
            l.max_depth = v as usize;
        }
        if let Some(v) = var("VAM_IMAGE_TIMEOUT_SECS") {
            l.timeout = Duration::from_secs(v);
        }
        l
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageEntryKind {
    File,
    Dir,
    /// A symlink, device, fifo or socket. Never materialised.
    Special(&'static str),
}

/// One entry met by the walk. `path` is `/`-separated, relative to the image
/// root, and made only of components that passed [`valid_component`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageEntry {
    pub path: String,
    pub kind: ImageEntryKind,
    /// Apparent size of a regular file; 0 otherwise.
    pub size: u64,
    /// Normalised permission bits: `0o755` when any execute bit is set, else
    /// `0o644` (files); `0o755` for directories. Ownership, setuid, setgid and
    /// sticky bits are never carried.
    pub mode: u32,
}

/// An entry the walk could not hold in a manifest or tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedEntry {
    /// Lossy display path (may hold U+FFFD for a non-UTF-8 name).
    pub path: String,
    pub reason: &'static str,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WalkReport {
    pub entries: u64,
    pub files: u64,
    pub dirs: u64,
    /// Sum of the apparent sizes of the regular files met.
    pub file_bytes: u64,
    /// Symlinks, devices, fifos and sockets: not materialised (first
    /// [`MAX_LISTED`]; `dropped_count` is exact).
    pub dropped: Vec<SkippedEntry>,
    pub dropped_count: u64,
    /// Names refused (non-UTF-8, control characters, duplicates): first
    /// [`MAX_LISTED`]; `skipped_count` is exact.
    pub skipped: Vec<SkippedEntry>,
    pub skipped_count: u64,
}

impl WalkReport {
    fn drop_entry(&mut self, path: &str, reason: &'static str) {
        self.dropped_count += 1;
        if self.dropped.len() < MAX_LISTED {
            self.dropped.push(SkippedEntry {
                path: path.to_string(),
                reason,
            });
        }
    }

    fn skip_entry(&mut self, path: String, reason: &'static str) {
        self.skipped_count += 1;
        if self.skipped.len() < MAX_LISTED {
            self.skipped.push(SkippedEntry { path, reason });
        }
    }
}

/// Receives the entries of a walk in a stable order (sorted by name within a
/// directory, a directory before its children).
pub trait ImageVisitor {
    /// Whether the content of this regular file is wanted.
    fn wants_content(&mut self, entry: &ImageEntry) -> bool;

    /// Called for every directory, regular file and special entry. `content`
    /// is `Some` only when [`wants_content`](Self::wants_content) said yes; it
    /// yields exactly `entry.size` bytes.
    fn visit(
        &mut self,
        entry: &ImageEntry,
        content: Option<&mut dyn Read>,
    ) -> Result<(), Drive9Error>;
}

/// Why a name was refused.
fn refuse_name(name: &[u8]) -> Option<&'static str> {
    if name.is_empty() {
        return Some("empty name");
    }
    if name.len() > 255 {
        return Some("name too long");
    }
    if name.contains(&b'/') {
        return Some("name contains a separator");
    }
    if name.iter().any(|b| *b < 0x20 || *b == 0x7f) {
        return Some("name contains a control character");
    }
    if std::str::from_utf8(name).is_err() {
        return Some("name is not UTF-8");
    }
    if name == b"." || name == b".." {
        return Some("dot name");
    }
    None
}

/// A name that may become one path component on the host: non-empty, at most
/// 255 bytes, valid UTF-8, not `.` or `..`, and free of `/`, NUL and control
/// characters.
pub fn valid_component(name: &str) -> bool {
    refuse_name(name.as_bytes()).is_none()
}

fn normalised_file_mode(mode: u16) -> u32 {
    if mode & 0o111 != 0 {
        0o755
    } else {
        0o644
    }
}

/// Reads the image file; every read checks the deadline.
struct GuardedFile {
    file: std::fs::File,
    deadline: Instant,
    tripped: Rc<Cell<bool>>,
}

impl Ext4Read for GuardedFile {
    fn read(
        &mut self,
        start_byte: u64,
        dst: &mut [u8],
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync + 'static>> {
        if Instant::now() > self.deadline {
            self.tripped.set(true);
            return Err(Box::new(io::Error::new(
                io::ErrorKind::TimedOut,
                "image read deadline passed",
            )));
        }
        self.file
            .read_exact_at(dst, start_byte)
            .map_err(|e| Box::new(e) as _)
    }
}

fn map_ext4(e: Ext4Error, tripped: &Cell<bool>) -> ImageReadError {
    if tripped.get() {
        return ImageReadError::Limit("the time limit for reading the image passed".into());
    }
    match e {
        Ext4Error::Io(inner) => ImageReadError::Io(io::Error::other(inner.to_string())),
        Ext4Error::Incompatible(i) => ImageReadError::Unsupported(i.to_string()),
        other => ImageReadError::Corrupt(other.to_string()),
    }
}

/// `Read` over one file in the image, bounded to its declared size.
struct FileReader<'a> {
    file: ext4_view::File,
    remaining: u64,
    deadline: Instant,
    tripped: &'a Cell<bool>,
}

impl Read for FileReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.remaining == 0 || buf.is_empty() {
            return Ok(0);
        }
        if Instant::now() > self.deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "image read deadline passed",
            ));
        }
        let want = buf
            .len()
            .min(self.remaining.min(READ_CHUNK as u64) as usize);
        match self.file.read_bytes(&mut buf[..want]) {
            Ok(0) => Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "a file in the image is shorter than its size",
            )),
            Ok(n) => {
                self.remaining -= n as u64;
                Ok(n)
            }
            Err(e) => {
                if self.tripped.get() {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "image read deadline passed",
                    ));
                }
                Err(io::Error::new(io::ErrorKind::InvalidData, e.to_string()))
            }
        }
    }
}

struct Child {
    name: String,
    path: ext4_view::PathBuf,
    kind: FileType,
    len: u64,
    mode: u16,
}

struct Walk<'a> {
    fs: Ext4,
    limits: &'a ImageLimits,
    deadline: Instant,
    tripped: Rc<Cell<bool>>,
    report: WalkReport,
    visitor: &'a mut dyn ImageVisitor,
}

impl Walk<'_> {
    fn check_time(&self) -> Result<(), ImageReadError> {
        if Instant::now() > self.deadline {
            return Err(ImageReadError::Limit(
                "the time limit for reading the image passed".into(),
            ));
        }
        Ok(())
    }

    fn err(&self, e: Ext4Error) -> ImageReadError {
        map_ext4(e, &self.tripped)
    }

    fn dir(
        &mut self,
        path: &ext4_view::PathBuf,
        rel: &str,
        depth: usize,
    ) -> Result<(), Drive9Error> {
        if depth > self.limits.max_depth {
            return Err(ImageReadError::Limit(format!(
                "directories nest deeper than {}",
                self.limits.max_depth
            ))
            .into());
        }
        let mut children: Vec<Child> = Vec::new();
        let mut seen: HashSet<Vec<u8>> = HashSet::new();
        let entries = self.fs.read_dir(path).map_err(|e| self.err(e))?;
        for entry in entries {
            self.check_time()?;
            let entry = entry.map_err(|e| self.err(e))?;
            let raw: Vec<u8> = AsRef::<[u8]>::as_ref(&entry.file_name()).to_vec();
            if raw == b"." || raw == b".." {
                continue;
            }
            self.report.entries += 1;
            if self.report.entries > self.limits.max_entries {
                return Err(ImageReadError::Limit(format!(
                    "more than {} directory entries",
                    self.limits.max_entries
                ))
                .into());
            }
            if depth == 0
                && RESERVED_TOP_LEVEL
                    .iter()
                    .any(|r| r.as_bytes() == raw.as_slice())
            {
                continue;
            }
            let shown = format!(
                "{}{}",
                if rel.is_empty() {
                    String::new()
                } else {
                    format!("{rel}/")
                },
                String::from_utf8_lossy(&raw)
            );
            if let Some(reason) = refuse_name(&raw) {
                self.report.skip_entry(shown, reason);
                continue;
            }
            if !seen.insert(raw.clone()) {
                self.report
                    .skip_entry(shown, "duplicate name in a directory");
                continue;
            }
            // The inode's own type decides, not the directory entry's.
            let md = entry.metadata().map_err(|e| self.err(e))?;
            children.push(Child {
                // Valid UTF-8 by `refuse_name`.
                name: String::from_utf8_lossy(&raw).into_owned(),
                path: entry.path(),
                kind: md.file_type(),
                len: md.len(),
                mode: md.mode(),
            });
        }
        children.sort_by(|a, b| a.name.cmp(&b.name));

        for child in children {
            self.check_time()?;
            let child_rel = if rel.is_empty() {
                child.name.clone()
            } else {
                format!("{rel}/{}", child.name)
            };
            match child.kind {
                FileType::Directory => {
                    self.report.dirs += 1;
                    let entry = ImageEntry {
                        path: child_rel.clone(),
                        kind: ImageEntryKind::Dir,
                        size: 0,
                        mode: 0o755,
                    };
                    self.visitor.visit(&entry, None)?;
                    self.dir(&child.path, &child_rel, depth + 1)?;
                }
                FileType::Regular => {
                    self.report.files += 1;
                    if child.len > self.limits.max_file_bytes {
                        return Err(ImageReadError::Limit(format!(
                            "a file is larger than {} bytes",
                            self.limits.max_file_bytes
                        ))
                        .into());
                    }
                    self.report.file_bytes = self.report.file_bytes.saturating_add(child.len);
                    if self.report.file_bytes > self.limits.max_total_bytes {
                        return Err(ImageReadError::Limit(format!(
                            "more than {} bytes of files",
                            self.limits.max_total_bytes
                        ))
                        .into());
                    }
                    let entry = ImageEntry {
                        path: child_rel,
                        kind: ImageEntryKind::File,
                        size: child.len,
                        mode: normalised_file_mode(child.mode),
                    };
                    if self.visitor.wants_content(&entry) {
                        let file = self.fs.open(&child.path).map_err(|e| self.err(e))?;
                        let mut reader = FileReader {
                            file,
                            remaining: child.len,
                            deadline: self.deadline,
                            tripped: &self.tripped,
                        };
                        self.visitor.visit(&entry, Some(&mut reader))?;
                    } else {
                        self.visitor.visit(&entry, None)?;
                    }
                }
                other => {
                    let kind = match other {
                        FileType::Symlink => "symlink",
                        FileType::BlockDevice => "block device",
                        FileType::CharacterDevice => "character device",
                        FileType::Fifo => "fifo",
                        FileType::Socket => "socket",
                        FileType::Directory | FileType::Regular => "special",
                    };
                    self.report.drop_entry(&child_rel, kind);
                    let entry = ImageEntry {
                        path: child_rel,
                        kind: ImageEntryKind::Special(kind),
                        size: 0,
                        mode: 0,
                    };
                    self.visitor.visit(&entry, None)?;
                }
            }
        }
        Ok(())
    }
}

fn open_image(image: &Path) -> Result<std::fs::File, Drive9Error> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(image)?;
    if !file.metadata()?.is_file() {
        return Err(Drive9Error::Layout(
            "the image is not a regular file".to_string(),
        ));
    }
    Ok(file)
}

/// Walks the ext4 image at `image`, calling `visitor` for each directory,
/// regular file and special entry (see the module rules).
pub fn walk_ext4_image(
    image: &Path,
    limits: &ImageLimits,
    visitor: &mut dyn ImageVisitor,
) -> Result<WalkReport, Drive9Error> {
    let file = open_image(image)?;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let tripped = Rc::new(Cell::new(false));
        let deadline = Instant::now() + limits.timeout;
        let reader = GuardedFile {
            file,
            deadline,
            tripped: tripped.clone(),
        };
        let fs = Ext4::load(Box::new(reader)).map_err(|e| map_ext4(e, &tripped))?;
        let mut walk = Walk {
            fs,
            limits,
            deadline,
            tripped,
            report: WalkReport::default(),
            visitor,
        };
        let root = ext4_view::PathBuf::new("/");
        walk.dir(&root, "", 0)?;
        Ok::<WalkReport, Drive9Error>(walk.report)
    }));
    match result {
        Ok(r) => r,
        Err(_) => Err(Drive9Error::ImageCorrupt(
            "the image reader failed unexpectedly".to_string(),
        )),
    }
}

/// Collects every entry without reading any content.
#[derive(Debug, Default)]
pub struct ScanVisitor {
    pub entries: Vec<ImageEntry>,
}

impl ImageVisitor for ScanVisitor {
    fn wants_content(&mut self, _entry: &ImageEntry) -> bool {
        false
    }

    fn visit(
        &mut self,
        entry: &ImageEntry,
        _content: Option<&mut dyn Read>,
    ) -> Result<(), Drive9Error> {
        self.entries.push(entry.clone());
        Ok(())
    }
}

/// Writes entries of an image into a directory the caller just created and
/// nobody else can write. Creates only directories and regular files; every
/// component is re-validated; existing names are never overwritten (a second
/// name that lands on the same host name is skipped and counted).
#[derive(Debug)]
pub struct TreeWriter {
    root: PathBuf,
    only: Option<HashSet<String>>,
    pub written_files: u64,
    pub written_bytes: u64,
    pub collisions: u64,
}

impl TreeWriter {
    /// `only`: when set, just these regular files are written (and their parent
    /// directories); otherwise every directory and file.
    pub fn new(root: PathBuf, only: Option<HashSet<String>>) -> Self {
        Self {
            root,
            only,
            written_files: 0,
            written_bytes: 0,
            collisions: 0,
        }
    }

    fn ensure_dirs(&self, rel_dirs: &[&str]) -> Result<PathBuf, Drive9Error> {
        let mut cur = self.root.clone();
        for comp in rel_dirs {
            if !valid_component(comp) {
                return Err(Drive9Error::ImageCorrupt(
                    "a path component is not allowed".to_string(),
                ));
            }
            cur.push(comp);
            match std::fs::symlink_metadata(&cur) {
                Ok(md) if md.is_dir() => {}
                Ok(_) => {
                    return Err(Drive9Error::Io(io::Error::from(
                        io::ErrorKind::AlreadyExists,
                    )))
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    std::fs::create_dir(&cur)?;
                    std::fs::set_permissions(&cur, std::fs::Permissions::from_mode(0o755))?;
                }
                Err(e) => return Err(e.into()),
            }
        }
        Ok(cur)
    }

    fn write_file(
        &mut self,
        entry: &ImageEntry,
        content: &mut dyn Read,
    ) -> Result<(), Drive9Error> {
        let comps: Vec<&str> = entry.path.split('/').collect();
        let Some((name, dirs)) = comps.split_last() else {
            return Ok(());
        };
        if !valid_component(name) {
            return Err(Drive9Error::ImageCorrupt(
                "a path component is not allowed".to_string(),
            ));
        }
        let parent = match self.ensure_dirs(dirs) {
            Ok(p) => p,
            Err(Drive9Error::Io(e)) if e.kind() == io::ErrorKind::AlreadyExists => {
                self.collisions += 1;
                return Ok(());
            }
            Err(e) => return Err(e),
        };
        let path = parent.join(name);
        let mut out = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .mode(0o600)
            .open(&path)
        {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                self.collisions += 1;
                return Ok(());
            }
            Err(e) => return Err(e.into()),
        };
        // Zero blocks stay holes, so a sparse file stays sparse.
        out.set_len(entry.size)?;
        let mut buf = vec![0u8; WRITE_BLOCK];
        let mut pos: u64 = 0;
        loop {
            let n = content.read(&mut buf).map_err(content_io_error)?;
            if n == 0 {
                break;
            }
            if buf[..n].iter().any(|b| *b != 0) {
                out.write_all_at(&buf[..n], pos)?;
            }
            pos += n as u64;
        }
        if pos != entry.size {
            return Err(Drive9Error::ImageCorrupt(
                "a file in the image is shorter than its size".to_string(),
            ));
        }
        out.flush()?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(entry.mode))?;
        self.written_files += 1;
        self.written_bytes += entry.size;
        Ok(())
    }
}

impl ImageVisitor for TreeWriter {
    fn wants_content(&mut self, entry: &ImageEntry) -> bool {
        entry.kind == ImageEntryKind::File
            && self
                .only
                .as_ref()
                .is_none_or(|set| set.contains(&entry.path))
    }

    fn visit(
        &mut self,
        entry: &ImageEntry,
        content: Option<&mut dyn Read>,
    ) -> Result<(), Drive9Error> {
        match (entry.kind, content) {
            (ImageEntryKind::Dir, _) if self.only.is_none() => {
                let comps: Vec<&str> = entry.path.split('/').collect();
                match self.ensure_dirs(&comps) {
                    Ok(_) => Ok(()),
                    Err(Drive9Error::Io(e)) if e.kind() == io::ErrorKind::AlreadyExists => {
                        self.collisions += 1;
                        Ok(())
                    }
                    Err(e) => Err(e),
                }
            }
            (ImageEntryKind::File, Some(content)) => self.write_file(entry, content),
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests;
