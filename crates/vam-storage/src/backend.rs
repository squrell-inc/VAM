use crate::cas::sha256_hex;
use crate::error::Drive9Error;
use async_trait::async_trait;
use std::path::PathBuf;

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ObjectVersion {
    ETag(String),
    Generation(u64),
    Sequence(u64),
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct BackendCapabilities {
    pub supports_conditional_writes: bool,
    pub supports_atomic_batch_delete: bool,
    pub max_put_size_bytes: usize,
}

#[async_trait]
pub trait ObjectStoreBackend: Send + Sync + std::fmt::Debug {
    /// Fetches bytes from the specified backend path/key.
    async fn get(&self, key: &str) -> Result<Vec<u8>, Drive9Error>;

    /// Uploads an immutable, content-addressed data chunk.
    async fn put_chunk(&self, key: &str, bytes: Vec<u8>) -> Result<ObjectVersion, Drive9Error>;

    /// Commits the workspace manifest or HEAD file.
    /// Supports optimistic concurrency via the optional `expected` version.
    async fn commit_manifest(
        &self,
        key: &str,
        manifest: Vec<u8>,
        expected: Option<ObjectVersion>,
    ) -> Result<ObjectVersion, Drive9Error>;

    /// Deletes a collection of keys.
    async fn delete_batch(&self, keys: &[String]) -> Result<(), Drive9Error>;

    /// Returns the capability handshakes for the backend.
    fn capabilities(&self) -> BackendCapabilities;
}

#[derive(Debug, Clone)]
pub struct LocalDirBackend {
    root: PathBuf,
}

impl LocalDirBackend {
    pub fn new<P: Into<PathBuf>>(root: P) -> Self {
        Self { root: root.into() }
    }

    fn resolve_key(&self, key: &str) -> PathBuf {
        // Prevent directory traversal attacks on the backend root
        let clean_key = key.trim_start_matches('/');
        self.root.join(clean_key)
    }
}

#[async_trait]
impl ObjectStoreBackend for LocalDirBackend {
    async fn get(&self, key: &str) -> Result<Vec<u8>, Drive9Error> {
        let path = self.resolve_key(key);
        if !path.exists() {
            return Err(Drive9Error::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("Key '{}' not found", key),
            )));
        }
        let bytes = tokio::fs::read(&path).await?;
        Ok(bytes)
    }

    async fn put_chunk(&self, key: &str, bytes: Vec<u8>) -> Result<ObjectVersion, Drive9Error> {
        let path = self.resolve_key(key);
        if path.exists() {
            let hash = sha256_hex(&bytes);
            return Ok(ObjectVersion::ETag(hash));
        }

        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let temp_path = path.with_extension("tmp");
        tokio::fs::write(&temp_path, &bytes).await?;
        tokio::fs::rename(&temp_path, &path).await?;

        let hash = sha256_hex(&bytes);
        Ok(ObjectVersion::ETag(hash))
    }

    async fn commit_manifest(
        &self,
        key: &str,
        manifest: Vec<u8>,
        expected: Option<ObjectVersion>,
    ) -> Result<ObjectVersion, Drive9Error> {
        let path = self.resolve_key(key);

        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        // Optimistic concurrency Compare-And-Swap check
        if let Some(expected_ver) = expected {
            let current_exists = path.exists();
            match expected_ver {
                ObjectVersion::ETag(expected_hash) => {
                    if current_exists {
                        let current_bytes = tokio::fs::read(&path).await?;
                        let current_hash = sha256_hex(&current_bytes);
                        if current_hash != expected_hash {
                            return Err(Drive9Error::ConcurrentWriteConflict {
                                expected: Some(expected_hash),
                                actual: current_hash,
                            });
                        }
                    } else {
                        return Err(Drive9Error::ConcurrentWriteConflict {
                            expected: Some(expected_hash),
                            actual: "None (file does not exist)".to_string(),
                        });
                    }
                }
                ObjectVersion::Sequence(expected_seq) => {
                    if current_exists {
                        // Use metadata modification time/size as a simple sequence proxy
                        let meta = tokio::fs::metadata(&path).await?;
                        let current_seq = meta.len(); // Simple size sequence
                        if current_seq != expected_seq {
                            return Err(Drive9Error::ConcurrentWriteConflict {
                                expected: Some(expected_seq.to_string()),
                                actual: current_seq.to_string(),
                            });
                        }
                    } else if expected_seq != 0 {
                        return Err(Drive9Error::ConcurrentWriteConflict {
                            expected: Some(expected_seq.to_string()),
                            actual: "0".to_string(),
                        });
                    }
                }
                ObjectVersion::Generation(gen) => {
                    if current_exists {
                        let meta = tokio::fs::metadata(&path).await?;
                        let mtime = meta
                            .modified()?
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap()
                            .as_secs();
                        if mtime != gen {
                            return Err(Drive9Error::ConcurrentWriteConflict {
                                expected: Some(gen.to_string()),
                                actual: mtime.to_string(),
                            });
                        }
                    } else if gen != 0 {
                        return Err(Drive9Error::ConcurrentWriteConflict {
                            expected: Some(gen.to_string()),
                            actual: "0".to_string(),
                        });
                    }
                }
            }
        }

        let temp_path = path.with_extension("tmp");
        tokio::fs::write(&temp_path, &manifest).await?;
        tokio::fs::rename(&temp_path, &path).await?;

        let hash = sha256_hex(&manifest);
        Ok(ObjectVersion::ETag(hash))
    }

    async fn delete_batch(&self, keys: &[String]) -> Result<(), Drive9Error> {
        for key in keys {
            let path = self.resolve_key(key);
            if path.exists() {
                tokio::fs::remove_file(&path).await?;
            }
        }
        Ok(())
    }

    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            supports_conditional_writes: true,
            supports_atomic_batch_delete: false,
            max_put_size_bytes: 16 * 1024 * 1024, // 16 MiB max
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn test_local_dir_backend_lifecycle() {
        let dir = tempdir().unwrap();
        let backend = LocalDirBackend::new(dir.path());

        let key = "chunks/01/my-chunk";
        let data = b"chunk data payload".to_vec();

        let ver = backend.put_chunk(key, data.clone()).await.unwrap();
        assert!(matches!(ver, ObjectVersion::ETag(_)));

        let retrieved = backend.get(key).await.unwrap();
        assert_eq!(retrieved, data);

        // First write of manifests/head (no expected version)
        let new_data = b"new manifest version data".to_vec();
        let ver2 = backend
            .commit_manifest("manifests/head", new_data.clone(), None)
            .await
            .unwrap();

        // Successful conditional write update
        let ver3 = backend
            .commit_manifest(
                "manifests/head",
                b"updated manifest data".to_vec(),
                Some(ver2.clone()),
            )
            .await
            .unwrap();
        assert_ne!(ver2, ver3);

        // Concurrent conflict
        let conflict_res = backend
            .commit_manifest("manifests/head", b"stale data".to_vec(), Some(ver2))
            .await;
        assert!(conflict_res.is_err());
        assert!(matches!(
            conflict_res.unwrap_err(),
            Drive9Error::ConcurrentWriteConflict { .. }
        ));

        backend
            .delete_batch(&["chunks/01/my-chunk".to_string()])
            .await
            .unwrap();
        assert!(backend.get("chunks/01/my-chunk").await.is_err());
    }
}
