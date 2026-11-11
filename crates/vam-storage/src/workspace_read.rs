//! Read committed Drive9 HEAD entries under the guest `workspace/` mount only.

use crate::cas::ChunkStore;
use crate::config::Drive9Layout;
use crate::error::Drive9Error;
use crate::manifest::{ManifestFile, ManifestManager};
use crate::path::sanitize_path;
use std::path::Path;

/// Maximum bytes returned by a single workspace file download.
pub const MAX_DOWNLOAD_BYTES: u64 = 32 * 1024 * 1024;

/// Default cap on entries returned by workspace file listing.
pub const DEFAULT_MAX_LIST_FILES: usize = 10_000;

const MANIFEST_PATH_ROOT: &str = "__manifest_root__";

const DENY_THREADS_PREFIX: &str = "workspace/.vam/threads/";
const DENY_SYSTEM_MD: &str = "workspace/.vam/SYSTEM.md";
const DENY_WORKSPACE_JSON: &str = "workspace/.vam/workspace.json";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceFileEntry {
    pub path: String,
    pub size_bytes: u64,
    pub mode: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceFilesListing {
    pub head: String,
    pub files: Vec<WorkspaceFileEntry>,
    pub truncated: bool,
}

/// Component-normalizes a manifest-relative path and enforces the `workspace/` jail.
/// Does not apply the read deny-list or the upload write deny.
pub(crate) fn normalize_workspace_jail(rel: &str) -> Result<String, Drive9Error> {
    let trimmed = rel.trim();
    if trimmed.is_empty() {
        return Err(Drive9Error::Layout(
            "workspace file path must not be empty".into(),
        ));
    }

    let normalized = sanitize_path(Path::new(MANIFEST_PATH_ROOT), Path::new(trimmed))?;
    let normalized_str = normalized
        .to_str()
        .ok_or_else(|| Drive9Error::Layout("workspace file path is not valid UTF-8".into()))?
        .strip_prefix(MANIFEST_PATH_ROOT)
        .ok_or_else(|| Drive9Error::Layout("failed to normalize workspace file path".into()))?
        .trim_start_matches('/')
        .replace('\\', "/");

    if normalized_str.is_empty() {
        return Err(Drive9Error::Layout(
            "workspace file path must not be empty".into(),
        ));
    }

    if normalized_str != "workspace" && !normalized_str.starts_with("workspace/") {
        return Err(Drive9Error::Layout(format!(
            "path '{}' is outside workspace jail",
            normalized_str
        )));
    }

    Ok(normalized_str)
}

/// Component-normalizes a manifest-relative path and enforces the workspace jail + deny-list.
pub fn normalize_workspace_file_path(rel: &str) -> Result<String, Drive9Error> {
    let normalized_str = normalize_workspace_jail(rel)?;
    if is_deny_listed(&normalized_str) {
        return Err(Drive9Error::NotFound(normalized_str));
    }
    Ok(normalized_str)
}

fn is_deny_listed(normalized: &str) -> bool {
    normalized.starts_with(DENY_THREADS_PREFIX)
        || normalized == DENY_SYSTEM_MD
        || normalized == DENY_WORKSPACE_JSON
}

fn is_listable_manifest_path(manifest_path: &str) -> bool {
    normalize_workspace_file_path(manifest_path).is_ok()
}

/// Lists files from the workspace HEAD manifest, filtered to allowed `workspace/` paths.
pub fn list_workspace_files_from_head(
    layout: &Drive9Layout,
    workspace_id: &str,
    max_files: usize,
) -> Result<WorkspaceFilesListing, Drive9Error> {
    let chunk_store = ChunkStore::new(layout.clone());
    let manifest_manager = ManifestManager::new(layout.clone(), chunk_store);

    let Some(head) = manifest_manager.read_head(workspace_id)? else {
        return Err(Drive9Error::NotFound(format!(
            "workspace '{}' has no committed HEAD",
            workspace_id
        )));
    };

    let manifest = manifest_manager.get_manifest(workspace_id, &head)?;
    let mut files: Vec<WorkspaceFileEntry> = manifest
        .files
        .iter()
        .filter(|f| is_listable_manifest_path(&f.path))
        .map(manifest_file_to_entry)
        .collect();
    files.sort_by(|a, b| a.path.cmp(&b.path));

    let truncated = files.len() > max_files;
    if truncated {
        files.truncate(max_files);
    }

    Ok(WorkspaceFilesListing {
        head,
        files,
        truncated,
    })
}

fn manifest_file_to_entry(file: &ManifestFile) -> WorkspaceFileEntry {
    WorkspaceFileEntry {
        path: file.path.clone(),
        size_bytes: file.size_bytes,
        mode: file.mode_permissions,
    }
}

/// Reads one file from HEAD by manifest-relative path.
pub fn read_workspace_file_from_head(
    layout: &Drive9Layout,
    workspace_id: &str,
    rel_path: &str,
    max_bytes: u64,
) -> Result<Vec<u8>, Drive9Error> {
    let normalized = normalize_workspace_file_path(rel_path)?;

    let chunk_store = ChunkStore::new(layout.clone());
    let manifest_manager = ManifestManager::new(layout.clone(), chunk_store.clone());

    let Some(head) = manifest_manager.read_head(workspace_id)? else {
        return Err(Drive9Error::NotFound(format!(
            "workspace '{}' has no committed HEAD",
            workspace_id
        )));
    };

    let manifest = manifest_manager.get_manifest(workspace_id, &head)?;
    let Some(file) = manifest.files.iter().find(|f| f.path == normalized) else {
        return Err(Drive9Error::NotFound(normalized));
    };

    if file.size_bytes > max_bytes {
        return Err(Drive9Error::FileTooLarge {
            size: file.size_bytes,
            max: max_bytes,
        });
    }

    let mut data = Vec::with_capacity(file.size_bytes as usize);
    for chunk_hash in &file.chunks {
        data.extend_from_slice(&chunk_store.get_chunk(chunk_hash)?);
    }
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Drive9Config;
    use crate::manifest::{ManifestManager, MerkleManifest};
    use tempfile::tempdir;

    fn commit_manifest_files(
        layout: &Drive9Layout,
        workspace_id: &str,
        files: Vec<ManifestFile>,
    ) -> String {
        let chunk_store = ChunkStore::new(layout.clone());
        let manager = ManifestManager::new(layout.clone(), chunk_store);
        let mut manifest = MerkleManifest::new(workspace_id.to_string(), None);
        manifest.files = files;
        let hash = manager.commit_manifest(&manifest).unwrap();
        manager.write_head(workspace_id, &hash).unwrap();
        hash
    }

    #[test]
    fn list_excludes_skills_paths() {
        let dir = tempdir().unwrap();
        let layout = Drive9Config::new(dir.path()).layout();
        commit_manifest_files(
            &layout,
            "ws-1",
            vec![
                ManifestFile {
                    path: "skills/pkg/SKILL.md".into(),
                    size_bytes: 1,
                    mode_permissions: 0o644,
                    chunks: vec![],
                },
                ManifestFile {
                    path: "workspace/out.txt".into(),
                    size_bytes: 2,
                    mode_permissions: 0o644,
                    chunks: vec![],
                },
            ],
        );

        let listing =
            list_workspace_files_from_head(&layout, "ws-1", DEFAULT_MAX_LIST_FILES).unwrap();
        assert_eq!(listing.files.len(), 1);
        assert_eq!(listing.files[0].path, "workspace/out.txt");
    }

    #[test]
    fn normalize_rejects_path_escape() {
        assert!(normalize_workspace_file_path("workspace/../skills/x").is_err());
    }

    #[test]
    fn deny_listed_paths_rejected_for_read() {
        assert!(matches!(
            normalize_workspace_file_path("workspace/.vam/SYSTEM.md"),
            Err(Drive9Error::NotFound(_))
        ));
        assert!(matches!(
            normalize_workspace_file_path("workspace/.vam/workspace.json"),
            Err(Drive9Error::NotFound(_))
        ));
        assert!(matches!(
            normalize_workspace_file_path("workspace/.vam/threads/t1/events.jsonl"),
            Err(Drive9Error::NotFound(_))
        ));
    }

    #[test]
    fn deny_listed_omitted_from_list() {
        let dir = tempdir().unwrap();
        let layout = Drive9Config::new(dir.path()).layout();
        commit_manifest_files(
            &layout,
            "ws-deny",
            vec![
                ManifestFile {
                    path: "workspace/.vam/SYSTEM.md".into(),
                    size_bytes: 1,
                    mode_permissions: 0o644,
                    chunks: vec![],
                },
                ManifestFile {
                    path: "workspace/ok.md".into(),
                    size_bytes: 1,
                    mode_permissions: 0o644,
                    chunks: vec![],
                },
            ],
        );
        let listing =
            list_workspace_files_from_head(&layout, "ws-deny", DEFAULT_MAX_LIST_FILES).unwrap();
        assert_eq!(listing.files.len(), 1);
        assert_eq!(listing.files[0].path, "workspace/ok.md");
    }

    #[test]
    fn missing_head_is_not_found() {
        let dir = tempdir().unwrap();
        let layout = Drive9Config::new(dir.path()).layout();
        assert!(matches!(
            list_workspace_files_from_head(&layout, "no-head", DEFAULT_MAX_LIST_FILES),
            Err(Drive9Error::NotFound(_))
        ));
    }

    #[test]
    fn oversize_before_chunk_read() {
        let dir = tempdir().unwrap();
        let layout = Drive9Config::new(dir.path()).layout();
        commit_manifest_files(
            &layout,
            "ws-big",
            vec![ManifestFile {
                path: "workspace/huge.bin".into(),
                size_bytes: MAX_DOWNLOAD_BYTES + 1,
                mode_permissions: 0o644,
                chunks: vec![],
            }],
        );
        assert!(matches!(
            read_workspace_file_from_head(
                &layout,
                "ws-big",
                "workspace/huge.bin",
                MAX_DOWNLOAD_BYTES
            ),
            Err(Drive9Error::FileTooLarge { .. })
        ));
    }

    #[test]
    fn list_truncates_when_over_max() {
        let dir = tempdir().unwrap();
        let layout = Drive9Config::new(dir.path()).layout();
        commit_manifest_files(
            &layout,
            "ws-trunc",
            vec![
                ManifestFile {
                    path: "workspace/a".into(),
                    size_bytes: 0,
                    mode_permissions: 0o644,
                    chunks: vec![],
                },
                ManifestFile {
                    path: "workspace/b".into(),
                    size_bytes: 0,
                    mode_permissions: 0o644,
                    chunks: vec![],
                },
                ManifestFile {
                    path: "workspace/c".into(),
                    size_bytes: 0,
                    mode_permissions: 0o644,
                    chunks: vec![],
                },
            ],
        );
        let listing = list_workspace_files_from_head(&layout, "ws-trunc", 2).unwrap();
        assert!(listing.truncated);
        assert_eq!(listing.files.len(), 2);
    }
}
