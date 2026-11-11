//! `GET /v1/node` login-probe matrix.
//!
//! Role defaults include `node:read` for member, org_admin, and operator.
//! Password sessions and default-scope API keys therefore return 200 so
//! The VAM app can keep probing `/v1/health` then `/v1/node` after sign-in.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;
use vam_core::{CidAllocator, MockVmm};
use vam_orchestrator::api::auth::ControlPlanePdp;
use vam_orchestrator::api::{self, AppState};
use vam_orchestrator::keys::{
    mint_api_key_secret, BOOTSTRAP_SCOPES, MEMBER_SCOPES, ORG_ADMIN_SCOPES,
};
use vam_orchestrator::manager::SessionManager;
use vam_orchestrator::secrets::SecretsVault;
use vam_orchestrator::users::{UserRole, OPERATOR_USERNAME, OPERATOR_USER_ID};
use vam_orchestrator::workflow::SqliteWorkflowStore;
use vam_orchestrator::VmmFactory;

fn test_app(state: AppState) -> axum::Router {
    api::router(state)
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

fn mint_owned_key(store: &SqliteWorkflowStore, user_id: &str, scopes: &[&str]) -> String {
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
        .expect("mint owned key");
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

async fn post_json(app: axum::Router, path: &str, body: &str) -> (StatusCode, String) {
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(path)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    (status, response_body(response).await)
}

async fn get_node(app: axum::Router, bearer: Option<&str>) -> (StatusCode, String) {
    let mut builder = Request::builder().method("GET").uri("/v1/node");
    if let Some(token) = bearer {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    let response = app
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    (status, response_body(response).await)
}

async fn login_token(app: axum::Router, username: &str, password: &str) -> String {
    let (status, body) = post_json(
        app,
        "/v1/login",
        &format!(r#"{{"username":"{username}","password":"{password}"}}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    serde_json::from_str::<serde_json::Value>(&body).unwrap()["token"]
        .as_str()
        .expect("token")
        .to_string()
}

/// Member / org admin / operator: password session and role-default API key
/// all return 200 on `GET /v1/node`.
#[tokio::test]
async fn get_node_login_probe_is_200_for_member_org_admin_and_operator() {
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    store
        .create_managed_user("alice", "alice", "alice-secret", UserRole::Member)
        .expect("alice");
    store
        .create_managed_user("admin", "admin", "admin-secret", UserRole::OrgAdmin)
        .expect("admin");
    store
        .reset_user_password(OPERATOR_USER_ID, "operator-secret")
        .expect("operator password");

    let alice_key = mint_owned_key(&store, "alice", MEMBER_SCOPES);
    let admin_key = mint_owned_key(&store, "admin", ORG_ADMIN_SCOPES);
    let operator_key = mint_owned_key(&store, OPERATOR_USER_ID, BOOTSTRAP_SCOPES);

    let app = test_app(app_state_http_only(store));

    let alice_session = login_token(app.clone(), "alice", "alice-secret").await;
    let admin_session = login_token(app.clone(), "admin", "admin-secret").await;
    let operator_session = login_token(app.clone(), OPERATOR_USERNAME, "operator-secret").await;

    let cases = [
        ("member session", alice_session.as_str()),
        ("member api key", alice_key.as_str()),
        ("org_admin session", admin_session.as_str()),
        ("org_admin api key", admin_key.as_str()),
        ("operator session", operator_session.as_str()),
        ("operator api key", operator_key.as_str()),
    ];
    for (label, bearer) in cases {
        let (status, body) = get_node(app.clone(), Some(bearer)).await;
        assert_eq!(status, StatusCode::OK, "{label}: {body}");
        assert!(
            body.contains("\"listenAddr\"") || body.contains("\"listen_addr\""),
            "{label}: expected node snapshot, got {body}"
        );
    }
}

#[tokio::test]
async fn get_node_unauthenticated_is_401() {
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let app = test_app(app_state_http_only(store));
    let (status, body) = get_node(app, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
}

/// Scope is still required: a member key minted without `node:read` is 403.
#[tokio::test]
async fn get_node_member_api_key_without_node_read_is_403() {
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    store
        .create_managed_user("alice", "alice", "alice-secret", UserRole::Member)
        .expect("alice");
    let secret = mint_owned_key(&store, "alice", &["sessions:write"]);
    let app = test_app(app_state_http_only(store));
    let (status, body) = get_node(app, Some(&secret)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
}
