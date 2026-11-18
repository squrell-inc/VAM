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
