//! Machine-readable output, stable exit codes, and TTY detection.
//!
//! Contract (see `docs/src/content/docs/getting-started/cli.md`):
//! - `--json`: stdout carries exactly one JSON document per command (NDJSON
//!   for streaming commands such as `run events --follow`). Failures print
//!   `{"error":{"code","exitCode","message","httpStatus"}}` to stdout.
//! - Exit codes are listed in [`ExitCode`] and never change meaning.
//! - Prompts require a TTY on stdin; otherwise the command fails with
//!   [`ExitCode::Usage`] and names the flag to use instead.

use std::io::IsTerminal;
use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::{json, Value};

static JSON_MODE: AtomicBool = AtomicBool::new(false);

pub fn set_json(on: bool) {
    JSON_MODE.store(on, Ordering::Relaxed);
}

pub fn is_json() -> bool {
    JSON_MODE.load(Ordering::Relaxed)
}

pub fn stdin_is_tty() -> bool {
    std::io::stdin().is_terminal()
}

pub fn stdout_is_tty() -> bool {
    std::io::stdout().is_terminal()
}

/// Print a structured result: JSON in `--json` mode, otherwise `human`.
pub fn emit(value: Value, human: impl FnOnce() -> String) {
    if is_json() {
        println!("{value}");
    } else {
        println!("{}", human());
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ExitCode {
    Ok = 0,
    Failure = 1,
    Usage = 2,
    Auth = 3,
    NotFound = 4,
    Conflict = 5,
    Invalid = 6,
    Server = 7,
    Unreachable = 8,
    VerifyFailed = 9,
}

impl ExitCode {
    pub fn name(self) -> &'static str {
        match self {
            ExitCode::Ok => "ok",
            ExitCode::Failure => "failure",
            ExitCode::Usage => "usage",
            ExitCode::Auth => "auth",
            ExitCode::NotFound => "not_found",
            ExitCode::Conflict => "conflict",
            ExitCode::Invalid => "invalid",
            ExitCode::Server => "server",
            ExitCode::Unreachable => "unreachable",
            ExitCode::VerifyFailed => "verify_failed",
        }
    }
}

/// An error that carries its own exit code.
#[derive(Debug)]
pub struct CliError {
    pub code: ExitCode,
    pub message: String,
    /// Set for vamd HTTP failures. Not printed. `--json` reports it as `httpStatus`.
    pub http_status: Option<u16>,
}

impl std::fmt::Display for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CliError {}

pub fn fail(code: ExitCode, message: impl Into<String>) -> anyhow::Error {
    CliError {
        code,
        message: message.into(),
        http_status: None,
    }
    .into()
}

pub fn fail_http(code: ExitCode, message: impl Into<String>, status: u16) -> anyhow::Error {
    CliError {
        code,
        message: message.into(),
        http_status: Some(status),
    }
    .into()
}

pub fn exit_code_for_status(status: u16) -> ExitCode {
    match status {
        401 | 403 => ExitCode::Auth,
        404 => ExitCode::NotFound,
        409 => ExitCode::Conflict,
        400 | 413 | 422 => ExitCode::Invalid,
        500..=599 => ExitCode::Server,
        _ => ExitCode::Failure,
    }
}

fn http_status(msg: &str) -> Option<u16> {
    let rest = msg.strip_prefix("HTTP ")?;
    rest.get(..3)?.parse().ok()
}

pub fn classify(err: &anyhow::Error) -> (ExitCode, Option<u16>) {
    for cause in err.chain() {
        if let Some(e) = cause.downcast_ref::<CliError>() {
            return (e.code, e.http_status);
        }
        if let Some(e) = cause.downcast_ref::<reqwest::Error>() {
            if e.is_connect() || e.is_timeout() {
                return (ExitCode::Unreachable, None);
            }
        }
        if let Some(status) = http_status(&cause.to_string()) {
            return (exit_code_for_status(status), Some(status));
        }
    }
    (ExitCode::Failure, None)
}

/// Report `err` on the right stream and return the process exit code.
pub fn report(err: &anyhow::Error) -> ExitCode {
    let (code, status) = classify(err);
    if is_json() {
        println!(
            "{}",
            json!({ "error": {
                "code": code.name(),
                "exitCode": code as u8,
                "message": format!("{err:#}"),
                "httpStatus": status,
            }})
        );
    } else {
        eprintln!("Error: {err:#}");
    }
    code
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_http_status() {
        let e = anyhow::anyhow!("HTTP 404 Not Found: nope");
        assert_eq!(classify(&e), (ExitCode::NotFound, Some(404)));
        let e = anyhow::anyhow!("HTTP 401 Unauthorized: x");
        assert_eq!(classify(&e).0, ExitCode::Auth);
        let e = anyhow::anyhow!("HTTP 503 Service Unavailable: x");
        assert_eq!(classify(&e).0, ExitCode::Server);
    }

    #[test]
    fn cli_error_code_wins() {
        let e = fail(ExitCode::Usage, "no tty").context("login");
        assert_eq!(classify(&e).0, ExitCode::Usage);
    }

    #[test]
    fn unknown_is_failure() {
        assert_eq!(classify(&anyhow::anyhow!("boom")).0, ExitCode::Failure);
    }
}
