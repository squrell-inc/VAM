use crate::cas::{chunk_bytes, sha256_hex, ChunkStore};
use crate::config::Drive9Layout;
use crate::disk::Drive9ImageIo;
use crate::error::Drive9Error;
use crate::image_read::{
    content_io_error, ImageEntry, ImageEntryKind, ImageLimits, ImageVisitor, WalkReport,
};
use crate::path::validate_manifest_path;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Read;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManifestFile {
    pub path: String,
    pub size_bytes: u64,
    pub mode_permissions: u32,
    pub chunks: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MerkleManifest {
    pub manifest_version: u32,
    pub workspace_id: String,
    pub parent_manifest_hash: Option<String>,
    pub created_at: String,
    pub files: Vec<ManifestFile>,
}

impl MerkleManifest {
    pub fn new(workspace_id: String, parent_manifest_hash: Option<String>) -> Self {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs().to_string())
            .unwrap_or_default();
        Self {
            manifest_version: 1,
            workspace_id,
            parent_manifest_hash,
            created_at: now,
            files: Vec::new(),
        }
    }

    pub fn serialize(&self) -> Result<String, Drive9Error> {
        serde_json::to_string_pretty(self).map_err(|e| Drive9Error::Serialization(e.to_string()))
    }

    pub fn deserialize(json: &str) -> Result<Self, Drive9Error> {
        serde_json::from_str(json).map_err(|e| Drive9Error::Serialization(e.to_string()))
    }

    pub fn hash(&self) -> Result<String, Drive9Error> {
        let serialized = self.serialize()?;
        Ok(sha256_hex(serialized.as_bytes()))
    }

    /// Hash of the workspace's files alone: path, size, mode and chunks of
    /// every entry outside the host `.vam/`, sorted by path. Unlike [`hash`]
    /// it does not cover the conversation, so it can be written into the
    /// conversation (the timeline, the thread log) without referring to
    /// itself, and two HEADs with the same files share it.
    ///
    /// [`hash`]: Self::hash
    pub fn files_hash(&self) -> String {
        let mut entries: Vec<(&str, u64, u32, &[String])> = self
            .files
            .iter()
            .filter(|f| f.path != ".vam" && !f.path.starts_with(".vam/"))
            .map(|f| {
                (
                    f.path.as_str(),
                    f.size_bytes,
                    f.mode_permissions,
                    f.chunks.as_slice(),
                )
            })
            .collect();
        entries.sort_by(|a, b| a.0.cmp(b.0));
        let canonical = serde_json::to_string(&entries).unwrap_or_default();
        sha256_hex(canonical.as_bytes())
    }

    /// [`files_hash`](Self::files_hash) of a workspace with no files.
    pub fn empty_files_hash() -> String {
        Self::new(String::new(), None).files_hash()
    }
}

pub struct ManifestManager {
    layout: Drive9Layout,
    chunk_store: ChunkStore,
}

impl ManifestManager {
    pub fn new(layout: Drive9Layout, chunk_store: ChunkStore) -> Self {
        Self {
            layout,
            chunk_store,
        }
    }

    /// Builds a manifest from a directory of files.
    pub fn build_from_dir(
        &self,
        workspace_id: &str,
        dir_path: &Path,
        parent_hash: Option<String>,
    ) -> Result<MerkleManifest, Drive9Error> {
        self.build_from_dir_excluding(workspace_id, dir_path, parent_hash, &[])
    }

    /// Like [`Self::build_from_dir`], leaving out the entries directly under
    /// `dir_path` whose name is in `skip_top` (a turn commit leaves out the
    /// host's `.vam/`, which a commit composes in separately).
    pub fn build_from_dir_excluding(
        &self,
        workspace_id: &str,
        dir_path: &Path,
        parent_hash: Option<String>,
        skip_top: &[&str],
    ) -> Result<MerkleManifest, Drive9Error> {
        if !dir_path.is_dir() {
            return Err(Drive9Error::WorkspaceNotDirectory(dir_path.to_path_buf()));
        }

        let mut manifest = MerkleManifest::new(workspace_id.to_string(), parent_hash);
        let mut files = Vec::new();
        self.collect_files_recursive(dir_path, dir_path, skip_top, &mut files)?;
        files.sort_by(|a, b| a.path.cmp(&b.path));
        manifest.files = files;

        Ok(manifest)
    }

    /// Builds a manifest of the regular files in a guest-written ext4 image,
    /// reading it through the safe reader: the top-level host `.vam/` is never
    /// read, symlinks and devices are not held (the report lists them), and no
    /// host path is built from an image name. Content goes straight to the chunk
    /// store.
    pub fn build_from_image(
        &self,
        workspace_id: &str,
        image_io: &dyn Drive9ImageIo,
        image: &Path,
        parent_hash: Option<String>,
        limits: &ImageLimits,
    ) -> Result<(MerkleManifest, WalkReport), Drive9Error> {
        struct Collect<'a> {
            store: &'a ChunkStore,
            files: Vec<ManifestFile>,
        }
        impl ImageVisitor for Collect<'_> {
            fn wants_content(&mut self, entry: &ImageEntry) -> bool {
                entry.kind == ImageEntryKind::File
            }
            fn visit(
                &mut self,
                entry: &ImageEntry,
                content: Option<&mut dyn Read>,
            ) -> Result<(), Drive9Error> {
                let (ImageEntryKind::File, Some(content)) = (entry.kind, content) else {
                    return Ok(());
                };
                validate_manifest_path(&entry.path)?;
                let mut bytes = Vec::with_capacity(entry.size.min(64 * 1024 * 1024) as usize);
                content.read_to_end(&mut bytes).map_err(content_io_error)?;
                if bytes.len() as u64 != entry.size {
                    return Err(Drive9Error::ImageCorrupt(
                        "a file in the image is shorter than its size".to_string(),
                    ));
                }
                let mut chunks = Vec::new();
                for payload in chunk_bytes(&bytes) {
                    chunks.push(self.store.put_chunk(&payload)?);
                }
                self.files.push(ManifestFile {
                    path: entry.path.clone(),
                    size_bytes: entry.size,
                    mode_permissions: 0o100000 | entry.mode,
                    chunks,
                });
                Ok(())
            }
        }
        let mut collect = Collect {
            store: &self.chunk_store,
            files: Vec::new(),
        };
        let report = image_io.walk_image(image, limits, &mut collect)?;
        let mut manifest = MerkleManifest::new(workspace_id.to_string(), parent_hash);
        let mut files = collect.files;
        files.sort_by(|a, b| a.path.cmp(&b.path));
        manifest.files = files;
        Ok((manifest, report))
    }

    /// A manifest of `base`'s entries outside `host_dir`, plus the files now
    /// under `<tree>/<host_dir>`. This is how a stop keeps the conversation
    /// (host `.vam/`) while the files stay as of an earlier turn commit,
    /// without reading the rest of the tree. Symlinks are not followed and a
    /// `host_dir` that is not a plain directory contributes nothing.
    pub fn compose_with_host_dir(
        &self,
        workspace_id: &str,
        base: Option<&MerkleManifest>,
        tree: &Path,
        host_dir: &str,
        parent_hash: Option<String>,
    ) -> Result<MerkleManifest, Drive9Error> {
        let prefix = format!("{host_dir}/");
        let mut manifest = MerkleManifest::new(workspace_id.to_string(), parent_hash);
        let mut files: Vec<ManifestFile> = base
            .map(|b| {
                b.files
                    .iter()
                    .filter(|f| !f.path.starts_with(&prefix) && f.path != host_dir)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        let host_path = tree.join(host_dir);
        let plain = fs::symlink_metadata(&host_path)
            .map(|m| m.file_type().is_dir())
            .unwrap_or(false);
        if plain {
            self.collect_files_recursive(tree, &host_path, &[], &mut files)?;
        }
        files.sort_by(|a, b| a.path.cmp(&b.path));
        manifest.files = files;
        Ok(manifest)
    }

    fn collect_files_recursive(
        &self,
        root: &Path,
        current: &Path,
        skip_top: &[&str],
        files: &mut Vec<ManifestFile>,
    ) -> Result<(), Drive9Error> {
        for entry in fs::read_dir(current)? {
            let entry = entry?;
            let path = entry.path();
            if current == root && skip_top.iter().any(|name| entry.file_name() == **name) {
                continue;
            }
            let metadata = entry.metadata()?;

            if metadata.is_dir() {
                self.collect_files_recursive(root, &path, skip_top, files)?;
            } else if metadata.is_file() {
                let relative = path.strip_prefix(root).map_err(std::io::Error::other)?;
                let rel_str = relative.to_string_lossy().replace('\\', "/");

                let file_bytes = fs::read(&path)?;
                let chunk_payloads = chunk_bytes(&file_bytes);
                let mut chunk_hashes = Vec::new();
                for payload in chunk_payloads {
                    let hash = self.chunk_store.put_chunk(&payload)?;
                    chunk_hashes.push(hash);
                }

                #[cfg(unix)]
                let mode = {
                    use std::os::unix::fs::MetadataExt;
                    metadata.mode()
                };
                #[cfg(not(unix))]
                let mode = 0o644;

                files.push(ManifestFile {
                    path: rel_str,
                    size_bytes: metadata.len(),
                    mode_permissions: mode,
                    chunks: chunk_hashes,
                });
            }
        }
        Ok(())
    }

    /// Commits the given manifest. Writes to physical manifests directory and returns its hash.
    pub fn commit_manifest(&self, manifest: &MerkleManifest) -> Result<String, Drive9Error> {
        for file in &manifest.files {
            validate_manifest_path(&file.path)?;
        }
        let hash = manifest.hash()?;
        let path = self.layout.manifest_path(&manifest.workspace_id, &hash);

        if path.exists() {
            return Ok(hash);
        }

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let serialized = manifest.serialize()?;
        let parent_dir = path.parent().unwrap();
        let temp_path = parent_dir.join(format!(".tmp-manifest-{}", hash));
        fs::write(&temp_path, serialized.as_bytes())?;

        if let Err(e) = fs::rename(&temp_path, &path) {
            let _ = fs::remove_file(&temp_path);
            return Err(Drive9Error::Io(e));
        }

        Ok(hash)
    }

    /// Reads a manifest from storage.
    pub fn get_manifest(
        &self,
        workspace_id: &str,
        hash: &str,
    ) -> Result<MerkleManifest, Drive9Error> {
        let path = self.layout.manifest_path(workspace_id, hash);
        if !path.exists() {
            return Err(Drive9Error::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("Manifest {} not found", hash),
            )));
        }
        let content = fs::read_to_string(path)?;
        MerkleManifest::deserialize(&content)
    }

    /// Reads the active manifest hash from HEAD file.
    pub fn read_head(&self, workspace_id: &str) -> Result<Option<String>, Drive9Error> {
        let head_path = self.layout.head_path(workspace_id);
        if !head_path.exists() {
            return Ok(None);
        }
        let hash = fs::read_to_string(head_path)?.trim().to_string();
        if hash.is_empty() {
            Ok(None)
        } else {
            Ok(Some(hash))
        }
    }

    /// Writes/commits the active manifest hash to HEAD file (compare-and-swap style logic on caller if needed).
    pub fn write_head(&self, workspace_id: &str, hash: &str) -> Result<(), Drive9Error> {
        let head_path = self.layout.head_path(workspace_id);
        if let Some(parent) = head_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let temp_path = head_path.with_extension("tmp");
        fs::write(&temp_path, hash.as_bytes())?;
        fs::rename(&temp_path, &head_path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Drive9Config;
    use tempfile::tempdir;

    #[test]
    fn test_merkle_manifest_determinism() {
        let manifest1 = MerkleManifest::new("ws-1".to_string(), None);
        let s1 = manifest1.serialize().unwrap();
        let d1 = MerkleManifest::deserialize(&s1).unwrap();
        assert_eq!(d1.workspace_id, "ws-1");
    }

    #[test]
    fn test_manifest_manager_lifecycle() {
        let dir = tempdir().unwrap();
        let config = Drive9Config::new(dir.path());
        let layout = config.layout();
        let chunk_store = ChunkStore::new(layout.clone());
        let manager = ManifestManager::new(layout, chunk_store);

        let workspace_dir = dir.path().join("my-ws");
        fs::create_dir_all(&workspace_dir).unwrap();
        fs::write(
            workspace_dir.join("a.txt"),
            b"some long file text content that chunker will process",
        )
        .unwrap();

        let manifest = manager
            .build_from_dir("my-ws", &workspace_dir, None)
            .unwrap();
        assert_eq!(manifest.files.len(), 1);
        assert_eq!(manifest.files[0].path, "a.txt");

        let hash = manager.commit_manifest(&manifest).unwrap();
        let retrieved = manager.get_manifest("my-ws", &hash).unwrap();
        assert_eq!(retrieved, manifest);

        manager.write_head("my-ws", &hash).unwrap();
        let head = manager.read_head("my-ws").unwrap().unwrap();
        assert_eq!(head, hash);
    }
}
