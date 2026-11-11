use crate::error::Drive9Error;
use crate::image_read::{
    ImageEntry, ImageEntryKind, ImageLimits, ImageVisitor, TreeWriter, WalkReport,
};
use crate::vam_dir::VAM_DIR;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

const MIB: u64 = 1024 * 1024;
const MIN_IMAGE_MIB: u64 = 64;
const HEADROOM_MIB: u64 = 64;

/// Walk `dir` and return (total apparent bytes, file count).
fn dir_content_stats(dir: &Path) -> Result<(u64, u64), Drive9Error> {
    let mut total_bytes = 0u64;
    let mut file_count = 0u64;

    fn walk(path: &Path, total: &mut u64, count: &mut u64) -> Result<(), Drive9Error> {
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            let meta = entry.metadata()?;
            if meta.is_dir() {
                walk(&entry.path(), total, count)?;
            } else if meta.is_file() {
                *total = total.saturating_add(meta.len());
                *count += 1;
            }
        }
        Ok(())
    }

    walk(dir, &mut total_bytes, &mut file_count)?;
    Ok((total_bytes, file_count))
}

fn align_1mib(bytes: u64) -> u64 {
    let align = MIB;
    bytes.div_ceil(align) * align
}

fn compute_image_size(content_bytes: u64, quota_bytes: u64) -> u64 {
    let min_size = MIN_IMAGE_MIB * MIB;
    let computed = align_1mib(content_bytes.saturating_add(HEADROOM_MIB * MIB));
    std::cmp::min(quota_bytes, std::cmp::max(min_size, computed))
}

fn compute_inode_count(file_count: u64) -> u64 {
    std::cmp::max(4096, file_count.saturating_mul(4).saturating_add(1024))
}

/// Where the host's `.vam/` waits while an image is built: next to the tree, in
/// the session directory, so a rename moves it without copying a byte.
const HELD_VAM_NAME: &str = ".vam-held";

/// Puts back a `.vam/` that a build crashed while holding (a rename-based hold
/// has no other recovery). A no-op when nothing is held or `.vam/` is there.
pub(crate) fn restore_held_vam(tree: &Path) {
    let Some(parent) = tree.parent() else { return };
    let held = parent.join(HELD_VAM_NAME);
    let vam = tree.join(VAM_DIR);
    if std::fs::symlink_metadata(&held).is_ok() && std::fs::symlink_metadata(&vam).is_err() {
        let _ = std::fs::rename(&held, &vam);
    }
}

/// The host's `.vam/` renamed aside for the length of an image build. The
/// conversation and the timeline are the host's: they must not appear in the
/// guest's disk. Dropping the hold puts them back, whether the build worked or
/// not.
struct VamHold {
    vam: PathBuf,
    held: PathBuf,
}

impl VamHold {
    fn take(tree: &Path) -> std::io::Result<Option<Self>> {
        let Some(parent) = tree.parent() else {
            return Ok(None);
        };
        let vam = tree.join(VAM_DIR);
        if std::fs::symlink_metadata(&vam).is_err() {
            return Ok(None);
        }
        let held = parent.join(HELD_VAM_NAME);
        std::fs::rename(&vam, &held)?;
        Ok(Some(Self { vam, held }))
    }
}

impl Drop for VamHold {
    fn drop(&mut self) {
        if let Err(e) = std::fs::rename(&self.held, &self.vam) {
            tracing::error!(
                "could not put the host .vam/ back after an image build: {e}; \
                 the session end restores it from {HELD_VAM_NAME}"
            );
        }
    }
}

pub trait Drive9ImageIo: Send + Sync + std::fmt::Debug {
    /// Builds an ext4 image containing the files from `source_dir`.
    fn build_image(
        &self,
        source_dir: &Path,
        image_path: &Path,
        size_bytes: u64,
    ) -> Result<(), Drive9Error>;

    /// Walks the image at `image_path` with the safe reader (see
    /// [`crate::image_read`]): every directory, regular file and special entry
    /// goes to `visitor`; the top-level host `.vam/` and `lost+found` are never read.
    fn walk_image(
        &self,
        image_path: &Path,
        limits: &ImageLimits,
        visitor: &mut dyn ImageVisitor,
    ) -> Result<WalkReport, Drive9Error>;

    /// Extracts the files of `image_path` into a new `target_dir` (replacing it),
    /// through the safe reader and [`TreeWriter`]: only plain files and
    /// directories are written, under validated names, within the default
    /// [`ImageLimits`].
    fn extract_image(&self, image_path: &Path, target_dir: &Path) -> Result<(), Drive9Error> {
        let parent = target_dir
            .parent()
            .ok_or_else(|| Drive9Error::Layout("extraction target has no parent".into()))?;
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let temp_dir = parent.join(format!("extract-tmp-{}-{}", std::process::id(), nonce));
        std::fs::create_dir(&temp_dir)?;
        std::fs::set_permissions(&temp_dir, std::fs::Permissions::from_mode(0o700))?;
        let mut writer = TreeWriter::new(temp_dir.clone(), None);
        if let Err(e) = self.walk_image(image_path, &ImageLimits::from_env(), &mut writer) {
            let _ = std::fs::remove_dir_all(&temp_dir);
            return Err(e);
        }
        std::fs::set_permissions(&temp_dir, std::fs::Permissions::from_mode(0o755))?;
        if target_dir.exists() {
            std::fs::remove_dir_all(target_dir)?;
        }
        std::fs::rename(&temp_dir, target_dir)?;
        Ok(())
    }

    /// Overlays `host_dir` onto `guest_prefix` inside an existing ext4 image via debugfs.
    fn overlay_dir(
        &self,
        image_path: &Path,
        host_dir: &Path,
        guest_prefix: &str,
    ) -> Result<(), Drive9Error>;
}

#[derive(Debug, Clone)]
pub struct E2fsImageIo;

impl Default for E2fsImageIo {
    fn default() -> Self {
        Self::new()
    }
}

impl E2fsImageIo {
    pub fn new() -> Self {
        Self
    }

    fn run_debugfs_write(image: &str, command: &str) -> Result<(), Drive9Error> {
        let status = Command::new("debugfs")
            .args(["-w", "-R", command, image])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()?;
        if !status.success() {
            return Err(Drive9Error::Io(std::io::Error::other(format!(
                "debugfs command failed ({:?}): {}",
                status.code(),
                command
            ))));
        }
        Ok(())
    }

    fn ensure_guest_dir(image: &str, guest_path: &str) -> Result<(), Drive9Error> {
        let normalized = guest_path.trim_matches('/');
        if normalized.is_empty() {
            return Ok(());
        }
        let mut current = String::new();
        for segment in normalized.split('/') {
            if segment.is_empty() {
                continue;
            }
            if current.is_empty() {
                current = segment.to_string();
            } else {
                current.push('/');
                current.push_str(segment);
            }
            let mkdir_cmd = format!("mkdir /{}", current);
            let _ = Self::run_debugfs_write(image, &mkdir_cmd);
        }
        Ok(())
    }
}

impl Drive9ImageIo for E2fsImageIo {
    fn build_image(
        &self,
        source_dir: &Path,
        image_path: &Path,
        size_bytes: u64,
    ) -> Result<(), Drive9Error> {
        // The host's `.vam/` stays out of the guest's disk (the image is built from a
        // tree that has none).
        restore_held_vam(source_dir);
        let _hold = VamHold::take(source_dir)?;
        let (content_bytes, file_count) = dir_content_stats(source_dir)?;
        let image_size = if std::env::var("VAM_GOLDEN_GEOMETRY").is_ok() {
            size_bytes
        } else {
            compute_image_size(content_bytes, size_bytes)
        };
        let inode_count = compute_inode_count(file_count);

        // 1. Pre-allocate target image file
        let file = std::fs::File::create(image_path)?;
        file.set_len(image_size)?;
        drop(file);

        let source = source_dir.to_str().ok_or_else(|| {
            Drive9Error::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "source_dir path is not valid UTF-8",
            ))
        })?;
        let image = image_path.to_str().ok_or_else(|| {
            Drive9Error::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "image_path is not valid UTF-8",
            ))
        })?;
        let inode_str = inode_count.to_string();

        // 2. Run mke2fs
        let status = Command::new("mke2fs")
            .args([
                "-F",
                "-O",
                "^has_journal",
                "-m",
                "0",
                "-E",
                "lazy_itable_init=1,nodiscard",
                "-I",
                "256",
                "-N",
                &inode_str,
                "-d",
                source,
                image,
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()?;

        if !status.success() {
            return Err(Drive9Error::Io(std::io::Error::other(format!(
                "mke2fs failed with status {:?}",
                status.code()
            ))));
        }

        Ok(())
    }

    fn walk_image(
        &self,
        image_path: &Path,
        limits: &ImageLimits,
        visitor: &mut dyn ImageVisitor,
    ) -> Result<WalkReport, Drive9Error> {
        crate::image_read::walk_ext4_image(image_path, limits, visitor)
    }

    fn overlay_dir(
        &self,
        image_path: &Path,
        host_dir: &Path,
        guest_prefix: &str,
    ) -> Result<(), Drive9Error> {
        if !host_dir.is_dir() {
            return Ok(());
        }

        let image = image_path.to_str().ok_or_else(|| {
            Drive9Error::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "image_path is not valid UTF-8",
            ))
        })?;
        let prefix = guest_prefix.trim_matches('/');

        if !prefix.is_empty() {
            let rm_cmd = format!("rm -r /{}", prefix);
            let _ = Self::run_debugfs_write(image, &rm_cmd);
        }

        let mut dirs: Vec<PathBuf> = Vec::new();
        let mut files: Vec<(PathBuf, PathBuf)> = Vec::new();
        collect_overlay_entries(host_dir, host_dir, &mut dirs, &mut files)?;

        dirs.sort_by_key(|p| p.components().count());
        for rel in dirs {
            let guest = overlay_guest_path(prefix, &rel);
            Self::ensure_guest_dir(image, &guest)?;
        }

        for (rel, host_path) in files {
            let guest = overlay_guest_path(prefix, &rel);
            Self::ensure_guest_dir(image, guest.rsplit_once('/').map(|(p, _)| p).unwrap_or(""))?;
            let host_str = host_path.to_str().ok_or_else(|| {
                Drive9Error::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "overlay host path is not valid UTF-8",
                ))
            })?;
            let write_cmd = format!("write /{} {}", guest.trim_matches('/'), host_str);
            Self::run_debugfs_write(image, &write_cmd)?;
        }

        Ok(())
    }
}

fn overlay_guest_path(prefix: &str, rel: &Path) -> String {
    let rel_str = rel.to_string_lossy().replace('\\', "/");
    if prefix.is_empty() {
        rel_str
    } else if rel_str.is_empty() {
        prefix.to_string()
    } else {
        format!("{}/{}", prefix, rel_str)
    }
}

fn collect_overlay_entries(
    base: &Path,
    current: &Path,
    dirs: &mut Vec<PathBuf>,
    files: &mut Vec<(PathBuf, PathBuf)>,
) -> Result<(), Drive9Error> {
    for entry in std::fs::read_dir(current)? {
        let entry = entry?;
        let path = entry.path();
        let rel = path.strip_prefix(base).map_err(|_| {
            Drive9Error::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "overlay path escaped base directory",
            ))
        })?;
        if path.is_dir() {
            dirs.push(rel.to_path_buf());
            collect_overlay_entries(base, &path, dirs, files)?;
        } else if path.is_file() {
            files.push((rel.to_path_buf(), path));
        }
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub struct MockImageIo;

impl Drive9ImageIo for MockImageIo {
    fn build_image(
        &self,
        _source_dir: &Path,
        image_path: &Path,
        size_bytes: u64,
    ) -> Result<(), Drive9Error> {
        if let Some(parent) = image_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = std::fs::File::create(image_path)?;
        file.set_len(size_bytes).map_err(Drive9Error::Io)?;
        Ok(())
    }

    fn walk_image(
        &self,
        _image_path: &Path,
        _limits: &ImageLimits,
        visitor: &mut dyn ImageVisitor,
    ) -> Result<WalkReport, Drive9Error> {
        // One guest-written file, as the old mock extraction produced.
        const BODY: &[u8] = b"mock extracted file";
        let entry = ImageEntry {
            path: "extracted.txt".to_string(),
            kind: ImageEntryKind::File,
            size: BODY.len() as u64,
            mode: 0o644,
        };
        let mut report = WalkReport {
            entries: 1,
            files: 1,
            file_bytes: BODY.len() as u64,
            ..WalkReport::default()
        };
        if visitor.wants_content(&entry) {
            let mut content: &[u8] = BODY;
            visitor.visit(&entry, Some(&mut content))?;
        } else {
            visitor.visit(&entry, None)?;
        }
        report.entries = 1;
        Ok(report)
    }

    fn overlay_dir(
        &self,
        _image_path: &Path,
        _host_dir: &Path,
        _guest_prefix: &str,
    ) -> Result<(), Drive9Error> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_compute_image_size_respects_quota_cap() {
        // 4 MiB cap with tiny content → use cap, not 64 MiB floor
        assert_eq!(compute_image_size(100, 4 * MIB), 4 * MIB);
        // Empty workspace with 1 GiB quota → 64 MiB floor
        assert_eq!(compute_image_size(0, 1024 * MIB), 64 * MIB);
        // Large content capped by quota
        assert_eq!(compute_image_size(900 * MIB, 1024 * MIB), 964 * MIB);
    }

    #[test]
    fn the_host_vam_dir_is_kept_out_of_a_built_image_and_put_back() {
        if Command::new("mke2fs").arg("-V").output().is_err() {
            eprintln!("Skipping: mke2fs is not installed");
            return;
        }
        let dir = tempdir().unwrap();
        let session = dir.path().join("session");
        let tree = session.join("materialized");
        std::fs::create_dir_all(tree.join("workspace")).unwrap();
        std::fs::create_dir_all(tree.join(".vam/threads/main")).unwrap();
        std::fs::write(tree.join(".vam/threads/main/log.jsonl"), b"private").unwrap();
        std::fs::write(tree.join("workspace/a.txt"), b"a").unwrap();
        let img = session.join("drive9.ext4");
        E2fsImageIo::new()
            .build_image(&tree, &img, 8 * 1024 * 1024)
            .unwrap();
        // Put back, with its bytes, and nothing left held.
        assert_eq!(
            std::fs::read(tree.join(".vam/threads/main/log.jsonl")).unwrap(),
            b"private"
        );
        assert!(!session.join(HELD_VAM_NAME).exists());
        // Not in the image: the raw bytes carry the workspace file but not the log.
        let raw = std::fs::read(&img).unwrap();
        let has = |needle: &[u8]| raw.windows(needle.len()).any(|w| w == needle);
        assert!(has(b"a.txt"), "the workspace file is in the image");
        assert!(!has(b"private"), "the host .vam/ content is in the image");
        assert!(!has(b"threads"), "the host .vam/ names are in the image");
    }

    #[test]
    fn a_vam_dir_held_by_a_crashed_build_is_restored_before_the_next_one() {
        let dir = tempdir().unwrap();
        let session = dir.path().join("session");
        let tree = session.join("materialized");
        std::fs::create_dir_all(&tree).unwrap();
        std::fs::create_dir_all(session.join(HELD_VAM_NAME)).unwrap();
        std::fs::write(session.join(HELD_VAM_NAME).join("x"), b"x").unwrap();
        restore_held_vam(&tree);
        assert!(tree.join(".vam/x").is_file());
        assert!(!session.join(HELD_VAM_NAME).exists());
        // With a `.vam/` already there the held one is left alone.
        std::fs::create_dir_all(session.join(HELD_VAM_NAME)).unwrap();
        restore_held_vam(&tree);
        assert!(session.join(HELD_VAM_NAME).exists());
    }

    #[test]
    fn test_e2fs_image_io() {
        if Command::new("mke2fs").arg("-V").output().is_err()
            || Command::new("debugfs").arg("-V").output().is_err()
        {
            eprintln!("Skipping test_e2fs_image_io: mke2fs or debugfs not installed");
            return;
        }
        let dir = tempdir().unwrap();
        let src_dir = dir.path().join("src");
        let dst_dir = dir.path().join("dst");
        let img_path = dir.path().join("drive.ext4");

        std::fs::create_dir_all(&src_dir).unwrap();
        std::fs::write(src_dir.join("test.txt"), b"hello drive9 ext4").unwrap();

        let io = E2fsImageIo::new();
        io.build_image(&src_dir, &img_path, 4 * 1024 * 1024)
            .unwrap();
        assert!(img_path.exists());

        io.extract_image(&img_path, &dst_dir).unwrap();
        assert!(dst_dir.join("test.txt").exists());
        let content = std::fs::read_to_string(dst_dir.join("test.txt")).unwrap();
        assert_eq!(content, "hello drive9 ext4");
    }
}
