//! The trigger gateway (E5, `design/hsp-triggers.md` amendment 2026-10-29 section 5).
//!
//! One Unix socket per approved broker that declares `run_request_permission`. The broker is the
//! socket a call arrived on: no request field names it. A call carries a grant token; the
//! checklist below turns it into a typed answer, and a fire goes through the shared
//! `start_run` as the grant's owner. Nothing here reads a broker id from a request.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use prost_types::Struct;
use sha2::{Digest as _, Sha256};
use vam_spec::v1::hosted_trigger_gateway_server::{
    HostedTriggerGateway, HostedTriggerGatewayServer,
};
use vam_spec::v1::{
    BusyReason, CreateRunRequest, DenyReason, ErrorStatus, RequestRunTriggerRequest,
    RequestRunTriggerResponse, ResourceId, ResourceRef, RevokeRunGrantRequest,
    RevokeRunGrantResponse, RunSpec, RunTriggerOutcome, TrustTier,
};

use crate::api::run_start::{
    start_run, BusyKind, RunCaller, RunSource, StartError, StartOpts, StartOutcome,
};
use crate::api::AppState;
use crate::credentials::CredentialError;
use crate::error::OrchestratorError;
use crate::grants::GrantConstraints;
use crate::users::UserRole;

/// `vam_hsp_trigger_total{broker_id, result}`.
pub const METRIC_HSP_TRIGGER_TOTAL: &str = "vam_hsp_trigger_total";
/// Env override of the directory the sockets live in.
pub const ENV_GATEWAY_DIR: &str = "VAMD_HSP_GATEWAY_DIR";
/// `sun_path` holds 108 bytes including the NUL.
const MAX_SOCKET_PATH_BYTES: usize = 107;
/// Longest idempotency key (`RequestRunTriggerRequest`).
const MAX_KEY_BYTES: usize = 128;
/// Scope the owner needs today for the run to start (what `create_run`'s PDP check requires).
const RUN_SCOPE: &str = "runs:write";
const BUSY_RETRY_MS: i32 = 2000;
const UNAVAILABLE_RETRY_MS: i32 = 5000;
/// Why a discarded fire ends.
pub const GRANT_REVOKED_MESSAGE: &str = "the schedule's grant was revoked";

// ---- hub: listeners and rate windows ------------------------------------------------------

struct Listener {
    path: PathBuf,
    task: tokio::task::JoinHandle<()>,
}

/// Per-broker request windows, in memory (a restart resets them).
#[derive(Default)]
struct Window {
    minute: VecDeque<Instant>,
    day: VecDeque<Instant>,
}

impl Window {
    /// Takes one request or says how long until one is free.
    fn take(&mut self, now: Instant, per_minute: usize, per_day: usize) -> Result<(), Duration> {
        let minute = Duration::from_secs(60);
        let day = Duration::from_secs(86_400);
        while self
            .minute
            .front()
            .is_some_and(|t| now.duration_since(*t) >= minute)
        {
            self.minute.pop_front();
        }
        while self
            .day
            .front()
            .is_some_and(|t| now.duration_since(*t) >= day)
        {
            self.day.pop_front();
        }
        if let (true, Some(oldest)) = (self.minute.len() >= per_minute, self.minute.front()) {
            return Err(minute.saturating_sub(now.duration_since(*oldest)));
        }
        if let (true, Some(oldest)) = (self.day.len() >= per_day, self.day.front()) {
            return Err(day.saturating_sub(now.duration_since(*oldest)));
        }
        self.minute.push_back(now);
        self.day.push_back(now);
        Ok(())
    }
}

/// Open gateway sockets and request windows of one store.
#[derive(Default)]
pub struct Hub {
    dir: Mutex<Option<PathBuf>>,
    listeners: Mutex<HashMap<String, Listener>>,
    windows: Mutex<HashMap<String, Window>>,
}

impl Hub {
    /// Where sockets go when the env var is not set (a test, or a store with no file).
    pub fn set_dir(&self, dir: PathBuf) {
        if let Ok(mut d) = self.dir.lock() {
            *d = Some(dir);
        }
    }

    /// Whether a socket is open for `broker_id`.
    pub fn is_open(&self, broker_id: &str) -> bool {
        self.listeners
            .lock()
            .map(|l| l.contains_key(broker_id))
            .unwrap_or(false)
    }
}

fn gateway_dir(store: &crate::workflow::SqliteWorkflowStore) -> Result<PathBuf, OrchestratorError> {
    if let Some(dir) = std::env::var_os(ENV_GATEWAY_DIR).filter(|d| !d.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    if let Some(dir) = store.gateways.dir.lock().ok().and_then(|d| d.clone()) {
        return Ok(dir);
    }
    let db = {
        let connection = store
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        connection
            .path()
            .filter(|p| !p.is_empty())
            .map(PathBuf::from)
    };
    let db = db.ok_or_else(|| {
        OrchestratorError::InvalidContract(format!(
            "the trigger gateway has no directory: the store is not a file; set {ENV_GATEWAY_DIR}"
        ))
    })?;
    let parent = db.parent().map(Path::to_path_buf).unwrap_or_default();
    Ok(parent.join("hsp").join("gw"))
}

/// `<dir>/<first 12 hex of sha256(broker_id)>.sock`.
pub fn socket_path(dir: &Path, broker_id: &str) -> Result<PathBuf, OrchestratorError> {
    let hex = format!("{:x}", Sha256::digest(broker_id.as_bytes()));
    let path = dir.join(format!("{}.sock", &hex[..12]));
    let len = path.as_os_str().len();
    if len > MAX_SOCKET_PATH_BYTES {
        return Err(OrchestratorError::InvalidContract(format!(
            "the gateway socket path is {len} bytes (limit {MAX_SOCKET_PATH_BYTES}); \
             set {ENV_GATEWAY_DIR} to a shorter directory"
        )));
    }
    Ok(path)
}

#[cfg(unix)]
fn make_private_dir(dir: &Path) -> Result<(), OrchestratorError> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(OrchestratorError::Io)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
        .map_err(OrchestratorError::Io)
}

/// Opens or closes the broker's socket to match its approval, and records the path. Returns the
/// open path. Call after every change of an approval and at start.
pub async fn sync_broker(
    state: &AppState,
    broker_id: &str,
) -> Result<Option<PathBuf>, OrchestratorError> {
    let store = &state.store;
    let wanted = store
        .get_hsp_broker_approval(broker_id)?
        .is_some_and(|a| a.enabled && wants_gateway(&a.manifest));
    if !wanted {
        close_broker(state, broker_id)?;
        return Ok(None);
    }
    let dir = gateway_dir(store)?;
    let path = socket_path(&dir, broker_id)?;
    {
        let listeners = store
            .gateways
            .listeners
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        if listeners.get(broker_id).is_some_and(|l| l.path == path) {
            store.set_hsp_broker_gateway_socket(broker_id, Some(&path))?;
            return Ok(Some(path));
        }
    }
    close_broker(state, broker_id)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        make_private_dir(&dir)?;
        let _ = std::fs::remove_file(&path);
        let listener = tokio::net::UnixListener::bind(&path).map_err(OrchestratorError::Io)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .map_err(OrchestratorError::Io)?;
        let service = Gateway {
            state: state.clone(),
            broker_id: broker_id.to_string(),
        };
        let (state_for_notify, id_for_notify) = (state.clone(), broker_id.to_string());
        let incoming = tokio_stream::wrappers::UnixListenerStream::new(listener);
        let id = broker_id.to_string();
        let task = tokio::spawn(async move {
            let served = tonic::transport::Server::builder()
                .add_service(HostedTriggerGatewayServer::new(service))
                .add_service(crate::notify_gateway::service(
                    state_for_notify,
                    id_for_notify,
                ))
                .serve_with_incoming(incoming)
                .await;
            if let Err(err) = served {
                tracing::warn!("trigger gateway of broker {id} stopped: {err}");
            }
        });
        store
            .gateways
            .listeners
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?
            .insert(
                broker_id.to_string(),
                Listener {
                    path: path.clone(),
                    task,
                },
            );
    }
    store.set_hsp_broker_gateway_socket(broker_id, Some(&path))?;
    tracing::info!(broker = broker_id, "trigger gateway socket open");
    Ok(Some(path))
}

/// Whether an approved manifest gets a gateway socket: it may request runs, or it is a channel
/// broker (a `notification_channel` block on a core or first-party tier, checked again here
/// whatever the registration said). Each service on the socket checks its own permission, so a
/// broker with one permission is `NOT_PERMITTED` on the other service.
pub(crate) fn wants_gateway(manifest: &vam_spec::v1::CapabilityManifest) -> bool {
    manifest.run_request_permission.is_some()
        || (manifest.notification_channel.is_some()
            && crate::notify_gateway::tier_may_deliver(manifest.trust_tier))
}

/// Closes the broker's socket (if open) and clears the recorded path.
pub fn close_broker(state: &AppState, broker_id: &str) -> Result<(), OrchestratorError> {
    let listener = state
        .store
        .gateways
        .listeners
        .lock()
        .map_err(|_| OrchestratorError::StorageUnavailable)?
        .remove(broker_id);
    if let Some(l) = listener {
        l.task.abort();
        let _ = std::fs::remove_file(&l.path);
        tracing::info!(broker = broker_id, "trigger gateway socket closed");
    }
    state.store.set_hsp_broker_gateway_socket(broker_id, None)?;
    Ok(())
}

/// Opens the socket of every enabled approval that declares the permission (vamd start).
pub async fn sync_all(state: &AppState) {
    let ids = state
        .store
        .list_enabled_hsp_broker_ids()
        .unwrap_or_default();
    for id in ids {
        if let Err(err) = sync_broker(state, &id).await {
            tracing::warn!("trigger gateway of broker {id} not opened: {err}");
        }
    }
}

// ---- the service --------------------------------------------------------------------------

#[derive(Clone)]
struct Gateway {
    state: AppState,
    broker_id: String,
}

#[tonic::async_trait]
impl HostedTriggerGateway for Gateway {
    async fn request_run_trigger(
        &self,
        request: tonic::Request<RequestRunTriggerRequest>,
    ) -> Result<tonic::Response<RequestRunTriggerResponse>, tonic::Status> {
        let peer_uid = peer_uid_of(&request);
        let answer = fire(&self.state, &self.broker_id, peer_uid, request.into_inner()).await;
        Ok(tonic::Response::new(answer))
    }

    async fn revoke_run_grant(
        &self,
        request: tonic::Request<RevokeRunGrantRequest>,
    ) -> Result<tonic::Response<RevokeRunGrantResponse>, tonic::Status> {
        let peer_uid = peer_uid_of(&request);
        let answer = revoke(&self.state, &self.broker_id, peer_uid, request.into_inner());
        Ok(tonic::Response::new(answer))
    }
}

#[cfg(unix)]
pub(crate) fn peer_uid_of<T>(request: &tonic::Request<T>) -> Option<i64> {
    request
        .extensions()
        .get::<tonic::transport::server::UdsConnectInfo>()
        .and_then(|i| i.peer_cred)
        .map(|c| i64::from(c.uid()))
}

// ---- answers ------------------------------------------------------------------------------

fn status(code: i32, message: impl Into<String>, retryable: bool) -> ErrorStatus {
    ErrorStatus {
        code,
        message: message.into(),
        details: vec![],
        retryable,
    }
}

fn denied(reason: DenyReason, detail: &str) -> RequestRunTriggerResponse {
    RequestRunTriggerResponse {
        outcome: RunTriggerOutcome::Denied as i32,
        deny_reason: reason as i32,
        error: Some(status(7, detail, false)),
        ..Default::default()
    }
}

fn busy(reason: BusyReason, retry_after_ms: i32, detail: &str) -> RequestRunTriggerResponse {
    RequestRunTriggerResponse {
        outcome: RunTriggerOutcome::Busy as i32,
        busy_reason: reason as i32,
        retry_after_ms,
        error: Some(status(9, detail, retry_after_ms > 0)),
        ..Default::default()
    }
}

fn unavailable(detail: &str) -> RequestRunTriggerResponse {
    RequestRunTriggerResponse {
        outcome: RunTriggerOutcome::Unavailable as i32,
        retry_after_ms: UNAVAILABLE_RETRY_MS,
        error: Some(status(14, detail, true)),
        ..Default::default()
    }
}

/// The metric label of an answer.
fn result_label(r: &RequestRunTriggerResponse) -> String {
    let word = |s: &str, prefix: &str| s.strip_prefix(prefix).unwrap_or(s).to_ascii_lowercase();
    let outcome = RunTriggerOutcome::try_from(r.outcome)
        .map(|o| word(o.as_str_name(), "RUN_TRIGGER_OUTCOME_"))
        .unwrap_or_else(|_| "unspecified".into());
    match RunTriggerOutcome::try_from(r.outcome) {
        Ok(RunTriggerOutcome::Denied) => DenyReason::try_from(r.deny_reason)
            .map(|d| format!("denied_{}", word(d.as_str_name(), "DENY_REASON_")))
            .unwrap_or(outcome),
        Ok(RunTriggerOutcome::Busy) => BusyReason::try_from(r.busy_reason)
            .map(|b| format!("busy_{}", word(b.as_str_name(), "BUSY_REASON_")))
            .unwrap_or(outcome),
        _ => outcome,
    }
}

fn record(broker_id: &str, grant_id: &str, answer: &RequestRunTriggerResponse) {
    let label = result_label(answer);
    let _ = vam_telemetry::VamMetricsRegistry::global().record_counter(
        METRIC_HSP_TRIGGER_TOTAL,
        1,
        &[("broker_id", broker_id), ("result", label.as_str())],
    );
    let refused = !matches!(
        RunTriggerOutcome::try_from(answer.outcome),
        Ok(RunTriggerOutcome::Accepted | RunTriggerOutcome::Queued | RunTriggerOutcome::Duplicate)
    );
    if refused {
        // The grant id and the reason; never the token.
        tracing::info!(
            broker = broker_id,
            grant = grant_id,
            reason = label.as_str(),
            "trigger refused"
        );
    }
}

// ---- checks 1 to 3 ------------------------------------------------------------------------

struct Proven {
    credential: crate::credentials::CheckedCredential,
    permission: vam_spec::v1::RunRequestPermission,
}

/// What the grant id is when it cannot be trusted (a log field).
fn claimed_grant(token: &str) -> String {
    crate::credentials::parse_grant_token(token)
        .map(|(id, _)| id.to_string())
        .unwrap_or_default()
}

#[allow(clippy::result_large_err)]
/// Steps 1 to 4 and 6: the socket, the token and the owner. `Err` is the answer.
fn prove(
    state: &AppState,
    broker_id: &str,
    peer_uid: Option<i64>,
    token: &str,
) -> Result<Proven, RequestRunTriggerResponse> {
    let store = &state.store;
    // 1. Socket -> broker; approval enabled; tier; permission; peer uid.
    let not_permitted = |why: &str| denied(DenyReason::NotPermitted, why);
    let approval = match store.get_hsp_broker_approval(broker_id) {
        Ok(Some(a)) if a.enabled => a,
        Ok(_) => return Err(not_permitted("the broker is not approved")),
        Err(_) => return Err(unavailable("the approval could not be read")),
    };
    if !tier_may_request_runs(approval.manifest.trust_tier) {
        return Err(not_permitted(
            "the broker's trust tier may not request runs",
        ));
    }
    let Some(permission) = approval.manifest.run_request_permission.clone() else {
        return Err(not_permitted("the broker has no run request permission"));
    };
    match store.get_hsp_broker_gateway(broker_id) {
        Ok((_, Some(want))) if peer_uid != Some(want) => {
            return Err(not_permitted("the caller is not the broker's user"));
        }
        Ok(_) => {}
        Err(_) => return Err(unavailable("the approval could not be read")),
    }
    // 2 to 4. Token, row, holder, revoked, expired.
    match store.check_grant_token(token, broker_id) {
        Ok(credential) => Ok(Proven {
            credential,
            permission,
        }),
        Err(CredentialError::Unknown) => Err(denied(DenyReason::InvalidRequest, "unknown grant")),
        Err(CredentialError::WrongTransport) => Err(denied(
            DenyReason::WrongBroker,
            "the grant belongs to another broker",
        )),
        Err(CredentialError::Revoked { owner_user_id }) => {
            // A grant revoked because its user was deactivated says so.
            if owner_disabled(store, &owner_user_id) {
                Err(denied(
                    DenyReason::OwnerDisabled,
                    "the grant's user is disabled",
                ))
            } else {
                Err(denied(DenyReason::Revoked, "the grant was revoked"))
            }
        }
        Err(CredentialError::Expired) => Err(denied(DenyReason::Expired, "the grant expired")),
        Err(CredentialError::Storage(_)) => Err(unavailable("the grant could not be read")),
    }
}

fn owner_disabled(store: &crate::workflow::SqliteWorkflowStore, owner: &str) -> bool {
    matches!(store.get_user(owner), Ok(u) if u.is_deactivated())
}

#[allow(clippy::result_large_err)]
/// The owner's current role (a missing user row reads as a member, as the queue starter does).
fn owner_role(
    store: &crate::workflow::SqliteWorkflowStore,
    owner: &str,
) -> Result<UserRole, RequestRunTriggerResponse> {
    match store.get_user(owner) {
        Ok(user) if user.is_deactivated() => Err(denied(
            DenyReason::OwnerDisabled,
            "the grant's user is disabled",
        )),
        Ok(user) => Ok(user.role),
        Err(OrchestratorError::NotFound(_)) => Ok(UserRole::Member),
        Err(_) => Err(unavailable("the owner could not be read")),
    }
}

/// Only core, first-party and operator brokers may request runs; an unspecified or third-party
/// tier may not (a stored manifest from before validation could carry either).
pub fn tier_may_request_runs(trust_tier: i32) -> bool {
    matches!(
        TrustTier::try_from(trust_tier),
        Ok(TrustTier::Core | TrustTier::FirstParty | TrustTier::Operator)
    )
}

/// Whether `scopes` let a user create a run.
pub fn scopes_allow_run(scopes: &[String]) -> bool {
    scopes.iter().any(|s| s == RUN_SCOPE)
}

// ---- RequestRunTrigger --------------------------------------------------------------------

/// The whole checklist (steps 1 to 13) for one call. Public for tests.
pub async fn fire(
    state: &AppState,
    broker_id: &str,
    peer_uid: Option<i64>,
    req: RequestRunTriggerRequest,
) -> RequestRunTriggerResponse {
    let grant_id = claimed_grant(&req.run_grant);
    let answer = fire_inner(state, broker_id, peer_uid, &req).await;
    record(broker_id, &grant_id, &answer);
    tell_owner(state, broker_id, &grant_id, &answer);
    answer
}

/// `trigger.fired` (queued behind a live task) and `trigger.skipped` (`BUSY SKIPPED`,
/// `TARGET_GONE`, `AGENT_UNAVAILABLE`, `EXPIRED`) for the grant owner, once per grant and UTC
/// day. A user's own revoke and a disabled owner are not told; neither is anything the
/// gateway could not tie to a grant of this broker.
fn tell_owner(
    state: &AppState,
    broker_id: &str,
    grant_id: &str,
    answer: &RequestRunTriggerResponse,
) {
    use crate::notifications::TriggerNotice;
    if grant_id.is_empty() {
        return;
    }
    let notice = match RunTriggerOutcome::try_from(answer.outcome) {
        Ok(RunTriggerOutcome::Queued) => TriggerNotice::Queued,
        Ok(RunTriggerOutcome::Busy) if answer.busy_reason == BusyReason::Skipped as i32 => {
            TriggerNotice::Skipped("the workspace was busy")
        }
        Ok(RunTriggerOutcome::Denied) => match DenyReason::try_from(answer.deny_reason) {
            Ok(DenyReason::TargetGone) => TriggerNotice::Skipped("its Space or thread is gone"),
            Ok(DenyReason::AgentUnavailable) => TriggerNotice::Skipped("its agent is unavailable"),
            Ok(DenyReason::Expired) => TriggerNotice::Skipped("the grant expired"),
            _ => return,
        },
        _ => return,
    };
    if let Err(err) = state
        .store
        .record_trigger_notice(broker_id, grant_id, notice)
    {
        tracing::warn!("trigger notification for grant {grant_id} not written: {err}");
    }
}

async fn fire_inner(
    state: &AppState,
    broker_id: &str,
    peer_uid: Option<i64>,
    req: &RequestRunTriggerRequest,
) -> RequestRunTriggerResponse {
    let store = &state.store;
    let proven = match prove(state, broker_id, peer_uid, &req.run_grant) {
        Ok(p) => p,
        Err(answer) => return answer,
    };
    let cred = &proven.credential;
    // 5. The tool is still approved to hold a grant.
    let tool = cred.tool_name.clone().unwrap_or_default();
    if !proven.permission.grant_tools.contains(&tool) {
        return denied(
            DenyReason::ToolNotApproved,
            "the grant's tool is no longer approved",
        );
    }
    // 6. The owner exists, is active, and may still start runs.
    let role = match owner_role(store, &cred.owner_user_id) {
        Ok(role) => role,
        Err(answer) => return answer,
    };
    let scopes = crate::keys::default_scopes_for_role(role);
    if !scopes_allow_run(&scopes) {
        return denied(
            DenyReason::NotPermitted,
            "the grant's user may no longer start runs",
        );
    }
    // The request itself.
    if req.idempotency_key.is_empty()
        || req.idempotency_key.len() > MAX_KEY_BYTES
        || req.idempotency_key.chars().any(char::is_control)
    {
        return denied(
            DenyReason::InvalidRequest,
            "the idempotency key is not valid",
        );
    }
    // 7. The trigger ref is pinned on the first accepted fire.
    if let Some(pinned) = cred.external_ref.as_deref().filter(|p| !p.is_empty()) {
        if req.trigger_ref != pinned {
            return denied(
                DenyReason::TriggerRefMismatch,
                "the trigger ref differs from the one pinned to this grant",
            );
        }
    }
    // 8. Rate limit.
    let (per_minute, per_day, _) =
        vam_spec::validation::effective_run_request_limits(&proven.permission);
    {
        let mut windows = match store.gateways.windows.lock() {
            Ok(w) => w,
            Err(_) => return unavailable("the rate limiter is unavailable"),
        };
        let window = windows.entry(broker_id.to_string()).or_default();
        if let Err(wait) = window.take(
            Instant::now(),
            per_minute.max(1) as usize,
            per_day.max(1) as usize,
        ) {
            return RequestRunTriggerResponse {
                outcome: RunTriggerOutcome::RateLimited as i32,
                retry_after_ms: wait.as_millis().min(i32::MAX as u128) as i32,
                error: Some(status(8, "the broker's request limit is reached", true)),
                ..Default::default()
            };
        }
    }
    // 9. Dedupe on the spec key, per owner (covers started and queued runs). The key names the
    //    grant too, so two grants of one broker never share a key.
    let spec_key = format!("hsp:{broker_id}:{}:{}", cred.key_id, req.idempotency_key);
    let constraints: GrantConstraints = match cred
        .constraints_json
        .as_deref()
        .and_then(|j| serde_json::from_str(j).ok())
    {
        Some(c) => c,
        None => return denied(DenyReason::InvalidRequest, "the grant carries no request"),
    };
    match store.find_run_by_idempotency_key(&cred.owner_user_id, &spec_key) {
        Ok(Some(run_id)) => {
            return RequestRunTriggerResponse {
                outcome: RunTriggerOutcome::Duplicate as i32,
                run_id: Some(ResourceId { urn: run_id }),
                space_id: constraints.space_id.clone(),
                thread_id: constraints.thread_id.clone(),
                ..Default::default()
            };
        }
        Ok(None) => {}
        Err(_) => return unavailable("the run keys could not be read"),
    }
    // 10. The Space and the thread still exist and belong to the owner.
    let gone = |e: OrchestratorError| match e {
        OrchestratorError::NotFound(_) => denied(
            DenyReason::TargetGone,
            "the grant's Space or thread is gone",
        ),
        _ => unavailable("the Space could not be read"),
    };
    if let Err(e) = store.get_space(&cred.owner_user_id, &constraints.space_id) {
        return gone(e);
    }
    if let Err(e) = store.get_thread(
        &cred.owner_user_id,
        &constraints.space_id,
        &constraints.thread_id,
    ) {
        return gone(e);
    }
    // 11 to 13. Start through the shared path.
    let create = match build_request(&constraints, &spec_key) {
        Ok(c) => c,
        Err(why) => return denied(DenyReason::InvalidRequest, &why),
    };
    let principal = store
        .run_owner_principal(cred.source_run_id.as_deref())
        .unwrap_or_else(|| cred.owner_user_id.clone());
    let source = RunSource::Trigger {
        broker_id: broker_id.to_string(),
        grant_id: cred.key_id.clone(),
        trigger_ref: req.trigger_ref.clone(),
        fire_at: req.fire_at.as_ref().map(|t| t.seconds).unwrap_or(0),
        fire_kind: fire_kind_word(req.kind).to_string(),
        surface: proven.permission.surface.clone(),
    };
    let caller = RunCaller {
        owner_user_id: cred.owner_user_id.clone(),
        principal,
        role,
        users_admin: scopes.iter().any(|s| s == crate::keys::USERS_ADMIN_SCOPE),
        scopes,
        credential_id: Some(cred.key_id.clone()),
        source,
        not_after: req.not_after.as_ref().map(|t| t.seconds),
        key_id: cred.key_id.clone(),
        name: format!("grant {}", cred.key_id),
        session_token: false,
        key_owner_user_id: cred.owner_user_id.clone(),
    };
    let opts = StartOpts {
        skip_if_busy: constraints.on_busy == "SKIP",
        idempotency_key: spec_key.clone(),
        traceparent: None,
    };
    let answer = match start_run(state, &caller, create, opts).await {
        Ok(StartOutcome::Started {
            run_id,
            workspace_id,
            thread_id,
            ..
        }) => RequestRunTriggerResponse {
            outcome: RunTriggerOutcome::Accepted as i32,
            run_id: Some(ResourceId { urn: run_id }),
            space_id: workspace_id,
            thread_id,
            ..Default::default()
        },
        Ok(StartOutcome::Queued {
            receipt,
            workspace_id,
        }) => RequestRunTriggerResponse {
            outcome: RunTriggerOutcome::Queued as i32,
            run_id: receipt.run_id,
            space_id: workspace_id,
            thread_id: constraints.thread_id.clone(),
            ..Default::default()
        },
        Ok(StartOutcome::Existing { run_id }) => RequestRunTriggerResponse {
            outcome: RunTriggerOutcome::Duplicate as i32,
            run_id: Some(ResourceId { urn: run_id }),
            space_id: constraints.space_id.clone(),
            thread_id: constraints.thread_id.clone(),
            ..Default::default()
        },
        // A trigger never steers; if it ever did, the fire did not start a run.
        Ok(StartOutcome::Steered { .. }) => {
            return unavailable("the workspace took the fire as a message")
        }
        Err(StartError::Busy { kind, .. }) => match kind {
            BusyKind::Skipped => busy(
                BusyReason::Skipped,
                0,
                "the workspace is busy and the grant skips",
            ),
            BusyKind::AlreadyQueued => busy(
                BusyReason::AlreadyQueued,
                0,
                "a fire from this grant is already queued",
            ),
            BusyKind::Live | BusyKind::Parked | BusyKind::Finalizing | BusyKind::Stopping => busy(
                BusyReason::ActiveRun,
                BUSY_RETRY_MS,
                "the task is ending; retry shortly",
            ),
        },
        Err(StartError::Api(err)) => map_api_error(err),
    };
    // 13. Pin the ref, stamp the event.
    if matches!(
        RunTriggerOutcome::try_from(answer.outcome),
        Ok(RunTriggerOutcome::Accepted | RunTriggerOutcome::Queued)
    ) {
        if !req.trigger_ref.is_empty() {
            if let Err(err) = store.pin_grant_external_ref(&cred.key_id, &req.trigger_ref) {
                tracing::warn!("grant {} ref not pinned: {err}", cred.key_id);
            }
        }
        if let Some(run) = &answer.run_id {
            let payload = serde_json::json!({
                "grantId": cred.key_id,
                "triggerRef": req.trigger_ref,
                "fireKind": fire_kind_word(req.kind),
            });
            if let Err(err) = store.append_outbox_event(
                &run.urn,
                crate::events::TYPE_TRIGGER_FIRED,
                &payload.to_string(),
            ) {
                tracing::warn!("trigger.fired write failed for {}: {err}", run.urn);
            }
        }
    }
    answer
}

fn map_api_error(err: crate::api::error::ApiError) -> RequestRunTriggerResponse {
    use axum::http::StatusCode;
    match err.status {
        StatusCode::NOT_FOUND | StatusCode::FORBIDDEN => denied(
            DenyReason::AgentUnavailable,
            "the grant's agent or playbook is not available",
        ),
        StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY | StatusCode::CONFLICT
            if !err.retryable =>
        {
            denied(
                DenyReason::InvalidRequest,
                "the grant's request can no longer be started",
            )
        }
        _ => unavailable("the node cannot start the run now"),
    }
}

fn fire_kind_word(kind: i32) -> &'static str {
    match kind {
        2 => "run_now",
        3 => "event",
        _ => "scheduled",
    }
}

fn parse_version(raw: &str) -> Option<vam_spec::v1::Version> {
    let (core, build) = raw.split_once('+').unwrap_or((raw, ""));
    let (nums, pre) = core.split_once('-').unwrap_or((core, ""));
    let mut it = nums.split('.');
    Some(vam_spec::v1::Version {
        major: it.next()?.parse().ok()?,
        minor: it.next()?.parse().ok()?,
        patch: it.next()?.parse().ok()?,
        prerelease: pre.to_string(),
        build: build.to_string(),
    })
}

/// The bound request of a grant as a `CreateRunRequest`.
fn build_request(c: &GrantConstraints, spec_key: &str) -> Result<CreateRunRequest, String> {
    let mut inputs = match &c.inputs {
        serde_json::Value::Object(map) => map.clone(),
        _ => serde_json::Map::new(),
    };
    if !c.message.is_empty() {
        inputs.insert("prompt".into(), c.message.clone().into());
    }
    let inputs: Struct = vam_spec::encoding::json_to_proto(
        "google.protobuf.Struct",
        &serde_json::Value::Object(inputs).to_string(),
    )
    .map_err(|e| format!("inputs: {e}"))?;
    let playbook_ref = match &c.playbook_ref {
        Some(pin) => Some(ResourceRef {
            resource_id: Some(ResourceId {
                urn: pin.urn.clone(),
            }),
            version: Some(parse_version(&pin.version).ok_or("playbook version")?),
            digest: Some(vam_spec::v1::Digest {
                algorithm: vam_spec::v1::digest::Algorithm::Sha256 as i32,
                hex_hash: pin.digest.trim_start_matches("sha256:").to_string(),
            }),
            locators: vec![],
        }),
        None => None,
    };
    let spec = RunSpec {
        agent_ref: Some(ResourceRef {
            resource_id: Some(ResourceId {
                urn: c.agent_urn.clone(),
            }),
            ..Default::default()
        }),
        space_id: c.space_id.clone(),
        thread_id: c.thread_id.clone(),
        inputs: Some(inputs),
        playbook_ref,
        idempotency_key: spec_key.to_string(),
        ..Default::default()
    };
    Ok(CreateRunRequest {
        spec: Some(spec),
        ..Default::default()
    })
}

// ---- RevokeRunGrant -----------------------------------------------------------------------

/// Proves possession (steps 1 to 3), revokes, and ends a queued fire of the grant. Idempotent: a
/// grant that is already revoked answers success to the broker that holds it.
pub fn revoke(
    state: &AppState,
    broker_id: &str,
    peer_uid: Option<i64>,
    req: RevokeRunGrantRequest,
) -> RevokeRunGrantResponse {
    let grant_id = claimed_grant(&req.run_grant);
    let fail = |answer: RequestRunTriggerResponse| {
        record(broker_id, &grant_id, &answer);
        RevokeRunGrantResponse {
            error: answer.error.or_else(|| Some(status(7, "refused", false))),
        }
    };
    let store = &state.store;
    // (key id, owner, still live)
    let (key_id, owner, live) = match prove(state, broker_id, peer_uid, &req.run_grant) {
        Ok(p) => (p.credential.key_id, p.credential.owner_user_id, true),
        // Already revoked (or expired) and the token proves possession: nothing to revoke.
        Err(answer)
            if matches!(
                DenyReason::try_from(answer.deny_reason),
                Ok(DenyReason::Revoked | DenyReason::Expired | DenyReason::OwnerDisabled)
            ) =>
        {
            match store.grant_possession(&req.run_grant, broker_id) {
                Some((key_id, owner)) => (key_id, owner, false),
                None => return fail(answer),
            }
        }
        Err(answer) => return fail(answer),
    };
    if live {
        if let Err(err) = store.revoke_api_key_for_owner(&key_id, &owner) {
            tracing::warn!("grant {key_id} not revoked: {err}");
            return fail(unavailable("the grant could not be revoked"));
        }
    }
    if let Err(err) = store.discard_queued_fires(Some(&key_id), None, GRANT_REVOKED_MESSAGE) {
        tracing::warn!("queued fire of grant {key_id} not discarded: {err}");
    }
    let _ = vam_telemetry::VamMetricsRegistry::global().record_counter(
        METRIC_HSP_TRIGGER_TOTAL,
        1,
        &[("broker_id", broker_id), ("result", "revoked_by_broker")],
    );
    RevokeRunGrantResponse { error: None }
}

// ---- queue start re-check -----------------------------------------------------------------

/// Why a queued fire may not start; `None` when it may. `scopes_of` gives the owner's current
/// scopes for a role (the real one is `default_scopes_for_role`; a test narrows it).
pub fn recheck_queued_fire(
    store: &crate::workflow::SqliteWorkflowStore,
    caller: &RunCaller,
    scopes_of: &dyn Fn(UserRole) -> Vec<String>,
) -> Option<String> {
    let RunSource::Trigger { broker_id, .. } = &caller.source else {
        return None;
    };
    let revoked = || GRANT_REVOKED_MESSAGE.to_string();
    let Some(credential) = caller.credential_id.as_deref() else {
        return Some(revoked());
    };
    let key = match store.get_api_key_for_owner(credential, &caller.key_owner_user_id) {
        Ok(k) => k,
        Err(_) => return Some(revoked()),
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    if key.revoked_at.is_some() || key.expires_at.is_none_or(|at| at <= now) {
        return Some(revoked());
    }
    match store.get_hsp_broker_approval(broker_id) {
        Ok(Some(a)) if a.enabled => {}
        _ => return Some("the broker was disabled".to_string()),
    }
    let role = match store.get_user(&caller.owner_user_id) {
        Ok(user) if user.is_deactivated() => return Some(revoked()),
        Ok(user) => user.role,
        Err(OrchestratorError::NotFound(_)) => UserRole::Member,
        Err(_) => return Some("the grant's user could not be read".to_string()),
    };
    if !scopes_allow_run(&scopes_of(role)) {
        return Some("the grant's user may no longer start runs".to_string());
    }
    None
}

// ---- store helpers ------------------------------------------------------------------------

impl crate::workflow::SqliteWorkflowStore {
    /// The principal of the run that issued a grant.
    fn run_owner_principal(&self, run_id: Option<&str>) -> Option<String> {
        let run_id = run_id?;
        let connection = self.connection.lock().ok()?;
        connection
            .query_row(
                "SELECT owner_principal FROM runs WHERE run_id = ?1",
                [run_id],
                |r| r.get::<_, Option<String>>(0),
            )
            .ok()
            .flatten()
            .filter(|p| !p.is_empty())
    }

    /// Pins the broker's trigger ref to the grant; only the first call writes.
    fn pin_grant_external_ref(
        &self,
        key_id: &str,
        trigger_ref: &str,
    ) -> Result<(), OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        connection.execute(
            "UPDATE api_keys SET external_ref = ?2 WHERE key_id = ?1 AND external_ref IS NULL",
            rusqlite::params![key_id, trigger_ref],
        )?;
        Ok(())
    }

    /// (grant id, owner) of the row a token names, when its secret is a grant of this broker
    /// (live or not): used to accept a repeated revoke.
    fn grant_possession(&self, token: &str, broker_id: &str) -> Option<(String, String)> {
        let (id, secret) = crate::credentials::parse_grant_token(token)?;
        let hash = crate::keys::hash_api_key_secret(secret);
        let connection = self.connection.lock().ok()?;
        let (key_id, holder, owner): (String, Option<String>, String) = connection
            .query_row(
                "SELECT key_id, holder, owner_user_id FROM api_keys WHERE sha256_secret = ?1",
                [hash],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .ok()?;
        (key_id == id && holder.as_deref() == Some(&format!("broker:{broker_id}")))
            .then_some((key_id, owner))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_path_is_twelve_hex_and_a_long_directory_is_a_typed_error() {
        let p = socket_path(Path::new("/tmp/gw"), "echo-ts").unwrap();
        let name = p.file_name().unwrap().to_str().unwrap();
        assert_eq!(name.len(), 12 + ".sock".len());
        assert!(name[..12].chars().all(|c| c.is_ascii_hexdigit()));
        let long = format!("/tmp/{}", "d".repeat(120));
        assert!(matches!(
            socket_path(Path::new(&long), "echo-ts"),
            Err(OrchestratorError::InvalidContract(_))
        ));
    }

    #[test]
    fn only_core_first_party_and_operator_tiers_may_request_runs() {
        assert!(tier_may_request_runs(TrustTier::Core as i32));
        assert!(tier_may_request_runs(TrustTier::FirstParty as i32));
        assert!(tier_may_request_runs(TrustTier::Operator as i32));
        assert!(!tier_may_request_runs(TrustTier::Unspecified as i32));
        assert!(!tier_may_request_runs(TrustTier::ThirdParty as i32));
    }
}
