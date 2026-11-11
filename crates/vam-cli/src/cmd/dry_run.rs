//! `vam run --dry-run`: resolve catalog refs, evaluate Cedar, print the
//! tool decision table, and do not call `POST /v1/runs`.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use vam_security::{
    classify_tools, compose_policy, hitl_execute_overlay, NamedTool, ToolDecision,
    BUILTIN_EXECUTE_TOOL_POLICY, BUILTIN_TOOL_NAMES,
};
use vam_spec::v1::{
    AgentSpec, GetAgentResponse, GetPlaybookResponse, GetPolicyResponse, GetSkillResponse,
    PlaybookSpec, ResourceRef, SkillManifest,
};

use crate::client::{self, Client};
use crate::output::{self, ExitCode};

pub struct DryRunReport {
    pub agent_urn: String,
    pub policy_urn: Option<String>,
    pub policy_source: &'static str,
    pub policy_warning: Option<String>,
    pub playbook_urn: Option<String>,
    pub skills: Vec<ResolvedSkill>,
    pub tools: Vec<ToolDecision>,
}

pub struct ResolvedSkill {
    pub urn: String,
    pub version: Option<String>,
}

pub async fn fetch_agent(c: &Client, urn: &str) -> Result<AgentSpec> {
    let path = format!("/v1/agents/{}", client::encode_path_segment(urn));
    let body = c
        .get(&path)
        .await
        .with_context(|| format!("GET agent {urn}"))?;
    let resp: GetAgentResponse = client::parse_json("vam.v1.GetAgentResponse", &body)?;
    resp.spec.ok_or_else(|| {
        output::fail(
            ExitCode::NotFound,
            format!("agent '{urn}' has no spec. Run `vam agent ls`."),
        )
    })
}

pub async fn fetch_playbook(c: &Client, urn: &str) -> Result<PlaybookSpec> {
    let path = format!("/v1/playbooks/{}", client::encode_path_segment(urn));
    let body = c
        .get(&path)
        .await
        .with_context(|| format!("GET playbook {urn}"))?;
    let resp: GetPlaybookResponse = client::parse_json("vam.v1.GetPlaybookResponse", &body)?;
    resp.spec.ok_or_else(|| {
        output::fail(
            ExitCode::NotFound,
            format!("playbook '{urn}' has no spec. Run `vam playbook ls`."),
        )
    })
}

pub fn ensure_playbook_allowed(agent: &AgentSpec, urn: &str) -> Result<()> {
    if agent.allow_any_playbook {
        return Ok(());
    }
    let listed = agent
        .allowed_playbook_refs
        .iter()
        .any(|resource_ref| ref_urn(resource_ref) == urn);
    if listed {
        return Ok(());
    }
    Err(output::fail(
        ExitCode::Invalid,
        format!(
            "playbook '{urn}' is not in this agent's allowedPlaybookRefs, and allowAnyPlaybook is false"
        ),
    ))
}

/// Resolve skills and the capability policy, then classify every tool.
/// Does not create a run or a VM.
pub async fn evaluate(
    c: &Client,
    agent: AgentSpec,
    playbook: Option<PlaybookSpec>,
) -> Result<DryRunReport> {
    let agent_urn = agent
        .resource_id
        .as_ref()
        .map(|id| id.urn.trim().to_string())
        .filter(|urn| !urn.is_empty())
        .ok_or_else(|| {
            output::fail(
                ExitCode::Invalid,
                "dry-run needs an agent URN on resourceId",
            )
        })?;

    let policy_urn = capability_policy_urn(&agent);
    let (base, policy_source, policy_warning) = match &policy_urn {
        Some(urn) => match fetch_policy_cedar(c, urn).await {
            Ok(cedar) => (cedar, "catalog", None),
            Err(err) if is_http_not_found(&err) => (
                BUILTIN_EXECUTE_TOOL_POLICY.to_string(),
                "builtins",
                Some(format!(
                    "declared capability policy '{urn}' is missing from the catalog; using builtins-only"
                )),
            ),
            Err(err) => return Err(err),
        },
        None => (
            BUILTIN_EXECUTE_TOOL_POLICY.to_string(),
            "builtins",
            None,
        ),
    };

    let monitored = agent
        .hitl_policy
        .as_ref()
        .map(|policy| {
            policy
                .monitored_tools
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let cedar = compose_policy(&base, &hitl_execute_overlay(&agent_urn, &monitored));

    let skill_urns = skill_urns(&agent, playbook.as_ref());
    let mut skills = Vec::with_capacity(skill_urns.len());
    let mut manifests = Vec::with_capacity(skill_urns.len());
    for urn in skill_urns {
        let manifest = fetch_skill(c, &urn).await?;
        skills.push(ResolvedSkill {
            urn: urn.clone(),
            version: manifest.version.as_ref().map(format_version),
        });
        manifests.push(manifest);
    }

    let named = collect_tools(&agent, playbook.as_ref(), &manifests);
    let tools = classify_tools(&cedar, &agent_urn, &named).map_err(|err| {
        output::fail(
            ExitCode::Invalid,
            format!("capability policy did not compile: {err}"),
        )
    })?;

    Ok(DryRunReport {
        agent_urn,
        policy_urn,
        policy_source,
        policy_warning,
        playbook_urn: playbook.as_ref().and_then(|spec| {
            spec.resource_id
                .as_ref()
                .map(|id| id.urn.clone())
                .filter(|urn| !urn.is_empty())
        }),
        skills,
        tools,
    })
}

pub fn emit_report(report: &DryRunReport) {
    let value = report_json(report);
    output::emit(value, || format_human(report));
}

fn report_json(report: &DryRunReport) -> serde_json::Value {
    serde_json::json!({
        "dryRun": true,
        "started": false,
        "resolved": {
            "agent": report.agent_urn,
            "policy": {
                "urn": report.policy_urn,
                "source": report.policy_source,
                "warning": report.policy_warning,
            },
            "playbook": report.playbook_urn,
            "skills": report.skills.iter().map(|skill| serde_json::json!({
                "urn": skill.urn,
                "version": skill.version,
            })).collect::<Vec<_>>(),
        },
        "tools": report.tools.iter().map(|tool| serde_json::json!({
            "name": tool.name,
            "decision": tool.effect.as_str(),
            "matchingPolicies": tool.matching_policies,
            "diagnostic": tool.diagnostic,
        })).collect::<Vec<_>>(),
    })
}

fn format_human(report: &DryRunReport) -> String {
    let mut out = String::new();
    out.push_str("Resolved\n");
    out.push_str(&format!("  agent     {}\n", report.agent_urn));
    match &report.policy_urn {
        Some(urn) if report.policy_source == "catalog" => {
            out.push_str(&format!("  policy    {urn}\n"));
        }
        Some(urn) => {
            out.push_str(&format!("  policy    builtins ({urn} missing)\n"));
        }
        None => out.push_str("  policy    builtins\n"),
    }
    match &report.playbook_urn {
        Some(urn) => out.push_str(&format!("  playbook  {urn}\n")),
        None => out.push_str("  playbook  -\n"),
    }
    if report.skills.is_empty() {
        out.push_str("  skill     -\n");
    } else {
        for skill in &report.skills {
            match &skill.version {
                Some(version) => out.push_str(&format!("  skill     {}  {version}\n", skill.urn)),
                None => out.push_str(&format!("  skill     {}\n", skill.urn)),
            }
        }
    }
    if let Some(warning) = &report.policy_warning {
        out.push_str(&format!("  warning   {warning}\n"));
    }
    out.push('\n');

    let name_width = report
        .tools
        .iter()
        .map(|tool| tool.name.len())
        .max()
        .unwrap_or(4)
        .max(4);
    out.push_str(&format!(
        "{:<name_width$}  {:<8}  POLICIES\n",
        "TOOL", "DECISION"
    ));
    for tool in &report.tools {
        let policies = if tool.matching_policies.is_empty() {
            "-".to_string()
        } else {
            tool.matching_policies.join(", ")
        };
        out.push_str(&format!(
            "{:<name_width$}  {:<8}  {policies}\n",
            tool.name,
            tool.effect.as_str()
        ));
        if !tool.diagnostic.is_empty() {
            out.push_str(&format!("  {}\n", tool.diagnostic));
        }
    }
    out.push_str("\nNo run started.\n");
    out
}

async fn fetch_policy_cedar(c: &Client, urn: &str) -> Result<String> {
    let path = format!("/v1/policies/{}", client::encode_path_segment(urn));
    let body = c
        .get(&path)
        .await
        .with_context(|| format!("GET policy {urn}"))?;
    let resp: GetPolicyResponse = client::parse_json("vam.v1.GetPolicyResponse", &body)?;
    let record = resp.record.ok_or_else(|| {
        output::fail(
            ExitCode::NotFound,
            format!("policy '{urn}' has no Cedar document"),
        )
    })?;
    if record.cedar.trim().is_empty() {
        return Err(output::fail(
            ExitCode::Invalid,
            format!("policy '{urn}' has empty Cedar text"),
        ));
    }
    Ok(record.cedar)
}

async fn fetch_skill(c: &Client, urn: &str) -> Result<SkillManifest> {
    let path = format!("/v1/skills/{}", client::encode_path_segment(urn));
    let body = c
        .get(&path)
        .await
        .with_context(|| format!("GET skill {urn}"))?;
    let resp: GetSkillResponse = client::parse_json("vam.v1.GetSkillResponse", &body)?;
    resp.spec.ok_or_else(|| {
        output::fail(
            ExitCode::NotFound,
            format!("skill '{urn}' has no manifest. Run `vam skill ls`."),
        )
    })
}

fn skill_urns(agent: &AgentSpec, playbook: Option<&PlaybookSpec>) -> Vec<String> {
    let mut urns = Vec::new();
    let playbook_refs = playbook.map(vam_spec::required_skill_refs).unwrap_or(&[]);
    for resource_ref in agent.skill_refs.iter().chain(playbook_refs.iter()) {
        let urn = ref_urn(resource_ref);
        if urn.is_empty() || urns.iter().any(|seen| seen == &urn) {
            continue;
        }
        urns.push(urn);
    }
    urns
}

fn collect_tools(
    agent: &AgentSpec,
    playbook: Option<&PlaybookSpec>,
    skills: &[SkillManifest],
) -> Vec<NamedTool> {
    let mut tools: BTreeMap<String, Option<i64>> = BTreeMap::new();
    for manifest in skills {
        for tool in &manifest.tools {
            let name = tool.name.trim();
            if name.is_empty() {
                continue;
            }
            tools.insert(name.to_string(), Some(i64::from(tool.side_effect)));
        }
    }
    if let Some(playbook) = playbook {
        for step in &playbook.steps {
            if let Some(binding) = &step.binding {
                let name = binding.tool_name.trim();
                if !name.is_empty() {
                    tools.entry(name.to_string()).or_default();
                }
            }
        }
    }
    if let Some(hitl) = &agent.hitl_policy {
        for name in &hitl.monitored_tools {
            let name = name.trim();
            if !name.is_empty() {
                tools.entry(name.to_string()).or_default();
            }
        }
    }
    for name in BUILTIN_TOOL_NAMES {
        tools.entry((*name).to_string()).or_default();
    }
    tools
        .into_iter()
        .map(|(name, side_effect)| NamedTool { name, side_effect })
        .collect()
}

fn capability_policy_urn(agent: &AgentSpec) -> Option<String> {
    agent
        .capability_policy
        .as_ref()
        .and_then(|policy| policy.policy_ref.as_ref())
        .and_then(|resource_ref| resource_ref.resource_id.as_ref())
        .map(|id| id.urn.trim().to_string())
        .filter(|urn| !urn.is_empty())
}

fn ref_urn(resource_ref: &ResourceRef) -> String {
    resource_ref
        .resource_id
        .as_ref()
        .map(|id| id.urn.trim().to_string())
        .unwrap_or_default()
}

fn format_version(version: &vam_spec::v1::Version) -> String {
    let mut text = format!("{}.{}.{}", version.major, version.minor, version.patch);
    if !version.prerelease.is_empty() {
        text.push('-');
        text.push_str(&version.prerelease);
    }
    text
}

fn is_http_not_found(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<crate::output::CliError>()
            .and_then(|cli| cli.http_status)
            == Some(404)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use vam_security::{DecidingScope, ToolEffect};

    #[test]
    fn human_table_lists_allow_deny_hitl_and_policies() {
        let report = DryRunReport {
            agent_urn: "urn:vam:agent:examples.local/vam/demo".into(),
            policy_urn: Some("urn:vam:policy:examples.local/vam/demo".into()),
            policy_source: "catalog",
            policy_warning: None,
            playbook_urn: None,
            skills: vec![],
            tools: vec![
                ToolDecision {
                    name: "read_file".into(),
                    effect: ToolEffect::Allow,
                    matching_policies: vec!["policy0".into()],
                    diagnostic: String::new(),
                    target: String::new(),
                    deciding_scope: DecidingScope::None,
                    deciding_rule: String::new(),
                    scopes: Vec::new(),
                },
                ToolDecision {
                    name: "write_file".into(),
                    effect: ToolEffect::Hitl,
                    matching_policies: vec!["policy1".into()],
                    diagnostic: "requires human approval before execution".into(),
                    target: String::new(),
                    deciding_scope: DecidingScope::None,
                    deciding_rule: String::new(),
                    scopes: Vec::new(),
                },
                ToolDecision {
                    name: "run".into(),
                    effect: ToolEffect::Deny,
                    matching_policies: vec![],
                    diagnostic: "denied".into(),
                    target: String::new(),
                    deciding_scope: DecidingScope::None,
                    deciding_rule: String::new(),
                    scopes: Vec::new(),
                },
            ],
        };
        let human = format_human(&report);
        assert!(human.contains("read_file"));
        assert!(human.contains("allow"));
        assert!(human.contains("policy0"));
        assert!(human.contains("hitl"));
        assert!(human.contains("policy1"));
        assert!(human.contains("deny"));
        assert!(human.contains("No run started."));
        let json = report_json(&report);
        assert_eq!(json["started"], false);
        assert_eq!(json["tools"][1]["decision"], "hitl");
        assert_eq!(json["tools"][1]["matchingPolicies"][0], "policy1");
    }
}
