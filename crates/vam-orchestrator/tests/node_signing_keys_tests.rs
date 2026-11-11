//! `GET /v1/node` publishes the node's signing keys: public halves only.
//!
//! One test, because the node keys are installed once per process.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;
use vam_core::{CidAllocator, MockVmm};
use vam_orchestrator::api::auth::ControlPlanePdp;
use vam_orchestrator::api::{self, AppState};
use vam_orchestrator::keys::{mint_api_key_secret, MEMBER_SCOPES};
use vam_orchestrator::manager::SessionManager;
use vam_orchestrator::secrets::SecretsVault;
use vam_orchestrator::signing::{self, NodeKeys};
use vam_orchestrator::users::UserRole;
use vam_orchestrator::workflow::SqliteWorkflowStore;
use vam_orchestrator::VmmFactory;

fn app_state(store: Arc<SqliteWorkflowStore>) -> AppState {
    if std::env::var("VAM_DRIVE9_ROOT").is_err() {
        std::env::set_var("VAM_DRIVE9_ROOT", "/tmp/vam-test-drive9");
    }
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

#[tokio::test]
async fn node_status_lists_public_signing_keys_and_never_a_secret() {
    // Keys on disk, with one retired key moved aside as an operator would.
    let state_dir = tempfile::tempdir().unwrap();
    let first = NodeKeys::load_or_create(state_dir.path()).unwrap();
    let retired_id = first.current().key_id().to_string();
    let retired_dir = state_dir.path().join("signing/retired");
    std::fs::create_dir_all(&retired_dir).unwrap();
    std::fs::rename(
        state_dir.path().join("signing/node-ed25519.key"),
        retired_dir.join("old.key"),
    )
    .unwrap();
    let keys = NodeKeys::load_or_create(state_dir.path()).unwrap();
    let current_id = keys.current().key_id().to_string();
    let current_public = keys.current().public_key_hex();
    let derived_session_seed = hex(&keys.session_signing_key("sess-1").to_bytes());
    assert!(signing::install(keys));

    let seed = std::fs::read_to_string(state_dir.path().join("signing/node-ed25519.key")).unwrap();
    let master =
        std::fs::read_to_string(state_dir.path().join("signing/session-master.key")).unwrap();
    let old_seed = std::fs::read_to_string(retired_dir.join("old.key")).unwrap();

    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    store
        .create_managed_user("alice", "alice", "alice-secret", UserRole::Member)
        .unwrap();
    let secret = mint_api_key_secret();
    let scopes: Vec<String> = MEMBER_SCOPES.iter().map(|s| (*s).to_string()).collect();
    store
        .create_api_key_owned(
            "key-alice",
            "alice",
            &secret,
            "User::\"alice\"",
            &scopes,
            "alice",
        )
        .unwrap();
    let app = api::router(app_state(store));
    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/node")
                .header("authorization", format!("Bearer {secret}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = String::from_utf8(
        hyper::body::to_bytes(response.into_body())
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();

    // The keys are there, current first, retired marked.
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    let listed = json["status"]["signingKeys"]
        .as_array()
        .expect("signingKeys");
    assert_eq!(listed.len(), 2, "{body}");
    assert_eq!(listed[0]["keyId"], current_id);
    assert_eq!(listed[0]["publicKey"], current_public);
    assert_eq!(listed[0]["state"], "current");
    assert_eq!(listed[1]["keyId"], retired_id);
    assert_eq!(listed[1]["state"], "retired");
    for k in listed {
        let mut names: Vec<&str> = k.as_object().unwrap().keys().map(String::as_str).collect();
        names.sort_unstable();
        assert_eq!(names, ["keyId", "publicKey", "state"], "only public fields");
        assert_eq!(k["publicKey"].as_str().unwrap().len(), 64);
    }

    // No secret appears anywhere in the response.
    for (what, secret_hex) in [
        ("node seed", seed.trim()),
        ("session master", master.trim()),
        ("retired seed", old_seed.trim()),
        ("derived session seed", derived_session_seed.as_str()),
    ] {
        assert!(
            !body.contains(secret_hex),
            "{what} leaked into GET /v1/node"
        );
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
