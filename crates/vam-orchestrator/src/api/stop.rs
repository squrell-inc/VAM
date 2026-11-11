//! `POST /v1/runs/{id}/stop` (AR-3.7, design V.3).
//!
//! The user's stop is **recorded first** (`SessionEnds::record_run_stop`) and then wins over
//! whatever the guest reports after it: the run ends FAILED, reason `stopped`, error
//! "stopped by user" (the rule lives where terminal results are staged, `complete_run_with_result`).
//! The stop then works the run to its end, whatever state it is in:
//!
//! * **Tracked** (a task being dispatched): the Phase 2 stop. The VM goes down, the run's
//!   finalizer writes the end of the conversation and settles the workspace at the last complete
//!   turn commit; the stop waits for that and for the result to be published. A run whose
//!   finalizer is already ending it (it finished or failed first) is waited for, not changed.
//! * **Parked on an approval** (PAUSED): the approval is rejected by a compare-and-set on
//!   `decision IS NULL` (a question is cancelled), "stopped by user", and the run never resumes.
//!   A warm session is taken down and ended by the same finalizer; a run whose VM is gone is
//!   ended on the host with no VM (`session_end::end_parked_on_host`). Either way the log says
//!   "Not run: stopped by user" and `task_end: stopped`, and the reservation is released.
//! * **Neither** (booting, or a decision that won the compare-and-set and is resuming): wait until
//!   the run is tracked or ended, then take the first branch.
//!
//! Pending steers and queued messages are discarded first and returned with their text.

use std::time::{Duration, Instant};

use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use vam_spec::v1::run_spec::InboxMode;
use vam_spec::v1::{DiscardedMessage, ResourceId, StopRunResponse};

use super::auth::{require_idempotency_key, AuthContext};
use super::error::{json_response, ApiError};
use super::{build_run_status, ensure_ready, json_http_response, AppState, DispatchOutcome};
use crate::error::OrchestratorError;
use crate::inbox_store::InboxRow;
use crate::receipt::is_terminal;
use crate::session_end::{ParkedEnd, RunTracking, STOP_SETTLE_TIMEOUT};

fn pending_error(message: &str) -> ApiError {
    ApiError {
        status: StatusCode::GATEWAY_TIMEOUT,
        code: vam_spec::v1::VamErrorCode::Unavailable,
        message: format!("stop_pending: {message}"),
        retryable: true,
    }
}

/// `POST /v1/runs/{id}/stop`.
pub(super) async fn stop_run(
    state: &AppState,
    auth: &AuthContext,
    run_id: &str,
    headers: &HeaderMap,
) -> Result<Response, ApiError> {
    ensure_ready(state, true)?;
    state.pdp.authorize(auth, "StopRun", "runs:write")?;
    let _idempotency = require_idempotency_key(headers).map_err(ApiError::from)?;
    state
        .store
        .require_run_owner(run_id, auth.owner_user_id())?;

    let who = Who {
        owner_user_id: auth.owner_user_id().to_string(),
        principal: auth.key.record.principal.clone(),
        surface: super::request_surface(headers),
    };
    // The work runs on its own task: a client that gives up does not leave a stop half done.
    let task = {
        let (state, run_id) = (state.clone(), run_id.to_string());
        tokio::spawn(async move { stop_body(&state, &who, &run_id).await })
    };
    let discarded = match tokio::time::timeout(STOP_SETTLE_TIMEOUT + Duration::from_secs(5), task)
        .await
    {
        Ok(Ok(result)) => result?,
        Ok(Err(_)) => return Err(OrchestratorError::Core("the stop task panicked".into()).into()),
        Err(_) => return Err(pending_error("the run is still being stopped")),
    };

    let resp = StopRunResponse {
        status: Some(build_run_status(state, run_id)?),
        discarded: discarded.iter().map(discarded_message).collect(),
    };
    let (code, json) = json_response("vam.v1.StopRunResponse", &resp)?;
    Ok(json_http_response(code, json))
}

fn discarded_message(row: &InboxRow) -> DiscardedMessage {
    let queued = row.mode == "QUEUE";
    DiscardedMessage {
        seq: row.seq,
        mode: if queued {
            InboxMode::Queue as i32
        } else {
            InboxMode::Steer as i32
        },
        // A queued message names the run it would have started.
        run_id: Some(ResourceId {
            urn: row
                .started_run_id
                .clone()
                .filter(|_| queued)
                .unwrap_or_else(|| row.run_id.clone()),
        }),
        text: row.text.clone(),
    }
}

/// Who stops, as the approval decision records it.
struct Who {
    owner_user_id: String,
    principal: String,
    surface: &'static str,
}

enum Step {
    /// Progress was made or may be made: look at the run again.
    Again,
    /// The run needs a moment (a resume is in flight).
    Wait,
}

async fn stop_body(state: &AppState, who: &Who, run_id: &str) -> Result<Vec<InboxRow>, ApiError> {
    let store = &state.store;
    // A run that has ended answers with its own end, and no lock is made for it.
    if is_terminal(&store.get_run_state(run_id)?) {
        return Ok(Vec::new());
    }
    // One stop at a time per run; a second one waits, then reads the end.
    let lock = store.session_ends.run_stop_lock(run_id);
    let _held = lock.lock().await;
    let started = Instant::now();
    let mut discarded: Vec<InboxRow> = Vec::new();

    // A run that has ended answers with its own end. A queued message that has not started
    // is discarded: it is only a message.
    let current = store.get_run_state(run_id)?;
    if is_terminal(&current) {
        store.session_ends.clear_run_stop(run_id);
        return Ok(discarded);
    }
    if store.is_queued_run(run_id) {
        if let Some(row) = store.discard_queued_message(run_id)? {
            crate::trajectory::append_trajectory_event(
                store,
                &row.run_id,
                crate::events::TYPE_INBOX_DISCARDED,
                &serde_json::json!({ "seqs": [row.seq] }),
            );
            discarded.push(row);
        }
        return Ok(discarded);
    }

    // The stop is recorded before anything else is done for it, in memory and in the database
    // (a restart must never resume a run the user stopped).
    store.session_ends.record_run_stop(run_id);
    if let Err(e) = store.persist_run_stop(run_id) {
        tracing::error!(run_id, "the stop is not persisted: {e}");
    }
    discarded.extend(crate::inbox::discard_for_stop(store, run_id)?);

    loop {
        let state_now = store.get_run_state(run_id)?;
        if is_terminal(&state_now) {
            return Ok(discarded);
        }
        let remaining = STOP_SETTLE_TIMEOUT.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return Err(pending_error("the run is stopped; it is still being ended"));
        }
        match stop_step(state, who, run_id, &state_now, remaining).await? {
            Step::Again => tokio::time::sleep(Duration::from_millis(10)).await,
            Step::Wait => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    }
}

async fn stop_step(
    state: &AppState,
    who: &Who,
    run_id: &str,
    run_state: &str,
    remaining: Duration,
) -> Result<Step, ApiError> {
    let store = &state.store;
    let session_id = store.get_run_session_id(run_id)?;

    if let Some(session) = session_id.as_deref() {
        if store.session_ends.is_tracked(session) {
            if store.session_ends.is_finalizing(session) {
                // The run's own end is being written: wait for the workspace to settle.
                if !store.session_ends.wait_settled(session, remaining).await {
                    return Err(pending_error(
                        "the run has ended; its workspace is still being settled",
                    ));
                }
                return Ok(Step::Again);
            }
            if store.session_ends.begin_stop(session) {
                let stop_started = Instant::now();
                state
                    .manager
                    .stop_session_vm(session.to_string())
                    .await
                    .map_err(ApiError::from)?;
                if !store.session_ends.wait_settled(session, remaining).await {
                    return Err(pending_error(
                        "the session is stopped; its workspace is still being settled",
                    ));
                }
                if let Some(timing) = store.session_ends.take_stop_timing(session) {
                    eprintln!(
                        "[orchestrator] stop settled: session={} wait_ms={} prepare_end_ms={} resolve_ms={} scan_ms={}",
                        session,
                        stop_started.elapsed().as_millis(),
                        timing.prepare_end_ms,
                        timing.resolve_ms,
                        timing.scan_ms
                    );
                }
            }
            return Ok(Step::Again);
        }
    }

    // Interrupted by a crash or a restart and waiting for a new session: the stop takes the run
    // out of the resume cycle (compare-and-set against the resume pass) and ends it on the host.
    if let Some(row) = store.resume_row(run_id)? {
        if row.state == crate::resume_store::ResumeState::Pending {
            if store.end_resume_pending(run_id)? {
                return stop_resume_pending(state, run_id, &row.prior_session_id).await;
            }
            // The pass took it first: its session is booting; the next look finds it tracked.
            return Ok(Step::Wait);
        }
    }
    if run_state == "PAUSED" {
        return stop_parked(state, who, run_id, session_id.as_deref()).await;
    }
    // Booting, or resuming after a decision that won: the next look finds it tracked or ended.
    Ok(Step::Wait)
}

/// The run waits to be resumed and the stop won it: the host ends it, as for a parked run whose
/// VM is gone ("Not run: stopped by user", `task_end: stopped`, HEAD at the last complete turn
/// commit).
async fn stop_resume_pending(
    state: &AppState,
    run_id: &str,
    prior_session_id: &str,
) -> Result<Step, ApiError> {
    let store = &state.store;
    let prompt = super::original_prompt_from_run(store, run_id)?;
    let (thread_ref, model) = super::thread_ref_and_model_from_run(store, run_id)?;
    let stopped = crate::session_end::stopped_result_json();
    let outcome = state
        .manager
        .end_parked_on_host(ParkedEnd {
            session_id: prior_session_id.to_string(),
            run_id: run_id.to_string(),
            user_prompt: prompt,
            thread_ref,
            model,
            failure: None,
        })
        .await
        .map_err(ApiError::from)?;
    if let crate::session_end::SettleOutcome::Kept { kept_id, .. } = &outcome {
        tracing::error!(
            run_id,
            "the stopped run's workspace was kept as '{kept_id}'"
        );
    }
    end_parked_run(state, run_id, prior_session_id, &stopped).await;
    Ok(Step::Again)
}

/// The run is parked on an approval (or a gate, or a question).
async fn stop_parked(
    state: &AppState,
    who: &Who,
    run_id: &str,
    run_session: Option<&str>,
) -> Result<Step, ApiError> {
    let store = &state.store;
    let rejection =
        store.reject_pending_for_stop(run_id, &who.owner_user_id, &who.principal, who.surface)?;
    if rejection.stopped.is_empty() && (rejection.lost > 0 || !store.has_stop_rejection(run_id)) {
        // A decision won: its resume is on its way and will be stopped as a running task.
        return Ok(Step::Wait);
    }
    let session_id = rejection
        .stopped
        .iter()
        .find_map(|a| a.session_id.clone())
        .or_else(|| run_session.map(str::to_string))
        .filter(|s| !s.is_empty());

    let prompt = super::original_prompt_from_run(store, run_id)?;
    let (thread_ref, model) = super::thread_ref_and_model_from_run(store, run_id)?;
    let session = session_id
        .as_deref()
        .and_then(|s| store.load_session(s).ok());
    let warm = session
        .as_ref()
        .is_some_and(crate::manager::session_is_warm);

    let stopped = crate::session_end::stopped_result_json();
    match (session_id.as_deref(), session.as_ref()) {
        (Some(session_id), Some(record)) if warm => {
            // The VM is still up: take it down and end the run with the finalizer.
            let _tracking = RunTracking::new(store.clone(), session_id, run_id);
            store.session_ends.begin_stop(session_id);
            if let Some(recorder) = store.thread_recorder(session_id) {
                recorder.mark_user_stop();
            }
            state
                .manager
                .stop_session_vm(session_id.to_string())
                .await
                .map_err(ApiError::from)?;
            store.complete_run_with_result(run_id, "FAILED", &stopped)?;
            let _ = store.release_run_reservation(run_id);
            let _ = store.clear_keep_warm(run_id);
            super::finalize_run_session(
                &state.manager,
                store.as_ref(),
                run_id,
                session_id,
                &record.workspace_id,
                &thread_ref,
                &prompt,
                &model,
                &DispatchOutcome {
                    answer: String::new(),
                    succeeded: false,
                    paused: false,
                    interrupted: false,
                    spend_micro: None,
                },
            )
            .await;
        }
        (Some(session_id), Some(_)) => {
            // The VM is gone: the host writes the end. No VM is booted.
            let outcome = state
                .manager
                .end_parked_on_host(ParkedEnd {
                    session_id: session_id.to_string(),
                    run_id: run_id.to_string(),
                    user_prompt: prompt,
                    thread_ref,
                    model,
                    failure: None,
                })
                .await
                .map_err(ApiError::from)?;
            if let crate::session_end::SettleOutcome::Kept { kept_id, .. } = &outcome {
                tracing::error!(
                    run_id,
                    "the stopped run's workspace was kept as '{kept_id}'"
                );
            }
            end_parked_run(state, run_id, session_id, &stopped).await;
        }
        _ => {
            // No session to settle (a legacy row): the run just ends.
            store.complete_run_with_result(run_id, "FAILED", &stopped)?;
            let _ = store.release_run_reservation(run_id);
            let _ = store.clear_keep_warm(run_id);
            crate::receipt::issue_receipt_logged(store, run_id, None);
        }
    }
    Ok(Step::Again)
}

/// The terminal work for a run ended on the host: result, reservation, hooks, grants, receipt.
pub(super) async fn end_parked_run(
    state: &AppState,
    run_id: &str,
    session_id: &str,
    result_json: &str,
) {
    end_parked_run_in(&state.store, run_id, session_id, result_json).await;
}

/// [`end_parked_run`] with only the store (the resume pass has no request state).
pub(super) async fn end_parked_run_in(
    store: &std::sync::Arc<crate::workflow::SqliteWorkflowStore>,
    run_id: &str,
    session_id: &str,
    result_json: &str,
) {
    if let Err(e) = store.complete_run_with_result(run_id, "FAILED", result_json) {
        tracing::error!(run_id, "stopped run not completed: {e}");
    }
    let _ = store.release_run_reservation(run_id);
    let _ = store.clear_keep_warm(run_id);
    if let Err(e) =
        super::dispatch_on_complete_for_terminal(store.as_ref(), run_id, session_id).await
    {
        tracing::warn!("[orchestrator] on_complete hooks failed: {e}");
    }
    let layout = vam_storage::Drive9Layout::new(super::drive9_root());
    for digest in store.revoke_run_context_grants(run_id) {
        crate::thread_context::remove_context(&layout, session_id, &digest);
    }
    crate::receipt::issue_receipt_logged(store, run_id, None);
}
