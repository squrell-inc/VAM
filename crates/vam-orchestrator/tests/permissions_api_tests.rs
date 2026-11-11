//! AX-5.6a: draft permission defaults on publish, and the owner's
//! GET/PUT of that draft.
//!
//! A minimal agent (no `permissions`, no legacy policy) is stored with
//! `tools.default = bySideEffect` and `internet.default = allow`, through the
//! same `PUT /v1/agents` the app and `vam agent publish` share. A legacy draft
//! is stored unchanged and `PUT .../permissions` is 409. A member writing the
//! admin's draft gets 404.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;
use vam_core::{CidAllocator, MockVmm};
use vam_orchestrator::api::auth::ControlPlanePdp;
use vam_orchestrator::api::{self, AppState};
use vam_orchestrator::keys::{mint_api_key_secret, ORG_ADMIN_SCOPES};
use vam_orchestrator::manager::SessionManager;
use vam_orchestrator::secrets::SecretsVault;
use vam_orchestrator::users::UserRole;
use vam_orchestrator::workflow::SqliteWorkflowStore;
use vam_orchestrator::VmmFactory;

fn test_app(state: AppState) -> axum::Router {
    api::router(state)
}

fn app_state(store: Arc<SqliteWorkflowStore>) -> AppState {
    if std::env::var("VAM_DRIVE9_ROOT").is_err() {
        std::env::set_var("VAM_DRIVE9_ROOT", "/tmp/vam-test-drive9");
    }
    let vault = SecretsVault::for_tests(store.clone());
    let vmm_factory: VmmFactory = Arc::new(|_vm_id, _guest_cid| Box::new(MockVmm::new()));
    let (manager, _cmd_rx) = SessionManager::new(
        store.clone(),
        Arc::new(CidAllocator::default()),
        vmm_factory,
    );
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

async fn response_body(response: axum::response::Response) -> String {
    String::from_utf8(
        hyper::body::to_bytes(response.into_body())
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap()
}

fn agent_body(urn: &str, instructions: &str, extra: Value) -> Value {
    let mut spec = json!({
        "resourceId": {"urn": urn},
        "displayName": "Owned agent",
        "description": "permissions api test",
        "role": "Tester",
        "systemInstructions": instructions,
        "allowAnyPlaybook": true
    });
    if let (Some(spec), Some(extra)) = (spec.as_object_mut(), extra.as_object()) {
        for (key, value) in extra {
            spec.insert(key.clone(), value.clone());
        }
    }
    json!({ "spec": spec })
}

async fn send(
    app: &axum::Router,
    method: &str,
    uri: &str,
    secret: &str,
    idem: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {secret}"));
    if let Some(idem) = idem {
        builder = builder.header("Idempotency-Key", idem);
    }
    let request = if let Some(body) = body {
        builder
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    } else {
        builder.body(Body::empty()).unwrap()
    };
    let response = app.clone().oneshot(request).await.expect("response");
    let status = response.status();
    let text = response_body(response).await;
    let json = if text.is_empty() {
        Value::Null
    } else {
        serde_json::from_str(&text).unwrap_or_else(|_| json!({ "raw": text }))
    };
    (status, json)
}

fn assert_defaults(permissions: &Value) {
    assert_eq!(
        permissions["tools"]["default"], "bySideEffect",
        "{permissions}"
    );
    assert_eq!(permissions["internet"]["default"], "allow", "{permissions}");
}

#[tokio::test]
async fn publish_and_permissions_api_store_defaults_for_a_minimal_agent() {
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let admin = mint_user_key(&store, "admin", UserRole::OrgAdmin, ORG_ADMIN_SCOPES);
    let app = test_app(app_state(store));
    let urn = "urn:vam:agent:local.vam.dev/acme/minimal";

    let (status, published) = send(
        &app,
        "PUT",
        "/v1/agents",
        &admin,
        Some("publish-minimal"),
        Some(agent_body(urn, "no permissions", json!({}))),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{published}");
    assert_defaults(&published["spec"]["permissions"]);

    let (status, got) = send(
        &app,
        "GET",
        &format!("/v1/agents/{urn}/permissions"),
        &admin,
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{got}");
    assert_defaults(&got);
}

#[tokio::test]
async fn put_replaces_draft_permissions_and_rejects_a_bad_rule() {
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let admin = mint_user_key(
        &store,
        "admin",
        UserRole::Publisher,
        &["catalog:write", "runs:write"],
    );
    let app = test_app(app_state(store));
    let urn = "urn:vam:agent:local.vam.dev/acme/editable";
    let (status, _) = send(
        &app,
        "PUT",
        "/v1/agents",
        &admin,
        Some("publish-editable"),
        Some(agent_body(urn, "start", json!({}))),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let replacement = json!({
        "tools": {
            "default": "deny",
            "rules": [{ "target": "builtin:run", "decision": "deny" }]
        },
        "internet": {
            "default": "deny",
            "rules": [{ "target": "*.pastebin.com", "decision": "deny" }]
        }
    });
    let (status, saved) = send(
        &app,
        "PUT",
        &format!("/v1/agents/{urn}/permissions"),
        &admin,
        Some("replace-permissions"),
        Some(replacement),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(saved["tools"]["default"], "deny", "{saved}");
    assert_eq!(
        saved["tools"]["rules"][0]["target"], "builtin:run",
        "{saved}"
    );
    assert_eq!(saved["internet"]["default"], "deny", "{saved}");
    assert_eq!(
        saved["internet"]["rules"][0]["target"], "*.pastebin.com",
        "{saved}"
    );

    let (status, got) = send(
        &app,
        "GET",
        &format!("/v1/agents/{urn}/permissions"),
        &admin,
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{got}");
    assert_eq!(got["tools"]["default"], "deny", "{got}");

    let (status, err) = send(
        &app,
        "PUT",
        &format!("/v1/agents/{urn}/permissions"),
        &admin,
        Some("bad-rule"),
        Some(json!({
            "tools": {
                "default": "allow",
                "rules": [{ "target": "skill:nope", "decision": "allow" }]
            }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{err}");
    let message = err["message"].as_str().unwrap_or("");
    assert!(
        message.contains("skill:nope"),
        "AX-5.1 validation message: {message}"
    );

    let (status, still) = send(
        &app,
        "GET",
        &format!("/v1/agents/{urn}/permissions"),
        &admin,
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{still}");
    assert_eq!(still["tools"]["default"], "deny", "{still}");
}

#[tokio::test]
async fn legacy_draft_is_stored_unchanged_and_put_says_migrate_first() {
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let admin = mint_user_key(
        &store,
        "admin",
        UserRole::Publisher,
        &["catalog:write", "runs:write"],
    );
    let app = test_app(app_state(store));
    let urn = "urn:vam:agent:local.vam.dev/acme/legacy";

    let (status, published) = send(
        &app,
        "PUT",
        "/v1/agents",
        &admin,
        Some("publish-legacy"),
        Some(agent_body(
            urn,
            "legacy",
            json!({
                "capabilityPolicy": { "securityProfileName": "restricted-egress" }
            }),
        )),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{published}");
    assert!(
        published["spec"].get("permissions").is_none(),
        "{published}"
    );
    assert_eq!(
        published["spec"]["capabilityPolicy"]["securityProfileName"], "restricted-egress",
        "{published}"
    );

    let (status, err) = send(
        &app,
        "PUT",
        &format!("/v1/agents/{urn}/permissions"),
        &admin,
        Some("put-legacy"),
        Some(json!({
            "tools": { "default": "bySideEffect" },
            "internet": { "default": "allow" }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{err}");
    let message = err["message"].as_str().unwrap_or("");
    assert!(message.contains("migrate first"), "{message}");

    let (status, agent) = send(
        &app,
        "GET",
        &format!("/v1/agents/{urn}"),
        &admin,
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{agent}");
    assert!(agent["spec"].get("permissions").is_none(), "{agent}");
    assert_eq!(
        agent["spec"]["capabilityPolicy"]["securityProfileName"], "restricted-egress",
        "{agent}"
    );
}

#[tokio::test]
async fn member_cannot_read_or_write_the_admins_draft() {
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let admin = mint_user_key(&store, "admin", UserRole::OrgAdmin, ORG_ADMIN_SCOPES);
    // A member who can call the catalog. The 404 is the draft check, not a missing scope.
    let member = mint_user_key(
        &store,
        "member",
        UserRole::Member,
        &["catalog:write", "runs:write"],
    );
    let app = test_app(app_state(store));
    let urn = "urn:vam:agent:local.vam.dev/acme/admin-only";

    let (status, _) = send(
        &app,
        "PUT",
        "/v1/agents",
        &admin,
        Some("publish-admin"),
        Some(agent_body(urn, "private", json!({}))),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, get_err) = send(
        &app,
        "GET",
        &format!("/v1/agents/{urn}/permissions"),
        &member,
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{get_err}");

    let (status, put_err) = send(
        &app,
        "PUT",
        &format!("/v1/agents/{urn}/permissions"),
        &member,
        Some("member-put"),
        Some(json!({
            "tools": { "default": "allow" },
            "internet": { "default": "allow" }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{put_err}");

    let (status, still) = send(
        &app,
        "GET",
        &format!("/v1/agents/{urn}/permissions"),
        &admin,
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{still}");
    assert_defaults(&still);
}

#[tokio::test]
async fn org_admin_can_read_a_foreign_draft_but_cannot_write_it() {
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let owner = mint_user_key(
        &store,
        "owner",
        UserRole::Publisher,
        &["catalog:write", "runs:write"],
    );
    let admin = mint_user_key(&store, "admin", UserRole::OrgAdmin, ORG_ADMIN_SCOPES);
    let app = test_app(app_state(store));
    let urn = "urn:vam:agent:local.vam.dev/acme/owner-draft";

    let (status, _) = send(
        &app,
        "PUT",
        "/v1/agents",
        &owner,
        Some("publish-owner"),
        Some(agent_body(urn, "owner", json!({}))),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, got) = send(
        &app,
        "GET",
        &format!("/v1/agents/{urn}/permissions"),
        &admin,
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{got}");
    assert_defaults(&got);

    let (status, put_err) = send(
        &app,
        "PUT",
        &format!("/v1/agents/{urn}/permissions"),
        &admin,
        Some("admin-put"),
        Some(json!({
            "tools": { "default": "deny" },
            "internet": { "default": "deny" }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{put_err}");
}

#[tokio::test]
async fn effective_and_explain_404_without_an_agent_and_presets_are_served() {
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let admin = mint_user_key(
        &store,
        "admin",
        UserRole::Publisher,
        &["catalog:write", "runs:write"],
    );
    let app = test_app(app_state(store));
    let urn = "urn:vam:agent:local.vam.dev/acme/later";
    for (method, uri, expect) in [
        (
            "GET",
            format!("/v1/agents/{urn}/permissions/effective"),
            StatusCode::NOT_FOUND,
        ),
        (
            "GET",
            format!("/v1/agents/{urn}/permissions/explain?tool=builtin_run"),
            StatusCode::NOT_FOUND,
        ),
        ("GET", "/v1/permission-presets".to_string(), StatusCode::OK),
    ] {
        let (status, body) = send(&app, method, &uri, &admin, None, None).await;
        assert_eq!(status, expect, "{method} {uri} {body}");
    }
}

// AX-5.6b: POST .../permissions/migrate.

fn legacy_agent_body(urn: &str, policy_urn: &str) -> Value {
    agent_body(
        urn,
        "legacy cedar",
        json!({
            "capabilityPolicy": {
                "policyRef": { "resourceId": { "urn": policy_urn } }
            },
            "hitlPolicy": {
                "monitoredTools": ["not_a_real_tool", "read_file"]
            }
        }),
    )
}

async fn put_known_policy(app: &axum::Router, secret: &str, policy_urn: &str) {
    let cedar = r#"
        permit (principal, action == Action::"ExecuteTool", resource)
        when { [Tool::"read_file", Tool::"load_skill"].contains(resource) };
    "#;
    let (status, body) = send(
        app,
        "PUT",
        "/v1/policies",
        secret,
        Some("put-migrate-policy"),
        Some(json!({
            "urn": policy_urn,
            "cedar": cedar,
            "idempotencyKey": "put-migrate-policy"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn migrate_requires_an_idempotency_key_and_hides_a_foreign_draft() {
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let owner = mint_user_key(&store, "owner", UserRole::OrgAdmin, ORG_ADMIN_SCOPES);
    let member = mint_user_key(
        &store,
        "member",
        UserRole::Member,
        &["catalog:write", "runs:write"],
    );
    let app = test_app(app_state(store));
    let urn = "urn:vam:agent:local.vam.dev/acme/migrate-auth";
    let policy_urn = "urn:vam:policy:local.vam.dev/acme/migrate-auth";
    put_known_policy(&app, &owner, policy_urn).await;

    let (status, published) = send(
        &app,
        "PUT",
        "/v1/agents",
        &owner,
        Some("publish-migrate-auth"),
        Some(legacy_agent_body(urn, policy_urn)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{published}");

    let (status, missing) = send(
        &app,
        "POST",
        &format!("/v1/agents/{urn}/permissions/migrate"),
        &owner,
        None,
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{missing}");

    let (status, hidden) = send(
        &app,
        "POST",
        &format!("/v1/agents/{urn}/permissions/migrate"),
        &member,
        Some("member-migrate"),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{hidden}");
}

#[tokio::test]
async fn migrate_conflicts_on_unmapped_tools_until_force_then_again() {
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let admin = mint_user_key(&store, "admin", UserRole::OrgAdmin, ORG_ADMIN_SCOPES);
    let app = test_app(app_state(store));
    let urn = "urn:vam:agent:local.vam.dev/acme/migrate-force";
    let policy_urn = "urn:vam:policy:local.vam.dev/acme/migrate-force";
    put_known_policy(&app, &admin, policy_urn).await;

    let (status, published) = send(
        &app,
        "PUT",
        "/v1/agents",
        &admin,
        Some("publish-migrate-force"),
        Some(legacy_agent_body(urn, policy_urn)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{published}");

    let (status, conflict) = send(
        &app,
        "POST",
        &format!("/v1/agents/{urn}/permissions/migrate"),
        &admin,
        Some("migrate-no-force"),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{conflict}");
    let message = conflict["message"].as_str().unwrap_or("");
    assert!(message.contains("not_a_real_tool"), "{message}");
    assert!(message.contains("force=true"), "{message}");

    let (status, unchanged) = send(
        &app,
        "GET",
        &format!("/v1/agents/{urn}"),
        &admin,
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{unchanged}");
    assert!(
        unchanged["spec"].get("permissions").is_none(),
        "{unchanged}"
    );
    assert!(
        unchanged["spec"].get("capabilityPolicy").is_some(),
        "{unchanged}"
    );

    let (status, report) = send(
        &app,
        "POST",
        &format!("/v1/agents/{urn}/permissions/migrate?force=true"),
        &admin,
        Some("migrate-force"),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{report}");
    assert!(
        report["unmapped"]
            .as_array()
            .unwrap()
            .iter()
            .any(|name| name == "not_a_real_tool"),
        "{report}"
    );
    assert!(
        report["converted"]
            .as_array()
            .unwrap()
            .iter()
            .any(|target| target == "builtin:read_file"),
        "{report}"
    );
    assert!(
        report["behaviourChanges"]
            .as_array()
            .unwrap()
            .iter()
            .any(|line| line.as_str().unwrap_or("").contains("Any site")),
        "{report}"
    );

    let (status, got) = send(
        &app,
        "GET",
        &format!("/v1/agents/{urn}/permissions"),
        &admin,
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{got}");
    assert_eq!(got["tools"]["default"], "deny", "{got}");
    assert_eq!(got["internet"]["default"], "deny", "{got}");

    let (status, agent) = send(
        &app,
        "GET",
        &format!("/v1/agents/{urn}"),
        &admin,
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{agent}");
    assert!(agent["spec"].get("capabilityPolicy").is_none(), "{agent}");
    assert!(agent["spec"].get("hitlPolicy").is_none(), "{agent}");

    let (status, again) = send(
        &app,
        "POST",
        &format!("/v1/agents/{urn}/permissions/migrate?force=1"),
        &admin,
        Some("migrate-again"),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{again}");
    let message = again["message"].as_str().unwrap_or("");
    assert!(
        message.contains("does not use capability_policy or hitl_policy"),
        "{message}"
    );
}

#[tokio::test]
async fn migrate_leaves_the_release_and_the_suggestion_names_the_cedar_move() {
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let admin = mint_user_key(&store, "admin", UserRole::OrgAdmin, ORG_ADMIN_SCOPES);
    let app = test_app(app_state(store));
    let urn = "urn:vam:agent:local.vam.dev/acme/migrate-release";
    let policy_urn = "urn:vam:policy:local.vam.dev/acme/migrate-release";
    put_known_policy(&app, &admin, policy_urn).await;

    let (status, published) = send(
        &app,
        "PUT",
        "/v1/agents",
        &admin,
        Some("publish-migrate-release"),
        Some(agent_body(
            urn,
            "released then migrated",
            json!({
                "capabilityPolicy": {
                    "policyRef": { "resourceId": { "urn": policy_urn } }
                }
            }),
        )),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{published}");

    let (status, policy_release) = send(
        &app,
        "POST",
        &format!("/v1/policies/{policy_urn}/release"),
        &admin,
        Some("release-migrate-policy"),
        Some(json!({ "version": { "major": 1, "minor": 0, "patch": 0 } })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{policy_release}");

    let (status, released) = send(
        &app,
        "POST",
        &format!("/v1/agents/{urn}/release"),
        &admin,
        Some("release-before-migrate"),
        Some(json!({ "version": { "major": 1, "minor": 0, "patch": 0 } })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{released}");
    assert!(
        released["spec"].get("capabilityPolicy").is_some(),
        "{released}"
    );
    assert!(released["spec"].get("permissions").is_none(), "{released}");

    let (status, report) = send(
        &app,
        "POST",
        &format!("/v1/agents/{urn}/permissions/migrate"),
        &admin,
        Some("migrate-after-release"),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{report}");
    assert!(
        report["converted"]
            .as_array()
            .unwrap()
            .iter()
            .any(|target| target == "builtin:read_file"),
        "{report}"
    );

    let (status, suggestion) = send(
        &app,
        "GET",
        &format!("/v1/agents/{urn}/release-suggestion"),
        &admin,
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{suggestion}");
    let blob = suggestion.to_string();
    assert!(blob.contains("moved from a Cedar policy"), "{suggestion}");
}
