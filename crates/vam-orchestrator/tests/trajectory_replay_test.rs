// Env-var serialisation lock is deliberately held for the whole test (process-global env); not a deadlock risk.
#![allow(clippy::await_holding_lock)]
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use ed25519_dalek::SigningKey;
use tower::ServiceExt;
use vam_core::{CidAllocator, MockVmm};
use vam_orchestrator::api::auth::ControlPlanePdp;
use vam_orchestrator::api::{self, AppState};
use vam_orchestrator::keys::{mint_api_key_secret, BOOTSTRAP_SCOPES};
use vam_orchestrator::manager::SessionManager;
use vam_orchestrator::secrets::SecretsVault;
use vam_orchestrator::trajectory::{load_trajectory_replay, persist_trajectory_replay_blob};
use vam_orchestrator::workflow::SqliteWorkflowStore;
use vam_orchestrator::{OrchestratorError, OutboxEvent, VmmFactory};
use vam_security::ApprovalSigner;
use vam_spec::v1::ResourceId;
use vam_storage::{ChunkStore, Drive9Layout};
use vam_telemetry::{
    compare_trajectories, parse_trajectory_replay, PartitionedAuditLedger, RunOutcomeAttestation,
};

fn bootstrap_store() -> (Arc<SqliteWorkflowStore>, String) {
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    store.initialize_catalog_schema().unwrap();
    let secret = mint_api_key_secret();
    let scopes: Vec<String> = BOOTSTRAP_SCOPES.iter().map(|s| s.to_string()).collect();
    store
        .create_api_key("test-key", "test", &secret, "User::\"tester\"", &scopes)
        .unwrap();
    (store, secret)
}

fn app_state_http_only(store: Arc<SqliteWorkflowStore>) -> AppState {
    let vault = SecretsVault::for_tests(store.clone());
    let cid_allocator = Arc::new(CidAllocator::default());
    let vmm_factory: VmmFactory = Arc::new(|_vm_id, _guest_cid| Box::new(MockVmm::new()));
    let (manager, _cmd_rx) = SessionManager::new(store.clone(), cid_allocator, vmm_factory);
    AppState {
        store,
        vault,
        manager,
        pdp: Arc::new(ControlPlanePdp::new().expect("pdp")),
        listen_addr: "127.0.0.1:8080".to_string(),
        draining: Arc::new(AtomicBool::new(false)),
        ready: Arc::new(AtomicBool::new(true)),
        e2e_mock_scripts: false,
    }
}

fn encode_run(run_id: &str) -> String {
    run_id
        .chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => c.to_string(),
            _ => format!("%{:02X}", c as u32),
        })
        .collect()
}

fn sample_events(run_id: &str, extra_deny: bool) -> Vec<OutboxEvent> {
    let mut events = vec![
        OutboxEvent {
            sequence: 1,
            run_id: run_id.to_string(),
            event_type: "vam.v1.tool.invoked".into(),
            payload: r#"{"tool_name":"bash","args_digest":"d1"}"#.into(),
        },
        OutboxEvent {
            sequence: 2,
            run_id: run_id.to_string(),
            event_type: "vam.v1.policy.decided".into(),
            payload: r#"{"tool_name":"bash","disposition":"ALLOW","args_digest":"d1"}"#.into(),
        },
    ];
    if extra_deny {
        events.push(OutboxEvent {
            sequence: 3,
            run_id: run_id.to_string(),
            event_type: "vam.v1.policy.decided".into(),
            payload: r#"{"tool_name":"bash","disposition":"DENY","args_digest":"d2"}"#.into(),
        });
    }
    events
}

fn seed_attested_with_digest(store: &SqliteWorkflowStore, run_id: &str, digest: &str) {
    store
        .persist_run_spec(
            run_id,
            "{}",
            Some("sess-traj"),
            "urn:vam:agent:local.vam.dev/acme/tester",
            None,
            "User::\"tester\"",
        )
        .expect("bind");
    let attestation = RunOutcomeAttestation {
        run_id: run_id.to_string(),
        session_id: "sess-traj".to_string(),
        goal: "sha256:abc".to_string(),
        terminal_state: "SUCCEEDED".to_string(),
        terminator: "agent".to_string(),
        trajectory_digest: digest.to_string(),
        artifacts: vec![],
        paths: vec![],
        skills: vec![],
        tools: vec!["bash".to_string()],
        eval: None,
        cost: None,
        timestamp_unix_ms: 1_778_000_000_000,
        thread_log_head_seq: None,
        thread_log_head_digest: None,
        workspace: None,
    };
    let signer = ApprovalSigner::new("host-key-traj", SigningKey::from_bytes(&[7u8; 32]));
    let envelope = attestation.sign(&signer).expect("sign");
    let json = vam_spec::encoding::proto_to_json("vam.v1.SignedEnvelope", &envelope).expect("json");
    store
        .append_outbox_event(run_id, "vam.v1.run.attested", &json)
        .expect("attest");
}

#[test]
fn compare_identical_blobs_equal_extra_deny_differs() {
    let a_events = sample_events("run-a", false);
    let mut b_events = sample_events("run-b", true);
    let replay_a = parse_trajectory_replay("da", &serde_json::to_vec(&a_events).expect("ser a"))
        .expect("parse a");
    let replay_b = parse_trajectory_replay("db", &serde_json::to_vec(&b_events).expect("ser b"))
        .expect("parse b");
    assert!(compare_trajectories(&replay_a, &replay_a).equal);
    let diff = compare_trajectories(&replay_a, &replay_b);
    assert!(!diff.equal);
    assert_eq!(diff.only_b.len(), 1);
    assert_eq!(diff.only_b[0].disposition.as_deref(), Some("DENY"));
    b_events.pop();
}

/// `VAM_DRIVE9_ROOT` is process-wide, and the tests below each point it at their
/// own temp dir. Hold this lock while they run, or one test's handler reads the
/// other test's root after that root has been deleted.
static DRIVE9_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[tokio::test]
async fn get_trajectory_returns_sanitized_cas_blob() {
    let _env = DRIVE9_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("temp");
    std::env::set_var("VAM_DRIVE9_ROOT", temp.path());
    let layout = Drive9Layout::new(temp.path().to_path_buf());
    let chunk_store = ChunkStore::new(layout);
    let (store, secret) = bootstrap_store();
    store.set_audit_ledger(Arc::new(PartitionedAuditLedger::new()));

    let run_id = "urn:vam:run:local.vam.dev/acme/traj-ok";
    let events = sample_events(run_id, false);
    let digest = persist_trajectory_replay_blob(&events, &chunk_store).expect("persist");
    seed_attested_with_digest(&store, run_id, &digest);

    let replay = load_trajectory_replay(&store, &chunk_store, run_id).expect("load");
    assert_eq!(replay.digest, digest);
    assert_eq!(replay.events.len(), 2);
    assert_eq!(replay.events[0].tool_name.as_deref(), Some("bash"));
    let json = serde_json::to_string(&replay).unwrap();
    assert!(!json.contains("args_digest"));
    assert!(!json.contains("stdout"));

    let encoded = encode_run(run_id);
    let app = api::router(app_state_http_only(store));
    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/v1/runs/{encoded}/trajectory"))
                .header("authorization", format!("Bearer {secret}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = hyper::body::to_bytes(response.into_body()).await.unwrap();
    let got: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(got["digest"], digest);
    assert_eq!(got["events"].as_array().map(|a| a.len()), Some(2));
}

#[tokio::test]
async fn unfinished_run_trajectory_returns_409() {
    let _env = DRIVE9_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().expect("temp");
    std::env::set_var("VAM_DRIVE9_ROOT", temp.path());
    let (store, secret) = bootstrap_store();
    let run_id = "urn:vam:run:local.vam.dev/acme/traj-inflight";
    store
        .create_run(&ResourceId {
            urn: run_id.to_string(),
        })
        .unwrap();

    let layout = Drive9Layout::new(temp.path().to_path_buf());
    let chunk_store = ChunkStore::new(layout);
    match load_trajectory_replay(&store, &chunk_store, run_id) {
        Err(OrchestratorError::Conflict(msg)) => {
            assert!(msg.contains("no trajectory blob"), "{msg}");
        }
        other => panic!("expected Conflict, got {other:?}"),
    }

    let encoded = encode_run(run_id);
    let app = api::router(app_state_http_only(store));
    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/v1/runs/{encoded}/trajectory"))
                .header("authorization", format!("Bearer {secret}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
}
