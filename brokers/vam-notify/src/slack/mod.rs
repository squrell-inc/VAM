//! The Slack adapter: DMs only (`conversations.open` then `chat.postMessage`; `chat.update` for an
//! edit). Rendering is in [`render`]. The bot token comes with each call and goes straight to the
//! transport; nothing here keeps it.

pub mod render;

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};
use vam_spec::v1::{DeliverOutcome, NotificationRendering};

use crate::adapter::{
    Binding, Capabilities, ChannelAdapter, Credentials, DeliveryResult, ExternalRef, InboundEvent,
    InboundStream, Relay,
};
use crate::log::plain;
use crate::transport::{SlackTransport, TransportError};

pub const CHANNEL: &str = "slack";
/// The key of the bot token in `resolved_secrets` (`vamd` sends it on each delivery).
pub const BOT_TOKEN_KEY: &str = "bot_token";
/// Slack's static select takes up to 100 options.
pub const MAX_SELECT_OPTIONS: usize = 100;

/// Slack's primitive set (design 9.3, catalog names). Not `NumberInput`, `FileInput`,
/// `ProgressBar`.
pub fn capabilities() -> Capabilities {
    Capabilities {
        native_primitives: [
            "Stack",
            "Text",
            "Badge",
            "KeyValue",
            "Button",
            "Input",
            "TextArea",
            "Select",
            "MultiSelect",
            "Toggle",
            "DateInput",
        ]
        .map(String::from)
        .to_vec(),
        max_select_options: MAX_SELECT_OPTIONS,
        takes_decisions: true,
        needs_verified_contact: true,
    }
}

pub struct SlackAdapter {
    transport: Arc<dyn SlackTransport>,
}

impl SlackAdapter {
    pub fn new(transport: Arc<dyn SlackTransport>) -> Self {
        Self { transport }
    }
}

/// The Slack user id of a `team/user` address (or a bare user id).
pub fn user_of(address: &str) -> &str {
    address.rsplit('/').next().unwrap_or(address)
}

/// Splits `team/user`; a bare user id has an empty team.
pub fn split_account(address: &str) -> (&str, &str) {
    match address.split_once('/') {
        Some((t, u)) => (t, u),
        None => ("", address),
    }
}

/// A Slack error code that will not get better on a retry.
const PERMANENT: &[&str] = &[
    "user_not_found",
    "users_not_found",
    "account_inactive",
    "user_disabled",
    "invalid_auth",
    "not_authed",
    "token_revoked",
    "token_expired",
    "missing_scope",
    "not_allowed_token_type",
    "channel_not_found",
    "cant_update_message",
    "message_not_found",
    "edit_window_closed",
    "cannot_dm_bot",
    "user_is_bot",
    "invalid_blocks",
    "invalid_blocks_format",
    "invalid_arguments",
    "invalid_metadata_format",
    "msg_too_long",
    "no_text",
];

fn classify(e: TransportError) -> DeliveryResult {
    match e {
        TransportError::RateLimited { retry_after_ms } => DeliveryResult {
            retry_after_ms: retry_after_ms.min(i32::MAX as u32) as i32,
            ..DeliveryResult::failed(
                DeliverOutcome::Retry,
                "ratelimited",
                "Slack asked for a pause",
            )
        },
        TransportError::Server(code) => DeliveryResult::failed(
            DeliverOutcome::Retry,
            "slack_server_error",
            &format!("Slack answered HTTP {code}"),
        ),
        TransportError::Unavailable(m) => {
            DeliveryResult::failed(DeliverOutcome::Retry, "unavailable", &m)
        }
        TransportError::Api(code) => {
            let outcome = if PERMANENT.contains(&code.as_str()) {
                DeliverOutcome::PermanentFailure
            } else {
                DeliverOutcome::Retry
            };
            // The vendor's code is short and plain; anything else is not echoed.
            let shown = if plain(&code) && code.len() <= 40 {
                code.as_str()
            } else {
                "slack_error"
            };
            DeliveryResult::failed(outcome, shown, "Slack refused the call")
        }
    }
}

fn metadata(kind: &str, notification_id: &str) -> Value {
    json!({
        "event_type": format!("vam.{kind}"),
        "event_payload": { "notification_id": notification_id },
    })
}

fn str_of(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_string)
}

#[async_trait]
impl ChannelAdapter for SlackAdapter {
    fn channel(&self) -> &'static str {
        CHANNEL
    }

    fn capabilities(&self) -> Capabilities {
        capabilities()
    }

    async fn deliver(
        &self,
        rendering: &NotificationRendering,
        binding: &Binding,
        credentials: &Credentials,
    ) -> DeliveryResult {
        let Some(token) = credentials.get(BOT_TOKEN_KEY).filter(|t| !t.is_empty()) else {
            return DeliveryResult::failed(
                DeliverOutcome::PermanentFailure,
                "no_token",
                "the call carried no bot token",
            );
        };
        let rendered = render::render(rendering, &binding.delivery_id, &capabilities());
        let kind = if binding.kind.is_empty() {
            "notification"
        } else {
            binding.kind.as_str()
        };

        let (method, body) = match &binding.update_of {
            Some(orig) => (
                "chat.update",
                json!({
                    "channel": orig.channel_id,
                    "ts": orig.ts,
                    "text": rendered.text,
                    "blocks": rendered.blocks,
                    "metadata": metadata(kind, &binding.notification_id),
                }),
            ),
            None => {
                let user = user_of(&binding.address);
                if user.is_empty() {
                    return DeliveryResult::failed(
                        DeliverOutcome::PermanentFailure,
                        "user_not_found",
                        "the contact has no Slack user id",
                    );
                }
                let opened = match self
                    .transport
                    .call("conversations.open", json!({ "users": user }), token)
                    .await
                {
                    Ok(v) => v,
                    Err(e) => return classify(e),
                };
                let Some(dm) = opened
                    .pointer("/channel/id")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                else {
                    return DeliveryResult::failed(
                        DeliverOutcome::Retry,
                        "bad_response",
                        "conversations.open returned no channel",
                    );
                };
                (
                    "chat.postMessage",
                    json!({
                        "channel": dm,
                        "text": rendered.text,
                        "blocks": rendered.blocks,
                        "metadata": metadata(kind, &binding.notification_id),
                        "unfurl_links": false,
                        "unfurl_media": false,
                    }),
                )
            }
        };
        let sent_to = str_of(&body, "channel").unwrap_or_default();
        match self.transport.call(method, body, token).await {
            Ok(v) => {
                let channel_id = str_of(&v, "channel").unwrap_or(sent_to);
                let Some(ts) = str_of(&v, "ts") else {
                    return DeliveryResult::failed(
                        DeliverOutcome::Retry,
                        "bad_response",
                        "Slack returned no message ts",
                    );
                };
                DeliveryResult {
                    external: Some(ExternalRef { channel_id, ts }),
                    offered: rendered.offered,
                    vendor_status: "ok".into(),
                    ..DeliveryResult::new(DeliverOutcome::Sent)
                }
            }
            Err(e) => classify(e),
        }
    }

    fn receive(&self, event: InboundEvent) -> Option<Relay> {
        match event {
            InboundEvent::Click {
                delivery_id,
                scope,
                user,
                decision,
                response,
            } => (plain(&delivery_id) && !decision.is_empty() && !user.is_empty()).then_some(
                Relay::Decision {
                    delivery_id,
                    scope,
                    user,
                    decision,
                    response,
                },
            ),
            InboundEvent::DirectMessage { scope, user, text } => {
                if user.is_empty() {
                    return None;
                }
                extract_code(&text).map(|code| Relay::LinkCode { code, scope, user })
            }
        }
    }

    async fn reply(
        &self,
        address: &str,
        channel_hint: Option<&str>,
        text: &str,
        ephemeral: bool,
        credentials: &Credentials,
    ) -> Result<(), String> {
        // The token comes from `vamd` for this one reply (`GetChannelConfig`, purpose `reply`);
        // the live transport then refuses with invalid_auth and the caller logs it.
        let token = credentials.get(BOT_TOKEN_KEY).unwrap_or("");
        let user = user_of(address);
        if user.is_empty() {
            return Err("no Slack user id".into());
        }
        let err = |e: TransportError| e.to_string();
        if ephemeral {
            let channel = channel_hint.unwrap_or(user);
            self.transport
                .call(
                    "chat.postEphemeral",
                    json!({ "channel": channel, "user": user, "text": text }),
                    token,
                )
                .await
                .map_err(err)?;
            return Ok(());
        }
        let opened = self
            .transport
            .call("conversations.open", json!({ "users": user }), token)
            .await
            .map_err(err)?;
        let dm = opened
            .pointer("/channel/id")
            .and_then(Value::as_str)
            .ok_or("conversations.open returned no channel")?
            .to_string();
        self.transport
            .call(
                "chat.postMessage",
                json!({ "channel": dm, "text": text }),
                token,
            )
            .await
            .map_err(err)?;
        Ok(())
    }

    async fn connect(&self, app_token: &str) -> Result<InboundStream, String> {
        self.transport
            .open_socket(app_token)
            .await
            .map_err(|e| e.to_string())
    }
}

/// A link code in a DM: the shown form `XXXXX-XXXXX`, or a bare ten-symbol token with a digit
/// when it is the whole message. Returns it upper-cased; `vamd` normalises the rest.
pub fn extract_code(text: &str) -> Option<String> {
    let trimmed = text
        .trim()
        .trim_matches(|c: char| matches!(c, '`' | '*' | '_' | '"' | '\''));
    let is_code = |t: &str| {
        let t = t.trim_matches(|c: char| {
            matches!(
                c,
                '`' | '*' | '_' | '"' | '\'' | '.' | ',' | ':' | ';' | '!' | '?' | '(' | ')'
            )
        });
        let b = t.as_bytes();
        let dashed = b.len() == 11
            && b[5] == b'-'
            && b[..5].iter().chain(&b[6..]).all(u8::is_ascii_alphanumeric);
        dashed.then(|| t.to_ascii_uppercase())
    };
    if let Some(c) = trimmed.split_whitespace().find_map(is_code) {
        return Some(c);
    }
    let whole = trimmed;
    (whole.len() == 10
        && whole.bytes().all(|c| c.is_ascii_alphanumeric())
        && whole.bytes().any(|c| c.is_ascii_digit()))
    .then(|| whole.to_ascii_uppercase())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use async_trait::async_trait;

    /// Plays back scripted answers and records what it was asked.
    struct Scripted {
        calls: Mutex<Vec<(String, Value, String)>>,
        answers: Mutex<Vec<Result<Value, TransportError>>>,
    }

    impl Scripted {
        fn new(answers: Vec<Result<Value, TransportError>>) -> Arc<Self> {
            Arc::new(Self {
                calls: Mutex::new(Vec::new()),
                answers: Mutex::new(answers),
            })
        }
    }

    #[async_trait]
    impl SlackTransport for Scripted {
        async fn call(
            &self,
            method: &str,
            body: Value,
            token: &str,
        ) -> Result<Value, TransportError> {
            self.calls
                .lock()
                .unwrap()
                .push((method.into(), body, token.into()));
            let mut a = self.answers.lock().unwrap();
            if a.is_empty() {
                Ok(json!({ "ok": true }))
            } else {
                a.remove(0)
            }
        }
        async fn open_socket(&self, _t: &str) -> Result<InboundStream, TransportError> {
            Err(TransportError::Unavailable("none".into()))
        }
    }

    fn creds() -> Credentials {
        Credentials::new([(BOT_TOKEN_KEY.to_string(), "xoxb-test".to_string())].into())
    }

    fn binding() -> Binding {
        Binding {
            address: "T1/U1".into(),
            delivery_id: "nd_1".into(),
            notification_id: "nt_1".into(),
            kind: "approval.pending".into(),
            update_of: None,
        }
    }

    fn rendering() -> NotificationRendering {
        NotificationRendering {
            title: "Approve?".into(),
            actions: vec![vam_spec::v1::RenderedAction {
                id: "approve".into(),
                label: "Approve".into(),
                decision: "APPROVED".into(),
                option: String::new(),
                style: 1,
            }],
            ..Default::default()
        }
    }

    #[test]
    fn capabilities_are_slacks_set() {
        let c = capabilities();
        assert!(c.takes_decisions && c.needs_verified_contact);
        assert_eq!(c.max_select_options, 100);
        for p in [
            "Text",
            "Button",
            "Select",
            "MultiSelect",
            "Toggle",
            "Input",
            "DateInput",
        ] {
            assert!(c.native_primitives.iter().any(|x| x == p), "{p}");
        }
        for p in ["FileInput", "NumberInput", "ProgressBar"] {
            assert!(!c.native_primitives.iter().any(|x| x == p), "{p}");
        }
    }

    #[tokio::test]
    async fn delivery_opens_the_dm_then_posts_with_the_token_and_the_kind() {
        let t = Scripted::new(vec![
            Ok(json!({ "ok": true, "channel": { "id": "D77" } })),
            Ok(json!({ "ok": true, "channel": "D77", "ts": "1700.000001" })),
        ]);
        let a = SlackAdapter::new(t.clone());
        let r = a.deliver(&rendering(), &binding(), &creds()).await;
        assert_eq!(r.outcome, DeliverOutcome::Sent);
        assert_eq!(
            r.external,
            Some(ExternalRef {
                channel_id: "D77".into(),
                ts: "1700.000001".into()
            })
        );
        assert_eq!(r.offered.len(), 1);
        let calls = t.calls.lock().unwrap();
        assert_eq!(calls[0].0, "conversations.open");
        assert_eq!(calls[0].1["users"], "U1");
        assert_eq!(calls[1].0, "chat.postMessage");
        assert_eq!(calls[1].1["channel"], "D77");
        assert_eq!(calls[1].1["metadata"]["event_type"], "vam.approval.pending");
        assert!(calls.iter().all(|c| c.2 == "xoxb-test"));
        assert!(
            !calls[1].1.to_string().contains("xoxb"),
            "the token is not in the body"
        );
    }

    #[tokio::test]
    async fn an_update_edits_the_stored_message_and_drops_the_buttons() {
        let t = Scripted::new(vec![Ok(
            json!({ "ok": true, "channel": "D77", "ts": "1700.000001" }),
        )]);
        let a = SlackAdapter::new(t.clone());
        let mut b = binding();
        b.delivery_id = "nd_2".into();
        b.update_of = Some(ExternalRef {
            channel_id: "D77".into(),
            ts: "1700.000001".into(),
        });
        let upd = NotificationRendering {
            title: "Decided by Ann in app at 2026-11-04 10:00 UTC".into(),
            body: "Approve?".into(),
            ..Default::default()
        };
        let r = a.deliver(&upd, &b, &creds()).await;
        assert_eq!(r.outcome, DeliverOutcome::Sent);
        assert!(r.offered.is_empty());
        let calls = t.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "chat.update");
        assert_eq!(calls[0].1["ts"], "1700.000001");
        assert!(!calls[0].1["blocks"].to_string().contains("\"button\""));
    }

    #[tokio::test]
    async fn vendor_answers_map_to_retry_after_or_permanent_failure() {
        let cases: Vec<(TransportError, DeliverOutcome, &str, i32)> = vec![
            (
                TransportError::RateLimited {
                    retry_after_ms: 7000,
                },
                DeliverOutcome::Retry,
                "ratelimited",
                7000,
            ),
            (
                TransportError::Server(503),
                DeliverOutcome::Retry,
                "slack_server_error",
                0,
            ),
            (
                TransportError::Unavailable("net".into()),
                DeliverOutcome::Retry,
                "unavailable",
                0,
            ),
            (
                TransportError::Api("user_not_found".into()),
                DeliverOutcome::PermanentFailure,
                "user_not_found",
                0,
            ),
            (
                TransportError::Api("invalid_auth".into()),
                DeliverOutcome::PermanentFailure,
                "invalid_auth",
                0,
            ),
            (
                TransportError::Api("internal_error".into()),
                DeliverOutcome::Retry,
                "internal_error",
                0,
            ),
        ];
        for (err, outcome, code, after) in cases {
            let t = Scripted::new(vec![Err(err)]);
            let r = SlackAdapter::new(t)
                .deliver(&rendering(), &binding(), &creds())
                .await;
            assert_eq!(
                (r.outcome, r.error_code.as_str(), r.retry_after_ms),
                (outcome, code, after)
            );
        }
    }

    #[tokio::test]
    async fn no_token_is_refused_before_any_call() {
        let t = Scripted::new(vec![]);
        let r = SlackAdapter::new(t.clone())
            .deliver(&rendering(), &binding(), &Credentials::default())
            .await;
        assert_eq!(r.outcome, DeliverOutcome::PermanentFailure);
        assert!(t.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn a_click_and_a_dm_become_relays() {
        let a = SlackAdapter::new(Scripted::new(vec![]));
        let r = a.receive(InboundEvent::Click {
            delivery_id: "nd_1".into(),
            scope: "T1".into(),
            user: "U1".into(),
            decision: "approve".into(),
            response: None,
        });
        assert_eq!(
            r,
            Some(Relay::Decision {
                delivery_id: "nd_1".into(),
                scope: "T1".into(),
                user: "U1".into(),
                decision: "approve".into(),
                response: None
            })
        );
        let r = a.receive(InboundEvent::DirectMessage {
            scope: "T1".into(),
            user: "U1".into(),
            text: "here: abcde-fg3hj thanks".into(),
        });
        assert_eq!(
            r,
            Some(Relay::LinkCode {
                code: "ABCDE-FG3HJ".into(),
                scope: "T1".into(),
                user: "U1".into()
            })
        );
        assert_eq!(
            a.receive(InboundEvent::DirectMessage {
                scope: "T1".into(),
                user: "U1".into(),
                text: "hello understand".into()
            }),
            None
        );
        assert_eq!(
            a.receive(InboundEvent::Click {
                delivery_id: "xoxb-1".into(),
                scope: "T1".into(),
                user: "U1".into(),
                decision: "approve".into(),
                response: None
            }),
            None
        );
    }

    #[test]
    fn codes_are_found_but_ordinary_words_are_not() {
        assert_eq!(extract_code("ABCDE-FGHJK"), Some("ABCDE-FGHJK".into()));
        assert_eq!(extract_code("`abcde-fghjk`"), Some("ABCDE-FGHJK".into()));
        assert_eq!(extract_code("ABCDEFG123"), Some("ABCDEFG123".into()));
        assert_eq!(extract_code("understand"), None);
        assert_eq!(extract_code("please link ABCDEFG123"), None);
        assert_eq!(extract_code(""), None);
    }

    #[tokio::test]
    async fn a_dm_reply_opens_the_dm_and_an_ephemeral_does_not() {
        let t = Scripted::new(vec![Ok(json!({ "ok": true, "channel": { "id": "D9" } }))]);
        let a = SlackAdapter::new(t.clone());
        a.reply("T1/U1", None, "Linked.", false, &creds())
            .await
            .unwrap();
        a.reply("T1/U1", Some("D9"), "Try again in the app.", true, &creds())
            .await
            .unwrap();
        let calls = t.calls.lock().unwrap();
        let methods: Vec<&str> = calls.iter().map(|c| c.0.as_str()).collect();
        assert_eq!(
            methods,
            [
                "conversations.open",
                "chat.postMessage",
                "chat.postEphemeral"
            ]
        );
        assert_eq!(calls[2].1["channel"], "D9");
    }
}
