use anyhow::{Context, Result};
use clap::Subcommand;

use crate::config;

#[derive(Subcommand)]
pub enum ConfigCmd {
    #[command(name = "set-host")]
    SetHost {
        url: String,
    },
    /// Trust the node's own certificate (a PEM file; for example `<state dir>/tls/ca.pem`).
    #[command(name = "set-ca")]
    SetCa {
        path: String,
    },
    #[command(name = "set-key")]
    SetKey {
        #[arg(long)]
        from_env: Option<String>,
    },
    Show,
}

pub fn run(cmd: ConfigCmd) -> Result<()> {
    match cmd {
        ConfigCmd::SetHost { url } => {
            let mut file = config::ConfigFile::load().unwrap_or_default();
            file.host = Some(url);
            file.save()?;
            saved("host");
        }
        ConfigCmd::SetCa { path } => {
            let pem = std::fs::read(&path).with_context(|| format!("read {path}"))?;
            let der = crate::transport::parse_pem_cert(&pem)
                .with_context(|| format!("{path} is not a PEM certificate"))?;
            let abs = std::fs::canonicalize(&path).with_context(|| format!("resolve {path}"))?;
            let mut file = config::ConfigFile::load().unwrap_or_default();
            file.ca_file = Some(abs.display().to_string());
            file.save()?;
            let fp = crate::transport::fingerprint(&der);
            crate::output::emit(
                serde_json::json!({
                    "saved": "ca_file",
                    "caFile": abs.display().to_string(),
                    "fingerprintSha256": fp,
                    "config": config::ConfigFile::path().display().to_string(),
                }),
                || {
                    format!("ca_file saved: {}\nsha256 fingerprint: {fp}\n(compare it with the fingerprint vamd logged)", abs.display())
                },
            );
        }
        ConfigCmd::SetKey { from_env } => {
            let key = if let Some(var) = from_env {
                std::env::var(&var).with_context(|| format!("read env {}", var))?
            } else {
                config::prompt("API key: ", "--from-env <VAR>")?
            };
            let mut file = config::ConfigFile::load().unwrap_or_default();
            file.api_key = Some(key);
            file.save()?;
            saved("api_key");
        }
        ConfigCmd::Show => {
            let file = config::ConfigFile::load().unwrap_or_default();
            let host = file
                .host
                .or_else(|| std::env::var("VAM_HOST").ok())
                .unwrap_or_else(|| config::DEFAULT_HOST.to_string());
            let ca_file = crate::transport::ca_file().map(|p| p.display().to_string());
            let key_display = file
                .api_key
                .as_deref()
                .map(config::redact_key)
                .unwrap_or_else(|| "(not set)".into());
            let session_display = file
                .session_token
                .as_deref()
                .map(config::redact_key)
                .unwrap_or_else(|| "(not set)".into());
            let path = config::ConfigFile::path();
            crate::output::emit(
                serde_json::json!({
                    "host": host,
                    "caFile": ca_file,
                    "apiKey": file.api_key.as_deref().map(config::redact_key),
                    "sessionToken": file.session_token.as_deref().map(config::redact_key),
                    "sessionExpiresAt": file.session_expires_at,
                    "config": path.display().to_string(),
                }),
                || {
                    let mut s = format!(
                        "host: {host}\napi_key: {key_display}\nsession_token: {session_display}\n"
                    );
                    if let Some(ca) = &ca_file {
                        s.push_str(&format!("ca_file: {ca}\n"));
                    }
                    if let Some(exp) = file.session_expires_at {
                        s.push_str(&format!("session_expires_at: {exp}\n"));
                    }
                    s.push_str(&format!("config: {path:?}"));
                    s
                },
            );
        }
    }
    Ok(())
}

fn saved(field: &str) {
    let path = config::ConfigFile::path();
    crate::output::emit(
        serde_json::json!({"saved": field, "config": path.display().to_string()}),
        || format!("{field} saved to {path:?}"),
    );
}
