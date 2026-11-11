use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use axum::body::{Body, HttpBody as _};
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use crate::api::auth::ControlPlanePdp;
use crate::api::{bootstrap_api_key_if_empty, router, AppState};
use crate::keys::default_scopes_for_role;
use crate::manager::SessionManager;
use crate::secrets::SecretsVault;
use crate::users::UserRole;
use crate::workflow::SqliteWorkflowStore;

async fn call(
    app: &axum::Router,
    method: &str,
    uri: &str,
    secret: &str,
    body: Option<serde_json::Value>,
    idem: bool,
) -> (StatusCode, serde_json::Value) {
    let mut b = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {secret}"))
        .header("content-type", "application/json");
    if idem {
        b = b.header("idempotency-key", "marketplaces-1");
    }
    let req = match body {
        Some(v) => b.body(Body::from(v.to_string())).unwrap(),
        None => b.body(Body::empty()).unwrap(),
    };
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    (
        status,
        serde_json::from_str(&text).unwrap_or(serde_json::json!({ "raw": text })),
    )
}

fn app() -> axum::Router {
    app_with_store().0
}

fn app_with_store() -> (axum::Router, Arc<SqliteWorkflowStore>) {
    let state_dir = tempfile::tempdir().unwrap().keep();
    crate::marketplace::install(crate::marketplace::MarketplaceConfig::from_env(&state_dir));
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    bootstrap_api_key_if_empty(&store, Some("operator-secret".into())).unwrap();
    for (user, secret, role) in [
        ("ada", "member-secret", UserRole::Member),
        ("olga", "orgadmin-secret", UserRole::OrgAdmin),
    ] {
        store.create_user(user, user).unwrap();
        store
            .create_api_key_owned(
                &format!("{user}-key"),
                user,
                secret,
                &format!("User::\"{user}\""),
                &default_scopes_for_role(role),
                user,
            )
            .unwrap();
    }
    let (manager, _rx) = SessionManager::new(
        store.clone(),
        Arc::new(vam_core::CidAllocator::default()),
        Arc::new(|_vm_id, _cid| -> Box<dyn vam_core::Vmm> { unreachable!("no VM here") }),
    );
    let app = router(AppState {
        store: store.clone(),
        vault: SecretsVault::for_tests(store.clone()),
        manager,
        pdp: Arc::new(ControlPlanePdp::new().unwrap()),
        listen_addr: "127.0.0.1:0".into(),
        draining: Arc::new(AtomicBool::new(false)),
        ready: Arc::new(AtomicBool::new(true)),
        e2e_mock_scripts: false,
    });
    (app, store)
}

#[tokio::test]
async fn a_member_gets_403_on_every_route() {
    let app = app();
    let add = serde_json::json!({ "url": "https://git.example.com/a/b", "publisherDomain": "x.example.com" });
    let on = serde_json::json!({ "enabled": true });
    for (method, uri, body) in [
        ("GET", "/v1/marketplaces", None),
        ("POST", "/v1/marketplaces", Some(add)),
        ("PUT", "/v1/marketplaces/squrell", Some(on)),
        ("DELETE", "/v1/marketplaces/squrell", None),
        (
            "POST",
            "/v1/marketplaces/squrell/refresh",
            Some(serde_json::json!({})),
        ),
        ("GET", "/v1/marketplaces/squrell/listings", None),
        (
            "POST",
            "/v1/marketplaces/squrell/listings/microsoft-365/install",
            Some(serde_json::json!({})),
        ),
        (
            "POST",
            "/v1/marketplaces/squrell/listings/microsoft-365/update",
            Some(serde_json::json!({})),
        ),
    ] {
        let (s, b) = call(&app, method, uri, "member-secret", body, true).await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{method} {uri}: {b}");
    }
    // An org admin passes the gate.
    let (s, b) = call(
        &app,
        "GET",
        "/v1/marketplaces",
        "orgadmin-secret",
        None,
        false,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

#[tokio::test]
async fn the_list_has_the_preconfigured_squrell_marketplace_from_boot() {
    let app = app();
    let (s, b) = call(
        &app,
        "GET",
        "/v1/marketplaces",
        "orgadmin-secret",
        None,
        false,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let m = &b["marketplaces"][0];
    assert_eq!(m["id"], "squrell");
    assert_eq!(m["name"], "Squrell");
    assert_eq!(m["trust"], "squrell");
    assert_eq!(m["preconfigured"], true);
    assert_eq!(m["enabled"], true);
    assert_eq!(m["publisherDomain"], "hub.squrell.cloud");
    assert!(m.get("lastError").is_none());
    let (s, l) = call(
        &app,
        "GET",
        "/v1/marketplaces/squrell/listings",
        "orgadmin-secret",
        None,
        false,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(l["listings"], serde_json::json!([]));
    let (s, _) = call(
        &app,
        "GET",
        "/v1/marketplaces/nope/listings",
        "orgadmin-secret",
        None,
        false,
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = call(
        &app,
        "GET",
        "/v1/marketplaces/..%2Fetc/listings",
        "orgadmin-secret",
        None,
        false,
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn the_preconfigured_marketplace_can_be_disabled_never_removed() {
    let app = app();
    let (s, b) = call(
        &app,
        "DELETE",
        "/v1/marketplaces/squrell",
        "orgadmin-secret",
        None,
        true,
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    let off = serde_json::json!({ "enabled": false });
    let (s, b) = call(
        &app,
        "PUT",
        "/v1/marketplaces/squrell",
        "orgadmin-secret",
        Some(off),
        true,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["marketplace"]["enabled"], false);
    // A disabled marketplace is not refreshed.
    let (s, _) = call(
        &app,
        "POST",
        "/v1/marketplaces/squrell/refresh",
        "orgadmin-secret",
        Some(serde_json::json!({})),
        true,
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT);
    let bad = serde_json::json!({ "enabled": true, "url": "x" });
    let (s, _) = call(
        &app,
        "PUT",
        "/v1/marketplaces/squrell",
        "orgadmin-secret",
        Some(bad),
        true,
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = call(
        &app,
        "PUT",
        "/v1/marketplaces/m-nope",
        "orgadmin-secret",
        Some(serde_json::json!({ "enabled": true })),
        true,
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn adding_needs_a_key_a_good_url_and_a_confirmed_domain() {
    let app = app();
    let good = serde_json::json!({ "url": "https://git.example.com/a/b", "publisherDomain": "x.example.com" });
    let (s, _) = call(
        &app,
        "POST",
        "/v1/marketplaces",
        "orgadmin-secret",
        Some(good),
        false,
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "Idempotency-Key is required");
    for url in [
        "http://git.example.com/a/b",
        "file:///srv/repo",
        "ssh://git@git.example.com/a/b",
        "https://127.0.0.1/a/b",
        "https://[::1]/a/b",
        "https://169.254.169.254/latest",
        "https://user:pw@git.example.com/a/b",
        "https://git.example.com:8443/a/b",
        "https://localhost/a/b",
    ] {
        let body = serde_json::json!({ "url": url, "publisherDomain": "x.example.com" });
        let (s, b) = call(
            &app,
            "POST",
            "/v1/marketplaces",
            "orgadmin-secret",
            Some(body),
            true,
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{url}: {b}");
    }
    let no_domain = serde_json::json!({ "url": "https://git.example.com/a/b" });
    let (s, b) = call(
        &app,
        "POST",
        "/v1/marketplaces",
        "orgadmin-secret",
        Some(no_domain),
        true,
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
    assert!(b["message"].as_str().unwrap().contains("publisherDomain"));
    for domain in ["hub.squrell.cloud", "local.vam.dev", "examples.local"] {
        let body =
            serde_json::json!({ "url": "https://git.example.com/a/b", "publisherDomain": domain });
        let (s, b) = call(
            &app,
            "POST",
            "/v1/marketplaces",
            "orgadmin-secret",
            Some(body),
            true,
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{domain}: {b}");
    }
    let extra = serde_json::json!({ "url": "https://git.example.com/a/b", "publisherDomain": "x.example.com", "trust": "squrell" });
    let (s, _) = call(
        &app,
        "POST",
        "/v1/marketplaces",
        "orgadmin-secret",
        Some(extra),
        true,
    )
    .await;
    assert_eq!(
        s,
        StatusCode::BAD_REQUEST,
        "an added marketplace cannot ask for trust"
    );
    let dup = serde_json::json!({ "url": "https://github.com/squrell-inc/vam-marketplace.git", "publisherDomain": "x.example.com" });
    let (s, _) = call(
        &app,
        "POST",
        "/v1/marketplaces",
        "orgadmin-secret",
        Some(dup),
        true,
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT);
    let (s, b) = call(
        &app,
        "GET",
        "/v1/marketplaces",
        "orgadmin-secret",
        None,
        false,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        b["marketplaces"].as_array().unwrap().len(),
        1,
        "nothing was added"
    );
}

// ---- installed rows over HTTP (CN-4.5) ---------------------------------------------------------

const AGENT: &str = "urn:vam:agent:hub.squrell.cloud/squrell/executive-assistant";
const SKILL: &str = "urn:vam:skill:hub.squrell.cloud/squrell/m365-mail";

/// What an import leaves: a released agent and skill package in the marketplace tier, with
/// their origins.
fn installed(store: &SqliteWorkflowStore) {
    use prost::Message;
    use vam_spec::v1::{AgentSpec, ResourceId, SkillManifest, Version};
    let mut agent = Vec::new();
    AgentSpec {
        resource_id: Some(ResourceId { urn: AGENT.into() }),
        display_name: "Executive assistant".into(),
        description: "d".into(),
        role: "assistant".into(),
        allow_any_playbook: true,
        ..Default::default()
    }
    .encode(&mut agent)
    .unwrap();
    let mut skill = Vec::new();
    SkillManifest {
        resource_id: Some(ResourceId { urn: SKILL.into() }),
        version: Some(Version {
            major: 1,
            ..Default::default()
        }),
        description: "m365".into(),
        ..Default::default()
    }
    .encode(&mut skill)
    .unwrap();
    let c = store.connection.lock().unwrap();
    let owner = SqliteWorkflowStore::ensure_marketplace_owner(&c, "squrell", 1).unwrap();
    c.execute(
        "INSERT INTO agents (urn, major, minor, patch, prerelease, channel, revision, digest,
             spec_bytes, created_at, updated_at, owner_user_id, catalog_tier)
         VALUES (?1, 1, 0, 0, '', 'released', 1, 'agentdigest', ?2, 1, 1, ?3, 'marketplace')",
        rusqlite::params![AGENT, agent, owner],
    )
    .unwrap();
    c.execute(
        "INSERT INTO skill_packages (urn, major, minor, patch, prerelease, channel, revision, digest,
             blob_relpath, size_bytes, spec_bytes, created_at, updated_at, owner_user_id, catalog_tier)
         VALUES (?1, 1, 0, 0, '', 'released', 1, 'skilldigest', 'marketplace:squrell/x.tar.gz', 1, ?2, 1, 1, ?3, 'marketplace')",
        rusqlite::params![SKILL, skill, owner],
    )
    .unwrap();
    for (kind, urn, digest) in [
        ("agent", AGENT, "agentdigest"),
        ("skill", SKILL, "skilldigest"),
    ] {
        crate::marketplace::installs::write_origin(
            &c,
            kind,
            urn,
            (1, 0, 0),
            &crate::marketplace::installs::OriginRow {
                marketplace: "squrell".into(),
                listing: "microsoft-365".into(),
                git_ref: "microsoft-365-v1.0.0".into(),
                commit: "a".repeat(40),
                package_digest: digest.into(),
            },
            1,
        )
        .unwrap();
    }
}

#[tokio::test]
async fn an_installed_row_shows_its_origin_and_nobody_changes_it() {
    let (app, store) = app_with_store();
    installed(&store);

    // A member sees the released install like any release, with its tier, read-only flag and origin.
    let (s, b) = call(&app, "GET", "/v1/agents", "member-secret", None, false).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let rec = b["records"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["spec"]["resourceId"]["urn"] == AGENT)
        .expect("the installed agent is listed")
        .clone();
    assert_eq!(rec["tier"], "marketplace");
    assert_eq!(rec["catalogTier"], "marketplace");
    assert_eq!(rec["readOnly"], true);
    assert_eq!(rec["origin"]["marketplace"], "squrell");
    assert_eq!(rec["origin"]["listing"], "microsoft-365");
    assert_eq!(rec["origin"]["ref"], "microsoft-365-v1.0.0");
    assert_eq!(rec["origin"]["commit"], "a".repeat(40));
    assert_eq!(rec["origin"]["packageDigest"], "agentdigest");
    let (s, b) = call(
        &app,
        "GET",
        &format!("/v1/agents/{AGENT}"),
        "member-secret",
        None,
        false,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        (b["tier"].as_str(), b["readOnly"].as_bool()),
        (Some("marketplace"), Some(true))
    );
    assert_eq!(b["origin"]["ref"], "microsoft-365-v1.0.0");
    let (s, b) = call(
        &app,
        "GET",
        "/v1/skills/packages",
        "member-secret",
        None,
        false,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let pkg = &b["packages"][0];
    assert_eq!(
        (pkg["tier"].as_str(), pkg["readOnly"].as_bool()),
        (Some("marketplace"), Some(true))
    );
    assert_eq!(pkg["origin"]["packageDigest"], "skilldigest");

    // The org admin cannot put, release or delete it.
    let put = serde_json::json!({ "spec": {
        "resourceId": { "urn": AGENT }, "displayName": "mine", "description": "mine",
        "role": "r", "systemInstructions": "x", "allowAnyPlaybook": true } });
    let (s, b) = call(
        &app,
        "PUT",
        "/v1/agents",
        "orgadmin-secret",
        Some(put),
        true,
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    let release = serde_json::json!({ "version": { "major": 9 } });
    let (s, b) = call(
        &app,
        "POST",
        &format!("/v1/agents/{AGENT}/release"),
        "operator-secret",
        Some(release.clone()),
        true,
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert!(b["message"].as_str().unwrap().contains("read-only"), "{b}");
    let (s, b) = call(
        &app,
        "POST",
        &format!("/v1/skills/packages/{SKILL}/release"),
        "operator-secret",
        Some(release),
        true,
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    for uri in [
        format!("/v1/agents/{AGENT}"),
        format!("/v1/skills/packages/{SKILL}"),
        "/v1/playbooks/urn:vam:playbook:local.vam.dev/acme/morning-triage".to_string(),
    ] {
        let (s, b) = call(&app, "DELETE", &uri, "orgadmin-secret", None, true).await;
        // The playbook was not installed here, so it is "not deletable"; the others are read-only.
        let want = if uri.contains("playbooks") {
            StatusCode::METHOD_NOT_ALLOWED
        } else {
            StatusCode::CONFLICT
        };
        assert_eq!(s, want, "{uri}: {b}");
    }
    // A plain org row is not deletable either; the catalog keeps every release.
    let (s, _) = call(
        &app,
        "DELETE",
        "/v1/agents/urn:vam:agent:local.vam.dev/acme/mine",
        "orgadmin-secret",
        None,
        true,
    )
    .await;
    assert_eq!(s, StatusCode::METHOD_NOT_ALLOWED);
    // A member cannot delete anything.
    let (s, _) = call(
        &app,
        "DELETE",
        &format!("/v1/agents/{AGENT}"),
        "member-secret",
        None,
        true,
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);

    // Still there, unchanged.
    let (s, b) = call(&app, "GET", "/v1/agents", "orgadmin-secret", None, false).await;
    assert_eq!(s, StatusCode::OK);
    let n = b["records"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["spec"]["resourceId"]["urn"] == AGENT)
        .count();
    assert_eq!(n, 1);
}

#[tokio::test]
async fn install_and_update_need_a_known_listing_and_say_why_they_refuse() {
    let (app, _store) = app_with_store();
    let none = Some(serde_json::json!({}));
    // The Squrell marketplace has not been read on this node (no git here): no listing.
    let (s, b) = call(
        &app,
        "POST",
        "/v1/marketplaces/squrell/listings/microsoft-365/install",
        "orgadmin-secret",
        none.clone(),
        true,
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
    let (s, _) = call(
        &app,
        "POST",
        "/v1/marketplaces/nope/listings/x/install",
        "orgadmin-secret",
        none.clone(),
        true,
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    // An Idempotency-Key is required, like every write.
    let (s, _) = call(
        &app,
        "POST",
        "/v1/marketplaces/squrell/listings/microsoft-365/update",
        "orgadmin-secret",
        none,
        false,
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    // The body takes only `ref`.
    let (s, _) = call(
        &app,
        "POST",
        "/v1/marketplaces/squrell/listings/microsoft-365/install",
        "orgadmin-secret",
        Some(serde_json::json!({ "trust": "squrell" })),
        true,
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}
