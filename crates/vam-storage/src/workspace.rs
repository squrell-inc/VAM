use crate::cache::CacheManager;
use crate::cas::{sha256_hex, ChunkStore};
use crate::config::{Drive9Config, Drive9Layout, StorageProfile};
use crate::disk::{Drive9ImageIo, E2fsImageIo};
use crate::error::{io_at, Drive9Error};
use crate::image_read::{
    ImageEntryKind, ImageLimits, ImageVisitor, ScanVisitor, TreeWriter, WalkReport,
};
use crate::instruction_template::{seed_plan_md_stub, seed_system_instruction_template};
use crate::manifest::{ManifestManager, MerkleManifest};
use crate::path::validate_manifest_path;
use crate::vam_dir::VAM_DIR;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

/// How a session's Drive9 tree reaches the guest.
///
/// `Ext4Image` (the default, and the only one a normal node uses) builds an
/// ext4 image from the materialized tree and extracts it again on commit.
/// `Directory` hands the guest the materialized directory itself; there is no
/// image, so no e2fsprogs. It exists for process guests (E9 test nodes), and
/// only vamd built with its `process-guest` feature selects it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Drive9Backend {
    #[default]
    Ext4Image,
    Directory,
}

/// Image IO of the directory backend: any call is a bug (it never builds or reads an image).
#[derive(Debug)]
struct NoImageIo;

impl Drive9ImageIo for NoImageIo {
    fn build_image(&self, _: &Path, _: &Path, _: u64) -> Result<(), Drive9Error> {
        Err(Drive9Error::Layout(
            "directory backend does not build images".into(),
        ))
    }
    fn walk_image(
        &self,
        _: &Path,
        _: &ImageLimits,
        _: &mut dyn ImageVisitor,
    ) -> Result<WalkReport, Drive9Error> {
        Err(Drive9Error::Layout(
            "directory backend does not read images".into(),
        ))
    }
    fn overlay_dir(&self, _: &Path, _: &Path, _: &str) -> Result<(), Drive9Error> {
        Err(Drive9Error::Layout(
            "directory backend does not overlay images".into(),
        ))
    }
}

/// Makes `root` and everything under it read-only (files 0444, dirs 0555).
fn make_read_only(root: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        if ty.is_dir() {
            make_read_only(&entry.path())?;
        } else if ty.is_file() {
            fs::set_permissions(entry.path(), fs::Permissions::from_mode(0o444))?;
        }
    }
    fs::set_permissions(root, fs::Permissions::from_mode(0o555))
}

/// Undoes [`make_read_only`] so the tree can be rewritten or removed.
fn make_writable(root: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let Ok(md) = fs::symlink_metadata(root) else {
        return;
    };
    if md.file_type().is_symlink() {
        return;
    }
    if md.is_dir() {
        let _ = fs::set_permissions(root, fs::Permissions::from_mode(0o755));
        if let Ok(rd) = fs::read_dir(root) {
            for e in rd.flatten() {
                make_writable(&e.path());
            }
        }
    } else {
        let _ = fs::set_permissions(root, fs::Permissions::from_mode(0o644));
    }
}

/// Runs `f` (which rewrites files under a session's `materialized/skills`) with the tree
/// writable, then puts it back to read-only if the directory backend had sealed it.
///
/// The directory backend makes `skills/` read-only at the end of materialize. Host code that
/// seeds more skill files afterwards (hosted-broker and MCP manifests) must go through this:
/// root bypasses the mode bits, so a plain rewrite only fails for a non-root vamd (CI).
pub fn with_skills_writable<T>(skills_dir: &Path, f: impl FnOnce() -> T) -> T {
    use std::os::unix::fs::PermissionsExt;
    let sealed = fs::metadata(skills_dir)
        .map(|m| m.is_dir() && m.permissions().mode() & 0o222 == 0)
        .unwrap_or(false);
    if sealed {
        make_writable(skills_dir);
    }
    let out = f();
    if sealed {
        let _ = make_read_only(skills_dir);
    }
    out
}

/// Where the files of an end commit come from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EndBase {
    /// The live tree (the image backend extracts its image first).
    Live,
    /// A turn commit's manifest (`None`: the current HEAD's files). Reads no files.
    TurnCommit(Option<String>),
}

/// An entry of the live tree that an end commit does not hold.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct DroppedEntry {
    pub path: String,
    pub size: u64,
}

/// The listed entries are capped; `count` is the real number.
pub const MAX_LISTED_DROPPED: usize = 200;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DroppedSummary {
    pub entries: Vec<DroppedEntry>,
    pub count: usize,
}

/// Result of [`WorkspaceManager::prepare_end`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedEnd {
    /// Manifest whose files the end commit holds; `None` for a workspace with
    /// no files at all yet.
    pub base_manifest: Option<String>,
    /// [`MerkleManifest::files_hash`] of those files.
    pub files_hash: String,
    pub dropped: DroppedSummary,
    /// The regular files of the live view that the end commit does not hold,
    /// with their sizes (what a stop keeps under `kept/<id>/rolled-back/`).
    /// `None` when they were not listed (a live end, or a live image that could
    /// not be read: the reclaim then lists them again).
    pub rolled_back: Option<Vec<(String, u64)>>,
    /// Whether the end re-installs the golden template from the live image. A
    /// stop does not: the live image holds the work that was rolled back.
    pub refresh_template: bool,
    /// Milliseconds the scan of the live view took (the stop timing line).
    pub scan_ms: u128,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BaseKind {
    Live,
    TurnCommit,
}

/// How much of the rolled-back work a stop kept.
pub const ROLLED_BACK_KEEP_CAP_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReclaimReport {
    /// `kept/<id>` holding `rolled-back/`, when any file was kept.
    pub kept_id: Option<String>,
    pub kept_files: usize,
    pub kept_bytes: u64,
    /// Rolled-back files listed but not kept because of the cap.
    pub skipped_over_cap: usize,
}

/// One entry of a live view of the workspace (a tree on disk, or a guest-written
/// image read through the safe reader), outside the host `.vam/`.
#[derive(Debug, Clone)]
struct Listed {
    rel: String,
    is_file: bool,
    is_dir: bool,
    size: u64,
}

fn listing_of_tree(root: &Path) -> Vec<Listed> {
    let mut out = Vec::new();
    walk_tree(root, root, &mut |rel, meta| {
        out.push(Listed {
            rel: rel.to_string(),
            is_file: meta.is_file(),
            is_dir: meta.is_dir(),
            size: if meta.is_file() { meta.len() } else { 0 },
        });
    });
    out
}

fn known_files(base: Option<&MerkleManifest>) -> std::collections::HashMap<&str, u64> {
    base.map(|m| {
        m.files
            .iter()
            .map(|f| (f.path.as_str(), f.size_bytes))
            .collect()
    })
    .unwrap_or_default()
}

/// Regular files of the listing that `base` does not hold as they are, with
/// their sizes, in path order. Links and devices are not listed here.
fn rolled_back_files(listing: &[Listed], base: Option<&MerkleManifest>) -> Vec<(String, u64)> {
    let known = known_files(base);
    let mut out: Vec<(String, u64)> = listing
        .iter()
        .filter(|e| e.is_file && known.get(e.rel.as_str()).copied() != Some(e.size))
        .map(|e| (e.rel.clone(), e.size))
        .collect();
    out.sort();
    out
}

/// What the listing holds that `base` does not: regular files that differ from
/// it, and entries a manifest cannot hold (links, devices, refused names).
fn dropped_entries(listing: &[Listed], base: Option<&MerkleManifest>) -> DroppedSummary {
    let known = known_files(base);
    let mut all: Vec<DroppedEntry> = listing
        .iter()
        .filter(|e| {
            let held = e.is_file && known.get(e.rel.as_str()).copied() == Some(e.size);
            !held && !e.is_dir
        })
        .map(|e| DroppedEntry {
            path: e.rel.clone(),
            size: if e.is_file { e.size } else { 0 },
        })
        .collect();
    all.sort_by(|a, b| a.path.cmp(&b.path));
    let count = all.len();
    all.truncate(MAX_LISTED_DROPPED);
    DroppedSummary {
        entries: all,
        count,
    }
}

/// Calls `visit(relative path, metadata)` for every entry under `root`, links
/// not followed, the host `.vam/` at the top left out.
fn walk_tree(root: &Path, dir: &Path, visit: &mut dyn FnMut(&str, &fs::Metadata)) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if dir == root && entry.file_name() == VAM_DIR {
            continue;
        }
        let Ok(meta) = fs::symlink_metadata(entry.path()) else {
            continue;
        };
        let rel = entry
            .path()
            .strip_prefix(root)
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        visit(&rel, &meta);
        if meta.is_dir() {
            walk_tree(root, &entry.path(), visit);
        }
    }
}

pub struct WorkspaceManager {
    backend: Drive9Backend,
    layout: Drive9Layout,
    chunk_store: ChunkStore,
    manifest_manager: ManifestManager,
    cache_manager: CacheManager,
    image_io: Box<dyn Drive9ImageIo>,
    /// One lock per session: a turn copy is taken, resolved and pruned, and the
    /// session tree is reclaimed, never at the same time.
    session_locks:
        std::sync::Mutex<std::collections::HashMap<String, std::sync::Arc<std::sync::Mutex<()>>>>,
}

fn record_step(
    sink: &mut Option<&mut Vec<(&'static str, u128)>>,
    label: &'static str,
    start: Instant,
) {
    if let Some(steps) = sink.as_mut() {
        steps.push((label, start.elapsed().as_micros()));
    }
}

fn copy_dir_all(
    src: impl AsRef<std::path::Path>,
    dst: impl AsRef<std::path::Path>,
) -> std::io::Result<()> {
    std::fs::create_dir_all(&dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        if ty.is_dir() {
            copy_dir_all(entry.path(), dst.as_ref().join(entry.file_name()))?;
        } else {
            let to = dst.as_ref().join(entry.file_name());
            // Replace, never write through: the file may be read-only (a skill restored from the
            // manifest is 0444; open-for-write is EACCES for a non-root user, root ignores it)
            // or a hard link into the chunk cache, which a write would corrupt.
            if to.symlink_metadata().is_ok() {
                std::fs::remove_file(&to)?;
            }
            std::fs::copy(entry.path(), &to)?;
        }
    }
    Ok(())
}

fn workspace_lock_path(layout: &Drive9Layout, workspace_id: &str) -> PathBuf {
    layout.workspace_dir(workspace_id).join("write.lock")
}

/// SHA-256 digest of a skills tree (relative path + file bytes), sorted by path.
pub fn skills_digest_from_dir(skills_dir: &Path) -> String {
    let mut entries: Vec<(String, Vec<u8>)> = Vec::new();
    if collect_skills_entries(skills_dir, skills_dir, &mut entries).is_err() {
        return "none".to_string();
    }
    if entries.is_empty() {
        return "none".to_string();
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    let mut hasher = Sha256::new();
    for (path, bytes) in entries {
        hasher.update(path.as_bytes());
        hasher.update(&bytes);
    }
    format!("{:x}", hasher.finalize())
}

/// Digest of seed `skills/` when present, otherwise `"none"`.
pub fn skills_digest(seed_dir: Option<&Path>) -> String {
    seed_dir
        .map(|seed| seed.join("skills"))
        .filter(|skills| skills.is_dir())
        .map(|skills| skills_digest_from_dir(&skills))
        .unwrap_or_else(|| "none".to_string())
}

fn collect_skills_entries(
    base: &Path,
    current: &Path,
    out: &mut Vec<(String, Vec<u8>)>,
) -> Result<(), Drive9Error> {
    for entry in fs::read_dir(current)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_skills_entries(base, &path, out)?;
        } else if path.is_file() {
            let rel = path
                .strip_prefix(base)
                .map_err(|_| Drive9Error::Layout("skills path escaped base".to_string()))?;
            let rel_str = rel.to_string_lossy().replace('\\', "/");
            let bytes = fs::read(&path)?;
            out.push((rel_str, bytes));
        }
    }
    Ok(())
}

fn template_head_key(
    manifest_manager: &ManifestManager,
    workspace_id: &str,
) -> Result<String, Drive9Error> {
    Ok(manifest_manager
        .read_head(workspace_id)?
        .unwrap_or_else(|| "empty".to_string()))
}

/// Opens a process-long Drive9 workspace manager rooted at `drive9_root`.
pub fn open_workspace_manager(drive9_root: &Path) -> Result<WorkspaceManager, Drive9Error> {
    open_workspace_manager_with(drive9_root, Drive9Backend::Ext4Image)
}

/// Like [`open_workspace_manager`] with an explicit backend. `Directory` needs no e2fsprogs.
pub fn open_workspace_manager_with(
    drive9_root: &Path,
    backend: Drive9Backend,
) -> Result<WorkspaceManager, Drive9Error> {
    let config = Drive9Config::new(drive9_root);
    let layout = config.layout();
    let chunk_store = ChunkStore::new(layout.clone());
    let manifest_manager = ManifestManager::new(layout.clone(), chunk_store.clone());
    let cache_manager =
        CacheManager::new(layout.clone(), chunk_store.clone(), 10 * 1024 * 1024 * 1024)?;
    let image_io: Box<dyn Drive9ImageIo> = match backend {
        Drive9Backend::Ext4Image => Box::new(E2fsImageIo::new()),
        Drive9Backend::Directory => Box::new(NoImageIo),
    };
    Ok(WorkspaceManager::new(
        layout,
        chunk_store,
        manifest_manager,
        cache_manager,
        image_io,
    )
    .with_backend(backend))
}

/// Returns the session ID currently holding the workspace write lease, if any.
pub fn read_workspace_lock_holder(
    layout: &Drive9Layout,
    workspace_id: &str,
) -> Result<Option<String>, Drive9Error> {
    let lock_path = workspace_lock_path(layout, workspace_id);
    if !lock_path.exists() {
        return Ok(None);
    }
    let holder = fs::read_to_string(&lock_path)?;
    let holder = holder.trim();
    if holder.is_empty() {
        return Ok(None);
    }
    Ok(Some(holder.to_string()))
}

/// Releases the workspace write lease.
///
/// When `session_id` is `Some`, the lock is removed only if that session holds it.
/// When `session_id` is `None`, the lock is removed regardless of holder.
pub fn release_workspace_lock(
    layout: &Drive9Layout,
    workspace_id: &str,
    session_id: Option<&str>,
) -> Result<(), Drive9Error> {
    let lock_path = workspace_lock_path(layout, workspace_id);
    if !lock_path.exists() {
        return Ok(());
    }
    if let Some(expected) = session_id {
        let holder = fs::read_to_string(&lock_path)?;
        if holder.trim() != expected {
            return Ok(());
        }
    }
    fs::remove_file(&lock_path)?;
    Ok(())
}

impl WorkspaceManager {
    pub fn new(
        layout: Drive9Layout,
        chunk_store: ChunkStore,
        manifest_manager: ManifestManager,
        cache_manager: CacheManager,
        image_io: Box<dyn Drive9ImageIo>,
    ) -> Self {
        Self {
            backend: Drive9Backend::Ext4Image,
            layout,
            chunk_store,
            manifest_manager,
            cache_manager,
            image_io,
            session_locks: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// Runs `f` while holding the session's lock. Turn copies (snapshot, resolve,
    /// prune) and the removal of the session tree take it, so a reclaim cannot
    /// pull the tree from under a copy, nor a copy start in a tree being reclaimed.
    pub fn with_session_lock<R>(&self, session_id: &str, f: impl FnOnce() -> R) -> R {
        let lock = {
            let mut map = self
                .session_locks
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            map.entry(session_id.to_string()).or_default().clone()
        };
        let _held = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        f()
    }

    /// Selects the Drive9 backend (default `Ext4Image`).
    pub fn with_backend(mut self, backend: Drive9Backend) -> Self {
        self.backend = backend;
        self
    }

    pub fn backend(&self) -> Drive9Backend {
        self.backend
    }

    /// Acquires a single-writer lease lock on the workspace.
    pub fn acquire_lock(&self, workspace_id: &str, session_id: &str) -> Result<(), Drive9Error> {
        let lock_path = workspace_lock_path(&self.layout, workspace_id);
        if let Some(parent) = lock_path.parent() {
            fs::create_dir_all(parent)?;
        }

        if let Some(active_session) = read_workspace_lock_holder(&self.layout, workspace_id)? {
            if active_session != session_id {
                return Err(Drive9Error::Layout(format!(
                    "Workspace '{}' is already locked by session '{}'",
                    workspace_id, active_session
                )));
            }
        }

        fs::write(&lock_path, session_id)?;
        Ok(())
    }

    /// Releases a single-writer lease lock.
    pub fn release_lock(&self, workspace_id: &str, session_id: &str) -> Result<(), Drive9Error> {
        release_workspace_lock(&self.layout, workspace_id, Some(session_id))
    }

    /// Returns the session ID currently holding this workspace's write lease.
    pub fn lock_holder(&self, workspace_id: &str) -> Result<Option<String>, Drive9Error> {
        read_workspace_lock_holder(&self.layout, workspace_id)
    }

    pub fn layout(&self) -> &Drive9Layout {
        &self.layout
    }

    pub fn chunk_store(&self) -> &ChunkStore {
        &self.chunk_store
    }

    fn install_template(
        &self,
        workspace_id: &str,
        head_hash: &str,
        skills_digest: &str,
        session_image: &Path,
    ) -> Result<(), Drive9Error> {
        let template_path = self
            .layout
            .template_path(workspace_id, head_hash, skills_digest);
        if let Some(parent) = template_path.parent() {
            fs::create_dir_all(parent)?;
        }
        self.cache_manager
            .clone_image(session_image, &template_path)
    }

    /// Materializes a workspace's state into a session directory on the host.
    pub fn materialize(
        &self,
        session_id: &str,
        workspace_id: &str,
        profile: &StorageProfile,
        seed_dir: Option<&std::path::Path>,
        catalog_skills: Option<&std::path::Path>,
        mut timing: Option<&mut Vec<(&'static str, u128)>>,
    ) -> Result<PathBuf, Drive9Error> {
        // 1. Enforce single-writer lock
        let t = Instant::now();
        self.acquire_lock(workspace_id, session_id).map_err(|e| {
            e.at(
                "materialize.acquire_lock",
                &workspace_lock_path(&self.layout, workspace_id),
            )
        })?;
        record_step(&mut timing, "materialize_acquire_lock", t);

        // 2. Resolve session & materialized paths
        let session_dir = self.layout.session_dir(session_id);
        let materialized_dir = session_dir.join("materialized");
        io_at(
            fs::create_dir_all(&materialized_dir),
            "materialize.create_session_dir",
            &materialized_dir,
        )?;
        if self.backend == Drive9Backend::Directory {
            // A previous materialize of this session left skills read-only.
            make_writable(&materialized_dir.join("skills"));
        }

        // 3. Resolve active HEAD manifest
        let t = Instant::now();
        if let Some(manifest_hash) = self.manifest_manager.read_head(workspace_id)? {
            let manifest = self
                .manifest_manager
                .get_manifest(workspace_id, &manifest_hash)?;

            // 4. Reconstruct directory files from cached/stored chunks
            let cache_files_dir = self.layout.cache_dir().join("files");
            io_at(
                fs::create_dir_all(&cache_files_dir),
                "materialize.create_cache_dir",
                &cache_files_dir,
            )?;

            for file in &manifest.files {
                // A manifest can come from a guest-written image: never join a path
                // that is absolute or climbs out.
                validate_manifest_path(&file.path)?;
                let file_path = materialized_dir.join(&file.path);
                if let Some(parent) = file_path.parent() {
                    io_at(
                        fs::create_dir_all(parent),
                        "materialize.create_file_parent",
                        parent,
                    )?;
                }

                // Gather and check file content hash
                // We'll construct the file under a cached path based on manifest's chunk sequence hash
                let cache_file_key = sha256_hex(file.chunks.join(",").as_bytes());
                let cached_file_path = cache_files_dir.join(&cache_file_key);

                if !cached_file_path.exists() {
                    let reg = vam_telemetry::VamMetricsRegistry::global();
                    let _ = reg.record_counter(
                        vam_telemetry::METRIC_DRIVE9_CACHE_MISSES_TOTAL,
                        1,
                        &[("subsystem", "drive9"), ("result", "miss")],
                    );
                    // Assemble from chunks
                    let mut file_data = Vec::new();
                    for chunk_hash in &file.chunks {
                        let chunk_bytes = self.chunk_store.get_chunk(chunk_hash)?;
                        file_data.extend_from_slice(&chunk_bytes);
                    }
                    io_at(
                        fs::write(&cached_file_path, &file_data),
                        "materialize.write_cache_file",
                        &cached_file_path,
                    )?;
                } else {
                    let reg = vam_telemetry::VamMetricsRegistry::global();
                    let _ = reg.record_counter(
                        vam_telemetry::METRIC_DRIVE9_CACHE_HITS_TOTAL,
                        1,
                        &[("subsystem", "drive9"), ("result", "hit")],
                    );
                }

                // Pin the chunks for this session
                self.cache_manager.pin_chunks(&file.chunks)?;

                // Clone/link from the cache to the session's materialized folder
                self.cache_manager
                    .clone_file(&cached_file_path, &file_path)
                    .map_err(|e| e.at("materialize.clone_cache_file", &file_path))?;

                // Apply permissions
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = fs::set_permissions(
                        &file_path,
                        fs::Permissions::from_mode(file.mode_permissions),
                    );
                }
            }
        } else {
            // New workspace: prepopulate with default structure (e.g. empty workspace/ folder)
            io_at(
                fs::create_dir_all(materialized_dir.join("workspace")),
                "materialize.create_workspace_dir",
                &materialized_dir,
            )?;
            io_at(
                fs::write(
                    materialized_dir.join("SKILLS.md"),
                    b"# Workspace Capability Instructions\n",
                ),
                "materialize.write_skills_md",
                &materialized_dir,
            )?;

            if let Some(seed) = seed_dir {
                let _ = copy_dir_all(seed, &materialized_dir);
            }
        }
        // A HEAD with no files under `workspace/` (a stop before any turn
        // commit keeps only the conversation) still gives the guest its dir.
        io_at(
            fs::create_dir_all(materialized_dir.join("workspace")),
            "materialize.ensure_workspace_dir",
            &materialized_dir,
        )?;
        record_step(&mut timing, "materialize_head_chunks", t);

        // Skills are host-authoritative: overlay seed skills on every materialize so
        // workspace HEAD cannot pin a stale/corrupt execute.js across sessions.
        let t = Instant::now();
        if let Some(seed) = seed_dir {
            let seed_skills = seed.join("skills");
            let mat_skills = materialized_dir.join("skills");
            if seed_skills.is_dir() {
                let _ = copy_dir_all(&seed_skills, &mat_skills);
            }
        }
        record_step(&mut timing, "materialize_seed_copy", t);

        // Catalog skill packages overlay seed skills (catalog wins per folder name).
        let t = Instant::now();
        let mat_skills = materialized_dir.join("skills");
        if let Some(catalog) = catalog_skills {
            if catalog.is_dir() {
                for entry in fs::read_dir(catalog)? {
                    let entry = entry?;
                    let path = entry.path();
                    if path.is_dir() {
                        copy_dir_all(&path, mat_skills.join(entry.file_name()))?;
                    }
                }
            }
        }
        record_step(&mut timing, "materialize_catalog_overlay", t);

        // 5. Seed or refresh the guest system instruction template and optional PLAN.md stub.
        let t = Instant::now();
        seed_system_instruction_template(&materialized_dir)
            .map_err(|e| e.at("materialize.seed_instruction_template", &materialized_dir))?;
        seed_plan_md_stub(&materialized_dir)
            .map_err(|e| e.at("materialize.seed_plan_stub", &materialized_dir))?;
        record_step(&mut timing, "materialize_template_seed", t);

        if self.backend == Drive9Backend::Directory {
            // The guest works in `materialized/` itself: no image, no golden
            // template. Skills are read-only (in a VM they are a read-only drive).
            if mat_skills.is_dir() {
                make_read_only(&mat_skills)?;
            }
            return Ok(materialized_dir);
        }

        // 6. Generate session ext4: clone golden template on hit, mke2fs on miss.
        let t = Instant::now();
        let image_path = self.layout.session_image_path(session_id);
        if let Some(parent) = image_path.parent() {
            fs::create_dir_all(parent)?;
        }

        let head_key = template_head_key(&self.manifest_manager, workspace_id)?;
        let skills_key = if mat_skills.is_dir() {
            skills_digest_from_dir(&mat_skills)
        } else {
            "none".to_string()
        };
        // Keyed by the host-prefixed workspace. Unprefixed goldens are not
        // consulted — first session after the prefix misses and re-bakes.
        let template_path = self
            .layout
            .template_path(workspace_id, &head_key, &skills_key);

        let mut used_clone = false;
        if template_path.is_file()
            && self
                .cache_manager
                .clone_image(&template_path, &image_path)
                .map_err(|e| {
                    tracing::warn!(
                        "template image clone failed, rebuilding: {}",
                        e.at("materialize.clone_template_image", &image_path)
                    );
                })
                .is_ok()
        {
            used_clone = true;
            record_step(&mut timing, "materialize_clone_image", t);
        }

        if !used_clone {
            let build_t = Instant::now();
            self.image_io
                .build_image(&materialized_dir, &image_path, profile.quota_bytes)
                .map_err(|e| e.at("materialize.build_image", &image_path))?;
            record_step(&mut timing, "materialize_build_image", build_t);
            let _ = self.install_template(workspace_id, &head_key, &skills_key, &image_path);
        }

        Ok(materialized_dir)
    }

    /// A session tree holding only the workspace's host `.vam/` (the conversation and
    /// timeline) as HEAD has it, for an end the host writes with no VM: the run parked
    /// on an approval and its guest is gone. Takes the workspace lock for `session_id`.
    /// The workspace files are not materialized: the end commit takes them from a
    /// manifest ([`Self::prepare_end`] / [`Self::finish_end`]), not from this tree.
    pub fn materialize_host_dir(
        &self,
        session_id: &str,
        workspace_id: &str,
    ) -> Result<PathBuf, Drive9Error> {
        self.acquire_lock(workspace_id, session_id)?;
        let materialized = self.layout.session_dir(session_id).join("materialized");
        io_at(
            fs::create_dir_all(&materialized),
            "materialize_host_dir.create_session_dir",
            &materialized,
        )?;
        let host = materialized.join(VAM_DIR);
        if host.exists() {
            io_at(
                fs::remove_dir_all(&host),
                "materialize_host_dir.clear",
                &host,
            )?;
        }
        if let Some(head) = self.manifest_manager.read_head(workspace_id)? {
            let manifest = self.manifest_manager.get_manifest(workspace_id, &head)?;
            let prefix = format!("{VAM_DIR}/");
            for file in &manifest.files {
                validate_manifest_path(&file.path)?;
                if !file.path.starts_with(&prefix) {
                    continue;
                }
                let dest = materialized.join(&file.path);
                if let Some(parent) = dest.parent() {
                    io_at(
                        fs::create_dir_all(parent),
                        "materialize_host_dir.create_parent",
                        parent,
                    )?;
                }
                let mut data = Vec::new();
                for chunk in &file.chunks {
                    data.extend_from_slice(&self.chunk_store.get_chunk(chunk)?);
                }
                io_at(fs::write(&dest, &data), "materialize_host_dir.write", &dest)?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = fs::set_permissions(
                        &dest,
                        fs::Permissions::from_mode(file.mode_permissions),
                    );
                }
            }
        }
        Ok(materialized)
    }

    /// Commits the modified session directory back, generating chunks, manifests, and updating HEAD.
    pub fn commit(&self, session_id: &str, workspace_id: &str) -> Result<String, Drive9Error> {
        let session_dir = self.layout.session_dir(session_id);
        let materialized_dir = session_dir.join("materialized");
        let image_path = self.layout.session_image_path(session_id);

        if !materialized_dir.exists() && !image_path.exists() {
            return Err(Drive9Error::Layout(format!(
                "Session '{}' has not been materialized",
                session_id
            )));
        }

        crate::disk::restore_held_vam(&materialized_dir);
        // 1. Read parent manifest hash from HEAD
        let parent_hash = self.manifest_manager.read_head(workspace_id)?;

        // 2. Build the manifest. A VM guest wrote an ext4 image: read it through
        // the safe reader (the host `.vam/` is never read from it) and compose the
        // host's own `.vam/` in from the session tree. The directory backend has no
        // image: the guest wrote materialized/ itself.
        let manifest = if self.backend == Drive9Backend::Ext4Image && image_path.exists() {
            let (files, _report) = self.manifest_manager.build_from_image(
                workspace_id,
                self.image_io.as_ref(),
                &image_path,
                parent_hash.clone(),
                &ImageLimits::from_env(),
            )?;
            self.manifest_manager.compose_with_host_dir(
                workspace_id,
                Some(&files),
                &materialized_dir,
                VAM_DIR,
                parent_hash,
            )?
        } else {
            self.manifest_manager
                .build_from_dir(workspace_id, &materialized_dir, parent_hash)?
        };

        // 5. Commit manifest
        let manifest_hash = self.manifest_manager.commit_manifest(&manifest)?;

        // 6. Update HEAD
        self.manifest_manager
            .write_head(workspace_id, &manifest_hash)?;

        // Unpin any previous session chunks if needed, but since we are committing we update cache pin count
        // Let's gather all chunk hashes to record and admit them to the LRU cache
        let mut chunks = Vec::new();
        for file in &manifest.files {
            chunks.extend(file.chunks.clone());
        }
        // Cache chunks
        self.cache_manager.pin_chunks(&chunks)?;

        // Refresh golden template at the new HEAD (captures guest writes).
        let skills_key = {
            let skills_dir = materialized_dir.join("skills");
            if skills_dir.is_dir() {
                skills_digest_from_dir(&skills_dir)
            } else {
                "none".to_string()
            }
        };
        if self.backend == Drive9Backend::Ext4Image {
            let _ = self.install_template(workspace_id, &manifest_hash, &skills_key, &image_path);
        }

        Ok(manifest_hash)
    }

    /// Commits the session workspace and releases the single-writer lock.
    pub fn commit_and_release(
        &self,
        session_id: &str,
        workspace_id: &str,
    ) -> Result<String, Drive9Error> {
        let manifest_hash = self.commit(session_id, workspace_id)?;
        self.release_lock(workspace_id, session_id)?;
        Ok(manifest_hash)
    }

    /// A turn commit of a directory-backed session: a manifest of the session
    /// tree with the host's `.vam/` left out, written to the manifest store.
    /// HEAD does not move. The guest is waiting on this, so it reads the tree
    /// as it is (no freeze). Symlinks are not followed.
    pub fn snapshot_tree_manifest(
        &self,
        session_id: &str,
        workspace_id: &str,
    ) -> Result<String, Drive9Error> {
        if self.backend != Drive9Backend::Directory {
            return Err(Drive9Error::Layout(
                "tree snapshots need the directory backend".into(),
            ));
        }
        let materialized = self.layout.session_dir(session_id).join("materialized");
        let parent = self.manifest_manager.read_head(workspace_id)?;
        let manifest = self.manifest_manager.build_from_dir_excluding(
            workspace_id,
            &materialized,
            parent,
            &[VAM_DIR],
        )?;
        self.manifest_manager.commit_manifest(&manifest)
    }

    /// The files hash of a committed manifest (see
    /// [`MerkleManifest::files_hash`]).
    pub fn manifest_files_hash(
        &self,
        workspace_id: &str,
        manifest_hash: &str,
    ) -> Result<String, Drive9Error> {
        Ok(self
            .manifest_manager
            .get_manifest(workspace_id, manifest_hash)?
            .files_hash())
    }

    /// The workspace's current HEAD manifest hash, if it has one.
    pub fn head(&self, workspace_id: &str) -> Result<Option<String>, Drive9Error> {
        self.manifest_manager.read_head(workspace_id)
    }

    /// Takes a private copy of the session's live image as turn copy `round`
    /// (`sessions/<id>/turns/<round>.ext4`): a reflink where the volume has them,
    /// else a deadline-bound hole-preserving copy (see [`crate::image_snapshot`]).
    /// The guest holds its filesystem frozen while this runs. The copy is a new
    /// inode in the host-only part of the session directory, never staged into a
    /// jail. Fails if the session tree is gone or the image is missing.
    pub fn snapshot_turn_image(
        &self,
        session_id: &str,
        round: u32,
        limits: &crate::image_snapshot::SnapshotLimits,
    ) -> Result<crate::image_snapshot::SnapshotReport, Drive9Error> {
        if self.backend != Drive9Backend::Ext4Image {
            return Err(Drive9Error::Layout(
                "turn copies need the image backend".into(),
            ));
        }
        self.with_session_lock(session_id, || {
            let session_dir = self.layout.session_dir(session_id);
            let image = self.layout.session_image_path(session_id);
            if !session_dir.is_dir() || !image.is_file() {
                return Err(Drive9Error::NotFound(
                    "the session image to copy".to_string(),
                ));
            }
            let turns = self.layout.session_turns_dir(session_id);
            if !turns.is_dir() {
                io_at(fs::create_dir(&turns), "snapshot.create_turns_dir", &turns)?;
                use std::os::unix::fs::PermissionsExt as _;
                io_at(
                    fs::set_permissions(&turns, fs::Permissions::from_mode(0o700)),
                    "snapshot.chmod_turns_dir",
                    &turns,
                )?;
            }
            let copy = self.layout.session_turn_image_path(session_id, round);
            crate::image_snapshot::snapshot_image(&image, &copy, limits)
        })
    }

    /// Removes every turn copy of the session except the rounds in `keep`
    /// (older copies, failed commits' copies, leftovers of an earlier run).
    /// Returns how many files went.
    pub fn prune_turn_copies(&self, session_id: &str, keep: &[u32]) -> Result<usize, Drive9Error> {
        self.with_session_lock(session_id, || {
            let turns = self.layout.session_turns_dir(session_id);
            let entries = match fs::read_dir(&turns) {
                Ok(entries) => entries,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
                Err(e) => return Err(Drive9Error::from(e).at("prune.read_turns_dir", &turns)),
            };
            let mut removed = 0;
            for entry in entries.flatten() {
                let name = entry.file_name();
                let round = name
                    .to_str()
                    .and_then(|n| n.strip_suffix(".ext4"))
                    .and_then(|n| n.parse::<u32>().ok());
                if round.is_some_and(|r| keep.contains(&r)) {
                    continue;
                }
                let path = entry.path();
                let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
                let gone = if is_dir {
                    fs::remove_dir_all(&path)
                } else {
                    fs::remove_file(&path)
                };
                if gone.is_ok() {
                    removed += 1;
                }
            }
            Ok(removed)
        })
    }

    /// The rounds that have a turn copy on disk, in ascending order.
    pub fn turn_copy_rounds(&self, session_id: &str) -> Vec<u32> {
        let turns = self.layout.session_turns_dir(session_id);
        let mut rounds: Vec<u32> = fs::read_dir(&turns)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                e.file_name()
                    .to_str()
                    .and_then(|n| n.strip_suffix(".ext4"))
                    .and_then(|n| n.parse().ok())
            })
            .collect();
        rounds.sort_unstable();
        rounds
    }

    /// Reads turn copy `round` with the safe reader into a manifest in the chunk
    /// store (HEAD does not move) and returns its hash. The copy is the host's
    /// own file, but its bytes are the guest's: an unreadable image is
    /// [`Drive9Error::ImageCorrupt`], a bound hit is [`Drive9Error::ImageLimit`].
    pub fn resolve_turn_copy(
        &self,
        session_id: &str,
        workspace_id: &str,
        round: u32,
    ) -> Result<String, Drive9Error> {
        self.with_session_lock(session_id, || {
            let copy = self.layout.session_turn_image_path(session_id, round);
            let meta = fs::symlink_metadata(&copy)
                .map_err(|e| Drive9Error::from(e).at("resolve.stat_turn_copy", &copy))?;
            if !meta.is_file() {
                return Err(Drive9Error::Layout(
                    "a turn copy is not a regular file".into(),
                ));
            }
            let head = self.manifest_manager.read_head(workspace_id)?;
            let (manifest, _report) = self.manifest_manager.build_from_image(
                workspace_id,
                self.image_io.as_ref(),
                &copy,
                head,
                &ImageLimits::from_env(),
            )?;
            self.manifest_manager.commit_manifest(&manifest)
        })
    }

    /// Every entry of a guest-written image (metadata only, nothing is read or
    /// written on the host), plus the names the reader refused, as a listing.
    fn listing_of_image(&self, image_path: &Path) -> Result<Vec<Listed>, Drive9Error> {
        let mut scan = ScanVisitor::default();
        let report = self
            .image_io
            .walk_image(image_path, &ImageLimits::from_env(), &mut scan)?;
        let mut out: Vec<Listed> = scan
            .entries
            .into_iter()
            .map(|e| Listed {
                is_file: e.kind == ImageEntryKind::File,
                is_dir: e.kind == ImageEntryKind::Dir,
                size: e.size,
                rel: e.path,
            })
            .collect();
        out.extend(report.skipped.into_iter().map(|s| Listed {
            rel: s.path,
            is_file: false,
            is_dir: false,
            size: 0,
        }));
        Ok(out)
    }

    /// First phase of the end of a session: fixes the files the end commit will
    /// hold and their [`files_hash`](crate::MerkleManifest::files_hash), so the
    /// conversation can record the hash before the host `.vam/` is composed in.
    /// The VM must be down. HEAD does not move and the lock is kept.
    ///
    /// * [`EndBase::Live`]: the live tree (the image backend extracts its image
    ///   first), the host `.vam/` left out.
    /// * [`EndBase::TurnCommit`]: no files are read; the files of that turn
    ///   commit's manifest, or of HEAD when there is none.
    ///
    /// `dropped` lists what the live tree holds that the end commit will not:
    /// after a stop, the files written since the base; after a live end, entries
    /// a manifest cannot hold (links, devices).
    pub fn prepare_end(
        &self,
        session_id: &str,
        workspace_id: &str,
        base: EndBase,
    ) -> Result<PreparedEnd, Drive9Error> {
        let session_dir = self.layout.session_dir(session_id);
        let materialized = session_dir.join("materialized");
        let image_path = self.layout.session_image_path(session_id);
        if !materialized.exists() && !image_path.exists() {
            return Err(Drive9Error::Layout(format!(
                "Session '{}' has not been materialized",
                session_id
            )));
        }
        let head = self.manifest_manager.read_head(workspace_id)?;
        let from_image = self.backend == Drive9Backend::Ext4Image && image_path.exists();
        let base_kind = match &base {
            EndBase::Live => BaseKind::Live,
            EndBase::TurnCommit(_) => BaseKind::TurnCommit,
        };
        let (base_manifest, manifest): (Option<String>, Option<MerkleManifest>) = match base {
            EndBase::Live if from_image => {
                // Read through the safe reader; the host `.vam/` in the session tree
                // is composed in by `finish_end`.
                let (manifest, _report) = self.manifest_manager.build_from_image(
                    workspace_id,
                    self.image_io.as_ref(),
                    &image_path,
                    head,
                    &ImageLimits::from_env(),
                )?;
                let hash = self.manifest_manager.commit_manifest(&manifest)?;
                (Some(hash), Some(manifest))
            }
            EndBase::Live => {
                let manifest = self.manifest_manager.build_from_dir_excluding(
                    workspace_id,
                    &materialized,
                    head,
                    &[VAM_DIR],
                )?;
                let hash = self.manifest_manager.commit_manifest(&manifest)?;
                (Some(hash), Some(manifest))
            }
            EndBase::TurnCommit(chosen) => match chosen.or(head) {
                Some(hash) => {
                    let manifest = self.manifest_manager.get_manifest(workspace_id, &hash)?;
                    (Some(hash), Some(manifest))
                }
                None => (None, None),
            },
        };
        let files_hash = manifest
            .as_ref()
            .map(MerkleManifest::files_hash)
            .unwrap_or_else(MerkleManifest::empty_files_hash);
        // What the live view holds that the end commit will not: for a VM guest the
        // image as the guest left it, else the session tree. After a stop the scan
        // only informs the conversation and the kept copy: a guest that wrote a
        // hostile or unreadable image must not block the rollback, so a failure
        // there is logged and the lists stay empty (the reclaim lists again).
        let stopped = matches!(base_kind, BaseKind::TurnCommit);
        let scan_started = Instant::now();
        let listing = if from_image {
            match self.listing_of_image(&image_path) {
                Ok(listing) => Some(listing),
                Err(e) if stopped => {
                    tracing::warn!(
                        session = session_id,
                        "the live image could not be scanned for the rolled-back list: {e}"
                    );
                    None
                }
                Err(e) => return Err(e),
            }
        } else {
            Some(listing_of_tree(&materialized))
        };
        let (dropped, rolled_back) = match listing.as_ref() {
            Some(listing) => (
                dropped_entries(listing, manifest.as_ref()),
                stopped.then(|| rolled_back_files(listing, manifest.as_ref())),
            ),
            None => (DroppedSummary::default(), None),
        };
        Ok(PreparedEnd {
            base_manifest,
            files_hash,
            dropped,
            rolled_back,
            refresh_template: !stopped,
            scan_ms: scan_started.elapsed().as_millis(),
        })
    }

    /// Second phase: HEAD becomes the prepared files plus the session tree's
    /// host `.vam/` (which by now holds the end of the conversation). The image
    /// backend also refreshes its golden template, except after a stop. Returns the new HEAD; the
    /// caller releases the lock.
    pub fn finish_end(
        &self,
        session_id: &str,
        workspace_id: &str,
        prepared: &PreparedEnd,
    ) -> Result<String, Drive9Error> {
        let hash =
            self.compose_head(session_id, workspace_id, prepared.base_manifest.as_deref())?;
        // A stop leaves the golden template alone: the live image holds the work
        // the stop rolled back, and a template must hold what HEAD holds.
        if self.backend == Drive9Backend::Ext4Image && prepared.refresh_template {
            let materialized = self.layout.session_dir(session_id).join("materialized");
            let skills_dir = materialized.join("skills");
            let skills_key = if skills_dir.is_dir() {
                skills_digest_from_dir(&skills_dir)
            } else {
                "none".to_string()
            };
            let image_path = self.layout.session_image_path(session_id);
            let _ = self.install_template(workspace_id, &hash, &skills_key, &image_path);
        }
        Ok(hash)
    }

    /// Removes a settled session's tree once HEAD reads back as `head_after`.
    /// With `keep_rolled_back` the files a stop rolled back (the live tree's
    /// regular files that `base_manifest` does not hold) are first copied to
    /// `kept/<id>/rolled-back/`, up to `cap_bytes`; the rest are only listed
    /// (the caller records the list). `listed` is the rolled-back list the stop's
    /// `prepare_end` already took; without it the files are listed again. Never
    /// called for a paused or warm session. Holds the session lock, so no turn
    /// copy is taken, resolved or pruned meanwhile.
    #[allow(clippy::too_many_arguments)]
    pub fn reclaim_session_tree(
        &self,
        session_id: &str,
        workspace_id: &str,
        head_after: &str,
        base_manifest: Option<&str>,
        keep_rolled_back: bool,
        listed: Option<Vec<(String, u64)>>,
        cap_bytes: u64,
    ) -> Result<ReclaimReport, Drive9Error> {
        let report = self.with_session_lock(session_id, || {
            self.reclaim_locked(
                session_id,
                workspace_id,
                head_after,
                base_manifest,
                keep_rolled_back,
                listed,
                cap_bytes,
            )
        });
        if report.is_ok() {
            // The tree is gone: so is the need for its lock entry.
            if let Ok(mut map) = self.session_locks.lock() {
                map.remove(session_id);
            }
        }
        report
    }

    #[allow(clippy::too_many_arguments)]
    fn reclaim_locked(
        &self,
        session_id: &str,
        workspace_id: &str,
        head_after: &str,
        base_manifest: Option<&str>,
        keep_rolled_back: bool,
        listed: Option<Vec<(String, u64)>>,
        cap_bytes: u64,
    ) -> Result<ReclaimReport, Drive9Error> {
        if self.manifest_manager.read_head(workspace_id)?.as_deref() != Some(head_after) {
            return Err(Drive9Error::Layout(
                "HEAD is not the settled commit; the session tree stays".into(),
            ));
        }
        self.manifest_manager
            .get_manifest(workspace_id, head_after)?;
        let session_dir = self.layout.session_dir(session_id);
        let materialized = session_dir.join("materialized");
        let mut report = ReclaimReport::default();
        let image_path = self.layout.session_image_path(session_id);
        let from_image = self.backend == Drive9Backend::Ext4Image && image_path.is_file();
        if keep_rolled_back && (from_image || materialized.is_dir()) {
            let base = match base_manifest {
                Some(hash) => Some(self.manifest_manager.get_manifest(workspace_id, hash)?),
                None => None,
            };
            // A VM guest's live files are in its image (the session tree holds the
            // pre-boot files), read through the safe reader. A failure to list
            // them keeps the tree: the rolled-back files are not discarded.
            let files = match listed {
                Some(files) => files,
                None => {
                    let listing = if from_image {
                        self.listing_of_image(&image_path)?
                    } else {
                        listing_of_tree(&materialized)
                    };
                    rolled_back_files(&listing, base.as_ref())
                }
            };
            if !files.is_empty() {
                let mut kept_id = session_id.to_string();
                let mut n = 1u32;
                while self.layout.kept_session_dir(&kept_id).exists() {
                    kept_id = format!("{session_id}-{n}");
                    n += 1;
                }
                let target = self.layout.kept_session_dir(&kept_id).join("rolled-back");
                // The files that fit under the cap, in path order.
                let mut chosen: Vec<(String, u64)> = Vec::new();
                let mut chosen_bytes = 0u64;
                for (rel, size) in files {
                    if chosen_bytes.saturating_add(size) > cap_bytes {
                        report.skipped_over_cap += 1;
                        continue;
                    }
                    chosen_bytes += size;
                    chosen.push((rel, size));
                }
                if from_image {
                    if !chosen.is_empty() {
                        io_at(fs::create_dir_all(&target), "reclaim.create_dir", &target)?;
                        // Image names reach the host only here: keep the copy root-only.
                        use std::os::unix::fs::PermissionsExt as _;
                        for dir in [&target, target.parent().unwrap_or(&target)] {
                            io_at(
                                fs::set_permissions(dir, fs::Permissions::from_mode(0o700)),
                                "reclaim.chmod_kept",
                                dir,
                            )?;
                        }
                        let only = chosen.iter().map(|(rel, _)| rel.clone()).collect();
                        let mut writer = TreeWriter::new(target.clone(), Some(only));
                        self.image_io.walk_image(
                            &image_path,
                            &ImageLimits::from_env(),
                            &mut writer,
                        )?;
                        report.kept_files = writer.written_files as usize;
                        report.kept_bytes = writer.written_bytes;
                    }
                } else {
                    for (rel, size) in chosen {
                        let to = target.join(&rel);
                        if let Some(parent) = to.parent() {
                            io_at(fs::create_dir_all(parent), "reclaim.create_dir", parent)?;
                        }
                        io_at(
                            fs::copy(materialized.join(&rel), &to),
                            "reclaim.copy_rolled_back",
                            &to,
                        )?;
                        report.kept_files += 1;
                        report.kept_bytes += size;
                    }
                }
                if report.kept_files > 0 {
                    report.kept_id = Some(kept_id);
                }
            }
        }
        if self.backend == Drive9Backend::Directory {
            make_writable(&session_dir);
        }
        io_at(
            fs::remove_dir_all(&session_dir),
            "reclaim.remove_tree",
            &session_dir,
        )?;
        Ok(report)
    }

    /// Ends a session without reading its files: HEAD becomes `base_hash`'s
    /// files (or the current HEAD's when `None`) plus the session tree's host
    /// `.vam/` (the conversation and timeline). The live tree (or, for a VM, the
    /// live image) is left where it is. The caller releases the workspace lock
    /// once HEAD is written (`release_lock`).
    pub fn commit_at_manifest(
        &self,
        session_id: &str,
        workspace_id: &str,
        base_hash: Option<&str>,
    ) -> Result<String, Drive9Error> {
        self.compose_head(session_id, workspace_id, base_hash)
    }

    /// HEAD := `base_hash`'s files (or the current HEAD's) plus the session
    /// tree's host `.vam/`. Shared by both backends.
    fn compose_head(
        &self,
        session_id: &str,
        workspace_id: &str,
        base_hash: Option<&str>,
    ) -> Result<String, Drive9Error> {
        let materialized = self.layout.session_dir(session_id).join("materialized");
        if !materialized.is_dir() {
            return Err(Drive9Error::Layout(format!(
                "Session '{}' has not been materialized",
                session_id
            )));
        }
        // A build that crashed while holding the host `.vam/` aside.
        crate::disk::restore_held_vam(&materialized);
        let head = self.manifest_manager.read_head(workspace_id)?;
        let base_manifest = match base_hash.map(str::to_string).or_else(|| head.clone()) {
            Some(hash) => Some(self.manifest_manager.get_manifest(workspace_id, &hash)?),
            None => None,
        };
        let manifest = self.manifest_manager.compose_with_host_dir(
            workspace_id,
            base_manifest.as_ref(),
            &materialized,
            VAM_DIR,
            head,
        )?;
        let manifest_hash = self.manifest_manager.commit_manifest(&manifest)?;
        self.manifest_manager
            .write_head(workspace_id, &manifest_hash)?;
        let chunks: Vec<String> = manifest
            .files
            .iter()
            .flat_map(|f| f.chunks.iter().cloned())
            .collect();
        self.cache_manager.pin_chunks(&chunks)?;
        Ok(manifest_hash)
    }

    /// Moves a session tree out of `sessions/` into `kept/` (never swept by
    /// `gc`) and releases the workspace lock. Used when a commit fails, so the
    /// work survives. Returns the kept id: the session id, with a numeric
    /// suffix if that name is taken.
    pub fn keep_session(
        &self,
        session_id: &str,
        workspace_id: &str,
    ) -> Result<String, Drive9Error> {
        self.with_session_lock(session_id, || {
            self.keep_session_locked(session_id, workspace_id)
        })
    }

    fn keep_session_locked(
        &self,
        session_id: &str,
        workspace_id: &str,
    ) -> Result<String, Drive9Error> {
        if session_id.is_empty()
            || session_id == "."
            || session_id == ".."
            || session_id.contains(['/', '\\', '\0'])
        {
            return Err(Drive9Error::Layout(format!(
                "session id '{session_id}' cannot name a kept tree"
            )));
        }
        let source = self.layout.session_dir(session_id);
        if !source.is_dir() {
            return Err(Drive9Error::NotFound(format!(
                "session tree for '{session_id}'"
            )));
        }
        let kept_root = self.layout.kept_dir();
        io_at(
            fs::create_dir_all(&kept_root),
            "keep.create_kept_dir",
            &kept_root,
        )?;
        let mut kept_id = session_id.to_string();
        let mut n = 1u32;
        while self.layout.kept_session_dir(&kept_id).exists() {
            kept_id = format!("{session_id}-{n}");
            n += 1;
        }
        let target = self.layout.kept_session_dir(&kept_id);
        io_at(fs::rename(&source, &target), "keep.rename_tree", &target)?;
        // The tree is safe; the workspace can be locked again.
        let _ = self.release_lock(workspace_id, session_id);
        Ok(kept_id)
    }

    /// Discards session assets and releases lease lock.
    pub fn discard(&self, session_id: &str, workspace_id: &str) -> Result<(), Drive9Error> {
        let session_dir = self.layout.session_dir(session_id);
        self.with_session_lock(session_id, || {
            if session_dir.exists() {
                if self.backend == Drive9Backend::Directory {
                    make_writable(&session_dir);
                }
                let _ = fs::remove_dir_all(&session_dir);
            }
        });
        self.release_lock(workspace_id, session_id)?;
        Ok(())
    }

    /// Garbage collects orphaned session folders and sweeps unreferenced CAS chunks.
    ///
    /// Chunk refcounts span every workspace manifest (including host-prefixed ids).
    pub fn gc(&self) -> Result<(), Drive9Error> {
        let sessions_dir = self.layout.sessions_dir();
        if sessions_dir.exists() {
            for entry in fs::read_dir(sessions_dir)? {
                let entry = entry?;
                let path = entry.path();
                if path.is_dir() {
                    // Orphaned sessions (inactive session directories) are cleaned
                    if self.backend == Drive9Backend::Directory {
                        make_writable(&path);
                    }
                    let _ = fs::remove_dir_all(path);
                }
            }
        }
        let _ = crate::gc::sweep_unreferenced_chunks(&self.layout)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::CloneStrategy;
    use crate::config::{Drive9Config, Drive9Layout};
    use crate::disk::MockImageIo;
    use crate::vam_dir::{
        append_turn, ensure_workspace_json, read_timeline, TIMELINE_JSONL, WORKSPACE_JSON_PATH,
    };
    use std::path::Path;
    use tempfile::tempdir;

    #[test]
    fn copy_dir_all_replaces_read_only_and_hard_linked_files() {
        // A skill restored from the manifest is a 0444 hard link into the chunk cache. The catalog
        // overlay copy must replace it without writing through (EACCES for a non-root user, a
        // corrupted cache file for anyone). Root ignores the mode, so this only failed on CI.
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let cache = dir.path().join("cache-file");
        fs::write(&cache, "old").unwrap();
        let src = dir.path().join("src/hosted-x");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("manifest.json"), "new").unwrap();
        let dst = dir.path().join("dst/hosted-x");
        fs::create_dir_all(&dst).unwrap();
        fs::hard_link(&cache, dst.join("manifest.json")).unwrap();
        fs::set_permissions(&cache, fs::Permissions::from_mode(0o444)).unwrap();

        copy_dir_all(dir.path().join("src"), dir.path().join("dst")).unwrap();
        assert_eq!(
            fs::read_to_string(dst.join("manifest.json")).unwrap(),
            "new"
        );
        assert_eq!(
            fs::read_to_string(&cache).unwrap(),
            "old",
            "cache file untouched"
        );
    }

    #[test]
    fn with_skills_writable_unseals_and_reseals() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let skills = dir.path().join("skills");
        fs::create_dir_all(skills.join("s")).unwrap();
        fs::write(skills.join("s/manifest.json"), "a").unwrap();
        make_read_only(&skills).unwrap();
        with_skills_writable(&skills, || {
            fs::write(skills.join("s/manifest.json"), "b").unwrap();
            fs::write(skills.join("s/extra.json"), "c").unwrap();
        });
        assert_eq!(
            fs::read_to_string(skills.join("s/manifest.json")).unwrap(),
            "b"
        );
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o222;
        assert_eq!(mode(&skills), 0, "sealed again");
        assert_eq!(mode(&skills.join("s/extra.json")), 0);
        // An unsealed dir stays unsealed.
        make_writable(&skills);
        with_skills_writable(&skills, || ());
        assert_ne!(mode(&skills), 0);
    }

    #[test]
    fn test_vam_timeline_survives_commit_cycle() {
        let dir = tempdir().unwrap();
        let config = Drive9Config::new(dir.path());
        let layout = config.layout();
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

        let profile = StorageProfile::small_for_test();
        let workspace_id = "ws-thread";
        let thread = "main";

        let mat_path = manager
            .materialize("sess-1", workspace_id, &profile, None, None, None)
            .unwrap();
        ensure_workspace_json(&mat_path, workspace_id, None).unwrap();
        append_turn(
            &mat_path,
            thread,
            "run-1",
            "write a note",
            "wrote result.txt",
            "openai/gpt-4o-mini",
            "pre-commit",
            None,
            None,
        )
        .unwrap();

        let manifest_hash = manager.commit("sess-1", workspace_id).unwrap();
        assert!(!manifest_hash.is_empty());

        let manifest = ManifestManager::new(layout.clone(), chunk_store.clone())
            .get_manifest(workspace_id, &manifest_hash)
            .unwrap();
        assert!(
            manifest
                .files
                .iter()
                .any(|f| f.path.ends_with(TIMELINE_JSONL)),
            "commit manifest should include timeline.jsonl"
        );
        assert!(
            manifest.files.iter().any(|f| f.path == WORKSPACE_JSON_PATH),
            "commit manifest should include workspace.json"
        );

        manager.discard("sess-1", workspace_id).unwrap();

        let mat_path_2 = manager
            .materialize("sess-2", workspace_id, &profile, None, None, None)
            .unwrap();
        let timeline = read_timeline(&mat_path_2, thread).unwrap();
        assert_eq!(timeline.len(), 1);
        assert_eq!(timeline[0].prompt, "write a note");
        assert_eq!(timeline[0].answer, "wrote result.txt");
    }

    #[test]
    fn test_workspace_manager_lifecycle() {
        let dir = tempdir().unwrap();
        let config = Drive9Config::new(dir.path());
        let layout = config.layout();
        let chunk_store = ChunkStore::new(layout.clone());
        let manifest_manager = ManifestManager::new(layout.clone(), chunk_store.clone());
        let cache_manager =
            CacheManager::new(layout.clone(), chunk_store.clone(), 10 * 1024 * 1024).unwrap();
        let image_io = Box::new(MockImageIo);

        let manager = WorkspaceManager::new(
            layout,
            chunk_store,
            manifest_manager,
            cache_manager,
            image_io,
        );

        let profile = StorageProfile::small_for_test();

        // 1. Materialize empty workspace
        let mat_path = manager
            .materialize("sess-1", "ws-1", &profile, None, None, None)
            .unwrap();
        assert!(mat_path.exists());
        assert!(mat_path.join("SKILLS.md").exists());
        assert!(mat_path.join("workspace/.vam/SYSTEM.md").exists());
        assert!(mat_path.join("workspace/PLAN.md").exists());

        // 2. Commit changes
        let hash = manager.commit("sess-1", "ws-1").unwrap();
        assert!(!hash.is_empty());

        // 3. Discard
        manager.discard("sess-1", "ws-1").unwrap();
        assert!(!manager.layout.session_dir("sess-1").exists());
        assert!(manager.lock_holder("ws-1").unwrap().is_none());
    }

    #[test]
    fn test_workspace_single_writer_lock() {
        let dir = tempdir().unwrap();
        let config = Drive9Config::new(dir.path());
        let layout = config.layout();
        let chunk_store = ChunkStore::new(layout.clone());
        let manifest_manager = ManifestManager::new(layout.clone(), chunk_store.clone());
        let cache_manager =
            CacheManager::new(layout.clone(), chunk_store.clone(), 10 * 1024 * 1024).unwrap();
        let manager = WorkspaceManager::new(
            layout.clone(),
            chunk_store,
            manifest_manager,
            cache_manager,
            Box::new(MockImageIo),
        );

        manager.acquire_lock("ws-1", "sess-1").unwrap();
        assert_eq!(
            manager.lock_holder("ws-1").unwrap().as_deref(),
            Some("sess-1")
        );

        let err = manager.acquire_lock("ws-1", "sess-2").unwrap_err();
        assert!(format!("{}", err).contains("already locked by session 'sess-1'"));

        // Same session may re-acquire.
        manager.acquire_lock("ws-1", "sess-1").unwrap();

        manager.release_lock("ws-1", "sess-1").unwrap();
        assert!(manager.lock_holder("ws-1").unwrap().is_none());
        manager.acquire_lock("ws-1", "sess-2").unwrap();
        assert_eq!(
            manager.lock_holder("ws-1").unwrap().as_deref(),
            Some("sess-2")
        );
    }

    #[test]
    fn prefixed_workspace_ids_isolate_volumes_and_locks() {
        let dir = tempdir().unwrap();
        let layout = Drive9Layout::new(dir.path());
        let alice_ws = crate::apply_owner_prefix("alice", "assistant").unwrap();
        let bob_ws = crate::apply_owner_prefix("bob", "assistant").unwrap();
        assert_ne!(alice_ws, bob_ws);
        assert_ne!(
            layout.workspace_dir(&alice_ws),
            layout.workspace_dir(&bob_ws)
        );

        let chunk_store = ChunkStore::new(layout.clone());
        let manifest_manager = ManifestManager::new(layout.clone(), chunk_store.clone());
        let cache_manager =
            CacheManager::new(layout.clone(), chunk_store.clone(), 10 * 1024 * 1024).unwrap();
        let manager = WorkspaceManager::new(
            layout,
            chunk_store,
            manifest_manager,
            cache_manager,
            Box::new(MockImageIo),
        );

        manager.acquire_lock(&alice_ws, "sess-alice").unwrap();
        manager.acquire_lock(&bob_ws, "sess-bob").unwrap();
        assert_eq!(
            manager.lock_holder(&alice_ws).unwrap().as_deref(),
            Some("sess-alice")
        );
        assert_eq!(
            manager.lock_holder(&bob_ws).unwrap().as_deref(),
            Some("sess-bob")
        );

        let err = manager.acquire_lock(&bob_ws, "sess-alice").unwrap_err();
        assert!(
            format!("{err}").contains("already locked by session 'sess-bob'"),
            "{err}"
        );
        assert_eq!(
            crate::apply_owner_prefix("alice", &bob_ws).unwrap(),
            alice_ws
        );
    }

    #[test]
    fn test_release_stale_workspace_lock() {
        let dir = tempdir().unwrap();
        let layout = Drive9Layout::new(dir.path());
        fs::create_dir_all(layout.workspace_dir("ws-1")).unwrap();
        fs::write(
            layout.workspace_dir("ws-1").join("write.lock"),
            "dead-sess\n",
        )
        .unwrap();

        assert_eq!(
            read_workspace_lock_holder(&layout, "ws-1")
                .unwrap()
                .as_deref(),
            Some("dead-sess")
        );
        release_workspace_lock(&layout, "ws-1", Some("other-sess")).unwrap();
        assert!(read_workspace_lock_holder(&layout, "ws-1")
            .unwrap()
            .is_some());

        release_workspace_lock(&layout, "ws-1", Some("dead-sess")).unwrap();
        assert!(read_workspace_lock_holder(&layout, "ws-1")
            .unwrap()
            .is_none());
    }

    /// Mock extract that simulates guest-written `workspace/notes/hello.txt` on the ext4 image.
    #[derive(Debug, Clone)]
    struct HelloNoteExtractMock;

    impl Drive9ImageIo for HelloNoteExtractMock {
        fn build_image(
            &self,
            source_dir: &Path,
            image_path: &Path,
            size_bytes: u64,
        ) -> Result<(), Drive9Error> {
            MockImageIo.build_image(source_dir, image_path, size_bytes)
        }

        fn walk_image(
            &self,
            _image_path: &Path,
            _limits: &ImageLimits,
            visitor: &mut dyn ImageVisitor,
        ) -> Result<WalkReport, Drive9Error> {
            use crate::image_read::ImageEntry;
            let files: [(&str, &[u8]); 2] = [
                ("workspace/notes/hello.txt", b"hello from guest"),
                ("SKILLS.md", b"# Workspace Capability Instructions\n"),
            ];
            let mut report = WalkReport::default();
            for (path, body) in files {
                let entry = ImageEntry {
                    path: path.to_string(),
                    kind: ImageEntryKind::File,
                    size: body.len() as u64,
                    mode: 0o644,
                };
                let mut content: &[u8] = body;
                visitor.visit(&entry, Some(&mut content))?;
                report.files += 1;
            }
            Ok(report)
        }

        fn overlay_dir(
            &self,
            _image_path: &Path,
            _host_dir: &Path,
            _guest_prefix: &str,
        ) -> Result<(), Drive9Error> {
            Ok(())
        }
    }

    #[test]
    fn commit_manifest_includes_guest_written_note() {
        let dir = tempdir().unwrap();
        let config = Drive9Config::new(dir.path());
        let layout = config.layout();
        let chunk_store = ChunkStore::new(layout.clone());
        let cache_manager =
            CacheManager::new(layout.clone(), chunk_store.clone(), 10 * 1024 * 1024).unwrap();
        let manager = WorkspaceManager::new(
            layout.clone(),
            chunk_store.clone(),
            ManifestManager::new(layout.clone(), chunk_store.clone()),
            cache_manager,
            Box::new(HelloNoteExtractMock),
        );

        let profile = StorageProfile::small_for_test();
        manager
            .materialize("sess-note", "ws-note", &profile, None, None, None)
            .unwrap();

        let manifest_hash = manager.commit("sess-note", "ws-note").unwrap();
        let verify_manager = ManifestManager::new(layout.clone(), chunk_store.clone());
        let manifest = verify_manager
            .get_manifest("ws-note", &manifest_hash)
            .unwrap();
        assert!(
            manifest
                .files
                .iter()
                .any(|f| f.path == "workspace/notes/hello.txt"),
            "commit manifest should list workspace/notes/hello.txt, got: {:?}",
            manifest.files.iter().map(|f| &f.path).collect::<Vec<_>>()
        );
    }

    fn mke2fs_available() -> bool {
        std::process::Command::new("mke2fs")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    fn mock_manager(dir: &Path) -> WorkspaceManager {
        let config = Drive9Config::new(dir);
        let layout = config.layout();
        let chunk_store = ChunkStore::new(layout.clone());
        let manifest_manager = ManifestManager::new(layout.clone(), chunk_store.clone());
        let cache_manager =
            CacheManager::new(layout.clone(), chunk_store.clone(), 10 * 1024 * 1024).unwrap();
        WorkspaceManager::new(
            layout,
            chunk_store,
            manifest_manager,
            cache_manager,
            Box::new(MockImageIo),
        )
    }

    fn small_profile() -> StorageProfile {
        StorageProfile::new("test".to_string(), 64 * 1024)
    }

    fn timing_has(steps: &[(&'static str, u128)], label: &str) -> bool {
        steps.iter().any(|(name, _)| *name == label)
    }

    fn e2fs_manager(dir: &Path) -> WorkspaceManager {
        let config = Drive9Config::new(dir);
        let layout = config.layout();
        let chunk_store = ChunkStore::new(layout.clone());
        let manifest_manager = ManifestManager::new(layout.clone(), chunk_store.clone());
        let cache_manager =
            CacheManager::new(layout.clone(), chunk_store.clone(), 10 * 1024 * 1024).unwrap();
        WorkspaceManager::new(
            layout,
            chunk_store,
            manifest_manager,
            cache_manager,
            Box::new(crate::disk::E2fsImageIo::new()),
        )
    }

    #[test]
    fn test_template_miss_then_hit_uses_clone() {
        if !mke2fs_available() {
            eprintln!("skipping test_template_miss_then_hit_uses_clone: mke2fs not found");
            return;
        }

        let dir = tempdir().unwrap();
        let manager = e2fs_manager(dir.path());
        let profile = StorageProfile::small_for_test();
        let workspace_id = "ws-template-hit";

        let t0 = std::time::Instant::now();
        manager
            .materialize("sess-a", workspace_id, &profile, None, None, None)
            .unwrap();
        let first_us = t0.elapsed().as_micros();

        let template_path = manager
            .layout()
            .template_path(workspace_id, "empty", "none");
        assert!(template_path.is_file(), "miss should install template");

        manager.discard("sess-a", workspace_id).unwrap();

        let t1 = std::time::Instant::now();
        manager
            .materialize("sess-b", workspace_id, &profile, None, None, None)
            .unwrap();
        let second_us = t1.elapsed().as_micros();

        let _img_a = manager.layout().session_image_path("sess-a");
        let img_b = manager.layout().session_image_path("sess-b");
        assert!(img_b.is_file());

        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if _img_a.is_file() {
                assert_ne!(
                    fs::metadata(&_img_a).unwrap().ino(),
                    fs::metadata(&img_b).unwrap().ino(),
                    "session images must not share inodes"
                );
            }
        }

        assert!(
            second_us <= first_us.saturating_mul(2).max(1),
            "second materialize ({second_us} µs) should not be slower than 2× first ({first_us} µs)"
        );

        manager.discard("sess-b", workspace_id).unwrap();
    }

    #[test]
    fn test_commit_refreshes_template_at_new_head() {
        if !mke2fs_available() {
            eprintln!("skipping test_commit_refreshes_template_at_new_head: mke2fs not found");
            return;
        }

        let dir = tempdir().unwrap();
        let manager = e2fs_manager(dir.path());
        let profile = StorageProfile::small_for_test();
        let workspace_id = "ws-template-commit";

        manager
            .materialize("sess-1", workspace_id, &profile, None, None, None)
            .unwrap();
        let old_template = manager
            .layout()
            .template_path(workspace_id, "empty", "none");
        assert!(old_template.is_file());

        let manifest_hash = manager.commit("sess-1", workspace_id).unwrap();
        let new_template = manager
            .layout()
            .template_path(workspace_id, &manifest_hash, "none");
        assert!(
            new_template.is_file(),
            "commit should refresh template at new HEAD"
        );

        manager.discard("sess-1", workspace_id).unwrap();

        manager
            .materialize("sess-2", workspace_id, &profile, None, None, None)
            .unwrap();
        let img = manager.layout().session_image_path("sess-2");
        assert!(img.is_file());
        manager.discard("sess-2", workspace_id).unwrap();
    }

    #[test]
    fn alice_golden_template_is_not_reused_as_bobs() {
        let dir = tempdir().unwrap();
        let manager = mock_manager(dir.path());
        let profile = small_profile();
        let alice_ws = crate::apply_owner_prefix("alice", "assistant").unwrap();
        let bob_ws = crate::apply_owner_prefix("bob", "assistant").unwrap();
        let alice_template = manager.layout().template_path(&alice_ws, "empty", "none");
        let bob_template = manager.layout().template_path(&bob_ws, "empty", "none");
        assert_ne!(alice_template, bob_template);

        let mut alice_steps = Vec::new();
        manager
            .materialize(
                "sess-alice",
                &alice_ws,
                &profile,
                None,
                None,
                Some(&mut alice_steps),
            )
            .unwrap();
        assert!(
            timing_has(&alice_steps, "materialize_build_image"),
            "alice first session should miss: {alice_steps:?}"
        );
        assert!(alice_template.is_file());
        fs::write(&alice_template, b"alice-only-golden").unwrap();

        let mut bob_steps = Vec::new();
        manager
            .materialize(
                "sess-bob",
                &bob_ws,
                &profile,
                None,
                None,
                Some(&mut bob_steps),
            )
            .unwrap();
        assert!(
            timing_has(&bob_steps, "materialize_build_image"),
            "bob must miss alice's template: {bob_steps:?}"
        );
        assert!(!timing_has(&bob_steps, "materialize_clone_image"));
        assert!(bob_template.is_file());
        assert_ne!(
            fs::read(&alice_template).unwrap(),
            fs::read(&bob_template).unwrap(),
            "bob's rebake must not overwrite or reuse alice's golden"
        );
        assert_eq!(
            fs::read_to_string(&alice_template).unwrap(),
            "alice-only-golden"
        );
        let bob_image = manager.layout().session_image_path("sess-bob");
        assert_ne!(
            fs::read(&bob_image).unwrap(),
            b"alice-only-golden",
            "bob's session image must not be cloned from alice's golden"
        );

        manager.discard("sess-alice", &alice_ws).unwrap();
        manager.discard("sess-bob", &bob_ws).unwrap();
    }

    #[test]
    fn first_prefixed_session_misses_unprefixed_template_then_hits() {
        let dir = tempdir().unwrap();
        let manager = mock_manager(dir.path());
        let profile = small_profile();
        let prefixed = crate::apply_owner_prefix("local", "assistant").unwrap();
        let unprefixed_template = manager.layout().template_path("assistant", "empty", "none");
        let prefixed_template = manager.layout().template_path(&prefixed, "empty", "none");

        fs::create_dir_all(unprefixed_template.parent().unwrap()).unwrap();
        fs::write(&unprefixed_template, b"legacy-unprefixed-golden").unwrap();
        assert!(!prefixed_template.exists());

        let mut first = Vec::new();
        manager
            .materialize(
                "sess-prefix-1",
                &prefixed,
                &profile,
                None,
                None,
                Some(&mut first),
            )
            .unwrap();
        assert!(
            timing_has(&first, "materialize_build_image"),
            "first prefixed session must miss the unprefixed golden: {first:?}"
        );
        assert!(!timing_has(&first, "materialize_clone_image"));
        assert!(
            prefixed_template.is_file(),
            "miss should re-bake under the prefixed workspace"
        );
        assert_eq!(
            fs::read_to_string(&unprefixed_template).unwrap(),
            "legacy-unprefixed-golden",
            "unprefixed golden must stay unused"
        );
        let first_image = manager.layout().session_image_path("sess-prefix-1");
        assert_ne!(fs::read(&first_image).unwrap(), b"legacy-unprefixed-golden");

        manager.discard("sess-prefix-1", &prefixed).unwrap();

        let mut second = Vec::new();
        manager
            .materialize(
                "sess-prefix-2",
                &prefixed,
                &profile,
                None,
                None,
                Some(&mut second),
            )
            .unwrap();
        assert!(
            timing_has(&second, "materialize_clone_image"),
            "later prefixed session should hit the rebaked golden: {second:?}"
        );
        assert!(!timing_has(&second, "materialize_build_image"));
        assert_eq!(
            fs::read_to_string(&unprefixed_template).unwrap(),
            "legacy-unprefixed-golden"
        );

        manager.discard("sess-prefix-2", &prefixed).unwrap();
    }

    #[test]
    fn test_skills_digest_change_misses_template() {
        let dir = tempdir().unwrap();
        let layout = Drive9Layout::new(dir.path());
        let seed_a = dir.path().join("seed-a");
        let seed_b = dir.path().join("seed-b");
        fs::create_dir_all(seed_a.join("skills/tool-a")).unwrap();
        fs::create_dir_all(seed_b.join("skills/tool-b")).unwrap();
        fs::write(seed_a.join("skills/tool-a/run.js"), b"v1").unwrap();
        fs::write(seed_b.join("skills/tool-b/run.js"), b"v2").unwrap();

        let digest_a = skills_digest(Some(&seed_a));
        let digest_b = skills_digest(Some(&seed_b));
        assert_ne!(digest_a, digest_b);

        let path_a = layout.template_path("ws-skills", "empty", &digest_a);
        let path_b = layout.template_path("ws-skills", "empty", &digest_b);
        assert_ne!(path_a, path_b);
    }

    #[test]
    fn test_materialize_catalog_overlay_wins() {
        let dir = tempdir().unwrap();
        let config = Drive9Config::new(dir.path());
        let layout = config.layout();
        let chunk_store = ChunkStore::new(layout.clone());
        let manifest_manager = ManifestManager::new(layout.clone(), chunk_store.clone());
        let cache_manager =
            CacheManager::new(layout.clone(), chunk_store.clone(), 10 * 1024 * 1024).unwrap();
        let manager = WorkspaceManager::new(
            layout.clone(),
            chunk_store,
            manifest_manager,
            cache_manager,
            Box::new(MockImageIo),
        );

        let seed = dir.path().join("seed");
        fs::create_dir_all(seed.join("skills/demo-tool")).unwrap();
        fs::write(seed.join("skills/demo-tool/run.js"), b"seed").unwrap();

        let catalog = dir.path().join("catalog-skills");
        fs::create_dir_all(catalog.join("demo-tool")).unwrap();
        fs::write(catalog.join("demo-tool/run.js"), b"catalog").unwrap();

        let profile = StorageProfile::small_for_test();
        let mat_path = manager
            .materialize(
                "sess-overlay",
                "ws-overlay",
                &profile,
                Some(&seed),
                Some(&catalog),
                None,
            )
            .unwrap();

        let run_js = mat_path.join("skills/demo-tool/run.js");
        assert_eq!(fs::read_to_string(run_js).unwrap(), "catalog");

        let seed_only_digest = skills_digest(Some(&seed));
        let overlay_digest = skills_digest_from_dir(&mat_path.join("skills"));
        assert_ne!(seed_only_digest, overlay_digest);

        manager.discard("sess-overlay", "ws-overlay").unwrap();
    }

    #[test]
    fn test_clone_image_copy_fallback() {
        let dir = tempdir().unwrap();
        let layout = Drive9Layout::new(dir.path());
        let chunk_store = ChunkStore::new(layout.clone());
        let cache = CacheManager::with_strategy_for_test(
            layout,
            chunk_store,
            10 * 1024 * 1024,
            CloneStrategy::Copy,
        )
        .unwrap();

        let src = dir.path().join("template.ext4");
        let dst = dir.path().join("session.ext4");
        fs::write(&src, vec![0u8; 4096]).unwrap();
        cache.clone_image(&src, &dst).unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert_ne!(
                fs::metadata(&src).unwrap().ino(),
                fs::metadata(&dst).unwrap().ino()
            );
        }

        fs::write(&dst, b"modified session image").unwrap();
        let src_len = fs::metadata(&src).unwrap().len();
        assert_eq!(
            src_len, 4096,
            "copy fallback must leave template independent"
        );
    }

    /// Without reflink a clone of a sparse workspace image must stay sparse:
    /// a default image is 1 GiB of holes around a few MiB of data.
    #[cfg(unix)]
    #[test]
    fn a_copy_clone_of_a_sparse_image_stays_sparse() {
        use std::io::{Seek, SeekFrom, Write};
        use std::os::unix::fs::MetadataExt;

        let dir = tempdir().unwrap();
        let layout = Drive9Layout::new(dir.path());
        let chunk_store = ChunkStore::new(layout.clone());
        let cache = CacheManager::with_strategy_for_test(
            layout,
            chunk_store,
            10 * 1024 * 1024,
            CloneStrategy::Copy,
        )
        .unwrap();
        let src = dir.path().join("template.ext4");
        let dst = dir.path().join("nested/session.ext4");
        let len: u64 = 16 * 1024 * 1024;
        {
            let mut f = fs::File::create(&src).unwrap();
            f.set_len(len).unwrap();
            f.write_all(b"superblock").unwrap();
            f.seek(SeekFrom::Start(len - 10)).unwrap();
            f.write_all(b"last bytes").unwrap();
        }
        cache.clone_image(&src, &dst).unwrap();
        let copy = fs::metadata(&dst).unwrap();
        assert_eq!(copy.len(), len);
        assert!(
            copy.blocks() * 512 <= fs::metadata(&src).unwrap().blocks() * 512 + 64 * 1024,
            "the clone allocated {} bytes",
            copy.blocks() * 512
        );
        assert_eq!(fs::read(&src).unwrap(), fs::read(&dst).unwrap());
    }
}

#[cfg(test)]
mod directory_backend_tests {
    use super::*;
    use crate::cas::ChunkStore;
    use crate::config::{Drive9Config, StorageProfile};
    use crate::manifest::ManifestManager;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;

    /// Any image IO call fails the test: the directory backend must not use it.
    #[derive(Debug)]
    struct PanicImageIo;
    impl Drive9ImageIo for PanicImageIo {
        fn build_image(&self, _: &Path, _: &Path, _: u64) -> Result<(), Drive9Error> {
            panic!("build_image called in directory mode")
        }
        fn walk_image(
            &self,
            _: &Path,
            _: &ImageLimits,
            _: &mut dyn ImageVisitor,
        ) -> Result<WalkReport, Drive9Error> {
            panic!("walk_image called in directory mode")
        }
        fn overlay_dir(&self, _: &Path, _: &Path, _: &str) -> Result<(), Drive9Error> {
            panic!("overlay_dir called in directory mode")
        }
    }

    fn manager(root: &Path, backend: Drive9Backend) -> WorkspaceManager {
        let layout = Drive9Config::new(root).layout();
        let chunks = ChunkStore::new(layout.clone());
        let manifests = ManifestManager::new(layout.clone(), chunks.clone());
        let cache = CacheManager::new(layout.clone(), chunks.clone(), 10 * 1024 * 1024).unwrap();
        WorkspaceManager::new(layout, chunks, manifests, cache, Box::new(PanicImageIo))
            .with_backend(backend)
    }

    fn seed_with_skill(dir: &Path) -> PathBuf {
        let seed = dir.join("seed");
        fs::create_dir_all(seed.join("skills/demo")).unwrap();
        fs::write(seed.join("skills/demo/run.js"), b"x").unwrap();
        seed
    }

    #[test]
    fn default_backend_is_ext4() {
        let d = tempdir().unwrap();
        assert_eq!(
            manager(d.path(), Drive9Backend::Ext4Image).backend(),
            Drive9Backend::Ext4Image
        );
        assert_eq!(Drive9Backend::default(), Drive9Backend::Ext4Image);
        let m = open_workspace_manager(d.path()).unwrap();
        assert_eq!(m.backend(), Drive9Backend::Ext4Image);
    }

    #[test]
    fn directory_materialize_builds_no_image_and_commits_the_directory() {
        let d = tempdir().unwrap();
        let m = manager(d.path(), Drive9Backend::Directory);
        let profile = StorageProfile::small_for_test();
        let mat = m
            .materialize("s1", "ws", &profile, None, None, None)
            .unwrap();
        assert!(!m.layout().session_image_path("s1").exists());
        // the guest writes into the directory
        fs::write(mat.join("workspace/note.txt"), b"hello").unwrap();
        let head = m.commit_and_release("s1", "ws").unwrap();
        assert!(!head.is_empty());
        assert!(
            !m.layout().session_image_path("s1").exists(),
            "no template image either"
        );
        // a second session sees the file
        let mat2 = m
            .materialize("s2", "ws", &profile, None, None, None)
            .unwrap();
        assert_eq!(fs::read(mat2.join("workspace/note.txt")).unwrap(), b"hello");
        m.discard("s2", "ws").unwrap();
    }

    #[test]
    fn directory_skills_are_read_only_and_discard_still_works() {
        let d = tempdir().unwrap();
        let seed = seed_with_skill(d.path());
        let m = manager(d.path(), Drive9Backend::Directory);
        let profile = StorageProfile::small_for_test();
        let mat = m
            .materialize("s1", "ws", &profile, Some(&seed), None, None)
            .unwrap();
        let file = mat.join("skills/demo/run.js");
        assert!(file.is_file());
        assert_eq!(fs::metadata(&file).unwrap().permissions().mode() & 0o222, 0);
        assert_eq!(
            fs::metadata(mat.join("skills"))
                .unwrap()
                .permissions()
                .mode()
                & 0o222,
            0
        );
        // workspace stays writable
        assert_ne!(
            fs::metadata(mat.join("workspace"))
                .unwrap()
                .permissions()
                .mode()
                & 0o200,
            0
        );
        // commit reads the read-only tree
        m.commit("s1", "ws").unwrap();
        // re-materializing the same session overlays skills again without error
        m.materialize("s1", "ws", &profile, Some(&seed), None, None)
            .unwrap();
        m.discard("s1", "ws").unwrap();
        assert!(!m.layout().session_dir("s1").exists());
    }

    #[test]
    fn ext4_backend_still_builds_an_image() {
        let d = tempdir().unwrap();
        let layout = Drive9Config::new(d.path()).layout();
        let chunks = ChunkStore::new(layout.clone());
        let manifests = ManifestManager::new(layout.clone(), chunks.clone());
        let cache = CacheManager::new(layout.clone(), chunks.clone(), 10 * 1024 * 1024).unwrap();
        let m = WorkspaceManager::new(
            layout,
            chunks,
            manifests,
            cache,
            Box::new(crate::disk::MockImageIo),
        );
        m.materialize(
            "s1",
            "ws",
            &StorageProfile::small_for_test(),
            None,
            None,
            None,
        )
        .unwrap();
        assert!(m.layout().session_image_path("s1").exists());
    }

    fn dir_manager_with_session(root: &Path) -> (WorkspaceManager, StorageProfile) {
        let m = manager(root, Drive9Backend::Directory);
        let profile = StorageProfile::small_for_test();
        m.materialize("s1", "ws", &profile, None, None, None)
            .unwrap();
        (m, profile)
    }

    fn read_head_files(m: &WorkspaceManager, ws: &str) -> Vec<String> {
        let head = m.manifest_manager.read_head(ws).unwrap().unwrap();
        let manifest = m.manifest_manager.get_manifest(ws, &head).unwrap();
        manifest.files.into_iter().map(|f| f.path).collect()
    }

    #[test]
    fn a_tree_snapshot_leaves_out_the_host_vam_dir_and_does_not_move_head() {
        let d = tempdir().unwrap();
        let (m, _) = dir_manager_with_session(d.path());
        let mat = m.layout().session_dir("s1").join("materialized");
        fs::write(mat.join("workspace/a.txt"), b"a").unwrap();
        fs::create_dir_all(mat.join(".vam/threads/t")).unwrap();
        fs::write(mat.join(".vam/threads/t/log.jsonl"), b"{}").unwrap();
        let hash = m.snapshot_tree_manifest("s1", "ws").unwrap();
        let manifest = m.manifest_manager.get_manifest("ws", &hash).unwrap();
        let paths: Vec<&str> = manifest.files.iter().map(|f| f.path.as_str()).collect();
        assert!(paths.contains(&"workspace/a.txt"));
        assert!(paths.iter().all(|p| !p.starts_with(".vam/")), "{paths:?}");
        assert!(m.manifest_manager.read_head("ws").unwrap().is_none());
        m.discard("s1", "ws").unwrap();
    }

    #[test]
    fn commit_at_manifest_keeps_the_conversation_and_drops_later_files() {
        let d = tempdir().unwrap();
        let (m, profile) = dir_manager_with_session(d.path());
        let mat = m.layout().session_dir("s1").join("materialized");
        fs::write(mat.join("workspace/note.txt"), b"kiwi").unwrap();
        let turn = m.snapshot_tree_manifest("s1", "ws").unwrap();
        // work after the turn commit, and a conversation record
        fs::write(mat.join("workspace/partial.txt"), b"half").unwrap();
        fs::create_dir_all(mat.join(".vam/threads/t/log")).unwrap();
        fs::write(mat.join(".vam/threads/t/log/00000001.jsonl"), b"{}\n").unwrap();
        let head = m.commit_at_manifest("s1", "ws", Some(&turn)).unwrap();
        assert_eq!(m.manifest_manager.read_head("ws").unwrap(), Some(head));
        let paths = read_head_files(&m, "ws");
        assert!(paths.contains(&"workspace/note.txt".to_string()));
        assert!(!paths.contains(&"workspace/partial.txt".to_string()));
        assert!(paths.contains(&".vam/threads/t/log/00000001.jsonl".to_string()));
        // the lock is the caller's to release; the live tree is untouched
        assert_eq!(m.lock_holder("ws").unwrap().as_deref(), Some("s1"));
        assert!(mat.join("workspace/partial.txt").exists());
        m.release_lock("ws", "s1").unwrap();
        // the next session sees the rolled-back files and the conversation
        let mat2 = m
            .materialize("s2", "ws", &profile, None, None, None)
            .unwrap();
        assert_eq!(fs::read(mat2.join("workspace/note.txt")).unwrap(), b"kiwi");
        assert!(!mat2.join("workspace/partial.txt").exists());
        assert!(mat2.join(".vam/threads/t/log/00000001.jsonl").exists());
        m.discard("s2", "ws").unwrap();
    }

    #[test]
    fn commit_at_manifest_without_a_base_starts_from_head_and_without_head_from_nothing() {
        let d = tempdir().unwrap();
        let (m, profile) = dir_manager_with_session(d.path());
        let mat = m.layout().session_dir("s1").join("materialized");
        fs::write(mat.join("workspace/first.txt"), b"1").unwrap();
        let first = m.commit_and_release("s1", "ws").unwrap();
        m.materialize("s2", "ws", &profile, None, None, None)
            .unwrap();
        let mat2 = m.layout().session_dir("s2").join("materialized");
        fs::write(mat2.join("workspace/second.txt"), b"2").unwrap();
        let head = m.commit_at_manifest("s2", "ws", None).unwrap();
        assert_ne!(head, first);
        let paths = read_head_files(&m, "ws");
        assert!(paths.contains(&"workspace/first.txt".to_string()));
        assert!(!paths.contains(&"workspace/second.txt".to_string()));
        m.release_lock("ws", "s2").unwrap();

        // a workspace with no HEAD at all: a stop keeps just the conversation
        m.materialize("s3", "fresh", &profile, None, None, None)
            .unwrap();
        let mat3 = m.layout().session_dir("s3").join("materialized");
        fs::create_dir_all(mat3.join(".vam")).unwrap();
        fs::write(mat3.join(".vam/workspace.json"), b"{}").unwrap();
        fs::write(mat3.join("workspace/lost.txt"), b"x").unwrap();
        m.commit_at_manifest("s3", "fresh", None).unwrap();
        let paths = read_head_files(&m, "fresh");
        assert_eq!(paths, vec![".vam/workspace.json".to_string()]);
        m.release_lock("fresh", "s3").unwrap();
        let again = m
            .materialize("s4", "fresh", &profile, None, None, None)
            .unwrap();
        assert!(again.join("workspace").is_dir());
        m.discard("s4", "fresh").unwrap();
    }

    #[test]
    fn commit_at_manifest_ignores_a_symlinked_vam_dir() {
        let d = tempdir().unwrap();
        let (m, _) = dir_manager_with_session(d.path());
        let mat = m.layout().session_dir("s1").join("materialized");
        let outside = d.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("secret.txt"), b"s").unwrap();
        std::os::unix::fs::symlink(&outside, mat.join(".vam")).unwrap();
        m.commit_at_manifest("s1", "ws", None).unwrap();
        let paths = read_head_files(&m, "ws");
        assert!(paths.iter().all(|p| !p.contains("secret")), "{paths:?}");
        m.release_lock("ws", "s1").unwrap();
    }

    #[test]
    fn the_ext4_backend_refuses_tree_snapshots_and_an_unmaterialized_manifest_commit() {
        let d = tempdir().unwrap();
        let m = manager(d.path(), Drive9Backend::Ext4Image);
        assert!(m.snapshot_tree_manifest("s1", "ws").is_err());
        // Committing at a manifest is allowed on this backend now (a stop rolls
        // back to a turn copy); this session has no tree.
        let err = m.commit_at_manifest("s1", "ws", None).unwrap_err();
        assert!(
            err.to_string().contains("has not been materialized"),
            "{err}"
        );
    }

    #[test]
    fn keep_session_moves_the_tree_out_of_sessions_and_gc_leaves_it() {
        let d = tempdir().unwrap();
        let (m, _) = dir_manager_with_session(d.path());
        let mat = m.layout().session_dir("s1").join("materialized");
        fs::write(mat.join("workspace/work.txt"), b"precious").unwrap();
        let kept = m.keep_session("s1", "ws").unwrap();
        assert_eq!(kept, "s1");
        assert!(!m.layout().session_dir("s1").exists());
        let kept_file = m
            .layout()
            .kept_session_dir(&kept)
            .join("materialized/workspace/work.txt");
        assert_eq!(fs::read(&kept_file).unwrap(), b"precious");
        assert_eq!(m.lock_holder("ws").unwrap(), None);
        m.gc().unwrap();
        assert!(kept_file.exists(), "gc must not touch kept trees");
        // a second tree with the same id gets a new kept id
        m.materialize(
            "s1",
            "ws",
            &StorageProfile::small_for_test(),
            None,
            None,
            None,
        )
        .unwrap();
        let again = m.keep_session("s1", "ws").unwrap();
        assert_eq!(again, "s1-1");
        assert!(kept_file.exists());
    }

    #[test]
    fn files_hash_ignores_the_conversation_and_follows_the_files() {
        let d = tempdir().unwrap();
        let (m, _) = dir_manager_with_session(d.path());
        let mat = m.layout().session_dir("s1").join("materialized");
        fs::write(mat.join("workspace/a.txt"), b"a").unwrap();
        let first = m.prepare_end("s1", "ws", EndBase::Live).unwrap();
        // The conversation changes; the files do not.
        fs::create_dir_all(mat.join(".vam/threads/t")).unwrap();
        fs::write(mat.join(".vam/threads/t/log"), b"more and more").unwrap();
        let same = m.prepare_end("s1", "ws", EndBase::Live).unwrap();
        assert_eq!(first.files_hash, same.files_hash);
        fs::write(mat.join("workspace/a.txt"), b"b").unwrap();
        let changed = m.prepare_end("s1", "ws", EndBase::Live).unwrap();
        assert_ne!(first.files_hash, changed.files_hash);
        assert!(first.dropped.entries.is_empty());
        m.discard("s1", "ws").unwrap();
    }

    #[test]
    fn the_two_phase_end_writes_the_conversation_after_the_files_hash_is_fixed() {
        let d = tempdir().unwrap();
        let (m, profile) = dir_manager_with_session(d.path());
        let mat = m.layout().session_dir("s1").join("materialized");
        fs::write(mat.join("workspace/note.txt"), b"kiwi").unwrap();
        let prepared = m.prepare_end("s1", "ws", EndBase::Live).unwrap();
        assert!(m.manifest_manager.read_head("ws").unwrap().is_none());
        // The end of the conversation is written after the hash is fixed.
        fs::create_dir_all(mat.join(".vam/threads/t")).unwrap();
        fs::write(
            mat.join(".vam/threads/t/timeline.jsonl"),
            format!("{{\"files_hash\":\"{}\"}}\n", prepared.files_hash),
        )
        .unwrap();
        let head = m.finish_end("s1", "ws", &prepared).unwrap();
        m.release_lock("ws", "s1").unwrap();
        let manifest = m.manifest_manager.get_manifest("ws", &head).unwrap();
        // HEAD holds the files the hash was taken over, and the conversation.
        assert_eq!(manifest.files_hash(), prepared.files_hash);
        assert!(manifest
            .files
            .iter()
            .any(|f| f.path == ".vam/threads/t/timeline.jsonl"));
        // The head differs from the files-only manifest the hash came from.
        assert_ne!(Some(head.clone()), prepared.base_manifest);
        let again = m
            .materialize("s2", "ws", &profile, None, None, None)
            .unwrap();
        assert_eq!(fs::read(again.join("workspace/note.txt")).unwrap(), b"kiwi");
        m.discard("s2", "ws").unwrap();
    }

    #[test]
    fn a_stop_prepares_from_the_turn_commit_and_lists_the_work_it_drops() {
        let d = tempdir().unwrap();
        let (m, _) = dir_manager_with_session(d.path());
        let mat = m.layout().session_dir("s1").join("materialized");
        fs::write(mat.join("workspace/note.txt"), b"kiwi").unwrap();
        let turn = m.snapshot_tree_manifest("s1", "ws").unwrap();
        fs::write(mat.join("workspace/partial.txt"), b"half").unwrap();
        fs::write(mat.join("workspace/note.txt"), b"kiwi-changed").unwrap();
        std::os::unix::fs::symlink("/etc/passwd", mat.join("workspace/link")).unwrap();
        let prepared = m
            .prepare_end("s1", "ws", EndBase::TurnCommit(Some(turn.clone())))
            .unwrap();
        assert_eq!(prepared.base_manifest.as_deref(), Some(turn.as_str()));
        let names: Vec<&str> = prepared
            .dropped
            .entries
            .iter()
            .map(|e| e.path.as_str())
            .collect();
        assert!(names.contains(&"workspace/partial.txt"), "{names:?}");
        assert!(
            names.contains(&"workspace/note.txt"),
            "changed size: {names:?}"
        );
        assert!(names.contains(&"workspace/link"), "{names:?}");
        assert!(prepared.dropped.count >= 3);
        // No base at all: the empty files hash.
        let none = m
            .prepare_end("s1", "ws", EndBase::TurnCommit(None))
            .unwrap();
        assert_eq!(none.base_manifest, None);
        assert_eq!(none.files_hash, MerkleManifest::empty_files_hash());
        m.discard("s1", "ws").unwrap();
    }

    #[test]
    fn the_dropped_list_is_capped_but_counts_everything() {
        let d = tempdir().unwrap();
        let (m, _) = dir_manager_with_session(d.path());
        let mat = m.layout().session_dir("s1").join("materialized");
        for i in 0..(MAX_LISTED_DROPPED + 20) {
            fs::write(mat.join(format!("workspace/f{i:04}")), b"x").unwrap();
        }
        let prepared = m
            .prepare_end("s1", "ws", EndBase::TurnCommit(None))
            .unwrap();
        assert_eq!(prepared.dropped.entries.len(), MAX_LISTED_DROPPED);
        assert!(prepared.dropped.count >= MAX_LISTED_DROPPED + 20);
        m.discard("s1", "ws").unwrap();
    }

    #[test]
    fn a_live_end_reclaims_the_tree_once_head_reads_back() {
        let d = tempdir().unwrap();
        let (m, _) = dir_manager_with_session(d.path());
        let mat = m.layout().session_dir("s1").join("materialized");
        fs::write(mat.join("workspace/a.txt"), b"a").unwrap();
        let prepared = m.prepare_end("s1", "ws", EndBase::Live).unwrap();
        let head = m.finish_end("s1", "ws", &prepared).unwrap();
        // HEAD is not what the caller thinks: the tree stays.
        assert!(m
            .reclaim_session_tree("s1", "ws", &"0".repeat(64), None, false, None, 0)
            .is_err());
        assert!(m.layout().session_dir("s1").exists());
        let report = m
            .reclaim_session_tree(
                "s1",
                "ws",
                &head,
                prepared.base_manifest.as_deref(),
                false,
                None,
                0,
            )
            .unwrap();
        assert_eq!(report, ReclaimReport::default());
        assert!(!m.layout().session_dir("s1").exists());
        m.release_lock("ws", "s1").unwrap();
    }

    #[test]
    fn a_stop_moves_only_the_rolled_back_files_to_kept_within_the_cap() {
        let d = tempdir().unwrap();
        let (m, _) = dir_manager_with_session(d.path());
        let mat = m.layout().session_dir("s1").join("materialized");
        fs::write(mat.join("workspace/note.txt"), b"kiwi").unwrap();
        let turn = m.snapshot_tree_manifest("s1", "ws").unwrap();
        fs::write(mat.join("workspace/small.txt"), b"half").unwrap();
        fs::write(mat.join("workspace/big.bin"), vec![7u8; 5000]).unwrap();
        fs::create_dir_all(mat.join("workspace/sub")).unwrap();
        fs::write(mat.join("workspace/sub/deep.txt"), b"deep").unwrap();
        let prepared = m
            .prepare_end("s1", "ws", EndBase::TurnCommit(Some(turn.clone())))
            .unwrap();
        let head = m.finish_end("s1", "ws", &prepared).unwrap();
        let report = m
            .reclaim_session_tree("s1", "ws", &head, Some(&turn), true, None, 100)
            .unwrap();
        assert_eq!(
            report.kept_files, 2,
            "small.txt and sub/deep.txt fit the cap"
        );
        assert_eq!(report.skipped_over_cap, 1, "big.bin does not");
        assert_eq!(report.kept_bytes, 8);
        let kept = m
            .layout()
            .kept_session_dir(report.kept_id.as_deref().unwrap())
            .join("rolled-back");
        assert_eq!(fs::read(kept.join("workspace/small.txt")).unwrap(), b"half");
        assert_eq!(
            fs::read(kept.join("workspace/sub/deep.txt")).unwrap(),
            b"deep"
        );
        assert!(
            !kept.join("workspace/note.txt").exists(),
            "committed files are not copied"
        );
        assert!(!kept.join("workspace/big.bin").exists());
        assert!(!m.layout().session_dir("s1").exists());
        m.gc().unwrap();
        assert!(kept.join("workspace/small.txt").exists(), "gc leaves kept/");
        m.release_lock("ws", "s1").unwrap();
    }

    #[test]
    fn keep_session_refuses_ids_that_could_escape_and_missing_trees() {
        let d = tempdir().unwrap();
        let m = manager(d.path(), Drive9Backend::Directory);
        for bad in ["", ".", "..", "a/b", "a\\b"] {
            assert!(m.keep_session(bad, "ws").is_err(), "{bad:?}");
        }
        assert!(m.keep_session("ghost", "ws").is_err());
    }
}

/// The image backend against real ext4 images. A guest is simulated by writing
/// into the session image with `debugfs -w` (the test's own tool; the product
/// code never runs it).
#[cfg(test)]
mod ext4_image_tests {
    use super::*;
    use crate::cas::ChunkStore;
    use crate::config::{Drive9Config, StorageProfile};
    use crate::manifest::ManifestManager;
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;
    use tempfile::tempdir;

    fn tools() -> bool {
        ["mke2fs", "debugfs"].iter().all(|t| {
            Command::new(t)
                .arg("-V")
                .output()
                .map(|o| o.status.success() || !o.stderr.is_empty())
                .unwrap_or(false)
        })
    }

    macro_rules! need_tools {
        () => {
            if !tools() {
                eprintln!("skipping: mke2fs or debugfs is not installed");
                return;
            }
        };
    }

    fn manager(root: &Path) -> WorkspaceManager {
        let layout = Drive9Config::new(root).layout();
        let chunks = ChunkStore::new(layout.clone());
        let manifests = ManifestManager::new(layout.clone(), chunks.clone());
        let cache = CacheManager::new(layout.clone(), chunks.clone(), 10 * 1024 * 1024).unwrap();
        WorkspaceManager::new(
            layout,
            chunks,
            manifests,
            cache,
            Box::new(E2fsImageIo::new()),
        )
    }

    fn profile() -> StorageProfile {
        StorageProfile::new("t".into(), 16 * 1024 * 1024)
    }

    fn debugfs(image: &Path, cmd: &str) {
        let out = Command::new("debugfs")
            .args(["-w", "-R", cmd])
            .arg(image)
            .output()
            .unwrap();
        assert!(out.status.success(), "debugfs {cmd}: {:?}", out.stderr);
    }

    /// Writes `body` to `guest_path` in the image, as the guest would.
    fn guest_write(
        m: &WorkspaceManager,
        session: &str,
        scratch: &Path,
        guest_path: &str,
        body: &[u8],
    ) {
        let image = m.layout().session_image_path(session);
        let host = scratch.join(format!("g{}", guest_path.replace('/', "_")));
        fs::write(&host, body).unwrap();
        if let Some((dir, _)) = guest_path.rsplit_once('/') {
            let mut cur = String::new();
            for part in dir.split('/') {
                cur = format!("{cur}/{part}");
                let _ = Command::new("debugfs")
                    .args(["-w", "-R", &format!("mkdir {cur}")])
                    .arg(&image)
                    .output();
            }
        }
        debugfs(&image, &format!("write {} {guest_path}", host.display()));
    }

    fn head_files(m: &WorkspaceManager, ws: &str) -> std::collections::BTreeMap<String, Vec<u8>> {
        let head = m.head(ws).unwrap().expect("a head");
        let manifest = m.manifest_manager.get_manifest(ws, &head).unwrap();
        manifest
            .files
            .iter()
            .map(|f| {
                let mut bytes = Vec::new();
                for c in &f.chunks {
                    bytes.extend(m.chunk_store.get_chunk(c).unwrap());
                }
                (f.path.clone(), bytes)
            })
            .collect()
    }

    #[test]
    fn a_commit_reads_the_guest_image_and_composes_the_host_vam_dir() {
        need_tools!();
        let d = tempdir().unwrap();
        let scratch = tempdir().unwrap();
        let m = manager(d.path());
        let tree = m
            .materialize("s1", "ws", &profile(), None, None, None)
            .unwrap();
        // The host's conversation, written after the image was built.
        fs::create_dir_all(tree.join(".vam/threads/t")).unwrap();
        fs::write(tree.join(".vam/threads/t/timeline.jsonl"), b"host side\n").unwrap();

        guest_write(
            &m,
            "s1",
            scratch.path(),
            "workspace/guest.txt",
            b"from the guest",
        );
        // The guest's own forged copy of the host's directory, and a symlink.
        guest_write(
            &m,
            "s1",
            scratch.path(),
            ".vam/threads/t/timeline.jsonl",
            b"forged\n",
        );
        debugfs(
            &m.layout().session_image_path("s1"),
            "symlink workspace/link /etc/passwd",
        );

        m.commit_and_release("s1", "ws").unwrap();
        let files = head_files(&m, "ws");
        assert_eq!(files["workspace/guest.txt"], b"from the guest");
        assert_eq!(files[".vam/threads/t/timeline.jsonl"], b"host side\n");
        assert!(!files.contains_key("workspace/link"), "{:?}", files.keys());
        // The session tree was not replaced by an extraction, and no copy of the
        // host `.vam/` was made.
        assert!(!tree.join("workspace/guest.txt").exists());
        let leftovers: Vec<_> = fs::read_dir(m.layout().session_dir("s1"))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(".vam-backup") || n.starts_with("extract-tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn a_live_end_reads_the_image_and_lists_what_a_manifest_cannot_hold() {
        need_tools!();
        let d = tempdir().unwrap();
        let scratch = tempdir().unwrap();
        let m = manager(d.path());
        m.materialize("s1", "ws", &profile(), None, None, None)
            .unwrap();
        guest_write(&m, "s1", scratch.path(), "workspace/a.txt", b"a");
        debugfs(
            &m.layout().session_image_path("s1"),
            "symlink workspace/link /etc/passwd",
        );

        let prepared = m.prepare_end("s1", "ws", EndBase::Live).unwrap();
        assert_eq!(prepared.dropped.count, 1, "{:?}", prepared.dropped);
        assert_eq!(prepared.dropped.entries[0].path, "workspace/link");
        let hash = m.finish_end("s1", "ws", &prepared).unwrap();
        m.release_lock("ws", "s1").unwrap();
        assert_eq!(m.head("ws").unwrap(), Some(hash.clone()));
        assert_eq!(
            m.manifest_files_hash("ws", &hash).unwrap(),
            prepared.files_hash
        );
        assert_eq!(head_files(&m, "ws")["workspace/a.txt"], b"a");
    }

    #[test]
    fn rolled_back_files_are_kept_from_the_image_within_the_cap() {
        need_tools!();
        let d = tempdir().unwrap();
        let scratch = tempdir().unwrap();
        let m = manager(d.path());
        m.materialize("s0", "ws", &profile(), None, None, None)
            .unwrap();
        m.commit_and_release("s0", "ws").unwrap();
        let base = m.head("ws").unwrap().unwrap();

        m.materialize("s1", "ws", &profile(), None, None, None)
            .unwrap();
        guest_write(&m, "s1", scratch.path(), "workspace/small.txt", b"half");
        guest_write(&m, "s1", scratch.path(), "workspace/sub/deep.txt", b"deep");
        guest_write(
            &m,
            "s1",
            scratch.path(),
            "workspace/big.bin",
            &vec![5u8; 200 * 1024],
        );
        let report = m
            .reclaim_session_tree("s1", "ws", &base, Some(&base), true, None, 100 * 1024)
            .unwrap();
        assert_eq!(report.kept_files, 2, "{report:?}");
        assert_eq!(report.skipped_over_cap, 1);
        let kept = m
            .layout()
            .kept_session_dir(report.kept_id.as_deref().unwrap())
            .join("rolled-back");
        assert_eq!(fs::read(kept.join("workspace/small.txt")).unwrap(), b"half");
        assert_eq!(
            fs::read(kept.join("workspace/sub/deep.txt")).unwrap(),
            b"deep"
        );
        assert!(!kept.join("workspace/big.bin").exists());
        assert!(!m.layout().session_dir("s1").exists());
        // The copy holds guest-named files: only the owner can open it.
        for dir in [kept.as_path(), kept.parent().unwrap()] {
            let mode = fs::metadata(dir).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700, "{dir:?}");
        }
    }

    #[test]
    fn an_unreadable_image_keeps_the_tree_instead_of_dropping_rolled_back_files() {
        need_tools!();
        let d = tempdir().unwrap();
        let m = manager(d.path());
        m.materialize("s0", "ws", &profile(), None, None, None)
            .unwrap();
        m.commit_and_release("s0", "ws").unwrap();
        let base = m.head("ws").unwrap().unwrap();
        m.materialize("s1", "ws", &profile(), None, None, None)
            .unwrap();
        fs::write(m.layout().session_image_path("s1"), vec![0xffu8; 1 << 20]).unwrap();
        let err = m
            .reclaim_session_tree("s1", "ws", &base, Some(&base), true, None, 1 << 20)
            .unwrap_err();
        assert!(matches!(err, Drive9Error::ImageCorrupt(_)), "{err:?}");
        assert!(m.layout().session_dir("s1").exists());
    }

    fn copy_limits() -> crate::image_snapshot::SnapshotLimits {
        crate::image_snapshot::SnapshotLimits::default()
    }

    /// A session at HEAD "base" whose guest wrote `note.txt`, took turn copy 1,
    /// then wrote `partial.txt` (and the host wrote `.vam/`).
    fn stopped_session(m: &WorkspaceManager, scratch: &Path) -> String {
        m.materialize("s0", "ws", &profile(), None, None, None)
            .unwrap();
        m.commit_and_release("s0", "ws").unwrap();
        let tree = m
            .materialize("s1", "ws", &profile(), None, None, None)
            .unwrap();
        fs::create_dir_all(tree.join(".vam/threads/t")).unwrap();
        fs::write(tree.join(".vam/threads/t/log"), b"conversation").unwrap();
        guest_write(m, "s1", scratch, "workspace/note.txt", b"kiwi-42");
        let report = m.snapshot_turn_image("s1", 1, &copy_limits()).unwrap();
        assert!(matches!(report.method.as_str(), "reflink" | "sparse-copy"));
        guest_write(m, "s1", scratch, "workspace/partial.txt", b"half");
        m.resolve_turn_copy("s1", "ws", 1).unwrap()
    }

    #[test]
    fn a_turn_copy_resolves_to_the_files_of_that_turn_not_the_live_image() {
        need_tools!();
        let d = tempdir().unwrap();
        let scratch = tempdir().unwrap();
        let m = manager(d.path());
        let turn = stopped_session(&m, scratch.path());
        let manifest = m.manifest_manager.get_manifest("ws", &turn).unwrap();
        let paths: Vec<&str> = manifest.files.iter().map(|f| f.path.as_str()).collect();
        assert!(paths.contains(&"workspace/note.txt"), "{paths:?}");
        assert!(!paths.contains(&"workspace/partial.txt"), "{paths:?}");
        // The copy is private to the host and apart from the live image.
        let copy = m.layout().session_turn_image_path("s1", 1);
        let mode = fs::metadata(&copy).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        use std::os::unix::fs::MetadataExt;
        assert_ne!(
            fs::metadata(&copy).unwrap().ino(),
            fs::metadata(m.layout().session_image_path("s1"))
                .unwrap()
                .ino()
        );
        // HEAD did not move.
        assert_ne!(m.head("ws").unwrap().unwrap(), turn);
    }

    #[test]
    fn an_ext4_stop_rolls_back_to_the_turn_copy_and_keeps_the_rest_in_kept() {
        need_tools!();
        let d = tempdir().unwrap();
        let scratch = tempdir().unwrap();
        let m = manager(d.path());
        let turn = stopped_session(&m, scratch.path());

        let prepared = m
            .prepare_end("s1", "ws", EndBase::TurnCommit(Some(turn.clone())))
            .unwrap();
        assert_eq!(prepared.base_manifest.as_deref(), Some(turn.as_str()));
        assert!(!prepared.refresh_template);
        let listed = prepared
            .rolled_back
            .clone()
            .expect("a stop lists the files");
        assert_eq!(
            listed.iter().map(|(p, _)| p.as_str()).collect::<Vec<_>>(),
            vec!["workspace/partial.txt"]
        );
        assert_eq!(prepared.dropped.count, 1);

        let head = m.finish_end("s1", "ws", &prepared).unwrap();
        m.release_lock("ws", "s1").unwrap();
        let files = head_files(&m, "ws");
        assert_eq!(files["workspace/note.txt"], b"kiwi-42");
        assert!(
            !files.contains_key("workspace/partial.txt"),
            "{:?}",
            files.keys()
        );
        // The host `.vam/` is composed in; the guest never wrote it.
        assert_eq!(files[".vam/threads/t/log"], b"conversation");
        // No template at the new HEAD: the live image held rolled-back work.
        assert!(!m.layout().template_path("ws", &head, "none").exists());

        let report = m
            .reclaim_session_tree(
                "s1",
                "ws",
                &head,
                prepared.base_manifest.as_deref(),
                true,
                prepared.rolled_back.clone(),
                ROLLED_BACK_KEEP_CAP_BYTES,
            )
            .unwrap();
        assert_eq!(report.kept_files, 1, "{report:?}");
        let kept = m
            .layout()
            .kept_session_dir(report.kept_id.as_deref().unwrap())
            .join("rolled-back");
        assert_eq!(
            fs::read(kept.join("workspace/partial.txt")).unwrap(),
            b"half"
        );
        assert!(!kept.join("workspace/note.txt").exists());
        // The tree, the live image and every turn copy are gone.
        assert!(!m.layout().session_dir("s1").exists());
    }

    #[test]
    fn a_live_end_refreshes_the_template_and_a_stop_does_not() {
        need_tools!();
        let d = tempdir().unwrap();
        let m = manager(d.path());
        m.materialize("s1", "ws", &profile(), None, None, None)
            .unwrap();
        let prepared = m.prepare_end("s1", "ws", EndBase::Live).unwrap();
        assert!(prepared.refresh_template);
        assert!(prepared.rolled_back.is_none());
        let head = m.finish_end("s1", "ws", &prepared).unwrap();
        assert!(
            m.layout().template_path("ws", &head, "none").is_file(),
            "a live end installs the template at the new HEAD"
        );
    }

    #[test]
    fn a_stop_with_an_unreadable_live_image_still_rolls_back_with_empty_lists() {
        need_tools!();
        let d = tempdir().unwrap();
        let scratch = tempdir().unwrap();
        let m = manager(d.path());
        let turn = stopped_session(&m, scratch.path());
        // The guest left the live image unreadable; the turn copy is fine.
        fs::write(m.layout().session_image_path("s1"), vec![0xffu8; 1 << 20]).unwrap();
        let prepared = m
            .prepare_end("s1", "ws", EndBase::TurnCommit(Some(turn.clone())))
            .unwrap();
        assert_eq!(prepared.base_manifest.as_deref(), Some(turn.as_str()));
        assert!(prepared.rolled_back.is_none());
        assert_eq!(prepared.dropped.count, 0);
        let head = m.finish_end("s1", "ws", &prepared).unwrap();
        assert_eq!(head_files(&m, "ws")["workspace/note.txt"], b"kiwi-42");
        // A live end of the same image is still a hard error.
        assert!(m.prepare_end("s1", "ws", EndBase::Live).is_err());
        let _ = head;
    }

    #[test]
    fn an_unreadable_turn_copy_is_image_corrupt_and_a_missing_one_is_an_io_error() {
        need_tools!();
        let d = tempdir().unwrap();
        let scratch = tempdir().unwrap();
        let m = manager(d.path());
        let _ = stopped_session(&m, scratch.path());
        let copy = m.layout().session_turn_image_path("s1", 1);
        fs::write(&copy, vec![0xffu8; 1 << 20]).unwrap();
        let err = m.resolve_turn_copy("s1", "ws", 1).unwrap_err();
        assert!(matches!(err, Drive9Error::ImageCorrupt(_)), "{err:?}");
        let err = m.resolve_turn_copy("s1", "ws", 7).unwrap_err();
        assert!(matches!(err, Drive9Error::IoAt { .. }), "{err:?}");
    }

    #[test]
    fn pruning_keeps_only_the_named_rounds_and_removes_strays() {
        need_tools!();
        let d = tempdir().unwrap();
        let m = manager(d.path());
        m.materialize("s1", "ws", &profile(), None, None, None)
            .unwrap();
        for round in [1, 2, 3] {
            m.snapshot_turn_image("s1", round, &copy_limits()).unwrap();
        }
        let turns = m.layout().session_turns_dir("s1");
        fs::write(turns.join("junk"), b"x").unwrap();
        fs::create_dir(turns.join("9.ext4.partial")).unwrap();
        assert_eq!(m.turn_copy_rounds("s1"), vec![1, 2, 3]);
        assert_eq!(m.prune_turn_copies("s1", &[3]).unwrap(), 4);
        assert_eq!(m.turn_copy_rounds("s1"), vec![3]);
        assert_eq!(fs::read_dir(&turns).unwrap().count(), 1);
        // No turns dir at all is fine.
        assert_eq!(m.prune_turn_copies("nobody", &[]).unwrap(), 0);
    }

    #[test]
    fn a_turn_copy_of_a_reclaimed_session_is_refused_and_leaves_nothing() {
        need_tools!();
        let d = tempdir().unwrap();
        let m = manager(d.path());
        m.materialize("s1", "ws", &profile(), None, None, None)
            .unwrap();
        m.discard("s1", "ws").unwrap();
        let err = m.snapshot_turn_image("s1", 1, &copy_limits()).unwrap_err();
        assert!(matches!(err, Drive9Error::NotFound(_)), "{err:?}");
        assert!(!m.layout().session_dir("s1").exists());
    }

    #[test]
    fn a_reclaim_waits_for_a_turn_copy_in_progress() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        let d = tempdir().unwrap();
        let m = Arc::new(mock_manager_for_lock(d.path()));
        let copying = Arc::new(AtomicBool::new(false));
        let reclaimed = Arc::new(AtomicBool::new(false));
        let (m1, copying1, reclaimed1) = (m.clone(), copying.clone(), reclaimed.clone());
        let holder = std::thread::spawn(move || {
            m1.with_session_lock("s1", || {
                copying1.store(true, Ordering::SeqCst);
                std::thread::sleep(std::time::Duration::from_millis(300));
                // The reclaim must still be waiting.
                assert!(!reclaimed1.load(Ordering::SeqCst));
            });
        });
        while !copying.load(Ordering::SeqCst) {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let (m2, reclaimed2) = (m.clone(), reclaimed.clone());
        let reclaimer = std::thread::spawn(move || {
            m2.with_session_lock("s1", || reclaimed2.store(true, Ordering::SeqCst));
        });
        holder.join().unwrap();
        reclaimer.join().unwrap();
        assert!(reclaimed.load(Ordering::SeqCst));
        // Another session is not held up.
        m.with_session_lock("other", || ());
    }

    fn mock_manager_for_lock(root: &Path) -> WorkspaceManager {
        let layout = Drive9Config::new(root).layout();
        let chunks = ChunkStore::new(layout.clone());
        let manifests = ManifestManager::new(layout.clone(), chunks.clone());
        let cache = CacheManager::new(layout.clone(), chunks.clone(), 10 * 1024 * 1024).unwrap();
        WorkspaceManager::new(
            layout,
            chunks,
            manifests,
            cache,
            Box::new(crate::MockImageIo),
        )
    }

    #[test]
    fn the_image_manifest_equals_the_directory_builders_for_the_same_tree() {
        need_tools!();
        let d = tempdir().unwrap();
        let src = d.path().join("src");
        fs::create_dir_all(src.join("workspace/sub")).unwrap();
        fs::create_dir_all(src.join(".vam/threads")).unwrap();
        fs::write(src.join(".vam/threads/x"), b"host only").unwrap();
        fs::write(src.join("workspace/a.txt"), b"alpha").unwrap();
        fs::write(src.join("workspace/sub/empty"), b"").unwrap();
        // More than one chunk, so chunk boundaries are compared too.
        let mut big = vec![0u8; 3 * 1024 * 1024 + 123];
        let mut x = 0x1234_5678_9abc_def0u64;
        for b in big.iter_mut() {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            *b = x as u8;
        }
        fs::write(src.join("workspace/sub/big.bin"), &big).unwrap();
        fs::write(src.join("run.sh"), b"#!/bin/sh\n").unwrap();
        fs::set_permissions(src.join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();

        // The node's image (ext2-style block maps, `build_image`) and an extent
        // image (`-t ext4`) must give the manifest the directory builder gives.
        let block_mapped = d.path().join("img-blockmap");
        E2fsImageIo::new()
            .build_image(&src, &block_mapped, 32 * 1024 * 1024)
            .unwrap();
        let extents = d.path().join("img-extents");
        let out = Command::new("mke2fs")
            .args([
                "-q",
                "-F",
                "-t",
                "ext4",
                "-O",
                "^has_journal",
                "-m",
                "0",
                "-d",
            ])
            .arg(&src)
            .arg(&extents)
            .arg("32M")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );

        let m = manager(d.path());
        let from_dir = m
            .manifest_manager
            .build_from_dir_excluding("ws", &src, None, &[VAM_DIR])
            .unwrap();
        let key = |f: &crate::manifest::ManifestFile| {
            (
                f.path.clone(),
                f.size_bytes,
                f.mode_permissions,
                f.chunks.clone(),
            )
        };
        let b: Vec<_> = from_dir.files.iter().map(key).collect();
        for image in [&block_mapped, &extents] {
            let (from_image, report) = m
                .manifest_manager
                .build_from_image(
                    "ws",
                    m.image_io.as_ref(),
                    image,
                    None,
                    &ImageLimits::default(),
                )
                .unwrap();
            assert_eq!(report.files, 4, "{image:?}");
            let a: Vec<_> = from_image.files.iter().map(key).collect();
            assert_eq!(a, b, "{image:?}");
            assert_eq!(from_image.files_hash(), from_dir.files_hash(), "{image:?}");
        }
    }

    #[test]
    fn manifest_paths_that_escape_are_refused_at_commit_and_at_materialize() {
        let d = tempdir().unwrap();
        let m = manager(d.path());
        for bad in ["../x", "/etc/cron.d/x", "a/../../x", "a//b", "", "a\0b"] {
            let mut manifest = MerkleManifest::new("ws".into(), None);
            manifest.files.push(crate::manifest::ManifestFile {
                path: bad.to_string(),
                size_bytes: 0,
                mode_permissions: 0o100644,
                chunks: vec![],
            });
            assert!(
                matches!(
                    m.manifest_manager.commit_manifest(&manifest),
                    Err(Drive9Error::InvalidManifestPath(_))
                ),
                "{bad:?}"
            );
        }
        // A manifest already on disk (written by an older build or by hand).
        let mut manifest = MerkleManifest::new("ws".into(), None);
        manifest.files.push(crate::manifest::ManifestFile {
            path: "../escape".into(),
            size_bytes: 0,
            mode_permissions: 0o100644,
            chunks: vec![],
        });
        let hash = manifest.hash().unwrap();
        let path = m.layout().manifest_path("ws", &hash);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, manifest.serialize().unwrap()).unwrap();
        m.manifest_manager.write_head("ws", &hash).unwrap();
        let err = m
            .materialize("s1", "ws", &profile(), None, None, None)
            .unwrap_err();
        assert!(
            matches!(err, Drive9Error::InvalidManifestPath(_)),
            "{err:?}"
        );
        assert!(!d.path().join("escape").exists());
        assert!(!m.layout().session_dir("s1").join("escape").exists());
    }
}
