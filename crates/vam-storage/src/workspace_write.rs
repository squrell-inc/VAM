//! Upsert one file into committed Drive9 HEAD under the guest `workspace/` mount.

use crate::cas::{chunk_bytes, ChunkStore};
use crate::config::Drive9Layout;
use crate::error::Drive9Error;
use crate::manifest::{ManifestFile, ManifestManager, MerkleManifest};
use crate::workspace_read::{normalize_workspace_jail, MAX_DOWNLOAD_BYTES};

const WRITE_DENY_PREFIX: &str = "workspace/.vam/";
const WRITE_DENY_EXACT: &str = "workspace/.vam";
const FILE_MODE: u32 = 0o644;

/// Result of committing one workspace file into HEAD.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PutWorkspaceFile {
    pub path: String,
    pub head: String,
    pub size_bytes: u64,
}

/// Jail plus upload write-deny. Refuses the directory `workspace` and every path under `workspace/.vam/`.
pub fn normalize_workspace_write_path(rel: &str) -> Result<String, Drive9Error> {
    let normalized = normalize_workspace_jail(rel)?;
    if normalized == "workspace" {
        return Err(Drive9Error::Layout(
            "workspace file path must name a file under workspace/".into(),
        ));
    }
    if is_write_denied(&normalized) {
        return Err(Drive9Error::Layout(format!(
            "path '{normalized}' is under workspace/.vam and cannot be uploaded"
        )));
    }
    Ok(normalized)
}

fn is_write_denied(normalized: &str) -> bool {
    normalized == WRITE_DENY_EXACT || normalized.starts_with(WRITE_DENY_PREFIX)
}

/// Stores `bytes` as CAS chunks and upserts `rel_path` into the workspace HEAD manifest.
///
/// Missing HEAD creates a manifest that contains only this file. An existing HEAD keeps
/// every other path and records the previous HEAD as `parent_manifest_hash`.
pub fn put_workspace_file_to_head(
    layout: &Drive9Layout,
    workspace_id: &str,
    rel_path: &str,
    bytes: &[u8],
) -> Result<PutWorkspaceFile, Drive9Error> {
    let path = normalize_workspace_write_path(rel_path)?;
    let size_bytes = bytes.len() as u64;
    if size_bytes > MAX_DOWNLOAD_BYTES {
        return Err(Drive9Error::FileTooLarge {
            size: size_bytes,
            max: MAX_DOWNLOAD_BYTES,
        });
    }

    let chunk_store = ChunkStore::new(layout.clone());
    let manifest_manager = ManifestManager::new(layout.clone(), chunk_store.clone());

    let mut chunk_hashes = Vec::new();
    for chunk in chunk_bytes(bytes) {
        chunk_hashes.push(chunk_store.put_chunk(&chunk)?);
    }

    let previous = manifest_manager.read_head(workspace_id)?;
    let mut manifest = match &previous {
        Some(head) => manifest_manager.get_manifest(workspace_id, head)?,
        None => MerkleManifest::new(workspace_id.to_string(), None),
    };
    manifest.parent_manifest_hash = previous.clone();

    let entry = ManifestFile {
        path: path.clone(),
        size_bytes,
        mode_permissions: FILE_MODE,
        chunks: chunk_hashes,
    };
    if let Some(slot) = manifest.files.iter_mut().find(|file| file.path == path) {
        *slot = entry;
    } else {
        manifest.files.push(entry);
    }

    let head = manifest_manager.commit_manifest(&manifest)?;
    manifest_manager.write_head(workspace_id, &head)?;

    Ok(PutWorkspaceFile {
        path,
        head,
        size_bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Drive9Config;
    use crate::workspace_read::read_workspace_file_from_head;
    use tempfile::tempdir;

    fn layout() -> (tempfile::TempDir, Drive9Layout) {
        let dir = tempdir().unwrap();
        let layout = Drive9Config::new(dir.path()).layout();
        (dir, layout)
    }

    #[test]
    fn write_path_rejects_jail_and_directory_root() {
        assert!(matches!(
            normalize_workspace_write_path(""),
            Err(Drive9Error::Layout(_))
        ));
        assert!(matches!(
            normalize_workspace_write_path("workspace"),
            Err(Drive9Error::Layout(_))
        ));
        assert!(matches!(
            normalize_workspace_write_path("skills/pkg/SKILL.md"),
            Err(Drive9Error::Layout(_))
        ));
        assert!(matches!(
            normalize_workspace_write_path("workspace/../skills/x"),
            Err(Drive9Error::Layout(_))
        ));
        assert!(matches!(
            normalize_workspace_write_path("/etc/passwd"),
            Err(Drive9Error::SandboxViolation { .. })
        ));
        assert!(matches!(
            normalize_workspace_write_path("../outside"),
            Err(Drive9Error::SandboxViolation { .. })
        ));
        assert_eq!(
            normalize_workspace_write_path("workspace/notes/a.md").unwrap(),
            "workspace/notes/a.md"
        );
    }

    #[test]
    fn write_path_refuses_all_of_dot_vam() {
        for path in [
            "workspace/.vam",
            "workspace/.vam/SYSTEM.md",
            "workspace/.vam/workspace.json",
            "workspace/.vam/threads/t",
            "workspace/.vam/host-artifacts/x",
            "workspace/.vam/agent/prompt.md",
        ] {
            assert!(
                matches!(
                    normalize_workspace_write_path(path),
                    Err(Drive9Error::Layout(_))
                ),
                "{path}"
            );
        }
    }

    #[test]
    fn rejects_body_over_32_mib_before_commit() {
        let (_dir, layout) = layout();
        let bytes = vec![0u8; (MAX_DOWNLOAD_BYTES as usize) + 1];
        assert!(matches!(
            put_workspace_file_to_head(&layout, "ws-big", "workspace/huge.bin", &bytes),
            Err(Drive9Error::FileTooLarge { .. })
        ));
        assert!(manifest_manager_head(&layout, "ws-big").is_none());
    }

    #[test]
    fn missing_head_creates_readable_file() {
        let (_dir, layout) = layout();
        let put = put_workspace_file_to_head(&layout, "ws-new", "workspace/notes/a.md", b"hello")
            .unwrap();
        assert_eq!(put.path, "workspace/notes/a.md");
        assert_eq!(put.size_bytes, 5);
        let bytes = read_workspace_file_from_head(
            &layout,
            "ws-new",
            "workspace/notes/a.md",
            MAX_DOWNLOAD_BYTES,
        )
        .unwrap();
        assert_eq!(bytes, b"hello");

        let manifest = load_head(&layout, "ws-new");
        assert!(manifest.parent_manifest_hash.is_none());
        assert_eq!(manifest.files.len(), 1);
        assert_eq!(manifest.files[0].mode_permissions, 0o644);
    }

    #[test]
    fn upsert_replaces_one_path_and_keeps_siblings() {
        let (_dir, layout) = layout();
        let first =
            put_workspace_file_to_head(&layout, "ws-up", "workspace/a.txt", b"alpha").unwrap();
        let second =
            put_workspace_file_to_head(&layout, "ws-up", "workspace/b.txt", b"beta").unwrap();
        assert_ne!(first.head, second.head);

        let replaced =
            put_workspace_file_to_head(&layout, "ws-up", "workspace/a.txt", b"alpha-2").unwrap();
        assert_ne!(replaced.head, second.head);

        let a =
            read_workspace_file_from_head(&layout, "ws-up", "workspace/a.txt", MAX_DOWNLOAD_BYTES)
                .unwrap();
        let b =
            read_workspace_file_from_head(&layout, "ws-up", "workspace/b.txt", MAX_DOWNLOAD_BYTES)
                .unwrap();
        assert_eq!(a, b"alpha-2");
        assert_eq!(b, b"beta");

        let manifest = load_head(&layout, "ws-up");
        assert_eq!(
            manifest.parent_manifest_hash.as_deref(),
            Some(second.head.as_str())
        );
        assert_eq!(manifest.files.len(), 2);
        assert!(manifest.files.iter().any(|f| f.path == "workspace/b.txt"));
    }

    fn load_head(layout: &Drive9Layout, workspace_id: &str) -> MerkleManifest {
        let head = manifest_manager_head(layout, workspace_id).expect("head");
        let chunk_store = ChunkStore::new(layout.clone());
        let manager = ManifestManager::new(layout.clone(), chunk_store);
        manager.get_manifest(workspace_id, &head).unwrap()
    }

    fn manifest_manager_head(layout: &Drive9Layout, workspace_id: &str) -> Option<String> {
        let chunk_store = ChunkStore::new(layout.clone());
        let manager = ManifestManager::new(layout.clone(), chunk_store);
        manager.read_head(workspace_id).unwrap()
    }
}
