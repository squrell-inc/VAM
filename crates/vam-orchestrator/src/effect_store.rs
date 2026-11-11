//! Storage for the effect journal (E10 Phase 4, AR-4.3): one row per outside call a run decided to
//! make, written before the call is sent. Contract and storage only; the host tool path that
//! writes and reads these rows comes with AR-4.4.
//!
//! A row is keyed by (run id, call id). Its `state` moves
//! `intended -> sent -> confirmed | failed | unknown`; `intended` may also go straight to `failed`
//! (denied) and `unknown` may go back to `sent` (a re-send after a check found nothing, or after
//! the person allowed it) or settle to `confirmed` or `failed`. `confirmed` and `failed` are final.
//!
//! **Every write returns a `Result` and none is swallowed.** A caller that cannot write the
//! `intended` or `sent` row must not send the call (fail closed, owner decision R1). Arguments are
//! kept as a digest only; result bytes (already redacted) are kept only until the thread log
//! records the tool message carrying them, then cleared with [`SqliteWorkflowStore::effect_clear_result`].

use rusqlite::{params, Connection, OptionalExtension};

use crate::effect_class::EffectClass;
use crate::error::OrchestratorError;
use crate::workflow::SqliteWorkflowStore;

/// Where a journaled call stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectState {
    /// Decided and recorded; not sent.
    Intended,
    /// About to leave the host (written immediately before the send).
    Sent,
    /// The answer is known and recorded.
    Confirmed,
    /// The call failed (denied, refused or errored) and is final.
    Failed,
    /// Sent, and the outcome is not known (timeout, transport loss, crash).
    Unknown,
}

impl EffectState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Intended => "intended",
            Self::Sent => "sent",
            Self::Confirmed => "confirmed",
            Self::Failed => "failed",
            Self::Unknown => "unknown",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "intended" => Some(Self::Intended),
            "sent" => Some(Self::Sent),
            "confirmed" => Some(Self::Confirmed),
            "failed" => Some(Self::Failed),
            "unknown" => Some(Self::Unknown),
            _ => None,
        }
    }

    /// States this one may be reached from.
    fn allowed_from(self) -> &'static [&'static str] {
        match self {
            Self::Intended => &[],
            Self::Sent => &["intended", "unknown"],
            Self::Confirmed => &["intended", "sent", "unknown"],
            Self::Failed => &["intended", "sent", "unknown"],
            Self::Unknown => &["sent"],
        }
    }
}

/// Creates the table and indexes. Safe to run on every start.
pub(crate) fn initialize_schema(connection: &Connection) -> Result<(), OrchestratorError> {
    connection.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS effect_journal (
            run_id TEXT NOT NULL,
            call_id TEXT NOT NULL,
            parent_call_id TEXT,
            tool TEXT NOT NULL,
            adapter TEXT NOT NULL,
            class TEXT NOT NULL CHECK (class IN ('read','idempotent','non_idempotent')),
            state TEXT NOT NULL CHECK (state IN
                ('intended','sent','confirmed','failed','unknown')),
            idempotency_key TEXT NOT NULL,
            args_digest TEXT NOT NULL,
            approval_id TEXT,
            attempts INTEGER NOT NULL DEFAULT 0,
            generation_sent INTEGER,
            result_digest TEXT,
            result_bytes BLOB,
            detail TEXT,
            reconciled_by TEXT,
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL,
            PRIMARY KEY (run_id, call_id)
        );
        CREATE INDEX IF NOT EXISTS idx_effect_journal_key
            ON effect_journal(idempotency_key);
        CREATE INDEX IF NOT EXISTS idx_effect_journal_run_state
            ON effect_journal(run_id, state);
        ",
    )?;
    Ok(())
}

/// A call to journal as `intended`.
#[derive(Debug, Clone)]
pub struct NewEffect {
    pub run_id: String,
    pub call_id: String,
    /// The calling playbook call for a nested step (`<call id>/<step id>` rows name their parent).
    pub parent_call_id: Option<String>,
    pub tool: String,
    /// `host`, `hsp`, `mcp` or `internal`.
    pub adapter: String,
    pub class: EffectClass,
    pub idempotency_key: String,
    pub args_digest: String,
    pub approval_id: Option<String>,
}

/// A stored journal row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectRow {
    pub run_id: String,
    pub call_id: String,
    pub parent_call_id: Option<String>,
    pub tool: String,
    pub adapter: String,
    pub class: EffectClass,
    pub state: EffectState,
    pub idempotency_key: String,
    pub args_digest: String,
    pub approval_id: Option<String>,
    pub attempts: u32,
    pub generation_sent: Option<u64>,
    pub result_digest: Option<String>,
    pub result_bytes: Option<Vec<u8>>,
    /// Typed error or reason for `failed` (for example `denied`).
    pub detail: Option<String>,
    /// How an `unknown` row was settled: `verify`, `model` or `person`.
    pub reconciled_by: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// Rows per state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EffectCounts {
    pub intended: u32,
    pub sent: u32,
    pub confirmed: u32,
    pub failed: u32,
    pub unknown: u32,
}

impl EffectCounts {
    pub fn total(&self) -> u32 {
        self.intended + self.sent + self.confirmed + self.failed + self.unknown
    }
}

/// What a run's journal holds, oldest first. E12 Phase 3 reads this for the receipt.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EffectsForRun {
    pub rows: Vec<EffectRow>,
    pub counts: EffectCounts,
}

const COLUMNS: &str = "run_id, call_id, parent_call_id, tool, adapter, class, state, \
     idempotency_key, args_digest, approval_id, attempts, generation_sent, result_digest, \
     result_bytes, detail, reconciled_by, created_at, updated_at";

fn corrupt(index: usize, what: String) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        index,
        rusqlite::types::Type::Text,
        Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, what)),
    )
}

fn read_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<EffectRow> {
    let class_raw: String = row.get(5)?;
    let state_raw: String = row.get(6)?;
    let attempts: i64 = row.get(10)?;
    let generation: Option<i64> = row.get(11)?;
    Ok(EffectRow {
        run_id: row.get(0)?,
        call_id: row.get(1)?,
        parent_call_id: row.get(2)?,
        tool: row.get(3)?,
        adapter: row.get(4)?,
        class: EffectClass::parse(&class_raw)
            .ok_or_else(|| corrupt(5, format!("unknown effect class '{class_raw}'")))?,
        state: EffectState::parse(&state_raw)
            .ok_or_else(|| corrupt(6, format!("unknown effect state '{state_raw}'")))?,
        idempotency_key: row.get(7)?,
        args_digest: row.get(8)?,
        approval_id: row.get(9)?,
        attempts: u32::try_from(attempts).map_err(|e| corrupt(10, e.to_string()))?,
        generation_sent: generation
            .map(|g| u64::try_from(g).map_err(|e| corrupt(11, e.to_string())))
            .transpose()?,
        result_digest: row.get(12)?,
        result_bytes: row.get(13)?,
        detail: row.get(14)?,
        reconciled_by: row.get(15)?,
        created_at: row.get(16)?,
        updated_at: row.get(17)?,
    })
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

fn get_locked(
    connection: &Connection,
    run_id: &str,
    call_id: &str,
) -> Result<Option<EffectRow>, OrchestratorError> {
    Ok(connection
        .query_row(
            &format!("SELECT {COLUMNS} FROM effect_journal WHERE run_id = ?1 AND call_id = ?2"),
            params![run_id, call_id],
            read_row,
        )
        .optional()?)
}

/// Moves a row to `to`, only from a state the transition allows, then returns it. `set` is the
/// extra assignments (after `state`, `updated_at`), `values` their parameters.
fn transition(
    connection: &Connection,
    run_id: &str,
    call_id: &str,
    to: EffectState,
    set: &str,
    values: &[&dyn rusqlite::ToSql],
) -> Result<EffectRow, OrchestratorError> {
    let from = to
        .allowed_from()
        .iter()
        .map(|s| format!("'{s}'"))
        .collect::<Vec<_>>()
        .join(",");
    let now = now_ms();
    let state = to.as_str();
    let mut all: Vec<&dyn rusqlite::ToSql> = vec![&state, &now, &run_id, &call_id];
    all.extend_from_slice(values);
    let extra = if set.is_empty() {
        String::new()
    } else {
        format!(", {set}")
    };
    let changed = connection.execute(
        &format!(
            "UPDATE effect_journal SET state = ?1, updated_at = ?2{extra}
             WHERE run_id = ?3 AND call_id = ?4 AND state IN ({from})"
        ),
        all.as_slice(),
    )?;
    let row = get_locked(connection, run_id, call_id)?.ok_or_else(|| {
        OrchestratorError::NotFound(format!("effect '{call_id}' of run '{run_id}'"))
    })?;
    if changed == 0 {
        return Err(OrchestratorError::InvalidState {
            expected: "a state that may move to the requested one",
            actual: format!("{} (wanted {})", row.state.as_str(), to.as_str()),
        });
    }
    Ok(row)
}

impl SqliteWorkflowStore {
    fn effect_lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>, OrchestratorError> {
        self.connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)
    }

    /// Writes the `intended` row. Fails with `Conflict` when (run, call) already has a row, and
    /// with a database error when the write fails: the caller must not send the call.
    pub fn effect_begin(&self, new: &NewEffect) -> Result<EffectRow, OrchestratorError> {
        let connection = self.effect_lock()?;
        let now = now_ms();
        let inserted = connection.execute(
            "INSERT INTO effect_journal (run_id, call_id, parent_call_id, tool, adapter, class,
                state, idempotency_key, args_digest, approval_id, attempts, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'intended', ?7, ?8, ?9, 0, ?10, ?10)
             ON CONFLICT(run_id, call_id) DO NOTHING",
            params![
                new.run_id,
                new.call_id,
                new.parent_call_id,
                new.tool,
                new.adapter,
                new.class.as_str(),
                new.idempotency_key,
                new.args_digest,
                new.approval_id,
                now,
            ],
        )?;
        if inserted == 0 {
            return Err(OrchestratorError::Conflict(format!(
                "effect '{}' of run '{}' is already journaled",
                new.call_id, new.run_id
            )));
        }
        get_locked(&connection, &new.run_id, &new.call_id)?.ok_or_else(|| {
            OrchestratorError::CorruptState("journal row missing after insert".into())
        })
    }

    /// Records the approval a suspended `intended` row waits on.
    pub fn effect_set_approval(
        &self,
        run_id: &str,
        call_id: &str,
        approval_id: &str,
    ) -> Result<EffectRow, OrchestratorError> {
        let connection = self.effect_lock()?;
        let changed = connection.execute(
            "UPDATE effect_journal SET approval_id = ?1, updated_at = ?2
             WHERE run_id = ?3 AND call_id = ?4 AND state = 'intended'",
            params![approval_id, now_ms(), run_id, call_id],
        )?;
        let row = get_locked(&connection, run_id, call_id)?.ok_or_else(|| {
            OrchestratorError::NotFound(format!("effect '{call_id}' of run '{run_id}'"))
        })?;
        if changed == 0 {
            return Err(OrchestratorError::InvalidState {
                expected: "intended",
                actual: row.state.as_str().to_string(),
            });
        }
        Ok(row)
    }

    /// Written immediately before the call leaves the host: counts the attempt and records the
    /// lifecycle generation it left from. From `intended`, or from `unknown` for a re-send.
    pub fn effect_mark_sent(
        &self,
        run_id: &str,
        call_id: &str,
        generation: u64,
    ) -> Result<EffectRow, OrchestratorError> {
        let generation = i64::try_from(generation)
            .map_err(|_| OrchestratorError::InvalidContract("generation out of range".into()))?;
        let connection = self.effect_lock()?;
        transition(
            &connection,
            run_id,
            call_id,
            EffectState::Sent,
            "attempts = attempts + 1, generation_sent = ?5",
            &[&generation],
        )
    }

    /// The answer is known. `result_bytes` is the already redacted response, kept until the log
    /// records it. `reconciled_by` is set when an `unknown` row was settled by a check.
    pub fn effect_mark_confirmed(
        &self,
        run_id: &str,
        call_id: &str,
        result_digest: &str,
        result_bytes: Option<&[u8]>,
        reconciled_by: Option<&str>,
    ) -> Result<EffectRow, OrchestratorError> {
        let connection = self.effect_lock()?;
        transition(
            &connection,
            run_id,
            call_id,
            EffectState::Confirmed,
            "result_digest = ?5, result_bytes = ?6, reconciled_by = ?7",
            &[&result_digest, &result_bytes, &reconciled_by],
        )
    }

    /// The call failed for good. `detail` is the typed error or reason (for example `denied`).
    pub fn effect_mark_failed(
        &self,
        run_id: &str,
        call_id: &str,
        detail: &str,
        reconciled_by: Option<&str>,
    ) -> Result<EffectRow, OrchestratorError> {
        let connection = self.effect_lock()?;
        transition(
            &connection,
            run_id,
            call_id,
            EffectState::Failed,
            "detail = ?5, reconciled_by = ?6",
            &[&detail, &reconciled_by],
        )
    }

    /// Sent, and no answer is known (timeout, transport loss, crash).
    pub fn effect_mark_unknown(
        &self,
        run_id: &str,
        call_id: &str,
    ) -> Result<EffectRow, OrchestratorError> {
        let connection = self.effect_lock()?;
        transition(&connection, run_id, call_id, EffectState::Unknown, "", &[])
    }

    /// Puts an `unknown` row of a read or idempotent call back to `intended` so it is sent again
    /// under the same key (E10 Phase 4, AR-4.7). Never used for a non-idempotent call.
    pub fn effect_resend(
        &self,
        run_id: &str,
        call_id: &str,
    ) -> Result<EffectRow, OrchestratorError> {
        let connection = self.effect_lock()?;
        let changed = connection.execute(
            "UPDATE effect_journal SET state = 'intended', updated_at = ?1
             WHERE run_id = ?2 AND call_id = ?3 AND state = 'unknown'
               AND class IN ('read','idempotent')",
            params![now_ms(), run_id, call_id],
        )?;
        let row = get_locked(&connection, run_id, call_id)?.ok_or_else(|| {
            OrchestratorError::NotFound(format!("effect '{call_id}' of run '{run_id}'"))
        })?;
        if changed == 0 {
            return Err(OrchestratorError::InvalidState {
                expected: "unknown (read or idempotent)",
                actual: row.state.as_str().to_string(),
            });
        }
        Ok(row)
    }

    /// Records that a verify read ran for an `unknown` row and found no sign of the effect. The
    /// row stays `unknown` (a read can miss a write that has not shown up yet).
    pub fn effect_note_checked(
        &self,
        run_id: &str,
        call_id: &str,
        by: &str,
    ) -> Result<(), OrchestratorError> {
        let connection = self.effect_lock()?;
        connection.execute(
            "UPDATE effect_journal SET reconciled_by = ?1, updated_at = ?2
             WHERE run_id = ?3 AND call_id = ?4 AND state = 'unknown'",
            params![by, now_ms(), run_id, call_id],
        )?;
        Ok(())
    }

    /// A row of the run, other than `except_call_id`, for the same tool and arguments whose
    /// effect may already have happened: still `unknown` (checked or not), or confirmed only by a
    /// verify read. An identical later call asks the person first.
    pub fn effect_find_maybe_done(
        &self,
        run_id: &str,
        tool: &str,
        args_digest: &str,
        except_call_id: &str,
    ) -> Result<Option<EffectRow>, OrchestratorError> {
        let connection = self.effect_lock()?;
        Ok(connection
            .query_row(
                &format!(
                    "SELECT {COLUMNS} FROM effect_journal
                     WHERE run_id = ?1 AND tool = ?2 AND args_digest = ?3 AND call_id <> ?4
                       AND (state = 'unknown'
                            OR (state = 'confirmed' AND reconciled_by = 'verify'))
                     ORDER BY created_at, rowid LIMIT 1"
                ),
                params![run_id, tool, args_digest, except_call_id],
                read_row,
            )
            .optional()?)
    }

    /// Drops the stored result bytes once the thread log records the tool message (the digest
    /// stays). Only a `confirmed` row holds bytes; clearing twice is fine.
    pub fn effect_clear_result(
        &self,
        run_id: &str,
        call_id: &str,
    ) -> Result<EffectRow, OrchestratorError> {
        let connection = self.effect_lock()?;
        connection.execute(
            "UPDATE effect_journal SET result_bytes = NULL, updated_at = ?1
             WHERE run_id = ?2 AND call_id = ?3",
            params![now_ms(), run_id, call_id],
        )?;
        get_locked(&connection, run_id, call_id)?.ok_or_else(|| {
            OrchestratorError::NotFound(format!("effect '{call_id}' of run '{run_id}'"))
        })
    }

    pub fn effect_get(
        &self,
        run_id: &str,
        call_id: &str,
    ) -> Result<Option<EffectRow>, OrchestratorError> {
        let connection = self.effect_lock()?;
        get_locked(&connection, run_id, call_id)
    }

    /// The row for an idempotency key, if any (a key is unique per (run, call, arguments)).
    pub fn effect_get_by_key(&self, key: &str) -> Result<Option<EffectRow>, OrchestratorError> {
        let connection = self.effect_lock()?;
        Ok(connection
            .query_row(
                &format!(
                    "SELECT {COLUMNS} FROM effect_journal WHERE idempotency_key = ?1
                     ORDER BY created_at, rowid LIMIT 1"
                ),
                params![key],
                read_row,
            )
            .optional()?)
    }

    /// Re-binds a still `intended` row to amended arguments (an approval that amended them keeps
    /// the call id; the digest and the key follow the arguments that will be sent).
    pub fn effect_rebind_args(
        &self,
        run_id: &str,
        call_id: &str,
        args_digest: &str,
        idempotency_key: &str,
    ) -> Result<EffectRow, OrchestratorError> {
        let connection = self.effect_lock()?;
        let changed = connection.execute(
            "UPDATE effect_journal SET args_digest = ?1, idempotency_key = ?2, updated_at = ?3
             WHERE run_id = ?4 AND call_id = ?5 AND state = 'intended'",
            params![args_digest, idempotency_key, now_ms(), run_id, call_id],
        )?;
        let row = get_locked(&connection, run_id, call_id)?.ok_or_else(|| {
            OrchestratorError::NotFound(format!("effect '{call_id}' of run '{run_id}'"))
        })?;
        if changed == 0 {
            return Err(OrchestratorError::InvalidState {
                expected: "intended",
                actual: row.state.as_str().to_string(),
            });
        }
        Ok(row)
    }

    /// The row an approval request waits on, if any.
    pub fn effect_get_by_approval(
        &self,
        approval_id: &str,
    ) -> Result<Option<EffectRow>, OrchestratorError> {
        let connection = self.effect_lock()?;
        Ok(connection
            .query_row(
                &format!(
                    "SELECT {COLUMNS} FROM effect_journal WHERE approval_id = ?1
                     ORDER BY created_at, rowid LIMIT 1"
                ),
                params![approval_id],
                read_row,
            )
            .optional()?)
    }

    /// The oldest `intended` row of a run for a tool, optionally with these arguments (an old
    /// guest that sends no call id on the call an approval allowed). With `args_digest` `None`
    /// only a row that waits on an approval is returned.
    pub fn effect_find_intended(
        &self,
        run_id: &str,
        tool: &str,
        args_digest: Option<&str>,
    ) -> Result<Option<EffectRow>, OrchestratorError> {
        let connection = self.effect_lock()?;
        let (sql, digest) = match args_digest {
            Some(d) => (
                format!(
                    "SELECT {COLUMNS} FROM effect_journal WHERE run_id = ?1 AND tool = ?2
                     AND state = 'intended' AND args_digest = ?3 AND approval_id IS NOT NULL
                     ORDER BY created_at, rowid LIMIT 1"
                ),
                d.to_string(),
            ),
            None => (
                format!(
                    "SELECT {COLUMNS} FROM effect_journal WHERE run_id = ?1 AND tool = ?2
                     AND state = 'intended' AND approval_id IS NOT NULL AND ?3 = ?3
                     ORDER BY created_at, rowid LIMIT 1"
                ),
                String::new(),
            ),
        };
        Ok(connection
            .query_row(&sql, params![run_id, tool, digest], read_row)
            .optional()?)
    }

    /// How many rows of a run have a call id starting with `prefix` (host-derived ids).
    pub fn effect_count_with_prefix(
        &self,
        run_id: &str,
        prefix: &str,
    ) -> Result<u32, OrchestratorError> {
        let connection = self.effect_lock()?;
        let n: i64 = connection.query_row(
            "SELECT COUNT(*) FROM effect_journal WHERE run_id = ?1 AND substr(call_id, 1, ?2) = ?3",
            params![run_id, prefix.len() as i64, prefix],
            |row| row.get(0),
        )?;
        Ok(u32::try_from(n).unwrap_or(u32::MAX))
    }

    /// Every journaled call of a run, oldest first, with the count per state.
    pub fn effects_for_run(&self, run_id: &str) -> Result<EffectsForRun, OrchestratorError> {
        let connection = self.effect_lock()?;
        let mut statement = connection.prepare(&format!(
            "SELECT {COLUMNS} FROM effect_journal WHERE run_id = ?1 ORDER BY created_at, rowid"
        ))?;
        let rows = statement
            .query_map(params![run_id], read_row)?
            .collect::<Result<Vec<_>, _>>()?;
        let mut counts = EffectCounts::default();
        for row in &rows {
            match row.state {
                EffectState::Intended => counts.intended += 1,
                EffectState::Sent => counts.sent += 1,
                EffectState::Confirmed => counts.confirmed += 1,
                EffectState::Failed => counts.failed += 1,
                EffectState::Unknown => counts.unknown += 1,
            }
        }
        Ok(EffectsForRun { rows, counts })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (SqliteWorkflowStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteWorkflowStore::open(dir.path().join("vam.db")).unwrap();
        (store, dir)
    }

    fn new_effect(call: &str) -> NewEffect {
        NewEffect {
            run_id: "run_1".into(),
            call_id: call.into(),
            parent_call_id: None,
            tool: "echo_create_ticket".into(),
            adapter: "hsp".into(),
            class: EffectClass::NonIdempotent,
            idempotency_key: format!("vamfx_{call}"),
            args_digest: "d".repeat(64),
            approval_id: None,
        }
    }

    #[test]
    fn begin_writes_intended_and_a_repeat_conflicts() {
        let (s, _d) = store();
        let row = s.effect_begin(&new_effect("c1")).unwrap();
        assert_eq!(row.state, EffectState::Intended);
        assert_eq!(row.attempts, 0);
        assert!(row.created_at > 0);
        assert!(matches!(
            s.effect_begin(&new_effect("c1")),
            Err(OrchestratorError::Conflict(_))
        ));
        assert_eq!(s.effects_for_run("run_1").unwrap().rows.len(), 1);
    }

    #[test]
    fn the_happy_path_moves_through_every_state() {
        let (s, _d) = store();
        s.effect_begin(&new_effect("c1")).unwrap();
        let sent = s.effect_mark_sent("run_1", "c1", 2).unwrap();
        assert_eq!(
            (sent.state, sent.attempts, sent.generation_sent),
            (EffectState::Sent, 1, Some(2))
        );
        let done = s
            .effect_mark_confirmed("run_1", "c1", "sha256:r", Some(b"T-1"), None)
            .unwrap();
        assert_eq!(done.state, EffectState::Confirmed);
        assert_eq!(done.result_bytes.as_deref(), Some(&b"T-1"[..]));
        assert_eq!(done.result_digest.as_deref(), Some("sha256:r"));
        let cleared = s.effect_clear_result("run_1", "c1").unwrap();
        assert_eq!(cleared.result_bytes, None);
        assert_eq!(cleared.result_digest.as_deref(), Some("sha256:r"));
    }

    #[test]
    fn transitions_are_checked() {
        let (s, _d) = store();
        s.effect_begin(&new_effect("c1")).unwrap();
        // intended cannot become unknown (it never left).
        assert!(matches!(
            s.effect_mark_unknown("run_1", "c1"),
            Err(OrchestratorError::InvalidState { .. })
        ));
        s.effect_mark_sent("run_1", "c1", 1).unwrap();
        s.effect_mark_confirmed("run_1", "c1", "d", None, None)
            .unwrap();
        // Final states stay final.
        for result in [
            s.effect_mark_sent("run_1", "c1", 2),
            s.effect_mark_failed("run_1", "c1", "x", None),
            s.effect_mark_unknown("run_1", "c1"),
            s.effect_mark_confirmed("run_1", "c1", "d", None, None),
        ] {
            assert!(matches!(
                result,
                Err(OrchestratorError::InvalidState { .. })
            ));
        }
        assert!(matches!(
            s.effect_mark_sent("run_1", "missing", 1),
            Err(OrchestratorError::NotFound(_))
        ));
    }

    #[test]
    fn a_denial_fails_an_intended_row_with_its_reason() {
        let (s, _d) = store();
        s.effect_begin(&new_effect("c1")).unwrap();
        s.effect_set_approval("run_1", "c1", "apr_1").unwrap();
        let row = s.effect_mark_failed("run_1", "c1", "denied", None).unwrap();
        assert_eq!(row.state, EffectState::Failed);
        assert_eq!(row.detail.as_deref(), Some("denied"));
        assert_eq!(row.approval_id.as_deref(), Some("apr_1"));
        assert!(matches!(
            s.effect_set_approval("run_1", "c1", "apr_2"),
            Err(OrchestratorError::InvalidState { .. })
        ));
    }

    #[test]
    fn unknown_settles_or_is_resent_and_counts_attempts() {
        let (s, _d) = store();
        s.effect_begin(&new_effect("c1")).unwrap();
        s.effect_mark_sent("run_1", "c1", 1).unwrap();
        assert_eq!(
            s.effect_mark_unknown("run_1", "c1").unwrap().state,
            EffectState::Unknown
        );
        let resent = s.effect_mark_sent("run_1", "c1", 2).unwrap();
        assert_eq!((resent.attempts, resent.generation_sent), (2, Some(2)));
        s.effect_mark_unknown("run_1", "c1").unwrap();
        let settled = s
            .effect_mark_confirmed("run_1", "c1", "d", None, Some("verify"))
            .unwrap();
        assert_eq!(settled.reconciled_by.as_deref(), Some("verify"));

        s.effect_begin(&new_effect("c2")).unwrap();
        s.effect_mark_sent("run_1", "c2", 1).unwrap();
        s.effect_mark_unknown("run_1", "c2").unwrap();
        let failed = s
            .effect_mark_failed("run_1", "c2", "not_found", Some("verify"))
            .unwrap();
        assert_eq!(failed.state, EffectState::Failed);
    }

    #[test]
    fn effects_for_run_lists_in_order_with_counts() {
        let (s, _d) = store();
        for c in ["c1", "c2", "c3"] {
            s.effect_begin(&new_effect(c)).unwrap();
        }
        let mut other = new_effect("c1");
        other.run_id = "run_2".into();
        s.effect_begin(&other).unwrap();
        s.effect_mark_sent("run_1", "c1", 1).unwrap();
        s.effect_mark_confirmed("run_1", "c1", "d", None, None)
            .unwrap();
        s.effect_mark_failed("run_1", "c2", "denied", None).unwrap();
        let all = s.effects_for_run("run_1").unwrap();
        let calls: Vec<_> = all.rows.iter().map(|r| r.call_id.as_str()).collect();
        assert_eq!(calls, ["c1", "c2", "c3"]);
        assert_eq!(
            all.counts,
            EffectCounts {
                intended: 1,
                sent: 0,
                confirmed: 1,
                failed: 1,
                unknown: 0
            }
        );
        assert_eq!(all.counts.total(), 3);
        assert_eq!(s.effects_for_run("none").unwrap(), EffectsForRun::default());
    }

    #[test]
    fn lookups_by_call_and_by_key() {
        let (s, _d) = store();
        let mut nested = new_effect("c1/step-a");
        nested.parent_call_id = Some("c1".into());
        s.effect_begin(&nested).unwrap();
        assert!(s.effect_get("run_1", "nope").unwrap().is_none());
        let row = s.effect_get("run_1", "c1/step-a").unwrap().unwrap();
        assert_eq!(row.parent_call_id.as_deref(), Some("c1"));
        let by_key = s.effect_get_by_key("vamfx_c1/step-a").unwrap().unwrap();
        assert_eq!(by_key.call_id, "c1/step-a");
        assert!(s.effect_get_by_key("vamfx_none").unwrap().is_none());
    }

    #[test]
    fn a_failed_write_is_an_error_not_a_silent_skip() {
        let (s, _d) = store();
        s.connection
            .lock()
            .unwrap()
            .execute_batch("DROP TABLE effect_journal;")
            .unwrap();
        assert!(s.effect_begin(&new_effect("c1")).is_err());
        assert!(s.effect_mark_sent("run_1", "c1", 1).is_err());
    }

    #[test]
    fn migration_creates_the_table_on_an_existing_database_and_keeps_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vam.db");
        {
            let s = SqliteWorkflowStore::open(&path).unwrap();
            // A database from before the journal: the table does not exist.
            s.connection
                .lock()
                .unwrap()
                .execute_batch("DROP TABLE effect_journal;")
                .unwrap();
        }
        let s = SqliteWorkflowStore::open(&path).unwrap();
        s.effect_begin(&new_effect("c1")).unwrap();
        drop(s);
        // A second open is a no-op and keeps the row.
        let s = SqliteWorkflowStore::open(&path).unwrap();
        assert_eq!(s.effects_for_run("run_1").unwrap().counts.intended, 1);
    }

    #[test]
    fn state_strings_round_trip() {
        for st in [
            EffectState::Intended,
            EffectState::Sent,
            EffectState::Confirmed,
            EffectState::Failed,
            EffectState::Unknown,
        ] {
            assert_eq!(EffectState::parse(st.as_str()), Some(st));
        }
        assert_eq!(EffectState::parse("x"), None);
    }
}
