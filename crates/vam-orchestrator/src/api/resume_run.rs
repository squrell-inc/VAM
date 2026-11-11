//! The resume pass (E10 Phase 4, AR-4.5): picks up the runs a crash or a restart interrupted.
//!
//! One task, like the queue starter: it runs once when `vamd` starts (after taking down the VMs
//! it adopted whose run was being worked on), is woken by every interrupt, and ticks every few
//! seconds. A pass holds a mutex, so there is one at a time, and a run is taken by a
//! compare-and-set on its `run_resume` row (`pending -> resuming`), so a stop, a cancel or a
//! second pass cannot also take it. Each claimed run resumes on its own task.
//!
//! A resume boots a new session on the workspace as the interrupt settled it, hands the guest
//! the host-assembled conversation (`ThreadContext.crash`), and runs it to the end of the task
//! like an approval resume. The run row (`runs.source`, credential, grants) is untouched.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use super::AppState;
use crate::manager::SessionManager;
use crate::receipt::is_terminal;
use crate::resume::{ContextRefusal, InterruptEnd};
use crate::resume_store::{Claim, ResumeRow};
use crate::session_end::ParkedEnd;
use crate::workflow::SqliteWorkflowStore;

/// How often a pass runs with no wake.
const TICK: Duration = Duration::from_secs(2);

/// How long a claim waits for the interrupted session's own dispatch to let go before it boots.
const PRIOR_RELEASE_WAIT: Duration = Duration::from_secs(10);

/// One pass at a time, whoever asks (the task, a test).
static PASS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Spawns the pass. It also takes down adopted VMs whose run was being worked on (Q11) and
/// releases claims a previous `vamd` died holding.
pub fn spawn_resume_pass(state: AppState) {
    tokio::spawn(async move {
        if let Err(e) = state.store.reset_stale_resume_claims() {
            tracing::warn!("resume: stale claims not reset: {e}");
        }
        let adopted = crate::resume::interrupt_adopted(&state.manager, &state.store).await;
        if adopted > 0 {
            tracing::info!("resume: {adopted} adopted VM(s) taken down; their runs will resume");
        }
        loop {
            run_resume_pass(&state).await;
            tokio::select! {
                _ = state.store.resume_wake.notified() => {}
                _ = tokio::time::sleep(TICK) => {}
            }
        }
    });
}

/// Claims every pending run it can and starts its resume. Returns how many it started.
pub async fn run_resume_pass(state: &AppState) -> usize {
    let _pass = PASS.lock().await;
    let store = &state.store;
    let rows = match store.resume_pending_runs() {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!("resume: pending runs not listed: {e}");
            return 0;
        }
    };
    let mut started = 0;
    // One resume per workspace at a time: the others wait their turn.
    let mut taken: HashSet<String> = HashSet::new();
    for row in rows {
        // A run that ended meanwhile (a cancel) is not resumed.
        match store.get_run_state(&row.run_id) {
            Ok(run_state) if !is_terminal(&run_state) => {}
            _ => {
                let _ = store.end_resume_claimed(&row.run_id);
                continue;
            }
        }
        if store.run_stop_known(&row.run_id) {
            // A stop is on its way to this run; it takes the row.
            continue;
        }
        if !taken.insert(row.workspace_id.clone())
            || super::inbox::workspace_taken_by_session(state, &row.workspace_id)
        {
            continue;
        }
        match store.claim_resume(&row.run_id) {
            Ok(Claim::Won(claimed)) => {
                started += 1;
                let state = state.clone();
                tokio::spawn(async move { resume_claimed(&state, claimed).await });
            }
            Ok(Claim::Capped { reason, row }) => {
                let message = if reason == "crashed_repeatedly" {
                    "the task crashed too many times to be resumed again"
                } else {
                    "vamd restarted too many times for the task to be resumed again"
                };
                end_on_host(state, &row, reason, message).await;
            }
            Ok(Claim::Lost) => {}
            Err(e) => tracing::warn!(run = %row.run_id, "resume: claim failed: {e}"),
        }
    }
    started
}

/// Ends a run that waits to be resumed, on the host: nothing is booted. The result carries the
/// reason; the conversation ends `task_end{failed}`; files stay at the last complete turn commit.
async fn end_on_host(state: &AppState, row: &ResumeRow, reason: &str, message: &str) {
    let store = &state.store;
    let _ = store.end_resume_claimed(&row.run_id);
    let result = serde_json::json!({ "error": message, "reason": reason }).to_string();
    let ended = match (
        super::original_prompt_from_run(store, &row.run_id),
        super::thread_ref_and_model_from_run(store, &row.run_id),
    ) {
        (Ok(prompt), Ok((thread_ref, model))) => state
            .manager
            .end_parked_on_host(ParkedEnd {
                session_id: row.prior_session_id.clone(),
                run_id: row.run_id.clone(),
                user_prompt: prompt,
                thread_ref,
                model,
                failure: Some(reason.to_string()),
            })
            .await
            .map(|_| ()),
        (Err(e), _) | (_, Err(e)) => Err(e),
    };
    if let Err(e) = ended {
        tracing::error!(run = %row.run_id, "resume: the run could not be ended cleanly: {e}");
    }
    super::stop::end_parked_run_in(store, &row.run_id, &row.prior_session_id, &result).await;
}

/// The run's own dispatch (or the interrupt) ended the session as a failure or a stop: finish
/// the run on the host. Used when the interrupt could not leave the run waiting.
pub async fn interrupt_or_end(
    manager: &SessionManager,
    store: &Arc<SqliteWorkflowStore>,
    run_id: &str,
    session_id: &str,
    reason: &str,
) {
    match crate::resume::interrupt(manager, store, run_id, session_id, reason).await {
        InterruptEnd::Pending => {
            tracing::info!(run_id, reason, "the run waits to be resumed");
        }
        InterruptEnd::Stopped => {
            // The user's stop won. The VM is down and the workspace settled: the host ends the
            // run as stopped.
            store.session_ends.record_run_stop(run_id);
            let stopped = crate::session_end::stopped_result_json();
            let ended = match (
                super::original_prompt_from_run(store, run_id),
                super::thread_ref_and_model_from_run(store, run_id),
            ) {
                (Ok(prompt), Ok((thread_ref, model))) => manager
                    .end_parked_on_host(ParkedEnd {
                        session_id: session_id.to_string(),
                        run_id: run_id.to_string(),
                        user_prompt: prompt,
                        thread_ref,
                        model,
                        failure: None,
                    })
                    .await
                    .map(|_| ()),
                (Err(e), _) | (_, Err(e)) => Err(e),
            };
            if let Err(e) = ended {
                tracing::error!(run_id, "the stopped run could not be ended cleanly: {e}");
            }
            super::stop::end_parked_run_in(store, run_id, session_id, &stopped).await;
        }
        InterruptEnd::Failed(json) => {
            super::stop::end_parked_run_in(store, run_id, session_id, &json).await;
        }
    }
}

/// Fails a resume the pass owns: the run ends `FAILED` with a reason. No session is up.
async fn fail_before_boot(state: &AppState, row: &ResumeRow, reason: &str, message: &str) {
    end_on_host(state, row, reason, message).await;
}

/// Puts a claimed run back to waiting (the attempt stays counted). `prior` is the session the
/// next attempt resumes from (a half-booted one has taken the resume key).
fn put_back(state: &AppState, run_id: &str, prior_session_id: Option<&str>) {
    if let Some(prior) = prior_session_id {
        let _ = state.store.set_resume_prior_session(run_id, prior);
    }
    let _ = state.store.release_resume_claim(run_id);
}

/// Resumes one claimed run on a new session.
async fn resume_claimed(state: &AppState, claimed: ResumeRow) {
    let store = state.store.clone();
    let manager = state.manager.clone();
    let run_id = claimed.run_id.clone();

    // 1. Who may resume it, and whether it may.
    match store.get_run_state(&run_id) {
        Ok(run_state) if !is_terminal(&run_state) => {}
        _ => {
            let _ = store.end_resume_claimed(&run_id);
            return;
        }
    }
    if store.run_stop_known(&run_id) {
        // A stop began after the claim: it takes the run (it ends it as soon as the session is
        // tracked or ended). Give the row back so its compare-and-set finds it.
        put_back(state, &run_id, None);
        return;
    }
    if crate::resume::legacy_ids_block_resume(&store, &run_id) {
        fail_before_boot(
            state,
            &claimed,
            crate::resume::REASON_NOT_RESUMABLE,
            "this task was started by an older guest that did not name its calls, so a call \
             that already happened could not be told from a new one; it is not resumed. Ask again",
        )
        .await;
        return;
    }
    if let Ok(owner) = store.run_owner_user_id(&run_id) {
        let deactivated = store
            .get_user(&owner)
            .map(|user| crate::users::reject_deactivated_user(&user).is_err())
            .unwrap_or(false);
        if deactivated {
            fail_before_boot(
                state,
                &claimed,
                "owner_deactivated",
                "the owner of this task was deactivated; it is not resumed",
            )
            .await;
            return;
        }
    }
    if super::active_budget_exhausted(&store, &run_id) {
        fail_before_boot(
            state,
            &claimed,
            "max_duration",
            "run exceeded max_duration of active time",
        )
        .await;
        return;
    }

    // 2. A new session on the workspace, at the next generation.
    let prior = match store.load_session(&claimed.prior_session_id) {
        Ok(prior) => prior,
        Err(e) => {
            tracing::error!(%run_id, "resume: the interrupted session is unknown: {e}");
            fail_before_boot(
                state,
                &claimed,
                crate::resume::REASON_RESUME_UNAVAILABLE,
                "the interrupted session could not be found",
            )
            .await;
            return;
        }
    };
    if let Err(e) = super::ensure_cold_resume_workspace(&store, &prior.workspace_id, &run_id) {
        tracing::warn!(%run_id, "resume: workspace not free yet: {e}");
        put_back(state, &run_id, None);
        return;
    }
    // The interrupted session's own dispatch lets go of the run's deferred result when it drops;
    // the new session must not start before that, or it would drop the new deferral too.
    let released = tokio::time::timeout(PRIOR_RELEASE_WAIT, async {
        while store.session_ends.is_tracked(&prior.session_id) {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await;
    if released.is_err() {
        tracing::warn!(%run_id, "resume: the interrupted session is still tracked; retrying");
        put_back(state, &run_id, None);
        return;
    }
    let generation = super::resume_lease_generation(prior.lifecycle_generation, 0);
    let session_id = format!("sess-{}", super::uuid_like());
    match super::boot_resume_session(&manager, &store, &prior, &run_id, &session_id, generation)
        .await
    {
        Ok(()) => {}
        Err(super::ResumeBootError::Superseded(e)) => {
            tracing::warn!(%run_id, "resume: another resume owns the run: {e}");
            return;
        }
        Err(super::ResumeBootError::Failed(e)) => {
            // A boot that fails counts as an attempt of the reason that caused the resume.
            tracing::error!(%run_id, "resume: boot failed: {e}");
            let _ = manager.terminate_session(session_id.clone()).await;
            put_back(state, &run_id, Some(&session_id));
            return;
        }
    }
    let _ = store.release_run_reservation(&run_id);

    // From here a stop of this session is settled by its end below.
    let tracking = crate::session_end::RunTracking::new(store.clone(), &session_id, &run_id);
    run_on_new_session(
        state,
        &claimed,
        &prior.workspace_id,
        &session_id,
        generation,
    )
    .await;
    drop(tracking);
}

async fn run_on_new_session(
    state: &AppState,
    claimed: &ResumeRow,
    workspace_id: &str,
    session_id: &str,
    generation: i64,
) {
    let store = state.store.clone();
    let manager = state.manager.clone();
    let run_id = claimed.run_id.as_str();
    let (prompt, thread_ref, model) = match (
        super::original_prompt_from_run(&store, run_id),
        super::thread_ref_and_model_from_run(&store, run_id),
    ) {
        (Ok(prompt), Ok((thread_ref, model))) => (prompt, thread_ref, model),
        _ => {
            end_new_session_failed(
                state,
                run_id,
                session_id,
                workspace_id,
                "",
                "",
                "",
                crate::resume::REASON_RESUME_UNAVAILABLE,
                "the task's request could not be read",
            )
            .await;
            return;
        }
    };
    let layout = vam_storage::Drive9Layout::new(super::drive9_root());
    let materialized = super::materialized_dir_for_session(session_id);

    // 3. What the guest starts from.
    let mut inputs = super::run_spec_inputs_from_store(&store, run_id).unwrap_or_default();
    inputs = super::remerge_catalog_agent_into_inputs(&store, run_id, inputs);
    let mut task_prompt = prompt.clone();
    let mut dangling = false;
    let mut rounds_before = 0usize;
    match crate::resume::build_crash_context(
        &store,
        &materialized,
        &thread_ref,
        run_id,
        &claimed.reason,
        claimed.total(),
    ) {
        Ok(ctx) => {
            if let Err(e) = super::stage_context(&store, &layout, run_id, session_id, &ctx.built) {
                end_new_session_failed(
                    state,
                    run_id,
                    session_id,
                    workspace_id,
                    &thread_ref,
                    &prompt,
                    &model,
                    crate::resume::REASON_RESUME_UNAVAILABLE,
                    &format!("the conversation could not be handed to the new guest: {e}"),
                )
                .await;
                return;
            }
            let mut holder = Some(std::mem::take(&mut inputs));
            super::set_context_inputs(&mut holder, &ctx.built);
            inputs = holder.unwrap_or_default();
            dangling = ctx.dangling;
            rounds_before = ctx.rounds_before;
        }
        Err(ContextRefusal::NothingDecided) => {
            // The model had decided nothing yet: the task simply starts over on the thread.
            let (spec_json, _, agent_urn) = store
                .get_run_spec_context(run_id)
                .unwrap_or((None, None, None));
            match super::prepare_thread_context(
                &materialized,
                workspace_id,
                &thread_ref,
                run_id,
                agent_urn.as_deref().unwrap_or(""),
                "",
                spec_json.as_deref().unwrap_or("{}"),
                &prompt,
            ) {
                Ok(start) => {
                    let mut holder = Some(std::mem::take(&mut inputs));
                    task_prompt = super::apply_thread_start(
                        &store,
                        &layout,
                        run_id,
                        session_id,
                        &prompt,
                        start,
                        &mut holder,
                    );
                    inputs = holder.unwrap_or_default();
                }
                Err(e) => {
                    end_new_session_failed(
                        state,
                        run_id,
                        session_id,
                        workspace_id,
                        &thread_ref,
                        &prompt,
                        &model,
                        crate::resume::REASON_RESUME_UNAVAILABLE,
                        &format!("the thread could not be prepared: {e}"),
                    )
                    .await;
                    return;
                }
            }
        }
        Err(ContextRefusal::Unavailable(why)) => {
            end_new_session_failed(
                state,
                run_id,
                session_id,
                workspace_id,
                &thread_ref,
                &prompt,
                &model,
                crate::resume::REASON_RESUME_UNAVAILABLE,
                &format!("the conversation cannot be trusted to resume: {why}"),
            )
            .await;
            return;
        }
    }

    // 4. Go.
    let lease = super::build_run_lease(run_id, generation);
    if let Err(e) = store.issue_lease(&lease) {
        tracing::error!(run_id, "resume: lease not issued: {e}");
        end_new_session_failed(
            state,
            run_id,
            session_id,
            workspace_id,
            &thread_ref,
            &prompt,
            &model,
            crate::resume::REASON_RESUME_UNAVAILABLE,
            "the lease could not be issued",
        )
        .await;
        return;
    }
    let session = match store.load_session(session_id) {
        Ok(session) => session,
        Err(e) => {
            tracing::error!(run_id, "resume: session not loaded: {e}");
            return;
        }
    };
    let vsock_path = vam_core::jail::vsock_uds_path(&session.vm_id);
    let timeout_secs = super::guest_task_timeout_secs(&store, run_id);
    let _ = store.begin_active_segment(run_id, super::unix_now_secs());
    if let Some(recorder) = store.thread_recorder(session_id) {
        recorder.seed_for_resume(dangling).await;
        recorder.record_task_start(&prompt, &model, "", true).await;
    }
    let from_round = rounds_before + 1;
    crate::events::append_run_event(
        &store,
        run_id,
        crate::events::TYPE_RUN_RESUMED,
        &serde_json::json!({
            "generation": generation,
            "reason": claimed.reason,
            "fromRound": from_round,
            "resumes": claimed.total(),
        }),
    );
    let _ = store.mark_resume_running(run_id);
    if let Err(e) = super::drive_resumed_guest(
        &manager,
        &store,
        run_id,
        session_id,
        workspace_id,
        &thread_ref,
        &task_prompt,
        &model,
        &lease,
        &vsock_path,
        inputs,
        timeout_secs,
    )
    .await
    {
        tracing::error!(run_id, "resumed run ended with an error: {e}");
    }
}

/// A new session is up but the run cannot go on: it ends `FAILED` with the reason and the
/// session is wound down like any run's.
#[allow(clippy::too_many_arguments)]
async fn end_new_session_failed(
    state: &AppState,
    run_id: &str,
    session_id: &str,
    workspace_id: &str,
    thread_ref: &str,
    prompt: &str,
    model: &str,
    reason: &str,
    message: &str,
) {
    let store = &state.store;
    let result = serde_json::json!({ "error": message, "reason": reason }).to_string();
    let _ = store.end_resume_claimed(run_id);
    if let Err(e) = store.complete_run_with_result(run_id, "FAILED", &result) {
        tracing::error!(run_id, "resumed run not completed: {e}");
    }
    let _ = store.release_run_reservation(run_id);
    super::finalize_run_session(
        &state.manager,
        store.as_ref(),
        run_id,
        session_id,
        workspace_id,
        thread_ref,
        prompt,
        model,
        &super::DispatchOutcome {
            answer: String::new(),
            succeeded: false,
            paused: false,
            interrupted: false,
            spend_micro: None,
        },
    )
    .await;
}
