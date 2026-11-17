//! `GET /v1/me` identity read.
//!
//! Any authenticated caller (password session or API key) receives
//! `userId`, `username`, `role`, and the bearer's live scopes. Members
//! are not 403. Deactivated users and revoked keys are 401.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;
use vam_core::{CidAllocator, MockVmm};
use vam_orchestrator::api::auth::ControlPlanePdp;
use vam_orchestrator::api::{self, AppState};
use vam_orchestrator::keys::{
    mint_api_key_secret, BOOTSTRAP_SCOPES, MEMBER_SCOPES, ORG_ADMIN_SCOPES, PUBLISHER_SCOPES,
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

async fn get_me(app: axum::Router, bearer: Option<&str>) -> (StatusCode, String) {
    let mut builder = Request::builder().method("GET").uri("/v1/me");
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

fn assert_identity(
    body: &str,
    user_id: &str,
    username: &str,
    role: &str,
    expected_scopes: &[&str],
) {
    let json: serde_json::Value = serde_json::from_str(body).unwrap_or_else(|_| {
        panic!("expected JSON identity body, got {body}");
    });
    assert_eq!(json["userId"].as_str(), Some(user_id), "{body}");
    assert_eq!(json["username"].as_str(), Some(username), "{body}");
    assert_eq!(json["role"].as_str(), Some(role), "{body}");
    // Unconfigured node: the default registry domain, no marketplace domain, and the
    // username lowercased as the namespace (the operator id is not a slug).
    assert_eq!(
        json["registryDomain"].as_str(),
        Some("local.vam.dev"),
        "{body}"
    );
    // proto3 JSON omits an empty string, so an unset marketplace is absent.
    assert!(json.get("marketplaceDomain").is_none(), "{body}");
    let namespace = json["defaultNamespace"].as_str().expect("defaultNamespace");
    assert_eq!(namespace, vam_spec::default_namespace(username), "{body}");
    let scopes = json["scopes"]
        .as_array()
        .unwrap_or_else(|| panic!("scopes array: {body}"));
    let got: Vec<&str> = scopes.iter().filter_map(|v| v.as_str()).collect();
    for scope in expected_scopes {
        assert!(
            got.iter().any(|s| s == scope),
            "missing scope {scope} in {got:?} ({body})"
        );
    }
    assert_eq!(
        got.len(),
        expected_scopes.len(),
        "scope set mismatch: got {got:?} expected {expected_scopes:?} ({body})"
    );
}

/// Member, publisher, org admin, and operator: password session and
/// role-default API key all return identity. Members are not 403.
#[tokio::test]
async fn get_me_returns_identity_for_password_session_and_api_key() {
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    store
        .create_managed_user("alice", "alice", "alice-secret", UserRole::Member)
        .expect("alice");
    store
        .create_managed_user("pub", "pub", "pub-secret", UserRole::Publisher)
        .expect("publisher");
    store
        .create_managed_user("admin", "admin", "admin-secret", UserRole::OrgAdmin)
        .expect("admin");
    store
        .reset_user_password(OPERATOR_USER_ID, "operator-secret")
        .expect("operator password");

    let alice_key = mint_owned_key(&store, "alice", MEMBER_SCOPES);
    let pub_key = mint_owned_key(&store, "pub", PUBLISHER_SCOPES);
    let admin_key = mint_owned_key(&store, "admin", ORG_ADMIN_SCOPES);
    let operator_key = mint_owned_key(&store, OPERATOR_USER_ID, BOOTSTRAP_SCOPES);

    let app = test_app(app_state_http_only(store));

    let alice_session = login_token(app.clone(), "alice", "alice-secret").await;
    let pub_session = login_token(app.clone(), "pub", "pub-secret").await;
    let admin_session = login_token(app.clone(), "admin", "admin-secret").await;
    let operator_session = login_token(app.clone(), OPERATOR_USERNAME, "operator-secret").await;

    #[allow(clippy::type_complexity)] // table of fixed string tuples; a named type adds nothing
    let cases: [(&str, &str, &str, &str, &str, &[&str]); 8] = [
        (
            "member session",
            alice_session.as_str(),
            "alice",
            "alice",
            "member",
            MEMBER_SCOPES,
        ),
        (
            "member api key",
            alice_key.as_str(),
            "alice",
            "alice",
            "member",
            MEMBER_SCOPES,
        ),
        (
            "publisher session",
            pub_session.as_str(),
            "pub",
            "pub",
            "publisher",
            PUBLISHER_SCOPES,
        ),
        (
            "publisher api key",
            pub_key.as_str(),
            "pub",
            "pub",
            "publisher",
            PUBLISHER_SCOPES,
        ),
        (
            "org_admin session",
            admin_session.as_str(),
            "admin",
            "admin",
            "org_admin",
            ORG_ADMIN_SCOPES,
        ),
        (
            "org_admin api key",
            admin_key.as_str(),
            "admin",
            "admin",
            "org_admin",
            ORG_ADMIN_SCOPES,
        ),
        (
            "operator session",
            operator_session.as_str(),
            OPERATOR_USER_ID,
            OPERATOR_USERNAME,
            "operator",
            BOOTSTRAP_SCOPES,
        ),
        (
            "operator api key",
            operator_key.as_str(),
            OPERATOR_USER_ID,
            OPERATOR_USERNAME,
            "operator",
            BOOTSTRAP_SCOPES,
        ),
    ];
    for (label, bearer, user_id, username, role, scopes) in cases {
        let (status, body) = get_me(app.clone(), Some(bearer)).await;
        assert_eq!(status, StatusCode::OK, "{label}: {body}");
        assert_identity(&body, user_id, username, role, scopes);
        assert!(
            !body.contains("UNAUTHENTICATED"),
            "{label}: unexpected error body {body}"
        );
    }
}

#[tokio::test]
async fn get_me_member_is_not_403() {
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    store
        .create_managed_user("alice", "alice", "alice-secret", UserRole::Member)
        .expect("alice");
    let app = test_app(app_state_http_only(store));
    let token = login_token(app.clone(), "alice", "alice-secret").await;
    let (status, body) = get_me(app, Some(&token)).await;
    assert_ne!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_identity(&body, "alice", "alice", "member", MEMBER_SCOPES);
}

/// A key minted with a subset of scopes reports those scopes, not the role default.
#[tokio::test]
async fn get_me_reports_bearer_scopes_not_role_defaults() {
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    store
        .create_managed_user("alice", "alice", "alice-secret", UserRole::Member)
        .expect("alice");
    let secret = mint_owned_key(&store, "alice", &["sessions:write"]);
    let app = test_app(app_state_http_only(store));
    let (status, body) = get_me(app, Some(&secret)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_identity(&body, "alice", "alice", "member", &["sessions:write"]);
    assert!(
        !body.contains("users:admin") && !body.contains("brokers:admin"),
        "{body}"
    );
}

#[tokio::test]
async fn get_me_unauthenticated_is_401() {
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let app = test_app(app_state_http_only(store));
    let (status, body) = get_me(app, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
}

#[tokio::test]
async fn get_me_deactivated_user_is_401() {
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    store
        .create_managed_user("alice", "alice", "alice-secret", UserRole::Member)
        .expect("alice");
    let secret = mint_owned_key(&store, "alice", MEMBER_SCOPES);
    let app = test_app(app_state_http_only(store.clone()));
    let token = login_token(app.clone(), "alice", "alice-secret").await;

    store.deactivate_user("alice").expect("deactivate");

    let (session_status, session_body) = get_me(app.clone(), Some(&token)).await;
    assert_eq!(session_status, StatusCode::UNAUTHORIZED, "{session_body}");
    assert!(
        session_body.contains("UNAUTHENTICATED") || session_body.contains("Unauthenticated"),
        "{session_body}"
    );

    let (key_status, key_body) = get_me(app, Some(&secret)).await;
    assert_eq!(key_status, StatusCode::UNAUTHORIZED, "{key_body}");
    assert!(
        key_body.contains("UNAUTHENTICATED") || key_body.contains("Unauthenticated"),
        "{key_body}"
    );
}

#[tokio::test]
async fn get_me_revoked_api_key_is_401() {
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    store
        .create_managed_user("alice", "alice", "alice-secret", UserRole::Member)
        .expect("alice");
    let secret = mint_owned_key(&store, "alice", MEMBER_SCOPES);
    store.revoke_api_key("key-alice").expect("revoke");
    let app = test_app(app_state_http_only(store));
    let (status, body) = get_me(app, Some(&secret)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    assert!(
        body.contains("UNAUTHENTICATED") || body.contains("Unauthenticated"),
        "{body}"
    );
}

/// An email username keeps its typed case in `username`; the default namespace is the part
/// before the `@`, without the `+tag`, lowercased. Login works in any letter case.
#[tokio::test]
async fn get_me_namespace_for_email_username() {
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let user = store
        .create_managed_user_generated(
            "Ada.Lovelace+work@Example.com",
            "ada-secret",
            UserRole::Member,
        )
        .expect("ada");
    let app = test_app(app_state_http_only(store));
    let session = login_token(app.clone(), "ADA.LOVELACE+WORK@EXAMPLE.COM", "ada-secret").await;
    let (status, body) = get_me(app, Some(&session)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        json["userId"].as_str(),
        Some(user.user_id.as_str()),
        "{body}"
    );
    assert_eq!(
        json["username"].as_str(),
        Some("Ada.Lovelace+work@Example.com"),
        "{body}"
    );
    assert_eq!(
        json["defaultNamespace"].as_str(),
        Some("ada.lovelace"),
        "{body}"
    );
}
