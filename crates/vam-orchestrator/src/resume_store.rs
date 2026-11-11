//! Storage for resuming a task (E10 Phase 4, AR-4.5): the `run_resume` row of a run that was
//! interrupted and waits to be picked up, and the persisted stop mark.
//!
//! **One owner at a time.** A run waiting to resume has a row in state `pending`. Only a
//! compare-and-set moves it out: the resume pass claims `pending -> resuming`, a stop or a cancel
//! takes `pending -> ended`, and an interrupt writes `-> pending`. Whoever loses the
//! compare-and-set leaves the run alone.
//!
//! **Two caps, counted apart** (the answer to Q10): a guest crash counts toward
//! [`MAX_CRASH_RESUMES`], a `vamd` restart toward [`MAX_RESTART_RESUMES`]. `RunStatus.resumes` is
//! their sum. A claim counts the attempt before the boot, so a boot error is an attempt of the
//! reason that caused the resume.
//!
//! **The stop mark** is the user's stop of a run, kept in the database (the in-memory record in
//! `session_end` is lost on a restart). While it exists a run is never moved to `pending`, so a
//! stop followed by a restart never resumes.

use rusqlite::{params, Connection, OptionalExtension};

use crate::error::OrchestratorError;
use crate::workflow::SqliteWorkflowStore;

/// Resumes after a guest crash a task gets; the next crash ends it `crashed_repeatedly`.
pub const MAX_CRASH_RESUMES: u32 = 2;
/// Resumes after a `vamd` restart a task gets; the next one ends it `restarted_repeatedly`.
pub const MAX_RESTART_RESUMES: u32 = 5;

pub const REASON_GUEST_CRASHED: &str = "guest_crashed";
pub const REASON_VAMD_RESTARTED: &str = "vamd_restarted";

/// Where a run stands in the resume cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeState {
    /// Interrupted; waits for the resume pass.
    Pending,
    /// The pass owns it and is booting a new session.
    Resuming,
    /// A resumed guest is working on it.
    Running,
    /// A stop or a cancel took it; it is never resumed.
    Ended,
}

impl ResumeState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Resuming => "resuming",
            Self::Running => "running",
            Self::Ended => "ended",
        }
    }

    fn parse(raw: &str) -> Option<Self> {
        match raw {
            "pending" => Some(Self::Pending),
            "resuming" => Some(Self::Resuming),
            "running" => Some(Self::Running),
            "ended" => Some(Self::Ended),
            _ => None,
        }
    }
}

/// The row of an interrupted run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeRow {
    pub run_id: String,
    pub state: ResumeState,
    pub reason: String,
    pub crashes: u32,
    pub restarts: u32,
    pub workspace_id: String,
    pub prior_session_id: String,
}

impl ResumeRow {
    /// Resumes so far, both kinds.
    pub fn total(&self) -> u32 {
        self.crashes + self.restarts
    }
}

/// What an interrupt's last step learned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingOutcome {
    /// The run waits for the resume pass.
    Pending,
    /// The user's stop is marked: the run is not resumed.
    Stopped,
}

/// What a claim by the resume pass learned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Claim {
    /// The pass owns the run now; the attempt is counted.
    Won(ResumeRow),
    /// The cap for this reason is reached: end the run with this reason.
    Capped {
        reason: &'static str,
        row: ResumeRow,
    },
    /// Someone else owns the run (or it is not pending).
    Lost,
}

pub(crate) fn initialize_schema(connection: &Connection) -> Result<(), OrchestratorError> {
    connection.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS run_resume (
            run_id TEXT PRIMARY KEY,
            state TEXT NOT NULL CHECK (state IN ('pending','resuming','running','ended')),
            reason TEXT NOT NULL,
            crashes INTEGER NOT NULL DEFAULT 0,
            restarts INTEGER NOT NULL DEFAULT 0,
            workspace_id TEXT NOT NULL,
            prior_session_id TEXT NOT NULL,
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_run_resume_workspace
            ON run_resume(workspace_id, state);
        CREATE TABLE IF NOT EXISTS run_stop_marks (
            run_id TEXT PRIMARY KEY,
            created_at INTEGER NOT NULL
        );
        ",
    )?;
    Ok(())
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

const COLUMNS: &str = "run_id, state, reason, crashes, restarts, workspace_id, prior_session_id";

fn read_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ResumeRow> {
    let state: String = row.get(1)?;
    Ok(ResumeRow {
        run_id: row.get(0)?,
        state: ResumeState::parse(&state).ok_or_else(|| {
            rusqlite::Error::FromSqlConversionFailure(
                1,
                rusqlite::types::Type::Text,
                Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("unknown resume state '{state}'"),
                )),
            )
        })?,
        reason: row.get(2)?,
        crashes: row.get::<_, i64>(3)?.max(0) as u32,
        restarts: row.get::<_, i64>(4)?.max(0) as u32,
        workspace_id: row.get(5)?,
        prior_session_id: row.get(6)?,
    })
}

fn get_locked(
    connection: &Connection,
    run_id: &str,
) -> Result<Option<ResumeRow>, OrchestratorError> {
    Ok(connection
        .query_row(
            &format!("SELECT {COLUMNS} FROM run_resume WHERE run_id = ?1"),
            params![run_id],
            read_row,
        )
        .optional()?)
}

impl SqliteWorkflowStore {
    fn resume_lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>, OrchestratorError> {
        self.connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)
    }

    /// Persists the user's stop of `run_id`.
    pub fn persist_run_stop(&self, run_id: &str) -> Result<(), OrchestratorError> {
        let connection = self.resume_lock()?;
        connection.execute(
            "INSERT OR IGNORE INTO run_stop_marks (run_id, created_at) VALUES (?1, ?2)",
            params![run_id, now_secs()],
        )?;
        Ok(())
    }

    pub fn run_stop_persisted(&self, run_id: &str) -> bool {
        self.resume_lock()
            .ok()
            .and_then(|c| {
                c.query_row(
                    "SELECT 1 FROM run_stop_marks WHERE run_id = ?1",
                    params![run_id],
                    |_| Ok(()),
                )
                .optional()
                .ok()
                .flatten()
            })
            .is_some()
    }

    /// Forgets the mark once the run has its terminal state.
    pub(crate) fn clear_persisted_run_stop(&self, run_id: &str) {
        if let Ok(connection) = self.resume_lock() {
            let _ = connection.execute(
                "DELETE FROM run_stop_marks WHERE run_id = ?1",
                params![run_id],
            );
        }
    }

    /// Whether the user's stop of the run is recorded, in memory or in the database.
    pub fn run_stop_known(&self, run_id: &str) -> bool {
        self.session_ends.run_stop_recorded(run_id) || self.run_stop_persisted(run_id)
    }

    /// The last step of an interrupt: the run waits to resume, unless the user's stop is marked
    /// (one transaction, so a stop and an interrupt cannot both win). Counts are kept.
    pub fn mark_resume_pending(
        &self,
        run_id: &str,
        reason: &str,
        workspace_id: &str,
        prior_session_id: &str,
    ) -> Result<PendingOutcome, OrchestratorError> {
        let connection = self.resume_lock()?;
        let marked = connection
            .query_row(
                "SELECT 1 FROM run_stop_marks WHERE run_id = ?1",
                params![run_id],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if marked || self.session_ends.run_stop_recorded(run_id) {
            return Ok(PendingOutcome::Stopped);
        }
        let now = now_secs();
        connection.execute(
            "INSERT INTO run_resume (run_id, state, reason, workspace_id, prior_session_id,
                created_at, updated_at)
             VALUES (?1, 'pending', ?2, ?3, ?4, ?5, ?5)
             ON CONFLICT(run_id) DO UPDATE SET state = 'pending', reason = excluded.reason,
                workspace_id = excluded.workspace_id,
                prior_session_id = excluded.prior_session_id, updated_at = excluded.updated_at
             WHERE run_resume.state != 'ended'",
            params![run_id, reason, workspace_id, prior_session_id, now],
        )?;
        drop(connection);
        self.resume_wake.notify_one();
        Ok(PendingOutcome::Pending)
    }

    pub fn resume_row(&self, run_id: &str) -> Result<Option<ResumeRow>, OrchestratorError> {
        let connection = self.resume_lock()?;
        get_locked(&connection, run_id)
    }

    /// Resumes this run has had (the sum of both kinds); 0 for a run never interrupted.
    pub fn run_resumes(&self, run_id: &str) -> u32 {
        self.resume_row(run_id)
            .ok()
            .flatten()
            .map(|row| row.total())
            .unwrap_or(0)
    }

    /// Runs waiting for the pass, oldest first.
    pub fn resume_pending_runs(&self) -> Result<Vec<ResumeRow>, OrchestratorError> {
        let connection = self.resume_lock()?;
        let mut statement = connection.prepare(&format!(
            "SELECT {COLUMNS} FROM run_resume WHERE state = 'pending' ORDER BY created_at, rowid"
        ))?;
        let rows = statement
            .query_map([], read_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// The pass takes a run: `pending -> resuming`, counting the attempt for its reason. At a
    /// reached cap nothing changes and the caller ends the run.
    pub fn claim_resume(&self, run_id: &str) -> Result<Claim, OrchestratorError> {
        let connection = self.resume_lock()?;
        let Some(row) = get_locked(&connection, run_id)? else {
            return Ok(Claim::Lost);
        };
        if row.state != ResumeState::Pending {
            return Ok(Claim::Lost);
        }
        let crash = row.reason == REASON_GUEST_CRASHED;
        if crash && row.crashes >= MAX_CRASH_RESUMES {
            return Ok(Claim::Capped {
                reason: "crashed_repeatedly",
                row,
            });
        }
        if !crash && row.restarts >= MAX_RESTART_RESUMES {
            return Ok(Claim::Capped {
                reason: "restarted_repeatedly",
                row,
            });
        }
        let column = if crash { "crashes" } else { "restarts" };
        let changed = connection.execute(
            &format!(
                "UPDATE run_resume SET state = 'resuming', {column} = {column} + 1,
                    updated_at = ?2 WHERE run_id = ?1 AND state = 'pending'"
            ),
            params![run_id, now_secs()],
        )?;
        if changed == 0 {
            return Ok(Claim::Lost);
        }
        Ok(get_locked(&connection, run_id)?
            .map(Claim::Won)
            .unwrap_or(Claim::Lost))
    }

    /// `resuming -> pending` after a boot error: the attempt stays counted.
    pub fn release_resume_claim(&self, run_id: &str) -> Result<bool, OrchestratorError> {
        let connection = self.resume_lock()?;
        let changed = connection.execute(
            "UPDATE run_resume SET state = 'pending', updated_at = ?2
             WHERE run_id = ?1 AND state = 'resuming'",
            params![run_id, now_secs()],
        )?;
        drop(connection);
        if changed > 0 {
            self.resume_wake.notify_one();
        }
        Ok(changed > 0)
    }

    /// A boot that failed took the resume key of the session it booted from: the next attempt
    /// resumes from the failed session instead.
    pub fn set_resume_prior_session(
        &self,
        run_id: &str,
        session_id: &str,
    ) -> Result<(), OrchestratorError> {
        let connection = self.resume_lock()?;
        connection.execute(
            "UPDATE run_resume SET prior_session_id = ?2, updated_at = ?3 WHERE run_id = ?1",
            params![run_id, session_id, now_secs()],
        )?;
        Ok(())
    }

    /// `resuming -> running`: a resumed guest has the task.
    pub fn mark_resume_running(&self, run_id: &str) -> Result<(), OrchestratorError> {
        let connection = self.resume_lock()?;
        connection.execute(
            "UPDATE run_resume SET state = 'running', updated_at = ?2
             WHERE run_id = ?1 AND state = 'resuming'",
            params![run_id, now_secs()],
        )?;
        Ok(())
    }

    /// `pending -> ended` (a stop or a cancel). `true` when this call took the run.
    pub fn end_resume_pending(&self, run_id: &str) -> Result<bool, OrchestratorError> {
        let connection = self.resume_lock()?;
        let changed = connection.execute(
            "UPDATE run_resume SET state = 'ended', updated_at = ?2
             WHERE run_id = ?1 AND state = 'pending'",
            params![run_id, now_secs()],
        )?;
        Ok(changed > 0)
    }

    /// The pass ends a claimed run itself (a cap, a refusal): `resuming -> ended`.
    pub fn end_resume_claimed(&self, run_id: &str) -> Result<(), OrchestratorError> {
        let connection = self.resume_lock()?;
        connection.execute(
            "UPDATE run_resume SET state = 'ended', updated_at = ?2
             WHERE run_id = ?1 AND state IN ('pending','resuming')",
            params![run_id, now_secs()],
        )?;
        Ok(())
    }

    /// The run a resume holds this workspace for (state `pending` or `resuming`, run not ended).
    /// A new task or a fire must not start on the workspace meanwhile.
    pub fn resume_holding_workspace(
        &self,
        workspace_id: &str,
    ) -> Result<Option<String>, OrchestratorError> {
        let connection = self.resume_lock()?;
        Ok(connection
            .query_row(
                "SELECT r.run_id FROM run_resume r JOIN runs ON runs.run_id = r.run_id
                 WHERE r.workspace_id = ?1 AND r.state IN ('pending','resuming')
                   AND runs.state NOT IN ('SUCCEEDED','FAILED','CANCELLED','TIMED_OUT','EXPIRED',
                                          'REJECTED')
                 ORDER BY r.created_at, r.rowid LIMIT 1",
                params![workspace_id],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// Rows a previous `vamd` left `resuming`: its pass died with it. The sessions they booted
    /// are settled as dead trees, which writes the row back to `pending`; this only makes a row
    /// whose session never existed claimable again.
    pub fn reset_stale_resume_claims(&self) -> Result<usize, OrchestratorError> {
        let connection = self.resume_lock()?;
        Ok(connection.execute(
            "UPDATE run_resume SET state = 'pending', updated_at = ?1 WHERE state = 'resuming'",
            params![now_secs()],
        )?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_with_run(run: &str) -> SqliteWorkflowStore {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        store
            .create_run(&vam_spec::v1::ResourceId {
                urn: run.to_string(),
            })
            .expect("run");
        store.update_run_state(run, "RUNNING").expect("running");
        store
    }

    const RUN: &str = "urn:vam:run:local.vam.dev/acme/resume-store";

    #[test]
    fn crashes_and_restarts_are_counted_apart_and_capped_apart() {
        let store = store_with_run(RUN);
        for expected in 1..=MAX_CRASH_RESUMES {
            store
                .mark_resume_pending(RUN, REASON_GUEST_CRASHED, "ws", "s1")
                .unwrap();
            match store.claim_resume(RUN).unwrap() {
                Claim::Won(row) => assert_eq!(row.crashes, expected),
                other => panic!("{other:?}"),
            }
        }
        // A restart still has its own budget.
        store
            .mark_resume_pending(RUN, REASON_VAMD_RESTARTED, "ws", "s2")
            .unwrap();
        match store.claim_resume(RUN).unwrap() {
            Claim::Won(row) => {
                assert_eq!((row.crashes, row.restarts, row.total()), (2, 1, 3))
            }
            other => panic!("{other:?}"),
        }
        // The third crash is over its cap.
        store
            .mark_resume_pending(RUN, REASON_GUEST_CRASHED, "ws", "s3")
            .unwrap();
        assert!(matches!(
            store.claim_resume(RUN).unwrap(),
            Claim::Capped {
                reason: "crashed_repeatedly",
                ..
            }
        ));
        assert_eq!(store.run_resumes(RUN), 3);
    }

    #[test]
    fn the_restart_cap_ends_with_its_own_reason() {
        let store = store_with_run(RUN);
        for _ in 0..MAX_RESTART_RESUMES {
            store
                .mark_resume_pending(RUN, REASON_VAMD_RESTARTED, "ws", "s")
                .unwrap();
            assert!(matches!(store.claim_resume(RUN).unwrap(), Claim::Won(_)));
        }
        store
            .mark_resume_pending(RUN, REASON_VAMD_RESTARTED, "ws", "s")
            .unwrap();
        assert!(matches!(
            store.claim_resume(RUN).unwrap(),
            Claim::Capped {
                reason: "restarted_repeatedly",
                ..
            }
        ));
    }

    #[test]
    fn only_one_owner_leaves_pending() {
        let store = store_with_run(RUN);
        store
            .mark_resume_pending(RUN, REASON_VAMD_RESTARTED, "ws", "s")
            .unwrap();
        // A stop takes it: the pass loses.
        assert!(store.end_resume_pending(RUN).unwrap());
        assert_eq!(store.claim_resume(RUN).unwrap(), Claim::Lost);
        // And the other way round: the pass takes it, the stop loses.
        let other = "urn:vam:run:local.vam.dev/acme/resume-store-2";
        store
            .create_run(&vam_spec::v1::ResourceId { urn: other.into() })
            .unwrap();
        store
            .mark_resume_pending(other, REASON_GUEST_CRASHED, "ws", "s")
            .unwrap();
        assert!(matches!(store.claim_resume(other).unwrap(), Claim::Won(_)));
        assert!(!store.end_resume_pending(other).unwrap());
        assert_eq!(store.claim_resume(other).unwrap(), Claim::Lost);
    }

    #[test]
    fn a_persisted_stop_keeps_a_run_from_pending() {
        let store = store_with_run(RUN);
        store.persist_run_stop(RUN).unwrap();
        assert_eq!(
            store
                .mark_resume_pending(RUN, REASON_VAMD_RESTARTED, "ws", "s")
                .unwrap(),
            PendingOutcome::Stopped
        );
        assert!(store.resume_row(RUN).unwrap().is_none());
        store.clear_persisted_run_stop(RUN);
        assert_eq!(
            store
                .mark_resume_pending(RUN, REASON_VAMD_RESTARTED, "ws", "s")
                .unwrap(),
            PendingOutcome::Pending
        );
    }

    #[test]
    fn a_waiting_run_holds_its_workspace_until_it_ends() {
        let store = store_with_run(RUN);
        store
            .mark_resume_pending(RUN, REASON_GUEST_CRASHED, "ws-held", "s")
            .unwrap();
        assert_eq!(
            store
                .resume_holding_workspace("ws-held")
                .unwrap()
                .as_deref(),
            Some(RUN)
        );
        assert!(store
            .resume_holding_workspace("ws-other")
            .unwrap()
            .is_none());
        store.update_run_state(RUN, "CANCELLED").unwrap();
        assert!(store.resume_holding_workspace("ws-held").unwrap().is_none());
    }

    #[test]
    fn a_claim_that_failed_to_boot_goes_back_to_pending_with_its_count() {
        let store = store_with_run(RUN);
        store
            .mark_resume_pending(RUN, REASON_GUEST_CRASHED, "ws", "s")
            .unwrap();
        assert!(matches!(store.claim_resume(RUN).unwrap(), Claim::Won(_)));
        assert!(store.release_resume_claim(RUN).unwrap());
        let row = store.resume_row(RUN).unwrap().unwrap();
        assert_eq!((row.state, row.crashes), (ResumeState::Pending, 1));
    }
}
