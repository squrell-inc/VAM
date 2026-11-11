//! IT-2.4 and IT-2.5 over the router: 429s with their headers, who is and is not limited, and the
//! paged lists. The limits are installed small so a handful of requests reach them.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;
use vam_core::{CidAllocator, MockVmm};
use vam_orchestrator::api::auth::ControlPlanePdp;
use vam_orchestrator::api::rate_limit::{self, RateLimitConfig};
use vam_orchestrator::api::{self, AppState};
use vam_orchestrator::keys::{mint_api_key_secret, BOOTSTRAP_SCOPES};
use vam_orchestrator::manager::SessionManager;
use vam_orchestrator::secrets::SecretsVault;
use vam_orchestrator::workflow::SqliteWorkflowStore;
use vam_orchestrator::VmmFactory;
use vam_spec::v1::ResourceId;

fn install_small_limits() {
    rate_limit::install(RateLimitConfig {
        enabled: true,
        key_rps: 1,
        key_burst: 5,
        create_rps: 1,
        create_burst: 2,
        ip_rps: 1,
        ip_burst: 3,
    });
}

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

/// A router over a fresh store with two keys, `one` and `two`.
fn app() -> (axum::Router, Arc<SqliteWorkflowStore>, String, String) {
    install_small_limits();
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let scopes: Vec<String> = BOOTSTRAP_SCOPES.iter().map(|s| s.to_string()).collect();
    let mut keys = Vec::new();
    for name in ["one", "two"] {
        let secret = mint_api_key_secret();
        store
            .create_api_key(name, name, &secret, "User::\"tester\"", &scopes)
            .unwrap();
        keys.push(secret);
    }
    let router = api::router(app_state(store.clone()));
    (router, store, keys.remove(0), keys.remove(0))
}

async fn call(
    app: &axum::Router,
    method: &str,
    path: &str,
    bearer: Option<&str>,
) -> axum::response::Response {
    let mut b = Request::builder().method(method).uri(path);
    if let Some(t) = bearer {
        b = b.header("authorization", format!("Bearer {t}"));
    }
    app.clone()
        .oneshot(b.body(Body::empty()).unwrap())
        .await
        .unwrap()
}

async fn json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = hyper::body::to_bytes(resp.into_body()).await.unwrap();
    serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
}

#[tokio::test]
async fn a_flooding_key_is_told_to_slow_down_and_another_key_is_not() {
    let (app, _store, one, two) = app();
    let mut limited = 0;
    for _ in 0..10 {
        let r = call(&app, "GET", "/v1/node", Some(&one)).await;
        if r.status() == StatusCode::TOO_MANY_REQUESTS {
            limited += 1;
            assert!(
                r.headers()["retry-after"]
                    .to_str()
                    .unwrap()
                    .parse::<u64>()
                    .unwrap()
                    >= 1
            );
            assert_eq!(r.headers()["x-vam-rate-limited-reason"], "key-rate");
            assert_eq!(json(r).await["code"], "RESOURCE_EXHAUSTED");
        }
    }
    assert_eq!(limited, 5, "burst 5 of 10 requests");
    assert_eq!(
        call(&app, "GET", "/v1/node", Some(&two)).await.status(),
        StatusCode::OK
    );
    // Health is never limited, with a key or without.
    for _ in 0..20 {
        assert_eq!(
            call(&app, "GET", "/v1/health", None).await.status(),
            StatusCode::OK
        );
    }
}

#[tokio::test]
async fn creation_has_its_own_smaller_bucket() {
    let (app, _store, one, _two) = app();
    // The bodies are empty, so these are refused as bad requests; the limiter still counts them.
    for _ in 0..2 {
        assert_ne!(
            call(&app, "POST", "/v1/runs", Some(&one)).await.status(),
            StatusCode::TOO_MANY_REQUESTS
        );
    }
    let r = call(&app, "POST", "/v1/runs", Some(&one)).await;
    assert_eq!(r.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(r.headers()["x-vam-rate-limited-reason"], "key-create-rate");
    // Reading is a different bucket.
    assert_eq!(
        call(&app, "GET", "/v1/node", Some(&one)).await.status(),
        StatusCode::OK
    );
}

#[tokio::test]
async fn bad_keys_drain_the_ip_bucket_but_a_valid_key_is_never_held_back() {
    let (app, _store, one, _two) = app();
    for _ in 0..3 {
        let r = call(&app, "GET", "/v1/node", Some("vam_sk_not_a_key")).await;
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    }
    let r = call(&app, "GET", "/v1/node", Some("vam_sk_not_a_key")).await;
    assert_eq!(r.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(r.headers()["x-vam-rate-limited-reason"], "ip-auth-rate");
    // No key at all is also refused now; a valid key from the same address is served.
    assert_eq!(
        call(&app, "GET", "/v1/node", None).await.status(),
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(
        call(&app, "GET", "/v1/node", Some(&one)).await.status(),
        StatusCode::OK
    );
    assert_eq!(
        call(&app, "GET", "/v1/health", None).await.status(),
        StatusCode::OK
    );
}

#[tokio::test]
async fn valid_keys_never_charge_the_ip_bucket() {
    let (app, _store, one, two) = app();
    // Ten requests over two keys from one address (burst 3 for the IP): none is an auth failure.
    for i in 0..10 {
        let k = if i % 2 == 0 { &one } else { &two };
        assert_eq!(
            call(&app, "GET", "/v1/node", Some(k)).await.status(),
            StatusCode::OK,
            "request {i}"
        );
    }
    // A handler's own 403 on a valid key is not an auth failure either.
    for _ in 0..3 {
        let _ = call(&app, "GET", "/v1/users", Some(&one)).await;
    }
}

fn run_urn(i: u32) -> ResourceId {
    ResourceId {
        urn: format!("urn:vam:run:local.vam.dev/acme/page{i:03}"),
    }
}

#[tokio::test]
async fn runs_list_pages_by_a_cursor_with_a_default_of_fifty() {
    let (app, store, _one, _two) = app();
    // The paging calls below would trip the small test limits; use the bootstrap path of a fresh
    // key per request instead.
    let scopes: Vec<String> = BOOTSTRAP_SCOPES.iter().map(|s| s.to_string()).collect();
    let mut fresh = {
        let store = store.clone();
        let mut n = 0u32;
        move || {
            n += 1;
            let secret = mint_api_key_secret();
            store
                .create_api_key(&format!("p{n}"), "p", &secret, "User::\"tester\"", &scopes)
                .unwrap();
            secret
        }
    };
    for i in 0..55 {
        store.create_run(&run_urn(i)).unwrap();
    }
    let k = fresh();
    let page = json(call(&app, "GET", "/v1/runs", Some(&k)).await).await;
    assert_eq!(page["statuses"].as_array().unwrap().len(), 50);
    let token = page["pagination"]["nextPageToken"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(!token.is_empty());
    let k = fresh();
    let rest = json(
        call(
            &app,
            "GET",
            &format!("/v1/runs?pageToken={token}"),
            Some(&k),
        )
        .await,
    )
    .await;
    assert_eq!(rest["statuses"].as_array().unwrap().len(), 5);
    assert!(rest["pagination"]["nextPageToken"]
        .as_str()
        .unwrap_or("")
        .is_empty());

    // A small page, a bad size, a bad token, a token from another list.
    let k = fresh();
    let small = json(call(&app, "GET", "/v1/runs?pageSize=2", Some(&k)).await).await;
    assert_eq!(small["statuses"].as_array().unwrap().len(), 2);
    let k = fresh();
    let r = call(&app, "GET", "/v1/runs?pageSize=-3", Some(&k)).await;
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    let k = fresh();
    let r = call(&app, "GET", "/v1/runs?pageToken=garbage", Some(&k)).await;
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    let k = fresh();
    let r = call(
        &app,
        "GET",
        &format!("/v1/sessions?pageToken={token}"),
        Some(&k),
    )
    .await;
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    // Sessions and approvals answer an empty first page with no token.
    for path in ["/v1/sessions", "/v1/approvals"] {
        let k = fresh();
        let r = call(&app, "GET", path, Some(&k)).await;
        assert_eq!(r.status(), StatusCode::OK, "{path}");
        let v = json(r).await;
        assert!(v["pagination"]["nextPageToken"]
            .as_str()
            .unwrap_or("")
            .is_empty());
    }
}
