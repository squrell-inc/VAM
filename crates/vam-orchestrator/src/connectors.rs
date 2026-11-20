//! Node enablement of connectors (CN-1.2).
//!
//! The built-in definitions come from `vam_spec::builtin_connectors()`. This module adds what is
//! node state: whether the org administrator enabled a connector, the recorded admin consent
//! (a tenant id, never a secret), and the client id the node uses. Nothing here touches the
//! vault: a listing never carries a vault id or a token.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use rusqlite::{params, Connection, OptionalExtension};
use secrecy::SecretString;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use vam_spec::connector::{
    builtin_connectors, is_placeholder_client_id, Connector, OAuth2Provider, OAuthProfile,
    OrgConsent, Provider, URN_PREFIX,
};

use crate::error::OrchestratorError;
use crate::mcp_client::McpTool;
use crate::mcp_oauth_refresh::{
    entra_token_endpoint, RefreshFailure, RefreshFailureKind, RefreshedToken,
};
use crate::mcp_registry::{validate_oauth_secret_value, McpConnection, McpOauthStatus};
use crate::secrets::{SecretKind, SecretScope, SecretsVault};
use crate::workflow::SqliteWorkflowStore;

// ---- node configuration ------------------------------------------------------------------------

/// Client ids a node sets per connector id (`VAMD_CONNECTOR_<ID>_CLIENT_ID` or
/// `--connector-client-id <id>=<client id>`). Absent: the definition's own (placeholder) value.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConnectorConfig {
    pub client_ids: BTreeMap<String, String>,
}

static CONFIG: OnceLock<ConnectorConfig> = OnceLock::new();

/// Installs the node's connector configuration (once, at startup).
pub fn install_config(config: ConnectorConfig) {
    let _ = CONFIG.set(config);
}

/// The installed configuration, or the empty one (every client id is the definition's).
pub fn config() -> ConnectorConfig {
    CONFIG.get().cloned().unwrap_or_default()
}

/// The environment variable that sets a connector's client id: `microsoft-365` reads
/// `VAMD_CONNECTOR_MICROSOFT_365_CLIENT_ID`.
pub fn client_id_env_name(connector_id: &str) -> String {
    format!(
        "VAMD_CONNECTOR_{}_CLIENT_ID",
        connector_id.to_ascii_uppercase().replace('-', "_")
    )
}

/// Reads the client ids from the environment (`get`) and the `--connector-client-id` values.
/// A client id must be a short token of `[A-Za-z0-9._-]`, so nothing odd reaches an authorize URL.
pub fn config_from(
    get: &dyn Fn(&str) -> Option<String>,
    flags: &[String],
) -> Result<ConnectorConfig, String> {
    let known: Vec<String> = builtin_connectors().iter().map(connector_id).collect();
    let mut client_ids = BTreeMap::new();
    for id in &known {
        if let Some(v) = get(&client_id_env_name(id)) {
            client_ids.insert(id.clone(), check_client_id(id, &v)?);
        }
    }
    for flag in flags {
        let (id, value) = flag
            .split_once('=')
            .ok_or_else(|| format!("--connector-client-id {flag:?}: expected <id>=<client id>"))?;
        if !known.iter().any(|k| k == id) {
            return Err(format!("--connector-client-id: `{id}` is not a connector"));
        }
        client_ids.insert(id.to_string(), check_client_id(id, value)?);
    }
    Ok(ConnectorConfig { client_ids })
}

fn check_client_id(id: &str, value: &str) -> Result<String, String> {
    let v = value.trim();
    let ok = !v.is_empty()
        && v.len() <= 128
        && v.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if ok {
        Ok(v.to_string())
    } else {
        Err(format!(
            "client id for `{id}` must be 1-128 characters of letters, digits, `.`, `_`, `-`"
        ))
    }
}

// ---- definitions and node state ---------------------------------------------------------------

/// The connector id: the last segment of the urn (`urn:vam:connector:<host>/<publisher>/<id>`).
pub fn connector_id(c: &Connector) -> String {
    c.urn
        .strip_prefix(URN_PREFIX)
        .unwrap_or(&c.urn)
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .to_string()
}

/// A built-in connector by id.
pub fn builtin(id: &str) -> Option<Connector> {
    builtin_connectors()
        .into_iter()
        .find(|c| connector_id(c) == id)
}

fn oauth(c: &Connector) -> Option<&OAuth2Provider> {
    match &c.provider {
        Provider::Oauth2(o) => Some(o),
        Provider::ApiKey(_) => None,
    }
}

/// The client id this node uses: its override, else the definition's.
pub fn effective_client_id(c: &Connector, cfg: &ConnectorConfig) -> Option<String> {
    cfg.client_ids
        .get(&connector_id(c))
        .cloned()
        .or_else(|| oauth(c).and_then(|o| o.client_id.clone()))
}

/// A connector is configured unless it names a client id and that id is still the placeholder.
pub fn is_configured(c: &Connector, cfg: &ConnectorConfig) -> bool {
    let named = oauth(c).map(|o| o.client_id.is_some()).unwrap_or(false);
    if !named {
        return true;
    }
    effective_client_id(c, cfg)
        .map(|v| !is_placeholder_client_id(&v))
        .unwrap_or(false)
}

/// What an MCP connector fixes about its connection (CN-1.3): the one URL it may use, the vetted
/// tools with their risk class, and which of them ask. Every value comes from the definition;
/// nothing a request, a vendor annotation or a member says can change it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpPolicy {
    pub connector_id: String,
    pub display_name: String,
    pub url: String,
    /// Vendor tool name -> `SideEffectClass` (1 read, 2 local, 3 external).
    pub side_effects: BTreeMap<String, i32>,
    /// Vendor tool names whose default is `ask`.
    pub ask_tools: Vec<String>,
}

impl McpPolicy {
    pub fn is_vetted(&self, vendor_tool: &str) -> bool {
        self.side_effects.contains_key(vendor_tool)
    }
}

/// The policy a connector definition fixes, if it is an MCP connector.
pub fn policy_of(id: &str, def: &Connector) -> Option<McpPolicy> {
    let mcp = def.mcp.as_ref()?;
    let mut side_effects = BTreeMap::new();
    let mut ask_tools = Vec::new();
    for t in &mcp.tools {
        side_effects.insert(
            t.remote.clone(),
            match t.side_effect {
                vam_spec::connector::SideEffect::ReadOnly => 1,
                vam_spec::connector::SideEffect::MutableLocal => 2,
                vam_spec::connector::SideEffect::MutableExternal => 3,
            },
        );
        if t.default == Some(vam_spec::connector::ToolDefault::Ask) {
            ask_tools.push(t.remote.clone());
        }
    }
    Some(McpPolicy {
        connector_id: id.to_string(),
        display_name: def.display_name.clone(),
        url: mcp.url.clone(),
        side_effects,
        ask_tools,
    })
}

/// A connector the node offers: its definition, and where it came from when a marketplace
/// installed it (CN-4.11). Built-in connectors have no `installed`.
#[derive(Debug, Clone, PartialEq)]
pub struct ConnectorEntry {
    /// The connector as the node uses it: the company address (if any) filled in.
    pub def: Connector,
    /// The connector file as published, with `{subdomain}` still in it.
    pub template: Connector,
    pub installed: Option<InstalledMeta>,
}

/// What the node knows about a marketplace connector besides its definition.
#[derive(Debug, Clone, PartialEq)]
pub struct InstalledMeta {
    pub trust: crate::marketplace::store::Trust,
    pub marketplace_id: String,
    pub marketplace_name: String,
    pub publisher_domain: String,
    pub listing: String,
    pub git_ref: String,
    pub commit: String,
    pub package_digest: String,
}

impl ConnectorEntry {
    pub fn id(&self) -> String {
        connector_id(&self.def)
    }

    /// `squrell` for a built-in connector, otherwise the trust of the marketplace it came from.
    pub fn trust_word(&self) -> &'static str {
        self.installed
            .as_ref()
            .map_or("squrell", |m| m.trust.as_str())
    }
}

fn entry_of(i: crate::marketplace::connector_store::InstalledConnector) -> ConnectorEntry {
    ConnectorEntry {
        template: i.def.clone(),
        installed: Some(InstalledMeta {
            trust: i.trust,
            marketplace_id: i.origin.marketplace,
            marketplace_name: i.marketplace_name,
            publisher_domain: i.publisher_domain,
            listing: i.origin.listing,
            git_ref: i.origin.git_ref,
            commit: i.origin.commit,
            package_digest: i.origin.package_digest,
        }),
        def: i.def,
    }
}

/// The built-in connectors, then the installed ones by id. A built-in connector wins an id.
pub fn entries_in(c: &Connection) -> Result<Vec<ConnectorEntry>, OrchestratorError> {
    let mut out: Vec<ConnectorEntry> = builtin_connectors()
        .into_iter()
        .map(|def| ConnectorEntry {
            template: def.clone(),
            def,
            installed: None,
        })
        .collect();
    for i in crate::marketplace::connector_store::installed_in(c)? {
        let id = connector_id(&i.def);
        if !out.iter().any(|e| e.id() == id) {
            out.push(entry_of(i));
        }
    }
    // The one place a definition is filled with the node's company address (CN-6.2).
    let nodes = node_connectors_in(c)?;
    for e in &mut out {
        let id = e.id();
        let sub = nodes
            .iter()
            .find(|n| n.connector_id == id)
            .and_then(|n| n.tenant_subdomain.as_deref());
        e.def = resolve(&e.template, sub);
    }
    Ok(out)
}

/// `e` with its definition filled from the published template and `node`'s company address: a
/// response that shows `node` renders its hosts, endpoints, bindings and capabilities from the same
/// row, never from a definition another read resolved.
pub fn entry_for_node(e: &ConnectorEntry, node: Option<&NodeConnector>) -> ConnectorEntry {
    ConnectorEntry {
        def: resolve(
            &e.template,
            node.and_then(|n| n.tenant_subdomain.as_deref()),
        ),
        template: e.template.clone(),
        installed: e.installed.clone(),
    }
}

/// The connector as the node uses it: `{subdomain}` filled with the node's company address. The
/// only way code reads a templated definition. With no address the endpoints and bindings are
/// gone and the MCP URL is still templated (see [`url_needs_address`]).
pub fn resolve(template: &Connector, subdomain: Option<&str>) -> Connector {
    vam_spec::connector::fill_subdomain(template, subdomain)
}

/// What a REST connection row stores as its `tenant_id` for an `oauth` connector: the company
/// address, or empty for a connector that declares no `tenant` (so it has none).
pub(crate) fn address_key(address: Option<&str>) -> &str {
    address.unwrap_or("")
}

/// True when a URL still holds `{subdomain}`: the connector has no company address yet, so the
/// URL names no server.
pub fn url_needs_address(url: &str) -> bool {
    url.contains(vam_spec::connector::SUBDOMAIN_PLACEHOLDER)
}

/// The connector with this id: built in, else installed.
pub fn entry_in(c: &Connection, id: &str) -> Result<Option<ConnectorEntry>, OrchestratorError> {
    let found = if let Some(def) = builtin(id) {
        Some(ConnectorEntry {
            template: def.clone(),
            def,
            installed: None,
        })
    } else {
        crate::marketplace::connector_store::installed_by_id_in(c, id)?.map(entry_of)
    };
    let Some(mut e) = found else {
        return Ok(None);
    };
    let node = node_connector_in(c, id)?;
    e.def = resolve(
        &e.template,
        node.as_ref().and_then(|n| n.tenant_subdomain.as_deref()),
    );
    Ok(Some(e))
}

/// The policy of the MCP connector with this id (built in or installed), if there is one.
/// Takes the connection a caller already holds, so it can run inside a store method.
pub fn mcp_policy_in(c: &Connection, id: &str) -> Result<Option<McpPolicy>, OrchestratorError> {
    Ok(entry_in(c, id)?.and_then(|e| policy_of(id, &e.def)))
}

impl SqliteWorkflowStore {
    fn connectors_conn(&self) -> Result<std::sync::MutexGuard<'_, Connection>, OrchestratorError> {
        self.connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)
    }

    /// Every connector the node offers: built in, then installed from a marketplace.
    pub fn connector_entries(&self) -> Result<Vec<ConnectorEntry>, OrchestratorError> {
        entries_in(&*self.connectors_conn()?)
    }

    /// The connector with this id (built in or installed).
    pub fn connector_entry(&self, id: &str) -> Result<Option<ConnectorEntry>, OrchestratorError> {
        entry_in(&*self.connectors_conn()?, id)
    }

    /// The policy of the MCP connector with this id.
    pub fn mcp_policy(&self, id: &str) -> Result<Option<McpPolicy>, OrchestratorError> {
        mcp_policy_in(&*self.connectors_conn()?, id)
    }
}

/// Why a connection write for a connector id is refused, or `Ok` with the policy to apply.
/// K2: refused unless the node enabled the connector; the URL must be the definition's.
pub fn gate_mcp_write(
    store: &SqliteWorkflowStore,
    id: &str,
    url: Option<&str>,
    current_url: Option<&str>,
) -> Result<Option<McpPolicy>, OrchestratorError> {
    let Some(policy) = store.mcp_policy(id)? else {
        return Ok(None);
    };
    if url_needs_address(&policy.url) {
        return Err(OrchestratorError::PermissionDenied(format!(
            "the `{id}` connector needs a company address; ask your admin (`vam connectors`)"
        )));
    }
    for u in [url, current_url].into_iter().flatten() {
        if u != policy.url {
            return Err(OrchestratorError::InvalidContract(format!(
                "`{id}` is a connector id and its url is {}; a server added by hand cannot take it. Use `vam connectors` or the app's Connections page",
                policy.url
            )));
        }
    }
    if !store.connector_enabled(id)? {
        return Err(OrchestratorError::PermissionDenied(format!(
            "the `{id}` connector is not enabled on this node; ask your admin (`vam connectors`)"
        )));
    }
    Ok(Some(policy))
}

/// Keeps the vetted tools of a vendor list.
pub fn keep_vetted(policy: &McpPolicy, tools: Vec<McpTool>) -> Vec<McpTool> {
    tools
        .into_iter()
        .filter(|t| policy.is_vetted(&t.name))
        .collect()
}

/// True when this row is the connector's connection and the node has not enabled the connector:
/// it is not seeded (open question 7: consent and the row stay, re-enable restores).
pub fn row_is_gated_off(
    store: &SqliteWorkflowStore,
    conn: &McpConnection,
) -> Result<bool, OrchestratorError> {
    match store.mcp_policy(&conn.connection_id)? {
        Some(p) if p.url == conn.url => Ok(!store.connector_enabled(&conn.connection_id)?),
        _ => Ok(false),
    }
}

/// The caller's state from their own MCP row (`has_tokens`: a refresh token is in the vault).
/// A row on another URL (hand-added before the connector existed) reads `needsReconnect`.
pub fn mcp_connection_state(
    policy: &McpPolicy,
    conn: Option<&McpConnection>,
    has_tokens: bool,
) -> Option<CallerState> {
    let conn = conn?;
    if conn.url != policy.url {
        return Some(CallerState::NeedsReconnect);
    }
    if conn.oauth.as_ref().map(|o| o.status) == Some(McpOauthStatus::NeedsReauth) {
        return Some(CallerState::NeedsReconnect);
    }
    has_tokens.then_some(CallerState::Connected)
}

/// The stored node state of one connector.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeConnector {
    pub connector_id: String,
    pub enabled: bool,
    pub consent_tenant_id: Option<String>,
    pub consent_recorded_at: Option<i64>,
    pub consent_recorded_by: Option<String>,
    /// The company address the admin entered at Enable (`{subdomain}`, CN-6.2). Kept on disable.
    pub tenant_subdomain: Option<String>,
    pub updated_at: i64,
    pub updated_by: String,
}

/// Body of `PUT /v1/connectors/{id}/node`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PutNodeRequest {
    pub enabled: bool,
    /// Omitted keeps the recorded consent; `null` clears it (an admin withdrawing consent).
    #[serde(default, deserialize_with = "some_or_null")]
    pub consent: Option<Option<ConsentRequest>>,
    /// An admin-supplied OAuth client (`provider.client: org`, CN-5.2). Omitted keeps the stored
    /// client; `null` clears it (only with `enabled: false`). Holds the client secret: its `Debug`
    /// output redacts it.
    #[serde(
        default,
        deserialize_with = "crate::connector_client::client_some_or_null"
    )]
    pub client: Option<Option<crate::connector_client::ClientRequest>>,
    /// The company address of a connector that declares `tenant`. Omitted keeps the stored one;
    /// `null` clears it (only with `enabled: false`).
    #[serde(default, deserialize_with = "tenant_some_or_null")]
    pub tenant: Option<Option<TenantRequest>>,
}

fn tenant_some_or_null<'de, D>(d: D) -> Result<Option<Option<TenantRequest>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<TenantRequest>::deserialize(d).map(Some)
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TenantRequest {
    pub subdomain: String,
}

fn some_or_null<'de, D>(d: D) -> Result<Option<Option<ConsentRequest>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<ConsentRequest>::deserialize(d).map(Some)
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConsentRequest {
    pub tenant_id: String,
}

/// A tenant id is a GUID (8-4-4-4-12 hex digits). `common`, `organizations`, `consumers` and
/// domain names are not accepted. Returned lowercase.
pub fn parse_tenant_guid(s: &str) -> Option<String> {
    let t = s.trim();
    let parts: Vec<&str> = t.split('-').collect();
    let lens = [8usize, 4, 4, 4, 12];
    let ok = parts.len() == 5
        && parts
            .iter()
            .zip(lens)
            .all(|(p, n)| p.len() == n && p.chars().all(|c| c.is_ascii_hexdigit()));
    ok.then(|| t.to_ascii_lowercase())
}

fn invalid(msg: impl Into<String>) -> OrchestratorError {
    OrchestratorError::InvalidContract(msg.into())
}

/// The company address a `PUT` leaves on the node row (CN-6.2).
///
/// - `tenant` on a connector that declares none: 400;
/// - `null` while enabling: 400; `null` with `enabled: false` clears it;
/// - a value that is not one DNS label: 400 (fixed text, the value is not quoted);
/// - enabling a connector with `tenant` and no address stored or given: 409.
///
/// Omitted keeps the stored address, so a disable and a re-enable need none.
fn plan_address(
    def: &Connector,
    existing: Option<&NodeConnector>,
    req: &PutNodeRequest,
) -> Result<Option<String>, OrchestratorError> {
    let id = connector_id(def);
    let stored = existing.and_then(|n| n.tenant_subdomain.clone());
    let Some(param) = &def.tenant else {
        return match &req.tenant {
            None => Ok(None),
            Some(_) => Err(invalid(format!("`{id}` takes no company address"))),
        };
    };
    let address = match &req.tenant {
        None => stored,
        Some(None) => {
            if req.enabled {
                return Err(invalid(
                    "the company address is removed only when disabling: send `enabled: false` with `tenant: null`",
                ));
            }
            None
        }
        Some(Some(t)) => Some(
            vam_spec::connector::check_subdomain(&t.subdomain)
                .map_err(|e| invalid(e.to_string().trim_start_matches("connector invalid: ")))?,
        ),
    };
    if req.enabled && address.is_none() {
        return Err(OrchestratorError::Conflict(format!(
            "`{id}` needs your company address before it can be enabled: enter the {} (one DNS label, the first part of an address on {})",
            param.label, param.suffix
        )));
    }
    Ok(address)
}

/// Decides a `PUT`: the row to store, or the refusal. Nothing is written here.
///
/// - consent on a connector whose `orgConsent` is `none`: 400; a tenant that is not a GUID: 400;
/// - enabling a connector that is not configured (placeholder client id): 409;
/// - no `consent` keeps the recorded consent (a disable and a re-enable restore it, Q7).
pub fn plan_put(
    def: &Connector,
    cfg: &ConnectorConfig,
    existing: Option<&NodeConnector>,
    req: &PutNodeRequest,
    now: i64,
    by: &str,
) -> Result<NodeConnector, OrchestratorError> {
    let id = connector_id(def);
    let consent_kind = oauth(def)
        .map(|o| o.org_consent)
        .unwrap_or(OrgConsent::None);
    let clear = matches!(req.consent, Some(None));
    let tenant = match &req.consent {
        None | Some(None) => None,
        Some(Some(c)) => {
            if consent_kind == OrgConsent::None {
                return Err(invalid(format!("`{id}` takes no admin consent")));
            }
            Some(parse_tenant_guid(&c.tenant_id).ok_or_else(|| {
                invalid("consent.tenantId must be a tenant GUID (not `organizations`, `common` or a domain)")
            })?)
        }
    };
    let address = plan_address(def, existing, req)?;
    if req.enabled && !is_configured(def, cfg) {
        return Err(OrchestratorError::Conflict(format!(
            "`{id}` is not configured on this node: its client id is not set (set {})",
            client_id_env_name(&id)
        )));
    }
    let mut row = existing.cloned().unwrap_or(NodeConnector {
        connector_id: id,
        enabled: false,
        consent_tenant_id: None,
        consent_recorded_at: None,
        consent_recorded_by: None,
        tenant_subdomain: None,
        updated_at: now,
        updated_by: by.to_string(),
    });
    row.enabled = req.enabled;
    row.tenant_subdomain = address;
    if clear {
        row.consent_tenant_id = None;
        row.consent_recorded_at = None;
        row.consent_recorded_by = None;
    }
    if let Some(t) = tenant {
        row.consent_tenant_id = Some(t);
        row.consent_recorded_at = Some(now);
        row.consent_recorded_by = Some(by.to_string());
    }
    row.updated_at = now;
    row.updated_by = by.to_string();
    Ok(row)
}

// ---- the caller's state ------------------------------------------------------------------------

/// True when the connector's sign-in needs the admin's recorded consent before a member may
/// connect (`orgConsent: admin_required`). A connector with `orgConsent: none` (a company-address
/// `oauth` connector, a remote MCP server) has no consent row, and none is asked for.
pub(crate) fn needs_admin_consent(def: &Connector) -> bool {
    oauth(def).is_some_and(|o| o.org_consent == OrgConsent::AdminRequired)
}

/// What the caller sees on the tile (design 3.1). `connected` and `needsReconnect` need a
/// connection store; the Phase 1 waves that add one pass it in here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum CallerState {
    AskAdmin,
    WaitingForConsent,
    Connect,
    Connected,
    NeedsReconnect,
}

impl CallerState {
    pub fn label(self) -> &'static str {
        match self {
            Self::AskAdmin => "Ask your admin",
            Self::WaitingForConsent => "Waiting for admin approval",
            Self::Connect => "Connect",
            Self::Connected => "Connected",
            Self::NeedsReconnect => "Needs reconnect",
        }
    }
}

/// A connector the node has not enabled reads `askAdmin` (open question 2); one that needs admin
/// consent and has none reads `waitingForConsent`; otherwise `connection` decides.
pub fn caller_state(
    def: &Connector,
    node: Option<&NodeConnector>,
    connection: Option<CallerState>,
) -> CallerState {
    let Some(n) = node.filter(|n| n.enabled) else {
        return CallerState::AskAdmin;
    };
    if needs_admin_consent(def) && n.consent_tenant_id.is_none() {
        return CallerState::WaitingForConsent;
    }
    connection.unwrap_or(CallerState::Connect)
}

/// One entry of `GET /v1/connectors`.
///
/// A connector the node has not enabled shows name, icon, description and publisher only (no
/// scopes, no client id, no endpoint). An enabled one adds the public definition with this node's
/// client id. `viewer_is_admin` adds who recorded the consent. No vault id or token is ever here.
pub fn list_entry(
    def: &Connector,
    cfg: &ConnectorConfig,
    node: Option<&NodeConnector>,
    connection: Option<CallerState>,
    viewer_is_admin: bool,
) -> Value {
    let id = connector_id(def);
    let state = caller_state(def, node, connection);
    let enabled = node.map(|n| n.enabled).unwrap_or(false);
    let mut node_view = json!({ "enabled": enabled, "configured": is_configured(def, cfg) });
    let mut entry = json!({
        "id": id,
        "displayName": def.display_name,
        "publisher": def.publisher,
        "description": def.description,
        "icon": def.icon,
        "state": state,
        "label": state.label(),
    });
    if let Some(n) = node {
        if let Some(t) = &n.consent_tenant_id {
            let mut consent = json!({ "tenantId": t, "recordedAt": n.consent_recorded_at });
            if viewer_is_admin {
                consent["recordedBy"] = json!(n.consent_recorded_by);
            }
            node_view["consent"] = consent;
        }
        if viewer_is_admin {
            node_view["updatedAt"] = json!(n.updated_at);
        }
        if let (Some(sub), Some(param)) = (&n.tenant_subdomain, &def.tenant) {
            if enabled || viewer_is_admin {
                node_view["tenant"] = json!({
                    "subdomain": sub,
                    "label": param.label,
                    "hosts": vam_spec::connector::filled_hosts(def, sub),
                });
            }
        }
    }
    if enabled {
        let mut public = serde_json::to_value(def).unwrap_or(Value::Null);
        if let (Some(cid), Some(p)) = (effective_client_id(def, cfg), public.get_mut("provider")) {
            if p.get("clientId").is_some() {
                p["clientId"] = json!(cid);
            }
        }
        if let (Value::Object(src), Value::Object(dst)) = (public, &mut entry) {
            for k in [
                "urn",
                "version",
                "provider",
                "connection",
                "bindings",
                "mcp",
            ] {
                if let Some(v) = src.get(k) {
                    dst.insert(k.to_string(), v.clone());
                }
            }
        }
    }
    entry["node"] = node_view;
    entry
}

/// True when `conn` is a member's connection to a connector a marketplace installed, on another
/// server than the connector names now, that an update marked `needs_reauth`. Such a row cannot
/// be reconnected (a connection never changes host), so a connect to the connector's own URL
/// replaces it.
pub fn is_stale_installed_row(
    store: &SqliteWorkflowStore,
    conn: &McpConnection,
    request_url: &str,
) -> Result<bool, OrchestratorError> {
    if conn.scope != SecretScope::User
        || conn.oauth.as_ref().map(|o| o.status) != Some(McpOauthStatus::NeedsReauth)
    {
        return Ok(false);
    }
    let Some(entry) = store.connector_entry(&conn.connection_id)? else {
        return Ok(false);
    };
    Ok(entry.installed.is_some()
        && policy_of(&conn.connection_id, &entry.def)
            .is_some_and(|p| p.url != conn.url && p.url == request_url))
}

/// Adds where a connector came from to its `GET /v1/connectors` entry: `source` (`builtin` or
/// `marketplace`), `trust` (`squrell` or `unverified`) and, for a marketplace connector, `origin`
/// (marketplace, listing, tag, commit, package digest). The admin sees `capabilities` (publisher,
/// sign-in profile and client id, scopes, the MCP server and its hosts, the vetted tools) whether or
/// not the connector is enabled; a member sees it once it is. Nothing here is a secret.
pub fn annotate_source(entry: &mut Value, e: &ConnectorEntry, viewer_is_admin: bool) {
    let enabled = entry["node"]["enabled"].as_bool().unwrap_or(false);
    entry["source"] = json!(if e.installed.is_some() {
        "marketplace"
    } else {
        "builtin"
    });
    entry["trust"] = json!(e.trust_word());
    let Some(m) = &e.installed else {
        return;
    };
    entry["origin"] = json!({
        "marketplace": m.marketplace_id,
        "marketplaceName": m.marketplace_name,
        "publisherDomain": m.publisher_domain,
        "listing": m.listing,
        "ref": m.git_ref,
        "commit": m.commit,
        "packageDigest": m.package_digest,
    });
    if viewer_is_admin || enabled {
        // Before an address is entered the admin reads the file as published; after it, every host
        // is the filled one.
        let shown = if entry["node"]["tenant"].is_object() {
            &e.def
        } else {
            &e.template
        };
        let summary = vam_spec::package::summarize_connector(shown);
        if let Some(c) = summary.connector {
            entry["capabilities"] = serde_json::to_value(c).unwrap_or(Value::Null);
        }
    }
}

// ---- the REST connection: Microsoft 365 (CN-1.4) ----------------------------------------------
//
// A second writer of `connection`-kind vault rows besides `mcp_registry` (see the note on
// `SecretKind::Connection`). It writes exactly two rows per user and connector,
// `connector/<id>/oauth/access-token` and `.../refresh-token`; the table holds no token.
//
// Three rules live here and nowhere else:
// - the tenant: `PUT` is refused (409, nothing written) unless the id token's `tid` equals the
//   admin's recorded consent tenant (open question 8: the claim is read, the signature is not);
// - the endpoint: a refresh goes to `entra_token_endpoint(<consent tenant>)`, built from the GUID
//   on the node's consent record, never from a request or from the stored row;
// - `resource`: the Entra refresh profile has no `resource` field (`mcp_oauth_refresh.rs`).

/// Where one user's connection to a REST connector stands. Needs a check to leave `Connected`
/// (open question 11); a `PUT` sets it back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionStatus {
    Connected,
    NeedsReconnect,
}

impl ConnectionStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Connected => "connected",
            Self::NeedsReconnect => "needs_reconnect",
        }
    }

    fn parse(s: &str) -> Self {
        if s == "connected" {
            Self::Connected
        } else {
            Self::NeedsReconnect
        }
    }
}

/// One row of `connector_connections`: the account facts, never a token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectorConnection {
    pub owner_user_id: String,
    pub connector_id: String,
    pub status: ConnectionStatus,
    /// The account as the tile shows it (`preferred_username` of the id token).
    pub label: Option<String>,
    /// The tenant the tokens were issued in (equal to the consent tenant when written).
    pub tenant_id: String,
    pub token_endpoint: String,
    pub scopes: Vec<String>,
    pub expires_at: Option<i64>,
    pub connected_at: i64,
    pub updated_at: i64,
    pub last_check_at: Option<i64>,
    /// A fixed string from a failed check; never token material or an AS body.
    pub last_error: Option<String>,
    /// The OAuth client id the grant was issued to (an `oauth` connector: the admin's client at the
    /// code exchange). Revocation authenticates as this client, never as whatever client the node
    /// has now. `None`: an Entra connection (the Squrell client), or a row from before it was kept.
    pub client_id: Option<String>,
}

/// Body of `PUT /v1/connectors/{id}/connection`. Holds secrets: no `Debug` output of its fields.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PutConnectionRequest {
    pub access_token: String,
    pub refresh_token: String,
    /// The OpenID Connect id token. Read for `tid` and `preferred_username`, then dropped.
    pub id_token: String,
    /// Seconds the access token lives, from the token response (optional).
    #[serde(default)]
    pub expires_in: Option<u64>,
}

impl std::fmt::Debug for PutConnectionRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PutConnectionRequest { .. }")
    }
}

/// `connector/<id>/oauth/access-token`: the access token (`connection` kind).
pub fn access_token_binding_id(connector_id: &str) -> String {
    format!("connector/{connector_id}/oauth/access-token")
}

/// `connector/<id>/oauth/refresh-token`: the refresh token (`connection` kind).
pub fn refresh_token_binding_id(connector_id: &str) -> String {
    format!("connector/{connector_id}/oauth/refresh-token")
}

/// The Entra provider of a REST connector, or why this id cannot take a REST connection.
fn entra_provider(def: &Connector) -> Result<&OAuth2Provider, OrchestratorError> {
    let id = connector_id(def);
    match oauth(def) {
        Some(o) if def.mcp.is_none() && o.profile == OAuthProfile::Entra => Ok(o),
        _ if def.mcp.is_some() => Err(invalid(format!(
            "`{id}` is an MCP connector; connect it through /v1/mcp/connections"
        ))),
        Some(o) if o.profile == OAuthProfile::Oauth => Err(invalid(exchange_only(&id))),
        _ => Err(invalid(format!(
            "`{id}` has no connection route (only the entra profile is supported)"
        ))),
    }
}

/// The refusal text of a connection route that cannot take an `oauth` connector's tokens: the node
/// exchanges the code, the app never holds the tokens (CN-6.3).
pub(crate) fn exchange_only(id: &str) -> String {
    format!(
        "`{id}` connects through the node's code exchange: use POST /v1/connectors/{id}/connection/oauth/code"
    )
}

/// The provider of a REST connector the node refreshes: Entra, or `oauth` (fixed endpoints).
fn rest_provider(def: &Connector) -> Result<&OAuth2Provider, OrchestratorError> {
    match oauth(def) {
        Some(o) if def.mcp.is_none() && o.profile == OAuthProfile::Oauth => Ok(o),
        _ => entra_provider(def),
    }
}

/// Every host an `oauth` connector's access token is pinned to: each host of each binding of the
/// filled connector, once. (Entra keeps its one Graph host.)
pub(crate) fn binding_hosts(def: &Connector) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for b in def.bindings.iter().flatten() {
        for h in &b.hosts {
            if !out.iter().any(|x| x == h) {
                out.push(h.clone());
            }
        }
    }
    out
}

/// Claims read from an id token. The signature is not checked (open question 8).
#[derive(Debug, PartialEq, Eq)]
pub struct IdClaims {
    pub tid: String,
    pub label: Option<String>,
}

const MAX_ID_TOKEN_BYTES: usize = 16 * 1024;

/// Reads `tid` and the account name from a JWT. Errors are fixed strings: nothing of the token.
pub fn read_id_token(token: &str) -> Result<IdClaims, OrchestratorError> {
    use base64::Engine as _;
    let bad = || invalid("idToken is not a readable id token");
    if token.is_empty() || token.len() > MAX_ID_TOKEN_BYTES {
        return Err(bad());
    }
    let mut parts = token.split('.');
    let (_, Some(payload), Some(_), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(bad());
    };
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .map_err(|_| bad())?;
    let claims: Value = serde_json::from_slice(&raw).map_err(|_| bad())?;
    let tid = claims
        .get("tid")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("idToken has no tid claim"))?
        .to_string();
    let label = ["preferred_username", "upn", "email"]
        .iter()
        .filter_map(|k| claims.get(*k).and_then(Value::as_str))
        .find_map(clean_label);
    Ok(IdClaims { tid, label })
}

/// A label is shown on a tile: short, printable, no control characters.
fn clean_label(s: &str) -> Option<String> {
    let t = s.trim();
    (!t.is_empty() && t.chars().count() <= 200 && !t.chars().any(char::is_control))
        .then(|| t.to_string())
}

/// What a `PUT …/connection` will write once every rule has passed.
#[derive(Debug, PartialEq, Eq)]
pub struct ConnectionPlan {
    pub tenant_id: String,
    pub label: Option<String>,
    pub token_endpoint: String,
    pub scopes: Vec<String>,
    /// Exact host the access token is pinned to in the vault row (the definition's first binding).
    pub access_host: String,
    pub token_host: String,
    pub expires_at: Option<i64>,
}

/// Decides a `PUT …/connection`: the rows to write, or the refusal. Nothing is written here, so a
/// refusal of any kind (not enabled, no consent, another tenant, a bad token) leaves no trace.
///
/// - not an Entra REST connector: 400; not enabled on this node: 403; no consent recorded: 409;
/// - an empty or oversized token, an unreadable id token: 400 (a fixed message, never the token);
/// - the id token's `tid` is not the consent tenant: 409.
pub fn plan_connection(
    def: &Connector,
    node: Option<&NodeConnector>,
    req: &PutConnectionRequest,
    now: i64,
) -> Result<ConnectionPlan, OrchestratorError> {
    let id = connector_id(def);
    let provider = entra_provider(def)?;
    let node = node.filter(|n| n.enabled).ok_or_else(|| {
        OrchestratorError::PermissionDenied(format!(
            "the `{id}` connector is not enabled on this node; ask your admin (`vam connectors`)"
        ))
    })?;
    let consent = node.consent_tenant_id.as_deref().ok_or_else(|| {
        OrchestratorError::Conflict(format!(
            "an admin has not recorded Microsoft admin consent for `{id}` yet"
        ))
    })?;
    for (name, value) in [
        ("accessToken", &req.access_token),
        ("refreshToken", &req.refresh_token),
    ] {
        validate_oauth_secret_value(value).map_err(|_| {
            invalid(format!(
                "{name} is empty, too long or has a control character"
            ))
        })?;
    }
    let claims = read_id_token(&req.id_token)?;
    if parse_tenant_guid(&claims.tid).as_deref() != Some(consent) {
        return Err(OrchestratorError::Conflict(format!(
            "this sign-in is from a different Microsoft tenant than the one the admin consented for `{id}`"
        )));
    }
    let token_endpoint = entra_token_endpoint(consent)
        .ok_or_else(|| OrchestratorError::CorruptState("consent tenant is not a GUID".into()))?;
    let access_host = def
        .bindings
        .as_ref()
        .and_then(|b| b.first())
        .and_then(|b| b.hosts.first())
        .cloned()
        .unwrap_or_else(|| "login.microsoftonline.com".to_string());
    Ok(ConnectionPlan {
        tenant_id: consent.to_string(),
        label: claims.label,
        token_endpoint,
        scopes: provider.scopes.clone(),
        access_host,
        token_host: "login.microsoftonline.com".to_string(),
        expires_at: req
            .expires_in
            .map(|s| now.saturating_add(i64::try_from(s).unwrap_or(i64::MAX))),
    })
}

/// Writes a planned connection: the two `connection`-kind vault rows, then the table row. The
/// refresh token goes first so an access token never exists without the means to renew it.
#[allow(clippy::too_many_arguments)]
pub(crate) fn write_connection(
    store: &SqliteWorkflowStore,
    vault: &SecretsVault,
    owner: &str,
    id: &str,
    plan: &ConnectionPlan,
    req: &PutConnectionRequest,
    idempotency_key: Option<&str>,
    now: i64,
) -> Result<ConnectorConnection, OrchestratorError> {
    let idem = |binding: &str| idempotency_key.map(|k| format!("{k}:{binding}"));
    let (rt, at) = (refresh_token_binding_id(id), access_token_binding_id(id));
    for (binding, value, host) in [
        (&rt, &req.refresh_token, &plan.token_host),
        (&at, &req.access_token, &plan.access_host),
    ] {
        vault.put_secret_with_owner(
            binding,
            value,
            SecretKind::Connection,
            std::slice::from_ref(host),
            idem(binding).as_deref(),
            owner,
        )?;
    }
    let row = ConnectorConnection {
        owner_user_id: owner.to_string(),
        connector_id: id.to_string(),
        status: ConnectionStatus::Connected,
        label: plan.label.clone(),
        tenant_id: plan.tenant_id.clone(),
        token_endpoint: plan.token_endpoint.clone(),
        scopes: plan.scopes.clone(),
        expires_at: plan.expires_at,
        connected_at: now,
        updated_at: now,
        last_check_at: None,
        last_error: None,
        client_id: None,
    };
    if let Err(err) = store.put_connector_connection(&row) {
        delete_connection_secrets(vault, owner, id);
        return Err(err);
    }
    store.resolve_connection_needs_reauth(owner, id, "acted");
    Ok(row)
}

/// Removes both token rows (a missing one is fine).
pub(crate) fn delete_connection_secrets(vault: &SecretsVault, owner: &str, id: &str) {
    for binding in [refresh_token_binding_id(id), access_token_binding_id(id)] {
        let _ = vault.delete_secret_at(SecretScope::User, owner, &binding);
    }
}

/// `DELETE …/connection`: the rows, the token secrets. `NotFound` when there is no connection.
pub(crate) fn disconnect(
    store: &SqliteWorkflowStore,
    vault: &SecretsVault,
    owner: &str,
    id: &str,
) -> Result<(), OrchestratorError> {
    if !store.delete_connector_connection(owner, id)? {
        // Rows without a table row (an interrupted write) still go.
        delete_connection_secrets(vault, owner, id);
        return Err(OrchestratorError::NotFound(format!(
            "connection to `{id}` (you are not connected)"
        )));
    }
    delete_connection_secrets(vault, owner, id);
    store.resolve_connection_needs_reauth(owner, id, "dismissed");
    Ok(())
}

/// The caller's state from their own connection row. `Connected` only while the tokens exist and
/// the connection is in the tenant the admin consented for now; otherwise `NeedsReconnect`.
pub fn rest_connection_state(
    conn: &ConnectorConnection,
    node: Option<&NodeConnector>,
    has_refresh_token: bool,
    node_token_endpoint: Option<&str>,
) -> CallerState {
    let same_tenant = node
        .and_then(|n| n.consent_tenant_id.as_deref())
        .map(|t| t == conn.tenant_id)
        .unwrap_or(true);
    // A connection made under another company address is not this company's (CN-6.2; the code
    // exchange writes the address it signed in under as `tenant_id`).
    let same_address = node
        .and_then(|n| n.tenant_subdomain.as_deref())
        .map(|t| t == conn.tenant_id)
        .unwrap_or(true);
    // The token endpoint the connection was made against is the one the node's client has now (an
    // `oauth` connector; `None` for Entra, whose endpoint follows the consent tenant above).
    let same_endpoint = node_token_endpoint
        .map(|e| e == conn.token_endpoint)
        .unwrap_or(true);
    let same_tenant = same_tenant && same_address && same_endpoint;
    if conn.status == ConnectionStatus::Connected && has_refresh_token && same_tenant {
        CallerState::Connected
    } else {
        CallerState::NeedsReconnect
    }
}

/// On a connected tile the label is the account (`member@contoso.com`); the app writes
/// "Connected as …" around it. Any other state keeps its fixed label.
pub fn show_account(entry: &mut Value, state: CallerState, account: Option<&str>) {
    if let (CallerState::Connected, Some(a)) = (state, account) {
        entry["label"] = json!(a);
    }
}

/// What one refresh of a REST connection is built from: the caller builds it from the node's own
/// state (the consent tenant, or the client the admin saved), never from a member's row or a
/// request. Entra: the endpoint is derived from the tenant GUID. `oauth`: the endpoint is the node
/// client's, pinned at Enable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RestRefreshTarget<'a> {
    Entra {
        tenant: &'a str,
        client_id: &'a str,
        scope: &'a str,
    },
    Oauth {
        token_endpoint: &'a str,
        client_id: &'a str,
        scope: Option<&'a str>,
    },
}

/// A refresh as `check_connection` and the swap-time refresh call it: the target and the refresh
/// token. Shared (and `Send`) so a session's token source can hold one.
pub(crate) type RestRefresher = std::sync::Arc<
    dyn for<'a> Fn(&RestRefreshTarget<'a>, &SecretString) -> Result<RefreshedToken, RefreshFailure>
        + Send
        + Sync,
>;

/// The real refresh. Entra: the consent tenant's endpoint, no `resource`, no client secret. `oauth`:
/// the node client's endpoint, no `resource`, no client secret, `scope` only if the file lists some.
pub(crate) fn refresh_rest_token(
    target: &RestRefreshTarget<'_>,
    refresh_token: &SecretString,
) -> Result<RefreshedToken, RefreshFailure> {
    match *target {
        RestRefreshTarget::Entra {
            tenant,
            client_id,
            scope,
        } => crate::mcp_oauth_refresh::refresh_access_token_entra(
            tenant,
            client_id,
            scope,
            refresh_token,
        ),
        RestRefreshTarget::Oauth {
            token_endpoint,
            client_id,
            scope,
        } => crate::mcp_oauth_refresh::refresh_access_token_oauth(
            token_endpoint,
            client_id,
            scope,
            refresh_token,
        ),
    }
}

pub(crate) fn live_refresher() -> RestRefresher {
    std::sync::Arc::new(refresh_rest_token)
}

/// A token whose expiry is within this many seconds is refreshed before it is attached (open
/// question 5).
pub(crate) const SWAP_REFRESH_WINDOW_SECS: i64 = 120;
/// Lifetime assumed when a refresh response omits `expires_in` (Entra always sends it).
const DEFAULT_ACCESS_LIFETIME_SECS: i64 = 300;

/// How one explicit check ended.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CheckOutcome {
    Connected(ConnectorConnection),
    NeedsReconnect(ConnectorConnection),
    /// Microsoft could not be reached or answered with an error that may pass; nothing changed
    /// but the check time. The string is fixed text.
    Unavailable(String),
}

/// One refresh at the consent tenant, to learn whether the connection still works (open question
/// 11: the only thing that moves a connection to `needsReconnect`).
///
/// A connection outside the consented tenant, or without its refresh token, is `NeedsReconnect`
/// without a request. A refresh the server rejects (`invalid_grant` and the other 4xx) is
/// `NeedsReconnect`; a network or 5xx failure is `Unavailable` and changes no status. A rotated
/// refresh token replaces the stored one before the new access token is written.
pub(crate) fn check_connection(
    store: &SqliteWorkflowStore,
    vault: &SecretsVault,
    cfg: &ConnectorConfig,
    owner: &str,
    id: &str,
    now: i64,
    refresh: impl for<'a> Fn(
        &RestRefreshTarget<'a>,
        &SecretString,
    ) -> Result<RefreshedToken, RefreshFailure>,
) -> Result<CheckOutcome, OrchestratorError> {
    refresh_connection(store, vault, cfg, owner, id, now, &refresh)
}

/// What a swap-time (or intent-time) freshness check found. Fixed outcomes only: no text from
/// the authorization server reaches a caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Freshness {
    /// The access token is not near expiry (or its expiry is not known): nothing was sent.
    Fresh,
    /// One refresh was sent and the new tokens are stored.
    Refreshed,
    /// The grant was refused, or the connection is outside the consented tenant or has no
    /// refresh token: the status is now `needs_reconnect` and nothing usable remains.
    NeedsReconnect,
    /// Microsoft could not be reached (or a row could not be written): no status changed.
    Unavailable,
}

/// Whether the stored access token expires within `horizon_secs` of `now`. An unknown expiry
/// (`None`) is not due: the app always sends `expiresIn` on connect, and a refresh response
/// always sets one.
pub(crate) fn token_due(conn: &ConnectorConnection, now: i64, horizon_secs: i64) -> bool {
    conn.expires_at
        .is_some_and(|e| e <= now.saturating_add(horizon_secs))
}

/// A token the intent check refreshed for this call is not refreshed again by the swap while it
/// has at least this long left.
const INTENT_REFRESH_MIN_LEFT_SECS: i64 = 20;

/// Whether a check at `now` with `horizon_secs` must refresh: [`token_due`], except that the
/// swap of a call whose intent check just refreshed (`intent_refreshed`) does not spend a second
/// single-use refresh token while the token has at least 20 s left. One tool call is checked
/// twice, at the intent (the tool's timeout as horizon) and at the swap (the 120 s window); a
/// provider whose access tokens live a minute is inside both windows. `intent_refreshed` is
/// per call (`connection_egress::IntentRefreshes`, keyed by the call id), never a time window, and
/// is false for a call that waited on an approval (its intent check runs again). The one place
/// both checks (`refresh_if_due` and `connection_egress::ensure_fresh`) decide.
pub(crate) fn needs_refresh(
    conn: &ConnectorConnection,
    now: i64,
    horizon_secs: i64,
    intent_refreshed: bool,
) -> bool {
    if !token_due(conn, now, horizon_secs) {
        return false;
    }
    let left = conn.expires_at.map_or(0, |e| e.saturating_sub(now));
    !(intent_refreshed && left >= INTENT_REFRESH_MIN_LEFT_SECS)
}

/// Refreshes the member's access token if it expires within `horizon_secs`. The caller must hold
/// [`lock_connection`] for `(owner, id)`; the expiry is read again here, under that lock, so a
/// refresh a concurrent caller just finished is not sent twice.
#[allow(clippy::too_many_arguments)]
#[allow(clippy::type_complexity)] // the injected refresh call; the Fn signature is the type
pub(crate) fn refresh_if_due(
    store: &SqliteWorkflowStore,
    vault: &SecretsVault,
    cfg: &ConnectorConfig,
    owner: &str,
    id: &str,
    now: i64,
    horizon_secs: i64,
    intent_refreshed: bool,
    refresh: &dyn for<'a> Fn(
        &RestRefreshTarget<'a>,
        &SecretString,
    ) -> Result<RefreshedToken, RefreshFailure>,
) -> Freshness {
    let conn = match store.connector_connection(owner, id) {
        Ok(Some(c)) => c,
        _ => return Freshness::Unavailable,
    };
    if conn.status != ConnectionStatus::Connected {
        return Freshness::NeedsReconnect;
    }
    if !needs_refresh(&conn, now, horizon_secs, intent_refreshed) {
        return Freshness::Fresh;
    }
    match refresh_connection(store, vault, cfg, owner, id, now, refresh) {
        Ok(CheckOutcome::Connected(_)) => Freshness::Refreshed,
        Ok(CheckOutcome::NeedsReconnect(_)) => Freshness::NeedsReconnect,
        Ok(CheckOutcome::Unavailable(_)) | Err(_) => Freshness::Unavailable,
    }
}

/// What a refresh needs once the node's own state has been read and checked, per profile.
enum Prepared {
    Entra {
        consent: String,
        client_id: String,
        scope: String,
    },
    Oauth {
        token_endpoint: String,
        client_id: String,
        /// The node client's revision when the refresh was decided: the write checks it again.
        client_revision: i64,
        scope: Option<String>,
    },
}

/// Whether `a` and `b` are the same grant: the member signed in once, and the row was not marked
/// since. What a refresh read and what the database holds when its result is written.
fn same_grant(a: &ConnectorConnection, b: &ConnectorConnection) -> bool {
    a.status == b.status
        && a.connected_at == b.connected_at
        && a.tenant_id == b.tenant_id
        && a.token_endpoint == b.token_endpoint
        && a.client_id == b.client_id
}

/// The 409 text of a refresh result that is not written because the state it was decided on moved.
const REFRESH_SUPERSEDED: &str =
    "the connection or the connector changed while it was being checked";

#[allow(clippy::type_complexity)] // the injected refresh call; the Fn signature is the type
fn refresh_connection(
    store: &SqliteWorkflowStore,
    vault: &SecretsVault,
    cfg: &ConnectorConfig,
    owner: &str,
    id: &str,
    now: i64,
    refresh: &dyn for<'a> Fn(
        &RestRefreshTarget<'a>,
        &SecretString,
    ) -> Result<RefreshedToken, RefreshFailure>,
) -> Result<CheckOutcome, OrchestratorError> {
    let def = store
        .connector_entry(id)?
        .ok_or_else(|| OrchestratorError::NotFound(format!("connector `{id}`")))?
        .def;
    let provider = rest_provider(&def)?;
    let mut conn = store.connector_connection(owner, id)?.ok_or_else(|| {
        OrchestratorError::NotFound(format!("connection to `{id}` (you are not connected)"))
    })?;
    let node = store
        .node_connector(id)?
        .filter(|n| n.enabled)
        .ok_or_else(|| {
            OrchestratorError::PermissionDenied(format!(
            "the `{id}` connector is not enabled on this node; ask your admin (`vam connectors`)"
        ))
        })?;
    // The row as this refresh read it: what the write at the end must still find.
    let read = conn.clone();

    let needs_reconnect = |conn: &mut ConnectorConnection, why: &str| {
        let was = conn.status;
        conn.status = ConnectionStatus::NeedsReconnect;
        conn.last_check_at = Some(now);
        conn.last_error = Some(why.to_string());
        conn.updated_at = now;
        store.put_connector_connection(conn)?;
        // Once per transition: a repeat failure while already needs-reconnect adds nothing.
        if was != ConnectionStatus::NeedsReconnect {
            store.raise_connection_needs_reauth(owner, id, &def.display_name);
        }
        Ok::<_, OrchestratorError>(CheckOutcome::NeedsReconnect(conn.clone()))
    };

    // Where the tokens are pinned, and the target of the refresh, decided from the node's state.
    let (prepared, token_host, access_hosts) = if provider.profile == OAuthProfile::Oauth {
        // A grant the admin invalidated (a replaced or removed client) is never presented again, by
        // a check any more than by a refresh: only a new sign-in under the current client brings it
        // back. Decided before the refresh token is read.
        if conn.status == ConnectionStatus::NeedsReconnect {
            return Ok(CheckOutcome::NeedsReconnect(conn));
        }
        // The node client is the only source of the endpoint and the client id (pinned at Enable
        // from the filled file); a connection row never names them.
        let Some(client) = store.node_client(id)? else {
            return needs_reconnect(&mut conn, "the admin removed the OAuth client");
        };
        let filled = provider
            .endpoints
            .as_ref()
            .map(|e| e.token.as_str())
            .filter(|t| !url_needs_address(t));
        // A refresh token is sent only to the endpoint the connector file names now, for the address
        // it was made under, and only if that is the endpoint the client was pinned to. And only as
        // the client the grant was issued to.
        if filled != Some(client.token_endpoint.as_str())
            || address_key(node.tenant_subdomain.as_deref()) != conn.tenant_id
            || conn.token_endpoint != client.token_endpoint
            || conn
                .client_id
                .as_deref()
                .is_some_and(|issued| issued != client.client_id)
        {
            return needs_reconnect(
                &mut conn,
                "connected under a different company address, client or sign-in endpoint than the current one",
            );
        }
        let Some(token_host) = crate::connector_discovery::host_of(&client.token_endpoint) else {
            return needs_reconnect(&mut conn, "the sign-in endpoint is not usable");
        };
        let scope = (!provider.scopes.is_empty()).then(|| provider.scopes.join(" "));
        let mut hosts = binding_hosts(&def);
        if hosts.is_empty() {
            hosts.push(token_host.clone());
        }
        (
            Prepared::Oauth {
                token_endpoint: client.token_endpoint.clone(),
                client_id: client.client_id.clone(),
                client_revision: client.revision,
                scope,
            },
            token_host,
            hosts,
        )
    } else {
        let consent = node.consent_tenant_id.clone().ok_or_else(|| {
            OrchestratorError::Conflict(format!(
                "an admin has not recorded Microsoft admin consent for `{id}` yet"
            ))
        })?;
        let client_id = effective_client_id(&def, cfg)
            .filter(|c| !is_placeholder_client_id(c))
            .ok_or_else(|| OrchestratorError::Conflict(format!("`{id}` is not configured")))?;
        // The refresh token belongs to the tenant it was issued in. If the admin has since
        // consented for another tenant, it is never sent anywhere.
        let endpoint_matches =
            entra_token_endpoint(&consent).as_deref() == Some(&conn.token_endpoint);
        if conn.tenant_id != consent || !endpoint_matches {
            return needs_reconnect(
                &mut conn,
                "connected in a different tenant than the consented one",
            );
        }
        let token_host = "login.microsoftonline.com".to_string();
        let access_host = def
            .bindings
            .as_ref()
            .and_then(|b| b.first())
            .and_then(|b| b.hosts.first())
            .cloned()
            .unwrap_or_else(|| token_host.clone());
        (
            Prepared::Entra {
                consent,
                client_id,
                scope: provider.scopes.join(" "),
            },
            token_host,
            vec![access_host],
        )
    };

    let rt_binding = refresh_token_binding_id(id);
    if !vault.has_binding_at(SecretScope::User, owner, &rt_binding)? {
        return needs_reconnect(&mut conn, "refresh token is missing");
    }
    let refresh_token = match &prepared {
        // An `oauth` refresh token is read only if its row is pinned to exactly the token host it is
        // about to be sent to (it was pinned there when the code was exchanged).
        Prepared::Oauth { .. } => {
            let pinned = vault.decrypt_secret_at_checked(
                SecretScope::User,
                owner,
                &rt_binding,
                |meta: &crate::secrets::HostSecretMetadata| {
                    let ok = meta.kind == SecretKind::Connection
                        && meta.allowed_destinations.len() == 1
                        && meta.allowed_destinations[0].eq_ignore_ascii_case(&token_host);
                    if ok {
                        Ok(())
                    } else {
                        Err(OrchestratorError::PermissionDenied(
                            "the refresh token is not pinned to the token endpoint's host".into(),
                        ))
                    }
                },
            );
            match pinned {
                Err(OrchestratorError::PermissionDenied(_)) => {
                    return needs_reconnect(
                        &mut conn,
                        "refresh token is not pinned to the sign-in endpoint's host",
                    )
                }
                other => other?,
            }
        }
        Prepared::Entra { .. } => vault.decrypt_secret_at(SecretScope::User, owner, &rt_binding)?,
    };
    let target = match &prepared {
        Prepared::Entra {
            consent,
            client_id,
            scope,
        } => RestRefreshTarget::Entra {
            tenant: consent,
            client_id,
            scope,
        },
        Prepared::Oauth {
            token_endpoint,
            client_id,
            scope,
            ..
        } => RestRefreshTarget::Oauth {
            token_endpoint,
            client_id,
            scope: scope.as_deref(),
        },
    };
    let declared_hosts = binding_hosts(&def);
    // The provider may take seconds, and an admin's save does not wait for the member's lock. What
    // the refresh was decided on is checked again inside the transaction that stores its result:
    // the member's row (not marked or replaced since), the enable, and for an `oauth` connector the
    // client (by revision), the address and the filled definition. A result that no longer fits is
    // dropped, and the newer row and tokens stay as they are.
    let still_current = |tx: &rusqlite::Connection| -> Result<(), OrchestratorError> {
        let moved = || OrchestratorError::Conflict(REFRESH_SUPERSEDED.into());
        let row_now = connector_connection_in(tx, owner, id)?.ok_or_else(moved)?;
        let node_now = node_connector_in(tx, id)?.ok_or_else(moved)?;
        if !same_grant(&row_now, &read) || !node_now.enabled {
            return Err(moved());
        }
        match &prepared {
            Prepared::Entra { consent, .. } => {
                if node_now.consent_tenant_id.as_deref() != Some(consent.as_str()) {
                    return Err(moved());
                }
            }
            Prepared::Oauth {
                token_endpoint,
                client_id,
                client_revision,
                ..
            } => {
                let same_client = crate::connector_client::client_in(tx, id)?.is_some_and(|n| {
                    n.revision == *client_revision
                        && n.client_id == *client_id
                        && n.token_endpoint == *token_endpoint
                });
                let same_address =
                    address_key(node_now.tenant_subdomain.as_deref()) == read.tenant_id;
                let same_file = entry_in(tx, id)?.is_some_and(|e| {
                    let endpoint = match &e.def.provider {
                        vam_spec::connector::Provider::Oauth2(o) => {
                            o.endpoints.as_ref().map(|x| x.token.clone())
                        }
                        vam_spec::connector::Provider::ApiKey(_) => None,
                    };
                    endpoint.as_deref() == Some(token_endpoint.as_str())
                        && binding_hosts(&e.def) == declared_hosts
                });
                if !(same_client && same_address && same_file) {
                    return Err(moved());
                }
            }
        }
        Ok(())
    };
    // A rotated refresh token is the credential live at the provider once the provider answered:
    // the one it replaced is spent. It is kept whenever the grant it renews is still valid: the
    // member's row not marked or replaced since (an admin's save that invalidated the grant marks
    // it) and the same issuing client at the same token endpoint. A save that kept that client
    // (a resave advances the revision) or a disable meanwhile does not drop it; the access token
    // and the Connected result still need `still_current`. For Microsoft 365 the issuer is the
    // consent tenant: the rotated token is kept while the admin's consent is the one it was
    // refreshed under, enabled or not; a consent withdrawn or recorded for another tenant drops it.
    let grant_kept = |tx: &rusqlite::Connection| -> Result<(), OrchestratorError> {
        let moved = || OrchestratorError::Conflict(REFRESH_SUPERSEDED.into());
        let row_now = connector_connection_in(tx, owner, id)?.ok_or_else(moved)?;
        let same_issuer = match &prepared {
            Prepared::Entra { consent, .. } => node_connector_in(tx, id)?
                .is_some_and(|n| n.consent_tenant_id.as_deref() == Some(consent.as_str())),
            Prepared::Oauth {
                token_endpoint,
                client_id,
                ..
            } => crate::connector_client::client_in(tx, id)?
                .is_some_and(|n| n.client_id == *client_id && n.token_endpoint == *token_endpoint),
        };
        if same_grant(&row_now, &read) && same_issuer {
            Ok(())
        } else {
            Err(moved())
        }
    };
    // One write, tried twice (a busy database is the usual failure); `Ok(false)`: not written
    // because the state moved.
    let apply = |rotated_refresh: Option<&str>,
                 access_token: Option<&str>,
                 row: Option<&ConnectorConnection>,
                 guard: &dyn Fn(&rusqlite::Connection) -> Result<(), OrchestratorError>|
     -> Result<bool, OrchestratorError> {
        let commit = RefreshCommit {
            owner,
            connector_id: id,
            token_host: &token_host,
            access_hosts: &access_hosts,
            rotated_refresh,
            access_token,
            row,
        };
        let mut done = store.commit_rest_refresh(vault, &commit, guard);
        if matches!(&done, Err(e) if !matches!(e, OrchestratorError::Conflict(_))) {
            done = store.commit_rest_refresh(vault, &commit, guard);
        }
        match done {
            Ok(()) => Ok(true),
            Err(OrchestratorError::Conflict(_)) => Ok(false),
            Err(e) => Err(e),
        }
    };
    // What a dropped result answers: the row as it is now (marked by the admin: needs reconnect), or
    // that the check could not be completed.
    let superseded = || -> Result<CheckOutcome, OrchestratorError> {
        Ok(match store.connector_connection(owner, id)? {
            Some(row) if row.status == ConnectionStatus::NeedsReconnect => {
                CheckOutcome::NeedsReconnect(row)
            }
            _ => CheckOutcome::Unavailable(REFRESH_SUPERSEDED.into()),
        })
    };
    match refresh(&target, &refresh_token) {
        Ok(token) => {
            conn.status = ConnectionStatus::Connected;
            conn.expires_at = Some(
                now.saturating_add(
                    token
                        .expires_in
                        .map(|s| i64::try_from(s).unwrap_or(i64::MAX))
                        .unwrap_or(DEFAULT_ACCESS_LIFETIME_SECS),
                ),
            );
            conn.last_check_at = Some(now);
            conn.last_error = None;
            conn.updated_at = now;
            // A rotated refresh token first and alone: it is the credential that is live at the
            // provider, kept whatever happens to the write after it, and the access token never
            // outlives its renewal. Then the access token and its expiry in ONE write, so the token is
            // never beside another token's expiry. If the second write fails the new access token is
            // never used: the stored row stays due, and the next refresh sends the rotated token.
            if let Some(rotated) = token.refresh_token.as_deref() {
                if !apply(Some(rotated), None, None, &grant_kept)? {
                    return superseded();
                }
            }
            if apply(
                None,
                Some(token.access_token.as_str()),
                Some(&conn),
                &still_current,
            )? {
                Ok(CheckOutcome::Connected(conn))
            } else {
                superseded()
            }
        }
        Err(failure) if failure.kind == RefreshFailureKind::NeedsReauth => {
            let was = conn.status;
            conn.status = ConnectionStatus::NeedsReconnect;
            conn.last_check_at = Some(now);
            conn.last_error = Some(failure.reason.clone());
            conn.updated_at = now;
            if !apply(None, None, Some(&conn), &still_current)? {
                return superseded();
            }
            if was != ConnectionStatus::NeedsReconnect {
                store.raise_connection_needs_reauth(owner, id, &def.display_name);
            }
            Ok(CheckOutcome::NeedsReconnect(conn))
        }
        Err(failure) => {
            conn.last_check_at = Some(now);
            conn.last_error = Some(failure.reason.clone());
            conn.updated_at = now;
            if apply(None, None, Some(&conn), &still_current)? {
                Ok(CheckOutcome::Unavailable(failure.reason))
            } else {
                superseded()
            }
        }
    }
}

/// Named one-shot signals a handler fires at a point a test must wait for (for example "the
/// exchange has read the client and is about to wait for the member's lock"). Test builds only:
/// an unarmed signal is a no-op.
#[cfg(test)]
pub(crate) mod test_signal {
    use std::collections::HashMap;
    use std::sync::{mpsc, Mutex, OnceLock};

    fn slots() -> &'static Mutex<HashMap<String, mpsc::Sender<()>>> {
        static SLOTS: OnceLock<Mutex<HashMap<String, mpsc::Sender<()>>>> = OnceLock::new();
        SLOTS.get_or_init(|| Mutex::new(HashMap::new()))
    }

    /// Arms `name`; the receiver gets one message when a handler fires it.
    pub(crate) fn arm(name: &str) -> mpsc::Receiver<()> {
        let (tx, rx) = mpsc::channel();
        slots()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(name.to_string(), tx);
        rx
    }

    type Pause = (mpsc::Sender<()>, tokio::sync::oneshot::Receiver<()>);

    fn pauses() -> &'static Mutex<HashMap<String, Pause>> {
        static PAUSES: OnceLock<Mutex<HashMap<String, Pause>>> = OnceLock::new();
        PAUSES.get_or_init(|| Mutex::new(HashMap::new()))
    }

    /// Arms a pause at `name`: the first receiver gets a message when a handler reaches it, and
    /// the handler waits there until the sender is used (or dropped).
    pub(crate) fn arm_pause(name: &str) -> (mpsc::Receiver<()>, tokio::sync::oneshot::Sender<()>) {
        let (arrived_tx, arrived_rx) = mpsc::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        pauses()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(name.to_string(), (arrived_tx, release_rx));
        (arrived_rx, release_tx)
    }

    /// Waits at `name` if a test armed a pause there; otherwise returns at once.
    pub(crate) async fn pause(name: &str) {
        let armed = pauses()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(name);
        if let Some((arrived, release)) = armed {
            let _ = arrived.send(());
            let _ = release.await;
        }
    }

    /// Fires `name` once, if a test armed it.
    pub(crate) fn fire(name: &str) {
        let tx = slots()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(name);
        if let Some(tx) = tx {
            let _ = tx.send(());
        }
    }
}

/// Serialises writes to one user's connection to one connector (a check against a disconnect
/// must not bring the rows back).
pub(crate) async fn lock_connection(owner: &str, id: &str) -> tokio::sync::OwnedMutexGuard<()> {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex, Weak};
    static LOCKS: OnceLock<Mutex<HashMap<String, Weak<tokio::sync::Mutex<()>>>>> = OnceLock::new();
    let gate = {
        let mut locks = LOCKS
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if locks.len() >= 64 {
            locks.retain(|_, g| g.strong_count() > 0);
        }
        let key = format!("{owner}\u{0}{id}");
        match locks.get(&key).and_then(Weak::upgrade) {
            Some(g) => g,
            None => {
                let g = Arc::new(tokio::sync::Mutex::new(()));
                locks.insert(key, Arc::downgrade(&g));
                g
            }
        }
    };
    gate.lock_owned().await
}

// ---- storage -----------------------------------------------------------------------------------

pub(crate) fn initialize_schema(connection: &Connection) -> Result<(), OrchestratorError> {
    connection.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS node_enabled_connectors (
            connector_id TEXT PRIMARY KEY,
            enabled INTEGER NOT NULL,
            consent_tenant_id TEXT,
            consent_recorded_at INTEGER,
            consent_recorded_by TEXT,
            updated_at INTEGER NOT NULL,
            updated_by TEXT NOT NULL
        );
        -- One user's connection to a REST connector (CN-1.4). Account facts only: the tokens are
        -- `connection`-kind vault rows `connector/<id>/oauth/*`, never a column here.
        CREATE TABLE IF NOT EXISTS connector_connections (
            owner_user_id TEXT NOT NULL,
            connector_id TEXT NOT NULL,
            status TEXT NOT NULL,
            label TEXT,
            tenant_id TEXT NOT NULL,
            token_endpoint TEXT NOT NULL,
            scopes_json TEXT NOT NULL,
            expires_at INTEGER,
            connected_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL,
            last_check_at INTEGER,
            last_error TEXT,
            PRIMARY KEY (owner_user_id, connector_id)
        );
        ",
    )?;
    // A store from before the company address: every row reads NULL, no address.
    let has_tenant: i64 = connection.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('node_enabled_connectors')
         WHERE name = 'tenant_subdomain'",
        [],
        |r| r.get(0),
    )?;
    if has_tenant == 0 {
        connection.execute(
            "ALTER TABLE node_enabled_connectors ADD COLUMN tenant_subdomain TEXT",
            [],
        )?;
    }
    // The client a grant was issued to (revocation authenticates as it). Older rows read NULL.
    let has_client: i64 = connection.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('connector_connections')
         WHERE name = 'client_id'",
        [],
        |r| r.get(0),
    )?;
    if has_client == 0 {
        connection.execute(
            "ALTER TABLE connector_connections ADD COLUMN client_id TEXT",
            [],
        )?;
    }
    Ok(())
}

/// The node row of `id`, read on the caller's connection or transaction.
pub(crate) fn node_connector_in(
    c: &rusqlite::Connection,
    id: &str,
) -> Result<Option<NodeConnector>, OrchestratorError> {
    Ok(c.query_row(
        &format!("SELECT {COLUMNS} FROM node_enabled_connectors WHERE connector_id = ?1"),
        params![id],
        row_to_node,
    )
    .optional()?)
}

/// Every node row, read on the caller's connection or transaction.
pub(crate) fn node_connectors_in(
    c: &rusqlite::Connection,
) -> Result<Vec<NodeConnector>, OrchestratorError> {
    let mut stmt = c.prepare(&format!(
        "SELECT {COLUMNS} FROM node_enabled_connectors ORDER BY connector_id"
    ))?;
    let rows = stmt
        .query_map([], row_to_node)?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

fn row_to_node(r: &rusqlite::Row<'_>) -> rusqlite::Result<NodeConnector> {
    Ok(NodeConnector {
        connector_id: r.get(0)?,
        enabled: r.get::<_, i64>(1)? != 0,
        consent_tenant_id: r.get(2)?,
        consent_recorded_at: r.get(3)?,
        consent_recorded_by: r.get(4)?,
        updated_at: r.get(5)?,
        updated_by: r.get(6)?,
        tenant_subdomain: r.get(7)?,
    })
}

const COLUMNS: &str = "connector_id, enabled, consent_tenant_id, consent_recorded_at, \
                       consent_recorded_by, updated_at, updated_by, tenant_subdomain";

const CONN_COLUMNS: &str = "owner_user_id, connector_id, status, label, tenant_id, token_endpoint, \
                            scopes_json, expires_at, connected_at, updated_at, last_check_at, last_error, \
                            client_id";

fn row_to_connection(r: &rusqlite::Row<'_>) -> rusqlite::Result<ConnectorConnection> {
    let scopes: String = r.get(6)?;
    Ok(ConnectorConnection {
        owner_user_id: r.get(0)?,
        connector_id: r.get(1)?,
        status: ConnectionStatus::parse(&r.get::<_, String>(2)?),
        label: r.get(3)?,
        tenant_id: r.get(4)?,
        token_endpoint: r.get(5)?,
        scopes: serde_json::from_str(&scopes).unwrap_or_default(),
        expires_at: r.get(7)?,
        connected_at: r.get(8)?,
        updated_at: r.get(9)?,
        last_check_at: r.get(10)?,
        last_error: r.get(11)?,
        client_id: r.get(12)?,
    })
}

/// One user's connection row, read on the caller's connection or transaction.
pub(crate) fn connector_connection_in(
    c: &Connection,
    owner: &str,
    id: &str,
) -> Result<Option<ConnectorConnection>, OrchestratorError> {
    Ok(c.query_row(
        &format!(
            "SELECT {CONN_COLUMNS} FROM connector_connections
             WHERE owner_user_id = ?1 AND connector_id = ?2"
        ),
        params![owner, id],
        row_to_connection,
    )
    .optional()?)
}

/// Upserts a connection row on the caller's connection or transaction.
pub(crate) fn put_connector_connection_in(
    c: &Connection,
    row: &ConnectorConnection,
) -> Result<(), OrchestratorError> {
    let scopes =
        serde_json::to_string(&row.scopes).map_err(|e| OrchestratorError::Core(e.to_string()))?;
    c.execute(
        "INSERT INTO connector_connections (owner_user_id, connector_id, status, label,
             tenant_id, token_endpoint, scopes_json, expires_at, connected_at, updated_at,
             last_check_at, last_error, client_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
         ON CONFLICT(owner_user_id, connector_id) DO UPDATE SET status = excluded.status,
             label = excluded.label, tenant_id = excluded.tenant_id,
             token_endpoint = excluded.token_endpoint, scopes_json = excluded.scopes_json,
             expires_at = excluded.expires_at, connected_at = excluded.connected_at,
             updated_at = excluded.updated_at, last_check_at = excluded.last_check_at,
             last_error = excluded.last_error, client_id = excluded.client_id",
        params![
            row.owner_user_id,
            row.connector_id,
            row.status.as_str(),
            row.label,
            row.tenant_id,
            row.token_endpoint,
            scopes,
            row.expires_at,
            row.connected_at,
            row.updated_at,
            row.last_check_at,
            row.last_error,
            row.client_id
        ],
    )?;
    Ok(())
}

pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

impl SqliteWorkflowStore {
    pub fn node_connector(&self, id: &str) -> Result<Option<NodeConnector>, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        node_connector_in(&connection, id)
    }

    pub fn node_connectors(&self) -> Result<Vec<NodeConnector>, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        node_connectors_in(&connection)
    }

    /// Whether the node has this connector enabled (the gate other waves read).
    pub fn connector_enabled(&self, id: &str) -> Result<bool, OrchestratorError> {
        Ok(self.node_connector(id)?.map(|n| n.enabled).unwrap_or(false))
    }

    /// One user's connection to a REST connector, if they have one.
    pub fn connector_connection(
        &self,
        owner: &str,
        id: &str,
    ) -> Result<Option<ConnectorConnection>, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        connector_connection_in(&connection, owner, id)
    }

    pub fn put_connector_connection(
        &self,
        row: &ConnectorConnection,
    ) -> Result<(), OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        put_connector_connection_in(&connection, row)
    }

    /// Whether a row was there to delete.
    pub fn delete_connector_connection(
        &self,
        owner: &str,
        id: &str,
    ) -> Result<bool, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        Ok(connection.execute(
            "DELETE FROM connector_connections WHERE owner_user_id = ?1 AND connector_id = ?2",
            params![owner, id],
        )? > 0)
    }

    pub fn put_node_connector(&self, row: &NodeConnector) -> Result<(), OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        put_node_connector_in(&connection, row)
    }
}

// ---- the node's code exchange for a REST connector (CN-6.3) ------------------------------------

/// What one member's code exchange commits for an `oauth` connector: both tokens and the
/// connection row, in one transaction.
pub(crate) struct RestCommit<'a> {
    pub(crate) owner: &'a str,
    pub(crate) connector_id: &'a str,
    /// The company address the sign-in was made under (stored as the row's `tenant_id`).
    pub(crate) address: &'a str,
    /// The token endpoint the code was exchanged at (the node client's, pinned at Enable).
    pub(crate) token_endpoint: &'a str,
    /// The client id the code was exchanged under (the grant's issuing client).
    pub(crate) client_id: &'a str,
    /// The host the refresh token is pinned to: the token endpoint's.
    pub(crate) token_host: &'a str,
    /// Every filled binding host: the access token is pinned to all of them.
    pub(crate) access_hosts: &'a [String],
    pub(crate) scopes: &'a [String],
    pub(crate) access_token: &'a str,
    pub(crate) refresh_token: &'a str,
    pub(crate) expires_at: Option<i64>,
    pub(crate) now: i64,
}

impl SqliteWorkflowStore {
    /// Commits a member's sign-in at an `oauth` connector: the refresh token (pinned to the token
    /// host), the access token (pinned to every filled binding host) and the `connector_connections`
    /// row (`connected`, the address as `tenant_id`, no label), in ONE transaction. `check` runs first,
    /// inside the transaction, and refuses (nothing written) if the client, the enable, the address
    /// or the connector's hosts are no longer what the code was exchanged for.
    pub(crate) fn commit_rest_connection(
        &self,
        vault: &SecretsVault,
        c: &RestCommit<'_>,
        check: &dyn Fn(&Connection) -> Result<(), OrchestratorError>,
    ) -> Result<ConnectorConnection, OrchestratorError> {
        validate_oauth_secret_value(c.access_token)?;
        validate_oauth_secret_value(c.refresh_token)?;
        let access = vault.seal(c.access_token)?;
        let refresh = vault.seal(c.refresh_token)?;
        let scopes =
            serde_json::to_string(c.scopes).map_err(|e| OrchestratorError::Core(e.to_string()))?;
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let tx = connection.transaction()?;
        check(&tx)?;
        // The refresh token goes first: an access token never exists without the means to renew it.
        for (binding, sealed, hosts) in [
            (
                refresh_token_binding_id(c.connector_id),
                &refresh,
                std::slice::from_ref(&c.token_host.to_string()).to_vec(),
            ),
            (
                access_token_binding_id(c.connector_id),
                &access,
                c.access_hosts.to_vec(),
            ),
        ] {
            crate::secrets::put_host_secret_in(
                &tx,
                &binding,
                SecretKind::Connection,
                &sealed.nonce,
                &sealed.ciphertext,
                &sealed.fingerprint,
                &hosts,
                None,
                SecretScope::User,
                c.owner,
            )?;
        }
        tx.execute(
            "INSERT INTO connector_connections (owner_user_id, connector_id, status, label,
                 tenant_id, token_endpoint, scopes_json, expires_at, connected_at, updated_at,
                 last_check_at, last_error, client_id)
             VALUES (?1, ?2, 'connected', NULL, ?3, ?4, ?5, ?6, ?7, ?7, NULL, NULL, ?8)
             ON CONFLICT(owner_user_id, connector_id) DO UPDATE SET status = 'connected',
                 label = NULL, tenant_id = excluded.tenant_id,
                 token_endpoint = excluded.token_endpoint, scopes_json = excluded.scopes_json,
                 expires_at = excluded.expires_at, connected_at = excluded.connected_at,
                 updated_at = excluded.updated_at, last_check_at = NULL, last_error = NULL,
                 client_id = excluded.client_id",
            params![
                c.owner,
                c.connector_id,
                c.address,
                c.token_endpoint,
                scopes,
                c.expires_at,
                c.now,
                c.client_id
            ],
        )?;
        let row = connector_connection_in(&tx, c.owner, c.connector_id)?.ok_or_else(|| {
            OrchestratorError::CorruptState("connection row missing after its write".into())
        })?;
        tx.commit()?;
        Ok(row)
    }
}

/// What one write at the end of a refresh stores: a rotated refresh token, the access token with
/// the connection row, or the row alone (a refresh that failed).
pub(crate) struct RefreshCommit<'a> {
    pub(crate) owner: &'a str,
    pub(crate) connector_id: &'a str,
    /// The host the refresh token is pinned to.
    pub(crate) token_host: &'a str,
    /// Every host the access token is pinned to.
    pub(crate) access_hosts: &'a [String],
    /// A rotated refresh token the provider sent: stored alone and first, so the credential that is
    /// live at the provider is kept whatever happens to the writes after it.
    pub(crate) rotated_refresh: Option<&'a str>,
    /// The new access token: stored in the same transaction as `row`, so the token and its expiry
    /// are never apart.
    pub(crate) access_token: Option<&'a str>,
    pub(crate) row: Option<&'a ConnectorConnection>,
}

impl SqliteWorkflowStore {
    /// One transaction of the end of a refresh: `check` first (a refusal writes nothing), then the
    /// tokens given (pinned as at the code exchange) and the connection row.
    pub(crate) fn commit_rest_refresh(
        &self,
        vault: &SecretsVault,
        c: &RefreshCommit<'_>,
        check: &dyn Fn(&Connection) -> Result<(), OrchestratorError>,
    ) -> Result<(), OrchestratorError> {
        for value in [c.rotated_refresh, c.access_token].into_iter().flatten() {
            validate_oauth_secret_value(value)?;
        }
        let rotated = c.rotated_refresh.map(|r| vault.seal(r)).transpose()?;
        let access = c.access_token.map(|a| vault.seal(a)).transpose()?;
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let tx = connection.transaction()?;
        check(&tx)?;
        let token_hosts = [c.token_host.to_string()];
        let mut puts: Vec<(String, &crate::secrets::SealedSecret, &[String])> = Vec::new();
        if let Some(r) = &rotated {
            puts.push((
                refresh_token_binding_id(c.connector_id),
                r,
                &token_hosts[..],
            ));
        }
        if let Some(a) = &access {
            puts.push((access_token_binding_id(c.connector_id), a, c.access_hosts));
        }
        for (binding, sealed, hosts) in puts {
            crate::secrets::put_host_secret_in(
                &tx,
                &binding,
                SecretKind::Connection,
                &sealed.nonce,
                &sealed.ciphertext,
                &sealed.fingerprint,
                hosts,
                None,
                SecretScope::User,
                c.owner,
            )?;
        }
        if let Some(row) = c.row {
            put_connector_connection_in(&tx, row)?;
        }
        tx.commit()?;
        Ok(())
    }
}

/// Marks every member's connection to a REST connector `needs_reconnect`, on a connection or
/// transaction the caller holds: the admin replaced or cleared the client, so their grants belong to
/// a client that is gone. No request is sent for them again; a new sign-in brings them back.
pub(crate) fn mark_rest_connections_need_reconnect_in(
    c: &Connection,
    connector_id: &str,
    why: &str,
    now: i64,
) -> Result<usize, OrchestratorError> {
    Ok(c.execute(
        "UPDATE connector_connections
         SET status = 'needs_reconnect', last_error = ?2, updated_at = ?3
         WHERE connector_id = ?1 AND status <> 'needs_reconnect'",
        params![connector_id, why, now],
    )?)
}

/// What revoking an `oauth` connection's refresh token needs, read before its rows are deleted.
pub(crate) struct RestRevoke {
    pub(crate) revocation_endpoint: String,
    pub(crate) client_id: String,
    pub(crate) refresh_token: SecretString,
}

/// What a disconnect does about the provider's side of an `oauth` grant.
pub(crate) enum RestRevokePlan {
    /// Nothing to revoke: the member has no connection, or the file declares no revocation endpoint.
    Nothing,
    /// The file declares a revocation endpoint but the refresh token is not sent: the grant was
    /// issued to another client than the node has now (revocation must authenticate as the client
    /// that issued it, RFC 7009 section 2.1), the issuing client is not known (a row with no
    /// `client_id`) or the admin cleared the client, or its pin or endpoint no longer match. The
    /// rows go; the provider's grant may remain, and the answer says so.
    Skipped,
    /// Send the revocation.
    Send(RestRevoke),
}

/// The revocation of a member's refresh token at an `oauth` connector, only when the connector file
/// declares `endpoints.revoke` (kept by the node only on the token endpoint's host), the grant was
/// made at the node client's token endpoint **by the node client's own client id**, and the refresh
/// token's row is pinned to that host (RFC 7009, public client, no secret). A grant whose issuing
/// client is not the node's now is [`RestRevokePlan::Skipped`]: no credentials are sent. So is a
/// grant whose issuing client is unknown (`client_id` NULL) or whose client the admin cleared, when
/// the connector declares a revocation endpoint: the grant exists and nothing can revoke it.
/// Nothing is sent here.
pub(crate) fn prepare_rest_revoke(
    store: &SqliteWorkflowStore,
    vault: &SecretsVault,
    owner: &str,
    id: &str,
) -> RestRevokePlan {
    let Some(conn) = store.connector_connection(owner, id).ok().flatten() else {
        return RestRevokePlan::Nothing;
    };
    let Some(client) = store.node_client(id).ok().flatten() else {
        // The admin cleared the client: the member's grant remains at a provider that could
        // revoke it, but no client is left to authenticate as.
        return if declares_revocation(store, id) {
            RestRevokePlan::Skipped
        } else {
            RestRevokePlan::Nothing
        };
    };
    let Some(revocation_endpoint) = client.revocation_endpoint.clone() else {
        return RestRevokePlan::Nothing;
    };
    let Some(issued_to) = conn.client_id.clone() else {
        // Which client issued the grant is not known: it is not revoked as the node's client.
        return RestRevokePlan::Skipped;
    };
    if conn.token_endpoint != client.token_endpoint || issued_to != client.client_id {
        return RestRevokePlan::Skipped;
    }
    let Some(token_host) = client.token_host() else {
        return RestRevokePlan::Skipped;
    };
    let binding = refresh_token_binding_id(id);
    let Ok(refresh_token) = vault.decrypt_secret_at_checked(
        SecretScope::User,
        owner,
        &binding,
        |meta: &crate::secrets::HostSecretMetadata| {
            if meta.allowed_destinations.len() == 1
                && meta.allowed_destinations[0].eq_ignore_ascii_case(&token_host)
            {
                Ok(())
            } else {
                Err(OrchestratorError::PermissionDenied(
                    "the refresh token is not pinned to the token endpoint's host".into(),
                ))
            }
        },
    ) else {
        return RestRevokePlan::Skipped;
    };
    RestRevokePlan::Send(RestRevoke {
        revocation_endpoint,
        client_id: issued_to,
        refresh_token,
    })
}

/// Whether the connector's file, as published, declares a revocation endpoint.
fn declares_revocation(store: &SqliteWorkflowStore, id: &str) -> bool {
    store
        .connector_entry(id)
        .ok()
        .flatten()
        .is_some_and(|e| match &e.template.provider {
            Provider::Oauth2(o) => o.endpoints.as_ref().is_some_and(|e| e.revoke.is_some()),
            _ => false,
        })
}

/// Upserts the node row on a connection the caller already holds (inside a transaction, or not).
/// Whether the node has this connector enabled, read on a connection the caller already holds.
pub(crate) fn node_enabled_in(c: &Connection, id: &str) -> Result<bool, OrchestratorError> {
    Ok(c.query_row(
        "SELECT enabled FROM node_enabled_connectors WHERE connector_id = ?1",
        params![id],
        |r| r.get::<_, i64>(0),
    )
    .optional()?
    .is_some_and(|enabled| enabled != 0))
}

pub(crate) fn put_node_connector_in(
    connection: &Connection,
    row: &NodeConnector,
) -> Result<(), OrchestratorError> {
    connection.execute(
        "INSERT INTO node_enabled_connectors (connector_id, enabled, consent_tenant_id,
             consent_recorded_at, consent_recorded_by, updated_at, updated_by, tenant_subdomain)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT(connector_id) DO UPDATE SET enabled = excluded.enabled,
             consent_tenant_id = excluded.consent_tenant_id,
             consent_recorded_at = excluded.consent_recorded_at,
             consent_recorded_by = excluded.consent_recorded_by,
             updated_at = excluded.updated_at, updated_by = excluded.updated_by,
             tenant_subdomain = excluded.tenant_subdomain",
        params![
            row.connector_id,
            row.enabled as i64,
            row.consent_tenant_id,
            row.consent_recorded_at,
            row.consent_recorded_by,
            row.updated_at,
            row.updated_by,
            row.tenant_subdomain
        ],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mcp_policy(id: &str) -> Option<McpPolicy> {
        builtin(id).and_then(|d| policy_of(id, &d))
    }
    use vam_spec::connector::PLACEHOLDER_CLIENT_ID;

    const TENANT: &str = "11111111-1111-4111-8111-111111111111";

    fn m365() -> Connector {
        builtin("microsoft-365").unwrap()
    }
    fn linear() -> Connector {
        builtin("linear").unwrap()
    }
    fn placeholder_cfg() -> ConnectorConfig {
        ConnectorConfig {
            client_ids: [(
                "microsoft-365".to_string(),
                PLACEHOLDER_CLIENT_ID.to_string(),
            )]
            .into(),
        }
    }
    fn cfg_with_client() -> ConnectorConfig {
        ConnectorConfig {
            client_ids: [("microsoft-365".to_string(), "e2e-client".to_string())].into(),
        }
    }
    fn put(enabled: bool, tenant: Option<&str>) -> PutNodeRequest {
        PutNodeRequest {
            tenant: None,
            enabled,
            consent: tenant.map(|t| {
                Some(ConsentRequest {
                    tenant_id: t.to_string(),
                })
            }),
            client: None,
        }
    }

    #[test]
    fn the_listing_says_where_a_connector_came_from_and_who_vouches_for_it() {
        use crate::marketplace::store::Trust;
        let cfg = placeholder_cfg();
        let builtin_entry = ConnectorEntry {
            template: linear(),
            def: linear(),
            installed: None,
        };
        let mut e = list_entry(&builtin_entry.def, &cfg, None, None, true);
        annotate_source(&mut e, &builtin_entry, true);
        assert_eq!(e["source"], "builtin");
        assert_eq!(e["trust"], "squrell");
        assert!(e.get("origin").is_none() && e.get("capabilities").is_none());

        let mut def = linear();
        def.urn = "urn:vam:connector:market.example.org/acme/bugwatch".into();
        def.publisher = "Acme Tools".into();
        let installed = ConnectorEntry {
            template: def.clone(),
            def,
            installed: Some(InstalledMeta {
                trust: Trust::Unverified,
                marketplace_id: "m-1".into(),
                marketplace_name: "Acme".into(),
                publisher_domain: "market.example.org".into(),
                listing: "bugwatch".into(),
                git_ref: "bugwatch-v1".into(),
                commit: "c".repeat(40),
                package_digest: "d".repeat(64),
            }),
        };
        // An admin sees the capabilities before the connector is enabled; a member does not.
        let mut admin = list_entry(&installed.def, &cfg, None, None, true);
        annotate_source(&mut admin, &installed, true);
        assert_eq!(admin["trust"], "unverified");
        assert_eq!(admin["source"], "marketplace");
        assert_eq!(admin["origin"]["marketplace"], "m-1");
        assert_eq!(admin["origin"]["ref"], "bugwatch-v1");
        assert_eq!(admin["capabilities"]["publisher"], "Acme Tools");
        assert_eq!(
            admin["capabilities"]["mcpUrl"],
            "https://mcp.linear.app/mcp"
        );
        assert_eq!(admin["capabilities"]["scopes"][1], "write");
        assert!(admin.get("provider").is_none(), "{admin}");
        let mut member = list_entry(&installed.def, &cfg, None, None, false);
        annotate_source(&mut member, &installed, false);
        assert_eq!(member["trust"], "unverified");
        assert!(member.get("capabilities").is_none());
        // Once enabled the member sees them too.
        let on = NodeConnector {
            tenant_subdomain: None,
            connector_id: "bugwatch".into(),
            enabled: true,
            consent_tenant_id: None,
            consent_recorded_at: None,
            consent_recorded_by: None,
            updated_at: 1,
            updated_by: "olga".into(),
        };
        let mut member = list_entry(&installed.def, &cfg, Some(&on), None, false);
        annotate_source(&mut member, &installed, false);
        assert_eq!(member["capabilities"]["publisher"], "Acme Tools");
    }

    #[test]
    fn ids_come_from_the_urn() {
        assert_eq!(connector_id(&m365()), "microsoft-365");
        assert_eq!(connector_id(&linear()), "linear");
        assert!(builtin("nope").is_none());
    }

    #[test]
    fn tenant_must_be_a_guid() {
        assert_eq!(
            parse_tenant_guid(&TENANT.to_uppercase()).as_deref(),
            Some(TENANT)
        );
        for bad in [
            "organizations",
            "common",
            "contoso.com",
            "not-a-guid",
            "",
            TENANT.trim_end_matches('1'),
        ] {
            assert!(parse_tenant_guid(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn placeholder_client_id_is_not_configured_and_enable_is_409() {
        // The shipped definition carries Squrell's real client id; a node set to the placeholder
        // is the unconfigured case.
        let none = placeholder_cfg();
        assert!(!is_configured(&m365(), &none));
        assert!(is_configured(&linear(), &none));
        let err = plan_put(&m365(), &none, None, &put(true, None), 1, "admin").unwrap_err();
        assert!(matches!(err, OrchestratorError::Conflict(_)), "{err:?}");
        // The sentinel set explicitly is still not configured.
        let sentinel = ConnectorConfig {
            client_ids: [("microsoft-365".into(), PLACEHOLDER_CLIENT_ID.into())].into(),
        };
        assert!(plan_put(&m365(), &sentinel, None, &put(true, None), 1, "a").is_err());
        // Disabling an unconfigured connector is fine.
        assert!(plan_put(&m365(), &none, None, &put(false, None), 1, "a").is_ok());
        assert!(plan_put(&m365(), &cfg_with_client(), None, &put(true, None), 1, "a").is_ok());
    }

    #[test]
    fn consent_rules() {
        let cfg = cfg_with_client();
        for bad in ["organizations", "not-a-guid", "common"] {
            let e = plan_put(&m365(), &cfg, None, &put(true, Some(bad)), 1, "a").unwrap_err();
            assert!(matches!(e, OrchestratorError::InvalidContract(_)), "{bad}");
        }
        let e = plan_put(&linear(), &cfg, None, &put(true, Some(TENANT)), 1, "a").unwrap_err();
        assert!(matches!(e, OrchestratorError::InvalidContract(_)));
        let row = plan_put(&m365(), &cfg, None, &put(true, Some(TENANT)), 5, "admin").unwrap();
        assert_eq!(row.consent_tenant_id.as_deref(), Some(TENANT));
        assert_eq!(row.consent_recorded_by.as_deref(), Some("admin"));
        assert_eq!(row.consent_recorded_at, Some(5));
    }

    #[test]
    fn disable_keeps_consent_and_reenable_restores_it() {
        let cfg = cfg_with_client();
        let on = plan_put(&m365(), &cfg, None, &put(true, Some(TENANT)), 1, "a").unwrap();
        let off = plan_put(&m365(), &cfg, Some(&on), &put(false, None), 2, "a").unwrap();
        assert!(!off.enabled);
        assert_eq!(off.consent_tenant_id.as_deref(), Some(TENANT));
        let again = plan_put(&m365(), &cfg, Some(&off), &put(true, None), 3, "a").unwrap();
        assert!(again.enabled);
        assert_eq!(again.consent_recorded_at, Some(1));
        assert_eq!(
            caller_state(&m365(), Some(&again), None),
            CallerState::Connect
        );
    }

    #[test]
    fn null_consent_clears_and_omitted_keeps() {
        let cfg = cfg_with_client();
        let on = plan_put(&m365(), &cfg, None, &put(true, Some(TENANT)), 1, "a").unwrap();
        let body: PutNodeRequest =
            serde_json::from_str(r#"{"enabled":false,"consent":null}"#).unwrap();
        assert!(matches!(body.consent, Some(None)));
        let off = plan_put(&m365(), &cfg, Some(&on), &body, 2, "a").unwrap();
        assert!(
            !off.enabled && off.consent_tenant_id.is_none() && off.consent_recorded_at.is_none()
        );
        let keep: PutNodeRequest = serde_json::from_str(r#"{"enabled":false}"#).unwrap();
        assert!(keep.consent.is_none());
        assert!(plan_put(&m365(), &cfg, Some(&on), &keep, 2, "a")
            .unwrap()
            .consent_tenant_id
            .is_some());
        // Clearing is allowed on a connector that takes no consent, and needs no client id.
        assert!(plan_put(&linear(), &cfg, None, &body, 2, "a").is_ok());
        assert!(plan_put(&m365(), &ConnectorConfig::default(), None, &body, 2, "a").is_ok());
    }

    #[test]
    fn caller_states() {
        let cfg = cfg_with_client();
        assert_eq!(caller_state(&linear(), None, None), CallerState::AskAdmin);
        let on = plan_put(&linear(), &cfg, None, &put(true, None), 1, "a").unwrap();
        assert_eq!(
            caller_state(&linear(), Some(&on), None),
            CallerState::Connect
        );
        assert_eq!(
            caller_state(&linear(), Some(&on), Some(CallerState::Connected)),
            CallerState::Connected
        );
        let m = plan_put(&m365(), &cfg, None, &put(true, None), 1, "a").unwrap();
        assert_eq!(
            caller_state(&m365(), Some(&m), Some(CallerState::Connected)),
            CallerState::WaitingForConsent
        );
        let off = plan_put(&linear(), &cfg, Some(&on), &put(false, None), 2, "a").unwrap();
        assert_eq!(
            caller_state(&linear(), Some(&off), None),
            CallerState::AskAdmin
        );
    }

    #[test]
    fn listing_hides_the_definition_until_enabled_and_never_has_a_secret() {
        let cfg = cfg_with_client();
        let e = list_entry(&m365(), &cfg, None, None, false);
        assert_eq!(e["state"], "askAdmin");
        assert_eq!(e["label"], "Ask your admin");
        for k in ["provider", "mcp", "bindings", "connection", "urn"] {
            assert!(e.get(k).is_none(), "{k} before enable");
        }
        assert_eq!(e["node"]["configured"], true);

        let row = plan_put(&m365(), &cfg, None, &put(true, Some(TENANT)), 9, "admin-1").unwrap();
        let e = list_entry(&m365(), &cfg, Some(&row), None, false);
        assert_eq!(e["state"], "connect");
        assert_eq!(e["provider"]["profile"], "entra");
        assert_eq!(e["provider"]["clientId"], "e2e-client");
        assert!(e["provider"]["scopes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s == "offline_access"));
        assert_eq!(e["node"]["consent"]["tenantId"], TENANT);
        assert!(e["node"]["consent"].get("recordedBy").is_none());
        let text = e.to_string();
        assert!(
            !text.contains("resource") && !text.contains("vault"),
            "{text}"
        );
        let admin = list_entry(&m365(), &cfg, Some(&row), None, true);
        assert_eq!(admin["node"]["consent"]["recordedBy"], "admin-1");
    }

    #[test]
    fn client_id_config_from_env_and_flag() {
        let env = |k: &str| {
            (k == "VAMD_CONNECTOR_MICROSOFT_365_CLIENT_ID").then(|| "from-env".to_string())
        };
        let c = config_from(&env, &[]).unwrap();
        assert_eq!(c.client_ids["microsoft-365"], "from-env");
        let c = config_from(&env, &["microsoft-365=from-flag".into()]).unwrap();
        assert_eq!(c.client_ids["microsoft-365"], "from-flag");
        assert!(config_from(&|_| None, &["nope=x".into()]).is_err());
        assert!(config_from(&|_| None, &["microsoft-365".into()]).is_err());
        assert!(config_from(&|_| None, &["microsoft-365=a b".into()]).is_err());
        assert!(config_from(&|_| None, &["microsoft-365=".into()]).is_err());
    }

    #[test]
    fn store_round_trip() {
        let store = SqliteWorkflowStore::in_memory().unwrap();
        assert!(store.node_connector("linear").unwrap().is_none());
        assert!(!store.connector_enabled("linear").unwrap());
        let row = plan_put(
            &linear(),
            &ConnectorConfig::default(),
            None,
            &put(true, None),
            1,
            "a",
        )
        .unwrap();
        store.put_node_connector(&row).unwrap();
        assert!(store.connector_enabled("linear").unwrap());
        assert_eq!(store.node_connectors().unwrap(), vec![row]);
    }

    // ---- CN-1.3: Linear as an MCP connector ------------------------------------------------

    use crate::mcp_registry::{seed_mcp_skills, McpAuthMode};
    use crate::secrets::SecretScope;

    fn tool(name: &str) -> McpTool {
        McpTool {
            name: name.into(),
            description: Some(format!("{name} tool")),
            input_schema: serde_json::json!({ "type": "object" }),
            ..Default::default()
        }
    }

    fn set_enabled(store: &SqliteWorkflowStore, enabled: bool) {
        store
            .put_node_connector(&NodeConnector {
                tenant_subdomain: None,
                connector_id: "linear".into(),
                enabled,
                consent_tenant_id: None,
                consent_recorded_at: None,
                consent_recorded_by: None,
                updated_at: 1,
                updated_by: "olga".into(),
            })
            .unwrap();
    }

    fn connect_linear(store: &SqliteWorkflowStore, user: &str, url: &str) {
        let policy = mcp_policy("linear").unwrap();
        let listed = [
            "list_issues",
            "get_issue",
            "save_issue",
            "save_comment",
            "export_workspace",
        ]
        .map(tool);
        store
            .upsert_mcp_connection(
                SecretScope::User,
                user,
                "linear",
                url,
                "",
                &[],
                &keep_vetted(&policy, listed.to_vec()),
                McpAuthMode::Static,
                None,
            )
            .unwrap();
        store
            .apply_connector_policy(SecretScope::User, user, &policy)
            .unwrap();
    }

    #[test]
    fn the_policy_is_the_definitions_and_vendor_extras_are_dropped() {
        let p = mcp_policy("linear").unwrap();
        assert_eq!(p.url, "https://mcp.linear.app/mcp");
        assert_eq!(p.side_effects["list_issues"], 1);
        assert_eq!(p.side_effects["save_issue"], 3);
        assert_eq!(p.ask_tools, vec!["save_issue", "save_comment"]);
        assert!(mcp_policy("microsoft-365").is_none());
        let kept = keep_vetted(&p, vec![tool("list_issues"), tool("export_workspace")]);
        assert_eq!(kept.len(), 1);
    }

    #[test]
    fn a_connector_id_write_needs_the_enabled_connector_and_its_own_url() {
        let store = SqliteWorkflowStore::in_memory().unwrap();
        let own = "https://mcp.linear.app/mcp";
        // Not enabled: refused with the own URL.
        let err = gate_mcp_write(&store, "linear", Some(own), None).unwrap_err();
        assert!(
            matches!(err, OrchestratorError::PermissionDenied(_)),
            "{err}"
        );
        // Another URL is refused whether or not it is enabled.
        set_enabled(&store, true);
        let err =
            gate_mcp_write(&store, "linear", Some("https://x.example/mcp"), None).unwrap_err();
        assert!(err.to_string().contains("vam connectors"), "{err}");
        // A stored row on another URL cannot be written through either.
        assert!(gate_mcp_write(&store, "linear", None, Some("https://x.example/mcp")).is_err());
        assert!(gate_mcp_write(&store, "linear", Some(own), None)
            .unwrap()
            .is_some());
        // Other ids are not touched.
        assert!(
            gate_mcp_write(&store, "e2e-mcp", Some("https://x.example/mcp"), None)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn a_trigger_started_session_seeds_its_grants_users_linear_tools() {
        // A trigger run is started as the grant's user (gateway.rs `RunCaller.owner_user_id`),
        // so the session owner is that user and `seed_mcp_skills` reads that user's rows.
        let store = SqliteWorkflowStore::in_memory().unwrap();
        set_enabled(&store, true);
        connect_linear(&store, "ada", "https://mcp.linear.app/mcp");
        let dir = tempfile::tempdir().unwrap();
        let ada = dir.path().join("ada");
        let planned = seed_mcp_skills(&ada, &store, "sess-trigger", "ada").unwrap();
        let names: Vec<_> = planned.iter().map(|t| t.guest_name.as_str()).collect();
        assert_eq!(
            names,
            [
                "linear__list_issues",
                "linear__get_issue",
                "linear__save_issue",
                "linear__save_comment"
            ]
        );
        for t in &planned {
            let write = t.vendor_tool.starts_with("save_");
            assert_eq!(t.side_effect, if write { 3 } else { 1 }, "{}", t.guest_name);
            assert_eq!(t.ask, write, "{}", t.guest_name);
        }
        // Another user's session does not get ada's tools.
        let bob = dir.path().join("bob");
        assert!(seed_mcp_skills(&bob, &store, "sess-bob", "bob")
            .unwrap()
            .is_empty());
        assert!(!bob.join("mcp-linear").exists());
    }

    #[test]
    fn disabled_connector_is_not_seeded_and_returns_when_enabled_again() {
        let store = SqliteWorkflowStore::in_memory().unwrap();
        set_enabled(&store, true);
        connect_linear(&store, "ada", "https://mcp.linear.app/mcp");
        set_enabled(&store, false);
        let dir = tempfile::tempdir().unwrap();
        let off = dir.path().join("off");
        assert!(seed_mcp_skills(&off, &store, "s1", "ada")
            .unwrap()
            .is_empty());
        assert!(!off.join("mcp-linear").exists());
        set_enabled(&store, true);
        let on = dir.path().join("on");
        assert_eq!(seed_mcp_skills(&on, &store, "s2", "ada").unwrap().len(), 4);
    }

    #[test]
    fn a_hand_added_row_with_the_connector_id_on_another_url_keeps_working() {
        let store = SqliteWorkflowStore::in_memory().unwrap();
        connect_linear(&store, "ada", "https://mcp.linear.example/mcp");
        let dir = tempfile::tempdir().unwrap();
        // Not the connector's connection, so not gated: it keeps its own tools, unvetted too.
        let planned = seed_mcp_skills(dir.path(), &store, "s1", "ada").unwrap();
        assert!(!planned.is_empty());
    }

    #[test]
    fn an_installed_connectors_row_on_a_stale_url_is_not_seeded() {
        // `filebox` is on `https://mcp.new-home.example/mcp` and enabled; a member's row from before
        // the connector moved names the old server. It must not skip the enable check and the vetted
        // filter by not matching the connector's URL: it is not offered at all.
        use crate::connector_client::test_support::{
            enable_filebox, install_filebox_rows, member_row,
        };
        let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
        let vault = SecretsVault::for_tests(store.clone());
        install_filebox_rows(&store, "new-home");
        enable_filebox(&store, &vault, "new-home", "c-1", "s-1");
        member_row(&store, &vault, "old-home", "ada", "c-old", None);
        let dir = tempfile::tempdir().unwrap();
        let planned = seed_mcp_skills(dir.path(), &store, "s1", "ada").unwrap();
        assert!(planned.is_empty(), "{planned:?}");
        assert!(!dir.path().join("mcp-filebox").exists());
        // The connector's own row, on its current URL, is seeded as before.
        member_row(&store, &vault, "new-home", "bob", "c-1", None);
        let dir = tempfile::tempdir().unwrap();
        let planned = seed_mcp_skills(dir.path(), &store, "s2", "bob").unwrap();
        assert_eq!(planned.len(), 1, "{planned:?}");
    }

    #[test]
    fn caller_state_reads_the_row() {
        let store = SqliteWorkflowStore::in_memory().unwrap();
        let p = mcp_policy("linear").unwrap();
        assert_eq!(mcp_connection_state(&p, None, false), None);
        connect_linear(&store, "ada", "https://mcp.linear.app/mcp");
        let conn = store
            .get_mcp_connection(SecretScope::User, "ada", "linear")
            .unwrap();
        assert_eq!(mcp_connection_state(&p, Some(&conn), false), None);
        assert_eq!(
            mcp_connection_state(&p, Some(&conn), true),
            Some(CallerState::Connected)
        );
        let mut reauth = conn.clone();
        reauth.oauth = Some(crate::mcp_registry::McpOauthMetadata {
            resource: p.url.clone(),
            authorization_server: "https://a".into(),
            token_endpoint: "https://a/t".into(),
            revocation_endpoint: None,
            client_id: "c".into(),
            registration_method: crate::mcp_registry::McpOauthRegistrationMethod::Dcr,
            scopes: vec![],
            expires_at: None,
            status: McpOauthStatus::NeedsReauth,
            last_refresh_at: None,
            last_refresh_error: None,
        });
        assert_eq!(
            mcp_connection_state(&p, Some(&reauth), true),
            Some(CallerState::NeedsReconnect)
        );
        let mut other = conn;
        other.url = "https://x.example/mcp".into();
        assert_eq!(
            mcp_connection_state(&p, Some(&other), true),
            Some(CallerState::NeedsReconnect)
        );
    }

    // ---- CN-1.4: the Microsoft 365 connection ----------------------------------------------

    use secrecy::ExposeSecret;
    use std::sync::Arc;

    const OTHER: &str = "22222222-2222-4222-8222-222222222222";
    const ACCESS: &str = "ACCESS-SECRET-aaaaaaaaaaaaaaaa";
    const REFRESH: &str = "REFRESH-SECRET-bbbbbbbbbbbbbbb";

    fn id_token(tid: &str, upn: &str) -> String {
        use base64::Engine as _;
        let b = |v: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v);
        format!(
            "{}.{}.{}",
            b(br#"{"alg":"none"}"#),
            b(json!({ "tid": tid, "preferred_username": upn })
                .to_string()
                .as_bytes()),
            b(b"sig")
        )
    }

    fn conn_req(tid: &str, upn: &str) -> PutConnectionRequest {
        PutConnectionRequest {
            access_token: ACCESS.into(),
            refresh_token: REFRESH.into(),
            id_token: id_token(tid, upn),
            expires_in: Some(3600),
        }
    }

    fn consented_node(tenant: Option<&str>, enabled: bool) -> NodeConnector {
        NodeConnector {
            tenant_subdomain: None,
            connector_id: "microsoft-365".into(),
            enabled,
            consent_tenant_id: tenant.map(str::to_string),
            consent_recorded_at: tenant.map(|_| 1),
            consent_recorded_by: tenant.map(|_| "admin".to_string()),
            updated_at: 1,
            updated_by: "admin".into(),
        }
    }

    fn rest_world() -> (Arc<SqliteWorkflowStore>, Arc<SecretsVault>) {
        let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
        let vault = SecretsVault::for_tests(store.clone());
        (store, vault)
    }

    #[test]
    fn id_token_claims_are_read_and_bad_tokens_say_nothing_of_themselves() {
        let c = read_id_token(&id_token(TENANT, "ada@contoso.com")).unwrap();
        assert_eq!(c.tid, TENANT);
        assert_eq!(c.label.as_deref(), Some("ada@contoso.com"));
        let no_name = {
            use base64::Engine as _;
            let p = base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(json!({ "tid": TENANT }).to_string());
            format!("x.{p}.y")
        };
        assert_eq!(read_id_token(&no_name).unwrap().label, None);
        for bad in [
            "",
            "a.b",
            "a.b.c.d",
            "a.!!!.c",
            "a.e30.c",
            &"x".repeat(20_000),
        ] {
            let err = read_id_token(bad).unwrap_err().to_string();
            assert!(!err.contains(bad) || bad.is_empty(), "{err}");
        }
    }

    #[test]
    fn a_connection_needs_enablement_consent_and_the_consented_tenant() {
        let def = m365();
        let req = conn_req(TENANT, "ada@contoso.com");
        let refused = |node: Option<&NodeConnector>, req: &PutConnectionRequest| {
            plan_connection(&def, node, req, 10).unwrap_err()
        };
        assert!(matches!(
            refused(None, &req),
            OrchestratorError::PermissionDenied(_)
        ));
        let off = consented_node(Some(TENANT), false);
        assert!(matches!(
            refused(Some(&off), &req),
            OrchestratorError::PermissionDenied(_)
        ));
        let no_consent = consented_node(None, true);
        assert!(matches!(
            refused(Some(&no_consent), &req),
            OrchestratorError::Conflict(_)
        ));

        let on = consented_node(Some(TENANT), true);
        let other = conn_req(OTHER, "someone@other.example");
        let err = refused(Some(&on), &other);
        assert!(matches!(err, OrchestratorError::Conflict(_)), "{err:?}");
        // The case of a GUID does not make it another tenant.
        let upper = conn_req(&TENANT.to_ascii_uppercase(), "ada@contoso.com");
        assert!(plan_connection(&def, Some(&on), &upper, 10).is_ok());
        // `common` and friends are not a tenant.
        for tid in ["common", "organizations", "", "11111111-1111-4111-8111"] {
            let r = conn_req(tid, "x@y.z");
            assert!(
                matches!(refused(Some(&on), &r), OrchestratorError::Conflict(_)),
                "{tid}"
            );
        }

        let plan = plan_connection(&def, Some(&on), &req, 100).unwrap();
        assert_eq!(plan.tenant_id, TENANT);
        assert_eq!(plan.label.as_deref(), Some("ada@contoso.com"));
        assert_eq!(
            plan.token_endpoint,
            format!("https://login.microsoftonline.com/{TENANT}/oauth2/v2.0/token")
        );
        assert_eq!(plan.access_host, "graph.microsoft.com");
        assert_eq!(plan.expires_at, Some(3700));
        assert!(plan.scopes.iter().any(|s| s == "offline_access"));

        // An MCP connector has no REST connection; neither does an empty token.
        let linear_err = plan_connection(&linear(), Some(&on), &req, 1).unwrap_err();
        assert!(matches!(linear_err, OrchestratorError::InvalidContract(_)));
        let mut empty = conn_req(TENANT, "x@y.z");
        empty.access_token.clear();
        assert!(matches!(
            refused(Some(&on), &empty),
            OrchestratorError::InvalidContract(_)
        ));
    }

    #[test]
    fn no_error_or_debug_text_carries_a_token() {
        let def = m365();
        let on = consented_node(Some(TENANT), true);
        let mut errors = vec![
            plan_connection(&def, Some(&on), &conn_req(OTHER, "a@b.c"), 1).unwrap_err(),
            plan_connection(&def, None, &conn_req(TENANT, "a@b.c"), 1).unwrap_err(),
        ];
        let mut bad_id = conn_req(TENANT, "a@b.c");
        bad_id.id_token = format!("{ACCESS}.{REFRESH}");
        errors.push(plan_connection(&def, Some(&on), &bad_id, 1).unwrap_err());
        for e in errors {
            let t = e.to_string();
            assert!(!t.contains(ACCESS) && !t.contains(REFRESH), "{t}");
        }
        let dbg = format!("{:?}", conn_req(TENANT, "a@b.c"));
        assert!(!dbg.contains(ACCESS) && !dbg.contains(REFRESH), "{dbg}");
    }

    fn connect(
        store: &SqliteWorkflowStore,
        vault: &SecretsVault,
        owner: &str,
    ) -> ConnectorConnection {
        let on = consented_node(Some(TENANT), true);
        store.put_node_connector(&on).unwrap();
        let req = conn_req(TENANT, "ada@contoso.com");
        let plan = plan_connection(&m365(), Some(&on), &req, 100).unwrap();
        write_connection(
            store,
            vault,
            owner,
            "microsoft-365",
            &plan,
            &req,
            Some("k1"),
            100,
        )
        .unwrap()
    }

    #[test]
    fn tokens_are_connection_kind_rows_and_the_table_holds_none() {
        let (store, vault) = rest_world();
        let row = connect(&store, &vault, "ada");
        assert_eq!(row.status, ConnectionStatus::Connected);
        assert_eq!(row.label.as_deref(), Some("ada@contoso.com"));
        for (binding, value, host) in [
            (
                refresh_token_binding_id("microsoft-365"),
                REFRESH,
                "login.microsoftonline.com",
            ),
            (
                access_token_binding_id("microsoft-365"),
                ACCESS,
                "graph.microsoft.com",
            ),
        ] {
            assert!(binding.starts_with("connector/microsoft-365/oauth/"));
            let meta = vault
                .get_metadata_at(SecretScope::User, "ada", &binding)
                .unwrap();
            assert_eq!(meta.kind, SecretKind::Connection);
            assert_eq!(meta.allowed_destinations, vec![host.to_string()]);
            assert_eq!(
                vault
                    .decrypt_secret_at(SecretScope::User, "ada", &binding)
                    .unwrap()
                    .expose_secret(),
                value
            );
            // Not the org partition, and not another user's.
            assert!(!vault
                .has_binding_at(SecretScope::Org, "", &binding)
                .unwrap());
            assert!(!vault
                .has_binding_at(SecretScope::User, "bob", &binding)
                .unwrap());
            // Never handed to a broker or a tool, like an MCP credential (DX-139).
            assert!(vault
                .decrypt_secret_for_broker("ada", &binding, &["graph.microsoft.com".to_string()])
                .is_err());
        }
        // The id token is not stored anywhere in the table row.
        let dump = format!("{row:?}");
        assert!(!dump.contains(ACCESS) && !dump.contains(REFRESH) && !dump.contains("eyJ"));
        assert_eq!(
            store.connector_connection("ada", "microsoft-365").unwrap(),
            Some(row)
        );
        assert_eq!(
            store.connector_connection("bob", "microsoft-365").unwrap(),
            None
        );
    }

    fn ok_refresh(
        target: &RestRefreshTarget<'_>,
        rt: &SecretString,
    ) -> Result<RefreshedToken, RefreshFailure> {
        let RestRefreshTarget::Entra {
            tenant,
            client_id,
            scope,
        } = *target
        else {
            panic!("an Entra connection is refreshed with an Entra target");
        };
        assert_eq!(tenant, TENANT);
        assert_eq!(client_id, "e2e-client");
        assert!(scope.split(' ').any(|s| s == "offline_access"));
        assert_eq!(rt.expose_secret(), REFRESH);
        Ok(RefreshedToken {
            access_token: "NEW-ACCESS-cccccccccccccccc".into(),
            expires_in: Some(1800),
            refresh_token: Some("NEW-REFRESH-dddddddddddddd".into()),
        })
    }
    fn revoked(
        _: &RestRefreshTarget<'_>,
        _: &SecretString,
    ) -> Result<RefreshedToken, RefreshFailure> {
        Err(RefreshFailure::needs_reauth("invalid_grant"))
    }
    fn unreachable_ms(
        _: &RestRefreshTarget<'_>,
        _: &SecretString,
    ) -> Result<RefreshedToken, RefreshFailure> {
        Err(RefreshFailure::transient(
            "OAuth token endpoint request failed",
        ))
    }
    fn must_not_be_called(
        _: &RestRefreshTarget<'_>,
        _: &SecretString,
    ) -> Result<RefreshedToken, RefreshFailure> {
        panic!("a refresh was sent");
    }

    #[test]
    fn a_check_refreshes_once_and_rotates_the_tokens() {
        let (store, vault) = rest_world();
        connect(&store, &vault, "ada");
        let cfg = cfg_with_client();
        let out = check_connection(
            &store,
            &vault,
            &cfg,
            "ada",
            "microsoft-365",
            200,
            ok_refresh,
        )
        .unwrap();
        let CheckOutcome::Connected(row) = out else {
            panic!("{out:?}")
        };
        assert_eq!(row.last_check_at, Some(200));
        assert_eq!(row.expires_at, Some(2000));
        let read = |b: String| {
            vault
                .decrypt_secret_at(SecretScope::User, "ada", &b)
                .unwrap()
                .expose_secret()
                .clone()
        };
        assert_eq!(
            read(refresh_token_binding_id("microsoft-365")),
            "NEW-REFRESH-dddddddddddddd"
        );
        assert_eq!(
            read(access_token_binding_id("microsoft-365")),
            "NEW-ACCESS-cccccccccccccccc"
        );
        let meta = vault
            .get_metadata_at(
                SecretScope::User,
                "ada",
                &access_token_binding_id("microsoft-365"),
            )
            .unwrap();
        assert_eq!(meta.kind, SecretKind::Connection);
    }

    #[test]
    fn an_entra_rotated_refresh_token_survives_a_disable_but_not_a_consent_change() {
        let cfg = cfg_with_client();
        for meanwhile in [
            "disabled",
            "consent withdrawn",
            "consent for another tenant",
        ] {
            let (store, vault) = rest_world();
            connect(&store, &vault, "ada");
            let out = check_connection(
                &store,
                &vault,
                &cfg,
                "ada",
                "microsoft-365",
                200,
                |t, rt| {
                    let mut n = store.node_connector("microsoft-365").unwrap().unwrap();
                    match meanwhile {
                        "disabled" => n.enabled = false,
                        "consent withdrawn" => n.consent_tenant_id = None,
                        _ => n.consent_tenant_id = Some("another-tenant".into()),
                    }
                    store.put_node_connector(&n).unwrap();
                    ok_refresh(t, rt)
                },
            )
            .unwrap();
            // The access token and the Connected result are not published under a moved state.
            assert!(
                matches!(out, CheckOutcome::Unavailable(_)),
                "{meanwhile}: {out:?}"
            );
            let read = |b: String| {
                vault
                    .decrypt_secret_at(SecretScope::User, "ada", &b)
                    .unwrap()
                    .expose_secret()
                    .clone()
            };
            assert_ne!(
                read(access_token_binding_id("microsoft-365")),
                "NEW-ACCESS-cccccccccccccccc",
                "{meanwhile}"
            );
            // The provider spent the old refresh token: under the same consent the rotated one is
            // kept, enabled or not; a withdrawn or changed consent discards it.
            let want = if meanwhile == "disabled" {
                "NEW-REFRESH-dddddddddddddd"
            } else {
                REFRESH
            };
            assert_eq!(
                read(refresh_token_binding_id("microsoft-365")),
                want,
                "{meanwhile}"
            );
        }
    }

    #[test]
    fn a_refused_grant_is_needs_reconnect_and_an_unreachable_one_changes_nothing() {
        let (store, vault) = rest_world();
        connect(&store, &vault, "ada");
        let cfg = cfg_with_client();
        let out = check_connection(
            &store,
            &vault,
            &cfg,
            "ada",
            "microsoft-365",
            300,
            unreachable_ms,
        )
        .unwrap();
        assert!(matches!(out, CheckOutcome::Unavailable(_)), "{out:?}");
        let row = store
            .connector_connection("ada", "microsoft-365")
            .unwrap()
            .unwrap();
        assert_eq!(row.status, ConnectionStatus::Connected);
        assert_eq!(
            row.last_error.as_deref(),
            Some("OAuth token endpoint request failed")
        );

        let out =
            check_connection(&store, &vault, &cfg, "ada", "microsoft-365", 400, revoked).unwrap();
        let CheckOutcome::NeedsReconnect(row) = out else {
            panic!("{out:?}")
        };
        assert_eq!(row.status, ConnectionStatus::NeedsReconnect);
        assert_eq!(row.last_error.as_deref(), Some("invalid_grant"));
        // The tokens stay until a disconnect or a reconnect replaces them.
        assert!(vault
            .has_binding_at(
                SecretScope::User,
                "ada",
                &refresh_token_binding_id("microsoft-365")
            )
            .unwrap());
        let node = store.node_connector("microsoft-365").unwrap();
        assert_eq!(
            rest_connection_state(&row, node.as_ref(), true, None),
            CallerState::NeedsReconnect
        );
        // A later good check brings it back.
        let out = check_connection(
            &store,
            &vault,
            &cfg,
            "ada",
            "microsoft-365",
            500,
            ok_refresh,
        )
        .unwrap();
        assert!(matches!(out, CheckOutcome::Connected(_)));
    }

    #[test]
    fn a_check_never_sends_a_refresh_token_outside_the_consented_tenant() {
        let (store, vault) = rest_world();
        connect(&store, &vault, "ada");
        let cfg = cfg_with_client();
        // The admin consents for another tenant: the saved grant belongs to the first one.
        store
            .put_node_connector(&consented_node(Some(OTHER), true))
            .unwrap();
        let out = check_connection(
            &store,
            &vault,
            &cfg,
            "ada",
            "microsoft-365",
            200,
            must_not_be_called,
        )
        .unwrap();
        assert!(matches!(out, CheckOutcome::NeedsReconnect(_)), "{out:?}");
        // A stored endpoint that is not the consent tenant's is not used either.
        connect(&store, &vault, "bob");
        let mut row = store
            .connector_connection("bob", "microsoft-365")
            .unwrap()
            .unwrap();
        row.token_endpoint = "https://evil.example/token".into();
        store.put_connector_connection(&row).unwrap();
        store
            .put_node_connector(&consented_node(Some(TENANT), true))
            .unwrap();
        let out = check_connection(
            &store,
            &vault,
            &cfg,
            "bob",
            "microsoft-365",
            200,
            must_not_be_called,
        )
        .unwrap();
        assert!(matches!(out, CheckOutcome::NeedsReconnect(_)), "{out:?}");
        // No refresh token: nothing to send.
        connect(&store, &vault, "cy");
        vault
            .delete_secret_at(
                SecretScope::User,
                "cy",
                &refresh_token_binding_id("microsoft-365"),
            )
            .unwrap();
        let out = check_connection(
            &store,
            &vault,
            &cfg,
            "cy",
            "microsoft-365",
            200,
            must_not_be_called,
        )
        .unwrap();
        assert!(matches!(out, CheckOutcome::NeedsReconnect(_)), "{out:?}");
    }

    #[test]
    fn a_check_needs_an_enabled_configured_connector_and_a_connection() {
        let (store, vault) = rest_world();
        let cfg = cfg_with_client();
        let check = |store: &SqliteWorkflowStore, cfg: &ConnectorConfig, who: &str| {
            check_connection(
                store,
                &vault,
                cfg,
                who,
                "microsoft-365",
                1,
                must_not_be_called,
            )
            .unwrap_err()
        };
        assert!(matches!(
            check(&store, &cfg, "ada"),
            OrchestratorError::NotFound(_)
        ));
        connect(&store, &vault, "ada");
        store
            .put_node_connector(&consented_node(Some(TENANT), false))
            .unwrap();
        assert!(matches!(
            check(&store, &cfg, "ada"),
            OrchestratorError::PermissionDenied(_)
        ));
        store
            .put_node_connector(&consented_node(Some(TENANT), true))
            .unwrap();
        // The placeholder client id never reaches Microsoft.
        assert!(matches!(
            check(&store, &placeholder_cfg(), "ada"),
            OrchestratorError::Conflict(_)
        ));
        assert!(matches!(
            check(&store, &cfg, "bob"),
            OrchestratorError::NotFound(_)
        ));
    }

    #[test]
    fn disconnect_removes_both_rows_and_the_table_row_for_that_user_only() {
        let (store, vault) = rest_world();
        connect(&store, &vault, "ada");
        connect(&store, &vault, "bob");
        disconnect(&store, &vault, "ada", "microsoft-365").unwrap();
        assert_eq!(
            store.connector_connection("ada", "microsoft-365").unwrap(),
            None
        );
        for b in [
            refresh_token_binding_id("microsoft-365"),
            access_token_binding_id("microsoft-365"),
        ] {
            assert!(!vault.has_binding_at(SecretScope::User, "ada", &b).unwrap());
            assert!(vault.has_binding_at(SecretScope::User, "bob", &b).unwrap());
        }
        assert!(matches!(
            disconnect(&store, &vault, "ada", "microsoft-365").unwrap_err(),
            OrchestratorError::NotFound(_)
        ));
    }

    #[test]
    fn the_listing_shows_the_account_only_when_connected() {
        let row = ConnectorConnection {
            owner_user_id: "ada".into(),
            connector_id: "microsoft-365".into(),
            status: ConnectionStatus::Connected,
            label: Some("ada@contoso.com".into()),
            tenant_id: TENANT.into(),
            token_endpoint: String::new(),
            scopes: vec![],
            expires_at: None,
            connected_at: 1,
            updated_at: 1,
            last_check_at: None,
            last_error: None,
            client_id: None,
        };
        let node = consented_node(Some(TENANT), true);
        let st = rest_connection_state(&row, Some(&node), true, None);
        assert_eq!(st, CallerState::Connected);
        let mut e = list_entry(&m365(), &cfg_with_client(), Some(&node), Some(st), false);
        assert_eq!(e["label"], "Connected");
        show_account(
            &mut e,
            caller_state(&m365(), Some(&node), Some(st)),
            row.label.as_deref(),
        );
        assert_eq!(e["state"], "connected");
        assert_eq!(e["label"], "ada@contoso.com");
        // No refresh token, or a consent that moved to another tenant: reconnect.
        assert_eq!(
            rest_connection_state(&row, Some(&node), false, None),
            CallerState::NeedsReconnect
        );
        let moved = consented_node(Some(OTHER), true);
        assert_eq!(
            rest_connection_state(&row, Some(&moved), true, None),
            CallerState::NeedsReconnect
        );
        let mut e = list_entry(
            &m365(),
            &cfg_with_client(),
            Some(&node),
            Some(CallerState::NeedsReconnect),
            false,
        );
        show_account(&mut e, CallerState::NeedsReconnect, row.label.as_deref());
        assert_eq!(e["label"], "Needs reconnect");
    }

    // ---- the company address (CN-6.2) ----------------------------------------------------------

    fn zendesk_def() -> Connector {
        let raw = include_str!("../../vam-spec/tests/fixtures/zendesk-shaped-connector.json");
        vam_spec::connector::parse_connector(raw).unwrap()
    }

    fn put_with(enabled: bool, tenant: Option<Option<&str>>) -> PutNodeRequest {
        PutNodeRequest {
            tenant: tenant.map(|t| {
                t.map(|s| TenantRequest {
                    subdomain: s.to_string(),
                })
            }),
            ..put(enabled, None)
        }
    }

    #[test]
    fn enabling_needs_an_address_and_a_refusal_writes_no_row() {
        let (def, cfg) = (zendesk_def(), ConnectorConfig::default());
        let e = plan_put(&def, &cfg, None, &put_with(true, None), 1, "a").unwrap_err();
        assert!(matches!(&e, OrchestratorError::Conflict(m) if m.contains("company address")));
        for bad in ["a.b", "-a", "a_b", "xn--a", &"a".repeat(64)] {
            let e =
                plan_put(&def, &cfg, None, &put_with(true, Some(Some(bad))), 1, "a").unwrap_err();
            assert!(
                matches!(&e, OrchestratorError::InvalidContract(m) if m.contains("one DNS label") && !m.contains(bad)),
                "{bad}"
            );
        }
        let row = plan_put(
            &def,
            &cfg,
            None,
            &put_with(true, Some(Some(" Acme "))),
            1,
            "a",
        )
        .unwrap();
        assert_eq!(row.tenant_subdomain.as_deref(), Some("acme"));
        // Omitted keeps it across a disable and an enable; null clears only a disable.
        let off = plan_put(&def, &cfg, Some(&row), &put_with(false, None), 2, "a").unwrap();
        assert_eq!(off.tenant_subdomain.as_deref(), Some("acme"));
        let on = plan_put(&def, &cfg, Some(&off), &put_with(true, None), 3, "a").unwrap();
        assert_eq!(on.tenant_subdomain.as_deref(), Some("acme"));
        assert!(plan_put(&def, &cfg, Some(&on), &put_with(true, Some(None)), 4, "a").is_err());
        let cleared =
            plan_put(&def, &cfg, Some(&on), &put_with(false, Some(None)), 4, "a").unwrap();
        assert_eq!(cleared.tenant_subdomain, None);
        // A connector with no `tenant` takes none.
        let e = plan_put(
            &linear(),
            &cfg,
            None,
            &put_with(false, Some(Some("acme"))),
            1,
            "a",
        )
        .unwrap_err();
        assert!(matches!(e, OrchestratorError::InvalidContract(_)));
    }

    #[test]
    fn an_entry_shows_the_hosts_of_the_node_row_it_shows_not_of_a_later_read() {
        let store = SqliteWorkflowStore::in_memory().unwrap();
        crate::connector_client::test_support::install_company_address_rows(&store);
        let row = |sub: &str| NodeConnector {
            connector_id: "zendesk".into(),
            enabled: true,
            consent_tenant_id: None,
            consent_recorded_at: None,
            consent_recorded_by: None,
            tenant_subdomain: Some(sub.into()),
            updated_at: 1,
            updated_by: "a".into(),
        };
        // GET read the node row at `acme`; an admin saved `globex` before it read the entries.
        let shown_row = row("acme");
        store.put_node_connector(&row("globex")).unwrap();
        let later = store.connector_entry("zendesk").unwrap().unwrap();
        let e = entry_for_node(&later, Some(&shown_row));
        let mut entry = list_entry(
            &e.def,
            &ConnectorConfig::default(),
            Some(&shown_row),
            None,
            true,
        );
        annotate_source(&mut entry, &e, true);
        let text = entry.to_string();
        assert_eq!(entry["node"]["tenant"]["subdomain"], "acme");
        assert!(text.contains("acme.zendesk.com"), "{text}");
        assert!(
            !text.contains("globex"),
            "a host of another read is shown: {text}"
        );
        assert_eq!(e.template, later.template);
    }

    #[test]
    fn the_resolver_fills_what_the_store_reads_and_the_entry_shows() {
        let store = SqliteWorkflowStore::in_memory().unwrap();
        crate::connector_client::test_support::install_company_address_rows(&store);
        // No address: nothing usable, and the template is still the file as published.
        let e = store.connector_entry("zendesk").unwrap().unwrap();
        assert!(e.def.bindings.is_none() && e.template.bindings.is_some());
        let mut row = NodeConnector {
            connector_id: "zendesk".into(),
            enabled: true,
            consent_tenant_id: None,
            consent_recorded_at: None,
            consent_recorded_by: None,
            tenant_subdomain: Some("acme".into()),
            updated_at: 1,
            updated_by: "a".into(),
        };
        store.put_node_connector(&row).unwrap();
        let e = store.connector_entry("zendesk").unwrap().unwrap();
        assert_eq!(
            e.def.bindings.as_ref().unwrap()[0].hosts,
            ["acme.zendesk.com"]
        );
        assert_eq!(
            store
                .connector_entries()
                .unwrap()
                .iter()
                .find(|x| x.id() == "zendesk")
                .unwrap()
                .def,
            e.def
        );
        let shown = list_entry(&e.def, &ConnectorConfig::default(), Some(&row), None, false);
        assert!(!shown.to_string().contains("{subdomain}"), "{shown}");
        assert_eq!(shown["node"]["tenant"]["hosts"][0], "acme.zendesk.com");
        // Another company, another read; the stored row is the only input.
        row.tenant_subdomain = Some("globex".into());
        store.put_node_connector(&row).unwrap();
        let e = store.connector_entry("zendesk").unwrap().unwrap();
        assert_eq!(
            e.def.bindings.as_ref().unwrap()[0].hosts,
            ["globex.zendesk.com"]
        );
    }

    #[test]
    fn an_unaddressed_mcp_url_keeps_the_id_reserved() {
        let mut def = linear();
        def.tenant = Some(vam_spec::connector::TenantParam {
            name: "subdomain".into(),
            suffix: "linear.app".into(),
            label: "Address".into(),
        });
        def.mcp.as_mut().unwrap().url = "https://{subdomain}.linear.app/mcp".into();
        let none = resolve(&def, None);
        assert!(url_needs_address(&none.mcp.as_ref().unwrap().url));
        assert!(policy_of("linear", &none).is_some());
        let some = resolve(&def, Some("acme"));
        assert_eq!(
            some.mcp.as_ref().unwrap().url,
            "https://acme.linear.app/mcp"
        );
        assert!(!url_needs_address(&policy_of("linear", &some).unwrap().url));
    }

    #[test]
    fn a_connection_under_another_address_needs_a_reconnect() {
        let row = ConnectorConnection {
            owner_user_id: "ada".into(),
            connector_id: "zendesk".into(),
            status: ConnectionStatus::Connected,
            label: None,
            tenant_id: "acme".into(),
            token_endpoint: String::new(),
            scopes: vec![],
            expires_at: None,
            connected_at: 1,
            updated_at: 1,
            last_check_at: None,
            last_error: None,
            client_id: None,
        };
        let node = |sub: &str| NodeConnector {
            tenant_subdomain: Some(sub.into()),
            ..consented_node(None, true)
        };
        assert_eq!(
            rest_connection_state(&row, Some(&node("acme")), true, None),
            CallerState::Connected
        );
        assert_eq!(
            rest_connection_state(&row, Some(&node("globex")), true, None),
            CallerState::NeedsReconnect
        );
    }

    #[test]
    fn a_store_from_before_the_address_gets_the_column_and_keeps_its_rows() {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE node_enabled_connectors (connector_id TEXT PRIMARY KEY, enabled INTEGER NOT NULL,
                consent_tenant_id TEXT, consent_recorded_at INTEGER, consent_recorded_by TEXT,
                updated_at INTEGER NOT NULL, updated_by TEXT NOT NULL);
             INSERT INTO node_enabled_connectors VALUES ('linear', 1, NULL, NULL, NULL, 5, 'olga');",
        )
        .unwrap();
        initialize_schema(&c).unwrap();
        initialize_schema(&c).unwrap();
        let n = node_connector_in(&c, "linear").unwrap().unwrap();
        assert!(n.enabled && n.tenant_subdomain.is_none() && n.updated_by == "olga");
    }

    #[test]
    fn a_write_for_a_connector_with_no_address_is_refused_by_the_gate() {
        let store = SqliteWorkflowStore::in_memory().unwrap();
        let raw = include_str!("../../vam-spec/tests/fixtures/zendesk-shaped-connector.json");
        let mut json: Value = serde_json::from_str(raw).unwrap();
        json.as_object_mut().unwrap().remove("bindings");
        json["provider"] = json!({ "kind": "oauth2", "profile": "discover", "client": "org",
            "clientSecret": "none", "scopes": ["read"], "orgConsent": "none" });
        json["mcp"] = json!({ "url": "https://{subdomain}.zendesk.com/mcp",
            "tools": [ { "remote": "search", "sideEffect": "READ_ONLY" } ] });
        let urn = json["urn"].as_str().unwrap().to_string();
        {
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
                 VALUES ('connector', ?1, 1, 0, 0, 'm-1', 'z', 'z-v1', 'c', 'd', 1)",
                [&urn],
            )
            .unwrap();
        }
        let mut row = NodeConnector {
            connector_id: "zendesk".into(),
            enabled: true,
            consent_tenant_id: None,
            consent_recorded_at: None,
            consent_recorded_by: None,
            tenant_subdomain: None,
            updated_at: 1,
            updated_by: "a".into(),
        };
        store.put_node_connector(&row).unwrap();
        // The id stays reserved, a hand-added server cannot take it, and nothing connects.
        assert!(store.mcp_policy("zendesk").unwrap().is_some());
        let e = gate_mcp_write(&store, "zendesk", None, None).unwrap_err();
        assert!(
            matches!(&e, OrchestratorError::PermissionDenied(m) if m.contains("company address"))
        );
        let e =
            gate_mcp_write(&store, "zendesk", Some("https://evil.example/mcp"), None).unwrap_err();
        assert!(matches!(e, OrchestratorError::PermissionDenied(_)));
        // With the address the gate passes for the filled URL only.
        row.tenant_subdomain = Some("acme".into());
        store.put_node_connector(&row).unwrap();
        assert!(gate_mcp_write(
            &store,
            "zendesk",
            Some("https://acme.zendesk.com/mcp"),
            None
        )
        .is_ok());
        assert!(gate_mcp_write(
            &store,
            "zendesk",
            Some("https://{subdomain}.zendesk.com/mcp"),
            None
        )
        .is_err());
    }
}

/// The node's code exchange and refresh for an `oauth` connector (CN-6.3): the REST connection's
/// storage, the refresh's endpoint and the refusals that send nothing.
#[cfg(test)]
mod oauth_rest_tests {
    use super::*;
    use crate::connector_client::{
        test_support::install_company_address_rows, ClientPlan, ClientWrite, NodeClient, PlanBasis,
        TokenAuthMethod,
    };
    use secrecy::ExposeSecret;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    const ID: &str = "zendesk";

    fn node(address: &str, enabled: bool) -> NodeConnector {
        NodeConnector {
            connector_id: ID.into(),
            enabled,
            consent_tenant_id: None,
            consent_recorded_at: None,
            consent_recorded_by: None,
            tenant_subdomain: Some(address.into()),
            updated_at: 1,
            updated_by: "olga".into(),
        }
    }

    fn client_for(address: &str, client_id: &str, revoke: bool) -> NodeClient {
        let host = format!("{address}.zendesk.com");
        NodeClient {
            connector_id: ID.into(),
            client_id: client_id.into(),
            issuer: format!("https://{host}"),
            authorize_endpoint: format!("https://{host}/oauth/authorizations/new"),
            token_endpoint: format!("https://{host}/oauth/tokens"),
            revocation_endpoint: revoke.then(|| format!("https://{host}/oauth/revoke")),
            token_auth_method: TokenAuthMethod::default(),
            revocation_auth_method: None,
            secret_set: false,
            updated_at: 1,
            updated_by: "olga".into(),
            revision: 0,
        }
    }

    /// Saves `address` and its client the way Enable does.
    fn save(store: &SqliteWorkflowStore, vault: &SecretsVault, address: &str, client_id: &str) {
        save_with(store, vault, address, client_id, false);
    }

    fn save_with(
        store: &SqliteWorkflowStore,
        vault: &SecretsVault,
        address: &str,
        client_id: &str,
        revoke: bool,
    ) {
        let template = store.connector_entry(ID).unwrap().unwrap().template;
        let stored = store.node_client(ID).unwrap();
        let stored_address = store
            .node_connector(ID)
            .unwrap()
            .and_then(|n| n.tenant_subdomain);
        let client = client_for(address, client_id, revoke);
        let reauth = stored.as_ref().is_some_and(|e| {
            e.client_id != client.client_id || e.token_endpoint != client.token_endpoint
        });
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

    fn world(address: &str) -> (Arc<SqliteWorkflowStore>, Arc<SecretsVault>) {
        let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
        let vault = SecretsVault::for_tests(store.clone());
        install_company_address_rows(&store);
        save(&store, &vault, address, "zc1");
        (store, vault)
    }

    fn commit(
        store: &SqliteWorkflowStore,
        vault: &SecretsVault,
        owner: &str,
        address: &str,
        hosts: &[String],
    ) -> ConnectorConnection {
        let host = format!("{address}.zendesk.com");
        let endpoint = format!("https://{host}/oauth/tokens");
        store
            .commit_rest_connection(
                vault,
                &RestCommit {
                    owner,
                    connector_id: ID,
                    address,
                    token_endpoint: &endpoint,
                    client_id: "zc1",
                    token_host: &host,
                    access_hosts: hosts,
                    scopes: &["read".to_string(), "write".to_string()],
                    access_token: "ACCESS-0000000000000000",
                    refresh_token: "REFRESH-0000000000000000",
                    expires_at: Some(50),
                    now: 10,
                },
                &|_| Ok(()),
            )
            .unwrap()
    }

    fn acme_hosts() -> Vec<String> {
        vec!["acme.zendesk.com".to_string()]
    }

    fn dests(vault: &SecretsVault, owner: &str, binding: String) -> Vec<String> {
        vault
            .get_metadata_at(SecretScope::User, owner, &binding)
            .unwrap()
            .allowed_destinations
    }

    fn read(vault: &SecretsVault, owner: &str, binding: String) -> String {
        vault
            .decrypt_secret_at(SecretScope::User, owner, &binding)
            .unwrap()
            .expose_secret()
            .clone()
    }

    fn pin_refresh_token_elsewhere(vault: &SecretsVault) {
        vault
            .put_secret_with_owner(
                &refresh_token_binding_id(ID),
                "REFRESH-0000000000000000",
                SecretKind::Connection,
                &["evil.example".to_string()],
                None,
                "ada",
            )
            .unwrap();
    }

    fn rotated(
        t: &RestRefreshTarget<'_>,
        rt: &SecretString,
    ) -> Result<RefreshedToken, RefreshFailure> {
        let RestRefreshTarget::Oauth {
            token_endpoint,
            client_id,
            scope,
        } = *t
        else {
            panic!("an oauth connection is refreshed with an oauth target");
        };
        assert_eq!(token_endpoint, "https://acme.zendesk.com/oauth/tokens");
        assert_eq!(client_id, "zc1");
        assert_eq!(scope, Some("read write"));
        assert_eq!(rt.expose_secret(), "REFRESH-0000000000000000");
        Ok(RefreshedToken {
            access_token: "ACCESS-1111111111111111".into(),
            expires_in: Some(3600),
            refresh_token: Some("REFRESH-1111111111111111".into()),
        })
    }

    fn never(
        calls: &Arc<AtomicUsize>,
    ) -> impl for<'a> Fn(&RestRefreshTarget<'a>, &SecretString) -> Result<RefreshedToken, RefreshFailure>
    {
        let calls = calls.clone();
        move |_: &RestRefreshTarget<'_>, _: &SecretString| {
            calls.fetch_add(1, Ordering::SeqCst);
            Err(RefreshFailure::transient("must not be called"))
        }
    }

    fn config() -> ConnectorConfig {
        ConnectorConfig {
            client_ids: BTreeMap::new(),
        }
    }

    fn check(
        store: &SqliteWorkflowStore,
        vault: &SecretsVault,
        calls: &Arc<AtomicUsize>,
    ) -> CheckOutcome {
        check_connection(store, vault, &config(), "ada", ID, 20, never(calls)).unwrap()
    }

    #[test]
    fn a_commit_pins_the_refresh_token_to_the_token_host_and_the_access_token_to_every_binding_host(
    ) {
        let (store, vault) = world("acme");
        let hosts = vec![
            "acme.zendesk.com".to_string(),
            "api.acme.zendesk.com".to_string(),
        ];
        let row = commit(&store, &vault, "ada", "acme", &hosts);
        assert_eq!(row.status, ConnectionStatus::Connected);
        assert_eq!(row.tenant_id, "acme");
        assert_eq!(row.token_endpoint, "https://acme.zendesk.com/oauth/tokens");
        assert_eq!(row.label, None);
        assert_eq!(
            dests(&vault, "ada", refresh_token_binding_id(ID)),
            acme_hosts()
        );
        assert_eq!(dests(&vault, "ada", access_token_binding_id(ID)), hosts);
        let meta = vault
            .get_metadata_at(SecretScope::User, "ada", &access_token_binding_id(ID))
            .unwrap();
        assert_eq!(meta.kind, SecretKind::Connection);
        let node = store.node_connector(ID).unwrap();
        let endpoint = Some("https://acme.zendesk.com/oauth/tokens");
        assert_eq!(
            rest_connection_state(&row, node.as_ref(), true, endpoint),
            CallerState::Connected
        );
    }

    #[test]
    fn a_commit_whose_check_refuses_writes_no_row_and_no_token() {
        let (store, vault) = world("acme");
        let host = "acme.zendesk.com".to_string();
        let err = store
            .commit_rest_connection(
                &vault,
                &RestCommit {
                    owner: "ada",
                    connector_id: ID,
                    address: "acme",
                    token_endpoint: "https://acme.zendesk.com/oauth/tokens",
                    client_id: "zc1",
                    token_host: &host,
                    access_hosts: std::slice::from_ref(&host),
                    scopes: &[],
                    access_token: "ACCESS-0000000000000000",
                    refresh_token: "REFRESH-0000000000000000",
                    expires_at: None,
                    now: 10,
                },
                &|_| Err(OrchestratorError::Conflict("superseded".into())),
            )
            .unwrap_err();
        assert!(matches!(err, OrchestratorError::Conflict(_)));
        assert!(store.connector_connection("ada", ID).unwrap().is_none());
        for binding in [refresh_token_binding_id(ID), access_token_binding_id(ID)] {
            assert!(!vault
                .has_binding_at(SecretScope::User, "ada", &binding)
                .unwrap());
        }
    }

    #[test]
    fn a_refresh_goes_to_the_node_clients_endpoint_and_stores_the_rotated_token_first() {
        let (store, vault) = world("acme");
        commit(&store, &vault, "ada", "acme", &acme_hosts());
        let out = check_connection(&store, &vault, &config(), "ada", ID, 20, rotated).unwrap();
        let CheckOutcome::Connected(row) = out else {
            panic!("{out:?}")
        };
        assert_eq!(row.expires_at, Some(3620));
        assert_eq!(
            read(&vault, "ada", refresh_token_binding_id(ID)),
            "REFRESH-1111111111111111"
        );
        assert_eq!(
            read(&vault, "ada", access_token_binding_id(ID)),
            "ACCESS-1111111111111111"
        );
        assert_eq!(
            dests(&vault, "ada", refresh_token_binding_id(ID)),
            acme_hosts()
        );
        assert_eq!(
            dests(&vault, "ada", access_token_binding_id(ID)),
            acme_hosts()
        );
    }

    #[test]
    fn a_refresh_token_made_under_acme_is_never_sent_once_the_address_is_globex() {
        let (store, vault) = world("acme");
        commit(&store, &vault, "ada", "acme", &acme_hosts());
        // The admin moves to globex and saves the client again: the row is marked, nothing is sent.
        save(&store, &vault, "globex", "zc1");
        let calls = Arc::new(AtomicUsize::new(0));
        let out = check(&store, &vault, &calls);
        assert!(matches!(out, CheckOutcome::NeedsReconnect(_)), "{out:?}");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let row = store.connector_connection("ada", ID).unwrap().unwrap();
        assert_eq!(row.status, ConnectionStatus::NeedsReconnect);
    }

    #[test]
    fn a_refresh_sends_nothing_when_the_address_endpoint_pin_or_client_is_not_the_nodes() {
        let calls = Arc::new(AtomicUsize::new(0));
        // The node row's address moved without the client being saved again (no mark was written).
        let (store, vault) = world("acme");
        commit(&store, &vault, "ada", "acme", &acme_hosts());
        store.put_node_connector(&node("globex", true)).unwrap();
        let out = check(&store, &vault, &calls);
        assert!(matches!(out, CheckOutcome::NeedsReconnect(_)), "{out:?}");

        // The row names another endpoint than the node's client.
        let (store, vault) = world("acme");
        let mut row = commit(&store, &vault, "ada", "acme", &acme_hosts());
        row.token_endpoint = "https://evil.example/oauth/tokens".into();
        store.put_connector_connection(&row).unwrap();
        let out = check(&store, &vault, &calls);
        assert!(matches!(out, CheckOutcome::NeedsReconnect(_)), "{out:?}");
        let node_row = store.node_connector(ID).unwrap();
        let endpoint = Some("https://acme.zendesk.com/oauth/tokens");
        assert_eq!(
            rest_connection_state(&row, node_row.as_ref(), true, endpoint),
            CallerState::NeedsReconnect
        );

        // The refresh token's row is pinned to another host than the token endpoint's.
        let (store, vault) = world("acme");
        commit(&store, &vault, "ada", "acme", &acme_hosts());
        pin_refresh_token_elsewhere(&vault);
        let out = check(&store, &vault, &calls);
        assert!(matches!(out, CheckOutcome::NeedsReconnect(_)), "{out:?}");

        // The admin cleared the client.
        let (store, vault) = world("acme");
        commit(&store, &vault, "ada", "acme", &acme_hosts());
        store
            .connection
            .lock()
            .unwrap()
            .execute("DELETE FROM node_connector_clients", [])
            .unwrap();
        let out = check(&store, &vault, &calls);
        assert!(matches!(out, CheckOutcome::NeedsReconnect(_)), "{out:?}");
        assert_eq!(calls.load(Ordering::SeqCst), 0, "no refresh was sent");
    }

    #[test]
    fn replacing_the_client_marks_members_connections_and_saving_the_same_one_does_not() {
        let (store, vault) = world("acme");
        commit(&store, &vault, "ada", "acme", &acme_hosts());
        save(&store, &vault, "acme", "zc1");
        assert_eq!(
            store
                .connector_connection("ada", ID)
                .unwrap()
                .unwrap()
                .status,
            ConnectionStatus::Connected
        );
        save(&store, &vault, "acme", "zc2");
        let row = store.connector_connection("ada", ID).unwrap().unwrap();
        assert_eq!(row.status, ConnectionStatus::NeedsReconnect);
        assert_eq!(
            row.last_error.as_deref(),
            Some("the admin changed the client: reconnect")
        );
    }

    #[test]
    fn a_revoke_is_prepared_only_when_the_file_declares_one_and_the_pins_match() {
        let (store, vault) = world("acme");
        commit(&store, &vault, "ada", "acme", &acme_hosts());
        assert!(matches!(
            prepare_rest_revoke(&store, &vault, "ada", ID),
            RestRevokePlan::Nothing
        ));
        save_with(&store, &vault, "acme", "zc1", true);
        let RestRevokePlan::Send(revoke) = prepare_rest_revoke(&store, &vault, "ada", ID) else {
            panic!("the revoke is prepared");
        };
        assert_eq!(
            revoke.revocation_endpoint,
            "https://acme.zendesk.com/oauth/revoke"
        );
        assert_eq!(revoke.client_id, "zc1");
        assert_eq!(
            revoke.refresh_token.expose_secret(),
            "REFRESH-0000000000000000"
        );
        pin_refresh_token_elsewhere(&vault);
        assert!(matches!(
            prepare_rest_revoke(&store, &vault, "ada", ID),
            RestRevokePlan::Skipped
        ));
    }

    #[test]
    fn a_grant_issued_to_a_replaced_client_is_not_revoked_as_the_new_client() {
        let (store, vault) = world("acme");
        save_with(&store, &vault, "acme", "zc1", true);
        let row = commit(&store, &vault, "ada", "acme", &acme_hosts());
        assert_eq!(
            row.client_id.as_deref(),
            Some("zc1"),
            "the issuing client is kept"
        );
        // The admin replaces the client at the same token endpoint: the node's client id is zc2 now.
        save_with(&store, &vault, "acme", "zc2", true);
        assert!(
            matches!(
                prepare_rest_revoke(&store, &vault, "ada", ID),
                RestRevokePlan::Skipped
            ),
            "no credentials go out under another client's id; the provider's grant may remain"
        );
        // A row with no issuing client (NULL) is not revoked as the node's client either.
        let mut legacy = store.connector_connection("ada", ID).unwrap().unwrap();
        legacy.client_id = None;
        store.put_connector_connection(&legacy).unwrap();
        assert!(
            matches!(
                prepare_rest_revoke(&store, &vault, "ada", ID),
                RestRevokePlan::Skipped
            ),
            "an unknown issuing client is not the node's client"
        );
    }

    #[test]
    fn a_grant_whose_client_the_admin_cleared_may_remain_when_the_file_declares_a_revocation() {
        let clear = |store: &SqliteWorkflowStore| {
            store
                .connection
                .lock()
                .unwrap()
                .execute("DELETE FROM node_connector_clients", [])
                .unwrap();
        };
        // The file declares a revocation endpoint.
        let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
        let vault = SecretsVault::for_tests(store.clone());
        crate::connector_client::test_support::install_company_address_variant(&store, |j| {
            j["provider"]["endpoints"]["revoke"] =
                serde_json::json!("https://{subdomain}.zendesk.com/oauth/revoke");
        });
        save_with(&store, &vault, "acme", "zc1", true);
        commit(&store, &vault, "ada", "acme", &acme_hosts());
        clear(&store);
        assert!(
            matches!(
                prepare_rest_revoke(&store, &vault, "ada", ID),
                RestRevokePlan::Skipped
            ),
            "the grant remains and nothing can revoke it: the answer says so"
        );
        // No grant: nothing to say.
        store.delete_connector_connection("ada", ID).unwrap();
        assert!(matches!(
            prepare_rest_revoke(&store, &vault, "ada", ID),
            RestRevokePlan::Nothing
        ));
        // A file with no revocation endpoint: nothing could have revoked it either way.
        let (store, vault) = world("acme");
        save(&store, &vault, "acme", "zc1");
        commit(&store, &vault, "ada", "acme", &acme_hosts());
        clear(&store);
        assert!(matches!(
            prepare_rest_revoke(&store, &vault, "ada", ID),
            RestRevokePlan::Nothing
        ));
    }

    #[test]
    fn an_explicit_check_never_refreshes_a_grant_the_admin_invalidated() {
        let (store, vault) = world("acme");
        commit(&store, &vault, "ada", "acme", &acme_hosts());
        // Same token endpoint, another client: the member's grant is marked, and a check must not
        // present the old refresh token to the new client or clear the mark.
        save(&store, &vault, "acme", "zc2");
        let calls = Arc::new(AtomicUsize::new(0));
        let out = check(&store, &vault, &calls);
        let CheckOutcome::NeedsReconnect(row) = out else {
            panic!("{out:?}")
        };
        assert_eq!(calls.load(Ordering::SeqCst), 0, "no refresh was sent");
        assert_eq!(row.status, ConnectionStatus::NeedsReconnect);
        let stored = store.connector_connection("ada", ID).unwrap().unwrap();
        assert_eq!(stored.status, ConnectionStatus::NeedsReconnect);
        assert_eq!(
            stored.last_error.as_deref(),
            Some("the admin changed the client: reconnect")
        );
        // The same holds when the provider would accept it.
        let out = check_connection(&store, &vault, &config(), "ada", ID, 20, rotated_zc2).unwrap();
        assert!(matches!(out, CheckOutcome::NeedsReconnect(_)), "{out:?}");
        assert_eq!(
            read(&vault, "ada", refresh_token_binding_id(ID)),
            "REFRESH-0000000000000000"
        );
    }

    fn rotated_zc2(
        _: &RestRefreshTarget<'_>,
        _: &SecretString,
    ) -> Result<RefreshedToken, RefreshFailure> {
        Ok(RefreshedToken {
            access_token: "ACCESS-2222222222222222".into(),
            expires_in: Some(3600),
            refresh_token: Some("REFRESH-2222222222222222".into()),
        })
    }

    #[test]
    fn a_refresh_in_flight_does_not_undo_an_admins_client_change() {
        // The admin replaces the client while the provider is answering.
        for answer in ["accepted", "transient"] {
            let (store, vault) = world("acme");
            commit(&store, &vault, "ada", "acme", &acme_hosts());
            let admin = |store: &SqliteWorkflowStore, vault: &SecretsVault| {
                save(store, vault, "acme", "zc2");
            };
            let out = check_connection(&store, &vault, &config(), "ada", ID, 20, |_, _| {
                admin(&store, &vault);
                if answer == "accepted" {
                    rotated_zc2(
                        &RestRefreshTarget::Oauth {
                            token_endpoint: "",
                            client_id: "",
                            scope: None,
                        },
                        &SecretString::new(String::new()),
                    )
                } else {
                    Err(RefreshFailure::transient("provider is down"))
                }
            })
            .unwrap();
            assert!(
                matches!(out, CheckOutcome::NeedsReconnect(_)),
                "{answer}: {out:?}"
            );
            let row = store.connector_connection("ada", ID).unwrap().unwrap();
            assert_eq!(row.status, ConnectionStatus::NeedsReconnect, "{answer}");
            assert_eq!(
                row.last_error.as_deref(),
                Some("the admin changed the client: reconnect"),
                "{answer}"
            );
            assert_eq!(row.expires_at, Some(50), "{answer}: the old expiry stands");
            // The new tokens were dropped, not stored beside the invalidated row.
            assert_eq!(
                read(&vault, "ada", refresh_token_binding_id(ID)),
                "REFRESH-0000000000000000",
                "{answer}"
            );
            assert_eq!(
                read(&vault, "ada", access_token_binding_id(ID)),
                "ACCESS-0000000000000000",
                "{answer}"
            );
        }
    }

    #[test]
    fn a_rotated_refresh_token_is_kept_when_the_admin_resaves_the_same_client_or_disables_meanwhile(
    ) {
        let disable = |store: &SqliteWorkflowStore| {
            let mut n = store.node_connector(ID).unwrap().unwrap();
            n.enabled = false;
            store.put_node_connector(&n).unwrap();
        };
        for meanwhile in ["same client resaved", "disabled"] {
            let (store, vault) = world("acme");
            commit(&store, &vault, "ada", "acme", &acme_hosts());
            let out = check_connection(&store, &vault, &config(), "ada", ID, 20, |_, _| {
                if meanwhile == "disabled" {
                    disable(&store);
                } else {
                    save(&store, &vault, "acme", "zc1");
                }
                rotated_zc2(
                    &RestRefreshTarget::Oauth {
                        token_endpoint: "",
                        client_id: "",
                        scope: None,
                    },
                    &SecretString::new(String::new()),
                )
            })
            .unwrap();
            // The access token and the Connected result are not published under a moved state...
            assert!(
                matches!(out, CheckOutcome::Unavailable(_)),
                "{meanwhile}: {out:?}"
            );
            assert_eq!(
                read(&vault, "ada", access_token_binding_id(ID)),
                "ACCESS-0000000000000000",
                "{meanwhile}"
            );
            // ...but the provider spent the old refresh token: the rotated one is what is kept.
            assert_eq!(
                read(&vault, "ada", refresh_token_binding_id(ID)),
                "REFRESH-2222222222222222",
                "{meanwhile}"
            );
            assert_eq!(
                dests(&vault, "ada", refresh_token_binding_id(ID)),
                vec!["acme.zendesk.com".to_string()],
                "{meanwhile}"
            );
        }
    }

    #[test]
    fn a_refresh_stores_the_access_token_and_its_expiry_in_one_write() {
        let (store, vault) = world("acme");
        commit(&store, &vault, "ada", "acme", &acme_hosts());
        let row = store.connector_connection("ada", ID).unwrap().unwrap();
        let mut next = row.clone();
        next.expires_at = Some(9_999);
        let host = "acme.zendesk.com".to_string();
        let write = |check: &dyn Fn(&Connection) -> Result<(), OrchestratorError>| {
            store.commit_rest_refresh(
                &vault,
                &RefreshCommit {
                    owner: "ada",
                    connector_id: ID,
                    token_host: &host,
                    access_hosts: std::slice::from_ref(&host),
                    rotated_refresh: None,
                    access_token: Some("ACCESS-3333333333333333"),
                    row: Some(&next),
                },
                check,
            )
        };
        // A write that is refused stores neither the access token nor the expiry.
        let err = write(&|_| Err(OrchestratorError::Conflict("moved".into()))).unwrap_err();
        assert!(matches!(err, OrchestratorError::Conflict(_)));
        assert_eq!(
            store
                .connector_connection("ada", ID)
                .unwrap()
                .unwrap()
                .expires_at,
            Some(50)
        );
        assert_eq!(
            read(&vault, "ada", access_token_binding_id(ID)),
            "ACCESS-0000000000000000"
        );
        // One that passes stores both, pinned as at the exchange.
        write(&|_| Ok(())).unwrap();
        assert_eq!(
            store
                .connector_connection("ada", ID)
                .unwrap()
                .unwrap()
                .expires_at,
            Some(9_999)
        );
        assert_eq!(
            read(&vault, "ada", access_token_binding_id(ID)),
            "ACCESS-3333333333333333"
        );
        assert_eq!(
            dests(&vault, "ada", access_token_binding_id(ID)),
            acme_hosts()
        );
        // A rotated refresh token is its own write, and a refused one is not stored either.
        let rotated = |check: &dyn Fn(&Connection) -> Result<(), OrchestratorError>| {
            store.commit_rest_refresh(
                &vault,
                &RefreshCommit {
                    owner: "ada",
                    connector_id: ID,
                    token_host: &host,
                    access_hosts: std::slice::from_ref(&host),
                    rotated_refresh: Some("REFRESH-3333333333333333"),
                    access_token: None,
                    row: None,
                },
                check,
            )
        };
        assert!(rotated(&|_| Err(OrchestratorError::Conflict("moved".into()))).is_err());
        assert_eq!(
            read(&vault, "ada", refresh_token_binding_id(ID)),
            "REFRESH-0000000000000000"
        );
        rotated(&|_| Ok(())).unwrap();
        assert_eq!(
            read(&vault, "ada", refresh_token_binding_id(ID)),
            "REFRESH-3333333333333333"
        );
        assert_eq!(
            dests(&vault, "ada", refresh_token_binding_id(ID)),
            acme_hosts()
        );
    }

    #[test]
    fn an_oauth_connector_has_no_app_token_route() {
        let (store, _vault) = world("acme");
        let def = store.connector_entry(ID).unwrap().unwrap().def;
        let req = PutConnectionRequest {
            access_token: "a".into(),
            refresh_token: "r".into(),
            id_token: "x.y.z".into(),
            expires_in: None,
        };
        let err = plan_connection(&def, store.node_connector(ID).unwrap().as_ref(), &req, 1)
            .unwrap_err()
            .to_string();
        assert!(err.contains("code exchange"), "{err}");
    }
}

/// The one rule both freshness checks share (CN-6.4): the swap of a call whose intent check just
/// refreshed does not spend a second refresh while the token has time.
#[cfg(test)]
mod refresh_dedupe_tests {
    use super::*;

    fn conn(expires_at: Option<i64>) -> ConnectorConnection {
        ConnectorConnection {
            owner_user_id: "ada".into(),
            connector_id: "zendesk".into(),
            status: ConnectionStatus::Connected,
            label: None,
            tenant_id: "acme".into(),
            token_endpoint: String::new(),
            scopes: vec![],
            expires_at,
            connected_at: 1,
            updated_at: 1,
            last_check_at: Some(100),
            last_error: None,
            client_id: None,
        }
    }

    #[test]
    fn a_token_outside_the_window_is_never_refreshed_and_one_inside_is() {
        for marked in [false, true] {
            assert!(!needs_refresh(&conn(Some(1_000)), 100, 120, marked));
            assert!(!needs_refresh(&conn(None), 100, 120, marked));
            assert!(needs_refresh(&conn(Some(90)), 100, 120, marked), "expired");
        }
        assert!(needs_refresh(&conn(Some(200)), 100, 120, false));
    }

    #[test]
    fn the_mark_skips_the_swap_only_while_the_token_has_twenty_seconds() {
        // Minted for 60 s at 100: inside the intent's 60 s horizon and the swap's 120 s window.
        let c = conn(Some(160));
        assert!(!needs_refresh(&c, 100, 120, true));
        assert!(!needs_refresh(&c, 140, 120, true), "20 s left is enough");
        assert!(
            needs_refresh(&c, 141, 120, true),
            "under 20 s left refreshes"
        );
        assert!(
            needs_refresh(&c, 100, 120, false),
            "no mark, the window decides"
        );
    }
}
