//! Members save their own draft agents (member-draft-agents Phase 3, decisions 6 to 9).
//!
//! `agents:draft` opens `PUT /v1/agents` and `PUT /v1/agents/{urn}/permissions` in restricted
//! mode and nothing else. Every other `catalog:write` route stays 403 for a member key.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;
use vam_core::{CidAllocator, MockVmm};
use vam_orchestrator::api::auth::ControlPlanePdp;
use vam_orchestrator::api::{self, AppState};
use vam_orchestrator::keys::{
    mint_api_key_secret, ScopeMode, AGENTS_DRAFT_SCOPE, MEMBER_SCOPES, PUBLISHER_SCOPES,
};
use vam_orchestrator::manager::SessionManager;
use vam_orchestrator::secrets::SecretsVault;
use vam_orchestrator::users::UserRole;
use vam_orchestrator::workflow::SqliteWorkflowStore;
use vam_orchestrator::VmmFactory;
use vam_spec::v1::{
    PlaybookActivation, PlaybookSpec, PlaybookStep, ResourceId, SkillChannel, SkillManifest,
    SkillPackageRecord, StepMode, Version,
};

const PLAYBOOK: &str = "urn:vam:playbook:local.vam.dev/acme/released-pb";
const DRAFT_SKILL: &str = "urn:vam:skill:local.vam.dev/acme/draft-skill";
const OWN: &str = "urn:vam:agent:local.vam.dev/member/own-agent";

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

fn mint_key(
    store: &SqliteWorkflowStore,
    user_id: &str,
    role: UserRole,
    scopes: &[&str],
    mode: ScopeMode,
) -> String {
    let _ = store.create_user_with_role(user_id, user_id, role);
    let secret = mint_api_key_secret();
    let scopes: Vec<String> = scopes.iter().map(|s| (*s).to_string()).collect();
    store
        .create_api_key_owned_with_mode(
            &format!("key-{user_id}-{}", scopes.len()),
            user_id,
            &secret,
            &format!("User::\"{user_id}\""),
            &scopes,
            user_id,
            mode,
        )
        .expect("mint user key");
    secret
}

struct World {
    store: Arc<SqliteWorkflowStore>,
    app: axum::Router,
    member: String,
    publisher: String,
}

fn world() -> World {
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let member = mint_key(
        &store,
        "member",
        UserRole::Member,
        MEMBER_SCOPES,
        ScopeMode::Role,
    );
    let publisher = mint_key(
        &store,
        "pub",
        UserRole::Publisher,
        PUBLISHER_SCOPES,
        ScopeMode::Role,
    );
    store
        .put_playbook_with_owner(
            &PlaybookSpec {
                activation: Some(PlaybookActivation::asking("Use when asked.")),
                resource_id: Some(ResourceId {
                    urn: PLAYBOOK.to_string(),
                }),
                display_name: "pb".to_string(),
                description: "test".to_string(),
                steps: vec![PlaybookStep {
                    id: "s1".to_string(),
                    description: "step".to_string(),
                    instruction: "Do the step.".to_string(),
                    mode: StepMode::Agentic as i32,
                    ..Default::default()
                }],
                ..Default::default()
            },
            "pub",
        )
        .expect("released playbook");
    let version = Version {
        major: 1,
        prerelease: "draft".to_string(),
        ..Default::default()
    };
    store
        .put_skill_package_with_owner(
            &SkillPackageRecord {
                resource_id: Some(ResourceId {
                    urn: DRAFT_SKILL.to_string(),
                }),
                version: Some(version.clone()),
                channel: SkillChannel::Draft as i32,
                digest: "a".repeat(64),
                blob_relpath: format!("local/sha256-{}.tar.gz", "a".repeat(64)),
                size_bytes: 8,
                spec: Some(SkillManifest {
                    resource_id: Some(ResourceId {
                        urn: DRAFT_SKILL.to_string(),
                    }),
                    version: Some(version),
                    description: "Test".to_string(),
                    ..Default::default()
                }),
                ..Default::default()
            },
            "pub",
        )
        .expect("publisher's draft skill");
    let app = api::router(app_state(store.clone()));
    World {
        store,
        app,
        member,
        publisher,
    }
}

static IDEM: AtomicUsize = AtomicUsize::new(0);

async fn send(
    app: &axum::Router,
    method: &str,
    uri: &str,
    secret: &str,
    body: &str,
    content_type: &str,
) -> (StatusCode, String) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header("authorization", format!("Bearer {secret}"))
                .header(
                    "Idempotency-Key",
                    format!("idem-{}", IDEM.fetch_add(1, Ordering::SeqCst)),
                )
                .header("content-type", content_type)
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let body = String::from_utf8(
        hyper::body::to_bytes(resp.into_body())
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    (status, body)
}

fn agent_body(urn: &str, extra: serde_json::Value) -> String {
    let mut spec = serde_json::json!({
        "resourceId": {"urn": urn},
        "displayName": "Member agent",
        "description": "member draft agent test",
        "role": "Tester",
        "systemInstructions": "Answer briefly.",
        "allowAnyPlaybook": true
    });
    if let (Some(spec), Some(extra)) = (spec.as_object_mut(), extra.as_object()) {
        for (k, v) in extra {
            spec.insert(k.clone(), v.clone());
        }
    }
    serde_json::json!({ "spec": spec }).to_string()
}

async fn put_agent(
    w: &World,
    key: &str,
    urn: &str,
    extra: serde_json::Value,
) -> (StatusCode, String) {
    send(
        &w.app,
        "PUT",
        "/v1/agents",
        key,
        &agent_body(urn, extra),
        "application/json",
    )
    .await
}

/// The 13 `catalog:write` sites that stay closed to `agents:draft`, by route.
#[tokio::test]
async fn closed_routes_stay_403_for_a_member_key() {
    let w = world();
    let agent = "urn:vam:agent:local.vam.dev/member/closed";
    let release = r#"{"version":{"major":1,"minor":0,"patch":0}}"#;
    let multipart = "multipart/form-data; boundary=xx";
    // (label, method, uri, body, content type, scope named in the 403)
    let table: Vec<(&str, &str, String, String, &str, &str)> = vec![
        (
            "delete agent (mod.rs delete)",
            "DELETE",
            format!("/v1/agents/{agent}"),
            String::new(),
            "application/json",
            "catalog:write",
        ),
        (
            "delete playbook (same site)",
            "DELETE",
            format!("/v1/playbooks/{PLAYBOOK}"),
            String::new(),
            "application/json",
            "catalog:write",
        ),
        (
            "put policy",
            "PUT",
            "/v1/policies".into(),
            "{}".into(),
            "application/json",
            "catalog:write",
        ),
        (
            "put playbook",
            "PUT",
            "/v1/playbooks".into(),
            "{}".into(),
            "application/json",
            "catalog:write",
        ),
        (
            "put skill",
            "PUT",
            "/v1/skills".into(),
            "{}".into(),
            "application/json",
            "catalog:write",
        ),
        (
            "list hosted brokers",
            "GET",
            "/v1/hosted/brokers".into(),
            String::new(),
            "application/json",
            "catalog:write",
        ),
        (
            "get hosted broker",
            "GET",
            "/v1/hosted/brokers/some-broker".into(),
            String::new(),
            "application/json",
            "catalog:write",
        ),
        (
            "post skill package",
            "POST",
            "/v1/skills/packages".into(),
            "--xx--\r\n".into(),
            multipart,
            "catalog:write",
        ),
        (
            "release skill package",
            "POST",
            "/v1/skills/packages/urn:vam:skill:local.vam.dev/member/s/release".into(),
            release.into(),
            "application/json",
            "catalog:write",
        ),
        (
            "release agent",
            "POST",
            format!("/v1/agents/{agent}/release"),
            release.into(),
            "application/json",
            "catalog:write",
        ),
        (
            "release playbook",
            "POST",
            format!("/v1/playbooks/{PLAYBOOK}/release"),
            release.into(),
            "application/json",
            "catalog:write",
        ),
        (
            "release skill",
            "POST",
            "/v1/skills/urn:vam:skill:local.vam.dev/member/s/release".into(),
            release.into(),
            "application/json",
            "catalog:write",
        ),
        (
            "release policy",
            "POST",
            "/v1/policies/urn:vam:policy:local.vam.dev/member/p/release".into(),
            release.into(),
            "application/json",
            "catalog:write",
        ),
        (
            "migrate agent permissions",
            "POST",
            format!("/v1/agents/{agent}/permissions/migrate"),
            String::new(),
            "application/json",
            "catalog:write",
        ),
    ];
    for (label, method, uri, body, content_type, scope) in table {
        let (status, text) = send(&w.app, method, &uri, &w.member, &body, content_type).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{label}: {text}");
        assert!(text.contains(scope), "{label}: {text}");
    }
    // `agents:draft` itself is not accepted where `catalog:write` is required, so a key
    // holding only that scope is refused on the same routes.
    let only_draft = mint_key(
        &w.store,
        "draftonly",
        UserRole::Member,
        &[AGENTS_DRAFT_SCOPE, "runs:write"],
        ScopeMode::Explicit,
    );
    let (status, _) = send(
        &w.app,
        "PUT",
        "/v1/playbooks",
        &only_draft,
        "{}",
        "application/json",
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn a_member_saves_an_own_draft_in_their_namespace_and_it_is_never_released() {
    let w = world();
    let (status, body) = put_agent(&w, &w.member, OWN, serde_json::json!({})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = put_agent(
        &w,
        &w.member,
        "urn:vam:agent:local.vam.dev/acme/other",
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("namespace"), "{body}");
    // The save wrote a draft row and no release.
    let versions = send(
        &w.app,
        "GET",
        &format!("/v1/agents/{OWN}/versions"),
        &w.member,
        "",
        "application/json",
    )
    .await;
    assert_eq!(versions.0, StatusCode::OK, "{}", versions.1);
    assert!(!versions.1.contains("releasedAt"), "{}", versions.1);
}

#[tokio::test]
async fn a_key_with_neither_scope_is_403_on_both_open_routes() {
    let w = world();
    let narrow = mint_key(
        &w.store,
        "narrow",
        UserRole::Member,
        &["runs:write", "node:read"],
        ScopeMode::Explicit,
    );
    let (status, body) = put_agent(
        &w,
        &narrow,
        "urn:vam:agent:local.vam.dev/narrow/a",
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(body.contains("catalog:write"), "{body}");
    let (status, body) = send(
        &w.app,
        "PUT",
        &format!("/v1/agents/{OWN}/permissions"),
        &narrow,
        "{}",
        "application/json",
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
}

#[tokio::test]
async fn publisher_and_bootstrap_keys_get_no_member_rules() {
    let w = world();
    let bootstrap = mint_api_key_secret();
    let scopes: Vec<String> = vam_orchestrator::keys::BOOTSTRAP_SCOPES
        .iter()
        .map(|s| s.to_string())
        .collect();
    w.store
        .create_api_key(
            "bootstrap",
            "bootstrap",
            &bootstrap,
            "User::\"bootstrap\"",
            &scopes,
        )
        .expect("bootstrap key");
    for (who, key) in [("publisher", &w.publisher), ("bootstrap", &bootstrap)] {
        // A URN outside the caller's namespace, a ref to a skill that is only a draft, and a
        // field a member may not set: all fine on the full path.
        let urn = format!("urn:vam:agent:local.vam.dev/acme/{who}-agent");
        let (status, body) = put_agent(
            &w,
            key,
            &urn,
            serde_json::json!({
                "skillRefs": [{"resourceId": {"urn": DRAFT_SKILL}}],
                "defaultPlaybookRef": {"resourceId": {"urn": "urn:vam:playbook:local.vam.dev/acme/not-there"}}
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{who}: {body}");
        let (status, body) = send(
            &w.app,
            "PUT",
            &format!("/v1/agents/{urn}/permissions"),
            key,
            r#"{"advanced":[{"resourceId":{"urn":"urn:vam:policy:local.vam.dev/acme/not-there"}}]}"#,
            "application/json",
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{who}: {body}");
    }
}

#[tokio::test]
async fn a_publisher_key_narrowed_to_agents_draft_is_restricted() {
    let w = world();
    let narrowed = mint_key(
        &w.store,
        "pub",
        UserRole::Publisher,
        &["runs:write", AGENTS_DRAFT_SCOPE],
        ScopeMode::Explicit,
    );
    let (status, body) = put_agent(
        &w,
        &narrowed,
        "urn:vam:agent:local.vam.dev/acme/narrowed",
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("namespace"), "{body}");
    let (status, body) = put_agent(
        &w,
        &narrowed,
        "urn:vam:agent:local.vam.dev/pub/narrowed",
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn refs_must_be_released_and_the_refusal_names_the_ref_alike_for_every_cause() {
    let w = world();
    let skill_ref = |urn: &str| serde_json::json!({"skillRefs": [{"resourceId": {"urn": urn}}]});
    let mut messages = Vec::new();
    for urn in ["urn:vam:skill:local.vam.dev/acme/not-there", DRAFT_SKILL] {
        let (status, body) = put_agent(&w, &w.member, OWN, skill_ref(urn)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(body.contains("has no released version"), "{body}");
        assert!(body.contains(urn), "{body}");
        messages.push(body.replace(urn, "<urn>"));
    }
    assert_eq!(
        messages[0], messages[1],
        "missing and someone's draft read alike"
    );
    // An explicit `-draft` pin is refused as well.
    let (status, body) = put_agent(
        &w,
        &w.member,
        OWN,
        serde_json::json!({"skillRefs": [{
            "resourceId": {"urn": DRAFT_SKILL},
            "version": {"major": 1, "minor": 0, "patch": 0, "prerelease": "draft"}
        }]}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    // A released playbook is fine, and nothing above was written.
    let (status, body) = put_agent(
        &w,
        &w.member,
        OWN,
        serde_json::json!({
            "allowAnyPlaybook": false,
            "allowedPlaybookRefs": [{"resourceId": {"urn": PLAYBOOK}}]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn each_refused_field_is_400_naming_it_over_http() {
    let w = world();
    for (field, extra) in [
        (
            "capabilityPolicy",
            serde_json::json!({"capabilityPolicy": {}}),
        ),
        ("hitlPolicy", serde_json::json!({"hitlPolicy": {}})),
    ] {
        let (status, body) = put_agent(&w, &w.member, OWN, extra).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{field}: {body}");
        assert!(body.contains(field), "{field}: {body}");
    }
}

#[tokio::test]
async fn the_permissions_put_of_an_own_draft_is_checked_and_leaves_the_draft() {
    let w = world();
    let (status, body) = put_agent(&w, &w.member, OWN, serde_json::json!({})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let path = format!("/v1/agents/{OWN}/permissions");
    let before = send(&w.app, "GET", &path, &w.member, "", "application/json").await;
    assert_eq!(before.0, StatusCode::OK, "{}", before.1);
    let (status, body) = send(
        &w.app,
        "PUT",
        &path,
        &w.member,
        r#"{"advanced":[{"resourceId":{"urn":"urn:vam:policy:local.vam.dev/acme/draft-only"}}]}"#,
        "application/json",
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("has no released version"), "{body}");
    let after = send(&w.app, "GET", &path, &w.member, "", "application/json").await;
    assert_eq!(before, after, "the draft permissions are unchanged");
    // Plain tool rules are accepted.
    let (status, body) = send(
        &w.app,
        "PUT",
        &path,
        &w.member,
        r#"{"tools":{"default":"deny"}}"#,
        "application/json",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // Another member cannot see or edit it: the same 404 as a missing draft.
    let other = mint_key(
        &w.store,
        "other",
        UserRole::Member,
        MEMBER_SCOPES,
        ScopeMode::Role,
    );
    let (status, _) = send(&w.app, "PUT", &path, &other, "{}", "application/json").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn the_draft_cap_answers_409_on_a_new_urn_and_not_on_an_edit() {
    let w = world();
    let max = vam_orchestrator::member_draft::member_draft_agents_max();
    assert_eq!(max, 50, "the default cap");
    for n in 0..max {
        let (status, body) = put_agent(
            &w,
            &w.member,
            &format!("urn:vam:agent:local.vam.dev/member/a{n}"),
            serde_json::json!({}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{n}: {body}");
    }
    let (status, body) = put_agent(
        &w,
        &w.member,
        "urn:vam:agent:local.vam.dev/member/one-too-many",
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body.contains("50"), "{body}");
    let (status, body) = put_agent(
        &w,
        &w.member,
        "urn:vam:agent:local.vam.dev/member/a0",
        serde_json::json!({"description": "edited at the cap"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}
