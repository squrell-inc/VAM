//! Golden Firecracker snapshot bake and restore helpers (Phase 4 Tier 1).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use sha2::{Digest, Sha256};

use crate::error::OrchestratorError;
use crate::timing::{merge_shared, SharedPhaseTimer};
use crate::VmmFactory;
use vam_core::Vmm;
use vam_network::NetResourcePool;
use vam_storage::WorkspaceManager;

pub const GOLDEN_VCPU: u8 = 2;
pub const GOLDEN_MEM_MIB: usize = 512;
pub const GOLDEN_GUEST_CID: u32 = 3;
pub const GOLDEN_DISK_BYTES: u64 = 1024 * 1024 * 1024;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GoldenKeyMeta {
    pub key: String,
    pub kernel_hash: String,
    pub rootfs_hash: String,
    pub vcpu_count: u8,
    pub mem_mib: usize,
    pub guest_cid: u32,
}

#[derive(Debug, Clone)]
pub struct GoldenSnapshotPaths {
    pub dir: PathBuf,
    pub vm_state: PathBuf,
    pub vm_mem: PathBuf,
    pub companion_disk: PathBuf,
    pub key_json: PathBuf,
}

#[derive(Debug, Clone)]
pub struct GoldenRestoreCandidate {
    pub paths: GoldenSnapshotPaths,
    pub meta: GoldenKeyMeta,
    pub guest_cid: u32,
}

static JAILED_NODE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static JAIL_UID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Marks this node as running Firecracker under the jailer as `jail_uid`. The golden
/// snapshot is then baked and restored inside jails: it records in-jail file names
/// (`/data0.ext4`, `/v.sock`), so it gets its own key (an unjailed snapshot records host
/// paths and cannot restore in a jail, nor the other way round), and its memory and
/// state files must be ones the jailed VMM can read but not write.
pub fn set_jailed_node(jail_uid: u32) {
    JAIL_UID.store(jail_uid, std::sync::atomic::Ordering::SeqCst);
    JAILED_NODE.store(true, std::sync::atomic::Ordering::SeqCst);
}

fn jailed_node() -> bool {
    JAILED_NODE.load(std::sync::atomic::Ordering::SeqCst)
}

/// Mixed into the golden key of a jailed node only, so unjailed keys (and the snapshots
/// already baked under them) do not change.
const JAILED_KEY_TAG: &str = "jailed-flat-v1";

/// Kernel argument PID 1 honours: it occupies `127.0.0.1:3128` before the egress
/// proxy starts, so the lane can prove the proxy copes with a busy loopback port.
const HOLD_PROXY_PORT_FLAG: &str = "vam_test_hold_proxy_port=1";

/// Mixed into the golden key of a node that sets [`HOLD_PROXY_PORT_FLAG`] only. The
/// snapshot remembers the bake command line and the key does not hash it, so a
/// snapshot baked without the flag must not be restored on a node that wants it.
/// Real nodes' keys and snapshots do not change.
const HOLD_PROXY_KEY_TAG: &str = "hold-proxy-port-v1";

/// True on a test node: the same "mock scripts loaded" signal that registers the
/// test-only host adapters. The flag only removes a loopback listener.
fn hold_proxy_port_node() -> bool {
    crate::brokers::mock_scripts_loaded()
}

/// Returns false when `VAM_SNAPSHOT_RESTORE=0`.
pub fn snapshot_restore_enabled() -> bool {
    std::env::var("VAM_SNAPSHOT_RESTORE").ok().as_deref() != Some("0")
}

pub fn golden_root(drive9_root: &Path) -> PathBuf {
    drive9_root.join("golden")
}

pub fn resolve_guest_kernel_path() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("VAM_KERNEL_PATH") {
        let pb = std::path::PathBuf::from(p);
        if pb.exists() {
            return pb;
        }
    }
    for candidate in [
        "/var/lib/vam/kernels/vmlinux-6.18",
        "/var/lib/vam/kernels/vmlinux-6.18.x",
        "/var/lib/vam/kernels/vmlinux-6.6",
        "/var/lib/vam/kernels/vmlinux-6.6.x",
        "/var/lib/vam/kernels/vmlinux-6.1",
        "/var/lib/vam/kernels/vmlinux-6.1.x",
        "/var/lib/vam/kernels/vmlinux-5.10.211",
        "/var/lib/vam/kernels/vmlinux",
    ] {
        let pb = std::path::PathBuf::from(candidate);
        if pb.exists() {
            return pb;
        }
    }
    std::path::PathBuf::from("/var/lib/vam/kernels/vmlinux-6.18")
}

pub fn resolve_rootfs_path() -> PathBuf {
    // No built-in path: a VM node refuses to start without `VAM_ROOTFS_PATH` (IT-2.8), so an
    // unset variable here is an empty path that fails where it is opened.
    std::env::var("VAM_ROOTFS_PATH")
        .map(PathBuf::from)
        .unwrap_or_default()
}

/// What the kernel says a file is: any change below (a rewrite in place, a rename
/// over it, a new inode, a new size) gives a different stamp.
///
/// There is no ctime: every per-session hard link into a jail bumps the shared
/// inode's ctime, which would force a multi-second re-hash per session. The jailer
/// preflight already refuses a kernel or rootfs that anyone but root can write, so
/// nothing that could rewrite the bytes while keeping dev, ino, size and mtime runs
/// as a non-root user.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FileStamp {
    dev: u64,
    ino: u64,
    size: u64,
    mtime: (i64, i64),
}

impl FileStamp {
    fn of(meta: &fs::Metadata) -> Self {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Self {
                dev: meta.dev(),
                ino: meta.ino(),
                size: meta.size(),
                mtime: (meta.mtime(), meta.mtime_nsec()),
            }
        }
        #[cfg(not(unix))]
        {
            let modified = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| (d.as_secs() as i64, d.subsec_nanos() as i64))
                .unwrap_or((0, 0));
            Self {
                dev: 0,
                ino: 0,
                size: meta.len(),
                mtime: modified,
            }
        }
    }

    /// True when the file's content changed less than `window` ago (mtime; a link
    /// or chown alone does not count). A change inside the timestamp granularity
    /// could leave the same stamp, so such a hash is not trusted for later calls.
    fn changed_within(&self, window: std::time::Duration) -> bool {
        let newest = self.mtime;
        let Ok(now) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) else {
            return true;
        };
        let age_ns = (now.as_secs() as i128 - newest.0 as i128) * 1_000_000_000
            + (now.subsec_nanos() as i128 - newest.1 as i128);
        age_ns < window.as_nanos() as i128
    }
}

/// SHA-256 of whole files, remembered per canonical path while the file's
/// [`FileStamp`] is unchanged. Every session start needs the golden key, which
/// hashes the guest kernel and rootfs: hundreds of MiB, read in full, in a debug
/// build on the lane. The stamp makes that once per file, not once per session.
pub(crate) struct HashCache {
    entries: std::sync::Mutex<std::collections::HashMap<PathBuf, (FileStamp, String)>>,
    reads: std::sync::atomic::AtomicUsize,
    racy_window: std::time::Duration,
}

impl HashCache {
    pub(crate) fn new(racy_window: std::time::Duration) -> Self {
        Self {
            entries: std::sync::Mutex::new(std::collections::HashMap::new()),
            reads: std::sync::atomic::AtomicUsize::new(0),
            racy_window,
        }
    }

    /// How many times a file was actually read and hashed.
    #[cfg(test)]
    fn reads(&self) -> usize {
        self.reads.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// The hash of `path`, and whether it came from the cache.
    fn hash(&self, path: &Path) -> Result<(String, bool), OrchestratorError> {
        let fail = |e: std::io::Error| {
            OrchestratorError::Core(format!("failed to hash {}: {}", path.display(), e))
        };
        let canonical = fs::canonicalize(path).map_err(fail)?;
        let before = FileStamp::of(&fs::metadata(&canonical).map_err(fail)?);
        {
            let entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
            if let Some((stamp, hash)) = entries.get(&canonical) {
                if *stamp == before {
                    return Ok((hash.clone(), true));
                }
            }
        }
        let data = fs::read(&canonical).map_err(fail)?;
        self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let hash = format!("{:x}", Sha256::digest(&data));
        // Cache it only if the file did not change while it was read, and is not
        // too fresh for its timestamps to tell a later rewrite apart.
        let after = FileStamp::of(&fs::metadata(&canonical).map_err(fail)?);
        if after == before && !before.changed_within(self.racy_window) {
            self.entries
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(canonical, (before, hash.clone()));
        }
        Ok((hash, false))
    }
}

/// The process-wide cache behind [`compute_golden_key`].
fn hash_cache() -> &'static HashCache {
    static CACHE: std::sync::OnceLock<HashCache> = std::sync::OnceLock::new();
    CACHE.get_or_init(|| HashCache::new(std::time::Duration::from_secs(2)))
}

/// `sha256(kernel_hash + rootfs_hash + layout + vcpu + mem_mib)[0:16]` hex chars.
///
/// `ROOT_DRIVE_LAYOUT` is bumped whenever the root drive attachment changes,
/// because a snapshot carries its block device state: snapshots baked with a
/// read-write root must never be restored against the read-only one. It is also
/// bumped when the bake data drive changes: v1 snapshots were baked against an
/// unformatted disk at a throwaway path, v2 against an empty ext4 stored at
/// `golden/<key>/disk.ext4`, and v1 keys must never validate. v3 snapshots are
/// taken after the real guest registers; v2 ones were taken about 2.5 s into the
/// kernel boot and cannot be restored.
///
/// Workspace id is not part of this key. Drive9 ext4 templates are
/// workspace-scoped; Firecracker Tier 1 snapshots stay artifact+geometry
/// scoped so a prefixed workspace does not force a VM-snapshot re-bake.
const ROOT_DRIVE_LAYOUT: &str = "rootfs-ro-v3-ext4-bake";

pub fn compute_golden_key(
    kernel: &Path,
    rootfs: &Path,
    vcpu: u8,
    mem_mib: usize,
) -> Result<String, OrchestratorError> {
    compute_golden_key_traced(kernel, rootfs, vcpu, mem_mib).map(|(key, _)| key)
}

/// [`compute_golden_key`], also reporting whether both file hashes came from the cache.
pub fn compute_golden_key_traced(
    kernel: &Path,
    rootfs: &Path,
    vcpu: u8,
    mem_mib: usize,
) -> Result<(String, bool), OrchestratorError> {
    golden_key_tagged(
        hash_cache(),
        kernel,
        rootfs,
        vcpu,
        mem_mib,
        jailed_node(),
        hold_proxy_port_node(),
    )
}

#[cfg(test)]
fn golden_key_with(
    cache: &HashCache,
    kernel: &Path,
    rootfs: &Path,
    vcpu: u8,
    mem_mib: usize,
    jailed: bool,
) -> Result<(String, bool), OrchestratorError> {
    golden_key_tagged(cache, kernel, rootfs, vcpu, mem_mib, jailed, false)
}

fn golden_key_tagged(
    cache: &HashCache,
    kernel: &Path,
    rootfs: &Path,
    vcpu: u8,
    mem_mib: usize,
    jailed: bool,
    hold_proxy_port: bool,
) -> Result<(String, bool), OrchestratorError> {
    let (kernel_hash, kernel_cached) = cache.hash(kernel)?;
    let (rootfs_hash, rootfs_cached) = cache.hash(rootfs)?;
    let mut hasher = Sha256::new();
    hasher.update(kernel_hash.as_bytes());
    hasher.update(rootfs_hash.as_bytes());
    hasher.update(ROOT_DRIVE_LAYOUT.as_bytes());
    hasher.update([vcpu]);
    hasher.update((mem_mib as u32).to_le_bytes());
    if jailed {
        hasher.update(JAILED_KEY_TAG.as_bytes());
    }
    if hold_proxy_port {
        hasher.update(HOLD_PROXY_KEY_TAG.as_bytes());
    }
    let digest = hasher.finalize();
    let key = format!(
        "{:016x}",
        u64::from_be_bytes(digest[..8].try_into().unwrap_or([0u8; 8]))
    );
    Ok((key, kernel_cached && rootfs_cached))
}

/// SHA-256 of the guest kernel and root filesystem this node boots, from the
/// same cache as the golden key (a warm cache reads nothing). Reads files in
/// full on a cold cache: call from a blocking context.
pub fn image_digests() -> Result<(String, String), OrchestratorError> {
    image_digests_of(&resolve_guest_kernel_path(), &resolve_rootfs_path())
}

pub(crate) fn image_digests_of(
    kernel: &Path,
    rootfs: &Path,
) -> Result<(String, String), OrchestratorError> {
    Ok((hash_file(kernel)?, hash_file(rootfs)?))
}

/// SHA-256 of a whole file as bare lowercase hex, from the same cache as the golden key (a
/// warm cache reads nothing). The node base report compares this with the rootfs digest in
/// `<image>.base.json`.
pub(crate) fn file_sha256(path: &Path) -> Result<String, OrchestratorError> {
    hash_file(path)
}

/// File hash for `key.json`, from the same cache as the key.
fn hash_file(path: &Path) -> Result<String, OrchestratorError> {
    hash_cache().hash(path).map(|(hash, _)| hash)
}

pub fn golden_paths_for_key(golden_base: &Path, key: &str) -> GoldenSnapshotPaths {
    let dir = golden_base.join(key);
    GoldenSnapshotPaths {
        key_json: dir.join("key.json"),
        vm_state: dir.join("vm_state.bin"),
        vm_mem: dir.join("vm_mem.bin"),
        companion_disk: dir.join("disk.ext4"),
        dir,
    }
}

pub fn golden_files_valid(paths: &GoldenSnapshotPaths, expected_key: &str) -> bool {
    if !paths.vm_state.is_file() || !paths.vm_mem.is_file() || !paths.companion_disk.is_file() {
        return false;
    }
    let Ok(raw) = fs::read_to_string(&paths.key_json) else {
        return false;
    };
    let Ok(meta) = serde_json::from_str::<GoldenKeyMeta>(&raw) else {
        return false;
    };
    meta.key == expected_key
}

/// Why a session boots cold instead of from the golden snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ColdBootReason {
    /// `VAM_SNAPSHOT_RESTORE=0`.
    RestoreDisabled,
    /// Golden snapshots are baked for the default storage profile only.
    NonDefaultProfile(String),
    /// The session image is missing or empty.
    ImageUnusable(String),
    /// The session image is larger than the baked data drive.
    ImageTooLarge { bytes: u64 },
    /// No baked snapshot matches the current kernel and rootfs.
    NoValidKey(String),
    /// A jailed node refuses golden files the VMM could write.
    UnsafeGoldenFile(String),
}

impl std::fmt::Display for ColdBootReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RestoreDisabled => write!(f, "restore disabled (VAM_SNAPSHOT_RESTORE=0)"),
            Self::NonDefaultProfile(id) => {
                write!(f, "non-default storage profile '{}'", id)
            }
            Self::ImageUnusable(why) => write!(f, "session image unusable: {}", why),
            Self::ImageTooLarge { bytes } => write!(
                f,
                "image too large ({} bytes > {} byte golden disk)",
                bytes, GOLDEN_DISK_BYTES
            ),
            Self::NoValidKey(why) => write!(f, "no valid key: {}", why),
            Self::UnsafeGoldenFile(why) => write!(f, "golden file not safe for a jail: {}", why),
        }
    }
}

/// Session image must fit the data drive baked into the golden snapshot.
///
/// Any size from one byte up to `GOLDEN_DISK_BYTES` is accepted; default session
/// images are content-sized (tens of MiB), not 1 GiB. The restore relies on
/// Firecracker re-reading the backing file when `restore_from_golden` PATCHes
/// `/drives/data0` to the session image: `Block::update_disk_image` re-opens the
/// file, recomputes the capacity from its length and raises a config-change
/// interrupt, and the guest then remounts `/dev/vdb` on `ResumeFromSnapshot`
/// (`remount_drive9_after_snapshot`). The ext4 inside is never larger than the
/// file, so a smaller image is fine. The upper bound keeps the image no larger
/// than the drive the snapshot was taken with. The vendored `vam_firecracker`
/// crate is only the REST client (`patch_guest_drive_by_id`); the resize
/// behaviour is Firecracker's own and is verified on a real host, not by the
/// mock VMM.
pub fn image_fits_golden_disk(image_path: &Path) -> Result<u64, ColdBootReason> {
    let len = fs::metadata(image_path)
        .map_err(|e| ColdBootReason::ImageUnusable(format!("{}: {}", image_path.display(), e)))?
        .len();
    if len == 0 {
        return Err(ColdBootReason::ImageUnusable(format!(
            "{} is empty",
            image_path.display()
        )));
    }
    if len > GOLDEN_DISK_BYTES {
        return Err(ColdBootReason::ImageTooLarge { bytes: len });
    }
    Ok(len)
}

/// Picks the boot path for a new session: a golden restore candidate, or the
/// reason it must boot cold (logged by the caller).
pub fn resolve_restore_candidate(
    drive9_root: &Path,
    storage_profile_id: &str,
    image_path: &Path,
) -> Result<GoldenRestoreCandidate, ColdBootReason> {
    if !snapshot_restore_enabled() {
        return Err(ColdBootReason::RestoreDisabled);
    }
    if storage_profile_id != "default" {
        return Err(ColdBootReason::NonDefaultProfile(
            storage_profile_id.to_string(),
        ));
    }
    image_fits_golden_disk(image_path)?;
    let kernel = resolve_guest_kernel_path();
    let rootfs = resolve_rootfs_path();
    let t_key = std::time::Instant::now();
    let (key, cached) = compute_golden_key_traced(&kernel, &rootfs, GOLDEN_VCPU, GOLDEN_MEM_MIB)
        .map_err(|e| ColdBootReason::NoValidKey(e.to_string()))?;
    eprintln!(
        "[orchestrator] golden key: {}ms cached={}",
        t_key.elapsed().as_millis(),
        cached
    );
    let paths = golden_paths_for_key(&golden_root(drive9_root), &key);
    if !golden_files_valid(&paths, &key) {
        return Err(ColdBootReason::NoValidKey(format!(
            "no complete snapshot for key {} under {}",
            key,
            paths.dir.display()
        )));
    }
    if jailed_node() {
        check_golden_files_for_jail(&paths, JAIL_UID.load(std::sync::atomic::Ordering::SeqCst))?;
    }
    let meta = fs::read_to_string(&paths.key_json)
        .map_err(|e| e.to_string())
        .and_then(|raw| serde_json::from_str::<GoldenKeyMeta>(&raw).map_err(|e| e.to_string()))
        .map_err(|e| ColdBootReason::NoValidKey(format!("key.json for {}: {}", key, e)))?;
    Ok(GoldenRestoreCandidate {
        guest_cid: meta.guest_cid,
        meta,
        paths,
    })
}

/// The VMM maps the memory file and reads the state file: both must be shared inodes it
/// can read and cannot rewrite. A jailed node that finds otherwise cold boots (it never
/// runs without the jailer).
fn check_golden_files_for_jail(
    paths: &GoldenSnapshotPaths,
    jail_uid: u32,
) -> Result<(), ColdBootReason> {
    for (what, path) in [
        ("golden snapshot memory", &paths.vm_mem),
        ("golden snapshot state", &paths.vm_state),
    ] {
        vam_core::jail::check_shared_asset(what, path, jail_uid)
            .map_err(|e| ColdBootReason::UnsafeGoldenFile(e.to_string()))?;
    }
    Ok(())
}

fn write_key_json(
    paths: &GoldenSnapshotPaths,
    meta: &GoldenKeyMeta,
) -> Result<(), OrchestratorError> {
    fs::create_dir_all(&paths.dir).map_err(|e| OrchestratorError::Core(e.to_string()))?;
    let json =
        serde_json::to_string_pretty(meta).map_err(|e| OrchestratorError::Core(e.to_string()))?;
    fs::write(&paths.key_json, json).map_err(|e| OrchestratorError::Core(e.to_string()))?;
    Ok(())
}

fn create_sparse_file(path: &Path, size: u64) -> Result<(), OrchestratorError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| OrchestratorError::Core(e.to_string()))?;
    }
    let file = fs::File::create(path).map_err(|e| OrchestratorError::Core(e.to_string()))?;
    file.set_len(size)
        .map_err(|e| OrchestratorError::Core(e.to_string()))?;
    Ok(())
}

/// Creates the bake data drive: a `GOLDEN_DISK_BYTES` sparse file formatted as an
/// empty ext4. The bake guest does not mount it (`vam_golden_bake=1`), but a
/// formatted disk keeps the artifact valid and inspectable, and a guest without
/// the flag would otherwise fail its fatal ext4 mount on all zeros.
fn create_bake_disk(path: &Path) -> Result<(), OrchestratorError> {
    create_formatted_disk(path, GOLDEN_DISK_BYTES)
}

/// A sparse file of `size` bytes formatted as an empty ext4.
fn create_formatted_disk(path: &Path, size: u64) -> Result<(), OrchestratorError> {
    create_sparse_file(path, size)?;
    let image = path.to_str().ok_or_else(|| {
        OrchestratorError::Core("golden bake disk path is not valid UTF-8".into())
    })?;
    // Same flags as vam-storage `E2fsImageIo::build_image`, without `-d`.
    let result = std::process::Command::new("mke2fs")
        .args([
            "-F",
            "-O",
            "^has_journal",
            "-m",
            "0",
            "-E",
            "lazy_itable_init=1,nodiscard",
            "-I",
            "256",
            image,
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .output();
    let failure = match result {
        Ok(out) if out.status.success() => return Ok(()),
        Ok(out) => format!(
            "mke2fs exited with {:?}: {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim()
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            "mke2fs not found in PATH (install e2fsprogs)".to_string()
        }
        Err(e) => format!("failed to run mke2fs: {}", e),
    };
    let _ = fs::remove_file(path);
    Err(OrchestratorError::Core(format!(
        "golden bake: cannot format {} as ext4: {}",
        path.display(),
        failure
    )))
}

/// Kernel args for the bake VM. `vam_golden_bake=1` tells guest init not to
/// mount the data drive: mounted bake-filesystem state would sit in the snapshot's
/// guest page cache and could be written into the session image patched in on
/// restore. Restore mounts `/dev/vdb` itself in `ResumeFromSnapshot`.
///
/// A restored session keeps this command line, so the busy-port test flag
/// ([`HOLD_PROXY_PORT_FLAG`]) rides here, on test nodes only.
fn bake_boot_args(vm_id: &str, hold_proxy_port: bool) -> String {
    let mut args = format!(
        "instance_id={} cid={} clocksource=kvm-clock vam_golden_bake=1",
        vm_id, GOLDEN_GUEST_CID
    );
    if hold_proxy_port {
        args.push(' ');
        args.push_str(HOLD_PROXY_PORT_FLAG);
    }
    args
}

/// Removes everything a bake writes so a half-finished one cannot validate.
/// `key.json` goes first: it is the marker `golden_files_valid` trusts.
fn discard_partial_bake(paths: &GoldenSnapshotPaths) {
    for file in [
        &paths.key_json,
        &paths.vm_state,
        &paths.vm_mem,
        &paths.companion_disk,
    ] {
        let _ = fs::remove_file(file);
    }
}

/// Daemon startup: skip bake when key matches; otherwise cold-boot a throwaway VM and snapshot it.
///
/// With `VAM_GOLDEN_BAKE_SOAK=N` set, N more bakes then run into scratch directories
/// (see [`run_bake_soak`]); the real snapshot is never touched by them.
pub async fn warmup_golden_snapshot(
    vmm_factory: VmmFactory,
    workspace_manager: Arc<WorkspaceManager>,
    net_pool: Arc<NetResourcePool>,
    drive9_root: &Path,
) {
    let ready = warmup_real_golden(
        vmm_factory.clone(),
        workspace_manager,
        net_pool,
        drive9_root,
    )
    .await;
    let Some(n) = bake_soak_count() else {
        return;
    };
    if !ready {
        eprintln!("[orchestrator] bake soak skipped: no golden snapshot is ready");
        return;
    }
    run_bake_soak(vmm_factory, drive9_root, n).await;
}

/// Bakes the golden snapshot for the current kernel and rootfs when it is missing.
/// True when a valid snapshot is in place afterwards.
async fn warmup_real_golden(
    vmm_factory: VmmFactory,
    workspace_manager: Arc<WorkspaceManager>,
    net_pool: Arc<NetResourcePool>,
    drive9_root: &Path,
) -> bool {
    let _ = (net_pool, workspace_manager);
    if !snapshot_restore_enabled() {
        eprintln!("Golden snapshot restore disabled (VAM_SNAPSHOT_RESTORE=0)");
        return false;
    }

    let kernel = resolve_guest_kernel_path();
    let rootfs = resolve_rootfs_path();
    let t_key = std::time::Instant::now();
    let key = match compute_golden_key_traced(&kernel, &rootfs, GOLDEN_VCPU, GOLDEN_MEM_MIB) {
        Ok((k, cached)) => {
            eprintln!(
                "[orchestrator] golden key: {}ms cached={}",
                t_key.elapsed().as_millis(),
                cached
            );
            k
        }
        Err(e) => {
            eprintln!("Golden snapshot bake failed: {} (cold boot only)", e);
            return false;
        }
    };

    let paths = golden_paths_for_key(&golden_root(drive9_root), &key);
    if golden_files_valid(&paths, &key) {
        eprintln!("Golden snapshot ready: {}", key);
        return true;
    }

    eprintln!("Golden snapshot miss for key {} — baking...", key);
    if let Err(e) = fs::create_dir_all(&paths.dir) {
        eprintln!("Golden snapshot bake failed: {} (cold boot only)", e);
        return false;
    }
    // Drop leftovers of an earlier failed bake before starting a new one.
    discard_partial_bake(&paths);
    let vm_id = format!("golden-bake-vm-{}", std::process::id());
    let job = BakeJob {
        vmm_factory,
        kernel: &kernel,
        rootfs: &rootfs,
        key: &key,
        paths: &paths,
        vsock_uds_path: vam_core::jail::vsock_uds_path(&vm_id),
        vm_id,
        timeouts: BakeTimeouts::from_env(),
    };
    match bake_into(job).await {
        Ok(timing) => {
            if golden_files_valid(&paths, &key) {
                eprintln!("[orchestrator] golden bake timing: {}", timing);
                eprintln!("Golden snapshot ready: {}", key);
                true
            } else {
                discard_partial_bake(&paths);
                eprintln!(
                    "Golden snapshot bake incomplete for key {} (missing vm_state/vm_mem/disk; cold boot only)",
                    key
                );
                false
            }
        }
        Err(e) => {
            discard_partial_bake(&paths);
            eprintln!("Golden snapshot bake failed: {} (cold boot only)", e);
            false
        }
    }
}

/// `MockVmm::get_pid`: the bake has no real guest to wait for.
const MOCK_VMM_PID: u32 = 12345;

fn bake_needs_stand_in_guest(vmm_pid: Option<u32>) -> bool {
    vmm_pid == Some(MOCK_VMM_PID)
}

/// How long each bake wait may take.
#[derive(Debug, Clone, Copy)]
struct BakeTimeouts {
    /// Boot to the guest's registration handshake.
    registration: std::time::Duration,
    /// Registration to the guest's `PrepareForSnapshot` answer.
    ready: std::time::Duration,
}

impl BakeTimeouts {
    fn from_env() -> Self {
        Self {
            registration: std::time::Duration::from_secs(30),
            ready: crate::driver::bake_ready_timeout(),
        }
    }
}

/// What the host's snapshot thread is told once the guest is (not) ready.
enum SnapGate {
    /// Pause the VM and snapshot it.
    Snapshot,
    /// Give up: shut the VM down without a snapshot.
    Abort,
}

/// Milliseconds from the VM start, per bake step.
#[derive(Debug, Clone, Copy, Default)]
struct BakeTiming {
    registered_ms: u128,
    ready_ms: u128,
    pause_ms: u128,
    snapshot_ms: u128,
}

impl std::fmt::Display for BakeTiming {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "registered={}ms ready={}ms pause={}ms snapshot={}ms",
            self.registered_ms, self.ready_ms, self.pause_ms, self.snapshot_ms
        )
    }
}

/// Removes the bake's vsock socket files on every exit path (Firecracker leaves
/// the base socket behind when the bake VM is killed).
struct BakeSockets(PathBuf);

impl Drop for BakeSockets {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
        for port in [52, 53, 54, 55] {
            let _ = fs::remove_file(format!("{}_{}", self.0.display(), port));
        }
    }
}

/// Everything one bake needs. The real bake and each soak bake differ only in `paths`,
/// `vm_id` and `vsock_uds_path`.
struct BakeJob<'a> {
    vmm_factory: VmmFactory,
    kernel: &'a Path,
    rootfs: &'a Path,
    key: &'a str,
    paths: &'a GoldenSnapshotPaths,
    vm_id: String,
    vsock_uds_path: PathBuf,
    timeouts: BakeTimeouts,
}

/// The stand-in a mock VMM bake uses for a guest: registers over the `_52` socket
/// and returns.
fn spawn_stand_in_guest(
    vm_id: String,
    connect_path: String,
) -> tokio::task::JoinHandle<Result<(), OrchestratorError>> {
    tokio::spawn(async move {
        let mut stream = None;
        for _ in 0..150 {
            #[cfg(unix)]
            if let Ok(s) = tokio::net::UnixStream::connect(&connect_path).await {
                stream = Some(s);
                break;
            }
            #[cfg(windows)]
            if let Ok(s) = tokio::net::TcpStream::connect("127.0.0.1:10052").await {
                stream = Some(s);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let stream = stream.ok_or_else(|| {
            OrchestratorError::Core("golden bake: guest failed to connect vsock".into())
        })?;
        vam_vsock::run_guest_registration(stream, &vm_id, GOLDEN_GUEST_CID)
            .await
            .map_err(|e| OrchestratorError::Core(format!("golden bake handshake: {}", e)))
    })
}

/// Boots a throwaway VM, waits for the guest to register and to confirm it is
/// quiescent, then pauses and snapshots it into `job.paths`. On any failure the VM is
/// shut down and nothing is left valid (the caller discards partial files).
async fn bake_into(job: BakeJob<'_>) -> Result<BakeTiming, OrchestratorError> {
    let BakeJob {
        vmm_factory,
        kernel,
        rootfs,
        key,
        paths,
        vm_id,
        vsock_uds_path,
        timeouts,
    } = job;
    let _sockets = BakeSockets(vsock_uds_path.clone());

    // Firecracker records the drive's `path_on_host` in `vm_state.bin` and reopens
    // it on `load_snapshot`, so the bake drive must live with the snapshot rather
    // than in a throwaway session dir. Restore then PATCHes it to the session image.
    let image_path = paths.companion_disk.clone();
    create_bake_disk(&image_path)?;

    let vsock_uds_path_with_port = format!("{}_52", vsock_uds_path.display());
    let _ = fs::remove_file(&vsock_uds_path_with_port);

    let host_listener = vam_vsock::HostListener::bind(&vsock_uds_path, 52)
        .await
        .map_err(|e| OrchestratorError::Core(format!("golden bake vsock bind: {}", e)))?;

    let (booted_tx, booted_rx) = std::sync::mpsc::sync_channel(1);
    let (snap_tx, snap_rx) = std::sync::mpsc::sync_channel::<SnapGate>(1);

    let vm_id_blocking = vm_id.clone();
    let kernel_path = kernel.to_path_buf();
    let rootfs_path = rootfs.to_path_buf();
    let image_path_blocking = image_path.clone();
    let paths_for_snap = paths.clone();
    let vmm_factory_boot = vmm_factory.clone();

    let boot_handle =
        tokio::task::spawn_blocking(move || -> Result<(u128, u128), OrchestratorError> {
            let mut vmm = vmm_factory_boot(&vm_id_blocking, GOLDEN_GUEST_CID);
            vmm.spawn()
                .map_err(|e| OrchestratorError::Core(format!("golden bake spawn: {}", e)))?;

            let config = vam_core::MicroVmConfigBuilder::new()
                .with_kernel(kernel_path)
                .with_rootfs(rootfs_path)
                .with_limits(GOLDEN_VCPU, GOLDEN_MEM_MIB)
                .with_guest_cid(GOLDEN_GUEST_CID)
                .with_boot_arg(&bake_boot_args(&vm_id_blocking, hold_proxy_port_node()))
                .with_data_drive(image_path_blocking)
                .build()
                .map_err(|e| OrchestratorError::Core(e.to_string()))?;

            vmm.configure_hardware(&config, None)
                .map_err(|e| OrchestratorError::Core(format!("golden bake configure: {}", e)))?;
            vmm.start_instance(None)
                .map_err(|e| OrchestratorError::Core(format!("golden bake start: {}", e)))?;

            booted_tx
                .send(vmm.get_pid())
                .map_err(|_| OrchestratorError::Core("golden bake boot signal lost".into()))?;

            // The host decides: snapshot once the guest is ready, abort otherwise. A
            // lost channel is an abort too, so this thread can never wait forever.
            match snap_rx.recv() {
                Ok(SnapGate::Snapshot) => {}
                Ok(SnapGate::Abort) | Err(_) => {
                    let _ = vmm.shutdown();
                    return Err(OrchestratorError::Core(
                        "golden bake aborted before the snapshot".into(),
                    ));
                }
            }

            let t_pause = std::time::Instant::now();
            vmm.pause()
                .map_err(|e| OrchestratorError::Core(format!("golden bake pause: {}", e)))?;
            let pause_ms = t_pause.elapsed().as_millis();
            let t_snap = std::time::Instant::now();
            vmm.create_snapshot(&paths_for_snap.vm_mem, &paths_for_snap.vm_state)
                .map_err(|e| OrchestratorError::Core(format!("golden bake snapshot: {}", e)))?;
            let snapshot_ms = t_snap.elapsed().as_millis();
            if !paths_for_snap.vm_mem.is_file() || !paths_for_snap.vm_state.is_file() {
                return Err(OrchestratorError::Core(format!(
                    "golden bake snapshot: expected {} and {} on disk",
                    paths_for_snap.vm_mem.display(),
                    paths_for_snap.vm_state.display()
                )));
            }
            vmm.shutdown()
                .map_err(|e| OrchestratorError::Core(format!("golden bake shutdown: {}", e)))?;
            Ok((pause_ms, snapshot_ms))
        });

    let booted_pid = booted_rx
        .recv()
        .map_err(|_| OrchestratorError::Core("golden bake boot wait failed".into()))?;
    let t_started = std::time::Instant::now();
    // A real guest registers itself once it has booted. Only a mock VMM has no guest,
    // so only then does the bake stand one in. Registering a stand-in for a real
    // guest would snapshot the VM mid kernel boot (the handshake completes
    // immediately), and the restored kernel then oopses in its vmgenid IRQ handler.
    let stand_in = bake_needs_stand_in_guest(booted_pid);
    let mut guest_task = if stand_in {
        Some(spawn_stand_in_guest(
            vm_id.clone(),
            vsock_uds_path_with_port.clone(),
        ))
    } else {
        None
    };

    let reg_future = async {
        let stream = host_listener
            .accept()
            .await
            .map_err(|e| OrchestratorError::Core(format!("golden bake accept: {}", e)))?;
        let challenge_registry = vam_vsock::ChallengeRegistry::new();
        let verifier = vam_vsock::DevChallengeOnlyVerifier;
        vam_vsock::handle_registration_handshake(
            stream,
            &vm_id,
            GOLDEN_GUEST_CID,
            &challenge_registry,
            &verifier,
        )
        .await
        .map_err(|e| OrchestratorError::Core(format!("golden bake registration: {}", e)))
    };

    let registered = match tokio::time::timeout(timeouts.registration, reg_future).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(OrchestratorError::Core(
            "golden bake registration timed out".into(),
        )),
    };
    if let Err(e) = registered {
        // Tell the VM thread to shut down; it would otherwise wait for the gate forever.
        let _ = snap_tx.send(SnapGate::Abort);
        if let Some(task) = guest_task.take() {
            let _ = task.await;
        }
        let _ = boot_handle.await;
        return Err(e);
    }
    let registered_ms = t_started.elapsed().as_millis();
    if let Some(task) = guest_task.take() {
        let _ = task.await;
    }

    // Registration only says the guest started. Pause it from a quiescent state: it
    // answers from its accept loop, with no dial outstanding and (bake VM) no flush
    // loop or egress connector running. Only a mock VMM has no guest to ask.
    if !stand_in {
        match crate::driver::prepare_guest_for_snapshot(&vsock_uds_path, timeouts.ready).await {
            Ok(t) => eprintln!(
                "[orchestrator] golden bake: guest ready (dial={}ms reply={}ms eof={})",
                t.dial_ms, t.reply_ms, t.eof_seen
            ),
            Err(e) => {
                let _ = snap_tx.send(SnapGate::Abort);
                let _ = boot_handle.await;
                return Err(OrchestratorError::Core(format!(
                    "golden bake: guest not ready to snapshot: {}",
                    e
                )));
            }
        }
    }
    let ready_ms = t_started.elapsed().as_millis();

    snap_tx
        .send(SnapGate::Snapshot)
        .map_err(|_| OrchestratorError::Core("golden bake snap signal send failed".into()))?;

    let (pause_ms, snapshot_ms) = boot_handle
        .await
        .map_err(|_| OrchestratorError::Core("golden bake tokio panic".to_string()))??;

    if !paths.companion_disk.is_file() {
        return Err(OrchestratorError::Core(format!(
            "golden bake: companion disk {} missing after snapshot",
            paths.companion_disk.display()
        )));
    }
    if jailed_node() {
        // The bake VMM (now gone) wrote this disk as the jail uid: nobody but root
        // owns the golden artifacts once the bake is published.
        vam_core::jail::reclaim_owned(&paths.companion_disk, 0, 0).map_err(|e| {
            OrchestratorError::Core(format!("golden bake: take back the bake disk: {}", e))
        })?;
    }

    let meta = GoldenKeyMeta {
        key: key.to_string(),
        kernel_hash: hash_file(kernel)?,
        rootfs_hash: hash_file(rootfs)?,
        vcpu_count: GOLDEN_VCPU,
        mem_mib: GOLDEN_MEM_MIB,
        guest_cid: GOLDEN_GUEST_CID,
    };
    write_key_json(paths, &meta)?;
    Ok(BakeTiming {
        registered_ms,
        ready_ms,
        pause_ms,
        snapshot_ms,
    })
}

/// `VAM_GOLDEN_BAKE_SOAK=N`: how many extra bakes to verify at startup. Unset, empty,
/// zero or not a number means none; more than 20 is capped.
fn bake_soak_count() -> Option<u32> {
    std::env::var("VAM_GOLDEN_BAKE_SOAK")
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .filter(|n| *n > 0)
        .map(|n| n.min(20))
}

/// One soak row: what the host saw for bake `k`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SoakRow {
    k: u32,
    n: u32,
    /// Boot to the guest's ready signal, when the bake got that far.
    ready_ms: Option<u128>,
    /// Reseed time on success, or `timeout` / the failure, when restore was tried.
    reseed: Option<String>,
    ok: bool,
}

impl std::fmt::Display for SoakRow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let ready = self
            .ready_ms
            .map(|ms| ms.to_string())
            .unwrap_or_else(|| "-".into());
        write!(
            f,
            "bake soak {}/{}: ready={} reseed={} ok={}",
            self.k,
            self.n,
            ready,
            self.reseed.as_deref().unwrap_or("-"),
            self.ok
        )
    }
}

/// Re-bakes `n` times into `<drive9_root>/golden-soak/<k>`, restores each bake onto a
/// fresh small session image and reseeds the guest, logs one line per bake and a
/// summary, and removes everything it made. It never reads or writes the real
/// `golden/<key>` directory, so it cannot publish or replace the golden snapshot.
/// Lane use only: each bake is a full VM boot.
pub async fn run_bake_soak(vmm_factory: VmmFactory, drive9_root: &Path, n: u32) {
    let kernel = resolve_guest_kernel_path();
    let rootfs = resolve_rootfs_path();
    let scratch_root = drive9_root.join("golden-soak");
    let mut ok_count = 0u32;
    for k in 1..=n {
        let row = soak_once(&vmm_factory, &kernel, &rootfs, &scratch_root, k, n).await;
        eprintln!("[orchestrator] {}", row);
        if row.ok {
            ok_count += 1;
        }
    }
    let _ = fs::remove_dir_all(&scratch_root);
    eprintln!("[orchestrator] bake soak: {}/{}", ok_count, n);
}

async fn soak_once(
    vmm_factory: &VmmFactory,
    kernel: &Path,
    rootfs: &Path,
    scratch_root: &Path,
    k: u32,
    n: u32,
) -> SoakRow {
    let mut row = SoakRow {
        k,
        n,
        ready_ms: None,
        reseed: None,
        ok: false,
    };
    let pid = std::process::id();
    let dir = scratch_root.join(k.to_string());
    let _ = fs::remove_dir_all(&dir);
    let paths = golden_paths_for_key(scratch_root, &k.to_string());
    if let Err(e) = fs::create_dir_all(&paths.dir) {
        row.reseed = Some(format!("scratch dir: {}", e));
        return row;
    }
    let bake_id = format!("golden-soak-bake-{}-{}", pid, k);
    let bake_sockets = vam_core::jail::vsock_uds_path(&bake_id);
    let job = BakeJob {
        vmm_factory: vmm_factory.clone(),
        kernel,
        rootfs,
        key: "soak",
        paths: &paths,
        vm_id: bake_id,
        vsock_uds_path: bake_sockets,
        timeouts: BakeTimeouts::from_env(),
    };
    match bake_into(job).await {
        Ok(timing) => row.ready_ms = Some(timing.ready_ms),
        Err(e) => {
            row.reseed = Some(format!("bake failed: {}", e));
            let _ = fs::remove_dir_all(&dir);
            return row;
        }
    }

    // Restore it the way a session does: onto a small formatted session image.
    let session_image = paths.dir.join("session.ext4");
    if let Err(e) = create_formatted_disk(&session_image, 64 * 1024 * 1024) {
        row.reseed = Some(format!("session image: {}", e));
        let _ = fs::remove_dir_all(&dir);
        return row;
    }
    let restore_id = format!("golden-soak-restore-{}-{}", pid, k);
    let restore_uds = vam_core::jail::vsock_uds_path(&restore_id);
    let _restore_sockets = BakeSockets(restore_uds.clone());
    let candidate = GoldenRestoreCandidate {
        meta: GoldenKeyMeta {
            key: "soak".into(),
            kernel_hash: String::new(),
            rootfs_hash: String::new(),
            vcpu_count: GOLDEN_VCPU,
            mem_mib: GOLDEN_MEM_MIB,
            guest_cid: GOLDEN_GUEST_CID,
        },
        paths: paths.clone(),
        guest_cid: GOLDEN_GUEST_CID,
    };
    let factory = vmm_factory.clone();
    let restore_uds_blocking = restore_uds.clone();
    let session_image_blocking = session_image.clone();
    let restored = tokio::task::spawn_blocking(move || {
        let mut vmm = factory(&restore_id, GOLDEN_GUEST_CID);
        let res = restore_from_golden(
            vmm.as_mut(),
            &candidate,
            &session_image_blocking,
            &restore_uds_blocking,
            None,
        );
        (vmm, res)
    })
    .await;
    let (mut vmm, restore_res) = match restored {
        Ok(v) => v,
        Err(_) => {
            row.reseed = Some("restore task panicked".into());
            let _ = fs::remove_dir_all(&dir);
            return row;
        }
    };
    match restore_res {
        Err(e) => row.reseed = Some(format!("restore failed: {}", e)),
        Ok(()) => {
            let t = std::time::Instant::now();
            // The same call a session makes; the 30 s default is shortened so a bad
            // bake costs the soak ten seconds, not thirty.
            match crate::driver::reseed_with_timeout(
                &restore_uds,
                RESTORE_SOAK_GENERATION,
                std::time::Duration::from_secs(10),
            )
            .await
            {
                Ok(()) => {
                    row.reseed = Some(t.elapsed().as_millis().to_string());
                    row.ok = true;
                }
                Err(e) if e.starts_with("Timed out") => row.reseed = Some("timeout".into()),
                Err(e) => row.reseed = Some(format!("failed: {}", e)),
            }
        }
    }
    // Always stop the restored VM: nothing may outlive the soak.
    let _ = tokio::task::spawn_blocking(move || {
        let _ = vmm.shutdown();
    })
    .await;
    let _ = fs::remove_dir_all(&dir);
    row
}

/// Lease generation the soak reseeds with (any positive value; no session reads it).
const RESTORE_SOAK_GENERATION: i64 = 1;

/// Load paused snapshot, patch drive + vsock, resume. Caller binds vsock ports before resume.
pub fn restore_from_golden(
    vmm: &mut dyn Vmm,
    golden: &GoldenRestoreCandidate,
    drive9_image_path: &Path,
    vsock_uds_path: &Path,
    timer: Option<SharedPhaseTimer>,
) -> Result<(), OrchestratorError> {
    let mut steps = Vec::new();
    let mut sink = Some(&mut steps as &mut Vec<(&'static str, u128)>);

    let t = std::time::Instant::now();
    vmm.spawn()
        .map_err(|e| OrchestratorError::Core(format!("restore spawn: {}", e)))?;

    // Jailed: the golden memory and state, the rootfs and a stand-in for the recorded
    // data drive go into this session's jail. Nothing for an unjailed VMM.
    let t_stage = std::time::Instant::now();
    let disk_len = fs::metadata(&golden.paths.companion_disk)
        .map_err(|e| OrchestratorError::Core(format!("restore golden disk: {}", e)))?
        .len();
    vmm.stage_snapshot_restore(
        &golden.paths.vm_mem,
        &golden.paths.vm_state,
        &resolve_rootfs_path(),
        disk_len,
    )
    .map_err(|e| OrchestratorError::Core(format!("restore stage: {}", e)))?;
    vam_core::record_timing_step(&mut sink, "snapshot_stage", t_stage);

    // The snapshot records the bake VM's vsock socket. Rebinding it on load would
    // fail with "Address in use" while that file exists, so load straight onto
    // the session's own path.
    vmm.load_snapshot(
        &golden.paths.vm_mem,
        &golden.paths.vm_state,
        false,
        Some(vsock_uds_path),
    )
    .map_err(|e| OrchestratorError::Core(format!("restore load_snapshot: {}", e)))?;
    vam_core::record_timing_step(&mut sink, "snapshot_load", t);

    let t = std::time::Instant::now();
    vmm.patch_data_drive_path("data0", drive9_image_path)
        .map_err(|e| OrchestratorError::Core(format!("restore patch drive: {}", e)))?;
    vam_core::record_timing_step(&mut sink, "snapshot_patch_drive", t);

    let t = std::time::Instant::now();
    vmm.resume()
        .map_err(|e| OrchestratorError::Core(format!("restore resume: {}", e)))?;
    vam_core::record_timing_step(&mut sink, "snapshot_resume", t);

    eprintln!(
        "[orchestrator] restore timing: {}",
        steps
            .iter()
            .map(|(name, us): &(&str, u128)| format!("{}={}ms", name, *us / 1000))
            .collect::<Vec<_>>()
            .join(" ")
    );
    merge_shared(&timer, steps);
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn image_digests_hash_the_kernel_and_rootfs_files() {
        let dir = tempfile::tempdir().expect("dir");
        let kernel = dir.path().join("vmlinux");
        let rootfs = dir.path().join("rootfs.ext4");
        std::fs::write(&kernel, b"kernel").unwrap();
        std::fs::write(&rootfs, b"rootfs").unwrap();
        let (k, r) = image_digests_of(&kernel, &rootfs).expect("digests");
        assert_eq!(k, format!("{:x}", Sha256::digest(b"kernel")));
        assert_eq!(r, format!("{:x}", Sha256::digest(b"rootfs")));
        assert!(image_digests_of(&dir.path().join("missing"), &rootfs).is_err());
    }

    use super::*;

    #[test]
    fn golden_key_is_deterministic() {
        let dir = tempfile::tempdir().unwrap();
        let kernel = dir.path().join("vmlinux");
        let rootfs = dir.path().join("rootfs.ext4");
        fs::write(&kernel, b"kernel-bytes").unwrap();
        fs::write(&rootfs, b"rootfs-bytes").unwrap();
        let k1 = compute_golden_key(&kernel, &rootfs, 2, 512).unwrap();
        let k2 = compute_golden_key(&kernel, &rootfs, 2, 512).unwrap();
        assert_eq!(k1, k2);
        assert_eq!(k1.len(), 16);
    }

    #[test]
    fn golden_key_excludes_workspace_id() {
        let dir = tempfile::tempdir().unwrap();
        let kernel = dir.path().join("vmlinux");
        let rootfs = dir.path().join("rootfs.ext4");
        fs::write(&kernel, b"kernel-bytes").unwrap();
        fs::write(&rootfs, b"rootfs-bytes").unwrap();
        let key = compute_golden_key(&kernel, &rootfs, GOLDEN_VCPU, GOLDEN_MEM_MIB).unwrap();
        assert_eq!(
            key,
            compute_golden_key(&kernel, &rootfs, GOLDEN_VCPU, GOLDEN_MEM_MIB).unwrap()
        );
        assert!(
            !key.contains("alice") && !key.contains("workspace"),
            "Firecracker golden key must stay kernel+rootfs+vcpu+mem: {key}"
        );
    }

    #[test]
    fn golden_key_is_stable_across_boots_of_the_same_rootfs() {
        let dir = tempfile::tempdir().unwrap();
        let kernel = dir.path().join("vmlinux");
        let rootfs = dir.path().join("rootfs.ext4");
        fs::write(&kernel, b"kernel-bytes").unwrap();
        fs::write(&rootfs, b"rootfs-bytes").unwrap();
        let before = compute_golden_key(&kernel, &rootfs, GOLDEN_VCPU, GOLDEN_MEM_MIB).unwrap();
        // A read-only root leaves the file untouched; a changed rootfs must still change the key.
        let after = compute_golden_key(&kernel, &rootfs, GOLDEN_VCPU, GOLDEN_MEM_MIB).unwrap();
        assert_eq!(before, after);
        fs::write(&rootfs, b"rootfs-bytes-modified").unwrap();
        let modified = compute_golden_key(&kernel, &rootfs, GOLDEN_VCPU, GOLDEN_MEM_MIB).unwrap();
        assert_ne!(before, modified);
    }

    fn mke2fs_available() -> bool {
        std::process::Command::new("mke2fs")
            .arg("-V")
            .output()
            .is_ok()
    }

    #[test]
    fn missing_or_empty_image_blocks_restore() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            image_fits_golden_disk(&dir.path().join("absent.ext4")),
            Err(ColdBootReason::ImageUnusable(_))
        ));
        let empty = dir.path().join("empty.ext4");
        fs::write(&empty, b"").unwrap();
        assert!(matches!(
            image_fits_golden_disk(&empty),
            Err(ColdBootReason::ImageUnusable(_))
        ));
    }

    #[test]
    fn content_sized_image_fits_golden_disk() {
        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join("session.ext4");
        let mib = 1024 * 1024;
        // Default sessions are content-sized, 64 MiB at minimum.
        create_sparse_file(&image, 64 * mib).unwrap();
        assert_eq!(image_fits_golden_disk(&image).unwrap(), 64 * mib);
        // Exactly the golden disk size (VAM_GOLDEN_GEOMETRY=1) still fits.
        create_sparse_file(&image, GOLDEN_DISK_BYTES).unwrap();
        assert!(image_fits_golden_disk(&image).is_ok());
    }

    #[test]
    fn oversized_image_blocks_restore() {
        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join("big.ext4");
        create_sparse_file(&image, GOLDEN_DISK_BYTES + 1024 * 1024).unwrap();
        assert!(matches!(
            image_fits_golden_disk(&image),
            Err(ColdBootReason::ImageTooLarge { .. })
        ));
    }

    #[test]
    fn bake_disk_is_formatted_ext4() {
        if !mke2fs_available() {
            eprintln!("skipping bake_disk_is_formatted_ext4: mke2fs not found");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let paths = golden_paths_for_key(dir.path(), "testkey");
        create_bake_disk(&paths.companion_disk).unwrap();
        assert_eq!(
            fs::metadata(&paths.companion_disk).unwrap().len(),
            GOLDEN_DISK_BYTES
        );
        // ext4 superblock magic 0xEF53 (little endian) at byte offset 1024 + 56.
        let mut f = fs::File::open(&paths.companion_disk).unwrap();
        let mut magic = [0u8; 2];
        std::io::Seek::seek(&mut f, std::io::SeekFrom::Start(1024 + 56)).unwrap();
        std::io::Read::read_exact(&mut f, &mut magic).unwrap();
        assert_eq!(magic, [0x53, 0xEF]);
    }

    #[test]
    fn bake_waits_for_the_real_guest() {
        assert!(bake_needs_stand_in_guest(Some(MOCK_VMM_PID)));
        assert!(!bake_needs_stand_in_guest(Some(98_765)));
        assert!(!bake_needs_stand_in_guest(None));
    }

    #[test]
    fn bake_boot_args_mark_the_bake_vm() {
        let args = bake_boot_args("vm-x", false);
        assert!(args.split_whitespace().any(|a| a == "vam_golden_bake=1"));
        assert!(args.contains("instance_id=vm-x"));
        assert!(!args.contains("vam_test_hold_proxy_port"));
    }

    #[test]
    fn bake_boot_args_carry_the_busy_port_flag_on_test_nodes_only() {
        let args = bake_boot_args("vm-x", true);
        assert!(args
            .split_whitespace()
            .any(|a| a == "vam_test_hold_proxy_port=1"));
        assert!(args.split_whitespace().any(|a| a == "vam_golden_bake=1"));
    }

    #[test]
    fn busy_port_flag_changes_the_golden_key_of_test_nodes_only() {
        let dir = tempfile::tempdir().unwrap();
        let (kernel, rootfs) = key_files(dir.path());
        let cache = HashCache::new(std::time::Duration::ZERO);
        let (plain, _) = golden_key_with(&cache, &kernel, &rootfs, 2, 512, false).unwrap();
        let (held, _) = golden_key_tagged(&cache, &kernel, &rootfs, 2, 512, false, true).unwrap();
        let (held_jailed, _) =
            golden_key_tagged(&cache, &kernel, &rootfs, 2, 512, true, true).unwrap();
        let (jailed, _) = golden_key_with(&cache, &kernel, &rootfs, 2, 512, true).unwrap();
        assert_ne!(
            plain, held,
            "a snapshot baked without the flag is not reused"
        );
        assert_ne!(jailed, held_jailed);
        assert_ne!(held, held_jailed);
        let (again, _) = golden_key_tagged(&cache, &kernel, &rootfs, 2, 512, false, false).unwrap();
        assert_eq!(plain, again, "a node without the flag keeps its key");
    }

    #[test]
    fn discarded_bake_does_not_validate() {
        let dir = tempfile::tempdir().unwrap();
        let paths = golden_paths_for_key(dir.path(), "k");
        fs::create_dir_all(&paths.dir).unwrap();
        fs::write(&paths.vm_state, b"s").unwrap();
        fs::write(&paths.vm_mem, b"m").unwrap();
        fs::write(&paths.companion_disk, b"d").unwrap();
        let meta = GoldenKeyMeta {
            key: "k".into(),
            kernel_hash: "kh".into(),
            rootfs_hash: "rh".into(),
            vcpu_count: GOLDEN_VCPU,
            mem_mib: GOLDEN_MEM_MIB,
            guest_cid: GOLDEN_GUEST_CID,
        };
        write_key_json(&paths, &meta).unwrap();
        assert!(golden_files_valid(&paths, "k"));
        // The companion disk is required.
        fs::remove_file(&paths.companion_disk).unwrap();
        assert!(!golden_files_valid(&paths, "k"));
        fs::write(&paths.companion_disk, b"d").unwrap();
        discard_partial_bake(&paths);
        assert!(!golden_files_valid(&paths, "k"));
        assert!(!paths.key_json.exists() && !paths.companion_disk.exists());
    }

    // ---- golden key cache -------------------------------------------------

    fn key_files(dir: &Path) -> (PathBuf, PathBuf) {
        let kernel = dir.join("vmlinux");
        let rootfs = dir.join("rootfs.ext4");
        fs::write(&kernel, b"kernel-bytes").unwrap();
        fs::write(&rootfs, b"rootfs-bytes").unwrap();
        (kernel, rootfs)
    }

    fn settle() {
        // Longer than the filesystem's timestamp granularity.
        std::thread::sleep(std::time::Duration::from_millis(30));
    }

    #[test]
    fn untouched_files_are_not_read_again() {
        let dir = tempfile::tempdir().unwrap();
        let (kernel, rootfs) = key_files(dir.path());
        let cache = HashCache::new(std::time::Duration::ZERO);
        let (k1, cached1) = golden_key_with(&cache, &kernel, &rootfs, 2, 512, false).unwrap();
        assert!(!cached1);
        assert_eq!(cache.reads(), 2);
        let (k2, cached2) = golden_key_with(&cache, &kernel, &rootfs, 2, 512, false).unwrap();
        assert!(cached2, "second key must come from the cache");
        assert_eq!(k1, k2);
        assert_eq!(
            cache.reads(),
            2,
            "an untouched file must not be hashed again"
        );
    }

    #[test]
    fn a_same_size_file_renamed_over_gives_a_new_key() {
        let dir = tempfile::tempdir().unwrap();
        let (kernel, rootfs) = key_files(dir.path());
        let cache = HashCache::new(std::time::Duration::ZERO);
        let (before, _) = golden_key_with(&cache, &kernel, &rootfs, 2, 512, false).unwrap();
        // Same length, different bytes, new inode (how an asset refresh replaces a file).
        settle();
        let staged = dir.path().join("rootfs.new");
        fs::write(&staged, b"rootfs-BYTES").unwrap();
        fs::rename(&staged, &rootfs).unwrap();
        let (after, cached) = golden_key_with(&cache, &kernel, &rootfs, 2, 512, false).unwrap();
        assert!(!cached);
        assert_ne!(before, after);
        assert_eq!(cache.reads(), 3, "only the replaced file is read again");
    }

    #[test]
    fn a_same_size_file_rewritten_in_place_gives_a_new_key() {
        let dir = tempfile::tempdir().unwrap();
        let (kernel, rootfs) = key_files(dir.path());
        let cache = HashCache::new(std::time::Duration::ZERO);
        let (before, _) = golden_key_with(&cache, &kernel, &rootfs, 2, 512, false).unwrap();
        settle();
        // Same inode and size; only mtime tells.
        fs::write(&kernel, b"KERNEL-bytes").unwrap();
        let (after, cached) = golden_key_with(&cache, &kernel, &rootfs, 2, 512, false).unwrap();
        assert!(!cached);
        assert_ne!(before, after);
    }

    #[test]
    fn a_file_changed_a_moment_ago_is_never_trusted_from_the_cache() {
        let dir = tempfile::tempdir().unwrap();
        let (kernel, rootfs) = key_files(dir.path());
        // The files were written just now: inside the window, so nothing is cached.
        let cache = HashCache::new(std::time::Duration::from_secs(3600));
        golden_key_with(&cache, &kernel, &rootfs, 2, 512, false).unwrap();
        let (_, cached) = golden_key_with(&cache, &kernel, &rootfs, 2, 512, false).unwrap();
        assert!(!cached);
        assert_eq!(cache.reads(), 4);
    }

    #[test]
    fn a_hard_link_alone_does_not_invalidate_the_cached_hash() {
        use std::os::unix::fs::MetadataExt;
        let dir = tempfile::tempdir().unwrap();
        let (kernel, rootfs) = key_files(dir.path());
        // Age the files so the racy window does not apply: only the link matters.
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        for f in [&kernel, &rootfs] {
            fs::File::options()
                .write(true)
                .open(f)
                .unwrap()
                .set_modified(old)
                .unwrap();
        }
        let cache = HashCache::new(std::time::Duration::from_secs(2));
        let (k1, _) = golden_key_with(&cache, &kernel, &rootfs, 2, 512, false).unwrap();
        assert_eq!(cache.reads(), 2);
        let ctime_before = fs::metadata(&rootfs).unwrap().ctime_nsec();
        settle();
        // What every session does: a hard link of the shared inode into a jail.
        fs::hard_link(&kernel, dir.path().join("jail-kernel")).unwrap();
        fs::hard_link(&rootfs, dir.path().join("jail-rootfs")).unwrap();
        assert_ne!(
            fs::metadata(&rootfs).unwrap().ctime_nsec(),
            ctime_before,
            "the link bumped ctime (the premise of this test)"
        );
        let (k2, cached) = golden_key_with(&cache, &kernel, &rootfs, 2, 512, false).unwrap();
        assert!(cached, "a link must not force a re-hash");
        assert_eq!(k1, k2);
        assert_eq!(cache.reads(), 2);
    }

    #[test]
    fn a_link_does_not_make_an_old_file_look_freshly_changed() {
        let dir = tempfile::tempdir().unwrap();
        let (kernel, rootfs) = key_files(dir.path());
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        for f in [&kernel, &rootfs] {
            fs::File::options()
                .write(true)
                .open(f)
                .unwrap()
                .set_modified(old)
                .unwrap();
        }
        // The link comes first: ctime is now, mtime is an hour ago.
        fs::hard_link(&rootfs, dir.path().join("l")).unwrap();
        let cache = HashCache::new(std::time::Duration::from_secs(2));
        golden_key_with(&cache, &kernel, &rootfs, 2, 512, false).unwrap();
        let (_, cached) = golden_key_with(&cache, &kernel, &rootfs, 2, 512, false).unwrap();
        assert!(cached, "only a change of content counts as recent");
    }

    #[test]
    fn a_jailed_node_gets_its_own_key_and_an_unjailed_key_is_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let (kernel, rootfs) = key_files(dir.path());
        let cache = HashCache::new(std::time::Duration::ZERO);
        let (plain, _) = golden_key_with(&cache, &kernel, &rootfs, 2, 512, false).unwrap();
        let (jailed, _) = golden_key_with(&cache, &kernel, &rootfs, 2, 512, true).unwrap();
        assert_ne!(
            plain, jailed,
            "in-jail names and host paths never share a snapshot"
        );
        // The unjailed digest is exactly what it was before the jailed tag existed.
        let mut h = Sha256::new();
        h.update(format!("{:x}", Sha256::digest(b"kernel-bytes")).as_bytes());
        h.update(format!("{:x}", Sha256::digest(b"rootfs-bytes")).as_bytes());
        h.update(ROOT_DRIVE_LAYOUT.as_bytes());
        h.update([2u8]);
        h.update(512u32.to_le_bytes());
        let d = h.finalize();
        assert_eq!(
            plain,
            format!("{:016x}", u64::from_be_bytes(d[..8].try_into().unwrap()))
        );
    }

    #[test]
    fn the_cache_follows_the_canonical_path() {
        let dir = tempfile::tempdir().unwrap();
        let (kernel, rootfs) = key_files(dir.path());
        let link = dir.path().join("rootfs-link.ext4");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&rootfs, &link).unwrap();
            let cache = HashCache::new(std::time::Duration::ZERO);
            golden_key_with(&cache, &kernel, &rootfs, 2, 512, false).unwrap();
            let (_, cached) = golden_key_with(&cache, &kernel, &link, 2, 512, false).unwrap();
            assert!(cached, "a symlink to the same file shares its entry");
        }
        #[cfg(not(unix))]
        let _ = (kernel, link);
    }

    #[test]
    fn a_missing_file_is_an_error_not_a_cached_value() {
        let dir = tempfile::tempdir().unwrap();
        let cache = HashCache::new(std::time::Duration::ZERO);
        let err = cache.hash(&dir.path().join("absent")).unwrap_err();
        assert!(err.to_string().contains("failed to hash"), "{err}");
    }

    // ---- bake with a mock VMM ---------------------------------------------

    type History = Arc<std::sync::Mutex<Vec<String>>>;

    fn mock_factory(pid: Option<u32>, history: &History) -> VmmFactory {
        let history = history.clone();
        Arc::new(move |_id, _cid| {
            let mut vmm = vam_core::MockVmm::with_shared_history(history.clone());
            vmm.pid = pid;
            Box::new(vmm)
        })
    }

    struct BakeRig {
        _dir: tempfile::TempDir,
        kernel: PathBuf,
        rootfs: PathBuf,
        paths: GoldenSnapshotPaths,
        uds: PathBuf,
    }

    fn bake_rig() -> BakeRig {
        let dir = tempfile::tempdir().unwrap();
        let (kernel, rootfs) = key_files(dir.path());
        let paths = golden_paths_for_key(&dir.path().join("golden"), "k");
        fs::create_dir_all(&paths.dir).unwrap();
        let uds = dir.path().join("vsock.socket");
        BakeRig {
            _dir: dir,
            kernel,
            rootfs,
            paths,
            uds,
        }
    }

    fn rig_job<'a>(rig: &'a BakeRig, factory: VmmFactory, timeouts: BakeTimeouts) -> BakeJob<'a> {
        BakeJob {
            vmm_factory: factory,
            kernel: &rig.kernel,
            rootfs: &rig.rootfs,
            key: "k",
            paths: &rig.paths,
            vm_id: "bake-test-vm".to_string(),
            vsock_uds_path: rig.uds.clone(),
            timeouts,
        }
    }

    fn short_timeouts() -> BakeTimeouts {
        BakeTimeouts {
            registration: std::time::Duration::from_millis(400),
            ready: std::time::Duration::from_millis(400),
        }
    }

    /// The Firecracker mux's base socket plus a guest that answers (or not)
    /// `PrepareForSnapshot` on port 52. Returns whether the guest was asked.
    fn spawn_fake_guest_mux(base: &Path, answer: bool) -> Arc<std::sync::atomic::AtomicBool> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let asked = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let asked_task = asked.clone();
        let listener = tokio::net::UnixListener::bind(base).unwrap();
        tokio::spawn(async move {
            let Ok((mut s, _)) = listener.accept().await else {
                return;
            };
            let mut b = [0u8; 1];
            loop {
                if s.read_exact(&mut b).await.is_err() || b[0] == b'\n' {
                    break;
                }
            }
            let _ = s.write_all(b"OK 52\n").await;
            let Ok(req) = vam_vsock::frame::Frame::read_from(&mut s).await else {
                return;
            };
            asked_task.store(true, std::sync::atomic::Ordering::SeqCst);
            if !answer {
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                return;
            }
            use prost::Message;
            let mut env = Vec::new();
            vam_spec::v1::VsockEnvelope {
                method: "PrepareForSnapshotResponse".to_string(),
                correlation_id: req.correlation_id,
                payload: Vec::new(),
                error: None,
                trace_context: None,
            }
            .encode(&mut env)
            .unwrap();
            let _ = vam_vsock::frame::Frame {
                frame_type: vam_vsock::frame::FrameType::ControlGrpc,
                correlation_id: req.correlation_id,
                payload: bytes::Bytes::from(env),
            }
            .write_to(&mut s)
            .await;
        });
        asked
    }

    fn history_of(h: &History) -> Vec<String> {
        h.lock().unwrap().clone()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn bake_aborts_cleanly_when_the_guest_never_registers() {
        if !mke2fs_available() {
            eprintln!("skipping: mke2fs not found");
            return;
        }
        let rig = bake_rig();
        let history: History = Default::default();
        // A pid that is not the mock's: no stand-in guest, so nobody registers.
        let job = rig_job(&rig, mock_factory(Some(4242), &history), short_timeouts());
        let res = tokio::time::timeout(std::time::Duration::from_secs(10), bake_into(job))
            .await
            .expect("the bake must give up, not hang");
        let err = res.unwrap_err().to_string();
        assert!(err.contains("registration timed out"), "{err}");
        let h = history_of(&history);
        assert!(h.iter().any(|a| a == "shutdown"), "VM shut down: {h:?}");
        assert!(!h.iter().any(|a| a == "pause"), "no pause: {h:?}");
        assert!(!rig.paths.vm_state.exists() && !rig.paths.key_json.exists());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn bake_does_not_snapshot_a_guest_that_never_becomes_ready() {
        if !mke2fs_available() {
            eprintln!("skipping: mke2fs not found");
            return;
        }
        let rig = bake_rig();
        let history: History = Default::default();
        let _asked = spawn_fake_guest_mux(&rig.uds, false);
        let _guest = spawn_stand_in_guest(
            "bake-test-vm".to_string(),
            format!("{}_52", rig.uds.display()),
        );
        let job = rig_job(&rig, mock_factory(Some(4242), &history), short_timeouts());
        let res = tokio::time::timeout(std::time::Duration::from_secs(10), bake_into(job))
            .await
            .expect("the bake must give up, not hang");
        let err = res.unwrap_err().to_string();
        assert!(err.contains("not ready to snapshot"), "{err}");
        let h = history_of(&history);
        assert!(h.iter().any(|a| a == "shutdown"), "VM shut down: {h:?}");
        assert!(
            !h.iter()
                .any(|a| a == "pause" || a.starts_with("create_snapshot")),
            "a guest that is not ready is never paused or snapshotted: {h:?}"
        );
        assert!(!rig.paths.key_json.exists(), "nothing is published");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn bake_pauses_only_after_the_guest_says_it_is_ready() {
        if !mke2fs_available() {
            eprintln!("skipping: mke2fs not found");
            return;
        }
        let rig = bake_rig();
        let history: History = Default::default();
        let asked = spawn_fake_guest_mux(&rig.uds, true);
        let _guest = spawn_stand_in_guest(
            "bake-test-vm".to_string(),
            format!("{}_52", rig.uds.display()),
        );
        // The mock snapshot writes nothing; stand in for Firecracker's files.
        fs::write(&rig.paths.vm_state, b"s").unwrap();
        fs::write(&rig.paths.vm_mem, b"m").unwrap();
        let job = rig_job(
            &rig,
            mock_factory(Some(4242), &history),
            BakeTimeouts {
                registration: std::time::Duration::from_secs(5),
                ready: std::time::Duration::from_secs(5),
            },
        );
        let timing = tokio::time::timeout(std::time::Duration::from_secs(15), bake_into(job))
            .await
            .expect("bake finishes")
            .unwrap();
        assert!(asked.load(std::sync::atomic::Ordering::SeqCst));
        let h = history_of(&history);
        let pos = |what: &str| h.iter().position(|a| a.starts_with(what));
        assert!(pos("pause") < pos("create_snapshot"), "{h:?}");
        assert!(pos("create_snapshot") < pos("shutdown"), "{h:?}");
        assert!(timing.ready_ms >= timing.registered_ms);
        assert!(golden_files_valid(&rig.paths, "k"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn mock_vmm_bake_has_no_guest_to_ask() {
        if !mke2fs_available() {
            eprintln!("skipping: mke2fs not found");
            return;
        }
        let rig = bake_rig();
        let history: History = Default::default();
        fs::write(&rig.paths.vm_state, b"s").unwrap();
        fs::write(&rig.paths.vm_mem, b"m").unwrap();
        let job = rig_job(
            &rig,
            mock_factory(Some(MOCK_VMM_PID), &history),
            BakeTimeouts {
                registration: std::time::Duration::from_secs(5),
                ready: std::time::Duration::from_millis(50),
            },
        );
        tokio::time::timeout(std::time::Duration::from_secs(15), bake_into(job))
            .await
            .expect("bake finishes")
            .unwrap();
        assert!(golden_files_valid(&rig.paths, "k"));
    }

    // ---- restore with a mock VMM ------------------------------------------------

    #[test]
    fn restore_stages_then_loads_then_patches_then_resumes() {
        let dir = tempfile::tempdir().unwrap();
        let paths = golden_paths_for_key(dir.path(), "k");
        fs::create_dir_all(&paths.dir).unwrap();
        fs::write(&paths.vm_state, b"s").unwrap();
        fs::write(&paths.vm_mem, b"m").unwrap();
        create_sparse_file(&paths.companion_disk, 8192).unwrap();
        let candidate = GoldenRestoreCandidate {
            meta: GoldenKeyMeta {
                key: "k".into(),
                kernel_hash: String::new(),
                rootfs_hash: String::new(),
                vcpu_count: GOLDEN_VCPU,
                mem_mib: GOLDEN_MEM_MIB,
                guest_cid: GOLDEN_GUEST_CID,
            },
            paths: paths.clone(),
            guest_cid: GOLDEN_GUEST_CID,
        };
        let history: History = Default::default();
        let mut vmm = vam_core::MockVmm::with_shared_history(history.clone());
        restore_from_golden(
            &mut vmm,
            &candidate,
            Path::new("/session.ext4"),
            Path::new("/run/vam-vsock-x.socket"),
            None,
        )
        .unwrap();
        let h = history_of(&history);
        let pos = |what: &str| h.iter().position(|a| a.starts_with(what)).unwrap();
        assert!(pos("spawn") < pos("stage_snapshot_restore"), "{h:?}");
        assert!(
            pos("stage_snapshot_restore") < pos("load_snapshot:"),
            "{h:?}"
        );
        assert!(
            pos("load_snapshot:") < pos("patch_data_drive:data0:"),
            "{h:?}"
        );
        assert!(
            pos("patch_data_drive:data0:") < pos("resume"),
            "the golden disk stand-in goes before resume: {h:?}"
        );
        let stage = &h[pos("stage_snapshot_restore")];
        assert!(
            stage.ends_with(":8192"),
            "the stand-in has the golden disk's length: {stage}"
        );
        assert!(stage.contains(&paths.vm_mem.display().to_string()));
    }

    #[test]
    fn a_failed_stage_stops_the_restore_before_the_guest_can_run() {
        let dir = tempfile::tempdir().unwrap();
        let paths = golden_paths_for_key(dir.path(), "k");
        fs::create_dir_all(&paths.dir).unwrap();
        // No companion disk: the restore must fail (and the caller boots cold).
        let candidate = GoldenRestoreCandidate {
            meta: GoldenKeyMeta {
                key: "k".into(),
                kernel_hash: String::new(),
                rootfs_hash: String::new(),
                vcpu_count: GOLDEN_VCPU,
                mem_mib: GOLDEN_MEM_MIB,
                guest_cid: GOLDEN_GUEST_CID,
            },
            paths,
            guest_cid: GOLDEN_GUEST_CID,
        };
        let history: History = Default::default();
        let mut vmm = vam_core::MockVmm::with_shared_history(history.clone());
        let err = restore_from_golden(
            &mut vmm,
            &candidate,
            Path::new("/s.ext4"),
            Path::new("/u"),
            None,
        )
        .unwrap_err();
        assert!(err.to_string().contains("golden disk"), "{err}");
        let h = history_of(&history);
        assert!(
            !h.iter()
                .any(|a| a.starts_with("load_snapshot") || a == "resume"),
            "{h:?}"
        );
    }

    #[test]
    fn a_jailed_node_only_restores_files_the_vmm_cannot_write() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let paths = golden_paths_for_key(dir.path(), "k");
        fs::create_dir_all(&paths.dir).unwrap();
        fs::write(&paths.vm_state, b"s").unwrap();
        fs::write(&paths.vm_mem, b"m").unwrap();
        let mode = |m: u32| {
            for f in [&paths.vm_state, &paths.vm_mem] {
                fs::set_permissions(f, fs::Permissions::from_mode(m)).unwrap();
            }
        };
        let me = {
            use std::os::unix::fs::MetadataExt;
            fs::metadata(&paths.vm_mem).unwrap().uid()
        };
        mode(0o644);
        assert!(check_golden_files_for_jail(&paths, me + 1).is_ok());
        // Owned by the jail uid: the VMM could rewrite it.
        assert!(matches!(
            check_golden_files_for_jail(&paths, me),
            Err(ColdBootReason::UnsafeGoldenFile(_))
        ));
        mode(0o666);
        assert!(matches!(
            check_golden_files_for_jail(&paths, me + 1),
            Err(ColdBootReason::UnsafeGoldenFile(_))
        ));
        mode(0o600);
        assert!(
            check_golden_files_for_jail(&paths, me + 1).is_err(),
            "must be readable by the VMM"
        );
    }

    // ---- soak ---------------------------------------------------------------

    #[test]
    fn soak_rows_print_in_the_documented_form() {
        let ok = SoakRow {
            k: 2,
            n: 5,
            ready_ms: Some(812),
            reseed: Some("44".into()),
            ok: true,
        };
        assert_eq!(ok.to_string(), "bake soak 2/5: ready=812 reseed=44 ok=true");
        let bad = SoakRow {
            k: 3,
            n: 5,
            ready_ms: Some(790),
            reseed: Some("timeout".into()),
            ok: false,
        };
        assert_eq!(
            bad.to_string(),
            "bake soak 3/5: ready=790 reseed=timeout ok=false"
        );
        let none = SoakRow {
            k: 1,
            n: 1,
            ready_ms: None,
            reseed: None,
            ok: false,
        };
        assert_eq!(none.to_string(), "bake soak 1/1: ready=- reseed=- ok=false");
    }

    #[test]
    fn soak_is_off_unless_asked_for() {
        // One test owns the variable so parallel tests cannot race on it.
        std::env::remove_var("VAM_GOLDEN_BAKE_SOAK");
        assert_eq!(bake_soak_count(), None);
        for (v, want) in [
            ("", None),
            ("0", None),
            ("soon", None),
            ("-3", None),
            ("5", Some(5)),
            (" 5 ", Some(5)),
            ("500", Some(20)),
        ] {
            std::env::set_var("VAM_GOLDEN_BAKE_SOAK", v);
            assert_eq!(bake_soak_count(), want, "VAM_GOLDEN_BAKE_SOAK={v:?}");
        }
        std::env::remove_var("VAM_GOLDEN_BAKE_SOAK");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn soak_never_touches_the_real_snapshot_and_cleans_up() {
        let root = tempfile::tempdir().unwrap();
        // A real golden snapshot directory the soak must leave alone.
        let real = golden_paths_for_key(&golden_root(root.path()), "realkey");
        fs::create_dir_all(&real.dir).unwrap();
        fs::write(&real.vm_state, b"state").unwrap();
        fs::write(&real.vm_mem, b"mem").unwrap();
        let history: History = Default::default();
        // The mock snapshot writes no files, so each soak bake fails: the point is
        // that failure leaves no scratch behind and never reaches `golden/`.
        run_bake_soak(mock_factory(Some(MOCK_VMM_PID), &history), root.path(), 2).await;
        assert_eq!(fs::read(&real.vm_state).unwrap(), b"state");
        assert_eq!(fs::read(&real.vm_mem).unwrap(), b"mem");
        assert!(!root.path().join("golden-soak").exists(), "scratch removed");
        let entries: Vec<_> = fs::read_dir(golden_root(root.path()))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(entries.len(), 1, "no new snapshot appeared: {entries:?}");
    }
}
