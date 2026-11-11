//! Reference-aware chunk garbage collection for the global CAS pool.
//!
//! Chunk bytes live under a single `/chunks/` tree. Workspace isolation uses
//! host-prefixed workspace directories, but refcounts span every manifest on
//! disk — GC must not drop a chunk another user still references.

use crate::config::Drive9Layout;
use crate::error::Drive9Error;
use crate::manifest::MerkleManifest;
use std::collections::HashSet;
use std::fs;

/// Gathers SHA-256 digests referenced by every committed manifest in every workspace.
///
/// Walks all entries under `{root}/workspaces/` (including host-prefixed ids such as
/// `alice--assistant`). There is no per-user GC pool.
pub fn collect_referenced_chunk_hashes(
    layout: &Drive9Layout,
) -> Result<HashSet<String>, Drive9Error> {
    let mut live = HashSet::new();
    let workspaces_dir = layout.workspaces_dir();
    if !workspaces_dir.is_dir() {
        return Ok(live);
    }

    for workspace_entry in fs::read_dir(&workspaces_dir)? {
        let workspace_entry = workspace_entry?;
        if !workspace_entry.file_type()?.is_dir() {
            continue;
        }
        let workspace_id = workspace_entry.file_name();
        let workspace_id = workspace_id.to_string_lossy();
        ingest_manifests_for_workspace(layout, &workspace_id, &mut live)?;
    }

    Ok(live)
}

fn ingest_manifests_for_workspace(
    layout: &Drive9Layout,
    workspace_id: &str,
    live: &mut HashSet<String>,
) -> Result<(), Drive9Error> {
    let manifests_dir = layout.manifests_dir(workspace_id);
    if !manifests_dir.is_dir() {
        return Ok(());
    }

    for entry in fs::read_dir(&manifests_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') {
            continue;
        }
        let content = fs::read_to_string(entry.path())?;
        let manifest = MerkleManifest::deserialize(&content)?;
        for file in &manifest.files {
            for hash in &file.chunks {
                live.insert(hash.clone());
            }
        }
    }
    Ok(())
}

/// Deletes physical chunk files that are not referenced by any workspace manifest.
pub fn sweep_unreferenced_chunks(layout: &Drive9Layout) -> Result<usize, Drive9Error> {
    let live = collect_referenced_chunk_hashes(layout)?;
    let mut removed = 0usize;
    let chunks_dir = layout.chunks_dir();
    if !chunks_dir.is_dir() {
        return Ok(0);
    }

    for prefix_entry in fs::read_dir(&chunks_dir)? {
        let prefix_entry = prefix_entry?;
        if !prefix_entry.file_type()?.is_dir() {
            continue;
        }
        for chunk_entry in fs::read_dir(prefix_entry.path())? {
            let chunk_entry = chunk_entry?;
            if !chunk_entry.file_type()?.is_file() {
                continue;
            }
            let hash = chunk_entry.file_name().to_string_lossy().to_string();
            if hash.starts_with(".tmp-") {
                continue;
            }
            if live.contains(&hash) {
                continue;
            }
            fs::remove_file(chunk_entry.path())?;
            removed += 1;
        }
    }

    Ok(removed)
}

/// Removes an entire workspace tree (manifests, HEAD, locks). Used when a user deletes a volume.
pub fn remove_workspace_tree(layout: &Drive9Layout, workspace_id: &str) -> Result<(), Drive9Error> {
    let path = layout.workspace_dir(workspace_id);
    if path.exists() {
        fs::remove_dir_all(&path)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apply_owner_prefix;
    use crate::cas::ChunkStore;
    use crate::config::Drive9Config;
    use crate::manifest::{ManifestFile, ManifestManager, MerkleManifest};
    use tempfile::tempdir;

    fn commit_manifest_with_chunks(
        layout: &Drive9Layout,
        chunk_store: &ChunkStore,
        workspace_id: &str,
        chunk_hashes: Vec<String>,
    ) -> Result<String, Drive9Error> {
        let manager = ManifestManager::new(layout.clone(), chunk_store.clone());
        let mut manifest = MerkleManifest::new(workspace_id.to_string(), None);
        manifest.files.push(ManifestFile {
            path: "shared.bin".to_string(),
            size_bytes: 0,
            mode_permissions: 0o644,
            chunks: chunk_hashes,
        });
        let hash = manager.commit_manifest(&manifest)?;
        manager.write_head(workspace_id, &hash)?;
        Ok(hash)
    }

    #[test]
    fn cas_two_users_share_identical_chunk_digest() {
        let dir = tempdir().unwrap();
        let layout = Drive9Config::new(dir.path()).layout();
        let store = ChunkStore::new(layout);

        let payload = b"shared dedup payload for alice and bob";
        let hash_a = store.put_chunk(payload).unwrap();
        let hash_b = store.put_chunk(payload).unwrap();
        assert_eq!(hash_a, hash_b);
        assert!(store.has_chunk(&hash_a));
    }

    #[test]
    fn gc_preserves_shared_chunk_when_one_prefixed_workspace_removed() {
        let dir = tempdir().unwrap();
        let layout = Drive9Config::new(dir.path()).layout();
        let chunk_store = ChunkStore::new(layout.clone());

        let alice_ws = apply_owner_prefix("alice", "assistant").unwrap();
        let bob_ws = apply_owner_prefix("bob", "assistant").unwrap();

        let shared = b"chunk referenced by both owners";
        let shared_hash = chunk_store.put_chunk(shared).unwrap();

        commit_manifest_with_chunks(&layout, &chunk_store, &alice_ws, vec![shared_hash.clone()])
            .unwrap();
        commit_manifest_with_chunks(&layout, &chunk_store, &bob_ws, vec![shared_hash.clone()])
            .unwrap();

        remove_workspace_tree(&layout, &alice_ws).unwrap();
        assert!(!layout.workspace_dir(&alice_ws).exists());
        assert!(layout.workspace_dir(&bob_ws).exists());

        let removed = sweep_unreferenced_chunks(&layout).unwrap();
        assert_eq!(removed, 0, "bob still references the shared chunk");

        let bytes = chunk_store.get_chunk(&shared_hash).unwrap();
        assert_eq!(bytes, shared);
    }

    #[test]
    fn gc_drops_chunk_when_no_manifest_references_remain() {
        let dir = tempdir().unwrap();
        let layout = Drive9Config::new(dir.path()).layout();
        let chunk_store = ChunkStore::new(layout.clone());

        let alice_ws = apply_owner_prefix("alice", "assistant").unwrap();
        let orphan_hash = chunk_store
            .put_chunk(b"only alice ever referenced this")
            .unwrap();
        let shared_hash = chunk_store.put_chunk(b"also on bob").unwrap();

        commit_manifest_with_chunks(
            &layout,
            &chunk_store,
            &alice_ws,
            vec![orphan_hash.clone(), shared_hash.clone()],
        )
        .unwrap();

        let bob_ws = apply_owner_prefix("bob", "assistant").unwrap();
        commit_manifest_with_chunks(&layout, &chunk_store, &bob_ws, vec![shared_hash.clone()])
            .unwrap();

        remove_workspace_tree(&layout, &alice_ws).unwrap();

        let removed = sweep_unreferenced_chunks(&layout).unwrap();
        assert_eq!(removed, 1, "orphan chunk should be swept");
        assert!(!chunk_store.has_chunk(&orphan_hash));
        assert!(chunk_store.has_chunk(&shared_hash));
    }

    #[test]
    fn collect_referenced_chunks_spans_all_workspace_directories() {
        let dir = tempdir().unwrap();
        let layout = Drive9Config::new(dir.path()).layout();
        let chunk_store = ChunkStore::new(layout.clone());

        let ws_a = apply_owner_prefix("user-a", "proj").unwrap();
        let ws_b = apply_owner_prefix("user-b", "proj").unwrap();
        let hash_a = chunk_store.put_chunk(b"a-only").unwrap();
        let hash_b = chunk_store.put_chunk(b"b-only").unwrap();

        commit_manifest_with_chunks(&layout, &chunk_store, &ws_a, vec![hash_a.clone()]).unwrap();
        commit_manifest_with_chunks(&layout, &chunk_store, &ws_b, vec![hash_b.clone()]).unwrap();

        let live = collect_referenced_chunk_hashes(&layout).unwrap();
        assert!(live.contains(&hash_a));
        assert!(live.contains(&hash_b));
    }

    #[test]
    fn workspace_manager_gc_preserves_cross_user_chunk_refs() {
        use crate::cache::CacheManager;
        use crate::disk::MockImageIo;
        use crate::manifest::ManifestManager;
        use crate::workspace::WorkspaceManager;

        let dir = tempdir().unwrap();
        let layout = Drive9Config::new(dir.path()).layout();
        let chunk_store = ChunkStore::new(layout.clone());
        let manifest_manager = ManifestManager::new(layout.clone(), chunk_store.clone());
        let cache_manager =
            CacheManager::new(layout.clone(), chunk_store.clone(), 10 * 1024 * 1024).unwrap();
        let manager = WorkspaceManager::new(
            layout.clone(),
            chunk_store.clone(),
            manifest_manager,
            cache_manager,
            Box::new(MockImageIo),
        );

        let alice_ws = apply_owner_prefix("alice", "ws").unwrap();
        let bob_ws = apply_owner_prefix("bob", "ws").unwrap();
        let shared_hash = chunk_store.put_chunk(b"shared-by-workspace-gc").unwrap();

        commit_manifest_with_chunks(&layout, &chunk_store, &alice_ws, vec![shared_hash.clone()])
            .unwrap();
        commit_manifest_with_chunks(&layout, &chunk_store, &bob_ws, vec![shared_hash.clone()])
            .unwrap();

        remove_workspace_tree(&layout, &alice_ws).unwrap();
        manager.gc().unwrap();

        assert!(chunk_store.get_chunk(&shared_hash).is_ok());
    }
}
