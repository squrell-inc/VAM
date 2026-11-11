use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ConfigFile {
    pub host: Option<String>,
    /// A PEM file with the node's certificate to trust (`vam config set-ca`; `VAM_CA_FILE` wins).
    #[serde(default)]
    pub ca_file: Option<String>,
    pub api_key: Option<String>,
    pub session_token: Option<String>,
    pub refresh_token: Option<String>,
    pub session_expires_at: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SessionCreds {
    #[serde(default, alias = "userId")]
    pub user_id: String,
    #[serde(default)]
    pub username: String,
    pub token: String,
    #[serde(alias = "expiresAt")]
    pub expires_at: i64,
    #[serde(alias = "refreshToken")]
    pub refresh_token: String,
    #[serde(default, alias = "refreshExpiresAt")]
    pub refresh_expires_at: i64,
}

/// vamd serves TLS by default; a node started with `--insecure-dev` needs an explicit `http://` host.
pub const DEFAULT_HOST: &str = "https://127.0.0.1:8080";

pub struct ResolvedConfig {
    pub host: String,
    pub api_key: String,
}

impl ConfigFile {
    pub fn path() -> PathBuf {
        dirs_config_path()
    }

    pub fn load() -> Result<Self> {
        Self::load_from(&Self::path())
    }

    pub fn load_from(path: &std::path::Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let contents = fs::read_to_string(path).with_context(|| format!("read {:?}", path))?;
        toml::from_str(&contents).with_context(|| format!("parse {:?}", path))
    }

    pub fn save(&self) -> Result<()> {
        self.save_to(&Self::path())
    }

    pub fn save_to(&self, path: &std::path::Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let contents = toml::to_string_pretty(self)?;
        fs::write(path, contents)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
        }
        Ok(())
    }

    pub fn apply_session(&mut self, issued: &SessionCreds) {
        self.session_token = Some(issued.token.clone());
        self.refresh_token = Some(issued.refresh_token.clone());
        self.session_expires_at = Some(issued.expires_at);
    }

    pub fn clear_session(&mut self) {
        self.session_token = None;
        self.refresh_token = None;
        self.session_expires_at = None;
    }

    pub fn session_expired(&self, now: i64) -> bool {
        match self.session_expires_at {
            Some(exp) => exp <= now,
            None => false,
        }
    }
}

pub const SESSION_EXPIRED_MESSAGE: &str = "session expired, run `vam login`";

pub async fn resolve(
    host_flag: Option<String>,
    token_flag: Option<String>,
) -> Result<ResolvedConfig> {
    let mut file = ConfigFile::load().unwrap_or_default();
    let host = resolve_host_from(&file, host_flag.clone(), std::env::var("VAM_HOST").ok());
    let env_api_key = std::env::var("VAM_API_KEY").ok().filter(|s| !s.is_empty());
    if token_flag.as_ref().map(|s| s.is_empty()).unwrap_or(true) && env_api_key.is_none() {
        maybe_refresh_session(&host, &mut file).await?;
    }
    resolve_from(
        &file,
        host_flag,
        token_flag,
        std::env::var("VAM_HOST").ok(),
        env_api_key,
    )
}

pub fn resolve_from(
    file: &ConfigFile,
    host_flag: Option<String>,
    token_flag: Option<String>,
    env_host: Option<String>,
    env_api_key: Option<String>,
) -> Result<ResolvedConfig> {
    let host = resolve_host_from(file, host_flag, env_host);
    let api_key = token_flag
        .filter(|s| !s.is_empty())
        .or_else(|| env_api_key.filter(|s| !s.is_empty()))
        .or_else(|| file.session_token.clone().filter(|s| !s.is_empty()))
        .or_else(|| file.api_key.clone().filter(|s| !s.is_empty()))
        .ok_or_else(|| {
            crate::output::fail(
                crate::output::ExitCode::Auth,
                "missing API key: set --token, VAM_API_KEY, run `vam login`, or `vam config set-key`",
            )
        })?;

    Ok(ResolvedConfig { host, api_key })
}

pub fn resolve_host(host_flag: Option<String>) -> Result<String> {
    let file = ConfigFile::load().unwrap_or_default();
    Ok(resolve_host_from(
        &file,
        host_flag,
        std::env::var("VAM_HOST").ok(),
    ))
}

fn resolve_host_from(
    file: &ConfigFile,
    host_flag: Option<String>,
    env_host: Option<String>,
) -> String {
    host_flag
        .filter(|s| !s.is_empty())
        .or_else(|| env_host.filter(|s| !s.is_empty()))
        .or_else(|| file.host.clone())
        .unwrap_or_else(|| DEFAULT_HOST.to_string())
}

pub async fn maybe_refresh_session(host: &str, file: &mut ConfigFile) -> Result<()> {
    let now = unix_timestamp();
    if !file.session_expired(now) {
        return Ok(());
    }
    let Some(refresh) = file.refresh_token.clone().filter(|s| !s.is_empty()) else {
        if file.session_token.is_some() {
            return Err(crate::output::fail(
                crate::output::ExitCode::Auth,
                SESSION_EXPIRED_MESSAGE,
            ));
        }
        return Ok(());
    };
    match crate::client::refresh_session(host, &refresh).await {
        Ok(issued) => {
            file.apply_session(&issued);
            file.save().context("save config after session refresh")?;
            Ok(())
        }
        Err(_) => Err(crate::output::fail(
            crate::output::ExitCode::Auth,
            SESSION_EXPIRED_MESSAGE,
        )),
    }
}

pub fn redact_key(key: &str) -> String {
    if key.len() <= 8 {
        return "***".to_string();
    }
    format!("{}...{}", &key[..4], &key[key.len() - 4..])
}

/// Interactive prompt. Fails with exit code `usage` when stdin is not a TTY
/// or `--json` is set; `flag` names the non-interactive equivalent.
pub fn prompt(label: &str, flag: &str) -> Result<String> {
    use std::io::{self, Write};
    if !crate::output::stdin_is_tty() || crate::output::is_json() {
        return Err(crate::output::fail(
            crate::output::ExitCode::Usage,
            format!("stdin is not a terminal; pass {flag} instead of the interactive prompt"),
        ));
    }
    print!("{label}");
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    Ok(line.trim().to_string())
}

fn unix_timestamp() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn dirs_config_path() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        return PathBuf::from(xdg).join("vam/config.toml");
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home).join(".config/vam/config.toml");
    }
    PathBuf::from(".config/vam/config.toml")
}
