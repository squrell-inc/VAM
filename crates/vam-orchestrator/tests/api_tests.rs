// PROCESS_STATE_LOCK is deliberately held for the whole test (process-global state); not a deadlock risk.
#![allow(clippy::await_holding_lock)]
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tokio::net::UnixStream;
use tower::ServiceExt;
use vam_core::{CidAllocator, MockVmm};
use vam_network::NetResourcePool;
use vam_orchestrator::api::auth::ControlPlanePdp;
use vam_orchestrator::api::{self, AppState};
use vam_orchestrator::keys::{
    hash_api_key_secret, mint_api_key_secret, BOOTSTRAP_SCOPES, MEMBER_SCOPES, ORG_ADMIN_SCOPES,
    PUBLISHER_SCOPES,
};
use vam_orchestrator::manager::{drive9_runtime, run_manager_loop, SessionManager};
use vam_orchestrator::secrets::SecretsVault;
use vam_orchestrator::workflow::SqliteWorkflowStore;
use vam_orchestrator::{
    UserRole, VmmFactory, BOOTSTRAP_KEY_ID, BOOTSTRAP_PRINCIPAL, LOCAL_USER_ID, OPERATOR_USER_ID,
};
use vam_spec::v1::{PlaybookSpec, PlaybookStep, ResourceId, StepMode};

/// Tests that run real session VMs share `/tmp` vsock sockets, and the mock-guest
/// helper below accepts the first session socket it finds. It can therefore
/// register a guest for another test's session. Every test holds this lock so
/// only one test's sessions exist at a time.
static PROCESS_STATE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Points drive9 and snapshot state at a fresh tempdir for one test. The
/// production default is root-owned `/var/lib/vam`, which a non-root CI runner
/// cannot create, so these tests must never fall through to it.
fn isolate_state() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("state tempdir");
    // Sessions spawned in-process skip vamd's startup, which is what installs the node
    // keys. Production stays fail-closed; the tests install throwaway in-memory keys
    // (a process-wide one-shot, so later calls are no-ops).
    let _ = vam_orchestrator::signing::install(vam_orchestrator::signing::NodeKeys::ephemeral());
    let drive9 = dir.path().join("drive9");
    let snapshots = dir.path().join("snapshots");
    // Create the roots up front, as `drive9_root()` does for the default path.
    std::fs::create_dir_all(&drive9).expect("drive9 root");
    std::fs::create_dir_all(&snapshots).expect("snapshot root");
    std::env::set_var("VAM_DRIVE9_ROOT", &drive9);
    std::env::set_var("VAM_SNAPSHOT_ROOT", &snapshots);
    dir
}

fn test_app(state: AppState) -> axum::Router {
    api::router(state)
}

/// Completes the host↔guest registration handshake for the next server-minted
/// session (`vm-sess-*`). Needed because HTTP create awaits guest registration.
fn spawn_mock_guest_for_server_minted_session() {
    tokio::spawn(async move {
        let prefix = "vam-vsock-vm-sess-";
        let suffix = ".socket_52";
        for _ in 0..250 {
            if let Ok(entries) = std::fs::read_dir("/tmp") {
                for entry in entries.flatten() {
                    let name = entry.file_name().to_string_lossy().to_string();
                    if !(name.starts_with(prefix) && name.ends_with(suffix)) {
                        continue;
                    }
                    let vm_id = name
                        .trim_start_matches("vam-vsock-")
                        .trim_end_matches(suffix)
                        .to_string();
                    if let Ok(stream) = UnixStream::connect(entry.path()).await {
                        let _ = vam_vsock::run_guest_registration(stream, &vm_id, 3).await;
                        return;
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    });
}

fn bootstrap_store() -> (Arc<SqliteWorkflowStore>, String) {
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    store.initialize_catalog_schema().unwrap();
    let secret = mint_api_key_secret();
    let scopes: Vec<String> = BOOTSTRAP_SCOPES.iter().map(|s| s.to_string()).collect();
    store
        .create_api_key("test-key", "test", &secret, "User::\"tester\"", &scopes)
        .unwrap();
    (store, secret)
}

/// Publishes and releases the playbook that the run fixtures reference.
/// Run creation resolves playbook refs against the catalog. The fixtures pin
/// the empty-digest placeholder, so an unseeded catalog rejects them with 400.
fn seed_test_playbook(store: &SqliteWorkflowStore) {
    let playbook = PlaybookSpec {
        activation: Some(vam_spec::v1::PlaybookActivation::asking(
            "Use when the request matches this test playbook.",
        )),
        resource_id: Some(ResourceId {
            urn: "urn:vam:playbook:local.vam.dev/acme/test-pb".to_string(),
        }),
        display_name: "Test playbook".to_string(),
        description: "Playbook referenced by the API test run fixtures".to_string(),
        steps: vec![PlaybookStep {
            id: "s1".to_string(),
            description: "step".to_string(),
            instruction: "Do the step.".to_string(),
            mode: StepMode::Agentic as i32,
            ..Default::default()
        }],
        ..Default::default()
    };
    store
        .put_playbook_with_owner(&playbook, LOCAL_USER_ID)
        .expect("seed test playbook");
}

/// Same shape as [`seed_test_playbook`], but its input schema is strict and
/// declares only `invoice_url`, like a real playbook the app chats through.
fn seed_strict_playbook(store: &SqliteWorkflowStore) {
    use prost_types::value::Kind;
    use prost_types::{ListValue, Struct, Value};

    fn text(value: &str) -> Value {
        Value {
            kind: Some(Kind::StringValue(value.to_string())),
        }
    }
    fn object(fields: Vec<(&str, Value)>) -> Value {
        let fields = fields
            .into_iter()
            .map(|(key, value)| (key.to_string(), value))
            .collect();
        Value {
            kind: Some(Kind::StructValue(Struct { fields })),
        }
    }

    let input_schema = Struct {
        fields: [
            ("type".to_string(), text("object")),
            (
                "additionalProperties".to_string(),
                Value {
                    kind: Some(Kind::BoolValue(false)),
                },
            ),
            (
                "required".to_string(),
                Value {
                    kind: Some(Kind::ListValue(ListValue {
                        values: vec![text("invoice_url")],
                    })),
                },
            ),
            (
                "properties".to_string(),
                object(vec![(
                    "invoice_url",
                    object(vec![("type", text("string"))]),
                )]),
            ),
        ]
        .into_iter()
        .collect(),
    };
    let playbook = PlaybookSpec {
        activation: Some(vam_spec::v1::PlaybookActivation::asking(
            "Use when the request matches this test playbook.",
        )),
        resource_id: Some(ResourceId {
            urn: "urn:vam:playbook:local.vam.dev/acme/strict-pb".to_string(),
        }),
        display_name: "Strict playbook".to_string(),
        description: "Playbook with a strict input schema".to_string(),
        input_schema: Some(input_schema),
        steps: vec![PlaybookStep {
            id: "s1".to_string(),
            description: "step".to_string(),
            instruction: "Do the step.".to_string(),
            mode: StepMode::Agentic as i32,
            ..Default::default()
        }],
        ..Default::default()
    };
    store
        .put_playbook_with_owner(&playbook, LOCAL_USER_ID)
        .expect("seed strict playbook");
}

fn app_state(store: Arc<SqliteWorkflowStore>) -> AppState {
    app_state_with_manager(store, true)
}

fn app_state_http_only(store: Arc<SqliteWorkflowStore>) -> AppState {
    app_state_with_manager(store, false)
}

fn app_state_with_manager(store: Arc<SqliteWorkflowStore>, spawn_manager: bool) -> AppState {
    if std::env::var("VAM_DRIVE9_ROOT").is_err() {
        std::env::set_var("VAM_DRIVE9_ROOT", "/tmp/vam-test-drive9");
    }
    let vault = SecretsVault::for_tests(store.clone());
    let cid_allocator = Arc::new(CidAllocator::default());
    let vmm_factory: VmmFactory = Arc::new(|_vm_id, _guest_cid| Box::new(MockVmm::new()));
    let (manager, cmd_rx) =
        SessionManager::new(store.clone(), cid_allocator.clone(), vmm_factory.clone());
    if spawn_manager {
        let (workspace_manager, catalog_cache) =
            drive9_runtime().expect("drive9 runtime should initialize");
        let net_pool = Arc::new(NetResourcePool::for_tests());
        tokio::spawn(run_manager_loop(
            store.clone(),
            vault.clone(),
            cid_allocator,
            vmm_factory,
            workspace_manager,
            catalog_cache,
            net_pool,
            cmd_rx,
            vam_telemetry::TelemetryCapture::default(),
        ));
    }
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

#[tokio::test]
async fn owner_stamped_on_http_create_key() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let bootstrap_secret = mint_api_key_secret();
    let scopes: Vec<String> = BOOTSTRAP_SCOPES.iter().map(|s| s.to_string()).collect();
    store
        .create_api_key(
            BOOTSTRAP_KEY_ID,
            "bootstrap",
            &bootstrap_secret,
            BOOTSTRAP_PRINCIPAL,
            &scopes,
        )
        .unwrap();
    let app = test_app(app_state_http_only(store.clone()));
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/keys")
                .header("authorization", format!("Bearer {}", bootstrap_secret))
                .header("Idempotency-Key", "owner-create-key")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"name":"alice","scopes":["node:read"]}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = response_body(response).await;
    assert_eq!(status, StatusCode::OK, "{}", body);
    let minted = store
        .list_api_keys()
        .unwrap()
        .into_iter()
        .find(|k| k.name == "alice")
        .expect("minted key");
    assert_eq!(minted.owner_user_id, OPERATOR_USER_ID);
}

#[tokio::test]
async fn keys_mint_attaches_user_id() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let bootstrap_secret = mint_api_key_secret();
    let scopes: Vec<String> = BOOTSTRAP_SCOPES.iter().map(|s| s.to_string()).collect();
    store
        .create_api_key(
            BOOTSTRAP_KEY_ID,
            "bootstrap",
            &bootstrap_secret,
            BOOTSTRAP_PRINCIPAL,
            &scopes,
        )
        .unwrap();
    let admin_user = store
        .bootstrap_first_admin("admin", "headless-admin-secret")
        .unwrap();
    let app = test_app(app_state_http_only(store.clone()));
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/keys")
                .header("authorization", format!("Bearer {}", bootstrap_secret))
                .header("Idempotency-Key", "per-user-create-key")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"name":"admin-ci","userId":"admin","scopes":["node:read","keys:admin"]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = response_body(response).await;
    assert_eq!(status, StatusCode::OK, "{}", body);
    assert!(body.contains("admin"), "{body}");
    assert!(!body.contains("keys:admin"), "{body}");
    let minted = store
        .list_api_keys()
        .unwrap()
        .into_iter()
        .find(|k| k.name == "admin-ci")
        .expect("minted key");
    // `userId` named the login name; the key is owned by the resolved, generated id.
    assert_eq!(minted.owner_user_id, admin_user.user_id);
    assert!(!minted.scopes.iter().any(|s| s == "keys:admin"));
    assert!(minted.scopes.iter().any(|s| s == "node:read"));
}

#[tokio::test]
async fn keys_bootstrap_operator_still_works_after_per_user_mint() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let bootstrap_secret = mint_api_key_secret();
    let scopes: Vec<String> = BOOTSTRAP_SCOPES.iter().map(|s| s.to_string()).collect();
    store
        .create_api_key(
            BOOTSTRAP_KEY_ID,
            "bootstrap",
            &bootstrap_secret,
            BOOTSTRAP_PRINCIPAL,
            &scopes,
        )
        .unwrap();
    let app = test_app(app_state_http_only(store.clone()));
    let minted = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/keys")
                .header("authorization", format!("Bearer {}", bootstrap_secret))
                .header("Idempotency-Key", "ci-style-mint")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"name":"ci","userId":"local","scopes":["node:read"]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        minted.status(),
        StatusCode::OK,
        "{}",
        response_body(minted).await
    );

    let node = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/node")
                .header("authorization", format!("Bearer {}", bootstrap_secret))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(node.status(), StatusCode::OK);
    let auth = store
        .authenticate_bearer(&bootstrap_secret)
        .expect("bootstrap VAM_API_KEY still authenticates");
    assert_eq!(auth.record.key_id, BOOTSTRAP_KEY_ID);
    assert_eq!(auth.record.owner_user_id, OPERATOR_USER_ID);
    assert!(auth.record.scopes.iter().any(|s| s == "keys:admin"));
}

#[tokio::test]
async fn owner_stamped_on_http_put_secret_and_catalog() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let bootstrap_secret = mint_api_key_secret();
    let local_secret = mint_api_key_secret();
    let bootstrap_scopes: Vec<String> = BOOTSTRAP_SCOPES.iter().map(|s| s.to_string()).collect();
    store
        .create_api_key(
            BOOTSTRAP_KEY_ID,
            "bootstrap",
            &bootstrap_secret,
            BOOTSTRAP_PRINCIPAL,
            &bootstrap_scopes,
        )
        .unwrap();
    // Catalog visibility is owner-scoped unless the caller has `users:admin`.
    let local_scopes: Vec<String> = ["catalog:write", "secrets:write"]
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    store
        .create_api_key(
            "other-key",
            "other",
            &local_secret,
            "User::\"tester\"",
            &local_scopes,
        )
        .unwrap();
    let app = test_app(app_state_http_only(store.clone()));

    let put_secret = |secret: &str, binding: &str, idem: &str| {
        let body = serde_json::json!({
            "bindingId": binding,
            "value": format!("sk-{binding}"),
            "kind": 1,
            "idempotencyKey": idem
        });
        Request::builder()
            .method("PUT")
            .uri("/v1/secrets")
            .header("authorization", format!("Bearer {}", secret))
            .header("Idempotency-Key", idem)
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    };

    let op_secret = app
        .clone()
        .oneshot(put_secret(&bootstrap_secret, "openai/api-key", "sec-op"))
        .await
        .unwrap();
    assert_eq!(op_secret.status(), StatusCode::OK);
    let loc_secret = app
        .clone()
        .oneshot(put_secret(&local_secret, "anthropic/api-key", "sec-loc"))
        .await
        .unwrap();
    assert_eq!(loc_secret.status(), StatusCode::OK);
    assert_eq!(
        store
            .host_secret_owner_user_id(
                vam_orchestrator::SecretScope::Org,
                vam_orchestrator::ORG_OWNER_USER_ID,
                "openai/api-key",
            )
            .unwrap(),
        vam_orchestrator::ORG_OWNER_USER_ID
    );
    assert_eq!(
        store
            .host_secret_owner_user_id(
                vam_orchestrator::SecretScope::User,
                LOCAL_USER_ID,
                "anthropic/api-key",
            )
            .unwrap(),
        LOCAL_USER_ID
    );

    let get_secret = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/secrets/openai/api-key?scope=org")
                .header("authorization", format!("Bearer {}", local_secret))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(get_secret.status(), StatusCode::OK);
    let get_body = response_body(get_secret).await;
    assert!(get_body.contains("openai/api-key"), "{get_body}");

    let list_secrets = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/secrets")
                .header("authorization", format!("Bearer {}", local_secret))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let list_secrets_body = response_body(list_secrets).await;
    assert!(
        list_secrets_body.contains("openai/api-key"),
        "{list_secrets_body}"
    );
    assert!(
        list_secrets_body.contains("anthropic/api-key"),
        "{list_secrets_body}"
    );

    let agent_op = "urn:vam:agent:local.vam.dev/acme/owner-op";
    let agent_loc = "urn:vam:agent:local.vam.dev/acme/owner-loc";
    put_test_agent(&app, &bootstrap_secret, agent_op, "Owner Op").await;
    put_test_agent(&app, &local_secret, agent_loc, "Owner Loc").await;
    assert_eq!(
        store.agent_owner_user_id(agent_op).unwrap(),
        OPERATOR_USER_ID
    );
    assert_eq!(store.agent_owner_user_id(agent_loc).unwrap(), LOCAL_USER_ID);

    let list_agents = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/agents")
                .header("authorization", format!("Bearer {}", local_secret))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let list_agents_body = response_body(list_agents).await;
    assert!(
        list_agents_body.contains(agent_loc),
        "local must see own draft: {list_agents_body}"
    );
    assert!(
        !list_agents_body.contains(agent_op),
        "local must not see operator draft: {list_agents_body}"
    );
}

#[tokio::test]
async fn owner_filters_sessions_and_runs_over_http() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let (store, secret) = bootstrap_store();
    store
        .create_session_with_owner("sess-op", None, "vm-op", "ws", "default", OPERATOR_USER_ID)
        .unwrap();
    store
        .create_session_with_owner("sess-loc", None, "vm-loc", "ws", "default", LOCAL_USER_ID)
        .unwrap();
    store
        .persist_run_spec(
            "urn:vam:run:local.vam.dev/acme/run-op",
            "{}",
            Some("sess-op"),
            "urn:vam:agent:op",
            Some("http-owner-op"),
            BOOTSTRAP_PRINCIPAL,
        )
        .unwrap();
    store
        .persist_run_spec(
            "urn:vam:run:local.vam.dev/acme/run-loc",
            "{}",
            Some("sess-loc"),
            "urn:vam:agent:loc",
            Some("http-owner-loc"),
            "User::\"tester\"",
        )
        .unwrap();
    store
        .create_api_key(
            BOOTSTRAP_KEY_ID,
            "bootstrap",
            &mint_api_key_secret(),
            BOOTSTRAP_PRINCIPAL,
            &BOOTSTRAP_SCOPES
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>(),
        )
        .unwrap();

    let app = test_app(app_state_http_only(store));

    let sessions = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/sessions")
                .header("authorization", format!("Bearer {}", secret))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let sessions_status = sessions.status();
    let sessions_body = response_body(sessions).await;
    assert_eq!(sessions_status, StatusCode::OK, "{sessions_body}");
    assert!(
        !sessions_body.contains("sess-op"),
        "local must not list operator session: {sessions_body}"
    );
    assert!(sessions_body.contains("sess-loc"), "{sessions_body}");

    let get_foreign = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/sessions/sess-op")
                .header("authorization", format!("Bearer {}", secret))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(get_foreign.status(), StatusCode::NOT_FOUND);

    let get_own = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/sessions/sess-loc")
                .header("authorization", format!("Bearer {}", secret))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(get_own.status(), StatusCode::OK);

    let runs = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/runs")
                .header("authorization", format!("Bearer {}", secret))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let runs_status = runs.status();
    let runs_body = response_body(runs).await;
    assert_eq!(runs_status, StatusCode::OK, "{runs_body}");
    assert!(
        !runs_body.contains("run-op"),
        "local must not list operator run: {runs_body}"
    );
    assert!(runs_body.contains("run-loc"), "{runs_body}");

    let approvals = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/approvals")
                .header("authorization", format!("Bearer {}", secret))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(approvals.status(), StatusCode::OK);

    let keys = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/keys")
                .header("authorization", format!("Bearer {}", secret))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let keys_body = response_body(keys).await;
    assert!(
        keys_body.contains("test-key") || keys_body.contains("test"),
        "{keys_body}"
    );
    assert!(keys_body.contains("bootstrap"), "{keys_body}");
}

#[tokio::test]
async fn isolation_two_users_cannot_read_each_others_rows() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    store.create_user("alice", "alice").unwrap();
    store.create_user("bob", "bob").unwrap();
    let scopes: Vec<String> = MEMBER_SCOPES.iter().map(|s| s.to_string()).collect();
    let alice_secret = mint_api_key_secret();
    let bob_secret = mint_api_key_secret();
    store
        .create_api_key_owned(
            "alice-key",
            "alice",
            &alice_secret,
            "User::\"alice\"",
            &scopes,
            "alice",
        )
        .unwrap();
    store
        .create_api_key_owned(
            "bob-key",
            "bob",
            &bob_secret,
            "User::\"bob\"",
            &scopes,
            "bob",
        )
        .unwrap();

    store
        .create_session_with_owner("sess-alice", None, "vm-alice", "ws-a", "default", "alice")
        .unwrap();
    store
        .create_session_with_owner("sess-bob", None, "vm-bob", "ws-b", "default", "bob")
        .unwrap();
    let alice_run = "urn:vam:run:local.vam.dev/acme/run-alice";
    let bob_run = "urn:vam:run:local.vam.dev/acme/run-bob";
    store
        .persist_run_spec_with_owner(
            alice_run,
            "{}",
            Some("sess-alice"),
            "urn:vam:agent:alice",
            Some("iso-alice"),
            "User::\"alice\"",
            "alice",
        )
        .unwrap();
    store
        .persist_run_spec_with_owner(
            bob_run,
            "{}",
            Some("sess-bob"),
            "urn:vam:agent:bob",
            Some("iso-bob"),
            "User::\"bob\"",
            "bob",
        )
        .unwrap();
    store.update_run_state(bob_run, "RUNNING").unwrap();

    let app = test_app(app_state_http_only(store));
    let bob_run_path = encode_run_path(bob_run);

    let list_sessions = app
        .clone()
        .oneshot(bearer_get("/v1/sessions", &alice_secret))
        .await
        .unwrap();
    let list_sessions_status = list_sessions.status();
    let list_sessions_body = response_body(list_sessions).await;
    assert_eq!(list_sessions_status, StatusCode::OK, "{list_sessions_body}");
    assert!(
        list_sessions_body.contains("sess-alice"),
        "{list_sessions_body}"
    );
    assert!(
        !list_sessions_body.contains("sess-bob"),
        "{list_sessions_body}"
    );

    let get_bob_session = app
        .clone()
        .oneshot(bearer_get("/v1/sessions/sess-bob", &alice_secret))
        .await
        .unwrap();
    assert_eq!(get_bob_session.status(), StatusCode::NOT_FOUND);

    let list_runs = app
        .clone()
        .oneshot(bearer_get("/v1/runs", &alice_secret))
        .await
        .unwrap();
    let list_runs_status = list_runs.status();
    let list_runs_body = response_body(list_runs).await;
    assert_eq!(list_runs_status, StatusCode::OK, "{list_runs_body}");
    assert!(list_runs_body.contains("run-alice"), "{list_runs_body}");
    assert!(!list_runs_body.contains("run-bob"), "{list_runs_body}");

    let get_bob_run = app
        .clone()
        .oneshot(bearer_get(
            &format!("/v1/runs/{bob_run_path}"),
            &alice_secret,
        ))
        .await
        .unwrap();
    assert_eq!(get_bob_run.status(), StatusCode::NOT_FOUND);

    let get_bob_events = app
        .clone()
        .oneshot(bearer_get(
            &format!("/v1/runs/{bob_run_path}/events"),
            &alice_secret,
        ))
        .await
        .unwrap();
    assert_eq!(get_bob_events.status(), StatusCode::NOT_FOUND);

    let get_bob_evidence = app
        .clone()
        .oneshot(bearer_get(
            &format!("/v1/runs/{bob_run_path}/evidence"),
            &alice_secret,
        ))
        .await
        .unwrap();
    assert_eq!(get_bob_evidence.status(), StatusCode::NOT_FOUND);

    let get_bob_trajectory = app
        .clone()
        .oneshot(bearer_get(
            &format!("/v1/runs/{bob_run_path}/trajectory"),
            &alice_secret,
        ))
        .await
        .unwrap();
    assert_eq!(get_bob_trajectory.status(), StatusCode::NOT_FOUND);

    let bob_evidence = app
        .clone()
        .oneshot(bearer_get(
            &format!("/v1/runs/{bob_run_path}/evidence"),
            &bob_secret,
        ))
        .await
        .unwrap();
    assert_eq!(
        bob_evidence.status(),
        StatusCode::CONFLICT,
        "owner of an unfinished run still gets 409, not a cross-user 404"
    );

    let bob_trajectory = app
        .oneshot(bearer_get(
            &format!("/v1/runs/{bob_run_path}/trajectory"),
            &bob_secret,
        ))
        .await
        .unwrap();
    assert_eq!(bob_trajectory.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn isolation_two_users_cannot_list_or_decide_each_others_approvals_or_keys() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    store.create_user("alice", "alice").unwrap();
    store.create_user("bob", "bob").unwrap();
    let scopes: Vec<String> = MEMBER_SCOPES.iter().map(|s| s.to_string()).collect();
    let alice_secret = mint_api_key_secret();
    let bob_secret = mint_api_key_secret();
    store
        .create_api_key_owned(
            "alice-key",
            "alice",
            &alice_secret,
            "User::\"alice\"",
            &scopes,
            "alice",
        )
        .unwrap();
    store
        .create_api_key_owned(
            "bob-key",
            "bob",
            &bob_secret,
            "User::\"bob\"",
            &scopes,
            "bob",
        )
        .unwrap();

    let alice_run = "urn:vam:run:local.vam.dev/acme/run-alice-hitl";
    let bob_run = "urn:vam:run:local.vam.dev/acme/run-bob-hitl";
    store
        .persist_run_spec_with_owner(
            alice_run,
            "{}",
            None,
            "urn:vam:agent:alice",
            Some("iso-alice-hitl"),
            "User::\"alice\"",
            "alice",
        )
        .unwrap();
    store
        .persist_run_spec_with_owner(
            bob_run,
            "{}",
            None,
            "urn:vam:agent:bob",
            Some("iso-bob-hitl"),
            "User::\"bob\"",
            "bob",
        )
        .unwrap();

    store
        .record_published_approval(
            "appr-alice",
            alice_run,
            "alice",
            r#"{"title":"alice-secret"}"#,
        )
        .unwrap();
    store
        .record_published_approval("appr-bob", bob_run, "bob", r#"{"title":"bob-secret"}"#)
        .unwrap();

    let alice_row = store.get_approval("appr-alice").unwrap();
    assert_eq!(alice_row.approver_user_id, "alice");
    assert!(
        vam_orchestrator::HitlApprover::parse(&alice_row.approver_user_id).allows_decider("alice")
    );
    assert!(
        !vam_orchestrator::HitlApprover::parse(&alice_row.approver_user_id).allows_decider("bob")
    );

    let app = test_app(app_state_http_only(store));

    let list = app
        .clone()
        .oneshot(bearer_get("/v1/approvals", &alice_secret))
        .await
        .unwrap();
    let list_status = list.status();
    let list_body = response_body(list).await;
    assert_eq!(list_status, StatusCode::OK, "{list_body}");
    assert!(list_body.contains("appr-alice"), "{list_body}");
    assert!(!list_body.contains("appr-bob"), "{list_body}");
    assert!(!list_body.contains("bob-secret"), "{list_body}");

    let get_bob = app
        .clone()
        .oneshot(bearer_get("/v1/approvals/appr-bob", &alice_secret))
        .await
        .unwrap();
    let get_bob_status = get_bob.status();
    let get_bob_body = response_body(get_bob).await;
    assert_eq!(get_bob_status, StatusCode::NOT_FOUND, "{get_bob_body}");
    assert!(
        !get_bob_body.contains("bob-secret"),
        "alice must not see bob requested_args: {get_bob_body}"
    );
    assert!(
        !get_bob_body.contains("requestedArgs") && !get_bob_body.contains("requested_args"),
        "{get_bob_body}"
    );

    let get_own = app
        .clone()
        .oneshot(bearer_get("/v1/approvals/appr-alice", &alice_secret))
        .await
        .unwrap();
    let get_own_status = get_own.status();
    let get_own_body = response_body(get_own).await;
    assert_eq!(get_own_status, StatusCode::OK, "{get_own_body}");
    assert!(get_own_body.contains("alice-secret"), "{get_own_body}");

    let decide_bob = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/approvals/appr-bob/decide")
                .header("authorization", format!("Bearer {alice_secret}"))
                .header("Idempotency-Key", "alice-decide-bob")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"decision":1}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    let decide_status = decide_bob.status();
    let decide_body = response_body(decide_bob).await;
    assert_eq!(decide_status, StatusCode::NOT_FOUND, "{decide_body}");
    assert!(!decide_body.contains("bob-secret"), "{decide_body}");

    let keys = app
        .clone()
        .oneshot(bearer_get("/v1/keys", &alice_secret))
        .await
        .unwrap();
    let keys_status = keys.status();
    let keys_body = response_body(keys).await;
    assert_eq!(keys_status, StatusCode::OK, "{keys_body}");
    assert!(
        keys_body.contains("alice-key") || keys_body.contains("alice"),
        "{keys_body}"
    );
    assert!(!keys_body.contains("bob-key"), "{keys_body}");

    let revoke_bob = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/v1/keys/bob-key")
                .header("authorization", format!("Bearer {alice_secret}"))
                .header("Idempotency-Key", "alice-revoke-bob")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(revoke_bob.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn grant_rows_never_open_http_and_list_and_revoke_per_owner() {
    use vam_orchestrator::credentials::GrantInsert;
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    store.create_user("alice", "alice").unwrap();
    store.create_user("bob", "bob").unwrap();
    let scopes: Vec<String> = MEMBER_SCOPES.iter().map(|s| s.to_string()).collect();
    let admin_scopes: Vec<String> = BOOTSTRAP_SCOPES.iter().map(|s| s.to_string()).collect();
    let (alice_secret, bob_secret, admin_secret) = (
        mint_api_key_secret(),
        mint_api_key_secret(),
        mint_api_key_secret(),
    );
    store
        .create_api_key_owned(
            "alice-key",
            "alice",
            &alice_secret,
            "User::\"alice\"",
            &scopes,
            "alice",
        )
        .unwrap();
    store
        .create_api_key_owned(
            "bob-key",
            "bob",
            &bob_secret,
            "User::\"bob\"",
            &scopes,
            "bob",
        )
        .unwrap();
    store
        .create_api_key_owned(
            "admin-key",
            "admin",
            &admin_secret,
            "User::\"admin\"",
            &admin_scopes,
            OPERATOR_USER_ID,
        )
        .unwrap();
    let (grant, token) = store
        .insert_grant_credential(&GrantInsert {
            name: "grant".into(),
            owner_user_id: "alice".into(),
            holder: "broker:vam.test".into(),
            target_kind: "space".into(),
            target_id: "sp_none".into(),
            constraints_json: r#"{"message":"hello grant"}"#.into(),
            issued_via: "invoke".into(),
            source_run_id: None,
            tool_name: Some("echo.arm".into()),
            scopes: vec!["runs:write".into()],
            expires_at: 4_000_000_000,
        })
        .unwrap();
    let bare = token.split_once('.').unwrap().1.to_string();
    let app = test_app(app_state_http_only(store));

    // The grant opens no HTTP route: full token and bare secret are 401.
    for presented in [token.as_str(), bare.as_str()] {
        let r = app
            .clone()
            .oneshot(bearer_get("/v1/keys", presented))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    }
    // Default list = today's list; holder=broker lists the grant for its owner only.
    let plain = response_body(
        app.clone()
            .oneshot(bearer_get("/v1/keys", &alice_secret))
            .await
            .unwrap(),
    )
    .await;
    assert!(
        plain.contains("alice-key") && !plain.contains(&grant.key_id),
        "{plain}"
    );
    let grants = response_body(
        app.clone()
            .oneshot(bearer_get("/v1/keys?holder=broker", &alice_secret))
            .await
            .unwrap(),
    )
    .await;
    assert!(
        grants.contains(&grant.key_id) && grants.contains("hello grant"),
        "{grants}"
    );
    assert!(
        grants.contains("broker:vam.test") && !grants.contains(&bare),
        "{grants}"
    );
    for secret in [&bob_secret, &admin_secret] {
        let r = app
            .clone()
            .oneshot(bearer_get("/v1/keys?holder=all", secret))
            .await
            .unwrap();
        let body = response_body(r).await;
        assert!(!body.contains(&grant.key_id), "{body}");
    }
    let bad = app
        .clone()
        .oneshot(bearer_get("/v1/keys?holder=nope", &alice_secret))
        .await
        .unwrap();
    assert_eq!(bad.status(), StatusCode::BAD_REQUEST);
    // A foreign user and keys:admin both get NotFound on revoke; the owner revokes it.
    let delete = |secret: &str, k: &str| {
        Request::builder()
            .method("DELETE")
            .uri(format!("/v1/keys/{}", grant.key_id))
            .header("authorization", format!("Bearer {secret}"))
            .header("Idempotency-Key", k.to_string())
            .body(Body::empty())
            .unwrap()
    };
    for (secret, k) in [(&bob_secret, "d-bob"), (&admin_secret, "d-admin")] {
        let r = app.clone().oneshot(delete(secret, k)).await.unwrap();
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
    }
    let r = app
        .clone()
        .oneshot(delete(&alice_secret, "d-alice"))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
}

/// POST a decision for `appr` and return the status and body.
async fn decide_with(
    app: &axum::Router,
    secret: &str,
    appr: &str,
    key: &str,
    body: &str,
) -> (StatusCode, String) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/v1/approvals/{appr}/decide"))
                .header("authorization", format!("Bearer {secret}"))
                .header("Idempotency-Key", key)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    (status, response_body(response).await)
}

#[tokio::test]
async fn decide_remember_must_agree_with_the_decision() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let (store, secret) = bootstrap_store();
    store
        .record_published_approval("appr-r1", "urn:vam:run:local.vam.dev/acme/r1", "", "{}")
        .unwrap();
    let app = test_app(app_state_http_only(store.clone()));
    let (status, body) = decide_with(
        &app,
        &secret,
        "appr-r1",
        "k1",
        r#"{"decision":2,"remember":"REMEMBER_ALWAYS_ALLOW","rejectionReason":"no"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("APPROVED"), "{body}");
    assert!(store.get_approval("appr-r1").unwrap().decision.is_none());
}

#[tokio::test]
async fn decide_always_on_a_run_without_a_draft_is_a_409_and_saves_nothing() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let (store, secret) = bootstrap_store();
    store
        .record_published_approval("appr-r2", "urn:vam:run:local.vam.dev/acme/r2", "", "{}")
        .unwrap();
    let app = test_app(app_state_http_only(store.clone()));
    for (n, remember) in ["REMEMBER_ALWAYS_ALLOW", "REMEMBER_ALWAYS_DENY"]
        .into_iter()
        .enumerate()
    {
        let (status, body) = decide_with(
            &app,
            &secret,
            "appr-r2",
            &format!("k{n}"),
            &format!(r#"{{"remember":"{remember}"}}"#),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert!(body.contains("once only"), "{body}");
        assert!(store.get_approval("appr-r2").unwrap().decision.is_none());
    }
}

#[tokio::test]
async fn decide_on_a_finished_run_is_a_409_and_the_run_stays_finished() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let (store, secret) = bootstrap_store();
    let run = "urn:vam:run:local.vam.dev/acme/r-done";
    store.create_run(&ResourceId { urn: run.into() }).unwrap();
    store.update_run_state(run, "SUCCEEDED").unwrap();
    store
        .record_published_approval("appr-done", run, "", "{}")
        .unwrap();
    let app = test_app(app_state_http_only(store.clone()));
    let (status, body) = decide_with(&app, &secret, "appr-done", "kd", r#"{"decision":1}"#).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body.contains("finished"), "{body}");
    assert!(store.get_approval("appr-done").unwrap().decision.is_none());
    assert_eq!(store.get_run_state(run).unwrap(), "SUCCEEDED");
}

/// A parked question (`ask_user`, AR-3.10) in `store`, owned by the local user.
fn parked_question_row(store: &SqliteWorkflowStore, appr: &str, run: &str) {
    store.create_run(&ResourceId { urn: run.into() }).unwrap();
    store.update_run_state(run, "PAUSED").unwrap();
    let asked = r#"{"jig":"vam.question","data":{"question":"Pick","choices":["blue","red"]}}"#;
    store
        .record_published_approval_of_kind(appr, run, "", asked, "question", "ask_user")
        .unwrap();
}

#[tokio::test]
async fn a_bad_answer_is_a_400_and_the_question_stays_pending() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let (store, secret) = bootstrap_store();
    parked_question_row(&store, "appr-q1", "urn:vam:run:local.vam.dev/acme/rq1");
    let app = test_app(app_state_http_only(store.clone()));
    let big = "x".repeat(17 * 1024);
    let bodies = [
        r#"{"decision":4,"response":{"answer":"green"}}"#.to_string(),
        r#"{"decision":4,"response":{"answer":42}}"#.to_string(),
        r#"{"decision":4}"#.to_string(),
        format!(r#"{{"decision":4,"response":{{"answer":"{big}"}}}}"#),
    ];
    for (n, body) in bodies.iter().enumerate() {
        let (status, text) = decide_with(&app, &secret, "appr-q1", &format!("kq{n}"), body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{n}: {text}");
        assert!(store.get_approval("appr-q1").unwrap().decision.is_none());
    }
    assert_eq!(
        store
            .get_run_state("urn:vam:run:local.vam.dev/acme/rq1")
            .unwrap(),
        "PAUSED"
    );
}

#[tokio::test]
async fn a_question_refuses_approve_amend_and_remember() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let (store, secret) = bootstrap_store();
    parked_question_row(&store, "appr-q2", "urn:vam:run:local.vam.dev/acme/rq2");
    let app = test_app(app_state_http_only(store.clone()));
    let attempts = [
        r#"{"decision":1}"#,
        r#"{"decision":3,"amendedArgs":{"jig":"vam.question"}}"#,
        r#"{"remember":"REMEMBER_ALWAYS_ALLOW"}"#,
        r#"{"remember":"REMEMBER_ALWAYS_DENY"}"#,
    ];
    for (n, body) in attempts.iter().enumerate() {
        let (status, text) = decide_with(&app, &secret, "appr-q2", &format!("kr{n}"), body).await;
        assert!(status.is_client_error(), "{n}: {status} {text}");
        assert!(store.get_approval("appr-q2").unwrap().decision.is_none());
    }
}

#[tokio::test]
async fn run_status_says_what_a_paused_run_waits_for() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let (store, secret) = bootstrap_store();
    let q_run = "urn:vam:run:local.vam.dev/acme/rq3";
    let t_run = "urn:vam:run:local.vam.dev/acme/rt3";
    let g_run = "urn:vam:run:local.vam.dev/acme/rg3";
    parked_question_row(&store, "appr-q3", q_run);
    for (run, appr) in [(t_run, "appr-t3"), (g_run, "appr-g3")] {
        store.create_run(&ResourceId { urn: run.into() }).unwrap();
        store.update_run_state(run, "PAUSED").unwrap();
        let kind = if appr == "appr-g3" {
            "gate"
        } else {
            "tool_call"
        };
        store
            .record_published_approval_of_kind(appr, run, "", "{}", kind, "tool")
            .unwrap();
    }
    let app = test_app(app_state_http_only(store.clone()));
    for (run, expected) in [
        (q_run, "WAITING_FOR_INPUT"),
        (t_run, "WAITING_FOR_APPROVAL"),
        (g_run, "WAITING_FOR_GATE"),
    ] {
        let response = app
            .clone()
            .oneshot(bearer_get(
                &format!("/v1/runs/{}", encode_run_path(run)),
                &secret,
            ))
            .await
            .unwrap();
        let status = response.status();
        let body = response_body(response).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body.contains(expected), "{run}: {body}");
    }
    // A run that is not parked waits for nothing.
    store.update_run_state(q_run, "RUNNING").unwrap();
    let response = app
        .clone()
        .oneshot(bearer_get(
            &format!("/v1/runs/{}", encode_run_path(q_run)),
            &secret,
        ))
        .await
        .unwrap();
    let body = response_body(response).await;
    assert!(!body.contains("WAITING_FOR"), "{body}");
}

#[tokio::test]
async fn isolation_single_user_local_sees_own_rows() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let (store, secret) = bootstrap_store();
    store
        .create_session("sess-local-only", None, "vm-local-only", "ws", "default")
        .unwrap();
    store
        .persist_run_spec(
            "urn:vam:run:local.vam.dev/acme/run-local-only",
            "{}",
            Some("sess-local-only"),
            "urn:vam:agent:local",
            Some("iso-local"),
            "User::\"tester\"",
        )
        .unwrap();
    let app = test_app(app_state_http_only(store));

    let sessions = app
        .clone()
        .oneshot(bearer_get("/v1/sessions", &secret))
        .await
        .unwrap();
    let sessions_status = sessions.status();
    let sessions_body = response_body(sessions).await;
    assert_eq!(sessions_status, StatusCode::OK, "{sessions_body}");
    assert!(sessions_body.contains("sess-local-only"), "{sessions_body}");

    let get_session = app
        .clone()
        .oneshot(bearer_get("/v1/sessions/sess-local-only", &secret))
        .await
        .unwrap();
    assert_eq!(get_session.status(), StatusCode::OK);

    let runs = app
        .clone()
        .oneshot(bearer_get("/v1/runs", &secret))
        .await
        .unwrap();
    let runs_status = runs.status();
    let runs_body = response_body(runs).await;
    assert_eq!(runs_status, StatusCode::OK, "{runs_body}");
    assert!(runs_body.contains("run-local-only"), "{runs_body}");

    let run_path = encode_run_path("urn:vam:run:local.vam.dev/acme/run-local-only");
    let get_run = app
        .oneshot(bearer_get(&format!("/v1/runs/{run_path}"), &secret))
        .await
        .unwrap();
    assert_eq!(get_run.status(), StatusCode::OK);
}

fn two_member_keys(store: &SqliteWorkflowStore) -> (String, String) {
    store.create_user("alice", "alice").unwrap();
    store.create_user("bob", "bob").unwrap();
    let scopes: Vec<String> = MEMBER_SCOPES.iter().map(|s| s.to_string()).collect();
    let alice_secret = mint_api_key_secret();
    let bob_secret = mint_api_key_secret();
    store
        .create_api_key_owned(
            "alice-key",
            "alice",
            &alice_secret,
            "User::\"alice\"",
            &scopes,
            "alice",
        )
        .unwrap();
    store
        .create_api_key_owned(
            "bob-key",
            "bob",
            &bob_secret,
            "User::\"bob\"",
            &scopes,
            "bob",
        )
        .unwrap();
    (alice_secret, bob_secret)
}

fn bearer_json(
    method: &str,
    uri: &str,
    secret: &str,
    idem: &str,
    body: serde_json::Value,
) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {secret}"))
        .header("Idempotency-Key", idem)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

#[tokio::test]
async fn idempotency_session_create_is_per_user() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let (alice_secret, bob_secret) = two_member_keys(&store);
    store
        .create_session_with_owner(
            "sess-alice",
            Some("shared-sess-k"),
            "vm-alice",
            "ws-alice-idemp",
            "default",
            "alice",
        )
        .unwrap();
    store
        .create_session_with_owner(
            "sess-bob",
            Some("shared-sess-k"),
            "vm-bob",
            "ws-bob-idemp",
            "default",
            "bob",
        )
        .unwrap();
    let app = test_app(app_state_http_only(store));

    let body = serde_json::json!({
        "idempotencyKey": "shared-sess-k",
        "workspaceId": "ws-alice-idemp",
        "storageProfileId": "default"
    });
    let alice_replay = app
        .clone()
        .oneshot(bearer_json(
            "POST",
            "/v1/sessions",
            &alice_secret,
            "shared-sess-k",
            body.clone(),
        ))
        .await
        .unwrap();
    let alice_status = alice_replay.status();
    let alice_body = response_body(alice_replay).await;
    assert_eq!(alice_status, StatusCode::OK, "{alice_body}");
    let alice_json: serde_json::Value = serde_json::from_str(&alice_body).unwrap();
    assert_eq!(
        alice_json["session"]["sessionId"]["urn"].as_str().unwrap(),
        "sess-alice"
    );

    let bob_body = serde_json::json!({
        "idempotencyKey": "shared-sess-k",
        "workspaceId": "ws-bob-idemp",
        "storageProfileId": "default"
    });
    let bob_create = app
        .clone()
        .oneshot(bearer_json(
            "POST",
            "/v1/sessions",
            &bob_secret,
            "shared-sess-k",
            bob_body,
        ))
        .await
        .unwrap();
    let bob_status = bob_create.status();
    let bob_resp = response_body(bob_create).await;
    assert_eq!(bob_status, StatusCode::OK, "{bob_resp}");
    let bob_json: serde_json::Value = serde_json::from_str(&bob_resp).unwrap();
    assert_eq!(
        bob_json["session"]["sessionId"]["urn"].as_str().unwrap(),
        "sess-bob",
        "{bob_resp}"
    );

    let alice_again = app
        .clone()
        .oneshot(bearer_json(
            "POST",
            "/v1/sessions",
            &alice_secret,
            "shared-sess-k",
            body,
        ))
        .await
        .unwrap();
    let alice_again_body = response_body(alice_again).await;
    let alice_again_json: serde_json::Value = serde_json::from_str(&alice_again_body).unwrap();
    assert_eq!(
        alice_again_json["session"]["sessionId"]["urn"]
            .as_str()
            .unwrap(),
        "sess-alice"
    );

    let bob_get_alice = app
        .oneshot(bearer_get("/v1/sessions/sess-alice", &bob_secret))
        .await
        .unwrap();
    assert_eq!(bob_get_alice.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn idempotency_run_create_is_per_user() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    store.initialize_catalog_schema().unwrap();
    seed_test_playbook(&store);
    let (alice_secret, bob_secret) = two_member_keys(&store);
    let alice_run = "urn:vam:run:local.vam.dev/acme/run-alice-idemp";
    store
        .persist_run_spec_with_owner(
            alice_run,
            "{}",
            None,
            "urn:vam:agent:alice",
            Some("shared-run-k"),
            "User::\"alice\"",
            "alice",
        )
        .unwrap();
    let app = test_app(app_state_http_only(store));

    let alice_body = create_run_body(alice_run, "shared-run-k");
    let alice_replay = app
        .clone()
        .oneshot(bearer_json(
            "POST",
            "/v1/runs",
            &alice_secret,
            "shared-run-k",
            alice_body,
        ))
        .await
        .unwrap();
    let alice_status = alice_replay.status();
    let alice_resp = response_body(alice_replay).await;
    assert!(alice_status.is_success(), "{alice_resp}");
    assert!(alice_resp.contains("run-alice-idemp"), "{alice_resp}");

    let bob_run = "urn:vam:run:local.vam.dev/acme/run-bob-idemp";
    let mut bob_body = create_run_body(bob_run, "shared-run-k");
    bob_body["spec"]["workspaceRefUri"] = serde_json::json!("volume://bob-idemp");
    let bob_create = app
        .clone()
        .oneshot(bearer_json(
            "POST",
            "/v1/runs",
            &bob_secret,
            "shared-run-k",
            bob_body,
        ))
        .await
        .unwrap();
    let bob_status = bob_create.status();
    let bob_resp = response_body(bob_create).await;
    assert_eq!(bob_status, StatusCode::ACCEPTED, "{bob_resp}");
    assert!(bob_resp.contains("run-bob-idemp"), "{bob_resp}");
    assert!(!bob_resp.contains("run-alice-idemp"), "{bob_resp}");

    let bob_get_alice = app
        .oneshot(bearer_get(
            &format!("/v1/runs/{}", encode_run_path(alice_run)),
            &bob_secret,
        ))
        .await
        .unwrap();
    assert_eq!(bob_get_alice.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn workspace_prefix_isolates_users_and_rejects_guest_override() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    store.initialize_catalog_schema().unwrap();
    seed_test_playbook(&store);
    let (alice_secret, bob_secret) = two_member_keys(&store);
    let agent_urn = "urn:vam:agent:local.vam.dev/acme/prefix-agent";
    let app = test_app(app_state_http_only(store.clone()));

    let alice_run = app
        .clone()
        .oneshot(bearer_json(
            "POST",
            "/v1/runs",
            &alice_secret,
            "alice-ws-prefix",
            sample_run_body(
                agent_urn,
                "assistant",
                "alice-ws-prefix",
                "urn:vam:run:local.vam.dev/acme/run-alice-ws",
            ),
        ))
        .await
        .unwrap();
    let alice_status = alice_run.status();
    let alice_body = response_body(alice_run).await;
    assert_eq!(alice_status, StatusCode::ACCEPTED, "{alice_body}");

    // Bob gets his own agent URN. The owner rule forbids him versioning
    // alice's, and this test is about workspace isolation, not agent sharing.
    let bob_agent_urn = "urn:vam:agent:local.vam.dev/acme/prefix-agent-bob";
    let bob_run = app
        .clone()
        .oneshot(bearer_json(
            "POST",
            "/v1/runs",
            &bob_secret,
            "bob-ws-prefix",
            sample_run_body(
                bob_agent_urn,
                "assistant",
                "bob-ws-prefix",
                "urn:vam:run:local.vam.dev/acme/run-bob-ws",
            ),
        ))
        .await
        .unwrap();
    let bob_status = bob_run.status();
    let bob_body = response_body(bob_run).await;
    assert_eq!(bob_status, StatusCode::ACCEPTED, "{bob_body}");
    assert_ne!(
        bob_status,
        StatusCode::CONFLICT,
        "bob must not fight alice for volume://assistant: {bob_body}"
    );

    // Each owner's "assistant" is a Space of their own, found by owner and id.
    let alice_sessions = store.load_sessions_for_owner("alice").unwrap();
    let bob_sessions = store.load_sessions_for_owner("bob").unwrap();
    let alice_ws = alice_sessions[0].workspace_id.clone();
    let bob_ws = bob_sessions[0].workspace_id.clone();
    assert!(vam_orchestrator::spaces::is_space_id(&alice_ws));
    assert!(vam_orchestrator::spaces::is_space_id(&bob_ws));
    assert_ne!(alice_ws, bob_ws);
    assert_eq!(
        store.get_space("alice", &alice_ws).unwrap().label,
        "assistant"
    );
    assert_eq!(store.get_space("bob", &bob_ws).unwrap().label, "assistant");

    // Alice names bob's Space by its id: refused exactly as an id that does not exist.
    let ask = |ws: String, key: &'static str| {
        let mut body = sample_run_body(
            agent_urn,
            "assistant",
            key,
            "urn:vam:run:local.vam.dev/acme/run-alice-steal",
        );
        body["spec"]["spaceId"] = serde_json::json!(ws);
        bearer_json("POST", "/v1/runs", &alice_secret, key, body)
    };
    let steal = app
        .clone()
        .oneshot(ask(bob_ws.clone(), "alice-steal-ws"))
        .await
        .unwrap();
    assert_eq!(steal.status(), StatusCode::NOT_FOUND);
    let steal_body = response_body(steal).await;
    let missing = app
        .clone()
        .oneshot(ask(
            vam_orchestrator::spaces::mint_id("sp_"),
            "alice-missing-ws",
        ))
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        steal_body,
        response_body(missing).await,
        "foreign is missing"
    );
    assert!(!steal_body.contains(&bob_ws), "{steal_body}");

    // The same door guards the routes: alice reads neither bob's threads nor his files.
    for path in [
        format!("/v1/workspaces/{bob_ws}/threads"),
        format!("/v1/workspaces/{bob_ws}/files"),
        format!("/v1/spaces/{bob_ws}"),
    ] {
        let response = app
            .clone()
            .oneshot(bearer_get(&path, &alice_secret))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
    }
    // And the list shows each owner only their own.
    let listed = response_body(
        app.clone()
            .oneshot(bearer_get("/v1/spaces", &alice_secret))
            .await
            .unwrap(),
    )
    .await;
    assert!(
        listed.contains(&alice_ws) && !listed.contains(&bob_ws),
        "{listed}"
    );
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn spaces_are_created_once_per_key_and_renamed_in_place() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    store.initialize_catalog_schema().unwrap();
    seed_test_playbook(&store);
    let (alice_secret, bob_secret) = two_member_keys(&store);
    let agent_urn = "urn:vam:agent:local.vam.dev/acme/prefix-agent";
    let app = test_app(app_state_http_only(store.clone()));
    // Alice's run puts the agent in the catalog.
    let run = app
        .clone()
        .oneshot(bearer_json(
            "POST",
            "/v1/runs",
            &alice_secret,
            "seed-agent",
            sample_run_body(
                agent_urn,
                "assistant",
                "seed-agent",
                "urn:vam:run:local.vam.dev/acme/run-seed-agent",
            ),
        ))
        .await
        .unwrap();
    assert_eq!(run.status(), StatusCode::ACCEPTED);

    let create = |key: &'static str, secret: String, body: String| {
        let app = app.clone();
        async move {
            let r = app
                .oneshot(bearer_json(
                    "POST",
                    "/v1/spaces",
                    &secret,
                    key,
                    serde_json::from_str(&body).unwrap(),
                ))
                .await
                .unwrap();
            let status = r.status();
            (status, response_body(r).await)
        }
    };
    let body = format!(r#"{{"agentUrn":"{agent_urn}","label":"  my notes  "}}"#);
    let (status, first) = create("space-key-1", alice_secret.clone(), body.clone()).await;
    assert_eq!(status, StatusCode::OK, "{first}");
    let first: serde_json::Value = serde_json::from_str(&first).unwrap();
    let id = first["spaceId"].as_str().unwrap().to_string();
    assert!(vam_orchestrator::spaces::is_space_id(&id));
    assert_eq!(first["name"], "my notes");
    // A retry with the same key is the same Space; a new key is a new one.
    let (_, again) = create("space-key-1", alice_secret.clone(), body.clone()).await;
    let again: serde_json::Value = serde_json::from_str(&again).unwrap();
    assert_eq!(again["spaceId"].as_str().unwrap(), id);
    let (_, other) = create("space-key-2", alice_secret.clone(), body.clone()).await;
    let other: serde_json::Value = serde_json::from_str(&other).unwrap();
    assert_ne!(other["spaceId"].as_str().unwrap(), id);
    // No key, a bad label, a missing agent: refused.
    let no_key = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/spaces")
                .header("authorization", format!("Bearer {alice_secret}"))
                .body(Body::from(body.clone()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(no_key.status(), StatusCode::BAD_REQUEST);
    let (status, _) = create(
        "space-key-3",
        alice_secret.clone(),
        format!(r#"{{"agentUrn":"{agent_urn}","label":"a\u0007b"}}"#),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = create(
        "space-key-4",
        alice_secret.clone(),
        r#"{"agentUrn":"urn:vam:agent:local.vam.dev/acme/nope","label":"x"}"#.into(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Rename in place, twice, and the thread too.
    let thread = store
        .create_thread("alice", &id, "first", "api")
        .unwrap()
        .thread_id;
    let patch = |uri: String, secret: String, label: &'static str| {
        let app = app.clone();
        async move {
            let r = app
                .oneshot(bearer_json(
                    "PATCH",
                    &uri,
                    &secret,
                    "rename",
                    serde_json::json!({ "label": label }),
                ))
                .await
                .unwrap();
            let status = r.status();
            (status, response_body(r).await)
        }
    };
    for _ in 0..2 {
        let (status, renamed) = patch(
            format!("/v1/spaces/{id}"),
            alice_secret.clone(),
            "Renamed Space",
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{renamed}");
        assert!(
            renamed.contains(&id) && renamed.contains("Renamed Space"),
            "{renamed}"
        );
        let (status, renamed) = patch(
            format!("/v1/spaces/{id}/threads/{thread}"),
            alice_secret.clone(),
            "Renamed thread",
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{renamed}");
        assert!(
            renamed.contains(&thread) && renamed.contains("Renamed thread"),
            "{renamed}"
        );
    }
    assert_eq!(
        store.get_space("alice", &id).unwrap().label,
        "Renamed Space"
    );
    assert_eq!(
        store.get_thread("alice", &id, &thread).unwrap().label,
        "Renamed thread"
    );
    // A foreign owner, a non-id and a missing thread are all 404, and change nothing.
    for uri in [
        format!("/v1/spaces/{id}"),
        format!("/v1/spaces/{id}/threads/{thread}"),
    ] {
        let (status, _) = patch(uri, bob_secret.clone(), "stolen").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
    let (status, _) = patch("/v1/spaces/my-notes".into(), alice_secret.clone(), "x").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = patch(
        format!("/v1/spaces/{id}/threads/th_00000000000000000000000000"),
        alice_secret.clone(),
        "x",
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        store.get_space("alice", &id).unwrap().label,
        "Renamed Space"
    );
    // Bob cannot create a Space on alice's draft-less agent id he does not own? Agents are
    // released and visible; his Space is his own.
    let (status, bobs) = create("bob-key", bob_secret.clone(), body).await;
    assert_eq!(status, StatusCode::OK, "{bobs}");
    assert!(!bobs.contains(&id));
}

fn bearer_get(uri: &str, secret: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(uri)
        .header("authorization", format!("Bearer {secret}"))
        .body(Body::empty())
        .unwrap()
}

fn encode_run_path(run_id: &str) -> String {
    run_id
        .chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => c.to_string(),
            _ => format!("%{:02X}", c as u32),
        })
        .collect()
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

#[tokio::test]
async fn unauthenticated_mutating_call_returns_401() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let (store, _) = bootstrap_store();
    let app = test_app(app_state_http_only(store));
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/sessions")
                .header("content-type", "application/json")
                .header("Idempotency-Key", "idem-1")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn bootstrap_key_can_put_agent_and_post_run() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let (store, secret) = bootstrap_store();
    seed_test_playbook(&store);
    let app = test_app(app_state(store));

    let agent_json = serde_json::json!({
        "spec": {
            "resourceId": {"urn": "urn:vam:agent:local.vam.dev/acme/test-agent"},
            "displayName": "Test Agent",
            "description": "API test agent",
            "role": "Tester",
            "systemInstructions": "test instructions",
            "allowAnyPlaybook": true
        },
        "idempotencyKey": "agent-idem"
    });

    let put_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/v1/agents")
                .header("authorization", format!("Bearer {}", secret))
                .header("Idempotency-Key", "agent-idem")
                .header("content-type", "application/json")
                .body(Body::from(agent_json.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        put_resp.status(),
        StatusCode::OK,
        "{}",
        response_body(put_resp).await
    );

    let run_body = serde_json::json!({
        "spec": {
            "runId": {"urn": "urn:vam:run:local.vam.dev/acme/run-api-test"},
            "agentRef": {
                "resourceId": {"urn": "urn:vam:agent:local.vam.dev/acme/test-agent"},
                "version": {"major": 1, "minor": 0, "patch": 0},
                "digest": {"algorithm": "ALGORITHM_SHA256", "hexHash": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"}
            },
            "playbookRef": {
                "resourceId": {"urn": "urn:vam:playbook:local.vam.dev/acme/test-pb"},
                "version": {"major": 1, "minor": 0, "patch": 0},
                "digest": {"algorithm": "ALGORITHM_SHA256", "hexHash": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"}
            },
            "inputs": {"fields": {"prompt": {"stringValue": "hello from api test"}}},
            "workspaceRefUri": "volume://mock",
            "idempotencyKey": "run-idem-1"
        },
        "inlineAgent": {
            "resourceId": {"urn": "urn:vam:agent:local.vam.dev/acme/test-agent"},
            "displayName": "Test Agent",
            "description": "inline",
            "role": "Tester",
            "systemInstructions": "test",
            "allowAnyPlaybook": true
        }
    });

    let run_resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/runs")
                .header("authorization", format!("Bearer {}", secret))
                .header("Idempotency-Key", "run-idem-1")
                .header("content-type", "application/json")
                .body(Body::from(run_body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        run_resp.status(),
        StatusCode::ACCEPTED,
        "{}",
        response_body(run_resp).await
    );
}

fn create_run_body(run_id: &str, idem: &str) -> serde_json::Value {
    serde_json::json!({
        "spec": {
            "runId": {"urn": run_id},
            "agentRef": {
                "resourceId": {"urn": "urn:vam:agent:local.vam.dev/acme/test-agent"},
                "version": {"major": 1, "minor": 0, "patch": 0},
                "digest": {"algorithm": "ALGORITHM_SHA256", "hexHash": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"}
            },
            "playbookRef": {
                "resourceId": {"urn": "urn:vam:playbook:local.vam.dev/acme/test-pb"},
                "version": {"major": 1, "minor": 0, "patch": 0},
                "digest": {"algorithm": "ALGORITHM_SHA256", "hexHash": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"}
            },
            "inputs": {"fields": {"prompt": {"stringValue": "hello from api test"}}},
            "workspaceRefUri": "volume://mock",
            "idempotencyKey": idem
        },
        "inlineAgent": {
            "resourceId": {"urn": "urn:vam:agent:local.vam.dev/acme/test-agent"},
            "displayName": "Test Agent",
            "description": "inline",
            "role": "Tester",
            "systemInstructions": "test",
            "allowAnyPlaybook": true
        }
    })
}

#[tokio::test]
async fn put_allowed_models_refuses_mock_model_without_the_scripted_flag() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let (store, secret) = bootstrap_store();
    let app = test_app(app_state(store.clone()));

    let body = serde_json::json!({"models": ["openai/gpt-4o", "mock-model"]});
    let resp = app
        .oneshot(bearer_json(
            "PUT",
            "/v1/node/allowed-models",
            &secret,
            "allowed-refused-1",
            body,
        ))
        .await
        .unwrap();
    let status = resp.status();
    let text = response_body(resp).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{text}");
    assert!(
        text.contains("mock-model is available only on a node started with VAMD_E2E_MOCK_SCRIPTS"),
        "{text}"
    );
    assert!(
        store.list_operator_allowed_models().unwrap().is_empty(),
        "a refused allowlist is not stored"
    );
}

#[tokio::test]
async fn put_allowed_models_accepts_mock_model_with_the_scripted_flag() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let (store, secret) = bootstrap_store();
    let mut state = app_state(store);
    state.e2e_mock_scripts = true;
    let app = test_app(state);

    let body = serde_json::json!({"models": ["mock-model"]});
    let resp = app
        .oneshot(bearer_json(
            "PUT",
            "/v1/node/allowed-models",
            &secret,
            "allowed-accepted-1",
            body,
        ))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "{}",
        response_body(resp).await
    );
}

#[tokio::test]
async fn create_run_on_mock_model_is_refused_without_the_scripted_flag() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let (store, secret) = bootstrap_store();
    seed_test_playbook(&store);
    let app = test_app(app_state(store));

    let mut body = create_run_body(
        "urn:vam:run:local.vam.dev/acme/run-mock-refused",
        "run-mock-refused",
    );
    body["spec"]["inputs"]["fields"]["model"] = serde_json::json!({"stringValue": "mock-model"});
    let resp = app
        .oneshot(bearer_json(
            "POST",
            "/v1/runs",
            &secret,
            "run-mock-refused",
            body,
        ))
        .await
        .unwrap();
    let status = resp.status();
    let text = response_body(resp).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{text}");
    assert!(text.contains("VAMD_E2E_MOCK_SCRIPTS"), "{text}");
}

#[tokio::test]
async fn create_run_invalid_traceparent_still_accepted() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let (store, secret) = bootstrap_store();
    seed_test_playbook(&store);
    let app = test_app(app_state_http_only(store));
    let run_body = create_run_body(
        "urn:vam:run:local.vam.dev/acme/run-bad-tp",
        "run-idem-bad-tp",
    );
    let run_resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/runs")
                .header("authorization", format!("Bearer {}", secret))
                .header("Idempotency-Key", "run-idem-bad-tp")
                .header("traceparent", "not-a-valid-traceparent")
                .header("content-type", "application/json")
                .body(Body::from(run_body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        run_resp.status().is_success(),
        "invalid traceparent must not reject CreateRun: {}",
        response_body(run_resp).await
    );
}

#[tokio::test]
async fn create_run_with_strict_playbook_accepts_chat_prompt() {
    // `prompt` and `model` are host-owned run inputs, so a strict playbook
    // schema ignores them even next to a required author input. This test
    // supplies `invoice_url` by hand. The app does not: it sends only `prompt`
    // and `model`, which is the request create_run_with_app_chat_inputs_names_missing_input
    // covers.
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let (store, secret) = bootstrap_store();
    seed_strict_playbook(&store);
    let app = test_app(app_state_http_only(store));
    let mut run_body = create_run_body(
        "urn:vam:run:local.vam.dev/acme/run-strict-chat",
        "run-idem-strict-chat",
    );
    run_body["spec"]["playbookRef"]["resourceId"]["urn"] =
        serde_json::json!("urn:vam:playbook:local.vam.dev/acme/strict-pb");
    run_body["spec"]["inputs"] = serde_json::json!({
        "invoice_url": "https://example.com/invoice.pdf",
        "prompt": "hello from api test",
        "model": "anthropic/claude-haiku-5-5"
    });

    let run_resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/runs")
                .header("authorization", format!("Bearer {}", secret))
                .header("Idempotency-Key", "run-idem-strict-chat")
                .header("content-type", "application/json")
                .body(Body::from(run_body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        run_resp.status(),
        StatusCode::ACCEPTED,
        "{}",
        response_body(run_resp).await
    );
}

async fn post_run(
    app: &axum::Router,
    secret: &str,
    idem: &str,
    body: &serde_json::Value,
) -> (StatusCode, String) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/runs")
                .header("authorization", format!("Bearer {}", secret))
                .header("Idempotency-Key", idem)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    (status, response_body(resp).await)
}

fn stored_run_inputs(store: &SqliteWorkflowStore, run_id: &str) -> serde_json::Value {
    let (spec_json, _, _) = store.get_run_spec_context(run_id).expect("run context");
    let spec: serde_json::Value = serde_json::from_str(&spec_json.expect("spec")).expect("json");
    spec["inputs"].clone()
}

#[tokio::test]
async fn playbook_activation_offers_attached_playbooks_to_a_run_that_names_none() {
    // A run with no playbook stays a plain run (the default is never bound), and
    // the model is offered the agent's playbooks with their `when` text. A run
    // that names one is bound at admission, exactly as before, and offers none.
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let (store, secret) = bootstrap_store();
    seed_test_playbook(&store);
    let app = test_app(app_state_http_only(store.clone()));
    let pb_ref = serde_json::json!({
        "resourceId": {"urn": "urn:vam:playbook:local.vam.dev/acme/test-pb"},
        "version": {"major": 1, "minor": 0, "patch": 0},
        "digest": {"algorithm": "ALGORITHM_SHA256", "hexHash": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"}
    });

    // 1. Names none: plain run, playbook offered. A caller-supplied list is dropped.
    let plain_id = "urn:vam:run:local.vam.dev/acme/run-act-plain";
    let mut plain = create_run_body(plain_id, "run-idem-act-plain");
    plain["spec"].as_object_mut().unwrap().remove("playbookRef");
    plain["spec"]["inputs"] = serde_json::json!({
        "prompt": "Hi",
        "attached_playbooks": "[{\"urn\":\"urn:vam:playbook:evil\",\"name\":\"x\",\"when\":\"x\"}]"
    });
    plain["inlineAgent"]["allowAnyPlaybook"] = serde_json::json!(false);
    plain["inlineAgent"]["defaultPlaybookRef"] = pb_ref.clone();
    let (status, body) = post_run(&app, &secret, "run-idem-act-plain", &plain).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let inputs = stored_run_inputs(&store, plain_id);
    assert!(inputs.get("playbook_procedure").is_none(), "{inputs}");
    let offered: serde_json::Value =
        serde_json::from_str(inputs["attached_playbooks"].as_str().expect("offered list"))
            .expect("json");
    assert_eq!(offered.as_array().map(Vec::len), Some(1), "{offered}");
    assert_eq!(
        offered[0]["urn"],
        "urn:vam:playbook:local.vam.dev/acme/test-pb"
    );
    assert_eq!(offered[0]["default"], true);
    assert_eq!(
        offered[0]["when"],
        "Use when the request matches this test playbook."
    );
    assert!(
        store.get_playbook_run(plain_id).expect("row").is_none(),
        "the default is not bound"
    );

    // 2. Names one (the CLI --playbook and API start): bound now, nothing offered.
    let named_id = "urn:vam:run:local.vam.dev/acme/run-act-named";
    let mut named = create_run_body(named_id, "run-idem-act-named");
    named["spec"]["playbookRef"] = pb_ref.clone();
    // The first run holds the workspace; the second uses its own.
    // (Names are labels, so "its own workspace" is a Space of its own, named by id.)
    let own = store
        .create_space(LOCAL_USER_ID, None, "mock-named", "run", None)
        .unwrap();
    named["spec"]["spaceId"] = serde_json::json!(own.space_id);
    named["inlineAgent"]["allowAnyPlaybook"] = serde_json::json!(false);
    named["inlineAgent"]["defaultPlaybookRef"] = pb_ref;
    let (status, body) = post_run(&app, &secret, "run-idem-act-named", &named).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let inputs = stored_run_inputs(&store, named_id);
    assert!(inputs.get("playbook_procedure").is_some(), "{inputs}");
    assert!(inputs.get("attached_playbooks").is_none(), "{inputs}");
    assert!(store.get_playbook_run(named_id).expect("row").is_some());
}

#[tokio::test]
async fn create_run_with_app_chat_inputs_names_missing_input() {
    // The app's exact chat request: `inputs` holds only `prompt` and
    // `model`, and the default playbook has a required author input. vamd must
    // refuse it and name the missing field, so the app has to collect it.
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let (store, secret) = bootstrap_store();
    seed_strict_playbook(&store);
    let app = test_app(app_state_http_only(store));
    let mut run_body = create_run_body(
        "urn:vam:run:local.vam.dev/acme/run-app-chat",
        "run-idem-app-chat",
    );
    run_body["spec"]["playbookRef"]["resourceId"]["urn"] =
        serde_json::json!("urn:vam:playbook:local.vam.dev/acme/strict-pb");
    run_body["spec"]["inputs"] = serde_json::json!({
        "prompt": "hello from the app",
        "model": "anthropic/claude-haiku-5-5"
    });

    let run_resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/runs")
                .header("authorization", format!("Bearer {}", secret))
                .header("Idempotency-Key", "run-idem-app-chat")
                .header("content-type", "application/json")
                .body(Body::from(run_body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = run_resp.status();
    let body = response_body(run_resp).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("invoice_url"), "{body}");
}

/// `PUT` one spec file through a catalog route, the way `vam playbook put` and
/// `vam agent put` do. Returns the status and body for the assertion.
async fn put_spec_file(
    app: &axum::Router,
    secret: &str,
    uri: &str,
    idem: &str,
    spec: serde_json::Value,
) -> (StatusCode, String) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(uri)
                .header("authorization", format!("Bearer {}", secret))
                .header("Idempotency-Key", idem)
                .header("content-type", "application/json")
                .body(Body::from(serde_json::json!({ "spec": spec }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    (status, response_body(resp).await)
}

fn read_json_file(path: &std::path::Path) -> serde_json::Value {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

#[tokio::test]
async fn put_playbook_accepts_every_example_playbook_file() {
    // Publishes each examples/*/playbook.json through PUT /v1/playbooks, so a
    // file that stops matching the spec fails here rather than on a Mac.
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let (store, secret) = bootstrap_store();
    let app = test_app(app_state_http_only(store));

    let examples = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples");
    let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(&examples)
        .expect("examples dir")
        .flatten()
        .map(|entry| entry.path().join("playbook.json"))
        .filter(|path| path.is_file())
        .collect();
    files.sort();
    assert!(
        files.len() >= 3,
        "expected example playbooks under {}, found {}",
        examples.display(),
        files.len()
    );

    for (i, path) in files.iter().enumerate() {
        let spec = read_json_file(path);
        // Examples are user content, so each file is published as-is.
        // examples.local is reserved for platform seeding and refused for callers,
        // so the file itself must not claim that domain.
        let urn = spec["resourceId"]["urn"].as_str().expect("resourceId.urn");
        assert!(
            !urn.contains("examples.local/"),
            "{}: {urn} uses the platform-reserved examples.local domain",
            path.display()
        );

        let (status, body) = put_spec_file(
            &app,
            &secret,
            "/v1/playbooks",
            &format!("put-example-playbook-{i}"),
            spec,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{}: {body}", path.display());
    }
}

#[tokio::test]
async fn create_run_valid_traceparent_is_accepted() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let (store, secret) = bootstrap_store();
    seed_test_playbook(&store);
    let app = test_app(app_state_http_only(store));
    let run_body = create_run_body(
        "urn:vam:run:local.vam.dev/acme/run-good-tp",
        "run-idem-good-tp",
    );
    let run_resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/runs")
                .header("authorization", format!("Bearer {}", secret))
                .header("Idempotency-Key", "run-idem-good-tp")
                .header(
                    "traceparent",
                    "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
                )
                .header("content-type", "application/json")
                .body(Body::from(run_body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        run_resp.status().is_success(),
        "valid traceparent CreateRun should be 2xx: {}",
        response_body(run_resp).await
    );
}

#[test]
fn extract_http_traceparent_drops_invalid() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    use axum::http::HeaderMap;
    let mut headers = HeaderMap::new();
    headers.insert("traceparent", "garbage".parse().unwrap());
    assert!(api::extract_http_traceparent(&headers).is_none());
    headers.insert(
        "traceparent",
        "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"
            .parse()
            .unwrap(),
    );
    let tp = api::extract_http_traceparent(&headers).expect("valid");
    assert_eq!(tp.trace_id, "4bf92f3577b34da6a3ce929d0e0e4736");
}

#[test]
fn extract_http_baggage_strips_guest_user_id() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    use axum::http::HeaderMap;
    let mut headers = HeaderMap::new();
    headers.insert(
        "baggage",
        "vam.origin=cli,vam.user.id=attacker".parse().unwrap(),
    );
    let entries = api::extract_http_baggage(&headers).expect("kept origin");
    assert_eq!(entries, vec![("vam.origin".to_string(), "cli".to_string())]);
    headers.insert("baggage", "vam.user.id=attacker".parse().unwrap());
    assert!(api::extract_http_baggage(&headers).is_none());
}

#[tokio::test]
async fn create_session_mints_ids() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let (store, secret) = bootstrap_store();
    let app = test_app(app_state(store));
    spawn_mock_guest_for_server_minted_session();

    let body = serde_json::json!({
        "idempotencyKey": "session-mint-key",
        "workspaceId": "ws-mint-ids",
        "storageProfileId": "default"
    });

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/sessions")
                .header("authorization", format!("Bearer {}", secret))
                .header("Idempotency-Key", "session-mint-key")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let resp_body = response_body(response).await;
    assert_eq!(status, StatusCode::OK, "{}", resp_body);
    let json: serde_json::Value = serde_json::from_str(&resp_body).unwrap();
    let session_id = json["session"]["sessionId"]["urn"]
        .as_str()
        .expect("sessionId")
        .to_string();
    let vm_id = json["session"]["vmId"].as_str().expect("vmId");
    assert!(
        session_id.starts_with("sess-") && session_id.len() > "sess-".len(),
        "server must mint session_id, got {session_id}"
    );
    assert_eq!(vm_id, format!("vm-{session_id}"));

    // Tear down so parallel api_tests are not blocked on a live workspace lock.
    let _ = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/v1/sessions/{session_id}/terminate"))
                .header("authorization", format!("Bearer {}", secret))
                .header("Idempotency-Key", "session-mint-term")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await;
}

#[tokio::test]
async fn create_session_rejects_client_supplied_ids() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let (store, secret) = bootstrap_store();
    let app = test_app(app_state_http_only(store));

    let cases = [
        serde_json::json!({
            "sessionId": {"urn": "sess-client-1"},
            "idempotencyKey": "reject-sess-id",
            "workspaceId": "mock",
            "storageProfileId": "default"
        }),
        serde_json::json!({
            "vmId": "vm-client-1",
            "idempotencyKey": "reject-vm-id",
            "workspaceId": "mock",
            "storageProfileId": "default"
        }),
        serde_json::json!({
            "sessionId": {"urn": "sess-client-2"},
            "vmId": "vm-client-2",
            "idempotencyKey": "reject-both-ids",
            "workspaceId": "mock",
            "storageProfileId": "default"
        }),
    ];

    for (i, body) in cases.into_iter().enumerate() {
        let idem = body["idempotencyKey"].as_str().unwrap();
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/sessions")
                    .header("authorization", format!("Bearer {}", secret))
                    .header("Idempotency-Key", idem)
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let resp_body = response_body(response).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "case {i}: expected 400, body={resp_body}"
        );
        assert!(
            resp_body.contains("INVALID_ARGUMENT")
                || resp_body.contains("InvalidArgument")
                || resp_body.contains("server-generated"),
            "case {i}: unexpected body={resp_body}"
        );
    }
}

#[tokio::test]
async fn idempotent_session_create_returns_same_session() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let (store, secret) = bootstrap_store();
    let app = test_app(app_state(store));
    spawn_mock_guest_for_server_minted_session();

    let body = serde_json::json!({
        "idempotencyKey": "session-idem-key",
        "workspaceId": "ws-idem-create",
        "storageProfileId": "default"
    });

    let req = || {
        Request::builder()
            .method("POST")
            .uri("/v1/sessions")
            .header("authorization", format!("Bearer {}", secret))
            .header("Idempotency-Key", "session-idem-key")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    };

    let first = app.clone().oneshot(req()).await.unwrap();
    let first_status = first.status();
    let first_body = response_body(first).await;
    assert_eq!(first_status, StatusCode::OK, "{}", first_body);
    let first_json: serde_json::Value = serde_json::from_str(&first_body).unwrap();
    assert!(first_json.get("session").is_some());
    let first_id = first_json["session"]["sessionId"]["urn"]
        .as_str()
        .unwrap()
        .to_string();
    let first_vm = first_json["session"]["vmId"].as_str().unwrap().to_string();
    assert!(first_id.starts_with("sess-"));
    assert_eq!(first_vm, format!("vm-{first_id}"));

    let second = app.clone().oneshot(req()).await.unwrap();
    assert_eq!(second.status(), StatusCode::OK);
    let second_body = response_body(second).await;
    let second_json: serde_json::Value = serde_json::from_str(&second_body).unwrap();
    let second_id = second_json["session"]["sessionId"]["urn"].as_str().unwrap();
    let second_vm = second_json["session"]["vmId"].as_str().unwrap();

    assert_eq!(first_id, second_id);
    assert_eq!(first_vm, second_vm);

    let _ = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/v1/sessions/{first_id}/terminate"))
                .header("authorization", format!("Bearer {}", secret))
                .header("Idempotency-Key", "session-idem-term")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await;
}

#[tokio::test]
async fn revoked_key_returns_403() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let (store, secret) = bootstrap_store();
    store.revoke_api_key("test-key").unwrap();
    let app = test_app(app_state_http_only(store));

    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/node")
                .header("authorization", format!("Bearer {}", secret))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn missing_session_returns_404() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let (store, secret) = bootstrap_store();
    let app = test_app(app_state_http_only(store));

    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/sessions/does-not-exist")
                .header("authorization", format!("Bearer {}", secret))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = response_body(response).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{}", body);
}

#[tokio::test]
async fn list_sessions_includes_agent_urn_from_run() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let (store, secret) = bootstrap_store();
    let agent_urn = "urn:vam:agent:local.vam.dev/acme/test-agent";
    store
        .create_session("sess-with-run", None, "vm-with-run", "assistant", "default")
        .unwrap();
    store
        .persist_run_spec(
            "urn:vam:run:local.vam.dev/acme/run-sess-1",
            "{}",
            Some("sess-with-run"),
            agent_urn,
            Some("run-sess-idem"),
            "User::\"tester\"",
        )
        .unwrap();

    let app = test_app(app_state_http_only(store));

    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/sessions")
                .header("authorization", format!("Bearer {}", secret))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response_body(response).await;
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    let sessions = json["sessions"].as_array().expect("sessions array");
    let matched = sessions
        .iter()
        .find(|s| s["sessionId"]["urn"].as_str() == Some("sess-with-run"))
        .expect("sess-with-run in list");
    assert_eq!(
        matched["agentUrn"].as_str(),
        Some(agent_urn),
        "body: {}",
        body
    );
}

#[test]
fn hash_api_key_is_stable() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let h1 = hash_api_key_secret("vam_sk_dev_test");
    let h2 = hash_api_key_secret("vam_sk_dev_test");
    assert_eq!(h1, h2);
    assert_ne!(h1, hash_api_key_secret("vam_sk_dev_other"));
}

#[tokio::test]
async fn put_secret_returns_metadata_without_plaintext() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let (store, secret) = bootstrap_store();
    let app = test_app(app_state_http_only(store));

    let put_body = serde_json::json!({
        "bindingId": "openai/api-key",
        "value": "sk-super-secret-openai",
        "kind": 1,
        "idempotencyKey": "secret-idem-1"
    });

    let put_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/v1/secrets")
                .header("authorization", format!("Bearer {}", secret))
                .header("Idempotency-Key", "secret-idem-1")
                .header("content-type", "application/json")
                .body(Body::from(put_body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        put_resp.status(),
        StatusCode::OK,
        "{}",
        response_body(put_resp).await
    );

    let list_resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/secrets")
                .header("authorization", format!("Bearer {}", secret))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(list_resp.status(), StatusCode::OK);
    let list_body = response_body(list_resp).await;
    assert!(!list_body.contains("sk-super-secret-openai"));
    assert!(list_body.contains("openai/api-key"));
    assert!(list_body.contains("fingerprint"));
}

#[tokio::test]
async fn put_secret_requires_auth_and_scope() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let (store, secret) = bootstrap_store();
    let app = test_app(app_state(store.clone()));

    let unauth = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/v1/secrets")
                .header("Idempotency-Key", "x")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(unauth.status(), StatusCode::UNAUTHORIZED);

    let limited_secret = mint_api_key_secret();
    store
        .create_api_key(
            "node-only",
            "node-only",
            &limited_secret,
            "User::\"limited\"",
            &["node:read".to_string()],
        )
        .unwrap();
    let app2 = test_app(app_state_http_only(store));
    let forbidden = app2
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/v1/secrets")
                .header("authorization", format!("Bearer {}", limited_secret))
                .header("Idempotency-Key", "x")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"bindingId":"x","value":"y","kind":1}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(forbidden.status(), StatusCode::FORBIDDEN);

    let _ = secret;
}

#[tokio::test]
async fn put_secret_overwrites_same_binding_id() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let (store, secret) = bootstrap_store();
    let app = test_app(app_state_http_only(store));

    let put = |idem: &str, value: &str| {
        let body = serde_json::json!({
            "bindingId": "openrouter/api-key",
            "value": value,
            "kind": 1,
            "idempotencyKey": idem
        });
        Request::builder()
            .method("PUT")
            .uri("/v1/secrets")
            .header("authorization", format!("Bearer {}", secret))
            .header("Idempotency-Key", idem)
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    };

    let first = app
        .clone()
        .oneshot(put("idem-a", "first-value"))
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::OK);
    let first_json: serde_json::Value =
        serde_json::from_slice(&hyper::body::to_bytes(first.into_body()).await.unwrap()).unwrap();
    let fp1 = first_json["secret"]["fingerprint"]
        .as_str()
        .unwrap()
        .to_string();

    let second = app.oneshot(put("idem-b", "second-value")).await.unwrap();
    assert_eq!(second.status(), StatusCode::OK);
    let second_json: serde_json::Value =
        serde_json::from_slice(&hyper::body::to_bytes(second.into_body()).await.unwrap()).unwrap();
    let fp2 = second_json["secret"]["fingerprint"]
        .as_str()
        .unwrap()
        .to_string();

    assert_ne!(fp1, fp2);
}

fn member_secret_for(store: &SqliteWorkflowStore, user_id: &str, username: &str) -> String {
    store.create_user(user_id, username).unwrap();
    let secret = mint_api_key_secret();
    let scopes: Vec<String> = MEMBER_SCOPES.iter().map(|s| s.to_string()).collect();
    store
        .create_api_key_owned(
            &format!("{user_id}-key"),
            username,
            &secret,
            &format!("User::\"{username}\""),
            &scopes,
            user_id,
        )
        .unwrap();
    secret
}

#[tokio::test]
async fn two_users_can_put_same_binding_without_overwrite() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let alice = member_secret_for(&store, "alice", "alice");
    let bob = member_secret_for(&store, "bob", "bob");
    let app = test_app(app_state_http_only(store.clone()));

    let put = |token: &str, value: &str, idem: &str| {
        let body = serde_json::json!({
            "bindingId": "jira/auth-header",
            "value": value,
            "kind": 2,
            "scope": 2,
            "idempotencyKey": idem
        });
        Request::builder()
            .method("PUT")
            .uri("/v1/secrets")
            .header("authorization", format!("Bearer {}", token))
            .header("Idempotency-Key", idem)
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    };

    let alice_put = app
        .clone()
        .oneshot(put(&alice, "alice-header", "alice-idem"))
        .await
        .unwrap();
    assert_eq!(alice_put.status(), StatusCode::OK);
    let bob_put = app
        .clone()
        .oneshot(put(&bob, "bob-header", "bob-idem"))
        .await
        .unwrap();
    assert_eq!(bob_put.status(), StatusCode::OK);

    let alice_get = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/secrets/jira%2Fauth-header?scope=user")
                .header("authorization", format!("Bearer {}", alice))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(alice_get.status(), StatusCode::OK);
    let alice_body = response_body(alice_get).await;
    assert!(alice_body.contains(&vam_orchestrator::SecretsVault::fingerprint("alice-header")));
    assert!(!alice_body.contains(&vam_orchestrator::SecretsVault::fingerprint("bob-header")));

    let bob_list = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/secrets")
                .header("authorization", format!("Bearer {}", bob))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let bob_list_body = response_body(bob_list).await;
    assert!(bob_list_body.contains(&vam_orchestrator::SecretsVault::fingerprint("bob-header")));
    assert!(!bob_list_body.contains(&vam_orchestrator::SecretsVault::fingerprint("alice-header")));
}

#[tokio::test]
async fn user_secret_get_does_not_fall_back_to_org() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let (store, operator) = bootstrap_operator_store();
    let alice = member_secret_for(&store, "alice", "alice");
    let app = test_app(app_state_http_only(store));

    let org_put = serde_json::json!({
        "bindingId": "jira/auth-header",
        "value": "org-header",
        "kind": 2,
        "scope": 1,
        "idempotencyKey": "org-jira"
    });
    let org_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/v1/secrets")
                .header("authorization", format!("Bearer {}", operator))
                .header("Idempotency-Key", "org-jira")
                .header("content-type", "application/json")
                .body(Body::from(org_put.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        org_resp.status(),
        StatusCode::OK,
        "{}",
        response_body(org_resp).await
    );

    let user_miss = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/secrets/jira%2Fauth-header?scope=user")
                .header("authorization", format!("Bearer {}", alice))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(user_miss.status(), StatusCode::NOT_FOUND);

    let org_get = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/secrets/jira%2Fauth-header?scope=org")
                .header("authorization", format!("Bearer {}", alice))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(org_get.status(), StatusCode::OK);
    let org_body = response_body(org_get).await;
    assert!(org_body.contains(&vam_orchestrator::SecretsVault::fingerprint("org-header")));
}

#[tokio::test]
async fn non_admin_cannot_write_org_scope_secret() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let alice = member_secret_for(&store, "alice", "alice");
    let app = test_app(app_state_http_only(store));
    let body = serde_json::json!({
        "bindingId": "jira/auth-header",
        "value": "nope",
        "kind": 2,
        "scope": 1,
        "idempotencyKey": "alice-org"
    });
    let resp = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/v1/secrets")
                .header("authorization", format!("Bearer {}", alice))
                .header("Idempotency-Key", "alice-org")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

fn bootstrap_operator_store() -> (Arc<SqliteWorkflowStore>, String) {
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let secret = mint_api_key_secret();
    let scopes: Vec<String> = BOOTSTRAP_SCOPES.iter().map(|s| s.to_string()).collect();
    store
        .create_api_key(
            BOOTSTRAP_KEY_ID,
            "bootstrap",
            &secret,
            BOOTSTRAP_PRINCIPAL,
            &scopes,
        )
        .unwrap();
    (store, secret)
}

#[tokio::test]
async fn run_events_accepts_url_encoded_run_urn() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let (store, secret) = bootstrap_store();
    let run_urn = "urn:vam:run:local.vam.dev/acme/run-events-test";
    store
        .create_run(&vam_spec::v1::ResourceId {
            urn: run_urn.to_string(),
        })
        .unwrap();
    let app = test_app(app_state_http_only(store));

    let encoded = run_urn
        .chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => c.to_string(),
            _ => format!("%{:02X}", c as u32),
        })
        .collect::<String>();

    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/v1/runs/{}/events", encoded))
                .header("authorization", format!("Bearer {}", secret))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "{}",
        response_body(response).await
    );
}

fn sample_run_body(
    agent_urn: &str,
    workspace: &str,
    idem: &str,
    run_urn: &str,
) -> serde_json::Value {
    serde_json::json!({
        "spec": {
            "runId": {"urn": run_urn},
            "agentRef": {
                "resourceId": {"urn": agent_urn},
                "version": {"major": 1, "minor": 0, "patch": 0},
                "digest": {"algorithm": "ALGORITHM_SHA256", "hexHash": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"}
            },
            "playbookRef": {
                "resourceId": {"urn": "urn:vam:playbook:local.vam.dev/acme/test-pb"},
                "version": {"major": 1, "minor": 0, "patch": 0},
                "digest": {"algorithm": "ALGORITHM_SHA256", "hexHash": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"}
            },
            "inputs": {"fields": {"prompt": {"stringValue": "hello from api test"}}},
            "workspaceRefUri": format!("volume://{}", workspace),
            "idempotencyKey": idem
        },
        "inlineAgent": {
            "resourceId": {"urn": agent_urn},
            "displayName": "Test Agent",
            "description": "inline",
            "role": "Tester",
            "systemInstructions": "test",
            "allowAnyPlaybook": true
        }
    })
}

async fn put_test_agent(app: &axum::Router, secret: &str, urn: &str, name: &str) {
    let agent_json = serde_json::json!({
        "spec": {
            "resourceId": {"urn": urn},
            "displayName": name,
            "description": "API test agent",
            "role": "Tester",
            "systemInstructions": "test instructions",
            "allowAnyPlaybook": true
        },
        "idempotencyKey": format!("agent-idem-{}", urn)
    });
    let put_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/v1/agents")
                .header("authorization", format!("Bearer {}", secret))
                .header("Idempotency-Key", format!("agent-idem-{}", urn))
                .header("content-type", "application/json")
                .body(Body::from(agent_json.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        put_resp.status(),
        StatusCode::OK,
        "{}",
        response_body(put_resp).await
    );
}

#[tokio::test]
async fn a_full_run_request_on_a_busy_workspace_is_not_queued_only_a_prompt_is() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let (store, secret) = bootstrap_store();
    let agent_urn = "urn:vam:agent:local.vam.dev/acme/test-agent";
    let busy_ws = store
        .create_space(LOCAL_USER_ID, Some(agent_urn), "busy-ws", "run", None)
        .unwrap()
        .space_id;
    store
        .create_session("sess-busy-1", None, "vm-busy-1", &busy_ws, "default")
        .unwrap();
    store
        .update_observed_state(
            "sess-busy-1",
            vam_orchestrator::session::ObservedState::Booting,
        )
        .unwrap();
    store
        .update_observed_state(
            "sess-busy-1",
            vam_orchestrator::session::ObservedState::Running,
        )
        .unwrap();
    store
        .persist_run_spec(
            "urn:vam:run:local.vam.dev/acme/run-busy-1",
            "{}",
            Some("sess-busy-1"),
            agent_urn,
            Some("run-busy-idem"),
            "User::\"tester\"",
        )
        .unwrap();
    store
        .create_run(&vam_spec::v1::ResourceId {
            urn: "urn:vam:run:local.vam.dev/acme/run-busy-1".to_string(),
        })
        .unwrap();

    let app = test_app(app_state_http_only(store));
    put_test_agent(&app, &secret, agent_urn, "Test Agent").await;

    // The live task is on the default thread; this message names another one.
    let mut run_body = sample_run_body(
        agent_urn,
        "busy-ws",
        "run-busy-idem-2",
        "urn:vam:run:local.vam.dev/acme/run-busy-2",
    );
    run_body["spec"]["threadRef"] = serde_json::json!("other-thread");
    run_body["spec"]["spaceId"] = serde_json::json!(busy_ws);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/runs")
                .header("authorization", format!("Bearer {}", secret))
                .header("Idempotency-Key", "run-busy-idem-2")
                .header("content-type", "application/json")
                .body(Body::from(run_body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    // Owner decision D: another thread on a busy workspace is queued, not refused.
    // (The message carries an inline agent and a playbook, which a queued message may not.)
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn create_run_on_reserved_workspace_names_parked_run_and_cancel() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let (store, secret) = bootstrap_store();
    let agent_urn = "urn:vam:agent:local.vam.dev/acme/test-agent";
    let parked_ws = store
        .create_space(LOCAL_USER_ID, Some(agent_urn), "parked-ws", "run", None)
        .unwrap()
        .space_id;
    let parked_run = "urn:vam:run:local.vam.dev/acme/run-parked";
    store
        .create_run(&vam_spec::v1::ResourceId {
            urn: parked_run.to_string(),
        })
        .unwrap();
    store
        .reserve_workspace(&parked_ws, parked_run, i64::MAX)
        .unwrap();

    let app = test_app(app_state_http_only(store));
    put_test_agent(&app, &secret, agent_urn, "Test Agent").await;

    let mut run_body = sample_run_body(
        agent_urn,
        "parked-ws",
        "run-parked-conflict",
        "urn:vam:run:local.vam.dev/acme/run-parked-next",
    );
    run_body["spec"]["spaceId"] = serde_json::json!(parked_ws);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/runs")
                .header("authorization", format!("Bearer {}", secret))
                .header("Idempotency-Key", "run-parked-conflict")
                .header("content-type", "application/json")
                .body(Body::from(run_body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let body = response_body(response).await;
    let cancel = format!("vam run cancel {parked_run}");
    assert!(
        body.contains(parked_run),
        "typed error must name the parked run: {body}"
    );
    assert!(
        body.contains(&cancel),
        "typed error must name the cancel command: {body}"
    );
    assert!(body.contains(&parked_ws), "{}", body);
    assert!(body.contains("retryable"), "{}", body);
}

// ---------------------------------------------------------------------------
// Inbox: a message to a running task (AR-3.5)
// ---------------------------------------------------------------------------

const INBOX_AGENT: &str = "urn:vam:agent:local.vam.dev/acme/test-agent";
const INBOX_RUN: &str = "urn:vam:run:local.vam.dev/acme/run-live-1";

/// The Space and thread ids of the live tasks the tests made, by workspace label (`name`) and
/// by `name/thread`. Names are labels only: a request names its place by id.
static PLACES: std::sync::Mutex<Vec<(String, String, String)>> = std::sync::Mutex::new(Vec::new());

fn remember_place(key: &str, space_id: &str, thread_id: &str) {
    PLACES
        .lock()
        .unwrap()
        .push((key.to_string(), space_id.to_string(), thread_id.to_string()));
}

/// The ids of the Space labelled `name` (and of its thread `thread`, when it was made).
fn place_of(name: &str, thread: &str) -> Option<(String, Option<String>)> {
    let places = PLACES.lock().unwrap();
    let space = places.iter().find(|(k, _, _)| k == name)?.1.clone();
    let thread_id = places
        .iter()
        .find(|(k, _, _)| *k == format!("{name}/{thread}"))
        .map(|(_, _, t)| t.clone());
    Some((space, thread_id))
}

/// The Space id of the live task made under `name`.
fn space_of(name: &str) -> String {
    place_of(name, "").expect("a live task under this name").0
}

/// A task that is running on the Space labelled `name`, thread labelled `thread`, owned by
/// `owner`.
fn live_task(store: &SqliteWorkflowStore, name: &str, thread: &str, owner: &str) -> String {
    let space = store
        .create_space(owner, Some(INBOX_AGENT), name, "run", None)
        .unwrap();
    let ws = space.space_id.clone();
    let thread_row = store.create_thread(owner, &ws, thread, "run").unwrap();
    remember_place(name, &ws, &thread_row.thread_id);
    remember_place(&format!("{name}/{thread}"), &ws, &thread_row.thread_id);
    let session = format!("sess-live-{name}");
    store
        .create_session(&session, None, &format!("vm-{name}"), &ws, "default")
        .unwrap();
    for state in [
        vam_orchestrator::session::ObservedState::Booting,
        vam_orchestrator::session::ObservedState::Running,
    ] {
        store.update_observed_state(&session, state).unwrap();
    }
    // A stored spec carries the place as `create_run` resolved it: the ids, and the thread
    // directory (its id).
    let spec = serde_json::json!({
        "workspaceRefUri": format!("volume://{ws}"),
        "threadRef": thread_row.thread_id,
        "spaceId": ws,
        "threadId": thread_row.thread_id,
        "inputs": {"prompt": "start", "model": "mock-model"},
    });
    store
        .persist_run_spec_with_owner(
            INBOX_RUN,
            &spec.to_string(),
            Some(&session),
            INBOX_AGENT,
            Some("live-start"),
            "User::\"tester\"",
            owner,
        )
        .unwrap();
    store
        .create_run(&vam_spec::v1::ResourceId {
            urn: INBOX_RUN.to_string(),
        })
        .unwrap();
    session
}

/// A run request for the live task made under `name`: it names the Space and thread by id when
/// they exist; a thread label with no thread yet starts a new thread in that Space.
fn steer_body(name: &str, thread: &str, key: &str, inputs: serde_json::Value) -> serde_json::Value {
    let mut spec = serde_json::json!({
        "agentRef": {"resourceId": {"urn": INBOX_AGENT}},
        "inputs": inputs,
        "idempotencyKey": key,
    });
    match place_of(name, thread) {
        Some((space, Some(thread_id))) => {
            spec["spaceId"] = space.into();
            spec["threadId"] = thread_id.into();
        }
        Some((space, None)) => {
            spec["spaceId"] = space.into();
            spec["threadRef"] = thread.into();
        }
        None => {
            spec["workspaceRefUri"] = format!("volume://{name}").into();
            spec["threadRef"] = thread.into();
        }
    }
    serde_json::json!({ "spec": spec })
}

async fn post_json(
    app: &axum::Router,
    uri: &str,
    secret: &str,
    key: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let response = app
        .clone()
        .oneshot(bearer_json("POST", uri, secret, key, body))
        .await
        .unwrap();
    let status = response.status();
    let text = response_body(response).await;
    (
        status,
        serde_json::from_str(&text).unwrap_or(serde_json::Value::String(text)),
    )
}

/// An int64 in proto JSON is a string; accept either.
fn as_i64(v: &serde_json::Value) -> i64 {
    v.as_i64()
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        .unwrap_or(-1)
}

async fn inbox_app() -> (Arc<SqliteWorkflowStore>, axum::Router, String) {
    let (store, secret) = bootstrap_store();
    let app = test_app(app_state_http_only(store.clone()));
    put_test_agent(&app, &secret, INBOX_AGENT, "Test Agent").await;
    (store, app, secret)
}

#[tokio::test]
async fn a_steer_on_the_live_thread_is_accepted_with_increasing_seq() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let (store, app, secret) = inbox_app().await;
    live_task(&store, "steer-ws", "main", LOCAL_USER_ID);

    for n in 1..=3 {
        let (status, body) = post_json(
            &app,
            "/v1/runs",
            &secret,
            &format!("steer-{n}"),
            steer_body(
                "steer-ws",
                "main",
                &format!("steer-{n}"),
                serde_json::json!({"prompt": format!("change {n}"), "model": "mock-model"}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        assert_eq!(as_i64(&body["inbox"]["seq"]), n, "{body}");
        assert_eq!(body["inbox"]["mode"], "STEER", "{body}");
        assert_eq!(body["inbox"]["runId"]["urn"], INBOX_RUN, "{body}");
        assert_eq!(body["status"]["runId"]["urn"], INBOX_RUN, "{body}");
    }
    let rows = store.list_inbox_messages(INBOX_RUN).unwrap();
    assert_eq!(rows.len(), 3);
    assert!(rows.iter().all(|r| r.state == "pending"));
    // No second run was started, and the events carry no text.
    assert_eq!(store.list_runs_for_owner(LOCAL_USER_ID).unwrap().len(), 1);
    let events = store.read_outbox_for_run(INBOX_RUN, 0).unwrap();
    let received: Vec<_> = events
        .iter()
        .filter(|e| e.event_type == "vam.v1.inbox.received")
        .collect();
    assert_eq!(received.len(), 3);
    assert!(received.iter().all(|e| !e.payload.contains("change")));
}

#[tokio::test]
async fn the_same_key_twice_is_one_message_with_one_event() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let (store, app, secret) = inbox_app().await;
    live_task(&store, "dup-ws", "main", LOCAL_USER_ID);
    let body = steer_body(
        "dup-ws",
        "main",
        "same",
        serde_json::json!({"prompt": "once"}),
    );
    let (s1, first) = post_json(&app, "/v1/runs", &secret, "same", body.clone()).await;
    let (s2, again) = post_json(&app, "/v1/runs", &secret, "same", body).await;
    assert_eq!((s1, s2), (StatusCode::ACCEPTED, StatusCode::ACCEPTED));
    assert_eq!(as_i64(&first["inbox"]["seq"]), 1);
    assert_eq!(as_i64(&again["inbox"]["seq"]), 1);
    assert_eq!(store.list_inbox_messages(INBOX_RUN).unwrap().len(), 1);
    let events = store.read_outbox_for_run(INBOX_RUN, 0).unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|e| e.event_type == "vam.v1.inbox.received")
            .count(),
        1
    );
}

#[tokio::test]
async fn a_message_on_another_thread_is_queued_as_a_run_of_its_own() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let (store, app, secret) = inbox_app().await;
    live_task(&store, "other-ws", "main", LOCAL_USER_ID);
    let (status, body) = post_json(
        &app,
        "/v1/runs",
        &secret,
        "other-1",
        steer_body(
            "other-ws",
            "side",
            "other-1",
            serde_json::json!({"prompt": "hi"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    // The answer names the run the message will start, and that run can be read.
    assert_eq!(body["inbox"]["mode"], "QUEUE", "{body}");
    let queued = body["inbox"]["runId"]["urn"].as_str().unwrap().to_string();
    assert_ne!(queued, INBOX_RUN);
    assert_eq!(body["status"]["runId"]["urn"], queued.as_str());
    assert_eq!(body["status"]["state"], "RUN_STATE_PENDING", "{body}");
    assert_eq!(body["threadRef"], "side", "{body}");
    assert_eq!(body["status"]["threadRef"], "side", "{body}");
    assert_eq!(body["workspaceId"], space_of("other-ws").as_str(), "{body}");
    assert_eq!(
        body["status"]["spaceId"],
        space_of("other-ws").as_str(),
        "{body}"
    );
    let get = bearer_get(&format!("/v1/runs/{}", encode_run_path(&queued)), &secret);
    let response = app.clone().oneshot(get).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    // One row, queued behind the live run, with the request to start; no steer.
    let rows = store.list_inbox_messages(INBOX_RUN).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        (rows[0].mode.as_str(), rows[0].state.as_str()),
        ("QUEUE", "queued")
    );
    // The queued row names the new thread by its id (its directory), and the request it holds
    // carries both ids, so it cannot land elsewhere.
    assert_eq!(
        rows[0].thread_ref,
        body["threadId"].as_str().unwrap(),
        "{body}"
    );
    assert!(vam_orchestrator::spaces::is_thread_id(&rows[0].thread_ref));
    let queued_spec = rows[0].queued_spec_json.as_deref().unwrap();
    assert!(queued_spec.contains(&space_of("other-ws")), "{queued_spec}");
    assert!(queued_spec.contains(&rows[0].thread_ref), "{queued_spec}");
    // The same key again is the same run.
    let (status, again) = post_json(
        &app,
        "/v1/runs",
        &secret,
        "other-1",
        steer_body(
            "other-ws",
            "side",
            "other-1",
            serde_json::json!({"prompt": "hi"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{again}");
    assert_eq!(again["status"]["runId"]["urn"], queued.as_str());
    assert_eq!(store.list_inbox_messages(INBOX_RUN).unwrap().len(), 1);
}

#[tokio::test]
async fn a_finalizing_run_is_still_refused_as_busy() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let (store, app, secret) = inbox_app().await;
    let session = live_task(&store, "fin-ws", "main", LOCAL_USER_ID);
    store.session_ends.track(&session);
    store.session_ends.begin_finalize(&session);
    let (status, body) = post_json(
        &app,
        "/v1/runs",
        &secret,
        "fin-1",
        steer_body(
            "fin-ws",
            "main",
            "fin-1",
            serde_json::json!({"prompt": "late"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(store.list_inbox_messages(INBOX_RUN).unwrap().is_empty());
}

#[tokio::test]
async fn a_parked_run_takes_the_message_and_stays_parked() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let (store, app, secret) = inbox_app().await;
    let space = store
        .create_space(LOCAL_USER_ID, Some(INBOX_AGENT), "park-ws", "run", None)
        .unwrap();
    let ws = space.space_id.clone();
    let thread = store
        .create_thread(LOCAL_USER_ID, &ws, "main", "run")
        .unwrap();
    remember_place("park-ws", &ws, &thread.thread_id);
    remember_place("park-ws/main", &ws, &thread.thread_id);
    let spec = serde_json::json!({
        "workspaceRefUri": format!("volume://{ws}"),
        "threadRef": thread.thread_id,
        "spaceId": ws,
        "threadId": thread.thread_id,
        "inputs": {"prompt": "start"},
    });
    store
        .persist_run_spec_with_owner(
            INBOX_RUN,
            &spec.to_string(),
            None,
            INBOX_AGENT,
            Some("park-start"),
            "User::\"tester\"",
            LOCAL_USER_ID,
        )
        .unwrap();
    store
        .create_run(&vam_spec::v1::ResourceId {
            urn: INBOX_RUN.to_string(),
        })
        .unwrap();
    store.update_run_state(INBOX_RUN, "PAUSED").unwrap();
    store.reserve_workspace(&ws, INBOX_RUN, i64::MAX).unwrap();

    let (status, body) = post_json(
        &app,
        "/v1/runs",
        &secret,
        "park-1",
        steer_body(
            "park-ws",
            "main",
            "park-1",
            serde_json::json!({"prompt": "after"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(as_i64(&body["inbox"]["seq"]), 1);
    assert_eq!(store.get_run_state(INBOX_RUN).unwrap(), "PAUSED");
    assert!(store
        .active_workspace_reservation(&ws, 0)
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn queue_mode_holds_the_message_for_the_next_task() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let (store, app, secret) = inbox_app().await;
    live_task(&store, "queue-ws", "main", LOCAL_USER_ID);
    let mut body = steer_body(
        "queue-ws",
        "main",
        "q-1",
        serde_json::json!({"prompt": "later"}),
    );
    body["spec"]["inboxMode"] = serde_json::json!("QUEUE");
    let (status, reply) = post_json(&app, "/v1/runs", &secret, "q-1", body).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{reply}");
    assert_eq!(reply["inbox"]["mode"], "QUEUE");
    let queued = reply["inbox"]["runId"]["urn"].as_str().unwrap();
    assert_ne!(queued, INBOX_RUN, "the receipt names the run it will start");
    let rows = store.list_inbox_messages(INBOX_RUN).unwrap();
    assert_eq!(
        (rows[0].mode.as_str(), rows[0].state.as_str()),
        ("QUEUE", "queued")
    );
    assert!(rows[0].queued_spec_json.is_some());
    assert_eq!(rows[0].started_run_id.as_deref(), Some(queued));
    // The live run's pull never sees it.
    assert!(store
        .pending_steers_after(INBOX_RUN, 0, 1 << 20)
        .unwrap()
        .is_empty());
    // By run id too.
    let uri = format!("/v1/runs/{}/messages", encode_run_path(INBOX_RUN));
    let (status, by_id) = post_json(
        &app,
        &uri,
        &secret,
        "q-2",
        serde_json::json!({"text": "and then this", "mode": "QUEUE"}),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{by_id}");
    assert_eq!(by_id["inbox"]["mode"], "QUEUE");
    assert_ne!(by_id["inbox"]["runId"]["urn"], INBOX_RUN);
}

#[tokio::test]
async fn a_steer_carries_only_a_prompt_and_the_runs_own_model() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let (store, app, secret) = inbox_app().await;
    live_task(&store, "strict-ws", "main", LOCAL_USER_ID);
    let cases = [
        ("extra", serde_json::json!({"prompt": "x", "harness": "y"})),
        (
            "model",
            serde_json::json!({"prompt": "x", "model": "gpt-9"}),
        ),
        ("empty", serde_json::json!({"prompt": "  "})),
        (
            "big",
            serde_json::json!({"prompt": "z".repeat(32 * 1024 + 1)}),
        ),
    ];
    for (key, inputs) in cases {
        let (status, body) = post_json(
            &app,
            "/v1/runs",
            &secret,
            key,
            steer_body("strict-ws", "main", key, inputs),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{key}: {body}");
    }
    let mut inline = steer_body(
        "strict-ws",
        "main",
        "inline",
        serde_json::json!({"prompt": "x"}),
    );
    inline["inlineAgent"] = serde_json::json!({"resourceId": {"urn": INBOX_AGENT}});
    let (status, body) = post_json(&app, "/v1/runs", &secret, "inline", inline).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(store.list_inbox_messages(INBOX_RUN).unwrap().is_empty());
    // The model the run was pinned to is fine.
    let (status, body) = post_json(
        &app,
        "/v1/runs",
        &secret,
        "flat",
        steer_body(
            "strict-ws",
            "main",
            "flat",
            serde_json::json!({"prompt": "ok", "model": "mock-model"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
}

#[tokio::test]
async fn the_run_messages_endpoint_accepts_dedups_and_caps_at_twenty() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let (store, app, secret) = inbox_app().await;
    live_task(&store, "msg-ws", "main", LOCAL_USER_ID);
    let uri = format!("/v1/runs/{}/messages", encode_run_path(INBOX_RUN));

    let (status, first) = post_json(
        &app,
        &uri,
        &secret,
        "m-1",
        serde_json::json!({"text": "one", "mode": "STEER"}),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{first}");
    assert_eq!(as_i64(&first["inbox"]["seq"]), 1);
    assert_eq!(first["inbox"]["mode"], "STEER");
    assert_eq!(first["status"]["runId"]["urn"], INBOX_RUN);
    // No mode means STEER; the same key is the same message.
    let (_, again) = post_json(
        &app,
        &uri,
        &secret,
        "m-1",
        serde_json::json!({"text": "one"}),
    )
    .await;
    assert_eq!(as_i64(&again["inbox"]["seq"]), 1);
    assert_eq!(store.list_inbox_messages(INBOX_RUN).unwrap().len(), 1);
    // A key in the body wins over the header.
    let (_, keyed) = post_json(
        &app,
        &uri,
        &secret,
        "ignored",
        serde_json::json!({"text": "two", "idempotencyKey": "body-key"}),
    )
    .await;
    assert_eq!(as_i64(&keyed["inbox"]["seq"]), 2);

    for n in 3..=20 {
        let (status, _) = post_json(
            &app,
            &uri,
            &secret,
            &format!("m-{n}"),
            serde_json::json!({"text": format!("msg {n}")}),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED);
    }
    let (status, body) = post_json(
        &app,
        &uri,
        &secret,
        "m-21",
        serde_json::json!({"text": "one too many"}),
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert!(body.to_string().contains("inbox_full"), "{body}");
    // A repeat of an accepted key still answers once the inbox is full.
    let (status, _) = post_json(
        &app,
        &uri,
        &secret,
        "m-5",
        serde_json::json!({"text": "msg 5"}),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    // The same cap holds on the run-create door.
    let (status, _) = post_json(
        &app,
        "/v1/runs",
        &secret,
        "c-21",
        steer_body("msg-ws", "main", "c-21", serde_json::json!({"prompt": "x"})),
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn the_run_messages_endpoint_is_owner_only_and_refuses_an_ended_run() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let (store, _secret) = bootstrap_store();
    let (alice, bob) = two_member_keys(&store);
    let app = test_app(app_state_http_only(store.clone()));
    live_task(&store, "own-ws", "main", "alice");
    let uri = format!("/v1/runs/{}/messages", encode_run_path(INBOX_RUN));

    let (status, body) =
        post_json(&app, &uri, &bob, "b-1", serde_json::json!({"text": "hi"})).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert!(store.list_inbox_messages(INBOX_RUN).unwrap().is_empty());

    let (status, body) =
        post_json(&app, &uri, &alice, "a-1", serde_json::json!({"text": "hi"})).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");

    // A body that names another run, no text, and no key are 400s.
    let (status, _) = post_json(
        &app,
        &uri,
        &alice,
        "a-2",
        serde_json::json!({"runId": {"urn": "urn:vam:run:local.vam.dev/acme/other"}, "text": "x"}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = post_json(&app, &uri, &alice, "a-3", serde_json::json!({"text": ""})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(&uri)
                .header("authorization", format!("Bearer {alice}"))
                .header("content-type", "application/json")
                .body(Body::from(r#"{"text":"no key"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    store.update_run_state(INBOX_RUN, "SUCCEEDED").unwrap();
    let (status, body) = post_json(
        &app,
        &uri,
        &alice,
        "a-4",
        serde_json::json!({"text": "late"}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(store.list_inbox_messages(INBOX_RUN).unwrap().len(), 1);
}

/// Counterpart to `second_create_run_while_session_active_returns_409`: after HITL
/// resume finalization marks the session Terminated, a follow-up create_run is accepted.
#[tokio::test]
async fn follow_up_create_run_after_hitl_session_terminated_is_accepted() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let (store, secret) = bootstrap_store();
    let agent_urn = "urn:vam:agent:local.vam.dev/acme/test-agent";
    let hitl_ws =
        vam_storage::apply_agent_workspace_id(LOCAL_USER_ID, agent_urn, "hitl-ws").unwrap();
    store
        .create_session("sess-hitl-done", None, "vm-hitl-done", &hitl_ws, "default")
        .unwrap();
    store
        .update_observed_state(
            "sess-hitl-done",
            vam_orchestrator::session::ObservedState::Booting,
        )
        .unwrap();
    store
        .update_observed_state(
            "sess-hitl-done",
            vam_orchestrator::session::ObservedState::Running,
        )
        .unwrap();
    // Terminal HITL finalize → terminate_session marks Terminated.
    store
        .update_observed_state(
            "sess-hitl-done",
            vam_orchestrator::session::ObservedState::Terminated,
        )
        .unwrap();

    let app = test_app(app_state(store));
    put_test_agent(&app, &secret, agent_urn, "Test Agent").await;

    let run_body = sample_run_body(
        agent_urn,
        "hitl-ws",
        "run-hitl-followup-idem",
        "urn:vam:run:local.vam.dev/acme/run-hitl-followup",
    );
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/runs")
                .header("authorization", format!("Bearer {}", secret))
                .header("Idempotency-Key", "run-hitl-followup-idem")
                .header("content-type", "application/json")
                .body(Body::from(run_body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_ne!(
        response.status(),
        StatusCode::CONFLICT,
        "follow-up after HITL finalize must not 409 AgentRunning: {}",
        response_body(response).await
    );
}

fn bind_workspace(drive9_root: &std::path::Path, workspace_id: &str, agent_urn: &str) {
    let layout = vam_storage::Drive9Layout::new(drive9_root);
    let chunk_store = vam_storage::ChunkStore::new(layout.clone());
    let manifest_manager = vam_storage::ManifestManager::new(layout, chunk_store);
    let mat = drive9_root.join(format!("mat-{workspace_id}"));
    vam_storage::ensure_workspace_json(
        &mat,
        workspace_id,
        Some(vam_storage::WorkspaceAgentBind {
            agent_urn,
            agent_digest: "digest",
            agent_version: None,
        }),
    )
    .unwrap();
    let manifest = manifest_manager
        .build_from_dir(workspace_id, &mat, None)
        .unwrap();
    let hash = manifest_manager.commit_manifest(&manifest).unwrap();
    manifest_manager.write_head(workspace_id, &hash).unwrap();
}

#[tokio::test]
async fn agent_workspace_mismatch_returns_409() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let (store, secret) = bootstrap_store();
    let bound_agent = "urn:vam:agent:local.vam.dev/acme/agent-a";
    let other_agent = "urn:vam:agent:local.vam.dev/acme/agent-b";

    let drive9_root = tempfile::tempdir().unwrap();
    std::env::set_var("VAM_DRIVE9_ROOT", drive9_root.path());

    // B's Space holds a volume that is already bound to A, so the run is rejected and the
    // binding is not rewritten.
    let bound_ws = store
        .create_space(LOCAL_USER_ID, None, "bound-ws", "run", None)
        .unwrap()
        .space_id;
    bind_workspace(drive9_root.path(), &bound_ws, bound_agent);

    let app = test_app(app_state_http_only(store.clone()));
    put_test_agent(&app, &secret, bound_agent, "Agent A").await;
    put_test_agent(&app, &secret, other_agent, "Agent B").await;

    let mut run_body = sample_run_body(
        other_agent,
        "bound-ws",
        "run-mismatch-idem",
        "urn:vam:run:local.vam.dev/acme/run-mismatch",
    );
    run_body["spec"]["spaceId"] = serde_json::json!(bound_ws);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/runs")
                .header("authorization", format!("Bearer {}", secret))
                .header("Idempotency-Key", "run-mismatch-idem")
                .header("content-type", "application/json")
                .body(Body::from(run_body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let body = response_body(response).await;
    assert!(
        body.contains("does not match workspace-bound agent"),
        "{}",
        body
    );
    assert!(body.contains(bound_agent), "{}", body);
    assert!(body.contains(other_agent), "{}", body);

    std::env::remove_var("VAM_DRIVE9_ROOT");
}

#[tokio::test]
async fn two_agents_sharing_volume_main_get_different_workspace_ids() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let (store, secret) = bootstrap_store();
    seed_test_playbook(&store);
    let agent_a = "urn:vam:agent:local.vam.dev/acme/agent-a";
    let agent_b = "urn:vam:agent:local.vam.dev/acme/agent-b";
    let app = test_app(app_state(store.clone()));

    let a_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/runs")
                .header("authorization", format!("Bearer {secret}"))
                .header("Idempotency-Key", "main-a")
                .header("content-type", "application/json")
                .body(Body::from(
                    sample_run_body(
                        agent_a,
                        "main",
                        "main-a",
                        "urn:vam:run:local.vam.dev/acme/run-main-a",
                    )
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let a_status = a_resp.status();
    let a_body = response_body(a_resp).await;
    assert_eq!(a_status, StatusCode::ACCEPTED, "{a_body}");
    let b_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/runs")
                .header("authorization", format!("Bearer {secret}"))
                .header("Idempotency-Key", "main-b")
                .header("content-type", "application/json")
                .body(Body::from(
                    sample_run_body(
                        agent_b,
                        "main",
                        "main-b",
                        "urn:vam:run:local.vam.dev/acme/run-main-b",
                    )
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let b_status = b_resp.status();
    let b_body = response_body(b_resp).await;
    assert_eq!(b_status, StatusCode::ACCEPTED, "{b_body}");

    let a_json: serde_json::Value = serde_json::from_str(&a_body).unwrap();
    let b_json: serde_json::Value = serde_json::from_str(&b_body).unwrap();
    let a_ws = a_json["workspaceId"].as_str().expect("workspaceId");
    let b_ws = b_json["workspaceId"].as_str().expect("workspaceId");
    // Each agent has its own default Space; the label "main" is not a key.
    let (expect_a, expect_b) = (a_ws.to_string(), b_ws.to_string());
    assert!(
        vam_orchestrator::spaces::is_space_id(a_ws) && vam_orchestrator::spaces::is_space_id(b_ws)
    );
    assert_ne!(a_ws, b_ws);
    assert_eq!(a_json["status"]["workspaceId"], expect_a);
    assert_eq!(b_json["status"]["workspaceId"], expect_b);
    assert_eq!(a_json["spaceId"], expect_a);
    assert_eq!(store.get_space(LOCAL_USER_ID, a_ws).unwrap().label, "main");

    let list = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/runs")
                .header("authorization", format!("Bearer {secret}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(list.status(), StatusCode::OK);
    let list_body = response_body(list).await;
    assert!(list_body.contains(&expect_a), "{list_body}");
    assert!(list_body.contains(&expect_b), "{list_body}");
}

#[tokio::test]
async fn follow_up_after_terminate_is_accepted() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let (store, secret) = bootstrap_store();
    seed_test_playbook(&store);
    let agent_urn = "urn:vam:agent:local.vam.dev/acme/test-agent";
    let idle_ws =
        vam_storage::apply_agent_workspace_id(LOCAL_USER_ID, agent_urn, "idle-ws").unwrap();
    store
        .create_session("sess-done-1", None, "vm-done-1", &idle_ws, "default")
        .unwrap();
    store
        .update_observed_state(
            "sess-done-1",
            vam_orchestrator::session::ObservedState::Terminated,
        )
        .unwrap();

    let app = test_app(app_state(store));
    put_test_agent(&app, &secret, agent_urn, "Test Agent").await;

    let run_body = sample_run_body(
        agent_urn,
        "idle-ws",
        "run-idle-idem",
        "urn:vam:run:local.vam.dev/acme/run-idle",
    );
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/runs")
                .header("authorization", format!("Bearer {}", secret))
                .header("Idempotency-Key", "run-idle-idem")
                .header("content-type", "application/json")
                .body(Body::from(run_body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::ACCEPTED,
        "{}",
        response_body(response).await
    );
}

#[tokio::test]
async fn run_events_accepts_unencoded_run_urn_with_slashes() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let (store, secret) = bootstrap_store();
    let run_urn = "urn:vam:run:local.vam.dev/acme/run-events-slash";
    store
        .create_run(&vam_spec::v1::ResourceId {
            urn: run_urn.to_string(),
        })
        .unwrap();
    let app = test_app(app_state_http_only(store));

    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/v1/runs/{}/events", run_urn))
                .header("authorization", format!("Bearer {}", secret))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "{}",
        response_body(response).await
    );
}

#[tokio::test]
async fn openapi_spec_is_accessible_unauthenticated() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let (store, _) = bootstrap_store();
    let app = test_app(app_state_http_only(store));

    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/openapi.yaml")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response_body(response).await;
    assert!(body.contains("openapi: 3.1.0"));
    let servers = body.split("servers:").nth(1).expect("servers block");
    let url = servers
        .lines()
        .find_map(|l| l.trim().strip_prefix("- url:"))
        .expect("servers url");
    assert!(url.trim().starts_with("https://"), "servers url: {url}");
    assert!(body.contains("/v1/agents"));
    assert!(body.contains("/v1/runs"));
    assert!(body.contains("/v1/sessions"));
}

#[tokio::test]
async fn docs_ui_is_accessible_unauthenticated() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let (store, _) = bootstrap_store();
    let app = test_app(app_state_http_only(store));

    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/docs")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response_body(response).await;
    assert!(body.contains("VAMD Control Plane API Documentation"));
    assert!(body.contains("@scalar/api-reference"));
    assert!(body.contains("/openapi.yaml"));
    let at = body
        .find("@scalar/api-reference@")
        .expect("versioned script URL");
    let rest = &body[at + "@scalar/api-reference@".len()..];
    let ver: String = rest
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    assert_eq!(ver.split('.').count(), 3, "need @x.y.z, got {ver}");
    assert!(ver
        .split('.')
        .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit())));
    assert!(body.contains(r#"integrity="sha384-"#));
    assert!(body.contains(r#"crossorigin="anonymous""#));
}

#[tokio::test]
async fn list_agents_returns_empty_then_both_after_puts() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let (store, secret) = bootstrap_store();
    let app = test_app(app_state_http_only(store));

    let empty_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/agents")
                .header("authorization", format!("Bearer {}", secret))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(empty_resp.status(), StatusCode::OK);
    let empty_body = response_body(empty_resp).await;
    assert!(
        !empty_body.contains("urn:vam:agent:"),
        "expected empty catalog, got: {empty_body}"
    );

    let urn_a = "urn:vam:agent:local.vam.dev/acme/agent-a";
    let urn_b = "urn:vam:agent:local.vam.dev/acme/agent-b";
    put_test_agent(&app, &secret, urn_a, "Agent A").await;
    put_test_agent(&app, &secret, urn_b, "Agent B").await;

    let list_resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/agents")
                .header("authorization", format!("Bearer {}", secret))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(list_resp.status(), StatusCode::OK);
    let list_body = response_body(list_resp).await;
    assert!(list_body.contains(urn_a));
    assert!(list_body.contains(urn_b));
}

#[tokio::test]
async fn put_invalid_cedar_policy_returns_400() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let (store, secret) = bootstrap_store();
    let app = test_app(app_state_http_only(store));

    let body = serde_json::json!({
        "urn": "urn:vam:policy:local.vam.dev/acme/bad-policy",
        "cedar": "this is not valid cedar {"
    });

    let response = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/v1/policies")
                .header("authorization", format!("Bearer {}", secret))
                .header("Idempotency-Key", "policy-bad-cedar")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn policy_put_get_list_roundtrip() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let (store, secret) = bootstrap_store();
    let app = test_app(app_state_http_only(store));
    let policy_urn = "urn:vam:policy:local.vam.dev/acme/test-policy";
    let cedar = r#"
permit (
    principal,
    action == Action::"ExecuteTool",
    resource == Tool::"read_file"
);
"#;

    let put_body = serde_json::json!({
        "urn": policy_urn,
        "cedar": cedar
    });
    let put_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/v1/policies")
                .header("authorization", format!("Bearer {}", secret))
                .header("Idempotency-Key", "policy-put-1")
                .header("content-type", "application/json")
                .body(Body::from(put_body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(put_resp.status(), StatusCode::OK);

    let get_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!(
                    "/v1/policies/{}",
                    policy_urn.replace('/', "%2F").replace(':', "%3A")
                ))
                .header("authorization", format!("Bearer {}", secret))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(get_resp.status(), StatusCode::OK);
    let get_body = response_body(get_resp).await;
    assert!(get_body.contains(policy_urn));
    assert!(get_body.contains("read_file"));

    let list_resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/policies")
                .header("authorization", format!("Bearer {}", secret))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(list_resp.status(), StatusCode::OK);
    let list_body = response_body(list_resp).await;
    assert!(list_body.contains(policy_urn));
}

#[test]
fn resolve_session_policy_prefers_catalog_over_workspace_heuristic() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    use vam_orchestrator::security::resolve_session_policy;
    use vam_spec::v1::agent_spec::InstructionSource;
    use vam_spec::v1::{AgentSpec, CapabilityPolicyRef, ResourceId, ResourceRef, Version};

    let store = SqliteWorkflowStore::in_memory().unwrap();
    store.initialize_catalog_schema().unwrap();

    let agent_urn = "urn:vam:agent:local.vam.dev/acme/not-bun-workspace";
    let policy_urn = "urn:vam:policy:local.vam.dev/acme/catalog-bound";
    let custom_marker = "catalog_only_tool_xyz";
    let custom_cedar = format!(
        r#"
permit (
    principal,
    action == Action::"ExecuteTool",
    resource == Tool::"{custom_marker}"
);
"#
    );

    let agent = AgentSpec {
        resource_id: Some(ResourceId {
            urn: agent_urn.to_string(),
        }),
        display_name: "Catalog-bound agent".to_string(),
        description: "test".to_string(),
        role: "test".to_string(),
        instruction_source: Some(InstructionSource::SystemInstructions(
            "test instructions".to_string(),
        )),
        model_requirements: None,
        preferred_models: vec![],
        context_policy: None,
        reasoning_policy: None,
        skill_refs: vec![],
        capability_policy: Some(CapabilityPolicyRef {
            policy_ref: Some(ResourceRef {
                resource_id: Some(ResourceId {
                    urn: policy_urn.to_string(),
                }),
                version: Some(Version {
                    major: 1,
                    minor: 0,
                    patch: 0,
                    prerelease: String::new(),
                    build: String::new(),
                }),
                digest: None,
                locators: vec![],
            }),
            security_profile_name: String::new(),
        }),
        hitl_policy: None,
        telemetry_policy: None,
        resource_requirements: None,
        completion_policy: None,
        default_playbook_ref: None,
        allowed_playbook_refs: vec![],
        allow_any_playbook: true,
        metadata: None,
        extends: None,
        overlay: None,
        ..Default::default()
    };
    // The policy is released first: releasing the agent freezes its policy ref.
    store.put_policy(policy_urn, &custom_cedar).unwrap();
    store.put_agent(&agent).unwrap();

    let session_id = "sess-policy-catalog";
    store
        .create_session(
            session_id,
            None,
            "vm-policy",
            "totally-unrelated-ws",
            "default",
        )
        .unwrap();
    store
        .persist_run_spec(
            "urn:vam:run:local.vam.dev/acme/run-policy-catalog",
            "{}",
            Some(session_id),
            agent_urn,
            None,
            "User::\"tester\"",
        )
        .unwrap();

    let resolved = resolve_session_policy(&store, session_id, Some(agent_urn)).unwrap();
    assert!(
        resolved.cedar.contains(custom_marker),
        "expected catalog cedar, got: {}",
        resolved.cedar
    );
    assert_eq!(
        resolved.kind,
        vam_orchestrator::PolicyResolutionKind::Catalog
    );
    assert!(
        !resolved.cedar.contains("calculator"),
        "workspace bun heuristic must not apply when agent binds catalog URN"
    );
}

fn parse_run_state(value: &serde_json::Value) -> i32 {
    match value {
        serde_json::Value::Number(n) => n.as_i64().unwrap_or(0) as i32,
        serde_json::Value::String(s) => {
            let upper = s.to_ascii_uppercase();
            if let Ok(n) = upper.parse::<i32>() {
                return n;
            }
            match upper.as_str() {
                "RUN_STATE_PENDING" | "PENDING" => 1,
                "RUN_STATE_RUNNING" | "RUNNING" => 2,
                "RUN_STATE_PAUSED" | "PAUSED" => 3,
                "RUN_STATE_SUCCEEDED" | "SUCCEEDED" => 4,
                "RUN_STATE_FAILED" | "FAILED" => 5,
                "RUN_STATE_CANCELLED" | "CANCELLED" => 6,
                "RUN_STATE_TIMED_OUT" | "TIMED_OUT" => 7,
                "RUN_STATE_EXPIRED" | "EXPIRED" => 8,
                "RUN_STATE_REJECTED" | "REJECTED" => 9,
                _ => 0,
            }
        }
        _ => 0,
    }
}

#[tokio::test]
async fn list_runs_includes_enriched_run_status_fields() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let (store, secret) = bootstrap_store();
    let agent_urn = "urn:vam:agent:local.vam.dev/acme/bun-assistant";
    let run_urn = "urn:vam:run:local.vam.dev/acme/run-enriched-1";
    let session_id = "sess-enriched-1";
    let spec_json = serde_json::json!({
        "runId": {"urn": run_urn},
        "agentRef": {
            "resourceId": {"urn": agent_urn},
            "version": {"major": 1, "minor": 0, "patch": 0},
            "digest": {"algorithm": "ALGORITHM_SHA256", "hexHash": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"}
        },
        "playbookRef": {
            "resourceId": {"urn": "urn:vam:playbook:local.vam.dev/acme/test-pb"},
            "version": {"major": 1, "minor": 0, "patch": 0},
            "digest": {"algorithm": "ALGORITHM_SHA256", "hexHash": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"}
        },
        "inputs": {"fields": {"prompt": {"stringValue": "hello from enriched run test"}}},
        "workspaceRefUri": "volume://assistant6",
        "threadRef": "main",
        "idempotencyKey": "run-enriched-idem"
    })
    .to_string();

    store
        .persist_run_spec(
            run_urn,
            &spec_json,
            Some(session_id),
            agent_urn,
            Some("run-enriched-idem"),
            "User::\"tester\"",
        )
        .unwrap();
    store
        .complete_run_with_result(
            run_urn,
            "SUCCEEDED",
            r#"{"answer":"The workspace is ready."}"#,
        )
        .unwrap();

    let app = test_app(app_state_http_only(store));

    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/runs")
                .header("authorization", format!("Bearer {}", secret))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response_body(response).await;
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    let statuses = json["statuses"]
        .as_array()
        .expect("statuses array should be present");
    let matched = statuses
        .iter()
        .find(|s| s["runId"]["urn"].as_str() == Some(run_urn))
        .expect("enriched run in list");

    assert_eq!(matched["agentUrn"].as_str(), Some(agent_urn), "{body}");
    assert_eq!(matched["sessionId"].as_str(), Some(session_id), "{body}");
    assert_eq!(matched["threadRef"].as_str(), Some("main"), "{body}");
    assert_eq!(
        matched["workspaceId"].as_str(),
        Some("assistant6"),
        "{body}"
    );
    assert_eq!(
        matched["prompt"].as_str(),
        Some("hello from enriched run test"),
        "{body}"
    );
    assert_eq!(
        matched["finalOutputs"]["answer"].as_str(),
        Some("The workspace is ready."),
        "{body}"
    );
    assert_eq!(parse_run_state(&matched["state"]), 4, "{body}");
}

mod skill_package_api {
    use super::*;
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::io::{copy, Cursor, Write};
    use std::sync::Mutex;
    use vam_orchestrator::skill_packages::{skill_blob_relpath, skill_catalog_root};

    static DRIVE9_ENV_LOCK: Mutex<()> = Mutex::new(());

    const TEST_URN: &str = "urn:vam:skill:vam.local/vam/assistant-tools";

    pub(super) struct Drive9TestRoot {
        _dir: tempfile::TempDir,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    pub(super) fn drive9_temp() -> Drive9TestRoot {
        let lock = DRIVE9_ENV_LOCK.lock().expect("drive9 env lock");
        let dir = tempfile::tempdir().expect("temp drive9 root");
        std::env::set_var("VAM_DRIVE9_ROOT", dir.path());
        Drive9TestRoot {
            _dir: dir,
            _lock: lock,
        }
    }

    pub(super) fn skills_md(urn: &str, body_suffix: &str) -> String {
        format!(
            "---\nid: \"{urn}\"\nname: Assistant Tools\nversion: \"1.0.0\"\ndescription: Test skill\n---\n\n# Body\n{body_suffix}\n"
        )
    }

    pub(super) fn manifest_json(urn: &str) -> String {
        format!(
            r#"{{
  "resourceId": {{"urn": "{urn}"}},
  "version": {{"major": 1, "minor": 0, "patch": 0}},
  "description": "Test skill",
  "tools": []
}}"#
        )
    }

    pub(super) fn write_tar_gz(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut tar_buf = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_buf);
            for (path, bytes) in entries {
                let mut header = tar::Header::new_gnu();
                header.set_size(bytes.len() as u64);
                header.set_mode(0o644);
                header.set_cksum();
                builder
                    .append_data(&mut header, *path, &mut Cursor::new(*bytes))
                    .expect("append tar entry");
            }
            builder.finish().expect("finish tar");
        }
        let mut gz = Vec::new();
        {
            let mut encoder = GzEncoder::new(&mut gz, Compression::default());
            copy(&mut Cursor::new(&tar_buf), &mut encoder).expect("gzip tar");
            encoder.finish().expect("finish gzip");
        }
        gz
    }

    fn multipart_body(boundary: &str, archive: &[u8]) -> Vec<u8> {
        let mut body = Vec::new();
        write!(
            body,
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"skill-package.tar.gz\"\r\nContent-Type: application/gzip\r\n\r\n"
        )
        .expect("write multipart header");
        body.extend_from_slice(archive);
        write!(body, "\r\n--{boundary}--\r\n").expect("write multipart footer");
        body
    }

    async fn publish_package(
        app: &axum::Router,
        secret: &str,
        archive: &[u8],
    ) -> axum::response::Response {
        publish_package_as(app, secret, archive, &uuid_like()).await
    }

    pub(super) async fn publish_package_as(
        app: &axum::Router,
        secret: &str,
        archive: &[u8],
        idem: &str,
    ) -> axum::response::Response {
        let boundary = "test-boundary";
        let body = multipart_body(boundary, archive);
        app.clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/skills/packages")
                    .header("authorization", format!("Bearer {}", secret))
                    .header("Idempotency-Key", idem)
                    .header(
                        "content-type",
                        format!("multipart/form-data; boundary={boundary}"),
                    )
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    fn uuid_like() -> String {
        format!("idem-{}", std::process::id())
    }

    fn package_digest(json: &serde_json::Value) -> String {
        json["package"]["digest"]
            .as_str()
            .expect("digest")
            .to_string()
    }

    fn package_revision(json: &serde_json::Value) -> i64 {
        json["package"]["metadata"]["revision"]
            .as_i64()
            .or_else(|| {
                json["package"]["metadata"]["revision"]
                    .as_str()
                    .and_then(|s| s.parse().ok())
            })
            .unwrap_or_else(|| {
                panic!(
                    "revision missing in response: {}",
                    serde_json::to_string_pretty(json).unwrap_or_default()
                )
            })
    }

    /// Packs a skill package directory the way `vam skill publish` does: every file, relative paths.
    fn pack_skill_dir(dir: &std::path::Path) -> Vec<u8> {
        fn collect(
            base: &std::path::Path,
            current: &std::path::Path,
            out: &mut Vec<(String, Vec<u8>)>,
        ) {
            for entry in std::fs::read_dir(current).expect("read skill dir") {
                let path = entry.expect("dir entry").path();
                if path.is_dir() {
                    collect(base, &path, out);
                } else {
                    let rel = path
                        .strip_prefix(base)
                        .expect("strip prefix")
                        .to_string_lossy()
                        .replace('\\', "/");
                    out.push((rel, std::fs::read(&path).expect("read skill file")));
                }
            }
        }
        let mut files = Vec::new();
        collect(dir, dir, &mut files);
        files.sort();
        let entries: Vec<(&str, &[u8])> = files
            .iter()
            .map(|(path, bytes)| (path.as_str(), bytes.as_slice()))
            .collect();
        write_tar_gz(&entries)
    }

    fn repo_root() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
    }

    #[tokio::test]
    async fn publish_repo_skill_packages_under_local_vam_dev_succeeds() {
        let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _state = isolate_state();
        let _drive9 = drive9_temp();
        let (store, secret) = bootstrap_store();
        let app = test_app(app_state_http_only(store.clone()));

        let mut dirs = Vec::new();
        for example in std::fs::read_dir(repo_root().join("examples")).expect("examples dir") {
            let skills = example.expect("example entry").path().join("skills");
            for skill in std::fs::read_dir(skills).into_iter().flatten().flatten() {
                if skill.path().is_dir() {
                    dirs.push(skill.path());
                }
            }
        }
        dirs.sort();
        assert!(!dirs.is_empty(), "no repo skill packages found");

        for (i, dir) in dirs.iter().enumerate() {
            let archive = pack_skill_dir(dir);
            let response =
                publish_package_as(&app, &secret, &archive, &format!("local-vam-dev-{i}")).await;
            let status = response.status();
            let body_text = response_body(response).await;
            assert_eq!(status, StatusCode::OK, "{}: {body_text}", dir.display());
            let body: serde_json::Value = serde_json::from_str(&body_text).expect("json");
            assert_eq!(body["package"]["channel"].as_str(), Some("DRAFT"));
        }
    }

    #[tokio::test]
    async fn publish_happy_path_writes_blob_and_draft_row() {
        let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _state = isolate_state();
        let _drive9 = drive9_temp();
        let (store, secret) = bootstrap_store();
        let app = test_app(app_state_http_only(store.clone()));

        let archive = write_tar_gz(&[
            ("SKILLS.md", skills_md(TEST_URN, "v1").as_bytes()),
            ("manifest.json", manifest_json(TEST_URN).as_bytes()),
            ("execute.js", b"export default 1;\n"),
        ]);
        let response = publish_package(&app, &secret, &archive).await;
        let status = response.status();
        let body_text = response_body(response).await;
        assert_eq!(status, StatusCode::OK, "{body_text}");

        let body: serde_json::Value = serde_json::from_str(&body_text).expect("json");
        assert_eq!(body["package"]["channel"].as_str(), Some("DRAFT"));
        assert!(!package_digest(&body).is_empty());

        let blob_path = skill_catalog_root(std::path::Path::new(
            std::env::var("VAM_DRIVE9_ROOT")
                .expect("VAM_DRIVE9_ROOT")
                .as_str(),
        ))
        .join(skill_blob_relpath(LOCAL_USER_ID, &package_digest(&body)));
        assert!(
            blob_path.is_file(),
            "blob missing at {}",
            blob_path.display()
        );
        assert!(
            blob_path
                .to_string_lossy()
                .contains(&format!("{LOCAL_USER_ID}/")),
            "blob must be owner-namespaced: {}",
            blob_path.display()
        );
    }

    #[tokio::test]
    async fn two_owners_write_same_digest_to_distinct_blob_paths() {
        let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _state = isolate_state();
        let _drive9 = drive9_temp();
        let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
        store.initialize_catalog_schema().unwrap();
        store.create_user("alice", "alice").unwrap();
        store.create_user("bob", "bob").unwrap();
        let scopes: Vec<String> = PUBLISHER_SCOPES.iter().map(|s| s.to_string()).collect();
        let alice_secret = mint_api_key_secret();
        let bob_secret = mint_api_key_secret();
        store
            .create_api_key_owned(
                "alice-pkg",
                "alice",
                &alice_secret,
                "User::\"alice\"",
                &scopes,
                "alice",
            )
            .unwrap();
        store
            .create_api_key_owned(
                "bob-pkg",
                "bob",
                &bob_secret,
                "User::\"bob\"",
                &scopes,
                "bob",
            )
            .unwrap();
        let app = test_app(app_state_http_only(store));

        let archive = write_tar_gz(&[
            ("SKILLS.md", skills_md(TEST_URN, "shared-digest").as_bytes()),
            ("manifest.json", manifest_json(TEST_URN).as_bytes()),
        ]);
        let alice = publish_package(&app, &alice_secret, &archive).await;
        assert_eq!(
            alice.status(),
            StatusCode::OK,
            "{}",
            response_body(alice).await
        );
        let alice_json: serde_json::Value =
            serde_json::from_str(&response_body(alice).await).unwrap();
        let digest = package_digest(&alice_json);

        let catalog = skill_catalog_root(std::path::Path::new(
            std::env::var("VAM_DRIVE9_ROOT")
                .expect("VAM_DRIVE9_ROOT")
                .as_str(),
        ));
        let alice_blob = catalog.join(skill_blob_relpath("alice", &digest));
        assert!(alice_blob.is_file(), "alice blob {}", alice_blob.display());
        let alice_bytes = std::fs::read(&alice_blob).expect("read alice blob");

        // The owner rule refuses bob a second owner for alice's URN. His publish
        // must be refused without touching alice's blob on disk.
        let bob = publish_package(&app, &bob_secret, &archive).await;
        assert_eq!(
            bob.status(),
            StatusCode::FORBIDDEN,
            "{}",
            response_body(bob).await
        );
        assert_eq!(
            std::fs::read(&alice_blob).expect("read alice blob after"),
            alice_bytes,
            "bob's refused publish must not overwrite alice's blob"
        );

        // Per-owner relpaths for one digest still differ on disk.
        let bob_blob = catalog.join(skill_blob_relpath("bob", &digest));
        assert_ne!(alice_blob, bob_blob);
        assert_eq!(alice_blob.file_name(), bob_blob.file_name());
    }

    #[tokio::test]
    async fn republish_same_bytes_is_idempotent() {
        let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _state = isolate_state();
        let _drive9 = drive9_temp();
        let (store, secret) = bootstrap_store();
        let app = test_app(app_state_http_only(store));

        let archive = write_tar_gz(&[
            ("SKILLS.md", skills_md(TEST_URN, "same").as_bytes()),
            ("manifest.json", manifest_json(TEST_URN).as_bytes()),
        ]);
        let first = publish_package(&app, &secret, &archive).await;
        assert_eq!(first.status(), StatusCode::OK);
        let first_json: serde_json::Value =
            serde_json::from_str(&response_body(first).await).unwrap();

        let second = publish_package(&app, &secret, &archive).await;
        assert_eq!(second.status(), StatusCode::OK);
        let second_json: serde_json::Value =
            serde_json::from_str(&response_body(second).await).unwrap();

        assert_eq!(package_digest(&first_json), package_digest(&second_json));
        assert_eq!(
            package_revision(&first_json),
            package_revision(&second_json)
        );
    }

    #[tokio::test]
    async fn changed_content_bumps_revision() {
        let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _state = isolate_state();
        let _drive9 = drive9_temp();
        let (store, secret) = bootstrap_store();
        let app = test_app(app_state_http_only(store));

        let first_archive = write_tar_gz(&[
            ("SKILLS.md", skills_md(TEST_URN, "alpha").as_bytes()),
            ("manifest.json", manifest_json(TEST_URN).as_bytes()),
        ]);
        let first = publish_package(&app, &secret, &first_archive).await;
        let first_json: serde_json::Value =
            serde_json::from_str(&response_body(first).await).unwrap();

        let second_archive = write_tar_gz(&[
            ("SKILLS.md", skills_md(TEST_URN, "beta").as_bytes()),
            ("manifest.json", manifest_json(TEST_URN).as_bytes()),
        ]);
        let second = publish_package(&app, &secret, &second_archive).await;
        let second_json: serde_json::Value =
            serde_json::from_str(&response_body(second).await).unwrap();

        assert_ne!(package_digest(&first_json), package_digest(&second_json));
        assert_eq!(
            package_revision(&second_json),
            package_revision(&first_json) + 1
        );
        assert_eq!(
            second_json["package"]["version"]["prerelease"].as_str(),
            Some("draft")
        );
    }

    #[tokio::test]
    async fn skill_package_versions_list_draft_then_release_with_note_and_releaser() {
        let _drive9 = drive9_temp();
        let (store, _bootstrap) = bootstrap_store();
        let ops = mint_user_key(&store, "ops", UserRole::OrgAdmin, PUBLISHER_SCOPES);
        let app = test_app(app_state_http_only(store));
        let encoded = TEST_URN.replace(':', "%3A").replace('/', "%2F");

        let archive = write_tar_gz(&[
            ("SKILLS.md", skills_md(TEST_URN, "versions-me").as_bytes()),
            ("manifest.json", manifest_json(TEST_URN).as_bytes()),
        ]);
        let publish = publish_package(&app, &ops, &archive).await;
        assert_eq!(publish.status(), StatusCode::OK);

        let release = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/v1/skills/packages/{encoded}/release"))
                    .header("authorization", format!("Bearer {ops}"))
                    .header("Idempotency-Key", uuid_like())
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "urn": TEST_URN,
                            "version": {"major": 1, "minor": 0, "patch": 0},
                            "note": "pkg first",
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            release.status(),
            StatusCode::OK,
            "{}",
            response_body(release).await
        );

        let versions = app
            .clone()
            .oneshot(bearer_get(
                &format!("/v1/skills/packages/{encoded}/versions"),
                &ops,
            ))
            .await
            .unwrap();
        assert_eq!(versions.status(), StatusCode::OK);
        let list: serde_json::Value = serde_json::from_str(&response_body(versions).await).unwrap();
        let entries = list["versions"].as_array().expect("versions array");
        assert_eq!(entries.len(), 2, "{list}");
        assert_eq!(entries[0]["version"]["prerelease"], "draft");
        assert_eq!(entries[1]["note"], "pkg first");
        assert_eq!(entries[1]["releasedBy"], "ops");
    }

    #[tokio::test]
    async fn release_then_get_released_and_publish_updates_draft_only() {
        let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _state = isolate_state();
        let _drive9 = drive9_temp();
        let (store, secret) = bootstrap_store();
        let app = test_app(app_state_http_only(store));

        let archive = write_tar_gz(&[
            ("SKILLS.md", skills_md(TEST_URN, "release-me").as_bytes()),
            ("manifest.json", manifest_json(TEST_URN).as_bytes()),
        ]);
        let publish = publish_package(&app, &secret, &archive).await;
        assert_eq!(publish.status(), StatusCode::OK);
        let published: serde_json::Value =
            serde_json::from_str(&response_body(publish).await).unwrap();
        let digest = package_digest(&published);

        let release_body = serde_json::json!({
            "urn": TEST_URN,
            "version": {"major": 1, "minor": 0, "patch": 0}
        });
        let release = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!(
                        "/v1/skills/packages/{}/release",
                        TEST_URN.replace(':', "%3A").replace('/', "%2F")
                    ))
                    .header("authorization", format!("Bearer {}", secret))
                    .header("Idempotency-Key", uuid_like())
                    .header("content-type", "application/json")
                    .body(Body::from(release_body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            release.status(),
            StatusCode::OK,
            "{}",
            response_body(release).await
        );

        let get_released = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(format!(
                        "/v1/skills/packages/{}?channel=released",
                        TEST_URN.replace(':', "%3A").replace('/', "%2F")
                    ))
                    .header("authorization", format!("Bearer {}", secret))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(get_released.status(), StatusCode::OK);
        let released_json: serde_json::Value =
            serde_json::from_str(&response_body(get_released).await).unwrap();
        assert_eq!(
            released_json["package"]["channel"].as_str(),
            Some("RELEASED")
        );
        assert_eq!(
            released_json["package"]["digest"].as_str(),
            Some(digest.as_str())
        );

        let changed = write_tar_gz(&[
            ("SKILLS.md", skills_md(TEST_URN, "after-release").as_bytes()),
            ("manifest.json", manifest_json(TEST_URN).as_bytes()),
        ]);
        let republish = publish_package(&app, &secret, &changed).await;
        assert_eq!(republish.status(), StatusCode::OK);
        let draft_json: serde_json::Value =
            serde_json::from_str(&response_body(republish).await).unwrap();
        assert_eq!(draft_json["package"]["channel"].as_str(), Some("DRAFT"));
        assert_ne!(package_digest(&draft_json), digest);
    }

    #[tokio::test]
    async fn get_version_returns_the_release_not_the_changed_draft() {
        let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _state = isolate_state();
        let _drive9 = drive9_temp();
        let (store, secret) = bootstrap_store();
        let app = test_app(app_state_http_only(store));
        let encoded = TEST_URN.replace(':', "%3A").replace('/', "%2F");

        let archive = write_tar_gz(&[
            ("SKILLS.md", skills_md(TEST_URN, "v1-release").as_bytes()),
            ("manifest.json", manifest_json(TEST_URN).as_bytes()),
        ]);
        let publish = publish_package(&app, &secret, &archive).await;
        assert_eq!(publish.status(), StatusCode::OK);
        let published: serde_json::Value =
            serde_json::from_str(&response_body(publish).await).unwrap();
        let released_digest = package_digest(&published);

        let release = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/v1/skills/packages/{encoded}/release"))
                    .header("authorization", format!("Bearer {secret}"))
                    .header("Idempotency-Key", uuid_like())
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "urn": TEST_URN,
                            "version": {"major": 1, "minor": 0, "patch": 0}
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(release.status(), StatusCode::OK);

        let changed = write_tar_gz(&[
            (
                "SKILLS.md",
                skills_md(TEST_URN, "draft-after-release").as_bytes(),
            ),
            ("manifest.json", manifest_json(TEST_URN).as_bytes()),
        ]);
        let republish = publish_package(&app, &secret, &changed).await;
        assert_eq!(republish.status(), StatusCode::OK);

        // `vam skill get --version 1.0.0` sends only the version, so the channel
        // defaults to draft. The answer must still be the release.
        let by_version = app
            .clone()
            .oneshot(bearer_get(
                &format!("/v1/skills/packages/{encoded}?version=1.0.0"),
                &secret,
            ))
            .await
            .unwrap();
        assert_eq!(by_version.status(), StatusCode::OK);
        let body: serde_json::Value =
            serde_json::from_str(&response_body(by_version).await).unwrap();
        assert_eq!(
            body["package"]["channel"].as_str(),
            Some("RELEASED"),
            "{body}"
        );
        assert_eq!(package_digest(&body), released_digest);

        // No release at 2.0.0: the error lists what is released.
        let missing = app
            .clone()
            .oneshot(bearer_get(
                &format!("/v1/skills/packages/{encoded}?version=2.0.0"),
                &secret,
            ))
            .await
            .unwrap();
        assert_ne!(missing.status(), StatusCode::OK);
        let message = response_body(missing).await;
        assert!(message.contains("released versions: 1.0.0"), "{message}");
    }

    #[tokio::test]
    async fn release_conflict_when_released_digest_differs() {
        let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _state = isolate_state();
        let _drive9 = drive9_temp();
        let (store, secret) = bootstrap_store();
        let app = test_app(app_state_http_only(store));

        let draft = write_tar_gz(&[
            ("SKILLS.md", skills_md(TEST_URN, "draft-a").as_bytes()),
            ("manifest.json", manifest_json(TEST_URN).as_bytes()),
        ]);
        assert_eq!(
            publish_package(&app, &secret, &draft).await.status(),
            StatusCode::OK
        );

        let release_body = serde_json::json!({
            "urn": TEST_URN,
            "version": {"major": 1, "minor": 0, "patch": 0}
        });
        let first_release = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!(
                        "/v1/skills/packages/{}/release",
                        TEST_URN.replace(':', "%3A").replace('/', "%2F")
                    ))
                    .header("authorization", format!("Bearer {}", secret))
                    .header("Idempotency-Key", uuid_like())
                    .header("content-type", "application/json")
                    .body(Body::from(release_body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(first_release.status(), StatusCode::OK);

        let changed = write_tar_gz(&[
            ("SKILLS.md", skills_md(TEST_URN, "draft-b").as_bytes()),
            ("manifest.json", manifest_json(TEST_URN).as_bytes()),
        ]);
        assert_eq!(
            publish_package(&app, &secret, &changed).await.status(),
            StatusCode::OK
        );

        let second_release = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!(
                        "/v1/skills/packages/{}/release",
                        TEST_URN.replace(':', "%3A").replace('/', "%2F")
                    ))
                    .header("authorization", format!("Bearer {}", secret))
                    .header("Idempotency-Key", uuid_like())
                    .header("content-type", "application/json")
                    .body(Body::from(release_body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(second_release.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn missing_manifest_is_rejected() {
        let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _state = isolate_state();
        let _drive9 = drive9_temp();
        let (store, secret) = bootstrap_store();
        let app = test_app(app_state_http_only(store));

        let archive = write_tar_gz(&[("SKILLS.md", skills_md(TEST_URN, "no manifest").as_bytes())]);
        let response = publish_package(&app, &secret, &archive).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn frontmatter_urn_mismatch_is_rejected() {
        let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _state = isolate_state();
        let _drive9 = drive9_temp();
        let (store, secret) = bootstrap_store();
        let app = test_app(app_state_http_only(store));

        let archive = write_tar_gz(&[
            (
                "SKILLS.md",
                skills_md("urn:vam:skill:hub.vam.dev/acme/other", "x").as_bytes(),
            ),
            ("manifest.json", manifest_json(TEST_URN).as_bytes()),
        ]);
        let response = publish_package(&app, &secret, &archive).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn non_local_urn_is_rejected() {
        let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _state = isolate_state();
        let _drive9 = drive9_temp();
        let (store, secret) = bootstrap_store();
        let app = test_app(app_state_http_only(store));
        let hub_urn = "urn:vam:skill:hub.vam.dev/acme/assistant-tools";

        let archive = write_tar_gz(&[
            ("SKILLS.md", skills_md(hub_urn, "x").as_bytes()),
            ("manifest.json", manifest_json(hub_urn).as_bytes()),
        ]);
        let response = publish_package(&app, &secret, &archive).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    fn encode_package_urn(urn: &str) -> String {
        urn.replace(':', "%3A").replace('/', "%2F")
    }

    #[tokio::test]
    async fn catalog_visibility_admin_lists_foreign_skill_package_draft() {
        let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _state = isolate_state();
        let _drive9 = drive9_temp();
        let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
        store.initialize_catalog_schema().unwrap();
        let alice = mint_user_key(&store, "alice", UserRole::Publisher, PUBLISHER_SCOPES);
        let bob = mint_user_key(
            &store,
            "bob",
            UserRole::Member,
            &["catalog:write", "sessions:write"],
        );
        let admin = mint_user_key(&store, "admin", UserRole::OrgAdmin, ORG_ADMIN_SCOPES);
        let app = test_app(app_state_http_only(store.clone()));
        let urn = "urn:vam:skill:vam.local/vam/alice-admin-pkg";
        let archive = write_tar_gz(&[
            ("SKILLS.md", skills_md(urn, "alice-draft").as_bytes()),
            ("manifest.json", manifest_json(urn).as_bytes()),
            ("execute.js", b"export default 1;\n"),
        ]);
        let published = publish_package_as(&app, &alice, &archive, "alice-pkg-draft").await;
        let published_status = published.status();
        let published_body = response_body(published).await;
        assert_eq!(published_status, StatusCode::OK, "{published_body}");

        let bob_list = app
            .clone()
            .oneshot(bearer_get("/v1/skills/packages", &bob))
            .await
            .unwrap();
        assert_eq!(bob_list.status(), StatusCode::OK);
        let bob_list_body = response_body(bob_list).await;
        assert!(
            !bob_list_body.contains(urn),
            "member must not see alice skill draft: {bob_list_body}"
        );
        let bob_get = app
            .clone()
            .oneshot(bearer_get(
                &format!(
                    "/v1/skills/packages/{}?channel=draft",
                    encode_package_urn(urn)
                ),
                &bob,
            ))
            .await
            .unwrap();
        assert_eq!(bob_get.status(), StatusCode::NOT_FOUND);

        let admin_list = app
            .clone()
            .oneshot(bearer_get("/v1/skills/packages", &admin))
            .await
            .unwrap();
        assert_eq!(admin_list.status(), StatusCode::OK);
        let admin_list_body = response_body(admin_list).await;
        assert!(
            admin_list_body.contains(urn),
            "admin must list alice skill draft: {admin_list_body}"
        );
        let admin_json: serde_json::Value =
            serde_json::from_str(&admin_list_body).expect("admin list json");
        let packages = admin_json["packages"].as_array().expect("packages");
        let row = packages
            .iter()
            .find(|p| p["resourceId"]["urn"].as_str() == Some(urn))
            .expect("alice package row");
        assert_eq!(row["ownerUserId"].as_str(), Some("alice"));
        let channel = row["channel"].as_str().unwrap_or("");
        assert!(
            channel.eq_ignore_ascii_case("DRAFT") || channel.eq_ignore_ascii_case("draft"),
            "{row}"
        );
        assert_eq!(row["catalogTier"].as_str(), Some("user"));

        let admin_get = app
            .clone()
            .oneshot(bearer_get(
                &format!(
                    "/v1/skills/packages/{}?channel=draft",
                    encode_package_urn(urn)
                ),
                &admin,
            ))
            .await
            .unwrap();
        assert_eq!(admin_get.status(), StatusCode::OK);
        let admin_get_body = response_body(admin_get).await;
        let admin_get_json: serde_json::Value =
            serde_json::from_str(&admin_get_body).expect("admin get json");
        let package = &admin_get_json["package"];
        assert_eq!(package["ownerUserId"].as_str(), Some("alice"));
        assert_eq!(package["catalogTier"].as_str(), Some("user"));

        let deactivate = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/users/alice/deactivate")
                    .header("authorization", format!("Bearer {admin}"))
                    .header("Idempotency-Key", "deactivate-alice-skill-pkg")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            deactivate.status(),
            StatusCode::OK,
            "{}",
            response_body(deactivate).await
        );

        let admin_list_after = app
            .clone()
            .oneshot(bearer_get("/v1/skills/packages", &admin))
            .await
            .unwrap();
        let after_body = response_body(admin_list_after).await;
        assert!(
            after_body.contains(urn),
            "admin still lists deactivated owner's skill draft: {after_body}"
        );

        let reassign = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/catalog/reassign-owner")
                    .header("authorization", format!("Bearer {admin}"))
                    .header("Idempotency-Key", "reassign-alice-skill-pkg")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "kind": "skill_package",
                            "urn": urn,
                            "ownerUserId": "bob"
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            reassign.status(),
            StatusCode::OK,
            "{}",
            response_body(reassign).await
        );
        assert_eq!(
            store
                .skill_package_urn_owner_user_id(urn)
                .expect("reassigned owner"),
            "bob"
        );
    }
}

mod hosted_brokers {
    use super::*;
    use tempfile::tempdir;
    use vam_orchestrator::hsp_echo::spawn_echo_unix;

    fn echo_manifest(broker_id: &str, trust_tier: i32) -> serde_json::Value {
        serde_json::json!({
            "brokerId": broker_id,
            "trustTier": trust_tier,
            "tools": [{
                "toolName": "echo.ping",
                "sideEffect": 0,
                "requiredSecrets": []
            }]
        })
    }

    #[tokio::test]
    async fn put_and_get_hosted_broker_registers_echo_ping() {
        let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _state = isolate_state();
        let dir = tempdir().unwrap();
        let sock = dir.path().join("echo-api.sock");
        let (sock, _handle) = spawn_echo_unix(sock).unwrap();
        // Give the server a moment to accept connections.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let (store, secret) = bootstrap_store();
        let app = test_app(app_state_http_only(store));

        let body = serde_json::json!({
            "manifest": echo_manifest("echo", 2),
            "unixPath": sock.to_string_lossy(),
            "idempotencyKey": "hsp-put-1"
        });

        let put_resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/v1/hosted/brokers")
                    .header("authorization", format!("Bearer {}", secret))
                    .header("Idempotency-Key", "hsp-put-1")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            put_resp.status(),
            StatusCode::OK,
            "{}",
            response_body(put_resp).await
        );

        let list_resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/v1/hosted/brokers")
                    .header("authorization", format!("Bearer {}", secret))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(list_resp.status(), StatusCode::OK);
        let list_body = response_body(list_resp).await;
        assert!(
            list_body.contains("echo.ping"),
            "expected echo.ping in catalog: {list_body}"
        );

        let get_resp = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/v1/hosted/brokers/echo")
                    .header("authorization", format!("Bearer {}", secret))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(get_resp.status(), StatusCode::OK);
        let get_body = response_body(get_resp).await;
        assert!(get_body.contains("echo.ping"), "{get_body}");
        assert!(
            get_body.contains(&sock.to_string_lossy().to_string()),
            "{get_body}"
        );
    }

    #[tokio::test]
    async fn hosted_broker_requires_operator_scope() {
        let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _state = isolate_state();
        let (store, _bootstrap) = bootstrap_store();
        let catalog_secret = mint_api_key_secret();
        store
            .create_api_key(
                "catalog-writer",
                "catalog-writer",
                &catalog_secret,
                "User::\"publisher\"",
                &["catalog:write".to_string()],
            )
            .unwrap();
        let app = test_app(app_state_http_only(store));
        let body = serde_json::json!({
            "manifest": echo_manifest("echo-escalation", 2),
            "unixPath": "/tmp/should-not-dial.sock",
            "idempotencyKey": "hsp-no-operator"
        });
        let resp = app
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/v1/hosted/brokers")
                    .header("authorization", format!("Bearer {}", catalog_secret))
                    .header("Idempotency-Key", "hsp-no-operator")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        let resp_body = response_body(resp).await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "catalog:write must receive 403 (not 404): {resp_body}"
        );
        assert_ne!(status, StatusCode::NOT_FOUND);
        assert!(
            resp_body.contains("PERMISSION_DENIED") || resp_body.contains("brokers:admin"),
            "expected missing-scope denial, got {resp_body}"
        );
    }

    #[tokio::test]
    async fn hosted_broker_operator_scope_can_admit() {
        let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _state = isolate_state();
        let dir = tempdir().unwrap();
        let sock = dir.path().join("echo-operator.sock");
        let (sock, _handle) = spawn_echo_unix(sock).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
        store.initialize_catalog_schema().unwrap();
        let secret = mint_api_key_secret();
        store
            .create_api_key(
                "broker-admin",
                "broker-admin",
                &secret,
                "User::\"operator\"",
                &[vam_orchestrator::keys::BROKERS_ADMIN_SCOPE.to_string()],
            )
            .unwrap();
        let app = test_app(app_state_http_only(store));
        let body = serde_json::json!({
            "manifest": echo_manifest("echo-operator", 2),
            "unixPath": sock.to_string_lossy(),
            "idempotencyKey": "hsp-operator-admit"
        });
        let resp = app
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/v1/hosted/brokers")
                    .header("authorization", format!("Bearer {}", secret))
                    .header("Idempotency-Key", "hsp-operator-admit")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "{}",
            response_body(resp).await
        );
    }

    #[tokio::test]
    async fn put_hosted_broker_requires_idempotency_key() {
        let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _state = isolate_state();
        let (store, secret) = bootstrap_store();
        let app = test_app(app_state_http_only(store));
        let body = serde_json::json!({
            "manifest": echo_manifest("echo", 2),
            "unixPath": "/tmp/missing.sock"
        });
        let resp = app
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/v1/hosted/brokers")
                    .header("authorization", format!("Bearer {}", secret))
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn put_hosted_broker_rejects_core_tier() {
        let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _state = isolate_state();
        let dir = tempdir().unwrap();
        let sock = dir.path().join("echo-core.sock");
        let (sock, _handle) = spawn_echo_unix(sock).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let (store, secret) = bootstrap_store();
        let app = test_app(app_state_http_only(store));
        let body = serde_json::json!({
            "manifest": echo_manifest("echo-core", 1),
            "unixPath": sock.to_string_lossy(),
            "idempotencyKey": "hsp-core"
        });
        let resp = app
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/v1/hosted/brokers")
                    .header("authorization", format!("Bearer {}", secret))
                    .header("Idempotency-Key", "hsp-core")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::FORBIDDEN,
            "{}",
            response_body(resp).await
        );
    }

    #[tokio::test]
    async fn put_hosted_broker_rejects_third_party_tier() {
        let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _state = isolate_state();
        let dir = tempdir().unwrap();
        let sock = dir.path().join("echo-3p.sock");
        let (sock, _handle) = spawn_echo_unix(sock).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let (store, secret) = bootstrap_store();
        let app = test_app(app_state_http_only(store));
        let body = serde_json::json!({
            "manifest": echo_manifest("echo-3p", 4),
            "unixPath": sock.to_string_lossy(),
            "idempotencyKey": "hsp-3p"
        });
        let resp = app
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/v1/hosted/brokers")
                    .header("authorization", format!("Bearer {}", secret))
                    .header("Idempotency-Key", "hsp-3p")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::FORBIDDEN,
            "{}",
            response_body(resp).await
        );
    }

    #[tokio::test]
    async fn put_hosted_broker_fails_closed_when_socket_missing() {
        let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _state = isolate_state();
        let (store, secret) = bootstrap_store();
        let app = test_app(app_state_http_only(store));
        let missing = std::env::temp_dir().join(format!(
            "hsp-missing-{}.sock",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let body = serde_json::json!({
            "manifest": echo_manifest("echo-missing", 2),
            "unixPath": missing.to_string_lossy(),
            "idempotencyKey": "hsp-missing"
        });
        let resp = app
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/v1/hosted/brokers")
                    .header("authorization", format!("Bearer {}", secret))
                    .header("Idempotency-Key", "hsp-missing")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::FORBIDDEN,
            "{}",
            response_body(resp).await
        );
    }
}

#[tokio::test]
async fn secret_resolve_reports_caller_vault_without_leaking_other_user() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    use vam_orchestrator::secrets::{SecretKind, SecretScope};
    use vam_orchestrator::{SecretScope as VaultScope, ORG_OWNER_USER_ID};

    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    // Two members: alice and bob. Bootstrap key is operator (org default).
    // Suffix minted secrets to avoid nanos-collision in rapid test minting.
    let bootstrap_secret = format!("{}-bootstrap", mint_api_key_secret());
    let alice_secret = format!("{}-alice", mint_api_key_secret());
    let bob_secret = format!("{}-bob", mint_api_key_secret());
    let member_scopes: Vec<String> = MEMBER_SCOPES.iter().map(|s| s.to_string()).collect();
    let bootstrap_scopes: Vec<String> = BOOTSTRAP_SCOPES.iter().map(|s| s.to_string()).collect();
    store
        .create_api_key(
            BOOTSTRAP_KEY_ID,
            "bootstrap",
            &bootstrap_secret,
            BOOTSTRAP_PRINCIPAL,
            &bootstrap_scopes,
        )
        .unwrap();
    store.create_user("alice", "alice").unwrap();
    store.create_user("bob", "bob").unwrap();
    store
        .create_api_key_owned(
            "alice-key",
            "alice",
            &alice_secret,
            "User::\"alice\"",
            &member_scopes,
            "alice",
        )
        .unwrap();
    store
        .create_api_key_owned(
            "bob-key",
            "bob",
            &bob_secret,
            "User::\"bob\"",
            &member_scopes,
            "bob",
        )
        .unwrap();
    let app = test_app(app_state_http_only(store.clone()));
    let vault = SecretsVault::for_tests(store.clone());

    // Org shared secret + alice user-scope secret. Bob has neither user row.
    vault
        .put_secret_at(
            "shared/api-key",
            "org-value",
            SecretKind::Egress,
            &[],
            None,
            SecretScope::Org,
            ORG_OWNER_USER_ID,
        )
        .unwrap();
    vault
        .put_secret_at(
            "jira/auth-header",
            "alice-value",
            SecretKind::Egress,
            &[],
            None,
            SecretScope::User,
            "alice",
        )
        .unwrap();

    let resolve = |secret: &str, body: serde_json::Value| {
        Request::builder()
            .method("POST")
            .uri("/v1/secrets/resolve")
            .header("authorization", format!("Bearer {secret}"))
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    };

    // Alice: user-scope configured, org configured via either.
    let alice_body = serde_json::json!({
        "requirements": [
            {"name": "jira/auth-header", "required": true, "scope": 2},
            {"name": "shared/api-key", "required": true, "scope": 3}
        ]
    });
    let alice_resp = app
        .clone()
        .oneshot(resolve(&alice_secret, alice_body))
        .await
        .unwrap();
    assert_eq!(alice_resp.status(), StatusCode::OK);
    let alice_json = response_body(alice_resp).await;
    assert!(alice_json.contains("jira/auth-header"), "{alice_json}");
    // No owner or fingerprint leak in discovery payload.
    assert!(!alice_json.contains("alice-value"), "{alice_json}");
    assert!(!alice_json.contains("ownerUserId"), "{alice_json}");
    assert!(!alice_json.contains("fingerprint"), "{alice_json}");

    // Bob: same user-scope binding is missing (no leak of alice's row).
    let bob_body = serde_json::json!({
        "requirements": [
            {"name": "jira/auth-header", "required": true, "scope": 2}
        ]
    });
    let bob_resp = app
        .clone()
        .oneshot(resolve(&bob_secret, bob_body))
        .await
        .unwrap();
    assert_eq!(bob_resp.status(), StatusCode::OK);
    let bob_json = response_body(bob_resp).await;
    assert!(bob_json.contains("jira/auth-header"), "{bob_json}");
    assert!(bob_json.contains("missing"), "{bob_json}");
    assert!(!bob_json.contains("alice"), "{bob_json}");
    // Vault type re-export sanity (keeps import used).
    let _ = VaultScope::Org;
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
            "description": "isolation publish test",
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
    resp.status()
}

async fn send_catalog_json(
    app: &axum::Router,
    method: &str,
    uri: &str,
    secret: &str,
    idem: &str,
    body: serde_json::Value,
) -> StatusCode {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
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

async fn release_catalog_version(
    app: &axum::Router,
    secret: &str,
    collection: &str,
    urn: &str,
    (major, minor, patch): (i32, i32, i32),
) -> StatusCode {
    send_catalog_json(
        app,
        "POST",
        &format!("/v1/{collection}/{urn}/release"),
        secret,
        &format!("release-{urn}-{major}.{minor}.{patch}"),
        serde_json::json!({"version": {"major": major, "minor": minor, "patch": patch}}),
    )
    .await
}

async fn get_catalog_list(app: &axum::Router, secret: &str, uri: &str) -> serde_json::Value {
    let resp = app.clone().oneshot(bearer_get(uri, secret)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    serde_json::from_str(&response_body(resp).await).unwrap()
}

/// Labels for the records of one URN, as `major.minor.patch` plus `-prerelease`.
/// Proto-JSON omits zero fields, so a missing component reads as 0.
fn version_labels_for(list: &serde_json::Value, urn: &str) -> Vec<String> {
    let mut labels: Vec<String> = list["records"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|record| {
            // Agent records nest the id under `spec`; policy records carry it at the top.
            let record_urn = record["spec"]["resourceId"]["urn"]
                .as_str()
                .or_else(|| record["resourceId"]["urn"].as_str());
            record_urn == Some(urn)
        })
        .map(|record| {
            let version = &record["version"];
            let triple = format!(
                "{}.{}.{}",
                version["major"].as_i64().unwrap_or(0),
                version["minor"].as_i64().unwrap_or(0),
                version["patch"].as_i64().unwrap_or(0),
            );
            match version["prerelease"].as_str() {
                Some(pre) if !pre.is_empty() => format!("{triple}-{pre}"),
                _ => triple,
            }
        })
        .collect();
    labels.sort();
    labels
}

#[tokio::test]
async fn agent_list_records_carry_version_for_owner_and_member() {
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let alice = mint_user_key(&store, "alice", UserRole::Publisher, PUBLISHER_SCOPES);
    let bob = member_secret_for(&store, "bob", "bob");
    let app = test_app(app_state_http_only(store));
    let urn = "urn:vam:agent:local.vam.dev/acme/versioned-agent";
    assert_eq!(
        put_agent_as(&app, &alice, urn, "versioned-draft").await,
        StatusCode::OK
    );
    assert_eq!(
        release_catalog_version(&app, &alice, "agents", urn, (0, 1, 0)).await,
        StatusCode::OK
    );
    assert_eq!(
        put_agent_as(&app, &alice, urn, "versioned-draft-v2").await,
        StatusCode::OK
    );
    assert_eq!(
        release_catalog_version(&app, &alice, "agents", urn, (1, 0, 0)).await,
        StatusCode::OK
    );

    let owner_list = get_catalog_list(&app, &alice, "/v1/agents").await;
    assert_eq!(
        version_labels_for(&owner_list, urn),
        ["0.1.0", "1.0.0", "1.0.0-draft"]
    );
    let draft = owner_list["records"]
        .as_array()
        .unwrap()
        .iter()
        .find(|record| record["version"]["prerelease"] == "draft")
        .expect("owner sees the draft record");
    assert_eq!(draft["channel"], "DRAFT", "{draft}");

    let member_list = get_catalog_list(&app, &bob, "/v1/agents").await;
    assert_eq!(version_labels_for(&member_list, urn), ["0.1.0", "1.0.0"]);
}

#[tokio::test]
async fn policy_list_records_carry_version_for_owner_and_member() {
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let alice = mint_user_key(&store, "alice", UserRole::Publisher, PUBLISHER_SCOPES);
    let bob = member_secret_for(&store, "bob", "bob");
    let app = test_app(app_state_http_only(store));
    let urn = "urn:vam:policy:local.vam.dev/acme/versioned-policy";
    let cedar = r#"permit(principal == User::"alice", action, resource);"#;
    assert_eq!(
        send_catalog_json(
            &app,
            "PUT",
            "/v1/policies",
            &alice,
            "policy-draft-versioned",
            serde_json::json!({
                "urn": urn,
                "cedar": cedar,
                "idempotencyKey": "policy-draft-versioned"
            }),
        )
        .await,
        StatusCode::OK
    );
    assert_eq!(
        release_catalog_version(&app, &alice, "policies", urn, (0, 1, 0)).await,
        StatusCode::OK
    );
    let cedar_v2 = r#"permit(principal == User::"alice-v2", action, resource);"#;
    assert_eq!(
        send_catalog_json(
            &app,
            "PUT",
            "/v1/policies",
            &alice,
            "policy-draft-versioned-v2",
            serde_json::json!({
                "urn": urn,
                "cedar": cedar_v2,
                "idempotencyKey": "policy-draft-versioned-v2"
            }),
        )
        .await,
        StatusCode::OK
    );
    assert_eq!(
        release_catalog_version(&app, &alice, "policies", urn, (1, 0, 0)).await,
        StatusCode::OK
    );

    let owner_list = get_catalog_list(&app, &alice, "/v1/policies").await;
    assert_eq!(
        version_labels_for(&owner_list, urn),
        ["0.1.0", "1.0.0", "1.0.0-draft"]
    );

    let member_list = get_catalog_list(&app, &bob, "/v1/policies").await;
    assert_eq!(version_labels_for(&member_list, urn), ["0.1.0", "1.0.0"]);
}

#[tokio::test]
async fn non_owner_cannot_release_catalog_agent() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let alice = mint_user_key(
        &store,
        "alice",
        UserRole::Member,
        &["catalog:write", "sessions:write"],
    );
    let bob = mint_user_key(&store, "bob", UserRole::Publisher, PUBLISHER_SCOPES);
    let app = test_app(app_state_http_only(store));
    let urn = "urn:vam:agent:local.vam.dev/acme/alice-owned";
    assert_eq!(
        put_agent_as(&app, &alice, urn, "alice-draft").await,
        StatusCode::OK
    );
    assert_eq!(
        put_agent_as(&app, &bob, urn, "bob-steal").await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        release_agent_as(&app, &bob, urn, "bob-release").await,
        StatusCode::FORBIDDEN
    );
}

async fn get_release_suggestion(
    app: &axum::Router,
    secret: &str,
    urn: &str,
) -> (StatusCode, serde_json::Value) {
    let resp = app
        .clone()
        .oneshot(bearer_get(
            &format!("/v1/agents/{urn}/release-suggestion"),
            secret,
        ))
        .await
        .unwrap();
    let status = resp.status();
    let body = response_body(resp).await;
    (
        status,
        serde_json::from_str(&body).unwrap_or(serde_json::Value::Null),
    )
}

#[tokio::test]
async fn release_suggestion_is_owner_only_and_has_the_documented_shape() {
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let alice = mint_user_key(&store, "alice", UserRole::Publisher, PUBLISHER_SCOPES);
    let bob = mint_user_key(&store, "bob", UserRole::Publisher, PUBLISHER_SCOPES);
    let member = mint_user_key(
        &store,
        "carol",
        UserRole::Member,
        &["runs:write", "sessions:write"],
    );
    let app = test_app(app_state_http_only(store));
    let urn = "urn:vam:agent:local.vam.dev/acme/suggest-me";
    assert_eq!(put_agent_as(&app, &alice, urn, "v1").await, StatusCode::OK);

    // No release yet: INITIAL 1.0.0, no `latest`.
    let (status, json) = get_release_suggestion(&app, &alice, urn).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["bump"], "INITIAL", "{json}");
    assert_eq!(json["suggested"]["major"], 1, "{json}");
    assert!(
        json.get("latest").is_none() || json["latest"].is_null(),
        "{json}"
    );

    assert_eq!(
        release_agent_as(&app, &alice, urn, "alice-release").await,
        StatusCode::OK
    );

    // Draft equals the release: NONE, suggested is latest with patch + 1.
    let (status, json) = get_release_suggestion(&app, &alice, urn).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["bump"], "NONE", "{json}");
    assert_eq!(json["unchanged"], true, "{json}");
    assert_eq!(json["latest"]["major"], 1, "{json}");
    assert_eq!(json["suggested"]["patch"], 1, "{json}");

    // Instructions only: PATCH, with the reason.
    assert_eq!(put_agent_as(&app, &alice, urn, "v2").await, StatusCode::OK);
    let (status, json) = get_release_suggestion(&app, &alice, urn).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["bump"], "PATCH", "{json}");
    // Proto JSON omits default values, so `unchanged: false` is absent.
    assert!(!json["unchanged"].as_bool().unwrap_or(false), "{json}");
    assert_eq!(json["reasons"][0], "edited instructions", "{json}");
    assert_eq!(json["suggested"]["patch"], 1, "{json}");

    // A non-owner with the publish grant, and a plain member, get 403.
    assert_eq!(
        get_release_suggestion(&app, &bob, urn).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        get_release_suggestion(&app, &member, urn).await.0,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn release_suggestion_includes_permission_changes() {
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let alice = mint_user_key(&store, "alice", UserRole::Publisher, PUBLISHER_SCOPES);
    let app = test_app(app_state_http_only(store));
    let urn = "urn:vam:agent:local.vam.dev/acme/permission-diff";

    let put = |instructions: &str, decision: &str| {
        serde_json::json!({
            "spec": {
                "resourceId": {"urn": urn},
                "displayName": "Owned agent",
                "description": "permission diff",
                "role": "Tester",
                "systemInstructions": instructions,
                "allowAnyPlaybook": true,
                "permissions": {
                    "tools": {
                        "default": "ask",
                        "rules": [{
                            "target": "mcp:mcp.atlassian.com/create_issue",
                            "decision": decision
                        }]
                    }
                }
            }
        })
    };

    let status = send_catalog_json(
        &app,
        "PUT",
        "/v1/agents",
        &alice,
        "put-permission-ask",
        put("v1", "ask"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        release_agent_as(&app, &alice, urn, "release-permission-ask").await,
        StatusCode::OK
    );

    let status = send_catalog_json(
        &app,
        "PUT",
        "/v1/agents",
        &alice,
        "put-permission-allow",
        put("v1", "allow"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, json) = get_release_suggestion(&app, &alice, urn).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["bump"], "MINOR", "{json}");
    assert_eq!(json["suggested"]["minor"], 1, "{json}");
    let changes = json["permissionChanges"]
        .as_array()
        .unwrap_or_else(|| panic!("permissionChanges missing: {json}"));
    assert_eq!(changes.len(), 1, "{json}");
    assert_eq!(
        changes[0]["target"], "mcp:mcp.atlassian.com/create_issue",
        "{json}"
    );
    assert_eq!(changes[0]["before"], "ask", "{json}");
    assert_eq!(changes[0]["after"], "allow", "{json}");
    assert_eq!(changes[0]["change"], "loosened", "{json}");
    assert_eq!(
        changes[0]["sentence"],
        "Now allowed without asking: `mcp:mcp.atlassian.com/create_issue` (was: ask)",
        "{json}"
    );
}

async fn release_agent_version_as(
    app: &axum::Router,
    secret: &str,
    urn: &str,
    idem: &str,
    (major, minor, patch): (i32, i32, i32),
) -> (StatusCode, String) {
    let body = serde_json::json!({
        "version": {"major": major, "minor": minor, "patch": patch}
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
    (status, response_body(resp).await)
}

#[tokio::test]
async fn release_over_http_rejects_lower_version_and_conflicts_on_changed_content() {
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let alice = mint_user_key(&store, "alice", UserRole::Publisher, PUBLISHER_SCOPES);
    let app = test_app(app_state_http_only(store));
    let urn = "urn:vam:agent:local.vam.dev/acme/http-release";

    assert_eq!(put_agent_as(&app, &alice, urn, "v1").await, StatusCode::OK);
    let (status, body) = release_agent_version_as(&app, &alice, urn, "http-r-010", (0, 1, 0)).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    assert_eq!(put_agent_as(&app, &alice, urn, "v2").await, StatusCode::OK);
    let (status, body) = release_agent_version_as(&app, &alice, urn, "http-r-100", (1, 0, 0)).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, body) = release_agent_version_as(&app, &alice, urn, "http-r-009", (0, 0, 9)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body.contains("must be greater than the latest release 1.0.0"),
        "{body}"
    );

    assert_eq!(put_agent_as(&app, &alice, urn, "v3").await, StatusCode::OK);
    let (status, body) =
        release_agent_version_as(&app, &alice, urn, "http-r-100-again", (1, 0, 0)).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body.contains("the next free version is 1.0.1"), "{body}");
}

async fn release_with_note_as(
    app: &axum::Router,
    secret: &str,
    collection: &str,
    urn: &str,
    (major, minor, patch): (i32, i32, i32),
    idem: &str,
    note: &str,
) -> (StatusCode, String) {
    let body = serde_json::json!({
        "version": {"major": major, "minor": minor, "patch": patch},
        "note": note,
    });
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/v1/{collection}/{urn}/release"))
                .header("authorization", format!("Bearer {secret}"))
                .header("Idempotency-Key", idem)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    (status, response_body(resp).await)
}

async fn versions_as(
    app: &axum::Router,
    secret: &str,
    collection: &str,
    urn: &str,
) -> (StatusCode, serde_json::Value) {
    let resp = app
        .clone()
        .oneshot(bearer_get(
            &format!("/v1/{collection}/{urn}/versions"),
            secret,
        ))
        .await
        .unwrap();
    let status = resp.status();
    let body = response_body(resp).await;
    let json = serde_json::from_str(&body).unwrap_or(serde_json::Value::Null);
    (status, json)
}

#[tokio::test]
async fn agent_versions_list_draft_then_releases_with_notes_and_releasers() {
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let alice = mint_user_key(&store, "alice", UserRole::Publisher, PUBLISHER_SCOPES);
    let bob = mint_user_key(
        &store,
        "bob",
        UserRole::Member,
        &["catalog:write", "sessions:write"],
    );
    let app = test_app(app_state_http_only(store));
    let urn = "urn:vam:agent:local.vam.dev/acme/versions-http";

    assert_eq!(put_agent_as(&app, &alice, urn, "v1").await, StatusCode::OK);
    let (status, body) = release_with_note_as(
        &app,
        &alice,
        "agents",
        urn,
        (1, 0, 0),
        "http-v-100",
        "first",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    assert_eq!(put_agent_as(&app, &alice, urn, "v2").await, StatusCode::OK);
    let (status, body) = release_with_note_as(
        &app,
        &alice,
        "agents",
        urn,
        (1, 1, 0),
        "http-v-110",
        "adds jira",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    assert_eq!(put_agent_as(&app, &alice, urn, "v3").await, StatusCode::OK);
    let (status, body) = release_with_note_as(
        &app,
        &alice,
        "agents",
        urn,
        (1, 2, 0),
        "http-v-120",
        "fix typo",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, list) = versions_as(&app, &alice, "agents", urn).await;
    assert_eq!(status, StatusCode::OK, "{list}");
    let versions = list["versions"].as_array().expect("versions array");
    assert_eq!(versions.len(), 4, "{list}");
    assert_eq!(versions[0]["version"]["prerelease"], "draft");
    assert!(versions[0]["note"].is_null(), "{list}");
    assert_eq!(versions[1]["note"], "fix typo");
    assert_eq!(versions[1]["releasedBy"], "alice");
    assert_eq!(versions[2]["note"], "adds jira");
    assert_eq!(versions[2]["releasedBy"], "alice");
    assert_eq!(versions[3]["note"], "first");
    assert_eq!(versions[3]["releasedBy"], "alice");

    let (status, list) = versions_as(&app, &bob, "agents", urn).await;
    assert_eq!(status, StatusCode::OK, "{list}");
    let versions = list["versions"].as_array().expect("versions array");
    assert_eq!(versions.len(), 3, "bob sees releases only: {list}");
    assert!(versions
        .iter()
        .all(|v| v["version"]["prerelease"] != "draft"));

    let too_long = "x".repeat(501);
    let (status, body) = release_with_note_as(
        &app,
        &alice,
        "agents",
        urn,
        (2, 0, 0),
        "http-v-200",
        &too_long,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}

#[tokio::test]
async fn owner_without_grant_cannot_release_until_org_admin_grants() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let alice = mint_user_key(
        &store,
        "alice",
        UserRole::Member,
        &["catalog:write", "sessions:write"],
    );
    let admin = mint_user_key(&store, "admin", UserRole::OrgAdmin, ORG_ADMIN_SCOPES);
    let app = test_app(app_state_http_only(store.clone()));
    let urn = "urn:vam:agent:local.vam.dev/acme/grant-me";
    assert_eq!(
        put_agent_as(&app, &alice, urn, "draft").await,
        StatusCode::OK
    );
    assert_eq!(
        release_agent_as(&app, &alice, urn, "alice-no-grant").await,
        StatusCode::FORBIDDEN
    );

    let grant = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/users/alice/publish-grant")
                .header("authorization", format!("Bearer {admin}"))
                .header("Idempotency-Key", "grant-alice")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        grant.status(),
        StatusCode::OK,
        "{}",
        response_body(grant).await
    );
    assert_eq!(store.get_user("alice").unwrap().role, UserRole::Publisher);
    assert_eq!(
        release_agent_as(&app, &alice, urn, "alice-granted").await,
        StatusCode::OK
    );

    let revoke = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/v1/users/alice/publish-grant")
                .header("authorization", format!("Bearer {admin}"))
                .header("Idempotency-Key", "revoke-alice")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        revoke.status(),
        StatusCode::OK,
        "{}",
        response_body(revoke).await
    );
    assert_eq!(store.get_user("alice").unwrap().role, UserRole::Member);
    assert_eq!(
        release_agent_as(&app, &alice, urn, "alice-revoked").await,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn member_cannot_grant_publish_right() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let alice = mint_user_key(&store, "alice", UserRole::Member, MEMBER_SCOPES);
    let _ = mint_user_key(&store, "bob", UserRole::Member, MEMBER_SCOPES);
    let app = test_app(app_state_http_only(store));
    let grant = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/users/bob/publish-grant")
                .header("authorization", format!("Bearer {alice}"))
                .header("Idempotency-Key", "alice-grant-bob")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(grant.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn org_admin_can_reassign_catalog_owner_and_published_stays() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let alice = mint_user_key(&store, "alice", UserRole::Publisher, PUBLISHER_SCOPES);
    let bob = mint_user_key(&store, "bob", UserRole::Publisher, PUBLISHER_SCOPES);
    let admin = mint_user_key(&store, "admin", UserRole::OrgAdmin, ORG_ADMIN_SCOPES);
    let app = test_app(app_state_http_only(store.clone()));
    let urn = "urn:vam:agent:local.vam.dev/acme/leave-alice";
    assert_eq!(
        put_agent_as(&app, &alice, urn, "alice-owned").await,
        StatusCode::OK
    );
    assert_eq!(
        release_agent_as(&app, &alice, urn, "alice-release").await,
        StatusCode::OK
    );

    let reassign = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/catalog/reassign-owner")
                .header("authorization", format!("Bearer {admin}"))
                .header("Idempotency-Key", "reassign-alice-to-bob")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "kind": "agent",
                        "urn": urn,
                        "ownerUserId": "bob"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        reassign.status(),
        StatusCode::OK,
        "{}",
        response_body(reassign).await
    );
    assert_eq!(store.agent_owner_user_id(urn).unwrap(), "bob");

    let bob_get = app
        .clone()
        .oneshot(bearer_get(&format!("/v1/agents/{urn}"), &bob))
        .await
        .unwrap();
    assert_eq!(bob_get.status(), StatusCode::OK);
    let bob_get_body = response_body(bob_get).await;
    assert!(bob_get_body.contains(urn), "{bob_get_body}");

    assert_eq!(
        put_agent_as(&app, &alice, urn, "alice-after-leave").await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        put_agent_as(&app, &bob, urn, "bob-next-draft").await,
        StatusCode::OK
    );
}

#[tokio::test]
async fn member_cannot_reassign_catalog_owner() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let alice = mint_user_key(&store, "alice", UserRole::Publisher, PUBLISHER_SCOPES);
    let mallory = mint_user_key(
        &store,
        "mallory",
        UserRole::Member,
        &["catalog:write", "sessions:write"],
    );
    let _ = mint_user_key(&store, "bob", UserRole::Publisher, PUBLISHER_SCOPES);
    let app = test_app(app_state_http_only(store));
    let urn = "urn:vam:agent:local.vam.dev/acme/no-steal";
    assert_eq!(
        put_agent_as(&app, &alice, urn, "alice-keep").await,
        StatusCode::OK
    );
    let reassign = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/catalog/reassign-owner")
                .header("authorization", format!("Bearer {mallory}"))
                .header("Idempotency-Key", "mallory-reassign")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "kind": "agent",
                        "urn": urn,
                        "ownerUserId": "bob"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(reassign.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn org_admin_can_create_list_and_deactivate_users() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let admin = mint_user_key(&store, "admin", UserRole::OrgAdmin, ORG_ADMIN_SCOPES);
    let app = test_app(app_state_http_only(store.clone()));

    let created = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/users")
                .header("authorization", format!("Bearer {admin}"))
                .header("Idempotency-Key", "create-carol")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "username": "carol",
                        "password": "carol-secret"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let created_status = created.status();
    let created_text = response_body(created).await;
    assert_eq!(created_status, StatusCode::OK, "{created_text}");
    // The node generated the id: read the user back by the id the response carries.
    let created_body: serde_json::Value = serde_json::from_str(&created_text).expect("create body");
    let carol_id = created_body["user"]["userId"].as_str().expect("userId");
    assert!(carol_id.starts_with("usr_"), "{carol_id}");
    let carol = store.get_user(carol_id).expect("carol");
    assert_eq!(carol.username, "carol");
    assert_eq!(carol.role, UserRole::Member);
    assert!(carol.has_password);
    assert!(!carol.is_deactivated());

    let listed = app
        .clone()
        .oneshot(bearer_get("/v1/users", &admin))
        .await
        .unwrap();
    assert_eq!(listed.status(), StatusCode::OK);
    let listed_body = response_body(listed).await;
    assert!(listed_body.contains("carol"), "{listed_body}");
    assert!(listed_body.contains("admin"), "{listed_body}");

    let deactivated = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/v1/users/{carol_id}/deactivate"))
                .header("authorization", format!("Bearer {admin}"))
                .header("Idempotency-Key", "deactivate-carol")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        deactivated.status(),
        StatusCode::OK,
        "{}",
        response_body(deactivated).await
    );
    assert!(store.get_user(carol_id).expect("carol").is_deactivated());
}

#[tokio::test]
async fn member_cannot_create_list_or_deactivate_users() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let alice = mint_user_key(&store, "alice", UserRole::Member, MEMBER_SCOPES);
    let _ = mint_user_key(&store, "bob", UserRole::Member, MEMBER_SCOPES);
    let app = test_app(app_state_http_only(store));

    let create = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/users")
                .header("authorization", format!("Bearer {alice}"))
                .header("Idempotency-Key", "alice-create")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "username": "mallory",
                        "password": "nope"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(create.status(), StatusCode::FORBIDDEN);

    let list = app
        .clone()
        .oneshot(bearer_get("/v1/users", &alice))
        .await
        .unwrap();
    assert_eq!(list.status(), StatusCode::FORBIDDEN);

    let deactivate = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/users/bob/deactivate")
                .header("authorization", format!("Bearer {alice}"))
                .header("Idempotency-Key", "alice-deactivate-bob")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(deactivate.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn org_admin_users_password_reset_then_login() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let admin = mint_user_key(&store, "admin", UserRole::OrgAdmin, ORG_ADMIN_SCOPES);
    let alice_key = mint_user_key(&store, "alice", UserRole::Member, MEMBER_SCOPES);
    store
        .reset_user_password("alice", "old-alice-secret")
        .expect("seed password");
    let app = test_app(app_state_http_only(store.clone()));

    let old_login = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/login")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "username": "alice",
                        "password": "old-alice-secret"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(old_login.status(), StatusCode::OK);
    let old_body = response_body(old_login).await;
    let old_token = serde_json::from_str::<serde_json::Value>(&old_body).unwrap()["token"]
        .as_str()
        .expect("token")
        .to_string();

    let reset = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/users/alice/password")
                .header("authorization", format!("Bearer {admin}"))
                .header("Idempotency-Key", "reset-alice")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "password": "new-alice-secret"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        reset.status(),
        StatusCode::OK,
        "{}",
        response_body(reset).await
    );

    let stale_session = app
        .clone()
        .oneshot(bearer_get("/v1/me", &old_token))
        .await
        .unwrap();
    assert_eq!(stale_session.status(), StatusCode::UNAUTHORIZED);

    let key_still = app
        .clone()
        .oneshot(bearer_get("/v1/me", &alice_key))
        .await
        .unwrap();
    assert_eq!(key_still.status(), StatusCode::OK);

    let old_password = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/login")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "username": "alice",
                        "password": "old-alice-secret"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(old_password.status(), StatusCode::UNAUTHORIZED);

    let new_login = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/login")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "username": "alice",
                        "password": "new-alice-secret"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        new_login.status(),
        StatusCode::OK,
        "{}",
        response_body(new_login).await
    );
}

#[tokio::test]
async fn member_cannot_reset_users_password() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let alice = mint_user_key(&store, "alice", UserRole::Member, MEMBER_SCOPES);
    let _ = mint_user_key(&store, "bob", UserRole::Member, MEMBER_SCOPES);
    store
        .reset_user_password("bob", "bob-secret")
        .expect("bob password");
    let app = test_app(app_state_http_only(store));

    let reset = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/users/bob/password")
                .header("authorization", format!("Bearer {alice}"))
                .header("Idempotency-Key", "alice-reset-bob")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "password": "stolen"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(reset.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn org_admin_cannot_reset_operator_users_password() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let admin = mint_user_key(&store, "admin", UserRole::OrgAdmin, ORG_ADMIN_SCOPES);
    let app = test_app(app_state_http_only(store.clone()));

    let reset = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/users/operator/password")
                .header("authorization", format!("Bearer {admin}"))
                .header("Idempotency-Key", "reset-operator")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "password": "nope"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(reset.status(), StatusCode::FORBIDDEN);
    assert!(
        !store
            .get_user(OPERATOR_USER_ID)
            .expect("operator")
            .has_password
    );
}

#[tokio::test]
async fn users_password_reset_missing_user_is_404_not_create() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let admin = mint_user_key(&store, "admin", UserRole::OrgAdmin, ORG_ADMIN_SCOPES);
    let app = test_app(app_state_http_only(store.clone()));

    let reset = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/users/nobody/password")
                .header("authorization", format!("Bearer {admin}"))
                .header("Idempotency-Key", "reset-nobody")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "password": "would-create"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(reset.status(), StatusCode::NOT_FOUND);
    assert!(store.get_user("nobody").is_err());
}

#[tokio::test]
async fn deactivated_owner_published_catalog_still_resolves() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let alice = mint_user_key(&store, "alice", UserRole::Publisher, PUBLISHER_SCOPES);
    let bob = mint_user_key(
        &store,
        "bob",
        UserRole::Member,
        &["catalog:write", "sessions:write"],
    );
    let admin = mint_user_key(&store, "admin", UserRole::OrgAdmin, ORG_ADMIN_SCOPES);
    let app = test_app(app_state_http_only(store.clone()));
    let urn = "urn:vam:agent:local.vam.dev/acme/departed";
    assert_eq!(
        put_agent_as(&app, &alice, urn, "alice-published").await,
        StatusCode::OK
    );
    assert_eq!(
        release_agent_as(&app, &alice, urn, "alice-pub-release").await,
        StatusCode::OK
    );

    let deactivate = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/users/alice/deactivate")
                .header("authorization", format!("Bearer {admin}"))
                .header("Idempotency-Key", "deactivate-alice")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        deactivate.status(),
        StatusCode::OK,
        "{}",
        response_body(deactivate).await
    );
    assert!(store.get_user("alice").expect("alice").is_deactivated());

    let departed_get = app
        .clone()
        .oneshot(bearer_get(&format!("/v1/agents/{urn}"), &alice))
        .await
        .unwrap();
    assert_eq!(
        departed_get.status(),
        StatusCode::FORBIDDEN,
        "deactivated owner's key is 403 PermissionDenied"
    );

    let bob_get = app
        .oneshot(bearer_get(&format!("/v1/agents/{urn}"), &bob))
        .await
        .unwrap();
    assert_eq!(bob_get.status(), StatusCode::OK);
    let bob_get_body = response_body(bob_get).await;
    assert!(
        bob_get_body.contains(urn),
        "published artifact must stay resolvable after owner deactivate: {bob_get_body}"
    );
    assert_eq!(store.agent_owner_user_id(urn).unwrap(), "alice");
}

#[tokio::test]
async fn org_admin_can_mint_list_and_revoke_user_api_keys() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let admin = mint_user_key(&store, "admin", UserRole::OrgAdmin, ORG_ADMIN_SCOPES);
    store
        .create_managed_user("alice", "alice", "alice-secret", UserRole::Member)
        .expect("alice");
    let app = test_app(app_state_http_only(store.clone()));

    let mint = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/users/alice/keys")
                .header("authorization", format!("Bearer {admin}"))
                .header("Idempotency-Key", "mint-alice-ci")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "name": "alice-ci",
                        "scopes": ["node:read"]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let mint_status = mint.status();
    let mint_body = response_body(mint).await;
    assert_eq!(mint_status, StatusCode::OK, "{}", mint_body);
    assert!(mint_body.contains("vam_sk_dev_"), "{mint_body}");
    assert!(mint_body.contains("alice-ci"), "{mint_body}");
    let secret = mint_body
        .split("\"secret\":\"")
        .nth(1)
        .and_then(|s| s.split('"').next())
        .expect("mint secret in body");

    let listed = app
        .clone()
        .oneshot(bearer_get("/v1/users/alice/keys", &admin))
        .await
        .unwrap();
    assert_eq!(listed.status(), StatusCode::OK);
    let listed_body = response_body(listed).await;
    assert!(listed_body.contains("alice-ci"), "{listed_body}");

    let node = app
        .clone()
        .oneshot(bearer_get("/v1/node", secret))
        .await
        .unwrap();
    assert_eq!(
        node.status(),
        StatusCode::OK,
        "{}",
        response_body(node).await
    );

    let key_id = store
        .list_api_keys_for_owner("alice")
        .expect("alice keys")
        .into_iter()
        .find(|k| k.name == "alice-ci")
        .expect("alice-ci key")
        .key_id;

    let revoked = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/v1/users/alice/keys/{key_id}"))
                .header("authorization", format!("Bearer {admin}"))
                .header("Idempotency-Key", "revoke-alice-ci")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        revoked.status(),
        StatusCode::OK,
        "{}",
        response_body(revoked).await
    );

    let after = app.oneshot(bearer_get("/v1/node", secret)).await.unwrap();
    assert_eq!(after.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn member_cannot_admin_manage_another_users_api_keys() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let alice = mint_user_key(&store, "alice", UserRole::Member, MEMBER_SCOPES);
    let _ = mint_user_key(&store, "bob", UserRole::Member, MEMBER_SCOPES);
    let app = test_app(app_state_http_only(store));

    let list = app
        .clone()
        .oneshot(bearer_get("/v1/users/bob/keys", &alice))
        .await
        .unwrap();
    assert_eq!(list.status(), StatusCode::FORBIDDEN);

    let mint = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/users/bob/keys")
                .header("authorization", format!("Bearer {alice}"))
                .header("Idempotency-Key", "alice-mint-bob")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"name":"stolen","scopes":["node:read"]}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(mint.status(), StatusCode::FORBIDDEN);

    let revoke = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/v1/users/bob/keys/key-bob")
                .header("authorization", format!("Bearer {alice}"))
                .header("Idempotency-Key", "alice-revoke-bob")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(revoke.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn org_admin_user_keys_distinct_from_operator_keys_admin_mint() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let admin = mint_user_key(&store, "admin", UserRole::OrgAdmin, ORG_ADMIN_SCOPES);
    store
        .create_managed_user("alice", "alice", "alice-secret", UserRole::Member)
        .expect("alice");
    let app = test_app(app_state_http_only(store));

    let operator_mint = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/keys")
                .header("authorization", format!("Bearer {admin}"))
                .header("Idempotency-Key", "admin-operator-mint")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"name":"nope","userId":"alice","scopes":["node:read"]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(operator_mint.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn org_admin_can_write_list_and_delete_org_secrets() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let admin = mint_user_key(&store, "admin", UserRole::OrgAdmin, ORG_ADMIN_SCOPES);
    let alice = mint_user_key(&store, "alice", UserRole::Member, MEMBER_SCOPES);
    let app = test_app(app_state_http_only(store));
    let org_value = "org-shared-header";
    let user_value = "alice-user-header";

    let user_put = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/v1/secrets")
                .header("authorization", format!("Bearer {alice}"))
                .header("Idempotency-Key", "alice-user-jira")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "bindingId": "jira/auth-header",
                        "value": user_value,
                        "kind": 2,
                        "scope": 2,
                        "idempotencyKey": "alice-user-jira"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        user_put.status(),
        StatusCode::OK,
        "{}",
        response_body(user_put).await
    );

    let put = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/v1/org/secrets")
                .header("authorization", format!("Bearer {admin}"))
                .header("Idempotency-Key", "admin-org-jira")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "bindingId": "jira/auth-header",
                        "value": org_value,
                        "kind": 2,
                        "idempotencyKey": "admin-org-jira"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let put_status = put.status();
    let put_body = response_body(put).await;
    assert_eq!(put_status, StatusCode::OK, "{put_body}");
    assert!(!put_body.contains(org_value), "{put_body}");
    assert!(!put_body.contains(user_value), "{put_body}");
    assert!(
        put_body.contains(&SecretsVault::fingerprint(org_value)),
        "{put_body}"
    );
    assert!(put_body.contains("SCOPE_ORG"), "{put_body}");

    let listed = app
        .clone()
        .oneshot(bearer_get("/v1/org/secrets", &admin))
        .await
        .unwrap();
    assert_eq!(listed.status(), StatusCode::OK);
    let listed_body = response_body(listed).await;
    assert!(listed_body.contains("jira/auth-header"), "{listed_body}");
    assert!(
        listed_body.contains(&SecretsVault::fingerprint(org_value)),
        "{listed_body}"
    );
    assert!(
        !listed_body.contains(&SecretsVault::fingerprint(user_value)),
        "org list must not include user-scope rows: {listed_body}"
    );
    assert!(!listed_body.contains(org_value), "{listed_body}");
    assert!(!listed_body.contains(user_value), "{listed_body}");

    let deleted = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/v1/org/secrets/jira%2Fauth-header")
                .header("authorization", format!("Bearer {admin}"))
                .header("Idempotency-Key", "admin-del-jira")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let del_status = deleted.status();
    let del_body = response_body(deleted).await;
    assert_eq!(del_status, StatusCode::OK, "{del_body}");
    assert!(!del_body.contains(org_value), "{del_body}");

    let after = app
        .clone()
        .oneshot(bearer_get("/v1/org/secrets", &admin))
        .await
        .unwrap();
    let after_body = response_body(after).await;
    assert!(!after_body.contains("jira/auth-header"), "{after_body}");

    let user_get = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/secrets/jira%2Fauth-header?scope=user")
                .header("authorization", format!("Bearer {alice}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(user_get.status(), StatusCode::OK);
    let user_body = response_body(user_get).await;
    assert!(
        user_body.contains(&SecretsVault::fingerprint(user_value)),
        "user-scope row must survive org delete: {user_body}"
    );
}

#[tokio::test]
async fn member_cannot_admin_write_or_delete_org_secrets() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let alice = mint_user_key(&store, "alice", UserRole::Member, MEMBER_SCOPES);
    let app = test_app(app_state_http_only(store));

    let list = app
        .clone()
        .oneshot(bearer_get("/v1/org/secrets", &alice))
        .await
        .unwrap();
    assert_eq!(list.status(), StatusCode::FORBIDDEN);

    let put = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/v1/org/secrets")
                .header("authorization", format!("Bearer {alice}"))
                .header("Idempotency-Key", "alice-org-admin")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"bindingId":"jira/auth-header","value":"nope","kind":2}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(put.status(), StatusCode::FORBIDDEN);

    let delete = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/v1/org/secrets/jira%2Fauth-header")
                .header("authorization", format!("Bearer {alice}"))
                .header("Idempotency-Key", "alice-del-org")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(delete.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn catalog_visibility_http_own_drafts_and_tenant_published() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let alice = mint_user_key(
        &store,
        "alice",
        UserRole::Member,
        &["catalog:write", "sessions:write"],
    );
    let bob = mint_user_key(&store, "bob", UserRole::Publisher, PUBLISHER_SCOPES);
    let app = test_app(app_state_http_only(store));
    let alice_urn = "urn:vam:agent:local.vam.dev/acme/alice-vis";
    let bob_urn = "urn:vam:agent:local.vam.dev/acme/bob-vis";
    assert_eq!(
        put_agent_as(&app, &alice, alice_urn, "alice-draft").await,
        StatusCode::OK
    );
    assert_eq!(
        put_agent_as(&app, &bob, bob_urn, "bob-draft").await,
        StatusCode::OK
    );

    let alice_list = app
        .clone()
        .oneshot(bearer_get("/v1/agents", &alice))
        .await
        .unwrap();
    assert_eq!(alice_list.status(), StatusCode::OK);
    let alice_list_body = response_body(alice_list).await;
    assert!(
        alice_list_body.contains(alice_urn),
        "owner must see own draft: {alice_list_body}"
    );
    assert!(
        !alice_list_body.contains(bob_urn),
        "alice must not see bob draft: {alice_list_body}"
    );

    let alice_own = app
        .clone()
        .oneshot(bearer_get(&format!("/v1/agents/{alice_urn}"), &alice))
        .await
        .unwrap();
    assert_eq!(alice_own.status(), StatusCode::OK);
    let alice_own_body = response_body(alice_own).await;
    assert!(alice_own_body.contains(alice_urn), "{alice_own_body}");

    let alice_bob_draft = app
        .clone()
        .oneshot(bearer_get(&format!("/v1/agents/{bob_urn}"), &alice))
        .await
        .unwrap();
    assert_eq!(alice_bob_draft.status(), StatusCode::NOT_FOUND);

    assert_eq!(
        release_agent_as(&app, &bob, bob_urn, "bob-vis-release").await,
        StatusCode::OK
    );

    let alice_list_after = app
        .clone()
        .oneshot(bearer_get("/v1/agents", &alice))
        .await
        .unwrap();
    assert_eq!(alice_list_after.status(), StatusCode::OK);
    let alice_list_after_body = response_body(alice_list_after).await;
    assert!(
        alice_list_after_body.contains(alice_urn),
        "owner still sees own draft: {alice_list_after_body}"
    );
    assert!(
        alice_list_after_body.contains(bob_urn),
        "alice must see bob released: {alice_list_after_body}"
    );

    let alice_bob_released = app
        .oneshot(bearer_get(&format!("/v1/agents/{bob_urn}"), &alice))
        .await
        .unwrap();
    assert_eq!(alice_bob_released.status(), StatusCode::OK);
    let alice_bob_released_body = response_body(alice_bob_released).await;
    assert!(
        alice_bob_released_body.contains(bob_urn),
        "{alice_bob_released_body}"
    );
}

fn catalog_row_for_urn<'a>(
    records: &'a [serde_json::Value],
    urn: &str,
) -> Option<&'a serde_json::Value> {
    records.iter().find(|row| {
        row["spec"]["resourceId"]["urn"].as_str() == Some(urn)
            || row["resourceId"]["urn"].as_str() == Some(urn)
    })
}

fn assert_catalog_listing_fields(row: &serde_json::Value, owner: &str, draft: bool) {
    assert_eq!(
        row["ownerUserId"].as_str(),
        Some(owner),
        "owner_user_id missing: {row}"
    );
    let channel = row["channel"].as_str().unwrap_or("");
    if draft {
        assert!(
            channel.eq_ignore_ascii_case("DRAFT") || channel.eq_ignore_ascii_case("draft"),
            "expected draft channel: {row}"
        );
    }
    assert_eq!(
        row["catalogTier"].as_str(),
        Some("user"),
        "catalog_tier missing: {row}"
    );
}

#[tokio::test]
async fn catalog_visibility_admin_lists_foreign_agent_draft_with_owner() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let alice = mint_user_key(
        &store,
        "alice",
        UserRole::Member,
        &["catalog:write", "sessions:write"],
    );
    let bob = mint_user_key(
        &store,
        "bob",
        UserRole::Member,
        &["catalog:write", "sessions:write"],
    );
    let admin = mint_user_key(&store, "admin", UserRole::OrgAdmin, ORG_ADMIN_SCOPES);
    let app = test_app(app_state_http_only(store.clone()));
    let alice_urn = "urn:vam:agent:local.vam.dev/acme/alice-admin-draft";
    assert_eq!(
        put_agent_as(&app, &alice, alice_urn, "alice-secret-draft").await,
        StatusCode::OK
    );

    let bob_list = app
        .clone()
        .oneshot(bearer_get("/v1/agents", &bob))
        .await
        .unwrap();
    assert_eq!(bob_list.status(), StatusCode::OK);
    let bob_list_body = response_body(bob_list).await;
    assert!(
        !bob_list_body.contains(alice_urn),
        "member must not see alice draft: {bob_list_body}"
    );
    let bob_get = app
        .clone()
        .oneshot(bearer_get(&format!("/v1/agents/{alice_urn}"), &bob))
        .await
        .unwrap();
    assert_eq!(bob_get.status(), StatusCode::NOT_FOUND);

    let admin_list = app
        .clone()
        .oneshot(bearer_get("/v1/agents", &admin))
        .await
        .unwrap();
    assert_eq!(admin_list.status(), StatusCode::OK);
    let admin_list_body = response_body(admin_list).await;
    assert!(
        admin_list_body.contains(alice_urn),
        "admin must list alice draft: {admin_list_body}"
    );
    let admin_json: serde_json::Value =
        serde_json::from_str(&admin_list_body).expect("admin list json");
    let records = admin_json["records"].as_array().expect("records on list");
    let row = catalog_row_for_urn(records, alice_urn).expect("alice row");
    assert_catalog_listing_fields(row, "alice", true);

    let admin_get = app
        .clone()
        .oneshot(bearer_get(&format!("/v1/agents/{alice_urn}"), &admin))
        .await
        .unwrap();
    assert_eq!(admin_get.status(), StatusCode::OK);
    let admin_get_body = response_body(admin_get).await;
    let admin_get_json: serde_json::Value =
        serde_json::from_str(&admin_get_body).expect("admin get json");
    assert_eq!(
        admin_get_json["spec"]["resourceId"]["urn"].as_str(),
        Some(alice_urn)
    );
    assert_catalog_listing_fields(&admin_get_json, "alice", true);
}

#[tokio::test]
async fn catalog_visibility_admin_reassign_deactivated_owner_draft() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let alice = mint_user_key(&store, "alice", UserRole::Publisher, PUBLISHER_SCOPES);
    let bob = mint_user_key(&store, "bob", UserRole::Publisher, PUBLISHER_SCOPES);
    let admin = mint_user_key(&store, "admin", UserRole::OrgAdmin, ORG_ADMIN_SCOPES);
    let app = test_app(app_state_http_only(store.clone()));
    let urn = "urn:vam:agent:local.vam.dev/acme/alice-left-draft";
    assert_eq!(
        put_agent_as(&app, &alice, urn, "unpublished-work").await,
        StatusCode::OK
    );

    let deactivate = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/users/alice/deactivate")
                .header("authorization", format!("Bearer {admin}"))
                .header("Idempotency-Key", "deactivate-alice-draft-owner")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        deactivate.status(),
        StatusCode::OK,
        "{}",
        response_body(deactivate).await
    );

    let admin_list = app
        .clone()
        .oneshot(bearer_get("/v1/agents", &admin))
        .await
        .unwrap();
    assert_eq!(admin_list.status(), StatusCode::OK);
    let admin_list_body = response_body(admin_list).await;
    assert!(
        admin_list_body.contains(urn),
        "admin must still find deactivated owner's draft: {admin_list_body}"
    );
    let admin_json: serde_json::Value =
        serde_json::from_str(&admin_list_body).expect("admin list json");
    let row = catalog_row_for_urn(admin_json["records"].as_array().expect("records"), urn)
        .expect("draft row");
    assert_catalog_listing_fields(row, "alice", true);

    let reassign = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/catalog/reassign-owner")
                .header("authorization", format!("Bearer {admin}"))
                .header("Idempotency-Key", "reassign-deactivated-alice-draft")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "kind": "agent",
                        "urn": urn,
                        "ownerUserId": "bob"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        reassign.status(),
        StatusCode::OK,
        "{}",
        response_body(reassign).await
    );
    assert_eq!(store.agent_owner_user_id(urn).unwrap(), "bob");

    let bob_get = app
        .oneshot(bearer_get(&format!("/v1/agents/{urn}"), &bob))
        .await
        .unwrap();
    assert_eq!(bob_get.status(), StatusCode::OK);
    let bob_get_body = response_body(bob_get).await;
    let bob_json: serde_json::Value = serde_json::from_str(&bob_get_body).expect("bob get json");
    assert_catalog_listing_fields(&bob_json, "bob", true);
}

#[tokio::test]
async fn release_agent_with_unreleased_dependencies_is_409_listing_every_offender() {
    use vam_spec::v1::{
        agent_spec::InstructionSource, AgentSpec, ResourceRef, SkillManifest, Version,
    };

    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let alice = mint_user_key(&store, "alice", UserRole::Publisher, PUBLISHER_SCOPES);
    let skill = |name: &str| format!("urn:vam:skill:local.vam.dev/acme/{name}");
    let urn = "urn:vam:agent:local.vam.dev/acme/needs-skills";
    let floating = |u: String| ResourceRef {
        resource_id: Some(ResourceId { urn: u }),
        ..Default::default()
    };
    // `jira` has only a draft; `slack` does not exist at all.
    store
        .publish_skill_draft_with_owner(
            &SkillManifest {
                resource_id: Some(ResourceId { urn: skill("jira") }),
                version: Some(Version {
                    major: 1,
                    ..Default::default()
                }),
                description: "draft only".into(),
                ..Default::default()
            },
            "alice",
        )
        .unwrap();
    store
        .publish_agent_draft_with_owner(
            &AgentSpec {
                resource_id: Some(ResourceId {
                    urn: urn.to_string(),
                }),
                display_name: "Needs skills".into(),
                description: "test".into(),
                role: "tester".into(),
                instruction_source: Some(InstructionSource::SystemInstructions("go".into())),
                allow_any_playbook: true,
                skill_refs: vec![floating(skill("jira")), floating(skill("slack"))],
                ..Default::default()
            },
            "alice",
        )
        .unwrap();
    let app = test_app(app_state_http_only(store.clone()));

    let (status, body) = release_agent_version_as(&app, &alice, urn, "rel-deps", (1, 0, 0)).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    let message = json["message"].as_str().unwrap();
    for name in ["jira", "slack"] {
        assert!(
            message.contains(&format!(
                "skill '{}' has no released version: release it first, then release this agent.",
                skill(name)
            )),
            "{message}"
        );
    }
    assert!(
        message.contains("2 dependencies are not released"),
        "{message}"
    );
    assert_eq!(json["code"], "FailedPrecondition", "{json}");

    // Nothing was released: the version list holds the draft only.
    let versions = get_catalog_list(&app, &alice, &format!("/v1/agents/{urn}/versions")).await;
    let released = versions["versions"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|v| v["channel"] == "SKILL_CHANNEL_RELEASED")
        .count();
    assert_eq!(released, 0, "{versions}");
}

/// Node-level registry domains (AX-7). Installs the process-global for one
/// test and restores the unconfigured default on drop.
mod registry_domain_api {
    use super::skill_package_api::{
        drive9_temp, manifest_json, publish_package_as, skills_md, write_tar_gz,
    };
    use super::*;

    const ORG: &str = "vam.squrell.dev";
    const MARKETPLACE: &str = "hub.example.test";

    struct InstalledDomains;

    impl InstalledDomains {
        fn new(registry: &str, marketplace: Option<&str>) -> Self {
            vam_orchestrator::registry::install_registry_domains(
                vam_spec::RegistryDomains::new(registry, marketplace).expect("domains"),
            );
            Self
        }
    }

    impl Drop for InstalledDomains {
        fn drop(&mut self) {
            vam_orchestrator::registry::install_registry_domains(
                vam_spec::RegistryDomains::default(),
            );
        }
    }

    fn skill_archive(urn: &str) -> Vec<u8> {
        write_tar_gz(&[
            ("SKILLS.md", skills_md(urn, "v1").as_bytes()),
            ("manifest.json", manifest_json(urn).as_bytes()),
            ("execute.js", b"export default 1;\n"),
        ])
    }

    #[tokio::test]
    async fn me_returns_configured_domains_for_admin_and_member() {
        let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _state = isolate_state();
        let _domains = InstalledDomains::new(ORG, Some(MARKETPLACE));
        let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
        let admin = mint_user_key(&store, "admin", UserRole::OrgAdmin, ORG_ADMIN_SCOPES);
        let member = mint_user_key(&store, "bob", UserRole::Member, MEMBER_SCOPES);
        let app = test_app(app_state_http_only(store));
        for (key, namespace) in [(admin, "admin"), (member, "bob")] {
            let response = app
                .clone()
                .oneshot(bearer_get("/v1/me", &key))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let json: serde_json::Value =
                serde_json::from_str(&response_body(response).await).unwrap();
            assert_eq!(json["registryDomain"], ORG, "{json}");
            assert_eq!(json["marketplaceDomain"], MARKETPLACE, "{json}");
            assert_eq!(json["defaultNamespace"], namespace, "{json}");
        }
    }

    #[tokio::test]
    async fn org_domain_skill_publishes_and_other_domain_is_still_refused() {
        let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _state = isolate_state();
        let _drive9 = drive9_temp();
        let _domains = InstalledDomains::new(ORG, None);
        let (store, secret) = bootstrap_store();
        let app = test_app(app_state_http_only(store));

        let org = skill_archive("urn:vam:skill:vam.squrell.dev/acme/org-tools");
        let response = publish_package_as(&app, &secret, &org, "org-skill").await;
        let status = response.status();
        let body = response_body(response).await;
        assert_eq!(status, StatusCode::OK, "{body}");

        let hub = skill_archive("urn:vam:skill:hub.vam.dev/acme/hub-tools");
        let response = publish_package_as(&app, &secret, &hub, "hub-skill").await;
        let status = response.status();
        let body = response_body(response).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(body.contains("is not a local catalog registry"), "{body}");
    }

    #[tokio::test]
    async fn create_under_marketplace_domain_is_refused_only_when_set() {
        let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _state = isolate_state();
        let (store, secret) = bootstrap_store();
        let app = test_app(app_state_http_only(store));
        let urn = format!("urn:vam:agent:{MARKETPLACE}/acme/forged");
        // Marketplace not set: nothing extra is refused.
        put_test_agent(&app, &secret, &urn, "Forged").await;

        let _domains = InstalledDomains::new(ORG, Some(MARKETPLACE));
        let other = format!("urn:vam:agent:{MARKETPLACE}/acme/forged-two");
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/v1/agents")
                    .header("authorization", format!("Bearer {secret}"))
                    .header("Idempotency-Key", "forge-two")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "spec": {
                                "resourceId": {"urn": other},
                                "displayName": "Forged",
                                "description": "x",
                                "role": "Tester",
                                "systemInstructions": "x",
                                "allowAnyPlaybook": true
                            },
                            "idempotencyKey": "forge-two"
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let body = response_body(response).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(body.contains("marketplace domain"), "{body}");
    }
}

// ---------------------------------------------------------------------------
// Stop and queue (AR-3.7, AR-3.8)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stop_of_a_run_that_has_ended_answers_with_its_own_end_every_time() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let (store, app, secret) = inbox_app().await;
    live_task(&store, "stop-ended-ws", "main", LOCAL_USER_ID);
    store.update_run_state(INBOX_RUN, "SUCCEEDED").unwrap();
    let uri = format!("/v1/runs/{}/stop", encode_run_path(INBOX_RUN));
    for key in ["stop-1", "stop-2"] {
        let (status, body) = post_json(&app, &uri, &secret, key, serde_json::json!({})).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["status"]["state"], "RUN_STATE_SUCCEEDED", "{body}");
        assert!(body["discarded"].as_array().is_none_or(|a| a.is_empty()));
    }
    assert!(
        !store.session_ends.run_stop_recorded(INBOX_RUN),
        "a stop of an ended run records nothing"
    );
}

#[tokio::test]
async fn stop_is_for_the_runs_owner_only() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let (store, _secret) = bootstrap_store();
    let (_alice, bob) = two_member_keys(&store);
    let app = test_app(app_state_http_only(store.clone()));
    live_task(&store, "stop-own-ws", "main", "alice");
    let uri = format!("/v1/runs/{}/stop", encode_run_path(INBOX_RUN));
    let (status, body) = post_json(&app, &uri, &bob, "b-stop", serde_json::json!({})).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert!(!store.session_ends.run_stop_recorded(INBOX_RUN));
}

#[tokio::test]
async fn stopping_a_queued_message_discards_it_and_hands_back_its_text() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let (store, app, secret) = inbox_app().await;
    live_task(&store, "stop-q-ws", "main", LOCAL_USER_ID);
    let (_, queued) = post_json(
        &app,
        "/v1/runs",
        &secret,
        "sq-1",
        steer_body(
            "stop-q-ws",
            "side",
            "sq-1",
            serde_json::json!({"prompt": "write b.txt"}),
        ),
    )
    .await;
    let queued_id = queued["inbox"]["runId"]["urn"]
        .as_str()
        .unwrap()
        .to_string();
    let uri = format!("/v1/runs/{}/stop", encode_run_path(&queued_id));
    let (status, body) = post_json(&app, &uri, &secret, "sq-stop", serde_json::json!({})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"]["state"], "RUN_STATE_CANCELLED", "{body}");
    assert_eq!(body["discarded"][0]["text"], "write b.txt", "{body}");
    assert_eq!(body["discarded"][0]["mode"], "QUEUE");
    assert_eq!(body["discarded"][0]["runId"]["urn"], queued_id.as_str());
    let rows = store.list_inbox_messages(INBOX_RUN).unwrap();
    assert_eq!(rows[0].state, "discarded");
    assert!(rows[0].text.is_empty(), "the text is not kept");
    let events = store.read_outbox_for_run(INBOX_RUN, 0).unwrap();
    assert!(events
        .iter()
        .any(|e| e.event_type == "vam.v1.inbox.discarded"));
    // Again: nothing more to discard.
    let (status, again) = post_json(&app, &uri, &secret, "sq-stop-2", serde_json::json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert!(again["discarded"].as_array().is_none_or(|a| a.is_empty()));
}

#[tokio::test]
async fn cancelling_a_run_discards_its_pending_and_queued_messages() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let (store, app, secret) = inbox_app().await;
    live_task(&store, "cancel-ws", "main", LOCAL_USER_ID);
    let (_, steer) = post_json(
        &app,
        "/v1/runs",
        &secret,
        "c-steer",
        steer_body(
            "cancel-ws",
            "main",
            "c-steer",
            serde_json::json!({"prompt": "change"}),
        ),
    )
    .await;
    assert_eq!(steer["inbox"]["mode"], "STEER");
    let (_, queued) = post_json(
        &app,
        "/v1/runs",
        &secret,
        "c-queue",
        steer_body(
            "cancel-ws",
            "side",
            "c-queue",
            serde_json::json!({"prompt": "later"}),
        ),
    )
    .await;
    let queued_id = queued["inbox"]["runId"]["urn"]
        .as_str()
        .unwrap()
        .to_string();
    let uri = format!("/v1/runs/{}/cancel", encode_run_path(INBOX_RUN));
    let (status, body) = post_json(&app, &uri, &secret, "cancel-1", serde_json::json!({})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(store
        .list_inbox_messages(INBOX_RUN)
        .unwrap()
        .iter()
        .all(|r| r.state == "discarded" && r.text.is_empty()));
    assert_eq!(store.get_run_state(&queued_id).unwrap(), "CANCELLED");
    let events = store.read_outbox_for_run(INBOX_RUN, 0).unwrap();
    assert!(events
        .iter()
        .any(|e| e.event_type == "vam.v1.inbox.discarded"));
}

/// The state a test needs to ask the starter for a pass.
fn starter_state(store: Arc<SqliteWorkflowStore>) -> AppState {
    app_state_http_only(store)
}

/// A run by agent ref starts from the released agent.
fn release_inbox_agent(store: &SqliteWorkflowStore) {
    store
        .release_agent(
            INBOX_AGENT,
            &vam_spec::v1::Version {
                major: 1,
                minor: 0,
                patch: 0,
                prerelease: String::new(),
                build: String::new(),
            },
        )
        .expect("release the test agent");
}

#[tokio::test]
async fn the_starter_starts_the_oldest_queued_message_after_the_task_ends_once() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let (store, secret) = bootstrap_store();
    let state = starter_state(store.clone());
    let app = test_app(state.clone());
    put_test_agent(&app, &secret, INBOX_AGENT, "Test Agent").await;
    release_inbox_agent(&store);
    let session = live_task(&store, "start-ws", "main", LOCAL_USER_ID);
    let mut first = steer_body(
        "start-ws",
        "side",
        "st-1",
        serde_json::json!({"prompt": "first"}),
    );
    first["spec"]["inboxMode"] = serde_json::json!("QUEUE");
    let (_, one) = post_json(&app, "/v1/runs", &secret, "st-1", first).await;
    let (_, two) = post_json(
        &app,
        "/v1/runs",
        &secret,
        "st-2",
        steer_body(
            "start-ws",
            "side",
            "st-2",
            serde_json::json!({"prompt": "second"}),
        ),
    )
    .await;
    let one_id = one["inbox"]["runId"]["urn"].as_str().unwrap().to_string();
    let two_id = two["inbox"]["runId"]["urn"].as_str().unwrap().to_string();

    // While the task lives, nothing starts.
    assert_eq!(
        vam_orchestrator::api::queue::run_queue_pass(&state).await,
        0
    );
    assert_eq!(store.get_run_state(&one_id).unwrap(), "PENDING");

    // The task ends and its session is gone: the oldest message starts, only it.
    store.update_run_state(INBOX_RUN, "SUCCEEDED").unwrap();
    store
        .update_observed_state(
            &session,
            vam_orchestrator::session::ObservedState::Terminated,
        )
        .unwrap();
    let started = vam_orchestrator::api::queue::run_queue_pass(&state).await;
    assert_eq!(
        started,
        1,
        "state={:?} result={:?}",
        store.get_run_state(&one_id),
        store.get_run_result_json(&one_id)
    );
    assert_eq!(
        store.get_run_state(&one_id).unwrap(),
        "RUNNING",
        "adopted, same run id"
    );
    assert!(
        store.get_run_session_id(&one_id).unwrap().is_some(),
        "it has its own session now"
    );
    assert_eq!(store.get_run_state(&two_id).unwrap(), "PENDING");
    let rows = store.list_inbox_messages(INBOX_RUN).unwrap();
    assert_eq!(
        rows.iter().map(|r| r.state.as_str()).collect::<Vec<_>>(),
        vec!["started", "queued"]
    );
    let started: Vec<_> = store
        .read_outbox_for_run(INBOX_RUN, 0)
        .unwrap()
        .into_iter()
        .filter(|e| e.event_type == "vam.v1.inbox.started")
        .collect();
    assert_eq!(started.len(), 1);
    assert!(
        started[0].payload.contains(&one_id),
        "{}",
        started[0].payload
    );
    // Its workspace is busy now, so the second waits; a repeated pass starts nothing twice.
    assert_eq!(
        vam_orchestrator::api::queue::run_queue_pass(&state).await,
        0
    );
    assert_eq!(store.get_run_state(&two_id).unwrap(), "PENDING");
}

#[tokio::test]
async fn a_message_queued_behind_a_run_waiting_to_resume_starts_once_after_it_and_not_before() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let (store, secret) = bootstrap_store();
    let state = starter_state(store.clone());
    let app = test_app(state.clone());
    put_test_agent(&app, &secret, INBOX_AGENT, "Test Agent").await;
    release_inbox_agent(&store);
    let session = live_task(&store, "resume-ws", "main", LOCAL_USER_ID);
    let ws = place_of("resume-ws", "main").unwrap().0;
    store.update_run_state(INBOX_RUN, "RUNNING").unwrap();

    // A message is queued for the next task while this one runs.
    let mut queued = steer_body(
        "resume-ws",
        "side",
        "rq-1",
        serde_json::json!({"prompt": "after the task"}),
    );
    queued["spec"]["inboxMode"] = serde_json::json!("QUEUE");
    let (_, one) = post_json(&app, "/v1/runs", &secret, "rq-1", queued).await;
    let one_id = one["inbox"]["runId"]["urn"].as_str().unwrap().to_string();

    // The guest crashed: its session is gone and the run waits for a new one. It is not
    // terminal, so the message keeps waiting; the workspace is not free either.
    store
        .update_observed_state(
            &session,
            vam_orchestrator::session::ObservedState::Terminated,
        )
        .unwrap();
    store
        .mark_resume_pending(INBOX_RUN, "guest_crashed", &ws, &session)
        .unwrap();
    assert_eq!(
        store.resume_holding_workspace(&ws).unwrap().as_deref(),
        Some(INBOX_RUN)
    );
    assert_eq!(
        vam_orchestrator::api::queue::run_queue_pass(&state).await,
        0,
        "the starter never starts a message behind a run waiting to resume"
    );
    assert_eq!(store.get_run_state(&one_id).unwrap(), "PENDING");

    // A new task for the workspace does not start on it either: it queues.
    let (status, other) = post_json(
        &app,
        "/v1/runs",
        &secret,
        "rq-2",
        steer_body(
            "resume-ws",
            "side",
            "rq-2",
            serde_json::json!({"prompt": "another"}),
        ),
    )
    .await;
    assert!(status.is_success(), "{other}");
    let other_id = other["inbox"]["runId"]["urn"].as_str().unwrap().to_string();
    assert_eq!(store.get_run_state(&other_id).unwrap(), "PENDING");
    assert_eq!(
        vam_orchestrator::api::queue::run_queue_pass(&state).await,
        0
    );

    // The pass takes the run and it finishes; the oldest message starts, once.
    assert!(matches!(
        store.claim_resume(INBOX_RUN).unwrap(),
        vam_orchestrator::resume_store::Claim::Won(_)
    ));
    assert_eq!(
        vam_orchestrator::api::queue::run_queue_pass(&state).await,
        0,
        "a run being resumed is still not finished"
    );
    store.update_run_state(INBOX_RUN, "SUCCEEDED").unwrap();
    assert_eq!(
        vam_orchestrator::api::queue::run_queue_pass(&state).await,
        1
    );
    assert_eq!(store.get_run_state(&one_id).unwrap(), "RUNNING");
    assert_eq!(store.get_run_state(&other_id).unwrap(), "PENDING");
    assert_eq!(
        vam_orchestrator::api::queue::run_queue_pass(&state).await,
        0,
        "a second pass starts nothing twice"
    );
}

/// A task whose session is gone and that waits to be resumed (a crash or a restart under it).
fn run_waiting_to_resume(
    store: &SqliteWorkflowStore,
    name: &str,
    reason: &str,
) -> (String, String) {
    let session = live_task(store, name, "main", LOCAL_USER_ID);
    let ws = place_of(name, "main").unwrap().0;
    store.update_run_state(INBOX_RUN, "RUNNING").unwrap();
    store
        .update_observed_state(
            &session,
            vam_orchestrator::session::ObservedState::Terminated,
        )
        .unwrap();
    store
        .mark_resume_pending(INBOX_RUN, reason, &ws, &session)
        .unwrap();
    (session, ws)
}

async fn a_task_over_its_resume_cap_ends_with(reason: &str, cap: u32, ended_as: &str) {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let (store, _secret) = bootstrap_store();
    let state = starter_state(store.clone());
    let (session, ws) = run_waiting_to_resume(&store, "cap-ws", reason);
    // It has used every resume its kind allows.
    for _ in 0..cap {
        assert!(matches!(
            store.claim_resume(INBOX_RUN).unwrap(),
            vam_orchestrator::resume_store::Claim::Won(_)
        ));
        store
            .mark_resume_pending(INBOX_RUN, reason, &ws, &session)
            .unwrap();
    }
    assert_eq!(
        vam_orchestrator::api::resume_run::run_resume_pass(&state).await,
        0,
        "a task over its cap is not resumed"
    );
    assert_eq!(store.get_run_state(INBOX_RUN).unwrap(), "FAILED");
    let result = store.get_run_result_json(INBOX_RUN).unwrap().unwrap();
    assert!(result.contains(ended_as), "{result}");
    let row = store.resume_row(INBOX_RUN).unwrap().unwrap();
    assert_eq!(
        row.state,
        vam_orchestrator::resume_store::ResumeState::Ended
    );
    assert_eq!(row.total(), cap, "no attempt was spent on the refusal");
    assert!(store.resume_holding_workspace(&ws).unwrap().is_none());
    assert_eq!(
        store.get_run_session_id(INBOX_RUN).unwrap().as_deref(),
        Some(session.as_str()),
        "no new session was booted"
    );
}

#[tokio::test]
async fn a_third_guest_crash_ends_the_task_crashed_repeatedly() {
    a_task_over_its_resume_cap_ends_with(
        "guest_crashed",
        vam_orchestrator::resume_store::MAX_CRASH_RESUMES,
        "crashed_repeatedly",
    )
    .await;
}

#[tokio::test]
async fn too_many_vamd_restarts_end_the_task_restarted_repeatedly() {
    a_task_over_its_resume_cap_ends_with(
        "vamd_restarted",
        vam_orchestrator::resume_store::MAX_RESTART_RESUMES,
        "restarted_repeatedly",
    )
    .await;
}

#[tokio::test]
async fn a_task_whose_journal_holds_a_legacy_call_id_is_not_resumed() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let (store, _secret) = bootstrap_store();
    let state = starter_state(store.clone());
    let (session, _ws) = run_waiting_to_resume(&store, "legacy-ws", "vamd_restarted");
    // An old guest sent no call id: the host journaled a create under a made-up id.
    store
        .effect_begin(&vam_orchestrator::effect_store::NewEffect {
            run_id: INBOX_RUN.to_string(),
            call_id: "legacy-echo_create_ticket-abcdef01-1".to_string(),
            parent_call_id: None,
            tool: "echo_create_ticket".to_string(),
            adapter: "hsp".to_string(),
            class: vam_orchestrator::effect_class::EffectClass::NonIdempotent,
            idempotency_key: "vamfx_legacy".to_string(),
            args_digest: "abcdef0123456789".to_string(),
            approval_id: None,
        })
        .unwrap();
    assert_eq!(
        vam_orchestrator::api::resume_run::run_resume_pass(&state).await,
        1,
        "the pass takes the run, then refuses it"
    );
    let mut state_now = String::new();
    for _ in 0..100 {
        state_now = store.get_run_state(INBOX_RUN).unwrap();
        if state_now == "FAILED" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(state_now, "FAILED");
    let result = store.get_run_result_json(INBOX_RUN).unwrap().unwrap();
    assert!(result.contains("not_resumable"), "{result}");
    assert!(result.contains("older guest"), "{result}");
    assert_eq!(
        store.get_run_session_id(INBOX_RUN).unwrap().as_deref(),
        Some(session.as_str()),
        "no new session was booted"
    );
}

#[tokio::test]
async fn a_run_the_user_is_stopping_is_not_claimed_by_the_resume_pass() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let (store, _secret) = bootstrap_store();
    let state = starter_state(store.clone());
    run_waiting_to_resume(&store, "stopping-ws", "guest_crashed");
    // Persisted only, as after a restart: the stop wins whoever looks first.
    store.persist_run_stop(INBOX_RUN).unwrap();
    assert_eq!(
        vam_orchestrator::api::resume_run::run_resume_pass(&state).await,
        0
    );
    let row = store.resume_row(INBOX_RUN).unwrap().unwrap();
    assert_eq!(
        (row.state, row.total()),
        (vam_orchestrator::resume_store::ResumeState::Pending, 0),
        "left for the stop to take"
    );
    assert_eq!(store.get_run_state(INBOX_RUN).unwrap(), "RUNNING");
}

#[tokio::test]
async fn nothing_starts_after_a_user_stop_and_a_cancelled_message_is_skipped() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let (store, secret) = bootstrap_store();
    let state = starter_state(store.clone());
    let app = test_app(state.clone());
    put_test_agent(&app, &secret, INBOX_AGENT, "Test Agent").await;
    live_task(&store, "nostart-ws", "main", LOCAL_USER_ID);
    let (_, queued) = post_json(
        &app,
        "/v1/runs",
        &secret,
        "ns-1",
        steer_body(
            "nostart-ws",
            "side",
            "ns-1",
            serde_json::json!({"prompt": "later"}),
        ),
    )
    .await;
    let queued_id = queued["inbox"]["runId"]["urn"]
        .as_str()
        .unwrap()
        .to_string();
    // The user stopped the task: its messages were discarded with it.
    store.session_ends.record_run_stop(INBOX_RUN);
    vam_orchestrator::inbox::discard_for_stop(&store, INBOX_RUN).unwrap();
    store
        .complete_run_with_result(INBOX_RUN, "FAILED", r#"{"error":"x"}"#)
        .unwrap();
    assert_eq!(
        vam_orchestrator::api::queue::run_queue_pass(&state).await,
        0
    );
    assert_eq!(store.get_run_state(&queued_id).unwrap(), "CANCELLED");
    assert!(store.list_queued().unwrap().is_empty());
}

#[tokio::test]
async fn an_undelivered_steer_at_the_end_of_a_task_becomes_the_next_task() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let (store, secret) = bootstrap_store();
    let state = starter_state(store.clone());
    let app = test_app(state.clone());
    put_test_agent(&app, &secret, INBOX_AGENT, "Test Agent").await;
    release_inbox_agent(&store);
    let session = live_task(&store, "steer-end-ws", "main", LOCAL_USER_ID);
    for (key, text) in [("se-1", "also add a title"), ("se-2", "and a footer")] {
        let (status, _) = post_json(
            &app,
            "/v1/runs",
            &secret,
            key,
            steer_body(
                "steer-end-ws",
                "main",
                key,
                serde_json::json!({"prompt": text}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED);
    }
    // The task ended without taking them in (and the log holds neither).
    store.update_run_state(INBOX_RUN, "FAILED").unwrap();
    store
        .update_observed_state(
            &session,
            vam_orchestrator::session::ObservedState::Terminated,
        )
        .unwrap();
    assert_eq!(
        vam_orchestrator::api::queue::run_queue_pass(&state).await,
        1
    );
    let rows = store.list_inbox_messages(INBOX_RUN).unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[2].mode, "QUEUE");
    assert_eq!(rows[2].state, "started");
    assert!(rows[0].state == "started" && rows[1].state == "started");
    let new_run = rows[2].started_run_id.clone().unwrap();
    let (spec_json, _, _) = store.get_run_spec_context(&new_run).unwrap();
    assert!(
        spec_json
            .unwrap()
            .contains("also add a title\\n\\nand a footer"),
        "one task, both texts"
    );
    // Nothing runs twice.
    assert_eq!(
        vam_orchestrator::api::queue::run_queue_pass(&state).await,
        0
    );
}

// WO-1.4: the shared start path. A triggered caller goes through `start_run` directly.

fn trigger_caller(
    credential: &str,
    not_after: Option<i64>,
) -> vam_orchestrator::api::run_start::RunCaller {
    use vam_orchestrator::api::run_start::{RunCaller, RunSource};
    RunCaller {
        owner_user_id: LOCAL_USER_ID.to_string(),
        principal: "User::\"tester\"".to_string(),
        role: vam_orchestrator::users::UserRole::default(),
        scopes: vec!["runs:write".to_string()],
        credential_id: Some(credential.to_string()),
        users_admin: false,
        source: RunSource::Trigger {
            broker_id: "b1".into(),
            grant_id: "g1".into(),
            trigger_ref: "t1".into(),
            fire_at: 1,
            fire_kind: "cron".into(),
            surface: "api".into(),
        },
        not_after,
        key_id: String::new(),
        name: "grant".into(),
        session_token: false,
        key_owner_user_id: LOCAL_USER_ID.to_string(),
    }
}

// ---- WO-1.6a: the trigger gateway -----------------------------------------------------------

/// Approves broker `id` with a `run_request_permission` over tool `echo.ping`.
fn approve_gateway_broker(store: &SqliteWorkflowStore, id: &str, tier: i32, per_minute: i32) {
    use vam_spec::v1::{CapabilityManifest, RunRequestPermission, ToolExport};
    store
        .upsert_hsp_broker_approval(&CapabilityManifest {
            broker_id: id.to_string(),
            trust_tier: tier,
            tools: vec![ToolExport {
                tool_name: "echo.ping".to_string(),
                side_effect: 0,
                ..Default::default()
            }],
            run_request_permission: Some(RunRequestPermission {
                grant_tools: vec!["echo.ping".to_string()],
                loop_guarded_tools: vec!["echo.ping".to_string()],
                surface: "echo".to_string(),
                max_requests_per_minute: per_minute,
                ..Default::default()
            }),
            ..Default::default()
        })
        .expect("approve the broker");
}

/// A grant for `broker` bound to the place (`""` = the one `live_task` made last is not looked
/// up: the caller passes ids). Returns (grant id, token).
fn make_grant(
    store: &SqliteWorkflowStore,
    owner: &str,
    broker: &str,
    space_id: &str,
    thread_id: &str,
    on_busy: &str,
) -> (String, String) {
    use vam_orchestrator::credentials::GrantInsert;
    let constraints = vam_orchestrator::grants::GrantConstraints {
        agent_urn: INBOX_AGENT.to_string(),
        space_id: space_id.to_string(),
        thread_id: thread_id.to_string(),
        message: "fired".to_string(),
        playbook_ref: None,
        inputs: serde_json::json!({}),
        on_busy: on_busy.to_string(),
    };
    let (record, token) = store
        .insert_grant_credential(&GrantInsert {
            name: "grant: echo.ping".to_string(),
            owner_user_id: owner.to_string(),
            holder: format!("broker:{broker}"),
            target_kind: "space".to_string(),
            target_id: space_id.to_string(),
            constraints_json: serde_json::to_string(&constraints).unwrap(),
            issued_via: "invoke".to_string(),
            source_run_id: None,
            tool_name: Some("echo.ping".to_string()),
            scopes: vec!["runs:start".to_string()],
            expires_at: 4_102_444_800,
        })
        .expect("insert the grant");
    (record.key_id, token)
}

fn start_request(body: serde_json::Value) -> vam_spec::v1::CreateRunRequest {
    vam_spec::encoding::json_to_proto("vam.v1.CreateRunRequest", &body.to_string()).unwrap()
}

fn trigger_opts(key: &str) -> vam_orchestrator::api::run_start::StartOpts {
    vam_orchestrator::api::run_start::StartOpts {
        skip_if_busy: false,
        idempotency_key: key.to_string(),
        traceparent: None,
    }
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn a_triggered_fire_on_a_busy_workspace_queues_keeps_its_inputs_and_records_its_source() {
    use vam_orchestrator::api::run_start::{start_run, StartError, StartOutcome};
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let (store, secret) = bootstrap_store();
    let state = starter_state(store.clone());
    let app = test_app(state.clone());
    put_test_agent(&app, &secret, INBOX_AGENT, "Test Agent").await;
    release_inbox_agent(&store);
    let session = live_task(&store, "trig-ws", "main", LOCAL_USER_ID);
    let body = |key: &str| {
        steer_body(
            "trig-ws",
            "main",
            key,
            serde_json::json!({"prompt": "fire", "extra": "kept"}),
        )
    };

    // A person sending the same inputs to the running task is refused.
    let (status, _) = post_json(&app, "/v1/runs", &secret, "p-1", body("p-1")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // A triggered fire never steers: it queues behind the task with its inputs.
    approve_gateway_broker(&store, "b1", 2, 0);
    let cred1 = make_grant(&store, LOCAL_USER_ID, "b1", "", "", "QUEUE").0;
    let cred2 = make_grant(&store, LOCAL_USER_ID, "b1", "", "", "QUEUE").0;
    let caller = trigger_caller(&cred1, None);
    let outcome = start_run(
        &state,
        &caller,
        start_request(body("f-1")),
        trigger_opts("f-1"),
    )
    .await
    .map_err(|_| "start refused")
    .unwrap();
    let StartOutcome::Queued { receipt, .. } = outcome else {
        panic!("expected a queued fire");
    };
    let queued_id = receipt.run_id.unwrap().urn;
    assert_eq!(store.get_run_state(&queued_id).unwrap(), "PENDING");
    assert_eq!(
        store.run_origin(&queued_id).unwrap(),
        (Some("broker:b1".to_string()), Some(cred1.clone()))
    );
    let rows = store.list_inbox_messages(INBOX_RUN).unwrap();
    assert!(rows[0]
        .queued_spec_json
        .as_deref()
        .unwrap()
        .contains("kept"));

    // One queued fire per credential; another credential still queues.
    match start_run(
        &state,
        &caller,
        start_request(body("f-2")),
        trigger_opts("f-2"),
    )
    .await
    {
        Err(StartError::Busy { kind, .. }) => {
            assert_eq!(
                kind,
                vam_orchestrator::api::run_start::BusyKind::AlreadyQueued
            )
        }
        _ => panic!("a second fire from one credential must be refused"),
    }
    let other = trigger_caller(&cred2, None);
    assert!(matches!(
        start_run(
            &state,
            &other,
            start_request(body("f-3")),
            trigger_opts("f-3")
        )
        .await,
        Ok(StartOutcome::Queued { .. })
    ));

    // The task ends: the starter starts the oldest fire as a run with its source kept.
    store.update_run_state(INBOX_RUN, "SUCCEEDED").unwrap();
    store
        .update_observed_state(
            &session,
            vam_orchestrator::session::ObservedState::Terminated,
        )
        .unwrap();
    assert_eq!(
        vam_orchestrator::api::queue::run_queue_pass(&state).await,
        1
    );
    assert_eq!(store.get_run_state(&queued_id).unwrap(), "RUNNING");
    assert_eq!(
        store.run_origin(&queued_id).unwrap(),
        (Some("broker:b1".to_string()), Some(cred1.clone()))
    );
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn a_person_run_has_no_source_and_a_queued_fire_past_not_after_ends_expired() {
    use vam_orchestrator::api::run_start::{start_run, StartOutcome};
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let (store, secret) = bootstrap_store();
    let state = starter_state(store.clone());
    let app = test_app(state.clone());
    put_test_agent(&app, &secret, INBOX_AGENT, "Test Agent").await;
    release_inbox_agent(&store);
    let session = live_task(&store, "exp-ws", "main", LOCAL_USER_ID);

    // A queued message from a person records no source.
    let mut person = steer_body(
        "exp-ws",
        "side",
        "pq-1",
        serde_json::json!({"prompt": "later"}),
    );
    person["spec"]["inboxMode"] = serde_json::json!("QUEUE");
    let (_, queued) = post_json(&app, "/v1/runs", &secret, "pq-1", person).await;
    let person_id = queued["inbox"]["runId"]["urn"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(store.run_origin(&person_id).unwrap(), (None, None));

    // A fire whose window passed.
    approve_gateway_broker(&store, "b1", 2, 0);
    let cred9 = make_grant(&store, LOCAL_USER_ID, "b1", "", "", "QUEUE").0;
    let caller = trigger_caller(&cred9, Some(1));
    let body = steer_body(
        "exp-ws",
        "main",
        "x-1",
        serde_json::json!({"prompt": "late"}),
    );
    let Ok(StartOutcome::Queued { receipt, .. }) =
        start_run(&state, &caller, start_request(body), trigger_opts("x-1")).await
    else {
        panic!("expected a queued fire");
    };
    let fire_id = receipt.run_id.unwrap().urn;

    // Even while the task still runs, the old fire is not kept: EXPIRED, never started.
    vam_orchestrator::api::queue::run_queue_pass(&state).await;
    assert_eq!(store.get_run_state(&fire_id).unwrap(), "EXPIRED");

    // The person's message still starts after the task, and keeps NULL source.
    store.update_run_state(INBOX_RUN, "SUCCEEDED").unwrap();
    store
        .update_observed_state(
            &session,
            vam_orchestrator::session::ObservedState::Terminated,
        )
        .unwrap();
    assert_eq!(
        vam_orchestrator::api::queue::run_queue_pass(&state).await,
        1
    );
    assert_eq!(store.get_run_state(&person_id).unwrap(), "RUNNING");
    assert_eq!(store.run_origin(&person_id).unwrap(), (None, None));
}

// ---- gateway tests --------------------------------------------------------------------------

mod gateway {
    use super::*;
    use vam_orchestrator::gateway;
    use vam_spec::v1::{
        BusyReason, DenyReason, RequestRunTriggerRequest, RequestRunTriggerResponse,
        RevokeRunGrantRequest, RunTriggerOutcome,
    };

    pub struct World {
        pub store: Arc<SqliteWorkflowStore>,
        pub state: AppState,
        pub secret: String,
        pub space: String,
        pub thread: String,
        pub grant: String,
        pub token: String,
        pub session: Option<String>,
        _dir: tempfile::TempDir,
    }

    /// A node with an approved broker `b1`, one grant for `local` on a Space and thread, and (if
    /// `busy`) a live task holding that workspace.
    pub async fn world(on_busy: &str, busy: bool) -> World {
        let dir = isolate_state();
        let (store, secret) = bootstrap_store();
        let state = starter_state(store.clone());
        let app = test_app(state.clone());
        put_test_agent(&app, &secret, INBOX_AGENT, "Test Agent").await;
        release_inbox_agent(&store);
        approve_gateway_broker(&store, "b1", 2, 0);
        let (space, thread, session) = if busy {
            static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let name = format!(
                "gw-ws-{}",
                N.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            );
            let session = live_task(&store, &name, "main", LOCAL_USER_ID);
            let (space, thread) = place_of(&name, "main").unwrap();
            (space, thread.unwrap(), Some(session))
        } else {
            let space = store
                .create_space(LOCAL_USER_ID, Some(INBOX_AGENT), "gw-idle", "run", None)
                .unwrap();
            let thread = store
                .create_thread(LOCAL_USER_ID, &space.space_id, "main", "run")
                .unwrap();
            (space.space_id, thread.thread_id, None)
        };
        let (grant, token) = make_grant(&store, LOCAL_USER_ID, "b1", &space, &thread, on_busy);
        World {
            store,
            state,
            secret,
            space,
            thread,
            grant,
            token,
            session,
            _dir: dir,
        }
    }

    pub fn req(token: &str, key: &str, trigger_ref: &str) -> RequestRunTriggerRequest {
        RequestRunTriggerRequest {
            run_grant: token.to_string(),
            idempotency_key: key.to_string(),
            trigger_ref: trigger_ref.to_string(),
            kind: 1,
            ..Default::default()
        }
    }

    pub async fn fire(w: &World, key: &str) -> RequestRunTriggerResponse {
        gateway::fire(&w.state, "b1", None, req(&w.token, key, "")).await
    }

    pub fn outcome(r: &RequestRunTriggerResponse) -> RunTriggerOutcome {
        RunTriggerOutcome::try_from(r.outcome).unwrap()
    }

    pub fn deny(r: &RequestRunTriggerResponse) -> DenyReason {
        assert_eq!(outcome(r), RunTriggerOutcome::Denied, "{r:?}");
        DenyReason::try_from(r.deny_reason).unwrap()
    }

    pub fn run_of(r: &RequestRunTriggerResponse) -> String {
        r.run_id.clone().expect("a run id").urn
    }

    fn lock() -> std::sync::MutexGuard<'static, ()> {
        PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn a_fire_starts_once_then_queues_then_says_already_queued_and_stamps_the_event() {
        let _p = lock();
        let w = world("QUEUE", false).await;
        let first = fire(&w, "k1").await;
        assert_eq!(outcome(&first), RunTriggerOutcome::Accepted, "{first:?}");
        assert_eq!(
            (first.space_id.as_str(), first.thread_id.as_str()),
            (w.space.as_str(), w.thread.as_str())
        );
        let run = run_of(&first);
        // The same key names the same run; no second run.
        let again = fire(&w, "k1").await;
        assert_eq!(outcome(&again), RunTriggerOutcome::Duplicate);
        assert_eq!(run_of(&again), run);
        // The run is the grant's: source and credential recorded, event stamped, no token in it.
        assert_eq!(
            w.store.run_origin(&run).unwrap(),
            (Some("broker:b1".to_string()), Some(w.grant.clone()))
        );
        let events = w.store.read_outbox_for_run(&run, 0).unwrap();
        let fired = events
            .iter()
            .find(|e| e.event_type == "vam.v1.trigger.fired")
            .expect("trigger.fired");
        assert!(fired.payload.contains(&w.grant) && !fired.payload.contains(&w.token));
        // The workspace is now taken: the next fire waits, a second one says already queued.
        let queued = fire(&w, "k2").await;
        assert_eq!(outcome(&queued), RunTriggerOutcome::Queued, "{queued:?}");
        assert_eq!(w.store.get_run_state(&run_of(&queued)).unwrap(), "PENDING");
        let third = fire(&w, "k3").await;
        assert_eq!(outcome(&third), RunTriggerOutcome::Busy);
        assert_eq!(third.busy_reason, BusyReason::AlreadyQueued as i32);
        // The metric counted each answer, with the broker and the result.
        let metrics = vam_telemetry::VamMetricsRegistry::global();
        assert!(
            metrics
                .get_counter(
                    gateway::METRIC_HSP_TRIGGER_TOTAL,
                    &[("broker_id", "b1"), ("result", "accepted")]
                )
                .unwrap_or(0)
                >= 1
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn a_skip_grant_on_a_busy_workspace_answers_skipped_and_queues_nothing() {
        let _p = lock();
        let w = world("SKIP", true).await;
        let r = fire(&w, "k1").await;
        assert_eq!(outcome(&r), RunTriggerOutcome::Busy, "{r:?}");
        assert_eq!(r.busy_reason, BusyReason::Skipped as i32);
        assert!(w.store.list_queued().unwrap().is_empty());
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn the_socket_and_the_token_decide_who_may_fire() {
        let _p = lock();
        let w = world("QUEUE", false).await;
        // Another broker's socket: the grant belongs to b1.
        approve_gateway_broker(&w.store, "b2", 2, 0);
        let r = gateway::fire(&w.state, "b2", None, req(&w.token, "k", "")).await;
        assert_eq!(deny(&r), DenyReason::WrongBroker);
        // A token whose id is not its row's id.
        let secret = w.token.split_once('.').unwrap().1;
        let forged = format!("vam_rg_rg_notthisgrant.{secret}");
        let r = gateway::fire(&w.state, "b1", None, req(&forged, "k", "")).await;
        assert_eq!(deny(&r), DenyReason::InvalidRequest);
        let r = gateway::fire(&w.state, "b1", None, req("vam_rg_nonsense", "k", "")).await;
        assert_eq!(deny(&r), DenyReason::InvalidRequest);
        // An unspecified or third-party tier may not request runs.
        for tier in [0, 4, 99] {
            assert!(!gateway::tier_may_request_runs(tier));
        }
        for tier in [1, 2, 3] {
            assert!(gateway::tier_may_request_runs(tier));
        }
        // A tool that is no longer approved to hold a grant.
        {
            use vam_spec::v1::{CapabilityManifest, RunRequestPermission, ToolExport};
            w.store
                .upsert_hsp_broker_approval(&CapabilityManifest {
                    broker_id: "b1".to_string(),
                    trust_tier: 2,
                    tools: vec![ToolExport {
                        tool_name: "echo.other".to_string(),
                        ..Default::default()
                    }],
                    run_request_permission: Some(RunRequestPermission {
                        grant_tools: vec!["echo.other".to_string()],
                        loop_guarded_tools: vec!["echo.other".to_string()],
                        surface: "echo".to_string(),
                        ..Default::default()
                    }),
                    ..Default::default()
                })
                .unwrap();
        }
        assert_eq!(deny(&fire(&w, "k").await), DenyReason::ToolNotApproved);
        // A disabled approval.
        approve_gateway_broker(&w.store, "b1", 2, 0);
        w.store.disable_hsp_broker_approval("b1").unwrap();
        assert_eq!(deny(&fire(&w, "k").await), DenyReason::NotPermitted);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn peer_uid_is_enforced_for_any_value_and_null_means_no_check() {
        let _p = lock();
        let w = world("QUEUE", false).await;
        // NULL: nobody is checked (no credentials at all is fine).
        assert_eq!(outcome(&fire(&w, "k0").await), RunTriggerOutcome::Accepted);
        // Root (0) is a real value.
        w.store.set_hsp_broker_gateway("b1", None, Some(0)).unwrap();
        let other = gateway::fire(&w.state, "b1", Some(1000), req(&w.token, "k1", "")).await;
        assert_eq!(deny(&other), DenyReason::NotPermitted);
        let nobody = gateway::fire(&w.state, "b1", None, req(&w.token, "k1", "")).await;
        assert_eq!(deny(&nobody), DenyReason::NotPermitted);
        let root = gateway::fire(&w.state, "b1", Some(0), req(&w.token, "k1", "")).await;
        assert_ne!(outcome(&root), RunTriggerOutcome::Denied, "{root:?}");
        // uid + 1 of a non-root broker.
        w.store
            .set_hsp_broker_gateway("b1", None, Some(4242))
            .unwrap();
        let wrong = gateway::fire(&w.state, "b1", Some(4243), req(&w.token, "k2", "")).await;
        assert_eq!(deny(&wrong), DenyReason::NotPermitted);
        let right = gateway::fire(&w.state, "b1", Some(4242), req(&w.token, "k2", "")).await;
        assert_ne!(outcome(&right), RunTriggerOutcome::Denied, "{right:?}");
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn revoked_expired_owner_disabled_and_demoted_owners_start_nothing() {
        use vam_orchestrator::credentials::GrantInsert;
        let _p = lock();
        let w = world("QUEUE", false).await;
        // Expired.
        let expired = {
            let (rec, token) = w
                .store
                .insert_grant_credential(&GrantInsert {
                    name: "g".into(),
                    owner_user_id: LOCAL_USER_ID.into(),
                    holder: "broker:b1".into(),
                    target_kind: "space".into(),
                    target_id: w.space.clone(),
                    constraints_json: "{}".into(),
                    issued_via: "invoke".into(),
                    source_run_id: None,
                    tool_name: Some("echo.ping".into()),
                    scopes: vec!["runs:start".into()],
                    expires_at: 1,
                })
                .unwrap();
            let _ = rec;
            token
        };
        let r = gateway::fire(&w.state, "b1", None, req(&expired, "k", "")).await;
        assert_eq!(deny(&r), DenyReason::Expired);
        // Revoked.
        w.store
            .revoke_api_key_for_owner(&w.grant, LOCAL_USER_ID)
            .unwrap();
        assert_eq!(deny(&fire(&w, "k").await), DenyReason::Revoked);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn a_deactivated_owner_answers_owner_disabled_and_loses_the_queued_fire() {
        let _p = lock();
        let w = world("QUEUE", true).await;
        let bob = w.store.create_user("bob", "bob").unwrap().user_id;
        // A busy workspace of bob's own, and a grant on it.
        let space = w
            .store
            .create_space(&bob, Some(INBOX_AGENT), "bob-ws", "run", None)
            .unwrap();
        let thread = w
            .store
            .create_thread(&bob, &space.space_id, "main", "run")
            .unwrap();
        let bob_run = "urn:vam:run:local.vam.dev/acme/run-bob-live";
        w.store
            .create_session("sess-bob", None, "vm-bob", &space.space_id, "default")
            .unwrap();
        for st in [
            vam_orchestrator::session::ObservedState::Booting,
            vam_orchestrator::session::ObservedState::Running,
        ] {
            w.store.update_observed_state("sess-bob", st).unwrap();
        }
        let spec = serde_json::json!({
            "workspaceRefUri": format!("volume://{}", space.space_id),
            "threadRef": thread.thread_id, "spaceId": space.space_id, "threadId": thread.thread_id,
            "inputs": {"prompt": "start", "model": "mock-model"},
        });
        w.store
            .persist_run_spec_with_owner(
                bob_run,
                &spec.to_string(),
                Some("sess-bob"),
                INBOX_AGENT,
                Some("bob-live"),
                "User::\"bob\"",
                &bob,
            )
            .unwrap();
        w.store
            .create_run(&ResourceId {
                urn: bob_run.into(),
            })
            .unwrap();
        let (grant, token) = make_grant(
            &w.store,
            &bob,
            "b1",
            &space.space_id,
            &thread.thread_id,
            "QUEUE",
        );
        let queued = gateway::fire(&w.state, "b1", None, req(&token, "k1", "")).await;
        assert_eq!(outcome(&queued), RunTriggerOutcome::Queued, "{queued:?}");
        let queued_run = run_of(&queued);
        assert_eq!(w.store.get_run_state(&queued_run).unwrap(), "PENDING");
        w.store.deactivate_user(&bob).unwrap();
        // The fire ended with its user; it is not left PENDING.
        assert_eq!(w.store.get_run_state(&queued_run).unwrap(), "CANCELLED");
        assert!(w.store.list_queued().unwrap().is_empty());
        let r = gateway::fire(&w.state, "b1", None, req(&token, "k2", "")).await;
        assert_eq!(deny(&r), DenyReason::OwnerDisabled);
        let _ = grant;
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn the_trigger_ref_is_pinned_on_the_first_accepted_fire() {
        let _p = lock();
        let w = world("QUEUE", false).await;
        let first = gateway::fire(&w.state, "b1", None, req(&w.token, "k1", "sch_1")).await;
        assert_eq!(outcome(&first), RunTriggerOutcome::Accepted, "{first:?}");
        let other = gateway::fire(&w.state, "b1", None, req(&w.token, "k2", "sch_2")).await;
        assert_eq!(deny(&other), DenyReason::TriggerRefMismatch);
        let same = gateway::fire(&w.state, "b1", None, req(&w.token, "k3", "sch_1")).await;
        assert_ne!(deny_or_ok(&same), Some(DenyReason::TriggerRefMismatch));
    }

    fn deny_or_ok(r: &RequestRunTriggerResponse) -> Option<DenyReason> {
        (outcome(r) == RunTriggerOutcome::Denied)
            .then(|| DenyReason::try_from(r.deny_reason).unwrap())
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn a_broker_over_its_limit_is_rate_limited_and_a_gone_target_is_denied() {
        let _p = lock();
        let w = world("QUEUE", false).await;
        approve_gateway_broker(&w.store, "b1", 2, 2);
        assert_eq!(outcome(&fire(&w, "k1").await), RunTriggerOutcome::Accepted);
        assert_eq!(outcome(&fire(&w, "k1").await), RunTriggerOutcome::Duplicate);
        let limited = fire(&w, "k1").await;
        assert_eq!(outcome(&limited), RunTriggerOutcome::RateLimited);
        assert!(limited.retry_after_ms > 0);
        // A grant whose Space is gone.
        let (_, token) = make_grant(&w.store, LOCAL_USER_ID, "b1", "sp_gone", "th_gone", "QUEUE");
        approve_gateway_broker(&w.store, "b1", 2, 0);
        let r = gateway::fire(&w.state, "b1", None, req(&token, "k9", "")).await;
        assert_eq!(deny(&r), DenyReason::TargetGone);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn revoke_run_grant_proves_possession_discards_the_queued_fire_and_is_idempotent() {
        let _p = lock();
        let w = world("QUEUE", true).await;
        let queued = fire(&w, "k1").await;
        assert_eq!(outcome(&queued), RunTriggerOutcome::Queued, "{queued:?}");
        let run = run_of(&queued);
        // Another broker's socket cannot revoke it.
        approve_gateway_broker(&w.store, "b2", 2, 0);
        let refused = gateway::revoke(
            &w.state,
            "b2",
            None,
            RevokeRunGrantRequest {
                run_grant: w.token.clone(),
            },
        );
        assert!(refused.error.is_some());
        assert_eq!(w.store.get_run_state(&run).unwrap(), "PENDING");
        // The holder can.
        let ok = gateway::revoke(
            &w.state,
            "b1",
            None,
            RevokeRunGrantRequest {
                run_grant: w.token.clone(),
            },
        );
        assert!(ok.error.is_none(), "{ok:?}");
        assert_eq!(w.store.get_run_state(&run).unwrap(), "CANCELLED");
        assert_eq!(deny(&fire(&w, "k2").await), DenyReason::Revoked);
        // Again: still a success.
        let again = gateway::revoke(
            &w.state,
            "b1",
            None,
            RevokeRunGrantRequest {
                run_grant: w.token.clone(),
            },
        );
        assert!(again.error.is_none(), "{again:?}");
        // A token that is not a grant proves nothing.
        let junk = gateway::revoke(
            &w.state,
            "b1",
            None,
            RevokeRunGrantRequest {
                run_grant: "vam_rg_x".into(),
            },
        );
        assert!(junk.error.is_some());
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn a_queued_fire_does_not_start_after_its_grant_is_revoked_or_the_user_may_not_run() {
        let _p = lock();
        let w = world("QUEUE", true).await;
        let queued = fire(&w, "k1").await;
        let run = run_of(&queued);
        // Revoked behind the gateway's back (no discard): the starter refuses it.
        w.store
            .revoke_api_key_for_owner(&w.grant, LOCAL_USER_ID)
            .unwrap();
        w.store.update_run_state(INBOX_RUN, "SUCCEEDED").unwrap();
        w.store
            .update_observed_state(
                w.session.as_deref().unwrap(),
                vam_orchestrator::session::ObservedState::Terminated,
            )
            .unwrap();
        assert_eq!(
            vam_orchestrator::api::queue::run_queue_pass(&w.state).await,
            0
        );
        assert_eq!(w.store.get_run_state(&run).unwrap(), "FAILED");
        let result = w
            .store
            .get_run_result_json(&run)
            .unwrap()
            .unwrap_or_default();
        assert!(
            result.contains("the schedule's grant was revoked"),
            "{result}"
        );
    }

    #[test]
    fn the_owner_s_current_scopes_decide_a_queued_fire_too() {
        let store = SqliteWorkflowStore::in_memory().unwrap();
        approve_gateway_broker(&store, "b1", 2, 0);
        let grant = make_grant(&store, LOCAL_USER_ID, "b1", "sp_x", "th_x", "QUEUE").0;
        let caller = trigger_caller(&grant, None);
        let real = |r| vam_orchestrator::keys::default_scopes_for_role(r);
        assert_eq!(gateway::recheck_queued_fire(&store, &caller, &real), None);
        // A role that no longer holds runs:write: the fire is refused.
        let none = |_r| Vec::<String>::new();
        let why = gateway::recheck_queued_fire(&store, &caller, &none).expect("refused");
        assert!(why.contains("no longer start runs"), "{why}");
        assert!(!gateway::scopes_allow_run(&["sessions:write".to_string()]));
        assert!(gateway::scopes_allow_run(&["runs:write".to_string()]));
        // A disabled broker stops it too.
        store.disable_hsp_broker_approval("b1").unwrap();
        assert!(gateway::recheck_queued_fire(&store, &caller, &real)
            .unwrap()
            .contains("disabled"));
    }

    #[cfg(unix)]
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn the_socket_opens_with_the_approval_closes_with_it_and_checks_the_peer() {
        use std::os::unix::fs::PermissionsExt;
        use vam_spec::v1::hosted_trigger_gateway_client::HostedTriggerGatewayClient;
        let _p = lock();
        let w = world("QUEUE", false).await;
        let dir = tempfile::Builder::new()
            .prefix("gw")
            .tempdir_in("/tmp")
            .unwrap();
        let gw_dir = dir.path().join("gw");
        w.store.gateways.set_dir(gw_dir.clone());
        let path = gateway::sync_broker(&w.state, "b1")
            .await
            .unwrap()
            .expect("open");
        assert!(path.as_os_str().len() <= 107);
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(&gw_dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            w.store.get_hsp_broker_gateway("b1").unwrap().0.as_deref(),
            path.to_str()
        );
        let connect = |p: std::path::PathBuf| async move {
            let ch = tonic::transport::Endpoint::try_from("http://127.0.0.1")
                .unwrap()
                .connect_with_connector(tower::service_fn(move |_u: http::Uri| {
                    let p = p.clone();
                    async move { tokio::net::UnixStream::connect(p).await }
                }))
                .await
                .unwrap();
            HostedTriggerGatewayClient::new(ch)
        };
        let mut client = connect(path.clone()).await;
        let r = client
            .request_run_trigger(req(&w.token, "k1", ""))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(outcome(&r), RunTriggerOutcome::Accepted, "{r:?}");
        // The caller's uid is checked against peer_uid, for any value (this process's own uid
        // passes, the next uid does not).
        let me = unsafe { libc::geteuid() } as i64;
        w.store
            .set_hsp_broker_gateway("b1", Some(&path), Some(me + 1))
            .unwrap();
        let r = client
            .request_run_trigger(req(&w.token, "k2", ""))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(deny(&r), DenyReason::NotPermitted);
        w.store
            .set_hsp_broker_gateway("b1", Some(&path), Some(me))
            .unwrap();
        let r = client
            .request_run_trigger(req(&w.token, "k2", ""))
            .await
            .unwrap()
            .into_inner();
        assert_ne!(deny_or_ok(&r), Some(DenyReason::NotPermitted), "{r:?}");
        // Disabling closes it.
        w.store.disable_hsp_broker_approval("b1").unwrap();
        gateway::close_broker(&w.state, "b1").unwrap();
        assert!(!path.exists());
        assert_eq!(w.store.get_hsp_broker_gateway("b1").unwrap().0, None);
        // A directory that makes the path too long is refused with a message.
        let long = std::path::PathBuf::from(format!("/tmp/{}", "d".repeat(120)));
        assert!(gateway::socket_path(&long, "b1").is_err());
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn delete_hosted_broker_disables_the_approval_and_needs_the_operator() {
        let _p = lock();
        let w = world("QUEUE", false).await;
        let app = test_app(w.state.clone());
        let call = |secret: String, id: &'static str| {
            let app = app.clone();
            async move {
                let response = tower::ServiceExt::oneshot(
                    app,
                    axum::http::Request::builder()
                        .method("DELETE")
                        .uri(format!("/v1/hosted/brokers/{id}"))
                        .header("authorization", format!("Bearer {secret}"))
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
                response.status()
            }
        };
        assert_eq!(
            call(w.secret.clone(), "missing").await,
            StatusCode::NOT_FOUND
        );
        assert_eq!(call(w.secret.clone(), "b1").await, StatusCode::OK);
        assert!(
            !w.store
                .get_hsp_broker_approval("b1")
                .unwrap()
                .unwrap()
                .enabled
        );
        // Idempotent.
        assert_eq!(call(w.secret.clone(), "b1").await, StatusCode::OK);
        // A member holds no brokers:admin.
        let member = {
            let id = w.store.create_user("mem", "mem").unwrap().user_id;
            let secret = mint_api_key_secret();
            w.store
                .create_api_key_owned(
                    "mem-key",
                    "mem",
                    &secret,
                    "User::\"mem\"",
                    &["runs:write".to_string()],
                    &id,
                )
                .unwrap();
            secret
        };
        assert_eq!(call(member, "b1").await, StatusCode::FORBIDDEN);
    }

    #[test]
    fn peer_uid_absent_is_none_and_zero_is_a_value() {
        let parse = |json: &str| -> vam_spec::v1::PutHostedBrokerRequest {
            vam_spec::encoding::json_to_proto("vam.v1.PutHostedBrokerRequest", json).unwrap()
        };
        assert_eq!(parse(r#"{"unixPath":"/x"}"#).peer_uid, None);
        assert_eq!(
            parse(r#"{"unixPath":"/x","peerUid":"0"}"#).peer_uid,
            Some(0)
        );
        assert_eq!(
            parse(r#"{"unixPath":"/x","peerUid":1000}"#).peer_uid,
            Some(1000)
        );
    }
}

async fn outcomes_as(
    app: &axum::Router,
    secret: &str,
    query: &str,
    urn: &str,
) -> (StatusCode, serde_json::Value) {
    let resp = app
        .clone()
        .oneshot(bearer_get(
            &format!("/v1/agents/{urn}/outcomes{query}"),
            secret,
        ))
        .await
        .unwrap();
    let status = resp.status();
    let body = response_body(resp).await;
    let json = serde_json::from_str(&body).unwrap_or(serde_json::Value::Null);
    (status, json)
}

#[tokio::test]
async fn agent_outcomes_follow_the_versions_auth_and_hide_a_foreign_draft() {
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let alice = mint_user_key(&store, "alice", UserRole::Publisher, PUBLISHER_SCOPES);
    let bob = mint_user_key(
        &store,
        "bob",
        UserRole::Member,
        &["catalog:write", "sessions:write"],
    );
    let app = test_app(app_state_http_only(store));
    let urn = "urn:vam:agent:local.vam.dev/acme/outcomes-http";

    assert_eq!(put_agent_as(&app, &alice, urn, "v1").await, StatusCode::OK);
    let (status, body) =
        release_with_note_as(&app, &alice, "agents", urn, (1, 0, 0), "out-100", "first").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(put_agent_as(&app, &alice, urn, "v2").await, StatusCode::OK);

    // A released version with no runs: zeros, no ratios, no ids.
    let (status, one) = outcomes_as(&app, &bob, "?version=1.0.0&k=2", urn).await;
    assert_eq!(status, StatusCode::OK, "{one}");
    assert_eq!(one["runs"], 0);
    assert_eq!(one["successes"], 0);
    assert_eq!(one["passK"]["k"], 2);
    assert!(one["passK"]["note"].is_string(), "{one}");
    assert!(one.get("successRate").is_none());

    // The draft: its owner reads it; another member gets a 404, like `versions`.
    let (status, draft) = outcomes_as(&app, &alice, "?version=draft", urn).await;
    assert_eq!(status, StatusCode::OK, "{draft}");
    assert_eq!(draft["version"], "draft");
    let (status, _) = outcomes_as(&app, &bob, "?version=draft", urn).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // The list shows only versions with receipts (none yet).
    let (status, list) = outcomes_as(&app, &bob, "", urn).await;
    assert_eq!(status, StatusCode::OK, "{list}");
    assert!(list["versions"].as_array().unwrap().is_empty(), "{list}");

    // Unknown version and bad parameters.
    let (status, _) = outcomes_as(&app, &alice, "?version=9.9.9", urn).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = outcomes_as(&app, &alice, "?version=1.0.0&k=0", urn).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = outcomes_as(&app, &alice, "?version=nope", urn).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn agent_outcomes_listing_is_404_for_a_foreign_draft_only_or_unknown_agent() {
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let alice = mint_user_key(&store, "alice", UserRole::Publisher, PUBLISHER_SCOPES);
    let bob = mint_user_key(
        &store,
        "bob",
        UserRole::Member,
        &["catalog:write", "sessions:write"],
    );
    let app = test_app(app_state_http_only(store));
    let urn = "urn:vam:agent:local.vam.dev/acme/outcomes-draft-only";

    assert_eq!(put_agent_as(&app, &alice, urn, "v1").await, StatusCode::OK);
    let (status, _) = outcomes_as(&app, &bob, "", urn).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = outcomes_as(&app, &alice, "", urn).await;
    assert_eq!(status, StatusCode::OK);

    let (status, _) = outcomes_as(&app, &bob, "", "urn:vam:agent:local.vam.dev/acme/nope").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn agent_outcomes_diff_names_the_base_and_is_404_for_a_foreign_agent() {
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let alice = mint_user_key(&store, "alice", UserRole::Publisher, PUBLISHER_SCOPES);
    let bob = mint_user_key(
        &store,
        "bob",
        UserRole::Member,
        &["catalog:write", "sessions:write"],
    );
    let app = test_app(app_state_http_only(store));
    let urn = "urn:vam:agent:local.vam.dev/acme/outcomes-diff";

    // A draft-only agent: invisible to bob; a draft diff for alice has no base.
    assert_eq!(put_agent_as(&app, &alice, urn, "v1").await, StatusCode::OK);
    let (status, _) = outcomes_as(&app, &bob, "/diff?version=draft", urn).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, d) = outcomes_as(&app, &alice, "/diff?version=draft", urn).await;
    assert_eq!(status, StatusCode::OK, "{d}");
    assert_eq!(d["candidate"]["version"], "draft");
    assert!(d.get("base").is_none(), "{d}");

    for (v, key) in [((1, 0, 0), "d-100"), ((1, 1, 0), "d-110")] {
        let (status, body) = release_with_note_as(&app, &alice, "agents", urn, v, key, "n").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(put_agent_as(&app, &alice, urn, key).await, StatusCode::OK);
    }
    let (status, d) = outcomes_as(&app, &bob, "/diff?version=1.1.0", urn).await;
    assert_eq!(status, StatusCode::OK, "{d}");
    assert_eq!(d["base"]["version"], "1.0.0");
    assert_eq!(d["candidate"]["version"], "1.1.0");
    let (_, d) = outcomes_as(&app, &alice, "/diff?version=draft", urn).await;
    assert_eq!(d["base"]["version"], "1.1.0");
    let (_, d) = outcomes_as(&app, &bob, "/diff?version=1.0.0", urn).await;
    assert!(d.get("base").is_none(), "{d}");

    let (status, _) = outcomes_as(&app, &bob, "/diff?version=draft", urn).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = outcomes_as(&app, &alice, "/diff", urn).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let foreign = "urn:vam:agent:local.vam.dev/acme/nope";
    let (status, _) = outcomes_as(&app, &bob, "/diff?version=1.0.0", foreign).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// POST /v1/me/password: self-service password change (user-identity UI-1.2)
// ---------------------------------------------------------------------------

fn json_request(method: &str, uri: &str, body: serde_json::Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

async fn login_response(
    app: &axum::Router,
    username: &str,
    password: &str,
) -> axum::response::Response {
    app.clone()
        .oneshot(json_request(
            "POST",
            "/v1/login",
            serde_json::json!({ "username": username, "password": password }),
        ))
        .await
        .unwrap()
}

/// Logs in and returns `(access token, refresh token)`.
async fn login_tokens(app: &axum::Router, username: &str, password: &str) -> (String, String) {
    let response = login_response(app, username, password).await;
    assert_eq!(response.status(), StatusCode::OK);
    let v: serde_json::Value = serde_json::from_str(&response_body(response).await).unwrap();
    (
        v["token"].as_str().expect("token").to_string(),
        v["refreshToken"]
            .as_str()
            .expect("refreshToken")
            .to_string(),
    )
}

/// `POST /v1/me/password` with a bearer, an optional Idempotency-Key and a raw body.
async fn post_my_password(
    app: &axum::Router,
    bearer: Option<&str>,
    idempotency_key: Option<&str>,
    body: String,
) -> axum::response::Response {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/v1/me/password")
        .header("content-type", "application/json");
    if let Some(b) = bearer {
        builder = builder.header("authorization", format!("Bearer {b}"));
    }
    if let Some(k) = idempotency_key {
        builder = builder.header("Idempotency-Key", k);
    }
    app.clone()
        .oneshot(builder.body(Body::from(body)).unwrap())
        .await
        .unwrap()
}

fn passwords(current: &str, new: &str) -> String {
    serde_json::json!({ "currentPassword": current, "newPassword": new }).to_string()
}

async fn assert_error(
    response: axum::response::Response,
    status: StatusCode,
    code: &str,
    message_contains: &str,
) {
    assert_eq!(response.status(), status);
    let body = response_body(response).await;
    let v: serde_json::Value = serde_json::from_str(&body).expect("error body is json");
    assert_eq!(v["code"], code, "{body}");
    let message = v["message"].as_str().unwrap_or_default();
    assert!(message.contains(message_contains), "{body}");
}

async fn assert_login_ok(app: &axum::Router, username: &str, password: &str) {
    let response = login_response(app, username, password).await;
    assert_eq!(response.status(), StatusCode::OK, "login as {username}");
}

async fn assert_login_refused(app: &axum::Router, username: &str, password: &str) {
    let response = login_response(app, username, password).await;
    assert_eq!(
        response.status(),
        StatusCode::UNAUTHORIZED,
        "login as {username}"
    );
}

async fn assert_me_status(app: &axum::Router, bearer: &str, status: StatusCode) {
    let response = app
        .clone()
        .oneshot(bearer_get("/v1/me", bearer))
        .await
        .unwrap();
    assert_eq!(response.status(), status);
}

/// A member with a key and a password.
fn member_with_password(store: &SqliteWorkflowStore, user_id: &str, password: &str) -> String {
    let key = mint_user_key(store, user_id, UserRole::Member, MEMBER_SCOPES);
    store
        .reset_user_password(user_id, password)
        .expect("seed password");
    key
}

#[tokio::test]
async fn change_my_password_unauthenticated_is_401() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let app = test_app(app_state_http_only(store));

    let none = post_my_password(&app, None, Some("idem-1"), passwords("a", "b")).await;
    assert_eq!(none.status(), StatusCode::UNAUTHORIZED);
    let bogus = post_my_password(
        &app,
        Some("vam_st_not-a-real-token"),
        Some("idem-2"),
        passwords("a", "b"),
    )
    .await;
    assert_eq!(bogus.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn change_my_password_member_api_key_is_403_and_changes_nothing() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let key = member_with_password(&store, "alice", "old-alice-secret");
    let app = test_app(app_state_http_only(store));

    let response = post_my_password(
        &app,
        Some(&key),
        Some("idem-key"),
        passwords("old-alice-secret", "new-alice-secret"),
    )
    .await;
    assert_error(
        response,
        StatusCode::FORBIDDEN,
        "PermissionDenied",
        "changing a password needs a password session (vam login); API keys cannot change a password",
    )
    .await;
    assert_login_ok(&app, "alice", "old-alice-secret").await;
    assert_login_refused(&app, "alice", "new-alice-secret").await;

    // The credential kind is judged first: no Idempotency-Key is still the 403.
    let no_idem = post_my_password(&app, Some(&key), None, passwords("a", "b")).await;
    assert_eq!(no_idem.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn change_my_password_bootstrap_key_is_403() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let secret = mint_api_key_secret();
    let scopes: Vec<String> = BOOTSTRAP_SCOPES.iter().map(|s| s.to_string()).collect();
    store
        .create_api_key(
            BOOTSTRAP_KEY_ID,
            "bootstrap",
            &secret,
            BOOTSTRAP_PRINCIPAL,
            &scopes,
        )
        .unwrap();
    store
        .reset_user_password(OPERATOR_USER_ID, "operator-secret")
        .expect("operator password");
    let app = test_app(app_state_http_only(store.clone()));

    let response = post_my_password(
        &app,
        Some(&secret),
        Some("idem-master"),
        passwords("operator-secret", "operator-new"),
    )
    .await;
    assert_error(
        response,
        StatusCode::FORBIDDEN,
        "PermissionDenied",
        "password session",
    )
    .await;
    assert!(store
        .verify_user_password(OPERATOR_USER_ID, "operator-secret")
        .unwrap());
}

#[tokio::test]
async fn change_my_password_wrong_current_is_403_never_401_and_changes_nothing() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let key = member_with_password(&store, "alice", "old-alice-secret");
    let app = test_app(app_state_http_only(store));
    let (token, refresh) = login_tokens(&app, "alice", "old-alice-secret").await;

    let response = post_my_password(
        &app,
        Some(&token),
        Some("idem-wrong"),
        passwords("not-the-password", "new-alice-secret"),
    )
    .await;
    assert_error(
        response,
        StatusCode::FORBIDDEN,
        "PermissionDenied",
        "current password is incorrect",
    )
    .await;

    // Nothing changed: the old password signs in, the new one does not, the session lives.
    assert_login_ok(&app, "alice", "old-alice-secret").await;
    assert_login_refused(&app, "alice", "new-alice-secret").await;
    assert_me_status(&app, &token, StatusCode::OK).await;
    assert_me_status(&app, &key, StatusCode::OK).await;
    let refreshed = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/v1/login/refresh",
            serde_json::json!({ "refreshToken": refresh }),
        ))
        .await
        .unwrap();
    assert_eq!(refreshed.status(), StatusCode::OK);
}

#[tokio::test]
async fn change_my_password_five_wrong_tries_then_429_even_with_the_right_password() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let _ = member_with_password(&store, "alice", "old-alice-secret");
    let _ = member_with_password(&store, "bob", "old-bob-secret");
    let app = test_app(app_state_http_only(store));
    let (alice, _) = login_tokens(&app, "alice", "old-alice-secret").await;
    let (bob, _) = login_tokens(&app, "bob", "old-bob-secret").await;

    for i in 0..5 {
        let response = post_my_password(
            &app,
            Some(&alice),
            Some(&format!("idem-wrong-{i}")),
            passwords("wrong", "new-alice-secret"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "try {i}");
    }

    let sixth = post_my_password(
        &app,
        Some(&alice),
        Some("idem-sixth"),
        passwords("wrong", "new-alice-secret"),
    )
    .await;
    assert_eq!(sixth.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        sixth
            .headers()
            .get("X-VAM-Rate-Limited-Reason")
            .and_then(|v| v.to_str().ok()),
        Some("password-attempt-rate")
    );
    let retry_after: u64 = sixth
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .expect("Retry-After header");
    assert!((1..=60).contains(&retry_after), "retry-after {retry_after}");
    let body = response_body(sixth).await;
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["code"], "RESOURCE_EXHAUSTED");
    assert_eq!(v["retryable"], true);
    assert_eq!(
        v["message"],
        format!("too many wrong password attempts; retry after {retry_after}s")
    );

    // The right password is not even verified while the bucket is empty.
    let right = post_my_password(
        &app,
        Some(&alice),
        Some("idem-right"),
        passwords("old-alice-secret", "new-alice-secret"),
    )
    .await;
    assert_eq!(right.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        right
            .headers()
            .get("X-VAM-Rate-Limited-Reason")
            .and_then(|v| v.to_str().ok()),
        Some("password-attempt-rate")
    );
    assert_login_ok(&app, "alice", "old-alice-secret").await;

    // The limit is per user: bob is untouched.
    let bobs = post_my_password(
        &app,
        Some(&bob),
        Some("idem-bob"),
        passwords("old-bob-secret", "new-bob-secret"),
    )
    .await;
    assert_eq!(bobs.status(), StatusCode::OK);
}

#[tokio::test]
async fn change_my_password_invalid_input_costs_no_attempt() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let _ = member_with_password(&store, "alice", "old-alice-secret");
    let app = test_app(app_state_http_only(store));
    let (token, _) = login_tokens(&app, "alice", "old-alice-secret").await;

    for i in 0..10 {
        let response = post_my_password(
            &app,
            Some(&token),
            Some(&format!("idem-empty-{i}")),
            passwords("", "new-alice-secret"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let response = post_my_password(
            &app,
            Some(&token),
            Some(&format!("idem-empty-new-{i}")),
            passwords("old-alice-secret", ""),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
    // Still a normal wrong-password answer, not 429, and the right password still works.
    let wrong = post_my_password(
        &app,
        Some(&token),
        Some("idem-wrong"),
        passwords("wrong", "new-alice-secret"),
    )
    .await;
    assert_eq!(wrong.status(), StatusCode::FORBIDDEN);
    let right = post_my_password(
        &app,
        Some(&token),
        Some("idem-right"),
        passwords("old-alice-secret", "new-alice-secret"),
    )
    .await;
    assert_eq!(right.status(), StatusCode::OK);
}

#[tokio::test]
async fn change_my_password_same_password_is_400_and_keeps_the_session() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let _ = member_with_password(&store, "alice", "old-alice-secret");
    let app = test_app(app_state_http_only(store));
    let (token, _) = login_tokens(&app, "alice", "old-alice-secret").await;

    let response = post_my_password(
        &app,
        Some(&token),
        Some("idem-same"),
        passwords("old-alice-secret", "old-alice-secret"),
    )
    .await;
    assert_error(
        response,
        StatusCode::BAD_REQUEST,
        "InvalidArgument",
        "new password must differ from the current password",
    )
    .await;
    assert_me_status(&app, &token, StatusCode::OK).await;
    assert_login_ok(&app, "alice", "old-alice-secret").await;
}

#[tokio::test]
async fn change_my_password_validates_inputs_with_400() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let _ = member_with_password(&store, "alice", "old-alice-secret");
    let app = test_app(app_state_http_only(store));
    let (token, _) = login_tokens(&app, "alice", "old-alice-secret").await;

    let empty_new = post_my_password(
        &app,
        Some(&token),
        Some("idem-1"),
        passwords("old-alice-secret", ""),
    )
    .await;
    assert_error(
        empty_new,
        StatusCode::BAD_REQUEST,
        "InvalidArgument",
        "password must be non-empty",
    )
    .await;

    let too_long = post_my_password(
        &app,
        Some(&token),
        Some("idem-2"),
        passwords("old-alice-secret", &"x".repeat(1025)),
    )
    .await;
    assert_error(
        too_long,
        StatusCode::BAD_REQUEST,
        "InvalidArgument",
        "password exceeds 1024 bytes",
    )
    .await;

    let empty_current = post_my_password(
        &app,
        Some(&token),
        Some("idem-3"),
        passwords("", "new-alice-secret"),
    )
    .await;
    assert_error(
        empty_current,
        StatusCode::BAD_REQUEST,
        "InvalidArgument",
        "currentPassword is required",
    )
    .await;

    let missing_current = post_my_password(
        &app,
        Some(&token),
        Some("idem-4"),
        serde_json::json!({ "newPassword": "new-alice-secret" }).to_string(),
    )
    .await;
    assert_error(
        missing_current,
        StatusCode::BAD_REQUEST,
        "InvalidArgument",
        "currentPassword is required",
    )
    .await;

    let malformed =
        post_my_password(&app, Some(&token), Some("idem-5"), "not json".to_string()).await;
    assert_eq!(malformed.status(), StatusCode::BAD_REQUEST);

    assert_me_status(&app, &token, StatusCode::OK).await;
    assert_login_ok(&app, "alice", "old-alice-secret").await;
}

#[tokio::test]
async fn change_my_password_needs_an_idempotency_key() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let _ = member_with_password(&store, "alice", "old-alice-secret");
    let app = test_app(app_state_http_only(store));
    let (token, _) = login_tokens(&app, "alice", "old-alice-secret").await;

    let response = post_my_password(
        &app,
        Some(&token),
        None,
        passwords("old-alice-secret", "new-alice-secret"),
    )
    .await;
    assert_error(
        response,
        StatusCode::BAD_REQUEST,
        "InvalidArgument",
        "Idempotency-Key",
    )
    .await;
    assert_me_status(&app, &token, StatusCode::OK).await;
    assert_login_ok(&app, "alice", "old-alice-secret").await;
}

#[tokio::test]
async fn change_my_password_success_signs_out_every_session_and_keeps_api_keys() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let key = member_with_password(&store, "alice", "old-alice-secret");
    let app = test_app(app_state_http_only(store.clone()));
    let (calling, calling_refresh) = login_tokens(&app, "alice", "old-alice-secret").await;
    let (other, other_refresh) = login_tokens(&app, "alice", "old-alice-secret").await;
    assert_ne!(calling, other);
    assert_me_status(&app, &calling, StatusCode::OK).await;
    assert_me_status(&app, &other, StatusCode::OK).await;

    let response = post_my_password(
        &app,
        Some(&calling),
        Some("idem-change"),
        passwords("old-alice-secret", "new-alice-secret"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = response_body(response).await;
    assert!(!body.contains("old-alice-secret") && !body.contains("new-alice-secret"));
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["reloginRequired"], true, "{body}");
    assert_eq!(v["user"]["userId"], "alice", "{body}");
    assert_eq!(v["user"]["username"], "alice", "{body}");
    assert_eq!(v["user"]["hasPassword"], true, "{body}");
    // proto3 JSON writes int64 as a string.
    let updated: i64 = v["passwordUpdatedAt"]
        .as_str()
        .expect("passwordUpdatedAt is a string")
        .parse()
        .expect("passwordUpdatedAt is an integer");
    assert!(updated > 0);
    assert_eq!(
        Some(updated),
        store.get_user("alice").unwrap().password_updated_at
    );

    // Every session is gone, the caller's and the other one, and so are the refresh tokens.
    assert_me_status(&app, &calling, StatusCode::UNAUTHORIZED).await;
    assert_me_status(&app, &other, StatusCode::UNAUTHORIZED).await;
    for refresh in [calling_refresh, other_refresh] {
        let refreshed = app
            .clone()
            .oneshot(json_request(
                "POST",
                "/v1/login/refresh",
                serde_json::json!({ "refreshToken": refresh }),
            ))
            .await
            .unwrap();
        assert_eq!(refreshed.status(), StatusCode::UNAUTHORIZED);
    }
    // The API key is a separate credential and still answers.
    assert_me_status(&app, &key, StatusCode::OK).await;

    assert_login_refused(&app, "alice", "old-alice-secret").await;
    assert_login_ok(&app, "alice", "new-alice-secret").await;
}

#[tokio::test]
async fn change_my_password_operator_changes_their_own_password() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    // What `vamd --reset-password` does for the operator row.
    vam_orchestrator::password::apply_admin_password_reset(&store, "operator", "operator-old")
        .expect("operator password");
    let app = test_app(app_state_http_only(store.clone()));
    let (token, _) = login_tokens(&app, "operator", "operator-old").await;

    let response = post_my_password(
        &app,
        Some(&token),
        Some("idem-operator"),
        passwords("operator-old", "operator-new"),
    )
    .await;
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "{}",
        response_body(response).await
    );
    assert_me_status(&app, &token, StatusCode::UNAUTHORIZED).await;
    assert_login_refused(&app, "operator", "operator-old").await;
    assert_login_ok(&app, "operator", "operator-new").await;
}

#[tokio::test]
async fn change_my_password_only_touches_the_callers_own_password() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let _ = member_with_password(&store, "alice", "old-alice-secret");
    let _ = member_with_password(&store, "bob", "old-bob-secret");
    let app = test_app(app_state_http_only(store));
    let (alice, _) = login_tokens(&app, "alice", "old-alice-secret").await;
    let (bob, _) = login_tokens(&app, "bob", "old-bob-secret").await;

    let response = post_my_password(
        &app,
        Some(&alice),
        Some("idem-alice"),
        passwords("old-alice-secret", "new-alice-secret"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_me_status(&app, &bob, StatusCode::OK).await;
    assert_login_ok(&app, "bob", "old-bob-secret").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn change_my_password_concurrent_wrong_guesses_get_at_most_five_tries() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let _ = member_with_password(&store, "alice", "old-alice-secret");
    let app = test_app(app_state_http_only(store.clone()));
    let (token, _) = login_tokens(&app, "alice", "old-alice-secret").await;

    // A stolen session guessing in parallel: each request holds argon2 for a while, so a
    // check-then-charge limiter would let far more than five through.
    let handles: Vec<_> = (0..16)
        .map(|i| {
            let app = app.clone();
            let token = token.clone();
            let request = Request::builder()
                .method("POST")
                .uri("/v1/me/password")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {token}"))
                .header("Idempotency-Key", format!("idem-race-{i}"))
                .body(Body::from(passwords(
                    &format!("guess-{i}"),
                    "new-alice-secret",
                )))
                .unwrap();
            // The router is moved into the task, so the requests run on separate workers.
            tokio::spawn(async move { app.oneshot(request).await.unwrap().status() })
        })
        .collect();
    let mut forbidden = 0;
    let mut limited = 0;
    for handle in handles {
        match handle.await.expect("request task") {
            StatusCode::FORBIDDEN => forbidden += 1,
            StatusCode::TOO_MANY_REQUESTS => limited += 1,
            other => panic!("unexpected status {other}"),
        }
    }
    assert_eq!(forbidden, 5, "exactly the burst was verified");
    assert_eq!(limited, 11, "the rest were refused without a verify");
    assert_login_ok(&app, "alice", "old-alice-secret").await;
}

#[tokio::test]
async fn change_my_password_refused_same_password_costs_no_attempt() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let _ = member_with_password(&store, "alice", "old-alice-secret");
    let app = test_app(app_state_http_only(store));
    let (token, _) = login_tokens(&app, "alice", "old-alice-secret").await;

    // The same-password 400 is not a wrong guess: ten of them leave the full burst.
    for i in 0..10 {
        let response = post_my_password(
            &app,
            Some(&token),
            Some(&format!("idem-same-{i}")),
            passwords("old-alice-secret", "old-alice-secret"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "try {i}");
    }
    for i in 0..5 {
        let response = post_my_password(
            &app,
            Some(&token),
            Some(&format!("idem-wrong-{i}")),
            passwords("wrong", "new-alice-secret"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "wrong try {i}");
    }
    let sixth = post_my_password(
        &app,
        Some(&token),
        Some("idem-wrong-6"),
        passwords("wrong", "new-alice-secret"),
    )
    .await;
    assert_eq!(sixth.status(), StatusCode::TOO_MANY_REQUESTS);
}

/// Store with a partitioned audit ledger installed, and the identity partition key.
fn store_with_identity_ledger() -> (
    Arc<SqliteWorkflowStore>,
    Arc<vam_telemetry::PartitionedAuditLedger>,
    String,
) {
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let ledger = Arc::new(vam_telemetry::PartitionedAuditLedger::new());
    store.set_audit_ledger(ledger.clone());
    let partition = vam_telemetry::tenant::TenantContext::partition_key(
        vam_telemetry::current_tenant_id().as_str(),
        "vam_audit_identity",
    );
    (store, ledger, partition)
}

#[tokio::test]
async fn change_my_password_success_writes_one_identity_audit_record_without_secrets() {
    use sha2::{Digest, Sha256};
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let (store, ledger, partition) = store_with_identity_ledger();
    let _ = member_with_password(&store, "alice", "old-alice-secret");
    let app = test_app(app_state_http_only(store.clone()));
    let (token, _) = login_tokens(&app, "alice", "old-alice-secret").await;
    assert!(ledger.read_partition(&partition).is_empty());

    let response = post_my_password(
        &app,
        Some(&token),
        Some("idem-audit"),
        passwords("old-alice-secret", "new-alice-secret"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let updated = store
        .get_user("alice")
        .unwrap()
        .password_updated_at
        .expect("password_updated_at");

    let records = ledger.read_partition(&partition);
    assert_eq!(records.len(), 1, "{records:?}");
    let record = &records[0];
    assert_eq!(record.event_type, "vam.v1.user.password_changed");
    assert_eq!(record.user_id.as_deref(), Some("alice"));

    // The ledger keeps the payload digest: rebuild the exact payload and match it.
    let payload = format!(
        r#"{{"userId":"alice","username":"alice","role":"member","actor":"self","credential":"session","passwordUpdatedAt":{updated}}}"#
    );
    assert_eq!(
        record.payload_digest,
        format!("{:x}", Sha256::digest(payload.as_bytes()))
    );
    for secret in ["old-alice-secret", "new-alice-secret", token.as_str()] {
        assert!(!payload.contains(secret));
    }
}

#[tokio::test]
async fn change_my_password_refusals_write_no_identity_audit_record() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let (store, ledger, partition) = store_with_identity_ledger();
    let key = member_with_password(&store, "alice", "old-alice-secret");
    let app = test_app(app_state_http_only(store));
    let (token, _) = login_tokens(&app, "alice", "old-alice-secret").await;

    let wrong = post_my_password(
        &app,
        Some(&token),
        Some("idem-a1"),
        passwords("not-the-password", "new-alice-secret"),
    )
    .await;
    assert_eq!(wrong.status(), StatusCode::FORBIDDEN);
    let api_key = post_my_password(
        &app,
        Some(&key),
        Some("idem-a2"),
        passwords("old-alice-secret", "new-alice-secret"),
    )
    .await;
    assert_eq!(api_key.status(), StatusCode::FORBIDDEN);
    let same = post_my_password(
        &app,
        Some(&token),
        Some("idem-a3"),
        passwords("old-alice-secret", "old-alice-secret"),
    )
    .await;
    assert_eq!(same.status(), StatusCode::BAD_REQUEST);

    assert!(ledger.read_partition(&partition).is_empty());
}

// ---- Phase 2 (user-identity UI-2.2 to UI-2.4): generated ids, login names, resolution ----

async fn identity_call(
    app: &axum::Router,
    method: &str,
    uri: &str,
    secret: &str,
    idempotency: &str,
    body: Option<serde_json::Value>,
) -> (StatusCode, serde_json::Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {secret}"));
    if method != "GET" {
        request = request.header("Idempotency-Key", idempotency);
    }
    let body = match body {
        Some(json) => {
            request = request.header("content-type", "application/json");
            Body::from(json.to_string())
        }
        None => Body::empty(),
    };
    let response = app
        .clone()
        .oneshot(request.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let text = response_body(response).await;
    let json = serde_json::from_str(&text).unwrap_or(serde_json::Value::String(text));
    (status, json)
}

async fn identity_login(
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
    let text = response_body(response).await;
    let json = serde_json::from_str(&text).unwrap_or(serde_json::Value::String(text));
    (status, json)
}

fn is_generated_id(value: &str) -> bool {
    vam_orchestrator::users::is_generated_user_id(value)
}

#[tokio::test]
async fn created_user_gets_a_generated_id_and_signs_in_by_name_or_id() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let admin = mint_user_key(&store, "admin", UserRole::OrgAdmin, ORG_ADMIN_SCOPES);
    let app = test_app(app_state_http_only(store.clone()));

    let (status, body) = identity_call(
        &app,
        "POST",
        "/v1/users",
        &admin,
        "create-erin",
        Some(serde_json::json!({
            "username": "Erin.Quinn+work@Example.Test",
            "password": "erin-secret"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let user_id = body["user"]["userId"].as_str().expect("userId").to_string();
    assert!(is_generated_id(&user_id), "{user_id}");
    assert_eq!(body["user"]["username"], "Erin.Quinn+work@Example.Test");
    let stored = store
        .get_user(&user_id)
        .expect("stored by the generated id");
    assert_eq!(stored.username, "Erin.Quinn+work@Example.Test");
    assert_ne!(stored.user_id, stored.username);

    // Two creates never share an id.
    let (_, second) = identity_call(
        &app,
        "POST",
        "/v1/users",
        &admin,
        "create-finn",
        Some(serde_json::json!({ "username": "finn", "password": "finn-secret" })),
    )
    .await;
    assert_ne!(second["user"]["userId"], body["user"]["userId"]);

    // The list shows the generated id.
    let (status, listed) = identity_call(&app, "GET", "/v1/users", &admin, "", None).await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    assert!(listed.to_string().contains(&user_id), "{listed}");

    // Sign-in by the name as typed, in lower case, in upper case, and by the id.
    for login in [
        "Erin.Quinn+work@Example.Test",
        "erin.quinn+work@example.test",
        "ERIN.QUINN+WORK@EXAMPLE.TEST",
        user_id.as_str(),
    ] {
        let (status, session) = identity_login(&app, login, "erin-secret").await;
        assert_eq!(status, StatusCode::OK, "{login}: {session}");
        assert_eq!(session["userId"], user_id.as_str(), "{login}");
        // The response keeps the stored name, not the typed one.
        assert_eq!(
            session["username"], "Erin.Quinn+work@Example.Test",
            "{login}"
        );
    }
    let (status, _) = identity_login(&app, "ERIN.QUINN+WORK@EXAMPLE.TEST", "wrong").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    // The upper-cased id is not an id and not a name.
    let (status, _) = identity_login(&app, &user_id.to_uppercase(), "erin-secret").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // The session reads as the generated id.
    let (_, session) = identity_login(&app, "ERIN.QUINN+WORK@EXAMPLE.TEST", "erin-secret").await;
    let token = session["token"].as_str().expect("token");
    let (status, me) = identity_call(&app, "GET", "/v1/me", token, "", None).await;
    assert_eq!(status, StatusCode::OK, "{me}");
    assert_eq!(me["userId"], user_id.as_str());
    assert_eq!(me["username"], "Erin.Quinn+work@Example.Test");
}

#[tokio::test]
async fn create_user_with_a_chosen_user_id_is_400_and_makes_no_row() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let admin = mint_user_key(&store, "admin", UserRole::OrgAdmin, ORG_ADMIN_SCOPES);
    let app = test_app(app_state_http_only(store.clone()));
    let before = store.user_count().unwrap();

    for (key, chosen) in [
        ("chosen-1", "usr_aaaaaaaaaaaaaaaaaaaaaaaaaa"),
        ("chosen-2", "dana"),
        ("chosen-3", " "),
    ] {
        let (status, body) = identity_call(
            &app,
            "POST",
            "/v1/users",
            &admin,
            key,
            Some(serde_json::json!({
                "username": "dana",
                "password": "dana-secret",
                "userId": chosen
            })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{chosen:?}: {body}");
        assert!(
            body.to_string()
                .contains("userId is generated by the node; leave it empty"),
            "{body}"
        );
    }
    assert_eq!(
        store.user_count().unwrap(),
        before,
        "a refused create makes no row"
    );
    assert!(store.get_user_by_username("dana").is_err());

    // The same username without a userId is created right after.
    let (status, body) = identity_call(
        &app,
        "POST",
        "/v1/users",
        &admin,
        "chosen-ok",
        Some(serde_json::json!({ "username": "dana", "password": "dana-secret" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(is_generated_id(body["user"]["userId"].as_str().unwrap()));
    assert_eq!(store.user_count().unwrap(), before + 1);
}

#[tokio::test]
async fn create_user_refuses_case_twins_reserved_and_malformed_names() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let admin = mint_user_key(&store, "admin", UserRole::OrgAdmin, ORG_ADMIN_SCOPES);
    let app = test_app(app_state_http_only(store.clone()));
    let (status, body) = identity_call(
        &app,
        "POST",
        "/v1/users",
        &admin,
        "twin-first",
        Some(serde_json::json!({ "username": "Ada@Example.Test", "password": "ada-secret" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let count = store.user_count().unwrap();

    // A name equal ignoring letter case is a conflict, whatever the case.
    for (i, twin) in ["ADA@EXAMPLE.TEST", "ada@example.test", "OPERATOR", "Local"]
        .into_iter()
        .enumerate()
    {
        let (status, body) = identity_call(
            &app,
            "POST",
            "/v1/users",
            &admin,
            &format!("twin-{i}"),
            Some(serde_json::json!({ "username": twin, "password": "x-secret" })),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{twin}: {body}");
    }
    // Reserved and malformed names are 400.
    for (i, bad) in [
        "usr_chosen",
        "USR_chosen",
        "bootstrap",
        "BootStrap",
        "a@b@c",
        "has space",
        "ünï",
        "pct%40sign",
    ]
    .into_iter()
    .enumerate()
    {
        let (status, body) = identity_call(
            &app,
            "POST",
            "/v1/users",
            &admin,
            &format!("bad-{i}"),
            Some(serde_json::json!({ "username": bad, "password": "x-secret" })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}: {body}");
    }
    assert_eq!(
        store.user_count().unwrap(),
        count,
        "no refused create made a row"
    );

    // A different name is accepted.
    let (status, body) = identity_call(
        &app,
        "POST",
        "/v1/users",
        &admin,
        "twin-ok",
        Some(serde_json::json!({ "username": "someone-else", "password": "x-secret" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn bootstrap_admin_gets_a_generated_id_and_signs_in_in_any_case() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let admin = store
        .bootstrap_first_admin("Root.Admin", "headless-admin-secret")
        .unwrap();
    assert!(is_generated_id(&admin.user_id), "{}", admin.user_id);
    assert_eq!(admin.username, "Root.Admin");
    // Idempotent by name, in any letter case: no second row, same id.
    let again = store
        .bootstrap_first_admin("ROOT.ADMIN", "ignored")
        .unwrap();
    assert_eq!(again.user_id, admin.user_id);
    let app = test_app(app_state_http_only(store));
    let (status, session) = identity_login(&app, "root.admin", "headless-admin-secret").await;
    assert_eq!(status, StatusCode::OK, "{session}");
    assert_eq!(session["userId"], admin.user_id.as_str());
    assert_eq!(session["username"], "Root.Admin");
}

#[tokio::test]
async fn admin_user_key_routes_reach_a_user_by_id_and_by_any_case_name() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let admin = mint_user_key(&store, "admin", UserRole::OrgAdmin, ORG_ADMIN_SCOPES);
    let member = store
        .create_managed_user_generated(
            "Ada.Lovelace+e2e@Example.Test",
            "ada-secret",
            UserRole::Member,
        )
        .unwrap();
    let app = test_app(app_state_http_only(store.clone()));

    let paths = [
        member.user_id.clone(),
        "Ada.Lovelace+e2e@Example.Test".to_string(),
        "ADA.LOVELACE+E2E@EXAMPLE.TEST".to_string(),
        "ada.lovelace+e2e@example.test".to_string(),
    ];
    for (i, who) in paths.iter().enumerate() {
        // Mint through the path, then list through the same path.
        let (status, minted) = identity_call(
            &app,
            "POST",
            &format!("/v1/users/{who}/keys"),
            &admin,
            &format!("mint-{i}"),
            Some(serde_json::json!({ "name": format!("ci-{i}"), "scopes": ["node:read"] })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{who}: {minted}");
        let (status, listed) = identity_call(
            &app,
            "GET",
            &format!("/v1/users/{who}/keys"),
            &admin,
            "",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{who}: {listed}");
        for j in 0..=i {
            assert!(
                listed.to_string().contains(&format!("ci-{j}")),
                "{who}: {listed}"
            );
        }
    }
    // Every key landed on the same member, whichever path named them.
    let owned: Vec<String> = store
        .list_api_keys_for_owner(&member.user_id)
        .unwrap()
        .into_iter()
        .map(|k| k.name)
        .collect();
    for i in 0..paths.len() {
        assert!(owned.contains(&format!("ci-{i}")), "{owned:?}");
    }
    // Revoke by the upper-cased name.
    let key_id = store
        .list_api_keys_for_owner(&member.user_id)
        .unwrap()
        .into_iter()
        .find(|k| k.name == "ci-0")
        .unwrap()
        .key_id;
    let (status, body) = identity_call(
        &app,
        "DELETE",
        &format!("/v1/users/ADA.LOVELACE+E2E@EXAMPLE.TEST/keys/{key_id}"),
        &admin,
        "revoke-0",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // An unknown name is 404, not a guess.
    let (status, _) =
        identity_call(&app, "GET", "/v1/users/nobody-here/keys", &admin, "", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn operator_in_any_letter_case_is_refused_on_every_user_key_route() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let admin = mint_user_key(&store, "admin", UserRole::OrgAdmin, ORG_ADMIN_SCOPES);
    let operator_keys_before = store
        .list_api_keys_for_owner(OPERATOR_USER_ID)
        .unwrap()
        .len();
    let app = test_app(app_state_http_only(store.clone()));
    for (i, who) in ["operator", "OPERATOR", "Operator", "oPeRaToR"]
        .into_iter()
        .enumerate()
    {
        let (status, body) = identity_call(
            &app,
            "GET",
            &format!("/v1/users/{who}/keys"),
            &admin,
            "",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "list {who}: {body}");
        let (status, body) = identity_call(
            &app,
            "POST",
            &format!("/v1/users/{who}/keys"),
            &admin,
            &format!("op-mint-{i}"),
            Some(serde_json::json!({ "name": "evil", "scopes": ["node:read"] })),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "mint {who}: {body}");
        let (status, body) = identity_call(
            &app,
            "DELETE",
            &format!("/v1/users/{who}/keys/key-any"),
            &admin,
            &format!("op-revoke-{i}"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "revoke {who}: {body}");
    }
    assert_eq!(
        store
            .list_api_keys_for_owner(OPERATOR_USER_ID)
            .unwrap()
            .len(),
        operator_keys_before,
        "no key reached the operator"
    );
}

#[tokio::test]
async fn the_id_beats_a_legacy_username_equal_to_it_on_user_key_routes() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let admin = mint_user_key(&store, "admin", UserRole::OrgAdmin, ORG_ADMIN_SCOPES);
    // A legacy cross-match: user `x` is named `carol`, and a user whose id is `carol` exists
    // under another name. Created in this order the node's own checks allow it, as an older
    // node did.
    let x = store.create_user("x", "carol").unwrap();
    let carol = store.create_user("carol", "caroline").unwrap();
    for (user, key) in [(&x, "key-x"), (&carol, "key-carol")] {
        store
            .create_api_key_owned(
                key,
                &format!("owned-by-{}", user.user_id),
                &mint_api_key_secret(),
                &format!("User::\"{}\"", user.user_id),
                &["node:read".to_string()],
                &user.user_id,
            )
            .unwrap();
    }
    let app = test_app(app_state_http_only(store));
    // The id `carol` is `carol`, not the user named carol.
    let (status, listed) =
        identity_call(&app, "GET", "/v1/users/carol/keys", &admin, "", None).await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    assert!(listed.to_string().contains("owned-by-carol"), "{listed}");
    assert!(!listed.to_string().contains("owned-by-x"), "{listed}");
    // The name in another letter case still reaches the user who holds it.
    let (status, listed) =
        identity_call(&app, "GET", "/v1/users/CAROL/keys", &admin, "", None).await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    assert!(listed.to_string().contains("owned-by-x"), "{listed}");
    assert!(!listed.to_string().contains("owned-by-carol"), "{listed}");
    // And login follows the same rule.
    set_password_and_login_as_id(&app, &admin, "carol", "carol-pw").await;
}

/// Sets a password through the org-admin route (id only) so the login rule can be exercised.
async fn set_password_and_login_as_id(
    app: &axum::Router,
    admin: &str,
    user_id: &str,
    password: &str,
) {
    let (status, body) = identity_call(
        app,
        "POST",
        &format!("/v1/users/{user_id}/password"),
        admin,
        &format!("pw-{user_id}"),
        Some(serde_json::json!({ "password": password })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, session) = identity_login(app, user_id, password).await;
    assert_eq!(status, StatusCode::OK, "{session}");
    assert_eq!(session["userId"], user_id, "the id wins at login");
}

#[tokio::test]
async fn operator_creates_a_key_for_a_user_named_in_any_case() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let (store, bootstrap) = bootstrap_store();
    let member = store
        .create_managed_user_generated("Grace@Example.Test", "grace-secret", UserRole::Member)
        .unwrap();
    let app = test_app(app_state_http_only(store.clone()));
    for (i, who) in [
        "Grace@Example.Test",
        "GRACE@EXAMPLE.TEST",
        member.user_id.as_str(),
    ]
    .into_iter()
    .enumerate()
    {
        let (status, body) = identity_call(
            &app,
            "POST",
            "/v1/keys",
            &bootstrap,
            &format!("op-key-{i}"),
            Some(serde_json::json!({
                "name": format!("grace-{i}"),
                "userId": who,
                "scopes": ["node:read"]
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{who}: {body}");
    }
    let owned = store.list_api_keys_for_owner(&member.user_id).unwrap();
    assert_eq!(owned.len(), 3, "{owned:?}");
}

#[tokio::test]
async fn admin_cannot_rename_or_reach_users_through_the_id_only_routes_by_name() {
    let _process = PROCESS_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _state = isolate_state();
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let admin = mint_user_key(&store, "admin", UserRole::OrgAdmin, ORG_ADMIN_SCOPES);
    let member = store
        .create_managed_user_generated("Hana", "hana-secret", UserRole::Member)
        .unwrap();
    let app = test_app(app_state_http_only(store.clone()));
    // Id-only routes stay id-only: the login name is a 404, the id works.
    let (status, _) = identity_call(
        &app,
        "POST",
        "/v1/users/hana/deactivate",
        &admin,
        "deact-name",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = identity_call(
        &app,
        "POST",
        "/v1/users/HANA/password",
        &admin,
        "pw-name",
        Some(serde_json::json!({ "password": "new-secret" })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(!store.get_user(&member.user_id).unwrap().is_deactivated());
    let (status, body) = identity_call(
        &app,
        "POST",
        &format!("/v1/users/{}/deactivate", member.user_id),
        &admin,
        "deact-id",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}
