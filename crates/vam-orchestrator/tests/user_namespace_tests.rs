//! One namespace per user (workstream `user-identity`, Phase 3, decisions 17 to 21).
//!
//! The scenarios prove two colliding members end up apart through the API. These tests prove
//! what a scenario cannot reach: the backfill of a database an older node left (who keeps a shared
//! namespace, the log lines, drafts that do not move), the same 400 for a free and a taken name in
//! another member's namespace, a rename that keeps the namespace, concurrent creates and the
//! reserved names.

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;
use vam_core::{CidAllocator, MockVmm};
use vam_orchestrator::api::auth::ControlPlanePdp;
use vam_orchestrator::api::{self, AppState};
use vam_orchestrator::keys::{mint_api_key_secret, ScopeMode, MEMBER_SCOPES, ORG_ADMIN_SCOPES};
use vam_orchestrator::manager::SessionManager;
use vam_orchestrator::secrets::SecretsVault;
use vam_orchestrator::users::{NamespaceMove, UserRole, NAMESPACE_INDEX};
use vam_orchestrator::workflow::SqliteWorkflowStore;
use vam_orchestrator::VmmFactory;
use vam_spec::v1::{agent_spec::InstructionSource, AgentSpec, ResourceId};

const DOMAIN: &str = "local.vam.dev";

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

fn urn(namespace: &str, name: &str) -> String {
    format!("urn:vam:agent:{DOMAIN}/{namespace}/{name}")
}

fn agent(urn: &str) -> AgentSpec {
    AgentSpec {
        resource_id: Some(ResourceId {
            urn: urn.to_string(),
        }),
        display_name: "Agent".to_string(),
        description: "namespace test".to_string(),
        role: "tester".to_string(),
        instruction_source: Some(InstructionSource::SystemInstructions(
            "Answer briefly.".to_string(),
        )),
        allow_any_playbook: true,
        ..Default::default()
    }
}

/// A member-scoped key that follows the user's live role.
fn member_key(store: &SqliteWorkflowStore, user_id: &str) -> String {
    key_with(store, user_id, MEMBER_SCOPES)
}

fn key_with(store: &SqliteWorkflowStore, user_id: &str, scopes: &[&str]) -> String {
    let secret = mint_api_key_secret();
    let scopes: Vec<String> = scopes.iter().map(|s| (*s).to_string()).collect();
    store
        .create_api_key_owned_with_mode(
            &format!("key-{user_id}"),
            user_id,
            &secret,
            &format!("User::\"{user_id}\""),
            &scopes,
            user_id,
            ScopeMode::Role,
        )
        .expect("mint member key");
    secret
}

static IDEM: AtomicUsize = AtomicUsize::new(0);

async fn send(
    app: &axum::Router,
    method: &str,
    uri: &str,
    key: &str,
    body: Option<String>,
) -> (StatusCode, serde_json::Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header("authorization", format!("Bearer {key}"))
                .header(
                    "Idempotency-Key",
                    format!("ns-idem-{}", IDEM.fetch_add(1, Ordering::SeqCst)),
                )
                .header("content-type", "application/json")
                .body(body.map(Body::from).unwrap_or_else(Body::empty))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    (
        status,
        serde_json::from_str(&text).unwrap_or(serde_json::Value::String(text)),
    )
}

async fn put_agent(app: &axum::Router, key: &str, urn: &str) -> (StatusCode, serde_json::Value) {
    let body = serde_json::json!({ "spec": {
        "resourceId": { "urn": urn },
        "displayName": "Member agent",
        "description": "namespace test",
        "role": "Tester",
        "systemInstructions": "Answer briefly.",
        "allowAnyPlaybook": true
    }})
    .to_string();
    send(app, "PUT", "/v1/agents", key, Some(body)).await
}

async fn me_namespace(app: &axum::Router, key: &str) -> String {
    let (status, me) = send(app, "GET", "/v1/me", key, None).await;
    assert_eq!(status, StatusCode::OK, "{me}");
    me["defaultNamespace"]
        .as_str()
        .expect("defaultNamespace")
        .to_string()
}

fn namespace_of(store: &SqliteWorkflowStore, user_id: &str) -> String {
    store
        .get_user(user_id)
        .expect("user")
        .namespace
        .expect("a stored namespace")
}

fn index_exists(path: &Path) -> bool {
    let conn = rusqlite::Connection::open(path).expect("raw open");
    conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = ?1",
        [NAMESPACE_INDEX],
        |row| row.get::<_, i64>(0),
    )
    .expect("index query")
        == 1
}

/// A database as a pre-Phase-3 node left it: no `namespace` values and no index.
///
/// - `north` (`ann.lee@north.test`) is the older user, but their draft under `ann.lee` is younger.
/// - `south` (`ann.lee@south.test`) owns the oldest row under `ann.lee`.
/// - `under` (`_x@example.test`) and `dash` (`-y@example.test`) both derived `default`.
/// - `bob` (`bob`) owns nothing; a publisher owns a row under `bob`.
fn seed_pre_phase_three_db(path: &Path) -> (String, String) {
    let (north_key, south_key);
    {
        let store = SqliteWorkflowStore::open(path).expect("seed open");
        for (id, name, role) in [
            ("north", "ann.lee@north.test", UserRole::Member),
            ("south", "ann.lee@south.test", UserRole::Member),
            ("under", "_x@example.test", UserRole::Member),
            ("dash", "-y@example.test", UserRole::Member),
            ("bob", "bob", UserRole::Member),
            ("pub", "pub", UserRole::Publisher),
        ] {
            store.create_user_with_role(id, name, role).expect(id);
        }
        store
            .put_agent_with_owner(&agent(&urn("ann.lee", "south-old")), "south")
            .expect("south draft");
        store
            .put_agent_with_owner(&agent(&urn("ann.lee", "north-old")), "north")
            .expect("north draft");
        store
            .put_agent_with_owner(&agent(&urn("bob", "pub-agent")), "pub")
            .expect("publisher row under bob");
        north_key = member_key(&store, "north");
        south_key = member_key(&store, "south");
    }
    let conn = rusqlite::Connection::open(path).expect("raw open");
    conn.execute_batch(&format!(
        "DROP INDEX {NAMESPACE_INDEX};
         UPDATE users SET namespace = NULL;
         UPDATE users SET created_at = 100 WHERE user_id = 'north';
         UPDATE users SET created_at = 200 WHERE user_id = 'south';
         UPDATE agents SET created_at = 10 WHERE urn LIKE '%/south-old';
         UPDATE agents SET created_at = 20 WHERE urn LIKE '%/north-old';"
    ))
    .expect("make it look like an older node's database");
    (north_key, south_key)
}

#[tokio::test]
async fn the_backfill_keeps_the_first_claimant_suffixes_the_rest_and_moves_no_draft() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("vamd.db");
    let (north_key, south_key) = seed_pre_phase_three_db(&path);
    assert!(!index_exists(&path));

    let store = Arc::new(SqliteWorkflowStore::open(&path).expect("open with the backfill"));
    assert!(index_exists(&path));

    // The owner of the oldest row under `ann.lee` keeps it, though they are the younger user.
    assert_eq!(namespace_of(&store, "south"), "ann.lee");
    let north = namespace_of(&store, "north");
    assert_eq!(north, vam_spec::namespace_suffixed("ann.lee", "north", 6));
    // `default` is nobody's any more.
    assert_eq!(namespace_of(&store, "under"), "x");
    assert_eq!(namespace_of(&store, "dash"), "y");
    // A publisher's row under `bob` keeps member `bob` out of it.
    assert_eq!(
        namespace_of(&store, "bob"),
        vam_spec::namespace_suffixed("bob", "bob", 6)
    );
    assert_eq!(namespace_of(&store, "pub"), "pub");
    // Seeds keep their values.
    assert_eq!(namespace_of(&store, "operator"), "operator");
    assert_eq!(namespace_of(&store, "local"), "local");

    // One log line per user moved, and only those.
    let mut moved: Vec<NamespaceMove> = store.namespace_moves();
    moved.sort_by(|a, b| a.user_id.cmp(&b.user_id));
    let ids: Vec<&str> = moved.iter().map(|m| m.user_id.as_str()).collect();
    assert_eq!(ids, ["bob", "dash", "north", "under"], "{moved:?}");
    let north_move = moved.iter().find(|m| m.user_id == "north").unwrap();
    assert_eq!(
        north_move.log_line(),
        format!(
            "namespace for user north (ann.lee@north.test) is now {north}; was shared as ann.lee"
        )
    );

    // Through the API: each reports their own, and north keeps editing the draft left in the
    // shared namespace, but saves new agents only under the new one.
    let app = app_for(store.clone());
    assert_eq!(me_namespace(&app, &north_key).await, north);
    assert_eq!(me_namespace(&app, &south_key).await, "ann.lee");
    let (status, body) = put_agent(&app, &north_key, &urn("ann.lee", "north-old")).await;
    assert_eq!(status, StatusCode::OK, "an edit of an own draft: {body}");
    assert_eq!(
        store
            .agent_owner_user_id(&urn("ann.lee", "north-old"))
            .unwrap(),
        "north"
    );
    let (status, body) = put_agent(&app, &north_key, &urn("ann.lee", "north-new")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["message"]
            .as_str()
            .unwrap()
            .contains(&format!("urn:vam:agent:{DOMAIN}/{north}/ (your namespace)")),
        "{body}"
    );
    let (status, body) = put_agent(&app, &north_key, &urn(&north, "north-new")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = put_agent(&app, &south_key, &urn("ann.lee", "south-new")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    drop(app);
    drop(store);

    // Idempotent: a second open changes nothing and logs nothing.
    let again = SqliteWorkflowStore::open(&path).expect("reopen");
    assert!(again.namespace_moves().is_empty());
    assert_eq!(namespace_of(&again, "north"), north);
    assert_eq!(namespace_of(&again, "south"), "ann.lee");
    assert_eq!(namespace_of(&again, "under"), "x");
}

#[tokio::test]
async fn a_free_and_a_taken_name_in_another_members_namespace_answer_alike() {
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let one = store
        .create_managed_user_generated("ann.lee@north.test", "pw-one-secret", UserRole::Member)
        .unwrap();
    let two = store
        .create_managed_user_generated("ann.lee@south.test", "pw-two-secret", UserRole::Member)
        .unwrap();
    let ns_one = one.namespace.clone().unwrap();
    let ns_two = two.namespace.clone().unwrap();
    assert_eq!(ns_one, "ann.lee");
    assert_eq!(
        ns_two,
        vam_spec::namespace_suffixed("ann.lee", &two.user_id, 6)
    );
    let key_one = member_key(&store, &one.user_id);
    let key_two = member_key(&store, &two.user_id);
    let app = app_for(store.clone());

    let taken = urn(&ns_one, "held");
    let (status, body) = put_agent(&app, &key_one, &taken).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (taken_status, taken_body) = put_agent(&app, &key_two, &taken).await;
    let free = urn(&ns_one, "never-saved");
    let (free_status, free_body) = put_agent(&app, &key_two, &free).await;
    assert_eq!(taken_status, StatusCode::BAD_REQUEST, "{taken_body}");
    assert_eq!(free_status, StatusCode::BAD_REQUEST, "{free_body}");
    let strip = |body: &serde_json::Value, urn: &str| {
        body["message"].as_str().unwrap().replace(urn, "<urn>")
    };
    assert_eq!(strip(&taken_body, &taken), strip(&free_body, &free));
    assert_eq!(taken_body["code"], free_body["code"]);

    // The second member saves under their own.
    let (status, body) = put_agent(&app, &key_two, &urn(&ns_two, "mine")).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // An org admin sees each user's namespace on the user list (open question 2).
    store
        .create_user_with_role("admin", "admin", UserRole::OrgAdmin)
        .unwrap();
    let admin = key_with(&store, "admin", ORG_ADMIN_SCOPES);
    let (status, list) = send(&app, "GET", "/v1/users", &admin, None).await;
    assert_eq!(status, StatusCode::OK, "{list}");
    let users = list["users"].as_array().expect("users");
    for (id, ns) in [(&one.user_id, &ns_one), (&two.user_id, &ns_two)] {
        let row = users
            .iter()
            .find(|u| u["userId"] == id.as_str())
            .expect("listed");
        assert_eq!(row["namespace"], ns.as_str(), "{row}");
    }
}

#[tokio::test]
async fn a_rename_keeps_the_namespace() {
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let user = store
        .create_managed_user_generated("ann.lee@north.test", "pw-ann-secret", UserRole::Member)
        .unwrap();
    let key = member_key(&store, &user.user_id);
    let app = app_for(store.clone());
    assert_eq!(me_namespace(&app, &key).await, "ann.lee");
    let renamed = store
        .update_username(&user.user_id, "someone.else@example.test")
        .unwrap();
    assert_eq!(renamed.namespace.as_deref(), Some("ann.lee"));
    assert_eq!(me_namespace(&app, &key).await, "ann.lee");
    // A new user with the old name's base does not get it.
    let other = store
        .create_managed_user_generated("ann.lee@south.test", "pw-other-secret", UserRole::Member)
        .unwrap();
    assert_ne!(other.namespace.as_deref(), Some("ann.lee"));
}

#[test]
fn concurrent_creates_with_one_base_get_different_namespaces() {
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let handles: Vec<_> = (0..8)
        .map(|i| {
            let store = store.clone();
            std::thread::spawn(move || {
                store
                    .create_managed_user_generated(
                        &format!("ann@domain-{i}.test"),
                        "pw-concurrent",
                        UserRole::Member,
                    )
                    .expect("create")
                    .namespace
                    .expect("namespace")
            })
        })
        .collect();
    let mut namespaces: Vec<String> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    namespaces.sort();
    let mut unique = namespaces.clone();
    unique.dedup();
    assert_eq!(unique.len(), 8, "{namespaces:?}");
    assert_eq!(namespaces.iter().filter(|n| *n == "ann").count(), 1);
    assert!(namespaces
        .iter()
        .all(|n| n == "ann" || (n.starts_with("ann-") && n.len() == "ann-".len() + 6)));
}

#[test]
fn reserved_bases_and_another_owners_rows_are_never_given() {
    let store = SqliteWorkflowStore::in_memory().unwrap();
    for name in [
        "default@x.test",
        "Operator@x.test",
        "local@x.test",
        "BOOTSTRAP@x.test",
    ] {
        let user = store
            .create_managed_user_generated(name, "pw-reserved", UserRole::Member)
            .unwrap();
        let base = name.split('@').next().unwrap().to_ascii_lowercase();
        assert_eq!(
            user.namespace.as_deref(),
            Some(vam_spec::namespace_suffixed(&base, &user.user_id, 6).as_str()),
            "{name}"
        );
    }
    store
        .create_user_with_role("pub", "pub", UserRole::Publisher)
        .unwrap();
    store
        .put_agent_with_owner(&agent(&urn("acme", "shared")), "pub")
        .unwrap();
    let member = store
        .create_managed_user_generated("acme@x.test", "pw-acme", UserRole::Member)
        .unwrap();
    assert_eq!(
        member.namespace.as_deref(),
        Some(vam_spec::namespace_suffixed("acme", &member.user_id, 6).as_str())
    );
    // A name that is not a slug gets a readable namespace of its own.
    let odd = store
        .create_managed_user_generated("_Ann-1a2b@Example.Test", "pw-odd", UserRole::Member)
        .unwrap();
    assert_eq!(odd.namespace.as_deref(), Some("ann-1a2b"));
}
