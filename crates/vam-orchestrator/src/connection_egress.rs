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
    rest_connection_state, CallerState, ConnectionStatus, ConnectorConfig, ConnectorEntry,
    Freshness, NodeConnector, RestRefresher, SWAP_REFRESH_WINDOW_SECS,
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

/// Every binding of every connector the node offers, built in or installed (CN-6.4): the
/// placeholders the gate reserves for a session, with the hosts the company address fills in.
///
/// A placeholder is reserved for every connector, so a connector with no address yet (or one a
/// marketplace installed and the admin has not enabled) has its placeholder refused at the gate
/// and never forwarded. Such a binding has no hosts, so nothing matches it. A host that still
/// holds `{subdomain}` is never listed. If the node's connectors cannot be read, only the built-in
/// bindings are reserved (the swap re-reads the store per request and fails closed).
pub fn resolved_bindings(store: &SqliteWorkflowStore) -> Vec<BindingSpec> {
    match store.connector_entries() {
        Ok(entries) => entries.iter().flat_map(entry_bindings).collect(),
        Err(e) => {
            tracing::warn!(
                "connector definitions unreadable, reserving built-in placeholders only: {e}"
            );
            builtin_bindings()
        }
    }
}

fn entry_bindings(e: &ConnectorEntry) -> Vec<BindingSpec> {
    e.template
        .bindings
        .iter()
        .flatten()
        .map(|b| BindingSpec {
            connector: e.def.urn.clone(),
            name: b.name.clone(),
            placeholder: b.placeholder.clone(),
            hosts: e
                .def
                .bindings
                .iter()
                .flatten()
                .find(|x| x.name == b.name)
                .map(|x| {
                    x.hosts
                        .iter()
                        .filter(|h| !h.contains(['{', '}']))
                        .cloned()
                        .collect()
                })
                .unwrap_or_default(),
        })
        .collect()
}

/// Fills the `{subdomain}.<suffix>` hosts of the declared shapes with the node's company address
/// (CN-6.4), as of session boot. The address and the suffix come from the connector the tool
/// requires, never from the skill: a templated host is filled only when the tool names a
/// connector (by URN) whose `tenant.suffix` is the host's suffix and that has an address on this
/// node. Every other templated shape is dropped, so a literal `{subdomain}` never reaches the
/// swap. Exact hosts (Microsoft Graph) pass through unchanged; the envelope still requires the
/// request's host to be one of the connector's binding hosts.
pub fn fill_skill_shapes(
    rows: Vec<ToolEgressRow>,
    entries: &[ConnectorEntry],
    nodes: &[NodeConnector],
) -> Vec<ToolEgressRow> {
    let address_of = |urn: &str| -> Option<(String, String)> {
        let entry = entries.iter().find(|e| e.template.urn == urn)?;
        let suffix = entry.template.tenant.as_ref()?.suffix.clone();
        let id = connector_id(&entry.template);
        let sub = nodes
            .iter()
            .find(|n| n.connector_id == id)?
            .tenant_subdomain
            .as_deref()?;
        let sub = vam_spec::connector::check_subdomain(sub).ok()?;
        Some((sub, suffix))
    };
    rows.into_iter()
        .map(|mut row| {
            let address = row.requires_connector.as_deref().and_then(address_of);
            row.shapes = std::mem::take(&mut row.shapes)
                .into_iter()
                .filter_map(|mut shape| {
                    if !shape.host.contains(['{', '}']) {
                        return Some(shape);
                    }
                    let want = vam_spec::connector::templated_suffix(&shape.host)?;
                    let (sub, suffix) = address.as_ref()?;
                    (want == suffix).then(|| {
                        shape.host = format!("{sub}.{suffix}");
                        shape
                    })
                })
                .collect();
            row
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
    // The declared shapes are filled with the node's company address now, not as of the boot: an
    // address entered, changed or cleared after the session booted (a run parked for the member to
    // connect and resumed in place) is the one the call is checked against.
    let row = store
        .tool_egress(session_id, tool_name)
        .ok()
        .flatten()
        .map(|row| fill_row_now(store, row));
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

/// [`fill_skill_shapes`] for one tool's row, against the node's connectors as they are now. A row
/// with no templated host is returned as it is, with no read of the node's state.
fn fill_row_now(store: &SqliteWorkflowStore, row: ToolEgressRow) -> ToolEgressRow {
    if !row.shapes.iter().any(|s| s.host.contains(['{', '}'])) {
        return row;
    }
    let filled = match (store.connector_entries(), store.node_connectors()) {
        (Ok(entries), Ok(nodes)) => fill_skill_shapes(vec![row], &entries, &nodes),
        _ => fill_skill_shapes(vec![row], &[], &[]),
    };
    filled.into_iter().next().unwrap_or_else(|| ToolEgressRow {
        tool: String::new(),
        requires_connector: None,
        shapes: Vec::new(),
        effect_class: None,
    })
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
    refresher: &RestRefresher,
    owner: &str,
    id: &str,
    horizon_secs: i64,
) -> Freshness {
    ensure_fresh_for_call(store, vault, cfg, refresher, owner, id, horizon_secs, None).await
}

/// [`ensure_fresh`] for the swap of one tool call: `call` is the broker's record of the calls that
/// were refreshed for (`IntentRefreshes`) and this call's id.
///
/// A token the intent check (or an earlier request of the same call) refreshed is not refreshed again
/// while it has at least 20 s left (`connectors::needs_refresh`). The record is read again under the
/// connection lock, and a refresh made here is recorded for the call before the lock is released, so a
/// request of the same call that was waiting for the lock sees it and does not spend a second
/// single-use refresh token.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn ensure_fresh_for_call(
    store: &Arc<SqliteWorkflowStore>,
    vault: &Arc<SecretsVault>,
    cfg: &ConnectorConfig,
    refresher: &RestRefresher,
    owner: &str,
    id: &str,
    horizon_secs: i64,
    call: Option<(&Arc<IntentRefreshes>, &str)>,
) -> Freshness {
    let marked = |call: &Option<(Arc<IntentRefreshes>, String)>| {
        call.as_ref().is_some_and(|(m, c)| m.is_marked(c))
    };
    let call: Option<(Arc<IntentRefreshes>, String)> =
        call.map(|(m, c)| (m.clone(), c.to_string()));
    // Cheap look first: a token that is not near expiry takes no lock.
    match store.connector_connection(owner, id) {
        Ok(Some(c))
            if !crate::connectors::needs_refresh(
                &c,
                crate::connectors::now_secs(),
                horizon_secs,
                marked(&call),
            ) =>
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
        let fresh = crate::connectors::refresh_if_due(
            &store,
            &vault,
            &cfg,
            &owner,
            &id,
            crate::connectors::now_secs(),
            horizon_secs,
            marked(&call),
            refresher.as_ref(),
        );
        if fresh == Freshness::Refreshed {
            if let Some((marks, call_id)) = &call {
                marks.mark(call_id);
            }
        }
        fresh
    })
    .await
    .unwrap_or(Freshness::Unavailable)
}

// ---- one refresh per call ----------------------------------------------------------------------

/// The calls whose intent check refreshed the member's token (CN-6.4), by call id. The intent
/// check and the swap are two looks at one token for one tool call; a provider whose access
/// tokens live a minute is inside both windows, and the second look would spend a second
/// single-use refresh token. The broker marks the call when the intent refreshed, clears the
/// mark when the same call id is asked about again (an approval wait: the intent runs again),
/// and the swap reads it for the grant's call id. Another call's mark is never read.
#[derive(Debug, Default)]
pub struct IntentRefreshes {
    marks: Mutex<HashMap<String, std::time::Instant>>,
}

/// Marks older than this are forgotten (a tool call that ran this long is long over).
const MARK_TTL: Duration = Duration::from_secs(600);

impl IntentRefreshes {
    pub fn new() -> Self {
        Self::default()
    }

    fn marks(&self) -> std::sync::MutexGuard<'_, HashMap<String, std::time::Instant>> {
        self.marks.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The intent check of `call_id` refreshed the token.
    pub fn mark(&self, call_id: &str) {
        let mut m = self.marks();
        m.retain(|_, at| at.elapsed() < MARK_TTL);
        m.insert(call_id.to_string(), std::time::Instant::now());
    }

    /// The intent of `call_id` is being decided (again): no earlier refresh stands for it.
    pub fn clear(&self, call_id: &str) {
        self.marks().remove(call_id);
    }

    pub fn is_marked(&self, call_id: &str) -> bool {
        self.marks()
            .get(call_id)
            .is_some_and(|at| at.elapsed() < MARK_TTL)
    }
}

// ---- the token source ----------------------------------------------------------------------------

/// Attaches the session owner's connection token, for a request the envelope admitted.
pub struct SessionConnectionSource {
    /// The window of the active call (shared with the broker that opens it).
    window: Arc<CallWindow>,
    /// The binding table and the envelope built from it. Built at session boot; for a source made
    /// with [`Self::new`] it is rebuilt from the node's connectors when a request carries a
    /// placeholder and they changed (see [`Self::refresh_bindings`]).
    envelope: Mutex<(Arc<Envelope>, Vec<BindingSpec>)>,
    /// Whether the table follows the node's connectors (false: a table fixed by a test).
    live_bindings: bool,
    store: Arc<SqliteWorkflowStore>,
    vault: Arc<SecretsVault>,
    session_id: String,
    owner: String,
    /// Client ids of the node (read at session start; the node installs them at boot).
    cfg: ConnectorConfig,
    refresher: RestRefresher,
    /// Calls whose intent check just refreshed (shared with the port-52 handler).
    intent_refreshes: Arc<IntentRefreshes>,
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
        let bindings = resolved_bindings(&store);
        let mut source = Self::with_bindings(bindings, window, store, vault, session_id, owner);
        source.live_bindings = true;
        source
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
            envelope: Mutex::new((
                Arc::new(Envelope::new(bindings.clone(), window.clone())),
                bindings,
            )),
            window,
            live_bindings: false,
            store,
            vault,
            session_id,
            owner,
            cfg: crate::connectors::config(),
            refresher: live_refresher(),
            intent_refreshes: Arc::new(IntentRefreshes::new()),
            attached: Mutex::new(Vec::new()),
        }
    }

    /// Shares the broker's record of which calls the intent check refreshed for.
    pub fn with_intent_refreshes(mut self, marks: Arc<IntentRefreshes>) -> Self {
        self.intent_refreshes = marks;
        self
    }

    /// Replaces the node configuration and the refresh call (tests).
    #[cfg(test)]
    fn with_refresh(mut self, cfg: ConnectorConfig, refresher: RestRefresher) -> Self {
        self.cfg = cfg;
        self.refresher = refresher;
        self
    }

    fn envelope(&self) -> Arc<Envelope> {
        self.envelope
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .0
            .clone()
    }

    /// Rebuilds the binding table from the node's connectors as they are now, when it differs from
    /// the one this session booted with. A company address entered, a connector enabled or
    /// installed after the boot (a parked run resumed in place, not in a new session) is then the one
    /// a request is judged against. Called only for a request that carries a placeholder.
    fn refresh_bindings(&self) {
        if !self.live_bindings {
            return;
        }
        // The store is read under the envelope lock: two overlapping requests then publish their
        // tables in the order they read them, so a table older than the installed one is never
        // put back.
        let mut guard = self.envelope.lock().unwrap_or_else(|e| e.into_inner());
        let fresh = resolved_bindings(&self.store);
        if guard.1 != fresh {
            guard.0 = Arc::new(Envelope::new(fresh.clone(), self.window.clone()));
            guard.1 = fresh;
        }
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

    /// The access token for `grant` (and the placeholder to scrub, and the connector id), or why
    /// there is none. Every check is on the host, per request, from the node's current state: a
    /// connection made after the session booted is used; one that was revoked, a connector that
    /// was disabled and an address that moved are not (the grant was made against the boot-time
    /// table, this is the second, live check).
    async fn token_for(
        &self,
        grant: &ConnectionGrant,
    ) -> Result<(SecretString, String, String), ConnectionRefusal> {
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
        // Built in or installed, with the node's company address filled in. The URN must be the
        // one the grant names exactly: a connector that only shares an id is another connector.
        let entry = self
            .store
            .connector_entry(vam_spec::connector_listing::connector_id_of_urn(
                &grant.connector,
            ))
            .ok()
            .flatten()
            .filter(|e| e.def.urn == grant.connector)
            .ok_or_else(|| Self::refuse(tool, "unknown connector"))?;
        let def = entry.def;
        let name = def.display_name.clone();
        let id = connector_id(&def);
        let binding = def
            .bindings
            .as_ref()
            .and_then(|b| b.iter().find(|b| b.name == grant.binding))
            .ok_or_else(|| {
                if entry.template.tenant.is_some() {
                    Self::refuse(tool, &format!("{name} has no company address"))
                } else {
                    Self::refuse(tool, "unknown binding")
                }
            })?;
        if binding.inject != vam_spec::connector::Inject::Bearer
            || binding.value != vam_spec::connector::BindingValue::AccessToken
        {
            return Err(Self::refuse(tool, "this binding cannot be attached"));
        }
        if binding.hosts.iter().all(|h| *h != grant.host) {
            return Err(Self::refuse(tool, "host is not one of the binding's hosts"));
        }
        // The node's enable and the member's connection, read from the node's state now.
        let live_state = || -> Result<(), ConnectionRefusal> {
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
            let endpoint = self
                .store
                .node_client(&id)
                .ok()
                .flatten()
                .map(|c| c.token_endpoint);
            if rest_connection_state(&conn, Some(&node), has_refresh, endpoint.as_deref())
                != CallerState::Connected
            {
                return Err(not_connected(&name));
            }
            Ok(())
        };
        live_state()?;
        // Refresh before swap: only now that the envelope, the destination, the binding's host
        // and the connection's state have all passed.
        match ensure_fresh_for_call(
            &self.store,
            &self.vault,
            &self.cfg,
            &self.refresher,
            &self.owner,
            &id,
            SWAP_REFRESH_WINDOW_SECS,
            Some((&self.intent_refreshes, grant.call_id.as_str())),
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
        // The refresh may have waited seconds on the provider: an admin may have turned the connector
        // off, or the member's grant may have been invalidated, meanwhile. Decided again, with the
        // same refusals, before the token is read.
        live_state()?;
        let row = access_token_binding_id(&id);
        // Only the member's own connection-kind row, pinned to this host. An egress or org row
        // that happens to carry this id is never used. The pin and the token come from ONE read of
        // the row: a reconnect that rewrites it between two reads cannot pair one row's pin with
        // another's token.
        let token = self
            .vault
            .decrypt_secret_at_checked(
                SecretScope::User,
                &self.owner,
                &row,
                |meta: &crate::secrets::HostSecretMetadata| {
                    if meta.kind == SecretKind::Connection
                        && meta.allowed_destinations.contains(&grant.host)
                    {
                        Ok(())
                    } else {
                        Err(OrchestratorError::PermissionDenied(
                            "the access token is not pinned to this host".into(),
                        ))
                    }
                },
            )
            .map_err(|_| not_connected(&name))?;
        Ok((token, binding.placeholder.clone(), id))
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
        match self.envelope().screen(request) {
            // No placeholder of the table, and nothing that looks like one: nothing to decide, and
            // no read of the node's state. A request carrying text with the placeholder prefix may
            // name a connector installed after this table was built: the table is reloaded first.
            Ok(None)
                if !self.live_bindings || !vam_network::connection::may_carry_placeholder(request) =>
            {
                Ok(None)
            }
            decided => {
                if !self.live_bindings {
                    return decided;
                }
                self.refresh_bindings();
                self.envelope().screen(request)
            }
        }
    }

    async fn attach(
        &self,
        grant: &ConnectionGrant,
        headers: &mut HashMap<String, String>,
    ) -> Result<(), ConnectionRefusal> {
        // The call the grant was made for must still be the active one.
        self.envelope().recheck(grant)?;
        let (token, placeholder, id) = self.token_for(grant).await?;
        // A refresh can take seconds: the call must still be the active one.
        self.envelope().recheck(grant)?;
        vam_network::connection::set_bearer(headers, token.expose_secret());
        self.audit(&access_token_binding_id(&id), "connection");
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
        self.envelope().recheck(grant)?;
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
        self.envelope().binds_host(host)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connectors::RestRefreshTarget;
    use crate::connectors::{ConnectorConnection, NodeConnector};
    use secrecy::SecretString;
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
                tenant_subdomain: None,
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
                client_id: None,
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
                            tenant_subdomain: None,
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
    ) -> RestRefresher {
        let calls = calls.clone();
        Arc::new(move |target: &RestRefreshTarget<'_>, rt: &SecretString| {
            let RestRefreshTarget::Entra {
                tenant,
                client_id: client,
                scope,
            } = *target
            else {
                panic!("an Entra connection is refreshed with an Entra target");
            };
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
    fn due_fx(expires_at: Option<i64>, refresher: RestRefresher) -> Fx {
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

/// The swap for a connector a marketplace installed (CN-6.4): the company's filled hosts, the
/// filled shapes, the placeholders, and the refusals that read no token.
#[cfg(test)]
mod installed_tests {
    use super::*;
    use crate::connector_client::{
        test_support::install_company_address_rows, ClientPlan, ClientWrite, NodeClient, PlanBasis,
        TokenAuthMethod,
    };
    use crate::connectors::now_secs;
    use crate::connectors::{RestCommit, RestRefreshTarget};
    use crate::mcp_oauth_refresh::RefreshedToken;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const ZID: &str = "zendesk";
    const ZURN: &str = "urn:vam:connector:connectors.community.example/community/zendesk";
    const ZPH: &str = "vamsec_zendesk_api";
    const M365_URN: &str = "urn:vam:connector:hub.squrell.cloud/squrell/microsoft-365";
    const M365_PH: &str = "vamsec_m365_graph";
    const OWNER: &str = "user-member";
    const ACCESS: &str = "ACCESS-zendesk-0123456789abcdef";
    const SESSION: &str = "sess-z";

    fn node(address: &str, enabled: bool) -> NodeConnector {
        NodeConnector {
            connector_id: ZID.into(),
            enabled,
            consent_tenant_id: None,
            consent_recorded_at: None,
            consent_recorded_by: None,
            tenant_subdomain: Some(address.into()),
            updated_at: 1,
            updated_by: "olga".into(),
        }
    }

    /// Saves the address and its client the way Enable does.
    fn save(store: &SqliteWorkflowStore, vault: &SecretsVault, address: &str) {
        let host = format!("{address}.zendesk.com");
        let template = store.connector_entry(ZID).unwrap().unwrap().template;
        let stored = store.node_client(ZID).unwrap();
        let stored_address = store
            .node_connector(ZID)
            .unwrap()
            .and_then(|n| n.tenant_subdomain);
        let client = NodeClient {
            connector_id: ZID.into(),
            client_id: "zc1".into(),
            issuer: format!("https://{host}"),
            authorize_endpoint: format!("https://{host}/oauth/authorizations/new"),
            token_endpoint: format!("https://{host}/oauth/tokens"),
            revocation_endpoint: None,
            token_auth_method: TokenAuthMethod::default(),
            revocation_auth_method: None,
            secret_set: false,
            updated_at: 1,
            updated_by: "olga".into(),
            revision: 0,
        };
        let reauth = stored
            .as_ref()
            .is_some_and(|e| e.token_endpoint != client.token_endpoint);
        store
            .put_node_with_client(
                vault,
                &node(address, true),
                &ClientPlan {
                    write: ClientWrite::Set {
                        client: Box::new(client),
                        secret: None,
                    },
                    reauth,
                },
                "",
                &PlanBasis {
                    def: &template,
                    client: stored.as_ref(),
                    tenant: stored_address.as_deref(),
                },
            )
            .unwrap();
    }

    struct World {
        store: Arc<SqliteWorkflowStore>,
        vault: Arc<SecretsVault>,
        window: Arc<CallWindow>,
    }

    /// An installed `zendesk` connector enabled for `address`, the member connected to it with a
    /// token that expires at `expires_at`.
    fn world(address: &str, expires_at: Option<i64>) -> World {
        let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
        let vault = SecretsVault::for_tests(store.clone());
        install_company_address_rows(&store);
        enable_and_connect(&store, &vault, address, expires_at);
        World {
            store,
            vault,
            window: Arc::new(CallWindow::new()),
        }
    }

    /// The admin enables `zendesk` for `address` with its client, and the member signs in.
    fn enable_and_connect(
        store: &SqliteWorkflowStore,
        vault: &SecretsVault,
        address: &str,
        expires_at: Option<i64>,
    ) {
        save(store, vault, address);
        let host = format!("{address}.zendesk.com");
        store
            .commit_rest_connection(
                vault,
                &RestCommit {
                    owner: OWNER,
                    connector_id: ZID,
                    address,
                    token_endpoint: &format!("https://{host}/oauth/tokens"),
                    client_id: "zc1",
                    token_host: &host,
                    access_hosts: std::slice::from_ref(&host),
                    scopes: &["read".to_string()],
                    access_token: ACCESS,
                    refresh_token: "REFRESH-zendesk-0123456789abcdef",
                    expires_at,
                    now: now_secs(),
                },
                &|_| Ok(()),
            )
            .unwrap();
    }

    fn source(w: &World) -> SessionConnectionSource {
        SessionConnectionSource::new(
            w.window.clone(),
            w.store.clone(),
            w.vault.clone(),
            SESSION.into(),
            OWNER.into(),
        )
    }

    fn templated_row(tool: &str) -> ToolEgressRow {
        ToolEgressRow {
            tool: tool.into(),
            requires_connector: Some(ZURN.into()),
            shapes: vec![Shape {
                binding: "api".into(),
                host: "{subdomain}.zendesk.com".into(),
                methods: vec!["GET".into()],
                path_prefixes: vec!["/api/v2/search.json".into(), "/api/v2/tickets/".into()],
                paths: vec![],
            }],
            effect_class: None,
        }
    }

    /// The shapes the session records at boot for `tool`, with the node's address filled in.
    fn boot_shapes(w: &World, tool: &str) -> Vec<Shape> {
        let rows = fill_skill_shapes(
            vec![templated_row(tool)],
            &w.store.connector_entries().unwrap(),
            &w.store.node_connectors().unwrap(),
        );
        rows.into_iter().next().unwrap().shapes
    }

    fn open(w: &World, tool: &str, requires: &str, shapes: Vec<Shape>) {
        w.window.open(ActiveCall::new(
            "call-1",
            tool,
            Some(requires.into()),
            shapes,
            Duration::from_secs(60),
        ));
    }

    fn ask(
        s: &SessionConnectionSource,
        placeholder: &str,
        method: &str,
        url: &str,
    ) -> Result<Option<ConnectionGrant>, ConnectionRefusal> {
        let headers = HashMap::from([("Authorization".to_string(), placeholder.to_string())]);
        s.authorize(&ConnectionRequest {
            method,
            url,
            headers: &headers,
            body: b"",
        })
    }

    async fn attach(
        s: &SessionConnectionSource,
        grant: &ConnectionGrant,
    ) -> Result<String, ConnectionRefusal> {
        let mut headers = HashMap::from([("Authorization".to_string(), ZPH.to_string())]);
        s.attach(grant, &mut headers)
            .await
            .map(|_| headers["Authorization"].clone())
    }

    #[test]
    fn the_shapes_are_filled_from_the_connector_and_the_nodes_address_never_from_the_skill() {
        let w = world("acme", None);
        let filled = boot_shapes(&w, "ticket_search");
        assert_eq!(filled.len(), 1);
        assert_eq!(filled[0].host, "acme.zendesk.com");
        assert_eq!(filled[0].path_prefixes.len(), 2);
        let entries = w.store.connector_entries().unwrap();
        let nodes = w.store.node_connectors().unwrap();
        let mk = |requires: Option<&str>, host: &str| ToolEgressRow {
            requires_connector: requires.map(str::to_string),
            shapes: vec![Shape {
                host: host.into(),
                ..templated_row("t").shapes[0].clone()
            }],
            ..templated_row("t")
        };
        for (requires, host, kept) in [
            // Another suffix than the connector's: dropped.
            (Some(ZURN), "{subdomain}.evil.example", false),
            // No connector named: nothing to fill from, dropped.
            (None, "{subdomain}.zendesk.com", false),
            // A connector that is not installed: dropped.
            (
                Some("urn:vam:connector:x.example/p/none"),
                "{subdomain}.zendesk.com",
                false,
            ),
            // The template anywhere but the whole first label: dropped.
            (Some(ZURN), "a.{subdomain}.zendesk.com", false),
            (Some(ZURN), "{subdomain}zendesk.com", false),
            // An exact host is not touched, whoever it belongs to.
            (Some(ZURN), "graph.microsoft.com", true),
            (Some(ZURN), "{subdomain}.zendesk.com", true),
        ] {
            let out = fill_skill_shapes(vec![mk(requires, host)], &entries, &nodes);
            assert_eq!(!out[0].shapes.is_empty(), kept, "{requires:?} {host}");
            for s in &out[0].shapes {
                assert!(!s.host.contains('{'), "{}", s.host);
            }
        }
        // Without an address on the node, or without the connector, nothing is filled.
        let none = fill_skill_shapes(vec![templated_row("t")], &entries, &[]);
        assert!(none[0].shapes.is_empty());
        let unfilled = fill_skill_shapes(vec![templated_row("t")], &[], &nodes);
        assert!(unfilled[0].shapes.is_empty());
    }

    #[tokio::test]
    async fn an_installed_binding_attaches_only_on_its_filled_host_and_inside_the_filled_shape() {
        let w = world("acme", None);
        let s = source(&w);
        open(&w, "ticket_search", ZURN, boot_shapes(&w, "ticket_search"));

        // Inside the shape, on the company's host: the member's token is attached.
        let ok = ask(
            &s,
            ZPH,
            "GET",
            "https://acme.zendesk.com/api/v2/search.json?query=printer",
        )
        .unwrap()
        .expect("grant");
        assert_eq!(ok.host, "acme.zendesk.com");
        assert_eq!(ok.connector, ZURN);
        assert_eq!(attach(&s, &ok).await.unwrap(), format!("Bearer {ACCESS}"));
        let ok = ask(
            &s,
            ZPH,
            "GET",
            "https://acme.zendesk.com/api/v2/tickets/7.json",
        )
        .unwrap()
        .expect("grant");
        assert!(attach(&s, &ok).await.is_ok());

        // Everything else is refused at the envelope, before any token is read.
        for (method, url) in [
            ("GET", "https://globex.zendesk.com/api/v2/search.json"),
            ("GET", "https://{subdomain}.zendesk.com/api/v2/search.json"),
            (
                "GET",
                "https://acme.zendesk.com.evil.example/api/v2/search.json",
            ),
            ("GET", "https://acme.zendesk.com/api/v2/users.json"),
            (
                "GET",
                "https://acme.zendesk.com/api/v2/search.json/../users.json",
            ),
            ("POST", "https://acme.zendesk.com/api/v2/search.json"),
            ("GET", "http://acme.zendesk.com/api/v2/search.json"),
        ] {
            let e = ask(&s, ZPH, method, url).unwrap_err();
            assert!(e.text.contains("shape"), "{method} {url}: {}", e.text);
        }
    }

    #[test]
    fn a_connector_with_no_address_still_reserves_its_placeholder_and_matches_no_host() {
        let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
        let vault = SecretsVault::for_tests(store.clone());
        install_company_address_rows(&store);
        let w = World {
            store,
            vault,
            window: Arc::new(CallWindow::new()),
        };
        let s = source(&w);
        assert!(!s.binds_host("acme.zendesk.com"));
        // A shape with an exact host cannot reach a token either: no host is pinned.
        let exact = Shape {
            host: "acme.zendesk.com".into(),
            ..templated_row("t").shapes[0].clone()
        };
        open(&w, "ticket_search", ZURN, vec![exact]);
        let e = ask(
            &s,
            ZPH,
            "GET",
            "https://acme.zendesk.com/api/v2/search.json",
        )
        .unwrap_err();
        assert!(e.text.contains("declared shape"), "{}", e.text);
        // The placeholder is refused, not left for the static path (`Ok(None)`).
        w.window.clear();
        assert!(ask(
            &s,
            ZPH,
            "GET",
            "https://acme.zendesk.com/api/v2/search.json"
        )
        .is_err());
    }

    #[tokio::test]
    async fn a_disabled_connector_or_a_moved_address_attaches_nothing_after_boot() {
        let w = world("acme", None);
        let s = source(&w);
        open(&w, "ticket_search", ZURN, boot_shapes(&w, "ticket_search"));
        let url = "https://acme.zendesk.com/api/v2/search.json";
        let grant = ask(&s, ZPH, "GET", url).unwrap().expect("grant");
        assert!(attach(&s, &grant).await.is_ok());

        // The admin disables the connector: the grant made at the boot-time table attaches nothing.
        w.store.put_node_connector(&node("acme", false)).unwrap();
        let e = attach(&s, &grant).await.unwrap_err();
        assert!(e.text.contains("not enabled"), "{}", e.text);

        // Enabled again, then the address moves to `globex`: acme's host is no longer the
        // connector's, whatever this session booted with.
        w.store.put_node_connector(&node("acme", true)).unwrap();
        assert!(attach(&s, &grant).await.is_ok());
        w.store.put_node_connector(&node("globex", true)).unwrap();
        let e = attach(&s, &grant).await.unwrap_err();
        assert!(e.text.contains("binding's hosts"), "{}", e.text);
        let reads = w.store.read_outbox_for_run(SESSION, 0).unwrap();
        assert!(reads.iter().all(|e| !e.payload.contains(ACCESS)));
    }

    #[tokio::test]
    async fn a_placeholder_of_one_connector_never_unlocks_another_connectors_token() {
        let w = world("acme", None);
        let s = source(&w);
        // A Zendesk tool carrying the Graph placeholder, on either host.
        open(&w, "ticket_search", ZURN, boot_shapes(&w, "ticket_search"));
        for url in [
            "https://acme.zendesk.com/api/v2/search.json",
            "https://graph.microsoft.com/v1.0/me/messages",
        ] {
            assert!(ask(&s, M365_PH, "GET", url).is_err(), "{url}");
        }
        // A Graph tool carrying the Zendesk placeholder.
        let graph_shape = Shape {
            binding: "graph".into(),
            host: "graph.microsoft.com".into(),
            methods: vec!["GET".into()],
            path_prefixes: vec!["/v1.0/me/messages".into()],
            paths: vec![],
        };
        open(&w, "m365_probe_search", M365_URN, vec![graph_shape]);
        for url in [
            "https://acme.zendesk.com/api/v2/search.json",
            "https://graph.microsoft.com/v1.0/me/messages",
        ] {
            assert!(ask(&s, ZPH, "GET", url).is_err(), "{url}");
        }
        // A tool that names a connector by another URN with the same id gets nothing either.
        open(
            &w,
            "ticket_search",
            "urn:vam:connector:other.example/pub/zendesk",
            boot_shapes(&w, "ticket_search"),
        );
        assert!(ask(
            &s,
            ZPH,
            "GET",
            "https://acme.zendesk.com/api/v2/search.json"
        )
        .is_err());
    }

    #[test]
    fn two_bindings_with_one_placeholder_are_refused_for_both() {
        let w = world("acme", None);
        let spec = |connector: &str, ph: &str, host: &str| BindingSpec {
            connector: connector.into(),
            name: "api".into(),
            placeholder: ph.into(),
            hosts: vec![host.into()],
        };
        let with = |bindings: Vec<BindingSpec>| {
            SessionConnectionSource::with_bindings(
                bindings,
                w.window.clone(),
                w.store.clone(),
                w.vault.clone(),
                SESSION.into(),
                OWNER.into(),
            )
        };
        let s = with(vec![
            spec(ZURN, ZPH, "acme.zendesk.com"),
            spec(
                "urn:vam:connector:x.example/p/other",
                ZPH,
                "api.other.example",
            ),
        ]);
        open(&w, "ticket_search", ZURN, boot_shapes(&w, "ticket_search"));
        for url in [
            "https://acme.zendesk.com/api/v2/search.json",
            "https://api.other.example/api/v2/search.json",
        ] {
            let e = ask(&s, ZPH, "GET", url).unwrap_err();
            assert!(e.text.contains("more than one"), "{}", e.text);
        }
        // A placeholder that contains another one is refused as well (the shorter is a substring).
        let s = with(vec![
            spec(ZURN, ZPH, "acme.zendesk.com"),
            spec(ZURN, "vamsec_zendesk_api_v2", "acme.zendesk.com"),
        ]);
        let e = ask(
            &s,
            "vamsec_zendesk_api_v2",
            "GET",
            "https://acme.zendesk.com/api/v2/search.json",
        )
        .unwrap_err();
        assert!(e.text.contains("more than one"), "{}", e.text);
    }

    #[test]
    fn the_resolved_bindings_hold_every_connectors_placeholder_with_filled_hosts_only() {
        let w = world("acme", None);
        let b = resolved_bindings(&w.store);
        let z = b.iter().find(|b| b.placeholder == ZPH).expect("zendesk");
        assert_eq!(z.connector, ZURN);
        assert_eq!(z.hosts, vec!["acme.zendesk.com".to_string()]);
        let g = b.iter().find(|b| b.placeholder == M365_PH).expect("graph");
        assert_eq!(g.hosts, vec!["graph.microsoft.com".to_string()]);
        assert!(b.iter().all(|b| b.hosts.iter().all(|h| !h.contains('{'))));
        // Placeholders are unique across the table.
        let mut all: Vec<&str> = b.iter().map(|b| b.placeholder.as_str()).collect();
        all.sort_unstable();
        all.dedup();
        assert_eq!(all.len(), b.len());
        // With no address the placeholder is there and no host is.
        let none = SqliteWorkflowStore::in_memory().unwrap();
        install_company_address_rows(&none);
        let z = resolved_bindings(&none)
            .into_iter()
            .find(|b| b.placeholder == ZPH)
            .expect("reserved");
        assert!(z.hosts.is_empty());
    }

    #[tokio::test]
    async fn the_swap_refreshes_at_the_filled_token_endpoint_and_two_requests_make_one_refresh() {
        let w = world("acme", Some(now_secs() - 5));
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = calls.clone();
        let refresher: RestRefresher =
            Arc::new(move |t: &RestRefreshTarget<'_>, rt: &SecretString| {
                let RestRefreshTarget::Oauth {
                    token_endpoint,
                    client_id,
                    ..
                } = *t
                else {
                    panic!("an `oauth` connection is refreshed with an `oauth` target");
                };
                assert_eq!(token_endpoint, "https://acme.zendesk.com/oauth/tokens");
                assert_eq!(client_id, "zc1");
                assert!(rt.expose_secret().starts_with("REFRESH-zendesk"));
                seen.fetch_add(1, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(150));
                Ok(RefreshedToken {
                    access_token: "NEW-ACCESS-zendesk-0123456789".into(),
                    expires_in: Some(3600),
                    refresh_token: Some("NEW-REFRESH-zendesk-0123456789".into()),
                })
            });
        let s = source(&w).with_refresh(crate::connectors::config(), refresher);
        open(&w, "ticket_search", ZURN, boot_shapes(&w, "ticket_search"));
        let url = "https://acme.zendesk.com/api/v2/search.json";
        let grant = ask(&s, ZPH, "GET", url).unwrap().expect("grant");
        let (a, b) = tokio::join!(attach(&s, &grant), attach(&s, &grant));
        assert_eq!(a.unwrap(), "Bearer NEW-ACCESS-zendesk-0123456789");
        assert_eq!(b.unwrap(), "Bearer NEW-ACCESS-zendesk-0123456789");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "one refresh for two requests"
        );
    }

    // ---- one refresh per call (CN-6.4) -----------------------------------------------------------

    /// A refresher for a provider whose access tokens live `ttl` seconds; counts its calls.
    fn minute_refresher(calls: &Arc<AtomicUsize>, ttl: u64) -> RestRefresher {
        let seen = calls.clone();
        Arc::new(move |_: &RestRefreshTarget<'_>, _: &SecretString| {
            let n = seen.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(50));
            Ok(RefreshedToken {
                access_token: format!("ACCESS-minute-{n}-0123456789abcdef"),
                expires_in: Some(ttl),
                refresh_token: Some(format!("REFRESH-minute-{n}-0123456789abcdef")),
            })
        })
    }

    fn open_call(w: &World, call_id: &str) {
        w.window.open(ActiveCall::new(
            call_id,
            "ticket_search",
            Some(ZURN.into()),
            boot_shapes(w, "ticket_search"),
            Duration::from_secs(60),
        ));
    }

    /// What the broker does for one intent: clear the call's mark, check at the tool's timeout
    /// (`horizon` 60 s), mark the call when that refreshed.
    async fn intent(
        w: &World,
        marks: &IntentRefreshes,
        refresher: &RestRefresher,
        call_id: &str,
    ) -> Freshness {
        marks.clear(call_id);
        let f = ensure_fresh(
            &w.store,
            &w.vault,
            &crate::connectors::config(),
            refresher,
            OWNER,
            ZID,
            60,
        )
        .await;
        if f == Freshness::Refreshed {
            marks.mark(call_id);
        }
        f
    }

    const SEARCH: &str = "https://acme.zendesk.com/api/v2/search.json";

    #[tokio::test]
    async fn the_intent_check_and_the_swap_of_one_call_make_one_refresh_a_second_call_refreshes_again(
    ) {
        // Tokens live 60 s: inside the intent's 60 s horizon and the swap's 120 s window.
        let w = world("acme", Some(now_secs() - 5));
        let calls = Arc::new(AtomicUsize::new(0));
        let refresher = minute_refresher(&calls, 60);
        let marks = Arc::new(IntentRefreshes::new());
        let s = source(&w)
            .with_refresh(crate::connectors::config(), refresher.clone())
            .with_intent_refreshes(marks.clone());

        // Call 1: the intent refreshes, the swap uses that token.
        open_call(&w, "call-1");
        assert_eq!(
            intent(&w, &marks, &refresher, "call-1").await,
            Freshness::Refreshed
        );
        let g = ask(&s, ZPH, "GET", SEARCH).unwrap().expect("grant");
        assert_eq!(
            attach(&s, &g).await.unwrap(),
            "Bearer ACCESS-minute-0-0123456789abcdef"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1, "one refresh for one call");
        // A second request of the same call still uses it.
        let g = ask(&s, ZPH, "GET", SEARCH).unwrap().expect("grant");
        assert!(attach(&s, &g).await.is_ok());
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // Call 2: its intent is due again (the token is a minute old at most) and refreshes.
        open_call(&w, "call-2");
        assert_eq!(
            intent(&w, &marks, &refresher, "call-2").await,
            Freshness::Refreshed
        );
        let g = ask(&s, ZPH, "GET", SEARCH).unwrap().expect("grant");
        assert_eq!(
            attach(&s, &g).await.unwrap(),
            "Bearer ACCESS-minute-1-0123456789abcdef"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "one more for the next call"
        );
    }

    #[tokio::test]
    async fn a_call_with_no_mark_refreshes_at_the_swap_and_a_stale_mark_never_borrows() {
        let w = world("acme", Some(now_secs() + 50));
        let calls = Arc::new(AtomicUsize::new(0));
        let refresher = minute_refresher(&calls, 60);
        let marks = Arc::new(IntentRefreshes::new());
        let s = source(&w)
            .with_refresh(crate::connectors::config(), refresher)
            .with_intent_refreshes(marks.clone());
        // Another call is marked; this one (call-2) is not: the swap refreshes (50 s is inside 120).
        marks.mark("call-1");
        open_call(&w, "call-2");
        let g = ask(&s, ZPH, "GET", SEARCH).unwrap().expect("grant");
        assert_eq!(
            attach(&s, &g).await.unwrap(),
            "Bearer ACCESS-minute-0-0123456789abcdef"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_mark_does_not_skip_the_swap_when_the_token_has_under_twenty_seconds() {
        // The intent refreshed, then the call waited (a slow tool start): 10 s are left.
        let w = world("acme", Some(now_secs() + 10));
        let calls = Arc::new(AtomicUsize::new(0));
        let marks = Arc::new(IntentRefreshes::new());
        let s = source(&w)
            .with_refresh(crate::connectors::config(), minute_refresher(&calls, 3600))
            .with_intent_refreshes(marks.clone());
        marks.mark("call-1");
        open_call(&w, "call-1");
        let g = ask(&s, ZPH, "GET", SEARCH).unwrap().expect("grant");
        assert_eq!(
            attach(&s, &g).await.unwrap(),
            "Bearer ACCESS-minute-0-0123456789abcdef"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // The same holds for a token that is already expired.
        let w = world("acme", Some(now_secs() - 1));
        let calls = Arc::new(AtomicUsize::new(0));
        let marks = Arc::new(IntentRefreshes::new());
        let s = source(&w)
            .with_refresh(crate::connectors::config(), minute_refresher(&calls, 3600))
            .with_intent_refreshes(marks.clone());
        marks.mark("call-1");
        open_call(&w, "call-1");
        let g = ask(&s, ZPH, "GET", SEARCH).unwrap().expect("grant");
        assert_eq!(
            attach(&s, &g).await.unwrap(),
            "Bearer ACCESS-minute-0-0123456789abcdef"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn two_calls_at_once_never_make_the_swap_send_an_expired_token() {
        // One source per session, one shared member: call A carries a mark, call B does not, and
        // the token is already expired. Whoever gets there first refreshes; nobody sends the old
        // token, and the refresh token is spent once.
        let w = world("acme", Some(now_secs() - 5));
        let calls = Arc::new(AtomicUsize::new(0));
        let refresher = minute_refresher(&calls, 3600);
        let marks = Arc::new(IntentRefreshes::new());
        let a = source(&w)
            .with_refresh(crate::connectors::config(), refresher.clone())
            .with_intent_refreshes(marks.clone());
        let b = SessionConnectionSource::new(
            Arc::new(CallWindow::new()),
            w.store.clone(),
            w.vault.clone(),
            "sess-b".into(),
            OWNER.into(),
        )
        .with_refresh(crate::connectors::config(), refresher);
        marks.mark("call-a");
        open_call(&w, "call-a");
        let ga = ask(&a, ZPH, "GET", SEARCH).unwrap().expect("grant");
        let wb = World {
            store: w.store.clone(),
            vault: w.vault.clone(),
            window: b.window.clone(),
        };
        open_call(&wb, "call-b");
        let gb = ask(&b, ZPH, "GET", SEARCH).unwrap().expect("grant");
        let (ra, rb) = tokio::join!(attach(&a, &ga), attach(&b, &gb));
        for r in [ra.unwrap(), rb.unwrap()] {
            assert_eq!(r, "Bearer ACCESS-minute-0-0123456789abcdef");
            assert_ne!(r, format!("Bearer {ACCESS}"));
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    fn authorized() -> ToolIntentResponse {
        ToolIntentResponse {
            disposition: Disposition::Authorized as i32,
            approval_request_id: None,
            error: None,
            capability_token: None,
            resource_envelope: Some(vam_spec::v1::ResourceEnvelope {
                max_wall_time_ms: 10_000,
                ..Default::default()
            }),
            replayed: None,
        }
    }

    #[tokio::test]
    async fn a_session_booted_before_the_address_is_judged_against_the_address_at_the_call() {
        // The session booted with no company address (its record holds the shape still templated, its
        // binding table has no host). The run parked for the member to connect; the admin entered the
        // address and the member signed in; the run resumed in the same session.
        let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
        let vault = SecretsVault::for_tests(store.clone());
        install_company_address_rows(&store);
        store
            .record_tool_egress(SESSION, &[templated_row("ticket_search")])
            .unwrap();
        let window = Arc::new(CallWindow::new());
        let s = SessionConnectionSource::new(
            window.clone(),
            store.clone(),
            vault.clone(),
            SESSION.into(),
            OWNER.into(),
        );
        assert!(!s.binds_host("acme.zendesk.com"), "no host at the boot");
        enable_and_connect(&store, &vault, "acme", None);
        record_intent(
            &window,
            &store,
            SESSION,
            "ticket_search",
            "call-1",
            1,
            &authorized(),
        );
        let grant = ask(
            &s,
            ZPH,
            "GET",
            "https://acme.zendesk.com/api/v2/search.json?query=printer",
        )
        .unwrap()
        .expect("the resumed call is admitted");
        let mut headers = HashMap::from([("Authorization".to_string(), ZPH.to_string())]);
        s.attach(&grant, &mut headers).await.unwrap();
        assert_eq!(headers["Authorization"], format!("Bearer {ACCESS}"));
        assert!(s.binds_host("acme.zendesk.com"));
        // The envelope still decides: another company's host and a path outside the shape are refused.
        for url in [
            "https://globex.zendesk.com/api/v2/search.json",
            "https://acme.zendesk.com/api/v2/users.json",
        ] {
            assert!(ask(&s, ZPH, "GET", url).is_err(), "{url}");
        }
        // The shape record is the boot's, never rewritten: a literal `{subdomain}` stays out of the window.
        let call = window.current().unwrap();
        assert!(call.shapes.iter().all(|sh| !sh.host.contains('{')));
        // And if the address is cleared again, the next call is judged without it.
        store
            .put_node_connector(&NodeConnector {
                tenant_subdomain: None,
                ..node("acme", false)
            })
            .unwrap();
        record_intent(
            &window,
            &store,
            SESSION,
            "ticket_search",
            "call-2",
            1,
            &authorized(),
        );
        let shapes = &window.current().unwrap().shapes;
        assert!(shapes.is_empty(), "no shape without an address: {shapes:?}");
    }

    #[tokio::test]
    async fn a_request_for_a_connector_installed_after_the_session_booted_reloads_the_table() {
        // The session booted before the connector was installed: its table has no placeholder for
        // it. The admin installs and enables it, the member connects, and the call's request carries
        // the new placeholder: the table is reloaded and the request gets the member's token.
        let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
        let vault = SecretsVault::for_tests(store.clone());
        store
            .record_tool_egress(SESSION, &[templated_row("ticket_search")])
            .unwrap();
        let window = Arc::new(CallWindow::new());
        let s = SessionConnectionSource::new(
            window.clone(),
            store.clone(),
            vault.clone(),
            SESSION.into(),
            OWNER.into(),
        );
        assert!(!s.binds_host("acme.zendesk.com"), "nothing installed at the boot");
        install_company_address_rows(&store);
        enable_and_connect(&store, &vault, "acme", None);
        record_intent(
            &window,
            &store,
            SESSION,
            "ticket_search",
            "call-1",
            1,
            &authorized(),
        );
        let grant = ask(
            &s,
            ZPH,
            "GET",
            "https://acme.zendesk.com/api/v2/search.json?query=printer",
        )
        .unwrap()
        .expect("the new connector's placeholder is judged, not passed through bare");
        let mut headers = HashMap::from([("Authorization".to_string(), ZPH.to_string())]);
        s.attach(&grant, &mut headers).await.unwrap();
        assert_eq!(headers["Authorization"], format!("Bearer {ACCESS}"));
    }

    #[test]
    fn a_binding_table_read_before_a_newer_one_was_installed_is_not_put_back() {
        // A request that read the node's connectors before Enable must not publish its table after
        // a newer request installed the filled one: the table is read under the envelope lock.
        let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
        let vault = SecretsVault::for_tests(store.clone());
        install_company_address_rows(&store);
        let s = SessionConnectionSource::new(
            Arc::new(CallWindow::new()),
            store.clone(),
            vault.clone(),
            SESSION.into(),
            OWNER.into(),
        );
        assert!(!s.binds_host("acme.zendesk.com"), "no host at the boot");
        std::thread::scope(|scope| {
            // Another request holds the lock (it is installing its table).
            let held = s.envelope.lock().unwrap();
            let late = scope.spawn(|| s.refresh_bindings());
            // This request is now waiting for the lock; meanwhile the admin enables `acme`.
            std::thread::sleep(std::time::Duration::from_millis(200));
            save(&store, &vault, "acme");
            drop(held);
            late.join().unwrap();
        });
        assert!(
            s.binds_host("acme.zendesk.com"),
            "the waiting request installed the table it read before the lock"
        );
    }

    #[tokio::test]
    async fn a_refresh_made_at_the_swap_is_recorded_for_the_call_so_its_next_request_does_not_refresh(
    ) {
        // A token with 90 s left is inside the swap's 120 s window and outside the intent's 60 s
        // horizon: the swap refreshes. The provider's tokens live a minute, which is inside the
        // window again; the call's next request must not spend a second refresh token.
        let w = world("acme", Some(now_secs() + 90));
        let calls = Arc::new(AtomicUsize::new(0));
        let marks = Arc::new(IntentRefreshes::new());
        let s = source(&w)
            .with_refresh(crate::connectors::config(), minute_refresher(&calls, 60))
            .with_intent_refreshes(marks.clone());
        open_call(&w, "call-1");
        let g = ask(&s, ZPH, "GET", SEARCH).unwrap().expect("grant");
        assert_eq!(
            attach(&s, &g).await.unwrap(),
            "Bearer ACCESS-minute-0-0123456789abcdef"
        );
        assert!(marks.is_marked("call-1"), "the swap's refresh is recorded");
        let g = ask(&s, ZPH, "GET", SEARCH).unwrap().expect("grant");
        assert_eq!(
            attach(&s, &g).await.unwrap(),
            "Bearer ACCESS-minute-0-0123456789abcdef"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1, "one refresh for one call");
        // Another call is its own.
        assert!(!marks.is_marked("call-2"));
    }

    #[tokio::test]
    async fn two_requests_of_one_call_waiting_on_the_lock_make_one_refresh() {
        let w = world("acme", Some(now_secs() + 90));
        let calls = Arc::new(AtomicUsize::new(0));
        let marks = Arc::new(IntentRefreshes::new());
        let s = source(&w)
            .with_refresh(crate::connectors::config(), minute_refresher(&calls, 60))
            .with_intent_refreshes(marks);
        open_call(&w, "call-1");
        let a = ask(&s, ZPH, "GET", SEARCH).unwrap().expect("grant");
        let b = ask(&s, ZPH, "GET", SEARCH).unwrap().expect("grant");
        let (ra, rb) = tokio::join!(attach(&s, &a), attach(&s, &b));
        for r in [ra.unwrap(), rb.unwrap()] {
            assert_eq!(r, "Bearer ACCESS-minute-0-0123456789abcdef");
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "the second waited for the lock, saw the first's mark and sent nothing"
        );
    }

    #[tokio::test]
    async fn a_connector_disabled_or_a_grant_invalidated_during_the_swap_refresh_attaches_nothing()
    {
        for what in ["disabled", "invalidated"] {
            let w = world("acme", Some(now_secs() + 10));
            let store = w.store.clone();
            let refresher: RestRefresher =
                Arc::new(move |_: &RestRefreshTarget<'_>, _: &SecretString| {
                    if what == "disabled" {
                        store.put_node_connector(&node("acme", false)).unwrap();
                    } else {
                        let mut row = store.connector_connection(OWNER, ZID).unwrap().unwrap();
                        row.status = ConnectionStatus::NeedsReconnect;
                        store.put_connector_connection(&row).unwrap();
                    }
                    Ok(RefreshedToken {
                        access_token: "ACCESS-fresh-0123456789abcdef".into(),
                        expires_in: Some(3600),
                        refresh_token: None,
                    })
                });
            let s = source(&w).with_refresh(crate::connectors::config(), refresher);
            open_call(&w, "call-1");
            let g = ask(&s, ZPH, "GET", SEARCH).unwrap().expect("grant");
            let e = attach(&s, &g).await.unwrap_err();
            // Refused, and no token in the answer: the result of that refresh is dropped (the state it
            // was decided on moved) or the live check after it refuses.
            assert!(
                e.text.contains("not enabled")
                    || e.text.contains("not connected")
                    || e.text.contains("temporarily unavailable")
                    || e.text.contains("needs reconnecting"),
                "{what}: {}",
                e.text
            );
            assert!(!e.text.contains("ACCESS"), "{what}: {}", e.text);
        }
    }

    #[test]
    fn marks_belong_to_one_call_id_and_clear_with_the_next_intent() {
        let m = IntentRefreshes::new();
        assert!(!m.is_marked("call-1"));
        m.mark("call-1");
        assert!(m.is_marked("call-1") && !m.is_marked("call-2"));
        m.mark("call-2");
        m.clear("call-1");
        assert!(!m.is_marked("call-1") && m.is_marked("call-2"));
    }
}
