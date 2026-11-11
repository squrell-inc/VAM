use anyhow::Result;
use clap::Subcommand;
use vam_spec::v1::{
    CreateSessionRequest, CreateSnapshotRequest, PauseSessionRequest, ResourceId,
    RestoreSessionRequest, ResumeSessionRequest, TerminateSessionRequest,
};

use crate::client::{self, resolve_idempotency_key, Client};
use crate::config;

#[derive(Debug, Subcommand)]
pub enum SessionCmd {
    Create {
        #[arg(long)]
        workspace: Option<String>,
        /// Reuse this idempotency key (default: mint a fresh key).
        #[arg(long, alias = "idempotency")]
        idempotency_key: Option<String>,
    },
    /// List sessions, newest first: one page of 50 (a hint names the next); `--all` follows every page.
    Ls {
        #[arg(long)]
        all: bool,
    },
    Get {
        id: String,
    },
    Pause {
        id: String,
        /// Reuse this idempotency key (default: mint a fresh key).
        #[arg(long)]
        idempotency_key: Option<String>,
    },
    Resume {
        id: String,
        /// Reuse this idempotency key (default: mint a fresh key).
        #[arg(long)]
        idempotency_key: Option<String>,
    },
    Terminate {
        id: String,
        /// Reuse this idempotency key (default: mint a fresh key).
        #[arg(long)]
        idempotency_key: Option<String>,
    },
    Snapshot {
        id: String,
        #[arg(long)]
        snapshot_id: Option<String>,
        /// Reuse this idempotency key (default: mint a fresh key).
        #[arg(long)]
        idempotency_key: Option<String>,
    },
    Restore {
        snapshot_id: String,
        #[arg(long)]
        session: String,
        /// Reuse this idempotency key (default: mint a fresh key).
        #[arg(long)]
        idempotency_key: Option<String>,
    },
}

pub async fn run(host: Option<String>, token: Option<String>, cmd: SessionCmd) -> Result<()> {
    let cfg = config::resolve(host, token).await?;
    let c = Client::new(&cfg)?;
    match cmd {
        SessionCmd::Create {
            workspace,
            idempotency_key,
        } => {
            // session_id and vm_id are server-minted; omit both from the request.
            let key = resolve_idempotency_key(idempotency_key);
            let req = CreateSessionRequest {
                session_id: None,
                vm_id: String::new(),
                idempotency_key: key.clone(),
                workspace_id: workspace.unwrap_or_else(|| "assistant".into()),
                ..Default::default()
            };
            let body = c
                .post_json(
                    "/v1/sessions",
                    "vam.v1.CreateSessionRequest",
                    &req,
                    Some(&key),
                )
                .await?;
            client::print_json(&body);
        }
        SessionCmd::Ls { all } => {
            let body = client::list_paged(&c, "/v1/sessions", "sessions", all).await?;
            client::print_json(&body);
        }
        SessionCmd::Get { id } => {
            let body = c
                .get(&format!(
                    "/v1/sessions/{}",
                    client::encode_path_segment(&id)
                ))
                .await?;
            client::print_json(&body);
        }
        SessionCmd::Pause {
            id,
            idempotency_key,
        } => {
            let req = PauseSessionRequest {
                session_id: Some(ResourceId { urn: id.clone() }),
                ..Default::default()
            };
            let json = vam_spec::encoding::proto_to_json("vam.v1.PauseSessionRequest", &req)
                .map_err(|e| anyhow::anyhow!(e))?;
            let key = resolve_idempotency_key(idempotency_key);
            let body = c
                .post_raw_json(
                    &format!("/v1/sessions/{}/pause", client::encode_path_segment(&id)),
                    &json,
                    &key,
                )
                .await?;
            client::print_json(&body);
        }
        SessionCmd::Resume {
            id,
            idempotency_key,
        } => {
            let req = ResumeSessionRequest {
                session_id: Some(ResourceId { urn: id.clone() }),
                ..Default::default()
            };
            let json = vam_spec::encoding::proto_to_json("vam.v1.ResumeSessionRequest", &req)
                .map_err(|e| anyhow::anyhow!(e))?;
            let key = resolve_idempotency_key(idempotency_key);
            let body = c
                .post_raw_json(
                    &format!("/v1/sessions/{}/resume", client::encode_path_segment(&id)),
                    &json,
                    &key,
                )
                .await?;
            client::print_json(&body);
        }
        SessionCmd::Terminate {
            id,
            idempotency_key,
        } => {
            let req = TerminateSessionRequest {
                session_id: Some(ResourceId { urn: id.clone() }),
                ..Default::default()
            };
            let json = vam_spec::encoding::proto_to_json("vam.v1.TerminateSessionRequest", &req)
                .map_err(|e| anyhow::anyhow!(e))?;
            let body = c
                .post_raw_json(
                    &format!(
                        "/v1/sessions/{}/terminate",
                        client::encode_path_segment(&id)
                    ),
                    &json,
                    &resolve_idempotency_key(idempotency_key),
                )
                .await?;
            client::print_json(&body);
        }
        SessionCmd::Snapshot {
            id,
            snapshot_id,
            idempotency_key,
        } => {
            let req = CreateSnapshotRequest {
                session_id: Some(ResourceId { urn: id.clone() }),
                snapshot_id: snapshot_id.map(|urn| ResourceId { urn }),
                ..Default::default()
            };
            let json = vam_spec::encoding::proto_to_json("vam.v1.CreateSnapshotRequest", &req)
                .map_err(|e| anyhow::anyhow!(e))?;
            let body = c
                .post_raw_json(
                    &format!(
                        "/v1/sessions/{}/snapshots",
                        client::encode_path_segment(&id)
                    ),
                    &json,
                    &resolve_idempotency_key(idempotency_key),
                )
                .await?;
            client::print_json(&body);
        }
        SessionCmd::Restore {
            snapshot_id,
            session,
            idempotency_key,
        } => {
            let req = RestoreSessionRequest {
                snapshot_id: Some(ResourceId { urn: snapshot_id }),
                new_session_id: Some(ResourceId { urn: session }),
                ..Default::default()
            };
            let body = c
                .post_json(
                    "/v1/sessions/restore",
                    "vam.v1.RestoreSessionRequest",
                    &req,
                    idempotency_key.as_deref(),
                )
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
    fn mutating_session_commands_accept_idempotency_key() {
        #[derive(Parser)]
        struct Wrap {
            #[command(subcommand)]
            cmd: SessionCmd,
        }
        let created = Wrap::try_parse_from(["vam", "create", "--idempotency-key", "sess"]).unwrap();
        assert!(matches!(
            created.cmd,
            SessionCmd::Create { ref idempotency_key, .. } if idempotency_key.as_deref() == Some("sess")
        ));
        let alias = Wrap::try_parse_from(["vam", "create", "--idempotency", "legacy"]).unwrap();
        assert!(matches!(
            alias.cmd,
            SessionCmd::Create { ref idempotency_key, .. } if idempotency_key.as_deref() == Some("legacy")
        ));
        for (verb, id) in [("pause", "s1"), ("resume", "s1"), ("terminate", "s1")] {
            let parsed = Wrap::try_parse_from(["vam", verb, id, "--idempotency-key", "k"]).unwrap();
            let got = match parsed.cmd {
                SessionCmd::Pause {
                    idempotency_key, ..
                }
                | SessionCmd::Resume {
                    idempotency_key, ..
                }
                | SessionCmd::Terminate {
                    idempotency_key, ..
                } => idempotency_key,
                other => panic!("expected {verb}, got {other:?}"),
            };
            assert_eq!(got.as_deref(), Some("k"));
        }
        let snap =
            Wrap::try_parse_from(["vam", "snapshot", "s1", "--idempotency-key", "snap"]).unwrap();
        assert!(matches!(
            snap.cmd,
            SessionCmd::Snapshot { ref idempotency_key, .. } if idempotency_key.as_deref() == Some("snap")
        ));
        let restore = Wrap::try_parse_from([
            "vam",
            "restore",
            "snap-1",
            "--session",
            "s2",
            "--idempotency-key",
            "rest",
        ])
        .unwrap();
        assert!(matches!(
            restore.cmd,
            SessionCmd::Restore { ref idempotency_key, .. } if idempotency_key.as_deref() == Some("rest")
        ));
    }
}
