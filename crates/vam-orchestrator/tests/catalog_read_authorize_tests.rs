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
        axum::body::to_bytes(response.into_body(), usize::MAX)
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

/// Spec change (member-draft-agents Phase 3, decisions 6 and 9): a member holds `agents:draft`,
/// so `PUT /v1/agents` of their own draft in their own namespace is 200; outside it is 400.
#[tokio::test]
async fn member_put_own_agent_draft_is_200_outside_namespace_is_400() {
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let member = mint_user_key(&store, "member", UserRole::Member, MEMBER_SCOPES);
    let app = test_app(app_state_http_only(store));
    let own = "urn:vam:agent:local.vam.dev/member/member-write";
    assert_eq!(
        put_agent_as(&app, &member, own, "member-draft").await,
        StatusCode::OK
    );
    let outside = "urn:vam:agent:local.vam.dev/acme/member-write";
    assert_eq!(
        put_agent_as(&app, &member, outside, "member-draft").await,
        StatusCode::BAD_REQUEST
    );
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

/// Seeds `urn:vam:skill:local.vam.dev/acme/<name>` as `owner`'s draft, released at 1.0.0 when `release`.
fn seed_skill(store: &SqliteWorkflowStore, name: &str, owner: &str, release: bool) -> String {
    let urn = format!("urn:vam:skill:local.vam.dev/acme/{name}");
    let version = vam_spec::v1::Version {
        major: 1,
        ..Default::default()
    };
    let manifest = vam_spec::v1::SkillManifest {
        resource_id: Some(vam_spec::v1::ResourceId { urn: urn.clone() }),
        version: Some(version.clone()),
        description: "Test".to_string(),
        ..Default::default()
    };
    store
        .publish_skill_draft_with_owner(&manifest, owner)
        .expect("skill draft");
    if release {
        store.release_skill(&urn, &version).expect("release skill");
    }
    urn
}

async fn get_as(app: &axum::Router, uri: &str, secret: &str) -> (StatusCode, serde_json::Value) {
    let response = app.clone().oneshot(bearer_get(uri, secret)).await.unwrap();
    let status = response.status();
    let body = response_body(response).await;
    (
        status,
        serde_json::from_str(&body).unwrap_or(serde_json::Value::String(body)),
    )
}

fn listed_urns(body: &serde_json::Value) -> Vec<(String, String)> {
    body["records"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .map(|r| {
                    (
                        r["spec"]["resourceId"]["urn"]
                            .as_str()
                            .unwrap_or("")
                            .to_string(),
                        r["channel"].as_str().unwrap_or("").to_string(),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

#[tokio::test]
async fn skills_list_is_released_only_and_drafts_need_catalog_write() {
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let publisher = mint_user_key(&store, "pub", UserRole::Publisher, PUBLISHER_SCOPES);
    let other = mint_user_key(&store, "pub2", UserRole::Publisher, PUBLISHER_SCOPES);
    let member = mint_user_key(&store, "member", UserRole::Member, MEMBER_SCOPES);
    let released = seed_skill(&store, "released", "pub", true);
    let mine = seed_skill(&store, "pub-wip", "pub", false);
    let theirs = seed_skill(&store, "pub2-wip", "pub2", false);
    let app = test_app(app_state_http_only(store));

    for secret in [&member, &publisher, &other] {
        let (status, body) = get_as(&app, "/v1/skills", secret).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            listed_urns(&body),
            [(released.clone(), "RELEASED".to_string())],
            "default is released only: {body}"
        );
    }

    let (status, body) = get_as(&app, "/v1/skills?includeDrafts=true", &member).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(
        body.to_string()
            .contains("missing required scope 'catalog:write'"),
        "{body}"
    );

    let (status, body) = get_as(&app, "/v1/skills?includeDrafts=true", &publisher).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let urns = listed_urns(&body);
    assert!(
        urns.contains(&(mine.clone(), "DRAFT".to_string())),
        "{body}"
    );
    assert!(urns.iter().all(|(u, _)| u != &theirs), "{body}");

    for bad in ["yes", "", "%20%20"] {
        let (status, body) =
            get_as(&app, &format!("/v1/skills?includeDrafts={bad}"), &publisher).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "includeDrafts={bad:?}: {body}"
        );
        assert!(body.to_string().contains("includeDrafts"), "{body}");
    }
}

#[tokio::test]
async fn skills_list_pages_with_a_token_and_refuses_a_bad_page_size() {
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let member = mint_user_key(&store, "member", UserRole::Member, MEMBER_SCOPES);
    let _ = mint_user_key(&store, "pub", UserRole::Publisher, PUBLISHER_SCOPES);
    let urns: Vec<String> = (0..3)
        .map(|i| seed_skill(&store, &format!("s{i}"), "pub", true))
        .collect();
    let app = test_app(app_state_http_only(store));

    let (status, first) = get_as(&app, "/v1/skills?pageSize=2", &member).await;
    assert_eq!(status, StatusCode::OK, "{first}");
    let token = first["pagination"]["nextPageToken"]
        .as_str()
        .expect("a next page")
        .to_string();
    assert!(!token.is_empty(), "{first}");
    let (status, second) = get_as(
        &app,
        &format!("/v1/skills?pageSize=2&pageToken={token}"),
        &member,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{second}");
    let seen: Vec<String> = listed_urns(&first)
        .into_iter()
        .chain(listed_urns(&second))
        .map(|(u, _)| u)
        .collect();
    assert_eq!(
        seen,
        [urns[2].clone(), urns[1].clone(), urns[0].clone()],
        "newest first, no repeat, no gap"
    );
    assert_eq!(
        second["pagination"]["nextPageToken"].as_str().unwrap_or(""),
        "",
        "last page has no token: {second}"
    );

    let (status, body) = get_as(&app, "/v1/skills?pageSize=-1", &member).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let runs_token = "djEKcnVucwox"; // base64url of "v1\nruns\n1": another list's token
    let (status, body) = get_as(&app, &format!("/v1/skills?pageToken={runs_token}"), &member).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}
