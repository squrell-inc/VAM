//! Safe unpacking of wheels (zip) and npm tarballs (tgz), run only after the hash matches.
//!
//! The hash proves integrity, not safety: the lockfile's author picked it, so the archive is
//! attacker content. Every entry is checked, nothing is extracted by a library routine, and the
//! unpacker never creates a link of any kind, so no symlink can exist in the tree to redirect a
//! later write. Modes are computed, never copied.

use super::limits::{Limits, MAX_COMPONENT_BYTES, MAX_DEPTH, MAX_PATH_BYTES};
use super::names::{normalize_project, versions_equal};
use super::{refused, DepsError};
use flate2::read::GzDecoder;
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::{BufReader, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

const MIB: u64 = 1024 * 1024;

/// 1980-01-01, the zip epoch: every unpacked file and directory carries it (a reproducible tree).
fn fixed_mtime() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(315_532_800)
}

/// The package being unpacked, for messages and layout checks.
pub(crate) struct Pkg<'a> {
    pub name: &'a str,
    pub version: &'a str,
}

impl Pkg<'_> {
    fn label(&self) -> String {
        format!("package '{}' {}", self.name, self.version)
    }
}

pub(crate) fn fmt_cap(bytes: u64) -> String {
    if bytes != 0 && bytes.is_multiple_of(MIB) {
        format!("{} MiB", bytes / MIB)
    } else {
        format!("{bytes} bytes")
    }
}

fn bytes_over(pkg: &Pkg, cap: u64) -> DepsError {
    refused(format!(
        "{}: skill packages are over the {} limit (unpacked)",
        pkg.label(),
        fmt_cap(cap)
    ))
}

fn files_over(pkg: &Pkg, cap: usize) -> DepsError {
    refused(format!(
        "{}: skill packages are over the {cap} file limit",
        pkg.label()
    ))
}

fn show(path: &[u8]) -> String {
    let s = String::from_utf8_lossy(path);
    let mut out: String = s
        .chars()
        .take(120)
        .map(|c| if c.is_control() { '?' } else { c })
        .collect();
    if s.chars().count() > 120 {
        out.push_str("...");
    }
    out
}

fn bad_entry(pkg: &Pkg, path: &[u8], why: &str) -> DepsError {
    refused(format!(
        "{}: archive entry '{}' {why}",
        pkg.label(),
        show(path)
    ))
}

// ---------------------------------------------------------------------------------------------
// Names
// ---------------------------------------------------------------------------------------------

/// Validate one archive entry name and split it into components. A single trailing `/` marks a
/// directory and is dropped; `.` components are skipped.
pub(crate) fn clean_name(raw: &[u8]) -> Result<Vec<String>, &'static str> {
    if raw.is_empty() {
        return Err("has an empty name");
    }
    if raw.contains(&0) {
        return Err("has a NUL byte in its name");
    }
    if raw.contains(&b'\\') {
        return Err("has a backslash in its name");
    }
    if raw.len() > MAX_PATH_BYTES {
        return Err("has a path over 512 bytes");
    }
    let s = std::str::from_utf8(raw).map_err(|_| "has a name that is not UTF-8")?;
    if s.starts_with("//") {
        return Err("has a name starting with //");
    }
    if s.starts_with('/') {
        return Err("has an absolute path");
    }
    let b = s.as_bytes();
    if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
        return Err("has a drive prefix");
    }
    let body = s.strip_suffix('/').unwrap_or(s);
    let mut comps = Vec::new();
    for comp in body.split('/') {
        match comp {
            "" => return Err("has an empty path component"),
            "." => continue,
            ".." => return Err("has a '..' path component"),
            c if c.len() > MAX_COMPONENT_BYTES => {
                return Err("has a path component over 255 bytes")
            }
            c => comps.push(c.to_string()),
        }
    }
    if comps.is_empty() {
        return Err("has an empty name");
    }
    if comps.len() > MAX_DEPTH {
        return Err("is nested deeper than 32 levels");
    }
    Ok(comps)
}

// ---------------------------------------------------------------------------------------------
// Duplicate tracking
// ---------------------------------------------------------------------------------------------

struct Seen {
    exact: String,
    is_dir: bool,
}

/// Duplicate and file-vs-directory detection inside one archive (exact and ASCII case-folded).
#[derive(Default)]
struct ArchiveNames {
    map: HashMap<String, Seen>,
}

impl ArchiveNames {
    fn add(&mut self, comps: &[String], is_dir: bool) -> Result<(), &'static str> {
        for end in 1..comps.len() {
            let p = comps[..end].join("/");
            let key = p.to_ascii_lowercase();
            match self.map.get(&key) {
                Some(s) if !s.is_dir => return Err("is both a file and a directory"),
                Some(s) if s.exact != p => {
                    return Err("collides with another entry that differs only in case")
                }
                Some(_) => {}
                None => {
                    self.map.insert(
                        key,
                        Seen {
                            exact: p,
                            is_dir: true,
                        },
                    );
                }
            }
        }
        let p = comps.join("/");
        let key = p.to_ascii_lowercase();
        match self.map.get(&key) {
            None => {
                self.map.insert(key, Seen { exact: p, is_dir });
                Ok(())
            }
            Some(s) if s.exact != p => Err("collides with another entry that differs only in case"),
            Some(s) if s.is_dir && is_dir => Ok(()),
            Some(s) if s.is_dir != is_dir => Err("is both a file and a directory"),
            Some(_) => Err("appears twice in the archive"),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Cross-package ledger
// ---------------------------------------------------------------------------------------------

#[derive(Debug)]
struct Claim {
    exact: String,
    is_dir: bool,
    owner: String,
}

/// What the whole skill's unpack has written so far: totals for the per-skill caps and every
/// path, so two packages writing one file are refused. Directories may be shared.
#[derive(Default, Debug)]
pub(crate) struct Ledger {
    pub(crate) files: usize,
    pub(crate) bytes: u64,
    claims: HashMap<String, Claim>,
}

impl Ledger {
    fn claim_dir(&mut self, path: &str, owner: &Pkg) -> Result<(), DepsError> {
        let key = path.to_ascii_lowercase();
        match self.claims.get(&key) {
            None => {
                self.claims.insert(
                    key,
                    Claim {
                        exact: path.to_string(),
                        is_dir: true,
                        owner: owner.label(),
                    },
                );
                Ok(())
            }
            Some(c) if c.is_dir && c.exact == path => Ok(()),
            Some(c) => Err(collision(&c.owner, owner, path)),
        }
    }

    fn claim_file(&mut self, path: &str, owner: &Pkg) -> Result<(), DepsError> {
        let key = path.to_ascii_lowercase();
        match self.claims.get(&key) {
            None => {
                self.claims.insert(
                    key,
                    Claim {
                        exact: path.to_string(),
                        is_dir: false,
                        owner: owner.label(),
                    },
                );
                Ok(())
            }
            Some(c) => Err(collision(&c.owner, owner, path)),
        }
    }
}

fn collision(first: &str, second: &Pkg, path: &str) -> DepsError {
    refused(format!(
        "{first} and {} both write '{path}'; the two packages overlap, lock one of them out",
        second.label()
    ))
}

// ---------------------------------------------------------------------------------------------
// Tree writer
// ---------------------------------------------------------------------------------------------

/// Writes into a fresh staging root. Directories are made one component at a time with
/// `create_dir`, files with `create_new` and `O_NOFOLLOW`.
struct TreeWriter<'a> {
    root: &'a Path,
    dirs: Vec<PathBuf>,
}

impl<'a> TreeWriter<'a> {
    fn new(root: &'a Path) -> TreeWriter<'a> {
        TreeWriter {
            root,
            dirs: Vec::new(),
        }
    }

    /// Make every directory of `comps`, claiming each in the ledger.
    fn ensure_dirs(
        &mut self,
        comps: &[String],
        ledger: &mut Ledger,
        pkg: &Pkg,
    ) -> Result<(), DepsError> {
        let mut path = self.root.to_path_buf();
        let mut rel = String::new();
        for comp in comps {
            path.push(comp);
            if !rel.is_empty() {
                rel.push('/');
            }
            rel.push_str(comp);
            ledger.claim_dir(&rel, pkg)?;
            match std::fs::symlink_metadata(&path) {
                Ok(m) if m.file_type().is_dir() => {}
                Ok(_) => {
                    return Err(refused(format!(
                        "{}: '{rel}' is both a file and a directory",
                        pkg.label()
                    )))
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    std::fs::create_dir(&path)?;
                    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
                    self.dirs.push(path.clone());
                }
                Err(e) => return Err(DepsError::Io(e)),
            }
        }
        Ok(())
    }

    /// Write one file; `max` bounds the bytes, `Err(WriteOutcome::TooBig)` when exceeded.
    fn write_file(
        &mut self,
        comps: &[String],
        reader: &mut dyn Read,
        exec: bool,
        max: u64,
        ledger: &mut Ledger,
        pkg: &Pkg,
    ) -> Result<Result<u64, TooBig>, DepsError> {
        let (name, parent) = match comps.split_last() {
            Some(x) => x,
            None => return Err(refused(format!("{}: internal empty path", pkg.label()))),
        };
        self.ensure_dirs(parent, ledger, pkg)?;
        ledger.claim_file(&comps.join("/"), pkg)?;
        let mut path = self.root.to_path_buf();
        for c in parent {
            path.push(c);
        }
        path.push(name);
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .custom_flags(libc::O_NOFOLLOW)
            .mode(0o600)
            .open(&path)?;
        let mut buf = vec![0u8; 64 * 1024];
        let mut total = 0u64;
        loop {
            let n = match reader.read(&mut buf) {
                Ok(n) => n,
                Err(e) => return Err(read_error(pkg, comps, e)),
            };
            if n == 0 {
                break;
            }
            total += n as u64;
            if total > max {
                return Ok(Err(TooBig));
            }
            file.write_all(&buf[..n])?;
        }
        file.set_modified(fixed_mtime())?;
        drop(file);
        std::fs::set_permissions(
            &path,
            std::fs::Permissions::from_mode(if exec { 0o555 } else { 0o444 }),
        )?;
        Ok(Ok(total))
    }

    /// Fix directory mtimes (creating files changed them), deepest first.
    fn finish(self) -> Result<(), DepsError> {
        for dir in self.dirs.iter().rev() {
            std::fs::File::open(dir)?.set_modified(fixed_mtime())?;
        }
        Ok(())
    }
}

struct TooBig;

fn read_error(pkg: &Pkg, comps: &[String], e: std::io::Error) -> DepsError {
    if e.kind() == std::io::ErrorKind::Other && e.to_string() == CAP_MARK {
        return refused(format!(
            "{}: the archive expands beyond the unpacked size limit",
            pkg.label()
        ));
    }
    refused(format!(
        "{}: archive entry '{}' is corrupt ({})",
        pkg.label(),
        show(comps.join("/").as_bytes()),
        e.kind()
    ))
}

// ---------------------------------------------------------------------------------------------
// Wheel
// ---------------------------------------------------------------------------------------------

struct WheelEntry {
    index: usize,
    /// Destination under the tree root, or `None` for entries that are dropped.
    dest: Option<Vec<String>>,
    is_dir: bool,
    exec: bool,
    size: u64,
}

/// Unpack a wheel into `out_root/<dest_prefix>`: `.data/purelib` and `.data/platlib` merge into
/// the root, the rest of `.data` (scripts, headers, data) is dropped, everything else lands as is.
pub(crate) fn unpack_wheel(
    archive: &Path,
    out_root: &Path,
    dest_prefix: &str,
    pkg: &Pkg,
    ledger: &mut Ledger,
    limits: &Limits,
) -> Result<(), DepsError> {
    let file = std::fs::File::open(archive)?;
    let mut zip = zip::ZipArchive::new(BufReader::new(file)).map_err(|_| {
        refused(format!(
            "{}: the file is not a valid wheel (zip) archive",
            pkg.label()
        ))
    })?;
    zip.validate_header_consistency().map_err(|_| {
        refused(format!(
            "{}: the wheel's local and central directory headers disagree",
            pkg.label()
        ))
    })?;
    if zip.len() > limits.max_archive_files {
        return Err(files_over(pkg, limits.max_archive_files));
    }

    let want_name = normalize_project(pkg.name);
    let mut names = ArchiveNames::default();
    let mut entries: Vec<WheelEntry> = Vec::new();
    let mut declared_total = 0u64;
    let mut saw_dist_info = false;
    for index in 0..zip.len() {
        let entry = zip.by_index_raw(index).map_err(|_| {
            refused(format!(
                "{}: the wheel's central directory is unreadable",
                pkg.label()
            ))
        })?;
        let raw = entry.name_raw().to_vec();
        if entry.encrypted() {
            return Err(bad_entry(pkg, &raw, "is encrypted"));
        }
        if !matches!(
            entry.compression(),
            zip::CompressionMethod::Stored | zip::CompressionMethod::Deflated
        ) {
            return Err(bad_entry(
                pkg,
                &raw,
                "uses a compression method other than stored or deflate",
            ));
        }
        let mode = entry.unix_mode().unwrap_or(0);
        match mode & 0o170000 {
            0 | 0o100000 | 0o040000 => {}
            0o120000 => return Err(bad_entry(pkg, &raw, "is a symbolic link")),
            0o060000 | 0o020000 => return Err(bad_entry(pkg, &raw, "is a device file")),
            0o010000 => return Err(bad_entry(pkg, &raw, "is a FIFO")),
            _ => return Err(bad_entry(pkg, &raw, "is a special file")),
        }
        let comps = clean_name(&raw).map_err(|why| bad_entry(pkg, &raw, why))?;
        let is_dir = raw.ends_with(b"/") || mode & 0o170000 == 0o040000;
        names
            .add(&comps, is_dir)
            .map_err(|why| bad_entry(pkg, &raw, why))?;
        let size = entry.size();
        if size > limits.max_file_bytes {
            return Err(bytes_over(pkg, limits.max_file_bytes));
        }
        declared_total = declared_total.saturating_add(size);
        if declared_total > limits.max_unpacked_archive_bytes {
            return Err(bytes_over(pkg, limits.max_unpacked_archive_bytes));
        }
        let dest = map_wheel_path(&comps, &want_name, pkg, &mut saw_dist_info)
            .map_err(|why| bad_entry(pkg, &raw, why))?;
        entries.push(WheelEntry {
            index,
            dest,
            is_dir,
            exec: mode & 0o100 != 0,
            size,
        });
    }
    if !saw_dist_info {
        return Err(refused(format!(
            "{}: the wheel has no .dist-info directory",
            pkg.label()
        )));
    }

    let prefix: Vec<String> = dest_prefix
        .split('/')
        .filter(|c| !c.is_empty())
        .map(str::to_string)
        .collect();
    let mut writer = TreeWriter::new(out_root);
    writer.ensure_dirs(&prefix, ledger, pkg)?;
    let mut archive_bytes = 0u64;
    for e in &entries {
        let Some(dest) = &e.dest else { continue };
        let mut full = prefix.clone();
        full.extend(dest.iter().cloned());
        if e.is_dir {
            writer.ensure_dirs(&full, ledger, pkg)?;
            continue;
        }
        if ledger.files + 1 > limits.max_skill_files {
            return Err(files_over(pkg, limits.max_skill_files));
        }
        let archive_left = limits
            .max_unpacked_archive_bytes
            .saturating_sub(archive_bytes);
        let skill_left = limits.max_unpacked_skill_bytes.saturating_sub(ledger.bytes);
        let max = limits.max_file_bytes.min(archive_left).min(skill_left);
        let mut f = zip
            .by_index(e.index)
            .map_err(|_| refused(format!("{}: a wheel entry cannot be read", pkg.label())))?;
        match writer.write_file(&full, &mut f, e.exec, max, ledger, pkg)? {
            Ok(n) => {
                // The declared size must hold, or the entry lied about itself.
                if n != e.size {
                    return Err(bad_entry(
                        pkg,
                        full.join("/").as_bytes(),
                        "does not match its declared size",
                    ));
                }
                archive_bytes += n;
                ledger.bytes += n;
                ledger.files += 1;
            }
            Err(TooBig) => {
                let cap = limits
                    .max_file_bytes
                    .min(limits.max_unpacked_archive_bytes)
                    .min(limits.max_unpacked_skill_bytes);
                return Err(bytes_over(pkg, cap));
            }
        }
    }
    writer.finish()
}

/// Layout of a wheel path. `Ok(None)` drops the entry.
fn map_wheel_path(
    comps: &[String],
    want_name: &str,
    pkg: &Pkg,
    saw_dist_info: &mut bool,
) -> Result<Option<Vec<String>>, &'static str> {
    let top = &comps[0];
    let matches_release = |stem: &str| {
        stem.rsplit_once('-').is_some_and(|(dist, ver)| {
            normalize_project(dist) == want_name && versions_equal(ver, pkg.version)
        })
    };
    if let Some(stem) = top.strip_suffix(".data") {
        if !matches_release(stem) {
            return Err(
                "is in a .data directory that does not match this wheel's name and version",
            );
        }
        return Ok(match comps.get(1).map(String::as_str) {
            Some("purelib") | Some("platlib") if comps.len() > 2 => Some(comps[2..].to_vec()),
            _ => None,
        });
    }
    if let Some(stem) = top.strip_suffix(".dist-info") {
        if !matches_release(stem) {
            return Err(
                "is in a .dist-info directory that does not match this wheel's name and version",
            );
        }
        *saw_dist_info = true;
    }
    Ok(Some(comps.to_vec()))
}

// ---------------------------------------------------------------------------------------------
// npm tarball
// ---------------------------------------------------------------------------------------------

const CAP_MARK: &str = "unpacked stream limit";

/// Counts decompressed bytes and fails once past the cap (a gzip bomb stops here, including the
/// bytes of entries the unpacker skips).
struct CapReader<R> {
    inner: R,
    left: u64,
    tripped: Arc<AtomicBool>,
}

impl<R: Read> Read for CapReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.left == 0 {
            let mut probe = [0u8; 1];
            return match self.inner.read(&mut probe)? {
                0 => Ok(0),
                _ => {
                    self.tripped.store(true, Ordering::SeqCst);
                    Err(std::io::Error::other(CAP_MARK))
                }
            };
        }
        let want = buf.len().min(self.left.min(usize::MAX as u64) as usize);
        let n = self.inner.read(&mut buf[..want])?;
        self.left -= n as u64;
        Ok(n)
    }
}

/// Unpack an npm tarball into `out_root/<dest_prefix>/` (the lock key path). The first path
/// component of every entry is stripped, whatever it is called.
pub(crate) fn unpack_tgz(
    archive: &Path,
    out_root: &Path,
    dest_prefix: &str,
    pkg: &Pkg,
    ledger: &mut Ledger,
    limits: &Limits,
) -> Result<(), DepsError> {
    let file = std::fs::File::open(archive)?;
    let tripped = Arc::new(AtomicBool::new(false));
    // Tar headers and padding add bytes beyond the file contents; allow for them.
    let stream_cap = limits
        .max_unpacked_archive_bytes
        .saturating_add((limits.max_archive_files as u64).saturating_mul(1536));
    let reader = CapReader {
        inner: GzDecoder::new(BufReader::new(file)),
        left: stream_cap,
        tripped: tripped.clone(),
    };
    let mut tar = tar::Archive::new(reader);
    let overflow = |tripped: &AtomicBool| {
        tripped
            .load(Ordering::SeqCst)
            .then(|| bytes_over(pkg, limits.max_unpacked_archive_bytes))
    };
    let corrupt = |tripped: &AtomicBool| {
        overflow(tripped).unwrap_or_else(|| {
            refused(format!(
                "{}: the tarball is not a valid .tgz archive",
                pkg.label()
            ))
        })
    };

    let prefix: Vec<String> = dest_prefix
        .split('/')
        .filter(|c| !c.is_empty())
        .map(str::to_string)
        .collect();
    let mut writer = TreeWriter::new(out_root);
    let mut names = ArchiveNames::default();
    let mut count = 0usize;
    let mut archive_bytes = 0u64;
    let entries = tar.entries().map_err(|_| corrupt(&tripped))?;
    for entry in entries {
        let mut entry = entry.map_err(|_| corrupt(&tripped))?;
        count += 1;
        if count > limits.max_archive_files {
            return Err(files_over(pkg, limits.max_archive_files));
        }
        let raw = entry.path_bytes().to_vec();
        let kind = entry.header().entry_type();
        use tar::EntryType as T;
        match kind {
            T::Regular | T::Continuous | T::Directory => {}
            T::XGlobalHeader => continue,
            T::Symlink => return Err(bad_entry(pkg, &raw, "is a symbolic link")),
            T::Link => return Err(bad_entry(pkg, &raw, "is a hard link")),
            T::Char | T::Block => return Err(bad_entry(pkg, &raw, "is a device file")),
            T::Fifo => return Err(bad_entry(pkg, &raw, "is a FIFO")),
            T::GNUSparse => return Err(bad_entry(pkg, &raw, "is a sparse file")),
            _ => return Err(bad_entry(pkg, &raw, "is a special file")),
        }
        let is_dir = kind == T::Directory;
        let comps = clean_name(&raw).map_err(|why| bad_entry(pkg, &raw, why))?;
        names
            .add(&comps, is_dir)
            .map_err(|why| bad_entry(pkg, &raw, why))?;
        let rest = &comps[1..];
        let mut full = prefix.clone();
        full.extend(rest.iter().cloned());
        if rest.is_empty() {
            if is_dir {
                continue;
            }
            return Err(bad_entry(pkg, &raw, "is a file with only one path component (npm tarballs nest everything under one directory)"));
        }
        if is_dir {
            writer.ensure_dirs(&full, ledger, pkg)?;
            continue;
        }
        let declared = entry.header().size().unwrap_or(u64::MAX);
        if declared > limits.max_file_bytes {
            return Err(bytes_over(pkg, limits.max_file_bytes));
        }
        if ledger.files + 1 > limits.max_skill_files {
            return Err(files_over(pkg, limits.max_skill_files));
        }
        let exec = entry
            .header()
            .mode()
            .map(|m| m & 0o100 != 0)
            .unwrap_or(false);
        let archive_left = limits
            .max_unpacked_archive_bytes
            .saturating_sub(archive_bytes);
        let skill_left = limits.max_unpacked_skill_bytes.saturating_sub(ledger.bytes);
        let max = limits.max_file_bytes.min(archive_left).min(skill_left);
        match writer.write_file(&full, &mut entry, exec, max, ledger, pkg) {
            Ok(Ok(n)) => {
                archive_bytes += n;
                ledger.bytes += n;
                ledger.files += 1;
            }
            Ok(Err(TooBig)) => {
                let cap = limits
                    .max_file_bytes
                    .min(limits.max_unpacked_archive_bytes)
                    .min(limits.max_unpacked_skill_bytes);
                return Err(bytes_over(pkg, cap));
            }
            Err(e) => return Err(overflow(&tripped).unwrap_or(e)),
        }
    }
    writer.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::os::unix::fs::MetadataExt;

    // ---- builders ----

    const PKG: Pkg<'static> = Pkg {
        name: "demo",
        version: "1.0",
    };

    fn limits() -> Limits {
        Limits::default()
    }

    /// (name, bytes, unix mode with type bits, symlink target)
    struct ZEntry<'a> {
        name: &'a str,
        data: &'a [u8],
        mode: u32,
    }

    fn zf<'a>(name: &'a str, data: &'a [u8]) -> ZEntry<'a> {
        ZEntry {
            name,
            data,
            mode: 0o100644,
        }
    }

    fn build_zip(entries: &[ZEntry]) -> Vec<u8> {
        let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for e in entries {
            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored)
                .unix_permissions(e.mode);
            match e.mode & 0o170000 {
                0o120000 => w
                    .add_symlink(e.name, std::str::from_utf8(e.data).expect("utf8"), opts)
                    .expect("symlink"),
                0o040000 => w.add_directory(e.name, opts).expect("dir"),
                _ => {
                    w.start_file(e.name, opts).expect("start");
                    w.write_all(e.data).expect("write");
                }
            }
        }
        let mut bytes = w.finish().expect("finish").into_inner();
        for e in entries {
            if e.mode != 0o100644 {
                patch_unix_mode(&mut bytes, e.name, e.mode);
            }
        }
        bytes
    }

    /// The zip writer masks permissions to 0o777; write the full mode into the central directory.
    fn patch_unix_mode(bytes: &mut [u8], name: &str, mode: u32) {
        let mut i = 0;
        while i + 46 <= bytes.len() {
            if bytes[i..i + 4] == [0x50, 0x4b, 0x01, 0x02] {
                let n = u16::from_le_bytes([bytes[i + 28], bytes[i + 29]]) as usize;
                let extra = u16::from_le_bytes([bytes[i + 30], bytes[i + 31]]) as usize;
                let comment = u16::from_le_bytes([bytes[i + 32], bytes[i + 33]]) as usize;
                if &bytes[i + 46..i + 46 + n] == name.as_bytes() {
                    bytes[i + 5] = 3; // made by: Unix
                    bytes[i + 38..i + 42].copy_from_slice(&(mode << 16).to_le_bytes());
                    return;
                }
                i += 46 + n + extra + comment;
            } else {
                i += 1;
            }
        }
        panic!("central directory entry {name} not found");
    }

    fn with_dist_info<'a>(mut v: Vec<ZEntry<'a>>) -> Vec<ZEntry<'a>> {
        v.push(zf("demo-1.0.dist-info/METADATA", b"Name: demo\n"));
        v
    }

    fn tar_header(
        name: &[u8],
        kind: tar::EntryType,
        size: u64,
        mode: u32,
        link: &[u8],
    ) -> tar::Header {
        let mut h = tar::Header::new_gnu();
        {
            let old = h.as_old_mut();
            old.name[..name.len().min(100)].copy_from_slice(&name[..name.len().min(100)]);
            old.linkname[..link.len()].copy_from_slice(link);
        }
        h.set_entry_type(kind);
        h.set_size(size);
        h.set_mode(mode);
        h.set_cksum();
        h
    }

    struct TEntry<'a> {
        name: &'a [u8],
        kind: tar::EntryType,
        data: &'a [u8],
        mode: u32,
        link: &'a [u8],
    }

    fn tf<'a>(name: &'a str, data: &'a [u8]) -> TEntry<'a> {
        TEntry {
            name: name.as_bytes(),
            kind: tar::EntryType::Regular,
            data,
            mode: 0o644,
            link: b"",
        }
    }

    fn build_tgz(entries: &[TEntry]) -> Vec<u8> {
        let mut raw = Vec::new();
        for e in entries {
            let h = tar_header(e.name, e.kind, e.data.len() as u64, e.mode, e.link);
            raw.extend_from_slice(h.as_bytes());
            raw.extend_from_slice(e.data);
            let pad = (512 - e.data.len() % 512) % 512;
            raw.extend(std::iter::repeat_n(0u8, pad));
        }
        raw.extend(std::iter::repeat_n(0u8, 1024));
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        enc.write_all(&raw).expect("gz");
        enc.finish().expect("gz finish")
    }

    struct Sandbox {
        _dir: tempfile::TempDir,
        archive: PathBuf,
        out: PathBuf,
    }

    fn sandbox(bytes: &[u8]) -> Sandbox {
        let dir = tempfile::tempdir().expect("tempdir");
        let archive = dir.path().join("pkg.bin");
        std::fs::write(&archive, bytes).expect("write");
        let out = dir.path().join("out");
        std::fs::create_dir(&out).expect("mkdir");
        Sandbox {
            _dir: dir,
            archive,
            out,
        }
    }

    fn wheel(entries: &[ZEntry], lim: &Limits) -> (Sandbox, Result<Ledger, DepsError>) {
        let sb = sandbox(&build_zip(entries));
        let mut ledger = Ledger::default();
        let r =
            unpack_wheel(&sb.archive, &sb.out, "python", &PKG, &mut ledger, lim).map(|_| ledger);
        (sb, r)
    }

    fn tgz(entries: &[TEntry], lim: &Limits) -> (Sandbox, Result<Ledger, DepsError>) {
        let sb = sandbox(&build_tgz(entries));
        let mut ledger = Ledger::default();
        let r = unpack_tgz(
            &sb.archive,
            &sb.out,
            "node_modules/demo",
            &PKG,
            &mut ledger,
            lim,
        )
        .map(|_| ledger);
        (sb, r)
    }

    fn refusal<T: std::fmt::Debug>(r: Result<T, DepsError>) -> String {
        match r {
            Err(DepsError::Refused(m)) => m,
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    fn mode_of(p: &Path) -> u32 {
        std::fs::symlink_metadata(p).expect("meta").mode() & 0o7777
    }

    // ---- names ----

    #[test]
    fn name_rules_table() {
        let long_comp = "a".repeat(256);
        let long_path = format!("{}/x", "d/".repeat(260));
        let deep = vec!["d"; 33].join("/");
        for (raw, why) in [
            (&b""[..], "empty name"),
            (b"a\0b", "NUL"),
            (b"a\\b", "backslash"),
            (b"/etc/passwd", "absolute"),
            (b"//server/share", "starting with //"),
            (b"C:/win/file", "drive prefix"),
            (b"c:evil", "drive prefix"),
            (b"a/../b", "'..'"),
            (b"..", "'..'"),
            (b"../x", "'..'"),
            (b"a//b", "empty path component"),
            (long_comp.as_bytes(), "over 255"),
            (long_path.as_bytes(), "over 512"),
            (deep.as_bytes(), "deeper than 32"),
            (&[0xff, 0xfe], "UTF-8"),
            (b"./", "empty name"),
        ] {
            let e = clean_name(raw).expect_err(&format!("{raw:?}"));
            assert!(e.contains(why), "{raw:?}: {e}");
        }
        assert_eq!(clean_name(b"./a/b/").expect("ok"), vec!["a", "b"]);
        assert_eq!(clean_name(b"a/./b").expect("ok"), vec!["a", "b"]);
    }

    #[test]
    fn unpack_name_table() {
        // Zip. The writer keeps hostile names verbatim.
        for (name, why) in [
            ("../escape.py", "'..'"),
            ("pkg/../../escape.py", "'..'"),
            ("/abs/escape.py", "absolute"),
            ("pkg\\evil.py", "backslash"),
            ("C:/evil.py", "drive prefix"),
            ("//unc/evil.py", "starting with //"),
        ] {
            let (sb, r) = wheel(&with_dist_info(vec![zf(name, b"x")]), &limits());
            let m = refusal(r);
            assert!(
                m.contains("package 'demo' 1.0: archive entry"),
                "{name}: {m}"
            );
            assert!(m.contains(why), "{name}: {m}");
            assert!(
                std::fs::read_dir(&sb.out).expect("ls").next().is_none(),
                "nothing written for {name}"
            );
        }
        // Tar.
        for (name, why) in [
            (&b"package/../escape.js"[..], "'..'"),
            (b"/abs/escape.js", "absolute"),
            (b"package\\evil.js", "backslash"),
            (b"C:/evil.js", "drive prefix"),
            (b"//unc/evil.js", "starting with //"),
        ] {
            let e = TEntry {
                name,
                kind: tar::EntryType::Regular,
                data: b"x",
                mode: 0o644,
                link: b"",
            };
            let (sb, r) = tgz(&[e], &limits());
            let m = refusal(r);
            assert!(m.contains(why), "{}: {m}", String::from_utf8_lossy(name));
            assert!(
                !sb.out.join("node_modules").exists()
                    || std::fs::read_dir(sb.out.join("node_modules/demo"))
                        .map(|mut d| d.next().is_none())
                        .unwrap_or(true)
            );
        }
    }

    #[test]
    fn unpack_refuses_links_and_devices() {
        // Zip: symlink, and special file types by mode.
        let (_sb, r) = wheel(
            &with_dist_info(vec![ZEntry {
                name: "evil",
                data: b"/etc/passwd",
                mode: 0o120777,
            }]),
            &limits(),
        );
        let m = refusal(r);
        assert!(
            m.contains("package 'demo' 1.0: archive entry 'evil' is a symbolic link"),
            "{m}"
        );
        for (mode, why) in [
            (0o060644u32, "device file"),
            (0o020644, "device file"),
            (0o010644, "FIFO"),
            (0o140644, "special file"),
        ] {
            let (_sb, r) = wheel(
                &with_dist_info(vec![ZEntry {
                    name: "dev",
                    data: b"",
                    mode,
                }]),
                &limits(),
            );
            assert!(refusal(r).contains(why), "{mode:o}");
        }
        // Tar: every non-regular type.
        use tar::EntryType as T;
        for (kind, why) in [
            (T::Symlink, "symbolic link"),
            (T::Link, "hard link"),
            (T::Char, "device file"),
            (T::Block, "device file"),
            (T::Fifo, "FIFO"),
        ] {
            let e = TEntry {
                name: b"package/x",
                kind,
                data: b"",
                mode: 0o644,
                link: b"/etc/passwd",
            };
            let (sb, r) = tgz(&[e], &limits());
            let m = refusal(r);
            assert!(m.contains(why) && m.contains("package/x"), "{kind:?}: {m}");
            assert!(!sb.out.join("node_modules/demo/x").exists());
        }
        // A GNU sparse entry: the tar layer may reject the header itself before the entry type is
        // seen; either way it is refused and nothing is written.
        let e = TEntry {
            name: b"package/x",
            kind: tar::EntryType::GNUSparse,
            data: b"",
            mode: 0o644,
            link: b"",
        };
        let (sb, r) = tgz(&[e], &limits());
        let m = refusal(r);
        assert!(
            m.contains("sparse file") || m.contains("not a valid .tgz"),
            "{m}"
        );
        assert!(!sb.out.join("node_modules/demo/x").exists());
    }

    #[test]
    fn no_link_can_redirect_a_later_write() {
        // A symlink entry followed by a file written "through" it is refused at the symlink,
        // and the tree holds no link at all.
        let entries = [
            TEntry {
                name: b"package/link",
                kind: tar::EntryType::Symlink,
                data: b"",
                mode: 0o777,
                link: b"/tmp",
            },
            tf("package/link/pwned", b"x"),
        ];
        let (sb, r) = tgz(&entries, &limits());
        assert!(refusal(r).contains("symbolic link"));
        assert!(!sb.out.join("node_modules/demo/link").exists());
        // A real directory in the staging area that is a symlink on disk is never followed.
        let sb = sandbox(&build_tgz(&[tf("package/dir/file", b"x")]));
        std::fs::create_dir_all(sb.out.join("node_modules")).expect("mkdir");
        let outside = tempfile::tempdir().expect("tempdir");
        std::os::unix::fs::symlink(outside.path(), sb.out.join("node_modules/demo")).expect("link");
        let mut ledger = Ledger::default();
        let r = unpack_tgz(
            &sb.archive,
            &sb.out,
            "node_modules/demo",
            &PKG,
            &mut ledger,
            &limits(),
        );
        assert!(
            refusal(r).contains("both a file and a directory"),
            "a planted link is refused, not followed"
        );
        assert!(
            std::fs::read_dir(outside.path())
                .expect("ls")
                .next()
                .is_none(),
            "nothing written through it"
        );
    }

    #[test]
    fn unpack_strips_special_modes() {
        let (sb, r) = wheel(
            &with_dist_info(vec![
                ZEntry {
                    name: "setuid.py",
                    data: b"x",
                    mode: 0o104777,
                },
                ZEntry {
                    name: "setgid.py",
                    data: b"x",
                    mode: 0o102755,
                },
                ZEntry {
                    name: "sticky.py",
                    data: b"x",
                    mode: 0o101777,
                },
                ZEntry {
                    name: "world.py",
                    data: b"x",
                    mode: 0o100666,
                },
                ZEntry {
                    name: "run.sh",
                    data: b"x",
                    mode: 0o100750,
                },
                ZEntry {
                    name: "plain.py",
                    data: b"x",
                    mode: 0o100644,
                },
                ZEntry {
                    name: "sub/",
                    data: b"",
                    mode: 0o040777,
                },
            ]),
            &limits(),
        );
        r.expect("unpack");
        let py = sb.out.join("python");
        assert_eq!(
            mode_of(&py.join("setuid.py")),
            0o555,
            "setuid dropped, exec kept"
        );
        assert_eq!(mode_of(&py.join("setgid.py")), 0o555);
        assert_eq!(mode_of(&py.join("sticky.py")), 0o555);
        assert_eq!(mode_of(&py.join("world.py")), 0o444, "world write dropped");
        assert_eq!(mode_of(&py.join("run.sh")), 0o555);
        assert_eq!(mode_of(&py.join("plain.py")), 0o444);
        assert_eq!(
            mode_of(&py.join("sub")),
            0o755,
            "directories are computed, not copied"
        );
        // Tar.
        let e = [
            TEntry {
                name: b"package/a",
                kind: tar::EntryType::Regular,
                data: b"x",
                mode: 0o6777,
                link: b"",
            },
            TEntry {
                name: b"package/b",
                kind: tar::EntryType::Regular,
                data: b"x",
                mode: 0o666,
                link: b"",
            },
            TEntry {
                name: b"package/d/",
                kind: tar::EntryType::Directory,
                data: b"",
                mode: 0o777,
                link: b"",
            },
        ];
        let (sb, r) = tgz(&e, &limits());
        r.expect("unpack");
        let base = sb.out.join("node_modules/demo");
        assert_eq!(mode_of(&base.join("a")), 0o555);
        assert_eq!(mode_of(&base.join("b")), 0o444);
        assert_eq!(mode_of(&base.join("d")), 0o755);
    }

    #[test]
    fn tree_is_reproducible() {
        let (sb, r) = wheel(&with_dist_info(vec![zf("m/a.py", b"x")]), &limits());
        r.expect("unpack");
        for p in ["python", "python/m", "python/m/a.py"] {
            let m = std::fs::symlink_metadata(sb.out.join(p)).expect("meta");
            assert_eq!(m.mtime(), 315_532_800, "{p}");
        }
    }

    #[test]
    fn unpack_bomb_declared_and_counted() {
        let big = vec![0u8; 3000];
        let small = Limits {
            max_unpacked_archive_bytes: 2000,
            max_unpacked_skill_bytes: 2000,
            ..limits()
        };
        // Declared sizes catch it up front, before anything is written.
        let (sb, r) = wheel(&with_dist_info(vec![zf("big.bin", &big)]), &small);
        let m = refusal(r);
        assert!(
            m.contains(
                "package 'demo' 1.0: skill packages are over the 2000 bytes limit (unpacked)"
            ),
            "{m}"
        );
        assert!(
            std::fs::read_dir(&sb.out).expect("ls").next().is_none(),
            "refused before writing"
        );
        // Per-file cap.
        let per_file = Limits {
            max_file_bytes: 100,
            ..limits()
        };
        assert!(
            refusal(wheel(&with_dist_info(vec![zf("big.bin", &big)]), &per_file).1)
                .contains("over the 100 bytes limit")
        );
        // A MiB-multiple cap is printed in MiB, like the production text.
        let mib = Limits {
            max_unpacked_skill_bytes: 384 * MIB,
            max_unpacked_archive_bytes: 1000,
            ..limits()
        };
        let m = refusal(wheel(&with_dist_info(vec![zf("big.bin", &big)]), &mib).1);
        assert!(m.contains("1000 bytes"), "{m}");
        assert_eq!(fmt_cap(384 * MIB), "384 MiB");
        // A tarball: the gzip stream is tiny, the expansion is not. The counting reader stops it.
        let zeros = vec![0u8; 200_000];
        let tarball = build_tgz(&[tf("package/zeros.bin", &zeros)]);
        assert!(
            tarball.len() < 2000,
            "a bomb: {} compressed bytes",
            tarball.len()
        );
        let sb = sandbox(&tarball);
        let tight = Limits {
            max_unpacked_archive_bytes: 10_000,
            max_archive_files: 10,
            ..limits()
        };
        let mut ledger = Ledger::default();
        let m = refusal(unpack_tgz(
            &sb.archive,
            &sb.out,
            "node_modules/demo",
            &PKG,
            &mut ledger,
            &tight,
        ));
        assert!(
            m.contains("skill packages are over the 10000 bytes limit (unpacked)"),
            "{m}"
        );
        // Bytes the unpacker skips are counted too: a huge global pax header is not free.
        let skipped = build_tgz(&[TEntry {
            name: b"pax_global_header",
            kind: tar::EntryType::XGlobalHeader,
            data: &zeros,
            mode: 0o644,
            link: b"",
        }]);
        let sb = sandbox(&skipped);
        let mut ledger = Ledger::default();
        let m = refusal(unpack_tgz(
            &sb.archive,
            &sb.out,
            "node_modules/demo",
            &PKG,
            &mut ledger,
            &tight,
        ));
        assert!(m.contains("over the 10000 bytes limit (unpacked)"), "{m}");
    }

    #[test]
    fn zip_lying_declared_size_is_refused() {
        // Build a stored entry, then shrink the uncompressed size in both headers.
        let mut bytes = build_zip(&with_dist_info(vec![zf("a.bin", &[7u8; 64])]));
        let sig_local = [0x50, 0x4b, 0x03, 0x04];
        let sig_central = [0x50, 0x4b, 0x01, 0x02];
        for (sig, size_off, csize_off) in [(sig_local, 22usize, 18usize), (sig_central, 24, 20)] {
            let mut i = 0;
            while i + 4 <= bytes.len() {
                if bytes[i..i + 4] == sig {
                    // Only the first entry (a.bin is first), at its first occurrence.
                    bytes[i + size_off..i + size_off + 4].copy_from_slice(&8u32.to_le_bytes());
                    bytes[i + csize_off..i + csize_off + 4].copy_from_slice(&8u32.to_le_bytes());
                    break;
                }
                i += 1;
            }
        }
        let sb = sandbox(&bytes);
        let mut ledger = Ledger::default();
        let r = unpack_wheel(&sb.archive, &sb.out, "python", &PKG, &mut ledger, &limits());
        // Whatever the zip layer does with the forged sizes, the result is a refusal.
        assert!(matches!(r, Err(DepsError::Refused(_))), "{r:?}");
    }

    #[test]
    fn unpack_file_count_and_depth_caps() {
        let names: Vec<String> = (0..6).map(|i| format!("f{i}.py")).collect();
        let entries: Vec<ZEntry> = names.iter().map(|n| zf(n, b"x")).collect();
        let few = Limits {
            max_archive_files: 5,
            ..limits()
        };
        let m = refusal(wheel(&with_dist_info(entries), &few).1);
        assert!(
            m.contains("package 'demo' 1.0: skill packages are over the 5 file limit"),
            "{m}"
        );
        // Skill-wide count.
        let skill = Limits {
            max_skill_files: 3,
            ..limits()
        };
        let entries: Vec<ZEntry> = names.iter().map(|n| zf(n, b"x")).collect();
        assert!(
            refusal(wheel(&with_dist_info(entries), &skill).1).contains("over the 3 file limit")
        );
        // Tar entry count.
        let tnames: Vec<String> = names.iter().map(|n| format!("package/{n}")).collect();
        let tentries: Vec<TEntry> = tnames.iter().map(|n| tf(n, b"x")).collect();
        assert!(refusal(tgz(&tentries, &few).1).contains("over the 5 file limit"));
        // Depth.
        let deep = format!("{}f.py", "d/".repeat(33));
        assert!(
            refusal(wheel(&with_dist_info(vec![zf(&deep, b"x")]), &limits()).1)
                .contains("deeper than 32")
        );
    }

    #[test]
    fn unpack_duplicate_and_case_fold_duplicates() {
        // Zip.
        let (_sb, r) = wheel(
            &with_dist_info(vec![zf("a.py", b"1"), zf("A.py", b"2")]),
            &limits(),
        );
        assert!(refusal(r).contains("differs only in case"));
        let (_sb, r) = wheel(
            &with_dist_info(vec![zf("a/b.py", b"1"), zf("a", b"2")]),
            &limits(),
        );
        assert!(refusal(r).contains("both a file and a directory"));
        let (_sb, r) = wheel(
            &with_dist_info(vec![zf("a", b"2"), zf("a/b.py", b"1")]),
            &limits(),
        );
        assert!(refusal(r).contains("both a file and a directory"));
        let (_sb, r) = wheel(
            &with_dist_info(vec![zf("Dir/x.py", b"1"), zf("dir/y.py", b"2")]),
            &limits(),
        );
        assert!(refusal(r).contains("differs only in case"));
        // Tar can repeat an exact name (a zip central directory cannot, the zip layer refuses it).
        let (_sb, r) = tgz(
            &[tf("package/a.js", b"1"), tf("package/a.js", b"2")],
            &limits(),
        );
        assert!(refusal(r).contains("appears twice"));
        let (_sb, r) = tgz(
            &[tf("package/a.js", b"1"), tf("package/A.JS", b"2")],
            &limits(),
        );
        assert!(refusal(r).contains("differs only in case"));
    }

    #[test]
    fn wheel_layout_data_scripts_and_purelib() {
        let (sb, r) = wheel(
            &[
                zf("demo/__init__.py", b"x = 1\n"),
                zf("demo-1.0.dist-info/METADATA", b"Name: demo\n"),
                zf(
                    "demo-1.0.dist-info/entry_points.txt",
                    b"[console_scripts]\nx = demo:main\n",
                ),
                zf("demo-1.0.data/scripts/demo-cli", b"#!/bin/sh\nrm -rf /\n"),
                zf("demo-1.0.data/headers/demo.h", b"h"),
                zf("demo-1.0.data/data/share/thing", b"d"),
                zf("demo-1.0.data/purelib/extra/__init__.py", b"p"),
                zf("demo-1.0.data/platlib/ext/_native.so", b"so"),
                zf("evil.pth", b"import os; os.system('x')\n"),
            ],
            &limits(),
        );
        r.expect("unpack");
        let py = sb.out.join("python");
        assert!(py.join("demo/__init__.py").is_file());
        assert!(py.join("demo-1.0.dist-info/METADATA").is_file());
        assert!(
            py.join("extra/__init__.py").is_file(),
            "purelib merged into the root"
        );
        assert!(
            py.join("ext/_native.so").is_file(),
            "platlib merged into the root"
        );
        assert!(
            !py.join("demo-1.0.data").exists(),
            "the .data directory itself is not kept"
        );
        for dropped in ["demo-cli", "demo.h", "share", "scripts", "headers"] {
            let found = walk(&py)
                .iter()
                .any(|p| p.file_name().is_some_and(|n| n == dropped));
            assert!(!found, "{dropped} must be dropped");
        }
    }

    #[test]
    fn wheel_data_scripts_dropped() {
        let (sb, r) = wheel(
            &[
                zf("demo-1.0.dist-info/METADATA", b"x"),
                zf("demo-1.0.data/scripts/run", b"#!/bin/sh\n"),
            ],
            &limits(),
        );
        r.expect("unpack");
        assert!(walk(&sb.out)
            .iter()
            .all(|p| !p.to_string_lossy().contains("scripts")));
        // No file in the tree is executable by accident of the scripts directory.
        assert!(!sb.out.join("python/run").exists());
    }

    fn walk(p: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        if let Ok(rd) = std::fs::read_dir(p) {
            for e in rd.flatten() {
                out.push(e.path());
                if e.path().is_dir() {
                    out.extend(walk(&e.path()));
                }
            }
        }
        out
    }

    #[test]
    fn wheel_dist_info_must_match_and_exist() {
        let (_sb, r) = wheel(&[zf("demo/__init__.py", b"x")], &limits());
        assert!(refusal(r).contains("no .dist-info"));
        let (_sb, r) = wheel(&[zf("other-2.0.dist-info/METADATA", b"x")], &limits());
        assert!(refusal(r).contains(".dist-info directory that does not match"));
        let (_sb, r) = wheel(
            &[
                zf("demo-1.0.dist-info/METADATA", b"x"),
                zf("other-1.0.data/purelib/x.py", b"x"),
            ],
            &limits(),
        );
        assert!(refusal(r).contains(".data directory that does not match"));
        // Name and version are compared normalised.
        let (_sb, r) = wheel(&[zf("Demo-1.0.0.dist-info/METADATA", b"x")], &limits());
        r.expect("normalised match");
    }

    #[test]
    fn wheel_refuses_corrupt_and_unsupported_archives() {
        let sb = sandbox(b"not a zip at all");
        let mut ledger = Ledger::default();
        assert!(refusal(unpack_wheel(
            &sb.archive,
            &sb.out,
            "python",
            &PKG,
            &mut ledger,
            &limits()
        ))
        .contains("not a valid wheel"));
        let sb = sandbox(b"not a gzip");
        assert!(refusal(unpack_tgz(
            &sb.archive,
            &sb.out,
            "node_modules/demo",
            &PKG,
            &mut ledger,
            &limits()
        ))
        .contains("not a valid .tgz"));
    }

    #[test]
    fn package_prefix_strip_for_tarballs() {
        let e = [
            tf("package/package.json", b"{}"),
            tf("package/lib/index.js", b"module.exports = 1\n"),
            TEntry {
                name: b"package/",
                kind: tar::EntryType::Directory,
                data: b"",
                mode: 0o755,
                link: b"",
            },
            tf("pkg-any-name/ignored-root-name-is-stripped.txt", b"x"),
        ];
        // The last entry has a different first component: any name is stripped, but it also
        // lands in the same package directory.
        let (sb, r) = tgz(&e, &limits());
        r.expect("unpack");
        let base = sb.out.join("node_modules/demo");
        assert!(base.join("package.json").is_file());
        assert!(base.join("lib/index.js").is_file());
        assert!(base.join("ignored-root-name-is-stripped.txt").is_file());
        assert!(!base.join("package").exists());
        // A single-component file is refused.
        let (_sb, r) = tgz(&[tf("loose.js", b"x")], &limits());
        assert!(refusal(r).contains("only one path component"));
    }

    #[test]
    fn two_packages_same_file_refused() {
        let sb = sandbox(&build_tgz(&[tf("package/index.js", b"a")]));
        let sb2 = sandbox(&build_tgz(&[tf("package/index.js", b"b")]));
        let a = Pkg {
            name: "a",
            version: "1.0.0",
        };
        let b = Pkg {
            name: "b",
            version: "2.0.0",
        };
        let mut ledger = Ledger::default();
        let root = sb.out.clone();
        unpack_tgz(
            &sb.archive,
            &root,
            "node_modules/shared",
            &a,
            &mut ledger,
            &limits(),
        )
        .expect("first");
        let m = refusal(unpack_tgz(
            &sb2.archive,
            &root,
            "node_modules/shared",
            &b,
            &mut ledger,
            &limits(),
        ));
        assert!(
            m.contains(
                "package 'a' 1.0.0 and package 'b' 2.0.0 both write 'node_modules/shared/index.js'"
            ),
            "{m}"
        );
        // Different destinations share only directories: fine.
        let sb3 = sandbox(&build_tgz(&[tf("package/index.js", b"c")]));
        let mut ledger = Ledger::default();
        unpack_tgz(
            &sb.archive,
            &sb3.out,
            "node_modules/one",
            &a,
            &mut ledger,
            &limits(),
        )
        .expect("one");
        unpack_tgz(
            &sb2.archive,
            &sb3.out,
            "node_modules/two",
            &b,
            &mut ledger,
            &limits(),
        )
        .expect("two");
        assert_eq!(ledger.files, 2);
        // A nested package whose path overlaps another package's file is a collision.
        let sb4 = sandbox(&build_tgz(&[tf("package/node_modules/inner/x.js", b"x")]));
        let sb5 = sandbox(&build_tgz(&[tf("package/x.js", b"y")]));
        let mut ledger = Ledger::default();
        let root = sb4.out.clone();
        unpack_tgz(
            &sb4.archive,
            &root,
            "node_modules/outer",
            &a,
            &mut ledger,
            &limits(),
        )
        .expect("outer");
        let m = refusal(unpack_tgz(
            &sb5.archive,
            &root,
            "node_modules/outer/node_modules/inner",
            &b,
            &mut ledger,
            &limits(),
        ));
        assert!(
            m.contains("both write 'node_modules/outer/node_modules/inner/x.js'"),
            "{m}"
        );
        // Wheels too.
        let w1 = sandbox(&build_zip(&[
            zf("shared.py", b"1"),
            zf("demo-1.0.dist-info/METADATA", b"x"),
        ]));
        let w2 = sandbox(&build_zip(&[
            zf("shared.py", b"2"),
            zf("other-1.0.dist-info/METADATA", b"x"),
        ]));
        let wa = Pkg {
            name: "demo",
            version: "1.0",
        };
        let wb = Pkg {
            name: "other",
            version: "1.0",
        };
        let mut ledger = Ledger::default();
        let root = w1.out.clone();
        unpack_wheel(&w1.archive, &root, "python", &wa, &mut ledger, &limits()).expect("w1");
        let m = refusal(unpack_wheel(
            &w2.archive,
            &root,
            "python",
            &wb,
            &mut ledger,
            &limits(),
        ));
        assert!(m.contains("both write 'python/shared.py'"), "{m}");
    }

    #[test]
    fn skill_wide_byte_cap_spans_packages() {
        let sb = sandbox(&build_tgz(&[tf("package/a.bin", &[0u8; 600])]));
        let sb2 = sandbox(&build_tgz(&[tf("package/b.bin", &[0u8; 600])]));
        let lim = Limits {
            max_unpacked_skill_bytes: 1000,
            ..limits()
        };
        let a = Pkg {
            name: "a",
            version: "1.0.0",
        };
        let b = Pkg {
            name: "b",
            version: "1.0.0",
        };
        let mut ledger = Ledger::default();
        unpack_tgz(
            &sb.archive,
            &sb.out,
            "node_modules/a",
            &a,
            &mut ledger,
            &lim,
        )
        .expect("a");
        let m = refusal(unpack_tgz(
            &sb2.archive,
            &sb.out,
            "node_modules/b",
            &b,
            &mut ledger,
            &lim,
        ));
        assert!(
            m.contains(
                "package 'b' 1.0.0: skill packages are over the 1000 bytes limit (unpacked)"
            ),
            "{m}"
        );
    }

    #[test]
    fn written_files_have_the_archive_contents() {
        let (sb, r) = wheel(
            &with_dist_info(vec![zf("pkg/mod.py", b"print('hi')\n")]),
            &limits(),
        );
        let ledger = r.expect("unpack");
        assert_eq!(
            std::fs::read(sb.out.join("python/pkg/mod.py")).expect("read"),
            b"print('hi')\n"
        );
        assert_eq!(ledger.files, 2);
        assert_eq!(ledger.bytes, 12 + 11);
    }
}
