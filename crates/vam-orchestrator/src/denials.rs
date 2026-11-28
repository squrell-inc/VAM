//! Typed denials: the `vam.v1.Denial` a refusal carries, with a host-written fix.
//!
//! A refusal says what would fix it and who can do it, from host facts only: the deciding
//! rules, the agent's owner, the caller's role. Nothing a tool or the model wrote is ever copied
//! into a denial. The message text of the refusal itself is unchanged; the denial rides in
//! `ErrorStatus.details` (`type.googleapis.com/vam.v1.Denial`) and on `policy.decided`.
//!
//! A denial whose fix the caller cannot apply gets a `denials` row, and `decision_ref` is its
//! id (`POST /v1/fix-requests` resolves it later). A failed row write never changes the
//! refusal: the denial goes out with no `decision_ref`.
//!
//! The proto type is always written `vam_spec::v1::Denial`: `vam-network` has an unrelated
//! internal struct of the same name.

use prost::Message;
use prost_types::Any;
use rusqlite::{params, Connection};
use vam_security::ScopeRule;
use vam_spec::v1::{Denial, DenialReason, FixChange, FixRoute, FixWho, SuggestedFix};

use crate::error::OrchestratorError;
use crate::session_targets::SessionToolTarget;
use crate::workflow::SqliteWorkflowStore;

/// The `type_url` of a [`Denial`] in `ErrorStatus.details`.
pub const DENIAL_TYPE_URL: &str = "type.googleapis.com/vam.v1.Denial";
const MAX_SUBJECT_BYTES: usize = 253;

pub(crate) fn initialize_schema(connection: &Connection) -> Result<(), OrchestratorError> {
    connection.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS denials (
            id TEXT PRIMARY KEY NOT NULL,
            run_id TEXT,
            session_id TEXT NOT NULL,
            caller_user_id TEXT NOT NULL,
            agent_urn TEXT NOT NULL,
            agent_version TEXT NOT NULL DEFAULT '',
            reason TEXT NOT NULL,
            subject TEXT NOT NULL,
            deciding_scope TEXT NOT NULL,
            target TEXT NOT NULL,
            change TEXT NOT NULL,
            who TEXT NOT NULL,
            decided_by TEXT NOT NULL DEFAULT '',
            org_rule TEXT NOT NULL DEFAULT '',
            source_rule TEXT NOT NULL DEFAULT '',
            agent_rule TEXT NOT NULL DEFAULT '',
            created_at INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_denials_run ON denials(run_id);
        ",
    )?;
    Ok(())
}

// ---- text ------------------------------------------------------------------------------------

/// Which fixed sentence a denial carries. A fix is keyed `(reason, change, who)`; the text-only
/// kinds are the denials that have no fix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Text {
    Fix(DenialReason, FixChange, FixWho),
    /// An explicit agent-scope deny: the owner chose it, so no request is offered.
    AgentBlocked,
    /// An org-wide rule denies; no org rule write route exists yet.
    OrgBlocked,
    /// The agent still uses a legacy policy and has no permissions to change.
    LegacyPolicy,
    /// A marketplace-installed agent: its publisher sets the rule.
    Installed,
    /// A platform or skill rule, a path limit, a host fact: nobody can change it.
    BuiltIn,
    /// The tool needs a connector the node does not have.
    ConnectorUnknown,
    /// A marketplace-installed agent's egress: its publisher sets the hosts.
    InstalledHost,
    /// The session's egress policy could not be built or evaluated; the request is refused.
    EgressUnavailable,
    /// The caller's version pin is not the one the Space is bound to.
    VersionStale,
    /// The run's inbox holds as many undelivered messages as it may.
    InboxFull,
}

/// The host sentence for a denial. `subject` is a host identifier; `publisher` names the
/// publisher of an installed agent. An unknown fix combination yields an empty text.
pub fn host_text(text: Text, subject: &str, publisher: &str) -> String {
    use DenialReason as R;
    use FixChange as C;
    use FixWho as W;
    match text {
        Text::Fix(R::PolicyRule, C::AllowTool, W::AgentOwner) => {
            format!("The agent's owner can allow {subject} for this agent.")
        }
        Text::Fix(R::PolicyRule, C::AllowTool, W::Self_) => {
            format!("You can allow {subject} for this agent.")
        }
        Text::Fix(R::ConnectorDisabled, C::EnableConnector, W::OrgAdmin) => {
            format!("An org admin can enable {subject} for this organization.")
        }
        Text::Fix(R::ConnectorNotConnected, C::ConnectConnector, W::Self_) => {
            format!("You can connect {subject} to continue.")
        }
        Text::Fix(R::EgressRule, C::AllowHost, W::AgentOwner) => {
            format!("The agent's owner can allow the host {subject} for this agent.")
        }
        Text::Fix(R::SecretMissing, C::SetSecret, W::Self_) => {
            format!("You can set the secret {subject} for yourself.")
        }
        Text::Fix(R::SecretMissing, C::SetSecret, W::OrgAdmin) => {
            format!("An org admin can set the secret {subject} for this organization.")
        }
        Text::Fix(..) => String::new(),
        Text::InstalledHost => format!(
            "This agent was installed from {publisher}. Its publisher sets which hosts it may reach."
        ),
        Text::EgressUnavailable => {
            "The egress policy for this session could not be evaluated, so the request was refused."
                .into()
        }
        Text::VersionStale => format!("Run again on version {subject}, or omit the version."),
        Text::InboxFull => {
            "This run already has as many undelivered messages as it may. Ask an org admin if the limit needs raising."
                .into()
        }
        Text::AgentBlocked => format!("The agent's owner has blocked {subject} for this agent."),
        Text::OrgBlocked => {
            format!("An organization rule blocks {subject}. Org admins manage that rule.")
        }
        Text::LegacyPolicy => format!(
            "This agent still uses a legacy policy. Its owner must migrate it to permissions \
             before {subject} can be allowed."
        ),
        Text::Installed => format!(
            "This agent was installed from {publisher}. Its publisher sets which tools it may use."
        ),
        Text::BuiltIn => "This call is refused by a built-in rule; no setting can allow it.".into(),
        Text::ConnectorUnknown => {
            "This tool needs a connector this server does not have. Ask an org admin to add it."
                .into()
        }
    }
}

/// A host identifier as a subject: control characters dropped, at most 253 bytes.
pub fn sanitize_subject(raw: &str) -> String {
    let mut out = String::new();
    for c in raw.chars().filter(|c| !c.is_control()) {
        if out.len() + c.len_utf8() > MAX_SUBJECT_BYTES {
            break;
        }
        out.push(c);
    }
    out
}

/// The publisher of an agent URN: its namespace segment (`urn:vam:agent:<domain>/<ns>/<name>`).
pub fn publisher_of(urn: &str) -> String {
    let ns = urn
        .strip_prefix("urn:vam:")
        .and_then(|rest| rest.split_once(':'))
        .and_then(|(_kind, rest)| rest.split('/').nth(1))
        .unwrap_or_default();
    sanitize_subject(ns)
}

/// The existing write route that applies `change` to `target`, for API callers.
pub fn route_for(change: FixChange, target: &str) -> Option<FixRoute> {
    let route = |path: String| {
        Some(FixRoute {
            method: "PUT".into(),
            path,
        })
    };
    match change {
        FixChange::AllowTool | FixChange::AllowHost => {
            let (urn, _) = target.split_once('#')?;
            route(format!("/v1/agents/{urn}/permissions"))
        }
        FixChange::EnableConnector => route(format!("/v1/connectors/{target}/node")),
        FixChange::ConnectConnector => route(format!("/v1/connectors/{target}/connection")),
        _ => None,
    }
}

// ---- the policy decision ---------------------------------------------------------------------

/// The three scopes' rules for one tool, from the session's target row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rules {
    pub org: ScopeRule,
    pub source: ScopeRule,
    pub agent: ScopeRule,
    /// The agent's deny is its default (`tools.default: deny`), not a rule naming the tool.
    pub agent_default: bool,
}

/// What the host knows about the agent and the caller when a policy deny is explained.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentFacts {
    pub has_permissions: bool,
    /// `None` when the catalog could not say.
    pub installed: Option<bool>,
    /// The session owner is the agent's owner.
    pub caller_is_owner: bool,
    /// `plan_remember`'s checks all pass: a draft run the caller owns, "Always" offered.
    pub self_can_remember: bool,
}

/// The facts [`decide_policy`] reads. All host-side.
#[derive(Debug, Clone, Copy)]
pub struct PolicyFacts<'a> {
    pub decided_by: &'a str,
    /// `None`: legacy mode, no target row.
    pub rules: Option<Rules>,
    /// `None`: the agent could not be loaded.
    pub agent: Option<AgentFacts>,
}

/// The sentence and the fix (if any) for a policy deny.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Verdict {
    pub text: Text,
    pub fix: Option<(FixChange, FixWho)>,
}

/// Decides the fix for a tool refusal from the rules, never from the deciding scope label
/// alone. A fix is offered only when the agent has no rule for the tool and its default denies
/// it (or no scope has a rule at all), on a permissions agent that is not an install. A rule
/// that names the tool and denies it is the owner's choice and gets text only.
pub fn decide_policy(f: &PolicyFacts) -> Verdict {
    let none = |text| Verdict { text, fix: None };
    if f.decided_by != crate::run_facts::decided_by::CEDAR_RULE {
        return none(Text::BuiltIn);
    }
    let Some(agent) = f.agent else {
        return none(Text::BuiltIn);
    };
    let Some(rules) = f.rules else {
        // Legacy mode: only an agent that really has no `permissions` is told to migrate.
        return none(if agent.has_permissions {
            Text::BuiltIn
        } else {
            Text::LegacyPolicy
        });
    };
    if rules.source == ScopeRule::Deny {
        return none(Text::BuiltIn);
    }
    if rules.org == ScopeRule::Deny {
        return none(Text::OrgBlocked);
    }
    let default_deny = (rules.org == ScopeRule::None
        && rules.source == ScopeRule::None
        && rules.agent == ScopeRule::None)
        || (rules.agent == ScopeRule::Deny && rules.agent_default);
    if !(default_deny || rules.agent == ScopeRule::Deny) {
        return none(Text::BuiltIn);
    }
    match agent.installed {
        Some(false) => {}
        Some(true) => return none(Text::Installed),
        None => return none(Text::BuiltIn),
    }
    if !default_deny {
        return none(Text::AgentBlocked);
    }
    let who = if agent.self_can_remember {
        FixWho::Self_
    } else {
        FixWho::AgentOwner
    };
    Verdict {
        text: Text::Fix(DenialReason::PolicyRule, FixChange::AllowTool, who),
        fix: Some((FixChange::AllowTool, who)),
    }
}

// ---- building denials ------------------------------------------------------------------------

/// A denial with no fix: a host fact or a rule nobody can change. `subject` is `""` when the
/// name is not host-verified (a tool the model made up).
pub fn plain_denial(subject: &str, text: Text) -> Denial {
    let subject = sanitize_subject(subject);
    Denial {
        reason: DenialReason::PolicyRule as i32,
        text: host_text(text, &subject, ""),
        subject,
        fix: None,
        decision_ref: String::new(),
    }
}

/// Everything a stored denial remembers.
#[derive(Debug, Clone)]
pub struct NewDenial<'a> {
    pub run_id: Option<&'a str>,
    pub session_id: &'a str,
    pub caller_user_id: &'a str,
    pub agent_urn: &'a str,
    pub agent_version: &'a str,
    pub reason: DenialReason,
    pub subject: &'a str,
    pub deciding_scope: &'a str,
    pub target: &'a str,
    pub change: FixChange,
    pub who: FixWho,
    pub decided_by: &'a str,
    pub rules: Option<Rules>,
}

/// A stored denial as `POST /v1/fix-requests` reads it: the columns a request is re-derived
/// from. Everything here was written by the host at refusal time.
#[derive(Debug, Clone)]
pub struct StoredDenial {
    pub id: String,
    pub run_id: Option<String>,
    pub caller_user_id: String,
    pub agent_urn: String,
    pub reason: String,
    pub subject: String,
    pub target: String,
    pub change: String,
    pub who: String,
    pub created_at: i64,
}

impl SqliteWorkflowStore {
    /// Stores one denial and returns its id (`dnl_` plus 26 characters).
    pub fn insert_denial(&self, d: &NewDenial<'_>) -> Result<String, OrchestratorError> {
        let id = crate::spaces::mint_id("dnl_");
        let rule = |pick: fn(&Rules) -> ScopeRule| {
            d.rules.as_ref().map(|r| pick(r).as_str()).unwrap_or("")
        };
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        connection.execute(
            "INSERT INTO denials
               (id, run_id, session_id, caller_user_id, agent_urn, agent_version, reason, subject,
                deciding_scope, target, change, who, decided_by, org_rule, source_rule, agent_rule,
                created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
            params![
                id,
                d.run_id,
                d.session_id,
                d.caller_user_id,
                d.agent_urn,
                d.agent_version,
                d.reason.as_str_name(),
                d.subject,
                d.deciding_scope,
                d.target,
                d.change.as_str_name(),
                d.who.as_str_name(),
                d.decided_by,
                rule(|r| r.org),
                rule(|r| r.source),
                rule(|r| r.agent),
                crate::catalog::unix_timestamp(),
            ],
        )?;
        Ok(id)
    }

    /// One stored denial by id, for `POST /v1/fix-requests`. `None` when there is no such row;
    /// a storage failure is an error, never "not found".
    pub fn get_denial(&self, id: &str) -> Result<Option<StoredDenial>, OrchestratorError> {
        use rusqlite::OptionalExtension;
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        Ok(connection
            .query_row(
                "SELECT id, run_id, caller_user_id, agent_urn, reason, subject, target, change,
                        who, created_at
                 FROM denials WHERE id = ?1",
                [id],
                |r| {
                    Ok(StoredDenial {
                        id: r.get(0)?,
                        run_id: r.get(1)?,
                        caller_user_id: r.get(2)?,
                        agent_urn: r.get(3)?,
                        reason: r.get(4)?,
                        subject: r.get(5)?,
                        target: r.get(6)?,
                        change: r.get(7)?,
                        who: r.get(8)?,
                        created_at: r.get(9)?,
                    })
                },
            )
            .optional()?)
    }

    /// Whether `urn` is a marketplace install (read-only on this node).
    pub fn agent_is_installed(&self, urn: &str) -> Result<bool, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        crate::catalog_tier::is_installed_urn(&connection, urn)
    }
}

/// The caller (the session's owner) and the run bound to the session, from the session binding
/// only: the lease is echoed by the guest and never trusted for a stored row.
fn binding(store: &SqliteWorkflowStore, session_id: &str) -> (Option<String>, Option<String>) {
    let caller = store
        .session_owner_user_id(session_id)
        .ok()
        .map(|c| crate::owner::resolved_owner(&c).to_string());
    let run = store.find_run_id_for_session(session_id).ok().flatten();
    (caller, run)
}

fn caller_is_org_admin(store: &SqliteWorkflowStore, caller: &str) -> bool {
    store
        .get_user(caller)
        .map(|u| u.deactivated_at.is_none() && u.role.can_write_org_secrets())
        .unwrap_or(false)
}

/// Assembles the denial for a fix; writes the row when the caller cannot apply the fix.
#[allow(clippy::too_many_arguments)]
fn with_fix(
    store: &SqliteWorkflowStore,
    session_id: &str,
    agent_urn: &str,
    agent_version: &str,
    reason: DenialReason,
    subject: &str,
    scope: &str,
    row_target: &str,
    fix_target: &str,
    fix: (FixChange, FixWho),
    can_apply: bool,
    decided_by: &str,
    rules: Option<Rules>,
    text: Text,
) -> Denial {
    with_fix_rows(
        store,
        session_id,
        agent_urn,
        agent_version,
        reason,
        subject,
        scope,
        row_target,
        fix_target,
        fix,
        can_apply,
        decided_by,
        rules,
        text,
        true,
    )
}

/// [`with_fix`], with `write_row` false for a caller that has already written enough rows.
#[allow(clippy::too_many_arguments)]
fn with_fix_rows(
    store: &SqliteWorkflowStore,
    session_id: &str,
    agent_urn: &str,
    agent_version: &str,
    reason: DenialReason,
    subject: &str,
    scope: &str,
    row_target: &str,
    fix_target: &str,
    (change, who): (FixChange, FixWho),
    can_apply: bool,
    decided_by: &str,
    rules: Option<Rules>,
    text: Text,
    write_row: bool,
) -> Denial {
    let subject = sanitize_subject(subject);
    let mut decision_ref = String::new();
    if !can_apply && write_row {
        let (caller, run) = binding(store, session_id);
        if let Some(caller) = caller {
            let row = NewDenial {
                run_id: run.as_deref(),
                session_id,
                caller_user_id: &caller,
                agent_urn,
                agent_version,
                reason,
                subject: &subject,
                deciding_scope: scope,
                target: row_target,
                change,
                who,
                decided_by,
                rules,
            };
            match store.insert_denial(&row) {
                Ok(id) => decision_ref = id,
                Err(error) => tracing::warn!(
                    session_id,
                    %error,
                    "could not store a denial; the refusal goes out without a decision reference"
                ),
            }
        }
    }
    Denial {
        reason: reason as i32,
        text: host_text(text, &subject, &publisher_of(agent_urn)),
        subject,
        fix: Some(SuggestedFix {
            change: change as i32,
            target: fix_target.to_string(),
            who: who as i32,
            route: route_for(change, fix_target),
            caller_can_apply: can_apply,
        }),
        decision_ref,
    }
}

/// The denial for a refusal at the tool's permission decision (the final deny of the intent).
/// `tool_name` is a catalog tool. `scope` is the deciding scope of the run's decision record.
pub fn policy_denial(
    store: &SqliteWorkflowStore,
    session_id: &str,
    agent_urn: &str,
    tool_name: &str,
    decided_by: &str,
    scope: &str,
    row: Option<&SessionToolTarget>,
) -> Denial {
    if decided_by != crate::run_facts::decided_by::CEDAR_RULE {
        return plain_denial(tool_name, Text::BuiltIn);
    }
    let (caller, _) = binding(store, session_id);
    let session = crate::policy_legacy::load_session_agent(store, session_id, None).ok();
    let agent = session.as_ref().and_then(|s| {
        let spec = s.agent.as_ref()?;
        let caller = caller.as_deref()?;
        let owner = store.agent_owner_user_id(agent_urn).ok();
        let caller_is_owner = owner.as_deref() == Some(caller);
        let self_can_remember = s.draft_run
            && s.resolve_owner == caller
            && caller_is_owner
            && spec.permissions.is_some()
            && row.is_some_and(|r| r.always_offered && !r.target.is_empty());
        Some(AgentFacts {
            has_permissions: spec.permissions.is_some(),
            installed: store.agent_is_installed(agent_urn).ok(),
            caller_is_owner,
            self_can_remember,
        })
    });
    let rules = row.map(|r| Rules {
        org: r.org_rule,
        source: r.source_rule,
        agent: r.agent_rule,
        agent_default: r.agent_deny_is_default(),
    });
    let verdict = decide_policy(&PolicyFacts {
        decided_by,
        rules,
        agent,
    });
    let (Some((change, who)), Some(agent)) = (verdict.fix, agent) else {
        let subject = sanitize_subject(tool_name);
        return Denial {
            reason: DenialReason::PolicyRule as i32,
            text: host_text(verdict.text, &subject, &publisher_of(agent_urn)),
            subject,
            fix: None,
            decision_ref: String::new(),
        };
    };
    let perm_target = row.map(|r| r.target.as_str()).unwrap_or(tool_name);
    // An MCP permission target is kept in the row only; the denial carries the tool name.
    let shown = if perm_target.starts_with("mcp:") {
        tool_name
    } else {
        perm_target
    };
    let fix_target = format!("{agent_urn}#{shown}");
    let version = session
        .as_ref()
        .and_then(|s| s.run_spec.as_ref())
        .and_then(|spec| crate::policy_legacy::pinned_agent_version(spec, agent_urn))
        .map(crate::security::version_text)
        .unwrap_or_default();
    with_fix(
        store,
        session_id,
        agent_urn,
        &version,
        DenialReason::PolicyRule,
        tool_name,
        scope,
        perm_target,
        &fix_target,
        (change, who),
        who == FixWho::Self_ || agent.caller_is_owner,
        decided_by,
        rules,
        verdict.text,
    )
}

/// The denial for a tool whose connector the node has but the organization has not enabled.
pub fn connector_disabled_denial(
    store: &SqliteWorkflowStore,
    session_id: &str,
    agent_urn: &str,
    connector_id: &str,
) -> Denial {
    let (caller, _) = binding(store, session_id);
    let can_apply = caller
        .as_deref()
        .is_some_and(|c| caller_is_org_admin(store, c));
    with_fix(
        store,
        session_id,
        agent_urn,
        "",
        DenialReason::ConnectorDisabled,
        connector_id,
        "org",
        connector_id,
        connector_id,
        (FixChange::EnableConnector, FixWho::OrgAdmin),
        can_apply,
        "connector_gate",
        None,
        Text::Fix(
            DenialReason::ConnectorDisabled,
            FixChange::EnableConnector,
            FixWho::OrgAdmin,
        ),
    )
}

/// The denial for a tool whose connector this node does not have: nothing to enable.
pub fn connector_unknown_denial() -> Denial {
    Denial {
        reason: DenialReason::ConnectorDisabled as i32,
        subject: String::new(),
        fix: None,
        text: host_text(Text::ConnectorUnknown, "", ""),
        decision_ref: String::new(),
    }
}

/// The denial a connector park carries: the member connects, so no row and no reference.
pub fn connector_not_connected_denial(connector_id: &str) -> Denial {
    let subject = sanitize_subject(connector_id);
    Denial {
        reason: DenialReason::ConnectorNotConnected as i32,
        text: host_text(
            Text::Fix(
                DenialReason::ConnectorNotConnected,
                FixChange::ConnectConnector,
                FixWho::Self_,
            ),
            &subject,
            "",
        ),
        fix: Some(SuggestedFix {
            change: FixChange::ConnectConnector as i32,
            target: subject.clone(),
            who: FixWho::Self_ as i32,
            route: route_for(FixChange::ConnectConnector, &subject),
            caller_can_apply: true,
        }),
        subject,
        decision_ref: String::new(),
    }
}

// ---- denials outside a tool intent -----------------------------------------------------------

fn no_fix(reason: DenialReason, subject: &str, text: Text, publisher: &str) -> Denial {
    let subject = sanitize_subject(subject);
    Denial {
        reason: reason as i32,
        text: host_text(text, &subject, publisher),
        subject,
        fix: None,
        decision_ref: String::new(),
    }
}

fn secret_who(scope: &str) -> FixWho {
    // A user-scope secret is the caller's own. `either` is satisfied by the user's own
    // binding, so the caller can fix it themselves. Only an org-scope secret needs an admin.
    if scope.trim().eq_ignore_ascii_case("org") {
        FixWho::OrgAdmin
    } else {
        FixWho::Self_
    }
}

fn secret_missing(binding: &str, who: FixWho, can_apply: bool, decision_ref: String) -> Denial {
    let subject = sanitize_subject(binding);
    Denial {
        reason: DenialReason::SecretMissing as i32,
        text: host_text(
            Text::Fix(DenialReason::SecretMissing, FixChange::SetSecret, who),
            &subject,
            "",
        ),
        fix: Some(SuggestedFix {
            change: FixChange::SetSecret as i32,
            target: subject.clone(),
            who: who as i32,
            route: route_for(FixChange::SetSecret, &subject),
            caller_can_apply: can_apply,
        }),
        subject,
        decision_ref,
    }
}

/// The denial for one missing secret when no caller is known: no row, no reference. `scope` is
/// `org`, `user` or `either`.
pub fn secret_missing_plain(binding: &str, scope: &str) -> Denial {
    let who = secret_who(scope);
    secret_missing(binding, who, who == FixWho::Self_, String::new())
}

/// One denial per missing secret of a run start. The caller can set a user-scope secret
/// itself; an org-scope one needs an org admin, so a caller who is not one gets a stored row
/// (`run_id` NULL, `session_id` empty: no session exists yet) and its id as `decision_ref`.
pub fn secret_missing_denials(
    store: &SqliteWorkflowStore,
    caller_user_id: &str,
    agent_urn: &str,
    items: &[(String, String)],
) -> Vec<Denial> {
    let caller = crate::owner::resolved_owner(caller_user_id).to_string();
    let is_admin = caller_is_org_admin(store, &caller);
    items
        .iter()
        .map(|(binding, scope)| {
            let who = secret_who(scope);
            let can_apply = who == FixWho::Self_ || is_admin;
            let mut decision_ref = String::new();
            if !can_apply {
                let subject = sanitize_subject(binding);
                let row = NewDenial {
                    run_id: None,
                    session_id: "",
                    caller_user_id: &caller,
                    agent_urn,
                    agent_version: "",
                    reason: DenialReason::SecretMissing,
                    subject: &subject,
                    deciding_scope: "org",
                    target: &subject,
                    change: FixChange::SetSecret,
                    who,
                    decided_by: "admission",
                    rules: None,
                };
                match store.insert_denial(&row) {
                    Ok(id) => decision_ref = id,
                    Err(error) => tracing::warn!(
                        %error,
                        "could not store a denial; the refusal goes out without a decision reference"
                    ),
                }
            }
            secret_missing(binding, who, can_apply, decision_ref)
        })
        .collect()
}

/// The denial for a run asked on a version other than the one its Space is bound to. The
/// caller fixes it by running on the bound version (or leaving the version out). There is no
/// `change`: `SET_SPACE_VERSION` is reserved for the Space version work, and no route applies
/// this fix, so `fix.target` carries the bound version for a client to name.
pub fn version_stale_denial(bound: &str) -> Denial {
    let subject = sanitize_subject(bound);
    Denial {
        reason: DenialReason::VersionStale as i32,
        text: host_text(Text::VersionStale, &subject, ""),
        fix: Some(SuggestedFix {
            change: FixChange::Unspecified as i32,
            target: subject.clone(),
            who: FixWho::Self_ as i32,
            route: None,
            caller_can_apply: true,
        }),
        subject,
        decision_ref: String::new(),
    }
}

/// The denial for a run whose inbox is full: a quota, nothing the caller can change.
pub fn inbox_full_denial(run_id: &str) -> Denial {
    no_fix(DenialReason::Quota, run_id, Text::InboxFull, "")
}

// ---- egress ----------------------------------------------------------------------------------

/// What decided an egress refusal, read from the rule on the `egress.decided` policy id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EgressKind {
    /// The agent's `internet:*` default denies and no rule names the host.
    AgentDefault,
    /// A rule the agent's owner wrote for this host denies it.
    AgentRule,
    /// A platform or secret-destination rule, the IP floor, the port list: nobody sets it here.
    Fixed,
    /// The session's policy could not be built or evaluated.
    Unavailable,
    /// The name did not resolve: not a policy decision.
    NameResolution,
}

/// Classifies a policy id of the form `rule=<rule>; <policy>` (the label `vam-network` writes).
pub fn egress_kind(policy_label: &str) -> EgressKind {
    let rule = policy_label
        .strip_prefix("rule=")
        .and_then(|rest| rest.rsplit_once("; "))
        .map(|(rule, _)| rule)
        .unwrap_or("");
    match rule {
        "agent rule internet:*" => EgressKind::AgentDefault,
        r if r.starts_with("agent rule ") => EgressKind::AgentRule,
        "egress policy unavailable" | "egress policy error" => EgressKind::Unavailable,
        "name resolution" => EgressKind::NameResolution,
        _ => EgressKind::Fixed,
    }
}

/// A host name usable in a fix target: lower-case letters, digits, dot, dash, underscore.
pub(crate) fn is_fixable_host(host: &str) -> bool {
    !host.is_empty()
        && host.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'-' | b'_')
        })
}

/// The denial for a refused host, or `None` when the refusal is not a policy decision (the name
/// did not resolve). `host` is what the guest asked for and is untrusted: it is normalized,
/// stripped of control characters and capped before it becomes a subject. The caller, run and
/// agent come from the session binding.
pub fn egress_denial(
    store: &SqliteWorkflowStore,
    session_id: &str,
    principal_urn: Option<&str>,
    host: &str,
    policy_label: &str,
) -> Option<Denial> {
    build_egress_denial(store, session_id, principal_urn, host, policy_label, true)
}

/// [`egress_denial`] that stores no row (the caller has hit its per-session cap).
pub fn egress_denial_without_row(
    store: &SqliteWorkflowStore,
    session_id: &str,
    principal_urn: Option<&str>,
    host: &str,
    policy_label: &str,
) -> Option<Denial> {
    build_egress_denial(store, session_id, principal_urn, host, policy_label, false)
}

fn build_egress_denial(
    store: &SqliteWorkflowStore,
    session_id: &str,
    principal_urn: Option<&str>,
    host: &str,
    policy_label: &str,
    write_row: bool,
) -> Option<Denial> {
    let kind = egress_kind(policy_label);
    let host = sanitize_subject(
        host.trim()
            .trim_end_matches('.')
            .to_ascii_lowercase()
            .as_str(),
    );
    let plain = |text: Text, publisher: &str| {
        Some(no_fix(DenialReason::EgressRule, &host, text, publisher))
    };
    match kind {
        EgressKind::NameResolution => return None,
        EgressKind::Unavailable => return plain(Text::EgressUnavailable, ""),
        EgressKind::Fixed => return plain(Text::BuiltIn, ""),
        EgressKind::AgentDefault | EgressKind::AgentRule => {}
    }
    let Some(urn) = crate::security::session_agent_urn(store, session_id, principal_urn)
        .ok()
        .flatten()
    else {
        return plain(Text::BuiltIn, "");
    };
    match store.agent_is_installed(&urn) {
        Ok(false) => {}
        Ok(true) => return plain(Text::InstalledHost, &publisher_of(&urn)),
        Err(_) => return plain(Text::BuiltIn, ""),
    }
    if kind == EgressKind::AgentRule {
        // The owner wrote this rule: text only, no request to undo it.
        return plain(Text::AgentBlocked, "");
    }
    if !is_fixable_host(&host) {
        return plain(Text::BuiltIn, "");
    }
    let (caller, _) = binding(store, session_id);
    let can_apply = match (caller.as_deref(), store.agent_owner_user_id(&urn).ok()) {
        (Some(caller), Some(owner)) => caller == owner,
        _ => false,
    };
    Some(with_fix_rows(
        store,
        session_id,
        &urn,
        "",
        DenialReason::EgressRule,
        &host,
        "agent",
        &host,
        &format!("{urn}#{host}"),
        (FixChange::AllowHost, FixWho::AgentOwner),
        can_apply,
        "egress",
        None,
        Text::Fix(
            DenialReason::EgressRule,
            FixChange::AllowHost,
            FixWho::AgentOwner,
        ),
        write_row,
    ))
}

// ---- wire forms ------------------------------------------------------------------------------

/// The `Any` for `ErrorStatus.details`.
pub fn pack_any(d: &Denial) -> Any {
    Any {
        type_url: DENIAL_TYPE_URL.to_string(),
        value: d.encode_to_vec(),
    }
}

/// The denial as proto JSON (camelCase keys, enum names), for event payloads. `None` when the
/// encoder fails: the event is then written without a denial and the refusal is unchanged.
pub fn denial_json(d: &Denial) -> Option<serde_json::Value> {
    let text = vam_spec::encoding::proto_to_json("vam.v1.Denial", d).ok()?;
    serde_json::from_str(&text).ok()
}

/// The denial for an event of the lease's run: only when that run is the one bound to the
/// session. The lease is echoed by the guest, so a mismatch gets the event without a denial.
pub fn event_denial(
    store: &SqliteWorkflowStore,
    session_id: &str,
    lease_run_id: &str,
    d: &Denial,
) -> Option<serde_json::Value> {
    if lease_run_id.is_empty() {
        return None;
    }
    let bound = store.find_run_id_for_session(session_id).ok().flatten()?;
    (bound == lease_run_id).then(|| denial_json(d)).flatten()
}

// ---- who to tell -----------------------------------------------------------------------------

/// The users a fix request for `who` goes to. An installed agent has no owner who can change
/// it, so AGENT_OWNER yields nobody for it; SELF has no recipients. Agent access replaces the
/// AGENT_OWNER arm with the agent's admins.
pub fn fix_recipients(
    store: &SqliteWorkflowStore,
    who: FixWho,
    agent_urn: &str,
) -> Result<Vec<String>, OrchestratorError> {
    match who {
        FixWho::AgentOwner => {
            if store.agent_is_installed(agent_urn)? {
                return Ok(Vec::new());
            }
            Ok(vec![store.agent_owner_user_id(agent_urn)?])
        }
        FixWho::OrgAdmin => Ok(store
            .list_users()?
            .into_iter()
            .filter(|u| u.deactivated_at.is_none() && u.role.can_write_org_secrets())
            .map(|u| u.user_id)
            .collect()),
        FixWho::Self_ | FixWho::Unspecified => Ok(Vec::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run_facts::decided_by as by;
    use crate::users::UserRole;

    const URN: &str = "urn:vam:agent:hub.example.dev/acme/helper";

    fn agent(installed: Option<bool>, owner: bool, own_draft: bool) -> Option<AgentFacts> {
        Some(AgentFacts {
            has_permissions: true,
            installed,
            caller_is_owner: owner,
            self_can_remember: own_draft,
        })
    }

    fn rules(org: ScopeRule, source: ScopeRule, agent: ScopeRule) -> Option<Rules> {
        Some(Rules {
            org,
            source,
            agent,
            agent_default: false,
        })
    }

    /// The agent denies by its `tools.default`, with no rule naming the tool.
    fn default_deny() -> Option<Rules> {
        Some(Rules {
            org: NONE,
            source: NONE,
            agent: ScopeRule::Deny,
            agent_default: true,
        })
    }

    fn verdict(r: Option<Rules>, a: Option<AgentFacts>) -> Verdict {
        decide_policy(&PolicyFacts {
            decided_by: by::CEDAR_RULE,
            rules: r,
            agent: a,
        })
    }

    const NONE: ScopeRule = ScopeRule::None;

    #[test]
    fn the_platform_default_offers_allow_tool_to_the_owner() {
        let v = verdict(rules(NONE, NONE, NONE), agent(Some(false), false, false));
        assert_eq!(v.fix, Some((FixChange::AllowTool, FixWho::AgentOwner)));
        assert_eq!(
            v.text,
            Text::Fix(
                DenialReason::PolicyRule,
                FixChange::AllowTool,
                FixWho::AgentOwner
            )
        );
    }

    #[test]
    fn a_default_deny_by_the_agents_tools_default_offers_allow_tool() {
        let v = verdict(default_deny(), agent(Some(false), false, false));
        assert_eq!(v.fix, Some((FixChange::AllowTool, FixWho::AgentOwner)));
        // An installed agent: the same default is its publisher's.
        let v = verdict(default_deny(), agent(Some(true), false, false));
        assert_eq!((v.fix, v.text), (None, Text::Installed));
    }

    #[test]
    fn the_owners_own_draft_run_is_self() {
        let v = verdict(rules(NONE, NONE, NONE), agent(Some(false), true, true));
        assert_eq!(v.fix, Some((FixChange::AllowTool, FixWho::Self_)));
    }

    #[test]
    fn an_explicit_agent_deny_is_text_only() {
        let v = verdict(
            rules(NONE, NONE, ScopeRule::Deny),
            agent(Some(false), true, true),
        );
        assert_eq!(v.fix, None);
        assert_eq!(v.text, Text::AgentBlocked);
    }

    #[test]
    fn an_org_deny_is_text_only_whatever_the_agent_says() {
        for agent_rule in [NONE, ScopeRule::Allow, ScopeRule::Deny, ScopeRule::Ask] {
            let v = verdict(
                rules(ScopeRule::Deny, NONE, agent_rule),
                agent(Some(false), true, true),
            );
            assert_eq!((v.fix, v.text), (None, Text::OrgBlocked), "{agent_rule:?}");
        }
    }

    #[test]
    fn a_source_deny_wins_every_tie_and_has_no_fix() {
        for (org, agent_rule) in [
            (NONE, NONE),
            (ScopeRule::Deny, ScopeRule::Deny),
            (ScopeRule::Deny, NONE),
            (NONE, ScopeRule::Deny),
        ] {
            let v = verdict(
                rules(org, ScopeRule::Deny, agent_rule),
                agent(Some(false), true, true),
            );
            assert_eq!((v.fix, v.text), (None, Text::BuiltIn));
        }
    }

    #[test]
    fn a_deny_that_is_not_the_rules_has_no_fix() {
        // A Cedar deny for another reason (budget, iteration, args) under rules that allow or ask.
        for (org, source, agent_rule) in [
            (NONE, NONE, ScopeRule::Allow),
            (NONE, NONE, ScopeRule::Ask),
            (NONE, ScopeRule::Ask, NONE),
            (NONE, ScopeRule::Allow, NONE),
            (ScopeRule::Allow, NONE, NONE),
        ] {
            let v = verdict(
                rules(org, source, agent_rule),
                agent(Some(false), true, true),
            );
            assert_eq!(v.fix, None, "{org:?} {source:?} {agent_rule:?}");
        }
    }

    #[test]
    fn only_a_cedar_rule_deny_can_have_a_fix() {
        for decided_by in [
            by::PATH_LIMB,
            by::PLAYBOOK_GUARD,
            by::HOST_ERROR,
            by::GATE,
            by::MISSING_PERMISSIONS_ROW,
        ] {
            let v = decide_policy(&PolicyFacts {
                decided_by,
                rules: rules(NONE, NONE, NONE),
                agent: agent(Some(false), true, true),
            });
            assert_eq!((v.fix, v.text), (None, Text::BuiltIn), "{decided_by}");
        }
    }

    #[test]
    fn an_installed_agent_gets_the_publisher_and_no_fix() {
        for agent_rule in [NONE, ScopeRule::Deny] {
            let v = verdict(
                rules(NONE, NONE, agent_rule),
                agent(Some(true), true, false),
            );
            assert_eq!((v.fix, v.text), (None, Text::Installed));
        }
        // The catalog could not say: no fix, the generic text.
        let v = verdict(rules(NONE, NONE, NONE), agent(None, true, false));
        assert_eq!((v.fix, v.text), (None, Text::BuiltIn));
    }

    #[test]
    fn legacy_mode_names_migration_only_for_an_agent_without_permissions() {
        let mut a = agent(Some(false), true, false);
        if let Some(f) = a.as_mut() {
            f.has_permissions = false;
        }
        let v = verdict(None, a);
        assert_eq!((v.fix, v.text), (None, Text::LegacyPolicy));
        let v = verdict(None, agent(Some(false), true, false));
        assert_eq!((v.fix, v.text), (None, Text::BuiltIn));
        let v = verdict(None, None);
        assert_eq!((v.fix, v.text), (None, Text::BuiltIn));
    }

    #[test]
    fn the_publisher_is_the_namespace_segment() {
        assert_eq!(publisher_of(URN), "acme");
        assert_eq!(publisher_of("not a urn"), "");
        assert_eq!(publisher_of("urn:vam:agent:only-domain"), "");
    }

    #[test]
    fn every_host_text_is_filled_and_plain() {
        let texts = [
            Text::Fix(
                DenialReason::PolicyRule,
                FixChange::AllowTool,
                FixWho::AgentOwner,
            ),
            Text::Fix(
                DenialReason::PolicyRule,
                FixChange::AllowTool,
                FixWho::Self_,
            ),
            Text::Fix(
                DenialReason::ConnectorDisabled,
                FixChange::EnableConnector,
                FixWho::OrgAdmin,
            ),
            Text::Fix(
                DenialReason::ConnectorNotConnected,
                FixChange::ConnectConnector,
                FixWho::Self_,
            ),
            Text::AgentBlocked,
            Text::OrgBlocked,
            Text::LegacyPolicy,
            Text::Installed,
            Text::BuiltIn,
            Text::ConnectorUnknown,
        ];
        for t in texts {
            let s = host_text(t, "run", "acme");
            assert!(!s.is_empty(), "{t:?}");
            assert!(!s.contains('{') && !s.contains('}'), "{s}");
            assert!(!s.chars().any(char::is_control), "{s}");
        }
        assert_eq!(
            host_text(
                Text::Fix(
                    DenialReason::PolicyRule,
                    FixChange::AllowTool,
                    FixWho::AgentOwner
                ),
                "run",
                ""
            ),
            "The agent's owner can allow run for this agent."
        );
        assert_eq!(
            host_text(Text::Installed, "run", "acme"),
            "This agent was installed from acme. Its publisher sets which tools it may use."
        );
        assert_eq!(
            host_text(Text::ConnectorUnknown, "", ""),
            "This tool needs a connector this server does not have. Ask an org admin to add it."
        );
        // A combination with no sentence is empty, not invented.
        assert_eq!(
            host_text(
                Text::Fix(DenialReason::Quota, FixChange::RaiseLimit, FixWho::OrgAdmin),
                "x",
                ""
            ),
            ""
        );
    }

    #[test]
    fn subjects_lose_control_characters_and_stop_at_253_bytes() {
        assert_eq!(
            sanitize_subject("run\nSuggested\u{0}fix\r"),
            "runSuggestedfix"
        );
        let long = "é".repeat(200);
        let s = sanitize_subject(&long);
        assert!(s.len() <= MAX_SUBJECT_BYTES);
        assert!(s.chars().all(|c| c == 'é'));
    }

    #[test]
    fn routes_are_the_existing_write_routes() {
        let r = route_for(FixChange::AllowTool, &format!("{URN}#builtin:run")).expect("route");
        assert_eq!(
            (r.method.as_str(), r.path.as_str()),
            (
                "PUT",
                "/v1/agents/urn:vam:agent:hub.example.dev/acme/helper/permissions"
            )
        );
        let r = route_for(FixChange::EnableConnector, "microsoft-365").expect("route");
        assert_eq!(r.path, "/v1/connectors/microsoft-365/node");
        let r = route_for(FixChange::ConnectConnector, "microsoft-365").expect("route");
        assert_eq!(r.path, "/v1/connectors/microsoft-365/connection");
        assert!(route_for(FixChange::StopRun, "x").is_none());
        assert!(route_for(FixChange::AllowTool, "no-hash").is_none());
    }

    #[test]
    fn the_json_names_are_the_ones_clients_and_scenarios_read() {
        let d = Denial {
            reason: DenialReason::PolicyRule as i32,
            subject: "run".into(),
            fix: Some(SuggestedFix {
                change: FixChange::AllowTool as i32,
                target: "t".into(),
                who: FixWho::OrgAdmin as i32,
                route: None,
                caller_can_apply: true,
            }),
            text: "x".into(),
            decision_ref: "dnl_abc".into(),
        };
        let j = denial_json(&d).expect("json");
        assert_eq!(j["reason"], "POLICY_RULE");
        assert_eq!(j["fix"]["who"], "ORG_ADMIN");
        assert_eq!(j["fix"]["change"], "ALLOW_TOOL");
        assert_eq!(j["fix"]["callerCanApply"], true);
        assert_eq!(j["decisionRef"], "dnl_abc");
        let mut none = d;
        none.fix = None;
        assert!(denial_json(&none).expect("json").get("fix").is_none());
    }

    #[test]
    fn the_any_carries_the_type_url_and_round_trips() {
        let d = connector_not_connected_denial("microsoft-365");
        let any = pack_any(&d);
        assert_eq!(any.type_url, DENIAL_TYPE_URL);
        let desc = vam_spec::encoding::DESCRIPTOR_POOL
            .get_message_by_name("vam.v1.Denial")
            .expect("descriptor");
        assert_eq!(
            any.type_url,
            format!("type.googleapis.com/{}", desc.full_name())
        );
        assert_eq!(Denial::decode(any.value.as_slice()).expect("decode"), d);
        let fix = d.fix.expect("fix");
        assert!(fix.caller_can_apply);
        assert_eq!(
            fix.route.expect("route").path,
            "/v1/connectors/microsoft-365/connection"
        );
        assert!(d.decision_ref.is_empty());
    }

    fn store() -> SqliteWorkflowStore {
        SqliteWorkflowStore::in_memory().expect("store")
    }

    fn new_denial<'a>(run: Option<&'a str>) -> NewDenial<'a> {
        NewDenial {
            run_id: run,
            session_id: "s1",
            caller_user_id: "u1",
            agent_urn: URN,
            agent_version: "1.0.0",
            reason: DenialReason::PolicyRule,
            subject: "run",
            deciding_scope: "platform",
            target: "builtin:run",
            change: FixChange::AllowTool,
            who: FixWho::AgentOwner,
            decided_by: by::CEDAR_RULE,
            rules: default_deny(),
        }
    }

    #[test]
    fn a_denial_row_stores_the_fix_and_the_rules() {
        let st = store();
        let id = st.insert_denial(&new_denial(Some("run-1"))).expect("row");
        assert!(id.starts_with("dnl_") && id.len() == 4 + 26, "{id}");
        let none_run = st.insert_denial(&new_denial(None)).expect("row");
        let c = st.connection.lock().expect("lock");
        let (run, change, who, agent_rule, by_): (Option<String>, String, String, String, String) =
            c.query_row(
                "SELECT run_id, change, who, agent_rule, decided_by FROM denials WHERE id = ?1",
                [&id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .expect("read");
        assert_eq!(run.as_deref(), Some("run-1"));
        assert_eq!(
            (change.as_str(), who.as_str()),
            ("ALLOW_TOOL", "AGENT_OWNER")
        );
        assert_eq!((agent_rule.as_str(), by_.as_str()), ("deny", "cedar_rule"));
        let null_run: Option<String> = c
            .query_row(
                "SELECT run_id FROM denials WHERE id = ?1",
                [&none_run],
                |r| r.get(0),
            )
            .expect("read");
        assert_eq!(null_run, None);
    }

    #[test]
    fn a_failed_row_write_leaves_the_denial_with_its_fix_and_no_reference() {
        let st = store();
        st.create_session("s1", None, "vm", "ws", "storage-default")
            .expect("session");
        st.connection
            .lock()
            .expect("lock")
            .execute_batch("DROP TABLE denials;")
            .expect("drop");
        // A member (not an admin) is refused for a disabled connector: a fix they cannot apply.
        let d = connector_disabled_denial(&st, "s1", URN, "microsoft-365");
        assert!(d.decision_ref.is_empty());
        let fix = d.fix.expect("the fix stays");
        assert_eq!(fix.change, FixChange::EnableConnector as i32);
        assert_eq!(fix.who, FixWho::OrgAdmin as i32);
        assert!(!fix.caller_can_apply);
        assert_eq!(d.subject, "microsoft-365");
    }

    #[test]
    fn a_member_gets_a_reference_and_an_admin_does_not() {
        let st = store();
        st.create_user_with_role("member1", "member1", UserRole::Member)
            .expect("member");
        st.create_user_with_role("admin1", "admin1", UserRole::OrgAdmin)
            .expect("admin");
        for (user, session) in [("member1", "s-member"), ("admin1", "s-admin")] {
            st.create_session(
                session,
                None,
                &format!("vm-{session}"),
                "ws",
                "storage-default",
            )
            .expect("session");
            st.connection
                .lock()
                .expect("lock")
                .execute(
                    "UPDATE sessions SET owner_user_id = ?1 WHERE session_id = ?2",
                    [user, session],
                )
                .expect("owner");
        }
        let d = connector_disabled_denial(&st, "s-member", URN, "microsoft-365");
        assert!(d.decision_ref.starts_with("dnl_"), "{d:?}");
        assert!(!d.fix.as_ref().expect("fix").caller_can_apply);
        let d = connector_disabled_denial(&st, "s-admin", URN, "microsoft-365");
        assert!(d.decision_ref.is_empty());
        assert!(d.fix.as_ref().expect("fix").caller_can_apply);
    }

    #[test]
    fn a_denial_for_the_leased_run_is_on_the_event_only_when_the_session_is_bound_to_it() {
        let st = store();
        st.create_session("s1", None, "vm", "ws", "storage-default")
            .expect("session");
        let d = plain_denial("run", Text::BuiltIn);
        // No run is bound to the session: nothing is attached, whatever the lease says.
        assert!(event_denial(&st, "s1", "run-x", &d).is_none());
        assert!(event_denial(&st, "s1", "", &d).is_none());
    }

    #[test]
    fn fix_recipients_for_each_who() {
        let st = store();
        st.create_user_with_role("admin1", "admin1", UserRole::OrgAdmin)
            .expect("admin");
        st.create_user_with_role("admin2", "admin2", UserRole::OrgAdmin)
            .expect("admin");
        st.create_user_with_role("member1", "member1", UserRole::Member)
            .expect("member");
        st.deactivate_user("admin2").expect("deactivate");
        let admins = fix_recipients(&st, FixWho::OrgAdmin, URN).expect("admins");
        assert!(admins.contains(&"admin1".to_string()), "{admins:?}");
        assert!(admins.contains(&crate::users::OPERATOR_USER_ID.to_string()));
        assert!(!admins.contains(&"admin2".to_string()), "deactivated");
        assert!(!admins.contains(&"member1".to_string()));
        assert!(fix_recipients(&st, FixWho::Self_, URN)
            .expect("self")
            .is_empty());
        // An agent the catalog does not have has no owner to tell.
        assert!(fix_recipients(&st, FixWho::AgentOwner, URN).is_err());
    }

    fn session_of(st: &SqliteWorkflowStore, user: &str, session: &str) {
        st.create_session(
            session,
            None,
            &format!("vm-{session}"),
            "ws",
            "storage-default",
        )
        .expect("session");
        st.connection
            .lock()
            .expect("lock")
            .execute(
                "UPDATE sessions SET owner_user_id = ?1 WHERE session_id = ?2",
                [user, session],
            )
            .expect("owner");
    }

    fn rows(st: &SqliteWorkflowStore) -> i64 {
        st.connection
            .lock()
            .expect("lock")
            .query_row("SELECT COUNT(*) FROM denials", [], |r| r.get(0))
            .expect("count")
    }

    #[test]
    fn a_missing_secret_is_the_callers_to_set_unless_it_is_org_scope() {
        let st = store();
        st.create_user_with_role("member1", "member1", UserRole::Member)
            .expect("member");
        st.create_user_with_role("admin1", "admin1", UserRole::OrgAdmin)
            .expect("admin");
        let items = vec![
            ("mine".to_string(), "user".to_string()),
            ("either".to_string(), "either".to_string()),
            ("shared".to_string(), "org".to_string()),
        ];
        let member = secret_missing_denials(&st, "member1", URN, &items);
        let who: Vec<i32> = member
            .iter()
            .map(|d| d.fix.as_ref().expect("fix").who)
            .collect();
        assert_eq!(
            who,
            vec![
                FixWho::Self_ as i32,
                FixWho::Self_ as i32,
                FixWho::OrgAdmin as i32
            ]
        );
        assert!(member[0].fix.as_ref().unwrap().caller_can_apply);
        assert!(member[0].decision_ref.is_empty());
        let org = member[2].fix.as_ref().unwrap();
        assert!(!org.caller_can_apply);
        assert_eq!(org.target, "shared");
        assert!(org.route.is_none(), "no write route is named for a secret");
        assert!(member[2].decision_ref.starts_with("dnl_"));
        assert_eq!(
            rows(&st),
            1,
            "a row only for the fix the caller cannot apply"
        );
        let (run, session, change, who): (Option<String>, String, String, String) = st
            .connection
            .lock()
            .expect("lock")
            .query_row(
                "SELECT run_id, session_id, change, who FROM denials WHERE id = ?1",
                [&member[2].decision_ref],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .expect("row");
        assert_eq!(run, None);
        assert_eq!(session, "");
        assert_eq!((change.as_str(), who.as_str()), ("SET_SECRET", "ORG_ADMIN"));

        // An org admin can set the org secret: no row, no reference.
        let admin = secret_missing_denials(&st, "admin1", URN, &items);
        assert!(admin[2].fix.as_ref().unwrap().caller_can_apply);
        assert!(admin[2].decision_ref.is_empty());
        assert_eq!(rows(&st), 1);
    }

    #[test]
    fn egress_kinds_are_read_from_the_rule_on_the_policy_id() {
        let k = egress_kind;
        assert_eq!(
            k("rule=agent rule internet:*; active-policy:terminate"),
            EgressKind::AgentDefault
        );
        assert_eq!(
            k("rule=agent rule *.pastebin.com; active-policy:terminate"),
            EgressKind::AgentRule
        );
        assert_eq!(
            k("rule=platform ip floor; active-policy:terminate"),
            EgressKind::Fixed
        );
        assert_eq!(
            k("rule=port allowlist; active-policy:terminate"),
            EgressKind::Fixed
        );
        assert_eq!(
            k("rule=secret destination *.x.com; active-policy:terminate"),
            EgressKind::Fixed
        );
        assert_eq!(
            k("rule=egress policy unavailable; active-policy:terminate"),
            EgressKind::Unavailable
        );
        assert_eq!(
            k("rule=name resolution; active-policy:terminate"),
            EgressKind::NameResolution
        );
        assert_eq!(k("active-policy:terminate"), EgressKind::Fixed);
    }

    #[test]
    fn egress_refusals_without_a_fix_have_no_fix_and_name_resolution_has_no_denial() {
        let st = store();
        let floor = egress_denial(
            &st,
            "s1",
            None,
            "169.254.169.254",
            "rule=platform ip floor; active-policy:terminate",
        )
        .expect("denial");
        assert_eq!(floor.reason, DenialReason::EgressRule as i32);
        assert!(floor.fix.is_none() && floor.decision_ref.is_empty());
        assert_eq!(floor.subject, "169.254.169.254");
        assert!(egress_denial(&st, "s1", None, "x.test", "rule=name resolution; p").is_none());
        let broken = egress_denial(
            &st,
            "s1",
            None,
            "x.test",
            "rule=egress policy error; active-policy:terminate",
        )
        .expect("denial");
        assert!(broken.fix.is_none() && !broken.text.is_empty());
    }

    #[test]
    fn a_refused_host_gets_the_owner_fix_and_a_row_for_a_caller_who_is_not_the_owner() {
        let st = store();
        st.create_user_with_role("owner1", "owner1", UserRole::Member)
            .expect("owner");
        st.create_user_with_role("other1", "other1", UserRole::Member)
            .expect("other");
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
        session_of(&st, "other1", "s-other");
        session_of(&st, "owner1", "s-owner");
        let label = "rule=agent rule internet:*; active-policy:terminate";

        // A caller who does not own the agent: AGENT_OWNER fix, a row, a reference. The host is
        // the guest's: it is lower-cased and loses its trailing dot.
        let d = egress_denial(&st, "s-other", Some(URN), "Example.COM.", label).expect("denial");
        let fix = d.fix.as_ref().expect("fix");
        assert_eq!(d.subject, "example.com");
        assert_eq!(fix.change, FixChange::AllowHost as i32);
        assert_eq!(fix.who, FixWho::AgentOwner as i32);
        assert_eq!(fix.target, format!("{URN}#example.com"));
        assert_eq!(
            fix.route.as_ref().map(|r| r.path.as_str()),
            Some(format!("/v1/agents/{URN}/permissions").as_str())
        );
        assert!(!fix.caller_can_apply);
        assert!(d.decision_ref.starts_with("dnl_"));
        assert_eq!(rows(&st), 1);

        // The owner can apply it: no row.
        let d = egress_denial(&st, "s-owner", Some(URN), "example.com", label).expect("denial");
        assert!(d.fix.as_ref().expect("fix").caller_can_apply);
        assert!(d.decision_ref.is_empty());
        assert_eq!(rows(&st), 1);

        // The cap stops rows, not the explanation.
        let d = egress_denial_without_row(&st, "s-other", Some(URN), "b.example.com", label)
            .expect("denial");
        assert!(d.fix.is_some() && d.decision_ref.is_empty());
        assert_eq!(rows(&st), 1);

        // A host with characters a fix target cannot carry gets no fix.
        let d = egress_denial(&st, "s-other", Some(URN), "a#b.example", label).expect("denial");
        assert!(d.fix.is_none());

        // An explicit deny rule is the owner's choice: text only.
        let d = egress_denial(
            &st,
            "s-other",
            Some(URN),
            "bad.example",
            "rule=agent rule bad.example; active-policy:terminate",
        )
        .expect("denial");
        assert!(d.fix.is_none() && d.text.contains("has blocked"));
        assert_eq!(rows(&st), 1);
    }

    #[test]
    fn a_host_is_capped_and_cleaned_before_it_is_a_subject() {
        let st = store();
        let long = format!("{}.example", "a".repeat(400));
        let d = egress_denial(&st, "s1", None, &long, "rule=port allowlist; p").expect("denial");
        assert!(d.subject.len() <= 253);
        let d = egress_denial(&st, "s1", None, "evil\n.example", "rule=port allowlist; p")
            .expect("denial");
        assert!(!d.subject.contains('\n'));
    }

    #[test]
    fn version_stale_names_the_bound_version_without_a_change() {
        let d = version_stale_denial("1.0.0");
        let fix = d.fix.expect("fix");
        assert_eq!(fix.change, FixChange::Unspecified as i32);
        assert_eq!(fix.target, "1.0.0");
        assert!(fix.caller_can_apply && fix.who == FixWho::Self_ as i32);
        assert_eq!(d.text, "Run again on version 1.0.0, or omit the version.");
    }
}
