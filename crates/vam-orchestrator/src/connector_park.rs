//! Connector gate at the tool intent (CN-2.5).
//!
//! A tool whose skill declares `requiresConnector` is checked before the policy decision:
//! a connector that is unknown or not enabled is refused ("ask your admin", nothing to park on:
//! the member cannot fix it, open question 7); a member who has not connected, or whose
//! connection needs reconnecting, is parked until they connect (`reauth:user:<owner>:connector:<id>`,
//! resolved by `PUT /v1/connectors/{id}/connection`). Before that, a connected token that
//! expires within the tool's timeout is refreshed once; a refusal there moves the connection to
//! needs-reconnect (and raises the notification) so the call parks before anything runs.
//! A network or 5xx failure changes nothing and lets the call through: the swap refuses it
//! with `connection_unavailable` (open question 6).

use std::sync::Arc;

use vam_spec::v1::{
    tool_intent_response::Disposition, ErrorStatus, ToolIntentRequest, ToolIntentResponse,
};

use crate::connection_egress::DEFAULT_TOOL_TIMEOUT;
use crate::connectors::{self, ConnectionStatus, Freshness, RestRefresher};
use crate::secrets::{SecretScope, SecretsVault};
use crate::workflow::SqliteWorkflowStore;

/// What the broker worked out for one intent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectorGate {
    /// The tool needs no connector, or the member is connected.
    Clear,
    /// Refused with this fixed text; no park.
    Refuse(String),
    /// Parked until `connector_id` is connected (`reconnect`: it was connected and is not now).
    Park {
        connector_id: String,
        name: String,
        reconnect: bool,
    },
}

/// What a parked call tells the member: "Connect <name> to continue", or "Reconnect <name> to
/// continue" when they were connected and are not now. The same words for every sign-in kind.
pub(crate) fn park_message(reconnect: bool, name: &str) -> String {
    let verb = if reconnect { "Reconnect" } else { "Connect" };
    format!("{verb} {name} to continue")
}

/// The park key: scope, owner and `connector:<id>` (an MCP id has no colon, so no collision).
pub fn park_key(owner: &str, connector_id: &str) -> String {
    crate::hitl::reauth_key(
        SecretScope::User,
        owner,
        &format!("connector:{connector_id}"),
    )
}

/// The owner and connector id of a [`park_key`], if `key` is one.
pub fn parse_park_key(key: &str) -> Option<(String, String)> {
    let prefix = crate::hitl::reauth_key(SecretScope::User, "", "");
    // `<prefix>` is `reauth:user::`: the owner and id go between and after its last colon.
    let rest = key.strip_prefix(prefix.strip_suffix(':')?)?;
    let (owner, id) = rest.rsplit_once(":connector:")?;
    (park_key(owner, id) == key).then(|| (owner.to_string(), id.to_string()))
}

/// The REST connector park keys whose connection is `Connected` now: parks a sign-in already
/// answered, whose resume did not run (the process stopped after the connection committed and
/// before the post-lock resume). [`ConnectorGate::Clear`] is what the gate decides for such a
/// connection, so resuming these parks is what the sign-in would have done.
pub fn connected_park_keys(store: &SqliteWorkflowStore) -> Vec<String> {
    let Ok(keys) = store.pending_reauth_keys() else {
        return Vec::new();
    };
    keys.into_iter()
        .filter(|key| {
            parse_park_key(key).is_some_and(|(owner, id)| {
                matches!(
                    store.connector_connection(&owner, &id),
                    Ok(Some(c)) if c.status == ConnectionStatus::Connected
                )
            })
        })
        .collect()
}

struct Needed {
    owner: String,
    id: String,
    name: String,
    /// The connector as the node uses it now (the company address filled in).
    def: Box<vam_spec::connector::Connector>,
}

enum Lookup {
    None,
    Refuse(String),
    Needed(Needed),
}

fn lookup(store: &SqliteWorkflowStore, session_id: &str, owner: &str, tool: &str) -> Lookup {
    let Some(urn) = store
        .tool_egress(session_id, tool)
        .ok()
        .flatten()
        .and_then(|r| r.requires_connector)
    else {
        return Lookup::None;
    };
    let Some(def) = store
        .connector_entries()
        .ok()
        .and_then(|all| all.into_iter().find(|e| e.def.urn == urn))
        .map(|e| e.def)
    else {
        return Lookup::Refuse(
            "This tool needs a connector this server does not know; ask your admin.".into(),
        );
    };
    // An MCP connector's connection is its MCP row (its own park); not this gate's.
    if def.mcp.is_some() {
        return Lookup::None;
    }
    let id = connectors::connector_id(&def);
    let name = def.display_name.clone();
    // The admin's consent is asked for only when the connector's sign-in needs it (Microsoft 365);
    // an `oauth` connector with `orgConsent: none` has none to record (CN-6.4).
    let needs_consent = connectors::needs_admin_consent(&def);
    let enabled = store
        .node_connector(&id)
        .ok()
        .flatten()
        .is_some_and(|n| n.enabled && (!needs_consent || n.consent_tenant_id.is_some()));
    if !enabled {
        return Lookup::Refuse(format!(
            "{name} is not enabled for this organization; ask your admin to enable {name}."
        ));
    }
    Lookup::Needed(Needed {
        owner: owner.to_string(),
        id,
        name,
        def: Box::new(def),
    })
}

/// Whether the member's connection row lets the call through. An OAuth connection: its status (the
/// refresh and the swap decide the rest). An API key connection (CN-7.6): its status, its key row
/// pinned to exactly the connector's filled hosts, and the node's address, the same state the tile
/// shows. Nothing is refreshed and no provider is asked either way.
fn is_connected(
    store: &SqliteWorkflowStore,
    n: &Needed,
    c: &connectors::ConnectorConnection,
) -> bool {
    if c.status != ConnectionStatus::Connected {
        return false;
    }
    if !connectors::is_api_key(&n.def) {
        return true;
    }
    let node = store.node_connector(&n.id).ok().flatten();
    connectors::rest_state_for(store, &n.def, &n.owner, c, node.as_ref(), || false, None)
        == connectors::CallerState::Connected
}

fn state_of(store: &SqliteWorkflowStore, n: &Needed) -> ConnectorGate {
    match store.connector_connection(&n.owner, &n.id) {
        Ok(Some(c)) if is_connected(store, n, &c) => ConnectorGate::Clear,
        Ok(Some(_)) => ConnectorGate::Park {
            connector_id: n.id.clone(),
            name: n.name.clone(),
            reconnect: true,
        },
        Ok(None) => ConnectorGate::Park {
            connector_id: n.id.clone(),
            name: n.name.clone(),
            reconnect: false,
        },
        // Unreadable: let the swap decide (it refuses without a connection).
        Err(_) => ConnectorGate::Clear,
    }
}

/// The gate for one intent, without the token look: refusals and parks from stored state.
pub fn gate(store: &SqliteWorkflowStore, session_id: &str, tool: &str) -> ConnectorGate {
    let Ok(owner) = store.session_owner_user_id(session_id) else {
        return ConnectorGate::Clear;
    };
    gate_for(store, session_id, &owner, tool)
}

fn gate_for(
    store: &SqliteWorkflowStore,
    session_id: &str,
    owner: &str,
    tool: &str,
) -> ConnectorGate {
    match lookup(store, session_id, owner, tool) {
        Lookup::None => ConnectorGate::Clear,
        Lookup::Refuse(t) => ConnectorGate::Refuse(t),
        Lookup::Needed(n) => state_of(store, &n),
    }
}

/// [`gate`], then for a connected member the intent-time refresh (`ensure_fresh` with the
/// tool's timeout as horizon).
///
/// The flag says the intent refreshed the member's token; the broker records it for the call
/// (`connection_egress::IntentRefreshes`) so the swap of the same call does not refresh again.
pub async fn precheck(
    store: &Arc<SqliteWorkflowStore>,
    vault: &Arc<SecretsVault>,
    session_id: &str,
    tool: &str,
) -> (ConnectorGate, bool) {
    let Ok(owner) = store.session_owner_user_id(session_id) else {
        return (ConnectorGate::Clear, false);
    };
    precheck_with_outcome(
        store,
        vault,
        &connectors::config(),
        &connectors::live_refresher(),
        session_id,
        &owner,
        tool,
    )
    .await
}

#[cfg(test)]
pub(crate) async fn precheck_with(
    store: &Arc<SqliteWorkflowStore>,
    vault: &Arc<SecretsVault>,
    cfg: &connectors::ConnectorConfig,
    refresher: &RestRefresher,
    session_id: &str,
    owner: &str,
    tool: &str,
) -> ConnectorGate {
    precheck_with_outcome(store, vault, cfg, refresher, session_id, owner, tool)
        .await
        .0
}

/// [`precheck_with`], and whether this intent refreshed the member's token.
pub(crate) async fn precheck_with_outcome(
    store: &Arc<SqliteWorkflowStore>,
    vault: &Arc<SecretsVault>,
    cfg: &connectors::ConnectorConfig,
    refresher: &RestRefresher,
    session_id: &str,
    owner: &str,
    tool: &str,
) -> (ConnectorGate, bool) {
    let n = match lookup(store, session_id, owner, tool) {
        Lookup::None => return (ConnectorGate::Clear, false),
        Lookup::Refuse(t) => return (ConnectorGate::Refuse(t), false),
        Lookup::Needed(n) => n,
    };
    let first = state_of(store, &n);
    if first != ConnectorGate::Clear {
        return (first, false);
    }
    let horizon = i64::try_from(DEFAULT_TOOL_TIMEOUT.as_secs()).unwrap_or(60);
    match crate::connection_egress::ensure_fresh(
        store, vault, cfg, refresher, &n.owner, &n.id, horizon,
    )
    .await
    {
        Freshness::NeedsReconnect => (state_of(store, &n), false),
        Freshness::Refreshed => (ConnectorGate::Clear, true),
        Freshness::Fresh | Freshness::Unavailable => (ConnectorGate::Clear, false),
    }
}

impl crate::security::SessionSecurityContext {
    /// Applies a [`ConnectorGate`] at the intent: a refusal, a park, or nothing.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn gate_connector(
        &self,
        store: &SqliteWorkflowStore,
        request: &ToolIntentRequest,
        catalog_entry: &crate::session::ToolCatalogEntry,
        args_digest: &str,
        args_json: &serde_json::Value,
        run_id: &str,
        gate: ConnectorGate,
    ) -> Option<ToolIntentResponse> {
        let refuse = |message: String| ToolIntentResponse {
            disposition: Disposition::Denied as i32,
            approval_request_id: None,
            error: Some(ErrorStatus {
                code: 7, // PERMISSION_DENIED
                message,
                details: vec![],
                retryable: false,
            }),
            capability_token: None,
            resource_envelope: None,
            replayed: None,
        };
        match gate {
            ConnectorGate::Clear => None,
            ConnectorGate::Refuse(text) => Some(refuse(text)),
            ConnectorGate::Park {
                connector_id,
                name,
                reconnect,
            } => {
                let message = park_message(reconnect, &name);
                let Ok(owner) = store.session_owner_user_id(&self.session_id) else {
                    return Some(refuse(message));
                };
                if run_id.is_empty() {
                    return Some(refuse(message));
                }
                let parked = self.suspend_for_reauth(
                    store,
                    request,
                    catalog_entry,
                    args_digest,
                    args_json,
                    run_id,
                    message.clone(),
                    park_key(&owner, &connector_id),
                    &connector_id,
                );
                if reconnect && parked.is_some() {
                    store.attach_run_to_connection_needs_reauth(&owner, &connector_id, run_id);
                }
                parked.or_else(|| Some(refuse(message)))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection_egress::ToolEgressRow;
    use crate::connectors::RestRefreshTarget;
    use crate::connectors::{ConnectorConnection, NodeConnector};
    use crate::mcp_oauth_refresh::{RefreshFailure, RefreshedToken};
    use secrecy::SecretString;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const URN: &str = "urn:vam:connector:hub.squrell.cloud/squrell/microsoft-365";
    const TENANT: &str = "11111111-1111-4111-8111-111111111111";
    const ME: &str = "user-me";
    const OTHER: &str = "user-other";
    const S: &str = "sess-1";
    const T: &str = "m365_probe_search";
    const REFRESH: &str = "refresh-VALUE-1";

    fn fx() -> (Arc<SqliteWorkflowStore>, Arc<SecretsVault>) {
        let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
        let vault = SecretsVault::for_tests(store.clone());
        store
            .record_tool_egress(
                S,
                &[ToolEgressRow {
                    tool: T.into(),
                    requires_connector: Some(URN.into()),
                    shapes: vec![],
                    effect_class: None,
                }],
            )
            .unwrap();
        (store, vault)
    }

    fn node(store: &SqliteWorkflowStore, enabled: bool, consent: bool) {
        store
            .put_node_connector(&NodeConnector {
                tenant_subdomain: None,
                connector_id: "microsoft-365".into(),
                enabled,
                consent_tenant_id: consent.then(|| TENANT.to_string()),
                consent_recorded_at: Some(1),
                consent_recorded_by: Some("admin".into()),
                updated_at: 1,
                updated_by: "admin".into(),
            })
            .unwrap();
    }

    fn connect(
        store: &SqliteWorkflowStore,
        vault: &SecretsVault,
        owner: &str,
        status: ConnectionStatus,
        expires_at: i64,
    ) {
        store
            .put_connector_connection(&ConnectorConnection {
                owner_user_id: owner.into(),
                connector_id: "microsoft-365".into(),
                status,
                label: None,
                tenant_id: TENANT.into(),
                token_endpoint: crate::mcp_oauth_refresh::entra_token_endpoint(TENANT).unwrap(),
                scopes: vec![],
                expires_at: Some(expires_at),
                connected_at: 1,
                updated_at: 1,
                last_check_at: None,
                last_error: None,
                client_id: None,
            })
            .unwrap();
        for (b, v, h) in [
            (
                connectors::refresh_token_binding_id("microsoft-365"),
                REFRESH,
                "login.microsoftonline.com",
            ),
            (
                connectors::access_token_binding_id("microsoft-365"),
                "access-VALUE",
                "graph.microsoft.com",
            ),
        ] {
            vault
                .put_secret_with_owner(
                    &b,
                    v,
                    crate::secrets::SecretKind::Connection,
                    &[h.to_string()],
                    None,
                    owner,
                )
                .unwrap();
        }
    }

    fn cfg() -> connectors::ConnectorConfig {
        connectors::ConnectorConfig {
            client_ids: [("microsoft-365".to_string(), "e2e-client".to_string())].into(),
        }
    }

    fn refresher(
        calls: &Arc<AtomicUsize>,
        reply: fn() -> Result<RefreshedToken, RefreshFailure>,
    ) -> RestRefresher {
        let calls = calls.clone();
        Arc::new(move |_: &RestRefreshTarget<'_>, _: &SecretString| {
            calls.fetch_add(1, Ordering::SeqCst);
            reply()
        })
    }

    fn ok() -> Result<RefreshedToken, RefreshFailure> {
        Ok(RefreshedToken {
            access_token: "new-access".into(),
            expires_in: Some(3600),
            refresh_token: None,
        })
    }

    fn revoked() -> Result<RefreshedToken, RefreshFailure> {
        Err(RefreshFailure::needs_reauth("invalid_grant"))
    }

    fn down() -> Result<RefreshedToken, RefreshFailure> {
        Err(RefreshFailure::transient(
            "OAuth token endpoint request failed",
        ))
    }

    fn open_rows(store: &SqliteWorkflowStore, user: &str) -> i64 {
        store
            .connection
            .lock()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM notifications
                 WHERE kind = 'connection.needs_reauth' AND user_id = ?1 AND resolved_at IS NULL",
                [user],
                |r| r.get(0),
            )
            .unwrap()
    }

    #[test]
    fn a_tool_that_needs_no_connector_is_clear() {
        let (store, _) = fx();
        assert_eq!(gate_for(&store, S, ME, "other_tool"), ConnectorGate::Clear);
    }

    #[test]
    fn a_connector_that_is_not_enabled_or_not_consented_is_refused_without_a_park() {
        let (store, _) = fx();
        for (enabled, consent) in [(false, true), (true, false)] {
            node(&store, enabled, consent);
            let ConnectorGate::Refuse(t) = gate_for(&store, S, ME, T) else {
                panic!("refused")
            };
            assert!(t.contains("ask your admin"), "{t}");
        }
        // No node row at all.
        let store2 = fx().0;
        assert!(matches!(
            gate_for(&store2, S, ME, T),
            ConnectorGate::Refuse(_)
        ));
    }

    #[test]
    fn an_admin_required_connector_enabled_without_consent_is_still_refused() {
        // Microsoft 365's sign-in needs the admin's recorded consent (`orgConsent:
        // admin_required`): enabled with no consent is refused, with a connected member too.
        let (store, vault) = fx();
        node(&store, true, false);
        connect(
            &store,
            &vault,
            ME,
            ConnectionStatus::Connected,
            10_000_000_000,
        );
        let ConnectorGate::Refuse(t) = gate_for(&store, S, ME, T) else {
            panic!("refused");
        };
        assert!(t.contains("not enabled"), "{t}");
        node(&store, true, true);
        assert_eq!(gate_for(&store, S, ME, T), ConnectorGate::Clear);
    }

    #[test]
    fn a_company_address_connector_needs_no_consent_row_to_be_enabled() {
        // `orgConsent: none` (an `oauth` connector from a marketplace): the admin's Enable stores
        // the address and no consent, and the gate must still let a connected member through.
        let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
        crate::connector_client::test_support::install_company_address_rows(&store);
        let urn = "urn:vam:connector:connectors.community.example/community/zendesk";
        store
            .record_tool_egress(
                S,
                &[ToolEgressRow {
                    tool: "ticket_search".into(),
                    requires_connector: Some(urn.into()),
                    shapes: vec![],
                    effect_class: None,
                }],
            )
            .unwrap();
        let row = |enabled| NodeConnector {
            connector_id: "zendesk".into(),
            enabled,
            consent_tenant_id: None,
            consent_recorded_at: None,
            consent_recorded_by: None,
            tenant_subdomain: Some("acme".into()),
            updated_at: 1,
            updated_by: "olga".into(),
        };
        store.put_node_connector(&row(false)).unwrap();
        assert!(matches!(
            gate_for(&store, S, ME, "ticket_search"),
            ConnectorGate::Refuse(t) if t.contains("not enabled")
        ));
        store.put_node_connector(&row(true)).unwrap();
        // Enabled, member not connected: parked on this connector, not refused.
        assert!(matches!(
            gate_for(&store, S, ME, "ticket_search"),
            ConnectorGate::Park { ref connector_id, reconnect: false, .. } if connector_id == "zendesk"
        ));
        store
            .put_connector_connection(&ConnectorConnection {
                owner_user_id: ME.into(),
                connector_id: "zendesk".into(),
                status: ConnectionStatus::Connected,
                label: None,
                tenant_id: "acme".into(),
                token_endpoint: "https://acme.zendesk.com/oauth/tokens".into(),
                scopes: vec![],
                expires_at: None,
                connected_at: 1,
                updated_at: 1,
                last_check_at: None,
                last_error: None,
                client_id: None,
            })
            .unwrap();
        assert_eq!(
            gate_for(&store, S, ME, "ticket_search"),
            ConnectorGate::Clear
        );
    }

    #[test]
    fn an_unknown_connector_urn_is_refused() {
        let (store, _) = fx();
        store
            .record_tool_egress(
                S,
                &[ToolEgressRow {
                    tool: T.into(),
                    requires_connector: Some("urn:vam:connector:x/y/nope".into()),
                    shapes: vec![],
                    effect_class: None,
                }],
            )
            .unwrap();
        assert!(matches!(
            gate_for(&store, S, ME, T),
            ConnectorGate::Refuse(_)
        ));
    }

    #[test]
    fn no_connection_parks_to_connect_and_a_dead_one_to_reconnect() {
        let (store, vault) = fx();
        node(&store, true, true);
        let g = gate_for(&store, S, ME, T);
        assert_eq!(
            g,
            ConnectorGate::Park {
                connector_id: "microsoft-365".into(),
                name: "Microsoft 365".into(),
                reconnect: false
            }
        );
        connect(&store, &vault, ME, ConnectionStatus::NeedsReconnect, 0);
        assert!(matches!(
            gate_for(&store, S, ME, T),
            ConnectorGate::Park {
                reconnect: true,
                ..
            }
        ));
        // Another member's state is their own.
        assert!(matches!(
            gate_for(&store, S, OTHER, T),
            ConnectorGate::Park {
                reconnect: false,
                ..
            }
        ));
        connect(
            &store,
            &vault,
            ME,
            ConnectionStatus::Connected,
            4_000_000_000,
        );
        assert_eq!(gate_for(&store, S, ME, T), ConnectorGate::Clear);
    }

    #[test]
    fn the_park_key_carries_the_owner_and_cannot_collide_with_an_mcp_id() {
        assert_ne!(
            park_key(ME, "microsoft-365"),
            park_key(OTHER, "microsoft-365")
        );
        assert!(park_key(ME, "microsoft-365").ends_with(":connector:microsoft-365"));
    }

    #[tokio::test]
    async fn a_token_inside_the_tool_timeout_is_refreshed_at_the_intent() {
        let (store, vault) = fx();
        node(&store, true, true);
        connect(
            &store,
            &vault,
            ME,
            ConnectionStatus::Connected,
            connectors::now_secs() + 30,
        );
        let calls = Arc::new(AtomicUsize::new(0));
        let r = refresher(&calls, ok);
        let g = precheck_with(&store, &vault, &cfg(), &r, S, ME, T).await;
        assert_eq!(g, ConnectorGate::Clear);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // Now fresh: no second refresh.
        precheck_with(&store, &vault, &cfg(), &r, S, ME, T).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_refused_refresh_at_the_intent_parks_and_tells_the_owner_once() {
        let (store, vault) = fx();
        node(&store, true, true);
        connect(&store, &vault, ME, ConnectionStatus::Connected, 0);
        let calls = Arc::new(AtomicUsize::new(0));
        let r = refresher(&calls, revoked);
        let g = precheck_with(&store, &vault, &cfg(), &r, S, ME, T).await;
        assert!(
            matches!(
                g,
                ConnectorGate::Park {
                    reconnect: true,
                    ..
                }
            ),
            "{g:?}"
        );
        assert_eq!(
            store
                .connector_connection(ME, "microsoft-365")
                .unwrap()
                .unwrap()
                .status,
            ConnectionStatus::NeedsReconnect
        );
        assert_eq!(open_rows(&store, ME), 1);
        assert_eq!(open_rows(&store, OTHER), 0, "never to another user");
        // A second failure while still revoked adds no row, and a check raises no second one.
        let _ = precheck_with(&store, &vault, &cfg(), &r, S, ME, T).await;
        let _ = crate::connectors::check_connection(
            &store,
            &vault,
            &cfg(),
            ME,
            "microsoft-365",
            connectors::now_secs(),
            &*r,
        );
        assert_eq!(open_rows(&store, ME), 1);
        let count: i64 = store
            .connection
            .lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM notifications", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
        // Connecting resolves it.
        store.resolve_connection_needs_reauth(ME, "microsoft-365", "acted");
        assert_eq!(open_rows(&store, ME), 0);
    }

    #[tokio::test]
    async fn an_unreachable_provider_at_the_intent_lets_the_call_through_and_changes_nothing() {
        let (store, vault) = fx();
        node(&store, true, true);
        connect(&store, &vault, ME, ConnectionStatus::Connected, 0);
        let calls = Arc::new(AtomicUsize::new(0));
        let r = refresher(&calls, down);
        let g = precheck_with(&store, &vault, &cfg(), &r, S, ME, T).await;
        assert_eq!(g, ConnectorGate::Clear);
        assert_eq!(
            store
                .connector_connection(ME, "microsoft-365")
                .unwrap()
                .unwrap()
                .status,
            ConnectionStatus::Connected
        );
        assert_eq!(open_rows(&store, ME), 0);
    }

    #[tokio::test]
    async fn a_resumed_intent_does_not_wait_on_the_connect_that_resumed_it() {
        let (store, vault) = fx();
        node(&store, true, true);
        connect(
            &store,
            &vault,
            ME,
            ConnectionStatus::Connected,
            connectors::now_secs() + 30,
        );
        // The connect handler releases its lock before it resumes; a resumed intent that takes
        // the lock for its refresh finishes at once.
        let held = connectors::lock_connection(ME, "microsoft-365").await;
        drop(held);
        let calls = Arc::new(AtomicUsize::new(0));
        let r = refresher(&calls, ok);
        let done = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            precheck_with(&store, &vault, &cfg(), &r, S, ME, T),
        )
        .await;
        assert_eq!(done.unwrap(), ConnectorGate::Clear);
    }

    fn park(store: &SqliteWorkflowStore, request: &str, key: &str) {
        store
            .connection
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO approvals (request_id, run_id, step_id, attempt_id, checkpoint_digest,
                     continuation_generation, published, locked_reason, kind)
                 VALUES (?1, 'run-1', 's', 'a', 'd', 0, 1, ?2, 'reauth')",
                rusqlite::params![request, key],
            )
            .unwrap();
    }

    #[test]
    fn a_park_whose_rest_connection_committed_before_a_restart_is_found_at_startup() {
        let (store, vault) = fx();
        node(&store, true, true);
        assert_eq!(
            parse_park_key(&park_key(ME, "microsoft-365")),
            Some((ME.to_string(), "microsoft-365".to_string()))
        );
        assert_eq!(parse_park_key("reauth:user:me:linear"), None);
        // ME's sign-in committed, then the process stopped before the resume: ME's park waits on a
        // connected connection. OTHER's connection needs a reconnect: still parked. An MCP park is
        // not this pass's.
        connect(
            &store,
            &vault,
            ME,
            ConnectionStatus::Connected,
            connectors::now_secs() + 3600,
        );
        park(&store, "req-me", &park_key(ME, "microsoft-365"));
        connect(
            &store,
            &vault,
            OTHER,
            ConnectionStatus::NeedsReconnect,
            connectors::now_secs() + 3600,
        );
        park(&store, "req-other", &park_key(OTHER, "microsoft-365"));
        park(
            &store,
            "req-mcp",
            &crate::hitl::reauth_key(SecretScope::User, ME, "linear"),
        );
        assert_eq!(connected_park_keys(&store), [park_key(ME, "microsoft-365")]);
        // Resolving it decides ME's park only (the startup pass then resumes the run).
        let decided = crate::api::reauth::resolve_key(&store, &park_key(ME, "microsoft-365"));
        assert_eq!(decided.len(), 1);
        assert!(connected_park_keys(&store).is_empty());
    }

    #[test]
    fn a_park_past_its_deadline_is_not_resumed_at_startup() {
        let (store, vault) = fx();
        node(&store, true, true);
        connect(
            &store,
            &vault,
            ME,
            ConnectionStatus::Connected,
            connectors::now_secs() + 3600,
        );
        // The deadline passed while the node was stopped: the expiry sweep has not run yet.
        park(&store, "req-late", &park_key(ME, "microsoft-365"));
        let set_deadline = |request: &str, at: i64| {
            store
                .connection
                .lock()
                .unwrap()
                .execute(
                    "UPDATE approvals SET expires_at = ?2 WHERE request_id = ?1",
                    rusqlite::params![request, at],
                )
                .unwrap();
        };
        set_deadline("req-late", connectors::now_secs() - 5);
        assert!(connected_park_keys(&store).is_empty());
        assert!(crate::api::reauth::resolve_key(&store, &park_key(ME, "microsoft-365")).is_empty());
        // The decision itself refuses a reconnect past the deadline, however it was selected.
        let late = store.decide_approval_with_surface(
            &vam_spec::v1::ResourceId {
                urn: "req-late".into(),
            },
            &vam_spec::v1::ResourceId { urn: "t".into() },
            crate::hitl::DECISION_RECONNECTED,
            "system",
            "",
            None,
            None,
            None,
            None,
            "system",
        );
        assert!(
            matches!(late, Err(crate::error::OrchestratorError::Conflict(_))),
            "{late:?}"
        );
        let undecided: Option<String> = store
            .connection
            .lock()
            .unwrap()
            .query_row(
                "SELECT decision FROM approvals WHERE request_id = 'req-late'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(undecided, None, "left for the expiry sweep");
        // An unexpired park on the same connection is still recovered.
        park(&store, "req-live", &park_key(ME, "microsoft-365"));
        set_deadline("req-live", connectors::now_secs() + 600);
        assert_eq!(connected_park_keys(&store), [park_key(ME, "microsoft-365")]);
        let decided = crate::api::reauth::resolve_key(&store, &park_key(ME, "microsoft-365"));
        assert_eq!(decided.len(), 1);
        assert_eq!(decided[0].request_id, "req-live");
    }

    #[test]
    fn a_connect_resolves_only_that_owners_parks() {
        use crate::api::reauth::resolve_key;
        let (store, _) = fx();
        // No parks exist: nothing is resolved for either owner, and the keys differ.
        assert!(resolve_key(&store, &park_key(ME, "microsoft-365")).is_empty());
        assert!(resolve_key(&store, &park_key(OTHER, "microsoft-365")).is_empty());
    }

    #[test]
    fn the_notification_has_fixed_text_and_no_token() {
        let (store, _) = fx();
        store.raise_connection_needs_reauth(ME, "microsoft-365", "Microsoft 365");
        let (title, body, action): (String, String, String) = store
            .connection
            .lock()
            .unwrap()
            .query_row(
                "SELECT title, body, action FROM notifications WHERE user_id = ?1",
                [ME],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(title, "Reconnect Microsoft 365 to continue");
        assert!(body.contains("Microsoft 365"));
        assert!(action.contains("\"connector\"") && action.contains("microsoft-365"));
        for t in ["token", "http", "Bearer"] {
            assert!(!title.contains(t) && !body.contains(t), "{t}");
        }
    }

    // ---- an API key connector (CN-7.6) -----------------------------------------------------------

    const NURN: &str = "urn:vam:connector:connectors.community.example/community/notesdesk";
    const NT: &str = "note_search";
    const NKEY: &str = "ndk_live_0123456789abcdef";

    /// An installed, enabled `notesdesk` and a tool that requires it.
    fn notesdesk_world() -> (Arc<SqliteWorkflowStore>, Arc<SecretsVault>) {
        let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
        let vault = SecretsVault::for_tests(store.clone());
        crate::connector_client::test_support::install_api_key_rows(&store);
        store
            .record_tool_egress(
                S,
                &[ToolEgressRow {
                    tool: NT.into(),
                    requires_connector: Some(NURN.into()),
                    shapes: vec![],
                    effect_class: None,
                }],
            )
            .unwrap();
        store
            .put_node_connector(&NodeConnector {
                tenant_subdomain: None,
                connector_id: "notesdesk".into(),
                enabled: true,
                consent_tenant_id: None,
                consent_recorded_at: None,
                consent_recorded_by: None,
                updated_at: 1,
                updated_by: "olga".into(),
            })
            .unwrap();
        (store, vault)
    }

    fn enter_key(store: &SqliteWorkflowStore, vault: &SecretsVault, owner: &str, hosts: &[&str]) {
        let hosts: Vec<String> = hosts.iter().map(|h| h.to_string()).collect();
        store
            .commit_api_key_connection(
                vault,
                &connectors::ApiKeyCommit {
                    owner,
                    connector_id: "notesdesk",
                    address: "",
                    hosts: &hosts,
                    api_key: NKEY,
                    now: 1,
                },
                &|_| Ok(()),
            )
            .unwrap();
    }

    #[test]
    fn the_park_words_are_connect_or_reconnect_name_to_continue() {
        assert_eq!(
            park_message(false, "Notesdesk"),
            "Connect Notesdesk to continue"
        );
        assert_eq!(
            park_message(true, "Notesdesk"),
            "Reconnect Notesdesk to continue"
        );
        let (store, vault) = notesdesk_world();
        // Not connected: the gate parks on the connector, and its name makes the words above.
        let ConnectorGate::Park {
            connector_id,
            name,
            reconnect,
        } = gate_for(&store, S, ME, NT)
        else {
            panic!("parked");
        };
        assert_eq!((connector_id.as_str(), reconnect), ("notesdesk", false));
        assert_eq!(
            park_message(reconnect, &name),
            "Connect Notesdesk to continue"
        );
        // Connected with the key: clear. Another member's state is their own.
        enter_key(&store, &vault, ME, &["api.notesdesk.example"]);
        assert_eq!(gate_for(&store, S, ME, NT), ConnectorGate::Clear);
        assert!(matches!(
            gate_for(&store, S, OTHER, NT),
            ConnectorGate::Park {
                reconnect: false,
                ..
            }
        ));
        // A key whose pin is not the connector's hosts now asks to Reconnect.
        enter_key(&store, &vault, ME, &["old.notesdesk.example"]);
        let ConnectorGate::Park {
            name, reconnect, ..
        } = gate_for(&store, S, ME, NT)
        else {
            panic!("parked");
        };
        assert!(reconnect);
        assert_eq!(
            park_message(reconnect, &name),
            "Reconnect Notesdesk to continue"
        );
        // A key row that is gone does too; the connection row alone is not enough.
        vault
            .delete_secret_at(
                crate::secrets::SecretScope::User,
                ME,
                &connectors::api_key_binding_id("notesdesk"),
            )
            .unwrap();
        assert!(matches!(
            gate_for(&store, S, ME, NT),
            ConnectorGate::Park {
                reconnect: true,
                ..
            }
        ));
        // A disabled connector is refused with no park.
        store
            .put_node_connector(&NodeConnector {
                tenant_subdomain: None,
                connector_id: "notesdesk".into(),
                enabled: false,
                consent_tenant_id: None,
                consent_recorded_at: None,
                consent_recorded_by: None,
                updated_at: 2,
                updated_by: "olga".into(),
            })
            .unwrap();
        assert!(matches!(
            gate_for(&store, S, ME, NT),
            ConnectorGate::Refuse(t) if t.contains("ask your admin")
        ));
    }

    #[tokio::test]
    async fn the_intent_never_calls_a_refresher_for_an_api_key() {
        let (store, vault) = notesdesk_world();
        enter_key(&store, &vault, ME, &["api.notesdesk.example"]);
        // Even a row that looks long expired is not refreshed: a key has nothing to renew.
        let mut row = store
            .connector_connection(ME, "notesdesk")
            .unwrap()
            .unwrap();
        row.expires_at = Some(1);
        store.put_connector_connection(&row).unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let r = refresher(&calls, ok);
        let (g, refreshed) = precheck_with_outcome(&store, &vault, &cfg(), &r, S, ME, NT).await;
        assert_eq!(g, ConnectorGate::Clear);
        assert!(!refreshed);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        // Not connected: parked, with no refresher call either.
        let (g, _) = precheck_with_outcome(&store, &vault, &cfg(), &r, S, OTHER, NT).await;
        assert!(matches!(
            g,
            ConnectorGate::Park {
                reconnect: false,
                ..
            }
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }
}
