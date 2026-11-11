use crate::error::OrchestratorError;
use crate::timing::{mark_shared, merge_shared, set_kernel_shared, SharedPhaseTimer};
use crate::workflow::SqliteWorkflowStore;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};

/// Lease generation a golden-restored guest is fenced to; the session row is stamped to match.
const RESTORE_LEASE_GENERATION: i64 = 2;
use vam_core::CidAllocator;
use vam_network::{
    KernelNetAdapter, LinuxKernelNetAdapter, MockKernelAdapter, NetResourcePool, NetworkMode,
    TapLeaseOutcome,
};

use crate::VmmFactory;

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub enum SnapshotKind {
    Cold,
    Warm,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SnapshotMetadata {
    pub snapshot_id: String,
    pub session_id: String,
    pub workspace_id: String,
    pub parent_snapshot_id: Option<String>,
    pub kind: SnapshotKind,
    pub drive9_manifest_hash: String,
    pub guest_cid: Option<u32>,
    pub tap_name: Option<String>,
    pub netns: Option<String>,
    pub fc_version: Option<String>,
    pub created_at: i64,
}

fn release_session_cid(cid_allocator: &CidAllocator, cid: u32, allocated: bool) {
    if allocated {
        cid_allocator.release(cid);
    }
}

/// A handle on the running VMM `pid` of session VM `vm_id`, built by the
/// configured factory (so a process-guest node never reaches Firecracker code)
/// and then adopted. An adopted Firecracker VMM keeps the node's jailer setting: a
/// jailed VM's teardown also removes its jail directory.
pub(crate) fn adopted_vmm(
    vmm_factory: &VmmFactory,
    vm_id: &str,
    cid: u32,
    pid: u32,
) -> Box<dyn vam_core::Vmm> {
    let mut vmm = vmm_factory(vm_id, cid);
    vmm.adopt(pid);
    vmm
}

/// Kills the Firecracker process of a session whose guest never registered and
/// removes the files it left in `/tmp`. Without this a failed boot leaves a
/// running VM that nothing tracks.
fn kill_unregistered_vmm(vmm_factory: &VmmFactory, vm_id: &str, cid: u32, pid: u32) {
    let result = adopted_vmm(vmm_factory, vm_id, cid, pid).shutdown();
    if let Err(e) = result {
        eprintln!(
            "[orchestrator] failed to stop VMM pid {} for unregistered session VM '{}': {}",
            pid, vm_id, e
        );
    }
}

fn remove_session_vsock_sockets(vsock_uds_path: &std::path::Path) {
    for port in [52u32, 53, 54, 55] {
        let _ = std::fs::remove_file(format!("{}_{port}", vsock_uds_path.display()));
    }
}

/// Binds the four per-session host vsock listeners (ports 52 to 55).
async fn bind_host_listeners(
    vsock_uds_path: &std::path::Path,
) -> std::io::Result<[vam_vsock::HostListener; 4]> {
    Ok([
        vam_vsock::HostListener::bind(vsock_uds_path, 52).await?,
        vam_vsock::HostListener::bind(vsock_uds_path, 53).await?,
        vam_vsock::HostListener::bind(vsock_uds_path, 54).await?,
        vam_vsock::HostListener::bind(vsock_uds_path, 55).await?,
    ])
}

fn mint_session_ca(
    session_id: &str,
) -> Result<Arc<vam_network::tls_ca::SessionCa>, OrchestratorError> {
    vam_network::tls_ca::SessionCa::mint(session_id)
        .map(Arc::new)
        .map_err(|err| OrchestratorError::Core(format!("failed to mint session CA: {err}")))
}

/// Directory that holds per-snapshot state. `VAM_SNAPSHOT_ROOT` overrides the
/// production default `/var/lib/vam/snapshots`, so tests can run as a non-root user.
pub(crate) fn snapshot_root() -> std::path::PathBuf {
    match std::env::var("VAM_SNAPSHOT_ROOT") {
        Ok(val) => std::path::PathBuf::from(val),
        Err(_) => std::path::PathBuf::from("/var/lib/vam/snapshots"),
    }
}

pub(crate) fn drive9_root() -> std::path::PathBuf {
    if let Ok(val) = std::env::var("VAM_DRIVE9_ROOT") {
        return std::path::PathBuf::from(val);
    }
    let default_path = std::path::PathBuf::from("/var/lib/vam/drive9");
    // `||` short-circuits: create_dir_all runs only when the path is missing, as before.
    if default_path.exists() || std::fs::create_dir_all(&default_path).is_ok() {
        default_path
    } else {
        let fallback = std::path::PathBuf::from("/tmp/vam/drive9");
        let _ = std::fs::create_dir_all(&fallback);
        fallback
    }
}

/// Opens the hoisted Drive9 stack and catalog cache for the manager loop.
pub fn drive9_runtime() -> Result<
    (
        Arc<vam_storage::WorkspaceManager>,
        Arc<crate::session::ToolCatalogCache>,
    ),
    OrchestratorError,
> {
    Ok((
        open_drive9_stack()?,
        Arc::new(crate::session::ToolCatalogCache::new()),
    ))
}

/// [`drive9_runtime`] with an explicit Drive9 backend. Only process-guest
/// vamd passes `Directory`; every other caller uses [`drive9_runtime`].
#[cfg(feature = "process-guest")]
pub fn drive9_runtime_with(
    backend: vam_storage::Drive9Backend,
) -> Result<
    (
        Arc<vam_storage::WorkspaceManager>,
        Arc<crate::session::ToolCatalogCache>,
    ),
    OrchestratorError,
> {
    let manager = vam_storage::open_workspace_manager_with(&drive9_root(), backend)
        .map(Arc::new)
        .map_err(|e| OrchestratorError::Core(format!("Failed to init Drive9 stack: {}", e)))?;
    Ok((manager, Arc::new(crate::session::ToolCatalogCache::new())))
}

/// Hoists network capability probing and the TAP pool at daemon startup.
pub fn net_pool_for_runtime() -> Arc<NetResourcePool> {
    Arc::new(NetResourcePool::new(
        vam_network::NetworkCapabilities::probe(),
    ))
}

fn open_drive9_stack() -> Result<Arc<vam_storage::WorkspaceManager>, OrchestratorError> {
    vam_storage::open_workspace_manager(&drive9_root())
        .map(Arc::new)
        .map_err(|e| OrchestratorError::Core(format!("Failed to init Drive9 stack: {}", e)))
}

/// Opens a hoisted Drive9 workspace manager for tests (uses explicit root, not env).
pub fn open_drive9_stack_at(
    drive9_root: &std::path::Path,
) -> Result<Arc<vam_storage::WorkspaceManager>, OrchestratorError> {
    vam_storage::open_workspace_manager(drive9_root)
        .map(Arc::new)
        .map_err(|e| OrchestratorError::Core(format!("Failed to init Drive9 stack: {}", e)))
}

/// Where example seeds come from: `VAMD_EXAMPLES_DIR` (or its older name `VAM_EXAMPLES_DIR`).
/// Unset: the dev tree's `examples/` when run under cargo (`CARGO_MANIFEST_DIR` is set only by
/// cargo, so a deployed node has none), else no seeds.
fn examples_base_dir() -> Option<std::path::PathBuf> {
    for name in ["VAMD_EXAMPLES_DIR", "VAM_EXAMPLES_DIR"] {
        if let Some(val) = std::env::var_os(name).filter(|v| !v.is_empty()) {
            return Some(std::path::PathBuf::from(val));
        }
    }
    let manifest = std::env::var_os("CARGO_MANIFEST_DIR")?;
    let dev = std::path::PathBuf::from(manifest).join("../../examples");
    dev.is_dir().then_some(dev)
}

/// The Space's label as the host reads it for seeding: a workspace is stored under its `sp_…`
/// id, which says nothing about its content, so the seed follows the label the Space was
/// created with (the owner of the session holds the Space). A workspace id that is no Space
/// (tests, sessions made before a Space) is its own label.
pub(crate) fn seed_label(
    store: &crate::workflow::SqliteWorkflowStore,
    session_id: &str,
    workspace_id: &str,
) -> String {
    store
        .session_owner_user_id(session_id)
        .ok()
        .and_then(|owner| store.get_space(&owner, workspace_id).ok())
        .map(|space| space.label)
        .unwrap_or_else(|| workspace_id.to_string())
}

/// The example seed directory for a Space label.
pub(crate) fn resolve_seed_dir(label: &str) -> Option<std::path::PathBuf> {
    let workspace_id = label;
    if workspace_id.contains('/') || workspace_id.contains("..") {
        return None;
    }
    let examples_base = examples_base_dir()?;
    let direct = examples_base.join(workspace_id);
    let suffixed = examples_base.join(format!("{}-assistant", workspace_id));
    if direct.is_dir() {
        Some(direct)
    } else if suffixed.is_dir() {
        Some(suffixed)
    } else if workspace_id == "assistant" || workspace_id.starts_with("assistant") {
        Some(examples_base.join("bun-assistant"))
    } else if workspace_id.starts_with("sqlite") {
        Some(examples_base.join("sqlite-assistant"))
    } else if workspace_id.starts_with("jira") {
        Some(examples_base.join("jira-assistant"))
    } else {
        None
    }
}

/// Sentinel principal used only when identity cannot be established.
/// Builtins-only Cedar permits any principal, so this never synthesizes a workspace URN.
pub const UNRESOLVED_AGENT_URN: &str = "urn:vam:agent:unresolved";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentIdentity {
    Resolved(String),
    Unresolved,
}

impl AgentIdentity {
    pub fn principal_urn(&self) -> &str {
        match self {
            Self::Resolved(urn) => urn.as_str(),
            Self::Unresolved => UNRESOLVED_AGENT_URN,
        }
    }

    pub fn as_resolved(&self) -> Option<&str> {
        match self {
            Self::Resolved(urn) => Some(urn.as_str()),
            Self::Unresolved => None,
        }
    }
}

fn exact_seed_dir(label: &str) -> Option<std::path::PathBuf> {
    let workspace_id = label;
    if workspace_id.contains('/') || workspace_id.contains("..") {
        return None;
    }
    let dir = examples_base_dir()?.join(workspace_id);
    if dir.is_dir() {
        Some(dir)
    } else {
        None
    }
}

fn agent_urn_from_agent_json(seed_dir: &std::path::Path) -> Option<String> {
    let content = std::fs::read_to_string(seed_dir.join("agent.json")).ok()?;
    let val: serde_json::Value = serde_json::from_str(&content).ok()?;
    val.get("resourceId")
        .or_else(|| val.get("resource_id"))
        .and_then(|r| r.get("urn"))
        .and_then(|u| u.as_str())
        .filter(|u| !u.is_empty())
        .map(str::to_string)
}

/// Resolve the agent principal: run binding first, then exact-name seed `agent.json`.
/// Does not synthesize a workspace-id URN and does not use `resolve_seed_dir` heuristics.
pub(crate) fn resolve_agent_identity(
    store: &crate::workflow::SqliteWorkflowStore,
    session_id: &str,
    workspace_id: &str,
) -> Result<AgentIdentity, OrchestratorError> {
    if let Some(urn) = store.find_agent_urn_for_session(session_id)? {
        if !urn.is_empty() {
            return Ok(AgentIdentity::Resolved(urn));
        }
    }
    if let Some(dir) = exact_seed_dir(&seed_label(store, session_id, workspace_id)) {
        if let Some(urn) = agent_urn_from_agent_json(&dir) {
            return Ok(AgentIdentity::Resolved(urn));
        }
    }
    Ok(AgentIdentity::Unresolved)
}

/// The agent a session's brokers enforce, failing closed for a run's session.
///
/// A session that belongs to a run takes its agent only from that run's
/// recorded `agent_urn`. If the run has none, this is an error: the session
/// must not start, and it never falls back to the builtins-only policy or to a
/// seed directory (which could name a different agent). Only a session with no
/// run (`POST /sessions`, snapshot restore) uses [`resolve_agent_identity`].
pub(crate) fn resolve_session_identity(
    store: &crate::workflow::SqliteWorkflowStore,
    session_id: &str,
    workspace_id: &str,
) -> Result<AgentIdentity, OrchestratorError> {
    match store.run_binding_for_session(session_id)? {
        Some((run_id, agent_urn)) => match agent_urn.filter(|urn| !urn.is_empty()) {
            Some(urn) => Ok(AgentIdentity::Resolved(urn)),
            None => Err(OrchestratorError::CorruptState(format!(
                "session '{session_id}' belongs to run '{run_id}' but the run records no agent; \
                 refusing to start the session (a run's session never runs without its agent's policy)"
            ))),
        },
        None => resolve_agent_identity(store, session_id, workspace_id),
    }
}

/// Restores workspace HEAD from the latest snapshot only when HEAD is missing (crash recovery).
fn restore_head_from_snapshot_if_missing_at(
    store: &SqliteWorkflowStore,
    workspace_id: &str,
    drive9_root_path: &std::path::Path,
) -> Result<(), OrchestratorError> {
    let drive9_config = vam_storage::Drive9Config::new(drive9_root_path);
    let layout = drive9_config.layout();
    let chunk_store = vam_storage::ChunkStore::new(layout.clone());
    let manifest_manager = vam_storage::ManifestManager::new(layout.clone(), chunk_store.clone());

    if manifest_manager
        .read_head(workspace_id)
        .map_err(|e| OrchestratorError::Core(e.to_string()))?
        .is_some()
    {
        return Ok(());
    }

    let Some(snapshot) = store.find_latest_snapshot_for_workspace(workspace_id)? else {
        return Ok(());
    };

    manifest_manager
        .write_head(workspace_id, &snapshot.drive9_manifest_hash)
        .map_err(|e| OrchestratorError::Core(format!("restore HEAD failed: {}", e)))?;
    eprintln!(
        "[orchestrator] restored workspace '{}' HEAD from snapshot {} (manifest {})",
        workspace_id, snapshot.snapshot_id, snapshot.drive9_manifest_hash
    );
    Ok(())
}

fn restore_head_from_snapshot_if_missing(
    store: &SqliteWorkflowStore,
    workspace_id: &str,
) -> Result<(), OrchestratorError> {
    restore_head_from_snapshot_if_missing_at(store, workspace_id, &drive9_root())
}

/// Records a Cold snapshot from an already-committed HEAD manifest (no live-disk extract).
pub fn record_workspace_snapshot(
    store: &SqliteWorkflowStore,
    session_id: &str,
    workspace_id: &str,
    snapshot_id: &str,
    manifest_hash: &str,
    guest_cid: Option<u32>,
    tap_name: Option<String>,
) -> Result<SnapshotMetadata, OrchestratorError> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    let meta = SnapshotMetadata {
        snapshot_id: snapshot_id.to_string(),
        session_id: session_id.to_string(),
        workspace_id: workspace_id.to_string(),
        parent_snapshot_id: None,
        kind: SnapshotKind::Cold,
        drive9_manifest_hash: manifest_hash.to_string(),
        guest_cid,
        tap_name,
        netns: None,
        fc_version: Some("v1.5.0".to_string()),
        created_at: now,
    };

    store.record_snapshot(&meta)?;
    Ok(meta)
}

fn discard_workspace_lease(
    workspace_manager: &vam_storage::WorkspaceManager,
    session_id: &str,
    workspace_id: &str,
) {
    if let Err(e) = workspace_manager.discard(session_id, workspace_id) {
        eprintln!(
            "[orchestrator] failed to discard workspace '{}' for session '{}': {:?}",
            workspace_id, session_id, e
        );
        let _ = vam_storage::release_workspace_lock(
            workspace_manager.layout(),
            workspace_id,
            Some(session_id),
        );
    }
}

fn session_vm_is_live(session: &crate::session::SessionRecord) -> bool {
    let Some(pid) = session.vmm_pid else {
        return false;
    };
    if pid == 12345 {
        return true;
    }
    match crate::reconcile::get_process_start_time(pid) {
        Ok(st) => session.vmm_start_time == Some(st),
        Err(_) => false,
    }
}

/// The guest is still inside the keep-warm window.
///
/// A parked session is `Terminated` even when the mock VMM pid is still the
/// sentinel `12345`, so observed state is checked before the process.
pub(crate) fn session_is_warm(session: &crate::session::SessionRecord) -> bool {
    matches!(
        session.observed_state,
        crate::session::ObservedState::Running | crate::session::ObservedState::Paused
    ) && session_vm_is_live(session)
}

fn reclaim_stale_workspace_lock(
    store: &SqliteWorkflowStore,
    workspace_id: &str,
    new_session_id: &str,
) {
    let layout = vam_storage::Drive9Layout::new(drive9_root());
    reclaim_stale_workspace_lock_at(store, &layout, workspace_id, new_session_id);
}

fn reclaim_stale_workspace_lock_at(
    store: &SqliteWorkflowStore,
    layout: &vam_storage::Drive9Layout,
    workspace_id: &str,
    new_session_id: &str,
) {
    let Ok(Some(holder)) = vam_storage::read_workspace_lock_holder(layout, workspace_id) else {
        return;
    };
    if holder == new_session_id {
        return;
    }
    // A stopped session's tree is settled by its run's finalizer; its lock is
    // not stale until then.
    if store.session_ends.settle_pending(&holder) {
        return;
    }
    let should_reclaim = match store.load_session(&holder) {
        Ok(session) => {
            let terminal = matches!(
                session.observed_state,
                crate::session::ObservedState::Terminated
                    | crate::session::ObservedState::Failed
                    | crate::session::ObservedState::Terminating
            );
            terminal || !session_vm_is_live(&session)
        }
        Err(_) => true,
    };
    if should_reclaim {
        let _ = vam_storage::release_workspace_lock(layout, workspace_id, Some(&holder));
        eprintln!(
            "[orchestrator] reclaimed stale workspace lock on '{}' from session '{}'",
            workspace_id, holder
        );
    }
}

/// Drive9 layout the session manager uses for workspace write locks.
///
/// Callers that release locks outside the manager (for example the expiry
/// sweeper) should use this so they hit the same root as `drive9_runtime()`.
pub fn drive9_layout() -> vam_storage::Drive9Layout {
    vam_storage::Drive9Layout::new(drive9_root())
}

/// Expire stale HITL approvals and tear down the sessions they parked.
///
/// For each approval the store moves to `EXPIRED`:
/// - terminates its session (idempotent: an already-terminated session is `Ok`)
/// - if termination succeeded, releases the session's workspace write lock
///   (only if that session still holds it). A failed termination keeps the lock.
///
/// Expiry does not finalize the run: no trajectory, attestation, or evidence pack.
/// Errors on one approval are logged and do not stop the rest of the sweep.
/// Returns how many approvals were expired in this pass, so a second pass
/// over the same rows returns `0`.
pub async fn sweep_expired_approvals(
    store: &SqliteWorkflowStore,
    manager: &SessionManager,
    layout: &vam_storage::Drive9Layout,
) -> Result<usize, OrchestratorError> {
    let expired = store.expire_stale_approvals()?;
    let count = expired.len();
    // Expiry runs no finalizer, so the receipt is issued once the sessions are down.
    let run_ids: Vec<String> = expired.iter().map(|a| a.run_id.clone()).collect();
    for approval in expired {
        let Some(session_id) = approval.session_id else {
            continue;
        };
        if let Err(e) = manager.terminate_session(session_id.clone()).await {
            // Keep the write lock when the VM may still be running: releasing it
            // would let another session write while this one is alive. The lock
            // is reclaimed later once the session is terminal or its VM is dead.
            tracing::error!(
                "[orchestrator] expiry: failed to terminate session '{}' for approval '{}', keeping workspace lock: {}",
                session_id,
                approval.request_id,
                e
            );
            continue;
        }
        if let Some(workspace_id) = approval.workspace_id {
            if let Err(e) =
                vam_storage::release_workspace_lock(layout, &workspace_id, Some(&session_id))
            {
                tracing::error!(
                    "[orchestrator] expiry: failed to release workspace '{}' lock held by session '{}': {}",
                    workspace_id,
                    session_id,
                    e
                );
            }
        }
    }
    for run_id in run_ids {
        crate::receipt::issue_receipt_logged(store, &run_id, None);
    }
    Ok(count)
}

/// Fail runs whose active time has reached `RunSpec.max_duration`.
///
/// For each run the store has already marked `FAILED` and dropped its
/// workspace reservation. This pass terminates a still-warm VM and releases
/// that session's workspace write lock. Parked time does not count, so a
/// `PAUSED` run is not selected. A second pass returns `0`.
pub async fn sweep_active_duration(
    store: &SqliteWorkflowStore,
    manager: &SessionManager,
    layout: &vam_storage::Drive9Layout,
) -> Result<usize, OrchestratorError> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let expired = store.expire_over_active_budget(now)?;
    let count = expired.len();
    let run_ids: Vec<String> = expired.iter().map(|r| r.run_id.clone()).collect();
    for run in expired {
        let Some(session_id) = run.session_id else {
            continue;
        };
        if let Err(e) = manager.terminate_session(session_id.clone()).await {
            tracing::error!(
                "[orchestrator] active duration: failed to terminate session '{}' for run '{}', keeping workspace lock: {}",
                session_id,
                run.run_id,
                e
            );
            continue;
        }
        if let Some(workspace_id) = run.workspace_id {
            if let Err(e) =
                vam_storage::release_workspace_lock(layout, &workspace_id, Some(&session_id))
            {
                tracing::error!(
                    "[orchestrator] active duration: failed to release workspace '{}' lock held by session '{}': {}",
                    workspace_id,
                    session_id,
                    e
                );
            }
        }
    }
    for run_id in run_ids {
        crate::receipt::issue_receipt_logged(store, &run_id, None);
    }
    Ok(count)
}

/// Park paused runs whose keep-warm window elapsed with no decision.
///
/// For each due run:
/// - shuts the guest down gracefully (`Shutdown` flushes Drive9) and terminates
///   the session, via [`SessionManager::terminate_session`]
/// - releases that session's workspace write lock
/// - inserts a workspace reservation so `CreateRun` cannot take the workspace
///
/// The run stays `PAUSED`. A failed termination keeps the lock and does not
/// reserve, so the next pass retries. A second pass over an already parked
/// run returns `0`.
pub async fn sweep_keep_warm(
    store: &SqliteWorkflowStore,
    manager: &SessionManager,
    layout: &vam_storage::Drive9Layout,
) -> Result<usize, OrchestratorError> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let due = store.runs_due_to_park(now)?;
    let mut parked = 0usize;
    for candidate in due {
        let session = match store.load_session(&candidate.session_id) {
            Ok(session) => session,
            Err(e) => {
                tracing::error!(
                    "[orchestrator] keep-warm: session '{}' for run '{}' could not be loaded: {}",
                    candidate.session_id,
                    candidate.run_id,
                    e
                );
                continue;
            }
        };
        let terminal = matches!(
            session.observed_state,
            crate::session::ObservedState::Terminated | crate::session::ObservedState::Failed
        );
        // The parked session's recorder is of no use now: a resume boots a new
        // session with a new recorder.
        if terminal {
            store.remove_thread_recorder(&candidate.session_id);
        }
        if !terminal {
            if let Err(e) = manager
                .terminate_session(candidate.session_id.clone())
                .await
            {
                // The VM may still be running. Leave the write lock in place
                // and retry on the next pass. Do not reserve yet: a live
                // session is already rejected as AgentRunning.
                tracing::error!(
                    "[orchestrator] keep-warm: failed to terminate session '{}' for run '{}', keeping workspace lock: {}",
                    candidate.session_id,
                    candidate.run_id,
                    e
                );
                continue;
            }
            store.remove_thread_recorder(&candidate.session_id);
            if let Err(e) = vam_storage::release_workspace_lock(
                layout,
                &candidate.workspace_id,
                Some(&candidate.session_id),
            ) {
                tracing::error!(
                    "[orchestrator] keep-warm: failed to release workspace '{}' lock held by session '{}': {}",
                    candidate.workspace_id,
                    candidate.session_id,
                    e
                );
            }
        }

        match store.workspace_reservation(&candidate.workspace_id)? {
            Some(existing) if existing.run_id == candidate.run_id => {
                store.clear_keep_warm(&candidate.run_id)?;
                continue;
            }
            Some(existing) => {
                tracing::error!(
                    "[orchestrator] keep-warm: workspace '{}' is reserved by run '{}', not replacing it for '{}'",
                    candidate.workspace_id,
                    existing.run_id,
                    candidate.run_id
                );
                continue;
            }
            None => {}
        }

        store.reserve_workspace(
            &candidate.workspace_id,
            &candidate.run_id,
            candidate.reservation_expires_at,
        )?;
        store.clear_keep_warm(&candidate.run_id)?;
        parked += 1;
    }
    Ok(parked)
}

struct WorkspaceLeaseGuard {
    workspace_manager: Arc<vam_storage::WorkspaceManager>,
    session_id: String,
    workspace_id: String,
    keep: bool,
}

impl WorkspaceLeaseGuard {
    fn new(
        workspace_manager: Arc<vam_storage::WorkspaceManager>,
        session_id: String,
        workspace_id: String,
    ) -> Self {
        Self {
            workspace_manager,
            session_id,
            workspace_id,
            keep: false,
        }
    }
}

impl Drop for WorkspaceLeaseGuard {
    fn drop(&mut self) {
        if !self.keep {
            discard_workspace_lease(
                self.workspace_manager.as_ref(),
                &self.session_id,
                &self.workspace_id,
            );
        }
    }
}

/// How far a terminate goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminateMode {
    /// Tear down the VM and settle the workspace (commit, or keep on failure).
    Full,
    /// Tear down the VM and leave the tree and the workspace lock: a stop
    /// request, whose run's finalizer settles the workspace after it has
    /// written the end of the task (see `session_end`).
    VmOnly,
}

pub enum ManagerCommand {
    CreateSession {
        session_id: String,
        idempotency_key: Option<String>,
        vm_id: String,
        workspace_id: String,
        storage_profile_id: String,
        timer: Option<SharedPhaseTimer>,
        reply: oneshot::Sender<Result<(), OrchestratorError>>,
    },
    TerminateSession {
        session_id: String,
        mode: TerminateMode,
        reply: oneshot::Sender<Result<(), OrchestratorError>>,
    },
    PrepareEnd {
        session_id: String,
        disposition: crate::session_end::Disposition,
        reply: oneshot::Sender<Result<crate::session_end::PreparedSummary, OrchestratorError>>,
    },
    /// The user's stop of a run parked on an approval whose VM is gone: the end is
    /// written on the host, no VM is booted (AR-3.7).
    EndParked {
        end: crate::session_end::ParkedEnd,
        reply: oneshot::Sender<Result<crate::session_end::SettleOutcome, OrchestratorError>>,
    },
    CreateSnapshot {
        session_id: String,
        snapshot_id: String,
        kind: SnapshotKind,
        reply: oneshot::Sender<Result<SnapshotMetadata, OrchestratorError>>,
    },
    RestoreSession {
        snapshot_id: String,
        new_session_id: String,
        reply: oneshot::Sender<Result<(), OrchestratorError>>,
    },
    PauseSession {
        session_id: String,
        reply: oneshot::Sender<Result<(), OrchestratorError>>,
    },
    ResumeSession {
        session_id: String,
        reply: oneshot::Sender<Result<(), OrchestratorError>>,
    },
}

#[derive(Clone)]
pub struct SessionManager {
    cmd_tx: mpsc::Sender<ManagerCommand>,
}

impl SessionManager {
    pub fn new(
        _store: Arc<SqliteWorkflowStore>,
        _cid_allocator: Arc<CidAllocator>,
        _vmm_factory: VmmFactory,
    ) -> (Self, mpsc::Receiver<ManagerCommand>) {
        let (cmd_tx, cmd_rx) = mpsc::channel(100);
        (Self { cmd_tx }, cmd_rx)
    }

    pub async fn create_session(
        &self,
        session_id: String,
        idempotency_key: Option<String>,
        vm_id: String,
        workspace_id: String,
        storage_profile_id: String,
        timer: Option<SharedPhaseTimer>,
    ) -> Result<(), OrchestratorError> {
        let (reply, rx) = oneshot::channel();
        self.cmd_tx
            .send(ManagerCommand::CreateSession {
                session_id,
                idempotency_key,
                vm_id,
                workspace_id,
                storage_profile_id,
                timer,
                reply,
            })
            .await
            .map_err(|_| OrchestratorError::Core("SessionManager channel closed".to_string()))?;
        rx.await.map_err(|_| {
            OrchestratorError::Core("SessionManager reply channel closed".to_string())
        })?
    }

    pub async fn terminate_session(&self, session_id: String) -> Result<(), OrchestratorError> {
        self.terminate_session_with(session_id, TerminateMode::Full)
            .await
    }

    /// Takes the VM down for a stop request and leaves the workspace to the
    /// run's finalizer ([`TerminateMode::VmOnly`]).
    pub async fn stop_session_vm(&self, session_id: String) -> Result<(), OrchestratorError> {
        self.terminate_session_with(session_id, TerminateMode::VmOnly)
            .await
    }

    async fn terminate_session_with(
        &self,
        session_id: String,
        mode: TerminateMode,
    ) -> Result<(), OrchestratorError> {
        let (reply, rx) = oneshot::channel();
        self.cmd_tx
            .send(ManagerCommand::TerminateSession {
                session_id,
                mode,
                reply,
            })
            .await
            .map_err(|_| OrchestratorError::Core("SessionManager channel closed".to_string()))?;
        rx.await.map_err(|_| {
            OrchestratorError::Core("SessionManager reply channel closed".to_string())
        })?
    }

    /// First phase of a run's end: fixes the files the end commit holds, with
    /// the VM already down, and keeps the plan for the settle that follows
    /// (see `session_end`). The summary is what the finalizer writes into the
    /// conversation before the files are composed into HEAD.
    pub async fn prepare_session_end(
        &self,
        session_id: String,
        disposition: crate::session_end::Disposition,
    ) -> Result<crate::session_end::PreparedSummary, OrchestratorError> {
        let (reply, rx) = oneshot::channel();
        self.cmd_tx
            .send(ManagerCommand::PrepareEnd {
                session_id,
                disposition,
                reply,
            })
            .await
            .map_err(|_| OrchestratorError::Core("SessionManager channel closed".to_string()))?;
        rx.await.map_err(|_| {
            OrchestratorError::Core("SessionManager reply channel closed".to_string())
        })?
    }

    /// Ends a run parked on an approval on the host, with no VM
    /// ([`crate::session_end::end_parked_on_host`]).
    pub async fn end_parked_on_host(
        &self,
        end: crate::session_end::ParkedEnd,
    ) -> Result<crate::session_end::SettleOutcome, OrchestratorError> {
        let (reply, rx) = oneshot::channel();
        self.cmd_tx
            .send(ManagerCommand::EndParked { end, reply })
            .await
            .map_err(|_| OrchestratorError::Core("SessionManager channel closed".to_string()))?;
        rx.await.map_err(|_| {
            OrchestratorError::Core("SessionManager reply channel closed".to_string())
        })?
    }

    pub async fn create_snapshot(
        &self,
        session_id: String,
        snapshot_id: String,
        kind: SnapshotKind,
    ) -> Result<SnapshotMetadata, OrchestratorError> {
        let (reply, rx) = oneshot::channel();
        self.cmd_tx
            .send(ManagerCommand::CreateSnapshot {
                session_id,
                snapshot_id,
                kind,
                reply,
            })
            .await
            .map_err(|_| OrchestratorError::Core("SessionManager channel closed".to_string()))?;
        rx.await.map_err(|_| {
            OrchestratorError::Core("SessionManager reply channel closed".to_string())
        })?
    }

    pub async fn restore_session(
        &self,
        snapshot_id: String,
        new_session_id: String,
    ) -> Result<(), OrchestratorError> {
        let (reply, rx) = oneshot::channel();
        self.cmd_tx
            .send(ManagerCommand::RestoreSession {
                snapshot_id,
                new_session_id,
                reply,
            })
            .await
            .map_err(|_| OrchestratorError::Core("SessionManager channel closed".to_string()))?;
        rx.await.map_err(|_| {
            OrchestratorError::Core("SessionManager reply channel closed".to_string())
        })?
    }

    pub async fn pause_session(&self, session_id: String) -> Result<(), OrchestratorError> {
        let (reply, rx) = oneshot::channel();
        self.cmd_tx
            .send(ManagerCommand::PauseSession { session_id, reply })
            .await
            .map_err(|_| OrchestratorError::Core("SessionManager channel closed".to_string()))?;
        rx.await.map_err(|_| {
            OrchestratorError::Core("SessionManager reply channel closed".to_string())
        })?
    }

    pub async fn resume_session(&self, session_id: String) -> Result<(), OrchestratorError> {
        let (reply, rx) = oneshot::channel();
        self.cmd_tx
            .send(ManagerCommand::ResumeSession { session_id, reply })
            .await
            .map_err(|_| OrchestratorError::Core("SessionManager channel closed".to_string()))?;
        rx.await.map_err(|_| {
            OrchestratorError::Core("SessionManager reply channel closed".to_string())
        })?
    }
}

#[allow(clippy::too_many_arguments)] // flat parameter list is the established call shape; a params struct would change callers
pub async fn run_manager_loop(
    store: Arc<SqliteWorkflowStore>,
    vault: Arc<crate::secrets::SecretsVault>,
    cid_allocator: Arc<CidAllocator>,
    vmm_factory: VmmFactory,
    workspace_manager: Arc<vam_storage::WorkspaceManager>,
    catalog_cache: Arc<crate::session::ToolCatalogCache>,
    net_pool: Arc<NetResourcePool>,
    mut rx: mpsc::Receiver<ManagerCommand>,
    telemetry_capture: vam_telemetry::TelemetryCapture,
) {
    while let Some(cmd) = rx.recv().await {
        match cmd {
            ManagerCommand::CreateSession {
                session_id,
                idempotency_key,
                vm_id,
                workspace_id,
                storage_profile_id,
                timer,
                reply,
            } => {
                let res = handle_create_session(
                    store.clone(),
                    vault.clone(),
                    cid_allocator.clone(),
                    vmm_factory.clone(),
                    workspace_manager.clone(),
                    catalog_cache.clone(),
                    net_pool.clone(),
                    session_id,
                    idempotency_key,
                    vm_id,
                    workspace_id,
                    storage_profile_id,
                    timer,
                    telemetry_capture,
                )
                .await;
                let _ = reply.send(res);
            }
            ManagerCommand::TerminateSession {
                session_id,
                mode,
                reply,
            } => {
                let res = handle_terminate_session(
                    store.clone(),
                    cid_allocator.clone(),
                    vmm_factory.clone(),
                    workspace_manager.clone(),
                    net_pool.clone(),
                    session_id,
                    mode,
                )
                .await;
                let _ = reply.send(res);
            }
            ManagerCommand::PrepareEnd {
                session_id,
                disposition,
                reply,
            } => {
                let res = handle_prepare_end(
                    store.clone(),
                    workspace_manager.clone(),
                    session_id,
                    disposition,
                )
                .await;
                let _ = reply.send(res);
            }
            ManagerCommand::EndParked { end, reply } => {
                let res =
                    crate::session_end::end_parked_on_host(&store, &workspace_manager, end).await;
                let _ = reply.send(res);
            }
            ManagerCommand::CreateSnapshot {
                session_id,
                snapshot_id,
                kind,
                reply,
            } => {
                let res = handle_create_snapshot(
                    store.clone(),
                    cid_allocator.clone(),
                    vmm_factory.clone(),
                    workspace_manager.clone(),
                    session_id,
                    snapshot_id,
                    kind,
                )
                .await;
                let _ = reply.send(res);
            }
            ManagerCommand::RestoreSession {
                snapshot_id,
                new_session_id,
                reply,
            } => {
                let res = handle_restore_session(
                    store.clone(),
                    vault.clone(),
                    cid_allocator.clone(),
                    vmm_factory.clone(),
                    workspace_manager.clone(),
                    catalog_cache.clone(),
                    net_pool.clone(),
                    snapshot_id,
                    new_session_id,
                    telemetry_capture,
                )
                .await;
                let _ = reply.send(res);
            }
            ManagerCommand::PauseSession { session_id, reply } => {
                let res =
                    handle_pause_session(store.clone(), vmm_factory.clone(), session_id).await;
                let _ = reply.send(res);
            }
            ManagerCommand::ResumeSession { session_id, reply } => {
                let res =
                    handle_resume_session(store.clone(), vmm_factory.clone(), session_id).await;
                let _ = reply.send(res);
            }
        }
    }
}

#[allow(clippy::too_many_arguments)] // flat parameter list is the established call shape; a params struct would change callers
async fn handle_create_session(
    store: Arc<SqliteWorkflowStore>,
    vault: Arc<crate::secrets::SecretsVault>,
    cid_allocator: Arc<CidAllocator>,
    vmm_factory: VmmFactory,
    workspace_manager: Arc<vam_storage::WorkspaceManager>,
    catalog_cache: Arc<crate::session::ToolCatalogCache>,
    net_pool: Arc<NetResourcePool>,
    session_id: String,
    idempotency_key: Option<String>,
    vm_id: String,
    workspace_id: String,
    storage_profile_id: String,
    timer: Option<SharedPhaseTimer>,
    telemetry_capture: vam_telemetry::TelemetryCapture,
) -> Result<(), OrchestratorError> {
    let boot_start_instant = std::time::Instant::now();
    // 1. Commit row to DB first (CREATING state). Satisfies O05 (durable before physical).
    let store_clone = store.clone();
    let session_id_clone = session_id.clone();
    let idempotency_key_clone = idempotency_key.clone();
    let vm_id_for_db = vm_id.clone();
    let vm_id_for_config = vm_id.clone();
    let workspace_id_clone = workspace_id.clone();
    let storage_profile_id_clone = storage_profile_id.clone();

    let maybe_existing = tokio::task::spawn_blocking(move || {
        store_clone.create_session(
            &session_id_clone,
            idempotency_key_clone.as_deref(),
            &vm_id_for_db,
            &workspace_id_clone,
            &storage_profile_id_clone,
        )
    })
    .await
    .map_err(|_| OrchestratorError::Core("Tokio task panic".to_string()))??;

    mark_shared(&timer, "sqlite_create_session");

    if let Some(existing) = maybe_existing {
        if !matches!(
            existing.observed_state,
            crate::session::ObservedState::Creating | crate::session::ObservedState::Booting
        ) {
            return Ok(());
        }
    }

    // Preflight before any CID, TAP or VM exists: a run's session whose agent
    // cannot be resolved fails closed here. `spawn_session_brokers` resolves
    // again and is the authoritative gate.
    if let Err(err) = resolve_session_identity(&store, &session_id, &workspace_id) {
        eprintln!("[orchestrator] session {session_id} refused: {err}");
        let _ = store.update_observed_state(&session_id, crate::session::ObservedState::Failed);
        return Err(err);
    }

    reclaim_stale_workspace_lock(&store, &workspace_id, &session_id);
    let mut workspace_lease = WorkspaceLeaseGuard::new(
        workspace_manager.clone(),
        session_id.clone(),
        workspace_id.clone(),
    );

    // Crash recovery: restore HEAD from snapshot only when HEAD is missing.
    if let Err(e) = restore_head_from_snapshot_if_missing(&store, &workspace_id) {
        eprintln!(
            "[orchestrator] workspace '{}' HEAD restore skipped: {}",
            workspace_id, e
        );
    }

    // Drive9 materialization and ext4 image build
    let workspace_id_clone2 = workspace_id.clone();
    let session_id_clone2 = session_id.clone();
    let storage_profile_id_clone2 = storage_profile_id.clone();
    let timer_materialize = timer.clone();
    let workspace_manager_materialize = workspace_manager.clone();
    let seed_label = seed_label(&store, &session_id, &workspace_id);
    let seed_dir_for_materialize = resolve_seed_dir(&seed_label);

    let catalog_skills = {
        let session_dir = workspace_manager.layout().session_dir(&session_id);
        let overlay_root = session_dir.join("catalog-skills");
        std::fs::create_dir_all(&overlay_root).map_err(OrchestratorError::Io)?;
        let mcp_session_owner = store
            .session_owner_user_id(&session_id)
            .unwrap_or_else(|_| crate::users::LOCAL_USER_ID.to_string());
        // Write synthetic `hosted-<broker>` skills before Drive9 image build so the
        // guest catalog can `load_skill` them (seed_hsp after materialize is host-only).
        let web_enabled = crate::web_admission::session_web_enabled(&store, &session_id, None);
        crate::hsp_store::seed_hsp_skills(&overlay_root, &store, web_enabled)?;
        crate::mcp_registry::seed_mcp_skills(
            &overlay_root,
            &store,
            &session_id,
            &mcp_session_owner,
        )?;

        if let Some(agent_urn) = store.find_agent_urn_for_session(&session_id)? {
            let session_owner = store
                .session_owner_user_id(&session_id)
                .unwrap_or_else(|_| crate::users::LOCAL_USER_ID.to_string());
            let run_spec = store
                .find_run_id_for_session(&session_id)?
                .and_then(|run_id| store.get_run_spec_context(&run_id).ok())
                .and_then(|(json, _, _)| json)
                .and_then(|json| {
                    vam_spec::encoding::json_to_proto::<vam_spec::v1::RunSpec>(
                        "vam.v1.RunSpec",
                        &json,
                    )
                    .ok()
                });

            let mut agent = match &run_spec {
                Some(spec) => crate::catalog_resolve::load_agent_for_run(
                    &store,
                    spec,
                    &agent_urn,
                    &session_owner,
                    false,
                )?,
                None => store
                    .get_agent_visible(&agent_urn, &session_owner, false)
                    .or_else(|_| store.get_agent(&agent_urn))?,
            };
            if let Some(spec) = &run_spec {
                crate::catalog_resolve::overlay_resolved_refs(&mut agent, spec);
            }
            if !agent.skill_refs.is_empty() {
                // Same owner and mode the run recorded its pins with.
                let (resolve_owner, resolve_mode) = match &run_spec {
                    Some(spec) => {
                        crate::catalog_resolve::run_resolve_context(&store, spec, &session_owner)
                    }
                    None => (
                        session_owner.clone(),
                        crate::catalog_resolve::ResolveMode::Release,
                    ),
                };
                crate::skill_package_resolve::resolve_agent_skill_refs(
                    &store,
                    &agent,
                    &session_dir,
                    seed_dir_for_materialize.as_deref(),
                    &resolve_owner,
                    resolve_mode,
                )?;
            }
        }

        if overlay_root
            .read_dir()
            .map(|mut entries| entries.next().is_some())
            .unwrap_or(false)
        {
            Some(overlay_root)
        } else {
            None
        }
    };

    let drive9_image_path =
        tokio::task::spawn_blocking(move || -> Result<std::path::PathBuf, OrchestratorError> {
            let quota = if storage_profile_id_clone2 == "default" {
                1024 * 1024 * 1024 // 1 GiB
            } else {
                512 * 1024 * 1024 // 512 MiB
            };

            let profile =
                vam_storage::StorageProfile::new(storage_profile_id_clone2.clone(), quota);
            let seed_dir = resolve_seed_dir(&seed_label);

            let mut materialize_steps = Vec::new();
            let timing_sink = Some(&mut materialize_steps as &mut Vec<(&'static str, u128)>);
            let _session_dir = workspace_manager_materialize
                .materialize(
                    &session_id_clone2,
                    &workspace_id_clone2,
                    &profile,
                    seed_dir.as_deref(),
                    catalog_skills.as_deref(),
                    timing_sink,
                )
                .map_err(|e| {
                    OrchestratorError::Core(format!("Workspace materialization failed: {}", e))
                })?;

            merge_shared(&timer_materialize, materialize_steps);

            Ok(workspace_manager_materialize
                .layout()
                .session_image_path(&session_id_clone2))
        })
        .await
        .map_err(|_| OrchestratorError::Core("Tokio task panic".to_string()))
        .and_then(|r| r);

    mark_shared(&timer, "spawn_blocking_materialize");

    let drive9_image_path = match drive9_image_path {
        Ok(p) => p,
        Err(e) => {
            let _ = store.update_observed_state(&session_id, crate::session::ObservedState::Failed);
            return Err(e);
        }
    };

    // Seed host-authoritative tool catalog (builtins + materialized skills).
    {
        let store_catalog = store.clone();
        let session_id_catalog = session_id.clone();
        let catalog_cache_seed = catalog_cache.clone();
        let workspace_manager_catalog = workspace_manager.clone();
        let mcp_owner_catalog = store
            .session_owner_user_id(&session_id)
            .unwrap_or_else(|_| crate::users::LOCAL_USER_ID.to_string());
        let catalog_seed = tokio::task::spawn_blocking(move || -> Result<(), OrchestratorError> {
            let layout = workspace_manager_catalog.layout();
            let skills_dir = layout
                .session_dir(&session_id_catalog)
                .join("materialized/skills");
            // Seed synthetic HSP skills before digest/parse so guest scan_catalog can list them.
            let web_enabled = crate::web_admission::session_web_enabled(
                &store_catalog,
                &session_id_catalog,
                None,
            );
            // The directory backend sealed `skills/` read-only at materialize: unseal it just
            // for these rewrites (a non-root vamd gets EACCES otherwise; root does not).
            let mcp_plan = vam_storage::with_skills_writable(&skills_dir, || {
                crate::hsp_store::seed_hsp_skills(&skills_dir, &store_catalog, web_enabled)?;
                crate::mcp_registry::seed_mcp_skills(
                    &skills_dir,
                    &store_catalog,
                    &session_id_catalog,
                    &mcp_owner_catalog,
                )
            })?;
            let skills_digest = vam_storage::skills_digest_from_dir(&skills_dir);
            // `post_message` and `show` join the builtins, so a skill or MCP tool of the
            // same name fails closed in the merge.
            let mut builtins = crate::session::builtin_tool_catalog();
            builtins.extend(crate::outbox::catalog_entries());
            // In the catalog only for an agent with an explicit allow rule (a same-name skill tool fails closed); a permission
            // rule on `builtin:notifications_list` decides whether it is offered.
            if crate::notifications_tool::session_opted_in(&store_catalog, &session_id_catalog) {
                builtins.push(crate::notifications_tool::catalog_entry());
            }
            // Hosted tools come from the SQLite overlay; filter skill-seeded hosted:
            // entries to avoid merge collisions with list_hsp_catalog_entries.
            let skills: Vec<_> = catalog_cache_seed
                .skills_for_digest(&skills_digest, &skills_dir)
                .into_iter()
                .filter(|entry| !entry.entrypoint.starts_with("hosted:"))
                .collect();
            let hosted = store_catalog.list_hsp_catalog_entries(web_enabled)?;
            let mut catalog = crate::session::merge_tool_catalog(builtins, skills, hosted)?;
            if let Some(run_id) = store_catalog.find_run_id_for_session(&session_id_catalog)? {
                let bound = store_catalog.get_playbook_run(&run_id)?.is_some();
                // A run that offers playbooks gets the step tools too: they stay
                // inert (the host refuses them) until `start_playbook` binds one.
                let offers = !bound
                    && !crate::playbook_activation::run_attached(&store_catalog, &run_id)
                        .is_empty();
                if offers {
                    catalog.push(crate::session::ToolCatalogEntry {
                        tool_name: crate::playbook_activation::START_PLAYBOOK_TOOL.to_string(),
                        command: "host".to_string(),
                        entrypoint: crate::playbook_activation::START_PLAYBOOK_TOOL.to_string(),
                        digest: vam_spec::validation::host_tool_digest(
                            crate::playbook_activation::START_PLAYBOOK_TOOL,
                        ),
                        side_effect: vam_spec::v1::SideEffectClass::MutableLocal as i32,
                        skill_name: String::new(),
                        approval_required: false,
                    });
                }
                if bound || offers {
                    for tool_name in ["step_complete", "run_step", "check_playbook_finish"] {
                        let side_effect = if tool_name == "check_playbook_finish" {
                            vam_spec::v1::SideEffectClass::ReadOnly as i32
                        } else {
                            vam_spec::v1::SideEffectClass::MutableLocal as i32
                        };
                        catalog.push(crate::session::ToolCatalogEntry {
                            tool_name: tool_name.to_string(),
                            command: "host".to_string(),
                            entrypoint: tool_name.to_string(),
                            digest: vam_spec::validation::host_tool_digest(tool_name),
                            side_effect,
                            skill_name: String::new(),
                            approval_required: false,
                        });
                    }
                }
            }
            store_catalog.record_tool_catalog(&session_id_catalog, &catalog)?;
            // What each skill tool declared it may reach through a connection, as of this boot.
            // The skills' cards, checked once for this session (a run reads only these).
            crate::skill_cards::seed_session(&session_id_catalog, &skills_dir);
            store_catalog.record_tool_egress(
                &session_id_catalog,
                &crate::connection_egress::parse_skill_shapes(&skills_dir),
            )?;
            // Guest tool name -> permission target, kept for the life of the
            // session and stamped on approvals (AX-5.3b).
            crate::session_targets::assemble_session_targets(
                &store_catalog,
                &session_id_catalog,
                &catalog,
                &mcp_plan,
            )
        })
        .await
        .map_err(|_| OrchestratorError::Core("Tokio task panic".to_string()))?;
        if let Err(e) = catalog_seed {
            let _ = store.update_observed_state(&session_id, crate::session::ObservedState::Failed);
            return Err(e);
        }
    }
    mark_shared(&timer, "spawn_blocking_tool_catalog");

    let golden_candidate = match crate::golden::resolve_restore_candidate(
        &drive9_root(),
        &storage_profile_id,
        &drive9_image_path,
    ) {
        Ok(candidate) => {
            eprintln!(
                "[orchestrator] session '{}' boot path: golden (key {})",
                session_id, candidate.meta.key
            );
            Some(candidate)
        }
        Err(reason) => {
            eprintln!(
                "[orchestrator] session '{}' boot path: cold ({})",
                session_id, reason
            );
            None
        }
    };

    // 2. Allocate guest CID. A golden restore runs on the baked CID, which is
    // reserved in the allocator like any other so a cold session never gets it
    // (sessions.guest_cid is UNIQUE). A second concurrent golden restore finds it
    // taken and boots cold instead.
    let golden_candidate = match golden_candidate {
        Some(golden) => match cid_allocator.reserve(golden.guest_cid) {
            Ok(()) => Some(golden),
            Err(e) => {
                eprintln!(
                    "[orchestrator] session '{}' boot path: cold (golden CID {} unavailable: {})",
                    session_id, golden.guest_cid, e
                );
                None
            }
        },
        None => None,
    };
    let cid = if let Some(ref golden) = golden_candidate {
        golden.guest_cid
    } else {
        cid_allocator.allocate().map_err(|e| {
            let _ = store.update_observed_state(&session_id, crate::session::ObservedState::Failed);
            OrchestratorError::Core(format!("CID allocation failed: {}", e))
        })?
    };
    let cid_allocated = true;

    // --- Network Plane Setup (Milestone N11) ---
    let network_mode = net_pool.default_mode();
    let lease_outcome = net_pool.lease(network_mode);

    if network_mode == NetworkMode::RestrictedDirect {
        if lease_outcome == TapLeaseOutcome::NotProvisioned {
            release_session_cid(&cid_allocator, cid, cid_allocated);
            let _ = store.update_observed_state(&session_id, crate::session::ObservedState::Failed);
            return Err(OrchestratorError::Core(
                "Restricted Direct network requires TAP pool provisioning (not yet available)"
                    .to_string(),
            ));
        }

        let caps = net_pool.capabilities();
        let use_mock_net = vm_id.contains("mock")
            || session_id.contains("mock")
            || std::env::var("VAM_MOCK_NET").is_ok()
            || !caps.has_tc
            || !caps.has_ip_route2;

        // 1. IPAM Lease Allocation via Cid mapping
        let subnet_index = cid - 3;
        let (_gateway_ipv4, guest_ipv4) = vam_network::calculate_ipv4_addresses(subnet_index)
            .map_err(|e| {
                release_session_cid(&cid_allocator, cid, cid_allocated);
                let _ =
                    store.update_observed_state(&session_id, crate::session::ObservedState::Failed);
                OrchestratorError::Core(e.to_string())
            })?;

        let tap_device_name = vam_network::generate_tap_name(&session_id);

        // Save TAP device name to sqlite row
        let store_clone_tap = store.clone();
        let session_id_clone_tap = session_id.clone();
        let tap_device_name_clone = tap_device_name.clone();
        tokio::task::spawn_blocking(move || {
            store_clone_tap.record_tap_device(&session_id_clone_tap, &tap_device_name_clone)
        })
        .await
        .map_err(|_| OrchestratorError::Core("Tokio task panic".to_string()))??;

        // 2. Provision network namespaces and interfaces
        let adapter: Arc<dyn KernelNetAdapter> = if use_mock_net {
            Arc::new(MockKernelAdapter::new())
        } else {
            Arc::new(LinuxKernelNetAdapter::new())
        };

        let netns_name = format!("ns_{}", &session_id[..std::cmp::min(10, session_id.len())]);

        // Create netns
        adapter.create_netns(&netns_name).await.map_err(|e| {
            release_session_cid(&cid_allocator, cid, cid_allocated);
            let _ = store.update_observed_state(&session_id, crate::session::ObservedState::Failed);
            OrchestratorError::Core(format!("Failed to create netns: {}", e))
        })?;
        mark_shared(&timer, "net_create_netns");

        // Create TAP
        if let Err(e) = adapter.create_tap(&tap_device_name).await {
            let _ = adapter.delete_netns(&netns_name).await;
            release_session_cid(&cid_allocator, cid, cid_allocated);
            let _ = store.update_observed_state(&session_id, crate::session::ObservedState::Failed);
            return Err(OrchestratorError::Core(format!(
                "Failed to create TAP device: {}",
                e
            )));
        }
        mark_shared(&timer, "net_create_tap");

        // Move TAP to netns
        if let Err(e) = adapter
            .move_interface_to_netns(&tap_device_name, &netns_name)
            .await
        {
            let _ = adapter.delete_netns(&netns_name).await;
            release_session_cid(&cid_allocator, cid, cid_allocated);
            let _ = store.update_observed_state(&session_id, crate::session::ObservedState::Failed);
            return Err(OrchestratorError::Core(format!(
                "Failed to move TAP to netns: {}",
                e
            )));
        }
        mark_shared(&timer, "net_move_tap_to_netns");

        // Assign IP
        let ip_with_prefix = format!("{}/30", guest_ipv4);
        if let Err(e) = adapter
            .assign_ip(&tap_device_name, &ip_with_prefix, Some(&netns_name))
            .await
        {
            let _ = adapter.delete_netns(&netns_name).await;
            release_session_cid(&cid_allocator, cid, cid_allocated);
            let _ = store.update_observed_state(&session_id, crate::session::ObservedState::Failed);
            return Err(OrchestratorError::Core(format!(
                "Failed to assign IP to TAP: {}",
                e
            )));
        }
        mark_shared(&timer, "net_assign_ip");

        // Set TAP up
        if let Err(e) = adapter
            .set_interface_up(&tap_device_name, Some(&netns_name))
            .await
        {
            let _ = adapter.delete_netns(&netns_name).await;
            release_session_cid(&cid_allocator, cid, cid_allocated);
            let _ = store.update_observed_state(&session_id, crate::session::ObservedState::Failed);
            return Err(OrchestratorError::Core(format!(
                "Failed to set TAP up: {}",
                e
            )));
        }
        mark_shared(&timer, "net_set_tap_up");

        // Apply TC bandwidth shaping
        if let Err(e) = adapter
            .apply_tc_shaping(
                &tap_device_name,
                Some(10_000_000),
                Some(10_000_000),
                Some(&netns_name),
            )
            .await
        {
            let _ = adapter.delete_netns(&netns_name).await;
            release_session_cid(&cid_allocator, cid, cid_allocated);
            let _ = store.update_observed_state(&session_id, crate::session::ObservedState::Failed);
            return Err(OrchestratorError::Core(format!(
                "Failed to apply tc shaping: {}",
                e
            )));
        }
        mark_shared(&timer, "net_apply_tc_shaping");
    } else {
        debug_assert_eq!(lease_outcome, TapLeaseOutcome::Skipped);
    }

    // 3. Setup host vsock listeners BEFORE booting the VM!
    let vsock_uds_path = vam_core::jail::vsock_uds_path(&vm_id);

    let mut host_listener = match vam_vsock::HostListener::bind(&vsock_uds_path, 52).await {
        Ok(l) => l,
        Err(e) => {
            release_session_cid(&cid_allocator, cid, cid_allocated);
            let _ = store.update_observed_state(&session_id, crate::session::ObservedState::Failed);
            return Err(OrchestratorError::Core(format!(
                "Failed to bind host vsock listener: {}",
                e
            )));
        }
    };
    mark_shared(&timer, "vsock_bind_52");

    let mut host_listener_53 = match vam_vsock::HostListener::bind(&vsock_uds_path, 53).await {
        Ok(l) => l,
        Err(e) => {
            release_session_cid(&cid_allocator, cid, cid_allocated);
            let _ = store.update_observed_state(&session_id, crate::session::ObservedState::Failed);
            return Err(OrchestratorError::Core(format!(
                "Failed to bind host port 53 vsock listener: {}",
                e
            )));
        }
    };
    mark_shared(&timer, "vsock_bind_53");

    let mut host_listener_54 = match vam_vsock::HostListener::bind(&vsock_uds_path, 54).await {
        Ok(l) => l,
        Err(e) => {
            remove_session_vsock_sockets(&vsock_uds_path);
            release_session_cid(&cid_allocator, cid, cid_allocated);
            let _ = store.update_observed_state(&session_id, crate::session::ObservedState::Failed);
            return Err(OrchestratorError::Core(format!(
                "Failed to bind host port 54 vsock listener: {}",
                e
            )));
        }
    };
    mark_shared(&timer, "vsock_bind_54");

    let mut host_listener_55 = match vam_vsock::HostListener::bind(&vsock_uds_path, 55).await {
        Ok(l) => l,
        Err(e) => {
            remove_session_vsock_sockets(&vsock_uds_path);
            release_session_cid(&cid_allocator, cid, cid_allocated);
            let _ = store.update_observed_state(&session_id, crate::session::ObservedState::Failed);
            return Err(OrchestratorError::Core(format!(
                "Failed to bind host port 55 vsock listener: {}",
                e
            )));
        }
    };
    mark_shared(&timer, "vsock_bind_55");

    // Minted after restore (a golden snapshot cannot carry a per-session key)
    // and before guest reseed. Cold boot mints later if restore did not.
    let mut minted_ca: Option<Arc<vam_network::tls_ca::SessionCa>> = None;

    // Transition observed state to BOOTING
    let store_clone = store.clone();
    let session_id_clone = session_id.clone();
    tokio::task::spawn_blocking(move || {
        store_clone.update_observed_state(&session_id_clone, crate::session::ObservedState::Booting)
    })
    .await
    .map_err(|_| OrchestratorError::Core("Tokio task panic".to_string()))??;

    if let Some(ref golden) = golden_candidate {
        let vm_id_restore = vm_id.clone();
        let session_id_restore = session_id.clone();
        let store_restore = store.clone();
        let timer_restore = timer.clone();
        let golden_clone = golden.clone();
        let drive9_clone = drive9_image_path.clone();
        let vsock_clone = vsock_uds_path.clone();
        let vmm_factory_restore = vmm_factory.clone();
        let restore_res = tokio::task::spawn_blocking(move || -> Result<(), OrchestratorError> {
            let mut vmm = vmm_factory_restore(&vm_id_restore, golden_clone.guest_cid);
            crate::golden::restore_from_golden(
                vmm.as_mut(),
                &golden_clone,
                &drive9_clone,
                &vsock_clone,
                timer_restore,
            )?;
            let pid = vmm
                .get_pid()
                .ok_or_else(|| OrchestratorError::Core("restore: missing VMM pid".into()))?;
            let start_time = if pid == 12345 {
                123456789_u64
            } else {
                crate::reconcile::get_process_start_time(pid)?
            };
            store_restore.record_vmm(
                &session_id_restore,
                pid,
                start_time,
                golden_clone.guest_cid,
            )?;
            vmm.detach();
            Ok(())
        })
        .await
        .map_err(|_| OrchestratorError::Core("Tokio task panic".to_string()))?;

        match restore_res {
            Ok(()) => {
                match mint_session_ca(&session_id) {
                    Ok(ca) => minted_ca = Some(ca),
                    Err(err) => {
                        remove_session_vsock_sockets(&vsock_uds_path);
                        release_session_cid(&cid_allocator, cid, cid_allocated);
                        let _ = store.update_observed_state(
                            &session_id,
                            crate::session::ObservedState::Failed,
                        );
                        return Err(err);
                    }
                }
                let reseed_result = crate::driver::reseed_guest_after_snapshot(
                    &vsock_uds_path,
                    RESTORE_LEASE_GENERATION,
                )
                .await;
                if reseed_result.is_ok() {
                    mark_shared(&timer, "snapshot_reseed");
                    // The guest fenced its lease generation to the reseed value, so
                    // the session row must carry it or the first lease is stale.
                    store.assign_lifecycle_generation(&session_id, RESTORE_LEASE_GENERATION)?;
                    let Some(session_ca) = minted_ca.clone() else {
                        remove_session_vsock_sockets(&vsock_uds_path);
                        release_session_cid(&cid_allocator, cid, cid_allocated);
                        let _ = store.update_observed_state(
                            &session_id,
                            crate::session::ObservedState::Failed,
                        );
                        return Err(OrchestratorError::Core(
                            "session CA missing after mint".into(),
                        ));
                    };
                    crate::brokers::spawn_session_brokers(
                        store.clone(),
                        vault.clone(),
                        workspace_manager.clone(),
                        session_id.clone(),
                        workspace_id.clone(),
                        timer.clone(),
                        host_listener,
                        host_listener_53,
                        host_listener_54,
                        host_listener_55,
                        telemetry_capture,
                        session_ca,
                    )
                    .await?;
                    crate::metrics_scrape::spawn_session_metrics_scrape(
                        session_id.clone(),
                        vm_id.clone(),
                    );
                    let store_clone = store.clone();
                    let session_id_clone = session_id.clone();
                    tokio::task::spawn_blocking(move || {
                        store_clone.update_observed_state(
                            &session_id_clone,
                            crate::session::ObservedState::Running,
                        )?;
                        crate::events::append_session_booted(
                            &store_clone,
                            &session_id_clone,
                            "restore",
                        );
                        Ok::<(), OrchestratorError>(())
                    })
                    .await
                    .map_err(|_| OrchestratorError::Core("Tokio task panic".to_string()))??;
                    let elapsed_ms = boot_start_instant.elapsed().as_millis() as u64;
                    let _ = vam_telemetry::report_slo_observation(
                        vam_telemetry::SloTier::Restore,
                        elapsed_ms,
                        &session_id,
                    );
                    workspace_lease.keep = true;
                    return Ok(());
                }
                let reseed_err = reseed_result
                    .err()
                    .unwrap_or_else(|| "unknown error".to_string());
                eprintln!(
                    "[orchestrator] snapshot reseed failed for session '{}': {} — cold boot fallback",
                    session_id, reseed_err
                );
                eprintln!(
                    "[orchestrator] session '{}' boot path: cold (restore failed: guest reseed: {})",
                    session_id, reseed_err
                );
                let mut vmm = vmm_factory.clone()(&vm_id, golden.guest_cid);
                let _ = vmm.shutdown();
            }
            Err(e) => {
                eprintln!(
                    "[orchestrator] golden restore failed for session '{}': {} — cold boot fallback",
                    session_id, e
                );
                eprintln!(
                    "[orchestrator] session '{}' boot path: cold (restore failed: {})",
                    session_id, e
                );
            }
        }
    }

    if golden_candidate.is_some() {
        // Only a failed restore reaches this point. Tearing the restored VMM down
        // deleted every `<uds>_<port>` file, including the sockets our host
        // listeners are bound to, so the cold-booted guest would find no host to
        // register with. Drop the stale listeners first: their Drop removes the
        // socket file, which would otherwise be the new one.
        drop(host_listener);
        drop(host_listener_53);
        drop(host_listener_54);
        drop(host_listener_55);
        match bind_host_listeners(&vsock_uds_path).await {
            Ok([l52, l53, l54, l55]) => {
                host_listener = l52;
                host_listener_53 = l53;
                host_listener_54 = l54;
                host_listener_55 = l55;
            }
            Err(e) => {
                remove_session_vsock_sockets(&vsock_uds_path);
                release_session_cid(&cid_allocator, cid, cid_allocated);
                let _ =
                    store.update_observed_state(&session_id, crate::session::ObservedState::Failed);
                return Err(OrchestratorError::Core(format!(
                    "Failed to rebind host vsock listeners after golden restore failure: {}",
                    e
                )));
            }
        }
    }

    // 4. Construct and start VMM (cold boot)
    let mut vmm = vmm_factory(&vm_id, cid);
    let store_clone = store.clone();
    let session_id_clone = session_id.clone();
    let timer_vmm = timer.clone();

    let start_res =
        tokio::task::spawn_blocking(move || -> Result<(u32, u64), OrchestratorError> {
            let vmm_t = std::time::Instant::now();
            vmm.spawn()
                .map_err(|e| OrchestratorError::Core(format!("VMM spawn failed: {}", e)))?;

            // Past this point the Firecracker process exists. If any step fails it
            // must not outlive the session that never came up.
            let started = (|| -> Result<(u32, u64), OrchestratorError> {
                let kernel_path = resolve_guest_kernel_path();
                set_kernel_shared(&timer_vmm, &kernel_path);
                let rootfs_path = crate::golden::resolve_rootfs_path();
                let log_path =
                    std::env::temp_dir().join(format!("vam-fc-log-{}.log", vm_id_for_config));
                let metrics_path =
                    crate::metrics_scrape::firecracker_metrics_path(&vm_id_for_config);

                let config = vam_core::MicroVmConfigBuilder::new()
                    .with_kernel(kernel_path)
                    .with_rootfs(rootfs_path)
                    .with_limits(2, 512)
                    .with_guest_cid(cid)
                    .with_boot_arg(&format!(
                        "instance_id={} cid={} clocksource=kvm-clock",
                        vm_id_for_config, cid
                    ))
                    .with_data_drive(drive9_image_path)
                    .with_log_path(log_path)
                    .with_metrics_path(metrics_path)
                    .build()
                    .map_err(|e| {
                        OrchestratorError::Core(format!("Config builder failed: {}", e))
                    })?;

                let mut vmm_steps = Vec::new();
                let mut vmm_sink = Some(&mut vmm_steps as &mut Vec<(&'static str, u128)>);
                vmm.configure_hardware(&config, vmm_sink.as_deref_mut())
                    .map_err(|e| OrchestratorError::Core(format!("VMM configure failed: {}", e)))?;
                vmm.start_instance(vmm_sink)
                    .map_err(|e| OrchestratorError::Core(format!("VMM start failed: {}", e)))?;
                merge_shared(&timer_vmm, vmm_steps);
                if timer_vmm.is_some() {
                    let coarse_us = vmm_t.elapsed().as_micros();
                    merge_shared(&timer_vmm, vec![("vmm_spawn_configure_start", coarse_us)]);
                }

                let pid = vmm
                    .get_pid()
                    .ok_or_else(|| OrchestratorError::Core("Failed to get VMM PID".to_string()))?;

                // Parse start_time from /proc/<pid>/stat if it's a real PID (not mock 12345)
                let start_time = if pid == 12345 {
                    123456789_u64
                } else {
                    crate::reconcile::get_process_start_time(pid)?
                };

                // Record VMM pid, start_time, cid in SQLite
                store_clone.record_vmm(&session_id_clone, pid, start_time, cid)?;

                // Detach handle so dropping it doesn't kill VM!
                vmm.detach();

                Ok((pid, start_time))
            })();
            if started.is_err() {
                let _ = vmm.shutdown();
            }
            started
        })
        .await
        .map_err(|_| OrchestratorError::Core("Tokio task panic".to_string()))?;

    let (vmm_pid, _start_time) = match start_res {
        Ok(p) => p,
        Err(e) => {
            remove_session_vsock_sockets(&vsock_uds_path);
            release_session_cid(&cid_allocator, cid, cid_allocated);
            let _ = store.update_observed_state(&session_id, crate::session::ObservedState::Failed);
            return Err(e);
        }
    };

    // 5. Await guest registration handshake with 10s timeout
    let challenge_registry = vam_vsock::ChallengeRegistry::new();
    let verifier = vam_vsock::DevChallengeOnlyVerifier;

    let reg_future = async {
        let stream = host_listener
            .accept()
            .await
            .map_err(|e| OrchestratorError::Core(format!("Accept failed: {}", e)))?;
        vam_vsock::handle_registration_handshake(
            stream,
            &vm_id,
            cid,
            &challenge_registry,
            &verifier,
        )
        .await
        .map_err(|e| OrchestratorError::Core(format!("Handshake failed: {}", e)))
    };

    match tokio::time::timeout(std::time::Duration::from_secs(10), reg_future).await {
        Ok(Ok(_)) => {
            mark_shared(&timer, "registration_handshake_wait");
            let session_ca = if let Some(ca) = minted_ca.clone() {
                ca
            } else {
                match mint_session_ca(&session_id) {
                    Ok(ca) => ca,
                    Err(err) => {
                        remove_session_vsock_sockets(&vsock_uds_path);
                        release_session_cid(&cid_allocator, cid, cid_allocated);
                        let _ = store.update_observed_state(
                            &session_id,
                            crate::session::ObservedState::Failed,
                        );
                        return Err(err);
                    }
                }
            };
            crate::brokers::spawn_session_brokers(
                store.clone(),
                vault.clone(),
                workspace_manager.clone(),
                session_id.clone(),
                workspace_id.clone(),
                timer.clone(),
                host_listener,
                host_listener_53,
                host_listener_54,
                host_listener_55,
                telemetry_capture,
                session_ca,
            )
            .await?;
            crate::metrics_scrape::spawn_session_metrics_scrape(session_id.clone(), vm_id.clone());
            let store_clone = store.clone();
            let session_id_clone = session_id.clone();
            tokio::task::spawn_blocking(move || {
                store_clone.update_observed_state(
                    &session_id_clone,
                    crate::session::ObservedState::Running,
                )?;
                crate::events::append_session_booted(&store_clone, &session_id_clone, "cold");
                Ok::<(), OrchestratorError>(())
            })
            .await
            .map_err(|_| OrchestratorError::Core("Tokio task panic".to_string()))??;
            let elapsed_ms = boot_start_instant.elapsed().as_millis() as u64;
            let _ = vam_telemetry::report_slo_observation(
                vam_telemetry::SloTier::Cold,
                elapsed_ms,
                &session_id,
            );
            workspace_lease.keep = true;
            Ok(())
        }
        Ok(Err(e)) => {
            kill_unregistered_vmm(&vmm_factory, &vm_id, cid, vmm_pid);
            remove_session_vsock_sockets(&vsock_uds_path);
            release_session_cid(&cid_allocator, cid, cid_allocated);
            let _ = store.update_observed_state(&session_id, crate::session::ObservedState::Failed);
            Err(OrchestratorError::Core(format!(
                "Guest registration handshake failed: {}",
                e
            )))
        }
        Err(_) => {
            kill_unregistered_vmm(&vmm_factory, &vm_id, cid, vmm_pid);
            remove_session_vsock_sockets(&vsock_uds_path);
            release_session_cid(&cid_allocator, cid, cid_allocated);
            let _ = store.update_observed_state(&session_id, crate::session::ObservedState::Failed);
            Err(OrchestratorError::Core(
                "Guest registration handshake timed out".to_string(),
            ))
        }
    }
}

/// Settles a session's workspace and reports the outcome. A stop request is
/// settled by rolling back to the last complete turn commit; every other end
/// commits the live tree. A failed commit keeps the tree (never discards).
fn settle_and_report(
    store: &SqliteWorkflowStore,
    workspace_manager: &vam_storage::WorkspaceManager,
    session_id: &str,
    workspace_id: &str,
) {
    let disposition = if store.session_ends.stop_requested(session_id) {
        crate::session_end::Disposition::Stopped
    } else {
        crate::session_end::Disposition::Live
    };
    let outcome = crate::session_end::settle_workspace(
        store,
        workspace_manager,
        session_id,
        workspace_id,
        disposition,
    );
    match &outcome {
        crate::session_end::SettleOutcome::Committed(hash) => println!(
            "[orchestrator] Committed workspace '{}' for session '{}' -> manifest {}",
            workspace_id, session_id, hash
        ),
        crate::session_end::SettleOutcome::Kept { kept_id, .. } => eprintln!(
            "[orchestrator] workspace '{}' commit failed for session '{}'; tree kept as '{}'",
            workspace_id, session_id, kept_id
        ),
        crate::session_end::SettleOutcome::KeepFailed { .. } => eprintln!(
            "[orchestrator] workspace '{}' commit failed for session '{}' and the tree could not be moved; it stays under sessions/",
            workspace_id, session_id
        ),
        crate::session_end::SettleOutcome::NoTree => {}
    }
    store.session_ends.finish_settle(session_id);
}

/// Fixes the files of a session's end commit (see
/// [`SessionManager::prepare_session_end`]). A stop reads no live files: it takes
/// the last complete turn commit (a microVM's is read from its private copy now).
async fn handle_prepare_end(
    store: Arc<SqliteWorkflowStore>,
    workspace_manager: Arc<vam_storage::WorkspaceManager>,
    session_id: String,
    disposition: crate::session_end::Disposition,
) -> Result<crate::session_end::PreparedSummary, OrchestratorError> {
    let session = store.load_session(&session_id)?;
    let workspace_id = session.workspace_id.clone();
    tokio::task::spawn_blocking(move || {
        let (plan, summary) = crate::session_end::plan_end(
            &store,
            &workspace_manager,
            &session_id,
            &workspace_id,
            disposition,
            true,
        )
        .map_err(|e| {
            OrchestratorError::Core(crate::session_end::scrub_host_paths(
                &e.to_string(),
                &workspace_manager,
            ))
        })?;
        store.session_ends.set_plan(&session_id, plan);
        Ok(summary)
    })
    .await
    .map_err(|_| OrchestratorError::Core("Tokio task panic".to_string()))?
}

async fn handle_terminate_session(
    store: Arc<SqliteWorkflowStore>,
    cid_allocator: Arc<CidAllocator>,
    vmm_factory: VmmFactory,
    workspace_manager: Arc<vam_storage::WorkspaceManager>,
    net_pool: Arc<NetResourcePool>,
    session_id: String,
    mode: TerminateMode,
) -> Result<(), OrchestratorError> {
    crate::metrics_scrape::stop_session_metrics_scrape(&session_id);

    let store_clone = store.clone();
    let session_id_clone = session_id.clone();

    let load_res = tokio::task::spawn_blocking(
        move || -> Result<crate::session::SessionRecord, OrchestratorError> {
            let s = store_clone.load_session(&session_id_clone)?;
            if s.observed_state == crate::session::ObservedState::Terminated {
                return Ok(s);
            }
            store_clone.update_observed_state(
                &session_id_clone,
                crate::session::ObservedState::Terminating,
            )?;
            store_clone.load_session(&session_id_clone)
        },
    )
    .await
    .map_err(|_| OrchestratorError::Core("Tokio task panic".to_string()))??;

    if load_res.observed_state == crate::session::ObservedState::Terminated {
        // A stop request took the VM down and left the tree for the run's
        // finalizer; this (full) terminate is that finalizer's call.
        if mode == TerminateMode::Full && store.session_ends.settle_pending(&session_id) {
            let store_clone = store.clone();
            let workspace_manager = workspace_manager.clone();
            let workspace_id = load_res.workspace_id.clone();
            let session_for_settle = session_id.clone();
            tokio::task::spawn_blocking(move || {
                settle_and_report(
                    &store_clone,
                    &workspace_manager,
                    &session_for_settle,
                    &workspace_id,
                )
            })
            .await
            .map_err(|_| OrchestratorError::Core("Tokio task panic".to_string()))?;
        }
        return Ok(());
    }

    let pid = load_res.vmm_pid;
    let start_time = load_res.vmm_start_time;
    let cid = load_res.guest_cid;
    let vm_id = load_res.vm_id.clone();
    let workspace_id = load_res.workspace_id.clone();
    let tap_device = load_res.tap_device.clone();

    // Graceful guest shutdown: flush Drive9 before killing Firecracker.
    if session_vm_is_live(&load_res) {
        let vsock_path = vam_core::jail::vsock_uds_path(&vm_id);
        if let Err(e) = crate::driver::request_guest_shutdown(&vsock_path, 1).await {
            eprintln!(
                "[orchestrator] guest Shutdown RPC failed for session '{}': {} (proceeding with VMM kill)",
                session_id, e
            );
        } else {
            eprintln!(
                "[orchestrator] guest Shutdown RPC accepted for session '{}'",
                session_id
            );
        }
    }

    let store_clone = store.clone();
    let session_id_clone = session_id.clone();
    let vm_id_for_shutdown = vm_id.clone();
    let vm_id_for_net = vm_id.clone();
    let workspace_manager_shutdown = workspace_manager.clone();
    let net_pool_shutdown = net_pool.clone();

    tokio::task::spawn_blocking(move || -> Result<(), OrchestratorError> {
        if let (Some(p), Some(st)) = (pid, start_time) {
            let is_mock = p == 12345;

            let safe_to_kill = if is_mock {
                true
            } else {
                match crate::reconcile::get_process_start_time(p) {
                    Ok(current_st) => {
                        if current_st != st {
                            return Err(OrchestratorError::Core(format!(
                                "PID reuse hazard detected! Expected start time {}, found {} for PID {}",
                                st, current_st, p
                            )));
                        }
                        true
                    }
                    Err(_) => {
                        // Already dead, proceed to cleanup
                        false
                    }
                }
            };

            if safe_to_kill {
                let mut vmm = adopted_vmm(&vmm_factory, &vm_id_for_shutdown, cid.unwrap_or(3), p);
                vmm.shutdown()
                    .map_err(|e| OrchestratorError::Core(format!("VMM shutdown failed: {}", e)))?;
            } else {
                // The VMM already exited: no signal (its pid may be reused), but
                // a process guest still leaves files behind (state dir, base socket).
                adopted_vmm(&vmm_factory, &vm_id_for_shutdown, cid.unwrap_or(3), p)
                    .cleanup_after_exit();
            }
        }

        let vsock_base = vam_core::jail::vsock_uds_path(&vm_id_for_shutdown);
        remove_session_vsock_sockets(&vsock_base);

        if let Some(c) = cid {
            cid_allocator.release(c);
        }

        // --- Network Plane Teardown (Milestone N11) ---
        if tap_device.is_some() {
            let caps = net_pool_shutdown.capabilities();
            let use_mock_net = vm_id_for_net.contains("mock")
                || session_id_clone.contains("mock")
                || std::env::var("VAM_MOCK_NET").is_ok()
                || !caps.has_tc
                || !caps.has_ip_route2;
            let netns_name = format!(
                "ns_{}",
                &session_id_clone[..std::cmp::min(10, session_id_clone.len())]
            );

            let adapter: std::sync::Arc<dyn KernelNetAdapter> = if use_mock_net {
                std::sync::Arc::new(MockKernelAdapter::new())
            } else {
                std::sync::Arc::new(LinuxKernelNetAdapter::new())
            };

            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.block_on(async {
                    let _ = adapter.delete_netns(&netns_name).await;
                });
            }
        }

        // Settle the Drive9 workspace: commit it (a stop rolls back to the last
        // complete turn commit), or keep the tree if the commit fails. A stop
        // request leaves this to the run's finalizer, which has not yet written
        // the end of the task (see `session_end`).
        if mode == TerminateMode::Full {
            settle_and_report(
                &store_clone,
                &workspace_manager_shutdown,
                &session_id_clone,
                &workspace_id,
            );
        }

        store_clone.update_desired_state(&session_id_clone, crate::session::DesiredState::Terminated)?;
        store_clone.update_observed_state(&session_id_clone, crate::session::ObservedState::Terminated)?;

        Ok(())
    })
    .await
    .map_err(|_| OrchestratorError::Core("Tokio task panic".to_string()))??;

    store.clear_session_ca(&session_id);
    Ok(())
}

async fn handle_create_snapshot(
    store: Arc<SqliteWorkflowStore>,
    _cid_allocator: Arc<CidAllocator>,
    _vmm_factory: VmmFactory,
    workspace_manager: Arc<vam_storage::WorkspaceManager>,
    session_id: String,
    snapshot_id: String,
    kind: SnapshotKind,
) -> Result<SnapshotMetadata, OrchestratorError> {
    let session_record = store.load_session(&session_id)?;
    let workspace_id = session_record.workspace_id.clone();

    let snap_dir = snapshot_root().join(&snapshot_id);
    std::fs::create_dir_all(&snap_dir)?;

    let layout = workspace_manager.layout();

    // Warm snapshot: copy session disk image to snapshot directory
    if kind == SnapshotKind::Warm {
        let session_img = layout.session_image_path(&session_id);
        let snap_disk = snap_dir.join("disk.ext4");
        if session_img.exists() {
            vam_storage::copy_sparse(&session_img, &snap_disk)?;
        }
    }

    // Commit workspace to get manifest hash without releasing lock
    let manifest_hash = workspace_manager
        .commit(&session_id, &workspace_id)
        .map_err(|e| {
            OrchestratorError::Core(format!("Snapshot workspace commit failed: {:?}", e))
        })?;

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    let meta = SnapshotMetadata {
        snapshot_id: snapshot_id.clone(),
        session_id: session_id.clone(),
        workspace_id: workspace_id.clone(),
        parent_snapshot_id: None,
        kind,
        drive9_manifest_hash: manifest_hash.clone(),
        guest_cid: session_record.guest_cid,
        tap_name: session_record.tap_device,
        netns: None,
        fc_version: Some("v1.5.0".to_string()),
        created_at: now,
    };

    let meta_path = snap_dir.join("meta.json");
    let meta_json =
        serde_json::to_string_pretty(&meta).map_err(|e| OrchestratorError::Core(e.to_string()))?;
    std::fs::write(&meta_path, meta_json)?;

    store.record_snapshot(&meta)?;

    Ok(meta)
}

#[allow(clippy::too_many_arguments)] // flat parameter list is the established call shape; a params struct would change callers
async fn handle_restore_session(
    store: Arc<SqliteWorkflowStore>,
    vault: Arc<crate::secrets::SecretsVault>,
    cid_allocator: Arc<CidAllocator>,
    vmm_factory: VmmFactory,
    workspace_manager: Arc<vam_storage::WorkspaceManager>,
    catalog_cache: Arc<crate::session::ToolCatalogCache>,
    net_pool: Arc<NetResourcePool>,
    snapshot_id: String,
    new_session_id: String,
    telemetry_capture: vam_telemetry::TelemetryCapture,
) -> Result<(), OrchestratorError> {
    let snap_dir = snapshot_root().join(&snapshot_id);
    let meta_path = snap_dir.join("meta.json");
    if !meta_path.exists() {
        return Err(OrchestratorError::NotFound(format!(
            "Snapshot {} not found",
            snapshot_id
        )));
    }
    let meta_bytes = std::fs::read_to_string(&meta_path)?;
    let meta: SnapshotMetadata =
        serde_json::from_str(&meta_bytes).map_err(|e| OrchestratorError::Core(e.to_string()))?;

    let vm_id = format!("vm-{}", new_session_id);
    let storage_profile_id = "default".to_string();

    handle_create_session(
        store,
        vault,
        cid_allocator,
        vmm_factory,
        workspace_manager,
        catalog_cache,
        net_pool,
        new_session_id,
        None,
        vm_id,
        meta.workspace_id,
        storage_profile_id,
        None,
        telemetry_capture,
    )
    .await
}

async fn handle_pause_session(
    store: Arc<SqliteWorkflowStore>,
    vmm_factory: VmmFactory,
    session_id: String,
) -> Result<(), OrchestratorError> {
    let record = store.load_session(&session_id)?;
    if record.observed_state == crate::session::ObservedState::Paused {
        return Ok(());
    }
    if record.observed_state != crate::session::ObservedState::Running {
        return Err(OrchestratorError::InvalidState {
            expected: "RUNNING",
            actual: record.observed_state.as_str().to_string(),
        });
    }

    store.update_desired_state(&session_id, crate::session::DesiredState::Paused)?;

    if let (Some(pid), Some(start_time), Some(cid)) =
        (record.vmm_pid, record.vmm_start_time, record.guest_cid)
    {
        let vm_id = record.vm_id.clone();
        tokio::task::spawn_blocking(move || -> Result<(), OrchestratorError> {
            let mut vmm = adopted_vmm(&vmm_factory, &vm_id, cid, pid);
            let _ = start_time;
            vmm.pause()
                .map_err(|e| OrchestratorError::Core(format!("VMM pause failed: {}", e)))
        })
        .await
        .map_err(|_| OrchestratorError::Core("Tokio task panic".to_string()))??;
    }

    store.update_observed_state(&session_id, crate::session::ObservedState::Paused)?;
    Ok(())
}

async fn handle_resume_session(
    store: Arc<SqliteWorkflowStore>,
    vmm_factory: VmmFactory,
    session_id: String,
) -> Result<(), OrchestratorError> {
    let record = store.load_session(&session_id)?;
    if record.observed_state == crate::session::ObservedState::Running {
        return Ok(());
    }
    if record.observed_state != crate::session::ObservedState::Paused {
        return Err(OrchestratorError::InvalidState {
            expected: "PAUSED",
            actual: record.observed_state.as_str().to_string(),
        });
    }

    store.update_desired_state(&session_id, crate::session::DesiredState::Running)?;

    if let (Some(pid), Some(start_time), Some(cid)) =
        (record.vmm_pid, record.vmm_start_time, record.guest_cid)
    {
        let vm_id = record.vm_id.clone();
        tokio::task::spawn_blocking(move || -> Result<(), OrchestratorError> {
            let mut vmm = adopted_vmm(&vmm_factory, &vm_id, cid, pid);
            let _ = start_time;
            vmm.resume()
                .map_err(|e| OrchestratorError::Core(format!("VMM resume failed: {}", e)))
        })
        .await
        .map_err(|_| OrchestratorError::Core("Tokio task panic".to_string()))??;
    }

    store.update_observed_state(&session_id, crate::session::ObservedState::Running)?;
    Ok(())
}

pub fn resolve_guest_kernel_path() -> std::path::PathBuf {
    crate::golden::resolve_guest_kernel_path()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::SqliteWorkflowStore;

    #[tokio::test]
    async fn host_listeners_rebind_after_vmm_teardown_removed_their_sockets() {
        let dir = tempfile::tempdir().unwrap();
        let uds = dir.path().join("vam-vsock-vm-x.socket");
        let sockets = |uds: &std::path::Path| {
            [52u32, 53, 54, 55].map(|p| std::path::PathBuf::from(format!("{}_{p}", uds.display())))
        };

        let stale = bind_host_listeners(&uds).await.unwrap();
        // Firecracker teardown deletes every `<uds>_<port>` file.
        remove_session_vsock_sockets(&uds);
        assert!(sockets(&uds).iter().all(|p| !p.exists()));

        drop(stale);
        let fresh = bind_host_listeners(&uds).await.unwrap();
        assert!(sockets(&uds).iter().all(|p| p.exists()));
        drop(fresh);
    }

    #[test]
    fn test_resolve_guest_kernel_path_fallback() {
        let path = resolve_guest_kernel_path();
        // Should resolve to existing 5.10.211 in /var/lib/vam/kernels/ or candidate fallback
        println!("Resolved guest kernel path: {:?}", path);
        assert!(!path.as_os_str().is_empty());
    }

    fn snapshot_meta(workspace_id: &str, manifest_hash: &str) -> SnapshotMetadata {
        SnapshotMetadata {
            snapshot_id: "snap-test".to_string(),
            session_id: "sess-old".to_string(),
            workspace_id: workspace_id.to_string(),
            parent_snapshot_id: None,
            kind: SnapshotKind::Cold,
            drive9_manifest_hash: manifest_hash.to_string(),
            guest_cid: None,
            tap_name: None,
            netns: None,
            fc_version: None,
            created_at: 1,
        }
    }

    fn recording_factory() -> (VmmFactory, Arc<std::sync::Mutex<Vec<String>>>) {
        let history = Arc::new(std::sync::Mutex::new(Vec::new()));
        let h = history.clone();
        let factory: VmmFactory =
            Arc::new(move |_, _| Box::new(vam_core::MockVmm::with_shared_history(h.clone())));
        (factory, history)
    }

    #[test]
    fn unregistered_vmm_is_stopped_through_the_factory() {
        let (factory, history) = recording_factory();
        kill_unregistered_vmm(&factory, "vm-x", 7, 4242);
        assert_eq!(*history.lock().unwrap(), vec!["adopt:4242", "shutdown"]);
    }

    #[tokio::test]
    async fn pause_and_resume_go_through_the_factory_with_the_stored_pid() {
        let store = Arc::new(SqliteWorkflowStore::in_memory().expect("store"));
        store
            .create_session("sess-pr", None, "vm-pr", "ws-pr", "default")
            .expect("session");
        store
            .update_observed_state("sess-pr", crate::session::ObservedState::Booting)
            .expect("booting");
        store.record_vmm("sess-pr", 4242, 1, 9).expect("pid");
        store
            .update_observed_state("sess-pr", crate::session::ObservedState::Running)
            .expect("running");
        let (factory, history) = recording_factory();
        handle_pause_session(store.clone(), factory.clone(), "sess-pr".into())
            .await
            .expect("pause");
        handle_resume_session(store.clone(), factory, "sess-pr".into())
            .await
            .expect("resume");
        assert_eq!(
            *history.lock().unwrap(),
            vec!["adopt:4242", "pause", "adopt:4242", "resume"]
        );
    }

    #[test]
    fn terminated_mock_vmm_is_not_warm() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        store
            .create_session("sess-warm", None, "vm-warm", "ws-warm", "default")
            .expect("session");
        store
            .update_observed_state("sess-warm", crate::session::ObservedState::Booting)
            .expect("booting");
        store.record_vmm("sess-warm", 12345, 1, 3).expect("pid");
        store
            .update_observed_state("sess-warm", crate::session::ObservedState::Running)
            .expect("running");
        let running = store.load_session("sess-warm").expect("load");
        assert!(session_is_warm(&running));
        store
            .assign_lifecycle_generation("sess-warm", 2)
            .expect("generation");
        assert_eq!(
            store
                .load_session("sess-warm")
                .unwrap()
                .lifecycle_generation,
            2
        );

        store
            .update_observed_state("sess-warm", crate::session::ObservedState::Terminated)
            .expect("terminated");
        let parked = store.load_session("sess-warm").expect("load");
        assert!(
            !session_is_warm(&parked),
            "a parked session is cold even when the mock VMM pid is still live"
        );
    }

    #[test]
    fn resolve_session_agent_urn_prefers_run_binding() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        store
            .persist_run_spec(
                "run-1",
                "{}",
                Some("sess-test1"),
                "urn:vam:agent:examples.local/vam/jira-assistant",
                None,
                "test",
            )
            .expect("persist");
        let identity = resolve_agent_identity(&store, "sess-test1", "test1").expect("resolve");
        assert_eq!(
            identity,
            AgentIdentity::Resolved("urn:vam:agent:examples.local/vam/jira-assistant".to_string())
        );
    }

    #[test]
    fn resolve_agent_identity_does_not_synthesize_workspace_urn() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        let identity = resolve_agent_identity(&store, "sess-missing", "test1").expect("resolve");
        assert_eq!(identity, AgentIdentity::Unresolved);
        assert_ne!(
            identity.principal_urn(),
            "urn:vam:agent:test1",
            "must not synthesize a workspace-id URN"
        );
    }

    #[test]
    fn resolve_seed_dir_follows_the_space_label() {
        assert!(resolve_seed_dir("assistant")
            .expect("label seed")
            .ends_with("bun-assistant"));
        assert!(resolve_seed_dir("sqlite-assistant")
            .expect("sqlite label")
            .ends_with("sqlite-assistant"));
        assert!(resolve_seed_dir("a/b").is_none());
    }

    #[test]
    fn resolve_agent_identity_exact_seed_name_reads_agent_json() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        let identity =
            resolve_agent_identity(&store, "sess-missing", "jira-assistant").expect("resolve");
        assert_eq!(
            identity,
            AgentIdentity::Resolved("urn:vam:agent:local.vam.dev/vam/jira-assistant".to_string())
        );
    }

    #[test]
    fn run_bound_session_resolves_only_the_runs_agent() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        store
            .persist_run_spec(
                "run-bound",
                "{}",
                Some("sess-bound"),
                "urn:vam:agent:local.vam.dev/acme/the-agent",
                None,
                "test",
            )
            .expect("persist");
        // The workspace names a different seed agent: the run still wins.
        let identity =
            resolve_session_identity(&store, "sess-bound", "jira-assistant").expect("resolve");
        assert_eq!(
            identity,
            AgentIdentity::Resolved("urn:vam:agent:local.vam.dev/acme/the-agent".to_string())
        );
    }

    #[test]
    fn run_bound_session_without_an_agent_fails_closed() {
        for agent in ["", "unset"] {
            let store = SqliteWorkflowStore::in_memory().expect("store");
            store
                .persist_run_spec(
                    "run-noagent",
                    "{}",
                    Some("sess-noagent"),
                    agent,
                    None,
                    "test",
                )
                .expect("persist");
            if agent == "unset" {
                store
                    .connection
                    .lock()
                    .expect("lock")
                    .execute(
                        "UPDATE runs SET agent_urn = NULL WHERE run_id = 'run-noagent'",
                        [],
                    )
                    .expect("null agent");
            }
            // An exact seed workspace must not rescue it: no seed fallback.
            let err = resolve_session_identity(&store, "sess-noagent", "jira-assistant")
                .expect_err("a run-bound session with no agent must not resolve");
            let text = err.to_string();
            assert!(text.contains("run-noagent"), "{text}");
            assert!(text.contains("refusing to start"), "{text}");
        }
    }

    #[test]
    fn session_with_no_run_keeps_the_seed_and_builtins_fallback() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        assert_eq!(
            resolve_session_identity(&store, "sess-bare", "jira-assistant").expect("seed"),
            AgentIdentity::Resolved("urn:vam:agent:local.vam.dev/vam/jira-assistant".to_string())
        );
        assert_eq!(
            resolve_session_identity(&store, "sess-bare", "operator--nothing").expect("bare"),
            AgentIdentity::Unresolved
        );
    }

    #[test]
    fn resolve_agent_identity_ignores_starts_with_heuristics() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        let identity = resolve_agent_identity(&store, "sess-missing", "jira").expect("resolve");
        assert_eq!(
            identity,
            AgentIdentity::Unresolved,
            "prefix heuristics belong to resolve_seed_dir, not identity"
        );
    }

    #[test]
    fn boot_does_not_overwrite_existing_head() {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteWorkflowStore::in_memory().expect("store should initialize");
        let layout = vam_storage::Drive9Layout::new(dir.path());
        let chunk_store = vam_storage::ChunkStore::new(layout.clone());
        let manifest_manager =
            vam_storage::ManifestManager::new(layout.clone(), chunk_store.clone());

        let head_hash = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let snap_hash = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        manifest_manager.write_head("ws-1", head_hash).unwrap();
        store
            .record_snapshot(&snapshot_meta("ws-1", snap_hash))
            .unwrap();

        restore_head_from_snapshot_if_missing_at(&store, "ws-1", dir.path()).unwrap();
        assert_eq!(
            manifest_manager.read_head("ws-1").unwrap().as_deref(),
            Some(head_hash)
        );
    }

    #[test]
    fn boot_restores_head_from_snapshot_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteWorkflowStore::in_memory().expect("store should initialize");
        let layout = vam_storage::Drive9Layout::new(dir.path());
        let chunk_store = vam_storage::ChunkStore::new(layout.clone());
        let manifest_manager =
            vam_storage::ManifestManager::new(layout.clone(), chunk_store.clone());

        let snap_hash = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
        store
            .record_snapshot(&snapshot_meta("ws-2", snap_hash))
            .unwrap();

        restore_head_from_snapshot_if_missing_at(&store, "ws-2", dir.path()).unwrap();
        assert_eq!(
            manifest_manager.read_head("ws-2").unwrap().as_deref(),
            Some(snap_hash)
        );
    }

    #[test]
    fn record_workspace_snapshot_uses_committed_hash() {
        let store = SqliteWorkflowStore::in_memory().expect("store should initialize");
        let manifest_hash = "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";
        let meta = record_workspace_snapshot(
            &store,
            "sess-1",
            "ws-3",
            "snap-post-commit",
            manifest_hash,
            Some(5),
            None,
        )
        .unwrap();
        assert_eq!(meta.drive9_manifest_hash, manifest_hash);
        assert_eq!(meta.kind, SnapshotKind::Cold);
        let latest = store
            .find_latest_snapshot_for_workspace("ws-3")
            .unwrap()
            .expect("snapshot should be recorded");
        assert_eq!(latest.drive9_manifest_hash, manifest_hash);
    }

    fn terminated_holder(store: &SqliteWorkflowStore, root: &std::path::Path, session: &str) {
        store
            .create_session(session, None, "vm-x", "ws-lock", "prof")
            .unwrap();
        store
            .update_observed_state(session, crate::session::ObservedState::Terminated)
            .unwrap();
        let lock = root.join("workspaces/ws-lock");
        std::fs::create_dir_all(&lock).unwrap();
        std::fs::write(lock.join("write.lock"), session).unwrap();
    }

    #[test]
    fn a_dead_holders_lock_is_reclaimed_but_not_while_its_stop_awaits_settlement() {
        let root = tempfile::tempdir().unwrap();
        let layout = vam_storage::Drive9Layout::new(root.path());
        let store = SqliteWorkflowStore::in_memory().unwrap();
        terminated_holder(&store, root.path(), "sess-stopped");

        // A stop took the VM down; its tree has not been settled.
        store.session_ends.track("sess-stopped");
        assert!(store.session_ends.begin_stop("sess-stopped"));
        reclaim_stale_workspace_lock_at(&store, &layout, "ws-lock", "sess-new");
        assert_eq!(
            vam_storage::read_workspace_lock_holder(&layout, "ws-lock").unwrap(),
            Some("sess-stopped".to_string()),
            "the lock stays until the finalizer has settled the tree"
        );

        // Settled: now it is stale like any other dead holder's.
        store.session_ends.finish_settle("sess-stopped");
        reclaim_stale_workspace_lock_at(&store, &layout, "ws-lock", "sess-new");
        assert_eq!(
            vam_storage::read_workspace_lock_holder(&layout, "ws-lock").unwrap(),
            None
        );
    }
}
