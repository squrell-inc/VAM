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
use crate::mcp_registry::{mcp_auth_binding_id, mcp_oauth_refresh_token_binding_id};
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
    // The definitions, the node rows and the clients in one snapshot, so an entry never pairs a
    // release with the enablement of another save, nor a node row with a client from another.
    let (entries, nodes, clients) = state.store.connector_listing()?;
    let admin = auth.has_users_admin();
    let list: Vec<Value> = entries
        .iter()
        .map(|e| {
            let id = e.id();
            let node = nodes.iter().find(|n| n.connector_id == id);
            // Every host shown is filled from the row this entry shows.
            let e = &connectors::entry_for_node(e, node);
            let def = &e.def;
            let client = clients.iter().find(|c| c.connector_id == id);
            let (mine, account) = caller_connection_state(&state, &auth, def, node, client);
            let mut entry = connectors::list_entry(def, &cfg, node, mine, admin);
            connectors::show_account(
                &mut entry,
                connectors::caller_state(def, node, mine),
                account.as_deref(),
            );
            connectors::annotate_source(&mut entry, e, admin);
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
/// connection yet). `node` and `client` are the pair the caller read in one snapshot: the state is
/// judged against that client, never against one read again here.
fn caller_connection_state(
    state: &AppState,
    auth: &AuthContext,
    def: &vam_spec::connector::Connector,
    node: Option<&connectors::NodeConnector>,
    client: Option<&connector_client::NodeClient>,
) -> (Option<connectors::CallerState>, Option<String>) {
    let id = connectors::connector_id(def);
    let id = id.as_str();
    let scope = SecretScope::User;
    let owner = vault_owner_user_id(scope, auth.owner_user_id()).to_string();
    let Some(policy) = state.store.mcp_policy(id).ok().flatten() else {
        let Ok(Some(conn)) = state.store.connector_connection(&owner, id) else {
            return (None, None);
        };
        // An `oauth` connector's connection is judged against the token endpoint of the node's
        // client as well (a connection made against another is not this company's). An `api_key`
        // connection is judged by its key's pin against the filled hosts (`rest_state_for`).
        let st = connectors::rest_state_for(
            &state.store,
            def,
            &owner,
            &conn,
            node,
            || {
                state
                    .vault
                    .has_binding_at(scope, &owner, &connectors::refresh_token_binding_id(id))
                    .unwrap_or(false)
            },
            client.map(|c| c.token_endpoint.as_str()),
        );
        return (Some(st), conn.label);
    };
    let Ok(conn) = state.store.get_mcp_connection(scope, &owner, id) else {
        return (None, None);
    };
    // An MCP connector with an API key has a static row and a header row, never a refresh token.
    if connectors::is_api_key(def) {
        let has_header = state
            .vault
            .has_binding_at(
                scope,
                &conn.owner_user_id,
                &mcp_auth_binding_id(&conn.connection_id),
            )
            .unwrap_or(false);
        return (
            connectors::mcp_api_key_state(&policy, Some(&conn), has_header),
            None,
        );
    }
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
    let id = e.id();
    let (node, client) = state.store.node_connector_and_client(&id)?;
    // Every host shown is filled from the node row this response shows, not from the definition the
    // caller resolved before it (an admin's save or an update may have landed between).
    let e = &connectors::entry_for_node(e, node.as_ref());
    let def = &e.def;
    let (mine, account) = caller_connection_state(state, auth, def, node.as_ref(), client.as_ref());
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
    // The file as published: the plan fills it with the address of this body.
    let template = found.template.clone();
    let req = parse_put_node(&body)?;
    let cfg = connectors::config();
    let existing = state.store.node_connector(&id)?;
    let existing_client = state.store.node_client(&id)?;
    let now = connectors::now_secs();
    // Everything is decided before anything is written: a refused body leaves the state as it was.
    let row = connectors::plan_put(
        &template,
        &cfg,
        existing.as_ref(),
        &req,
        now,
        auth.owner_user_id(),
    )?;
    // The connector as this save leaves it: `{subdomain}` filled, so every URL below is exact.
    let def = connectors::resolve(&template, row.tenant_subdomain.as_deref());
    connector_client::check_address_change(
        &template,
        existing
            .as_ref()
            .and_then(|n| n.tenant_subdomain.as_deref()),
        row.tenant_subdomain.as_deref(),
        &req.client,
        existing_client.as_ref(),
    )?;
    let intent =
        connector_client::check_request(&def, existing_client.as_ref(), req.enabled, &req.client)?;
    let mcp_url = def.mcp.as_ref().map(|m| m.url.clone()).unwrap_or_default();
    let discovered = match &intent {
        // A provider without metadata: the endpoints come from the file, no request is made.
        connector_client::ClientIntent::Set(_) if connector_client::is_rest_oauth(&def) => {
            Some(connector_client::fixed_endpoints(&def)?)
        }
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
    let saved = state.store.put_node_with_client(
        &state.vault,
        &row,
        &plan,
        &mcp_url,
        &connector_client::PlanBasis {
            def: &template,
            client: existing_client.as_ref(),
            tenant: existing
                .as_ref()
                .and_then(|n| n.tenant_subdomain.as_deref()),
        },
    )?;
    #[cfg(test)]
    test_seam::after_save(&mcp_url);
    // Members whose refresh the provider had refused for this client are connected again (the same
    // client, a corrected secret, or the enable after a save that corrected it with a disable):
    // their reconnect notification is resolved and parked runs go on. A save that leaves the
    // connector off clears no one, so nothing is resumed on a disabled connector.
    for owner in &saved.cleared {
        if let Ok(conn) = state
            .store
            .get_mcp_connection(SecretScope::User, owner, &id)
        {
            super::reauth::resume_parked(&state, &conn).await;
        }
    }
    // The node row and the client are the ones this save committed, read in its transaction: a
    // concurrent save cannot pair this node row with another save's client (or with none).
    let (mine, account) = caller_connection_state(
        &state,
        &auth,
        &def,
        Some(&saved.node),
        saved.client.as_ref(),
    );
    let mut entry = connectors::list_entry(&def, &cfg, Some(&saved.node), mine, true);
    connectors::show_account(
        &mut entry,
        connectors::caller_state(&def, Some(&saved.node), mine),
        account.as_deref(),
    );
    let mut shown = found.clone();
    shown.def = def.clone();
    connectors::annotate_source(&mut entry, &shown, true);
    connector_client::annotate_client(&mut entry, &def, saved.client.as_ref(), true);
    Ok(json_http_response(StatusCode::OK, entry.to_string()))
}

/// Runs a test's step right after a node save committed, as another admin's save landing then.
#[cfg(test)]
pub(crate) mod test_seam {
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    type Step = Box<dyn FnOnce() + Send>;
    static AFTER_SAVE: Mutex<BTreeMap<String, Step>> = Mutex::new(BTreeMap::new());

    /// The next save of the connector whose MCP server is `mcp_url` runs `step` once committed.
    pub(crate) fn on_next_save(mcp_url: &str, step: impl FnOnce() + Send + 'static) {
        AFTER_SAVE
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(mcp_url.to_string(), Box::new(step));
    }

    pub(super) fn after_save(mcp_url: &str) {
        let step = AFTER_SAVE
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(mcp_url);
        if let Some(step) = step {
            step();
        }
    }
}

/// Parses the body of `PUT /v1/connectors/{id}/node`. A body that carries a `client` may carry the
/// client secret, and a serde error can quote the value it rejected (before the body is even known
/// to be valid JSON), so every parse failure gets the same fixed message.
fn parse_put_node(body: &str) -> Result<PutNodeRequest, OrchestratorError> {
    serde_json::from_str(body).map_err(|_| {
        OrchestratorError::InvalidContract(
            "connector node body must be { enabled[, consent][, tenant: { subdomain } | null][, client: { clientId[, clientSecret] } | null] }"
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
    // A connector that signs in with an API key takes the member's key and nothing else (CN-7.6).
    if connectors::is_api_key(&def.template) {
        return put_api_key_connection(&state, &auth, &def, &body, &idempotency).await;
    }
    // A connector with fixed endpoints and the admin's client connects only through the node's code
    // exchange (CN-6.3): tokens the app holds cannot be tied to the company address, so they are
    // refused before the body is read. Nothing is written. The Entra route below is unchanged.
    if connector_client::is_rest_oauth(&def.def) {
        return Err(OrchestratorError::InvalidContract(connectors::exchange_only(&id)).into());
    }
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

/// `PUT /v1/connectors/{id}/connection` for an `api_key` connector: the member's own key (CN-7.6).
///
/// The body is `{ apiKey }` and nothing else. Every refusal is decided before anything is written
/// and is a fixed text that never quotes the key: not enabled (403), no company address (409), a
/// body or key that does not fit (400). Under the member's connection lock, ONE transaction writes
/// the `connector_connections` row and the `connector/<id>/api-key` vault row (`connection` kind,
/// sealed before the transaction, pinned to the filled binding hosts); the enable, the address and
/// the hosts are checked again inside it. Then this member's parks on the connector continue. No
/// request is made to the provider. The answer is the member's list entry: no key is in it.
async fn put_api_key_connection(
    state: &AppState,
    auth: &AuthContext,
    found: &connectors::ConnectorEntry,
    body: &str,
    idempotency: &str,
) -> Result<Response, ApiError> {
    let id = found.id();
    // A fixed message: a serde error can quote the value it rejected, and this body holds a key.
    let req = connectors::parse_api_key_body(body)?;
    let owner = vault_owner_user_id(SecretScope::User, auth.owner_user_id()).to_string();
    {
        let _gate = connectors::lock_connection(&owner, &id).await;
        let now = connectors::now_secs();
        let node = state.store.node_connector(&id)?;
        // Every refusal is decided here, before any write.
        let plan = connectors::plan_api_key_connection(&found.template, node.as_ref(), &req)?;
        if let Some(url) = &plan.mcp_url {
            // An MCP connector: the node writes the member's static MCP row (CN-7.7). The key is
            // staged and listed from memory, then written with the row in one transaction.
            super::mcp_connections::connect_api_key_mcp(
                state,
                auth,
                &id,
                url,
                &req.api_key,
                idempotency,
            )
            .await?;
            state
                .store
                .resolve_connection_needs_reauth(&owner, &id, "acted");
        } else {
            // Inside the transaction: the connector is still enabled, still at the address and still
            // names the hosts the key is about to be pinned to (an admin's save or a listing update
            // that landed meanwhile stores nothing).
            let (check_id, address, hosts) = (id.clone(), plan.address.clone(), plan.hosts.clone());
            let still_current = move |tx: &rusqlite::Connection| -> Result<(), OrchestratorError> {
                let node_now = connectors::node_connector_in(tx, &check_id)?;
                let enabled = node_now.as_ref().is_some_and(|n| n.enabled);
                let same_address =
                    connectors::address_key(node_now.and_then(|n| n.tenant_subdomain).as_deref())
                        == address;
                let same_file = connectors::entry_in(tx, &check_id)?.is_some_and(|e| {
                    connectors::is_api_key(&e.def) && connectors::binding_hosts(&e.def) == hosts
                });
                if enabled && same_address && same_file {
                    Ok(())
                } else {
                    Err(OrchestratorError::Conflict(format!(
                    "the `{check_id}` connector changed while the key was being saved: read it again and retry"
                )))
                }
            };
            state.store.commit_api_key_connection(
                &state.vault,
                &connectors::ApiKeyCommit {
                    owner: &owner,
                    connector_id: &id,
                    address: &plan.address,
                    hosts: &plan.hosts,
                    api_key: &req.api_key,
                    now,
                },
                &still_current,
            )?;
            // The member's reconnect notification (if any) is resolved: they entered the key again.
            state
                .store
                .resolve_connection_needs_reauth(&owner, &id, "acted");
        }
    }
    // The lock is released: a resumed intent takes it. Only this owner's parks on this connector
    // continue (the key carries the owner).
    super::reauth::resume_key(state, &crate::connector_park::park_key(&owner, &id));
    Ok(json_http_response(
        StatusCode::OK,
        entry_for(state, auth, found)?.to_string(),
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
    let owner = vault_owner_user_id(SecretScope::User, auth.owner_user_id()).to_string();
    if state.store.mcp_policy(&id)?.is_some() {
        // An MCP connector with an API key: its static row and its header row go, and nothing is
        // asked of the provider (CN-7.7). An OAuth MCP connector disconnects at the generic route.
        if connectors::is_api_key(&def.template) {
            let _gate = connectors::lock_connection(&owner, &id).await;
            connectors::disconnect_mcp_api_key(&state.store, &state.vault, &owner, &id)?;
            return Ok(json_http_response(
                StatusCode::OK,
                entry_for(&state, &auth, &def)?.to_string(),
            ));
        }
        return Err(OrchestratorError::InvalidContract(format!(
            "`{id}` is an MCP connector; disconnect it with `vam mcp disconnect {id}` (/v1/mcp/connections)"
        ))
        .into());
    }
    let _gate = connectors::lock_connection(&owner, &id).await;
    // An `oauth` connector's refresh token is revoked only when its file declares a revocation
    // endpoint (RFC 7009, public client) and the grant was issued to the client the node has now:
    // revocation authenticates as the issuing client, so a grant of a replaced client is deleted
    // locally with nothing sent. Read before the rows go; sent once they are gone.
    let plan = if connector_client::is_rest_oauth(&def.def) {
        connectors::prepare_rest_revoke(&state.store, &state.vault, &owner, &id)
    } else {
        connectors::RestRevokePlan::Nothing
    };
    connectors::disconnect(&state.store, &state.vault, &owner, &id)?;
    // The answer says when the provider's grant may remain: not revoked (another or an unknown
    // client issued it, the admin cleared the client, or its pins no longer match) or the
    // revocation did not go through.
    let mut may_remain = matches!(plan, connectors::RestRevokePlan::Skipped);
    if let connectors::RestRevokePlan::Send(r) = plan {
        let sent = tokio::task::spawn_blocking(move || {
            crate::mcp_oauth_refresh::revoke_oauth_connector_refresh_token(
                &r.revocation_endpoint,
                &r.client_id,
                &r.refresh_token,
            )
        })
        .await;
        may_remain = !sent.is_ok_and(|a| a.succeeded);
    }
    let mut entry = entry_for(&state, &auth, &def)?;
    if may_remain {
        entry["providerGrantMayRemain"] = Value::Bool(true);
    }
    Ok(json_http_response(StatusCode::OK, entry.to_string()))
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
    // An API key has nothing to refresh and nothing at the provider to ask: the answer is the
    // stored state (the row, the key's pin and the address), with no request made.
    if connectors::is_api_key(&def.template) {
        return Ok(json_http_response(
            StatusCode::OK,
            entry_for(&state, &auth, &def)?.to_string(),
        ));
    }
    let owner = vault_owner_user_id(SecretScope::User, auth.owner_user_id()).to_string();
    // The lock moves into the worker: a client that goes away mid-refresh does not release it while
    // the refresh is still running, so a sign-in cannot interleave with a stale check result.
    let gate = connectors::lock_connection(&owner, &id).await;
    let (store, vault, cfg) = (
        state.store.clone(),
        state.vault.clone(),
        connectors::config(),
    );
    let (owner2, id2) = (owner.clone(), id.clone());
    let outcome = tokio::task::spawn_blocking(move || {
        let _gate = gate;
        connectors::check_connection(
            &store,
            &vault,
            &cfg,
            &owner2,
            &id2,
            connectors::now_secs(),
            connectors::refresh_rest_token,
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
/// app got from the browser sign-in (CN-5.3, CN-6.3, D29, locked decision 13 as amended).
///
/// For every connector with `client: org`, MCP or REST, with a secret or without: the enabled
/// connector's saved client and nothing else. The body is `{ code, codeVerifier, redirectUri }` and
/// names no endpoint or client. The node posts the code to the token endpoint it pinned at Enable
/// (with the org secret when the org keeps one, with none otherwise) and connects the caller: an MCP
/// row, or for a REST connector (`profile: oauth`) a `connector_connections` row. Every refusal is
/// decided before anything is sent or written; a provider that refuses the client (`invalid_client`)
/// is a 409 naming the admin's client, a refused code a 400, an unreachable provider a 502. The
/// answer is the caller's list entry: no code, verifier, secret or token is in it, or in any error.
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
    // An API key has no code to exchange: the member enters the key itself (CN-7.6).
    if connectors::is_api_key(&found.template) {
        return Err(OrchestratorError::InvalidContract(connectors::api_key_only(&id)).into());
    }
    // A fixed message: a serde error can quote the value it rejected, and this body holds a code.
    let req: connector_client::CodeExchangeRequest = serde_json::from_str(&body).map_err(|_| {
        OrchestratorError::InvalidContract(
            "code exchange body must be { code, codeVerifier, redirectUri }".into(),
        )
    })?;
    let org = state.store.org_connector(&id)?.ok_or_else(|| {
        OrchestratorError::InvalidContract(format!(
            "`{id}` has no code exchange on the node: it signs in with the app's own exchange"
        ))
    })?;
    // Enabled on this node, its own URL (an MCP connector), or enabled with its company address (a
    // REST connector, which `gate_mcp_write` does not cover).
    if org.is_rest() {
        let enabled = state.store.node_connector(&id)?.is_some_and(|n| n.enabled);
        if !enabled {
            return Err(OrchestratorError::PermissionDenied(format!(
                "the `{id}` connector is not enabled on this node; ask your admin (`vam connectors`)"
            ))
            .into());
        }
    } else {
        connectors::gate_mcp_write(&state.store, &id, None, None)?;
    }
    let client = org.client.clone().ok_or_else(|| {
        OrchestratorError::Conflict(format!(
            "`{id}` has no OAuth client saved by your admin yet"
        ))
    })?;
    if org.keeps_secret && !client.secret_set {
        return Err(OrchestratorError::Conflict(format!(
            "`{id}` has no client secret saved by your admin yet"
        ))
        .into());
    }
    connector_client::check_code_exchange(&req)?;
    let owner = vault_owner_user_id(SecretScope::User, auth.owner_user_id()).to_string();
    let (store, vault) = (state.store.clone(), state.vault.clone());
    let (creator, id2) = (auth.owner_user_id().to_string(), id.clone());

    if org.is_rest() {
        // The connection's lock: a refresh of this member's connection does not interleave with the
        // exchange. The exchange and every write after it run to the end on a worker that owns the
        // lock, so a caller that goes away cannot leave a spent code or refresh token unstored. The
        // worker also resumes the runs parked on this member's connection once the connection is
        // committed and the lock is released (a resumed intent takes it), whether or not this
        // request is still there to see it.
        #[cfg(test)]
        connectors::test_signal::fire(&format!("exchange-read-client:{owner}:{id}:{idempotency}"));
        let gate = connectors::lock_connection(&owner, &id).await;
        let owner2 = owner.clone();
        let resume_state = state.clone();
        let park = crate::connector_park::park_key(&owner, &id);
        tokio::task::spawn_blocking(move || {
            let c = connector_exchange::CodeConnect {
                owner: &owner2,
                creator: &creator,
                connector_id: &id2,
                org: &org,
                client: &client,
                request: &req,
                idempotency_key: &idempotency,
            };
            connector_exchange::exchange_and_connect_rest(&store, &c, &vault, gate, || {
                // Runs parked on this member's connection continue (the key carries the owner).
                super::reauth::resume_key(&resume_state, &park);
            })
        })
        .await
        .map_err(|_| {
            ApiError::from(OrchestratorError::Core(
                "connector code exchange task failed".into(),
            ))
        })??;
        return Ok(json_http_response(
            StatusCode::OK,
            entry_for(&state, &auth, &found)?.to_string(),
        ));
    }

    // The connection's credential gate: a refresh of this member's row does not interleave with the
    // exchange, and a call that began with the old tokens retries with the new ones.
    let mut gate =
        crate::host_tools::lock_mcp_credentials_for(&state.store, SecretScope::User, &owner, &id)
            .await
            .map_err(|_| {
                OrchestratorError::CorruptState("mcp credential lock unavailable".into())
            })?;
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
                tenant_subdomain: None,
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
        /// While set, `/revoke` requires this client id and secret (RFC 7009 §2.1 client
        /// authentication), in the body or as HTTP Basic, and answers anything else with a 401
        /// `invalid_client` without taking a queued reply.
        revoke_auth: Arc<Mutex<Option<(String, String)>>>,
    }

    /// Whether a request authenticated as `client_id` with `secret`: exactly the body fields with no
    /// `Authorization` header, or exactly that HTTP Basic header with no secret in the body.
    fn client_authenticated(seen: &Seen, client_id: &str, secret: &str) -> bool {
        use base64::Engine as _;
        let basic = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(format!("{client_id}:{secret}"))
        );
        let in_body = seen.authorization.is_none()
            && seen.form.get("client_id").map(String::as_str) == Some(client_id)
            && seen.form.get("client_secret").map(String::as_str) == Some(secret);
        let as_basic = seen.authorization.as_deref() == Some(basic.as_str())
            && !seen.form.contains_key("client_secret");
        in_body || as_basic
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
        let revoke_auth = Arc::new(Mutex::new(None::<(String, String)>));
        let (log, queue, held) = (seen.clone(), replies.clone(), hold.clone());
        let required = revoke_auth.clone();
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
                let request = Seen {
                    path,
                    form,
                    authorization,
                };
                let refused = request.path == "/revoke"
                    && required
                        .lock()
                        .unwrap()
                        .as_ref()
                        .is_some_and(|(id, secret)| !client_authenticated(&request, id, secret));
                log.lock().unwrap().push(request);
                if refused {
                    let payload = r#"{"error":"invalid_client"}"#;
                    let reply = format!(
                        "HTTP/1.1 401 X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                        payload.len()
                    );
                    stream.write_all(reply.as_bytes()).unwrap();
                    continue;
                }
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
            revoke_auth,
        }
    }

    impl Provider {
        /// From now on `/revoke` refuses a request that does not authenticate as this client.
        fn require_revoke_auth(&self, client_id: &str, secret: &str) {
            *self.revoke_auth.lock().unwrap() = Some((client_id.into(), secret.into()));
        }
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
        // Not even the node's own client and endpoint pass: an `oauth` block on this route is refused
        // for every connector that uses the admin's client (the node exchanges the code), so the row
        // the exchange made keeps the node's authorization server, revocation endpoint and method.
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
        assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
        assert!(b.to_string().contains("exchange"), "{b}");
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
    async fn an_exchange_rechecks_the_enable_once_it_holds_the_gate() {
        let n = xchg_node("xchg-off-at-gate");
        let (s, b) = put_client(&n, "c-1", Some(SECRET)).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        let client = n.store.node_client("filebox").unwrap().unwrap();
        // The route passed its enable check, then waited for the member's credential gate; the
        // admin turned the connector off meanwhile. Holding the gate, the exchange checks again:
        // a 403, no secret read, no code sent, nothing written.
        let (s, b) = call(
            &n.app,
            "PUT",
            "/v1/connectors/filebox/node",
            "orgadmin-secret",
            Some(serde_json::json!({ "enabled": false })),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        n.token.reply(200, tokens_reply());
        let e = exchange_made_with(&n, "ada", client).await.err().unwrap();
        assert_eq!(e.status, StatusCode::FORBIDDEN, "{}", e.message);
        assert!(n.token.requests().is_empty(), "{:?}", n.token.requests());
        assert!(n
            .store
            .get_mcp_connection(SecretScope::User, "ada", "filebox")
            .is_err());
        assert!(member_vault_ids(&n.vault).is_empty());
    }

    #[tokio::test]
    async fn the_node_answer_is_the_node_and_client_its_save_committed() {
        let n = xchg_node("xchg-save-snapshot");
        // Another admin's write lands right after this save commits (here: the client row goes).
        let store = n.store.clone();
        super::test_seam::on_next_save("https://mcp.xchg-save-snapshot.example/mcp", move || {
            store
                .connection
                .lock()
                .unwrap()
                .execute(
                    "DELETE FROM node_connector_clients WHERE connector_id = 'filebox'",
                    [],
                )
                .unwrap();
        });
        let (s, b) = put_client(&n, "c-1", Some(SECRET)).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        // The answer pairs this save's node row with this save's client, not with the later state.
        assert_eq!(b["node"]["enabled"], true, "{b}");
        assert_eq!(b["node"]["configured"], true, "{b}");
        assert_eq!(b["node"]["client"]["clientId"], "c-1", "{b}");
        assert_eq!(n.store.node_client("filebox").unwrap(), None);
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

        // The connector turned off while the provider answered: the same. (Turned off before the
        // exchange holds the gate, it is a 403 and nothing is sent:
        // `an_exchange_rechecks_the_enable_once_it_holds_the_gate`.)
        let current = n.store.node_client("filebox").unwrap().unwrap();
        n.token.reply(
            200,
            serde_json::json!({ "access_token": "LATE-A", "refresh_token": "LATE-R" }),
        );
        let (arrived, release) = n.token.hold_next();
        let admin = async {
            tokio::task::spawn_blocking(move || arrived.recv().unwrap())
                .await
                .unwrap();
            let (s, b) = call(
                &n.app,
                "PUT",
                "/v1/connectors/filebox/node",
                "orgadmin-secret",
                Some(serde_json::json!({ "enabled": false })),
            )
            .await;
            assert_eq!(s, StatusCode::OK, "{b}");
            release.send(()).unwrap();
        };
        let (result, ()) = tokio::join!(exchange_made_with(&n, "olga", current), admin);
        let e = result.err().unwrap();
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
    async fn a_moved_connectors_old_grant_of_the_admins_client_is_revoked_with_the_org_secret() {
        use crate::mcp_registry::{
            McpAuthMode, McpOauthMetadata, McpOauthRegistrationMethod, McpOauthStatus,
        };
        let tag = "xchg-moved-auth";
        let n = xchg_node(tag);
        let (s, b) = put_client(&n, "c-1", Some(SECRET)).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        // Ada signed in on the connector's previous server with the admin's current client; the
        // update moved the server, so her row no longer matches the connector's URL.
        let oauth = McpOauthMetadata {
            resource: "https://mcp.old-home.example/mcp".into(),
            authorization_server: format!("https://auth.{tag}.example"),
            token_endpoint: format!("https://auth.{tag}.example/token"),
            revocation_endpoint: Some(format!("https://auth.{tag}.example/revoke")),
            client_id: "c-1".into(),
            registration_method: McpOauthRegistrationMethod::Manual,
            scopes: vec![],
            expires_at: None,
            status: McpOauthStatus::NeedsReauth,
            last_refresh_at: None,
            last_refresh_error: Some("the connector moved".into()),
        };
        let stale = n
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
        crate::mcp_registry::store_refreshed_mcp_tokens(&n.vault, &stale, "OLD-A", Some("OLD-R"))
            .unwrap();
        // The provider refuses a revocation that does not authenticate the client.
        n.token.require_revoke_auth("c-1", SECRET);
        let basic = {
            use base64::Engine as _;
            format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode(format!("c-1:{SECRET}"))
            )
        };

        // A disconnect of that row revokes with the org secret, the way the node's client is
        // pinned to (the provider omits the revocation methods: HTTP Basic).
        n.token.reply(200, serde_json::Value::Null);
        let (vault, row) = (n.vault.clone(), stale.clone());
        let attempt = tokio::task::spawn_blocking(move || {
            crate::mcp_oauth_refresh::best_effort_revoke_oauth_refresh(&vault, &row)
        })
        .await
        .unwrap();
        assert!(attempt.succeeded, "{attempt:?}");
        let seen = n.token.requests();
        let revoke = seen.last().unwrap();
        assert_eq!(revoke.path, "/revoke");
        assert_eq!(revoke.form["token"], "OLD-R");
        assert_eq!(revoke.authorization.as_deref(), Some(basic.as_str()));

        // The sign-in on the new server replaces the row, and the old grant is revoked the same
        // way once the replacement has committed.
        n.token.reply(200, tokens_reply());
        n.token.reply(200, serde_json::Value::Null);
        let (s, b) = exchange(
            &n,
            "member-secret",
            "filebox",
            exchange_body("squrell://oauth"),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        let seen = n.token.requests();
        let revoke = seen.last().unwrap();
        assert_eq!(revoke.path, "/revoke", "{seen:?}");
        assert_eq!(revoke.form["token"], "OLD-R");
        assert_eq!(revoke.authorization.as_deref(), Some(basic.as_str()));
        assert!(
            !revoke.form.contains_key("client_secret"),
            "{:?}",
            revoke.form
        );
        assert_eq!(
            row_of(&n, "ada").url,
            format!("https://mcp.{tag}.example/mcp")
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
    async fn an_exchanges_failed_listing_leaves_the_mark_of_a_client_changed_and_changed_back() {
        use crate::mcp_registry::{McpOauthStatus::NeedsReauth, TOOLS_PENDING_REASON};
        let tools_ok = Arc::new(AtomicBool::new(false));
        let n = xchg_node_with("xchg-aba-listing", tools_ok.clone());
        let (s, b) = put_client(&n, "c-1", Some(SECRET)).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        // The exchange commits the sign-in tools-pending; this is the row and the credential
        // generation its adoption holds while `tools/list` is in flight.
        n.token.reply(200, tokens_reply());
        let (s, b) = exchange(
            &n,
            "member-secret",
            "filebox",
            exchange_body("squrell://oauth"),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_GATEWAY, "{b}");
        let committed = row_of(&n, "ada");
        assert_eq!(
            committed
                .oauth
                .as_ref()
                .unwrap()
                .last_refresh_error
                .as_deref(),
            Some(TOOLS_PENDING_REASON)
        );
        let generation = crate::host_tools::lock_mcp_credentials(&n.store, &committed)
            .await
            .unwrap()
            .generation_handle();
        let policy = n.store.mcp_policy("filebox").unwrap().unwrap();
        // Meanwhile the admin changes c-1 to c-2 and back: the member's credential generation is
        // unchanged, but the row now carries the admin's reconnect mark.
        let (s, b) = put_client(&n, "c-2", Some("OTHER-SECRET-qqqqqqqqqqqq")).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        let (s, b) = put_client(&n, "c-1", Some(SECRET)).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        let marked = row_of(&n, "ada");
        assert_eq!(marked.oauth.as_ref().unwrap().status, NeedsReauth);
        assert_ne!(
            marked.oauth.as_ref().unwrap().last_refresh_error.as_deref(),
            Some(TOOLS_PENDING_REASON)
        );
        // The exchange's listing fails now: a 409, and the admin's mark is not replaced by the
        // tools-pending mark the tools-refresh route would finish.
        let e = super::super::mcp_connections::adopt_tools_and_reconnect(
            &n.state,
            &committed,
            &policy,
            &generation,
        )
        .await
        .err()
        .unwrap();
        assert_eq!(e.status, StatusCode::CONFLICT, "{}", e.message);
        assert_eq!(row_of(&n, "ada").oauth, marked.oauth);
        // So the tools-refresh route does not adopt the old sign-in.
        tools_ok.store(true, std::sync::atomic::Ordering::SeqCst);
        let (s, b) = call(
            &n.app,
            "POST",
            "/v1/mcp/connections/filebox/refresh?scope=user",
            "member-secret",
            None,
        )
        .await;
        assert_ne!(s, StatusCode::OK, "{b}");
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
        // The provider advertises `client_secret_post`: the org secret is in the body and there is
        // no Authorization header (the Basic wire test covers the other method).
        assert_eq!(
            grant.form.get("client_secret").map(String::as_str),
            Some(SECRET),
            "the org secret authenticates the refresh"
        );
        assert!(grant.authorization.is_none(), "{:?}", grant.authorization);
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

    // ---- the company address (CN-6.2) ----------------------------------------------------------

    async fn put_zendesk(
        app: &axum::Router,
        secret: &str,
        body: serde_json::Value,
    ) -> (StatusCode, serde_json::Value) {
        call(
            app,
            "PUT",
            "/v1/connectors/zendesk/node",
            secret,
            Some(body),
        )
        .await
    }

    async fn zendesk_entry(app: &axum::Router, secret: &str) -> serde_json::Value {
        let (_, l) = call(app, "GET", "/v1/connectors", secret, None).await;
        l["connectors"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["id"] == "zendesk")
            .unwrap()
            .clone()
    }

    #[tokio::test]
    async fn enable_needs_one_dns_label_and_no_secret_and_fills_every_host() {
        let (app, store, _vault) = app_world();
        crate::connector_client::test_support::install_company_address_rows(&store);
        let client = serde_json::json!({ "clientId": "c1" });
        // No address.
        let (s, b) = put_zendesk(
            &app,
            "orgadmin-secret",
            serde_json::json!({ "enabled": true, "client": client }),
        )
        .await;
        assert_eq!(s, StatusCode::CONFLICT, "{b}");
        assert!(b.to_string().contains("company address"), "{b}");
        // Not one DNS label; the value is never quoted.
        for bad in ["acme.evil", "-acme", "a_b", "xn--acme", &"a".repeat(64)] {
            let (s, b) = put_zendesk(&app, "orgadmin-secret", serde_json::json!({ "enabled": true, "tenant": { "subdomain": bad }, "client": client })).await;
            assert_eq!(s, StatusCode::BAD_REQUEST, "{bad}: {b}");
            assert!(b.to_string().contains("one DNS label"), "{b}");
            assert!(!b.to_string().contains(bad), "{b}");
        }
        // A secret for a client that keeps none.
        let (s, b) = put_zendesk(&app, "orgadmin-secret", serde_json::json!({ "enabled": true, "tenant": { "subdomain": "acme" }, "client": { "clientId": "c1", "clientSecret": "s3cret" } })).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
        assert!(
            b.to_string().contains("secret") && !b.to_string().contains("s3cret"),
            "{b}"
        );
        // A member.
        let (s, _) = put_zendesk(&app, "member-secret", serde_json::json!({ "enabled": true, "tenant": { "subdomain": "acme" }, "client": client })).await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        // Nothing was written by any refusal.
        assert!(store.node_connector("zendesk").unwrap().is_none());
        assert!(store.node_client("zendesk").unwrap().is_none());
        let before = zendesk_entry(&app, "member-secret").await;
        assert_eq!(before["state"], "askAdmin");
        assert!(before["node"].get("tenant").is_none());

        // Enabled with " Acme ": trimmed and lowercased.
        let (s, b) = put_zendesk(&app, "orgadmin-secret", serde_json::json!({ "enabled": true, "tenant": { "subdomain": " Acme " }, "client": client })).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        for who in ["orgadmin-secret", "member-secret"] {
            let e = zendesk_entry(&app, who).await;
            assert_eq!(e["node"]["tenant"]["subdomain"], "acme");
            assert_eq!(e["node"]["tenant"]["hosts"][0], "acme.zendesk.com");
            assert_eq!(e["bindings"][0]["hosts"][0], "acme.zendesk.com");
            assert_eq!(
                e["provider"]["endpoints"]["token"],
                "https://acme.zendesk.com/oauth/tokens"
            );
            assert_eq!(
                e["node"]["client"]["tokenEndpoint"],
                "https://acme.zendesk.com/oauth/tokens"
            );
            assert_eq!(
                e["node"]["client"]["authorizeEndpoint"],
                "https://acme.zendesk.com/oauth/authorizations/new"
            );
            assert!(!e.to_string().contains("{subdomain}"), "{e}");
        }
        // The answer of the save is filled too.
        assert!(!b.to_string().contains("{subdomain}"), "{b}");

        // Disabling keeps the address and the client; enabling alone needs neither.
        let (s, _) = put_zendesk(
            &app,
            "orgadmin-secret",
            serde_json::json!({ "enabled": false }),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        let (s, b) = put_zendesk(
            &app,
            "orgadmin-secret",
            serde_json::json!({ "enabled": true }),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(b["node"]["tenant"]["subdomain"], "acme");
    }

    #[tokio::test]
    async fn a_new_address_needs_the_client_again_and_null_only_when_disabling() {
        let (app, store, _vault) = app_world();
        crate::connector_client::test_support::install_company_address_rows(&store);
        let client = serde_json::json!({ "clientId": "c1" });
        let (s, b) = put_zendesk(&app, "orgadmin-secret", serde_json::json!({ "enabled": true, "tenant": { "subdomain": "acme" }, "client": client })).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        // Another company without the client: 409, nothing changes.
        let (s, b) = put_zendesk(
            &app,
            "orgadmin-secret",
            serde_json::json!({ "enabled": true, "tenant": { "subdomain": "globex" } }),
        )
        .await;
        assert_eq!(s, StatusCode::CONFLICT, "{b}");
        assert_eq!(
            store
                .node_connector("zendesk")
                .unwrap()
                .unwrap()
                .tenant_subdomain
                .as_deref(),
            Some("acme")
        );
        // The same address again asks for nothing.
        let (s, b) = put_zendesk(
            &app,
            "orgadmin-secret",
            serde_json::json!({ "enabled": true, "tenant": { "subdomain": "acme" } }),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        // With the client: both move together.
        let (s, b) = put_zendesk(&app, "orgadmin-secret", serde_json::json!({ "enabled": true, "tenant": { "subdomain": "globex" }, "client": client })).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(
            b["node"]["client"]["tokenEndpoint"],
            "https://globex.zendesk.com/oauth/tokens"
        );
        // Removing the address is only for a disable.
        let (s, b) = put_zendesk(
            &app,
            "orgadmin-secret",
            serde_json::json!({ "enabled": true, "tenant": null }),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
        let (s, b) = put_zendesk(
            &app,
            "orgadmin-secret",
            serde_json::json!({ "enabled": false, "tenant": null, "client": null }),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert!(b["node"].get("tenant").is_none(), "{b}");
        // Enabling again asks for the address.
        let (s, b) = put_zendesk(
            &app,
            "orgadmin-secret",
            serde_json::json!({ "enabled": true, "client": client }),
        )
        .await;
        assert_eq!(s, StatusCode::CONFLICT, "{b}");
        assert!(b.to_string().contains("company address"), "{b}");
    }

    #[tokio::test]
    async fn a_connector_without_a_company_address_takes_none() {
        let app = app();
        let (s, b) = call(
            &app,
            "PUT",
            "/v1/connectors/linear/node",
            "orgadmin-secret",
            Some(serde_json::json!({ "enabled": true, "tenant": { "subdomain": "acme" } })),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
        assert!(b.to_string().contains("takes no company address"), "{b}");
    }

    // ---- the node's code exchange for a company-address connector (CN-6.3) -----------------------

    /// `zendesk` installed and enabled for `sub` with the client `zc1` (no secret), its token host
    /// answered by a loopback provider.
    async fn zendesk_node(sub: &str) -> (axum::Router, AppState, Provider) {
        let (app, state) = app_world_with_state();
        crate::connector_client::test_support::install_company_address_rows(&state.store);
        let p = provider();
        crate::mcp_oauth_refresh::test_seam::serve(&format!("{sub}.zendesk.com"), p.base.clone());
        let (s, b) = put_zendesk(
            &app,
            "orgadmin-secret",
            serde_json::json!({
                "enabled": true, "tenant": { "subdomain": sub }, "client": { "clientId": "zc1" }
            }),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        (app, state, p)
    }

    async fn zendesk_exchange(
        app: &axum::Router,
        redirect: &str,
        key: &str,
    ) -> (StatusCode, serde_json::Value) {
        call_with_key(
            app,
            "POST",
            "/v1/connectors/zendesk/connection/oauth/code",
            "member-secret",
            Some(exchange_body(redirect)),
            key,
        )
        .await
    }

    fn nothing_stored(state: &AppState) {
        assert!(state
            .store
            .connector_connection("ada", "zendesk")
            .unwrap()
            .is_none());
        let ids: Vec<String> = state
            .vault
            .list_metadata()
            .unwrap()
            .into_iter()
            .map(|m| m.binding_id)
            .filter(|id| id.starts_with("connector/zendesk/oauth/"))
            .collect();
        assert!(ids.is_empty(), "{ids:?}");
    }

    #[tokio::test]
    async fn the_node_exchanges_the_code_with_no_secret_and_keeps_a_rest_connection() {
        let (app, state, p) = zendesk_node("xrest-ok").await;
        p.reply(200, tokens_reply());
        let (s, b) = zendesk_exchange(&app, "squrell://oauth", "zk-1").await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_no_value("the exchange answer", &b.to_string());
        assert_eq!(b["state"], "connected", "{b}");
        // The one request the provider saw: the admin's client, the verifier, the redirect URI; no
        // secret, no resource, no Authorization header.
        let seen = p.requests();
        assert_eq!(seen.len(), 1);
        let r = &seen[0];
        assert_eq!(r.path, "/oauth/tokens");
        assert_eq!(r.authorization, None);
        assert_eq!(r.form["grant_type"], "authorization_code");
        assert_eq!(r.form["client_id"], "zc1");
        assert_eq!(r.form["code"], CODE);
        assert_eq!(r.form["code_verifier"], VERIFIER);
        assert_eq!(r.form["redirect_uri"], "squrell://oauth");
        for key in ["client_secret", "resource", "scope"] {
            assert!(!r.form.contains_key(key), "{key} in {:?}", r.form);
        }
        // The REST connection: the address it was made under, the filled endpoint, tokens pinned.
        let row = state
            .store
            .connector_connection("ada", "zendesk")
            .unwrap()
            .unwrap();
        assert_eq!(row.tenant_id, "xrest-ok");
        assert_eq!(
            row.token_endpoint,
            "https://xrest-ok.zendesk.com/oauth/tokens"
        );
        assert_eq!(row.label, None);
        for binding in ["refresh-token", "access-token"] {
            let meta = state
                .vault
                .get_metadata_at(
                    crate::secrets::SecretScope::User,
                    "ada",
                    &format!("connector/zendesk/oauth/{binding}"),
                )
                .unwrap();
            assert_eq!(meta.allowed_destinations, vec!["xrest-ok.zendesk.com"]);
        }
        let list = call(&app, "GET", "/v1/connectors", "member-secret", None)
            .await
            .1;
        assert_no_value("the listing", &list.to_string());
        // The app cannot hand the node tokens for this connector.
        let (s, b) = call(
            &app,
            "PUT",
            "/v1/connectors/zendesk/connection",
            "member-secret",
            Some(serde_json::json!({ "accessToken": "app-a", "refreshToken": "app-r" })),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
        assert!(b.to_string().contains("exchange"), "{b}");
        // The same Idempotency-Key sends nothing again.
        let (s, b) = zendesk_exchange(&app, "squrell://oauth", "zk-1").await;
        assert_eq!(s, StatusCode::CONFLICT, "{b}");
        assert_eq!(p.requests().len(), 1);
        // The admin moves to another address and saves the client again: the member reconnects, and
        // no request goes to the old or the new host.
        let (s, b) = put_zendesk(
            &app,
            "orgadmin-secret",
            serde_json::json!({
                "enabled": true, "tenant": { "subdomain": "xrest-other" },
                "client": { "clientId": "zc1" }
            }),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(
            zendesk_entry(&app, "member-secret").await["state"],
            "needsReconnect"
        );
        let (s, b) = call(
            &app,
            "POST",
            "/v1/connectors/zendesk/connection/check",
            "member-secret",
            None,
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(b["state"], "needsReconnect", "{b}");
        assert_eq!(p.requests().len(), 1, "no refresh, nothing to the new host");
    }

    #[tokio::test]
    async fn every_refusal_of_a_company_address_exchange_sends_and_writes_nothing() {
        // Not enabled yet: 403.
        let (app, state) = app_world_with_state();
        crate::connector_client::test_support::install_company_address_rows(&state.store);
        let (s, b) = zendesk_exchange(&app, "squrell://oauth", "zk-a").await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{b}");
        nothing_stored(&state);

        let (app, state, p) = zendesk_node("xrest-no").await;
        // A body that is not the app's, a redirect URI that is not the app's.
        let (s, _) = call(
            &app,
            "POST",
            "/v1/connectors/zendesk/connection/oauth/code",
            "member-secret",
            Some(serde_json::json!({ "code": CODE, "tokenEndpoint": "https://evil.example/t" })),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        let (s, _) = zendesk_exchange(&app, "https://evil.example/callback", "zk-b").await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        // The client was pinned to another endpoint than the connector names now: 409 before a request.
        state
            .store
            .connection
            .lock()
            .unwrap()
            .execute(
                "UPDATE node_connector_clients SET token_endpoint = 'https://other.zendesk.com/oauth/tokens'",
                [],
            )
            .unwrap();
        let (s, b) = zendesk_exchange(&app, "squrell://oauth", "zk-c").await;
        assert_eq!(s, StatusCode::CONFLICT, "{b}");
        assert!(b.to_string().contains("save the client again"), "{b}");
        assert!(p.requests().is_empty());
        nothing_stored(&state);

        // Turned off again: 403.
        let (app, _state, p) = zendesk_node("xrest-off").await;
        let (s, _) = put_zendesk(
            &app,
            "orgadmin-secret",
            serde_json::json!({ "enabled": false }),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        let (s, _) = zendesk_exchange(&app, "squrell://oauth", "zk-d").await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        assert!(p.requests().is_empty());

        // The provider refuses the code: 400, nothing stored.
        let (app, state, p) = zendesk_node("xrest-bad").await;
        p.reply(400, serde_json::json!({ "error": "invalid_grant" }));
        let (s, b) = zendesk_exchange(&app, "squrell://oauth", "zk-e").await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
        assert_eq!(p.requests().len(), 1);
        nothing_stored(&state);

        // The provider issues no refresh token: refused, nothing stored.
        let (app, state, p) = zendesk_node("xrest-norefresh").await;
        p.reply(
            200,
            serde_json::json!({ "access_token": NEW_ACCESS, "token_type": "Bearer" }),
        );
        let (s, b) = zendesk_exchange(&app, "squrell://oauth", "zk-f").await;
        assert_eq!(s, StatusCode::BAD_GATEWAY, "{b}");
        assert!(b.to_string().contains("refresh token"), "{b}");
        assert_no_value("the answer", &b.to_string());
        nothing_stored(&state);

        // A connector with the publisher's own client keeps the app's exchange.
        let (s, b) = call(
            &app,
            "POST",
            "/v1/connectors/linear/connection/oauth/code",
            "member-secret",
            Some(exchange_body("squrell://oauth")),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
        assert!(b.to_string().contains("app's own exchange"), "{b}");
    }

    // ---- the release review of #112: the REST exchange, disconnect and the connect route -----------

    #[tokio::test]
    async fn a_code_is_not_sent_to_a_client_the_admin_replaced_while_the_exchange_waited_for_the_lock(
    ) {
        let (app, state, p) = zendesk_node("xrest-wait").await;
        p.reply(200, tokens_reply());
        // The member's connection is busy (a refresh, another sign-in): the exchange passes the
        // route's checks and waits for the lock.
        let held = crate::connectors::lock_connection("ada", "zendesk").await;
        // Scoped to this request's key: other Zendesk exchanges as `ada` run in parallel.
        let read =
            crate::connectors::test_signal::arm("exchange-read-client:ada:zendesk:zk-lock-wait");
        let waiting = {
            let app = app.clone();
            tokio::spawn(
                async move { zendesk_exchange(&app, "squrell://oauth", "zk-lock-wait").await },
            )
        };
        // The exchange has read the old client and is about to wait for the lock.
        tokio::task::spawn_blocking(move || read.recv_timeout(std::time::Duration::from_secs(10)))
            .await
            .unwrap()
            .expect("the exchange read the client before the admin's save");
        // Meanwhile the admin saves another client for the same address.
        let (s, b) = put_zendesk(
            &app,
            "orgadmin-secret",
            serde_json::json!({
                "enabled": true, "tenant": { "subdomain": "xrest-wait" },
                "client": { "clientId": "zc2" }
            }),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        drop(held);
        let (s, b) = waiting.await.unwrap();
        assert_eq!(s, StatusCode::CONFLICT, "{b}");
        assert!(
            p.requests().is_empty(),
            "the code never left the node: {:?}",
            p.requests()
        );
        nothing_stored(&state);
    }

    /// `zendesk` as a connector with fixed endpoints and no company address (no `tenant`).
    async fn fixed_endpoint_node(host: &str, revoke: bool) -> (axum::Router, AppState, Provider) {
        let (app, state) = app_world_with_state();
        crate::connector_client::test_support::install_company_address_variant(&state.store, |j| {
            j.as_object_mut().unwrap().remove("tenant");
            let mut endpoints = serde_json::json!({
                "authorize": format!("https://auth.{host}/oauth/authorize"),
                "token": format!("https://auth.{host}/oauth/token"),
            });
            if revoke {
                endpoints["revoke"] = format!("https://auth.{host}/oauth/revoke").into();
            }
            j["provider"]["endpoints"] = endpoints;
            j["bindings"][0]["hosts"] = serde_json::json!([format!("api.{host}")]);
        });
        let p = provider();
        crate::mcp_oauth_refresh::test_seam::serve(&format!("auth.{host}"), p.base.clone());
        let (s, b) = put_zendesk(
            &app,
            "orgadmin-secret",
            serde_json::json!({ "enabled": true, "client": { "clientId": "zc1" } }),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        (app, state, p)
    }

    #[tokio::test]
    async fn a_connector_with_fixed_endpoints_and_no_company_address_completes_the_exchange() {
        let (app, state, p) = fixed_endpoint_node("fixedrest.example", false).await;
        p.reply(200, tokens_reply());
        let (s, b) = zendesk_exchange(&app, "squrell://oauth", "zk-fixed").await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(b["state"], "connected", "{b}");
        let row = state
            .store
            .connector_connection("ada", "zendesk")
            .unwrap()
            .unwrap();
        assert_eq!(row.tenant_id, "", "no address, so none is stored");
        assert_eq!(
            row.token_endpoint,
            "https://auth.fixedrest.example/oauth/token"
        );
        assert_eq!(row.client_id.as_deref(), Some("zc1"));
        assert_eq!(p.requests().len(), 1);
        // It stays connected: the listing and a check judge it against no address too.
        assert_eq!(
            zendesk_entry(&app, "member-secret").await["state"],
            "connected"
        );
        let (s, b) = call(
            &app,
            "POST",
            "/v1/connectors/zendesk/connection/check",
            "member-secret",
            None,
        )
        .await;
        assert_eq!(
            s,
            StatusCode::BAD_GATEWAY,
            "the provider mock has no refresh answer: {b}"
        );
        assert_eq!(
            zendesk_entry(&app, "member-secret").await["state"],
            "connected"
        );
    }

    #[tokio::test]
    async fn a_disconnect_after_the_admin_replaced_the_client_revokes_nothing_and_says_so() {
        let (app, state, p) = fixed_endpoint_node("fixedrevoke.example", true).await;
        p.reply(200, tokens_reply());
        let (s, b) = zendesk_exchange(&app, "squrell://oauth", "zk-rv").await;
        assert_eq!(s, StatusCode::OK, "{b}");
        // The admin replaces the client at the same endpoint.
        let (s, b) = put_zendesk(
            &app,
            "orgadmin-secret",
            serde_json::json!({ "enabled": true, "client": { "clientId": "zc2" } }),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        let before = p.requests().len();
        let (s, b) = call(
            &app,
            "DELETE",
            "/v1/connectors/zendesk/connection",
            "member-secret",
            None,
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(b["providerGrantMayRemain"], true, "{b}");
        assert_eq!(
            p.requests().len(),
            before,
            "no credentials were sent to the provider"
        );
        assert!(state
            .store
            .connector_connection("ada", "zendesk")
            .unwrap()
            .is_none());
        assert_no_value("the disconnect answer", &b.to_string());
    }

    #[tokio::test]
    async fn a_disconnect_of_a_grant_the_nodes_client_issued_revokes_it_and_claims_nothing_remains()
    {
        let (app, _state, p) = fixed_endpoint_node("fixedrevoke2.example", true).await;
        p.reply(200, tokens_reply());
        let (s, b) = zendesk_exchange(&app, "squrell://oauth", "zk-rv2").await;
        assert_eq!(s, StatusCode::OK, "{b}");
        p.reply(200, serde_json::Value::Null);
        let (s, b) = call(
            &app,
            "DELETE",
            "/v1/connectors/zendesk/connection",
            "member-secret",
            None,
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert!(b.get("providerGrantMayRemain").is_none(), "{b}");
        let revoke = p.requests().into_iter().last().unwrap();
        assert_eq!(revoke.path, "/oauth/revoke");
        assert_eq!(revoke.form["client_id"], "zc1");
        assert_eq!(revoke.form["token"], NEW_REFRESH);
    }

    #[tokio::test]
    async fn the_committed_rest_exchange_resumes_parked_runs_after_the_lock_is_released_and_only_then(
    ) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let (_app, state, p) = zendesk_node("xrest-resume").await;
        let org = state.store.org_connector("zendesk").unwrap().unwrap();
        let client = org.client.clone().unwrap();
        let p = &p;
        let resumed = Arc::new(AtomicUsize::new(0));
        let lock_free = Arc::new(AtomicBool::new(false));
        let run = |key: &'static str, ok: bool| {
            let (store, vault) = (state.store.clone(), state.vault.clone());
            let (org, client) = (org.clone(), client.clone());
            let req: connector_client::CodeExchangeRequest =
                serde_json::from_value(exchange_body("squrell://oauth")).unwrap();
            let (resumed, lock_free) = (resumed.clone(), lock_free.clone());
            async move {
                if ok {
                    p.reply(200, tokens_reply());
                } else {
                    p.reply(400, serde_json::json!({ "error": "invalid_grant" }));
                }
                let gate = crate::connectors::lock_connection("ada", "zendesk").await;
                tokio::task::spawn_blocking(move || {
                    let c = connector_exchange::CodeConnect {
                        owner: "ada",
                        creator: "ada",
                        connector_id: "zendesk",
                        org: &org,
                        client: &client,
                        request: &req,
                        idempotency_key: key,
                    };
                    connector_exchange::exchange_and_connect_rest(&store, &c, &vault, gate, || {
                        // The resumed intents take this lock: it must be free by now.
                        let free = tokio::runtime::Handle::current().block_on(async {
                            tokio::time::timeout(
                                std::time::Duration::from_millis(500),
                                crate::connectors::lock_connection("ada", "zendesk"),
                            )
                            .await
                            .is_ok()
                        });
                        lock_free.store(free, Ordering::SeqCst);
                        resumed.fetch_add(1, Ordering::SeqCst);
                    })
                    .is_ok()
                })
                .await
                .unwrap()
            }
        };
        // A refused code resumes nothing; a committed connection resumes once, lock released.
        assert!(!run("zk-r1", false).await);
        assert_eq!(resumed.load(Ordering::SeqCst), 0);
        assert!(run("zk-r2", true).await);
        assert_eq!(resumed.load(Ordering::SeqCst), 1);
        assert!(
            lock_free.load(Ordering::SeqCst),
            "the lock was held while resuming"
        );
    }

    #[tokio::test]
    async fn a_connect_the_connector_refuses_leaves_the_stale_row_and_its_grant_alone() {
        // `filebox` moved from the old server to `xchg-purge`; the member's row is on the old one.
        let n = xchg_node("xchg-purge");
        let (s, b) = put_client(&n, "c-1", Some(SECRET)).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        previous_server_row(&n);
        // An OAuth block on a connector whose client is the admin's is refused (the node's exchange
        // is the way in): the answer is a 400, and the old row and its tokens are exactly where they were.
        let (s, b) = call(
            &n.app,
            "PUT",
            "/v1/mcp/connections",
            "member-secret",
            Some(serde_json::json!({
                "connectionId": "filebox",
                "url": "https://mcp.xchg-purge.example/mcp",
                "authMode": "oauth",
                "oauth": {
                    "resource": "https://mcp.xchg-purge.example/mcp",
                    "authorizationServer": "https://auth.xchg-purge.example",
                    "tokenEndpoint": "https://auth.xchg-purge.example/token",
                    "clientId": "c-1",
                    "registrationMethod": "manual"
                }
            })),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
        let row = row_of(&n, "ada");
        assert_eq!(row.url, "https://mcp.old-home.example/mcp");
        assert_eq!(token_of(&n, "ada", "filebox/oauth/refresh-token"), "OLD-R");
        assert_eq!(token_of(&n, "ada", "filebox/oauth/access-token"), "OLD-A");
        assert!(
            n.token.requests().is_empty(),
            "nothing was revoked at the provider"
        );
    }

    #[tokio::test]
    async fn a_settings_write_does_not_put_back_oauth_state_a_sign_in_committed_while_it_waited() {
        use crate::mcp_registry::{
            McpAuthMode, McpOauthMetadata, McpOauthRegistrationMethod, McpOauthStatus,
        };
        let (app, state) = app_world_with_state();
        let oauth = |expires_at: i64| McpOauthMetadata {
            resource: "https://mcp.settings-race.example/mcp".into(),
            authorization_server: "https://auth.settings-race.example".into(),
            token_endpoint: "https://auth.settings-race.example/token".into(),
            revocation_endpoint: None,
            client_id: "c-1".into(),
            registration_method: McpOauthRegistrationMethod::Manual,
            scopes: vec![],
            expires_at: Some(expires_at),
            status: McpOauthStatus::Connected,
            last_refresh_at: None,
            last_refresh_error: None,
        };
        let upsert = |meta: &McpOauthMetadata| {
            state
                .store
                .upsert_mcp_connection(
                    crate::secrets::SecretScope::User,
                    "ada",
                    "settings-race",
                    "https://mcp.settings-race.example/mcp",
                    "",
                    &[],
                    &[],
                    McpAuthMode::Oauth,
                    Some(meta),
                )
                .unwrap()
        };
        let conn = upsert(&oauth(100));
        // A sign-in holds the credential gate; the settings write (no OAuth block) waits for it.
        let gate = crate::host_tools::lock_mcp_credentials(&state.store, &conn)
            .await
            .unwrap();
        let read = crate::connectors::test_signal::arm("mcp-connect-read:settings-race");
        let waiting = {
            let app = app.clone();
            tokio::spawn(async move {
                call(
                    &app,
                    "PUT",
                    "/v1/mcp/connections",
                    "member-secret",
                    Some(serde_json::json!({
                        "connectionId": "settings-race",
                        "url": "https://mcp.settings-race.example/mcp",
                        "timeoutMs": 90000
                    })),
                )
                .await
            })
        };
        // The settings write has read the row with the old expiry and waits for the gate.
        tokio::task::spawn_blocking(move || read.recv_timeout(std::time::Duration::from_secs(10)))
            .await
            .unwrap()
            .expect("the settings write read the row before the sign-in committed");
        upsert(&oauth(999_999));
        drop(gate);
        let (s, b) = waiting.await.unwrap();
        assert_eq!(s, StatusCode::OK, "{b}");
        let stored = state
            .store
            .get_mcp_connection(crate::secrets::SecretScope::User, "ada", "settings-race")
            .unwrap();
        assert_eq!(
            stored.oauth.unwrap().expires_at,
            Some(999_999),
            "the write carried the sign-in's expiry forward, not the one it read before waiting"
        );
        assert_eq!(stored.timeout_ms, 90_000);
    }

    #[tokio::test]
    async fn a_settings_write_keeps_a_reconnect_mark_made_after_it_read_the_row() {
        use crate::mcp_registry::{
            McpAuthMode, McpOauthMetadata, McpOauthRegistrationMethod, McpOauthStatus,
        };
        let (app, state) = app_world_with_state();
        let oauth = |status: McpOauthStatus| McpOauthMetadata {
            resource: "https://mcp.settings-mark.example/mcp".into(),
            authorization_server: "https://auth.settings-mark.example".into(),
            token_endpoint: "https://auth.settings-mark.example/token".into(),
            revocation_endpoint: None,
            client_id: "c-1".into(),
            registration_method: McpOauthRegistrationMethod::Manual,
            scopes: vec![],
            expires_at: Some(100),
            status,
            last_refresh_at: None,
            last_refresh_error: None,
        };
        let upsert = |meta: &McpOauthMetadata| {
            state
                .store
                .upsert_mcp_connection(
                    crate::secrets::SecretScope::User,
                    "ada",
                    "settings-mark",
                    "https://mcp.settings-mark.example/mcp",
                    "",
                    &[],
                    &[],
                    McpAuthMode::Oauth,
                    Some(meta),
                )
                .unwrap()
        };
        upsert(&oauth(McpOauthStatus::Connected));
        // The settings write stops just before its write, after its last read of the row.
        let (arrived, release) =
            crate::connectors::test_signal::arm_pause("mcp-connect-write:settings-mark");
        let waiting = {
            let app = app.clone();
            tokio::spawn(async move {
                call(
                    &app,
                    "PUT",
                    "/v1/mcp/connections",
                    "member-secret",
                    Some(serde_json::json!({
                        "connectionId": "settings-mark",
                        "url": "https://mcp.settings-mark.example/mcp",
                        "timeoutMs": 90000
                    })),
                )
                .await
            })
        };
        tokio::task::spawn_blocking(move || {
            arrived.recv_timeout(std::time::Duration::from_secs(10))
        })
        .await
        .unwrap()
        .expect("the settings write reached its write");
        // An admin's client save marks the member's row for reconnect (without the member's gate).
        upsert(&oauth(McpOauthStatus::NeedsReauth));
        release.send(()).unwrap();
        let (s, b) = waiting.await.unwrap();
        assert_eq!(s, StatusCode::OK, "{b}");
        let stored = state
            .store
            .get_mcp_connection(crate::secrets::SecretScope::User, "ada", "settings-mark")
            .unwrap();
        assert_eq!(
            stored.oauth.unwrap().status,
            McpOauthStatus::NeedsReauth,
            "the settings write put back the connected state it read"
        );
        assert_eq!(stored.timeout_ms, 90_000);
    }

    /// A hand-added static MCP server on loopback, answering `host`; `tools_ok` false makes its
    /// `tools/list` fail.
    fn static_mcp_server(
        host: &str,
        tools_ok: Arc<AtomicBool>,
    ) -> Arc<std::sync::Mutex<crate::mcp_client::mock::Seen>> {
        let (url, seen) = crate::mcp_client::mock::spawn(move |method, id, params| {
            if method == "tools/list" && !tools_ok.load(std::sync::atomic::Ordering::SeqCst) {
                return Some((
                    "application/json".into(),
                    serde_json::json!({ "jsonrpc": "2.0", "id": id,
                        "error": { "code": -32000, "message": "tools are down" } })
                    .to_string(),
                ));
            }
            crate::mcp_client::mock::standard(method, id, params)
        });
        crate::mcp_oauth_refresh::test_seam::serve(host, url::Url::parse(&url).unwrap());
        seen
    }

    fn static_connect_body(id: &str, host: &str, authorization: &str) -> serde_json::Value {
        serde_json::json!({
            "connectionId": id,
            "url": format!("https://{host}/mcp"),
            "secretValue": authorization,
        })
    }

    fn stored_header(state: &AppState, id: &str) -> String {
        secrecy::ExposeSecret::expose_secret(
            &state
                .vault
                .decrypt_secret_at(
                    crate::secrets::SecretScope::User,
                    "ada",
                    &format!("{id}/auth-header"),
                )
                .unwrap(),
        )
        .clone()
    }

    #[tokio::test]
    async fn two_first_time_static_connects_leave_the_accepted_ones_header_in_force() {
        let (app, state) = app_world_with_state();
        let host = "mcp.static-race.example";
        static_mcp_server(host, Arc::new(AtomicBool::new(true)));
        let connect = |app: axum::Router, key: &'static str, authorization: &'static str| {
            tokio::spawn(async move {
                call_with_key(
                    &app,
                    "PUT",
                    "/v1/mcp/connections",
                    "member-secret",
                    Some(static_connect_body("static-race", host, authorization)),
                    key,
                )
                .await
            })
        };
        let wait = |arrived: std::sync::mpsc::Receiver<()>| {
            tokio::task::spawn_blocking(move || {
                arrived.recv_timeout(std::time::Duration::from_secs(10))
            })
        };
        // A and B are both first-time connects: each has read "no row" and listed the tools, and
        // each stops just before its row write.
        let (arrived_a, release_a) =
            crate::connectors::test_signal::arm_pause("mcp-connect-write:static-race");
        let a = connect(app.clone(), "race-a", "Bearer token-A");
        wait(arrived_a)
            .await
            .unwrap()
            .expect("A reached its row write");
        let (arrived_b, release_b) =
            crate::connectors::test_signal::arm_pause("mcp-connect-write:static-race");
        let b = connect(app.clone(), "race-b", "Bearer token-B");
        wait(arrived_b)
            .await
            .unwrap()
            .expect("B reached its row write");
        // A's row commits; B is refused.
        release_a.send(()).unwrap();
        let (s, body) = a.await.unwrap();
        assert_eq!(s, StatusCode::OK, "{body}");
        release_b.send(()).unwrap();
        let (s, body) = b.await.unwrap();
        assert_eq!(s, StatusCode::CONFLICT, "{body}");
        // The request the member was told succeeded is the one in force.
        assert_eq!(
            stored_header(&state, "static-race"),
            "Bearer token-A",
            "the accepted row's header is not the refused request's value"
        );
    }

    #[tokio::test]
    async fn a_static_connect_whose_tool_listing_fails_keeps_the_old_header() {
        let (app, state) = app_world_with_state();
        let host = "mcp.static-keep.example";
        let tools_ok = Arc::new(AtomicBool::new(true));
        let seen = static_mcp_server(host, tools_ok.clone());
        let (s, body) = call_with_key(
            &app,
            "PUT",
            "/v1/mcp/connections",
            "member-secret",
            Some(static_connect_body("static-keep", host, "Bearer old")),
            "keep-1",
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{body}");
        assert_eq!(stored_header(&state, "static-keep"), "Bearer old");
        // The server now refuses the listing; the member replaces the header.
        tools_ok.store(false, std::sync::atomic::Ordering::SeqCst);
        let (s, body) = call_with_key(
            &app,
            "PUT",
            "/v1/mcp/connections",
            "member-secret",
            Some(static_connect_body("static-keep", host, "Bearer new")),
            "keep-2",
        )
        .await;
        assert_ne!(s, StatusCode::OK, "{body}");
        assert!(
            seen.lock()
                .unwrap()
                .auth
                .iter()
                .any(|a| a.as_deref() == Some("Bearer new")),
            "the listing was tried with the new header"
        );
        assert_eq!(
            stored_header(&state, "static-keep"),
            "Bearer old",
            "a refused connect replaced a working header"
        );
        // A listing that works takes the new header.
        tools_ok.store(true, std::sync::atomic::Ordering::SeqCst);
        let (s, body) = call_with_key(
            &app,
            "PUT",
            "/v1/mcp/connections",
            "member-secret",
            Some(static_connect_body("static-keep", host, "Bearer new")),
            "keep-3",
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{body}");
        assert_eq!(stored_header(&state, "static-keep"), "Bearer new");
    }

    /// A marketplace connector that signs in with an API key (REST, one bearer binding).
    fn install_notesdesk(store: &SqliteWorkflowStore) {
        install_notesdesk_with(store, |_| {});
    }

    /// [`install_notesdesk`] with the file edited by `edit` first.
    fn install_notesdesk_with(
        store: &SqliteWorkflowStore,
        edit: impl FnOnce(&mut serde_json::Value),
    ) {
        let mut json = serde_json::json!({
            "urn": "urn:vam:connector:connectors.community.example/community/notesdesk",
            "version": "1.0.0", "displayName": "Notesdesk", "publisher": "Community Tools Ltd",
            "description": "Notes.", "icon": "notesdesk",
            "provider": { "kind": "api_key",
                          "fields": [{ "name": "key", "label": "Notesdesk API key" }] },
            "connection": { "holder": "user", "multiAccount": false, "label": "none" },
            "bindings": [{ "name": "api", "placeholder": "vamsec_notesdesk_api",
                           "value": "api_key", "inject": "bearer",
                           "hosts": ["api.notesdesk.example"] }]
        });
        edit(&mut json);
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
             VALUES ('connector', ?1, 1, 0, 0, 'm-1', 'notesdesk-connector', 'notesdesk-v1', 'c', 'd', 1)",
            [urn],
        )
        .unwrap();
    }

    #[tokio::test]
    async fn an_api_key_connector_takes_no_client_and_no_consent_and_lists_its_field() {
        let (app, state) = app_world_with_state();
        install_notesdesk(&state.store);
        let node = |body: serde_json::Value| {
            let app = app.clone();
            async move {
                call(
                    &app,
                    "PUT",
                    "/v1/connectors/notesdesk/node",
                    "orgadmin-secret",
                    Some(body),
                )
                .await
            }
        };
        for body in [
            serde_json::json!({ "enabled": true, "client": { "clientId": "c-1" } }),
            serde_json::json!({ "enabled": true, "client": { "clientId": "c-1", "clientSecret": "s" } }),
            serde_json::json!({ "enabled": true, "consent": { "tenantId": "11111111-2222-3333-4444-555555555555" } }),
        ] {
            let (s, b) = node(body).await;
            assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
            assert!(b.to_string().contains("API key"), "{b}");
        }
        // Nothing was saved by the refused bodies.
        assert!(state.store.node_connector("notesdesk").unwrap().is_none());
        let (s, e) = node(serde_json::json!({ "enabled": true })).await;
        assert_eq!(s, StatusCode::OK, "{e}");
        assert_eq!(e["provider"]["kind"], "api_key");
        assert_eq!(e["provider"]["fields"][0]["label"], "Notesdesk API key");
        // A member reads the same entry: the sign-in kind and the label, and the connect state.
        let (s, l) = call(&app, "GET", "/v1/connectors", "member-secret", None).await;
        assert_eq!(s, StatusCode::OK);
        let entry = l["connectors"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["id"] == "notesdesk")
            .unwrap();
        assert_eq!(entry["state"], "connect", "{entry}");
        assert_eq!(entry["provider"]["kind"], "api_key");
        assert_eq!(entry["provider"]["fields"][0]["label"], "Notesdesk API key");
    }

    #[tokio::test]
    async fn a_check_whose_request_goes_away_keeps_the_members_lock_until_the_refresh_returns() {
        let (app, _state, p) = fixed_endpoint_node("fixedcheck.example", false).await;
        p.reply(200, tokens_reply());
        let (s, b) = zendesk_exchange(&app, "squrell://oauth", "zk-ck").await;
        assert_eq!(s, StatusCode::OK, "{b}");
        // The provider holds the refresh answer; the member's request is dropped meanwhile.
        let (arrived, release) = p.hold_next();
        p.reply(200, tokens_reply());
        let check = {
            let app = app.clone();
            tokio::spawn(async move {
                call(
                    &app,
                    "POST",
                    "/v1/connectors/zendesk/connection/check",
                    "member-secret",
                    None,
                )
                .await
            })
        };
        tokio::task::spawn_blocking(move || {
            arrived
                .recv_timeout(std::time::Duration::from_secs(10))
                .expect("the check sent a refresh to the provider")
        })
        .await
        .unwrap();
        check.abort();
        let _ = check.await;
        // The refresh is still running on its worker: a sign-in must not get the lock under it.
        let early = tokio::time::timeout(
            std::time::Duration::from_millis(300),
            crate::connectors::lock_connection("ada", "zendesk"),
        )
        .await;
        assert!(
            early.is_err(),
            "the lock was released while the refresh was in flight"
        );
        release.send(()).unwrap();
        let late = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            crate::connectors::lock_connection("ada", "zendesk"),
        )
        .await;
        assert!(late.is_ok(), "the lock is free once the refresh returned");
    }

    // ---- the API key connection (CN-7.6) ---------------------------------------------------------

    const KEY: &str = "ndk_live_0123456789abcdef";
    const KEY_ROW: &str = "connector/notesdesk/api-key";

    async fn enable_notesdesk(app: &axum::Router, body: serde_json::Value) {
        let (s, b) = call(
            app,
            "PUT",
            "/v1/connectors/notesdesk/node",
            "orgadmin-secret",
            Some(body),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
    }

    async fn put_key(
        app: &axum::Router,
        secret: &str,
        body: serde_json::Value,
    ) -> (StatusCode, serde_json::Value) {
        call(
            app,
            "PUT",
            "/v1/connectors/notesdesk/connection",
            secret,
            Some(body),
        )
        .await
    }

    fn key_meta(
        state: &AppState,
        owner: &str,
    ) -> Result<crate::secrets::HostSecretMetadata, OrchestratorError> {
        state
            .store
            .get_host_secret_metadata(SecretScope::User, owner, KEY_ROW)
    }

    fn stored_key(state: &AppState, owner: &str) -> String {
        use secrecy::ExposeSecret;
        state
            .vault
            .decrypt_secret_at(SecretScope::User, owner, KEY_ROW)
            .unwrap()
            .expose_secret()
            .clone()
    }

    async fn member_entry(app: &axum::Router) -> serde_json::Value {
        let (s, l) = call(app, "GET", "/v1/connectors", "member-secret", None).await;
        assert_eq!(s, StatusCode::OK);
        l["connectors"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["id"] == "notesdesk")
            .cloned()
            .unwrap()
    }

    #[tokio::test]
    async fn an_api_key_is_stored_pinned_to_the_filled_hosts_and_shown_nowhere() {
        let (app, state) = app_world_with_state();
        install_notesdesk(&state.store);
        enable_notesdesk(&app, serde_json::json!({ "enabled": true })).await;
        assert_eq!(member_entry(&app).await["state"], "connect");

        let (s, b) = put_key(&app, "member-secret", serde_json::json!({ "apiKey": KEY })).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(b["state"], "connected", "{b}");
        // The vault row: this member's, `connection` kind, pinned to the one filled host.
        let meta = key_meta(&state, "ada").unwrap();
        assert_eq!(meta.kind, crate::secrets::SecretKind::Connection);
        assert_eq!(meta.allowed_destinations, vec!["api.notesdesk.example"]);
        assert_eq!(stored_key(&state, "ada"), KEY);
        // The row: connected, no token endpoint, no expiry, the empty address.
        let row = state
            .store
            .connector_connection("ada", "notesdesk")
            .unwrap()
            .unwrap();
        assert_eq!(row.status, crate::connectors::ConnectionStatus::Connected);
        assert_eq!(row.token_endpoint, "");
        assert_eq!(row.tenant_id, "");
        assert_eq!(row.expires_at, None);
        // No answer holds the key, for the member or for the admin; and it is one vault row only.
        for secret in ["member-secret", "orgadmin-secret"] {
            let (_, l) = call(&app, "GET", "/v1/connectors", secret, None).await;
            assert!(!l.to_string().contains(KEY), "{secret}: {l}");
        }
        assert!(!b.to_string().contains(KEY));
        // Only the member's own tile reads connected.
        let (_, l) = call(&app, "GET", "/v1/connectors", "orgadmin-secret", None).await;
        let admin_entry = l["connectors"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["id"] == "notesdesk")
            .unwrap();
        assert_eq!(admin_entry["state"], "connect");
        assert!(key_meta(&state, "olga").is_err());
        // A replaced key is the stored one.
        let (s, b) = put_key(
            &app,
            "member-secret",
            serde_json::json!({ "apiKey": "ndk_live_ZZZZZZZZZZZZZZZZ" }),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(stored_key(&state, "ada"), "ndk_live_ZZZZZZZZZZZZZZZZ");
        assert!(!b.to_string().contains("ndk_live_"));
    }

    #[tokio::test]
    async fn every_api_key_refusal_is_a_fixed_text_that_never_quotes_the_key() {
        let (app, state) = app_world_with_state();
        install_notesdesk(&state.store);
        enable_notesdesk(&app, serde_json::json!({ "enabled": true })).await;
        let long = "k".repeat(4097);
        let bad_values: Vec<String> = vec![
            "short".into(),
            "1234567".into(),
            long,
            " ndk_live_0123456789".into(),
            "ndk_live_0123456789 ".into(),
            "ndk live 0123456789".into(),
            "ndk_live_\n0123456789".into(),
            "ndk_live_\t0123456789".into(),
            "ndk_live_\r\n0123456789".into(),
            "ndk_live_\u{0}0123456789".into(),
            "ndk_live_\u{7f}0123456789".into(),
            "ndk_live_\u{a0}0123456789".into(),
            "ndk_live_\u{2028}0123456789".into(),
        ];
        for v in &bad_values {
            let (s, b) = put_key(&app, "member-secret", serde_json::json!({ "apiKey": v })).await;
            assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
            assert!(
                b.to_string().contains(
                    "apiKey must be 8 to 4096 bytes, with no whitespace or control characters"
                ),
                "{b}"
            );
            let shown = b.to_string();
            assert!(
                !shown.contains("ndk_live_") && !shown.contains(&"k".repeat(20)),
                "{b}"
            );
        }
        // A body that is not exactly `{ apiKey }`: one fixed text, the value is never echoed.
        for raw in [
            serde_json::json!({}),
            serde_json::json!({ "apiKey": KEY, "extra": 1 }),
            serde_json::json!({ "apiKey": KEY, "accessToken": "x" }),
            serde_json::json!({ "apiKey": 12345678 }),
            serde_json::json!({ "api_key": KEY }),
            serde_json::json!([KEY]),
            serde_json::json!({ "accessToken": KEY, "refreshToken": KEY, "idToken": KEY }),
        ] {
            let (s, b) = put_key(&app, "member-secret", raw).await;
            assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
            assert!(
                b.to_string().contains("connection body must be { apiKey }"),
                "{b}"
            );
            assert!(!b.to_string().contains(KEY), "{b}");
        }
        let req = Request::builder()
            .method("PUT")
            .uri("/v1/connectors/notesdesk/connection")
            .header("authorization", "Bearer member-secret")
            .header("idempotency-key", "k-1")
            .body(Body::from(format!("{{\"apiKey\": \"{KEY}\" ")))
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        let text = String::from_utf8(res.into_body().collect().await.unwrap().to_bytes().to_vec())
            .unwrap();
        assert!(!text.contains(KEY), "{text}");
        // Nothing was written by any refusal.
        assert!(key_meta(&state, "ada").is_err());
        assert!(state
            .store
            .connector_connection("ada", "notesdesk")
            .unwrap()
            .is_none());
        // The limits themselves are accepted, and the key is kept exactly as typed.
        for ok in [
            "12345678".to_string(),
            "k".repeat(4096),
            "ünïcode-key-çhars".to_string(),
        ] {
            let (s, b) = put_key(&app, "member-secret", serde_json::json!({ "apiKey": ok })).await;
            assert_eq!(s, StatusCode::OK, "{b}");
            assert_eq!(stored_key(&state, "ada"), ok);
        }
    }

    #[tokio::test]
    async fn a_key_is_refused_before_enable_and_before_the_company_address() {
        let (app, state) = app_world_with_state();
        install_notesdesk(&state.store);
        // Not enabled: 403 with the Phase 6 words; a disabled row too.
        let (s, b) = put_key(&app, "member-secret", serde_json::json!({ "apiKey": KEY })).await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{b}");
        assert!(b.to_string().contains("ask your admin"), "{b}");
        enable_notesdesk(&app, serde_json::json!({ "enabled": true })).await;
        enable_notesdesk(&app, serde_json::json!({ "enabled": false })).await;
        let (s, b) = put_key(&app, "member-secret", serde_json::json!({ "apiKey": KEY })).await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{b}");
        assert!(!b.to_string().contains(KEY));
        assert!(key_meta(&state, "ada").is_err());

        // A connector with a company address: enabled with none stored is a 409, nothing written.
        let (app, state) = app_world_with_state();
        install_notesdesk_with(&state.store, |j| {
            j["tenant"] = serde_json::json!({
                "name": "subdomain", "suffix": "notesdesk.example", "label": "Notesdesk address"
            });
            j["bindings"][0]["hosts"] = serde_json::json!(["{subdomain}.notesdesk.example"]);
        });
        state
            .store
            .put_node_connector(&crate::connectors::NodeConnector {
                connector_id: "notesdesk".into(),
                enabled: true,
                consent_tenant_id: None,
                consent_recorded_at: None,
                consent_recorded_by: None,
                tenant_subdomain: None,
                updated_at: 1,
                updated_by: "olga".into(),
            })
            .unwrap();
        let (s, b) = put_key(&app, "member-secret", serde_json::json!({ "apiKey": KEY })).await;
        assert_eq!(s, StatusCode::CONFLICT, "{b}");
        assert!(b.to_string().contains("company address"), "{b}");
        assert!(key_meta(&state, "ada").is_err());
        // With the address the key is pinned to the filled host and the row carries the address.
        enable_notesdesk(
            &app,
            serde_json::json!({ "enabled": true, "tenant": { "subdomain": "acme" } }),
        )
        .await;
        let (s, b) = put_key(&app, "member-secret", serde_json::json!({ "apiKey": KEY })).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(b["state"], "connected");
        assert_eq!(
            key_meta(&state, "ada").unwrap().allowed_destinations,
            vec!["acme.notesdesk.example"]
        );
        assert_eq!(
            state
                .store
                .connector_connection("ada", "notesdesk")
                .unwrap()
                .unwrap()
                .tenant_id,
            "acme"
        );
        // A move of the address reads needsReconnect: the key stays pinned to the old host.
        enable_notesdesk(
            &app,
            serde_json::json!({ "enabled": true, "tenant": { "subdomain": "globex" } }),
        )
        .await;
        assert_eq!(member_entry(&app).await["state"], "needsReconnect");
        assert_eq!(
            key_meta(&state, "ada").unwrap().allowed_destinations,
            vec!["acme.notesdesk.example"]
        );
    }

    #[tokio::test]
    async fn an_api_key_is_the_members_own_and_needs_the_connection_scope() {
        let (app, state) = app_world_with_state();
        install_notesdesk(&state.store);
        enable_notesdesk(&app, serde_json::json!({ "enabled": true })).await;
        state
            .store
            .create_api_key_owned(
                "readonly-key",
                "ada",
                "readonly-secret",
                "User::\"ada\"",
                &["node:read".to_string()],
                "ada",
            )
            .unwrap();
        let (s, b) = put_key(
            &app,
            "readonly-secret",
            serde_json::json!({ "apiKey": KEY }),
        )
        .await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{b}");
        assert!(key_meta(&state, "ada").is_err());
        // No key at all: not authorized.
        let (s, _) = put_key(&app, "nobody", serde_json::json!({ "apiKey": KEY })).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
        // The member writes their own row; the admin's is untouched, and the admin's key is theirs.
        let (s, _) = put_key(&app, "member-secret", serde_json::json!({ "apiKey": KEY })).await;
        assert_eq!(s, StatusCode::OK);
        assert!(state
            .store
            .connector_connection("olga", "notesdesk")
            .unwrap()
            .is_none());
        let (s, _) = put_key(
            &app,
            "orgadmin-secret",
            serde_json::json!({ "apiKey": "ndk_admin_0123456789" }),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(stored_key(&state, "ada"), KEY);
        assert_eq!(stored_key(&state, "olga"), "ndk_admin_0123456789");
        // The Entra body is not taken for an API key connector, and writes nothing more.
        let (s, b) = put_key(
            &app,
            "member-secret",
            serde_json::json!({ "accessToken": "a1234567", "refreshToken": "r1234567", "idToken": "x.y.z" }),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
        assert_eq!(stored_key(&state, "ada"), KEY);
    }

    #[tokio::test]
    async fn disconnect_and_check_for_an_api_key_ask_the_provider_nothing() {
        let (app, state) = app_world_with_state();
        install_notesdesk(&state.store);
        enable_notesdesk(&app, serde_json::json!({ "enabled": true })).await;
        let (s, _) = put_key(&app, "member-secret", serde_json::json!({ "apiKey": KEY })).await;
        assert_eq!(s, StatusCode::OK);
        // The check answers the stored state (the node has no route to any provider here: a
        // provider call would have to fail the test by hanging or erroring).
        let (s, b) = call(
            &app,
            "POST",
            "/v1/connectors/notesdesk/connection/check",
            "member-secret",
            None,
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(b["state"], "connected");
        assert!(!b.to_string().contains(KEY));
        // A key pinned to other hosts reads needsReconnect on the check as on the tile.
        state
            .store
            .connection
            .lock()
            .unwrap()
            .execute(
                "UPDATE host_secrets SET allowed_destinations_json = '[\"evil.example\"]' WHERE binding_id = ?1",
                [KEY_ROW],
            )
            .unwrap();
        let (_, b) = call(
            &app,
            "POST",
            "/v1/connectors/notesdesk/connection/check",
            "member-secret",
            None,
        )
        .await;
        assert_eq!(b["state"], "needsReconnect", "{b}");
        // A code exchange is refused for an API key connector, before anything is read.
        let (s, b) = call(
            &app,
            "POST",
            "/v1/connectors/notesdesk/connection/oauth/code",
            "member-secret",
            Some(serde_json::json!({ "code": "c", "codeVerifier": "v", "redirectUri": "squrell://oauth" })),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
        assert!(b.to_string().contains("API key"), "{b}");
        // Disconnect removes the row and the key, whether or not the node still enables it.
        enable_notesdesk(&app, serde_json::json!({ "enabled": false })).await;
        let (s, b) = call(
            &app,
            "DELETE",
            "/v1/connectors/notesdesk/connection",
            "member-secret",
            None,
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert!(b.get("providerGrantMayRemain").is_none(), "{b}");
        assert!(key_meta(&state, "ada").is_err());
        assert!(state
            .store
            .connector_connection("ada", "notesdesk")
            .unwrap()
            .is_none());
        let (s, _) = call(
            &app,
            "DELETE",
            "/v1/connectors/notesdesk/connection",
            "member-secret",
            None,
        )
        .await;
        assert_eq!(s, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_host_change_by_an_update_keeps_the_key_but_reads_needs_reconnect_until_it_is_entered_again(
    ) {
        let (app, state) = app_world_with_state();
        install_notesdesk(&state.store);
        enable_notesdesk(&app, serde_json::json!({ "enabled": true })).await;
        let (s, _) = put_key(&app, "member-secret", serde_json::json!({ "apiKey": KEY })).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(member_entry(&app).await["state"], "connected");
        // The listing is updated: the connector now also names the EU host.
        let urn = "urn:vam:connector:connectors.community.example/community/notesdesk";
        let updated = serde_json::json!({
            "urn": urn,
            "version": "1.1.0", "displayName": "Notesdesk", "publisher": "Community Tools Ltd",
            "description": "Notes.", "icon": "notesdesk",
            "provider": { "kind": "api_key",
                          "fields": [{ "name": "key", "label": "Notesdesk API key" }] },
            "connection": { "holder": "user", "multiAccount": false, "label": "none" },
            "bindings": [{ "name": "api", "placeholder": "vamsec_notesdesk_api",
                           "value": "api_key", "inject": "bearer",
                           "hosts": ["api.notesdesk.example", "eu.api.notesdesk.example"] }]
        });
        state
            .store
            .connection
            .lock()
            .unwrap()
            .execute(
                "UPDATE connector_releases SET spec_bytes = ?1 WHERE urn = ?2",
                rusqlite::params![updated.to_string().as_bytes(), urn],
            )
            .unwrap();
        // The key is kept where it was, pinned to the old host, and reads needsReconnect.
        let entry = member_entry(&app).await;
        assert_eq!(entry["state"], "needsReconnect", "{entry}");
        assert_eq!(
            key_meta(&state, "ada").unwrap().allowed_destinations,
            vec!["api.notesdesk.example"]
        );
        assert_eq!(stored_key(&state, "ada"), KEY);
        // Entering it again pins it to both hosts and the tile reads connected.
        let (s, b) = put_key(&app, "member-secret", serde_json::json!({ "apiKey": KEY })).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(b["state"], "connected");
        assert_eq!(
            key_meta(&state, "ada").unwrap().allowed_destinations,
            vec!["api.notesdesk.example", "eu.api.notesdesk.example"]
        );
    }

    #[tokio::test]
    async fn a_listing_update_that_lands_during_the_save_stores_nothing() {
        // The transaction checks the hosts again: a connector whose hosts moved since the plan was
        // made refuses the write.
        let (_app, state) = app_world_with_state();
        install_notesdesk(&state.store);
        let node = crate::connectors::NodeConnector {
            connector_id: "notesdesk".into(),
            enabled: true,
            consent_tenant_id: None,
            consent_recorded_at: None,
            consent_recorded_by: None,
            tenant_subdomain: None,
            updated_at: 1,
            updated_by: "olga".into(),
        };
        state.store.put_node_connector(&node).unwrap();
        let hosts = vec!["api.notesdesk.example".to_string()];
        let moved = |_: &rusqlite::Connection| -> Result<(), OrchestratorError> {
            Err(OrchestratorError::Conflict("moved".into()))
        };
        let commit = crate::connectors::ApiKeyCommit {
            owner: "ada",
            connector_id: "notesdesk",
            address: "",
            hosts: &hosts,
            api_key: KEY,
            now: 1,
        };
        assert!(state
            .store
            .commit_api_key_connection(&state.vault, &commit, &moved)
            .is_err());
        assert!(key_meta(&state, "ada").is_err());
        assert!(state
            .store
            .connector_connection("ada", "notesdesk")
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn the_entry_a_write_returns_is_filled_from_the_node_row_it_shows() {
        use crate::keys::{ApiKeyRecord, AuthSource, AuthenticatedKey};
        let (_app, state) = app_world_with_state();
        install_notesdesk_with(&state.store, |j| {
            j["tenant"] = serde_json::json!({
                "name": "subdomain", "suffix": "notesdesk.example", "label": "Notesdesk address"
            });
            j["bindings"][0]["hosts"] = serde_json::json!(["{subdomain}.notesdesk.example"]);
        });
        let node = |sub: &str| crate::connectors::NodeConnector {
            connector_id: "notesdesk".into(),
            enabled: true,
            consent_tenant_id: None,
            consent_recorded_at: None,
            consent_recorded_by: None,
            tenant_subdomain: Some(sub.into()),
            updated_at: 1,
            updated_by: "olga".into(),
        };
        // The entry the route resolved when the node was at `old`...
        state.store.put_node_connector(&node("old")).unwrap();
        let stale = state.store.connector_entry("notesdesk").unwrap().unwrap();
        assert_eq!(
            stale.def.bindings.as_ref().unwrap()[0].hosts,
            vec!["old.notesdesk.example"]
        );
        // ...and an admin's save that moved the address before the response is built.
        state.store.put_node_connector(&node("acme")).unwrap();
        let auth = crate::api::auth::AuthContext {
            key: AuthenticatedKey {
                record: ApiKeyRecord {
                    key_id: "k".into(),
                    name: "k".into(),
                    sha256_secret: "h".into(),
                    principal: "User::\"ada\"".into(),
                    scopes: default_scopes_for_role(UserRole::Member),
                    created_at: 1,
                    owner_user_id: "ada".into(),
                    ..Default::default()
                },
                source: AuthSource::ApiKey,
                role: UserRole::Member,
            },
        };
        let entry = super::entry_for(&state, &auth, &stale).unwrap();
        assert_eq!(
            entry["bindings"][0]["hosts"],
            serde_json::json!(["acme.notesdesk.example"]),
            "{entry}"
        );
        assert_eq!(
            entry["node"]["tenant"]["hosts"],
            serde_json::json!(["acme.notesdesk.example"]),
            "{entry}"
        );
    }

    // ---- an MCP connector with an API key (CN-7.7) ---------------------------------------------

    const MCP_ID: &str = "keyed-bugwatch";
    const MCP_HEADER_ROW: &str = "keyed-bugwatch/auth-header";
    const MCP_KEY: &str = "bwk_live_0123456789abcdef";

    /// The marketplace connector `keyed-bugwatch`: an API key, a remote MCP server at `host`, one
    /// vetted tool (`list_issues`; the loopback server also offers `create_issue`).
    fn install_bugwatch(store: &SqliteWorkflowStore, host: &str) {
        install_notesdesk_with(store, |j| {
            j["urn"] = serde_json::json!(
                "urn:vam:connector:connectors.community.example/community/keyed-bugwatch"
            );
            j["displayName"] = serde_json::json!("Bugwatch");
            j["provider"]["fields"][0]["label"] = serde_json::json!("Bugwatch API key");
            j.as_object_mut().unwrap().remove("bindings");
            j["mcp"] = serde_json::json!({
                "url": format!("https://{host}/mcp"),
                "tools": [{ "remote": "list_issues", "sideEffect": "READ_ONLY" }]
            });
        });
    }

    async fn mcp_world(
        host: &str,
        tools_ok: Arc<AtomicBool>,
    ) -> (
        axum::Router,
        AppState,
        Arc<std::sync::Mutex<crate::mcp_client::mock::Seen>>,
    ) {
        let (app, state) = app_world_with_state();
        let seen = static_mcp_server(host, tools_ok);
        install_bugwatch(&state.store, host);
        let (s, b) = call(
            &app,
            "PUT",
            "/v1/connectors/keyed-bugwatch/node",
            "orgadmin-secret",
            Some(serde_json::json!({ "enabled": true })),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        (app, state, seen)
    }

    async fn put_mcp_key(
        app: &axum::Router,
        key: &str,
        body: serde_json::Value,
        idempotency: &str,
    ) -> (StatusCode, serde_json::Value) {
        let answer = call_with_key(
            app,
            "PUT",
            "/v1/connectors/keyed-bugwatch/connection",
            "member-secret",
            Some(body),
            idempotency,
        )
        .await;
        // No answer of the route, accepted or refused, holds the key it was sent.
        assert!(
            !answer.1.to_string().contains(key),
            "the answer holds the key"
        );
        answer
    }

    fn mcp_header(state: &AppState) -> String {
        use secrecy::ExposeSecret;
        state
            .vault
            .decrypt_secret_at(SecretScope::User, "ada", MCP_HEADER_ROW)
            .unwrap()
            .expose_secret()
            .clone()
    }

    fn mcp_row(state: &AppState) -> Result<crate::mcp_registry::McpConnection, OrchestratorError> {
        state
            .store
            .get_mcp_connection(SecretScope::User, "ada", MCP_ID)
    }

    fn has_mcp_header(state: &AppState) -> bool {
        state
            .vault
            .has_binding_at(SecretScope::User, "ada", MCP_HEADER_ROW)
            .unwrap()
    }

    async fn bugwatch_state(app: &axum::Router) -> serde_json::Value {
        let (_, l) = call(app, "GET", "/v1/connectors", "member-secret", None).await;
        entry_of(&l, MCP_ID)["state"].clone()
    }

    #[tokio::test]
    async fn the_generic_mcp_routes_refuse_an_api_key_connector_id() {
        let host = "mcp.bugwatch-generic.example";
        let (app, state, seen) = mcp_world(host, Arc::new(AtomicBool::new(true))).await;
        install_notesdesk(&state.store);
        let refused = |what: &str, s: StatusCode, b: &serde_json::Value| {
            assert_eq!(s, StatusCode::BAD_REQUEST, "{what}: {b}");
            let text = b.to_string();
            assert!(
                text.contains("signs in with an API key")
                    && text.contains("PUT /v1/connectors/")
                    && text.contains("apiKey"),
                "{what}: {b}"
            );
        };
        for (id, url) in [
            (MCP_ID, format!("https://{host}/mcp")),
            ("notesdesk", "https://mcp.elsewhere.example/mcp".to_string()),
        ] {
            // The connect, whatever the URL, header, mode or scope the app sends.
            for body in [
                serde_json::json!({ "connectionId": id, "url": url, "secretValue": "Bearer sneaky-value" }),
                serde_json::json!({ "connectionId": id, "url": url,
                    "headers": [{ "name": "X-Api-Key", "value": "sneaky-value" }] }),
                serde_json::json!({ "connectionId": id, "url": url, "authMode": "oauth" }),
            ] {
                let (s, b) = call(
                    &app,
                    "PUT",
                    "/v1/mcp/connections",
                    "member-secret",
                    Some(body),
                )
                .await;
                refused(&format!("connect {id}"), s, &b);
                assert!(!b.to_string().contains("sneaky-value"), "{b}");
            }
            // The OAuth token upsert, for a connection that does not exist.
            let (s, b) = call(
                &app,
                "PUT",
                &format!("/v1/mcp/connections/{id}/oauth/tokens"),
                "member-secret",
                Some(serde_json::json!({ "accessToken": "a1234567", "refreshToken": "r1234567" })),
            )
            .await;
            refused(&format!("tokens {id}"), s, &b);
        }
        // Nothing was written, and the server was never asked.
        assert!(mcp_row(&state).is_err());
        assert!(!has_mcp_header(&state));
        assert!(seen.lock().unwrap().methods.is_empty());
        // The refusal does not reach into other ids: a hand-added server still connects.
        let (s, b) = call(
            &app,
            "PUT",
            "/v1/mcp/connections",
            "member-secret",
            Some(serde_json::json!({ "connectionId": "hand-added", "url": format!("https://{host}/mcp"),
                "secretValue": "Bearer fine" })),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
    }

    #[tokio::test]
    async fn an_mcp_api_key_writes_the_static_row_with_the_vetted_tools_and_shows_nowhere() {
        let host = "mcp.bugwatch-connect.example";
        let (app, state, seen) = mcp_world(host, Arc::new(AtomicBool::new(true))).await;
        assert_eq!(bugwatch_state(&app).await, "connect");

        let (s, b) = put_mcp_key(
            &app,
            MCP_KEY,
            serde_json::json!({ "apiKey": MCP_KEY }),
            "mcp-key-1",
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(b["state"], "connected", "{b}");
        assert_eq!(b["mcp"]["url"], format!("https://{host}/mcp"), "{b}");
        // The row: this member's, static, the connector's own URL, one header binding, and only the
        // vetted tool (the server also offered `create_issue`).
        let row = mcp_row(&state).unwrap();
        assert_eq!(row.auth_mode, crate::mcp_registry::McpAuthMode::Static);
        assert_eq!(row.url, format!("https://{host}/mcp"));
        assert_eq!(row.header_bindings.len(), 1);
        assert_eq!(row.header_bindings[0].name, "Authorization");
        assert_eq!(row.header_bindings[0].binding_id, MCP_HEADER_ROW);
        let names: Vec<&str> = row.tools.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["list_issues"]);
        // The connector's risk class is applied: the read-only tool.
        assert!(row.ask_tools.is_empty(), "{:?}", row.ask_tools);
        // The header row: `Authorization: Bearer <key>`, a `connection` row pinned to the one MCP host.
        assert_eq!(mcp_header(&state), format!("Bearer {MCP_KEY}"));
        let meta = state
            .store
            .get_host_secret_metadata(SecretScope::User, "ada", MCP_HEADER_ROW)
            .unwrap();
        assert_eq!(meta.kind, crate::secrets::SecretKind::Connection);
        assert_eq!(meta.allowed_destinations, vec![host.to_string()]);
        // The server saw the member's key as the bearer.
        assert!(seen
            .lock()
            .unwrap()
            .auth
            .iter()
            .any(|a| a.as_deref() == Some(format!("Bearer {MCP_KEY}").as_str())));
        // No route answer holds the key, for the member or for the admin.
        for (secret, uri) in [
            ("member-secret", "/v1/connectors"),
            ("orgadmin-secret", "/v1/connectors"),
            ("member-secret", "/v1/mcp/connections"),
        ] {
            let (s, l) = call(&app, "GET", uri, secret, None).await;
            assert_eq!(s, StatusCode::OK);
            assert!(!l.to_string().contains(MCP_KEY), "{secret} {uri}: {l}");
        }
        // Only the member's own tile reads connected.
        let (_, l) = call(&app, "GET", "/v1/connectors", "orgadmin-secret", None).await;
        assert_eq!(entry_of(&l, MCP_ID)["state"], "connect");
        // A replaced key is the stored one, and the row stays the one row.
        let (s, b) = put_mcp_key(
            &app,
            "bwk_live_ZZZZZZZZZZZZZZZZ",
            serde_json::json!({ "apiKey": "bwk_live_ZZZZZZZZZZZZZZZZ" }),
            "mcp-key-2",
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(mcp_header(&state), "Bearer bwk_live_ZZZZZZZZZZZZZZZZ");
        assert_eq!(mcp_row(&state).unwrap().tools.len(), 1);
    }

    #[tokio::test]
    async fn the_mcp_key_route_keeps_the_rest_routes_refusals_and_texts() {
        let host = "mcp.bugwatch-refusals.example";
        let (app, state) = app_world_with_state();
        static_mcp_server(host, Arc::new(AtomicBool::new(true)));
        install_bugwatch(&state.store, host);
        // Not enabled: 403 in the Phase 6 words, nothing written, the server never asked.
        let (s, b) = put_mcp_key(
            &app,
            MCP_KEY,
            serde_json::json!({ "apiKey": MCP_KEY }),
            "r-1",
        )
        .await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{b}");
        assert!(b.to_string().contains("ask your admin"), "{b}");
        let (s, _) = call(
            &app,
            "PUT",
            "/v1/connectors/keyed-bugwatch/node",
            "orgadmin-secret",
            Some(serde_json::json!({ "enabled": true })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        // The body rules and their fixed texts.
        for body in [
            serde_json::json!({ "apiKey": "short" }),
            serde_json::json!({ "apiKey": "has white space in it" }),
            serde_json::json!({ "apiKey": MCP_KEY, "url": "https://evil.example/mcp" }),
            serde_json::json!({ "accessToken": "a1234567", "refreshToken": "r1234567", "idToken": "x" }),
            serde_json::json!([MCP_KEY]),
        ] {
            let (s, b) = put_mcp_key(&app, MCP_KEY, body, "r-2").await;
            assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
            let t = b.to_string();
            assert!(
                t.contains(crate::connectors::API_KEY_BODY_TEXT)
                    || t.contains(crate::connectors::API_KEY_VALUE_TEXT),
                "{b}"
            );
        }
        assert!(mcp_row(&state).is_err());
        assert!(!has_mcp_header(&state));
        // A key with no connection scope is refused before anything else.
        let (s, b) = call_with_key(
            &app,
            "PUT",
            "/v1/connectors/keyed-bugwatch/connection",
            "wrong-secret",
            Some(serde_json::json!({ "apiKey": MCP_KEY })),
            "r-3",
        )
        .await;
        assert!(
            s == StatusCode::UNAUTHORIZED || s == StatusCode::FORBIDDEN,
            "{s} {b}"
        );
        assert!(!has_mcp_header(&state));
    }

    #[tokio::test]
    async fn a_failed_tool_listing_writes_no_key_and_keeps_the_old_one() {
        let host = "mcp.bugwatch-listing.example";
        let tools_ok = Arc::new(AtomicBool::new(false));
        let (app, state, seen) = mcp_world(host, tools_ok.clone()).await;
        // A first connect whose listing fails stores nothing: no row, no header row.
        let (s, b) = put_mcp_key(
            &app,
            MCP_KEY,
            serde_json::json!({ "apiKey": MCP_KEY }),
            "l-1",
        )
        .await;
        assert_ne!(s, StatusCode::OK, "{b}");
        assert!(mcp_row(&state).is_err());
        assert!(!has_mcp_header(&state));
        assert_eq!(bugwatch_state(&app).await, "connect");
        // A working listing connects with the first key.
        tools_ok.store(true, std::sync::atomic::Ordering::SeqCst);
        let (s, b) = put_mcp_key(
            &app,
            MCP_KEY,
            serde_json::json!({ "apiKey": MCP_KEY }),
            "l-2",
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        let before = mcp_row(&state).unwrap();
        // The server refuses the listing again; a new key is tried and refused, and the old key
        // and the row stay exactly as they were.
        tools_ok.store(false, std::sync::atomic::Ordering::SeqCst);
        let new_key = "bwk_live_NEWNEWNEWNEWNEWN";
        let (s, b) = put_mcp_key(
            &app,
            new_key,
            serde_json::json!({ "apiKey": new_key }),
            "l-3",
        )
        .await;
        assert_ne!(s, StatusCode::OK, "{b}");
        assert!(
            seen.lock()
                .unwrap()
                .auth
                .iter()
                .any(|a| a.as_deref() == Some(format!("Bearer {new_key}").as_str())),
            "the listing was tried with the new key"
        );
        assert_eq!(mcp_header(&state), format!("Bearer {MCP_KEY}"));
        let after = mcp_row(&state).unwrap();
        assert_eq!(after.tools, before.tools);
        assert_eq!(after.header_bindings, before.header_bindings);
        assert_eq!(bugwatch_state(&app).await, "connected");
    }

    #[tokio::test]
    async fn two_concurrent_first_mcp_key_connects_leave_the_accepted_ones_key_in_force() {
        let host = "mcp.bugwatch-race.example";
        let (app, state, _seen) = mcp_world(host, Arc::new(AtomicBool::new(true))).await;
        let send = |app: axum::Router, key: &'static str, idempotency: &'static str| {
            tokio::spawn(async move {
                call_with_key(
                    &app,
                    "PUT",
                    "/v1/connectors/keyed-bugwatch/connection",
                    "member-secret",
                    Some(serde_json::json!({ "apiKey": key })),
                    idempotency,
                )
                .await
            })
        };
        let wait = |arrived: std::sync::mpsc::Receiver<()>, secs: u64| {
            tokio::task::spawn_blocking(move || {
                arrived.recv_timeout(std::time::Duration::from_secs(secs))
            })
        };
        let key_a = "bwk_live_AAAAAAAAAAAAAAAA";
        let key_b = "bwk_live_BBBBBBBBBBBBBBBB";
        // A has read "no row", listed the tools and stops just before its write.
        let (arrived_a, release_a) =
            crate::connectors::test_signal::arm_pause("mcp-connect-write:keyed-bugwatch");
        let a = send(app.clone(), key_a, "race-a");
        wait(arrived_a, 10)
            .await
            .unwrap()
            .expect("A reached its write");
        // B is sent while A holds the member's connection lock: it cannot reach its own write.
        let (arrived_b, release_b) =
            crate::connectors::test_signal::arm_pause("mcp-connect-write:keyed-bugwatch");
        let b = send(app.clone(), key_b, "race-b");
        assert!(
            wait(arrived_b, 1).await.unwrap().is_err(),
            "B got past the member's lock while A was writing"
        );
        // A commits; its key is the one in force.
        release_a.send(()).unwrap();
        let (s, body) = a.await.unwrap();
        assert_eq!(s, StatusCode::OK, "{body}");
        assert_eq!(mcp_header(&state), format!("Bearer {key_a}"));
        // B then reads A's row, replaces it, and its key is the one in force: the accepted
        // request is always the one whose key is stored.
        let _ = release_b.send(());
        let (s, body) = b.await.unwrap();
        assert_eq!(s, StatusCode::OK, "{body}");
        assert_eq!(mcp_header(&state), format!("Bearer {key_b}"));
        assert_eq!(mcp_row(&state).unwrap().tools.len(), 1);
    }

    #[tokio::test]
    async fn a_row_another_writer_created_meanwhile_refuses_the_write_and_stores_no_key() {
        let host = "mcp.bugwatch-guard.example";
        let (app, state, _seen) = mcp_world(host, Arc::new(AtomicBool::new(true))).await;
        let (arrived, release) =
            crate::connectors::test_signal::arm_pause("mcp-connect-write:keyed-bugwatch");
        let a = {
            let app = app.clone();
            tokio::spawn(async move {
                call_with_key(
                    &app,
                    "PUT",
                    "/v1/connectors/keyed-bugwatch/connection",
                    "member-secret",
                    Some(serde_json::json!({ "apiKey": MCP_KEY })),
                    "guard-a",
                )
                .await
            })
        };
        tokio::task::spawn_blocking(move || {
            arrived.recv_timeout(std::time::Duration::from_secs(10))
        })
        .await
        .unwrap()
        .expect("the write reached its guard");
        // A writer that does not take the member's connection lock puts a row there.
        state
            .store
            .upsert_mcp_connection(
                SecretScope::User,
                "ada",
                MCP_ID,
                &format!("https://{host}/mcp"),
                "",
                &[],
                &[],
                crate::mcp_registry::McpAuthMode::Static,
                None,
            )
            .unwrap();
        release.send(()).unwrap();
        let (s, b) = a.await.unwrap();
        assert_eq!(s, StatusCode::CONFLICT, "{b}");
        assert!(!b.to_string().contains(MCP_KEY), "{b}");
        // The refused write stored no key, in the transaction or outside it.
        assert!(!has_mcp_header(&state));
        assert!(mcp_row(&state).unwrap().header_bindings.is_empty());
    }

    #[tokio::test]
    async fn the_mcp_state_reads_connect_and_connected_and_disconnect_removes_both_rows() {
        let host = "mcp.bugwatch-state.example";
        let (app, state, seen) = mcp_world(host, Arc::new(AtomicBool::new(true))).await;
        assert_eq!(bugwatch_state(&app).await, "connect");
        // Disconnecting when not connected is a 404, as for a REST connector.
        let (s, _) = call(
            &app,
            "DELETE",
            "/v1/connectors/keyed-bugwatch/connection",
            "member-secret",
            None,
        )
        .await;
        assert_eq!(s, StatusCode::NOT_FOUND);
        let (s, _) = put_mcp_key(
            &app,
            MCP_KEY,
            serde_json::json!({ "apiKey": MCP_KEY }),
            "s-1",
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(bugwatch_state(&app).await, "connected");
        // The check answers the stored state and asks the MCP server nothing.
        let asked = seen.lock().unwrap().methods.len();
        let (s, b) = call(
            &app,
            "POST",
            "/v1/connectors/keyed-bugwatch/connection/check",
            "member-secret",
            None,
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(b["state"], "connected");
        assert_eq!(
            seen.lock().unwrap().methods.len(),
            asked,
            "the check made a request"
        );
        // A row whose header row is gone is not connected.
        state
            .vault
            .delete_secret_at(SecretScope::User, "ada", MCP_HEADER_ROW)
            .unwrap();
        assert!(mcp_row(&state).is_ok());
        assert_eq!(bugwatch_state(&app).await, "connect");
        let (s, _) = put_mcp_key(
            &app,
            MCP_KEY,
            serde_json::json!({ "apiKey": MCP_KEY }),
            "s-2",
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(bugwatch_state(&app).await, "connected");
        // Another member's row is theirs: olga's tile is `connect`.
        let (_, l) = call(&app, "GET", "/v1/connectors", "orgadmin-secret", None).await;
        assert_eq!(entry_of(&l, MCP_ID)["state"], "connect");
        // Disconnect removes the row and the key, with no request to the server, and works when the
        // node no longer enables the connector.
        let asked = seen.lock().unwrap().methods.len();
        let (s, _) = call(
            &app,
            "PUT",
            "/v1/connectors/keyed-bugwatch/node",
            "orgadmin-secret",
            Some(serde_json::json!({ "enabled": false })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        let (s, b) = call(
            &app,
            "DELETE",
            "/v1/connectors/keyed-bugwatch/connection",
            "member-secret",
            None,
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert!(b.get("providerGrantMayRemain").is_none(), "{b}");
        assert!(mcp_row(&state).is_err());
        assert!(!has_mcp_header(&state));
        assert_eq!(
            seen.lock().unwrap().methods.len(),
            asked,
            "the disconnect made a request"
        );
        // Disabled on the node, so the tile asks the admin; enabled again, it is `connect`.
        assert_eq!(bugwatch_state(&app).await, "askAdmin");
        let (s, _) = call(
            &app,
            "PUT",
            "/v1/connectors/keyed-bugwatch/node",
            "orgadmin-secret",
            Some(serde_json::json!({ "enabled": true })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(bugwatch_state(&app).await, "connect");
        let (s, _) = call(
            &app,
            "DELETE",
            "/v1/connectors/keyed-bugwatch/connection",
            "member-secret",
            None,
        )
        .await;
        assert_eq!(s, StatusCode::NOT_FOUND);
    }
}
