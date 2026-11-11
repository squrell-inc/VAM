//! Messages to a running task (AR-3.5, design V.3).
//!
//! Two doors, one rule. `POST /v1/runs` on the thread of a live task, and
//! `POST /v1/runs/{id}/messages` by run id, both put the text in the run's
//! inbox: a **steer** (taken in at the task's next turn boundary) or, with
//! `inboxMode: QUEUE`, a message for the next task. The text is kept only until
//! the guest has applied it (`crate::inbox`).
//!
//! `admit` is the one call `create_run` makes where it used to refuse a busy
//! workspace. A message for another thread on a busy workspace is queued (owner
//! decision D): it becomes a PENDING run of its own that the starter (`api/queue.rs`)
//! starts when the workspace is free. A run that is finalizing or settling, or being
//! stopped, keeps today's retryable answer.

use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use vam_spec::v1::run_spec::InboxMode;
use vam_spec::v1::{
    CreateRunResponse, InboxReceipt, ResourceId, RunSpec, SendRunMessageRequest,
    SendRunMessageResponse,
};

use super::auth::{require_idempotency_key, AuthContext};
use super::error::{json_response_accepted, parse_json_body, ApiError};
use super::run_start::RunCaller;
use super::{
    build_run_status, ensure_ready, find_active_session_on_workspace, json_http_response, AppState,
};
use crate::error::OrchestratorError;
use crate::inbox::{QueuedCaller, QueuedStart};
use crate::inbox_store::{NewInboxRow, NewQueuedRun, MAX_TEXT_BYTES};

/// Why a busy workspace cannot take the message as a steer or a queued message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BusyKind {
    /// A live task holds the workspace and the message could not wait (a retryable refusal).
    Live,
    /// A parked run's reservation holds the workspace.
    Parked,
    /// The live task is finalizing; retry shortly.
    Finalizing,
    /// The user is stopping the live task; retry shortly.
    Stopping,
    /// A triggered fire from this credential already waits in the queue.
    AlreadyQueued,
    /// The grant says `on_busy: SKIP` and a task holds the workspace.
    Skipped,
}

/// What `start_run` learns from asking whether the workspace is busy. The HTTP handler
/// renders each (`StartOutcome::to_http`); the trigger gateway maps them itself.
pub enum Admit {
    /// Nothing live on the workspace: start a run.
    Proceed,
    /// The message went to the live run as a steer.
    Steered {
        run_id: String,
        receipt: InboxReceipt,
    },
    /// The message waits behind the live run as a queued run (`receipt.run_id`).
    Queued { receipt: InboxReceipt },
    /// The message cannot go anywhere now; `refusal` is today's retryable answer.
    Busy {
        kind: BusyKind,
        refusal: OrchestratorError,
    },
}

/// The inputs `create_run` already holds, as the busy check needs them.
pub struct Candidate<'a> {
    pub state: &'a AppState,
    pub caller: &'a RunCaller,
    pub spec: &'a RunSpec,
    pub has_inline: bool,
    pub requested_urn: Option<&'a str>,
    pub workspace_id: &'a str,
    pub thread_ref: &'a str,
    pub skip_if_busy: bool,
}

struct Busy {
    run_id: String,
    session_id: Option<String>,
    /// Today's answer for a message that cannot go to this run.
    refusal: OrchestratorError,
}

fn busy_run(state: &AppState, workspace_id: &str) -> Result<Option<Busy>, OrchestratorError> {
    if let Some(active) = find_active_session_on_workspace(&state.store, workspace_id)? {
        let run_id = state
            .store
            .find_run_id_for_session(&active.session_id)?
            .unwrap_or_else(|| "unknown".to_string());
        return Ok(Some(Busy {
            refusal: OrchestratorError::AgentRunning {
                workspace_id: workspace_id.to_string(),
                session_id: active.session_id.clone(),
                run_id: run_id.clone(),
            },
            session_id: Some(active.session_id),
            run_id,
        }));
    }
    // A run interrupted by a crash or a restart waits for a new session. It has no session and
    // no reservation, but its workspace is not free: a new task or a fire must not start on it.
    // A message for the same thread steers it (taken in after the resume); the rest queue.
    if let Some(run_id) = state.store.resume_holding_workspace(workspace_id)? {
        return Ok(Some(Busy {
            refusal: OrchestratorError::AgentRunning {
                workspace_id: workspace_id.to_string(),
                session_id: String::new(),
                run_id: run_id.clone(),
            },
            session_id: None,
            run_id,
        }));
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    if let Some(reservation) = state
        .store
        .active_workspace_reservation(workspace_id, now)?
    {
        return Ok(Some(Busy {
            refusal: OrchestratorError::WorkspaceReserved {
                workspace_id: workspace_id.to_string(),
                run_id: reservation.run_id.clone(),
                cancel_command: crate::error::cancel_run_command(&reservation.run_id),
            },
            session_id: None,
            run_id: reservation.run_id,
        }));
    }
    Ok(None)
}

/// Whether a live session holds the workspace.
pub(super) fn workspace_taken_by_session(state: &AppState, workspace_id: &str) -> bool {
    find_active_session_on_workspace(&state.store, workspace_id)
        .map(|session| session.is_some())
        .unwrap_or(true)
}

/// Whether a task holds the workspace (a live session, or a parked run's reservation).
pub(super) fn workspace_busy(state: &AppState, workspace_id: &str) -> bool {
    busy_run(state, workspace_id).map_or(true, |busy| busy.is_some())
}

/// A run takes messages while it works or is parked, and not once it has ended, its
/// finalizer is settling the workspace, the user is stopping it, or it is only a queued
/// message waiting for its turn.
pub(super) fn run_takes_messages(
    state: &AppState,
    run_id: &str,
) -> Result<bool, OrchestratorError> {
    let run_state = state.store.get_run_state(run_id)?;
    if !matches!(run_state.as_str(), "RUNNING" | "PENDING" | "PAUSED") {
        return Ok(false);
    }
    if state.store.session_ends.run_stop_recorded(run_id) {
        return Ok(false);
    }
    let (_, session_id, _) = state.store.get_run_spec_context(run_id)?;
    if run_state == "PENDING" && session_id.is_none() {
        return Ok(false);
    }
    Ok(session_id.is_none_or(|s| !state.store.session_ends.is_finalizing(&s)))
}

fn stored_spec(
    state: &AppState,
    run_id: &str,
) -> Result<(RunSpec, Option<String>), OrchestratorError> {
    let (spec_json, _, agent_urn) = state.store.get_run_spec_context(run_id)?;
    let spec = spec_json
        .and_then(|json| vam_spec::encoding::json_to_proto::<RunSpec>("vam.v1.RunSpec", &json).ok())
        .ok_or_else(|| OrchestratorError::NotFound(format!("run '{run_id}' has no spec")))?;
    Ok((spec, agent_urn))
}

fn mode_name(mode: i32) -> &'static str {
    if mode == InboxMode::Queue as i32 {
        "QUEUE"
    } else {
        "STEER"
    }
}

/// The inputs struct clients send is either flat or wrapped once in `fields`
/// (the form `extract_prompt_from_inputs` reads); both are understood.
fn input_layers(spec: &RunSpec) -> Vec<&prost_types::Struct> {
    let Some(inputs) = spec.inputs.as_ref() else {
        return vec![];
    };
    let mut layers = vec![inputs];
    if let Some(prost_types::value::Kind::StructValue(nested)) =
        inputs.fields.get("fields").and_then(|v| v.kind.as_ref())
    {
        layers.push(nested);
    }
    layers
}

fn input_string(spec: &RunSpec, key: &str) -> Option<String> {
    input_layers(spec)
        .into_iter()
        .find_map(|layer| match layer.fields.get(key)?.kind.as_ref()? {
            prost_types::value::Kind::StringValue(s) => Some(s.clone()),
            _ => None,
        })
}

/// An input a message to a running task may not carry.
fn foreign_input(spec: &RunSpec) -> Option<String> {
    let layers = input_layers(spec);
    let wrapped = layers.len() > 1;
    for (depth, layer) in layers.iter().enumerate() {
        for key in layer.fields.keys() {
            let allowed =
                key == "prompt" || key == "model" || (depth == 0 && wrapped && key == "fields");
            if !allowed {
                return Some(key.clone());
            }
        }
    }
    None
}

fn invalid(message: &str) -> OrchestratorError {
    OrchestratorError::InvalidContract(message.to_string())
}

/// Validate and store one message; emits `inbox.received` for a new one.
#[allow(clippy::too_many_arguments)] // flat parameter list is the established call shape; a params struct would change callers
fn submit(
    state: &AppState,
    caller: &RunCaller,
    run_id: &str,
    mode: i32,
    text: &str,
    idempotency_key: &str,
    thread_ref: &str,
    workspace_id: &str,
    queued_spec_json: Option<String>,
) -> Result<InboxReceipt, OrchestratorError> {
    if text.trim().is_empty() {
        return Err(invalid("a message needs text"));
    }
    if text.len() > MAX_TEXT_BYTES {
        return Err(invalid(&format!(
            "a message is at most {MAX_TEXT_BYTES} bytes; this one is {}",
            text.len()
        )));
    }
    let mode = mode_name(mode);
    let (row, is_new) = state.store.insert_inbox_message_checked(&NewInboxRow {
        run_id: run_id.to_string(),
        mode: mode.to_string(),
        idempotency_key: idempotency_key.to_string(),
        owner_user_id: caller.owner_user_id.clone(),
        text: text.to_string(),
        thread_ref: thread_ref.to_string(),
        workspace_id: workspace_id.to_string(),
        queued_spec_json,
    })?;
    if is_new {
        crate::inbox::emit_received(&state.store, run_id, row.seq, mode);
    }
    Ok(InboxReceipt {
        seq: row.seq,
        mode: if mode == "QUEUE" {
            InboxMode::Queue as i32
        } else {
            InboxMode::Steer as i32
        },
        run_id: Some(ResourceId {
            urn: run_id.to_string(),
        }),
    })
}

/// Stores a message for the next task as a PENDING run of its own, behind the live run
/// `live_run_id`, and says which run it will start. `resolved` is the spec as `create_run`
/// resolved it (Space and thread ids stamped); the run starts from the same request.
#[allow(clippy::too_many_arguments)] // flat parameter list is the established call shape; a params struct would change callers
fn queue_message(
    state: &AppState,
    caller: &RunCaller,
    live_run_id: &str,
    resolved: &RunSpec,
    prompt_only: bool,
    idempotency_key: &str,
    thread_ref: &str,
    workspace_id: &str,
) -> Result<InboxReceipt, OrchestratorError> {
    let text = input_string(resolved, "prompt").unwrap_or_default();
    if text.trim().is_empty() {
        return Err(invalid("a message needs text"));
    }
    if text.len() > MAX_TEXT_BYTES {
        return Err(invalid(&format!(
            "a message is at most {MAX_TEXT_BYTES} bytes; this one is {}",
            text.len()
        )));
    }
    let queued_id = format!("urn:vam:run:local.vam.dev/acme/run-{}", super::uuid_like());
    let mut request = crate::inbox::request_from_resolved(resolved, prompt_only);
    request.idempotency_key = format!("queued:{idempotency_key}");
    let start = QueuedStart {
        request: vam_spec::encoding::proto_to_json("vam.v1.RunSpec", &request)
            .map_err(OrchestratorError::Core)?,
        caller: QueuedCaller::from_caller(caller),
    };
    let mut display = resolved.clone();
    display.run_id = Some(ResourceId {
        urn: queued_id.clone(),
    });
    display.inbox_mode = 0;
    display.idempotency_key = idempotency_key.to_string();
    let agent_urn = crate::run_context::requested_agent_urn(resolved).unwrap_or_default();
    let new = NewInboxRow {
        run_id: live_run_id.to_string(),
        mode: "QUEUE".to_string(),
        idempotency_key: idempotency_key.to_string(),
        owner_user_id: caller.owner_user_id.clone(),
        text,
        thread_ref: thread_ref.to_string(),
        workspace_id: workspace_id.to_string(),
        queued_spec_json: Some(
            serde_json::to_string(&start).map_err(|e| OrchestratorError::Core(e.to_string()))?,
        ),
    };
    let queued = NewQueuedRun {
        run_id: queued_id,
        spec_json: vam_spec::encoding::proto_to_json("vam.v1.RunSpec", &display)
            .map_err(OrchestratorError::Core)?,
        agent_urn,
        idempotency_key: Some(idempotency_key.to_string()),
        owner_principal: caller.principal.clone(),
        source: caller.source.record(),
        credential_id: caller.credential_id.clone(),
    };
    let (row, is_new) = state.store.insert_queued_message(&new, &queued)?;
    if is_new {
        crate::inbox::emit_received(&state.store, live_run_id, row.seq, "QUEUE");
    }
    Ok(InboxReceipt {
        seq: row.seq,
        mode: InboxMode::Queue as i32,
        run_id: row.started_run_id.map(|urn| ResourceId { urn }),
    })
}

/// The answer for a queued message: the run it will start, as `create_run` would answer for
/// a new one (Space and thread ids), plus the receipt.
pub(super) fn queued_response(
    state: &AppState,
    receipt: InboxReceipt,
    workspace_id: &str,
) -> Result<Response, ApiError> {
    let queued_id = receipt
        .run_id
        .as_ref()
        .map(|r| r.urn.clone())
        .unwrap_or_default();
    let resp = CreateRunResponse {
        status: Some(build_run_status(state, &queued_id)?),
        inbox: Some(receipt),
    };
    let (code, json) = json_response_accepted("vam.v1.CreateRunResponse", &resp)?;
    let mut value: serde_json::Value = serde_json::from_str(&json).unwrap_or_default();
    if let Some(object) = value.as_object_mut() {
        object.insert("workspaceId".into(), workspace_id.into());
        // The thread label and the ids, as `create_run` answers them.
        let status = object.get("status").cloned().unwrap_or_default();
        for key in ["threadRef", "spaceId", "threadId"] {
            if let Some(v) = status.get(key) {
                object.insert(key.into(), v.clone());
            }
        }
        object.insert("sessionId".into(), "".into());
    }
    Ok(json_http_response(
        code,
        serde_json::to_string(&value).unwrap_or(json),
    ))
}

/// Which busy state `busy` is, as the gateway tells them apart.
fn busy_kind(state: &AppState, busy: &Busy) -> BusyKind {
    if state.store.session_ends.run_stop_recorded(&busy.run_id) {
        return BusyKind::Stopping;
    }
    match &busy.session_id {
        Some(session_id) if state.store.session_ends.is_finalizing(session_id) => {
            BusyKind::Finalizing
        }
        Some(_) => BusyKind::Live,
        None => BusyKind::Parked,
    }
}

/// The busy check of `start_run`. A person's message to a live task steers or queues; a
/// triggered fire never steers (it always waits for the task) and keeps its playbook and
/// inputs, with one queued fire per credential.
pub fn admit(c: Candidate<'_>) -> Result<Admit, ApiError> {
    let Some(busy) = busy_run(c.state, c.workspace_id)? else {
        return Ok(Admit::Proceed);
    };
    let refuse = |busy: Busy| -> Result<Admit, ApiError> {
        Ok(Admit::Busy {
            kind: busy_kind(c.state, &busy),
            refusal: busy.refusal,
        })
    };
    let triggered = c.caller.source.is_trigger();
    if c.skip_if_busy {
        return Ok(Admit::Busy {
            kind: BusyKind::Skipped,
            refusal: OrchestratorError::Conflict(
                "the workspace is busy and the grant says skip".to_string(),
            ),
        });
    }

    // The starter is starting a queued run and the workspace was taken meanwhile: it stays
    // queued (today's retryable answer), it is not queued again.
    if c.spec
        .run_id
        .as_ref()
        .is_some_and(|r| c.state.store.queue_start_in_progress(&r.urn))
    {
        return refuse(busy);
    }
    // Not a live task, or one that is ending or being stopped: today's retryable answer.
    if !run_takes_messages(c.state, &busy.run_id).unwrap_or(false) {
        return refuse(busy);
    }
    if let Some(session_id) = &busy.session_id {
        if c.state.store.session_ends.is_finalizing(session_id) {
            return refuse(busy);
        }
    }
    let Ok((live, live_agent)) = stored_spec(c.state, &busy.run_id) else {
        return refuse(busy);
    };
    if c.state
        .store
        .require_run_owner(&busy.run_id, &c.caller.owner_user_id)
        .is_err()
    {
        return refuse(busy);
    }

    // From here the message is for this workspace's task, so a bad one is an error to the sender.
    if let (Some(bound), Some(requested)) = (live_agent.as_deref(), c.requested_urn) {
        if bound != requested {
            return Err(OrchestratorError::AgentWorkspaceMismatch {
                workspace_id: c.workspace_id.to_string(),
                bound: bound.to_string(),
                requested: requested.to_string(),
            }
            .into());
        }
    }
    // A message to a running task carries only a prompt. A triggered fire is a whole run
    // that waits its turn, so it keeps its playbook and inputs.
    if !triggered {
        if c.has_inline || c.spec.playbook_ref.is_some() {
            return Err(invalid("a message to a running task carries only a prompt").into());
        }
        if let Some(key) = foreign_input(c.spec) {
            return Err(invalid(&format!(
                "a message to a running task carries only a prompt; input '{key}' is not allowed"
            ))
            .into());
        }
    }
    let live_thread = crate::run_context::extract_thread_ref(&live).ok();
    let same_thread = live_thread.as_deref() == Some(c.thread_ref);

    // Another thread on a busy workspace, a message sent in queue mode, or a triggered
    // fire: it waits for the task and starts as the next one (owner decision D, AR-3.8).
    if triggered || !same_thread || c.spec.inbox_mode == InboxMode::Queue as i32 {
        if let Some(credential) = c.caller.credential_id.as_deref().filter(|_| triggered) {
            if c.state.store.credential_has_queued_fire(credential)? {
                return Ok(Admit::Busy {
                    kind: BusyKind::AlreadyQueued,
                    refusal: OrchestratorError::Conflict(
                        "a fire from this credential is already queued".to_string(),
                    ),
                });
            }
        }
        return match queue_message(
            c.state,
            c.caller,
            &busy.run_id,
            c.spec,
            false,
            &c.spec.idempotency_key,
            c.thread_ref,
            c.workspace_id,
        ) {
            Ok(receipt) => Ok(Admit::Queued { receipt }),
            // The run ended between the check and the insert.
            Err(OrchestratorError::Conflict(_)) => refuse(busy),
            Err(other) => Err(other.into()),
        };
    }

    if let Some(model) = input_string(c.spec, "model") {
        if input_string(&live, "model").as_deref() != Some(model.as_str()) {
            return Err(
                invalid("a running task keeps its model; the message names another").into(),
            );
        }
    }
    let text = input_string(c.spec, "prompt").unwrap_or_default();
    match submit(
        c.state,
        c.caller,
        &busy.run_id,
        c.spec.inbox_mode,
        &text,
        &c.spec.idempotency_key,
        c.thread_ref,
        c.workspace_id,
        None,
    ) {
        Ok(receipt) => Ok(Admit::Steered {
            run_id: busy.run_id,
            receipt,
        }),
        // The run ended between the check and the insert.
        Err(OrchestratorError::Conflict(_)) => refuse(busy),
        Err(other) => Err(other.into()),
    }
}

/// Run cancel discards what the run never took in (pending steers, queued messages), as a
/// stop does (owner decision, AR-3.7).
pub(super) fn discard_on_cancel(state: &AppState, run_id: &str) {
    if let Err(err) = crate::inbox::discard_for_stop(&state.store, run_id) {
        tracing::warn!("inbox: messages of cancelled run {run_id} not discarded: {err}");
    }
}

/// `POST /v1/runs/{id}/messages`.
pub(super) async fn send_run_message(
    state: &AppState,
    auth: &AuthContext,
    run_id: &str,
    headers: &HeaderMap,
    body: &str,
) -> Result<Response, ApiError> {
    ensure_ready(state, true)?;
    state.pdp.authorize(auth, "SendRunMessage", "runs:write")?;
    let header_key = require_idempotency_key(headers).map_err(ApiError::from)?;
    state
        .store
        .require_run_owner(run_id, auth.owner_user_id())?;
    let req: SendRunMessageRequest = parse_json_body("vam.v1.SendRunMessageRequest", body)?;
    if let Some(named) = req.run_id.as_ref().filter(|r| !r.urn.is_empty()) {
        if named.urn != run_id {
            return Err(invalid("runId in the body names another run").into());
        }
    }
    let key = if req.idempotency_key.is_empty() {
        header_key
    } else {
        req.idempotency_key.clone()
    };
    if !run_takes_messages(state, run_id)? {
        return Err(OrchestratorError::Conflict(format!(
            "run '{run_id}' is not running; start a new task instead"
        ))
        .into());
    }
    let (spec, _) = stored_spec(state, run_id)?;
    let thread_ref = crate::run_context::extract_thread_ref(&spec)
        .unwrap_or_else(|_| crate::run_context::DEFAULT_THREAD_REF.to_string());
    let workspace_id =
        crate::run_context::parse_workspace_ref_uri(&spec.workspace_ref_uri).unwrap_or_default();
    let caller = RunCaller::from_auth(auth, super::request_surface(headers));
    if req.mode == InboxMode::Queue as i32 {
        // A message for the next task: the live run's own request again, with this text.
        let mut resolved = spec.clone();
        crate::inbox::set_prompt(&mut resolved, &req.text);
        resolved.idempotency_key = key.clone();
        let receipt = queue_message(
            state,
            &caller,
            run_id,
            &resolved,
            true,
            &key,
            &thread_ref,
            &workspace_id,
        )?;
        return queued_message_response(state, receipt);
    }
    let receipt = submit(
        state,
        &caller,
        run_id,
        req.mode,
        &req.text,
        &key,
        &thread_ref,
        &workspace_id,
        None,
    )?;
    let resp = SendRunMessageResponse {
        status: Some(build_run_status(state, run_id)?),
        inbox: Some(receipt),
    };
    let (code, json) = json_response_accepted("vam.v1.SendRunMessageResponse", &resp)?;
    debug_assert_eq!(code, StatusCode::ACCEPTED);
    Ok(json_http_response(code, json))
}

fn queued_message_response(state: &AppState, receipt: InboxReceipt) -> Result<Response, ApiError> {
    let queued_id = receipt
        .run_id
        .as_ref()
        .map(|r| r.urn.clone())
        .unwrap_or_default();
    let resp = SendRunMessageResponse {
        status: Some(build_run_status(state, &queued_id)?),
        inbox: Some(receipt),
    };
    let (code, json) = json_response_accepted("vam.v1.SendRunMessageResponse", &resp)?;
    Ok(json_http_response(code, json))
}
