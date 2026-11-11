use anyhow::Result;
use clap::Subcommand;
use serde_json::Value;
use vam_spec::v1::{DecideApprovalRequest, ResourceId};

use crate::client::{self, resolve_idempotency_key, Client};
use crate::config;

#[derive(Subcommand)]
pub enum ApproveCmd {
    /// List pending approvals, oldest first: one page of 50 (a hint names the next); `--all`
    /// follows every page.
    Ls {
        #[arg(long)]
        all: bool,
    },
    Get {
        id: String,
    },
    Decide {
        id: String,
        #[arg(long)]
        decision: String,
        #[arg(long)]
        reason: Option<String>,
        #[arg(long = "amended-args")]
        amended_args: Option<String>,
        /// JSON object submitted for a gate (`--decision submitted`).
        #[arg(long)]
        response: Option<String>,
        #[arg(long = "expected-revision")]
        expected_revision: Option<i64>,
        /// Save the decision as the agent's rule: `allow` (with `approved`) or
        /// `deny` (with `rejected`). Draft runs of your own agent only.
        #[arg(long)]
        always: Option<String>,
        /// Reuse this idempotency key (default: mint a fresh key).
        #[arg(long)]
        idempotency_key: Option<String>,
    },
}

fn approval_paths(id: &str) -> Result<(String, String)> {
    let canonical = client::normalize_resource_id(id)?;
    let encoded = client::encode_path_segment(&canonical);
    Ok((canonical, encoded))
}

/// Human listing of pending tool-level HITL waits. Each row is a parked run
/// (`PAUSED`, warm during keep-warm or cold after the VM is shut down) that
/// `vam approve decide` can resume.
pub fn format_parked_runs(body: &str) -> String {
    let Ok(value) = serde_json::from_str::<Value>(body) else {
        return body.to_string();
    };
    let Some(approvals) = value.get("approvals").and_then(Value::as_array) else {
        return serde_json::to_string_pretty(&value).unwrap_or_else(|_| body.to_string());
    };
    if approvals.is_empty() {
        return "no parked runs".to_string();
    }
    let mut lines = Vec::with_capacity(approvals.len());
    for approval in approvals {
        let claims = approval.get("claims").unwrap_or(approval);
        let run_id = json_urn(claims, &["runId", "run_id"]).unwrap_or_else(|| "-".into());
        let request_id =
            json_urn(claims, &["requestId", "request_id"]).unwrap_or_else(|| "-".into());
        let tool = json_string(claims, &["targetToolName", "target_tool_name"])
            .unwrap_or_else(|| "-".into());
        lines.push(format!(
            "parked  run={run_id}  tool={tool}  approval={request_id}"
        ));
        if let Some(line) = reconnect_line(claims) {
            lines.push(format!("    {line}"));
        }
    }
    lines.join("\n")
}

/// The line for a call parked until a connector is connected (`reauth:<scope>:<owner>:connector:<id>`
/// in `lockedReason`): the message and the command that unparks it. A decision is not needed.
fn reconnect_line(claims: &Value) -> Option<String> {
    let reason = json_string(claims, &["lockedReason", "locked_reason"])?;
    let id = reason.rsplit_once(":connector:")?.1;
    if !reason.starts_with("reauth:") || id.is_empty() {
        return None;
    }
    let message = json_string(claims, &["approvalMessage", "approval_message"])
        .unwrap_or_else(|| format!("Connect {id} to continue"));
    Some(format!("{message}: vam connectors connect {id}"))
}

fn json_urn(value: &Value, keys: &[&str]) -> Option<String> {
    let field = json_field(value, keys)?;
    if let Some(text) = field.as_str() {
        return Some(text.to_string());
    }
    field.get("urn").and_then(Value::as_str).map(str::to_string)
}

fn json_string(value: &Value, keys: &[&str]) -> Option<String> {
    json_field(value, keys).and_then(|field| field.as_str().map(str::to_string))
}

fn json_field<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    keys.iter().find_map(|key| value.get(*key))
}

pub async fn run(host: Option<String>, token: Option<String>, cmd: ApproveCmd) -> Result<()> {
    let cfg = config::resolve(host, token).await?;
    let c = Client::new(&cfg)?;
    match cmd {
        ApproveCmd::Ls { all } => {
            let body = client::list_paged(&c, "/v1/approvals", "approvals", all).await?;
            if crate::output::is_json() {
                client::print_json(&body);
            } else {
                println!("{}", format_parked_runs(&body));
            }
        }
        ApproveCmd::Get { id } => {
            let (_, encoded) = approval_paths(&id)?;
            let body = c.get(&format!("/v1/approvals/{encoded}")).await?;
            client::print_json(&body);
        }
        ApproveCmd::Decide {
            id,
            decision,
            reason,
            amended_args,
            response,
            expected_revision,
            always,
            idempotency_key,
        } => {
            let (canonical, encoded) = approval_paths(&id)?;
            let decision_enum = match decision.to_ascii_lowercase().as_str() {
                "approved" => 1,
                "rejected" => 2,
                "amended" => 3,
                "submitted" => 4,
                other => anyhow::bail!(
                    "Wrong: decision '{other}' is not a HITL decision.\nExpected: approved, rejected, amended, or submitted.\nFix: vam approve decide <ID> --decision approved."
                ),
            };
            let remember = match always.as_deref().map(str::to_ascii_lowercase).as_deref() {
                None => vam_spec::v1::Remember::Unspecified,
                Some("allow") => vam_spec::v1::Remember::AlwaysAllow,
                Some("deny") => vam_spec::v1::Remember::AlwaysDeny,
                Some(other) => anyhow::bail!(
                    "Wrong: --always '{other}' is not a rule.\nExpected: allow or deny.\nFix: vam approve decide <ID> --decision approved --always allow."
                ),
            };
            let amended_struct = if let Some(json) = amended_args {
                Some(
                    vam_spec::encoding::json_to_proto::<prost_types::Struct>(
                        "google.protobuf.Struct",
                        &json,
                    )
                    .map_err(|e| anyhow::anyhow!(e))?,
                )
            } else {
                None
            };
            let response_struct = if let Some(json) = response {
                Some(
                    vam_spec::encoding::json_to_proto::<prost_types::Struct>(
                        "google.protobuf.Struct",
                        &json,
                    )
                    .map_err(|e| anyhow::anyhow!(e))?,
                )
            } else {
                None
            };
            let req = DecideApprovalRequest {
                request_id: Some(ResourceId { urn: canonical }),
                decision: decision_enum,
                rejection_reason: reason.unwrap_or_default(),
                amended_args: amended_struct,
                response: response_struct,
                expected_decision_revision: expected_revision.unwrap_or(0),
                remember: remember as i32,
                ..Default::default()
            };
            let json = vam_spec::encoding::proto_to_json("vam.v1.DecideApprovalRequest", &req)
                .map_err(|e| anyhow::anyhow!(e))?;
            let key = resolve_idempotency_key(idempotency_key);
            let body = c
                .post_raw_json(&format!("/v1/approvals/{encoded}/decide"), &json, &key)
                .await?;
            client::print_json(&body);
        }
    }
    Ok(())
}

#[cfg(test)]
mod flag_tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn decide_accepts_submitted_response() {
        #[derive(Parser)]
        struct Wrap {
            #[command(subcommand)]
            cmd: ApproveCmd,
        }
        let parsed = Wrap::try_parse_from([
            "vam",
            "decide",
            "req-1",
            "--decision",
            "submitted",
            "--response",
            r#"{"option":"approved"}"#,
        ])
        .unwrap();
        assert!(matches!(
            parsed.cmd,
            ApproveCmd::Decide { ref decision, ref response, .. }
                if decision == "submitted" && response.as_deref() == Some(r#"{"option":"approved"}"#)
        ));
    }

    #[test]
    fn decide_accepts_idempotency_key() {
        #[derive(Parser)]
        struct Wrap {
            #[command(subcommand)]
            cmd: ApproveCmd,
        }
        let parsed = Wrap::try_parse_from([
            "vam",
            "decide",
            "req-1",
            "--decision",
            "approved",
            "--idempotency-key",
            "hitl",
        ])
        .unwrap();
        assert!(matches!(
            parsed.cmd,
            ApproveCmd::Decide { ref idempotency_key, .. } if idempotency_key.as_deref() == Some("hitl")
        ));
    }

    #[test]
    fn format_parked_runs_lists_tool_hitl_and_empty() {
        let body = r#"{
            "approvals": [{
                "claims": {
                    "requestId": {"urn": "urn:vam:hitl:local.vam.dev/acme/approval-1"},
                    "runId": {"urn": "urn:vam:run:local.vam.dev/acme/run-1"},
                    "targetToolName": "jira_create_issue"
                }
            }]
        }"#;
        let text = format_parked_runs(body);
        assert!(text.contains("parked  run=urn:vam:run:local.vam.dev/acme/run-1"));
        assert!(text.contains("tool=jira_create_issue"));
        assert!(text.contains("approval=urn:vam:hitl:local.vam.dev/acme/approval-1"));
        assert_eq!(format_parked_runs(r#"{"approvals":[]}"#), "no parked runs");
    }

    #[test]
    fn a_connector_park_names_the_command_that_continues_it() {
        let body = r#"{"approvals":[{"claims":{
            "requestId":{"urn":"urn:a"},"runId":{"urn":"urn:r"},"targetToolName":"m365_search",
            "approvalMessage":"Reconnect Microsoft 365 to continue",
            "lockedReason":"reauth:user:u1:connector:microsoft-365"}}]}"#;
        let text = format_parked_runs(body);
        assert!(text
            .contains("Reconnect Microsoft 365 to continue: vam connectors connect microsoft-365"));
        // An MCP park and a plain approval get no extra line.
        let mcp = r#"{"approvals":[{"claims":{"lockedReason":"reauth:user:u1:linear"}}]}"#;
        assert_eq!(format_parked_runs(mcp).lines().count(), 1);
    }
}
