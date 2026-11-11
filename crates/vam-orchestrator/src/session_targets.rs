//! Guest tool name to permission target, resolved when a session's tool list is built.
//!
//! The host knows only the guest `tool_name` when an approval is decided, and
//! the name cannot be turned back into a target: MCP names are legalized
//! (`{connection}__{vendor}`), skill tools carry no skill name in the catalog,
//! and the connection can be edited or removed while a run waits. So the
//! mapping is computed once at session start, stored per session, and stamped
//! on the approval at suspend time (design Part IV §IV.2, §IV.7).
//!
//! In permissions mode a tool with no row here is denied (fail closed).

use rusqlite::{params, OptionalExtension};
use vam_security::{
    always_offered, resolve_tool, AlwaysOffer, ScopeLabel, ScopeRule, ScopedTool, SourceRule,
    ToolEntity, ToolQuery,
};
use vam_spec::permissions::{mcp_server_id, Target};
use vam_spec::v1::{AgentPermissions, SideEffectClass};

use crate::error::OrchestratorError;
use crate::session::ToolCatalogEntry;
use crate::workflow::SqliteWorkflowStore;

/// `locked_reason` stamped on every legacy session.
pub const LEGACY_LOCKED_REASON: &str = "legacy policy";
/// "Always" is not offered on a released version.
pub const RELEASED_LOCKED_REASON: &str = "released version: approvals are once only";
/// "Always" is offered to the agent owner's own draft run only.
pub const NOT_OWNER_DRAFT_LOCKED_REASON: &str =
    "only the agent owner's draft run can save a decision";

/// Tools the host adds when the run has a playbook.
const PLAYBOOK_CONTROL_TOOLS: &[&str] = &[
    "step_complete",
    "run_step",
    "check_playbook_finish",
    crate::playbook_activation::START_PLAYBOOK_TOOL,
    crate::notifications_tool::TOOL,
];

/// Where a session tool comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    Builtin,
    Skill,
    Mcp,
    /// A hosted (HSP) tool.
    Host,
}

impl SourceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SourceKind::Builtin => "builtin",
            SourceKind::Skill => "skill",
            SourceKind::Mcp => "mcp",
            SourceKind::Host => "host",
        }
    }

    fn parse(text: &str) -> Self {
        match text {
            "skill" => SourceKind::Skill,
            "mcp" => SourceKind::Mcp,
            "host" => SourceKind::Host,
            _ => SourceKind::Builtin,
        }
    }
}

/// One MCP tool as `seed_mcp_skills` planned it, which is what the guest sees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedMcpTool {
    pub connection_id: String,
    /// The connection's URL. Its canonical server id is part of the target.
    pub server_url: String,
    /// Vendor tool name, as the server calls it.
    pub vendor_tool: String,
    /// Name the guest calls (`{connection}__{vendor}`, legalized).
    pub guest_name: String,
    /// Stored `SideEffectClass` for this tool (default mutable external).
    pub side_effect: i32,
    /// The connection's `ask_tools` names this tool (a source rule of `ask`).
    pub ask: bool,
}

/// How the session's agent is being run, for the agent label and "Always".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunProvenance {
    /// Explain origin for agent rows (`agent draft`, `agent v1.1.0`).
    pub agent_origin: String,
    /// The session is the agent owner's draft run.
    pub owner_draft_run: bool,
}

/// One row of `session_tool_targets`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionToolTarget {
    pub tool_name: String,
    /// Canonical target: `skill:jira/create_issue`,
    /// `mcp:mcp.atlassian.com/create_issue`, `builtin:run`.
    pub target: String,
    pub source_kind: SourceKind,
    pub side_effect: i64,
    pub source_rule: ScopeRule,
    pub source_label: String,
    pub agent_rule: ScopeRule,
    pub agent_label: String,
    /// Always `none` until org guardrails (E3).
    pub org_rule: ScopeRule,
    pub deciding_scope: String,
    pub deciding_rule: String,
    /// "Always allow / deny" can take effect for this tool in this session.
    pub always_offered: bool,
    pub locked_reason: String,
}

impl SessionToolTarget {
    /// The `Tool` entity for a Cedar request. All four attributes are set.
    pub fn entity(&self) -> ToolEntity {
        ToolEntity::new(
            self.tool_name.clone(),
            self.agent_rule,
            self.source_rule,
            self.org_rule,
            self.target.clone(),
        )
    }
}

/// Where a catalog tool comes from, and the target it is named by.
struct Classified {
    kind: SourceKind,
    target: Target,
    source: Option<SourceRule>,
}

fn classify(
    entry: &ToolCatalogEntry,
    mcp: &[PlannedMcpTool],
) -> Result<Option<Classified>, String> {
    let name = entry.tool_name.as_str();
    if let Some(planned) = mcp.iter().find(|tool| tool.guest_name == name) {
        let server = mcp_server_id(&planned.server_url).map_err(|e| e.to_string())?;
        let target = Target::parse(&format!("mcp:{server}/{}", planned.vendor_tool))
            .map_err(|e| e.to_string())?;
        let source = planned.ask.then(|| {
            SourceRule::ask(
                "askTools",
                format!("mcp connection {}", planned.connection_id),
            )
        });
        return Ok(Some(Classified {
            kind: SourceKind::Mcp,
            target,
            source,
        }));
    }
    let builtin = |kind: SourceKind| -> Result<Option<Classified>, String> {
        let target = Target::parse(&format!("builtin:{name}")).map_err(|e| e.to_string())?;
        Ok(Some(Classified {
            kind,
            target,
            source: None,
        }))
    };
    if entry.entrypoint.starts_with("hosted:") {
        return builtin(SourceKind::Host);
    }
    if entry.command == "builtin"
        || (entry.command == "host"
            && entry.skill_name.is_empty()
            && PLAYBOOK_CONTROL_TOOLS.contains(&name))
    {
        return builtin(SourceKind::Builtin);
    }
    if entry.skill_name.is_empty() {
        return Ok(None);
    }
    let target =
        Target::parse(&format!("skill:{}/{name}", entry.skill_name)).map_err(|e| e.to_string())?;
    let source = entry
        .approval_required
        .then(|| SourceRule::ask("approvalRequired", format!("skill {}", entry.skill_name)));
    Ok(Some(Classified {
        kind: SourceKind::Skill,
        target,
        source,
    }))
}

fn side_effect_class(value: i32) -> Option<SideEffectClass> {
    SideEffectClass::try_from(value).ok()
}

/// A tool the session can expose that has no target (an unparseable skill
/// name or tool token). It gets no row, so permissions mode denies it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UntargetedTool {
    pub tool_name: String,
    pub reason: String,
}

/// Rows for every tool a session can expose. Pure: no store, no session.
///
/// `permissions` [`None`] is the legacy path: every row has `none` rules and
/// is stamped `always_offered = false`, `locked_reason = "legacy policy"`.
/// AX-5.6b and AX-5.8a reuse this for an agent that is not running.
pub fn targets_for_agent(
    permissions: Option<&AgentPermissions>,
    provenance: &RunProvenance,
    catalog: &[ToolCatalogEntry],
    mcp: &[PlannedMcpTool],
) -> (Vec<SessionToolTarget>, Vec<UntargetedTool>) {
    let mut rows = Vec::new();
    let mut skipped = Vec::new();
    for entry in catalog {
        let classified = match classify(entry, mcp) {
            Ok(Some(classified)) => classified,
            Ok(None) => {
                skipped.push(UntargetedTool {
                    tool_name: entry.tool_name.clone(),
                    reason: "tool has no skill name, builtin name, or MCP plan".to_string(),
                });
                continue;
            }
            Err(reason) => {
                skipped.push(UntargetedTool {
                    tool_name: entry.tool_name.clone(),
                    reason,
                });
                continue;
            }
        };
        let side_effect = match classified.kind {
            SourceKind::Mcp => mcp
                .iter()
                .find(|tool| tool.guest_name == entry.tool_name)
                .map(|tool| tool.side_effect)
                .unwrap_or(entry.side_effect),
            _ => entry.side_effect,
        };
        let Some(permissions) = permissions else {
            rows.push(legacy_row(entry, &classified, side_effect));
            continue;
        };
        let query = ToolQuery {
            name: &entry.tool_name,
            target: &classified.target,
            side_effect: side_effect_class(side_effect),
            source: classified.source.as_ref(),
            org: None,
        };
        let scoped = match resolve_tool(permissions, &provenance.agent_origin, query) {
            Ok(mut scoped) => {
                // Only the host's own builtin entry: a skill, MCP or hosted tool that
                // happens to share the name gets no default allow.
                if classified.kind == SourceKind::Builtin
                    && vam_spec::outbox::is_outbox_tool(&entry.tool_name)
                    && classified.target.to_string() == format!("builtin:{}", entry.tool_name)
                {
                    keep_outbox_tool(&mut scoped, permissions, &classified.target, provenance);
                }
                // Opt-in only: whatever the default, the notification list is offered
                // only when a rule names this exact tool and allows it.
                if classified.kind == SourceKind::Builtin
                    && entry.tool_name == crate::notifications_tool::TOOL
                    && !crate::notifications_tool::explicit_allow(Some(permissions))
                {
                    scoped.agent = ScopeLabel::new(
                        ScopeRule::Deny,
                        NOTIFICATIONS_OPT_IN_RULE,
                        provenance.agent_origin.clone(),
                    );
                }
                scoped
            }
            Err(err) => {
                skipped.push(UntargetedTool {
                    tool_name: entry.tool_name.clone(),
                    reason: err.to_string(),
                });
                continue;
            }
        };
        rows.push(scoped_row(
            &scoped,
            classified.kind,
            side_effect,
            provenance,
        ));
    }
    (rows, skipped)
}

/// Origin text of the agent decision the host grants the outbox tools.
/// Label of the forced deny for a tool that needs an explicit allow rule.
const NOTIFICATIONS_OPT_IN_RULE: &str = "opt-in tool (needs an explicit allow rule)";
const OUTBOX_DEFAULT_RULE: &str = "conversation tool (host default)";

/// `post_message` and `show` stay with an agent that never named them (AR-3.9, Q10),
/// as `start_playbook` does through its overlay. The platform policy forbids any tool
/// whose agent decision is `deny`, and a default deny sets that, so the overlay alone
/// cannot keep them: the agent decision becomes `allow` here unless the agent has a
/// rule naming this exact tool. A `tools:*` rule is a default, not a naming rule.
/// The org and source decisions are untouched, so an org deny still wins.
fn keep_outbox_tool(
    scoped: &mut ScopedTool,
    permissions: &AgentPermissions,
    target: &Target,
    provenance: &RunProvenance,
) {
    let named = permissions.tools.as_ref().is_some_and(|tools| {
        tools
            .rules
            .iter()
            .any(|rule| Target::parse(&rule.target).is_ok_and(|t| &t == target))
    });
    if !named {
        scoped.agent = ScopeLabel::new(
            ScopeRule::Allow,
            OUTBOX_DEFAULT_RULE,
            provenance.agent_origin.clone(),
        );
    }
}

fn legacy_row(
    entry: &ToolCatalogEntry,
    classified: &Classified,
    side_effect: i32,
) -> SessionToolTarget {
    SessionToolTarget {
        tool_name: entry.tool_name.clone(),
        target: classified.target.to_string(),
        source_kind: classified.kind,
        side_effect: i64::from(side_effect),
        source_rule: ScopeRule::None,
        source_label: String::new(),
        agent_rule: ScopeRule::None,
        agent_label: String::new(),
        org_rule: ScopeRule::None,
        deciding_scope: "none".to_string(),
        deciding_rule: String::new(),
        always_offered: false,
        locked_reason: LEGACY_LOCKED_REASON.to_string(),
    }
}

fn scoped_row(
    scoped: &ScopedTool,
    kind: SourceKind,
    side_effect: i32,
    provenance: &RunProvenance,
) -> SessionToolTarget {
    let (deciding_scope, deciding_rule) = deciding(scoped);
    let (offered, locked_reason) = match always_offered(scoped) {
        AlwaysOffer::Blocked { reason } => (false, reason),
        AlwaysOffer::Offered if !provenance.owner_draft_run => (
            false,
            if provenance.agent_origin == "agent draft" {
                NOT_OWNER_DRAFT_LOCKED_REASON.to_string()
            } else {
                RELEASED_LOCKED_REASON.to_string()
            },
        ),
        AlwaysOffer::Offered => (true, String::new()),
    };
    SessionToolTarget {
        tool_name: scoped.name.clone(),
        target: scoped.target.clone(),
        source_kind: kind,
        side_effect: i64::from(side_effect),
        source_rule: scoped.source.decision,
        source_label: label_text(&scoped.source.rule, &scoped.source.origin),
        agent_rule: scoped.agent.decision,
        agent_label: label_text(&scoped.agent.rule, &scoped.agent.origin),
        org_rule: scoped.org.decision,
        deciding_scope,
        deciding_rule,
        always_offered: offered,
        locked_reason,
    }
}

fn label_text(rule: &str, origin: &str) -> String {
    match (rule.is_empty(), origin.is_empty()) {
        (true, _) => String::new(),
        (false, true) => rule.to_string(),
        (false, false) => format!("{origin}: {rule}"),
    }
}

/// The strictest scope decides; a tie goes to the outermost (org, source,
/// agent), as `classify_scoped_tools` does.
fn deciding(scoped: &ScopedTool) -> (String, String) {
    let rank = |rule: ScopeRule| match rule {
        ScopeRule::Deny => 3,
        ScopeRule::Ask => 2,
        ScopeRule::Allow => 1,
        ScopeRule::None => 0,
    };
    let mut best: Option<(&str, &vam_security::ScopeLabel)> = None;
    for (scope, label) in [
        ("org", &scoped.org),
        ("source", &scoped.source),
        ("agent", &scoped.agent),
    ] {
        if rank(label.decision) == 0 {
            continue;
        }
        if best.is_none_or(|(_, current)| rank(label.decision) > rank(current.decision)) {
            best = Some((scope, label));
        }
    }
    match best {
        Some((scope, label)) => (scope.to_string(), label.rule.clone()),
        None => ("platform".to_string(), String::new()),
    }
}

/// The stamp an approval carries: target, whether "Always" is offered, why not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalStamp {
    pub target: String,
    pub always_offered: bool,
    pub locked_reason: String,
}

impl ApprovalStamp {
    /// A session with no row for the tool is legacy: nothing to save.
    pub fn legacy() -> Self {
        Self {
            target: String::new(),
            always_offered: false,
            locked_reason: LEGACY_LOCKED_REASON.to_string(),
        }
    }
}

impl From<&SessionToolTarget> for ApprovalStamp {
    fn from(row: &SessionToolTarget) -> Self {
        Self {
            target: row.target.clone(),
            always_offered: row.always_offered,
            locked_reason: row.locked_reason.clone(),
        }
    }
}

const COLUMNS: &str = "tool_name, target, source_kind, side_effect, source_rule, source_label, \
     agent_rule, agent_label, org_rule, deciding_scope, deciding_rule, always_offered, locked_reason";

fn scope_rule_from(text: &str) -> ScopeRule {
    match text {
        "allow" => ScopeRule::Allow,
        "ask" => ScopeRule::Ask,
        "deny" => ScopeRule::Deny,
        _ => ScopeRule::None,
    }
}

fn map_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SessionToolTarget> {
    Ok(SessionToolTarget {
        tool_name: row.get(0)?,
        target: row.get(1)?,
        source_kind: SourceKind::parse(&row.get::<_, String>(2)?),
        side_effect: row.get(3)?,
        source_rule: scope_rule_from(&row.get::<_, String>(4)?),
        source_label: row.get(5)?,
        agent_rule: scope_rule_from(&row.get::<_, String>(6)?),
        agent_label: row.get(7)?,
        org_rule: scope_rule_from(&row.get::<_, String>(8)?),
        deciding_scope: row.get(9)?,
        deciding_rule: row.get(10)?,
        always_offered: row.get::<_, i64>(11)? != 0,
        locked_reason: row.get(12)?,
    })
}

pub(crate) fn initialize_session_targets_schema(
    connection: &rusqlite::Connection,
) -> Result<(), OrchestratorError> {
    connection
        .execute_batch(
            "
            CREATE TABLE IF NOT EXISTS session_tool_targets (
                session_id TEXT NOT NULL,
                tool_name TEXT NOT NULL,
                target TEXT NOT NULL,
                source_kind TEXT NOT NULL,
                side_effect INTEGER NOT NULL DEFAULT 0,
                source_rule TEXT NOT NULL DEFAULT 'none',
                source_label TEXT NOT NULL DEFAULT '',
                agent_rule TEXT NOT NULL DEFAULT 'none',
                agent_label TEXT NOT NULL DEFAULT '',
                org_rule TEXT NOT NULL DEFAULT 'none',
                deciding_scope TEXT NOT NULL DEFAULT 'none',
                deciding_rule TEXT NOT NULL DEFAULT '',
                always_offered INTEGER NOT NULL DEFAULT 0,
                locked_reason TEXT NOT NULL DEFAULT '',
                PRIMARY KEY (session_id, tool_name)
            );
            ",
        )
        .map_err(OrchestratorError::Database)?;
    let _ = connection.execute("ALTER TABLE runs ADD COLUMN policy_mode TEXT", []);
    Ok(())
}

impl SqliteWorkflowStore {
    pub(crate) fn initialize_session_targets_schema(&self) -> Result<(), OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        initialize_session_targets_schema(&connection)
    }

    /// Replace the session's rows with `rows` in one transaction.
    pub fn record_session_tool_targets(
        &self,
        session_id: &str,
        rows: &[SessionToolTarget],
    ) -> Result<(), OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let transaction = connection.unchecked_transaction()?;
        transaction.execute(
            "DELETE FROM session_tool_targets WHERE session_id = ?1",
            [session_id],
        )?;
        for row in rows {
            transaction.execute(
                &format!(
                    "INSERT INTO session_tool_targets (session_id, {COLUMNS})
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)"
                ),
                params![
                    session_id,
                    row.tool_name,
                    row.target,
                    row.source_kind.as_str(),
                    row.side_effect,
                    row.source_rule.as_str(),
                    row.source_label,
                    row.agent_rule.as_str(),
                    row.agent_label,
                    row.org_rule.as_str(),
                    row.deciding_scope,
                    row.deciding_rule,
                    i64::from(row.always_offered),
                    row.locked_reason,
                ],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    /// Every row of the session, by tool name.
    pub fn list_session_tool_targets(
        &self,
        session_id: &str,
    ) -> Result<Vec<SessionToolTarget>, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let mut stmt = connection.prepare(&format!(
            "SELECT {COLUMNS} FROM session_tool_targets WHERE session_id = ?1 ORDER BY tool_name"
        ))?;
        let rows = stmt.query_map([session_id], map_row)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(OrchestratorError::Database)
    }

    /// Record the policy mode on the session's run, when it has one.
    pub fn record_run_policy_mode(
        &self,
        session_id: &str,
        mode: &str,
    ) -> Result<(), OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        connection.execute(
            "UPDATE runs SET policy_mode = ?1 WHERE session_id = ?2",
            params![mode, session_id],
        )?;
        Ok(())
    }

    pub fn run_policy_mode(&self, session_id: &str) -> Result<Option<String>, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let mode = connection
            .query_row(
                "SELECT policy_mode FROM runs WHERE session_id = ?1 LIMIT 1",
                [session_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
        Ok(mode)
    }
}

/// What "Always allow / deny" on an approval saves, once it has passed every check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RememberPlan {
    pub agent_urn: String,
    /// The agent draft's owner, who is also the run's owner.
    pub owner: String,
    pub session_id: String,
    pub target: String,
    pub rule: ScopeRule,
}

fn remember_conflict(message: impl Into<String>) -> OrchestratorError {
    OrchestratorError::Conflict(message.into())
}

impl SqliteWorkflowStore {
    /// Check that `approval` may be remembered as `rule` by `caller`; nothing
    /// is written. Every refusal is a 409 that says why and what to do.
    ///
    /// Allowed only for a `tool_call` approval, on a draft run, where the
    /// caller owns the run and the agent's draft and the approval offered
    /// "Always" (decisions 14, 21, 27).
    pub(crate) fn plan_remember(
        &self,
        approval: &crate::error::StoredApproval,
        caller: &str,
        rule: ScopeRule,
    ) -> Result<RememberPlan, OrchestratorError> {
        if approval.kind != crate::hitl::APPROVAL_KIND_TOOL_CALL {
            return Err(remember_conflict(
                "Always allow and Always deny apply to tool approvals, not gates or questions. \
                 Decide this one without `remember`.",
            ));
        }
        let session = crate::policy_legacy::load_session_agent(self, &approval.session_id, None)?;
        if !session.draft_run {
            return Err(remember_conflict(
                "approvals on a released version are once only. Choose Allow once, or \
                 change the rule on the agent's draft and release it.",
            ));
        }
        let session_owner = self.session_owner_user_id(&approval.session_id)?;
        let caller = crate::owner::resolved_owner(caller).to_string();
        if session_owner != caller || session.resolve_owner != caller {
            return Err(remember_conflict(format!(
                "{NOT_OWNER_DRAFT_LOCKED_REASON}. Choose Allow once, or ask the agent's \
                 owner to run the draft."
            )));
        }
        let agent = session.agent.as_ref().ok_or_else(|| {
            remember_conflict("this run has no agent whose draft could save a rule")
        })?;
        if agent.permissions.is_none() {
            return Err(remember_conflict(
                "this agent still uses capability_policy or hitl_policy, so there is no \
                 permission rule to save. Migrate the agent first, or choose Allow once.",
            ));
        }
        if !approval.always_offered {
            let why = if approval.locked_reason.is_empty() {
                "this approval was not offered Always"
            } else {
                approval.locked_reason.as_str()
            };
            return Err(remember_conflict(format!(
                "cannot save a rule for this tool: {why}. Only Allow once is possible."
            )));
        }
        if approval.target.is_empty() {
            return Err(remember_conflict(
                "this approval has no permission target, so no rule can be saved. \
                 Only Allow once is possible.",
            ));
        }
        let agent_urn = agent
            .resource_id
            .as_ref()
            .map(|id| id.urn.clone())
            .filter(|urn| !urn.is_empty())
            .ok_or_else(|| remember_conflict("this run's agent has no URN"))?;
        Ok(RememberPlan {
            agent_urn,
            owner: caller,
            session_id: approval.session_id.clone(),
            target: approval.target.clone(),
            rule,
        })
    }

    /// Write the rule to the agent's draft, then to this session's rows so the
    /// rest of the run follows it. Both steps are idempotent: a retry after a
    /// partial failure rewrites the same rule (identical bytes keep the draft
    /// revision) and the same rows.
    pub(crate) fn save_remembered_rule(
        &self,
        plan: &RememberPlan,
    ) -> Result<(), OrchestratorError> {
        let decision = match plan.rule {
            ScopeRule::Allow => vam_spec::v1::PermissionDecision::Allow,
            ScopeRule::Deny => vam_spec::v1::PermissionDecision::Deny,
            _ => {
                return Err(OrchestratorError::InvalidContract(
                    "only allow and deny can be remembered".into(),
                ))
            }
        };
        crate::catalog_publish::update_draft_permissions(
            self,
            &plan.agent_urn,
            &plan.owner,
            |permissions| {
                crate::catalog_publish::upsert_permission_rule(
                    permissions,
                    vam_spec::v1::PermissionRule {
                        target: plan.target.clone(),
                        decision: decision as i32,
                    },
                );
            },
        )?;
        self.apply_agent_rule_to_session(&plan.session_id, &plan.target, plan.rule)
    }

    /// Point every row of the session for `target` at the agent rule `rule`.
    /// Only rows where "Always" was offered change, so no source or org rule
    /// is overridden.
    pub fn apply_agent_rule_to_session(
        &self,
        session_id: &str,
        target: &str,
        rule: ScopeRule,
    ) -> Result<(), OrchestratorError> {
        let rule_text = format!("{target}: {}", rule.as_str());
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        connection.execute(
            "UPDATE session_tool_targets
             SET agent_rule = ?1, agent_label = ?2, deciding_scope = 'agent', deciding_rule = ?3
             WHERE session_id = ?4 AND target = ?5 AND always_offered = 1",
            params![
                rule.as_str(),
                label_text(&rule_text, "agent draft"),
                rule_text,
                session_id,
                target
            ],
        )?;
        Ok(())
    }
}

/// The row for one guest tool name, or [`None`] when the session has none.
pub fn target_for(
    store: &SqliteWorkflowStore,
    session_id: &str,
    tool_name: &str,
) -> Result<Option<SessionToolTarget>, OrchestratorError> {
    let connection = store
        .connection
        .lock()
        .map_err(|_| OrchestratorError::StorageUnavailable)?;
    connection
        .query_row(
            &format!(
                "SELECT {COLUMNS} FROM session_tool_targets
                 WHERE session_id = ?1 AND tool_name = ?2"
            ),
            params![session_id, tool_name],
            map_row,
        )
        .optional()
        .map_err(OrchestratorError::Database)
}

/// Compute and store the session's rows. Called once, with the catalog and the
/// MCP plan the guest sees.
///
/// Loads the agent the run pinned. An agent with `permissions` resolves each
/// tool through the shared resolver. Any other session gets legacy rows.
pub(crate) fn assemble_session_targets(
    store: &SqliteWorkflowStore,
    session_id: &str,
    catalog: &[ToolCatalogEntry],
    mcp: &[PlannedMcpTool],
) -> Result<(), OrchestratorError> {
    let session = crate::policy_legacy::load_session_agent(store, session_id, None)?;
    let provenance = crate::security::run_provenance(store, session_id, &session);
    let permissions = session.agent.as_ref().and_then(|a| a.permissions.as_ref());
    let (rows, skipped) = targets_for_agent(permissions, &provenance, catalog, mcp);
    for tool in &skipped {
        eprintln!(
            "[orchestrator] WARN session {session_id} tool '{}' has no permission target: {}",
            tool.tool_name, tool.reason
        );
    }
    store.record_session_tool_targets(session_id, &rows)?;
    let mode = if permissions.is_some() {
        "permissions"
    } else {
        "legacy"
    };
    store.record_run_policy_mode(session_id, mode)
}

#[cfg(test)]
mod tests {
    use super::*;
    use vam_spec::v1::{PermissionDecision, PermissionRule, ToolDefault, ToolPermissions};

    const MUTABLE_EXTERNAL: i32 = SideEffectClass::MutableExternal as i32;

    fn entry(
        name: &str,
        command: &str,
        skill: &str,
        side_effect: i32,
        ask: bool,
    ) -> ToolCatalogEntry {
        ToolCatalogEntry {
            tool_name: name.to_string(),
            command: command.to_string(),
            entrypoint: name.to_string(),
            digest: String::new(),
            side_effect,
            skill_name: skill.to_string(),
            approval_required: ask,
        }
    }

    fn mcp_tool(conn: &str, url: &str, vendor: &str, guest: &str) -> PlannedMcpTool {
        PlannedMcpTool {
            connection_id: conn.to_string(),
            server_url: url.to_string(),
            vendor_tool: vendor.to_string(),
            guest_name: guest.to_string(),
            side_effect: MUTABLE_EXTERNAL,
            ask: false,
        }
    }

    fn defaults() -> AgentPermissions {
        AgentPermissions {
            tools: Some(ToolPermissions {
                default: ToolDefault::BySideEffect as i32,
                rules: vec![],
            }),
            ..Default::default()
        }
    }

    fn draft() -> RunProvenance {
        RunProvenance {
            agent_origin: "agent draft".to_string(),
            owner_draft_run: true,
        }
    }

    fn released() -> RunProvenance {
        RunProvenance {
            agent_origin: "agent v1.1.0".to_string(),
            owner_draft_run: false,
        }
    }

    fn by_name<'a>(rows: &'a [SessionToolTarget], name: &str) -> &'a SessionToolTarget {
        rows.iter().find(|r| r.tool_name == name).expect(name)
    }

    fn catalog() -> Vec<ToolCatalogEntry> {
        vec![
            entry("read_file", "builtin", "", 1, false),
            entry("run", "builtin", "", 2, false),
            entry("step_complete", "host", "", 2, false),
            entry("jira_search", "bun", "jira", 1, false),
            entry("jira_create_issue", "bun", "jira", 2, true),
            entry("atlassian__create_issue", "host", "", 3, false),
            ToolCatalogEntry {
                entrypoint: "hosted:echo:echo.ping".to_string(),
                ..entry("echo_ping", "host", "", 1, false)
            },
        ]
    }

    fn deny_by_default() -> AgentPermissions {
        AgentPermissions {
            tools: Some(ToolPermissions {
                default: ToolDefault::Deny as i32,
                rules: vec![],
            }),
            ..Default::default()
        }
    }

    #[test]
    fn the_outbox_default_allow_is_for_the_hosts_builtin_entry_only() {
        for name in ["post_message", "show"] {
            // The host's own entry keeps the tool under a default deny.
            let (rows, _) = targets_for_agent(
                Some(&deny_by_default()),
                &draft(),
                &[entry(name, "builtin", "", 1, false)],
                &[],
            );
            assert_eq!(by_name(&rows, name).agent_rule, ScopeRule::Allow, "{name}");
            assert_eq!(by_name(&rows, name).target, format!("builtin:{name}"));

            // A skill tool, an MCP tool and a hosted tool of the same name stay denied.
            let skill = entry(name, "bun", "demo", 1, false);
            let hosted = ToolCatalogEntry {
                entrypoint: format!("hosted:echo:{name}"),
                ..entry(name, "host", "", 1, false)
            };
            for (other, mcp) in [
                (skill, vec![]),
                (
                    entry(name, "host", "", 3, false),
                    vec![mcp_tool("conn", "https://mcp.example.com/", "vendor", name)],
                ),
                (hosted, vec![]),
            ] {
                let (rows, _) =
                    targets_for_agent(Some(&deny_by_default()), &draft(), &[other], &mcp);
                let row = by_name(&rows, name);
                let kind = row.source_kind;
                // A hosted tool is a `builtin:` target of kind Host: still no default allow.
                assert_eq!(row.agent_rule, ScopeRule::Deny, "{name} {kind:?}");
            }
        }
    }

    #[test]
    fn notifications_list_is_offered_only_by_an_allow_rule() {
        let cat = [crate::notifications_tool::catalog_entry()];
        // No rule: the default deny holds, so the tool is not offered at all.
        let (rows, _) = targets_for_agent(Some(&deny_by_default()), &draft(), &cat, &[]);
        let row = by_name(&rows, "notifications_list");
        assert_eq!(row.target, "builtin:notifications_list");
        assert_ne!(row.agent_rule, ScopeRule::Allow);
        // The rule opts in.
        let mut permissions = deny_by_default();
        permissions.tools.as_mut().unwrap().rules = vec![PermissionRule {
            target: "builtin:notifications_list".to_string(),
            decision: PermissionDecision::Allow as i32,
        }];
        let (rows, _) = targets_for_agent(Some(&permissions), &draft(), &cat, &[]);
        assert_eq!(
            by_name(&rows, "notifications_list").agent_rule,
            ScopeRule::Allow
        );
    }

    #[test]
    fn a_rule_naming_the_outbox_tool_is_respected() {
        let mut permissions = deny_by_default();
        permissions.tools.as_mut().unwrap().rules = vec![PermissionRule {
            target: "builtin:post_message".to_string(),
            decision: PermissionDecision::Deny as i32,
        }];
        let (rows, _) = targets_for_agent(
            Some(&permissions),
            &draft(),
            &[
                entry("post_message", "builtin", "", 1, false),
                entry("show", "builtin", "", 1, false),
            ],
            &[],
        );
        assert_eq!(by_name(&rows, "post_message").agent_rule, ScopeRule::Deny);
        assert_eq!(by_name(&rows, "show").agent_rule, ScopeRule::Allow);
    }

    fn plan() -> Vec<PlannedMcpTool> {
        vec![mcp_tool(
            "atlassian",
            "https://MCP.Atlassian.com:443/",
            "create-issue",
            "atlassian__create_issue",
        )]
    }

    #[test]
    fn each_source_kind_gets_its_canonical_target() {
        let (rows, skipped) = targets_for_agent(Some(&defaults()), &draft(), &catalog(), &plan());
        assert!(skipped.is_empty(), "{skipped:?}");
        assert_eq!(by_name(&rows, "read_file").target, "builtin:read_file");
        assert_eq!(by_name(&rows, "read_file").source_kind, SourceKind::Builtin);
        assert_eq!(
            by_name(&rows, "step_complete").target,
            "builtin:step_complete"
        );
        assert_eq!(
            by_name(&rows, "jira_search").target,
            "skill:jira/jira_search"
        );
        assert_eq!(by_name(&rows, "jira_search").source_kind, SourceKind::Skill);
        assert_eq!(by_name(&rows, "echo_ping").source_kind, SourceKind::Host);
        let mcp = by_name(&rows, "atlassian__create_issue");
        assert_eq!(mcp.target, "mcp:mcp.atlassian.com/create-issue");
        assert_eq!(mcp.source_kind, SourceKind::Mcp);
    }

    #[test]
    fn mcp_target_survives_name_legalization_and_connection_rename() {
        // The guest name replaced `-` with `_`; the target keeps the vendor name
        // and the server id, so renaming the connection does not change it.
        let renamed = vec![mcp_tool(
            "jira-work",
            "mcp.atlassian.com",
            "create-issue",
            "jira_work__create_issue",
        )];
        let catalog = vec![entry("jira_work__create_issue", "host", "", 3, false)];
        let (rows, _) = targets_for_agent(Some(&defaults()), &draft(), &catalog, &renamed);
        assert_eq!(rows[0].target, "mcp:mcp.atlassian.com/create-issue");
    }

    #[test]
    fn default_permissions_allow_local_and_ask_for_mcp() {
        let (rows, _) = targets_for_agent(Some(&defaults()), &draft(), &catalog(), &plan());
        assert_eq!(by_name(&rows, "jira_search").agent_rule, ScopeRule::Allow);
        assert_eq!(by_name(&rows, "run").agent_rule, ScopeRule::Allow);
        let mcp = by_name(&rows, "atlassian__create_issue");
        assert_eq!(mcp.agent_rule, ScopeRule::Ask);
        assert_eq!(mcp.org_rule, ScopeRule::None);
        assert!(mcp.always_offered, "{mcp:?}");
        assert_eq!(mcp.locked_reason, "");
        assert_eq!(mcp.deciding_scope, "agent");
    }

    #[test]
    fn approval_required_under_allow_all_asks_and_locks_always() {
        let mut permissions = defaults();
        permissions
            .tools
            .as_mut()
            .unwrap()
            .rules
            .push(PermissionRule {
                target: "tools:*".to_string(),
                decision: PermissionDecision::Allow as i32,
            });
        let (rows, _) = targets_for_agent(Some(&permissions), &draft(), &catalog(), &plan());
        let tool = by_name(&rows, "jira_create_issue");
        assert_eq!(tool.agent_rule, ScopeRule::Allow);
        assert_eq!(tool.source_rule, ScopeRule::Ask);
        assert_eq!(tool.deciding_scope, "source");
        assert!(!tool.always_offered);
        assert!(tool.locked_reason.contains("skill jira"), "{tool:?}");
        assert!(tool.locked_reason.contains("jira_create_issue"), "{tool:?}");
    }

    #[test]
    fn mcp_ask_tools_is_a_source_rule() {
        let mut planned = plan();
        planned[0].ask = true;
        let mut permissions = defaults();
        permissions
            .tools
            .as_mut()
            .unwrap()
            .rules
            .push(PermissionRule {
                target: "mcp:mcp.atlassian.com/*".to_string(),
                decision: PermissionDecision::Allow as i32,
            });
        let (rows, _) = targets_for_agent(Some(&permissions), &draft(), &catalog(), &planned);
        let mcp = by_name(&rows, "atlassian__create_issue");
        assert_eq!(mcp.source_rule, ScopeRule::Ask);
        assert!(!mcp.always_offered);
        assert!(mcp.locked_reason.contains("atlassian"), "{mcp:?}");
    }

    #[test]
    fn released_run_never_offers_always() {
        let (rows, _) = targets_for_agent(Some(&defaults()), &released(), &catalog(), &plan());
        let mcp = by_name(&rows, "atlassian__create_issue");
        assert!(!mcp.always_offered);
        assert_eq!(mcp.locked_reason, RELEASED_LOCKED_REASON);
    }

    #[test]
    fn legacy_rows_have_no_rules_and_stamp_legacy_policy() {
        let (rows, _) = targets_for_agent(None, &released(), &catalog(), &plan());
        let tool = by_name(&rows, "jira_create_issue");
        assert_eq!(tool.target, "skill:jira/jira_create_issue");
        assert_eq!(tool.agent_rule, ScopeRule::None);
        assert_eq!(tool.source_rule, ScopeRule::None);
        assert!(!tool.always_offered);
        assert_eq!(tool.locked_reason, LEGACY_LOCKED_REASON);
    }

    fn web_catalog() -> Vec<ToolCatalogEntry> {
        ["web_search", "web_fetch"]
            .iter()
            .map(|name| ToolCatalogEntry {
                entrypoint: format!("hosted:vam.web:{name}"),
                ..entry(name, "host", "", SideEffectClass::ReadOnly as i32, false)
            })
            .collect()
    }

    #[test]
    fn web_tools_are_read_only_builtin_targets_allowed_without_asking() {
        let (rows, skipped) = targets_for_agent(Some(&defaults()), &draft(), &web_catalog(), &[]);
        assert!(skipped.is_empty(), "{skipped:?}");
        for name in ["web_search", "web_fetch"] {
            let row = by_name(&rows, name);
            assert_eq!(row.target, format!("builtin:{name}"));
            assert_eq!(row.source_kind, SourceKind::Host);
            assert_eq!(row.agent_rule, ScopeRule::Allow, "{row:?}");
            assert_eq!(row.source_rule, ScopeRule::None);
        }
    }

    #[test]
    fn explicit_rules_on_web_tools_still_apply() {
        let mut permissions = defaults();
        let rules = &mut permissions.tools.as_mut().unwrap().rules;
        rules.push(PermissionRule {
            target: "builtin:web_fetch".to_string(),
            decision: PermissionDecision::Ask as i32,
        });
        rules.push(PermissionRule {
            target: "builtin:web_search".to_string(),
            decision: PermissionDecision::Deny as i32,
        });
        let (rows, _) = targets_for_agent(Some(&permissions), &draft(), &web_catalog(), &[]);
        assert_eq!(by_name(&rows, "web_fetch").agent_rule, ScopeRule::Ask);
        assert_eq!(by_name(&rows, "web_search").agent_rule, ScopeRule::Deny);
    }

    #[test]
    fn a_tool_that_cannot_be_named_gets_no_row() {
        let catalog = vec![
            entry("ok_tool", "bun", "jira", 1, false),
            entry("odd_tool", "bun", "Jira Tools", 1, false),
            entry("orphan", "bun", "", 1, false),
        ];
        let (rows, skipped) = targets_for_agent(Some(&defaults()), &draft(), &catalog, &[]);
        assert_eq!(rows.len(), 1);
        assert_eq!(skipped.len(), 2);
    }

    #[test]
    fn rows_round_trip_through_the_store() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        let (rows, _) = targets_for_agent(Some(&defaults()), &draft(), &catalog(), &plan());
        store
            .create_session("s1", None, "vm", "ws", "storage-default")
            .expect("session");
        store
            .record_session_tool_targets("s1", &rows)
            .expect("record");
        let mut stored = store.list_session_tool_targets("s1").expect("list");
        let mut expected = rows.clone();
        stored.sort_by(|a, b| a.tool_name.cmp(&b.tool_name));
        expected.sort_by(|a, b| a.tool_name.cmp(&b.tool_name));
        assert_eq!(stored, expected);
        let one = target_for(&store, "s1", "atlassian__create_issue")
            .expect("lookup")
            .expect("row");
        assert_eq!(one.target, "mcp:mcp.atlassian.com/create-issue");
        assert_eq!(one.entity().target(), one.target);
        assert!(target_for(&store, "s1", "nope").expect("lookup").is_none());
        assert!(target_for(&store, "other", "run")
            .expect("lookup")
            .is_none());
    }

    // ---- "Always allow / deny" (AX-5.5a) ----

    use crate::users::LOCAL_USER_ID;
    use vam_spec::v1::{
        agent_spec::InstructionSource, AgentSpec, CapabilityPolicyRef, ResourceId, ResourceRef,
        RunSpec, Version,
    };

    const AGENT: &str = "urn:vam:agent:local.vam.dev/acme/remember-agent";
    const RUN: &str = "urn:vam:run:local.vam.dev/acme/remember-run";
    const SESSION: &str = "sess-remember";
    const TARGET: &str = "mcp:mcp.atlassian.com/create_issue";

    fn remember_agent(legacy: bool) -> AgentSpec {
        AgentSpec {
            resource_id: Some(ResourceId {
                urn: AGENT.to_string(),
            }),
            display_name: "remember".to_string(),
            description: "remember test".to_string(),
            role: "assistant".to_string(),
            instruction_source: Some(InstructionSource::SystemInstructions("x".to_string())),
            capability_policy: legacy.then(CapabilityPolicyRef::default),
            ..Default::default()
        }
    }

    /// A session whose run pins the agent's draft (or its 1.0.0 release), with a
    /// row for `atlassian__create_issue` that offers "Always".
    fn remember_store(draft_run: bool, legacy: bool) -> SqliteWorkflowStore {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        store
            .publish_agent_draft_with_owner(&remember_agent(legacy), LOCAL_USER_ID)
            .expect("draft");
        let version = if draft_run {
            Version {
                major: 1,
                prerelease: "draft".to_string(),
                ..Default::default()
            }
        } else {
            let released = Version {
                major: 1,
                ..Default::default()
            };
            store.release_agent(AGENT, &released).expect("release");
            released
        };
        store
            .create_session(SESSION, None, "vm", "ws", "storage-default")
            .expect("session");
        let spec = RunSpec {
            run_id: Some(ResourceId {
                urn: RUN.to_string(),
            }),
            agent_ref: Some(ResourceRef {
                resource_id: Some(ResourceId {
                    urn: AGENT.to_string(),
                }),
                version: Some(version),
                ..Default::default()
            }),
            ..Default::default()
        };
        let json = vam_spec::encoding::proto_to_json("vam.v1.RunSpec", &spec).expect("json");
        store
            .persist_run_spec_with_owner(
                RUN,
                &json,
                Some(SESSION),
                AGENT,
                None,
                "User::\"local\"",
                LOCAL_USER_ID,
            )
            .expect("run");
        let row = |name: &str, target: &str| SessionToolTarget {
            tool_name: name.to_string(),
            target: target.to_string(),
            source_kind: SourceKind::Mcp,
            side_effect: 3,
            source_rule: ScopeRule::None,
            source_label: String::new(),
            agent_rule: ScopeRule::Ask,
            agent_label: "agent draft: tools default".to_string(),
            org_rule: ScopeRule::None,
            deciding_scope: "agent".to_string(),
            deciding_rule: String::new(),
            always_offered: true,
            locked_reason: String::new(),
        };
        store
            .record_session_tool_targets(
                SESSION,
                &[
                    row("atlassian__create_issue", TARGET),
                    row("atlassian__other", "mcp:mcp.atlassian.com/other"),
                ],
            )
            .expect("rows");
        store
    }

    fn pending(always: bool, reason: &str, kind: &str) -> crate::error::StoredApproval {
        crate::error::StoredApproval {
            request_id: "appr-1".to_string(),
            run_id: RUN.to_string(),
            step_id: "s".to_string(),
            attempt_id: "a".to_string(),
            session_id: SESSION.to_string(),
            tool_name: "atlassian__create_issue".to_string(),
            approval_message: String::new(),
            requested_args_json: "{}".to_string(),
            amendable_schema_json: "{}".to_string(),
            args_digest: "d".to_string(),
            required_roles_json: "[]".to_string(),
            checkpoint_digest: "c".to_string(),
            continuation_generation: 0,
            created_at: 0,
            expires_at: None,
            decision: None,
            decision_revision: 0,
            reviewer_principal: None,
            amended_args_json: None,
            amended_args_digest: None,
            rejection_reason: None,
            published: true,
            approver_user_id: LOCAL_USER_ID.to_string(),
            kind: kind.to_string(),
            target: TARGET.to_string(),
            always_offered: always,
            locked_reason: reason.to_string(),
            card_json: None,
        }
    }

    fn draft_rules(store: &SqliteWorkflowStore) -> Vec<(String, i32)> {
        let draft = crate::catalog_publish::load_agent_draft(store, AGENT, LOCAL_USER_ID, false)
            .expect("draft");
        draft
            .permissions
            .and_then(|p| p.tools)
            .map(|t| {
                t.rules
                    .into_iter()
                    .map(|r| (r.target, r.decision))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn conflict_text(result: Result<RememberPlan, OrchestratorError>) -> String {
        match result {
            Err(OrchestratorError::Conflict(message)) => message,
            other => panic!("expected a 409 conflict, got {other:?}"),
        }
    }

    #[test]
    fn always_allow_writes_the_draft_rule_and_the_session_row() {
        let store = remember_store(true, false);
        let approval = pending(true, "", "tool_call");
        let plan = store
            .plan_remember(&approval, LOCAL_USER_ID, ScopeRule::Allow)
            .expect("plan");
        store.save_remembered_rule(&plan).expect("save");

        assert_eq!(
            draft_rules(&store),
            vec![(TARGET.to_string(), PermissionDecision::Allow as i32)]
        );
        let row = target_for(&store, SESSION, "atlassian__create_issue")
            .expect("lookup")
            .expect("row");
        assert_eq!(row.agent_rule, ScopeRule::Allow);
        assert_eq!(row.deciding_scope, "agent");
        assert_eq!(row.entity().target(), TARGET);
        // Only this tool's row changed.
        let other = target_for(&store, SESSION, "atlassian__other")
            .expect("lookup")
            .expect("row");
        assert_eq!(other.agent_rule, ScopeRule::Ask);
    }

    #[test]
    fn always_deny_writes_a_deny_rule() {
        let store = remember_store(true, false);
        let plan = store
            .plan_remember(
                &pending(true, "", "tool_call"),
                LOCAL_USER_ID,
                ScopeRule::Deny,
            )
            .expect("plan");
        store.save_remembered_rule(&plan).expect("save");
        assert_eq!(
            draft_rules(&store),
            vec![(TARGET.to_string(), PermissionDecision::Deny as i32)]
        );
        let row = target_for(&store, SESSION, "atlassian__create_issue")
            .expect("lookup")
            .expect("row");
        assert_eq!(row.agent_rule, ScopeRule::Deny);
    }

    #[test]
    fn a_retried_save_neither_duplicates_the_rule_nor_bumps_the_revision() {
        let store = remember_store(true, false);
        let plan = store
            .plan_remember(
                &pending(true, "", "tool_call"),
                LOCAL_USER_ID,
                ScopeRule::Allow,
            )
            .expect("plan");
        store.save_remembered_rule(&plan).expect("first save");
        let after_first =
            crate::catalog_publish::load_agent_draft(&store, AGENT, LOCAL_USER_ID, false)
                .expect("draft");
        store.save_remembered_rule(&plan).expect("retry");
        let after_retry =
            crate::catalog_publish::load_agent_draft(&store, AGENT, LOCAL_USER_ID, false)
                .expect("draft");
        assert_eq!(after_first, after_retry);
        assert_eq!(draft_rules(&store).len(), 1);
    }

    #[test]
    fn a_later_always_replaces_the_earlier_rule_for_the_same_target() {
        let store = remember_store(true, false);
        for rule in [ScopeRule::Allow, ScopeRule::Deny] {
            let plan = store
                .plan_remember(&pending(true, "", "tool_call"), LOCAL_USER_ID, rule)
                .expect("plan");
            store.save_remembered_rule(&plan).expect("save");
        }
        assert_eq!(
            draft_rules(&store),
            vec![(TARGET.to_string(), PermissionDecision::Deny as i32)]
        );
    }

    #[test]
    fn released_version_is_once_only_and_saves_nothing() {
        let store = remember_store(false, false);
        let before = draft_rules(&store);
        let message = conflict_text(store.plan_remember(
            &pending(false, RELEASED_LOCKED_REASON, "tool_call"),
            LOCAL_USER_ID,
            ScopeRule::Allow,
        ));
        assert!(message.contains("released version"), "{message}");
        assert!(message.contains("once only"), "{message}");
        assert_eq!(draft_rules(&store), before);
    }

    #[test]
    fn another_user_cannot_save_a_rule_on_the_owners_draft_run() {
        let store = remember_store(true, false);
        store.create_user("bob", "bob").expect("bob");
        let message = conflict_text(store.plan_remember(
            &pending(true, "", "tool_call"),
            "bob",
            ScopeRule::Allow,
        ));
        assert!(message.contains("owner"), "{message}");
        assert!(draft_rules(&store).is_empty());
    }

    #[test]
    fn a_locked_approval_names_the_locking_rule() {
        let store = remember_store(true, false);
        let reason = "skill jira marks create_issue approvalRequired; only Allow once is possible";
        let message = conflict_text(store.plan_remember(
            &pending(false, reason, "tool_call"),
            LOCAL_USER_ID,
            ScopeRule::Allow,
        ));
        assert!(message.contains("skill jira"), "{message}");
        assert!(message.contains("approvalRequired"), "{message}");
        assert!(draft_rules(&store).is_empty());
    }

    #[test]
    fn a_gate_cannot_be_remembered() {
        let store = remember_store(true, false);
        let message = conflict_text(store.plan_remember(
            &pending(true, "", "gate"),
            LOCAL_USER_ID,
            ScopeRule::Allow,
        ));
        assert!(message.contains("gate"), "{message}");
    }

    #[test]
    fn a_legacy_draft_has_no_rule_to_save() {
        let store = remember_store(true, true);
        let message = conflict_text(store.plan_remember(
            &pending(false, LEGACY_LOCKED_REASON, "tool_call"),
            LOCAL_USER_ID,
            ScopeRule::Allow,
        ));
        assert!(message.contains("capability_policy"), "{message}");
    }
}
