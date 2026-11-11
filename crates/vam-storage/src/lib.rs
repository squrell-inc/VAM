//! Host-side immutable workspace checkpoint barriers.
//!
//! The production Drive9 backend is intentionally not implemented here. This
//! crate defines the barrier contract and a deterministic filesystem-backed
//! implementation used by orchestration tests.

pub mod backend;
pub mod cache;
pub mod cas;
pub mod config;
pub mod disk;
pub mod error;
pub mod frames;
pub mod gc;
pub mod image_read;
pub mod image_snapshot;
pub mod instruction_template;
pub mod log_quota;
pub mod manifest;
pub mod owner_prefix;
pub mod path;
pub mod prompt_store;
pub mod sparse;
pub mod thread_log;
pub mod vam_dir;
pub mod workspace;
pub mod workspace_read;
pub mod workspace_write;

pub use backend::{BackendCapabilities, LocalDirBackend, ObjectStoreBackend, ObjectVersion};
pub use cache::{CacheManager, CloneStrategy};
pub use cas::{chunk_bytes, sha256_hex, ChunkStore};
pub use config::{Drive9Config, Drive9Layout, StorageProfile};
pub use disk::{Drive9ImageIo, E2fsImageIo, MockImageIo};
pub use error::Drive9Error;
pub use frames::{Drive9Client, Drive9Server};
pub use gc::{collect_referenced_chunk_hashes, remove_workspace_tree, sweep_unreferenced_chunks};
pub use image_read::{
    valid_component, ImageEntry, ImageEntryKind, ImageLimits, ImageVisitor, ScanVisitor,
    SkippedEntry, TreeWriter, WalkReport,
};
pub use image_snapshot::{
    snapshot_image, SnapshotLimits, SnapshotMethod, SnapshotReport, COPY_DEADLINE,
    FREE_SPACE_MARGIN_BYTES, MAX_IMAGE_APPARENT_BYTES,
};
pub use instruction_template::{
    parse_template_version, seed_plan_md_stub, seed_system_instruction_template,
    SHIPPED_TEMPLATE_VERSION, SYSTEM_MD_REL_PATH,
};
pub use log_quota::{LogQuota, LogUsage, QuotaScope};
pub use manifest::{ManifestFile, ManifestManager, MerkleManifest};
pub use owner_prefix::{
    agent_workspace_key, apply_agent_workspace_id, apply_owner_prefix, guest_id_suffix,
    owner_prefix_suffix, resolve_owned_workspace_id, IMPLICIT_OWNER_USER_ID, OWNER_PREFIX_SEP,
};
pub use path::{sanitize_path, validate_manifest_path};
pub use sparse::copy_sparse;
pub use vam_dir::{
    append_turn, append_turn_full, append_turn_with_log, ensure_thread_json, ensure_workspace_json,
    hash_cloudevent_json, hash_turn_record, parse_timeline, read_timeline, read_workspace_meta,
    read_workspace_meta_from_head, replay_prompt, sanitize_thread_id, ThreadMeta,
    TrajectorySummary, TurnLogRange, TurnRecord, TurnWorkspace, WorkspaceAgentBind,
    WorkspaceAgentVersion, WorkspaceMeta, DEFAULT_REPLAY_TURNS, DEFAULT_THREAD_REF,
    THREAD_TURN_EVENT_TYPE, VAM_SPEC_VERSION, WORKSPACE_JSON_PATH,
};
pub use workspace::{
    open_workspace_manager, open_workspace_manager_with, read_workspace_lock_holder,
    release_workspace_lock, skills_digest, skills_digest_from_dir, with_skills_writable,
    Drive9Backend, DroppedEntry, DroppedSummary, EndBase, PreparedEnd, ReclaimReport,
    WorkspaceManager, ROLLED_BACK_KEEP_CAP_BYTES,
};
pub use workspace_read::{
    list_workspace_files_from_head, normalize_workspace_file_path, read_workspace_file_from_head,
    WorkspaceFileEntry, WorkspaceFilesListing, DEFAULT_MAX_LIST_FILES, MAX_DOWNLOAD_BYTES,
};
pub use workspace_write::{
    normalize_workspace_write_path, put_workspace_file_to_head, PutWorkspaceFile,
};

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use thiserror::Error;
use vam_spec::v1::{ArtifactRef, Digest, ResourceId};

#[derive(Debug, Clone, PartialEq)]
pub struct CheckpointRequest {
    pub workspace_id: ResourceId,
    pub checkpoint_name: String,
}

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("workspace path '{0}' is not a directory")]
    WorkspaceNotDirectory(PathBuf),
    #[error("checkpoint name '{0}' is invalid")]
    InvalidCheckpointName(String),
    #[error("storage I/O failed: {0}")]
    Io(#[from] io::Error),
}

/// Commits all workspace files into an immutable checkpoint before a workflow
/// transition with external consequences can proceed.
pub trait CheckpointCoordinator: Send + Sync {
    fn checkpoint(
        &self,
        workspace: &Path,
        request: &CheckpointRequest,
    ) -> Result<ArtifactRef, StorageError>;
}

#[derive(Debug, Clone)]
pub struct FilesystemCheckpointCoordinator {
    manifest_root: PathBuf,
}

impl FilesystemCheckpointCoordinator {
    pub fn new(manifest_root: impl Into<PathBuf>) -> Self {
        Self {
            manifest_root: manifest_root.into(),
        }
    }
}

impl CheckpointCoordinator for FilesystemCheckpointCoordinator {
    fn checkpoint(
        &self,
        workspace: &Path,
        request: &CheckpointRequest,
    ) -> Result<ArtifactRef, StorageError> {
        if !workspace.is_dir() {
            return Err(StorageError::WorkspaceNotDirectory(workspace.to_path_buf()));
        }
        if !is_checkpoint_name(&request.checkpoint_name) {
            return Err(StorageError::InvalidCheckpointName(
                request.checkpoint_name.clone(),
            ));
        }

        let layout = Drive9Layout::new(&self.manifest_root);
        let chunk_store = ChunkStore::new(layout.clone());
        let manifest_manager = ManifestManager::new(layout, chunk_store);

        let manifest = manifest_manager
            .build_from_dir(&request.workspace_id.urn, workspace, None)
            .map_err(|e| StorageError::Io(io::Error::other(e.to_string())))?;

        let manifest_digest = manifest
            .hash()
            .map_err(|e| StorageError::Io(io::Error::other(e.to_string())))?;

        fs::create_dir_all(&self.manifest_root)?;
        let manifest_path = self.manifest_root.join(format!(
            "{}-{}.manifest",
            request.checkpoint_name, manifest_digest
        ));

        if !manifest_path.exists() {
            let serialized = manifest
                .serialize()
                .map_err(|e| StorageError::Io(io::Error::other(e.to_string())))?;
            fs::write(&manifest_path, serialized.as_bytes())?;
        }

        Ok(ArtifactRef {
            artifact_id: Some(request.workspace_id.clone()),
            digest: Some(Digest {
                algorithm: 1,
                hex_hash: manifest_digest,
            }),
            media_type: "application/vnd.vam.workspace-manifest".to_string(),
            size_bytes: fs::metadata(&manifest_path)?.len() as i64,
            uri: manifest_path.to_string_lossy().into_owned(),
        })
    }
}

fn is_checkpoint_name(name: &str) -> bool {
    !name.is_empty()
        && name.chars().all(|character| {
            character.is_ascii_alphanumeric() || character == '-' || character == '_'
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checkpoint_is_content_addressed() {
        let root = std::env::temp_dir().join("vam-storage-checkpoint-test");
        let workspace = root.join("workspace");
        let manifests = root.join("manifests");
        fs::create_dir_all(&workspace).expect("workspace should be created");
        fs::write(workspace.join("result.txt"), "complete").expect("fixture should be written");

        let coordinator = FilesystemCheckpointCoordinator::new(&manifests);
        let request = CheckpointRequest {
            workspace_id: ResourceId {
                urn: "urn:vam:workspace:local.vam.dev/acme/demo".to_string(),
            },
            checkpoint_name: "before-approval".to_string(),
        };
        let first = coordinator
            .checkpoint(&workspace, &request)
            .expect("checkpoint should succeed");
        let second = coordinator
            .checkpoint(&workspace, &request)
            .expect("checkpoint should be idempotent");

        assert_eq!(first.digest, second.digest);
        fs::remove_dir_all(root).expect("test files should be removed");
    }
}
