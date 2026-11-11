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
use vam_orchestrator::users::LOCAL_USER_ID;
use vam_orchestrator::workflow::SqliteWorkflowStore;
use vam_orchestrator::{VmmFactory, DEFAULT_REFRESH_TTL_SECS};

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

fn store_with_password(secret: &str) -> Arc<SqliteWorkflowStore> {
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    store.reset_user_password(LOCAL_USER_ID, secret).unwrap();
    store
}

fn store_with_password_and_api_key(password: &str) -> (Arc<SqliteWorkflowStore>, String) {
    let store = store_with_password(password);
    let api_key = mint_api_key_secret();
    let scopes: Vec<String> = BOOTSTRAP_SCOPES.iter().map(|s| s.to_string()).collect();
    store
        .create_api_key("test-key", "test", &api_key, "User::\"tester\"", &scopes)
        .unwrap();
    (store, api_key)
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

async fn get_node(app: axum::Router, bearer: &str) -> (StatusCode, String) {
    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/node")
                .header("authorization", format!("Bearer {bearer}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    (status, response_body(response).await)
}

#[tokio::test]
async fn login_good_password_returns_token() {
    let store = store_with_password("correct-http-login");
    let app = test_app(app_state_http_only(store));
    let (status, body) = post_json(
        app,
        "/v1/login",
        r#"{"username":"local","password":"correct-http-login"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    let token = json["token"].as_str().expect("token");
    assert!(token.starts_with("vam_st_"), "{token}");
    assert!(json["refreshToken"]
        .as_str()
        .unwrap_or("")
        .starts_with("vam_rt_"));
    assert_eq!(json["username"].as_str(), Some("local"));
    assert!(json["expiresAt"].as_i64().unwrap_or(0) > 0);
}

#[tokio::test]
async fn login_bad_password_is_401() {
    let store = store_with_password("correct-http-login");
    let app = test_app(app_state_http_only(store));
    let (status, body) = post_json(
        app,
        "/v1/login",
        r#"{"username":"local","password":"wrong-password"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    assert!(body.contains("UNAUTHENTICATED") || body.contains("Unauthenticated"));
}

#[tokio::test]
async fn login_unknown_user_is_401() {
    let store = store_with_password("correct-http-login");
    let app = test_app(app_state_http_only(store));
    let (status, body) = post_json(
        app,
        "/v1/login",
        r#"{"username":"nobody","password":"correct-http-login"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
}

#[tokio::test]
async fn login_session_token_authorizes_node() {
    let store = store_with_password("session-node");
    let app = test_app(app_state_http_only(store));
    let (status, body) = post_json(
        app.clone(),
        "/v1/login",
        r#"{"username":"local","password":"session-node"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let token = serde_json::from_str::<serde_json::Value>(&body).unwrap()["token"]
        .as_str()
        .unwrap()
        .to_string();
    let (node_status, node_body) = get_node(app, &token).await;
    assert_eq!(node_status, StatusCode::OK, "{node_body}");
}

#[tokio::test]
async fn login_token_expires_is_401() {
    let store = store_with_password("expire-http");
    let user = store.get_user(LOCAL_USER_ID).unwrap();
    let issued = store
        .issue_session(&user, -1, DEFAULT_REFRESH_TTL_SECS)
        .unwrap();
    let app = test_app(app_state_http_only(store));
    let (status, body) = get_node(app, &issued.token).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
}

#[tokio::test]
async fn login_api_key_still_authorizes() {
    let (store, api_key) = store_with_password_and_api_key("both-auth");
    let app = test_app(app_state_http_only(store));
    let (login_status, login_body) = post_json(
        app.clone(),
        "/v1/login",
        r#"{"username":"local","password":"both-auth"}"#,
    )
    .await;
    assert_eq!(login_status, StatusCode::OK, "{login_body}");
    let (status, body) = get_node(app, &api_key).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn login_refresh_issues_new_token() {
    let store = store_with_password("refresh-http");
    let app = test_app(app_state_http_only(store));
    let (status, body) = post_json(
        app.clone(),
        "/v1/login",
        r#"{"username":"local","password":"refresh-http"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let first: serde_json::Value = serde_json::from_str(&body).unwrap();
    let refresh = first["refreshToken"].as_str().unwrap();
    let old_token = first["token"].as_str().unwrap().to_string();
    let (refresh_status, refresh_body) = post_json(
        app.clone(),
        "/v1/login/refresh",
        &format!(r#"{{"refreshToken":"{refresh}"}}"#),
    )
    .await;
    assert_eq!(refresh_status, StatusCode::OK, "{refresh_body}");
    let second: serde_json::Value = serde_json::from_str(&refresh_body).unwrap();
    let new_token = second["token"].as_str().unwrap();
    assert_ne!(old_token, new_token);
    let (old_status, _) = get_node(app.clone(), &old_token).await;
    assert_eq!(old_status, StatusCode::UNAUTHORIZED);
    let (new_status, new_body) = get_node(app, new_token).await;
    assert_eq!(new_status, StatusCode::OK, "{new_body}");
}

#[tokio::test]
async fn login_logout_revokes_token() {
    let store = store_with_password("logout-http");
    let app = test_app(app_state_http_only(store));
    let (status, body) = post_json(
        app.clone(),
        "/v1/login",
        r#"{"username":"local","password":"logout-http"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let token = serde_json::from_str::<serde_json::Value>(&body).unwrap()["token"]
        .as_str()
        .unwrap()
        .to_string();
    let logout = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/logout")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    let logout_status = logout.status();
    let logout_body = response_body(logout).await;
    assert_eq!(logout_status, StatusCode::OK, "{logout_body}");
    assert!(logout_body.contains("\"revoked\":true") || logout_body.contains("\"revoked\": true"));
    let (after, after_body) = get_node(app, &token).await;
    assert_eq!(after, StatusCode::UNAUTHORIZED, "{after_body}");
}
