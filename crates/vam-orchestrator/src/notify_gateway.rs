//! The notification gateway (NT-2.3, NT-2.5; `design/notifications.md` sections 3.5, 3.6, 9.4).
//!
//! `HostedNotificationGateway` is served by `vamd` on the per-broker gateway socket of a channel
//! broker (`gateway.rs::sync_broker` opens it for a manifest with `notification_channel`). **The
//! broker is the socket a call arrived on; no request field names it.** Every RPC runs
//! [`prove_channel_broker`] first. The checks, in order, for every RPC:
//!
//! 1. the socket's broker has an approval row, and it is enabled (else `NOT_PERMITTED`);
//! 2. its trust tier is core or first-party (else `NOT_PERMITTED`; checked again here, whatever the
//!    registration said);
//! 3. its approved manifest has `notification_channel` (else `NOT_PERMITTED`; the trigger broker on
//!    its own socket stops here);
//! 4. `SO_PEERCRED`: when the approval records a peer uid, the caller's uid must equal it (else
//!    `NOT_PERMITTED`).
//!
//! Then each RPC:
//!
//! - `GetChannelConfig {channel}`: 5. the channel named must be the one the manifest declares
//!   (else `NOT_PERMITTED`); 6. the node setting must enable it (else `enabled: false`, no
//!   settings, no secret); 7. the adapter's app-level token is read from the org vault **for this
//!   call**, returned in `resolved_secrets`, and kept nowhere. Values are never logged.
//! - `LinkChannelAccount {code, external_account}`: 5. the node setting must enable the channel
//!   (else `UNAVAILABLE`); 6. the account is well formed (else `INVALID_CODE`); 7. a workspace other
//!   than the one already linked on this node is refused (`WRONG_WORKSPACE`, one workspace per
//!   node); 8. `verify_contact_with_code`, whose outcomes map one to one. The code is never logged.
//! - `GetChannelConfig {channel, purpose}`: see [`channel_config`]. A `connect` (the default) also
//!   revokes the broker's open action handles and queues fresh ones; a `reply` returns the bot
//!   token only.
//! - `ActOnNotification {handle, external_account, decision, response}`: after 1 to 4, in order:
//!   5. the handle parses, its hash matches (constant time), its holder is this socket's broker
//!   (else `INVALID`); 6. unexpired (else `EXPIRED`); 7. unused: a used or revoked handle answers
//!   `ALREADY_RESOLVED` when its record is resolved, else `INVALID`; 8. the record is unresolved
//!   (else `ALREADY_RESOLVED`, with who, which surface and when); 9. `external_account` is a
//!   verified, unrevoked contact of the handle's user and the handle's own contact (else
//!   `NOT_LINKED`; an account linked to a *different* user answers `INVALID`: the handle is not
//!   that person's); 10. the user is active (else `NOT_LINKED`); 11. the decision is in the
//!   handle's offered set and passes `validate_channel_decision` (never `AMENDED`, never a remember
//!   word), and a gate or question response validates (else `INVALID`); 12. the per-broker rate,
//!   `max_actions_per_minute` with 0 meaning 60 (else `RATE_LIMITED`); 13. `decide_as` with the
//!   handle's user and **the surface stored on the handle** (`allow_remember: false`). The request
//!   has no surface field, and nothing in it names a user. The handle is used up by the decision's
//!   own transaction (`notifications::resolve` revokes every handle of the record).
//!
//! Where each handle field comes from: user, contact, notification and approval from the record
//! `vamd` was delivering; channel from the contact; offered decisions from the rendering `vamd` built;
//! surface from the broker's operator-approved manifest (`notification_channel.surface`); holder
//! from the broker the delivery went to.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use secrecy::ExposeSecret;
use vam_spec::v1::hosted_notification_gateway_server::{
    HostedNotificationGateway, HostedNotificationGatewayServer,
};
use vam_spec::v1::{
    ActOnNotificationRequest, ActOnNotificationResponse, ActOnOutcome, GetChannelConfigRequest,
    GetChannelConfigResponse, LinkChannelAccountRequest, LinkChannelAccountResponse, LinkOutcome,
    NotificationChannel, TrustTier,
};

use crate::api::AppState;
use crate::contacts::LinkOutcome as Verified;
use crate::error::OrchestratorError;

/// `vam_notify_gateway_total{rpc, result}`.
pub const METRIC_GATEWAY_TOTAL: &str = "vam_notify_gateway_total";
/// Key of the app-level token in `GetChannelConfigResponse.resolved_secrets`.
pub const APP_TOKEN_KEY: &str = "app_token";
const MAX_CODE_BYTES: usize = 64;
const MAX_DISPLAY_BYTES: usize = 128;

/// Only core and first-party brokers may deliver notifications or take a decision back.
pub fn tier_may_deliver(trust_tier: i32) -> bool {
    matches!(
        TrustTier::try_from(trust_tier),
        Ok(TrustTier::Core | TrustTier::FirstParty)
    )
}

/// The service for one broker's socket. `broker_id` is the socket's, never the request's.
pub fn service(
    state: AppState,
    broker_id: String,
) -> HostedNotificationGatewayServer<NotifyGateway> {
    HostedNotificationGatewayServer::new(NotifyGateway {
        state: Arc::new(state),
        broker_id,
    })
}

#[derive(Clone)]
pub struct NotifyGateway {
    state: Arc<AppState>,
    broker_id: String,
}

/// What a broker is allowed to do on this socket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelBroker {
    pub channel: String,
    pub surface: String,
    /// `notification_channel.max_actions_per_minute`; 0 means 60.
    pub max_actions_per_minute: i64,
}

/// Why a call is refused before it is looked at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refused {
    NotPermitted(&'static str),
    Unavailable,
}

/// Checks 1 to 4 of the module docs. `peer_uid` is `SO_PEERCRED`'s uid when the transport has it.
pub fn prove_channel_broker(
    state: &AppState,
    broker_id: &str,
    peer_uid: Option<i64>,
) -> Result<ChannelBroker, Refused> {
    let store = &state.store;
    // 1. Socket -> broker; approval enabled.
    let approval = match store.get_hsp_broker_approval(broker_id) {
        Ok(Some(a)) if a.enabled => a,
        Ok(_) => return Err(Refused::NotPermitted("the broker is not approved")),
        Err(_) => return Err(Refused::Unavailable),
    };
    // 2. Tier.
    if !tier_may_deliver(approval.manifest.trust_tier) {
        return Err(Refused::NotPermitted(
            "the broker's trust tier may not deliver notifications",
        ));
    }
    // 3. The manifest declares a notification channel.
    let Some(NotificationChannel {
        channel,
        surface,
        max_actions_per_minute,
    }) = approval.manifest.notification_channel.clone()
    else {
        return Err(Refused::NotPermitted(
            "the broker has no notification channel",
        ));
    };
    // 4. SO_PEERCRED, when configured.
    match store.get_hsp_broker_gateway(broker_id) {
        Ok((_, Some(want))) if peer_uid != Some(want) => {
            return Err(Refused::NotPermitted("the caller is not the broker's user"));
        }
        Ok(_) => {}
        Err(_) => return Err(Refused::Unavailable),
    }
    Ok(ChannelBroker {
        channel,
        surface,
        max_actions_per_minute: i64::from(max_actions_per_minute),
    })
}

fn count(rpc: &str, result: &str) {
    let _ = vam_telemetry::VamMetricsRegistry::global().record_counter(
        METRIC_GATEWAY_TOTAL,
        1,
        &[("rpc", rpc), ("result", result)],
    );
}

fn status_of(refused: Refused) -> tonic::Status {
    match refused {
        Refused::NotPermitted(why) => tonic::Status::permission_denied(why),
        Refused::Unavailable => tonic::Status::unavailable("try again"),
    }
}

#[tonic::async_trait]
impl HostedNotificationGateway for NotifyGateway {
    async fn act_on_notification(
        &self,
        request: tonic::Request<ActOnNotificationRequest>,
    ) -> Result<tonic::Response<ActOnNotificationResponse>, tonic::Status> {
        let peer_uid = crate::gateway::peer_uid_of(&request);
        Ok(tonic::Response::new(
            act_on(
                &self.state,
                &self.broker_id,
                peer_uid,
                &request.into_inner(),
            )
            .await,
        ))
    }

    async fn link_channel_account(
        &self,
        request: tonic::Request<LinkChannelAccountRequest>,
    ) -> Result<tonic::Response<LinkChannelAccountResponse>, tonic::Status> {
        let peer_uid = crate::gateway::peer_uid_of(&request);
        link(
            &self.state,
            &self.broker_id,
            peer_uid,
            &request.into_inner(),
        )
        .map(tonic::Response::new)
    }

    async fn get_channel_config(
        &self,
        request: tonic::Request<GetChannelConfigRequest>,
    ) -> Result<tonic::Response<GetChannelConfigResponse>, tonic::Status> {
        let peer_uid = crate::gateway::peer_uid_of(&request);
        channel_config(
            &self.state,
            &self.broker_id,
            peer_uid,
            &request.into_inner(),
        )
        .map(tonic::Response::new)
    }
}

// ---- ActOnNotification -----------------------------------------------------------------------

/// Actions a broker may take in one minute when its manifest says 0.
pub const DEFAULT_ACTIONS_PER_MINUTE: i64 = 60;
const RATE_WINDOW: Duration = Duration::from_secs(60);

fn rate_book() -> &'static Mutex<BTreeMap<String, VecDeque<Instant>>> {
    static BOOK: OnceLock<Mutex<BTreeMap<String, VecDeque<Instant>>>> = OnceLock::new();
    BOOK.get_or_init(Default::default)
}

/// Counts one action of `broker_id` against its per-minute limit. `Err(ms)`: over the limit, try
/// again after that many milliseconds. Memory only; a restart forgets it.
fn rate_take(broker_id: &str, per_minute: i64, now: Instant) -> Result<(), i32> {
    let limit = if per_minute <= 0 {
        DEFAULT_ACTIONS_PER_MINUTE
    } else {
        per_minute
    } as usize;
    let Ok(mut book) = rate_book().lock() else {
        return Ok(());
    };
    let window = book.entry(broker_id.to_string()).or_default();
    while window
        .front()
        .is_some_and(|t| now.duration_since(*t) >= RATE_WINDOW)
    {
        window.pop_front();
    }
    if window.len() >= limit {
        let wait = window
            .front()
            .map(|t| RATE_WINDOW.saturating_sub(now.duration_since(*t)))
            .unwrap_or(RATE_WINDOW);
        return Err(i32::try_from(wait.as_millis()).unwrap_or(i32::MAX).max(1));
    }
    window.push_back(now);
    Ok(())
}

fn answer(outcome: ActOnOutcome, message: &str) -> ActOnNotificationResponse {
    count(
        "act_on_notification",
        &outcome.as_str_name().to_ascii_lowercase(),
    );
    ActOnNotificationResponse {
        outcome: outcome as i32,
        message: message.into(),
        ..Default::default()
    }
}

/// What the broker is told about a record that is already closed: who closed it, from where,
/// when. Empty parts when it was closed by the system (an expiry, a stopped run).
fn already_resolved(state: &AppState, notification_id: &str) -> ActOnNotificationResponse {
    let mut out = answer(ActOnOutcome::AlreadyResolved, "this was already decided");
    let Ok(conn) = state.store.connection.lock() else {
        return out;
    };
    let row: Option<(Option<i64>, Option<String>, Option<String>)> = conn
        .query_row(
            "SELECT resolved_at, acted_by_user_id, acted_surface FROM notifications WHERE id = ?1",
            [notification_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .ok();
    if let Some((at, by, surface)) = row {
        out.decided_at = at.map(|seconds| prost_types::Timestamp { seconds, nanos: 0 });
        out.decided_surface = surface.unwrap_or_default();
        if let Some(by) = by {
            out.decided_by = conn
                .query_row(
                    "SELECT username FROM users WHERE user_id = ?1",
                    [&by],
                    |r| r.get::<_, String>(0),
                )
                .unwrap_or(by);
        }
    }
    out
}

/// The record a handle names.
struct RecordState {
    user_id: String,
    approval_id: Option<String>,
    resolved: bool,
}

fn load_record(
    state: &AppState,
    notification_id: &str,
) -> Result<Option<RecordState>, OrchestratorError> {
    let conn = state
        .store
        .connection
        .lock()
        .map_err(|_| OrchestratorError::StorageUnavailable)?;
    Ok(rusqlite::OptionalExtension::optional(conn.query_row(
        "SELECT user_id, approval_id, resolved_at IS NOT NULL FROM notifications WHERE id = ?1",
        [notification_id],
        |r| {
            Ok(RecordState {
                user_id: r.get(0)?,
                approval_id: r.get(1)?,
                resolved: r.get(2)?,
            })
        },
    ))?)
}

/// The verified, unrevoked contact of `channel` at `address`: `(contact id, user id)`.
fn live_contact(
    state: &AppState,
    channel: &str,
    address: &str,
) -> Result<Option<(String, String)>, OrchestratorError> {
    let conn = state
        .store
        .connection
        .lock()
        .map_err(|_| OrchestratorError::StorageUnavailable)?;
    Ok(rusqlite::OptionalExtension::optional(conn.query_row(
        "SELECT id, user_id FROM contacts
         WHERE channel = ?1 AND address = ?2 AND verified_at IS NOT NULL AND revoked_at IS NULL",
        [channel, address],
        |r| Ok((r.get(0)?, r.get(1)?)),
    ))?)
}

/// Serves `ActOnNotification`. The checklist is in the module docs; every refusal leaves the
/// approval as it was and the handle usable (except that a resolved record spends its handles).
/// Public for tests.
pub async fn act_on(
    state: &AppState,
    broker_id: &str,
    peer_uid: Option<i64>,
    req: &ActOnNotificationRequest,
) -> ActOnNotificationResponse {
    // 1 to 4: the socket's broker.
    let proven = match prove_channel_broker(state, broker_id, peer_uid) {
        Ok(p) => p,
        Err(Refused::NotPermitted(why)) => return answer(ActOnOutcome::NotPermitted, why),
        Err(Refused::Unavailable) => {
            return answer(ActOnOutcome::Unavailable, "try again in a moment")
        }
    };
    let unavailable = || answer(ActOnOutcome::Unavailable, "try again in a moment");

    // 5. The handle: parses, hash matches, holder is this socket's broker.
    let handle = {
        let Ok(conn) = state.store.connection.lock() else {
            return unavailable();
        };
        match crate::credentials::find_action_handle(&conn, &req.handle, broker_id) {
            Ok(h) => h,
            Err(crate::credentials::HandleError::Invalid) => {
                return answer(ActOnOutcome::Invalid, "that action is not valid")
            }
            Err(crate::credentials::HandleError::Storage(_)) => return unavailable(),
        }
    };
    if handle.channel != proven.channel {
        return answer(ActOnOutcome::Invalid, "that action is not valid");
    }
    // 6. Unexpired.
    let now = crate::catalog::unix_timestamp();
    if handle.expires_at.is_none_or(|at| at <= now) {
        return answer(ActOnOutcome::Expired, "that action has expired");
    }
    let record = match load_record(state, &handle.notification_id) {
        Ok(Some(r)) => r,
        Ok(None) => return answer(ActOnOutcome::Invalid, "that action is not valid"),
        Err(_) => return unavailable(),
    };
    // 7. Unused: a used or revoked handle on a resolved record is "already resolved".
    if handle.revoked_at.is_some() {
        return if record.resolved {
            already_resolved(state, &handle.notification_id)
        } else {
            answer(ActOnOutcome::Invalid, "that action is not valid any more")
        };
    }
    // 8. The record is unresolved, and still the handle's approval.
    if record.resolved {
        return already_resolved(state, &handle.notification_id);
    }
    if record.user_id != handle.user_id
        || record.approval_id.as_deref() != Some(&handle.approval_id)
    {
        return answer(ActOnOutcome::Invalid, "that action is not valid any more");
    }
    // 9. The account is a verified, unrevoked contact of the handle's user, and the handle's own.
    let account = req
        .external_account
        .as_ref()
        .and_then(|a| account_of(&a.scope, &a.id));
    let Some(account) = account else {
        return answer(ActOnOutcome::NotLinked, "that account is not linked");
    };
    match live_contact(state, &handle.channel, &account) {
        Ok(Some((contact_id, user_id))) => {
            if user_id != handle.user_id {
                // A linked person presenting somebody else's handle.
                return answer(ActOnOutcome::Invalid, "that action is not valid");
            }
            if contact_id != handle.contact_id {
                return answer(ActOnOutcome::NotLinked, "that account is not linked");
            }
        }
        Ok(None) => return answer(ActOnOutcome::NotLinked, "that account is not linked"),
        Err(_) => return unavailable(),
    }
    // 10. The user is active.
    match state.store.get_user(&handle.user_id) {
        Ok(u) if !u.is_deactivated() => {}
        Ok(_) | Err(OrchestratorError::NotFound(_)) => {
            return answer(ActOnOutcome::NotLinked, "that account is not linked")
        }
        Err(_) => return unavailable(),
    }
    // 11. The decision is one the message offered and a channel may take; the response validates.
    let offered: Vec<&str> = handle.decisions.iter().map(String::as_str).collect();
    if vam_spec::validation::validate_channel_decision(&req.decision, &offered).is_err()
        || !offered.contains(&req.decision.as_str())
    {
        return answer(ActOnOutcome::Invalid, "that action was not offered");
    }
    let stored = match state
        .store
        .get_approval_for_owner(&handle.approval_id, &handle.user_id)
    {
        Ok(a) if a.published && a.decision.is_none() => a,
        Ok(_) | Err(OrchestratorError::ApprovalNotFound(_)) => {
            return answer(ActOnOutcome::Invalid, "that action is not valid any more")
        }
        Err(_) => return unavailable(),
    };
    let (decision, response_json): (&'static str, Option<String>) = match req.decision.as_str() {
        "APPROVED" => ("APPROVED", None),
        "REJECTED" => ("REJECTED", None),
        "SUBMITTED" => (
            "SUBMITTED",
            req.response
                .as_ref()
                .and_then(|s| vam_spec::encoding::proto_to_json("google.protobuf.Struct", s).ok()),
        ),
        // Anything else the handle offers is a gate option id.
        option if stored.kind == crate::hitl::APPROVAL_KIND_GATE => (
            "SUBMITTED",
            Some(serde_json::json!({ "option": option }).to_string()),
        ),
        _ => return answer(ActOnOutcome::Invalid, "that action was not offered"),
    };
    let checked = match stored.kind.as_str() {
        k if k == crate::hitl::APPROVAL_KIND_GATE && decision == "SUBMITTED" => response_json
            .as_deref()
            .ok_or(())
            .and_then(|r| {
                crate::hitl::validate_gate_response(r, &stored.amendable_schema_json)
                    .map_err(|_| ())
            })
            .is_ok(),
        k if k == crate::hitl::APPROVAL_KIND_QUESTION && decision == "SUBMITTED" => response_json
            .as_deref()
            .ok_or(())
            .and_then(|r| {
                crate::hitl::validate_question_answer(&stored.requested_args_json, r)
                    .map_err(|_| ())
            })
            .is_ok(),
        _ => true,
    };
    if !checked {
        return answer(ActOnOutcome::Invalid, "that answer does not fit");
    }
    // 12. The per-broker rate.
    if let Err(retry_after_ms) = rate_take(broker_id, proven.max_actions_per_minute, Instant::now())
    {
        let mut out = answer(ActOnOutcome::RateLimited, "too many actions just now");
        out.retry_after_ms = retry_after_ms;
        return out;
    }
    // 13. Decide, through the one function the HTTP route also uses. The user and the surface are
    // the handle's; `allow_remember` is false, so no standing rule can come from a channel.
    let caller = crate::api::decide::DeciderCaller {
        principal: format!("User::\"{}\"", handle.user_id),
        user_id: handle.user_id.clone(),
        surface: handle.surface.clone(),
        allow_remember: false,
    };
    let input = crate::api::decide::DecideInput {
        decision,
        remember: None,
        rejection_reason: if decision == "REJECTED" {
            format!("Denied from {}", handle.surface)
        } else {
            String::new()
        },
        amended_args_json: None,
        response_json,
        expected_revision: None,
    };
    let request_id = vam_spec::v1::ResourceId {
        urn: handle.approval_id.clone(),
    };
    match crate::api::decide::decide_as(state, caller, request_id, input).await {
        Ok(_) => {
            if let Ok(conn) = state.store.connection.lock() {
                let _ = crate::credentials::stamp_handle_used(&conn, &handle.key_id);
            }
            answer(ActOnOutcome::Accepted, "")
        }
        Err(OrchestratorError::ApprovalAlreadyDecided(_)) => {
            already_resolved(state, &handle.notification_id)
        }
        Err(OrchestratorError::Conflict(_)) => {
            if load_record(state, &handle.notification_id)
                .ok()
                .flatten()
                .is_some_and(|r| r.resolved)
            {
                already_resolved(state, &handle.notification_id)
            } else {
                answer(ActOnOutcome::Invalid, "that action is not valid any more")
            }
        }
        Err(OrchestratorError::InvalidContract(_) | OrchestratorError::ApprovalNotFound(_)) => {
            answer(ActOnOutcome::Invalid, "that action is not valid")
        }
        Err(OrchestratorError::PermissionDenied(_)) => answer(
            ActOnOutcome::NotPermitted,
            "that person may not decide this",
        ),
        Err(_) => unavailable(),
    }
}

// ---- LinkChannelAccount ----------------------------------------------------------------------

/// `scope/id` of an external account, or `None` when either part is empty, holds a `/`, or is not
/// printable ASCII.
fn account_of(scope: &str, id: &str) -> Option<String> {
    let part = |s: &str| {
        !s.is_empty() && s.len() <= 64 && s.chars().all(|c| c.is_ascii_graphic() && c != '/')
    };
    (part(scope) && part(id)).then(|| format!("{scope}/{id}"))
}

/// Link answers carry no `NOT_PERMITTED` value: a refused broker gets a `PermissionDenied` status.
#[allow(clippy::result_large_err)]
pub fn link(
    state: &AppState,
    broker_id: &str,
    peer_uid: Option<i64>,
    req: &LinkChannelAccountRequest,
) -> Result<LinkChannelAccountResponse, tonic::Status> {
    let proven = prove_channel_broker(state, broker_id, peer_uid).map_err(|r| {
        count("link_channel_account", "not_permitted");
        status_of(r)
    })?;
    let answer = |outcome: LinkOutcome, contact_id: String| {
        count(
            "link_channel_account",
            &outcome.as_str_name().to_ascii_lowercase(),
        );
        Ok(LinkChannelAccountResponse {
            outcome: outcome as i32,
            contact_id,
        })
    };
    // 5. The node setting enables the channel.
    match state.store.notify_channel_enabled(&proven.channel) {
        Ok(true) => {}
        Ok(false) | Err(_) => return answer(LinkOutcome::Unavailable, String::new()),
    }
    // 6. A well-formed code and account.
    let code = req.code.trim();
    let account = req
        .external_account
        .as_ref()
        .and_then(|a| account_of(&a.scope, &a.id));
    let (Some(account), true) = (account, !code.is_empty() && code.len() <= MAX_CODE_BYTES) else {
        return answer(LinkOutcome::InvalidCode, String::new());
    };
    let _display = req
        .display
        .chars()
        .take(MAX_DISPLAY_BYTES)
        .collect::<String>();
    // 7. One workspace per node: the team of an account already linked wins.
    let scope = account.split('/').next().unwrap_or_default();
    match state.store.linked_scope(&proven.channel) {
        Ok(Some(linked)) if linked != scope => {
            return answer(LinkOutcome::WrongWorkspace, String::new())
        }
        Ok(_) => {}
        Err(_) => return answer(LinkOutcome::Unavailable, String::new()),
    }
    // 8. The code proves the person; the store maps its outcomes.
    match state.store.verify_contact_with_code(code, &account) {
        Ok(Verified::Verified { contact_id, .. }) => answer(LinkOutcome::Linked, contact_id),
        Ok(Verified::Expired) => answer(LinkOutcome::Expired, String::new()),
        Ok(Verified::TooManyAttempts) => answer(LinkOutcome::TooManyAttempts, String::new()),
        Ok(Verified::AccountTaken) => answer(LinkOutcome::AlreadyLinked, String::new()),
        // A spent code, a code for another account and an unknown code look alike to the caller.
        Ok(Verified::Invalid | Verified::AlreadyUsed | Verified::WrongAccount) => {
            answer(LinkOutcome::InvalidCode, String::new())
        }
        Err(OrchestratorError::InvalidContract(_)) => {
            answer(LinkOutcome::InvalidCode, String::new())
        }
        Err(_) => answer(LinkOutcome::Unavailable, String::new()),
    }
}

impl crate::workflow::SqliteWorkflowStore {
    /// The workspace (`scope` of `scope/id`) of the verified, unrevoked contacts on `channel`, if
    /// any. A node has one.
    pub fn linked_scope(&self, channel: &str) -> Result<Option<String>, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let address: Option<String> = rusqlite::OptionalExtension::optional(connection.query_row(
            "SELECT address FROM contacts
             WHERE channel = ?1 AND verified_at IS NOT NULL AND revoked_at IS NULL
             ORDER BY verified_at LIMIT 1",
            [channel],
            |r| r.get(0),
        ))?;
        Ok(address.and_then(|a| a.split('/').next().map(str::to_string)))
    }
}

// ---- GetChannelConfig ------------------------------------------------------------------------

/// `GetChannelConfigRequest.purpose` of one reply to a person: the bot token only, nothing else
/// changes. Anything else (the default, `connect`) is a start or reconnect.
pub const PURPOSE_REPLY: &str = "reply";
/// The purpose of a start or reconnect.
pub const PURPOSE_CONNECT: &str = "connect";
/// Key of the bot token in `GetChannelConfigResponse.resolved_secrets`.
pub const BOT_TOKEN_KEY: &str = crate::notify_delivery::BOT_TOKEN_KEY;

/// What the broker needs: non-secret settings, and the adapter's tokens read from the vault now.
/// Nothing is cached or logged. The broker may ask for the bot token once per reply and must drop
/// it when the reply is sent.
///
/// - `connect` (or empty): the app-level token and the bot token. The broker holds its action
///   handles in memory only, so a start or reconnect means they are gone: this revokes the broker's
///   open handles and queues, for every decision already sent and still waiting, an edit of the
///   sent message carrying the actions and a new handle (`notify_delivery::requeue_after_broker_start`).
/// - `reply`: the bot token only. No handle is touched.
#[allow(clippy::result_large_err)]
pub fn channel_config(
    state: &AppState,
    broker_id: &str,
    peer_uid: Option<i64>,
    req: &GetChannelConfigRequest,
) -> Result<GetChannelConfigResponse, tonic::Status> {
    let proven = prove_channel_broker(state, broker_id, peer_uid).map_err(|r| {
        count("get_channel_config", "not_permitted");
        status_of(r)
    })?;
    // 5. Only the channel the manifest declares.
    if req.channel != proven.channel {
        count("get_channel_config", "not_permitted");
        return Err(tonic::Status::permission_denied(
            "the broker does not declare that channel",
        ));
    }
    let reply = match req.purpose.as_str() {
        "" | PURPOSE_CONNECT => false,
        PURPOSE_REPLY => true,
        _ => {
            count("get_channel_config", "invalid");
            return Err(tonic::Status::invalid_argument("unknown purpose"));
        }
    };
    // 6. Only when the node setting enables it.
    let settings = state
        .store
        .node_notify_settings()
        .map(|(s, _)| s)
        .map_err(|_| {
            count("get_channel_config", "unavailable");
            tonic::Status::unavailable("try again")
        })?;
    if !settings.channel_enabled(&proven.channel) {
        count("get_channel_config", "disabled");
        return Ok(GetChannelConfigResponse {
            enabled: false,
            ..Default::default()
        });
    }
    // 7. The secrets, per call.
    let (app_ref, bot_ref) = match proven.channel.as_str() {
        "slack" => (
            settings.slack.app_token_ref.clone(),
            settings.slack.bot_token_ref.clone(),
        ),
        _ => (None, None),
    };
    let mut secrets = std::collections::HashMap::new();
    let wanted = [
        (APP_TOKEN_KEY, app_ref.filter(|_| !reply)),
        (BOT_TOKEN_KEY, bot_ref),
    ];
    for (key, secret_ref) in wanted {
        let Some(r) = secret_ref else { continue };
        match state.vault.decrypt_secret_not_connection(&r) {
            Ok(v) => {
                secrets.insert(key.to_string(), v.expose_secret().clone());
            }
            Err(OrchestratorError::PermissionDenied(_)) => {
                count("get_channel_config", "secret_not_permitted");
                return Err(tonic::Status::permission_denied(
                    "the adapter's secret is not permitted",
                ));
            }
            Err(_) => {
                count("get_channel_config", "secret_unavailable");
                return Err(tonic::Status::unavailable(
                    "the adapter's secret is not readable",
                ));
            }
        }
    }
    let mut out = std::collections::HashMap::new();
    out.insert("surface".to_string(), proven.surface.clone());
    if let Some(u) = &settings.app_url {
        out.insert("app_url".to_string(), u.clone());
    }
    if !reply {
        // The broker's memory of its handles is gone: revoke them and queue fresh ones.
        let requeued = state
            .store
            .connection
            .lock()
            .map_err(|_| tonic::Status::unavailable("try again"))
            .and_then(|conn| {
                let tx = conn
                    .unchecked_transaction()
                    .map_err(|_| tonic::Status::unavailable("try again"))?;
                let n = crate::notify_delivery::requeue_after_broker_start(
                    &tx,
                    broker_id,
                    &proven.channel,
                    crate::catalog::unix_timestamp(),
                )
                .map_err(|_| tonic::Status::unavailable("try again"))?;
                tx.commit()
                    .map_err(|_| tonic::Status::unavailable("try again"))?;
                Ok(n)
            })
            .inspect_err(|_| count("get_channel_config", "unavailable"))?;
        tracing::info!(broker = broker_id, channel = %proven.channel, requeued, "channel config served");
    }
    count("get_channel_config", if reply { "reply" } else { "ok" });
    Ok(GetChannelConfigResponse {
        enabled: true,
        settings: out,
        resolved_secrets: secrets,
    })
}

#[cfg(test)]
mod tests;
