//! Turn a vamd HTTP error into text an agent can act on.
//!
//! The message names what was wrong, what was expected, and the command that
//! fixes it. It does not include a raw HTTP status. The status stays on
//! [`crate::output::CliError`] so exit codes and `--json` `httpStatus` still work.

use serde_json::Value;

use crate::output;

struct ApiBody {
    message: String,
    retryable: bool,
}

pub fn failure(method: &str, path: &str, status: u16, body: &str) -> anyhow::Error {
    let message = explain(method, path, status, body);
    output::fail_http(output::exit_code_for_status(status), message, status)
}

pub fn explain(method: &str, path: &str, status: u16, body: &str) -> String {
    let parsed = parse_body(body);
    let wrong = if parsed.message.is_empty() {
        "the request was rejected".to_string()
    } else {
        parsed.message.clone()
    };
    let (expected, fix) = hint(method, path, status, &parsed);
    let text = format!(
        "{}\n{}\n{}",
        line("Wrong", &wrong),
        line("Expected", &expected),
        line("Fix", &fix),
    );
    strip_http_status(&text)
}

fn line(label: &str, text: &str) -> String {
    let text = text.trim().trim_end_matches('.').trim();
    format!("{label}: {text}.")
}

fn parse_body(body: &str) -> ApiBody {
    let scrubbed = strip_http_status(body.trim());
    if let Ok(value) = serde_json::from_str::<Value>(&scrubbed) {
        let message = value
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let retryable = value
            .get("retryable")
            .and_then(|r| r.as_bool())
            .unwrap_or(false);
        if !message.is_empty() {
            return ApiBody { message, retryable };
        }
        if let Some(code) = value.get("code").and_then(|c| c.as_str()) {
            return ApiBody {
                message: code.to_string(),
                retryable,
            };
        }
    }
    ApiBody {
        message: scrubbed,
        retryable: false,
    }
}

fn hint(method: &str, path: &str, status: u16, body: &ApiBody) -> (String, String) {
    let msg = body.message.to_ascii_lowercase();
    if status == 401 || msg.contains("bearer token") || msg.contains("unauthenticated") {
        return (
            "a live session token or API key".into(),
            "vam login --username <NAME> --password-from-env <VAR>".into(),
        );
    }
    if status == 403 {
        return (
            "a credential allowed to call this route".into(),
            "vam login as a user who has the required scope".into(),
        );
    }
    if is_workspace_file_put(method, path) {
        if msg.contains("agent is running") {
            let session = extract_after(&body.message, "(session ")
                .and_then(|s| s.split(',').next())
                .map(str::trim);
            return (
                "an idle workspace with no active session".into(),
                match session {
                    Some(id) => format!("vam session terminate {id}"),
                    None => "vam session ls".into(),
                },
            );
        }
        if status == 413 {
            return (
                "a local file at or under the 32 MiB upload limit".into(),
                command_for(method, path),
            );
        }
    }
    if msg.contains("agent is running") {
        let run = extract_after(&body.message, ", run ").unwrap_or("<RUN_ID>");
        return (
            "no active run on that workspace".into(),
            format!("vam run cancel {run}"),
        );
    }
    if msg.contains("reserved by parked run") {
        let run = quoted_after(&body.message, "parked run").unwrap_or_else(|| "<RUN_ID>".into());
        return (
            "that parked run to be cancelled, or to expire, before the workspace can be reused"
                .into(),
            format!("vam run cancel {run}"),
        );
    }
    if msg.contains("workspace-bound agent") {
        let bound = quoted_after(&body.message, "workspace-bound agent");
        let fix = match bound {
            Some(agent) => format!("vam run {agent} -p \"...\""),
            None => "vam run <agent> -p \"...\" on a new workspace".into(),
        };
        return (
            "the agent already bound to that workspace, or a new workspace".into(),
            fix,
        );
    }
    if msg.contains("not bound to an agent") {
        return (
            "an agent on the first run for that workspace".into(),
            "vam run <agent> -p \"...\"".into(),
        );
    }
    if msg.contains("missing secret") || msg.contains("missing required secrets") {
        let binding = quoted_after(&body.message, "binding");
        let fix = match binding {
            Some(id) => format!("vam secrets set {id} --from-env <VAR>"),
            None => "vam secrets set <BINDING_ID> --from-env <VAR>".into(),
        };
        return ("that secret in the vault before the run starts".into(), fix);
    }
    if status == 409 && msg.contains("dependenc") && msg.contains("not released") {
        let mut fixes = release_fixes(&body.message);
        fixes.push(command_for(method, path));
        return (
            "every dependency released before this one".into(),
            fixes.join("; then "),
        );
    }
    if msg.contains("already decided") {
        return (
            "an approval that is still pending".into(),
            "vam approve ls".into(),
        );
    }
    if msg.contains("mcp tools drifted") {
        let id = quoted_after(&body.message, "connection").unwrap_or_else(|| "<ID>".into());
        return (
            "the pinned tool list, or an explicit re-approval of the new tools".into(),
            format!("vam mcp refresh {id} --approve-drift"),
        );
    }
    if msg.contains("namespaced mcp tool") || msg.contains("declared more than once") {
        return (
            format!(
                "a tool name of at most {} characters in [a-z0-9_], not a builtin, and unique in the session",
                vam_spec::validation::TOOL_NAME_MAX_LEN
            ),
            "rename the tool on the MCP server or shorten the connection id, then retry vam mcp connect".into(),
        );
    }
    if msg.contains("idempotency") {
        let cmd = command_for(method, path);
        return (
            "an idempotency key on this write".into(),
            format!("{cmd} --idempotency-key <KEY>"),
        );
    }
    if status == 404 {
        return (
            "an id returned by the matching list command".into(),
            list_command(path),
        );
    }
    if matches!(status, 400 | 413 | 422) {
        let expected = if status == 413 && is_workspace_file_put(method, path) {
            "a local file at or under the 32 MiB upload limit"
        } else if status == 413 {
            "a file at or under the 32 MiB download limit"
        } else {
            "a payload that matches this command's flags"
        };
        return (expected.into(), command_for(method, path));
    }
    if status == 409 || status == 412 {
        let expected = if body.retryable {
            "the conflicting run to finish, or a cancel, before retrying the same key"
        } else {
            "the resource in the state this command needs"
        };
        return (expected.into(), command_for(method, path));
    }
    if status == 503 {
        return (
            "a node that is accepting writes".into(),
            "vam node status".into(),
        );
    }
    if (500..600).contains(&status) {
        return ("vamd to accept the request".into(), "vam health".into());
    }
    (
        "a successful response from vamd".into(),
        command_for(method, path),
    )
}

/// `vam <kind> release <urn> --version <X.Y.Z>` for each dependency a release
/// refusal lists. Offender lines read `  <kind> '<urn>' ...`; the header line and
/// kinds with no release command (evaluators) are skipped.
fn release_fixes(message: &str) -> Vec<String> {
    let mut fixes = Vec::new();
    for line in message.lines() {
        let mut words = line.trim().splitn(2, ' ');
        let (Some(kind), Some(rest)) = (words.next(), words.next()) else {
            continue;
        };
        if !matches!(kind, "skill" | "playbook" | "policy" | "agent") {
            continue;
        }
        let Some(urn) = rest.strip_prefix('\'').and_then(|r| r.split('\'').next()) else {
            continue;
        };
        let fix = format!("vam {kind} release {urn} --version <X.Y.Z>");
        if !fixes.contains(&fix) {
            fixes.push(fix);
        }
    }
    fixes
}

fn command_for(method: &str, path: &str) -> String {
    let seg = segments(path);
    let kind = seg.get(1).map(String::as_str).unwrap_or("");
    let third = seg.get(2).map(String::as_str);
    let fourth = seg.get(3).map(String::as_str);
    match (method, kind, third, fourth) {
        ("POST", "runs", None, _) => "vam run <agent> -p \"...\"".into(),
        ("POST", "runs", Some(id), Some("cancel")) => format!("vam run cancel {id}"),
        ("PUT", "agents", None, _) => "vam agent put <FILE>".into(),
        ("POST", "agents", Some(urn), Some("release")) => {
            format!("vam agent release {urn} --version <X.Y.Z>")
        }
        ("PUT", "playbooks", None, _) => "vam playbook put <FILE>".into(),
        ("POST", "playbooks", Some(urn), Some("release")) => {
            format!("vam playbook release {urn} --version <X.Y.Z>")
        }
        ("PUT", "policies", None, _) => "vam policy put <FILE> --urn <URN>".into(),
        ("POST", "policies", Some(urn), Some("release")) => {
            format!("vam policy release {urn} --version <X.Y.Z>")
        }
        ("PUT", "skills", None, _) => "vam skill put <FILE>".into(),
        ("POST", "skills", Some("packages"), None) => "vam skill publish <DIR>".into(),
        ("POST", "skills", Some("packages"), Some(_))
            if seg.get(4).map(String::as_str) == Some("release") =>
        {
            let urn = fourth.unwrap_or("<URN>");
            format!("vam skill release {urn} --version <X.Y.Z>")
        }
        ("POST", "keys", None, _) => "vam keys create --name <NAME>".into(),
        ("DELETE", "keys", Some(id), _) => format!("vam keys revoke {id}"),
        ("PUT", "secrets", None, _) => "vam secrets set <BINDING_ID> --from-env <VAR>".into(),
        ("DELETE", "secrets", Some(id), _) => format!("vam secrets delete {id}"),
        ("POST", "sessions", None, _) => "vam session create".into(),
        ("POST", "sessions", Some("restore"), _) => {
            "vam session restore <SNAPSHOT_ID> --session <ID>".into()
        }
        ("POST", "sessions", Some(id), Some("pause")) => format!("vam session pause {id}"),
        ("POST", "sessions", Some(id), Some("resume")) => format!("vam session resume {id}"),
        ("POST", "sessions", Some(id), Some("terminate")) => format!("vam session terminate {id}"),
        ("POST", "sessions", Some(id), Some("snapshots")) => format!("vam session snapshot {id}"),
        ("POST", "approvals", Some(id), Some("decide")) => {
            format!("vam approve decide {id} --decision approved")
        }
        ("POST", "login", _, _) => "vam login --username <NAME> --password-from-env <VAR>".into(),
        ("POST", "logout", _, _) => "vam logout".into(),
        ("PUT", "workspaces", Some(_), Some("files")) => {
            "vam workspace put <PATH> <LOCAL-FILE> -w <WORKSPACE>".into()
        }
        _ => list_command(path),
    }
}

fn is_workspace_file_put(method: &str, path: &str) -> bool {
    method == "PUT" && path.contains("/workspaces/") && path.contains("/files/")
}

fn list_command(path: &str) -> String {
    match segments(path).get(1).map(String::as_str).unwrap_or("") {
        "runs" => "vam run ls".into(),
        "sessions" => "vam session ls".into(),
        "agents" => "vam agent ls".into(),
        "playbooks" => "vam playbook ls".into(),
        "policies" => "vam policy ls".into(),
        "skills" => "vam skill ls".into(),
        "approvals" => "vam approve ls".into(),
        "keys" => "vam keys list".into(),
        "secrets" => "vam secrets list".into(),
        "workspaces" => "vam workspace ls".into(),
        _ => "vam health".into(),
    }
}

fn segments(path: &str) -> Vec<String> {
    path.split('?')
        .next()
        .unwrap_or(path)
        .split('/')
        .filter(|part| !part.is_empty())
        .map(|part| crate::client::decode_path_segment(part).unwrap_or_else(|_| part.to_string()))
        .collect()
}

fn extract_after<'a>(message: &'a str, marker: &str) -> Option<&'a str> {
    let rest = message.split(marker).nth(1)?.trim();
    let id = rest.trim_end_matches(|c: char| c == ')' || c == '.' || c.is_whitespace());
    if id.is_empty() {
        None
    } else {
        Some(id)
    }
}

fn quoted_after(message: &str, marker: &str) -> Option<String> {
    let rest = message.split(marker).nth(1)?;
    let start = rest.find('\'')? + 1;
    let end = rest[start..].find('\'')?;
    let value = &rest[start..start + end];
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

/// Remove `HTTP 422` (and any other 3-digit status after `HTTP `).
pub fn strip_http_status(text: &str) -> String {
    let mut rest = text;
    let mut out = String::new();
    while let Some(idx) = rest.find("HTTP ") {
        out.push_str(&rest[..idx]);
        let after = &rest[idx + 5..];
        let digits: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
        let parsed = digits.parse::<u16>().ok();
        if digits.len() == 3 && parsed.is_some_and(|n| (100..600).contains(&n)) {
            let mut skip = 5 + digits.len();
            let tail = &rest[idx + skip..];
            if tail.starts_with(':') {
                skip += 1;
                if rest[idx + skip..].starts_with(' ') {
                    skip += 1;
                }
            } else if tail.starts_with(' ') {
                skip += 1;
            }
            rest = &rest[idx + skip..];
        } else {
            out.push_str("HTTP ");
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::ExitCode;

    fn body(code: &str, message: &str, retryable: bool) -> String {
        serde_json::json!({
            "code": code,
            "message": message,
            "retryable": retryable,
        })
        .to_string()
    }

    #[test]
    fn unreleased_dependencies_each_get_a_line_and_a_release_command() {
        let message = "cannot release agent 'urn:vam:agent:local.vam.dev/acme/a': 2 dependencies are not released\n  skill 'urn:vam:skill:local.vam.dev/acme/jira' has no released version: release it first, then release this agent.\n  playbook 'urn:vam:playbook:local.vam.dev/acme/pb' has no released version: release it first, then release this agent.";
        let raw = body("FailedPrecondition", message, false);
        let err = failure(
            "POST",
            "/v1/agents/urn%3Avam%3Aagent%3Alocal.vam.dev%2Facme%2Fa/release",
            409,
            &raw,
        );
        let text = err.to_string();
        assert!(
            text.contains(
                "\n  skill 'urn:vam:skill:local.vam.dev/acme/jira' has no released version"
            ),
            "{text}"
        );
        assert!(
            text.contains(
                "\n  playbook 'urn:vam:playbook:local.vam.dev/acme/pb' has no released version"
            ),
            "{text}"
        );
        assert!(
            text.contains("Expected: every dependency released before this one."),
            "{text}"
        );
        assert!(
            text.contains("Fix: vam skill release urn:vam:skill:local.vam.dev/acme/jira --version <X.Y.Z>; then vam playbook release urn:vam:playbook:local.vam.dev/acme/pb --version <X.Y.Z>; then vam agent release"),
            "{text}"
        );
        assert!(!text.contains("HTTP"), "{text}");
        assert_eq!(output::classify(&err).1, Some(409));
    }

    #[test]
    fn unprocessable_entity_names_the_fix_without_a_status() {
        let raw = body("InvalidArgument", "field prompt is required", false);
        let err = failure("POST", "/v1/runs", 422, &raw);
        let text = err.to_string();
        assert!(!text.contains("HTTP"), "{text}");
        assert!(!text.contains("422"), "{text}");
        assert!(text.contains("Wrong: field prompt is required."), "{text}");
        assert!(
            text.contains("Expected: a payload that matches this command's flags."),
            "{text}"
        );
        assert!(text.contains("Fix: vam run <agent> -p \"...\"."), "{text}");
        assert_eq!(output::classify(&err), (ExitCode::Invalid, Some(422)));
    }

    #[test]
    fn workspace_put_busy_suggests_session_terminate() {
        let raw = body(
            "FailedPrecondition",
            "agent is running on workspace 'assistant' (session sess-1, run run-1)",
            true,
        );
        let text = explain(
            "PUT",
            "/v1/workspaces/assistant/files/workspace/notes/a.md",
            409,
            &raw,
        );
        assert!(text.contains("idle workspace"), "{text}");
        assert!(text.contains("vam session terminate sess-1"), "{text}");
    }

    #[test]
    fn workspace_put_413_names_upload_limit() {
        let raw = body(
            "InvalidArgument",
            "file size 33000000 bytes exceeds maximum 33554432 bytes",
            false,
        );
        let text = explain(
            "PUT",
            "/v1/workspaces/assistant/files/workspace/big.bin",
            413,
            &raw,
        );
        assert!(text.contains("32 MiB upload limit"), "{text}");
        assert!(text.contains("vam workspace put"), "{text}");
    }

    #[test]
    fn agent_running_tells_you_to_cancel() {
        let raw = body(
            "FailedPrecondition",
            "agent is running on workspace 'assistant' (session sess-1, run run-1)",
            true,
        );
        let text = explain("POST", "/v1/runs", 409, &raw);
        assert!(!text.contains("HTTP"), "{text}");
        assert!(!text.contains("409"), "{text}");
        assert!(
            text.contains("Wrong: agent is running on workspace 'assistant'"),
            "{text}"
        );
        assert!(
            text.contains("Expected: no active run on that workspace."),
            "{text}"
        );
        assert!(text.contains("Fix: vam run cancel run-1."), "{text}");
    }

    #[test]
    fn parked_workspace_tells_you_to_cancel() {
        let raw = body(
            "FailedPrecondition",
            "workspace 'assistant' is reserved by parked run 'run-9'; cancel it with: vam run cancel run-9",
            true,
        );
        let text = explain("POST", "/v1/runs", 409, &raw);
        assert!(text.contains("parked run 'run-9'"), "{text}");
        assert!(text.contains("Fix: vam run cancel run-9."), "{text}");
        assert!(!text.contains("HTTP"), "{text}");
        assert!(!text.contains("409"), "{text}");
    }

    #[test]
    fn not_found_names_the_list_command() {
        let raw = body("NotFound", "agent 'urn:vam:agent:ex/a' not found", false);
        let text = explain("GET", "/v1/agents/urn%3Avam%3Aagent%3Aex%2Fa", 404, &raw);
        assert!(text.contains("Fix: vam agent ls."), "{text}");
        assert!(!text.contains("HTTP"), "{text}");
        assert!(!text.contains("404"), "{text}");
    }

    #[test]
    fn auth_failure_names_login() {
        let raw = body("Unauthenticated", "missing or invalid bearer token", false);
        let text = explain("GET", "/v1/node", 401, &raw);
        assert!(
            text.contains("Wrong: missing or invalid bearer token."),
            "{text}"
        );
        assert!(
            text.contains("Fix: vam login --username <NAME> --password-from-env <VAR>."),
            "{text}"
        );
    }

    #[test]
    fn missing_secret_names_secrets_set() {
        let raw = body(
            "FailedPrecondition",
            "missing secret binding 'jira/auth-header' (scope user)",
            false,
        );
        let text = explain("POST", "/v1/runs", 412, &raw);
        assert!(
            text.contains("Fix: vam secrets set jira/auth-header --from-env <VAR>."),
            "{text}"
        );
    }

    #[test]
    fn status_line_in_the_body_is_removed() {
        let text = explain(
            "PUT",
            "/v1/agents",
            422,
            "HTTP 422: {\"message\":\"name is required\"}",
        );
        assert!(!text.contains("HTTP"), "{text}");
        assert!(!text.contains("422"), "{text}");
        assert!(text.contains("Wrong: name is required."), "{text}");
        assert!(text.contains("Fix: vam agent put <FILE>."), "{text}");
    }
}
