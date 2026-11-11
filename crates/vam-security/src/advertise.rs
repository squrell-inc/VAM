//! Advertise-time tool decisions. Same two-pass Cedar check the host uses
//! before a tool is shown to the model: Allow, Deny, or HITL (deny until
//! `hitl_approved`, then allow). Does not mint tokens or start a run.
//!
//! [`DecisionOutcome`] already carries `matching_policies` on Allow and Deny.
//! HITL is the second pass, so the policies surfaced for it are the Allow
//! policies from that pass. Suspend is synthesized later by the host and has
//! no policy list of its own.

use crate::pdp::{
    EvaluationContext, EvaluationRequest, PolicyDecisionPoint, ScopeRule, ToolEntity,
};
use crate::{CedarPolicyEngine, PolicyMode, SecurityError};

/// Builtins-only fallback. Unknown or missing capability policies use this
/// plus the HITL overlay. Skill tools are not permitted here.
pub const BUILTIN_EXECUTE_TOOL_POLICY: &str = r#"
permit (
    principal,
    action == Action::"ExecuteTool",
    resource
)
when {
    [
        Tool::"read_file",
        Tool::"write_file",
        Tool::"edit_file",
        Tool::"delete_file",
        Tool::"glob",
        Tool::"grep",
        Tool::"run",
        Tool::"todo_write",
        Tool::"load_skill"
    ].contains(resource)
};

permit (
    principal,
    action == Action::"ReadFile",
    resource
)
when {
    context has path &&
    (context.path like "/mnt/drive9/workspace/*" ||
     context.path like "/mnt/drive9/skills/*")
};

permit (
    principal,
    action == Action::"WriteFile",
    resource
)
when {
    context has path &&
    context.path like "/mnt/drive9/workspace/*" &&
    !(context.path like "/mnt/drive9/skills/*") &&
    context has file_size_bytes &&
    context.file_size_bytes <= 10485760
};

forbid (
    principal,
    action == Action::"WriteFile",
    resource
)
when {
    context has path && context.path like "/mnt/drive9/skills/*"
};
"#;

/// Guest builtins named by [`BUILTIN_EXECUTE_TOOL_POLICY`].
pub const BUILTIN_TOOL_NAMES: &[&str] = &[
    "read_file",
    "write_file",
    "edit_file",
    "delete_file",
    "glob",
    "grep",
    "run",
    "todo_write",
    "load_skill",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolEffect {
    Allow,
    Deny,
    /// Denied until a human approves. The second Cedar pass would Allow.
    Hitl,
}

impl ToolEffect {
    pub fn as_str(self) -> &'static str {
        match self {
            ToolEffect::Allow => "allow",
            ToolEffect::Deny => "deny",
            ToolEffect::Hitl => "hitl",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamedTool {
    pub name: String,
    pub side_effect: Option<i64>,
}

/// Which scope produced [`ToolDecision::deciding_rule`].
///
/// [`DecidingScope::None`] is the legacy `classify_tools` path, which has no
/// per-scope attributes. [`DecidingScope::Platform`] is deny-by-default when
/// every scope is `none`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecidingScope {
    Platform,
    Org,
    Source,
    Agent,
    None,
}

impl DecidingScope {
    pub fn as_str(self) -> &'static str {
        match self {
            DecidingScope::Platform => "platform",
            DecidingScope::Org => "org",
            DecidingScope::Source => "source",
            DecidingScope::Agent => "agent",
            DecidingScope::None => "none",
        }
    }
}

/// One scope's resolved decision, the rule text, and where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeLabel {
    pub decision: ScopeRule,
    pub rule: String,
    pub origin: String,
}

impl ScopeLabel {
    pub fn new(decision: ScopeRule, rule: impl Into<String>, origin: impl Into<String>) -> Self {
        Self {
            decision,
            rule: rule.into(),
            origin: origin.into(),
        }
    }

    pub fn none() -> Self {
        Self {
            decision: ScopeRule::None,
            rule: String::new(),
            origin: String::new(),
        }
    }
}

/// Explain row: scope, decision, rule text, origin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeExplanation {
    pub scope: DecidingScope,
    pub decision: ScopeRule,
    pub rule: String,
    pub origin: String,
}

/// A tool plus the three scope attributes and their rule labels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopedTool {
    pub name: String,
    pub side_effect: Option<i64>,
    pub target: String,
    pub org: ScopeLabel,
    pub source: ScopeLabel,
    pub agent: ScopeLabel,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolDecision {
    pub name: String,
    pub effect: ToolEffect,
    /// Policies that decided this row. For HITL, the Allow policies from the
    /// `hitl_approved` pass.
    pub matching_policies: Vec<String>,
    pub diagnostic: String,
    /// Target attribute (`skill:jira/create_issue`). Empty on the legacy path.
    pub target: String,
    /// Scope that decided the row. [`DecidingScope::None`] on the legacy path.
    pub deciding_scope: DecidingScope,
    /// Rule text (`approvalRequired`, `skill:jira/*`). On the legacy path,
    /// the matching policy ids joined with `,`.
    pub deciding_rule: String,
    /// Per-scope explain rows. Empty on the legacy path.
    pub scopes: Vec<ScopeExplanation>,
}

/// Append `overlay` after `base`. An empty overlay leaves `base` unchanged.
pub fn compose_policy(base: &str, overlay: &str) -> String {
    let overlay = overlay.trim();
    if overlay.is_empty() {
        base.to_string()
    } else {
        format!("{}\n\n{overlay}", base.trim_end())
    }
}

/// Permit-under-approval plus forbid-unless-approved for each monitored tool.
///
/// Invalid tool names and unsafe URN literals are skipped so they cannot
/// break the Cedar document. Same statements the host appends at session start.
pub fn hitl_execute_overlay(agent_urn: &str, monitored_tools: &[&str]) -> String {
    if !is_cedar_string_safe(agent_urn) {
        eprintln!(
            "[orchestrator] WARN skipping HITL overlay: agent URN is not a safe Cedar string literal"
        );
        return String::new();
    }

    let mut out = String::new();
    for tool in monitored_tools {
        if !is_catalog_tool_name(tool) || !is_cedar_string_safe(tool) {
            eprintln!(
                "[orchestrator] WARN skipping HITL overlay for tool {tool:?}: name is not a catalog-safe Cedar identifier"
            );
            continue;
        }
        out.push_str(&format!(
            r#"
permit (
    principal == Agent::"{agent_urn}",
    action == Action::"ExecuteTool",
    resource == Tool::"{tool}"
)
when {{
    context has hitl_approved && context.hitl_approved == true
}};

forbid (
    principal == Agent::"{agent_urn}",
    action == Action::"ExecuteTool",
    resource == Tool::"{tool}"
)
unless {{
    context has hitl_approved && context.hitl_approved == true
}};
"#
        ));
    }
    out
}

/// Evaluate each tool in Enforce mode. Nothing is executed.
///
/// Legacy path: Cedar text in, no tool attributes. `deciding_scope` is
/// [`DecidingScope::None`] and `deciding_rule` is the matching policy ids.
pub fn classify_tools(
    policy_src: &str,
    agent_urn: &str,
    tools: &[NamedTool],
) -> Result<Vec<ToolDecision>, SecurityError> {
    let engine = CedarPolicyEngine::from_default_schema(policy_src, PolicyMode::Enforce, 1)?;
    let mut decisions = Vec::with_capacity(tools.len());
    for tool in tools {
        decisions.push(classify_one(&engine, agent_urn, tool));
    }
    Ok(decisions)
}

/// Like [`classify_tools`], with per-tool attributes and rule labels.
///
/// Across scopes the strictest decision wins (`deny` > `ask` > `allow`).
/// The deciding scope is that strictest one. A tie goes to the outermost
/// (`org` > `source` > `agent`). The effect still comes from Cedar.
pub fn classify_scoped_tools(
    policy_src: &str,
    agent_urn: &str,
    tools: &[ScopedTool],
) -> Result<Vec<ToolDecision>, SecurityError> {
    let engine = CedarPolicyEngine::from_default_schema(policy_src, PolicyMode::Enforce, 1)?;
    let mut decisions = Vec::with_capacity(tools.len());
    for tool in tools {
        decisions.push(classify_scoped_one(&engine, agent_urn, tool));
    }
    Ok(decisions)
}

fn classify_one(pdp: &dyn PolicyDecisionPoint, agent_urn: &str, tool: &NamedTool) -> ToolDecision {
    let pass = two_pass(pdp, agent_urn, &tool.name, tool.side_effect, Vec::new());
    legacy_decision(&tool.name, pass)
}

fn classify_scoped_one(
    pdp: &dyn PolicyDecisionPoint,
    agent_urn: &str,
    tool: &ScopedTool,
) -> ToolDecision {
    let entity = ToolEntity::new(
        tool.name.clone(),
        tool.agent.decision,
        tool.source.decision,
        tool.org.decision,
        tool.target.clone(),
    );
    let pass = two_pass(pdp, agent_urn, &tool.name, tool.side_effect, vec![entity]);
    let mut decision = legacy_decision(&tool.name, pass);
    decision.target = tool.target.clone();
    decision.scopes = vec![
        explanation(DecidingScope::Agent, &tool.agent),
        explanation(DecidingScope::Source, &tool.source),
        explanation(DecidingScope::Org, &tool.org),
    ];
    let (scope, rule) = deciding_scope(tool);
    if scope == DecidingScope::Platform && decision.effect != ToolEffect::Deny {
        decision.deciding_scope = DecidingScope::None;
        decision.deciding_rule = decision.matching_policies.join(",");
    } else {
        decision.deciding_scope = scope;
        decision.deciding_rule = rule;
    }
    decision
}

struct Pass {
    effect: ToolEffect,
    matching_policies: Vec<String>,
    diagnostic: String,
}

fn two_pass(
    pdp: &dyn PolicyDecisionPoint,
    agent_urn: &str,
    name: &str,
    side_effect: Option<i64>,
    entities: Vec<ToolEntity>,
) -> Pass {
    let principal = format!("Agent::\"{agent_urn}\"");
    let action = "Action::\"ExecuteTool\"".to_string();
    let resource = format!("Tool::\"{name}\"");
    let mut context = EvaluationContext {
        side_effect,
        ..Default::default()
    };
    let first = pdp.evaluate(&EvaluationRequest {
        principal: principal.clone(),
        action: action.clone(),
        resource: resource.clone(),
        context: context.clone(),
        entities: entities.clone(),
    });
    if first.is_allowed() {
        return Pass {
            effect: ToolEffect::Allow,
            matching_policies: first.matching_policies().to_vec(),
            diagnostic: String::new(),
        };
    }

    context.hitl_approved = Some(true);
    let second = pdp.evaluate(&EvaluationRequest {
        principal,
        action,
        resource,
        context,
        entities,
    });
    if second.is_allowed() {
        return Pass {
            effect: ToolEffect::Hitl,
            matching_policies: second.matching_policies().to_vec(),
            diagnostic: "requires human approval before execution".to_string(),
        };
    }

    Pass {
        effect: ToolEffect::Deny,
        matching_policies: first.matching_policies().to_vec(),
        diagnostic: first.diagnostic_message().to_string(),
    }
}

fn legacy_decision(name: &str, pass: Pass) -> ToolDecision {
    let deciding_rule = pass.matching_policies.join(",");
    ToolDecision {
        name: name.to_string(),
        effect: pass.effect,
        matching_policies: pass.matching_policies,
        diagnostic: pass.diagnostic,
        target: String::new(),
        deciding_scope: DecidingScope::None,
        deciding_rule,
        scopes: Vec::new(),
    }
}

fn explanation(scope: DecidingScope, label: &ScopeLabel) -> ScopeExplanation {
    ScopeExplanation {
        scope,
        decision: label.decision,
        rule: label.rule.clone(),
        origin: label.origin.clone(),
    }
}

/// Outermost first so an equal strictness keeps org over source over agent.
fn deciding_scope(tool: &ScopedTool) -> (DecidingScope, String) {
    let ranked = [
        (DecidingScope::Org, &tool.org),
        (DecidingScope::Source, &tool.source),
        (DecidingScope::Agent, &tool.agent),
    ];
    let mut best: Option<(DecidingScope, String, u8)> = None;
    for (scope, label) in ranked {
        let rank = strictness(label.decision);
        if rank == 0 {
            continue;
        }
        let replace = match &best {
            Some((_, _, best_rank)) => rank > *best_rank,
            None => true,
        };
        if replace {
            best = Some((scope, label.rule.clone(), rank));
        }
    }
    match best {
        Some((scope, rule, _)) => (scope, rule),
        None => (DecidingScope::Platform, String::new()),
    }
}

fn strictness(rule: ScopeRule) -> u8 {
    match rule {
        ScopeRule::Deny => 3,
        ScopeRule::Ask => 2,
        ScopeRule::Allow => 1,
        ScopeRule::None => 0,
    }
}

fn is_catalog_tool_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= vam_spec::validation::TOOL_NAME_MAX_LEN
        && name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

fn is_cedar_string_safe(value: &str) -> bool {
    !value.is_empty()
        && !value.contains('"')
        && !value.contains('\\')
        && !value.contains('\n')
        && !value.contains('\r')
}

#[cfg(test)]
mod tests {
    use super::*;

    const AGENT: &str = "urn:vam:agent:examples.local/vam/demo";

    fn tool(name: &str) -> NamedTool {
        NamedTool {
            name: name.to_string(),
            side_effect: None,
        }
    }

    #[test]
    fn allow_hitl_and_deny_surface_matching_policies() {
        let policy = r#"
permit (
    principal == Agent::"urn:vam:agent:examples.local/vam/demo",
    action == Action::"ExecuteTool",
    resource == Tool::"read_file"
);
permit (
    principal == Agent::"urn:vam:agent:examples.local/vam/demo",
    action == Action::"ExecuteTool",
    resource == Tool::"write_file"
)
when { context has hitl_approved && context.hitl_approved == true };
"#;
        let decisions = classify_tools(
            policy,
            AGENT,
            &[tool("read_file"), tool("write_file"), tool("run")],
        )
        .expect("policy compiles");
        let read = &decisions[0];
        assert_eq!(read.effect, ToolEffect::Allow);
        assert!(!read.matching_policies.is_empty());
        assert_eq!(read.deciding_scope, DecidingScope::None);
        assert_eq!(read.deciding_rule, read.matching_policies.join(","));
        assert!(read.target.is_empty());
        assert!(read.scopes.is_empty());
        let write = &decisions[1];
        assert_eq!(write.effect, ToolEffect::Hitl);
        assert!(!write.matching_policies.is_empty());
        assert!(write.diagnostic.contains("human approval"));
        let run = &decisions[2];
        assert_eq!(run.effect, ToolEffect::Deny);
        assert!(run.diagnostic.contains("deny"));
    }

    #[test]
    fn overlay_turns_an_unconditional_permit_into_hitl() {
        let overlay = hitl_execute_overlay(AGENT, &["run"]);
        let policy = compose_policy(BUILTIN_EXECUTE_TOOL_POLICY, &overlay);
        let decisions =
            classify_tools(&policy, AGENT, &[tool("run"), tool("read_file")]).expect("compiles");
        assert_eq!(decisions[0].effect, ToolEffect::Hitl);
        assert!(!decisions[0].matching_policies.is_empty());
        assert_eq!(decisions[1].effect, ToolEffect::Allow);
        assert!(!decisions[1].matching_policies.is_empty());
    }

    #[test]
    fn overlay_skips_names_that_break_cedar() {
        let overlay = hitl_execute_overlay(
            AGENT,
            &[
                r#"evil";permit (principal, action, resource);"#,
                "Not Valid",
                "ok_tool",
            ],
        );
        assert!(overlay.contains("Tool::\"ok_tool\""));
        assert!(!overlay.contains("evil"));
        assert!(!overlay.contains("Not Valid"));
    }

    #[test]
    fn source_ask_is_hitl_decided_by_approval_required() {
        use crate::pdp::ScopeRule;
        use crate::platform_policy::PLATFORM_TOOL_POLICY;

        let tool = ScopedTool {
            name: "jira_create_issue".into(),
            side_effect: None,
            target: "skill:jira/create_issue".into(),
            agent: ScopeLabel::new(ScopeRule::Allow, "skill:jira/*", "agent v1.3.0"),
            source: ScopeLabel::new(ScopeRule::Ask, "approvalRequired", "skill jira v2.1.0"),
            org: ScopeLabel::none(),
        };
        let decisions = classify_scoped_tools(PLATFORM_TOOL_POLICY, AGENT, &[tool])
            .expect("platform policy compiles");
        let row = &decisions[0];
        assert_eq!(row.effect, ToolEffect::Hitl);
        assert_eq!(row.deciding_scope, DecidingScope::Source);
        assert_eq!(row.deciding_rule, "approvalRequired");
        assert_eq!(row.target, "skill:jira/create_issue");
        let source = row
            .scopes
            .iter()
            .find(|scope| scope.scope == DecidingScope::Source)
            .expect("source row");
        assert_eq!(source.decision, ScopeRule::Ask);
        assert_eq!(source.rule, "approvalRequired");
        assert_eq!(source.origin, "skill jira v2.1.0");
    }

    #[test]
    fn strictest_scope_wins_and_ties_go_to_the_outermost() {
        use crate::pdp::ScopeRule;
        use crate::platform_policy::PLATFORM_TOOL_POLICY;

        let deny_beats_ask = ScopedTool {
            name: "jira_create_issue".into(),
            side_effect: None,
            target: "skill:jira/create_issue".into(),
            agent: ScopeLabel::new(ScopeRule::Allow, "skill:jira/*", "agent"),
            source: ScopeLabel::new(ScopeRule::Ask, "approvalRequired", "skill"),
            org: ScopeLabel::new(ScopeRule::Deny, "org-guard", "org"),
        };
        let tied_ask = ScopedTool {
            name: "other".into(),
            side_effect: None,
            target: "skill:other".into(),
            agent: ScopeLabel::new(ScopeRule::Allow, "skill:other", "agent"),
            source: ScopeLabel::new(ScopeRule::Ask, "approvalRequired", "skill"),
            org: ScopeLabel::new(ScopeRule::Ask, "org-ask", "org"),
        };
        let tied_deny = ScopedTool {
            name: "blocked".into(),
            side_effect: None,
            target: "skill:blocked".into(),
            agent: ScopeLabel::new(ScopeRule::Deny, "agent-deny", "agent"),
            source: ScopeLabel::new(ScopeRule::Deny, "source-deny", "skill"),
            org: ScopeLabel::new(ScopeRule::Deny, "org-deny", "org"),
        };
        let decisions = classify_scoped_tools(
            PLATFORM_TOOL_POLICY,
            AGENT,
            &[deny_beats_ask, tied_ask, tied_deny],
        )
        .expect("compiles");
        assert_eq!(decisions[0].effect, ToolEffect::Deny);
        assert_eq!(decisions[0].deciding_scope, DecidingScope::Org);
        assert_eq!(decisions[0].deciding_rule, "org-guard");
        assert_eq!(decisions[1].effect, ToolEffect::Hitl);
        assert_eq!(decisions[1].deciding_scope, DecidingScope::Org);
        assert_eq!(decisions[1].deciding_rule, "org-ask");
        assert_eq!(decisions[2].effect, ToolEffect::Deny);
        assert_eq!(decisions[2].deciding_scope, DecidingScope::Org);
        assert_eq!(decisions[2].deciding_rule, "org-deny");
    }
}
