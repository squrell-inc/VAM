//! Operator allowed-models HTTP + CreateRun enforcement.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;
use vam_core::{CidAllocator, MockVmm};
use vam_orchestrator::api::auth::ControlPlanePdp;
use vam_orchestrator::api::{self, AppState};
use vam_orchestrator::keys::{
    mint_api_key_secret, BROKERS_ADMIN_SCOPE, MEMBER_SCOPES, ORG_ADMIN_SCOPES,
};
use vam_orchestrator::manager::SessionManager;
use vam_orchestrator::secrets::SecretsVault;
use vam_orchestrator::users::UserRole;
use vam_orchestrator::workflow::SqliteWorkflowStore;
use vam_orchestrator::VmmFactory;

fn test_app(state: AppState) -> axum::Router {
    api::router(state)
}

fn app_state_http_only(store: Arc<SqliteWorkflowStore>) -> (AppState, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("drive9");
    std::env::set_var("VAM_DRIVE9_ROOT", dir.path());
    let vault = SecretsVault::for_tests(store.clone());
    let cid_allocator = Arc::new(CidAllocator::default());
    let vmm_factory: VmmFactory = Arc::new(|_vm_id, _guest_cid| Box::new(MockVmm::new()));
    let (manager, _cmd_rx) = SessionManager::new(store.clone(), cid_allocator, vmm_factory);
    (
        AppState {
            store,
            vault,
            manager,
            pdp: Arc::new(ControlPlanePdp::new().expect("pdp")),
            listen_addr: "127.0.0.1:8080".to_string(),
            draining: Arc::new(AtomicBool::new(false)),
            ready: Arc::new(AtomicBool::new(true)),
            e2e_mock_scripts: false,
        },
        dir,
    )
}

fn mint_user_key(
    store: &SqliteWorkflowStore,
    user_id: &str,
    role: UserRole,
    scopes: &[&str],
) -> String {
    let _ = store.create_user_with_role(user_id, user_id, role);
    let secret = mint_api_key_secret();
    let scopes: Vec<String> = scopes.iter().map(|s| (*s).to_string()).collect();
    store
        .create_api_key_owned(
            &format!("key-{user_id}"),
            user_id,
            &secret,
            &format!("User::\"{user_id}\""),
            &scopes,
            user_id,
        )
        .expect("mint user key");
    secret
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

fn create_run_body(run_id: &str, idem: &str, model: Option<&str>) -> serde_json::Value {
    let mut inputs = serde_json::json!({
        "fields": {
            "prompt": {"stringValue": "hello from allowlist test"}
        }
    });
    if let Some(model) = model {
        inputs["fields"]["model"] = serde_json::json!({"stringValue": model});
    }
    serde_json::json!({
        "spec": {
            "runId": {"urn": run_id},
            "agentRef": {
                "resourceId": {"urn": "urn:vam:agent:local.vam.dev/acme/test-agent"},
                "version": {"major": 1, "minor": 0, "patch": 0},
                "digest": {"algorithm": "ALGORITHM_SHA256", "hexHash": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"}
            },
            "inputs": inputs,
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

async fn put_allowed_models(
    app: &axum::Router,
    secret: &str,
    models: &[&str],
    idem: &str,
) -> (StatusCode, String) {
    let body = serde_json::json!({ "models": models });
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/v1/node/allowed-models")
                .header("authorization", format!("Bearer {secret}"))
                .header("Idempotency-Key", idem)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let body = response_body(resp).await;
    (status, body)
}

async fn get_allowed_models(app: &axum::Router, secret: &str) -> (StatusCode, String) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/node/allowed-models")
                .header("authorization", format!("Bearer {secret}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let body = response_body(resp).await;
    (status, body)
}

async fn create_run_as(
    app: &axum::Router,
    secret: &str,
    body: serde_json::Value,
    idem: &str,
) -> (StatusCode, String) {
    let resp = app
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
        .unwrap();
    let status = resp.status();
    let body = response_body(resp).await;
    (status, body)
}

#[tokio::test]
async fn member_can_get_allowed_models() {
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let operator = mint_user_key(
        &store,
        "operator",
        UserRole::Operator,
        &[BROKERS_ADMIN_SCOPE],
    );
    let member = mint_user_key(&store, "member", UserRole::Member, MEMBER_SCOPES);
    let (state, _dir) = app_state_http_only(store);
    let app = test_app(state);

    let (status, body) = put_allowed_models(
        &app,
        &operator,
        &["openai/gpt-4o", "anthropic/claude-sonnet-4"],
        "put-models-member-get",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, body) = get_allowed_models(&app, &member).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("openai/gpt-4o"), "{body}");
    assert!(body.contains("anthropic/claude-sonnet-4"), "{body}");
}

#[tokio::test]
async fn org_admin_cannot_put_allowed_models() {
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let admin = mint_user_key(&store, "admin", UserRole::OrgAdmin, ORG_ADMIN_SCOPES);
    let (state, _dir) = app_state_http_only(store);
    let app = test_app(state);

    let (status, body) =
        put_allowed_models(&app, &admin, &["openai/gpt-4o"], "put-models-admin").await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(
        body.contains("PERMISSION_DENIED") || body.contains("brokers:admin"),
        "{body}"
    );
}

#[tokio::test]
async fn create_run_rejects_model_off_allowlist() {
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let operator = mint_user_key(
        &store,
        "operator",
        UserRole::Operator,
        &[BROKERS_ADMIN_SCOPE, "runs:write"],
    );
    let (state, _dir) = app_state_http_only(store);
    let app = test_app(state);

    let (status, body) =
        put_allowed_models(&app, &operator, &["openai/gpt-4o"], "put-models-miss").await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, body) = create_run_as(
        &app,
        &operator,
        create_run_body(
            "urn:vam:run:local.vam.dev/acme/run-off-list",
            "run-off-list",
            Some("openai/gpt-4o-mini"),
        ),
        "run-off-list",
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body.contains("INVALID_ARGUMENT") || body.contains("allowlist"),
        "{body}"
    );
}

#[tokio::test]
async fn create_run_records_listed_model() {
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let operator = mint_user_key(
        &store,
        "operator",
        UserRole::Operator,
        &[BROKERS_ADMIN_SCOPE, "runs:write"],
    );
    let (state, _dir) = app_state_http_only(store.clone());
    let app = test_app(state);

    let (status, body) = put_allowed_models(
        &app,
        &operator,
        &["openai/gpt-4o", "anthropic/claude-sonnet-4"],
        "put-models-hit",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let run_id = "urn:vam:run:local.vam.dev/acme/run-on-list";
    let (status, body) = create_run_as(
        &app,
        &operator,
        create_run_body(run_id, "run-on-list", Some("openai/gpt-4o")),
        "run-on-list",
    )
    .await;
    assert!(status.is_success(), "{status} {body}");

    let (spec_json, _, _) = store.get_run_spec_context(run_id).expect("run spec");
    let spec_json = spec_json.expect("spec_json");
    assert!(
        spec_json.contains("openai/gpt-4o"),
        "listed model must be recorded on the run: {spec_json}"
    );
    let spec: vam_spec::v1::RunSpec =
        vam_spec::encoding::json_to_proto("vam.v1.RunSpec", &spec_json).expect("parse spec");
    assert_eq!(
        vam_orchestrator::run_context::extract_model(&spec),
        "openai/gpt-4o"
    );
}
