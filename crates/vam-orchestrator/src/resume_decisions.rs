//! One resume per parked run, and the decisions it carries.
//!
//! A run parked on several waits (a gate and a question, two tool calls) can have more than one
//! decision arrive at once. Exactly one of them may resume the run:
//!
//! - **The claim.** The decision that moves the run `PAUSED -> RUNNING` owns the resume
//!   ([`SqliteWorkflowStore::claim_parked_run`], one compare-and-set). A later decision only
//!   records itself; it never starts a second loop.
//! - **What the resume is told.** The owner reads every decision recorded for the run and not yet
//!   delivered ([`SqliteWorkflowStore::take_recorded_decisions`]) and marks them delivered, so
//!   the one task sees all of them.
//! - **A decision that lands after that** stays recorded. When the resumed task ends or parks,
//!   what is still undelivered is marked `late` ([`SqliteWorkflowStore::mark_decisions_late`]),
//!   and the next resume reports it as text. It never resumes anything.

use crate::error::{OrchestratorError, StoredApproval};
use crate::workflow::SqliteWorkflowStore;
use rusqlite::params;

/// `approvals.resume_state` of a decision the one resume was told about.
pub const RESUME_DELIVERED: &str = "delivered";
/// `approvals.resume_state` of a decision that landed after the resume took its decisions.
pub const RESUME_LATE: &str = "late";

/// The decisions a resume takes: the ones recorded for its park, and the late ones from an
/// earlier resume that nobody has told the task about yet.
#[derive(Debug, Default, Clone)]
pub struct RecordedDecisions {
    pub fresh: Vec<StoredApproval>,
    pub late: Vec<StoredApproval>,
}

impl SqliteWorkflowStore {
    /// Moves a parked run to `RUNNING`. True for the one caller that made the move: that caller
    /// owns the resume. False when the run is not parked (another decision already owns the
    /// resume, or the run is not waiting).
    pub fn claim_parked_run(&self, run_id: &str) -> Result<bool, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let transaction = connection.unchecked_transaction()?;
        let changed = transaction.execute(
            "UPDATE runs SET state = 'RUNNING', revision = revision + 1
             WHERE run_id = ?1 AND state = 'PAUSED'",
            params![run_id],
        )?;
        if changed == 1 {
            crate::workflow::append_outbox(&transaction, run_id, "vam.v1.run.state", "RUNNING")?;
        }
        transaction.commit()?;
        Ok(changed == 1)
    }

    /// How many published approvals of the run still wait for a decision.
    pub fn undecided_approval_count(&self, run_id: &str) -> Result<usize, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let n: i64 = connection.query_row(
            "SELECT COUNT(*) FROM approvals
             WHERE run_id = ?1 AND published = 1 AND decision IS NULL
               AND COALESCE(kind, 'tool_call') IN ('tool_call', 'gate', 'question')",
            [run_id],
            |row| row.get(0),
        )?;
        Ok(n as usize)
    }

    /// Every decision recorded for the run that no resume has been told about, oldest first.
    /// They are marked delivered in the same transaction, so a second caller gets none of them.
    pub fn take_recorded_decisions(
        &self,
        run_id: &str,
    ) -> Result<RecordedDecisions, OrchestratorError> {
        let ids: Vec<(String, Option<String>)> = {
            let connection = self
                .connection
                .lock()
                .map_err(|_| OrchestratorError::StorageUnavailable)?;
            let transaction = connection.unchecked_transaction()?;
            let rows = {
                let mut stmt = transaction.prepare(
                    "SELECT request_id, resume_state FROM approvals
                     WHERE run_id = ?1 AND published = 1
                       AND decision IN ('APPROVED', 'AMENDED', 'REJECTED', 'SUBMITTED')
                       AND COALESCE(kind, 'tool_call') IN ('tool_call', 'gate', 'question')
                       AND (resume_state IS NULL OR resume_state = ?2)
                     ORDER BY COALESCE(decided_at, 0), created_at, request_id",
                )?;
                let rows = stmt
                    .query_map(params![run_id, RESUME_LATE], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
                rows
            };
            for (request_id, _) in &rows {
                transaction.execute(
                    "UPDATE approvals SET resume_state = ?2 WHERE request_id = ?1",
                    params![request_id, RESUME_DELIVERED],
                )?;
            }
            transaction.commit()?;
            rows
        };
        let mut out = RecordedDecisions::default();
        for (request_id, state) in ids {
            let stored = self.get_approval(&request_id)?;
            if state.as_deref() == Some(RESUME_LATE) {
                out.late.push(stored);
            } else {
                out.fresh.push(stored);
            }
        }
        Ok(out)
    }

    /// Marks the decisions no resume was told about as late. Called when a resumed task ends or
    /// parks again. Returns the request ids marked.
    pub fn mark_decisions_late(&self, run_id: &str) -> Result<Vec<String>, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let transaction = connection.unchecked_transaction()?;
        let ids = {
            let mut stmt = transaction.prepare(
                "SELECT request_id FROM approvals
                 WHERE run_id = ?1 AND published = 1 AND resume_state IS NULL
                   AND decision IN ('APPROVED', 'AMENDED', 'REJECTED', 'SUBMITTED')
                   AND COALESCE(kind, 'tool_call') IN ('tool_call', 'gate', 'question')",
            )?;
            let ids = stmt
                .query_map([run_id], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            ids
        };
        for id in &ids {
            transaction.execute(
                "UPDATE approvals SET resume_state = ?2 WHERE request_id = ?1",
                params![id, RESUME_LATE],
            )?;
        }
        transaction.commit()?;
        Ok(ids)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vam_spec::v1::ResourceId;

    const RUN: &str = "urn:vam:run:local.vam.dev/acme/run-resume-decisions";

    fn store() -> SqliteWorkflowStore {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        store
            .create_run(&ResourceId {
                urn: RUN.to_string(),
            })
            .expect("run");
        store
    }

    /// A published approval of the run; `decision` None leaves it waiting.
    fn approval(store: &SqliteWorkflowStore, id: &str, kind: &str, decision: Option<&str>) {
        let connection = store.connection.lock().expect("connection");
        connection
            .execute(
                "INSERT INTO approvals (
                    request_id, run_id, step_id, attempt_id, checkpoint_digest,
                    continuation_generation, decision, published, kind, tool_name,
                    owner_user_id, approver_user_id, decided_at, created_at
                 ) VALUES (?1, ?2, 'step', 'attempt', 'digest', 1, ?3, 1, ?4, 'ask_user',
                    'local', 'local', 100, 100)",
                params![id, RUN, decision, kind],
            )
            .expect("insert approval");
    }

    fn state_of(store: &SqliteWorkflowStore, id: &str) -> Option<String> {
        let connection = store.connection.lock().expect("connection");
        connection
            .query_row(
                "SELECT resume_state FROM approvals WHERE request_id = ?1",
                [id],
                |row| row.get(0),
            )
            .expect("row")
    }

    #[test]
    fn only_one_caller_moves_a_parked_run_to_running() {
        let store = store();
        store.update_run_state(RUN, "PAUSED").expect("paused");
        assert!(store.claim_parked_run(RUN).expect("first claim"));
        assert!(!store.claim_parked_run(RUN).expect("second claim"));
        assert_eq!(store.get_run_state(RUN).expect("state"), "RUNNING");
        // A run that is not parked cannot be claimed either.
        store.update_run_state(RUN, "SUCCEEDED").expect("done");
        assert!(!store.claim_parked_run(RUN).expect("finished run"));
        assert_eq!(store.get_run_state(RUN).expect("state"), "SUCCEEDED");
    }

    #[test]
    fn a_resume_takes_every_recorded_decision_once() {
        let store = store();
        approval(&store, "gate-1", "gate", Some("SUBMITTED"));
        approval(&store, "question-1", "question", Some("SUBMITTED"));
        approval(&store, "waiting-1", "tool_call", None);
        assert_eq!(store.undecided_approval_count(RUN).expect("count"), 1);

        let taken = store.take_recorded_decisions(RUN).expect("take");
        let mut ids: Vec<&str> = taken.fresh.iter().map(|s| s.request_id.as_str()).collect();
        ids.sort();
        assert_eq!(ids, ["gate-1", "question-1"]);
        assert!(taken.late.is_empty());
        assert_eq!(
            state_of(&store, "gate-1").as_deref(),
            Some(RESUME_DELIVERED)
        );
        assert_eq!(state_of(&store, "waiting-1"), None);

        // A second resume of the same park is told nothing it was told before.
        let again = store.take_recorded_decisions(RUN).expect("take again");
        assert!(again.fresh.is_empty() && again.late.is_empty());
    }

    #[test]
    fn a_decision_after_the_read_is_late_and_the_next_resume_reports_it() {
        let store = store();
        approval(&store, "gate-1", "gate", Some("SUBMITTED"));
        let first = store.take_recorded_decisions(RUN).expect("take");
        assert_eq!(first.fresh.len(), 1);

        // The question is answered after the resume read its decisions.
        approval(&store, "question-1", "question", Some("SUBMITTED"));
        let late = store.mark_decisions_late(RUN).expect("mark late");
        assert_eq!(late, ["question-1"]);
        assert_eq!(state_of(&store, "question-1").as_deref(), Some(RESUME_LATE));
        // Nothing is marked twice, and the delivered one is not touched.
        assert!(store.mark_decisions_late(RUN).expect("again").is_empty());
        assert_eq!(
            state_of(&store, "gate-1").as_deref(),
            Some(RESUME_DELIVERED)
        );

        let next = store.take_recorded_decisions(RUN).expect("next resume");
        assert!(next.fresh.is_empty());
        assert_eq!(
            next.late
                .iter()
                .map(|s| s.request_id.as_str())
                .collect::<Vec<_>>(),
            ["question-1"]
        );
        assert_eq!(
            state_of(&store, "question-1").as_deref(),
            Some(RESUME_DELIVERED)
        );
    }

    #[test]
    fn an_expired_or_cancelled_wait_is_not_a_decision_to_tell() {
        let store = store();
        approval(&store, "expired-1", "gate", Some("EXPIRED"));
        approval(&store, "cancelled-1", "question", Some("CANCELLED"));
        let taken = store.take_recorded_decisions(RUN).expect("take");
        assert!(taken.fresh.is_empty() && taken.late.is_empty());
    }
}
