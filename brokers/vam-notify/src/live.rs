//! The live Slack client: the Web API over HTTPS and Socket Mode over WSS.
//!
//! Tokens:
//! - The bot token arrives with each [`SlackTransport::call`], goes into one `Authorization`
//!   header and is dropped with the call.
//! - The app-level token arrives with [`SlackTransport::open_socket`], is used for the one
//!   `apps.connections.open` call and is dropped. The WSS URL it returns carries a one-time ticket,
//!   so the URL is never logged or put in an error.
//! - `LiveTransport` holds no token, `Debug` shows only the endpoint, and every error text is
//!   scrubbed of the token it was made with before it leaves this file.
//!
//! Verification: in Socket Mode, Slack authenticates the connection itself (the URL is single use
//! and issued to the app token's holder over TLS), so there is no request signing to check, unlike
//! the HTTP Events API. What the broker does check is which workspace an event is from (the hub
//! drops events from a team other than the pinned one) and that a button belongs to a message this
//! broker sent (the hub's held-message table).
//!
//! Endpoints: [`LiveTransport::new`] is the only constructor in a default build and points at
//! `https://slack.com/api`. A different base URL (and a plain `ws://` socket URL) can be set only
//! by [`LiveTransport::for_test`], which exists in this crate's tests and behind the
//! `fake-transport` feature. No flag or environment variable reaches either.

use std::fmt;
use std::time::Duration;

use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

use crate::adapter::{InboundEvent, InboundStream};
use crate::slack::render::response_from_state;
use crate::transport::{SlackTransport, TransportError};

/// The Web API root of the real service.
const SLACK_API: &str = "https://slack.com/api";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
/// Slack wants an envelope acknowledged within 3 s.
const ACK_DEADLINE: Duration = Duration::from_secs(3);
/// Slack pings about every 30 s; a socket silent for this long is dead.
const IDLE_LIMIT: Duration = Duration::from_secs(120);
/// The wait when a 429 carries no usable `Retry-After`.
const DEFAULT_RETRY_AFTER_MS: u32 = 30_000;
const MAX_RETRY_AFTER_MS: u32 = 3_600_000;
const MAX_BODY: usize = 4 * 1024 * 1024;
const EVENT_BUFFER: usize = 64;

#[derive(Clone)]
pub struct LiveTransport {
    api_base: String,
    http: reqwest::Client,
    allow_insecure_ws: bool,
}

impl fmt::Debug for LiveTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LiveTransport")
            .field("api_base", &self.api_base)
            .finish()
    }
}

impl Default for LiveTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl LiveTransport {
    /// The real service: `https://slack.com/api`, `wss://` sockets only.
    pub fn new() -> Self {
        Self::build(SLACK_API.to_string(), false)
    }

    /// Tests only: Web API at `api_base` (a local fake server) and `ws://` sockets allowed.
    #[cfg(any(test, feature = "fake-transport"))]
    pub fn for_test(api_base: &str) -> Self {
        Self::build(api_base.trim_end_matches('/').to_string(), true)
    }

    fn build(api_base: String, allow_insecure_ws: bool) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            // Never follow a redirect: a 3xx could send the bearer token to another host.
            .redirect(reqwest::redirect::Policy::none())
            .user_agent("vam-notify")
            .build()
            .expect("the HTTP client builds with rustls");
        Self {
            api_base,
            http,
            allow_insecure_ws,
        }
    }
}

/// Removes `token` from `text`, so no error can carry it.
fn scrub(text: &str, token: &str) -> String {
    if token.is_empty() {
        text.to_string()
    } else {
        text.replace(token, "<redacted>")
    }
}

fn valid_method(method: &str) -> bool {
    !method.is_empty()
        && method.len() <= 64
        && method
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_')
}

/// `Retry-After` seconds as milliseconds, clamped.
fn retry_after_ms(header: Option<&str>) -> u32 {
    header
        .and_then(|v| v.trim().parse::<u32>().ok())
        .map(|s| s.saturating_mul(1000).clamp(1000, MAX_RETRY_AFTER_MS))
        .unwrap_or(DEFAULT_RETRY_AFTER_MS)
}

/// Accepts only the URL `apps.connections.open` returns, and only as `wss://` (a test may allow
/// `ws://`).
fn check_ws_url(raw: &str, allow_insecure: bool) -> Result<url::Url, &'static str> {
    let u = url::Url::parse(raw).map_err(|_| "the socket URL does not parse")?;
    if u.host_str().is_none_or(str::is_empty) {
        return Err("the socket URL has no host");
    }
    match u.scheme() {
        "wss" => Ok(u),
        "ws" if allow_insecure => Ok(u),
        _ => Err("the socket URL is not wss://"),
    }
}

#[async_trait]
impl SlackTransport for LiveTransport {
    async fn call(&self, method: &str, body: Value, token: &str) -> Result<Value, TransportError> {
        if !valid_method(method) {
            return Err(TransportError::Api("invalid_method".into()));
        }
        let url = format!("{}/{method}", self.api_base);
        let resp = self
            .http
            .post(url)
            .bearer_auth(token)
            .header("Content-Type", "application/json; charset=utf-8")
            .body(body.to_string())
            .send()
            .await
            .map_err(|e| {
                let kind = if e.is_timeout() {
                    "timed out"
                } else if e.is_connect() {
                    "could not connect"
                } else {
                    "request failed"
                };
                TransportError::Unavailable(format!("slack {method}: {kind}"))
            })?;
        let status = resp.status();
        let retry_header = resp
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        if status.as_u16() == 429 {
            return Err(TransportError::RateLimited {
                retry_after_ms: retry_after_ms(retry_header.as_deref()),
            });
        }
        if status.is_server_error() {
            return Err(TransportError::Server(status.as_u16()));
        }
        if !status.is_success() {
            return Err(TransportError::Unavailable(format!(
                "slack {method}: HTTP {}",
                status.as_u16()
            )));
        }
        let bytes = resp.bytes().await.map_err(|_| {
            TransportError::Unavailable(format!("slack {method}: the reply was cut off"))
        })?;
        if bytes.len() > MAX_BODY {
            return Err(TransportError::Unavailable(format!(
                "slack {method}: the reply is too large"
            )));
        }
        let value: Value = serde_json::from_slice(&bytes).map_err(|_| {
            TransportError::Unavailable(format!("slack {method}: the reply is not JSON"))
        })?;
        if value.get("ok").and_then(Value::as_bool) == Some(true) {
            return Ok(value);
        }
        let code = value
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("unknown_error");
        let code: String = scrub(code, token).chars().take(64).collect();
        if code == "ratelimited" {
            return Err(TransportError::RateLimited {
                retry_after_ms: retry_after_ms(retry_header.as_deref()),
            });
        }
        Err(TransportError::Api(code))
    }

    async fn open_socket(&self, app_token: &str) -> Result<InboundStream, TransportError> {
        let opened = self
            .call("apps.connections.open", json!({}), app_token)
            .await?;
        let raw = opened
            .get("url")
            .and_then(Value::as_str)
            .ok_or_else(|| TransportError::Api("bad_response".into()))?;
        let url = check_ws_url(raw, self.allow_insecure_ws)
            .map_err(|m| TransportError::Unavailable(m.into()))?;
        // The URL holds a ticket: no error below names it.
        let (ws, _) = tokio::time::timeout(
            CONNECT_TIMEOUT,
            tokio_tungstenite::connect_async(url.as_str()),
        )
        .await
        .map_err(|_| TransportError::Unavailable("socket connect timed out".into()))?
        .map_err(|_| TransportError::Unavailable("socket connect failed".into()))?;
        let (tx, rx) = mpsc::channel(EVENT_BUFFER);
        tokio::spawn(socket_loop(ws, tx));
        Ok(rx)
    }
}

/// Reads envelopes until the socket ends. Ends (and so closes the event stream, which makes the
/// hub reconnect with a fresh token fetch) on `disconnect`, close, error or silence.
async fn socket_loop<S>(mut ws: WebSocketStream<S>, tx: mpsc::Sender<InboundEvent>)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    loop {
        let msg = match tokio::time::timeout(IDLE_LIMIT, ws.next()).await {
            Err(_) => {
                tracing::warn!("slack socket silent; closing");
                break;
            }
            Ok(None) => break,
            Ok(Some(Err(_))) => {
                tracing::warn!("slack socket error; closing");
                break;
            }
            Ok(Some(Ok(m))) => m,
        };
        match msg {
            Message::Text(text) => {
                let Ok(env) = serde_json::from_str::<Value>(&text) else {
                    continue;
                };
                match env.get("type").and_then(Value::as_str).unwrap_or("") {
                    "hello" => tracing::debug!("slack socket hello"),
                    "disconnect" => {
                        tracing::info!(reason = reason_of(&env), "slack asked for a reconnect");
                        break;
                    }
                    _ => {
                        // Ack first, before anything slow: Slack resends after 3 s.
                        if let Some(id) = env.get("envelope_id").and_then(Value::as_str) {
                            let ack = json!({ "envelope_id": id }).to_string();
                            let sent =
                                tokio::time::timeout(ACK_DEADLINE, ws.send(Message::Text(ack)))
                                    .await;
                            if !matches!(sent, Ok(Ok(()))) {
                                tracing::warn!("slack envelope ack failed; closing");
                                break;
                            }
                        }
                        if let Some(ev) = to_event(&env) {
                            if tx.send(ev).await.is_err() {
                                break;
                            }
                        }
                    }
                }
            }
            Message::Ping(p) => {
                if ws.send(Message::Pong(p)).await.is_err() {
                    break;
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }
    let _ = tokio::time::timeout(Duration::from_secs(1), ws.close(None)).await;
}

fn reason_of(env: &Value) -> &str {
    env.get("reason").and_then(Value::as_str).unwrap_or("")
}

fn s<'a>(v: &'a Value, ptr: &str) -> &'a str {
    v.pointer(ptr).and_then(Value::as_str).unwrap_or("")
}

/// One Socket Mode envelope as an inbound event: a `message.im` DM, or a `block_actions` press of
/// one of this broker's buttons. Anything else is dropped.
fn to_event(env: &Value) -> Option<InboundEvent> {
    let payload = env.get("payload")?;
    match env.get("type").and_then(Value::as_str)? {
        "events_api" => {
            let ev = payload.get("event")?;
            let is_dm = s(ev, "/type") == "message" && s(ev, "/channel_type") == "im";
            // An edit, a deletion or a bot's own message (our replies) is not a person writing.
            if !is_dm || ev.get("subtype").is_some() || ev.get("bot_id").is_some() {
                return None;
            }
            let team = match s(payload, "/team_id") {
                "" => s(ev, "/team"),
                t => t,
            };
            Some(InboundEvent::DirectMessage {
                scope: team.to_string(),
                user: s(ev, "/user").to_string(),
                text: s(ev, "/text").to_string(),
            })
        }
        "interactive" => {
            if s(payload, "/type") != "block_actions" {
                return None;
            }
            let action = payload.get("actions")?.as_array()?.first()?;
            if !s(action, "/action_id").starts_with("vam_act:") {
                return None;
            }
            // The button's value is `<delivery_id>|<action_id>` (never a handle).
            let (delivery_id, decision) = s(action, "/value").split_once('|')?;
            let response = payload
                .pointer("/state/values")
                .map(response_from_state)
                .filter(|r| r.as_object().is_some_and(|o| !o.is_empty()));
            Some(InboundEvent::Click {
                delivery_id: delivery_id.to_string(),
                scope: s(payload, "/team/id").to_string(),
                user: s(payload, "/user/id").to_string(),
                decision: decision.to_string(),
                response,
            })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_wss_urls_are_accepted_by_default() {
        assert!(check_ws_url("wss://wss-primary.slack.com/link/?ticket=x", false).is_ok());
        assert!(check_ws_url("ws://127.0.0.1:9/x", false).is_err());
        assert!(check_ws_url("https://slack.com/x", false).is_err());
        assert!(check_ws_url("wss://", false).is_err());
        assert!(check_ws_url("not a url", false).is_err());
        assert!(check_ws_url("ws://127.0.0.1:9/x", true).is_ok());
    }

    #[test]
    fn the_default_transport_points_at_slack_and_refuses_plain_ws() {
        let t = LiveTransport::new();
        assert_eq!(t.api_base, "https://slack.com/api");
        assert!(!t.allow_insecure_ws);
    }

    #[test]
    fn retry_after_is_clamped_and_defaults() {
        assert_eq!(retry_after_ms(Some("3")), 3000);
        assert_eq!(retry_after_ms(Some("0")), 1000);
        assert_eq!(retry_after_ms(Some("999999")), MAX_RETRY_AFTER_MS);
        assert_eq!(retry_after_ms(Some("soon")), DEFAULT_RETRY_AFTER_MS);
        assert_eq!(retry_after_ms(None), DEFAULT_RETRY_AFTER_MS);
    }

    #[test]
    fn scrub_removes_the_token() {
        assert_eq!(scrub("bad xoxb-1 here", "xoxb-1"), "bad <redacted> here");
        assert_eq!(scrub("same", ""), "same");
    }

    #[test]
    fn a_dm_becomes_a_direct_message_and_a_bots_own_does_not() {
        let dm = json!({ "type": "events_api", "payload": { "team_id": "T1",
            "event": { "type": "message", "channel_type": "im", "user": "U1", "text": "ABCDE-FGHJK" } } });
        assert_eq!(
            to_event(&dm),
            Some(InboundEvent::DirectMessage {
                scope: "T1".into(),
                user: "U1".into(),
                text: "ABCDE-FGHJK".into()
            })
        );
        let own = json!({ "type": "events_api", "payload": { "team_id": "T1",
            "event": { "type": "message", "channel_type": "im", "user": "UB", "bot_id": "B1", "text": "hi" } } });
        assert_eq!(to_event(&own), None);
        let channel = json!({ "type": "events_api", "payload": { "team_id": "T1",
            "event": { "type": "message", "channel_type": "channel", "user": "U1", "text": "hi" } } });
        assert_eq!(to_event(&channel), None);
    }

    #[test]
    fn a_button_press_becomes_a_click() {
        let env = json!({ "type": "interactive", "payload": { "type": "block_actions",
            "team": { "id": "T1" }, "user": { "id": "U1" },
            "actions": [{ "action_id": "vam_act:approve", "value": "nd_1|approve" }] } });
        assert_eq!(
            to_event(&env),
            Some(InboundEvent::Click {
                delivery_id: "nd_1".into(),
                scope: "T1".into(),
                user: "U1".into(),
                decision: "approve".into(),
                response: None
            })
        );
        let foreign = json!({ "type": "interactive", "payload": { "type": "block_actions",
            "team": { "id": "T1" }, "user": { "id": "U1" },
            "actions": [{ "action_id": "other", "value": "a|b" }] } });
        assert_eq!(to_event(&foreign), None);
    }
}
