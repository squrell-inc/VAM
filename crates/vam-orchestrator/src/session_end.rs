//! How a session's workspace ends (AR-2.6, AR-2.8).
//!
//! Three rules hold the ordering together:
//!
//! 1. **One owner of the end.** A run being dispatched is *tracked*. A stop
//!    request kills the VM only ([`crate::manager::TerminateMode::VmOnly`]) and
//!    leaves the tree and the workspace lock; the run's finalizer then writes
//!    the end of the conversation and calls the full terminate, which settles
//!    the workspace. The stop endpoint waits for that settlement. Nothing else
//!    writes into the tree between the VM kill and the settlement.
//! 2. **Stop rolls back to the last complete turn commit** (decision 12): HEAD
//!    becomes that commit's files plus the tree's host `.vam/`, on both
//!    backends (a microVM's commit is a private copy of its image, read at the
//!    stop; an unreadable copy falls back to the previous complete commit). Every
//!    other end commits the live tree as before.
//! 3. **Work is never discarded.** A commit that fails moves the tree to
//!    `drive9/kept/` and records it; nothing in `sessions/` is deleted.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rusqlite::{params, OptionalExtension};
use tokio::sync::watch;
use vam_storage::{PreparedEnd, WorkspaceManager};

use crate::error::OrchestratorError;
use crate::events::TYPE_WORKSPACE_COMMIT_FAILED;
use crate::workflow::SqliteWorkflowStore;

/// How long a stop request waits for the run's finalizer to settle the
/// workspace before it answers `stop_pending`.
pub const STOP_SETTLE_TIMEOUT: Duration = Duration::from_secs(60);

/// What the finalizer writes into the conversation about the files the end
/// commit holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedSummary {
    pub files_hash: String,
    /// `live` (the tree as it is) or `turn_commit` (rolled back to a commit).
    pub base: &'static str,
    pub dropped: vam_storage::DroppedSummary,
}

/// What the run's finalizer fixed before it wrote the end of the task: the
/// files the end commit holds, and how the tree is dealt with afterwards.
#[derive(Debug, Clone)]
pub struct EndPlan {
    pub prepared: PreparedEnd,
    pub disposition: Disposition,
    /// Remove (or, after a stop, move the rolled-back files out of) the session
    /// tree once HEAD reads back. Only a finalizer sets it: never a sweep that
    /// parks a paused or warm session.
    pub reclaim: bool,
}

/// Where a stop's time went on the host (the `stop settled` line).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StopTiming {
    pub prepare_end_ms: u128,
    /// Reading the turn copy into a manifest (zero for a directory guest).
    pub resolve_ms: u128,
    /// Scanning the live image for the rolled-back list.
    pub scan_ms: u128,
}

#[derive(Debug)]
struct EndEntry {
    stop_requested: AtomicBool,
    settle_pending: AtomicBool,
    /// The finalizer has taken over: a stop now waits for it instead.
    finalizing: AtomicBool,
    /// Makes "a stop begins" and "the end is taken over" exclusive: whichever takes it first wins.
    arbiter: Mutex<()>,
    plan: Mutex<Option<EndPlan>>,
    done: watch::Sender<bool>,
}

/// The reason and error a run carries when the user stopped it (AR-3.7, Q7).
pub const STOPPED_REASON: &str = "stopped";
pub const STOPPED_BY_USER: &str = "stopped by user";

/// The stored result of a run the user stopped.
pub fn stopped_result_json() -> String {
    serde_json::json!({ "error": STOPPED_BY_USER, "reason": STOPPED_REASON }).to_string()
}

/// Sessions with a run being dispatched, and what is pending for each.
#[derive(Debug, Default)]
pub struct SessionEnds {
    map: Mutex<HashMap<String, Arc<EndEntry>>>,
    timings: Mutex<HashMap<String, StopTiming>>,
    /// Runs whose user stop is recorded (AR-3.7). Recorded before anything else is
    /// done for the stop, kept until the run's terminal state is written: whatever
    /// the guest reports after it cannot change the reason.
    run_stops: Mutex<HashSet<String>>,
    /// One stop at a time per run: a second stop waits, then reads the end.
    run_stop_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl SessionEnds {
    /// Records the user's stop of `run_id`. `true` for the first record.
    pub fn record_run_stop(&self, run_id: &str) -> bool {
        self.run_stops
            .lock()
            .map(|mut set| set.insert(run_id.to_string()))
            .unwrap_or(false)
    }

    pub fn run_stop_recorded(&self, run_id: &str) -> bool {
        self.run_stops
            .lock()
            .map(|set| set.contains(run_id))
            .unwrap_or(false)
    }

    /// Forgets the record once the run has its terminal state.
    pub fn clear_run_stop(&self, run_id: &str) {
        if let Ok(mut set) = self.run_stops.lock() {
            set.remove(run_id);
        }
        if let Ok(mut locks) = self.run_stop_locks.lock() {
            locks.remove(run_id);
        }
    }

    /// The lock one stop of the run holds while it works.
    pub fn run_stop_lock(&self, run_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self
            .run_stop_locks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        locks
            .entry(run_id.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    /// The user's stop wins over what the guest reports after it: any end but a
    /// success becomes FAILED `stopped`. A run that had already succeeded keeps
    /// its success (Phase 2).
    pub fn apply_stop_precedence(
        &self,
        run_id: &str,
        state: &str,
        result_json: &str,
    ) -> (String, String) {
        if state != "SUCCEEDED" && self.run_stop_recorded(run_id) {
            return ("FAILED".to_string(), stopped_result_json());
        }
        (state.to_string(), result_json.to_string())
    }

    fn entry(&self, session_id: &str) -> Option<Arc<EndEntry>> {
        self.map.lock().ok()?.get(session_id).cloned()
    }

    /// Registers a session whose run is being dispatched. Only a tracked
    /// session can be stopped with a rollback, because only then is there a
    /// finalizer to settle it.
    pub fn track(&self, session_id: &str) {
        if let Ok(mut map) = self.map.lock() {
            map.entry(session_id.to_string()).or_insert_with(|| {
                Arc::new(EndEntry {
                    stop_requested: AtomicBool::new(false),
                    settle_pending: AtomicBool::new(false),
                    finalizing: AtomicBool::new(false),
                    arbiter: Mutex::new(()),
                    plan: Mutex::new(None),
                    done: watch::channel(false).0,
                })
            });
        }
    }

    /// Ends tracking and wakes any waiter.
    pub fn forget(&self, session_id: &str) {
        let removed = self.map.lock().ok().and_then(|mut m| m.remove(session_id));
        if let Some(entry) = removed {
            entry.settle_pending.store(false, Ordering::SeqCst);
            let _ = entry.done.send(true);
        }
    }

    /// Marks the session stopped by request. `false` when the session is not
    /// tracked (no finalizer would settle it); the caller then ends it the
    /// old way.
    pub fn begin_stop(&self, session_id: &str) -> bool {
        let Some(entry) = self.entry(session_id) else {
            return false;
        };
        let _arbiter = entry
            .arbiter
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if entry.finalizing.load(Ordering::SeqCst) {
            // The run has ended; its finalizer is settling the workspace.
            return false;
        }
        entry.stop_requested.store(true, Ordering::SeqCst);
        entry.settle_pending.store(true, Ordering::SeqCst);
        entry.done.send_replace(false);
        true
    }

    /// The run's finalizer takes over the end of the session: the workspace is
    /// settled once, by it. Later stops wait instead of starting another end.
    pub fn begin_finalize(&self, session_id: &str) {
        if let Some(entry) = self.entry(session_id) {
            let _arbiter = entry
                .arbiter
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            entry.finalizing.store(true, Ordering::SeqCst);
            entry.settle_pending.store(true, Ordering::SeqCst);
            entry.done.send_replace(false);
        }
    }

    /// The end of a session whose guest died is taken over, to pick the task up again
    /// (E10 Phase 4). Atomic with [`begin_stop`](Self::begin_stop): `false` when a stop of the
    /// session began first (the stop wins), or when the session is not tracked. On `true` the
    /// session is marked as being ended, so a stop that comes now waits for the settle, and no
    /// new turn commit is taken (`stop_requested` reads true so the end rolls back like a stop).
    pub fn begin_interrupt(&self, session_id: &str) -> bool {
        let Some(entry) = self.entry(session_id) else {
            return false;
        };
        let _arbiter = entry
            .arbiter
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if entry.stop_requested.load(Ordering::SeqCst) || entry.finalizing.load(Ordering::SeqCst) {
            return false;
        }
        entry.finalizing.store(true, Ordering::SeqCst);
        entry.stop_requested.store(true, Ordering::SeqCst);
        entry.settle_pending.store(true, Ordering::SeqCst);
        entry.done.send_replace(false);
        true
    }

    /// A run is being dispatched on this session (a stop of it is settled by its finalizer).
    pub fn is_tracked(&self, session_id: &str) -> bool {
        self.entry(session_id).is_some()
    }

    pub fn is_finalizing(&self, session_id: &str) -> bool {
        self.entry(session_id)
            .map(|e| e.finalizing.load(Ordering::SeqCst))
            .unwrap_or(false)
    }

    /// Remembers where a stop's host time went, for the stop endpoint to log.
    pub fn set_stop_timing(&self, session_id: &str, timing: StopTiming) {
        if let Ok(mut map) = self.timings.lock() {
            map.insert(session_id.to_string(), timing);
        }
    }

    pub fn take_stop_timing(&self, session_id: &str) -> Option<StopTiming> {
        self.timings.lock().ok()?.remove(session_id)
    }

    /// Stores the finalizer's plan for the settle that follows.
    pub fn set_plan(&self, session_id: &str, plan: EndPlan) {
        if let Some(entry) = self.entry(session_id) {
            if let Ok(mut slot) = entry.plan.lock() {
                *slot = Some(plan);
            }
        }
    }

    pub fn take_plan(&self, session_id: &str) -> Option<EndPlan> {
        self.entry(session_id)?.plan.lock().ok()?.take()
    }

    pub fn stop_requested(&self, session_id: &str) -> bool {
        self.entry(session_id)
            .map(|e| e.stop_requested.load(Ordering::SeqCst))
            .unwrap_or(false)
    }

    /// The run finished before the stop took effect: the end is a normal one.
    /// The workspace is still settled (the VM is gone), but live.
    pub fn demote_stop(&self, session_id: &str) {
        if let Some(entry) = self.entry(session_id) {
            entry.stop_requested.store(false, Ordering::SeqCst);
        }
    }

    /// A stop took the VM down and the tree has not been settled yet.
    pub fn settle_pending(&self, session_id: &str) -> bool {
        self.entry(session_id)
            .map(|e| e.settle_pending.load(Ordering::SeqCst))
            .unwrap_or(false)
    }

    /// The workspace of the session has been settled (committed or kept).
    pub fn finish_settle(&self, session_id: &str) {
        if let Some(entry) = self.entry(session_id) {
            entry.settle_pending.store(false, Ordering::SeqCst);
            let _ = entry.done.send(true);
        }
    }

    /// Waits until the session is settled or forgotten. `true` when it is.
    pub async fn wait_settled(&self, session_id: &str, timeout: Duration) -> bool {
        let Some(entry) = self.entry(session_id) else {
            return true;
        };
        let mut rx = entry.done.subscribe();
        if *rx.borrow() || !entry.settle_pending.load(Ordering::SeqCst) {
            return true;
        }
        tokio::time::timeout(timeout, async {
            loop {
                if *rx.borrow() {
                    return true;
                }
                if rx.changed().await.is_err() {
                    // Forgotten: the finalizer is done.
                    return true;
                }
            }
        })
        .await
        .unwrap_or(false)
    }
}

/// Keeps a session tracked for the life of a run's dispatch and forgets it on
/// drop, on every exit path.
pub struct RunTracking {
    store: Arc<SqliteWorkflowStore>,
    session_id: String,
    run_id: String,
}

impl RunTracking {
    pub fn new(store: Arc<SqliteWorkflowStore>, session_id: &str, run_id: &str) -> Self {
        store.session_ends.track(session_id);
        store.defer_run_result(run_id);
        // A stop recorded before this session was tracked (a resume the stop raced
        // with) applies to it at once; the stop handler takes the VM down.
        if store.session_ends.run_stop_recorded(run_id) {
            store.session_ends.begin_stop(session_id);
        }
        Self {
            store,
            session_id: session_id.to_string(),
            run_id: run_id.to_string(),
        }
    }
}

impl Drop for RunTracking {
    fn drop(&mut self) {
        // Whatever path ended the dispatch, a staged result is never lost.
        if let Err(e) = self.store.publish_run_result(&self.run_id, None) {
            tracing::error!("staged result of run {} not published: {e}", self.run_id);
        }
        self.store.session_ends.forget(&self.session_id);
    }
}

/// How a session's workspace is committed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    /// Commit the live tree (success, failure, park, expiry).
    Live,
    /// A stop: HEAD is the last complete turn commit's files plus the host
    /// `.vam/`, on either backend.
    Stopped,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettleOutcome {
    Committed(String),
    /// The commit failed and the tree is now at `drive9/kept/<kept_id>`.
    Kept {
        kept_id: String,
        error: String,
    },
    /// The commit failed and the tree could not be moved either. It is still
    /// under `sessions/`, untouched.
    KeepFailed {
        error: String,
    },
    /// Nothing was materialized.
    NoTree,
}

/// A tree kept after a failed commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeptRecord {
    pub session_id: String,
    pub workspace_id: String,
    pub run_id: String,
    pub kept_id: String,
    pub error: String,
    pub attempts: u32,
}

/// The workspace on both sides of a run (a receipt pin for E12).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunWorkspaceHeads {
    pub run_id: String,
    pub head_before: String,
    pub files_hash_before: String,
    pub head_after: Option<String>,
    pub files_hash_after: Option<String>,
}

impl SqliteWorkflowStore {
    /// From now until [`publish_run_result`](Self::publish_run_result), the
    /// run's terminal state is staged, not written.
    pub fn defer_run_result(&self, run_id: &str) {
        if let Ok(mut map) = self.deferred_runs.lock() {
            map.entry(run_id.to_string()).or_insert(None);
        }
    }

    /// Stages a terminal result if the run is deferring. `true` when staged.
    pub(crate) fn stage_run_result(&self, run_id: &str, state: &str, result_json: &str) -> bool {
        match self.deferred_runs.lock() {
            Ok(mut map) => match map.get_mut(run_id) {
                Some(slot) => {
                    *slot = Some((state.to_string(), result_json.to_string()));
                    true
                }
                None => false,
            },
            Err(_) => false,
        }
    }

    /// The result staged for a run, if any.
    pub fn staged_run_result(&self, run_id: &str) -> Option<(String, String)> {
        self.deferred_runs.lock().ok()?.get(run_id)?.clone()
    }

    /// Ends deferral and writes the staged terminal result (or `replacement`,
    /// for a run whose workspace could not be committed). Returns the state
    /// written, or `None` when nothing was staged and no replacement given.
    pub fn publish_run_result(
        &self,
        run_id: &str,
        replacement: Option<(&str, &str)>,
    ) -> Result<Option<String>, OrchestratorError> {
        let staged = self
            .deferred_runs
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?
            .remove(run_id)
            .flatten();
        let (state, json) = match (replacement, staged) {
            (Some((state, json)), _) => (state.to_string(), json.to_string()),
            (None, Some(staged)) => staged,
            (None, None) => return Ok(None),
        };
        self.complete_run_with_result_now(run_id, &state, &json)?;
        Ok(Some(state))
    }

    /// Records the HEAD a run started from. The first call wins: a resumed run
    /// keeps its original start.
    pub fn record_run_head_before(
        &self,
        run_id: &str,
        session_id: &str,
        head_before: &str,
        files_hash_before: &str,
    ) -> Result<(), OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        connection.execute(
            "INSERT OR IGNORE INTO run_workspace_heads
                (run_id, session_id, head_before, files_hash_before, updated_at)
             VALUES (?1, ?2, ?3, ?4, strftime('%s','now'))",
            params![run_id, session_id, head_before, files_hash_before],
        )?;
        Ok(())
    }

    /// Records the HEAD a run left, after its end commit.
    pub fn record_run_head_after(
        &self,
        run_id: &str,
        head_after: &str,
        files_hash_after: &str,
    ) -> Result<(), OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        connection.execute(
            "UPDATE run_workspace_heads SET head_after = ?2, files_hash_after = ?3,
                updated_at = strftime('%s','now') WHERE run_id = ?1",
            params![run_id, head_after, files_hash_after],
        )?;
        Ok(())
    }

    pub fn run_workspace_heads(
        &self,
        run_id: &str,
    ) -> Result<Option<RunWorkspaceHeads>, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        Ok(connection
            .query_row(
                "SELECT run_id, head_before, files_hash_before, head_after, files_hash_after
                 FROM run_workspace_heads WHERE run_id = ?1",
                [run_id],
                |row| {
                    Ok(RunWorkspaceHeads {
                        run_id: row.get(0)?,
                        head_before: row.get(1)?,
                        files_hash_before: row.get(2)?,
                        head_after: row.get(3)?,
                        files_hash_after: row.get(4)?,
                    })
                },
            )
            .optional()?)
    }

    pub fn record_workspace_kept(
        &self,
        session_id: &str,
        workspace_id: &str,
        run_id: &str,
        kept_id: &str,
        error: &str,
    ) -> Result<(), OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        connection.execute(
            "INSERT INTO workspace_kept
                (session_id, workspace_id, run_id, kept_id, error, attempts, state, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 1, 'kept', strftime('%s','now'))
             ON CONFLICT(session_id) DO UPDATE SET
                kept_id = excluded.kept_id, error = excluded.error,
                attempts = attempts + 1",
            params![session_id, workspace_id, run_id, kept_id, error],
        )?;
        Ok(())
    }

    pub fn kept_for_session(
        &self,
        session_id: &str,
    ) -> Result<Option<KeptRecord>, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        Ok(connection
            .query_row(
                "SELECT session_id, workspace_id, run_id, kept_id, error, attempts
                 FROM workspace_kept WHERE session_id = ?1",
                [session_id],
                |row| {
                    Ok(KeptRecord {
                        session_id: row.get(0)?,
                        workspace_id: row.get(1)?,
                        run_id: row.get(2)?,
                        kept_id: row.get(3)?,
                        error: row.get(4)?,
                        attempts: row.get::<_, i64>(5)? as u32,
                    })
                },
            )
            .optional()?)
    }
}

/// The error text with the host's Drive9 root replaced, so an event or a run
/// result never names a host path.
pub fn scrub_host_paths(text: &str, wm: &WorkspaceManager) -> String {
    let root = wm.layout().sessions_dir();
    match root.parent().and_then(|p| p.to_str()) {
        Some(root) if !root.is_empty() => text.replace(root, "<drive9>"),
        _ => text.to_string(),
    }
}

/// Payload of `vam.v1.workspace.commit_failed`. It names a kept id, never a
/// host path.
pub fn commit_failed_payload(
    workspace_id: &str,
    session_id: &str,
    run_id: &str,
    kept_id: Option<&str>,
    error: &str,
    phase: &str,
) -> serde_json::Value {
    serde_json::json!({
        "workspace_id": workspace_id,
        "session_id": session_id,
        "run_id": run_id,
        "kept_id": kept_id,
        "error": error,
        "phase": phase,
        "reason": crate::turn_commit::RUN_ERROR_STORAGE_COMMIT_FAILED,
    })
}

/// Emits the commit-failed event on the run's outbox, if the run is known.
pub fn emit_commit_failed(store: &SqliteWorkflowStore, run_id: &str, payload: &serde_json::Value) {
    if run_id.is_empty() {
        return;
    }
    if let Err(e) =
        store.append_outbox_event(run_id, TYPE_WORKSPACE_COMMIT_FAILED, &payload.to_string())
    {
        tracing::error!("failed to emit {TYPE_WORKSPACE_COMMIT_FAILED}: {e}");
    }
}

/// Fixes the files of a session's end commit: the one place a plan is made,
/// whether the run's finalizer asks (`prepare_session_end`) or a settle with no
/// plan does (a dead tree at startup).
///
/// * A live end reads the live tree (or image).
/// * A stop resolves the newest complete turn commit to a manifest (a microVM's
///   deferred `image:<round>` token is read from its private copy; an unreadable
///   copy falls back to the previous complete commit, with `commit_failed`
///   raised for the bad one) and takes it as the base. With no complete commit
///   the base is the HEAD the run started from.
///
/// Blocking. An error here means the files of the end could not be fixed; the
/// settle then keeps the whole session tree.
pub fn plan_end(
    store: &SqliteWorkflowStore,
    wm: &WorkspaceManager,
    session_id: &str,
    workspace_id: &str,
    disposition: Disposition,
    reclaim: bool,
) -> Result<(EndPlan, PreparedSummary), vam_storage::Drive9Error> {
    let started = std::time::Instant::now();
    let mut timing = StopTiming::default();
    let (base, label) = if disposition == Disposition::Stopped {
        let run_id = store
            .find_run_id_for_session(session_id)
            .ok()
            .flatten()
            .unwrap_or_default();
        let resolving = std::time::Instant::now();
        let manifest = if run_id.is_empty() {
            None
        } else {
            crate::turn_commit::resolve_stop_base(store, wm, &run_id, session_id, workspace_id)?
        };
        timing.resolve_ms = resolving.elapsed().as_millis();
        (vam_storage::EndBase::TurnCommit(manifest), "turn_commit")
    } else {
        (vam_storage::EndBase::Live, "live")
    };
    let prepared = wm.prepare_end(session_id, workspace_id, base)?;
    timing.scan_ms = prepared.scan_ms;
    timing.prepare_end_ms = started.elapsed().as_millis();
    if disposition == Disposition::Stopped {
        store.session_ends.set_stop_timing(session_id, timing);
    }
    let summary = PreparedSummary {
        files_hash: prepared.files_hash.clone(),
        base: label,
        dropped: prepared.dropped.clone(),
    };
    Ok((
        EndPlan {
            prepared,
            disposition,
            reclaim,
        },
        summary,
    ))
}

/// Commits (or, if that fails, keeps) the session's tree and releases the
/// workspace lock. Blocking: callers run it on a blocking thread.
///
/// If the run's finalizer fixed an [`EndPlan`] (the files and their hash, written
/// into the conversation before this), HEAD is composed from it. Otherwise the
/// end is planned here, the old way. A failed commit never calls `discard`: the
/// tree moves to `kept/`, a `workspace_kept` row and the `commit_failed` event
/// record it, and the run's finalizer ends the run `storage_commit_failed`.
pub fn settle_workspace(
    store: &SqliteWorkflowStore,
    wm: &WorkspaceManager,
    session_id: &str,
    workspace_id: &str,
    disposition: Disposition,
) -> SettleOutcome {
    let plan = store.session_ends.take_plan(session_id);
    settle_with_plan(
        store,
        wm,
        session_id,
        workspace_id,
        disposition,
        plan,
        false,
    )
}

/// [`settle_workspace`] with the plan given, and `reclaim_unplanned` for an end
/// nobody planned (a dead session settled at startup): the tree is reclaimed
/// after the commit like a finalizer's.
pub fn settle_with_plan(
    store: &SqliteWorkflowStore,
    wm: &WorkspaceManager,
    session_id: &str,
    workspace_id: &str,
    disposition: Disposition,
    plan: Option<EndPlan>,
    reclaim_unplanned: bool,
) -> SettleOutcome {
    let materialized = wm.layout().session_dir(session_id).join("materialized");
    if !materialized.exists() {
        if let Err(e) = wm.discard(session_id, workspace_id) {
            tracing::error!(
                "failed to discard unmaterialized workspace '{workspace_id}' for session '{session_id}': {e:?}"
            );
        }
        return SettleOutcome::NoTree;
    }
    let run_id = store
        .find_run_id_for_session(session_id)
        .ok()
        .flatten()
        .unwrap_or_default();

    let stopped_rollback = disposition == Disposition::Stopped;
    // A stop nobody planned (a dead session settled at startup) is planned here,
    // the same way the finalizer plans one. If that fails the commit fails and
    // the tree is kept whole.
    let mut planning_error = None;
    let plan = match plan {
        None if stopped_rollback => {
            match plan_end(
                store,
                wm,
                session_id,
                workspace_id,
                disposition,
                reclaim_unplanned,
            ) {
                Ok((plan, _summary)) => Some(plan),
                Err(e) => {
                    planning_error = Some(e);
                    None
                }
            }
        }
        other => other,
    };
    // The manifest whose files the end commit holds, when it is not the live
    // tree: what a stop's rolled-back files are measured against.
    let mut base_for_diff: Option<String> = None;
    let committed = if let Some(error) = planning_error {
        Err(error)
    } else if let Some(plan) = plan.as_ref() {
        base_for_diff = plan.prepared.base_manifest.clone();
        wm.finish_end(session_id, workspace_id, &plan.prepared)
            .and_then(|hash| wm.release_lock(workspace_id, session_id).map(|_| hash))
    } else {
        wm.commit_and_release(session_id, workspace_id)
    };

    match committed {
        Ok(hash) => {
            let _ = store.update_active_manifest_hash(session_id, &hash);
            if stopped_rollback && !run_id.is_empty() {
                let _ = store.fail_pending_turn_commits(
                    &run_id,
                    session_id,
                    "the task was stopped before the commit completed",
                );
            }
            if !run_id.is_empty() {
                let files_hash = wm
                    .manifest_files_hash(workspace_id, &hash)
                    .unwrap_or_default();
                let _ = store.record_run_head_after(&run_id, &hash, &files_hash);
            }
            let reclaim = plan
                .as_ref()
                .map(|p| p.reclaim)
                .unwrap_or(reclaim_unplanned);
            if reclaim {
                match wm.reclaim_session_tree(
                    session_id,
                    workspace_id,
                    &hash,
                    base_for_diff.as_deref(),
                    stopped_rollback,
                    plan.as_ref().and_then(|p| p.prepared.rolled_back.clone()),
                    vam_storage::ROLLED_BACK_KEEP_CAP_BYTES,
                ) {
                    Ok(report) => {
                        if report.kept_files > 0 || report.skipped_over_cap > 0 {
                            tracing::info!(
                                session = session_id,
                                kept = report.kept_files,
                                kept_bytes = report.kept_bytes,
                                skipped = report.skipped_over_cap,
                                "rolled-back files moved to kept/"
                            );
                        }
                    }
                    Err(e) => tracing::warn!(
                        session = session_id,
                        "session tree not reclaimed (it stays for gc): {e}"
                    ),
                }
            }
            SettleOutcome::Committed(hash)
        }
        Err(commit_err) => {
            let error = scrub_host_paths(&commit_err.to_string(), wm);
            tracing::error!(
                "workspace commit failed for session '{session_id}' ({workspace_id}): {error}; keeping the tree"
            );
            match wm.keep_session(session_id, workspace_id) {
                Ok(kept_id) => {
                    if let Err(e) = store.record_workspace_kept(
                        session_id,
                        workspace_id,
                        &run_id,
                        &kept_id,
                        &error,
                    ) {
                        tracing::error!("failed to record the kept tree '{kept_id}': {e}");
                    }
                    emit_commit_failed(
                        store,
                        &run_id,
                        &commit_failed_payload(
                            workspace_id,
                            session_id,
                            &run_id,
                            Some(&kept_id),
                            &error,
                            "session_end",
                        ),
                    );
                    SettleOutcome::Kept { kept_id, error }
                }
                Err(keep_err) => {
                    let keep_error = scrub_host_paths(&keep_err.to_string(), wm);
                    tracing::error!(
                        "could not keep the tree of session '{session_id}' either: {keep_error}; it stays under sessions/"
                    );
                    emit_commit_failed(
                        store,
                        &run_id,
                        &commit_failed_payload(
                            workspace_id,
                            session_id,
                            &run_id,
                            None,
                            &format!("{error}; keeping the tree failed: {keep_error}"),
                            "session_end",
                        ),
                    );
                    SettleOutcome::KeepFailed {
                        error: format!("{error}; {keep_error}"),
                    }
                }
            }
        }
    }
}

/// What the host needs to end a parked run itself.
#[derive(Debug, Clone)]
pub struct ParkedEnd {
    pub session_id: String,
    pub run_id: String,
    pub user_prompt: String,
    pub thread_ref: String,
    pub model: String,
    /// `None`: the user stopped the run. `Some(reason)`: the host ends it as failed for that
    /// reason (a run that crashed or restarted too often to be resumed again).
    pub failure: Option<String>,
}

/// Ends the user-stopped run that is parked on an approval whose VM is gone, on the host
/// (AR-3.7): no VM is booted. The conversation gets "Not run: stopped by user" for the
/// calls the approval held back and `task_end: stopped`, HEAD is the last complete turn
/// commit's files plus the conversation, and the session tree is reclaimed.
///
/// The tree to write into is the workspace's host `.vam/` read from HEAD
/// ([`WorkspaceManager::materialize_host_dir`]), unless the session still holds the
/// workspace lock over a tree (a stop took its VM down first): that tree is settled as it is.
pub async fn end_parked_on_host(
    store: &Arc<SqliteWorkflowStore>,
    wm: &Arc<WorkspaceManager>,
    end: ParkedEnd,
) -> Result<SettleOutcome, OrchestratorError> {
    let session = store.load_session(&end.session_id)?;
    if crate::manager::session_is_warm(&session) {
        return Err(OrchestratorError::Conflict(format!(
            "session '{}' is still live",
            end.session_id
        )));
    }
    let workspace_id = session.workspace_id.clone();
    let core =
        |e: vam_storage::Drive9Error| OrchestratorError::Core(scrub_host_paths(&e.to_string(), wm));
    // 1. The tree the conversation is written into.
    {
        let (wm, sid, ws) = (wm.clone(), end.session_id.clone(), workspace_id.clone());
        tokio::task::spawn_blocking(move || -> Result<(), vam_storage::Drive9Error> {
            let tree = wm.layout().session_dir(&sid).join("materialized");
            let holds = wm.lock_holder(&ws)?.as_deref() == Some(sid.as_str());
            if holds && tree.is_dir() {
                return Ok(());
            }
            wm.materialize_host_dir(&sid, &ws).map(|_| ())
        })
        .await
        .map_err(|_| OrchestratorError::Core("Tokio task panic".to_string()))?
        .map_err(core)?;
    }
    // 2. The files of the end commit, fixed first so the conversation can name their hash.
    let (plan, summary) = {
        let (store, wm, sid, ws) = (
            store.clone(),
            wm.clone(),
            end.session_id.clone(),
            workspace_id.clone(),
        );
        tokio::task::spawn_blocking(move || {
            plan_end(&store, &wm, &sid, &ws, Disposition::Stopped, true)
        })
        .await
        .map_err(|_| OrchestratorError::Core("Tokio task panic".to_string()))?
        .map_err(core)?
    };
    // 3. The end of the conversation.
    let recorder = crate::thread_recorder::ThreadRecorder::new(
        end.session_id.clone(),
        store.clone(),
        wm.layout().clone(),
    );
    if end.failure.is_none() {
        recorder.mark_user_stop();
    }
    recorder.set_end_commit(&summary.files_hash, summary.base, &summary.dropped);
    let log_head = match end.failure.as_deref() {
        None => recorder.record_stop().await,
        Some(reason) => recorder.record_interrupted("failed", reason).await,
    };
    // 4. The timeline turn every end writes.
    {
        let tree = wm
            .layout()
            .session_dir(&end.session_id)
            .join("materialized");
        let events = store
            .read_outbox_for_run(&end.run_id, 0)
            .unwrap_or_default();
        let trajectory = crate::trajectory::summarize_trajectory(&events);
        let head_before = store
            .run_workspace_heads(&end.run_id)
            .ok()
            .flatten()
            .map(|h| h.head_before)
            .unwrap_or_default();
        let log_range = match (recorder.run_first_seq(), &log_head) {
            (Some(from), Some(head)) => Some(vam_storage::TurnLogRange {
                seq_from: from,
                seq_to: head.seq,
                head_digest: head.digest.clone(),
            }),
            _ => None,
        };
        let turn_workspace = vam_storage::TurnWorkspace {
            head_before: head_before.clone(),
            files_hash: summary.files_hash.clone(),
        };
        if let Err(e) = vam_storage::append_turn_full(
            &tree,
            &end.thread_ref,
            &end.run_id,
            &end.user_prompt,
            if end.failure.is_none() {
                "(the task was stopped)"
            } else {
                "(the task did not finish)"
            },
            if end.model.is_empty() {
                "unknown"
            } else {
                &end.model
            },
            &head_before,
            None,
            Some(&trajectory),
            log_range.as_ref(),
            Some(&turn_workspace),
        ) {
            tracing::error!("[orchestrator] timeline append failed: {e}");
        }
    }
    // 5. HEAD, the lock, the tree.
    let (store_c, wm_c, sid, ws) = (
        store.clone(),
        wm.clone(),
        end.session_id.clone(),
        workspace_id,
    );
    let outcome = tokio::task::spawn_blocking(move || {
        settle_with_plan(
            &store_c,
            &wm_c,
            &sid,
            &ws,
            Disposition::Stopped,
            Some(plan),
            false,
        )
    })
    .await
    .map_err(|_| OrchestratorError::Core("Tokio task panic".to_string()))?;
    store.remove_thread_recorder(&end.session_id);
    Ok(outcome)
}

/// How long a dead session's VMM gets to exit before its image is read.
pub const DEAD_VMM_EXIT_WAIT: Duration = Duration::from_secs(5);

/// Waits until the VMM `pid` is gone, but only while it is still the process the
/// session recorded (`start_time` is field 22 of `/proc/<pid>/stat`): a reused
/// pid is somebody else. A zombie counts as gone. `true` when it is gone.
pub async fn wait_for_vmm_exit(pid: u32, start_time: Option<u64>, timeout: Duration) -> bool {
    let started = std::time::Instant::now();
    loop {
        if !vmm_still_running(pid, start_time) {
            return true;
        }
        if started.elapsed() >= timeout {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn vmm_still_running(pid: u32, start_time: Option<u64>) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    // `pid (comm) S ... starttime ...`: fields after the last ')'.
    let Some(rest) = stat.rsplit(')').next() else {
        return false;
    };
    let fields: Vec<&str> = rest.split_whitespace().collect();
    if matches!(fields.first().copied(), Some("Z") | Some("X")) {
        return false;
    }
    match (
        start_time,
        fields.get(19).and_then(|f| f.parse::<u64>().ok()),
    ) {
        (Some(recorded), Some(actual)) => recorded == actual,
        // Without a recorded start time the pid cannot be told from a reused one.
        _ => false,
    }
}

/// What a restart does with the run of a dead session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeadRunFate {
    /// The run was not being worked on (it had ended, or it is parked).
    NotRunning,
    /// It waits to be picked up on a new session.
    Resume,
    /// The user's stop is marked: it ends `stopped`.
    Stopped,
    /// It never began (nothing in the conversation): it ends as it always did.
    Failed,
}

/// Settles the trees [`crate::reconcile`] found over a dead session's lock.
///
/// A run that was being worked on waits to be resumed on a new session (`run_resume`, reason
/// `vamd_restarted`): its workspace is rolled back to its last complete turn commit like a
/// stop (decision 12) and its conversation keeps the model's last decided round open. One whose
/// stop is marked ends `stopped`; one that never began (nothing in its conversation) ends
/// `FAILED` `vamd_restarted` as before. Any other
/// tree (the run had ended, or there was no run) is committed as it is. A
/// commit that fails keeps the tree. Either way the tree is reclaimed once
/// HEAD reads back. Returns how many sessions were settled.
pub async fn settle_dead_trees(
    store: &Arc<SqliteWorkflowStore>,
    wm: &Arc<WorkspaceManager>,
    trees: &[crate::reconcile::DeadTree],
) -> usize {
    let mut settled = 0;
    for tree in trees {
        if let Some(pid) = tree.vmm_pid {
            if !wait_for_vmm_exit(pid, tree.vmm_start_time, DEAD_VMM_EXIT_WAIT).await {
                tracing::warn!(
                    session = %tree.session_id,
                    "the dead session's VMM is still running after {:?}; settling anyway",
                    DEAD_VMM_EXIT_WAIT
                );
            }
        }
        let run_id = store
            .find_run_id_for_session(&tree.session_id)
            .ok()
            .flatten();
        let running = run_id.as_ref().is_some_and(|run| {
            store
                .get_run_state(run)
                .map(|state| {
                    !matches!(
                        state.to_ascii_uppercase().as_str(),
                        "SUCCEEDED" | "FAILED" | "CANCELLED" | "TIMED_OUT" | "REJECTED" | "PAUSED"
                    )
                })
                .unwrap_or(false)
        });
        // What becomes of a run that was being worked on (E10 Phase 4): it waits to be picked up
        // again on a new session, unless the user's stop is marked or it never began (nothing in
        // the log), which end it as before.
        let mut fate = DeadRunFate::NotRunning;
        if running {
            let recorder = crate::thread_recorder::ThreadRecorder::new(
                tree.session_id.clone(),
                store.clone(),
                wm.layout().clone(),
            );
            let stopped = run_id
                .as_deref()
                .is_some_and(|run| store.run_stop_known(run));
            if stopped {
                recorder.mark_user_stop();
                recorder.record_stop().await;
                fate = DeadRunFate::Stopped;
            } else if recorder.run_has_task_start().await {
                recorder.record_interrupted_for_resume().await;
                fate = DeadRunFate::Resume;
            } else {
                recorder
                    .record_interrupted("interrupted", "vamd_restarted")
                    .await;
                fate = DeadRunFate::Failed;
            }
        }
        let disposition = if running {
            Disposition::Stopped
        } else {
            Disposition::Live
        };
        let (store_c, wm_c, session, workspace) = (
            store.clone(),
            wm.clone(),
            tree.session_id.clone(),
            tree.workspace_id.clone(),
        );
        let outcome = tokio::task::spawn_blocking(move || {
            settle_with_plan(
                &store_c,
                &wm_c,
                &session,
                &workspace,
                disposition,
                None,
                true,
            )
        })
        .await;
        match outcome {
            Ok(SettleOutcome::Kept { ref kept_id, .. }) => {
                tracing::error!(
                    session = %tree.session_id,
                    "the dead session's workspace could not be committed; kept as '{kept_id}'"
                );
            }
            Ok(_) => {}
            Err(e) => {
                tracing::error!(session = %tree.session_id, "settling a dead tree panicked: {e}");
                continue;
            }
        }
        if let Some(run) = run_id.as_ref().filter(|_| running) {
            let kept = matches!(
                outcome,
                Ok(SettleOutcome::Kept { .. }) | Ok(SettleOutcome::KeepFailed { .. })
            );
            if fate == DeadRunFate::Resume && kept {
                fate = DeadRunFate::Failed;
            }
            if fate == DeadRunFate::Resume {
                match store.mark_resume_pending(
                    run,
                    crate::resume_store::REASON_VAMD_RESTARTED,
                    &tree.workspace_id,
                    &tree.session_id,
                ) {
                    Ok(crate::resume_store::PendingOutcome::Pending) => {
                        // Its active time stops while it waits.
                        let _ = store.end_active_segment(run, crate::catalog::unix_timestamp());
                        tracing::info!(%run, "the run waits to be resumed after the restart");
                    }
                    Ok(crate::resume_store::PendingOutcome::Stopped) => {
                        fate = DeadRunFate::Stopped;
                    }
                    Err(e) => {
                        tracing::error!(%run, "run not marked for resume after the restart: {e}");
                        fate = DeadRunFate::Failed;
                    }
                }
            }
            let json = match fate {
                DeadRunFate::Resume | DeadRunFate::NotRunning => None,
                DeadRunFate::Stopped => Some(stopped_result_json()),
                DeadRunFate::Failed => Some(
                    serde_json::json!({
                        "error": "vamd restarted while the task was running",
                        "reason": "vamd_restarted",
                    })
                    .to_string(),
                ),
            };
            if let Some(json) = json {
                if fate == DeadRunFate::Stopped {
                    store.session_ends.record_run_stop(run);
                }
                if let Err(e) = store.complete_run_with_result(run, "FAILED", &json) {
                    tracing::error!(%run, "run not completed after the restart: {e}");
                }
            }
        }
        settled += 1;
    }
    settled
}

#[cfg(test)]
mod tests {
    use super::*;
    use vam_storage::Drive9Backend;

    #[test]
    fn only_a_tracked_session_can_be_stopped() {
        let ends = SessionEnds::default();
        assert!(!ends.begin_stop("s1"));
        ends.track("s1");
        assert!(!ends.stop_requested("s1"));
        assert!(ends.begin_stop("s1"));
        assert!(ends.stop_requested("s1"));
        assert!(ends.settle_pending("s1"));
        ends.finish_settle("s1");
        assert!(!ends.settle_pending("s1"));
        ends.forget("s1");
        assert!(!ends.stop_requested("s1"));
    }

    #[test]
    fn a_finished_run_demotes_a_stop_to_a_live_end() {
        let ends = SessionEnds::default();
        ends.track("s1");
        ends.begin_stop("s1");
        ends.demote_stop("s1");
        assert!(!ends.stop_requested("s1"));
        assert!(
            ends.settle_pending("s1"),
            "the tree still has to be settled"
        );
    }

    #[tokio::test]
    async fn a_waiter_wakes_on_settle_and_times_out_otherwise() {
        let ends = Arc::new(SessionEnds::default());
        // Not tracked: nothing to wait for.
        assert!(ends.wait_settled("none", Duration::from_millis(10)).await);

        ends.track("s1");
        ends.begin_stop("s1");
        assert!(!ends.wait_settled("s1", Duration::from_millis(30)).await);

        let waiter = {
            let ends = ends.clone();
            tokio::spawn(async move { ends.wait_settled("s1", Duration::from_secs(5)).await })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        ends.finish_settle("s1");
        assert!(waiter.await.expect("join"));
    }

    #[tokio::test]
    async fn forgetting_a_session_wakes_its_waiters() {
        let ends = Arc::new(SessionEnds::default());
        ends.track("s1");
        ends.begin_stop("s1");
        let waiter = {
            let ends = ends.clone();
            tokio::spawn(async move { ends.wait_settled("s1", Duration::from_secs(5)).await })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        ends.forget("s1");
        assert!(waiter.await.expect("join"));
    }

    #[test]
    fn the_commit_failed_payload_names_a_kept_id_not_a_path() {
        let payload = commit_failed_payload("ws", "s1", "run", Some("s1"), "disk full", "x");
        assert_eq!(payload["kept_id"], "s1");
        assert_eq!(payload["reason"], "storage_commit_failed");
        assert!(!payload.to_string().contains('/'));
    }

    const RUN: &str = "urn:vam:run:local.vam.dev/acme/run-settle";

    struct Rig {
        _dir: tempfile::TempDir,
        store: SqliteWorkflowStore,
        wm: WorkspaceManager,
    }

    fn rig(backend: Drive9Backend) -> Rig {
        let dir = tempfile::tempdir().expect("dir");
        let wm = match backend {
            Drive9Backend::Directory => {
                vam_storage::open_workspace_manager_with(dir.path(), backend).expect("wm")
            }
            Drive9Backend::Ext4Image => {
                let layout = vam_storage::Drive9Config::new(dir.path()).layout();
                let chunks = vam_storage::ChunkStore::new(layout.clone());
                let manifests = vam_storage::ManifestManager::new(layout.clone(), chunks.clone());
                let cache = vam_storage::CacheManager::new(layout.clone(), chunks.clone(), 1 << 24)
                    .expect("cache");
                WorkspaceManager::new(
                    layout,
                    chunks,
                    manifests,
                    cache,
                    Box::new(vam_storage::MockImageIo),
                )
            }
        };
        let store = SqliteWorkflowStore::in_memory().expect("store");
        store
            .persist_run_spec(
                RUN,
                r#"{"inputs":{"prompt":"go"}}"#,
                Some("s1"),
                "urn:vam:agent:test",
                None,
                "User::\"tester\"",
            )
            .expect("run");
        Rig {
            _dir: dir,
            store,
            wm,
        }
    }

    fn materialize(r: &Rig, session: &str, ws: &str) -> std::path::PathBuf {
        r.wm.materialize(
            session,
            ws,
            &vam_storage::StorageProfile::new("t".into(), 8 * 1024 * 1024),
            None,
            None,
            None,
        )
        .expect("materialize")
    }

    fn head_paths(r: &Rig, ws: &str) -> Vec<String> {
        let layout = r.wm.layout().clone();
        let chunks = vam_storage::ChunkStore::new(layout.clone());
        let manifests = vam_storage::ManifestManager::new(layout, chunks);
        let head = manifests.read_head(ws).expect("head").expect("some head");
        manifests
            .get_manifest(ws, &head)
            .expect("manifest")
            .files
            .into_iter()
            .map(|f| f.path)
            .collect()
    }

    #[test]
    fn a_live_end_commits_the_tree_and_releases_the_lock() {
        let r = rig(Drive9Backend::Directory);
        let tree = materialize(&r, "s1", "ws");
        std::fs::write(tree.join("workspace/a.txt"), b"a").expect("write");
        let outcome = settle_workspace(&r.store, &r.wm, "s1", "ws", Disposition::Live);
        assert!(
            matches!(outcome, SettleOutcome::Committed(_)),
            "{outcome:?}"
        );
        assert!(head_paths(&r, "ws").contains(&"workspace/a.txt".to_string()));
        assert_eq!(r.wm.lock_holder("ws").expect("lock"), None);
    }

    #[test]
    fn a_stop_rolls_files_back_to_the_last_complete_turn_commit() {
        let r = rig(Drive9Backend::Directory);
        let tree = materialize(&r, "s1", "ws");
        std::fs::write(tree.join("workspace/note.txt"), b"kiwi-42").expect("write");
        let manifest = r.wm.snapshot_tree_manifest("s1", "ws").expect("snapshot");
        r.store
            .record_turn_commit_pending(RUN, "s1", 1, 3, &"aa".repeat(32), &"bb".repeat(32))
            .expect("pending");
        r.store
            .set_turn_commit_manifest(RUN, 1, &manifest)
            .expect("manifest");
        r.store
            .match_turn_commit_tail(RUN, &"bb".repeat(32), 4, &"cc".repeat(32))
            .expect("boundary");
        // A second round that never completed, and a cut-off write.
        r.store
            .record_turn_commit_pending(RUN, "s1", 2, 5, &"aa".repeat(32), &"dd".repeat(32))
            .expect("pending 2");
        std::fs::write(tree.join("workspace/partial.txt"), b"half").expect("write");
        std::fs::create_dir_all(tree.join(".vam/threads/t")).expect("dir");
        std::fs::write(tree.join(".vam/threads/t/timeline.jsonl"), b"{}\n").expect("write");

        let outcome = settle_workspace(&r.store, &r.wm, "s1", "ws", Disposition::Stopped);
        assert!(
            matches!(outcome, SettleOutcome::Committed(_)),
            "{outcome:?}"
        );
        let paths = head_paths(&r, "ws");
        assert!(paths.contains(&"workspace/note.txt".to_string()));
        assert!(!paths.contains(&"workspace/partial.txt".to_string()));
        assert!(paths.contains(&".vam/threads/t/timeline.jsonl".to_string()));
        assert_eq!(r.wm.lock_holder("ws").expect("lock"), None);
        assert_eq!(
            r.store.turn_commit(RUN, 2).expect("read").map(|(s, _)| s),
            Some(crate::turn_commit::TurnCommitState::Failed)
        );
        assert_eq!(
            r.store.turn_commit(RUN, 1).expect("read").map(|(s, _)| s),
            Some(crate::turn_commit::TurnCommitState::Complete)
        );
    }

    #[test]
    fn a_stop_with_no_complete_commit_keeps_the_head_files_and_the_conversation() {
        let r = rig(Drive9Backend::Directory);
        let tree = materialize(&r, "s0", "ws");
        std::fs::write(tree.join("workspace/before.txt"), b"1").expect("write");
        settle_workspace(&r.store, &r.wm, "s0", "ws", Disposition::Live);
        let tree = materialize(&r, "s1", "ws");
        std::fs::write(tree.join("workspace/after.txt"), b"2").expect("write");
        std::fs::create_dir_all(tree.join(".vam")).expect("dir");
        std::fs::write(tree.join(".vam/log"), b"x").expect("write");
        settle_workspace(&r.store, &r.wm, "s1", "ws", Disposition::Stopped);
        let paths = head_paths(&r, "ws");
        assert!(paths.contains(&"workspace/before.txt".to_string()));
        assert!(!paths.contains(&"workspace/after.txt".to_string()));
        assert!(paths.contains(&".vam/log".to_string()));
    }

    #[test]
    fn the_image_backend_rolls_back_on_a_stop_like_the_directory_backend() {
        // No complete turn commit: the stop keeps the HEAD the run started from
        // (here none, so no files) and the host `.vam/`, not the live image.
        let r = rig(Drive9Backend::Ext4Image);
        let tree = materialize(&r, "s1", "ws");
        std::fs::create_dir_all(tree.join(".vam")).expect("dir");
        std::fs::write(tree.join(".vam/log"), b"x").expect("write");
        let outcome = settle_workspace(&r.store, &r.wm, "s1", "ws", Disposition::Stopped);
        assert!(
            matches!(outcome, SettleOutcome::Committed(_)),
            "{outcome:?}"
        );
        let paths = head_paths(&r, "ws");
        // The mock image holds one file; it is not in HEAD after a stop.
        assert!(!paths.contains(&"extracted.txt".to_string()), "{paths:?}");
        assert!(paths.contains(&".vam/log".to_string()));
    }

    #[test]
    fn a_failed_commit_keeps_the_tree_records_it_and_raises_the_event_without_a_path() {
        let r = rig(Drive9Backend::Directory);
        let tree = materialize(&r, "s1", "ws");
        std::fs::write(tree.join("workspace/work.txt"), b"precious").expect("write");
        // HEAD cannot be read: the commit fails.
        std::fs::create_dir_all(r.wm.layout().head_path("ws")).expect("break head");

        let outcome = settle_workspace(&r.store, &r.wm, "s1", "ws", Disposition::Live);
        let SettleOutcome::Kept { kept_id, error } = outcome else {
            panic!("expected the tree to be kept, got {outcome:?}");
        };
        assert_eq!(kept_id, "s1");
        assert!(!error.contains(r.wm.layout().sessions_dir().to_str().expect("utf8")));
        // Nothing was deleted: the work is under kept/, outside sessions/.
        assert!(!r.wm.layout().session_dir("s1").exists());
        assert_eq!(
            std::fs::read(
                r.wm.layout()
                    .kept_session_dir("s1")
                    .join("materialized/workspace/work.txt")
            )
            .expect("kept file"),
            b"precious"
        );
        assert_eq!(r.wm.lock_holder("ws").expect("lock"), None);
        let row = r.store.kept_for_session("s1").expect("read").expect("row");
        assert_eq!(
            (row.kept_id.as_str(), row.workspace_id.as_str()),
            ("s1", "ws")
        );
        let events = r.store.read_outbox_for_run(RUN, 0).expect("events");
        let raised: Vec<_> = events
            .iter()
            .filter(|e| e.event_type == TYPE_WORKSPACE_COMMIT_FAILED)
            .collect();
        assert_eq!(raised.len(), 1);
        let payload: serde_json::Value = serde_json::from_str(&raised[0].payload).expect("json");
        assert_eq!(payload["kept_id"], "s1");
        assert_eq!(payload["reason"], "storage_commit_failed");
        assert_eq!(payload["workspace_id"], "ws");
        assert!(!raised[0]
            .payload
            .contains(r.wm.layout().sessions_dir().to_str().expect("utf8")));
        // A later `gc` leaves the kept tree alone.
        r.wm.gc().expect("gc");
        assert!(r.wm.layout().kept_session_dir("s1").exists());
    }

    #[test]
    fn nothing_materialized_settles_as_no_tree() {
        let r = rig(Drive9Backend::Directory);
        let outcome = settle_workspace(&r.store, &r.wm, "ghost", "ws", Disposition::Live);
        assert_eq!(outcome, SettleOutcome::NoTree);
    }

    #[test]
    fn a_finalizing_session_cannot_be_stopped_and_keeps_its_plan_for_the_settle() {
        let ends = SessionEnds::default();
        ends.track("s1");
        ends.begin_finalize("s1");
        assert!(ends.is_finalizing("s1"));
        assert!(ends.settle_pending("s1"));
        assert!(
            !ends.begin_stop("s1"),
            "the run has ended; nothing to interrupt"
        );
        assert!(!ends.stop_requested("s1"));
        assert!(ends.take_plan("s1").is_none());
        ends.set_plan(
            "s1",
            EndPlan {
                prepared: PreparedEnd {
                    base_manifest: None,
                    files_hash: "f".repeat(64),
                    dropped: Default::default(),
                    rolled_back: None,
                    refresh_template: true,
                    scan_ms: 0,
                },
                disposition: Disposition::Live,
                reclaim: true,
            },
        );
        assert_eq!(
            ends.take_plan("s1").unwrap().prepared.files_hash,
            "f".repeat(64)
        );
        assert!(ends.take_plan("s1").is_none(), "a plan is used once");
    }

    #[test]
    fn a_planned_live_end_writes_head_records_both_sides_and_reclaims_the_tree() {
        let r = rig(Drive9Backend::Directory);
        let tree = materialize(&r, "s1", "ws");
        r.store
            .record_run_head_before(
                RUN,
                "s1",
                "",
                &vam_storage::MerkleManifest::empty_files_hash(),
            )
            .unwrap();
        std::fs::write(tree.join("workspace/a.txt"), b"a").unwrap();
        let prepared =
            r.wm.prepare_end("s1", "ws", vam_storage::EndBase::Live)
                .unwrap();
        // The conversation is written after the files are fixed.
        std::fs::create_dir_all(tree.join(".vam/threads/t")).unwrap();
        std::fs::write(
            tree.join(".vam/threads/t/timeline.jsonl"),
            prepared.files_hash.as_bytes(),
        )
        .unwrap();
        let files_hash = prepared.files_hash.clone();
        let outcome = settle_with_plan(
            &r.store,
            &r.wm,
            "s1",
            "ws",
            Disposition::Live,
            Some(EndPlan {
                prepared,
                disposition: Disposition::Live,
                reclaim: true,
            }),
            false,
        );
        let SettleOutcome::Committed(head) = outcome else {
            panic!("{outcome:?}");
        };
        let heads = r.store.run_workspace_heads(RUN).unwrap().unwrap();
        assert_eq!(heads.head_after.as_deref(), Some(head.as_str()));
        assert_eq!(heads.files_hash_after.as_deref(), Some(files_hash.as_str()));
        assert_eq!(heads.head_before, "");
        assert!(head_paths(&r, "ws").contains(&".vam/threads/t/timeline.jsonl".to_string()));
        assert!(
            !r.wm.layout().session_dir("s1").exists(),
            "the tree is reclaimed"
        );
        assert_eq!(r.wm.lock_holder("ws").unwrap(), None);
    }

    #[test]
    fn a_planned_stop_keeps_only_the_rolled_back_files_and_a_sweep_never_reclaims() {
        let r = rig(Drive9Backend::Directory);
        let tree = materialize(&r, "s1", "ws");
        std::fs::write(tree.join("workspace/note.txt"), b"kiwi").unwrap();
        let turn = r.wm.snapshot_tree_manifest("s1", "ws").unwrap();
        std::fs::write(tree.join("workspace/partial.txt"), b"half").unwrap();
        let prepared =
            r.wm.prepare_end("s1", "ws", vam_storage::EndBase::TurnCommit(Some(turn)))
                .unwrap();
        assert_eq!(prepared.dropped.entries.len(), 1);
        let outcome = settle_with_plan(
            &r.store,
            &r.wm,
            "s1",
            "ws",
            Disposition::Stopped,
            Some(EndPlan {
                prepared,
                disposition: Disposition::Stopped,
                reclaim: true,
            }),
            false,
        );
        assert!(
            matches!(outcome, SettleOutcome::Committed(_)),
            "{outcome:?}"
        );
        let paths = head_paths(&r, "ws");
        assert!(paths.contains(&"workspace/note.txt".to_string()));
        assert!(!paths.contains(&"workspace/partial.txt".to_string()));
        let kept = r.wm.layout().kept_session_dir("s1").join("rolled-back");
        assert_eq!(
            std::fs::read(kept.join("workspace/partial.txt")).unwrap(),
            b"half"
        );
        assert!(!kept.join("workspace/note.txt").exists());
        assert!(!r.wm.layout().session_dir("s1").exists());

        // An end nobody planned (a sweep parking a session) leaves the tree.
        let tree = materialize(&r, "s2", "ws2");
        std::fs::write(tree.join("workspace/b.txt"), b"b").unwrap();
        let outcome = settle_workspace(&r.store, &r.wm, "s2", "ws2", Disposition::Live);
        assert!(matches!(outcome, SettleOutcome::Committed(_)));
        assert!(r.wm.layout().session_dir("s2").exists());
    }

    #[test]
    fn a_tree_that_cannot_be_committed_is_kept_whole_and_never_reclaimed() {
        let r = rig(Drive9Backend::Directory);
        let tree = materialize(&r, "s1", "ws");
        std::fs::write(tree.join("workspace/work.txt"), b"precious").unwrap();
        let prepared =
            r.wm.prepare_end("s1", "ws", vam_storage::EndBase::Live)
                .unwrap();
        std::fs::create_dir_all(r.wm.layout().head_path("ws")).unwrap();
        let outcome = settle_with_plan(
            &r.store,
            &r.wm,
            "s1",
            "ws",
            Disposition::Live,
            Some(EndPlan {
                prepared,
                disposition: Disposition::Live,
                reclaim: true,
            }),
            false,
        );
        assert!(matches!(outcome, SettleOutcome::Kept { .. }), "{outcome:?}");
        assert!(r
            .wm
            .layout()
            .kept_session_dir("s1")
            .join("materialized/workspace/work.txt")
            .exists());
    }

    #[test]
    fn a_runs_terminal_state_is_staged_and_published_once_with_an_optional_replacement() {
        let store = SqliteWorkflowStore::in_memory().unwrap();
        store
            .persist_run_spec(RUN, "{}", Some("s1"), "urn:vam:agent:t", None, "u")
            .unwrap();
        store.update_run_state(RUN, "RUNNING").unwrap();
        store.defer_run_result(RUN);
        store
            .complete_run_with_result(RUN, "SUCCEEDED", r#"{"answer":"ok"}"#)
            .unwrap();
        assert_eq!(
            store.get_run_state(RUN).unwrap(),
            "RUNNING",
            "not reported yet"
        );
        assert_eq!(
            store.staged_run_result(RUN),
            Some(("SUCCEEDED".into(), r#"{"answer":"ok"}"#.into()))
        );
        // The workspace could not be committed: the run does not report success.
        let published = store
            .publish_run_result(
                RUN,
                Some(("FAILED", r#"{"reason":"storage_commit_failed"}"#)),
            )
            .unwrap();
        assert_eq!(published.as_deref(), Some("FAILED"));
        assert_eq!(store.get_run_state(RUN).unwrap(), "FAILED");
        let events = store.read_outbox_for_run(RUN, 0).unwrap();
        assert!(
            !events
                .iter()
                .any(|e| e.event_type == "vam.v1.run.state" && e.payload == "SUCCEEDED"),
            "SUCCEEDED was never announced"
        );
        // Deferral ended: results are written directly again; nothing to publish twice.
        assert_eq!(store.publish_run_result(RUN, None).unwrap(), None);
        store.complete_run_with_result(RUN, "FAILED", "{}").unwrap();
    }

    #[test]
    fn dropping_the_tracking_publishes_a_staged_result() {
        let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
        store
            .persist_run_spec(RUN, "{}", Some("s1"), "urn:vam:agent:t", None, "u")
            .unwrap();
        store.update_run_state(RUN, "RUNNING").unwrap();
        {
            let _tracking = RunTracking::new(store.clone(), "s1", RUN);
            store
                .complete_run_with_result(RUN, "SUCCEEDED", "{}")
                .unwrap();
            assert_eq!(store.get_run_state(RUN).unwrap(), "RUNNING");
        }
        assert_eq!(store.get_run_state(RUN).unwrap(), "SUCCEEDED");
        assert!(!store.session_ends.is_finalizing("s1"));
    }

    #[test]
    fn run_workspace_heads_keep_the_first_start_and_fill_in_the_end() {
        let store = SqliteWorkflowStore::in_memory().unwrap();
        assert!(store.run_workspace_heads(RUN).unwrap().is_none());
        store.record_run_head_before(RUN, "s1", "h0", "f0").unwrap();
        store
            .record_run_head_before(RUN, "s2", "other", "other")
            .unwrap();
        let heads = store.run_workspace_heads(RUN).unwrap().unwrap();
        assert_eq!(
            (heads.head_before.as_str(), heads.files_hash_before.as_str()),
            ("h0", "f0")
        );
        assert_eq!(heads.head_after, None);
        store.record_run_head_after(RUN, "h1", "f1").unwrap();
        let heads = store.run_workspace_heads(RUN).unwrap().unwrap();
        assert_eq!(heads.head_after.as_deref(), Some("h1"));
        assert_eq!(heads.files_hash_after.as_deref(), Some("f1"));
    }

    /// A session that died holding a tree, whose task had begun (its `task_start` is in the
    /// conversation): one finished round committed, one cut off.
    async fn dead_tree_of_a_begun_task(
        r: Rig,
    ) -> (
        tempfile::TempDir,
        Arc<SqliteWorkflowStore>,
        Arc<WorkspaceManager>,
        Vec<crate::reconcile::DeadTree>,
    ) {
        r.store
            .create_session("s1", None, "vm-s1", "ws", "default")
            .unwrap();
        let store = Arc::new(r.store);
        let wm = Arc::new(r.wm);
        let tree = wm
            .materialize(
                "s1",
                "ws",
                &vam_storage::StorageProfile::new("t".into(), 8 * 1024 * 1024),
                None,
                None,
                None,
            )
            .unwrap();
        std::fs::write(tree.join("workspace/note.txt"), b"kiwi").unwrap();
        let turn = wm.snapshot_tree_manifest("s1", "ws").unwrap();
        store
            .record_turn_commit_pending(RUN, "s1", 1, 3, &"aa".repeat(32), &"bb".repeat(32))
            .unwrap();
        store.set_turn_commit_manifest(RUN, 1, &turn).unwrap();
        store
            .match_turn_commit_tail(RUN, &"bb".repeat(32), 4, &"cc".repeat(32))
            .unwrap();
        std::fs::write(tree.join("workspace/partial.txt"), b"half").unwrap();
        store.update_run_state(RUN, "RUNNING").unwrap();
        // The task began: its conversation holds `task_start`.
        let recorder = crate::thread_recorder::ThreadRecorder::new(
            "s1".to_string(),
            store.clone(),
            wm.layout().clone(),
        );
        recorder.record_task_start("go", "mock", "d", false).await;
        assert!(recorder.run_has_task_start().await);
        let trees = vec![crate::reconcile::DeadTree {
            session_id: "s1".into(),
            workspace_id: "ws".into(),
            vmm_pid: None,
            vmm_start_time: None,
        }];
        (r._dir, store, wm, trees)
    }

    #[tokio::test]
    async fn a_restart_leaves_a_running_task_waiting_to_resume_with_its_work_rolled_back() {
        let (_dir, store, wm, trees) =
            dead_tree_of_a_begun_task(rig(Drive9Backend::Directory)).await;
        assert_eq!(settle_dead_trees(&store, &wm, &trees).await, 1);

        // Not failed: the run keeps running and waits for the resume pass.
        assert_eq!(store.get_run_state(RUN).unwrap(), "RUNNING");
        assert!(store.get_run_result_json(RUN).unwrap().is_none());
        let row = store.resume_row(RUN).unwrap().expect("a resume row");
        assert_eq!(
            (row.state, row.reason.as_str(), row.workspace_id.as_str()),
            (
                crate::resume_store::ResumeState::Pending,
                "vamd_restarted",
                "ws"
            )
        );
        assert_eq!(row.prior_session_id, "s1");
        assert_eq!(
            store.resume_holding_workspace("ws").unwrap().as_deref(),
            Some(RUN)
        );
        // The files are the last finished step; the lock is free for the new session.
        let head = wm.head("ws").unwrap().unwrap();
        let layout = wm.layout().clone();
        let manifest =
            vam_storage::ManifestManager::new(layout.clone(), vam_storage::ChunkStore::new(layout))
                .get_manifest("ws", &head)
                .unwrap();
        let paths: Vec<&str> = manifest.files.iter().map(|f| f.path.as_str()).collect();
        assert!(paths.contains(&"workspace/note.txt"));
        assert!(!paths.contains(&"workspace/partial.txt"), "{paths:?}");
        assert_eq!(wm.lock_holder("ws").unwrap(), None);
    }

    #[tokio::test]
    async fn a_restart_after_the_users_stop_ends_the_run_stopped_and_never_resumes_it() {
        let (_dir, store, wm, trees) =
            dead_tree_of_a_begun_task(rig(Drive9Backend::Directory)).await;
        // The stop was recorded, then vamd died before the run's end was written. The memory of
        // the stop is gone with the process; the database keeps it.
        store.persist_run_stop(RUN).unwrap();
        assert!(!store.session_ends.run_stop_recorded(RUN));
        assert_eq!(settle_dead_trees(&store, &wm, &trees).await, 1);

        assert_eq!(store.get_run_state(RUN).unwrap(), "FAILED");
        let result = store.get_run_result_json(RUN).unwrap().unwrap();
        assert!(result.contains("stopped"), "{result}");
        assert!(store.resume_row(RUN).unwrap().is_none());
        assert!(store.resume_holding_workspace("ws").unwrap().is_none());
    }

    #[tokio::test]
    async fn a_restart_leaves_a_parked_run_parked_and_resumes_nothing() {
        let (_dir, store, wm, trees) =
            dead_tree_of_a_begun_task(rig(Drive9Backend::Directory)).await;
        store.update_run_state(RUN, "PAUSED").unwrap();
        assert_eq!(settle_dead_trees(&store, &wm, &trees).await, 1);
        assert_eq!(store.get_run_state(RUN).unwrap(), "PAUSED");
        assert!(store.resume_row(RUN).unwrap().is_none());
    }

    #[tokio::test]
    async fn dead_trees_are_settled_a_running_run_ends_vamd_restarted_and_work_is_rolled_back() {
        let r = rig(Drive9Backend::Directory);
        let store = Arc::new(r.store);
        let wm = Arc::new(r.wm);
        // The session was working: one finished (committed) round, one cut off.
        let tree = wm
            .materialize(
                "s1",
                "ws",
                &vam_storage::StorageProfile::new("t".into(), 8 * 1024 * 1024),
                None,
                None,
                None,
            )
            .unwrap();
        std::fs::write(tree.join("workspace/note.txt"), b"kiwi").unwrap();
        let turn = wm.snapshot_tree_manifest("s1", "ws").unwrap();
        store
            .record_turn_commit_pending(RUN, "s1", 1, 3, &"aa".repeat(32), &"bb".repeat(32))
            .unwrap();
        store.set_turn_commit_manifest(RUN, 1, &turn).unwrap();
        store
            .match_turn_commit_tail(RUN, &"bb".repeat(32), 4, &"cc".repeat(32))
            .unwrap();
        std::fs::write(tree.join("workspace/partial.txt"), b"half").unwrap();
        store.update_run_state(RUN, "RUNNING").unwrap();

        let trees = vec![crate::reconcile::DeadTree {
            session_id: "s1".into(),
            workspace_id: "ws".into(),
            vmm_pid: None,
            vmm_start_time: None,
        }];
        assert_eq!(settle_dead_trees(&store, &wm, &trees).await, 1);

        assert_eq!(store.get_run_state(RUN).unwrap(), "FAILED");
        let result = store.get_run_result_json(RUN).unwrap().unwrap();
        assert!(result.contains("vamd_restarted"), "{result}");
        let head = wm.head("ws").unwrap().unwrap();
        let layout = wm.layout().clone();
        let manifest =
            vam_storage::ManifestManager::new(layout.clone(), vam_storage::ChunkStore::new(layout))
                .get_manifest("ws", &head)
                .unwrap();
        let paths: Vec<&str> = manifest.files.iter().map(|f| f.path.as_str()).collect();
        assert!(paths.contains(&"workspace/note.txt"));
        assert!(!paths.contains(&"workspace/partial.txt"), "{paths:?}");
        assert_eq!(wm.lock_holder("ws").unwrap(), None);
        assert!(!wm.layout().session_dir("s1").exists());
        assert!(wm
            .layout()
            .kept_session_dir("s1")
            .join("rolled-back/workspace/partial.txt")
            .exists());
        // A second pass finds nothing to do.
        assert_eq!(settle_dead_trees(&store, &wm, &[]).await, 0);
    }

    #[tokio::test]
    async fn a_dead_tree_whose_run_had_ended_is_committed_as_it_is() {
        let r = rig(Drive9Backend::Directory);
        let store = Arc::new(r.store);
        let wm = Arc::new(r.wm);
        let tree = wm
            .materialize(
                "s1",
                "ws",
                &vam_storage::StorageProfile::new("t".into(), 8 * 1024 * 1024),
                None,
                None,
                None,
            )
            .unwrap();
        std::fs::write(tree.join("workspace/done.txt"), b"d").unwrap();
        store.update_run_state(RUN, "SUCCEEDED").unwrap();
        let trees = vec![crate::reconcile::DeadTree {
            session_id: "s1".into(),
            workspace_id: "ws".into(),
            vmm_pid: None,
            vmm_start_time: None,
        }];
        assert_eq!(settle_dead_trees(&store, &wm, &trees).await, 1);
        assert_eq!(
            store.get_run_state(RUN).unwrap(),
            "SUCCEEDED",
            "its state is not rewritten"
        );
        let head = wm.head("ws").unwrap().unwrap();
        let layout = wm.layout().clone();
        let files: Vec<String> =
            vam_storage::ManifestManager::new(layout.clone(), vam_storage::ChunkStore::new(layout))
                .get_manifest("ws", &head)
                .unwrap()
                .files
                .into_iter()
                .map(|f| f.path)
                .collect();
        assert!(files.contains(&"workspace/done.txt".to_string()));
    }

    #[test]
    fn a_recorded_user_stop_wins_over_what_the_guest_reports_after_it() {
        let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
        store
            .persist_run_spec(RUN, "{}", Some("s1"), "urn:vam:agent:t", None, "u")
            .unwrap();
        store.update_run_state(RUN, "RUNNING").unwrap();
        {
            let _tracking = RunTracking::new(store.clone(), "s1", RUN);
            assert!(store.session_ends.record_run_stop(RUN));
            assert!(!store.session_ends.record_run_stop(RUN), "recorded once");
            // The guest's loop detector, then its iteration cap, report after the stop.
            store
                .complete_run_with_result(RUN, "FAILED", r#"{"error":"loop_detected: run x5"}"#)
                .unwrap();
            store
                .complete_run_with_result(RUN, "FAILED", r#"{"error":"iteration cap"}"#)
                .unwrap();
            assert_eq!(
                store.staged_run_result(RUN),
                Some(("FAILED".into(), stopped_result_json()))
            );
        }
        assert_eq!(store.get_run_state(RUN).unwrap(), "FAILED");
        let result: serde_json::Value =
            serde_json::from_str(&store.get_run_result_json(RUN).unwrap().unwrap()).unwrap();
        assert_eq!(result["error"], "stopped by user");
        assert_eq!(result["reason"], "stopped");
        assert!(
            !store.session_ends.run_stop_recorded(RUN),
            "the record ends with the run's terminal state"
        );
    }

    #[test]
    fn a_run_that_had_already_succeeded_keeps_its_success_after_a_stop() {
        let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
        store
            .persist_run_spec(RUN, "{}", Some("s1"), "urn:vam:agent:t", None, "u")
            .unwrap();
        store.update_run_state(RUN, "RUNNING").unwrap();
        store.session_ends.record_run_stop(RUN);
        store
            .complete_run_with_result(RUN, "SUCCEEDED", r#"{"answer":"ok"}"#)
            .unwrap();
        assert_eq!(store.get_run_state(RUN).unwrap(), "SUCCEEDED");
    }

    #[test]
    fn a_stop_recorded_before_a_resume_is_tracked_applies_to_its_session() {
        let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
        store.session_ends.record_run_stop(RUN);
        let _tracking = RunTracking::new(store.clone(), "s9", RUN);
        assert!(store.session_ends.stop_requested("s9"));
        assert!(store.session_ends.settle_pending("s9"));
    }

    #[tokio::test]
    async fn a_second_stop_waits_for_the_first() {
        let ends = Arc::new(SessionEnds::default());
        let first = ends.run_stop_lock(RUN);
        let held = first.lock().await;
        let ends2 = ends.clone();
        let second = tokio::spawn(async move {
            let lock = ends2.run_stop_lock(RUN);
            let _g = lock.lock().await;
        });
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(!second.is_finished());
        drop(held);
        second.await.unwrap();
    }

    #[tokio::test]
    async fn a_parked_run_the_user_stopped_is_ended_on_the_host_with_no_vm() {
        let r = rig(Drive9Backend::Directory);
        let store = Arc::new(r.store);
        let wm = Arc::new(r.wm);
        store
            .create_session("s1", None, "vm-s1", "ws", "default")
            .unwrap();
        // One finished round (note.txt), then work the park committed but no turn commit holds.
        let tree = wm
            .materialize(
                "s1",
                "ws",
                &vam_storage::StorageProfile::new("t".into(), 8 * 1024 * 1024),
                None,
                None,
                None,
            )
            .unwrap();
        std::fs::write(tree.join("workspace/note.txt"), b"kiwi").unwrap();
        let turn = wm.snapshot_tree_manifest("s1", "ws").unwrap();
        store
            .record_turn_commit_pending(RUN, "s1", 1, 3, &"aa".repeat(32), &"bb".repeat(32))
            .unwrap();
        store.set_turn_commit_manifest(RUN, 1, &turn).unwrap();
        store
            .match_turn_commit_tail(RUN, &"bb".repeat(32), 4, &"cc".repeat(32))
            .unwrap();
        std::fs::write(tree.join("workspace/partial.txt"), b"half").unwrap();
        // The keep-warm window lapsed: the session was terminated and committed live.
        wm.commit_and_release("s1", "ws").unwrap();
        for state in [
            crate::session::ObservedState::Booting,
            crate::session::ObservedState::Running,
            crate::session::ObservedState::Terminated,
        ] {
            store.update_observed_state("s1", state).unwrap();
        }
        store.update_run_state(RUN, "PAUSED").unwrap();
        assert!(head_paths_of(&wm, "ws").contains(&"workspace/partial.txt".to_string()));

        let outcome = end_parked_on_host(
            &store,
            &wm,
            ParkedEnd {
                session_id: "s1".into(),
                run_id: RUN.into(),
                user_prompt: "go".into(),
                thread_ref: "main".into(),
                model: "mock".into(),
                failure: None,
            },
        )
        .await
        .unwrap();
        assert!(
            matches!(outcome, SettleOutcome::Committed(_)),
            "{outcome:?}"
        );
        let paths = head_paths_of(&wm, "ws");
        assert!(
            paths.contains(&"workspace/note.txt".to_string()),
            "{paths:?}"
        );
        assert!(
            !paths.contains(&"workspace/partial.txt".to_string()),
            "files are at the last complete turn commit: {paths:?}"
        );
        assert_eq!(wm.lock_holder("ws").unwrap(), None);
        assert!(!wm.layout().session_dir("s1").exists(), "tree reclaimed");
        // The conversation ends `stopped`, written by the host.
        let layout = wm.layout().clone();
        let chunks = vam_storage::ChunkStore::new(layout.clone());
        let manifests = vam_storage::ManifestManager::new(layout, chunks.clone());
        use vam_storage::thread_log::{HeadThreadReader, ThreadLogReader};
        let records = HeadThreadReader::new(&manifests, &chunks, "ws")
            .read("main", 0, usize::MAX)
            .unwrap()
            .records;
        let end = records.iter().rev().find(|r| r.kind == "task_end").unwrap();
        assert_eq!(end.body.as_ref().unwrap()["outcome"], "stopped");
    }

    fn head_paths_of(wm: &WorkspaceManager, ws: &str) -> Vec<String> {
        let layout = wm.layout().clone();
        let chunks = vam_storage::ChunkStore::new(layout.clone());
        let manifests = vam_storage::ManifestManager::new(layout, chunks);
        let head = manifests.read_head(ws).unwrap().unwrap();
        manifests
            .get_manifest(ws, &head)
            .unwrap()
            .files
            .into_iter()
            .map(|f| f.path)
            .collect()
    }

    #[test]
    fn a_kept_row_round_trips_and_counts_attempts() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        store
            .record_workspace_kept("s1", "ws", "run", "s1", "disk full")
            .expect("record");
        let kept = store.kept_for_session("s1").expect("read").expect("row");
        assert_eq!(kept.kept_id, "s1");
        assert_eq!(kept.attempts, 1);
        store
            .record_workspace_kept("s1", "ws", "run", "s1-1", "still full")
            .expect("record again");
        let kept = store.kept_for_session("s1").expect("read").expect("row");
        assert_eq!(kept.kept_id, "s1-1");
        assert_eq!(kept.attempts, 2);
        assert!(store.kept_for_session("none").expect("read").is_none());
    }
}

/// A stop on the image backend, against real ext4 images: the guest is simulated
/// by writing into the session image with `debugfs -w`.
#[cfg(test)]
mod ext4_stop_tests {
    use super::*;
    use crate::turn_commit::{image_token, ImageTurnCommitter, TurnCommitRequest, TurnCommitter};
    use vam_storage::{
        CacheManager, ChunkStore, Drive9Config, E2fsImageIo, ManifestManager, StorageProfile,
    };

    const RUN: &str = "urn:vam:run:local.vam.dev/acme/run-ext4-stop";
    const SESSION: &str = "s1";

    fn have_tools() -> bool {
        ["mke2fs", "debugfs"].iter().all(|t| {
            std::process::Command::new(t)
                .arg("-V")
                .output()
                .map(|o| o.status.success() || !o.stderr.is_empty())
                .unwrap_or(false)
        })
    }

    macro_rules! need_tools {
        () => {
            if !have_tools() {
                eprintln!("skipping: mke2fs or debugfs is not installed");
                return;
            }
        };
    }

    struct Real {
        dir: tempfile::TempDir,
        store: Arc<SqliteWorkflowStore>,
        wm: Arc<WorkspaceManager>,
    }

    fn real() -> Real {
        let dir = tempfile::tempdir().expect("dir");
        let layout = Drive9Config::new(dir.path()).layout();
        let chunks = ChunkStore::new(layout.clone());
        let manifests = ManifestManager::new(layout.clone(), chunks.clone());
        let cache = CacheManager::new(layout.clone(), chunks.clone(), 1 << 24).expect("cache");
        let wm = Arc::new(WorkspaceManager::new(
            layout,
            chunks,
            manifests,
            cache,
            Box::new(E2fsImageIo::new()),
        ));
        let store = SqliteWorkflowStore::in_memory().expect("store");
        store
            .persist_run_spec(
                RUN,
                r#"{"inputs":{"prompt":"go"}}"#,
                Some(SESSION),
                "urn:vam:agent:test",
                None,
                "User::\"tester\"",
            )
            .expect("run");
        Real {
            dir,
            store: Arc::new(store),
            wm,
        }
    }

    fn start_session(r: &Real) -> std::path::PathBuf {
        let tree =
            r.wm.materialize(
                SESSION,
                "ws",
                &StorageProfile::new("t".into(), 8 * 1024 * 1024),
                None,
                None,
                None,
            )
            .expect("materialize");
        // The host's conversation.
        std::fs::create_dir_all(tree.join(".vam/threads/t")).expect("dir");
        std::fs::write(tree.join(".vam/threads/t/log"), b"conversation").expect("log");
        tree
    }

    fn guest_write(r: &Real, guest_path: &str, body: &[u8]) {
        let image = r.wm.layout().session_image_path(SESSION);
        let host = r.dir.path().join("guest-src");
        std::fs::write(&host, body).expect("src");
        if let Some((dir, _)) = guest_path.rsplit_once('/') {
            let mut cur = String::new();
            for part in dir.split('/') {
                cur = format!("{cur}/{part}");
                let _ = std::process::Command::new("debugfs")
                    .args(["-w", "-R", &format!("mkdir {cur}")])
                    .arg(&image)
                    .output();
            }
        }
        let _ = std::process::Command::new("debugfs")
            .args(["-w", "-R", &format!("rm {guest_path}")])
            .arg(&image)
            .output();
        let out = std::process::Command::new("debugfs")
            .args([
                "-w",
                "-R",
                &format!("write {} {guest_path}", host.display()),
            ])
            .arg(&image)
            .output()
            .expect("debugfs");
        assert!(out.status.success(), "{:?}", out.stderr);
    }

    /// The commit a round takes: the host copy, then the two halves recorded.
    fn take_turn(r: &Real, round: u32) {
        let committer = ImageTurnCommitter::new(r.wm.clone(), r.store.clone());
        let handle = committer
            .snapshot(&TurnCommitRequest {
                workspace_id: "ws".into(),
                session_id: SESSION.into(),
                run_id: RUN.into(),
                round,
                messages_digest: "ab".repeat(32),
                wrote_files: true,
            })
            .expect("snapshot");
        assert_eq!(handle.0, image_token(round));
        let tail = format!("{round:02x}").repeat(32);
        r.store
            .record_turn_commit_pending(RUN, SESSION, round, round as u64, &"cd".repeat(32), &tail)
            .expect("pending");
        r.store
            .set_turn_commit_manifest(RUN, round, &handle.0)
            .expect("token");
        r.store
            .match_turn_commit_tail(RUN, &tail, round as u64 + 1, &"bb".repeat(32))
            .expect("boundary");
    }

    fn head_files(r: &Real) -> std::collections::BTreeMap<String, Vec<u8>> {
        let layout = r.wm.layout().clone();
        let chunks = ChunkStore::new(layout.clone());
        let manifests = ManifestManager::new(layout, chunks.clone());
        let head = manifests.read_head("ws").expect("head").expect("some head");
        manifests
            .get_manifest("ws", &head)
            .expect("manifest")
            .files
            .iter()
            .map(|f| {
                let mut bytes = Vec::new();
                for c in &f.chunks {
                    bytes.extend(chunks.get_chunk(c).expect("chunk"));
                }
                (f.path.clone(), bytes)
            })
            .collect()
    }

    fn session_with_one_finished_round_and_a_cut_off_write(r: &Real) {
        start_session(r);
        guest_write(r, "workspace/note.txt", b"kiwi-42");
        take_turn(r, 1);
        guest_write(r, "workspace/partial.txt", b"half");
    }

    #[test]
    fn an_ext4_stop_rolls_back_to_the_last_complete_commit_and_partial_goes_to_kept() {
        need_tools!();
        let r = real();
        session_with_one_finished_round_and_a_cut_off_write(&r);

        let (plan, summary) =
            plan_end(&r.store, &r.wm, SESSION, "ws", Disposition::Stopped, true).expect("plan");
        assert_eq!(summary.base, "turn_commit");
        assert_eq!(summary.dropped.count, 1, "{:?}", summary.dropped);
        assert_eq!(summary.dropped.entries[0].path, "workspace/partial.txt");
        assert!(!plan.prepared.refresh_template);
        let outcome = settle_with_plan(
            &r.store,
            &r.wm,
            SESSION,
            "ws",
            Disposition::Stopped,
            Some(plan),
            false,
        );
        let SettleOutcome::Committed(head) = outcome else {
            panic!("{outcome:?}");
        };

        let files = head_files(&r);
        assert_eq!(files["workspace/note.txt"], b"kiwi-42");
        assert!(
            !files.contains_key("workspace/partial.txt"),
            "{:?}",
            files.keys()
        );
        // The host `.vam/` came from the session tree, composed in without a
        // backup copy anywhere.
        assert_eq!(files[".vam/threads/t/log"], b"conversation");
        // The stop left the golden template alone.
        assert!(!r.wm.layout().template_path("ws", &head, "none").exists());
        // The rolled-back file is in kept/, the tree and its copies are gone.
        let kept =
            r.wm.layout()
                .kept_session_dir(SESSION)
                .join("rolled-back/workspace/partial.txt");
        assert_eq!(std::fs::read(kept).expect("kept"), b"half");
        assert!(!r.wm.layout().session_dir(SESSION).exists());
        assert_eq!(r.wm.lock_holder("ws").expect("lock"), None);
        // The commit row holds the real manifest hash now, not the token.
        let row = r
            .store
            .last_complete_turn_commit(RUN, SESSION)
            .expect("read")
            .expect("row");
        assert!(crate::turn_commit::parse_image_token(&row.manifest_hash).is_none());
    }

    #[test]
    fn a_stop_nobody_planned_is_planned_by_the_settle_the_same_way() {
        need_tools!();
        let r = real();
        session_with_one_finished_round_and_a_cut_off_write(&r);
        let outcome = settle_with_plan(
            &r.store,
            &r.wm,
            SESSION,
            "ws",
            Disposition::Stopped,
            None,
            true,
        );
        assert!(
            matches!(outcome, SettleOutcome::Committed(_)),
            "{outcome:?}"
        );
        let files = head_files(&r);
        assert!(files.contains_key("workspace/note.txt"));
        assert!(!files.contains_key("workspace/partial.txt"));
        assert!(r
            .wm
            .layout()
            .kept_session_dir(SESSION)
            .join("rolled-back/workspace/partial.txt")
            .exists());
    }

    #[test]
    fn an_unreadable_newest_copy_falls_back_to_the_previous_commit_and_raises_the_event() {
        need_tools!();
        let r = real();
        start_session(&r);
        guest_write(&r, "workspace/note.txt", b"first");
        take_turn(&r, 1);
        // Round 2's copy is taken straight from the image (as a committer that
        // keeps both would) and then damaged.
        guest_write(&r, "workspace/note.txt", b"second");
        r.wm.snapshot_turn_image(SESSION, 2, &vam_storage::SnapshotLimits::default())
            .expect("copy 2");
        let tail = "02".repeat(32);
        r.store
            .record_turn_commit_pending(RUN, SESSION, 2, 2, &"cd".repeat(32), &tail)
            .unwrap();
        r.store.set_turn_commit_manifest(RUN, 2, "image:2").unwrap();
        r.store
            .match_turn_commit_tail(RUN, &tail, 3, &"bb".repeat(32))
            .unwrap();
        std::fs::write(
            r.wm.layout().session_turn_image_path(SESSION, 2),
            vec![0xffu8; 1 << 20],
        )
        .unwrap();

        let outcome = settle_workspace(&r.store, &r.wm, SESSION, "ws", Disposition::Stopped);
        assert!(
            matches!(outcome, SettleOutcome::Committed(_)),
            "{outcome:?}"
        );
        assert_eq!(head_files(&r)["workspace/note.txt"], b"first");
        assert_eq!(
            r.store.turn_commit(RUN, 2).unwrap().map(|(s, _)| s),
            Some(crate::turn_commit::TurnCommitState::Failed)
        );
        let events = r.store.read_outbox_for_run(RUN, 0).unwrap();
        assert!(events
            .iter()
            .any(|e| e.event_type == TYPE_WORKSPACE_COMMIT_FAILED));
    }

    #[test]
    fn a_stop_that_cannot_read_its_copy_keeps_the_whole_tree() {
        need_tools!();
        let r = real();
        start_session(&r);
        guest_write(&r, "workspace/note.txt", b"first");
        take_turn(&r, 1);
        // An I/O error, not corruption: the copy is gone.
        std::fs::remove_file(r.wm.layout().session_turn_image_path(SESSION, 1)).unwrap();
        let outcome = settle_workspace(&r.store, &r.wm, SESSION, "ws", Disposition::Stopped);
        let SettleOutcome::Kept { kept_id, error } = outcome else {
            panic!("expected the tree to be kept, got {outcome:?}");
        };
        assert_eq!(kept_id, SESSION);
        assert!(!error.contains(r.dir.path().to_str().unwrap()), "{error}");
        assert!(r
            .wm
            .layout()
            .kept_session_dir(SESSION)
            .join("drive9.ext4")
            .is_file());
    }

    #[test]
    fn a_live_end_refreshes_the_template_after_reading_the_live_image() {
        need_tools!();
        let r = real();
        start_session(&r);
        guest_write(&r, "workspace/live.txt", b"all of it");
        let (plan, summary) =
            plan_end(&r.store, &r.wm, SESSION, "ws", Disposition::Live, true).expect("plan");
        assert_eq!(summary.base, "live");
        assert!(plan.prepared.refresh_template);
        let outcome = settle_with_plan(
            &r.store,
            &r.wm,
            SESSION,
            "ws",
            Disposition::Live,
            Some(plan),
            false,
        );
        let SettleOutcome::Committed(head) = outcome else {
            panic!("{outcome:?}");
        };
        assert!(r.wm.layout().template_path("ws", &head, "none").is_file());
        assert_eq!(head_files(&r)["workspace/live.txt"], b"all of it");
    }

    #[tokio::test]
    async fn a_dead_tree_in_vm_mode_is_settled_from_the_surviving_copies() {
        need_tools!();
        let r = real();
        session_with_one_finished_round_and_a_cut_off_write(&r);
        r.store.update_run_state(RUN, "RUNNING").unwrap();
        let trees = vec![crate::reconcile::DeadTree {
            session_id: SESSION.into(),
            workspace_id: "ws".into(),
            vmm_pid: None,
            vmm_start_time: None,
        }];
        assert_eq!(settle_dead_trees(&r.store, &r.wm, &trees).await, 1);

        assert_eq!(r.store.get_run_state(RUN).unwrap(), "FAILED");
        let files = head_files(&r);
        assert_eq!(files["workspace/note.txt"], b"kiwi-42");
        assert!(!files.contains_key("workspace/partial.txt"));
        assert_eq!(files[".vam/threads/t/log"], b"conversation");
        assert!(r
            .wm
            .layout()
            .kept_session_dir(SESSION)
            .join("rolled-back/workspace/partial.txt")
            .exists());
        assert!(!r.wm.layout().session_dir(SESSION).exists());
    }

    #[tokio::test]
    async fn the_settle_waits_for_a_dead_sessions_vmm_to_exit_while_its_start_time_matches() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("sleep");
        let pid = child.id();
        let start = crate::reconcile::get_process_start_time(pid).expect("start time");
        let killer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(400));
            let _ = child.kill();
            let _ = child.wait();
        });
        let started = std::time::Instant::now();
        assert!(wait_for_vmm_exit(pid, Some(start), Duration::from_secs(10)).await);
        assert!(
            started.elapsed() >= Duration::from_millis(300),
            "{:?}",
            started.elapsed()
        );
        killer.join().unwrap();
    }

    #[tokio::test]
    async fn a_reused_pid_a_gone_process_and_a_timeout_are_told_apart() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("sleep");
        let pid = child.id();
        let start = crate::reconcile::get_process_start_time(pid).expect("start time");
        // Another start time: the pid is somebody else's now, nothing to wait for.
        let t = std::time::Instant::now();
        assert!(wait_for_vmm_exit(pid, Some(start + 1), Duration::from_secs(5)).await);
        assert!(t.elapsed() < Duration::from_secs(1));
        // No recorded start time: the same.
        assert!(wait_for_vmm_exit(pid, None, Duration::from_secs(5)).await);
        // Still the recorded process: the wait times out.
        let t = std::time::Instant::now();
        assert!(!wait_for_vmm_exit(pid, Some(start), Duration::from_millis(200)).await);
        assert!(t.elapsed() >= Duration::from_millis(200));
        let _ = child.kill();
        let _ = child.wait();
        // Gone (reaped): immediately true.
        assert!(wait_for_vmm_exit(pid, Some(start), Duration::from_secs(5)).await);
    }
}
