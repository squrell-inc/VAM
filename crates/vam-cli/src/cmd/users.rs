use anyhow::{bail, Context, Result};
use clap::Subcommand;
use vam_spec::v1::CreateUserRequest;

use crate::client::{self, Client};
use crate::config;

#[derive(Subcommand)]
pub enum UsersCmd {
    /// List the control-plane users with their ids. Requires the `users:admin` scope.
    Ls,
    /// Create a control-plane user. Requires the `users:admin` scope.
    Create {
        #[arg(long)]
        username: String,
        /// Role for the new user. `operator` is not offered here.
        #[arg(long, value_parser = ["member", "publisher", "org_admin"])]
        role: String,
        /// Name of the env var that holds the new user's password. The
        /// password is never taken from argv.
        #[arg(long)]
        password_from_env: String,
    },
}

pub async fn run(host: Option<String>, token: Option<String>, cmd: UsersCmd) -> Result<()> {
    let cfg = config::resolve(host, token).await?;
    let c = Client::new(&cfg)?;
    match cmd {
        UsersCmd::Ls => {
            let body = c.get("/v1/users").await?;
            if crate::output::is_json() {
                client::print_json(&body);
            } else {
                println!("{}", format_users(&body));
            }
        }
        UsersCmd::Create {
            username,
            role,
            password_from_env,
        } => {
            let password = read_password_env(&password_from_env)?;
            let req = CreateUserRequest {
                username,
                password,
                role,
                ..Default::default()
            };
            let body = c
                .post_json("/v1/users", "vam.v1.CreateUserRequest", &req, None)
                .await?;
            client::print_json(&body);
        }
    }
    Ok(())
}

/// The password comes from the named env var, as `vam login` does. Errors name
/// the variable, never its value.
fn read_password_env(var: &str) -> Result<String> {
    let password = std::env::var(var).with_context(|| format!("read env {var}"))?;
    if password.is_empty() {
        bail!("env {var} is empty; set it to the new user's password");
    }
    Ok(password)
}

/// `USER ID  USERNAME  ROLE  STATE`.
pub fn format_users(body: &str) -> String {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return body.to_string();
    };
    let users = v["users"].as_array().cloned().unwrap_or_default();
    if users.is_empty() {
        return "no users".to_string();
    }
    let text = |u: &serde_json::Value, k: &str| u[k].as_str().unwrap_or("").to_string();
    let rows: Vec<Vec<String>> = users
        .iter()
        .map(|u| {
            let state = if u["deactivated"].as_bool().unwrap_or(false) {
                "deactivated"
            } else {
                "active"
            };
            vec![
                text(u, "userId"),
                text(u, "username"),
                text(u, "role"),
                state.to_string(),
            ]
        })
        .collect();
    super::access::table(&["USER ID", "USERNAME", "ROLE", "STATE"], &rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn users_table_shows_ids_and_state() {
        let body = r#"{"users":[{"userId":"usr_1","username":"ada","role":"member"},{"userId":"usr_2","username":"bob","role":"publisher","deactivated":true}]}"#;
        let out = format_users(body);
        assert!(out.starts_with("USER ID"), "{out}");
        assert!(out.contains("usr_1") && out.contains("active"), "{out}");
        assert!(out.contains("deactivated"), "{out}");
    }
}
