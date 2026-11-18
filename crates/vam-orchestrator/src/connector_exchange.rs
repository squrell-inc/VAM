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
//! cannot leave a spent single-use code or refresh token unstored. The member's row, both tokens and
//! the end of a previous server's row are ONE transaction (`commit_connector_connection`), and the
//! transaction first checks that the admin's client (by its revision), the connector's server and
//! the enable are still the ones the code was exchanged for: a client replaced while the provider
//! was answering, even one replaced and then put back, stores nothing and the member signs in again.
//!
//! The row is committed tools-pending (`needs_reauth` with `TOOLS_PENDING_REASON`): the route then
//! adopts the vetted tools, and only that adoption makes it `connected`. A caller that goes away
//! between the two leaves a row the tools-refresh route finishes, never a `connected` row with no
//! tools.

use crate::api::error::ApiError;
use crate::connector_client::{
    client_in, org_client_secret, CodeExchangeRequest, NodeClient, OrgConnector,
};
use crate::connectors::now_secs;
use crate::error::OrchestratorError;
use crate::host_tools::{CredentialGeneration, McpCredentialWriteGuard};
use crate::mcp_oauth_refresh::{
    exchange_authorization_code, prepare_oauth_refresh_revoke, CodeGrant, RefreshFailure,
    RefreshFailureKind,
};
use crate::mcp_registry::{
    ConnectorConnectionCommit, McpAuthMode, McpConnection, McpOauthMetadata,
    McpOauthRegistrationMethod, McpOauthStatus, TOOLS_PENDING_REASON,
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

/// What a committed exchange leaves: the member's row, and the credential gate's generation after
/// the write (the tool adoption that follows is conditional on it).
pub(crate) struct Connected {
    pub(crate) conn: McpConnection,
    pub(crate) credentials: CredentialGeneration,
}

/// The 409 when the node's state moved while the provider was answering.
fn superseded(connector_id: &str) -> OrchestratorError {
    OrchestratorError::Conflict(format!(
        "the OAuth client for `{connector_id}` was changed or the connector was turned off while you were signing in: sign in again"
    ))
}

/// The 502 when the provider spent the code but the node could not save the connection. Not
/// retryable: the same request carries the same spent code, so the member starts a new sign-in.
/// (Other MCP errors keep the global mapping, which is retryable.)
fn spent_but_not_saved() -> ApiError {
    let mut e = ApiError::from(OrchestratorError::Mcp(
        "the provider accepted the sign-in, but the connection could not be saved and the code cannot be used again: sign in again"
            .into(),
    ));
    e.retryable = false;
    e
}

/// The 502 when the provider accepted the code but sent no refresh token. Nothing is written. Not
/// retryable: the code is spent, and a new sign-in gets no refresh token either until the provider
/// is set up to issue one (offline access) for the admin's client.
fn no_refresh_token() -> ApiError {
    let mut e = ApiError::from(OrchestratorError::Mcp(
        "the provider accepted the sign-in but issued no refresh token, so the connection could not be kept: the provider must issue a refresh token (offline access) for this client before you sign in again"
            .into(),
    ));
    e.retryable = false;
    e
}

/// Exchanges the code and connects the member, all under `gate` (the connection's credential
/// gate). Blocking.
///
/// 1. every refusal that needs no network: the row of another server under the connector's id;
/// 2. the org secret, host-checked, and the one POST to the pinned token endpoint;
/// 3. only then the write, one transaction: it re-checks the client, the enable and the connector's
///    server, replaces the row and both tokens (and removes a previous server's row), and the
///    gate's generation is bumped after the commit. A failure to save after the provider spent the
///    code says to sign in again.
pub(crate) fn exchange_and_connect(
    store: &SqliteWorkflowStore,
    vault: &SecretsVault,
    c: &CodeConnect<'_>,
    gate: &mut McpCredentialWriteGuard,
) -> Result<Connected, ApiError> {
    let id = c.connector_id;
    let existing = match store.get_mcp_connection(SecretScope::User, c.owner, id) {
        Ok(row) => Some(row),
        Err(OrchestratorError::NotFound(_)) => None,
        Err(e) => return Err(e.into()),
    };
    // A row on another URL is a server the member added by hand under this id: the connector cannot
    // take it. The one exception is the row of a connector update that moved its server: it is
    // eligible for replacement, and goes only once the new credentials are valid, in the commit.
    let mut stale: Option<McpConnection> = None;
    if let Some(row) = existing.as_ref().filter(|r| r.url != c.org.mcp_url) {
        if crate::connectors::is_stale_installed_row(store, row, &c.org.mcp_url)? {
            stale = Some(row.clone());
        } else {
            crate::connectors::gate_mcp_write(store, id, None, Some(&row.url))?;
            return Err(OrchestratorError::InvalidContract(format!(
                "`{id}` is a connector id and cannot take a server added by hand"
            ))
            .into());
        }
    }
    let existing = existing.filter(|r| r.url == c.org.mcp_url);

    // The secret is read and sent here, to the one host it is pinned to, and nowhere else.
    // A client the admin saved again after the route read it is not this sign-in's: nothing is
    // sent, and the member signs in again (the commit makes the same check if the save lands
    // while the provider answers).
    let secret =
        org_client_secret(vault, c.client, &c.client.token_endpoint).map_err(|e| match e {
            OrchestratorError::Conflict(msg)
                if msg.ends_with(crate::connector_client::SECRET_READ_SUPERSEDED) =>
            {
                superseded(id)
            }
            e => e,
        })?;
    let grant = CodeGrant {
        token_endpoint: &c.client.token_endpoint,
        client_id: &c.client.client_id,
        resource: &c.org.mcp_url,
        code: &c.request.code,
        code_verifier: &c.request.code_verifier,
        redirect_uri: &c.request.redirect_uri,
        auth_method: c.client.token_auth_method,
    };
    let tokens = exchange_authorization_code(&grant, &secret)
        .map_err(|failure| exchange_failure(id, &failure))?;
    drop(secret);
    let Some(refresh_token) = tokens.refresh_token.as_deref() else {
        return Err(no_refresh_token());
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
        // Committed as a sign-in whose tools are not adopted yet: `needs_reauth` with
        // TOOLS_PENDING_REASON, which only the tool adoption clears. If the request goes away (or
        // the node stops) between this commit and the adoption, the row is not left `connected`
        // with no tools: it waits, retryable, for `POST /v1/mcp/connections/{id}/refresh`.
        status: McpOauthStatus::NeedsReauth,
        last_refresh_at: Some(now),
        last_refresh_error: Some(TOOLS_PENDING_REASON.into()),
    };
    let oauth_prior = existing
        .as_ref()
        .filter(|r| r.auth_mode == McpAuthMode::Oauth);
    let (tools, bindings) = match oauth_prior {
        Some(prior) => (prior.tools.clone(), prior.header_bindings.clone()),
        None => (Vec::new(), Vec::new()),
    };
    // The previous server's refresh token is read now, while its row still exists, and revoked
    // (best effort) only once the commit below has replaced that row: a refused or failed commit
    // leaves the old row and a token the provider still honours.
    let stale_revoke = stale
        .as_ref()
        .and_then(|row| prepare_oauth_refresh_revoke(vault, row).ok());
    let commit = ConnectorConnectionCommit {
        owner: c.owner,
        creator: c.creator,
        connection_id: id,
        url: &c.org.mcp_url,
        oauth: &oauth,
        header_bindings: &bindings,
        tools: &tools,
        access_token: &tokens.access_token,
        refresh_token,
        stale: stale.as_ref(),
        replaced_auth: existing
            .as_ref()
            .filter(|r| r.auth_mode != McpAuthMode::Oauth),
    };
    // Inside the transaction: the node's client, the enable and the connector's server are still
    // what this code was exchanged for. The admin's own write marked the member's old row
    // `needs_reauth` when it replaced the client, so a refused commit leaves that mark as it is.
    // The client is compared by its revision, not by its id: a client changed A→B→A while the
    // provider answered has A's id again, but B's save marked the row, and only a new sign-in
    // under the current client may clear that mark.
    let still_current = |tx: &rusqlite::Connection| -> Result<(), OrchestratorError> {
        let same_client = client_in(tx, id)?.is_some_and(|n| {
            n.revision == c.client.revision
                && n.client_id == c.client.client_id
                && n.token_endpoint == c.client.token_endpoint
        });
        let enabled = crate::connectors::node_enabled_in(tx, id)?;
        let same_server =
            crate::connectors::mcp_policy_in(tx, id)?.is_some_and(|p| p.url == c.org.mcp_url);
        if same_client && enabled && same_server {
            Ok(())
        } else {
            Err(superseded(id))
        }
    };
    let conn = match store.commit_connector_connection(vault, &commit, &still_current) {
        Ok(conn) => conn,
        Err(e @ OrchestratorError::Conflict(_)) => return Err(e.into()),
        Err(e) => {
            tracing::warn!(connection_id = id, error = %e, "connector connection not saved after the code was spent");
            return Err(spent_but_not_saved());
        }
    };
    if let Some(prepared) = stale_revoke {
        let _attempt = prepared.send();
    }
    // Callers that began with the replaced tokens retry with these (and do not refresh with the old
    // single-use refresh token).
    gate.credentials_replaced();
    Ok(Connected {
        conn,
        credentials: gate.generation_handle(),
    })
}
