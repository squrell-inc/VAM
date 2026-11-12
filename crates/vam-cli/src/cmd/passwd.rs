//! `vam passwd`: change your own password.
//!
//! Both passwords stay out of argv: they are typed at a hidden prompt, or read from the
//! environment variables the flags name. On success the node has signed out every session of
//! the user, so the stored session is dropped from the config file too.

use anyhow::{Context, Result};
use clap::Args;
use vam_spec::v1::ChangeMyPasswordRequest;

use crate::client::Client;
use crate::config::{self, ConfigFile};
use crate::output::{self, ExitCode};

#[derive(Args, Debug, Default)]
pub struct PasswdArgs {
    /// Name of the env var that holds your current password (instead of the prompt).
    #[arg(long, value_name = "VAR")]
    pub current_password_from_env: Option<String>,
    /// Name of the env var that holds the new password (instead of the prompt).
    #[arg(long, value_name = "VAR")]
    pub new_password_from_env: Option<String>,
}

/// Where a typed password comes from, so tests can stand in for the terminal.
pub trait Prompter {
    fn ask(&mut self, label: &str, flag: &str) -> Result<String>;
}

/// Hidden prompt on the terminal. Refuses, naming the flag, when there is no terminal or
/// `--json` is set.
struct TerminalPrompter;

impl Prompter for TerminalPrompter {
    fn ask(&mut self, label: &str, flag: &str) -> Result<String> {
        if !output::stdin_is_tty() || output::is_json() {
            return Err(output::fail(
                ExitCode::Usage,
                format!("stdin is not a terminal; pass {flag} instead of the interactive prompt"),
            ));
        }
        rpassword::prompt_password(label).context("read password")
    }
}

pub async fn run(host: Option<String>, token: Option<String>, args: PasswdArgs) -> Result<()> {
    let (current, new) = collect_passwords(&args, &mut TerminalPrompter)?;
    let cfg = config::resolve(host, token).await?;
    let client = Client::new(&cfg)?;
    let mut file = ConfigFile::load().unwrap_or_default();
    change_and_clear_session(&client, &current, &new, &mut file).await?;
    file.save()?;
    output::emit(
        serde_json::json!({"passwordChanged": true, "reloginRequired": true}),
        || "password changed; every session was signed out. Run vam login.".to_string(),
    );
    Ok(())
}

/// Change the password; only after the node accepted it, drop the stored session from `file`
/// (the node signed every session out). A failure leaves `file` as it was. The API key a user
/// may have stored stays: the node keeps API keys on a password change.
pub async fn change_and_clear_session(
    client: &Client,
    current: &str,
    new: &str,
    file: &mut ConfigFile,
) -> Result<()> {
    change_password(client, current, new).await?;
    file.clear_session();
    Ok(())
}

/// One `POST /v1/me/password` with a fresh `Idempotency-Key`.
pub async fn change_password(client: &Client, current: &str, new: &str) -> Result<String> {
    let req = ChangeMyPasswordRequest {
        current_password: current.to_string(),
        new_password: new.to_string(),
    };
    client
        .post_json(
            "/v1/me/password",
            "vam.v1.ChangeMyPasswordRequest",
            &req,
            None,
        )
        .await
}

/// Current and new password, from the env vars the flags name or from hidden prompts.
/// A typed new password is asked twice; a mismatch is a usage error and nothing is sent.
pub fn collect_passwords(
    args: &PasswdArgs,
    prompter: &mut dyn Prompter,
) -> Result<(String, String)> {
    let current = match &args.current_password_from_env {
        Some(var) => read_password_env(var, "your current password")?,
        None => prompter.ask("Current password: ", "--current-password-from-env <VAR>")?,
    };
    let new = match &args.new_password_from_env {
        Some(var) => read_password_env(var, "the new password")?,
        None => {
            let flag = "--new-password-from-env <VAR>";
            let first = prompter.ask("New password: ", flag)?;
            let again = prompter.ask("Repeat new password: ", flag)?;
            if first != again {
                return Err(output::fail(
                    ExitCode::Usage,
                    "the two new passwords do not match; nothing was sent",
                ));
            }
            first
        }
    };
    Ok((current, new))
}

/// Errors name the variable, never its value.
fn read_password_env(var: &str, what: &str) -> Result<String> {
    let value = std::env::var(var).with_context(|| format!("read env {var}"))?;
    if value.is_empty() {
        anyhow::bail!("env {var} is empty; set it to {what}");
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    struct Scripted(VecDeque<&'static str>, Vec<String>);

    impl Prompter for Scripted {
        fn ask(&mut self, label: &str, _flag: &str) -> Result<String> {
            self.1.push(label.to_string());
            Ok(self.0.pop_front().unwrap_or_default().to_string())
        }
    }

    fn scripted(lines: &[&'static str]) -> Scripted {
        Scripted(lines.iter().copied().collect(), Vec::new())
    }

    #[test]
    fn prompts_ask_current_new_and_repeat_in_order() {
        let mut p = scripted(&["old-pw", "new-pw", "new-pw"]);
        let (cur, new) = collect_passwords(&PasswdArgs::default(), &mut p).unwrap();
        assert_eq!((cur.as_str(), new.as_str()), ("old-pw", "new-pw"));
        assert_eq!(
            p.1,
            [
                "Current password: ",
                "New password: ",
                "Repeat new password: "
            ]
        );
    }

    #[test]
    fn mismatched_repeat_is_a_usage_error() {
        let mut p = scripted(&["old-pw", "new-pw", "other-pw"]);
        let err = collect_passwords(&PasswdArgs::default(), &mut p).unwrap_err();
        assert_eq!(output::classify(&err).0, ExitCode::Usage);
        assert!(err.to_string().contains("nothing was sent"), "{err}");
    }

    #[test]
    fn env_flags_are_read_without_prompting() {
        std::env::set_var("VAM_TEST_PASSWD_CUR", "cur-secret");
        std::env::set_var("VAM_TEST_PASSWD_NEW", "new-secret");
        let args = PasswdArgs {
            current_password_from_env: Some("VAM_TEST_PASSWD_CUR".into()),
            new_password_from_env: Some("VAM_TEST_PASSWD_NEW".into()),
        };
        let mut p = scripted(&[]);
        let (cur, new) = collect_passwords(&args, &mut p).unwrap();
        assert_eq!((cur.as_str(), new.as_str()), ("cur-secret", "new-secret"));
        assert!(p.1.is_empty(), "no prompt expected, got {:?}", p.1);
    }

    #[test]
    fn a_missing_or_empty_env_var_names_the_variable_not_a_value() {
        std::env::set_var("VAM_TEST_PASSWD_EMPTY", "");
        let args = PasswdArgs {
            current_password_from_env: Some("VAM_TEST_PASSWD_EMPTY".into()),
            new_password_from_env: Some("VAM_TEST_PASSWD_UNSET".into()),
        };
        let err = collect_passwords(&args, &mut scripted(&[])).unwrap_err();
        assert!(err.to_string().contains("VAM_TEST_PASSWD_EMPTY"), "{err}");

        std::env::set_var("VAM_TEST_PASSWD_CUR2", "x");
        let args = PasswdArgs {
            current_password_from_env: Some("VAM_TEST_PASSWD_CUR2".into()),
            new_password_from_env: Some("VAM_TEST_PASSWD_UNSET".into()),
        };
        let err = collect_passwords(&args, &mut scripted(&[])).unwrap_err();
        assert!(err.to_string().contains("VAM_TEST_PASSWD_UNSET"), "{err}");
    }
}
