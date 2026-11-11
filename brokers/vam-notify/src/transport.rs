//! The transport under an adapter: how a Slack Web API call reaches Slack.
//!
//! `LiveTransport` (in `live.rs`) talks to Slack. The fake (cargo feature `fake-transport`, never in a
//! normal build) records each call for the scenarios.

use async_trait::async_trait;

use crate::adapter::InboundStream;

#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    /// The network or the process under the transport is not there. Worth a retry.
    #[error("transport unavailable: {0}")]
    Unavailable(String),
    /// Slack answered `ok: false` with this error code (`user_not_found`, `invalid_auth`, ...).
    #[error("slack error: {0}")]
    Api(String),
    /// HTTP 429.
    #[error("slack rate limit; retry after {retry_after_ms} ms")]
    RateLimited { retry_after_ms: u32 },
    /// HTTP 5xx.
    #[error("slack server error {0}")]
    Server(u16),
}

/// One Slack Web API call. The token comes with the call and is never stored.
#[async_trait]
pub trait SlackTransport: Send + Sync {
    async fn call(
        &self,
        method: &str,
        body: serde_json::Value,
        token: &str,
    ) -> Result<serde_json::Value, TransportError>;

    /// Opens the Socket Mode connection. `app_token` is good for this call only: the transport
    /// exchanges it for the connection and keeps nothing of it.
    async fn open_socket(&self, app_token: &str) -> Result<InboundStream, TransportError>;
}

pub use crate::live::LiveTransport;

#[cfg(feature = "fake-transport")]
pub mod fake {
    //! Test only. Writes each call (method and body, token removed, with a time) to
    //! `<dir>/sent.jsonl`. Clicks and DMs come in through `control`, not through here.

    use std::io::Write;
    use std::path::PathBuf;
    use std::sync::Mutex;

    use super::*;

    /// A string only a fake-transport binary contains; the build check looks for it.
    pub const MARKER: &str = "VAM_NOTIFY_FAKE_TRANSPORT";
    pub const SENT_FILE: &str = "sent.jsonl";

    pub struct FakeTransport {
        path: PathBuf,
        counter: Mutex<u64>,
        /// Keeps the event streams handed out open: the fake never closes a connection.
        senders: Mutex<Vec<tokio::sync::mpsc::Sender<crate::adapter::InboundEvent>>>,
    }

    impl FakeTransport {
        pub fn new(dir: &std::path::Path) -> Self {
            let path = dir.join(SENT_FILE);
            let seen = std::fs::read_to_string(&path)
                .map(|s| s.lines().count() as u64)
                .unwrap_or(0);
            Self {
                path,
                counter: Mutex::new(seen),
                senders: Mutex::new(Vec::new()),
            }
        }

        fn next(&self) -> Result<u64, TransportError> {
            let mut c = self
                .counter
                .lock()
                .map_err(|_| TransportError::Api("lock".into()))?;
            *c += 1;
            Ok(*c)
        }
    }

    fn now_ms() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    }

    #[async_trait]
    impl SlackTransport for FakeTransport {
        async fn call(
            &self,
            method: &str,
            body: serde_json::Value,
            _token: &str,
        ) -> Result<serde_json::Value, TransportError> {
            let n = self.next()?;
            let mut recorded = body.clone();
            if let Some(o) = recorded.as_object_mut() {
                o.remove("token");
            }
            let line =
                serde_json::json!({ "at_ms": now_ms() as u64, "method": method, "body": recorded });
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)
                .map_err(|e| TransportError::Unavailable(e.to_string()))?;
            writeln!(f, "{line}").map_err(|e| TransportError::Unavailable(e.to_string()))?;
            let secs = now_ms() / 1000;
            let ts = format!("{secs}.{n:06}");
            Ok(match method {
                // The fake DM channel is the user's id, which is what the scenarios read.
                "conversations.open" => {
                    let user = body["users"].as_str().unwrap_or("UFAKE");
                    serde_json::json!({ "ok": true, "channel": { "id": user } })
                }
                "chat.update" => serde_json::json!({
                    "ok": true, "channel": body["channel"], "ts": body["ts"]
                }),
                _ => serde_json::json!({ "ok": true, "channel": body["channel"], "ts": ts }),
            })
        }

        async fn open_socket(&self, _app_token: &str) -> Result<InboundStream, TransportError> {
            let (tx, rx) = tokio::sync::mpsc::channel(16);
            self.senders
                .lock()
                .map_err(|_| TransportError::Api("lock".into()))?
                .push(tx);
            Ok(rx)
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[tokio::test]
        async fn records_the_call_without_the_token() {
            let tmp = tempfile::tempdir().unwrap();
            let t = FakeTransport::new(tmp.path());
            let out = t
                .call(
                    "chat.postMessage",
                    serde_json::json!({"text": "hi", "token": "xoxb-secret", "channel": "U1"}),
                    "xoxb-secret",
                )
                .await
                .unwrap();
            assert_eq!(out["ok"], true);
            let sent = std::fs::read_to_string(tmp.path().join(SENT_FILE)).unwrap();
            assert!(sent.contains("chat.postMessage") && !sent.contains("xoxb-secret"));
            assert!(sent.contains("at_ms"));
        }

        #[tokio::test]
        async fn a_dm_channel_is_the_user_id() {
            let tmp = tempfile::tempdir().unwrap();
            let t = FakeTransport::new(tmp.path());
            let out = t
                .call(
                    "conversations.open",
                    serde_json::json!({"users": "U7"}),
                    "t",
                )
                .await
                .unwrap();
            assert_eq!(out["channel"]["id"], "U7");
        }
    }
}
