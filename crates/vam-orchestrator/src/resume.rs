//! Resuming a task after its guest died or `vamd` restarted (E10 Phase 4, AR-4.5).
//!
//! A crashed guest or a restarted `vamd` does not end the task. The session is taken down, the
//! workspace settles at the last complete turn commit (like a stop, decision 12), the
//! conversation keeps what the host recorded (including the model's last decided round), and
//! the run waits in `run_resume` to be picked up on a new session. The resume pass
//! (`api::resume_run`) does that; this module is the host logic around it:
//!
//! * [`classify`] tells a crash from a user stop, a budget timeout and a reported end. **A stop
//!   always wins**: it is checked at classification, atomically with taking over the session's
//!   end ([`crate::session_end::SessionEnds::begin_interrupt`]), and again inside
//!   [`SqliteWorkflowStore::mark_resume_pending`] against the persisted stop mark.
//! * [`interrupt`] is the one way a run becomes `pending`: for a guest crash, for a session a
//!   restarted `vamd` found dead (`session_end::settle_dead_trees`) and for a VM it adopted.
//! * [`build_crash_context`] derives what the new guest starts from: the logged conversation
//!   and the loop counters, none of it from a guest.
//! * [`legacy_ids_block_resume`] is the rule for runs started by a guest that did not name its
//!   calls: they are not resumed.

use std::path::Path;
use std::sync::Arc;

use vam_spec::thread_context::{CrashResume, LoopState};
use vam_storage::thread_log::{kind, read_all_records, Integrity, TreeThreadStore};

use crate::driver::GuestTaskError;
use crate::effect_class::EffectClass;
use crate::manager::SessionManager;
use crate::resume_store::{PendingOutcome, REASON_GUEST_CRASHED, REASON_VAMD_RESTARTED};
use crate::session_end::Disposition;
use crate::thread_context::{BuiltContext, ContextError};
use crate::workflow::SqliteWorkflowStore;

/// What a failed `ExecuteTask` exchange was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The user stopped the run or terminated its session: it ends `stopped`.
    Stopped,
    /// The guest died with no stop recorded, no budget timeout and no reported end.
    Crash,
    /// A timeout, a refusal the guest reported, a dial failure or a malformed answer: the run
    /// fails as it always did.
    Other,
}

/// Tells a crash from the other ways an exchange can end. A stop wins over everything.
pub fn classify(
    store: &SqliteWorkflowStore,
    run_id: &str,
    session_id: &str,
    err: &GuestTaskError,
) -> Verdict {
    if store.run_stop_known(run_id) || store.session_ends.stop_requested(session_id) {
        return Verdict::Stopped;
    }
    match err {
        GuestTaskError::Transport(_) => Verdict::Crash,
        _ => Verdict::Other,
    }
}

/// How an interrupt ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InterruptEnd {
    /// The run waits in `run_resume`; the pass will pick it up.
    Pending,
    /// A stop won (a stop began first, or its mark is persisted). The VM is down and the
    /// workspace settled; the caller ends the run `stopped`.
    Stopped,
    /// The session could not be ended cleanly; the run fails with this result JSON.
    Failed(String),
}

/// The reasons a run ends instead of resuming.
pub const REASON_NOT_RESUMABLE: &str = "not_resumable";
pub const REASON_RESUME_UNAVAILABLE: &str = "resume_unavailable";

fn failed(error: &str, reason: &str) -> InterruptEnd {
    InterruptEnd::Failed(serde_json::json!({ "error": error, "reason": reason }).to_string())
}

/// Takes the session of a run down and leaves the run waiting to be resumed. The session must be
/// tracked (`RunTracking`) so the stop machinery can see it. Used by the run's own dispatch after
/// a guest crash and by [`interrupt_adopted`].
pub async fn interrupt(
    manager: &SessionManager,
    store: &Arc<SqliteWorkflowStore>,
    run_id: &str,
    session_id: &str,
    reason: &str,
) -> InterruptEnd {
    let session = match store.load_session(session_id) {
        Ok(session) => session,
        Err(e) => return failed(&e.to_string(), "resume_interrupt_failed"),
    };
    let _ = store.end_active_segment(run_id, crate::catalog::unix_timestamp());
    // The end of this session is ours unless a stop got there first.
    if !store.session_ends.begin_interrupt(session_id) {
        return InterruptEnd::Stopped;
    }
    if let Err(e) = manager.stop_session_vm(session_id.to_string()).await {
        tracing::error!(run_id, "interrupt: the VM would not go down: {e}");
        return failed(&e.to_string(), "resume_interrupt_failed");
    }
    // The conversation: a host call in flight finishes and is journaled first; the end keeps
    // the decided round open and rolls back like a stop.
    let recorder = match store.thread_recorder(session_id) {
        Some(recorder) => recorder,
        None => Arc::new(crate::thread_recorder::ThreadRecorder::new(
            session_id.to_string(),
            store.clone(),
            crate::manager::drive9_layout(),
        )),
    };
    recorder.record_interrupted_for_resume().await;
    if let Err(e) = manager
        .prepare_session_end(session_id.to_string(), Disposition::Stopped)
        .await
    {
        tracing::warn!(
            "interrupt: the files of the end commit could not be fixed first ({e}); \
             the end plans them itself"
        );
    }
    let term = manager.terminate_session(session_id.to_string()).await;
    store.remove_thread_recorder(session_id);
    {
        let layout = crate::manager::drive9_layout();
        for digest in store.revoke_run_context_grants(run_id) {
            crate::thread_context::remove_context(&layout, session_id, &digest);
        }
    }
    if let Err(e) = term {
        tracing::error!(run_id, "interrupt: the session would not settle: {e}");
        return failed(&e.to_string(), "resume_interrupt_failed");
    }
    if let Ok(Some(kept)) = store.kept_for_session(session_id) {
        return InterruptEnd::Failed(
            serde_json::json!({
                "error": "the workspace could not be committed; the work was kept",
                "reason": crate::turn_commit::RUN_ERROR_STORAGE_COMMIT_FAILED,
                "kept_id": kept.kept_id,
            })
            .to_string(),
        );
    }
    match store.mark_resume_pending(run_id, reason, &session.workspace_id, session_id) {
        Ok(PendingOutcome::Pending) => InterruptEnd::Pending,
        Ok(PendingOutcome::Stopped) => InterruptEnd::Stopped,
        Err(e) => failed(&e.to_string(), "resume_interrupt_failed"),
    }
}

/// `vamd` restarted and adopted a VM whose run was being worked on: nothing awaits its
/// `ExecuteTask` any more, so the VM is taken down and the run resumes on a new one (Q11). A
/// session parked on an approval is not touched.
pub async fn interrupt_adopted(
    manager: &SessionManager,
    store: &Arc<SqliteWorkflowStore>,
) -> usize {
    let Ok(sessions) = store.load_all_sessions() else {
        return 0;
    };
    let mut interrupted = 0;
    for session in sessions {
        if !crate::manager::session_is_warm(&session) {
            continue;
        }
        let Some(run_id) = store
            .find_run_id_for_session(&session.session_id)
            .ok()
            .flatten()
        else {
            continue;
        };
        // Only a run that is RUNNING has an exchange nobody awaits; PAUSED waits for a person.
        if store.get_run_state(&run_id).ok().as_deref() != Some("RUNNING") {
            continue;
        }
        if store.session_ends.is_tracked(&session.session_id) {
            continue;
        }
        let tracking =
            crate::session_end::RunTracking::new(store.clone(), &session.session_id, &run_id);
        let end = interrupt(
            manager,
            store,
            &run_id,
            &session.session_id,
            REASON_VAMD_RESTARTED,
        )
        .await;
        drop(tracking);
        match end {
            InterruptEnd::Pending => interrupted += 1,
            InterruptEnd::Stopped => {
                let _ = store.session_ends.record_run_stop(&run_id);
                if let Err(e) = store.complete_run_with_result(
                    &run_id,
                    "FAILED",
                    &crate::session_end::stopped_result_json(),
                ) {
                    tracing::error!(%run_id, "stopped run not completed after a restart: {e}");
                }
            }
            InterruptEnd::Failed(json) => {
                let _ = store.complete_run_with_result(&run_id, "FAILED", &json);
            }
        }
    }
    interrupted
}

/// Whether the journal holds a call id the host made up for a guest that sent none (an old
/// guest image). Such an id is not stable across a resume, so a replay could not find the call
/// and would send it again: the run is not resumed. Reads are harmless and do not count.
pub fn legacy_ids_block_resume(store: &SqliteWorkflowStore, run_id: &str) -> bool {
    store
        .effects_for_run(run_id)
        .map(|journal| {
            journal
                .rows
                .iter()
                .any(|row| row.call_id.starts_with("legacy-") && row.class != EffectClass::Read)
        })
        // A journal that cannot be read cannot vouch for anything.
        .unwrap_or(true)
}

/// What the log says about a run, for deciding how to resume it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LogFacts {
    /// Model requests the run has had recorded (assistant messages of the run).
    pub model_requests: u32,
    /// The run has a `gap` record: the host could not record something, so the conversation may
    /// be missing a message.
    pub gap: bool,
    pub broken: bool,
}

/// Reads the thread log of `thread_ref` under `materialized` for `run_id`.
pub fn log_facts(materialized: &Path, thread_ref: &str, run_id: &str) -> Result<LogFacts, String> {
    let store = TreeThreadStore::new(materialized);
    let (records, integrity) =
        read_all_records(&store, thread_ref).map_err(|e| format!("thread log unreadable: {e}"))?;
    let mut facts = LogFacts {
        broken: matches!(integrity, Integrity::BrokenAt { .. }),
        ..Default::default()
    };
    for record in records.iter().filter(|r| r.run == run_id) {
        if record.kind == kind::GAP {
            facts.gap = true;
        }
        if record.message().is_some_and(|m| {
            m.role == "assistant" && record.lane == vam_storage::thread_log::MAIN_LANE
        }) {
            facts.model_requests += 1;
        }
    }
    Ok(facts)
}

/// The calls of the conversation's last assistant message that have no result yet, in the order
/// the model made them. Empty when the conversation does not end in a decided round.
pub fn unanswered_calls(messages: &[vam_spec::thread_context::WireMessage]) -> Vec<String> {
    let Some(index) = messages
        .iter()
        .rposition(|m| m.role == "assistant" && !m.tool_calls.is_empty())
    else {
        return Vec::new();
    };
    // Only tool results may follow the round; anything else means the round was closed.
    if messages[index + 1..].iter().any(|m| m.role != "tool") {
        return Vec::new();
    }
    let answered: Vec<&str> = messages[index + 1..]
        .iter()
        .map(|m| m.tool_call_id.as_str())
        .collect();
    messages[index]
        .tool_calls
        .iter()
        .filter(|call| !answered.contains(&call.id.as_str()))
        .map(|call| call.id.clone())
        .collect()
}

/// Marks the steers the log already holds delivered (their guest applied them, then died before
/// acknowledging) and returns the highest delivered steer seq: the new guest asks for messages
/// after it.
pub fn ack_logged_steers(store: &SqliteWorkflowStore, run_id: &str) -> i64 {
    let rows = store.list_inbox_messages(run_id).unwrap_or_default();
    let pending: Vec<(i64, String)> = rows
        .iter()
        .filter(|r| r.mode == "STEER" && r.state == crate::inbox_store::STATE_PENDING)
        .map(|r| (r.seq, r.text.clone()))
        .collect();
    if !pending.is_empty() {
        let logged = crate::inbox::logged_user_messages(store, run_id).unwrap_or_default();
        let already = crate::inbox::steers_in_log(&logged, &pending);
        if !already.is_empty() {
            if let Ok(done) = store.mark_steers_delivered_by_seq(run_id, &already) {
                if !done.is_empty() {
                    crate::trajectory::append_trajectory_event(
                        store,
                        run_id,
                        crate::events::TYPE_INBOX_DELIVERED,
                        &serde_json::json!({ "seqs": done }),
                    );
                }
            }
        }
    }
    store
        .list_inbox_messages(run_id)
        .unwrap_or_default()
        .iter()
        .filter(|r| r.mode == "STEER" && r.state == crate::inbox_store::STATE_DELIVERED)
        .map(|r| r.seq)
        .max()
        .unwrap_or(0)
}

/// What the new guest starts from.
pub struct CrashContext {
    pub built: BuiltContext,
    /// The round number before the one the guest runs first.
    pub rounds_before: usize,
    /// The log ends in a decided round the guest must act on before asking the model again.
    pub dangling: bool,
}

/// Why a run cannot be resumed from its log.
#[derive(Debug)]
pub enum ContextRefusal {
    /// The run has no model request in the log: nothing was decided, it starts over.
    NothingDecided,
    /// The log cannot vouch for the conversation (broken, or a recorded gap).
    Unavailable(String),
}

/// Builds the context a crash-resumed guest starts from, from the thread log under
/// `materialized` (the new session's tree: the workspace as the interrupt settled it).
pub fn build_crash_context(
    store: &SqliteWorkflowStore,
    materialized: &Path,
    thread_ref: &str,
    run_id: &str,
    reason: &str,
    resume: u32,
) -> Result<CrashContext, ContextRefusal> {
    let facts = log_facts(materialized, thread_ref, run_id).map_err(ContextRefusal::Unavailable)?;
    if facts.broken {
        return Err(ContextRefusal::Unavailable(
            "the thread log is broken".to_string(),
        ));
    }
    if facts.model_requests == 0 {
        return Err(ContextRefusal::NothingDecided);
    }
    if facts.gap {
        return Err(ContextRefusal::Unavailable(
            "the host could not record part of the conversation".to_string(),
        ));
    }
    let sources: Vec<Box<dyn crate::thread_context::ContextSource>> = vec![Box::new(
        crate::thread_context::ThreadLogSource::new(materialized, thread_ref),
    )];
    let base = crate::thread_context::build_context(run_id, &sources)
        .map_err(|e: ContextError| ContextRefusal::Unavailable(e.to_string()))?;
    let unanswered = unanswered_calls(&base.context.messages);
    let dangling = !unanswered.is_empty();
    let max_round = store.max_turn_commit_round(run_id).unwrap_or(0) as usize;
    let requests = facts.model_requests as usize;
    // The decided round was request number `requests`: the round the guest runs first.
    let rounds_before = if dangling {
        requests.saturating_sub(1).max(max_round)
    } else {
        requests.max(max_round)
    };
    let metering = store.get_run_metering(run_id).ok().flatten();
    let state = LoopState {
        loaded_skill_ids: Vec::new(),
        iterations_used: rounds_before,
        max_iterations: 0,
        prompt_tokens: metering.as_ref().map_or(0, |m| m.prompt_tokens as i32),
        completion_tokens: metering.as_ref().map_or(0, |m| m.completion_tokens as i32),
        cached_tokens: 0,
        total_tokens: metering.as_ref().map_or(0, |m| m.token_count as i32),
        nudge_count: 0,
        skill_tool_called: false,
        claim_reprompted: false,
        last_answer_candidate: String::new(),
        inbox_seq: ack_logged_steers(store, run_id),
    };
    let crash = CrashResume {
        reason: reason.to_string(),
        resume,
        unanswered,
        state,
    };
    let built = crate::thread_context::with_crash(base, crash)
        .map_err(|e| ContextRefusal::Unavailable(e.to_string()))?;
    Ok(CrashContext {
        built,
        rounds_before,
        dangling,
    })
}

/// `guest_crashed` or `vamd_restarted` for the event and the context.
pub fn is_known_reason(reason: &str) -> bool {
    reason == REASON_GUEST_CRASHED || reason == REASON_VAMD_RESTARTED
}

#[cfg(test)]
mod tests {
    use super::*;
    use vam_spec::thread_context::{WireMessage, WireToolCall};

    fn msg(role: &str, calls: &[&str], id: &str) -> WireMessage {
        WireMessage {
            role: role.into(),
            content: String::new(),
            tool_calls: calls
                .iter()
                .map(|c| WireToolCall {
                    id: (*c).into(),
                    name: "t".into(),
                    arguments: "{}".into(),
                })
                .collect(),
            tool_call_id: id.into(),
        }
    }

    #[test]
    fn a_decided_round_lists_the_calls_without_a_result_in_order() {
        let messages = vec![
            msg("user", &[], ""),
            msg("assistant", &["a", "b", "c"], ""),
            msg("tool", &[], "b"),
        ];
        assert_eq!(unanswered_calls(&messages), vec!["a", "c"]);
    }

    #[test]
    fn a_conversation_that_ends_in_a_result_or_a_closed_round_has_nothing_to_act_on() {
        let answered = vec![
            msg("user", &[], ""),
            msg("assistant", &["a"], ""),
            msg("tool", &[], "a"),
        ];
        assert!(unanswered_calls(&answered).is_empty());
        let closed = vec![msg("assistant", &["a"], ""), msg("user", &[], "")];
        assert!(unanswered_calls(&closed).is_empty());
        assert!(unanswered_calls(&[]).is_empty());
    }

    fn store_with_run(run: &str) -> SqliteWorkflowStore {
        let store = SqliteWorkflowStore::in_memory().unwrap();
        store
            .create_run(&vam_spec::v1::ResourceId { urn: run.into() })
            .unwrap();
        store
    }

    #[test]
    fn a_stop_wins_over_a_transport_loss_however_it_was_recorded() {
        let run = "urn:vam:run:local.vam.dev/acme/classify";
        let store = store_with_run(run);
        let err = GuestTaskError::Transport("eof".into());
        assert_eq!(classify(&store, run, "s", &err), Verdict::Crash);
        // In memory.
        store.session_ends.record_run_stop(run);
        assert_eq!(classify(&store, run, "s", &err), Verdict::Stopped);
        store.session_ends.clear_run_stop(run);
        assert_eq!(classify(&store, run, "s", &err), Verdict::Crash);
        // Persisted only (a restart lost the memory).
        store.persist_run_stop(run).unwrap();
        assert_eq!(classify(&store, run, "s", &err), Verdict::Stopped);
        store.clear_persisted_run_stop(run);
        // A session terminate.
        store.session_ends.track("s");
        assert!(store.session_ends.begin_stop("s"));
        assert_eq!(classify(&store, run, "s", &err), Verdict::Stopped);
    }

    #[test]
    fn only_a_lost_connection_is_a_crash() {
        let run = "urn:vam:run:local.vam.dev/acme/classify-kinds";
        let store = store_with_run(run);
        for err in [
            GuestTaskError::Dial("x".into()),
            GuestTaskError::Timeout("x".into()),
            GuestTaskError::Reported("x".into()),
            GuestTaskError::Protocol("x".into()),
        ] {
            assert_eq!(classify(&store, run, "s", &err), Verdict::Other, "{err:?}");
        }
        assert_eq!(
            classify(&store, run, "s", &GuestTaskError::Transport("x".into())),
            Verdict::Crash
        );
    }

    #[test]
    fn a_stop_that_begins_first_keeps_the_interrupt_out_and_the_other_way_round() {
        let ends = crate::session_end::SessionEnds::default();
        ends.track("a");
        assert!(ends.begin_stop("a"));
        assert!(!ends.begin_interrupt("a"), "a stop that began first wins");
        ends.track("b");
        assert!(ends.begin_interrupt("b"));
        assert!(
            !ends.begin_stop("b"),
            "once the end is taken over a stop waits for the settle"
        );
        assert!(ends.is_finalizing("b"));
        assert!(!ends.begin_interrupt("untracked"));
    }

    #[test]
    fn a_journal_with_a_legacy_id_blocks_a_resume_unless_it_is_only_reads() {
        use crate::effect_store::NewEffect;
        let run = "urn:vam:run:local.vam.dev/acme/legacy";
        let store = store_with_run(run);
        assert!(!legacy_ids_block_resume(&store, run));
        let new = |call: &str, class| NewEffect {
            run_id: run.into(),
            call_id: call.into(),
            parent_call_id: None,
            tool: "t".into(),
            adapter: "hsp".into(),
            class,
            idempotency_key: format!("k-{call}"),
            args_digest: "d".into(),
            approval_id: None,
        };
        store
            .effect_begin(&new("call_1", EffectClass::NonIdempotent))
            .unwrap();
        assert!(!legacy_ids_block_resume(&store, run), "a stable id is fine");
        store
            .effect_begin(&new("legacy-t-d-1", EffectClass::Read))
            .unwrap();
        assert!(
            !legacy_ids_block_resume(&store, run),
            "a read can be redone"
        );
        store
            .effect_begin(&new("legacy-t-d-2", EffectClass::NonIdempotent))
            .unwrap();
        assert!(legacy_ids_block_resume(&store, run));
    }
}
