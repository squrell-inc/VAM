//! Migrate an owner's legacy draft from Cedar and `monitored_tools` into
//! `AgentPermissions`.
//!
//! `monitored_tools` become `ask` rules. A capability policy becomes rules
//! when every statement is a known tool-list shape (see
//! [`vam_security::convert_legacy_capability`]); anything else keeps the
//! policy ref on `permissions.advanced`. Known tool statements are still
//! written as rules in that case: the platform policy's forbids would
//! otherwise override permits in the attachment, and the attachment still
//! enforces conditions a rule cannot (the sqlite `query_db` guard).
//!
//! Internet keeps the legacy session's reach: `internet.default = deny`
//! plus `allow` rules for the destinations of the secrets this agent's
//! skills require, resolved the way a session resolves them. The report
//! suggests "Any site" so the owner can open that from the permissions page.
//!
//! Released rows are not rewritten. A guest tool name that maps to no target
//! is listed in `unmapped` and dropped only when `force` is set; otherwise
//! the draft is left unchanged and the call is a conflict.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;
use vam_security::{convert_legacy_capability, ScopeRule};
use vam_spec::v1::{
    AgentPermissions, AgentSpec, HostDefault, InternetPermissions, PermissionDecision,
    PermissionRule, ResourceRef, SkillManifest, ToolDefault, ToolPermissions, ToolSpec,
};
use vam_spec::Target;

use crate::catalog_publish::{load_agent_draft, store_migrated_permissions};
use crate::error::OrchestratorError;
use crate::mcp_registry::{mcp_tool_side_effect, plan_mcp_tools, McpConnection};
use crate::secret_requirements::{is_requirement_satisfied, RequirementScope};
use crate::secrets::{SecretScope, SecretsVault};
use crate::security::DEFAULT_BUILTIN_FALLBACK_POLICY;
use crate::session::{builtin_tool_catalog, merge_tool_catalog, ToolCatalogEntry};
use crate::session_targets::{targets_for_agent, PlannedMcpTool, RunProvenance};
use crate::workflow::SqliteWorkflowStore;

/// Shown on every migration so the owner can opt into the new internet default.
pub const ANY_SITE_BEHAVIOUR_CHANGE: &str = "Internet is limited to the sites this agent's secrets already allowed. Choose \"Any site\" on the permissions page to allow every site.";

const KEPT_ADVANCED_BEHAVIOUR_CHANGE: &str =
    "A Cedar statement was not a tool-list rule, so the policy stays attached as advanced Cedar.";

/// `POST /v1/agents/*urn/permissions/migrate` body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MigrationReport {
    /// Canonical targets that received a tool rule.
    pub converted: Vec<String>,
    /// Capability-policy URNs kept on `permissions.advanced`.
    pub kept_advanced: Vec<String>,
    /// Guest tool names that mapped to no target. Dropped only when forced.
    pub unmapped: Vec<String>,
    /// Meaning that the rules do not carry by themselves.
    pub behaviour_changes: Vec<String>,
}

/// Convert `urn`'s draft for `owner` and store it.
///
/// A missing draft and someone else's draft are both NotFound. A draft that
/// is already on `permissions` is a conflict. Unmapped tool names are a
/// conflict unless `force` is set.
pub fn migrate_legacy_draft(
    store: &SqliteWorkflowStore,
    vault: &SecretsVault,
    urn: &str,
    owner: &str,
    force: bool,
) -> Result<MigrationReport, OrchestratorError> {
    let spec = load_agent_draft(store, urn, owner, false)?;
    if spec.capability_policy.is_none() && spec.hitl_policy.is_none() {
        return Err(OrchestratorError::Conflict(format!(
            "agent '{urn}' does not use capability_policy or hitl_policy"
        )));
    }
    let agent_urn = agent_urn_of(&spec)?;
    let (catalog, mcp) = tool_catalog(store, &spec, owner)?;
    let (rows, _) = targets_for_agent(
        None,
        &RunProvenance {
            agent_origin: "agent draft".to_string(),
            owner_draft_run: true,
        },
        &catalog,
        &mcp,
    );
    let mut name_to_target = BTreeMap::new();
    for row in &rows {
        name_to_target.insert(row.tool_name.clone(), row.target.clone());
    }

    let loaded = load_capability_cedar(store, &spec, owner)?;
    let converted = convert_legacy_capability(&loaded.cedar, agent_urn).map_err(|err| {
        OrchestratorError::InvalidContract(format!(
            "capability policy for agent '{agent_urn}' did not parse: {err}"
        ))
    })?;

    let monitored = spec
        .hitl_policy
        .as_ref()
        .map(|hitl| hitl.monitored_tools.clone())
        .unwrap_or_default();

    let mut decisions = converted.tools.clone();
    let mut unmapped = BTreeSet::new();
    for name in converted.tools.keys() {
        if !name_to_target.contains_key(name) {
            unmapped.insert(name.clone());
        }
    }
    for name in &monitored {
        if !name_to_target.contains_key(name) {
            unmapped.insert(name.clone());
            continue;
        }
        // The HITL overlay forbids the tool unless it was approved, which
        // turns an allow into ask and leaves a deny as deny.
        decisions
            .entry(name.clone())
            .and_modify(|decision| {
                if *decision != ScopeRule::Deny {
                    *decision = ScopeRule::Ask;
                }
            })
            .or_insert(ScopeRule::Ask);
    }
    if !unmapped.is_empty() && !force {
        let listed = unmapped.into_iter().collect::<Vec<_>>().join(", ");
        return Err(OrchestratorError::Conflict(format!(
            "cannot migrate agent '{urn}': unmapped tools {listed}; retry with ?force=true to drop them"
        )));
    }

    let mut permissions = AgentPermissions {
        tools: Some(ToolPermissions {
            default: ToolDefault::Deny as i32,
            rules: Vec::new(),
        }),
        internet: Some(InternetPermissions {
            default: HostDefault::Deny as i32,
            rules: Vec::new(),
        }),
        advanced: Vec::new(),
        web: Default::default(),
    };

    let mut by_target: BTreeMap<String, ScopeRule> = BTreeMap::new();
    for (name, decision) in &decisions {
        if unmapped.contains(name) {
            continue;
        }
        let Some(target) = name_to_target.get(name) else {
            continue;
        };
        by_target
            .entry(target.clone())
            .and_modify(|current| *current = stricter_rule(*current, *decision))
            .or_insert(*decision);
    }
    let mut converted_targets = Vec::new();
    for (target, decision) in by_target {
        crate::catalog_publish::upsert_permission_rule(
            &mut permissions,
            PermissionRule {
                target: target.clone(),
                decision: permission_decision(decision) as i32,
            },
        );
        converted_targets.push(target);
    }
    converted_targets.sort();

    let mut behaviour_changes = vec![ANY_SITE_BEHAVIOUR_CHANGE.to_string()];
    let mut kept_advanced = Vec::new();
    if !converted.fully_recognized {
        if let Some(policy_ref) = loaded.policy_ref.clone() {
            if let Some(urn) = ref_urn(&policy_ref) {
                kept_advanced.push(urn.to_string());
                permissions.advanced.push(policy_ref);
            }
        }
        behaviour_changes.push(KEPT_ADVANCED_BEHAVIOUR_CHANGE.to_string());
    }
    if let Some(missing) = loaded.missing_urn.as_deref() {
        behaviour_changes.push(format!(
            "Declared capability policy '{missing}' is missing from the catalog; builtins were converted instead."
        ));
    }

    let skills = load_skills(store, &spec, owner)?;
    let destinations = internet_destinations(vault, owner, &skills)?;
    for destination in destinations {
        match Target::parse(&destination) {
            Ok(target) if target.is_host() && !matches!(target, Target::InternetAll) => {
                crate::catalog_publish::upsert_permission_rule(
                    &mut permissions,
                    PermissionRule {
                        target: target.to_string(),
                        decision: PermissionDecision::Allow as i32,
                    },
                );
            }
            _ => behaviour_changes.push(format!(
                "Secret destination '{destination}' is not a host rule and was not copied. Choose \"Any site\" to allow every site."
            )),
        }
    }

    store_migrated_permissions(store, urn, owner, permissions)?;

    Ok(MigrationReport {
        converted: converted_targets,
        kept_advanced,
        unmapped: unmapped.into_iter().collect(),
        behaviour_changes,
    })
}

struct LoadedCapability {
    cedar: String,
    /// The draft's policy ref, present only when its Cedar was loaded.
    policy_ref: Option<ResourceRef>,
    /// Set when the draft names a policy the catalog does not have.
    missing_urn: Option<String>,
}

fn load_capability_cedar(
    store: &SqliteWorkflowStore,
    spec: &AgentSpec,
    owner: &str,
) -> Result<LoadedCapability, OrchestratorError> {
    let Some(policy_ref) = spec
        .capability_policy
        .as_ref()
        .and_then(|cap| cap.policy_ref.clone())
    else {
        return Ok(LoadedCapability {
            cedar: DEFAULT_BUILTIN_FALLBACK_POLICY.to_string(),
            policy_ref: None,
            missing_urn: None,
        });
    };
    let Some(urn) = ref_urn(&policy_ref).map(str::to_string) else {
        return Ok(LoadedCapability {
            cedar: DEFAULT_BUILTIN_FALLBACK_POLICY.to_string(),
            policy_ref: None,
            missing_urn: None,
        });
    };
    match store.get_policy_visible(&urn, owner) {
        Ok(record) => Ok(LoadedCapability {
            cedar: record.cedar,
            policy_ref: Some(policy_ref),
            missing_urn: None,
        }),
        Err(OrchestratorError::NotFound(_)) => Ok(LoadedCapability {
            cedar: DEFAULT_BUILTIN_FALLBACK_POLICY.to_string(),
            policy_ref: None,
            missing_urn: Some(urn),
        }),
        Err(err) => Err(err),
    }
}

fn ref_urn(policy_ref: &ResourceRef) -> Option<&str> {
    policy_ref
        .resource_id
        .as_ref()
        .map(|id| id.urn.as_str())
        .filter(|urn| !urn.is_empty())
}

fn agent_urn_of(spec: &AgentSpec) -> Result<&str, OrchestratorError> {
    spec.resource_id
        .as_ref()
        .map(|id| id.urn.as_str())
        .filter(|urn| !urn.is_empty())
        .ok_or_else(|| {
            OrchestratorError::InvalidContract("AgentSpec.resource_id is required".to_string())
        })
}

fn permission_decision(rule: ScopeRule) -> PermissionDecision {
    match rule {
        ScopeRule::Allow => PermissionDecision::Allow,
        ScopeRule::Ask => PermissionDecision::Ask,
        ScopeRule::Deny | ScopeRule::None => PermissionDecision::Deny,
    }
}

/// Two names that land on one target: deny wins, then ask, then allow.
/// Monitored tools have already turned an allow into ask.
fn stricter_rule(left: ScopeRule, right: ScopeRule) -> ScopeRule {
    fn rank(rule: ScopeRule) -> u8 {
        match rule {
            ScopeRule::Deny => 3,
            ScopeRule::Ask => 2,
            ScopeRule::Allow => 1,
            ScopeRule::None => 0,
        }
    }
    if rank(right) > rank(left) {
        right
    } else {
        left
    }
}

fn load_skills(
    store: &SqliteWorkflowStore,
    spec: &AgentSpec,
    owner: &str,
) -> Result<Vec<SkillManifest>, OrchestratorError> {
    let mut skills = Vec::new();
    for skill_ref in &spec.skill_refs {
        let Some(urn) = ref_urn(skill_ref) else {
            continue;
        };
        match store.get_skill_manifest_any(urn, None, owner, false) {
            Ok(skill) => skills.push(skill),
            Err(OrchestratorError::NotFound(_)) => continue,
            Err(err) => return Err(err),
        }
    }
    Ok(skills)
}

fn tool_catalog(
    store: &SqliteWorkflowStore,
    spec: &AgentSpec,
    owner: &str,
) -> Result<(Vec<ToolCatalogEntry>, Vec<PlannedMcpTool>), OrchestratorError> {
    let skills = load_skills(store, spec, owner)?;
    let mut skill_entries = Vec::new();
    for skill in &skills {
        let slug = skill
            .resource_id
            .as_ref()
            .map(|id| id.urn.as_str())
            .and_then(|urn| urn.rsplit('/').next())
            .unwrap_or("");
        if slug.is_empty() {
            continue;
        }
        for tool in &skill.tools {
            if let Some(entry) = skill_catalog_entry(slug, tool) {
                skill_entries.push(entry);
            }
        }
    }
    let mcp = planned_mcp_tools(store, owner)?;
    let mut mcp_entries = Vec::new();
    for tool in &mcp {
        mcp_entries.push(ToolCatalogEntry {
            tool_name: tool.guest_name.clone(),
            command: "host".to_string(),
            entrypoint: format!("mcp:{}:{}", tool.connection_id, tool.vendor_tool),
            digest: String::new(),
            side_effect: tool.side_effect,
            skill_name: String::new(),
            approval_required: false,
        });
    }
    let catalog = merge_tool_catalog(builtin_tool_catalog(), skill_entries, mcp_entries)?;
    Ok((catalog, mcp))
}

fn skill_catalog_entry(skill_name: &str, tool: &ToolSpec) -> Option<ToolCatalogEntry> {
    let name = tool.name.as_str();
    if name.is_empty()
        || vam_spec::validation::reserved_builtin_tool_names().contains(&name)
        || matches!(name, "write_note" | "list_workspace")
    {
        return None;
    }
    use vam_spec::v1::tool_spec::ExecutionProvider;
    let (command, entrypoint, digest) = match &tool.execution_provider {
        Some(ExecutionProvider::HostExec(host)) if !host.adapter_id.is_empty() => (
            "host".to_string(),
            host.adapter_id.clone(),
            vam_spec::validation::host_tool_digest(&host.adapter_id),
        ),
        Some(ExecutionProvider::LocalExec(local)) => (
            local.command.clone(),
            local.entrypoint.clone(),
            String::new(),
        ),
        Some(ExecutionProvider::McpServer(_)) => ("mcp".to_string(), String::new(), String::new()),
        _ => return None,
    };
    Some(ToolCatalogEntry {
        tool_name: name.to_string(),
        command,
        entrypoint,
        digest,
        side_effect: tool.side_effect,
        skill_name: skill_name.to_string(),
        approval_required: tool.approval_required,
    })
}

/// MCP tools the owner can see, with the same guest names a session would plan.
fn planned_mcp_tools(
    store: &SqliteWorkflowStore,
    owner: &str,
) -> Result<Vec<PlannedMcpTool>, OrchestratorError> {
    let mut chosen: BTreeMap<String, McpConnection> = BTreeMap::new();
    for conn in store.list_visible_mcp_connections(owner)? {
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
    let mut used: BTreeMap<String, (String, String)> = BTreeMap::new();
    let mut planned = Vec::new();
    for conn in chosen.into_values() {
        let tools = plan_mcp_tools(&conn.connection_id, &conn.tools)?;
        for (tool, guest) in &tools {
            if let Some((other_id, other_vendor)) = used.get(guest) {
                return Err(OrchestratorError::InvalidContract(format!(
                    "namespaced mcp tool '{}' on connection '{}' collides with '{other_vendor}' on connection '{other_id}' (both become '{guest}')",
                    tool.name, conn.connection_id
                )));
            }
            used.insert(
                guest.clone(),
                (conn.connection_id.clone(), tool.name.clone()),
            );
        }
        for (tool, guest_name) in tools {
            planned.push(PlannedMcpTool {
                connection_id: conn.connection_id.clone(),
                server_url: conn.url.clone(),
                vendor_tool: tool.name.clone(),
                guest_name,
                side_effect: mcp_tool_side_effect(&conn, &tool.name),
                ask: crate::mcp_registry::mcp_tool_source_rule(&conn, &tool.name).is_some(),
            });
        }
    }
    Ok(planned)
}

/// Destinations a legacy session would have allowed from the secrets these
/// skills require. User-scope rows win over org rows, and an empty
/// destination list uses the same fallback a session binding does.
fn internet_destinations(
    vault: &SecretsVault,
    owner: &str,
    skills: &[SkillManifest],
) -> Result<Vec<String>, OrchestratorError> {
    let requirements = crate::secret_requirements::collect_skill_requirements(skills);
    let bindings = vault.session_egress_bindings_for_caller(owner)?;
    let mut seen_names = BTreeSet::new();
    let mut destinations = Vec::new();
    for requirement in requirements {
        if !seen_names.insert(requirement.name.clone()) {
            continue;
        }
        let scope = RequirementScope::from_proto(requirement.scope)?;
        if !is_requirement_satisfied(vault, owner, &requirement.name, scope)? {
            continue;
        }
        let Some(binding) = bindings
            .iter()
            .find(|binding| binding.binding_id == requirement.name)
        else {
            continue;
        };
        for destination in &binding.allowed_destinations {
            if !destinations.iter().any(|existing| existing == destination) {
                destinations.push(destination.clone());
            }
        }
    }
    Ok(destinations)
}

/// Effective decision the AX-5.2b resolver reports for one guest tool.
#[cfg(test)]
fn resolved_effect(
    permissions: &AgentPermissions,
    name: &str,
    target: &str,
    side_effect: i32,
) -> Result<vam_security::ToolEffect, OrchestratorError> {
    use vam_security::ToolQuery;
    use vam_spec::v1::SideEffectClass;

    let target = Target::parse(target).map_err(|err| {
        OrchestratorError::InvalidContract(format!("target '{target}' did not parse: {err}"))
    })?;
    let side_effect = SideEffectClass::try_from(side_effect)
        .ok()
        .filter(|class| *class != SideEffectClass::Unspecified);
    let explained = vam_security::explain_tool(
        permissions,
        "agent draft",
        ToolQuery {
            name,
            target: &target,
            side_effect,
            source: None,
            org: None,
        },
    )
    .map_err(|err| OrchestratorError::InvalidContract(err.to_string()))?;
    Ok(explained.effect)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use vam_security::{classify_tools, NamedTool, ToolEffect};
    use vam_spec::encoding::json_to_proto;
    use vam_spec::v1::{
        agent_spec::InstructionSource, CapabilityPolicyRef, ResourceId, SkillManifest, Version,
    };

    use crate::catalog_suggest::SuggestKind;
    use crate::secrets::SecretKind;
    use crate::security::{compose_policy, derive_hitl_overlay};
    use crate::users::LOCAL_USER_ID;

    const JIRA_AGENT_JSON: &str = include_str!("../../../examples/jira-assistant/agent.json");
    const JIRA_POLICY: &str = include_str!("../../../examples/jira-assistant/policy.cedar");
    const JIRA_SKILL_JSON: &str =
        include_str!("../../../examples/jira-assistant/skills/jira-integration/manifest.json");
    const SQLITE_AGENT_JSON: &str = include_str!("../../../examples/sqlite-assistant/agent.json");
    const SQLITE_POLICY: &str = include_str!("../../../examples/sqlite-assistant/policy.cedar");
    const SQLITE_SKILL_JSON: &str =
        include_str!("../../../examples/sqlite-assistant/skills/db-tools/manifest.json");

    fn store_and_vault() -> (Arc<SqliteWorkflowStore>, Arc<SecretsVault>) {
        let store = Arc::new(SqliteWorkflowStore::in_memory().expect("store"));
        let vault = SecretsVault::for_tests(Arc::clone(&store));
        (store, vault)
    }

    fn publish_example(
        store: &SqliteWorkflowStore,
        agent_json: &str,
        policy_cedar: &str,
        skill_json: &str,
    ) -> String {
        let agent: AgentSpec = json_to_proto("vam.v1.AgentSpec", agent_json).expect("agent json");
        let skill: SkillManifest =
            json_to_proto("vam.v1.SkillManifest", skill_json).expect("skill json");
        let policy_urn = agent
            .capability_policy
            .as_ref()
            .and_then(|cap| cap.policy_ref.as_ref())
            .and_then(|policy| policy.resource_id.as_ref())
            .map(|id| id.urn.clone())
            .expect("policy urn");
        let agent_urn = agent
            .resource_id
            .as_ref()
            .map(|id| id.urn.clone())
            .expect("agent urn");
        store
            .put_policy_with_owner(&policy_urn, policy_cedar, LOCAL_USER_ID)
            .expect("policy");
        store.put_skill(&skill).expect("skill");
        store
            .publish_agent_draft_with_owner(&agent, LOCAL_USER_ID)
            .expect("draft");
        agent_urn
    }

    fn effect_label(effect: ToolEffect) -> &'static str {
        match effect {
            ToolEffect::Allow => "allow",
            ToolEffect::Hitl => "ask",
            ToolEffect::Deny => "deny",
        }
    }

    fn assert_example_decisions_match(
        store: &SqliteWorkflowStore,
        vault: &SecretsVault,
        agent_json: &str,
        policy_cedar: &str,
        skill_json: &str,
        force: bool,
    ) -> MigrationReport {
        let agent_urn = publish_example(store, agent_json, policy_cedar, skill_json);
        let before = load_agent_draft(store, &agent_urn, LOCAL_USER_ID, false).expect("draft");
        let monitored = before
            .hitl_policy
            .as_ref()
            .map(|hitl| hitl.monitored_tools.clone())
            .unwrap_or_default();
        let (catalog, _) = tool_catalog(store, &before, LOCAL_USER_ID).expect("catalog");
        let legacy_cedar =
            compose_policy(policy_cedar, &derive_hitl_overlay(&agent_urn, &monitored));
        let named: Vec<NamedTool> = catalog
            .iter()
            .map(|entry| NamedTool {
                name: entry.tool_name.clone(),
                side_effect: Some(i64::from(entry.side_effect)),
            })
            .collect();
        let legacy = classify_tools(&legacy_cedar, &agent_urn, &named).expect("legacy decisions");

        let report =
            migrate_legacy_draft(store, vault, &agent_urn, LOCAL_USER_ID, force).expect("migrate");
        let after = load_agent_draft(store, &agent_urn, LOCAL_USER_ID, false).expect("migrated");
        assert!(after.capability_policy.is_none());
        assert!(after.hitl_policy.is_none());
        let permissions = after.permissions.expect("permissions");
        assert_eq!(
            permissions.tools.as_ref().map(|tools| tools.default),
            Some(ToolDefault::Deny as i32)
        );
        assert_eq!(
            permissions
                .internet
                .as_ref()
                .map(|internet| internet.default),
            Some(HostDefault::Deny as i32)
        );

        let (rows, _) = targets_for_agent(
            None,
            &RunProvenance {
                agent_origin: "agent draft".to_string(),
                owner_draft_run: true,
            },
            &catalog,
            &[],
        );
        for (entry, legacy_decision) in catalog.iter().zip(legacy.iter()) {
            let target = rows
                .iter()
                .find(|row| row.tool_name == entry.tool_name)
                .map(|row| row.target.as_str())
                .unwrap_or("");
            assert!(!target.is_empty(), "no target for {}", entry.tool_name);
            let resolved =
                resolved_effect(&permissions, &entry.tool_name, target, entry.side_effect)
                    .unwrap_or_else(|err| panic!("{}: {err}", entry.tool_name));
            assert_eq!(
                effect_label(resolved),
                effect_label(legacy_decision.effect),
                "{} ({target})",
                entry.tool_name
            );
        }
        report
    }

    #[test]
    fn jira_and_sqlite_examples_match_legacy_cedar_decisions() {
        let (store, vault) = store_and_vault();
        let jira = assert_example_decisions_match(
            &store,
            &vault,
            JIRA_AGENT_JSON,
            JIRA_POLICY,
            JIRA_SKILL_JSON,
            true,
        );
        assert!(
            jira.unmapped.contains(&"jira_delete_issue".to_string()),
            "{jira:?}"
        );
        assert!(
            jira.kept_advanced.is_empty(),
            "jira policy is known shapes plus the path limb: {jira:?}"
        );
        assert!(
            jira.behaviour_changes
                .iter()
                .any(|line| line.contains("Any site")),
            "{jira:?}"
        );
        assert!(jira
            .converted
            .iter()
            .any(|target| target == "skill:jira-integration/jira_search_issues"));
        assert!(jira
            .converted
            .iter()
            .any(|target| target == "skill:jira-integration/jira_create_issue"));
        assert!(jira
            .converted
            .iter()
            .any(|target| target == "builtin:read_file"));

        let (store, vault) = store_and_vault();
        let sqlite = assert_example_decisions_match(
            &store,
            &vault,
            SQLITE_AGENT_JSON,
            SQLITE_POLICY,
            SQLITE_SKILL_JSON,
            true,
        );
        assert_eq!(
            sqlite.kept_advanced,
            vec!["urn:vam:policy:local.vam.dev/vam/sqlite-memory-only".to_string()],
            "the query_db SQL guard stays attached"
        );
        assert!(sqlite
            .converted
            .iter()
            .any(|target| target == "skill:db-tools/query_db"));
        let after = load_agent_draft(
            &store,
            "urn:vam:agent:local.vam.dev/vam/sqlite-assistant",
            LOCAL_USER_ID,
            false,
        )
        .expect("sqlite draft");
        let advanced = &after.permissions.expect("permissions").advanced;
        assert_eq!(advanced.len(), 1);
        assert_eq!(
            advanced[0].resource_id.as_ref().map(|id| id.urn.as_str()),
            Some("urn:vam:policy:local.vam.dev/vam/sqlite-memory-only")
        );
    }

    #[test]
    fn an_unknown_cedar_shape_stays_in_advanced_and_is_reported() {
        let (store, vault) = store_and_vault();
        let policy_urn = "urn:vam:policy:local.vam.dev/acme/odd-shape";
        let agent_urn = "urn:vam:agent:local.vam.dev/acme/odd-shape";
        let cedar = r#"
            permit (principal, action == Action::"ExecuteTool", resource)
            when { context has reason && context.reason == "because" };
        "#;
        store
            .put_policy_with_owner(policy_urn, cedar, LOCAL_USER_ID)
            .expect("policy");
        let mut spec = AgentSpec {
            resource_id: Some(ResourceId {
                urn: agent_urn.to_string(),
            }),
            display_name: "Odd".to_string(),
            description: "unknown shape".to_string(),
            role: "tester".to_string(),
            instruction_source: Some(InstructionSource::SystemInstructions("x".to_string())),
            allow_any_playbook: true,
            capability_policy: Some(CapabilityPolicyRef {
                policy_ref: Some(ResourceRef {
                    resource_id: Some(ResourceId {
                        urn: policy_urn.to_string(),
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        spec.hitl_policy = None;
        store
            .publish_agent_draft_with_owner(&spec, LOCAL_USER_ID)
            .expect("draft");
        let report =
            migrate_legacy_draft(&store, &vault, agent_urn, LOCAL_USER_ID, false).expect("migrate");
        assert_eq!(report.kept_advanced, vec![policy_urn.to_string()]);
        assert!(report.converted.is_empty(), "{report:?}");
        assert!(report
            .behaviour_changes
            .iter()
            .any(|line| line.contains("advanced")));
        let stored = load_agent_draft(&store, agent_urn, LOCAL_USER_ID, false).expect("stored");
        assert!(stored.capability_policy.is_none());
        assert_eq!(
            stored
                .permissions
                .as_ref()
                .and_then(|permissions| permissions.advanced.first())
                .and_then(|policy| policy.resource_id.as_ref())
                .map(|id| id.urn.as_str()),
            Some(policy_urn)
        );
    }

    #[test]
    fn unmapped_monitored_tools_conflict_until_force() {
        let (store, vault) = store_and_vault();
        let agent_urn = publish_example(&store, JIRA_AGENT_JSON, JIRA_POLICY, JIRA_SKILL_JSON);
        let err = migrate_legacy_draft(&store, &vault, &agent_urn, LOCAL_USER_ID, false)
            .expect_err("unmapped");
        match err {
            OrchestratorError::Conflict(message) => {
                assert!(message.contains("jira_delete_issue"), "{message}");
                assert!(message.contains("force=true"), "{message}");
            }
            other => panic!("expected conflict, got {other}"),
        }
        let still = load_agent_draft(&store, &agent_urn, LOCAL_USER_ID, false).expect("unchanged");
        assert!(still.capability_policy.is_some());
        assert!(still.permissions.is_none());

        let report =
            migrate_legacy_draft(&store, &vault, &agent_urn, LOCAL_USER_ID, true).expect("force");
        assert!(report.unmapped.contains(&"jira_delete_issue".to_string()));
        let stored = load_agent_draft(&store, &agent_urn, LOCAL_USER_ID, false).expect("stored");
        assert!(stored.capability_policy.is_none());
        assert!(stored.hitl_policy.is_none());
    }

    #[test]
    fn internet_keeps_secret_destinations_and_suggests_any_site() {
        let (store, vault) = store_and_vault();
        vault
            .put_secret(
                "jira/base-url",
                "https://acme.atlassian.net",
                SecretKind::Egress,
                &["acme.atlassian.net".to_string()],
                None,
            )
            .expect("secret");
        let agent_urn = publish_example(&store, JIRA_AGENT_JSON, JIRA_POLICY, JIRA_SKILL_JSON);
        let report =
            migrate_legacy_draft(&store, &vault, &agent_urn, LOCAL_USER_ID, true).expect("migrate");
        assert!(report
            .behaviour_changes
            .iter()
            .any(|line| line.contains("Any site")));
        let stored = load_agent_draft(&store, &agent_urn, LOCAL_USER_ID, false).expect("stored");
        let internet = stored
            .permissions
            .as_ref()
            .and_then(|permissions| permissions.internet.as_ref())
            .expect("internet");
        assert_eq!(internet.default, HostDefault::Deny as i32);
        assert!(
            internet
                .rules
                .iter()
                .any(|rule| rule.target == "acme.atlassian.net"
                    && rule.decision == PermissionDecision::Allow as i32),
            "{internet:?}"
        );
    }

    #[test]
    fn migration_leaves_the_released_version_and_shows_the_cedar_suggestion() {
        let (store, vault) = store_and_vault();
        let policy_urn = "urn:vam:policy:local.vam.dev/acme/moved";
        let agent_urn = "urn:vam:agent:local.vam.dev/acme/moved";
        let cedar = r#"
            permit (principal, action == Action::"ExecuteTool", resource)
            when { [Tool::"read_file", Tool::"load_skill"].contains(resource) };
        "#;
        store
            .put_policy_with_owner(policy_urn, cedar, LOCAL_USER_ID)
            .expect("policy");
        let spec = AgentSpec {
            resource_id: Some(ResourceId {
                urn: agent_urn.to_string(),
            }),
            display_name: "Moved".to_string(),
            description: "release interplay".to_string(),
            role: "tester".to_string(),
            instruction_source: Some(InstructionSource::SystemInstructions(
                "stay released".to_string(),
            )),
            allow_any_playbook: true,
            capability_policy: Some(CapabilityPolicyRef {
                policy_ref: Some(ResourceRef {
                    resource_id: Some(ResourceId {
                        urn: policy_urn.to_string(),
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        store
            .publish_agent_draft_with_owner(&spec, LOCAL_USER_ID)
            .expect("draft");
        let version = Version {
            major: 1,
            minor: 0,
            patch: 0,
            ..Default::default()
        };
        let released = store.release_agent(agent_urn, &version).expect("release");
        assert!(released.capability_policy.is_some());
        assert!(released.permissions.is_none());

        migrate_legacy_draft(&store, &vault, agent_urn, LOCAL_USER_ID, false).expect("migrate");

        let still = store
            .get_agent_at_version(agent_urn, &version)
            .expect("released row");
        assert!(still.capability_policy.is_some());
        assert!(still.permissions.is_none());
        assert!(still.hitl_policy.is_none());

        let suggestion = store
            .release_suggestion(SuggestKind::Agent, agent_urn)
            .expect("suggestion");
        assert!(
            suggestion
                .reasons
                .iter()
                .any(|reason| { reason.contains("moved from a Cedar policy") }),
            "{:?}",
            suggestion.reasons
        );
        assert!(suggestion
            .permission_changes
            .iter()
            .any(|change| { change.sentence.contains("moved from a Cedar policy") }));
    }
}
