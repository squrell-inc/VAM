use std::fs;

use anyhow::Result;
use clap::Subcommand;
use serde_json::{json, Value};

use crate::client::{self, Client};
use crate::config;
use crate::output::{self, ExitCode};

#[derive(Subcommand)]
pub enum McpCmd {
    /// Register a remote MCP server and snapshot its `tools/list`.
    Connect {
        /// Connection id, e.g. `linear` ([a-z0-9_-], max 64).
        connection_id: String,
        /// HTTPS endpoint of the vendor's MCP server.
        #[arg(long)]
        url: String,
        /// Existing vault binding. Omit when passing a token to store.
        #[arg(long)]
        secret: Option<String>,
        /// Token to store. Prefer `--from-env` so the shell history stays clean.
        #[arg(long)]
        value: Option<String>,
        /// Read the token from this environment variable.
        #[arg(long)]
        from_env: Option<String>,
        /// Read the token from this file.
        #[arg(long)]
        file: Option<String>,
        /// `org` (shared, admin/operator) or `user` (caller only).
        #[arg(long, value_parser = ["org", "user"])]
        scope: Option<String>,
        /// Reuse this idempotency key (default: mint a fresh key).
        #[arg(long)]
        idempotency_key: Option<String>,
        /// Store a tool list that added or changed tools since the pin.
        #[arg(long)]
        approve_drift: bool,
        /// Per-tool side effect. `create_issue=read` lowers it. Default is true.
        #[arg(long = "side-effect")]
        side_effect: Vec<String>,
        /// Tool-call timeout in whole seconds (1-600). Default is 120.
        #[arg(long)]
        timeout: Option<u64>,
        /// Treat the server's `readOnlyHint: true` tools as read-only. Without
        /// this a hint is only shown ("read-only (suggested)").
        #[arg(long)]
        accept_read_only_hints: bool,
    },
    /// Change an existing connection's tool-call timeout.
    Update {
        connection_id: String,
        /// Tool-call timeout in whole seconds (1-600).
        #[arg(long)]
        timeout: u64,
        #[arg(long, value_parser = ["org", "user"])]
        scope: Option<String>,
        /// Reuse this idempotency key (default: mint a fresh key).
        #[arg(long)]
        idempotency_key: Option<String>,
    },
    List {
        #[arg(long, value_parser = ["org", "user"])]
        scope: Option<String>,
    },
    Disconnect {
        connection_id: String,
        #[arg(long, value_parser = ["org", "user"])]
        scope: Option<String>,
        /// Reuse this idempotency key (default: mint a fresh key).
        #[arg(long)]
        idempotency_key: Option<String>,
    },
    /// Re-run `tools/list` against the snapshot pinned at connect.
    Refresh {
        connection_id: String,
        #[arg(long, value_parser = ["org", "user"])]
        scope: Option<String>,
        /// Reuse this idempotency key (default: mint a fresh key).
        #[arg(long)]
        idempotency_key: Option<String>,
        /// Accept a tool list that added or changed a name, description, schema,
        /// annotation or server instruction, and approve flagged vendor text.
        #[arg(long)]
        approve_drift: bool,
        /// Treat the server's `readOnlyHint: true` tools as read-only.
        #[arg(long)]
        accept_read_only_hints: bool,
    },
}

pub async fn run(host: Option<String>, token: Option<String>, cmd: McpCmd) -> Result<()> {
    let cfg = config::resolve(host, token).await?;
    let c = Client::new(&cfg)?;
    match cmd {
        McpCmd::Connect {
            connection_id,
            url,
            secret,
            value,
            from_env,
            file,
            scope,
            idempotency_key,
            approve_drift,
            side_effect,
            timeout,
            accept_read_only_hints,
        } => {
            check_connection_id(&connection_id)?;
            check_not_a_connector(&connection_id)?;
            check_url(&url)?;
            let token = resolve_mcp_token(value, from_env, file)?;
            check_secret_inputs(&connection_id, secret.as_deref(), token.is_some())?;
            let mut body = json!({
                "connectionId": connection_id,
                "url": url,
                "secretBindingId": secret.unwrap_or_default(),
                "scope": scope,
            });
            if let Some(token) = token {
                body["secretValue"] = Value::String(token);
            }
            if let Some(seconds) = timeout {
                body["timeoutMs"] = json!(check_timeout_seconds(
                    seconds,
                    "vam mcp connect <ID> --url https://<host>/mcp --timeout 120."
                )?);
            }
            if approve_drift {
                body["approveDrift"] = json!(true);
            }
            if accept_read_only_hints {
                body["acceptReadOnlyHints"] = json!(true);
            }
            if !side_effect.is_empty() {
                body["sideEffects"] = json!(parse_side_effect_flags(&side_effect)?);
            }
            let text = c
                .put_raw_json(
                    "/v1/mcp/connections",
                    &body.to_string(),
                    idempotency_key.as_deref(),
                )
                .await?;
            emit_connection(&text);
        }
        McpCmd::List { scope } => {
            let text = c.get(&with_scope("/v1/mcp/connections", &scope)).await?;
            let value = parse(&text);
            output::emit(value.clone(), || format_list(&value));
        }
        McpCmd::Disconnect {
            connection_id,
            scope,
            idempotency_key,
        } => {
            check_connection_id(&connection_id)?;
            let path = with_scope(&format!("/v1/mcp/connections/{connection_id}"), &scope);
            let text = c.delete(&path, idempotency_key.as_deref()).await?;
            let value = with_restart_notice(parse(&text));
            output::emit(value, || {
                format!("disconnected {connection_id}\n{RESTART_NOTICE}")
            });
        }
        McpCmd::Refresh {
            connection_id,
            scope,
            idempotency_key,
            approve_drift,
            accept_read_only_hints,
        } => {
            check_connection_id(&connection_id)?;
            let path = with_scope(
                &format!("/v1/mcp/connections/{connection_id}/refresh"),
                &scope,
            );
            let idem = client::resolve_idempotency_key(idempotency_key);
            let body = json!({
                "approveDrift": approve_drift,
                "acceptReadOnlyHints": accept_read_only_hints,
            })
            .to_string();
            let text = c.post_raw_json(&path, &body, &idem).await?;
            emit_refresh(&text);
        }
        McpCmd::Update {
            connection_id,
            timeout,
            scope,
            idempotency_key,
        } => {
            check_connection_id(&connection_id)?;
            let timeout_ms = check_timeout_seconds(timeout, "vam mcp update <ID> --timeout 120.")?;
            let path = with_scope(&format!("/v1/mcp/connections/{connection_id}"), &scope);
            let body = json!({ "timeoutMs": timeout_ms });
            let text = c
                .patch_raw_json(&path, &body.to_string(), idempotency_key.as_deref())
                .await?;
            emit_timeout_update(&text);
        }
    }
    Ok(())
}

fn check_timeout_seconds(seconds: u64, fix: &str) -> Result<u64> {
    if (1..=600).contains(&seconds) {
        return Ok(seconds.saturating_mul(1_000));
    }
    Err(output::fail(
        ExitCode::Invalid,
        format!(
            "Wrong: timeout {seconds}s is outside 1-600.\nExpected: a whole number of seconds from 1 to 600.\nFix: {fix}"
        ),
    ))
}

fn check_connection_id(id: &str) -> Result<()> {
    let ok = !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
    if ok {
        return Ok(());
    }
    Err(output::fail(
        ExitCode::Invalid,
        format!(
            "Wrong: connection id '{id}' is not valid.\nExpected: 1-64 characters of a-z, 0-9, '-' or '_'.\nFix: vam mcp connect sentry --url https://mcp.sentry.dev/mcp."
        ),
    ))
}

/// A connector's id is the connector's (CN-1.3): the node refuses a hand-added server on it, so
/// say so here, before anything is sent.
fn check_not_a_connector(id: &str) -> Result<()> {
    let is_mcp_connector = vam_spec::connector::builtin_connectors()
        .iter()
        .any(|c| c.mcp.is_some() && c.urn.rsplit('/').next() == Some(id));
    if !is_mcp_connector {
        return Ok(());
    }
    Err(output::fail(
        ExitCode::Invalid,
        format!(
            "Wrong: '{id}' is a built-in connector, not a server to add by hand.\nExpected: a connection id that is not a connector id.\nFix: ask an admin to enable it (vam connectors), then connect it from the app's Connections page."
        ),
    ))
}

fn check_url(url: &str) -> Result<()> {
    if url.starts_with("https://") {
        return Ok(());
    }
    Err(output::fail(
        ExitCode::Invalid,
        format!(
            "Wrong: MCP url '{url}' is not HTTPS.\nExpected: an https:// endpoint operated by the vendor.\nFix: vam mcp connect <ID> --url https://<vendor-host>/mcp."
        ),
    ))
}

fn resolve_mcp_token(
    value: Option<String>,
    from_env: Option<String>,
    file: Option<String>,
) -> Result<Option<String>> {
    let sources = [value.is_some(), from_env.is_some(), file.is_some()]
        .into_iter()
        .filter(|set| *set)
        .count();
    if sources > 1 {
        return Err(output::fail(
            ExitCode::Invalid,
            "Wrong: more than one token source was given.\nExpected: one of --value, --from-env, or --file.\nFix: vam mcp connect <ID> --url https://<host>/mcp --from-env <VAR>.",
        ));
    }
    if let Some(value) = value {
        return Ok(Some(value));
    }
    if let Some(var) = from_env {
        return match std::env::var(&var) {
            Ok(value) => Ok(Some(value)),
            Err(_) => Err(output::fail(
                ExitCode::Invalid,
                format!(
                    "Wrong: environment variable '{var}' is not set.\nExpected: the token in that variable.\nFix: vam mcp connect <ID> --url https://<host>/mcp --from-env {var}."
                ),
            )),
        };
    }
    if let Some(path) = file {
        return match fs::read_to_string(&path) {
            Ok(text) => Ok(Some(text.trim().to_string())),
            Err(_) => Err(output::fail(
                ExitCode::Invalid,
                format!(
                    "Wrong: could not read secret file '{path}'.\nExpected: a file containing the token.\nFix: vam mcp connect <ID> --url https://<host>/mcp --file {path}."
                ),
            )),
        };
    }
    Ok(None)
}

fn check_secret_inputs(connection_id: &str, binding: Option<&str>, has_token: bool) -> Result<()> {
    let binding_set = binding.is_some_and(|value| !value.is_empty());
    if binding_set && has_token {
        return Err(output::fail(
            ExitCode::Invalid,
            format!(
                "Wrong: --secret and a token were both given.\nExpected: --secret <BINDING_ID> for a vault row that already exists, or one token source to store a new row.\nFix: vam mcp connect {connection_id} --url https://<host>/mcp --scope user --from-env <VAR>."
            ),
        ));
    }
    Ok(())
}

fn with_scope(path: &str, scope: &Option<String>) -> String {
    match scope {
        Some(scope) => format!("{path}?scope={scope}"),
        None => path.to_string(),
    }
}

fn parse(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or_else(|_| json!({ "raw": text }))
}

/// Tools are copied into the session once, when it starts. A later connect does
/// not change a session that is already running.
const RESTART_NOTICE: &str =
    "Restart the session before these tools change. A connection takes effect at the next session start.";

fn with_restart_notice(mut value: Value) -> Value {
    if let Some(obj) = value.as_object_mut() {
        obj.insert("restartRequired".into(), json!(true));
        obj.insert("restartMessage".into(), json!(RESTART_NOTICE));
    }
    value
}

fn emit_refresh(text: &str) {
    let parsed = parse(text);
    let drift = parsed["drift"].as_str().unwrap_or("").to_string();
    let value = if drift == "identical" {
        parsed
    } else {
        with_restart_notice(parsed)
    };
    output::emit(value.clone(), || {
        let c = &value["connection"];
        let tools = c["tools"].as_array().map(Vec::len).unwrap_or(0);
        let head = format!(
            "{} ({}) {} — {tools} tools",
            c["connectionId"].as_str().unwrap_or_default(),
            c["scope"].as_str().unwrap_or_default(),
            c["url"].as_str().unwrap_or_default(),
        );
        match drift.as_str() {
            "identical" => format!("{head}\nTool list unchanged."),
            "shrunk" => {
                let removed = value["removedTools"]
                    .as_array()
                    .map(|rows| {
                        rows.iter()
                            .filter_map(|row| row.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .unwrap_or_default();
                format!("{head}\nRemoved tools: {removed}.\n{RESTART_NOTICE}")
            }
            _ => format!("{head}\n{RESTART_NOTICE}"),
        }
    });
}

fn emit_timeout_update(text: &str) {
    let value = parse(text);
    output::emit(value.clone(), || {
        let c = &value["connection"];
        let seconds = c["timeoutMs"].as_u64().unwrap_or(120_000) / 1_000;
        format!(
            "{} ({}) timeout {seconds}s",
            c["connectionId"].as_str().unwrap_or_default(),
            c["scope"].as_str().unwrap_or_default(),
        )
    });
}

fn emit_connection(text: &str) {
    let value = with_restart_notice(parse(text));
    output::emit(value.clone(), || {
        let c = &value["connection"];
        let tools = c["tools"].as_array().map(Vec::len).unwrap_or(0);
        format!(
            "{} ({}) {} — {tools} tools\n{}{RESTART_NOTICE}",
            c["connectionId"].as_str().unwrap_or_default(),
            c["scope"].as_str().unwrap_or_default(),
            c["url"].as_str().unwrap_or_default(),
            [format_metadata(c), format_capabilities(c)].concat(),
        )
    });
}

/// Vendor text for a terminal: no control characters (an escape sequence in a
/// description must not drive the terminal), bounded.
fn terminal_safe(raw: &str, max: usize) -> String {
    raw.chars()
        .filter(|c| !c.is_control())
        .take(max)
        .collect::<String>()
}

/// Server instructions, tool titles, read-only suggestions and vendor text
/// that waits for approval. Everything here is the vendor's claim.
fn format_metadata(c: &Value) -> String {
    let mut out = String::new();
    if let Some(first) = c["instructions"]
        .as_str()
        .and_then(|text| text.lines().find(|line| !line.trim().is_empty()))
    {
        out.push_str(&format!(
            "  instructions (from the server): {}\n",
            terminal_safe(first, 160)
        ));
    }
    let suggested = &c["suggestedSideEffects"];
    for tool in c["tools"].as_array().into_iter().flatten() {
        let name = tool["name"].as_str().unwrap_or_default();
        let title = tool["title"]
            .as_str()
            .or_else(|| tool["annotations"]["title"].as_str())
            .map(|t| format!("  \"{}\"", terminal_safe(t, 80)))
            .unwrap_or_default();
        let hint = if suggested.get(name).is_some() {
            "  read-only (suggested)"
        } else if tool["annotations"]["readOnlyHint"] == json!(true) {
            "  read-only (confirmed by the owner)"
        } else {
            ""
        };
        if !title.is_empty() || !hint.is_empty() {
            out.push_str(&format!("  {}{title}{hint}\n", terminal_safe(name, 80)));
        }
    }
    for flagged in c["flaggedVendorText"].as_array().into_iter().flatten() {
        let rules = flagged["rules"]
            .as_array()
            .map(|rows| {
                rows.iter()
                    .filter_map(|r| r.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        out.push_str(&format!(
            "  needs approval (--approve-drift): {} [{}]\n",
            terminal_safe(flagged["target"].as_str().unwrap_or_default(), 100),
            terminal_safe(&rules, 100)
        ));
    }
    out
}

fn format_capabilities(c: &Value) -> String {
    let Some(caps) = c["capabilities"].as_array() else {
        return String::new();
    };
    if caps.is_empty() {
        return String::new();
    }
    let lines = caps
        .iter()
        .map(|cap| {
            format!(
                "  {}  {}  {}",
                cap["token"].as_str().unwrap_or_default(),
                cap["origin"].as_str().unwrap_or("inferred"),
                cap["grant"].as_str().unwrap_or("prompt"),
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!("capabilities (inferred, prompt — not auto-granted):\n{lines}\n")
}

fn parse_side_effect_flags(flags: &[String]) -> Result<serde_json::Map<String, Value>> {
    let mut map = serde_json::Map::new();
    for flag in flags {
        let Some((name, class)) = flag.split_once('=') else {
            return Err(output::fail(
                ExitCode::Invalid,
                format!(
                    "Wrong: --side-effect '{flag}' is missing '=read' or '=true'.\nExpected: TOOL=read or TOOL=true.\nFix: vam mcp connect linear --url https://mcp.linear.app/mcp --side-effect create_issue=read."
                ),
            ));
        };
        if name.is_empty() || class.is_empty() {
            return Err(output::fail(
                ExitCode::Invalid,
                "Wrong: --side-effect needs a tool name and a class.\nExpected: TOOL=read or TOOL=true.\nFix: vam mcp connect linear --url https://mcp.linear.app/mcp --side-effect create_issue=read.".to_string(),
            ));
        }
        map.insert(name.to_string(), Value::String(class.to_string()));
    }
    Ok(map)
}

fn format_list(value: &Value) -> String {
    let rows = value["connections"].as_array().cloned().unwrap_or_default();
    if rows.is_empty() {
        return "no MCP connections".to_string();
    }
    rows.iter()
        .map(|c| {
            format!(
                "{}\t{}\t{}\t{} tools\t{}s\n{}",
                c["connectionId"].as_str().unwrap_or_default(),
                c["scope"].as_str().unwrap_or_default(),
                c["url"].as_str().unwrap_or_default(),
                c["tools"].as_array().map(Vec::len).unwrap_or(0),
                c["timeoutMs"].as_u64().unwrap_or(120_000) / 1_000,
                [format_metadata(c), format_capabilities(c)].concat(),
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod flag_tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct Wrap {
        #[command(subcommand)]
        cmd: McpCmd,
    }

    #[test]
    fn mutating_mcp_commands_accept_idempotency_key() {
        for args in [
            &[
                "vam",
                "connect",
                "linear",
                "--url",
                "https://mcp.linear.app/mcp",
                "--idempotency-key",
                "k",
            ][..],
            &["vam", "disconnect", "linear", "--idempotency-key", "k"][..],
            &[
                "vam",
                "refresh",
                "linear",
                "--idempotency-key",
                "k",
                "--approve-drift",
            ][..],
            &[
                "vam",
                "update",
                "linear",
                "--timeout",
                "120",
                "--idempotency-key",
                "k",
            ][..],
        ] {
            let parsed = Wrap::try_parse_from(args).unwrap();
            let key = match parsed.cmd {
                McpCmd::Connect {
                    idempotency_key, ..
                }
                | McpCmd::Disconnect {
                    idempotency_key, ..
                }
                | McpCmd::Update {
                    idempotency_key, ..
                } => idempotency_key,
                McpCmd::Refresh {
                    idempotency_key,
                    approve_drift,
                    ..
                } => {
                    assert!(approve_drift, "{args:?}");
                    idempotency_key
                }
                McpCmd::List { .. } => None,
            };
            assert_eq!(key.as_deref(), Some("k"), "{args:?}");
        }
    }

    #[test]
    fn mcp_name_and_drift_errors_are_three_lines_and_name_the_tool() {
        let name = crate::http_error::explain(
            "PUT",
            "/v1/mcp/connections",
            400,
            r#"{"message":"namespaced mcp tool 'read_file' on connection 'jira' shadows builtin 'read_file'","code":"InvalidArgument"}"#,
        );
        assert!(name.starts_with("Wrong: "), "{name}");
        assert!(name.contains("read_file"), "{name}");
        assert!(
            name.contains("\nExpected: ") && name.contains("\nFix: "),
            "{name}"
        );
        assert_eq!(output::exit_code_for_status(400), ExitCode::Invalid);

        let drift = crate::http_error::explain(
            "POST",
            "/v1/mcp/connections/jira/refresh",
            409,
            r#"{"message":"mcp tools drifted on connection 'jira': changed 'search' (description)","code":"AlreadyExists"}"#,
        );
        assert!(drift.starts_with("Wrong: "), "{drift}");
        assert!(drift.contains("search"), "{drift}");
        assert!(drift.contains("description"), "{drift}");
        assert!(
            drift.contains("vam mcp refresh jira --approve-drift"),
            "{drift}"
        );
        assert_ne!(output::exit_code_for_status(409), ExitCode::Ok);
    }

    #[test]
    fn local_validation_is_three_line_and_invalid() {
        for err in [
            check_connection_id("Bad/Id").unwrap_err(),
            check_url("http://mcp.linear.app").unwrap_err(),
            check_not_a_connector("linear").unwrap_err(),
        ] {
            let msg = err.to_string();
            assert!(msg.starts_with("Wrong: "), "{msg}");
            assert!(
                msg.contains("\nExpected: ") && msg.contains("\nFix: "),
                "{msg}"
            );
            assert_eq!(output::classify(&err).0, ExitCode::Invalid);
        }
    }

    #[test]
    fn a_connector_refusal_names_vam_connectors_and_other_ids_pass() {
        let msg = check_not_a_connector("linear").unwrap_err().to_string();
        assert!(msg.contains("vam connectors"), "{msg}");
        assert!(check_not_a_connector("e2e-mcp").is_ok());
        assert!(check_not_a_connector("microsoft-365").is_ok());
    }

    #[test]
    fn connect_token_flags_do_not_echo_the_token() {
        let parsed = Wrap::try_parse_from([
            "vam",
            "connect",
            "linear",
            "--url",
            "https://mcp.linear.app/mcp",
            "--scope",
            "user",
            "--from-env",
            "LINEAR_TOKEN",
            "--idempotency-key",
            "k",
        ])
        .unwrap();
        match parsed.cmd {
            McpCmd::Connect {
                scope,
                from_env,
                value,
                file,
                secret,
                idempotency_key,
                ..
            } => {
                assert_eq!(scope.as_deref(), Some("user"));
                assert_eq!(from_env.as_deref(), Some("LINEAR_TOKEN"));
                assert!(value.is_none() && file.is_none() && secret.is_none());
                assert_eq!(idempotency_key.as_deref(), Some("k"));
            }
            McpCmd::List { .. }
            | McpCmd::Disconnect { .. }
            | McpCmd::Refresh { .. }
            | McpCmd::Update { .. } => {
                panic!("expected connect");
            }
        }

        let err = check_secret_inputs("linear", Some("other/auth-header"), true).unwrap_err();
        let msg = err.to_string();
        assert!(!msg.contains("super-secret-token"), "{msg}");
        assert!(msg.starts_with("Wrong: "), "{msg}");
        assert!(
            msg.contains("\nExpected: ") && msg.contains("\nFix: "),
            "{msg}"
        );
        assert_eq!(output::classify(&err).0, ExitCode::Invalid);

        let err = resolve_mcp_token(
            Some("super-secret-token".into()),
            Some("LINEAR_TOKEN".into()),
            None,
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(!msg.contains("super-secret-token"), "{msg}");
        assert!(
            msg.contains("\nExpected: ") && msg.contains("\nFix: "),
            "{msg}"
        );
    }

    #[test]
    fn connect_output_says_a_restart_is_required() {
        let value = with_restart_notice(json!({
            "connection": {
                "connectionId": "linear",
                "scope": "user",
                "url": "https://mcp.linear.app/mcp",
                "tools": []
            }
        }));
        assert_eq!(value["restartRequired"], true);
        let message = value["restartMessage"].as_str().unwrap();
        assert!(message.contains("Restart the session"), "{message}");
        assert!(message.contains("next session start"), "{message}");
        assert!(!message.contains("token"), "{message}");
    }

    #[test]
    fn side_effect_flags_and_capability_lines() {
        let parsed = Wrap::try_parse_from([
            "vam",
            "connect",
            "linear",
            "--url",
            "https://mcp.linear.app/mcp",
            "--side-effect",
            "create_issue=read",
        ])
        .unwrap();
        let McpCmd::Connect { side_effect, .. } = parsed.cmd else {
            panic!("expected connect");
        };
        let map = parse_side_effect_flags(&side_effect).unwrap();
        assert_eq!(map["create_issue"], "read");

        let err = parse_side_effect_flags(&["create_issue".into()]).unwrap_err();
        let msg = err.to_string();
        assert!(msg.starts_with("Wrong: "), "{msg}");
        assert!(
            msg.contains("\nExpected: ") && msg.contains("\nFix: "),
            "{msg}"
        );

        let text = format_capabilities(&json!({
            "capabilities": [{
                "token": "tool:linear__create_issue",
                "origin": "inferred",
                "grant": "prompt"
            }]
        }));
        assert!(text.contains("tool:linear__create_issue"), "{text}");
        assert!(text.contains("inferred"), "{text}");
        assert!(text.contains("prompt"), "{text}");
        assert!(text.contains("not auto-granted"), "{text}");
    }

    #[test]
    fn connect_and_update_parse_timeout_and_reject_out_of_range() {
        let parsed = Wrap::try_parse_from([
            "vam",
            "connect",
            "linear",
            "--url",
            "https://mcp.linear.app/mcp",
            "--timeout",
            "180",
        ])
        .unwrap();
        let McpCmd::Connect { timeout, .. } = parsed.cmd else {
            panic!("expected connect");
        };
        assert_eq!(timeout, Some(180));
        assert_eq!(
            check_timeout_seconds(180, "vam mcp connect <ID> --timeout 120.").unwrap(),
            180_000
        );

        let parsed = Wrap::try_parse_from([
            "vam",
            "update",
            "linear",
            "--timeout",
            "45",
            "--scope",
            "org",
            "--idempotency-key",
            "k",
        ])
        .unwrap();
        let McpCmd::Update {
            connection_id,
            timeout,
            scope,
            idempotency_key,
        } = parsed.cmd
        else {
            panic!("expected update");
        };
        assert_eq!(connection_id, "linear");
        assert_eq!(timeout, 45);
        assert_eq!(scope.as_deref(), Some("org"));
        assert_eq!(idempotency_key.as_deref(), Some("k"));
        assert_eq!(
            check_timeout_seconds(45, "vam mcp update linear --timeout 120.").unwrap(),
            45_000
        );

        for (seconds, fix) in [
            (
                0,
                "vam mcp connect <ID> --url https://<host>/mcp --timeout 120.",
            ),
            (601, "vam mcp update <ID> --timeout 120."),
        ] {
            let err = check_timeout_seconds(seconds, fix).unwrap_err();
            let msg = err.to_string();
            assert!(msg.starts_with("Wrong: "), "{msg}");
            assert!(msg.contains("1-600"), "{msg}");
            assert!(
                msg.contains("\nExpected: ") && msg.contains("\nFix: "),
                "{msg}"
            );
            assert!(msg.contains(fix), "{msg}");
            assert_eq!(output::classify(&err).0, ExitCode::Invalid);
        }
        assert_eq!(check_timeout_seconds(1, "fix").unwrap(), 1_000);
        assert_eq!(check_timeout_seconds(600, "fix").unwrap(), 600_000);

        let listed = format_list(&json!({
            "connections": [{
                "connectionId": "linear",
                "scope": "user",
                "url": "https://mcp.linear.app/mcp",
                "tools": [{ "name": "create_issue" }],
                "timeoutMs": 180000
            }]
        }));
        assert!(
            listed.contains("linear\tuser\thttps://mcp.linear.app/mcp\t1 tools\t180s"),
            "{listed}"
        );
    }

    #[test]
    fn list_shows_instructions_titles_read_only_suggestion_and_flagged_text() {
        let listed = format_list(&json!({
            "connections": [{
                "connectionId": "notion",
                "scope": "org",
                "url": "https://mcp.notion.com/mcp",
                "timeoutMs": 120000,
                "instructions": "\u{1b}[31mSearch first.\nThen create.",
                "tools": [
                    { "name": "search", "title": "Search pages",
                      "annotations": { "readOnlyHint": true } },
                    { "name": "create", "annotations": { "readOnlyHint": false } }
                ],
                "suggestedSideEffects": { "search": "read" },
                "flaggedVendorText": [
                    { "target": "tool:create", "rules": ["vt1.instruction_override"] }
                ]
            }]
        }));
        assert!(
            listed.contains("instructions (from the server): [31mSearch first."),
            "{listed}"
        );
        assert!(!listed.contains('\u{1b}'), "no terminal escapes");
        assert!(
            listed.contains("search  \"Search pages\"  read-only (suggested)"),
            "{listed}"
        );
        assert!(!listed.contains("create  read-only"), "{listed}");
        assert!(
            listed.contains(
                "needs approval (--approve-drift): tool:create [vt1.instruction_override]"
            ),
            "{listed}"
        );
    }

    #[test]
    fn accept_read_only_hints_flag_parses_on_connect_and_refresh() {
        for args in [
            &[
                "vam",
                "connect",
                "n",
                "--url",
                "https://mcp.notion.com/mcp",
                "--accept-read-only-hints",
            ][..],
            &["vam", "refresh", "n", "--accept-read-only-hints"][..],
        ] {
            let parsed = Wrap::try_parse_from(args).unwrap();
            let accepted = match parsed.cmd {
                McpCmd::Connect {
                    accept_read_only_hints,
                    ..
                }
                | McpCmd::Refresh {
                    accept_read_only_hints,
                    ..
                } => accept_read_only_hints,
                _ => false,
            };
            assert!(accepted, "{args:?}");
        }
    }
}
