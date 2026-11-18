//! The admin-supplied OAuth client of a connector (CN-5.2, locked decisions 12 and 13).
//!
//! A connector with `provider.client: "org"` has no client id in its file. The org admin registers
//! an OAuth client with the provider and saves it with the Enable (`PUT /v1/connectors/{id}/node`,
//! `client: { clientId, clientSecret? }`). Three things are stored:
//!
//! - the client id and the endpoints discovery pinned (`node_connector_clients`): public facts a
//!   member's sign-in needs, never the secret;
//! - the secret, when the connector says `clientSecret: "org_vault"`: one org-scope `connection`
//!   vault row `connector/<id>/oauth/client-secret` pinned to the token endpoint's host. It is
//!   written in the same transaction as the rows above, only after every check passed, by this
//!   module's store method and by no other code. It is never returned, listed, logged or put in an
//!   event; `GET /v1/secrets` omits the row and `DELETE /v1/secrets/...` refuses it.
//! - the endpoints are pinned at the save: a token endpoint that moves needs the secret entered
//!   again, so a changed provider answer can never redirect the stored secret.
//!
//! What the node does with the secret (the code exchange, refresh, revoke) is later waves.

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Deserializer};
use serde_json::{json, Value};
use vam_spec::connector::{ClientKind, ClientSecret, Connector, Provider};

use crate::connector_discovery::{host_of, Discovered};
use crate::connectors::connector_id;
use crate::error::OrchestratorError;
use crate::secrets::{SecretKind, SecretScope, SecretsVault, ORG_OWNER_USER_ID};
use crate::workflow::SqliteWorkflowStore;

const MAX_CLIENT_ID_LEN: usize = 128;
const MAX_SECRET_LEN: usize = 4096;

/// `connector/<id>/oauth/client-secret`: the org's client secret (`connection` kind, org scope).
pub fn client_secret_binding_id(connector_id: &str) -> String {
    format!("connector/{connector_id}/oauth/client-secret")
}

/// True for an org-scope row that belongs to a connector's node state (the client secret). The
/// secrets API neither lists, reads, writes nor deletes such a row (open question 11): the
/// connector entry's `secretSet` is the only view.
pub fn is_connector_org_row(scope: SecretScope, binding_id: &str) -> bool {
    scope == SecretScope::Org
        && binding_id.starts_with("connector/")
        && binding_id.contains("/oauth/")
}

/// `provider.client` of a connector (`publisher` for a connector with no OAuth provider).
pub fn client_kind(def: &Connector) -> ClientKind {
    match &def.provider {
        Provider::Oauth2(o) => o.client,
        Provider::ApiKey(_) => ClientKind::Publisher,
    }
}

/// `provider.clientSecret` of a connector (`none` for a connector with no OAuth provider).
fn secret_kind(def: &Connector) -> ClientSecret {
    match &def.provider {
        Provider::Oauth2(o) => o.client_secret,
        Provider::ApiKey(_) => ClientSecret::None,
    }
}

/// The `client` of `PUT /v1/connectors/{id}/node`. Holds a secret: no `Debug` output of it.
#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ClientRequest {
    pub client_id: String,
    #[serde(default)]
    pub client_secret: Option<String>,
}

impl std::fmt::Debug for ClientRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientRequest")
            .field("client_id", &self.client_id)
            .field(
                "client_secret",
                &self.client_secret.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

/// Omitted keeps the stored client; `null` clears it; an object sets it.
pub(crate) fn client_some_or_null<'de, D>(d: D) -> Result<Option<Option<ClientRequest>>, D::Error>
where
    D: Deserializer<'de>,
{
    Option::<ClientRequest>::deserialize(d).map(Some)
}

/// The stored, public half of an admin-supplied client. No secret is here, only whether one is set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeClient {
    pub connector_id: String,
    pub client_id: String,
    pub issuer: String,
    pub authorize_endpoint: String,
    pub token_endpoint: String,
    /// Only when it is on the token endpoint's host.
    pub revocation_endpoint: Option<String>,
    pub secret_set: bool,
    pub updated_at: i64,
    pub updated_by: String,
}

impl NodeClient {
    /// The host the client secret is pinned to (and the only host it is ever sent to).
    pub fn token_host(&self) -> Option<String> {
        host_of(&self.token_endpoint)
    }
}

/// What the request does to the stored client, decided from the body and the stored state.
#[derive(Debug, Clone, PartialEq)]
pub enum ClientIntent {
    /// The body has no `client`.
    Keep,
    /// `client: null` with `enabled: false` and a client stored.
    Clear,
    /// A client in the body: discovery runs, then the client is stored.
    Set(ClientRequest),
}

fn invalid(msg: impl Into<String>) -> OrchestratorError {
    OrchestratorError::InvalidContract(msg.into())
}

fn check_client_id(id: &str, value: &str) -> Result<String, OrchestratorError> {
    let v = value.trim();
    let ok = !v.is_empty()
        && v.len() <= MAX_CLIENT_ID_LEN
        && v.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if ok {
        Ok(v.to_string())
    } else {
        Err(invalid(format!(
            "the client id for `{id}` must be 1-{MAX_CLIENT_ID_LEN} characters of letters, digits, `.`, `_`, `-`"
        )))
    }
}

/// A fixed text: the value is never quoted.
fn check_secret(id: &str, value: &str) -> Result<(), OrchestratorError> {
    if value.is_empty()
        || value.len() > MAX_SECRET_LEN
        || value.bytes().any(|b| b == b'\r' || b == b'\n' || b == 0)
    {
        return Err(invalid(format!(
            "the client secret for `{id}` must be 1-{MAX_SECRET_LEN} characters with no line break"
        )));
    }
    Ok(())
}

/// Decides what a body's `client` means for this connector. Pure; nothing is read from the network
/// or written. Every refusal is decided here or in [`plan_client`], before any write.
///
/// - a client on a connector that uses the publisher's own client: 400 (names the client);
/// - enabling an `org` connector with no client stored or given: 409 (names the client id);
/// - `org_vault` with no secret stored or given: 400 (names the secret). A stored secret is kept
///   only for the same client id: a secret belongs to the client it was registered with, so a new
///   client id needs its secret again (400). A secret on a connector that keeps none: 400;
/// - `client: null` only with `enabled: false`.
pub fn check_request(
    def: &Connector,
    existing: Option<&NodeClient>,
    enabled: bool,
    field: &Option<Option<ClientRequest>>,
) -> Result<ClientIntent, OrchestratorError> {
    let id = connector_id(def);
    let org = client_kind(def) == ClientKind::Org;
    match field {
        None => {
            if enabled && org && existing.is_none() {
                return Err(OrchestratorError::Conflict(format!(
                    "`{id}` needs your organization's OAuth client id before it can be enabled: \
                     register a client with the provider and send `client: {{ clientId, clientSecret }}`"
                )));
            }
            Ok(ClientIntent::Keep)
        }
        Some(_) if !org => Err(invalid(format!(
            "`{id}` uses the publisher's own OAuth client; it takes no client"
        ))),
        Some(None) => {
            if enabled {
                return Err(invalid(
                    "a client is removed only when disabling: send `enabled: false` with `client: null`",
                ));
            }
            Ok(if existing.is_some() {
                ClientIntent::Clear
            } else {
                ClientIntent::Keep
            })
        }
        Some(Some(c)) => {
            check_client_id(&id, &c.client_id)?;
            match (secret_kind(def), &c.client_secret) {
                (ClientSecret::OrgVault, Some(s)) => check_secret(&id, s)?,
                (ClientSecret::OrgVault, None)
                    if existing.is_some_and(|e| e.secret_set && e.client_id == c.client_id) => {}
                (ClientSecret::OrgVault, None) => {
                    return Err(invalid(format!(
                        "`{id}` needs the client secret you registered with the provider: send `client.clientSecret`"
                    )));
                }
                (_, Some(_)) => {
                    return Err(invalid(format!("`{id}` keeps no client secret")));
                }
                (_, None) => {}
            }
            Ok(ClientIntent::Set(c.clone()))
        }
    }
}

/// Whether the connector keeps its secret on the node (the provider must then take it by POST).
pub fn keeps_secret(def: &Connector) -> bool {
    secret_kind(def) == ClientSecret::OrgVault
}

/// The write a request makes, once discovery (for a `Set`) answered.
#[derive(Debug, Clone, PartialEq)]
pub enum ClientWrite {
    Keep,
    Set {
        client: Box<NodeClient>,
        /// The new secret to seal into the vault row; `None` keeps the stored one.
        secret: Option<String>,
    },
    Clear,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ClientPlan {
    pub write: ClientWrite,
    /// Members' connections to the connector are marked `needs_reauth` in the same transaction: the
    /// client id or the token endpoint changed, or the client was removed. A new secret alone is not.
    pub reauth: bool,
}

/// Plans the write for `intent`. `discovered` is the answer for a `Set` (and `None` otherwise).
/// A token endpoint that moved to another host needs the secret entered again: the stored secret
/// is pinned to the old host and never follows a changed provider answer (O2).
pub fn plan_client(
    def: &Connector,
    existing: Option<&NodeClient>,
    intent: ClientIntent,
    discovered: Option<Discovered>,
    now: i64,
    by: &str,
) -> Result<ClientPlan, OrchestratorError> {
    let id = connector_id(def);
    match intent {
        ClientIntent::Keep => Ok(ClientPlan {
            write: ClientWrite::Keep,
            reauth: false,
        }),
        ClientIntent::Clear => Ok(ClientPlan {
            write: ClientWrite::Clear,
            reauth: true,
        }),
        ClientIntent::Set(req) => {
            let d = discovered.ok_or_else(|| {
                OrchestratorError::Core("client planned without discovery".into())
            })?;
            let client_id = check_client_id(&id, &req.client_id)?;
            let keeps = keeps_secret(def);
            let new_host = host_of(&d.token_endpoint);
            if keeps
                && req.client_secret.is_none()
                && existing.is_some_and(|e| e.token_host() != new_host)
            {
                return Err(OrchestratorError::Conflict(format!(
                    "the provider's token endpoint for `{id}` is now on {}; enter the client secret again \
                     so it is stored for that host",
                    new_host.as_deref().unwrap_or("another host")
                )));
            }
            let client = NodeClient {
                connector_id: id,
                client_id,
                issuer: d.issuer,
                authorize_endpoint: d.authorize_endpoint,
                token_endpoint: d.token_endpoint,
                revocation_endpoint: d.revocation_endpoint,
                secret_set: keeps
                    && (req.client_secret.is_some() || existing.is_some_and(|e| e.secret_set)),
                updated_at: now,
                updated_by: by.to_string(),
            };
            let reauth = existing.is_some_and(|e| {
                e.client_id != client.client_id || e.token_endpoint != client.token_endpoint
            });
            Ok(ClientPlan {
                write: ClientWrite::Set {
                    client: Box::new(client),
                    secret: req.client_secret,
                },
                reauth,
            })
        }
    }
}

/// Adds `node.client` and the admin's client id to a connector's list entry. For a connector that
/// uses the publisher's client nothing changes. A member sees the client once the connector is
/// enabled (id, whether a secret is set, the pinned authorize and token endpoints, when it was
/// saved); an admin always, and with who saved it. The secret is not an input of this function.
pub fn annotate_client(
    entry: &mut Value,
    def: &Connector,
    client: Option<&NodeClient>,
    viewer_is_admin: bool,
) {
    if client_kind(def) != ClientKind::Org {
        return;
    }
    let enabled = entry["node"]["enabled"].as_bool().unwrap_or(false);
    entry["node"]["configured"] = json!(client.is_some());
    let Some(c) = client else { return };
    if !(enabled || viewer_is_admin) {
        return;
    }
    let mut view = json!({
        "clientId": c.client_id,
        "secretSet": c.secret_set,
        "authorizeEndpoint": c.authorize_endpoint,
        "tokenEndpoint": c.token_endpoint,
        "updatedAt": c.updated_at,
    });
    if viewer_is_admin {
        view["updatedBy"] = json!(c.updated_by);
    }
    entry["node"]["client"] = view;
    if enabled {
        if let Some(p) = entry.get_mut("provider") {
            p["clientId"] = json!(c.client_id);
        }
    }
}

// ---- storage -----------------------------------------------------------------------------------

pub(crate) fn initialize_schema(connection: &Connection) -> Result<(), OrchestratorError> {
    connection.execute_batch(
        "
        -- The public half of an admin-supplied client (CN-5.2): never the secret, which is the
        -- org-scope `connection` vault row `connector/<id>/oauth/client-secret`.
        CREATE TABLE IF NOT EXISTS node_connector_clients (
            connector_id TEXT PRIMARY KEY,
            client_id TEXT NOT NULL,
            issuer TEXT NOT NULL,
            authorize_endpoint TEXT NOT NULL,
            token_endpoint TEXT NOT NULL,
            revocation_endpoint TEXT,
            secret_set INTEGER NOT NULL,
            updated_at INTEGER NOT NULL,
            updated_by TEXT NOT NULL
        );
        ",
    )?;
    Ok(())
}

const COLUMNS: &str = "connector_id, client_id, issuer, authorize_endpoint, token_endpoint, \
                       revocation_endpoint, secret_set, updated_at, updated_by";

fn row_to_client(r: &rusqlite::Row<'_>) -> rusqlite::Result<NodeClient> {
    Ok(NodeClient {
        connector_id: r.get(0)?,
        client_id: r.get(1)?,
        issuer: r.get(2)?,
        authorize_endpoint: r.get(3)?,
        token_endpoint: r.get(4)?,
        revocation_endpoint: r.get(5)?,
        secret_set: r.get::<_, i64>(6)? != 0,
        updated_at: r.get(7)?,
        updated_by: r.get(8)?,
    })
}

/// Removes a connector's stored client and its secret row inside the caller's transaction. Used by
/// `client: null` and by an import that moved the connector's MCP URL (the pinned endpoints were
/// discovered from the old one). Returns whether a client was there.
pub(crate) fn clear_client_in(
    c: &Connection,
    connector_id: &str,
) -> Result<bool, OrchestratorError> {
    let removed = c.execute(
        "DELETE FROM node_connector_clients WHERE connector_id = ?1",
        params![connector_id],
    )?;
    c.execute(
        "DELETE FROM host_secrets WHERE scope = 'org' AND owner_user_id = ?1 AND binding_id = ?2",
        params![ORG_OWNER_USER_ID, client_secret_binding_id(connector_id)],
    )?;
    Ok(removed > 0)
}

impl SqliteWorkflowStore {
    pub fn node_client(&self, id: &str) -> Result<Option<NodeClient>, OrchestratorError> {
        let c = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        Ok(c.query_row(
            &format!("SELECT {COLUMNS} FROM node_connector_clients WHERE connector_id = ?1"),
            params![id],
            row_to_client,
        )
        .optional()?)
    }

    pub fn node_clients(&self) -> Result<Vec<NodeClient>, OrchestratorError> {
        let c = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let mut stmt = c.prepare(&format!(
            "SELECT {COLUMNS} FROM node_connector_clients ORDER BY connector_id"
        ))?;
        let rows = stmt
            .query_map([], row_to_client)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Writes the node row, the client row, the secret row and the members' `needs_reauth` marks in
    /// one transaction: all of it, or none. The secret is sealed before the transaction opens (so
    /// an encryption failure writes nothing) and written last inside it.
    ///
    /// This is the only code that writes `connector/<id>/oauth/client-secret`.
    pub(crate) fn put_node_with_client(
        &self,
        vault: &SecretsVault,
        node: &crate::connectors::NodeConnector,
        plan: &ClientPlan,
        mcp_url: &str,
    ) -> Result<(), OrchestratorError> {
        let sealed = match &plan.write {
            ClientWrite::Set {
                secret: Some(s), ..
            } => Some(vault.seal(s)?),
            _ => None,
        };
        let mut c = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let tx = c.transaction()?;
        crate::connectors::put_node_connector_in(&tx, node)?;
        match &plan.write {
            ClientWrite::Keep => {}
            ClientWrite::Clear => {
                clear_client_in(&tx, &node.connector_id)?;
            }
            ClientWrite::Set { client, .. } => {
                tx.execute(
                    &format!(
                        "INSERT INTO node_connector_clients ({COLUMNS})
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                         ON CONFLICT(connector_id) DO UPDATE SET client_id = excluded.client_id,
                             issuer = excluded.issuer,
                             authorize_endpoint = excluded.authorize_endpoint,
                             token_endpoint = excluded.token_endpoint,
                             revocation_endpoint = excluded.revocation_endpoint,
                             secret_set = excluded.secret_set,
                             updated_at = excluded.updated_at, updated_by = excluded.updated_by"
                    ),
                    params![
                        client.connector_id,
                        client.client_id,
                        client.issuer,
                        client.authorize_endpoint,
                        client.token_endpoint,
                        client.revocation_endpoint,
                        client.secret_set as i64,
                        client.updated_at,
                        client.updated_by
                    ],
                )?;
                if let (Some(sealed), Some(host)) = (&sealed, client.token_host()) {
                    crate::secrets::put_host_secret_in(
                        &tx,
                        &client_secret_binding_id(&client.connector_id),
                        SecretKind::Connection,
                        &sealed.nonce,
                        &sealed.ciphertext,
                        &sealed.fingerprint,
                        &[host],
                        None,
                        SecretScope::Org,
                        ORG_OWNER_USER_ID,
                    )?;
                }
            }
        }
        if plan.reauth {
            crate::mcp_registry::mark_connector_rows_need_reauth(
                &tx,
                &node.connector_id,
                &[mcp_url],
                "the admin changed the client: reconnect",
            )?;
        }
        tx.commit()?;
        Ok(())
    }
}

// ---- what the node does with the client (CN-5.3, CN-5.4) ----------------------------------------

/// A connector with an admin-supplied client, and the client the admin saved for it (if any).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct OrgConnector {
    pub(crate) mcp_url: String,
    /// `clientSecret: "org_vault"`: the node holds a secret and does the code exchange.
    pub(crate) keeps_secret: bool,
    pub(crate) scopes: Vec<String>,
    pub(crate) client: Option<NodeClient>,
}

/// How a connector-backed MCP row gets its OAuth client.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum RowClient {
    /// Not a connector's row, or a connector that uses the publisher's own client or discovers its
    /// own (the row's stored endpoints and secret are what the member's app set).
    Own,
    /// The connector's client is the admin's: endpoints and client id come from the node row.
    Org(Box<OrgConnector>),
}

impl SqliteWorkflowStore {
    /// The connector with this id when it uses the admin's client (`provider.client: org`) and is an
    /// MCP connector; `None` for every other connector id.
    pub(crate) fn org_connector(
        &self,
        id: &str,
    ) -> Result<Option<OrgConnector>, OrchestratorError> {
        let Some(entry) = self.connector_entry(id)? else {
            return Ok(None);
        };
        if client_kind(&entry.def) != ClientKind::Org {
            return Ok(None);
        }
        let Some(policy) = crate::connectors::policy_of(id, &entry.def) else {
            return Ok(None);
        };
        let scopes = match &entry.def.provider {
            Provider::Oauth2(o) => o.scopes.clone(),
            Provider::ApiKey(_) => Vec::new(),
        };
        Ok(Some(OrgConnector {
            mcp_url: policy.url,
            keeps_secret: keeps_secret(&entry.def),
            scopes,
            client: self.node_client(id)?,
        }))
    }

    /// Whose client a member's MCP row uses. A row the connector owns (user scope, the connector's
    /// URL) of a `client: org` connector uses the node's client; any other row uses its own.
    pub(crate) fn row_client(
        &self,
        conn: &crate::mcp_registry::McpConnection,
    ) -> Result<RowClient, OrchestratorError> {
        if conn.scope != SecretScope::User {
            return Ok(RowClient::Own);
        }
        Ok(match self.org_connector(&conn.connection_id)? {
            Some(org) if org.mcp_url == conn.url => RowClient::Org(Box::new(org)),
            _ => RowClient::Own,
        })
    }
}

/// The org's client secret, for one request to `endpoint`. **The only place the secret is read for
/// sending.** It is read at org scope with [`SecretsVault::decrypt_secret_at`] (never a lookup that
/// falls back across scopes), and only when the endpoint's host is the token endpoint's host the
/// node pinned at Enable and the vault row is pinned to exactly that host: a provider answer, a
/// member's request or a connector update can never aim the secret at another host (O2).
pub(crate) fn org_client_secret(
    vault: &SecretsVault,
    client: &NodeClient,
    endpoint: &str,
) -> Result<secrecy::SecretString, OrchestratorError> {
    let id = &client.connector_id;
    let not_saved = || {
        OrchestratorError::Conflict(format!(
            "`{id}` has no client secret saved: ask your admin to save the client again"
        ))
    };
    if !client.secret_set {
        return Err(not_saved());
    }
    let host = host_of(endpoint);
    let pinned = client.token_host();
    let (Some(host), Some(pinned)) = (host, pinned) else {
        return Err(OrchestratorError::PermissionDenied(
            "the client secret is only sent to the token endpoint pinned at Enable".into(),
        ));
    };
    if !host.eq_ignore_ascii_case(&pinned) {
        return Err(OrchestratorError::PermissionDenied(
            "the client secret is only sent to the token endpoint pinned at Enable".into(),
        ));
    }
    let binding = client_secret_binding_id(id);
    let meta = match vault.get_metadata_at(SecretScope::Org, ORG_OWNER_USER_ID, &binding) {
        Ok(meta) => meta,
        Err(OrchestratorError::StorageUnavailable) => {
            return Err(OrchestratorError::StorageUnavailable)
        }
        Err(_) => return Err(not_saved()),
    };
    let pinned_to_host = meta.allowed_destinations.len() == 1
        && meta.allowed_destinations[0].eq_ignore_ascii_case(&host);
    if meta.kind != SecretKind::Connection || meta.scope != SecretScope::Org || !pinned_to_host {
        return Err(OrchestratorError::PermissionDenied(
            "the client secret is not pinned to the token endpoint's host".into(),
        ));
    }
    vault.decrypt_secret_at(SecretScope::Org, ORG_OWNER_USER_ID, &binding)
}

/// The body of `POST /v1/connectors/{id}/connection/oauth/code`: what the app got from the browser
/// sign-in. No endpoint and no client field: the node's pinned client is the only one. Holds a code
/// and a verifier: no `Debug` output of them.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CodeExchangeRequest {
    pub code: String,
    pub code_verifier: String,
    pub redirect_uri: String,
}

impl std::fmt::Debug for CodeExchangeRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodeExchangeRequest")
            .field("redirect_uri", &self.redirect_uri)
            .finish_non_exhaustive()
    }
}

/// The app's own redirect URIs (open question 6): `squrell://oauth`, or a loopback address with a
/// port and a path. The admin registers them with the provider.
pub const APP_REDIRECT_URI: &str = "squrell://oauth";
const MAX_CODE_LEN: usize = 4096;

/// Checks a code exchange body before anything is read or sent. Messages never quote a value.
pub fn check_code_exchange(req: &CodeExchangeRequest) -> Result<(), OrchestratorError> {
    let bad = |what: &str| OrchestratorError::InvalidContract(format!("code exchange: {what}"));
    if req.code.trim().is_empty()
        || req.code.len() > MAX_CODE_LEN
        || req.code.bytes().any(|b| b.is_ascii_control() || b == b' ')
    {
        return Err(bad(
            "`code` must be 1-4096 characters with no space or control character",
        ));
    }
    // RFC 7636 §4.1: 43-128 unreserved characters.
    let v = &req.code_verifier;
    if !(43..=128).contains(&v.len())
        || !v
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~'))
    {
        return Err(bad(
            "`codeVerifier` must be 43-128 characters of letters, digits, `-`, `.`, `_`, `~`",
        ));
    }
    if !redirect_uri_is_the_apps(&req.redirect_uri) {
        return Err(bad(
            "`redirectUri` must be the app's: `squrell://oauth` or `http://127.0.0.1:<port>/<path>`",
        ));
    }
    Ok(())
}

fn redirect_uri_is_the_apps(raw: &str) -> bool {
    if raw == APP_REDIRECT_URI {
        return true;
    }
    if raw.len() > 512 {
        return false;
    }
    let Ok(u) = url::Url::parse(raw) else {
        return false;
    };
    u.scheme() == "http"
        && u.host_str() == Some("127.0.0.1")
        && u.port().is_some()
        && u.username().is_empty()
        && u.password().is_none()
        && u.query().is_none()
        && u.fragment().is_none()
        && u.path().starts_with('/')
}

#[cfg(test)]
mod tests {
    use super::*;
    use vam_spec::connector::Connector;

    fn def(client: &str, secret: &str) -> Connector {
        let provider = if client == "org" {
            format!(
                r#"{{ "kind": "oauth2", "profile": "discover", "client": "org", "clientSecret": "{secret}", "scopes": [], "orgConsent": "none" }}"#
            )
        } else {
            r#"{ "kind": "oauth2", "profile": "discover", "clientSecret": "none", "scopes": [], "orgConsent": "none" }"#.to_string()
        };
        let raw = format!(
            r#"{{ "urn": "urn:vam:connector:hub.example/pub/filebox", "version": "1.0.0",
                  "displayName": "Filebox", "publisher": "Pub", "description": "d", "icon": "folder",
                  "provider": {provider},
                  "connection": {{ "holder": "user", "multiAccount": false, "label": "Filebox" }},
                  "mcp": {{ "url": "https://mcp.filebox.example/mcp", "tools": [] }} }}"#
        );
        serde_json::from_str(&raw).unwrap()
    }

    fn org() -> Connector {
        def("org", "org_vault")
    }

    fn req(id: &str, secret: Option<&str>) -> Option<Option<ClientRequest>> {
        Some(Some(ClientRequest {
            client_id: id.into(),
            client_secret: secret.map(Into::into),
        }))
    }

    fn found(token: &str) -> Discovered {
        Discovered {
            issuer: "https://auth.filebox.example".into(),
            authorize_endpoint: "https://auth.filebox.example/authorize".into(),
            token_endpoint: token.into(),
            revocation_endpoint: None,
        }
    }

    fn stored(secret: bool) -> NodeClient {
        NodeClient {
            connector_id: "filebox".into(),
            client_id: "c1".into(),
            issuer: "https://auth.filebox.example".into(),
            authorize_endpoint: "https://auth.filebox.example/authorize".into(),
            token_endpoint: "https://auth.filebox.example/token".into(),
            revocation_endpoint: None,
            secret_set: secret,
            updated_at: 1,
            updated_by: "olga".into(),
        }
    }

    #[test]
    fn enabling_an_org_connector_without_a_client_is_a_conflict_naming_the_client_id() {
        let e = check_request(&org(), None, true, &None).unwrap_err();
        assert!(matches!(e, OrchestratorError::Conflict(_)));
        assert!(e.to_string().contains("client id"), "{e}");
        // Disabled, or a client stored: kept.
        assert_eq!(
            check_request(&org(), None, false, &None).unwrap(),
            ClientIntent::Keep
        );
        assert_eq!(
            check_request(&org(), Some(&stored(true)), true, &None).unwrap(),
            ClientIntent::Keep
        );
    }

    #[test]
    fn a_client_on_a_publisher_client_connector_is_refused_naming_the_client() {
        for field in [req("c1", None), Some(None)] {
            let e = check_request(&def("publisher", "none"), None, false, &field).unwrap_err();
            assert!(matches!(e, OrchestratorError::InvalidContract(_)));
            assert!(e.to_string().contains("client"), "{e}");
        }
        // A publisher-client connector enables with no client, as ever.
        assert_eq!(
            check_request(&def("publisher", "none"), None, true, &None).unwrap(),
            ClientIntent::Keep
        );
    }

    #[test]
    fn a_secret_is_needed_stored_or_given_and_named() {
        let e = check_request(&org(), None, true, &req("c1", None)).unwrap_err();
        assert!(matches!(e, OrchestratorError::InvalidContract(_)));
        assert!(e.to_string().contains("secret"), "{e}");
        // Stored: the same client id keeps its secret; a new client id needs its own secret.
        assert!(check_request(&org(), Some(&stored(true)), true, &req("c1", None)).is_ok());
        let e = check_request(&org(), Some(&stored(true)), true, &req("c2", None)).unwrap_err();
        assert!(e.to_string().contains("secret"), "{e}");
        assert!(check_request(&org(), Some(&stored(false)), true, &req("c2", None)).is_err());
        // A connector that keeps no secret takes none.
        let none = def("org", "none");
        assert!(check_request(&none, None, true, &req("c1", None)).is_ok());
        let e = check_request(&none, None, true, &req("c1", Some("s"))).unwrap_err();
        assert!(e.to_string().contains("no client secret"), "{e}");
    }

    #[test]
    fn client_id_and_secret_syntax_never_quotes_the_value() {
        for bad in ["", "has space", "semi;colon", &"x".repeat(129)] {
            let e = check_request(&org(), None, true, &req(bad, Some("s"))).unwrap_err();
            assert!(e.to_string().contains("client id"), "{e}");
        }
        for bad in ["", "zz-secret-\nvalue", "zz-secret-\0value"] {
            let e = check_request(&org(), None, true, &req("c1", Some(bad))).unwrap_err();
            assert!(!e.to_string().contains("zz-secret"), "{e}");
            assert!(e.to_string().contains("client secret"), "{e}");
        }
    }

    #[test]
    fn a_client_is_removed_only_when_disabling() {
        let e = check_request(&org(), Some(&stored(true)), true, &Some(None)).unwrap_err();
        assert!(e.to_string().contains("enabled: false"), "{e}");
        assert_eq!(
            check_request(&org(), Some(&stored(true)), false, &Some(None)).unwrap(),
            ClientIntent::Clear
        );
        assert_eq!(
            check_request(&org(), None, false, &Some(None)).unwrap(),
            ClientIntent::Keep
        );
    }

    fn set(secret: Option<&str>) -> ClientIntent {
        ClientIntent::Set(ClientRequest {
            client_id: "c1".into(),
            client_secret: secret.map(Into::into),
        })
    }

    #[test]
    fn a_new_secret_alone_keeps_members_and_a_new_client_id_or_endpoint_does_not() {
        let tok = "https://auth.filebox.example/token";
        let plan = |i, d: &str, e: &NodeClient| {
            plan_client(&org(), Some(e), i, Some(found(d)), 5, "olga").unwrap()
        };
        assert!(!plan(set(Some("new")), tok, &stored(true)).reauth);
        let other = ClientIntent::Set(ClientRequest {
            client_id: "c2".into(),
            client_secret: Some("s".into()),
        });
        assert!(plan(other, tok, &stored(true)).reauth);
        assert!(
            plan(
                set(Some("s")),
                "https://auth.filebox.example/token2",
                &stored(true)
            )
            .reauth
        );
        // First save: nobody to mark.
        let first = plan_client(&org(), None, set(Some("s")), Some(found(tok)), 5, "olga").unwrap();
        assert!(!first.reauth);
        match first.write {
            ClientWrite::Set { client, secret } => {
                assert!(client.secret_set);
                assert_eq!(client.updated_by, "olga");
                assert_eq!(secret.as_deref(), Some("s"));
            }
            other => panic!("{other:?}"),
        }
        assert!(
            plan_client(
                &org(),
                Some(&stored(true)),
                ClientIntent::Clear,
                None,
                5,
                "o"
            )
            .unwrap()
            .reauth
        );
    }

    #[test]
    fn a_token_endpoint_that_moved_hosts_needs_the_secret_entered_again() {
        let moved = "https://login.elsewhere.example/token";
        let e = plan_client(
            &org(),
            Some(&stored(true)),
            set(None),
            Some(found(moved)),
            5,
            "olga",
        )
        .unwrap_err();
        assert!(matches!(e, OrchestratorError::Conflict(_)));
        assert!(e.to_string().contains("login.elsewhere.example"), "{e}");
        // With a new secret it is stored, pinned to the new host.
        assert!(plan_client(
            &org(),
            Some(&stored(true)),
            set(Some("s")),
            Some(found(moved)),
            5,
            "olga"
        )
        .is_ok());
    }

    #[test]
    fn the_entry_shows_the_client_but_never_a_secret_and_updated_by_only_to_admins() {
        let base = || {
            json!({
                "node": { "enabled": true, "configured": false },
                "provider": { "kind": "oauth2", "client": "org" }
            })
        };
        let c = stored(true);
        let mut admin = base();
        annotate_client(&mut admin, &org(), Some(&c), true);
        assert_eq!(admin["node"]["client"]["clientId"], "c1");
        assert_eq!(admin["node"]["client"]["secretSet"], true);
        assert_eq!(admin["node"]["client"]["updatedBy"], "olga");
        assert_eq!(admin["node"]["configured"], true);
        assert_eq!(admin["provider"]["clientId"], "c1");
        let mut member = base();
        annotate_client(&mut member, &org(), Some(&c), false);
        assert_eq!(
            member["node"]["client"]["authorizeEndpoint"],
            "https://auth.filebox.example/authorize"
        );
        assert!(member["node"]["client"].get("updatedBy").is_none());
        for v in [&admin, &member] {
            let text = v.to_string();
            assert!(!text.contains("lientSecret"), "{text}");
        }
        // Disabled: a member sees nothing of it, an admin does.
        let mut off = base();
        off["node"]["enabled"] = json!(false);
        let mut m = off.clone();
        annotate_client(&mut m, &org(), Some(&c), false);
        assert!(m["node"].get("client").is_none());
        assert!(m["provider"].get("clientId").is_none());
        annotate_client(&mut off, &org(), Some(&c), true);
        assert!(off["node"].get("client").is_some());
        // A publisher-client connector is untouched.
        let mut pubc = base();
        annotate_client(&mut pubc, &def("publisher", "none"), Some(&c), true);
        assert_eq!(pubc["node"]["configured"], false);
    }

    #[test]
    fn the_request_never_prints_the_secret() {
        let r = ClientRequest {
            client_id: "c1".into(),
            client_secret: Some("super-secret-value".into()),
        };
        assert!(!format!("{r:?}").contains("super-secret-value"));
        assert!(!format!("{:?}", req("c1", Some("super-secret-value"))).contains("super-secret"));
    }

    #[test]
    fn connector_org_rows_are_recognised() {
        let id = client_secret_binding_id("filebox");
        assert_eq!(id, "connector/filebox/oauth/client-secret");
        assert!(is_connector_org_row(SecretScope::Org, &id));
        assert!(is_connector_org_row(
            SecretScope::Org,
            "connector/x/oauth/anything"
        ));
        // A member's own REST token rows and ordinary secrets are not.
        assert!(!is_connector_org_row(SecretScope::User, &id));
        assert!(!is_connector_org_row(SecretScope::Org, "openai/api-key"));
        assert!(!is_connector_org_row(SecretScope::Org, "connectors/x"));
    }
}

/// Fixtures the exchange, refresh and revoke tests share: the `filebox` connector as a marketplace
/// installs it, and its client as an admin saves it, on hosts of one test's own `tag`.
#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use crate::connectors::NodeConnector;

    /// The connector file of the `filebox` listing, on `https://mcp.<tag>.example/mcp`.
    pub(crate) fn install_filebox_rows(store: &SqliteWorkflowStore, tag: &str) {
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
        let urn = json["urn"].as_str().unwrap_or_default().to_string();
        let c = store.connection.lock().unwrap();
        c.execute(
            "INSERT INTO connector_releases (urn, major, minor, patch, digest, spec_bytes, created_at,
                 updated_at, owner_user_id) VALUES (?1, 1, 0, 0, 'd', ?2, 1, 1, 'system')",
            params![urn, json.to_string().as_bytes()],
        )
        .unwrap();
        c.execute(
            "INSERT INTO catalog_origins (kind, urn, major, minor, patch, marketplace_id, listing_id,
                 git_ref, commit_sha, package_digest, installed_at)
             VALUES ('connector', ?1, 1, 0, 0, 'm-1', 'filebox-connector', 'filebox-v1', 'c', 'd', 1)",
            [urn],
        )
        .unwrap();
    }

    /// A member's connected row for `filebox` on `tag`'s hosts: the node's endpoints, `client_id`,
    /// tokens `A-old` / `R-old`, and the one tool of the connector.
    pub(crate) fn member_row(
        store: &SqliteWorkflowStore,
        vault: &SecretsVault,
        tag: &str,
        owner: &str,
        client_id: &str,
        revocation: Option<&str>,
    ) -> crate::mcp_registry::McpConnection {
        use crate::mcp_registry::{
            McpAuthMode, McpOauthMetadata, McpOauthRegistrationMethod, McpOauthStatus,
        };
        let oauth = McpOauthMetadata {
            resource: format!("https://mcp.{tag}.example/mcp"),
            authorization_server: format!("https://auth.{tag}.example"),
            token_endpoint: format!("https://auth.{tag}.example/token"),
            revocation_endpoint: revocation.map(str::to_string),
            client_id: client_id.into(),
            registration_method: McpOauthRegistrationMethod::Manual,
            scopes: vec![],
            expires_at: None,
            status: McpOauthStatus::Connected,
            last_refresh_at: None,
            last_refresh_error: None,
        };
        let tool = crate::mcp_client::McpTool {
            name: "search_files".into(),
            description: None,
            input_schema: serde_json::json!({}),
            ..Default::default()
        };
        let conn = store
            .upsert_mcp_connection(
                SecretScope::User,
                owner,
                "filebox",
                &format!("https://mcp.{tag}.example/mcp"),
                "",
                &[],
                &[tool],
                McpAuthMode::Oauth,
                Some(&oauth),
            )
            .unwrap();
        crate::mcp_registry::store_refreshed_mcp_tokens(vault, &conn, "A-old", Some("R-old"))
            .unwrap();
        conn
    }

    /// The node's client for `filebox` on `tag`'s hosts: enabled, `client_id`, `secret` in the org
    /// vault row pinned to `auth.<tag>.example`. Written the way Enable writes it.
    pub(crate) fn enable_filebox(
        store: &SqliteWorkflowStore,
        vault: &SecretsVault,
        tag: &str,
        client_id: &str,
        secret: &str,
    ) -> NodeClient {
        let client = NodeClient {
            connector_id: "filebox".into(),
            client_id: client_id.into(),
            issuer: format!("https://auth.{tag}.example"),
            authorize_endpoint: format!("https://auth.{tag}.example/authorize"),
            token_endpoint: format!("https://auth.{tag}.example/token"),
            revocation_endpoint: Some(format!("https://auth.{tag}.example/revoke")),
            secret_set: true,
            updated_at: 1,
            updated_by: "olga".into(),
        };
        let node = NodeConnector {
            connector_id: "filebox".into(),
            enabled: true,
            consent_tenant_id: None,
            consent_recorded_at: None,
            consent_recorded_by: None,
            updated_at: 1,
            updated_by: "olga".into(),
        };
        let plan = ClientPlan {
            write: ClientWrite::Set {
                client: Box::new(client.clone()),
                secret: Some(secret.to_string()),
            },
            reauth: false,
        };
        store
            .put_node_with_client(
                vault,
                &node,
                &plan,
                &format!("https://mcp.{tag}.example/mcp"),
            )
            .unwrap();
        client
    }
}

#[cfg(test)]
mod exchange_tests {
    use super::test_support::*;
    use super::*;
    use secrecy::ExposeSecret;
    use std::sync::Arc;

    fn world(tag: &str) -> (Arc<SqliteWorkflowStore>, Arc<SecretsVault>, NodeClient) {
        let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
        let vault = SecretsVault::for_tests(store.clone());
        install_filebox_rows(&store, tag);
        let client = enable_filebox(&store, &vault, tag, "c-1", "ORG-SECRET-value");
        (store, vault, client)
    }

    fn exchange(code: &str, verifier: &str, redirect: &str) -> CodeExchangeRequest {
        CodeExchangeRequest {
            code: code.into(),
            code_verifier: verifier.into(),
            redirect_uri: redirect.into(),
        }
    }

    const VERIFIER: &str = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789-._~";

    #[test]
    fn the_secret_is_read_only_for_the_pinned_token_host() {
        let (_store, vault, client) = world("secret-host");
        let ok = org_client_secret(&vault, &client, &client.token_endpoint).unwrap();
        assert_eq!(ok.expose_secret(), "ORG-SECRET-value");
        // The revocation endpoint is on the token host: allowed. Any other host is refused, whatever
        // asks: no value is read.
        let revoke = client.revocation_endpoint.clone().unwrap();
        assert!(org_client_secret(&vault, &client, &revoke).is_ok());
        for other in [
            "https://evil.example/token",
            "https://auth.secret-host.example.evil.example/token",
            "https://mcp.secret-host.example/token",
            "not a url",
            "",
        ] {
            let e = org_client_secret(&vault, &client, other).unwrap_err();
            assert!(
                matches!(e, OrchestratorError::PermissionDenied(_)),
                "{other}: {e}"
            );
            assert!(!e.to_string().contains("ORG-SECRET"), "{e}");
        }
    }

    #[test]
    fn a_row_not_pinned_to_exactly_the_token_host_is_never_read() {
        let (_store, vault, client) = world("secret-pin");
        let binding = client_secret_binding_id("filebox");
        // A wildcard, a second destination, or another host on the row: refused.
        for dests in [
            vec!["*".to_string()],
            vec!["*.secret-pin.example".to_string()],
            vec![
                "auth.secret-pin.example".to_string(),
                "evil.example".to_string(),
            ],
            vec!["evil.example".to_string()],
        ] {
            vault
                .put_secret(
                    &binding,
                    "ORG-SECRET-value",
                    SecretKind::Connection,
                    &dests,
                    None,
                )
                .unwrap();
            let e = org_client_secret(&vault, &client, &client.token_endpoint).unwrap_err();
            assert!(
                matches!(e, OrchestratorError::PermissionDenied(_)),
                "{dests:?}: {e}"
            );
        }
    }

    #[test]
    fn a_missing_or_unset_secret_is_a_conflict_that_names_the_admin() {
        let (_store, vault, mut client) = world("secret-missing");
        vault
            .delete_secret(&client_secret_binding_id("filebox"))
            .unwrap();
        let e = org_client_secret(&vault, &client, &client.token_endpoint).unwrap_err();
        assert!(matches!(e, OrchestratorError::Conflict(_)), "{e}");
        assert!(e.to_string().contains("admin"), "{e}");
        client.secret_set = false;
        assert!(matches!(
            org_client_secret(&vault, &client, &client.token_endpoint),
            Err(OrchestratorError::Conflict(_))
        ));
    }

    #[test]
    fn a_members_own_row_with_the_same_binding_id_is_never_the_org_secret() {
        let (_store, vault, client) = world("secret-scope");
        let binding = client_secret_binding_id("filebox");
        vault
            .put_secret_with_owner(
                &binding,
                "MEMBER-VALUE",
                SecretKind::Connection,
                &["auth.secret-scope.example".to_string()],
                None,
                "ada",
            )
            .unwrap();
        let got = org_client_secret(&vault, &client, &client.token_endpoint).unwrap();
        assert_eq!(got.expose_secret(), "ORG-SECRET-value");
    }

    #[test]
    fn the_exchange_body_is_checked_before_anything_is_sent() {
        let good = exchange("code-1", VERIFIER, "squrell://oauth");
        assert!(check_code_exchange(&good).is_ok());
        for uri in [
            "http://127.0.0.1:51234/callback",
            "http://127.0.0.1:1/oauth/cb",
            "http://127.0.0.1:5000",
        ] {
            assert!(
                check_code_exchange(&exchange("c", VERIFIER, uri)).is_ok(),
                "{uri}"
            );
        }
        for uri in [
            "",
            "squrell://oauth/extra",
            "squrell://other",
            "https://evil.example/callback",
            "https://127.0.0.1:5000/cb",
            "http://localhost:5000/cb",
            "http://127.0.0.1/cb",
            "http://127.0.0.1:5000/cb?x=1",
            "http://127.0.0.1:5000/cb#f",
            "http://user@127.0.0.1:5000/cb",
            "http://127.0.0.1.evil.example:5000/cb",
            "http://[::1]:5000/cb",
        ] {
            let e = check_code_exchange(&exchange("c", VERIFIER, uri)).unwrap_err();
            assert!(
                matches!(e, OrchestratorError::InvalidContract(_)),
                "{uri}: {e}"
            );
            assert!(e.to_string().contains("redirectUri"), "{uri}: {e}");
        }
        for code in ["", "a b", "a\nb", "a\0b", &"x".repeat(MAX_CODE_LEN + 1)] {
            let e = check_code_exchange(&exchange(code, VERIFIER, "squrell://oauth")).unwrap_err();
            assert!(e.to_string().contains("`code`"), "{e}");
        }
        for verifier in ["short", &"a".repeat(129), &format!("{}!", "a".repeat(50))] {
            let e = check_code_exchange(&exchange("c", verifier, "squrell://oauth")).unwrap_err();
            assert!(e.to_string().contains("codeVerifier"), "{e}");
        }
    }

    #[test]
    fn a_refusal_never_quotes_the_code_or_the_verifier_and_debug_hides_them() {
        let bad = exchange("CODE-VALUE-123", "short-VERIFIER-value", "squrell://oauth");
        let e = check_code_exchange(&bad).unwrap_err();
        for v in ["CODE-VALUE-123", "short-VERIFIER-value"] {
            assert!(!e.to_string().contains(v), "{e}");
            assert!(!format!("{bad:?}").contains(v));
        }
    }

    #[test]
    fn the_body_takes_no_endpoint_and_no_client() {
        for extra in [
            "tokenEndpoint",
            "clientId",
            "clientSecret",
            "resource",
            "extra",
        ] {
            let raw = format!(
                r#"{{ "code": "c", "codeVerifier": "{VERIFIER}", "redirectUri": "squrell://oauth", "{extra}": "x" }}"#
            );
            assert!(
                serde_json::from_str::<CodeExchangeRequest>(&raw).is_err(),
                "{extra}"
            );
        }
        let raw = format!(
            r#"{{ "code": "c", "codeVerifier": "{VERIFIER}", "redirectUri": "squrell://oauth" }}"#
        );
        assert!(serde_json::from_str::<CodeExchangeRequest>(&raw).is_ok());
    }

    #[test]
    fn a_connector_row_uses_the_nodes_client_only_on_the_connectors_own_url() {
        let (store, _vault, client) = world("row-client");
        let mut conn = crate::mcp_registry::McpConnection {
            connection_id: "filebox".into(),
            owner_user_id: "ada".into(),
            scope: SecretScope::User,
            url: "https://mcp.row-client.example/mcp".into(),
            secret_binding_id: String::new(),
            header_bindings: vec![],
            auth_mode: crate::mcp_registry::McpAuthMode::Oauth,
            oauth: None,
            tools: vec![],
            side_effects: Default::default(),
            created_at: 1,
            updated_at: 1,
            ask_tools: vec![],
            created_by_user_id: String::new(),
            timeout_ms: crate::mcp_registry::DEFAULT_MCP_TIMEOUT_MS,
            instructions: None,
            approved_vendor_text: Default::default(),
            effects: Default::default(),
        };
        match store.row_client(&conn).unwrap() {
            RowClient::Org(org) => {
                assert_eq!(org.client.as_ref(), Some(&client));
                assert!(org.keeps_secret);
            }
            other => panic!("{other:?}"),
        }
        // A server the member added by hand under the id, an org-scope row, and another id.
        conn.url = "https://elsewhere.example/mcp".into();
        assert_eq!(store.row_client(&conn).unwrap(), RowClient::Own);
        conn.url = "https://mcp.row-client.example/mcp".into();
        conn.scope = SecretScope::Org;
        assert_eq!(store.row_client(&conn).unwrap(), RowClient::Own);
        conn.scope = SecretScope::User;
        conn.connection_id = "linear".into();
        assert_eq!(store.row_client(&conn).unwrap(), RowClient::Own);
    }
}
