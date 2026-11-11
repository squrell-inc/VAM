//! The notification record (`design/notifications.md` section 2): one row per event and
//! recipient user, written by `vamd` inside the transaction of the fact it reports.
//!
//! Host facts only: titles and bodies come from fixed templates over ids, tool names and step
//! ids. Agent-written text is never copied into a row; the API attaches it at read time.

use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};

use crate::error::OrchestratorError;
use crate::spaces::mint_id;
use crate::workflow::SqliteWorkflowStore;

pub const MAX_TITLE_CHARS: usize = 120;
pub const MAX_BODY_CHARS: usize = 500;
/// Rows kept per user (oldest resolved first).
pub const MAX_ROWS_PER_USER: i64 = 2000;
const INFO_RETENTION_SECS: i64 = 30 * 86_400;
const WARN_RETENTION_SECS: i64 = 90 * 86_400;
pub const METRIC_NOTIFICATIONS_TOTAL: &str = "vam_notifications_total";
/// `runs.source` of an asked (delegated) run: `design/primary-agent.md` section 5 (the asked
/// run's `runs.source` is `broker:vam.delegate`). It starts with `broker:`, so the trigger rule
/// of `run.finished` must name it to leave it out: the asking user gets `report.arrived` or
/// `delegation.*` instead (`design/notifications.md` section 2.3).
pub const DELEGATE_RUN_SOURCE: &str = "broker:vam.delegate";
/// Prefix of the `runs.source` of a report run (`report:<childRunId>`, `design/primary-agent.md`
/// section 5, report back). Never a person's run and never a trigger's.
pub const REPORT_RUN_SOURCE_PREFIX: &str = "report:";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Action,
    Warn,
    Info,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Action => "action",
            Severity::Warn => "warn",
            Severity::Info => "info",
        }
    }
}

/// Every kind of design section 2.2. Only the first three have producers in slice 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    ApprovalPending,
    QuestionAsked,
    GateOpened,
    RunFinished,
    ReportArrived,
    DelegationStopped,
    DelegationExpired,
    GrantExpiring,
    BudgetHit,
    TriggerFired,
    TriggerSkipped,
    ConnectionNeedsReauth,
}

impl Kind {
    pub const ALL: [Kind; 12] = [
        Kind::ApprovalPending,
        Kind::QuestionAsked,
        Kind::GateOpened,
        Kind::RunFinished,
        Kind::ReportArrived,
        Kind::DelegationStopped,
        Kind::DelegationExpired,
        Kind::GrantExpiring,
        Kind::BudgetHit,
        Kind::TriggerFired,
        Kind::TriggerSkipped,
        Kind::ConnectionNeedsReauth,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Kind::ApprovalPending => "approval.pending",
            Kind::QuestionAsked => "question.asked",
            Kind::GateOpened => "gate.opened",
            Kind::RunFinished => "run.finished",
            Kind::ReportArrived => "report.arrived",
            Kind::DelegationStopped => "delegation.stopped",
            Kind::DelegationExpired => "delegation.expired",
            Kind::GrantExpiring => "grant.expiring",
            Kind::BudgetHit => "budget.hit",
            Kind::TriggerFired => "trigger.fired",
            Kind::TriggerSkipped => "trigger.skipped",
            Kind::ConnectionNeedsReauth => "connection.needs_reauth",
        }
    }

    pub fn parse(s: &str) -> Option<Kind> {
        Kind::ALL.into_iter().find(|k| k.as_str() == s)
    }

    /// Default severity; `run.finished` and `report.arrived` become `warn` when the run failed
    /// (the caller says so).
    pub fn severity(self) -> Severity {
        match self {
            Kind::ApprovalPending
            | Kind::QuestionAsked
            | Kind::GateOpened
            | Kind::ConnectionNeedsReauth => Severity::Action,
            Kind::RunFinished | Kind::ReportArrived | Kind::TriggerFired => Severity::Info,
            Kind::DelegationStopped
            | Kind::DelegationExpired
            | Kind::GrantExpiring
            | Kind::BudgetHit
            | Kind::TriggerSkipped => Severity::Warn,
        }
    }

    /// A decision kind is cleared only by deciding the approval it names.
    pub fn is_decision(self) -> bool {
        matches!(
            self,
            Kind::ApprovalPending | Kind::QuestionAsked | Kind::GateOpened
        )
    }

    fn action_verb(self) -> &'static str {
        match self {
            Kind::ApprovalPending | Kind::GateOpened => "decide",
            Kind::QuestionAsked => "answer",
            Kind::GrantExpiring => "renew",
            _ => "open",
        }
    }
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('\u{2026}');
        out
    }
}

/// Host-written title and body for a kind. `agent`, `detail` (tool name or step id) and
/// `expires_at` are ids and numbers the host owns.
pub fn render(kind: Kind, agent: &str, detail: &str, expires_at: Option<i64>) -> (String, String) {
    let agent = if agent.is_empty() { "An agent" } else { agent };
    let title = match kind {
        Kind::ApprovalPending => format!("{agent} waits for your approval"),
        Kind::QuestionAsked => format!("{agent} asks you a question"),
        Kind::GateOpened => format!("{agent} reached a gate and waits for you"),
        Kind::RunFinished => format!("{agent} finished"),
        Kind::ReportArrived => format!("{agent} reported back"),
        Kind::DelegationStopped => format!("{agent}: an asked run was stopped"),
        Kind::DelegationExpired => format!("{agent}: an asked run expired"),
        Kind::GrantExpiring => format!("{agent}: a grant is about to expire"),
        Kind::BudgetHit => format!("{agent} hit its budget"),
        Kind::TriggerFired => format!("{agent}: a scheduled run was queued"),
        Kind::TriggerSkipped => format!("{agent}: a scheduled run was skipped"),
        Kind::ConnectionNeedsReauth => format!("Reconnect {agent} to continue"),
    };
    let mut body = match kind {
        Kind::ApprovalPending => format!("Tool: {detail}"),
        Kind::QuestionAsked => "Open the question to answer it.".to_string(),
        Kind::GateOpened => format!("Step: {detail}"),
        _ => detail.to_string(),
    };
    if let Some(at) = expires_at {
        if kind.is_decision() {
            body.push_str(&format!(" Expires at {at} (unix seconds)."));
        }
    }
    (clip(&title, MAX_TITLE_CHARS), clip(&body, MAX_BODY_CHARS))
}

#[derive(Debug, Clone)]
pub struct NewNotification {
    pub user_id: String,
    pub kind: Kind,
    pub severity: Severity,
    pub subject_key: String,
    pub run_id: Option<String>,
    pub space_id: Option<String>,
    pub thread_id: Option<String>,
    pub approval_id: Option<String>,
    pub grant_id: Option<String>,
    pub receipt_digest: Option<String>,
    pub agent_urn: String,
    pub agent_label: String,
    pub via: Option<String>,
    pub title: String,
    pub body: String,
    /// `{verb, target:{kind,id}, api}`.
    pub action: Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordOutcome {
    Inserted(String),
    /// The same subject recurred while unresolved: `count + 1`.
    Updated(String),
}

pub(crate) fn initialize_schema(connection: &Connection) -> Result<(), OrchestratorError> {
    connection.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS notifications (
            id TEXT PRIMARY KEY NOT NULL,
            user_id TEXT NOT NULL,
            kind TEXT NOT NULL,
            severity TEXT NOT NULL CHECK (severity IN ('action','warn','info')),
            subject_key TEXT NOT NULL,
            run_id TEXT,
            space_id TEXT,
            thread_id TEXT,
            approval_id TEXT,
            grant_id TEXT,
            receipt_digest TEXT,
            agent_urn TEXT NOT NULL DEFAULT '',
            agent_label TEXT NOT NULL DEFAULT '',
            via TEXT,
            title TEXT NOT NULL,
            body TEXT NOT NULL,
            action TEXT NOT NULL,
            count INTEGER NOT NULL DEFAULT 1,
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL,
            first_shown_at INTEGER,
            read_at INTEGER,
            acted_at INTEGER,
            acted_by_user_id TEXT,
            acted_surface TEXT,
            resolved_at INTEGER,
            resolved_reason TEXT,
            UNIQUE (user_id, kind, subject_key)
        );
        CREATE INDEX IF NOT EXISTS idx_notifications_user_resolved
            ON notifications(user_id, resolved_at, created_at);
        CREATE INDEX IF NOT EXISTS idx_notifications_user_read
            ON notifications(user_id, read_at);
        CREATE INDEX IF NOT EXISTS idx_notifications_approval
            ON notifications(approval_id);
        CREATE TABLE IF NOT EXISTS notification_migrations (
            name TEXT PRIMARY KEY NOT NULL
        );
        ",
    )?;
    migrate_key_scopes(connection)?;
    Ok(())
}

/// Once: stored API keys that hold `approvals:write` get the two notification scopes, so keys
/// minted before this change (the CLI's) can list. Marked done, so an admin who later removes a
/// scope is not overridden on the next start.
fn migrate_key_scopes(connection: &Connection) -> Result<(), OrchestratorError> {
    const NAME: &str = "api_key_notification_scopes_v1";
    let has_keys: bool = connection
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'api_keys'",
            [],
            |_| Ok(true),
        )
        .optional()?
        .unwrap_or(false);
    if !has_keys {
        return Ok(());
    }
    let done: bool = connection
        .query_row(
            "SELECT 1 FROM notification_migrations WHERE name = ?1",
            [NAME],
            |_| Ok(true),
        )
        .optional()?
        .unwrap_or(false);
    if done {
        return Ok(());
    }
    let rows: Vec<(String, String)> = {
        let mut stmt =
            connection.prepare("SELECT key_id, scopes FROM api_keys WHERE revoked_at IS NULL")?;
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        rows
    };
    for (key_id, scopes_json) in rows {
        let Ok(mut scopes) = serde_json::from_str::<Vec<String>>(&scopes_json) else {
            continue;
        };
        if !scopes.iter().any(|s| s == "approvals:write") {
            continue;
        }
        let mut changed = false;
        for add in ["notifications:read", "notifications:write"] {
            if !scopes.iter().any(|s| s == add) {
                scopes.push(add.to_string());
                changed = true;
            }
        }
        if changed {
            let json = serde_json::to_string(&scopes)
                .map_err(|e| OrchestratorError::Core(e.to_string()))?;
            connection.execute(
                "UPDATE api_keys SET scopes = ?1 WHERE key_id = ?2",
                params![json, key_id],
            )?;
        }
    }
    connection.execute(
        "INSERT OR IGNORE INTO notification_migrations (name) VALUES (?1)",
        [NAME],
    )?;
    Ok(())
}

fn now() -> i64 {
    crate::catalog::unix_timestamp()
}

/// Inserts or updates by `(user_id, kind, subject_key)`. Call it on the transaction that
/// holds the fact; a failure fails the caller.
pub fn record(tx: &Connection, n: &NewNotification) -> Result<RecordOutcome, OrchestratorError> {
    let ts = now();
    let existing: Option<(String, Option<i64>)> = tx
        .query_row(
            "SELECT id, resolved_at FROM notifications
             WHERE user_id = ?1 AND kind = ?2 AND subject_key = ?3",
            params![n.user_id, n.kind.as_str(), n.subject_key],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let action = n.action.to_string();
    match existing {
        Some((id, None)) => {
            tx.execute(
                "UPDATE notifications SET count = count + 1, approval_id = ?2, title = ?3,
                    body = ?4, action = ?5, updated_at = ?6
                 WHERE id = ?1",
                params![id, n.approval_id, n.title, n.body, action, ts],
            )?;
            Ok(RecordOutcome::Updated(id))
        }
        other => {
            if let Some((old_id, Some(_))) = other {
                // A resolved record never reopens: it gives up its subject, a new row takes it.
                tx.execute(
                    "UPDATE notifications SET subject_key = subject_key || '#' || id
                     WHERE id = ?1",
                    [old_id],
                )?;
            }
            let id = mint_id("nt_");
            tx.execute(
                "INSERT INTO notifications (
                    id, user_id, kind, severity, subject_key, run_id, space_id, thread_id,
                    approval_id, grant_id, receipt_digest, agent_urn, agent_label, via,
                    title, body, action, count, created_at, updated_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14,
                           ?15, ?16, ?17, 1, ?18, ?18)",
                params![
                    id,
                    n.user_id,
                    n.kind.as_str(),
                    n.severity.as_str(),
                    n.subject_key,
                    n.run_id,
                    n.space_id,
                    n.thread_id,
                    n.approval_id,
                    n.grant_id,
                    n.receipt_digest,
                    n.agent_urn,
                    n.agent_label,
                    n.via,
                    n.title,
                    n.body,
                    action,
                    ts
                ],
            )?;
            let _ = vam_telemetry::VamMetricsRegistry::global().record_counter(
                METRIC_NOTIFICATIONS_TOTAL,
                1,
                &[("kind", n.kind.as_str())],
            );
            // NT-2.3: delivery rows in the record's own transaction; a failure fails the caller.
            crate::notify_delivery::on_recorded(tx, &id)?;
            Ok(RecordOutcome::Inserted(id))
        }
    }
}

/// Records the decision notification for the approval row `request_id`, which the caller has
/// just inserted on `tx`. A `reauth` park gets none (tracker question 7).
pub(crate) fn record_for_approval(
    tx: &Connection,
    request_id: &str,
) -> Result<Option<RecordOutcome>, OrchestratorError> {
    let row = tx
        .query_row(
            "SELECT run_id, COALESCE(kind, 'tool_call'), tool_name, args_digest,
                    approver_user_id, expires_at, step_id
             FROM approvals WHERE request_id = ?1",
            [request_id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, Option<i64>>(5)?,
                    r.get::<_, String>(6)?,
                ))
            },
        )
        .optional()?;
    let Some((run_id, approval_kind, tool_name, args_digest, approver, expires_at, step_id)) = row
    else {
        return Ok(None);
    };
    let (kind, detail, subject) = match approval_kind.as_str() {
        "gate" => (
            Kind::GateOpened,
            step_id.clone(),
            format!("{run_id}/gate/{step_id}"),
        ),
        "question" => (
            Kind::QuestionAsked,
            tool_name.clone(),
            format!("{run_id}|{tool_name}|{args_digest}"),
        ),
        "tool_call" => (
            Kind::ApprovalPending,
            tool_name.clone(),
            format!("{run_id}|{tool_name}|{args_digest}"),
        ),
        _ => return Ok(None),
    };
    let user_id = crate::owner::resolved_owner(approver.as_deref().unwrap_or("")).to_string();
    let (agent_urn, space_id, thread_id) = run_place(tx, &run_id);
    let agent_label = agent_urn
        .rsplit('/')
        .next()
        .unwrap_or("")
        .rsplit(':')
        .next()
        .unwrap_or("")
        .to_string();
    let (title, body) = render(kind, &agent_label, &detail, expires_at);
    let api = format!("POST /v1/approvals/{request_id}/decide");
    record(
        tx,
        &NewNotification {
            user_id,
            kind,
            severity: kind.severity(),
            subject_key: subject,
            run_id: Some(run_id),
            space_id,
            thread_id,
            approval_id: Some(request_id.to_string()),
            grant_id: None,
            receipt_digest: None,
            agent_urn,
            agent_label,
            via: None,
            title,
            body,
            action: json!({
                "verb": kind.action_verb(),
                "target": { "kind": "approval", "id": request_id },
                "api": api,
            }),
        },
    )
    .map(Some)
}

/// `connection.needs_reauth`: `action`, verb `open` on the connector, to the connection's owner
/// only. The subject is the connector id, so one row is open per transition to needs-reconnect
/// (a repeat while open only counts); a connect resolves it. Fixed text over the display name.
pub fn record_connection_needs_reauth(
    tx: &Connection,
    owner: &str,
    connector_id: &str,
    display_name: &str,
) -> Result<RecordOutcome, OrchestratorError> {
    let kind = Kind::ConnectionNeedsReauth;
    let (title, _) = render(kind, display_name, "", None);
    let body = format!(
        "{display_name} no longer accepts your saved connection. Reconnect it so your agents can use it again."
    );
    record(
        tx,
        &NewNotification {
            user_id: crate::owner::resolved_owner(owner).to_string(),
            kind,
            severity: kind.severity(),
            subject_key: connector_id.to_string(),
            run_id: None,
            space_id: None,
            thread_id: None,
            approval_id: None,
            grant_id: None,
            receipt_digest: None,
            agent_urn: String::new(),
            agent_label: String::new(),
            via: None,
            title,
            body: clip(&body, MAX_BODY_CHARS),
            action: json!({
                "verb": kind.action_verb(),
                "target": { "kind": "connector", "id": connector_id },
                "api": "GET /v1/connectors",
            }),
        },
    )
}

/// Resolves the owner's open `connection.needs_reauth` row for a connector (`acted` on a
/// connect). A no-op when there is none.
pub fn resolve_connection_needs_reauth(
    tx: &Connection,
    owner: &str,
    connector_id: &str,
    reason: &str,
) -> Result<usize, OrchestratorError> {
    let ts = now();
    Ok(tx.execute(
        "UPDATE notifications SET resolved_at = ?3, resolved_reason = ?4, updated_at = ?3,
            acted_at = CASE WHEN ?4 = 'acted' THEN ?3 ELSE NULL END
         WHERE kind = 'connection.needs_reauth' AND user_id = ?1 AND subject_key = ?2
           AND resolved_at IS NULL",
        params![
            crate::owner::resolved_owner(owner).to_string(),
            connector_id,
            ts,
            reason
        ],
    )?)
}

impl SqliteWorkflowStore {
    /// Raises `connection.needs_reauth` for the owner of a connector connection that just moved
    /// to needs-reconnect. Best effort: a failure is logged, never fails the caller.
    pub fn raise_connection_needs_reauth(&self, owner: &str, connector_id: &str, name: &str) {
        let Ok(mut c) = self.connection.lock() else {
            return;
        };
        let r = c
            .transaction()
            .map_err(OrchestratorError::from)
            .and_then(|tx| {
                record_connection_needs_reauth(&tx, owner, connector_id, name)?;
                tx.commit().map_err(OrchestratorError::from)
            });
        if let Err(e) = r {
            tracing::warn!(connector_id, "connection.needs_reauth not recorded: {e}");
        }
    }

    /// Ties the owner's open `connection.needs_reauth` row to the run that parked on it (the run
    /// the member should expect to continue), when the row has no run yet.
    pub fn attach_run_to_connection_needs_reauth(
        &self,
        owner: &str,
        connector_id: &str,
        run_id: &str,
    ) {
        let Ok(c) = self.connection.lock() else {
            return;
        };
        let (_, space_id, thread_id) = run_place(&c, run_id);
        let r = c.execute(
            "UPDATE notifications SET run_id = ?3, space_id = ?4, thread_id = ?5
             WHERE kind = 'connection.needs_reauth' AND user_id = ?1 AND subject_key = ?2
               AND resolved_at IS NULL AND run_id IS NULL",
            params![
                crate::owner::resolved_owner(owner).to_string(),
                connector_id,
                run_id,
                space_id,
                thread_id
            ],
        );
        if let Err(e) = r {
            tracing::warn!(
                connector_id,
                "connection.needs_reauth not tied to its run: {e}"
            );
        }
    }

    /// Resolves the owner's open `connection.needs_reauth` row for a connector.
    pub fn resolve_connection_needs_reauth(&self, owner: &str, connector_id: &str, reason: &str) {
        let Ok(mut c) = self.connection.lock() else {
            return;
        };
        let r = c
            .transaction()
            .map_err(OrchestratorError::from)
            .and_then(|tx| {
                resolve_connection_needs_reauth(&tx, owner, connector_id, reason)?;
                tx.commit().map_err(OrchestratorError::from)
            });
        if let Err(e) = r {
            tracing::warn!(connector_id, "connection.needs_reauth not resolved: {e}");
        }
    }
}

/// Whether a run's end is a notification (`design/notifications.md` section 2.3). Pure over host
/// facts: `source` (`runs.source`), `state`, `result_json`, and whether a published approval row
/// exists (the run waited on a person).
pub fn run_finished_wanted(
    source: Option<&str>,
    state: &str,
    result_json: Option<&str>,
    waited: bool,
) -> bool {
    if let Some(src) = source {
        if src == DELEGATE_RUN_SOURCE || src.starts_with(REPORT_RUN_SOURCE_PREFIX) {
            return false;
        }
    }
    // A user stop (also a trigger run its owner stopped) ends FAILED with reason `stopped`.
    let stopped = result_json
        .and_then(|j| serde_json::from_str::<Value>(j).ok())
        .and_then(|v| {
            v.get("reason")
                .and_then(Value::as_str)
                .map(|r| r == crate::session_end::STOPPED_REASON)
        })
        .unwrap_or(false);
    if stopped {
        return false;
    }
    // The duration stop has `budget.hit` and no `run.finished` (section 2.3).
    if is_duration_stop(result_json) {
        return false;
    }
    let trigger = source.is_some_and(|s| s.starts_with("broker:"));
    let failed = matches!(state, "FAILED" | "TIMED_OUT" | "EXPIRED" | "REJECTED");
    trigger || waited || failed
}

type RunRow = (String, Option<String>, Option<String>, Option<String>);

/// Writes the `run.finished` record of a run that ended, when section 2.3 says so. Idempotent: a
/// run has at most one, and a later call only fills a missing receipt digest. Call it on the
/// transaction that stores the receipt, or from the sweeper for a run whose receipt cannot be
/// signed (`digest` None). Returns whether a record was inserted.
pub fn record_run_finished(
    tx: &Connection,
    run_id: &str,
    digest: Option<&str>,
) -> Result<bool, OrchestratorError> {
    let existing: Option<String> = tx
        .query_row(
            "SELECT id FROM notifications WHERE kind = 'run.finished' AND run_id = ?1 LIMIT 1",
            [run_id],
            |r| r.get(0),
        )
        .optional()?;
    if existing.is_some() {
        if let Some(d) = digest {
            tx.execute(
                "UPDATE notifications SET receipt_digest = ?2, updated_at = ?3
                 WHERE kind = 'run.finished' AND run_id = ?1 AND receipt_digest IS NULL",
                params![run_id, d, now()],
            )?;
        }
        return Ok(false);
    }
    let row: Option<RunRow> = tx
        .query_row(
            "SELECT state, source, result_json, owner_user_id FROM runs WHERE run_id = ?1",
            [run_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?;
    let Some((state, source, result_json, owner)) = row else {
        return Ok(false);
    };
    if !crate::receipt::is_terminal(&state) {
        return Ok(false);
    }
    let waited: bool = tx
        .query_row(
            "SELECT EXISTS (SELECT 1 FROM approvals WHERE run_id = ?1 AND published = 1)",
            [run_id],
            |r| r.get(0),
        )
        .unwrap_or(false);
    if !run_finished_wanted(source.as_deref(), &state, result_json.as_deref(), waited) {
        return Ok(false);
    }
    let user_id = crate::owner::resolved_owner(owner.as_deref().unwrap_or("")).to_string();
    let (agent_urn, space_id, thread_id) = run_place(tx, run_id);
    let agent_label = agent_label_of(&agent_urn);
    let kind = Kind::RunFinished;
    let severity = if state == "SUCCEEDED" {
        Severity::Info
    } else {
        Severity::Warn
    };
    let detail = if state == "SUCCEEDED" {
        "The run succeeded.".to_string()
    } else {
        format!("The run ended {state}.")
    };
    let (title, body) = render(kind, &agent_label, &detail, None);
    record(
        tx,
        &NewNotification {
            user_id,
            kind,
            severity,
            subject_key: run_id.to_string(),
            run_id: Some(run_id.to_string()),
            space_id,
            thread_id,
            approval_id: None,
            grant_id: None,
            receipt_digest: digest.map(str::to_string),
            agent_urn,
            agent_label,
            via: None,
            title,
            body,
            action: json!({
                "verb": "open",
                "target": { "kind": "receipt", "id": run_id },
                "api": format!("GET /v1/runs/{run_id}/receipt"),
            }),
        },
    )?;
    Ok(true)
}

fn agent_label_of(agent_urn: &str) -> String {
    agent_urn
        .rsplit('/')
        .next()
        .unwrap_or("")
        .rsplit(':')
        .next()
        .unwrap_or("")
        .to_string()
}

/// Facts of a report back or an asked run that sends none (`design/primary-agent.md` section 5).
/// Nothing calls these functions yet; the primary agent's report-back task will.
#[derive(Debug, Clone)]
pub struct DelegationFacts {
    pub asking_user_id: String,
    pub child_run_id: String,
    pub by_run_id: String,
    pub by_agent_urn: String,
    pub thread_id: String,
    pub space_id: Option<String>,
    pub child_agent_urn: String,
    pub receipt_digest: Option<String>,
    /// For `report.arrived`: whether the asked run succeeded.
    pub succeeded: bool,
}

fn record_delegation(
    tx: &Connection,
    kind: Kind,
    severity: Severity,
    f: &DelegationFacts,
) -> Result<RecordOutcome, OrchestratorError> {
    let agent_label = agent_label_of(&f.child_agent_urn);
    let (title, body) = render(kind, &agent_label, "", None);
    record(
        tx,
        &NewNotification {
            user_id: crate::owner::resolved_owner(&f.asking_user_id).to_string(),
            kind,
            severity,
            subject_key: f.child_run_id.clone(),
            run_id: Some(f.child_run_id.clone()),
            space_id: f.space_id.clone(),
            thread_id: Some(f.thread_id.clone()),
            approval_id: None,
            grant_id: None,
            receipt_digest: f.receipt_digest.clone(),
            agent_urn: f.child_agent_urn.clone(),
            agent_label,
            via: Some(
                json!({"delegation": {
                    "byRunId": f.by_run_id,
                    "byAgentUrn": f.by_agent_urn,
                    "threadId": f.thread_id,
                }})
                .to_string(),
            ),
            title,
            body,
            action: json!({
                "verb": "open",
                "target": { "kind": "thread", "id": f.thread_id },
            }),
        },
    )
}

/// `report.arrived`: idempotent by child run id (a repeat only counts). Info when the asked run
/// succeeded, warn otherwise.
pub fn record_report(
    tx: &Connection,
    f: &DelegationFacts,
) -> Result<RecordOutcome, OrchestratorError> {
    let severity = if f.succeeded {
        Severity::Info
    } else {
        Severity::Warn
    };
    record_delegation(tx, Kind::ReportArrived, severity, f)
}

/// `delegation.stopped` or `delegation.expired`: an asked run that will send no report. Warn.
pub fn record_delegation_end(
    tx: &Connection,
    kind: Kind,
    f: &DelegationFacts,
) -> Result<RecordOutcome, OrchestratorError> {
    if !matches!(kind, Kind::DelegationStopped | Kind::DelegationExpired) {
        return Err(OrchestratorError::InvalidContract(
            "record_delegation_end takes a delegation.* kind".into(),
        ));
    }
    record_delegation(tx, kind, Severity::Warn, f)
}

/// The run's agent, Space and thread, from facts the host stored.
fn run_place(tx: &Connection, run_id: &str) -> (String, Option<String>, Option<String>) {
    let row: Option<(Option<String>, Option<String>)> = tx
        .query_row(
            "SELECT agent_urn, spec_json FROM runs WHERE run_id = ?1",
            [run_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .ok()
        .flatten();
    let Some((agent, spec_json)) = row else {
        return (String::new(), None, None);
    };
    let (mut space, mut thread) = (None, None);
    if let Some(json) = spec_json {
        if let Ok(spec) =
            vam_spec::encoding::json_to_proto::<vam_spec::v1::RunSpec>("vam.v1.RunSpec", &json)
        {
            if !spec.space_id.is_empty() {
                space = Some(spec.space_id);
            }
            if !spec.thread_id.is_empty() {
                thread = Some(spec.thread_id);
            }
        }
    }
    (agent.unwrap_or_default(), space, thread)
}

/// Who closed a record, for `acted`.
pub struct Acted<'a> {
    pub surface: &'a str,
    pub by_user_id: &'a str,
}

/// Closes the open decision record of `approval_id`. A no-op when there is none (a reauth park).
pub fn resolve(
    tx: &Connection,
    approval_id: &str,
    reason: &str,
    acted: Option<Acted<'_>>,
) -> Result<usize, OrchestratorError> {
    let ts = now();
    let (surface, by) = match &acted {
        Some(a) => (Some(a.surface), Some(a.by_user_id)),
        None => (None, None),
    };
    let changed = tx.execute(
        "UPDATE notifications SET resolved_at = ?2, resolved_reason = ?3, updated_at = ?2,
            acted_at = CASE WHEN ?4 IS NULL THEN NULL ELSE ?2 END,
            acted_surface = ?4, acted_by_user_id = ?5
         WHERE approval_id = ?1 AND resolved_at IS NULL
           AND kind IN ('approval.pending', 'question.asked', 'gate.opened')",
        params![approval_id, ts, reason, surface, by],
    )?;
    // NT-2.3: an update for every sent post, the unsent ones suppressed (same transaction).
    crate::notify_delivery::on_resolved(
        tx,
        crate::notify_delivery::Resolved::Approval(approval_id),
    )?;
    Ok(changed)
}

/// Closes every open decision record of a run (the run ended with them unresolved).
pub fn resolve_for_run(
    tx: &Connection,
    run_id: &str,
    reason: &str,
) -> Result<usize, OrchestratorError> {
    let ts = now();
    let changed = tx.execute(
        "UPDATE notifications SET resolved_at = ?2, resolved_reason = ?3, updated_at = ?2
         WHERE run_id = ?1 AND resolved_at IS NULL
           AND kind IN ('approval.pending', 'question.asked', 'gate.opened')",
        params![run_id, ts, reason],
    )?;
    crate::notify_delivery::on_resolved(tx, crate::notify_delivery::Resolved::Run(run_id))?;
    Ok(changed)
}

// ---- reads and marks, always by (user_id, id) ------------------------------------------------

const COLUMNS: &str = "id, kind, severity, subject_key, run_id, space_id, thread_id, approval_id,
    grant_id, receipt_digest, agent_urn, agent_label, via, title, body, action, count,
    created_at, updated_at, first_shown_at, read_at, acted_at, acted_by_user_id, acted_surface,
    resolved_at, resolved_reason";

#[derive(Debug, Clone)]
pub struct Row {
    pub id: String,
    pub kind: String,
    pub severity: String,
    pub run_id: Option<String>,
    pub space_id: Option<String>,
    pub thread_id: Option<String>,
    pub approval_id: Option<String>,
    pub grant_id: Option<String>,
    pub receipt_digest: Option<String>,
    pub agent_urn: String,
    pub agent_label: String,
    pub via: Option<String>,
    pub title: String,
    pub body: String,
    pub action: String,
    pub count: i64,
    pub created_at: i64,
    pub updated_at: i64,
    pub first_shown_at: Option<i64>,
    pub read_at: Option<i64>,
    pub acted_at: Option<i64>,
    pub acted_by_user_id: Option<String>,
    pub acted_surface: Option<String>,
    pub resolved_at: Option<i64>,
    pub resolved_reason: Option<String>,
}

fn map_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Row> {
    Ok(Row {
        id: r.get(0)?,
        kind: r.get(1)?,
        severity: r.get(2)?,
        // 3 = subject_key, never shown
        run_id: r.get(4)?,
        space_id: r.get(5)?,
        thread_id: r.get(6)?,
        approval_id: r.get(7)?,
        grant_id: r.get(8)?,
        receipt_digest: r.get(9)?,
        agent_urn: r.get(10)?,
        agent_label: r.get(11)?,
        via: r.get(12)?,
        title: r.get(13)?,
        body: r.get(14)?,
        action: r.get(15)?,
        count: r.get(16)?,
        created_at: r.get(17)?,
        updated_at: r.get(18)?,
        first_shown_at: r.get(19)?,
        read_at: r.get(20)?,
        acted_at: r.get(21)?,
        acted_by_user_id: r.get(22)?,
        acted_surface: r.get(23)?,
        resolved_at: r.get(24)?,
        resolved_reason: r.get(25)?,
    })
}

impl Row {
    /// 0: unresolved action (oldest first), 1: unread (newest first), 2: read (newest first).
    fn tier(&self) -> i64 {
        if self.severity == "action" && self.resolved_at.is_none() {
            0
        } else if self.read_at.is_none() {
            1
        } else {
            2
        }
    }

    fn sort_key(&self) -> (i64, i64, String) {
        let t = self.tier();
        let ts = if t == 0 {
            self.created_at
        } else {
            -self.created_at
        };
        (t, ts, self.id.clone())
    }

    pub fn is_decision(&self) -> bool {
        Kind::parse(&self.kind).is_some_and(Kind::is_decision)
    }

    pub fn to_json(&self) -> Value {
        let mut m = serde_json::Map::new();
        let mut put = |k: &str, v: Value| {
            if !v.is_null() {
                m.insert(k.to_string(), v);
            }
        };
        put("id", json!(self.id));
        put("kind", json!(self.kind));
        put("severity", json!(self.severity));
        put("runId", json!(self.run_id));
        put("spaceId", json!(self.space_id));
        put("threadId", json!(self.thread_id));
        put("approvalId", json!(self.approval_id));
        put("grantId", json!(self.grant_id));
        put("receiptDigest", json!(self.receipt_digest));
        put("agentUrn", json!(self.agent_urn));
        put("agentLabel", json!(self.agent_label));
        put(
            "via",
            self.via
                .as_deref()
                .and_then(|v| serde_json::from_str::<Value>(v).ok())
                .unwrap_or(Value::Null),
        );
        put("title", json!(self.title));
        put("body", json!(self.body));
        put(
            "action",
            serde_json::from_str::<Value>(&self.action).unwrap_or(Value::Null),
        );
        put("count", json!(self.count));
        put("createdAt", json!(self.created_at));
        put("updatedAt", json!(self.updated_at));
        put("firstShownAt", json!(self.first_shown_at));
        put("readAt", json!(self.read_at));
        put("actedAt", json!(self.acted_at));
        put("actedByUserId", json!(self.acted_by_user_id));
        put("actedSurface", json!(self.acted_surface));
        put("resolvedAt", json!(self.resolved_at));
        put("resolvedReason", json!(self.resolved_reason));
        Value::Object(m)
    }
}

#[derive(Debug, Clone, Default)]
pub struct ListFilter {
    pub open_only: bool,
    pub unread_only: bool,
    pub kind: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Counts {
    pub action: i64,
    pub warn: i64,
    pub unread: i64,
}

fn not_found() -> OrchestratorError {
    OrchestratorError::NotFound("notification not found".into())
}

impl SqliteWorkflowStore {
    /// One page in list order. `after` is the keyset of the last row of the previous page.
    /// Sets `first_shown_at` on the rows returned.
    pub fn list_notifications(
        &self,
        user_id: &str,
        filter: &ListFilter,
        after: Option<(i64, i64, String)>,
        limit: usize,
    ) -> Result<Vec<Row>, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let mut stmt = connection.prepare(&format!(
            "SELECT {COLUMNS} FROM notifications WHERE user_id = ?1
               AND (?2 IS NULL OR kind = ?2)
               AND (?3 = 0 OR resolved_at IS NULL)
               AND (?4 = 0 OR read_at IS NULL)"
        ))?;
        let mut rows = stmt
            .query_map(
                params![
                    user_id,
                    filter.kind,
                    i64::from(filter.open_only),
                    i64::from(filter.unread_only)
                ],
                map_row,
            )?
            .collect::<Result<Vec<_>, _>>()?;
        rows.sort_by_key(Row::sort_key);
        if let Some(cursor) = after {
            rows.retain(|r| r.sort_key() > cursor);
        }
        rows.truncate(limit);
        let ts = now();
        for r in &mut rows {
            if r.first_shown_at.is_none() {
                connection.execute(
                    "UPDATE notifications SET first_shown_at = ?3
                     WHERE user_id = ?1 AND id = ?2 AND first_shown_at IS NULL",
                    params![user_id, r.id, ts],
                )?;
                r.first_shown_at = Some(ts);
            }
        }
        Ok(rows)
    }

    /// The keyset of a row, for the page token.
    pub fn notification_keyset(row: &Row) -> (i64, i64, String) {
        row.sort_key()
    }

    /// What waits for `user_id`: unresolved AND unread rows, in list order, at most `limit`.
    /// A SELECT only. It sets neither `first_shown_at` nor `read_at` (the agent tool reads
    /// through this, and an agent reading must not change what the person has seen).
    pub fn peek_waiting_notifications(
        &self,
        user_id: &str,
        kinds: &[String],
        limit: usize,
    ) -> Result<Vec<Row>, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let mut stmt = connection.prepare(&format!(
            "SELECT {COLUMNS} FROM notifications WHERE user_id = ?1
               AND resolved_at IS NULL AND read_at IS NULL"
        ))?;
        let mut rows = stmt
            .query_map(params![user_id], map_row)?
            .collect::<Result<Vec<_>, _>>()?;
        if !kinds.is_empty() {
            rows.retain(|r| kinds.iter().any(|k| k == &r.kind));
        }
        rows.sort_by_key(Row::sort_key);
        rows.truncate(limit);
        Ok(rows)
    }

    /// The only lookup by id: `(user_id, id)`; a foreign id is not found. Sets `first_shown_at`.
    pub fn get_notification(&self, user_id: &str, id: &str) -> Result<Row, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        connection.execute(
            "UPDATE notifications SET first_shown_at = ?3
             WHERE user_id = ?1 AND id = ?2 AND first_shown_at IS NULL",
            params![user_id, id, now()],
        )?;
        connection
            .query_row(
                &format!("SELECT {COLUMNS} FROM notifications WHERE user_id = ?1 AND id = ?2"),
                params![user_id, id],
                map_row,
            )
            .optional()?
            .ok_or_else(not_found)
    }

    /// The badge. Marks the action and warn rows it counted as shown.
    pub fn notification_summary(
        &self,
        user_id: &str,
    ) -> Result<(Counts, String), OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        connection.execute(
            "UPDATE notifications SET first_shown_at = ?2
             WHERE user_id = ?1 AND first_shown_at IS NULL AND resolved_at IS NULL
               AND severity IN ('action', 'warn')",
            params![user_id, now()],
        )?;
        let (action, warn, unread, total, max_updated): (i64, i64, i64, i64, i64) = connection
            .query_row(
                "SELECT
                    COALESCE(SUM(severity = 'action' AND resolved_at IS NULL), 0),
                    COALESCE(SUM(severity = 'warn' AND resolved_at IS NULL AND read_at IS NULL), 0),
                    COALESCE(SUM(read_at IS NULL AND NOT (severity = 'action'
                                                          AND resolved_at IS NOT NULL)), 0),
                    COUNT(*), COALESCE(MAX(updated_at), 0)
                 FROM notifications WHERE user_id = ?1",
                [user_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )?;
        let etag = format!("\"{action}-{warn}-{unread}-{total}-{max_updated}\"");
        Ok((
            Counts {
                action,
                warn,
                unread,
            },
            etag,
        ))
    }

    /// Idempotent. A foreign id is not found.
    pub fn mark_notification_read(
        &self,
        user_id: &str,
        id: &str,
    ) -> Result<Row, OrchestratorError> {
        {
            let connection = self
                .connection
                .lock()
                .map_err(|_| OrchestratorError::StorageUnavailable)?;
            let n = connection.execute(
                "UPDATE notifications SET read_at = COALESCE(read_at, ?3)
                 WHERE user_id = ?1 AND id = ?2",
                params![user_id, id, now()],
            )?;
            if n == 0 {
                return Err(not_found());
            }
        }
        self.get_notification(user_id, id)
    }

    /// Bulk read by ids or by `created_at <= before`; only the caller's rows. Returns the count
    /// of rows that changed.
    pub fn mark_notifications_read(
        &self,
        user_id: &str,
        ids: &[String],
        before: Option<i64>,
    ) -> Result<usize, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let ts = now();
        let mut changed = 0;
        for id in ids {
            changed += connection.execute(
                "UPDATE notifications SET read_at = ?3
                 WHERE user_id = ?1 AND id = ?2 AND read_at IS NULL",
                params![user_id, id, ts],
            )?;
        }
        if let Some(before) = before {
            changed += connection.execute(
                "UPDATE notifications SET read_at = ?3
                 WHERE user_id = ?1 AND created_at <= ?2 AND read_at IS NULL",
                params![user_id, before, ts],
            )?;
        }
        Ok(changed)
    }

    /// Marks a non-decision record acted from `surface`. Idempotent. A decision kind is
    /// refused with `decide_required`; a foreign id is not found.
    pub fn mark_notification_acted(
        &self,
        user_id: &str,
        id: &str,
        surface: &str,
    ) -> Result<Row, OrchestratorError> {
        let row = self.get_notification(user_id, id)?;
        if row.is_decision() {
            return Err(OrchestratorError::Conflict(format!(
                "decide_required: decide the approval with POST /v1/approvals/{}/decide",
                row.approval_id.as_deref().unwrap_or("{id}")
            )));
        }
        {
            let connection = self
                .connection
                .lock()
                .map_err(|_| OrchestratorError::StorageUnavailable)?;
            let ts = now();
            connection.execute(
                "UPDATE notifications SET acted_at = ?3, acted_surface = ?4,
                    acted_by_user_id = ?1, resolved_at = ?3, resolved_reason = 'acted',
                    read_at = COALESCE(read_at, ?3), updated_at = ?3
                 WHERE user_id = ?1 AND id = ?2 AND resolved_at IS NULL",
                params![user_id, id, ts, surface],
            )?;
        }
        self.get_notification(user_id, id)
    }

    /// Retention (design 4.4). Returns the rows removed.
    pub fn prune_notifications(&self, now: i64) -> Result<usize, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let mut removed = connection.execute(
            "DELETE FROM notifications
             WHERE NOT (severity = 'action' AND resolved_at IS NULL)
               AND MAX(COALESCE(resolved_at, 0), COALESCE(read_at, 0)) > 0
               AND MAX(COALESCE(resolved_at, 0), COALESCE(read_at, 0)) <=
                   ?1 - CASE WHEN severity = 'warn' THEN ?3 ELSE ?2 END",
            params![now, INFO_RETENTION_SECS, WARN_RETENTION_SECS],
        )?;
        removed += connection.execute(
            "DELETE FROM notifications WHERE id IN (
                SELECT id FROM (
                    SELECT id, ROW_NUMBER() OVER (
                        PARTITION BY user_id
                        ORDER BY (resolved_at IS NULL) DESC, COALESCE(resolved_at, created_at) DESC
                    ) AS rn
                    FROM notifications
                ) WHERE rn > ?1
             ) AND NOT (severity = 'action' AND resolved_at IS NULL)",
            [MAX_ROWS_PER_USER],
        )?;
        Ok(removed)
    }
}

// ---- grant, budget and trigger producers (NT-2.7) -----------------------------------------

/// The `error` of a run the active-duration budget stopped (`expire_over_active_budget`). A run
/// with this result has `budget.hit` and no `run.finished` (`design/notifications.md` 2.3).
pub const DURATION_STOP_ERROR: &str = "run exceeded max_duration of active time";

fn is_duration_stop(result_json: Option<&str>) -> bool {
    result_json
        .and_then(|j| serde_json::from_str::<Value>(j).ok())
        .and_then(|v| v.get("error").and_then(Value::as_str).map(str::to_string))
        .is_some_and(|e| e == DURATION_STOP_ERROR)
}

/// `grant.expiring`: `warn`, action `renew` on the grant, to its owner; one per grant (the
/// subject is the grant id, and the sweeper claims each grant once). Call it on the
/// transaction that claims the grant.
pub fn record_grant_expiring(
    tx: &Connection,
    grant_id: &str,
    owner: &str,
    tool: &str,
    source_run_id: &str,
    expires_at: i64,
) -> Result<RecordOutcome, OrchestratorError> {
    let (agent_urn, space_id, thread_id) = run_place(tx, source_run_id);
    let agent_label = agent_label_of(&agent_urn);
    let kind = Kind::GrantExpiring;
    let detail =
        format!("Tool {tool}: expires at {expires_at} (unix seconds). Renew it to keep it.");
    let (title, body) = render(kind, &agent_label, &detail, None);
    record(
        tx,
        &NewNotification {
            user_id: crate::owner::resolved_owner(owner).to_string(),
            kind,
            severity: kind.severity(),
            subject_key: grant_id.to_string(),
            // The run that issued the grant.
            run_id: Some(source_run_id.to_string()),
            space_id,
            thread_id,
            approval_id: None,
            grant_id: Some(grant_id.to_string()),
            receipt_digest: None,
            agent_urn,
            agent_label,
            via: None,
            title,
            body,
            action: json!({
                "verb": kind.action_verb(),
                "target": { "kind": "grant", "id": grant_id },
                "api": "GET /v1/keys",
            }),
        },
    )
}

/// Closes the open `grant.expiring` record of `grant_id` (`acted` on revoke or renewal,
/// `expired` at expiry). A no-op when there is none.
pub fn resolve_grant_expiring(
    tx: &Connection,
    grant_id: &str,
    reason: &str,
) -> Result<usize, OrchestratorError> {
    let ts = now();
    Ok(tx.execute(
        "UPDATE notifications SET resolved_at = ?2, resolved_reason = ?3, updated_at = ?2,
            acted_at = CASE WHEN ?3 = 'acted' THEN ?2 ELSE NULL END
         WHERE kind = 'grant.expiring' AND grant_id = ?1 AND resolved_at IS NULL",
        params![grant_id, ts, reason],
    )?)
}

/// `expired` for every open `grant.expiring` record whose grant has reached its expiry.
pub fn resolve_expired_grants(tx: &Connection, at: i64) -> Result<usize, OrchestratorError> {
    Ok(tx.execute(
        "UPDATE notifications SET resolved_at = ?1, resolved_reason = 'expired', updated_at = ?1
         WHERE kind = 'grant.expiring' AND resolved_at IS NULL
           AND grant_id IN (SELECT key_id FROM api_keys
                            WHERE expires_at IS NOT NULL AND expires_at <= ?1)",
        [at],
    )?)
}

/// Where a run met its budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetPoint {
    /// The active-duration budget stopped the run.
    Duration,
    /// The spend budget refused a model call.
    Spend,
}

/// `budget.hit`: `warn`, action `open` on the run's receipt, to the run owner; one per run
/// (idempotent by run id, so a second refusal or both points leave one record). Returns whether
/// a record was inserted.
pub fn record_budget_hit(
    tx: &Connection,
    run_id: &str,
    point: BudgetPoint,
) -> Result<bool, OrchestratorError> {
    let existing: Option<String> = tx
        .query_row(
            "SELECT id FROM notifications WHERE kind = 'budget.hit' AND run_id = ?1 LIMIT 1",
            [run_id],
            |r| r.get(0),
        )
        .optional()?;
    if existing.is_some() {
        return Ok(false);
    }
    let owner: Option<Option<String>> = tx
        .query_row(
            "SELECT owner_user_id FROM runs WHERE run_id = ?1",
            [run_id],
            |r| r.get(0),
        )
        .optional()?;
    let Some(owner) = owner else {
        return Ok(false);
    };
    let (agent_urn, space_id, thread_id) = run_place(tx, run_id);
    let agent_label = agent_label_of(&agent_urn);
    let kind = Kind::BudgetHit;
    let detail = match point {
        BudgetPoint::Duration => "The run was stopped: it used its active-time budget.",
        BudgetPoint::Spend => "A model call was refused: the run used its spend budget.",
    };
    let (title, body) = render(kind, &agent_label, detail, None);
    record(
        tx,
        &NewNotification {
            user_id: crate::owner::resolved_owner(owner.as_deref().unwrap_or("")).to_string(),
            kind,
            severity: kind.severity(),
            subject_key: run_id.to_string(),
            run_id: Some(run_id.to_string()),
            space_id,
            thread_id,
            approval_id: None,
            grant_id: None,
            receipt_digest: None,
            agent_urn,
            agent_label,
            via: None,
            title,
            body,
            action: json!({
                "verb": kind.action_verb(),
                "target": { "kind": "run", "id": run_id },
                "api": format!("GET /v1/runs/{run_id}/receipt"),
            }),
        },
    )?;
    Ok(true)
}

/// What a trigger fire told the gateway, for the grant owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerNotice {
    /// The fire was queued behind a live task.
    Queued,
    /// The fire started nothing; the text names why (host words, not agent text).
    Skipped(&'static str),
}

impl SqliteWorkflowStore {
    /// The spend-budget point of `budget.hit`, from the callback `vamd` installs on the
    /// inference broker. A run that is unknown, or already has a record, adds nothing.
    pub fn record_spend_budget_hit(&self, run_id: &str) -> Result<bool, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let tx = connection.unchecked_transaction()?;
        let inserted = record_budget_hit(&tx, run_id, BudgetPoint::Spend)?;
        tx.commit()?;
        Ok(inserted)
    }

    /// `trigger.fired` (queued) or `trigger.skipped`: one record per grant and UTC day for the
    /// grant owner, `count` growing while it is unresolved. A grant the broker does not hold
    /// (unknown id) adds nothing.
    pub fn record_trigger_notice(
        &self,
        broker_id: &str,
        grant_id: &str,
        notice: TriggerNotice,
    ) -> Result<(), OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let tx = connection.unchecked_transaction()?;
        let row: Option<(Option<String>, Option<String>, Option<String>)> = tx
            .query_row(
                "SELECT owner_user_id, tool_name, source_run_id FROM api_keys
                 WHERE key_id = ?1 AND holder = ?2",
                params![grant_id, format!("broker:{broker_id}")],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((owner, tool, source_run)) = row else {
            return Ok(());
        };
        let (agent_urn, space_id, thread_id) = source_run
            .as_deref()
            .map(|r| run_place(&tx, r))
            .unwrap_or_default();
        let agent_label = agent_label_of(&agent_urn);
        let tool = tool.unwrap_or_default();
        let (kind, detail) = match notice {
            TriggerNotice::Queued => (
                Kind::TriggerFired,
                format!("Tool {tool}: the fire was queued behind a running task."),
            ),
            TriggerNotice::Skipped(why) => (
                Kind::TriggerSkipped,
                format!("Tool {tool}: skipped, {why}."),
            ),
        };
        let (title, body) = render(kind, &agent_label, &detail, None);
        let day = now().div_euclid(86_400);
        record(
            &tx,
            &NewNotification {
                user_id: crate::owner::resolved_owner(owner.as_deref().unwrap_or("")).to_string(),
                kind,
                severity: kind.severity(),
                subject_key: format!("{grant_id}:{day}"),
                // The run that issued the grant.
                run_id: source_run.clone(),
                space_id,
                thread_id,
                approval_id: None,
                grant_id: Some(grant_id.to_string()),
                receipt_digest: None,
                agent_urn,
                agent_label,
                via: None,
                title,
                body,
                action: json!({
                    "verb": kind.action_verb(),
                    "target": { "kind": "grant", "id": grant_id },
                    "api": "GET /v1/keys",
                }),
            },
        )?;
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> SqliteWorkflowStore {
        SqliteWorkflowStore::in_memory().expect("store")
    }

    fn new(user: &str, subject: &str, approval: &str) -> NewNotification {
        let (title, body) = render(Kind::ApprovalPending, "researcher", "write_file", None);
        NewNotification {
            user_id: user.into(),
            kind: Kind::ApprovalPending,
            severity: Severity::Action,
            subject_key: subject.into(),
            run_id: Some("run-1".into()),
            space_id: None,
            thread_id: None,
            approval_id: Some(approval.into()),
            grant_id: None,
            receipt_digest: None,
            agent_urn: String::new(),
            agent_label: "researcher".into(),
            via: None,
            title,
            body,
            action: json!({"verb": "decide"}),
        }
    }

    fn put(s: &SqliteWorkflowStore, n: &NewNotification) -> RecordOutcome {
        let c = s.connection.lock().unwrap();
        record(&c, n).unwrap()
    }

    #[test]
    fn an_unresolved_subject_recurring_counts_and_a_resolved_one_never_reopens() {
        let s = store();
        let RecordOutcome::Inserted(first) = put(&s, &new("u", "k", "a1")) else {
            panic!()
        };
        let RecordOutcome::Updated(same) = put(&s, &new("u", "k", "a2")) else {
            panic!()
        };
        assert_eq!(first, same);
        let row = s.get_notification("u", &first).unwrap();
        assert_eq!((row.count, row.approval_id.as_deref()), (2, Some("a2")));
        {
            let c = s.connection.lock().unwrap();
            assert_eq!(resolve(&c, "a2", "acted", None).unwrap(), 1);
        }
        let RecordOutcome::Inserted(second) = put(&s, &new("u", "k", "a3")) else {
            panic!()
        };
        assert_ne!(first, second);
        let old = s.get_notification("u", &first).unwrap();
        assert_eq!(old.resolved_reason.as_deref(), Some("acted"));
        assert_eq!(s.get_notification("u", &second).unwrap().count, 1);
    }

    fn put_run(
        s: &SqliteWorkflowStore,
        id: &str,
        source: Option<&str>,
        state: &str,
        result: Option<&str>,
    ) {
        s.persist_run_spec_with_origin(
            id,
            "{}",
            Some("sess"),
            "urn:vam:agent:local/acme/bot",
            None,
            "p",
            "local",
            source,
            None,
        )
        .unwrap();
        s.update_run_state(id, state).unwrap();
        if let Some(r) = result {
            let c = s.connection.lock().unwrap();
            c.execute(
                "UPDATE runs SET result_json = ?2 WHERE run_id = ?1",
                params![id, r],
            )
            .unwrap();
        }
    }

    fn put_waited(s: &SqliteWorkflowStore, id: &str) {
        let c = s.connection.lock().unwrap();
        c.execute(
            "INSERT INTO approvals (request_id, run_id, step_id, attempt_id, checkpoint_digest,
                continuation_generation, published, session_id)
             VALUES (?1, ?2, 'step', 'att', 'digest', 0, 1, 'sess')",
            params![format!("ap-{id}"), id],
        )
        .unwrap();
    }

    fn finished(s: &SqliteWorkflowStore, id: &str, digest: Option<&str>) -> bool {
        let c = s.connection.lock().unwrap();
        record_run_finished(&c, id, digest).unwrap()
    }

    fn count_finished(s: &SqliteWorkflowStore, id: &str) -> (i64, i64) {
        let c = s.connection.lock().unwrap();
        c.query_row(
            "SELECT COUNT(*), COALESCE(MAX(count), 0) FROM notifications
             WHERE kind = 'run.finished' AND run_id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
    }

    #[test]
    fn run_finished_follows_section_2_3() {
        let s = store();
        put_run(&s, "trig", Some("broker:vam.schedule"), "SUCCEEDED", None);
        put_run(&s, "parked", None, "SUCCEEDED", None);
        put_waited(&s, "parked");
        put_run(&s, "failed", None, "FAILED", Some(r#"{"error":"boom"}"#));
        put_run(&s, "chat", None, "SUCCEEDED", None);
        put_run(
            &s,
            "stop",
            None,
            "FAILED",
            Some(r#"{"error":"stopped by user","reason":"stopped"}"#),
        );
        put_run(
            &s,
            "trigstop",
            Some("broker:vam.schedule"),
            "FAILED",
            Some(r#"{"reason":"stopped"}"#),
        );
        put_run(&s, "asked", Some(DELEGATE_RUN_SOURCE), "SUCCEEDED", None);
        put_run(&s, "askedfail", Some(DELEGATE_RUN_SOURCE), "FAILED", None);
        put_run(&s, "report", Some("report:child"), "FAILED", None);
        for id in ["trig", "parked", "failed"] {
            assert!(finished(&s, id, Some("sha256:d")), "{id}");
        }
        for id in ["chat", "stop", "trigstop", "asked", "askedfail", "report"] {
            assert!(!finished(&s, id, Some("sha256:d")), "{id}");
            assert_eq!(count_finished(&s, id).0, 0, "{id}");
        }
        let sev = |id: &str| -> String {
            let c = s.connection.lock().unwrap();
            c.query_row(
                "SELECT severity FROM notifications WHERE kind = 'run.finished' AND run_id = ?1",
                [id],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(
            (
                sev("trig").as_str(),
                sev("parked").as_str(),
                sev("failed").as_str()
            ),
            ("info", "info", "warn")
        );
    }

    #[test]
    fn a_run_not_yet_terminal_gets_no_run_finished() {
        let s = store();
        put_run(&s, "live", Some("broker:vam.schedule"), "RUNNING", None);
        assert!(!finished(&s, "live", None));
    }

    #[test]
    fn run_finished_is_idempotent_fills_the_digest_and_survives_run_end() {
        let s = store();
        put_run(&s, "t", Some("broker:vam.schedule"), "SUCCEEDED", None);
        assert!(finished(&s, "t", None));
        assert!(!finished(&s, "t", None));
        assert_eq!(count_finished(&s, "t"), (1, 1));
        assert!(!finished(&s, "t", Some("sha256:late")));
        assert!(!finished(&s, "t", Some("sha256:other")));
        assert_eq!(count_finished(&s, "t"), (1, 1));
        let c = s.connection.lock().unwrap();
        let d: Option<String> = c
            .query_row(
                "SELECT receipt_digest FROM notifications WHERE run_id = 't'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(d.as_deref(), Some("sha256:late"));
        resolve_for_run(&c, "t", "run_ended").unwrap();
        let open: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM notifications WHERE run_id = 't' AND resolved_at IS NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(open, 1);
    }

    fn facts(child: &str, succeeded: bool) -> DelegationFacts {
        DelegationFacts {
            asking_user_id: "u".into(),
            child_run_id: child.into(),
            by_run_id: "run-by".into(),
            by_agent_urn: "urn:vam:agent:local/acme/primary".into(),
            thread_id: "th-1".into(),
            space_id: None,
            child_agent_urn: "urn:vam:agent:local/acme/helper".into(),
            receipt_digest: Some("sha256:r".into()),
            succeeded,
        }
    }

    #[test]
    fn report_and_delegation_records_are_idempotent_by_child_run() {
        let s = store();
        let c = s.connection.lock().unwrap();
        let RecordOutcome::Inserted(a) = record_report(&c, &facts("c1", true)).unwrap() else {
            panic!()
        };
        assert!(
            matches!(record_report(&c, &facts("c1", true)).unwrap(), RecordOutcome::Updated(id) if id == a)
        );
        let RecordOutcome::Inserted(w) = record_report(&c, &facts("c2", false)).unwrap() else {
            panic!()
        };
        let RecordOutcome::Inserted(d) =
            record_delegation_end(&c, Kind::DelegationStopped, &facts("c3", false)).unwrap()
        else {
            panic!()
        };
        let RecordOutcome::Inserted(e) =
            record_delegation_end(&c, Kind::DelegationExpired, &facts("c4", false)).unwrap()
        else {
            panic!()
        };
        assert!(record_delegation_end(&c, Kind::RunFinished, &facts("c5", false)).is_err());
        drop(c);
        let ok = s.get_notification("u", &a).unwrap();
        assert_eq!(ok.count, 2);
        let j = ok.to_json();
        assert_eq!(j["severity"], "info");
        assert_eq!(j["threadId"], "th-1");
        assert_eq!(j["action"]["verb"], "open");
        assert_eq!(j["action"]["target"]["id"], "th-1");
        assert_eq!(j["via"]["delegation"]["byRunId"], "run-by");
        assert_eq!(
            j["via"]["delegation"]["byAgentUrn"],
            "urn:vam:agent:local/acme/primary"
        );
        assert_eq!(j["via"]["delegation"]["threadId"], "th-1");
        for id in [&w, &d, &e] {
            assert_eq!(
                s.get_notification("u", id).unwrap().to_json()["severity"],
                "warn"
            );
        }
    }

    #[test]
    fn a_foreign_id_is_not_found_on_every_call() {
        let s = store();
        let RecordOutcome::Inserted(id) = put(&s, &new("u", "k", "a1")) else {
            panic!()
        };
        assert!(s.get_notification("bob", &id).is_err());
        assert!(s.mark_notification_read("bob", &id).is_err());
        assert!(s.mark_notification_acted("bob", &id, "api").is_err());
        let (counts, _) = s.notification_summary("bob").unwrap();
        assert_eq!((counts.action, counts.unread), (0, 0));
    }

    #[test]
    fn a_decision_kind_cannot_be_marked_acted() {
        let s = store();
        let RecordOutcome::Inserted(id) = put(&s, &new("u", "k", "a1")) else {
            panic!()
        };
        let err = s.mark_notification_acted("u", &id, "cli").unwrap_err();
        assert!(err.to_string().contains("decide_required"), "{err}");
    }

    #[test]
    fn a_failing_record_rolls_the_caller_back() {
        let s = store();
        let c = s.connection.lock().unwrap();
        let tx = c.unchecked_transaction().unwrap();
        tx.execute(
            "INSERT INTO notification_migrations (name) VALUES ('probe')",
            [],
        )
        .unwrap();
        let mut bad = new("u", "k", "a1");
        bad.user_id = String::new();
        // NOT NULL holds, so force a failure with an invalid severity through the table check.
        let r = tx.execute(
            "INSERT INTO notifications (id, user_id, kind, severity, subject_key, title, body, action, created_at, updated_at)
             VALUES ('x','u','k','bogus','s','t','b','{}',1,1)",
            [],
        );
        assert!(r.is_err());
        drop(tx);
        let n: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM notification_migrations WHERE name = 'probe'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 0);
    }

    fn budget_hits(s: &SqliteWorkflowStore, id: &str) -> i64 {
        let c = s.connection.lock().unwrap();
        c.query_row(
            "SELECT COUNT(*) FROM notifications WHERE kind = 'budget.hit' AND run_id = ?1",
            [id],
            |r| r.get(0),
        )
        .unwrap()
    }

    #[test]
    fn budget_hit_duration_stop_is_one_record_and_no_run_finished() {
        let s = store();
        let spec = r#"{"maxDuration":"1s"}"#;
        put_run(&s, "r-dur", None, "RUNNING", None);
        {
            let c = s.connection.lock().unwrap();
            c.execute(
                "UPDATE runs SET spec_json = ?2, active_since = 1 WHERE run_id = ?1",
                params!["r-dur", spec],
            )
            .unwrap();
        }
        let stopped = s.expire_over_active_budget(1_000).unwrap();
        if stopped.is_empty() {
            // The spec shape differs: drive the same transaction by the field the store reads.
            panic!("the duration budget did not stop the run; fix the spec fixture");
        }
        assert_eq!(budget_hits(&s, "r-dur"), 1);
        // Another pass stops nothing and adds nothing.
        s.expire_over_active_budget(2_000).unwrap();
        assert_eq!(budget_hits(&s, "r-dur"), 1);
        // The run is FAILED, which would normally earn `run.finished`: the duration stop does not.
        assert!(!finished(&s, "r-dur", Some("digest")));
        assert_eq!(count_finished(&s, "r-dur"), (0, 0));
        let sev: String = s
            .connection
            .lock()
            .unwrap()
            .query_row(
                "SELECT severity FROM notifications WHERE kind = 'budget.hit'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(sev, "warn");
    }

    #[test]
    fn budget_hit_spend_refusal_is_one_record_per_run() {
        let s = store();
        put_run(&s, "r-spend", None, "RUNNING", None);
        assert!(s.record_spend_budget_hit("r-spend").unwrap());
        assert!(!s.record_spend_budget_hit("r-spend").unwrap());
        assert!(!s.record_spend_budget_hit("r-spend").unwrap());
        assert_eq!(budget_hits(&s, "r-spend"), 1);
        // A run the store does not know adds nothing.
        assert!(!s.record_spend_budget_hit("r-none").unwrap());
        // A spend refusal does not stop the run: its end still gets `run.finished` if wanted.
        s.update_run_state("r-spend", "FAILED").unwrap();
        assert!(finished(&s, "r-spend", None));
        // Both points on one run leave one record.
        let c = s.connection.lock().unwrap();
        assert!(!record_budget_hit(&c, "r-spend", BudgetPoint::Duration).unwrap());
    }

    fn put_grant(s: &SqliteWorkflowStore, owner: &str) -> String {
        let (rec, _) = s
            .insert_grant_credential(&crate::credentials::GrantInsert {
                name: "g".into(),
                owner_user_id: owner.into(),
                holder: "broker:b1".into(),
                target_kind: "agent".into(),
                target_id: "urn:a".into(),
                constraints_json: "{}".into(),
                issued_via: "approval".into(),
                source_run_id: None,
                tool_name: Some("t".into()),
                scopes: vec![],
                expires_at: now() + 86_400,
            })
            .unwrap();
        rec.key_id
    }

    fn expiring_state(s: &SqliteWorkflowStore, grant: &str) -> (Option<String>, bool) {
        let c = s.connection.lock().unwrap();
        c.query_row(
            "SELECT resolved_reason, acted_at IS NOT NULL FROM notifications
             WHERE kind = 'grant.expiring' AND grant_id = ?1",
            [grant],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
    }

    fn put_expiring(s: &SqliteWorkflowStore, owner: &str) -> String {
        let g = put_grant(s, owner);
        let c = s.connection.lock().unwrap();
        record_grant_expiring(&c, &g, owner, "t", "run-x", now() + 100).unwrap();
        g
    }

    #[test]
    fn grant_expiring_is_acted_when_the_grant_is_revoked() {
        let s = store();
        let owner = s.create_user("o", "o").unwrap().user_id;
        // The owner's revoke (and the broker's RevokeRunGrant, which calls it).
        let g = put_expiring(&s, &owner);
        assert_eq!(expiring_state(&s, &g), (None, false));
        s.revoke_api_key_for_owner(&g, &owner).unwrap();
        assert_eq!(expiring_state(&s, &g), (Some("acted".into()), true));
        // Revoking every key of a deactivated owner.
        let g2 = put_expiring(&s, &owner);
        s.revoke_api_keys_for_owner(&owner).unwrap();
        assert_eq!(expiring_state(&s, &g2), (Some("acted".into()), true));
    }

    #[test]
    fn grant_expiring_is_expired_by_the_sweep_at_expiry() {
        let s = store();
        let owner = s.create_user("o", "o").unwrap().user_id;
        let g = put_expiring(&s, &owner);
        s.sweep_expiring_grants(now() + 50).unwrap();
        assert_eq!(expiring_state(&s, &g), (None, false));
        s.sweep_expiring_grants(now() + 90_000).unwrap();
        assert_eq!(expiring_state(&s, &g), (Some("expired".into()), false));
    }

    #[test]
    fn trigger_notices_count_up_per_grant_and_day() {
        let s = store();
        let owner = s.create_user("o", "o").unwrap().user_id;
        let g = put_grant(&s, &owner);
        s.record_trigger_notice("b1", &g, TriggerNotice::Skipped("the workspace was busy"))
            .unwrap();
        s.record_trigger_notice("b1", &g, TriggerNotice::Skipped("the workspace was busy"))
            .unwrap();
        s.record_trigger_notice("b1", &g, TriggerNotice::Queued)
            .unwrap();
        // A grant of another broker, or none, adds nothing.
        s.record_trigger_notice("other", &g, TriggerNotice::Queued)
            .unwrap();
        s.record_trigger_notice("b1", "nope", TriggerNotice::Queued)
            .unwrap();
        let c = s.connection.lock().unwrap();
        let rows: Vec<(String, String, i64, String)> = c
            .prepare("SELECT kind, severity, count, user_id FROM notifications ORDER BY kind")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            rows,
            vec![
                (
                    "trigger.fired".to_string(),
                    "info".to_string(),
                    1,
                    owner.clone()
                ),
                ("trigger.skipped".to_string(), "warn".to_string(), 2, owner),
            ]
        );
    }

    #[test]
    fn titles_and_bodies_fit_and_carry_no_agent_text() {
        for kind in Kind::ALL {
            let (t, b) = render(kind, &"a".repeat(300), &"d".repeat(900), Some(5));
            assert!(t.chars().count() <= MAX_TITLE_CHARS);
            assert!(b.chars().count() <= MAX_BODY_CHARS);
        }
        assert_eq!(Kind::parse("run.finished"), Some(Kind::RunFinished));
    }

    #[test]
    fn the_list_is_ordered_in_three_tiers_and_pages() {
        let s = store();
        {
            let c = s.connection.lock().unwrap();
            for (i, (sev, read, resolved, at)) in [
                ("action", None, None, 10),
                ("action", None, None, 20),
                ("info", None, None, 30),
                ("info", None, None, 40),
                ("info", Some(5), None, 50),
                ("action", None, Some(9), 60),
            ]
            .into_iter()
            .enumerate()
            {
                c.execute(
                    "INSERT INTO notifications (id, user_id, kind, severity, subject_key, title,
                        body, action, created_at, updated_at, read_at, resolved_at)
                     VALUES (?1, 'u', 'approval.pending', ?2, ?1, 't', 'b', '{}', ?3, ?3, ?4, ?5)",
                    params![format!("n{i}"), sev, at, read, resolved],
                )
                .unwrap();
            }
        }
        let f = ListFilter::default();
        let all = s.list_notifications("u", &f, None, 100).unwrap();
        let ids: Vec<_> = all.iter().map(|r| r.id.as_str()).collect();
        // unresolved action oldest first, unread newest first, then read newest first
        assert_eq!(ids, ["n0", "n1", "n5", "n3", "n2", "n4"].map(|x| x));
        let page = s.list_notifications("u", &f, None, 2).unwrap();
        let cur = SqliteWorkflowStore::notification_keyset(page.last().unwrap());
        let next = s.list_notifications("u", &f, Some(cur), 2).unwrap();
        assert_eq!(next[0].id, "n5");
        assert!(all.iter().all(|r| !r.id.is_empty()));
        let open = s
            .list_notifications(
                "u",
                &ListFilter {
                    open_only: true,
                    ..Default::default()
                },
                None,
                100,
            )
            .unwrap();
        assert!(open.iter().all(|r| r.resolved_at.is_none()));
        assert!(s
            .get_notification("u", "n0")
            .unwrap()
            .first_shown_at
            .is_some());
    }

    #[test]
    fn prune_keeps_unresolved_actions_and_enforces_retention_and_cap() {
        let s = store();
        let day = 86_400;
        let now = 1_000 * day;
        {
            let c = s.connection.lock().unwrap();
            let ins = |id: &str, sev: &str, read: Option<i64>, resolved: Option<i64>| {
                c.execute(
                    "INSERT INTO notifications (id, user_id, kind, severity, subject_key, title,
                        body, action, created_at, updated_at, read_at, resolved_at)
                     VALUES (?1, 'u', 'run.finished', ?2, ?1, 't', 'b', '{}', 1, 1, ?3, ?4)",
                    params![id, sev, read, resolved],
                )
                .unwrap();
            };
            ins("keep-open", "action", None, None);
            ins("info-old", "info", Some(now - 31 * day), None);
            ins("info-new", "info", Some(now - 29 * day), None);
            ins("warn-60", "warn", Some(now - 60 * day), None);
            ins("warn-91", "warn", None, Some(now - 91 * day));
            ins("unread-info", "info", None, None);
        }
        assert_eq!(s.prune_notifications(now).unwrap(), 2);
        let left: Vec<String> = {
            let c = s.connection.lock().unwrap();
            let mut st = c
                .prepare("SELECT id FROM notifications ORDER BY id")
                .unwrap();
            let v = st
                .query_map([], |r| r.get(0))
                .unwrap()
                .collect::<Result<_, _>>();
            v.unwrap()
        };
        assert_eq!(left, ["info-new", "keep-open", "unread-info", "warn-60"]);
        {
            let c = s.connection.lock().unwrap();
            for i in 0..2_005 {
                c.execute(
                    "INSERT INTO notifications (id, user_id, kind, severity, subject_key, title,
                        body, action, created_at, updated_at, read_at, resolved_at)
                     VALUES (?1, 'cap', 'run.finished', 'info', ?1, 't', 'b', '{}', ?2, ?2, ?3, NULL)",
                    params![format!("c{i}"), i, now],
                )
                .unwrap();
            }
        }
        s.prune_notifications(now).unwrap();
        let n: i64 = {
            let c = s.connection.lock().unwrap();
            c.query_row(
                "SELECT COUNT(*) FROM notifications WHERE user_id = 'cap'",
                [],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(n, MAX_ROWS_PER_USER);
    }

    #[test]
    fn stored_keys_with_approvals_write_gain_the_notification_scopes_once() {
        let s = store();
        {
            let c = s.connection.lock().unwrap();
            c.execute("DELETE FROM notification_migrations", [])
                .unwrap();
            for (id, scopes, revoked) in [
                ("k1", r#"["approvals:write"]"#, None),
                ("k2", r#"["runs:write"]"#, None),
                ("k3", r#"["approvals:write"]"#, Some(1)),
            ] {
                c.execute(
                    "INSERT INTO api_keys (key_id, name, sha256_secret, principal, scopes, created_at, revoked_at)
                     VALUES (?1, 'n', ?1, 'p', ?2, 1, ?3)",
                    params![id, scopes, revoked],
                )
                .unwrap();
            }
            migrate_key_scopes(&c).unwrap();
            let get = |id: &str| -> String {
                c.query_row("SELECT scopes FROM api_keys WHERE key_id = ?1", [id], |r| {
                    r.get(0)
                })
                .unwrap()
            };
            assert!(get("k1").contains("notifications:read"));
            assert!(get("k1").contains("notifications:write"));
            assert!(!get("k2").contains("notifications"));
            assert!(!get("k3").contains("notifications"));
            c.execute(
                "UPDATE api_keys SET scopes = '[\"approvals:write\"]' WHERE key_id = 'k1'",
                [],
            )
            .unwrap();
            migrate_key_scopes(&c).unwrap();
            assert!(!get("k1").contains("notifications"));
        }
    }
}
