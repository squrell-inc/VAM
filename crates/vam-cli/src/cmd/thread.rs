//! `vam thread`: read a workspace's conversations through the thread API.
//! Everything printed from a thread is untrusted data (user text and tool
//! output), so control characters are stripped before it reaches the terminal.

use anyhow::{Context, Result};
use clap::Subcommand;
use serde_json::{json, Value};

use super::space;

use crate::client::{self, Client};
use crate::config;
use crate::output;

/// Records per request when paging through a thread.
const PAGE: usize = 500;
/// A tool card is cut to this many characters.
const CARD_WIDTH: usize = 100;

#[derive(Subcommand)]
pub enum ThreadCmd {
    /// List the threads of a Space: id, label, records
    Ls {
        /// Space id (`sp_…`); see `vam space ls`
        #[arg(long, alias = "workspace")]
        space: String,
    },
    /// Rename a thread (the id stays)
    Rename {
        /// Space id (`sp_…`)
        #[arg(long, alias = "workspace")]
        space: String,
        /// Thread id (`th_…`)
        thread: String,
        /// New label
        label: String,
    },
    /// Show a thread: user, assistant and tool messages
    Show {
        /// Thread id (`th_…`)
        thread: String,
        /// Space id (`sp_…`); see `vam space ls`
        #[arg(long, alias = "workspace")]
        space: String,
        /// Show the context the model sees next (after compaction and rollback)
        /// instead of every record
        #[arg(long)]
        context: bool,
    },
}

pub async fn run(host: Option<String>, token: Option<String>, cmd: ThreadCmd) -> Result<()> {
    let cfg = config::resolve(host, token).await?;
    let c = Client::new(&cfg)?;
    match cmd {
        ThreadCmd::Ls { space } => ls(&c, &space).await,
        ThreadCmd::Rename {
            space,
            thread,
            label,
        } => rename(&c, &space, &thread, &label).await,
        ThreadCmd::Show {
            thread,
            space,
            context,
        } => show(&c, &space, &thread, context).await,
    }
}

async fn rename(c: &Client, space: &str, thread: &str, label: &str) -> Result<()> {
    space::require_space_id(space)?;
    space::require_thread_id(thread)?;
    let sp = client::encode_path_segment(space.trim());
    let th = client::encode_path_segment(thread.trim());
    let path = format!("/v1/spaces/{sp}/threads/{th}");
    let body = c
        .patch_raw_json(&path, &json!({ "label": label }).to_string(), None)
        .await
        .with_context(|| format!("PATCH {path}"))?;
    if output::is_json() {
        client::print_json(&body);
        return Ok(());
    }
    let v: Value = serde_json::from_str(&body).context("parse thread JSON")?;
    println!(
        "{} ({})",
        one_line(v["threadId"].as_str().unwrap_or("?"), 120),
        one_line(v["name"].as_str().unwrap_or(""), 120)
    );
    Ok(())
}

async fn ls(c: &Client, workspace: &str) -> Result<()> {
    space::require_space_id(workspace)?;
    let id = client::encode_path_segment(workspace.trim());
    let api_path = format!("/v1/workspaces/{id}/threads");
    let body = c
        .get(&api_path)
        .await
        .with_context(|| format!("GET {api_path}"))?;
    if output::is_json() {
        client::print_json(&body);
        return Ok(());
    }
    let value: Value = serde_json::from_str(&body).context("parse thread list JSON")?;
    for line in render_list(&value) {
        println!("{line}");
    }
    Ok(())
}

async fn show(c: &Client, workspace: &str, thread: &str, context: bool) -> Result<()> {
    space::require_space_id(workspace)?;
    space::require_thread_id(thread)?;
    let ws = client::encode_path_segment(workspace.trim());
    let th = client::encode_path_segment(thread.trim());
    let view = if context { "context" } else { "all" };
    let mut records: Vec<Value> = Vec::new();
    let mut after = 0u64;
    let mut last: Value;
    loop {
        let api_path = format!(
            "/v1/workspaces/{ws}/threads/{th}/messages?view={view}&limit={PAGE}&after={after}"
        );
        let body = c
            .get(&api_path)
            .await
            .with_context(|| format!("GET {api_path}"))?;
        let mut page: Value = serde_json::from_str(&body).context("parse thread messages JSON")?;
        if let Some(items) = page["records"].as_array_mut() {
            records.append(items);
        }
        let more = page["hasMore"].as_bool().unwrap_or(false);
        let next = page["nextAfter"].as_u64().unwrap_or(after);
        last = page;
        if !more || next <= after {
            break;
        }
        after = next;
    }
    if output::is_json() {
        last["records"] = Value::Array(records);
        last["hasMore"] = Value::Bool(false);
        println!("{last}");
        return Ok(());
    }
    let integrity = last["integrity"].as_str().unwrap_or("ok");
    if integrity != "ok" {
        eprintln!("[vam] warning: thread integrity {integrity}; showing records before the break");
    }
    for line in render_records(&records) {
        println!("{line}");
    }
    if let Some(plan) = render_plan(&last["plan"]) {
        println!("{plan}");
    }
    Ok(())
}

/// Removes control characters (terminal escapes among them) and cuts to `max`
/// characters with an ellipsis.
fn one_line(text: &str, max: usize) -> String {
    let cleaned: String = text
        .chars()
        .map(|c| if c == '\n' || c == '\t' { ' ' } else { c })
        .filter(|c| !c.is_control())
        .collect();
    let cleaned = cleaned.trim().to_string();
    if cleaned.chars().count() <= max {
        return cleaned;
    }
    let mut cut: String = cleaned.chars().take(max).collect();
    cut.push('…');
    cut
}

/// Removes control characters but keeps line breaks and tabs.
fn clean_block(text: &str) -> String {
    text.chars()
        .filter(|c| *c == '\n' || *c == '\t' || !c.is_control())
        .collect()
}

pub(crate) fn render_list(value: &Value) -> Vec<String> {
    let mut out = Vec::new();
    for t in value["threads"].as_array().into_iter().flatten() {
        let id = one_line(t["threadId"].as_str().unwrap_or("?"), 120);
        let records = t["head"]["seq"].as_u64().unwrap_or(0);
        let kind = if t["legacy"].as_bool().unwrap_or(false) {
            "legacy"
        } else {
            "log"
        };
        let label = one_line(t["name"].as_str().unwrap_or(""), 120);
        let agent = one_line(t["agentUrn"].as_str().unwrap_or("-"), 120);
        out.push(format!("{id}\t{label}\t{records}\t{kind}\t{agent}"));
    }
    out
}

fn part_text(part: &Value, out: &mut String) {
    match part["type"].as_str() {
        Some("text") => out.push_str(part["text"].as_str().unwrap_or_default()),
        Some("tool_result") => {
            for inner in part["content"].as_array().into_iter().flatten() {
                part_text(inner, out);
            }
        }
        _ => {}
    }
}

/// One display block per record. User and assistant text is shown whole; tool
/// calls and results are one-line cards; a compaction is a divider.
pub(crate) fn render_records(records: &[Value]) -> Vec<String> {
    let mut out = Vec::new();
    for rec in records {
        let kind = rec["kind"].as_str().unwrap_or_default();
        if kind == "compaction" {
            out.push("---------- context compacted ----------".to_string());
        }
        let Some(role) = rec["message"]["role"].as_str() else {
            continue;
        };
        let parts = rec["message"]["parts"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let mut text = String::new();
        let mut cards = Vec::new();
        for part in &parts {
            match part["type"].as_str() {
                Some("text") => text.push_str(part["text"].as_str().unwrap_or_default()),
                Some("tool_use") => cards.push(format!(
                    "  [tool call] {}({})",
                    one_line(part["name"].as_str().unwrap_or("?"), 60),
                    one_line(part["arguments"].as_str().unwrap_or_default(), CARD_WIDTH)
                )),
                Some("tool_result") => {
                    let mut body = String::new();
                    part_text(part, &mut body);
                    cards.push(format!("  [tool result] {}", one_line(&body, CARD_WIDTH)));
                }
                _ => {}
            }
        }
        if !text.trim().is_empty() {
            let label = if kind == "compaction" {
                "summary"
            } else {
                role
            };
            out.push(format!("{label}: {}", clean_block(text.trim())));
        }
        out.extend(cards);
    }
    out
}

pub(crate) fn render_plan(plan: &Value) -> Option<String> {
    let items = plan["items"].as_array()?;
    let mut lines = vec!["Plan:".to_string()];
    for item in items {
        let mark = match item["status"].as_str() {
            Some("done") | Some("completed") => "x",
            Some("in_progress") => "~",
            _ => " ",
        };
        lines.push(format!(
            "  [{mark}] {}: {}",
            one_line(item["id"].as_str().unwrap_or("?"), 40),
            one_line(item["content"].as_str().unwrap_or_default(), 200)
        ));
    }
    Some(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use serde_json::json;

    #[derive(Parser)]
    struct Wrap {
        #[command(subcommand)]
        cmd: ThreadCmd,
    }

    const SP: &str = "sp_k3q7xw2m5b4n6a7c3d2e3f4g5h";
    const TH: &str = "th_k3q7xw2m5b4n6a7c3d2e3f4g5h";

    #[test]
    fn parses_ls_show_and_rename() {
        // A Space is required: there is no default.
        assert!(Wrap::try_parse_from(["x", "ls"]).is_err());
        let w = Wrap::try_parse_from(["x", "ls", "--space", SP]).unwrap();
        assert!(matches!(w.cmd, ThreadCmd::Ls { space } if space == SP));
        let w = Wrap::try_parse_from(["x", "show", TH, "--space", SP, "--context"]).unwrap();
        match w.cmd {
            ThreadCmd::Show {
                thread,
                space,
                context,
            } => assert_eq!((thread.as_str(), space.as_str(), context), (TH, SP, true)),
            _ => panic!("show"),
        }
        assert!(Wrap::try_parse_from(["x", "show"]).is_err());
        let w = Wrap::try_parse_from(["x", "rename", "--space", SP, TH, "new name"]).unwrap();
        match w.cmd {
            ThreadCmd::Rename {
                space,
                thread,
                label,
            } => assert_eq!(
                (space.as_str(), thread.as_str(), label.as_str()),
                (SP, TH, "new name")
            ),
            _ => panic!("rename"),
        }
    }

    #[test]
    fn renders_the_thread_list() {
        let v = json!({"threads": [
            {"threadId": TH, "name": "my notes", "legacy": false, "head": {"seq": 6, "digest": "d"}, "agentUrn": "urn:vam:agent:a"},
            {"threadId": "old", "legacy": true, "head": null, "agentUrn": null}
        ]});
        assert_eq!(
            render_list(&v),
            vec![
                format!("{TH}\tmy notes\t6\tlog\turn:vam:agent:a"),
                "old\t\t0\tlegacy\t-".to_string()
            ]
        );
    }

    #[test]
    fn renders_messages_cards_and_dividers() {
        let long = "x".repeat(300);
        let records = vec![
            json!({"kind": "task_start", "detail": {}}),
            json!({"kind": "message", "message": {"role": "user", "parts": [{"type": "text", "text": "Save the note"}]}}),
            json!({"kind": "message", "message": {"role": "assistant", "parts": [
                {"type": "tool_use", "id": "c1", "name": "write_file", "arguments": "{\"path\":\"note.txt\"}"}]}}),
            json!({"kind": "message", "message": {"role": "tool", "parts": [
                {"type": "tool_result", "tool_use_id": "c1", "content": [{"type": "text", "text": long}]}]}}),
            json!({"kind": "compaction", "message": {"role": "user", "parts": [{"type": "text", "text": "goal"}]}}),
            json!({"kind": "message", "message": {"role": "assistant", "parts": [{"type": "text", "text": "Saved."}]}}),
        ];
        let lines = render_records(&records);
        assert_eq!(lines[0], "user: Save the note");
        assert_eq!(
            lines[1],
            "  [tool call] write_file({\"path\":\"note.txt\"})"
        );
        assert!(lines[2].starts_with("  [tool result] xxx"));
        assert!(lines[2].ends_with('…'));
        assert!(lines[2].chars().count() < 130);
        assert_eq!(lines[3], "---------- context compacted ----------");
        assert_eq!(lines[4], "summary: goal");
        assert_eq!(lines[5], "assistant: Saved.");
    }

    #[test]
    fn terminal_escapes_in_tool_output_are_stripped() {
        let records = vec![
            json!({"kind": "message", "message": {"role": "tool", "parts": [
            {"type": "tool_result", "tool_use_id": "c", "content": [{"type": "text", "text": "a\u{1b}[31mred\u{7}"}]}]}}),
        ];
        let lines = render_records(&records);
        assert!(!lines[0].contains('\u{1b}') && !lines[0].contains('\u{7}'));
        assert!(lines[0].contains("red"));
        let user = vec![
            json!({"kind": "message", "message": {"role": "user", "parts": [{"type": "text", "text": "hi\u{1b}[2J there"}]}}),
        ];
        assert!(!render_records(&user)[0].contains('\u{1b}'));
    }

    #[test]
    fn renders_the_plan() {
        let plan = json!({"seq": 4, "items": [
            {"id": "a", "content": "first", "status": "done"},
            {"id": "b", "content": "second", "status": "pending"}]});
        assert_eq!(
            render_plan(&plan).unwrap(),
            "Plan:\n  [x] a: first\n  [ ] b: second"
        );
        assert!(render_plan(&Value::Null).is_none());
    }
}
