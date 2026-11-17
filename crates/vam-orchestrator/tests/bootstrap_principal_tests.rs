//! `bootstrap` is reserved to the deployment bootstrap key.
//!
//! A principal string, a key name or a username never makes a caller the operator:
//! only the key whose id is `bootstrap` does. Covers the user-identity hotfix UI-2.5.

use std::path::PathBuf;
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
use vam_orchestrator::users::{UserRole, OPERATOR_USER_ID};
use vam_orchestrator::workflow::SqliteWorkflowStore;
use vam_orchestrator::{VmmFactory, BOOTSTRAP_KEY_ID, BOOTSTRAP_PRINCIPAL};

fn temp_db(label: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!(
        "vam-bootstrap-principal-{label}-{}-{nanos}.db",
        std::process::id()
    ))
}

fn cleanup(path: &PathBuf) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(format!("{}-wal", path.display()));
    let _ = std::fs::remove_file(format!("{}-shm", path.display()));
}

fn app_for(store: Arc<SqliteWorkflowStore>) -> axum::Router {
    if std::env::var("VAM_DRIVE9_ROOT").is_err() {
        std::env::set_var("VAM_DRIVE9_ROOT", "/tmp/vam-test-drive9");
    }
    let vault = SecretsVault::for_tests(store.clone());
    let cid_allocator = Arc::new(CidAllocator::default());
    let vmm_factory: VmmFactory = Arc::new(|_vm_id, _guest_cid| Box::new(MockVmm::new()));
    let (manager, _cmd_rx) = SessionManager::new(store.clone(), cid_allocator, vmm_factory);
    api::router(AppState {
        store,
        vault,
        manager,
        pdp: Arc::new(ControlPlanePdp::new().expect("pdp")),
        listen_addr: "127.0.0.1:8080".to_string(),
        draining: Arc::new(AtomicBool::new(false)),
        ready: Arc::new(AtomicBool::new(true)),
        e2e_mock_scripts: false,
    })
}

fn scopes_of(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| (*s).to_string()).collect()
}

/// The genuine bootstrap key, minted the way first start mints it.
fn seed_bootstrap_key(store: &SqliteWorkflowStore) -> String {
    let secret = mint_api_key_secret();
    store
        .create_api_key(
            BOOTSTRAP_KEY_ID,
            "bootstrap",
            &secret,
            BOOTSTRAP_PRINCIPAL,
            &scopes_of(BOOTSTRAP_SCOPES),
        )
        .expect("bootstrap key");
    secret
}

async fn call(
    app: &axum::Router,
    method: &str,
    path: &str,
    bearer: &str,
    body: Option<&str>,
) -> (StatusCode, String) {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header("authorization", format!("Bearer {bearer}"))
        .header("content-type", "application/json");
    if method == "POST" {
        builder = builder.header("Idempotency-Key", format!("idem-{}", mint_api_key_secret()));
    }
    let response = app
        .clone()
        .oneshot(
            builder
                .body(Body::from(body.unwrap_or("").to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = hyper::body::to_bytes(response.into_body()).await.unwrap();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

async fn me(app: &axum::Router, bearer: &str) -> serde_json::Value {
    let (status, body) = call(app, "GET", "/v1/me", bearer, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    serde_json::from_str(&body).unwrap()
}

async fn login(app: &axum::Router, username: &str, password: &str) -> String {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/login")
                .header("content-type", "application/json")
                .body(Body::from(format!(
                    r#"{{"username":"{username}","password":"{password}"}}"#
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = hyper::body::to_bytes(response.into_body()).await.unwrap();
    let body = String::from_utf8(bytes.to_vec()).unwrap();
    assert_eq!(status, StatusCode::OK, "{body}");
    serde_json::from_str::<serde_json::Value>(&body).unwrap()["token"]
        .as_str()
        .expect("token")
        .to_string()
}

#[tokio::test]
async fn real_bootstrap_key_is_still_the_operator() {
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let secret = seed_bootstrap_key(&store);
    let app = app_for(store);
    let identity = me(&app, &secret).await;
    assert_eq!(identity["userId"].as_str(), Some(OPERATOR_USER_ID));
}

#[tokio::test]
async fn session_of_a_legacy_user_named_bootstrap_owns_its_own_rows() {
    let path = temp_db("legacy-user");
    let store = Arc::new(SqliteWorkflowStore::open(&path).unwrap());
    let bootstrap_secret = seed_bootstrap_key(&store);
    {
        // A row from before the name was reserved: the API cannot create one any more.
        let connection = rusqlite::Connection::open(&path).unwrap();
        connection
            .execute(
                "INSERT INTO users (user_id, username, tenant_id, created_at, role)
                 VALUES ('bootstrap', 'bootstrap', 'bootstrap', 1, 'member')",
                [],
            )
            .unwrap();
    }
    store
        .reset_user_password("bootstrap", "legacy-secret-123")
        .unwrap();
    let app = app_for(store);
    let token = login(&app, "bootstrap", "legacy-secret-123").await;

    let identity = me(&app, &token).await;
    assert_eq!(identity["userId"].as_str(), Some("bootstrap"), "{identity}");
    assert_eq!(identity["role"].as_str(), Some("member"), "{identity}");

    // Not an operator: no users:admin, and the operator's key is not in its key list.
    let (status, _) = call(&app, "GET", "/v1/users", &token, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, keys) = call(&app, "GET", "/v1/keys", &token, None).await;
    assert_eq!(status, StatusCode::OK, "{keys}");
    assert!(!keys.contains(BOOTSTRAP_KEY_ID), "{keys}");

    // The real bootstrap key is still the operator next to it.
    assert_eq!(
        me(&app, &bootstrap_secret).await["userId"].as_str(),
        Some(OPERATOR_USER_ID)
    );
    cleanup(&path);
}

#[tokio::test]
async fn minting_a_key_named_or_principaled_bootstrap_is_400() {
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let secret = seed_bootstrap_key(&store);
    store
        .create_managed_user("alice", "alice", "alice-secret-123", UserRole::Member)
        .unwrap();
    store
        .create_managed_user("admin1", "admin1", "admin-secret-1234", UserRole::OrgAdmin)
        .unwrap();
    let admin_secret = mint_api_key_secret();
    store
        .create_api_key_owned(
            "key-admin1",
            "admin1",
            &admin_secret,
            "User::\"admin1\"",
            &scopes_of(ORG_ADMIN_SCOPES),
            "admin1",
        )
        .unwrap();
    let app = app_for(store);
    let scope = r#""scopes":["sessions:write"]"#;

    let cases = [
        format!(r#"{{"name":"bootstrap",{scope}}}"#),
        format!(r#"{{"name":"Bootstrap",{scope}}}"#),
        format!(r#"{{"name":"ok","principal":"User::\"bootstrap\"",{scope}}}"#),
    ];
    for body in &cases {
        // Admin path: an org admin mints for a member.
        let (status, text) = call(
            &app,
            "POST",
            "/v1/users/alice/keys",
            &admin_secret,
            Some(body),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}: {text}");
        assert!(text.contains("reserved"), "{text}");
        // Key path: the key admin mints for a member.
        let with_user = body.replacen('{', r#"{"userId":"alice","#, 1);
        let (status, text) = call(&app, "POST", "/v1/keys", &secret, Some(&with_user)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{with_user}: {text}");
        assert!(text.contains("reserved"), "{text}");
    }

    // A normal mint still works on both paths and the key stays the member's.
    let (status, text) = call(
        &app,
        "POST",
        "/v1/users/alice/keys",
        &admin_secret,
        Some(&format!(r#"{{"name":"laptop",{scope}}}"#)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let minted = serde_json::from_str::<serde_json::Value>(&text).unwrap()["secret"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(me(&app, &minted).await["userId"].as_str(), Some("alice"));
}

#[tokio::test]
async fn creating_a_user_named_bootstrap_is_400() {
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let secret = seed_bootstrap_key(&store);
    let app = app_for(store);
    for username in ["bootstrap", "Bootstrap", "BOOTSTRAP"] {
        let (status, text) = call(
            &app,
            "POST",
            "/v1/users",
            &secret,
            Some(&format!(
                r#"{{"username":"{username}","password":"some-secret-1234"}}"#
            )),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{username}: {text}");
        assert!(text.contains("reserved"), "{text}");
    }
    // An explicit id cannot smuggle the name in either.
    let (status, text) = call(
        &app,
        "POST",
        "/v1/users",
        &secret,
        Some(r#"{"username":"carol","userId":"bootstrap","password":"some-secret-1234"}"#),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{text}");
    let (status, text) = call(
        &app,
        "POST",
        "/v1/users",
        &secret,
        Some(r#"{"username":"carol","password":"some-secret-1234"}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{text}");
}

#[tokio::test]
async fn a_member_key_with_the_bootstrap_principal_stays_the_member_after_restart() {
    let path = temp_db("restart");
    let member_secret = mint_api_key_secret();
    let bootstrap_secret;
    {
        let store = SqliteWorkflowStore::open(&path).unwrap();
        bootstrap_secret = seed_bootstrap_key(&store);
        store
            .create_managed_user("alice", "alice", "alice-secret-123", UserRole::Member)
            .unwrap();
        // The shape the old mint paths could produce: a member's key carrying the
        // bootstrap principal.
        store
            .create_api_key_owned(
                "key-alice-boot",
                "alice laptop",
                &member_secret,
                BOOTSTRAP_PRINCIPAL,
                &scopes_of(MEMBER_SCOPES),
                "alice",
            )
            .unwrap();
        let app = app_for(Arc::new(store));
        assert_eq!(
            me(&app, &member_secret).await["userId"].as_str(),
            Some("alice")
        );
    }

    // Restart: the store opens again and the owner backfill runs again.
    let store = Arc::new(SqliteWorkflowStore::open(&path).unwrap());
    let keys = store.list_api_keys().unwrap();
    let owner_of = |id: &str| {
        keys.iter()
            .find(|k| k.key_id == id)
            .map(|k| k.owner_user_id.clone())
            .unwrap()
    };
    assert_eq!(owner_of("key-alice-boot"), "alice");
    assert_eq!(owner_of(BOOTSTRAP_KEY_ID), OPERATOR_USER_ID);

    let app = app_for(store);
    let identity = me(&app, &member_secret).await;
    assert_eq!(identity["userId"].as_str(), Some("alice"), "{identity}");
    assert_eq!(identity["role"].as_str(), Some("member"), "{identity}");
    let (status, _) = call(&app, "GET", "/v1/users", &member_secret, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(
        me(&app, &bootstrap_secret).await["userId"].as_str(),
        Some(OPERATOR_USER_ID)
    );
    cleanup(&path);
}
