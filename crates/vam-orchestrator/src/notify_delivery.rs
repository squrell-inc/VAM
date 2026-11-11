//! The delivery worker (NT-2.3) and the neutral rendering (NT-2.10, `vamd` side).
//!
//! `vamd` owns the **intent and the state** of a delivery (`notification_deliveries`): which
//! record goes to which verified contact over which channel, when, how often it was tried and why
//! it stopped. The broker owns every vendor fact (the Slack message id, the DM channel): an update
//! names the original `delivery_id` in `update_of` and the broker maps it. **No message id, no
//! address, no token and no handle is stored here** (a handle is minted at send time into
//! `api_keys` as a hash, and its token rides in `DeliverNotification.action_handle` for one call); the contact is a `ct_` id, the address is read
//! from `contacts` at send time, the bot token is read from the vault at send time and rides in
//! `resolved_secrets` for one call.
//!
//! Rows are created inside the transaction that writes or resolves the record
//! ([`on_recorded`], [`on_resolved`]; called from `notifications::record`, `resolve` and
//! `resolve_for_run`), from the user's preferences, the node's settings, the user's **verified,
//! unrevoked** contacts and the approved channel broker. A failed record write rolls them back.
//!
//! The worker ([`run_once`], looped by `vamd`) sends due rows at least once; `delivery_id` (the row
//! id) is the broker's idempotency key.
//!
//! Rules:
//! - quiet hours hold every external delivery (approvals too) except `always` kinds; held rows go
//!   out at the end, one message when there are several;
//! - at most [`RATE_PER_HOUR`] immediate posts per user and channel per hour; the rest are held and
//!   fold into one "and N more" message;
//! - when a record resolves, every `sent` post gets an `update` row ("Decided by ... in ... at ...",
//!   no actions) and every unsent post is suppressed;
//! - retries at 30 s, 2 m, 10 m, 30 m, 1 h, then hourly, until 24 h after the row was created.

use std::collections::BTreeMap;
use std::time::Duration;

use prost_types::Struct;
use rusqlite::{params, Connection, OptionalExtension};
use secrecy::ExposeSecret;
use serde_json::{json, Value};
use vam_spec::encoding::json_to_proto;
use vam_spec::v1::{
    ActionStyle, AgentText, CapabilityManifest, DeliverNotificationRequest,
    DeliverNotificationResponse, DeliverOutcome, NotificationRendering, RenderedAction,
};

use crate::error::OrchestratorError;
use crate::hsp::HspEndpoint;
use crate::node_notify::{NodeNotifySettings, CHANNELS};
use crate::notification_prefs::Prefs;
use crate::notifications::{Kind, Severity};
use crate::secrets::SecretsVault;
use crate::spaces::mint_id;
use crate::workflow::SqliteWorkflowStore;

/// `vam_notify_deliveries_total{channel, state}`.
pub const METRIC_DELIVERIES_TOTAL: &str = "vam_notify_deliveries_total";
/// Immediate posts per user and channel per hour; the rest fold.
pub const RATE_PER_HOUR: i64 = 30;
/// Waits after the 1st, 2nd, ... failed attempt; later ones wait [`RETRY_TAIL_SECS`].
pub const RETRY_SECS: [i64; 5] = [30, 120, 600, 1800, 3600];
pub const RETRY_TAIL_SECS: i64 = 3600;
/// A row still unsent this long after it was created is `failed`.
pub const GIVE_UP_SECS: i64 = 86_400;
/// Agent-written text in a rendering (open question 5).
pub const AGENT_TEXT_MAX_BYTES: usize = 1024;
/// Sections in a folded message before "and N more".
pub const SUMMARY_MAX_SECTIONS: usize = 10;
/// A Select with more options than this is not native in Slack.
pub const SLACK_MAX_SELECT_OPTIONS: usize = 100;
/// The jig primitives the Slack adapter renders (open question and NT-2.10). Not `NumberInput`,
/// `FileInput`, `ProgressBar`.
pub const SLACK_PRIMITIVES: [&str; 11] = [
    "Stack",
    "Text",
    "Badge",
    "KeyValue",
    "Button",
    "Input",
    "TextArea",
    "Select",
    "MultiSelect",
    "Toggle",
    "DateInput",
];
/// The key of the bot token in `resolved_secrets` of `DeliverNotification`.
pub const BOT_TOKEN_KEY: &str = "bot_token";
/// Error codes that end a row at once (open question 4).
const PERMANENT_CODES: [&str; 3] = ["user_not_found", "account_inactive", "invalid_auth"];
const SEND_TIMEOUT: Duration = Duration::from_secs(10);
const BATCH: usize = 50;
const DECISION_KINDS_SQL: &str = "('approval.pending', 'question.asked', 'gate.opened')";

fn now() -> i64 {
    crate::catalog::unix_timestamp()
}

pub(crate) fn initialize_schema(connection: &Connection) -> Result<(), OrchestratorError> {
    connection.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS notification_deliveries (
            id TEXT PRIMARY KEY NOT NULL,
            notification_id TEXT NOT NULL,
            user_id TEXT NOT NULL,
            contact_id TEXT NOT NULL,
            channel TEXT NOT NULL,
            kind TEXT NOT NULL CHECK (kind IN ('post', 'update', 'summary')),
            update_of TEXT,
            folded_into TEXT,
            state TEXT NOT NULL
                CHECK (state IN ('pending', 'sent', 'held', 'failed', 'suppressed')),
            hold TEXT CHECK (hold IS NULL OR hold IN ('quiet', 'rate')),
            attempts INTEGER NOT NULL DEFAULT 0,
            not_before INTEGER NOT NULL,
            last_error TEXT,
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL,
            sent_at INTEGER,
            refresh INTEGER NOT NULL DEFAULT 0
        );
        CREATE INDEX IF NOT EXISTS idx_notification_deliveries_due
            ON notification_deliveries(state, not_before);
        CREATE INDEX IF NOT EXISTS idx_notification_deliveries_record
            ON notification_deliveries(notification_id);
        CREATE INDEX IF NOT EXISTS idx_notification_deliveries_user
            ON notification_deliveries(user_id, channel, created_at);
        ",
    )?;
    // `refresh` (NT-2.4): an edit of a sent decision message that carries the actions and a new
    // handle, queued when the channel broker restarts. Tables from earlier waves gain it here.
    let has_refresh = connection
        .prepare(
            "SELECT 1 FROM pragma_table_info('notification_deliveries') WHERE name = 'refresh'",
        )?
        .exists([])?;
    if !has_refresh {
        connection.execute(
            "ALTER TABLE notification_deliveries ADD COLUMN refresh INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    Ok(())
}

// ---- who may receive -------------------------------------------------------------------------

/// The id of the approved, enabled broker whose manifest declares `notification_channel` for
/// `channel` on a tier that may deliver (core or first-party).
pub fn find_channel_broker(
    conn: &Connection,
    channel: &str,
) -> Result<Option<String>, OrchestratorError> {
    Ok(find_channel_broker_surface(conn, channel)?.map(|(id, _)| id))
}

/// Like [`find_channel_broker`], with the surface the broker's approved manifest declares: the
/// only place a handle's surface comes from.
pub fn find_channel_broker_surface(
    conn: &Connection,
    channel: &str,
) -> Result<Option<(String, String)>, OrchestratorError> {
    let mut stmt = conn.prepare(
        "SELECT broker_id, manifest_json FROM hsp_broker_approvals
         WHERE enabled = 1 ORDER BY broker_id",
    )?;
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
        .collect::<Result<Vec<_>, _>>()?;
    for (broker_id, json) in rows {
        let Ok(manifest) = json_to_proto::<CapabilityManifest>("vam.v1.CapabilityManifest", &json)
        else {
            continue;
        };
        if !crate::notify_gateway::tier_may_deliver(manifest.trust_tier) {
            continue;
        }
        if let Some(c) = manifest
            .notification_channel
            .as_ref()
            .filter(|c| c.channel == channel)
        {
            return Ok(Some((broker_id, c.surface.clone())));
        }
    }
    Ok(None)
}

fn load_node(conn: &Connection) -> Result<NodeNotifySettings, OrchestratorError> {
    let json: Option<String> = conn
        .query_row(
            "SELECT settings_json FROM node_notify_settings WHERE id = 1",
            [],
            |r| r.get(0),
        )
        .optional()?;
    match json {
        None => Ok(NodeNotifySettings::default()),
        Some(j) => serde_json::from_str(&j).map_err(|e| OrchestratorError::Core(e.to_string())),
    }
}

fn load_prefs(conn: &Connection, user_id: &str) -> Result<Prefs, OrchestratorError> {
    let json: Option<String> = conn
        .query_row(
            "SELECT prefs_json FROM notification_prefs WHERE user_id = ?1",
            [user_id],
            |r| r.get(0),
        )
        .optional()?;
    match json {
        None => Ok(Prefs::default()),
        Some(j) => serde_json::from_str(&j).map_err(|e| OrchestratorError::Core(e.to_string())),
    }
}

/// The user's verified, unrevoked contact on `channel`.
fn verified_contact_id(
    conn: &Connection,
    user_id: &str,
    channel: &str,
) -> Result<Option<String>, OrchestratorError> {
    Ok(conn
        .query_row(
            "SELECT id FROM contacts WHERE user_id = ?1 AND channel = ?2
               AND verified_at IS NOT NULL AND revoked_at IS NULL",
            params![user_id, channel],
            |r| r.get(0),
        )
        .optional()?)
}

fn severity_of(s: &str) -> Severity {
    match s {
        "action" => Severity::Action,
        "warn" => Severity::Warn,
        _ => Severity::Info,
    }
}

// ---- creating rows ---------------------------------------------------------------------------

/// Delivery rows for a record just inserted on `tx`. Returns how many were created.
pub fn on_recorded(tx: &Connection, notification_id: &str) -> Result<usize, OrchestratorError> {
    on_recorded_at(tx, notification_id, now())
}

pub fn on_recorded_at(
    tx: &Connection,
    notification_id: &str,
    now: i64,
) -> Result<usize, OrchestratorError> {
    let row: Option<(String, String, String)> = tx
        .query_row(
            "SELECT user_id, kind, severity FROM notifications WHERE id = ?1",
            [notification_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let Some((user_id, kind, severity)) = row else {
        return Ok(0);
    };
    let Some(kind) = Kind::parse(&kind) else {
        return Ok(0);
    };
    let severity = severity_of(&severity);
    let node = load_node(tx)?;
    let mut created = 0;
    for channel in CHANNELS {
        // Enabled adapter, an approved broker that declares the channel, a verified contact.
        if !node.channel_enabled(channel) {
            continue;
        }
        if find_channel_broker(tx, channel)?.is_none() {
            continue;
        }
        let Some(contact_id) = verified_contact_id(tx, &user_id, channel)? else {
            continue;
        };
        let prefs = load_prefs(tx, &user_id)?;
        if !prefs.wants_external(&node, kind, severity) {
            continue;
        }
        let (state, hold, not_before) = if let Some(end) = prefs
            .held_by_quiet_hours(kind, now)
            .then(|| prefs.quiet_hours_end(now))
            .flatten()
        {
            ("held", Some("quiet"), end)
        } else if let Some(free_at) = rate_free_at(tx, &user_id, channel, now)? {
            ("held", Some("rate"), free_at)
        } else {
            ("pending", None, now)
        };
        tx.execute(
            "INSERT INTO notification_deliveries
               (id, notification_id, user_id, contact_id, channel, kind, state, hold,
                attempts, not_before, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 'post', ?6, ?7, 0, ?8, ?9, ?9)",
            params![
                mint_id("nd_"),
                notification_id,
                user_id,
                contact_id,
                channel,
                state,
                hold,
                not_before,
                now
            ],
        )?;
        created += 1;
    }
    Ok(created)
}

/// `Some(when)` when the user already has [`RATE_PER_HOUR`] immediate posts in the last hour: the
/// moment the oldest of them leaves the window.
fn rate_free_at(
    conn: &Connection,
    user_id: &str,
    channel: &str,
    now: i64,
) -> Result<Option<i64>, OrchestratorError> {
    let mut stmt = conn.prepare(
        "SELECT created_at FROM notification_deliveries
         WHERE user_id = ?1 AND channel = ?2 AND kind = 'post' AND hold IS NULL
           AND state <> 'suppressed' AND created_at > ?3
         ORDER BY created_at DESC LIMIT ?4",
    )?;
    let times = stmt
        .query_map(params![user_id, channel, now - 3600, RATE_PER_HOUR], |r| {
            r.get::<_, i64>(0)
        })?
        .collect::<Result<Vec<_>, _>>()?;
    if (times.len() as i64) < RATE_PER_HOUR {
        return Ok(None);
    }
    let oldest = times.last().copied().unwrap_or(now);
    Ok(Some((oldest + 3600).max(now + 1)))
}

/// What a resolve hook names.
pub enum Resolved<'a> {
    Approval(&'a str),
    Run(&'a str),
    Notification(&'a str),
}

/// A record was resolved on `tx`: unsent posts of it are suppressed, and every sent post gets one
/// update row. Idempotent.
pub fn on_resolved(tx: &Connection, which: Resolved<'_>) -> Result<usize, OrchestratorError> {
    on_resolved_at(tx, which, now())
}

pub fn on_resolved_at(
    tx: &Connection,
    which: Resolved<'_>,
    now: i64,
) -> Result<usize, OrchestratorError> {
    let (column, key) = match which {
        Resolved::Approval(a) => ("approval_id", a),
        Resolved::Run(r) => ("run_id", r),
        Resolved::Notification(n) => ("id", n),
    };
    let ids = {
        let mut stmt = tx.prepare(&format!(
            "SELECT id FROM notifications
             WHERE {column} = ?1 AND resolved_at IS NOT NULL AND kind IN {DECISION_KINDS_SQL}"
        ))?;
        let rows = stmt
            .query_map([key], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        rows
    };
    let mut updates = 0;
    for id in ids {
        // The record is resolved, by anyone from anywhere: every action handle for it is spent,
        // in this same transaction (a handle that decided is used up with its decision).
        crate::credentials::revoke_handles_for_notification(tx, &id, now)?;
        tx.execute(
            "UPDATE notification_deliveries
             SET state = 'suppressed', last_error = 'resolved', updated_at = ?2
             WHERE notification_id = ?1
               AND ((kind = 'post' AND state IN ('pending', 'held'))
                    OR (refresh = 1 AND state IN ('pending', 'held')))",
            params![id, now],
        )?;
        let sent = {
            let mut stmt = tx.prepare(
                "SELECT d.id, d.user_id, d.contact_id, d.channel FROM notification_deliveries d
                 WHERE d.notification_id = ?1 AND d.kind = 'post' AND d.state = 'sent'
                   AND NOT EXISTS (SELECT 1 FROM notification_deliveries u
                                   WHERE u.update_of = d.id AND u.refresh = 0)",
            )?;
            let rows = stmt
                .query_map([&id], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            rows
        };
        for (post, user_id, contact_id, channel) in sent {
            // An update is an edit of a message already shown: quiet hours do not hold it.
            tx.execute(
                "INSERT INTO notification_deliveries
                   (id, notification_id, user_id, contact_id, channel, kind, update_of, state,
                    attempts, not_before, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'update', ?6, 'pending', 0, ?7, ?7, ?7)",
                params![mint_id("nd_"), id, user_id, contact_id, channel, post, now],
            )?;
            updates += 1;
        }
    }
    Ok(updates)
}

// ---- the worker ------------------------------------------------------------------------------

/// A row the worker is about to send.
#[derive(Debug, Clone)]
pub struct Due {
    pub id: String,
    pub notification_id: String,
    pub user_id: String,
    pub contact_id: String,
    pub channel: String,
    pub kind: String,
    pub update_of: Option<String>,
    pub attempts: i64,
    pub created_at: i64,
}

/// Pending rows whose time has come, oldest first.
pub fn due_rows(
    store: &SqliteWorkflowStore,
    now: i64,
    limit: usize,
) -> Result<Vec<Due>, OrchestratorError> {
    let conn = store
        .connection
        .lock()
        .map_err(|_| OrchestratorError::StorageUnavailable)?;
    let mut stmt = conn.prepare(
        "SELECT id, notification_id, user_id, contact_id, channel, kind, update_of, attempts,
                created_at
         FROM notification_deliveries
         WHERE state = 'pending' AND not_before <= ?1
         ORDER BY not_before, created_at, id LIMIT ?2",
    )?;
    let rows = stmt
        .query_map(params![now, limit as i64], |r| {
            Ok(Due {
                id: r.get(0)?,
                notification_id: r.get(1)?,
                user_id: r.get(2)?,
                contact_id: r.get(3)?,
                channel: r.get(4)?,
                kind: r.get(5)?,
                update_of: r.get(6)?,
                attempts: r.get(7)?,
                created_at: r.get(8)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Held rows whose time has come become `pending`, as themselves when alone and as one summary
/// message when there are several for a user and channel. A held decision whose record has
/// resolved is suppressed; a quiet-hours row whose window was extended waits again. Returns the
/// number of rows moved.
pub fn release_held(store: &SqliteWorkflowStore, now: i64) -> Result<usize, OrchestratorError> {
    let conn = store
        .connection
        .lock()
        .map_err(|_| OrchestratorError::StorageUnavailable)?;
    let tx = conn.unchecked_transaction()?;
    struct Held {
        id: String,
        notification_id: String,
        user_id: String,
        contact_id: String,
        channel: String,
        hold: Option<String>,
    }
    let held = {
        let mut stmt = tx.prepare(
            "SELECT id, notification_id, user_id, contact_id, channel, hold
             FROM notification_deliveries
             WHERE state = 'held' AND kind = 'post' AND not_before <= ?1
             ORDER BY created_at, id",
        )?;
        let rows = stmt
            .query_map([now], |r| {
                Ok(Held {
                    id: r.get(0)?,
                    notification_id: r.get(1)?,
                    user_id: r.get(2)?,
                    contact_id: r.get(3)?,
                    channel: r.get(4)?,
                    hold: r.get(5)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        rows
    };
    let mut moved = 0;
    let mut groups: BTreeMap<(String, String), Vec<Held>> = BTreeMap::new();
    for h in held {
        let facts = load_facts(&tx, &h.notification_id)?;
        let Some(facts) = facts else {
            tx.execute(
                "UPDATE notification_deliveries SET state = 'suppressed',
                    last_error = 'record_gone', updated_at = ?2 WHERE id = ?1",
                params![h.id, now],
            )?;
            moved += 1;
            continue;
        };
        if facts.kind.is_decision() && facts.resolved_at.is_some() {
            tx.execute(
                "UPDATE notification_deliveries SET state = 'suppressed',
                    last_error = 'resolved', updated_at = ?2 WHERE id = ?1",
                params![h.id, now],
            )?;
            moved += 1;
            continue;
        }
        if h.hold.as_deref() == Some("quiet") {
            let prefs = load_prefs(&tx, &h.user_id)?;
            if let Some(end) = prefs
                .held_by_quiet_hours(facts.kind, now)
                .then(|| prefs.quiet_hours_end(now))
                .flatten()
            {
                tx.execute(
                    "UPDATE notification_deliveries SET not_before = ?2, updated_at = ?3
                     WHERE id = ?1",
                    params![h.id, end, now],
                )?;
                continue;
            }
        }
        groups
            .entry((h.user_id.clone(), h.channel.clone()))
            .or_default()
            .push(h);
    }
    for (_, rows) in groups {
        if rows.len() == 1 {
            tx.execute(
                "UPDATE notification_deliveries SET state = 'pending', hold = NULL,
                    not_before = ?2, updated_at = ?2 WHERE id = ?1",
                params![rows[0].id, now],
            )?;
            moved += 1;
            continue;
        }
        let first = &rows[0];
        let summary = mint_id("nd_");
        tx.execute(
            "INSERT INTO notification_deliveries
               (id, notification_id, user_id, contact_id, channel, kind, state, attempts,
                not_before, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 'summary', 'pending', 0, ?6, ?6, ?6)",
            params![
                summary,
                first.notification_id,
                first.user_id,
                first.contact_id,
                first.channel,
                now
            ],
        )?;
        for r in &rows {
            tx.execute(
                "UPDATE notification_deliveries SET state = 'suppressed', last_error = 'folded',
                    folded_into = ?2, updated_at = ?3 WHERE id = ?1",
                params![r.id, summary, now],
            )?;
            moved += 1;
        }
    }
    tx.commit()?;
    Ok(moved)
}

/// What the worker does with one due row.
pub enum Prepared {
    Send {
        endpoint: HspEndpoint,
        request: Box<DeliverNotificationRequest>,
    },
    /// Never send it (the record resolved, the contact was unlinked, the adapter was switched off).
    Suppress(&'static str),
    /// Cannot send now; try again on the schedule.
    Retry(&'static str),
    /// Cannot ever be sent.
    Fail(&'static str),
}

impl std::fmt::Debug for Prepared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never the request: it carries the bot token.
        match self {
            Prepared::Send { .. } => f.write_str("Send"),
            Prepared::Suppress(c) => write!(f, "Suppress({c})"),
            Prepared::Retry(c) => write!(f, "Retry({c})"),
            Prepared::Fail(c) => write!(f, "Fail({c})"),
        }
    }
}

/// Reads everything a send needs, at send time: the record, the contact's address, the adapter's
/// bot token (from the vault, for this call only), the broker's endpoint. Builds the rendering.
pub fn prepare(store: &SqliteWorkflowStore, vault: &SecretsVault, due: &Due, now: i64) -> Prepared {
    match prepare_inner(store, vault, due, now) {
        Ok(p) => p,
        Err(_) => Prepared::Retry("storage"),
    }
}

fn prepare_inner(
    store: &SqliteWorkflowStore,
    vault: &SecretsVault,
    due: &Due,
    _now: i64,
) -> Result<Prepared, OrchestratorError> {
    let (facts, approval, address, node, broker_id, surface, display, members, refresh) = {
        let conn = store
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let Some(facts) = load_facts(&conn, &due.notification_id)? else {
            return Ok(Prepared::Suppress("record_gone"));
        };
        if due.kind == "post" && facts.kind.is_decision() && facts.resolved_at.is_some() {
            return Ok(Prepared::Suppress("resolved"));
        }
        let address: Option<String> = conn
            .query_row(
                "SELECT address FROM contacts WHERE id = ?1 AND verified_at IS NOT NULL
                   AND revoked_at IS NULL",
                [&due.contact_id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(address) = address else {
            return Ok(Prepared::Suppress("contact_gone"));
        };
        let node = load_node(&conn)?;
        if !node.channel_enabled(&due.channel) {
            return Ok(Prepared::Suppress("adapter_disabled"));
        }
        let Some((broker_id, surface)) = find_channel_broker_surface(&conn, &due.channel)? else {
            return Ok(Prepared::Suppress("no_broker"));
        };
        let refresh = due.kind == "update"
            && conn
                .query_row(
                    "SELECT refresh FROM notification_deliveries WHERE id = ?1",
                    [&due.id],
                    |r| r.get::<_, i64>(0),
                )
                .optional()?
                == Some(1);
        let approval = match &facts.approval_id {
            Some(a) => load_approval(&conn, a)?,
            None => None,
        };
        let display: String = conn
            .query_row(
                "SELECT username FROM users WHERE user_id = ?1",
                [facts.acted_by.as_deref().unwrap_or("")],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or_else(|| facts.acted_by.clone().unwrap_or_default());
        let members = if due.kind == "summary" {
            summary_members(&conn, &due.id)?
        } else {
            Vec::new()
        };
        (
            facts, approval, address, node, broker_id, surface, display, members, refresh,
        )
    };
    let Some(endpoint) = store.get_hsp_broker_endpoint(&broker_id)? else {
        return Ok(Prepared::Retry("broker_unavailable"));
    };
    let bot_ref = match due.channel.as_str() {
        "slack" => node.slack.bot_token_ref.clone(),
        _ => None,
    };
    let Some(bot_ref) = bot_ref else {
        return Ok(Prepared::Fail("no_token_ref"));
    };
    // The vault is read here, per call. The value goes into this request and nowhere else.
    let token = match vault.decrypt_secret_not_connection(&bot_ref) {
        Ok(t) => t,
        Err(OrchestratorError::NotFound(_)) => return Ok(Prepared::Retry("secret_missing")),
        // An MCP connection credential is never a channel token. Retrying cannot fix it.
        Err(OrchestratorError::PermissionDenied(_)) => {
            return Ok(Prepared::Fail("secret_not_permitted"))
        }
        Err(_) => return Ok(Prepared::Retry("secret_unreadable")),
    };
    let app_url = node.app_url.as_deref();
    // A refresh of a record that is still waiting is the full message again (with its actions
    // and a new handle); once the record resolves it is the ordinary "Decided by" edit.
    let refresh = refresh && facts.resolved_at.is_none();
    let (rendering, kind, update_of) = match due.kind.as_str() {
        "update" if refresh => {
            let Some(original) = due.update_of.clone() else {
                return Ok(Prepared::Fail("no_original"));
            };
            (
                build_rendering(&facts, approval.as_ref(), app_url),
                facts.kind.as_str().to_string(),
                original,
            )
        }
        "update" => {
            let Some(original) = due.update_of.clone() else {
                return Ok(Prepared::Fail("no_original"));
            };
            (
                build_update(&facts, &display, app_url),
                facts.kind.as_str().to_string(),
                original,
            )
        }
        "summary" => (
            build_summary(&members, app_url),
            "notifications.summary".to_string(),
            String::new(),
        ),
        _ => (
            build_rendering(&facts, approval.as_ref(), app_url),
            facts.kind.as_str().to_string(),
            String::new(),
        ),
    };
    // A decision message that offers actions carries a single-use handle, minted now (and the
    // record is re-checked unresolved in the same transaction).
    let action_handle = if (due.kind == "post" || refresh) && !rendering.actions.is_empty() {
        match mint_handle(store, due, &facts, &broker_id, &surface, &rendering)? {
            Ok(h) => h,
            Err(why) => {
                tracing::warn!(delivery = %due.id, why, "no action handle for a decision post; not sent");
                return Ok(Prepared::Suppress(why));
            }
        }
    } else {
        String::new()
    };
    let mut secrets = std::collections::HashMap::new();
    secrets.insert(BOT_TOKEN_KEY.to_string(), token.expose_secret().clone());
    Ok(Prepared::Send {
        endpoint,
        request: Box::new(DeliverNotificationRequest {
            delivery_id: due.id.clone(),
            channel: due.channel.clone(),
            channel_address: address,
            notification_id: due.notification_id.clone(),
            kind,
            rendering: Some(rendering),
            action_handle,
            update_of,
            resolved_secrets: secrets,
            trace_context: None,
        }),
    })
}

/// How long a handle lives when its approval has no deadline.
pub const HANDLE_FALLBACK_TTL_SECS: i64 = 7 * 86_400;

/// The decisions a rendering offers, as a handle's allowed set: a gate option's id, else the
/// decision word. (The broker sends the same words back.)
pub fn offered_decisions(r: &NotificationRendering) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for a in &r.actions {
        let word = if a.option.is_empty() {
            &a.decision
        } else {
            &a.option
        };
        if !word.is_empty() && !out.contains(word) {
            out.push(word.clone());
        }
    }
    out
}

/// Mints the handle of one delivery. `Err(code)` when nothing may be minted (the record or its approval resolved meanwhile,
/// the contact is gone, the approval expired). Bound to the record, the approval, the user, the contact, the channel,
/// the offered decisions and the surface of the broker's approved manifest; expiring with the
/// approval. Earlier open handles of the same record and contact are revoked.
fn mint_handle(
    store: &SqliteWorkflowStore,
    due: &Due,
    facts: &Facts,
    broker_id: &str,
    surface: &str,
    rendering: &NotificationRendering,
) -> Result<Result<String, &'static str>, OrchestratorError> {
    let Some(approval_id) = facts.approval_id.as_deref() else {
        return Ok(Err("no_approval"));
    };
    let decisions = offered_decisions(rendering);
    if decisions.is_empty() {
        return Ok(Err("no_actions"));
    }
    let now = now();
    let conn = store
        .connection
        .lock()
        .map_err(|_| OrchestratorError::StorageUnavailable)?;
    let tx = conn.unchecked_transaction()?;
    let open: Option<(Option<i64>, Option<String>)> = tx
        .query_row(
            "SELECT a.expires_at, a.decision FROM approvals a
               JOIN notifications n ON n.approval_id = a.request_id
             WHERE n.id = ?1 AND n.resolved_at IS NULL AND a.request_id = ?2",
            params![due.notification_id, approval_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((expires_at, decision)) = open else {
        return Ok(Err("resolved"));
    };
    if decision.is_some() {
        return Ok(Err("resolved"));
    }
    let live_contact: bool = tx
        .query_row(
            "SELECT 1 FROM contacts WHERE id = ?1 AND user_id = ?2 AND verified_at IS NOT NULL
               AND revoked_at IS NULL",
            params![due.contact_id, due.user_id],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if !live_contact {
        return Ok(Err("contact_gone"));
    }
    let expires_at = expires_at.unwrap_or(now + HANDLE_FALLBACK_TTL_SECS);
    if expires_at <= now {
        return Ok(Err("approval_expired"));
    }
    let token = crate::credentials::mint_action_handle(
        &tx,
        &crate::credentials::HandleMint {
            broker_id,
            notification_id: &due.notification_id,
            approval_id,
            user_id: &due.user_id,
            contact_id: &due.contact_id,
            channel: &due.channel,
            decisions: &decisions,
            surface,
            expires_at,
        },
    )?;
    tx.commit()?;
    Ok(Ok(token))
}

/// The channel broker asked for its config (it started or reconnected): the handles it held in
/// memory are gone. Revokes every open handle of that broker and queues, for each decision it
/// already sent that is still waiting, an edit of the sent message that carries the actions and
/// a new handle (`refresh`). Runs on the caller's transaction. Returns how many were queued.
pub fn requeue_after_broker_start(
    tx: &Connection,
    broker_id: &str,
    channel: &str,
    now: i64,
) -> Result<usize, OrchestratorError> {
    crate::credentials::revoke_handles_for_broker(tx, broker_id, now)?;
    let sent = {
        let mut stmt = tx.prepare(&format!(
            "SELECT d.id, d.notification_id, d.user_id, d.contact_id
             FROM notification_deliveries d
               JOIN notifications n ON n.id = d.notification_id
             WHERE d.channel = ?1 AND d.kind = 'post' AND d.state = 'sent'
               AND n.resolved_at IS NULL AND n.kind IN {DECISION_KINDS_SQL}
               AND NOT EXISTS (SELECT 1 FROM notification_deliveries r
                               WHERE r.update_of = d.id AND r.refresh = 1
                                 AND r.state IN ('pending', 'held'))
             ORDER BY d.created_at, d.id"
        ))?;
        let rows = stmt
            .query_map([channel], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        rows
    };
    let mut queued = 0;
    for (post, notification_id, user_id, contact_id) in sent {
        tx.execute(
            "INSERT INTO notification_deliveries
               (id, notification_id, user_id, contact_id, channel, kind, update_of, state,
                attempts, not_before, created_at, updated_at, refresh)
             VALUES (?1, ?2, ?3, ?4, ?5, 'update', ?6, 'pending', 0, ?7, ?7, ?7, 1)",
            params![
                mint_id("nd_"),
                notification_id,
                user_id,
                contact_id,
                channel,
                post,
                now
            ],
        )?;
        queued += 1;
    }
    Ok(queued)
}

fn code_of(s: &str) -> String {
    let c: String = s
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .take(64)
        .collect();
    if c.is_empty() {
        "unspecified".into()
    } else {
        c
    }
}

/// Records what became of a send: sent, or the next retry, or `failed`.
pub fn finish(
    store: &SqliteWorkflowStore,
    due: &Due,
    result: Result<DeliverNotificationResponse, OrchestratorError>,
    now: i64,
) -> Result<&'static str, OrchestratorError> {
    let attempts = due.attempts + 1;
    let (state, code, retry_after_secs): (&'static str, String, i64) = match result {
        Ok(r) => match DeliverOutcome::try_from(r.outcome) {
            Ok(DeliverOutcome::Sent | DeliverOutcome::Duplicate) => ("sent", String::new(), 0),
            Ok(DeliverOutcome::PermanentFailure) => ("failed", code_of(&r.error_code), 0),
            Ok(DeliverOutcome::Unsupported) => (
                "failed",
                if r.error_code.is_empty() {
                    "unsupported".into()
                } else {
                    code_of(&r.error_code)
                },
                0,
            ),
            Ok(DeliverOutcome::Retry) => {
                if PERMANENT_CODES.contains(&r.error_code.as_str()) {
                    ("failed", code_of(&r.error_code), 0)
                } else {
                    (
                        "retry",
                        if r.error_code.is_empty() {
                            "retry".into()
                        } else {
                            code_of(&r.error_code)
                        },
                        i64::from(r.retry_after_ms.max(0)) / 1000,
                    )
                }
            }
            _ => ("retry", "bad_outcome".into(), 0),
        },
        Err(_) => ("retry", "transport".into(), 0),
    };
    let conn = store
        .connection
        .lock()
        .map_err(|_| OrchestratorError::StorageUnavailable)?;
    let tx = conn.unchecked_transaction()?;
    let final_state = match state {
        "sent" => {
            tx.execute(
                "UPDATE notification_deliveries SET state = 'sent', attempts = ?2, sent_at = ?3,
                    last_error = NULL, updated_at = ?3 WHERE id = ?1",
                params![due.id, attempts, now],
            )?;
            if due.kind != "update" {
                mark_shown(&tx, due, now)?;
            }
            if due.kind == "post" {
                // The record may have resolved while this post was in flight.
                on_resolved_at(&tx, Resolved::Notification(&due.notification_id), now)?;
            }
            "sent"
        }
        "failed" => {
            tx.execute(
                "UPDATE notification_deliveries SET state = 'failed', attempts = ?2,
                    last_error = ?3, updated_at = ?4 WHERE id = ?1",
                params![due.id, attempts, code, now],
            )?;
            "failed"
        }
        _ => {
            let wait = retry_delay(attempts).max(retry_after_secs);
            if now + wait > due.created_at + GIVE_UP_SECS {
                tx.execute(
                    "UPDATE notification_deliveries SET state = 'failed', attempts = ?2,
                        last_error = 'gave_up', updated_at = ?3 WHERE id = ?1",
                    params![due.id, attempts, now],
                )?;
                "failed"
            } else {
                tx.execute(
                    "UPDATE notification_deliveries SET state = 'pending', attempts = ?2,
                        not_before = ?3, last_error = ?4, updated_at = ?5 WHERE id = ?1",
                    params![due.id, attempts, now + wait, code, now],
                )?;
                "pending"
            }
        }
    };
    tx.commit()?;
    if final_state != "pending" {
        count(&due.channel, final_state);
    }
    Ok(final_state)
}

/// The wait after the `attempts`-th failed attempt (1-based).
pub fn retry_delay(attempts: i64) -> i64 {
    usize::try_from(attempts - 1)
        .ok()
        .and_then(|i| RETRY_SECS.get(i).copied())
        .unwrap_or(RETRY_TAIL_SECS)
}

fn count(channel: &str, state: &str) {
    let _ = vam_telemetry::VamMetricsRegistry::global().record_counter(
        METRIC_DELIVERIES_TOTAL,
        1,
        &[("channel", channel), ("state", state)],
    );
}

/// `first_shown_at` on the record(s) a sent post or summary carried.
fn mark_shown(tx: &Connection, due: &Due, now: i64) -> Result<(), OrchestratorError> {
    tx.execute(
        "UPDATE notifications SET first_shown_at = COALESCE(first_shown_at, ?2)
         WHERE id = ?1 OR id IN (SELECT notification_id FROM notification_deliveries
                                 WHERE folded_into = ?3)",
        params![due.notification_id, now, due.id],
    )?;
    Ok(())
}

/// Closes a row without a send.
pub fn suppress(
    store: &SqliteWorkflowStore,
    due: &Due,
    code: &str,
    now: i64,
) -> Result<(), OrchestratorError> {
    set_terminal(store, due, "suppressed", code, now)
}

pub fn fail(
    store: &SqliteWorkflowStore,
    due: &Due,
    code: &str,
    now: i64,
) -> Result<(), OrchestratorError> {
    set_terminal(store, due, "failed", code, now)
}

fn set_terminal(
    store: &SqliteWorkflowStore,
    due: &Due,
    state: &'static str,
    code: &str,
    now: i64,
) -> Result<(), OrchestratorError> {
    let conn = store
        .connection
        .lock()
        .map_err(|_| OrchestratorError::StorageUnavailable)?;
    conn.execute(
        "UPDATE notification_deliveries SET state = ?2, last_error = ?3, updated_at = ?4
         WHERE id = ?1",
        params![due.id, state, code, now],
    )?;
    count(&due.channel, state);
    Ok(())
}

/// A pending row that cannot be sent now: the next retry, or `failed` at the limit.
pub fn retry_later(
    store: &SqliteWorkflowStore,
    due: &Due,
    code: &str,
    now: i64,
) -> Result<&'static str, OrchestratorError> {
    finish(
        store,
        due,
        Ok(DeliverNotificationResponse {
            outcome: DeliverOutcome::Retry as i32,
            error_code: code.to_string(),
            message: String::new(),
            retry_after_ms: 0,
        }),
        now,
    )
}

/// One pass of the worker: release held rows, drop rows of pruned records, send what is due.
/// Returns the number of rows sent or closed.
pub async fn run_once(
    store: &std::sync::Arc<SqliteWorkflowStore>,
    vault: &std::sync::Arc<SecretsVault>,
) -> usize {
    run_once_at(store, vault, now()).await
}

pub async fn run_once_at(
    store: &std::sync::Arc<SqliteWorkflowStore>,
    vault: &std::sync::Arc<SecretsVault>,
    at: i64,
) -> usize {
    let st = store.clone();
    let due = tokio::task::spawn_blocking(move || {
        // Rows are pruned with their record (design 4.4).
        if let Ok(conn) = st.connection.lock() {
            let _ = conn.execute(
                "DELETE FROM notification_deliveries
                 WHERE notification_id NOT IN (SELECT id FROM notifications)",
                [],
            );
        }
        if let Err(e) = release_held(&st, at) {
            tracing::error!("[vamd] notification release error: {e}");
        }
        due_rows(&st, at, BATCH)
    })
    .await;
    let due = match due {
        Ok(Ok(d)) => d,
        Ok(Err(e)) => {
            tracing::error!("[vamd] notification delivery query error: {e}");
            return 0;
        }
        Err(_) => return 0,
    };
    let mut done = 0;
    for d in due {
        let (st, vt, row) = (store.clone(), vault.clone(), d.clone());
        let prepared = tokio::task::spawn_blocking(move || prepare(&st, &vt, &row, at)).await;
        let Ok(prepared) = prepared else { continue };
        let outcome = match prepared {
            Prepared::Send { endpoint, request } => {
                let result =
                    crate::hsp::deliver_notification(&endpoint, *request, SEND_TIMEOUT).await;
                let (st, row) = (store.clone(), d.clone());
                tokio::task::spawn_blocking(move || finish(&st, &row, result, at)).await
            }
            Prepared::Suppress(code) => {
                let (st, row) = (store.clone(), d.clone());
                tokio::task::spawn_blocking(move || {
                    suppress(&st, &row, code, at).map(|_| "suppressed")
                })
                .await
            }
            Prepared::Retry(code) => {
                let (st, row) = (store.clone(), d.clone());
                tokio::task::spawn_blocking(move || retry_later(&st, &row, code, at)).await
            }
            Prepared::Fail(code) => {
                let (st, row) = (store.clone(), d.clone());
                tokio::task::spawn_blocking(move || fail(&st, &row, code, at).map(|_| "failed"))
                    .await
            }
        };
        match outcome {
            Ok(Ok(_)) => done += 1,
            Ok(Err(e)) => tracing::error!("[vamd] notification delivery {} error: {e}", d.id),
            Err(_) => {}
        }
    }
    done
}

// ---- facts and rendering ---------------------------------------------------------------------

/// What the rendering needs of a record. Host fields only.
#[derive(Debug, Clone)]
pub struct Facts {
    pub id: String,
    pub kind: Kind,
    pub approval_id: Option<String>,
    pub agent_label: String,
    pub title: String,
    pub body: String,
    pub resolved_at: Option<i64>,
    pub resolved_reason: Option<String>,
    pub acted_by: Option<String>,
    pub acted_surface: Option<String>,
}

fn load_facts(conn: &Connection, id: &str) -> Result<Option<Facts>, OrchestratorError> {
    let row = conn
        .query_row(
            "SELECT kind, approval_id, agent_label, title, body, resolved_at, resolved_reason,
                    acted_by_user_id, acted_surface
             FROM notifications WHERE id = ?1",
            [id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, Option<i64>>(5)?,
                    r.get::<_, Option<String>>(6)?,
                    r.get::<_, Option<String>>(7)?,
                    r.get::<_, Option<String>>(8)?,
                ))
            },
        )
        .optional()?;
    Ok(row.and_then(
        |(kind, approval_id, agent_label, title, body, ra, rr, by, surface)| {
            Some(Facts {
                id: id.to_string(),
                kind: Kind::parse(&kind)?,
                approval_id,
                agent_label,
                title,
                body,
                resolved_at: ra,
                resolved_reason: rr,
                acted_by: by,
                acted_surface: surface,
            })
        },
    ))
}

/// The approval row a decision record names.
#[derive(Debug, Clone)]
pub struct ApprovalFacts {
    pub kind: String,
    pub tool_name: String,
    pub requested_args_json: String,
}

fn load_approval(
    conn: &Connection,
    request_id: &str,
) -> Result<Option<ApprovalFacts>, OrchestratorError> {
    Ok(conn
        .query_row(
            "SELECT COALESCE(kind, 'tool_call'), tool_name, requested_args_json
             FROM approvals WHERE request_id = ?1",
            [request_id],
            |r| {
                Ok(ApprovalFacts {
                    kind: r.get(0)?,
                    tool_name: r.get(1)?,
                    requested_args_json: r.get(2)?,
                })
            },
        )
        .optional()?)
}

fn summary_members(conn: &Connection, summary_id: &str) -> Result<Vec<Facts>, OrchestratorError> {
    let ids = {
        let mut stmt = conn.prepare(
            "SELECT notification_id FROM notification_deliveries WHERE folded_into = ?1
             ORDER BY created_at, id",
        )?;
        let rows = stmt
            .query_map([summary_id], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        rows
    };
    let mut out = Vec::new();
    for id in ids {
        if let Some(f) = load_facts(conn, &id)? {
            out.push(f);
        }
    }
    Ok(out)
}

fn action(
    id: &str,
    label: &str,
    decision: &str,
    option: &str,
    style: ActionStyle,
) -> RenderedAction {
    RenderedAction {
        id: id.to_string(),
        label: label.to_string(),
        decision: decision.to_string(),
        option: option.to_string(),
        style: style as i32,
    }
}

fn open_url(app_url: Option<&str>, id: &str) -> String {
    app_url
        .map(|u| format!("{}/notifications/{id}", u.trim_end_matches('/')))
        .unwrap_or_default()
}

/// Longest prefix of `s` that is at most `max` bytes and ends on a character boundary.
fn clip_bytes(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Agent-written text as a rendering may carry it: at most [`AGENT_TEXT_MAX_BYTES`], no control
/// characters but newline, and no run of backticks that could close the adapter's fence.
pub fn agent_text(label: &str, text: &str) -> Option<AgentText> {
    let cleaned: String = text
        .chars()
        .filter(|c| *c == '\n' || !c.is_control())
        .collect::<String>()
        .replace("```", "'''");
    let cleaned = clip_bytes(cleaned.trim(), AGENT_TEXT_MAX_BYTES).to_string();
    if cleaned.is_empty() {
        return None;
    }
    Some(AgentText {
        label: label.to_string(),
        text: cleaned,
    })
}

fn written_by(agent_label: &str) -> String {
    let who = if agent_label.trim().is_empty() {
        "the agent"
    } else {
        agent_label.trim()
    };
    format!("written by {who}, not verified by the host")
}

/// Substitutes `{"$data": "/pointer"}` with the data it names, drops nodes whose `visible` is
/// falsy or whose `hidden` is truthy, and strips the two schemas. The result is a self-contained
/// IR: the adapter needs no data.
pub fn materialize_ir(mut ir: Value, data: &Value) -> Value {
    fn subst(v: &mut Value, data: &Value) {
        match v {
            Value::Object(m) if m.len() == 1 && m.contains_key("$data") => {
                let p = m["$data"].as_str().unwrap_or("");
                *v = data.pointer(p).cloned().unwrap_or(Value::Null);
            }
            Value::Object(m) => m.values_mut().for_each(|x| subst(x, data)),
            Value::Array(a) => a.iter_mut().for_each(|x| subst(x, data)),
            _ => {}
        }
    }
    fn truthy(v: &Value) -> bool {
        match v {
            Value::Null => false,
            Value::Bool(b) => *b,
            Value::String(s) => !s.is_empty(),
            Value::Array(a) => !a.is_empty(),
            Value::Object(o) => !o.is_empty(),
            Value::Number(_) => true,
        }
    }
    if let Some(o) = ir.as_object_mut() {
        o.remove("inputSchema");
        o.remove("resultSchema");
    }
    if let Some(nodes) = ir.get_mut("nodes") {
        subst(nodes, data);
    }
    let mut gone: Vec<String> = Vec::new();
    if let Some(nodes) = ir.get_mut("nodes").and_then(Value::as_object_mut) {
        for (id, node) in nodes.iter_mut() {
            let Some(n) = node.as_object_mut() else {
                continue;
            };
            let hide = n.remove("hidden").is_some_and(|h| truthy(&h));
            let show = n.remove("visible").map(|v| truthy(&v)).unwrap_or(true);
            if hide || !show {
                gone.push(id.clone());
            }
        }
    }
    drop_nodes(&mut ir, &gone);
    ir
}

/// Removes nodes from an IR and from every `children` list.
fn drop_nodes(ir: &mut Value, gone: &[String]) {
    if let Some(nodes) = ir.get_mut("nodes").and_then(Value::as_object_mut) {
        for id in gone {
            nodes.remove(id);
        }
        for node in nodes.values_mut() {
            if let Some(children) = node.get_mut("children").and_then(Value::as_array_mut) {
                children.retain(|c| c.as_str().is_none_or(|c| !gone.iter().any(|g| g == c)));
            }
        }
    }
}

/// Whether the IR renders natively in Slack: every node is in [`SLACK_PRIMITIVES`], every Select
/// has at most [`SLACK_MAX_SELECT_OPTIONS`] options, and every action is a submit.
pub fn renders_natively(ir: &Value) -> bool {
    let Some(nodes) = ir.get("nodes").and_then(Value::as_object) else {
        return false;
    };
    if nodes.is_empty() {
        return false;
    }
    nodes.values().all(|n| {
        let ty = n.get("type").and_then(Value::as_str).unwrap_or("");
        if !SLACK_PRIMITIVES.contains(&ty) {
            return false;
        }
        if ty == "Select"
            && n.pointer("/props/options")
                .and_then(Value::as_array)
                .is_some_and(|o| o.len() > SLACK_MAX_SELECT_OPTIONS)
        {
            return false;
        }
        match n.pointer("/props/action") {
            None => true,
            Some(a) => a.get("event").and_then(Value::as_str) == Some("submit"),
        }
    })
}

fn to_struct(ir: &Value) -> Option<Struct> {
    vam_spec::harness::json_to_struct(ir)
}

/// The approve jig with the decisions vamd offers: no `always` or `never` button (locked 12) and
/// none of the agent-written text (it travels labelled in `agent_text`).
fn approve_ir(tool: &str) -> Option<Value> {
    let mut ir = vam_spec::jig::builtin("vam.approve").ok()?.ir();
    drop_nodes(
        &mut ir,
        &[
            "always".into(),
            "never".into(),
            "summary".into(),
            "why".into(),
        ],
    );
    let ir = materialize_ir(ir, &json!({ "tool": tool }));
    renders_natively(&ir).then_some(ir)
}

/// The question jig, without the question text (agent-written, so in `agent_text`).
fn question_ir(choices: &[String]) -> Option<Value> {
    let mut ir = vam_spec::jig::builtin("vam.question").ok()?.ir();
    drop_nodes(&mut ir, &["q".into()]);
    let ir = materialize_ir(ir, &json!({ "choices": choices }));
    renders_natively(&ir).then_some(ir)
}

/// The generated gate form for a response schema, when it renders natively.
fn gate_form_ir(question: &str, schema: &Value) -> Option<Value> {
    let data = json!({ "question": question, "response_schema": schema });
    let mut ir = vam_spec::jig::form_ir("vam.gate_default", &data).ok()?;
    drop_nodes(&mut ir, &["q".into()]);
    let ir = materialize_ir(ir, &data);
    renders_natively(&ir).then_some(ir)
}

/// The neutral rendering of a post (NT-2.10). Host title and body; the actions `vamd` offers;
/// `jig_ir` only when the whole jig renders natively in Slack and its only action is a submit;
/// agent-written text only in `agent_text`; `open_url` from the node's app URL when set; the CLI
/// line always.
pub fn build_rendering(
    f: &Facts,
    approval: Option<&ApprovalFacts>,
    app_url: Option<&str>,
) -> NotificationRendering {
    let mut r = NotificationRendering {
        title: f.title.clone(),
        body: f.body.clone(),
        open_url: open_url(app_url, &f.id),
        cli_line: match (&f.approval_id, f.kind.is_decision()) {
            (Some(a), true) => format!("vam approve decide {a}"),
            _ => format!("vam notifications show {}", f.id),
        },
        ..Default::default()
    };
    if !f.kind.is_decision() || f.resolved_at.is_some() {
        return r;
    }
    let Some(a) = approval else { return r };
    let args: Value = serde_json::from_str(&a.requested_args_json).unwrap_or(Value::Null);
    match (f.kind, a.kind.as_str()) {
        (Kind::ApprovalPending, "tool_call") => {
            // Approve and Deny only. Never "always": a standing rule is set in the app or CLI.
            r.actions = vec![
                action("approve", "Approve", "APPROVED", "", ActionStyle::Primary),
                action("deny", "Deny", "REJECTED", "", ActionStyle::Danger),
            ];
            r.jig_ir = approve_ir(&a.tool_name).as_ref().and_then(to_struct);
            let preview = if args.is_null() {
                String::new()
            } else {
                args.to_string()
            };
            r.agent_text = agent_text(&written_by(&f.agent_label), &preview);
        }
        (Kind::QuestionAsked, "question") => {
            let data = args.get("data").unwrap_or(&Value::Null);
            let question = data.get("question").and_then(Value::as_str).unwrap_or("");
            let choices: Vec<String> = data
                .get("choices")
                .and_then(Value::as_array)
                .map(|c| {
                    c.iter()
                        .filter_map(|x| x.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            r.agent_text = agent_text(&written_by(&f.agent_label), question);
            if let Some(ir) = question_ir(&choices) {
                r.jig_ir = to_struct(&ir);
                r.actions = vec![action(
                    "send",
                    "Send",
                    "SUBMITTED",
                    "",
                    ActionStyle::Primary,
                )];
            }
        }
        (Kind::GateOpened, "gate") => {
            let question = args.get("question").and_then(Value::as_str).unwrap_or("");
            r.agent_text = agent_text("from the playbook", question);
            let options: Vec<&str> = args
                .get("options")
                .and_then(Value::as_array)
                .map(|o| o.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            if !options.is_empty() {
                // An option gate: one button per option and no form.
                r.actions = options
                    .iter()
                    .enumerate()
                    .map(|(i, o)| {
                        action(&format!("opt_{i}"), o, "SUBMITTED", o, ActionStyle::Default)
                    })
                    .collect();
            } else if let Some(ir) = args
                .get("response_schema")
                .and_then(|s| gate_form_ir(question, s))
            {
                r.jig_ir = to_struct(&ir);
                r.actions = vec![action(
                    "submit",
                    "Submit",
                    "SUBMITTED",
                    "",
                    ActionStyle::Primary,
                )];
            }
            // Otherwise: title, body and Open in VAM only. No fields, no buttons.
        }
        _ => {}
    }
    r
}

/// The edit that replaces a message once its record is resolved: no actions, no agent text.
pub fn build_update(f: &Facts, decided_by: &str, app_url: Option<&str>) -> NotificationRendering {
    let title = match (&f.acted_surface, f.resolved_at) {
        (Some(surface), Some(at)) => {
            let who = if decided_by.is_empty() {
                "a user"
            } else {
                decided_by
            };
            format!("Decided by {who} in {surface} at {}", format_time(at))
        }
        (None, Some(at)) => format!(
            "No longer waiting ({}) at {}",
            f.resolved_reason.as_deref().unwrap_or("closed"),
            format_time(at)
        ),
        _ => "No longer waiting".to_string(),
    };
    NotificationRendering {
        title,
        body: f.title.clone(),
        open_url: open_url(app_url, &f.id),
        cli_line: format!("vam notifications show {}", f.id),
        ..Default::default()
    }
}

fn format_time(at: i64) -> String {
    chrono::DateTime::from_timestamp(at, 0)
        .map(|t| t.format("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_else(|| at.to_string())
}

/// The "and N more" message: one line per record (at most [`SUMMARY_MAX_SECTIONS`]), then a count.
pub fn build_summary(members: &[Facts], app_url: Option<&str>) -> NotificationRendering {
    let shown = members.iter().take(SUMMARY_MAX_SECTIONS);
    let mut body: Vec<String> = shown.map(|m| format!("- {}", m.title)).collect();
    if members.len() > SUMMARY_MAX_SECTIONS {
        body.push(format!("and {} more", members.len() - SUMMARY_MAX_SECTIONS));
    }
    NotificationRendering {
        title: format!("{} notifications", members.len()),
        body: body.join("\n"),
        open_url: app_url
            .map(|u| format!("{}/notifications", u.trim_end_matches('/')))
            .unwrap_or_default(),
        cli_line: "vam notifications".to_string(),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests;
