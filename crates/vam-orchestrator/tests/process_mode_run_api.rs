//! Process-guest mode refuses runs for any model but `mock-model` (feature
//! `process-guest`). Own test binary: installing process mode is process-wide.
#![cfg(feature = "process-guest")]

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;
use vam_core::{CidAllocator, MockVmm};
use vam_orchestrator::api::auth::ControlPlanePdp;
use vam_orchestrator::api::{self, AppState};
use vam_orchestrator::keys::{mint_api_key_secret, BOOTSTRAP_SCOPES};
use vam_orchestrator::manager::SessionManager;
use vam_orchestrator::secrets::SecretsVault;
use vam_orchestrator::workflow::SqliteWorkflowStore;
use vam_orchestrator::{VmmFactory, LOCAL_USER_ID};
use vam_spec::v1::{PlaybookSpec, PlaybookStep, ResourceId, StepMode};

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

/// Publishes and releases the playbook that the run fixtures reference.
/// Run creation resolves playbook refs against the catalog.
fn seed_test_playbook(store: &SqliteWorkflowStore) {
    let playbook = PlaybookSpec {
        activation: Some(vam_spec::v1::PlaybookActivation::asking(
            "Use when the request matches this test playbook.",
        )),
        resource_id: Some(ResourceId {
            urn: "urn:vam:playbook:local.vam.dev/acme/test-pb".to_string(),
        }),
        display_name: "Test playbook".to_string(),
        description: "Playbook referenced by the API test run fixtures".to_string(),
        steps: vec![PlaybookStep {
            id: "s1".to_string(),
            description: "step".to_string(),
            instruction: "Do the step.".to_string(),
            mode: StepMode::Agentic as i32,
            ..Default::default()
        }],
        ..Default::default()
    };
    store
        .put_playbook_with_owner(&playbook, LOCAL_USER_ID)
        .expect("seed test playbook");
}

fn bearer_json(
    method: &str,
    uri: &str,
    secret: &str,
    idem: &str,
    body: serde_json::Value,
) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {secret}"))
        .header("Idempotency-Key", idem)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

async fn response_body(response: axum::response::Response) -> String {
    String::from_utf8(
        hyper::body::to_bytes(response.into_body())
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap()
}

fn create_run_body(run_id: &str, idem: &str) -> serde_json::Value {
    serde_json::json!({
        "spec": {
            "runId": {"urn": run_id},
            "agentRef": {
                "resourceId": {"urn": "urn:vam:agent:local.vam.dev/acme/test-agent"},
                "version": {"major": 1, "minor": 0, "patch": 0},
                "digest": {"algorithm": "ALGORITHM_SHA256", "hexHash": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"}
            },
            "playbookRef": {
                "resourceId": {"urn": "urn:vam:playbook:local.vam.dev/acme/test-pb"},
                "version": {"major": 1, "minor": 0, "patch": 0},
                "digest": {"algorithm": "ALGORITHM_SHA256", "hexHash": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"}
            },
            "inputs": {"fields": {"prompt": {"stringValue": "hello from api test"}}},
            "workspaceRefUri": "volume://mock",
            "idempotencyKey": idem
        },
        "inlineAgent": {
            "resourceId": {"urn": "urn:vam:agent:local.vam.dev/acme/test-agent"},
            "displayName": "Test Agent",
            "description": "inline",
            "role": "Tester",
            "systemInstructions": "test",
            "allowAnyPlaybook": true
        }
    })
}

fn app_state(store: Arc<SqliteWorkflowStore>) -> AppState {
    let vault = SecretsVault::for_tests(store.clone());
    let vmm_factory: VmmFactory = Arc::new(|_vm_id, _guest_cid| Box::new(MockVmm::new()));
    let (manager, _cmd_rx) = SessionManager::new(
        store.clone(),
        Arc::new(CidAllocator::default()),
        vmm_factory,
    );
    AppState {
        store,
        vault,
        manager,
        pdp: Arc::new(ControlPlanePdp::new().expect("pdp")),
        listen_addr: "127.0.0.1:8080".to_string(),
        draining: Arc::new(AtomicBool::new(false)),
        ready: Arc::new(AtomicBool::new(true)),
        // Scripted mode is on (the mock-model gate passes); only process mode can refuse.
        e2e_mock_scripts: true,
    }
}

async fn post_run(model: &str, run: &str) -> (StatusCode, String) {
    let _state = isolate_state();
    let (store, secret) = bootstrap_store();
    seed_test_playbook(&store);
    let app = api::router(app_state(store));
    let mut body = create_run_body(&format!("urn:vam:run:local.vam.dev/acme/{run}"), run);
    body["spec"]["inputs"]["fields"]["model"] = serde_json::json!({"stringValue": model});
    let resp = app
        .oneshot(bearer_json("POST", "/v1/runs", &secret, run, body))
        .await
        .unwrap();
    let status = resp.status();
    (status, response_body(resp).await)
}

#[tokio::test]
async fn runs_on_any_model_but_mock_model_are_refused_in_process_mode() {
    vam_orchestrator::process_mode::install(vam_orchestrator::process_mode::ProcessMode {
        guest_bin: "/bin/sh".into(),
        state_root: "/tmp/vam-process-mode-test".into(),
        connector_endpoints: Default::default(),
        marketplace_urls: Default::default(),
    });
    assert_eq!(vam_orchestrator::guest_mode(), "process");
    for (i, model) in ["openai/gpt-4o", "anthropic/claude-sonnet-4", "mock-model-2"]
        .into_iter()
        .enumerate()
    {
        let (status, text) = post_run(model, &format!("run-pm-refused-{i}")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{model}: {text}");
        assert!(text.contains("serves only 'mock-model'"), "{model}: {text}");
    }
    let (status, text) = post_run("mock-model", "run-pm-accepted").await;
    assert!(
        status.is_success(),
        "mock-model must pass the gate: {status} {text}"
    );
}
