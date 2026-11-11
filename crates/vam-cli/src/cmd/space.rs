//! `vam space`: list, create and rename Spaces. A Space is reached by its `sp_…` id; the label
//! is only a name the person chose (any characters, not unique, never a lookup key).

use anyhow::{Context, Result};
use clap::Subcommand;
use serde_json::{json, Value};

use crate::client::{self, Client};
use crate::config;
use crate::output::{self, ExitCode};

#[derive(Subcommand)]
pub enum SpaceCmd {
    /// List your Spaces: id, label, agent
    Ls,
    /// Create a Space for an agent
    Create {
        /// Agent URN the Space belongs to
        #[arg(long)]
        agent: String,
        /// Label for the Space (any characters)
        label: String,
    },
    /// Rename a Space (the id stays)
    Rename {
        /// Space id (`sp_…`)
        space: String,
        /// New label
        label: String,
    },
}

/// `sp_` + 26 base32 characters (a-z, 2-7).
pub fn is_space_id(value: &str) -> bool {
    has_id_shape(value, "sp_")
}

/// `th_` + 26 base32 characters (a-z, 2-7).
pub fn is_thread_id(value: &str) -> bool {
    has_id_shape(value, "th_")
}

fn has_id_shape(value: &str, prefix: &str) -> bool {
    value.strip_prefix(prefix).is_some_and(|rest| {
        rest.len() == 26
            && rest
                .bytes()
                .all(|b| b.is_ascii_lowercase() || (b'2'..=b'7').contains(&b))
    })
}

/// A Space is named by its id; a label would reach nothing, so it is refused up front.
pub fn require_space_id(value: &str) -> Result<()> {
    if is_space_id(value.trim()) {
        return Ok(());
    }
    Err(output::fail(
        ExitCode::Usage,
        format!(
            "'{}' is not a Space id. A Space is reached by its id (sp_…), not by its label.\nList your Spaces with: vam space ls",
            value.trim()
        ),
    ))
}

/// A thread is named by its id.
pub fn require_thread_id(value: &str) -> Result<()> {
    if is_thread_id(value.trim()) {
        return Ok(());
    }
    Err(output::fail(
        ExitCode::Usage,
        format!(
            "'{}' is not a thread id. A thread is reached by its id (th_…), not by its label.\nList threads with: vam thread ls --space <sp_id>",
            value.trim()
        ),
    ))
}

/// `space sp_… (label) · thread th_… (label)`.
pub fn place_line(space: &str, space_label: &str, thread: &str, thread_label: &str) -> String {
    let show = |id: &str, label: &str| {
        if label.is_empty() {
            id.to_string()
        } else {
            format!("{id} ({label})")
        }
    };
    format!(
        "space {} · thread {}",
        show(space, &clean(space_label)),
        show(thread, &clean(thread_label))
    )
}

/// Labels are user text: control characters never reach the terminal.
fn clean(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).collect()
}

pub async fn run(host: Option<String>, token: Option<String>, cmd: SpaceCmd) -> Result<()> {
    let cfg = config::resolve(host, token).await?;
    let c = Client::new(&cfg)?;
    match cmd {
        SpaceCmd::Ls => ls(&c).await,
        SpaceCmd::Create { agent, label } => create(&c, &agent, &label).await,
        SpaceCmd::Rename { space, label } => rename(&c, &space, &label).await,
    }
}

async fn ls(c: &Client) -> Result<()> {
    let body = c
        .get("/v1/spaces?pageSize=200")
        .await
        .context("GET /v1/spaces")?;
    if output::is_json() {
        client::print_json(&body);
        return Ok(());
    }
    let value: Value = serde_json::from_str(&body).context("parse space list JSON")?;
    for line in render_list(&value) {
        println!("{line}");
    }
    Ok(())
}

pub(crate) fn render_list(value: &Value) -> Vec<String> {
    value["spaces"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|s| {
            format!(
                "{}\t{}\t{}",
                clean(s["spaceId"].as_str().unwrap_or("?")),
                clean(s["name"].as_str().unwrap_or("")),
                clean(s["agentUrn"].as_str().unwrap_or("-")),
            )
        })
        .collect()
}

async fn create(c: &Client, agent: &str, label: &str) -> Result<()> {
    let agent = agent.trim();
    let body = json!({ "agentUrn": agent, "label": label }).to_string();
    // A fresh key per invocation: a retried HTTP call is safe, a second command is a new Space.
    let key = client::new_idempotency_key();
    let resp = c
        .post_raw_json("/v1/spaces", &body, &key)
        .await
        .context("POST /v1/spaces")?;
    print_space(&resp)
}

async fn rename(c: &Client, space: &str, label: &str) -> Result<()> {
    require_space_id(space)?;
    let id = client::encode_path_segment(space.trim());
    let path = format!("/v1/spaces/{id}");
    let resp = c
        .patch_raw_json(&path, &json!({ "label": label }).to_string(), None)
        .await
        .with_context(|| format!("PATCH {path}"))?;
    print_space(&resp)
}

fn print_space(body: &str) -> Result<()> {
    if output::is_json() {
        client::print_json(body);
        return Ok(());
    }
    let v: Value = serde_json::from_str(body).context("parse space JSON")?;
    println!(
        "{} ({})",
        clean(v["spaceId"].as_str().unwrap_or("?")),
        clean(v["name"].as_str().unwrap_or(""))
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SP: &str = "sp_k3q7xw2m5b4n6a7c3d2e3f4g5h";
    const TH: &str = "th_k3q7xw2m5b4n6a7c3d2e3f4g5h";

    #[test]
    fn ids_and_labels_are_told_apart() {
        assert!(is_space_id(SP) && !is_space_id(TH) && !is_space_id("assistant"));
        assert!(is_thread_id(TH) && !is_thread_id(SP) && !is_thread_id("main"));
        assert!(require_space_id("my notes").is_err());
        assert!(require_thread_id("main").is_err());
        assert!(require_space_id(&format!(" {SP} ")).is_ok());
    }

    #[test]
    fn the_place_line_shows_ids_and_labels() {
        assert_eq!(
            place_line(SP, "my notes", TH, "plan\u{1b}[2J"),
            format!("space {SP} (my notes) · thread {TH} (plan[2J)")
        );
        assert_eq!(
            place_line(SP, "", TH, ""),
            format!("space {SP} · thread {TH}")
        );
    }

    #[test]
    fn the_list_shows_id_label_and_agent() {
        let v = json!({"spaces": [{"spaceId": SP, "name": "my notes", "agentUrn": "urn:a"}]});
        assert_eq!(render_list(&v), vec![format!("{SP}\tmy notes\turn:a")]);
    }
}
