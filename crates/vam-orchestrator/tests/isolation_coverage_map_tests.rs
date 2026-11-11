//! Phase 9 coverage-map: two members (A/B). A cannot list, get, or mutate B.
//!
//! Cross-user get is 404 (no existence oracle), matching the Phase 3 rule.
//! Catalog owner-only PUT stays 403 (Phase 7). Evidence export/verify across
//! two users is `crates/vam-orchestrator/tests/evidence_pack_test.rs`
//! (`two_user_evidence_export_verify_isolated`).

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
    let (manager, _cmd_rx) =
        SessionManager::new(store.clone(), cid_allocator.clone(), vmm_factory.clone());
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

fn member_plus_catalog_scopes() -> Vec<String> {
    let mut scopes: Vec<String> = MEMBER_SCOPES.iter().map(|s| (*s).to_string()).collect();
    scopes.push("catalog:write".to_string());
    scopes
}

fn two_members(store: &SqliteWorkflowStore) -> (String, String) {
    store.create_user("alice", "alice").unwrap();
    store.create_user("bob", "bob").unwrap();
    let scopes = member_plus_catalog_scopes();
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

fn bearer_get(uri: &str, secret: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(uri)
        .header("authorization", format!("Bearer {secret}"))
        .body(Body::empty())
        .unwrap()
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

fn bearer_empty(method: &str, uri: &str, secret: &str, idem: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {secret}"))
        .header("Idempotency-Key", idem)
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

fn seed_control_plane(store: &SqliteWorkflowStore) {
    store
        .create_session_with_owner(
            "sess-alice",
            None,
            "vm-alice",
            "alice--ws",
            "default",
            "alice",
        )
        .unwrap();
    store
        .create_session_with_owner("sess-bob", None, "vm-bob", "bob--ws", "default", "bob")
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
    store
        .append_outbox_event(bob_run, "prompt", r#"{"text":"bob-secret-prompt"}"#)
        .unwrap();
    store
        .record_published_approval(
            "appr-alice",
            alice_run,
            "alice",
            r#"{"title":"alice-hitl"}"#,
        )
        .unwrap();
    store
        .record_published_approval("appr-bob", bob_run, "bob", r#"{"title":"bob-hitl"}"#)
        .unwrap();
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
            "description": "coverage map",
            "role": "Tester",
            "systemInstructions": instructions,
            "allowAnyPlaybook": true
        }
    });
    let resp = app
        .clone()
        .oneshot(bearer_json(
            "PUT",
            "/v1/agents",
            secret,
            &format!("put-{urn}-{instructions}"),
            body,
        ))
        .await
        .unwrap();
    let status = resp.status();
    let _ = response_body(resp).await;
    status
}

/// Walks control plane, catalog, secrets, workspaces, approvals, and events.
#[tokio::test]
async fn isolation_two_user_coverage_map_a_cannot_see_or_mutate_b() {
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    store.initialize_catalog_schema().unwrap();
    let (alice, bob) = two_members(&store);
    seed_control_plane(&store);
    let app = test_app(app_state_http_only(store.clone()));
    let bob_run = "urn:vam:run:local.vam.dev/acme/run-bob";
    let bob_run_path = encode_run_path(bob_run);

    // --- Control plane: sessions ---
    let list_sessions = app
        .clone()
        .oneshot(bearer_get("/v1/sessions", &alice))
        .await
        .unwrap();
    let list_sessions_body = response_body(list_sessions).await;
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
        .oneshot(bearer_get("/v1/sessions/sess-bob", &alice))
        .await
        .unwrap();
    assert_eq!(get_bob_session.status(), StatusCode::NOT_FOUND);

    for (method, uri, idem) in [
        ("POST", "/v1/sessions/sess-bob/pause", "alice-pause-bob"),
        ("POST", "/v1/sessions/sess-bob/resume", "alice-resume-bob"),
        ("POST", "/v1/sessions/sess-bob/terminate", "alice-term-bob"),
    ] {
        let resp = app
            .clone()
            .oneshot(bearer_empty(method, uri, &alice, idem))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::NOT_FOUND,
            "{method} {uri} must be 404"
        );
        let _ = response_body(resp).await;
    }

    let snap = app
        .clone()
        .oneshot(bearer_json(
            "POST",
            "/v1/sessions/sess-bob/snapshots",
            &alice,
            "alice-snap-bob",
            serde_json::json!({"kind": 1}),
        ))
        .await
        .unwrap();
    assert_eq!(snap.status(), StatusCode::NOT_FOUND);
    let _ = response_body(snap).await;

    // --- Control plane: runs ---
    let list_runs = app
        .clone()
        .oneshot(bearer_get("/v1/runs", &alice))
        .await
        .unwrap();
    let list_runs_body = response_body(list_runs).await;
    assert!(list_runs_body.contains("run-alice"), "{list_runs_body}");
    assert!(!list_runs_body.contains("run-bob"), "{list_runs_body}");

    let get_bob_run = app
        .clone()
        .oneshot(bearer_get(&format!("/v1/runs/{bob_run_path}"), &alice))
        .await
        .unwrap();
    assert_eq!(get_bob_run.status(), StatusCode::NOT_FOUND);

    let cancel_bob = app
        .clone()
        .oneshot(bearer_empty(
            "POST",
            &format!("/v1/runs/{bob_run_path}/cancel"),
            &alice,
            "alice-cancel-bob",
        ))
        .await
        .unwrap();
    assert_eq!(cancel_bob.status(), StatusCode::NOT_FOUND);
    let _ = response_body(cancel_bob).await;
    assert_eq!(store.get_run_state(bob_run).unwrap(), "RUNNING");

    // --- Events ---
    let alice_events = app
        .clone()
        .oneshot(bearer_get(
            &format!("/v1/runs/{bob_run_path}/events"),
            &alice,
        ))
        .await
        .unwrap();
    let alice_events_status = alice_events.status();
    let alice_events_body = response_body(alice_events).await;
    assert_eq!(
        alice_events_status,
        StatusCode::NOT_FOUND,
        "{alice_events_body}"
    );
    assert!(
        !alice_events_body.contains("bob-secret-prompt"),
        "{alice_events_body}"
    );

    let bob_events = app
        .clone()
        .oneshot(bearer_get(&format!("/v1/runs/{bob_run_path}/events"), &bob))
        .await
        .unwrap();
    let bob_events_status = bob_events.status();
    let bob_events_body = response_body(bob_events).await;
    assert_eq!(bob_events_status, StatusCode::OK, "{bob_events_body}");
    assert!(
        bob_events_body.contains("bob-secret-prompt"),
        "{bob_events_body}"
    );

    // --- Approvals ---
    let list_appr = app
        .clone()
        .oneshot(bearer_get("/v1/approvals", &alice))
        .await
        .unwrap();
    let list_appr_body = response_body(list_appr).await;
    assert!(list_appr_body.contains("appr-alice"), "{list_appr_body}");
    assert!(!list_appr_body.contains("appr-bob"), "{list_appr_body}");
    assert!(!list_appr_body.contains("bob-hitl"), "{list_appr_body}");

    let get_bob_appr = app
        .clone()
        .oneshot(bearer_get("/v1/approvals/appr-bob", &alice))
        .await
        .unwrap();
    let get_bob_appr_status = get_bob_appr.status();
    let get_bob_appr_body = response_body(get_bob_appr).await;
    assert_eq!(
        get_bob_appr_status,
        StatusCode::NOT_FOUND,
        "{get_bob_appr_body}"
    );
    assert!(
        !get_bob_appr_body.contains("bob-hitl"),
        "{get_bob_appr_body}"
    );

    let decide_bob = app
        .clone()
        .oneshot(bearer_json(
            "POST",
            "/v1/approvals/appr-bob/decide",
            &alice,
            "alice-decide-bob-map",
            serde_json::json!({"decision": 1}),
        ))
        .await
        .unwrap();
    let decide_status = decide_bob.status();
    let decide_body = response_body(decide_bob).await;
    assert_eq!(decide_status, StatusCode::NOT_FOUND, "{decide_body}");
    assert!(!decide_body.contains("bob-hitl"), "{decide_body}");

    // --- Secrets ---
    let put = |token: &str, value: &str, idem: &str| {
        bearer_json(
            "PUT",
            "/v1/secrets",
            token,
            idem,
            serde_json::json!({
                "bindingId": "jira/auth-header",
                "value": value,
                "kind": 2,
                "scope": 2,
                "idempotencyKey": idem
            }),
        )
    };
    assert_eq!(
        app.clone()
            .oneshot(put(&alice, "alice-header", "alice-secret-map"))
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        app.clone()
            .oneshot(put(&bob, "bob-header", "bob-secret-map"))
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );

    let alice_fp = vam_orchestrator::SecretsVault::fingerprint("alice-header");
    let bob_fp = vam_orchestrator::SecretsVault::fingerprint("bob-header");

    let alice_list = app
        .clone()
        .oneshot(bearer_get("/v1/secrets", &alice))
        .await
        .unwrap();
    let alice_list_body = response_body(alice_list).await;
    assert!(alice_list_body.contains(&alice_fp), "{alice_list_body}");
    assert!(!alice_list_body.contains(&bob_fp), "{alice_list_body}");
    assert!(!alice_list_body.contains("bob-header"), "{alice_list_body}");

    let alice_get_bob = app
        .clone()
        .oneshot(bearer_get(
            "/v1/secrets/jira%2Fauth-header?scope=user",
            &alice,
        ))
        .await
        .unwrap();
    let alice_get_bob_status = alice_get_bob.status();
    let alice_get_bob_body = response_body(alice_get_bob).await;
    assert_eq!(alice_get_bob_status, StatusCode::OK, "{alice_get_bob_body}");
    assert!(
        alice_get_bob_body.contains(&alice_fp),
        "{alice_get_bob_body}"
    );
    assert!(
        !alice_get_bob_body.contains(&bob_fp),
        "{alice_get_bob_body}"
    );

    let alice_del_bob = app
        .clone()
        .oneshot(bearer_empty(
            "DELETE",
            "/v1/secrets/jira%2Fauth-header?scope=user",
            &alice,
            "alice-del-bob-secret",
        ))
        .await
        .unwrap();
    assert_eq!(alice_del_bob.status(), StatusCode::OK);
    let _ = response_body(alice_del_bob).await;

    let bob_still = app
        .clone()
        .oneshot(bearer_get(
            "/v1/secrets/jira%2Fauth-header?scope=user",
            &bob,
        ))
        .await
        .unwrap();
    let bob_still_status = bob_still.status();
    let bob_still_body = response_body(bob_still).await;
    assert_eq!(bob_still_status, StatusCode::OK, "{bob_still_body}");
    assert!(bob_still_body.contains(&bob_fp), "{bob_still_body}");

    // --- Catalog ---
    let alice_urn = "urn:vam:agent:local.vam.dev/acme/alice-map";
    let bob_urn = "urn:vam:agent:local.vam.dev/acme/bob-map";
    assert_eq!(
        put_agent_as(&app, &alice, alice_urn, "alice-draft").await,
        StatusCode::OK
    );
    assert_eq!(
        put_agent_as(&app, &bob, bob_urn, "bob-draft").await,
        StatusCode::OK
    );

    let alice_list_agents = app
        .clone()
        .oneshot(bearer_get("/v1/agents", &alice))
        .await
        .unwrap();
    let alice_list_agents_body = response_body(alice_list_agents).await;
    assert!(
        alice_list_agents_body.contains(alice_urn),
        "{alice_list_agents_body}"
    );
    assert!(
        !alice_list_agents_body.contains(bob_urn),
        "{alice_list_agents_body}"
    );

    let alice_get_bob_agent = app
        .clone()
        .oneshot(bearer_get(&format!("/v1/agents/{bob_urn}"), &alice))
        .await
        .unwrap();
    assert_eq!(alice_get_bob_agent.status(), StatusCode::NOT_FOUND);

    assert_eq!(
        put_agent_as(&app, &alice, bob_urn, "alice-steal").await,
        StatusCode::FORBIDDEN
    );

    // --- Workspaces: host prefix; guest cannot steal another user's volume ---
    assert_eq!(
        vam_orchestrator::run_context::resolve_session_workspace_id("alice", "assistant").unwrap(),
        "alice--assistant"
    );

    let alice_sessions = store.load_sessions_for_owner("alice").unwrap();
    let bob_sessions = store.load_sessions_for_owner("bob").unwrap();
    assert!(
        alice_sessions.iter().any(|s| s.workspace_id == "alice--ws"),
        "{alice_sessions:?}"
    );
    assert!(
        bob_sessions.iter().any(|s| s.workspace_id == "bob--ws"),
        "{bob_sessions:?}"
    );
    assert!(
        !alice_sessions.iter().any(|s| s.workspace_id == "bob--ws"),
        "alice must not see bob's workspace row"
    );
}

/// Single-user `local` stays on the same owner-filter path (Phase 9 parity).
#[tokio::test]
async fn isolation_two_user_coverage_map_single_user_parity() {
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    store.initialize_catalog_schema().unwrap();
    let secret = mint_api_key_secret();
    let scopes = member_plus_catalog_scopes();
    store
        .create_api_key_owned(
            "local-key",
            "local",
            &secret,
            "User::\"local\"",
            &scopes,
            "local",
        )
        .unwrap();
    store
        .create_session_with_owner(
            "sess-local-map",
            None,
            "vm-local-map",
            "local--ws",
            "default",
            "local",
        )
        .unwrap();
    store
        .persist_run_spec_with_owner(
            "urn:vam:run:local.vam.dev/acme/run-local-map",
            "{}",
            Some("sess-local-map"),
            "urn:vam:agent:local",
            Some("iso-local-map"),
            "User::\"local\"",
            "local",
        )
        .unwrap();
    let app = test_app(app_state_http_only(store));

    let sessions = app
        .clone()
        .oneshot(bearer_get("/v1/sessions", &secret))
        .await
        .unwrap();
    let sessions_body = response_body(sessions).await;
    assert!(sessions_body.contains("sess-local-map"), "{sessions_body}");

    let runs = app.oneshot(bearer_get("/v1/runs", &secret)).await.unwrap();
    let runs_body = response_body(runs).await;
    assert!(runs_body.contains("run-local-map"), "{runs_body}");
}
