//! Remote MCP connection registry HTTP:
//! `PUT|GET /v1/mcp/connections`, `PATCH|DELETE /v1/mcp/connections/:id`,
//! `POST /v1/mcp/connections/:id/refresh`,
//! `PUT /v1/mcp/connections/:id/oauth/tokens`.

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::{delete, post, put, MethodRouter};
use serde::{Deserialize, Serialize};

use crate::api::auth::{require_idempotency_key, AuthContext};
use crate::api::error::ApiError;
use crate::api::{ensure_ready, json_http_response, AppState};
use crate::connectors;
use crate::error::OrchestratorError;
use crate::mcp_client::{
    connection_host, enforce_registered_host, McpTool, DEFAULT_MCP_TIMEOUT_MS,
};
use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::mcp_registry::{
    apply_oauth_token_metadata, assert_no_visible_name_collision,
    decide_pinned_snapshot_with_instructions, delete_replaced_auth_mode_bindings,
    fetch_tools_snapshot, fetch_tools_snapshot_for_connection, flagged_vendor_text,
    mcp_auth_binding_id, mcp_capabilities, mcp_oauth_refresh_token_binding_id,
    merge_header_bindings, normalize_connect_headers, parse_side_effect_class,
    resolve_connection_auth, sanitize_oauth_last_refresh_error, sanitized_oauth_metadata,
    store_mcp_header_credential, store_mcp_oauth_tokens, suggested_read_only_tools,
    validate_connection_id, validate_mcp_timeout_ms, HeaderBinding, McpAuthMode, McpConnection,
    McpHeaderInput, McpOauthMetadata, McpOauthRegistrationMethod, McpOauthStatus,
    McpOauthTokenWrite, McpSnapshot, PinnedSnapshotDecision,
};
use crate::secrets::{
    authorize_org_secret_write, resolve_secret_scope, vault_owner_user_id, SecretScope,
    SecretsVault,
};

const SCOPE: &str = "secrets:write";

pub fn collection_route() -> MethodRouter<AppState> {
    put(connect).get(list)
}

pub fn item_route() -> MethodRouter<AppState> {
    delete(disconnect).patch(update)
}

pub fn refresh_route() -> MethodRouter<AppState> {
    post(refresh)
}

pub fn oauth_tokens_route() -> MethodRouter<AppState> {
    put(upsert_oauth_tokens)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectMcpRequest {
    pub connection_id: String,
    pub url: String,
    #[serde(default)]
    pub secret_binding_id: String,
    /// Token to store in the vault. Not echoed in the response.
    #[serde(default)]
    pub secret_value: Option<String>,
    #[serde(default)]
    pub scope: Option<String>,
    /// Store a tool list that added or changed tools since the pin.
    #[serde(default)]
    pub approve_drift: bool,
    /// Confirm the vendor's `readOnlyHint: true` claims: those tools become
    /// read-only. Without it a hint is only shown and suggested.
    #[serde(default)]
    pub accept_read_only_hints: bool,
    /// Vendor tool name → `true`/`false`/`read`/`local`/`external`.
    /// Absent tools stay at the mutating default.
    #[serde(default)]
    pub side_effects: BTreeMap<String, String>,
    /// Vendor tool names, or `*` for every tool. Omitted leaves the stored
    /// list unchanged. Every value means `ask`. Only the connection owner
    /// may set this.
    #[serde(default)]
    pub ask_tools: Option<Vec<String>>,
    /// Vendor tool name -> `EffectSpec` JSON (`class`, optional `verify`). Owner-set; a vendor
    /// hint is never trusted. Omitted leaves the stored map unchanged.
    #[serde(default)]
    pub effects: Option<BTreeMap<String, serde_json::Value>>,
    /// Static HTTP headers for the MCP server. Values are write-only (never returned).
    #[serde(default)]
    pub headers: Vec<ConnectMcpHeaderRequest>,
    /// `static` (default) or `oauth`. Omitted on update preserves the existing mode.
    #[serde(default)]
    pub auth_mode: Option<String>,
    /// Required when `authMode` is `oauth` (or when creating oauth). Never includes tokens.
    #[serde(default)]
    pub oauth: Option<ConnectMcpOauthRequest>,
    /// Tool-call wall time in milliseconds. Omitted on create uses the default
    /// (120000). Omitted on update keeps the stored value.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectMcpHeaderRequest {
    pub name: String,
    pub value: String,
}

/// Write-only OAuth tokens from the app after code exchange. Values are never
/// echoed; they live only in the vault.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpsertMcpOauthTokensRequest {
    pub access_token: String,
    pub refresh_token: String,
    #[serde(default)]
    pub client_secret: Option<String>,
    #[serde(default)]
    pub expires_at: Option<i64>,
    #[serde(default)]
    pub expires_in: Option<i64>,
    #[serde(default)]
    pub status: Option<String>,
}

/// OAuth identity fields accepted on connect. Token material is rejected / out of scope
/// for this endpoint (see token upsert).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectMcpOauthRequest {
    pub resource: String,
    pub authorization_server: String,
    pub token_endpoint: String,
    #[serde(default)]
    pub revocation_endpoint: Option<String>,
    pub client_id: String,
    pub registration_method: String,
    #[serde(default)]
    pub scopes: Vec<String>,
    #[serde(default)]
    pub expires_at: Option<i64>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub last_refresh_at: Option<i64>,
    #[serde(default)]
    pub last_refresh_error: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ConnectionView<'a> {
    connection_id: &'a str,
    scope: &'static str,
    owner_user_id: &'a str,
    url: &'a str,
    secret_binding_id: &'a str,
    auth_mode: &'static str,
    tools: &'a [McpTool],
    timeout_ms: u64,
    created_at: i64,
    updated_at: i64,
}

/// JSON for list, connect, tool-snapshot refresh, and token upsert.
///
/// OAuth rows include `oauth.status` (`connected` or `needs_reauth`),
/// `oauth.expiresAt` when known, sanitized `oauth.lastRefreshError` when set,
/// and `hasRefreshBinding`. The refresh flag is vault presence of
/// `{id}/oauth/refresh-token`, not a refresh attempt. Static rows keep
/// `oauth: null` and do not gain those fields. There is no separate GET-by-id;
/// list items and these single-connection bodies are the same shape.
fn connection_value(
    c: &McpConnection,
    has_refresh_binding: bool,
) -> Result<serde_json::Value, OrchestratorError> {
    let mut value = serde_json::to_value(view(c))
        .map_err(|err| OrchestratorError::Core(format!("connection view: {err}")))?;
    let caps = mcp_capabilities(c)?;
    value["capabilities"] = serde_json::to_value(&caps)
        .map_err(|err| OrchestratorError::Core(format!("capabilities view: {err}")))?;
    value["sideEffects"] = serde_json::json!(c.side_effects);
    value["effects"] = serde_json::json!(c.effects);
    value["askTools"] = serde_json::json!(c.ask_tools);
    value["instructions"] = serde_json::json!(c.instructions.as_deref().unwrap_or_default());
    value["suggestedSideEffects"] = serde_json::json!(suggested_read_only_tools(c)
        .into_iter()
        .map(|name| (name, "read"))
        .collect::<BTreeMap<_, _>>());
    value["flaggedVendorText"] = serde_json::to_value(flagged_vendor_text(c))
        .map_err(|err| OrchestratorError::Core(format!("flagged text view: {err}")))?;
    value["headerBindings"] = serde_json::to_value(&c.header_bindings)
        .map_err(|err| OrchestratorError::Core(format!("header bindings view: {err}")))?;
    if let Some(oauth) = &c.oauth {
        let oauth = sanitized_oauth_metadata(oauth);
        let mut oauth_value = serde_json::to_value(&oauth)
            .map_err(|err| OrchestratorError::Core(format!("oauth view: {err}")))?;
        if let Some(obj) = oauth_value.as_object_mut() {
            obj.insert(
                "hasRefreshBinding".to_string(),
                serde_json::Value::Bool(has_refresh_binding),
            );
        }
        value["oauth"] = oauth_value;
    } else {
        value["oauth"] = serde_json::Value::Null;
    }
    Ok(value)
}

/// The row and vault bindings are already gone. Drop reconnect fields so the
/// disconnect body cannot leave `needs_reauth` or `lastRefreshError` behind.
fn omit_deleted_reconnect_state(connection: &mut serde_json::Value) {
    let Some(oauth) = connection.get_mut("oauth").and_then(|v| v.as_object_mut()) else {
        return;
    };
    oauth.remove("status");
    oauth.remove("lastRefreshError");
}

/// True when this OAuth connection has a refresh-token vault binding.
/// Does not read or return the token.
fn refresh_binding_exists(
    vault: &SecretsVault,
    conn: &McpConnection,
) -> Result<bool, OrchestratorError> {
    if conn.oauth.is_none() {
        return Ok(false);
    }
    let binding_id = mcp_oauth_refresh_token_binding_id(&conn.connection_id);
    vault.has_binding_at(conn.scope, &conn.owner_user_id, &binding_id)
}

fn view(c: &McpConnection) -> ConnectionView<'_> {
    ConnectionView {
        connection_id: &c.connection_id,
        scope: c.scope.as_str(),
        owner_user_id: &c.owner_user_id,
        url: &c.url,
        secret_binding_id: &c.secret_binding_id,
        auth_mode: c.auth_mode.as_str(),
        tools: &c.tools,
        timeout_ms: c.timeout_ms,
        created_at: c.created_at,
        updated_at: c.updated_at,
    }
}

fn respond(value: serde_json::Value) -> Response {
    json_http_response(StatusCode::OK, value.to_string())
}

#[derive(Deserialize, Default)]
pub struct ScopeQuery {
    scope: Option<String>,
}

fn parse_scope(raw: Option<&str>) -> Result<Option<SecretScope>, OrchestratorError> {
    match raw.map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok(None),
        Some(s) => SecretScope::parse(s).map(Some),
    }
}

fn parse_connect_oauth(req: ConnectMcpOauthRequest) -> Result<McpOauthMetadata, OrchestratorError> {
    let revocation = req
        .revocation_endpoint
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let status = match req.status.as_deref() {
        None => McpOauthStatus::Connected,
        Some(raw) => McpOauthStatus::parse(raw)?,
    };
    Ok(McpOauthMetadata {
        resource: req.resource.trim().to_string(),
        authorization_server: req.authorization_server.trim().to_string(),
        token_endpoint: req.token_endpoint.trim().to_string(),
        revocation_endpoint: revocation,
        client_id: req.client_id.trim().to_string(),
        registration_method: McpOauthRegistrationMethod::parse(&req.registration_method)?,
        scopes: req.scopes,
        expires_at: req.expires_at,
        status,
        last_refresh_at: req.last_refresh_at,
        last_refresh_error: req
            .last_refresh_error
            .as_deref()
            .and_then(sanitize_oauth_last_refresh_error),
    })
}

/// Org connections snapshot with the org credential only, so a shared
/// snapshot is never taken with one member's personal token.
fn credential_caller(scope: SecretScope, owner: &str) -> &str {
    match scope {
        SecretScope::Org => "",
        SecretScope::User => owner,
    }
}

async fn snapshot(
    state: &AppState,
    scope: SecretScope,
    owner: &str,
    url: &str,
    header_bindings: &[HeaderBinding],
) -> Result<McpSnapshot, ApiError> {
    let vault = state.vault.clone();
    let caller = credential_caller(scope, owner).to_string();
    let url = url.to_string();
    let header_bindings = header_bindings.to_vec();
    tokio::task::spawn_blocking(move || {
        fetch_tools_snapshot(
            &vault,
            &caller,
            &url,
            &header_bindings,
            DEFAULT_MCP_TIMEOUT_MS,
        )
    })
    .await
    .map_err(|e| OrchestratorError::Core(format!("mcp snapshot task failed: {e}")))?
    .map_err(ApiError::from)
    .map(sanitize_snapshot)
}

fn sanitize_snapshot(snapshot: McpSnapshot) -> McpSnapshot {
    McpSnapshot {
        tools: snapshot
            .tools
            .iter()
            .map(crate::mcp_registry::sanitize_mcp_tool)
            .collect(),
        instructions: snapshot
            .instructions
            .as_deref()
            .and_then(crate::mcp_registry::sanitize_mcp_instructions),
    }
}

async fn connect(
    State(state): State<AppState>,
    auth: AuthContext,
    headers: HeaderMap,
    body: String,
) -> Result<Response, ApiError> {
    ensure_ready(&state, true)?;
    state.pdp.authorize(&auth, "ConnectMcpServer", SCOPE)?;
    let idempotency = require_idempotency_key(&headers).map_err(ApiError::from)?;
    let mut req: ConnectMcpRequest = serde_json::from_str(&body)
        .map_err(|_| OrchestratorError::InvalidContract("invalid request body".into()))?;
    validate_connection_id(&req.connection_id)?;
    if let Some(timeout_ms) = req.timeout_ms {
        validate_mcp_timeout_ms(timeout_ms)?;
    }
    let scope = resolve_secret_scope(parse_scope(req.scope.as_deref())?, auth.role());
    authorize_org_secret_write(scope, auth.role())?;
    let owner = vault_owner_user_id(scope, auth.owner_user_id()).to_string();
    let host = connection_host(&req.url).map_err(OrchestratorError::from)?;
    let existing = match state
        .store
        .get_mcp_connection(scope, &owner, &req.connection_id)
    {
        Ok(existing) if connectors::is_stale_installed_row(&state.store, &existing, &req.url)? => {
            // Only a connect the connector would accept (enabled, its own URL) replaces the row.
            connectors::gate_mcp_write(&state.store, &req.connection_id, Some(&req.url), None)?;
            // The connector an update changed names another server than this row's, and the row
            // was marked needs_reauth when the update landed: it is a connection to a server the
            // connector no longer names. Disconnect it (tokens revoked and removed) so the member
            // connects to the connector's current server.
            let (vault, store) = (state.vault.clone(), state.store.clone());
            tokio::task::spawn_blocking(move || {
                crate::mcp_oauth_refresh::purge_disconnected_mcp_connection(
                    &vault, &store, &existing,
                )
            })
            .await
            .map_err(|err| {
                OrchestratorError::Core(format!("mcp disconnect task failed: {err}"))
            })??;
            None
        }
        Ok(existing) => {
            enforce_registered_host(&existing.url, &req.url).map_err(OrchestratorError::from)?;
            Some(existing)
        }
        Err(OrchestratorError::NotFound(_)) => None,
        Err(err) => return Err(err.into()),
    };
    // A connector id is the connector's: enabled on this node, its own URL, its own risk
    // classes. What a request says about classes, ask rules or effects is ignored (CN-1.3).
    let policy = connectors::gate_mcp_write(
        &state.store,
        &req.connection_id,
        Some(&req.url),
        existing.as_ref().map(|c| c.url.as_str()),
    )?;
    if policy.is_some() {
        if scope == SecretScope::Org {
            return Err(OrchestratorError::InvalidContract(
                "a connector is connected by each member, not for the org".into(),
            )
            .into());
        }
        req.side_effects.clear();
        req.ask_tools = None;
        req.effects = None;
        req.accept_read_only_hints = false;
    }
    if req.ask_tools.is_some() {
        if let Some(existing) = existing.as_ref() {
            // An unclaimed row (no creator yet) is claimed by this connect.
            // A recorded creator is the only caller who may change ask rules.
            if !existing.created_by_user_id.is_empty()
                && !crate::mcp_registry::caller_may_set_mcp_ask_rules(
                    existing,
                    auth.owner_user_id(),
                )
            {
                return Err(OrchestratorError::PermissionDenied(
                    "only the connection owner may set ask rules".into(),
                )
                .into());
            }
        }
    }
    let requested_mode = match req.auth_mode.as_deref() {
        None => None,
        Some(raw) => Some(McpAuthMode::parse(raw)?),
    };
    let requested_oauth = match req.oauth.take() {
        None => None,
        Some(oauth) => Some(parse_connect_oauth(oauth)?),
    };
    let (auth_mode, oauth) =
        resolve_connection_auth(existing.as_ref(), requested_mode, requested_oauth)?;
    // Hold the refresh gate for an existing OAuth connection so an in-flight
    // token refresh cannot write bindings back after a mode switch or before
    // this update finishes.
    let mut credential_guard = match existing.as_ref() {
        Some(conn) if conn.auth_mode == McpAuthMode::Oauth => Some(
            crate::host_tools::lock_mcp_credentials(&state.store, conn)
                .await
                .map_err(|_| {
                    OrchestratorError::CorruptState("mcp credential lock unavailable".into())
                })?,
        ),
        _ => None,
    };
    let secret_value = req.secret_value.take();
    if secret_value
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .is_some()
    {
        let generated = mcp_auth_binding_id(&req.connection_id);
        if !req.secret_binding_id.is_empty() && req.secret_binding_id != generated {
            return Err(OrchestratorError::InvalidContract(format!(
                "secretBindingId '{}' does not match the vault binding this connect writes ('{generated}')",
                req.secret_binding_id
            ))
            .into());
        }
    }
    let header_inputs: Vec<McpHeaderInput> = req
        .headers
        .iter()
        .map(|h| McpHeaderInput {
            name: h.name.clone(),
            value: h.value.clone(),
        })
        .collect();
    let normalized = normalize_connect_headers(secret_value.as_deref(), &header_inputs)?;
    let mut stored_bindings = Vec::with_capacity(normalized.len());
    for (name, value) in &normalized {
        let binding_id = store_mcp_header_credential(
            &state.vault,
            scope,
            &owner,
            &req.connection_id,
            name,
            value,
            &host,
            Some(idempotency.as_str()),
            auth.role(),
        )?;
        stored_bindings.push(HeaderBinding {
            name: name.clone(),
            binding_id,
        });
    }
    let leaving_static_for_oauth = existing.as_ref().is_some_and(|prior| {
        prior.auth_mode == McpAuthMode::Static && auth_mode == McpAuthMode::Oauth
    });
    // Static → oauth must not keep the previous mode's header bindings. Those
    // vault ids are deleted below; carrying them forward would point at
    // missing secrets.
    let header_bindings = if leaving_static_for_oauth {
        stored_bindings.clone()
    } else {
        resolve_connect_header_bindings(existing.as_ref(), &req.secret_binding_id, &stored_bindings)
    };
    // OAuth connections may be registered before tokens exist (token upsert is
    // a separate API). Skip live tools/list when there are no headers to send.
    let (tools, instructions) = if auth_mode == McpAuthMode::Oauth && header_bindings.is_empty() {
        (
            existing
                .as_ref()
                .map(|c| c.tools.clone())
                .unwrap_or_default(),
            existing.as_ref().and_then(|c| c.instructions.clone()),
        )
    } else {
        let mut listed = snapshot(&state, scope, &owner, &req.url, &header_bindings).await?;
        if let Some(p) = &policy {
            listed.tools.retain(|t| p.is_vetted(&t.name));
        }
        match existing.as_ref() {
            None => {
                let instructions = Some(listed.instructions.clone().unwrap_or_default());
                (listed.tools, instructions)
            }
            Some(pinned) => {
                let decided = decide_pinned_snapshot_with_instructions(
                    &req.connection_id,
                    &pinned.tools,
                    pinned.instructions.as_deref(),
                    &listed,
                    req.approve_drift,
                )?;
                let tools = match decided.decision {
                    PinnedSnapshotDecision::Identical => pinned.tools.clone(),
                    PinnedSnapshotDecision::Shrunk { tools, .. }
                    | PinnedSnapshotDecision::Reapproved { tools }
                    | PinnedSnapshotDecision::Upgraded { tools } => tools,
                };
                (tools, decided.instructions)
            }
        }
    };
    assert_no_visible_name_collision(&state.store, scope, &owner, &req.connection_id, &tools)?;
    if let Some(prior) = existing.as_ref() {
        if prior.auth_mode != auth_mode {
            let retained: Vec<String> = stored_bindings
                .iter()
                .map(|binding| binding.binding_id.clone())
                .collect();
            delete_replaced_auth_mode_bindings(&state.vault, prior, auth_mode, &retained)?;
        }
    }
    state.store.upsert_mcp_connection(
        scope,
        &owner,
        &req.connection_id,
        &req.url,
        "",
        &header_bindings,
        &tools,
        auth_mode,
        oauth.as_ref(),
    )?;
    if instructions.is_some() {
        state.store.set_mcp_instructions(
            scope,
            &owner,
            &req.connection_id,
            instructions.as_deref(),
        )?;
    }
    if req.approve_drift {
        state
            .store
            .approve_flagged_vendor_text(scope, &owner, &req.connection_id)?;
    }
    if req.accept_read_only_hints {
        state
            .store
            .accept_read_only_hints(scope, &owner, &req.connection_id)?;
    }
    if let Some(timeout_ms) = req.timeout_ms {
        state
            .store
            .set_mcp_connection_timeout(scope, &owner, &req.connection_id, timeout_ms)?;
    }
    if !req.side_effects.is_empty() {
        state.store.set_mcp_side_effects(
            scope,
            &owner,
            &req.connection_id,
            &parse_side_effects(&req.side_effects)?,
        )?;
    }
    if let Some(effects) = &req.effects {
        state
            .store
            .set_mcp_effects(scope, &owner, &req.connection_id, effects)?;
    }
    let mut conn = state.store.note_mcp_connection_creator(
        scope,
        &owner,
        &req.connection_id,
        auth.owner_user_id(),
    )?;
    if let Some(p) = &policy {
        conn = state.store.apply_connector_policy(scope, &owner, p)?;
    }
    if let Some(ask_tools) = req.ask_tools {
        conn = state.store.set_mcp_ask_tools(
            scope,
            &owner,
            &req.connection_id,
            auth.owner_user_id(),
            &ask_tools,
        )?;
    }
    if existing
        .as_ref()
        .is_some_and(|prior| prior.auth_mode != auth_mode)
    {
        if let Some(guard) = credential_guard.as_mut() {
            guard.credentials_replaced();
        }
    }
    drop(credential_guard);
    let has_refresh = refresh_binding_exists(&state.vault, &conn)?;
    Ok(respond(
        serde_json::json!({ "connection": connection_value(&conn, has_refresh)? }),
    ))
}

async fn list(
    State(state): State<AppState>,
    auth: AuthContext,
    Query(query): Query<ScopeQuery>,
) -> Result<Response, ApiError> {
    state.pdp.authorize(&auth, "ListMcpConnections", SCOPE)?;
    let filter = parse_scope(query.scope.as_deref())?;
    let mut conns = state
        .store
        .list_visible_mcp_connections(auth.owner_user_id())?;
    if let Some(scope) = filter {
        conns.retain(|c| c.scope == scope);
    }
    let mut views = Vec::with_capacity(conns.len());
    for conn in &conns {
        let has_refresh = refresh_binding_exists(&state.vault, conn)?;
        views.push(connection_value(conn, has_refresh)?);
    }
    Ok(respond(serde_json::json!({ "connections": views })))
}

async fn disconnect(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(id): Path<String>,
    Query(query): Query<ScopeQuery>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    ensure_ready(&state, true)?;
    state.pdp.authorize(&auth, "DisconnectMcpServer", SCOPE)?;
    let _idempotency = require_idempotency_key(&headers).map_err(ApiError::from)?;
    let scope = resolve_secret_scope(parse_scope(query.scope.as_deref())?, auth.role());
    authorize_org_secret_write(scope, auth.role())?;
    let conn = state
        .store
        .get_mcp_connection(scope, auth.owner_user_id(), &id)?;
    let mut credential_guard = if conn.auth_mode == McpAuthMode::Oauth {
        Some(
            crate::host_tools::lock_mcp_credentials(&state.store, &conn)
                .await
                .map_err(|_| {
                    OrchestratorError::CorruptState("mcp credential lock unavailable".into())
                })?,
        )
    } else {
        None
    };
    // Re-read under the gate so revocation uses a refresh token that a
    // just-finished rotation wrote, not a snapshot from before the lock.
    let conn = state
        .store
        .get_mcp_connection(scope, auth.owner_user_id(), &id)?;
    let vault = state.vault.clone();
    let store = state.store.clone();
    let conn = tokio::task::spawn_blocking(move || {
        crate::mcp_oauth_refresh::purge_disconnected_mcp_connection(&vault, &store, &conn)
    })
    .await
    .map_err(|err| OrchestratorError::Core(format!("mcp disconnect task failed: {err}")))??;
    if let Some(guard) = credential_guard.as_mut() {
        guard.credentials_replaced();
    }
    drop(credential_guard);
    let has_refresh = refresh_binding_exists(&state.vault, &conn)?;
    let mut connection = connection_value(&conn, has_refresh)?;
    omit_deleted_reconnect_state(&mut connection);
    Ok(respond(serde_json::json!({ "connection": connection })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpdateMcpTimeoutRequest {
    timeout_ms: u64,
}

/// Change only `timeoutMs`. Does not refetch `tools/list`.
async fn update(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(id): Path<String>,
    Query(query): Query<ScopeQuery>,
    headers: HeaderMap,
    body: String,
) -> Result<Response, ApiError> {
    ensure_ready(&state, true)?;
    state.pdp.authorize(&auth, "ConnectMcpServer", SCOPE)?;
    let _idempotency = require_idempotency_key(&headers).map_err(ApiError::from)?;
    let scope = resolve_secret_scope(parse_scope(query.scope.as_deref())?, auth.role());
    authorize_org_secret_write(scope, auth.role())?;
    let req: UpdateMcpTimeoutRequest = serde_json::from_str(&body)
        .map_err(|_| OrchestratorError::InvalidContract("invalid request body".into()))?;
    validate_mcp_timeout_ms(req.timeout_ms)?;
    let owner = vault_owner_user_id(scope, auth.owner_user_id()).to_string();
    let conn = state
        .store
        .set_mcp_connection_timeout(scope, &owner, &id, req.timeout_ms)?;
    let has_refresh = refresh_binding_exists(&state.vault, &conn)?;
    Ok(respond(serde_json::json!({
        "connection": connection_value(&conn, has_refresh)?
    })))
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct RefreshMcpRequest {
    #[serde(default)]
    approve_drift: bool,
    #[serde(default)]
    accept_read_only_hints: bool,
}

async fn refresh(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(id): Path<String>,
    Query(query): Query<ScopeQuery>,
    headers: HeaderMap,
    body: String,
) -> Result<Response, ApiError> {
    ensure_ready(&state, true)?;
    state.pdp.authorize(&auth, "RefreshMcpConnection", SCOPE)?;
    let _idempotency = require_idempotency_key(&headers).map_err(ApiError::from)?;
    let scope = resolve_secret_scope(parse_scope(query.scope.as_deref())?, auth.role());
    authorize_org_secret_write(scope, auth.role())?;
    let existing = state
        .store
        .get_mcp_connection(scope, auth.owner_user_id(), &id)?;
    let req: RefreshMcpRequest = if body.trim().is_empty() {
        RefreshMcpRequest::default()
    } else {
        serde_json::from_str(&body)
            .map_err(|_| OrchestratorError::InvalidContract("invalid request body".into()))?
    };
    let listed = snapshot_connection(&state, &existing).await?;
    let decided = decide_pinned_snapshot_with_instructions(
        &id,
        &existing.tools,
        existing.instructions.as_deref(),
        &listed,
        req.approve_drift,
    )?;
    let new_instructions = decided.instructions.clone();
    let (conn, drift, removed) = match decided.decision {
        PinnedSnapshotDecision::Identical => (existing, "identical", Vec::new()),
        PinnedSnapshotDecision::Shrunk { tools, removed } => {
            let owner = existing.owner_user_id.clone();
            let url = existing.url.clone();
            let header_bindings = existing.header_bindings.clone();
            let auth_mode = existing.auth_mode;
            let oauth = existing.oauth.clone();
            assert_no_visible_name_collision(&state.store, scope, &owner, &id, &tools)?;
            let conn = state.store.upsert_mcp_connection(
                scope,
                &owner,
                &id,
                &url,
                "",
                &header_bindings,
                &tools,
                auth_mode,
                oauth.as_ref(),
            )?;
            (conn, "shrunk", removed)
        }
        PinnedSnapshotDecision::Reapproved { tools } => {
            let owner = existing.owner_user_id.clone();
            let url = existing.url.clone();
            let header_bindings = existing.header_bindings.clone();
            let auth_mode = existing.auth_mode;
            let oauth = existing.oauth.clone();
            assert_no_visible_name_collision(&state.store, scope, &owner, &id, &tools)?;
            let conn = state.store.upsert_mcp_connection(
                scope,
                &owner,
                &id,
                &url,
                "",
                &header_bindings,
                &tools,
                auth_mode,
                oauth.as_ref(),
            )?;
            (conn, "reapproved", Vec::new())
        }
        PinnedSnapshotDecision::Upgraded { tools } => {
            let owner = existing.owner_user_id.clone();
            let url = existing.url.clone();
            let header_bindings = existing.header_bindings.clone();
            let auth_mode = existing.auth_mode;
            let oauth = existing.oauth.clone();
            assert_no_visible_name_collision(&state.store, scope, &owner, &id, &tools)?;
            let conn = state.store.upsert_mcp_connection(
                scope,
                &owner,
                &id,
                &url,
                "",
                &header_bindings,
                &tools,
                auth_mode,
                oauth.as_ref(),
            )?;
            (conn, "upgraded", Vec::new())
        }
    };
    let conn = if new_instructions.is_some() && new_instructions != conn.instructions {
        state.store.set_mcp_instructions(
            conn.scope,
            &conn.owner_user_id,
            &id,
            new_instructions.as_deref(),
        )?
    } else {
        conn
    };
    let conn = if req.approve_drift {
        state
            .store
            .approve_flagged_vendor_text(conn.scope, &conn.owner_user_id, &id)?
    } else {
        conn
    };
    let conn = if req.accept_read_only_hints {
        state
            .store
            .accept_read_only_hints(conn.scope, &conn.owner_user_id, &id)?
    } else {
        conn
    };
    // A connector row takes the definition's classes at every refresh (open question 6).
    let conn = match state.store.mcp_policy(&id)?.filter(|p| p.url == conn.url) {
        Some(p) => state
            .store
            .apply_connector_policy(conn.scope, &conn.owner_user_id, &p)?,
        None => conn,
    };
    let has_refresh = refresh_binding_exists(&state.vault, &conn)?;
    Ok(respond(serde_json::json!({
        "connection": connection_value(&conn, has_refresh)?,
        "drift": drift,
        "removedTools": removed,
    })))
}

/// Header bindings used for connect-time snapshot and persistence.
fn resolve_connect_header_bindings(
    existing: Option<&McpConnection>,
    secret_binding_id: &str,
    stored: &[HeaderBinding],
) -> Vec<HeaderBinding> {
    if !stored.is_empty() {
        return match existing {
            Some(ex) => merge_header_bindings(&ex.header_bindings, stored),
            None => stored.to_vec(),
        };
    }
    if let Some(ex) = existing {
        if !ex.header_bindings.is_empty() {
            return ex.header_bindings.clone();
        }
    }
    if secret_binding_id.is_empty() {
        return Vec::new();
    }
    vec![HeaderBinding {
        name: "Authorization".into(),
        binding_id: secret_binding_id.to_string(),
    }]
}

async fn snapshot_connection(
    state: &AppState,
    conn: &McpConnection,
) -> Result<McpSnapshot, ApiError> {
    let vault = state.vault.clone();
    let caller = credential_caller(conn.scope, &conn.owner_user_id).to_string();
    let policy = state
        .store
        .mcp_policy(&conn.connection_id)?
        .filter(|p| p.url == conn.url);
    let conn = conn.clone();
    tokio::task::spawn_blocking(move || {
        fetch_tools_snapshot_for_connection(&vault, &caller, &conn, DEFAULT_MCP_TIMEOUT_MS)
    })
    .await
    .map_err(|e| OrchestratorError::Core(format!("mcp snapshot task failed: {e}")))?
    .map_err(ApiError::from)
    .map(sanitize_snapshot)
    .map(|mut listed| {
        if let Some(p) = &policy {
            listed.tools.retain(|t| p.is_vetted(&t.name));
        }
        listed
    })
}

async fn upsert_oauth_tokens(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(id): Path<String>,
    Query(query): Query<ScopeQuery>,
    headers: HeaderMap,
    body: String,
) -> Result<Response, ApiError> {
    ensure_ready(&state, true)?;
    state.pdp.authorize(&auth, "ConnectMcpServer", SCOPE)?;
    let idempotency = require_idempotency_key(&headers).map_err(ApiError::from)?;
    let mut req: UpsertMcpOauthTokensRequest = serde_json::from_str(&body)
        .map_err(|_| OrchestratorError::InvalidContract("invalid request body".into()))?;
    let scope = resolve_secret_scope(parse_scope(query.scope.as_deref())?, auth.role());
    authorize_org_secret_write(scope, auth.role())?;
    let owner = vault_owner_user_id(scope, auth.owner_user_id()).to_string();
    let conn = state.store.get_mcp_connection(scope, &owner, &id)?;
    let policy = connectors::gate_mcp_write(&state.store, &id, None, Some(&conn.url))?;
    // Serialize with tool-call refresh for this connection: a refresh that
    // started with the old credentials must not finish afterwards and mark
    // the freshly reconnected connection needs_reauth.
    let mut credentials = crate::host_tools::lock_mcp_credentials(&state.store, &conn)
        .await
        .map_err(|_| OrchestratorError::CorruptState("mcp credential lock unavailable".into()))?;
    let conn = state.store.get_mcp_connection(scope, &owner, &id)?;
    if conn.auth_mode != McpAuthMode::Oauth {
        return Err(OrchestratorError::InvalidContract(
            "oauth token upsert requires authMode=oauth".into(),
        )
        .into());
    }
    // Validate the complete metadata update before mutating any vault binding.
    // A rejected expiry/status must not rotate credentials or consume idempotency.
    let expires_at = resolve_token_expires_at(
        req.expires_at,
        req.expires_in,
        conn.oauth.as_ref().and_then(|oauth| oauth.expires_at),
    )?;
    let status = match req.status.as_deref() {
        None => McpOauthStatus::Connected,
        Some(raw) => McpOauthStatus::parse(raw)?,
    };
    let oauth = conn.oauth.as_ref().ok_or_else(|| {
        OrchestratorError::InvalidContract(
            "oauth token upsert requires oauth metadata on the connection".into(),
        )
    })?;
    let oauth = apply_oauth_token_metadata(oauth, expires_at, status)?;
    let access_token = std::mem::take(&mut req.access_token);
    let refresh_token = std::mem::take(&mut req.refresh_token);
    let client_secret = req.client_secret.take();
    let tokens = McpOauthTokenWrite {
        access_token: access_token.trim().to_string(),
        refresh_token: refresh_token.trim().to_string(),
        client_secret: client_secret
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
    };
    drop(access_token);
    drop(refresh_token);
    let bindings = store_mcp_oauth_tokens(
        &state.vault,
        &conn,
        &tokens,
        Some(idempotency.as_str()),
        auth.role(),
    )?;
    drop(tokens);
    let conn = state.store.upsert_mcp_connection(
        scope,
        &owner,
        &id,
        &conn.url,
        "",
        &conn.header_bindings,
        &conn.tools,
        McpAuthMode::Oauth,
        Some(&oauth),
    )?;
    credentials.credentials_replaced();
    drop(credentials);
    // A connector's tools are listed once its tokens are in, and only the vetted ones stay.
    let conn = match &policy {
        Some(p) => {
            let listed = snapshot_connection(&state, &conn).await?;
            state.store.upsert_mcp_connection(
                scope,
                &owner,
                &id,
                &conn.url,
                "",
                &conn.header_bindings,
                &listed.tools,
                McpAuthMode::Oauth,
                conn.oauth.as_ref(),
            )?;
            if listed.instructions.is_some() {
                state.store.set_mcp_instructions(
                    scope,
                    &owner,
                    &id,
                    listed.instructions.as_deref(),
                )?;
            }
            state.store.apply_connector_policy(scope, &owner, p)?
        }
        None => conn,
    };
    let mut bindings_json = serde_json::json!({
        "accessToken": bindings.access_token_binding_id,
        "refreshToken": bindings.refresh_token_binding_id,
    });
    if let Some(client_secret_binding_id) = bindings.client_secret_binding_id {
        bindings_json["clientSecret"] = serde_json::Value::String(client_secret_binding_id);
    }
    let has_refresh = refresh_binding_exists(&state.vault, &conn)?;
    // A connector-backed row (Linear): its reconnect notification is done.
    if conn.scope == SecretScope::User
        && state
            .store
            .mcp_policy(&conn.connection_id)?
            .is_some_and(|p| p.url == conn.url)
    {
        state.store.resolve_connection_needs_reauth(
            &conn.owner_user_id,
            &conn.connection_id,
            "acted",
        );
    }
    // Runs parked on this connection (reauth) continue now that it is connected again.
    super::reauth::resume_parked(&state, &conn).await;
    Ok(respond(serde_json::json!({
        "connection": connection_value(&conn, has_refresh)?,
        "bindings": bindings_json,
    })))
}

fn resolve_token_expires_at(
    expires_at: Option<i64>,
    expires_in: Option<i64>,
    existing: Option<i64>,
) -> Result<Option<i64>, OrchestratorError> {
    if let Some(expires_at) = expires_at {
        if expires_at < 0 {
            return Err(OrchestratorError::InvalidContract(
                "expiresAt must be a non-negative unix timestamp".into(),
            ));
        }
        return Ok(Some(expires_at));
    }
    if let Some(expires_in) = expires_in {
        if expires_in < 0 {
            return Err(OrchestratorError::InvalidContract(
                "expiresIn must be a non-negative duration in seconds".into(),
            ));
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        return Ok(Some(now.saturating_add(expires_in)));
    }
    Ok(existing)
}

fn parse_side_effects(
    raw: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, i32>, OrchestratorError> {
    let mut out = BTreeMap::new();
    for (name, class) in raw {
        let class = parse_side_effect_class(class)
            .map_err(|err| OrchestratorError::InvalidContract(format!("tool '{name}': {err}")))?;
        out.insert(name.clone(), class);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;
    use crate::mcp_client::McpTool;
    use crate::mcp_registry::{
        HeaderBinding, McpOauthMetadata, McpOauthRegistrationMethod, McpOauthStatus,
    };

    fn sample_oauth() -> McpOauthMetadata {
        McpOauthMetadata {
            resource: "https://mcp.example.com/mcp".into(),
            authorization_server: "https://auth.example.com".into(),
            token_endpoint: "https://auth.example.com/oauth/token".into(),
            revocation_endpoint: Some("https://auth.example.com/oauth/revoke".into()),
            client_id: "public-client".into(),
            registration_method: McpOauthRegistrationMethod::Dcr,
            scopes: vec!["mcp".into()],
            expires_at: Some(1_700_000_000),
            status: McpOauthStatus::Connected,
            last_refresh_at: None,
            last_refresh_error: None,
        }
    }

    #[test]
    fn connection_view_json_omits_header_plaintext() {
        let conn = McpConnection {
            connection_id: "acme".into(),
            owner_user_id: "alice".into(),
            scope: SecretScope::User,
            url: "https://mcp.example.com/mcp".into(),
            secret_binding_id: "acme/auth-header".into(),
            header_bindings: vec![
                HeaderBinding {
                    name: "Authorization".into(),
                    binding_id: "acme/auth-header".into(),
                },
                HeaderBinding {
                    name: "X-Extra".into(),
                    binding_id: "acme/header/x-extra".into(),
                },
            ],
            auth_mode: McpAuthMode::Static,
            oauth: None,
            tools: vec![McpTool {
                name: "ping".into(),
                description: None,
                input_schema: serde_json::json!({}),
                ..Default::default()
            }],
            side_effects: BTreeMap::new(),
            created_at: 1,
            updated_at: 1,
            ask_tools: Vec::new(),
            created_by_user_id: String::new(),
            timeout_ms: crate::mcp_registry::DEFAULT_MCP_TIMEOUT_MS,
            instructions: None,
            approved_vendor_text: Default::default(),
            effects: BTreeMap::new(),
        };
        let leaked = "Bearer super-secret-header-value";
        let json = connection_value(&conn, true).unwrap().to_string();
        assert!(!json.contains(leaked));
        assert!(!json.contains("super-secret-header-value"));
        assert!(json.contains("headerBindings"));
        assert!(json.contains("acme/header/x-extra"));
        assert!(json.contains("\"askTools\":[]"), "{json}");
        assert!(json.contains("\"authMode\":\"static\""));
        assert!(json.contains("\"timeoutMs\":120000"), "{json}");
        assert!(json.contains("\"oauth\":null"));
        assert!(!json.contains("hasRefreshBinding"));
        assert!(!json.contains("expiresAt"));
        assert!(!json.contains("lastRefreshError"));
        assert!(!json.contains("needs_reauth"));
        assert!(!json.contains("\"status\""));
    }

    #[test]
    fn connection_view_includes_oauth_metadata_without_tokens() {
        let oauth = sample_oauth();
        let conn = McpConnection {
            connection_id: "acme".into(),
            owner_user_id: "alice".into(),
            scope: SecretScope::User,
            url: "https://mcp.example.com/mcp".into(),
            secret_binding_id: "".into(),
            header_bindings: vec![],
            auth_mode: McpAuthMode::Oauth,
            oauth: Some(oauth.clone()),
            tools: vec![],
            side_effects: BTreeMap::new(),
            created_at: 1,
            updated_at: 1,
            ask_tools: Vec::new(),
            created_by_user_id: String::new(),
            timeout_ms: crate::mcp_registry::DEFAULT_MCP_TIMEOUT_MS,
            instructions: None,
            approved_vendor_text: Default::default(),
            effects: BTreeMap::new(),
        };
        let json = connection_value(&conn, false).unwrap();
        assert_eq!(json["authMode"], "oauth");
        assert_eq!(json["oauth"]["registrationMethod"], "dcr");
        assert_eq!(json["oauth"]["status"], "connected");
        assert_eq!(json["oauth"]["expiresAt"], 1_700_000_000);
        assert_eq!(json["oauth"]["hasRefreshBinding"], false);
        assert_eq!(json["oauth"]["clientId"], "public-client");
        let text = json.to_string();
        assert!(!text.contains("access_token"));
        assert!(!text.contains("refresh_token"));
        assert!(!text.contains("client_secret"));
        assert!(json["oauth"].get("accessToken").is_none());
        assert!(json["oauth"].get("refreshToken").is_none());
        assert!(json["oauth"].get("clientSecret").is_none());
    }

    #[test]
    fn connection_view_maps_needs_reauth_and_refresh_binding_without_tokens() {
        let mut oauth = sample_oauth();
        oauth.status = McpOauthStatus::NeedsReauth;
        oauth.expires_at = Some(1_700_000_100);
        oauth.last_refresh_error = Some("invalid_grant".into());
        let conn = McpConnection {
            connection_id: "acme".into(),
            owner_user_id: "alice".into(),
            scope: SecretScope::User,
            url: "https://mcp.example.com/mcp".into(),
            secret_binding_id: "".into(),
            header_bindings: vec![],
            auth_mode: McpAuthMode::Oauth,
            oauth: Some(oauth),
            tools: vec![],
            side_effects: BTreeMap::new(),
            created_at: 1,
            updated_at: 1,
            ask_tools: Vec::new(),
            created_by_user_id: String::new(),
            timeout_ms: crate::mcp_registry::DEFAULT_MCP_TIMEOUT_MS,
            instructions: None,
            approved_vendor_text: Default::default(),
            effects: BTreeMap::new(),
        };
        let json = connection_value(&conn, true).unwrap();
        assert_eq!(json["authMode"], "oauth");
        assert_eq!(json["oauth"]["status"], "needs_reauth");
        assert_eq!(json["oauth"]["expiresAt"], 1_700_000_100);
        assert_eq!(json["oauth"]["hasRefreshBinding"], true);
        assert_eq!(json["oauth"]["lastRefreshError"], "invalid_grant");
        let text = json.to_string();
        assert!(!text.contains("access_token"));
        assert!(!text.contains("refresh_token"));
        assert!(!text.contains("client_secret"));
        assert!(!text.contains("Bearer "));
    }

    #[test]
    fn connection_view_sanitizes_refresh_error_for_list_and_snapshot() {
        let mut oauth = sample_oauth();
        oauth.status = McpOauthStatus::NeedsReauth;
        oauth.expires_at = Some(1_700_000_100);
        oauth.last_refresh_error =
            Some(r#"{"error":"invalid_grant","error_description":"SECRET-AS-CONTENT"}"#.into());
        let conn = McpConnection {
            connection_id: "acme".into(),
            owner_user_id: "alice".into(),
            scope: SecretScope::User,
            url: "https://mcp.example.com/mcp".into(),
            secret_binding_id: "".into(),
            header_bindings: vec![],
            auth_mode: McpAuthMode::Oauth,
            oauth: Some(oauth),
            tools: vec![],
            side_effects: BTreeMap::new(),
            created_at: 1,
            updated_at: 1,
            ask_tools: Vec::new(),
            created_by_user_id: String::new(),
            timeout_ms: crate::mcp_registry::DEFAULT_MCP_TIMEOUT_MS,
            instructions: None,
            approved_vendor_text: Default::default(),
            effects: BTreeMap::new(),
        };
        let json = connection_value(&conn, true).unwrap();
        assert_eq!(json["oauth"]["status"], "needs_reauth");
        assert_eq!(json["oauth"]["expiresAt"], 1_700_000_100);
        assert_eq!(json["oauth"]["lastRefreshError"], "refresh failed");
        let text = json.to_string();
        assert!(!text.contains("SECRET-AS-CONTENT"));
        assert!(!text.contains("error_description"));

        let mut deleted = json;
        omit_deleted_reconnect_state(&mut deleted);
        assert!(deleted["oauth"].get("status").is_none());
        assert!(deleted["oauth"].get("lastRefreshError").is_none());
        assert_eq!(deleted["oauth"]["clientId"], "public-client");
        assert!(!deleted.to_string().contains("needs_reauth"));
    }

    #[test]
    fn connection_view_omits_expires_at_when_unknown() {
        let mut oauth = sample_oauth();
        oauth.expires_at = None;
        let conn = McpConnection {
            connection_id: "acme".into(),
            owner_user_id: "alice".into(),
            scope: SecretScope::User,
            url: "https://mcp.example.com/mcp".into(),
            secret_binding_id: "".into(),
            header_bindings: vec![],
            auth_mode: McpAuthMode::Oauth,
            oauth: Some(oauth),
            tools: vec![],
            side_effects: BTreeMap::new(),
            created_at: 1,
            updated_at: 1,
            ask_tools: Vec::new(),
            created_by_user_id: String::new(),
            timeout_ms: crate::mcp_registry::DEFAULT_MCP_TIMEOUT_MS,
            instructions: None,
            approved_vendor_text: Default::default(),
            effects: BTreeMap::new(),
        };
        let json = connection_value(&conn, false).unwrap();
        assert!(json["oauth"].get("expiresAt").is_none());
        assert_eq!(json["oauth"]["status"], "connected");
        assert_eq!(json["oauth"]["hasRefreshBinding"], false);
    }

    #[test]
    fn connect_oauth_request_parses_registration_method_and_status() {
        let raw = serde_json::json!({
            "resource": "https://mcp.example.com/mcp",
            "authorizationServer": "https://auth.example.com",
            "tokenEndpoint": "https://auth.example.com/token",
            "clientId": "cid",
            "registrationMethod": "cimd",
            "status": "needs_reauth",
            "scopes": ["a", "b"]
        });
        let req: ConnectMcpOauthRequest = serde_json::from_value(raw).unwrap();
        let meta = parse_connect_oauth(req).unwrap();
        assert_eq!(meta.registration_method, McpOauthRegistrationMethod::Cimd);
        assert_eq!(meta.status, McpOauthStatus::NeedsReauth);
        assert_eq!(meta.scopes, vec!["a", "b"]);
    }

    #[test]
    fn connect_oauth_request_sanitizes_last_refresh_error_before_persist() {
        let raw = serde_json::json!({
            "resource": "https://mcp.example.com/mcp",
            "authorizationServer": "https://auth.example.com",
            "tokenEndpoint": "https://auth.example.com/token",
            "clientId": "cid",
            "registrationMethod": "dcr",
            "lastRefreshError": r#"{"error":"invalid_grant","error_description":"SECRET-AS-CONTENT"}"#
        });
        let req: ConnectMcpOauthRequest = serde_json::from_value(raw).unwrap();
        let meta = parse_connect_oauth(req).unwrap();
        assert_eq!(meta.last_refresh_error.as_deref(), Some("refresh failed"));
    }

    #[test]
    fn upsert_oauth_tokens_request_is_write_only_shape() {
        let raw = serde_json::json!({
            "accessToken": "tok-access",
            "refreshToken": "tok-refresh",
            "clientSecret": "tok-secret",
            "expiresAt": 1_700_000_100,
            "expiresIn": 3600,
            "status": "connected"
        });
        let req: UpsertMcpOauthTokensRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.access_token, "tok-access");
        assert_eq!(req.refresh_token, "tok-refresh");
        assert_eq!(req.client_secret.as_deref(), Some("tok-secret"));
        assert_eq!(req.expires_at, Some(1_700_000_100));
    }

    #[test]
    fn resolve_token_expires_at_prefers_absolute_then_expires_in() {
        let at = resolve_token_expires_at(Some(50), Some(10), Some(1)).unwrap();
        assert_eq!(at, Some(50));
        let from_in = resolve_token_expires_at(None, Some(0), Some(1))
            .unwrap()
            .unwrap();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        assert!((now - 2..=now + 2).contains(&from_in));
        let kept = resolve_token_expires_at(None, None, Some(9)).unwrap();
        assert_eq!(kept, Some(9));
        assert!(resolve_token_expires_at(Some(-1), None, None).is_err());
        assert!(resolve_token_expires_at(None, Some(-5), None).is_err());
    }

    fn oauth_connect_body(scope: &str, timeout_ms: Option<u64>) -> serde_json::Value {
        let mut body = serde_json::json!({
            "connectionId": "acme",
            "url": "https://mcp.example.com/mcp",
            "scope": scope,
            "authMode": "oauth",
            "oauth": {
                "resource": "https://mcp.example.com/mcp",
                "authorizationServer": "https://auth.example.com",
                "tokenEndpoint": "https://auth.example.com/oauth/token",
                "clientId": "public-client",
                "registrationMethod": "dcr"
            }
        });
        if let Some(timeout_ms) = timeout_ms {
            body["timeoutMs"] = serde_json::json!(timeout_ms);
        }
        body
    }

    async fn mcp_http(
        app: &axum::Router,
        method: &str,
        uri: &str,
        secret: &str,
        body: Option<serde_json::Value>,
    ) -> (axum::http::StatusCode, serde_json::Value) {
        use axum::body::{Body, HttpBody as _};
        use axum::http::Request;
        use tower::ServiceExt;

        let builder = Request::builder()
            .method(method)
            .uri(uri)
            .header("authorization", format!("Bearer {secret}"))
            .header("idempotency-key", "mcp-timeout-1")
            .header("content-type", "application/json");
        let request = match body {
            Some(body) => builder.body(Body::from(body.to_string())).unwrap(),
            None => builder.body(Body::empty()).unwrap(),
        };
        let response = app.clone().oneshot(request).await.expect("response");
        let status = response.status();
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        let text = String::from_utf8(bytes.to_vec()).expect("utf8");
        let json =
            serde_json::from_str(&text).unwrap_or_else(|_| serde_json::json!({ "raw": text }));
        (status, json)
    }

    #[tokio::test]
    async fn put_timeout_defaults_updates_and_patch_changes_only_timeout() {
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;

        use axum::http::StatusCode;

        use crate::api::auth::ControlPlanePdp;
        use crate::api::{bootstrap_api_key_if_empty, router, AppState};
        use crate::keys::default_scopes_for_role;
        use crate::manager::SessionManager;
        use crate::mcp_registry::DEFAULT_MCP_TIMEOUT_MS;
        use crate::secrets::{SecretScope, SecretsVault};
        use crate::users::{UserRole, OPERATOR_USER_ID};
        use crate::workflow::SqliteWorkflowStore;

        let store = Arc::new(SqliteWorkflowStore::in_memory().expect("store"));
        let operator = bootstrap_api_key_if_empty(&store, Some("operator-secret".into()))
            .expect("bootstrap")
            .expect("secret");
        store.create_user("ada", "ada").unwrap();
        store
            .create_api_key_owned(
                "ada-key",
                "ada",
                "member-secret",
                "User::\"ada\"",
                &default_scopes_for_role(UserRole::Member),
                "ada",
            )
            .unwrap();
        let (manager, _rx) = SessionManager::new(
            store.clone(),
            Arc::new(vam_core::CidAllocator::default()),
            Arc::new(|_vm_id, _cid| -> Box<dyn vam_core::Vmm> {
                unreachable!("mcp timeout test does not boot a VM")
            }),
        );
        let state = AppState {
            store: store.clone(),
            vault: SecretsVault::for_tests(store.clone()),
            manager,
            pdp: Arc::new(ControlPlanePdp::new().expect("pdp")),
            listen_addr: "127.0.0.1:0".to_string(),
            draining: Arc::new(AtomicBool::new(false)),
            ready: Arc::new(AtomicBool::new(true)),
            e2e_mock_scripts: false,
        };
        let app = router(state);

        let (member_put, member_put_body) = mcp_http(
            &app,
            "PUT",
            "/v1/mcp/connections",
            "member-secret",
            Some(oauth_connect_body("org", None)),
        )
        .await;
        let (member_patch, member_patch_body) = mcp_http(
            &app,
            "PATCH",
            "/v1/mcp/connections/acme?scope=org",
            "member-secret",
            Some(serde_json::json!({ "timeoutMs": 120_000 })),
        )
        .await;
        assert_eq!(member_put, StatusCode::FORBIDDEN, "{member_put_body}");
        assert_eq!(member_patch, StatusCode::FORBIDDEN, "{member_patch_body}");
        assert_eq!(member_put_body["message"], member_patch_body["message"]);
        assert!(
            member_put_body["message"]
                .as_str()
                .unwrap_or_default()
                .contains("org-scope"),
            "{member_put_body}"
        );

        let (created, created_body) = mcp_http(
            &app,
            "PUT",
            "/v1/mcp/connections",
            &operator,
            Some(oauth_connect_body("user", None)),
        )
        .await;
        assert_eq!(created, StatusCode::OK, "{created_body}");
        assert_eq!(
            created_body["connection"]["timeoutMs"],
            DEFAULT_MCP_TIMEOUT_MS
        );
        let stored = store
            .get_mcp_connection(SecretScope::User, OPERATOR_USER_ID, "acme")
            .unwrap();
        assert_eq!(stored.timeout_ms, 120_000);

        let (set, set_body) = mcp_http(
            &app,
            "PUT",
            "/v1/mcp/connections",
            &operator,
            Some(oauth_connect_body("user", Some(300_000))),
        )
        .await;
        assert_eq!(set, StatusCode::OK, "{set_body}");
        assert_eq!(set_body["connection"]["timeoutMs"], 300_000);
        assert_eq!(
            store
                .get_mcp_connection(SecretScope::User, OPERATOR_USER_ID, "acme")
                .unwrap()
                .timeout_ms,
            300_000
        );

        let (kept, kept_body) = mcp_http(
            &app,
            "PUT",
            "/v1/mcp/connections",
            &operator,
            Some(oauth_connect_body("user", None)),
        )
        .await;
        assert_eq!(kept, StatusCode::OK, "{kept_body}");
        assert_eq!(kept_body["connection"]["timeoutMs"], 300_000);

        for bad in [999_u64, 600_001] {
            let (status, body) = mcp_http(
                &app,
                "PUT",
                "/v1/mcp/connections",
                &operator,
                Some(oauth_connect_body("user", Some(bad))),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
            let message = body["message"].as_str().unwrap_or_default();
            assert!(message.contains("1000"), "{message}");
            assert!(message.contains("600000"), "{message}");
        }
        let before_patch = store
            .get_mcp_connection(SecretScope::User, OPERATOR_USER_ID, "acme")
            .unwrap();
        assert_eq!(before_patch.timeout_ms, 300_000);

        {
            let connection = store.connection.lock().unwrap();
            connection
                .execute(
                    "UPDATE mcp_connections
                     SET tools_snapshot_json = ?1
                     WHERE connection_id = 'acme'",
                    [r#"[{"name":"ping","inputSchema":{}}]"#],
                )
                .unwrap();
        }

        let (patched, patched_body) = mcp_http(
            &app,
            "PATCH",
            "/v1/mcp/connections/acme?scope=user",
            &operator,
            Some(serde_json::json!({ "timeoutMs": 180_000 })),
        )
        .await;
        assert_eq!(patched, StatusCode::OK, "{patched_body}");
        assert_eq!(patched_body["connection"]["timeoutMs"], 180_000);
        let after = store
            .get_mcp_connection(SecretScope::User, OPERATOR_USER_ID, "acme")
            .unwrap();
        assert_eq!(after.timeout_ms, 180_000);
        assert_eq!(after.url, before_patch.url);
        assert_eq!(after.auth_mode, before_patch.auth_mode);
        assert_eq!(after.oauth, before_patch.oauth);
        assert_eq!(after.header_bindings, before_patch.header_bindings);
        assert_eq!(after.tools.len(), 1);
        assert_eq!(after.tools[0].name, "ping");
        assert_eq!(patched_body["connection"]["tools"][0]["name"], "ping");

        let (missing, missing_body) = mcp_http(
            &app,
            "PATCH",
            "/v1/mcp/connections/missing?scope=user",
            &operator,
            Some(serde_json::json!({ "timeoutMs": 120_000 })),
        )
        .await;
        assert_eq!(missing, StatusCode::NOT_FOUND, "{missing_body}");
        assert_eq!(
            store
                .get_mcp_connection(SecretScope::User, OPERATOR_USER_ID, "acme")
                .unwrap()
                .timeout_ms,
            180_000
        );
    }
}
