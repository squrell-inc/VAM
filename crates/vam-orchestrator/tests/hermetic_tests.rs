// Env-var serialisation lock is deliberately held for the whole test (process-global env); not a deadlock risk.
#![allow(clippy::await_holding_lock)]
use axum::body::Body;
use axum::http::Request;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use tokio::net::UnixStream;
use tower::ServiceExt;
use vam_core::{CidAllocator, MockVmm};
use vam_network::{NetResourcePool, NetworkMode};
use vam_orchestrator::api::auth::ControlPlanePdp;
use vam_orchestrator::api::{self, AppState};
use vam_orchestrator::error::AttemptKey;
use vam_orchestrator::keys::{mint_api_key_secret, BOOTSTRAP_SCOPES};
use vam_orchestrator::manager::{
    drive9_runtime, run_manager_loop, sweep_active_duration, sweep_expired_approvals,
    sweep_keep_warm, SessionManager,
};
use vam_orchestrator::reconcile::run_startup_reconciliation;
use vam_orchestrator::secrets::SecretsVault;
use vam_orchestrator::session::{DesiredState, ObservedState};
use vam_orchestrator::workflow::SqliteWorkflowStore;
use vam_orchestrator::{VmmFactory, LOCAL_USER_ID};

/// Env vars such as `VAM_DRIVE9_ROOT`, `VAM_SNAPSHOT_RESTORE` and the kernel and
/// rootfs paths are process-wide, and several tests here set them. Every test in
/// this file holds this lock so one test's settings never leak into another's
/// session, which would cause handshake timeouts.
static HERMETIC_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// How many `ExecuteTask` calls `serve_resume_guest` has answered (reset by the test that counts).
static RESUME_TASKS_RUN: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Points drive9 and snapshot state at a fresh tempdir for one test. The
/// production default is root-owned `/var/lib/vam`, which a non-root CI runner
/// cannot create, so these tests must never fall through to it.
fn isolate_state() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("state tempdir");
    // Sessions spawned in-process skip vamd's startup, which is what installs the node
    // keys. Production stays fail-closed; the tests install throwaway in-memory keys
    // (a process-wide one-shot, so later calls are no-ops).
    let _ = vam_orchestrator::signing::install(vam_orchestrator::signing::NodeKeys::ephemeral());
    let drive9 = dir.path().join("drive9");
    let snapshots = dir.path().join("snapshots");
    // Create the roots up front, as `drive9_root()` does for the default path.
    std::fs::create_dir_all(&drive9).expect("drive9 root");
    std::fs::create_dir_all(&snapshots).expect("snapshot root");
    std::env::set_var("VAM_DRIVE9_ROOT", &drive9);
    std::env::set_var("VAM_SNAPSHOT_ROOT", &snapshots);
    dir
}

#[test]
fn test_sqlite_session_crud() {
    let _env = HERMETIC_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let store = SqliteWorkflowStore::in_memory().unwrap();
    let session_id = "session-123";
    let vm_id = "vm-123";

    // Create session
    assert!(store
        .create_session(session_id, None, vm_id, "workspace-1", "profile-1")
        .is_ok());

    // Check initial state
    let record = store.load_session(session_id).unwrap();
    assert_eq!(record.session_id, session_id);
    assert_eq!(record.vm_id, vm_id);
    assert_eq!(record.desired_state, DesiredState::Created);
    assert_eq!(record.observed_state, ObservedState::Creating);
    assert_eq!(record.vmm_pid, None);
    assert_eq!(record.vmm_start_time, None);
    assert_eq!(record.guest_cid, None);

    // Update observed state
    store
        .update_observed_state(session_id, ObservedState::Booting)
        .unwrap();
    let record = store.load_session(session_id).unwrap();
    assert_eq!(record.observed_state, ObservedState::Booting);

    // Update desired state
    store
        .update_desired_state(session_id, DesiredState::Running)
        .unwrap();
    let record = store.load_session(session_id).unwrap();
    assert_eq!(record.desired_state, DesiredState::Running);

    // Record VMM
    store.record_vmm(session_id, 456, 1234567, 33).unwrap();
    let record = store.load_session(session_id).unwrap();
    assert_eq!(record.vmm_pid, Some(456));
    assert_eq!(record.vmm_start_time, Some(1234567));
    assert_eq!(record.guest_cid, Some(33));

    // Check loading all sessions
    let all = store.load_all_sessions().unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].session_id, session_id);
}

#[test]
fn test_reconciliation_logic() {
    let _env = HERMETIC_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let store = SqliteWorkflowStore::in_memory().unwrap();
    let cid_allocator = CidAllocator::default();

    // Session 1: Running VMM with correct start-time -> should be adopted
    assert!(store
        .create_session("sess-1", None, "vm-1", "ws-1", "prof-1")
        .is_ok());
    store
        .update_observed_state("sess-1", ObservedState::Booting)
        .unwrap();
    store.record_vmm("sess-1", 101, 11111, 3).unwrap();
    store
        .update_observed_state("sess-1", ObservedState::Running)
        .unwrap();

    // Session 2: Running VMM but process is dead -> should be repaired to Failed
    assert!(store
        .create_session("sess-2", None, "vm-2", "ws-1", "prof-1")
        .is_ok());
    store
        .update_observed_state("sess-2", ObservedState::Booting)
        .unwrap();
    store.record_vmm("sess-2", 102, 22222, 4).unwrap();
    store
        .update_observed_state("sess-2", ObservedState::Running)
        .unwrap();

    // Session 3: Creating but no PID -> should be repaired to Failed (as dead)
    assert!(store
        .create_session("sess-3", None, "vm-3", "ws-1", "prof-1")
        .is_ok());
    store
        .update_observed_state("sess-3", ObservedState::Creating)
        .unwrap();

    // Mock running process PIDs and start times
    // 101 is active and correct
    // 105 is an untracked orphan firecracker process
    let mock_pids = vec![(101_u32, 11111_u64), (105_u32, 55555_u64)];

    let report = run_startup_reconciliation(&store, &cid_allocator, Some(mock_pids), None).unwrap();

    assert_eq!(report.adopted_sessions, 1);
    assert_eq!(report.reaped_orphans, 1); // 105 reaped
    assert_eq!(report.repaired_transitions, 2); // sess-2 and sess-3 repaired

    // Verifying states in DB
    let s1 = store.load_session("sess-1").unwrap();
    let s2 = store.load_session("sess-2").unwrap();
    let s3 = store.load_session("sess-3").unwrap();

    assert_eq!(s1.observed_state, ObservedState::Running);
    assert_eq!(s2.observed_state, ObservedState::Failed);
    assert_eq!(s3.observed_state, ObservedState::Failed);
}

#[test]
fn test_reconciliation_releases_locks_for_dead_sessions() {
    let _env = HERMETIC_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let store = SqliteWorkflowStore::in_memory().unwrap();
    let cid_allocator = CidAllocator::default();

    assert!(store
        .create_session("sess-1", None, "vm-1", "ws-live", "prof-1")
        .is_ok());
    store
        .update_observed_state("sess-1", ObservedState::Booting)
        .unwrap();
    store.record_vmm("sess-1", 101, 11111, 3).unwrap();
    store
        .update_observed_state("sess-1", ObservedState::Running)
        .unwrap();

    assert!(store
        .create_session("sess-2", None, "vm-2", "ws-dead", "prof-1")
        .is_ok());
    store
        .update_observed_state("sess-2", ObservedState::Booting)
        .unwrap();
    store.record_vmm("sess-2", 102, 22222, 4).unwrap();
    store
        .update_observed_state("sess-2", ObservedState::Running)
        .unwrap();

    let drive9 = std::env::temp_dir().join(format!("vam-reconcile-lock-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&drive9);
    std::fs::create_dir_all(drive9.join("workspaces/ws-live")).unwrap();
    std::fs::create_dir_all(drive9.join("workspaces/ws-dead")).unwrap();
    std::fs::write(drive9.join("workspaces/ws-live/write.lock"), "sess-1").unwrap();
    std::fs::write(drive9.join("workspaces/ws-dead/write.lock"), "sess-2").unwrap();

    let mock_pids = vec![(101_u32, 11111_u64)];
    let report = run_startup_reconciliation(
        &store,
        &cid_allocator,
        Some(mock_pids),
        Some(drive9.as_path()),
    )
    .unwrap();
    assert_eq!(report.adopted_sessions, 1);
    assert_eq!(report.repaired_transitions, 1);

    assert_eq!(
        std::fs::read_to_string(drive9.join("workspaces/ws-live/write.lock")).unwrap(),
        "sess-1"
    );
    assert!(!drive9.join("workspaces/ws-dead/write.lock").exists());

    let _ = std::fs::remove_dir_all(&drive9);
}

/// PB-1.9: a parked run is rows in the store and has no VM.
/// Reopening the database is a `vamd` restart. Reconciliation must adopt a
/// live VM, repair a dead one, and leave the parked run's checkpoint,
/// metering, reservation, and pending decision intact. A leftover pid that
/// matches the parked session is not adopted, and repairing another session
/// on the same workspace does not clear the reservation.
#[test]
fn parked_run_survives_vamd_restart_and_reconcile() {
    let _env = HERMETIC_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    use vam_spec::v1::{
        ArtifactRef, Digest, ExecutionLease, ResourceId, TaskOutcome, TaskOutcomeState,
    };

    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("workflow.db");
    let drive9 = dir.path().join("drive9");
    let run_id = "urn:vam:run:local.vam.dev/acme/run-parked-restart";
    let approval_id = "urn:vam:hitl:local.vam.dev/acme/approval-parked-restart";
    let parked_workspace = "ws-parked-restart";
    let checkpoint_bytes = format!(
        r#"{{"schema_version":1,"run_id":"{run_id}","lifecycle_generation":1,"marker":"parked-restart"}}"#
    )
    .into_bytes();
    let checkpoint_digest = format!(
        "{:x}",
        <sha2::Sha256 as sha2::Digest>::digest(&checkpoint_bytes)
    );

    let reservation_created_at;
    {
        let store = SqliteWorkflowStore::open(&db_path).expect("open");

        store
            .create_session("sess-live", None, "vm-live", "ws-live", "prof-1")
            .unwrap();
        store
            .update_observed_state("sess-live", ObservedState::Booting)
            .unwrap();
        store.record_vmm("sess-live", 201, 11111, 3).unwrap();
        store
            .update_observed_state("sess-live", ObservedState::Running)
            .unwrap();

        store
            .create_session("sess-dead", None, "vm-dead", "ws-dead", "prof-1")
            .unwrap();
        store
            .update_observed_state("sess-dead", ObservedState::Booting)
            .unwrap();
        store.record_vmm("sess-dead", 202, 22222, 4).unwrap();
        store
            .update_observed_state("sess-dead", ObservedState::Running)
            .unwrap();

        store
            .create_session("sess-parked", None, "vm-parked", parked_workspace, "prof-1")
            .unwrap();
        store
            .update_observed_state("sess-parked", ObservedState::Booting)
            .unwrap();
        store.record_vmm("sess-parked", 12345, 33333, 5).unwrap();
        store
            .update_observed_state("sess-parked", ObservedState::Running)
            .unwrap();
        store
            .update_observed_state("sess-parked", ObservedState::Terminated)
            .unwrap();

        // A second, non-terminal session on the reserved workspace. Repairing
        // it must not clear the parked run's reservation.
        store
            .create_session("sess-stray", None, "vm-stray", parked_workspace, "prof-1")
            .unwrap();

        store
            .persist_run_spec(
                run_id,
                r#"{"inputs":{"prompt":"finish after approval"},"maxDuration":"30s"}"#,
                Some("sess-parked"),
                "urn:vam:agent:test",
                None,
                "User::\"local\"",
            )
            .unwrap();
        store
            .put_run_checkpoint(run_id, 1, &checkpoint_digest, &checkpoint_bytes)
            .unwrap();

        let budget = vam_inference::SessionBudget::new(None);
        budget
            .record_usage(6, 4, 1_000_000, 1_000_000)
            .expect("usage");
        store.apply_run_metering(run_id, &budget).unwrap();
        store.apply_run_metering(run_id, &budget).unwrap();

        let lease = ExecutionLease {
            run_id: Some(ResourceId {
                urn: run_id.to_string(),
            }),
            step_id: "step-1".to_string(),
            attempt_id: Some(ResourceId {
                urn: "urn:vam:attempt:local.vam.dev/acme/attempt-parked-restart".to_string(),
            }),
            lease_id: Some(ResourceId {
                urn: "urn:vam:lease:local.vam.dev/acme/lease-parked-restart".to_string(),
            }),
            lifecycle_generation: 1,
            fencing_token: vec![1],
            expires_at: Some(prost_types::Timestamp {
                seconds: 4_102_444_800,
                nanos: 0,
            }),
            idempotency_key: "idempotency-parked-restart".to_string(),
        };
        let key = AttemptKey {
            run_id: run_id.to_string(),
            step_id: "step-1".to_string(),
            attempt_id: lease.attempt_id.as_ref().unwrap().urn.clone(),
        };
        let approval = ResourceId {
            urn: approval_id.to_string(),
        };
        let artifact = ArtifactRef {
            artifact_id: Some(ResourceId {
                urn: "urn:vam:artifact:local.vam.dev/acme/checkpoint-parked-restart".to_string(),
            }),
            digest: Some(Digest {
                algorithm: 1,
                hex_hash: checkpoint_digest.clone(),
            }),
            media_type: "application/json".to_string(),
            size_bytes: checkpoint_bytes.len() as i64,
            uri: "memory://checkpoint".to_string(),
        };
        store.issue_lease(&lease).unwrap();
        store
            .register_hitl_suspend(
                &key,
                "sess-parked",
                &approval,
                "jira_transition_issue",
                r#"{"issue_key":"CGA-1"}"#,
                "digest-parked-restart",
                Some(4_102_444_800),
            )
            .unwrap();
        store
            .record_outcome(&TaskOutcome {
                lease: Some(lease),
                state: TaskOutcomeState::Suspended as i32,
                continuation: Some(artifact.clone()),
                ..Default::default()
            })
            .unwrap();
        store
            .create_approval(&key, &approval, &artifact, 1)
            .unwrap();
        store.update_run_state(run_id, "PAUSED").unwrap();
        store
            .reserve_workspace(parked_workspace, run_id, 4_102_444_800)
            .unwrap();
        store.clear_keep_warm(run_id).unwrap();
        reservation_created_at = store
            .workspace_reservation(parked_workspace)
            .unwrap()
            .expect("reservation before restart")
            .created_at;

        std::fs::create_dir_all(drive9.join("workspaces/ws-live")).unwrap();
        std::fs::create_dir_all(drive9.join("workspaces/ws-dead")).unwrap();
        std::fs::create_dir_all(drive9.join("workspaces").join(parked_workspace)).unwrap();
        std::fs::write(drive9.join("workspaces/ws-live/write.lock"), "sess-live").unwrap();
        std::fs::write(drive9.join("workspaces/ws-dead/write.lock"), "sess-dead").unwrap();
        std::fs::write(
            drive9
                .join("workspaces")
                .join(parked_workspace)
                .join("write.lock"),
            "sess-parked",
        )
        .unwrap();
    }

    let store = SqliteWorkflowStore::open(&db_path).expect("reopen after restart");
    let cid_allocator = CidAllocator::default();
    // 201 is the live VM. 12345 is the parked session's old pid with a matching
    // start time: it must be reaped as an orphan, not adopted.
    let report = run_startup_reconciliation(
        &store,
        &cid_allocator,
        Some(vec![(201, 11111), (12345, 33333)]),
        Some(drive9.as_path()),
    )
    .unwrap();

    assert_eq!(report.adopted_sessions, 1);
    assert_eq!(report.reaped_orphans, 1);
    assert_eq!(report.repaired_transitions, 2);
    assert_eq!(report.freed_cids, vec![4]);
    assert!(cid_allocator.is_allocated(3));
    assert!(!cid_allocator.is_allocated(4));
    assert!(!cid_allocator.is_allocated(5));

    assert_eq!(
        store.load_session("sess-live").unwrap().observed_state,
        ObservedState::Running
    );
    assert_eq!(
        store.load_session("sess-dead").unwrap().observed_state,
        ObservedState::Failed
    );
    assert_eq!(
        store.load_session("sess-parked").unwrap().observed_state,
        ObservedState::Terminated,
        "a parked session is not adopted or repaired"
    );
    assert_eq!(
        store.load_session("sess-stray").unwrap().observed_state,
        ObservedState::Failed
    );

    assert_eq!(
        std::fs::read_to_string(drive9.join("workspaces/ws-live/write.lock")).unwrap(),
        "sess-live"
    );
    assert!(!drive9.join("workspaces/ws-dead/write.lock").exists());
    assert!(
        !drive9
            .join("workspaces")
            .join(parked_workspace)
            .join("write.lock")
            .exists(),
        "a stale write.lock is not the parked reservation"
    );

    assert_eq!(store.get_run_state(run_id).unwrap(), "PAUSED");
    let reservation = store
        .workspace_reservation(parked_workspace)
        .unwrap()
        .expect("reservation survives restart and reconcile");
    assert_eq!(reservation.run_id, run_id);
    assert_eq!(reservation.expires_at, 4_102_444_800);
    assert_eq!(reservation.created_at, reservation_created_at);
    assert!(store.keep_warm_deadline(run_id).unwrap().is_none());

    let checkpoint = store
        .latest_run_checkpoint(run_id)
        .unwrap()
        .expect("checkpoint");
    assert_eq!(checkpoint.lifecycle_generation, 1);
    assert_eq!(checkpoint.digest, checkpoint_digest);
    assert_eq!(checkpoint.bytes, checkpoint_bytes);

    let metering = store.get_run_metering(run_id).unwrap().expect("metering");
    assert_eq!(metering.iteration, 2);
    assert_eq!(metering.spend_micro, 10);
    assert_eq!(metering.token_count, 10);
    assert_eq!(metering.prompt_tokens, 6);
    assert_eq!(metering.completion_tokens, 4);

    let pending = store
        .list_published_approvals()
        .unwrap()
        .into_iter()
        .find(|approval| approval.request_id == approval_id)
        .expect("pending decision");
    assert!(pending.decision.is_none());
    assert!(pending.published);
    assert_eq!(pending.tool_name, "jira_transition_issue");
    assert_eq!(pending.requested_args_json, r#"{"issue_key":"CGA-1"}"#);
    assert_eq!(pending.run_id, run_id);
    let stored = store.get_approval(approval_id).unwrap();
    assert!(stored.decision.is_none());
    assert_eq!(stored.decision_revision, pending.decision_revision);
}

#[tokio::test]
async fn test_session_manager_lifecycle_with_handshake() {
    let _env = HERMETIC_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let cid_allocator = Arc::new(CidAllocator::default());
    let session_id = "session-test";
    let vm_id = "vm-test";

    let vmm_factory: VmmFactory = Arc::new(|_vm_id, _guest_cid| Box::new(MockVmm::new()));

    let (manager, cmd_rx) =
        SessionManager::new(store.clone(), cid_allocator.clone(), vmm_factory.clone());

    let (workspace_manager, catalog_cache) =
        drive9_runtime().expect("drive9 runtime should initialize");

    let net_pool = Arc::new(NetResourcePool::for_tests());

    // Spawn the command manager loop
    let manager_handle = tokio::spawn(run_manager_loop(
        store.clone(),
        SecretsVault::for_tests(store.clone()),
        cid_allocator.clone(),
        vmm_factory.clone(),
        workspace_manager,
        catalog_cache,
        net_pool,
        cmd_rx,
        vam_telemetry::TelemetryCapture::default(),
    ));

    // Setup vsock uds socket path (mimics the one in manager.rs)
    let vsock_uds_path = format!("/tmp/vam-vsock-{}.socket", vm_id);
    let vsock_uds_path_with_port = format!("{}_52", vsock_uds_path);
    let _ = std::fs::remove_file(&vsock_uds_path_with_port);

    // Spawn guest handshake task in background
    let vsock_uds_path_clone = vsock_uds_path_with_port.clone();
    let vm_id_clone = vm_id.to_string();
    tokio::spawn(async move {
        // Wait for the socket file to be created by manager.rs binding
        let mut stream = None;
        for _ in 0..100 {
            if let Ok(s) = UnixStream::connect(&vsock_uds_path_clone).await {
                stream = Some(s);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }

        let stream = stream.expect("Mock guest failed to connect to vsock UDS socket");

        // Complete the handshake
        vam_vsock::run_guest_registration(stream, &vm_id_clone, 3)
            .await
            .unwrap();
    });

    // Request creation of session!
    manager
        .create_session(
            session_id.to_string(),
            None,
            vm_id.to_string(),
            "workspace-1".to_string(),
            "profile-1".to_string(),
            None,
        )
        .await
        .unwrap();

    // Verify that session is now RUNNING!
    let record = store.load_session(session_id).unwrap();
    assert_eq!(record.observed_state, ObservedState::Running);
    assert_eq!(record.vmm_pid, Some(12345)); // MockVmm default pid
    assert_eq!(
        record.tap_device, None,
        "default brokered sessions must not record a TAP device"
    );

    // Request termination of session!
    manager
        .terminate_session(session_id.to_string())
        .await
        .unwrap();

    // Verify that session is now TERMINATED!
    let record = store.load_session(session_id).unwrap();
    assert_eq!(record.observed_state, ObservedState::Terminated);

    // Cleanup socket
    let _ = std::fs::remove_file(&vsock_uds_path_with_port);

    // Drop and stop manager
    drop(manager);
    manager_handle.await.unwrap();
}

/// AR-2.6: a stop request takes the VM down and leaves the tree and the
/// workspace lock to the run's finalizer; the finalizer's full terminate then
/// settles the workspace once, and a repeat is a no-op.
#[tokio::test]
async fn a_stop_takes_the_vm_down_and_the_full_terminate_settles_the_workspace_once() {
    let _env = HERMETIC_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let cid_allocator = Arc::new(CidAllocator::default());
    let session_id = "session-stop";
    let vm_id = "vm-stop";
    let workspace_id = "workspace-stop";

    let vmm_factory: VmmFactory = Arc::new(|_vm_id, _guest_cid| Box::new(MockVmm::new()));
    let (manager, cmd_rx) =
        SessionManager::new(store.clone(), cid_allocator.clone(), vmm_factory.clone());
    let (workspace_manager, catalog_cache) =
        drive9_runtime().expect("drive9 runtime should initialize");
    let net_pool = Arc::new(NetResourcePool::for_tests());
    let manager_handle = tokio::spawn(run_manager_loop(
        store.clone(),
        SecretsVault::for_tests(store.clone()),
        cid_allocator.clone(),
        vmm_factory.clone(),
        workspace_manager,
        catalog_cache,
        net_pool,
        cmd_rx,
        vam_telemetry::TelemetryCapture::default(),
    ));

    let vsock_uds_path_with_port = format!("/tmp/vam-vsock-{}.socket_52", vm_id);
    let _ = std::fs::remove_file(&vsock_uds_path_with_port);
    let socket = vsock_uds_path_with_port.clone();
    tokio::spawn(async move {
        let mut stream = None;
        for _ in 0..100 {
            if let Ok(s) = UnixStream::connect(&socket).await {
                stream = Some(s);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let stream = stream.expect("mock guest connects");
        vam_vsock::run_guest_registration(stream, "vm-stop", 3)
            .await
            .unwrap();
    });
    manager
        .create_session(
            session_id.to_string(),
            None,
            vm_id.to_string(),
            workspace_id.to_string(),
            "profile-1".to_string(),
            None,
        )
        .await
        .unwrap();

    let layout = vam_orchestrator::manager::drive9_layout();
    assert_eq!(
        vam_storage::read_workspace_lock_holder(&layout, workspace_id).unwrap(),
        Some(session_id.to_string())
    );

    // The run is dispatched and a stop is requested.
    store.session_ends.track(session_id);
    assert!(store.session_ends.begin_stop(session_id));
    manager
        .stop_session_vm(session_id.to_string())
        .await
        .unwrap();

    // The VM is down; the tree and the lock wait for the finalizer.
    let record = store.load_session(session_id).unwrap();
    assert_eq!(record.observed_state, ObservedState::Terminated);
    assert_eq!(
        vam_storage::read_workspace_lock_holder(&layout, workspace_id).unwrap(),
        Some(session_id.to_string()),
        "the lock is held until the workspace is settled"
    );
    assert!(layout.session_dir(session_id).exists());
    assert!(store.session_ends.settle_pending(session_id));
    assert!(
        !store
            .session_ends
            .wait_settled(session_id, std::time::Duration::from_millis(50))
            .await,
        "nothing settled yet"
    );

    // The finalizer's terminate settles it.
    manager
        .terminate_session(session_id.to_string())
        .await
        .unwrap();
    assert!(!store.session_ends.settle_pending(session_id));
    assert!(
        store
            .session_ends
            .wait_settled(session_id, std::time::Duration::from_secs(1))
            .await
    );
    assert_eq!(
        vam_storage::read_workspace_lock_holder(&layout, workspace_id).unwrap(),
        None
    );
    let head = vam_storage::ManifestManager::new(
        layout.clone(),
        vam_storage::ChunkStore::new(layout.clone()),
    )
    .read_head(workspace_id)
    .unwrap();
    assert!(head.is_some(), "the workspace was committed");

    // A repeat is a no-op, and a stop of a finished session changes nothing.
    manager
        .terminate_session(session_id.to_string())
        .await
        .unwrap();
    manager
        .stop_session_vm(session_id.to_string())
        .await
        .unwrap();
    assert_eq!(
        store.load_session(session_id).unwrap().observed_state,
        ObservedState::Terminated
    );
    assert!(store.kept_for_session(session_id).unwrap().is_none());

    let _ = std::fs::remove_file(&vsock_uds_path_with_port);
    drop(manager);
    manager_handle.await.unwrap();
}

/// PB-1.1: an expired HITL approval terminates the session it parked
/// and releases that session's workspace write lock. A second sweep is a no-op.
#[tokio::test]
async fn test_expired_approval_terminates_session_and_releases_lock() {
    let _env = HERMETIC_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    use vam_spec::v1::{
        ArtifactRef, Digest, ExecutionLease, ResourceId, TaskOutcome, TaskOutcomeState,
    };

    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let cid_allocator = Arc::new(CidAllocator::default());
    let vmm_factory: VmmFactory = Arc::new(|_vm_id, _guest_cid| Box::new(MockVmm::new()));

    let session_id = "sess-expiry";
    let vm_id = "vm-expiry";
    let workspace_id = "ws-expiry";

    // Live session (mock pid 12345) holding its workspace write lock.
    store
        .create_session(session_id, None, vm_id, workspace_id, "prof-1")
        .unwrap();
    store
        .update_observed_state(session_id, ObservedState::Booting)
        .unwrap();
    store.record_vmm(session_id, 12345, 11111, 3).unwrap();
    store
        .update_observed_state(session_id, ObservedState::Running)
        .unwrap();

    let drive9 = std::env::temp_dir().join(format!("vam-expiry-sweep-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&drive9);
    std::fs::create_dir_all(drive9.join("workspaces").join(workspace_id)).unwrap();
    std::fs::write(
        drive9
            .join("workspaces")
            .join(workspace_id)
            .join("write.lock"),
        session_id,
    )
    .unwrap();
    let layout = vam_storage::Drive9Layout::new(drive9.clone());
    assert_eq!(
        vam_storage::read_workspace_lock_holder(&layout, workspace_id).unwrap(),
        Some(session_id.to_string())
    );

    // Approval parked on that session, with an expiry already in the past.
    let run = ResourceId {
        urn: "urn:vam:run:local.vam.dev/acme/run-expiry".to_string(),
    };
    let attempt = ResourceId {
        urn: "urn:vam:attempt:local.vam.dev/acme/attempt-expiry".to_string(),
    };
    let lease = ExecutionLease {
        run_id: Some(run.clone()),
        step_id: "step-1".to_string(),
        attempt_id: Some(attempt.clone()),
        lease_id: Some(ResourceId {
            urn: "urn:vam:lease:local.vam.dev/acme/lease-expiry".to_string(),
        }),
        lifecycle_generation: 1,
        fencing_token: vec![1],
        expires_at: Some(prost_types::Timestamp {
            seconds: 4_102_444_800,
            nanos: 0,
        }),
        idempotency_key: "idempotency-expiry".to_string(),
    };
    let key = AttemptKey {
        run_id: run.urn.clone(),
        step_id: "step-1".to_string(),
        attempt_id: attempt.urn.clone(),
    };
    let approval = ResourceId {
        urn: "urn:vam:hitl:local.vam.dev/acme/approval-expiry".to_string(),
    };
    let checkpoint = ArtifactRef {
        artifact_id: Some(ResourceId {
            urn: "urn:vam:artifact:local.vam.dev/acme/checkpoint-expiry".to_string(),
        }),
        digest: Some(Digest {
            algorithm: 1,
            hex_hash: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
                .to_string(),
        }),
        media_type: "application/octet-stream".to_string(),
        size_bytes: 0,
        uri: "memory://checkpoint".to_string(),
    };
    store.create_run(&run).unwrap();
    store.issue_lease(&lease).unwrap();
    store
        .register_hitl_suspend(
            &key,
            session_id,
            &approval,
            "jira_create_issue",
            r#"{"title":"issue"}"#,
            "abc123",
            Some(1), // expires_at: 1970, long past
        )
        .unwrap();
    store
        .record_outcome(&TaskOutcome {
            lease: Some(lease.clone()),
            state: TaskOutcomeState::Suspended as i32,
            output: None,
            output_artifacts: Vec::new(),
            continuation: Some(checkpoint.clone()),
            evidence: None,
            observed_usage: None,
            error: None,
        })
        .unwrap();
    store
        .create_approval(&key, &approval, &checkpoint, 1)
        .unwrap();
    // Still inside the keep-warm window, and a reservation is already held.
    // Expiry must drop both while the VM is warm.
    store
        .schedule_keep_warm(&run.urn, unix_now() + 600)
        .unwrap();
    store
        .reserve_workspace(workspace_id, &run.urn, unix_now() + 3_600)
        .unwrap();

    // Real session manager loop, as in the lifecycle test above.
    let (manager, cmd_rx) =
        SessionManager::new(store.clone(), cid_allocator.clone(), vmm_factory.clone());
    let (workspace_manager, catalog_cache) =
        drive9_runtime().expect("drive9 runtime should initialize");
    let net_pool = Arc::new(NetResourcePool::for_tests());
    let manager_handle = tokio::spawn(run_manager_loop(
        store.clone(),
        SecretsVault::for_tests(store.clone()),
        cid_allocator.clone(),
        vmm_factory.clone(),
        workspace_manager,
        catalog_cache,
        net_pool,
        cmd_rx,
        vam_telemetry::TelemetryCapture::default(),
    ));

    // First sweep: expires the approval, terminates the session, frees the lock.
    let swept = sweep_expired_approvals(&store, &manager, &layout)
        .await
        .unwrap();
    assert_eq!(swept, 1);

    let record = store.load_session(session_id).unwrap();
    assert_eq!(record.observed_state, ObservedState::Terminated);
    assert_eq!(
        vam_storage::read_workspace_lock_holder(&layout, workspace_id).unwrap(),
        None,
        "expiry must release the workspace write lock"
    );
    assert_eq!(
        store
            .get_approval(&approval.urn)
            .unwrap()
            .decision
            .as_deref(),
        Some("EXPIRED")
    );
    assert!(
        store.workspace_reservation(workspace_id).unwrap().is_none(),
        "expiry must release the workspace reservation"
    );
    assert_eq!(
        store.keep_warm_deadline(&run.urn).unwrap(),
        None,
        "expiry must clear the keep-warm deadline"
    );

    // Second sweep: nothing left to expire, session stays terminated.
    let swept_again = sweep_expired_approvals(&store, &manager, &layout)
        .await
        .unwrap();
    assert_eq!(swept_again, 0);
    assert_eq!(
        store.load_session(session_id).unwrap().observed_state,
        ObservedState::Terminated
    );

    let _ = std::fs::remove_dir_all(&drive9);
    drop(manager);
    manager_handle.await.unwrap();
}

/// A run that has used its active `max_duration` loses the warm VM
/// and the workspace reservation. A second pass is a no-op.
#[tokio::test]
async fn test_active_duration_terminates_warm_vm_and_releases_reservation() {
    let _env = HERMETIC_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let cid_allocator = Arc::new(CidAllocator::default());
    let vmm_factory: VmmFactory = Arc::new(|_vm_id, _guest_cid| Box::new(MockVmm::new()));

    let session_id = "sess-duration";
    let vm_id = "vm-duration";
    let workspace_id = "ws-duration";
    let run_id = "urn:vam:run:local.vam.dev/acme/run-duration";

    store
        .create_session(session_id, None, vm_id, workspace_id, "prof-1")
        .unwrap();
    store
        .update_observed_state(session_id, ObservedState::Booting)
        .unwrap();
    store.record_vmm(session_id, 12345, 11111, 3).unwrap();
    store
        .update_observed_state(session_id, ObservedState::Running)
        .unwrap();

    let drive9 = std::env::temp_dir().join(format!(
        "vam-duration-sweep-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&drive9);
    std::fs::create_dir_all(drive9.join("workspaces").join(workspace_id)).unwrap();
    std::fs::write(
        drive9
            .join("workspaces")
            .join(workspace_id)
            .join("write.lock"),
        session_id,
    )
    .unwrap();
    let layout = vam_storage::Drive9Layout::new(drive9.clone());

    store
        .persist_run_spec(
            run_id,
            r#"{"maxDuration":"30s"}"#,
            Some(session_id),
            "urn:vam:agent:test",
            None,
            "User::\"tester\"",
        )
        .unwrap();
    let now = unix_now();
    store.begin_active_segment(run_id, now - 100).unwrap();
    store
        .reserve_workspace(workspace_id, run_id, now + 3_600)
        .unwrap();
    store.schedule_keep_warm(run_id, now + 600).unwrap();

    let (manager, cmd_rx) =
        SessionManager::new(store.clone(), cid_allocator.clone(), vmm_factory.clone());
    let (workspace_manager, catalog_cache) =
        drive9_runtime().expect("drive9 runtime should initialize");
    let net_pool = Arc::new(NetResourcePool::for_tests());
    let manager_handle = tokio::spawn(run_manager_loop(
        store.clone(),
        SecretsVault::for_tests(store.clone()),
        cid_allocator,
        vmm_factory,
        workspace_manager,
        catalog_cache,
        net_pool,
        cmd_rx,
        vam_telemetry::TelemetryCapture::default(),
    ));

    let swept = sweep_active_duration(&store, &manager, &layout)
        .await
        .unwrap();
    assert_eq!(swept, 1);
    assert_eq!(
        store.load_session(session_id).unwrap().observed_state,
        ObservedState::Terminated
    );
    assert_eq!(
        vam_storage::read_workspace_lock_holder(&layout, workspace_id).unwrap(),
        None
    );
    assert!(store.workspace_reservation(workspace_id).unwrap().is_none());
    assert_eq!(store.keep_warm_deadline(run_id).unwrap(), None);
    assert_eq!(store.get_run_state(run_id).unwrap(), "FAILED");

    let swept_again = sweep_active_duration(&store, &manager, &layout)
        .await
        .unwrap();
    assert_eq!(swept_again, 0);

    let _ = std::fs::remove_dir_all(&drive9);
    drop(manager);
    manager_handle.await.unwrap();
}

/// After keep-warm with no decision, the host flushes the guest (via terminate),
/// ends the session, and holds the workspace for the parked run.
#[tokio::test]
async fn test_keep_warm_parks_run_and_reserves_workspace() {
    let _env = HERMETIC_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    use vam_spec::v1::{
        ArtifactRef, Digest, ExecutionLease, ResourceId, TaskOutcome, TaskOutcomeState,
    };

    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let cid_allocator = Arc::new(CidAllocator::default());
    let vmm_factory: VmmFactory = Arc::new(|_vm_id, _guest_cid| Box::new(MockVmm::new()));

    let session_id = "sess-park";
    let vm_id = "vm-park";
    let workspace_id = "ws-park";
    let run_urn = "urn:vam:run:local.vam.dev/acme/run-park";

    store
        .create_session(session_id, None, vm_id, workspace_id, "prof-1")
        .unwrap();
    store
        .update_observed_state(session_id, ObservedState::Booting)
        .unwrap();
    store.record_vmm(session_id, 12345, 22222, 4).unwrap();
    store
        .update_observed_state(session_id, ObservedState::Running)
        .unwrap();

    let drive9 = std::env::temp_dir().join(format!("vam-park-sweep-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&drive9);
    std::fs::create_dir_all(drive9.join("workspaces").join(workspace_id)).unwrap();
    std::fs::write(
        drive9
            .join("workspaces")
            .join(workspace_id)
            .join("write.lock"),
        session_id,
    )
    .unwrap();
    let layout = vam_storage::Drive9Layout::new(drive9.clone());

    let run = ResourceId {
        urn: run_urn.to_string(),
    };
    let attempt = ResourceId {
        urn: "urn:vam:attempt:local.vam.dev/acme/attempt-park".to_string(),
    };
    let lease = ExecutionLease {
        run_id: Some(run.clone()),
        step_id: "step-1".to_string(),
        attempt_id: Some(attempt.clone()),
        lease_id: Some(ResourceId {
            urn: "urn:vam:lease:local.vam.dev/acme/lease-park".to_string(),
        }),
        lifecycle_generation: 1,
        fencing_token: vec![1],
        expires_at: Some(prost_types::Timestamp {
            seconds: 4_102_444_800,
            nanos: 0,
        }),
        idempotency_key: "idempotency-park".to_string(),
    };
    let key = AttemptKey {
        run_id: run.urn.clone(),
        step_id: "step-1".to_string(),
        attempt_id: attempt.urn.clone(),
    };
    let approval = ResourceId {
        urn: "urn:vam:hitl:local.vam.dev/acme/approval-park".to_string(),
    };
    let checkpoint = ArtifactRef {
        artifact_id: Some(ResourceId {
            urn: "urn:vam:artifact:local.vam.dev/acme/checkpoint-park".to_string(),
        }),
        digest: Some(Digest {
            algorithm: 1,
            hex_hash: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
                .to_string(),
        }),
        media_type: "application/octet-stream".to_string(),
        size_bytes: 0,
        uri: "memory://checkpoint".to_string(),
    };
    store.create_run(&run).unwrap();
    store.issue_lease(&lease).unwrap();
    store
        .register_hitl_suspend(
            &key,
            session_id,
            &approval,
            "jira_create_issue",
            r#"{"title":"issue"}"#,
            "abc123",
            Some(4_102_444_800),
        )
        .unwrap();
    store
        .record_outcome(&TaskOutcome {
            lease: Some(lease.clone()),
            state: TaskOutcomeState::Suspended as i32,
            output: None,
            output_artifacts: Vec::new(),
            continuation: Some(checkpoint.clone()),
            evidence: None,
            observed_usage: None,
            error: None,
        })
        .unwrap();
    store
        .create_approval(&key, &approval, &checkpoint, 1)
        .unwrap();
    store.update_run_state(run_urn, "PAUSED").unwrap();
    // Deadline already passed: the sweep must not sleep for the 5 minute default.
    store.schedule_keep_warm(run_urn, 1).unwrap();

    let (manager, cmd_rx) =
        SessionManager::new(store.clone(), cid_allocator.clone(), vmm_factory.clone());
    let (workspace_manager, catalog_cache) =
        drive9_runtime().expect("drive9 runtime should initialize");
    let net_pool = Arc::new(NetResourcePool::for_tests());
    let manager_handle = tokio::spawn(run_manager_loop(
        store.clone(),
        SecretsVault::for_tests(store.clone()),
        cid_allocator.clone(),
        vmm_factory.clone(),
        workspace_manager,
        catalog_cache,
        net_pool,
        cmd_rx,
        vam_telemetry::TelemetryCapture::default(),
    ));

    store.register_thread_recorder(
        session_id,
        Arc::new(
            vam_orchestrator::thread_recorder::ThreadRecorder::with_resolver(
                session_id.to_string(),
                std::env::temp_dir().join("vam-parked-recorder"),
                Arc::new(|| Err("no scope".to_string())),
            ),
        ),
    );
    let parked = sweep_keep_warm(&store, &manager, &layout).await.unwrap();
    assert_eq!(parked, 1);
    assert!(
        store.thread_recorder(session_id).is_none(),
        "a parked session's recorder is dropped with the session"
    );

    let record = store.load_session(session_id).unwrap();
    assert_eq!(record.observed_state, ObservedState::Terminated);
    assert_eq!(
        vam_storage::read_workspace_lock_holder(&layout, workspace_id).unwrap(),
        None,
        "park releases the session write lock after the guest shutdown"
    );
    assert_eq!(store.get_run_state(run_urn).unwrap(), "PAUSED");
    let reservation = store
        .workspace_reservation(workspace_id)
        .unwrap()
        .expect("workspace stays reserved for the parked run");
    assert_eq!(reservation.run_id, run_urn);
    assert_eq!(reservation.expires_at, 4_102_444_800);
    assert!(store.keep_warm_deadline(run_urn).unwrap().is_none());

    let parked_again = sweep_keep_warm(&store, &manager, &layout).await.unwrap();
    assert_eq!(parked_again, 0);
    assert_eq!(
        store
            .workspace_reservation(workspace_id)
            .unwrap()
            .expect("reservation survives a second sweep")
            .run_id,
        run_urn
    );

    let _ = std::fs::remove_dir_all(&drive9);
    drop(manager);
    manager_handle.await.unwrap();
}

#[tokio::test]
async fn test_restricted_direct_fails_without_tap_pool() {
    let _env = HERMETIC_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let cid_allocator = Arc::new(CidAllocator::default());
    let session_id = "session-rd-fail";
    let vm_id = "vm-rd-fail";

    let vmm_factory: VmmFactory = Arc::new(|_vm_id, _guest_cid| Box::new(MockVmm::new()));

    let (manager, cmd_rx) =
        SessionManager::new(store.clone(), cid_allocator.clone(), vmm_factory.clone());

    let (workspace_manager, catalog_cache) =
        drive9_runtime().expect("drive9 runtime should initialize");

    let net_pool =
        Arc::new(NetResourcePool::for_tests().with_default_mode(NetworkMode::RestrictedDirect));

    let manager_handle = tokio::spawn(run_manager_loop(
        store.clone(),
        SecretsVault::for_tests(store.clone()),
        cid_allocator.clone(),
        vmm_factory.clone(),
        workspace_manager,
        catalog_cache,
        net_pool,
        cmd_rx,
        vam_telemetry::TelemetryCapture::default(),
    ));

    let err = manager
        .create_session(
            session_id.to_string(),
            None,
            vm_id.to_string(),
            "workspace-1".to_string(),
            "profile-1".to_string(),
            None,
        )
        .await
        .expect_err("Restricted Direct should fail when TAP pool is empty");

    assert!(
        err.to_string().contains("TAP pool provisioning"),
        "unexpected error: {}",
        err
    );

    let record = store.load_session(session_id).unwrap();
    assert_eq!(record.observed_state, ObservedState::Failed);
    assert_eq!(record.tap_device, None);

    drop(manager);
    manager_handle.await.unwrap();
}

fn install_fake_golden_snapshot(drive9_root: &std::path::Path) {
    std::fs::create_dir_all(drive9_root).expect("drive9 root");
    let kernel = drive9_root.join("test-vmlinux");
    let rootfs = drive9_root.join("test-rootfs.ext4");
    std::fs::write(&kernel, b"test-kernel-bytes").expect("kernel stub");
    std::fs::write(&rootfs, b"test-rootfs-bytes").expect("rootfs stub");
    std::env::set_var("VAM_KERNEL_PATH", &kernel);
    std::env::set_var("VAM_ROOTFS_PATH", &rootfs);

    let key = vam_orchestrator::golden::compute_golden_key(
        &kernel,
        &rootfs,
        vam_orchestrator::golden::GOLDEN_VCPU,
        vam_orchestrator::golden::GOLDEN_MEM_MIB,
    )
    .expect("golden key");
    let paths = vam_orchestrator::golden::golden_paths_for_key(
        &vam_orchestrator::golden::golden_root(drive9_root),
        &key,
    );
    std::fs::create_dir_all(&paths.dir).expect("golden dir");
    std::fs::write(&paths.vm_state, b"fake-state").expect("vm state");
    std::fs::write(&paths.vm_mem, b"fake-mem").expect("vm mem");
    let companion = std::fs::File::create(&paths.companion_disk).expect("companion");
    companion
        .set_len(vam_orchestrator::golden::GOLDEN_DISK_BYTES)
        .expect("companion size");
    let meta = vam_orchestrator::golden::GoldenKeyMeta {
        key: key.clone(),
        kernel_hash: "test-kernel-hash".into(),
        rootfs_hash: "test-rootfs-hash".into(),
        vcpu_count: vam_orchestrator::golden::GOLDEN_VCPU,
        mem_mib: vam_orchestrator::golden::GOLDEN_MEM_MIB,
        guest_cid: vam_orchestrator::golden::GOLDEN_GUEST_CID,
    };
    std::fs::write(
        &paths.key_json,
        serde_json::to_string(&meta).expect("key json"),
    )
    .expect("write key");
}

async fn mock_vsock_guest_for_restore(vsock_base: String) {
    use prost::Message;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixListener;
    use vam_spec::v1::{ResumeFromSnapshotRequest, ResumeFromSnapshotResponse, VsockEnvelope};
    use vam_vsock::frame::{Frame, FrameType};

    let _ = std::fs::remove_file(&vsock_base);
    let listener = UnixListener::bind(&vsock_base).expect("bind mux uds");
    let (mut stream, _) = listener.accept().await.expect("mux accept");

    let mut reader = BufReader::new(&mut stream);
    let mut connect_line = String::new();
    reader
        .read_line(&mut connect_line)
        .await
        .expect("connect line");
    assert!(connect_line.starts_with("CONNECT 52"));
    stream.write_all(b"OK 52\n").await.expect("connect ok");
    stream.flush().await.expect("flush ok");

    let frame = Frame::read_from(&mut stream).await.expect("reseed frame");
    assert_eq!(frame.frame_type, FrameType::ControlGrpc);
    let env = VsockEnvelope::decode(&*frame.payload).expect("reseed env");
    assert_eq!(env.method, "ResumeFromSnapshot");
    let _req = ResumeFromSnapshotRequest::decode(&*env.payload).expect("reseed req");

    let response = ResumeFromSnapshotResponse { accepted: true };
    let mut resp_payload = Vec::new();
    response.encode(&mut resp_payload).unwrap();
    let env_out = VsockEnvelope {
        method: "ResumeFromSnapshotResponse".to_string(),
        correlation_id: env.correlation_id,
        payload: resp_payload,
        error: None,
        trace_context: None,
    };
    let mut env_payload = Vec::new();
    env_out.encode(&mut env_payload).unwrap();
    let frame_out = Frame {
        frame_type: FrameType::ControlGrpc,
        correlation_id: env.correlation_id,
        payload: bytes::Bytes::from(env_payload),
    };
    frame_out.write_to(&mut stream).await.expect("reseed reply");
}

#[tokio::test]
async fn test_golden_restore_skips_registration_handshake() {
    let _env = HERMETIC_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    // The golden snapshot is ~1.6 GB. Keep the guard alive for the whole test so
    // the directory is removed at the end, not left behind in the temp dir.
    let drive9_guard = tempfile::tempdir().expect("drive9 temp");
    let drive9 = drive9_guard.path().to_path_buf();
    std::env::set_var("VAM_DRIVE9_ROOT", &drive9);
    std::env::set_var("VAM_SNAPSHOT_RESTORE", "1");
    // Default sessions get content-sized images; restore must not need the 1 GiB geometry.
    std::env::remove_var("VAM_GOLDEN_GEOMETRY");
    std::env::set_var("VAM_TIMING", "1");
    install_fake_golden_snapshot(&drive9);

    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let cid_allocator = Arc::new(CidAllocator::default());
    let session_id = "session-golden-restore";
    let vm_id = "vm-golden-restore";
    let workspace_id = "workspace-golden-restore";

    let history = Arc::new(std::sync::Mutex::new(Vec::new()));
    let vmm_factory: VmmFactory = {
        let hist = history.clone();
        Arc::new(move |_, _| Box::new(MockVmm::with_shared_history(hist.clone())))
    };

    let (manager, cmd_rx) =
        SessionManager::new(store.clone(), cid_allocator.clone(), vmm_factory.clone());
    let (workspace_manager, catalog_cache) = drive9_runtime().expect("drive9");
    let net_pool = Arc::new(NetResourcePool::for_tests());
    let manager_handle = tokio::spawn(run_manager_loop(
        store.clone(),
        SecretsVault::for_tests(store.clone()),
        cid_allocator.clone(),
        vmm_factory,
        workspace_manager,
        catalog_cache,
        net_pool,
        cmd_rx,
        vam_telemetry::TelemetryCapture::default(),
    ));

    let vsock_uds_path = format!("/tmp/vam-vsock-{}.socket", vm_id);
    let _ = std::fs::remove_file(format!("{}_52", vsock_uds_path));
    let _ = std::fs::remove_file(format!("{}_53", vsock_uds_path));
    let _ = std::fs::remove_file(format!("{}_54", vsock_uds_path));
    let _ = std::fs::remove_file(format!("{}_55", vsock_uds_path));
    tokio::spawn(mock_vsock_guest_for_restore(vsock_uds_path.clone()));

    let timer = vam_orchestrator::timing::PhaseTimer::shared(
        "run-golden".into(),
        session_id.into(),
        workspace_id.into(),
    );

    manager
        .create_session(
            session_id.to_string(),
            None,
            vm_id.to_string(),
            workspace_id.to_string(),
            "default".to_string(),
            Some(timer.clone()),
        )
        .await
        .unwrap();

    let record = store.load_session(session_id).unwrap();
    assert_eq!(record.observed_state, ObservedState::Running);

    let hist = history.lock().unwrap().clone();
    assert!(hist.iter().any(|h| h.starts_with("load_snapshot:")));
    assert!(hist.iter().any(|h| h.starts_with("patch_data_drive:")));
    assert!(hist
        .iter()
        .any(|h| h.starts_with("load_snapshot_vsock_override:")));
    assert!(!hist.iter().any(|h| h == "configure_hardware"));

    // The session image restored onto the baked drive is content-sized, not 1 GiB.
    let patched = hist
        .iter()
        .find_map(|h| h.strip_prefix("patch_data_drive:data0:"))
        .expect("patch_data_drive entry");
    let image_len = std::fs::metadata(patched).expect("session image").len();
    assert!(
        image_len > 0 && image_len < vam_orchestrator::golden::GOLDEN_DISK_BYTES,
        "expected a content-sized session image, got {} bytes",
        image_len
    );

    let step_names = timer.lock().unwrap().step_labels();
    assert!(step_names.contains(&"snapshot_reseed"));
    assert!(!step_names.contains(&"registration_handshake_wait"));

    drop(manager);
    manager_handle.await.unwrap();
    let _ = std::fs::remove_dir_all(&drive9);
}

#[tokio::test]
async fn test_geometry_mismatch_falls_back_to_cold_boot() {
    let _env = HERMETIC_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let drive9 = std::env::temp_dir().join(format!("vam-golden-mismatch-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&drive9);
    std::env::set_var("VAM_DRIVE9_ROOT", &drive9);
    std::env::set_var("VAM_SNAPSHOT_RESTORE", "1");
    install_fake_golden_snapshot(&drive9);

    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let cid_allocator = Arc::new(CidAllocator::default());
    let session_id = "session-geom-mismatch";
    let vm_id = "vm-geom-mismatch";
    let workspace_id = "workspace-geom-mismatch";

    let history = Arc::new(std::sync::Mutex::new(Vec::new()));
    let vmm_factory: VmmFactory = {
        let hist = history.clone();
        Arc::new(move |_, _| Box::new(MockVmm::with_shared_history(hist.clone())))
    };

    let (manager, cmd_rx) =
        SessionManager::new(store.clone(), cid_allocator.clone(), vmm_factory.clone());
    let (workspace_manager, catalog_cache) = drive9_runtime().expect("drive9");
    let net_pool = Arc::new(NetResourcePool::for_tests());
    let manager_handle = tokio::spawn(run_manager_loop(
        store.clone(),
        SecretsVault::for_tests(store.clone()),
        cid_allocator,
        vmm_factory,
        workspace_manager,
        catalog_cache,
        net_pool,
        cmd_rx,
        vam_telemetry::TelemetryCapture::default(),
    ));

    let vsock_uds_path = format!("/tmp/vam-vsock-{}.socket", vm_id);
    let vsock_uds_path_with_port = format!("{}_52", vsock_uds_path);
    let _ = std::fs::remove_file(&vsock_uds_path_with_port);
    let vm_id_clone = vm_id.to_string();
    tokio::spawn(async move {
        let mut stream = None;
        for _ in 0..100 {
            if let Ok(s) = UnixStream::connect(&vsock_uds_path_with_port).await {
                stream = Some(s);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let stream = stream.expect("handshake connect");
        vam_vsock::run_guest_registration(stream, &vm_id_clone, 3)
            .await
            .expect("registration");
    });

    manager
        .create_session(
            session_id.to_string(),
            None,
            vm_id.to_string(),
            workspace_id.to_string(),
            "small".to_string(),
            None,
        )
        .await
        .unwrap();

    let hist = history.lock().unwrap().clone();
    assert!(hist.iter().any(|h| h == "configure_hardware"));
    assert!(!hist.iter().any(|h| h.starts_with("load_snapshot:")));

    drop(manager);
    manager_handle.await.unwrap();
    let _ = std::fs::remove_dir_all(&drive9);
}

/// Guest stand-in for the Firecracker vsock mux. Accepts `CONNECT 52`, records
/// the resume `ExecuteTask`, and answers `Shutdown` so park and finalize do
/// not wait out the dial timeout.
async fn serve_resume_guest(
    vm_id: String,
    seen: Arc<std::sync::Mutex<Option<vam_spec::v1::ExecuteTaskRequest>>>,
) {
    use prost::Message;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::UnixListener;
    use vam_spec::v1::{
        ExecuteTaskRequest, ExecuteTaskResponse, ShutdownResponse, TaskOutcome, TaskOutcomeState,
        VsockEnvelope,
    };
    use vam_vsock::frame::{Frame, FrameType};

    let path = format!("/tmp/vam-vsock-{vm_id}.socket");
    let _ = std::fs::remove_file(&path);
    let listener = match UnixListener::bind(&path) {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("resume mux bind {path} failed: {error}");
            return;
        }
    };
    loop {
        let Ok((mut stream, _)) = listener.accept().await else {
            break;
        };
        let mut line = Vec::new();
        let mut byte = [0u8; 1];
        let mut connect_ok = false;
        while stream.read_exact(&mut byte).await.is_ok() {
            line.push(byte[0]);
            if byte[0] == b'\n' {
                connect_ok = true;
                break;
            }
            if line.len() > 64 {
                break;
            }
        }
        if !connect_ok || !String::from_utf8_lossy(&line).starts_with("CONNECT ") {
            continue;
        }
        if stream.write_all(b"OK 52\n").await.is_err() {
            continue;
        }
        let _ = stream.flush().await;
        let frame = match Frame::read_from(&mut stream).await {
            Ok(frame) => frame,
            Err(_) => continue,
        };
        if frame.frame_type != FrameType::ControlGrpc {
            continue;
        }
        let Ok(env) = VsockEnvelope::decode(&*frame.payload) else {
            continue;
        };
        let (method, payload) = if env.method == "ExecuteTask" {
            let Ok(req) = ExecuteTaskRequest::decode(&*env.payload) else {
                continue;
            };
            if let Ok(mut slot) = seen.lock() {
                *slot = Some(req.clone());
            }
            RESUME_TASKS_RUN.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let outcome = TaskOutcome {
                lease: req.lease.clone(),
                state: TaskOutcomeState::Succeeded as i32,
                output: Some(prost_types::Struct {
                    fields: [(
                        "answer".to_string(),
                        prost_types::Value {
                            kind: Some(prost_types::value::Kind::StringValue(
                                "resumed".to_string(),
                            )),
                        },
                    )]
                    .into_iter()
                    .collect(),
                }),
                output_artifacts: Vec::new(),
                continuation: None,
                evidence: None,
                observed_usage: None,
                error: None,
            };
            let response = ExecuteTaskResponse {
                outcome: Some(outcome),
                resource_usage: None,
            };
            let mut encoded = Vec::new();
            if response.encode(&mut encoded).is_err() {
                continue;
            }
            ("ExecuteTaskResponse".to_string(), encoded)
        } else if env.method == "Shutdown" {
            let response = ShutdownResponse { accepted: true };
            let mut encoded = Vec::new();
            if response.encode(&mut encoded).is_err() {
                continue;
            }
            ("ShutdownResponse".to_string(), encoded)
        } else {
            continue;
        };
        let env_out = VsockEnvelope {
            method,
            correlation_id: env.correlation_id,
            payload,
            error: None,
            trace_context: None,
        };
        let mut env_payload = Vec::new();
        if env_out.encode(&mut env_payload).is_err() {
            continue;
        }
        let frame_out = Frame {
            frame_type: FrameType::ControlGrpc,
            correlation_id: env.correlation_id,
            payload: bytes::Bytes::from(env_payload),
        };
        let _ = frame_out.write_to(&mut stream).await;
    }
}

async fn register_booting_guest(vm_id: String) {
    let path = format!("/tmp/vam-vsock-{vm_id}.socket_52");
    let mut stream = None;
    for _ in 0..200 {
        if let Ok(connected) = UnixStream::connect(&path).await {
            stream = Some(connected);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    let Some(stream) = stream else {
        return;
    };
    let _ = vam_vsock::run_guest_registration(stream, &vm_id, 3).await;
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}

struct PausedHitl {
    store: Arc<SqliteWorkflowStore>,
    session_id: String,
    vm_id: String,
    workspace_id: String,
    run_id: String,
    approval_id: String,
    marker: String,
    args_digest: String,
}

fn seed_paused_hitl(label: &str) -> PausedHitl {
    seed_paused_with(
        label,
        Arc::new(SqliteWorkflowStore::in_memory().unwrap()),
        false,
    )
}

/// A run parked on an `ask_user` question (AR-3.10) in `store`.
fn seed_paused_question(label: &str, store: Arc<SqliteWorkflowStore>) -> PausedHitl {
    seed_paused_with(label, store, true)
}

fn seed_paused_with(label: &str, store: Arc<SqliteWorkflowStore>, question: bool) -> PausedHitl {
    use vam_spec::v1::{
        ArtifactRef, Digest, ExecutionLease, ResourceId, TaskOutcome, TaskOutcomeState,
    };

    let session_id = format!("sess-{label}");
    let vm_id = format!("vm-{label}");
    let workspace_id = format!("ws-{label}");
    let run_id = format!("urn:vam:run:local.vam.dev/acme/run-{label}");
    let approval_id = format!("urn:vam:hitl:local.vam.dev/acme/approval-{label}");
    let marker = format!("checkpoint-{label}");
    let args_digest = format!("digest-{label}");
    // A resumed session is tied to the run before it boots, so the run's agent
    // is loaded like a fresh run's: it must be in the catalog.
    store
        .put_agent(&vam_spec::v1::AgentSpec {
            resource_id: Some(ResourceId {
                urn: "urn:vam:agent:local.vam.dev/acme/test".to_string(),
            }),
            display_name: "Test agent".to_string(),
            role: "Tester".to_string(),
            instruction_source: Some(
                vam_spec::v1::agent_spec::InstructionSource::SystemInstructions(
                    "Finish when asked.".to_string(),
                ),
            ),
            allow_any_playbook: true,
            ..Default::default()
        })
        .unwrap();

    store
        .create_session(&session_id, None, &vm_id, &workspace_id, "default")
        .unwrap();
    store
        .update_observed_state(&session_id, ObservedState::Booting)
        .unwrap();
    store.record_vmm(&session_id, 12345, 22222, 4).unwrap();
    store
        .update_observed_state(&session_id, ObservedState::Running)
        .unwrap();

    let checkpoint_bytes = format!(
        r#"{{"schema_version":1,"run_id":"{run_id}","lifecycle_generation":1,"marker":"{marker}"}}"#
    )
    .into_bytes();
    let checkpoint_digest = format!(
        "{:x}",
        <sha2::Sha256 as sha2::Digest>::digest(&checkpoint_bytes)
    );
    store
        .persist_run_spec(
            &run_id,
            r#"{"inputs":{"prompt":"finish after approval"},"maxDuration":"30s"}"#,
            Some(&session_id),
            "urn:vam:agent:local.vam.dev/acme/test",
            None,
            "User::\"local\"",
        )
        .unwrap();
    store
        .put_run_checkpoint(&run_id, 1, &checkpoint_digest, &checkpoint_bytes)
        .unwrap();

    let lease = ExecutionLease {
        run_id: Some(ResourceId {
            urn: run_id.clone(),
        }),
        step_id: "step-1".to_string(),
        attempt_id: Some(ResourceId {
            urn: format!("urn:vam:attempt:local.vam.dev/acme/attempt-{label}"),
        }),
        lease_id: Some(ResourceId {
            urn: format!("urn:vam:lease:local.vam.dev/acme/lease-{label}"),
        }),
        lifecycle_generation: 1,
        fencing_token: vec![1],
        expires_at: Some(prost_types::Timestamp {
            seconds: 4_102_444_800,
            nanos: 0,
        }),
        idempotency_key: format!("idempotency-{label}"),
    };
    let key = AttemptKey {
        run_id: run_id.clone(),
        step_id: "step-1".to_string(),
        attempt_id: lease.attempt_id.as_ref().unwrap().urn.clone(),
    };
    let approval = ResourceId {
        urn: approval_id.clone(),
    };
    let artifact = ArtifactRef {
        artifact_id: Some(ResourceId {
            urn: format!("urn:vam:artifact:local.vam.dev/acme/checkpoint-{label}"),
        }),
        digest: Some(Digest {
            algorithm: 1,
            hex_hash: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
                .to_string(),
        }),
        media_type: "application/octet-stream".to_string(),
        size_bytes: checkpoint_bytes.len() as i64,
        uri: "memory://checkpoint".to_string(),
    };
    store.issue_lease(&lease).unwrap();
    if question {
        let data = serde_json::json!({"question": "Which name should I use?", "choices": ["Ada", "Grace"]});
        let custom = vam_orchestrator::hitl::question_approval("vam.question", &data).unwrap();
        store
            .register_hitl_suspend_custom(
                &key,
                &session_id,
                &approval,
                "ask_user",
                &serde_json::json!({"jig": "vam.question", "data": data}).to_string(),
                &args_digest,
                Some(4_102_444_800),
                Some(&custom),
            )
            .unwrap();
    } else {
        store
            .register_hitl_suspend(
                &key,
                &session_id,
                &approval,
                "jira_create_issue",
                r#"{"title":"issue"}"#,
                &args_digest,
                Some(4_102_444_800),
            )
            .unwrap();
    }
    store
        .record_outcome(&TaskOutcome {
            lease: Some(lease),
            state: TaskOutcomeState::Suspended as i32,
            output: None,
            output_artifacts: Vec::new(),
            continuation: Some(artifact.clone()),
            evidence: None,
            observed_usage: None,
            error: None,
        })
        .unwrap();
    store
        .create_approval(&key, &approval, &artifact, 1)
        .unwrap();
    store.update_run_state(&run_id, "PAUSED").unwrap();

    PausedHitl {
        store,
        session_id,
        vm_id,
        workspace_id,
        run_id,
        approval_id,
        marker,
        args_digest,
    }
}

fn decide_paused(wait: &PausedHitl) -> vam_orchestrator::ApprovalRecord {
    use vam_spec::v1::ResourceId;
    wait.store
        .decide_approval(
            &ResourceId {
                urn: wait.approval_id.clone(),
            },
            &ResourceId {
                urn: format!("urn:vam:token:local.vam.dev/acme/token-{}", wait.session_id),
            },
            "APPROVED",
            "User::\"local\"",
            vam_orchestrator::LOCAL_USER_ID,
            None,
            None,
            None,
            None,
        )
        .expect("decision")
}

fn spawn_manager(
    store: Arc<SqliteWorkflowStore>,
    vmm_factory: VmmFactory,
) -> (SessionManager, tokio::task::JoinHandle<()>) {
    let cid_allocator = Arc::new(CidAllocator::default());
    let (manager, cmd_rx) =
        SessionManager::new(store.clone(), cid_allocator.clone(), vmm_factory.clone());
    let (workspace_manager, catalog_cache) = drive9_runtime().expect("drive9");
    let net_pool = Arc::new(NetResourcePool::for_tests());
    let manager_handle = tokio::spawn(run_manager_loop(
        store.clone(),
        SecretsVault::for_tests(store),
        cid_allocator,
        vmm_factory,
        workspace_manager,
        catalog_cache,
        net_pool,
        cmd_rx,
        vam_telemetry::TelemetryCapture::default(),
    ));
    (manager, manager_handle)
}

fn struct_string(value: &prost_types::Struct, key: &str) -> Option<String> {
    match value.fields.get(key).and_then(|field| field.kind.as_ref()) {
        Some(prost_types::value::Kind::StringValue(text)) => Some(text.clone()),
        _ => None,
    }
}

fn struct_bool(value: &prost_types::Struct, key: &str) -> Option<bool> {
    match value.fields.get(key).and_then(|field| field.kind.as_ref()) {
        Some(prost_types::value::Kind::BoolValue(flag)) => Some(*flag),
        _ => None,
    }
}

async fn park_after_keep_warm(wait: &PausedHitl, manager: &SessionManager) {
    let drive9 = std::env::temp_dir().join(format!(
        "vam-resume-park-{}-{}",
        wait.workspace_id,
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&drive9);
    std::fs::create_dir_all(drive9.join("workspaces").join(&wait.workspace_id)).unwrap();
    std::fs::write(
        drive9
            .join("workspaces")
            .join(&wait.workspace_id)
            .join("write.lock"),
        &wait.session_id,
    )
    .unwrap();
    let layout = vam_storage::Drive9Layout::new(drive9.clone());
    // Simulated clock: the keep-warm window closed 24h before this decision.
    wait.store
        .schedule_keep_warm(&wait.run_id, unix_now() - 86_400)
        .unwrap();
    let parked = sweep_keep_warm(wait.store.as_ref(), manager, &layout)
        .await
        .unwrap();
    assert_eq!(parked, 1);
    assert_eq!(
        wait.store
            .load_session(&wait.session_id)
            .unwrap()
            .observed_state,
        ObservedState::Terminated
    );
    let _ = std::fs::remove_dir_all(&drive9);
}

/// Watch for the cold-boot session, complete its registration, and serve the
/// resume task on its vsock mux.
fn watch_cold_boot(
    store: Arc<SqliteWorkflowStore>,
    original_session: String,
    seen: Arc<std::sync::Mutex<Option<vam_spec::v1::ExecuteTaskRequest>>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut registered = std::collections::HashSet::new();
        let mut served = std::collections::HashSet::new();
        for _ in 0..500 {
            let sessions = store.load_all_sessions().unwrap_or_default();
            for session in sessions {
                if session.session_id == original_session {
                    continue;
                }
                if !registered.contains(&session.vm_id) {
                    let sock = format!("/tmp/vam-vsock-{}.socket_52", session.vm_id);
                    if std::path::Path::new(&sock).exists() {
                        registered.insert(session.vm_id.clone());
                        tokio::spawn(register_booting_guest(session.vm_id.clone()));
                    }
                }
                if !served.contains(&session.vm_id) {
                    served.insert(session.vm_id.clone());
                    tokio::spawn(serve_resume_guest(session.vm_id.clone(), seen.clone()));
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
}

#[tokio::test]
async fn test_warm_hitl_decision_resumes_in_place() {
    let _env = HERMETIC_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let wait = seed_paused_hitl("warm-resume");
    let seen = Arc::new(std::sync::Mutex::new(None));
    let guest = tokio::spawn(serve_resume_guest(wait.vm_id.clone(), seen.clone()));
    let vmm_factory: VmmFactory = Arc::new(|_, _| Box::new(MockVmm::new()));
    let (manager, manager_handle) = spawn_manager(wait.store.clone(), vmm_factory);
    let record = decide_paused(&wait);

    vam_orchestrator::api::resume_after_hitl_decision(manager.clone(), wait.store.clone(), record)
        .await
        .expect("warm resume");

    assert_eq!(wait.store.get_run_state(&wait.run_id).unwrap(), "SUCCEEDED");
    let sessions = wait.store.load_all_sessions().unwrap();
    assert_eq!(sessions.len(), 1, "warm resume must not boot a second VM");
    assert_eq!(sessions[0].session_id, wait.session_id);
    assert!(sessions[0].lifecycle_generation > 1);

    let request = seen.lock().unwrap().clone().expect("resume task");
    let inputs = request.inputs.expect("resume inputs");
    assert_eq!(struct_bool(&inputs, "hitl_resume"), Some(true));
    assert_eq!(
        struct_string(&inputs, "approved_tool").as_deref(),
        Some("jira_create_issue")
    );
    assert!(struct_string(&inputs, "hitl_continuation").is_none());
    let stored = wait
        .store
        .latest_run_checkpoint(&wait.run_id)
        .unwrap()
        .expect("checkpoint");
    assert!(stored
        .bytes
        .windows(wait.marker.len())
        .any(|window| window == wait.marker.as_bytes()));
    assert_eq!(
        struct_string(&inputs, "hitl_continuation_digest").as_deref(),
        Some(stored.digest.as_str())
    );
    assert_eq!(
        struct_string(&inputs, "hitl_continuation_generation").as_deref(),
        Some("1")
    );
    assert_eq!(
        request
            .lease
            .as_ref()
            .map(|lease| lease.lifecycle_generation),
        Some(sessions[0].lifecycle_generation)
    );

    guest.abort();
    drop(manager);
    manager_handle.await.unwrap();
    let _ = std::fs::remove_file(format!("/tmp/vam-vsock-{}.socket", wait.vm_id));
}

/// A second wait of the parked run: a question, pending beside the tool call (its own attempt).
fn add_pending_question(wait: &PausedHitl) -> String {
    use vam_spec::v1::{
        ArtifactRef, Digest, ExecutionLease, ResourceId, TaskOutcome, TaskOutcomeState,
    };
    let label = wait.session_id.trim_start_matches("sess-");
    let approval_id = format!("urn:vam:hitl:local.vam.dev/acme/question-{label}");
    let lease = ExecutionLease {
        run_id: Some(ResourceId {
            urn: wait.run_id.clone(),
        }),
        step_id: "step-q".to_string(),
        attempt_id: Some(ResourceId {
            urn: format!("urn:vam:attempt:local.vam.dev/acme/attempt-q-{label}"),
        }),
        lease_id: Some(ResourceId {
            urn: format!("urn:vam:lease:local.vam.dev/acme/lease-q-{label}"),
        }),
        lifecycle_generation: 1,
        fencing_token: vec![1],
        expires_at: Some(prost_types::Timestamp {
            seconds: 4_102_444_800,
            nanos: 0,
        }),
        idempotency_key: format!("idempotency-q-{label}"),
    };
    let key = AttemptKey {
        run_id: wait.run_id.clone(),
        step_id: "step-q".to_string(),
        attempt_id: lease.attempt_id.as_ref().unwrap().urn.clone(),
    };
    let artifact = ArtifactRef {
        artifact_id: Some(ResourceId {
            urn: format!("urn:vam:artifact:local.vam.dev/acme/checkpoint-q-{label}"),
        }),
        digest: Some(Digest {
            algorithm: 1,
            hex_hash: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
                .to_string(),
        }),
        media_type: "application/octet-stream".to_string(),
        size_bytes: 1,
        uri: "memory://checkpoint".to_string(),
    };
    wait.store.issue_lease(&lease).unwrap();
    let data =
        serde_json::json!({"question": "Which name should I use?", "choices": ["Ada", "Grace"]});
    let custom = vam_orchestrator::hitl::question_approval("vam.question", &data).unwrap();
    wait.store
        .register_hitl_suspend_custom(
            &key,
            &wait.session_id,
            &ResourceId {
                urn: approval_id.clone(),
            },
            "ask_user",
            &serde_json::json!({"jig": "vam.question", "data": data}).to_string(),
            "digest-question",
            Some(4_102_444_800),
            Some(&custom),
        )
        .unwrap();
    wait.store
        .record_outcome(&TaskOutcome {
            lease: Some(lease),
            state: TaskOutcomeState::Suspended as i32,
            output: None,
            output_artifacts: Vec::new(),
            continuation: Some(artifact.clone()),
            evidence: None,
            observed_usage: None,
            error: None,
        })
        .unwrap();
    wait.store
        .create_approval(
            &key,
            &ResourceId {
                urn: approval_id.clone(),
            },
            &artifact,
            1,
        )
        .unwrap();
    approval_id
}

/// Two decisions on the one parked, kept-alive run (a tool call approved, a question answered)
/// race to resume it. The session runs one task: the resume the first claim owns, which is told
/// both decisions. The lease it runs under is above the parked task's, and the host accepts the
/// task's outcome under it.
#[tokio::test]
async fn test_two_decisions_on_a_warm_session_resume_it_once() {
    let _env = HERMETIC_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let wait = seed_paused_hitl("warm-twice");
    let question_id = add_pending_question(&wait);
    let seen = Arc::new(std::sync::Mutex::new(None));
    RESUME_TASKS_RUN.store(0, std::sync::atomic::Ordering::SeqCst);
    let guest = tokio::spawn(serve_resume_guest(wait.vm_id.clone(), seen.clone()));
    let vmm_factory: VmmFactory = Arc::new(|_, _| Box::new(MockVmm::new()));
    let (manager, manager_handle) = spawn_manager(wait.store.clone(), vmm_factory);

    let approved = decide_paused(&wait);
    let answered = wait
        .store
        .decide_approval(
            &vam_spec::v1::ResourceId {
                urn: question_id.clone(),
            },
            &vam_spec::v1::ResourceId {
                urn: format!(
                    "urn:vam:token:local.vam.dev/acme/answer-{}",
                    wait.session_id
                ),
            },
            "SUBMITTED",
            "User::\"local\"",
            LOCAL_USER_ID,
            None,
            None,
            None,
            Some(r#"{"answer":"Ada"}"#),
        )
        .expect("answer");
    // Each decision resumes the run, at the same moment.
    let (first, second) = tokio::join!(
        vam_orchestrator::api::resume_after_hitl_decision(
            manager.clone(),
            wait.store.clone(),
            approved,
        ),
        vam_orchestrator::api::resume_after_hitl_decision(
            manager.clone(),
            wait.store.clone(),
            answered,
        ),
    );
    first.expect("first resume");
    second.expect("second resume is a no-op, not a failure");

    assert_eq!(
        RESUME_TASKS_RUN.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the kept-alive session runs one task for two decisions"
    );
    assert_eq!(wait.store.get_run_state(&wait.run_id).unwrap(), "SUCCEEDED");
    let sessions = wait.store.load_all_sessions().unwrap();
    assert_eq!(sessions.len(), 1, "no second VM");
    // The parked task ran under generation 1; the resume runs under 2, and the host accepted its
    // outcome under that lease (the run succeeded).
    assert_eq!(sessions[0].lifecycle_generation, 2);
    let request = seen.lock().unwrap().clone().expect("resume task");
    assert_eq!(
        request.lease.as_ref().map(|l| l.lifecycle_generation),
        Some(2)
    );
    // The one task is told both decisions: whichever owns the resume, the other is in its inputs.
    let inputs = request.inputs.expect("resume inputs");
    let text = format!("{inputs:?}");
    assert!(
        text.contains("jira_create_issue"),
        "the tool call's decision reaches the task: {text}"
    );
    assert!(
        text.contains("Ada"),
        "the question's answer reaches the task: {text}"
    );
    // Both decisions count as told, and the event says what the resume carried.
    let events = wait.store.read_outbox_for_run(&wait.run_id, 0).unwrap();
    let carried: Vec<_> = events
        .iter()
        .filter(|e| e.event_type == vam_orchestrator::events::TYPE_RESUME_DECISIONS)
        .collect();
    assert_eq!(carried.len(), 1, "one event for the one resume");
    assert!(carried[0].payload.contains(&question_id));
    assert!(carried[0].payload.contains(&wait.approval_id));

    guest.abort();
    drop(manager);
    manager_handle.await.unwrap();
    let _ = std::fs::remove_file(format!("/tmp/vam-vsock-{}.socket", wait.vm_id));
}

#[tokio::test]
async fn test_decision_after_one_day_resumes_on_a_new_vm() {
    let _env = HERMETIC_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let wait = seed_paused_hitl("cold-24h");
    let shutdown = Arc::new(std::sync::Mutex::new(None));
    let park_guest = tokio::spawn(serve_resume_guest(wait.vm_id.clone(), shutdown));
    let vmm_factory: VmmFactory = Arc::new(|_, _| Box::new(MockVmm::new()));
    let (manager, manager_handle) = spawn_manager(wait.store.clone(), vmm_factory);
    park_after_keep_warm(&wait, &manager).await;
    park_guest.abort();

    let seen = Arc::new(std::sync::Mutex::new(None));
    let watcher = watch_cold_boot(wait.store.clone(), wait.session_id.clone(), seen.clone());
    let record = decide_paused(&wait);
    assert!(
        record.continuation_generation > 1,
        "the decision bumps the continuation generation"
    );

    vam_orchestrator::api::resume_after_hitl_decision(manager.clone(), wait.store.clone(), record)
        .await
        .expect("cold resume");

    assert_eq!(wait.store.get_run_state(&wait.run_id).unwrap(), "SUCCEEDED");
    assert!(
        wait.store
            .workspace_reservation(&wait.workspace_id)
            .unwrap()
            .is_none(),
        "a completed resume releases the park reservation"
    );
    let sessions = wait.store.load_all_sessions().unwrap();
    let resumed = sessions
        .iter()
        .find(|session| session.session_id != wait.session_id)
        .expect("new session");
    assert!(resumed.lifecycle_generation > 1);
    // The resumed session belongs to the run and resolves the run's agent and
    // owner (its brokers read exactly these at boot).
    assert_eq!(
        wait.store
            .find_run_id_for_session(&resumed.session_id)
            .unwrap()
            .as_deref(),
        Some(wait.run_id.as_str())
    );
    assert_eq!(
        wait.store
            .find_agent_urn_for_session(&resumed.session_id)
            .unwrap()
            .as_deref(),
        Some("urn:vam:agent:local.vam.dev/acme/test")
    );
    assert_eq!(
        wait.store
            .session_owner_user_id(&resumed.session_id)
            .unwrap(),
        wait.store.run_owner_user_id(&wait.run_id).unwrap()
    );
    assert_eq!(
        wait.store
            .load_session(&wait.session_id)
            .unwrap()
            .observed_state,
        ObservedState::Terminated
    );
    assert!(
        wait.store
            .has_hitl_grant(&resumed.session_id, "jira_create_issue", &wait.args_digest),
        "the approval grant follows the new VM"
    );

    let request = seen
        .lock()
        .unwrap()
        .clone()
        .expect("resume task on the new VM");
    let inputs = request.inputs.expect("resume inputs");
    assert_eq!(struct_bool(&inputs, "hitl_resume"), Some(true));
    assert_eq!(
        struct_string(&inputs, "approved_tool").as_deref(),
        Some("jira_create_issue")
    );
    assert!(struct_string(&inputs, "hitl_continuation").is_none());
    let stored = wait
        .store
        .latest_run_checkpoint(&wait.run_id)
        .unwrap()
        .expect("host checkpoint");
    assert!(stored
        .bytes
        .windows(wait.marker.len())
        .any(|window| window == wait.marker.as_bytes()));
    assert!(stored
        .bytes
        .windows(wait.run_id.len())
        .any(|window| window == wait.run_id.as_bytes()));
    assert_eq!(
        struct_string(&inputs, "hitl_continuation_digest").as_deref(),
        Some(stored.digest.as_str())
    );
    assert_eq!(
        struct_string(&inputs, "hitl_continuation_generation").as_deref(),
        Some("1")
    );
    assert_eq!(
        request.lease.expect("lease").lifecycle_generation,
        resumed.lifecycle_generation
    );

    watcher.abort();
    drop(manager);
    manager_handle.await.unwrap();
}

/// A session that belongs to a run whose agent cannot be resolved is refused
/// before any VM exists. It never starts under the builtins-only fallback.
#[tokio::test]
async fn test_run_bound_session_without_an_agent_is_refused_before_any_vm() {
    let _env = HERMETIC_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    store
        .persist_run_spec(
            "urn:vam:run:local.vam.dev/acme/run-noagent",
            "{}",
            Some("sess-noagent"),
            "",
            None,
            "User::\"local\"",
        )
        .unwrap();
    let history = Arc::new(std::sync::Mutex::new(Vec::new()));
    let vmm_factory: VmmFactory = {
        let hist = history.clone();
        Arc::new(move |_, _| Box::new(MockVmm::with_shared_history(hist.clone())))
    };
    let (manager, manager_handle) = spawn_manager(store.clone(), vmm_factory);

    let err = manager
        .create_session(
            "sess-noagent".to_string(),
            None,
            "vm-noagent".to_string(),
            "ws-noagent".to_string(),
            "default".to_string(),
            None,
        )
        .await
        .expect_err("a run's session without an agent must be refused");
    assert!(err.to_string().contains("refusing to start"), "{err}");
    assert!(
        history.lock().unwrap().is_empty(),
        "no VM may start for a refused session"
    );
    assert_eq!(
        store.load_session("sess-noagent").unwrap().observed_state,
        ObservedState::Failed
    );

    drop(manager);
    manager_handle.await.unwrap();
}

/// Pause notification (`vam.v1.hitl.paused` plus `on_paused`) leaves the
/// approval listed. A required hook FAIL does not cancel the wait. Warm and
/// cold sessions both stay decidable for the existing resume path.
#[tokio::test]
async fn on_paused_notifies_and_parked_runs_stay_decidable() {
    let _env = HERMETIC_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    use std::time::Duration;
    use vam_orchestrator::hsp::{HspClient, HspEndpoint};
    use vam_orchestrator::hsp_echo::{
        spawn_scripted_hook_unix, ScriptedHookBehavior, ScriptedHookBroker,
    };
    use vam_orchestrator::hsp_hooks::notify_run_paused;
    use vam_orchestrator::hsp_registry::HspRegistry;
    use vam_spec::v1::{
        CapabilityManifest, HookParticipation, HookPhase, RegisterRequest, ToolExport,
    };

    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("on-paused.sock");
    let tool = Arc::new(std::sync::Mutex::new(None));
    let mut broker = ScriptedHookBroker::default().with_on_paused(ScriptedHookBehavior::Fail);
    broker.last_paused_tool = tool.clone();
    let (_path, _handle) = spawn_scripted_hook_unix(sock.clone(), broker).unwrap();

    let manifest = CapabilityManifest {
        broker_id: "pager".to_string(),
        trust_tier: 2,
        hooks: vec![HookParticipation {
            phase: HookPhase::OnPaused as i32,
            required: true,
        }],
        tools: vec![ToolExport {
            tool_name: "echo.ping".to_string(),
            side_effect: 0,
            required_secrets: vec![],
            destination_policy: Default::default(),
            effect: None,
        }],
        ..Default::default()
    };

    async fn admit(
        store: &SqliteWorkflowStore,
        sock: &std::path::Path,
        manifest: &CapabilityManifest,
    ) {
        store.upsert_hsp_broker_approval(manifest).unwrap();
        let mut client = HspClient::connect_with_timeout(
            HspEndpoint::Unix(sock.to_path_buf()),
            Duration::from_secs(2),
        )
        .await
        .unwrap();
        HspRegistry::new(store)
            .register_admitted(
                &mut client,
                RegisterRequest {
                    broker_id: manifest.broker_id.clone(),
                    broker_version: "pb-1.8".to_string(),
                    manifest: Some(manifest.clone()),
                },
            )
            .await
            .unwrap();
    }

    let wait = seed_paused_hitl("notify-warm");
    admit(wait.store.as_ref(), &sock, &manifest).await;
    let noted = notify_run_paused(wait.store.as_ref(), &wait.run_id, &wait.session_id)
        .await
        .unwrap();
    assert!(noted.aborted, "required on_paused FAIL is reported");
    let events = wait.store.read_outbox_for_run(&wait.run_id, 0).unwrap();
    assert!(
        events
            .iter()
            .any(|event| event.event_type == "vam.v1.hitl.paused"),
        "pause still emits vam.v1.hitl.paused"
    );
    assert!(
        events.iter().any(|event| {
            event.event_type == "vam.v1.hsp.hook.failed" && event.payload.contains("on_paused")
        }),
        "on_paused failure is audited, got {events:?}"
    );
    assert_eq!(tool.lock().unwrap().as_deref(), Some("jira_create_issue"));
    assert!(
        wait.store
            .list_published_approvals()
            .unwrap()
            .iter()
            .any(|row| row.run_id == wait.run_id && row.tool_name == "jira_create_issue"),
        "the parked run stays listed for vam approve"
    );
    let record = decide_paused(&wait);
    assert_eq!(record.tool_name, "jira_create_issue");
    assert_eq!(record.session_id, wait.session_id);
    assert_eq!(
        wait.store
            .load_session(&wait.session_id)
            .unwrap()
            .observed_state,
        ObservedState::Running,
        "a warm wait is still in place for resume"
    );

    let cold = seed_paused_hitl("notify-cold");
    admit(cold.store.as_ref(), &sock, &manifest).await;
    let shutdown = Arc::new(std::sync::Mutex::new(None));
    let park_guest = tokio::spawn(serve_resume_guest(cold.vm_id.clone(), shutdown));
    let vmm_factory: VmmFactory = Arc::new(|_, _| Box::new(MockVmm::new()));
    let (manager, manager_handle) = spawn_manager(cold.store.clone(), vmm_factory);
    park_after_keep_warm(&cold, &manager).await;
    park_guest.abort();
    let noted = notify_run_paused(cold.store.as_ref(), &cold.run_id, &cold.session_id)
        .await
        .unwrap();
    assert!(noted.aborted);
    assert_eq!(
        cold.store
            .load_session(&cold.session_id)
            .unwrap()
            .observed_state,
        ObservedState::Terminated,
        "keep-warm park shuts the VM down"
    );
    assert!(
        cold.store
            .list_published_approvals()
            .unwrap()
            .iter()
            .any(|row| row.run_id == cold.run_id),
        "a cold parked run is still listed"
    );
    let record = decide_paused(&cold);
    assert_eq!(record.run_id, cold.run_id);
    assert_eq!(record.session_id, cold.session_id);
    assert!(
        record.continuation_generation > 1,
        "deciding a cold park still bumps the continuation generation"
    );

    drop(manager);
    manager_handle.await.unwrap();
}

#[tokio::test]
async fn test_cold_resume_boot_failure_fails_the_run() {
    let _env = HERMETIC_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let wait = seed_paused_hitl("boot-fail");
    let shutdown = Arc::new(std::sync::Mutex::new(None));
    let park_guest = tokio::spawn(serve_resume_guest(wait.vm_id.clone(), shutdown));
    let vmm_factory: VmmFactory = Arc::new(|_, _| {
        let mut vmm = MockVmm::new();
        vmm.configure_error = Some("simulated boot failure".to_string());
        Box::new(vmm)
    });
    let (manager, manager_handle) = spawn_manager(wait.store.clone(), vmm_factory);
    park_after_keep_warm(&wait, &manager).await;
    park_guest.abort();

    let record = decide_paused(&wait);
    let error = vam_orchestrator::api::resume_after_hitl_decision(
        manager.clone(),
        wait.store.clone(),
        record,
    )
    .await
    .expect_err("boot must fail");
    let message = error.to_string();
    assert!(
        message.contains("simulated boot failure"),
        "unexpected error: {message}"
    );
    assert_eq!(wait.store.get_run_state(&wait.run_id).unwrap(), "FAILED");
    assert!(
        wait.store
            .workspace_reservation(&wait.workspace_id)
            .unwrap()
            .is_none(),
        "boot failure must not leave the workspace reserved"
    );
    assert!(wait
        .store
        .keep_warm_deadline(&wait.run_id)
        .unwrap()
        .is_none());
    let hung = wait
        .store
        .load_all_sessions()
        .unwrap()
        .into_iter()
        .any(|session| {
            session.workspace_id == wait.workspace_id
                && matches!(
                    session.observed_state,
                    ObservedState::Running | ObservedState::Booting | ObservedState::Paused
                )
        });
    assert!(!hung, "boot failure must not leave a live parked VM");

    drop(manager);
    manager_handle.await.unwrap();
}

/// Pins `pinned.example` at the local echo server and `denied.example` at a
/// public address so the deny is the allowlist, not the protected IP set.
struct Port55Resolver {
    echo: std::net::SocketAddr,
}

impl vam_network::HostResolver for Port55Resolver {
    fn lookup(&self, host: &str, port: u16) -> std::io::Result<Vec<std::net::SocketAddr>> {
        let ip = match host {
            "pinned.example" => self.echo.ip(),
            "denied.example" => std::net::IpAddr::V4(std::net::Ipv4Addr::new(8, 8, 8, 8)),
            _ => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("unresolved {host}"),
                ));
            }
        };
        Ok(vec![std::net::SocketAddr::new(ip, port)])
    }
}

struct ResetEgressOverride;

impl Drop for ResetEgressOverride {
    fn drop(&mut self) {
        vam_orchestrator::brokers::set_egress_test_override(None);
    }
}

fn assert_data_dir_has_no_private_key(root: &std::path::Path) {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let bytes = std::fs::read(&path).unwrap_or_default();
            let text = String::from_utf8_lossy(&bytes);
            assert!(
                !text.contains("PRIVATE KEY"),
                "private key material written to {}",
                path.display()
            );
        }
    }
}

async fn read_proxy_head(stream: &mut UnixStream) -> String {
    use tokio::io::AsyncReadExt;
    let mut buf = Vec::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while !buf.windows(4).any(|window| window == b"\r\n\r\n") {
        if tokio::time::Instant::now() >= deadline {
            panic!(
                "timed out reading proxy response: {}",
                String::from_utf8_lossy(&buf)
            );
        }
        let mut tmp = [0u8; 1024];
        let n = tokio::time::timeout_at(deadline, stream.read(&mut tmp))
            .await
            .expect("proxy response timeout")
            .expect("proxy response read");
        assert!(n > 0, "proxy closed before a response");
        buf.extend_from_slice(&tmp[..n]);
    }
    String::from_utf8_lossy(&buf).into_owned()
}

fn decode_egress_decided(payload: &str) -> vam_spec::v1::EgressDecidedPayload {
    use prost::Message;
    let event: vam_spec::v1::VamCloudEvent =
        vam_spec::encoding::json_to_proto("vam.v1.VamCloudEvent", payload)
            .expect("egress event json");
    let data = event.data.expect("egress event data");
    vam_spec::v1::EgressDecidedPayload::decode(data.value.as_slice()).expect("egress payload")
}

fn pem_fingerprint(pem: &str) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(pem.as_bytes()))
}

/// E8-1.5: port 55 exists for a mock-guest session, passthrough tunnels bytes,
/// a denied host is 403 with an `egress_decided` DENY on the real port, and
/// two sessions do not share a CA or write the private key.
#[tokio::test]
async fn port_55_passthrough_deny_and_distinct_cas() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use vam_orchestrator::brokers::{set_egress_test_override, EgressTestOverride};
    use vam_orchestrator::secrets::SecretKind;

    let _env = HERMETIC_ENV_LOCK
        .lock()
        .unwrap_or_else(|err| err.into_inner());
    let state = isolate_state();
    let _reset = ResetEgressOverride;

    let echo = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("echo listener");
    let echo_addr = echo.local_addr().expect("echo addr");
    assert_ne!(
        echo_addr.port(),
        443,
        "ephemeral port must stay passthrough"
    );
    tokio::spawn(async move {
        loop {
            let Ok((sock, _)) = echo.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let (mut reader, mut writer) = tokio::io::split(sock);
                let _ = tokio::io::copy(&mut reader, &mut writer).await;
            });
        }
    });

    set_egress_test_override(Some(EgressTestOverride {
        resolver: Arc::new(Port55Resolver { echo: echo_addr }),
        exempt_ip_deny: true,
        allowed_ports: vec![vam_spec::v1::AllowedPortRange {
            from_port: u32::from(echo_addr.port()),
            to_port: u32::from(echo_addr.port()),
            protocol: "tcp".to_string(),
        }],
    }));

    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let vault = SecretsVault::for_tests(store.clone());
    vault
        .put_secret(
            "pinned/token",
            "not-a-real-secret",
            SecretKind::Egress,
            &["pinned.example".to_string()],
            None,
        )
        .unwrap();

    let cid_allocator = Arc::new(CidAllocator::default());
    let vmm_factory: VmmFactory = Arc::new(|_vm_id, _guest_cid| Box::new(MockVmm::new()));
    let (manager, cmd_rx) =
        SessionManager::new(store.clone(), cid_allocator.clone(), vmm_factory.clone());
    let (workspace_manager, catalog_cache) = drive9_runtime().expect("drive9");
    let net_pool = Arc::new(NetResourcePool::for_tests());
    let manager_handle = tokio::spawn(run_manager_loop(
        store.clone(),
        vault,
        cid_allocator,
        vmm_factory,
        workspace_manager,
        catalog_cache,
        net_pool,
        cmd_rx,
        vam_telemetry::TelemetryCapture::default(),
    ));

    async fn boot(manager: &SessionManager, session_id: &str, vm_id: &str, workspace_id: &str) {
        let socket = format!("/tmp/vam-vsock-{vm_id}.socket_52");
        let vm = vm_id.to_string();
        tokio::spawn(async move {
            let mut stream = None;
            for _ in 0..100 {
                if let Ok(connected) = UnixStream::connect(&socket).await {
                    stream = Some(connected);
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            let stream = stream.expect("mock guest failed to connect");
            vam_vsock::run_guest_registration(stream, &vm, 3)
                .await
                .unwrap();
        });
        manager
            .create_session(
                session_id.to_string(),
                None,
                vm_id.to_string(),
                workspace_id.to_string(),
                "profile-1".to_string(),
                None,
            )
            .await
            .unwrap();
    }

    let session_a = "session-e8-a";
    let vm_a = "vm-e8-proxy-a";
    boot(&manager, session_a, vm_a, "ws-e8-a").await;
    let socket_55 = format!("/tmp/vam-vsock-{vm_a}.socket_55");
    assert!(
        std::path::Path::new(&socket_55).exists(),
        "port 55 socket must exist after boot"
    );

    let mut client = UnixStream::connect(&socket_55)
        .await
        .expect("connect port 55");
    let request = format!(
        "CONNECT pinned.example:{} HTTP/1.1\r\nHost: pinned.example:{}\r\n\r\n",
        echo_addr.port(),
        echo_addr.port()
    );
    client.write_all(request.as_bytes()).await.unwrap();
    let established = read_proxy_head(&mut client).await;
    assert!(
        established.starts_with("HTTP/1.1 200"),
        "passthrough CONNECT should tunnel, got {established}"
    );
    client.write_all(b"ping-e8").await.unwrap();
    let mut echoed = [0u8; 7];
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        client.read_exact(&mut echoed),
    )
    .await
    .expect("echo timeout")
    .expect("echo read");
    assert_eq!(&echoed, b"ping-e8");
    drop(client);

    let mut denied = UnixStream::connect(&socket_55).await.expect("deny connect");
    let deny_request = format!(
        "CONNECT denied.example:{} HTTP/1.1\r\nHost: denied.example:{}\r\n\r\n",
        echo_addr.port(),
        echo_addr.port()
    );
    denied.write_all(deny_request.as_bytes()).await.unwrap();
    let denied_head = read_proxy_head(&mut denied).await;
    assert!(
        denied_head.starts_with("HTTP/1.1 403"),
        "denied host must be 403, got {denied_head}"
    );
    drop(denied);

    let events = store.read_outbox(0).unwrap();
    let denies: Vec<_> = events
        .iter()
        .filter(|event| {
            event.run_id == session_a
                && event.event_type == vam_orchestrator::events::TYPE_EGRESS_DECIDED
        })
        .map(|event| decode_egress_decided(&event.payload))
        .filter(|event| event.disposition == "DENY")
        .collect();
    assert!(
        denies.iter().any(|event| {
            event.domain == "denied.example" && event.port == u32::from(echo_addr.port())
        }),
        "DENY event must carry the real port, got {denies:?}"
    );

    let pem_a = store.session_ca_pem(session_a).expect("session A CA");
    assert!(!pem_a.contains("PRIVATE KEY"));
    assert!(pem_a.contains("BEGIN CERTIFICATE"));
    assert_data_dir_has_no_private_key(state.path());

    // MockVmm reports one pid, and sessions.vmm_pid is unique, so the second
    // session starts after the first has released that pid.
    manager
        .terminate_session(session_a.to_string())
        .await
        .unwrap();
    assert!(
        !std::path::Path::new(&socket_55).exists(),
        "port 55 socket must be removed on teardown"
    );
    assert!(store.session_ca_pem(session_a).is_none());

    let session_b = "session-e8-b";
    let vm_b = "vm-e8-proxy-b";
    boot(&manager, session_b, vm_b, "ws-e8-b").await;
    let pem_b = store.session_ca_pem(session_b).expect("session B CA");
    assert!(!pem_b.contains("PRIVATE KEY"));
    assert_ne!(pem_a, pem_b);
    assert_ne!(pem_fingerprint(&pem_a), pem_fingerprint(&pem_b));
    assert_data_dir_has_no_private_key(state.path());

    manager
        .terminate_session(session_b.to_string())
        .await
        .unwrap();
    assert!(!std::path::Path::new(&format!("/tmp/vam-vsock-{vm_b}.socket_55")).exists());
    assert!(store.session_ca_pem(session_b).is_none());

    drop(manager);
    manager_handle.await.unwrap();
}

fn budget_http_app(store: Arc<SqliteWorkflowStore>) -> axum::Router {
    let vault = SecretsVault::for_tests(store.clone());
    let cid_allocator = Arc::new(CidAllocator::default());
    let vmm_factory: VmmFactory = Arc::new(|_vm_id, _guest_cid| Box::new(MockVmm::new()));
    let (manager, _cmd_rx) = SessionManager::new(store.clone(), cid_allocator, vmm_factory);
    api::router(AppState {
        store,
        vault,
        manager,
        pdp: Arc::new(ControlPlanePdp::new().expect("pdp")),
        listen_addr: "127.0.0.1:0".to_string(),
        draining: Arc::new(AtomicBool::new(false)),
        ready: Arc::new(AtomicBool::new(true)),
        e2e_mock_scripts: false,
    })
}

fn budget_store() -> (Arc<SqliteWorkflowStore>, String) {
    let store = Arc::new(SqliteWorkflowStore::in_memory().expect("store"));
    store.initialize_catalog_schema().expect("catalog schema");
    let secret = mint_api_key_secret();
    let scopes: Vec<String> = BOOTSTRAP_SCOPES
        .iter()
        .map(|scope| (*scope).to_string())
        .collect();
    store
        .create_api_key("test-key", "test", &secret, "User::\"tester\"", &scopes)
        .expect("api key");
    (store, secret)
}

fn budget_agent(
    urn: &str,
    policy: Option<vam_spec::v1::ReasoningPolicy>,
) -> vam_spec::v1::AgentSpec {
    use vam_spec::v1::agent_spec::InstructionSource;
    vam_spec::v1::AgentSpec {
        resource_id: Some(vam_spec::v1::ResourceId {
            urn: urn.to_string(),
        }),
        display_name: "Budget Agent".to_string(),
        role: "assistant".to_string(),
        instruction_source: Some(InstructionSource::SystemInstructions(
            "Do the task.".to_string(),
        )),
        reasoning_policy: policy,
        allow_any_playbook: true,
        ..Default::default()
    }
}

fn reasoning_policy(iterations: i32, timeout_secs: Option<i64>) -> vam_spec::v1::ReasoningPolicy {
    vam_spec::v1::ReasoningPolicy {
        max_iterations: iterations,
        step_timeout: timeout_secs.map(|seconds| prost_types::Duration { seconds, nanos: 0 }),
        termination: 0,
    }
}

fn strict_budget_playbook() -> vam_spec::v1::PlaybookSpec {
    use prost_types::value::Kind;
    use prost_types::{ListValue, Struct, Value};

    fn text(value: &str) -> Value {
        Value {
            kind: Some(Kind::StringValue(value.to_string())),
        }
    }
    fn object(fields: Vec<(&str, Value)>) -> Value {
        Value {
            kind: Some(Kind::StructValue(Struct {
                fields: fields
                    .into_iter()
                    .map(|(key, value)| (key.to_string(), value))
                    .collect(),
            })),
        }
    }

    vam_spec::v1::PlaybookSpec {
        activation: Some(vam_spec::v1::PlaybookActivation::asking(
            "Use when the request matches this test playbook.",
        )),
        resource_id: Some(vam_spec::v1::ResourceId {
            urn: "urn:vam:playbook:local.vam.dev/acme/budget-strict".to_string(),
        }),
        display_name: "Strict".to_string(),
        description: "Playbook with a strict input schema".to_string(),
        input_schema: Some(Struct {
            fields: [
                ("type".to_string(), text("object")),
                (
                    "additionalProperties".to_string(),
                    Value {
                        kind: Some(Kind::BoolValue(false)),
                    },
                ),
                (
                    "required".to_string(),
                    Value {
                        kind: Some(Kind::ListValue(ListValue {
                            values: vec![text("invoice_url")],
                        })),
                    },
                ),
                (
                    "properties".to_string(),
                    object(vec![(
                        "invoice_url",
                        object(vec![("type", text("string"))]),
                    )]),
                ),
            ]
            .into_iter()
            .collect(),
        }),
        steps: vec![vam_spec::v1::PlaybookStep {
            id: "s1".to_string(),
            description: "step".to_string(),
            instruction: "Do the step.".to_string(),
            mode: vam_spec::v1::StepMode::Agentic as i32,
            ..Default::default()
        }],
        ..Default::default()
    }
}

fn stored_budgets(store: &SqliteWorkflowStore, run_id: &str) -> (String, String, i64) {
    let (spec_json, _, _) = store.get_run_spec_context(run_id).expect("spec row");
    let spec: vam_spec::v1::RunSpec = vam_spec::encoding::json_to_proto(
        "vam.v1.RunSpec",
        spec_json.as_deref().expect("spec json"),
    )
    .unwrap_or_else(|err| panic!("decode {run_id}: {err}"));
    let inputs = spec.inputs.expect("inputs");
    let iterations = match inputs
        .fields
        .get("max_iterations")
        .and_then(|value| value.kind.as_ref())
    {
        Some(prost_types::value::Kind::StringValue(text)) => text.clone(),
        other => panic!("max_iterations string on {run_id}, got {other:?}"),
    };
    let step = match inputs
        .fields
        .get("step_timeout_secs")
        .and_then(|value| value.kind.as_ref())
    {
        Some(prost_types::value::Kind::StringValue(text)) => text.clone(),
        other => panic!("step_timeout_secs string on {run_id}, got {other:?}"),
    };
    (
        iterations,
        step,
        spec.max_duration.expect("max_duration").seconds,
    )
}

async fn post_budget_run(
    store: &Arc<SqliteWorkflowStore>,
    app: &axum::Router,
    secret: &str,
    body: &serde_json::Value,
) -> String {
    // Each run is on a Space of its own (a label names none), made here and named by id.
    let mut body = body.clone();
    let label = body["spec"]["workspaceRefUri"]
        .as_str()
        .unwrap_or("volume://budget")
        .trim_start_matches("volume://")
        .to_string();
    let space = store
        .create_space(LOCAL_USER_ID, None, &label, "run", None)
        .expect("space");
    body["spec"]["spaceId"] = serde_json::json!(space.space_id);
    let body = &body;
    let idem = body["spec"]["idempotencyKey"]
        .as_str()
        .expect("idempotency key");
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/runs")
                .header("authorization", format!("Bearer {secret}"))
                .header("Idempotency-Key", idem)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .expect("create run");
    let status = response.status();
    let text = String::from_utf8(
        hyper::body::to_bytes(response.into_body())
            .await
            .expect("body")
            .to_vec(),
    )
    .expect("utf-8");
    assert!(status.is_success(), "{status} {text}");
    body["spec"]["runId"]["urn"]
        .as_str()
        .expect("run id")
        .to_string()
}

fn catalog_run_body(
    run_id: &str,
    workspace: &str,
    agent_urn: &str,
    inputs: serde_json::Value,
) -> serde_json::Value {
    serde_json::json!({
        "spec": {
            "runId": {"urn": run_id},
            "agentRef": {"resourceId": {"urn": agent_urn}},
            "inputs": inputs,
            "workspaceRefUri": format!("volume://{workspace}"),
            "idempotencyKey": workspace
        }
    })
}

/// create_run stores the same resolved budgets for every run kind.
#[tokio::test]
async fn create_run_stores_default_budgets_for_every_run_kind() {
    let _env = HERMETIC_ENV_LOCK
        .lock()
        .unwrap_or_else(|err| err.into_inner());
    let _state = isolate_state();
    let (store, secret) = budget_store();
    let agent_urn = "urn:vam:agent:local.vam.dev/acme/budget-default";
    store
        .put_agent_with_owner(&budget_agent(agent_urn, None), LOCAL_USER_ID)
        .expect("publish agent");
    store
        .put_playbook_with_owner(&strict_budget_playbook(), LOCAL_USER_ID)
        .expect("publish playbook");
    let app = budget_http_app(store.clone());

    let plain = post_budget_run(
        &store,
        &app,
        &secret,
        &catalog_run_body(
            "urn:vam:run:local.vam.dev/acme/budget-plain",
            "budget-plain",
            agent_urn,
            serde_json::json!({"prompt": "hello"}),
        ),
    )
    .await;
    let mut playbook = catalog_run_body(
        "urn:vam:run:local.vam.dev/acme/budget-playbook",
        "budget-playbook",
        agent_urn,
        serde_json::json!({
            "prompt": "hello",
            "invoice_url": "https://example.com/invoice.pdf"
        }),
    );
    playbook["spec"]["playbookRef"] = serde_json::json!({
        "resourceId": {"urn": "urn:vam:playbook:local.vam.dev/acme/budget-strict"}
    });
    let playbook_run = post_budget_run(&store, &app, &secret, &playbook).await;
    let inline = serde_json::json!({
        "spec": {
            "runId": {"urn": "urn:vam:run:local.vam.dev/acme/budget-inline"},
            "inputs": {"prompt": "hello"},
            "workspaceRefUri": "volume://budget-inline",
            "idempotencyKey": "budget-inline"
        },
        "inlineAgent": {
            "resourceId": {"urn": "urn:vam:agent:local.vam.dev/acme/budget-inline"},
            "displayName": "Inline",
            "role": "assistant",
            "systemInstructions": "Do the task.",
            "allowAnyPlaybook": true
        }
    });
    let inline_run = post_budget_run(&store, &app, &secret, &inline).await;

    for run_id in [plain, playbook_run, inline_run] {
        assert_eq!(
            stored_budgets(&store, &run_id),
            ("50".to_string(), "300".to_string(), 1_800),
            "{run_id}"
        );
    }
}

/// Agent policy, the hard cap, a lowering caller, and maxDuration.
#[tokio::test]
async fn create_run_applies_agent_policy_caller_cap_and_duration() {
    let _env = HERMETIC_ENV_LOCK
        .lock()
        .unwrap_or_else(|err| err.into_inner());
    let _state = isolate_state();
    let (store, secret) = budget_store();
    let default_urn = "urn:vam:agent:local.vam.dev/acme/budget-default";
    let eight_urn = "urn:vam:agent:local.vam.dev/acme/budget-eight";
    let cap_urn = "urn:vam:agent:local.vam.dev/acme/budget-cap";
    store
        .put_agent_with_owner(&budget_agent(default_urn, None), LOCAL_USER_ID)
        .expect("default agent");
    store
        .put_agent_with_owner(
            &budget_agent(eight_urn, Some(reasoning_policy(8, Some(60)))),
            LOCAL_USER_ID,
        )
        .expect("policy agent");
    store
        .put_agent_with_owner(
            &budget_agent(cap_urn, Some(reasoning_policy(500, None))),
            LOCAL_USER_ID,
        )
        .expect("capped agent");
    let app = budget_http_app(store.clone());

    let policy_run = post_budget_run(
        &store,
        &app,
        &secret,
        &catalog_run_body(
            "urn:vam:run:local.vam.dev/acme/budget-policy",
            "budget-policy",
            eight_urn,
            serde_json::json!({"prompt": "hello"}),
        ),
    )
    .await;
    assert_eq!(
        stored_budgets(&store, &policy_run),
        ("8".to_string(), "60".to_string(), 1_800)
    );

    let capped_run = post_budget_run(
        &store,
        &app,
        &secret,
        &catalog_run_body(
            "urn:vam:run:local.vam.dev/acme/budget-capped",
            "budget-capped",
            cap_urn,
            serde_json::json!({"prompt": "hello"}),
        ),
    )
    .await;
    assert_eq!(
        stored_budgets(&store, &capped_run),
        ("200".to_string(), "300".to_string(), 1_800)
    );

    let lowered = post_budget_run(
        &store,
        &app,
        &secret,
        &catalog_run_body(
            "urn:vam:run:local.vam.dev/acme/budget-lower",
            "budget-lower",
            default_urn,
            serde_json::json!({"prompt": "hello", "max_iterations": "15"}),
        ),
    )
    .await;
    assert_eq!(stored_budgets(&store, &lowered).0, "15");

    let raised = post_budget_run(
        &store,
        &app,
        &secret,
        &catalog_run_body(
            "urn:vam:run:local.vam.dev/acme/budget-raise",
            "budget-raise",
            default_urn,
            serde_json::json!({"prompt": "hello", "max_iterations": "99"}),
        ),
    )
    .await;
    assert_eq!(stored_budgets(&store, &raised).0, "50");

    let mut kept = catalog_run_body(
        "urn:vam:run:local.vam.dev/acme/budget-duration",
        "budget-duration",
        default_urn,
        serde_json::json!({"prompt": "hello"}),
    );
    kept["spec"]["maxDuration"] = serde_json::json!("600s");
    let duration_run = post_budget_run(&store, &app, &secret, &kept).await;
    assert_eq!(stored_budgets(&store, &duration_run).2, 600);
}

/// AR-3.10: vamd restarts while a question is pending on a cold-parked run. The
/// owner then answers: a wrong answer is refused and the question stays open, a
/// valid one resumes the task on a new VM with the answer as `ask_user`'s result.
#[tokio::test]
async fn a_question_pending_across_a_vamd_restart_resumes_with_the_answer() {
    let _env = HERMETIC_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let state = isolate_state();
    let db_path = state.path().join("question-restart.db");
    let first = Arc::new(SqliteWorkflowStore::open(&db_path).expect("open"));
    let wait = seed_paused_question("question-restart", first.clone());

    // The run parks cold (the keep-warm window closed), then vamd goes away.
    let park_seen = Arc::new(std::sync::Mutex::new(None));
    let park_guest = tokio::spawn(serve_resume_guest(wait.vm_id.clone(), park_seen));
    let vmm_factory: VmmFactory = Arc::new(|_, _| Box::new(MockVmm::new()));
    let (manager, manager_handle) = spawn_manager(wait.store.clone(), vmm_factory.clone());
    park_after_keep_warm(&wait, &manager).await;
    park_guest.abort();
    drop(manager);
    manager_handle.await.unwrap();
    drop(wait);
    drop(first);

    // vamd restarts: a new store on the same file, a new manager.
    let store = Arc::new(SqliteWorkflowStore::open(&db_path).expect("reopen after restart"));
    let wait = PausedHitl {
        store: store.clone(),
        session_id: "sess-question-restart".to_string(),
        vm_id: "vm-question-restart".to_string(),
        workspace_id: "ws-question-restart".to_string(),
        run_id: "urn:vam:run:local.vam.dev/acme/run-question-restart".to_string(),
        approval_id: "urn:vam:hitl:local.vam.dev/acme/approval-question-restart".to_string(),
        marker: "checkpoint-question-restart".to_string(),
        args_digest: "digest-question-restart".to_string(),
    };
    let (manager, manager_handle) = spawn_manager(store.clone(), vmm_factory);
    assert_eq!(store.get_run_state(&wait.run_id).unwrap(), "PAUSED");
    assert_eq!(
        store.pending_approval_kind(&wait.run_id).as_deref(),
        Some("question"),
        "the question survived the restart"
    );

    let decide = |response: Option<&str>| {
        store.decide_approval(
            &vam_spec::v1::ResourceId {
                urn: wait.approval_id.clone(),
            },
            &vam_spec::v1::ResourceId {
                urn: "urn:vam:token:local.vam.dev/acme/token-question".to_string(),
            },
            "SUBMITTED",
            "User::\"local\"",
            vam_orchestrator::LOCAL_USER_ID,
            None,
            None,
            None,
            response,
        )
    };
    // Not one of the offered choices, and too large: both refused, still pending.
    assert!(decide(Some(r#"{"answer":"Linus"}"#)).is_err());
    let huge = format!(r#"{{"answer":"{}"}}"#, "x".repeat(20 * 1024));
    assert!(decide(Some(&huge)).is_err());
    assert!(store
        .get_approval(&wait.approval_id)
        .unwrap()
        .decision
        .is_none());
    assert_eq!(store.get_run_state(&wait.run_id).unwrap(), "PAUSED");

    let seen = Arc::new(std::sync::Mutex::new(None));
    let watcher = watch_cold_boot(store.clone(), wait.session_id.clone(), seen.clone());
    let record = decide(Some(r#"{"answer":"Ada"}"#)).expect("valid answer");
    // The entry claims the parked run itself (decide_as claims before it spawns the resume).
    vam_orchestrator::api::resume_after_hitl_decision(manager.clone(), store.clone(), record)
        .await
        .expect("cold resume after a restart");

    assert_eq!(store.get_run_state(&wait.run_id).unwrap(), "SUCCEEDED");
    let request = seen
        .lock()
        .unwrap()
        .clone()
        .expect("resume task on the new VM");
    let inputs = request.inputs.expect("resume inputs");
    assert_eq!(struct_bool(&inputs, "hitl_resume"), Some(true));
    assert_eq!(
        struct_string(&inputs, "approved_tool").as_deref(),
        Some("ask_user")
    );
    let answer = match inputs
        .fields
        .get("hitl_answer")
        .and_then(|v| v.kind.as_ref())
    {
        Some(prost_types::value::Kind::StructValue(answer)) => answer.clone(),
        other => panic!("hitl_answer missing: {other:?}"),
    };
    assert_eq!(struct_string(&answer, "answer").as_deref(), Some("Ada"));
    // No grant follows the answer to the new VM.
    let resumed = store
        .load_all_sessions()
        .unwrap()
        .into_iter()
        .find(|s| s.session_id != wait.session_id)
        .expect("new session");
    assert!(!store.has_hitl_grant(&resumed.session_id, "ask_user", &wait.args_digest));

    watcher.abort();
    drop(manager);
    manager_handle.await.unwrap();
}
