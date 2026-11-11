use anyhow::{bail, Context, Result};
use clap::Subcommand;
use vam_spec::v1::CreateUserRequest;

use crate::client::{self, Client};
use crate::config;

#[derive(Subcommand)]
pub enum UsersCmd {
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
