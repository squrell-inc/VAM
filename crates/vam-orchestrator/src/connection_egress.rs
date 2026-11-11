//! Connection bindings on the host (CN-2.2) and the per-call egress envelope's host side
//! (CN-2.3).
//!
//! A connector definition's binding (`vamsec_m365_graph`) is a placeholder the guest sends. The
//! host egress gate swaps in the member's access token, and only inside the shape the calling
//! tool declared in its manifest. The pure checks live in `vam_network::connection`; this
//! module owns what needs the node: the connector definitions, the boot-time snapshot of each
//! tool's declared shapes, the call window the PEP's last authorization opened, and the vault.
//!
//! The guest only ever holds the placeholder. The token is read here, after the destination was
//! allowed, and goes into the outgoing request headers and nowhere else. Connection rows never
//! enter the session's static egress bindings (locked decision 7).

use crate::connectors::{
    access_token_binding_id, connector_id, live_refresher, refresh_token_binding_id,
    rest_connection_state, CallerState, ConnectionStatus, ConnectorConfig, EntraRefresher,
    Freshness, SWAP_REFRESH_WINDOW_SECS,
};
use crate::error::OrchestratorError;
use crate::secrets::{SecretKind, SecretScope, SecretsVault};
use crate::workflow::SqliteWorkflowStore;

use rusqlite::{params, Connection, OptionalExtension};
use secrecy::{ExposeSecret, SecretString};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use vam_network::{
    ActiveCall, BindingSpec, CallWindow, ConnectionGrant, ConnectionRefusal, ConnectionRequest,
    ConnectionTokenSource, Envelope, Shape, WriteTicket,
};
use vam_spec::v1::{tool_intent_response::Disposition, SkillManifest, ToolIntentResponse};

/// How long past the tool's own wall time a call window stays open (open question 1).
const WINDOW_GRACE: Duration = Duration::from_secs(30);
/// Used when the intent response carries no resource envelope.
pub(crate) const DEFAULT_TOOL_TIMEOUT: Duration = Duration::from_secs(60);
/// Tokens kept to scrub from responses: the current one and a few it replaced.
const SCRUB_KEEP: usize = 4;

/// Every binding of every built-in connector. The gate treats each placeholder as reserved:
/// a request that carries one is admitted for the active call or refused, never forwarded.
pub fn builtin_bindings() -> Vec<BindingSpec> {
    vam_spec::connector::builtin_connectors()
        .into_iter()
        .flat_map(|c| {
            let urn = c.urn.clone();
            c.bindings
                .unwrap_or_default()
                .into_iter()
                .map(move |b| BindingSpec {
                    connector: urn.clone(),
                    name: b.name,
                    placeholder: b.placeholder,
                    hosts: b.hosts,
                })
        })
        .collect()
}

// ---- declared shapes, snapshotted at session boot --------------------------------------------

/// One tool's declaration, as the session booted with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolEgressRow {
    pub tool: String,
    pub requires_connector: Option<String>,
    pub shapes: Vec<Shape>,
    /// The effect class the skill manifest declared for the tool (`idempotent`,
    /// `non_idempotent`, `read`), if it declared one. The effect journal uses it for a code tool
    /// that writes through a connection (CN-4.7).
    pub effect_class: Option<String>,
}

/// The egress declarations of every tool in the materialized skills. A manifest that does not
/// parse declares nothing (its tools get no shape, so nothing is attached for them).
pub fn parse_skill_shapes(skills_dir: &Path) -> Vec<ToolEgressRow> {
    let mut rows = Vec::new();
    let Ok(entries) = std::fs::read_dir(skills_dir) else {
        return rows;
    };
    for entry in entries.flatten() {
        let Ok(content) = std::fs::read_to_string(entry.path().join("manifest.json")) else {
            continue;
        };
        let Ok(manifest) =
            vam_spec::encoding::json_to_proto::<SkillManifest>("vam.v1.SkillManifest", &content)
        else {
            continue;
        };
        let requires = Some(manifest.requires_connector.clone()).filter(|u| !u.is_empty());
        for tool in &manifest.tools {
            if tool.egress.is_empty() && requires.is_none() {
                continue;
            }
            rows.push(ToolEgressRow {
                tool: tool.name.clone(),
                requires_connector: requires.clone(),
                effect_class: tool.effect.as_ref().and_then(|e| {
                    match vam_spec::v1::EffectClass::try_from(e.class).ok()? {
                        vam_spec::v1::EffectClass::Read => Some("read".to_string()),
                        vam_spec::v1::EffectClass::Idempotent => Some("idempotent".to_string()),
                        vam_spec::v1::EffectClass::NonIdempotent => {
                            Some("non_idempotent".to_string())
                        }
                        vam_spec::v1::EffectClass::Unspecified => None,
                    }
                }),
                shapes: tool
                    .egress
                    .iter()
                    .map(|e| Shape {
                        binding: e.binding.clone(),
                        host: e.host.clone(),
                        methods: e.methods.clone(),
                        path_prefixes: e.path_prefixes.clone(),
                        paths: e.paths.clone(),
                    })
                    .collect(),
            });
        }
    }
    rows
}

pub(crate) fn initialize_schema(connection: &Connection) -> Result<(), OrchestratorError> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS session_tool_egress (
            session_id TEXT NOT NULL,
            tool_name TEXT NOT NULL,
            requires_connector TEXT,
            shapes_json TEXT NOT NULL,
            PRIMARY KEY (session_id, tool_name)
        );",
    )?;
    // Added with the journal of connection writes (CN-4.7); a row without it declares no class.
    let has_class = connection
        .prepare(
            "SELECT 1 FROM pragma_table_info('session_tool_egress') WHERE name = 'effect_class'",
        )?
        .exists([])?;
    if !has_class {
        connection.execute(
            "ALTER TABLE session_tool_egress ADD COLUMN effect_class TEXT",
            [],
        )?;
    }
    Ok(())
}

impl SqliteWorkflowStore {
    /// Replaces the session's declared shapes (called with the catalog at session boot).
    pub fn record_tool_egress(
        &self,
        session_id: &str,
        rows: &[ToolEgressRow],
    ) -> Result<(), OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let tx = connection.unchecked_transaction()?;
        tx.execute(
            "DELETE FROM session_tool_egress WHERE session_id = ?1",
            params![session_id],
        )?;
        for row in rows {
            let shapes = serde_json::to_string(&row.shapes)
                .map_err(|e| OrchestratorError::Core(e.to_string()))?;
            tx.execute(
                "INSERT OR REPLACE INTO session_tool_egress
                     (session_id, tool_name, requires_connector, shapes_json, effect_class)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    session_id,
                    row.tool,
                    row.requires_connector,
                    shapes,
                    row.effect_class
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// The shapes a tool declared when this session booted, if it declared any.
    pub fn tool_egress(
        &self,
        session_id: &str,
        tool: &str,
    ) -> Result<Option<ToolEgressRow>, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let found: Option<(Option<String>, String, Option<String>)> = connection
            .query_row(
                "SELECT requires_connector, shapes_json, effect_class FROM session_tool_egress
                 WHERE session_id = ?1 AND tool_name = ?2",
                params![session_id, tool],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        Ok(
            found.map(|(requires_connector, json, effect_class)| ToolEgressRow {
                tool: tool.to_string(),
                requires_connector,
                // A row that does not read back declares nothing.
                shapes: serde_json::from_str(&json).unwrap_or_default(),
                effect_class,
            }),
        )
    }
}

// ---- the call window ---------------------------------------------------------------------------

/// Records the PEP's answer for the session's call window. Every intent first closes the
/// previous call's window; only an `AUTHORIZED` one opens a new one, so a denied, parked or
/// replayed intent leaves nothing open. A tool with no declaration still opens a window (with no
/// shape), so a refusal can name it.
pub fn record_intent(
    window: &CallWindow,
    store: &SqliteWorkflowStore,
    session_id: &str,
    tool_name: &str,
    call_id: &str,
    generation: u64,
    response: &ToolIntentResponse,
) {
    window.clear();
    if response.disposition != Disposition::Authorized as i32 {
        return;
    }
    let row = store.tool_egress(session_id, tool_name).ok().flatten();
    let ttl = response
        .resource_envelope
        .as_ref()
        .map(|e| Duration::from_millis(e.max_wall_time_ms))
        .unwrap_or(DEFAULT_TOOL_TIMEOUT)
        + WINDOW_GRACE;
    window.open(
        ActiveCall::new(
            call_id,
            tool_name,
            row.as_ref().and_then(|r| r.requires_connector.clone()),
            row.map(|r| r.shapes).unwrap_or_default(),
            ttl,
        )
        .with_generation(generation),
    );
}

// ---- refresh before swap -----------------------------------------------------------------------

/// Makes sure the member's access token for `id` is not within `horizon_secs` of expiry,
/// refreshing it once if it is. Single flight per `(owner, connector)`: the caller takes the
/// connection lock, reads the expiry again under it, and only then sends. The lock guard moves
/// into the blocking task, so a caller that is dropped mid-refresh (a cancelled request) does not
/// let a second refresh start before the rotated refresh token is written.
///
/// Write order inside (see `connectors::refresh_connection`): rotated refresh token, access token,
/// then the table row with the new expiry. The swap uses a 120 s horizon; the intent-time call
/// (CN-2.5) passes the tool's timeout.
pub(crate) async fn ensure_fresh(
    store: &Arc<SqliteWorkflowStore>,
    vault: &Arc<SecretsVault>,
    cfg: &ConnectorConfig,
    refresher: &EntraRefresher,
    owner: &str,
    id: &str,
    horizon_secs: i64,
) -> Freshness {
    // Cheap look first: a token that is not near expiry takes no lock.
    match store.connector_connection(owner, id) {
        Ok(Some(c))
            if !crate::connectors::token_due(&c, crate::connectors::now_secs(), horizon_secs) =>
        {
            return Freshness::Fresh;
        }
        Ok(Some(_)) => {}
        _ => return Freshness::Unavailable,
    }
    let guard = crate::connectors::lock_connection(owner, id).await;
    let (store, vault, cfg, refresher) =
        (store.clone(), vault.clone(), cfg.clone(), refresher.clone());
    let (owner, id) = (owner.to_string(), id.to_string());
    tokio::task::spawn_blocking(move || {
        let _guard = guard;
        crate::connectors::refresh_if_due(
            &store,
            &vault,
            &cfg,
            &owner,
            &id,
            crate::connectors::now_secs(),
            horizon_secs,
            refresher.as_ref(),
        )
    })
    .await
    .unwrap_or(Freshness::Unavailable)
}

// ---- the token source ----------------------------------------------------------------------------

/// Attaches the session owner's connection token, for a request the envelope admitted.
pub struct SessionConnectionSource {
    envelope: Envelope,
    store: Arc<SqliteWorkflowStore>,
    vault: Arc<SecretsVault>,
    session_id: String,
    owner: String,
    /// Client ids of the node (read at session start; the node installs them at boot).
    cfg: ConnectorConfig,
    refresher: EntraRefresher,
    /// `(token, placeholder)` pairs attached in this session, newest last, for the response scrub.
    attached: Mutex<Vec<(SecretString, String)>>,
}

impl std::fmt::Debug for SessionConnectionSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionConnectionSource")
            .field("session_id", &self.session_id)
            .finish_non_exhaustive()
    }
}

impl SessionConnectionSource {
    pub fn new(
        window: Arc<CallWindow>,
        store: Arc<SqliteWorkflowStore>,
        vault: Arc<SecretsVault>,
        session_id: String,
        owner: String,
    ) -> Self {
        Self::with_bindings(builtin_bindings(), window, store, vault, session_id, owner)
    }

    pub fn with_bindings(
        bindings: Vec<BindingSpec>,
        window: Arc<CallWindow>,
        store: Arc<SqliteWorkflowStore>,
        vault: Arc<SecretsVault>,
        session_id: String,
        owner: String,
    ) -> Self {
        Self {
            envelope: Envelope::new(bindings, window),
            store,
            vault,
            session_id,
            owner,
            cfg: crate::connectors::config(),
            refresher: live_refresher(),
            attached: Mutex::new(Vec::new()),
        }
    }

    /// Replaces the node configuration and the refresh call (tests).
    #[cfg(test)]
    fn with_refresh(mut self, cfg: ConnectorConfig, refresher: EntraRefresher) -> Self {
        self.cfg = cfg;
        self.refresher = refresher;
        self
    }

    /// A write the journal does not allow: the guest is told why, nothing is attached or sent.
    fn refuse_journal(tool: &str, text: &str) -> ConnectionRefusal {
        ConnectionRefusal {
            text: text.to_string(),
            rule: format!("tool:{tool} effect journal"),
        }
    }

    fn refuse(tool: &str, why: &str) -> ConnectionRefusal {
        ConnectionRefusal {
            text: format!("Connection credential refused: {why}."),
            rule: format!("tool:{tool} egress"),
        }
    }

    /// The access token for `grant`, or why there is none. Every check is on the host, per
    /// request, so a connection made after the session booted is used and one that was revoked
    /// is not.
    async fn token_for(
        &self,
        grant: &ConnectionGrant,
    ) -> Result<(SecretString, String), ConnectionRefusal> {
        let tool = grant.tool.as_str();
        // The intent gate (`connector_park`) normally decides these first; this is the backstop
        // for a connection that went away after the intent.
        let not_enabled = |def_name: &str| {
            Self::refuse(
                tool,
                &format!("{def_name} is not enabled for this organization; ask your admin to enable {def_name}"),
            )
        };
        let not_connected = |def_name: &str| {
            Self::refuse(
                tool,
                &format!("{def_name} is not connected; connect {def_name} to continue"),
            )
        };
        let def = vam_spec::connector::builtin_connectors()
            .into_iter()
            .find(|c| c.urn == grant.connector)
            .ok_or_else(|| Self::refuse(tool, "unknown connector"))?;
        let name = def.display_name.clone();
        let id = connector_id(&def);
        let binding = def
            .bindings
            .as_ref()
            .and_then(|b| b.iter().find(|b| b.name == grant.binding))
            .ok_or_else(|| Self::refuse(tool, "unknown binding"))?;
        if binding.inject != vam_spec::connector::Inject::Bearer
            || binding.value != vam_spec::connector::BindingValue::AccessToken
        {
            return Err(Self::refuse(tool, "this binding cannot be attached"));
        }
        if binding.hosts.iter().all(|h| *h != grant.host) {
            return Err(Self::refuse(tool, "host is not one of the binding's hosts"));
        }
        let node = self
            .store
            .node_connector(&id)
            .map_err(|_| not_enabled(&name))?
            .filter(|n| n.enabled)
            .ok_or_else(|| not_enabled(&name))?;
        let conn = self
            .store
            .connector_connection(&self.owner, &id)
            .map_err(|_| not_connected(&name))?
            .filter(|c| c.status == ConnectionStatus::Connected)
            .ok_or_else(|| not_connected(&name))?;
        let has_refresh = self
            .vault
            .has_binding_at(
                SecretScope::User,
                &self.owner,
                &refresh_token_binding_id(&id),
            )
            .unwrap_or(false);
        if rest_connection_state(&conn, Some(&node), has_refresh) != CallerState::Connected {
            return Err(not_connected(&name));
        }
        // Refresh before swap: only now that the envelope, the destination, the binding's host
        // and the connection's state have all passed.
        match ensure_fresh(
            &self.store,
            &self.vault,
            &self.cfg,
            &self.refresher,
            &self.owner,
            &id,
            SWAP_REFRESH_WINDOW_SECS,
        )
        .await
        {
            Freshness::Fresh => {}
            Freshness::Refreshed => {
                self.audit(&refresh_token_binding_id(&id), "connection_refresh")
            }
            Freshness::NeedsReconnect => {
                tracing::warn!(connector = %id, "connection refresh refused: needs reconnect");
                return Err(Self::refuse(
                    tool,
                    &format!(
                        "the {name} connection needs reconnecting (connection_needs_reconnect)"
                    ),
                ));
            }
            Freshness::Unavailable => {
                tracing::warn!(connector = %id, "connection refresh failed: unavailable");
                return Err(Self::refuse(
                    tool,
                    &format!(
                        "the {name} connection is temporarily unavailable (connection_unavailable)"
                    ),
                ));
            }
        }
        let row = access_token_binding_id(&id);
        // Only the member's own connection-kind row, pinned to this host. An egress or org row
        // that happens to carry this id is never used.
        let meta = self
            .vault
            .get_metadata_at(SecretScope::User, &self.owner, &row)
            .map_err(|_| not_connected(&name))?;
        if meta.kind != SecretKind::Connection || !meta.allowed_destinations.contains(&grant.host) {
            return Err(not_connected(&name));
        }
        let token = self
            .vault
            .decrypt_secret_at(SecretScope::User, &self.owner, &row)
            .map_err(|_| not_connected(&name))?;
        Ok((token, binding.placeholder.clone()))
    }

    fn audit(&self, binding_id: &str, purpose: &str) {
        let run = self
            .store
            .find_run_id_for_session(&self.session_id)
            .ok()
            .flatten()
            .unwrap_or_else(|| self.session_id.clone());
        if let Ok(payload) = crate::events::secret_accessed_json(&run, 0, binding_id, purpose) {
            let _ =
                self.store
                    .append_outbox_event(&run, crate::events::TYPE_SECRET_ACCESSED, &payload);
        }
    }
}

#[tonic::async_trait]
impl ConnectionTokenSource for SessionConnectionSource {
    fn authorize(
        &self,
        request: &ConnectionRequest<'_>,
    ) -> Result<Option<ConnectionGrant>, ConnectionRefusal> {
        self.envelope.screen(request)
    }

    async fn attach(
        &self,
        grant: &ConnectionGrant,
        headers: &mut HashMap<String, String>,
    ) -> Result<(), ConnectionRefusal> {
        // The call the grant was made for must still be the active one.
        self.envelope.recheck(grant)?;
        let (token, placeholder) = self.token_for(grant).await?;
        // A refresh can take seconds: the call must still be the active one.
        self.envelope.recheck(grant)?;
        vam_network::connection::set_bearer(headers, token.expose_secret());
        self.audit(
            &access_token_binding_id(&connector_id_of(&grant.connector)),
            "connection",
        );
        let mut kept = self.attached.lock().unwrap_or_else(|e| e.into_inner());
        kept.retain(|(t, _)| t.expose_secret() != token.expose_secret());
        kept.push((token, placeholder));
        let over = kept.len().saturating_sub(SCRUB_KEEP);
        kept.drain(..over);
        Ok(())
    }

    async fn reserve_write(
        &self,
        grant: &ConnectionGrant,
    ) -> Result<Option<Box<dyn WriteTicket>>, ConnectionRefusal> {
        // Reads are not journaled. A write of a code tool is: reserved here, before the token
        // is read; `sent` is written when the gate is about to dispatch it (CN-4.7).
        if vam_network::is_read_method(&grant.method) {
            return Ok(None);
        }
        // The caller decided the grant from the active call: the window is checked again here so
        // a stale grant reserves nothing.
        self.envelope.recheck(grant)?;
        match crate::effect_path::reserve_write(
            &self.store,
            &self.session_id,
            &grant.tool,
            &grant.call_id,
            grant.generation,
        ) {
            Ok(None) => Ok(None),
            Ok(Some(call)) => Ok(Some(Box::new(call))),
            Err(refusal) => Err(Self::refuse_journal(&grant.tool, &refusal.text())),
        }
    }

    fn scrub(&self, response: &mut vam_spec::v1::EgressResponse) {
        let kept = self.attached.lock().unwrap_or_else(|e| e.into_inner());
        for (token, placeholder) in kept.iter() {
            vam_network::connection::scrub_secret(response, token.expose_secret(), placeholder);
        }
    }

    fn binds_host(&self, host: &str) -> bool {
        self.envelope.binds_host(host)
    }
}

fn connector_id_of(urn: &str) -> String {
    builtin_by_urn(urn)
        .map(|c| connector_id(&c))
        .unwrap_or_default()
}

fn builtin_by_urn(urn: &str) -> Option<vam_spec::connector::Connector> {
    vam_spec::connector::builtin_connectors()
        .into_iter()
        .find(|c| c.urn == urn)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connectors::{ConnectorConnection, NodeConnector};
    use vam_spec::v1::ResourceEnvelope;

    const URN: &str = "urn:vam:connector:hub.squrell.cloud/squrell/microsoft-365";
    const PH: &str = "vamsec_m365_graph";
    const TENANT: &str = "11111111-1111-4111-8111-111111111111";
    const OWNER: &str = "user-member";
    const ACCESS: &str = "access-token-VALUE-0123456789";
    const REFRESH: &str = "refresh-token-VALUE-9876543210";
    const SESSION: &str = "sess-1";

    struct Fx {
        store: Arc<SqliteWorkflowStore>,
        vault: Arc<SecretsVault>,
        window: Arc<CallWindow>,
        source: SessionConnectionSource,
    }

    fn read_shape() -> Shape {
        Shape {
            binding: "graph".into(),
            host: "graph.microsoft.com".into(),
            methods: vec!["GET".into()],
            path_prefixes: vec!["/v1.0/me/messages".into()],
            paths: vec![],
        }
    }

    fn fx() -> Fx {
        let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
        let vault = SecretsVault::for_tests(store.clone());
        let window = Arc::new(CallWindow::new());
        let source = SessionConnectionSource::new(
            window.clone(),
            store.clone(),
            vault.clone(),
            SESSION.into(),
            OWNER.into(),
        );
        Fx {
            store,
            vault,
            window,
            source,
        }
    }

    fn connect(f: &Fx) {
        f.store
            .put_node_connector(&NodeConnector {
                connector_id: "microsoft-365".into(),
                enabled: true,
                consent_tenant_id: Some(TENANT.into()),
                consent_recorded_at: Some(1),
                consent_recorded_by: Some("admin".into()),
                updated_at: 1,
                updated_by: "admin".into(),
            })
            .unwrap();
        f.store
            .put_connector_connection(&ConnectorConnection {
                owner_user_id: OWNER.into(),
                connector_id: "microsoft-365".into(),
                status: ConnectionStatus::Connected,
                label: None,
                tenant_id: TENANT.into(),
                token_endpoint: String::new(),
                scopes: vec![],
                expires_at: None,
                connected_at: 1,
                updated_at: 1,
                last_check_at: None,
                last_error: None,
            })
            .unwrap();
        put_row(
            f,
            &access_token_binding_id("microsoft-365"),
            ACCESS,
            SecretKind::Connection,
            "graph.microsoft.com",
            OWNER,
        );
        put_row(
            f,
            &refresh_token_binding_id("microsoft-365"),
            REFRESH,
            SecretKind::Connection,
            "login.microsoftonline.com",
            OWNER,
        );
    }

    fn put_row(f: &Fx, id: &str, value: &str, kind: SecretKind, host: &str, owner: &str) {
        f.vault
            .put_secret_with_owner(id, value, kind, &[host.to_string()], None, owner)
            .unwrap();
    }

    fn open(f: &Fx, tool: &str, shapes: Vec<Shape>) {
        f.window.open(ActiveCall::new(
            "call-1",
            tool,
            Some(URN.into()),
            shapes,
            Duration::from_secs(60),
        ));
    }

    fn admitted(f: &Fx) -> ConnectionGrant {
        let headers = HashMap::from([("Authorization".to_string(), PH.to_string())]);
        f.source
            .authorize(&ConnectionRequest {
                method: "GET",
                url: "https://graph.microsoft.com/v1.0/me/messages",
                headers: &headers,
                body: b"",
            })
            .unwrap()
            .expect("grant")
    }

    async fn attach(
        f: &Fx,
        grant: &ConnectionGrant,
    ) -> Result<HashMap<String, String>, ConnectionRefusal> {
        let mut headers = HashMap::from([("Authorization".to_string(), PH.to_string())]);
        f.source.attach(grant, &mut headers).await.map(|_| headers)
    }

    #[test]
    fn every_built_in_binding_is_reserved() {
        let b = builtin_bindings();
        let graph = b
            .iter()
            .find(|b| b.placeholder == PH)
            .expect("graph binding");
        assert_eq!(graph.connector, URN);
        assert_eq!(graph.name, "graph");
        assert_eq!(graph.hosts, vec!["graph.microsoft.com".to_string()]);
    }

    #[tokio::test]
    async fn the_access_token_is_attached_and_the_refresh_token_never() {
        let f = fx();
        connect(&f);
        open(&f, "m365_probe_search", vec![read_shape()]);
        let grant = admitted(&f);
        let headers = attach(&f, &grant).await.unwrap();
        assert_eq!(headers["Authorization"], format!("Bearer {ACCESS}"));
        assert!(headers
            .values()
            .all(|v| !v.contains(REFRESH) && !v.contains(PH)));
        // The audit event names the row, not the value.
        let events = f.store.read_outbox_for_run(SESSION, 0).unwrap();
        assert!(events
            .iter()
            .any(|e| e.event_type == crate::events::TYPE_SECRET_ACCESSED
                && e.payload
                    .contains("connector/microsoft-365/oauth/access-token")));
        assert!(events
            .iter()
            .all(|e| !e.payload.contains(ACCESS) && !e.payload.contains(REFRESH)));
        // The response scrub covers what was attached.
        let mut resp = vam_spec::v1::EgressResponse {
            status_code: 200,
            headers: HashMap::new(),
            body: format!("echo {ACCESS}").into_bytes(),
            error: None,
        };
        f.source.scrub(&mut resp);
        let body = String::from_utf8(resp.body).unwrap();
        assert!(!body.contains(ACCESS) && body.contains(PH), "{body}");
        assert!(!format!("{:?}", f.source).contains(ACCESS));
    }

    #[tokio::test]
    async fn a_connection_made_after_the_source_exists_is_used() {
        let f = fx();
        open(&f, "m365_probe_search", vec![read_shape()]);
        let grant = admitted(&f);
        assert!(attach(&f, &grant).await.is_err());
        connect(&f);
        assert!(attach(&f, &grant).await.is_ok());
    }

    #[tokio::test]
    async fn nothing_is_attached_without_an_enabled_connected_connection() {
        // A table of named breakers; a local alias would add nothing.
        #[allow(clippy::type_complexity)]
        let broken: Vec<(&str, Box<dyn Fn(&Fx)>)> = vec![
            ("no connection", Box::new(|_| {})),
            (
                "disabled",
                Box::new(|f| {
                    connect(f);
                    f.store
                        .put_node_connector(&NodeConnector {
                            connector_id: "microsoft-365".into(),
                            enabled: false,
                            consent_tenant_id: Some(TENANT.into()),
                            consent_recorded_at: Some(1),
                            consent_recorded_by: None,
                            updated_at: 2,
                            updated_by: "admin".into(),
                        })
                        .unwrap();
                }),
            ),
            (
                "needs reconnect",
                Box::new(|f| {
                    connect(f);
                    let mut c = f
                        .store
                        .connector_connection(OWNER, "microsoft-365")
                        .unwrap()
                        .unwrap();
                    c.status = ConnectionStatus::NeedsReconnect;
                    f.store.put_connector_connection(&c).unwrap();
                }),
            ),
            (
                "other tenant",
                Box::new(|f| {
                    connect(f);
                    let mut c = f
                        .store
                        .connector_connection(OWNER, "microsoft-365")
                        .unwrap()
                        .unwrap();
                    c.tenant_id = "22222222-2222-4222-8222-222222222222".into();
                    f.store.put_connector_connection(&c).unwrap();
                }),
            ),
            (
                "refresh token gone",
                Box::new(|f| {
                    connect(f);
                    f.vault
                        .delete_secret_at(
                            SecretScope::User,
                            OWNER,
                            &refresh_token_binding_id("microsoft-365"),
                        )
                        .unwrap();
                }),
            ),
            (
                "token row is not a connection row",
                Box::new(|f| {
                    connect(f);
                    // The vault refuses to rewrite a connection row as another kind, so the
                    // row is removed first.
                    f.vault
                        .delete_secret_at(
                            SecretScope::User,
                            OWNER,
                            &access_token_binding_id("microsoft-365"),
                        )
                        .unwrap();
                    put_row(
                        f,
                        &access_token_binding_id("microsoft-365"),
                        "planted",
                        SecretKind::Egress,
                        "graph.microsoft.com",
                        OWNER,
                    );
                }),
            ),
            (
                "token row pinned elsewhere",
                Box::new(|f| {
                    connect(f);
                    put_row(
                        f,
                        &access_token_binding_id("microsoft-365"),
                        ACCESS,
                        SecretKind::Connection,
                        "evil.example",
                        OWNER,
                    );
                }),
            ),
            (
                "someone else's connection",
                Box::new(|f| {
                    connect(f);
                    let mut c = f
                        .store
                        .connector_connection(OWNER, "microsoft-365")
                        .unwrap()
                        .unwrap();
                    c.owner_user_id = "user-other".into();
                    f.store
                        .delete_connector_connection(OWNER, "microsoft-365")
                        .unwrap();
                    f.store.put_connector_connection(&c).unwrap();
                }),
            ),
        ];
        for (name, setup) in broken {
            let f = fx();
            setup(&f);
            open(&f, "m365_probe_search", vec![read_shape()]);
            let grant = admitted(&f);
            let err = attach(&f, &grant).await.expect_err(name);
            assert!(
                err.text.starts_with("Connection credential refused"),
                "{name}: {}",
                err.text
            );
            assert_eq!(err.rule, "tool:m365_probe_search egress", "{name}");
            assert!(
                !err.text.contains(ACCESS) && !err.text.contains("planted"),
                "{name}"
            );
        }
    }

    #[tokio::test]
    async fn attach_rechecks_the_call_it_was_authorized_for() {
        let f = fx();
        connect(&f);
        open(&f, "m365_probe_search", vec![read_shape()]);
        let grant = admitted(&f);
        // The next intent replaced the call.
        f.window.open(ActiveCall::new(
            "call-2",
            "other",
            None,
            vec![],
            Duration::from_secs(60),
        ));
        let err = attach(&f, &grant).await.unwrap_err();
        assert!(err.text.contains("declared shape"), "{}", err.text);
        f.window.clear();
        assert!(attach(&f, &grant).await.is_err());
        // Nothing was read for either refusal.
        assert!(f.store.read_outbox_for_run(SESSION, 0).unwrap().is_empty());
    }

    fn response(disposition: Disposition, wall_ms: u64) -> ToolIntentResponse {
        ToolIntentResponse {
            disposition: disposition as i32,
            approval_request_id: None,
            error: None,
            capability_token: None,
            resource_envelope: Some(ResourceEnvelope {
                max_wall_time_ms: wall_ms,
                ..Default::default()
            }),
            replayed: None,
        }
    }

    #[test]
    fn only_an_authorized_intent_opens_the_window_and_every_intent_closes_the_last() {
        let f = fx();
        f.store
            .record_tool_egress(
                SESSION,
                &[ToolEgressRow {
                    tool: "m365_probe_search".into(),
                    requires_connector: Some(URN.into()),
                    shapes: vec![read_shape()],
                    effect_class: None,
                }],
            )
            .unwrap();
        let rec = |tool: &str, d: Disposition| {
            record_intent(
                &f.window,
                &f.store,
                SESSION,
                tool,
                "call-9",
                3,
                &response(d, 10_000),
            )
        };
        rec("m365_probe_search", Disposition::Authorized);
        let call = f.window.current().expect("open");
        assert_eq!(
            (call.tool.as_str(), call.call_id.as_str()),
            ("m365_probe_search", "call-9")
        );
        assert_eq!(call.requires_connector.as_deref(), Some(URN));
        assert_eq!(call.shapes, vec![read_shape()]);
        // A suspended, denied or replayed next intent closes it and opens nothing.
        for d in [
            Disposition::Suspended,
            Disposition::Denied,
            Disposition::Replayed,
        ] {
            rec("m365_probe_search", Disposition::Authorized);
            rec("m365_probe_send", d);
            assert!(f.window.current().is_none(), "{d:?}");
        }
        // A tool with no declaration still opens a window, with no shape.
        rec("m365_probe_undeclared", Disposition::Authorized);
        let call = f.window.current().unwrap();
        assert!(call.shapes.is_empty() && call.requires_connector.is_none());
    }

    #[test]
    fn the_window_closes_at_the_tool_timeout_plus_a_grace() {
        let f = fx();
        record_intent(
            &f.window,
            &f.store,
            SESSION,
            "t",
            "c",
            1,
            &response(Disposition::Authorized, 0),
        );
        // 0 ms of wall time still leaves the grace, so a call is not cut off mid-start.
        assert!(f.window.current().is_some());
    }

    #[test]
    fn shapes_are_snapshotted_per_session_and_replaced_on_reboot() {
        let f = fx();
        let row = |tool: &str| ToolEgressRow {
            tool: tool.into(),
            requires_connector: Some(URN.into()),
            shapes: vec![read_shape()],
            effect_class: None,
        };
        f.store.record_tool_egress("a", &[row("t1")]).unwrap();
        f.store.record_tool_egress("b", &[row("t2")]).unwrap();
        assert!(f.store.tool_egress("a", "t1").unwrap().is_some());
        assert!(f.store.tool_egress("a", "t2").unwrap().is_none());
        f.store.record_tool_egress("a", &[row("t3")]).unwrap();
        assert!(f.store.tool_egress("a", "t1").unwrap().is_none());
        assert!(f.store.tool_egress("a", "t3").unwrap().is_some());
        assert!(f.store.tool_egress("b", "t2").unwrap().is_some());
    }

    #[test]
    fn exact_paths_survive_the_side_table() {
        let f = fx();
        let exact = ToolEgressRow {
            tool: "m365_create_event".into(),
            requires_connector: Some(URN.into()),
            shapes: vec![Shape {
                methods: vec!["POST".into()],
                path_prefixes: vec![],
                paths: vec!["/v1.0/me/events".into()],
                ..read_shape()
            }],
            effect_class: None,
        };
        f.store
            .record_tool_egress("s", std::slice::from_ref(&exact))
            .unwrap();
        assert_eq!(
            f.store.tool_egress("s", "m365_create_event").unwrap(),
            Some(exact)
        );
    }

    #[test]
    fn manifests_declare_shapes_and_a_broken_manifest_declares_none() {
        let dir = tempfile::tempdir().unwrap();
        let skill = dir.path().join("m365-probe");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(
            skill.join("manifest.json"),
            serde_json::json!({
                "requiresConnector": URN,
                "tools": [
                    { "name": "m365_probe_search", "egress": [
                        { "binding": "graph", "host": "graph.microsoft.com",
                          "methods": ["GET"], "pathPrefixes": ["/v1.0/me/messages"] } ] },
                    { "name": "m365_create_event",
                      "effect": { "class": "EFFECT_CLASS_IDEMPOTENT" },
                      "egress": [
                        { "binding": "graph", "host": "graph.microsoft.com",
                          "methods": ["POST"], "paths": ["/v1.0/me/events"] } ] },
                    { "name": "m365_probe_undeclared" }
                ]
            })
            .to_string(),
        )
        .unwrap();
        let broken = dir.path().join("broken");
        std::fs::create_dir_all(&broken).unwrap();
        std::fs::write(broken.join("manifest.json"), "{ not json").unwrap();
        let rows = parse_skill_shapes(dir.path());
        // A tool with no shape still carries the skill's connector, so the intent gate sees it.
        assert_eq!(
            rows,
            vec![
                ToolEgressRow {
                    tool: "m365_probe_search".into(),
                    requires_connector: Some(URN.into()),
                    shapes: vec![read_shape()],
                    effect_class: None,
                },
                ToolEgressRow {
                    tool: "m365_create_event".into(),
                    requires_connector: Some(URN.into()),
                    shapes: vec![Shape {
                        binding: "graph".into(),
                        host: "graph.microsoft.com".into(),
                        methods: vec!["POST".into()],
                        path_prefixes: vec![],
                        paths: vec!["/v1.0/me/events".into()],
                    }],
                    effect_class: Some("idempotent".into()),
                },
                ToolEgressRow {
                    tool: "m365_probe_undeclared".into(),
                    requires_connector: Some(URN.into()),
                    shapes: vec![],
                    effect_class: None,
                },
            ]
        );
    }

    // ---- refresh before swap (CN-2.4) -----------------------------------------------------------

    use crate::connectors::{now_secs, ConnectorConfig as Cfg};
    use crate::mcp_oauth_refresh::{RefreshFailure, RefreshedToken};
    use std::sync::atomic::{AtomicUsize, Ordering};

    const NEW_ACCESS: &str = "NEW-ACCESS-cccccccccccccccc";
    const NEW_REFRESH: &str = "NEW-REFRESH-dddddddddddddd";

    fn cfg() -> Cfg {
        Cfg {
            client_ids: [("microsoft-365".to_string(), "e2e-client".to_string())].into(),
        }
    }

    type Outcome = Result<RefreshedToken, RefreshFailure>;

    /// A refresher that counts its calls and checks what it was sent.
    fn counted(
        calls: &Arc<AtomicUsize>,
        delay_ms: u64,
        reply: impl Fn() -> Outcome + Send + Sync + 'static,
    ) -> EntraRefresher {
        let calls = calls.clone();
        Arc::new(move |tenant, client, scope, rt| {
            calls.fetch_add(1, Ordering::SeqCst);
            assert_eq!(tenant, TENANT, "the consent tenant");
            assert_eq!(client, "e2e-client");
            assert!(scope.split(' ').any(|s| s == "offline_access"));
            assert!(rt.expose_secret() == REFRESH || rt.expose_secret() == NEW_REFRESH);
            std::thread::sleep(Duration::from_millis(delay_ms));
            reply()
        })
    }

    fn rotated() -> Outcome {
        Ok(RefreshedToken {
            access_token: NEW_ACCESS.into(),
            expires_in: Some(3600),
            refresh_token: Some(NEW_REFRESH.into()),
        })
    }

    /// A connected member whose access token expires at `expires_at`, with the refresher.
    fn due_fx(expires_at: Option<i64>, refresher: EntraRefresher) -> Fx {
        let mut f = fx();
        f.source = SessionConnectionSource::new(
            f.window.clone(),
            f.store.clone(),
            f.vault.clone(),
            SESSION.into(),
            OWNER.into(),
        )
        .with_refresh(cfg(), refresher);
        connect(&f);
        let mut c = f
            .store
            .connector_connection(OWNER, "microsoft-365")
            .unwrap()
            .unwrap();
        c.expires_at = expires_at;
        c.token_endpoint = crate::mcp_oauth_refresh::entra_token_endpoint(TENANT).unwrap();
        f.store.put_connector_connection(&c).unwrap();
        open(&f, "m365_probe_search", vec![read_shape()]);
        f
    }

    fn stored(f: &Fx, binding: String) -> String {
        f.vault
            .decrypt_secret_at(SecretScope::User, OWNER, &binding)
            .unwrap()
            .expose_secret()
            .clone()
    }

    fn row(f: &Fx) -> crate::connectors::ConnectorConnection {
        f.store
            .connector_connection(OWNER, "microsoft-365")
            .unwrap()
            .unwrap()
    }

    #[tokio::test]
    async fn an_expired_token_is_refreshed_once_before_it_is_attached() {
        let calls = Arc::new(AtomicUsize::new(0));
        let f = due_fx(Some(now_secs() - 5), counted(&calls, 0, rotated));
        let grant = admitted(&f);
        let headers = attach(&f, &grant).await.unwrap();
        assert_eq!(headers["Authorization"], format!("Bearer {NEW_ACCESS}"));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            stored(&f, access_token_binding_id("microsoft-365")),
            NEW_ACCESS
        );
        assert_eq!(
            stored(&f, refresh_token_binding_id("microsoft-365")),
            NEW_REFRESH
        );
        let c = row(&f);
        assert_eq!(c.status, ConnectionStatus::Connected);
        assert!(c.expires_at.unwrap() > now_secs() + 3500);
        // A second request inside the new lifetime sends nothing.
        let headers = attach(&f, &grant).await.unwrap();
        assert_eq!(headers["Authorization"], format!("Bearer {NEW_ACCESS}"));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // The old token is gone from the response scrub's reach only by being replaced; both the
        // new token and the audit stay clean.
        let events = f.store.read_outbox_for_run(SESSION, 0).unwrap();
        assert!(events.iter().any(|e| e
            .payload
            .contains("connector/microsoft-365/oauth/refresh-token")));
        for e in &events {
            for t in [ACCESS, REFRESH, NEW_ACCESS, NEW_REFRESH] {
                assert!(!e.payload.contains(t));
            }
        }
        let mut resp = vam_spec::v1::EgressResponse {
            status_code: 200,
            headers: HashMap::new(),
            body: format!("echo {NEW_ACCESS}").into_bytes(),
            error: None,
        };
        f.source.scrub(&mut resp);
        assert!(!String::from_utf8(resp.body).unwrap().contains(NEW_ACCESS));
    }

    #[tokio::test]
    async fn only_a_token_inside_the_window_is_refreshed() {
        let now = now_secs();
        // (expires_at, refreshed): 120 s is the swap window; an unknown expiry is not refreshed
        // (the app always sends `expiresIn`, and a refresh always records one).
        let cases = [
            (Some(now + 600), false),
            (Some(now + 400), false),
            (Some(now + 60), true),
            (Some(now - 10), true),
            (None, false),
        ];
        for (expires_at, refreshed) in cases {
            let calls = Arc::new(AtomicUsize::new(0));
            let f = due_fx(expires_at, counted(&calls, 0, rotated));
            let grant = admitted(&f);
            let headers = attach(&f, &grant).await.unwrap();
            assert_eq!(
                calls.load(Ordering::SeqCst),
                usize::from(refreshed),
                "{expires_at:?}"
            );
            let want = if refreshed { NEW_ACCESS } else { ACCESS };
            assert_eq!(headers["Authorization"], format!("Bearer {want}"));
        }
    }

    #[tokio::test]
    async fn the_intent_horizon_is_the_tools_timeout() {
        let calls = Arc::new(AtomicUsize::new(0));
        let f = due_fx(Some(now_secs() + 300), counted(&calls, 0, rotated));
        let refresher = counted(&calls, 0, rotated);
        let node_cfg = cfg();
        let go = |horizon| {
            ensure_fresh(
                &f.store,
                &f.vault,
                &node_cfg,
                &refresher,
                OWNER,
                "microsoft-365",
                horizon,
            )
        };
        assert_eq!(go(120).await, Freshness::Fresh);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(go(600).await, Freshness::Refreshed);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(go(600).await, Freshness::Fresh);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn two_requests_at_once_make_one_refresh_even_across_sessions() {
        let calls = Arc::new(AtomicUsize::new(0));
        let refresher = counted(&calls, 150, rotated);
        let f = due_fx(Some(now_secs() - 5), refresher.clone());
        // A second session of the same member: its own source, the same node and vault.
        let other = SessionConnectionSource::new(
            f.window.clone(),
            f.store.clone(),
            f.vault.clone(),
            "sess-2".into(),
            OWNER.into(),
        )
        .with_refresh(cfg(), refresher);
        let grant = admitted(&f);
        let mut h1 = HashMap::from([("Authorization".to_string(), PH.to_string())]);
        let mut h2 = h1.clone();
        let mut h3 = h1.clone();
        let (a, b, c) = tokio::join!(
            f.source.attach(&grant, &mut h1),
            other.attach(&grant, &mut h2),
            f.source.attach(&grant, &mut h3),
        );
        a.unwrap();
        b.unwrap();
        c.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1, "one refresh");
        for h in [&h1, &h2, &h3] {
            assert_eq!(h["Authorization"], format!("Bearer {NEW_ACCESS}"));
        }
    }

    #[tokio::test]
    async fn a_swap_waits_for_a_check_holding_the_connection_lock() {
        let calls = Arc::new(AtomicUsize::new(0));
        let f = due_fx(Some(now_secs() - 5), counted(&calls, 0, rotated));
        let grant = admitted(&f);
        let guard = crate::connectors::lock_connection(OWNER, "microsoft-365").await;
        let waited = tokio::time::timeout(Duration::from_millis(100), attach(&f, &grant)).await;
        assert!(waited.is_err(), "the swap must wait for the lock");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        drop(guard);
        assert!(attach(&f, &grant).await.is_ok());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_refused_grant_needs_reconnect_and_the_request_is_refused_with_fixed_text() {
        let calls = Arc::new(AtomicUsize::new(0));
        let f = due_fx(
            Some(now_secs() - 5),
            counted(&calls, 0, || {
                Err(RefreshFailure::needs_reauth("invalid_grant"))
            }),
        );
        let grant = admitted(&f);
        let err = attach(&f, &grant).await.unwrap_err();
        assert!(
            err.text.contains("(connection_needs_reconnect)")
                && err.text.starts_with("Connection credential refused"),
            "{}",
            err.text
        );
        assert_eq!(err.rule, "tool:m365_probe_search egress");
        let c = row(&f);
        assert_eq!(c.status, ConnectionStatus::NeedsReconnect);
        assert_eq!(c.last_error.as_deref(), Some("invalid_grant"));
        // The tokens are untouched, nothing is attached, and the next request sends nothing.
        assert_eq!(stored(&f, access_token_binding_id("microsoft-365")), ACCESS);
        assert!(attach(&f, &grant).await.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        for t in [ACCESS, REFRESH, "invalid_grant"] {
            assert!(!err.text.contains(t));
        }
    }

    #[tokio::test]
    async fn an_unreachable_provider_refuses_the_request_and_changes_no_status() {
        let calls = Arc::new(AtomicUsize::new(0));
        let f = due_fx(
            Some(now_secs() - 5),
            counted(&calls, 0, || {
                Err(RefreshFailure::transient(
                    "OAuth token endpoint request failed",
                ))
            }),
        );
        let grant = admitted(&f);
        let err = attach(&f, &grant).await.unwrap_err();
        assert!(
            err.text.contains("(connection_unavailable)"),
            "{}",
            err.text
        );
        assert!(!err.text.contains("OAuth token endpoint"));
        assert_eq!(row(&f).status, ConnectionStatus::Connected);
        assert_eq!(stored(&f, access_token_binding_id("microsoft-365")), ACCESS);
        // A later request tries again.
        assert!(attach(&f, &grant).await.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn the_rotated_refresh_token_is_written_before_the_access_token() {
        // The access-token write fails (an empty value is refused by the vault), after the
        // rotated refresh token was stored: the connection keeps the refresh token that is
        // live at the provider, and the stale access token is never attached.
        let calls = Arc::new(AtomicUsize::new(0));
        let f = due_fx(
            Some(now_secs() - 5),
            counted(&calls, 0, || {
                Ok(RefreshedToken {
                    access_token: String::new(),
                    expires_in: Some(3600),
                    refresh_token: Some(NEW_REFRESH.into()),
                })
            }),
        );
        let grant = admitted(&f);
        let before = row(&f);
        let err = attach(&f, &grant).await.unwrap_err();
        assert!(
            err.text.contains("(connection_unavailable)"),
            "{}",
            err.text
        );
        assert_eq!(
            stored(&f, refresh_token_binding_id("microsoft-365")),
            NEW_REFRESH
        );
        assert_eq!(stored(&f, access_token_binding_id("microsoft-365")), ACCESS);
        let after = row(&f);
        assert_eq!(after.status, ConnectionStatus::Connected);
        assert_eq!(
            after.expires_at, before.expires_at,
            "still due: refreshed next time"
        );
        // Next time the new refresh token is the one sent.
        let next = Arc::new(AtomicUsize::new(0));
        let ok = counted(&next, 0, rotated);
        assert_eq!(
            ensure_fresh(&f.store, &f.vault, &cfg(), &ok, OWNER, "microsoft-365", 120).await,
            Freshness::Refreshed
        );
    }

    #[tokio::test]
    async fn a_refresh_token_that_cannot_be_stored_means_the_new_access_token_is_not_used() {
        let calls = Arc::new(AtomicUsize::new(0));
        let f = due_fx(
            Some(now_secs() - 5),
            counted(&calls, 0, || {
                Ok(RefreshedToken {
                    access_token: NEW_ACCESS.into(),
                    expires_in: Some(3600),
                    refresh_token: Some(String::new()),
                })
            }),
        );
        let grant = admitted(&f);
        let err = attach(&f, &grant).await.unwrap_err();
        assert!(
            err.text.contains("(connection_unavailable)"),
            "{}",
            err.text
        );
        assert_eq!(stored(&f, access_token_binding_id("microsoft-365")), ACCESS);
        assert_eq!(
            stored(&f, refresh_token_binding_id("microsoft-365")),
            REFRESH
        );
        assert!(!err.text.contains(NEW_ACCESS));
    }

    #[tokio::test]
    async fn a_response_without_a_new_refresh_token_keeps_the_stored_one() {
        let calls = Arc::new(AtomicUsize::new(0));
        let f = due_fx(
            Some(now_secs() - 5),
            counted(&calls, 0, || {
                Ok(RefreshedToken {
                    access_token: NEW_ACCESS.into(),
                    expires_in: None,
                    refresh_token: None,
                })
            }),
        );
        let grant = admitted(&f);
        attach(&f, &grant).await.unwrap();
        assert_eq!(
            stored(&f, refresh_token_binding_id("microsoft-365")),
            REFRESH
        );
        // No `expires_in` still records a short expiry, so the token is not trusted forever.
        let e = row(&f).expires_at.unwrap();
        assert!(e > now_secs() && e <= now_secs() + 300);
    }

    #[tokio::test]
    async fn a_refresh_token_is_never_sent_outside_the_consented_tenant() {
        let calls = Arc::new(AtomicUsize::new(0));
        let f = due_fx(Some(now_secs() - 5), counted(&calls, 0, rotated));
        let mut c = row(&f);
        c.token_endpoint = "https://login.microsoftonline.com/common/oauth2/v2.0/token".into();
        f.store.put_connector_connection(&c).unwrap();
        let grant = admitted(&f);
        let err = attach(&f, &grant).await.unwrap_err();
        assert!(
            err.text.contains("(connection_needs_reconnect)"),
            "{}",
            err.text
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(row(&f).status, ConnectionStatus::NeedsReconnect);
    }

    #[tokio::test]
    async fn a_window_that_closes_during_the_refresh_attaches_nothing() {
        let calls = Arc::new(AtomicUsize::new(0));
        let window_slot: Arc<Mutex<Option<Arc<CallWindow>>>> = Arc::new(Mutex::new(None));
        let slot = window_slot.clone();
        let f = due_fx(
            Some(now_secs() - 5),
            counted(&calls, 0, move || {
                if let Some(w) = slot.lock().unwrap().as_ref() {
                    w.clear();
                }
                rotated()
            }),
        );
        *window_slot.lock().unwrap() = Some(f.window.clone());
        let grant = admitted(&f);
        let mut headers = HashMap::from([("Authorization".to_string(), PH.to_string())]);
        let err = f.source.attach(&grant, &mut headers).await.unwrap_err();
        assert!(err.text.starts_with("Connection credential refused"));
        assert_eq!(headers["Authorization"], PH, "the placeholder is untouched");
        assert!(!headers.values().any(|v| v.contains(NEW_ACCESS)));
    }

    #[test]
    fn a_table_from_before_the_effect_class_is_upgraded_and_keeps_its_rows() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE session_tool_egress (
                    session_id TEXT NOT NULL,
                    tool_name TEXT NOT NULL,
                    requires_connector TEXT,
                    shapes_json TEXT NOT NULL,
                    PRIMARY KEY (session_id, tool_name)
                );
                INSERT INTO session_tool_egress VALUES ('s', 't', NULL, '[]');",
            )
            .unwrap();
        initialize_schema(&connection).unwrap();
        // Running it again changes nothing.
        initialize_schema(&connection).unwrap();
        let class: Option<String> = connection
            .query_row(
                "SELECT effect_class FROM session_tool_egress WHERE tool_name = 't'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(class, None);
    }

    fn write_shape() -> Shape {
        Shape {
            methods: vec!["POST".into()],
            path_prefixes: vec![],
            paths: vec!["/v1.0/me/sendMail".into()],
            ..read_shape()
        }
    }

    /// A bound run and a mutating tool `mail_send` that declares a write shape.
    fn write_world(f: &Fx) {
        use crate::session::{builtin_tool_catalog, ToolCatalogEntry};
        f.store
            .create_session(SESSION, None, "vm-1", "ws-1", "storage-default")
            .unwrap();
        let mut catalog = builtin_tool_catalog();
        catalog.push(ToolCatalogEntry {
            tool_name: "mail_send".into(),
            command: "bun".into(),
            entrypoint: "mail_send.ts".into(),
            digest: String::new(),
            side_effect: 3,
            skill_name: "m365-mail".into(),
            approval_required: false,
        });
        f.store.record_tool_catalog(SESSION, &catalog).unwrap();
        f.store
            .record_tool_egress(
                SESSION,
                &[ToolEgressRow {
                    tool: "mail_send".into(),
                    requires_connector: Some(URN.into()),
                    shapes: vec![write_shape()],
                    effect_class: None,
                }],
            )
            .unwrap();
        f.store
            .persist_run_spec(
                "run-1",
                "{}",
                Some(SESSION),
                "urn:vam:agent:test",
                None,
                "u",
            )
            .unwrap();
        f.window.open(
            ActiveCall::new(
                "call-1",
                "mail_send",
                Some(URN.into()),
                vec![write_shape()],
                Duration::from_secs(60),
            )
            .with_generation(4),
        );
    }

    fn write_grant(f: &Fx) -> ConnectionGrant {
        let headers = HashMap::from([("Authorization".to_string(), PH.to_string())]);
        f.source
            .authorize(&ConnectionRequest {
                method: "POST",
                url: "https://graph.microsoft.com/v1.0/me/sendMail",
                headers: &headers,
                body: b"{}",
            })
            .unwrap()
            .expect("grant")
    }

    #[tokio::test]
    async fn a_write_with_no_journal_row_is_refused_before_any_token_is_read() {
        let f = fx();
        connect(&f);
        write_world(&f);
        let grant = write_grant(&f);
        assert_eq!((grant.method.as_str(), grant.generation), ("POST", 4));
        let err = f
            .source
            .reserve_write(&grant)
            .await
            .err()
            .expect("no intent was journaled for this call");
        assert!(
            err.text.contains("effect_journal_unavailable"),
            "{}",
            err.text
        );
        assert!(err.text.contains("nothing left the host"), "{}", err.text);
        // The token was never read.
        assert!(f
            .store
            .read_outbox_for_run("run-1", 0)
            .unwrap()
            .iter()
            .all(|e| e.event_type != crate::events::TYPE_SECRET_ACCESSED));
    }

    #[tokio::test]
    async fn a_journaled_write_is_reserved_with_the_leases_generation_and_a_read_is_not() {
        let f = fx();
        connect(&f);
        write_world(&f);
        // The intent journaled the call (what the intent path did before the window opened).
        f.store
            .effect_begin(&crate::effect_store::NewEffect {
                run_id: "run-1".into(),
                call_id: "call-1".into(),
                parent_call_id: None,
                tool: "mail_send".into(),
                adapter: "code".into(),
                class: crate::effect_class::EffectClass::NonIdempotent,
                idempotency_key: "k".into(),
                args_digest: "d".into(),
                approval_id: None,
            })
            .unwrap();
        let grant = write_grant(&f);
        let mut ticket = f
            .source
            .reserve_write(&grant)
            .await
            .unwrap()
            .expect("a write is journaled");
        ticket.commit_send().unwrap();
        let row = f.store.effect_get("run-1", "call-1").unwrap().unwrap();
        assert_eq!(row.state, crate::effect_store::EffectState::Sent);
        assert_eq!(row.generation_sent, Some(4));
        ticket.answered(202);
        assert_eq!(
            f.store
                .effect_get("run-1", "call-1")
                .unwrap()
                .unwrap()
                .state,
            crate::effect_store::EffectState::Confirmed
        );
        // A GET is never reserved.
        f.window.clear();
        open(&f, "m365_probe_search", vec![read_shape()]);
        let read = admitted(&f);
        assert!(f.source.reserve_write(&read).await.unwrap().is_none());
    }
}
