//! Turn commits: a conversation position and a workspace snapshot recorded
//! together (design V.2).
//!
//! A turn commit is taken after a round that ran a tool that may write files.
//! It has two halves, and it is `complete` only when both are recorded:
//!
//! * the **manifest**: the committer snapshots the workspace while the guest
//!   waits on `CommitTurn`, and the snapshot's manifest hash is recorded;
//! * the **boundary**: the position in the thread log right after the round's
//!   tool results. The host recorder records it when the guest's next model
//!   request carries a tool-result tail whose digest equals the one `CommitTurn`
//!   announced. A guest cannot make a commit complete by saying so: only the
//!   host seeing the matching tail does.
//!
//! A stop rolls files and conversation back to the newest complete commit of
//! the stopped session (decision 12). Process guests (the directory backend)
//! use [`DirectoryTurnCommitter`]; microVM guests (the image backend) use
//! [`ImageTurnCommitter`]: the guest freezes its filesystem, the host takes a
//! private copy of the image while the guest waits, and the copy is read into a
//! manifest lazily, only when a stop (or a park or an end) needs it.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use rusqlite::{params, OptionalExtension};
use thiserror::Error;
use vam_spec::v1::{
    CommitTurnRequest, CommitTurnResponse, ErrorStatus, TurnCommitState as WireState,
};

use crate::error::OrchestratorError;
use crate::run_checkpoint::bind_checkpoint_lease;
use crate::workflow::SqliteWorkflowStore;

/// Run error reason when a turn commit or the session-end commit fails
/// (AR-2.8). The work is kept, not discarded.
pub const RUN_ERROR_STORAGE_COMMIT_FAILED: &str = "storage_commit_failed";

/// Most turn commits one run may take. The agent loop's hard cap is 200 turns;
/// a guest asking for more is refused.
pub const MAX_TURN_COMMITS_PER_RUN: u32 = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnCommitState {
    Pending,
    Complete,
    Failed,
}

impl TurnCommitState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Complete => "complete",
            Self::Failed => "failed",
        }
    }

    pub fn parse(value: &str) -> Result<Self, OrchestratorError> {
        match value {
            "pending" => Ok(Self::Pending),
            "complete" => Ok(Self::Complete),
            "failed" => Ok(Self::Failed),
            _ => Err(OrchestratorError::CorruptState(value.to_string())),
        }
    }

    fn wire(self) -> WireState {
        match self {
            Self::Pending => WireState::Pending,
            Self::Complete => WireState::Complete,
            Self::Failed => WireState::Failed,
        }
    }
}

/// What a committer needs to snapshot one turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnCommitRequest {
    pub workspace_id: String,
    pub session_id: String,
    pub run_id: String,
    pub round: u32,
    /// Digest of the tool-result tail the round appends (as the guest claims it).
    pub messages_digest: String,
    pub wrote_files: bool,
}

/// Opaque handle to a snapshot taken by [`TurnCommitter::snapshot`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotHandle(pub String);

#[derive(Debug, Error)]
pub enum TurnCommitError {
    #[error("turn commits are not available: {0}")]
    Unavailable(String),
    #[error("workspace snapshot failed: {0}")]
    Snapshot(String),
    #[error("manifest extraction failed: {0}")]
    Extract(String),
    /// A stop was requested: no new commit is taken.
    #[error("the task is being stopped")]
    Stopping,
    #[error(transparent)]
    Store(#[from] OrchestratorError),
}

/// Resolves to the Drive9 manifest hash of the snapshot.
pub type ExtractFuture = Pin<Box<dyn Future<Output = Result<String, TurnCommitError>> + Send>>;

/// How a turn's files are captured. The directory backend has
/// [`DirectoryTurnCommitter`]; the image backend gets its committer in Wave 6.
pub trait TurnCommitter: Send + Sync {
    /// Runs while the guest waits (a tree manifest, or freeze and a
    /// copy-on-write copy). Blocking: callers run it on a blocking thread.
    fn snapshot(&self, request: &TurnCommitRequest) -> Result<SnapshotHandle, TurnCommitError>;

    /// Background extraction of a snapshot to a manifest hash. Completes the commit.
    fn extract(&self, handle: SnapshotHandle) -> ExtractFuture;

    /// State of a recorded turn commit, if any.
    fn status(&self, run_id: &str, round: u32) -> Result<Option<TurnCommitState>, TurnCommitError>;
}

/// Refuses every commit. The committer of backends that have none.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopTurnCommitter;

impl TurnCommitter for NoopTurnCommitter {
    fn snapshot(&self, _request: &TurnCommitRequest) -> Result<SnapshotHandle, TurnCommitError> {
        Err(TurnCommitError::Unavailable(
            "turn commits not enabled".into(),
        ))
    }

    fn extract(&self, _handle: SnapshotHandle) -> ExtractFuture {
        Box::pin(async {
            Err(TurnCommitError::Unavailable(
                "turn commits not enabled".into(),
            ))
        })
    }

    fn status(
        &self,
        _run_id: &str,
        _round: u32,
    ) -> Result<Option<TurnCommitState>, TurnCommitError> {
        Ok(None)
    }
}

/// Process guests share the session's materialized directory with the host, so
/// a turn commit is a manifest of that tree (the host `.vam/` left out) built
/// while the guest waits. Cost grows with the workspace: this is the test-node
/// backend; the image backend's copy-on-write committer is Wave 6.
pub struct DirectoryTurnCommitter {
    workspaces: Arc<vam_storage::WorkspaceManager>,
}

impl DirectoryTurnCommitter {
    pub fn new(workspaces: Arc<vam_storage::WorkspaceManager>) -> Self {
        Self { workspaces }
    }
}

impl TurnCommitter for DirectoryTurnCommitter {
    fn snapshot(&self, request: &TurnCommitRequest) -> Result<SnapshotHandle, TurnCommitError> {
        self.workspaces
            .snapshot_tree_manifest(&request.session_id, &request.workspace_id)
            .map(SnapshotHandle)
            .map_err(|e| TurnCommitError::Snapshot(e.to_string()))
    }

    fn extract(&self, handle: SnapshotHandle) -> ExtractFuture {
        // The snapshot already is a manifest.
        Box::pin(async move { Ok(handle.0) })
    }

    fn status(
        &self,
        _run_id: &str,
        _round: u32,
    ) -> Result<Option<TurnCommitState>, TurnCommitError> {
        Ok(None)
    }
}

/// The committer of a node's guests, by backend: process guests share their
/// tree with the host ([`DirectoryTurnCommitter`]); microVM guests write an
/// image ([`ImageTurnCommitter`]).
pub fn committer_for(
    workspaces: &Arc<vam_storage::WorkspaceManager>,
    store: &Arc<SqliteWorkflowStore>,
) -> Arc<dyn TurnCommitter> {
    match workspaces.backend() {
        vam_storage::Drive9Backend::Directory => {
            Arc::new(DirectoryTurnCommitter::new(workspaces.clone()))
        }
        vam_storage::Drive9Backend::Ext4Image => {
            Arc::new(ImageTurnCommitter::new(workspaces.clone(), store.clone()))
        }
    }
}

/// Prefix of the `manifest_hash` of a commit whose manifest is not read yet: the
/// image committer records `image:<round>`, naming the private copy
/// `sessions/<id>/turns/<round>.ext4`. A stop swaps it for the real manifest
/// hash ([`resolve_stop_base`]).
pub const IMAGE_TOKEN_PREFIX: &str = "image:";

pub fn image_token(round: u32) -> String {
    format!("{IMAGE_TOKEN_PREFIX}{round}")
}

/// The round a deferred-manifest token names, if `value` is one.
pub fn parse_image_token(value: &str) -> Option<u32> {
    value.strip_prefix(IMAGE_TOKEN_PREFIX)?.parse().ok()
}

/// Most turn copies waiting for their boundary at once (besides the newest
/// complete one). A commit whose boundary never arrives must not hold a copy of
/// the image for ever.
pub const MAX_PENDING_TURN_COPIES: usize = 2;

/// MicroVM guests write their own disk image, so a turn commit is a private
/// copy of it, taken while the guest waits on `CommitTurn` with its filesystem
/// frozen (the guest freezes before it asks and thaws on the answer).
///
/// * `snapshot` copies `sessions/<id>/drive9.ext4` to
///   `sessions/<id>/turns/<round>.ext4` (reflink where the volume has it, else a
///   deadline-bound sparse copy) and prunes every copy but the newest complete
///   one and at most [`MAX_PENDING_TURN_COPIES`] pending ones.
/// * `extract` returns at once with the token `image:<round>`: reading the copy
///   into a manifest costs time proportional to the workspace, and only a stop
///   needs it. Nothing reads the copy until then.
pub struct ImageTurnCommitter {
    workspaces: Arc<vam_storage::WorkspaceManager>,
    store: Arc<SqliteWorkflowStore>,
    limits: vam_storage::SnapshotLimits,
}

impl ImageTurnCommitter {
    pub fn new(
        workspaces: Arc<vam_storage::WorkspaceManager>,
        store: Arc<SqliteWorkflowStore>,
    ) -> Self {
        Self {
            workspaces,
            store,
            limits: vam_storage::SnapshotLimits::default(),
        }
    }

    pub fn with_limits(mut self, limits: vam_storage::SnapshotLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Settles the rows this session's copies belong to and prunes the rest:
    /// returns after only the newest complete copy and the pending ones that fit
    /// next to the new copy are left.
    fn prune(&self, request: &TurnCommitRequest) -> Result<(), TurnCommitError> {
        let rows = self
            .store
            .turn_commit_rows(&request.run_id, &request.session_id)?;
        let newest_complete = rows
            .iter()
            .filter(|r| r.state == TurnCommitState::Complete)
            .map(|r| r.round)
            .max();
        let mut keep: Vec<u32> = newest_complete.into_iter().collect();
        let mut pending: Vec<u32> = rows
            .iter()
            .filter(|r| r.state == TurnCommitState::Pending)
            .map(|r| r.round)
            .collect();
        pending.sort_unstable();
        // A pending commit older than a complete one can no longer complete to
        // any use; and room is made for the copy about to be taken.
        let mut live: Vec<u32> = Vec::new();
        for round in pending {
            if newest_complete.is_some_and(|c| round < c) {
                let _ = self.store.fail_turn_commit(
                    &request.run_id,
                    round,
                    "superseded by a later turn commit",
                );
            } else {
                live.push(round);
            }
        }
        while live.len() >= MAX_PENDING_TURN_COPIES {
            let oldest = live.remove(0);
            let _ = self.store.fail_turn_commit(
                &request.run_id,
                oldest,
                "dropped: too many turn copies waiting",
            );
        }
        keep.extend(live);
        self.workspaces
            .prune_turn_copies(&request.session_id, &keep)
            .map_err(|e| TurnCommitError::Snapshot(e.to_string()))?;
        Ok(())
    }
}

impl TurnCommitter for ImageTurnCommitter {
    fn snapshot(&self, request: &TurnCommitRequest) -> Result<SnapshotHandle, TurnCommitError> {
        if self.store.session_ends.stop_requested(&request.session_id) {
            return Err(TurnCommitError::Stopping);
        }
        self.prune(request)?;
        let report = self
            .workspaces
            .snapshot_turn_image(&request.session_id, request.round, &self.limits)
            .map_err(|e| TurnCommitError::Snapshot(e.to_string()))?;
        eprintln!(
            "[orchestrator] turn commit snapshot: session={} round={} method={} copied_mib={} ms={}",
            request.session_id,
            request.round,
            report.method.as_str(),
            report.copied_mib(),
            report.elapsed_ms
        );
        Ok(SnapshotHandle(image_token(request.round)))
    }

    fn extract(&self, handle: SnapshotHandle) -> ExtractFuture {
        // Lazy: the token stands for the copy until a stop reads it.
        Box::pin(async move { Ok(handle.0) })
    }

    fn status(
        &self,
        _run_id: &str,
        _round: u32,
    ) -> Result<Option<TurnCommitState>, TurnCommitError> {
        Ok(None)
    }
}

/// The manifest hash of the files a stop rolls back to: the newest complete turn
/// commit of the session, with a deferred `image:<round>` token read into a real
/// manifest (and the real hash stored back).
///
/// If that copy is unreadable ([`vam_storage::Drive9Error::ImageCorrupt`]) the
/// commit is marked failed, `commit_failed` is raised, and the previous complete
/// commit is tried; with none left the answer is `None` (the run's start). Any
/// other failure (I/O, a read limit, a missing copy) is returned: the settle
/// fails and the whole session tree is kept.
pub fn resolve_stop_base(
    store: &SqliteWorkflowStore,
    workspaces: &vam_storage::WorkspaceManager,
    run_id: &str,
    session_id: &str,
    workspace_id: &str,
) -> Result<Option<String>, vam_storage::Drive9Error> {
    loop {
        let Some(row) = store
            .last_complete_turn_commit(run_id, session_id)
            .map_err(|e| vam_storage::Drive9Error::Layout(e.to_string()))?
        else {
            return Ok(None);
        };
        let Some(round) = parse_image_token(&row.manifest_hash) else {
            return Ok(Some(row.manifest_hash));
        };
        match workspaces.resolve_turn_copy(session_id, workspace_id, round) {
            Ok(hash) => {
                if let Err(e) = store.resolve_turn_commit_manifest(run_id, round, &hash) {
                    tracing::warn!("resolved manifest of {run_id}#{round} not stored: {e}");
                }
                return Ok(Some(hash));
            }
            Err(vam_storage::Drive9Error::ImageCorrupt(why)) => {
                tracing::error!(
                    "turn copy {round} of session {session_id} is unreadable ({why}); \
                     falling back to the previous complete turn commit"
                );
                store
                    .fail_complete_turn_commit(run_id, round, "the turn copy is unreadable")
                    .map_err(|e| vam_storage::Drive9Error::Layout(e.to_string()))?;
                emit_turn_commit_failed(
                    store,
                    &TurnCommitRequest {
                        workspace_id: workspace_id.to_string(),
                        session_id: session_id.to_string(),
                        run_id: run_id.to_string(),
                        round,
                        messages_digest: String::new(),
                        wrote_files: true,
                    },
                    &format!("the turn copy was unreadable: {why}"),
                );
            }
            Err(other) => return Err(other),
        }
    }
}

/// A recorded turn commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnCommitRow {
    pub run_id: String,
    pub session_id: String,
    pub round: u32,
    /// Log head when `CommitTurn` arrived (the round's assistant message).
    pub log_seq: u64,
    pub log_digest: String,
    pub tail_digest: String,
    pub manifest_hash: String,
    /// Log position right after the round's tool results; set by the recorder.
    pub boundary_seq: Option<u64>,
    pub boundary_digest: String,
    pub state: TurnCommitState,
    pub error: String,
}

const ROW_COLUMNS: &str = "run_id, session_id, round, log_seq, log_digest, tail_digest, \
     manifest_hash, boundary_seq, boundary_digest, state, error";

fn row_to_commit(row: &rusqlite::Row<'_>) -> rusqlite::Result<(TurnCommitRow, String)> {
    let state: String = row.get(9)?;
    Ok((
        TurnCommitRow {
            run_id: row.get(0)?,
            session_id: row.get(1)?,
            round: row.get::<_, i64>(2)? as u32,
            log_seq: row.get::<_, i64>(3)? as u64,
            log_digest: row.get(4)?,
            tail_digest: row.get(5)?,
            manifest_hash: row.get(6)?,
            boundary_seq: row.get::<_, Option<i64>>(7)?.map(|s| s as u64),
            boundary_digest: row.get(8)?,
            state: TurnCommitState::Pending,
            error: row.get(10)?,
        },
        state,
    ))
}

fn finish_row(parts: (TurnCommitRow, String)) -> Result<TurnCommitRow, OrchestratorError> {
    let (mut row, state) = parts;
    row.state = TurnCommitState::parse(&state)?;
    Ok(row)
}

/// `UPDATE ... complete` when both halves are present. Returns whether this
/// call completed the commit.
fn try_complete(
    connection: &rusqlite::Connection,
    run_id: &str,
    round: u32,
) -> Result<bool, OrchestratorError> {
    let changed = connection.execute(
        "UPDATE turn_commits SET state = 'complete'
         WHERE run_id = ?1 AND round = ?2 AND state = 'pending'
           AND manifest_hash != '' AND boundary_seq IS NOT NULL",
        params![run_id, round],
    )?;
    Ok(changed > 0)
}

impl SqliteWorkflowStore {
    /// Record a turn commit as `pending`. A repeat for the same round and tail
    /// digest is a no-op.
    pub fn record_turn_commit_pending(
        &self,
        run_id: &str,
        session_id: &str,
        round: u32,
        log_seq: u64,
        log_digest: &str,
        tail_digest: &str,
    ) -> Result<(), OrchestratorError> {
        if log_digest.is_empty() {
            return Err(OrchestratorError::InvalidContract(
                "a turn commit needs the thread log digest".into(),
            ));
        }
        if tail_digest.is_empty() {
            return Err(OrchestratorError::InvalidContract(
                "a turn commit needs the tool-result tail digest".into(),
            ));
        }
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let existing: Option<String> = connection
            .query_row(
                "SELECT tail_digest FROM turn_commits WHERE run_id = ?1 AND round = ?2",
                params![run_id, round],
                |row| row.get(0),
            )
            .optional()?;
        match existing {
            Some(digest) if digest == tail_digest => Ok(()),
            Some(_) => Err(OrchestratorError::Conflict(format!(
                "run {run_id} round {round} already has a turn commit for other results"
            ))),
            None => {
                connection.execute(
                    "INSERT INTO turn_commits
                        (run_id, session_id, round, log_seq, log_digest, tail_digest, state, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'pending', strftime('%s','now'))",
                    params![run_id, session_id, round, log_seq as i64, log_digest, tail_digest],
                )?;
                Ok(())
            }
        }
    }

    /// One recorded turn commit.
    pub fn turn_commit_row(
        &self,
        run_id: &str,
        round: u32,
    ) -> Result<Option<TurnCommitRow>, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        connection
            .query_row(
                &format!("SELECT {ROW_COLUMNS} FROM turn_commits WHERE run_id = ?1 AND round = ?2"),
                params![run_id, round],
                row_to_commit,
            )
            .optional()?
            .map(finish_row)
            .transpose()
    }

    /// Turn commits recorded for a run (any state).
    pub fn count_turn_commits(&self, run_id: &str) -> Result<u32, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        Ok(connection.query_row(
            "SELECT COUNT(*) FROM turn_commits WHERE run_id = ?1",
            [run_id],
            |row| row.get::<_, i64>(0),
        )? as u32)
    }

    /// Turn commits one session of a run has recorded (any state).
    pub fn count_session_turn_commits(
        &self,
        run_id: &str,
        session_id: &str,
    ) -> Result<u32, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        Ok(connection.query_row(
            "SELECT COUNT(*) FROM turn_commits WHERE run_id = ?1 AND session_id = ?2",
            params![run_id, session_id],
            |row| row.get::<_, i64>(0),
        )? as u32)
    }

    /// The thread log position a turn commit recorded when it was taken.
    pub fn turn_commit_log_position(
        &self,
        run_id: &str,
        round: u32,
    ) -> Result<Option<(u64, String)>, OrchestratorError> {
        Ok(self
            .turn_commit_row(run_id, round)?
            .map(|row| (row.log_seq, row.log_digest)))
    }

    /// Records the manifest hash of a commit's snapshot. Completes the commit
    /// when its boundary is already known; returns whether it did.
    pub fn set_turn_commit_manifest(
        &self,
        run_id: &str,
        round: u32,
        manifest_hash: &str,
    ) -> Result<bool, OrchestratorError> {
        if manifest_hash.is_empty() {
            return Err(OrchestratorError::InvalidContract(
                "a turn commit needs a manifest hash".into(),
            ));
        }
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let changed = connection.execute(
            "UPDATE turn_commits SET manifest_hash = ?3
             WHERE run_id = ?1 AND round = ?2 AND state = 'pending'",
            params![run_id, round, manifest_hash],
        )?;
        if changed == 0 {
            return Err(OrchestratorError::InvalidContract(format!(
                "run {run_id} round {round} has no pending turn commit"
            )));
        }
        try_complete(&connection, run_id, round)
    }

    /// The host recorder saw a request whose tool-result tail has `tail_digest`
    /// and ends at log position `(boundary_seq, boundary_digest)`. If a pending
    /// commit of the run announced that digest, its boundary is recorded (the
    /// newest such round wins). Returns the round and whether the commit is now
    /// complete.
    pub fn match_turn_commit_tail(
        &self,
        run_id: &str,
        tail_digest: &str,
        boundary_seq: u64,
        boundary_digest: &str,
    ) -> Result<Option<(u32, bool)>, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let round: Option<i64> = connection
            .query_row(
                "SELECT round FROM turn_commits
                 WHERE run_id = ?1 AND tail_digest = ?2 AND state = 'pending'
                   AND boundary_seq IS NULL
                 ORDER BY round DESC LIMIT 1",
                params![run_id, tail_digest],
                |row| row.get(0),
            )
            .optional()?;
        let Some(round) = round else {
            return Ok(None);
        };
        let round = round as u32;
        connection.execute(
            "UPDATE turn_commits SET boundary_seq = ?3, boundary_digest = ?4
             WHERE run_id = ?1 AND round = ?2 AND state = 'pending'",
            params![run_id, round, boundary_seq as i64, boundary_digest],
        )?;
        let completed = try_complete(&connection, run_id, round)?;
        Ok(Some((round, completed)))
    }

    /// Marks one pending commit failed (a snapshot or extraction error).
    pub fn fail_turn_commit(
        &self,
        run_id: &str,
        round: u32,
        error: &str,
    ) -> Result<(), OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let changed = connection.execute(
            "UPDATE turn_commits SET state = 'failed', error = ?3
             WHERE run_id = ?1 AND round = ?2 AND state = 'pending'",
            params![run_id, round, error],
        )?;
        if changed == 0 {
            return Err(OrchestratorError::InvalidContract(format!(
                "run {run_id} round {round} has no pending turn commit"
            )));
        }
        Ok(())
    }

    /// Marks every commit of a session that never completed as failed, so a
    /// stopped task leaves no pending rows. Returns how many.
    pub fn fail_pending_turn_commits(
        &self,
        run_id: &str,
        session_id: &str,
        error: &str,
    ) -> Result<usize, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        Ok(connection.execute(
            "UPDATE turn_commits SET state = 'failed', error = ?3
             WHERE run_id = ?1 AND session_id = ?2 AND state = 'pending'",
            params![run_id, session_id, error],
        )?)
    }

    /// The newest complete commit of one session of a run.
    pub fn last_complete_turn_commit(
        &self,
        run_id: &str,
        session_id: &str,
    ) -> Result<Option<TurnCommitRow>, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        connection
            .query_row(
                &format!(
                    "SELECT {ROW_COLUMNS} FROM turn_commits
                     WHERE run_id = ?1 AND session_id = ?2 AND state = 'complete'
                     ORDER BY round DESC LIMIT 1"
                ),
                params![run_id, session_id],
                row_to_commit,
            )
            .optional()?
            .map(finish_row)
            .transpose()
    }

    /// The newest complete commit of `session_id`, else (a session that took over a task after
    /// a crash has none of its own yet) the newest complete commit any earlier session of the run
    /// took. Rounds only grow across sessions, so the newest is the latest finished step.
    pub fn last_complete_turn_commit_for_run(
        &self,
        run_id: &str,
        session_id: &str,
    ) -> Result<Option<TurnCommitRow>, OrchestratorError> {
        if let Some(row) = self.last_complete_turn_commit(run_id, session_id)? {
            return Ok(Some(row));
        }
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        connection
            .query_row(
                &format!(
                    "SELECT {ROW_COLUMNS} FROM turn_commits
                     WHERE run_id = ?1 AND state = 'complete'
                     ORDER BY round DESC LIMIT 1"
                ),
                params![run_id],
                row_to_commit,
            )
            .optional()?
            .map(finish_row)
            .transpose()
    }

    /// The highest round any commit row of the run has, in any state.
    pub fn max_turn_commit_round(&self, run_id: &str) -> Result<u32, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let max: Option<i64> = connection.query_row(
            "SELECT MAX(round) FROM turn_commits WHERE run_id = ?1",
            params![run_id],
            |row| row.get(0),
        )?;
        Ok(max.unwrap_or(0).max(0) as u32)
    }

    /// Every turn commit one session of a run has recorded, oldest round first.
    pub fn turn_commit_rows(
        &self,
        run_id: &str,
        session_id: &str,
    ) -> Result<Vec<TurnCommitRow>, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let mut statement = connection.prepare(&format!(
            "SELECT {ROW_COLUMNS} FROM turn_commits
             WHERE run_id = ?1 AND session_id = ?2 ORDER BY round ASC"
        ))?;
        let parts = statement
            .query_map(params![run_id, session_id], row_to_commit)?
            .collect::<Result<Vec<_>, _>>()?;
        parts.into_iter().map(finish_row).collect()
    }

    /// Replaces a complete commit's deferred token with the manifest hash it
    /// names (see [`IMAGE_TOKEN_PREFIX`]).
    pub fn resolve_turn_commit_manifest(
        &self,
        run_id: &str,
        round: u32,
        manifest_hash: &str,
    ) -> Result<(), OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        connection.execute(
            "UPDATE turn_commits SET manifest_hash = ?3
             WHERE run_id = ?1 AND round = ?2 AND state = 'complete'
               AND manifest_hash LIKE 'image:%'",
            params![run_id, round, manifest_hash],
        )?;
        Ok(())
    }

    /// Marks a complete commit failed: its snapshot turned out to be unreadable,
    /// so a stop falls back to the previous complete commit.
    pub fn fail_complete_turn_commit(
        &self,
        run_id: &str,
        round: u32,
        error: &str,
    ) -> Result<(), OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        connection.execute(
            "UPDATE turn_commits SET state = 'failed', error = ?3
             WHERE run_id = ?1 AND round = ?2 AND state = 'complete'",
            params![run_id, round, error],
        )?;
        Ok(())
    }

    /// Manifest hash of the newest complete commit of a session's run.
    pub fn last_complete_turn_manifest(
        &self,
        run_id: &str,
        session_id: &str,
    ) -> Result<Option<String>, OrchestratorError> {
        Ok(self
            .last_complete_turn_commit(run_id, session_id)?
            .map(|row| row.manifest_hash))
    }

    /// State and manifest hash of a recorded turn commit.
    pub fn turn_commit(
        &self,
        run_id: &str,
        round: u32,
    ) -> Result<Option<(TurnCommitState, String)>, OrchestratorError> {
        Ok(self
            .turn_commit_row(run_id, round)?
            .map(|row| (row.state, row.manifest_hash)))
    }
}

fn refusal(code: i32, message: impl Into<String>) -> CommitTurnResponse {
    CommitTurnResponse {
        accepted: false,
        error: Some(ErrorStatus {
            code,
            message: message.into(),
            details: vec![],
            retryable: false,
        }),
        state: WireState::Unspecified as i32,
        manifest_hash: String::new(),
    }
}

/// The host's view of a session's thread log head. The position a turn commit
/// records comes from here, never from the guest.
pub trait ThreadLogHeads: Send + Sync {
    /// `(seq, digest)` of the newest record of the thread the session's run
    /// writes to, or `None` when the host has recorded nothing.
    fn log_head(&self, session_id: &str) -> Option<(u64, String)>;
}

/// No recorder is wired (before the host recorder lands): there is no head.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoLogHeads;

impl ThreadLogHeads for NoLogHeads {
    fn log_head(&self, _session_id: &str) -> Option<(u64, String)> {
        None
    }
}

/// Answer a guest `CommitTurn`.
///
/// * The lease must belong to the run bound to `session_id`; the workspace and
///   thread come from the host's rows, never from the request.
/// * `round` must be one the host recorder has seen a model request for, at
///   most one commit per round (a repeat with the same digest is answered with
///   the current state), at most [`MAX_TURN_COMMITS_PER_RUN`] per run.
/// * Refused once the session is being stopped or its recorder is sealed.
/// * The recorded log position is the host's own head. The request's digest is
///   only what the recorder later matches; the commit completes when the host
///   sees it, not when the guest says so.
/// * A storage failure is answered with code 13 and a message starting
///   `storage_commit_failed`, naming no host path; the detail goes to the
///   `commit_failed` event.
pub async fn handle_commit_turn(
    store: &Arc<SqliteWorkflowStore>,
    committer: &Arc<dyn TurnCommitter>,
    heads: &Arc<dyn ThreadLogHeads>,
    session_id: &str,
    request: &CommitTurnRequest,
) -> CommitTurnResponse {
    let bound = match bind_checkpoint_lease(store, session_id, request.lease.as_ref()) {
        Ok(bound) => bound,
        Err((code, message)) => return refusal(code, message),
    };
    let digest = match request.checkpoint_digest.as_ref() {
        Some(d) if d.algorithm == 1 && !d.hex_hash.is_empty() => d.hex_hash.clone(),
        _ => return refusal(3, "checkpoint digest (SHA-256) is required"),
    };
    if request.round == 0 {
        return refusal(3, "round must be positive");
    }
    if store.session_ends.stop_requested(session_id) {
        return refusal(9, "the task is being stopped");
    }
    // A repeat of a commit already taken is answered with its state.
    match store.turn_commit_row(&bound.run_id, request.round) {
        Ok(Some(row)) if row.tail_digest == digest => {
            return CommitTurnResponse {
                accepted: true,
                error: None,
                state: row.state.wire() as i32,
                manifest_hash: if row.state == TurnCommitState::Complete {
                    row.manifest_hash
                } else {
                    String::new()
                },
            };
        }
        Ok(Some(_)) => return refusal(6, "this round already has a turn commit"),
        Ok(None) => {}
        Err(err) => return refusal(13, err.to_string()),
    }
    if let Some(recorder) = store.thread_recorder(session_id) {
        if recorder.is_sealed() {
            return refusal(9, "the task has ended");
        }
        // A session never takes more turn commits than the model requests the
        // host has recorded for it. (The guest's round number can run ahead of
        // the host's count after a resume, so the count is what is bounded.)
        let taken = store
            .count_session_turn_commits(&bound.run_id, session_id)
            .unwrap_or(0);
        if u64::from(taken) >= recorder.rounds() {
            return refusal(3, "more turn commits than model requests the host has seen");
        }
    }
    match store.count_turn_commits(&bound.run_id) {
        Ok(n) if n >= MAX_TURN_COMMITS_PER_RUN => {
            return refusal(8, "too many turn commits for this run")
        }
        Ok(_) => {}
        Err(err) => return refusal(13, err.to_string()),
    }
    let workspace_id =
        match crate::run_checkpoint::thread_scope_for_session(store, session_id, &bound.run_id) {
            Ok((workspace_id, _)) => workspace_id,
            Err(err) => return refusal(13, err.to_string()),
        };
    let commit = TurnCommitRequest {
        workspace_id: workspace_id.clone(),
        session_id: session_id.to_string(),
        run_id: bound.run_id.clone(),
        round: request.round,
        messages_digest: digest.clone(),
        wrote_files: request.wrote_files,
    };
    let snapshot = {
        let committer = committer.clone();
        let commit = commit.clone();
        tokio::task::spawn_blocking(move || committer.snapshot(&commit)).await
    };
    let handle = match snapshot {
        Ok(Ok(handle)) => handle,
        Ok(Err(TurnCommitError::Unavailable(message))) => return refusal(12, message),
        Ok(Err(TurnCommitError::Stopping)) => return refusal(9, "the task is being stopped"),
        Ok(Err(err)) => return storage_refusal(store, &commit, &err.to_string()),
        Err(join) => return storage_refusal(store, &commit, &format!("snapshot task: {join}")),
    };
    // The head is read after the snapshot: it is the position of the round's
    // assistant message, which the recorder wrote before the guest saw it.
    let Some((log_seq, log_digest)) = heads.log_head(session_id) else {
        return refusal(9, "the host has no thread log head for this session");
    };
    if let Err(err) = store.record_turn_commit_pending(
        &bound.run_id,
        session_id,
        request.round,
        log_seq,
        &log_digest,
        &digest,
    ) {
        return refusal(13, err.to_string());
    }
    {
        let store = store.clone();
        let committer = committer.clone();
        let (run_id, round) = (bound.run_id.clone(), request.round);
        let commit = commit.clone();
        tokio::spawn(async move {
            match committer.extract(handle).await {
                Ok(hash) => {
                    if let Err(e) = store.set_turn_commit_manifest(&run_id, round, &hash) {
                        tracing::error!("turn commit {run_id}#{round} manifest not recorded: {e}");
                    }
                }
                Err(err) => {
                    let _ = store.fail_turn_commit(&run_id, round, "extraction failed");
                    emit_turn_commit_failed(&store, &commit, &err.to_string());
                }
            }
        });
    }
    CommitTurnResponse {
        accepted: true,
        error: None,
        state: TurnCommitState::Pending.wire() as i32,
        manifest_hash: String::new(),
    }
}

fn emit_turn_commit_failed(store: &SqliteWorkflowStore, commit: &TurnCommitRequest, detail: &str) {
    tracing::error!(
        "turn commit failed for run {} round {}: {detail}",
        commit.run_id,
        commit.round
    );
    crate::session_end::emit_commit_failed(
        store,
        &commit.run_id,
        &crate::session_end::commit_failed_payload(
            &commit.workspace_id,
            &commit.session_id,
            &commit.run_id,
            None,
            "the workspace snapshot of a turn could not be stored",
            "turn_commit",
        ),
    );
}

/// A storage failure while taking a turn commit: alert, and tell the guest
/// without any host detail. The session tree stays where it is; the guest ends
/// the task and the session-end commit keeps the work.
fn storage_refusal(
    store: &SqliteWorkflowStore,
    commit: &TurnCommitRequest,
    detail: &str,
) -> CommitTurnResponse {
    emit_turn_commit_failed(store, commit, detail);
    refusal(
        13,
        format!("{RUN_ERROR_STORAGE_COMMIT_FAILED}: the host could not snapshot the workspace"),
    )
}

#[cfg(test)]
mod tests;
