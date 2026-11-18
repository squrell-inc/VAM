//! The node's code exchange for a connector with an admin-supplied client and a secret (CN-5.3,
//! locked decision 13, D29).
//!
//! The app runs the browser sign-in with PKCE and hands the code, the verifier and the redirect URI
//! to `POST /v1/connectors/{id}/connection/oauth/code`. This module is what that route runs: it
//! exchanges the code at the token endpoint the node pinned at Enable with the org's client secret
//! (the secret is read by [`org_client_secret`], which checks the host), then writes the member's
//! connection. The app never holds the secret and never sees a token.
//!
//! Nothing is written unless the provider accepted the code. From then on the writes run to the end
//! on a blocking worker that owns the connection's credential gate, so a caller that goes away
//! cannot leave a spent single-use code or refresh token unstored.

use crate::connector_client::{org_client_secret, CodeExchangeRequest, NodeClient, OrgConnector};
use crate::connectors::now_secs;
use crate::error::OrchestratorError;
use crate::host_tools::McpCredentialWriteGuard;
use crate::mcp_oauth_refresh::{
    exchange_authorization_code, purge_disconnected_mcp_connection, CodeGrant, RefreshFailure,
    RefreshFailureKind,
};
use crate::mcp_registry::{
    delete_replaced_auth_mode_bindings, mcp_oauth_client_secret_binding_id,
    store_refreshed_mcp_tokens, McpAuthMode, McpConnection, McpOauthMetadata,
    McpOauthRegistrationMethod, McpOauthStatus,
};
use crate::secrets::{SecretScope, SecretsVault};
use crate::workflow::SqliteWorkflowStore;

/// What the route decided before the exchange: the member, the connector and its saved client.
pub(crate) struct CodeConnect<'a> {
    /// The member whose connection this is (their user-scope vault owner).
    pub(crate) owner: &'a str,
    /// Who the connection is recorded as created by.
    pub(crate) creator: &'a str,
    pub(crate) connector_id: &'a str,
    pub(crate) org: &'a OrgConnector,
    pub(crate) client: &'a NodeClient,
    pub(crate) request: &'a CodeExchangeRequest,
}

/// The failure of the exchange as the API tells it. Every text is fixed: no code, verifier, secret,
/// token or provider answer is in it.
pub(crate) fn exchange_failure(connector_id: &str, failure: &RefreshFailure) -> OrchestratorError {
    match failure.kind {
        RefreshFailureKind::Transient => OrchestratorError::Mcp(format!(
            "could not exchange the code with the provider: {}",
            failure.reason
        )),
        RefreshFailureKind::NeedsReauth => match failure.reason.as_str() {
            "invalid_client" | "unauthorized_client" => OrchestratorError::Conflict(format!(
                "the provider refused the client id or secret your admin entered for `{connector_id}`: ask your admin to check them"
            )),
            "invalid_grant" => OrchestratorError::InvalidContract(
                "the provider refused the sign-in code: it was used, expired or does not match; sign in again"
                    .into(),
            ),
            _ => OrchestratorError::InvalidContract(
                "the provider refused the code exchange; sign in again".into(),
            ),
        },
    }
}

/// Exchanges the code and connects the member, all under `gate` (the connection's credential
/// gate). Blocking.
///
/// 1. every refusal that needs no network: the row of another server under the connector's id;
/// 2. the org secret, host-checked, and the one POST to the pinned token endpoint;
/// 3. only then the writes: the row (client, endpoints and resource are the node's), the refresh
///    token, the access token, the gate's generation bump.
pub(crate) fn exchange_and_connect(
    store: &SqliteWorkflowStore,
    vault: &SecretsVault,
    c: &CodeConnect<'_>,
    gate: &mut McpCredentialWriteGuard,
) -> Result<McpConnection, OrchestratorError> {
    let id = c.connector_id;
    let existing = match store.get_mcp_connection(SecretScope::User, c.owner, id) {
        Ok(row) => Some(row),
        Err(OrchestratorError::NotFound(_)) => None,
        Err(e) => return Err(e),
    };
    // A row on another URL is a server the member added by hand under this id: the connector cannot
    // take it. The one exception is the row of a connector update that moved its server.
    if let Some(row) = existing.as_ref().filter(|r| r.url != c.org.mcp_url) {
        if crate::connectors::is_stale_installed_row(store, row, &c.org.mcp_url)? {
            purge_disconnected_mcp_connection(vault, store, row)?;
        } else {
            crate::connectors::gate_mcp_write(store, id, None, Some(&row.url))?;
            return Err(OrchestratorError::InvalidContract(format!(
                "`{id}` is a connector id and cannot take a server added by hand"
            )));
        }
    }
    let existing = existing.filter(|r| r.url == c.org.mcp_url);

    // The secret is read and sent here, to the one host it is pinned to, and nowhere else.
    let secret = org_client_secret(vault, c.client, &c.client.token_endpoint)?;
    let grant = CodeGrant {
        token_endpoint: &c.client.token_endpoint,
        client_id: &c.client.client_id,
        resource: &c.org.mcp_url,
        code: &c.request.code,
        code_verifier: &c.request.code_verifier,
        redirect_uri: &c.request.redirect_uri,
    };
    let tokens = exchange_authorization_code(&grant, &secret)
        .map_err(|failure| exchange_failure(id, &failure))?;
    drop(secret);
    let Some(refresh_token) = tokens.refresh_token.as_deref() else {
        return Err(OrchestratorError::Mcp(
            "the provider returned no refresh token, so the connection could not be kept".into(),
        ));
    };

    let now = now_secs();
    let oauth = McpOauthMetadata {
        resource: c.org.mcp_url.clone(),
        authorization_server: c.client.issuer.clone(),
        token_endpoint: c.client.token_endpoint.clone(),
        revocation_endpoint: c.client.revocation_endpoint.clone(),
        client_id: c.client.client_id.clone(),
        registration_method: McpOauthRegistrationMethod::Manual,
        scopes: c.org.scopes.clone(),
        expires_at: tokens
            .expires_in
            .and_then(|s| i64::try_from(s).ok())
            .map(|s| now.saturating_add(s)),
        status: McpOauthStatus::Connected,
        last_refresh_at: Some(now),
        last_refresh_error: None,
    };
    if let Some(prior) = existing
        .as_ref()
        .filter(|r| r.auth_mode != McpAuthMode::Oauth)
    {
        delete_replaced_auth_mode_bindings(vault, prior, McpAuthMode::Oauth, &[])?;
    }
    let (tools, bindings) = match existing
        .as_ref()
        .filter(|r| r.auth_mode == McpAuthMode::Oauth)
    {
        Some(prior) => (prior.tools.clone(), prior.header_bindings.clone()),
        None => (Vec::new(), Vec::new()),
    };
    let conn = store.upsert_mcp_connection(
        SecretScope::User,
        c.owner,
        id,
        &c.org.mcp_url,
        "",
        &bindings,
        &tools,
        McpAuthMode::Oauth,
        Some(&oauth),
    )?;
    // The refresh token first, then the access token (the order a refresh uses): a member whose
    // access write failed refreshes on the first call; one whose refresh write failed would not.
    store_refreshed_mcp_tokens(vault, &conn, &tokens.access_token, Some(refresh_token))?;
    // The org's client has one secret, the org's. A per-row secret never belongs to this row.
    match vault.delete_secret_at(
        SecretScope::User,
        c.owner,
        &mcp_oauth_client_secret_binding_id(id),
    ) {
        Ok(_) | Err(OrchestratorError::NotFound(_)) => {}
        Err(e) => return Err(e),
    }
    let conn = store.note_mcp_connection_creator(SecretScope::User, c.owner, id, c.creator)?;
    // Callers that began with the replaced tokens retry with these (and do not refresh with the old
    // single-use refresh token).
    gate.credentials_replaced();
    Ok(conn)
}
