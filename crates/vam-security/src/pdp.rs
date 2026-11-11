use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PolicyMode {
    /// Strict enforcement: decisions are binding and denials halt/reject execution.
    Enforce,
    /// Shadow / dry-run: evaluates and logs what would happen, but does not block.
    Shadow,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DecisionOutcome {
    Allow {
        matching_policies: Vec<String>,
    },
    Deny {
        diagnostic_message: String,
        matching_policies: Vec<String>,
    },
    Suspend {
        approval_request_id: String,
        diagnostic_message: String,
    },
}

impl DecisionOutcome {
    pub fn is_allowed(&self) -> bool {
        matches!(self, DecisionOutcome::Allow { .. })
    }

    pub fn is_denied(&self) -> bool {
        matches!(self, DecisionOutcome::Deny { .. })
    }

    pub fn is_suspended(&self) -> bool {
        matches!(self, DecisionOutcome::Suspend { .. })
    }

    /// Policies Cedar reported for this outcome.
    ///
    /// Allow and Deny carry them. Suspend is built by the host after a second
    /// evaluation and has no list; use the Allow policies from that pass.
    pub fn matching_policies(&self) -> &[String] {
        match self {
            DecisionOutcome::Allow { matching_policies } => matching_policies,
            DecisionOutcome::Deny {
                matching_policies, ..
            } => matching_policies,
            DecisionOutcome::Suspend { .. } => &[],
        }
    }

    pub fn diagnostic_message(&self) -> &str {
        match self {
            DecisionOutcome::Allow { .. } => "",
            DecisionOutcome::Deny {
                diagnostic_message, ..
            } => diagnostic_message,
            DecisionOutcome::Suspend {
                diagnostic_message, ..
            } => diagnostic_message,
        }
    }
}

/// Decision written on one scope of a [`ToolEntity`].
///
/// The host sets every scope. [`ScopeRule::None`] is the value when that
/// scope has no rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScopeRule {
    Allow,
    Ask,
    Deny,
    None,
}

impl ScopeRule {
    pub fn as_str(self) -> &'static str {
        match self {
            ScopeRule::Allow => "allow",
            ScopeRule::Ask => "ask",
            ScopeRule::Deny => "deny",
            ScopeRule::None => "none",
        }
    }
}

/// Resource entity for `Action::"ExecuteTool"`.
///
/// Fields are private and [`ToolEntity::new`] takes every attribute, so a
/// `Tool` cannot be built with one missing. The Cedar schema marks the same
/// four attributes required.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolEntity {
    id: String,
    agent_rule: ScopeRule,
    source_rule: ScopeRule,
    org_rule: ScopeRule,
    target: String,
}

impl ToolEntity {
    pub fn new(
        id: impl Into<String>,
        agent_rule: ScopeRule,
        source_rule: ScopeRule,
        org_rule: ScopeRule,
        target: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            agent_rule,
            source_rule,
            org_rule,
            target: target.into(),
        }
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn agent_rule(&self) -> ScopeRule {
        self.agent_rule
    }

    pub fn source_rule(&self) -> ScopeRule {
        self.source_rule
    }

    pub fn org_rule(&self) -> ScopeRule {
        self.org_rule
    }

    pub fn target(&self) -> &str {
        &self.target
    }

    pub(crate) fn cedar_entity_json(&self) -> serde_json::Value {
        serde_json::json!({
            "uid": {"type": "Tool", "id": self.id},
            "attrs": {
                "agent_rule": self.agent_rule.as_str(),
                "source_rule": self.source_rule.as_str(),
                "org_rule": self.org_rule.as_str(),
                "target": self.target,
            },
            "parents": [],
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvaluationRequest {
    pub principal: String,
    pub action: String,
    pub resource: String,
    pub context: EvaluationContext,
    /// Resource entities for this request. Empty keeps legacy evaluation:
    /// the engine passes an empty Cedar entity store, as it does today.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entities: Vec<ToolEntity>,
}

impl EvaluationRequest {
    pub fn with_entities(mut self, entities: Vec<ToolEntity>) -> Self {
        self.entities = entities;
        self
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EvaluationContext {
    pub args_digest: Option<String>,
    pub script_digest: Option<String>,
    pub side_effect: Option<i64>,
    pub iteration: Option<i64>,
    pub spend_micro_amount: Option<i64>,
    pub token_count: Option<i64>,
    pub restored_from_snapshot: Option<bool>,
    pub session_lineage_depth: Option<i64>,
    pub lifecycle_generation: Option<i64>,
    pub hitl_approved: Option<bool>,
    pub query: Option<String>,
    pub file_size_bytes: Option<i64>,
    pub path: Option<String>,
    pub host: Option<String>,
    pub port: Option<i64>,
    pub protocol: Option<String>,
    pub method: Option<String>,
    pub custom_attributes: HashMap<String, serde_json::Value>,
}

impl EvaluationContext {
    pub fn to_cedar_json_value(&self) -> serde_json::Value {
        let mut map = serde_json::Map::new();
        if let Some(ref d) = self.args_digest {
            map.insert(
                "args_digest".to_string(),
                serde_json::Value::String(d.clone()),
            );
        }
        if let Some(ref s) = self.script_digest {
            map.insert(
                "script_digest".to_string(),
                serde_json::Value::String(s.clone()),
            );
        }
        if let Some(s) = self.side_effect {
            map.insert(
                "side_effect".to_string(),
                serde_json::Value::Number(s.into()),
            );
        }
        if let Some(i) = self.iteration {
            map.insert("iteration".to_string(), serde_json::Value::Number(i.into()));
        }
        if let Some(s) = self.spend_micro_amount {
            map.insert(
                "spend_micro_amount".to_string(),
                serde_json::Value::Number(s.into()),
            );
        }
        if let Some(t) = self.token_count {
            map.insert(
                "token_count".to_string(),
                serde_json::Value::Number(t.into()),
            );
        }
        if let Some(r) = self.restored_from_snapshot {
            map.insert(
                "restored_from_snapshot".to_string(),
                serde_json::Value::Bool(r),
            );
        }
        if let Some(d) = self.session_lineage_depth {
            map.insert(
                "session_lineage_depth".to_string(),
                serde_json::Value::Number(d.into()),
            );
        }
        if let Some(g) = self.lifecycle_generation {
            map.insert(
                "lifecycle_generation".to_string(),
                serde_json::Value::Number(g.into()),
            );
        }
        if let Some(h) = self.hitl_approved {
            map.insert("hitl_approved".to_string(), serde_json::Value::Bool(h));
        }
        if let Some(ref q) = self.query {
            map.insert("query".to_string(), serde_json::Value::String(q.clone()));
        }
        if let Some(fs) = self.file_size_bytes {
            map.insert(
                "file_size_bytes".to_string(),
                serde_json::Value::Number(fs.into()),
            );
        }
        if let Some(ref p) = self.path {
            map.insert("path".to_string(), serde_json::Value::String(p.clone()));
        }
        if let Some(ref h) = self.host {
            map.insert("host".to_string(), serde_json::Value::String(h.clone()));
        }
        if let Some(p) = self.port {
            map.insert("port".to_string(), serde_json::Value::Number(p.into()));
        }
        if let Some(ref pr) = self.protocol {
            map.insert(
                "protocol".to_string(),
                serde_json::Value::String(pr.clone()),
            );
        }
        if let Some(ref m) = self.method {
            map.insert("method".to_string(), serde_json::Value::String(m.clone()));
        }
        for (k, v) in &self.custom_attributes {
            map.insert(k.clone(), v.clone());
        }
        serde_json::Value::Object(map)
    }
}

pub trait PolicyDecisionPoint: Send + Sync {
    fn evaluate(&self, req: &EvaluationRequest) -> DecisionOutcome;
    fn mode(&self) -> PolicyMode;
    fn generation(&self) -> u64;
    fn policy_bundle_digest(&self) -> String;
}
