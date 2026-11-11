//! `POST /v1/agent-checks`, the composition check behind `vam agent check`.
//!
//! Informational: a required skill the agent does not attach is `attached: false`,
//! never an error status.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;
use vam_core::{CidAllocator, MockVmm};
use vam_orchestrator::api::auth::ControlPlanePdp;
use vam_orchestrator::api::{self, AppState};
use vam_orchestrator::keys::{mint_api_key_secret, PUBLISHER_SCOPES};
use vam_orchestrator::manager::SessionManager;
use vam_orchestrator::secrets::SecretsVault;
use vam_orchestrator::users::UserRole;
use vam_orchestrator::workflow::SqliteWorkflowStore;
use vam_orchestrator::VmmFactory;
use vam_spec::v1::{
    agent_spec::InstructionSource, AgentSpec, PlaybookRequirements, PlaybookSpec, PlaybookStep,
    ResourceId, ResourceRef, SkillChannel, SkillManifest, SkillPackageRecord, StepMode, Version,
};

const ALICE_AGENT: &str = "urn:vam:agent:local.vam.dev/acme/composer";
const PLAYBOOK: &str = "urn:vam:playbook:local.vam.dev/acme/triage";
const SKILL: &str = "urn:vam:skill:local.vam.dev/acme/jira-tools";
const DIGEST: &str = "1111111111111111111111111111111111111111111111111111111111111111";

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

fn mint_key(store: &SqliteWorkflowStore, user_id: &str) -> String {
    let _ = store.create_user_with_role(user_id, user_id, UserRole::Publisher);
    let secret = mint_api_key_secret();
    let scopes: Vec<String> = PUBLISHER_SCOPES.iter().map(|s| s.to_string()).collect();
    store
        .create_api_key_owned(
            &format!("key-{user_id}"),
            user_id,
            &secret,
            &format!("User::\"{user_id}\""),
            &scopes,
            user_id,
        )
        .expect("mint key");
    secret
}

fn floating(urn: &str) -> ResourceRef {
    ResourceRef {
        resource_id: Some(ResourceId { urn: urn.into() }),
        ..Default::default()
    }
}

fn release_version(major: i32) -> Version {
    Version {
        major,
        ..Default::default()
    }
}

fn skill_package() -> SkillPackageRecord {
    let manifest = SkillManifest {
        resource_id: Some(ResourceId { urn: SKILL.into() }),
        version: Some(release_version(1)),
        description: "Jira".into(),
        capabilities: vec!["tool:jira.search".into(), "egress:jira.example.com".into()],
        ..Default::default()
    };
    SkillPackageRecord {
        resource_id: manifest.resource_id.clone(),
        version: Some(release_version(1)),
        channel: SkillChannel::Released as i32,
        digest: DIGEST.into(),
        blob_relpath: format!("local/sha256-{DIGEST}.tar.gz"),
        size_bytes: 8,
        spec: Some(manifest),
        ..Default::default()
    }
}

fn playbook_requiring_skill() -> PlaybookSpec {
    PlaybookSpec {
        activation: Some(vam_spec::v1::PlaybookActivation::asking(
            "Use when the request matches this test playbook.",
        )),
        resource_id: Some(ResourceId {
            urn: PLAYBOOK.into(),
        }),
        display_name: "Triage".into(),
        description: "Triage tickets".into(),
        steps: vec![PlaybookStep {
            id: "s1".into(),
            description: "step".into(),
            instruction: "Do the step.".into(),
            mode: StepMode::Agentic as i32,
            ..Default::default()
        }],
        requires: Some(PlaybookRequirements {
            skills: vec![floating(SKILL)],
            capabilities: vec!["tool:jira.search".into()],
        }),
        ..Default::default()
    }
}

fn agent(skills: Vec<ResourceRef>, playbook: &str) -> AgentSpec {
    AgentSpec {
        resource_id: Some(ResourceId {
            urn: ALICE_AGENT.into(),
        }),
        display_name: "Composer".into(),
        description: "composition check".into(),
        role: "Tester".into(),
        instruction_source: Some(InstructionSource::SystemInstructions("be kind".into())),
        skill_refs: skills,
        default_playbook_ref: Some(floating(playbook)),
        ..Default::default()
    }
}

async fn check(app: &axum::Router, secret: &str, agent_ref: Value) -> (StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/agent-checks")
                .header("authorization", format!("Bearer {secret}"))
                .header("content-type", "application/json")
                .body(Body::from(json!({ "agentRef": agent_ref }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = hyper::body::to_bytes(response.into_body()).await.unwrap();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

fn draft_ref() -> Value {
    json!({
        "resourceId": {"urn": ALICE_AGENT},
        "version": {"major": 1, "prerelease": "draft"}
    })
}

struct World {
    store: Arc<SqliteWorkflowStore>,
    app: axum::Router,
    alice: String,
    bob: String,
}

fn world() -> World {
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    store.initialize_catalog_schema().unwrap();
    let alice = mint_key(&store, "alice");
    let bob = mint_key(&store, "bob");
    let app = api::router(app_state(store.clone()));
    World {
        store,
        app,
        alice,
        bob,
    }
}

fn seed_catalog(world: &World) {
    world.store.put_skill_package(&skill_package()).unwrap();
    world
        .store
        .put_playbook_with_owner(&playbook_requiring_skill(), "alice")
        .unwrap();
}

#[tokio::test]
async fn draft_agent_flags_a_required_skill_it_does_not_attach_then_clears_it() {
    let w = world();
    seed_catalog(&w);
    w.store
        .publish_agent_draft_with_owner(&agent(vec![], PLAYBOOK), "alice")
        .unwrap();

    let (status, body) = check(&w.app, &w.alice, draft_ref()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let playbook = &body["playbooks"][0];
    assert_eq!(playbook["urn"], PLAYBOOK);
    assert_eq!(playbook["role"], "default");
    assert_eq!(playbook["requiredCapabilities"][0], "tool:jira.search");
    let required = &playbook["requiredSkills"][0];
    assert_eq!(required["urn"], SKILL);
    assert_eq!(required["version"]["major"], 1);
    assert!(
        required["attached"].is_null() || required["attached"] == false,
        "{required}"
    );
    let skill = &body["skills"][0];
    assert_eq!(skill["urn"], SKILL);
    assert_eq!(skill["capabilities"][1], "egress:jira.example.com");
    assert_eq!(skill["requiredBy"][0], PLAYBOOK);

    // Attach the skill: the same check now reports it attached.
    w.store
        .publish_agent_draft_with_owner(&agent(vec![floating(SKILL)], PLAYBOOK), "alice")
        .unwrap();
    let (status, body) = check(&w.app, &w.alice, draft_ref()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["playbooks"][0]["requiredSkills"][0]["attached"], true);
    assert_eq!(body["skills"][0]["attached"], true);
    assert_eq!(body["skills"].as_array().unwrap().len(), 1, "{body}");
}

#[tokio::test]
async fn released_agent_version_checks_against_its_frozen_refs() {
    let w = world();
    seed_catalog(&w);
    w.store
        .publish_agent_draft_with_owner(&agent(vec![floating(SKILL)], PLAYBOOK), "alice")
        .unwrap();
    w.store
        .release_agent(ALICE_AGENT, &release_version(1))
        .expect("release");
    // Change the draft after release: the released version must not follow it.
    w.store
        .publish_agent_draft_with_owner(&agent(vec![], PLAYBOOK), "alice")
        .unwrap();

    let (status, body) = check(
        &w.app,
        &w.bob,
        json!({"resourceId": {"urn": ALICE_AGENT}, "version": {"major": 1}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["agentRef"]["version"]["major"], 1);
    assert_eq!(body["playbooks"][0]["requiredSkills"][0]["attached"], true);
    // Refs were frozen at release, so the version carries the pinned digest and version.
    assert_eq!(body["playbooks"][0]["version"]["major"], 1);
}

#[tokio::test]
async fn another_users_draft_is_not_found() {
    let w = world();
    seed_catalog(&w);
    w.store
        .publish_agent_draft_with_owner(&agent(vec![], PLAYBOOK), "alice")
        .unwrap();

    let (explicit, _) = check(&w.app, &w.bob, draft_ref()).await;
    assert_eq!(explicit, StatusCode::NOT_FOUND);
    let (floating_ref, _) =
        check(&w.app, &w.bob, json!({"resourceId": {"urn": ALICE_AGENT}})).await;
    assert_eq!(floating_ref, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn unresolved_playbook_is_reported_with_an_error_not_a_failure() {
    let w = world();
    seed_catalog(&w);
    let missing = "urn:vam:playbook:local.vam.dev/acme/not-published";
    w.store
        .publish_agent_draft_with_owner(&agent(vec![], missing), "alice")
        .unwrap();

    let (status, body) = check(&w.app, &w.alice, draft_ref()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let playbook = &body["playbooks"][0];
    assert_eq!(playbook["urn"], missing);
    let error = playbook["error"].as_str().expect("error string");
    assert!(
        error.contains("not-published") || error.contains("playbook"),
        "{error}"
    );
    assert!(playbook["requiredSkills"].is_null(), "{playbook}");
}

#[tokio::test]
async fn allow_any_playbook_is_a_flag_not_an_enumeration() {
    let w = world();
    seed_catalog(&w);
    let mut spec = agent(vec![], PLAYBOOK);
    spec.default_playbook_ref = None;
    spec.allow_any_playbook = true;
    w.store
        .publish_agent_draft_with_owner(&spec, "alice")
        .unwrap();

    let (status, body) = check(&w.app, &w.alice, draft_ref()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["allowAnyPlaybook"], true);
    assert!(body["playbooks"].is_null(), "{body}");
}

#[tokio::test]
async fn playbook_list_carries_records_with_version_and_requires() {
    let w = world();
    seed_catalog(&w);
    let response = w
        .app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/playbooks")
                .header("authorization", format!("Bearer {}", w.alice))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = hyper::body::to_bytes(response.into_body()).await.unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    let record = &body["records"][0];
    assert_eq!(record["spec"]["resourceId"]["urn"], PLAYBOOK);
    assert_eq!(
        record["spec"]["requires"]["skills"][0]["resourceId"]["urn"],
        SKILL
    );
    assert_eq!(record["ownerUserId"], "alice");
    assert!(record["version"]["major"].as_i64().unwrap_or(0) >= 0);
    // The existing field is unchanged.
    assert_eq!(body["specs"][0]["resourceId"]["urn"], PLAYBOOK);
}
