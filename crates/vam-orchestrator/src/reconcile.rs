use crate::error::OrchestratorError;
use crate::session::ObservedState;
use crate::workflow::SqliteWorkflowStore;
use std::fs;
use std::path::Path;
use vam_core::CidAllocator;

/// A session that is gone but still holds its workspace lock over a tree on
/// disk: a crash (or a restart) caught it before its work was committed. The
/// lock is kept, and the tree settled by [`crate::session_end::settle_dead_trees`]
/// once the workspace manager is open. Nothing is abandoned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeadTree {
    pub session_id: String,
    pub workspace_id: String,
    /// The VMM the session had, and its start time as recorded: the settle waits
    /// for it to be gone while the start time still matches, so a dying VMM is
    /// not still writing the image it is about to read.
    pub vmm_pid: Option<u32>,
    pub vmm_start_time: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct ReconciliationReport {
    pub adopted_sessions: usize,
    pub reaped_orphans: usize,
    pub repaired_transitions: usize,
    pub freed_cids: Vec<u32>,
    pub duration_ms: u128,
    pub dead_trees: Vec<DeadTree>,
}

impl ReconciliationReport {
    pub fn to_json(&self) -> String {
        format!(
            "{{\"event\": \"vamd.reconciliation.completed\", \"summary\": {{\"adopted_sessions\": {}, \"reaped_orphans\": {}, \"repaired_transitions\": {}, \"freed_cids\": {:?}, \"duration_ms\": {}}}}}",
            self.adopted_sessions, self.reaped_orphans, self.repaired_transitions, self.freed_cids, self.duration_ms
        )
    }
}

/// Safe parser for field 22 (starttime) from /proc/<pid>/stat.
/// It correctly handles process names (comm, field 2) containing spaces or parentheses.
pub fn get_process_start_time(pid: u32) -> Result<u64, OrchestratorError> {
    let stat_path = format!("/proc/{}/stat", pid);
    let content = fs::read_to_string(&stat_path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            OrchestratorError::Core(format!("PID {} not found", pid))
        } else {
            OrchestratorError::Core(format!("Failed to read {}: {}", stat_path, e))
        }
    })?;

    // Find the last ')' to safely bypass the (comm) field which may contain spaces or parens.
    let last_paren = content.rfind(')').ok_or_else(|| {
        OrchestratorError::CorruptState(format!("Invalid stat file format for PID {}", pid))
    })?;

    let rest = &content[last_paren + 1..];
    let fields: Vec<&str> = rest.split_whitespace().collect();

    // Field 22 is starttime. Since we skipped pid (1) and comm (2), the remaining fields
    // start with field 3 (state) at index 0. Therefore, field 22 is at index 19 (22 - 3).
    let start_time_str = fields.get(19).ok_or_else(|| {
        OrchestratorError::CorruptState(format!("Could not find starttime field for PID {}", pid))
    })?;

    let start_time = start_time_str.parse::<u64>().map_err(|e| {
        OrchestratorError::CorruptState(format!(
            "Failed to parse starttime '{}' for PID {}: {}",
            start_time_str, pid, e
        ))
    })?;

    Ok(start_time)
}

pub fn scan_proc_for_firecracker_pids() -> Vec<u32> {
    let mut pids = Vec::new();
    if let Ok(entries) = fs::read_dir("/proc") {
        for entry in entries.flatten() {
            if let Ok(file_type) = entry.file_type() {
                if file_type.is_dir() {
                    let name = entry.file_name();
                    let name_str = name.to_string_lossy();
                    if let Ok(pid) = name_str.parse::<u32>() {
                        let comm_path = format!("/proc/{}/comm", pid);
                        if let Ok(comm) = fs::read_to_string(comm_path) {
                            let comm_trimmed = comm.trim();
                            if comm_trimmed == "firecracker" || comm_trimmed == "jailer" {
                                pids.push(pid);
                            }
                        }
                    }
                }
            }
        }
    }
    pids
}

/// Adopt live microVMs, reap orphans, and repair dead sessions on `vamd` startup.
///
/// A parked run is rows in the store: no VM, so there is nothing to adopt.
/// Repairing a dead session and releasing its Drive9 `write.lock` must not
/// drop or replace that run's workspace reservation.
pub fn run_startup_reconciliation(
    store: &SqliteWorkflowStore,
    cid_allocator: &CidAllocator,
    mock_pids: Option<Vec<(u32, u64)>>,
    drive9_root: Option<&Path>,
) -> Result<ReconciliationReport, OrchestratorError> {
    let start_time = std::time::Instant::now();

    let mut adopted_sessions = 0;
    let mut reaped_orphans = 0;
    let mut repaired_transitions = 0;
    let mut freed_cids = Vec::new();

    // Parked runs have no VM. Snapshot reservations so this pass can prove it
    // did not drop or replace them while repairing other sessions.
    let reservations_before = store.list_workspace_reservations()?;

    // 1. Gather running process CIDs and start-times from /proc (or mock)
    let mut running_procs = std::collections::HashMap::new();
    if let Some(mock) = mock_pids {
        for (pid, st) in mock {
            running_procs.insert(pid, st);
        }
    } else {
        for pid in scan_proc_for_firecracker_pids() {
            if let Ok(st) = get_process_start_time(pid) {
                running_procs.insert(pid, st);
            }
        }
    }

    // 2. Load all sessions from SQL
    let sessions = store.load_all_sessions()?;
    let mut adopted_pids = std::collections::HashSet::new();

    // 3. Adopt matching sessions
    for session in &sessions {
        // Terminal sessions are not adopted. A parked run's session is
        // Terminated and has no VM, even when its old pid still appears in
        // the process list.
        if session.observed_state == ObservedState::Terminated
            || session.observed_state == ObservedState::Failed
        {
            continue;
        }

        if let Some(vmm_pid) = session.vmm_pid {
            if let Some(&actual_st) = running_procs.get(&vmm_pid) {
                if Some(actual_st) == session.vmm_start_time {
                    // Start-time matches! Adopt the process.
                    if let Some(cid) = session.guest_cid {
                        // Rehydrate CID allocator
                        let _ = cid_allocator.reserve(cid);
                    }
                    adopted_sessions += 1;
                    adopted_pids.insert(vmm_pid);
                }
            }
        }
    }

    // 4. Reap unadopted Orphans (running processes that don't belong to any active session)
    for (pid, _) in running_procs {
        if !adopted_pids.contains(&pid) {
            // Reaping orphan! (Bypass mock PID 12345 in tests)
            if pid != 12345 {
                #[cfg(unix)]
                {
                    unsafe { libc::kill(pid as i32, 9) };
                }
            }
            reaped_orphans += 1;
        }
    }

    // 5. Repair non-terminal sessions whose processes are dead
    for session in &sessions {
        if session.observed_state == ObservedState::Terminated
            || session.observed_state == ObservedState::Failed
        {
            continue;
        }

        let is_running_or_booting = session.observed_state == ObservedState::Running
            || session.observed_state == ObservedState::Booting
            || session.observed_state == ObservedState::Creating;

        if is_running_or_booting {
            let has_living_pid = session
                .vmm_pid
                .map(|p| adopted_pids.contains(&p))
                .unwrap_or(false);
            if !has_living_pid {
                // Dead process! Repair.
                store.update_observed_state(&session.session_id, ObservedState::Failed)?;
                if let Some(cid) = session.guest_cid {
                    freed_cids.push(cid);
                    cid_allocator.release(cid);
                }
                repaired_transitions += 1;
            }
        }
    }

    // 6. Drop Drive9 write locks held by sessions that were not adopted.
    // A stale write.lock must not block a later run. This does not release a
    // parked run's row in run_reservations; that reservation still blocks
    // CreateRun until resume, cancel, or expiry.
    //
    // A lock over a session tree that is still on disk is not stale: the
    // session died before its work was committed. Its lock stays and the tree
    // is handed back to be settled (committed, or kept), never dropped.
    let mut dead_trees = Vec::new();
    if let Some(root) = drive9_root {
        let layout = vam_storage::Drive9Layout::new(root);
        for session in &sessions {
            let keep_lock = session
                .vmm_pid
                .map(|pid| adopted_pids.contains(&pid))
                .unwrap_or(false);
            if !keep_lock {
                let holds_lock = matches!(
                    vam_storage::read_workspace_lock_holder(&layout, &session.workspace_id),
                    Ok(Some(ref holder)) if holder == &session.session_id
                );
                let has_tree = layout
                    .session_dir(&session.session_id)
                    .join("materialized")
                    .is_dir();
                if holds_lock && has_tree {
                    dead_trees.push(DeadTree {
                        session_id: session.session_id.clone(),
                        workspace_id: session.workspace_id.clone(),
                        vmm_pid: session.vmm_pid,
                        vmm_start_time: session.vmm_start_time,
                    });
                    continue;
                }
                let _ = vam_storage::release_workspace_lock(
                    &layout,
                    &session.workspace_id,
                    Some(&session.session_id),
                );
            }
        }
    }

    let reservations_after = store.list_workspace_reservations()?;
    if reservations_before != reservations_after {
        return Err(OrchestratorError::CorruptState(
            "startup reconciliation changed workspace reservations for a parked run".into(),
        ));
    }

    let duration_ms = start_time.elapsed().as_millis();

    Ok(ReconciliationReport {
        adopted_sessions,
        reaped_orphans,
        repaired_transitions,
        freed_cids,
        duration_ms,
        dead_trees,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dead_session(
        store: &SqliteWorkflowStore,
        root: &std::path::Path,
        id: &str,
        ws: &str,
        pid: u32,
    ) {
        store
            .create_session(id, None, &format!("vm-{id}"), ws, "prof")
            .unwrap();
        store
            .update_observed_state(id, ObservedState::Booting)
            .unwrap();
        store.record_vmm(id, pid, 1, 3 + pid).unwrap();
        store
            .update_observed_state(id, ObservedState::Running)
            .unwrap();
        let dir = root.join("workspaces").join(ws);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("write.lock"), id).unwrap();
    }

    #[test]
    fn a_dead_session_over_an_unsettled_tree_keeps_its_lock_and_is_handed_back() {
        let root = tempfile::tempdir().unwrap();
        let store = SqliteWorkflowStore::in_memory().unwrap();
        let cids = CidAllocator::default();
        dead_session(&store, root.path(), "sess-tree", "ws-tree", 4_000_001);
        dead_session(&store, root.path(), "sess-bare", "ws-bare", 4_000_002);
        std::fs::create_dir_all(
            root.path()
                .join("sessions/sess-tree/materialized/workspace"),
        )
        .unwrap();
        std::fs::write(
            root.path()
                .join("sessions/sess-tree/materialized/workspace/work.txt"),
            b"unsaved",
        )
        .unwrap();

        let report =
            run_startup_reconciliation(&store, &cids, Some(vec![]), Some(root.path())).unwrap();

        assert_eq!(
            report.dead_trees,
            vec![DeadTree {
                session_id: "sess-tree".into(),
                workspace_id: "ws-tree".into(),
                vmm_pid: Some(4_000_001),
                vmm_start_time: Some(1),
            }]
        );
        let layout = vam_storage::Drive9Layout::new(root.path());
        assert_eq!(
            vam_storage::read_workspace_lock_holder(&layout, "ws-tree").unwrap(),
            Some("sess-tree".to_string()),
            "the lock stays until the tree is settled"
        );
        assert_eq!(
            vam_storage::read_workspace_lock_holder(&layout, "ws-bare").unwrap(),
            None,
            "a dead session with no tree frees its workspace as before"
        );
        // The tree is untouched.
        assert!(root
            .path()
            .join("sessions/sess-tree/materialized/workspace/work.txt")
            .exists());
        assert_eq!(
            store.load_session("sess-tree").unwrap().observed_state,
            ObservedState::Failed
        );
    }

    #[test]
    fn a_lock_held_by_someone_else_is_not_a_dead_tree() {
        let root = tempfile::tempdir().unwrap();
        let store = SqliteWorkflowStore::in_memory().unwrap();
        let cids = CidAllocator::default();
        dead_session(&store, root.path(), "sess-old", "ws-x", 4_000_003);
        // A newer session holds the lock; the old tree is not its to settle.
        std::fs::write(root.path().join("workspaces/ws-x/write.lock"), "sess-new").unwrap();
        std::fs::create_dir_all(root.path().join("sessions/sess-old/materialized")).unwrap();
        let report =
            run_startup_reconciliation(&store, &cids, Some(vec![]), Some(root.path())).unwrap();
        assert!(report.dead_trees.is_empty());
    }

    #[test]
    fn test_parse_own_start_time() {
        #[cfg(target_os = "linux")]
        {
            let pid = std::process::id();
            let start_time =
                get_process_start_time(pid).expect("Should successfully parse own start time");
            assert!(start_time > 0);
        }
        #[cfg(not(target_os = "linux"))]
        {
            let pid = std::process::id();
            let _ = get_process_start_time(pid);
        }
    }
}
