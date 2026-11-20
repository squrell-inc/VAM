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

/// How the node authenticates the org's client secret at the token and revocation endpoints (RFC 6749
/// §2.3.1). Pinned at Enable from the provider's metadata and used by the code exchange, refresh and
/// revoke (the revoke uses its own pinned method when the provider lists one): the secret never
/// moves from one method to another on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TokenAuthMethod {
    /// `client_id` and `client_secret` in the form body.
    #[default]
    ClientSecretPost,
    /// HTTP Basic with the form-urlencoded client id and secret (RFC 8414's default when the
    /// provider's metadata does not say).
    ClientSecretBasic,
}

impl TokenAuthMethod {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ClientSecretPost => "client_secret_post",
            Self::ClientSecretBasic => "client_secret_basic",
        }
    }

    /// An unknown stored value reads as the method every row had before the column existed.
    pub fn parse(value: &str) -> Self {
        match value {
            "client_secret_basic" => Self::ClientSecretBasic,
            _ => Self::ClientSecretPost,
        }
    }
}

/// The reason a member's row carries after a refresh the provider refused for the admin's client
/// (`invalid_client`). An admin saving a corrected secret for the same client clears exactly the
/// rows that carry it; a revoked grant (`invalid_grant`) never does.
pub(crate) const CLIENT_REFUSED_REASON: &str =
    "the provider refused the client id or secret your admin entered";

/// [`CLIENT_REFUSED_REASON`] for a row the refusal found tools-pending (a sign-in stored, its tools
/// not adopted yet). A corrected secret for the same client puts such a row back to
/// [`crate::mcp_registry::TOOLS_PENDING_REASON`], never to `connected`: only the adoption connects
/// it. A save that changes the client replaces it like any recoverable mark.
pub(crate) const CLIENT_REFUSED_PENDING_REASON: &str =
    "the provider refused the client id or secret your admin entered (sign-in stored: its tools are not read yet)";

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
    /// Pinned at Enable (the provider's metadata decides); only used when a secret is kept.
    pub token_auth_method: TokenAuthMethod,
    /// Pinned at Enable from `revocation_endpoint_auth_methods_supported`; `None` (the provider
    /// listed none it can use, or a client saved before it was read) means the revoke uses
    /// [`Self::token_auth_method`].
    pub revocation_auth_method: Option<TokenAuthMethod>,
    pub secret_set: bool,
    pub updated_at: i64,
    pub updated_by: String,
    /// Moves on every write of the client row, a secret-only save and a save that repeats the stored
    /// client included. It never goes back, not even through a removed and re-added client
    /// ([`next_client_revision`]), so a sign-in, a refresh or a save plan that captured it can tell
    /// "still the client I read" from "the same client id again" (A→B→A) or "a newer save in the
    /// same second" (`updated_at` has one-second resolution). Set by
    /// [`SqliteWorkflowStore::put_node_with_client`]; a planned client carries the stored one.
    pub revision: i64,
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
/// - `org_vault` with no secret stored or given: 400 (names the secret), also when the body has no
///   `client` at all and the connector is being enabled (the saved client has no secret, e.g. after
///   an update turned `none` into `org_vault`). A stored secret is kept
///   only for the same (trimmed) client id: a secret belongs to the client it was registered with, so a new
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
            if enabled && org {
                match existing {
                    None => {
                        return Err(OrchestratorError::Conflict(format!(
                            "`{id}` needs your organization's OAuth client id before it can be enabled: \
                             register a client with the provider and send `client: {{ clientId, clientSecret }}`"
                        )));
                    }
                    // A marketplace update can turn a connector with no secret into one that keeps
                    // one while the saved client stays: members would sign in and the exchange
                    // would refuse them. A public client and a disable need no secret.
                    Some(e) if secret_kind(def) == ClientSecret::OrgVault && !e.secret_set => {
                        return Err(invalid(format!(
                            "`{id}` needs the client secret you registered with the provider: send `client.clientSecret`"
                        )));
                    }
                    Some(_) => {}
                }
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
            // The id as `plan_client` stores it, so a padded repeat of the saved id is the same id.
            let client_id = check_client_id(&id, &c.client_id)?;
            match (secret_kind(def), &c.client_secret) {
                (ClientSecret::OrgVault, Some(s)) => check_secret(&id, s)?,
                (ClientSecret::OrgVault, None)
                    if existing.is_some_and(|e| e.secret_set && e.client_id == client_id) => {}
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

/// A new company address needs the client saved again in the same body: a client is registered in
/// one provider instance, so one saved under the old address is never used at the new one (409,
/// nothing written). An address that stays the same, or no client stored, asks for nothing.
pub fn check_address_change(
    def: &Connector,
    stored_address: Option<&str>,
    new_address: Option<&str>,
    client_field: &Option<Option<ClientRequest>>,
    existing: Option<&NodeClient>,
) -> Result<(), OrchestratorError> {
    if def.tenant.is_some()
        && new_address.is_some()
        && new_address != stored_address
        && existing.is_some()
        && client_field.is_none()
    {
        return Err(OrchestratorError::Conflict(format!(
            "`{}` has an OAuth client registered under the previous company address: send `client: {{ clientId }}` again with the new address",
            connector_id(def)
        )));
    }
    Ok(())
}

/// True for a REST connector with fixed sign-in endpoints (`profile: oauth`): no discovery runs.
pub fn is_rest_oauth(def: &Connector) -> bool {
    matches!(&def.provider, Provider::Oauth2(o) if o.profile == vam_spec::connector::OAuthProfile::Oauth)
}

/// What discovery would have answered, taken from the connector file's own endpoints (already
/// filled with the company address). No request is made. The issuer is the token endpoint's origin;
/// a revoke endpoint is kept only on the token endpoint's host; the connector keeps no secret, so
/// the auth methods are the defaults and never used.
pub fn fixed_endpoints(def: &Connector) -> Result<Discovered, OrchestratorError> {
    let id = connector_id(def);
    let Provider::Oauth2(o) = &def.provider else {
        return Err(invalid(format!("`{id}` has no OAuth endpoints")));
    };
    let e = o
        .endpoints
        .as_ref()
        .filter(|e| !crate::connectors::url_needs_address(&e.token))
        .ok_or_else(|| {
            OrchestratorError::Conflict(format!(
                "`{id}` needs your company address before it can be enabled"
            ))
        })?;
    let token_host = host_of(&e.token);
    let revocation_endpoint = e
        .revoke
        .clone()
        .filter(|r| host_of(r).is_some() && host_of(r) == token_host);
    let issuer = format!("https://{}", token_host.unwrap_or_default());
    Ok(Discovered {
        issuer,
        authorize_endpoint: e.authorize.clone(),
        token_endpoint: e.token.clone(),
        revocation_endpoint,
        token_auth_method: TokenAuthMethod::default(),
        revocation_auth_method: None,
    })
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
                token_auth_method: d.token_auth_method,
                revocation_auth_method: d.revocation_auth_method,
                secret_set: keeps
                    && (req.client_secret.is_some() || existing.is_some_and(|e| e.secret_set)),
                updated_at: now,
                updated_by: by.to_string(),
                revision: existing.map_or(0, |e| e.revision),
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
            updated_by TEXT NOT NULL,
            token_auth_method TEXT NOT NULL DEFAULT 'client_secret_post',
            revision INTEGER NOT NULL DEFAULT 0,
            revocation_auth_method TEXT,
            secret_saved_while_off INTEGER
        );
        -- The last client revision a connector was given. Kept when its client is removed, so a
        -- client saved again later never reuses a revision a sign-in or a refresh captured.
        CREATE TABLE IF NOT EXISTS node_connector_client_revisions (
            connector_id TEXT PRIMARY KEY,
            last_revision INTEGER NOT NULL
        );
        -- One row per code exchange the node was about to send (CN-5.3), written before the code
        -- goes to the provider: a request repeated with the same Idempotency-Key never sends its
        -- code again. The key is stored as its SHA-256.
        CREATE TABLE IF NOT EXISTS connector_code_exchanges (
            owner_user_id TEXT NOT NULL,
            connector_id TEXT NOT NULL,
            key_digest TEXT NOT NULL,
            created_at INTEGER NOT NULL,
            PRIMARY KEY (owner_user_id, connector_id, key_digest)
        );
        ",
    )?;
    // A store from before the method was pinned: every saved client posts its secret, as it did.
    let has_method: i64 = connection.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('node_connector_clients')
         WHERE name = 'token_auth_method'",
        [],
        |r| r.get(0),
    )?;
    if has_method == 0 {
        connection.execute(
            "ALTER TABLE node_connector_clients
             ADD COLUMN token_auth_method TEXT NOT NULL DEFAULT 'client_secret_post'",
            [],
        )?;
    }
    // A store from before the client revision: every saved client starts at 0, and the first save
    // that changes it moves it to 1 or more.
    let has_revision: i64 = connection.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('node_connector_clients') WHERE name = 'revision'",
        [],
        |r| r.get(0),
    )?;
    if has_revision == 0 {
        connection.execute(
            "ALTER TABLE node_connector_clients ADD COLUMN revision INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    // A store from before the revocation method was read: NULL, the revoke keeps the token method.
    let has_revocation_method: i64 = connection.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('node_connector_clients')
         WHERE name = 'revocation_auth_method'",
        [],
        |r| r.get(0),
    )?;
    if has_revocation_method == 0 {
        connection.execute(
            "ALTER TABLE node_connector_clients ADD COLUMN revocation_auth_method TEXT",
            [],
        )?;
    }
    // A store from before the pending-correction marker: NULL, no secret is waiting for an enable.
    let has_off_marker: i64 = connection.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('node_connector_clients')
         WHERE name = 'secret_saved_while_off'",
        [],
        |r| r.get(0),
    )?;
    if has_off_marker == 0 {
        connection.execute(
            "ALTER TABLE node_connector_clients ADD COLUMN secret_saved_while_off INTEGER",
            [],
        )?;
    }
    Ok(())
}

/// The next client revision for `connector_id`, inside the caller's transaction: one more than any
/// revision the connector's client ever had, a removed client's included.
fn next_client_revision(c: &Connection, connector_id: &str) -> Result<i64, OrchestratorError> {
    c.execute(
        "INSERT INTO node_connector_client_revisions (connector_id, last_revision)
         VALUES (?1, COALESCE((SELECT revision FROM node_connector_clients
                               WHERE connector_id = ?1), 0) + 1)
         ON CONFLICT(connector_id) DO UPDATE SET last_revision = MAX(last_revision,
             COALESCE((SELECT revision FROM node_connector_clients WHERE connector_id = ?1), 0)) + 1",
        params![connector_id],
    )?;
    Ok(c.query_row(
        "SELECT last_revision FROM node_connector_client_revisions WHERE connector_id = ?1",
        params![connector_id],
        |r| r.get(0),
    )?)
}

const COLUMNS: &str = "connector_id, client_id, issuer, authorize_endpoint, token_endpoint, \
                       revocation_endpoint, secret_set, updated_at, updated_by, token_auth_method, \
                       revision, revocation_auth_method";

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
        token_auth_method: TokenAuthMethod::parse(&r.get::<_, String>(9)?),
        revision: r.get(10)?,
        revocation_auth_method: r
            .get::<_, Option<String>>(11)?
            .as_deref()
            .map(TokenAuthMethod::parse),
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

/// A connector's saved client, read on a connection the caller already holds.
pub(crate) fn client_in(c: &Connection, id: &str) -> Result<Option<NodeClient>, OrchestratorError> {
    Ok(c.query_row(
        &format!("SELECT {COLUMNS} FROM node_connector_clients WHERE connector_id = ?1"),
        params![id],
        row_to_client,
    )
    .optional()?)
}

/// Whether a secret was saved for the connector's client while the connector was disabled, and no
/// enable has acted on it yet (see [`SqliteWorkflowStore::put_node_with_client`]).
fn secret_saved_while_off_in(
    c: &Connection,
    connector_id: &str,
) -> Result<bool, OrchestratorError> {
    Ok(c.query_row(
        "SELECT secret_saved_while_off FROM node_connector_clients WHERE connector_id = ?1",
        params![connector_id],
        |r| r.get::<_, Option<i64>>(0),
    )
    .optional()?
    .flatten()
    .is_some_and(|v| v != 0))
}

fn set_secret_saved_while_off_in(
    c: &Connection,
    connector_id: &str,
    on: bool,
) -> Result<(), OrchestratorError> {
    c.execute(
        "UPDATE node_connector_clients SET secret_saved_while_off = ?2 WHERE connector_id = ?1",
        params![connector_id, on.then_some(1i64)],
    )?;
    Ok(())
}

/// What a save's plan assumed about the stored state when it was made: the connector's definition
/// and the client the plan kept the secret of (or replaced). [`put_node_with_client`] checks both
/// again inside its transaction: discovery, between the plan and the write, can take seconds, and
/// another admin's save can commit in that time.
pub(crate) struct PlanBasis<'a> {
    /// The connector file as published (the template, not filled with an address).
    pub def: &'a Connector,
    pub client: Option<&'a NodeClient>,
    /// The company address stored when the plan was made.
    pub tenant: Option<&'a str>,
}

/// The text of the 409 when the state moved under a save.
pub(crate) const CLIENT_CHANGED_WHILE_SAVING: &str = "the client changed while saving; save again";

/// Whether two reads of a connector's client are the same state for a plan. The revision decides
/// it: every client write moves it, so a secret-only save in the same second as the plan's read
/// (which leaves `updated_at` as it was) still makes the plan stale, and a plan that keeps or
/// clears "the stored client" acts only on the one it read.
fn same_client_state(a: Option<&NodeClient>, b: Option<&NodeClient>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => {
            a.client_id == b.client_id
                && a.token_endpoint == b.token_endpoint
                && a.secret_set == b.secret_set
                && a.updated_at == b.updated_at
                && a.revision == b.revision
        }
        _ => false,
    }
}

/// How long a code exchange's Idempotency-Key is remembered. An authorization code lives minutes;
/// a week covers any retry of a lost answer by far.
const CODE_EXCHANGE_KEY_TTL_SECS: i64 = 7 * 24 * 60 * 60;

impl SqliteWorkflowStore {
    /// Records that `owner`'s code exchange for `connector_id` with this Idempotency-Key is about to
    /// send its code. `false`: a request with the same key already got this far, so its code was
    /// (or is being) sent, and this one must not send it again (RFC 6749 §4.1.2: a provider may
    /// revoke the tokens of a code that is presented twice). Keys older than
    /// [`CODE_EXCHANGE_KEY_TTL_SECS`] are forgotten here.
    pub(crate) fn claim_code_exchange(
        &self,
        owner: &str,
        connector_id: &str,
        idempotency_key: &str,
        now: i64,
    ) -> Result<bool, OrchestratorError> {
        use sha2::{Digest, Sha256};
        let digest = format!("{:x}", Sha256::digest(idempotency_key.as_bytes()));
        let c = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        c.execute(
            "DELETE FROM connector_code_exchanges WHERE created_at < ?1",
            params![now.saturating_sub(CODE_EXCHANGE_KEY_TTL_SECS)],
        )?;
        let inserted = c.execute(
            "INSERT OR IGNORE INTO connector_code_exchanges
                 (owner_user_id, connector_id, key_digest, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![owner, connector_id, digest, now],
        )?;
        Ok(inserted == 1)
    }

    pub fn node_client(&self, id: &str) -> Result<Option<NodeClient>, OrchestratorError> {
        let c = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        client_in(&c, id)
    }

    /// The node row and the client of `id`, in one read: never a node row of one save with the
    /// client of another.
    pub(crate) fn node_connector_and_client(
        &self,
        id: &str,
    ) -> Result<(Option<crate::connectors::NodeConnector>, Option<NodeClient>), OrchestratorError>
    {
        let c = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        Ok((
            crate::connectors::node_connector_in(&c, id)?,
            client_in(&c, id)?,
        ))
    }

    /// Every node row and every client, in one read (the listing's snapshot).
    pub(crate) fn node_connectors_and_clients(
        &self,
    ) -> Result<(Vec<crate::connectors::NodeConnector>, Vec<NodeClient>), OrchestratorError> {
        let c = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let nodes = crate::connectors::node_connectors_in(&c)?;
        let mut stmt = c.prepare(&format!(
            "SELECT {COLUMNS} FROM node_connector_clients ORDER BY connector_id"
        ))?;
        let clients = stmt
            .query_map([], row_to_client)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok((nodes, clients))
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
    /// The transaction first checks that the connector's definition and its stored client are what
    /// `basis` says the plan was made from; if either moved, nothing is written and the answer is a
    /// 409 ([`CLIENT_CHANGED_WHILE_SAVING`]).
    ///
    /// A save of a corrected secret for the unchanged client also clears the rows a refresh the
    /// provider refused for that client (`invalid_client`) put into `needs_reauth`, and only those:
    /// a revoked grant still needs a sign-in. That happens only when the save leaves the connector
    /// enabled. A save that stores a secret and leaves the connector disabled clears nothing; it
    /// sets the client's `secret_saved_while_off` marker instead, and the save that enables the
    /// connector again (the client kept or repeated) clears the refused rows only when that marker
    /// is set. A plain disable and enable with no secret saved in between clears nothing: the
    /// provider refused the secret that is still saved, so the members stay parked. Any save that
    /// leaves the connector enabled, or changes the client, resets the marker. The owners of the
    /// cleared rows are returned so the caller can continue their parked runs.
    ///
    /// This is the only code that writes `connector/<id>/oauth/client-secret`.
    pub(crate) fn put_node_with_client(
        &self,
        vault: &SecretsVault,
        node: &crate::connectors::NodeConnector,
        plan: &ClientPlan,
        mcp_url: &str,
        basis: &PlanBasis<'_>,
    ) -> Result<NodeSave, OrchestratorError> {
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
        let def_now = crate::connectors::entry_in(&tx, &node.connector_id)?.map(|e| e.template);
        let stored = client_in(&tx, &node.connector_id)?;
        let stored_tenant = crate::connectors::node_connector_in(&tx, &node.connector_id)?
            .and_then(|n| n.tenant_subdomain);
        if def_now.as_ref() != Some(basis.def)
            || !same_client_state(stored.as_ref(), basis.client)
            || stored_tenant.as_deref() != basis.tenant
        {
            return Err(OrchestratorError::Conflict(
                CLIENT_CHANGED_WHILE_SAVING.into(),
            ));
        }
        let was_enabled = crate::connectors::node_enabled_in(&tx, &node.connector_id)?;
        crate::connectors::put_node_connector_in(&tx, node)?;
        match &plan.write {
            ClientWrite::Keep => {}
            ClientWrite::Clear => {
                clear_client_in(&tx, &node.connector_id)?;
            }
            ClientWrite::Set { client, .. } => {
                let revision = next_client_revision(&tx, &node.connector_id)?;
                tx.execute(
                    &format!(
                        "INSERT INTO node_connector_clients ({COLUMNS})
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
                         ON CONFLICT(connector_id) DO UPDATE SET client_id = excluded.client_id,
                             issuer = excluded.issuer,
                             authorize_endpoint = excluded.authorize_endpoint,
                             token_endpoint = excluded.token_endpoint,
                             revocation_endpoint = excluded.revocation_endpoint,
                             secret_set = excluded.secret_set,
                             updated_at = excluded.updated_at, updated_by = excluded.updated_by,
                             token_auth_method = excluded.token_auth_method,
                             revision = excluded.revision,
                             revocation_auth_method = excluded.revocation_auth_method"
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
                        client.updated_by,
                        client.token_auth_method.as_str(),
                        revision,
                        client.revocation_auth_method.map(TokenAuthMethod::as_str)
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
        let mut cleared = Vec::new();
        let secret_saved = matches!(
            &plan.write,
            ClientWrite::Set {
                secret: Some(_),
                ..
            }
        );
        // Read after the client write: an upsert keeps the column, a new client row has NULL.
        let saved_while_off = secret_saved_while_off_in(&tx, &node.connector_id)?;
        if plan.reauth {
            crate::mcp_registry::mark_connector_rows_need_reauth(
                &tx,
                &node.connector_id,
                &[mcp_url],
                "the admin changed the client: reconnect",
            )?;
            // A REST connector's members hold grants of the client being replaced or cleared too
            // (`oauth` profile, CN-6.3): their rows read `needs_reconnect`, and the refresh token is
            // never sent for them again.
            crate::connectors::mark_rest_connections_need_reconnect_in(
                &tx,
                &node.connector_id,
                "the admin changed the client: reconnect",
                node.updated_at,
            )?;
            set_secret_saved_while_off_in(&tx, &node.connector_id, false)?;
        } else if !node.enabled {
            // A secret saved on a disabled connector waits for the enable; a save that keeps the
            // secret leaves an earlier one waiting.
            if secret_saved {
                set_secret_saved_while_off_in(&tx, &node.connector_id, true)?;
            }
        } else if secret_saved || (!was_enabled && saved_while_off) {
            // The same client with a new secret: a refresh the old secret failed is worth another
            // try, so the rows it parked are back (the next call refreshes with the new secret).
            // Only on a save that leaves the connector enabled: a save that corrects the secret
            // and disables the connector leaves the rows parked, with their notifications open,
            // and the enable that follows (with the client kept) is the save that clears them. An
            // enable with no secret saved while the connector was off is not a correction: the
            // provider refused the secret that is still saved.
            set_secret_saved_while_off_in(&tx, &node.connector_id, false)?;
            cleared = crate::mcp_registry::clear_refused_client_rows(
                &tx,
                &node.connector_id,
                &[mcp_url],
                CLIENT_REFUSED_REASON,
            )?;
            for owner in &cleared {
                crate::notifications::resolve_connection_needs_reauth(
                    &tx,
                    owner,
                    &node.connector_id,
                    "acted",
                )?;
            }
        }
        // What this save committed, read in its own transaction: the answer to the save shows
        // exactly this, whatever another save writes after the commit.
        let saved_node = crate::connectors::node_connector_in(&tx, &node.connector_id)?
            .unwrap_or_else(|| node.clone());
        let saved_client = client_in(&tx, &node.connector_id)?;
        tx.commit()?;
        Ok(NodeSave {
            cleared,
            node: saved_node,
            client: saved_client,
        })
    }
}

/// What [`SqliteWorkflowStore::put_node_with_client`] committed.
#[derive(Debug)]
pub(crate) struct NodeSave {
    /// Members whose refused-client mark the save cleared (their parked runs go on).
    pub(crate) cleared: Vec<String>,
    /// The node row as committed.
    pub(crate) node: crate::connectors::NodeConnector,
    /// The client as committed (`None`: the connector has none).
    pub(crate) client: Option<NodeClient>,
}

// ---- what the node does with the client (CN-5.3, CN-5.4) ----------------------------------------

/// A connector with an admin-supplied client, and the client the admin saved for it (if any).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct OrgConnector {
    /// The connector's MCP URL; `None` for a REST connector (`profile: oauth`, CN-6.3).
    pub(crate) mcp_url: Option<String>,
    /// `clientSecret: "org_vault"`: the node holds a secret. Either way the node does the code
    /// exchange for an org client; without a secret it sends none.
    pub(crate) keeps_secret: bool,
    pub(crate) scopes: Vec<String>,
    pub(crate) client: Option<NodeClient>,
    /// A REST connector's company address as the node row holds it (the one the endpoints and
    /// hosts were filled with); `None` for an MCP connector or when no address is set.
    pub(crate) tenant_subdomain: Option<String>,
    /// A REST connector's token endpoint as the connector file names it now, filled with the
    /// address (`None` while it is still templated). The node client's pinned endpoint must equal it
    /// before a code or a refresh token is sent.
    pub(crate) filled_token_endpoint: Option<String>,
}

impl OrgConnector {
    /// True for a REST connector (`profile: oauth`): its connection is a `connector_connections`
    /// row, not an MCP row.
    pub(crate) fn is_rest(&self) -> bool {
        self.mcp_url.is_none()
    }
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
    /// The connector with this id when it uses the admin's client (`provider.client: org`): an MCP
    /// connector, or a REST connector with fixed endpoints (`profile: oauth`). `None` for every other
    /// connector id.
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
        let scopes = match &entry.def.provider {
            Provider::Oauth2(o) => o.scopes.clone(),
            Provider::ApiKey(_) => Vec::new(),
        };
        let (mcp_url, tenant_subdomain, filled_token_endpoint) =
            if let Some(policy) = crate::connectors::policy_of(id, &entry.def) {
                (Some(policy.url), None, None)
            } else if is_rest_oauth(&entry.def) {
                let filled = match &entry.def.provider {
                    Provider::Oauth2(o) => o
                        .endpoints
                        .as_ref()
                        .map(|e| e.token.clone())
                        .filter(|t| !crate::connectors::url_needs_address(t)),
                    Provider::ApiKey(_) => None,
                };
                let address = self.node_connector(id)?.and_then(|n| n.tenant_subdomain);
                (None, address, filled)
            } else {
                return Ok(None);
            };
        Ok(Some(OrgConnector {
            mcp_url,
            keeps_secret: keeps_secret(&entry.def),
            scopes,
            client: self.node_client(id)?,
            tenant_subdomain,
            filled_token_endpoint,
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
            Some(org) if org.mcp_url.as_deref() == Some(conn.url.as_str()) => {
                RowClient::Org(Box::new(org))
            }
            _ => RowClient::Own,
        })
    }

    /// Whose client revokes a member's MCP row's refresh token. As [`Self::row_client`], and also a
    /// user-scope row on another URL than the connector's (the connector's server moved) whose
    /// grant was issued to the node's current client: the same client id and token endpoint. That
    /// grant belongs to the admin's client, so a provider that requires client authentication at
    /// revocation (RFC 7009 §2.1) needs the org secret to revoke it; sent without it, the revoke is
    /// refused while the row is deleted, and the token stays valid at the provider.
    ///
    /// Refresh keeps [`Self::row_client`]: a row on the connector's old URL was marked
    /// `needs_reauth` when the server moved, is never refreshed, and is replaced by the next
    /// sign-in.
    pub(crate) fn revoke_client(
        &self,
        conn: &crate::mcp_registry::McpConnection,
    ) -> Result<RowClient, OrchestratorError> {
        let own = self.row_client(conn)?;
        if matches!(own, RowClient::Org(_)) || conn.scope != SecretScope::User {
            return Ok(own);
        }
        let Some(grant) = conn.oauth.as_ref() else {
            return Ok(own);
        };
        Ok(match self.org_connector(&conn.connection_id)? {
            Some(org)
                if !org.is_rest()
                    && org.client.as_ref().is_some_and(|node| {
                        node.client_id == grant.client_id
                            && node.token_endpoint == grant.token_endpoint
                    }) =>
            {
                RowClient::Org(Box::new(org))
            }
            _ => own,
        })
    }
}

/// The text of the 409 when the admin saved the client again between the read of the client and the
/// read of its secret (after the connector id).
pub(crate) const SECRET_READ_SUPERSEDED: &str =
    "was saved again while its secret was being read: try again";

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
    // The row's destination is judged on the same stored state the ciphertext is read from: a
    // secret an admin save replaced in between is never decrypted against the old host's check.
    let accept = |meta: &crate::secrets::HostSecretMetadata| {
        let pinned_to_host = meta.allowed_destinations.len() == 1
            && meta.allowed_destinations[0].eq_ignore_ascii_case(&host);
        if meta.kind != SecretKind::Connection || meta.scope != SecretScope::Org || !pinned_to_host
        {
            return Err(OrchestratorError::PermissionDenied(
                "the client secret is not pinned to the token endpoint's host".into(),
            ));
        }
        Ok(())
    };
    let secret = match vault.decrypt_secret_at_checked(
        SecretScope::Org,
        ORG_OWNER_USER_ID,
        &binding,
        accept,
    ) {
        Err(OrchestratorError::NotFound(_)) => return Err(not_saved()),
        other => other?,
    };
    // The client and its secret are two reads: a save between them could pair `client`'s id with
    // the next client's secret. Every save moves the revision and the revision never goes back, so
    // the client still at `client.revision` after the secret was read means the secret is that
    // client's. Otherwise nothing is sent.
    let current = vault.workflow_store().node_client(id)?;
    if current.as_ref().map(|c| c.revision) != Some(client.revision) {
        return Err(OrchestratorError::Conflict(format!(
            "the OAuth client for `{id}` {SECRET_READ_SUPERSEDED}"
        )));
    }
    Ok(secret)
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

/// The app's own redirect URIs (open question 6): `squrell://oauth`, or a loopback address with an
/// explicit port other than 80 (the URL parser drops a default port, so `:80` reads as none) and a
/// path. The admin registers them with the provider.
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
            token_auth_method: TokenAuthMethod::ClientSecretPost,
            revocation_auth_method: None,
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
            token_auth_method: TokenAuthMethod::ClientSecretPost,
            revocation_auth_method: None,
            secret_set: secret,
            updated_at: 1,
            updated_by: "olga".into(),
            revision: 1,
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
    fn enabling_a_kept_client_that_has_no_secret_is_refused_naming_the_secret() {
        // A marketplace update can turn `clientSecret: none` into `org_vault` while the saved client
        // stays: it has no secret, and members would sign in only to be refused at the exchange.
        let e = check_request(&org(), Some(&stored(false)), true, &None).unwrap_err();
        assert!(matches!(e, OrchestratorError::InvalidContract(_)), "{e}");
        assert!(e.to_string().contains("secret"), "{e}");
        // A kept client with its secret enables; a disable needs none.
        assert_eq!(
            check_request(&org(), Some(&stored(true)), true, &None).unwrap(),
            ClientIntent::Keep
        );
        assert_eq!(
            check_request(&org(), Some(&stored(false)), false, &None).unwrap(),
            ClientIntent::Keep
        );
        // A public client (`org` with no secret kept) needs none either.
        assert_eq!(
            check_request(&def("org", "none"), Some(&stored(false)), true, &None).unwrap(),
            ClientIntent::Keep
        );
    }

    #[test]
    fn a_padded_repeat_of_the_saved_client_id_keeps_its_secret_and_another_id_does_not() {
        // The first save stored the id trimmed, so a padded repeat is the same id.
        assert!(check_request(&org(), Some(&stored(true)), true, &req("  c1\t", None)).is_ok());
        let e = check_request(&org(), Some(&stored(true)), true, &req(" c2 ", None)).unwrap_err();
        assert!(e.to_string().contains("secret"), "{e}");
        // The plan stores the trimmed id, and a same-id repeat does not mark members for reconnect.
        let tok = "https://auth.filebox.example/token";
        let padded = ClientIntent::Set(ClientRequest {
            client_id: "  c1 ".into(),
            client_secret: None,
        });
        let plan = plan_client(
            &org(),
            Some(&stored(true)),
            padded,
            Some(found(tok)),
            5,
            "o",
        )
        .unwrap();
        assert!(!plan.reauth);
        match plan.write {
            ClientWrite::Set { client, .. } => assert_eq!(client.client_id, "c1"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_store_from_before_the_method_was_pinned_reads_every_saved_client_as_post() {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE node_connector_clients (
                connector_id TEXT PRIMARY KEY, client_id TEXT NOT NULL, issuer TEXT NOT NULL,
                authorize_endpoint TEXT NOT NULL, token_endpoint TEXT NOT NULL,
                revocation_endpoint TEXT, secret_set INTEGER NOT NULL,
                updated_at INTEGER NOT NULL, updated_by TEXT NOT NULL);
             INSERT INTO node_connector_clients VALUES
                ('filebox', 'c1', 'https://auth.filebox.example', 'https://auth.filebox.example/a',
                 'https://auth.filebox.example/token', NULL, 1, 1, 'olga');",
        )
        .unwrap();
        initialize_schema(&c).unwrap();
        // Running it again is fine, and the saved client now says what it always did.
        initialize_schema(&c).unwrap();
        let saved = client_in(&c, "filebox").unwrap().unwrap();
        assert_eq!(saved.token_auth_method, TokenAuthMethod::ClientSecretPost);
        // No revocation method was read for it: the revoke keeps the token method.
        assert_eq!(saved.revocation_auth_method, None);
        assert_eq!(saved.client_id, "c1");
        // And at revision 0: the first save that changes it moves it on.
        assert_eq!(saved.revision, 0);
        assert_eq!(next_client_revision(&c, "filebox").unwrap(), 1);
        assert_eq!(
            TokenAuthMethod::parse("client_secret_basic").as_str(),
            "client_secret_basic"
        );
        assert_eq!(
            TokenAuthMethod::parse("anything else"),
            TokenAuthMethod::ClientSecretPost
        );
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

    /// The `zendesk`-shaped company-address connector (`profile: oauth`, `{subdomain}` under
    /// `zendesk.com`, one REST binding, no secret) as a marketplace installs it.
    pub(crate) fn install_company_address_rows(store: &SqliteWorkflowStore) {
        install_company_address_variant(store, |_| {});
    }

    /// [`install_company_address_rows`] with the file edited by `edit` first.
    pub(crate) fn install_company_address_variant(
        store: &SqliteWorkflowStore,
        edit: impl FnOnce(&mut serde_json::Value),
    ) {
        let raw = include_str!("../../vam-spec/tests/fixtures/zendesk-shaped-connector.json");
        let mut json: serde_json::Value = serde_json::from_str(raw).unwrap();
        edit(&mut json);
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
             VALUES ('connector', ?1, 1, 0, 0, 'm-1', 'zendesk-connector', 'zendesk-v1', 'c', 'd', 1)",
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
            token_auth_method: TokenAuthMethod::ClientSecretPost,
            revocation_auth_method: None,
            secret_set: true,
            updated_at: 1,
            updated_by: "olga".into(),
            revision: 0,
        };
        let node = NodeConnector {
            tenant_subdomain: None,
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
        write_node(store, vault, &node, &plan, tag);
        store.node_client("filebox").unwrap().unwrap()
    }

    /// Pins `method` on the saved client of `filebox`, the way an Enable against a provider that
    /// asks for it does.
    pub(crate) fn pin_token_auth_method(store: &SqliteWorkflowStore, method: TokenAuthMethod) {
        store
            .connection
            .lock()
            .unwrap()
            .execute(
                "UPDATE node_connector_clients SET token_auth_method = ?1, revision = revision + 1
                 WHERE connector_id = 'filebox'",
                [method.as_str()],
            )
            .unwrap();
    }

    /// `put_node_with_client` against the state as it is now, the way a save with no other admin in
    /// the way does it.
    pub(crate) fn write_node(
        store: &SqliteWorkflowStore,
        vault: &SecretsVault,
        node: &NodeConnector,
        plan: &ClientPlan,
        tag: &str,
    ) -> Vec<String> {
        let def = store.connector_entry("filebox").unwrap().unwrap().def;
        let current = store.node_client("filebox").unwrap();
        store
            .put_node_with_client(
                vault,
                node,
                plan,
                &format!("https://mcp.{tag}.example/mcp"),
                &PlanBasis {
                    tenant: None,
                    def: &def,
                    client: current.as_ref(),
                },
            )
            .unwrap()
            .cleared
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

    fn discovered(tag: &str, method: TokenAuthMethod) -> Discovered {
        Discovered {
            issuer: format!("https://auth.{tag}.example"),
            authorize_endpoint: format!("https://auth.{tag}.example/authorize"),
            token_endpoint: format!("https://auth.{tag}.example/token"),
            revocation_endpoint: Some(format!("https://auth.{tag}.example/revoke")),
            token_auth_method: method,
            revocation_auth_method: None,
        }
    }

    fn node(enabled: bool) -> crate::connectors::NodeConnector {
        crate::connectors::NodeConnector {
            tenant_subdomain: None,
            connector_id: "filebox".into(),
            enabled,
            consent_tenant_id: None,
            consent_recorded_at: None,
            consent_recorded_by: None,
            updated_at: 2,
            updated_by: "olga".into(),
        }
    }

    fn set(
        def: &Connector,
        existing: Option<&NodeClient>,
        tag: &str,
        id: &str,
        secret: Option<&str>,
    ) -> ClientPlan {
        let request = Some(Some(ClientRequest {
            client_id: id.into(),
            client_secret: secret.map(Into::into),
        }));
        let intent = check_request(def, existing, true, &request).unwrap();
        plan_client(
            def,
            existing,
            intent,
            Some(discovered(tag, TokenAuthMethod::ClientSecretPost)),
            5,
            "olga",
        )
        .unwrap()
    }

    fn org_secret_of(vault: &SecretsVault) -> String {
        vault
            .decrypt_secret_at(
                SecretScope::Org,
                ORG_OWNER_USER_ID,
                &client_secret_binding_id("filebox"),
            )
            .unwrap()
            .expose_secret()
            .clone()
    }

    #[test]
    fn a_save_planned_before_another_admins_save_commits_nothing() {
        let tag = "save-race";
        let (store, vault, _c1) = world(tag);
        let mcp_url = format!("https://mcp.{tag}.example/mcp");
        let def = store.connector_entry("filebox").unwrap().unwrap().def;
        // Admin A reads the saved client (c-1) and plans "save c-1, keep the stored secret".
        let read_by_a = store.node_client("filebox").unwrap();
        let plan_a = set(&def, read_by_a.as_ref(), tag, "c-1", None);
        assert!(matches!(
            &plan_a.write,
            ClientWrite::Set { secret: None, .. }
        ));
        // Admin B saves c-2 with its own secret while A's discovery is running, and commits first.
        let plan_b = set(&def, read_by_a.as_ref(), tag, "c-2", Some("B-SECRET"));
        write_node(&store, &vault, &node(true), &plan_b, tag);
        assert_eq!(org_secret_of(&vault), "B-SECRET");

        // A commits last, with what it read: refused, and nothing of A's is written (A's plan would
        // have restored c-1 beside B's secret).
        let basis = PlanBasis {
            tenant: None,
            def: &def,
            client: read_by_a.as_ref(),
        };
        let e = store
            .put_node_with_client(&vault, &node(true), &plan_a, &mcp_url, &basis)
            .unwrap_err();
        assert!(matches!(e, OrchestratorError::Conflict(_)), "{e}");
        assert!(
            e.to_string().contains("changed while saving; save again"),
            "{e}"
        );
        let now = store.node_client("filebox").unwrap().unwrap();
        assert_eq!(now.client_id, "c-2");
        assert_eq!(org_secret_of(&vault), "B-SECRET");

        // The same holds when the connector's definition moved under the plan (an update installed).
        let read_now = store.node_client("filebox").unwrap();
        let plan_c = set(&def, read_now.as_ref(), tag, "c-2", None);
        store
            .connection
            .lock()
            .unwrap()
            .execute(
                "UPDATE connector_releases
                 SET spec_bytes = CAST(REPLACE(CAST(spec_bytes AS TEXT), 'Files.', 'Other files.') AS BLOB)",
                [],
            )
            .unwrap();
        let basis = PlanBasis {
            tenant: None,
            def: &def,
            client: read_now.as_ref(),
        };
        let e = store
            .put_node_with_client(&vault, &node(true), &plan_c, &mcp_url, &basis)
            .unwrap_err();
        assert!(e.to_string().contains("changed while saving"), "{e}");
        // With the state it now has, the same save goes through.
        let def_now = store.connector_entry("filebox").unwrap().unwrap().def;
        let basis = PlanBasis {
            tenant: None,
            def: &def_now,
            client: read_now.as_ref(),
        };
        store
            .put_node_with_client(&vault, &node(true), &plan_c, &mcp_url, &basis)
            .unwrap();
    }

    #[test]
    fn a_corrected_secret_for_the_same_client_clears_only_the_rows_a_refused_client_parked() {
        let tag = "fix-secret";
        let (store, vault, client) = world(tag);
        let member = |who: &str| member_row(&store, &vault, tag, who, "c-1", None);
        let (ada, bob, _cy) = (member("ada"), member("bob"), member("cy"));
        // ada's refresh was refused for the admin's client; bob's grant was revoked; cy is fine.
        store
            .record_mcp_oauth_refresh_failure(&ada, true, CLIENT_REFUSED_REASON)
            .unwrap();
        store
            .record_mcp_oauth_refresh_failure(&bob, true, "invalid_grant")
            .unwrap();
        let open_notice = |who: &str| -> i64 {
            store
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
        };
        assert_eq!((open_notice("ada"), open_notice("bob")), (1, 1));

        // The admin saves a corrected secret for the same client.
        let def = store.connector_entry("filebox").unwrap().unwrap().def;
        let plan = set(&def, Some(&client), tag, "c-1", Some("CORRECTED"));
        assert!(!plan.reauth);
        let cleared = write_node(&store, &vault, &node(true), &plan, tag);
        assert_eq!(cleared, ["ada"]);
        let status = |who: &str| {
            let row = store
                .get_mcp_connection(SecretScope::User, who, "filebox")
                .unwrap();
            let oauth = row.oauth.unwrap();
            (oauth.status, oauth.last_refresh_error)
        };
        use crate::mcp_registry::McpOauthStatus::{Connected, NeedsReauth};
        assert_eq!(status("ada"), (Connected, None));
        assert_eq!(status("cy"), (Connected, None));
        // A revoked grant still needs a sign-in, whatever the admin saves.
        assert_eq!(
            status("bob"),
            (NeedsReauth, Some("invalid_grant".to_string()))
        );
        assert_eq!((open_notice("ada"), open_notice("bob")), (0, 1));
        assert_eq!(org_secret_of(&vault), "CORRECTED");

        // A save of a different client id is not a correction: it marks everyone, clears no one.
        let read = store.node_client("filebox").unwrap();
        let plan = set(&def, read.as_ref(), tag, "c-2", Some("OTHER"));
        assert!(plan.reauth);
        assert!(write_node(&store, &vault, &node(true), &plan, tag).is_empty());
        assert_eq!(status("cy").0, NeedsReauth);
    }

    #[test]
    fn a_plain_disable_and_enable_leaves_a_refused_client_parked() {
        use crate::mcp_registry::McpOauthStatus::{Connected, NeedsReauth};
        let tag = "toggle-refused";
        let (store, vault, _c1) = world(tag);
        let def = store.connector_entry("filebox").unwrap().unwrap().def;
        let ada = member_row(&store, &vault, tag, "ada", "c-1", None);
        store
            .record_mcp_oauth_refresh_failure(&ada, true, CLIENT_REFUSED_REASON)
            .unwrap();
        let keep = ClientPlan {
            write: ClientWrite::Keep,
            reauth: false,
        };
        let status = || {
            store
                .get_mcp_connection(SecretScope::User, "ada", "filebox")
                .unwrap()
                .oauth
                .unwrap()
                .status
        };
        // Off and on again with no secret saved in between: the provider refused the secret that
        // is still saved, so ada stays parked.
        assert!(write_node(&store, &vault, &node(false), &keep, tag).is_empty());
        assert!(write_node(&store, &vault, &node(true), &keep, tag).is_empty());
        assert_eq!(status(), NeedsReauth);
        // A save that keeps the connector enabled, with no secret, clears nothing either.
        assert!(write_node(&store, &vault, &node(true), &keep, tag).is_empty());
        assert_eq!(status(), NeedsReauth);

        // A corrected secret saved while it is off (in a save of its own) waits for the enable,
        // and a plain save while off in between does not drop it.
        assert!(write_node(&store, &vault, &node(false), &keep, tag).is_empty());
        let read = store.node_client("filebox").unwrap();
        let plan = set(&def, read.as_ref(), tag, "c-1", Some("CORRECTED"));
        assert!(!plan.reauth);
        assert!(write_node(&store, &vault, &node(false), &plan, tag).is_empty());
        assert!(write_node(&store, &vault, &node(false), &keep, tag).is_empty());
        assert_eq!(status(), NeedsReauth);
        assert_eq!(write_node(&store, &vault, &node(true), &keep, tag), ["ada"]);
        assert_eq!(status(), Connected);

        // The enable used the marker up: refused again, another plain toggle parks her still.
        let ada = store
            .get_mcp_connection(SecretScope::User, "ada", "filebox")
            .unwrap();
        store
            .record_mcp_oauth_refresh_failure(&ada, true, CLIENT_REFUSED_REASON)
            .unwrap();
        assert!(write_node(&store, &vault, &node(false), &keep, tag).is_empty());
        assert!(write_node(&store, &vault, &node(true), &keep, tag).is_empty());
        assert_eq!(status(), NeedsReauth);
    }

    #[test]
    fn a_refusal_of_a_pending_sign_in_stays_pending_and_a_changed_client_replaces_it() {
        use crate::mcp_registry::McpOauthStatus::NeedsReauth;
        use crate::mcp_registry::TOOLS_PENDING_REASON;
        let tag = "pending-refused";
        let (store, vault, _c1) = world(tag);
        let def = store.connector_entry("filebox").unwrap().unwrap().def;
        let refuse_pending = |who: &str| {
            let row = member_row(&store, &vault, tag, who, "c-1", None);
            store
                .record_mcp_oauth_refresh_failure(&row, true, TOOLS_PENDING_REASON)
                .unwrap();
            let row = store
                .get_mcp_connection(SecretScope::User, who, "filebox")
                .unwrap();
            store
                .record_mcp_oauth_refresh_failure(&row, true, CLIENT_REFUSED_REASON)
                .unwrap();
        };
        let state = |who: &str| {
            let oauth = store
                .get_mcp_connection(SecretScope::User, who, "filebox")
                .unwrap()
                .oauth
                .unwrap();
            (oauth.status, oauth.last_refresh_error.unwrap_or_default())
        };
        refuse_pending("ada");
        assert_eq!(
            state("ada"),
            (NeedsReauth, CLIENT_REFUSED_PENDING_REASON.to_string())
        );
        // A corrected secret for the same client: pending again, not returned (nothing resumes).
        let read = store.node_client("filebox").unwrap();
        let plan = set(&def, read.as_ref(), tag, "c-1", Some("CORRECTED"));
        assert!(write_node(&store, &vault, &node(true), &plan, tag).is_empty());
        assert_eq!(
            state("ada"),
            (NeedsReauth, TOOLS_PENDING_REASON.to_string())
        );

        // A→B→A: refused again while pending, then the admin moves to c-2 and back to c-1 with a
        // corrected secret. B's save replaced the recoverable mark: she signs in again.
        let row = store
            .get_mcp_connection(SecretScope::User, "ada", "filebox")
            .unwrap();
        store
            .record_mcp_oauth_refresh_failure(&row, true, CLIENT_REFUSED_REASON)
            .unwrap();
        let read = store.node_client("filebox").unwrap();
        let plan = set(&def, read.as_ref(), tag, "c-2", Some("B"));
        assert!(plan.reauth);
        write_node(&store, &vault, &node(true), &plan, tag);
        let changed = state("ada");
        assert_eq!(changed.0, NeedsReauth);
        assert!(changed.1.contains("changed the client"), "{changed:?}");
        let read = store.node_client("filebox").unwrap();
        let plan = set(&def, read.as_ref(), tag, "c-1", Some("A"));
        write_node(&store, &vault, &node(true), &plan, tag);
        let read = store.node_client("filebox").unwrap();
        let plan = set(&def, read.as_ref(), tag, "c-1", Some("A-CORRECTED"));
        assert!(write_node(&store, &vault, &node(true), &plan, tag).is_empty());
        assert_eq!(state("ada"), changed);
    }

    #[test]
    fn a_corrected_secret_for_a_later_client_leaves_a_row_an_earlier_client_parked() {
        use crate::mcp_registry::McpOauthStatus::NeedsReauth;
        let tag = "fix-later-client";
        let (store, vault, _c1) = world(tag);
        let def = store.connector_entry("filebox").unwrap().unwrap().def;
        // c-1 was refused for ada; her tokens are c-1's.
        let ada = member_row(&store, &vault, tag, "ada", "c-1", None);
        store
            .record_mcp_oauth_refresh_failure(&ada, true, CLIENT_REFUSED_REASON)
            .unwrap();
        // The admin moves to c-2 (ada is already parked, so the mark leaves her reason), then
        // corrects c-2's secret.
        let read = store.node_client("filebox").unwrap();
        let plan = set(&def, read.as_ref(), tag, "c-2", Some("C2-FIRST"));
        assert!(plan.reauth);
        write_node(&store, &vault, &node(true), &plan, tag);
        let read = store.node_client("filebox").unwrap();
        let plan = set(&def, read.as_ref(), tag, "c-2", Some("C2-CORRECTED"));
        assert!(!plan.reauth);
        let cleared = write_node(&store, &vault, &node(true), &plan, tag);
        // Her c-1 tokens are not c-2's: she still signs in again.
        assert!(cleared.is_empty(), "{cleared:?}");
        let oauth = store
            .get_mcp_connection(SecretScope::User, "ada", "filebox")
            .unwrap()
            .oauth
            .unwrap();
        assert_eq!(oauth.status, NeedsReauth);
        assert_eq!(oauth.client_id, "c-1");
    }

    #[test]
    fn a_clear_planned_before_a_secret_only_save_in_the_same_second_commits_nothing() {
        let tag = "same-second";
        let (store, vault, _first) = world(tag);
        let def = store.connector_entry("filebox").unwrap().unwrap().def;
        // Both saves below are stamped with the same second (`set` plans at `now = 5`).
        let read = store.node_client("filebox").unwrap();
        let plan = set(&def, read.as_ref(), tag, "c-1", Some("FIRST-SECRET"));
        write_node(&store, &vault, &node(true), &plan, tag);
        // Admin A reads the client and plans to clear it.
        let earlier = store.node_client("filebox").unwrap().unwrap();
        // Admin B saves a new secret for the same client in that second.
        let plan = set(&def, Some(&earlier), tag, "c-1", Some("NEWER-SECRET"));
        write_node(&store, &vault, &node(true), &plan, tag);
        let newer = store.node_client("filebox").unwrap().unwrap();
        assert_eq!(newer.updated_at, earlier.updated_at);
        // A's clear, planned from the earlier row, is refused and removes nothing.
        let clear = ClientPlan {
            write: ClientWrite::Clear,
            reauth: true,
        };
        let e = store
            .put_node_with_client(
                &vault,
                &node(false),
                &clear,
                &format!("https://mcp.{tag}.example/mcp"),
                &PlanBasis {
                    tenant: None,
                    def: &def,
                    client: Some(&earlier),
                },
            )
            .unwrap_err();
        assert!(
            matches!(&e, OrchestratorError::Conflict(m) if m == CLIENT_CHANGED_WHILE_SAVING),
            "{e}"
        );
        assert_eq!(store.node_client("filebox").unwrap(), Some(newer));
        assert_eq!(org_secret_of(&vault), "NEWER-SECRET");
    }

    #[test]
    fn the_client_revision_moves_on_every_client_write_and_never_goes_back() {
        let tag = "revision";
        let (store, vault, first) = world(tag);
        let def = store.connector_entry("filebox").unwrap().unwrap().def;
        let rev = || store.node_client("filebox").unwrap().map(|c| c.revision);
        assert!(first.revision > 0);
        let save = |id: &str, secret: Option<&str>| {
            let read = store.node_client("filebox").unwrap();
            let plan = set(&def, read.as_ref(), tag, id, secret);
            write_node(&store, &vault, &node(true), &plan, tag);
        };
        // Every write moves it: the same client saved again, a new secret, another id, and that id
        // back again.
        save("c-1", None);
        let resaved = rev().unwrap();
        assert!(resaved > first.revision);
        save("c-1", Some("NEW-SECRET"));
        let after_secret = rev().unwrap();
        assert!(after_secret > resaved);
        save("c-2", Some("B-SECRET"));
        let after_b = rev().unwrap();
        assert!(after_b > after_secret);
        save("c-1", Some("NEW-SECRET"));
        let back = rev().unwrap();
        assert!(back > after_b);
        // Removed and saved again: the revision does not start over.
        let clear = ClientPlan {
            write: ClientWrite::Clear,
            reauth: true,
        };
        write_node(&store, &vault, &node(false), &clear, tag);
        assert_eq!(rev(), None);
        save("c-1", Some("AGAIN"));
        assert!(rev().unwrap() > back);
    }

    #[test]
    fn the_pinned_token_auth_method_is_stored_with_the_client() {
        let tag = "method-kept";
        let (store, vault, _c) = world(tag);
        let def = store.connector_entry("filebox").unwrap().unwrap().def;
        let read = store.node_client("filebox").unwrap();
        assert_eq!(
            read.as_ref().unwrap().token_auth_method,
            TokenAuthMethod::ClientSecretPost
        );
        let intent = check_request(
            &def,
            read.as_ref(),
            true,
            &Some(Some(ClientRequest {
                client_id: "c-1".into(),
                client_secret: Some("S".into()),
            })),
        )
        .unwrap();
        let plan = plan_client(
            &def,
            read.as_ref(),
            intent,
            Some(discovered(tag, TokenAuthMethod::ClientSecretBasic)),
            6,
            "olga",
        )
        .unwrap();
        write_node(&store, &vault, &node(true), &plan, tag);
        assert_eq!(
            store
                .node_client("filebox")
                .unwrap()
                .unwrap()
                .token_auth_method,
            TokenAuthMethod::ClientSecretBasic
        );
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
    fn a_secret_replaced_after_the_destination_check_is_not_the_one_that_is_read() {
        let (_store, vault, _client) = world("secret-swap");
        let binding = client_secret_binding_id("filebox");
        // The check sees the row as it is; an admin's save then replaces it (other value, another
        // host) before the caller reads on. What is decrypted is still the row that was checked.
        let mut checked_for = Vec::new();
        let got = vault
            .decrypt_secret_at_checked(SecretScope::Org, ORG_OWNER_USER_ID, &binding, |meta| {
                checked_for = meta.allowed_destinations.clone();
                vault
                    .put_secret(
                        &binding,
                        "REPLACEMENT-SECRET",
                        SecretKind::Connection,
                        &["auth.elsewhere.example".to_string()],
                        None,
                    )
                    .unwrap();
                Ok(())
            })
            .unwrap();
        assert_eq!(checked_for, ["auth.secret-swap.example"]);
        assert_eq!(got.expose_secret(), "ORG-SECRET-value");
        // A row the check refuses is never decrypted.
        let e = vault
            .decrypt_secret_at_checked(SecretScope::Org, ORG_OWNER_USER_ID, &binding, |_| {
                Err(OrchestratorError::PermissionDenied("no".into()))
            })
            .unwrap_err();
        assert!(matches!(e, OrchestratorError::PermissionDenied(_)));
    }

    #[test]
    fn a_secret_saved_after_the_client_was_read_is_not_paired_with_it() {
        let (store, vault, client) = world("secret-revision");
        // An admin saves the client again (a new secret) after `client` was read: the secret now
        // stored belongs to the newer revision and is not sent with the older read.
        let newer = enable_filebox(&store, &vault, "secret-revision", "c-2", "NEWER-SECRET-v");
        assert!(newer.revision > client.revision);
        let e = org_client_secret(&vault, &client, &client.token_endpoint).unwrap_err();
        assert!(matches!(e, OrchestratorError::Conflict(_)), "{e}");
        assert!(!e.to_string().contains("NEWER-SECRET"), "{e}");
        // The current read gets its own secret.
        let got = org_client_secret(&vault, &newer, &newer.token_endpoint).unwrap();
        assert_eq!(got.expose_secret(), "NEWER-SECRET-v");
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
            // The default port reads as no port (`Url::port()` is `None` for it): refused too.
            "http://127.0.0.1:80/cb",
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
