//! The starter of queued messages (AR-3.8, owner decision D).
//!
//! A queued message is a `run_inbox` row plus a PENDING run with no session, created when the
//! message was taken in. When the task it waits behind has ended (any end but a user stop,
//! which discards it) and the workspace is free, the starter starts that run: it posts the
//! stored request through `create_run` as the person who sent it, naming the PENDING run's
//! id, and `create_run` adopts the row. One task does this, one run at a time per workspace,
//! so a message starts once. A pass is woken by every terminal write, ticks every few
//! seconds, and runs once when vamd starts (a message queued behind a run that ended while
//! vamd was down).

use std::collections::HashSet;
use std::time::Duration;

use axum::http::StatusCode;
use vam_spec::v1::{CreateRunRequest, RunSpec};

use super::run_start::{start_run, StartError, StartOpts};
use super::AppState;
use crate::inbox::QueuedStart;
use crate::inbox_store::InboxRow;
use crate::receipt::is_terminal;

/// How often a pass runs with no wake.
const TICK: Duration = Duration::from_secs(2);

/// One pass at a time, whoever asks (the task, a test).
static PASS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Spawns the starter. A pass also runs right away, for a restart.
pub fn spawn_queue_starter(state: AppState) {
    tokio::spawn(async move {
        loop {
            run_queue_pass(&state).await;
            tokio::select! {
                _ = state.store.queue_wake.notified() => {}
                _ = tokio::time::sleep(TICK) => {}
            }
        }
    });
}

/// Turns the steers of ended runs into queued tasks, then starts the oldest startable queued
/// message of each workspace. Returns how many runs it started.
pub async fn run_queue_pass(state: &AppState) -> usize {
    let _pass = PASS.lock().await;
    let store = &state.store;
    crate::inbox::queue_ended_runs_steers(store);
    let queued = match store.list_queued() {
        Ok(rows) => rows,
        Err(err) => {
            tracing::warn!("queue: queued messages not listed: {err}");
            return 0;
        }
    };
    let mut seen: HashSet<String> = HashSet::new();
    let mut started = 0;
    for row in queued {
        // A triggered fire carries a window: past it the run ends EXPIRED and never starts,
        // wherever it stands in the line.
        if past_its_window(&row) {
            if let Some(queued_run) = row.started_run_id.as_deref() {
                expire(state, &row, queued_run);
            }
            continue;
        }
        // Oldest first, and only the oldest of a workspace is looked at: the others wait
        // their turn even when the oldest cannot start yet.
        if !seen.insert(row.workspace_id.clone()) {
            continue;
        }
        if start_queued(state, &row).await {
            started += 1;
        }
    }
    started
}

/// Starts one queued message if it can start now. `true` when a run was started.
async fn start_queued(state: &AppState, row: &InboxRow) -> bool {
    let store = &state.store;
    let Some(queued_run) = row.started_run_id.clone() else {
        return false;
    };
    // What the run behind the message is now. A person may have cancelled it; a pass that
    // died after starting it left it started.
    match store.get_run_state(&queued_run).as_deref() {
        Ok("PENDING") if store.is_queued_run(&queued_run) => {}
        Ok("CANCELLED") => {
            let _ = store.mark_queued_discarded(&row.run_id, row.seq);
            return false;
        }
        Ok(_) => {
            if store
                .mark_queued_started(&row.run_id, row.seq)
                .unwrap_or(false)
            {
                crate::inbox::emit_started(store, &row.run_id, row.seq, &queued_run);
            }
            return false;
        }
        Err(_) => {
            let _ = store.mark_queued_discarded(&row.run_id, row.seq);
            return false;
        }
    }
    // The task it waits behind must have ended, and the workspace be free.
    if !store
        .get_run_state(&row.run_id)
        .map(|s| is_terminal(&s))
        .unwrap_or(false)
    {
        return false;
    }
    if super::inbox::workspace_busy(state, &row.workspace_id) {
        return false;
    }

    let Some(start) = row
        .queued_spec_json
        .as_deref()
        .and_then(|json| serde_json::from_str::<QueuedStart>(json).ok())
    else {
        fail(
            state,
            row,
            &queued_run,
            "the queued message has no request to start",
        );
        return false;
    };
    let caller = match start.caller.to_caller(store) {
        Ok(caller) => caller,
        Err(err) => {
            fail(
                state,
                row,
                &queued_run,
                &format!("the queued message was not started: {err}"),
            );
            return false;
        }
    };
    // A fire starts only while its grant, its broker and its user's permission still stand.
    if let Some(why) =
        crate::gateway::recheck_queued_fire(store, &caller, &crate::keys::default_scopes_for_role)
    {
        fail(state, row, &queued_run, &why);
        return false;
    }
    let mut spec: RunSpec =
        match vam_spec::encoding::json_to_proto("vam.v1.RunSpec", &start.request) {
            Ok(spec) => spec,
            Err(err) => {
                fail(
                    state,
                    row,
                    &queued_run,
                    &format!("the queued request is unreadable: {err}"),
                );
                return false;
            }
        };
    spec.run_id = Some(vam_spec::v1::ResourceId {
        urn: queued_run.clone(),
    });
    let opts = StartOpts {
        skip_if_busy: false,
        idempotency_key: spec.idempotency_key.clone(),
        traceparent: None,
    };
    let req = CreateRunRequest {
        spec: Some(spec),
        ..Default::default()
    };

    let _starting = store.begin_queue_start(&queued_run);
    match start_run(state, &caller, req, opts).await {
        Ok(_) => {
            if store
                .mark_queued_started(&row.run_id, row.seq)
                .unwrap_or(false)
            {
                crate::inbox::emit_started(store, &row.run_id, row.seq, &queued_run);
            }
            true
        }
        // The workspace was taken between the check and the start: it stays queued.
        Err(StartError::Busy { .. }) => false,
        Err(err) => {
            let err = err.into_api();
            // The same answer by status, for a refusal the HTTP path reports as a conflict.
            if err.status == StatusCode::CONFLICT && err.retryable {
                return false;
            }
            fail(
                state,
                row,
                &queued_run,
                &format!("the queued message was not started: {}", err.message),
            );
            false
        }
    }
}

/// Whether the queued message is a fire whose `not_after` has passed.
fn past_its_window(row: &InboxRow) -> bool {
    row.queued_spec_json
        .as_deref()
        .and_then(|json| serde_json::from_str::<QueuedStart>(json).ok())
        .and_then(|start| start.caller.not_after)
        .is_some_and(|not_after| now_secs() > not_after)
}

/// Unix seconds now.
fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// The queued fire's window passed: its run ends EXPIRED and the row is spent.
fn expire(state: &AppState, row: &InboxRow, queued_run: &str) {
    tracing::info!("queue: {queued_run}: not started, its window passed");
    let _ = state.store.expire_queued_run(
        queued_run,
        "the queued fire was not started before its not_after",
    );
    let _ = state.store.mark_queued_discarded(&row.run_id, row.seq);
}

/// The message cannot start: its run ends FAILED with the reason and the row is spent.
fn fail(state: &AppState, row: &InboxRow, queued_run: &str, message: &str) {
    tracing::warn!("queue: {queued_run}: {message}");
    let _ = state.store.fail_queued_run(queued_run, message);
    let _ = state.store.mark_queued_discarded(&row.run_id, row.seq);
}
