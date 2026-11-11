//! Test only (cargo feature `fake-transport`). The control socket of the fake-transport broker:
//! `vam-notify-broker fake click|dm|act-raw|handles` are clients of it. A person pressing a
//! button or writing to the bot is a line of JSON here; it goes through the same code a live
//! Slack event does ([`Hub::on_event`]).
//!
//! The socket is `<state-dir>/fake.sock`, mode 0600, in the 0700 state directory.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use vam_spec::v1::{ActOnNotificationRequest, ActOnOutcome, ExternalAccount};

use crate::adapter::InboundEvent;
use crate::gateway::GatewayClient;
use crate::hub::{Hub, Overrides, Secret};
use crate::slack::split_account;

pub const SOCKET_FILE: &str = "fake.sock";
const CHANNEL: &str = "slack";
const MAX_LINE: u64 = 1 << 20;

/// A handle vamd never issued.
const FORGED: &str = "vam_na_forged00000.forgedforgedforgedforgedforgedforged";

/// What the control path remembers between calls (memory only).
#[derive(Default)]
pub struct FakeState {
    /// The handle of the last click, for `--handle replay`.
    last_click: Mutex<Option<Secret>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    /// A user presses a button of the newest `message` (a kind, or `latest`) held for `account`.
    Click {
        account: String,
        message: String,
        action: String,
        /// `forged`, `replay`, `from:<account>`; omitted: the message's own.
        handle: Option<String>,
        gateway_socket: Option<PathBuf>,
    },
    /// A user writes to the bot.
    Dm { account: String, text: String },
    /// `ActOnNotification` with whatever handle and decision are given: no message needed.
    ActRaw {
        account: String,
        handle: String,
        decision: String,
        gateway_socket: Option<PathBuf>,
    },
    /// The delivery ids the broker holds (never the handles).
    Handles,
}

fn fail(msg: impl Into<String>) -> Value {
    json!({ "ok": false, "error": msg.into() })
}

fn answered(status: &str, detail: &str) -> Value {
    json!({ "ok": true, "status": status, "detail": detail })
}

/// Binds the control socket (before the broker reports ready, so a client never races it).
pub fn bind(dir: &Path) -> std::io::Result<tokio::net::UnixListener> {
    crate::serve::bind_unix_listener(&dir.join(SOCKET_FILE))
}

pub async fn serve(hub: Arc<Hub>, listener: tokio::net::UnixListener) -> std::io::Result<()> {
    loop {
        let (stream, _) = listener.accept().await?;
        let hub = hub.clone();
        tokio::spawn(async move {
            if let Err(e) = handle(hub, stream).await {
                tracing::warn!(error = %e, "control connection");
            }
        });
    }
}

async fn handle(hub: Arc<Hub>, stream: UnixStream) -> std::io::Result<()> {
    let (r, mut w) = stream.into_split();
    let mut line = String::new();
    BufReader::new(r.take(MAX_LINE))
        .read_line(&mut line)
        .await?;
    let reply = match serde_json::from_str::<Request>(line.trim()) {
        Ok(req) => run(&hub, req).await,
        Err(e) => fail(format!("bad control request: {e}")),
    };
    w.write_all(format!("{reply}\n").as_bytes()).await?;
    w.shutdown().await
}

async fn run(hub: &Arc<Hub>, req: Request) -> Value {
    match req {
        Request::Click {
            account,
            message,
            action,
            handle,
            gateway_socket,
        } => click(hub, account, message, action, handle, gateway_socket).await,
        Request::Dm { account, text } => {
            let (scope, user) = split_account(&account);
            let ev = InboundEvent::DirectMessage {
                scope: scope.into(),
                user: user.into(),
                text,
            };
            let a = hub.on_event(CHANNEL, ev, Overrides::default()).await;
            if a.reached {
                answered(&a.status, &a.detail)
            } else {
                fail(format!("link could not reach vamd: {}", a.detail))
            }
        }
        Request::ActRaw {
            account,
            handle,
            decision,
            gateway_socket,
        } => {
            let (scope, user) = split_account(&account);
            let gateway = gateway_socket.map(|p| GatewayClient::new(Some(p)));
            let Some(gateway) = gateway.or_else(|| hub.gateway_client()) else {
                return fail("no gateway socket");
            };
            let r = gateway
                .act(ActOnNotificationRequest {
                    handle,
                    external_account: Some(ExternalAccount {
                        scope: scope.into(),
                        id: user.into(),
                    }),
                    decision,
                    response: None,
                })
                .await;
            match r {
                Ok(r) => answered(
                    act_name(
                        ActOnOutcome::try_from(r.outcome).unwrap_or(ActOnOutcome::Unspecified),
                    ),
                    &r.message,
                ),
                Err(e) => fail(e.to_string()),
            }
        }
        Request::Handles => {
            let list: Vec<Value> = hub
                .listing()
                .into_iter()
                .map(|(id, h)| {
                    json!({
                        "delivery_id": id,
                        "kind": h.kind,
                        "account": h.address,
                        "actions": h.offered.iter().map(|o| o.id.clone()).collect::<Vec<_>>(),
                        "handle_held": h.handle.is_some(),
                    })
                })
                .collect();
            json!({ "ok": true, "handles": list })
        }
    }
}

fn act_name(o: ActOnOutcome) -> &'static str {
    match o {
        ActOnOutcome::Accepted => "OK",
        ActOnOutcome::NotPermitted => "NOT_PERMITTED",
        ActOnOutcome::Invalid => "INVALID",
        ActOnOutcome::Expired => "EXPIRED",
        ActOnOutcome::AlreadyResolved => "ALREADY_RESOLVED",
        ActOnOutcome::NotLinked => "NOT_LINKED",
        ActOnOutcome::RateLimited => "RATE_LIMITED",
        ActOnOutcome::Unavailable | ActOnOutcome::Unspecified => "UNAVAILABLE",
    }
}

async fn click(
    hub: &Arc<Hub>,
    account: String,
    message: String,
    action: String,
    handle: Option<String>,
    gateway_socket: Option<PathBuf>,
) -> Value {
    let Some((delivery_id, held)) = hub.pick(&message, &account, false) else {
        return fail(format!(
            "no message of kind '{message}' was delivered to {account}"
        ));
    };
    let own = held.handle.clone();
    let chosen: Secret = match handle.as_deref() {
        None => own.unwrap_or_else(|| Secret::new("")),
        Some("forged") => Secret::new(FORGED),
        Some("replay") => match hub.fake.last_click.lock().ok().and_then(|l| l.clone()) {
            Some(h) => h,
            None => return fail("replay: there was no earlier click"),
        },
        Some(other) => {
            let Some(from) = other.strip_prefix("from:") else {
                return fail(format!(
                    "unknown --handle '{other}' (forged, replay, from:<account>)"
                ));
            };
            match hub.pick(&message, from, true) {
                Some((_, h)) => h.handle.unwrap_or_else(|| Secret::new("")),
                None => {
                    return fail(format!(
                        "no message of kind '{message}' was delivered to {from}"
                    ))
                }
            }
        }
    };
    if let Ok(mut l) = hub.fake.last_click.lock() {
        *l = Some(chosen.clone());
    }
    // The runner names a button as the person sees it: `approve`, or a gate option's label.
    // Resolve that to the action id the button carries; the live path matches ids only.
    let action = held
        .offered
        .iter()
        .find(|o| o.id == action)
        .or_else(|| held.offered.iter().find(|o| o.decision == action))
        .map(|o| o.id.clone())
        .unwrap_or(action);
    let (scope, user) = split_account(&account);
    let ev = InboundEvent::Click {
        delivery_id,
        scope: scope.into(),
        user: user.into(),
        decision: action,
        response: None,
    };
    let ov = Overrides {
        handle: Some(chosen),
        gateway: gateway_socket.map(|p| GatewayClient::new(Some(p))),
    };
    let a = hub.on_event(CHANNEL, ev, ov).await;
    if a.reached {
        answered(&a.status, &a.detail)
    } else {
        fail(format!(
            "the click did not reach ActOnNotification: {}",
            a.detail
        ))
    }
}

/// One request to a running broker's control socket.
pub async fn call(state_dir: &Path, req: &Request) -> Result<Value, String> {
    let path = state_dir.join(SOCKET_FILE);
    let mut stream = UnixStream::connect(&path).await.map_err(|e| {
        format!(
            "{}: {e} (is the broker running with --fake-transport?)",
            path.display()
        )
    })?;
    let line = serde_json::to_string(req).map_err(|e| e.to_string())?;
    stream
        .write_all(format!("{line}\n").as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    let mut reply = String::new();
    BufReader::new(stream)
        .read_line(&mut reply)
        .await
        .map_err(|e| e.to_string())?;
    let v: Value = serde_json::from_str(reply.trim()).map_err(|e| format!("bad reply: {e}"))?;
    if v["ok"].as_bool() == Some(true) {
        Ok(v)
    } else {
        Err(v["error"]
            .as_str()
            .unwrap_or("the broker refused")
            .to_string())
    }
}

/// Runs one `fake` subcommand and prints what the runner reads.
pub async fn run_cli(state_dir: &Path, req: Request) -> Result<(), String> {
    let is_dm = matches!(req, Request::Dm { .. });
    let v = call(state_dir, &req).await?;
    if let Some(list) = v.get("handles") {
        println!("{list}");
        return Ok(());
    }
    let status = v["status"].as_str().unwrap_or_default();
    if is_dm && !matches!(status, "LINKED" | "IGNORED") {
        return Err(format!(
            "the DM was answered {status}: {}",
            v["detail"].as_str().unwrap_or("")
        ));
    }
    println!("{}", json!({ "status": status, "detail": v["detail"] }));
    Ok(())
}
