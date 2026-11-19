//! Host-side OAuth token endpoint and RFC 7009 revocation for remote MCP.
//!
//! Authorization-server requests are sent over HTTPS to a DNS-pinned public IP,
//! with redirects and environment proxies disabled. Errors are deliberately
//! generic: neither a request URL, token value, nor authorization-server
//! response body is safe to log or persist.

use std::io::Read;
use std::time::Duration;
use vam_security::tls_roots::BundledRoots;

use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;

use crate::connector_client::{org_client_secret, RowClient, TokenAuthMethod};
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
    /// A REST connector with fixed endpoints and an admin-supplied public client (`oauth`
    /// profile, CN-6.3): no `resource`, no `client_secret` (it has none), and the definition's
    /// `scope` only when the file lists scopes.
    Oauth { scope: Option<&'a str> },
}

/// Where a refresh goes and how it is shaped.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RefreshTarget<'a> {
    pub(crate) token_endpoint: &'a str,
    pub(crate) client_id: &'a str,
    pub(crate) profile: RefreshProfile<'a>,
    /// How a client secret authenticates (only read when one is sent).
    pub(crate) auth_method: TokenAuthMethod,
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
            if let (Some(secret), TokenAuthMethod::ClientSecretPost) =
                (client_secret, target.auth_method)
            {
                // client_secret_post: registered confidential clients may provide a
                // client secret; public clients omit this parameter entirely. With
                // client_secret_basic the secret travels in the Authorization header instead.
                form.push(("client_secret", secret));
            }
        }
        // No `resource`, and a public client has no secret to send.
        RefreshProfile::Entra { scope } => form.push(("scope", scope)),
        // Same for a fixed-endpoint public client: never `resource`, never a secret.
        RefreshProfile::Oauth { scope } => {
            if let Some(scope) = scope {
                form.push(("scope", scope));
            }
        }
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
    auth_method: TokenAuthMethod,
) -> Result<RefreshedToken, RefreshFailure> {
    refresh_target(
        &RefreshTarget {
            token_endpoint: &oauth.token_endpoint,
            client_id: &oauth.client_id,
            profile: RefreshProfile::Mcp {
                resource: &oauth.resource,
            },
            auth_method,
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
            auth_method: TokenAuthMethod::default(),
        },
        refresh_token,
        None,
    )
}

/// One refresh for a connector whose profile is `oauth`, at the token endpoint the node pinned at
/// Enable (the caller passes the node client's endpoint, never a member row's or a request's) with
/// the admin's client id: no `resource`, no client secret, `scope` only when the file lists scopes.
pub(crate) fn refresh_access_token_oauth(
    token_endpoint: &str,
    client_id: &str,
    scope: Option<&str>,
    refresh_token: &SecretString,
) -> Result<RefreshedToken, RefreshFailure> {
    refresh_target(
        &RefreshTarget {
            token_endpoint,
            client_id,
            profile: RefreshProfile::Oauth { scope },
            auth_method: TokenAuthMethod::default(),
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
    let (client, endpoint) = pinned_token_client(target.token_endpoint)?;
    exchange(&client, &endpoint, target, refresh_token, client_secret)
}

/// The loopback stand-in for an authorization-server URL: the test-node seam of a process-mode
/// `vamd` (the `oauth:<origin>` and Entra keys) and, in unit tests, [`test_seam`]. `None` on every
/// other node, which then keeps the public-address rules.
fn mapped_loopback(url: &str) -> Option<url::Url> {
    #[cfg(test)]
    if let Some(mapped) = test_seam::map(url) {
        return Some(mapped);
    }
    #[cfg(feature = "process-guest")]
    if let Some(mapped) = crate::process_mode::map_connector_url(url) {
        return Some(mapped);
    }
    let _ = url;
    None
}

/// The HTTP client and URL for one request to `token_endpoint`: HTTPS only, the host's DNS answer
/// pinned to a public address, no redirects, no proxy. A request to this client reaches the host the
/// endpoint names and no other: nothing the authorization server answers moves it.
fn pinned_token_client(
    token_endpoint: &str,
) -> Result<(reqwest::blocking::Client, url::Url), RefreshFailure> {
    let invalid = || RefreshFailure::needs_reauth("invalid OAuth token endpoint");
    if let Some(mapped) = mapped_loopback(token_endpoint) {
        let client = client_builder()
            .build()
            .map_err(|_| RefreshFailure::transient("failed to create OAuth HTTP client"))?;
        return Ok((client, mapped));
    }
    let endpoint = url::Url::parse(token_endpoint).map_err(|_| invalid())?;
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
    Ok((client, pinned.final_url))
}

/// Unit-test stand-ins for authorization-server hosts, by exact host, so a route test reaches a
/// loopback server while the code under test still sees the `https://` endpoint the node pinned.
/// Tests use hosts of their own so they never share an entry.
#[cfg(test)]
pub(crate) mod test_seam {
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    static HOSTS: Mutex<BTreeMap<String, url::Url>> = Mutex::new(BTreeMap::new());

    /// `host` (no port) is answered by `http://127.0.0.1:<port>`; the path and query are kept.
    pub(crate) fn serve(host: &str, loopback: url::Url) {
        HOSTS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(host.to_string(), loopback);
    }

    pub(crate) fn map(raw: &str) -> Option<url::Url> {
        let u = url::Url::parse(raw).ok()?;
        let base = HOSTS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(u.host_str()?)
            .cloned()?;
        let mut out = base;
        out.set_path(u.path());
        out.set_query(u.query());
        Some(out)
    }
}

/// Loopback-only test seam. HTTPS metadata is mapped to HTTP on loopback for
/// a local mock AS. The exchange, timeout, redirect and proxy rules remain
/// identical; only HTTPS/public-IP validation and DNS pinning differ.
#[cfg(test)]
pub(crate) fn refresh_access_token_unpinned_for_tests(
    oauth: &McpOauthMetadata,
    refresh_token: &SecretString,
    client_secret: Option<&SecretString>,
    auth_method: TokenAuthMethod,
) -> Result<RefreshedToken, RefreshFailure> {
    refresh_target_unpinned_for_tests(
        &RefreshTarget {
            token_endpoint: &oauth.token_endpoint,
            client_id: &oauth.client_id,
            profile: RefreshProfile::Mcp {
                resource: &oauth.resource,
            },
            auth_method,
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
        .bundled_roots()
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

    let secret = client_secret.map(|secret| secret.expose_secret().as_str());
    let form = refresh_form(target, refresh_token.expose_secret().as_str(), secret);
    let basic = match (secret, target.auth_method) {
        (Some(secret), TokenAuthMethod::ClientSecretBasic) => Some((target.client_id, secret)),
        _ => None,
    };
    post_token_form(client, endpoint, &form, basic)
}

/// `client_secret_basic` (RFC 6749 §2.3.1): the client id and the secret, each
/// `application/x-www-form-urlencoded`, are the user name and the password of HTTP Basic.
fn with_basic_client_auth(
    request: reqwest::blocking::RequestBuilder,
    credentials: Option<(&str, &str)>,
) -> reqwest::blocking::RequestBuilder {
    let Some((client_id, secret)) = credentials else {
        return request;
    };
    let encode = |value: &str| -> String {
        url::form_urlencoded::byte_serialize(value.as_bytes()).collect()
    };
    request.basic_auth(encode(client_id), Some(encode(secret)))
}

/// One POST of `form` to a token endpoint and the parsed answer. The failure carries a fixed or
/// allowlisted reason, never the response body.
fn post_token_form(
    client: &reqwest::blocking::Client,
    endpoint: &url::Url,
    form: &[(&str, &str)],
    basic: Option<(&str, &str)>,
) -> Result<RefreshedToken, RefreshFailure> {
    let response = with_basic_client_auth(client.post(endpoint.clone()).form(form), basic)
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

/// How a code exchange is shaped, as [`RefreshProfile`] is for a refresh: a type, so no code path can
/// add a `resource` to the request of an `oauth` connector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CodeProfile<'a> {
    /// An MCP connector: the connector's MCP URL is the RFC 8707 `resource`; the client secret is sent
    /// when the org keeps one.
    Mcp { resource: &'a str },
    /// A REST connector with fixed endpoints (`oauth` profile, CN-6.3): a public client. No
    /// `resource`, no client secret, no Authorization header.
    Oauth,
}

/// One authorization-code exchange at a connector's pinned token endpoint (CN-5.3, CN-6.3, D29).
pub(crate) struct CodeGrant<'a> {
    /// The endpoint the node pinned at Enable. Never a request's value.
    pub(crate) token_endpoint: &'a str,
    pub(crate) client_id: &'a str,
    pub(crate) profile: CodeProfile<'a>,
    pub(crate) code: &'a str,
    pub(crate) code_verifier: &'a str,
    pub(crate) redirect_uri: &'a str,
    /// The method pinned at Enable for the org's secret.
    pub(crate) auth_method: TokenAuthMethod,
}

/// The form of a code exchange. Pure, so the shape is unit-testable without a server.
///
/// `client_secret` is only ever used for [`CodeProfile::Mcp`]: an `oauth` connector's client is
/// public, so whatever a caller passes, its form has no secret and no `resource`.
fn code_form<'a>(grant: &CodeGrant<'a>, client_secret: Option<&'a str>) -> Vec<(&'a str, &'a str)> {
    let mut form = vec![
        ("grant_type", "authorization_code"),
        ("code", grant.code),
        ("client_id", grant.client_id),
    ];
    // A confidential org client: the secret is in the body for client_secret_post, and in the
    // Authorization header (not here) for client_secret_basic. A client with no secret sends none.
    if let (CodeProfile::Mcp { .. }, Some(secret), TokenAuthMethod::ClientSecretPost) =
        (grant.profile, client_secret, grant.auth_method)
    {
        form.push(("client_secret", secret));
    }
    form.extend([
        ("code_verifier", grant.code_verifier),
        ("redirect_uri", grant.redirect_uri),
    ]);
    if let CodeProfile::Mcp { resource } = grant.profile {
        form.push(("resource", resource));
    }
    form
}

/// Exchange an authorization code at the pinned token endpoint, with the org's client secret when
/// the org keeps one (`None`: a public client, nothing secret is sent). The secret leaves the node
/// only here, in the body of one POST to the pinned public address of `grant.token_endpoint` (HTTPS,
/// the host's DNS answer pinned, no redirects, no proxy). The caller has already checked that this
/// host is the one the secret is pinned to. A failure is a fixed or allowlisted reason
/// (`invalid_client`, `invalid_grant`, ...): never the endpoint, the code, the verifier or a token.
pub(crate) fn exchange_authorization_code(
    grant: &CodeGrant<'_>,
    client_secret: Option<&SecretString>,
) -> Result<RefreshedToken, RefreshFailure> {
    // An `oauth` connector has no secret to send: it is dropped here, not trusted to the caller.
    let client_secret = client_secret.filter(|_| matches!(grant.profile, CodeProfile::Mcp { .. }));
    if let Some(secret) = client_secret {
        validate_oauth_secret_value(secret.expose_secret())
            .map_err(|_| RefreshFailure::needs_reauth("invalid OAuth client secret"))?;
    }
    for value in [grant.code, grant.code_verifier] {
        validate_oauth_secret_value(value)
            .map_err(|_| RefreshFailure::needs_reauth("invalid OAuth authorization code"))?;
    }
    let (client, endpoint) = pinned_token_client(grant.token_endpoint)?;
    let secret = client_secret.map(|s| s.expose_secret().as_str());
    let form = code_form(grant, secret);
    let basic = match (secret, grant.auth_method) {
        (Some(secret), TokenAuthMethod::ClientSecretBasic) => Some((grant.client_id, secret)),
        _ => None,
    };
    post_token_form(&client, &endpoint, &form, basic)
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
    match prepare_oauth_refresh_revoke(vault, conn) {
        Ok(prepared) => prepared.send(),
        Err(attempt) => attempt,
    }
}

/// What revoking a connection's refresh token needs, read from the vault while its rows still
/// exist: the endpoint and client to use, the refresh token, the client secret and how it is sent.
/// Reading is separate from sending so a caller can read before it deletes the rows and send only
/// once that delete has committed (a revoke must never run for a write that is then rolled back).
pub(crate) struct PreparedRevoke {
    oauth: McpOauthMetadata,
    refresh_token: SecretString,
    client_secret: Option<SecretString>,
    auth_method: TokenAuthMethod,
    connection_id: String,
}

impl PreparedRevoke {
    /// Sends the revocation. Best effort: a failure is logged and returned, never raised.
    pub(crate) fn send(self) -> RevokeAttempt {
        let attempt = dispatch_revoke(
            &self.oauth,
            &self.refresh_token,
            self.client_secret.as_ref(),
            self.auth_method,
        );
        if attempt.attempted && !attempt.succeeded {
            tracing::warn!(
                connection_id = %self.connection_id,
                reason = %attempt.reason,
                "MCP OAuth refresh-token revocation failed; the replacement stands"
            );
        }
        attempt
    }
}

/// Reads what [`PreparedRevoke::send`] needs. `Err` is the attempt to report when there is nothing
/// to revoke (skipped) or it cannot be read (failed); nothing is sent either way.
pub(crate) fn prepare_oauth_refresh_revoke(
    vault: &SecretsVault,
    conn: &McpConnection,
) -> Result<PreparedRevoke, RevokeAttempt> {
    let Some(oauth) = conn
        .oauth
        .as_ref()
        .filter(|_| conn.auth_mode == McpAuthMode::Oauth)
    else {
        return Err(RevokeAttempt::skipped());
    };
    // A row of a connector whose client is the admin's revokes with the node's client: the pinned
    // revocation endpoint, the node's client id and the org secret. The row's own are not used.
    // The client is chosen by the grant's client id and token endpoint, not by the connector's
    // current URL: a row left on the server a connector update moved away from still holds a grant
    // of the admin's client (`revoke_client`).
    let org = match vault.workflow_store().revoke_client(conn) {
        Ok(RowClient::Org(org)) => Some(org),
        Ok(RowClient::Own) => None,
        Err(_) => return Err(RevokeAttempt::failed("credential store unavailable")),
    };
    let org_oauth: Option<McpOauthMetadata> = match &org {
        None => None,
        Some(org) => {
            let Some(node) = org.client.as_ref() else {
                return Err(RevokeAttempt::skipped());
            };
            // Tokens of an earlier client or endpoint are not revoked with another client's secret.
            if oauth.client_id != node.client_id || oauth.token_endpoint != node.token_endpoint {
                return Err(RevokeAttempt::skipped());
            }
            let Some(revocation) = node.revocation_endpoint.clone() else {
                return Err(RevokeAttempt::skipped());
            };
            Some(McpOauthMetadata {
                revocation_endpoint: Some(revocation),
                client_id: node.client_id.clone(),
                ..oauth.clone()
            })
        }
    };
    let oauth = org_oauth.as_ref().unwrap_or(oauth);
    if oauth
        .revocation_endpoint
        .as_deref()
        .map(str::trim)
        .filter(|endpoint| !endpoint.is_empty())
        .is_none()
    {
        return Err(RevokeAttempt::skipped());
    }
    let refresh_id = mcp_oauth_refresh_token_binding_id(&conn.connection_id);
    let has_refresh = match vault.has_binding_at(conn.scope, &conn.owner_user_id, &refresh_id) {
        Ok(present) => present,
        Err(_) => return Err(RevokeAttempt::failed("credential store unavailable")),
    };
    if !has_refresh {
        return Err(RevokeAttempt::skipped());
    }
    let refresh_token = match vault.decrypt_secret_at(conn.scope, &conn.owner_user_id, &refresh_id)
    {
        Ok(token) => token,
        Err(_) => return Err(RevokeAttempt::failed("credential store unavailable")),
    };
    if let Some(org) = &org {
        let client_secret = match (
            org.keeps_secret,
            org.client.as_ref(),
            &oauth.revocation_endpoint,
        ) {
            (true, Some(node), Some(endpoint)) => match org_client_secret(vault, node, endpoint) {
                Ok(secret) => Some(secret),
                Err(_) => return Err(RevokeAttempt::failed("OAuth client secret unavailable")),
            },
            _ => None,
        };
        // The revocation endpoint's own pinned method when the provider listed one, else the
        // token endpoint's (a provider that lists nothing, or a client saved before it was read).
        let method = org
            .client
            .as_ref()
            .map(|node| {
                node.revocation_auth_method
                    .unwrap_or(node.token_auth_method)
            })
            .unwrap_or_default();
        return Ok(PreparedRevoke {
            oauth: oauth.clone(),
            refresh_token,
            client_secret,
            auth_method: method,
            connection_id: conn.connection_id.clone(),
        });
    }
    let client_secret_id = mcp_oauth_client_secret_binding_id(&conn.connection_id);
    let client_secret =
        match vault.has_binding_at(conn.scope, &conn.owner_user_id, &client_secret_id) {
            Ok(true) => {
                match vault.decrypt_secret_at(conn.scope, &conn.owner_user_id, &client_secret_id) {
                    Ok(secret) => Some(secret),
                    Err(_) => return Err(RevokeAttempt::failed("credential store unavailable")),
                }
            }
            Ok(false) => None,
            Err(_) => return Err(RevokeAttempt::failed("credential store unavailable")),
        };
    Ok(PreparedRevoke {
        oauth: oauth.clone(),
        refresh_token,
        client_secret,
        auth_method: TokenAuthMethod::default(),
        connection_id: conn.connection_id.clone(),
    })
}

/// RFC 7009 revocation of a REST connector's refresh token at the revocation endpoint the node
/// pinned at Enable (the file's `endpoints.revoke`, kept only on the token endpoint's host). The
/// client is public: no secret, no Authorization header. Best effort, one request; the caller
/// deletes the rows whatever this returns.
pub(crate) fn revoke_oauth_connector_refresh_token(
    revocation_endpoint: &str,
    client_id: &str,
    refresh_token: &SecretString,
) -> RevokeAttempt {
    let oauth = McpOauthMetadata {
        resource: String::new(),
        authorization_server: String::new(),
        token_endpoint: String::new(),
        revocation_endpoint: Some(revocation_endpoint.to_string()),
        client_id: client_id.to_string(),
        registration_method: crate::mcp_registry::McpOauthRegistrationMethod::Manual,
        scopes: Vec::new(),
        expires_at: None,
        status: crate::mcp_registry::McpOauthStatus::Connected,
        last_refresh_at: None,
        last_refresh_error: None,
    };
    dispatch_revoke(&oauth, refresh_token, None, TokenAuthMethod::default())
}

fn dispatch_revoke(
    oauth: &McpOauthMetadata,
    refresh_token: &SecretString,
    client_secret: Option<&SecretString>,
    auth_method: TokenAuthMethod,
) -> RevokeAttempt {
    // Crate unit tests may point `revocationEndpoint` at a loopback mock.
    // Release builds and integration tests always use the pinned HTTPS client,
    // which rejects loopback before a request is sent.
    #[cfg(test)]
    if revocation_endpoint_is_loopback(oauth) {
        return revoke_refresh_token_unpinned_for_tests(
            oauth,
            refresh_token,
            client_secret,
            auth_method,
        );
    }
    revoke_refresh_token(oauth, refresh_token, client_secret, auth_method)
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
    auth_method: TokenAuthMethod,
) -> RevokeAttempt {
    let Some(raw) = oauth.revocation_endpoint.as_deref() else {
        return RevokeAttempt::skipped();
    };
    let endpoint = match parse_revocation_url(raw, true) {
        Ok(endpoint) => endpoint,
        Err(attempt) => return attempt,
    };
    // Test-node seam: the same loopback stand-ins as the token endpoint.
    if let Some(mapped) = mapped_loopback(endpoint.as_str()) {
        let client = match client_builder().build() {
            Ok(client) => client,
            Err(_) => return RevokeAttempt::failed("failed to create OAuth HTTP client"),
        };
        return post_revocation(
            &client,
            &mapped,
            oauth,
            refresh_token,
            client_secret,
            auth_method,
        );
    }
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
        auth_method,
    )
}

/// Loopback-only test seam. HTTPS metadata is mapped to HTTP on loopback for
/// a local mock authorization server. One request; no retries.
#[cfg(test)]
pub(crate) fn revoke_refresh_token_unpinned_for_tests(
    oauth: &McpOauthMetadata,
    refresh_token: &SecretString,
    client_secret: Option<&SecretString>,
    auth_method: TokenAuthMethod,
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
    post_revocation(
        &client,
        &endpoint,
        oauth,
        refresh_token,
        client_secret,
        auth_method,
    )
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
    auth_method: TokenAuthMethod,
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
    let mut basic = None;
    if let Some(secret) = client_secret {
        match auth_method {
            TokenAuthMethod::ClientSecretPost => {
                form.push(("client_secret", secret.expose_secret().as_str()));
            }
            TokenAuthMethod::ClientSecretBasic => {
                basic = Some((oauth.client_id.as_str(), secret.expose_secret().as_str()));
            }
        }
    }
    let request = with_basic_client_auth(client.post(endpoint.clone()).form(&form), basic);
    let response = match request.send() {
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
            TokenAuthMethod::ClientSecretPost,
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
            auth_method: TokenAuthMethod::default(),
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
            auth_method: TokenAuthMethod::default(),
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
                auth_method: TokenAuthMethod::default(),
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
                auth_method: TokenAuthMethod::default(),
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
            TokenAuthMethod::ClientSecretPost,
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
            TokenAuthMethod::ClientSecretPost,
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
                TokenAuthMethod::ClientSecretPost,
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
            TokenAuthMethod::ClientSecretPost,
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
                TokenAuthMethod::ClientSecretPost,
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
        let err = refresh_access_token(
            &oauth,
            &SecretString::new("refresh".into()),
            None,
            TokenAuthMethod::ClientSecretPost,
        )
        .err()
        .unwrap();
        assert_eq!(
            err,
            RefreshFailure::needs_reauth("invalid OAuth token endpoint")
        );
        let oauth = metadata("https://127.0.0.1:12345/token".into());
        let err = refresh_access_token(
            &oauth,
            &SecretString::new("refresh".into()),
            None,
            TokenAuthMethod::ClientSecretPost,
        )
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
        let err = refresh_access_token(
            &oauth,
            &SecretString::new("refresh".into()),
            None,
            TokenAuthMethod::ClientSecretPost,
        )
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
            TokenAuthMethod::ClientSecretPost,
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
            TokenAuthMethod::ClientSecretPost,
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
            TokenAuthMethod::ClientSecretPost,
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
        let attempt = revoke_refresh_token(
            &oauth,
            &SecretString::new(refresh.into()),
            None,
            TokenAuthMethod::ClientSecretPost,
        );
        assert!(attempt.attempted);
        assert!(!attempt.succeeded);
        assert_eq!(attempt.reason, "OAuth revocation endpoint rejected");
        assert!(!attempt.reason.contains(refresh));
        let http = revocation_metadata("http://auth.example.com/oauth/revoke".into());
        let attempt = revoke_refresh_token(
            &http,
            &SecretString::new(refresh.into()),
            None,
            TokenAuthMethod::ClientSecretPost,
        );
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

/// The code exchange and the revoke of a connector whose client is the admin's (CN-5.3, CN-5.4):
/// what is sent, to which host, with which secret, and what an answer may say.
#[cfg(test)]
mod org_client_tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    use super::*;
    use crate::connector_client::test_support::{enable_filebox, install_filebox_rows, member_row};
    use crate::mcp_registry::{McpOauthRegistrationMethod, McpOauthStatus};
    use crate::secrets::SecretScope;

    const SECRET: &str = "ORG-SECRET-value-9f2c";

    /// One loopback answer: reads one request, replies, returns the request text.
    fn serve_once(status: &str, payload: &str) -> (url::Url, thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = url::Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let (status, payload) = (status.to_owned(), payload.to_owned());
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
            let header = String::from_utf8_lossy(&request[..body_start]).to_ascii_lowercase();
            let length: usize = header
                .lines()
                .find_map(|l| l.strip_prefix("content-length: ").map(str::to_owned))
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
        (base, worker)
    }

    fn form_of(request: &str) -> std::collections::BTreeMap<String, String> {
        url::form_urlencoded::parse(request.split_once("\r\n\r\n").unwrap().1.as_bytes())
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect()
    }

    fn grant<'a>(endpoint: &'a str) -> CodeGrant<'a> {
        CodeGrant {
            token_endpoint: endpoint,
            client_id: "c-1",
            profile: CodeProfile::Mcp {
                resource: "https://mcp.example/mcp",
            },
            code: "CODE-1",
            code_verifier: "VERIFIER-abcdefghijklmnopqrstuvwxyz0123456789-ABCDE",
            redirect_uri: "squrell://oauth",
            auth_method: TokenAuthMethod::ClientSecretPost,
        }
    }

    /// The value of one request header (case of the name ignored, case of the value kept).
    fn header_of(request: &str, name: &str) -> Option<String> {
        request.split("\r\n").find_map(|line| {
            let (k, v) = line.split_once(": ")?;
            k.eq_ignore_ascii_case(name).then(|| v.to_string())
        })
    }

    /// A secret that every character class of `application/x-www-form-urlencoded` touches, and
    /// that again as RFC 6749 §2.3.1 sends it (the user name is `c-1`).
    const BASIC_SECRET: &str = "s3cr3t:with space/+&=%\u{e9}";
    fn basic_header() -> String {
        use base64::Engine as _;
        format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD
                .encode("c-1:s3cr3t%3Awith+space%2F%2B%26%3D%25%C3%A9")
        )
    }

    #[test]
    fn client_secret_basic_sends_the_urlencoded_id_and_secret_as_http_basic_and_no_secret_field() {
        // The code exchange.
        let (base, worker) = serve_once(
            "200 OK",
            r#"{"access_token":"A1","token_type":"bearer","refresh_token":"R1"}"#,
        );
        test_seam::serve("auth.basic-code.example", base);
        let mut g = grant("https://auth.basic-code.example/token");
        g.auth_method = TokenAuthMethod::ClientSecretBasic;
        let secret = SecretString::new(BASIC_SECRET.into());
        exchange_authorization_code(&g, Some(&secret)).ok().unwrap();
        let request = worker.join().unwrap();
        assert_eq!(header_of(&request, "authorization"), Some(basic_header()));
        let form = form_of(&request);
        assert!(!form.contains_key("client_secret"), "{form:?}");
        assert!(!request.contains("s3cr3t"), "{request}");
        assert_eq!(form["client_id"], "c-1");
        assert_eq!(form["grant_type"], "authorization_code");
        assert_eq!(form["code"], "CODE-1");

        // The refresh.
        let (base, worker) = serve_once(
            "200 OK",
            r#"{"access_token":"A2","token_type":"bearer","refresh_token":"R2"}"#,
        );
        test_seam::serve("auth.basic-refresh.example", base);
        let oauth = McpOauthMetadata {
            resource: "https://mcp.example/mcp".into(),
            authorization_server: "https://auth.basic-refresh.example".into(),
            token_endpoint: "https://auth.basic-refresh.example/token".into(),
            revocation_endpoint: None,
            client_id: "c-1".into(),
            registration_method: McpOauthRegistrationMethod::Manual,
            scopes: vec![],
            expires_at: None,
            status: McpOauthStatus::Connected,
            last_refresh_at: None,
            last_refresh_error: None,
        };
        refresh_access_token(
            &oauth,
            &SecretString::new("R1".into()),
            Some(&secret),
            TokenAuthMethod::ClientSecretBasic,
        )
        .ok()
        .unwrap();
        let request = worker.join().unwrap();
        assert_eq!(header_of(&request, "authorization"), Some(basic_header()));
        let form = form_of(&request);
        assert!(!form.contains_key("client_secret"), "{form:?}");
        assert_eq!(form["grant_type"], "refresh_token");
        assert_eq!(form["resource"], "https://mcp.example/mcp");

        // The revoke.
        let (base, worker) = serve_once("200 OK", "");
        let mut revoking = oauth.clone();
        revoking.revocation_endpoint = Some(format!("{base}revoke"));
        let attempt = revoke_refresh_token_unpinned_for_tests(
            &revoking,
            &SecretString::new("R2".into()),
            Some(&secret),
            TokenAuthMethod::ClientSecretBasic,
        );
        assert!(attempt.succeeded, "{attempt:?}");
        let request = worker.join().unwrap();
        assert_eq!(header_of(&request, "authorization"), Some(basic_header()));
        let form = form_of(&request);
        assert!(!form.contains_key("client_secret"), "{form:?}");
        assert_eq!(form["token"], "R2");

        // The default is the body, and sends no Authorization header at all.
        let (base, worker) = serve_once(
            "200 OK",
            r#"{"access_token":"A3","token_type":"bearer","refresh_token":"R3"}"#,
        );
        test_seam::serve("auth.post-code.example", base);
        exchange_authorization_code(
            &grant("https://auth.post-code.example/token"),
            Some(&secret),
        )
        .ok()
        .unwrap();
        let request = worker.join().unwrap();
        assert_eq!(header_of(&request, "authorization"), None);
        assert_eq!(form_of(&request)["client_secret"], BASIC_SECRET);
    }

    fn code_grant<'a>(profile: CodeProfile<'a>, method: TokenAuthMethod) -> CodeGrant<'a> {
        CodeGrant {
            token_endpoint: "https://as.example/token",
            client_id: "c",
            profile,
            code: "CODE",
            code_verifier: "V",
            redirect_uri: "squrell://oauth",
            auth_method: method,
        }
    }

    #[test]
    fn a_code_form_has_resource_only_for_mcp_and_a_secret_only_when_the_org_keeps_one() {
        let mcp = CodeProfile::Mcp {
            resource: "https://mcp.example/mcp",
        };
        // Filebox (a secret, client_secret_post): byte-identical to what shipped in Phase 5.
        assert_eq!(
            code_form(
                &code_grant(mcp, TokenAuthMethod::ClientSecretPost),
                Some("cs")
            ),
            vec![
                ("grant_type", "authorization_code"),
                ("code", "CODE"),
                ("client_id", "c"),
                ("client_secret", "cs"),
                ("code_verifier", "V"),
                ("redirect_uri", "squrell://oauth"),
                ("resource", "https://mcp.example/mcp"),
            ]
        );
        // client_secret_basic: the secret is the Authorization header's, never in the body.
        let basic = code_form(
            &code_grant(mcp, TokenAuthMethod::ClientSecretBasic),
            Some("cs"),
        );
        assert!(
            basic.iter().all(|(k, _)| *k != "client_secret"),
            "{basic:?}"
        );
        // A secret-less MCP client sends no secret and keeps its resource.
        let public = code_form(&code_grant(mcp, TokenAuthMethod::ClientSecretPost), None);
        assert!(
            public.iter().all(|(k, _)| *k != "client_secret"),
            "{public:?}"
        );
        assert!(public.contains(&("resource", "https://mcp.example/mcp")));
        // An `oauth` connector: no secret and no resource, whatever the caller hands in.
        for secret in [None, Some("cs")] {
            for method in [
                TokenAuthMethod::ClientSecretPost,
                TokenAuthMethod::ClientSecretBasic,
            ] {
                assert_eq!(
                    code_form(&code_grant(CodeProfile::Oauth, method), secret),
                    vec![
                        ("grant_type", "authorization_code"),
                        ("code", "CODE"),
                        ("client_id", "c"),
                        ("code_verifier", "V"),
                        ("redirect_uri", "squrell://oauth"),
                    ]
                );
            }
        }
    }

    #[test]
    fn an_oauth_refresh_form_has_no_secret_no_resource_and_a_scope_only_when_listed() {
        let target = |scope| RefreshTarget {
            token_endpoint: "https://acme.example/oauth/tokens",
            client_id: "zc1",
            profile: RefreshProfile::Oauth { scope },
            auth_method: TokenAuthMethod::default(),
        };
        let with: Vec<_> = refresh_form(&target(Some("read write")), "rt", Some("cs"));
        assert_eq!(
            with,
            vec![
                ("grant_type", "refresh_token"),
                ("refresh_token", "rt"),
                ("client_id", "zc1"),
                ("scope", "read write"),
            ]
        );
        let without: Vec<_> = refresh_form(&target(None), "rt", Some("cs"));
        assert_eq!(without.len(), 3, "{without:?}");
    }

    #[test]
    fn an_oauth_code_exchange_and_refresh_on_the_wire_send_no_secret_resource_or_authorization() {
        let (base, worker) = serve_once(
            "200 OK",
            r#"{"access_token":"A1","token_type":"Bearer","expires_in":3600,"refresh_token":"R1"}"#,
        );
        test_seam::serve("acme.oauth-wire.example", base);
        let secret = SecretString::new("MUST-NOT-TRAVEL-0000".into());
        let grant = CodeGrant {
            token_endpoint: "https://acme.oauth-wire.example/oauth/tokens",
            ..code_grant(CodeProfile::Oauth, TokenAuthMethod::ClientSecretPost)
        };
        // Even handed a secret by mistake, the exchange of an `oauth` connector sends none.
        exchange_authorization_code(&grant, Some(&secret))
            .ok()
            .unwrap();
        let request = worker.join().unwrap();
        assert_eq!(header_of(&request, "authorization"), None, "{request}");
        assert!(!request.contains("MUST-NOT-TRAVEL"), "{request}");
        let form = form_of(&request);
        for key in ["client_secret", "resource", "scope"] {
            assert!(!form.contains_key(key), "{key} in {form:?}");
        }
        assert_eq!(form["grant_type"], "authorization_code");
        assert_eq!(form["client_id"], "c");
        assert_eq!(form["code_verifier"], "V");
        assert_eq!(form["redirect_uri"], "squrell://oauth");

        let (base, worker) = serve_once(
            "200 OK",
            r#"{"access_token":"A2","token_type":"Bearer","refresh_token":"R2"}"#,
        );
        test_seam::serve("acme.oauth-wire-refresh.example", base);
        let token = refresh_access_token_oauth(
            "https://acme.oauth-wire-refresh.example/oauth/tokens",
            "zc1",
            Some("read"),
            &SecretString::new("R1".into()),
        )
        .ok()
        .unwrap();
        assert_eq!(token.refresh_token.as_deref(), Some("R2"));
        let request = worker.join().unwrap();
        assert_eq!(header_of(&request, "authorization"), None, "{request}");
        let form = form_of(&request);
        assert!(!form.contains_key("client_secret") && !form.contains_key("resource"));
        assert_eq!(form["grant_type"], "refresh_token");
        assert_eq!(form["refresh_token"], "R1");
        assert_eq!(form["client_id"], "zc1");
        assert_eq!(form["scope"], "read");
    }

    #[test]
    fn a_code_exchange_posts_exactly_these_fields_to_the_pinned_endpoint() {
        let (base, worker) = serve_once(
            "200 OK",
            r#"{"access_token":"A1","token_type":"bearer","expires_in":3600,"refresh_token":"R1"}"#,
        );
        test_seam::serve("auth.code-form.example", base);
        let secret = SecretString::new(SECRET.into());
        let tokens = exchange_authorization_code(
            &grant("https://auth.code-form.example/token"),
            Some(&secret),
        )
        .ok()
        .unwrap();
        assert_eq!(tokens.access_token, "A1");
        assert_eq!(tokens.refresh_token.as_deref(), Some("R1"));
        assert_eq!(tokens.expires_in, Some(3600));
        let request = worker.join().unwrap();
        assert!(request.starts_with("POST /token HTTP/1.1\r\n"), "{request}");
        let form = form_of(&request);
        assert_eq!(
            form.keys().map(String::as_str).collect::<Vec<_>>(),
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
        assert_eq!(form["grant_type"], "authorization_code");
        assert_eq!(form["client_secret"], SECRET);
        assert_eq!(form["client_id"], "c-1");
        assert_eq!(form["code"], "CODE-1");
        assert_eq!(form["redirect_uri"], "squrell://oauth");
        assert_eq!(form["resource"], "https://mcp.example/mcp");
    }

    #[test]
    fn a_refused_exchange_names_a_code_never_a_value() {
        for (status, body, kind, reason) in [
            (
                "401 Unauthorized",
                r#"{"error":"invalid_client","error_description":"bad ORG-SECRET-value-9f2c"}"#,
                RefreshFailureKind::NeedsReauth,
                "invalid_client",
            ),
            (
                "400 Bad Request",
                r#"{"error":"invalid_grant","error_description":"CODE-1 spent"}"#,
                RefreshFailureKind::NeedsReauth,
                "invalid_grant",
            ),
            (
                "503 Service Unavailable",
                r#"{"error":"temporarily_unavailable"}"#,
                RefreshFailureKind::Transient,
                "OAuth token endpoint unavailable (HTTP 503)",
            ),
            (
                "200 OK",
                r#"{"not":"a token"}"#,
                RefreshFailureKind::Transient,
                "invalid OAuth token response",
            ),
        ] {
            let (base, worker) = serve_once(status, body);
            let host = format!("auth.code-refused-{}.example", reason.len());
            test_seam::serve(&host, base);
            let secret = SecretString::new(SECRET.into());
            let failure = exchange_authorization_code(
                &grant(&format!("https://{host}/token")),
                Some(&secret),
            )
            .err()
            .unwrap();
            worker.join().unwrap();
            assert_eq!(failure.kind, kind, "{status}");
            assert_eq!(failure.reason, reason);
            for value in [SECRET, "CODE-1", "VERIFIER-abc", &host] {
                assert!(!failure.reason.contains(value), "{}", failure.reason);
            }
        }
    }

    #[test]
    fn the_exchange_never_leaves_the_public_https_rules() {
        // No test-seam entry and no process-mode seam: the pin refuses a restricted address and a
        // plain-http endpoint before any request, so the secret is never sent.
        let secret = SecretString::new(SECRET.into());
        for endpoint in [
            "https://127.0.0.1:9/token",
            "https://10.0.0.1/token",
            "http://auth.never-mapped.example/token",
            "https://user:pw@auth.never-mapped.example/token",
            "not a url",
        ] {
            let failure = exchange_authorization_code(&grant(endpoint), Some(&secret))
                .err()
                .unwrap();
            assert!(
                matches!(
                    failure.reason.as_str(),
                    "OAuth token endpoint rejected" | "invalid OAuth token endpoint"
                ),
                "{endpoint}: {}",
                failure.reason
            );
        }
        // A value that cannot travel in a form is refused first.
        let bad_secret = SecretString::new("line\nbreak".into());
        let failure = exchange_authorization_code(
            &grant("https://auth.never-mapped.example/token"),
            Some(&bad_secret),
        )
        .err()
        .unwrap();
        assert_eq!(failure.reason, "invalid OAuth client secret");
    }

    #[test]
    fn a_disconnect_revokes_at_the_nodes_endpoint_with_the_org_secret() {
        let store = std::sync::Arc::new(SqliteWorkflowStore::in_memory().unwrap());
        let vault = SecretsVault::for_tests(store.clone());
        install_filebox_rows(&store, "revoke-ok");
        enable_filebox(&store, &vault, "revoke-ok", "c-1", SECRET);
        // The row names a revocation endpoint of its own: the node's is the one used.
        let conn = member_row(
            &store,
            &vault,
            "revoke-ok",
            "ada",
            "c-1",
            Some("https://evil.example/revoke"),
        );
        let (base, worker) = serve_once("200 OK", "");
        test_seam::serve("auth.revoke-ok.example", base);
        let attempt = best_effort_revoke_oauth_refresh(&vault, &conn);
        assert!(attempt.attempted && attempt.succeeded, "{attempt:?}");
        let request = worker.join().unwrap();
        assert!(
            request.starts_with("POST /revoke HTTP/1.1\r\n"),
            "{request}"
        );
        let form = form_of(&request);
        assert_eq!(form["token"], "R-old");
        assert_eq!(form["token_type_hint"], "refresh_token");
        assert_eq!(form["client_id"], "c-1");
        assert_eq!(form["client_secret"], SECRET);
    }

    #[test]
    fn tokens_of_another_client_are_not_revoked_with_this_clients_secret() {
        let store = std::sync::Arc::new(SqliteWorkflowStore::in_memory().unwrap());
        let vault = SecretsVault::for_tests(store.clone());
        install_filebox_rows(&store, "revoke-old");
        enable_filebox(&store, &vault, "revoke-old", "c-2", SECRET);
        // The member's tokens belong to client c-1; the admin moved to c-2.
        let conn = member_row(
            &store,
            &vault,
            "revoke-old",
            "ada",
            "c-1",
            Some("https://auth.revoke-old.example/revoke"),
        );
        // No server is registered: any request would fail, and `attempted` would be true.
        let attempt = best_effort_revoke_oauth_refresh(&vault, &conn);
        assert!(!attempt.attempted, "{attempt:?}");
    }

    #[test]
    fn a_row_of_another_server_keeps_its_own_client_and_secret_for_revoke() {
        let store = std::sync::Arc::new(SqliteWorkflowStore::in_memory().unwrap());
        let vault = SecretsVault::for_tests(store.clone());
        install_filebox_rows(&store, "revoke-own");
        enable_filebox(&store, &vault, "revoke-own", "c-1", SECRET);
        // A server added by hand under another connection id uses the per-row secret as before.
        let oauth = McpOauthMetadata {
            resource: "https://other.example/mcp".into(),
            authorization_server: "https://auth.revoke-own.example".into(),
            token_endpoint: "https://auth.revoke-own.example/token".into(),
            revocation_endpoint: Some("https://auth.revoke-own.example/revoke".into()),
            client_id: "member-client".into(),
            registration_method: McpOauthRegistrationMethod::Manual,
            scopes: vec![],
            expires_at: None,
            status: McpOauthStatus::Connected,
            last_refresh_at: None,
            last_refresh_error: None,
        };
        let conn = store
            .upsert_mcp_connection(
                SecretScope::User,
                "ada",
                "other",
                "https://other.example/mcp",
                "",
                &[],
                &[],
                McpAuthMode::Oauth,
                Some(&oauth),
            )
            .unwrap();
        crate::mcp_registry::store_mcp_oauth_tokens(
            &vault,
            &conn,
            &crate::mcp_registry::McpOauthTokenWrite {
                access_token: "A".into(),
                refresh_token: "R-other".into(),
                client_secret: Some("MEMBER-OWN-SECRET".into()),
            },
            None,
            crate::users::UserRole::Member,
        )
        .unwrap();
        let (base, worker) = serve_once("200 OK", "");
        test_seam::serve("auth.revoke-own.example", base);
        let attempt = best_effort_revoke_oauth_refresh(&vault, &conn);
        assert!(attempt.succeeded, "{attempt:?}");
        let form = form_of(&worker.join().unwrap());
        assert_eq!(form["client_id"], "member-client");
        assert_eq!(form["client_secret"], "MEMBER-OWN-SECRET");
    }
}
