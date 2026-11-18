//! `GET /v1/connectors`, `PUT /v1/connectors/{id}/node` and the REST connection routes.
//! Anyone with `node:read` lists the built-in
//! connectors with the node state and their own state. Only an org administrator (`users:admin`)
//! enables or disables one and records the admin consent. A member with `secrets:write` connects
//! a REST connector (`PUT …/connection`), checks it (`POST …/connection/check`) and disconnects
//! it (`DELETE …/connection`). A listing or a response carries no vault id and no token.

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::{get, post, put};
use axum::Router;
use serde_json::{json, Value};

use crate::api::auth::{require_idempotency_key, AuthContext};
use crate::api::error::ApiError;
use crate::api::{ensure_ready, json_http_response, AppState};
use crate::connectors::{self, PutNodeRequest};
use crate::error::OrchestratorError;
use crate::keys::USERS_ADMIN_SCOPE;
use crate::mcp_registry::mcp_oauth_refresh_token_binding_id;
use crate::secrets::{vault_owner_user_id, SecretScope};
use crate::{connector_client, connector_discovery};
use vam_spec::v1::VamErrorCode;

/// The scope of the connection routes: a member manages their own connection like a secret.
const CONNECTION_SCOPE: &str = "secrets:write";

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/connectors", get(list_connectors))
        .route("/v1/connectors/:id/node", put(put_node))
        .route(
            "/v1/connectors/:id/connection",
            put(put_connection).delete(delete_connection),
        )
        .route(
            "/v1/connectors/:id/connection/check",
            post(check_connection),
        )
}

async fn list_connectors(
    State(state): State<AppState>,
    auth: AuthContext,
) -> Result<Response, ApiError> {
    ensure_ready(&state, false)?;
    state.pdp.authorize(&auth, "ListConnectors", "node:read")?;
    let cfg = connectors::config();
    let nodes = state.store.node_connectors()?;
    let clients = state.store.node_clients()?;
    let admin = auth.has_users_admin();
    let list: Vec<Value> = state
        .store
        .connector_entries()?
        .iter()
        .map(|e| {
            let def = &e.def;
            let id = e.id();
            let node = nodes.iter().find(|n| n.connector_id == id);
            let (mine, account) = caller_connection_state(&state, &auth, &id, node);
            let mut entry = connectors::list_entry(def, &cfg, node, mine, admin);
            connectors::show_account(
                &mut entry,
                connectors::caller_state(def, node, mine),
                account.as_deref(),
            );
            connectors::annotate_source(&mut entry, e, admin);
            let client = clients.iter().find(|c| c.connector_id == id);
            connector_client::annotate_client(&mut entry, def, client, admin);
            entry
        })
        .collect();
    Ok(json_http_response(
        StatusCode::OK,
        json!({ "connectors": list }).to_string(),
    ))
}

/// The caller's own connection to a connector, as a state and the account to show: from their MCP
/// row for an MCP connector, from their `connector_connections` row for a REST one (`None`: no
/// connection yet).
fn caller_connection_state(
    state: &AppState,
    auth: &AuthContext,
    id: &str,
    node: Option<&connectors::NodeConnector>,
) -> (Option<connectors::CallerState>, Option<String>) {
    let scope = SecretScope::User;
    let owner = vault_owner_user_id(scope, auth.owner_user_id()).to_string();
    let Some(policy) = state.store.mcp_policy(id).ok().flatten() else {
        let Ok(Some(conn)) = state.store.connector_connection(&owner, id) else {
            return (None, None);
        };
        let has_refresh = state
            .vault
            .has_binding_at(scope, &owner, &connectors::refresh_token_binding_id(id))
            .unwrap_or(false);
        let st = connectors::rest_connection_state(&conn, node, has_refresh);
        return (Some(st), conn.label);
    };
    let Ok(conn) = state.store.get_mcp_connection(scope, &owner, id) else {
        return (None, None);
    };
    let has_tokens = state
        .vault
        .has_binding_at(
            scope,
            &conn.owner_user_id,
            &mcp_oauth_refresh_token_binding_id(&conn.connection_id),
        )
        .unwrap_or(false);
    (
        connectors::mcp_connection_state(&policy, Some(&conn), has_tokens),
        None,
    )
}

/// The caller's list entry for one connector, after a write.
fn entry_for(
    state: &AppState,
    auth: &AuthContext,
    e: &connectors::ConnectorEntry,
) -> Result<Value, OrchestratorError> {
    let def = &e.def;
    let id = e.id();
    let node = state.store.node_connector(&id)?;
    let (mine, account) = caller_connection_state(state, auth, &id, node.as_ref());
    let mut entry = connectors::list_entry(
        def,
        &connectors::config(),
        node.as_ref(),
        mine,
        auth.has_users_admin(),
    );
    connectors::show_account(
        &mut entry,
        connectors::caller_state(def, node.as_ref(), mine),
        account.as_deref(),
    );
    connectors::annotate_source(&mut entry, e, auth.has_users_admin());
    let client = state.store.node_client(&id)?;
    connector_client::annotate_client(&mut entry, def, client.as_ref(), auth.has_users_admin());
    Ok(entry)
}

async fn put_node(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: String,
) -> Result<Response, ApiError> {
    ensure_ready(&state, true)?;
    state
        .pdp
        .authorize(&auth, "PutConnectorNode", USERS_ADMIN_SCOPE)?;
    let _idempotency = require_idempotency_key(&headers).map_err(ApiError::from)?;
    let found = entry_or_404(&state, &id)?;
    let def = found.def.clone();
    let req = parse_put_node(&body)?;
    let cfg = connectors::config();
    let existing = state.store.node_connector(&id)?;
    let existing_client = state.store.node_client(&id)?;
    let now = connectors::now_secs();
    // Everything is decided before anything is written: a refused body leaves the state as it was.
    let row = connectors::plan_put(
        &def,
        &cfg,
        existing.as_ref(),
        &req,
        now,
        auth.owner_user_id(),
    )?;
    let intent =
        connector_client::check_request(&def, existing_client.as_ref(), req.enabled, &req.client)?;
    let mcp_url = def.mcp.as_ref().map(|m| m.url.clone()).unwrap_or_default();
    let discovered = match &intent {
        connector_client::ClientIntent::Set(_) => {
            // The node's first fetch of URLs a connector file names: guarded, pinned, bounded.
            let (url, keeps) = (mcp_url.clone(), connector_client::keeps_secret(&def));
            let found = tokio::task::spawn_blocking(move || {
                connector_discovery::discover(&url, keeps, &connector_discovery::GuardedFetcher)
            })
            .await
            .map_err(|_| OrchestratorError::Core("provider discovery task failed".into()))?;
            Some(found.map_err(|e| match e {
                connector_discovery::DiscoveryError::Unreachable(why) => OrchestratorError::Mcp(
                    format!("could not read the provider's OAuth metadata: {why}"),
                ),
                connector_discovery::DiscoveryError::Unsuitable(why) => {
                    OrchestratorError::Conflict(why)
                }
            })?)
        }
        _ => None,
    };
    let plan = connector_client::plan_client(
        &def,
        existing_client.as_ref(),
        intent,
        discovered,
        now,
        auth.owner_user_id(),
    )?;
    state
        .store
        .put_node_with_client(&state.vault, &row, &plan, &mcp_url)?;
    let (mine, account) = caller_connection_state(&state, &auth, &id, Some(&row));
    let mut entry = connectors::list_entry(&def, &cfg, Some(&row), mine, true);
    connectors::show_account(
        &mut entry,
        connectors::caller_state(&def, Some(&row), mine),
        account.as_deref(),
    );
    connectors::annotate_source(&mut entry, &found, true);
    let client = state.store.node_client(&id)?;
    connector_client::annotate_client(&mut entry, &def, client.as_ref(), true);
    Ok(json_http_response(StatusCode::OK, entry.to_string()))
}

/// Parses the body of `PUT /v1/connectors/{id}/node`. A body that carries a `client` may carry the
/// client secret, and a serde error can quote the value it rejected, so such a body gets a fixed
/// message.
fn parse_put_node(body: &str) -> Result<PutNodeRequest, OrchestratorError> {
    serde_json::from_str(body).map_err(|e| {
        let has_client = serde_json::from_str::<Value>(body)
            .ok()
            .is_some_and(|v| v.get("client").is_some());
        if has_client {
            OrchestratorError::InvalidContract(
                "connector node body must be { enabled[, consent][, client: { clientId[, clientSecret] } | null] }"
                    .into(),
            )
        } else {
            OrchestratorError::InvalidContract(format!("connector node body: {e}"))
        }
    })
}

fn entry_or_404(
    state: &AppState,
    id: &str,
) -> Result<connectors::ConnectorEntry, OrchestratorError> {
    state
        .store
        .connector_entry(id)?
        .ok_or_else(|| OrchestratorError::NotFound(format!("connector `{id}`")))
}

/// `PUT /v1/connectors/{id}/connection`: the caller's own tokens for a REST connector.
///
/// Refused (and nothing written) unless the node enabled the connector with admin consent and the
/// id token's `tid` is the consent tenant. The access and refresh tokens become `connection`-kind
/// vault rows; the id token is read and dropped. The response is the caller's list entry.
async fn put_connection(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: String,
) -> Result<Response, ApiError> {
    ensure_ready(&state, true)?;
    state
        .pdp
        .authorize(&auth, "PutConnectorConnection", CONNECTION_SCOPE)?;
    let idempotency = require_idempotency_key(&headers).map_err(ApiError::from)?;
    let def = entry_or_404(&state, &id)?;
    // A fixed message: a serde error can quote the value it rejected.
    let req: connectors::PutConnectionRequest = serde_json::from_str(&body).map_err(|_| {
        OrchestratorError::InvalidContract(
            "connection body must be { accessToken, refreshToken, idToken[, expiresIn] }".into(),
        )
    })?;
    let owner = vault_owner_user_id(SecretScope::User, auth.owner_user_id()).to_string();
    {
        let _gate = connectors::lock_connection(&owner, &id).await;
        let now = connectors::now_secs();
        let node = state.store.node_connector(&id)?;
        // Every refusal is decided here, before any write.
        let plan = connectors::plan_connection(&def.def, node.as_ref(), &req, now)?;
        connectors::write_connection(
            &state.store,
            &state.vault,
            &owner,
            &id,
            &plan,
            &req,
            Some(&idempotency),
            now,
        )?;
    }
    // The lock is released: a resumed intent takes it to check the token's expiry. Only this
    // owner's parks on this connector continue (the key carries the owner).
    super::reauth::resume_key(&state, &crate::connector_park::park_key(&owner, &id));
    Ok(json_http_response(
        StatusCode::OK,
        entry_for(&state, &auth, &def)?.to_string(),
    ))
}

/// `DELETE /v1/connectors/{id}/connection`: the caller's token rows and connection row. Allowed
/// whether or not the node still has the connector enabled.
async fn delete_connection(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    ensure_ready(&state, true)?;
    state
        .pdp
        .authorize(&auth, "DeleteConnectorConnection", CONNECTION_SCOPE)?;
    let _idempotency = require_idempotency_key(&headers).map_err(ApiError::from)?;
    let def = entry_or_404(&state, &id)?;
    if state.store.mcp_policy(&id)?.is_some() {
        return Err(OrchestratorError::InvalidContract(format!(
            "`{id}` is an MCP connector; disconnect it with `vam mcp disconnect {id}` (/v1/mcp/connections)"
        ))
        .into());
    }
    let owner = vault_owner_user_id(SecretScope::User, auth.owner_user_id()).to_string();
    let _gate = connectors::lock_connection(&owner, &id).await;
    connectors::disconnect(&state.store, &state.vault, &owner, &id)?;
    Ok(json_http_response(
        StatusCode::OK,
        entry_for(&state, &auth, &def)?.to_string(),
    ))
}

/// `POST /v1/connectors/{id}/connection/check`: one refresh at the consent tenant. The answer is
/// the caller's list entry: `connected`, or `needsReconnect` when Microsoft refused the grant.
/// Microsoft being unreachable is a 502 and leaves the state as it was.
async fn check_connection(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    ensure_ready(&state, true)?;
    state
        .pdp
        .authorize(&auth, "CheckConnectorConnection", CONNECTION_SCOPE)?;
    let _idempotency = require_idempotency_key(&headers).map_err(ApiError::from)?;
    let def = entry_or_404(&state, &id)?;
    let owner = vault_owner_user_id(SecretScope::User, auth.owner_user_id()).to_string();
    let _gate = connectors::lock_connection(&owner, &id).await;
    let (store, vault, cfg) = (
        state.store.clone(),
        state.vault.clone(),
        connectors::config(),
    );
    let (owner2, id2) = (owner.clone(), id.clone());
    let outcome = tokio::task::spawn_blocking(move || {
        connectors::check_connection(
            &store,
            &vault,
            &cfg,
            &owner2,
            &id2,
            connectors::now_secs(),
            crate::mcp_oauth_refresh::refresh_access_token_entra,
        )
    })
    .await
    .map_err(|_| OrchestratorError::Core("connector check task failed".into()))??;
    match outcome {
        connectors::CheckOutcome::Connected(_) | connectors::CheckOutcome::NeedsReconnect(_) => Ok(
            json_http_response(StatusCode::OK, entry_for(&state, &auth, &def)?.to_string()),
        ),
        connectors::CheckOutcome::Unavailable(reason) => Err(ApiError {
            status: StatusCode::BAD_GATEWAY,
            code: VamErrorCode::Unavailable,
            message: format!("could not check the connection: {reason}"),
            retryable: true,
        }),
    }
}

#[cfg(test)]
mod tests {
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
    ) -> (StatusCode, serde_json::Value) {
        let b = Request::builder()
            .method(method)
            .uri(uri)
            .header("authorization", format!("Bearer {secret}"))
            .header("idempotency-key", "connectors-1")
            .header("content-type", "application/json");
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
        app_world().0
    }

    fn app_world() -> (axum::Router, Arc<SqliteWorkflowStore>, Arc<SecretsVault>) {
        // The shipped definition carries Squrell's real client id. These tests run the node set
        // to the placeholder, so no check here can send a refresh to Microsoft.
        crate::connectors::install_config(crate::connectors::ConnectorConfig {
            client_ids: [(
                "microsoft-365".to_string(),
                vam_spec::connector::PLACEHOLDER_CLIENT_ID.to_string(),
            )]
            .into(),
        });
        let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
        bootstrap_api_key_if_empty(&store, Some("operator-secret".into())).unwrap();
        for (user, secret, role) in [
            ("ada", "member-secret", UserRole::Member),
            ("olga", "orgadmin-secret", UserRole::OrgAdmin),
        ] {
            store.create_user_with_role(user, user, role).unwrap();
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
        let vault = SecretsVault::for_tests(store.clone());
        let app = router(AppState {
            store: store.clone(),
            vault: vault.clone(),
            manager,
            pdp: Arc::new(ControlPlanePdp::new().unwrap()),
            listen_addr: "127.0.0.1:0".into(),
            draining: Arc::new(AtomicBool::new(false)),
            ready: Arc::new(AtomicBool::new(true)),
            e2e_mock_scripts: false,
        });
        (app, store, vault)
    }

    fn state_of(list: &serde_json::Value, id: &str) -> String {
        list["connectors"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["id"] == id)
            .unwrap()["state"]
            .as_str()
            .unwrap()
            .to_string()
    }

    #[tokio::test]
    async fn member_cannot_put_org_admin_can_and_the_list_follows() {
        let app = app();
        let (s, l) = call(&app, "GET", "/v1/connectors", "member-secret", None).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(state_of(&l, "linear"), "askAdmin");
        assert_eq!(state_of(&l, "microsoft-365"), "askAdmin");

        let on = serde_json::json!({ "enabled": true });
        let (s, b) = call(
            &app,
            "PUT",
            "/v1/connectors/linear/node",
            "member-secret",
            Some(on.clone()),
        )
        .await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{b}");
        let (_, l) = call(&app, "GET", "/v1/connectors", "member-secret", None).await;
        assert_eq!(state_of(&l, "linear"), "askAdmin");

        // An org administrator (users:admin, not the operator key) may.
        let (s, b) = call(
            &app,
            "PUT",
            "/v1/connectors/linear/node",
            "orgadmin-secret",
            Some(on.clone()),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        let (_, l) = call(&app, "GET", "/v1/connectors", "member-secret", None).await;
        assert_eq!(state_of(&l, "linear"), "connect");
        assert!(!l.to_string().contains("vault"));

        let off = serde_json::json!({ "enabled": false });
        let (s, _) = call(
            &app,
            "PUT",
            "/v1/connectors/linear/node",
            "operator-secret",
            Some(off),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        let (_, l) = call(&app, "GET", "/v1/connectors", "member-secret", None).await;
        assert_eq!(state_of(&l, "linear"), "askAdmin");
    }

    #[tokio::test]
    async fn refusals_unknown_unconfigured_consent_and_bad_bodies() {
        let app = app();
        let put = |id: &'static str, body: serde_json::Value| {
            let app = app.clone();
            async move {
                call(
                    &app,
                    "PUT",
                    &format!("/v1/connectors/{id}/node"),
                    "orgadmin-secret",
                    Some(body),
                )
                .await
                .0
            }
        };
        assert_eq!(
            put("nope", serde_json::json!({"enabled": true})).await,
            StatusCode::NOT_FOUND
        );
        // The node is set to the placeholder client id, so enabling is a 409.
        assert_eq!(
            put("microsoft-365", serde_json::json!({"enabled": true})).await,
            StatusCode::CONFLICT
        );
        // Consent on a connector that takes none.
        assert_eq!(
            put("linear", serde_json::json!({"enabled": true, "consent": {"tenantId": "11111111-1111-4111-8111-111111111111"}})).await,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            put("linear", serde_json::json!({"enable": true})).await,
            StatusCode::BAD_REQUEST
        );
        // Nothing was written by any refusal.
        let (_, l) = call(&app, "GET", "/v1/connectors", "orgadmin-secret", None).await;
        assert_eq!(state_of(&l, "linear"), "askAdmin");
        // No idempotency key: refused before anything is written.
        let req = Request::builder()
            .method("PUT")
            .uri("/v1/connectors/linear/node")
            .header("authorization", "Bearer orgadmin-secret")
            .body(Body::from(r#"{"enabled":true}"#))
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    const T: &str = "11111111-1111-4111-8111-111111111111";
    const ACCESS: &str = "ACCESS-SECRET-aaaaaaaaaaaaaaaa";
    const REFRESH: &str = "REFRESH-SECRET-bbbbbbbbbbbbbbb";

    fn id_token(tid: &str, upn: &str) -> String {
        use base64::Engine as _;
        let b = |v: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v);
        format!(
            "{}.{}.{}",
            b(br#"{"alg":"none"}"#),
            b(serde_json::json!({ "tid": tid, "preferred_username": upn })
                .to_string()
                .as_bytes()),
            b(b"sig")
        )
    }

    fn body(tid: &str, upn: &str) -> serde_json::Value {
        serde_json::json!({
            "accessToken": ACCESS, "refreshToken": REFRESH, "idToken": id_token(tid, upn)
        })
    }

    fn node(store: &SqliteWorkflowStore, enabled: bool, tenant: Option<&str>) {
        store
            .put_node_connector(&crate::connectors::NodeConnector {
                connector_id: "microsoft-365".into(),
                enabled,
                consent_tenant_id: tenant.map(str::to_string),
                consent_recorded_at: tenant.map(|_| 1),
                consent_recorded_by: tenant.map(|_| "olga".to_string()),
                updated_at: 1,
                updated_by: "olga".into(),
            })
            .unwrap();
    }

    fn connector_rows(vault: &SecretsVault) -> Vec<String> {
        vault
            .list_metadata()
            .unwrap()
            .into_iter()
            .map(|m| m.binding_id)
            .filter(|b| b.starts_with("connector/"))
            .collect()
    }

    fn no_token_text(text: &str) {
        for needle in [ACCESS, REFRESH, "eyJ", "NEW-ACCESS", "NEW-REFRESH"] {
            assert!(!text.contains(needle), "{needle} in {text}");
        }
    }

    #[tokio::test]
    async fn a_connection_is_refused_outside_the_consented_tenant_and_writes_nothing() {
        let (app, store, vault) = app_world();
        let put = |who: &'static str, b: serde_json::Value| {
            let app = app.clone();
            async move {
                call(
                    &app,
                    "PUT",
                    "/v1/connectors/microsoft-365/connection",
                    who,
                    Some(b),
                )
                .await
            }
        };
        // Not enabled: 403. Enabled without consent: 409.
        let (s, b) = put("member-secret", body(T, "ada@contoso.com")).await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{b}");
        node(&store, true, None);
        let (s, b) = put("member-secret", body(T, "ada@contoso.com")).await;
        assert_eq!(s, StatusCode::CONFLICT, "{b}");
        node(&store, true, Some(T));

        // A token from another tenant: 409, no vault row, no table row, tile stays "connect".
        let other = "22222222-2222-4222-8222-222222222222";
        let (s, b) = put("member-secret", body(other, "x@other.example")).await;
        assert_eq!(s, StatusCode::CONFLICT, "{b}");
        no_token_text(&b.to_string());
        assert!(connector_rows(&vault).is_empty());
        assert_eq!(
            store.connector_connection("ada", "microsoft-365").unwrap(),
            None
        );
        let (_, l) = call(&app, "GET", "/v1/connectors", "member-secret", None).await;
        assert_eq!(state_of(&l, "microsoft-365"), "connect");

        // A body the server cannot read says nothing of what was sent.
        let (s, b) = put(
            "member-secret",
            serde_json::json!({ "accessToken": ACCESS, "refreshToken": REFRESH, "idToken": "x", "expiresIn": REFRESH }),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
        no_token_text(&b.to_string());
        let (s, b) = put(
            "member-secret",
            serde_json::json!({ "accessToken": ACCESS }),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
        no_token_text(&b.to_string());
        assert!(connector_rows(&vault).is_empty());

        // An unknown connector, an MCP connector, and no idempotency key write nothing either.
        let (s, _) = call(
            &app,
            "PUT",
            "/v1/connectors/nope/connection",
            "member-secret",
            Some(body(T, "a@b.c")),
        )
        .await;
        assert_eq!(s, StatusCode::NOT_FOUND);
        let (s, _) = call(
            &app,
            "PUT",
            "/v1/connectors/linear/connection",
            "member-secret",
            Some(body(T, "a@b.c")),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        let req = Request::builder()
            .method("PUT")
            .uri("/v1/connectors/microsoft-365/connection")
            .header("authorization", "Bearer member-secret")
            .body(Body::from(body(T, "a@b.c").to_string()))
            .unwrap();
        assert_eq!(
            app.clone().oneshot(req).await.unwrap().status(),
            StatusCode::BAD_REQUEST
        );
        assert!(connector_rows(&vault).is_empty());
    }

    #[tokio::test]
    async fn a_connection_in_the_consented_tenant_is_connected_with_the_account_and_no_token_shows()
    {
        let (app, store, vault) = app_world();
        node(&store, true, Some(T));
        let (s, b) = call(
            &app,
            "PUT",
            "/v1/connectors/microsoft-365/connection",
            "member-secret",
            Some(body(T, "ada@contoso.com")),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(b["state"], "connected");
        assert_eq!(b["label"], "ada@contoso.com");
        no_token_text(&b.to_string());
        assert_eq!(connector_rows(&vault).len(), 2);

        // Only the member's own tile: the org administrator is not connected.
        let (_, l) = call(&app, "GET", "/v1/connectors", "member-secret", None).await;
        assert_eq!(state_of(&l, "microsoft-365"), "connected");
        no_token_text(&l.to_string());
        let (_, l) = call(&app, "GET", "/v1/connectors", "orgadmin-secret", None).await;
        assert_eq!(state_of(&l, "microsoft-365"), "connect");

        // The public secrets API shows metadata only and cannot rewrite the rows.
        let (s, lst) = call(&app, "GET", "/v1/secrets", "member-secret", None).await;
        assert_eq!(s, StatusCode::OK);
        no_token_text(&lst.to_string());
        let binding = "connector/microsoft-365/oauth/refresh-token";
        for kind in [1, 2, 3] {
            let (s, b) = call(
                &app,
                "PUT",
                "/v1/secrets",
                "member-secret",
                Some(serde_json::json!({ "bindingId": binding, "value": "EVIL-OVERWRITE", "kind": kind, "scope": 2 })),
            )
            .await;
            assert!(
                s == StatusCode::BAD_REQUEST || s == StatusCode::FORBIDDEN,
                "{kind}: {s} {b}"
            );
        }
        let (s, meta) = call(
            &app,
            "GET",
            "/v1/secrets/connector%2Fmicrosoft-365%2Foauth%2Frefresh-token?scope=user",
            "member-secret",
            None,
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{meta}");
        assert_eq!(meta["kind"], "KIND_CONNECTION");
        no_token_text(&meta.to_string());
        use secrecy::ExposeSecret;
        assert_eq!(
            vault
                .decrypt_secret_at(crate::secrets::SecretScope::User, "ada", binding)
                .unwrap()
                .expose_secret(),
            REFRESH
        );

        // Without a client id the check sends nothing (the placeholder never reaches Microsoft).
        let (s, b) = call(
            &app,
            "POST",
            "/v1/connectors/microsoft-365/connection/check",
            "member-secret",
            Some(serde_json::json!({})),
        )
        .await;
        assert_eq!(s, StatusCode::CONFLICT, "{b}");
        no_token_text(&b.to_string());
        let (s, _) = call(
            &app,
            "POST",
            "/v1/connectors/microsoft-365/connection/check",
            "orgadmin-secret",
            Some(serde_json::json!({})),
        )
        .await;
        assert_eq!(s, StatusCode::NOT_FOUND);

        // Disconnect removes the rows; a disabled connector can still be disconnected.
        node(&store, false, Some(T));
        let (s, b) = call(
            &app,
            "DELETE",
            "/v1/connectors/microsoft-365/connection",
            "member-secret",
            None,
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert!(connector_rows(&vault).is_empty());
        assert_eq!(
            store.connector_connection("ada", "microsoft-365").unwrap(),
            None
        );
        let (s, _) = call(
            &app,
            "DELETE",
            "/v1/connectors/microsoft-365/connection",
            "member-secret",
            None,
        )
        .await;
        assert_eq!(s, StatusCode::NOT_FOUND);
        let (s, _) = call(
            &app,
            "DELETE",
            "/v1/connectors/linear/connection",
            "member-secret",
            None,
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn a_reconnect_replaces_the_tokens_and_a_second_user_has_their_own() {
        let (app, store, vault) = app_world();
        node(&store, true, Some(T));
        for who in ["member-secret", "orgadmin-secret", "member-secret"] {
            let (s, b) = call(
                &app,
                "PUT",
                "/v1/connectors/microsoft-365/connection",
                who,
                Some(body(T, "u@contoso.com")),
            )
            .await;
            assert_eq!(s, StatusCode::OK, "{b}");
        }
        assert_eq!(connector_rows(&vault).len(), 4);
        assert!(store
            .connector_connection("olga", "microsoft-365")
            .unwrap()
            .is_some());
    }
    // ---- CN-5.2: Enable with the admin's client ------------------------------------------------

    const SECRET: &str = "ORG-CLIENT-SECRET-zzzzzzzzzzzz";

    /// The `filebox` listing's connector file, on hosts of this test's own (`tag`), installed as a
    /// marketplace would, with the provider's metadata answered by the discovery test seam.
    fn install_filebox(store: &SqliteWorkflowStore, tag: &str) {
        let json = serde_json::json!({
            "urn": "urn:vam:connector:connectors.community.example/community/filebox",
            "version": "1.0.0", "displayName": "Filebox", "publisher": "Community Tools Ltd",
            "description": "Files.", "icon": "filebox",
            "provider": { "kind": "oauth2", "profile": "discover", "client": "org",
                          "clientSecret": "org_vault", "scopes": ["read"], "orgConsent": "none" },
            "connection": { "holder": "user", "multiAccount": false, "label": "whoami.email" },
            "mcp": { "url": format!("https://mcp.{tag}.example/mcp"),
                     "tools": [ { "remote": "search_files", "sideEffect": "READ_ONLY" } ] }
        });
        let urn = json["urn"].as_str().unwrap().to_string();
        let c = store.connection.lock().unwrap();
        c.execute(
            "INSERT INTO connector_releases (urn, major, minor, patch, digest, spec_bytes, created_at,
                 updated_at, owner_user_id) VALUES (?1, 1, 0, 0, 'd', ?2, 1, 1, 'system')",
            rusqlite::params![urn, json.to_string().as_bytes()],
        )
        .unwrap();
        c.execute(
            "INSERT INTO catalog_origins (kind, urn, major, minor, patch, marketplace_id, listing_id,
                 git_ref, commit_sha, package_digest, installed_at)
             VALUES ('connector', ?1, 1, 0, 0, 'm-1', 'filebox-connector', 'filebox-v1', 'c', 'd', 1)",
            [urn],
        )
        .unwrap();
        crate::connector_discovery::test_seam::answer(
            &format!("https://mcp.{tag}.example/.well-known/oauth-protected-resource/mcp"),
            Some(serde_json::json!({
                "resource": format!("https://mcp.{tag}.example/mcp"),
                "authorization_servers": [format!("https://auth.{tag}.example")]
            })),
        );
        crate::connector_discovery::test_seam::answer(
            &format!("https://auth.{tag}.example/.well-known/oauth-authorization-server"),
            Some(serde_json::json!({
                "issuer": format!("https://auth.{tag}.example"),
                "authorization_endpoint": format!("https://auth.{tag}.example/authorize"),
                "token_endpoint": format!("https://auth.{tag}.example/token"),
                "revocation_endpoint": format!("https://auth.{tag}.example/revoke"),
                "code_challenge_methods_supported": ["S256"],
                "token_endpoint_auth_methods_supported": ["client_secret_post"]
            })),
        );
    }

    fn client_body(id: &str, secret: Option<&str>) -> serde_json::Value {
        let mut client = serde_json::json!({ "clientId": id });
        if let Some(s) = secret {
            client["clientSecret"] = serde_json::json!(s);
        }
        serde_json::json!({ "enabled": true, "client": client })
    }

    fn org_rows(vault: &SecretsVault) -> Vec<crate::secrets::HostSecretMetadata> {
        vault
            .list_metadata()
            .unwrap()
            .into_iter()
            .filter(|m| m.binding_id == "connector/filebox/oauth/client-secret")
            .collect()
    }

    fn assert_no_secret(what: &str, text: &str) {
        for needle in [SECRET, &SecretsVault::fingerprint(SECRET)] {
            assert!(!text.contains(needle), "{needle} in {what}: {text}");
        }
    }

    fn org_secret_value(vault: &SecretsVault) -> String {
        secrecy::ExposeSecret::expose_secret(
            &vault
                .decrypt_secret_at(
                    crate::secrets::SecretScope::Org,
                    crate::secrets::ORG_OWNER_USER_ID,
                    "connector/filebox/oauth/client-secret",
                )
                .unwrap(),
        )
        .clone()
    }

    fn entry_of<'a>(list: &'a serde_json::Value, id: &str) -> &'a serde_json::Value {
        list["connectors"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["id"] == id)
            .unwrap()
    }

    #[tokio::test]
    async fn an_admin_enables_with_a_client_and_the_secret_is_one_org_connection_row_never_read_back(
    ) {
        let (app, store, vault) = app_world();
        install_filebox(&store, "enable-ok");
        let put = |who: &'static str, b: serde_json::Value| {
            let app = app.clone();
            async move { call(&app, "PUT", "/v1/connectors/filebox/node", who, Some(b)).await }
        };
        // Refusals write nothing: no client, no secret, a member, a connector with the
        // publisher's own client.
        let (s, b) = put("orgadmin-secret", serde_json::json!({ "enabled": true })).await;
        assert_eq!(s, StatusCode::CONFLICT, "{b}");
        assert!(b.to_string().contains("client id"), "{b}");
        let (s, b) = put("orgadmin-secret", client_body("c-1", None)).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
        assert!(b.to_string().contains("secret"), "{b}");
        let (s, _) = put("member-secret", client_body("c-1", Some(SECRET))).await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        let (s, b) = call(
            &app,
            "PUT",
            "/v1/connectors/linear/node",
            "orgadmin-secret",
            Some(client_body("c-1", Some(SECRET))),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
        assert!(b.to_string().contains("client"), "{b}");
        assert_no_secret("a refusal", &b.to_string());
        assert_eq!(store.node_connector("filebox").unwrap(), None);
        assert_eq!(store.node_client("filebox").unwrap(), None);
        assert!(org_rows(&vault).is_empty());

        // The admin enables: the entry carries the client and the pinned endpoints, no secret.
        let (s, b) = put("orgadmin-secret", client_body("c-1", Some(SECRET))).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_no_secret("the enable answer", &b.to_string());
        assert_eq!(b["provider"]["client"], "org");
        assert_eq!(b["provider"]["clientId"], "c-1");
        assert_eq!(b["node"]["client"]["clientId"], "c-1");
        assert_eq!(b["node"]["client"]["secretSet"], true);
        assert_eq!(
            b["node"]["client"]["authorizeEndpoint"],
            "https://auth.enable-ok.example/authorize"
        );
        assert_eq!(
            b["node"]["client"]["tokenEndpoint"],
            "https://auth.enable-ok.example/token"
        );
        assert_eq!(b["node"]["client"]["updatedBy"], "olga");
        assert_eq!(b["node"]["configured"], true);
        assert_eq!(b["state"], "connect");

        // Exactly one vault row: org scope, connection kind, pinned to the token endpoint's host.
        let rows = org_rows(&vault);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].scope, crate::secrets::SecretScope::Org);
        assert_eq!(rows[0].kind, crate::secrets::SecretKind::Connection);
        assert_eq!(rows[0].allowed_destinations, ["auth.enable-ok.example"]);
        assert_eq!(org_secret_value(&vault), SECRET);

        // It is in no listing, for the admin or a member, and the secrets API refuses the row.
        for who in ["orgadmin-secret", "member-secret", "operator-secret"] {
            for uri in ["/v1/connectors", "/v1/secrets", "/v1/secrets?scope=org"] {
                let (s, b) = call(&app, "GET", uri, who, None).await;
                assert_eq!(s, StatusCode::OK, "{uri} {b}");
                assert_no_secret(uri, &b.to_string());
                assert!(!b.to_string().contains("client-secret"), "{uri}: {b}");
            }
        }
        let (s, b) = call(&app, "GET", "/v1/org/secrets", "orgadmin-secret", None).await;
        assert_eq!(s, StatusCode::OK);
        assert_no_secret("/v1/org/secrets", &b.to_string());
        assert!(!b.to_string().contains("client-secret"), "{b}");
        let row = "connector/filebox/oauth/client-secret";
        for (method, uri) in [
            ("GET", format!("/v1/secrets/{row}")),
            ("DELETE", format!("/v1/secrets/{row}")),
            ("DELETE", format!("/v1/secrets/{row}?scope=org")),
            ("DELETE", format!("/v1/org/secrets/{row}")),
        ] {
            let (s, b) = call(&app, method, &uri, "orgadmin-secret", None).await;
            assert_eq!(s, StatusCode::BAD_REQUEST, "{method} {uri} {b}");
            assert_no_secret(&uri, &b.to_string());
        }
        for uri in ["/v1/secrets", "/v1/org/secrets"] {
            let (s, b) = call(
                &app,
                "PUT",
                uri,
                "orgadmin-secret",
                Some(serde_json::json!({
                    "bindingId": row, "value": "overwritten", "kind": 2, "scope": 1
                })),
            )
            .await;
            assert_eq!(s, StatusCode::BAD_REQUEST, "{uri} {b}");
        }
        assert_eq!(org_rows(&vault).len(), 1, "the row survived every attempt");
        assert_eq!(org_secret_value(&vault), SECRET);

        // A member sees the client but not who saved it.
        let (_, l) = call(&app, "GET", "/v1/connectors", "member-secret", None).await;
        let e = entry_of(&l, "filebox");
        assert_eq!(e["node"]["client"]["clientId"], "c-1");
        assert_eq!(e["node"]["client"]["secretSet"], true);
        assert!(e["node"]["client"].get("updatedBy").is_none(), "{e}");

        // Disabled, then enabled again with no client in the request: the client and secret stay.
        let (s, b) = put("orgadmin-secret", serde_json::json!({ "enabled": false })).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(b["node"]["client"]["secretSet"], true);
        let (_, l) = call(&app, "GET", "/v1/connectors", "member-secret", None).await;
        let e = entry_of(&l, "filebox");
        assert_eq!(e["state"], "askAdmin");
        assert!(e["node"].get("client").is_none(), "{e}");
        let (s, b) = put("orgadmin-secret", serde_json::json!({ "enabled": true })).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(b["node"]["client"]["secretSet"], true);
        assert_eq!(org_rows(&vault).len(), 1);
    }

    #[tokio::test]
    async fn a_failed_discovery_or_an_unusable_answer_writes_nothing() {
        let (app, store, vault) = app_world();
        install_filebox(&store, "discover-bad");
        let put = |b: serde_json::Value| {
            let app = app.clone();
            async move {
                call(
                    &app,
                    "PUT",
                    "/v1/connectors/filebox/node",
                    "orgadmin-secret",
                    Some(b),
                )
                .await
            }
        };
        let meta_url = "https://auth.discover-bad.example/.well-known/oauth-authorization-server";
        let good = crate::connector_discovery::test_seam::lookup(meta_url)
            .unwrap()
            .unwrap();
        // The provider does not answer: 502, nothing stored.
        crate::connector_discovery::test_seam::answer(meta_url, None);
        crate::connector_discovery::test_seam::answer(
            "https://auth.discover-bad.example/.well-known/openid-configuration",
            None,
        );
        let (s, b) = put(client_body("c-1", Some(SECRET))).await;
        assert_eq!(s, StatusCode::BAD_GATEWAY, "{b}");
        assert_no_secret("an unreachable provider", &b.to_string());
        // The provider does not offer S256: 409.
        let mut no_pkce = good.clone();
        no_pkce["code_challenge_methods_supported"] = serde_json::json!(["plain"]);
        crate::connector_discovery::test_seam::answer(meta_url, Some(no_pkce));
        let (s, b) = put(client_body("c-1", Some(SECRET))).await;
        assert_eq!(s, StatusCode::CONFLICT, "{b}");
        assert!(b.to_string().contains("S256"), "{b}");
        // A token endpoint on a plain-http or an internal address: 409.
        for bad in [
            "http://auth.discover-bad.example/token",
            "https://127.0.0.1/token",
        ] {
            let mut meta = good.clone();
            meta["token_endpoint"] = serde_json::json!(bad);
            crate::connector_discovery::test_seam::answer(meta_url, Some(meta));
            let (s, b) = put(client_body("c-1", Some(SECRET))).await;
            assert_eq!(s, StatusCode::CONFLICT, "{bad}: {b}");
        }
        // A malformed body never echoes the secret it carries.
        let (s, b) = put(serde_json::json!({
            "enabled": true, "client": { "clientId": 7, "clientSecret": SECRET }
        }))
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
        assert_no_secret("a malformed body", &b.to_string());
        let (s, b) = put(serde_json::json!({ "enabled": true, "client": SECRET })).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
        assert_no_secret("a client that is a string", &b.to_string());
        assert_eq!(store.node_connector("filebox").unwrap(), None);
        assert_eq!(store.node_client("filebox").unwrap(), None);
        assert!(org_rows(&vault).is_empty());
        assert!(vault.list_metadata().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_new_secret_keeps_members_connected_and_a_new_client_id_or_removal_does_not() {
        use crate::mcp_registry::{
            McpAuthMode, McpOauthMetadata, McpOauthRegistrationMethod, McpOauthStatus,
        };
        let (app, store, vault) = app_world();
        install_filebox(&store, "reauth");
        let put = |b: serde_json::Value| {
            let app = app.clone();
            async move {
                call(
                    &app,
                    "PUT",
                    "/v1/connectors/filebox/node",
                    "orgadmin-secret",
                    Some(b),
                )
                .await
            }
        };
        let (s, b) = put(client_body("c-1", Some(SECRET))).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        // A member is connected.
        let meta = McpOauthMetadata {
            resource: "https://mcp.reauth.example/mcp".into(),
            authorization_server: "https://auth.reauth.example".into(),
            token_endpoint: "https://auth.reauth.example/token".into(),
            revocation_endpoint: None,
            client_id: "c-1".into(),
            registration_method: McpOauthRegistrationMethod::Manual,
            scopes: vec![],
            expires_at: None,
            status: McpOauthStatus::Connected,
            last_refresh_at: None,
            last_refresh_error: None,
        };
        store
            .upsert_mcp_connection(
                crate::secrets::SecretScope::User,
                "ada",
                "filebox",
                "https://mcp.reauth.example/mcp",
                "",
                &[],
                &[],
                McpAuthMode::Oauth,
                Some(&meta),
            )
            .unwrap();
        let status = || {
            store
                .get_mcp_connection(crate::secrets::SecretScope::User, "ada", "filebox")
                .unwrap()
                .oauth
                .unwrap()
                .status
        };
        // A new secret alone, and the same client again: the member stays connected.
        let (s, b) = put(client_body("c-1", Some("A-NEW-SECRET-yyyyyyyy"))).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(status(), McpOauthStatus::Connected);
        assert_eq!(org_rows(&vault).len(), 1);
        let (s, _) = put(client_body("c-1", None)).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(status(), McpOauthStatus::Connected);
        // The row holds the new secret (replaced in place).
        assert_eq!(org_secret_value(&vault), "A-NEW-SECRET-yyyyyyyy");
        // A new client id: members must connect again.
        let (s, b) = put(client_body("c-2", None)).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(b["node"]["client"]["clientId"], "c-2");
        assert_eq!(status(), McpOauthStatus::NeedsReauth);
        // Removal: only with a disable; the row goes.
        let (s, b) = put(serde_json::json!({ "enabled": true, "client": null })).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
        assert_eq!(org_rows(&vault).len(), 1);
        let (s, b) = put(serde_json::json!({ "enabled": false, "client": null })).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert!(b["node"].get("client").is_none(), "{b}");
        assert_eq!(b["node"]["configured"], false);
        assert!(org_rows(&vault).is_empty());
        assert_eq!(store.node_client("filebox").unwrap(), None);
        // With no client, Enable is refused again.
        let (s, b) = put(serde_json::json!({ "enabled": true })).await;
        assert_eq!(s, StatusCode::CONFLICT, "{b}");
        assert!(b.to_string().contains("client id"), "{b}");
    }
}
