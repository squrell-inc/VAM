//! `POST /v1/fix-requests`: a member asks the person who can apply the fix a refusal named.
//!
//! The requester must be the run's caller; every other way a reference can fail answers the same
//! 404, byte for byte, and notifies nobody. Change, target and who come from the stored row, never
//! from the body.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;
use vam_core::{CidAllocator, MockVmm};
use vam_orchestrator::api::auth::ControlPlanePdp;
use vam_orchestrator::api::{self, AppState};
use vam_orchestrator::denials::NewDenial;
use vam_orchestrator::keys::{mint_api_key_secret, MEMBER_SCOPES};
use vam_orchestrator::manager::SessionManager;
use vam_orchestrator::secrets::SecretsVault;
use vam_orchestrator::users::UserRole;
use vam_orchestrator::workflow::SqliteWorkflowStore;
use vam_orchestrator::VmmFactory;
use vam_spec::v1::{DenialReason, FixChange, FixWho};

const URN: &str = "urn:vam:agent:hub.example.dev/acme/helper";
const MISSING: &str = "dnl_00000000000000000000000000";

fn state(store: Arc<SqliteWorkflowStore>) -> AppState {
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

struct World {
    store: Arc<SqliteWorkflowStore>,
    app: axum::Router,
    keys: std::collections::BTreeMap<&'static str, String>,
}

fn key(store: &SqliteWorkflowStore, user: &str, scopes: &[&str]) -> String {
    let secret = mint_api_key_secret();
    let scopes: Vec<String> = scopes.iter().map(|s| (*s).to_string()).collect();
    store
        .create_api_key_owned(
            &format!("key-{user}"),
            user,
            &secret,
            &format!("User::\"{user}\""),
            &scopes,
            user,
        )
        .expect("key");
    secret
}

fn world() -> World {
    let store = Arc::new(SqliteWorkflowStore::in_memory().expect("store"));
    for (id, role) in [
        ("alice", UserRole::Member),
        ("bob", UserRole::Member),
        ("owner1", UserRole::Member),
        ("admin1", UserRole::OrgAdmin),
    ] {
        store.create_user_with_role(id, id, role).expect("user");
    }
    let spec = vam_spec::v1::AgentSpec {
        resource_id: Some(vam_spec::v1::ResourceId { urn: URN.into() }),
        display_name: "Helper".into(),
        role: "assistant".into(),
        instruction_source: Some(
            vam_spec::v1::agent_spec::InstructionSource::SystemInstructions("x".into()),
        ),
        ..Default::default()
    };
    store
        .publish_agent_draft_with_owner(&spec, "owner1")
        .expect("agent");
    let mut keys = std::collections::BTreeMap::new();
    for user in ["alice", "bob", "owner1", "admin1"] {
        keys.insert(user, key(&store, user, MEMBER_SCOPES));
    }
    // A key that may read notifications and nothing else.
    store
        .create_user_with_role("reader", "reader", UserRole::Member)
        .expect("reader");
    keys.insert("reader", key(&store, "reader", &["notifications:read"]));
    let app = api::router(state(store.clone()));
    World { store, app, keys }
}

struct Reply {
    status: StatusCode,
    headers: Vec<(String, String)>,
    body: String,
}

impl Reply {
    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or(Value::Null)
    }
}

impl World {
    async fn send(&self, req: Request<Body>) -> Reply {
        let r = self.app.clone().oneshot(req).await.expect("response");
        let status = r.status();
        let mut headers: Vec<(String, String)> = r
            .headers()
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
            .collect();
        headers.sort();
        let body = String::from_utf8(
            axum::body::to_bytes(r.into_body(), usize::MAX)
                .await
                .expect("body")
                .to_vec(),
        )
        .expect("utf8");
        Reply {
            status,
            headers,
            body,
        }
    }

    async fn post_raw(&self, who: &str, idem: Option<&str>, body: String) -> Reply {
        let mut b = Request::builder()
            .method("POST")
            .uri("/v1/fix-requests")
            .header("authorization", format!("Bearer {}", self.keys[who]))
            .header("content-type", "application/json");
        if let Some(i) = idem {
            b = b.header("Idempotency-Key", i);
        }
        self.send(b.body(Body::from(body)).unwrap()).await
    }

    async fn ask(&self, who: &str, reference: &str) -> Reply {
        self.post_raw(
            who,
            Some("k-1"),
            json!({ "decisionRef": reference }).to_string(),
        )
        .await
    }

    async fn notifications(&self, who: &str) -> Vec<Value> {
        let r = self
            .send(
                Request::builder()
                    .method("GET")
                    .uri("/v1/notifications?kind=fix.requested")
                    .header("authorization", format!("Bearer {}", self.keys[who]))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
        assert_eq!(r.status, StatusCode::OK, "{}", r.body);
        r.json()["records"].as_array().cloned().unwrap_or_default()
    }

    #[allow(clippy::too_many_arguments)]
    fn denial(
        &self,
        caller: &str,
        reason: DenialReason,
        change: FixChange,
        who: FixWho,
        agent: &str,
        subject: &str,
        target: &str,
    ) -> String {
        self.store
            .insert_denial(&NewDenial {
                run_id: Some("run-1"),
                session_id: "s1",
                caller_user_id: caller,
                agent_urn: agent,
                agent_version: "",
                reason,
                subject,
                deciding_scope: "org",
                target,
                change,
                who,
                decided_by: "connector_gate",
                rules: None,
            })
            .expect("denial")
    }

    fn connector(&self, caller: &str) -> String {
        self.denial(
            caller,
            DenialReason::ConnectorDisabled,
            FixChange::EnableConnector,
            FixWho::OrgAdmin,
            URN,
            "microsoft-365",
            "microsoft-365",
        )
    }

    fn host(&self, caller: &str, agent: &str, host: &str) -> String {
        self.denial(
            caller,
            DenialReason::EgressRule,
            FixChange::AllowHost,
            FixWho::AgentOwner,
            agent,
            host,
            host,
        )
    }
}

#[tokio::test]
async fn another_callers_reference_is_a_404_like_a_missing_one_and_the_owner_can_file_it() {
    let w = world();
    let alices = w.connector("alice");

    // Not yours, nonexistent, over-long, no fix, not askable: one answer, byte for byte.
    let no_fix = w.denial(
        "bob",
        DenialReason::PolicyRule,
        FixChange::Unspecified,
        FixWho::Unspecified,
        URN,
        "write_file",
        "builtin:write_file",
    );
    let self_fix = w.denial(
        "bob",
        DenialReason::SecretMissing,
        FixChange::SetSecret,
        FixWho::Self_,
        URN,
        "tok",
        "tok",
    );
    let refused = [
        w.ask("bob", &alices).await,
        w.ask("bob", MISSING).await,
        w.ask("bob", &"d".repeat(300)).await,
        w.ask("bob", &no_fix).await,
        w.ask("bob", &self_fix).await,
        w.ask("bob", "").await,
        w.ask("reader", &alices).await,
    ];
    // (the reader has no `runs:write`: that is a 403 for everyone, whatever the reference)
    let reader = &refused[6];
    assert_eq!(reader.status, StatusCode::FORBIDDEN, "{}", reader.body);
    for r in &refused[..6] {
        assert_eq!(r.status, StatusCode::NOT_FOUND, "{}", r.body);
        assert_eq!(r.body, refused[0].body, "same body");
        assert_eq!(r.headers, refused[0].headers, "same headers");
    }
    assert_eq!(refused[0].json()["message"], "denial not found");
    assert_eq!(refused[0].json()["code"], "NotFound");
    assert!(!refused[0].body.contains("dnl_"));
    // Nobody was told.
    for user in ["admin1", "alice", "bob", "owner1"] {
        assert!(w.notifications(user).await.is_empty(), "{user}");
    }

    // Alice files her own: 201, from the row.
    let first = w.ask("alice", &alices).await;
    assert_eq!(first.status, StatusCode::CREATED, "{}", first.body);
    let body = first.json();
    assert!(body["id"].as_str().unwrap().starts_with("frq_"));
    assert_eq!(body["change"], "ENABLE_CONNECTOR");
    assert_eq!(body["target"], "microsoft-365");
    assert_eq!(body["who"], "ORG_ADMIN");
    assert_eq!(body["asked"], "an org admin");
    assert_eq!(body["existing"], false);
    assert!(body["createdAt"].as_i64().unwrap() > 0);

    // The org admin has exactly one, with the card's payload and the denial's run.
    let rows = w.notifications("admin1").await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    let n = &rows[0];
    assert_eq!(n["kind"], "fix.requested");
    assert_eq!(n["severity"], "action");
    assert_eq!(n["runId"], "run-1");
    assert_eq!(n["agentUrn"], URN);
    assert_eq!(n["agentLabel"], "helper");
    assert_eq!(n["title"], "alice asks you to enable microsoft-365");
    assert!(n.get("spaceId").is_none() && n.get("threadId").is_none());
    let fr = &n["fixRequest"];
    assert_eq!(fr["id"], body["id"]);
    assert_eq!(fr["requester"], "alice");
    assert_eq!(fr["requesterUserId"], "alice");
    assert_eq!(fr["denial"]["reason"], "CONNECTOR_DISABLED");
    assert_eq!(fr["denial"]["fix"]["who"], "ORG_ADMIN");
    assert_eq!(
        fr["denial"]["fix"]["route"]["path"],
        "/v1/connectors/microsoft-365/node"
    );
    // Neither the requester nor an unrelated member is notified.
    assert!(w.notifications("alice").await.is_empty());
    assert!(w.notifications("bob").await.is_empty());

    // Bob still cannot use alice's reference, now that a request exists.
    let again_bob = w.ask("bob", &alices).await;
    assert_eq!(again_bob.status, StatusCode::NOT_FOUND);
    assert_eq!(again_bob.body, refused[0].body);

    // A repeat answers 200 with the first request and tells nobody.
    let second = w.ask("alice", &alices).await;
    assert_eq!(second.status, StatusCode::OK, "{}", second.body);
    assert_eq!(second.json()["id"], body["id"]);
    assert_eq!(second.json()["existing"], true);
    assert_eq!(w.notifications("admin1").await.len(), 1);
}

#[tokio::test]
async fn nothing_the_client_sends_besides_the_reference_is_read() {
    let w = world();
    let alices = w.connector("alice");
    let r = w
        .post_raw(
            "alice",
            Some("k-1"),
            json!({
                "decisionRef": alices,
                "fix": { "change": "ALLOW_TOOL", "target": "urn:x#y", "who": "SELF" },
                "change": "SET_SECRET", "target": "evil", "who": "AGENT_OWNER",
                "recipients": ["bob"], "title": "hi"
            })
            .to_string(),
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    let b = r.json();
    assert_eq!(
        (b["change"].as_str(), b["target"].as_str()),
        (Some("ENABLE_CONNECTOR"), Some("microsoft-365"))
    );
    assert!(w.notifications("bob").await.is_empty());
    assert_eq!(w.notifications("admin1").await.len(), 1);
}

#[tokio::test]
async fn a_malformed_request_is_refused_before_any_lookup() {
    let w = world();
    let alices = w.connector("alice");
    let r = w.post_raw("alice", Some("k-1"), "{}".into()).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "{}", r.body);
    let r = w
        .post_raw(
            "alice",
            Some("k-1"),
            json!({ "decisionRef": 7 }).to_string(),
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    let r = w.post_raw("alice", Some("k-1"), "not json".into()).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "{}", r.body);
    let r = w
        .post_raw("alice", None, json!({ "decisionRef": alices }).to_string())
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "{}", r.body);
    assert!(r.body.contains("Idempotency-Key"), "{}", r.body);
    let r = w.post_raw("reader", Some("k-1"), "not json".into()).await;
    assert_eq!(
        r.status,
        StatusCode::FORBIDDEN,
        "scope before body: {}",
        r.body
    );
    assert!(w.notifications("admin1").await.is_empty());
}

#[tokio::test]
async fn the_owner_is_asked_unless_they_are_the_requester_or_gone() {
    let w = world();
    // alice is refused a host on owner1's agent: owner1 is asked.
    let r = w.ask("alice", &w.host("alice", URN, "example.com")).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    assert_eq!(r.json()["asked"], "the agent's owner");
    assert_eq!(r.json()["target"], format!("{URN}#example.com"));
    let rows = w.notifications("owner1").await;
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0]["title"],
        "alice asks you to allow a host for helper"
    );
    assert!(!rows[0]["title"].as_str().unwrap().contains("example.com"));
    assert!(rows[0]["body"].as_str().unwrap().contains("`example.com`"));
    assert!(w.notifications("admin1").await.is_empty());

    // A second agent with the same host is a second request (the key is the fix target).
    let spec = vam_spec::v1::AgentSpec {
        resource_id: Some(vam_spec::v1::ResourceId {
            urn: "urn:vam:agent:hub.example.dev/acme/other".into(),
        }),
        display_name: "Other".into(),
        role: "assistant".into(),
        instruction_source: Some(
            vam_spec::v1::agent_spec::InstructionSource::SystemInstructions("x".into()),
        ),
        ..Default::default()
    };
    w.store
        .publish_agent_draft_with_owner(&spec, "owner1")
        .expect("agent");
    let r = w
        .ask(
            "alice",
            &w.host(
                "alice",
                "urn:vam:agent:hub.example.dev/acme/other",
                "example.com",
            ),
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    assert_eq!(w.notifications("owner1").await.len(), 2);

    // The requester is the owner: nobody else to ask.
    let r = w
        .ask("owner1", &w.host("owner1", URN, "mine.example.com"))
        .await;
    assert_eq!(r.status, StatusCode::CONFLICT, "{}", r.body);
    assert_eq!(r.json()["code"], "FailedPrecondition");
    assert_eq!(w.notifications("owner1").await.len(), 2);

    // An agent the catalog no longer has: no owner, the same 409 (never a different 404).
    let r = w
        .ask(
            "alice",
            &w.host(
                "alice",
                "urn:vam:agent:hub.example.dev/acme/gone",
                "x.example.com",
            ),
        )
        .await;
    assert_eq!(r.status, StatusCode::CONFLICT, "{}", r.body);

    // A deactivated owner is not asked.
    w.store.deactivate_user("owner1").expect("deactivate");
    let r = w
        .ask("alice", &w.host("alice", URN, "later.example.com"))
        .await;
    assert_eq!(r.status, StatusCode::CONFLICT, "{}", r.body);
}

#[tokio::test]
async fn host_requests_stop_at_ten_a_day_per_agent() {
    let w = world();
    for i in 0..10 {
        let r = w
            .ask("alice", &w.host("alice", URN, &format!("h{i}.example.com")))
            .await;
        assert_eq!(r.status, StatusCode::CREATED, "{i}: {}", r.body);
    }
    let r = w
        .ask("alice", &w.host("alice", URN, "h-over.example.com"))
        .await;
    assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS, "{}", r.body);
    assert_eq!(r.json()["code"], "ResourceExhausted");
    assert_eq!(w.notifications("owner1").await.len(), 10);
}

#[tokio::test]
async fn two_requests_at_once_notify_the_admin_once() {
    let w = world();
    let alices = w.connector("alice");
    let (a, b) = tokio::join!(w.ask("alice", &alices), w.ask("alice", &alices));
    let mut statuses = [a.status, b.status];
    statuses.sort();
    assert_eq!(statuses, [StatusCode::OK, StatusCode::CREATED]);
    assert_eq!(a.json()["id"], b.json()["id"]);
    assert_eq!(w.notifications("admin1").await.len(), 1);
}
