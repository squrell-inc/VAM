//! `GET /v1/runs/{id}/receipt`: stored receipts are served, never issued by the route.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;
use vam_core::{CidAllocator, MockVmm};
use vam_orchestrator::api::auth::ControlPlanePdp;
use vam_orchestrator::api::{self, AppState};
use vam_orchestrator::keys::{mint_api_key_secret, BOOTSTRAP_SCOPES, MEMBER_SCOPES};
use vam_orchestrator::manager::SessionManager;
use vam_orchestrator::secrets::SecretsVault;
use vam_orchestrator::workflow::SqliteWorkflowStore;
use vam_orchestrator::VmmFactory;
use vam_spec::v1::ResourceId;

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

async fn response_body(response: axum::response::Response) -> String {
    String::from_utf8(
        hyper::body::to_bytes(response.into_body())
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap()
}

fn encode_run_path(run_id: &str) -> String {
    run_id
        .chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => c.to_string(),
            _ => format!("%{:02X}", c as u32),
        })
        .collect()
}

fn member_secret(store: &SqliteWorkflowStore, user_id: &str) -> String {
    store.create_user(user_id, user_id).unwrap();
    let scopes: Vec<String> = MEMBER_SCOPES.iter().map(|s| (*s).to_string()).collect();
    let secret = mint_api_key_secret();
    store
        .create_api_key_owned(
            &format!("{user_id}-key"),
            user_id,
            &secret,
            &format!("User::\"{user_id}\""),
            &scopes,
            user_id,
        )
        .unwrap();
    secret
}

fn get(run_id: &str, secret: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(format!("/v1/runs/{}/receipt", encode_run_path(run_id)))
        .header("authorization", format!("Bearer {secret}"))
        .body(Body::empty())
        .unwrap()
}

async fn fetch(
    store: &Arc<SqliteWorkflowStore>,
    secret: &str,
    run_id: &str,
) -> (StatusCode, String) {
    let app = api::router(app_state_http_only(store.clone()));
    let response = app.oneshot(get(run_id, secret)).await.unwrap();
    let status = response.status();
    (status, response_body(response).await)
}

fn install_keys() {
    let _ = vam_orchestrator::signing::install(vam_orchestrator::signing::NodeKeys::ephemeral());
}

fn make_run(store: &SqliteWorkflowStore, run_id: &str) {
    store
        .create_run(&ResourceId {
            urn: run_id.to_string(),
        })
        .unwrap();
}

#[tokio::test]
async fn a_live_run_answers_409_and_a_finished_run_without_one_says_not_yet_issued() {
    install_keys();
    let (store, secret) = bootstrap_store();
    let run_id = "urn:vam:run:local.vam.dev/acme/rcpt-live";
    make_run(&store, run_id);
    let (status, body) = fetch(&store, &secret, run_id).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body.contains("issued when it ends"), "{body}");

    store
        .complete_run_with_result(run_id, "SUCCEEDED", "{}")
        .unwrap();
    let (status, body) = fetch(&store, &secret, run_id).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body.contains("not yet issued"), "{body}");
    // The route never issues: an early GET would store an incomplete receipt.
    assert!(store.run_receipt(run_id).unwrap().is_none());
}

#[tokio::test]
async fn an_issued_receipt_is_served_byte_for_byte_and_verifies() {
    install_keys();
    let (store, secret) = bootstrap_store();
    let run_id = "urn:vam:run:local.vam.dev/acme/rcpt-done";
    make_run(&store, run_id);
    store
        .complete_run_with_result(run_id, "SUCCEEDED", r#"{"answer":"hello"}"#)
        .unwrap();
    vam_orchestrator::receipt::issue_receipt(&store, run_id, Some("tdigest")).unwrap();
    let (status, first) = fetch(&store, &secret, run_id).await;
    assert_eq!(status, StatusCode::OK, "{first}");
    let (_, second) = fetch(&store, &secret, run_id).await;
    assert_eq!(first, second);
    assert_eq!(first, store.run_receipt(run_id).unwrap().unwrap().document);
    vam_security::receipt::verify_receipt(first.as_bytes(), &[]).expect("verifies offline");
    let doc: serde_json::Value = serde_json::from_str(&first).unwrap();
    assert_eq!(doc["modelAppendix"]["finalReply"], "hello");
    // A run created after this node first started with the effects section is journaled;
    // with no outside call it says so with zeros, never the old "not journaled" line.
    let effects = &doc["receipt"]["effects"];
    assert_eq!(effects["journaled"], true);
    assert!(effects.get("statement").is_none());
    assert_eq!(effects["duplicates"], 0);
    assert_eq!(effects["unknown"], 0);
    assert_eq!(effects["lines"], serde_json::json!([]));
}

#[tokio::test]
async fn another_users_run_and_an_unknown_run_are_404() {
    install_keys();
    let (store, _bootstrap) = bootstrap_store();
    let run_id = "urn:vam:run:local.vam.dev/acme/rcpt-foreign";
    make_run(&store, run_id);
    store
        .complete_run_with_result(run_id, "SUCCEEDED", "{}")
        .unwrap();
    vam_orchestrator::receipt::issue_receipt(&store, run_id, None).unwrap();
    let other = member_secret(&store, "mallory");
    let (status, _) = fetch(&store, &other, run_id).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = fetch(&store, &other, "urn:vam:run:local.vam.dev/acme/none").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
