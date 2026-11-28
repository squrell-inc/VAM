//! The lockfile inputs of one skill, in memory. `from_dir` is the only function in the pure half
//! of this module tree's neighbourhood that reads the disk, and it reads at most the five lockfile
//! names at the skill root and checks for a `node_modules` directory.

use super::DepsError;
use std::io::Read;
use std::path::Path;

pub(crate) const REQUIREMENTS: &str = "requirements.txt";
pub(crate) const PYLOCK: &str = "pylock.toml";
pub(crate) const PACKAGE_JSON: &str = "package.json";
pub(crate) const PACKAGE_LOCK: &str = "package-lock.json";

/// Lockfiles of other tools, refused by name when present.
pub(crate) const REFUSED_LOCKFILES: [&str; 5] = [
    "uv.lock",
    "bun.lock",
    "bun.lockb",
    "yarn.lock",
    "pnpm-lock.yaml",
];

/// Read bound per file: the default lockfile cap plus one byte, so an oversized file is seen as
/// oversized by the reader and refused there.
const READ_BOUND: u64 = 4 * 1024 * 1024 + 1;

/// What the skill ships at its root that this module looks at.
#[derive(Debug, Clone, Default)]
pub struct SkillFiles {
    pub(crate) requirements_txt: Option<Vec<u8>>,
    pub(crate) pylock_toml: Option<Vec<u8>>,
    pub(crate) package_json: Option<Vec<u8>>,
    pub(crate) package_lock_json: Option<Vec<u8>>,
    /// Names of lockfiles this module refuses (`uv.lock`, `bun.lock`, ...), in the order found.
    pub(crate) refused: Vec<String>,
    pub(crate) has_node_modules: bool,
}

impl SkillFiles {
    /// From `(file name, bytes)` pairs; names this module does not look at are ignored.
    pub fn from_entries(entries: &[(&str, &[u8])], has_node_modules: bool) -> SkillFiles {
        let mut f = SkillFiles {
            has_node_modules,
            ..SkillFiles::default()
        };
        for (name, bytes) in entries {
            match *name {
                REQUIREMENTS => f.requirements_txt = Some(bytes.to_vec()),
                PYLOCK => f.pylock_toml = Some(bytes.to_vec()),
                PACKAGE_JSON => f.package_json = Some(bytes.to_vec()),
                PACKAGE_LOCK => f.package_lock_json = Some(bytes.to_vec()),
                other if REFUSED_LOCKFILES.contains(&other) => f.refused.push(other.to_string()),
                _ => {}
            }
        }
        f
    }

    /// Read the lockfiles at `skill_root`. A lockfile that is a symlink or not a regular file is
    /// refused: the bytes read are the bytes keyed and fetched.
    pub fn from_dir(skill_root: &Path) -> Result<SkillFiles, DepsError> {
        let mut f = SkillFiles::default();
        for name in [REQUIREMENTS, PYLOCK, PACKAGE_JSON, PACKAGE_LOCK] {
            let path = skill_root.join(name);
            let meta = match std::fs::symlink_metadata(&path) {
                Ok(m) => m,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(DepsError::Io(e)),
            };
            if !meta.file_type().is_file() {
                return Err(super::refused(format!(
                    "{name} is not a regular file; ship the lockfile itself, not a link"
                )));
            }
            let mut buf = Vec::new();
            std::fs::File::open(&path)?
                .take(READ_BOUND)
                .read_to_end(&mut buf)?;
            match name {
                REQUIREMENTS => f.requirements_txt = Some(buf),
                PYLOCK => f.pylock_toml = Some(buf),
                PACKAGE_JSON => f.package_json = Some(buf),
                _ => f.package_lock_json = Some(buf),
            }
        }
        for name in REFUSED_LOCKFILES {
            if std::fs::symlink_metadata(skill_root.join(name)).is_ok() {
                f.refused.push(name.to_string());
            }
        }
        f.has_node_modules = std::fs::symlink_metadata(skill_root.join("node_modules")).is_ok();
        Ok(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_dir_reads_the_known_names_only() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("requirements.txt"), b"a").expect("write");
        std::fs::write(dir.path().join("package.json"), b"{}").expect("write");
        std::fs::write(dir.path().join("uv.lock"), b"x").expect("write");
        std::fs::write(dir.path().join("other.txt"), b"x").expect("write");
        std::fs::create_dir(dir.path().join("node_modules")).expect("mkdir");
        let f = SkillFiles::from_dir(dir.path()).expect("read");
        assert_eq!(f.requirements_txt.as_deref(), Some(&b"a"[..]));
        assert_eq!(f.package_json.as_deref(), Some(&b"{}"[..]));
        assert!(f.pylock_toml.is_none() && f.package_lock_json.is_none());
        assert_eq!(f.refused, vec!["uv.lock"]);
        assert!(f.has_node_modules);
    }

    #[test]
    fn from_dir_refuses_a_linked_lockfile() {
        let dir = tempfile::tempdir().expect("tempdir");
        let outside = tempfile::tempdir().expect("tempdir");
        std::fs::write(outside.path().join("secret"), b"x").expect("write");
        std::os::unix::fs::symlink(
            outside.path().join("secret"),
            dir.path().join("requirements.txt"),
        )
        .expect("link");
        assert!(
            matches!(SkillFiles::from_dir(dir.path()), Err(DepsError::Refused(m)) if m.contains("not a regular file"))
        );
    }

    #[test]
    fn from_dir_bounds_the_read() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("requirements.txt"),
            vec![b'#'; 5 * 1024 * 1024],
        )
        .expect("write");
        let f = SkillFiles::from_dir(dir.path()).expect("read");
        assert_eq!(f.requirements_txt.map(|b| b.len() as u64), Some(READ_BOUND));
    }

    #[test]
    fn from_entries_routes_by_name() {
        let f = SkillFiles::from_entries(
            &[
                ("requirements.txt", b"r"),
                ("bun.lockb", b"b"),
                ("README.md", b"x"),
            ],
            true,
        );
        assert_eq!(f.requirements_txt.as_deref(), Some(&b"r"[..]));
        assert_eq!(f.refused, vec!["bun.lockb"]);
        assert!(f.has_node_modules);
    }
}
