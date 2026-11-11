//! Effective permissions, explain, and the Squrell preset catalog.
//!
//! Design: `design/agent-experience.md` §IV.6 and §IV.9. Locked decisions 18
//! and 25. The HTTP handlers live in `api/permissions.rs`.
//!
//! Effective rows are one tool the caller can see: builtins, the agent's
//! skills, and the caller's visible MCP connections. The guest name maps to
//! a target through [`targets_for_agent`]. The decision comes from
//! [`explain_tool`] (permissions) or [`classify_tools`] (legacy Cedar).
//! Explain adds no rule of its own: it returns [`explain_tool`] /
//! [`explain_host`].
//!
//! Presets are static JSON compiled into vamd (`presets/*.json`). They are
//! copied into an agent, never referenced. The three rule sets are a proposal
//! for the product owner (decision 25).

use std::collections::{BTreeMap, HashMap, HashSet};

use serde::Serialize;
use vam_security::{
    always_offered, classify_tools, explain_host, explain_tool, format_host_explain,
    format_tool_explain, resolve_internet, resolve_tool, AlwaysOffer, DecidingScope, NamedTool,
    ScopeExplanation, ScopeRule, SourceRule, ToolDecision, ToolEffect, ToolQuery,
};
use vam_spec::permissions::Target;
use vam_spec::v1::{AgentPermissions, AgentSpec, SideEffectClass, SkillManifest, Version};
use vam_spec::{mcp_server_id, validate_agent_permissions};

use crate::catalog_publish::load_agent_draft;
use crate::catalog_resolve::{resolve_ref_visible, ArtifactKind, ResolveMode};
use crate::error::OrchestratorError;
use crate::mcp_registry::{
    mcp_tool_side_effect, mcp_tool_source_rule, plan_mcp_tools, McpConnection,
};
use crate::secrets::SecretScope;
use crate::security::{
    capability_policy_urn, compose_policy, derive_hitl_overlay, monitored_tools_from_agent,
    DEFAULT_BUILTIN_FALLBACK_POLICY,
};
use crate::session::{builtin_tool_catalog, ToolCatalogEntry};
use crate::session_targets::{targets_for_agent, PlannedMcpTool, RunProvenance, SourceKind};
use crate::web_admission::agent_web_enabled;
use crate::workflow::SqliteWorkflowStore;

/// Version of the compiled preset set. Bump when a shipped rule set changes.
pub const PRESETS_VERSION: u32 = 1;

const PRESET_JSON: &[&str] = &[
    include_str!("../presets/ask-before-writes.json"),
    include_str!("../presets/open-internet.json"),
    include_str!("../presets/read-only.json"),
];

/// `GET /v1/agents/*urn/permissions/effective`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EffectivePermissions {
    pub urn: String,
    /// `draft`, or the release `X.Y.Z`.
    pub version: String,
    pub legacy: bool,
    pub tools: Vec<EffectiveTool>,
    /// Absent on a legacy agent. Legacy Cedar has no `internet:*` rules.
    pub internet: Option<EffectiveInternet>,
    /// Web search and fetch (AR-1.7): on or off for this agent.
    pub web: EffectiveWeb,
}

/// `permissions.web` as the session would see it.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EffectiveWeb {
    /// `permissions.web.enabled`. Always false for a legacy agent.
    pub enabled: bool,
    /// An enabled web broker with registered tools exists on this node.
    pub available: bool,
    /// Why web is off or unusable. Empty when it works.
    pub note: String,
}

/// One tool the caller can see, and the rule that decides it.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EffectiveTool {
    pub tool_name: String,
    pub target: String,
    /// `builtin`, `skill <name> vX`, or `mcp <server>`.
    pub source: String,
    /// `allow`, `ask`, or `deny`.
    pub decision: String,
    pub deciding_scope: String,
    pub deciding_rule: String,
    /// True when a source or org rule decides, or the agent is legacy.
    pub locked: bool,
    pub locked_reason: String,
    pub is_default: bool,
    /// Short label such as `default (read-only)`. Empty when `isDefault` is false.
    pub default_label: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EffectiveInternet {
    /// `allow` or `deny`.
    pub default: String,
    pub rules: Vec<InternetRule>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InternetRule {
    pub target: String,
    pub decision: String,
}

/// `GET /v1/agents/*urn/permissions/explain`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionExplanation {
    pub urn: String,
    pub version: String,
    /// `tool` or `host`.
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// `allow`, `ask`, or `deny`.
    pub decision: String,
    /// §IV.9 text (`ASK   …` / `DENY  …`).
    pub text: String,
    pub deciding: ExplainScope,
    pub scopes: Vec<ExplainScope>,
}

/// One scope row: `{scope, decision, rule, origin}` plus whether it decides.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExplainScope {
    pub scope: String,
    pub decision: String,
    pub rule: String,
    pub origin: String,
    pub deciding: bool,
}

/// `GET /v1/permission-presets`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionPresets {
    pub version: u32,
    pub presets: Vec<serde_json::Value>,
}

struct LoadedAgent {
    spec: AgentSpec,
    version_label: String,
    origin: String,
    resolve_owner: String,
    mode: ResolveMode,
}

struct PreparedTool {
    entry: ToolCatalogEntry,
    source: String,
    mcp: Option<PlannedMcpTool>,
}

/// Effective permissions of the owner's draft, or of `version` when set.
pub fn effective_permissions(
    store: &SqliteWorkflowStore,
    caller_user_id: &str,
    include_foreign_drafts: bool,
    urn: &str,
    version: Option<&str>,
) -> Result<EffectivePermissions, OrchestratorError> {
    let loaded = load_agent(store, caller_user_id, include_foreign_drafts, urn, version)?;
    let prepared = visible_tools(store, &loaded, caller_user_id, include_foreign_drafts)?;
    let legacy = uses_legacy_policy(&loaded.spec);
    let (tools, internet) = if legacy {
        let cedar = legacy_cedar(store, &loaded.spec, urn, &loaded.resolve_owner)?;
        (legacy_rows(urn, &loaded, &prepared, &cedar)?, None)
    } else {
        let permissions = permissions_of(&loaded.spec);
        (
            permission_rows(&loaded, &prepared, &permissions)?,
            Some(internet_view(&permissions)?),
        )
    };
    let web = effective_web(store, &loaded.spec, legacy);
    Ok(EffectivePermissions {
        urn: urn.to_string(),
        version: loaded.version_label,
        legacy,
        tools,
        internet,
        web,
    })
}

/// Explain one guest tool name, one target, or one host.
pub fn explain_permissions(
    store: &SqliteWorkflowStore,
    caller_user_id: &str,
    include_foreign_drafts: bool,
    urn: &str,
    version: Option<&str>,
    tool: Option<&str>,
    host: Option<&str>,
) -> Result<PermissionExplanation, OrchestratorError> {
    let tool = blank(tool);
    let host = blank(host);
    match (tool, host) {
        (Some(_), Some(_)) => Err(OrchestratorError::InvalidContract(
            "explain takes either tool or host".into(),
        )),
        (None, None) => Err(OrchestratorError::InvalidContract(
            "explain requires tool or host".into(),
        )),
        (None, Some(host)) => explain_one_host(
            store,
            caller_user_id,
            include_foreign_drafts,
            urn,
            version,
            host,
        ),
        (Some(tool), None) => explain_one_tool(
            store,
            caller_user_id,
            include_foreign_drafts,
            urn,
            version,
            tool,
        ),
    }
}

/// Squrell presets, copied into an agent. The rule sets are proposed.
pub fn permission_presets() -> Result<PermissionPresets, OrchestratorError> {
    let mut presets = Vec::with_capacity(PRESET_JSON.len());
    for raw in PRESET_JSON {
        let value: serde_json::Value = serde_json::from_str(raw).map_err(|err| {
            OrchestratorError::CorruptState(format!("permission preset JSON: {err}"))
        })?;
        let version = value
            .get("version")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| {
                OrchestratorError::CorruptState("permission preset has no version".into())
            })?;
        if version != u64::from(PRESETS_VERSION) {
            return Err(OrchestratorError::CorruptState(format!(
                "permission preset version {version} does not match set {PRESETS_VERSION}"
            )));
        }
        let permissions = value.get("permissions").ok_or_else(|| {
            OrchestratorError::CorruptState("permission preset has no permissions".into())
        })?;
        let parsed: AgentPermissions =
            vam_spec::encoding::json_to_proto("vam.v1.AgentPermissions", &permissions.to_string())
                .map_err(OrchestratorError::CorruptState)?;
        validate_agent_permissions(&parsed).map_err(|err| {
            OrchestratorError::CorruptState(format!("permission preset is invalid: {err}"))
        })?;
        presets.push(value);
    }
    Ok(PermissionPresets {
        version: PRESETS_VERSION,
        presets,
    })
}

fn effective_web(store: &SqliteWorkflowStore, spec: &AgentSpec, legacy: bool) -> EffectiveWeb {
    let enabled = !legacy && agent_web_enabled(spec);
    let available = store.web_broker_available();
    let note = if legacy {
        "legacy policy: convert the agent to permissions to use web access"
    } else if enabled && !available {
        "web is on, but no web broker is registered on this node"
    } else {
        ""
    };
    EffectiveWeb {
        enabled,
        available,
        note: note.to_string(),
    }
}

fn explain_one_host(
    store: &SqliteWorkflowStore,
    caller: &str,
    include_foreign: bool,
    urn: &str,
    version: Option<&str>,
    host: &str,
) -> Result<PermissionExplanation, OrchestratorError> {
    let loaded = load_agent(store, caller, include_foreign, urn, version)?;
    if uses_legacy_policy(&loaded.spec) {
        return Err(OrchestratorError::Conflict(format!(
            "agent '{urn}' uses a legacy policy; host explain applies to permissions"
        )));
    }
    let permissions = permissions_of(&loaded.spec);
    let explained = explain_host(&permissions, host, &loaded.origin)
        .map_err(|err| OrchestratorError::InvalidContract(err.to_string()))?;
    let decision = explained.decision.as_str().to_string();
    let row = ExplainScope {
        scope: "agent".into(),
        decision: decision.clone(),
        rule: explained.deciding_rule.clone(),
        origin: explained.origin.clone(),
        deciding: true,
    };
    Ok(PermissionExplanation {
        urn: urn.to_string(),
        version: loaded.version_label,
        kind: "host".into(),
        tool_name: None,
        target: None,
        host: Some(explained.host.clone()),
        decision,
        text: format_host_explain(&explained),
        deciding: row.clone(),
        scopes: vec![row],
    })
}

fn explain_one_tool(
    store: &SqliteWorkflowStore,
    caller: &str,
    include_foreign: bool,
    urn: &str,
    version: Option<&str>,
    query: &str,
) -> Result<PermissionExplanation, OrchestratorError> {
    let loaded = load_agent(store, caller, include_foreign, urn, version)?;
    let prepared = visible_tools(store, &loaded, caller, include_foreign)?;
    let legacy = uses_legacy_policy(&loaded.spec);
    let permissions = (!legacy).then(|| permissions_of(&loaded.spec));
    let catalog = catalog_of(&prepared);
    let plan = prepared_mcp(&prepared);
    let provenance = provenance_for(&loaded, caller);
    let (rows, _) = targets_for_agent(permissions.as_ref(), &provenance, &catalog, &plan);
    let Some(row) = rows
        .iter()
        .find(|row| row.tool_name == query || row.target == query)
    else {
        if let Some(off) = web_off_explanation(store, &loaded, urn, query) {
            return Ok(off);
        }
        return Err(OrchestratorError::NotFound(format!(
            "tool '{query}' is not visible on agent '{urn}'"
        )));
    };
    let source = source_of(&prepared, &row.tool_name);
    if legacy {
        return legacy_tool_explanation(store, &loaded, urn, row, &source);
    }
    let permissions = permissions.unwrap_or_default();
    permission_tool_explanation(&loaded, &permissions, row, &source, urn)
}

/// A web broker's tool asked about while the agent has web off: the tool is
/// not in the session, so say that instead of "not visible".
fn web_off_explanation(
    store: &SqliteWorkflowStore,
    loaded: &LoadedAgent,
    urn: &str,
    query: &str,
) -> Option<PermissionExplanation> {
    let name = query.strip_prefix("builtin:").unwrap_or(query);
    let entries = store.list_hsp_catalog_entries(true).ok()?;
    let entry = entries.iter().find(|entry| {
        entry.tool_name == name
            && vam_spec::validation::parse_hosted_adapter_id(&entry.entrypoint)
                .is_some_and(|(broker, _)| store.hsp_broker_is_web(&broker))
    })?;
    let rule = "web.enabled=false".to_string();
    let row = ExplainScope {
        scope: "agent".into(),
        decision: "deny".into(),
        rule: rule.clone(),
        origin: loaded.origin.clone(),
        deciding: true,
    };
    Some(PermissionExplanation {
        urn: urn.to_string(),
        version: loaded.version_label.clone(),
        kind: "tool".into(),
        tool_name: Some(entry.tool_name.clone()),
        target: Some(format!("builtin:{}", entry.tool_name)),
        host: None,
        decision: "deny".into(),
        text: format!(
            "DENY  {}  (web access is off for this agent: `{rule}`)",
            entry.tool_name
        ),
        deciding: row.clone(),
        scopes: vec![row],
    })
}

fn permission_tool_explanation(
    loaded: &LoadedAgent,
    permissions: &AgentPermissions,
    row: &crate::session_targets::SessionToolTarget,
    source: &str,
    urn: &str,
) -> Result<PermissionExplanation, OrchestratorError> {
    let target = Target::parse(&row.target)
        .map_err(|err| OrchestratorError::InvalidContract(err.to_string()))?;
    let source_rule = source_rule_for(row, source);
    let query = ToolQuery {
        name: &row.tool_name,
        target: &target,
        side_effect: side_effect_class(row.side_effect),
        source: source_rule.as_ref(),
        org: None,
    };
    let explained = explain_tool(permissions, &loaded.origin, query)
        .map_err(|err| OrchestratorError::InvalidContract(err.to_string()))?;
    Ok(explanation_from_tool(
        &loaded.version_label,
        urn,
        &explained,
    ))
}

fn legacy_tool_explanation(
    store: &SqliteWorkflowStore,
    loaded: &LoadedAgent,
    urn: &str,
    row: &crate::session_targets::SessionToolTarget,
    source: &str,
) -> Result<PermissionExplanation, OrchestratorError> {
    let cedar = legacy_cedar(store, &loaded.spec, urn, &loaded.resolve_owner)?;
    let named = [NamedTool {
        name: row.tool_name.clone(),
        side_effect: Some(row.side_effect),
    }];
    let decision = classify_tools(&cedar, urn, &named)?
        .into_iter()
        .next()
        .ok_or_else(|| {
            OrchestratorError::CorruptState("legacy policy returned no tool decision".into())
        })?;
    let word = effect_word(decision.effect).to_string();
    let scope = ExplainScope {
        scope: "legacy policy".into(),
        decision: word.clone(),
        rule: decision.deciding_rule.clone(),
        origin: "legacy policy".into(),
        deciding: true,
    };
    let _ = source;
    Ok(PermissionExplanation {
        urn: urn.to_string(),
        version: loaded.version_label.clone(),
        kind: "tool".into(),
        tool_name: Some(row.tool_name.clone()),
        target: Some(row.target.clone()),
        host: None,
        decision: word,
        text: format!(
            "{:<6}{}  ({})\n  legacy policy  {}  {}  (legacy policy)     <- decides",
            effect_heading(decision.effect),
            row.tool_name,
            row.target,
            scope.decision,
            scope.rule
        ),
        deciding: scope.clone(),
        scopes: vec![scope],
    })
}

fn explanation_from_tool(
    version: &str,
    urn: &str,
    decision: &ToolDecision,
) -> PermissionExplanation {
    let scopes: Vec<ExplainScope> = decision
        .scopes
        .iter()
        .map(|row| explain_scope(row, decision.deciding_scope))
        .collect();
    let deciding = scopes
        .iter()
        .find(|row| row.deciding)
        .cloned()
        .unwrap_or_else(|| ExplainScope {
            scope: decision.deciding_scope.as_str().to_string(),
            decision: effect_word(decision.effect).to_string(),
            rule: decision.deciding_rule.clone(),
            origin: String::new(),
            deciding: true,
        });
    PermissionExplanation {
        urn: urn.to_string(),
        version: version.to_string(),
        kind: "tool".into(),
        tool_name: Some(decision.name.clone()),
        target: Some(decision.target.clone()),
        host: None,
        decision: effect_word(decision.effect).to_string(),
        text: format_tool_explain(decision),
        deciding,
        scopes,
    }
}

fn explain_scope(row: &ScopeExplanation, deciding: DecidingScope) -> ExplainScope {
    ExplainScope {
        scope: row.scope.as_str().to_string(),
        decision: row.decision.as_str().to_string(),
        rule: row.rule.clone(),
        origin: row.origin.clone(),
        deciding: row.scope == deciding,
    }
}

fn permission_rows(
    loaded: &LoadedAgent,
    prepared: &[PreparedTool],
    permissions: &AgentPermissions,
) -> Result<Vec<EffectiveTool>, OrchestratorError> {
    let catalog = catalog_of(prepared);
    let plan = prepared_mcp(prepared);
    let provenance = provenance_for(loaded, &loaded.resolve_owner);
    let (rows, _) = targets_for_agent(Some(permissions), &provenance, &catalog, &plan);
    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        let source = source_of(prepared, &row.tool_name);
        let target = Target::parse(&row.target)
            .map_err(|err| OrchestratorError::InvalidContract(err.to_string()))?;
        let source_rule = source_rule_for(row, &source);
        let query = ToolQuery {
            name: &row.tool_name,
            target: &target,
            side_effect: side_effect_class(row.side_effect),
            source: source_rule.as_ref(),
            org: None,
        };
        let scoped = resolve_tool(permissions, &loaded.origin, query)
            .map_err(|err| OrchestratorError::InvalidContract(err.to_string()))?;
        let explained = explain_tool(permissions, &loaded.origin, query)
            .map_err(|err| OrchestratorError::InvalidContract(err.to_string()))?;
        let (locked, locked_reason) = always_locked(&scoped);
        let (is_default, default_label) = default_marker(&explained.deciding_rule);
        out.push(EffectiveTool {
            tool_name: row.tool_name.clone(),
            target: row.target.clone(),
            source,
            decision: effect_word(explained.effect).to_string(),
            deciding_scope: explained.deciding_scope.as_str().to_string(),
            deciding_rule: explained.deciding_rule.clone(),
            locked,
            locked_reason,
            is_default,
            default_label,
        });
    }
    Ok(out)
}

fn legacy_rows(
    urn: &str,
    loaded: &LoadedAgent,
    prepared: &[PreparedTool],
    cedar: &str,
) -> Result<Vec<EffectiveTool>, OrchestratorError> {
    let catalog = catalog_of(prepared);
    let plan = prepared_mcp(prepared);
    let provenance = provenance_for(loaded, &loaded.resolve_owner);
    let (rows, _) = targets_for_agent(None, &provenance, &catalog, &plan);
    let targets: HashMap<String, String> = rows
        .into_iter()
        .map(|row| (row.tool_name, row.target))
        .collect();
    let named: Vec<NamedTool> = catalog
        .iter()
        .map(|entry| NamedTool {
            name: entry.tool_name.clone(),
            side_effect: Some(i64::from(entry.side_effect)),
        })
        .collect();
    let decisions = classify_tools(cedar, urn, &named)?;
    let mut out = Vec::with_capacity(decisions.len());
    for decision in decisions {
        let target = targets.get(&decision.name).cloned().unwrap_or_default();
        out.push(EffectiveTool {
            source: source_of(prepared, &decision.name),
            tool_name: decision.name,
            target,
            decision: effect_word(decision.effect).to_string(),
            deciding_scope: "legacy policy".into(),
            deciding_rule: decision.deciding_rule,
            locked: true,
            locked_reason: "legacy policy".into(),
            is_default: false,
            default_label: String::new(),
        });
    }
    Ok(out)
}

/// Rebuild the MCP plan from the prepared rows. The catalog and the plan
/// must name the same guest tools, which they do: both come from `visible_tools`.
fn prepared_mcp(prepared: &[PreparedTool]) -> Vec<PlannedMcpTool> {
    prepared
        .iter()
        .filter_map(|tool| tool.mcp.clone())
        .collect()
}

fn internet_view(permissions: &AgentPermissions) -> Result<EffectiveInternet, OrchestratorError> {
    let resolved = resolve_internet(permissions)
        .map_err(|err| OrchestratorError::InvalidContract(err.to_string()))?;
    Ok(EffectiveInternet {
        default: resolved.default_decision.to_string(),
        rules: resolved
            .rules
            .into_iter()
            .map(|(target, decision)| InternetRule { target, decision })
            .collect(),
    })
}

fn load_agent(
    store: &SqliteWorkflowStore,
    caller: &str,
    include_foreign: bool,
    urn: &str,
    version: Option<&str>,
) -> Result<LoadedAgent, OrchestratorError> {
    match blank(version) {
        Some(raw) => {
            let version = parse_version(raw)?;
            let spec =
                store.get_agent_at_version_visible(urn, &version, caller, include_foreign)?;
            let version_label = version_label(&version);
            Ok(LoadedAgent {
                spec,
                origin: format!("agent v{version_label}"),
                version_label,
                resolve_owner: caller.to_string(),
                mode: ResolveMode::Release,
            })
        }
        None => {
            let spec = load_agent_draft(store, urn, caller, include_foreign)?;
            Ok(LoadedAgent {
                spec,
                version_label: "draft".into(),
                origin: "agent draft".into(),
                resolve_owner: draft_owner(store, urn, caller, include_foreign)?,
                mode: ResolveMode::Draft,
            })
        }
    }
}

fn draft_owner(
    store: &SqliteWorkflowStore,
    urn: &str,
    caller: &str,
    include_foreign: bool,
) -> Result<String, OrchestratorError> {
    if !include_foreign {
        return Ok(caller.to_string());
    }
    let records = store.list_agent_records_visible(caller, true)?;
    for record in records {
        let matches = record
            .spec
            .as_ref()
            .and_then(|spec| spec.resource_id.as_ref())
            .is_some_and(|id| id.urn == urn);
        let is_draft = record
            .version
            .as_ref()
            .is_some_and(|version| version.prerelease == "draft");
        if matches && is_draft {
            return Ok(record.owner_user_id);
        }
    }
    Ok(caller.to_string())
}

fn visible_tools(
    store: &SqliteWorkflowStore,
    loaded: &LoadedAgent,
    caller: &str,
    include_foreign: bool,
) -> Result<Vec<PreparedTool>, OrchestratorError> {
    let mut tools = Vec::new();
    let mut seen = HashSet::new();
    for entry in builtin_tool_catalog() {
        push_tool(&mut tools, &mut seen, entry, "builtin".into(), None)?;
    }
    append_skill_tools(
        store,
        &loaded.spec,
        &loaded.resolve_owner,
        loaded.mode,
        include_foreign,
        &mut tools,
        &mut seen,
    )?;
    for planned in visible_mcp_plan(store, caller)? {
        let server = mcp_server_id(&planned.server_url)
            .map_err(|err| OrchestratorError::InvalidContract(err.to_string()))?;
        let entry = ToolCatalogEntry {
            tool_name: planned.guest_name.clone(),
            command: "host".into(),
            entrypoint: format!("mcp:{}:{}", planned.connection_id, planned.vendor_tool),
            digest: String::new(),
            side_effect: planned.side_effect,
            skill_name: String::new(),
            approval_required: false,
        };
        push_tool(
            &mut tools,
            &mut seen,
            entry,
            format!("mcp {server}"),
            Some(planned),
        )?;
    }
    // Hosted (HSP) tools are in every session's catalog. A web broker's tools
    // only when the agent has web on (AR-1.7); the same filter as session spawn.
    let web_enabled = !uses_legacy_policy(&loaded.spec) && agent_web_enabled(&loaded.spec);
    for entry in store.list_hsp_catalog_entries(web_enabled)? {
        let Some((broker, _)) = vam_spec::validation::parse_hosted_adapter_id(&entry.entrypoint)
        else {
            continue;
        };
        if seen.contains(&entry.tool_name) {
            continue;
        }
        push_tool(
            &mut tools,
            &mut seen,
            entry,
            format!("hosted {broker}"),
            None,
        )?;
    }
    Ok(tools)
}

fn append_skill_tools(
    store: &SqliteWorkflowStore,
    agent: &AgentSpec,
    resolve_owner: &str,
    mode: ResolveMode,
    include_foreign: bool,
    tools: &mut Vec<PreparedTool>,
    seen: &mut HashSet<String>,
) -> Result<(), OrchestratorError> {
    for skill_ref in &agent.skill_refs {
        let Some(urn) = skill_ref
            .resource_id
            .as_ref()
            .map(|id| id.urn.as_str())
            .filter(|urn| !urn.is_empty())
        else {
            continue;
        };
        let pinned = match resolve_ref_visible(
            store,
            ArtifactKind::Skill,
            skill_ref,
            resolve_owner,
            mode,
            include_foreign,
        ) {
            Ok(pinned) => pinned,
            Err(OrchestratorError::NotFound(_)) => continue,
            Err(err) => return Err(err),
        };
        let Some(version) = pinned.reference.version.as_ref() else {
            continue;
        };
        let manifest = match store.get_skill_manifest_any(
            urn,
            Some(version),
            resolve_owner,
            include_foreign,
        ) {
            Ok(manifest) => manifest,
            Err(OrchestratorError::NotFound(_)) => continue,
            Err(err) => return Err(err),
        };
        let name = skill_name_of(urn);
        let source = format!("skill {name} v{}", version_label(version));
        for entry in skill_tools(&manifest, &name) {
            push_tool(tools, seen, entry, source.clone(), None)?;
        }
    }
    Ok(())
}

fn skill_tools(manifest: &SkillManifest, skill_name: &str) -> Vec<ToolCatalogEntry> {
    let mut out = Vec::new();
    for tool in &manifest.tools {
        if tool.name.is_empty() || vam_spec::validation::is_reserved_builtin_name(&tool.name) {
            continue;
        }
        let (command, entrypoint) = match &tool.execution_provider {
            Some(vam_spec::v1::tool_spec::ExecutionProvider::LocalExec(local)) => {
                (local.command.clone(), local.entrypoint.clone())
            }
            Some(vam_spec::v1::tool_spec::ExecutionProvider::HostExec(host)) => {
                if host.adapter_id.trim().is_empty() {
                    continue;
                }
                ("host".into(), host.adapter_id.clone())
            }
            Some(vam_spec::v1::tool_spec::ExecutionProvider::Builtin(_)) => {
                ("builtin".into(), tool.name.clone())
            }
            Some(vam_spec::v1::tool_spec::ExecutionProvider::McpServer(_)) => {
                ("mcp".into(), tool.name.clone())
            }
            None => continue,
        };
        out.push(ToolCatalogEntry {
            tool_name: tool.name.clone(),
            command,
            entrypoint,
            digest: String::new(),
            side_effect: tool.side_effect,
            skill_name: skill_name.to_string(),
            approval_required: tool.approval_required,
        });
    }
    out
}

fn visible_mcp_plan(
    store: &SqliteWorkflowStore,
    caller: &str,
) -> Result<Vec<PlannedMcpTool>, OrchestratorError> {
    let mut chosen: BTreeMap<String, McpConnection> = BTreeMap::new();
    for conn in store.list_visible_mcp_connections(caller)? {
        if conn.tools.is_empty() {
            continue;
        }
        match chosen.get(&conn.connection_id) {
            Some(existing) if existing.scope == SecretScope::User => {}
            _ => {
                chosen.insert(conn.connection_id.clone(), conn);
            }
        }
    }
    let mut planned = Vec::new();
    let mut used: BTreeMap<String, (String, String)> = BTreeMap::new();
    for conn in chosen.into_values() {
        let tools = plan_mcp_tools(&conn.connection_id, &conn.tools)?;
        for (tool, guest) in tools {
            if let Some((other_id, other_vendor)) = used.get(&guest) {
                return Err(OrchestratorError::InvalidContract(format!(
                    "namespaced mcp tool '{}' on connection '{}' collides with '{other_vendor}' on connection '{other_id}' (both become '{guest}')",
                    tool.name, conn.connection_id
                )));
            }
            used.insert(
                guest.clone(),
                (conn.connection_id.clone(), tool.name.clone()),
            );
            planned.push(PlannedMcpTool {
                connection_id: conn.connection_id.clone(),
                server_url: conn.url.clone(),
                vendor_tool: tool.name.clone(),
                guest_name: guest,
                side_effect: mcp_tool_side_effect(&conn, &tool.name),
                ask: mcp_tool_source_rule(&conn, &tool.name).is_some(),
            });
        }
    }
    Ok(planned)
}

fn push_tool(
    tools: &mut Vec<PreparedTool>,
    seen: &mut HashSet<String>,
    entry: ToolCatalogEntry,
    source: String,
    mcp: Option<PlannedMcpTool>,
) -> Result<(), OrchestratorError> {
    if !seen.insert(entry.tool_name.clone()) {
        return Err(OrchestratorError::Conflict(format!(
            "tool '{}' is declared more than once",
            entry.tool_name
        )));
    }
    tools.push(PreparedTool { entry, source, mcp });
    Ok(())
}

fn catalog_of(prepared: &[PreparedTool]) -> Vec<ToolCatalogEntry> {
    prepared.iter().map(|tool| tool.entry.clone()).collect()
}

fn source_of(prepared: &[PreparedTool], tool_name: &str) -> String {
    prepared
        .iter()
        .find(|tool| tool.entry.tool_name == tool_name)
        .map(|tool| tool.source.clone())
        .unwrap_or_default()
}

fn source_rule_for(
    row: &crate::session_targets::SessionToolTarget,
    source: &str,
) -> Option<SourceRule> {
    if row.source_rule != ScopeRule::Ask {
        return None;
    }
    let rule = match row.source_kind {
        SourceKind::Skill => "approvalRequired",
        SourceKind::Mcp => "askTools",
        SourceKind::Builtin | SourceKind::Host => "ask",
    };
    Some(SourceRule::ask(rule, source))
}

fn provenance_for(loaded: &LoadedAgent, caller: &str) -> RunProvenance {
    RunProvenance {
        agent_origin: loaded.origin.clone(),
        owner_draft_run: loaded.mode == ResolveMode::Draft && loaded.resolve_owner == caller,
    }
}

fn uses_legacy_policy(spec: &AgentSpec) -> bool {
    spec.permissions.is_none() && (spec.capability_policy.is_some() || spec.hitl_policy.is_some())
}

fn permissions_of(spec: &AgentSpec) -> AgentPermissions {
    spec.permissions.clone().unwrap_or_default()
}

fn legacy_cedar(
    store: &SqliteWorkflowStore,
    agent: &AgentSpec,
    urn: &str,
    owner: &str,
) -> Result<String, OrchestratorError> {
    let base = match capability_policy_urn(agent) {
        Some(policy_urn) => load_legacy_policy(store, agent, &policy_urn, owner)?,
        None => DEFAULT_BUILTIN_FALLBACK_POLICY.to_string(),
    };
    let monitored = monitored_tools_from_agent(agent);
    let overlay = if monitored.is_empty() {
        String::new()
    } else {
        derive_hitl_overlay(urn, &monitored)
    };
    Ok(compose_policy(&base, &overlay))
}

fn load_legacy_policy(
    store: &SqliteWorkflowStore,
    agent: &AgentSpec,
    policy_urn: &str,
    owner: &str,
) -> Result<String, OrchestratorError> {
    let pinned = agent
        .capability_policy
        .as_ref()
        .and_then(|policy| policy.policy_ref.as_ref())
        .and_then(|reference| reference.version.as_ref());
    if let Some(version) = pinned {
        match store.get_policy_at_version_visible(policy_urn, version, owner) {
            Ok(record) => return Ok(record.cedar),
            Err(OrchestratorError::NotFound(_)) => {}
            Err(err) => return Err(err),
        }
    }
    match store.get_policy_visible(policy_urn, owner) {
        Ok(record) => Ok(record.cedar),
        Err(OrchestratorError::NotFound(_)) => match store.get_policy(policy_urn) {
            Ok(record) => Ok(record.cedar),
            Err(OrchestratorError::NotFound(_)) => Ok(DEFAULT_BUILTIN_FALLBACK_POLICY.to_string()),
            Err(err) => Err(err),
        },
        Err(err) => Err(err),
    }
}

fn always_locked(scoped: &vam_security::ScopedTool) -> (bool, String) {
    match always_offered(scoped) {
        AlwaysOffer::Blocked { reason } => (true, reason),
        AlwaysOffer::Offered => (false, String::new()),
    }
}

/// `tools:* default bySideEffect (read-only)` becomes `default (read-only)`.
fn default_marker(rule: &str) -> (bool, String) {
    match default_label(rule) {
        Some(label) => (true, label),
        None => (false, String::new()),
    }
}

fn default_label(rule: &str) -> Option<String> {
    let rest = rule.strip_prefix("tools:* default ")?;
    if let Some(phrase) = rest
        .strip_prefix("bySideEffect (")
        .and_then(|text| text.strip_suffix(')'))
    {
        if phrase.is_empty() {
            return None;
        }
        return Some(format!("default ({phrase})"));
    }
    match rest {
        "allow" | "ask" | "deny" => Some(format!("default ({rest})")),
        _ => None,
    }
}

fn side_effect_class(value: i64) -> Option<SideEffectClass> {
    let value = i32::try_from(value).ok()?;
    SideEffectClass::try_from(value).ok()
}

fn effect_word(effect: ToolEffect) -> &'static str {
    match effect {
        ToolEffect::Allow => "allow",
        ToolEffect::Hitl => "ask",
        ToolEffect::Deny => "deny",
    }
}

fn effect_heading(effect: ToolEffect) -> &'static str {
    match effect {
        ToolEffect::Allow => "ALLOW",
        ToolEffect::Hitl => "ASK",
        ToolEffect::Deny => "DENY",
    }
}

fn skill_name_of(urn: &str) -> String {
    vam_spec::parse_skill_urn(urn)
        .map(|(_, _, name)| name)
        .unwrap_or_else(|_| urn.rsplit('/').next().unwrap_or("").to_string())
}

fn version_label(version: &Version) -> String {
    if version.prerelease.is_empty() {
        format!("{}.{}.{}", version.major, version.minor, version.patch)
    } else {
        format!(
            "{}.{}.{}-{}",
            version.major, version.minor, version.patch, version.prerelease
        )
    }
}

fn parse_version(raw: &str) -> Result<Version, OrchestratorError> {
    let parts: Vec<&str> = raw.split('.').collect();
    if parts.len() != 3 {
        return Err(OrchestratorError::InvalidContract(format!(
            "version must be X.Y.Z, got '{raw}'"
        )));
    }
    let parse = |part: &str, name: &str| {
        part.parse::<i32>().map_err(|_| {
            OrchestratorError::InvalidContract(format!("invalid version {name} in '{raw}'"))
        })
    };
    let patch_text = parts[2];
    if patch_text.contains('-') {
        return Err(OrchestratorError::InvalidContract(format!(
            "version must be X.Y.Z, got '{raw}'"
        )));
    }
    Ok(Version {
        major: parse(parts[0], "major")?,
        minor: parse(parts[1], "minor")?,
        patch: parse(patch_text, "patch")?,
        prerelease: String::new(),
        build: String::new(),
    })
}

fn blank(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_labels_match_the_resolver_phrases() {
        assert_eq!(
            default_label("tools:* default bySideEffect (read-only)").as_deref(),
            Some("default (read-only)")
        );
        assert_eq!(
            default_label("tools:* default bySideEffect (mutable external)").as_deref(),
            Some("default (mutable external)")
        );
        assert_eq!(
            default_label("tools:* default bySideEffect (mutable local)").as_deref(),
            Some("default (mutable local)")
        );
        assert_eq!(
            default_label("tools:* default allow").as_deref(),
            Some("default (allow)")
        );
        assert_eq!(default_label("skill:jira/*"), None);
        assert_eq!(default_label("approvalRequired"), None);
    }

    #[test]
    fn presets_are_versioned_and_ask_before_writes_is_the_defaults() {
        let presets = permission_presets().expect("presets");
        assert_eq!(presets.version, 1);
        assert_eq!(presets.presets.len(), 3);
        let ids: Vec<&str> = presets
            .presets
            .iter()
            .filter_map(|preset| preset["id"].as_str())
            .collect();
        assert_eq!(ids, vec!["ask-before-writes", "open-internet", "read-only"]);
        let ask = &presets.presets[0]["permissions"];
        assert_eq!(ask["tools"]["default"], "bySideEffect");
        assert_eq!(ask["internet"]["default"], "allow");
        assert!(
            ask["tools"].get("rules").is_none()
                || ask["tools"]["rules"]
                    .as_array()
                    .is_some_and(|r| r.is_empty())
        );
        assert_eq!(
            presets.presets[1]["permissions"]["tools"]["default"],
            "allow"
        );
        assert_eq!(
            presets.presets[1]["permissions"]["internet"]["default"],
            "allow"
        );
        assert_eq!(
            presets.presets[2]["permissions"]["tools"]["default"],
            "deny"
        );
        assert_eq!(
            presets.presets[2]["permissions"]["internet"]["default"],
            "deny"
        );
        assert_eq!(
            presets.presets[2]["permissions"]["tools"]["rules"][0]["target"],
            "builtin:read_file"
        );
    }
}
