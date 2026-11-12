use std::io::IsTerminal;

use anyhow::Result;
use clap::{ColorChoice, Parser, Subcommand};

use vam_cli::{cmd, output};

/// `vam --version`: the crate version and the git commit it was built from.
const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), " (", env!("VAM_GIT_SHA"), ")");

#[derive(Parser)]
#[command(
    name = "vam",
    version = VERSION,
    about = "VAM control-plane CLI (HTTP client for vamd)"
)]
struct Cli {
    #[arg(long, global = true)]
    host: Option<String>,

    #[arg(long, global = true)]
    token: Option<String>,

    /// Emit machine-readable JSON on stdout (errors too); never prompt.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Config {
        #[command(subcommand)]
        command: cmd::config::ConfigCmd,
    },
    /// Exchange username + password for a short-lived session token.
    Login {
        #[arg(long)]
        username: Option<String>,
        #[arg(long)]
        password_from_env: Option<String>,
    },
    /// Revoke the stored session token and remove it from local config.
    Logout,
    /// Change your own password. Signs out every session; run `vam login` after.
    Passwd(cmd::passwd::PasswdArgs),
    /// Manage control-plane users (org admin).
    Users {
        #[command(subcommand)]
        command: cmd::users::UsersCmd,
    },
    Health,
    Node {
        #[command(subcommand)]
        command: cmd::node::NodeCmd,
    },
    /// Connectors this node offers: list them, (org admin) enable or disable one, and check or
    /// disconnect your own Microsoft 365 connection.
    Connectors {
        #[command(subcommand)]
        command: cmd::connectors::ConnectorsCmd,
    },
    /// Git marketplaces this node trusts (org admin): list, add by URL, enable or disable, remove,
    /// refresh (read now), and show what each listing installs.
    Marketplace {
        #[command(subcommand)]
        command: cmd::marketplace::MarketplaceCmd,
    },
    Keys {
        #[command(subcommand)]
        command: cmd::keys::KeysCmd,
    },
    /// Grants brokers hold for you (same as `vam keys list --grants`).
    Grants {
        #[command(subcommand)]
        command: cmd::keys::GrantsCmd,
    },
    Secrets {
        #[command(subcommand)]
        command: cmd::secrets::SecretsCmd,
    },
    /// Remote MCP server connections (Jira, Linear, Sentry).
    Mcp {
        #[command(subcommand)]
        command: cmd::mcp::McpCmd,
    },
    Agent {
        #[command(subcommand)]
        command: cmd::catalog::AgentCmd,
    },
    Playbook {
        #[command(subcommand)]
        command: cmd::catalog::PlaybookCmd,
    },
    Policy {
        #[command(subcommand)]
        command: cmd::catalog::PolicyCmd,
    },
    Skill {
        #[command(subcommand)]
        command: cmd::catalog::SkillCmd,
    },
    /// Catalog-wide operations: check packages and marketplaces offline.
    Catalog {
        #[command(subcommand)]
        command: cmd::catalog::CatalogCmd,
    },
    Session {
        #[command(subcommand)]
        command: cmd::session::SessionCmd,
    },
    /// Run an installed agent, or manage runs.
    Run(cmd::run::RunArgs),
    Approve {
        #[command(subcommand)]
        command: cmd::approve::ApproveCmd,
    },
    /// What waits for you: list, show and mark read.
    Notifications(cmd::notifications::NotificationsArgs),
    Evidence {
        #[command(subcommand)]
        command: cmd::evidence::EvidenceCmd,
    },
    /// Show a run's signed receipt, or verify a receipt file offline.
    Receipt {
        #[command(subcommand)]
        command: cmd::receipt::ReceiptCmd,
    },
    /// List or download committed Drive9 workspace HEAD files.
    Workspace {
        #[command(subcommand)]
        command: cmd::workspace::WorkspaceCmd,
    },
    /// List, create and rename Spaces (a Space is reached by its `sp_…` id).
    Space {
        #[command(subcommand)]
        command: cmd::space::SpaceCmd,
    },
    /// Read a Space's conversations (threads).
    Thread {
        #[command(subcommand)]
        command: cmd::thread::ThreadCmd,
    },
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let color = if std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none() {
        ColorChoice::Auto
    } else {
        ColorChoice::Never
    };
    let cli = match <Cli as clap::CommandFactory>::command()
        .color(color)
        .try_get_matches()
        .and_then(|m| <Cli as clap::FromArgMatches>::from_arg_matches(&m))
    {
        Ok(cli) => cli,
        Err(e) => {
            let _ = e.print();
            let code = if e.use_stderr() {
                output::ExitCode::Usage
            } else {
                output::ExitCode::Ok
            };
            return std::process::ExitCode::from(code as u8);
        }
    };
    output::set_json(cli.json);
    let host_flag = cli.host.clone();
    match dispatch(cli).await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            // A wrong scheme or an untrusted certificate gets a guided error.
            let e = match vam_cli::config::resolve_host(host_flag) {
                Ok(host) => vam_cli::transport::explain(e, &host).await,
                Err(_) => e,
            };
            std::process::ExitCode::from(output::report(&e) as u8)
        }
    }
}

async fn dispatch(cli: Cli) -> Result<()> {
    match cli.command {
        Commands::Config { command } => cmd::config::run(command),
        Commands::Login {
            username,
            password_from_env,
        } => cmd::login::run_login(cli.host, username, password_from_env).await,
        Commands::Logout => cmd::login::run_logout(cli.host).await,
        Commands::Passwd(args) => cmd::passwd::run(cli.host, cli.token, args).await,
        Commands::Users { command } => cmd::users::run(cli.host, cli.token, command).await,
        Commands::Health => cmd::health::run(cli.host, cli.token).await,
        Commands::Node { command } => cmd::node::run(cli.host, cli.token, command).await,
        Commands::Connectors { command } => {
            cmd::connectors::run(cli.host, cli.token, command).await
        }
        Commands::Marketplace { command } => {
            cmd::marketplace::run(cli.host, cli.token, command).await
        }
        Commands::Keys { command } => cmd::keys::run(cli.host, cli.token, command).await,
        Commands::Grants { command } => cmd::keys::run_grants(cli.host, cli.token, command).await,
        Commands::Secrets { command } => cmd::secrets::run(cli.host, cli.token, command).await,
        Commands::Mcp { command } => cmd::mcp::run(cli.host, cli.token, command).await,
        Commands::Agent { command } => cmd::catalog::run_agent(cli.host, cli.token, command).await,
        Commands::Playbook { command } => {
            cmd::catalog::run_playbook(cli.host, cli.token, command).await
        }
        Commands::Policy { command } => {
            let host = cmd::catalog::policy_server_host(cli.host, &command);
            cmd::catalog::run_policy(host, cli.token, command).await
        }
        Commands::Skill { command } => cmd::catalog::run_skill(cli.host, cli.token, command).await,
        Commands::Catalog { command } => {
            cmd::catalog::run_catalog(cli.host, cli.token, command).await
        }
        Commands::Session { command } => cmd::session::run(cli.host, cli.token, command).await,
        Commands::Run(args) => cmd::run::run(cli.host, cli.token, args).await,
        Commands::Approve { command } => cmd::approve::run(cli.host, cli.token, command).await,
        Commands::Notifications(args) => cmd::notifications::run(cli.host, cli.token, args).await,
        Commands::Evidence { command } => cmd::evidence::run(cli.host, cli.token, command).await,
        Commands::Receipt { command } => cmd::receipt::run(cli.host, cli.token, command).await,
        Commands::Workspace { command } => cmd::workspace::run(cli.host, cli.token, command).await,
        Commands::Space { command } => cmd::space::run(cli.host, cli.token, command).await,
        Commands::Thread { command } => cmd::thread::run(cli.host, cli.token, command).await,
    }
}

#[cfg(test)]
mod tests {
    use super::Cli;
    use clap::CommandFactory;

    /// The help texts of `cmd` and everything under it, with the command path for messages.
    fn help_texts(cmd: &clap::Command, path: &str, out: &mut Vec<(String, String)>) {
        let mut texts: Vec<String> = Vec::new();
        texts.extend(cmd.get_about().map(|s| s.to_string()));
        texts.extend(cmd.get_long_about().map(|s| s.to_string()));
        for arg in cmd.get_arguments() {
            texts.extend(arg.get_help().map(|s| s.to_string()));
            texts.extend(arg.get_long_help().map(|s| s.to_string()));
        }
        out.extend(texts.into_iter().map(|t| (path.to_string(), t)));
        for sub in cmd.get_subcommands() {
            help_texts(sub, &format!("{path} {}", sub.get_name()), out);
        }
    }

    /// Follows the words of `vam a b ...` through the command tree; the first word that should
    /// be a subcommand and is not one is returned as the error.
    fn resolve(root: &clap::Command, words: &[&str]) -> Result<(), String> {
        let mut cmd = root;
        for word in words {
            if word.starts_with('-') || word.starts_with('<') || word.contains("...") {
                return Ok(());
            }
            if cmd.get_subcommands().next().is_none() {
                return Ok(()); // the rest are positional values
            }
            match cmd.find_subcommand(word) {
                Some(sub) => cmd = sub,
                None if cmd.get_positionals().next().is_some() => return Ok(()),
                None => return Err(format!("`{}` has no subcommand `{word}`", cmd.get_name())),
            }
        }
        Ok(())
    }

    #[test]
    fn passwd_parses_the_env_flags() {
        use clap::Parser;
        let cli = Cli::try_parse_from([
            "vam",
            "passwd",
            "--current-password-from-env",
            "OLD",
            "--new-password-from-env",
            "NEW",
        ])
        .unwrap();
        match cli.command {
            super::Commands::Passwd(a) => {
                assert_eq!(a.current_password_from_env.as_deref(), Some("OLD"));
                assert_eq!(a.new_password_from_env.as_deref(), Some("NEW"));
            }
            _ => panic!("expected passwd"),
        }
        // No flag takes the password itself.
        assert!(Cli::try_parse_from(["vam", "passwd", "--password", "x"]).is_err());
        assert!(Cli::try_parse_from(["vam", "passwd"]).is_ok());
    }

    #[test]
    fn every_vam_command_named_in_help_exists() {
        let root = Cli::command();
        let mut texts = Vec::new();
        help_texts(&root, "vam", &mut texts);
        let mut bad = Vec::new();
        for (path, text) in texts {
            for (i, span) in text.split('`').enumerate() {
                // Odd pieces are inside backticks.
                if i % 2 == 0 {
                    continue;
                }
                let Some(rest) = span.strip_prefix("vam ") else {
                    continue;
                };
                let words: Vec<&str> = rest.split_whitespace().collect();
                if let Err(e) = resolve(&root, &words) {
                    bad.push(format!("{path}: `{span}`: {e}"));
                }
            }
        }
        assert!(
            bad.is_empty(),
            "help names commands that do not exist:\n{}",
            bad.join("\n")
        );
    }
}
