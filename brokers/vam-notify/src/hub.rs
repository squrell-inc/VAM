//! The broker's core: delivers a rendering through an adapter, keeps the delivery log, holds the
//! action handles in memory, and relays what a person does back to `vamd`'s gateway.
//!
//! What lives where:
//! - the delivery log (SQLite, ids only): [`crate::log`];
//! - the action handle of a delivery, and the actions its message offered: **memory only**, in
//!   [`Hub::pending`]. Never written down. A restart drops them, and a click on a message from
//!   before the restart is answered "try again in the app" until `vamd` mints new ones (it does,
//!   as an edit of the sent message carrying a new handle, when this broker's start asks for its
//!   config);
//! - the bot token: in the call that carries it, nowhere else. For a reply to a person it is
//!   asked of `vamd` for that one reply (`GetChannelConfig`, purpose `reply`) and dropped;
//! - the app-level token: fetched with `GetChannelConfig`, handed to the connect call, dropped.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use vam_spec::harness::json_to_struct;
use vam_spec::v1::{
    ActOnNotificationRequest, ActOnOutcome, DeliverNotificationRequest,
    DeliverNotificationResponse, DeliverOutcome, ExternalAccount, GetChannelConfigRequest,
    LinkChannelAccountRequest, LinkOutcome,
};

use crate::adapter::{
    Binding, ChannelAdapter, Credentials, ExternalRef, InboundEvent, InboundStream, Offered,
    Registry, Relay,
};
use crate::gateway::{GatewayClient, GatewayError};
use crate::log::{plain, DeliveryLog};

/// Key of the app-level token in `GetChannelConfigResponse.resolved_secrets`.
pub const APP_TOKEN_KEY: &str = "app_token";
/// Key of the bot token, in `DeliverNotificationRequest.resolved_secrets` and, when `vamd` sends
/// it there, in a config answer (used for a reply, then dropped).
pub const BOT_TOKEN_KEY: &str = "bot_token";
/// `GetChannelConfigRequest.purpose` of a start or reconnect: `vamd` revokes this broker's open
/// action handles and queues fresh ones (the broker keeps handles in memory only).
/// The key of the pinned workspace (Slack team id) in the config's non-secret `settings`.
pub const PINNED_WORKSPACE_KEY: &str = "pinned_workspace";
pub const PURPOSE_CONNECT: &str = "connect";
/// `GetChannelConfigRequest.purpose` of one reply to a person: `vamd` returns the bot token and
/// changes nothing. The broker drops the token when the reply is sent.
pub const PURPOSE_REPLY: &str = "reply";
/// How many deliveries' handles the broker holds in memory (the oldest go first).
const MAX_HELD: usize = 4096;

/// A secret string. `Debug` never shows it; it is not `Serialize`.
#[derive(Clone)]
pub struct Secret(String);

impl Secret {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<hidden>)")
    }
}

/// What the broker remembers, in memory, about one delivered message.
#[derive(Debug, Clone)]
pub struct Held {
    pub kind: String,
    /// The Slack account the message went to (`team/user`).
    pub address: String,
    pub handle: Option<Secret>,
    pub offered: Vec<Offered>,
    pub seq: u64,
}

#[derive(Default)]
struct Pending {
    map: HashMap<String, Held>,
    seq: u64,
}

/// Per-call changes to the relay path (the test transport's forged and foreign handles).
#[derive(Default)]
pub struct Overrides {
    pub handle: Option<Secret>,
    pub gateway: Option<GatewayClient>,
}

/// What a relayed event came to, as a status word `vamd`'s outcomes share (`OK`, `INVALID`,
/// `NOT_LINKED`, ..., `LINKED`) plus a short detail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Answer {
    pub status: String,
    pub detail: String,
    /// False when the call could not reach `vamd`'s gateway.
    pub reached: bool,
}

impl Answer {
    fn new(status: &str, detail: &str) -> Self {
        Self {
            status: status.into(),
            detail: detail.into(),
            reached: true,
        }
    }
}

pub struct Hub {
    registry: Registry,
    log: DeliveryLog,
    gateway: GatewayClient,
    pending: Mutex<Pending>,
    /// One delivery at a time: a repeated `delivery_id` finds the first one's row.
    deliver_lock: tokio::sync::Mutex<()>,
    /// The workspace (team id) this node is pinned to, from the last channel config. Events from
    /// another team are dropped. Not a secret.
    pinned_workspace: Mutex<Option<String>>,
    #[cfg(feature = "fake-transport")]
    pub(crate) fake: crate::control::FakeState,
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn response(
    outcome: DeliverOutcome,
    code: &str,
    message: &str,
    after_ms: i32,
) -> DeliverNotificationResponse {
    DeliverNotificationResponse {
        outcome: outcome as i32,
        error_code: code.into(),
        message: message.into(),
        retry_after_ms: after_ms,
    }
}

impl Hub {
    pub fn new(registry: Registry, log: DeliveryLog, gateway: GatewayClient) -> Arc<Self> {
        Arc::new(Self {
            registry,
            log,
            gateway,
            pending: Mutex::new(Pending::default()),
            deliver_lock: tokio::sync::Mutex::new(()),
            pinned_workspace: Mutex::new(None),
            #[cfg(feature = "fake-transport")]
            fake: Default::default(),
        })
    }

    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    pub fn log(&self) -> &DeliveryLog {
        &self.log
    }

    /// The broker's own gateway client, when it has a socket.
    #[cfg_attr(not(feature = "fake-transport"), allow(dead_code))]
    pub(crate) fn gateway_client(&self) -> Option<GatewayClient> {
        self.gateway.is_configured().then(|| self.gateway.clone())
    }

    // ---- delivery ----------------------------------------------------------------------------

    /// `HostedNotificationChannel.DeliverNotification`: idempotent by `delivery_id`.
    pub async fn deliver(&self, req: DeliverNotificationRequest) -> DeliverNotificationResponse {
        let Some(adapter) = self.registry.get(&req.channel) else {
            return response(
                DeliverOutcome::Unsupported,
                "unknown_channel",
                &format!("no adapter for channel '{}'", req.channel),
                0,
            );
        };
        let bad_id = !plain(&req.delivery_id)
            || !plain(&req.channel)
            || (!req.notification_id.is_empty() && !plain(&req.notification_id))
            || (!req.update_of.is_empty() && !plain(&req.update_of));
        if bad_id {
            return response(
                DeliverOutcome::PermanentFailure,
                "invalid_request",
                "delivery, notification and channel ids must be plain ids",
                0,
            );
        }
        let _one = self.deliver_lock.lock().await;

        match self.log.get(&req.delivery_id) {
            Ok(Some(row)) if row.is_sent() => {
                return response(DeliverOutcome::Duplicate, "", "already delivered", 0)
            }
            Ok(_) => {}
            Err(e) => return store_error(&e),
        }
        let update_of = if req.update_of.is_empty() {
            None
        } else {
            match self.log.get(&req.update_of) {
                Ok(Some(row)) if row.is_sent() => Some(ExternalRef {
                    channel_id: row.slack_channel_id,
                    ts: row.message_ts,
                }),
                Ok(_) => {
                    return response(
                        DeliverOutcome::PermanentFailure,
                        "original_not_found",
                        "no sent message is logged for the delivery to update",
                        0,
                    )
                }
                Err(e) => return store_error(&e),
            }
        };
        if let Err(e) = self
            .log
            .begin_attempt(&req.delivery_id, &req.notification_id, &req.channel)
        {
            return store_error(&e);
        }

        let binding = Binding {
            address: req.channel_address.clone(),
            delivery_id: req.delivery_id.clone(),
            notification_id: req.notification_id.clone(),
            kind: req.kind.clone(),
            update_of: update_of.clone(),
        };
        // The secrets live in this value for this call.
        let credentials = Credentials::new(req.resolved_secrets.clone().into_iter().collect());
        let rendering = req.rendering.clone().unwrap_or_default();
        let result = adapter.deliver(&rendering, &binding, &credentials).await;
        drop(credentials);

        if result.outcome == DeliverOutcome::Sent {
            if let Some(ext) = &result.external {
                if let Err(e) =
                    self.log
                        .record_sent(&req.delivery_id, &ext.channel_id, &ext.ts, now_secs())
                {
                    // The message is out; a retry would post it twice. Say sent, log the fault.
                    tracing::error!(delivery_id = %req.delivery_id, error = %e, "delivery log write failed after send");
                }
            }
            // A post is held with its handle. So is an edit that carries a fresh handle: `vamd`
            // re-mints after a restart as an `update_of` the sent message with the actions and a
            // new handle, and its buttons point at this delivery.
            if update_of.is_none() || !req.action_handle.is_empty() {
                let handle =
                    (!req.action_handle.is_empty()).then(|| Secret::new(req.action_handle.clone()));
                self.hold(
                    &req.delivery_id,
                    &req.kind,
                    &req.channel_address,
                    handle,
                    result.offered.clone(),
                );
            }
        } else if let Err(e) = self
            .log
            .record_status(&req.delivery_id, &result.vendor_status)
        {
            tracing::error!(delivery_id = %req.delivery_id, error = %e, "delivery log write failed");
        }
        // Ids and outcomes only: never the rendering, the handle or the secrets.
        tracing::info!(delivery_id = %req.delivery_id, outcome = ?result.outcome, "deliver answered");
        DeliverNotificationResponse {
            outcome: result.outcome as i32,
            error_code: result.error_code,
            message: result.message,
            retry_after_ms: result.retry_after_ms,
        }
    }

    fn hold(
        &self,
        delivery_id: &str,
        kind: &str,
        address: &str,
        handle: Option<Secret>,
        offered: Vec<Offered>,
    ) {
        let Ok(mut p) = self.pending.lock() else {
            return;
        };
        p.seq += 1;
        let seq = p.seq;
        if p.map.len() >= MAX_HELD {
            if let Some(oldest) = p
                .map
                .iter()
                .min_by_key(|(_, h)| h.seq)
                .map(|(k, _)| k.clone())
            {
                p.map.remove(&oldest);
            }
        }
        p.map.insert(
            delivery_id.to_string(),
            Held {
                kind: kind.to_string(),
                address: address.to_string(),
                handle,
                offered,
                seq,
            },
        );
    }

    /// The held entry of a delivery.
    pub(crate) fn held(&self, delivery_id: &str) -> Option<Held> {
        self.pending.lock().ok()?.map.get(delivery_id).cloned()
    }

    /// The newest held message of `kind` (`latest` for any kind) to `account`; unless `strict`,
    /// when none went to that account, the newest of that kind to anyone.
    #[cfg_attr(not(feature = "fake-transport"), allow(dead_code))]
    pub(crate) fn pick(&self, kind: &str, account: &str, strict: bool) -> Option<(String, Held)> {
        let p = self.pending.lock().ok()?;
        let same =
            |a: &str, b: &str| a == b || crate::slack::user_of(a) == crate::slack::user_of(b);
        let newest = |only_account: bool| {
            let mut best: Option<(&String, &Held)> = None;
            for (k, h) in p.map.iter() {
                let kind_ok = kind == "latest" || h.kind == kind;
                if kind_ok
                    && (!only_account || same(&h.address, account))
                    && best.is_none_or(|(_, b)| h.seq > b.seq)
                {
                    best = Some((k, h));
                }
            }
            best.map(|(k, h)| (k.clone(), h.clone()))
        };
        newest(true).or_else(|| if strict { None } else { newest(false) })
    }

    /// `(delivery id, held)` of every held message, oldest first.
    #[cfg_attr(not(feature = "fake-transport"), allow(dead_code))]
    pub(crate) fn listing(&self) -> Vec<(String, Held)> {
        let Ok(p) = self.pending.lock() else {
            return Vec::new();
        };
        let mut v: Vec<(String, Held)> =
            p.map.iter().map(|(k, h)| (k.clone(), h.clone())).collect();
        v.sort_by_key(|(_, h)| h.seq);
        v
    }

    // ---- what a person does ------------------------------------------------------------------

    /// A channel event: a press becomes `ActOnNotification`, a DM with a code becomes
    /// `LinkChannelAccount`. The outcome is told to the person in the channel.
    pub async fn on_event(&self, channel: &str, event: InboundEvent, ov: Overrides) -> Answer {
        let Some(adapter) = self.registry.get(channel) else {
            return Answer::new("UNSUPPORTED", "no adapter for that channel");
        };
        match adapter.receive(event) {
            None => Answer::new("IGNORED", ""),
            Some(Relay::Decision {
                delivery_id,
                scope,
                user,
                decision,
                response,
            }) => {
                self.decision(&adapter, delivery_id, scope, user, decision, response, ov)
                    .await
            }
            Some(Relay::LinkCode { code, scope, user }) => {
                self.link(&adapter, code, scope, user).await
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn decision(
        &self,
        adapter: &Arc<dyn ChannelAdapter>,
        delivery_id: String,
        scope: String,
        user: String,
        decision: String,
        response: Option<serde_json::Value>,
        ov: Overrides,
    ) -> Answer {
        let address = format!("{scope}/{user}");
        let hint = self
            .log
            .get(&delivery_id)
            .ok()
            .flatten()
            .map(|r| r.slack_channel_id)
            .filter(|c| !c.is_empty());
        let Some(held) = self.held(&delivery_id) else {
            self.say(
                adapter,
                &address,
                hint.as_deref(),
                "That message is no longer available here. Open it in VAM.",
                true,
            )
            .await;
            return Answer::new("INVALID", "unknown or forgotten delivery");
        };
        // A press on something the message never offered is refused here, whatever the handle.
        let Some(offered) = held.offered.iter().find(|o| o.id == decision) else {
            self.say(
                adapter,
                &address,
                hint.as_deref(),
                "That action is not offered on this message.",
                true,
            )
            .await;
            return Answer::new("INVALID", "action not offered");
        };
        let handle = ov
            .handle
            .as_ref()
            .or(held.handle.as_ref())
            .map(|h| h.expose().to_string())
            .unwrap_or_default();
        let request = ActOnNotificationRequest {
            handle,
            external_account: Some(ExternalAccount { scope, id: user }),
            decision: offered.decision.clone(),
            response: response.as_ref().and_then(json_to_struct),
        };
        let gateway = ov.gateway.as_ref().unwrap_or(&self.gateway);
        match gateway.act(request).await {
            Ok(r) => {
                let outcome =
                    ActOnOutcome::try_from(r.outcome).unwrap_or(ActOnOutcome::Unspecified);
                let (status, text): (&str, Option<String>) = match outcome {
                    ActOnOutcome::Accepted => ("OK", None),
                    ActOnOutcome::AlreadyResolved => (
                        "ALREADY_RESOLVED",
                        Some(if r.decided_by.is_empty() {
                            "This was already decided.".to_string()
                        } else {
                            format!(
                                "This was already decided by {} in {}.",
                                r.decided_by, r.decided_surface
                            )
                        }),
                    ),
                    ActOnOutcome::NotLinked => (
                        "NOT_LINKED",
                        Some(
                            "This Slack account is not linked to the person this belongs to."
                                .into(),
                        ),
                    ),
                    ActOnOutcome::NotPermitted => (
                        "NOT_PERMITTED",
                        Some("That is not allowed from here. Open it in VAM.".into()),
                    ),
                    ActOnOutcome::Invalid => (
                        "INVALID",
                        Some("That action is not valid any more. Open it in VAM.".into()),
                    ),
                    ActOnOutcome::Expired => {
                        ("EXPIRED", Some("That has expired. Open it in VAM.".into()))
                    }
                    ActOnOutcome::RateLimited => (
                        "RATE_LIMITED",
                        Some("Too many actions just now. Try again in a moment.".into()),
                    ),
                    // Today `vamd` answers this until wave 5. The buttons stay.
                    ActOnOutcome::Unavailable | ActOnOutcome::Unspecified => (
                        "UNAVAILABLE",
                        Some("I could not record that just now. Try again in the app.".into()),
                    ),
                };
                if let Some(t) = text {
                    self.say(adapter, &address, hint.as_deref(), &t, true).await;
                }
                Answer::new(status, &r.message)
            }
            // A socket that does not serve the notification gateway for this broker refuses it
            // outright (permission denied, or no such service).
            Err(GatewayError::Status { code, message })
                if code == "PermissionDenied" || code == "Unimplemented" =>
            {
                self.say(
                    adapter,
                    &address,
                    hint.as_deref(),
                    "That is not allowed from here. Open it in VAM.",
                    true,
                )
                .await;
                Answer::new("NOT_PERMITTED", &format!("{code}: {message}"))
            }
            Err(e) => {
                tracing::warn!(delivery_id = %delivery_id, error = %e, "ActOnNotification did not reach vamd");
                self.say(
                    adapter,
                    &address,
                    hint.as_deref(),
                    "I could not record that just now. Try again in the app.",
                    true,
                )
                .await;
                Answer {
                    status: "UNAVAILABLE".into(),
                    detail: e.to_string(),
                    reached: false,
                }
            }
        }
    }

    async fn link(
        &self,
        adapter: &Arc<dyn ChannelAdapter>,
        code: String,
        scope: String,
        user: String,
    ) -> Answer {
        let address = format!("{scope}/{user}");
        // The code goes to `vamd` and is gone: not stored, not logged.
        let result = self
            .gateway
            .link(LinkChannelAccountRequest {
                code,
                external_account: Some(ExternalAccount { scope, id: user }),
                display: String::new(),
            })
            .await;
        let (status, text) = match &result {
            Ok(r) => match LinkOutcome::try_from(r.outcome).unwrap_or(LinkOutcome::Unspecified) {
                LinkOutcome::Linked => (
                    "LINKED",
                    "Linked. VAM will send your approvals and questions to this chat.",
                ),
                LinkOutcome::InvalidCode => (
                    "INVALID_CODE",
                    "That code did not work. Check it and send it again, or make a new one in VAM.",
                ),
                LinkOutcome::Expired => {
                    ("EXPIRED", "That code has expired. Make a new one in VAM.")
                }
                LinkOutcome::TooManyAttempts => (
                    "TOO_MANY_ATTEMPTS",
                    "Too many tries. Wait a while, then make a new code in VAM.",
                ),
                LinkOutcome::AlreadyLinked => ("ALREADY_LINKED", "This account is already linked."),
                LinkOutcome::WrongWorkspace => (
                    "WRONG_WORKSPACE",
                    "This VAM node is linked to a different Slack workspace.",
                ),
                LinkOutcome::Unavailable | LinkOutcome::Unspecified => (
                    "UNAVAILABLE",
                    "Linking is not available right now. Try again soon.",
                ),
            },
            Err(_) => (
                "UNAVAILABLE",
                "Linking is not available right now. Try again soon.",
            ),
        };
        self.say(adapter, &address, None, text, false).await;
        match result {
            Ok(_) => Answer::new(status, ""),
            Err(e) => Answer {
                status: status.into(),
                detail: e.to_string(),
                reached: false,
            },
        }
    }

    /// Tells the person something short. The bot token for this one call is asked of `vamd` and
    /// dropped (a config answer that carries none leaves the call without one).
    async fn say(
        &self,
        adapter: &Arc<dyn ChannelAdapter>,
        address: &str,
        hint: Option<&str>,
        text: &str,
        ephemeral: bool,
    ) {
        let credentials = self.reply_credentials(adapter.channel()).await;
        if let Err(e) = adapter
            .reply(address, hint, text, ephemeral, &credentials)
            .await
        {
            tracing::warn!(error = %e, "reply to the person failed");
        }
    }

    async fn reply_credentials(&self, channel: &str) -> Credentials {
        let mut map = std::collections::BTreeMap::new();
        if let Ok(mut cfg) = self
            .gateway
            .config(GetChannelConfigRequest {
                channel: channel.to_string(),
                purpose: PURPOSE_REPLY.to_string(),
            })
            .await
        {
            if let Some(bot) = cfg.resolved_secrets.remove(BOT_TOKEN_KEY) {
                map.insert(BOT_TOKEN_KEY.to_string(), bot);
            }
            cfg.resolved_secrets.clear();
        }
        Credentials::new(map)
    }

    // ---- the live connection -----------------------------------------------------------------

    /// Fetches the channel config and opens the connection with the app-level token, which is
    /// dropped as soon as the call returns. `None`: the channel is not enabled on this node.
    pub async fn connect_once(&self, channel: &str) -> Result<Option<InboundStream>, String> {
        let adapter = self
            .registry
            .get(channel)
            .ok_or_else(|| format!("no adapter for channel '{channel}'"))?;
        let mut cfg = self
            .gateway
            .config(GetChannelConfigRequest {
                channel: channel.to_string(),
                purpose: PURPOSE_CONNECT.to_string(),
            })
            .await
            .map_err(|e| e.to_string())?;
        if !cfg.enabled {
            return Ok(None);
        }
        let token = cfg.resolved_secrets.remove(APP_TOKEN_KEY);
        cfg.resolved_secrets.clear();
        if let Ok(mut pin) = self.pinned_workspace.lock() {
            *pin = cfg
                .settings
                .get(PINNED_WORKSPACE_KEY)
                .filter(|t| !t.is_empty())
                .cloned();
        }
        drop(cfg);
        let Some(token) = token.filter(|t| !t.is_empty()) else {
            return Err("the channel config carried no app token".into());
        };
        let stream = adapter.connect(&token).await;
        drop(token);
        stream.map(Some)
    }

    /// False when a workspace is pinned and `scope` is another team. Socket Mode authenticates the
    /// connection itself, so this is the only check an event needs; with no pin, `vamd` decides.
    fn in_pinned_workspace(&self, scope: &str) -> bool {
        match self.pinned_workspace.lock() {
            Ok(pin) => pin.as_deref().is_none_or(|p| p == scope),
            Err(_) => false,
        }
    }

    /// Keeps the connection up: asks `vamd` for the config on start and on every reconnect, and
    /// feeds what arrives to [`Hub::on_event`]. Returns at once when there is no gateway.
    pub async fn run_connection(self: Arc<Self>, channel: &'static str) {
        if !self.gateway.is_configured() {
            tracing::info!("no gateway socket: the {channel} connection stays down");
            return;
        }
        let mut backoff = Duration::from_secs(2);
        loop {
            match self.connect_once(channel).await {
                Ok(Some(mut events)) => {
                    backoff = Duration::from_secs(2);
                    tracing::info!("{channel} connection open");
                    while let Some(ev) = events.recv().await {
                        if !self.in_pinned_workspace(ev.scope()) {
                            tracing::debug!("an event from another workspace was ignored");
                            continue;
                        }
                        let hub = self.clone();
                        tokio::spawn(async move {
                            let answer = hub.on_event(channel, ev, Overrides::default()).await;
                            tracing::info!(status = %answer.status, "event handled");
                        });
                    }
                    tracing::warn!("{channel} connection closed; reconnecting");
                }
                Ok(None) => {
                    tracing::info!("{channel} is not enabled on this node; checking again later");
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    continue;
                }
                Err(e) => tracing::warn!(error = %e, "{channel} connection failed"),
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(60));
        }
    }
}

fn store_error(e: &crate::error::NotifyError) -> DeliverNotificationResponse {
    tracing::error!(error = %e, "delivery log");
    response(
        DeliverOutcome::Retry,
        "store_error",
        "the delivery log could not be used",
        0,
    )
}
