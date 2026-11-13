//! One permissions resolver for agent, source, and org scopes.
//!
//! See docs: security/permissions. Within a scope the most
//! specific matching rule wins ([`Target::specificity`]). Across scopes the
//! strictest wins, which the AX-5.2 platform policy evaluates from the
//! attributes this module writes. Source and org rules can only tighten.
//!
//! MCP tools with no stored class are mutable external. `vam-orchestrator`
//! seeds that as `MCP_SEEDED_SIDE_EFFECT` (`SideEffectClass::MutableExternal`,
//! value 3). Under `bySideEffect` that class asks, and so does an unspecified
//! class.

use std::cmp::Ordering;

use thiserror::Error;
use vam_spec::v1::{
    AgentPermissions, HostDefault, PermissionDecision, PermissionRule, SideEffectClass, ToolDefault,
};
use vam_spec::{HostPattern, Target, ToolSelector};

use crate::advertise::{
    classify_scoped_tools, DecidingScope, ScopeExplanation, ScopeLabel, ScopedTool, ToolDecision,
    ToolEffect,
};
use crate::pdp::{ScopeRule, ToolEntity};
use crate::platform_policy::{compile_egress_rules, EgressRule, PLATFORM_TOOL_POLICY};
use crate::SecurityError;

/// Principal passed to the platform policy. The policy does not match on it.
const EXPLAIN_AGENT: &str = "urn:vam:agent:local.vam.dev/explain";

/// A source-scope ask. Source rules tighten only, so `allow` cannot be built.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceRule {
    rule: String,
    origin: String,
}

impl SourceRule {
    /// `approvalRequired` from a skill, or an MCP connection ask rule.
    ///
    /// `origin` names the source in explain rows (`skill jira v2.1.0`).
    pub fn ask(rule: impl Into<String>, origin: impl Into<String>) -> Self {
        Self {
            rule: rule.into(),
            origin: origin.into(),
        }
    }

    pub fn rule(&self) -> &str {
        &self.rule
    }

    pub fn origin(&self) -> &str {
        &self.origin
    }
}

/// An org-scope rule. Org rules tighten only (`ask` or `deny`). Phase 5
/// passes [`None`](Option::None); E3 fills the slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrgRule {
    decision: ScopeRule,
    rule: String,
    origin: String,
}

impl OrgRule {
    pub fn ask(rule: impl Into<String>, origin: impl Into<String>) -> Self {
        Self::new(ScopeRule::Ask, rule, origin)
    }

    pub fn deny(rule: impl Into<String>, origin: impl Into<String>) -> Self {
        Self::new(ScopeRule::Deny, rule, origin)
    }

    fn new(decision: ScopeRule, rule: impl Into<String>, origin: impl Into<String>) -> Self {
        Self {
            decision,
            rule: rule.into(),
            origin: origin.into(),
        }
    }

    pub fn decision(&self) -> ScopeRule {
        self.decision
    }

    pub fn rule(&self) -> &str {
        &self.rule
    }

    pub fn origin(&self) -> &str {
        &self.origin
    }
}

/// One tool to resolve. `side_effect` [`None`] or
/// [`SideEffectClass::Unspecified`] both mean unspecified.
#[derive(Debug, Clone, Copy)]
pub struct ToolQuery<'a> {
    /// Guest tool name. This is the Cedar `Tool` id and the explain header.
    pub name: &'a str,
    /// Exact tool target (`skill:jira/create_issue`, an MCP tool, or a builtin).
    pub target: &'a Target,
    pub side_effect: Option<SideEffectClass>,
    /// Ask rule already selected for this tool, when the source has one.
    pub source: Option<&'a SourceRule>,
    /// Org rule already selected for this tool. Phase 5 leaves this empty.
    pub org: Option<&'a OrgRule>,
}

/// "Always allow / Always deny" is offered when the agent scope decides.
///
/// A source or org `ask` or `deny` blocks it. [`AlwaysOffer::Blocked`] names
/// that rule (`skill jira marks create_issue approvalRequired`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AlwaysOffer {
    Offered,
    Blocked { reason: String },
}

impl AlwaysOffer {
    pub fn offered(&self) -> bool {
        matches!(self, AlwaysOffer::Offered)
    }

    pub fn reason(&self) -> Option<&str> {
        match self {
            AlwaysOffer::Offered => None,
            AlwaysOffer::Blocked { reason } => Some(reason),
        }
    }
}

/// Host rules plus the Cedar `EgressRequest` policy from the AX-5.2 compiler.
///
/// `rules` is `(pattern, "allow" | "deny")` in author order, without
/// `internet:*`. That entry is [`InternetResolution::default_decision`] and
/// is applied last, after the most specific matching host rule.
///
/// [`explain_host`] is that specificity decision. The compiled policy uses
/// Cedar forbid-overrides, so a broader `deny` still forbids a host that a
/// more specific `allow` names. A more specific `deny` under an `allow`
/// (the `internet:*` default, or a shorter wildcard) agrees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InternetResolution {
    pub default_decision: &'static str,
    pub rules: Vec<(String, String)>,
    pub cedar: String,
}

/// Decision for one host. The most specific matching host rule wins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostExplanation {
    pub host: String,
    pub decision: ScopeRule,
    /// Canonical pattern, or `internet:*` when the default decides.
    pub deciding_rule: String,
    pub origin: String,
}

impl HostExplanation {
    pub fn allowed(&self) -> bool {
        self.decision == ScopeRule::Allow
    }
}

#[derive(Debug, Error)]
pub enum PermissionsError {
    #[error("{0}")]
    InvalidTarget(String),
    #[error("tool resolution needs an exact tool target, found '{0}'")]
    NotExactTool(String),
    #[error("tool name is empty")]
    EmptyToolName,
    #[error("{0}")]
    InvalidRule(String),
    #[error(transparent)]
    Policy(#[from] SecurityError),
}

struct StoredRule {
    target: Target,
    decision: PermissionDecision,
}

/// Resolve one tool to `Tool` attributes and a label per scope.
///
/// The agent label is the most specific matching rule, or `tools` default
/// when nothing matches. `bySideEffect` allows read-only and mutable local
/// tools and asks for mutable external and unspecified tools. `agent_origin`
/// is the explain origin for agent rows (`agent v1.3.0`).
pub fn resolve_tool(
    permissions: &AgentPermissions,
    agent_origin: &str,
    query: ToolQuery<'_>,
) -> Result<ScopedTool, PermissionsError> {
    if query.name.is_empty() {
        return Err(PermissionsError::EmptyToolName);
    }
    exact_tool(query.target)?;
    let rules = tool_rules(permissions)?;
    let (decision, rule) =
        match most_specific(&rules, |rule| tool_rule_matches(&rule.target, query.target)) {
            Some(rule) => (
                scope_from_decision(rule.decision, &rule.target.to_string())?,
                rule.target.to_string(),
            ),
            None => default_tool_label(permissions.effective_defaults().tools, query.side_effect),
        };
    Ok(ScopedTool {
        name: query.name.to_string(),
        side_effect: query.side_effect.map(|class| i64::from(class as i32)),
        target: query.target.to_string(),
        agent: ScopeLabel::new(decision, rule, agent_origin),
        source: source_label(query.source),
        org: org_label(query.org),
    })
}

/// `Tool` entity for a resolved tool. Every attribute is set; an empty scope
/// is [`ScopeRule::None`], never a missing field.
pub fn tool_entity(tool: &ScopedTool) -> ToolEntity {
    ToolEntity::new(
        tool.name.clone(),
        tool.agent.decision,
        tool.source.decision,
        tool.org.decision,
        tool.target.clone(),
    )
}

/// Per-scope rows and the deciding scope, in the [`ToolDecision`] shape
/// `classify_scoped_tools` returns. The effect comes from the platform policy.
pub fn explain_tool(
    permissions: &AgentPermissions,
    agent_origin: &str,
    query: ToolQuery<'_>,
) -> Result<ToolDecision, PermissionsError> {
    let resolved = resolve_tool(permissions, agent_origin, query)?;
    let mut decisions = classify_scoped_tools(PLATFORM_TOOL_POLICY, EXPLAIN_AGENT, &[resolved])?;
    decisions.pop().ok_or_else(|| {
        PermissionsError::InvalidRule("platform policy returned no tool decision".to_string())
    })
}

/// Whether "Always allow / Always deny" can take effect for this tool.
pub fn always_offered(tool: &ScopedTool) -> AlwaysOffer {
    let token = tool_token(&tool.target).unwrap_or_else(|| tool.name.clone());
    let mut blocker: Option<&ScopeLabel> = None;
    for label in [&tool.org, &tool.source] {
        if tighten_rank(label.decision) == 0 {
            continue;
        }
        let replace = match blocker {
            Some(best) => tighten_rank(label.decision) > tighten_rank(best.decision),
            None => true,
        };
        if replace {
            blocker = Some(label);
        }
    }
    match blocker {
        None => AlwaysOffer::Offered,
        Some(label) => AlwaysOffer::Blocked {
            reason: tightening_reason(label, &token),
        },
    }
}

/// Host rules and the compiled `EgressRequest` policy.
pub fn resolve_internet(
    permissions: &AgentPermissions,
) -> Result<InternetResolution, PermissionsError> {
    let mut default_decision = match permissions.effective_defaults().internet {
        HostDefault::Deny => "deny",
        HostDefault::Allow | HostDefault::Unspecified => "allow",
    };
    let mut resolved: Vec<(&Target, String, &'static str)> = Vec::new();
    let stored = host_rules(permissions)?;
    for rule in &stored {
        let word = host_decision_word(rule.decision, &rule.target.to_string())?;
        if matches!(rule.target, Target::InternetAll) {
            default_decision = word;
            continue;
        }
        let pattern = match &rule.target {
            Target::Host(pattern) => host_pattern(pattern),
            other => other.to_string(),
        };
        resolved.push((&rule.target, pattern, word));
    }
    // Cedar's `forbid` beats every `permit`, so each rule excludes the more
    // specific rules it overlaps. That keeps the compiled policy equal to
    // `explain_host`: the most specific matching rule decides.
    let egress: Vec<EgressRule<'_>> = resolved
        .iter()
        .map(|(target, pattern, word)| EgressRule {
            pattern,
            decision: word,
            except: resolved
                .iter()
                .filter(|(other, _, _)| {
                    specificity_cmp(other, target) == Ordering::Greater
                        && overlaps_as_subset(target, other)
                })
                .map(|(_, other_pattern, _)| other_pattern.as_str())
                .collect(),
        })
        .collect();
    let cedar = compile_egress_rules(default_decision, &egress)?;
    let rules = resolved
        .into_iter()
        .map(|(_, pattern, word)| (pattern, word.to_string()))
        .collect();
    Ok(InternetResolution {
        default_decision,
        rules,
        cedar,
    })
}

/// True when every host `inner` matches is also matched by `outer`. Host
/// patterns are an exact name or `*.suffix`, so overlap is always containment.
fn overlaps_as_subset(outer: &Target, inner: &Target) -> bool {
    match inner {
        Target::Host(HostPattern::Exact(host)) => outer.matches_host(host),
        Target::Host(HostPattern::Wildcard { suffix }) => {
            outer.matches_host(&format!("x.{suffix}"))
        }
        _ => false,
    }
}

/// Decision and deciding rule for `host`.
///
/// The most specific matching host rule wins. `internet:*` (the default, or
/// an explicit `internet:*` rule) is last. An empty host matches nothing and
/// is denied.
pub fn explain_host(
    permissions: &AgentPermissions,
    host: &str,
    agent_origin: &str,
) -> Result<HostExplanation, PermissionsError> {
    let rules = host_rules(permissions)?;
    let matched = most_specific(&rules, |rule| rule.target.matches_host(host));
    let (decision, deciding_rule) = match matched {
        Some(rule) => {
            let word = host_decision_word(rule.decision, &rule.target.to_string())?;
            (scope_from_word(word), rule.target.to_string())
        }
        None if !host.is_empty() => {
            let word = match permissions.effective_defaults().internet {
                HostDefault::Deny => "deny",
                HostDefault::Allow | HostDefault::Unspecified => "allow",
            };
            (scope_from_word(word), "internet:*".to_string())
        }
        None => (ScopeRule::Deny, String::new()),
    };
    let origin = if deciding_rule.is_empty() {
        String::new()
    } else {
        agent_origin.to_string()
    };
    Ok(HostExplanation {
        host: host.to_string(),
        decision,
        deciding_rule,
        origin,
    })
}

/// §IV.9 tool block. `ASK` is the human-approval effect.
pub fn format_tool_explain(decision: &ToolDecision) -> String {
    let mut lines = vec![format!(
        "{:<6}{}  ({})",
        effect_word(decision.effect),
        decision.name,
        decision.target
    )];
    for row in &decision.scopes {
        lines.push(format_scope_row(row, decision.deciding_scope));
    }
    lines.join("\n")
}

/// §IV.9 host line: `DENY  paste.pastebin.com   agent rule *.pastebin.com at agent v1.3.0`.
pub fn format_host_explain(explanation: &HostExplanation) -> String {
    format!(
        "{:<6}{}   agent rule {} at {}",
        host_effect_word(explanation.decision),
        explanation.host,
        explanation.deciding_rule,
        explanation.origin
    )
}

fn format_scope_row(row: &ScopeExplanation, deciding: DecidingScope) -> String {
    let scope = format!("{:<6}", row.scope.as_str());
    if row.decision == ScopeRule::None {
        return format!("  {scope} {}", row.decision.as_str());
    }
    let decision = format!("{:<5}", row.decision.as_str());
    let marker = if row.scope == deciding {
        "     <- decides"
    } else {
        ""
    };
    format!(
        "  {scope} {decision}   {}  ({}){marker}",
        row.rule, row.origin
    )
}

fn effect_word(effect: ToolEffect) -> &'static str {
    match effect {
        ToolEffect::Allow => "ALLOW",
        ToolEffect::Hitl => "ASK",
        ToolEffect::Deny => "DENY",
    }
}

fn host_effect_word(decision: ScopeRule) -> &'static str {
    match decision {
        ScopeRule::Allow => "ALLOW",
        ScopeRule::Deny => "DENY",
        ScopeRule::Ask => "ASK",
        ScopeRule::None => "DENY",
    }
}

fn exact_tool(target: &Target) -> Result<(), PermissionsError> {
    let exact = matches!(
        target,
        Target::Skill {
            tool: ToolSelector::Exact(_),
            ..
        } | Target::Mcp {
            tool: ToolSelector::Exact(_),
            ..
        } | Target::Builtin { .. }
    );
    if exact {
        Ok(())
    } else {
        Err(PermissionsError::NotExactTool(target.to_string()))
    }
}

fn tool_rule_matches(rule: &Target, tool: &Target) -> bool {
    match (rule, tool) {
        (Target::ToolsAll, tool) if tool.is_tool() => true,
        (
            Target::Skill {
                name: rule_name,
                tool: ToolSelector::All,
            },
            Target::Skill { name, .. },
        ) => rule_name == name,
        (
            Target::Mcp {
                server: rule_server,
                tool: ToolSelector::All,
            },
            Target::Mcp { server, .. },
        ) => rule_server == server,
        (rule, tool) => rule == tool,
    }
}

fn most_specific(
    rules: &[StoredRule],
    matches: impl Fn(&StoredRule) -> bool,
) -> Option<&StoredRule> {
    let mut winner: Option<&StoredRule> = None;
    for rule in rules.iter().filter(|rule| matches(rule)) {
        winner = Some(match winner {
            None => rule,
            Some(best) => match specificity_cmp(&rule.target, &best.target) {
                Ordering::Greater => rule,
                Ordering::Less | Ordering::Equal => best,
            },
        });
    }
    winner
}

fn specificity_cmp(left: &Target, right: &Target) -> Ordering {
    left.specificity()
        .partial_cmp(&right.specificity())
        .unwrap_or(Ordering::Equal)
}

fn default_tool_label(default: ToolDefault, class: Option<SideEffectClass>) -> (ScopeRule, String) {
    match default {
        ToolDefault::Allow => (ScopeRule::Allow, "tools:* default allow".to_string()),
        ToolDefault::Ask => (ScopeRule::Ask, "tools:* default ask".to_string()),
        ToolDefault::Deny => (ScopeRule::Deny, "tools:* default deny".to_string()),
        ToolDefault::BySideEffect | ToolDefault::Unspecified => {
            let decision = match class {
                Some(SideEffectClass::ReadOnly | SideEffectClass::MutableLocal) => ScopeRule::Allow,
                Some(SideEffectClass::MutableExternal | SideEffectClass::Unspecified) | None => {
                    ScopeRule::Ask
                }
            };
            (
                decision,
                format!(
                    "tools:* default bySideEffect ({})",
                    side_effect_phrase(class)
                ),
            )
        }
    }
}

fn side_effect_phrase(class: Option<SideEffectClass>) -> &'static str {
    match class {
        Some(SideEffectClass::ReadOnly) => "read-only",
        Some(SideEffectClass::MutableLocal) => "mutable local",
        Some(SideEffectClass::MutableExternal) => "mutable external",
        Some(SideEffectClass::Unspecified) | None => "unspecified",
    }
}

fn source_label(source: Option<&SourceRule>) -> ScopeLabel {
    match source {
        Some(rule) => ScopeLabel::new(ScopeRule::Ask, rule.rule.clone(), rule.origin.clone()),
        None => ScopeLabel::none(),
    }
}

fn org_label(org: Option<&OrgRule>) -> ScopeLabel {
    match org {
        Some(rule) => ScopeLabel::new(rule.decision, rule.rule.clone(), rule.origin.clone()),
        None => ScopeLabel::none(),
    }
}

fn tighten_rank(rule: ScopeRule) -> u8 {
    match rule {
        ScopeRule::Deny => 2,
        ScopeRule::Ask => 1,
        ScopeRule::Allow | ScopeRule::None => 0,
    }
}

fn tightening_reason(label: &ScopeLabel, token: &str) -> String {
    if label.origin.is_empty() {
        format!("{} marks {token}", label.rule)
    } else {
        format!("{} marks {token} {}", label.origin, label.rule)
    }
}

fn tool_token(target: &str) -> Option<String> {
    let parsed = Target::parse(target).ok()?;
    match parsed {
        Target::Skill {
            tool: ToolSelector::Exact(name),
            ..
        }
        | Target::Mcp {
            tool: ToolSelector::Exact(name),
            ..
        } => Some(name),
        Target::Builtin { tool } => Some(tool),
        _ => None,
    }
}

fn tool_rules(permissions: &AgentPermissions) -> Result<Vec<StoredRule>, PermissionsError> {
    match &permissions.tools {
        Some(tools) => parse_rules(&tools.rules, RuleKind::Tool),
        None => Ok(Vec::new()),
    }
}

fn host_rules(permissions: &AgentPermissions) -> Result<Vec<StoredRule>, PermissionsError> {
    match &permissions.internet {
        Some(internet) => parse_rules(&internet.rules, RuleKind::Host),
        None => Ok(Vec::new()),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RuleKind {
    Tool,
    Host,
}

fn parse_rules(
    rules: &[PermissionRule],
    kind: RuleKind,
) -> Result<Vec<StoredRule>, PermissionsError> {
    let mut parsed = Vec::with_capacity(rules.len());
    for rule in rules {
        let target = Target::parse(&rule.target)
            .map_err(|err| PermissionsError::InvalidTarget(err.message))?;
        let fits = match kind {
            RuleKind::Tool => target.is_tool(),
            RuleKind::Host => target.is_host(),
        };
        if !fits {
            let expected = match kind {
                RuleKind::Tool => "a skill, mcp, or builtin target, or tools:*",
                RuleKind::Host => "a host pattern or internet:*",
            };
            return Err(PermissionsError::InvalidRule(format!(
                "permission target '{target}' must be {expected}"
            )));
        }
        let decision = match PermissionDecision::try_from(rule.decision) {
            Ok(decision) => decision,
            Err(_) => PermissionDecision::Unspecified,
        };
        if kind == RuleKind::Host {
            host_decision_word(decision, &target.to_string())?;
        } else {
            scope_from_decision(decision, &target.to_string())?;
        }
        parsed.push(StoredRule { target, decision });
    }
    Ok(parsed)
}

fn scope_from_decision(
    decision: PermissionDecision,
    target: &str,
) -> Result<ScopeRule, PermissionsError> {
    match decision {
        PermissionDecision::Allow => Ok(ScopeRule::Allow),
        PermissionDecision::Ask => Ok(ScopeRule::Ask),
        PermissionDecision::Deny => Ok(ScopeRule::Deny),
        PermissionDecision::Unspecified => Err(PermissionsError::InvalidRule(format!(
            "tool rule '{target}' decision must be allow, ask, or deny"
        ))),
    }
}

fn host_decision_word(
    decision: PermissionDecision,
    target: &str,
) -> Result<&'static str, PermissionsError> {
    match decision {
        PermissionDecision::Allow => Ok("allow"),
        PermissionDecision::Deny => Ok("deny"),
        PermissionDecision::Ask => Err(PermissionsError::InvalidRule(format!(
            "host rules are allow or deny; decision 'ask' is not valid for '{target}'"
        ))),
        PermissionDecision::Unspecified => Err(PermissionsError::InvalidRule(format!(
            "host rule '{target}' decision must be allow or deny"
        ))),
    }
}

fn scope_from_word(word: &str) -> ScopeRule {
    match word {
        "allow" => ScopeRule::Allow,
        "deny" => ScopeRule::Deny,
        "ask" => ScopeRule::Ask,
        _ => ScopeRule::None,
    }
}

fn host_pattern(pattern: &HostPattern) -> String {
    match pattern {
        HostPattern::Exact(host) => host.clone(),
        HostPattern::Wildcard { suffix } => format!("*.{suffix}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform_policy::evaluate_egress;
    use vam_spec::v1::{HostDefault, InternetPermissions, PermissionRule, ToolPermissions};

    const ORIGIN: &str = "agent v1.3.0";

    fn rule(target: &str, decision: PermissionDecision) -> PermissionRule {
        PermissionRule {
            target: target.to_string(),
            decision: decision as i32,
        }
    }

    fn perms(
        tool_default: ToolDefault,
        tool_rules: &[(&str, PermissionDecision)],
        host_default: HostDefault,
        host_rules: &[(&str, PermissionDecision)],
    ) -> AgentPermissions {
        AgentPermissions {
            tools: Some(ToolPermissions {
                default: tool_default as i32,
                rules: tool_rules
                    .iter()
                    .map(|(target, decision)| rule(target, *decision))
                    .collect(),
            }),
            internet: Some(InternetPermissions {
                default: host_default as i32,
                rules: host_rules
                    .iter()
                    .map(|(target, decision)| rule(target, *decision))
                    .collect(),
            }),
            advanced: Vec::new(),
            web: Default::default(),
        }
    }

    fn target(text: &str) -> Target {
        Target::parse(text).expect("target")
    }

    fn query<'a>(
        name: &'a str,
        target: &'a Target,
        side_effect: Option<SideEffectClass>,
    ) -> ToolQuery<'a> {
        ToolQuery {
            name,
            target,
            side_effect,
            source: None,
            org: None,
        }
    }

    #[test]
    fn j1_defaults_allow_read_only_ask_mutable_external_and_open_internet() {
        let permissions = AgentPermissions::default();
        let read_target = target("skill:jira/search_issues");
        let read = explain_tool(
            &permissions,
            ORIGIN,
            query(
                "search_issues",
                &read_target,
                Some(SideEffectClass::ReadOnly),
            ),
        )
        .expect("read-only");
        assert_eq!(read.effect, ToolEffect::Allow);
        assert_eq!(read.deciding_scope, DecidingScope::Agent);
        assert_eq!(
            read.deciding_rule,
            "tools:* default bySideEffect (read-only)"
        );
        assert_eq!(read.agent_row().decision, ScopeRule::Allow);

        let mcp_target = target("mcp:mcp.atlassian.com/create_issue");
        let mcp = explain_tool(
            &permissions,
            ORIGIN,
            query(
                "create_issue",
                &mcp_target,
                Some(SideEffectClass::MutableExternal),
            ),
        )
        .expect("mcp");
        assert_eq!(mcp.effect, ToolEffect::Hitl);
        assert_eq!(
            mcp.deciding_rule,
            "tools:* default bySideEffect (mutable external)"
        );
        assert_eq!(SideEffectClass::MutableExternal as i32, 3);

        let internet = resolve_internet(&permissions).expect("internet");
        assert_eq!(internet.default_decision, "allow");
        assert!(internet.rules.is_empty());
        let example = evaluate_egress(&internet.cedar, "example.com").expect("egress");
        assert!(example.allowed);
        assert_eq!(example.deciding_rule, "internet:*");
        let explained = explain_host(&permissions, "example.com", ORIGIN).expect("host");
        assert!(explained.allowed());
        assert_eq!(explained.deciding_rule, "internet:*");
    }

    #[test]
    fn j2_source_ask_beats_tools_allow_and_blocks_always() {
        let permissions = perms(ToolDefault::Allow, &[], HostDefault::Allow, &[]);
        let tool_target = target("skill:jira/create_issue");
        let source = SourceRule::ask("approvalRequired", "skill jira");
        let resolved = resolve_tool(
            &permissions,
            ORIGIN,
            ToolQuery {
                name: "jira_create_issue",
                target: &tool_target,
                side_effect: Some(SideEffectClass::ReadOnly),
                source: Some(&source),
                org: None,
            },
        )
        .expect("resolve");
        assert_eq!(resolved.agent.decision, ScopeRule::Allow);
        assert_eq!(resolved.agent.rule, "tools:* default allow");
        assert_eq!(resolved.source.decision, ScopeRule::Ask);
        let entity = tool_entity(&resolved);
        assert_eq!(entity.agent_rule(), ScopeRule::Allow);
        assert_eq!(entity.source_rule(), ScopeRule::Ask);
        assert_eq!(entity.org_rule(), ScopeRule::None);
        assert_eq!(entity.target(), "skill:jira/create_issue");

        let offer = always_offered(&resolved);
        assert!(!offer.offered());
        assert_eq!(
            offer.reason(),
            Some("skill jira marks create_issue approvalRequired")
        );

        let explained = explain_tool(
            &permissions,
            ORIGIN,
            ToolQuery {
                name: "jira_create_issue",
                target: &tool_target,
                side_effect: Some(SideEffectClass::ReadOnly),
                source: Some(&source),
                org: None,
            },
        )
        .expect("explain");
        assert_eq!(explained.effect, ToolEffect::Hitl);
        assert_eq!(explained.deciding_scope, DecidingScope::Source);
        assert_eq!(explained.deciding_rule, "approvalRequired");
    }

    #[test]
    fn j5_exact_tool_beats_source_wildcard() {
        let permissions = perms(
            ToolDefault::Allow,
            &[
                ("skill:jira/*", PermissionDecision::Allow),
                ("skill:jira/create_issue", PermissionDecision::Ask),
            ],
            HostDefault::Allow,
            &[],
        );
        let tool_target = target("skill:jira/create_issue");
        let explained = explain_tool(
            &permissions,
            ORIGIN,
            query(
                "jira_create_issue",
                &tool_target,
                Some(SideEffectClass::MutableExternal),
            ),
        )
        .expect("explain");
        assert_eq!(explained.effect, ToolEffect::Hitl);
        assert_eq!(explained.deciding_scope, DecidingScope::Agent);
        assert_eq!(explained.deciding_rule, "skill:jira/create_issue");
        assert_eq!(explained.agent_row().decision, ScopeRule::Ask);
        assert_eq!(explained.agent_row().rule, "skill:jira/create_issue");
        assert_eq!(explained.source_row().decision, ScopeRule::None);
        assert!(always_offered_from(&permissions, &tool_target).offered());
    }

    #[test]
    fn section_iv_9_explain_rows() {
        let permissions = perms(
            ToolDefault::BySideEffect,
            &[("skill:jira/*", PermissionDecision::Allow)],
            HostDefault::Allow,
            &[("*.pastebin.com", PermissionDecision::Deny)],
        );
        let tool_target = target("skill:jira/create_issue");
        let source = SourceRule::ask("approvalRequired", "skill jira v2.1.0");
        let explained = explain_tool(
            &permissions,
            ORIGIN,
            ToolQuery {
                name: "jira_create_issue",
                target: &tool_target,
                side_effect: Some(SideEffectClass::MutableExternal),
                source: Some(&source),
                org: None,
            },
        )
        .expect("explain");
        assert_eq!(
            format_tool_explain(&explained),
            "\
ASK   jira_create_issue  (skill:jira/create_issue)
  agent  allow   skill:jira/*  (agent v1.3.0)
  source ask     approvalRequired  (skill jira v2.1.0)     <- decides
  org    none"
        );
        assert_eq!(explained.deciding_scope, DecidingScope::Source);
        assert_eq!(explained.deciding_rule, "approvalRequired");
    }

    #[test]
    fn j7_pastebin_wildcard_denies_subdomain_and_allows_github() {
        let permissions = perms(
            ToolDefault::BySideEffect,
            &[],
            HostDefault::Allow,
            &[("*.pastebin.com", PermissionDecision::Deny)],
        );
        let internet = resolve_internet(&permissions).expect("internet");
        assert_eq!(internet.default_decision, "allow");
        assert_eq!(
            internet.rules,
            vec![("*.pastebin.com".to_string(), "deny".to_string())]
        );
        assert!(internet.cedar.contains("@id(\"internet:*\")"));
        assert!(internet
            .cedar
            .contains("context.host like \"*.pastebin.com\""));

        let paste = explain_host(&permissions, "paste.pastebin.com", ORIGIN).expect("paste");
        assert_eq!(paste.decision, ScopeRule::Deny);
        assert_eq!(paste.deciding_rule, "*.pastebin.com");
        assert_eq!(
            format_host_explain(&paste),
            "DENY  paste.pastebin.com   agent rule *.pastebin.com at agent v1.3.0"
        );
        let github = explain_host(&permissions, "api.github.com", ORIGIN).expect("github");
        assert!(github.allowed());
        assert_eq!(github.deciding_rule, "internet:*");

        let paste_cedar = evaluate_egress(&internet.cedar, "paste.pastebin.com").expect("eval");
        assert!(!paste_cedar.allowed);
        assert_eq!(paste_cedar.deciding_rule, "*.pastebin.com");
        let github_cedar = evaluate_egress(&internet.cedar, "api.github.com").expect("eval");
        assert!(github_cedar.allowed);
        assert_eq!(github_cedar.deciding_rule, "internet:*");

        let apex = explain_host(&permissions, "pastebin.com", ORIGIN).expect("apex");
        assert!(apex.allowed());
        assert_eq!(apex.deciding_rule, "internet:*");
    }

    #[test]
    fn side_effect_classes_follow_by_side_effect() {
        let permissions = AgentPermissions::default();
        let tool_target = target("builtin:read_file");
        let cases = [
            (None, ScopeRule::Ask, "unspecified"),
            (
                Some(SideEffectClass::Unspecified),
                ScopeRule::Ask,
                "unspecified",
            ),
            (
                Some(SideEffectClass::ReadOnly),
                ScopeRule::Allow,
                "read-only",
            ),
            (
                Some(SideEffectClass::MutableLocal),
                ScopeRule::Allow,
                "mutable local",
            ),
            (
                Some(SideEffectClass::MutableExternal),
                ScopeRule::Ask,
                "mutable external",
            ),
        ];
        for (class, decision, phrase) in cases {
            let resolved = resolve_tool(
                &permissions,
                ORIGIN,
                query("read_file", &tool_target, class),
            )
            .expect("resolve");
            assert_eq!(resolved.agent.decision, decision, "{phrase}");
            assert_eq!(
                resolved.agent.rule,
                format!("tools:* default bySideEffect ({phrase})")
            );
            assert_eq!(resolved.source.decision, ScopeRule::None);
            assert_eq!(resolved.org.decision, ScopeRule::None);
            assert!(always_offered(&resolved).offered());
        }
    }

    #[test]
    fn explicit_defaults_ignore_side_effect() {
        let tool_target = target("skill:jira/create_issue");
        let cases = [
            (
                ToolDefault::Allow,
                ScopeRule::Allow,
                "tools:* default allow",
            ),
            (ToolDefault::Ask, ScopeRule::Ask, "tools:* default ask"),
            (ToolDefault::Deny, ScopeRule::Deny, "tools:* default deny"),
            (
                ToolDefault::Unspecified,
                ScopeRule::Ask,
                "tools:* default bySideEffect (mutable external)",
            ),
        ];
        for (default, decision, label) in cases {
            let permissions = perms(default, &[], HostDefault::Unspecified, &[]);
            let resolved = resolve_tool(
                &permissions,
                ORIGIN,
                query(
                    "create_issue",
                    &tool_target,
                    Some(SideEffectClass::MutableExternal),
                ),
            )
            .expect("resolve");
            assert_eq!(resolved.agent.decision, decision, "{label}");
            assert_eq!(resolved.agent.rule, label);
        }
    }

    #[test]
    fn specificity_table_picks_the_most_specific_matching_rule() {
        let tool_target = target("skill:jira/create_issue");
        let cases = [
            (
                vec![
                    ("tools:*", PermissionDecision::Allow),
                    ("skill:jira/*", PermissionDecision::Deny),
                    ("skill:jira/create_issue", PermissionDecision::Ask),
                ],
                ScopeRule::Ask,
                "skill:jira/create_issue",
            ),
            (
                vec![
                    ("tools:*", PermissionDecision::Deny),
                    ("skill:jira/*", PermissionDecision::Allow),
                ],
                ScopeRule::Allow,
                "skill:jira/*",
            ),
            (
                vec![("tools:*", PermissionDecision::Deny)],
                ScopeRule::Deny,
                "tools:*",
            ),
            (
                vec![("skill:other/*", PermissionDecision::Deny)],
                ScopeRule::Allow,
                "tools:* default allow",
            ),
            (
                vec![("builtin:run", PermissionDecision::Deny)],
                ScopeRule::Allow,
                "tools:* default allow",
            ),
            (
                vec![("mcp:mcp.atlassian.com/*", PermissionDecision::Deny)],
                ScopeRule::Allow,
                "tools:* default allow",
            ),
        ];
        for (rules, decision, label) in cases {
            let rules: Vec<(&str, PermissionDecision)> = rules;
            let permissions = perms(ToolDefault::Allow, &rules, HostDefault::Allow, &[]);
            let resolved = resolve_tool(
                &permissions,
                ORIGIN,
                query("create_issue", &tool_target, None),
            )
            .expect("resolve");
            assert_eq!(resolved.agent.decision, decision, "{label}");
            assert_eq!(resolved.agent.rule, label);
        }

        let mcp_target = target("mcp:mcp.atlassian.com/v1/createIssue");
        let permissions = perms(
            ToolDefault::Deny,
            &[
                ("mcp:mcp.atlassian.com/*", PermissionDecision::Deny),
                ("mcp:mcp.atlassian.com/v1/*", PermissionDecision::Allow),
                (
                    "mcp:https://MCP.Atlassian.com:443/v1/createIssue",
                    PermissionDecision::Ask,
                ),
            ],
            HostDefault::Allow,
            &[],
        );
        let resolved = resolve_tool(
            &permissions,
            ORIGIN,
            query("createIssue", &mcp_target, None),
        )
        .expect("mcp");
        assert_eq!(resolved.agent.decision, ScopeRule::Ask);
        assert_eq!(resolved.agent.rule, "mcp:mcp.atlassian.com/v1/createIssue");

        let sibling = target("mcp:mcp.atlassian.com/v1/list");
        let sibling =
            resolve_tool(&permissions, ORIGIN, query("list", &sibling, None)).expect("sibling");
        assert_eq!(sibling.agent.decision, ScopeRule::Allow);
        assert_eq!(sibling.agent.rule, "mcp:mcp.atlassian.com/v1/*");

        let other_server = target("mcp:mcp.atlassian.com/create_issue");
        let other = resolve_tool(
            &permissions,
            ORIGIN,
            query("create_issue", &other_server, None),
        )
        .expect("other server");
        assert_eq!(other.agent.decision, ScopeRule::Deny);
        assert_eq!(other.agent.rule, "mcp:mcp.atlassian.com/*");

        let builtin = target("builtin:run");
        let builtin_permissions = perms(
            ToolDefault::Allow,
            &[("builtin:run", PermissionDecision::Deny)],
            HostDefault::Allow,
            &[],
        );
        let resolved = resolve_tool(&builtin_permissions, ORIGIN, query("run", &builtin, None))
            .expect("builtin");
        assert_eq!(resolved.agent.decision, ScopeRule::Deny);
        assert_eq!(resolved.agent.rule, "builtin:run");
    }

    #[test]
    fn host_specificity_table() {
        let permissions = perms(
            ToolDefault::BySideEffect,
            &[],
            HostDefault::Deny,
            &[
                ("internet:*", PermissionDecision::Allow),
                ("*.com", PermissionDecision::Deny),
                ("*.pastebin.com", PermissionDecision::Allow),
                ("paste.pastebin.com", PermissionDecision::Deny),
                ("api.github.com", PermissionDecision::Allow),
            ],
        );
        let cases = [
            ("paste.pastebin.com", ScopeRule::Deny, "paste.pastebin.com"),
            ("evil.pastebin.com", ScopeRule::Allow, "*.pastebin.com"),
            ("pastebin.com", ScopeRule::Deny, "*.com"),
            ("api.github.com", ScopeRule::Allow, "api.github.com"),
            ("other.github.com", ScopeRule::Deny, "*.com"),
            ("example.org", ScopeRule::Allow, "internet:*"),
        ];
        for (host, decision, rule) in cases {
            let explained = explain_host(&permissions, host, ORIGIN).expect(host);
            assert_eq!(explained.decision, decision, "{host}");
            assert_eq!(explained.deciding_rule, rule, "{host}");
        }

        let internet = resolve_internet(&permissions).expect("compile");
        assert_eq!(internet.default_decision, "allow");
        assert!(!internet
            .rules
            .iter()
            .any(|(pattern, _)| pattern == "internet:*"));
        let cedar_paste =
            evaluate_egress(&internet.cedar, "paste.pastebin.com").expect("cedar paste");
        assert!(!cedar_paste.allowed);
        // Enforcement (compiled Cedar) must equal the explanation for every host:
        // Cedar forbid beats permit, so the resolver adds exceptions.
        for host in [
            "paste.pastebin.com",
            "evil.pastebin.com",
            "pastebin.com",
            "api.github.com",
            "other.github.com",
            "example.org",
        ] {
            let explained = explain_host(&permissions, host, ORIGIN).unwrap();
            let cedar = evaluate_egress(&internet.cedar, host).unwrap();
            assert_eq!(
                explained.decision == ScopeRule::Allow,
                cedar.allowed,
                "{host}: explain vs cedar"
            );
        }
    }

    #[test]
    fn org_deny_beats_agent_allow_and_blocks_always() {
        let permissions = perms(ToolDefault::Allow, &[], HostDefault::Allow, &[]);
        let tool_target = target("skill:jira/create_issue");
        let org = OrgRule::deny("org-guard", "security");
        let resolved = resolve_tool(
            &permissions,
            ORIGIN,
            ToolQuery {
                name: "create_issue",
                target: &tool_target,
                side_effect: None,
                source: None,
                org: Some(&org),
            },
        )
        .expect("resolve");
        let explained = explain_tool(
            &permissions,
            ORIGIN,
            ToolQuery {
                name: "create_issue",
                target: &tool_target,
                side_effect: None,
                source: None,
                org: Some(&org),
            },
        )
        .expect("explain");
        assert_eq!(explained.effect, ToolEffect::Deny);
        assert_eq!(explained.deciding_scope, DecidingScope::Org);
        assert_eq!(explained.deciding_rule, "org-guard");
        assert_eq!(
            always_offered(&resolved).reason(),
            Some("security marks create_issue org-guard")
        );
    }

    #[test]
    fn equal_tighten_names_the_org_rule() {
        let permissions = perms(ToolDefault::Allow, &[], HostDefault::Allow, &[]);
        let tool_target = target("builtin:run");
        let source = SourceRule::ask("approvalRequired", "skill shell");
        let org = OrgRule::ask("org-ask", "org");
        let resolved = resolve_tool(
            &permissions,
            ORIGIN,
            ToolQuery {
                name: "run",
                target: &tool_target,
                side_effect: Some(SideEffectClass::MutableLocal),
                source: Some(&source),
                org: Some(&org),
            },
        )
        .expect("resolve");
        assert_eq!(
            always_offered(&resolved).reason(),
            Some("org marks run org-ask")
        );
        assert_eq!(source.rule(), "approvalRequired");
        assert_eq!(org.decision(), ScopeRule::Ask);
    }

    #[test]
    fn source_deny_is_stricter_than_org_ask() {
        let permissions = AgentPermissions::default();
        let tool_target = target("skill:jira/create_issue");
        let org = OrgRule::ask("org-ask", "org");
        let resolved = resolve_tool(
            &permissions,
            ORIGIN,
            ToolQuery {
                name: "create_issue",
                target: &tool_target,
                side_effect: None,
                source: None,
                org: Some(&org),
            },
        )
        .expect("resolve");
        let mut with_source_deny = resolved.clone();
        with_source_deny.source = ScopeLabel::new(ScopeRule::Deny, "source-deny", "skill jira");
        assert_eq!(
            always_offered(&with_source_deny).reason(),
            Some("skill jira marks create_issue source-deny")
        );
    }

    #[test]
    fn rejects_a_wildcard_tool_target_and_a_host_ask() {
        let permissions = AgentPermissions::default();
        let wildcard = target("skill:jira/*");
        let err = resolve_tool(&permissions, ORIGIN, query("jira", &wildcard, None))
            .expect_err("wildcard");
        assert!(matches!(err, PermissionsError::NotExactTool(_)));

        let mut permissions = permissions;
        permissions.internet = Some(InternetPermissions {
            default: HostDefault::Allow as i32,
            rules: vec![rule("*.pastebin.com", PermissionDecision::Ask)],
        });
        let err = resolve_internet(&permissions).expect_err("ask");
        assert!(err.to_string().contains("allow or deny"));
    }

    #[test]
    fn default_deny_internet_allows_only_listed_hosts() {
        let permissions = perms(
            ToolDefault::BySideEffect,
            &[],
            HostDefault::Deny,
            &[("api.github.com", PermissionDecision::Allow)],
        );
        let explained = explain_host(&permissions, "api.github.com", ORIGIN).expect("github");
        assert!(explained.allowed());
        assert_eq!(explained.deciding_rule, "api.github.com");
        let other = explain_host(&permissions, "example.com", ORIGIN).expect("other");
        assert_eq!(other.decision, ScopeRule::Deny);
        assert_eq!(other.deciding_rule, "internet:*");
        let internet = resolve_internet(&permissions).expect("compile");
        assert_eq!(internet.default_decision, "deny");
        let cedar = evaluate_egress(&internet.cedar, "example.com").expect("eval");
        assert!(!cedar.allowed);
    }

    fn always_offered_from(permissions: &AgentPermissions, tool_target: &Target) -> AlwaysOffer {
        let resolved =
            resolve_tool(permissions, ORIGIN, query("tool", tool_target, None)).expect("resolve");
        always_offered(&resolved)
    }

    trait Rows {
        fn agent_row(&self) -> &ScopeExplanation;
        fn source_row(&self) -> &ScopeExplanation;
    }

    impl Rows for ToolDecision {
        fn agent_row(&self) -> &ScopeExplanation {
            self.scopes
                .iter()
                .find(|row| row.scope == DecidingScope::Agent)
                .expect("agent row")
        }

        fn source_row(&self) -> &ScopeExplanation {
            self.scopes
                .iter()
                .find(|row| row.scope == DecidingScope::Source)
                .expect("source row")
        }
    }
}
