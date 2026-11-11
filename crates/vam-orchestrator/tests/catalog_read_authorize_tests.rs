//! Catalog GET/list without `catalog:write`.
//!
//! Members with run rights (`runs:write`) list/get released catalog. Foreign
//! drafts stay 404. PUT/release remain `catalog:write` / `publish:grant`.

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

fn bearer_get(uri: &str, secret: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(uri)
        .header("authorization", format!("Bearer {secret}"))
        .body(Body::empty())
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

async fn put_agent_as(
    app: &axum::Router,
    secret: &str,
    urn: &str,
    instructions: &str,
) -> StatusCode {
    let body = serde_json::json!({
        "spec": {
            "resourceId": {"urn": urn},
            "displayName": "Owned agent",
            "description": "catalog read authorize test",
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

async fn put_policy_as(app: &axum::Router, secret: &str, urn: &str, idem: &str) -> StatusCode {
    let body = serde_json::json!({
        "urn": urn,
        "cedar": "permit(principal, action, resource);"
    });
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/v1/policies")
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

async fn release_policy_as(app: &axum::Router, secret: &str, urn: &str, idem: &str) -> StatusCode {
    let body = serde_json::json!({
        "version": {"major": 1, "minor": 0, "patch": 0}
    });
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/v1/policies/{urn}/release"))
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

#[tokio::test]
async fn publisher_put_agent_is_200() {
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let publisher = mint_user_key(&store, "pub", UserRole::Publisher, PUBLISHER_SCOPES);
    let app = test_app(app_state_http_only(store));
    let urn = "urn:vam:agent:local.vam.dev/acme/pub-write";
    assert_eq!(
        put_agent_as(&app, &publisher, urn, "publisher-draft").await,
        StatusCode::OK
    );
}

#[tokio::test]
async fn member_put_agent_is_403() {
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let member = mint_user_key(&store, "member", UserRole::Member, MEMBER_SCOPES);
    let app = test_app(app_state_http_only(store));
    let urn = "urn:vam:agent:local.vam.dev/acme/member-write";
    let status = put_agent_as(&app, &member, urn, "member-draft").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn member_put_policy_is_403() {
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let member = mint_user_key(&store, "member", UserRole::Member, MEMBER_SCOPES);
    let app = test_app(app_state_http_only(store));
    let urn = "urn:vam:policy:local.vam.dev/acme/member-write";
    assert_eq!(
        put_policy_as(&app, &member, urn, "member-policy").await,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn member_get_agents_returns_released_only() {
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let publisher = mint_user_key(&store, "pub", UserRole::Publisher, PUBLISHER_SCOPES);
    let member = mint_user_key(&store, "member", UserRole::Member, MEMBER_SCOPES);
    let app = test_app(app_state_http_only(store));
    let released_urn = "urn:vam:agent:local.vam.dev/acme/released";
    let draft_urn = "urn:vam:agent:local.vam.dev/acme/still-draft";
    assert_eq!(
        put_agent_as(&app, &publisher, released_urn, "will-release").await,
        StatusCode::OK
    );
    assert_eq!(
        put_agent_as(&app, &publisher, draft_urn, "stays-draft").await,
        StatusCode::OK
    );
    assert_eq!(
        release_agent_as(&app, &publisher, released_urn, "rel-released").await,
        StatusCode::OK
    );

    let list = app
        .clone()
        .oneshot(bearer_get("/v1/agents", &member))
        .await
        .unwrap();
    assert_eq!(list.status(), StatusCode::OK);
    let list_body = response_body(list).await;
    assert!(
        list_body.contains(released_urn),
        "member must see released: {list_body}"
    );
    assert!(
        !list_body.contains(draft_urn),
        "member must not see foreign draft: {list_body}"
    );
    assert!(
        !list_body.contains("catalog:write"),
        "must not 403 missing catalog:write: {list_body}"
    );

    let get_released = app
        .clone()
        .oneshot(bearer_get(&format!("/v1/agents/{released_urn}"), &member))
        .await
        .unwrap();
    assert_eq!(get_released.status(), StatusCode::OK);
}

#[tokio::test]
async fn member_get_foreign_draft_is_404() {
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let publisher = mint_user_key(&store, "pub", UserRole::Publisher, PUBLISHER_SCOPES);
    let member = mint_user_key(&store, "member", UserRole::Member, MEMBER_SCOPES);
    let app = test_app(app_state_http_only(store));
    let draft_urn = "urn:vam:agent:local.vam.dev/acme/foreign-draft";
    assert_eq!(
        put_agent_as(&app, &publisher, draft_urn, "foreign-draft").await,
        StatusCode::OK
    );

    let get_draft = app
        .oneshot(bearer_get(&format!("/v1/agents/{draft_urn}"), &member))
        .await
        .unwrap();
    assert_eq!(get_draft.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn member_get_policies_and_skill_packages_is_200() {
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let publisher = mint_user_key(&store, "pub", UserRole::Publisher, PUBLISHER_SCOPES);
    let member = mint_user_key(&store, "member", UserRole::Member, MEMBER_SCOPES);
    let app = test_app(app_state_http_only(store));
    let policy_urn = "urn:vam:policy:local.vam.dev/acme/released-policy";
    assert_eq!(
        put_policy_as(&app, &publisher, policy_urn, "pub-policy").await,
        StatusCode::OK
    );
    assert_eq!(
        release_policy_as(&app, &publisher, policy_urn, "rel-policy").await,
        StatusCode::OK
    );

    let policies = app
        .clone()
        .oneshot(bearer_get("/v1/policies", &member))
        .await
        .unwrap();
    assert_eq!(policies.status(), StatusCode::OK);
    let policies_body = response_body(policies).await;
    assert!(
        policies_body.contains(policy_urn),
        "member must see released policy: {policies_body}"
    );

    let packages = app
        .oneshot(bearer_get("/v1/skills/packages", &member))
        .await
        .unwrap();
    assert_eq!(packages.status(), StatusCode::OK);
}
