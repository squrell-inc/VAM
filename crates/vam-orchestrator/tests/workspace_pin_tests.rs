//! Workspace / CreateRun pins agent URN + catalog version.
//!
//! Draft pins load the owner draft (not latest released). Unpublished released
//! `1.0.0` stays 404. Members cannot pin a foreign draft.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;
use vam_core::{CidAllocator, MockVmm};
use vam_orchestrator::api::auth::ControlPlanePdp;
use vam_orchestrator::api::{self, AppState};
use vam_orchestrator::keys::{mint_api_key_secret, MEMBER_SCOPES, PUBLISHER_SCOPES};
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

async fn put_agent_as(
    app: &axum::Router,
    secret: &str,
    urn: &str,
    instructions: &str,
) -> StatusCode {
    let body = serde_json::json!({
        "spec": {
            "resourceId": {"urn": urn},
            "displayName": "Pinned agent",
            "description": "workspace pin test",
            "role": "Tester",
            "systemInstructions": instructions,
            "allowAnyPlaybook": true
        }
    });
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/v1/agents")
                .header("authorization", format!("Bearer {secret}"))
                .header("Idempotency-Key", format!("put-{urn}-{instructions}"))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let _ = response_body(resp).await;
    status
}

async fn release_agent_as(app: &axum::Router, secret: &str, urn: &str, idem: &str) -> StatusCode {
    let body = serde_json::json!({
        "version": {"major": 1, "minor": 0, "patch": 0}
    });
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/v1/agents/{urn}/release"))
                .header("authorization", format!("Bearer {secret}"))
                .header("Idempotency-Key", idem)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let _ = response_body(resp).await;
    status
}

fn catalog_run_body(
    agent_urn: &str,
    workspace: &str,
    idem: &str,
    run_urn: &str,
    prerelease: &str,
) -> serde_json::Value {
    serde_json::json!({
        "spec": {
            "runId": {"urn": run_urn},
            "agentRef": {
                "resourceId": {"urn": agent_urn},
                "version": {
                    "major": 1,
                    "minor": 0,
                    "patch": 0,
                    "prerelease": prerelease
                }
            },
            "inputs": {"fields": {"prompt": {"stringValue": "hello pin"}}},
            "workspaceRefUri": format!("volume://{workspace}"),
            "idempotencyKey": idem
        }
    })
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
async fn publisher_draft_pin_create_run_is_accepted() {
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let publisher = mint_user_key(&store, "pub", UserRole::Publisher, PUBLISHER_SCOPES);
    let (state, _dir) = app_state_http_only(store);
    let app = test_app(state);
    let urn = "urn:vam:agent:local.vam.dev/acme/draft-pin";
    assert_eq!(
        put_agent_as(&app, &publisher, urn, "owner-draft").await,
        StatusCode::OK
    );

    let (status, body) = create_run_as(
        &app,
        &publisher,
        catalog_run_body(
            urn,
            "draft-ws",
            "draft-pin-idem",
            "urn:vam:run:local.vam.dev/acme/draft-pin",
            "draft",
        ),
        "draft-pin-idem",
    )
    .await;
    assert!(
        status.is_success(),
        "draft pin must not 404: {status} {body}"
    );
    assert!(
        !body.contains("1.0.0 not found"),
        "draft pin must not look up released 1.0.0: {body}"
    );
}

#[tokio::test]
async fn unpublished_released_pin_is_404() {
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let publisher = mint_user_key(&store, "pub", UserRole::Publisher, PUBLISHER_SCOPES);
    let (state, _dir) = app_state_http_only(store);
    let app = test_app(state);
    let urn = "urn:vam:agent:local.vam.dev/acme/missing-release";
    assert_eq!(
        put_agent_as(&app, &publisher, urn, "draft-only").await,
        StatusCode::OK
    );

    let (status, body) = create_run_as(
        &app,
        &publisher,
        catalog_run_body(
            urn,
            "missing-rel-ws",
            "missing-rel-idem",
            "urn:vam:run:local.vam.dev/acme/missing-rel",
            "",
        ),
        "missing-rel-idem",
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert!(
        body.contains("1.0.0 not found"),
        "unpublished released pin: {body}"
    );
}

#[tokio::test]
async fn released_pin_create_run_is_accepted() {
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let publisher = mint_user_key(&store, "pub", UserRole::Publisher, PUBLISHER_SCOPES);
    let (state, _dir) = app_state_http_only(store);
    let app = test_app(state);
    let urn = "urn:vam:agent:local.vam.dev/acme/released-pin";
    assert_eq!(
        put_agent_as(&app, &publisher, urn, "will-release").await,
        StatusCode::OK
    );
    assert_eq!(
        release_agent_as(&app, &publisher, urn, "rel-pin").await,
        StatusCode::OK
    );

    let (status, body) = create_run_as(
        &app,
        &publisher,
        catalog_run_body(
            urn,
            "released-ws",
            "released-pin-idem",
            "urn:vam:run:local.vam.dev/acme/released-pin",
            "",
        ),
        "released-pin-idem",
    )
    .await;
    assert!(
        status.is_success(),
        "released pin must load 1.0.0: {status} {body}"
    );
}

#[tokio::test]
async fn member_cannot_pin_foreign_draft() {
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let publisher = mint_user_key(&store, "pub", UserRole::Publisher, PUBLISHER_SCOPES);
    let member = mint_user_key(&store, "member", UserRole::Member, MEMBER_SCOPES);
    let (state, _dir) = app_state_http_only(store);
    let app = test_app(state);
    let urn = "urn:vam:agent:local.vam.dev/acme/foreign-draft-pin";
    assert_eq!(
        put_agent_as(&app, &publisher, urn, "publisher-draft").await,
        StatusCode::OK
    );

    let (status, body) = create_run_as(
        &app,
        &member,
        catalog_run_body(
            urn,
            "member-draft-ws",
            "member-draft-idem",
            "urn:vam:run:local.vam.dev/acme/member-draft",
            "draft",
        ),
        "member-draft-idem",
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert!(
        body.contains(&format!("no draft of agent '{urn}' for this user")),
        "member draft pin: {body}"
    );
}
