//! The content-addressed cache under `<drive9>/deps/` (VI-1.5): layout, `deps.json`, validation
//! on hit, atomic publish and stale-staging cleanup.
//!
//! ```text
//! deps/<key>/            key = 64 hex, one per (lockfile, ecosystem, ABI, fetcher version)
//!   python/              or node_modules/   (unpacked tree; files 0444/0555)
//!   deps.json
//! deps/.tmp/<key>-<pid>-<rand>/   staging, removed on every exit path (Drop guard)
//! ```
//! A reader never sees a partial tree: `deps/<key>` exists only as a fully built directory,
//! put there by one `rename` from the staging area on the same filesystem.

use super::limits::Limits;
use super::{DepsError, Ecosystem, FETCHER_VERSION};
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// `deps.json`, written last.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct DepsJson {
    pub v: u32,
    pub ecosystem: String,
    pub key: String,
    pub target: String,
    pub fetcher: u32,
    /// Files unpacked (directories do not count).
    pub files: u64,
    /// Unpacked bytes.
    pub bytes: u64,
    pub packages: Vec<PkgRecord>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct PkgRecord {
    pub name: String,
    pub version: String,
    /// The mirror file name (wheel) or tarball name.
    pub file: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub integrity: Option<String>,
    pub bytes: u64,
}

/// A validated cache entry.
#[derive(Debug, Clone)]
pub(crate) struct Entry {
    pub dir: PathBuf,
    pub tree: PathBuf,
    pub package_count: u32,
    pub file_count: u32,
    pub bytes: u64,
}

pub(crate) fn tree_name(eco: Ecosystem) -> &'static str {
    match eco {
        Ecosystem::Python => "python",
        Ecosystem::Js => "node_modules",
    }
}

pub(crate) struct Cache {
    root: PathBuf,
}

/// A staging directory, removed when dropped (success after the rename, or any failure).
pub(crate) struct Staging {
    path: PathBuf,
}

impl Staging {
    pub(crate) fn out(&self) -> PathBuf {
        self.path.join("out")
    }

    pub(crate) fn downloads(&self) -> PathBuf {
        self.path.join("dl")
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

pub(crate) enum Published {
    /// Our rename landed.
    Won,
    /// Another writer's tree is already there.
    Lost,
}

fn rand_hex() -> String {
    format!("{:016x}", rand::random::<u64>())
}

impl Cache {
    pub(crate) fn new(drive9_root: &Path) -> Cache {
        Cache {
            root: drive9_root.join("deps"),
        }
    }

    /// Create `deps/` and `deps/.tmp/`. Errors are returned to the caller that needs the cache.
    pub(crate) fn ensure(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(self.tmp())?;
        std::fs::set_permissions(
            &self.root,
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )?;
        Ok(())
    }

    pub(crate) fn dir_for(&self, key: &str) -> PathBuf {
        self.root.join(key)
    }

    fn tmp(&self) -> PathBuf {
        self.root.join(".tmp")
    }

    pub(crate) fn new_staging(&self, key: &str) -> Result<Staging, DepsError> {
        self.ensure()?;
        let path = self
            .tmp()
            .join(format!("{key}-{}-{}", std::process::id(), rand_hex()));
        std::fs::create_dir(&path)?;
        let staging = Staging { path };
        std::fs::create_dir(staging.out())?;
        std::fs::create_dir(staging.downloads())?;
        Ok(staging)
    }

    /// The entry for `key` if it is a complete, valid tree that fits the current limits. A bad
    /// entry is moved to `.tmp/trash-*` (one atomic rename) and reported as a miss.
    pub(crate) fn lookup(
        &self,
        key: &str,
        eco: Ecosystem,
        limits: &Limits,
    ) -> Result<Option<Entry>, DepsError> {
        let dir = self.dir_for(key);
        match std::fs::symlink_metadata(&dir) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(DepsError::Io(e)),
        }
        match self.validate(&dir, key, eco, limits) {
            Some(entry) => Ok(Some(entry)),
            None => {
                self.trash(&dir)?;
                Ok(None)
            }
        }
    }

    fn validate(&self, dir: &Path, key: &str, eco: Ecosystem, limits: &Limits) -> Option<Entry> {
        let is_real_dir = |p: &Path| {
            std::fs::symlink_metadata(p)
                .map(|m| m.file_type().is_dir())
                .unwrap_or(false)
        };
        if !is_real_dir(dir) {
            return None;
        }
        let json_path = dir.join("deps.json");
        let meta = std::fs::symlink_metadata(&json_path).ok()?;
        if !meta.file_type().is_file() || meta.len() > 8 * 1024 * 1024 {
            return None;
        }
        let mut raw = Vec::new();
        std::fs::File::open(&json_path)
            .ok()?
            .take(8 * 1024 * 1024)
            .read_to_end(&mut raw)
            .ok()?;
        let d: DepsJson = serde_json::from_slice(&raw).ok()?;
        let tree = dir.join(tree_name(eco));
        let ok = d.v == 1
            && d.key == key
            && d.ecosystem == eco.as_str()
            && d.fetcher == FETCHER_VERSION
            && is_real_dir(&tree)
            && d.bytes <= limits.max_unpacked_skill_bytes
            && d.files <= limits.max_skill_files as u64
            && d.packages.len() <= limits.max_packages;
        ok.then(|| Entry {
            dir: dir.to_path_buf(),
            tree,
            package_count: d.packages.len() as u32,
            file_count: d.files.min(u32::MAX as u64) as u32,
            bytes: d.bytes,
        })
    }

    /// Move a bad entry out of the way. Losing a race to another mover is fine.
    pub(crate) fn trash(&self, dir: &Path) -> Result<(), DepsError> {
        self.ensure()?;
        let dest = self.tmp().join(format!("trash-{}", rand_hex()));
        match std::fs::rename(dir, &dest) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(DepsError::Io(e)),
        }
    }

    /// Flush the staging tree, then rename `out/` to `deps/<key>`. `rename` onto an existing
    /// non-empty directory fails, which is the lost-race path.
    pub(crate) fn publish(&self, staging: &Staging, key: &str) -> Result<Published, DepsError> {
        sync_filesystem(&staging.out());
        match std::fs::rename(staging.out(), self.dir_for(key)) {
            Ok(()) => Ok(Published::Won),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::DirectoryNotEmpty | std::io::ErrorKind::AlreadyExists
                ) =>
            {
                Ok(Published::Lost)
            }
            Err(e) => Err(DepsError::Io(e)),
        }
    }

    /// Remove staging and trash directories left by a crash. A staging directory whose process
    /// is still alive is kept while younger than `older_than`. Returns how many were removed.
    pub(crate) fn sweep_stale_tmp(&self, older_than: Duration) -> usize {
        let Ok(rd) = std::fs::read_dir(self.tmp()) else {
            return 0;
        };
        let mut removed = 0;
        for entry in rd.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let age = entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.elapsed().ok())
                .unwrap_or(Duration::MAX);
            let pid = if name.starts_with("trash-") {
                None
            } else {
                name.rsplit('-').nth(1).and_then(|p| p.parse::<u32>().ok())
            };
            let live = pid.is_none_or(|p| Path::new(&format!("/proc/{p}")).exists());
            if age < older_than && live {
                continue;
            }
            if std::fs::remove_dir_all(entry.path()).is_ok() {
                removed += 1;
            }
        }
        removed
    }
}

/// Best effort: ask the kernel to flush the filesystem holding `path` before the rename makes
/// the tree visible. A failure only loses the durability hint.
fn sync_filesystem(path: &Path) {
    if let Ok(f) = std::fs::File::open(path) {
        use std::os::fd::AsRawFd;
        // SAFETY: `syncfs` takes a valid open descriptor and has no other preconditions.
        unsafe {
            libc::syncfs(f.as_raw_fd());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> String {
        "a".repeat(64)
    }

    fn write_entry(
        cache: &Cache,
        key: &str,
        eco: Ecosystem,
        mutate: impl FnOnce(&mut DepsJson),
    ) -> PathBuf {
        cache.ensure().expect("ensure");
        let dir = cache.dir_for(key);
        std::fs::create_dir_all(dir.join(tree_name(eco))).expect("mkdir");
        let mut d = DepsJson {
            v: 1,
            ecosystem: eco.as_str().into(),
            key: key.into(),
            target: "t".into(),
            fetcher: FETCHER_VERSION,
            files: 3,
            bytes: 100,
            packages: vec![PkgRecord {
                name: "a".into(),
                version: "1".into(),
                file: "a.whl".into(),
                sha256: Some("ab".into()),
                integrity: None,
                bytes: 100,
            }],
        };
        mutate(&mut d);
        std::fs::write(dir.join("deps.json"), serde_json::to_vec(&d).expect("json"))
            .expect("write");
        dir
    }

    fn tmp_entries(cache: &Cache) -> usize {
        std::fs::read_dir(cache.tmp())
            .map(|d| d.count())
            .unwrap_or(0)
    }

    #[test]
    fn valid_entry_is_a_hit() {
        let root = tempfile::tempdir().expect("tempdir");
        let cache = Cache::new(root.path());
        write_entry(&cache, &key(), Ecosystem::Python, |_| {});
        let e = cache
            .lookup(&key(), Ecosystem::Python, &Limits::default())
            .expect("lookup")
            .expect("hit");
        assert_eq!((e.package_count, e.file_count, e.bytes), (1, 3, 100));
        assert!(e.tree.ends_with("python"));
        assert!(cache
            .lookup(&"b".repeat(64), Ecosystem::Python, &Limits::default())
            .expect("lookup")
            .is_none());
    }

    #[test]
    fn corrupt_cache_entry_is_trashed_and_a_miss() {
        let limits = Limits::default();
        type Setup = Box<dyn Fn(&Cache)>;
        let cases: Vec<(&str, Setup)> = vec![
            (
                "version",
                Box::new(|c| {
                    write_entry(c, &key(), Ecosystem::Python, |d| d.v = 2);
                }),
            ),
            (
                "key",
                Box::new(|c| {
                    write_entry(c, &key(), Ecosystem::Python, |d| d.key = "b".repeat(64));
                }),
            ),
            (
                "ecosystem",
                Box::new(|c| {
                    write_entry(c, &key(), Ecosystem::Python, |d| d.ecosystem = "js".into());
                }),
            ),
            (
                "fetcher",
                Box::new(|c| {
                    write_entry(c, &key(), Ecosystem::Python, |d| d.fetcher = 99);
                }),
            ),
            (
                "garbage json",
                Box::new(|c| {
                    let d = write_entry(c, &key(), Ecosystem::Python, |_| {});
                    std::fs::write(d.join("deps.json"), b"{nope").expect("w");
                }),
            ),
            (
                "no deps.json",
                Box::new(|c| {
                    let d = write_entry(c, &key(), Ecosystem::Python, |_| {});
                    std::fs::remove_file(d.join("deps.json")).expect("rm");
                }),
            ),
            (
                "no tree",
                Box::new(|c| {
                    let d = write_entry(c, &key(), Ecosystem::Python, |_| {});
                    std::fs::remove_dir(d.join("python")).expect("rm");
                }),
            ),
            (
                "tree is a file",
                Box::new(|c| {
                    let d = write_entry(c, &key(), Ecosystem::Python, |_| {});
                    std::fs::remove_dir(d.join("python")).expect("rm");
                    std::fs::write(d.join("python"), b"x").expect("w");
                }),
            ),
            (
                "empty dir",
                Box::new(|c| {
                    c.ensure().expect("ensure");
                    std::fs::create_dir(c.dir_for(&key())).expect("mkdir");
                }),
            ),
            (
                "a file",
                Box::new(|c| {
                    c.ensure().expect("ensure");
                    std::fs::write(c.dir_for(&key()), b"x").expect("w");
                }),
            ),
            (
                "a symlink",
                Box::new(|c| {
                    c.ensure().expect("ensure");
                    let real = write_entry(c, &"c".repeat(64), Ecosystem::Python, |d| {
                        d.key = "c".repeat(64)
                    });
                    std::os::unix::fs::symlink(real, c.dir_for(&key())).expect("link");
                }),
            ),
        ];
        for (name, setup) in cases {
            let root = tempfile::tempdir().expect("tempdir");
            let cache = Cache::new(root.path());
            setup(&cache);
            let r = cache
                .lookup(&key(), Ecosystem::Python, &limits)
                .expect("lookup");
            assert!(r.is_none(), "{name}: expected a miss");
            assert!(
                !cache.dir_for(&key()).exists(),
                "{name}: the bad entry was moved away"
            );
            assert!(tmp_entries(&cache) >= 1, "{name}: it went to .tmp/trash-*");
        }
    }

    #[test]
    fn cap_shrink_invalidates_a_hit() {
        let root = tempfile::tempdir().expect("tempdir");
        let cache = Cache::new(root.path());
        write_entry(&cache, &key(), Ecosystem::Python, |d| {
            d.bytes = 5000;
            d.files = 10;
        });
        assert!(cache
            .lookup(&key(), Ecosystem::Python, &Limits::default())
            .expect("l")
            .is_some());
        let smaller = Limits {
            max_unpacked_skill_bytes: 4000,
            ..Limits::default()
        };
        assert!(
            cache
                .lookup(&key(), Ecosystem::Python, &smaller)
                .expect("l")
                .is_none(),
            "bytes cap shrank"
        );
        write_entry(&cache, &key(), Ecosystem::Python, |d| {
            d.bytes = 5000;
            d.files = 10;
        });
        let fewer = Limits {
            max_skill_files: 9,
            ..Limits::default()
        };
        assert!(
            cache
                .lookup(&key(), Ecosystem::Python, &fewer)
                .expect("l")
                .is_none(),
            "file cap shrank"
        );
        write_entry(&cache, &key(), Ecosystem::Python, |d| {
            d.bytes = 5000;
            d.files = 10;
        });
        let pk = Limits {
            max_packages: 0,
            ..Limits::default()
        };
        assert!(
            cache
                .lookup(&key(), Ecosystem::Python, &pk)
                .expect("l")
                .is_none(),
            "package cap shrank"
        );
    }

    #[test]
    fn staging_is_removed_on_drop() {
        let root = tempfile::tempdir().expect("tempdir");
        let cache = Cache::new(root.path());
        let path = {
            let s = cache.new_staging(&key()).expect("staging");
            std::fs::write(s.out().join("f"), b"x").expect("w");
            assert!(s.downloads().is_dir());
            s.path.clone()
        };
        assert!(!path.exists());
        assert_eq!(tmp_entries(&cache), 0, "no tmp left");
    }

    #[test]
    fn publish_wins_then_loses_to_the_existing_tree() {
        let root = tempfile::tempdir().expect("tempdir");
        let cache = Cache::new(root.path());
        let s1 = cache.new_staging(&key()).expect("staging");
        std::fs::write(s1.out().join("deps.json"), b"{}").expect("w");
        assert!(matches!(
            cache.publish(&s1, &key()).expect("publish"),
            Published::Won
        ));
        assert!(cache.dir_for(&key()).join("deps.json").is_file());
        // A second writer with the same key: its rename hits a non-empty directory.
        let s2 = cache.new_staging(&key()).expect("staging");
        std::fs::write(s2.out().join("deps.json"), b"{}").expect("w");
        assert!(matches!(
            cache.publish(&s2, &key()).expect("publish"),
            Published::Lost
        ));
        drop((s1, s2));
        assert_eq!(tmp_entries(&cache), 0);
    }

    #[test]
    fn sweep_removes_dead_and_old_staging_but_keeps_live_young() {
        let root = tempfile::tempdir().expect("tempdir");
        let cache = Cache::new(root.path());
        cache.ensure().expect("ensure");
        let mine = cache
            .tmp()
            .join(format!("{}-{}-{}", key(), std::process::id(), "rand"));
        let dead = cache
            .tmp()
            .join(format!("{}-{}-{}", key(), 4_000_000, "rand"));
        let trash = cache.tmp().join("trash-1234");
        for d in [&mine, &dead, &trash] {
            std::fs::create_dir_all(d.join("sub")).expect("mkdir");
            std::fs::write(d.join("sub/f"), b"x").expect("w");
        }
        // Young: the live one and the trash stay, the dead pid goes.
        assert_eq!(cache.sweep_stale_tmp(Duration::from_secs(3600)), 1);
        assert!(mine.exists() && trash.exists() && !dead.exists());
        // Everything is older than zero.
        assert_eq!(cache.sweep_stale_tmp(Duration::ZERO), 2);
        assert_eq!(tmp_entries(&cache), 0);
        assert_eq!(
            Cache::new(&root.path().join("nowhere")).sweep_stale_tmp(Duration::ZERO),
            0
        );
    }
}
