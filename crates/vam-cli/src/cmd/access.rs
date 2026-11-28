//! Agent access for members: browse the agents you may ask for, add or request one, leave one,
//! set an agent's access policy, and list your own requests (`vam access mine`). The CLI holds no
//! authority: it calls the access routes and prints what vamd decided.

use anyhow::Result;
use clap::{Args, Subcommand};
use serde_json::{json, Value};

use crate::client::{self, encode_path_segment, resolve_idempotency_key, Client};
use crate::output::{self, fail, ExitCode};

const IDEM_HELP: &str = "Reuse this idempotency key (default: mint a fresh key).";

/// `vam agent browse`, and `vam access mine`: one page, or every page with `--all`.
#[derive(Args)]
pub struct ListArgs {
    /// Follow every page.
    #[arg(long)]
    pub all: bool,
    /// Rows per page (vamd default 50, at most 1000).
    #[arg(long)]
    pub page_size: Option<u32>,
}

/// `vam agent add <agent>` and `vam agent request <agent>`: both call the same route.
#[derive(Args)]
pub struct AskArgs {
    /// An agent URN, or its short name when exactly one agent you may ask for has it.
    pub agent: String,
    /// A note for the agent's admins.
    #[arg(short = 'm', long)]
    pub message: Option<String>,
    #[arg(long, help = IDEM_HELP)]
    pub idempotency_key: Option<String>,
}

#[derive(Args)]
pub struct LeaveArgs {
    /// An agent URN, or its short name.
    pub agent: String,
    #[arg(long, help = IDEM_HELP)]
    pub idempotency_key: Option<String>,
}

#[derive(Args)]
pub struct PolicyArgs {
    /// An agent URN, or its short name.
    pub agent: String,
    /// Who can see the agent.
    #[arg(long, value_parser = ["org", "restricted"])]
    pub visibility: String,
    /// How members get access.
    #[arg(long, value_parser = ["everyone", "open", "approval"])]
    pub mode: String,
    #[arg(long, help = IDEM_HELP)]
    pub idempotency_key: Option<String>,
}

#[derive(Subcommand)]
pub enum AccessCmd {
    /// Your own access requests, newest first: one page, or `--all`.
    Mine(ListArgs),
    /// Requests for the agents you administer (all agents for an org admin); pending by default.
    Requests(RequestsArgs),
    /// Approve a pending request.
    Approve(ApproveArgs),
    /// Deny a pending request; the member sees the reason.
    Deny(DenyArgs),
    /// End a member's approved access to an agent.
    Revoke(RevokeArgs),
}

#[derive(Args)]
pub struct RequestsArgs {
    /// Which requests to list.
    #[arg(long, default_value = "pending", value_parser = ["pending", "approved", "denied", "revoked", "withdrawn", "all"])]
    pub state: String,
    /// Only requests for this agent URN (filtered here, per page).
    #[arg(long)]
    pub agent: Option<String>,
    /// Follow every page.
    #[arg(long)]
    pub all: bool,
    /// Rows per page (vamd default 50, at most 1000).
    #[arg(long)]
    pub page_size: Option<u32>,
}

#[derive(Args)]
pub struct ApproveArgs {
    /// The request id (`vam access requests`).
    pub id: String,
    /// A note the member sees.
    #[arg(long)]
    pub reason: Option<String>,
    #[arg(long, help = IDEM_HELP)]
    pub idempotency_key: Option<String>,
}

#[derive(Args)]
pub struct DenyArgs {
    /// The request id (`vam access requests`).
    pub id: String,
    /// Why; the member sees it.
    #[arg(long)]
    pub reason: String,
    #[arg(long, help = IDEM_HELP)]
    pub idempotency_key: Option<String>,
}

#[derive(Args)]
pub struct RevokeArgs {
    /// An agent URN, or its short name.
    pub agent: String,
    /// A user id, or a username with approved access to the agent.
    pub user: String,
    /// Why; the member sees it.
    #[arg(long)]
    pub reason: String,
    #[arg(long, help = IDEM_HELP)]
    pub idempotency_key: Option<String>,
}

fn list_path(base: &str, page_size: Option<u32>) -> String {
    match page_size {
        Some(n) => format!("{base}?pageSize={n}"),
        None => base.to_string(),
    }
}

/// User text never reaches the terminal with control characters in it.
pub(crate) fn clean(text: &str) -> String {
    text.chars()
        .filter_map(|c| match c {
            ' ' => Some(' '),
            c if c.is_whitespace() => Some(' '),
            c if c.is_control() => None,
            '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{2069}'
            | '\u{FEFF}' => None,
            c => Some(c),
        })
        .collect()
}

fn text<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

/// Left-aligned columns, two spaces apart; cells are cleaned first.
pub(crate) fn table(header: &[&str], rows: &[Vec<String>]) -> String {
    let rows: Vec<Vec<String>> = rows
        .iter()
        .map(|r| r.iter().map(|c| clean(c)).collect())
        .collect();
    let mut widths: Vec<usize> = header.iter().map(|h| h.chars().count()).collect();
    for r in &rows {
        for (i, c) in r.iter().enumerate() {
            widths[i] = widths[i].max(c.chars().count());
        }
    }
    let fmt = |cells: &[String]| {
        let mut line = String::new();
        for (i, c) in cells.iter().enumerate() {
            if i + 1 == cells.len() {
                line.push_str(c);
            } else {
                line.push_str(&format!("{c:<w$}  ", w = widths[i]));
            }
        }
        line.trim_end().to_string()
    };
    let mut out = vec![fmt(&header
        .iter()
        .map(|h| h.to_string())
        .collect::<Vec<_>>())];
    out.extend(rows.iter().map(|r| fmt(r)));
    out.join("\n")
}

/// The browse table; a denied or revoked row carries vamd's reason under it.
pub fn format_browse(body: &str) -> String {
    let Ok(v) = serde_json::from_str::<Value>(body) else {
        return body.to_string();
    };
    let agents = v["agents"].as_array().cloned().unwrap_or_default();
    if agents.is_empty() {
        return "no agents to browse".to_string();
    }
    let mut lines: Vec<String> = Vec::new();
    let mut rows = Vec::new();
    let mut reasons = Vec::new();
    for a in &agents {
        let state = a
            .pointer("/access/state")
            .and_then(Value::as_str)
            .unwrap_or("");
        rows.push(vec![
            text(a, "urn").to_string(),
            text(a, "displayName").to_string(),
            text(a, "latestVersion").to_string(),
            text(a, "mode").to_string(),
            state.to_string(),
            if a["canAdmin"].as_bool().unwrap_or(false) {
                "yes".to_string()
            } else {
                "no".to_string()
            },
        ]);
        let reason = a
            .pointer("/access/reason")
            .and_then(Value::as_str)
            .unwrap_or("");
        reasons.push(
            (matches!(state, "denied" | "revoked") && !reason.is_empty())
                .then(|| format!("    reason: {}", clean(reason))),
        );
    }
    let rendered = table(&["URN", "NAME", "VERSION", "MODE", "STATE", "ADMIN"], &rows);
    let mut it = rendered.lines();
    lines.extend(it.next().map(str::to_string));
    for (line, reason) in it.zip(reasons) {
        lines.push(line.to_string());
        lines.extend(reason);
    }
    lines.join("\n")
}

/// The caller's own rows.
pub fn format_mine(body: &str) -> String {
    let Ok(v) = serde_json::from_str::<Value>(body) else {
        return body.to_string();
    };
    let rows = v["requests"].as_array().cloned().unwrap_or_default();
    if rows.is_empty() {
        return "no access requests".to_string();
    }
    let rows: Vec<Vec<String>> = rows
        .iter()
        .map(|r| {
            vec![
                text(r, "requestId").to_string(),
                text(r, "urn").to_string(),
                text(r, "state").to_string(),
                text(r, "requestedAt").to_string(),
                text(r, "reason").to_string(),
            ]
        })
        .collect();
    table(&["REQUEST", "AGENT", "STATE", "REQUESTED", "REASON"], &rows)
}

/// What vamd decided about `POST .../access`.
pub fn format_decision(body: &str) -> String {
    let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let r = &v["request"];
    let (id, urn) = (clean(text(r, "requestId")), clean(text(r, "urn")));
    match text(r, "state") {
        "approved" => format!("request {id} approved"),
        "pending" => format!("request {id} pending: an admin of {urn} decides"),
        other => format!("request {id} {}", clean(other)),
    }
}

/// Every browse row, all pages.
pub async fn browse_all(c: &Client) -> Result<Vec<Value>> {
    let body = client::list_paged(c, "/v1/catalog/agents?pageSize=1000", "agents", true).await?;
    let v: Value = serde_json::from_str(&body)?;
    Ok(v["agents"].as_array().cloned().unwrap_or_default())
}

fn slug(urn: &str) -> &str {
    urn.rsplit('/').find(|s| !s.is_empty()).unwrap_or(urn)
}

/// The URNs in `rows` named by `name`: the whole URN or its last segment.
pub fn urns_named(rows: &[Value], name: &str) -> Vec<String> {
    let mut urns: Vec<String> = rows
        .iter()
        .filter_map(|a| a.get("urn").and_then(Value::as_str))
        .filter(|urn| *urn == name || slug(urn) == name)
        .map(str::to_string)
        .collect();
    urns.sort();
    urns.dedup();
    urns
}

fn pick(name: &str, urns: Vec<String>) -> Result<String> {
    match urns.as_slice() {
        [] => Err(fail(
            ExitCode::NotFound,
            format!(
                "Wrong: no agent you may ask for matches '{}'.\nExpected: an agent URN, or the short name of one that browse lists.\nFix: vam agent browse.",
                clean(name)
            ),
        )),
        [urn] => Ok(urn.clone()),
        many => Err(fail(
            ExitCode::Usage,
            format!(
                "'{}' matches more than one agent:\n  {}\nPass the URN: vam agent request <urn>",
                clean(name),
                many.iter().map(|u| clean(u)).collect::<Vec<_>>().join("\n  ")
            ),
        )),
    }
}

/// A URN stays as given; any other word is a short name matched through browse (Q2).
pub(crate) async fn resolve_agent(c: &Client, arg: &str) -> Result<String> {
    let arg = arg.trim();
    if arg.starts_with("urn:") {
        return Ok(arg.to_string());
    }
    let rows = browse_all(c).await?;
    pick(arg, urns_named(&rows, arg))
}

fn access_path(urn: &str) -> String {
    format!("/v1/agents/{}/access", encode_path_segment(urn))
}

pub async fn run_browse(c: &Client, args: ListArgs) -> Result<()> {
    let path = list_path("/v1/catalog/agents", args.page_size);
    let body = client::list_paged(c, &path, "agents", args.all).await?;
    if output::is_json() {
        client::print_json(&body);
    } else {
        println!("{}", format_browse(&body));
    }
    Ok(())
}

pub async fn run_ask(c: &Client, args: AskArgs) -> Result<()> {
    let urn = resolve_agent(c, &args.agent).await?;
    let body = match args
        .message
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty())
    {
        Some(m) => json!({ "message": m }).to_string(),
        None => "{}".to_string(),
    };
    let key = resolve_idempotency_key(args.idempotency_key);
    let out = c.post_raw_json(&access_path(&urn), &body, &key).await?;
    if output::is_json() {
        client::print_json(&out);
    } else {
        println!("{}", format_decision(&out));
    }
    Ok(())
}

pub async fn run_leave(c: &Client, args: LeaveArgs) -> Result<()> {
    let urn = resolve_agent(c, &args.agent).await?;
    c.delete(&access_path(&urn), args.idempotency_key.as_deref())
        .await?;
    output::emit(json!({ "urn": urn, "state": "withdrawn" }), || {
        format!("left {}", clean(&urn))
    });
    Ok(())
}

pub async fn run_policy(c: &Client, args: PolicyArgs) -> Result<()> {
    let urn = resolve_agent(c, &args.agent).await?;
    let body = json!({ "visibility": args.visibility, "mode": args.mode }).to_string();
    let path = format!("/v1/agents/{}/access-policy", encode_path_segment(&urn));
    let out = c
        .put_raw_json(&path, &body, args.idempotency_key.as_deref())
        .await?;
    if output::is_json() {
        client::print_json(&out);
    } else {
        let v: Value = serde_json::from_str(&out).unwrap_or(Value::Null);
        let p = &v["policy"];
        println!(
            "{} visibility {} mode {}",
            clean(&urn),
            clean(text(p, "visibility")),
            clean(text(p, "mode"))
        );
    }
    Ok(())
}

pub async fn run(host: Option<String>, token: Option<String>, cmd: AccessCmd) -> Result<()> {
    let cfg = crate::config::resolve(host, token).await?;
    let c = &Client::new(&cfg)?;
    match cmd {
        AccessCmd::Mine(args) => {
            let path = list_path("/v1/me/access", args.page_size);
            let body = client::list_paged(c, &path, "requests", args.all).await?;
            if output::is_json() {
                client::print_json(&body);
            } else {
                println!("{}", format_mine(&body));
            }
            Ok(())
        }
        AccessCmd::Requests(args) => run_requests(c, args).await,
        AccessCmd::Approve(a) => {
            let reason = a.reason.as_deref().map(str::trim).filter(|r| !r.is_empty());
            decide(c, &a.id, "approve", reason, a.idempotency_key).await
        }
        AccessCmd::Deny(a) => {
            let reason = require_reason(&a.reason)?;
            decide(c, &a.id, "deny", Some(&reason), a.idempotency_key).await
        }
        AccessCmd::Revoke(a) => run_revoke(c, a).await,
    }
}

fn require_reason(reason: &str) -> Result<String> {
    let r = reason.trim();
    if r.is_empty() {
        return Err(fail(
            ExitCode::Usage,
            "Wrong: --reason is empty.\nExpected: a reason the member sees.\nFix: pass --reason \"...\".",
        ));
    }
    Ok(r.to_string())
}

/// Keep only the rows of `urn` in a `requests` page.
fn filter_agent(body: &str, urn: &str) -> String {
    let Ok(mut v) = serde_json::from_str::<Value>(body) else {
        return body.to_string();
    };
    if let Some(rows) = v.get_mut("requests").and_then(Value::as_array_mut) {
        rows.retain(|r| r.get("urn").and_then(Value::as_str) == Some(urn));
    }
    v.to_string()
}

/// The admin queue: `REQUEST AGENT USER STATE REQUESTED MESSAGE`.
pub fn format_requests(body: &str) -> String {
    let Ok(v) = serde_json::from_str::<Value>(body) else {
        return body.to_string();
    };
    let rows = v["requests"].as_array().cloned().unwrap_or_default();
    if rows.is_empty() {
        return "no access requests".to_string();
    }
    let rows: Vec<Vec<String>> = rows
        .iter()
        .map(|r| {
            let user = match text(r, "username") {
                "" => text(r, "userId"),
                name => name,
            };
            vec![
                text(r, "requestId").to_string(),
                text(r, "urn").to_string(),
                user.to_string(),
                text(r, "state").to_string(),
                text(r, "requestedAt").to_string(),
                text(r, "message").to_string(),
            ]
        })
        .collect();
    table(
        &["REQUEST", "AGENT", "USER", "STATE", "REQUESTED", "MESSAGE"],
        &rows,
    )
}

async fn run_requests(c: &Client, args: RequestsArgs) -> Result<()> {
    let mut path = format!("/v1/access-requests?state={}&adminOf=me", args.state);
    if let Some(n) = args.page_size {
        path.push_str(&format!("&pageSize={n}"));
    }
    let mut body = client::list_paged(c, &path, "requests", args.all).await?;
    if let Some(urn) = args.agent.as_deref() {
        body = filter_agent(&body, urn.trim());
    }
    if output::is_json() {
        client::print_json(&body);
    } else {
        println!("{}", format_requests(&body));
    }
    Ok(())
}

/// What a decide or revoke answered, as `request <id> <state>`.
fn format_outcome(body: &str, id: &str, fallback: &str) -> String {
    let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let r = &v["request"];
    let id = match text(r, "requestId") {
        "" => id,
        x => x,
    };
    let state = match text(r, "state") {
        "" => fallback,
        x => x,
    };
    format!("request {} {}", clean(id), clean(state))
}

async fn decide(
    c: &Client,
    id: &str,
    decision: &str,
    reason: Option<&str>,
    key: Option<String>,
) -> Result<()> {
    let mut body = json!({ "decision": decision });
    if let Some(r) = reason {
        body["reason"] = json!(r);
    }
    let path = format!("/v1/access-requests/{}/decide", encode_path_segment(id));
    let key = resolve_idempotency_key(key);
    let out = c.post_raw_json(&path, &body.to_string(), &key).await?;
    if output::is_json() {
        client::print_json(&out);
    } else {
        let fallback = if decision == "approve" {
            "approved"
        } else {
            "denied"
        };
        println!("{}", format_outcome(&out, id, fallback));
    }
    Ok(())
}

/// `(userId, username)` pairs from the admin queue rows.
pub(crate) fn user_pairs(rows: &[Value]) -> Vec<(String, String)> {
    rows.iter()
        .filter_map(|r| {
            let id = r.get("userId").and_then(Value::as_str)?;
            Some((id.to_string(), text(r, "username").to_string()))
        })
        .collect()
}

/// Every row of `GET /v1/access-requests` for `state`, all pages.
pub(crate) async fn queue_rows(c: &Client, state: &str) -> Result<Vec<Value>> {
    let path = format!("/v1/access-requests?state={state}&adminOf=me&pageSize=1000");
    let body = client::list_paged(c, &path, "requests", true).await?;
    let v: Value = serde_json::from_str(&body)?;
    Ok(v["requests"].as_array().cloned().unwrap_or_default())
}

/// A generated user id: `usr_` plus 26 characters. Usernames never take that shape.
pub(crate) fn looks_like_user_id(s: &str) -> bool {
    s.strip_prefix("usr_").is_some_and(|rest| {
        rest.len() == 26 && rest.bytes().all(|b| matches!(b, b'a'..=b'z' | b'2'..=b'7'))
    })
}

/// The id of `who` in `pairs`: the same id, else the username in any letter case.
pub(crate) fn find_user_id(pairs: &[(String, String)], who: &str) -> Option<String> {
    pairs
        .iter()
        .find(|(id, _)| id == who)
        .map(|(id, _)| id.clone())
}

/// The id of the user named `who`, in any letter case.
pub(crate) fn find_user_name(pairs: &[(String, String)], who: &str) -> Option<String> {
    pairs
        .iter()
        .find(|(_, name)| !name.is_empty() && name.eq_ignore_ascii_case(who))
        .map(|(id, _)| id.clone())
}

pub(crate) fn find_user(pairs: &[(String, String)], who: &str) -> Option<String> {
    pairs
        .iter()
        .find(|(id, _)| id == who)
        .or_else(|| {
            pairs
                .iter()
                .find(|(_, name)| !name.is_empty() && name.eq_ignore_ascii_case(who))
        })
        .map(|(id, _)| id.clone())
}

async fn run_revoke(c: &Client, a: RevokeArgs) -> Result<()> {
    let reason = require_reason(&a.reason)?;
    let urn = resolve_agent(c, &a.agent).await?;
    let who = a.user.trim();
    let user_id = if looks_like_user_id(who) {
        who.to_string()
    } else {
        let rows: Vec<Value> = queue_rows(c, "approved")
            .await?
            .into_iter()
            .filter(|r| r.get("urn").and_then(Value::as_str) == Some(urn.as_str()))
            .collect();
        find_user(&user_pairs(&rows), who).ok_or_else(|| {
            fail(
                ExitCode::NotFound,
                format!(
                    "Wrong: no member with approved access to {} matches '{}'.\nExpected: a user id, or the username of a member listed as approved.\nFix: vam access requests --state approved --agent {}.",
                    clean(&urn),
                    clean(who),
                    clean(&urn)
                ),
            )
        })?
    };
    let path = format!(
        "/v1/agents/{}/access/{}/revoke",
        encode_path_segment(&urn),
        encode_path_segment(&user_id)
    );
    let key = resolve_idempotency_key(a.idempotency_key);
    let out = c
        .post_raw_json(&path, &json!({ "reason": reason }).to_string(), &key)
        .await?;
    if output::is_json() {
        client::print_json(&out);
    } else {
        println!("{}", format_outcome(&out, "", "revoked"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decision_wording_is_exact() {
        let ok = r#"{"request":{"requestId":"acr_1","urn":"urn:a","state":"approved"}}"#;
        assert_eq!(format_decision(ok), "request acr_1 approved");
        let pend = r#"{"request":{"requestId":"acr_2","urn":"urn:a","state":"pending"}}"#;
        assert_eq!(
            format_decision(pend),
            "request acr_2 pending: an admin of urn:a decides"
        );
    }

    #[test]
    fn tables_strip_control_characters_from_user_text() {
        let body = json!({"requests":[{"requestId":"acr_1","urn":"u","state":"denied","requestedAt":"t","reason":"no\u{1b}[31m\nway"}]})
            .to_string();
        let out = format_mine(&body);
        assert!(!out.contains('\u{1b}'));
        assert_eq!(out.lines().count(), 2, "{out}");
        let browse = json!({"agents":[{"urn":"u","displayName":"x\u{7}y","latestVersion":"1.0.0","mode":"approval","canAdmin":false,"access":{"state":"denied","reason":"a\u{1b}b\nc"}}]})
            .to_string();
        let out = format_browse(&browse);
        assert!(!out.chars().any(|c| c.is_control() && c != '\n'), "{out}");
        assert!(out.contains("reason: ab c"));
        assert!(out.starts_with("URN"));
    }

    #[test]
    fn users_resolve_by_id_then_name_in_any_case() {
        let pairs = vec![
            ("usr_a".to_string(), "Ada".to_string()),
            ("usr_b".to_string(), String::new()),
        ];
        assert_eq!(find_user(&pairs, "usr_b").as_deref(), Some("usr_b"));
        assert_eq!(find_user(&pairs, "aDa").as_deref(), Some("usr_a"));
        assert_eq!(find_user(&pairs, ""), None);
        assert_eq!(find_user(&pairs, "bob"), None);
        assert!(looks_like_user_id("usr_abcdefghijklmnopqrstuvwxyz"));
        assert!(!looks_like_user_id("usr_short"));
        assert!(!looks_like_user_id("ada"));
    }

    #[test]
    fn requests_filter_by_agent_and_print_the_queue() {
        let body = json!({"requests":[
            {"requestId":"acr_1","urn":"urn:a","userId":"u1","username":"ada","state":"pending","requestedAt":"t","message":"hi\u{1b}"},
            {"requestId":"acr_2","urn":"urn:b","userId":"u2","state":"pending","requestedAt":"t"}]})
        .to_string();
        let only = filter_agent(&body, "urn:b");
        let out = format_requests(&only);
        assert!(out.contains("acr_2") && !out.contains("acr_1"), "{out}");
        let all = format_requests(&body);
        assert!(all.starts_with("REQUEST"), "{all}");
        assert!(all.contains("ada") && all.contains("u2") && !all.contains('\u{1b}'));
        assert_eq!(format_requests(r#"{"requests":[]}"#), "no access requests");
    }

    #[test]
    fn outcome_wording() {
        let b = r#"{"request":{"requestId":"acr_1","state":"revoked"}}"#;
        assert_eq!(format_outcome(b, "", "x"), "request acr_1 revoked");
        assert_eq!(
            format_outcome("", "acr_9", "denied"),
            "request acr_9 denied"
        );
    }

    #[test]
    fn short_names_resolve_unique_none_and_ambiguous() {
        let rows = vec![
            json!({"urn":"urn:vam:agent:h/ns/plain"}),
            json!({"urn":"urn:vam:agent:h/ns/twin"}),
            json!({"urn":"urn:vam:agent:h/other/twin"}),
        ];
        let one = urns_named(&rows, "plain");
        assert_eq!(pick("plain", one).unwrap(), "urn:vam:agent:h/ns/plain");
        let none = pick("nope", urns_named(&rows, "nope")).unwrap_err();
        assert_eq!(output::classify(&none).0, ExitCode::NotFound);
        assert!(none.to_string().contains("vam agent browse"));
        let many = pick("twin", urns_named(&rows, "twin")).unwrap_err();
        assert_eq!(output::classify(&many).0, ExitCode::Usage);
    }

    #[test]
    fn clean_drops_format_characters() {
        assert_eq!(clean("a\u{202E}b\u{200B}c\td\ne"), "abc d e");
    }
}
