//! Host store for the system prompt and tool definitions a thread's requests
//! carried, addressed by SHA-256 (the thread log keeps digests only).
//!
//! Location: `<drive9_root>/workspaces/<workspace_id>/prompts/`. That is outside
//! the workspace's materialized tree and outside its manifest, so it is never
//! versioned, never listed by a files API and never reachable by the guest. It
//! is removed with the workspace tree.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

use thiserror::Error;

use crate::cas::sha256_hex;
use crate::config::Drive9Layout;
use crate::log_quota::{LogQuota, LogUsage};

pub const PROMPTS_DIR: &str = "prompts";

#[derive(Debug, Error)]
pub enum PromptStoreError {
    #[error("prompt store I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid workspace id for the prompt store: {0}")]
    InvalidWorkspace(String),
    #[error("invalid digest: {0}")]
    InvalidDigest(String),
    #[error("stored bytes for {0} do not match their digest")]
    Tampered(String),
    #[error("the workspace log is full: {used} of {limit} bytes in use")]
    QuotaExceeded { limit: u64, used: u64 },
}

/// What a stored blob is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptKind {
    /// System prompt, raw UTF-8 bytes.
    System,
    /// Tool definitions, canonical JSON.
    Tools,
}

impl PromptKind {
    fn suffix(self) -> &'static str {
        match self {
            PromptKind::System => "system",
            PromptKind::Tools => "tools.json",
        }
    }
}

#[derive(Debug, Clone)]
pub struct PromptStore {
    dir: PathBuf,
    quota: Option<(LogQuota, LogUsage)>,
}

fn valid_digest(digest: &str) -> bool {
    digest.len() == 64
        && digest
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

impl PromptStore {
    pub fn new(layout: &Drive9Layout, workspace_id: &str) -> Result<Self, PromptStoreError> {
        if workspace_id.is_empty()
            || workspace_id == "."
            || workspace_id == ".."
            || workspace_id.contains('/')
            || workspace_id.contains('\\')
            || workspace_id.contains('\0')
        {
            return Err(PromptStoreError::InvalidWorkspace(workspace_id.to_string()));
        }
        Ok(Self {
            dir: layout.workspace_dir(workspace_id).join(PROMPTS_DIR),
            quota: None,
        })
    }

    /// Counts what this store writes against the workspace limit (thread logs
    /// plus prompts, shared through `usage`).
    pub fn with_quota(mut self, quota: LogQuota, usage: LogUsage) -> Self {
        self.quota = Some((quota, usage));
        self
    }

    /// Bytes of every stored blob.
    pub fn bytes_used(&self) -> u64 {
        let Ok(entries) = fs::read_dir(&self.dir) else {
            return 0;
        };
        entries
            .flatten()
            .filter_map(|e| e.metadata().ok())
            .filter(|m| m.is_file())
            .map(|m| m.len())
            .sum()
    }

    fn path(&self, digest: &str, kind: PromptKind) -> Result<PathBuf, PromptStoreError> {
        if !valid_digest(digest) {
            return Err(PromptStoreError::InvalidDigest(digest.to_string()));
        }
        Ok(self.dir.join(format!("{digest}.{}", kind.suffix())))
    }

    /// Stores `bytes` if absent and returns their SHA-256.
    pub fn put(&self, kind: PromptKind, bytes: &[u8]) -> Result<String, PromptStoreError> {
        let digest = sha256_hex(bytes);
        let path = self.path(&digest, kind)?;
        if path.is_file() {
            return Ok(digest);
        }
        // Prompts are conversation content: they never use the reserve.
        let charged = bytes.len() as u64;
        if let Some((quota, usage)) = &self.quota {
            usage
                .try_add(charged, quota.workspace_bytes)
                .map_err(|used| PromptStoreError::QuotaExceeded {
                    limit: quota.workspace_bytes,
                    used,
                })?;
        }
        let written = self.write_blob(&path, &digest, bytes);
        if written.is_err() {
            if let Some((_, usage)) = &self.quota {
                usage.sub(charged);
            }
        }
        written?;
        Ok(digest)
    }

    fn write_blob(
        &self,
        path: &std::path::Path,
        digest: &str,
        bytes: &[u8],
    ) -> Result<(), PromptStoreError> {
        fs::create_dir_all(&self.dir)?;
        let tmp = self
            .dir
            .join(format!(".tmp-{digest}-{}", std::process::id()));
        {
            let mut file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&tmp)?;
            file.write_all(bytes)?;
            file.sync_all()?;
        }
        fs::rename(&tmp, path)?;
        Ok(())
    }

    /// The bytes for `digest`, re-hashed before they are returned.
    pub fn get(&self, kind: PromptKind, digest: &str) -> Result<Option<Vec<u8>>, PromptStoreError> {
        let path = self.path(digest, kind)?;
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        if sha256_hex(&bytes) != digest {
            return Err(PromptStoreError::Tampered(digest.to_string()));
        }
        Ok(Some(bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn put_get_dedupes_and_lives_outside_the_workspace_tree() {
        let dir = tempdir().unwrap();
        let layout = Drive9Layout::new(dir.path());
        let store = PromptStore::new(&layout, "ws-1").unwrap();
        let a = store.put(PromptKind::System, b"you are helpful").unwrap();
        let b = store.put(PromptKind::System, b"you are helpful").unwrap();
        assert_eq!(a, b);
        assert_eq!(
            store.get(PromptKind::System, &a).unwrap().unwrap(),
            b"you are helpful"
        );
        let tools = store.put(PromptKind::Tools, b"[]").unwrap();
        assert_eq!(
            store.get(PromptKind::Tools, &tools).unwrap().unwrap(),
            b"[]"
        );
        // Same digest, other kind: a different file.
        assert!(store.get(PromptKind::Tools, &a).unwrap().is_none());
        let files: Vec<_> = fs::read_dir(layout.workspace_dir("ws-1").join(PROMPTS_DIR))
            .unwrap()
            .collect();
        assert_eq!(files.len(), 2);
        assert!(layout
            .workspace_dir("ws-1")
            .join(PROMPTS_DIR)
            .starts_with(layout.workspaces_dir()));
    }

    #[test]
    fn a_tampered_file_is_refused() {
        let dir = tempdir().unwrap();
        let layout = Drive9Layout::new(dir.path());
        let store = PromptStore::new(&layout, "ws-1").unwrap();
        let digest = store.put(PromptKind::System, b"original").unwrap();
        let path = layout
            .workspace_dir("ws-1")
            .join(PROMPTS_DIR)
            .join(format!("{digest}.system"));
        fs::write(path, b"edited").unwrap();
        assert!(matches!(
            store.get(PromptKind::System, &digest),
            Err(PromptStoreError::Tampered(_))
        ));
    }

    #[test]
    fn prompts_count_against_the_workspace_limit_and_a_known_digest_costs_nothing() {
        let dir = tempdir().unwrap();
        let layout = Drive9Layout::new(dir.path());
        let quota = LogQuota {
            thread_bytes: 1 << 30,
            workspace_bytes: 100,
            reserve_bytes: 1000,
        };
        let usage = LogUsage::new(|| 0);
        let store = PromptStore::new(&layout, "ws-1")
            .unwrap()
            .with_quota(quota, usage.clone());
        let first = store.put(PromptKind::System, &[b'a'; 60]).unwrap();
        // The same bytes again cost nothing.
        assert_eq!(store.put(PromptKind::System, &[b'a'; 60]).unwrap(), first);
        assert_eq!(usage.used(), 60);
        // New bytes that do not fit are refused; the reserve is not for prompts.
        let err = store.put(PromptKind::Tools, &[b'b'; 60]).unwrap_err();
        assert!(matches!(
            err,
            PromptStoreError::QuotaExceeded {
                limit: 100,
                used: 60
            }
        ));
        assert_eq!(usage.used(), 60);
        assert_eq!(store.bytes_used(), 60);
        assert!(store
            .get(PromptKind::Tools, &sha256_hex(&[b'b'; 60]))
            .unwrap()
            .is_none());
    }

    #[test]
    fn ids_and_digests_are_validated() {
        let dir = tempdir().unwrap();
        let layout = Drive9Layout::new(dir.path());
        for bad in ["", ".", "..", "a/b", "a\\b", "a\0b"] {
            assert!(PromptStore::new(&layout, bad).is_err(), "{bad:?}");
        }
        let store = PromptStore::new(&layout, "ws-1").unwrap();
        for bad in [
            "../x",
            "abc",
            &"G".repeat(64),
            &format!("{}/", "a".repeat(63)),
        ] {
            assert!(store.get(PromptKind::System, bad).is_err(), "{bad:?}");
        }
    }
}
