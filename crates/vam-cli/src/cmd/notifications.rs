//! `vam notifications`: what waits for you. Every request carries `X-Vam-Surface: cli`
//! (the client sets it), so an acted-on record names the CLI as its surface.

use anyhow::Result;
use clap::{Args, Subcommand};
use serde_json::{json, Value};

use crate::client::{self, resolve_idempotency_key, Client};
use crate::config;

#[derive(Args)]
pub struct NotificationsArgs {
    #[command(subcommand)]
    pub command: Option<NotificationsCmd>,
    /// Only unread records.
    #[arg(long)]
    pub unread: bool,
    /// Only this kind (for example `approval.pending`).
    #[arg(long)]
    pub kind: Option<String>,
    /// Include resolved records (default: open ones only).
    #[arg(long)]
    pub all: bool,
}

#[derive(Subcommand)]
pub enum NotificationsCmd {
    /// Show one record in full.
    Show { id: String },
    /// Mark a record read, or every record with `--all`.
    Read {
        id: Option<String>,
        /// Mark every record read.
        #[arg(long, conflicts_with = "id")]
        all: bool,
    },
    /// Your linked channel accounts and links waiting for their code.
    Contacts,
    /// Link a channel account (for example `slack`). Prints a ten-minute, single-use code to
    /// send from that account to the VAM app.
    Link {
        channel: String,
        /// Only this account may verify (Slack `<team>/<user>`); any account when omitted.
        #[arg(long)]
        account: Option<String>,
    },
    /// Unlink your account on a channel. It receives nothing afterwards.
    Unlink { channel: String },
    /// Show or change which notifications go to your channels and when.
    Prefs(PrefsArgs),
}

#[derive(Args, Default)]
pub struct PrefsArgs {
    /// Quiet hours as `HH:MM-HH:MM` (whole minutes). Nothing goes to a channel inside them,
    /// approvals included, except kinds marked `--always`. Needs `--tz`.
    #[arg(long, value_name = "START-END")]
    pub quiet: Option<String>,
    /// IANA time zone for `--quiet` (for example Europe/Berlin).
    #[arg(long)]
    pub tz: Option<String>,
    /// Remove quiet hours.
    #[arg(long, conflicts_with_all = ["quiet", "tz"])]
    pub no_quiet: bool,
    /// The kind the flags below apply to (for example `run.finished`).
    #[arg(long)]
    pub kind: Option<String>,
    /// Send this kind to channels.
    #[arg(long, requires = "kind", conflicts_with = "off")]
    pub on: bool,
    /// Do not send this kind to channels (the app still lists it).
    #[arg(long, requires = "kind")]
    pub off: bool,
    /// Send this kind even inside quiet hours.
    #[arg(long, requires = "kind", conflicts_with = "not_always")]
    pub always: bool,
    /// Stop sending this kind inside quiet hours.
    #[arg(long, requires = "kind")]
    pub not_always: bool,
}

impl PrefsArgs {
    fn changes_anything(&self) -> bool {
        self.quiet.is_some()
            || self.tz.is_some()
            || self.no_quiet
            || self.on
            || self.off
            || self.always
            || self.not_always
    }
}

/// `22:00-07:00` into `(start, end)`; the server checks the digits and the range.
pub fn split_quiet(s: &str) -> Result<(String, String)> {
    s.split_once('-')
        .map(|(a, b)| (a.trim().to_string(), b.trim().to_string()))
        .filter(|(a, b)| !a.is_empty() && !b.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!("Wrong: --quiet {s}.\nFix: --quiet 22:00-07:00 --tz Europe/Berlin.")
        })
}

/// The `PUT /v1/me/notification-prefs` body: the current document with the flags laid over it.
pub fn merge_prefs(current: &Value, a: &PrefsArgs) -> Result<Value> {
    let mut kinds = current["kinds"].as_object().cloned().unwrap_or_default();
    let mut always: Vec<String> = current["always"]
        .as_array()
        .map(|l| {
            l.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let mut quiet = current.get("quietHours").filter(|q| q.is_object()).cloned();
    if a.no_quiet {
        quiet = None;
    }
    if a.quiet.is_some() || a.tz.is_some() {
        let old = quiet.clone().unwrap_or_else(|| json!({}));
        let (start, end) = match &a.quiet {
            Some(q) => split_quiet(q)?,
            None => (
                old["start"].as_str().unwrap_or("").to_string(),
                old["end"].as_str().unwrap_or("").to_string(),
            ),
        };
        let tz =
            a.tz.clone()
                .or_else(|| old["tz"].as_str().map(String::from))
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "Wrong: --quiet needs a time zone.\nFix: add --tz Europe/Berlin (or UTC)."
                    )
                })?;
        if start.is_empty() || end.is_empty() {
            anyhow::bail!(
                "Wrong: no quiet window to change.\nFix: --quiet 22:00-07:00 --tz <zone>."
            );
        }
        quiet = Some(json!({ "start": start, "end": end, "tz": tz }));
    }
    if let Some(kind) = a.kind.as_deref() {
        if a.on {
            kinds.insert(kind.into(), json!("immediate"));
        }
        if a.off {
            kinds.insert(kind.into(), json!("off"));
        }
        if a.always && !always.iter().any(|k| k == kind) {
            always.push(kind.to_string());
        }
        if a.not_always {
            always.retain(|k| k != kind);
        }
    }
    let mut out = json!({ "kinds": kinds, "always": always });
    if let Some(q) = quiet {
        out["quietHours"] = q;
    }
    Ok(out)
}

/// A readable view of a prefs body.
pub fn prefs_lines(body: &Value) -> Vec<String> {
    let mut lines = Vec::new();
    match body.get("quietHours").filter(|q| q.is_object()) {
        Some(q) => lines.push(format!(
            "quiet hours: {}-{} {}",
            q["start"].as_str().unwrap_or("?"),
            q["end"].as_str().unwrap_or("?"),
            q["tz"].as_str().unwrap_or("?")
        )),
        None => lines.push("quiet hours: none".to_string()),
    }
    let always: Vec<&str> = body["always"]
        .as_array()
        .map(|l| l.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    lines.push(format!(
        "always sent in quiet hours: {}",
        if always.is_empty() {
            "none".to_string()
        } else {
            always.join(", ")
        }
    ));
    if let Some(eff) = body["effective"].as_object() {
        for (kind, mode) in eff {
            let mode = match mode.as_str().unwrap_or("?") {
                "warn" => "immediate if it is a warning",
                m => m,
            };
            let mark = if body["kinds"].get(kind).is_some() {
                " (yours)"
            } else {
                ""
            };
            lines.push(format!("  {kind:<20} {mode}{mark}"));
        }
    }
    lines
}

/// A readable view of a `GET /v1/me/contacts` body.
pub fn contacts_lines(body: &Value) -> Vec<String> {
    let list = body["contacts"].as_array().cloned().unwrap_or_default();
    if list.is_empty() {
        return vec!["No linked accounts. Link one with: vam notifications link slack".to_string()];
    }
    list.iter()
        .map(|c| {
            let s = |k: &str| c.get(k).and_then(Value::as_str).unwrap_or("");
            let extra = match s("state") {
                "pending" => " (waiting for the code)".to_string(),
                _ => String::new(),
            };
            format!(
                "{}  {}  {}  {}{extra}",
                s("id"),
                s("channel"),
                s("state"),
                s("display")
            )
        })
        .collect()
}

fn list_path(unread: bool, kind: Option<&str>, all: bool) -> String {
    let mut q = vec![format!("state={}", if all { "all" } else { "open" })];
    if unread {
        q.push("unread=true".to_string());
    }
    if let Some(k) = kind {
        q.push(format!("kind={}", client::encode_path_segment(k)));
    }
    q.push("pageSize=50".to_string());
    format!("/v1/notifications?{}", q.join("&"))
}

/// The command a person runs to act on a record, for decision kinds.
fn decide_line(rec: &Value) -> Option<String> {
    let verb = rec.pointer("/action/verb").and_then(Value::as_str)?;
    let approval = rec.get("approvalId").and_then(Value::as_str)?;
    (verb == "decide").then(|| format!("vam approve decide {approval}"))
}

/// The command that clears a `connection.needs_reauth` record.
fn connect_line(rec: &Value) -> Option<String> {
    if rec.get("kind").and_then(Value::as_str) != Some("connection.needs_reauth") {
        return None;
    }
    let id = rec.pointer("/action/target/id").and_then(Value::as_str)?;
    Some(format!("vam connectors connect {id}"))
}

pub fn format_list(body: &Value) -> String {
    let records = body
        .get("records")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if records.is_empty() {
        return "Nothing waiting.".to_string();
    }
    let mut out = Vec::new();
    for rec in &records {
        let s = |k: &str| rec.get(k).and_then(Value::as_str).unwrap_or("");
        let mark = if rec.get("readAt").is_some_and(|v| !v.is_null()) {
            " "
        } else {
            "*"
        };
        out.push(format!(
            "{mark} {}  {}  {}  {}",
            s("id"),
            s("severity"),
            s("kind"),
            s("title")
        ));
        if !s("body").is_empty() {
            out.push(format!("    {}", s("body")));
        }
        if let Some(line) = decide_line(rec) {
            out.push(format!("    decide: {line} --decision approved|rejected"));
        }
        if let Some(line) = connect_line(rec) {
            out.push(format!("    connect: {line}"));
        }
    }
    out.join("\n")
}

pub async fn run(
    host: Option<String>,
    token: Option<String>,
    args: NotificationsArgs,
) -> Result<()> {
    let cfg = config::resolve(host, token).await?;
    let c = Client::new(&cfg)?;
    match args.command {
        None => {
            let text = c
                .get(&list_path(args.unread, args.kind.as_deref(), args.all))
                .await?;
            if crate::output::is_json() {
                client::print_json(&text);
            } else {
                let body: Value = serde_json::from_str(&text)?;
                println!("{}", format_list(&body));
                if let Some(t) = body
                    .pointer("/pagination/nextPageToken")
                    .and_then(Value::as_str)
                    .filter(|t| !t.is_empty())
                {
                    eprintln!("more results exist (next page token {t})");
                }
            }
        }
        Some(NotificationsCmd::Show { id }) => {
            let text = c
                .get(&format!(
                    "/v1/notifications/{}",
                    client::encode_path_segment(&id)
                ))
                .await?;
            if crate::output::is_json() {
                client::print_json(&text);
            } else {
                let rec: Value = serde_json::from_str(&text)?;
                println!("{}", format_list(&json!({ "records": [rec.clone()] })));
                if let Some(t) = rec.pointer("/agentText/text").and_then(Value::as_str) {
                    let label = rec
                        .pointer("/agentText/label")
                        .and_then(Value::as_str)
                        .unwrap_or("agent");
                    println!("\n{label} says: {t}");
                }
            }
        }
        Some(NotificationsCmd::Read { id, all }) => {
            let key = resolve_idempotency_key(None);
            let text = match (id, all) {
                (Some(id), false) => {
                    c.post_raw_json(
                        &format!("/v1/notifications/{}/read", client::encode_path_segment(&id)),
                        "{}",
                        &key,
                    )
                    .await?
                }
                (None, true) => {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs() as i64)
                        .unwrap_or(0);
                    c.post_raw_json(
                        "/v1/notifications/read",
                        &json!({ "before": now }).to_string(),
                        &key,
                    )
                    .await?
                }
                _ => anyhow::bail!(
                    "Wrong: give a record id or --all.\nFix: vam notifications read <ID>  or  vam notifications read --all."
                ),
            };
            client::print_json(&text);
        }
        Some(NotificationsCmd::Contacts) => {
            let text = c.get("/v1/me/contacts").await?;
            if crate::output::is_json() {
                client::print_json(&text);
            } else {
                for line in contacts_lines(&serde_json::from_str(&text)?) {
                    println!("{line}");
                }
            }
        }
        Some(NotificationsCmd::Link { channel, account }) => {
            let mut body = json!({ "channel": channel });
            if let Some(a) = account {
                body["address"] = json!(a);
            }
            let key = resolve_idempotency_key(None);
            let text = c
                .post_raw_json("/v1/me/contacts", &body.to_string(), &key)
                .await?;
            if crate::output::is_json() {
                client::print_json(&text);
            } else {
                let v: Value = serde_json::from_str(&text)?;
                println!(
                    "Your code: {}\nSend it from your {channel} account to the VAM app within {} minutes. It works once.",
                    v["code"].as_str().unwrap_or("?"),
                    v["expiresInSecs"].as_i64().unwrap_or(600) / 60
                );
            }
        }
        Some(NotificationsCmd::Unlink { channel }) => {
            let listed: Value = serde_json::from_str(&c.get("/v1/me/contacts").await?)?;
            let id = listed["contacts"]
                .as_array()
                .and_then(|l| {
                    l.iter()
                        .find(|x| x["channel"].as_str() == Some(channel.as_str()))
                })
                .and_then(|x| x["id"].as_str().map(String::from))
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "Wrong: no {channel} account is linked.\nFix: vam notifications contacts."
                    )
                })?;
            let key = resolve_idempotency_key(None);
            let text = c
                .post_raw_json(
                    &format!(
                        "/v1/me/contacts/{}/unlink",
                        client::encode_path_segment(&id)
                    ),
                    "{}",
                    &key,
                )
                .await?;
            if crate::output::is_json() {
                client::print_json(&text);
            } else {
                println!("Unlinked {channel}. It receives nothing from now on.");
            }
        }
        Some(NotificationsCmd::Prefs(p)) => {
            let current: Value = serde_json::from_str(&c.get("/v1/me/notification-prefs").await?)?;
            let shown = if p.changes_anything() {
                let body = merge_prefs(&current, &p)?;
                let key = resolve_idempotency_key(None);
                let text = c
                    .put_raw_json("/v1/me/notification-prefs", &body.to_string(), Some(&key))
                    .await?;
                serde_json::from_str(&text)?
            } else {
                current
            };
            if crate::output::is_json() {
                println!("{shown}");
            } else {
                for line in prefs_lines(&shown) {
                    println!("{line}");
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_carries_the_filters() {
        assert_eq!(
            list_path(true, Some("approval.pending"), false),
            "/v1/notifications?state=open&unread=true&kind=approval.pending&pageSize=50"
        );
        assert!(list_path(false, None, true).contains("state=all"));
    }

    #[test]
    fn prefs_flags_lay_over_the_current_document() {
        let cur = json!({"kinds":{"budget.hit":"off"},"always":["gate.opened"],
            "quietHours":{"start":"22:00","end":"07:00","tz":"UTC"},"effective":{}});
        let a = PrefsArgs {
            kind: Some("run.finished".into()),
            on: true,
            always: true,
            tz: Some("Europe/Berlin".into()),
            ..PrefsArgs::default()
        };
        let out = merge_prefs(&cur, &a).unwrap();
        assert_eq!(out["kinds"]["run.finished"], "immediate");
        assert_eq!(out["kinds"]["budget.hit"], "off");
        assert_eq!(out["always"], json!(["gate.opened", "run.finished"]));
        assert_eq!(
            out["quietHours"],
            json!({"start":"22:00","end":"07:00","tz":"Europe/Berlin"})
        );
        assert!(out.get("effective").is_none());
        let a = PrefsArgs {
            no_quiet: true,
            ..PrefsArgs::default()
        };
        assert!(merge_prefs(&cur, &a).unwrap().get("quietHours").is_none());
        let a = PrefsArgs {
            quiet: Some("23:00-06:30".into()),
            ..PrefsArgs::default()
        };
        assert_eq!(merge_prefs(&cur, &a).unwrap()["quietHours"]["end"], "06:30");
        // No zone known: say so.
        assert!(merge_prefs(&json!({}), &a).is_err());
        assert!(split_quiet("2200").is_err());
        let a = PrefsArgs {
            kind: Some("gate.opened".into()),
            not_always: true,
            ..PrefsArgs::default()
        };
        assert_eq!(merge_prefs(&cur, &a).unwrap()["always"], json!([]));
    }

    #[test]
    fn contacts_and_prefs_print_plainly_without_codes() {
        let lines = contacts_lines(&json!({"contacts":[
            {"id":"ct_1","channel":"slack","state":"verified","display":"T1/U1"},
            {"id":"ct_2","channel":"slack","state":"pending","display":""}]}));
        assert!(lines[0].contains("verified") && lines[1].contains("waiting for the code"));
        assert!(contacts_lines(&json!({"contacts":[]}))[0].contains("vam notifications link slack"));
        let p = prefs_lines(
            &json!({"always":[],"effective":{"run.finished":"warn","budget.hit":"off"},
            "kinds":{"budget.hit":"off"}}),
        );
        assert!(p
            .iter()
            .any(|l| l.contains("run.finished") && l.contains("if it is a warning")));
        assert!(p
            .iter()
            .any(|l| l.contains("budget.hit") && l.contains("(yours)")));
    }

    #[test]
    fn decision_kinds_print_the_decide_command() {
        let body = json!({"records": [
            {"id":"n1","severity":"action","kind":"approval.pending","title":"T","body":"B",
             "approvalId":"ap-1","action":{"verb":"decide"}},
            {"id":"n2","severity":"info","kind":"run.finished","title":"Done","readAt":5,
             "action":{"verb":"open"}}
        ]});
        let text = format_list(&body);
        assert!(text.contains("vam approve decide ap-1"), "{text}");
        assert_eq!(text.matches("vam approve decide").count(), 1);
        assert!(text.contains("* n1") && text.contains("  n2"));
        assert_eq!(format_list(&json!({"records": []})), "Nothing waiting.");
        let rec = json!({"id":"nt_1","severity":"action","kind":"connection.needs_reauth",
            "title":"Reconnect Microsoft 365 to continue","body":"b",
            "action":{"verb":"open","target":{"kind":"connector","id":"microsoft-365"}}});
        assert!(format_list(&json!({"records":[rec]}))
            .contains("    connect: vam connectors connect microsoft-365"));
    }
}
