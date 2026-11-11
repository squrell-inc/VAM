// Env-var serialisation lock is deliberately held for the whole test (process-global env); not a deadlock risk.
#![allow(clippy::await_holding_lock)]
use std::path::PathBuf;
use std::sync::Arc;
use tokio::net::UnixStream;
use vam_core::{CidAllocator, MockVmm};
use vam_network::NetResourcePool;
use vam_orchestrator::manager::{drive9_runtime, run_manager_loop, SessionManager};
use vam_orchestrator::secrets::SecretsVault;
use vam_orchestrator::session::ObservedState;
use vam_orchestrator::workflow::SqliteWorkflowStore;
use vam_orchestrator::VmmFactory;

/// Tests here share process env (`VAM_DRIVE9_ROOT`, `VAM_SNAPSHOT_ROOT`, example
/// paths), so each test holds this lock while it runs.
static SCENARIO_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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

#[tokio::test]
async fn test_workspace_seeding_all_agents() {
    let _env = SCENARIO_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let cid_allocator = Arc::new(CidAllocator::default());

    let vmm_factory: VmmFactory = Arc::new(|_id, _guest_cid| Box::new(MockVmm::new()));

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

    // Test workspaces for our agent archetypes
    let agent_workspaces = vec![
        ("sess-bun", "vm-bun", "bun-assistant"),
        ("sess-sqlite", "vm-sqlite", "sqlite-assistant"),
        ("sess-chaos", "vm-chaos", "chaos-assistant"),
        ("sess-jira", "vm-jira", "jira-assistant"),
    ];

    for (sess_id, vm_id, ws_id) in agent_workspaces.into_iter() {
        println!("Testing workspace materialization for {}", ws_id);

        let vsock_uds_path = format!("/tmp/vam-vsock-{}.socket_52", vm_id);
        let _ = std::fs::remove_file(&vsock_uds_path);

        // Spawn mock guest registration handshake
        let vsock_path_clone = vsock_uds_path.clone();
        let vm_id_clone = vm_id.to_string();
        let handshake_task = tokio::spawn(async move {
            let mut stream = None;
            for _ in 0..100 {
                if let Ok(s) = UnixStream::connect(&vsock_path_clone).await {
                    stream = Some(s);
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            if let Some(s) = stream {
                let _ = vam_vsock::run_guest_registration(s, &vm_id_clone, 3).await;
            }
        });

        manager
            .create_session(
                sess_id.to_string(),
                None,
                vm_id.to_string(),
                ws_id.to_string(),
                "profile-test".to_string(),
                None,
            )
            .await
            .unwrap();

        let _ = handshake_task.await;

        let record = store.load_session(sess_id).unwrap();
        assert_eq!(record.observed_state, ObservedState::Running);

        // Terminate and clean up session
        manager
            .terminate_session(sess_id.to_string())
            .await
            .unwrap();
        let record = store.load_session(sess_id).unwrap();
        assert_eq!(record.observed_state, ObservedState::Terminated);

        let _ = std::fs::remove_file(&vsock_uds_path);
    }

    drop(manager);
    manager_handle.await.unwrap();
}

#[tokio::test]
async fn test_sqlite_assistant_tool_execution() {
    let _env = SCENARIO_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let script_path = if PathBuf::from("examples/sqlite-assistant/skills/db-tools/execute.py")
        .exists()
    {
        PathBuf::from("examples/sqlite-assistant/skills/db-tools/execute.py")
    } else if let Ok(manifest) = std::env::var("CARGO_MANIFEST_DIR") {
        PathBuf::from(manifest).join("../../examples/sqlite-assistant/skills/db-tools/execute.py")
    } else {
        panic!("run from the repo root or under cargo: no examples directory to find");
    };
    if !script_path.exists() {
        panic!("Script does not exist at {:?}", script_path);
    }

    let test_db_path = format!("/tmp/test_scenario_{}.db", std::process::id());
    let _ = std::fs::remove_file(&test_db_path);

    // 1. Test init_db
    let mut child = tokio::process::Command::new("python3")
        .arg(&script_path)
        .arg("init_db")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("Failed to spawn python3");

    {
        use tokio::io::AsyncWriteExt;
        let mut stdin = child.stdin.take().unwrap();
        let payload = serde_json::json!({ "db_path": test_db_path });
        stdin
            .write_all(payload.to_string().as_bytes())
            .await
            .unwrap();
        stdin.flush().await.unwrap();
    }

    let output = child.wait_with_output().await.unwrap();
    assert!(
        output.status.success(),
        "init_db failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let init_res: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(init_res["success"], true);

    // 2. Test insert_fact
    let mut child = tokio::process::Command::new("python3")
        .arg(&script_path)
        .arg("insert_fact")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("Failed to spawn python3");

    {
        use tokio::io::AsyncWriteExt;
        let mut stdin = child.stdin.take().unwrap();
        let payload = serde_json::json!({
            "db_path": test_db_path,
            "category": "storage",
            "key": "drive9_type",
            "value": "CAS + CoW Overlay"
        });
        stdin
            .write_all(payload.to_string().as_bytes())
            .await
            .unwrap();
        stdin.flush().await.unwrap();
    }

    let output = child.wait_with_output().await.unwrap();
    assert!(output.status.success());

    // 3. Test query_db
    let mut child = tokio::process::Command::new("python3")
        .arg(&script_path)
        .arg("query_db")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("Failed to spawn python3");

    {
        use tokio::io::AsyncWriteExt;
        let mut stdin = child.stdin.take().unwrap();
        let payload = serde_json::json!({
            "db_path": test_db_path,
            "query": "SELECT category, key, value FROM facts WHERE key = 'drive9_type'"
        });
        stdin
            .write_all(payload.to_string().as_bytes())
            .await
            .unwrap();
        stdin.flush().await.unwrap();
    }

    let output = child.wait_with_output().await.unwrap();
    assert!(output.status.success());
    let query_res: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(query_res["row_count"], 1);
    assert_eq!(query_res["rows"][0]["value"], "CAS + CoW Overlay");

    // Clean up test DB
    let _ = std::fs::remove_file(&test_db_path);
}

#[tokio::test]
async fn test_chaos_tools_execution() {
    let _env = SCENARIO_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let script_path = if PathBuf::from("examples/chaos-assistant/skills/chaos-tools/execute.py")
        .exists()
    {
        PathBuf::from("examples/chaos-assistant/skills/chaos-tools/execute.py")
    } else if let Ok(manifest) = std::env::var("CARGO_MANIFEST_DIR") {
        PathBuf::from(manifest).join("../../examples/chaos-assistant/skills/chaos-tools/execute.py")
    } else {
        panic!("run from the repo root or under cargo: no examples directory to find");
    };
    assert!(script_path.exists());

    // 1. Test probe_sandbox
    let mut child = tokio::process::Command::new("python3")
        .arg(&script_path)
        .arg("probe_sandbox")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("Failed to spawn python3");

    {
        use tokio::io::AsyncWriteExt;
        let mut stdin = child.stdin.take().unwrap();
        let payload = serde_json::json!({ "target_path": "/etc/shadow" });
        stdin
            .write_all(payload.to_string().as_bytes())
            .await
            .unwrap();
        stdin.flush().await.unwrap();
    }

    let output = child.wait_with_output().await.unwrap();
    assert!(output.status.success());
    let res: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(res["success"], true);

    // 2. Test check_env
    let mut child = tokio::process::Command::new("python3")
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .arg(&script_path)
        .arg("check_env")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("Failed to spawn python3");

    {
        use tokio::io::AsyncWriteExt;
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(b"{}").await.unwrap();
        stdin.flush().await.unwrap();
    }

    let output = child.wait_with_output().await.unwrap();
    assert!(output.status.success());
    let res: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(res["success"], true);
    assert_eq!(res["is_clean"], true);
}
