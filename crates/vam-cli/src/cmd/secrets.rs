use std::fs;

use anyhow::{Context, Result};
use clap::Subcommand;
use vam_spec::v1::PutHostSecretRequest;

use crate::client::{self, Client};
use crate::config;

#[derive(Subcommand)]
pub enum SecretsCmd {
    Set {
        binding_id: String,
        #[arg(long, default_value = "inference")]
        kind: String,
        #[arg(long)]
        value: Option<String>,
        #[arg(long)]
        from_env: Option<String>,
        #[arg(long)]
        file: Option<String>,
        /// Repeatable destination allowlist for egress secrets (e.g. *.atlassian.net).
        #[arg(long = "allowed-destination")]
        allowed_destination: Vec<String>,
        /// Vault partition: `org` (admin/operator) or `user` (caller only).
        /// Omitted: org for operator/org_admin, user otherwise. No user→org fallback.
        #[arg(long, value_parser = ["org", "user"])]
        scope: Option<String>,
        /// Reuse this idempotency key (default: mint a fresh key).
        #[arg(long)]
        idempotency_key: Option<String>,
    },
    List {
        #[arg(long, value_parser = ["org", "user"])]
        scope: Option<String>,
    },
    Get {
        binding_id: String,
        #[arg(long, value_parser = ["org", "user"])]
        scope: Option<String>,
    },
    Delete {
        binding_id: String,
        #[arg(long, value_parser = ["org", "user"])]
        scope: Option<String>,
        /// Reuse this idempotency key (default: mint a fresh key).
        #[arg(long)]
        idempotency_key: Option<String>,
    },
}

pub async fn run(host: Option<String>, token: Option<String>, cmd: SecretsCmd) -> Result<()> {
    let cfg = config::resolve(host, token).await?;
    let c = Client::new(&cfg)?;
    match cmd {
        SecretsCmd::Set {
            binding_id,
            kind,
            value,
            from_env,
            file,
            allowed_destination,
            scope,
            idempotency_key,
        } => {
            let secret_value = resolve_secret_value(value, from_env, file)?;
            let kind_enum = match kind.as_str() {
                "inference" => 1,
                "egress" => 2,
                other => anyhow::bail!(
                    "Wrong: kind '{other}' is not a secret kind.\nExpected: inference or egress.\nFix: vam secrets set <BINDING_ID> --kind inference --from-env <VAR>."
                ),
            };
            let req = PutHostSecretRequest {
                binding_id,
                value: secret_value,
                kind: kind_enum,
                allowed_destinations: allowed_destination,
                scope: scope_to_proto(scope.as_deref()),
                ..Default::default()
            };
            let body = c
                .put_json(
                    "/v1/secrets",
                    "vam.v1.PutHostSecretRequest",
                    &req,
                    idempotency_key.as_deref(),
                )
                .await?;
            client::print_json(&body);
        }
        SecretsCmd::List { scope } => {
            let path = match scope.as_deref() {
                Some(scope) => format!("/v1/secrets?scope={scope}"),
                None => "/v1/secrets".to_string(),
            };
            let body = c.get(&path).await?;
            client::print_json(&body);
        }
        SecretsCmd::Get { binding_id, scope } => {
            let mut path = format!("/v1/secrets/{}", urlencoding(&binding_id));
            if let Some(scope) = scope {
                path.push_str("?scope=");
                path.push_str(&scope);
            }
            let body = c.get(&path).await?;
            client::print_json(&body);
        }
        SecretsCmd::Delete {
            binding_id,
            scope,
            idempotency_key,
        } => {
            let mut path = format!("/v1/secrets/{}", urlencoding(&binding_id));
            if let Some(scope) = scope {
                path.push_str("?scope=");
                path.push_str(&scope);
            }
            let body = c.delete(&path, idempotency_key.as_deref()).await?;
            client::print_json(&body);
        }
    }
    Ok(())
}

fn resolve_secret_value(
    value: Option<String>,
    from_env: Option<String>,
    file: Option<String>,
) -> Result<String> {
    if let Some(v) = value {
        return Ok(v);
    }
    if let Some(var) = from_env {
        return std::env::var(&var).with_context(|| format!("read env {}", var));
    }
    if let Some(path) = file {
        return fs::read_to_string(&path)
            .with_context(|| format!("read secret file {:?}", path))
            .map(|s| s.trim().to_string());
    }
    anyhow::bail!(
        "Wrong: no secret value was given.\nExpected: --value, --from-env, or --file.\nFix: vam secrets set <BINDING_ID> --from-env <VAR>."
    )
}

fn urlencoding(s: &str) -> String {
    s.replace('/', "%2F").replace(':', "%3A")
}

fn scope_to_proto(scope: Option<&str>) -> i32 {
    match scope {
        Some("org") => 1,
        Some("user") => 2,
        _ => 0,
    }
}

#[cfg(test)]
mod flag_tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn mutating_secret_commands_accept_idempotency_key() {
        #[derive(Parser)]
        struct Wrap {
            #[command(subcommand)]
            cmd: SecretsCmd,
        }
        let set = Wrap::try_parse_from([
            "vam",
            "set",
            "openai/api-key",
            "--from-env",
            "OPENAI_API_KEY",
            "--idempotency-key",
            "sec",
        ])
        .unwrap();
        assert!(matches!(
            set.cmd,
            SecretsCmd::Set { ref idempotency_key, .. } if idempotency_key.as_deref() == Some("sec")
        ));
        let delete = Wrap::try_parse_from([
            "vam",
            "delete",
            "openai/api-key",
            "--idempotency-key",
            "sec-del",
        ])
        .unwrap();
        assert!(matches!(
            delete.cmd,
            SecretsCmd::Delete { ref idempotency_key, .. } if idempotency_key.as_deref() == Some("sec-del")
        ));
    }
}
