//! AX-5.8a: effective permissions, explain, and presets.
//!
//! J1: a new agent's effective view lists the caller's MCP tool as ask,
//! "default (mutable external)".
//! J5: `skill:jira/*: allow` plus skill `approvalRequired` explains as ASK,
//! and the source row decides with `approvalRequired (skill jira v2.1.0)`.
//! J6: a legacy agent's effective view says "legacy policy".
//! J7: `*.pastebin.com: deny` denies a host that rule matches and names the
//! agent version. The apex `pastebin.com` is not that host: AX-5.2b
//! `explain_host` does not match `*.suffix` against the apex.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;
use vam_core::{CidAllocator, MockVmm};
use vam_orchestrator::api::auth::ControlPlanePdp;
use vam_orchestrator::api::{self, AppState};
use vam_orchestrator::keys::{mint_api_key_secret, MEMBER_SCOPES, ORG_ADMIN_SCOPES};
use vam_orchestrator::manager::SessionManager;
use vam_orchestrator::mcp_client::McpTool;
use vam_orchestrator::mcp_registry::McpAuthMode;
use vam_orchestrator::secrets::{SecretScope, SecretsVault};
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
    let scopes: Vec<String> = scopes.iter().map(|scope| (*scope).to_string()).collect();
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

fn agent_body(urn: &str, extra: Value) -> Value {
    let mut spec = json!({
        "resourceId": {"urn": urn},
        "displayName": "Owned agent",
        "description": "permissions view test",
        "role": "Tester",
        "systemInstructions": "test",
        "allowAnyPlaybook": true
    });
    if let (Some(spec), Some(extra)) = (spec.as_object_mut(), extra.as_object()) {
        for (key, value) in extra {
            spec.insert(key.clone(), value.clone());
        }
    }
    json!({ "spec": spec })
}

fn tool_named<'a>(body: &'a Value, name: &str) -> &'a Value {
    body["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("tools: {body}"))
        .iter()
        .find(|row| row["toolName"] == name)
        .unwrap_or_else(|| panic!("missing tool {name} in {body}"))
}

fn scope_named<'a>(body: &'a Value, scope: &str) -> &'a Value {
    body["scopes"]
        .as_array()
        .unwrap_or_else(|| panic!("scopes: {body}"))
        .iter()
        .find(|row| row["scope"] == scope)
        .unwrap_or_else(|| panic!("missing scope {scope} in {body}"))
}

#[tokio::test]
async fn j1_effective_view_lists_an_mcp_tool_as_the_mutable_external_default() {
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let owner = mint_user_key(&store, "owner", UserRole::OrgAdmin, ORG_ADMIN_SCOPES);
    let member = mint_user_key(&store, "member", UserRole::Member, MEMBER_SCOPES);
    store
        .upsert_mcp_connection(
            SecretScope::User,
            "owner",
            "jira",
            "https://mcp.atlassian.com/v1",
            "",
            &[],
            &[McpTool {
                name: "create_issue".into(),
                description: Some("Create an issue".into()),
                input_schema: json!({"type": "object"}),
                ..Default::default()
            }],
            McpAuthMode::Static,
            None,
        )
        .expect("mcp connection");
    let app = test_app(app_state(store));
    let urn = "urn:vam:agent:local.vam.dev/acme/new-agent";

    let (status, published) = send(
        &app,
        "PUT",
        "/v1/agents",
        &owner,
        Some("publish-new"),
        Some(agent_body(urn, json!({}))),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{published}");

    let (status, view) = send(
        &app,
        "GET",
        &format!("/v1/agents/{urn}/permissions/effective"),
        &owner,
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{view}");
    assert_eq!(view["version"], "draft");
    assert_eq!(view["legacy"], false);
    assert_eq!(view["internet"]["default"], "allow");
    assert!(view["internet"]["rules"].as_array().unwrap().is_empty());

    let mcp = tool_named(&view, "jira__create_issue");
    assert_eq!(mcp["decision"], "ask", "{mcp}");
    assert_eq!(mcp["target"], "mcp:mcp.atlassian.com/v1/create_issue");
    assert_eq!(mcp["source"], "mcp mcp.atlassian.com/v1");
    assert_eq!(mcp["decidingScope"], "agent");
    assert_eq!(
        mcp["decidingRule"],
        "tools:* default bySideEffect (mutable external)"
    );
    assert_eq!(mcp["isDefault"], true);
    assert_eq!(mcp["defaultLabel"], "default (mutable external)");
    assert_eq!(mcp["locked"], false);

    let read = tool_named(&view, "read_file");
    assert_eq!(read["decision"], "allow");
    assert_eq!(read["source"], "builtin");
    assert_eq!(read["target"], "builtin:read_file");
    assert_eq!(read["isDefault"], true);
    assert_eq!(read["defaultLabel"], "default (read-only)");

    let (status, hidden) = send(
        &app,
        "GET",
        &format!("/v1/agents/{urn}/permissions/effective"),
        &member,
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{hidden}");

    let (status, released) = send(
        &app,
        "POST",
        &format!("/v1/agents/{urn}/release"),
        &owner,
        Some("release-new"),
        Some(json!({"version": {"major": 1, "minor": 0, "patch": 0}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{released}");

    let (status, member_view) = send(
        &app,
        "GET",
        &format!("/v1/agents/{urn}/permissions/effective?version=1.0.0"),
        &member,
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{member_view}");
    assert_eq!(member_view["version"], "1.0.0");
    assert!(member_view["tools"]
        .as_array()
        .unwrap()
        .iter()
        .all(|row| row["toolName"] != "jira__create_issue"));
    assert_eq!(tool_named(&member_view, "read_file")["decision"], "allow");
}

#[tokio::test]
async fn j5_explain_names_the_skill_approval_and_j7_names_the_host_rule() {
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let owner = mint_user_key(&store, "owner", UserRole::OrgAdmin, ORG_ADMIN_SCOPES);
    let member = mint_user_key(&store, "member", UserRole::Member, MEMBER_SCOPES);
    let app = test_app(app_state(store));
    let skill = "urn:vam:skill:local.vam.dev/acme/jira";
    let urn = "urn:vam:agent:local.vam.dev/acme/jira-assistant";

    let (status, published_skill) = send(
        &app,
        "PUT",
        "/v1/skills",
        &owner,
        Some("publish-jira"),
        Some(json!({
            "spec": {
                "resourceId": {"urn": skill},
                "version": {"major": 2, "minor": 1, "patch": 0},
                "description": "Jira",
                "tools": [{
                    "name": "create_issue",
                    "description": "Create an issue",
                    "inputSchemaJson": "{}",
                    "approvalRequired": true,
                    "sideEffect": "SIDE_EFFECT_CLASS_MUTABLE_EXTERNAL",
                    "localExec": {"command": "bun", "entrypoint": "./execute.js"}
                }]
            }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{published_skill}");

    let (status, released_skill) = send(
        &app,
        "POST",
        &format!("/v1/skills/{skill}/release"),
        &owner,
        Some("release-jira"),
        Some(json!({"version": {"major": 2, "minor": 1, "patch": 0}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{released_skill}");

    let (status, published) = send(
        &app,
        "PUT",
        "/v1/agents",
        &owner,
        Some("publish-assistant"),
        Some(agent_body(
            urn,
            json!({
                "skillRefs": [{
                    "resourceId": {"urn": skill},
                    "version": {"major": 2, "minor": 1, "patch": 0}
                }],
                "permissions": {
                    "tools": {
                        "default": "bySideEffect",
                        "rules": [{ "target": "skill:jira/*", "decision": "allow" }]
                    },
                    "internet": {
                        "default": "allow",
                        "rules": [{ "target": "*.pastebin.com", "decision": "deny" }]
                    }
                }
            }),
        )),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{published}");

    let (status, released) = send(
        &app,
        "POST",
        &format!("/v1/agents/{urn}/release"),
        &owner,
        Some("release-assistant"),
        Some(json!({"version": {"major": 1, "minor": 3, "patch": 0}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{released}");

    let (status, explained) = send(
        &app,
        "GET",
        &format!("/v1/agents/{urn}/permissions/explain?tool=create_issue&version=1.3.0"),
        &member,
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{explained}");
    assert_eq!(explained["kind"], "tool");
    assert_eq!(explained["decision"], "ask");
    assert_eq!(explained["version"], "1.3.0");
    assert_eq!(explained["target"], "skill:jira/create_issue");
    assert_eq!(explained["deciding"]["scope"], "source");
    assert_eq!(explained["deciding"]["rule"], "approvalRequired");
    assert_eq!(explained["deciding"]["origin"], "skill jira v2.1.0");
    assert_eq!(explained["deciding"]["deciding"], true);
    let source = scope_named(&explained, "source");
    assert_eq!(source["deciding"], true);
    assert_eq!(source["decision"], "ask");
    assert_eq!(source["rule"], "approvalRequired");
    assert_eq!(source["origin"], "skill jira v2.1.0");
    let agent = scope_named(&explained, "agent");
    assert_eq!(agent["deciding"], false);
    assert_eq!(agent["decision"], "allow");
    assert_eq!(agent["rule"], "skill:jira/*");
    assert_eq!(agent["origin"], "agent v1.3.0");
    assert_eq!(scope_named(&explained, "org")["decision"], "none");
    assert_eq!(
        explained["text"],
        "\
ASK   create_issue  (skill:jira/create_issue)
  agent  allow   skill:jira/*  (agent v1.3.0)
  source ask     approvalRequired  (skill jira v2.1.0)     <- decides
  org    none"
    );

    let (status, by_target) = send(
        &app,
        "GET",
        &format!("/v1/agents/{urn}/permissions/explain?tool=skill:jira/create_issue&version=1.3.0"),
        &member,
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{by_target}");
    assert_eq!(by_target["toolName"], "create_issue");
    assert_eq!(by_target["decision"], "ask");

    // `*.pastebin.com` matches a subdomain. The apex is a different host.
    let (status, host) = send(
        &app,
        "GET",
        &format!("/v1/agents/{urn}/permissions/explain?host=www.pastebin.com&version=1.3.0"),
        &member,
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{host}");
    assert_eq!(host["kind"], "host");
    assert_eq!(host["decision"], "deny");
    assert_eq!(host["deciding"]["rule"], "*.pastebin.com");
    assert_eq!(host["deciding"]["origin"], "agent v1.3.0");
    assert_eq!(host["deciding"]["scope"], "agent");
    assert_eq!(host["scopes"][0]["deciding"], true);
    assert_eq!(
        host["text"],
        "DENY  www.pastebin.com   agent rule *.pastebin.com at agent v1.3.0"
    );

    let (status, apex) = send(
        &app,
        "GET",
        &format!("/v1/agents/{urn}/permissions/explain?host=pastebin.com&version=1.3.0"),
        &owner,
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{apex}");
    assert_eq!(apex["decision"], "allow");
    assert_eq!(apex["deciding"]["rule"], "internet:*");
    assert_eq!(apex["deciding"]["origin"], "agent v1.3.0");

    let (status, effective) = send(
        &app,
        "GET",
        &format!("/v1/agents/{urn}/permissions/effective?version=1.3.0"),
        &member,
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{effective}");
    let create = tool_named(&effective, "create_issue");
    assert_eq!(create["decision"], "ask");
    assert_eq!(create["source"], "skill jira v2.1.0");
    assert_eq!(create["decidingScope"], "source");
    assert_eq!(create["decidingRule"], "approvalRequired");
    assert_eq!(create["locked"], true);
    assert_eq!(create["isDefault"], false);
    assert!(
        create["lockedReason"]
            .as_str()
            .unwrap()
            .contains("approvalRequired"),
        "{create}"
    );
    assert_eq!(effective["internet"]["default"], "allow");
    assert_eq!(
        effective["internet"]["rules"][0]["target"],
        "*.pastebin.com"
    );
    assert_eq!(effective["internet"]["rules"][0]["decision"], "deny");

    let (status, both) = send(
        &app,
        "GET",
        &format!(
            "/v1/agents/{urn}/permissions/explain?tool=create_issue&host=example.com&version=1.3.0"
        ),
        &member,
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{both}");

    let (status, neither) = send(
        &app,
        "GET",
        &format!("/v1/agents/{urn}/permissions/explain?version=1.3.0"),
        &member,
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{neither}");

    let (status, missing) = send(
        &app,
        "GET",
        &format!("/v1/agents/{urn}/permissions/explain?tool=not_a_tool&version=1.3.0"),
        &member,
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{missing}");
}

#[tokio::test]
async fn j6_legacy_effective_view_says_legacy_policy() {
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let owner = mint_user_key(&store, "owner", UserRole::OrgAdmin, ORG_ADMIN_SCOPES);
    let app = test_app(app_state(store));
    let urn = "urn:vam:agent:local.vam.dev/acme/legacy";

    let (status, published) = send(
        &app,
        "PUT",
        "/v1/agents",
        &owner,
        Some("publish-legacy"),
        Some(agent_body(
            urn,
            json!({
                "capabilityPolicy": { "securityProfileName": "restricted-egress" },
                "hitlPolicy": { "monitoredTools": ["run"] }
            }),
        )),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{published}");
    assert!(
        published["spec"].get("permissions").is_none(),
        "{published}"
    );

    let (status, view) = send(
        &app,
        "GET",
        &format!("/v1/agents/{urn}/permissions/effective"),
        &owner,
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{view}");
    assert_eq!(view["legacy"], true);
    assert!(view["internet"].is_null(), "{view}");

    let read = tool_named(&view, "read_file");
    assert_eq!(read["decision"], "allow", "{read}");
    assert_eq!(read["decidingScope"], "legacy policy");
    assert_eq!(read["lockedReason"], "legacy policy");
    assert_eq!(read["locked"], true);
    assert_eq!(read["source"], "builtin");
    assert_eq!(read["target"], "builtin:read_file");

    let run = tool_named(&view, "run");
    assert_eq!(run["decision"], "ask", "{run}");
    assert_eq!(run["decidingScope"], "legacy policy");
    assert_eq!(run["lockedReason"], "legacy policy");

    let (status, host) = send(
        &app,
        "GET",
        &format!("/v1/agents/{urn}/permissions/explain?host=example.com"),
        &owner,
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{host}");
}

#[tokio::test]
async fn presets_are_static_and_require_catalog_read() {
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let member = mint_user_key(&store, "member", UserRole::Member, MEMBER_SCOPES);
    let app = test_app(app_state(store));

    let (status, presets) = send(&app, "GET", "/v1/permission-presets", &member, None, None).await;
    assert_eq!(status, StatusCode::OK, "{presets}");
    assert_eq!(presets["version"], 1);
    assert_eq!(presets["presets"][0]["id"], "ask-before-writes");
    assert_eq!(presets["presets"][0]["name"], "Ask before writes");
    assert_eq!(
        presets["presets"][0]["permissions"]["tools"]["default"],
        "bySideEffect"
    );
    assert_eq!(
        presets["presets"][0]["permissions"]["internet"]["default"],
        "allow"
    );
    assert_eq!(presets["presets"][1]["id"], "open-internet");
    assert_eq!(
        presets["presets"][1]["permissions"]["tools"]["default"],
        "allow"
    );
    assert_eq!(presets["presets"][2]["id"], "read-only");
    assert_eq!(
        presets["presets"][2]["permissions"]["tools"]["default"],
        "deny"
    );
    assert_eq!(
        presets["presets"][2]["permissions"]["internet"]["default"],
        "deny"
    );
    assert_eq!(
        presets["presets"][2]["permissions"]["tools"]["rules"][0]["target"],
        "builtin:read_file"
    );

    let open = test_app(app_state(Arc::new(
        SqliteWorkflowStore::in_memory().unwrap(),
    )))
    .oneshot(
        Request::builder()
            .method("GET")
            .uri("/v1/permission-presets")
            .body(Body::empty())
            .unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(open.status(), StatusCode::UNAUTHORIZED);
}

fn register_web_broker(store: &SqliteWorkflowStore) {
    let mut labels = std::collections::HashMap::new();
    labels.insert("vam.capability".to_string(), "web".to_string());
    let manifest = vam_spec::v1::CapabilityManifest {
        broker_id: "vam.web".to_string(),
        trust_tier: 2,
        tools: ["web_search", "web_fetch"]
            .iter()
            .map(|name| vam_spec::v1::ToolExport {
                tool_name: name.to_string(),
                side_effect: 1,
                required_secrets: vec![],
                destination_policy: 0,
                effect: None,
            })
            .collect(),
        labels,
        ..Default::default()
    };
    store.upsert_hsp_broker_approval(&manifest).unwrap();
    let snapshots: Vec<_> = ["web_search", "web_fetch"]
        .iter()
        .map(|name| vam_orchestrator::hsp_store::HspBrokerToolSnapshot {
            tool_name: name.to_string(),
            adapter_id: format!("hosted:vam.web:{name}"),
            side_effect: 1,
            description: String::new(),
            input_schema_json: "{}".to_string(),
        })
        .collect();
    store
        .replace_hsp_broker_tools("vam.web", &snapshots)
        .unwrap();
}

#[tokio::test]
async fn web_shows_in_effective_and_explain_and_follows_the_switch() {
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let owner = mint_user_key(&store, "owner", UserRole::OrgAdmin, ORG_ADMIN_SCOPES);
    register_web_broker(&store);
    let app = test_app(app_state(store));
    let urn = "urn:vam:agent:local.vam.dev/acme/web-agent";

    // Web on: both tools are listed and allowed with no approval.
    let (status, published) = send(
        &app,
        "PUT",
        "/v1/agents",
        &owner,
        Some("web-on"),
        Some(agent_body(
            urn,
            json!({"permissions": {"web": {"enabled": true}}}),
        )),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{published}");
    let (status, view) = send(
        &app,
        "GET",
        &format!("/v1/agents/{urn}/permissions/effective"),
        &owner,
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{view}");
    assert_eq!(view["web"]["enabled"], true, "{view}");
    assert_eq!(view["web"]["available"], true, "{view}");
    for name in ["web_search", "web_fetch"] {
        let row = tool_named(&view, name);
        assert_eq!(row["decision"], "allow", "{row}");
        assert_eq!(row["target"], format!("builtin:{name}"));
        assert_eq!(row["source"], "hosted vam.web");
    }
    let (status, explained) = send(
        &app,
        "GET",
        &format!("/v1/agents/{urn}/permissions/explain?tool=web_fetch"),
        &owner,
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{explained}");
    assert_eq!(explained["decision"], "allow", "{explained}");

    // Web off: no web rows, and explain says why.
    let (status, republished) = send(
        &app,
        "PUT",
        "/v1/agents",
        &owner,
        Some("web-off"),
        Some(agent_body(urn, json!({"permissions": {}}))),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{republished}");
    let (_, view) = send(
        &app,
        "GET",
        &format!("/v1/agents/{urn}/permissions/effective"),
        &owner,
        None,
        None,
    )
    .await;
    assert_eq!(view["web"]["enabled"], false, "{view}");
    let names: Vec<&str> = view["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|row| row["toolName"].as_str())
        .collect();
    assert!(
        !names.contains(&"web_fetch") && !names.contains(&"web_search"),
        "{names:?}"
    );
    let (status, explained) = send(
        &app,
        "GET",
        &format!("/v1/agents/{urn}/permissions/explain?tool=web_fetch"),
        &owner,
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{explained}");
    assert_eq!(explained["decision"], "deny", "{explained}");
    assert!(
        explained["text"]
            .as_str()
            .unwrap()
            .contains("web access is off"),
        "{explained}"
    );
}

#[tokio::test]
async fn effective_view_notes_web_on_without_a_broker() {
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let owner = mint_user_key(&store, "owner", UserRole::OrgAdmin, ORG_ADMIN_SCOPES);
    let app = test_app(app_state(store));
    let urn = "urn:vam:agent:local.vam.dev/acme/lonely-web";
    let (status, published) = send(
        &app,
        "PUT",
        "/v1/agents",
        &owner,
        Some("lonely"),
        Some(agent_body(
            urn,
            json!({"permissions": {"web": {"enabled": true}}}),
        )),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{published}");
    let (_, view) = send(
        &app,
        "GET",
        &format!("/v1/agents/{urn}/permissions/effective"),
        &owner,
        None,
        None,
    )
    .await;
    assert_eq!(view["web"]["enabled"], true);
    assert_eq!(view["web"]["available"], false);
    assert!(view["web"]["note"]
        .as_str()
        .unwrap()
        .contains("no web broker"));
}
