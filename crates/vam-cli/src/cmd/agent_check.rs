//! `vam agent check <urn>`. The server owns the check (`POST /v1/agent-checks`); this
//! module sends the request and formats the answer. Informational: a missing skill
//! never changes the exit code.

use anyhow::Result;
use serde_json::{json, Value};

use super::catalog::parse_version_triplet;
use crate::client::{self, Client};

pub(super) async fn run(
    c: &Client,
    urn: &str,
    version: Option<&str>,
    draft: bool,
    json: bool,
) -> Result<()> {
    let mut agent_ref = json!({ "resourceId": { "urn": urn } });
    if draft {
        agent_ref["version"] = json!({ "major": 1, "prerelease": "draft" });
    } else if let Some(raw) = version {
        let v = parse_version_triplet(raw)?;
        agent_ref["version"] = json!({ "major": v.major, "minor": v.minor, "patch": v.patch });
    }
    let body = json!({ "agentRef": agent_ref }).to_string();
    let resp = c
        .post_raw_json("/v1/agent-checks", &body, "agent-check")
        .await?;
    if json || crate::output::is_json() {
        client::print_json(&resp);
        return Ok(());
    }
    let parsed: Value = serde_json::from_str(&resp)?;
    print!("{}", render(&parsed));
    Ok(())
}

fn label(version: &Value) -> String {
    if version.is_null() {
        return "-".to_string();
    }
    if version["prerelease"].as_str() == Some("draft") {
        return "draft".to_string();
    }
    let n = |k: &str| version[k].as_i64().unwrap_or(0);
    format!("{}.{}.{}", n("major"), n("minor"), n("patch"))
}

fn short(urn: &str) -> &str {
    urn.rsplit('/').next().unwrap_or(urn)
}

fn caps(v: &Value) -> String {
    let list: Vec<&str> = v
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    if list.is_empty() {
        "-".to_string()
    } else {
        list.join(", ")
    }
}

/// The table printed for `vam agent check`.
pub(super) fn render(resp: &Value) -> String {
    let mut out = String::new();
    let agent = &resp["agentRef"];
    out.push_str(&format!(
        "Agent {} ({})\n",
        agent["resourceId"]["urn"].as_str().unwrap_or("-"),
        label(&agent["version"])
    ));
    if resp["allowAnyPlaybook"].as_bool() == Some(true) {
        out.push_str("Allows any playbook (not listed).\n");
    }
    out.push('\n');
    out.push_str(&format!(
        "{:<28} {:<28} {:<9} CAPABILITIES\n",
        "PLAYBOOK", "REQUIRED SKILL", "ATTACHED"
    ));
    let skills = resp["skills"].as_array().cloned().unwrap_or_default();
    for playbook in resp["playbooks"].as_array().into_iter().flatten() {
        let urn = playbook["urn"].as_str().unwrap_or("-");
        let name = format!(
            "{} {}",
            short(urn),
            if playbook["role"].as_str() == Some("default") {
                "(default)"
            } else {
                ""
            }
        );
        let name = name.trim_end();
        if let Some(error) = playbook["error"].as_str().filter(|e| !e.is_empty()) {
            out.push_str(&format!("{name:<28} error: {error}\n"));
            continue;
        }
        let required = playbook["requiredSkills"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        if required.is_empty() {
            out.push_str(&format!(
                "{name:<28} {:<28} {:<9} {}\n",
                "-",
                "-",
                caps(&playbook["requiredCapabilities"])
            ));
            continue;
        }
        for (i, skill) in required.iter().enumerate() {
            let skill_urn = skill["urn"].as_str().unwrap_or("-");
            let shown = if i == 0 { name } else { "" };
            let skill_name = format!("{} {}", short(skill_urn), label(&skill["version"]));
            let attached = if skill["attached"].as_bool() == Some(true) {
                "yes"
            } else {
                "no"
            };
            let detail = match skill["error"].as_str().filter(|e| !e.is_empty()) {
                Some(error) => format!("error: {error}"),
                None => skills
                    .iter()
                    .find(|s| s["urn"].as_str() == Some(skill_urn))
                    .map(|s| caps(&s["capabilities"]))
                    .unwrap_or_else(|| "-".to_string()),
            };
            out.push_str(&format!(
                "{shown:<28} {skill_name:<28} {attached:<9} {detail}\n"
            ));
        }
    }
    if resp["playbooks"].as_array().is_none_or(|p| p.is_empty()) {
        out.push_str("(no playbooks referenced)\n");
    }
    let missing = resp["playbooks"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|p| p["requiredSkills"].as_array().into_iter().flatten())
        .filter(|s| s["attached"].as_bool() != Some(true))
        .count();
    if missing > 0 {
        out.push_str(&format!(
            "\n{missing} required skill(s) not attached. vamd still adds them at run time.\n"
        ));
    }
    for warning in resp["warnings"].as_array().into_iter().flatten() {
        if let Some(text) = warning.as_str() {
            out.push_str(&format!("\nwarning: {text}\n"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_prints_warnings() {
        let resp = json!({
            "agentRef": {"resourceId": {"urn": "urn:vam:agent:local.vam.dev/acme/a"}},
            "playbooks": [],
            "warnings": ["termination cannot take effect"]
        });
        assert!(render(&resp).contains("warning: termination cannot take effect"));
        let none = json!({"agentRef": {"resourceId": {"urn": "u"}}, "playbooks": []});
        assert!(!render(&none).contains("warning:"));
    }

    #[test]
    fn render_marks_missing_skills_and_lists_capabilities() {
        let resp = json!({
            "agentRef": {"resourceId": {"urn": "urn:vam:agent:x/y/a"}, "version": {"prerelease": "draft"}},
            "playbooks": [{
                "urn": "urn:vam:playbook:x/y/triage", "role": "default",
                "requiredSkills": [{"urn": "urn:vam:skill:x/y/jira", "version": {"major": 1}}],
                "requiredCapabilities": ["tool:jira.search"]
            }],
            "skills": [{"urn": "urn:vam:skill:x/y/jira", "capabilities": ["tool:jira.search"]}]
        });
        let text = render(&resp);
        assert!(text.contains("triage (default)"), "{text}");
        assert!(text.contains("jira 1.0.0"), "{text}");
        assert!(text.contains(" no "), "{text}");
        assert!(text.contains("tool:jira.search"), "{text}");
        assert!(text.contains("1 required skill(s) not attached"), "{text}");
    }
}
