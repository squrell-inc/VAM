use crate::config::Drive9Layout;
use crate::error::Drive9Error;
use sha2::{Digest, Sha256};
use std::fs;

/// Splits bytes into content-defined chunks (min 1 MiB, max 8 MiB, average target 4 MiB)
pub fn chunk_bytes(data: &[u8]) -> Vec<Vec<u8>> {
    let min_chunk = 1024 * 1024;
    let max_chunk = 8 * 1024 * 1024;
    let mask: u64 = 0x3F_FFFF; // 22 bits for ~4 MiB chunks

    let mut chunks = Vec::new();
    let mut start = 0;
    let n = data.len();

    while start < n {
        let mut end = start + min_chunk;
        if end >= n {
            chunks.push(data[start..n].to_vec());
            break;
        }

        // Sliding window initial hash seed
        let mut hash: u64 = 0;
        let window_size = 48;
        let seed_end = std::cmp::min(end + window_size, n);
        for &byte in data.iter().take(seed_end).skip(end) {
            hash = hash.wrapping_mul(31).wrapping_add(byte as u64);
        }
        end = seed_end;

        while end < n && end - start < max_chunk {
            hash = hash.wrapping_mul(31).wrapping_add(data[end] as u64);
            if (hash & mask) == 0 {
                break;
            }
            end += 1;
        }

        end = std::cmp::min(end, n);
        chunks.push(data[start..end].to_vec());
        start = end;
    }

    chunks
}

/// Helper to compute the SHA-256 digest hex of a byte slice
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

/// Content-Addressable Chunk Store
#[derive(Clone, Debug)]
pub struct ChunkStore {
    layout: Drive9Layout,
}

impl ChunkStore {
    pub fn new(layout: Drive9Layout) -> Self {
        Self { layout }
    }

    /// Stores a raw chunk. Returns its SHA-256 hex hash.
    /// Uses write-to-temp and atomic rename to prevent corruption.
    pub fn put_chunk(&self, bytes: &[u8]) -> Result<String, Drive9Error> {
        let hash = sha256_hex(bytes);
        let path = self.layout.chunk_path(&hash);

        let reg = vam_telemetry::VamMetricsRegistry::global();
        let _ = reg.record_counter(
            vam_telemetry::METRIC_DRIVE9_CHUNK_COUNT,
            1,
            &[("subsystem", "drive9")],
        );
        let _ = reg.record_counter(
            vam_telemetry::METRIC_DRIVE9_CHUNK_BYTES,
            bytes.len() as u64,
            &[("subsystem", "drive9")],
        );

        if path.exists() {
            return Ok(hash);
        }

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        // Write to temp file in same directory for atomic rename
        let parent_dir = path.parent().unwrap();
        let temp_path = parent_dir.join(format!(".tmp-{}", hash));
        fs::write(&temp_path, bytes)?;

        if let Err(e) = fs::rename(&temp_path, &path) {
            let _ = fs::remove_file(&temp_path);
            return Err(Drive9Error::Io(e));
        }

        Ok(hash)
    }

    /// Retrieves a chunk's bytes by its SHA-256 hex hash.
    pub fn get_chunk(&self, hash: &str) -> Result<Vec<u8>, Drive9Error> {
        let path = self.layout.chunk_path(hash);
        if !path.exists() {
            return Err(Drive9Error::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("Chunk {} not found", hash),
            )));
        }
        let bytes = fs::read(&path)?;
        let verified_hash = sha256_hex(&bytes);
        if verified_hash != hash {
            return Err(Drive9Error::ChecksumMismatch {
                expected: hash.to_string(),
                actual: verified_hash,
            });
        }
        Ok(bytes)
    }

    /// Checks if a chunk is present.
    pub fn has_chunk(&self, hash: &str) -> bool {
        self.layout.chunk_path(hash).exists()
    }

    /// Stores a policy bundle text in the CAS chunk store. Returns its SHA-256 hex digest.
    pub fn put_policy_bundle(&self, policy_str: &str) -> Result<String, Drive9Error> {
        self.put_chunk(policy_str.as_bytes())
    }

    /// Retrieves a policy bundle text by its SHA-256 hex digest.
    pub fn get_policy_bundle(&self, digest: &str) -> Result<String, Drive9Error> {
        let bytes = self.get_chunk(digest)?;
        String::from_utf8(bytes).map_err(|e| {
            Drive9Error::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("Invalid UTF-8 in policy bundle: {}", e),
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Drive9Config;
    use tempfile::tempdir;

    #[test]
    fn test_chunk_bytes_limits() {
        let data = vec![0u8; 12 * 1024 * 1024]; // 12 MiB
        let chunks = chunk_bytes(&data);
        assert!(!chunks.is_empty());
        for (i, chunk) in chunks.iter().enumerate() {
            if i < chunks.len() - 1 {
                assert!(
                    chunk.len() >= 1024 * 1024,
                    "Chunk {} too small: {}",
                    i,
                    chunk.len()
                );
            }
            assert!(
                chunk.len() <= 8 * 1024 * 1024,
                "Chunk {} too large: {}",
                i,
                chunk.len()
            );
        }
    }

    #[test]
    fn test_chunk_store_lifecycle() {
        let dir = tempdir().unwrap();
        let config = Drive9Config::new(dir.path());
        let store = ChunkStore::new(config.layout());

        let data = b"hello physical storage blocks!";
        let hash = store.put_chunk(data).unwrap();
        assert!(store.has_chunk(&hash));

        let retrieved = store.get_chunk(&hash).unwrap();
        assert_eq!(retrieved, data);
    }
}
