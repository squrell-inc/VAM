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
use crate::{connector_client, connector_discovery, connector_exchange};
use vam_spec::v1::VamErrorCode;

/// The scope of the connection routes: a member manages their own connection like a secret.
const CONNECTION_SCOPE: &str = "secrets:write";

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/connectors", get(list_connectors))
        .route("/v1/connectors/{id}/node", put(put_node))
        .route(
            "/v1/connectors/{id}/connection",
            put(put_connection).delete(delete_connection),
        )
        .route(
            "/v1/connectors/{id}/connection/check",
            post(check_connection),
        )
        .route(
            "/v1/connectors/{id}/connection/oauth/code",
            post(exchange_code),
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
    // The plan was made from `def` and `existing_client`, before discovery ran: the write checks both
    // again inside its transaction and refuses (409, nothing written) if another admin's save moved
    // either in the meantime.
    let cleared = state.store.put_node_with_client(
        &state.vault,
        &row,
        &plan,
        &mcp_url,
        &connector_client::PlanBasis {
            def: &def,
            client: existing_client.as_ref(),
        },
    )?;
    // Members whose refresh the provider had refused for this client are connected again (the same
    // client, a corrected secret, or the enable after a save that corrected it with a disable):
    // their reconnect notification is resolved and parked runs go on. A save that leaves the
    // connector off clears no one, so nothing is resumed on a disabled connector.
    for owner in &cleared {
        if let Ok(conn) = state
            .store
            .get_mcp_connection(SecretScope::User, owner, &id)
        {
            super::reauth::resume_parked(&state, &conn).await;
        }
    }
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
/// client secret, and a serde error can quote the value it rejected (before the body is even known
/// to be valid JSON), so every parse failure gets the same fixed message.
fn parse_put_node(body: &str) -> Result<PutNodeRequest, OrchestratorError> {
    serde_json::from_str(body).map_err(|_| {
        OrchestratorError::InvalidContract(
            "connector node body must be { enabled[, consent][, client: { clientId[, clientSecret] } | null] }"
                .into(),
        )
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

/// `POST /v1/connectors/{id}/connection/oauth/code`: the node exchanges the authorization code the
/// app got from the browser sign-in (CN-5.3, D29).
///
/// For a connector with `client: org` and `clientSecret: org_vault`, the enabled connector's saved
/// client and nothing else: the body is `{ code, codeVerifier, redirectUri }` and names no endpoint
/// or client. The node posts the code with the org secret to the token endpoint it pinned at Enable
/// and connects the caller. Every refusal is decided before anything is sent or written; a provider
/// that refuses the client (`invalid_client`) is a 409 naming the admin's client, a refused code a
/// 400, an unreachable provider a 502. The answer is the caller's list entry: no code, verifier,
/// secret or token is in it, or in any error.
async fn exchange_code(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: String,
) -> Result<Response, ApiError> {
    ensure_ready(&state, true)?;
    state
        .pdp
        .authorize(&auth, "ExchangeConnectorCode", CONNECTION_SCOPE)?;
    let idempotency = require_idempotency_key(&headers).map_err(ApiError::from)?;
    let found = entry_or_404(&state, &id)?;
    // A fixed message: a serde error can quote the value it rejected, and this body holds a code.
    let req: connector_client::CodeExchangeRequest = serde_json::from_str(&body).map_err(|_| {
        OrchestratorError::InvalidContract(
            "code exchange body must be { code, codeVerifier, redirectUri }".into(),
        )
    })?;
    let org = state
        .store
        .org_connector(&id)?
        .filter(|o| o.keeps_secret)
        .ok_or_else(|| {
            OrchestratorError::InvalidContract(format!(
                "`{id}` has no code exchange on the node: it signs in with the app's own exchange"
            ))
        })?;
    // Enabled on this node, its own URL.
    connectors::gate_mcp_write(&state.store, &id, None, None)?;
    let client = org.client.clone().ok_or_else(|| {
        OrchestratorError::Conflict(format!(
            "`{id}` has no OAuth client saved by your admin yet"
        ))
    })?;
    if !client.secret_set {
        return Err(OrchestratorError::Conflict(format!(
            "`{id}` has no client secret saved by your admin yet"
        ))
        .into());
    }
    connector_client::check_code_exchange(&req)?;
    let owner = vault_owner_user_id(SecretScope::User, auth.owner_user_id()).to_string();
    // The connection's credential gate: a refresh of this member's row does not interleave with the
    // exchange, and a call that began with the old tokens retries with the new ones.
    let mut gate =
        crate::host_tools::lock_mcp_credentials_for(&state.store, SecretScope::User, &owner, &id)
            .await
            .map_err(|_| {
                OrchestratorError::CorruptState("mcp credential lock unavailable".into())
            })?;
    let (store, vault) = (state.store.clone(), state.vault.clone());
    let (creator, id2) = (auth.owner_user_id().to_string(), id.clone());
    // The exchange and every write after it run to the end on a worker that owns the gate, so a
    // caller that goes away cannot leave a spent code or refresh token unstored.
    let connected = tokio::task::spawn_blocking(move || {
        let c = connector_exchange::CodeConnect {
            owner: &owner,
            creator: &creator,
            connector_id: &id2,
            org: &org,
            client: &client,
            request: &req,
            idempotency_key: &idempotency,
        };
        connector_exchange::exchange_and_connect(&store, &vault, &c, &mut gate)
    })
    .await
    .map_err(|_| {
        ApiError::from(OrchestratorError::Core(
            "connector code exchange task failed".into(),
        ))
    })??;
    // The row was committed tools-pending. Tools are listed with the member's new token and the
    // vetted ones stay, which makes it `connected`; the reconnect (notification, parked runs)
    // follows once they are in. If they cannot be read, or this request is dropped before this
    // point, the row stays in the pending state the tools-refresh route finishes (see
    // `adopt_tools_and_reconnect`).
    if let Some(policy) = state.store.mcp_policy(&id)? {
        super::mcp_connections::adopt_tools_and_reconnect(
            &state,
            &connected.conn,
            &policy,
            &connected.credentials,
        )
        .await?;
    }
    Ok(json_http_response(
        StatusCode::OK,
        entry_for(&state, &auth, &found)?.to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    use crate::error::OrchestratorError;
    use crate::secrets::SecretScope;
    use crate::{connector_client, connector_exchange};
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt as _;
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
        call_with_key(app, method, uri, secret, body, "connectors-1").await
    }

    /// A fresh Idempotency-Key: every code exchange of a test is its own sign-in unless the test
    /// repeats one on purpose.
    fn next_idempotency_key() -> String {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        format!(
            "exchange-{}",
            NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        )
    }

    async fn call_with_key(
        app: &axum::Router,
        method: &str,
        uri: &str,
        secret: &str,
        body: Option<serde_json::Value>,
        key: &str,
    ) -> (StatusCode, serde_json::Value) {
        let b = Request::builder()
            .method(method)
            .uri(uri)
            .header("authorization", format!("Bearer {secret}"))
            .header("idempotency-key", key)
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
        let (app, state) = app_world_with_state();
        (app, state.store.clone(), state.vault.clone())
    }

    fn app_world_with_state() -> (axum::Router, AppState) {
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
        let state = AppState {
            store: store.clone(),
            vault: vault.clone(),
            manager,
            pdp: Arc::new(ControlPlanePdp::new().unwrap()),
            listen_addr: "127.0.0.1:0".into(),
            draining: Arc::new(AtomicBool::new(false)),
            ready: Arc::new(AtomicBool::new(true)),
            e2e_mock_scripts: false,
        };
        (router(state.clone()), state)
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
        // A new client id without its secret is refused and changes nothing: a secret belongs to
        // the client it was registered with.
        let (s, b) = put(client_body("c-2", None)).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
        assert!(b.to_string().contains("secret"), "{b}");
        assert_eq!(status(), McpOauthStatus::Connected);
        assert_eq!(org_secret_value(&vault), "A-NEW-SECRET-yyyyyyyy");
        // A new client id with its secret: members must connect again.
        let (s, b) = put(client_body("c-2", Some("C2-SECRET-zzzzzzzzzzzz"))).await;
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

    // ---- the node's code exchange (CN-5.3) -----------------------------------------------------

    use std::collections::{BTreeMap, VecDeque};
    use std::io::{BufRead, BufReader, Read, Write};
    use std::sync::Mutex;

    const CODE: &str = "AUTH-CODE-4f8a2c";
    const VERIFIER: &str = "VERIFIER-0123456789-abcdefghijklmnopqrstuvwxyz-ABCDEFGH";
    const NEW_ACCESS: &str = "NEW_ACCESS-TOKEN-b71d";
    const NEW_REFRESH: &str = "NEW_REFRESH-TOKEN-c93e";

    /// One request the loopback provider saw.
    #[derive(Debug, Clone)]
    struct Seen {
        path: String,
        form: BTreeMap<String, String>,
        /// The `Authorization` header, when the request had one (`client_secret_basic`).
        authorization: Option<String>,
    }

    /// A loopback authorization server: records every request, answers from a queue of
    /// `(status, body)` (a 500 when the queue is empty).
    struct Provider {
        base: url::Url,
        seen: Arc<Mutex<Vec<Seen>>>,
        replies: Arc<Mutex<VecDeque<(u16, String)>>>,
        hold: HeldReply,
    }

    /// The next request's answer is held: the provider says the request arrived on the first
    /// channel, then waits on the second before it answers.
    type HeldReply =
        Arc<Mutex<Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>>>;

    fn provider() -> Provider {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = url::Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let seen = Arc::new(Mutex::new(Vec::<Seen>::new()));
        let replies = Arc::new(Mutex::new(VecDeque::<(u16, String)>::new()));
        let hold: HeldReply = Arc::new(Mutex::new(None));
        let (log, queue, held) = (seen.clone(), replies.clone(), hold.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let path = line
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or_default()
                    .to_string();
                let mut length = 0usize;
                let mut authorization = None;
                loop {
                    line.clear();
                    reader.read_line(&mut line).unwrap();
                    if line.trim_end().is_empty() {
                        break;
                    }
                    if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = v.trim().parse().unwrap();
                    }
                    if line.to_ascii_lowercase().starts_with("authorization:") {
                        authorization = line.split_once(':').map(|(_, v)| v.trim().to_string());
                    }
                }
                let mut body = vec![0u8; length];
                reader.read_exact(&mut body).unwrap();
                let form = url::form_urlencoded::parse(&body)
                    .map(|(k, v)| (k.into_owned(), v.into_owned()))
                    .collect();
                log.lock().unwrap().push(Seen {
                    path,
                    form,
                    authorization,
                });
                let pending = held.lock().unwrap().take();
                if let Some((arrived, release)) = pending {
                    let _ = arrived.send(());
                    let _ = release.recv();
                }
                let (status, payload) = queue
                    .lock()
                    .unwrap()
                    .pop_front()
                    .unwrap_or((500, String::new()));
                let reply = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                    payload.len()
                );
                stream.write_all(reply.as_bytes()).unwrap();
            }
        });
        Provider {
            base,
            seen,
            replies,
            hold,
        }
    }

    impl Provider {
        /// Holds the answer to the next request: returns "it arrived" and "answer now".
        fn hold_next(&self) -> (std::sync::mpsc::Receiver<()>, std::sync::mpsc::Sender<()>) {
            let (arrived_tx, arrived_rx) = std::sync::mpsc::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            *self.hold.lock().unwrap() = Some((arrived_tx, release_rx));
            (arrived_rx, release_tx)
        }
        fn reply(&self, status: u16, body: serde_json::Value) {
            let body = if body.is_null() {
                String::new()
            } else {
                body.to_string()
            };
            self.replies.lock().unwrap().push_back((status, body));
        }
        fn requests(&self) -> Vec<Seen> {
            self.seen.lock().unwrap().clone()
        }
    }

    fn tokens_reply() -> serde_json::Value {
        serde_json::json!({
            "access_token": NEW_ACCESS, "refresh_token": NEW_REFRESH,
            "token_type": "Bearer", "expires_in": 3600
        })
    }

    fn exchange_body(redirect: &str) -> serde_json::Value {
        serde_json::json!({ "code": CODE, "codeVerifier": VERIFIER, "redirectUri": redirect })
    }

    /// Nothing the app or the member could see carries one of the values a sign-in handles.
    fn assert_no_value(what: &str, text: &str) {
        for needle in [
            SECRET,
            &SecretsVault::fingerprint(SECRET),
            CODE,
            VERIFIER,
            NEW_ACCESS,
            NEW_REFRESH,
            "ORG-CLIENT-SECRET",
        ] {
            assert!(!text.contains(needle), "{needle} in {what}: {text}");
        }
    }

    fn member_vault_ids(vault: &SecretsVault) -> Vec<String> {
        vault
            .list_metadata()
            .unwrap()
            .into_iter()
            .map(|m| m.binding_id)
            .filter(|id| id.starts_with("filebox/"))
            .collect()
    }

    struct Node {
        app: axum::Router,
        state: AppState,
        store: Arc<SqliteWorkflowStore>,
        vault: Arc<SecretsVault>,
        token: Provider,
        mcp_seen: Arc<Mutex<crate::mcp_client::mock::Seen>>,
    }

    /// `filebox` installed, an app, a loopback provider for `auth.<tag>.example` and a loopback MCP
    /// server for `mcp.<tag>.example`. Not enabled yet.
    fn xchg_node(tag: &str) -> Node {
        xchg_node_with(tag, Arc::new(AtomicBool::new(true)))
    }

    /// [`xchg_node`] whose MCP server refuses `tools/list` while `tools_ok` is false.
    fn xchg_node_with(tag: &str, tools_ok: Arc<AtomicBool>) -> Node {
        xchg_node_rejecting(tag, tools_ok, Arc::new(Mutex::new(Vec::new())))
    }

    /// [`xchg_node_with`] whose MCP server answers HTTP 401 to any request whose bearer token is in
    /// `rejected`.
    fn xchg_node_rejecting(
        tag: &str,
        tools_ok: Arc<AtomicBool>,
        rejected: Arc<Mutex<Vec<String>>>,
    ) -> Node {
        let (app, state) = app_world_with_state();
        let (store, vault) = (state.store.clone(), state.vault.clone());
        install_filebox(&store, tag);
        let token = provider();
        crate::mcp_oauth_refresh::test_seam::serve(
            &format!("auth.{tag}.example"),
            token.base.clone(),
        );
        let seen_slot: Arc<std::sync::OnceLock<Arc<Mutex<crate::mcp_client::mock::Seen>>>> =
            Arc::new(std::sync::OnceLock::new());
        let slot = seen_slot.clone();
        let (mcp_url, mcp_seen) =
            crate::mcp_client::mock::spawn_with_status(move |method, id, params| {
                let auth = slot
                    .get()
                    .and_then(|seen| seen.lock().unwrap().auth.last().cloned().flatten());
                let refused = auth.is_some_and(|a| {
                    rejected
                        .lock()
                        .unwrap()
                        .iter()
                        .any(|t| a == format!("Bearer {t}"))
                });
                if refused {
                    return Some((401, "application/json".into(), "{}".into()));
                }
                with_status_ok(xchg_mcp_reply(&tools_ok, method, id, params))
            });
        let _ = seen_slot.set(mcp_seen.clone());
        crate::mcp_oauth_refresh::test_seam::serve(
            &format!("mcp.{tag}.example"),
            url::Url::parse(&mcp_url).unwrap(),
        );
        Node {
            app,
            state,
            store,
            vault,
            token,
            mcp_seen,
        }
    }

    fn with_status_ok(reply: Option<(String, String)>) -> Option<(u16, String, String)> {
        reply.map(|(ct, body)| (200, ct, body))
    }

    /// The `filebox` MCP server of [`xchg_node_with`].
    fn xchg_mcp_reply(
        tools_ok: &AtomicBool,
        method: &str,
        id: &serde_json::Value,
        params: &serde_json::Value,
    ) -> Option<(String, String)> {
        if method == "tools/list" {
            if !tools_ok.load(std::sync::atomic::Ordering::SeqCst) {
                return Some((
                    "application/json".into(),
                    serde_json::json!({ "jsonrpc": "2.0", "id": id,
                        "error": { "code": -32000, "message": "tools are down" } })
                    .to_string(),
                ));
            }
            return crate::mcp_client::mock::ok(
                id,
                serde_json::json!({ "tools": [
                    { "name": "search_files", "inputSchema": { "type": "object" } },
                    { "name": "delete_everything", "inputSchema": { "type": "object" } }
                ] }),
            );
        }
        crate::mcp_client::mock::standard(method, id, params)
    }

    async fn exchange(
        n: &Node,
        who: &str,
        id: &str,
        body: serde_json::Value,
    ) -> (StatusCode, serde_json::Value) {
        exchange_with_key(n, who, id, body, &next_idempotency_key()).await
    }

    async fn exchange_with_key(
        n: &Node,
        who: &str,
        id: &str,
        body: serde_json::Value,
        key: &str,
    ) -> (StatusCode, serde_json::Value) {
        call_with_key(
            &n.app,
            "POST",
            &format!("/v1/connectors/{id}/connection/oauth/code"),
            who,
            Some(body),
            key,
        )
        .await
    }

    #[tokio::test]
    async fn a_repeated_exchange_with_the_same_key_never_sends_its_code_again() {
        let n = xchg_node("xchg-idem");
        let (s, b) = put_client(&n, "c-1", Some(SECRET)).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        // The first exchange succeeds; the app loses the answer and retries with the same key.
        n.token.reply(200, tokens_reply());
        let (s, b) = exchange_with_key(
            &n,
            "member-secret",
            "filebox",
            exchange_body("squrell://oauth"),
            "sign-in-1",
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(n.token.requests().len(), 1);
        let connected = row_of(&n, "ada");
        let refresh = token_of(&n, "ada", "filebox/oauth/refresh-token");
        // The provider would refuse the reused code (and may revoke the first tokens); the node
        // does not present it at all: a 409 saying to read the connection state, nothing changed.
        n.token.reply(
            400,
            serde_json::json!({ "error": "invalid_grant", "error_description": "code reused" }),
        );
        let (s, b) = exchange_with_key(
            &n,
            "member-secret",
            "filebox",
            exchange_body("squrell://oauth"),
            "sign-in-1",
        )
        .await;
        assert_eq!(s, StatusCode::CONFLICT, "{b}");
        assert!(b.to_string().contains("already submitted"), "{b}");
        assert_eq!(b["retryable"], false, "{b}");
        assert_no_value("the repeat", &b.to_string());
        assert_eq!(n.token.requests().len(), 1, "the code was sent again");
        assert_eq!(row_of(&n, "ada").oauth, connected.oauth);
        assert_eq!(token_of(&n, "ada", "filebox/oauth/refresh-token"), refresh);
        // A first answer that was a failure after the send is not replayed either: the code may be
        // spent. A request refused before the send (here: the connector is off) records nothing,
        // so the same key goes through once the connector is back.
        let (s, b) = call(
            &n.app,
            "PUT",
            "/v1/connectors/filebox/node",
            "orgadmin-secret",
            Some(serde_json::json!({ "enabled": false })),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        let (s, b) = exchange_with_key(
            &n,
            "member-secret",
            "filebox",
            exchange_body("squrell://oauth"),
            "sign-in-2",
        )
        .await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{b}");
        let (s, b) = call(
            &n.app,
            "PUT",
            "/v1/connectors/filebox/node",
            "orgadmin-secret",
            Some(serde_json::json!({ "enabled": true })),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        // A new sign-in with its own key still works (the queued refusal is its answer first).
        let (s, b) = exchange_with_key(
            &n,
            "member-secret",
            "filebox",
            exchange_body("squrell://oauth"),
            "sign-in-2",
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
        assert_eq!(n.token.requests().len(), 2);
        n.token.reply(200, tokens_reply());
        let (s, b) = exchange_with_key(
            &n,
            "member-secret",
            "filebox",
            exchange_body("squrell://oauth"),
            "sign-in-3",
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(n.token.requests().len(), 3);
        // Another member's key space is their own: the same key is a new sign-in for olga.
        n.token.reply(200, tokens_reply());
        let (s, b) = exchange_with_key(
            &n,
            "orgadmin-secret",
            "filebox",
            exchange_body("squrell://oauth"),
            "sign-in-1",
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(n.token.requests().len(), 4);
        // And sign-in-2, whose code was sent and refused, is not sent again either.
        let (s, b) = exchange_with_key(
            &n,
            "member-secret",
            "filebox",
            exchange_body("squrell://oauth"),
            "sign-in-2",
        )
        .await;
        assert_eq!(s, StatusCode::CONFLICT, "{b}");
        assert_eq!(n.token.requests().len(), 4);
    }

    #[tokio::test]
    async fn every_refusal_of_the_exchange_is_decided_before_anything_is_sent_or_written() {
        let n = xchg_node("xchg-refuse");
        // Not enabled: refused.
        let (s, b) = exchange(
            &n,
            "member-secret",
            "filebox",
            exchange_body("squrell://oauth"),
        )
        .await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{b}");
        // Enabled with the admin's client.
        let (s, b) = call(
            &n.app,
            "PUT",
            "/v1/connectors/filebox/node",
            "orgadmin-secret",
            Some(client_body("c-1", Some(SECRET))),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        n.token.reply(200, tokens_reply());

        let with = |extra: (&str, &str)| {
            let mut b = exchange_body("squrell://oauth");
            b[extra.0] = serde_json::json!(extra.1);
            b
        };
        let mut refused = vec![
            // The body names no endpoint and no client.
            (
                with(("tokenEndpoint", "https://evil.example/token")),
                StatusCode::BAD_REQUEST,
            ),
            (with(("clientId", "someone-elses")), StatusCode::BAD_REQUEST),
            (with(("clientSecret", "mine")), StatusCode::BAD_REQUEST),
            // A redirect URI that is not the app's.
            (
                exchange_body("https://evil.example/callback"),
                StatusCode::BAD_REQUEST,
            ),
            (
                exchange_body("http://localhost:9/cb"),
                StatusCode::BAD_REQUEST,
            ),
            // A verifier that is not RFC 7636's, an empty code, a missing field.
            (
                serde_json::json!({ "code": CODE, "codeVerifier": "short", "redirectUri": "squrell://oauth" }),
                StatusCode::BAD_REQUEST,
            ),
            (
                serde_json::json!({ "code": "", "codeVerifier": VERIFIER, "redirectUri": "squrell://oauth" }),
                StatusCode::BAD_REQUEST,
            ),
            (serde_json::json!({ "code": CODE }), StatusCode::BAD_REQUEST),
        ];
        refused.push((serde_json::json!("not an object"), StatusCode::BAD_REQUEST));
        for (body, want) in refused {
            let (s, b) = exchange(&n, "member-secret", "filebox", body).await;
            assert_eq!(s, want, "{b}");
            assert_no_value("a refusal", &b.to_string());
        }
        // A connector that uses the publisher's own client has no node exchange; an unknown id is
        // a 404.
        let (s, b) = exchange(
            &n,
            "member-secret",
            "linear",
            exchange_body("squrell://oauth"),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
        let (s, _) = exchange(
            &n,
            "member-secret",
            "nope",
            exchange_body("squrell://oauth"),
        )
        .await;
        assert_eq!(s, StatusCode::NOT_FOUND);
        // Nothing was sent to the provider and nothing was written for the member.
        assert!(n.token.requests().is_empty(), "{:?}", n.token.requests());
        assert!(n
            .store
            .get_mcp_connection(crate::secrets::SecretScope::User, "ada", "filebox")
            .is_err());
        assert!(member_vault_ids(&n.vault).is_empty());
    }

    #[tokio::test]
    async fn the_node_exchanges_the_code_with_the_org_secret_and_connects_the_member() {
        let n = xchg_node("xchg-ok");
        let (s, b) = call(
            &n.app,
            "PUT",
            "/v1/connectors/filebox/node",
            "orgadmin-secret",
            Some(client_body("c-1", Some(SECRET))),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");

        n.token.reply(200, tokens_reply());
        let (s, b) = exchange(
            &n,
            "member-secret",
            "filebox",
            exchange_body("squrell://oauth"),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(b["state"], "connected", "{b}");
        assert_no_value("the exchange answer", &b.to_string());

        // One request to the pinned token endpoint, with exactly these fields.
        let seen = n.token.requests();
        assert_eq!(seen.len(), 1, "{seen:?}");
        assert_eq!(seen[0].path, "/token");
        let f = &seen[0].form;
        assert_eq!(
            f.keys().map(String::as_str).collect::<Vec<_>>(),
            [
                "client_id",
                "client_secret",
                "code",
                "code_verifier",
                "grant_type",
                "redirect_uri",
                "resource"
            ]
        );
        assert_eq!(f["grant_type"], "authorization_code");
        assert_eq!(f["client_id"], "c-1");
        assert_eq!(f["client_secret"], SECRET);
        assert_eq!(f["code"], CODE);
        assert_eq!(f["code_verifier"], VERIFIER);
        assert_eq!(f["redirect_uri"], "squrell://oauth");
        assert_eq!(f["resource"], "https://mcp.xchg-ok.example/mcp");

        // The member's row: the node's client and endpoints, only the vetted tool, tokens in the
        // member's own vault rows.
        let row = n
            .store
            .get_mcp_connection(crate::secrets::SecretScope::User, "ada", "filebox")
            .unwrap();
        let oauth = row.oauth.clone().unwrap();
        assert_eq!(row.url, "https://mcp.xchg-ok.example/mcp");
        assert_eq!(oauth.client_id, "c-1");
        assert_eq!(oauth.token_endpoint, "https://auth.xchg-ok.example/token");
        assert_eq!(oauth.authorization_server, "https://auth.xchg-ok.example");
        assert_eq!(
            oauth.revocation_endpoint.as_deref(),
            Some("https://auth.xchg-ok.example/revoke")
        );
        assert_eq!(
            oauth.registration_method,
            crate::mcp_registry::McpOauthRegistrationMethod::Manual
        );
        assert_eq!(oauth.status, crate::mcp_registry::McpOauthStatus::Connected);
        assert!(oauth.expires_at.is_some());
        assert_eq!(
            row.tools
                .iter()
                .map(|t| t.name.as_str())
                .collect::<Vec<_>>(),
            ["search_files"]
        );
        let at = |id: &str| {
            secrecy::ExposeSecret::expose_secret(
                &n.vault
                    .decrypt_secret_at(crate::secrets::SecretScope::User, "ada", id)
                    .unwrap(),
            )
            .clone()
        };
        assert_eq!(at("filebox/oauth/access-token"), NEW_ACCESS);
        assert_eq!(at("filebox/oauth/refresh-token"), NEW_REFRESH);
        // No per-row client secret: the org's is the only one.
        assert!(!n
            .vault
            .has_binding_at(
                crate::secrets::SecretScope::User,
                "ada",
                "filebox/oauth/client-secret"
            )
            .unwrap());
        // The tool list was read with the member's new token.
        let auth: Vec<_> = n.mcp_seen.lock().unwrap().auth.clone();
        assert!(
            auth.iter()
                .any(|a| a.as_deref() == Some(&format!("Bearer {NEW_ACCESS}"))),
            "{auth:?}"
        );

        // No listing, for anyone, carries a secret, a code, a verifier or a token.
        for who in ["member-secret", "orgadmin-secret", "operator-secret"] {
            for uri in [
                "/v1/connectors",
                "/v1/mcp/connections",
                "/v1/secrets",
                "/v1/secrets?scope=org",
                "/v1/org/secrets",
            ] {
                let (s, b) = call(&n.app, "GET", uri, who, None).await;
                // The org's secrets list is the admin's; a member is refused it.
                let want = if uri == "/v1/org/secrets" && who == "member-secret" {
                    StatusCode::FORBIDDEN
                } else {
                    StatusCode::OK
                };
                assert_eq!(s, want, "{uri} {b}");
                assert_no_value(uri, &b.to_string());
            }
        }

        // The app that holds tokens itself is refused: tokens, a client secret, another endpoint,
        // another client id, a static connect. Nothing changes.
        let before = (
            at("filebox/oauth/access-token"),
            at("filebox/oauth/refresh-token"),
        );
        let (s, b) = call(
            &n.app,
            "PUT",
            "/v1/mcp/connections/filebox/oauth/tokens",
            "member-secret",
            Some(serde_json::json!({ "accessToken": "app-a", "refreshToken": "app-r" })),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
        assert!(b.to_string().contains("exchange"), "{b}");
        let (s, b) = call(
            &n.app,
            "PUT",
            "/v1/mcp/connections/filebox/oauth/tokens",
            "member-secret",
            Some(serde_json::json!({
                "accessToken": "app-a", "refreshToken": "app-r", "clientSecret": "app-secret"
            })),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
        let connect = |token_endpoint: &str, client_id: &str, mode: &str| {
            serde_json::json!({
                "connectionId": "filebox", "url": "https://mcp.xchg-ok.example/mcp",
                "authMode": mode,
                "oauth": {
                    "resource": "https://mcp.xchg-ok.example/mcp",
                    "authorizationServer": "https://evil.example",
                    "revocationEndpoint": "https://evil.example/revoke",
                    "tokenEndpoint": token_endpoint, "clientId": client_id,
                    "registrationMethod": "dcr", "scopes": []
                }
            })
        };
        for body in [
            connect("https://evil.example/token", "c-1", "oauth"),
            connect(
                "https://auth.xchg-ok.example/token",
                "someone-elses",
                "oauth",
            ),
            connect("https://auth.xchg-ok.example/token", "c-1", "static"),
        ] {
            let (s, b) = call(
                &n.app,
                "PUT",
                "/v1/mcp/connections",
                "member-secret",
                Some(body),
            )
            .await;
            assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
        }
        // The node's own client and endpoint pass, and the rest of the block is the node's whatever
        // the request said.
        let (s, b) = call(
            &n.app,
            "PUT",
            "/v1/mcp/connections",
            "member-secret",
            Some(connect(
                "https://auth.xchg-ok.example/token",
                "c-1",
                "oauth",
            )),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        let oauth = n
            .store
            .get_mcp_connection(crate::secrets::SecretScope::User, "ada", "filebox")
            .unwrap()
            .oauth
            .unwrap();
        assert_eq!(oauth.authorization_server, "https://auth.xchg-ok.example");
        assert_eq!(
            oauth.revocation_endpoint.as_deref(),
            Some("https://auth.xchg-ok.example/revoke")
        );
        assert_eq!(
            oauth.registration_method,
            crate::mcp_registry::McpOauthRegistrationMethod::Manual
        );
        assert_eq!(
            before,
            (
                at("filebox/oauth/access-token"),
                at("filebox/oauth/refresh-token")
            )
        );

        // A disconnect revokes the refresh token at the node's endpoint with the org secret. The
        // provider's metadata omits `revocation_endpoint_auth_methods_supported`, so the revoke
        // uses RFC 8414's default, HTTP Basic (the token endpoint takes POST).
        n.token.reply(200, serde_json::Value::Null);
        let (s, b) = call(
            &n.app,
            "DELETE",
            "/v1/mcp/connections/filebox",
            "member-secret",
            None,
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_no_value("the disconnect answer", &b.to_string());
        let seen = n.token.requests();
        let revoke = seen.last().unwrap();
        assert_eq!(revoke.path, "/revoke");
        assert_eq!(revoke.form["token"], NEW_REFRESH);
        let expected = {
            use base64::Engine as _;
            format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode(format!("c-1:{SECRET}"))
            )
        };
        assert_eq!(revoke.authorization.as_deref(), Some(expected.as_str()));
        assert!(
            !revoke.form.contains_key("client_secret"),
            "{:?}",
            revoke.form
        );
        assert!(member_vault_ids(&n.vault).is_empty());
    }

    #[tokio::test]
    async fn a_provider_that_refuses_the_exchange_writes_nothing_and_names_what_to_fix() {
        let n = xchg_node("xchg-refused");
        let (s, b) = call(
            &n.app,
            "PUT",
            "/v1/connectors/filebox/node",
            "orgadmin-secret",
            Some(client_body("c-1", Some(SECRET))),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        let cases = [
            // The provider refuses the admin's client: a 409 naming it.
            (
                401,
                serde_json::json!({ "error": "invalid_client", "error_description": SECRET }),
                StatusCode::CONFLICT,
                "client id or secret",
            ),
            // The code is spent, expired or not this verifier's.
            (
                400,
                serde_json::json!({ "error": "invalid_grant", "error_description": CODE }),
                StatusCode::BAD_REQUEST,
                "sign in again",
            ),
            // The provider is down.
            (
                503,
                serde_json::json!({ "error": "temporarily_unavailable" }),
                StatusCode::BAD_GATEWAY,
                "exchange",
            ),
            // The provider answers 200 with no refresh token: the connection could not be kept,
            // and repeating the request cannot help (the code is spent, and the provider must be
            // set up to issue a refresh token first).
            (
                200,
                serde_json::json!({ "access_token": NEW_ACCESS, "token_type": "Bearer" }),
                StatusCode::BAD_GATEWAY,
                "offline access",
            ),
        ];
        for (provider_status, provider_body, want, text) in cases {
            n.token.reply(provider_status, provider_body);
            let (s, b) = exchange(
                &n,
                "member-secret",
                "filebox",
                exchange_body("squrell://oauth"),
            )
            .await;
            assert_eq!(s, want, "{b}");
            assert!(b.to_string().contains(text), "{b}");
            if want == StatusCode::BAD_GATEWAY {
                // Only an unreachable provider is worth retrying.
                assert_eq!(b["retryable"], text == "exchange", "{b}");
            }
            assert_no_value("a refused exchange", &b.to_string());
            assert!(
                n.store
                    .get_mcp_connection(crate::secrets::SecretScope::User, "ada", "filebox")
                    .is_err(),
                "nothing was written"
            );
            assert!(member_vault_ids(&n.vault).is_empty());
        }
        assert_eq!(n.token.requests().len(), 4);
        // The tile still reads `connect`.
        let (_, l) = call(&n.app, "GET", "/v1/connectors", "member-secret", None).await;
        assert_eq!(state_of(&l, "filebox"), "connect");
    }

    #[tokio::test]
    async fn a_member_cannot_exchange_for_someone_else_and_a_reconnect_replaces_the_tokens() {
        let n = xchg_node("xchg-twice");
        let (s, b) = call(
            &n.app,
            "PUT",
            "/v1/connectors/filebox/node",
            "orgadmin-secret",
            Some(client_body("c-1", Some(SECRET))),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        n.token.reply(200, tokens_reply());
        let (s, _) = exchange(
            &n,
            "member-secret",
            "filebox",
            exchange_body("squrell://oauth"),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        // The admin is another user: their own connection is theirs, and ada's is untouched.
        n.token.reply(
            200,
            serde_json::json!({
                "access_token": "ADMIN-A", "refresh_token": "ADMIN-R", "expires_in": 60
            }),
        );
        let (s, b) = exchange(
            &n,
            "orgadmin-secret",
            "filebox",
            exchange_body("http://127.0.0.1:51000/oauth"),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        let token_of = |owner: &str, id: &str| {
            secrecy::ExposeSecret::expose_secret(
                &n.vault
                    .decrypt_secret_at(crate::secrets::SecretScope::User, owner, id)
                    .unwrap(),
            )
            .clone()
        };
        assert_eq!(token_of("ada", "filebox/oauth/refresh-token"), NEW_REFRESH);
        assert_eq!(token_of("olga", "filebox/oauth/refresh-token"), "ADMIN-R");
        // Ada signs in again: her tokens are replaced, the gate's generation moves.
        n.token.reply(
            200,
            serde_json::json!({
                "access_token": "ADA-A2", "refresh_token": "ADA-R2", "expires_in": 60
            }),
        );
        let (s, b) = exchange(
            &n,
            "member-secret",
            "filebox",
            exchange_body("squrell://oauth"),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(token_of("ada", "filebox/oauth/refresh-token"), "ADA-R2");
        assert_eq!(token_of("ada", "filebox/oauth/access-token"), "ADA-A2");
        assert_eq!(token_of("olga", "filebox/oauth/refresh-token"), "ADMIN-R");
    }

    // ---- the release review of #52: what the exchange commits, and when ------------------------

    async fn put_client(
        n: &Node,
        id: &str,
        secret: Option<&str>,
    ) -> (StatusCode, serde_json::Value) {
        call(
            &n.app,
            "PUT",
            "/v1/connectors/filebox/node",
            "orgadmin-secret",
            Some(client_body(id, secret)),
        )
        .await
    }

    fn row_of(n: &Node, who: &str) -> crate::mcp_registry::McpConnection {
        n.store
            .get_mcp_connection(crate::secrets::SecretScope::User, who, "filebox")
            .unwrap()
    }

    fn token_of(n: &Node, who: &str, binding: &str) -> String {
        secrecy::ExposeSecret::expose_secret(
            &n.vault
                .decrypt_secret_at(crate::secrets::SecretScope::User, who, binding)
                .unwrap(),
        )
        .clone()
    }

    fn open_notices(n: &Node, who: &str) -> i64 {
        n.store
            .connection
            .lock()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM notifications WHERE kind = 'connection.needs_reauth'
                 AND user_id = ?1 AND resolved_at IS NULL",
                [who],
                |r| r.get(0),
            )
            .unwrap()
    }

    /// The exchange of `who`'s code made with `stale` as the client the route read before an admin
    /// changed it: what a client replaced while the provider was answering looks like to the commit.
    async fn exchange_made_with(
        n: &Node,
        who: &str,
        stale: crate::connector_client::NodeClient,
    ) -> Result<connector_exchange::Connected, crate::api::error::ApiError> {
        let org = n.store.org_connector("filebox").unwrap().unwrap();
        let mut gate = crate::host_tools::lock_mcp_credentials_for(
            &n.store,
            SecretScope::User,
            who,
            "filebox",
        )
        .await
        .unwrap();
        let (store, vault, who) = (n.store.clone(), n.vault.clone(), who.to_string());
        tokio::task::spawn_blocking(move || {
            let req = connector_client::CodeExchangeRequest {
                code: CODE.into(),
                code_verifier: VERIFIER.into(),
                redirect_uri: "squrell://oauth".into(),
            };
            let c = connector_exchange::CodeConnect {
                owner: &who,
                creator: &who,
                connector_id: "filebox",
                org: &org,
                client: &stale,
                request: &req,
                idempotency_key: &next_idempotency_key(),
            };
            connector_exchange::exchange_and_connect(&store, &vault, &c, &mut gate)
        })
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn an_exchange_for_a_client_the_admin_replaced_meanwhile_stores_nothing() {
        let n = xchg_node("xchg-superseded");
        let (s, b) = put_client(&n, "c-1", Some(SECRET)).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        let c1 = n.store.node_client("filebox").unwrap().unwrap();
        n.token.reply(200, tokens_reply());
        let (s, b) = exchange(
            &n,
            "member-secret",
            "filebox",
            exchange_body("squrell://oauth"),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        let before = (
            token_of(&n, "ada", "filebox/oauth/access-token"),
            token_of(&n, "ada", "filebox/oauth/refresh-token"),
        );

        // The admin replaces the client after the route read it for ada's next sign-in: her old
        // row is marked, and the exchange made with the old client's read sends nothing (the
        // secret now stored is c-2's).
        let (s, b) = put_client(&n, "c-2", Some("C2-SECRET-wwwwwwwwwwww")).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        let e = exchange_made_with(&n, "ada", c1.clone())
            .await
            .err()
            .unwrap();
        assert_eq!(e.status, StatusCode::CONFLICT, "{}", e.message);
        assert!(e.message.contains("sign in again"), "{}", e.message);
        // No request was sent (only the first sign-in's), and nothing was stored: the row still
        // needs a sign-in, with the tokens it had.
        assert_eq!(n.token.requests().len(), 1);
        let row = row_of(&n, "ada");
        assert_eq!(
            row.oauth.as_ref().unwrap().status,
            crate::mcp_registry::McpOauthStatus::NeedsReauth
        );
        assert_eq!(row.oauth.as_ref().unwrap().client_id, "c-1");
        assert_eq!(
            before,
            (
                token_of(&n, "ada", "filebox/oauth/access-token"),
                token_of(&n, "ada", "filebox/oauth/refresh-token"),
            )
        );
        assert_no_value("the refusal", &e.message);

        // A member who had no row gets none, and no vault row.
        let e = exchange_made_with(&n, "olga", c1).await.err().unwrap();
        assert!(e.message.contains("sign in again"), "{}", e.message);
        assert!(n
            .store
            .get_mcp_connection(SecretScope::User, "olga", "filebox")
            .is_err());
        assert!(!n
            .vault
            .has_binding_at(SecretScope::User, "olga", "filebox/oauth/access-token")
            .unwrap());

        // The connector turned off while the provider answered: the same.
        let current = n.store.node_client("filebox").unwrap().unwrap();
        let (s, b) = call(
            &n.app,
            "PUT",
            "/v1/connectors/filebox/node",
            "orgadmin-secret",
            Some(serde_json::json!({ "enabled": false })),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        n.token.reply(
            200,
            serde_json::json!({ "access_token": "LATE-A", "refresh_token": "LATE-R" }),
        );
        let e = exchange_made_with(&n, "olga", current).await.err().unwrap();
        assert_eq!(e.status, StatusCode::CONFLICT, "{}", e.message);
        assert!(n
            .store
            .get_mcp_connection(SecretScope::User, "olga", "filebox")
            .is_err());
    }

    #[tokio::test]
    async fn an_exchange_for_a_client_changed_and_changed_back_meanwhile_stores_nothing() {
        let n = xchg_node("xchg-aba");
        let (s, b) = put_client(&n, "c-1", Some(SECRET)).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        let c1 = n.store.node_client("filebox").unwrap().unwrap();
        n.token.reply(200, tokens_reply());
        let (s, b) = exchange(
            &n,
            "member-secret",
            "filebox",
            exchange_body("squrell://oauth"),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");

        // While ada's next sign-in is at the provider, the admin saves c-2 (her row is marked) and
        // then c-1 again: the client id and token endpoint are the ones the sign-in started with.
        let (s, b) = put_client(&n, "c-2", Some("C2-SECRET-wwwwwwwwwwww")).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        let (s, b) = put_client(&n, "c-1", Some(SECRET)).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        let back = n.store.node_client("filebox").unwrap().unwrap();
        assert_eq!(
            (&back.client_id, &back.token_endpoint),
            (&c1.client_id, &c1.token_endpoint)
        );
        assert!(back.revision > c1.revision, "{back:?} {c1:?}");
        let e = exchange_made_with(&n, "ada", c1).await.err().unwrap();
        assert_eq!(e.status, StatusCode::CONFLICT, "{}", e.message);
        assert!(e.message.contains("sign in again"), "{}", e.message);
        // The mark the admin's saves left stays, and so do the tokens.
        let oauth = row_of(&n, "ada").oauth.unwrap();
        assert_eq!(
            oauth.status,
            crate::mcp_registry::McpOauthStatus::NeedsReauth
        );
        assert_eq!(
            token_of(&n, "ada", "filebox/oauth/refresh-token"),
            NEW_REFRESH
        );
        // A sign-in under the client as it is now connects.
        n.token.reply(200, tokens_reply());
        let (s, b) = exchange(
            &n,
            "member-secret",
            "filebox",
            exchange_body("squrell://oauth"),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
    }

    #[tokio::test]
    async fn a_client_changed_and_changed_back_while_the_provider_answers_stores_nothing() {
        let n = xchg_node("xchg-aba-held");
        let (s, b) = put_client(&n, "c-1", Some(SECRET)).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        n.token.reply(200, tokens_reply());
        let (s, b) = exchange(
            &n,
            "member-secret",
            "filebox",
            exchange_body("squrell://oauth"),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        let c1 = n.store.node_client("filebox").unwrap().unwrap();

        // ada signs in again. The secret read and the request pass every check (nothing has
        // changed yet); the provider accepts the code, but its answer is held until the admin
        // has saved c-2 and then c-1 again.
        let (arrived, release) = n.token.hold_next();
        n.token.reply(
            200,
            serde_json::json!({ "access_token": "HELD-A", "refresh_token": "HELD-R", "expires_in": 3600 }),
        );
        let admin = async {
            tokio::task::spawn_blocking(move || arrived.recv().unwrap())
                .await
                .unwrap();
            let (s, b) = put_client(&n, "c-2", Some("C2-SECRET-wwwwwwwwwwww")).await;
            assert_eq!(s, StatusCode::OK, "{b}");
            let (s, b) = put_client(&n, "c-1", Some(SECRET)).await;
            assert_eq!(s, StatusCode::OK, "{b}");
            release.send(()).unwrap();
        };
        let ((s, b), ()) = tokio::join!(
            exchange(
                &n,
                "member-secret",
                "filebox",
                exchange_body("squrell://oauth"),
            ),
            admin
        );
        // The commit's revision check refuses it: the client id and endpoint are c-1's again,
        // but not the revision the sign-in read.
        let back = n.store.node_client("filebox").unwrap().unwrap();
        assert_eq!(back.client_id, c1.client_id);
        assert!(back.revision > c1.revision);
        assert_eq!(n.token.requests().len(), 2, "the held request was sent");
        assert_eq!(s, StatusCode::CONFLICT, "{b}");
        assert!(b.to_string().contains("sign in again"), "{b}");
        assert_no_value("the refusal", &b.to_string());
        // The tokens are the first sign-in's, and the mark c-2's save left stays.
        assert_eq!(
            token_of(&n, "ada", "filebox/oauth/access-token"),
            NEW_ACCESS
        );
        assert_eq!(
            token_of(&n, "ada", "filebox/oauth/refresh-token"),
            NEW_REFRESH
        );
        let oauth = row_of(&n, "ada").oauth.unwrap();
        assert_eq!(
            oauth.status,
            crate::mcp_registry::McpOauthStatus::NeedsReauth
        );
        assert!(
            oauth
                .last_refresh_error
                .as_deref()
                .is_some_and(|e| e.contains("changed the client")),
            "{oauth:?}"
        );
    }

    #[tokio::test]
    async fn a_save_that_fails_after_the_code_was_spent_replaces_nothing_and_says_to_sign_in_again()
    {
        let n = xchg_node("xchg-atomic");
        let (s, b) = put_client(&n, "c-1", Some(SECRET)).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        n.token.reply(200, tokens_reply());
        let (s, b) = exchange(
            &n,
            "member-secret",
            "filebox",
            exchange_body("squrell://oauth"),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        let before_row = row_of(&n, "ada");
        let before = (
            token_of(&n, "ada", "filebox/oauth/access-token"),
            token_of(&n, "ada", "filebox/oauth/refresh-token"),
        );
        // The disk refuses the access-token write, the second of the two the exchange makes.
        n.store
            .connection
            .lock()
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER no_access_token BEFORE INSERT ON host_secrets
                 WHEN NEW.binding_id = 'filebox/oauth/access-token'
                 BEGIN SELECT RAISE(ABORT, 'disk full'); END;",
            )
            .unwrap();
        n.token.reply(
            200,
            serde_json::json!({ "access_token": "X-A2", "refresh_token": "X-R2", "expires_in": 60 }),
        );
        let (s, b) = exchange(
            &n,
            "member-secret",
            "filebox",
            exchange_body("squrell://oauth"),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_GATEWAY, "{b}");
        assert!(b.to_string().contains("sign in again"), "{b}");
        // The same request would carry the same spent code: not retryable.
        assert_eq!(b["retryable"], false, "{b}");
        assert_no_value("the failed save", &b.to_string());
        assert!(
            !b.to_string().contains("X-R2") && !b.to_string().contains("X-A2"),
            "{b}"
        );
        // The provider spent the code (two requests); neither token, nor the row's metadata, moved.
        assert_eq!(n.token.requests().len(), 2);
        assert_eq!(
            before,
            (
                token_of(&n, "ada", "filebox/oauth/access-token"),
                token_of(&n, "ada", "filebox/oauth/refresh-token"),
            )
        );
        assert_eq!(row_of(&n, "ada").oauth, before_row.oauth);
        assert_eq!(row_of(&n, "ada").updated_at, before_row.updated_at);
    }

    /// A member's row on the connector's previous server, needing a sign-in, with tokens.
    fn previous_server_row(n: &Node) {
        previous_server_row_revoked_at(n, None);
    }

    /// [`previous_server_row`] whose server revokes refresh tokens at `revocation`.
    fn previous_server_row_revoked_at(n: &Node, revocation: Option<String>) {
        use crate::mcp_registry::{
            McpAuthMode, McpOauthMetadata, McpOauthRegistrationMethod, McpOauthStatus,
        };
        let oauth = McpOauthMetadata {
            resource: "https://mcp.old-home.example/mcp".into(),
            authorization_server: "https://auth.old-home.example".into(),
            token_endpoint: "https://auth.old-home.example/token".into(),
            revocation_endpoint: revocation,
            client_id: "c-old".into(),
            registration_method: McpOauthRegistrationMethod::Manual,
            scopes: vec![],
            expires_at: None,
            status: McpOauthStatus::NeedsReauth,
            last_refresh_at: None,
            last_refresh_error: Some("the connector moved".into()),
        };
        let conn = n
            .store
            .upsert_mcp_connection(
                SecretScope::User,
                "ada",
                "filebox",
                "https://mcp.old-home.example/mcp",
                "",
                &[],
                &[],
                McpAuthMode::Oauth,
                Some(&oauth),
            )
            .unwrap();
        crate::mcp_registry::store_refreshed_mcp_tokens(&n.vault, &conn, "OLD-A", Some("OLD-R"))
            .unwrap();
    }

    #[tokio::test]
    async fn a_row_of_a_moved_connector_is_replaced_only_once_the_new_grant_is_valid() {
        let n = xchg_node("xchg-moved");
        let (s, b) = put_client(&n, "c-1", Some(SECRET)).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        previous_server_row(&n);
        // The provider refuses the admin's client: the member gets the refusal, and her old row and
        // tokens are exactly where they were.
        n.token.reply(
            401,
            serde_json::json!({ "error": "invalid_client", "error_description": SECRET }),
        );
        let (s, b) = exchange(
            &n,
            "member-secret",
            "filebox",
            exchange_body("squrell://oauth"),
        )
        .await;
        assert_eq!(s, StatusCode::CONFLICT, "{b}");
        assert!(b.to_string().contains("client id or secret"), "{b}");
        let row = row_of(&n, "ada");
        assert_eq!(row.url, "https://mcp.old-home.example/mcp");
        assert_eq!(token_of(&n, "ada", "filebox/oauth/refresh-token"), "OLD-R");
        assert_eq!(token_of(&n, "ada", "filebox/oauth/access-token"), "OLD-A");
        // The grant succeeds: now the old row is gone and the connector's own row replaces it, with
        // both new tokens, in one commit.
        n.token.reply(200, tokens_reply());
        let (s, b) = exchange(
            &n,
            "member-secret",
            "filebox",
            exchange_body("squrell://oauth"),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        let row = row_of(&n, "ada");
        assert_eq!(row.url, "https://mcp.xchg-moved.example/mcp");
        assert_eq!(row.oauth.unwrap().client_id, "c-1");
        assert_eq!(
            token_of(&n, "ada", "filebox/oauth/refresh-token"),
            NEW_REFRESH
        );
        assert_eq!(
            token_of(&n, "ada", "filebox/oauth/access-token"),
            NEW_ACCESS
        );
    }

    #[tokio::test]
    async fn a_moved_connectors_old_token_is_revoked_only_after_its_replacement_commits() {
        let n = xchg_node("xchg-revoke-after");
        let (s, b) = put_client(&n, "c-1", Some(SECRET)).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        let revoker = provider();
        crate::mcp_oauth_refresh::test_seam::serve(
            "revoke.old-home-revoke.example",
            revoker.base.clone(),
        );
        previous_server_row_revoked_at(
            &n,
            Some("https://revoke.old-home-revoke.example/revoke".into()),
        );
        // The provider accepts the code, but the disk refuses the replacement: the old row and its
        // refresh token stay, and nothing has asked the old server to revoke that token.
        n.store
            .connection
            .lock()
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER no_new_access_token BEFORE INSERT ON host_secrets
                 WHEN NEW.binding_id = 'filebox/oauth/access-token'
                 BEGIN SELECT RAISE(ABORT, 'disk full'); END;",
            )
            .unwrap();
        n.token.reply(200, tokens_reply());
        let (s, b) = exchange(
            &n,
            "member-secret",
            "filebox",
            exchange_body("squrell://oauth"),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_GATEWAY, "{b}");
        assert!(revoker.requests().is_empty(), "revoked before the commit");
        assert_eq!(row_of(&n, "ada").url, "https://mcp.old-home.example/mcp");
        assert_eq!(token_of(&n, "ada", "filebox/oauth/refresh-token"), "OLD-R");
        // The replacement commits: then, and only then, the old token is revoked, once.
        n.store
            .connection
            .lock()
            .unwrap()
            .execute_batch("DROP TRIGGER no_new_access_token;")
            .unwrap();
        revoker.reply(200, serde_json::Value::Null);
        n.token.reply(200, tokens_reply());
        let (s, b) = exchange(
            &n,
            "member-secret",
            "filebox",
            exchange_body("squrell://oauth"),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        let revoked = revoker.requests();
        assert_eq!(revoked.len(), 1, "one revoke after the commit");
        assert_eq!(
            revoked[0].form.get("token").map(String::as_str),
            Some("OLD-R")
        );
        assert_eq!(
            token_of(&n, "ada", "filebox/oauth/refresh-token"),
            NEW_REFRESH
        );
    }

    #[tokio::test]
    async fn an_exchange_that_reports_a_failure_committed_nothing_so_its_generation_is_current() {
        // The exchange advances the credential generation only on `Ok`. So every failure must mean
        // nothing was committed: a write that lands and then fails to read its row back would leave
        // new tokens on disk while waiters keep the replaced single-use refresh token.
        let n = xchg_node("xchg-readback");
        let (s, b) = put_client(&n, "c-1", Some(SECRET)).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        n.token.reply(200, tokens_reply());
        let (s, b) = exchange(
            &n,
            "member-secret",
            "filebox",
            exchange_body("squrell://oauth"),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        let before = crate::host_tools::lock_mcp_credentials(&n.store, &row_of(&n, "ada"))
            .await
            .unwrap()
            .generation_handle();
        // The write lands, but the row it leaves cannot be read back.
        n.store
            .connection
            .lock()
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER unreadable_row AFTER UPDATE OF tools_snapshot_json ON mcp_connections
                 WHEN NEW.connection_id = 'filebox' AND NEW.tools_snapshot_json <> 'not json'
                 BEGIN UPDATE mcp_connections SET tools_snapshot_json = 'not json'
                       WHERE rowid = NEW.rowid; END;
                 CREATE TRIGGER unreadable_new_row AFTER INSERT ON mcp_connections
                 WHEN NEW.connection_id = 'filebox'
                 BEGIN UPDATE mcp_connections SET tools_snapshot_json = 'not json'
                       WHERE rowid = NEW.rowid; END;",
            )
            .unwrap();
        n.token.reply(
            200,
            serde_json::json!({ "access_token": "RE-A", "refresh_token": "RE-R" }),
        );
        let (s, b) = exchange(
            &n,
            "member-secret",
            "filebox",
            exchange_body("squrell://oauth"),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_GATEWAY, "{b}");
        // The failure rolled the whole write back: the generation is the one the first sign-in
        // left, and its tokens are still the ones (exactly).
        assert!(
            before.lock_if_unchanged().await.is_some(),
            "a reported failure moved the credential generation"
        );
        assert_eq!(
            token_of(&n, "ada", "filebox/oauth/refresh-token"),
            NEW_REFRESH
        );
        assert_eq!(
            token_of(&n, "ada", "filebox/oauth/access-token"),
            NEW_ACCESS
        );
    }

    #[tokio::test]
    async fn a_disconnect_while_tools_are_listed_does_not_bring_the_row_back() {
        let n = xchg_node("xchg-disconnect");
        let (s, b) = put_client(&n, "c-1", Some(SECRET)).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        n.token.reply(200, tokens_reply());
        let (s, b) = exchange(
            &n,
            "member-secret",
            "filebox",
            exchange_body("squrell://oauth"),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        let conn = row_of(&n, "ada");
        let policy = n.store.mcp_policy("filebox").unwrap().unwrap();
        // What the exchange left: the generation its tool adoption is conditional on.
        let handle = crate::host_tools::lock_mcp_credentials(&n.store, &conn)
            .await
            .unwrap()
            .generation_handle();

        // Ada reconnects (new tokens, the generation moves) while the old adoption was in flight:
        // it lists the tools fine, finds the credentials are not the ones it was made for, and
        // writes nothing.
        n.token.reply(
            200,
            serde_json::json!({ "access_token": "RE-A", "refresh_token": "RE-R" }),
        );
        let (s, b) = exchange(
            &n,
            "member-secret",
            "filebox",
            exchange_body("squrell://oauth"),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        let updated = row_of(&n, "ada").updated_at;
        let e =
            super::super::mcp_connections::adopt_connector_tools(&n.state, &conn, &policy, &handle)
                .await
                .err()
                .unwrap();
        assert_eq!(e.status, StatusCode::CONFLICT, "{}", e.message);
        // The sign-in this adoption was for is stored (and was replaced): the app reads the state
        // again and does not repeat the exchange with its spent code.
        assert!(
            e.message.contains("read the connection's state again"),
            "{}",
            e.message
        );
        assert!(!e.retryable);
        assert_eq!(row_of(&n, "ada").updated_at, updated);

        // Ada disconnects: row and tokens go. A late adoption writes no row, however it is asked.
        let (s, b) = call(
            &n.app,
            "DELETE",
            "/v1/mcp/connections/filebox",
            "member-secret",
            None,
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert!(member_vault_ids(&n.vault).is_empty());
        let current = crate::host_tools::lock_mcp_credentials(&n.store, &conn)
            .await
            .unwrap()
            .generation_handle();
        let _ = super::super::mcp_connections::adopt_connector_tools(
            &n.state, &conn, &policy, &current,
        )
        .await;
        let tool = crate::mcp_client::McpTool {
            name: "search_files".into(),
            description: None,
            input_schema: serde_json::json!({}),
            ..Default::default()
        };
        let e = n
            .store
            .adopt_connector_tools_if_present(
                SecretScope::User,
                "ada",
                &policy,
                conn.oauth.as_ref(),
                &[tool],
                None,
            )
            .unwrap_err();
        assert!(matches!(e, OrchestratorError::Conflict(_)), "{e}");
        assert!(n
            .store
            .get_mcp_connection(SecretScope::User, "ada", "filebox")
            .is_err());
        assert!(member_vault_ids(&n.vault).is_empty());
    }

    #[tokio::test]
    async fn an_adoption_writes_no_tools_onto_a_sign_in_other_than_the_one_it_read() {
        use crate::mcp_registry::McpOauthStatus::{Connected, NeedsReauth};
        let tools_ok = Arc::new(AtomicBool::new(false));
        let n = xchg_node_with("xchg-observed", tools_ok.clone());
        let (s, b) = put_client(&n, "c-1", Some(SECRET)).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        // A first sign-in whose tools cannot be read: it is pending.
        n.token.reply(200, tokens_reply());
        let (s, b) = exchange(
            &n,
            "member-secret",
            "filebox",
            exchange_body("squrell://oauth"),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_GATEWAY, "{b}");
        let first = row_of(&n, "ada");
        assert_eq!(first.oauth.as_ref().unwrap().status, NeedsReauth);
        // The refresh route read that row; then a second sign-in committed (still pending). The
        // route's generation is taken after the second commit, so only the row can tell.
        n.token.reply(
            200,
            serde_json::json!({ "access_token": "RE-A", "refresh_token": "RE-R", "expires_in": 7200 }),
        );
        let (s, b) = exchange(
            &n,
            "member-secret",
            "filebox",
            exchange_body("squrell://oauth"),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_GATEWAY, "{b}");
        let second = row_of(&n, "ada");
        assert_ne!(first.oauth, second.oauth);
        let generation = crate::host_tools::lock_mcp_credentials(&n.store, &second)
            .await
            .unwrap()
            .generation_handle();
        let policy = n.store.mcp_policy("filebox").unwrap().unwrap();
        tools_ok.store(true, std::sync::atomic::Ordering::SeqCst);
        let e = super::super::mcp_connections::adopt_tools_and_reconnect(
            &n.state,
            &first,
            &policy,
            &generation,
        )
        .await
        .err()
        .unwrap();
        assert_eq!(e.status, StatusCode::CONFLICT, "{}", e.message);
        let row = row_of(&n, "ada");
        assert!(row.tools.is_empty(), "{:?}", row.tools);
        assert_eq!(row.oauth, second.oauth);
        // The pending sign-in the route reads now still finishes.
        let (s, b) = call(
            &n.app,
            "POST",
            "/v1/mcp/connections/filebox/refresh",
            "member-secret",
            None,
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(b["drift"], "adopted", "{b}");
        assert_eq!(row_of(&n, "ada").oauth.unwrap().status, Connected);
    }

    #[tokio::test]
    async fn a_failed_listing_never_marks_a_row_another_request_already_moved_on() {
        use crate::mcp_registry::McpOauthStatus::{Connected, NeedsReauth};
        let tools_ok = Arc::new(AtomicBool::new(false));
        let n = xchg_node_with("xchg-mark-race", tools_ok.clone());
        let (s, b) = put_client(&n, "c-1", Some(SECRET)).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        n.token.reply(200, tokens_reply());
        let (s, b) = exchange(
            &n,
            "member-secret",
            "filebox",
            exchange_body("squrell://oauth"),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_GATEWAY, "{b}");
        // Two recoveries read the pending row at the same credential generation.
        let pending = row_of(&n, "ada");
        let generation = crate::host_tools::lock_mcp_credentials(&n.store, &pending)
            .await
            .unwrap()
            .generation_handle();
        let policy = n.store.mcp_policy("filebox").unwrap().unwrap();
        // One adopts the tools.
        tools_ok.store(true, std::sync::atomic::Ordering::SeqCst);
        let (s, b) = call(
            &n.app,
            "POST",
            "/v1/mcp/connections/filebox/refresh",
            "member-secret",
            None,
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        let connected = row_of(&n, "ada");
        assert_eq!(connected.oauth.as_ref().unwrap().status, Connected);
        // The other's listing fails after that: a 409, and the connected row is not marked.
        tools_ok.store(false, std::sync::atomic::Ordering::SeqCst);
        let e = super::super::mcp_connections::adopt_tools_and_reconnect(
            &n.state,
            &pending,
            &policy,
            &generation,
        )
        .await
        .err()
        .unwrap();
        assert_eq!(e.status, StatusCode::CONFLICT, "{}", e.message);
        let row = row_of(&n, "ada");
        assert_eq!(row.oauth, connected.oauth);
        assert_eq!(row.tools.len(), 1, "{:?}", row.tools);

        // An admin save marks the row while a listing for the connected row is in flight: the
        // failed listing leaves the admin's mark.
        let (s, b) = put_client(&n, "c-2", Some("OTHER-SECRET-qqqqqqqqqqqq")).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        let marked = row_of(&n, "ada");
        assert_eq!(marked.oauth.as_ref().unwrap().status, NeedsReauth);
        let e = super::super::mcp_connections::adopt_tools_and_reconnect(
            &n.state,
            &connected,
            &policy,
            &generation,
        )
        .await
        .err()
        .unwrap();
        assert_eq!(e.status, StatusCode::CONFLICT, "{}", e.message);
        assert_eq!(row_of(&n, "ada").oauth, marked.oauth);
    }

    #[tokio::test]
    async fn an_adoption_clears_instructions_the_server_no_longer_sends() {
        let n = xchg_node("xchg-instructions");
        let (s, b) = put_client(&n, "c-1", Some(SECRET)).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        n.token.reply(200, tokens_reply());
        let (s, b) = exchange(
            &n,
            "member-secret",
            "filebox",
            exchange_body("squrell://oauth"),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        let conn = n
            .store
            .set_mcp_instructions(SecretScope::User, "ada", "filebox", Some("Old notes."))
            .unwrap();
        assert_eq!(conn.instructions.as_deref(), Some("Old notes."));
        let policy = n.store.mcp_policy("filebox").unwrap().unwrap();
        let tool = crate::mcp_client::McpTool {
            name: "search_files".into(),
            input_schema: serde_json::json!({ "type": "object" }),
            ..Default::default()
        };
        // A successful snapshot whose `initialize` had no (or empty) instructions: none are kept.
        for presented in [None, Some("")] {
            n.store
                .set_mcp_instructions(SecretScope::User, "ada", "filebox", Some("Old notes."))
                .unwrap();
            let conn = row_of(&n, "ada");
            let adopted = n
                .store
                .adopt_connector_tools_if_present(
                    SecretScope::User,
                    "ada",
                    &policy,
                    conn.oauth.as_ref(),
                    std::slice::from_ref(&tool),
                    presented,
                )
                .unwrap();
            assert_eq!(adopted.instructions.as_deref(), Some(""), "{presented:?}");
        }
    }

    #[tokio::test]
    async fn a_first_sign_in_whose_request_is_dropped_before_adoption_stays_retryable() {
        use crate::mcp_registry::McpOauthStatus::{Connected, NeedsReauth};
        let n = xchg_node("xchg-dropped");
        let (s, b) = put_client(&n, "c-1", Some(SECRET)).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        // The worker commits the exchange; the handler that would adopt the tools is gone (the
        // client disconnected, or the node stopped).
        let client = n.store.node_client("filebox").unwrap().unwrap();
        n.token.reply(200, tokens_reply());
        let connected = exchange_made_with(&n, "ada", client).await.unwrap();
        drop(connected);
        // Not `connected` with no tools: pending, and not announced.
        let row = row_of(&n, "ada");
        assert!(row.tools.is_empty());
        let oauth = row.oauth.clone().unwrap();
        assert_eq!(oauth.status, NeedsReauth);
        assert_eq!(
            oauth.last_refresh_error.as_deref(),
            Some(crate::mcp_registry::TOOLS_PENDING_REASON)
        );
        assert_eq!(open_notices(&n, "ada"), 0);
        let (_, l) = call(&n.app, "GET", "/v1/connectors", "member-secret", None).await;
        assert_eq!(state_of(&l, "filebox"), "needsReconnect");
        // The tools-refresh route finishes it.
        let (s, b) = call(
            &n.app,
            "POST",
            "/v1/mcp/connections/filebox/refresh",
            "member-secret",
            None,
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(b["drift"], "adopted", "{b}");
        let row = row_of(&n, "ada");
        assert_eq!(row.oauth.unwrap().status, Connected);
        assert_eq!(
            row.tools
                .iter()
                .map(|t| t.name.as_str())
                .collect::<Vec<_>>(),
            ["search_files"]
        );
    }

    #[tokio::test]
    async fn a_pending_sign_in_whose_access_token_expired_is_renewed_before_its_tools_are_read() {
        use crate::mcp_registry::McpOauthStatus::{Connected, NeedsReauth};
        let tools_ok = Arc::new(AtomicBool::new(false));
        let n = xchg_node_with("xchg-pending-expired", tools_ok.clone());
        let (s, b) = put_client(&n, "c-1", Some(SECRET)).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        // The sign-in is stored, its tools could not be read, and its access token has expired
        // by the time the member retries.
        n.token.reply(
            200,
            serde_json::json!({ "access_token": NEW_ACCESS, "refresh_token": NEW_REFRESH, "expires_in": 0 }),
        );
        let (s, b) = exchange(
            &n,
            "member-secret",
            "filebox",
            exchange_body("squrell://oauth"),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_GATEWAY, "{b}");
        tools_ok.store(true, std::sync::atomic::Ordering::SeqCst);

        // The provider is down: nothing is recorded, the row stays pending and retryable.
        let (s, b) = call(
            &n.app,
            "POST",
            "/v1/mcp/connections/filebox/refresh",
            "member-secret",
            None,
        )
        .await;
        assert_eq!(s, StatusCode::BAD_GATEWAY, "{b}");
        assert_eq!(b["retryable"], true, "{b}");
        let oauth = row_of(&n, "ada").oauth.unwrap();
        assert_eq!(oauth.status, NeedsReauth);
        assert_eq!(
            oauth.last_refresh_error.as_deref(),
            Some(crate::mcp_registry::TOOLS_PENDING_REASON)
        );
        assert_eq!(
            token_of(&n, "ada", "filebox/oauth/refresh-token"),
            NEW_REFRESH
        );

        // The provider answers: the node's client and the org secret renew the token and the
        // rotated tokens are stored. The tools are down again: the renewal alone does not connect
        // the row, it stays pending.
        tools_ok.store(false, std::sync::atomic::Ordering::SeqCst);
        n.token.reply(
            200,
            serde_json::json!({ "access_token": "FRESH-A", "refresh_token": "FRESH-R", "expires_in": 3600 }),
        );
        let (s, b) = call(
            &n.app,
            "POST",
            "/v1/mcp/connections/filebox/refresh",
            "member-secret",
            None,
        )
        .await;
        assert_eq!(s, StatusCode::BAD_GATEWAY, "{b}");
        let oauth = row_of(&n, "ada").oauth.unwrap();
        assert_eq!(oauth.status, NeedsReauth);
        assert_eq!(
            oauth.last_refresh_error.as_deref(),
            Some(crate::mcp_registry::TOOLS_PENDING_REASON)
        );
        // The tools are back: listed with the new access token (not renewed again), adopted.
        tools_ok.store(true, std::sync::atomic::Ordering::SeqCst);
        let (s, b) = call(
            &n.app,
            "POST",
            "/v1/mcp/connections/filebox/refresh",
            "member-secret",
            None,
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(
            n.token.requests().len(),
            3,
            "exchange, failed renewal, renewal"
        );
        assert_eq!(b["drift"], "adopted", "{b}");
        assert_no_value("the adoption", &b.to_string());
        let grant = n.token.requests().last().cloned().unwrap();
        assert_eq!(grant.form["grant_type"], "refresh_token");
        assert_eq!(grant.form["refresh_token"], NEW_REFRESH);
        assert_eq!(grant.form["client_id"], "c-1");
        assert!(
            grant.form.get("client_secret").map(String::as_str) == Some(SECRET)
                || grant.authorization.is_some(),
            "the org secret authenticates the refresh"
        );
        assert_eq!(token_of(&n, "ada", "filebox/oauth/access-token"), "FRESH-A");
        assert_eq!(
            token_of(&n, "ada", "filebox/oauth/refresh-token"),
            "FRESH-R"
        );
        {
            let seen = n.mcp_seen.lock().unwrap();
            let last_list = seen
                .methods
                .iter()
                .rposition(|m| m == "tools/list")
                .unwrap();
            assert_eq!(seen.auth[last_list].as_deref(), Some("Bearer FRESH-A"));
        }
        let row = row_of(&n, "ada");
        let oauth = row.oauth.unwrap();
        assert_eq!((oauth.status, oauth.last_refresh_error), (Connected, None));
        assert!(oauth.expires_at.unwrap() > crate::connectors::now_secs() + 3000);
        assert_eq!(
            row.tools
                .iter()
                .map(|t| t.name.as_str())
                .collect::<Vec<_>>(),
            ["search_files"]
        );
    }

    #[tokio::test]
    async fn a_refused_client_on_a_pending_sign_in_goes_back_to_pending_after_a_corrected_secret() {
        use crate::mcp_registry::McpOauthStatus::{Connected, NeedsReauth};
        let tools_ok = Arc::new(AtomicBool::new(false));
        let n = xchg_node_with("xchg-pending-refused", tools_ok.clone());
        let (s, b) = put_client(&n, "c-1", Some(SECRET)).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        // A stored sign-in whose tools could not be read, with an access token that has expired.
        n.token.reply(
            200,
            serde_json::json!({ "access_token": NEW_ACCESS, "refresh_token": NEW_REFRESH, "expires_in": 0 }),
        );
        let (s, b) = exchange(
            &n,
            "member-secret",
            "filebox",
            exchange_body("squrell://oauth"),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_GATEWAY, "{b}");
        tools_ok.store(true, std::sync::atomic::Ordering::SeqCst);
        // The renewal before the tools are read is refused for the admin's client.
        n.token.reply(
            401,
            serde_json::json!({ "error": "invalid_client", "error_description": SECRET }),
        );
        let (s, b) = call(
            &n.app,
            "POST",
            "/v1/mcp/connections/filebox/refresh",
            "member-secret",
            None,
        )
        .await;
        assert_eq!(s, StatusCode::CONFLICT, "{b}");
        let oauth = row_of(&n, "ada").oauth.unwrap();
        assert_eq!(oauth.status, NeedsReauth);
        assert_eq!(
            oauth.last_refresh_error.as_deref(),
            Some(connector_client::CLIENT_REFUSED_PENDING_REASON)
        );

        // The admin corrects the secret: the sign-in is pending again, not connected (no tools are
        // adopted yet), and nothing is announced.
        let (s, b) = put_client(&n, "c-1", Some("CORRECTED-SECRET-qqqqqqqq")).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        let row = row_of(&n, "ada");
        assert!(row.tools.is_empty(), "{:?}", row.tools);
        let oauth = row.oauth.unwrap();
        assert_eq!(oauth.status, NeedsReauth);
        assert_eq!(
            oauth.last_refresh_error.as_deref(),
            Some(crate::mcp_registry::TOOLS_PENDING_REASON)
        );
        assert_eq!(open_notices(&n, "ada"), 0);

        // The tools-refresh route finishes it with the corrected secret.
        n.token.reply(
            200,
            serde_json::json!({ "access_token": "FRESH-A", "refresh_token": "FRESH-R", "expires_in": 3600 }),
        );
        let (s, b) = call(
            &n.app,
            "POST",
            "/v1/mcp/connections/filebox/refresh",
            "member-secret",
            None,
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(b["drift"], "adopted", "{b}");
        let row = row_of(&n, "ada");
        assert_eq!(row.oauth.unwrap().status, Connected);
        assert_eq!(
            row.tools
                .iter()
                .map(|t| t.name.as_str())
                .collect::<Vec<_>>(),
            ["search_files"]
        );
    }

    #[tokio::test]
    async fn a_pending_sign_in_is_not_recovered_while_the_connector_is_off() {
        use crate::mcp_registry::McpOauthStatus::Connected;
        let tools_ok = Arc::new(AtomicBool::new(false));
        let n = xchg_node_with("xchg-pending-off", tools_ok.clone());
        let (s, b) = put_client(&n, "c-1", Some(SECRET)).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        n.token.reply(
            200,
            serde_json::json!({ "access_token": NEW_ACCESS, "refresh_token": NEW_REFRESH, "expires_in": 0 }),
        );
        let (s, b) = exchange(
            &n,
            "member-secret",
            "filebox",
            exchange_body("squrell://oauth"),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_GATEWAY, "{b}");
        tools_ok.store(true, std::sync::atomic::Ordering::SeqCst);
        let set_enabled = |enabled: bool| {
            call(
                &n.app,
                "PUT",
                "/v1/connectors/filebox/node",
                "orgadmin-secret",
                Some(serde_json::json!({ "enabled": enabled })),
            )
        };
        let (s, b) = set_enabled(false).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        let pending = row_of(&n, "ada");
        // Off: refused before the provider is contacted; the sign-in stays pending.
        let (s, b) = call(
            &n.app,
            "POST",
            "/v1/mcp/connections/filebox/refresh",
            "member-secret",
            None,
        )
        .await;
        assert_eq!(s, StatusCode::CONFLICT, "{b}");
        assert!(b.to_string().contains("turned off"), "{b}");
        assert_eq!(n.token.requests().len(), 1, "only the exchange");
        assert_eq!(row_of(&n, "ada").oauth, pending.oauth);
        assert_eq!(
            token_of(&n, "ada", "filebox/oauth/refresh-token"),
            NEW_REFRESH
        );
        // The adoption commit checks the enable too (a disable that lands while tools are read).
        let policy = n.store.mcp_policy("filebox").unwrap().unwrap();
        let tool = crate::mcp_client::McpTool {
            name: "search_files".into(),
            input_schema: serde_json::json!({ "type": "object" }),
            ..Default::default()
        };
        let e = n
            .store
            .adopt_connector_tools_if_present(
                SecretScope::User,
                "ada",
                &policy,
                pending.oauth.as_ref(),
                std::slice::from_ref(&tool),
                None,
            )
            .unwrap_err();
        assert!(matches!(e, OrchestratorError::Conflict(_)), "{e}");
        let row = row_of(&n, "ada");
        assert_eq!(row.oauth, pending.oauth);
        assert!(row.tools.is_empty(), "{:?}", row.tools);

        // On again: the same route finishes the sign-in.
        let (s, b) = set_enabled(true).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        n.token.reply(
            200,
            serde_json::json!({ "access_token": "FRESH-A", "refresh_token": "FRESH-R", "expires_in": 3600 }),
        );
        let (s, b) = call(
            &n.app,
            "POST",
            "/v1/mcp/connections/filebox/refresh",
            "member-secret",
            None,
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(b["drift"], "adopted", "{b}");
        assert_eq!(row_of(&n, "ada").oauth.unwrap().status, Connected);
    }

    #[tokio::test]
    async fn a_pending_sign_in_whose_token_the_server_refuses_is_refreshed_once_and_adopted() {
        use crate::mcp_registry::McpOauthStatus::{Connected, NeedsReauth};
        let rejected = Arc::new(Mutex::new(vec![NEW_ACCESS.to_string()]));
        let n = xchg_node_rejecting(
            "xchg-pending-401",
            Arc::new(AtomicBool::new(true)),
            rejected.clone(),
        );
        let (s, b) = put_client(&n, "c-1", Some(SECRET)).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        // The provider sends no `expires_in`, so no expiry is stored; the MCP server refuses the
        // access token, so the tools cannot be read and the sign-in is pending.
        n.token.reply(
            200,
            serde_json::json!({ "access_token": NEW_ACCESS, "refresh_token": NEW_REFRESH, "token_type": "Bearer" }),
        );
        let (s, b) = exchange(
            &n,
            "member-secret",
            "filebox",
            exchange_body("squrell://oauth"),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_GATEWAY, "{b}");
        let oauth = row_of(&n, "ada").oauth.unwrap();
        assert_eq!(oauth.status, NeedsReauth);
        assert_eq!(oauth.expires_at, None);

        // The retry: the refused token is refreshed once, and the renewed one is refused too.
        // No loop: a 502, one refresh, and the sign-in stays pending with the rotated tokens.
        rejected.lock().unwrap().push("STILL-BAD".into());
        n.token.reply(
            200,
            serde_json::json!({ "access_token": "STILL-BAD", "refresh_token": "R-2" }),
        );
        let refresh = || {
            call(
                &n.app,
                "POST",
                "/v1/mcp/connections/filebox/refresh",
                "member-secret",
                None,
            )
        };
        let (s, b) = refresh().await;
        assert_eq!(s, StatusCode::BAD_GATEWAY, "{b}");
        assert_eq!(n.token.requests().len(), 2, "exchange, one refresh");
        let oauth = row_of(&n, "ada").oauth.unwrap();
        assert_eq!(oauth.status, NeedsReauth);
        assert_eq!(
            oauth.last_refresh_error.as_deref(),
            Some(crate::mcp_registry::TOOLS_PENDING_REASON)
        );
        assert_eq!(token_of(&n, "ada", "filebox/oauth/refresh-token"), "R-2");

        // The next retry: refused once more, refreshed once, and the new token lists the tools.
        n.token.reply(
            200,
            serde_json::json!({ "access_token": "FRESH-A", "refresh_token": "FRESH-R" }),
        );
        let (s, b) = refresh().await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(b["drift"], "adopted", "{b}");
        assert_eq!(n.token.requests().len(), 3, "one refresh per retry");
        let grant = n.token.requests().last().cloned().unwrap();
        assert_eq!(grant.form["grant_type"], "refresh_token");
        assert_eq!(grant.form["refresh_token"], "R-2");
        assert_eq!(token_of(&n, "ada", "filebox/oauth/access-token"), "FRESH-A");
        assert_eq!(
            token_of(&n, "ada", "filebox/oauth/refresh-token"),
            "FRESH-R"
        );
        let row = row_of(&n, "ada");
        assert_eq!(row.oauth.unwrap().status, Connected);
        assert_eq!(
            row.tools
                .iter()
                .map(|t| t.name.as_str())
                .collect::<Vec<_>>(),
            ["search_files"]
        );
    }

    #[tokio::test]
    async fn a_sign_in_whose_tools_cannot_be_read_stays_retryable_and_is_not_announced() {
        use crate::mcp_registry::McpOauthStatus::{Connected, NeedsReauth};
        let tools_ok = Arc::new(AtomicBool::new(true));
        let n = xchg_node_with("xchg-pending", tools_ok.clone());
        let (s, b) = put_client(&n, "c-1", Some(SECRET)).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        n.token.reply(200, tokens_reply());
        let (s, b) = exchange(
            &n,
            "member-secret",
            "filebox",
            exchange_body("squrell://oauth"),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        // Ada's grant is revoked later: she needs a sign-in, and has a notification saying so.
        n.store
            .record_mcp_oauth_refresh_failure(&row_of(&n, "ada"), true, "invalid_grant")
            .unwrap();
        assert_eq!(open_notices(&n, "ada"), 1);

        // She signs in again, but the provider cannot list tools just now.
        tools_ok.store(false, std::sync::atomic::Ordering::SeqCst);
        n.token.reply(
            200,
            serde_json::json!({ "access_token": "RE-A", "refresh_token": "RE-R" }),
        );
        let (s, b) = exchange(
            &n,
            "member-secret",
            "filebox",
            exchange_body("squrell://oauth"),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_GATEWAY, "{b}");
        assert!(b.to_string().contains("refresh the connection"), "{b}");
        assert_no_value("the failed adoption", &b.to_string());
        // The sign-in is not lost: the new tokens are stored. It is not announced either: the row
        // still needs attention, the notification is still open, and a run parked on it would stay
        // parked.
        assert_eq!(token_of(&n, "ada", "filebox/oauth/refresh-token"), "RE-R");
        let row = row_of(&n, "ada");
        let oauth = row.oauth.clone().unwrap();
        assert_eq!(oauth.status, NeedsReauth);
        assert_eq!(
            oauth.last_refresh_error.as_deref(),
            Some(crate::mcp_registry::TOOLS_PENDING_REASON)
        );
        assert_eq!(open_notices(&n, "ada"), 1);
        let (_, l) = call(&n.app, "GET", "/v1/connectors", "member-secret", None).await;
        assert_eq!(state_of(&l, "filebox"), "needsReconnect");

        // The tools come back; the existing tools-refresh route finishes the connection.
        tools_ok.store(true, std::sync::atomic::Ordering::SeqCst);
        let (s, b) = call(
            &n.app,
            "POST",
            "/v1/mcp/connections/filebox/refresh",
            "member-secret",
            None,
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(b["drift"], "adopted", "{b}");
        let row = row_of(&n, "ada");
        let oauth = row.oauth.clone().unwrap();
        assert_eq!((oauth.status, oauth.last_refresh_error), (Connected, None));
        assert_eq!(
            row.tools
                .iter()
                .map(|t| t.name.as_str())
                .collect::<Vec<_>>(),
            ["search_files"]
        );
        assert_eq!(open_notices(&n, "ada"), 0);
        let (_, l) = call(&n.app, "GET", "/v1/connectors", "member-secret", None).await;
        assert_eq!(state_of(&l, "filebox"), "connected");

        // A row that needs a sign-in for another reason is not "finished" by the route.
        n.store
            .record_mcp_oauth_refresh_failure(&row_of(&n, "ada"), true, "invalid_grant")
            .unwrap();
        let (s, b) = call(
            &n.app,
            "POST",
            "/v1/mcp/connections/filebox/refresh",
            "member-secret",
            None,
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_ne!(b["drift"], "adopted", "{b}");
        let oauth = row_of(&n, "ada").oauth.unwrap();
        assert_eq!(oauth.status, NeedsReauth);
        assert_eq!(oauth.last_refresh_error.as_deref(), Some("invalid_grant"));
        assert_eq!(open_notices(&n, "ada"), 1);
    }

    #[tokio::test]
    async fn a_corrected_secret_for_the_same_client_brings_back_members_a_refused_client_parked() {
        use crate::mcp_registry::McpOauthStatus::{Connected, NeedsReauth};
        let n = xchg_node("xchg-corrected");
        let (s, b) = put_client(&n, "c-1", Some(SECRET)).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        for (who, key) in [("ada", "member-secret"), ("olga", "orgadmin-secret")] {
            n.token.reply(200, tokens_reply());
            let (s, b) = exchange(&n, key, "filebox", exchange_body("squrell://oauth")).await;
            assert_eq!(s, StatusCode::OK, "{who} {b}");
        }
        // A refresh with the wrong secret was refused for ada; olga's grant was revoked.
        n.store
            .record_mcp_oauth_refresh_failure(
                &row_of(&n, "ada"),
                true,
                connector_client::CLIENT_REFUSED_REASON,
            )
            .unwrap();
        n.store
            .record_mcp_oauth_refresh_failure(&row_of(&n, "olga"), true, "invalid_grant")
            .unwrap();
        let (_, l) = call(&n.app, "GET", "/v1/connectors", "member-secret", None).await;
        assert_eq!(state_of(&l, "filebox"), "needsReconnect");

        // The admin saves the right secret for the same client.
        let (s, b) = put_client(&n, "c-1", Some("CORRECTED-SECRET-qqqqqqqq")).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        let (_, l) = call(&n.app, "GET", "/v1/connectors", "member-secret", None).await;
        assert_eq!(state_of(&l, "filebox"), "connected");
        assert_eq!(row_of(&n, "ada").oauth.unwrap().status, Connected);
        assert_eq!(open_notices(&n, "ada"), 0);
        // A revoked grant still needs a sign-in.
        assert_eq!(row_of(&n, "olga").oauth.unwrap().status, NeedsReauth);
        assert_eq!(open_notices(&n, "olga"), 1);
    }

    #[tokio::test]
    async fn a_corrected_secret_saved_with_a_disable_keeps_members_parked_until_the_enable() {
        use crate::mcp_registry::McpOauthStatus::{Connected, NeedsReauth};
        let n = xchg_node("xchg-corrected-off");
        let (s, b) = put_client(&n, "c-1", Some(SECRET)).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        for (who, key) in [("ada", "member-secret"), ("olga", "orgadmin-secret")] {
            n.token.reply(200, tokens_reply());
            let (s, b) = exchange(&n, key, "filebox", exchange_body("squrell://oauth")).await;
            assert_eq!(s, StatusCode::OK, "{who} {b}");
        }
        n.store
            .record_mcp_oauth_refresh_failure(
                &row_of(&n, "ada"),
                true,
                connector_client::CLIENT_REFUSED_REASON,
            )
            .unwrap();
        n.store
            .record_mcp_oauth_refresh_failure(&row_of(&n, "olga"), true, "invalid_grant")
            .unwrap();
        assert_eq!(open_notices(&n, "ada"), 1);

        // A plain off and on, with no secret saved in between, is not a correction: the provider
        // refused the secret that is still saved, so ada stays parked.
        for enabled in [false, true] {
            let (s, b) = call(
                &n.app,
                "PUT",
                "/v1/connectors/filebox/node",
                "orgadmin-secret",
                Some(serde_json::json!({ "enabled": enabled })),
            )
            .await;
            assert_eq!(s, StatusCode::OK, "{b}");
        }
        assert_eq!(row_of(&n, "ada").oauth.unwrap().status, NeedsReauth);
        assert_eq!(open_notices(&n, "ada"), 1);

        // The admin corrects the secret and turns the connector off in the same save: nothing is
        // released on a connector that is off.
        let (s, b) = call(
            &n.app,
            "PUT",
            "/v1/connectors/filebox/node",
            "orgadmin-secret",
            Some(serde_json::json!({
                "enabled": false,
                "client": { "clientId": "c-1", "clientSecret": "CORRECTED-SECRET-qqqqqqqq" }
            })),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        let oauth = row_of(&n, "ada").oauth.unwrap();
        assert_eq!(oauth.status, NeedsReauth);
        assert_eq!(
            oauth.last_refresh_error.as_deref(),
            Some(connector_client::CLIENT_REFUSED_REASON)
        );
        assert_eq!(open_notices(&n, "ada"), 1);

        // The enable that follows keeps the client: the rows the refused client parked are back.
        let (s, b) = call(
            &n.app,
            "PUT",
            "/v1/connectors/filebox/node",
            "orgadmin-secret",
            Some(serde_json::json!({ "enabled": true })),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(row_of(&n, "ada").oauth.unwrap().status, Connected);
        assert_eq!(open_notices(&n, "ada"), 0);
        // A revoked grant still needs a sign-in.
        assert_eq!(row_of(&n, "olga").oauth.unwrap().status, NeedsReauth);
        assert_eq!(open_notices(&n, "olga"), 1);
        // A save that keeps it enabled clears nothing more (there is nothing to clear).
        let (s, b) = call(
            &n.app,
            "PUT",
            "/v1/connectors/filebox/node",
            "orgadmin-secret",
            Some(serde_json::json!({ "enabled": true })),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(row_of(&n, "olga").oauth.unwrap().status, NeedsReauth);
    }

    /// `ada` signed in under `c-1` (connected, tools adopted).
    async fn signed_in_node(tag: &str) -> Node {
        let n = xchg_node(tag);
        let (s, b) = put_client(&n, "c-1", Some(SECRET)).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        n.token.reply(200, tokens_reply());
        let (s, b) = exchange(
            &n,
            "member-secret",
            "filebox",
            exchange_body("squrell://oauth"),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        n
    }

    #[tokio::test]
    async fn a_rotated_refresh_token_survives_a_failed_access_token_write() {
        let n = signed_in_node("xchg-rotate-keep").await;
        let row = row_of(&n, "ada");
        let used = n.store.node_client("filebox").unwrap().unwrap();
        // The provider rotated the refresh token (the old one is spent); the access-token write
        // then fails.
        n.store
            .connection
            .lock()
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER no_access_token BEFORE INSERT ON host_secrets
                 WHEN NEW.binding_id = 'filebox/oauth/access-token'
                 BEGIN SELECT RAISE(ABORT, 'disk full'); END;",
            )
            .unwrap();
        let r = n.store.commit_org_client_refresh(
            &n.vault,
            &row,
            &used,
            "ROTATED-A",
            Some("ROTATED-R"),
            Some(3600),
        );
        assert!(r.is_err(), "{r:?}");
        // The new refresh token is kept (the next refresh uses it); the rest is as it was.
        assert_eq!(
            token_of(&n, "ada", "filebox/oauth/refresh-token"),
            "ROTATED-R"
        );
        assert_eq!(
            token_of(&n, "ada", "filebox/oauth/access-token"),
            NEW_ACCESS
        );
        assert_eq!(row_of(&n, "ada").oauth, row.oauth);
    }

    #[tokio::test]
    async fn a_refresh_at_the_new_revision_of_a_client_changed_back_does_not_clear_the_mark() {
        use crate::mcp_registry::McpOauthStatus::NeedsReauth;
        let n = signed_in_node("xchg-refresh-aba").await;
        let row = row_of(&n, "ada");
        // c-1 → c-2 (ada's row is marked) → c-1 again, at a new revision.
        let (s, b) = put_client(&n, "c-2", Some("C2-SECRET-wwwwwwwwwwww")).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        let (s, b) = put_client(&n, "c-1", Some(SECRET)).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        let marked = row_of(&n, "ada").oauth.unwrap();
        assert_eq!(marked.status, NeedsReauth);
        // A refresh that read c-1 at that revision stores nothing and leaves the mark.
        let used = n.store.node_client("filebox").unwrap().unwrap();
        let stored = n
            .store
            .commit_org_client_refresh(&n.vault, &row, &used, "LATE-A", Some("LATE-R"), None)
            .unwrap();
        assert!(!stored);
        assert_eq!(row_of(&n, "ada").oauth.unwrap(), marked);
        assert_eq!(
            token_of(&n, "ada", "filebox/oauth/refresh-token"),
            NEW_REFRESH
        );
        // And a parked row is not refreshed at all: no request reaches the provider.
        let before = n.token.requests().len();
        let (store, vault, parked) = (n.store.clone(), n.vault.clone(), row_of(&n, "ada"));
        let outcome = tokio::task::spawn_blocking(move || {
            crate::host_tools::refresh_connection_for_tests(
                &store,
                &vault,
                &parked,
                crate::mcp_oauth_refresh::refresh_access_token,
            )
        })
        .await
        .unwrap();
        assert!(outcome.is_err());
        assert_eq!(n.token.requests().len(), before);
        assert_eq!(row_of(&n, "ada").oauth.unwrap(), marked);
    }

    #[tokio::test]
    async fn a_refused_client_mark_does_not_come_back_through_a_client_changed_and_back() {
        use crate::mcp_registry::McpOauthStatus::NeedsReauth;
        let n = signed_in_node("xchg-refused-aba").await;
        n.store
            .record_mcp_oauth_refresh_failure(
                &row_of(&n, "ada"),
                true,
                connector_client::CLIENT_REFUSED_REASON,
            )
            .unwrap();
        // c-1 → c-2 → c-1, then the admin corrects c-1's secret: ada's tokens are of c-1, but
        // the client changed in between, so only a new sign-in brings her back.
        let (s, b) = put_client(&n, "c-2", Some("C2-SECRET-wwwwwwwwwwww")).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        let (s, b) = put_client(&n, "c-1", Some(SECRET)).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        let (s, b) = put_client(&n, "c-1", Some("CORRECTED-SECRET-qqqqqqqq")).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        let oauth = row_of(&n, "ada").oauth.unwrap();
        assert_eq!(oauth.status, NeedsReauth);
        assert_ne!(
            oauth.last_refresh_error.as_deref(),
            Some(connector_client::CLIENT_REFUSED_REASON)
        );
        assert_eq!(open_notices(&n, "ada"), 1);
    }

    #[tokio::test]
    async fn the_secret_goes_as_http_basic_when_the_provider_does_not_say_how() {
        let n = xchg_node("xchg-basic");
        // The provider's metadata omits `token_endpoint_auth_methods_supported`: RFC 8414 says Basic.
        crate::connector_discovery::test_seam::answer(
            "https://auth.xchg-basic.example/.well-known/oauth-authorization-server",
            Some(serde_json::json!({
                "issuer": "https://auth.xchg-basic.example",
                "authorization_endpoint": "https://auth.xchg-basic.example/authorize",
                "token_endpoint": "https://auth.xchg-basic.example/token",
                "revocation_endpoint": "https://auth.xchg-basic.example/revoke",
                "code_challenge_methods_supported": ["S256"]
            })),
        );
        let (s, b) = put_client(&n, "c-1", Some(SECRET)).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(
            n.store
                .node_client("filebox")
                .unwrap()
                .unwrap()
                .token_auth_method,
            connector_client::TokenAuthMethod::ClientSecretBasic
        );
        let expected = {
            use base64::Engine as _;
            format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode(format!("c-1:{SECRET}"))
            )
        };
        // The exchange: Basic, and no client_secret field.
        n.token.reply(200, tokens_reply());
        let (s, b) = exchange(
            &n,
            "member-secret",
            "filebox",
            exchange_body("squrell://oauth"),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        let seen = n.token.requests();
        assert_eq!(seen[0].authorization.as_deref(), Some(expected.as_str()));
        assert!(
            !seen[0].form.contains_key("client_secret"),
            "{:?}",
            seen[0].form
        );
        assert_eq!(seen[0].form["client_id"], "c-1");
        // The revoke on disconnect: the same.
        n.token.reply(200, serde_json::Value::Null);
        let (s, b) = call(
            &n.app,
            "DELETE",
            "/v1/mcp/connections/filebox",
            "member-secret",
            None,
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        let seen = n.token.requests();
        let revoke = seen.last().unwrap();
        assert_eq!(revoke.path, "/revoke");
        assert_eq!(revoke.authorization.as_deref(), Some(expected.as_str()));
        assert!(
            !revoke.form.contains_key("client_secret"),
            "{:?}",
            revoke.form
        );
        assert_eq!(revoke.form["token"], NEW_REFRESH);
    }

    #[tokio::test]
    async fn the_revoke_authenticates_the_way_the_revocation_endpoint_says() {
        let n = xchg_node("xchg-revoke-basic");
        // The token endpoint lists POST; the revocation endpoint only Basic.
        crate::connector_discovery::test_seam::answer(
            "https://auth.xchg-revoke-basic.example/.well-known/oauth-authorization-server",
            Some(serde_json::json!({
                "issuer": "https://auth.xchg-revoke-basic.example",
                "authorization_endpoint": "https://auth.xchg-revoke-basic.example/authorize",
                "token_endpoint": "https://auth.xchg-revoke-basic.example/token",
                "revocation_endpoint": "https://auth.xchg-revoke-basic.example/revoke",
                "code_challenge_methods_supported": ["S256"],
                "token_endpoint_auth_methods_supported": ["client_secret_post"],
                "revocation_endpoint_auth_methods_supported": ["client_secret_basic"]
            })),
        );
        let (s, b) = put_client(&n, "c-1", Some(SECRET)).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        let client = n.store.node_client("filebox").unwrap().unwrap();
        assert_eq!(
            client.token_auth_method,
            connector_client::TokenAuthMethod::ClientSecretPost
        );
        assert_eq!(
            client.revocation_auth_method,
            Some(connector_client::TokenAuthMethod::ClientSecretBasic)
        );
        // The exchange posts the secret, as the token endpoint says.
        n.token.reply(200, tokens_reply());
        let (s, b) = exchange(
            &n,
            "member-secret",
            "filebox",
            exchange_body("squrell://oauth"),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        let seen = n.token.requests();
        assert_eq!(seen[0].form["client_secret"], SECRET);
        assert!(seen[0].authorization.is_none());
        // The revoke on disconnect uses Basic, as the revocation endpoint says.
        n.token.reply(200, serde_json::Value::Null);
        let (s, b) = call(
            &n.app,
            "DELETE",
            "/v1/mcp/connections/filebox",
            "member-secret",
            None,
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        let expected = {
            use base64::Engine as _;
            format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode(format!("c-1:{SECRET}"))
            )
        };
        let seen = n.token.requests();
        let revoke = seen.last().unwrap();
        assert_eq!(revoke.path, "/revoke");
        assert_eq!(revoke.authorization.as_deref(), Some(expected.as_str()));
        assert!(
            !revoke.form.contains_key("client_secret"),
            "{:?}",
            revoke.form
        );
        assert_eq!(revoke.form["token"], NEW_REFRESH);
    }

    #[tokio::test]
    async fn a_body_that_fails_to_parse_never_quotes_what_it_held() {
        let (app, store, _vault) = app_world();
        install_filebox(&store, "xchg-parse");
        // The first value is rejected by the typed parse before the truncation is reached, and the
        // body is not valid JSON, so a generic parse would not have caught it.
        for body in [
            r#"{"enabled":"SUPER-SECRET-zz","client":{"clientId":"c","clientSecret":"x"}"#,
            r#"{"enabled":true,"SUPER-SECRET-zz":1"#,
            r#"{"enabled":true,"client":{"clientId":"c","SUPER-SECRET-zz":"x"}"#,
        ] {
            let req = Request::builder()
                .method("PUT")
                .uri("/v1/connectors/filebox/node")
                .header("authorization", "Bearer orgadmin-secret")
                .header("idempotency-key", "k-1")
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap();
            let res = app.clone().oneshot(req).await.unwrap();
            assert_eq!(res.status(), StatusCode::BAD_REQUEST);
            let bytes = res.into_body().collect().await.unwrap().to_bytes();
            let text = String::from_utf8(bytes.to_vec()).unwrap();
            assert!(!text.contains("SUPER-SECRET"), "{body}: {text}");
            assert!(text.contains("connector node body must be"), "{text}");
        }
    }
}
