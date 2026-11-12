//! Host-side OAuth token endpoint and RFC 7009 revocation for remote MCP.
//!
//! Authorization-server requests are sent over HTTPS to a DNS-pinned public IP,
//! with redirects and environment proxies disabled. Errors are deliberately
//! generic: neither a request URL, token value, nor authorization-server
//! response body is safe to log or persist.

use std::io::Read;
use std::time::Duration;

use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;

use crate::error::OrchestratorError;
use crate::mcp_registry::{
    delete_mcp_connection_secrets, mcp_oauth_client_secret_binding_id,
    mcp_oauth_refresh_token_binding_id, validate_oauth_secret_value, McpAuthMode, McpConnection,
    McpOauthMetadata,
};
use crate::secrets::SecretsVault;
use crate::ssrf_guard::{PinError, PinnedUrl};
use crate::workflow::SqliteWorkflowStore;

const MAX_RESPONSE_BYTES: u64 = 64 * 1024;

/// OAuth `error` codes (RFC 6749 §5.2, RFC 8707) that may be echoed into
/// `lastRefreshError`. Anything else from the AS is replaced by a fixed string.
const KNOWN_OAUTH_ERRORS: &[&str] = &[
    "invalid_grant",
    "invalid_client",
    "unauthorized_client",
    "invalid_scope",
    "invalid_request",
    "unsupported_grant_type",
    "invalid_target",
];

/// RFC 7009 §2.2.1 error codes. Only these strings may be copied out of a
/// revocation response; `error_description` and any other body text are dropped
/// because they can echo the token.
const KNOWN_REVOKE_ERRORS: &[&str] = &[
    "invalid_request",
    "invalid_client",
    "unauthorized_client",
    "unsupported_token_type",
];

/// Whether a failed refresh needs the user to reconnect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RefreshFailureKind {
    /// The grant, client, or stored configuration is unusable (for example
    /// `invalid_grant`, a missing refresh credential, or a rejected token
    /// endpoint). Retrying will not help; the connection needs reauthorization.
    NeedsReauth,
    /// Network failure, timeout, AS 5xx/429, or a malformed response. A later
    /// call may refresh successfully, so the stored status is not changed.
    Transient,
}

/// Classified refresh failure. `reason` is a fixed, sanitized string that is
/// safe to persist as `lastRefreshError`: it never contains token material,
/// the endpoint URL, or free-form authorization-server response content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RefreshFailure {
    pub(crate) kind: RefreshFailureKind,
    pub(crate) reason: String,
}

impl RefreshFailure {
    pub(crate) fn needs_reauth(reason: impl Into<String>) -> Self {
        Self {
            kind: RefreshFailureKind::NeedsReauth,
            reason: reason.into(),
        }
    }

    pub(crate) fn transient(reason: impl Into<String>) -> Self {
        Self {
            kind: RefreshFailureKind::Transient,
            reason: reason.into(),
        }
    }
}

impl std::fmt::Display for RefreshFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.reason)
    }
}

/// Secret-bearing result. Do not log or return this value to callers outside
/// the host; the caller is responsible for vault writes and expiry handling.
pub(crate) struct RefreshedToken {
    pub(crate) access_token: String,
    pub(crate) expires_in: Option<u64>,
    pub(crate) refresh_token: Option<String>,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    token_type: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    refresh_token: Option<String>,
}

/// How a refresh request is shaped for its authorization server. The variant is the whole
/// difference between an MCP server and Microsoft Entra, and it is a type so that no code path
/// can add a `resource` to an Entra request (locked decision 11, open question 12).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RefreshProfile<'a> {
    /// A remote MCP server: RFC 8707 `resource` on every refresh, and `client_secret` when the
    /// connection has one (docs: runtime/tools, MCP connections).
    Mcp { resource: &'a str },
    /// Microsoft Entra (`entra` connector profile): no `resource` (the audience is in the
    /// scopes), no `client_secret` (a public client), and the definition's `scope`.
    Entra { scope: &'a str },
}

/// Where a refresh goes and how it is shaped.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RefreshTarget<'a> {
    pub(crate) token_endpoint: &'a str,
    pub(crate) client_id: &'a str,
    pub(crate) profile: RefreshProfile<'a>,
}

/// The only host a connector refresh with the Entra profile is ever sent to.
const ENTRA_AUTHORITY: &str = "https://login.microsoftonline.com";

/// The consent tenant's v2.0 token endpoint. `tenant` must be a lowercase GUID (the consent
/// record's `tenant_id` or a verified `tid` claim); anything else (`common`, `organizations`, a
/// domain, a path) gives `None`, so no request-supplied string can reach the URL.
pub(crate) fn entra_token_endpoint(tenant: &str) -> Option<String> {
    let guid = crate::connectors::parse_tenant_guid(tenant).filter(|g| g == tenant)?;
    Some(format!("{ENTRA_AUTHORITY}/{guid}/oauth2/v2.0/token"))
}

/// The form of a refresh request. Pure, so the shape is unit-testable without a server.
fn refresh_form<'a>(
    target: &RefreshTarget<'a>,
    refresh_token: &'a str,
    client_secret: Option<&'a str>,
) -> Vec<(&'a str, &'a str)> {
    let mut form = vec![
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", target.client_id),
    ];
    match target.profile {
        RefreshProfile::Mcp { resource } => {
            form.push(("resource", resource));
            if let Some(secret) = client_secret {
                // client_secret_post: registered confidential clients may provide a
                // client secret; public clients omit this parameter entirely.
                form.push(("client_secret", secret));
            }
        }
        // No `resource`, and a public client has no secret to send.
        RefreshProfile::Entra { scope } => form.push(("scope", scope)),
    }
    form
}

/// Exchange a vault-backed refresh token at the stored token endpoint.
/// Errors contain only fixed strings, an allowlisted OAuth error code, or an
/// HTTP status code, never token material, the endpoint URL, or free-form
/// authorization-server response content.
pub(crate) fn refresh_access_token(
    oauth: &McpOauthMetadata,
    refresh_token: &SecretString,
    client_secret: Option<&SecretString>,
) -> Result<RefreshedToken, RefreshFailure> {
    refresh_target(
        &RefreshTarget {
            token_endpoint: &oauth.token_endpoint,
            client_id: &oauth.client_id,
            profile: RefreshProfile::Mcp {
                resource: &oauth.resource,
            },
        },
        refresh_token,
        client_secret,
    )
}

/// One refresh for a connector whose profile is `entra`, at the consent tenant's token endpoint
/// (built here from the GUID, never taken from a caller) with the definition's `scope` and no
/// `resource` and no client secret.
pub(crate) fn refresh_access_token_entra(
    tenant: &str,
    client_id: &str,
    scope: &str,
    refresh_token: &SecretString,
) -> Result<RefreshedToken, RefreshFailure> {
    let endpoint = entra_token_endpoint(tenant)
        .ok_or_else(|| RefreshFailure::needs_reauth("invalid OAuth token endpoint"))?;
    refresh_target(
        &RefreshTarget {
            token_endpoint: &endpoint,
            client_id,
            profile: RefreshProfile::Entra { scope },
        },
        refresh_token,
        None,
    )
}

fn refresh_target(
    target: &RefreshTarget<'_>,
    refresh_token: &SecretString,
    client_secret: Option<&SecretString>,
) -> Result<RefreshedToken, RefreshFailure> {
    let invalid = || RefreshFailure::needs_reauth("invalid OAuth token endpoint");
    // Test-node seam: the Entra token endpoint reaches the runner's loopback mock only.
    #[cfg(feature = "process-guest")]
    if let Some(mapped) = crate::process_mode::map_connector_url(target.token_endpoint) {
        let client = client_builder()
            .build()
            .map_err(|_| RefreshFailure::transient("failed to create OAuth HTTP client"))?;
        return exchange(&client, &mapped, target, refresh_token, client_secret);
    }
    let endpoint = url::Url::parse(target.token_endpoint).map_err(|_| invalid())?;
    if endpoint.scheme() != "https"
        || endpoint.host_str().is_none()
        || !endpoint.username().is_empty()
        || endpoint.password().is_some()
        || endpoint.fragment().is_some()
    {
        return Err(invalid());
    }
    let host = endpoint.host_str().ok_or_else(invalid)?;
    // An exact hostname allowlist (not a wildcard) plus a pinned DNS entry
    // keeps the token exchange on the validated public IP for this AS host.
    let pinned = PinnedUrl::validate_and_pin_typed(endpoint.as_str(), &[host.to_owned()])
        .map_err(classify_pin_error)?;
    let client = pinned
        .apply_to_client_builder(client_builder())
        .build()
        .map_err(|_| RefreshFailure::transient("failed to create OAuth HTTP client"))?;
    exchange(
        &client,
        &pinned.final_url,
        target,
        refresh_token,
        client_secret,
    )
}

/// Loopback-only test seam. HTTPS metadata is mapped to HTTP on loopback for
/// a local mock AS. The exchange, timeout, redirect and proxy rules remain
/// identical; only HTTPS/public-IP validation and DNS pinning differ.
#[cfg(test)]
pub(crate) fn refresh_access_token_unpinned_for_tests(
    oauth: &McpOauthMetadata,
    refresh_token: &SecretString,
    client_secret: Option<&SecretString>,
) -> Result<RefreshedToken, RefreshFailure> {
    refresh_target_unpinned_for_tests(
        &RefreshTarget {
            token_endpoint: &oauth.token_endpoint,
            client_id: &oauth.client_id,
            profile: RefreshProfile::Mcp {
                resource: &oauth.resource,
            },
        },
        refresh_token,
        client_secret,
    )
}

/// [`refresh_access_token_unpinned_for_tests`] for any profile (an Entra refresh against a
/// loopback mock, which the real tenant URL cannot be).
#[cfg(test)]
pub(crate) fn refresh_target_unpinned_for_tests(
    target: &RefreshTarget<'_>,
    refresh_token: &SecretString,
    client_secret: Option<&SecretString>,
) -> Result<RefreshedToken, RefreshFailure> {
    let invalid = || RefreshFailure::needs_reauth("invalid OAuth test token endpoint");
    let mut endpoint = url::Url::parse(target.token_endpoint).map_err(|_| invalid())?;
    if !matches!(endpoint.scheme(), "http" | "https")
        || !matches!(
            endpoint.host_str(),
            Some("127.0.0.1" | "localhost" | "[::1]")
        )
        || !endpoint.username().is_empty()
        || endpoint.password().is_some()
        || endpoint.fragment().is_some()
    {
        return Err(RefreshFailure::needs_reauth(
            "OAuth test endpoint must be loopback",
        ));
    }
    endpoint.set_scheme("http").map_err(|_| invalid())?;
    let client = client_builder()
        .build()
        .map_err(|_| RefreshFailure::transient("failed to create OAuth HTTP client"))?;
    exchange(&client, &endpoint, target, refresh_token, client_secret)
}

/// A policy rejection (restricted IP, allowlist miss) cannot heal without
/// reconnecting, so it needs reauth. A DNS resolution failure may be a
/// transient resolver outage and must not durably block the connection.
fn classify_pin_error(err: PinError) -> RefreshFailure {
    match err {
        PinError::Rejected(_) => RefreshFailure::needs_reauth("OAuth token endpoint rejected"),
        PinError::Resolution(_) => {
            RefreshFailure::transient("OAuth token endpoint could not be resolved")
        }
    }
}

fn client_builder() -> reqwest::blocking::ClientBuilder {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(10))
        .connect_timeout(Duration::from_secs(5))
        .redirect(reqwest::redirect::Policy::none())
        // Proxies resolve the destination themselves and would bypass the pin.
        .no_proxy()
}

#[derive(Deserialize)]
struct TokenErrorResponse {
    error: String,
}

/// Classify a non-2xx token endpoint response (RFC 6749 §5.2).
///
/// 5xx, 408 and 429 are transient. Any other 4xx means the AS refused this
/// grant or client, so a retry with the same refresh token cannot succeed.
/// Only allowlisted OAuth error codes are kept; the description is dropped.
fn classify_error_response(status: u16, body: &[u8]) -> RefreshFailure {
    if status >= 500 || status == 408 || status == 429 {
        return RefreshFailure::transient(format!(
            "OAuth token endpoint unavailable (HTTP {status})"
        ));
    }
    let code = serde_json::from_slice::<TokenErrorResponse>(body)
        .ok()
        .map(|parsed| parsed.error)
        .filter(|code| KNOWN_OAUTH_ERRORS.contains(&code.as_str()));
    match code {
        Some(code) => RefreshFailure::needs_reauth(code),
        None => RefreshFailure::needs_reauth(format!(
            "OAuth token endpoint rejected refresh (HTTP {status})"
        )),
    }
}

fn read_bounded(response: reqwest::blocking::Response) -> Result<Vec<u8>, RefreshFailure> {
    let mut body = Vec::new();
    response
        .take(MAX_RESPONSE_BYTES + 1)
        .read_to_end(&mut body)
        .map_err(|_| RefreshFailure::transient("failed to read OAuth token response"))?;
    if body.len() as u64 > MAX_RESPONSE_BYTES {
        return Err(RefreshFailure::transient(
            "OAuth token response is too large",
        ));
    }
    Ok(body)
}

fn exchange(
    client: &reqwest::blocking::Client,
    endpoint: &url::Url,
    target: &RefreshTarget<'_>,
    refresh_token: &SecretString,
    client_secret: Option<&SecretString>,
) -> Result<RefreshedToken, RefreshFailure> {
    validate_oauth_secret_value(refresh_token.expose_secret())
        .map_err(|_| RefreshFailure::needs_reauth("invalid OAuth refresh token"))?;
    if let Some(secret) = client_secret {
        validate_oauth_secret_value(secret.expose_secret())
            .map_err(|_| RefreshFailure::needs_reauth("invalid OAuth client secret"))?;
    }

    let form = refresh_form(
        target,
        refresh_token.expose_secret().as_str(),
        client_secret.map(|secret| secret.expose_secret().as_str()),
    );
    let response = client
        .post(endpoint.clone())
        .form(&form)
        .send()
        .map_err(|_| RefreshFailure::transient("OAuth token endpoint request failed"))?;
    let status = response.status();
    if !status.is_success() {
        // A body that cannot be read still carries a usable status code.
        let body = read_bounded(response).unwrap_or_default();
        return Err(classify_error_response(status.as_u16(), &body));
    }
    let body = read_bounded(response)?;
    let invalid = |reason: &str| RefreshFailure::transient(reason);
    let token: TokenResponse =
        serde_json::from_slice(&body).map_err(|_| invalid("invalid OAuth token response"))?;
    validate_oauth_secret_value(&token.access_token)
        .map_err(|_| invalid("invalid OAuth access token response"))?;
    if token
        .token_type
        .as_deref()
        .is_some_and(|t| !t.eq_ignore_ascii_case("Bearer"))
    {
        return Err(invalid("unsupported OAuth token type"));
    }
    if let Some(rotated) = token.refresh_token.as_deref() {
        validate_oauth_secret_value(rotated)
            .map_err(|_| invalid("invalid OAuth refresh token response"))?;
    }
    Ok(RefreshedToken {
        access_token: token.access_token,
        expires_in: token.expires_in,
        refresh_token: token.refresh_token,
    })
}

/// Outcome of one best-effort RFC 7009 revocation. `reason` is safe to log:
/// it never contains token material, the endpoint URL, or free-form
/// authorization-server response content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RevokeAttempt {
    pub(crate) attempted: bool,
    pub(crate) succeeded: bool,
    pub(crate) reason: String,
}

impl RevokeAttempt {
    fn skipped() -> Self {
        Self {
            attempted: false,
            succeeded: false,
            reason: String::new(),
        }
    }

    fn ok() -> Self {
        Self {
            attempted: true,
            succeeded: true,
            reason: String::new(),
        }
    }

    fn failed(reason: impl Into<String>) -> Self {
        Self {
            attempted: true,
            succeeded: false,
            reason: reason.into(),
        }
    }
}

/// Revoke the refresh token once when `revocationEndpoint` is known, then
/// delete every vault binding and the connection row.
///
/// Authorization-server errors are logged (sanitized) and do not fail the
/// caller. Callers that race tool-call refresh must hold the connection
/// credential lock across this call.
pub(crate) fn purge_disconnected_mcp_connection(
    vault: &SecretsVault,
    store: &SqliteWorkflowStore,
    conn: &McpConnection,
) -> Result<McpConnection, OrchestratorError> {
    let _attempt = best_effort_revoke_oauth_refresh(vault, conn);
    delete_mcp_connection_secrets(vault, conn)?;
    store.delete_mcp_connection(conn.scope, &conn.owner_user_id, &conn.connection_id)
}

/// POST the refresh token to `revocationEndpoint` at most once.
///
/// Missing endpoint, non-OAuth mode, or a missing refresh binding skips the
/// request. Failures are logged and returned as a sanitized [`RevokeAttempt`];
/// they are not errors for disconnect.
pub(crate) fn best_effort_revoke_oauth_refresh(
    vault: &SecretsVault,
    conn: &McpConnection,
) -> RevokeAttempt {
    let attempt = revoke_oauth_refresh_if_configured(vault, conn);
    if attempt.attempted && !attempt.succeeded {
        tracing::warn!(
            connection_id = %conn.connection_id,
            reason = %attempt.reason,
            "MCP OAuth refresh-token revocation failed; disconnect continues"
        );
    }
    attempt
}

fn revoke_oauth_refresh_if_configured(vault: &SecretsVault, conn: &McpConnection) -> RevokeAttempt {
    let Some(oauth) = conn
        .oauth
        .as_ref()
        .filter(|_| conn.auth_mode == McpAuthMode::Oauth)
    else {
        return RevokeAttempt::skipped();
    };
    if oauth
        .revocation_endpoint
        .as_deref()
        .map(str::trim)
        .filter(|endpoint| !endpoint.is_empty())
        .is_none()
    {
        return RevokeAttempt::skipped();
    }
    let refresh_id = mcp_oauth_refresh_token_binding_id(&conn.connection_id);
    let has_refresh = match vault.has_binding_at(conn.scope, &conn.owner_user_id, &refresh_id) {
        Ok(present) => present,
        Err(_) => return RevokeAttempt::failed("credential store unavailable"),
    };
    if !has_refresh {
        return RevokeAttempt::skipped();
    }
    let refresh_token = match vault.decrypt_secret_at(conn.scope, &conn.owner_user_id, &refresh_id)
    {
        Ok(token) => token,
        Err(_) => return RevokeAttempt::failed("credential store unavailable"),
    };
    let client_secret_id = mcp_oauth_client_secret_binding_id(&conn.connection_id);
    let client_secret =
        match vault.has_binding_at(conn.scope, &conn.owner_user_id, &client_secret_id) {
            Ok(true) => {
                match vault.decrypt_secret_at(conn.scope, &conn.owner_user_id, &client_secret_id) {
                    Ok(secret) => Some(secret),
                    Err(_) => return RevokeAttempt::failed("credential store unavailable"),
                }
            }
            Ok(false) => None,
            Err(_) => return RevokeAttempt::failed("credential store unavailable"),
        };
    dispatch_revoke(oauth, &refresh_token, client_secret.as_ref())
}

fn dispatch_revoke(
    oauth: &McpOauthMetadata,
    refresh_token: &SecretString,
    client_secret: Option<&SecretString>,
) -> RevokeAttempt {
    // Crate unit tests may point `revocationEndpoint` at a loopback mock.
    // Release builds and integration tests always use the pinned HTTPS client,
    // which rejects loopback before a request is sent.
    #[cfg(test)]
    if revocation_endpoint_is_loopback(oauth) {
        return revoke_refresh_token_unpinned_for_tests(oauth, refresh_token, client_secret);
    }
    revoke_refresh_token(oauth, refresh_token, client_secret)
}

#[cfg(test)]
fn revocation_endpoint_is_loopback(oauth: &McpOauthMetadata) -> bool {
    oauth
        .revocation_endpoint
        .as_deref()
        .and_then(|raw| url::Url::parse(raw.trim()).ok())
        .is_some_and(|endpoint| {
            matches!(
                endpoint.host_str(),
                Some("127.0.0.1" | "localhost" | "[::1]")
            )
        })
}

/// RFC 7009 revocation against a pinned public HTTPS endpoint. One request;
/// no retries. The result reason never includes the token or response body.
pub(crate) fn revoke_refresh_token(
    oauth: &McpOauthMetadata,
    refresh_token: &SecretString,
    client_secret: Option<&SecretString>,
) -> RevokeAttempt {
    let Some(raw) = oauth.revocation_endpoint.as_deref() else {
        return RevokeAttempt::skipped();
    };
    let endpoint = match parse_revocation_url(raw, true) {
        Ok(endpoint) => endpoint,
        Err(attempt) => return attempt,
    };
    let host = match endpoint.host_str() {
        Some(host) => host.to_owned(),
        None => return RevokeAttempt::failed("invalid OAuth revocation endpoint"),
    };
    let pinned = match PinnedUrl::validate_and_pin_typed(endpoint.as_str(), &[host]) {
        Ok(pinned) => pinned,
        Err(err) => return classify_revoke_pin_error(err),
    };
    let client = match pinned.apply_to_client_builder(client_builder()).build() {
        Ok(client) => client,
        Err(_) => return RevokeAttempt::failed("failed to create OAuth HTTP client"),
    };
    post_revocation(
        &client,
        &pinned.final_url,
        oauth,
        refresh_token,
        client_secret,
    )
}

/// Loopback-only test seam. HTTPS metadata is mapped to HTTP on loopback for
/// a local mock authorization server. One request; no retries.
#[cfg(test)]
pub(crate) fn revoke_refresh_token_unpinned_for_tests(
    oauth: &McpOauthMetadata,
    refresh_token: &SecretString,
    client_secret: Option<&SecretString>,
) -> RevokeAttempt {
    let Some(raw) = oauth.revocation_endpoint.as_deref() else {
        return RevokeAttempt::skipped();
    };
    let mut endpoint = match parse_revocation_url(raw, false) {
        Ok(endpoint) => endpoint,
        Err(attempt) => return attempt,
    };
    if !matches!(
        endpoint.host_str(),
        Some("127.0.0.1" | "localhost" | "[::1]")
    ) {
        return RevokeAttempt::failed("OAuth test revocation endpoint must be loopback");
    }
    if endpoint.set_scheme("http").is_err() {
        return RevokeAttempt::failed("invalid OAuth revocation endpoint");
    }
    let client = match client_builder().build() {
        Ok(client) => client,
        Err(_) => return RevokeAttempt::failed("failed to create OAuth HTTP client"),
    };
    post_revocation(&client, &endpoint, oauth, refresh_token, client_secret)
}

fn parse_revocation_url(raw: &str, require_https: bool) -> Result<url::Url, RevokeAttempt> {
    let endpoint = url::Url::parse(raw.trim())
        .map_err(|_| RevokeAttempt::failed("invalid OAuth revocation endpoint"))?;
    let scheme_ok = if require_https {
        endpoint.scheme() == "https"
    } else {
        matches!(endpoint.scheme(), "http" | "https")
    };
    if !scheme_ok
        || endpoint.host_str().is_none()
        || !endpoint.username().is_empty()
        || endpoint.password().is_some()
        || endpoint.fragment().is_some()
    {
        return Err(RevokeAttempt::failed("invalid OAuth revocation endpoint"));
    }
    Ok(endpoint)
}

fn classify_revoke_pin_error(err: PinError) -> RevokeAttempt {
    match err {
        PinError::Rejected(_) => RevokeAttempt::failed("OAuth revocation endpoint rejected"),
        PinError::Resolution(_) => {
            RevokeAttempt::failed("OAuth revocation endpoint could not be resolved")
        }
    }
}

fn post_revocation(
    client: &reqwest::blocking::Client,
    endpoint: &url::Url,
    oauth: &McpOauthMetadata,
    refresh_token: &SecretString,
    client_secret: Option<&SecretString>,
) -> RevokeAttempt {
    if validate_oauth_secret_value(refresh_token.expose_secret()).is_err() {
        return RevokeAttempt::failed("invalid OAuth refresh token");
    }
    if let Some(secret) = client_secret {
        if validate_oauth_secret_value(secret.expose_secret()).is_err() {
            return RevokeAttempt::failed("invalid OAuth client secret");
        }
    }
    let mut form = vec![
        ("token", refresh_token.expose_secret().as_str()),
        ("token_type_hint", "refresh_token"),
        ("client_id", oauth.client_id.as_str()),
    ];
    if let Some(secret) = client_secret {
        form.push(("client_secret", secret.expose_secret().as_str()));
    }
    let response = match client.post(endpoint.clone()).form(&form).send() {
        Ok(response) => response,
        Err(_) => return RevokeAttempt::failed("OAuth revocation endpoint request failed"),
    };
    let status = response.status();
    if status.is_success() {
        // RFC 7009: the client ignores the response body. Read it only so the
        // socket drains; never copy it into a log or error string.
        let _ = read_bounded(response);
        return RevokeAttempt::ok();
    }
    let code = status.as_u16();
    let body = read_bounded(response).unwrap_or_default();
    RevokeAttempt::failed(sanitize_revoke_error(code, &body))
}

/// Allowlisted OAuth `error` code, or a fixed string with the HTTP status.
/// The response body is not otherwise copied: descriptions can echo tokens.
fn sanitize_revoke_error(status: u16, body: &[u8]) -> String {
    if status >= 500 || status == 408 || status == 429 {
        return format!("OAuth revocation endpoint unavailable (HTTP {status})");
    }
    if let Some(code) = allowlisted_revoke_error(body) {
        return code.to_string();
    }
    format!("OAuth revocation endpoint rejected token (HTTP {status})")
}

fn allowlisted_revoke_error(body: &[u8]) -> Option<&'static str> {
    #[derive(Deserialize)]
    struct ErrBody {
        error: String,
    }
    let parsed = serde_json::from_slice::<ErrBody>(body).ok()?;
    KNOWN_REVOKE_ERRORS
        .iter()
        .copied()
        .find(|code| *code == parsed.error)
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    use super::*;
    use crate::mcp_registry::{McpOauthRegistrationMethod, McpOauthStatus};

    fn metadata(endpoint: String) -> McpOauthMetadata {
        McpOauthMetadata {
            resource: "https://mcp.example.com/service?tenant=a&mode=b".into(),
            authorization_server: "https://auth.example.com".into(),
            token_endpoint: endpoint,
            revocation_endpoint: None,
            client_id: "my client+id".into(),
            registration_method: McpOauthRegistrationMethod::Manual,
            scopes: vec![],
            expires_at: None,
            status: McpOauthStatus::Connected,
            last_refresh_at: None,
            last_refresh_error: None,
        }
    }

    // Reads one request and captures bytes only inside the test; no secret is
    // printed or logged by the HTTP implementation.
    fn mock_as(status: &str, payload: &str) -> (String, thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/oauth/token", listener.local_addr().unwrap());
        let status = status.to_owned();
        let payload = payload.to_owned();
        let worker = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            let body_start = loop {
                let mut chunk = [0u8; 4096];
                let n = stream.read(&mut chunk).unwrap();
                assert!(n > 0);
                request.extend_from_slice(&chunk[..n]);
                if let Some(pos) = request.windows(4).position(|b| b == b"\r\n\r\n") {
                    break pos + 4;
                }
            };
            let header = String::from_utf8_lossy(&request[..body_start]);
            let length: usize = header
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length: ")
                        .map(str::to_owned)
                })
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            while request.len() - body_start < length {
                let mut chunk = [0u8; 4096];
                let n = stream.read(&mut chunk).unwrap();
                assert!(n > 0);
                request.extend_from_slice(&chunk[..n]);
            }
            let reply = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                payload.len()
            );
            stream.write_all(reply.as_bytes()).unwrap();
            String::from_utf8(request).unwrap()
        });
        (url, worker)
    }

    #[test]
    fn sends_refresh_form_and_accepts_rotation() {
        let (url, worker) = mock_as(
            "200 OK",
            r#"{"access_token":"new-access","token_type":"bearer","expires_in":3600,"refresh_token":"new-refresh"}"#,
        );
        let token = refresh_access_token_unpinned_for_tests(
            &metadata(url),
            &SecretString::new("old refresh+&".into()),
            Some(&SecretString::new("client secret=&".into())),
        )
        .ok()
        .unwrap();
        assert_eq!(token.access_token, "new-access");
        assert_eq!(token.expires_in, Some(3600));
        assert_eq!(token.refresh_token.as_deref(), Some("new-refresh"));
        let request = worker.join().unwrap();
        assert!(request.starts_with("POST /oauth/token HTTP/1.1\r\n"));
        assert!(request
            .to_ascii_lowercase()
            .contains("content-type: application/x-www-form-urlencoded"));
        let form = request.split_once("\r\n\r\n").unwrap().1;
        let entries: std::collections::HashMap<_, _> = url::form_urlencoded::parse(form.as_bytes())
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        assert_eq!(entries.len(), 5);
        assert_eq!(entries["grant_type"], "refresh_token");
        assert_eq!(entries["refresh_token"], "old refresh+&");
        assert_eq!(entries["client_id"], "my client+id");
        assert_eq!(
            entries["resource"],
            "https://mcp.example.com/service?tenant=a&mode=b"
        );
        assert_eq!(entries["client_secret"], "client secret=&");
    }

    fn parsed(request: &str) -> std::collections::HashMap<String, String> {
        form_entries(request)
    }

    #[test]
    fn the_mcp_form_still_sends_resource_and_the_entra_form_never_does() {
        let mcp = RefreshTarget {
            token_endpoint: "https://as.example/token",
            client_id: "c",
            profile: RefreshProfile::Mcp {
                resource: "https://mcp.example/mcp",
            },
        };
        let form: std::collections::HashMap<_, _> =
            refresh_form(&mcp, "rt", Some("cs")).into_iter().collect();
        assert_eq!(form["resource"], "https://mcp.example/mcp");
        assert_eq!(form["client_secret"], "cs");
        assert!(!form.contains_key("scope"));

        let entra = RefreshTarget {
            token_endpoint: "https://login.microsoftonline.com/t/oauth2/v2.0/token",
            client_id: "c",
            profile: RefreshProfile::Entra {
                scope: "openid offline_access Mail.ReadWrite",
            },
        };
        // Even handed a secret, an Entra request has none and has no `resource`.
        let form: std::collections::HashMap<_, _> =
            refresh_form(&entra, "rt", Some("cs")).into_iter().collect();
        assert!(!form.contains_key("resource"));
        assert!(!form.contains_key("client_secret"));
        assert_eq!(form["scope"], "openid offline_access Mail.ReadWrite");
        assert_eq!(form["grant_type"], "refresh_token");
        assert_eq!(form["refresh_token"], "rt");
        assert_eq!(form["client_id"], "c");
        assert_eq!(form.len(), 4);
    }

    #[test]
    fn an_entra_refresh_on_the_wire_has_no_resource_and_no_secret() {
        let (url, worker) = mock_as(
            "200 OK",
            r#"{"access_token":"new-access","token_type":"Bearer","expires_in":3599,"refresh_token":"new-refresh"}"#,
        );
        let token = refresh_target_unpinned_for_tests(
            &RefreshTarget {
                token_endpoint: &url,
                client_id: "e2e-client",
                profile: RefreshProfile::Entra {
                    scope: "openid offline_access User.Read",
                },
            },
            &SecretString::new("old refresh+&".into()),
            None,
        )
        .ok()
        .unwrap();
        assert_eq!(token.access_token, "new-access");
        assert_eq!(token.expires_in, Some(3599));
        let request = worker.join().unwrap();
        let entries = parsed(&request);
        assert_eq!(entries["grant_type"], "refresh_token");
        assert_eq!(entries["refresh_token"], "old refresh+&");
        assert_eq!(entries["client_id"], "e2e-client");
        assert_eq!(entries["scope"], "openid offline_access User.Read");
        assert!(!request.contains("resource") && !request.contains("client_secret"));
    }

    #[test]
    fn a_revoked_entra_grant_needs_reauth_and_leaks_nothing() {
        let (url, worker) = mock_as(
            "400 Bad Request",
            r#"{"error":"invalid_grant","error_description":"AADSTS70008 SECRET-DESCRIPTION"}"#,
        );
        let err = refresh_target_unpinned_for_tests(
            &RefreshTarget {
                token_endpoint: &url,
                client_id: "c",
                profile: RefreshProfile::Entra { scope: "openid" },
            },
            &SecretString::new("SECRET-REFRESH".into()),
            None,
        )
        .err()
        .unwrap();
        worker.join().unwrap();
        assert_eq!(err, RefreshFailure::needs_reauth("invalid_grant"));
        assert!(!err.reason.contains("SECRET"));
    }

    #[test]
    fn the_entra_endpoint_is_built_from_a_tenant_guid_only() {
        let t = "11111111-1111-4111-8111-111111111111";
        assert_eq!(
            entra_token_endpoint(t).unwrap(),
            format!("https://login.microsoftonline.com/{t}/oauth2/v2.0/token")
        );
        for bad in [
            "",
            "common",
            "organizations",
            "contoso.onmicrosoft.com",
            "11111111-1111-4111-8111-11111111111G",
            "11111111-1111-4111-8111-111111111111/../x",
            "evil.example/11111111-1111-4111-8111-111111111111",
            "11111111-1111-4111-8111-111111111111?x=1",
            "11111111-1111-4111-8111-11111111111A",
        ] {
            assert_eq!(entra_token_endpoint(bad), None, "{bad:?}");
        }
        // The public entry point refuses a non-GUID before any request.
        let err =
            refresh_access_token_entra("common", "c", "openid", &SecretString::new("rt".into()))
                .err()
                .unwrap();
        assert_eq!(err.kind, RefreshFailureKind::NeedsReauth);
    }

    #[test]
    fn public_client_omits_secret_and_optional_fields() {
        let (url, worker) = mock_as("200 OK", r#"{"access_token":"new-access"}"#);
        // Metadata can stay valid HTTPS even though the mock AS speaks HTTP.
        let https_url = url.replacen("http://", "https://", 1);
        let token = refresh_access_token_unpinned_for_tests(
            &metadata(https_url),
            &SecretString::new("old-refresh".into()),
            None,
        )
        .ok()
        .unwrap();
        assert!(token.expires_in.is_none());
        assert!(token.refresh_token.is_none());
        let request = worker.join().unwrap();
        assert!(!request.contains("client_secret"));
    }

    #[test]
    fn invalid_grant_needs_reauth_without_leaking_description() {
        let (url, worker) = mock_as(
            "400 Bad Request",
            r#"{"error":"invalid_grant", "error_description":"SECRET-AS-CONTENT"}"#,
        );
        let err = refresh_access_token_unpinned_for_tests(
            &metadata(url),
            &SecretString::new("SECRET-REFRESH".into()),
            Some(&SecretString::new("SECRET-CLIENT".into())),
        )
        .err()
        .unwrap();
        worker.join().unwrap();
        assert_eq!(err, RefreshFailure::needs_reauth("invalid_grant"));
        for secret in ["SECRET-AS-CONTENT", "SECRET-REFRESH", "SECRET-CLIENT"] {
            assert!(!err.reason.contains(secret));
        }
    }

    #[test]
    fn classifies_token_endpoint_error_responses() {
        for (status, payload, expected) in [
            (
                "401 Unauthorized",
                r#"{"error":"invalid_client"}"#,
                RefreshFailure::needs_reauth("invalid_client"),
            ),
            (
                "400 Bad Request",
                r#"{"error":"SECRET-AS-CODE"}"#,
                RefreshFailure::needs_reauth("OAuth token endpoint rejected refresh (HTTP 400)"),
            ),
            (
                "403 Forbidden",
                "SECRET-AS-CONTENT",
                RefreshFailure::needs_reauth("OAuth token endpoint rejected refresh (HTTP 403)"),
            ),
            (
                "503 Service Unavailable",
                r#"{"error":"invalid_grant"}"#,
                RefreshFailure::transient("OAuth token endpoint unavailable (HTTP 503)"),
            ),
            (
                "429 Too Many Requests",
                "{}",
                RefreshFailure::transient("OAuth token endpoint unavailable (HTTP 429)"),
            ),
        ] {
            let (url, worker) = mock_as(status, payload);
            let err = refresh_access_token_unpinned_for_tests(
                &metadata(url),
                &SecretString::new("SECRET-REFRESH".into()),
                None,
            )
            .err()
            .unwrap();
            worker.join().unwrap();
            assert_eq!(err, expected, "{status}");
            assert!(!err.reason.contains("SECRET"));
        }
    }

    #[test]
    fn network_failure_is_transient() {
        // Bind then drop: the port is closed, so the connect is refused.
        let addr = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let err = refresh_access_token_unpinned_for_tests(
            &metadata(format!("http://{addr}/oauth/token")),
            &SecretString::new("SECRET-REFRESH".into()),
            None,
        )
        .err()
        .unwrap();
        assert_eq!(
            err,
            RefreshFailure::transient("OAuth token endpoint request failed")
        );
    }

    #[test]
    fn rejects_invalid_json_and_invalid_secret_bearing_fields() {
        for (payload, expected) in [
            ("SECRET-AS-CONTENT", "invalid OAuth token response"),
            (
                r#"{"access_token":""}"#,
                "invalid OAuth access token response",
            ),
            (
                r#"{"access_token":"a\nb"}"#,
                "invalid OAuth access token response",
            ),
            (
                r#"{"access_token":"good","refresh_token":""}"#,
                "invalid OAuth refresh token response",
            ),
            (
                r#"{"access_token":"good","token_type":"mac"}"#,
                "unsupported OAuth token type",
            ),
        ] {
            let (url, worker) = mock_as("200 OK", payload);
            let err = refresh_access_token_unpinned_for_tests(
                &metadata(url),
                &SecretString::new("SECRET-REFRESH".into()),
                None,
            )
            .err()
            .unwrap();
            worker.join().unwrap();
            assert_eq!(err, RefreshFailure::transient(expected));
        }
    }

    #[test]
    fn production_rejects_loopback_and_http_before_sending() {
        let oauth = metadata("http://127.0.0.1:12345/token".into());
        let err = refresh_access_token(&oauth, &SecretString::new("refresh".into()), None)
            .err()
            .unwrap();
        assert_eq!(
            err,
            RefreshFailure::needs_reauth("invalid OAuth token endpoint")
        );
        let oauth = metadata("https://127.0.0.1:12345/token".into());
        let err = refresh_access_token(&oauth, &SecretString::new("refresh".into()), None)
            .err()
            .unwrap();
        assert_eq!(
            err,
            RefreshFailure::needs_reauth("OAuth token endpoint rejected")
        );
    }

    #[test]
    fn pin_errors_split_policy_rejection_from_dns_failure() {
        assert_eq!(
            classify_pin_error(PinError::Rejected("restricted".into())),
            RefreshFailure::needs_reauth("OAuth token endpoint rejected")
        );
        assert_eq!(
            classify_pin_error(PinError::Resolution("lookup failed".into())),
            RefreshFailure::transient("OAuth token endpoint could not be resolved")
        );
    }

    #[test]
    fn unresolvable_token_endpoint_is_transient() {
        // RFC 6761: `.invalid` never resolves, standing in for a DNS outage.
        let oauth = metadata("https://auth.vam-test.invalid/token".into());
        let err = refresh_access_token(&oauth, &SecretString::new("refresh".into()), None)
            .err()
            .unwrap();
        assert_eq!(
            err,
            RefreshFailure::transient("OAuth token endpoint could not be resolved")
        );
    }

    #[test]
    fn failure_reasons_are_valid_last_refresh_errors() {
        use crate::mcp_registry::validate_oauth_metadata;
        let mut meta = metadata("https://auth.example.com/token".into());
        for reason in KNOWN_OAUTH_ERRORS.iter().copied().chain([
            "invalid OAuth refresh token",
            "invalid OAuth refresh token response",
            "OAuth token endpoint rejected refresh (HTTP 400)",
            "OAuth token endpoint rejected",
            "OAuth token endpoint could not be resolved",
        ]) {
            meta.last_refresh_error = Some(reason.into());
            validate_oauth_metadata(&meta).unwrap();
        }
    }

    fn revocation_metadata(endpoint: String) -> McpOauthMetadata {
        let mut oauth = metadata("https://auth.example.com/oauth/token".into());
        oauth.revocation_endpoint = Some(endpoint);
        oauth.client_id = "revoke-client".into();
        oauth
    }

    /// One RFC 7009 request, then a short wait for a retry on the same socket
    /// or a second connection.
    fn mock_revoke(status: &str, payload: &str) -> (String, thread::JoinHandle<(String, bool)>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("https://{}/oauth/revoke", listener.local_addr().unwrap());
        let status = status.to_owned();
        let payload = payload.to_owned();
        let worker = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            let body_start = loop {
                let mut chunk = [0u8; 4096];
                let n = stream.read(&mut chunk).unwrap();
                assert!(n > 0);
                request.extend_from_slice(&chunk[..n]);
                if let Some(pos) = request.windows(4).position(|b| b == b"\r\n\r\n") {
                    break pos + 4;
                }
            };
            let header = String::from_utf8_lossy(&request[..body_start]);
            let length: usize = header
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length: ")
                        .map(str::to_owned)
                })
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            while request.len() - body_start < length {
                let mut chunk = [0u8; 4096];
                let n = stream.read(&mut chunk).unwrap();
                assert!(n > 0);
                request.extend_from_slice(&chunk[..n]);
            }
            let reply = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                payload.len()
            );
            stream.write_all(reply.as_bytes()).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_millis(200)))
                .unwrap();
            let mut extra = [0u8; 1];
            let mut retried = matches!(stream.read(&mut extra), Ok(n) if n > 0);
            drop(stream);
            listener.set_nonblocking(true).unwrap();
            if listener.accept().is_ok() {
                retried = true;
            }
            (String::from_utf8(request).unwrap(), retried)
        });
        (url, worker)
    }

    fn form_entries(request: &str) -> std::collections::HashMap<String, String> {
        let form = request.split_once("\r\n\r\n").unwrap().1;
        url::form_urlencoded::parse(form.as_bytes())
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect()
    }

    #[test]
    fn revoke_posts_refresh_token_once_with_client_secret() {
        let refresh = "revoke-refresh+&";
        let secret = "revoke-secret=&";
        let (url, worker) = mock_revoke("200 OK", "");
        let attempt = revoke_refresh_token_unpinned_for_tests(
            &revocation_metadata(url),
            &SecretString::new(refresh.into()),
            Some(&SecretString::new(secret.into())),
        );
        assert!(attempt.succeeded, "{}", attempt.reason);
        assert!(attempt.reason.is_empty());
        let (request, retried) = worker.join().unwrap();
        assert!(!retried);
        assert!(request.starts_with("POST /oauth/revoke HTTP/1.1\r\n"));
        assert!(request
            .to_ascii_lowercase()
            .contains("content-type: application/x-www-form-urlencoded"));
        let entries = form_entries(&request);
        assert_eq!(entries.len(), 4);
        assert_eq!(entries["token"], refresh);
        assert_eq!(entries["token_type_hint"], "refresh_token");
        assert_eq!(entries["client_id"], "revoke-client");
        assert_eq!(entries["client_secret"], secret);
    }

    #[test]
    fn public_client_revoke_omits_client_secret() {
        let (url, worker) = mock_revoke("200 OK", "");
        let attempt = revoke_refresh_token_unpinned_for_tests(
            &revocation_metadata(url),
            &SecretString::new("public-refresh".into()),
            None,
        );
        assert!(attempt.succeeded, "{}", attempt.reason);
        let (request, retried) = worker.join().unwrap();
        assert!(!retried);
        let entries = form_entries(&request);
        assert_eq!(entries.len(), 3);
        assert!(!entries.contains_key("client_secret"));
        assert_eq!(entries["token_type_hint"], "refresh_token");
    }

    #[test]
    fn revoke_error_body_is_sanitized_and_not_retried() {
        let refresh = "super-secret-refresh-token-value";
        let body =
            format!(r#"{{"error":"invalid_request","error_description":"leaked {refresh}"}}"#);
        let (url, worker) = mock_revoke("400 Bad Request", &body);
        let attempt = revoke_refresh_token_unpinned_for_tests(
            &revocation_metadata(url),
            &SecretString::new(refresh.into()),
            None,
        );
        assert!(attempt.attempted);
        assert!(!attempt.succeeded);
        assert_eq!(attempt.reason, "invalid_request");
        assert!(!attempt.reason.contains(refresh));
        let (_request, retried) = worker.join().unwrap();
        assert!(!retried);

        let noisy = format!("plain {refresh} in the body");
        let reason = sanitize_revoke_error(400, noisy.as_bytes());
        assert_eq!(
            reason,
            "OAuth revocation endpoint rejected token (HTTP 400)"
        );
        assert!(!reason.contains(refresh));
        let unavailable = sanitize_revoke_error(503, refresh.as_bytes());
        assert_eq!(
            unavailable,
            "OAuth revocation endpoint unavailable (HTTP 503)"
        );
        assert!(!unavailable.contains(refresh));
    }

    #[test]
    fn production_revoke_rejects_loopback_without_sending_the_token() {
        let refresh = "must-not-appear";
        let oauth = revocation_metadata("https://127.0.0.1:9/oauth/revoke".into());
        let attempt = revoke_refresh_token(&oauth, &SecretString::new(refresh.into()), None);
        assert!(attempt.attempted);
        assert!(!attempt.succeeded);
        assert_eq!(attempt.reason, "OAuth revocation endpoint rejected");
        assert!(!attempt.reason.contains(refresh));
        let http = revocation_metadata("http://auth.example.com/oauth/revoke".into());
        let attempt = revoke_refresh_token(&http, &SecretString::new(refresh.into()), None);
        assert_eq!(attempt.reason, "invalid OAuth revocation endpoint");
        assert!(!attempt.reason.contains(refresh));
    }

    struct LogCapture(std::sync::Arc<std::sync::Mutex<String>>);

    impl tracing::Subscriber for LogCapture {
        fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
        fn event(&self, event: &tracing::Event<'_>) {
            event.record(&mut LogVisitor(self.0.clone()));
        }
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}
    }

    struct LogVisitor(std::sync::Arc<std::sync::Mutex<String>>);

    impl tracing::field::Visit for LogVisitor {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            let mut buf = self.0.lock().expect("log buffer");
            buf.push_str(field.name());
            buf.push('=');
            buf.push_str(&format!("{value:?}"));
            buf.push('\n');
        }
    }

    #[test]
    fn disconnect_purge_revokes_once_deletes_bindings_and_omits_tokens_from_logs() {
        use std::sync::{Arc, Mutex};

        use crate::mcp_registry::{store_mcp_oauth_tokens, McpOauthTokenWrite};
        use crate::secrets::SecretScope;
        use crate::users::UserRole;

        let refresh = "purge-refresh-token-plaintext";
        let access = "purge-access-token-plaintext";
        let secret = "purge-client-secret-plaintext";
        let body = format!(r#"{{"error":"unauthorized_client","error_description":"{refresh}"}}"#);
        let (url, worker) = mock_revoke("401 Unauthorized", &body);
        let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
        let vault = SecretsVault::for_tests(store.clone());
        let oauth = revocation_metadata(url);
        let conn = store
            .upsert_mcp_connection(
                SecretScope::User,
                "alice",
                "acme",
                "https://mcp.example.com/mcp",
                "",
                &[],
                &[],
                McpAuthMode::Oauth,
                Some(&oauth),
            )
            .unwrap();
        store_mcp_oauth_tokens(
            &vault,
            &conn,
            &McpOauthTokenWrite {
                access_token: access.into(),
                refresh_token: refresh.into(),
                client_secret: Some(secret.into()),
            },
            None,
            UserRole::Member,
        )
        .unwrap();
        let logs = Arc::new(Mutex::new(String::new()));
        let logs_for_sub = logs.clone();
        let removed = tracing::subscriber::with_default(LogCapture(logs_for_sub), || {
            purge_disconnected_mcp_connection(&vault, &store, &conn).unwrap()
        });
        assert_eq!(removed.connection_id, "acme");
        let (request, retried) = worker.join().unwrap();
        assert!(!retried);
        let entries = form_entries(&request);
        assert_eq!(entries["token"], refresh);
        assert_eq!(entries["client_secret"], secret);
        assert_eq!(entries["token_type_hint"], "refresh_token");
        for binding in [
            crate::mcp_registry::mcp_oauth_access_token_binding_id("acme"),
            crate::mcp_registry::mcp_oauth_refresh_token_binding_id("acme"),
            crate::mcp_registry::mcp_oauth_client_secret_binding_id("acme"),
        ] {
            assert!(
                vault
                    .get_metadata_at(SecretScope::User, "alice", &binding)
                    .is_err(),
                "{binding}"
            );
        }
        assert!(matches!(
            store.get_mcp_connection(SecretScope::User, "alice", "acme"),
            Err(OrchestratorError::NotFound(_))
        ));
        let logged = logs.lock().expect("log buffer").clone();
        assert!(
            logged.contains("unauthorized_client"),
            "expected sanitized revoke error in logs: {logged}"
        );
        assert!(!logged.contains(refresh), "{logged}");
        assert!(!logged.contains(access), "{logged}");
        assert!(!logged.contains(secret), "{logged}");
        assert!(!logged.contains("error_description"), "{logged}");
    }

    #[test]
    fn disconnect_without_revocation_endpoint_still_deletes_oauth_bindings() {
        use crate::mcp_registry::{store_mcp_oauth_tokens, McpOauthTokenWrite};
        use crate::secrets::SecretScope;
        use crate::users::UserRole;
        use std::sync::Arc;

        let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
        let vault = SecretsVault::for_tests(store.clone());
        let mut oauth = revocation_metadata("https://auth.example.com/oauth/revoke".into());
        oauth.revocation_endpoint = None;
        let conn = store
            .upsert_mcp_connection(
                SecretScope::User,
                "alice",
                "acme",
                "https://mcp.example.com/mcp",
                "",
                &[],
                &[],
                McpAuthMode::Oauth,
                Some(&oauth),
            )
            .unwrap();
        store_mcp_oauth_tokens(
            &vault,
            &conn,
            &McpOauthTokenWrite {
                access_token: "access".into(),
                refresh_token: "refresh".into(),
                client_secret: None,
            },
            None,
            UserRole::Member,
        )
        .unwrap();
        let attempt = best_effort_revoke_oauth_refresh(&vault, &conn);
        assert!(!attempt.attempted);
        purge_disconnected_mcp_connection(&vault, &store, &conn).unwrap();
        assert!(vault
            .get_metadata_at(
                SecretScope::User,
                "alice",
                &crate::mcp_registry::mcp_oauth_refresh_token_binding_id("acme")
            )
            .is_err());
        assert!(matches!(
            store.get_mcp_connection(SecretScope::User, "alice", "acme"),
            Err(OrchestratorError::NotFound(_))
        ));
    }
}
