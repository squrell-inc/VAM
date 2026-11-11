//! Storage for the run inbox (E10 Phase 3, AR-3.3): messages a user sends to a
//! thread while a task is live. Contract and storage only; the behaviour that
//! reads and writes these rows comes with the inbox API (AR-3.5 onwards).
//!
//! One row per message. `mode` is `STEER` (delivered to the live run at its
//! next turn boundary) or `QUEUE` (starts as the next task). A queued row keeps
//! the whole run spec it will start and its own `thread_ref` and
//! `workspace_id`, so a message for another thread on a busy workspace waits
//! as a row too (owner decision D).

use rusqlite::{params, Connection, OptionalExtension};

use crate::error::OrchestratorError;
use crate::workflow::SqliteWorkflowStore;

pub const STATE_PENDING: &str = "pending";
pub const STATE_DELIVERED: &str = "delivered";
pub const STATE_QUEUED: &str = "queued";
pub const STATE_STARTED: &str = "started";
pub const STATE_DISCARDED: &str = "discarded";

/// Undelivered messages one run may hold (429 `inbox_full` beyond it).
pub const MAX_PENDING_PER_RUN: usize = 20;
/// Longest message text, in bytes.
pub const MAX_TEXT_BYTES: usize = 32 * 1024;

/// Creates the table and indexes. Safe to run on every start.
pub(crate) fn initialize_schema(connection: &Connection) -> Result<(), OrchestratorError> {
    connection.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS run_inbox (
            run_id TEXT NOT NULL,
            seq INTEGER NOT NULL,
            mode TEXT NOT NULL CHECK (mode IN ('STEER','QUEUE')),
            idempotency_key TEXT NOT NULL,
            owner_user_id TEXT NOT NULL,
            text TEXT NOT NULL,
            thread_ref TEXT NOT NULL DEFAULT 'main',
            workspace_id TEXT NOT NULL DEFAULT '',
            queued_spec_json TEXT,
            state TEXT NOT NULL CHECK (state IN
                ('pending','delivered','queued','started','discarded')),
            created_at INTEGER NOT NULL,
            delivered_at INTEGER,
            log_record_id TEXT,
            started_run_id TEXT,
            PRIMARY KEY (run_id, seq)
        );
        CREATE UNIQUE INDEX IF NOT EXISTS idx_run_inbox_idem
            ON run_inbox(owner_user_id, run_id, idempotency_key);
        CREATE INDEX IF NOT EXISTS idx_run_inbox_workspace_state
            ON run_inbox(workspace_id, state);
        ",
    )?;
    // The credential a triggered queued fire came from (NULL for a person). Additive.
    let _ = connection.execute("ALTER TABLE run_inbox ADD COLUMN credential_id TEXT", []);
    // Backstop for "one queued fire per credential": the admit check is the answer, this
    // makes a race lose cleanly.
    connection.execute_batch(
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_run_inbox_one_queued_per_credential
            ON run_inbox(credential_id)
            WHERE state = 'queued' AND credential_id IS NOT NULL;",
    )?;
    Ok(())
}

/// A message to store.
#[derive(Debug, Clone)]
pub struct NewInboxRow {
    pub run_id: String,
    pub mode: String,
    pub idempotency_key: String,
    pub owner_user_id: String,
    pub text: String,
    pub thread_ref: String,
    pub workspace_id: String,
    /// The `RunSpec` JSON a QUEUE message will start. None for a steer.
    pub queued_spec_json: Option<String>,
}

/// A stored message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboxRow {
    pub run_id: String,
    pub seq: i64,
    pub mode: String,
    pub idempotency_key: String,
    pub owner_user_id: String,
    pub text: String,
    pub thread_ref: String,
    pub workspace_id: String,
    pub queued_spec_json: Option<String>,
    pub state: String,
    pub created_at: i64,
    pub delivered_at: Option<i64>,
    pub log_record_id: Option<String>,
    pub started_run_id: Option<String>,
}

const COLUMNS: &str = "run_id, seq, mode, idempotency_key, owner_user_id, text, thread_ref,
    workspace_id, queued_spec_json, state, created_at, delivered_at, log_record_id,
    started_run_id";

fn read_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<InboxRow> {
    Ok(InboxRow {
        run_id: row.get(0)?,
        seq: row.get(1)?,
        mode: row.get(2)?,
        idempotency_key: row.get(3)?,
        owner_user_id: row.get(4)?,
        text: row.get(5)?,
        thread_ref: row.get(6)?,
        workspace_id: row.get(7)?,
        queued_spec_json: row.get(8)?,
        state: row.get(9)?,
        created_at: row.get(10)?,
        delivered_at: row.get(11)?,
        log_record_id: row.get(12)?,
        started_run_id: row.get(13)?,
    })
}

/// Ends the mark of [`SqliteWorkflowStore::begin_queue_start`] on drop.
pub struct QueueStartGuard {
    store: std::sync::Arc<SqliteWorkflowStore>,
    run_id: String,
}

impl Drop for QueueStartGuard {
    fn drop(&mut self) {
        if let Ok(mut set) = self.store.queue_starting.lock() {
            set.remove(&self.run_id);
        }
    }
}

/// The PENDING run behind a queued message.
#[derive(Debug, Clone)]
pub struct NewQueuedRun {
    pub run_id: String,
    /// The resolved spec, for the run's status (thread, workspace, prompt).
    pub spec_json: String,
    pub agent_urn: String,
    pub idempotency_key: Option<String>,
    pub owner_principal: String,
    /// `broker:<id>` for a triggered run, None for a person.
    pub source: Option<String>,
    /// The grant credential of a triggered run.
    pub credential_id: Option<String>,
}

/// Cancels a PENDING run that has no session. `true` when it was one.
fn cancel_queued_run(
    transaction: &rusqlite::Transaction<'_>,
    run_id: &str,
    reason: &str,
) -> Result<bool, OrchestratorError> {
    let result = serde_json::json!({ "error": reason, "reason": reason }).to_string();
    let changed = transaction.execute(
        "UPDATE runs SET state = 'CANCELLED', revision = revision + 1, result_json = ?2
         WHERE run_id = ?1 AND state = 'PENDING' AND session_id IS NULL",
        params![run_id, result],
    )?;
    if changed > 0 {
        crate::workflow::append_outbox(transaction, run_id, "vam.v1.run.state", "CANCELLED")?;
        crate::workflow::append_outbox(transaction, run_id, "vam.v1.run.completed", &result)?;
    }
    Ok(changed > 0)
}

/// The insert of [`SqliteWorkflowStore::insert_inbox_message_checked`], on a connection
/// the caller already holds.
fn insert_row_locked(
    connection: &Connection,
    new: &NewInboxRow,
) -> Result<(InboxRow, bool), OrchestratorError> {
    let by_key = format!(
        "SELECT {COLUMNS} FROM run_inbox
         WHERE owner_user_id = ?1 AND run_id = ?2 AND idempotency_key = ?3"
    );
    let key_params = params![new.owner_user_id, new.run_id, new.idempotency_key];
    if let Some(row) = connection
        .query_row(&by_key, key_params, read_row)
        .optional()?
    {
        return Ok((row, false));
    }
    let run_state: Option<String> = connection
        .query_row(
            "SELECT state FROM runs WHERE run_id = ?1",
            params![new.run_id],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(state) = run_state {
        if matches!(
            state.as_str(),
            "SUCCEEDED" | "FAILED" | "CANCELLED" | "TIMED_OUT" | "EXPIRED" | "REJECTED"
        ) {
            return Err(OrchestratorError::Conflict(format!(
                "run '{}' has ended",
                new.run_id
            )));
        }
    }
    let pending: i64 = connection.query_row(
        "SELECT COUNT(*) FROM run_inbox WHERE run_id = ?1 AND state IN ('pending','queued')",
        params![new.run_id],
        |r| r.get(0),
    )?;
    if pending as usize >= MAX_PENDING_PER_RUN {
        return Err(OrchestratorError::InboxFull {
            run_id: new.run_id.clone(),
            pending: pending as usize,
        });
    }
    let state = if new.mode == "QUEUE" {
        STATE_QUEUED
    } else {
        STATE_PENDING
    };
    connection.execute(
        "INSERT INTO run_inbox
           (run_id, seq, mode, idempotency_key, owner_user_id, text, thread_ref,
            workspace_id, queued_spec_json, state, created_at)
         VALUES (?1,
           (SELECT COALESCE(MAX(seq), 0) + 1 FROM run_inbox WHERE run_id = ?1),
           ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            new.run_id,
            new.mode,
            new.idempotency_key,
            new.owner_user_id,
            new.text,
            new.thread_ref,
            new.workspace_id,
            new.queued_spec_json,
            state,
            crate::catalog::unix_timestamp()
        ],
    )?;
    Ok((connection.query_row(&by_key, key_params, read_row)?, true))
}

impl SqliteWorkflowStore {
    /// Stores a message with the run's next `seq`. A repeat of the same
    /// (owner, run, idempotency key) returns the stored row unchanged. A steer
    /// starts `pending`; a queued message starts `queued`.
    pub fn insert_inbox_message(&self, new: &NewInboxRow) -> Result<InboxRow, OrchestratorError> {
        self.insert_inbox_message_checked(new).map(|(row, _)| row)
    }

    /// [`Self::insert_inbox_message`] that also says whether the row is new (a repeated
    /// idempotency key returns the stored row, `false`). Refuses a run that has ended:
    /// the check and the insert share one lock, so a message cannot slip in after the end.
    pub fn insert_inbox_message_checked(
        &self,
        new: &NewInboxRow,
    ) -> Result<(InboxRow, bool), OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        insert_row_locked(&connection, new)
    }

    /// Stores a queued message together with the run it will start: a PENDING row with no
    /// session (AR-3.8), readable as a run so the client can follow it. One lock, so the
    /// message never exists without its run. A repeated key returns the stored row.
    pub fn insert_queued_message(
        &self,
        new: &NewInboxRow,
        queued: &NewQueuedRun,
    ) -> Result<(InboxRow, bool), OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let transaction = connection.unchecked_transaction()?;
        let (row, is_new) = insert_row_locked(&transaction, new)?;
        if !is_new {
            return Ok((row, false));
        }
        transaction.execute(
            "INSERT INTO runs (run_id, state, revision, spec_json, session_id, agent_urn,
                               idempotency_key, owner_principal, owner_user_id, created_at)
             VALUES (?1, 'PENDING', 0, ?2, NULL, ?3, ?4, ?5, ?6, ?7)",
            params![
                queued.run_id,
                queued.spec_json,
                queued.agent_urn,
                queued.idempotency_key,
                queued.owner_principal,
                crate::owner::resolved_owner(&new.owner_user_id),
                crate::catalog::unix_timestamp()
            ],
        )?;
        transaction.execute(
            "UPDATE run_inbox SET started_run_id = ?3 WHERE run_id = ?1 AND seq = ?2",
            params![row.run_id, row.seq, queued.run_id],
        )?;
        if queued.source.is_some() || queued.credential_id.is_some() {
            transaction.execute(
                "UPDATE runs SET source = ?2, credential_id = ?3 WHERE run_id = ?1",
                params![queued.run_id, queued.source, queued.credential_id],
            )?;
            transaction.execute(
                "UPDATE run_inbox SET credential_id = ?3 WHERE run_id = ?1 AND seq = ?2",
                params![row.run_id, row.seq, queued.credential_id],
            )?;
        }
        crate::workflow::append_outbox(
            &transaction,
            &queued.run_id,
            "vam.v1.run.created",
            "queued",
        )?;
        transaction.commit()?;
        let mut row = row;
        row.started_run_id = Some(queued.run_id.clone());
        Ok((row, true))
    }

    /// Discards the run's undelivered messages (pending steers and queued messages) and
    /// returns them, text included, for the stop answer. Their text is cleared (decision 23)
    /// and the PENDING run of a queued message is cancelled.
    pub fn discard_run_inbox(&self, run_id: &str) -> Result<Vec<InboxRow>, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let transaction = connection.unchecked_transaction()?;
        let rows = {
            let mut statement = transaction.prepare(&format!(
                "SELECT {COLUMNS} FROM run_inbox
                 WHERE run_id = ?1 AND state IN ('pending','queued') ORDER BY seq"
            ))?;
            let rows = statement
                .query_map(params![run_id], read_row)?
                .collect::<Result<Vec<_>, _>>()?;
            rows
        };
        for row in &rows {
            transaction.execute(
                "UPDATE run_inbox SET state = 'discarded', text = '', queued_spec_json = NULL
                 WHERE run_id = ?1 AND seq = ?2",
                params![row.run_id, row.seq],
            )?;
            if let Some(started) = row
                .started_run_id
                .as_deref()
                .filter(|_| row.mode == "QUEUE")
            {
                cancel_queued_run(&transaction, started, "discarded")?;
            }
        }
        transaction.commit()?;
        Ok(rows)
    }

    /// Discards the queued message whose run is `started_run_id` (its run is cancelled).
    pub fn discard_queued_message(
        &self,
        started_run_id: &str,
    ) -> Result<Option<InboxRow>, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let transaction = connection.unchecked_transaction()?;
        let row = transaction
            .query_row(
                &format!(
                    "SELECT {COLUMNS} FROM run_inbox
                     WHERE started_run_id = ?1 AND mode = 'QUEUE' AND state = 'queued'"
                ),
                params![started_run_id],
                read_row,
            )
            .optional()?;
        if let Some(row) = &row {
            transaction.execute(
                "UPDATE run_inbox SET state = 'discarded', text = '', queued_spec_json = NULL
                 WHERE run_id = ?1 AND seq = ?2",
                params![row.run_id, row.seq],
            )?;
            cancel_queued_run(&transaction, started_run_id, "discarded")?;
        }
        transaction.commit()?;
        Ok(row)
    }

    /// Discards the queued fires of grants: `credential_id` names one grant, `owner_user_id`
    /// every grant of a user. Each queued run ends CANCELLED with `reason` (not left PENDING).
    /// Returns the cancelled run ids.
    pub fn discard_queued_fires(
        &self,
        credential_id: Option<&str>,
        owner_user_id: Option<&str>,
        reason: &str,
    ) -> Result<Vec<String>, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let transaction = connection.unchecked_transaction()?;
        let rows: Vec<InboxRow> = {
            let mut stmt = transaction.prepare(&format!(
                "SELECT {COLUMNS} FROM run_inbox
                 WHERE state = 'queued' AND credential_id IS NOT NULL
                   AND (?1 IS NULL OR credential_id = ?1)
                   AND (?2 IS NULL OR credential_id IN
                        (SELECT key_id FROM api_keys WHERE owner_user_id = ?2))"
            ))?;
            let mapped = stmt.query_map(params![credential_id, owner_user_id], read_row)?;
            mapped.collect::<Result<Vec<_>, _>>()?
        };
        let mut ended = Vec::new();
        for row in &rows {
            transaction.execute(
                "UPDATE run_inbox SET state = 'discarded', text = '', queued_spec_json = NULL
                 WHERE run_id = ?1 AND seq = ?2",
                params![row.run_id, row.seq],
            )?;
            if let Some(run) = &row.started_run_id {
                if cancel_queued_run(&transaction, run, reason)? {
                    ended.push(run.clone());
                }
            }
        }
        transaction.commit()?;
        drop(connection);
        if !ended.is_empty() {
            self.queue_wake.notify_one();
        }
        Ok(ended)
    }

    /// Every queued message, oldest first (any workspace).
    pub fn list_queued(&self) -> Result<Vec<InboxRow>, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let mut statement = connection.prepare(&format!(
            "SELECT {COLUMNS} FROM run_inbox WHERE state = 'queued'
             ORDER BY created_at, run_id, seq"
        ))?;
        let rows = statement.query_map([], read_row)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Runs that have ended and still hold undelivered steers.
    pub fn ended_runs_with_pending_steers(&self) -> Result<Vec<String>, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let mut statement = connection.prepare(
            "SELECT DISTINCT i.run_id FROM run_inbox i JOIN runs r ON r.run_id = i.run_id
             WHERE i.mode = 'STEER' AND i.state = 'pending'
               AND r.state IN ('SUCCEEDED','FAILED','CANCELLED','TIMED_OUT','EXPIRED','REJECTED')
             ORDER BY i.run_id",
        )?;
        let rows = statement.query_map([], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Marks a queued message started by the run it was waiting to become.
    pub fn mark_queued_started(&self, run_id: &str, seq: i64) -> Result<bool, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let changed = connection.execute(
            "UPDATE run_inbox SET state = 'started', text = '', queued_spec_json = NULL
             WHERE run_id = ?1 AND seq = ?2 AND state = 'queued'",
            params![run_id, seq],
        )?;
        Ok(changed > 0)
    }

    /// Replaces the undelivered steers `seqs` of an ended run with one queued message
    /// (their texts, joined by a blank line, are its text). The steers become `started`
    /// pointing at the new run, their text cleared. `None` when the run is not terminal
    /// or a row changed under us.
    pub fn merge_steers_into_queued(
        &self,
        run_id: &str,
        seqs: &[i64],
        new: &NewInboxRow,
        queued: &NewQueuedRun,
    ) -> Result<Option<InboxRow>, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let transaction = connection.unchecked_transaction()?;
        let ended: bool = transaction
            .query_row(
                "SELECT state IN ('SUCCEEDED','FAILED','CANCELLED','TIMED_OUT','EXPIRED','REJECTED')
                 FROM runs WHERE run_id = ?1",
                params![run_id],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(false);
        if !ended {
            return Ok(None);
        }
        // The queued row is inserted after the end check above, so it skips the
        // "run has ended" refusal of the live path.
        let seq: i64 = transaction.query_row(
            "SELECT COALESCE(MAX(seq), 0) + 1 FROM run_inbox WHERE run_id = ?1",
            params![run_id],
            |r| r.get(0),
        )?;
        transaction.execute(
            "INSERT INTO run_inbox
               (run_id, seq, mode, idempotency_key, owner_user_id, text, thread_ref,
                workspace_id, queued_spec_json, state, created_at, started_run_id)
             VALUES (?1, ?2, 'QUEUE', ?3, ?4, ?5, ?6, ?7, ?8, 'queued', ?9, ?10)",
            params![
                run_id,
                seq,
                new.idempotency_key,
                new.owner_user_id,
                new.text,
                new.thread_ref,
                new.workspace_id,
                new.queued_spec_json,
                crate::catalog::unix_timestamp(),
                queued.run_id
            ],
        )?;
        for steer in seqs {
            let changed = transaction.execute(
                "UPDATE run_inbox SET state = 'started', text = '', started_run_id = ?3
                 WHERE run_id = ?1 AND seq = ?2 AND mode = 'STEER' AND state = 'pending'",
                params![run_id, steer, queued.run_id],
            )?;
            if changed == 0 {
                return Ok(None);
            }
        }
        transaction.execute(
            "INSERT INTO runs (run_id, state, revision, spec_json, session_id, agent_urn,
                               idempotency_key, owner_principal, owner_user_id, created_at)
             VALUES (?1, 'PENDING', 0, ?2, NULL, ?3, ?4, ?5, ?6, ?7)",
            params![
                queued.run_id,
                queued.spec_json,
                queued.agent_urn,
                queued.idempotency_key,
                queued.owner_principal,
                crate::owner::resolved_owner(&new.owner_user_id),
                crate::catalog::unix_timestamp()
            ],
        )?;
        crate::workflow::append_outbox(
            &transaction,
            &queued.run_id,
            "vam.v1.run.created",
            "queued",
        )?;
        let row = transaction.query_row(
            &format!("SELECT {COLUMNS} FROM run_inbox WHERE run_id = ?1 AND seq = ?2"),
            params![run_id, seq],
            read_row,
        )?;
        transaction.commit()?;
        Ok(Some(row))
    }

    /// Marks steers delivered without an acknowledgement from the guest: the thread
    /// log already holds them (the guest applied them, then died before acknowledging).
    pub fn mark_steers_delivered_by_seq(
        &self,
        run_id: &str,
        seqs: &[i64],
    ) -> Result<Vec<i64>, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let mut done = Vec::new();
        for seq in seqs {
            let changed = connection.execute(
                "UPDATE run_inbox SET state = 'delivered', text = '', delivered_at = ?3
                 WHERE run_id = ?1 AND seq = ?2 AND mode = 'STEER' AND state = 'pending'",
                params![run_id, seq, crate::catalog::unix_timestamp()],
            )?;
            if changed > 0 {
                done.push(*seq);
            }
        }
        Ok(done)
    }

    /// Who started a run (the principal string it was stored with) and its owner.
    pub fn run_principal_and_owner(
        &self,
        run_id: &str,
    ) -> Result<(String, String), OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        connection
            .query_row(
                "SELECT COALESCE(owner_principal, ''), COALESCE(owner_user_id, 'local')
                 FROM runs WHERE run_id = ?1",
                params![run_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .ok_or_else(|| OrchestratorError::NotFound(format!("run '{run_id}' not found")))
    }

    /// The starter is starting this queued run right now.
    pub fn queue_start_in_progress(&self, run_id: &str) -> bool {
        self.queue_starting
            .lock()
            .map(|set| set.contains(run_id))
            .unwrap_or(false)
    }

    /// Marks the start of a queued run; the mark ends when the guard drops.
    pub fn begin_queue_start(self: &std::sync::Arc<Self>, run_id: &str) -> QueueStartGuard {
        if let Ok(mut set) = self.queue_starting.lock() {
            set.insert(run_id.to_string());
        }
        QueueStartGuard {
            store: self.clone(),
            run_id: run_id.to_string(),
        }
    }

    /// The run behind a queued message is only a PENDING row with no session.
    pub fn is_queued_run(&self, run_id: &str) -> bool {
        let Ok(connection) = self.connection.lock() else {
            return false;
        };
        connection
            .query_row(
                "SELECT 1 FROM runs WHERE run_id = ?1 AND state = 'PENDING' AND session_id IS NULL",
                params![run_id],
                |_| Ok(()),
            )
            .optional()
            .ok()
            .flatten()
            .is_some()
    }

    /// Ends a queued run that could not start: FAILED with the reason, so the person who
    /// sent it can see why. `false` when it was not a PENDING run with no session.
    pub fn fail_queued_run(&self, run_id: &str, message: &str) -> Result<bool, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let transaction = connection.unchecked_transaction()?;
        let result =
            serde_json::json!({ "error": message, "reason": "queue_start_failed" }).to_string();
        let changed = transaction.execute(
            "UPDATE runs SET state = 'FAILED', revision = revision + 1, result_json = ?2
             WHERE run_id = ?1 AND state = 'PENDING' AND session_id IS NULL",
            params![run_id, result],
        )?;
        if changed > 0 {
            crate::workflow::append_outbox(&transaction, run_id, "vam.v1.run.state", "FAILED")?;
            crate::workflow::append_outbox(&transaction, run_id, "vam.v1.run.completed", &result)?;
        }
        transaction.commit()?;
        drop(connection);
        if changed > 0 {
            self.queue_wake.notify_one();
        }
        Ok(changed > 0)
    }

    /// Ends a queued run whose window passed (`not_after`): EXPIRED, never started. `false`
    /// when it was not a PENDING run with no session.
    pub fn expire_queued_run(
        &self,
        run_id: &str,
        message: &str,
    ) -> Result<bool, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let transaction = connection.unchecked_transaction()?;
        let result =
            serde_json::json!({ "error": message, "reason": "queued_fire_expired" }).to_string();
        let changed = transaction.execute(
            "UPDATE runs SET state = 'EXPIRED', revision = revision + 1, result_json = ?2
             WHERE run_id = ?1 AND state = 'PENDING' AND session_id IS NULL",
            params![run_id, result],
        )?;
        if changed > 0 {
            crate::workflow::append_outbox(&transaction, run_id, "vam.v1.run.state", "EXPIRED")?;
            crate::workflow::append_outbox(&transaction, run_id, "vam.v1.run.completed", &result)?;
        }
        transaction.commit()?;
        drop(connection);
        if changed > 0 {
            self.queue_wake.notify_one();
        }
        Ok(changed > 0)
    }

    /// Whether this credential already has a fire waiting in a queue.
    pub fn credential_has_queued_fire(
        &self,
        credential_id: &str,
    ) -> Result<bool, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let n: i64 = connection.query_row(
            "SELECT COUNT(*) FROM run_inbox WHERE credential_id = ?1 AND state = 'queued'",
            params![credential_id],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    /// Marks a queued message discarded without touching its run (the user cancelled it).
    pub fn mark_queued_discarded(&self, run_id: &str, seq: i64) -> Result<bool, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let changed = connection.execute(
            "UPDATE run_inbox SET state = 'discarded', text = '', queued_spec_json = NULL
             WHERE run_id = ?1 AND seq = ?2 AND state = 'queued'",
            params![run_id, seq],
        )?;
        Ok(changed > 0)
    }

    /// Cancels a queued run (its message was discarded or could not start).
    pub fn cancel_queued_run(&self, run_id: &str, reason: &str) -> Result<bool, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let transaction = connection.unchecked_transaction()?;
        let cancelled = cancel_queued_run(&transaction, run_id, reason)?;
        transaction.commit()?;
        Ok(cancelled)
    }

    /// Pending steers after `after_seq`, in `seq` order, at most `budget` bytes of text
    /// (always at least one when any is pending, so a reply makes progress).
    pub fn pending_steers_after(
        &self,
        run_id: &str,
        after_seq: i64,
        budget: usize,
    ) -> Result<Vec<InboxRow>, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let mut statement = connection.prepare(&format!(
            "SELECT {COLUMNS} FROM run_inbox
             WHERE run_id = ?1 AND mode = 'STEER' AND state = 'pending' AND seq > ?2
             ORDER BY seq"
        ))?;
        let rows = statement
            .query_map(params![run_id, after_seq], read_row)?
            .collect::<Result<Vec<_>, _>>()?;
        let mut used = 0usize;
        let mut out = Vec::new();
        for row in rows {
            if !out.is_empty() && used + row.text.len() > budget {
                break;
            }
            used += row.text.len();
            out.push(row);
        }
        Ok(out)
    }

    /// Marks the run's pending steers up to `up_to_seq` delivered and clears their text
    /// (decision 23: the conversation log holds it now). Returns the seqs that changed.
    pub fn mark_steers_delivered(
        &self,
        run_id: &str,
        up_to_seq: i64,
    ) -> Result<Vec<i64>, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let mut statement = connection.prepare(
            "SELECT seq FROM run_inbox
             WHERE run_id = ?1 AND mode = 'STEER' AND state = 'pending' AND seq <= ?2
             ORDER BY seq",
        )?;
        let seqs = statement
            .query_map(params![run_id, up_to_seq], |r| r.get::<_, i64>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        if !seqs.is_empty() {
            connection.execute(
                "UPDATE run_inbox SET state = 'delivered', text = '', delivered_at = ?3
                 WHERE run_id = ?1 AND mode = 'STEER' AND state = 'pending' AND seq <= ?2",
                params![run_id, up_to_seq, crate::catalog::unix_timestamp()],
            )?;
        }
        Ok(seqs)
    }

    /// The run's messages in `seq` order.
    pub fn list_inbox_messages(&self, run_id: &str) -> Result<Vec<InboxRow>, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let mut statement = connection.prepare(&format!(
            "SELECT {COLUMNS} FROM run_inbox WHERE run_id = ?1 ORDER BY seq"
        ))?;
        let rows = statement.query_map(params![run_id], read_row)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Queued messages waiting on a workspace, oldest first (any thread).
    pub fn list_queued_for_workspace(
        &self,
        workspace_id: &str,
    ) -> Result<Vec<InboxRow>, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let mut statement = connection.prepare(&format!(
            "SELECT {COLUMNS} FROM run_inbox
             WHERE workspace_id = ?1 AND state = 'queued' ORDER BY created_at, run_id, seq"
        ))?;
        let rows = statement.query_map(params![workspace_id], read_row)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new(run: &str, key: &str, mode: &str, thread: &str) -> NewInboxRow {
        NewInboxRow {
            run_id: run.into(),
            mode: mode.into(),
            idempotency_key: key.into(),
            owner_user_id: "u1".into(),
            text: "hello".into(),
            thread_ref: thread.into(),
            workspace_id: "ws1".into(),
            queued_spec_json: (mode == "QUEUE").then(|| "{\"threadRef\":\"t2\"}".into()),
        }
    }

    #[test]
    fn seq_is_per_run_and_states_follow_mode() {
        let store = SqliteWorkflowStore::in_memory().unwrap();
        let a = store
            .insert_inbox_message(&new("r1", "k1", "STEER", "main"))
            .unwrap();
        let b = store
            .insert_inbox_message(&new("r1", "k2", "QUEUE", "t2"))
            .unwrap();
        let c = store
            .insert_inbox_message(&new("r2", "k1", "STEER", "main"))
            .unwrap();
        assert_eq!((a.seq, b.seq, c.seq), (1, 2, 1));
        assert_eq!(a.state, STATE_PENDING);
        assert_eq!(b.state, STATE_QUEUED);
        assert_eq!(b.thread_ref, "t2");
        assert!(b.queued_spec_json.is_some());
    }

    #[test]
    fn same_idempotency_key_returns_the_stored_row() {
        let store = SqliteWorkflowStore::in_memory().unwrap();
        let a = store
            .insert_inbox_message(&new("r1", "k1", "STEER", "main"))
            .unwrap();
        let again = store
            .insert_inbox_message(&new("r1", "k1", "STEER", "main"))
            .unwrap();
        assert_eq!(a, again);
        assert_eq!(store.list_inbox_messages("r1").unwrap().len(), 1);
    }

    #[test]
    fn cap_is_inbox_full() {
        let store = SqliteWorkflowStore::in_memory().unwrap();
        for i in 0..MAX_PENDING_PER_RUN {
            store
                .insert_inbox_message(&new("r1", &format!("k{i}"), "STEER", "main"))
                .unwrap();
        }
        let err = store
            .insert_inbox_message(&new("r1", "over", "STEER", "main"))
            .unwrap_err();
        assert!(matches!(err, OrchestratorError::InboxFull { .. }));
    }

    #[test]
    fn queued_rows_of_another_thread_are_listed_by_workspace() {
        let store = SqliteWorkflowStore::in_memory().unwrap();
        store
            .insert_inbox_message(&new("r1", "k1", "STEER", "main"))
            .unwrap();
        store
            .insert_inbox_message(&new("r1", "k2", "QUEUE", "t2"))
            .unwrap();
        let q = store.list_queued_for_workspace("ws1").unwrap();
        assert_eq!(q.len(), 1);
        assert_eq!(q[0].thread_ref, "t2");
    }

    #[test]
    fn migration_runs_again_on_an_existing_database() {
        let connection = Connection::open_in_memory().unwrap();
        initialize_schema(&connection).unwrap();
        connection
            .execute(
                "INSERT INTO run_inbox (run_id, seq, mode, idempotency_key, owner_user_id,
                    text, state, created_at) VALUES ('r',1,'STEER','k','u','t','pending',1)",
                [],
            )
            .unwrap();
        initialize_schema(&connection).unwrap();
        let n: i64 = connection
            .query_row("SELECT COUNT(*) FROM run_inbox", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
        assert!(connection
            .execute(
                "INSERT INTO run_inbox (run_id, seq, mode, idempotency_key, owner_user_id,
                    text, state, created_at) VALUES ('r',2,'STEER','k2','u','t','bogus',1)",
                [],
            )
            .is_err());
    }

    const RUN: &str = "urn:vam:run:local.vam.dev/acme/run-ended";

    #[test]
    fn an_ended_run_takes_no_message_but_a_repeat_still_answers() {
        let store = SqliteWorkflowStore::in_memory().unwrap();
        store
            .create_run(&vam_spec::v1::ResourceId {
                urn: RUN.to_string(),
            })
            .unwrap();
        let first = store
            .insert_inbox_message_checked(&new(RUN, "k1", "STEER", "main"))
            .unwrap();
        assert!(first.1);
        store.update_run_state(RUN, "SUCCEEDED").unwrap();
        let repeat = store
            .insert_inbox_message_checked(&new(RUN, "k1", "STEER", "main"))
            .unwrap();
        assert!(!repeat.1);
        assert!(matches!(
            store.insert_inbox_message_checked(&new(RUN, "k2", "STEER", "main")),
            Err(OrchestratorError::Conflict(_))
        ));
    }

    #[test]
    fn delivery_marks_and_clears_only_pending_steers_up_to_the_seq() {
        let store = SqliteWorkflowStore::in_memory().unwrap();
        for k in ["a", "b", "c"] {
            store
                .insert_inbox_message(&new("r1", k, "STEER", "main"))
                .unwrap();
        }
        assert_eq!(store.mark_steers_delivered("r1", 2).unwrap(), vec![1, 2]);
        assert!(store.mark_steers_delivered("r1", 2).unwrap().is_empty());
        let rows = store.list_inbox_messages("r1").unwrap();
        assert_eq!(rows[0].state, STATE_DELIVERED);
        assert!(rows[0].text.is_empty() && rows[0].delivered_at.is_some());
        assert_eq!(rows[2].state, STATE_PENDING);
        assert_eq!(rows[2].text, "hello");
    }

    fn live_run(store: &SqliteWorkflowStore, run: &str) {
        store.create_user("u1", "u1").ok();
        store
            .persist_run_spec_with_owner(
                run,
                r#"{"workspaceRefUri":"volume://ws1","threadRef":"main","inputs":{"prompt":"go"}}"#,
                Some("sess"),
                "urn:vam:agent:a",
                Some(&format!("start-{run}")),
                "User::\"u\"",
                "u1",
            )
            .unwrap();
    }

    fn queued_run(id: &str, key: &str) -> NewQueuedRun {
        NewQueuedRun {
            run_id: id.into(),
            spec_json: r#"{"workspaceRefUri":"volume://ws1","threadRef":"main"}"#.into(),
            agent_urn: "urn:vam:agent:a".into(),
            idempotency_key: Some(key.into()),
            owner_principal: "User::\"u\"".into(),
            source: None,
            credential_id: None,
        }
    }

    #[test]
    fn a_queued_message_comes_with_a_pending_run_with_no_session_and_a_repeat_returns_it() {
        let store = SqliteWorkflowStore::in_memory().unwrap();
        live_run(&store, "r1");
        let new = new("r1", "k1", "QUEUE", "main");
        let (row, is_new) = store
            .insert_queued_message(&new, &queued_run("q1", "k1"))
            .unwrap();
        assert!(is_new);
        assert_eq!(row.state, STATE_QUEUED);
        assert_eq!(row.started_run_id.as_deref(), Some("q1"));
        assert_eq!(store.get_run_state("q1").unwrap(), "PENDING");
        assert!(store.is_queued_run("q1"));
        let (again, is_new) = store
            .insert_queued_message(&new, &queued_run("q2", "k1"))
            .unwrap();
        assert!(!is_new);
        assert_eq!(
            again.started_run_id.as_deref(),
            Some("q1"),
            "same key, same run"
        );
        assert!(store.get_run_state("q2").is_err(), "no second run");
    }

    #[test]
    fn a_stop_discards_steers_and_queued_messages_and_cancels_the_queued_run() {
        let store = SqliteWorkflowStore::in_memory().unwrap();
        live_run(&store, "r1");
        store
            .insert_inbox_message(&new("r1", "s1", "STEER", "main"))
            .unwrap();
        store
            .insert_queued_message(&new("r1", "k1", "QUEUE", "t2"), &queued_run("q1", "k1"))
            .unwrap();
        let rows = store.discard_run_inbox("r1").unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].text, "hello", "the text comes back once");
        assert!(store
            .list_inbox_messages("r1")
            .unwrap()
            .iter()
            .all(|r| r.state == STATE_DISCARDED && r.text.is_empty()));
        assert_eq!(store.get_run_state("q1").unwrap(), "CANCELLED");
        assert!(
            store.discard_run_inbox("r1").unwrap().is_empty(),
            "idempotent"
        );
    }

    #[test]
    fn undelivered_steers_of_an_ended_run_merge_into_one_queued_message() {
        let store = SqliteWorkflowStore::in_memory().unwrap();
        live_run(&store, "r1");
        for key in ["a", "b"] {
            store
                .insert_inbox_message(&new("r1", key, "STEER", "main"))
                .unwrap();
        }
        let merged = NewInboxRow {
            text: "hello\n\nhello".into(),
            ..new("r1", "merge", "QUEUE", "main")
        };
        // Not while the run lives.
        assert!(store
            .merge_steers_into_queued("r1", &[1, 2], &merged, &queued_run("q1", "merge"))
            .unwrap()
            .is_none());
        assert!(store.ended_runs_with_pending_steers().unwrap().is_empty());
        store.update_run_state("r1", "FAILED").unwrap();
        assert_eq!(store.ended_runs_with_pending_steers().unwrap(), vec!["r1"]);
        let row = store
            .merge_steers_into_queued("r1", &[1, 2], &merged, &queued_run("q1", "merge"))
            .unwrap()
            .expect("merged");
        assert_eq!(
            (row.mode.as_str(), row.state.as_str()),
            ("QUEUE", STATE_QUEUED)
        );
        assert_eq!(row.started_run_id.as_deref(), Some("q1"));
        let rows = store.list_inbox_messages("r1").unwrap();
        assert_eq!(
            rows.iter().map(|r| r.state.as_str()).collect::<Vec<_>>(),
            vec![STATE_STARTED, STATE_STARTED, STATE_QUEUED]
        );
        assert!(store.ended_runs_with_pending_steers().unwrap().is_empty());
        assert_eq!(store.get_run_state("q1").unwrap(), "PENDING");
    }

    #[test]
    fn only_the_run_being_started_adopts_its_pending_row() {
        let store = std::sync::Arc::new(SqliteWorkflowStore::in_memory().unwrap());
        live_run(&store, "r1");
        store
            .insert_queued_message(&new("r1", "k1", "QUEUE", "main"), &queued_run("q1", "k1"))
            .unwrap();
        let persist = || {
            store
                .persist_run_spec_with_owner(
                    "q1",
                    "{}",
                    Some("sess-new"),
                    "urn:vam:agent:a",
                    Some("queued:k1"),
                    "User::\"u\"",
                    "u1",
                )
                .unwrap()
        };
        persist();
        assert_eq!(
            store.get_run_state("q1").unwrap(),
            "PENDING",
            "a client cannot start it"
        );
        {
            let _starting = store.begin_queue_start("q1");
            assert!(store.queue_start_in_progress("q1"));
            persist();
        }
        assert!(!store.queue_start_in_progress("q1"));
        assert_eq!(store.get_run_state("q1").unwrap(), "RUNNING");
        assert!(!store.is_queued_run("q1"));
    }
}
