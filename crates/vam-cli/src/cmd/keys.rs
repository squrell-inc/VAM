use anyhow::Result;
use clap::Subcommand;
use vam_spec::v1::CreateApiKeyRequest;

use crate::client::{self, Client};
use crate::config;

#[derive(Subcommand)]
pub enum KeysCmd {
    Create {
        #[arg(long)]
        name: String,
        #[arg(long)]
        scope: Vec<String>,
        /// Bind the key to this user: a user id or a login name (any letter case). Defaults to the caller.
        #[arg(long)]
        user_id: Option<String>,
        /// Reuse this idempotency key (default: mint a fresh key).
        #[arg(long)]
        idempotency_key: Option<String>,
    },
    /// List API keys; `--grants` lists the grants issued to brokers instead.
    List {
        /// Grants (holder, target, message preview, issued via, expiry, last used).
        #[arg(long)]
        grants: bool,
    },
    Revoke {
        id: String,
        /// Reuse this idempotency key (default: mint a fresh key).
        #[arg(long)]
        idempotency_key: Option<String>,
    },
}

/// `vam grants ...`: the grants brokers hold on the user's behalf (alias of `vam keys --grants`).
#[derive(Subcommand)]
pub enum GrantsCmd {
    List,
    Revoke {
        id: String,
        /// Reuse this idempotency key (default: mint a fresh key).
        #[arg(long)]
        idempotency_key: Option<String>,
    },
}

pub async fn run_grants(host: Option<String>, token: Option<String>, cmd: GrantsCmd) -> Result<()> {
    match cmd {
        GrantsCmd::List => run(host, token, KeysCmd::List { grants: true }).await,
        GrantsCmd::Revoke {
            id,
            idempotency_key,
        } => {
            run(
                host,
                token,
                KeysCmd::Revoke {
                    id,
                    idempotency_key,
                },
            )
            .await
        }
    }
}

/// One row per grant: holder, target, message preview, issued via, expiry, last used.
fn grants_table(body: &str) -> String {
    let value: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
    let rows = value
        .get("keys")
        .and_then(|k| k.as_array())
        .cloned()
        .unwrap_or_default();
    if rows.is_empty() {
        return "no grants".to_string();
    }
    let text = |row: &serde_json::Value, key: &str| {
        row.get(key)
            .and_then(|v| v.as_str())
            .filter(|v| !v.is_empty())
            .unwrap_or("-")
            .to_string()
    };
    let when = |row: &serde_json::Value, key: &str| {
        row.get(key)
            .and_then(|t| t.get("seconds"))
            .and_then(|s| {
                s.as_str()
                    .map(str::to_string)
                    .or_else(|| s.as_i64().map(|n| n.to_string()))
            })
            .unwrap_or_else(|| "-".to_string())
    };
    let mut out =
        String::from("ID\tHOLDER\tTARGET\tMESSAGE\tISSUED VIA\tEXPIRES\tLAST USED\tREVOKED\n");
    for row in rows {
        let id = row
            .get("keyId")
            .and_then(|k| k.get("urn"))
            .and_then(|u| u.as_str())
            .unwrap_or("-");
        out.push_str(&format!(
            "{id}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
            text(&row, "holder"),
            text(&row, "targetLabel"),
            text(&row, "messagePreview"),
            text(&row, "issuedVia"),
            when(&row, "expiresAt"),
            when(&row, "lastUsedAt"),
            if row.get("revokedAt").is_some() {
                "yes"
            } else {
                "-"
            },
        ));
    }
    out
}

pub async fn run(host: Option<String>, token: Option<String>, cmd: KeysCmd) -> Result<()> {
    let cfg = config::resolve(host, token).await?;
    let c = Client::new(&cfg)?;
    match cmd {
        KeysCmd::Create {
            name,
            scope,
            user_id,
            idempotency_key,
        } => {
            let req = CreateApiKeyRequest {
                name,
                scopes: scope,
                user_id: user_id.unwrap_or_default(),
                ..Default::default()
            };
            let body = c
                .post_json(
                    "/v1/keys",
                    "vam.v1.CreateApiKeyRequest",
                    &req,
                    idempotency_key.as_deref(),
                )
                .await?;
            client::print_json(&body);
        }
        KeysCmd::List { grants: true } => {
            let body = c.get("/v1/keys?holder=broker").await?;
            println!("{}", grants_table(&body));
        }
        KeysCmd::List { grants: false } => {
            let body = c.get("/v1/keys").await?;
            client::print_json(&body);
        }
        KeysCmd::Revoke {
            id,
            idempotency_key,
        } => {
            let body = c
                .delete(&format!("/v1/keys/{}", id), idempotency_key.as_deref())
                .await?;
            client::print_json(&body);
        }
    }
    Ok(())
}

#[cfg(test)]
mod flag_tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn mutating_key_commands_accept_idempotency_key() {
        #[derive(Parser)]
        struct Wrap {
            #[command(subcommand)]
            cmd: KeysCmd,
        }
        let create = Wrap::try_parse_from([
            "vam",
            "create",
            "--name",
            "ci",
            "--idempotency-key",
            "k-create",
        ])
        .unwrap();
        assert!(matches!(
            create.cmd,
            KeysCmd::Create { ref idempotency_key, .. } if idempotency_key.as_deref() == Some("k-create")
        ));
        let revoke =
            Wrap::try_parse_from(["vam", "revoke", "key-1", "--idempotency-key", "k-revoke"])
                .unwrap();
        let grants = Wrap::try_parse_from(["vam", "list", "--grants"]).unwrap();
        assert!(matches!(grants.cmd, KeysCmd::List { grants: true }));
        let plain = Wrap::try_parse_from(["vam", "list"]).unwrap();
        assert!(matches!(plain.cmd, KeysCmd::List { grants: false }));
        assert!(matches!(
            revoke.cmd,
            KeysCmd::Revoke { ref idempotency_key, .. } if idempotency_key.as_deref() == Some("k-revoke")
        ));
    }

    #[test]
    fn grants_alias_parses_and_the_table_reads_plain() {
        #[derive(Parser)]
        struct Wrap {
            #[command(subcommand)]
            cmd: GrantsCmd,
        }
        assert!(matches!(
            Wrap::try_parse_from(["vam", "list"]).unwrap().cmd,
            GrantsCmd::List
        ));
        assert!(matches!(
            Wrap::try_parse_from(["vam", "revoke", "rg_1", "--idempotency-key", "k"])
                .unwrap()
                .cmd,
            GrantsCmd::Revoke { ref idempotency_key, .. } if idempotency_key.as_deref() == Some("k")
        ));
        let table = grants_table(
            r#"{"keys":[{"keyId":{"urn":"rg_1"},"holder":"broker:vam.schedule","targetLabel":"assistant / daily","messagePreview":"hello","issuedVia":"invoke","expiresAt":{"seconds":"99"}}]}"#,
        );
        assert!(
            table.contains("rg_1\tbroker:vam.schedule\tassistant / daily\thello\tinvoke\t99\t-\t-")
        );
        assert_eq!(grants_table(r#"{"keys":[]}"#), "no grants");
    }
}
