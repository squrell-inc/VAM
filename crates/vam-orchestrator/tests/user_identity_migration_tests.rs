//! An existing user survives the user-identity migration (workstream `user-identity`, UI-2.3).
//!
//! The harness cannot seed a `vamd.db`, so this is where "existing users keep their ids" is
//! proven: a database as a pre-Phase-2 node left it (user `alice` whose id is `alice`, no
//! case-insensitive index, owned catalog and key rows) opens with the migration, and afterwards
//! id, username, owner rows and `defaultNamespace` are unchanged and `ALICE` signs in as alice.

use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;
use vam_core::{CidAllocator, MockVmm};
use vam_orchestrator::api::auth::ControlPlanePdp;
use vam_orchestrator::api::{self, AppState};
use vam_orchestrator::keys::{mint_api_key_secret, MEMBER_SCOPES};
use vam_orchestrator::manager::SessionManager;
use vam_orchestrator::secrets::SecretsVault;
use vam_orchestrator::users::{UserRole, USERNAME_NOCASE_INDEX};
use vam_orchestrator::workflow::SqliteWorkflowStore;
use vam_orchestrator::VmmFactory;
use vam_spec::v1::{agent_spec::InstructionSource, AgentSpec, ResourceId};

const ALICE_PASSWORD: &str = "alice-old-password";
const ALICE_AGENT: &str = "urn:vam:agent:local.vam.dev/alice/legacy-agent";

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

async fn body_text(response: axum::response::Response) -> String {
    String::from_utf8(
        hyper::body::to_bytes(response.into_body())
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap()
}

async fn login(
    app: &axum::Router,
    username: &str,
    password: &str,
) -> (StatusCode, serde_json::Value) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/login")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({ "username": username, "password": password }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let text = body_text(response).await;
    (
        status,
        serde_json::from_str(&text).unwrap_or(serde_json::Value::String(text)),
    )
}

async fn get_me(app: &axum::Router, bearer: &str) -> (StatusCode, serde_json::Value) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/me")
                .header("authorization", format!("Bearer {bearer}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let text = body_text(response).await;
    (
        status,
        serde_json::from_str(&text).unwrap_or(serde_json::Value::String(text)),
    )
}

fn agent(urn: &str) -> AgentSpec {
    AgentSpec {
        resource_id: Some(ResourceId {
            urn: urn.to_string(),
        }),
        display_name: "Legacy agent".to_string(),
        description: "owned by alice before the migration".to_string(),
        role: "tester".to_string(),
        instruction_source: Some(InstructionSource::SystemInstructions(
            "legacy instructions".to_string(),
        )),
        allow_any_playbook: true,
        ..Default::default()
    }
}

fn index_names(path: &Path) -> Vec<String> {
    let conn = rusqlite::Connection::open(path).expect("raw open");
    let mut stmt = conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'index' AND tbl_name = 'users'")
        .expect("prepare");
    let names = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .expect("query")
        .collect::<Result<Vec<_>, _>>()
        .expect("rows");
    names
}

/// Builds the pre-Phase-2 database at `path`: alice (id `alice`) with a password, a released
/// agent and an API key she owns, and no case-insensitive username index. Returns her key.
fn seed_pre_phase_two_db(path: &Path) -> String {
    let key_secret = mint_api_key_secret();
    {
        let store = SqliteWorkflowStore::open(path).expect("seed open");
        store
            .create_user_with_role("alice", "alice", UserRole::Member)
            .expect("alice");
        store
            .reset_user_password("alice", ALICE_PASSWORD)
            .expect("alice password");
        store
            .put_agent_with_owner(&agent(ALICE_AGENT), "alice")
            .expect("alice agent");
        let scopes: Vec<String> = MEMBER_SCOPES.iter().map(|s| (*s).to_string()).collect();
        store
            .create_api_key_owned(
                "key-alice-legacy",
                "alice-legacy",
                &key_secret,
                "User::\"alice\"",
                &scopes,
                "alice",
            )
            .expect("alice key");
    }
    // An older node never made the index: take it away again.
    {
        let conn = rusqlite::Connection::open(path).expect("raw open");
        conn.execute_batch(&format!("DROP INDEX {USERNAME_NOCASE_INDEX};"))
            .expect("drop the index");
    }
    assert!(
        !index_names(path).iter().any(|n| n == USERNAME_NOCASE_INDEX),
        "the seed must look like an old node's database"
    );
    key_secret
}

#[tokio::test]
async fn an_existing_user_keeps_id_name_owner_rows_and_namespace_through_the_migration() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("vamd.db");
    let key_secret = seed_pre_phase_two_db(&path);

    // The migration runs on open.
    let store = Arc::new(SqliteWorkflowStore::open(&path).expect("open with the migration"));
    assert!(store.username_clashes().is_empty());
    assert!(store.username_nocase_index_exists().expect("index"));
    assert!(index_names(&path)
        .iter()
        .any(|n| n == USERNAME_NOCASE_INDEX));

    // Id, username and tenant are what they were: no row was rewritten.
    let alice = store.get_user("alice").expect("alice by id");
    assert_eq!(alice.user_id, "alice");
    assert_eq!(alice.username, "alice");
    assert_eq!(alice.tenant_id, "alice");
    assert_eq!(alice.role, UserRole::Member);
    assert!(alice.has_password);
    // Owned rows still point at her.
    assert_eq!(
        store.agent_owner_user_id(ALICE_AGENT).expect("agent owner"),
        "alice"
    );
    store
        .get_agent_visible(ALICE_AGENT, "alice", false)
        .expect("alice still sees her agent");
    let keys = store.list_api_keys_for_owner("alice").expect("alice keys");
    assert!(
        keys.iter()
            .any(|k| k.key_id == "key-alice-legacy" && k.owner_user_id == "alice"),
        "{keys:?}"
    );
    // No other user appeared.
    assert_eq!(
        store.user_count().expect("count"),
        3,
        "operator, local, alice"
    );

    let app = app_for(store.clone());

    // Her API key still reaches her, and her default namespace is unchanged.
    let (status, me) = get_me(&app, &key_secret).await;
    assert_eq!(status, StatusCode::OK, "{me}");
    assert_eq!(me["userId"], "alice");
    assert_eq!(me["username"], "alice");
    assert_eq!(me["defaultNamespace"], "alice");

    // She signs in as typed, and as ALICE (the new, case-insensitive rule); the session is hers.
    for typed in ["alice", "ALICE", "Alice"] {
        let (status, session) = login(&app, typed, ALICE_PASSWORD).await;
        assert_eq!(status, StatusCode::OK, "{typed}: {session}");
        assert_eq!(session["userId"], "alice", "{typed}");
        assert_eq!(
            session["username"], "alice",
            "{typed}: the stored name, not the typed one"
        );
        let token = session["token"].as_str().expect("token");
        let (status, me) = get_me(&app, token).await;
        assert_eq!(status, StatusCode::OK, "{typed}: {me}");
        assert_eq!(me["userId"], "alice", "{typed}");
        assert_eq!(me["username"], "alice", "{typed}");
        assert_eq!(me["defaultNamespace"], "alice", "{typed}");
    }
    let (status, _) = login(&app, "ALICE", "wrong-password").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // A user created now gets a generated id, and alice's is untouched by it.
    let created = store
        .create_managed_user_generated("alice-two", "two-secret", UserRole::Member)
        .expect("new user");
    assert!(created.user_id.starts_with("usr_"), "{}", created.user_id);
    assert_eq!(store.get_user("alice").expect("alice").username, "alice");
    // A name that differs from hers only in letter case is refused from now on.
    let err = store
        .create_managed_user_generated("ALICE", "twin-secret", UserRole::Member)
        .expect_err("case twin");
    assert!(
        matches!(err, vam_orchestrator::OrchestratorError::Conflict(_)),
        "{err:?}"
    );
}

#[tokio::test]
async fn the_migration_is_idempotent_across_restarts() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("vamd.db");
    let _ = seed_pre_phase_two_db(&path);
    for round in 0..3 {
        let store = SqliteWorkflowStore::open(&path).expect("open");
        assert!(store.username_clashes().is_empty(), "round {round}");
        assert!(
            store.username_nocase_index_exists().expect("index"),
            "round {round}"
        );
        let alice = store.get_user("alice").expect("alice");
        assert_eq!(
            (alice.user_id.as_str(), alice.username.as_str()),
            ("alice", "alice")
        );
        assert_eq!(store.user_count().expect("count"), 3, "round {round}");
        let indexes = index_names(&path);
        assert_eq!(
            indexes
                .iter()
                .filter(|n| *n == USERNAME_NOCASE_INDEX)
                .count(),
            1,
            "{indexes:?}"
        );
    }
}
