//! API keys authorize by the live role (member-draft-agents Phase 1).
//!
//! Mint routes store `scope_mode` (`role` for an empty scope list, `explicit` otherwise),
//! authentication replaces the stored scopes with the effective ones, `GET /v1/me` reports
//! them, and release needs `publish:grant` in them.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;
use vam_core::{CidAllocator, MockVmm};
use vam_orchestrator::api::auth::ControlPlanePdp;
use vam_orchestrator::api::{self, AppState};
use vam_orchestrator::keys::{
    mint_api_key_secret, ScopeMode, BOOTSTRAP_SCOPES, MEMBER_SCOPES, PUBLISHER_SCOPES,
};
use vam_orchestrator::manager::SessionManager;
use vam_orchestrator::secrets::SecretsVault;
use vam_orchestrator::users::{UserRole, OPERATOR_USER_ID};
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

/// A store with the bootstrap key (operator, stored scopes) and its secret.
fn node() -> (Arc<SqliteWorkflowStore>, axum::Router, String) {
    let store = Arc::new(SqliteWorkflowStore::in_memory().expect("store"));
    let secret = mint_api_key_secret();
    let scopes: Vec<String> = BOOTSTRAP_SCOPES.iter().map(|s| (*s).to_string()).collect();
    store
        .create_api_key_owned(
            "bootstrap",
            "bootstrap",
            &secret,
            "User::\"bootstrap\"",
            &scopes,
            OPERATOR_USER_ID,
        )
        .expect("bootstrap key");
    let app = api::router(app_state(store.clone()));
    (store, app, secret)
}

async fn call(
    app: &axum::Router,
    method: &str,
    uri: &str,
    secret: &str,
    body: Option<&str>,
) -> (StatusCode, String) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header("authorization", format!("Bearer {secret}"))
                .header("Idempotency-Key", format!("idem-{}", mint_api_key_secret()))
                .header("content-type", "application/json")
                .body(Body::from(body.unwrap_or("").to_string()))
                .expect("request"),
        )
        .await
        .expect("response");
    let status = response.status();
    let bytes = hyper::body::to_bytes(response.into_body())
        .await
        .expect("body");
    (status, String::from_utf8_lossy(&bytes).to_string())
}

fn json(body: &str) -> serde_json::Value {
    serde_json::from_str(body).unwrap_or_else(|e| panic!("json {e}: {body}"))
}

/// Mints through `POST /v1/users/{id}/keys`; returns (key id, secret).
async fn mint_user_key(
    app: &axum::Router,
    admin: &str,
    user: &str,
    name: &str,
    scopes: &[&str],
) -> (String, String) {
    let body = serde_json::json!({ "name": name, "scopes": scopes }).to_string();
    let (status, resp) = call(
        app,
        "POST",
        &format!("/v1/users/{user}/keys"),
        admin,
        Some(&body),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{resp}");
    let v = json(&resp);
    let key_id = key_id_of(&resp);
    (key_id, v["secret"].as_str().expect("secret").to_string())
}

fn key_id_of(resp: &str) -> String {
    json(resp)["key"]["keyId"]["urn"]
        .as_str()
        .unwrap_or_else(|| panic!("key id in {resp}"))
        .to_string()
}

async fn me_scopes(app: &axum::Router, secret: &str) -> Vec<String> {
    let (status, body) = call(app, "GET", "/v1/me", secret, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    json(&body)["scopes"]
        .as_array()
        .expect("scopes")
        .iter()
        .filter_map(|s| s.as_str().map(str::to_string))
        .collect()
}

fn has(scopes: &[String], scope: &str) -> bool {
    scopes.iter().any(|s| s == scope)
}

#[tokio::test]
async fn mint_routes_store_role_for_an_empty_list_and_explicit_for_a_list() {
    let (store, app, boot) = node();
    store.create_user("alice", "alice").expect("alice");

    let (id_default, _) = mint_user_key(&app, &boot, "alice", "default", &[]).await;
    let (id_list, _) = mint_user_key(&app, &boot, "alice", "list", &["runs:write"]).await;
    assert_eq!(
        store.get_api_key(&id_default).expect("row").scope_mode,
        ScopeMode::Role
    );
    assert_eq!(
        store.get_api_key(&id_list).expect("row").scope_mode,
        ScopeMode::Explicit
    );

    // `POST /v1/keys` (keys:admin) with `userId`: same rule.
    for (name, scopes, want) in [
        ("k-empty", serde_json::json!([]), ScopeMode::Role),
        (
            "k-list",
            serde_json::json!(["node:read"]),
            ScopeMode::Explicit,
        ),
    ] {
        let body = serde_json::json!({"name": name, "scopes": scopes, "userId": "alice"});
        let (status, resp) = call(&app, "POST", "/v1/keys", &boot, Some(&body.to_string())).await;
        assert_eq!(status, StatusCode::OK, "{resp}");
        let id = key_id_of(&resp);
        assert_eq!(
            store.get_api_key(&id).expect("row").scope_mode,
            want,
            "{name}"
        );
    }
}

#[tokio::test]
async fn me_reports_effective_scopes_and_listing_keeps_the_stored_list() {
    let (store, app, boot) = node();
    store.create_user("alice", "alice").expect("alice");
    let (_, default_key) = mint_user_key(&app, &boot, "alice", "default", &[]).await;
    let (_, narrow_key) = mint_user_key(&app, &boot, "alice", "narrow", &["runs:write"]).await;
    let (above_id, above_key) = mint_user_key(
        &app,
        &boot,
        "alice",
        "above",
        &["runs:write", "catalog:write"],
    )
    .await;

    assert!(!has(&me_scopes(&app, &default_key).await, "catalog:write"));
    assert_eq!(me_scopes(&app, &narrow_key).await, ["runs:write"]);
    assert_eq!(me_scopes(&app, &above_key).await, ["runs:write"]);
    // The row keeps what was asked for.
    assert!(has(
        &store.get_api_key(&above_id).expect("row").scopes,
        "catalog:write"
    ));

    store
        .set_user_role("alice", UserRole::Publisher)
        .expect("promote");
    let promoted = me_scopes(&app, &default_key).await;
    assert!(has(&promoted, "catalog:write") && has(&promoted, "publish:grant"));
    assert_eq!(me_scopes(&app, &narrow_key).await, ["runs:write"]);
    assert_eq!(
        me_scopes(&app, &above_key).await,
        ["runs:write", "catalog:write"]
    );

    store
        .set_user_role("alice", UserRole::Member)
        .expect("demote");
    assert!(!has(&me_scopes(&app, &default_key).await, "catalog:write"));
    assert_eq!(me_scopes(&app, &above_key).await, ["runs:write"]);
}

#[tokio::test]
async fn bootstrap_key_keeps_its_stored_scopes() {
    let (_store, app, boot) = node();
    let scopes = me_scopes(&app, &boot).await;
    assert!(has(&scopes, "keys:admin") && has(&scopes, "brokers:admin"));
    let (status, body) = call(&app, "GET", "/v1/users", &boot, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

const RELEASE_BODY: &str = r#"{"version":{"major":1,"minor":0,"patch":0}}"#;
const RELEASE_URL: &str = "/v1/playbooks/urn:vam:playbook:local.vam.dev%2Facme%2Fnothing/release";

#[tokio::test]
async fn release_needs_publish_grant_in_the_effective_scopes() {
    let (store, app, boot) = node();
    store
        .create_user_with_role("pat", "pat", UserRole::Publisher)
        .expect("pat");
    let (_, default_key) = mint_user_key(&app, &boot, "pat", "default", &[]).await;
    let (_, narrow_key) = mint_user_key(
        &app,
        &boot,
        "pat",
        "narrow",
        &["catalog:write", "runs:write", "sessions:write"],
    )
    .await;
    assert!(PUBLISHER_SCOPES.contains(&"publish:grant"));

    // Narrowed without publish:grant: refused at the grant, before the catalog is read.
    let (status, body) = call(&app, "POST", RELEASE_URL, &narrow_key, Some(RELEASE_BODY)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(body.contains("publish:grant"), "{body}");

    // A default publisher key passes the grant (the playbook does not exist: not a 403).
    let (status, body) = call(&app, "POST", RELEASE_URL, &default_key, Some(RELEASE_BODY)).await;
    assert_ne!(status, StatusCode::FORBIDDEN, "{body}");

    // Demoted: the same default key loses it (it is refused at catalog:write first).
    store
        .set_user_role("pat", UserRole::Member)
        .expect("demote");
    let (status, body) = call(&app, "POST", RELEASE_URL, &default_key, Some(RELEASE_BODY)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    // The bootstrap key (keys:admin) keeps the operator bypass.
    let (status, body) = call(&app, "POST", RELEASE_URL, &boot, Some(RELEASE_BODY)).await;
    assert_ne!(status, StatusCode::FORBIDDEN, "{body}");
    let _ = MEMBER_SCOPES;
}
