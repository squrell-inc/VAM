//! The channel adapter interface (docs: hosted/notify). `vamd` produces one neutral
//! rendering; an adapter turns it into a channel message and turns a channel event into a relay.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::mpsc;
use vam_spec::v1::{DeliverOutcome, NotificationRendering};

/// What an adapter can do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capabilities {
    /// Jig primitives rendered natively (catalog names: `Stack`, `Text`, `Input`, ...).
    pub native_primitives: Vec<String>,
    /// The most options one select can carry.
    pub max_select_options: usize,
    /// Whether a decision can come back through this channel.
    pub takes_decisions: bool,
    /// Whether delivery needs a verified contact.
    pub needs_verified_contact: bool,
}

/// The vendor's id of a sent message (Slack channel id and message ts). Not a secret.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExternalRef {
    pub channel_id: String,
    pub ts: String,
}

/// Where one delivery goes: the verified contact's address (Slack `team/user`) and, for an
/// update, the vendor ids the broker stored for the original delivery.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Binding {
    pub address: String,
    pub delivery_id: String,
    pub notification_id: String,
    /// The record kind (`approval.pending`), carried to the channel as message metadata.
    pub kind: String,
    /// The original message to edit, resolved by the broker from its delivery log.
    pub update_of: Option<ExternalRef>,
}

/// One action a message offered: the id the button carried and what the broker tells `vamd`
/// when it is pressed (the gate option id when there is one, else the decision word).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Offered {
    pub id: String,
    pub decision: String,
}

/// Secrets for one call. They arrive with the call and are dropped with it; `Debug` hides them.
#[derive(Clone, Default)]
pub struct Credentials(BTreeMap<String, String>);

impl Credentials {
    pub fn new(map: BTreeMap<String, String>) -> Self {
        Self(map)
    }
    pub fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(String::as_str)
    }
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credentials")
            .field("keys", &self.0.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// The result of one `deliver`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryResult {
    pub outcome: DeliverOutcome,
    pub error_code: String,
    pub message: String,
    pub retry_after_ms: i32,
    /// Where the message landed (set on SENT).
    pub external: Option<ExternalRef>,
    /// The actions the sent message actually carries.
    pub offered: Vec<Offered>,
    /// What the vendor last said (`ok`, `ratelimited`, `user_not_found`, ...).
    pub vendor_status: String,
}

impl DeliveryResult {
    pub fn new(outcome: DeliverOutcome) -> Self {
        Self {
            outcome,
            error_code: String::new(),
            message: String::new(),
            retry_after_ms: 0,
            external: None,
            offered: Vec::new(),
            vendor_status: String::new(),
        }
    }

    pub fn failed(outcome: DeliverOutcome, code: &str, message: &str) -> Self {
        Self {
            error_code: code.into(),
            message: message.into(),
            vendor_status: code.into(),
            ..Self::new(outcome)
        }
    }
}

/// Events a live channel connection produces.
pub type InboundStream = mpsc::Receiver<InboundEvent>;

/// A channel event as the adapter's transport sees it. `Debug` hides a DM's text: it may carry a
/// link code.
#[derive(Clone, PartialEq)]
pub enum InboundEvent {
    /// A button or form submit. `delivery_id` is the opaque id the message carried and `decision`
    /// the id of the action pressed.
    Click {
        delivery_id: String,
        scope: String,
        user: String,
        decision: String,
        response: Option<serde_json::Value>,
    },
    /// A direct message to the bot (carries a link code).
    DirectMessage {
        scope: String,
        user: String,
        text: String,
    },
}

impl InboundEvent {
    /// The workspace (team id) the event came from; empty when the channel gave none.
    pub fn scope(&self) -> &str {
        match self {
            Self::Click { scope, .. } | Self::DirectMessage { scope, .. } => scope,
        }
    }
}

impl fmt::Debug for InboundEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Click {
                delivery_id,
                scope,
                user,
                decision,
                ..
            } => write!(f, "Click({delivery_id}, {scope}/{user}, {decision})"),
            Self::DirectMessage { scope, user, .. } => {
                write!(f, "DirectMessage({scope}/{user}, <text hidden>)")
            }
        }
    }
}

/// What the broker relays to `vamd`'s gateway. `Debug` hides a link code.
#[derive(Clone, PartialEq)]
pub enum Relay {
    /// A press. `decision` is the id of the action pressed; the broker maps it to what `vamd` is
    /// told, from the actions the message offered. There is no surface here: `vamd` takes it from
    /// the handle it issued.
    Decision {
        delivery_id: String,
        scope: String,
        user: String,
        decision: String,
        response: Option<serde_json::Value>,
    },
    LinkCode {
        code: String,
        scope: String,
        user: String,
    },
}

impl fmt::Debug for Relay {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Decision {
                delivery_id,
                scope,
                user,
                decision,
                ..
            } => write!(f, "Decision({delivery_id}, {scope}/{user}, {decision})"),
            Self::LinkCode { scope, user, .. } => {
                write!(f, "LinkCode(<hidden>, {scope}/{user})")
            }
        }
    }
}

#[async_trait]
pub trait ChannelAdapter: Send + Sync {
    /// The adapter's channel name (`slack`).
    fn channel(&self) -> &'static str;
    fn capabilities(&self) -> Capabilities;
    async fn deliver(
        &self,
        rendering: &NotificationRendering,
        binding: &Binding,
        credentials: &Credentials,
    ) -> DeliveryResult;
    fn receive(&self, event: InboundEvent) -> Option<Relay>;
    /// Tells the person something short in the channel (a link outcome, "try again in the app").
    /// `channel_hint` is the vendor channel of an earlier delivery to them, when there is one.
    async fn reply(
        &self,
        address: &str,
        channel_hint: Option<&str>,
        text: &str,
        ephemeral: bool,
        credentials: &Credentials,
    ) -> Result<(), String>;
    /// Opens the live connection with the app-level token, which is used for this call only and
    /// not kept. The default has no live connection.
    async fn connect(&self, _app_token: &str) -> Result<InboundStream, String> {
        Err("this adapter has no live connection".into())
    }
}

/// The adapters this broker serves, by channel name.
#[derive(Default, Clone)]
pub struct Registry {
    adapters: BTreeMap<&'static str, Arc<dyn ChannelAdapter>>,
}

impl Registry {
    pub fn with(mut self, adapter: Arc<dyn ChannelAdapter>) -> Self {
        self.adapters.insert(adapter.channel(), adapter);
        self
    }
    pub fn get(&self, channel: &str) -> Option<Arc<dyn ChannelAdapter>> {
        self.adapters.get(channel).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_debug_hides_values() {
        let c = Credentials::new(BTreeMap::from([("bot".into(), "xoxb-secret".into())]));
        let shown = format!("{c:?}");
        assert!(shown.contains("bot") && !shown.contains("xoxb-secret"));
        assert_eq!(c.get("bot"), Some("xoxb-secret"));
    }

    #[test]
    fn a_dm_and_a_relay_never_print_the_code() {
        let ev = InboundEvent::DirectMessage {
            scope: "T1".into(),
            user: "U1".into(),
            text: "ABCDE-FGHJK".into(),
        };
        assert!(!format!("{ev:?}").contains("ABCDE"));
        let relay = Relay::LinkCode {
            code: "ABCDE-FGHJK".into(),
            scope: "T1".into(),
            user: "U1".into(),
        };
        assert!(!format!("{relay:?}").contains("ABCDE"));
    }
}
