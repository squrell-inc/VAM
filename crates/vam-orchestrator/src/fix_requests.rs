//! Fix requests: a member asks the person who can apply a fix a refusal named.
//!
//! `POST /v1/fix-requests` (`api/fix_requests.rs`) resolves the `decisionRef` of a stored
//! denial row (`denials.rs`), checks that the requester is the run's caller, and re-derives the
//! change, target and who from that row alone: nothing the client sent is read. Recipients are
//! recomputed at request time. One `fix_requests` row and one `fix.requested` notification per
//! recipient are written in one transaction.
//!
//! A request has no state in S1: no queue, no decision. One request per (requester, change,
//! fix target) per 24 hours; a repeat answers with the first one and notifies nobody.

use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde_json::{json, Value};
use vam_spec::v1::{Denial, DenialReason, FixChange, FixWho, SuggestedFix};

use crate::denials::{self, StoredDenial, Text};
use crate::error::OrchestratorError;
use crate::notifications::{self, Kind, NewNotification, MAX_BODY_CHARS, MAX_TITLE_CHARS};
use crate::workflow::SqliteWorkflowStore;

/// A repeat of (requester, change, target) inside this window answers the first request.
pub const DEDUPE_SECS: i64 = 24 * 60 * 60;
/// A `decisionRef` older than this is stale context and answers like a missing row.
pub const STALE_AFTER_SECS: i64 = 30 * 24 * 60 * 60;
/// New `ALLOW_HOST` requests one requester may file for one agent per [`DEDUPE_SECS`]: the host
/// is chosen by the guest, so distinct hosts must not become distinct admin notifications
/// without bound.
pub const HOST_REQUESTS_PER_DAY: i64 = 10;

pub(crate) fn initialize_schema(connection: &Connection) -> Result<(), OrchestratorError> {
    connection.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS fix_requests (
            id TEXT PRIMARY KEY NOT NULL,
            denial_id TEXT NOT NULL,
            requester_user_id TEXT NOT NULL,
            agent_urn TEXT NOT NULL DEFAULT '',
            run_id TEXT,
            change TEXT NOT NULL,
            target TEXT NOT NULL,
            who TEXT NOT NULL,
            created_at INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_fix_requests_dedupe
            ON fix_requests(requester_user_id, change, target, created_at);
        ",
    )?;
    Ok(())
}

// ---- what can be asked ------------------------------------------------------------------------

/// The (reason, change, who) triples a request may be filed for. Anything else (SELF fixes,
/// `REQUEST_AGENT_ACCESS`, which has its own route, an unknown value in a corrupt row) is
/// answered like a missing row.
pub fn askable(reason: DenialReason, change: FixChange, who: FixWho) -> bool {
    matches!(
        (reason, change, who),
        (
            DenialReason::PolicyRule,
            FixChange::AllowTool,
            FixWho::AgentOwner
        ) | (
            DenialReason::EgressRule,
            FixChange::AllowHost,
            FixWho::AgentOwner
        ) | (
            DenialReason::ConnectorDisabled,
            FixChange::EnableConnector,
            FixWho::OrgAdmin
        ) | (
            DenialReason::SecretMissing,
            FixChange::SetSecret,
            FixWho::OrgAdmin
        )
    )
}

/// The fix target the denial showed, rebuilt from the stored row: `{urn}#{tool or host}` for an
/// allow (an MCP permission target is kept in the row only and the denial carried the tool
/// name), the connector id or binding id otherwise.
pub fn fix_target_of(change: FixChange, agent_urn: &str, target: &str, subject: &str) -> String {
    match change {
        FixChange::AllowTool | FixChange::AllowHost => {
            let shown = if target.starts_with("mcp:") {
                subject
            } else {
                target
            };
            format!("{agent_urn}#{shown}")
        }
        _ => target.to_string(),
    }
}

/// A stored denial that a request may be filed for, with the target it will carry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Eligible {
    pub reason: DenialReason,
    pub change: FixChange,
    pub who: FixWho,
    pub target: String,
}

/// Whether `den` may be asked about at `now`, and for what. `None` for a stale row, an
/// unknown or unaskable (reason, change, who), an empty target, an allow with no agent, and a
/// host a fix target cannot carry.
pub fn eligible(den: &StoredDenial, now: i64) -> Option<Eligible> {
    if now.saturating_sub(den.created_at) > STALE_AFTER_SECS {
        return None;
    }
    let reason = DenialReason::from_str_name(&den.reason)?;
    let change = FixChange::from_str_name(&den.change)?;
    let who = FixWho::from_str_name(&den.who)?;
    if !askable(reason, change, who) {
        return None;
    }
    let target = fix_target_of(change, &den.agent_urn, &den.target, &den.subject);
    match change {
        FixChange::AllowTool | FixChange::AllowHost => {
            if den.agent_urn.is_empty() {
                return None;
            }
            let (_, shown) = target.split_once('#')?;
            if shown.is_empty()
                || (change == FixChange::AllowHost && !denials::is_fixable_host(shown))
            {
                return None;
            }
        }
        _ if target.is_empty() => return None,
        _ => {}
    }
    Some(Eligible {
        reason,
        change,
        who,
        target,
    })
}

/// The denial a recipient's card draws, re-derived from the row (never from a client). The
/// recipient holds the right by role or ownership, so `callerCanApply` is true; it is a hint.
pub fn denial_for_recipient(den: &StoredDenial, el: &Eligible) -> Denial {
    let subject = denials::sanitize_subject(&den.subject);
    Denial {
        reason: el.reason as i32,
        text: denials::host_text(
            Text::Fix(el.reason, el.change, el.who),
            &subject,
            &denials::publisher_of(&den.agent_urn),
        ),
        subject,
        fix: Some(SuggestedFix {
            change: el.change as i32,
            target: el.target.clone(),
            who: el.who as i32,
            route: denials::route_for(el.change, &el.target),
            caller_can_apply: true,
        }),
        decision_ref: String::new(),
    }
}

/// What the requester is told they asked: host text, no names.
pub fn asked_text(who: &str) -> &'static str {
    match who {
        "AGENT_OWNER" => "the agent's owner",
        "ORG_ADMIN" => "an org admin",
        _ => "someone who can apply it",
    }
}

// ---- notification text ------------------------------------------------------------------------

/// A value fit for a sentence: `[A-Za-z0-9._-]`, at most 128 bytes. Anything else becomes a
/// generic phrase, so no value can carry markup, a link or a line break into a notification.
fn plain(value: &str) -> Option<&str> {
    let ok = !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    ok.then_some(value)
}

/// A secret id fit for a code span: `[A-Za-z0-9._/-]`, 1 to 128 bytes (no `:`, no backtick).
fn secret_id(value: &str) -> Option<&str> {
    let ok = !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'/'));
    ok.then_some(value)
}

/// A username (or user id) as a name: letters, digits and `. - _ + @` only.
fn display_name(name: &str) -> String {
    let ok = !name.is_empty()
        && name.len() <= 254
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'+' | b'@'));
    if ok {
        name.to_string()
    } else {
        "A user".to_string()
    }
}

/// Title and body of a `fix.requested` row, from fixed templates over host identifiers. The
/// egress host is chosen by the guest: it never appears in the title, and in the body only as a
/// code span next to a fixed warning.
pub fn notification_text(
    change: FixChange,
    requester: &str,
    agent_urn: &str,
    fix_target: &str,
    subject: &str,
) -> (String, String) {
    let u = display_name(requester);
    let label = notifications::agent_label_of(agent_urn);
    let agent = plain(&label).unwrap_or("an agent");
    let shown = fix_target
        .split_once('#')
        .map(|(_, rest)| rest)
        .unwrap_or(fix_target);
    let (title, body) = match change {
        FixChange::EnableConnector => {
            let c = plain(shown).unwrap_or("a connector");
            (
                format!("{u} asks you to enable {c}"),
                format!(
                    "{u} ran {agent} and it was refused because {c} is not enabled for this organization."
                ),
            )
        }
        FixChange::AllowTool => {
            // The target of an allow may be `builtin:write_file`; the denial subject is the
            // catalog tool name.
            let subject = denials::sanitize_subject(subject);
            let t = plain(&subject).unwrap_or("a tool");
            (
                format!("{u} asks you to allow {t} for {agent}"),
                format!(
                    "{u}'s run of {agent} was refused because {t} is not allowed for it. You can allow it in the agent's permissions."
                ),
            )
        }
        FixChange::AllowHost => {
            let host = match plain(shown) {
                Some(h) => format!("`{h}`"),
                None => "a host".to_string(),
            };
            (
                format!("{u} asks you to allow a host for {agent}"),
                format!(
                    "{u} ran {agent}. The agent tried to reach {host} during a run. Allowing it lets the agent send data there."
                ),
            )
        }
        FixChange::SetSecret => {
            match secret_id(shown) {
                Some(id) => (
                    format!("{u} asks you to set a secret for {agent}"),
                    format!(
                        "A run of {agent} by {u} needs the organization secret `{id}`, which is not set."
                    ),
                ),
                None => (
                    format!("{u} asks you to set a secret for {agent}"),
                    format!(
                        "A run of {agent} by {u} needs a secret this agent needs, which is not set."
                    ),
                ),
            }
        }
        _ => (
            format!("{u} asks for a change to {agent}"),
            format!("{u} ran {agent} and it was refused."),
        ),
    };
    (
        notifications::clip(&title, MAX_TITLE_CHARS),
        notifications::clip(&body, MAX_BODY_CHARS),
    )
}

// ---- the request ------------------------------------------------------------------------------

/// A stored request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FixRequest {
    pub id: String,
    pub denial_id: String,
    pub requester_user_id: String,
    pub agent_urn: String,
    pub run_id: Option<String>,
    pub change: String,
    pub target: String,
    pub who: String,
    pub created_at: i64,
}

/// Everything a request needs, computed before the store lock is taken. `notices` holds one
/// notification per recipient; the request id is minted here so each carries it.
#[derive(Debug, Clone)]
pub struct Plan {
    pub request: FixRequest,
    pub notices: Vec<NewNotification>,
}

/// The `fixRequest` object of a notification's action (lifted by `Row::to_json`).
fn fix_request_json(req: &FixRequest, requester_name: &str, denial: Option<Value>) -> Value {
    let mut v = json!({
        "id": req.id,
        "change": req.change,
        "target": req.target,
        "who": req.who,
        "requester": requester_name,
        "requesterUserId": req.requester_user_id,
        "createdAt": req.created_at,
    });
    if let (Some(d), Some(o)) = (denial, v.as_object_mut()) {
        o.insert("denial".to_string(), d);
    }
    v
}

/// Builds the request and its notifications. `recipients` are already filtered (not the
/// requester, active, deduplicated); `requester_name` is the username, or the user id when the
/// lookup failed.
pub fn plan(
    den: &StoredDenial,
    el: &Eligible,
    requester_user_id: &str,
    requester_name: &str,
    recipients: &[String],
    now: i64,
) -> Plan {
    let request = FixRequest {
        id: crate::spaces::mint_id("frq_"),
        denial_id: den.id.clone(),
        requester_user_id: requester_user_id.to_string(),
        agent_urn: den.agent_urn.clone(),
        run_id: den.run_id.clone(),
        change: el.change.as_str_name().to_string(),
        target: el.target.clone(),
        who: el.who.as_str_name().to_string(),
        created_at: now,
    };
    let (title, body) = notification_text(
        el.change,
        requester_name,
        &den.agent_urn,
        &el.target,
        &den.subject,
    );
    let denial = denials::denial_json(&denial_for_recipient(den, el));
    let api = denials::route_for(el.change, &el.target)
        .map(|r| format!("{} {}", r.method, r.path))
        .unwrap_or_else(|| "GET /v1/notifications".to_string());
    let action = json!({
        "verb": "open",
        "target": { "kind": "fix_request", "id": request.id },
        "api": api,
        "fixRequest": fix_request_json(&request, requester_name, denial),
    });
    let kind = Kind::FixRequested;
    let notices = recipients
        .iter()
        .map(|user| NewNotification {
            user_id: user.clone(),
            kind,
            severity: kind.severity(),
            subject_key: request.id.clone(),
            run_id: request.run_id.clone(),
            // No Space or thread: the admin is not a participant of the requester's thread.
            space_id: None,
            thread_id: None,
            approval_id: None,
            grant_id: None,
            receipt_digest: None,
            agent_urn: den.agent_urn.clone(),
            agent_label: notifications::agent_label_of(&den.agent_urn),
            via: None,
            title: title.clone(),
            body: body.clone(),
            action: action.clone(),
        })
        .collect();
    Plan { request, notices }
}

/// The outcome of filing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Filed {
    pub request: FixRequest,
    /// True when an earlier request in the last 24 hours already covers this one; nothing new was
    /// written and the returned request is that earlier one.
    pub existing: bool,
}

#[derive(Debug)]
pub enum FixRequestError {
    /// Nobody can be asked now (and no earlier request answers).
    NoRecipients,
    /// Too many new host requests for this agent in 24 hours.
    RateLimited,
    Store(OrchestratorError),
}

impl From<OrchestratorError> for FixRequestError {
    fn from(e: OrchestratorError) -> Self {
        FixRequestError::Store(e)
    }
}

impl From<rusqlite::Error> for FixRequestError {
    fn from(e: rusqlite::Error) -> Self {
        FixRequestError::Store(OrchestratorError::Database(e))
    }
}

fn map_request(r: &rusqlite::Row<'_>) -> rusqlite::Result<FixRequest> {
    Ok(FixRequest {
        id: r.get(0)?,
        denial_id: r.get(1)?,
        requester_user_id: r.get(2)?,
        agent_urn: r.get(3)?,
        run_id: r.get(4)?,
        change: r.get(5)?,
        target: r.get(6)?,
        who: r.get(7)?,
        created_at: r.get(8)?,
    })
}

impl SqliteWorkflowStore {
    /// Files `plan` in one immediate transaction: the newest request of the same (requester,
    /// change, target) in the last 24 hours answers as `existing` and nothing is written;
    /// otherwise, with recipients and under the host cap, the request and its notifications
    /// commit together. The connection mutex serializes callers in this process and the
    /// immediate transaction covers any other connection, so two concurrent requests file one.
    /// No other store method runs inside: everything the plan needs was computed first.
    pub fn file_fix_request(&self, plan: &Plan) -> Result<Filed, FixRequestError> {
        let req = &plan.request;
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let since = req.created_at - DEDUPE_SECS;
        let existing = tx
            .query_row(
                "SELECT id, denial_id, requester_user_id, agent_urn, run_id, change, target, who,
                        created_at
                 FROM fix_requests
                 WHERE requester_user_id = ?1 AND change = ?2 AND target = ?3
                   AND created_at >= ?4
                 ORDER BY created_at DESC, rowid DESC LIMIT 1",
                params![req.requester_user_id, req.change, req.target, since],
                map_request,
            )
            .optional()?;
        if let Some(request) = existing {
            return Ok(Filed {
                request,
                existing: true,
            });
        }
        if plan.notices.is_empty() {
            return Err(FixRequestError::NoRecipients);
        }
        if req.change == FixChange::AllowHost.as_str_name() {
            let recent: i64 = tx.query_row(
                "SELECT COUNT(*) FROM fix_requests
                 WHERE requester_user_id = ?1 AND agent_urn = ?2 AND change = ?3
                   AND created_at >= ?4",
                params![req.requester_user_id, req.agent_urn, req.change, since],
                |r| r.get(0),
            )?;
            if recent >= HOST_REQUESTS_PER_DAY {
                return Err(FixRequestError::RateLimited);
            }
        }
        tx.execute(
            "INSERT INTO fix_requests
               (id, denial_id, requester_user_id, agent_urn, run_id, change, target, who,
                created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                req.id,
                req.denial_id,
                req.requester_user_id,
                req.agent_urn,
                req.run_id,
                req.change,
                req.target,
                req.who,
                req.created_at
            ],
        )?;
        for notice in &plan.notices {
            notifications::record(&tx, notice)?;
        }
        tx.commit()?;
        Ok(Filed {
            request: req.clone(),
            existing: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::users::UserRole;
    use std::sync::Arc;

    const URN: &str = "urn:vam:agent:hub.example.dev/acme/helper";
    const URN2: &str = "urn:vam:agent:hub.example.dev/acme/other";
    const NOW: i64 = 1_800_000_000;

    fn store() -> SqliteWorkflowStore {
        SqliteWorkflowStore::in_memory().expect("store")
    }

    fn stored(change: FixChange, reason: DenialReason, who: FixWho) -> StoredDenial {
        StoredDenial {
            id: "dnl_x".into(),
            run_id: Some("run-1".into()),
            caller_user_id: "alice".into(),
            agent_urn: URN.into(),
            reason: reason.as_str_name().into(),
            subject: match change {
                FixChange::AllowTool => "write_file".into(),
                FixChange::AllowHost => "example.com".into(),
                FixChange::EnableConnector => "microsoft-365".into(),
                _ => "jira-token".into(),
            },
            target: match change {
                FixChange::AllowTool => "builtin:write_file".into(),
                FixChange::AllowHost => "example.com".into(),
                FixChange::EnableConnector => "microsoft-365".into(),
                _ => "jira-token".into(),
            },
            change: change.as_str_name().into(),
            who: who.as_str_name().into(),
            created_at: NOW,
        }
    }

    fn connector() -> StoredDenial {
        let mut d = stored(
            FixChange::EnableConnector,
            DenialReason::ConnectorDisabled,
            FixWho::OrgAdmin,
        );
        d.agent_urn = URN.into();
        d
    }

    fn host(urn: &str, host: &str) -> StoredDenial {
        let mut d = stored(
            FixChange::AllowHost,
            DenialReason::EgressRule,
            FixWho::AgentOwner,
        );
        d.agent_urn = urn.into();
        d.subject = host.into();
        d.target = host.into();
        d
    }

    fn plan_for(
        den: &StoredDenial,
        requester: &str,
        recipients: &[&str],
        now: i64,
    ) -> Option<Plan> {
        let el = eligible(den, now)?;
        let recipients: Vec<String> = recipients.iter().map(|s| s.to_string()).collect();
        Some(plan(den, &el, requester, requester, &recipients, now))
    }

    fn count(st: &SqliteWorkflowStore, table: &str) -> i64 {
        st.connection
            .lock()
            .expect("lock")
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .expect("count")
    }

    #[test]
    fn only_the_four_triples_are_askable() {
        use DenialReason as R;
        use FixChange as C;
        use FixWho as W;
        assert!(askable(R::PolicyRule, C::AllowTool, W::AgentOwner));
        assert!(askable(R::EgressRule, C::AllowHost, W::AgentOwner));
        assert!(askable(
            R::ConnectorDisabled,
            C::EnableConnector,
            W::OrgAdmin
        ));
        assert!(askable(R::SecretMissing, C::SetSecret, W::OrgAdmin));
        // The reason must match too, and SELF or a change with its own route never qualifies.
        assert!(!askable(R::EgressRule, C::AllowTool, W::AgentOwner));
        assert!(!askable(R::PolicyRule, C::AllowTool, W::OrgAdmin));
        assert!(!askable(R::SecretMissing, C::SetSecret, W::Self_));
        assert!(!askable(
            R::AgentNotGranted,
            C::RequestAgentAccess,
            W::AgentOwner
        ));
        assert!(!askable(
            R::ConnectorNotConnected,
            C::ConnectConnector,
            W::Self_
        ));
    }

    #[test]
    fn a_row_is_eligible_only_when_its_target_can_be_carried() {
        let ok = eligible(&connector(), NOW).expect("connector");
        assert_eq!(ok.target, "microsoft-365");
        // An unknown or corrupt value in the row.
        let mut d = connector();
        d.change = "NOPE".into();
        assert!(eligible(&d, NOW).is_none());
        d = connector();
        d.reason = "".into();
        assert!(eligible(&d, NOW).is_none());
        // No fix.
        d = connector();
        d.change = "FIX_CHANGE_UNSPECIFIED".into();
        assert!(eligible(&d, NOW).is_none());
        // An empty target.
        d = connector();
        d.target = String::new();
        assert!(eligible(&d, NOW).is_none());
        // An allow needs an agent and something after the `#`.
        let mut t = stored(
            FixChange::AllowTool,
            DenialReason::PolicyRule,
            FixWho::AgentOwner,
        );
        t.agent_urn = String::new();
        assert!(eligible(&t, NOW).is_none());
        t.agent_urn = URN.into();
        t.target = String::new();
        t.subject = String::new();
        assert!(eligible(&t, NOW).is_none());
        // A host a fix target cannot carry.
        assert!(eligible(&host(URN, "a#b.example"), NOW).is_none());
        assert!(eligible(&host(URN, "Example.com"), NOW).is_none());
        assert!(eligible(&host(URN, "example.com"), NOW).is_some());
    }

    #[test]
    fn a_decision_ref_older_than_thirty_days_is_stale() {
        let d = connector();
        assert!(
            eligible(&d, NOW + STALE_AFTER_SECS).is_some(),
            "exactly 30 days"
        );
        assert!(eligible(&d, NOW + STALE_AFTER_SECS + 1).is_none());
    }

    #[test]
    fn the_fix_target_is_the_one_the_denial_showed() {
        use crate::denials::{egress_denial, secret_missing_denials};
        // Every constructor's denial, stored, re-derives to the target it carried.
        let st = store();
        st.create_user_with_role("owner1", "owner1", UserRole::Member)
            .expect("owner");
        st.create_user_with_role("alice", "alice", UserRole::Member)
            .expect("alice");
        let spec = vam_spec::v1::AgentSpec {
            resource_id: Some(vam_spec::v1::ResourceId { urn: URN.into() }),
            display_name: "Helper".into(),
            role: "assistant".into(),
            instruction_source: Some(
                vam_spec::v1::agent_spec::InstructionSource::SystemInstructions("x".into()),
            ),
            ..Default::default()
        };
        st.publish_agent_draft_with_owner(&spec, "owner1")
            .expect("agent");
        st.create_session("s1", None, "vm-s1", "ws", "storage-default")
            .expect("session");
        st.connection
            .lock()
            .expect("lock")
            .execute(
                "UPDATE sessions SET owner_user_id = 'alice' WHERE session_id = 's1'",
                [],
            )
            .expect("owner");
        let label = "rule=agent rule internet:*; active-policy:terminate";
        let d = egress_denial(&st, "s1", Some(URN), "example.com", label).expect("denial");
        let den = st.get_denial(&d.decision_ref).expect("get").expect("row");
        let el = eligible(&den, den.created_at).expect("eligible");
        assert_eq!(
            Some(el.target.as_str()),
            d.fix.as_ref().map(|f| f.target.as_str())
        );
        let recipient = denial_for_recipient(&den, &el);
        assert_eq!(recipient.fix.as_ref().expect("fix").target, el.target);
        assert!(recipient.fix.as_ref().expect("fix").caller_can_apply);
        assert!(recipient.text.contains("example.com") && recipient.decision_ref.is_empty());

        // Secret (org scope, no session: a caller-keyed row).
        let items = vec![("jira/auth-header".to_string(), "org".to_string())];
        let ds = secret_missing_denials(&st, "alice", URN, &items);
        let den = st
            .get_denial(&ds[0].decision_ref)
            .expect("get")
            .expect("row");
        let el = eligible(&den, den.created_at).expect("eligible");
        assert_eq!(el.target, "jira/auth-header");
        assert_eq!(el.change, FixChange::SetSecret);
        assert!(den.run_id.is_none());

        // The policy constructor keeps the permission target in the row; the MCP form shows the
        // tool name.
        assert_eq!(
            fix_target_of(
                FixChange::AllowTool,
                URN,
                "mcp:srv/jira",
                "jira_create_issue"
            ),
            format!("{URN}#jira_create_issue")
        );
        assert_eq!(
            fix_target_of(
                FixChange::AllowTool,
                URN,
                "builtin:write_file",
                "write_file"
            ),
            format!("{URN}#builtin:write_file")
        );
        assert_eq!(
            fix_target_of(FixChange::EnableConnector, URN, "microsoft-365", "x"),
            "microsoft-365"
        );
    }

    #[test]
    fn notification_text_is_host_text_with_plain_values_only() {
        let (t, b) = notification_text(
            FixChange::EnableConnector,
            "alice",
            URN,
            "microsoft-365",
            "",
        );
        assert_eq!(t, "alice asks you to enable microsoft-365");
        assert!(
            b.contains("alice ran helper") && b.contains("not enabled"),
            "{b}"
        );
        // The host never reaches the title; the body has it as a code span with the warning.
        let (t, b) = notification_text(
            FixChange::AllowHost,
            "alice",
            URN,
            &format!("{URN}#evil.example.com"),
            "evil.example.com",
        );
        assert!(!t.contains("evil.example.com"), "{t}");
        assert!(b.contains("`evil.example.com`"), "{b}");
        assert!(
            b.contains("Allowing it lets the agent send data there."),
            "{b}"
        );
        // A value outside [A-Za-z0-9._-] becomes a generic phrase.
        let (t, b) = notification_text(
            FixChange::SetSecret,
            "alice",
            URN,
            "jira/auth header\n[x](http://e)",
            "",
        );
        assert_eq!(t, "alice asks you to set a secret for helper");
        assert!(!b.contains("http") && !b.contains('\n'), "{b}");
        assert!(b.contains("a secret this agent needs"), "{b}");
        // A `/`-containing id is shown in a code span, in the body only.
        let (t, b) = notification_text(FixChange::SetSecret, "alice", URN, "jira/auth-header", "");
        assert!(!t.contains("jira"), "{t}");
        assert!(
            b.contains("the organization secret `jira/auth-header`,"),
            "{b}"
        );
        // A `:` or a backtick is not an id.
        for bad in ["a:b", "x`y", "", &"a".repeat(129)] {
            let (_, b) = notification_text(FixChange::SetSecret, "alice", URN, bad, "");
            assert!(
                b.contains("a secret this agent needs") && !b.contains('`'),
                "{b}"
            );
        }
        // A builtin tool target renders the catalog tool name, not the target.
        let (t, b) = notification_text(
            FixChange::AllowTool,
            "alice",
            URN,
            &format!("{URN}#builtin:write_file"),
            "write_file",
        );
        assert_eq!(t, "alice asks you to allow write_file for helper");
        assert!(b.contains("write_file is not allowed"), "{b}");
        let (t, _) = notification_text(
            FixChange::AllowTool,
            "alice",
            URN,
            &format!("{URN}#{}", "a".repeat(129)),
            &"a".repeat(129),
        );
        assert!(t.contains("allow a tool for helper"), "{t}");
        // A name with markup is not shown.
        let (t, _) = notification_text(
            FixChange::AllowTool,
            "al ice]\n",
            URN,
            &format!("{URN}#t"),
            "t",
        );
        assert!(t.starts_with("A user asks"), "{t}");
        // Sizes.
        let long = "a".repeat(300);
        let (t, b) = notification_text(
            FixChange::AllowTool,
            &long,
            &format!("urn:vam:agent:h/n/{long}"),
            &format!("{URN}#{long}"),
            &long,
        );
        assert!(t.chars().count() <= MAX_TITLE_CHARS && b.chars().count() <= MAX_BODY_CHARS);
    }

    fn setup() -> SqliteWorkflowStore {
        let st = store();
        for (id, role) in [
            ("alice", UserRole::Member),
            ("admin1", UserRole::OrgAdmin),
            ("admin2", UserRole::OrgAdmin),
        ] {
            st.create_user_with_role(id, id, role).expect("user");
        }
        st
    }

    fn notes(st: &SqliteWorkflowStore, user: &str) -> Vec<(String, Option<String>, String)> {
        let c = st.connection.lock().expect("lock");
        let mut s = c
            .prepare(
                "SELECT kind, run_id, subject_key FROM notifications WHERE user_id = ?1
                 ORDER BY created_at",
            )
            .expect("prepare");
        s.query_map([user], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .expect("query")
            .map(|r| r.expect("row"))
            .collect()
    }

    #[test]
    fn a_first_request_writes_the_request_and_one_notification_per_recipient() {
        let st = setup();
        let p = plan_for(&connector(), "alice", &["admin1", "admin2"], NOW).expect("plan");
        let filed = st.file_fix_request(&p).expect("filed");
        assert!(!filed.existing);
        assert_eq!(count(&st, "fix_requests"), 1);
        for admin in ["admin1", "admin2"] {
            let n = notes(&st, admin);
            assert_eq!(
                n,
                vec![(
                    "fix.requested".to_string(),
                    Some("run-1".to_string()),
                    p.request.id.clone()
                )]
            );
        }
        // The card: severity, no Space or thread, `fixRequest` lifted to the top.
        let (row, json) = {
            let row = st
                .list_notifications("admin1", &Default::default(), None, 10)
                .expect("list")
                .remove(0);
            let j = row.to_json();
            (row, j)
        };
        assert_eq!(row.severity, "action");
        assert!(json.get("spaceId").is_none() && json.get("threadId").is_none());
        let fr = &json["fixRequest"];
        assert_eq!(fr["id"], p.request.id);
        assert_eq!(fr["change"], "ENABLE_CONNECTOR");
        assert_eq!(fr["target"], "microsoft-365");
        assert_eq!(fr["who"], "ORG_ADMIN");
        assert_eq!(fr["requester"], "alice");
        assert_eq!(fr["requesterUserId"], "alice");
        assert_eq!(fr["denial"]["reason"], "CONNECTOR_DISABLED");
        assert_eq!(fr["denial"]["fix"]["change"], "ENABLE_CONNECTOR");
        assert_eq!(
            fr["denial"]["fix"]["route"]["path"],
            "/v1/connectors/microsoft-365/node"
        );
        assert!(fr["denial"].get("decisionRef").is_none());
        assert!(
            json["action"].get("fixRequest").is_none(),
            "lifted out of the action"
        );
        assert_eq!(json["action"]["verb"], "open");
        assert_eq!(json["action"]["target"]["kind"], "fix_request");
        assert_eq!(json["kind"], "fix.requested");
        // Admins close it with the generic `acted`.
        let acted = st
            .mark_notification_acted("admin1", json["id"].as_str().expect("id"), "api")
            .expect("acted");
        assert!(acted.resolved_at.is_some());
    }

    #[test]
    fn a_repeat_inside_24_hours_answers_the_first_and_notifies_nobody() {
        let st = setup();
        let first = st
            .file_fix_request(&plan_for(&connector(), "alice", &["admin1"], NOW).expect("p"))
            .expect("first");
        // Even another denial of the same fix, a minute later, and even with nobody to ask now.
        let again = st
            .file_fix_request(&plan_for(&connector(), "alice", &[], NOW + 60).expect("p"))
            .expect("again");
        assert!(again.existing);
        assert_eq!(again.request.id, first.request.id);
        assert_eq!(count(&st, "fix_requests"), 1);
        assert_eq!(notes(&st, "admin1").len(), 1);
        // Exactly 24 h later it is still the same request; one second after, a new one.
        let edge = st
            .file_fix_request(
                &plan_for(&connector(), "alice", &["admin1"], NOW + DEDUPE_SECS).expect("p"),
            )
            .expect("edge");
        assert!(edge.existing);
        let next = st
            .file_fix_request(
                &plan_for(&connector(), "alice", &["admin1"], NOW + DEDUPE_SECS + 1).expect("p"),
            )
            .expect("next");
        assert!(!next.existing);
        assert_ne!(next.request.id, first.request.id);
        assert_eq!(count(&st, "fix_requests"), 2);
        assert_eq!(notes(&st, "admin1").len(), 2);
    }

    #[test]
    fn the_key_is_the_fix_target_so_two_agents_asking_for_one_host_are_two_requests() {
        let st = setup();
        for urn in [URN, URN2] {
            let f = st
                .file_fix_request(
                    &plan_for(&host(urn, "example.com"), "alice", &["admin1"], NOW).expect("p"),
                )
                .expect("filed");
            assert!(!f.existing, "{urn}");
        }
        assert_eq!(count(&st, "fix_requests"), 2);
        // Another requester for the same fix is not a repeat either.
        let f = st
            .file_fix_request(
                &plan_for(&host(URN, "example.com"), "bob", &["admin1"], NOW).expect("p"),
            )
            .expect("filed");
        assert!(!f.existing);
    }

    #[test]
    fn no_recipients_files_nothing() {
        let st = setup();
        let err = st
            .file_fix_request(&plan_for(&connector(), "alice", &[], NOW).expect("p"))
            .expect_err("nobody");
        assert!(matches!(err, FixRequestError::NoRecipients));
        assert_eq!(
            (count(&st, "fix_requests"), count(&st, "notifications")),
            (0, 0)
        );
    }

    #[test]
    fn host_requests_are_capped_per_requester_and_agent() {
        let st = setup();
        for i in 0..HOST_REQUESTS_PER_DAY {
            let p = plan_for(
                &host(URN, &format!("h{i}.example.com")),
                "alice",
                &["admin1"],
                NOW + i,
            )
            .expect("p");
            assert!(!st.file_fix_request(&p).expect("filed").existing);
        }
        let over = plan_for(
            &host(URN, "h-over.example.com"),
            "alice",
            &["admin1"],
            NOW + 20,
        )
        .expect("p");
        assert!(matches!(
            st.file_fix_request(&over),
            Err(FixRequestError::RateLimited)
        ));
        assert_eq!(count(&st, "fix_requests"), HOST_REQUESTS_PER_DAY);
        assert_eq!(notes(&st, "admin1").len() as i64, HOST_REQUESTS_PER_DAY);
        // A repeat of one of the ten still answers 200, another agent has its own budget, and the
        // window slides.
        let rep =
            plan_for(&host(URN, "h0.example.com"), "alice", &["admin1"], NOW + 30).expect("p");
        assert!(st.file_fix_request(&rep).expect("repeat").existing);
        let other = plan_for(
            &host(URN2, "h-over.example.com"),
            "alice",
            &["admin1"],
            NOW + 30,
        )
        .expect("p");
        assert!(!st.file_fix_request(&other).expect("other").existing);
        let later = plan_for(
            &host(URN, "h-over.example.com"),
            "alice",
            &["admin1"],
            NOW + DEDUPE_SECS + 20,
        )
        .expect("p");
        assert!(!st.file_fix_request(&later).expect("later").existing);
    }

    #[test]
    fn two_concurrent_requests_file_one() {
        let st = Arc::new(setup());
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let st = Arc::clone(&st);
                std::thread::spawn(move || {
                    let p = plan_for(&connector(), "alice", &["admin1", "admin2"], NOW).expect("p");
                    st.file_fix_request(&p).expect("filed")
                })
            })
            .collect();
        let filed: Vec<Filed> = handles
            .into_iter()
            .map(|h| h.join().expect("join"))
            .collect();
        assert_eq!(filed.iter().filter(|f| !f.existing).count(), 1);
        let ids: std::collections::BTreeSet<_> =
            filed.iter().map(|f| f.request.id.clone()).collect();
        assert_eq!(ids.len(), 1, "every caller was answered the one request");
        assert_eq!(count(&st, "fix_requests"), 1);
        assert_eq!(notes(&st, "admin1").len(), 1);
        assert_eq!(notes(&st, "admin2").len(), 1);
    }

    #[test]
    fn a_failing_notification_rolls_the_request_back() {
        let st = setup();
        let p = plan_for(&connector(), "alice", &["admin1"], NOW).expect("p");
        // A notification insert that fails: the request must not stay without it.
        st.connection
            .lock()
            .expect("lock")
            .execute(
                "CREATE TRIGGER no_notes BEFORE INSERT ON notifications
                 BEGIN SELECT RAISE(ABORT, 'refused'); END",
                [],
            )
            .expect("trigger");
        assert!(matches!(
            st.file_fix_request(&p),
            Err(FixRequestError::Store(_))
        ));
        assert_eq!(count(&st, "fix_requests"), 0);
        st.connection
            .lock()
            .expect("lock")
            .execute("DROP TRIGGER no_notes", [])
            .expect("drop");
        assert!(!st.file_fix_request(&p).expect("retry").existing);
    }

    #[test]
    fn get_denial_answers_none_for_a_missing_row_and_the_row_otherwise() {
        let st = setup();
        assert!(st
            .get_denial("dnl_00000000000000000000000000")
            .expect("get")
            .is_none());
        assert!(st.get_denial("").expect("get").is_none());
    }
}
