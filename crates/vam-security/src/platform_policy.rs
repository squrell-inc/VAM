//! Fixed platform policy set for tool rules, and host-rule compilation.
//!
//! Tool decisions are data on the `Tool` entity. This module holds the three
//! Cedar statements that read those attributes (§IV.3) and the compiler that
//! turns resolved host rules into `EgressRequest` policies (§IV.4).

use std::collections::HashSet;
use std::str::FromStr;

use crate::engine::CedarPolicyEngine;
use crate::pdp::{
    DecisionOutcome, EvaluationContext, EvaluationRequest, PolicyDecisionPoint, PolicyMode,
};
use crate::SecurityError;

/// The three platform statements. A playbook overlay is appended with
/// [`compose_platform_policy`].
pub const PLATFORM_TOOL_POLICY: &str = r#"
permit (principal, action == Action::"ExecuteTool", resource)
when { resource.agent_rule == "allow" || resource.agent_rule == "ask" };

forbid (principal, action == Action::"ExecuteTool", resource)
when { resource.org_rule == "deny" || resource.source_rule == "deny" ||
       resource.agent_rule == "deny" };

forbid (principal, action == Action::"ExecuteTool", resource)
when { resource.org_rule == "ask" || resource.source_rule == "ask" ||
       resource.agent_rule == "ask" }
unless { context has hitl_approved && context.hitl_approved == true };
"#;

/// Append `overlay` (playbook Cedar, or empty) after [`PLATFORM_TOOL_POLICY`].
///
/// Same concatenation as [`crate::compose_policy`].
pub fn compose_platform_policy(overlay: &str) -> String {
    crate::advertise::compose_policy(PLATFORM_TOOL_POLICY, overlay)
}

/// Two-pass HITL over one `ExecuteTool` request.
///
/// Allow when the first pass allows. Deny when both passes deny. Suspend
/// when the first pass denies and the same request with `hitl_approved`
/// allows. Does not register an approval id; the orchestrator still owns that.
pub fn evaluate_tool_decision(
    pdp: &dyn PolicyDecisionPoint,
    req: &EvaluationRequest,
) -> DecisionOutcome {
    let first = pdp.evaluate(req);
    if first.is_allowed() {
        return first;
    }
    let mut approved = req.clone();
    approved.context.hitl_approved = Some(true);
    let second = pdp.evaluate(&approved);
    if second.is_allowed() {
        DecisionOutcome::Suspend {
            approval_request_id: String::new(),
            diagnostic_message: "requires human approval before execution".to_string(),
        }
    } else {
        first
    }
}

/// Outcome of one `EgressRequest` evaluation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EgressDecision {
    pub allowed: bool,
    /// Policy id that determined the outcome. `internet:*` is the blanket
    /// permit. A host rule's id is its pattern (`*.pastebin.com`).
    pub deciding_rule: String,
}

/// Compile resolved host rules into Cedar.
///
/// `default` is the `internet:*` decision, `"allow"` or `"deny"`. Each rule
/// is `(pattern, "allow" | "deny")`. A deny becomes
/// `forbid ... when { context.host like "..." }`. `"allow"` for `internet:*`
/// becomes one unconditional `permit`. Each allow pattern becomes its own
/// `permit`.
pub fn compile_egress_policy(
    default: &str,
    rules: &[(&str, &str)],
) -> Result<String, SecurityError> {
    let rules: Vec<EgressRule<'_>> = rules
        .iter()
        .map(|(pattern, decision)| EgressRule {
            pattern,
            decision,
            except: Vec::new(),
        })
        .collect();
    compile_egress_rules(default, &rules)
}

/// One resolved host rule. `except` lists the patterns of more specific rules
/// that this rule must not decide. Cedar's `forbid` overrides every `permit`,
/// so a deny on `*.com` would otherwise beat an allow on `*.pastebin.com`. The
/// exceptions let the most specific rule win, as the resolver reports.
#[derive(Debug, Clone)]
pub struct EgressRule<'a> {
    pub pattern: &'a str,
    pub decision: &'a str,
    pub except: Vec<&'a str>,
}

/// [`compile_egress_policy`] with per-rule exceptions.
pub fn compile_egress_rules(
    default: &str,
    rules: &[EgressRule<'_>],
) -> Result<String, SecurityError> {
    let default = default.trim();
    if default != "allow" && default != "deny" {
        return Err(SecurityError::PolicySyntax(format!(
            "egress default must be \"allow\" or \"deny\", found {default:?}"
        )));
    }

    let mut seen = HashSet::new();
    let mut cedar = String::new();
    if default == "allow" {
        seen.insert("internet:*".to_string());
        cedar.push_str(
            r#"@id("internet:*")
permit (
    principal,
    action == Action::"EgressRequest",
    resource
);
"#,
        );
    }

    for rule in rules {
        let (pattern, decision) = (rule.pattern, rule.decision);
        let effect = match decision {
            "allow" => "permit",
            "deny" => "forbid",
            other => {
                return Err(SecurityError::PolicySyntax(format!(
                    "egress rule decision must be \"allow\" or \"deny\", found {other:?}"
                )));
            }
        };
        let literal = cedar_literal(pattern)?;
        let mut excepts = Vec::new();
        for except in &rule.except {
            excepts.push(format!("context.host like \"{}\"", cedar_literal(except)?));
        }
        let unless = if excepts.is_empty() {
            String::new()
        } else {
            format!(" unless {{ {} }}", excepts.join(" || "))
        };
        if !seen.insert(literal.clone()) {
            return Err(SecurityError::PolicySyntax(format!(
                "duplicate egress rule id {literal:?}"
            )));
        }
        cedar.push_str(&format!(
            r#"@id("{literal}")
{effect} (
    principal,
    action == Action::"EgressRequest",
    resource
)
when {{ context.host like "{literal}" }}{unless};
"#
        ));
    }

    CedarPolicyEngine::from_default_schema(&cedar, PolicyMode::Enforce, 1)?;
    Ok(cedar)
}

/// Evaluate `policy_set` for `host` as `Action::"EgressRequest"`.
pub fn evaluate_egress(policy_set: &str, host: &str) -> Result<EgressDecision, SecurityError> {
    let engine = CedarPolicyEngine::from_default_schema(policy_set, PolicyMode::Enforce, 1)?;
    // Cedar's text parser numbers policies `policy0`, `policy1`, … The
    // deciding rule is the `@id` annotation on the policy that fired.
    let parsed = cedar_policy::PolicySet::from_str(policy_set)
        .map_err(|err| SecurityError::PolicySyntax(format!("Policy set parse error: {err}")))?;
    let request = EvaluationRequest {
        principal: "Agent::\"egress\"".to_string(),
        action: "Action::\"EgressRequest\"".to_string(),
        resource: "Host::\"egress\"".to_string(),
        context: EvaluationContext {
            host: Some(host.to_string()),
            ..Default::default()
        },
        entities: Vec::new(),
    };
    match engine.evaluate(&request) {
        DecisionOutcome::Allow { matching_policies } => Ok(EgressDecision {
            allowed: true,
            deciding_rule: deciding_rule(&parsed, &matching_policies),
        }),
        DecisionOutcome::Deny {
            matching_policies, ..
        } => Ok(EgressDecision {
            allowed: false,
            deciding_rule: deciding_rule(&parsed, &matching_policies),
        }),
        DecisionOutcome::Suspend { .. } => Ok(EgressDecision {
            allowed: false,
            deciding_rule: String::new(),
        }),
    }
}

fn deciding_rule(policies: &cedar_policy::PolicySet, ids: &[String]) -> String {
    let labels: Vec<String> = ids.iter().map(|id| rule_label(policies, id)).collect();
    let specific: Vec<&str> = labels
        .iter()
        .map(String::as_str)
        .filter(|id| *id != "internet:*")
        .collect();
    if specific.len() == 1 {
        specific[0].to_string()
    } else if !specific.is_empty() {
        specific.join(",")
    } else {
        labels.first().cloned().unwrap_or_default()
    }
}

fn rule_label(policies: &cedar_policy::PolicySet, id: &str) -> String {
    policies
        .annotation(&cedar_policy::PolicyId::new(id), "id")
        .unwrap_or(id)
        .to_string()
}

fn cedar_literal(value: &str) -> Result<String, SecurityError> {
    if value.is_empty()
        || value
            .chars()
            .any(|c| matches!(c, '"' | '\\' | '\n' | '\r' | '\0'))
    {
        return Err(SecurityError::PolicySyntax(format!(
            "egress pattern is not a safe Cedar string: {value:?}"
        )));
    }
    Ok(value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pdp::{ScopeRule, ToolEntity};
    use crate::DEFAULT_CEDAR_SCHEMA;
    use cedar_policy::{Entities, Schema};

    fn engine(policy: &str) -> CedarPolicyEngine {
        CedarPolicyEngine::from_default_schema(policy, PolicyMode::Enforce, 1)
            .expect("policy should validate")
    }

    fn tool_request(tool: ToolEntity) -> EvaluationRequest {
        let resource = format!("Tool::\"{}\"", tool.id());
        EvaluationRequest {
            principal: "Agent::\"urn:vam:agent:examples.local/vam/demo\"".to_string(),
            action: "Action::\"ExecuteTool\"".to_string(),
            resource,
            context: EvaluationContext::default(),
            entities: vec![tool],
        }
    }

    #[test]
    fn missing_tool_attribute_cannot_be_built_and_source_deny_forbids() {
        let missing = r#"{"id":"jira_create_issue","agent_rule":"allow","org_rule":"none","target":"skill:jira/create_issue"}"#;
        assert!(
            serde_json::from_str::<ToolEntity>(missing).is_err(),
            "ToolEntity JSON must include all four attributes"
        );

        let (schema, _) =
            Schema::from_cedarschema_str(DEFAULT_CEDAR_SCHEMA).expect("schema parses");
        let partial = r#"[{
            "uid": {"type": "Tool", "id": "jira_create_issue"},
            "attrs": {
                "agent_rule": "allow",
                "org_rule": "none",
                "target": "skill:jira/create_issue"
            },
            "parents": []
        }]"#;
        assert!(
            Entities::from_json_str(partial, Some(&schema)).is_err(),
            "Cedar schema must reject a Tool missing source_rule"
        );

        let tool = ToolEntity::new(
            "jira_create_issue",
            ScopeRule::Allow,
            ScopeRule::Deny,
            ScopeRule::None,
            "skill:jira/create_issue",
        );
        let outcome = engine(PLATFORM_TOOL_POLICY).evaluate(&tool_request(tool));
        match outcome {
            DecisionOutcome::Deny {
                matching_policies,
                diagnostic_message,
            } => {
                assert!(
                    !matching_policies.is_empty(),
                    "forbid should fire, got {diagnostic_message}"
                );
                assert!(
                    !diagnostic_message.contains("evaluation errors"),
                    "{diagnostic_message}"
                );
            }
            other => panic!("source deny should forbid, got {other:?}"),
        }
    }

    #[test]
    fn agent_allow_is_allow_and_ask_suspends_until_approved() {
        let allow = tool_request(ToolEntity::new(
            "read_file",
            ScopeRule::Allow,
            ScopeRule::None,
            ScopeRule::None,
            "builtin:read_file",
        ));
        assert!(evaluate_tool_decision(&engine(PLATFORM_TOOL_POLICY), &allow).is_allowed());

        let ask = tool_request(ToolEntity::new(
            "jira_create_issue",
            ScopeRule::Ask,
            ScopeRule::None,
            ScopeRule::None,
            "skill:jira/create_issue",
        ));
        let pdp = engine(PLATFORM_TOOL_POLICY);
        assert!(evaluate_tool_decision(&pdp, &ask).is_suspended());
        let mut approved = ask.clone();
        approved.context.hitl_approved = Some(true);
        assert!(pdp.evaluate(&approved).is_allowed());
    }

    #[test]
    fn any_deny_stays_deny_and_agent_none_is_denied() {
        let cases = [
            (ScopeRule::Deny, ScopeRule::None, ScopeRule::None),
            (ScopeRule::Allow, ScopeRule::Deny, ScopeRule::None),
            (ScopeRule::Allow, ScopeRule::None, ScopeRule::Deny),
            (ScopeRule::Ask, ScopeRule::None, ScopeRule::Deny),
        ];
        for (agent, source, org) in cases {
            let req = tool_request(ToolEntity::new("tool", agent, source, org, "tool"));
            let pdp = engine(PLATFORM_TOOL_POLICY);
            let outcome = evaluate_tool_decision(&pdp, &req);
            assert!(
                outcome.is_denied(),
                "{agent:?}/{source:?}/{org:?} -> {outcome:?}"
            );
            let mut approved = req.clone();
            approved.context.hitl_approved = Some(true);
            assert!(pdp.evaluate(&approved).is_denied());
        }

        let none = tool_request(ToolEntity::new(
            "tool",
            ScopeRule::None,
            ScopeRule::None,
            ScopeRule::None,
            "tool",
        ));
        assert!(evaluate_tool_decision(&engine(PLATFORM_TOOL_POLICY), &none).is_denied());
    }

    #[test]
    fn empty_entities_keep_legacy_cedar_behavior() {
        let policy = r#"
permit (
    principal,
    action == Action::"ExecuteTool",
    resource == Tool::"read_file"
);
"#;
        let pdp = engine(policy);
        let allow = EvaluationRequest {
            principal: "Agent::\"legacy\"".to_string(),
            action: "Action::\"ExecuteTool\"".to_string(),
            resource: "Tool::\"read_file\"".to_string(),
            context: EvaluationContext::default(),
            entities: Vec::new(),
        };
        assert!(pdp.evaluate(&allow).is_allowed());

        let mut with_entity = allow.clone();
        with_entity.entities.push(ToolEntity::new(
            "read_file",
            ScopeRule::None,
            ScopeRule::None,
            ScopeRule::None,
            "builtin:read_file",
        ));
        assert!(pdp.evaluate(&with_entity).is_allowed());

        let deny = EvaluationRequest {
            resource: "Tool::\"run\"".to_string(),
            ..allow
        };
        assert!(pdp.evaluate(&deny).is_denied());
    }

    #[test]
    fn compose_appends_playbook_overlay() {
        assert_eq!(
            compose_platform_policy("").trim(),
            PLATFORM_TOOL_POLICY.trim()
        );
        let overlay = r#"permit (
    principal,
    action == Action::"ExecuteTool",
    resource == Tool::"run_step"
);"#;
        let cedar = compose_platform_policy(overlay);
        assert!(cedar.contains("resource.agent_rule"));
        assert!(cedar.contains("Tool::\"run_step\""));
        let pdp = engine(&cedar);
        let req = tool_request(ToolEntity::new(
            "run_step",
            ScopeRule::None,
            ScopeRule::None,
            ScopeRule::None,
            "builtin:run_step",
        ));
        assert!(pdp.evaluate(&req).is_allowed());
    }

    #[test]
    fn egress_allows_internet_default_and_denies_pastebin() {
        let policy = compile_egress_policy("allow", &[("*.pastebin.com", "deny")])
            .expect("egress policy compiles");
        assert!(policy.contains("context.host like \"*.pastebin.com\""));
        assert!(policy.contains("@id(\"internet:*\")"));

        let github = evaluate_egress(&policy, "api.github.com").expect("eval");
        assert!(github.allowed);
        assert_eq!(github.deciding_rule, "internet:*");

        let pastebin = evaluate_egress(&policy, "x.pastebin.com").expect("eval");
        assert!(!pastebin.allowed);
        assert_eq!(pastebin.deciding_rule, "*.pastebin.com");
    }

    #[test]
    fn egress_allow_rules_permit_listed_hosts_when_default_is_deny() {
        let policy = compile_egress_policy(
            "deny",
            &[("api.github.com", "allow"), ("*.pastebin.com", "deny")],
        )
        .expect("compiles");
        assert!(!policy.contains("internet:*"));
        let github = evaluate_egress(&policy, "api.github.com").expect("eval");
        assert!(github.allowed);
        assert_eq!(github.deciding_rule, "api.github.com");
        let other = evaluate_egress(&policy, "evil.test").expect("eval");
        assert!(!other.allowed);
        assert!(compile_egress_policy("ask", &[]).is_err());
        assert!(compile_egress_policy("allow", &[("a\"b", "deny")]).is_err());
    }
}
