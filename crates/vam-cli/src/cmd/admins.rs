//! `vam agent admins ls|add|rm`: the admins of one agent. vamd replaces the whole list on `PUT`,
//! so `add` and `rm` read the list, change one id and send the whole set. The CLI holds no
//! authority: vamd decides who may read or change the list.

use anyhow::Result;
use clap::Subcommand;
use serde_json::{json, Value};

use super::access::{
    clean, find_user_id, find_user_name, looks_like_user_id, queue_rows, resolve_agent, table,
    user_pairs,
};
use crate::client::{self, encode_path_segment, Client};
use crate::output::{self, fail, ExitCode};

const IDEM_HELP: &str = "Reuse this idempotency key (default: mint a fresh key).";

#[derive(Subcommand)]
pub enum AdminsCmd {
    /// List the admins of an agent (an admin of it or an org admin).
    Ls {
        /// An agent URN, or its short name.
        agent: String,
    },
    /// Add an admin. `<user>` is a user id or a username.
    Add {
        agent: String,
        user: String,
        #[arg(long, help = IDEM_HELP)]
        idempotency_key: Option<String>,
    },
    /// Remove an admin. `<user>` is a user id or a username.
    Rm {
        agent: String,
        user: String,
        #[arg(long, help = IDEM_HELP)]
        idempotency_key: Option<String>,
    },
}

fn text<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

fn admins_path(urn: &str) -> String {
    format!("/v1/agents/{}/admins", encode_path_segment(urn))
}

/// The table: `USER ID  USERNAME  ADDED BY  ADDED AT`; a deactivated admin is marked.
pub fn format_admins(body: &str) -> String {
    let Ok(v) = serde_json::from_str::<Value>(body) else {
        return body.to_string();
    };
    let rows = v["admins"].as_array().cloned().unwrap_or_default();
    if rows.is_empty() {
        return "no admins: the org admins decide requests".to_string();
    }
    let rows: Vec<Vec<String>> = rows
        .iter()
        .map(|a| {
            let mut name = text(a, "username").to_string();
            if a["deactivated"].as_bool().unwrap_or(false) {
                name = format!("{name} (deactivated)").trim().to_string();
            }
            vec![
                text(a, "userId").to_string(),
                name,
                text(a, "addedBy").to_string(),
                text(a, "addedAt").to_string(),
            ]
        })
        .collect();
    table(&["USER ID", "USERNAME", "ADDED BY", "ADDED AT"], &rows)
}

/// `(userId, username)` of the admins in a `GET .../admins` answer, in order.
fn admin_pairs(body: &str) -> Vec<(String, String)> {
    let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    v["admins"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .map(|a| {
                    (
                        text(a, "userId").to_string(),
                        text(a, "username").to_string(),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

/// A refusal (403) or a missing route (404) means "this source is not open to the caller";
/// anything else (network, 5xx) is a real failure and goes through.
fn soft<T>(r: Result<T>) -> Result<Option<T>> {
    match r {
        Ok(v) => Ok(Some(v)),
        Err(e) if matches!(output::classify(&e).1, Some(403 | 404)) => Ok(None),
        Err(e) => Err(e),
    }
}

/// True when `GET /v1/me` shows an org admin (role or the `users:admin` scope).
async fn is_org_admin(c: &Client) -> Result<bool> {
    let Some(body) = soft(c.get("/v1/me").await)? else {
        return Ok(false);
    };
    let v: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    let scoped = v["scopes"]
        .as_array()
        .is_some_and(|s| s.iter().any(|x| x.as_str() == Some("users:admin")));
    Ok(scoped || v["role"].as_str() == Some("org_admin"))
}

/// Resolve `who` to a user id. Every exact id wins over any username: the admin list, an
/// id-shaped input, the caller's queue rows, `GET /v1/users` (org admins only); then usernames
/// in the same order.
async fn resolve_user(c: &Client, who: &str, admins: &[(String, String)]) -> Result<String> {
    if let Some(id) = find_user_id(admins, who) {
        return Ok(id);
    }
    if looks_like_user_id(who) {
        return Ok(who.to_string());
    }
    let mut sources: Vec<Vec<(String, String)>> = vec![admins.to_vec()];
    if let Some(rows) = soft(queue_rows(c, "all").await)? {
        sources.push(user_pairs(&rows));
    }
    if is_org_admin(c).await? {
        if let Some(body) = soft(c.get("/v1/users").await)? {
            let v: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
            let pairs = v["users"]
                .as_array()
                .map(|us| {
                    us.iter()
                        .map(|u| {
                            (
                                text(u, "userId").to_string(),
                                text(u, "username").to_string(),
                            )
                        })
                        .collect()
                })
                .unwrap_or_default();
            sources.push(pairs);
        }
    }
    sources
        .iter()
        .find_map(|p| find_user_id(p, who))
        .or_else(|| sources.iter().find_map(|p| find_user_name(p, who)))
        .ok_or_else(|| unknown_user(who))
}

fn unknown_user(who: &str) -> anyhow::Error {
    fail(
        ExitCode::NotFound,
        format!(
            "Wrong: no user named '{}' is known to you.\nExpected: a user id, or the username of someone in the admin list or in your access requests.\nFix: pass the user id (an org admin lists ids with `vam users ls`).",
            clean(who)
        ),
    )
}

async fn put_admins(c: &Client, urn: &str, ids: &[String], key: Option<&str>) -> Result<String> {
    let body = json!({ "userIds": ids }).to_string();
    c.put_raw_json(&admins_path(urn), &body, key).await
}

pub async fn run(c: &Client, cmd: AdminsCmd) -> Result<()> {
    match cmd {
        AdminsCmd::Ls { agent } => {
            let urn = resolve_agent(c, &agent).await?;
            let body = c.get(&admins_path(&urn)).await?;
            show(&body);
            Ok(())
        }
        AdminsCmd::Add {
            agent,
            user,
            idempotency_key,
        } => change(c, &agent, &user, true, idempotency_key).await,
        AdminsCmd::Rm {
            agent,
            user,
            idempotency_key,
        } => change(c, &agent, &user, false, idempotency_key).await,
    }
}

fn show(body: &str) {
    if output::is_json() {
        client::print_json(body);
    } else {
        println!("{}", format_admins(body));
    }
}

async fn change(c: &Client, agent: &str, user: &str, add: bool, key: Option<String>) -> Result<()> {
    let urn = resolve_agent(c, agent).await?;
    let who = user.trim();
    let listed = c.get(&admins_path(&urn)).await?;
    let id = resolve_user(c, who, &admin_pairs(&listed)).await?;
    // Read the list again right before the write: one round trip between read and PUT.
    let current = c.get(&admins_path(&urn)).await?;
    let pairs = admin_pairs(&current);
    let mut ids: Vec<String> = pairs.iter().map(|(i, _)| i.clone()).collect();
    let present = ids.contains(&id);
    if add == present {
        let note = if add {
            "is already an admin of"
        } else {
            "is not an admin of"
        };
        eprintln!("{} {note} {}: nothing changed", clean(who), clean(&urn));
        show(&current);
        return Ok(());
    }
    if add {
        ids.push(id);
    } else {
        ids.retain(|i| *i != id);
    }
    let out = put_admins(c, &urn, &ids, key.as_deref()).await?;
    if ids.is_empty() {
        eprintln!(
            "warning: {} has no admins now: the org admins decide its requests",
            clean(&urn)
        );
    }
    show(&out);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_marks_a_deactivated_admin() {
        let body = json!({"admins":[
            {"userId":"u1","username":"ada","addedBy":"u0","addedAt":"t"},
            {"userId":"u2","username":"bob","addedBy":"u0","addedAt":"t","deactivated":true},
            {"userId":"u3","addedBy":"u0","addedAt":"t","deactivated":true}]})
        .to_string();
        let out = format_admins(&body);
        assert!(out.starts_with("USER ID"), "{out}");
        assert!(out.contains("bob (deactivated)"), "{out}");
        assert!(out.contains("(deactivated)"), "{out}");
        assert!(format_admins(r#"{"admins":[]}"#).contains("org admins"));
    }

    #[test]
    fn admin_pairs_keep_the_order() {
        let body =
            json!({"admins":[{"userId":"u2","username":"b"},{"userId":"u1","username":"a"}]})
                .to_string();
        let ids: Vec<String> = admin_pairs(&body).into_iter().map(|p| p.0).collect();
        assert_eq!(ids, ["u2", "u1"]);
    }

    #[test]
    fn an_id_match_beats_a_username_match() {
        let pairs = vec![
            ("u1".to_string(), "u2".to_string()),
            ("u2".to_string(), "bob".to_string()),
        ];
        assert_eq!(find_user_id(&pairs, "u2").as_deref(), Some("u2"));
        assert_eq!(find_user_name(&pairs, "U2").as_deref(), Some("u1"));
        assert!(find_user_id(&pairs, "bob").is_none());
    }
}
