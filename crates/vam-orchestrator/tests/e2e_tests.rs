#![allow(unused_imports, dead_code)]
use std::path::PathBuf;
use std::sync::Arc;
use vam_core::{CidAllocator, FirecrackerVmm};
use vam_network::NetResourcePool;
use vam_orchestrator::manager::{drive9_runtime, run_manager_loop, SessionManager};
use vam_orchestrator::secrets::SecretsVault;
use vam_orchestrator::session::ObservedState;
use vam_orchestrator::workflow::SqliteWorkflowStore;
use vam_orchestrator::VmmFactory;
use vam_spec::v1::{ExecutionLease, ResourceId};

/// Reads a lane asset path from the environment (scripts/e2e/firecracker-assets.sh).
fn lane_path(name: &str) -> PathBuf {
    match std::env::var(name) {
        Ok(v) if !v.is_empty() => PathBuf::from(v),
        _ => panic!("{name} is not set; run via scripts/e2e/firecracker-assets.sh --env-file"),
    }
}

fn e2e_test_lease(label: &str) -> ExecutionLease {
    ExecutionLease {
        run_id: Some(ResourceId {
            urn: format!("urn:vam:run:local.vam.dev/acme/e2e-{label}"),
        }),
        step_id: format!("e2e-step-{label}"),
        attempt_id: Some(ResourceId {
            urn: format!("urn:vam:attempt:local.vam.dev/acme/e2e-attempt-{label}"),
        }),
        lease_id: Some(ResourceId {
            urn: format!("urn:vam:lease:local.vam.dev/acme/e2e-lease-{label}"),
        }),
        lifecycle_generation: 1,
        fencing_token: vec![1],
        expires_at: Some(prost_types::Timestamp {
            seconds: 4_102_444_800,
            nanos: 0,
        }),
        idempotency_key: format!("e2e-idemp-{label}"),
    }
}

#[tokio::test]
#[ignore = "needs KVM: firecracker lane"]
#[cfg(target_os = "linux")]
async fn test_vamd_real_firecracker_e2e() {
    real_firecracker_session(false).await;
}

/// The same session boot with the jailer on. Needs root (the jailer chroots and drops
/// to the jail uid), so the lane runs this one test binary under `sudo`.
#[tokio::test]
#[ignore = "needs KVM and root: firecracker lane, run under sudo"]
#[cfg(target_os = "linux")]
async fn test_vamd_real_firecracker_jailed_e2e() {
    assert_eq!(
        vam_core::jail::current_euid(),
        0,
        "the jailer needs root: run this test binary with sudo"
    );
    // A jailed node's golden snapshot has its own key and records in-jail names.
    vam_orchestrator::golden::set_jailed_node(vam_core::jail::JailConfig::default().uid);
    // The vsock sockets live beside the jails (hard-linked into them), as `vamd` does.
    let run_dir = vam_core::jail::JailConfig::default().base_dir.join("run");
    vam_core::jail::prepare_run_dir(&run_dir).unwrap();
    vam_core::jail::set_run_dir(run_dir);
    real_firecracker_session(true).await;
}

#[cfg(target_os = "linux")]
async fn real_firecracker_session(use_jailer: bool) {
    // Sessions spawned in-process skip vamd's startup, which installs the node keys.
    let _ = vam_orchestrator::signing::install(vam_orchestrator::signing::NodeKeys::ephemeral());
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let cid_allocator = Arc::new(CidAllocator::default());
    let session_id = "e2e-session";
    let vm_id = "e2e-vm";

    // Configure VmmFactory to spawn a real FirecrackerVmm
    let firecracker_bin = lane_path("VAMD_FIRECRACKER_BIN");
    let jailer_bin = lane_path("VAMD_JAILER_BIN");

    let vmm_factory: VmmFactory = Arc::new(move |id, guest_cid| {
        Box::new(FirecrackerVmm::new(
            id.to_string(),
            firecracker_bin.clone(),
            jailer_bin.clone(),
            use_jailer,
            guest_cid,
        ))
    });

    let (manager, cmd_rx) =
        SessionManager::new(store.clone(), cid_allocator.clone(), vmm_factory.clone());

    let (workspace_manager, catalog_cache) =
        drive9_runtime().expect("drive9 runtime should initialize");

    let net_pool = Arc::new(NetResourcePool::for_tests());

    // Start manager loop
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

    let start_time = std::time::Instant::now();

    // Create the session (this will boot the real VM, configure it, and await registration)
    manager
        .create_session(
            session_id.to_string(),
            None,
            vm_id.to_string(),
            "workspace-e2e".to_string(),
            "profile-e2e".to_string(),
            None,
        )
        .await
        .unwrap();

    let boot_latency = start_time.elapsed();
    println!(
        "DAEMON_E2E_REAL_BOOT_TO_RUNNING_LATENCY: {:?}",
        boot_latency
    );

    // Check state in SQLite
    let record = store.load_session(session_id).unwrap();
    assert_eq!(record.observed_state, ObservedState::Running);
    assert!(record.vmm_pid.is_some());
    assert!(record.vmm_start_time.is_some());
    assert_eq!(record.guest_cid, Some(3));

    let pid = record.vmm_pid.unwrap();

    // Verify that the PID is actually alive and is a Firecracker process
    let comm = std::fs::read_to_string(format!("/proc/{}/comm", pid)).unwrap();
    assert_eq!(comm.trim(), "firecracker");

    let exec_name = lane_path("VAMD_FIRECRACKER_BIN")
        .file_name()
        .unwrap()
        .to_owned();
    let jail_root = vam_core::jail::JailConfig::default()
        .jail_dir(&exec_name, vm_id)
        .join("root");
    if use_jailer {
        // Chrooted (the jail's root directory is the one the process opened), not root,
        // in its own pid and mount namespaces.
        use std::os::unix::fs::MetadataExt;
        let theirs = std::fs::metadata(format!("/proc/{pid}/root")).unwrap();
        let jail = std::fs::metadata(&jail_root).unwrap();
        assert_eq!((theirs.dev(), theirs.ino()), (jail.dev(), jail.ino()));
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
        let uid_line = status.lines().find(|l| l.starts_with("Uid:")).unwrap();
        assert!(
            uid_line.split_whitespace().skip(1).all(|u| u != "0"),
            "{uid_line}"
        );
        for ns in ["pid", "mnt"] {
            assert_ne!(
                std::fs::read_link(format!("/proc/{pid}/ns/{ns}")).unwrap(),
                std::fs::read_link(format!("/proc/self/ns/{ns}")).unwrap(),
                "own {ns} namespace"
            );
        }
    } else {
        assert!(!jail_root.exists(), "no jail without the jailer");
    }

    // Terminate session
    manager
        .terminate_session(session_id.to_string())
        .await
        .unwrap();

    // Verify State is Terminated in SQLite
    let record = store.load_session(session_id).unwrap();
    assert_eq!(record.observed_state, ObservedState::Terminated);

    // Verify sockets are cleaned up (GC Teardown Invariant)
    assert!(!std::path::Path::new(&format!("/tmp/vam-vsock-{}.socket_52", vm_id)).exists());
    assert!(!std::path::Path::new(&format!("/tmp/vam-fc-{}.socket", vm_id)).exists());
    assert!(
        std::fs::symlink_metadata(vam_core::jail::vsock_uds_path(vm_id)).is_err(),
        "the vsock path (a symlink into the jail) is removed"
    );
    assert!(!jail_root.exists(), "the jail directory is removed");

    // Cleanup & stop manager
    drop(manager);
    manager_handle.await.unwrap();
}

#[tokio::test]
#[ignore = "live model: needs VAM_LIVE_INFERENCE and OPENROUTER_API_KEY; mock only on the lane"]
#[cfg(target_os = "linux")]
async fn test_bun_agent_live_openrouter() {
    // Sessions spawned in-process skip vamd's startup, which installs the node keys.
    let _ = vam_orchestrator::signing::install(vam_orchestrator::signing::NodeKeys::ephemeral());
    // 0. Gate on env variables
    dotenvy::dotenv().ok();
    if std::env::var("VAM_LIVE_INFERENCE").is_err() {
        println!("Skipping test_bun_agent_live_openrouter (VAM_LIVE_INFERENCE is not set).");
        return;
    }
    if std::env::var("OPENROUTER_API_KEY").is_err() {
        panic!("OPENROUTER_API_KEY must be set for live inference testing!");
    }

    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let cid_allocator = Arc::new(CidAllocator::default());
    let session_id = "live-assistant-session";
    let vm_id = "live-assistant-vm";

    // Configure VmmFactory to spawn a real FirecrackerVmm
    let firecracker_bin = lane_path("VAMD_FIRECRACKER_BIN");
    let jailer_bin = lane_path("VAMD_JAILER_BIN");

    let vmm_factory: VmmFactory = Arc::new(move |id, guest_cid| {
        Box::new(FirecrackerVmm::new(
            id.to_string(),
            firecracker_bin.clone(),
            jailer_bin.clone(),
            false, // use_jailer: false
            guest_cid,
        ))
    });

    let (manager, cmd_rx) =
        SessionManager::new(store.clone(), cid_allocator.clone(), vmm_factory.clone());

    let (workspace_manager, catalog_cache) =
        drive9_runtime().expect("drive9 runtime should initialize");

    let net_pool = Arc::new(NetResourcePool::for_tests());

    // Start manager loop
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

    // Force pre-cleanup of old runs if left over
    let vsock_uds_path = PathBuf::from(format!("/tmp/vam-vsock-{}.socket", vm_id));
    let _ = std::fs::remove_file(&vsock_uds_path);
    let _ = std::fs::remove_file(format!("{}_52", vsock_uds_path.to_string_lossy()));
    let _ = std::fs::remove_file(format!("{}_53", vsock_uds_path.to_string_lossy()));
    let _ = std::fs::remove_file(format!("/tmp/vam-fc-{}.socket", vm_id));

    // Create session: using "assistant" as workspace_id to trigger Bun assistant seeding!
    manager
        .create_session(
            session_id.to_string(),
            None,
            vm_id.to_string(),
            "assistant".to_string(),
            "profile-e2e".to_string(),
            None,
        )
        .await
        .unwrap();

    let record = store.load_session(session_id).unwrap();
    assert_eq!(record.observed_state, ObservedState::Running);

    // Call host task driver to execute the prompt
    let prompt_text = "Using your calculator tool, evaluate 42 * 17. Then use write_note to write that number to the note 'result.txt'. Then use list_workspace to confirm the note is written. Finally, summarize what you did.";

    let mut inputs = prost_types::Struct::default();
    inputs.fields.insert(
        "model".to_string(),
        prost_types::Value {
            kind: Some(prost_types::value::Kind::StringValue(
                "openai/gpt-4o-mini".to_string(),
            )),
        },
    );
    inputs.fields.insert(
        "max_iterations".to_string(),
        prost_types::Value {
            kind: Some(prost_types::value::Kind::NumberValue(5.0)),
        },
    );

    println!("Sending task prompt to guest agent...");
    let lease = e2e_test_lease("bun");
    let outcome_res = vam_orchestrator::driver::run_task_on_guest(
        &vsock_uds_path,
        &lease,
        prompt_text,
        Some(inputs),
        180, // 3 minutes timeout
    )
    .await;

    // Check outcome
    match outcome_res {
        Ok(outcome) => {
            println!("Task succeeded!");
            assert_eq!(outcome.state, 1); // TaskOutcomeState::Succeeded
            if let Some(out_struct) = outcome.output {
                if let Some(prost_types::Value {
                    kind: Some(prost_types::value::Kind::StringValue(ref s)),
                    ..
                }) = out_struct.fields.get("answer")
                {
                    println!(
                        "--- Agent Final Answer ---\n{}\n--------------------------",
                        s
                    );
                    assert!(!s.is_empty());
                    assert!(s.contains("714")); // 42 * 17 = 714
                } else {
                    panic!("Expected 'answer' field in outcome output");
                }
            } else {
                panic!("Expected outcome output to be populated");
            }

            if let Some(usage) = outcome.observed_usage {
                println!(
                    "Token usage: prompt={}, completion={}, total={}",
                    usage.prompt_tokens, usage.completion_tokens, usage.total_tokens
                );
                assert!(usage.total_tokens > 0);
            } else {
                panic!("Expected token usage statistics to be present");
            }
        }
        Err(e) => {
            panic!("Live task execution failed: {}", e);
        }
    }

    // Terminate session
    manager
        .terminate_session(session_id.to_string())
        .await
        .unwrap();

    // Cleanup & stop manager
    drop(manager);
    manager_handle.await.unwrap();
}

#[tokio::test]
#[ignore = "live model: needs VAM_LIVE_INFERENCE and OPENROUTER_API_KEY; mock only on the lane"]
#[cfg(target_os = "linux")]
async fn test_sqlite_agent_live_openrouter() {
    // Sessions spawned in-process skip vamd's startup, which installs the node keys.
    let _ = vam_orchestrator::signing::install(vam_orchestrator::signing::NodeKeys::ephemeral());
    // 0. Gate on env variables
    dotenvy::dotenv().ok();
    if std::env::var("VAM_LIVE_INFERENCE").is_err() {
        println!("Skipping test_sqlite_agent_live_openrouter (VAM_LIVE_INFERENCE is not set).");
        return;
    }
    if std::env::var("OPENROUTER_API_KEY").is_err() {
        panic!("OPENROUTER_API_KEY must be set for live inference testing!");
    }

    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let cid_allocator = Arc::new(CidAllocator::default());
    let session_id = "live-sqlite-session";
    let vm_id = "live-sqlite-vm";

    let firecracker_bin = lane_path("VAMD_FIRECRACKER_BIN");
    let jailer_bin = lane_path("VAMD_JAILER_BIN");

    let vmm_factory: VmmFactory = Arc::new(move |id, guest_cid| {
        Box::new(FirecrackerVmm::new(
            id.to_string(),
            firecracker_bin.clone(),
            jailer_bin.clone(),
            false,
            guest_cid,
        ))
    });

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

    let vsock_uds_path = PathBuf::from(format!("/tmp/vam-vsock-{}.socket", vm_id));
    let _ = std::fs::remove_file(&vsock_uds_path);
    let _ = std::fs::remove_file(format!("{}_52", vsock_uds_path.to_string_lossy()));
    let _ = std::fs::remove_file(format!("{}_53", vsock_uds_path.to_string_lossy()));
    let _ = std::fs::remove_file(format!("/tmp/vam-fc-{}.socket", vm_id));

    // Create session: using "sqlite-assistant" as workspace_id to trigger SQLite assistant seeding!
    manager
        .create_session(
            session_id.to_string(),
            None,
            vm_id.to_string(),
            "sqlite-assistant".to_string(),
            "profile-e2e".to_string(),
            None,
        )
        .await
        .unwrap();

    let record = store.load_session(session_id).unwrap();
    assert_eq!(record.observed_state, ObservedState::Running);

    // Call host task driver to execute the prompt autonomously without dictating tool names
    let prompt_text = "We need to document and store our core architecture facts. Please record that our framework's 'vam_runtime' is 'rust_microvm_engine' under the 'framework' category in your persistent memory database. Verify that it was successfully saved, and write a structured analysis summary of our recorded architecture facts to 'ANALYSIS.md'.";

    let mut inputs = prost_types::Struct::default();
    inputs.fields.insert(
        "model".to_string(),
        prost_types::Value {
            kind: Some(prost_types::value::Kind::StringValue(
                "openai/gpt-4o-mini".to_string(),
            )),
        },
    );
    inputs.fields.insert("system_instructions".to_string(), prost_types::Value {
        kind: Some(prost_types::value::Kind::StringValue(
            "You are an autonomous engineering analyst operating inside a secure VAM microVM. You have access to tools for recording structured facts, storing code analysis findings, executing SQL queries against your memory, and writing documentation. Fulfill the user's objective autonomously using your available capabilities.".to_string()
        )),
    });
    inputs.fields.insert(
        "max_iterations".to_string(),
        prost_types::Value {
            kind: Some(prost_types::value::Kind::NumberValue(6.0)),
        },
    );

    println!("Sending autonomous SQLite task prompt to guest agent...");
    let lease = e2e_test_lease("sqlite");
    let outcome_res = vam_orchestrator::driver::run_task_on_guest(
        &vsock_uds_path,
        &lease,
        prompt_text,
        Some(inputs),
        180,
    )
    .await;

    match outcome_res {
        Ok(outcome) => {
            println!("SQLite agent task succeeded!");
            assert_eq!(outcome.state, 1); // TaskOutcomeState::Succeeded
            if let Some(out_struct) = outcome.output {
                if let Some(prost_types::Value {
                    kind: Some(prost_types::value::Kind::StringValue(ref s)),
                    ..
                }) = out_struct.fields.get("answer")
                {
                    println!(
                        "--- Agent Final Answer ---\n{}\n--------------------------",
                        s
                    );
                    assert!(!s.is_empty());
                } else {
                    panic!("Expected 'answer' field in outcome output");
                }
            } else {
                panic!("Expected outcome output to be populated");
            }

            if let Some(usage) = outcome.observed_usage {
                println!(
                    "Token usage: prompt={}, completion={}, total={}",
                    usage.prompt_tokens, usage.completion_tokens, usage.total_tokens
                );
                assert!(usage.total_tokens > 0);
            }
        }
        Err(e) => {
            panic!("Live SQLite task execution failed: {}", e);
        }
    }

    manager
        .terminate_session(session_id.to_string())
        .await
        .unwrap();
    drop(manager);
    manager_handle.await.unwrap();
}

#[tokio::test]
#[ignore = "live model: needs VAM_LIVE_INFERENCE and OPENROUTER_API_KEY; mock only on the lane"]
#[cfg(target_os = "linux")]
async fn test_sqlite_agent_follow_up_question_same_session() {
    // Sessions spawned in-process skip vamd's startup, which installs the node keys.
    let _ = vam_orchestrator::signing::install(vam_orchestrator::signing::NodeKeys::ephemeral());
    dotenvy::dotenv().ok();
    if std::env::var("VAM_LIVE_INFERENCE").is_err() || std::env::var("OPENROUTER_API_KEY").is_err()
    {
        println!("Skipping test (VAM_LIVE_INFERENCE or OPENROUTER_API_KEY not set).");
        return;
    }

    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let cid_allocator = Arc::new(CidAllocator::default());
    let session_id = "live-followup-session";
    let vm_id = "live-followup-vm";

    let firecracker_bin = lane_path("VAMD_FIRECRACKER_BIN");
    let jailer_bin = lane_path("VAMD_JAILER_BIN");

    let vmm_factory: VmmFactory = Arc::new(move |id, guest_cid| {
        Box::new(FirecrackerVmm::new(
            id.to_string(),
            firecracker_bin.clone(),
            jailer_bin.clone(),
            false,
            guest_cid,
        ))
    });

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

    let vsock_uds_path = PathBuf::from(format!("/tmp/vam-vsock-{}.socket", vm_id));
    let _ = std::fs::remove_file(&vsock_uds_path);
    let _ = std::fs::remove_file(format!("{}_52", vsock_uds_path.to_string_lossy()));
    let _ = std::fs::remove_file(format!("{}_53", vsock_uds_path.to_string_lossy()));
    let _ = std::fs::remove_file(format!("/tmp/vam-fc-{}.socket", vm_id));

    // 1. Create and boot session
    manager
        .create_session(
            session_id.to_string(),
            None,
            vm_id.to_string(),
            "sqlite-followup-ws".to_string(),
            "profile-e2e".to_string(),
            None,
        )
        .await
        .unwrap();

    let record = store.load_session(session_id).unwrap();
    assert_eq!(record.observed_state, ObservedState::Running);

    // 2. TURN 1: Initial question to the agent
    println!("\n=== TURN 1: Initial Question ===");
    let prompt_1 = "Please record in your database that 'primary_db' is 'sqlite_embedded' with category='storage'. Confirm it was saved.";
    let mut inputs_1 = prost_types::Struct::default();
    inputs_1.fields.insert(
        "model".to_string(),
        prost_types::Value {
            kind: Some(prost_types::value::Kind::StringValue(
                "openai/gpt-4o-mini".to_string(),
            )),
        },
    );

    let lease_1 = e2e_test_lease("jira-1");
    let outcome_1 = vam_orchestrator::driver::run_task_on_guest(
        &vsock_uds_path,
        &lease_1,
        prompt_1,
        Some(inputs_1),
        120,
    )
    .await
    .expect("Turn 1 failed");

    assert_eq!(outcome_1.state, 1);
    println!("Turn 1 Outcome: {:?}", outcome_1.output);

    // 3. TURN 2: Follow-up question in the EXACT SAME running session!
    println!("\n=== TURN 2: Follow-Up Question (Same Session) ===");
    let prompt_2 = "What was our primary_db according to your memory? Please also record that 'cache_layer' is 'lru_cow' with category='storage', and return both facts from the database.";
    let mut inputs_2 = prost_types::Struct::default();
    inputs_2.fields.insert(
        "model".to_string(),
        prost_types::Value {
            kind: Some(prost_types::value::Kind::StringValue(
                "openai/gpt-4o-mini".to_string(),
            )),
        },
    );

    let lease_2 = e2e_test_lease("jira-2");
    let outcome_2 = vam_orchestrator::driver::run_task_on_guest(
        &vsock_uds_path,
        &lease_2,
        prompt_2,
        Some(inputs_2),
        120,
    )
    .await
    .expect("Turn 2 follow-up failed");

    assert_eq!(outcome_2.state, 1);
    if let Some(out_struct) = outcome_2.output {
        if let Some(prost_types::Value {
            kind: Some(prost_types::value::Kind::StringValue(ref s)),
            ..
        }) = out_struct.fields.get("answer")
        {
            println!(
                "--- Agent Turn 2 Answer ---\n{}\n---------------------------",
                s
            );
            assert!(s.to_lowercase().contains("sqlite_embedded"));
            assert!(s.to_lowercase().contains("lru_cow"));
        } else {
            panic!("Expected 'answer' in outcome output");
        }
    }

    // 4. Terminate session
    manager
        .terminate_session(session_id.to_string())
        .await
        .unwrap();
    drop(manager);
    manager_handle.await.unwrap();
}

#[tokio::test]
#[ignore = "live model: needs VAM_LIVE_INFERENCE and OPENROUTER_API_KEY; mock only on the lane"]
#[cfg(target_os = "linux")]
async fn test_sqlite_agent_workspace_checkpoint_resume() {
    // Sessions spawned in-process skip vamd's startup, which installs the node keys.
    let _ = vam_orchestrator::signing::install(vam_orchestrator::signing::NodeKeys::ephemeral());
    dotenvy::dotenv().ok();
    if std::env::var("VAM_LIVE_INFERENCE").is_err() || std::env::var("OPENROUTER_API_KEY").is_err()
    {
        println!("Skipping test (VAM_LIVE_INFERENCE or OPENROUTER_API_KEY not set).");
        return;
    }

    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let cid_allocator = Arc::new(CidAllocator::default());
    let firecracker_bin = lane_path("VAMD_FIRECRACKER_BIN");
    let jailer_bin = lane_path("VAMD_JAILER_BIN");

    // Shared workspace ID across two distinct sessions!
    let shared_workspace_id = "sqlite-checkpoint-ws";
    let drive9_root =
        std::env::var("VAM_DRIVE9_ROOT").unwrap_or_else(|_| "/var/lib/vam/drive9".into());
    let _ = std::fs::remove_dir_all(format!("{drive9_root}/workspaces/{shared_workspace_id}"));
    let _ = std::fs::remove_dir_all(format!("{drive9_root}/sessions/checkpoint-session-1"));
    let _ = std::fs::remove_dir_all(format!("{drive9_root}/sessions/checkpoint-session-2"));

    // -------------------------------------------------------------
    // SESSION 1: Launch microVM, save database fact, and commit checkpoint
    // -------------------------------------------------------------
    let session_1_id = "checkpoint-session-1";
    let vm_1_id = "checkpoint-vm-1";

    let fb1 = firecracker_bin.clone();
    let jb1 = jailer_bin.clone();
    let vmm_factory_1: VmmFactory = Arc::new(move |id, guest_cid| {
        Box::new(FirecrackerVmm::new(
            id.to_string(),
            fb1.clone(),
            jb1.clone(),
            false,
            guest_cid,
        ))
    });

    let vmm_factory_1_clone = vmm_factory_1.clone();
    let (manager_1, cmd_rx_1) =
        SessionManager::new(store.clone(), cid_allocator.clone(), vmm_factory_1);
    let (workspace_manager_1, catalog_cache_1) =
        drive9_runtime().expect("drive9 runtime should initialize");
    let net_pool_1 = Arc::new(NetResourcePool::for_tests());
    let handle_1 = tokio::spawn(run_manager_loop(
        store.clone(),
        SecretsVault::for_tests(store.clone()),
        cid_allocator.clone(),
        vmm_factory_1_clone,
        workspace_manager_1,
        catalog_cache_1,
        net_pool_1,
        cmd_rx_1,
        vam_telemetry::TelemetryCapture::default(),
    ));

    let vsock_1 = PathBuf::from(format!("/tmp/vam-vsock-{}.socket", vm_1_id));
    let _ = std::fs::remove_file(&vsock_1);
    let _ = std::fs::remove_file(format!("{}_52", vsock_1.to_string_lossy()));
    let _ = std::fs::remove_file(format!("{}_53", vsock_1.to_string_lossy()));
    let _ = std::fs::remove_file(format!("/tmp/vam-fc-{}.socket", vm_1_id));

    println!("\n=== SESSION 1: Creating Checkpoint ===");
    manager_1
        .create_session(
            session_1_id.to_string(),
            None,
            vm_1_id.to_string(),
            shared_workspace_id.to_string(),
            "profile-e2e".to_string(),
            None,
        )
        .await
        .unwrap();

    let prompt_1 = "Please record in your database that 'auth_mode' is 'cedar_policy' under category='security'. Then write a confirmation note to 'CHECKPOINT_NOTE.txt'.";
    let mut inputs_1 = prost_types::Struct::default();
    inputs_1.fields.insert(
        "model".to_string(),
        prost_types::Value {
            kind: Some(prost_types::value::Kind::StringValue(
                "openai/gpt-4o-mini".to_string(),
            )),
        },
    );

    let lease_1 = e2e_test_lease("restore-1");
    let outcome_1 = vam_orchestrator::driver::run_task_on_guest(
        &vsock_1,
        &lease_1,
        prompt_1,
        Some(inputs_1),
        120,
    )
    .await
    .expect("Session 1 task failed");
    assert_eq!(outcome_1.state, 1);

    // Terminate Session 1 -> commits Drive9 Merkle Manifest checkpoint!
    manager_1
        .terminate_session(session_1_id.to_string())
        .await
        .unwrap();
    drop(manager_1);
    handle_1.await.unwrap();

    // -------------------------------------------------------------
    // SESSION 2: Start a NEW session & microVM from the committed workspace checkpoint!
    // -------------------------------------------------------------
    let session_2_id = "checkpoint-session-2";
    let vm_2_id = "checkpoint-vm-2";

    let fb2 = firecracker_bin.clone();
    let jb2 = jailer_bin.clone();
    let vmm_factory_2: VmmFactory = Arc::new(move |id, guest_cid| {
        Box::new(FirecrackerVmm::new(
            id.to_string(),
            fb2.clone(),
            jb2.clone(),
            false,
            guest_cid,
        ))
    });

    let vmm_factory_2_clone = vmm_factory_2.clone();
    let (manager_2, cmd_rx_2) =
        SessionManager::new(store.clone(), cid_allocator.clone(), vmm_factory_2);
    let (workspace_manager_2, catalog_cache_2) =
        drive9_runtime().expect("drive9 runtime should initialize");
    let net_pool_2 = Arc::new(NetResourcePool::for_tests());
    let handle_2 = tokio::spawn(run_manager_loop(
        store.clone(),
        SecretsVault::for_tests(store.clone()),
        cid_allocator.clone(),
        vmm_factory_2_clone,
        workspace_manager_2,
        catalog_cache_2,
        net_pool_2,
        cmd_rx_2,
        vam_telemetry::TelemetryCapture::default(),
    ));

    let vsock_2 = PathBuf::from(format!("/tmp/vam-vsock-{}.socket", vm_2_id));
    let _ = std::fs::remove_file(&vsock_2);
    let _ = std::fs::remove_file(format!("{}_52", vsock_2.to_string_lossy()));
    let _ = std::fs::remove_file(format!("{}_53", vsock_2.to_string_lossy()));
    let _ = std::fs::remove_file(format!("/tmp/vam-fc-{}.socket", vm_2_id));

    println!("\n=== SESSION 2: Resuming from Workspace Checkpoint ===");
    manager_2
        .create_session(
            session_2_id.to_string(),
            None,
            vm_2_id.to_string(),
            shared_workspace_id.to_string(),
            "profile-e2e".to_string(),
            None,
        )
        .await
        .unwrap();

    let prompt_2 = "Query your database to find out what 'auth_mode' was recorded previously under category 'security'. Also read 'CHECKPOINT_NOTE.txt' from the workspace. Summarize both.";
    let mut inputs_2 = prost_types::Struct::default();
    inputs_2.fields.insert(
        "model".to_string(),
        prost_types::Value {
            kind: Some(prost_types::value::Kind::StringValue(
                "openai/gpt-4o-mini".to_string(),
            )),
        },
    );

    let lease_2 = e2e_test_lease("restore-2");
    let outcome_2 = vam_orchestrator::driver::run_task_on_guest(
        &vsock_2,
        &lease_2,
        prompt_2,
        Some(inputs_2),
        120,
    )
    .await
    .expect("Session 2 task failed");
    assert_eq!(outcome_2.state, 1);

    if let Some(out_struct) = outcome_2.output {
        if let Some(prost_types::Value {
            kind: Some(prost_types::value::Kind::StringValue(ref s)),
            ..
        }) = out_struct.fields.get("answer")
        {
            println!("--- Session 2 Restored State Answer ---\n{}\n---------------------------------------", s);
            assert!(s.to_lowercase().contains("cedar_policy"));
        }
    }

    manager_2
        .terminate_session(session_2_id.to_string())
        .await
        .unwrap();
    drop(manager_2);
    handle_2.await.unwrap();
}
