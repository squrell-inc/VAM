use anyhow::{Context, Result};

use crate::client;
use crate::config::{self, ConfigFile, SessionCreds};

pub async fn run_login(
    host: Option<String>,
    username: Option<String>,
    password_from_env: Option<String>,
) -> Result<()> {
    let username = match username {
        Some(name) if !name.trim().is_empty() => name,
        _ => config::prompt("Username: ", "--username <NAME>")?,
    };
    let password = if let Some(var) = password_from_env {
        std::env::var(&var).with_context(|| format!("read env {var}"))?
    } else {
        config::prompt("Password: ", "--password-from-env <VAR>")?
    };
    let host = config::resolve_host(host)?;
    let mut file = ConfigFile::load().unwrap_or_default();
    let issued = login_into(&host, &username, &password, &mut file).await?;
    file.save()?;
    crate::output::emit(
        serde_json::json!({
            "username": issued.username,
            "expiresAt": issued.expires_at,
            "config": ConfigFile::path().display().to_string(),
        }),
        || {
            format!(
                "logged in as {} (token stored in {:?})",
                issued.username,
                ConfigFile::path()
            )
        },
    );
    Ok(())
}

pub async fn run_logout(host: Option<String>) -> Result<()> {
    let host = config::resolve_host(host)?;
    let mut file = ConfigFile::load().unwrap_or_default();
    logout_from(&host, &mut file).await?;
    file.save()?;
    crate::output::emit(
        serde_json::json!({"loggedOut": true, "config": ConfigFile::path().display().to_string()}),
        || format!("logged out ({:?})", ConfigFile::path()),
    );
    Ok(())
}

pub async fn login_into(
    host: &str,
    username: &str,
    password: &str,
    file: &mut ConfigFile,
) -> Result<SessionCreds> {
    let issued = client::login(host, username, password).await?;
    apply_issued_session(file, host, &issued);
    Ok(issued)
}

pub async fn logout_from(host: &str, file: &mut ConfigFile) -> Result<()> {
    if let Some(token) = file.session_token.clone() {
        match client::logout(host, &token).await {
            Ok(_) => {}
            Err(err) => {
                // Always clear the local session; the server may already have
                // expired or revoked the token.
                eprintln!("logout revoke: {err}");
            }
        }
    }
    file.clear_session();
    Ok(())
}

pub fn apply_issued_session(file: &mut ConfigFile, host: &str, issued: &SessionCreds) {
    if file.host.is_none() {
        file.host = Some(host.trim_end_matches('/').to_string());
    }
    file.apply_session(issued);
}
