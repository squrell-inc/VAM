//! `vam policy explain`, `vam agent permissions`, and `vam agent migrate`.
//!
//! These commands call the permissions HTTP API. Human explain output matches
//! design §IV.9. `permissions set` prints AX-5.1 validation errors as vamd
//! returned them.

use std::fs;
use std::io::Read;

use anyhow::{Context, Result};
use clap::Subcommand;
use serde_json::Value;

use super::catalog::parse_version_triplet;
use crate::client::{self, encode_path_segment, Client};
use crate::http_error;
use crate::output::{self, fail, fail_http, ExitCode};

#[derive(Subcommand)]
pub enum PermissionsCmd {
    /// Print draft permissions as JSON. `--effective` prints the decided table.
    Get {
        agent: String,
        /// Release X.Y.Z for `--effective`. Default: your draft if you own one,
        /// otherwise the latest release.
        #[arg(long)]
        version: Option<String>,
        /// Print the effective permissions table (human; `--json` for the response).
        #[arg(long)]
        effective: bool,
        /// Print the raw JSON response.
        #[arg(long)]
        json: bool,
    },
    /// Replace the owner's draft permissions from a JSON file, or `-` for stdin.
    Set {
        agent: String,
        /// Permissions JSON file, or `-` to read stdin.
        #[arg(short = 'f', long = "file")]
        file: String,
        /// Reuse this idempotency key (default: mint a fresh key).
        #[arg(long)]
        idempotency_key: Option<String>,
    },
    /// Turn web search and fetch on or off in the owner's draft. Off by default.
    Web {
        agent: String,
        /// `on` lets the agent use the web broker's tools; `off` removes them.
        #[arg(value_enum)]
        state: WebState,
    },
    /// List Squrell permission presets (JSON).
    Presets {
        /// Print compact JSON. Pretty JSON is the default.
        #[arg(long)]
        json: bool,
    },
}

/// `vam agent permissions web <agent> on|off`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum WebState {
    On,
    Off,
}

pub async fn run_permissions(c: &Client, cmd: PermissionsCmd) -> Result<()> {
    match cmd {
        PermissionsCmd::Get {
            agent,
            version,
            effective,
            json,
        } => run_get(c, &agent, version.as_deref(), effective, json).await,
        PermissionsCmd::Set {
            agent,
            file,
            idempotency_key,
        } => run_set(c, &agent, &file, idempotency_key.as_deref()).await,
        PermissionsCmd::Web { agent, state } => run_web(c, &agent, state).await,
        PermissionsCmd::Presets { json } => run_presets(c, json).await,
    }
}

pub async fn run_explain(
    c: &Client,
    agent: &str,
    tool: Option<&str>,
    host: Option<&str>,
    version: Option<&str>,
    json: bool,
) -> Result<()> {
    let tool = blank(tool);
    let host = blank(host);
    match (tool, host) {
        (None, None) => {
            return Err(fail(
                ExitCode::Usage,
                "pass a tool name or --host <HOST>: vam policy explain <AGENT> <TOOL>",
            ));
        }
        (Some(_), Some(_)) => {
            return Err(fail(
                ExitCode::Usage,
                "pass a tool name or --host, not both",
            ));
        }
        _ => {}
    }
    let urn = resolve_agent(c, agent).await?;
    let resolved = resolve_view_version(c, &urn, version).await?;
    let mut path = format!(
        "/v1/agents/{}/permissions/explain",
        encode_path_segment(&urn)
    );
    let mut first = true;
    if let Some(tool) = tool {
        push_query(&mut path, "tool", tool, &mut first);
    }
    if let Some(host) = host {
        push_query(&mut path, "host", host, &mut first);
    }
    if let Some(version) = resolved.as_deref() {
        push_query(&mut path, "version", version, &mut first);
    }
    let body = c.get(&path).await?;
    if json || output::is_json() {
        client::print_json(&body);
        return Ok(());
    }
    let parsed: Value = serde_json::from_str(&body).context("parse explain response")?;
    println!("{}", render_explain(&parsed));
    Ok(())
}

pub async fn run_migrate(
    c: &Client,
    agent: &str,
    force: bool,
    idempotency_key: Option<&str>,
    json: bool,
) -> Result<()> {
    let urn = resolve_agent(c, agent).await?;
    let mut path = format!(
        "/v1/agents/{}/permissions/migrate",
        encode_path_segment(&urn)
    );
    if force {
        let mut first = true;
        push_query(&mut path, "force", "true", &mut first);
    }
    let reply = c
        .exchange(reqwest::Method::POST, &path, Some("{}"), idempotency_key)
        .await?;
    if !(200..300).contains(&reply.status) {
        return Err(migrate_error(&path, reply.status, &reply.body));
    }
    if json || output::is_json() {
        client::print_json(&reply.body);
    }
    let report: Value = serde_json::from_str(&reply.body).context("parse migration report")?;
    if !(json || output::is_json()) {
        println!("{}", render_migration(&report));
    }
    if !force && list_nonempty(&report["unmapped"]) {
        let names = join_strings(&report["unmapped"]);
        return Err(fail(
            ExitCode::Conflict,
            format!(
                "cannot migrate agent '{urn}': unmapped tools {names}; retry with --force to drop them"
            ),
        ));
    }
    Ok(())
}

async fn run_get(
    c: &Client,
    agent: &str,
    version: Option<&str>,
    effective: bool,
    json: bool,
) -> Result<()> {
    if version.is_some() && !effective {
        return Err(fail(
            ExitCode::Usage,
            "`--version` applies to the effective table. Pass --effective, or omit --version to print your draft as JSON.",
        ));
    }
    let urn = resolve_agent(c, agent).await?;
    if !effective {
        let path = format!("/v1/agents/{}/permissions", encode_path_segment(&urn));
        let body = c.get(&path).await?;
        emit_json(&body, json);
        return Ok(());
    }
    let resolved = resolve_view_version(c, &urn, version).await?;
    let mut path = format!(
        "/v1/agents/{}/permissions/effective",
        encode_path_segment(&urn)
    );
    if let Some(version) = resolved.as_deref() {
        push_query(&mut path, "version", version, &mut true);
    }
    let body = c.get(&path).await?;
    if json || output::is_json() {
        client::print_json(&body);
        return Ok(());
    }
    let parsed: Value = serde_json::from_str(&body).context("parse effective permissions")?;
    println!("{}", render_effective(&parsed));
    Ok(())
}

async fn run_set(c: &Client, agent: &str, file: &str, idempotency_key: Option<&str>) -> Result<()> {
    let body = read_permissions_json(file)?;
    let urn = resolve_agent(c, agent).await?;
    let path = format!("/v1/agents/{}/permissions", encode_path_segment(&urn));
    let reply = c
        .exchange(reqwest::Method::PUT, &path, Some(&body), idempotency_key)
        .await?;
    if !(200..300).contains(&reply.status) {
        return Err(permissions_error("PUT", &path, reply.status, &reply.body));
    }
    emit_json(&reply.body, false);
    Ok(())
}

/// Set `web.enabled` on the owner's draft and leave every other field as it
/// was. Off removes the key (absent means off).
async fn run_web(c: &Client, agent: &str, state: WebState) -> Result<()> {
    let urn = resolve_agent(c, agent).await?;
    let path = format!("/v1/agents/{}/permissions", encode_path_segment(&urn));
    let body = c.get(&path).await?;
    let mut permissions: Value = serde_json::from_str(&body).context("parse draft permissions")?;
    apply_web_state(&mut permissions, state);
    let reply = c
        .exchange(
            reqwest::Method::PUT,
            &path,
            Some(&permissions.to_string()),
            None,
        )
        .await?;
    if reply.status == 409 {
        return Err(fail_http(
            ExitCode::Conflict,
            format!(
                "agent '{urn}' still uses a legacy policy. Convert it first with \
                 `vam agent migrate {agent}`, then run this again."
            ),
            reply.status,
        ));
    }
    if !(200..300).contains(&reply.status) {
        return Err(permissions_error("PUT", &path, reply.status, &reply.body));
    }
    let word = if state == WebState::On { "on" } else { "off" };
    println!("web {word} in the draft of {urn}. Release the agent to apply it to a version.");
    Ok(())
}

fn apply_web_state(permissions: &mut Value, state: WebState) {
    if !permissions.is_object() {
        *permissions = Value::Object(Default::default());
    }
    if let Some(map) = permissions.as_object_mut() {
        match state {
            WebState::On => {
                map.insert("web".into(), serde_json::json!({ "enabled": true }));
            }
            WebState::Off => {
                map.remove("web");
            }
        }
    }
}

async fn run_presets(c: &Client, json: bool) -> Result<()> {
    let body = c.get("/v1/permission-presets").await?;
    emit_json(&body, json);
    Ok(())
}

fn emit_json(body: &str, json: bool) {
    if json || output::is_json() {
        if let Ok(value) = serde_json::from_str::<Value>(body) {
            println!("{value}");
            return;
        }
        println!("{}", serde_json::json!({ "raw": body }));
        return;
    }
    client::print_json(body);
}

/// AX-5.1 validation text is printed unchanged. Other HTTP errors use the
/// shared Wrong / Expected / Fix shape.
fn permissions_error(method: &str, path: &str, status: u16, body: &str) -> anyhow::Error {
    if matches!(status, 400 | 422) {
        if let Some(message) = json_message(body) {
            if message.starts_with("Validation failed") {
                return fail_http(ExitCode::Invalid, message, status);
            }
        }
    }
    http_error::failure(method, path, status, body)
}

fn migrate_error(path: &str, status: u16, body: &str) -> anyhow::Error {
    if status == 409 {
        if let Some(message) = json_message(body) {
            if message.contains("unmapped") {
                let message = message.replace("?force=true", "--force");
                return fail_http(ExitCode::Conflict, message, status);
            }
        }
    }
    http_error::failure("POST", path, status, body)
}

fn json_message(body: &str) -> Option<String> {
    let value: Value = serde_json::from_str(body).ok()?;
    let message = value.get("message")?.as_str()?.trim();
    if message.is_empty() {
        None
    } else {
        Some(message.to_string())
    }
}

fn read_permissions_json(file: &str) -> Result<String> {
    if file == "-" {
        // Stdin is the permissions document. This does not prompt, so a pipe works.
        read_permissions_from(std::io::stdin())
    } else {
        let raw = fs::read_to_string(file).with_context(|| format!("read {file}"))?;
        accept_permissions_json(&raw)
    }
}

fn read_permissions_from(mut reader: impl Read) -> Result<String> {
    let mut raw = String::new();
    reader
        .read_to_string(&mut raw)
        .context("read permissions JSON from stdin")?;
    accept_permissions_json(&raw)
}

fn accept_permissions_json(raw: &str) -> Result<String> {
    let raw = raw.trim_start_matches('\u{feff}').trim();
    if raw.is_empty() {
        return Err(fail(ExitCode::Invalid, "permissions JSON is empty"));
    }
    serde_json::from_str::<Value>(raw)
        .map_err(|err| fail(ExitCode::Invalid, format!("permissions JSON: {err}")))?;
    Ok(raw.to_string())
}

async fn resolve_agent(c: &Client, agent: &str) -> Result<String> {
    let agent = agent.trim();
    if agent.is_empty() {
        return Err(fail(ExitCode::Usage, "agent name or URN is required"));
    }
    if agent.starts_with("urn:") {
        return Ok(agent.to_string());
    }
    let body = c.get("/v1/agents").await?;
    let parsed: Value = serde_json::from_str(&body).context("parse agent list")?;
    let urns = agent_urns(&parsed);
    let mut matches: Vec<&str> = urns
        .iter()
        .filter(|urn| urn.as_str() == agent || slug(urn) == agent)
        .map(String::as_str)
        .collect();
    matches.sort_unstable();
    matches.dedup();
    match matches.as_slice() {
        [] => Err(fail(
            ExitCode::NotFound,
            format!("no installed agent matches '{agent}'. Run `vam agent ls`, or pass a URN."),
        )),
        [urn] => Ok((*urn).to_string()),
        many => Err(fail(
            ExitCode::Usage,
            format!(
                "'{agent}' matches more than one agent:\n  {}\nPass the URN.",
                many.join("\n  ")
            ),
        )),
    }
}

fn agent_urns(parsed: &Value) -> Vec<String> {
    let mut urns = Vec::new();
    let mut push = |urn: &str| {
        let urn = urn.trim();
        if urn.is_empty() || urns.iter().any(|existing| existing == urn) {
            return;
        }
        urns.push(urn.to_string());
    };
    for spec in parsed
        .get("specs")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(urn) = urn_of(spec) {
            push(urn);
        }
    }
    for record in parsed
        .get("records")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(urn) = record.get("spec").and_then(urn_of) {
            push(urn);
        }
    }
    urns
}

fn urn_of(spec: &Value) -> Option<&str> {
    spec.get("resourceId")
        .or_else(|| spec.get("resource_id"))
        .and_then(|id| id.get("urn"))
        .and_then(Value::as_str)
}

fn slug(urn: &str) -> &str {
    urn.rsplit('/')
        .find(|segment| !segment.is_empty())
        .unwrap_or(urn)
}

/// `None` means the owner's draft (omit `version` on the request).
async fn resolve_view_version(
    c: &Client,
    urn: &str,
    version: Option<&str>,
) -> Result<Option<String>> {
    if let Some(raw) = blank(version) {
        parse_version_triplet(raw).map_err(|_| {
            fail(
                ExitCode::Usage,
                format!("version must be X.Y.Z, got '{raw}'"),
            )
        })?;
        return Ok(Some(raw.to_string()));
    }
    let path = format!("/v1/agents/{}/versions", encode_path_segment(urn));
    let body = c.get(&path).await?;
    let parsed: Value = serde_json::from_str(&body).context("parse version list")?;
    Ok(select_version(&parsed))
}

fn select_version(parsed: &Value) -> Option<String> {
    let versions = parsed.get("versions")?.as_array()?;
    if versions.iter().any(is_draft_entry) {
        return None;
    }
    latest_release(versions)
}

fn is_draft_entry(entry: &Value) -> bool {
    if entry
        .get("version")
        .and_then(|version| version.get("prerelease"))
        .and_then(Value::as_str)
        == Some("draft")
    {
        return true;
    }
    match entry.get("channel").and_then(Value::as_str) {
        Some(channel) => {
            let channel = channel.to_ascii_uppercase();
            channel == "DRAFT" || channel.ends_with("_DRAFT")
        }
        None => false,
    }
}

fn latest_release(versions: &[Value]) -> Option<String> {
    let mut best: Option<(u64, u64, u64)> = None;
    for entry in versions {
        if is_draft_entry(entry) {
            continue;
        }
        let Some(version) = entry.get("version") else {
            continue;
        };
        if version.is_null() {
            continue;
        }
        let triple = (
            version.get("major").and_then(Value::as_u64).unwrap_or(0),
            version.get("minor").and_then(Value::as_u64).unwrap_or(0),
            version.get("patch").and_then(Value::as_u64).unwrap_or(0),
        );
        if best.is_none_or(|current| triple > current) {
            best = Some(triple);
        }
    }
    best.map(|(major, minor, patch)| format!("{major}.{minor}.{patch}"))
}

fn push_query(path: &mut String, key: &str, value: &str, first: &mut bool) {
    path.push(if *first { '?' } else { '&' });
    *first = false;
    path.push_str(key);
    path.push('=');
    path.push_str(&encode_path_segment(value));
}

fn blank(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

/// §IV.9 tool block or host line.
fn render_explain(value: &Value) -> String {
    let kind = value.get("kind").and_then(Value::as_str).unwrap_or("");
    if kind == "host" {
        return render_host(value);
    }
    if kind == "tool"
        || value.get("toolName").and_then(Value::as_str).is_some()
        || value
            .get("scopes")
            .and_then(Value::as_array)
            .is_some_and(|rows| !rows.is_empty())
    {
        return render_tool(value);
    }
    if value.get("host").and_then(Value::as_str).is_some() {
        return render_host(value);
    }
    value
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn render_tool(value: &Value) -> String {
    let name = value.get("toolName").and_then(Value::as_str).unwrap_or("-");
    let target = value.get("target").and_then(Value::as_str).unwrap_or("-");
    let decision = value
        .get("decision")
        .and_then(Value::as_str)
        .or_else(|| {
            value
                .get("deciding")
                .and_then(|row| row.get("decision"))
                .and_then(Value::as_str)
        })
        .unwrap_or("deny");
    let mut lines = vec![format!("{:<6}{}  ({})", heading(decision), name, target)];
    let deciding_scope = value
        .get("deciding")
        .and_then(|row| row.get("scope"))
        .and_then(Value::as_str)
        .unwrap_or("");
    if let Some(rows) = value.get("scopes").and_then(Value::as_array) {
        for row in rows {
            lines.push(render_scope_row(row, deciding_scope));
        }
    }
    lines.join("\n")
}

fn render_scope_row(row: &Value, deciding_scope: &str) -> String {
    let scope = row.get("scope").and_then(Value::as_str).unwrap_or("");
    let scope = format!("{scope:<6}");
    let decision = row
        .get("decision")
        .and_then(Value::as_str)
        .unwrap_or("none")
        .to_ascii_lowercase();
    if decision.is_empty() || decision == "none" {
        return format!("  {scope} none");
    }
    let decision = format!("{decision:<5}");
    let rule = row.get("rule").and_then(Value::as_str).unwrap_or("");
    let origin = row.get("origin").and_then(Value::as_str).unwrap_or("");
    let marker = if row_decides(row, deciding_scope) {
        "     <- decides"
    } else {
        ""
    };
    format!("  {scope} {decision}   {rule}  ({origin}){marker}")
}

fn row_decides(row: &Value, deciding_scope: &str) -> bool {
    match row.get("deciding").and_then(Value::as_bool) {
        Some(flag) => flag,
        None => row.get("scope").and_then(Value::as_str) == Some(deciding_scope),
    }
}

fn render_host(value: &Value) -> String {
    let decision = value
        .get("decision")
        .and_then(Value::as_str)
        .or_else(|| {
            value
                .get("deciding")
                .and_then(|row| row.get("decision"))
                .and_then(Value::as_str)
        })
        .unwrap_or("deny");
    let host = value.get("host").and_then(Value::as_str).unwrap_or("-");
    let rule = value
        .get("deciding")
        .and_then(|row| row.get("rule"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let origin = value
        .get("deciding")
        .and_then(|row| row.get("origin"))
        .and_then(Value::as_str)
        .unwrap_or("");
    format!(
        "{:<6}{host}   agent rule {rule} at {origin}",
        heading(decision)
    )
}

fn heading(decision: &str) -> &'static str {
    match decision.to_ascii_lowercase().as_str() {
        "allow" => "ALLOW",
        "ask" | "hitl" => "ASK",
        _ => "DENY",
    }
}

fn render_effective(value: &Value) -> String {
    let version = value.get("version").and_then(Value::as_str).unwrap_or("-");
    let mut out = format!("version  {version}\n\n");
    out.push_str(&format!(
        "{:<22} {:<8} {:<22} {}\n",
        "TOOL", "DECISION", "SOURCE", "LABEL"
    ));
    for tool in value
        .get("tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let name = tool.get("toolName").and_then(Value::as_str).unwrap_or("-");
        let decision = heading(tool.get("decision").and_then(Value::as_str).unwrap_or("-"));
        let source = tool.get("source").and_then(Value::as_str).unwrap_or("-");
        let label = effective_label(tool);
        out.push_str(&format!("{name:<22} {decision:<8} {source:<22} {label}\n"));
    }
    if let Some(web) = value.get("web").filter(|item| !item.is_null()) {
        let on = web.get("enabled").and_then(Value::as_bool) == Some(true);
        out.push_str(&format!("\nWEB  {}\n", if on { "on" } else { "off" }));
        if let Some(note) = web
            .get("note")
            .and_then(Value::as_str)
            .filter(|note| !note.is_empty())
        {
            out.push_str(&format!("  {note}\n"));
        }
    }
    if let Some(internet) = value.get("internet").filter(|item| !item.is_null()) {
        let default = heading(
            internet
                .get("default")
                .and_then(Value::as_str)
                .unwrap_or("-"),
        );
        out.push_str(&format!("\nINTERNET  {default}\n"));
        for rule in internet
            .get("rules")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let target = rule.get("target").and_then(Value::as_str).unwrap_or("-");
            let decision = heading(rule.get("decision").and_then(Value::as_str).unwrap_or("-"));
            out.push_str(&format!("  {decision:<8} {target}\n"));
        }
    }
    out.trim_end().to_string()
}

fn effective_label(tool: &Value) -> String {
    let label = tool
        .get("defaultLabel")
        .and_then(Value::as_str)
        .unwrap_or("");
    if tool.get("isDefault").and_then(Value::as_bool) == Some(true) && !label.is_empty() {
        return label.to_string();
    }
    if !label.is_empty() {
        return label.to_string();
    }
    tool.get("decidingRule")
        .and_then(Value::as_str)
        .unwrap_or("-")
        .to_string()
}

fn render_migration(report: &Value) -> String {
    let mut out = String::new();
    push_list(&mut out, "Converted", &report["converted"]);
    push_list(
        &mut out,
        "Kept as advanced",
        report
            .get("keptAdvanced")
            .or_else(|| report.get("kept_advanced"))
            .unwrap_or(&Value::Null),
    );
    push_list(&mut out, "Unmapped", &report["unmapped"]);
    push_list(
        &mut out,
        "Behaviour changes",
        report
            .get("behaviourChanges")
            .or_else(|| report.get("behaviour_changes"))
            .unwrap_or(&Value::Null),
    );
    out.trim_end().to_string()
}

fn push_list(out: &mut String, title: &str, value: &Value) {
    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str(title);
    out.push_str(":\n");
    let items = string_list(value);
    if items.is_empty() {
        out.push_str("  (none)\n");
        return;
    }
    for item in items {
        out.push_str("  ");
        out.push_str(&item);
        out.push('\n');
    }
}

fn string_list(value: &Value) -> Vec<String> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| item.as_str().map(str::to_string))
        .collect()
}

fn list_nonempty(value: &Value) -> bool {
    value.as_array().is_some_and(|items| {
        items
            .iter()
            .any(|item| item.as_str().is_some_and(|s| !s.is_empty()))
    })
}

fn join_strings(value: &Value) -> String {
    string_list(value).join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::{Method, MockServer};
    use std::io::Cursor;

    const URN: &str = "urn:vam:agent:local.vam.dev/acme/jira-assistant";

    const TOOL_EXPLAIN: &str = r#"{
        "urn": "urn:vam:agent:local.vam.dev/acme/jira-assistant",
        "version": "1.3.0",
        "kind": "tool",
        "toolName": "jira_create_issue",
        "target": "skill:jira/create_issue",
        "decision": "ask",
        "deciding": {
            "scope": "source",
            "decision": "ask",
            "rule": "approvalRequired",
            "origin": "skill jira v2.1.0",
            "deciding": true
        },
        "scopes": [
            {"scope": "agent", "decision": "allow", "rule": "skill:jira/*", "origin": "agent v1.3.0", "deciding": false},
            {"scope": "source", "decision": "ask", "rule": "approvalRequired", "origin": "skill jira v2.1.0", "deciding": true},
            {"scope": "org", "decision": "none", "rule": "", "origin": "", "deciding": false}
        ]
    }"#;

    const HOST_EXPLAIN: &str = r#"{
        "kind": "host",
        "host": "pastebin.com",
        "decision": "deny",
        "deciding": {
            "scope": "agent",
            "decision": "deny",
            "rule": "*.pastebin.com",
            "origin": "agent v1.3.0",
            "deciding": true
        }
    }"#;

    const TOOL_GOLDEN: &str = "\
ASK   jira_create_issue  (skill:jira/create_issue)
  agent  allow   skill:jira/*  (agent v1.3.0)
  source ask     approvalRequired  (skill jira v2.1.0)     <- decides
  org    none";

    const HOST_GOLDEN: &str = "DENY  pastebin.com   agent rule *.pastebin.com at agent v1.3.0";

    #[test]
    fn explain_tool_matches_the_design_example() {
        let value: Value = serde_json::from_str(TOOL_EXPLAIN).unwrap();
        assert_eq!(render_explain(&value), TOOL_GOLDEN);
    }

    #[test]
    fn explain_host_matches_the_design_example() {
        let value: Value = serde_json::from_str(HOST_EXPLAIN).unwrap();
        assert_eq!(render_explain(&value), HOST_GOLDEN);
    }

    #[test]
    fn effective_table_shows_an_mcp_tool_as_ask_with_the_default_label() {
        let value: Value = serde_json::from_str(
            r#"{
                "version": "draft",
                "tools": [{
                    "toolName": "linear_create_issue",
                    "target": "mcp:linear.example/create_issue",
                    "source": "mcp linear.example",
                    "decision": "ask",
                    "decidingRule": "tools:* default bySideEffect (unspecified)",
                    "isDefault": true,
                    "defaultLabel": "default (unspecified)"
                }]
            }"#,
        )
        .unwrap();
        let rendered = render_effective(&value);
        let expected = "\
version  draft

TOOL                   DECISION SOURCE                 LABEL
linear_create_issue    ASK      mcp linear.example     default (unspecified)";
        assert_eq!(rendered, expected);
        assert!(rendered.contains("ASK"), "{rendered}");
        assert!(rendered.contains("default (unspecified)"), "{rendered}");
    }

    #[test]
    fn migration_report_names_each_section() {
        let value: Value = serde_json::from_str(
            r#"{
                "converted": ["skill:jira/create_issue"],
                "keptAdvanced": ["urn:vam:policy:local.vam.dev/acme/legacy"],
                "unmapped": ["old_tool"],
                "behaviourChanges": ["Internet is limited to the sites this agent's secrets already allowed."]
            }"#,
        )
        .unwrap();
        assert_eq!(
            render_migration(&value),
            "\
Converted:
  skill:jira/create_issue

Kept as advanced:
  urn:vam:policy:local.vam.dev/acme/legacy

Unmapped:
  old_tool

Behaviour changes:
  Internet is limited to the sites this agent's secrets already allowed."
        );
    }

    #[test]
    fn stdin_permissions_json_does_not_require_a_tty() {
        let raw =
            read_permissions_from(Cursor::new(r#"{"tools":{"default":"bySideEffect"}}"#)).unwrap();
        assert!(raw.contains("bySideEffect"));
    }

    #[test]
    fn draft_wins_over_a_newer_release_when_version_is_omitted() {
        let parsed: Value = serde_json::from_str(
            r#"{"versions":[
                {"version":{"prerelease":"draft"},"channel":"DRAFT"},
                {"version":{"major":1,"minor":3},"channel":"RELEASED"}
            ]}"#,
        )
        .unwrap();
        assert_eq!(select_version(&parsed), None);
    }

    #[test]
    fn latest_release_is_used_when_there_is_no_draft() {
        let parsed: Value = serde_json::from_str(
            r#"{"versions":[
                {"version":{"major":1},"channel":"RELEASED"},
                {"version":{"major":1,"minor":3},"channel":"RELEASED"}
            ]}"#,
        )
        .unwrap();
        assert_eq!(select_version(&parsed).as_deref(), Some("1.3.0"));
    }

    fn versions_body(draft: bool) -> String {
        if draft {
            r#"{"versions":[
                {"version":{"prerelease":"draft"},"channel":"DRAFT"},
                {"version":{"major":1,"minor":2},"channel":"RELEASED"}
            ]}"#
            .into()
        } else {
            r#"{"versions":[
                {"version":{"major":1},"channel":"RELEASED"},
                {"version":{"major":1,"minor":3},"channel":"RELEASED"}
            ]}"#
            .into()
        }
    }

    fn explain_path() -> String {
        format!(
            "/v1/agents/{}/permissions/explain",
            encode_path_segment(URN)
        )
    }

    #[tokio::test]
    async fn explain_without_a_tool_or_host_is_usage() {
        let result = crate::cmd::catalog::run_policy(
            Some("http://127.0.0.1:9".into()),
            Some("vam_sk_test".into()),
            crate::cmd::catalog::PolicyCmd::Explain {
                agent: URN.into(),
                tool: None,
                host: None,
                version: None,
                json: false,
            },
        )
        .await;
        let err = result.expect_err("tool or host is required");
        assert_eq!(output::classify(&err).0, ExitCode::Usage);
    }

    #[tokio::test]
    async fn version_without_effective_is_usage() {
        let result = crate::cmd::catalog::run_agent(
            Some("http://127.0.0.1:9".into()),
            Some("vam_sk_test".into()),
            crate::cmd::catalog::AgentCmd::Permissions {
                command: PermissionsCmd::Get {
                    agent: URN.into(),
                    version: Some("1.2.3".into()),
                    effective: false,
                    json: false,
                },
            },
        )
        .await;
        let err = result.expect_err("version needs --effective");
        assert_eq!(output::classify(&err).0, ExitCode::Usage);
    }

    #[tokio::test]
    async fn explain_tool_uses_the_draft_when_you_own_one() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(Method::GET)
                .path(format!("/v1/agents/{}/versions", encode_path_segment(URN)));
            then.status(200)
                .header("content-type", "application/json")
                .body(versions_body(true));
        });
        let explain = server.mock(|when, then| {
            when.method(Method::GET)
                .path(explain_path())
                .query_param("tool", "jira_create_issue")
                .matches(|req| {
                    req.query_params
                        .as_ref()
                        .is_none_or(|params| params.iter().all(|(key, _)| key != "version"))
                });
            then.status(200)
                .header("content-type", "application/json")
                .body(TOOL_EXPLAIN);
        });
        let result = crate::cmd::catalog::run_policy(
            Some(server.base_url()),
            Some("vam_sk_test".into()),
            crate::cmd::catalog::PolicyCmd::Explain {
                agent: URN.into(),
                tool: Some("jira_create_issue".into()),
                host: None,
                version: None,
                json: false,
            },
        )
        .await;
        assert!(result.is_ok(), "{result:?}");
        explain.assert();
    }

    #[tokio::test]
    async fn explain_host_uses_the_latest_release_without_a_draft() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(Method::GET)
                .path(format!("/v1/agents/{}/versions", encode_path_segment(URN)));
            then.status(200)
                .header("content-type", "application/json")
                .body(versions_body(false));
        });
        let explain = server.mock(|when, then| {
            when.method(Method::GET)
                .path(explain_path())
                .query_param("host", "pastebin.com")
                .query_param("version", "1.3.0");
            then.status(200)
                .header("content-type", "application/json")
                .body(HOST_EXPLAIN);
        });
        let result = crate::cmd::catalog::run_policy(
            Some(server.base_url()),
            Some("vam_sk_test".into()),
            crate::cmd::catalog::PolicyCmd::Explain {
                agent: URN.into(),
                tool: None,
                host: Some("pastebin.com".into()),
                version: None,
                json: true,
            },
        )
        .await;
        assert!(result.is_ok(), "{result:?}");
        explain.assert();
    }

    #[tokio::test]
    async fn explain_version_flag_skips_the_version_list() {
        let server = MockServer::start();
        let explain = server.mock(|when, then| {
            when.method(Method::GET)
                .path(explain_path())
                .query_param("tool", "jira_create_issue")
                .query_param("version", "1.2.3");
            then.status(200)
                .header("content-type", "application/json")
                .body(TOOL_EXPLAIN);
        });
        let result = crate::cmd::catalog::run_policy(
            Some(server.base_url()),
            Some("vam_sk_test".into()),
            crate::cmd::catalog::PolicyCmd::Explain {
                agent: URN.into(),
                tool: Some("jira_create_issue".into()),
                host: None,
                version: Some("1.2.3".into()),
                json: false,
            },
        )
        .await;
        assert!(result.is_ok(), "{result:?}");
        explain.assert();
    }

    #[tokio::test]
    async fn explain_short_name_resolves_to_the_urn() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(Method::GET).path("/v1/agents");
            then.status(200)
                .header("content-type", "application/json")
                .body(format!(
                    r#"{{"specs":[{{"resourceId":{{"urn":"{URN}"}}}}]}}"#
                ));
        });
        server.mock(|when, then| {
            when.method(Method::GET)
                .path(format!("/v1/agents/{}/versions", encode_path_segment(URN)));
            then.status(200)
                .header("content-type", "application/json")
                .body(versions_body(true));
        });
        let explain = server.mock(|when, then| {
            when.method(Method::GET)
                .path(explain_path())
                .query_param("tool", "jira_create_issue");
            then.status(200)
                .header("content-type", "application/json")
                .body(TOOL_EXPLAIN);
        });
        let result = crate::cmd::catalog::run_policy(
            Some(server.base_url()),
            Some("vam_sk_test".into()),
            crate::cmd::catalog::PolicyCmd::Explain {
                agent: "jira-assistant".into(),
                tool: Some("jira_create_issue".into()),
                host: None,
                version: None,
                json: false,
            },
        )
        .await;
        assert!(result.is_ok(), "{result:?}");
        explain.assert();
    }

    #[tokio::test]
    async fn permissions_set_prints_validation_errors_verbatim_and_exits_invalid() {
        let server = MockServer::start();
        let message = "Validation failed with 1 violation(s):\n  - permissions.tools.rules[0].target: 'skill:nope' is not a tool target (constraint: target)";
        let put = server.mock(|when, then| {
            when.method(Method::PUT)
                .path(format!(
                    "/v1/agents/{}/permissions",
                    encode_path_segment(URN)
                ))
                .header_exists("Idempotency-Key")
                .body_contains("skill:nope");
            then.status(400)
                .header("content-type", "application/json")
                .body(
                    serde_json::json!({
                        "code": "InvalidArgument",
                        "message": message,
                        "retryable": false
                    })
                    .to_string(),
                );
        });
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            file.path(),
            r#"{"tools":{"rules":[{"target":"skill:nope","decision":"allow"}]}}"#,
        )
        .unwrap();
        let result = crate::cmd::catalog::run_agent(
            Some(server.base_url()),
            Some("vam_sk_test".into()),
            crate::cmd::catalog::AgentCmd::Permissions {
                command: PermissionsCmd::Set {
                    agent: URN.into(),
                    file: file.path().display().to_string(),
                    idempotency_key: Some("perm-set".into()),
                },
            },
        )
        .await;
        let err = result.expect_err("validation must fail");
        assert_eq!(err.to_string(), message);
        assert_eq!(output::classify(&err), (ExitCode::Invalid, Some(400)));
        put.assert();
    }

    #[tokio::test]
    async fn migrate_unmapped_names_exit_conflict() {
        let server = MockServer::start();
        let message = format!(
            "cannot migrate agent '{URN}': unmapped tools ghost_tool; retry with ?force=true to drop them"
        );
        server.mock(|when, then| {
            when.method(Method::POST).path(format!(
                "/v1/agents/{}/permissions/migrate",
                encode_path_segment(URN)
            ));
            then.status(409)
                .header("content-type", "application/json")
                .body(
                    serde_json::json!({
                        "code": "AlreadyExists",
                        "message": message,
                        "retryable": false
                    })
                    .to_string(),
                );
        });
        let result = crate::cmd::catalog::run_agent(
            Some(server.base_url()),
            Some("vam_sk_test".into()),
            crate::cmd::catalog::AgentCmd::Migrate {
                agent: URN.into(),
                force: false,
                idempotency_key: None,
                json: false,
            },
        )
        .await;
        let err = result.expect_err("unmapped tools block migration");
        assert_eq!(output::classify(&err).0, ExitCode::Conflict);
        let text = err.to_string();
        assert!(text.contains("ghost_tool"), "{text}");
        assert!(text.contains("--force"), "{text}");
        assert!(!text.contains("?force=true"), "{text}");
    }

    #[tokio::test]
    async fn migrate_force_prints_the_report_and_exits_ok() {
        let server = MockServer::start();
        let post = server.mock(|when, then| {
            when.method(Method::POST)
                .path(format!(
                    "/v1/agents/{}/permissions/migrate",
                    encode_path_segment(URN)
                ))
                .query_param("force", "true");
            then.status(200)
                .header("content-type", "application/json")
                .body(
                    r#"{"converted":["builtin:read_file"],"keptAdvanced":[],"unmapped":["ghost_tool"],"behaviourChanges":["Internet stays closed."]}"#,
                );
        });
        let result = crate::cmd::catalog::run_agent(
            Some(server.base_url()),
            Some("vam_sk_test".into()),
            crate::cmd::catalog::AgentCmd::Migrate {
                agent: URN.into(),
                force: true,
                idempotency_key: None,
                json: true,
            },
        )
        .await;
        assert!(result.is_ok(), "{result:?}");
        post.assert();
    }

    #[tokio::test]
    async fn effective_get_requests_the_effective_table() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(Method::GET)
                .path(format!("/v1/agents/{}/versions", encode_path_segment(URN)));
            then.status(200)
                .header("content-type", "application/json")
                .body(versions_body(true));
        });
        let get = server.mock(|when, then| {
            when.method(Method::GET)
                .path(format!(
                    "/v1/agents/{}/permissions/effective",
                    encode_path_segment(URN)
                ))
                .matches(|req| {
                    req.query_params.as_ref().is_none_or(|params| {
                        params.iter().all(|(key, _)| key != "version")
                    })
                });
            then.status(200)
                .header("content-type", "application/json")
                .body(
                    r#"{"version":"draft","tools":[{"toolName":"linear_create_issue","source":"mcp linear.example","decision":"ask","isDefault":true,"defaultLabel":"default (unspecified)"}]}"#,
                );
        });
        let result = crate::cmd::catalog::run_agent(
            Some(server.base_url()),
            Some("vam_sk_test".into()),
            crate::cmd::catalog::AgentCmd::Permissions {
                command: PermissionsCmd::Get {
                    agent: URN.into(),
                    version: None,
                    effective: true,
                    json: false,
                },
            },
        )
        .await;
        assert!(result.is_ok(), "{result:?}");
        get.assert();
    }

    #[test]
    fn web_state_edits_only_the_web_key() {
        let mut permissions = serde_json::json!({"tools": {"default": "TOOL_DEFAULT_ASK"}});
        apply_web_state(&mut permissions, WebState::On);
        assert_eq!(permissions["web"]["enabled"], true);
        assert_eq!(permissions["tools"]["default"], "TOOL_DEFAULT_ASK");
        apply_web_state(&mut permissions, WebState::Off);
        assert!(permissions.get("web").is_none());
        assert_eq!(permissions["tools"]["default"], "TOOL_DEFAULT_ASK");
        let mut empty = Value::Null;
        apply_web_state(&mut empty, WebState::On);
        assert_eq!(empty["web"]["enabled"], true);
    }

    #[test]
    fn effective_table_shows_web_on_and_off() {
        let on: Value = serde_json::from_str(
            r#"{"version":"draft","tools":[],"web":{"enabled":true,"available":false,"note":"web is on, but no web broker is registered on this node"}}"#,
        )
        .unwrap();
        let text = render_effective(&on);
        assert!(text.contains("WEB  on"), "{text}");
        assert!(text.contains("no web broker is registered"), "{text}");
        let off: Value =
            serde_json::from_str(r#"{"version":"1.0.0","tools":[],"web":{"enabled":false}}"#)
                .unwrap();
        assert!(render_effective(&off).contains("WEB  off"));
    }

    async fn web_cmd(server: &MockServer, state: WebState) -> Result<()> {
        crate::cmd::catalog::run_agent(
            Some(server.base_url()),
            Some("vam_sk_test".into()),
            crate::cmd::catalog::AgentCmd::Permissions {
                command: PermissionsCmd::Web {
                    agent: URN.into(),
                    state,
                },
            },
        )
        .await
    }

    #[tokio::test]
    async fn web_on_round_trips_the_draft_with_web_enabled() {
        let server = MockServer::start();
        let path = format!("/v1/agents/{}/permissions", encode_path_segment(URN));
        let get = server.mock(|when, then| {
            when.method(Method::GET).path(path.clone());
            then.status(200)
                .header("content-type", "application/json")
                .body(r#"{"tools":{"default":"TOOL_DEFAULT_ASK"}}"#);
        });
        let put = server.mock(|when, then| {
            when.method(Method::PUT).path(path.clone()).matches(|req| {
                let body: Value = serde_json::from_slice(req.body.as_deref().unwrap_or_default())
                    .unwrap_or_default();
                body["web"]["enabled"] == true && body["tools"]["default"] == "TOOL_DEFAULT_ASK"
            });
            then.status(200)
                .header("content-type", "application/json")
                .body(r#"{"web":{"enabled":true}}"#);
        });
        let result = web_cmd(&server, WebState::On).await;
        assert!(result.is_ok(), "{result:?}");
        get.assert();
        put.assert();
    }

    #[tokio::test]
    async fn web_off_drops_the_key() {
        let server = MockServer::start();
        let path = format!("/v1/agents/{}/permissions", encode_path_segment(URN));
        server.mock(|when, then| {
            when.method(Method::GET).path(path.clone());
            then.status(200)
                .header("content-type", "application/json")
                .body(r#"{"web":{"enabled":true}}"#);
        });
        let put = server.mock(|when, then| {
            when.method(Method::PUT).path(path.clone()).matches(|req| {
                let body: Value = serde_json::from_slice(req.body.as_deref().unwrap_or_default())
                    .unwrap_or_default();
                body.get("web").is_none()
            });
            then.status(200)
                .header("content-type", "application/json")
                .body("{}");
        });
        let result = web_cmd(&server, WebState::Off).await;
        assert!(result.is_ok(), "{result:?}");
        put.assert();
    }

    #[tokio::test]
    async fn web_on_for_a_legacy_agent_says_to_convert() {
        let server = MockServer::start();
        let path = format!("/v1/agents/{}/permissions", encode_path_segment(URN));
        server.mock(|when, then| {
            when.method(Method::GET).path(path.clone());
            then.status(200)
                .header("content-type", "application/json")
                .body("{}");
        });
        server.mock(|when, then| {
            when.method(Method::PUT).path(path.clone());
            then.status(409)
                .header("content-type", "application/json")
                .body(r#"{"message":"migrate first"}"#);
        });
        let err = web_cmd(&server, WebState::On).await.unwrap_err();
        assert!(format!("{err:#}").contains("vam agent migrate"), "{err:#}");
    }
}
