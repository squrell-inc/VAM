use crate::cas::{sha256_hex, ChunkStore};
use crate::config::Drive9Layout;
use crate::error::Drive9Error;
use rusqlite::{params, Connection};
use std::fs;
use std::path::Path;
use std::sync::Mutex;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloneStrategy {
    Reflink,
    Hardlink,
    Copy,
}

pub struct CacheManager {
    layout: Drive9Layout,
    chunk_store: ChunkStore,
    conn: Mutex<Connection>,
    strategy: CloneStrategy,
    max_size_bytes: u64,
}

impl CacheManager {
    pub fn new(
        layout: Drive9Layout,
        chunk_store: ChunkStore,
        max_size_bytes: u64,
    ) -> Result<Self, Drive9Error> {
        let cache_dir = layout.cache_dir();
        fs::create_dir_all(&cache_dir)?;

        let db_path = layout.cache_db_path();
        let conn = Connection::open(db_path)?;
        conn.execute(
            "CREATE TABLE IF NOT EXISTS chunk_cache (
                hash TEXT PRIMARY KEY,
                size_bytes INTEGER NOT NULL,
                last_accessed_at INTEGER NOT NULL,
                pinned_count INTEGER NOT NULL DEFAULT 0
            )",
            [],
        )?;

        let strategy = Self::probe_clone_strategy(&cache_dir);

        Ok(Self {
            layout,
            chunk_store,
            conn: Mutex::new(conn),
            strategy,
            max_size_bytes,
        })
    }

    pub fn strategy(&self) -> CloneStrategy {
        self.strategy
    }

    /// Probes the filesystem to determine the most efficient cloning strategy.
    fn probe_clone_strategy(cache_dir: &Path) -> CloneStrategy {
        let probe_src = cache_dir.join(".probe-src");
        let probe_dst = cache_dir.join(".probe-dst");

        let _ = fs::write(&probe_src, b"probe content");

        // 1. Try Reflink
        #[cfg(unix)]
        {
            let status = std::process::Command::new("cp")
                .args([
                    "--reflink=always",
                    probe_src.to_str().unwrap(),
                    probe_dst.to_str().unwrap(),
                ])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
            if let Ok(stat) = status {
                if stat.success() {
                    let _ = fs::remove_file(&probe_src);
                    let _ = fs::remove_file(&probe_dst);
                    return CloneStrategy::Reflink;
                }
            }
        }

        // 2. Try Hardlink
        let _ = fs::remove_file(&probe_dst);
        if fs::hard_link(&probe_src, &probe_dst).is_ok() {
            let _ = fs::remove_file(&probe_src);
            let _ = fs::remove_file(&probe_dst);
            return CloneStrategy::Hardlink;
        }

        // 3. Fallback to Copy
        let _ = fs::remove_file(&probe_src);
        let _ = fs::remove_file(&probe_dst);
        CloneStrategy::Copy
    }

    /// Clone/link a file from source to target based on the chosen strategy.
    pub fn clone_file(&self, src: &Path, dst: &Path) -> Result<(), Drive9Error> {
        self.clone_file_with_strategy(src, dst, self.strategy)
    }

    /// Clone a writable ext4 image: reflink when available, otherwise copy.
    /// Hardlinks are never used — two sessions must not share one inode.
    pub fn clone_image(&self, src: &Path, dst: &Path) -> Result<(), Drive9Error> {
        let strategy = match self.strategy {
            CloneStrategy::Reflink => CloneStrategy::Reflink,
            CloneStrategy::Hardlink | CloneStrategy::Copy => CloneStrategy::Copy,
        };
        self.clone_file_with_strategy(src, dst, strategy)
    }

    fn clone_file_with_strategy(
        &self,
        src: &Path,
        dst: &Path,
        strategy: CloneStrategy,
    ) -> Result<(), Drive9Error> {
        if let Some(parent) = dst.parent() {
            crate::error::io_at(fs::create_dir_all(parent), "clone.create_parent", parent)?;
        }
        if dst.exists() {
            crate::error::io_at(fs::remove_file(dst), "clone.remove_existing", dst)?;
        }

        match strategy {
            CloneStrategy::Reflink => {
                let src_str = src.to_str().ok_or_else(|| {
                    Drive9Error::Io(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "clone source path is not valid UTF-8",
                    ))
                })?;
                let dst_str = dst.to_str().ok_or_else(|| {
                    Drive9Error::Io(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "clone destination path is not valid UTF-8",
                    ))
                })?;
                let status = std::process::Command::new("cp")
                    .args(["--reflink=always", src_str, dst_str])
                    .status()?;
                if !status.success() {
                    crate::error::io_at(
                        crate::sparse::copy_sparse(src, dst),
                        "clone.copy_after_reflink_failed",
                        dst,
                    )?;
                }
            }
            CloneStrategy::Hardlink => {
                if fs::hard_link(src, dst).is_err() {
                    crate::error::io_at(
                        crate::sparse::copy_sparse(src, dst),
                        "clone.copy_after_hardlink_failed",
                        dst,
                    )?;
                }
            }
            CloneStrategy::Copy => {
                crate::error::io_at(crate::sparse::copy_sparse(src, dst), "clone.copy", dst)?;
            }
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn with_strategy_for_test(
        layout: Drive9Layout,
        chunk_store: ChunkStore,
        max_size_bytes: u64,
        strategy: CloneStrategy,
    ) -> Result<Self, Drive9Error> {
        let cache_dir = layout.cache_dir();
        fs::create_dir_all(&cache_dir)?;
        let db_path = layout.cache_db_path();
        let conn = Connection::open(db_path)?;
        conn.execute(
            "CREATE TABLE IF NOT EXISTS chunk_cache (
                hash TEXT PRIMARY KEY,
                size_bytes INTEGER NOT NULL,
                last_accessed_at INTEGER NOT NULL,
                pinned_count INTEGER NOT NULL DEFAULT 0
            )",
            [],
        )?;
        Ok(Self {
            layout,
            chunk_store,
            conn: Mutex::new(conn),
            strategy,
            max_size_bytes,
        })
    }

    /// Admits a chunk to the cache. Computes its SHA-256 independently (anti-poisoning).
    pub fn admit_chunk(&self, bytes: &[u8]) -> Result<String, Drive9Error> {
        // Independently compute hash
        let hash = sha256_hex(bytes);

        // Store chunk in ChunkStore
        self.chunk_store.put_chunk(bytes)?;

        // Enforce size limits and record in index
        let size = bytes.len() as i64;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        {
            let conn = self.conn.lock().unwrap();
            conn.execute(
                "INSERT OR REPLACE INTO chunk_cache (hash, size_bytes, last_accessed_at, pinned_count)
                 VALUES (?1, ?2, ?3, COALESCE((SELECT pinned_count FROM chunk_cache WHERE hash=?1), 0))",
                params![hash, size, now],
            )?;
        }

        self.enforce_lru_limits()?;

        Ok(hash)
    }

    /// Retrieves a chunk from the cache index and underlying chunk store, updating access time
    /// and recording hit/miss metrics (Decision T20 low-cardinality compliant).
    pub fn get_chunk(&self, hash: &str) -> Result<Option<Vec<u8>>, Drive9Error> {
        let conn = self.conn.lock().unwrap();
        let exists: bool = conn
            .query_row("SELECT 1 FROM chunk_cache WHERE hash = ?1", [hash], |_| {
                Ok(true)
            })
            .unwrap_or(false);

        let reg = vam_telemetry::VamMetricsRegistry::global();
        if exists && self.chunk_store.has_chunk(hash) {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64;
            let _ = conn.execute(
                "UPDATE chunk_cache SET last_accessed_at = ?1 WHERE hash = ?2",
                params![now, hash],
            );
            drop(conn);

            let _ = reg.record_counter(
                vam_telemetry::METRIC_DRIVE9_CACHE_HITS_TOTAL,
                1,
                &[("subsystem", "drive9"), ("result", "hit")],
            );
            let bytes = self.chunk_store.get_chunk(hash)?;
            Ok(Some(bytes))
        } else {
            drop(conn);
            let _ = reg.record_counter(
                vam_telemetry::METRIC_DRIVE9_CACHE_MISSES_TOTAL,
                1,
                &[("subsystem", "drive9"), ("result", "miss")],
            );
            Ok(None)
        }
    }

    /// Pins a list of chunk hashes to prevent their eviction during an active session.
    pub fn pin_chunks(&self, hashes: &[String]) -> Result<(), Drive9Error> {
        let conn = self.conn.lock().unwrap();
        for hash in hashes {
            conn.execute(
                "UPDATE chunk_cache SET pinned_count = pinned_count + 1 WHERE hash = ?1",
                [hash],
            )?;
        }
        Ok(())
    }

    /// Unpins a list of chunk hashes when the session terminates.
    pub fn unpin_chunks(&self, hashes: &[String]) -> Result<(), Drive9Error> {
        let conn = self.conn.lock().unwrap();
        for hash in hashes {
            conn.execute(
                "UPDATE chunk_cache SET pinned_count = MAX(0, pinned_count - 1) WHERE hash = ?1",
                [hash],
            )?;
        }
        Ok(())
    }

    /// Evicts LRU chunks if total size exceeds limit.
    fn enforce_lru_limits(&self) -> Result<(), Drive9Error> {
        let conn = self.conn.lock().unwrap();

        // 1. Calculate total unpinned cache size
        let total_size: i64 = conn.query_row(
            "SELECT COALESCE(SUM(size_bytes), 0) FROM chunk_cache",
            [],
            |row| row.get(0),
        )?;

        if total_size as u64 <= self.max_size_bytes {
            return Ok(());
        }

        // 2. Select unpinned chunks in LRU order
        let mut stmt = conn.prepare(
            "SELECT hash, size_bytes FROM chunk_cache WHERE pinned_count = 0 ORDER BY last_accessed_at ASC",
        )?;
        let mut rows = stmt.query([])?;

        let mut current_size = total_size;
        let mut to_delete = Vec::new();

        while let Some(row) = rows.next()? {
            let hash: String = row.get(0)?;
            let size_bytes: i64 = row.get(1)?;

            to_delete.push(hash);
            current_size -= size_bytes;

            if current_size as u64 <= self.max_size_bytes {
                break;
            }
        }

        // Drop the stmt / rows borrow before mutably updating the DB
        drop(rows);
        drop(stmt);

        // 3. Delete from filesystem and DB
        for hash in to_delete {
            let chunk_path = self.layout.chunk_path(&hash);
            if chunk_path.exists() {
                let _ = fs::remove_file(chunk_path);
            }
            conn.execute("DELETE FROM chunk_cache WHERE hash = ?1", [hash])?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Drive9Config;
    use tempfile::tempdir;

    #[test]
    fn test_cache_manager_lru_eviction() {
        let dir = tempdir().unwrap();
        let config = Drive9Config::new(dir.path());
        let layout = config.layout();
        let chunk_store = ChunkStore::new(layout.clone());

        // Max cache size 2 MiB
        let manager = CacheManager::new(layout, chunk_store, 2 * 1024 * 1024).unwrap();

        // Admit 3 chunks of 1 MiB each
        let chunk1 = vec![1u8; 1024 * 1024];
        let chunk2 = vec![2u8; 1024 * 1024];
        let chunk3 = vec![3u8; 1024 * 1024];

        let hash1 = manager.admit_chunk(&chunk1).unwrap();
        // Pin chunk 1 so it cannot be evicted
        manager.pin_chunks(std::slice::from_ref(&hash1)).unwrap();

        let hash2 = manager.admit_chunk(&chunk2).unwrap();
        let hash3 = manager.admit_chunk(&chunk3).unwrap();

        // Total admitted size is 3 MiB. Limit is 2 MiB.
        // Chunk 1 is pinned. Chunk 2 was admitted before Chunk 3, so Chunk 2 (LRU) should be evicted.
        // Let's assert Chunk 2 is evicted and Chunk 1, Chunk 3 remain.
        assert!(manager.chunk_store.has_chunk(&hash1));
        assert!(!manager.chunk_store.has_chunk(&hash2));
        assert!(manager.chunk_store.has_chunk(&hash3));

        // Unpin and re-evict
        manager.unpin_chunks(std::slice::from_ref(&hash1)).unwrap();
    }

    #[test]
    fn test_clone_image_never_hardlinks() {
        let dir = tempdir().unwrap();
        let config = Drive9Config::new(dir.path());
        let layout = config.layout();
        let chunk_store = ChunkStore::new(layout.clone());
        let manager = CacheManager::with_strategy_for_test(
            layout,
            chunk_store,
            10 * 1024 * 1024,
            CloneStrategy::Hardlink,
        )
        .unwrap();

        let src = dir.path().join("src.ext4");
        let dst = dir.path().join("dst.ext4");
        fs::write(&src, b"mock ext4 image bytes").unwrap();

        manager.clone_image(&src, &dst).unwrap();
        assert!(dst.exists());

        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let src_ino = fs::metadata(&src).unwrap().ino();
            let dst_ino = fs::metadata(&dst).unwrap().ino();
            assert_ne!(
                src_ino, dst_ino,
                "clone_image must not hardlink writable images"
            );
        }
    }

    #[test]
    fn test_cache_manager_get_chunk_metrics() {
        let dir = tempdir().unwrap();
        let config = Drive9Config::new(dir.path());
        let layout = config.layout();
        let chunk_store = ChunkStore::new(layout.clone());
        let manager = CacheManager::new(layout, chunk_store, 10 * 1024 * 1024).unwrap();

        let data = b"sample drive9 chunk data";
        let hash = manager.admit_chunk(data).unwrap();

        // Query cache hit
        let hit = manager.get_chunk(&hash).unwrap();
        assert_eq!(hit, Some(data.to_vec()));

        // Query cache miss
        let miss = manager.get_chunk("nonexistent-hash-1234").unwrap();
        assert_eq!(miss, None);

        let reg = vam_telemetry::VamMetricsRegistry::global();
        let hits = reg.get_counter(
            vam_telemetry::METRIC_DRIVE9_CACHE_HITS_TOTAL,
            &[("subsystem", "drive9"), ("result", "hit")],
        );
        assert!(hits.unwrap_or(0) >= 1);

        let misses = reg.get_counter(
            vam_telemetry::METRIC_DRIVE9_CACHE_MISSES_TOTAL,
            &[("subsystem", "drive9"), ("result", "miss")],
        );
        assert!(misses.unwrap_or(0) >= 1);

        let chunk_count = reg.get_counter(
            vam_telemetry::METRIC_DRIVE9_CHUNK_COUNT,
            &[("subsystem", "drive9")],
        );
        assert!(chunk_count.unwrap_or(0) >= 1);

        let chunk_bytes = reg.get_counter(
            vam_telemetry::METRIC_DRIVE9_CHUNK_BYTES,
            &[("subsystem", "drive9")],
        );
        assert!(chunk_bytes.unwrap_or(0) >= data.len() as u64);
    }
}
