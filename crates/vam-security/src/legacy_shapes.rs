//! Known Cedar shapes a legacy capability policy can become permission rules.
//!
//! Parsed with Cedar's parser (`PolicySet::from_str` and `Policy::to_json`).
//! A statement is a known shape when it is a permit or forbid of `ExecuteTool`
//! over a tool list (a `[Tool::"…"].contains(resource)` test, or
//! `resource == Tool::"…"`), optionally gated by `context.hitl_approved`.
//! The filesystem path limb copied into every example policy matches
//! [`BUILTIN_EXECUTE_TOOL_POLICY`] and is ignored: permissions mode already
//! carries those three statements.
//!
//! Within one policy, Cedar's forbid-overrides and permit-overrides decide a
//! tool: an unconditional forbid is `deny`, an unconditional permit is
//! `allow` even when another statement only permits that tool after approval,
//! and a permit that fires only with `hitl_approved` is `ask`.

use std::collections::BTreeMap;
use std::str::FromStr;

use cedar_policy::PolicySet;
use serde_json::{json, Value};

use crate::advertise::BUILTIN_EXECUTE_TOOL_POLICY;
use crate::pdp::ScopeRule;
use crate::SecurityError;

/// Guest tool names a legacy capability policy turned into decisions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyCapabilityConversion {
    /// One decision per guest tool name. Deny wins, then an unconditional
    /// allow, then ask.
    pub tools: BTreeMap<String, ScopeRule>,
    /// Every statement was a known tool shape, the path limb, or a permit
    /// that names a different agent.
    pub fully_recognized: bool,
    /// Why a statement was not a known shape. Empty when `fully_recognized`.
    pub unrecognized_reasons: Vec<String>,
}

/// Classify `cedar` for `agent_urn`.
///
/// A `principal == Agent::"…"` statement applies only when the id is
/// `agent_urn`. A statement for a different agent is skipped. Anything else
/// that is not a known tool shape or the path limb sets
/// [`LegacyCapabilityConversion::fully_recognized`] to false. Known tool
/// statements are still returned, so the caller can emit rules for them.
pub fn convert_legacy_capability(
    cedar: &str,
    agent_urn: &str,
) -> Result<LegacyCapabilityConversion, SecurityError> {
    let set = PolicySet::from_str(cedar).map_err(|err| {
        SecurityError::PolicySyntax(format!("capability policy did not parse: {err}"))
    })?;
    let limbs = path_limb_shapes()?;
    let mut tools = BTreeMap::new();
    let mut fully_recognized = true;
    let mut unrecognized_reasons = Vec::new();

    if set.templates().next().is_some() {
        fully_recognized = false;
        unrecognized_reasons.push("policy set contains a template".to_string());
    }

    for policy in set.policies() {
        if !policy.is_static() {
            fully_recognized = false;
            unrecognized_reasons.push(format!("policy {} is a template link", policy.id()));
            continue;
        }
        let json = policy.to_json().map_err(|err| {
            SecurityError::PolicySyntax(format!("policy {} has no JSON form: {err}", policy.id()))
        })?;
        let shaped = shape(&json);
        if limbs.iter().any(|limb| limb == &shaped) {
            continue;
        }
        match statement_tools(&shaped, agent_urn) {
            Statement::Skip => {}
            Statement::Tools(found) => {
                for (name, decision) in found {
                    merge_tool(&mut tools, name, decision);
                }
            }
            Statement::Unrecognized(reason) => {
                fully_recognized = false;
                unrecognized_reasons.push(format!("policy {}: {reason}", policy.id()));
            }
        }
    }

    Ok(LegacyCapabilityConversion {
        tools,
        fully_recognized,
        unrecognized_reasons,
    })
}

fn path_limb_shapes() -> Result<Vec<Value>, SecurityError> {
    let set = PolicySet::from_str(BUILTIN_EXECUTE_TOOL_POLICY).map_err(|err| {
        SecurityError::PolicySyntax(format!("builtin policy did not parse: {err}"))
    })?;
    let mut limbs = Vec::new();
    for policy in set.policies() {
        let json = policy.to_json().map_err(|err| {
            SecurityError::PolicySyntax(format!("builtin policy has no JSON form: {err}"))
        })?;
        let shaped = shape(&json);
        if action_id(&shaped) != Some("ExecuteTool") {
            limbs.push(shaped);
        }
    }
    Ok(limbs)
}

fn shape(json: &Value) -> Value {
    json!({
        "effect": json.get("effect").cloned().unwrap_or(Value::Null),
        "principal": json.get("principal").cloned().unwrap_or(Value::Null),
        "action": json.get("action").cloned().unwrap_or(Value::Null),
        "resource": json.get("resource").cloned().unwrap_or(Value::Null),
        "conditions": json.get("conditions").cloned().unwrap_or(json!([])),
    })
}

enum Statement {
    Skip,
    Tools(Vec<(String, ScopeRule)>),
    Unrecognized(String),
}

fn statement_tools(shaped: &Value, agent_urn: &str) -> Statement {
    match principal_scope(&shaped["principal"], agent_urn) {
        PrincipalScope::Applies => {}
        PrincipalScope::OtherAgent => return Statement::Skip,
        PrincipalScope::Unknown => {
            return Statement::Unrecognized(
                "principal is not unconstrained or this agent".to_string(),
            );
        }
    }
    if action_id(shaped) != Some("ExecuteTool") {
        return Statement::Unrecognized("action is not ExecuteTool".to_string());
    }
    let effect = shaped["effect"].as_str().unwrap_or("");
    let resource = resource_tools(&shaped["resource"]);
    let conditions = shaped["conditions"].as_array();
    let Some(conditions) = conditions else {
        return Statement::Unrecognized("conditions are not a list".to_string());
    };
    if conditions.len() > 1 {
        return Statement::Unrecognized("more than one condition clause".to_string());
    }
    let (kind, body) = match conditions.first() {
        None => (None, None),
        Some(clause) => (clause["kind"].as_str(), Some(&clause["body"])),
    };

    let mut conjuncts = Vec::new();
    if let Some(body) = body {
        flatten_and(body, &mut conjuncts);
    }
    let mut listed: Option<Vec<String>> = None;
    let mut saw_has = false;
    let mut saw_eq = false;
    for piece in &conjuncts {
        if is_hitl_has(piece) {
            saw_has = true;
        } else if is_hitl_eq(piece) {
            saw_eq = true;
        } else if let Some(names) = tool_contains(piece) {
            if listed.is_some() {
                return Statement::Unrecognized("more than one tool list".to_string());
            }
            listed = Some(names);
        } else {
            return Statement::Unrecognized(
                "condition is not a tool list or an approval gate".to_string(),
            );
        }
    }
    let hitl = saw_has && saw_eq;
    if (saw_has || saw_eq) && !hitl {
        return Statement::Unrecognized("approval gate is incomplete".to_string());
    }

    let tools = match (resource, listed) {
        (ResourceTools::One(name), None) => vec![name],
        (ResourceTools::All, Some(names)) => names,
        (ResourceTools::One(name), Some(names)) if names == vec![name.clone()] => vec![name],
        (ResourceTools::All, None) if conjuncts.is_empty() => {
            return Statement::Unrecognized("permit or forbid of every tool".to_string());
        }
        _ => {
            return Statement::Unrecognized("resource is not a tool list or one tool".to_string());
        }
    };
    if tools.is_empty() {
        return Statement::Unrecognized("tool list is empty".to_string());
    }

    let decision = match (effect, kind, hitl) {
        ("permit", None, false) => ScopeRule::Allow,
        ("forbid", None, false) => ScopeRule::Deny,
        ("permit", Some("when"), false) => ScopeRule::Allow,
        ("forbid", Some("when"), false) => ScopeRule::Deny,
        ("permit", Some("when"), true) => ScopeRule::Ask,
        ("forbid", Some("unless"), true) => ScopeRule::Ask,
        _ => {
            return Statement::Unrecognized(
                "effect and condition are not allow, ask, or deny".to_string(),
            );
        }
    };
    Statement::Tools(tools.into_iter().map(|name| (name, decision)).collect())
}

enum PrincipalScope {
    Applies,
    OtherAgent,
    Unknown,
}

fn principal_scope(principal: &Value, agent_urn: &str) -> PrincipalScope {
    match principal["op"].as_str() {
        Some("All") => PrincipalScope::Applies,
        Some("==") => {
            let entity = &principal["entity"];
            if entity["type"] == "Agent" {
                if entity["id"].as_str() == Some(agent_urn) {
                    PrincipalScope::Applies
                } else {
                    PrincipalScope::OtherAgent
                }
            } else {
                PrincipalScope::Unknown
            }
        }
        _ => PrincipalScope::Unknown,
    }
}

fn action_id(shaped: &Value) -> Option<&str> {
    let action = &shaped["action"];
    if action["op"] != "eq" && action["op"] != "==" {
        return None;
    }
    if action["entity"]["type"] != "Action" {
        return None;
    }
    action["entity"]["id"].as_str()
}

enum ResourceTools {
    All,
    One(String),
    Other,
}

fn resource_tools(resource: &Value) -> ResourceTools {
    match resource["op"].as_str() {
        Some("All") => ResourceTools::All,
        Some("==") if resource["entity"]["type"] == "Tool" => resource["entity"]["id"]
            .as_str()
            .map(|id| ResourceTools::One(id.to_string()))
            .unwrap_or(ResourceTools::Other),
        _ => ResourceTools::Other,
    }
}

fn flatten_and<'a>(expr: &'a Value, out: &mut Vec<&'a Value>) {
    if let Some(and) = expr.get("&&") {
        flatten_and(&and["left"], out);
        flatten_and(&and["right"], out);
    } else {
        out.push(expr);
    }
}

fn is_hitl_has(expr: &Value) -> bool {
    let Some(has) = expr.get("has") else {
        return false;
    };
    has.get("left") == Some(&json!({"Var": "context"})) && attr_is(&has["attr"], "hitl_approved")
}

fn is_hitl_eq(expr: &Value) -> bool {
    let Some(eq) = expr.get("==") else {
        return false;
    };
    eq.get("right") == Some(&json!({"Value": true}))
        && eq.get("left").is_some_and(|left| {
            left.get(".").is_some_and(|dot| {
                dot.get("left") == Some(&json!({"Var": "context"}))
                    && attr_is(&dot["attr"], "hitl_approved")
            })
        })
}

fn attr_is(attr: &Value, name: &str) -> bool {
    match attr {
        Value::String(text) => text == name,
        Value::Array(parts) => {
            parts.len() == 1 && parts.first().and_then(Value::as_str) == Some(name)
        }
        _ => false,
    }
}

fn tool_contains(expr: &Value) -> Option<Vec<String>> {
    let contains = expr.get("contains")?;
    if contains.get("right") != Some(&json!({"Var": "resource"})) {
        return None;
    }
    let set = contains.get("left")?.get("Set")?.as_array()?;
    let mut names = Vec::with_capacity(set.len());
    for item in set {
        let entity = item.get("Value")?.get("__entity")?;
        if entity.get("type")? != "Tool" {
            return None;
        }
        names.push(entity.get("id")?.as_str()?.to_string());
    }
    if names.is_empty() {
        None
    } else {
        Some(names)
    }
}

fn merge_tool(tools: &mut BTreeMap<String, ScopeRule>, name: String, decision: ScopeRule) {
    tools
        .entry(name)
        .and_modify(|current| *current = merge_decision(*current, decision))
        .or_insert(decision);
}

/// Forbid wins. An unconditional permit wins over an approval-gated permit.
fn merge_decision(left: ScopeRule, right: ScopeRule) -> ScopeRule {
    match (left, right) {
        (ScopeRule::Deny, _) | (_, ScopeRule::Deny) => ScopeRule::Deny,
        (ScopeRule::Allow, _) | (_, ScopeRule::Allow) => ScopeRule::Allow,
        _ => ScopeRule::Ask,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const JIRA_POLICY: &str = include_str!("../../../examples/jira-assistant/policy.cedar");
    const SQLITE_POLICY: &str = include_str!("../../../examples/sqlite-assistant/policy.cedar");
    const JIRA_AGENT: &str = "urn:vam:agent:local.vam.dev/vam/jira-assistant";
    const SQLITE_AGENT: &str = "urn:vam:agent:local.vam.dev/vam/sqlite-assistant";

    #[test]
    fn jira_policy_is_only_known_shapes_and_the_path_limb() {
        let converted = convert_legacy_capability(JIRA_POLICY, JIRA_AGENT).expect("parse");
        assert!(
            converted.fully_recognized,
            "{:?}",
            converted.unrecognized_reasons
        );
        assert_eq!(
            converted.tools.get("jira_search_issues"),
            Some(&ScopeRule::Allow)
        );
        assert_eq!(converted.tools.get("read_file"), Some(&ScopeRule::Allow));
        assert_eq!(converted.tools.get("load_skill"), Some(&ScopeRule::Allow));
        assert_eq!(
            converted.tools.get("jira_create_issue"),
            Some(&ScopeRule::Ask)
        );
        assert_eq!(
            converted.tools.get("jira_transition_issue"),
            Some(&ScopeRule::Ask)
        );
        assert_eq!(converted.tools.get("http_write"), Some(&ScopeRule::Ask));
        assert!(!converted.tools.contains_key("write_file"));
    }

    #[test]
    fn sqlite_query_guard_is_not_a_known_shape_and_query_db_stays_allow() {
        let converted = convert_legacy_capability(SQLITE_POLICY, SQLITE_AGENT).expect("parse");
        assert!(
            !converted.fully_recognized,
            "the SQL forbid is not a tool-list forbid"
        );
        assert_eq!(converted.tools.get("query_db"), Some(&ScopeRule::Allow));
        assert_eq!(converted.tools.get("init_db"), Some(&ScopeRule::Allow));
        assert_eq!(converted.tools.get("run"), Some(&ScopeRule::Allow));
        assert!(!converted.tools.contains_key("delete_file"));
    }

    #[test]
    fn an_unconditional_permit_wins_over_an_approval_gate_for_the_same_tool() {
        let cedar = r#"
            permit (principal, action == Action::"ExecuteTool", resource == Tool::"echo");
            permit (principal, action == Action::"ExecuteTool", resource == Tool::"echo")
            when { context has hitl_approved && context.hitl_approved == true };
        "#;
        let converted = convert_legacy_capability(cedar, JIRA_AGENT).expect("parse");
        assert!(converted.fully_recognized);
        assert_eq!(converted.tools.get("echo"), Some(&ScopeRule::Allow));
    }

    #[test]
    fn a_context_gate_other_than_approval_is_unrecognized() {
        let cedar = r#"
            permit (principal, action == Action::"ExecuteTool", resource)
            when { context has reason && context.reason == "because" };
        "#;
        let converted = convert_legacy_capability(cedar, JIRA_AGENT).expect("parse");
        assert!(!converted.fully_recognized);
        assert!(converted.tools.is_empty());
    }
}
