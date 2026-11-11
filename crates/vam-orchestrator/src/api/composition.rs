//! `POST /v1/agent-checks`: what an agent's playbooks need, and whether it has it.
//!
//! Informational only. A required skill the agent does not attach is reported with
//! `attached: false`; run-time inclusion (`catalog_resolve`) and release are unchanged.

use axum::extract::State;
use axum::response::Response;
use vam_spec::v1::{
    AgentCheckPlaybook, AgentCheckRequest, AgentCheckRequiredSkill, AgentCheckResponse,
    AgentCheckSkill, AgentSpec, ResourceId, ResourceRef, RunSpec, SkillChannel, Version,
};

use crate::api::auth::AuthContext;
use crate::api::catalog_auth::require_catalog_read;
use crate::api::error::{json_response, parse_json_body, ApiError};
use crate::api::{json_http_response, AppState};
use crate::catalog_resolve::{
    resolve_ref_for_owner, run_resolve_context, ArtifactKind, PinnedRef, ResolveMode,
};
use crate::error::OrchestratorError;
use crate::workflow::SqliteWorkflowStore;

pub(super) async fn agent_check(
    State(state): State<AppState>,
    auth: AuthContext,
    body: String,
) -> Result<Response, ApiError> {
    require_catalog_read(&auth)?;
    let req: AgentCheckRequest = parse_json_body("vam.v1.AgentCheckRequest", &body)?;
    let agent_ref = req
        .agent_ref
        .ok_or_else(|| OrchestratorError::InvalidContract("agentRef is required".into()))?;
    let resp = check_agent(
        &state.store,
        &agent_ref,
        auth.owner_user_id(),
        auth.has_users_admin(),
    )?;
    let (code, json) = json_response("vam.v1.AgentCheckResponse", &resp)?;
    Ok(json_http_response(code, json))
}

fn urn_of(resource_ref: &ResourceRef) -> &str {
    resource_ref
        .resource_id
        .as_ref()
        .map(|id| id.urn.as_str())
        .unwrap_or("")
}

/// Load the agent the way `GET /v1/agents/{urn}` does, and the version it resolved to.
fn load_agent(
    store: &SqliteWorkflowStore,
    agent_ref: &ResourceRef,
    caller: &str,
    include_foreign_drafts: bool,
) -> Result<(AgentSpec, Version), OrchestratorError> {
    let urn = urn_of(agent_ref);
    if urn.is_empty() {
        return Err(OrchestratorError::InvalidContract(
            "agentRef.resourceId.urn is required".into(),
        ));
    }
    if !urn.starts_with("urn:vam:agent:") {
        return Err(OrchestratorError::InvalidContract(format!(
            "agentRef '{urn}' is not an agent URN"
        )));
    }
    match agent_ref.version.as_ref() {
        Some(version) => {
            let agent =
                store.get_agent_at_version_visible(urn, version, caller, include_foreign_drafts)?;
            Ok((agent, version.clone()))
        }
        None => {
            let record = store.get_agent_record_visible(urn, caller, include_foreign_drafts)?;
            let agent = record.spec.ok_or_else(|| {
                OrchestratorError::CorruptState(format!("agent '{urn}' listing missing spec"))
            })?;
            Ok((agent, record.version.unwrap_or_default()))
        }
    }
}

pub(crate) fn check_agent(
    store: &SqliteWorkflowStore,
    agent_ref: &ResourceRef,
    caller: &str,
    include_foreign_drafts: bool,
) -> Result<AgentCheckResponse, OrchestratorError> {
    let (agent, agent_version) = load_agent(store, agent_ref, caller, include_foreign_drafts)?;
    let agent_urn = urn_of(agent_ref).to_string();
    let checked_ref = ResourceRef {
        resource_id: Some(ResourceId {
            urn: agent_urn.clone(),
        }),
        version: Some(agent_version.clone()),
        ..Default::default()
    };

    // Same owner and mode a run of this agent would resolve its refs with.
    let (owner, mode) = run_resolve_context(
        store,
        &RunSpec {
            agent_ref: Some(checked_ref.clone()),
            ..Default::default()
        },
        caller,
    );

    let attached_refs: Vec<&ResourceRef> = agent
        .skill_refs
        .iter()
        .chain(
            agent
                .overlay
                .iter()
                .filter_map(|o| o.skills.as_ref())
                .flat_map(|s| s.add.iter()),
        )
        .filter(|r| !urn_of(r).is_empty())
        .collect();
    let attached: Vec<String> = attached_refs
        .iter()
        .map(|r| urn_of(r).to_string())
        .collect();

    let mut skills: Vec<AgentCheckSkill> = Vec::new();
    for skill_ref in attached_refs {
        skill_entry(&mut skills, urn_of(skill_ref)).attached = true;
        fill_skill(store, &mut skills, skill_ref, &owner, mode);
    }

    let mut playbooks: Vec<AgentCheckPlaybook> = Vec::new();
    let mut any_output_schema = false;
    let default_ref = agent.default_playbook_ref.iter().map(|r| ("default", r));
    let allowed_refs = agent.allowed_playbook_refs.iter().map(|r| ("allowed", r));
    for (role, playbook_ref) in default_ref.chain(allowed_refs) {
        let urn = urn_of(playbook_ref);
        if urn.is_empty() || playbooks.iter().any(|p| p.urn == urn) {
            continue;
        }
        let (entry, has_output_schema) = check_playbook(
            store,
            role,
            playbook_ref,
            &attached,
            &mut skills,
            &owner,
            mode,
        );
        any_output_schema |= has_output_schema;
        playbooks.push(entry);
    }

    let termination = agent
        .reasoning_policy
        .as_ref()
        .map(|policy| policy.termination)
        .unwrap_or_default();

    Ok(AgentCheckResponse {
        agent_ref: Some(checked_ref),
        allow_any_playbook: agent.allow_any_playbook,
        playbooks,
        skills,
        warnings: {
            let mut warnings = termination_warnings(termination, any_output_schema);
            warnings.extend(crate::web_admission::check_warnings(store, &agent));
            warnings
        },
    })
}

fn check_playbook(
    store: &SqliteWorkflowStore,
    role: &str,
    playbook_ref: &ResourceRef,
    attached: &[String],
    skills: &mut Vec<AgentCheckSkill>,
    owner: &str,
    mode: ResolveMode,
) -> (AgentCheckPlaybook, bool) {
    let urn = urn_of(playbook_ref).to_string();
    let mut entry = AgentCheckPlaybook {
        urn: urn.clone(),
        role: role.to_string(),
        ..Default::default()
    };
    let loaded = resolve_ref_for_owner(store, ArtifactKind::Playbook, playbook_ref, owner, mode)
        .and_then(|pinned| {
            let version = pinned.reference.version.clone().ok_or_else(|| {
                OrchestratorError::InvalidContract(format!("playbook '{urn}' has no version"))
            })?;
            let spec = store.get_playbook_at_version(&urn, &version, owner, false)?;
            Ok((version, spec))
        });
    let (version, spec) = match loaded {
        Ok(found) => found,
        Err(err) => {
            entry.error = err.to_string();
            return (entry, false);
        }
    };
    let has_output_schema = spec.output_schema.is_some();
    entry.version = Some(version);
    entry.required_capabilities = spec
        .requires
        .as_ref()
        .map(|r| r.capabilities.clone())
        .unwrap_or_default();

    for skill_ref in vam_spec::required_skill_refs(&spec) {
        let skill_urn = urn_of(skill_ref).to_string();
        if skill_urn.is_empty() {
            continue;
        }
        let mut required = AgentCheckRequiredSkill {
            urn: skill_urn.clone(),
            attached: attached.contains(&skill_urn),
            ..Default::default()
        };
        let skill = skill_entry(skills, &skill_urn);
        if !skill.required_by.contains(&urn) {
            skill.required_by.push(urn.clone());
        }
        // An attached skill keeps the agent's own ref; otherwise the playbook's ref decides.
        if !required.attached {
            fill_skill(store, skills, skill_ref, owner, mode);
        }
        let skill = skill_entry(skills, &skill_urn);
        required.version = skill.version.clone();
        required.error = skill.error.clone();
        entry.required_skills.push(required);
    }
    (entry, has_output_schema)
}

/// `TERMINATION_POLICY_OUTPUT_SATISFIED` ends a run on the playbook output
/// check. Without a playbook that declares an output schema it cannot decide, and
/// the run ends as if the policy were unset.
fn termination_warnings(termination: i32, any_output_schema: bool) -> Vec<String> {
    if termination == vam_spec::v1::TerminationPolicy::OutputSatisfied as i32 && !any_output_schema
    {
        return vec![
            "reasoningPolicy.termination is TERMINATION_POLICY_OUTPUT_SATISFIED, but no playbook of this agent declares an output schema. The run ends on a reply without tool calls, as if termination were unset."
                .to_string(),
        ];
    }
    Vec::new()
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)] // test module position left as is; moving it is a src/ layout change
mod termination_warning_tests {
    use super::termination_warnings;
    use vam_spec::v1::TerminationPolicy;

    #[test]
    fn output_satisfied_without_an_output_schema_warns() {
        let satisfied = TerminationPolicy::OutputSatisfied as i32;
        assert_eq!(termination_warnings(satisfied, false).len(), 1);
        assert!(termination_warnings(satisfied, true).is_empty());
        assert!(termination_warnings(TerminationPolicy::ExplicitClaim as i32, false).is_empty());
        assert!(termination_warnings(0, false).is_empty());
    }
}

fn skill_entry<'a>(skills: &'a mut Vec<AgentCheckSkill>, urn: &str) -> &'a mut AgentCheckSkill {
    if let Some(index) = skills.iter().position(|s| s.urn == urn) {
        return &mut skills[index];
    }
    skills.push(AgentCheckSkill {
        urn: urn.to_string(),
        ..Default::default()
    });
    let last = skills.len() - 1;
    &mut skills[last]
}

/// Resolve `skill_ref` and record its version and manifest capabilities on its entry.
/// A ref that does not resolve sets `error`; it is never an HTTP failure.
fn fill_skill(
    store: &SqliteWorkflowStore,
    skills: &mut Vec<AgentCheckSkill>,
    skill_ref: &ResourceRef,
    owner: &str,
    mode: ResolveMode,
) {
    let urn = urn_of(skill_ref).to_string();
    let entry = skill_entry(skills, &urn);
    if entry.version.is_some() || !entry.error.is_empty() {
        return;
    }
    match resolve_skill(store, skill_ref, owner, mode) {
        Ok((version, capabilities)) => {
            entry.version = Some(version);
            entry.capabilities = capabilities;
        }
        Err(err) => entry.error = err.to_string(),
    }
}

fn resolve_skill(
    store: &SqliteWorkflowStore,
    skill_ref: &ResourceRef,
    owner: &str,
    mode: ResolveMode,
) -> Result<(Version, Vec<String>), OrchestratorError> {
    let PinnedRef { reference, channel } =
        resolve_ref_for_owner(store, ArtifactKind::Skill, skill_ref, owner, mode)?;
    let urn = urn_of(&reference).to_string();
    let version = reference.version.clone().ok_or_else(|| {
        OrchestratorError::InvalidContract(format!("skill '{urn}' has no version"))
    })?;
    // Skills published as packages carry the manifest on the package row.
    let package_channel = if channel == SkillChannel::Draft {
        SkillChannel::Draft
    } else {
        SkillChannel::Released
    };
    let manifest = match store.get_skill_package_for_query_visible(
        &urn,
        Some(&version),
        package_channel,
        owner,
        false,
    ) {
        Ok(package) => package.spec,
        Err(OrchestratorError::NotFound(_)) => {
            Some(store.get_skill_at_version(&urn, &version, owner, false)?)
        }
        Err(err) => return Err(err),
    };
    let capabilities = manifest.map(|m| m.capabilities).unwrap_or_default();
    Ok((version, capabilities))
}
