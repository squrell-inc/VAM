//! The SQLite store: `schedules`, `fires`, `pending_revokes` (design/schedule-broker.md "The store").
//!
//! Opens and migrates the store, and holds the queries the fire loop needs (due selection, the
//! fire ledger, one transaction that records an outcome and advances the row) and the
//! `pending_revokes` methods. The tools' own queries (list, get by owner) arrive with wave 4.

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use rusqlite::{params, Connection, OptionalExtension, Row};

use crate::error::ScheduleError;
use crate::grant::GrantToken;
use crate::idempotency::{idempotency_key, KeyKind};
use crate::state::{open_private, set_mode};

/// Ordered migrations. `PRAGMA user_version` is the number applied; each one runs in its own
/// transaction together with the version bump, so a reopen applies only the missing ones.
const MIGRATIONS: &[&str] = &[
    // 1: the design's schema, plus `not_before` on pending_revokes (a QUEUED `once` fire's grant
    // is revoked only after its `not_after`; work-outside-chat Q4).
    r#"
CREATE TABLE schedules (
  schedule_id TEXT PRIMARY KEY,
  grant_id TEXT NOT NULL UNIQUE,
  grant_token TEXT NOT NULL,
  grant_expires_at INTEGER NOT NULL,
  target_json TEXT NOT NULL,
  message_preview TEXT NOT NULL,
  owner_user_id TEXT NOT NULL,
  creator_agent_urn TEXT NOT NULL,
  visibility TEXT NOT NULL DEFAULT 'agent' CHECK(visibility IN ('agent','user')),
  title TEXT NOT NULL,
  kind TEXT NOT NULL CHECK(kind IN ('once','cron')),
  expression TEXT NOT NULL,
  timezone TEXT NOT NULL,
  catch_up TEXT NOT NULL DEFAULT 'latest' CHECK(catch_up IN ('latest','skip')),
  enabled INTEGER NOT NULL DEFAULT 1,
  state TEXT NOT NULL DEFAULT 'active'
    CHECK(state IN ('active','completed','revoked','expired','needs_attention','deleting')),
  state_reason TEXT,
  next_fire_at INTEGER,
  backoff_until INTEGER,
  attempts INTEGER NOT NULL DEFAULT 0,
  client_request_id TEXT,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  UNIQUE(owner_user_id, creator_agent_urn, client_request_id)
);
CREATE INDEX schedules_due ON schedules(next_fire_at) WHERE enabled = 1 AND state = 'active';

CREATE TABLE fires (
  schedule_id TEXT NOT NULL,
  fire_at INTEGER NOT NULL,
  kind TEXT NOT NULL CHECK(kind IN ('scheduled','run_now')),
  idempotency_key TEXT NOT NULL,
  outcome TEXT,
  run_id TEXT,
  missed_count INTEGER NOT NULL DEFAULT 0,
  attempts INTEGER NOT NULL DEFAULT 0,
  first_attempt_at INTEGER,
  last_attempt_at INTEGER,
  PRIMARY KEY (schedule_id, fire_at, kind)
);

CREATE TABLE pending_revokes (
  grant_id TEXT PRIMARY KEY,
  grant_token TEXT NOT NULL,
  reason TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  not_before INTEGER NOT NULL DEFAULT 0
);
"#,
    // 2: a change marker bumped by every user write (update, enable or disable, re-bind, delete
    // marking). A fire records its outcome always, but advances the row only if the marker is
    // the one it read, so a user's change is never undone by a fire that was already in flight.
    "ALTER TABLE schedules ADD COLUMN version INTEGER NOT NULL DEFAULT 0;",
];

/// A schedule to insert (wave 4's `schedule_create`, and the tests).
#[derive(Debug, Clone)]
pub struct NewSchedule {
    pub schedule_id: String,
    pub grant_id: String,
    pub token: GrantToken,
    pub grant_expires_at: i64,
    pub target_json: String,
    pub message_preview: String,
    pub owner_user_id: String,
    pub creator_agent_urn: String,
    pub title: String,
    /// `once` or `cron`.
    pub kind: String,
    pub expression: String,
    pub timezone: String,
    /// `latest` or `skip`.
    pub catch_up: String,
    pub next_fire_at: Option<i64>,
    pub now: i64,
}

/// The columns of a schedule the fire loop reads.
#[derive(Debug, Clone)]
pub struct ScheduleRow {
    pub schedule_id: String,
    pub grant_id: String,
    pub token: GrantToken,
    pub kind: String,
    pub expression: String,
    pub timezone: String,
    pub catch_up: String,
    pub enabled: bool,
    pub state: String,
    pub state_reason: Option<String>,
    pub next_fire_at: Option<i64>,
    pub backoff_until: Option<i64>,
    pub attempts: i64,
    /// The change marker: bumped by every user write.
    pub version: i64,
}

/// One row of the fire ledger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FireRow {
    pub schedule_id: String,
    pub fire_at: i64,
    pub kind: String,
    pub idempotency_key: String,
    pub outcome: Option<String>,
    pub run_id: Option<String>,
    pub missed_count: i64,
    pub attempts: i64,
    pub first_attempt_at: Option<i64>,
    pub last_attempt_at: Option<i64>,
}

/// Nominal times a catch-up folded into one `missed` row: keyed at the oldest one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MissedRun {
    pub first_fire_at: i64,
    pub count: i64,
}

/// The fire the loop records as finished.
#[derive(Debug, Clone)]
pub struct SettledFire {
    pub fire_at: i64,
    pub outcome: String,
    pub run_id: Option<String>,
}

/// Everything one transaction writes when a fire ends: the ledger row, the schedule's advance
/// and, when the grant is to be given up, its `pending_revokes` row.
#[derive(Debug, Clone)]
pub struct Settle {
    pub schedule_id: String,
    pub fire: Option<SettledFire>,
    /// Written only when there is no fire (the whole backlog was missed).
    pub missed: Option<MissedRun>,
    pub next_fire_at: Option<i64>,
    pub state: String,
    pub reason: Option<String>,
    pub enabled: bool,
    pub revoke: Option<PendingRevoke>,
    pub now: i64,
    /// The row's `version` when the fire was selected. The row is advanced (and `revoke` queued)
    /// only if it is still this; the fire's own ledger row is written either way.
    pub expected_version: i64,
}

const SCHEDULE_COLS: &str = "schedule_id, grant_id, grant_token, kind, expression, timezone, catch_up, enabled, state, state_reason, next_fire_at, backoff_until, attempts, version";
const FIRE_COLS: &str = "schedule_id, fire_at, kind, idempotency_key, outcome, run_id, missed_count, attempts, first_attempt_at, last_attempt_at";

fn schedule_row(r: &Row<'_>) -> rusqlite::Result<ScheduleRow> {
    Ok(ScheduleRow {
        schedule_id: r.get(0)?,
        grant_id: r.get(1)?,
        token: GrantToken::new(r.get::<_, String>(2)?),
        kind: r.get(3)?,
        expression: r.get(4)?,
        timezone: r.get(5)?,
        catch_up: r.get(6)?,
        enabled: r.get::<_, i64>(7)? != 0,
        state: r.get(8)?,
        state_reason: r.get(9)?,
        next_fire_at: r.get(10)?,
        backoff_until: r.get(11)?,
        attempts: r.get(12)?,
        version: r.get(13)?,
    })
}

fn fire_row(r: &Row<'_>) -> rusqlite::Result<FireRow> {
    Ok(FireRow {
        schedule_id: r.get(0)?,
        fire_at: r.get(1)?,
        kind: r.get(2)?,
        idempotency_key: r.get(3)?,
        outcome: r.get(4)?,
        run_id: r.get(5)?,
        missed_count: r.get(6)?,
        attempts: r.get(7)?,
        first_attempt_at: r.get(8)?,
        last_attempt_at: r.get(9)?,
    })
}

/// The interim outcome of a fire whose last answer was a retryable BUSY.
pub const OUTCOME_BUSY_RETRY: &str = "busy_retry";

/// A grant to give up when the gateway answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingRevoke {
    pub grant_id: String,
    pub token: GrantToken,
    pub reason: String,
    pub created_at: i64,
    /// Not before this time (UTC seconds); 0 = now.
    pub not_before: i64,
}

/// The store. One connection behind a mutex; the broker is one process (see `StateLock`).
#[derive(Debug)]
pub struct Store {
    conn: Mutex<Connection>,
}

impl Store {
    /// Open (creating at 0600) and migrate the database at `path`.
    pub fn open(path: &Path) -> Result<Self, ScheduleError> {
        // Create the file ourselves so it never exists with a looser mode.
        drop(open_private(path)?);
        let conn = Connection::open(path)?;
        set_mode(path, 0o600)?;
        Self::init(conn)
    }

    /// An in-memory store (tests).
    pub fn open_in_memory() -> Result<Self, ScheduleError> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self, ScheduleError> {
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        migrate(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn conn(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The connection, for tests that set up rows directly.
    #[cfg(test)]
    pub(crate) fn conn_for_test(&self) -> MutexGuard<'_, Connection> {
        self.conn()
    }

    /// The number of migrations applied.
    pub fn schema_version(&self) -> Result<u32, ScheduleError> {
        user_version(&self.conn())
    }

    /// Queue a grant to revoke. The same grant twice keeps the first row.
    pub fn queue_revoke(&self, r: &PendingRevoke) -> Result<(), ScheduleError> {
        self.conn().execute(
            "INSERT OR IGNORE INTO pending_revokes (grant_id, grant_token, reason, created_at, not_before)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![r.grant_id, r.token.expose(), r.reason, r.created_at, r.not_before],
        )?;
        Ok(())
    }

    /// The queued revokes whose `not_before` has passed, oldest first.
    pub fn due_revokes(&self, now: i64) -> Result<Vec<PendingRevoke>, ScheduleError> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT grant_id, grant_token, reason, created_at, not_before FROM pending_revokes
             WHERE not_before <= ?1 ORDER BY created_at, grant_id",
        )?;
        let rows = stmt.query_map([now], |row| {
            Ok(PendingRevoke {
                grant_id: row.get(0)?,
                token: GrantToken::new(row.get::<_, String>(1)?),
                reason: row.get(2)?,
                created_at: row.get(3)?,
                not_before: row.get(4)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Drop a queued revoke once the gateway answered. Returns whether a row was removed.
    pub fn finish_revoke(&self, grant_id: &str) -> Result<bool, ScheduleError> {
        let n = self.conn().execute(
            "DELETE FROM pending_revokes WHERE grant_id = ?1",
            [grant_id],
        )?;
        Ok(n > 0)
    }

    /// Insert a schedule.
    pub fn insert_schedule(&self, n: &NewSchedule) -> Result<(), ScheduleError> {
        self.conn().execute(
            "INSERT INTO schedules (schedule_id, grant_id, grant_token, grant_expires_at, target_json,
               message_preview, owner_user_id, creator_agent_urn, title, kind, expression, timezone,
               catch_up, next_fire_at, created_at, updated_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?15)",
            params![
                n.schedule_id,
                n.grant_id,
                n.token.expose(),
                n.grant_expires_at,
                n.target_json,
                n.message_preview,
                n.owner_user_id,
                n.creator_agent_urn,
                n.title,
                n.kind,
                n.expression,
                n.timezone,
                n.catch_up,
                n.next_fire_at,
                n.now
            ],
        )?;
        Ok(())
    }

    /// One schedule by id (no ownership filter: the loop's own view).
    pub fn get_schedule(&self, id: &str) -> Result<Option<ScheduleRow>, ScheduleError> {
        let sql = format!("SELECT {SCHEDULE_COLS} FROM schedules WHERE schedule_id = ?1");
        Ok(self.conn().query_row(&sql, [id], schedule_row).optional()?)
    }

    /// Active, enabled schedules whose time has come and whose backoff has passed, earliest first.
    pub fn due_schedules(&self, now: i64, limit: usize) -> Result<Vec<ScheduleRow>, ScheduleError> {
        let conn = self.conn();
        let sql = format!(
            "SELECT {SCHEDULE_COLS} FROM schedules
             WHERE enabled = 1 AND state = 'active' AND next_fire_at IS NOT NULL
               AND next_fire_at <= ?1 AND (backoff_until IS NULL OR backoff_until <= ?1)
             ORDER BY next_fire_at, schedule_id LIMIT ?2"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![now, limit as i64], schedule_row)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// The earliest moment a schedule not in `exclude` can fire: its time, or its backoff end if
    /// later. `None` when nothing is waiting.
    pub fn next_wake(&self, exclude: &[String]) -> Result<Option<i64>, ScheduleError> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT schedule_id, MAX(next_fire_at, COALESCE(backoff_until, 0)) AS wake FROM schedules
             WHERE enabled = 1 AND state = 'active' AND next_fire_at IS NOT NULL
             ORDER BY wake LIMIT ?1",
        )?;
        let rows = stmt.query_map([exclude.len() as i64 + 1], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })?;
        for r in rows {
            let (id, wake) = r?;
            if !exclude.contains(&id) {
                return Ok(Some(wake));
            }
        }
        Ok(None)
    }

    /// The scheduled fire that was begun and not finished (a retry in progress, or a crash).
    pub fn open_fire(&self, schedule_id: &str) -> Result<Option<FireRow>, ScheduleError> {
        let sql = format!(
            "SELECT {FIRE_COLS} FROM fires WHERE schedule_id = ?1 AND kind = 'scheduled'
               AND (outcome IS NULL OR outcome = '{OUTCOME_BUSY_RETRY}') ORDER BY fire_at LIMIT 1"
        );
        Ok(self
            .conn()
            .query_row(&sql, [schedule_id], fire_row)
            .optional()?)
    }

    /// Every ledger row of a schedule, oldest first.
    pub fn fires_for(&self, schedule_id: &str) -> Result<Vec<FireRow>, ScheduleError> {
        let conn = self.conn();
        let sql =
            format!("SELECT {FIRE_COLS} FROM fires WHERE schedule_id = ?1 ORDER BY fire_at, kind");
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map([schedule_id], fire_row)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Write the intent of one attempt before the gateway is called: the ledger row (first
    /// attempt) or its attempt count (a retry), and the folded `missed` row of a catch-up. The
    /// key is fixed by the first call. Returns the attempt number.
    pub fn begin_fire(
        &self,
        schedule_id: &str,
        fire_at: i64,
        kind: &str,
        key: &str,
        folded: Option<MissedRun>,
        now: i64,
    ) -> Result<i64, ScheduleError> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO fires (schedule_id, fire_at, kind, idempotency_key, attempts, first_attempt_at, last_attempt_at)
             VALUES (?1, ?2, ?3, ?4, 1, ?5, ?5)
             ON CONFLICT(schedule_id, fire_at, kind) DO UPDATE SET attempts = attempts + 1, last_attempt_at = ?5",
            params![schedule_id, fire_at, kind, key, now],
        )?;
        if let Some(m) = folded {
            insert_missed(&tx, schedule_id, m, now)?;
        }
        let attempts = tx.query_row(
            "SELECT attempts FROM fires WHERE schedule_id = ?1 AND fire_at = ?2 AND kind = ?3",
            params![schedule_id, fire_at, kind],
            |r| r.get::<_, i64>(0),
        )?;
        tx.commit()?;
        Ok(attempts)
    }

    /// A retryable answer: do not try again before `backoff_until`. `interim` marks the ledger row
    /// (`busy_retry` for a BUSY, none otherwise).
    #[allow(clippy::too_many_arguments)]
    pub fn schedule_retry(
        &self,
        schedule_id: &str,
        fire_at: i64,
        backoff_until: i64,
        attempts: i64,
        interim: Option<&str>,
        now: i64,
        expected_version: i64,
    ) -> Result<(), ScheduleError> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        // The backoff is written only if the user has not changed the row since the fire began.
        tx.execute(
            "UPDATE schedules SET backoff_until = ?2, attempts = ?3, updated_at = ?4
             WHERE schedule_id = ?1 AND version = ?5",
            params![schedule_id, backoff_until, attempts, now, expected_version],
        )?;
        tx.execute(
            "UPDATE fires SET outcome = ?3 WHERE schedule_id = ?1 AND fire_at = ?2 AND kind = 'scheduled'",
            params![schedule_id, fire_at, interim],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Record how a fire ended and advance the schedule, in one transaction.
    ///
    /// The ledger row is always written. The schedule's advance and the queued revoke apply only
    /// if the row's `version` is still `expected_version`; otherwise the user's change stays and
    /// this returns `false`.
    pub fn settle(&self, s: &Settle) -> Result<bool, ScheduleError> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        if let Some(f) = &s.fire {
            tx.execute(
                "UPDATE fires SET outcome = ?3, run_id = ?4, last_attempt_at = ?5
                 WHERE schedule_id = ?1 AND fire_at = ?2 AND kind = 'scheduled'",
                params![s.schedule_id, f.fire_at, f.outcome, f.run_id, s.now],
            )?;
        }
        if let (None, Some(m)) = (&s.fire, s.missed) {
            insert_missed(&tx, &s.schedule_id, m, s.now)?;
        }
        let applied = tx.execute(
            "UPDATE schedules SET next_fire_at = ?2, backoff_until = NULL, attempts = 0, state = ?3,
               state_reason = ?4, enabled = ?5, updated_at = ?6 WHERE schedule_id = ?1 AND version = ?7",
            params![
                s.schedule_id,
                s.next_fire_at,
                s.state,
                s.reason,
                s.enabled as i64,
                s.now,
                s.expected_version
            ],
        )? > 0;
        if let (true, Some(r)) = (applied, &s.revoke) {
            tx.execute(
                "INSERT OR IGNORE INTO pending_revokes (grant_id, grant_token, reason, created_at, not_before)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    r.grant_id,
                    r.token.expose(),
                    r.reason,
                    r.created_at,
                    r.not_before
                ],
            )?;
        }
        tx.commit()?;
        Ok(applied)
    }

    /// A delete the gateway could not confirm: the row stops firing (`deleting`) and its token is
    /// queued for revoke, in one transaction.
    pub fn mark_deleting(&self, schedule_id: &str, now: i64) -> Result<(), ScheduleError> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT OR IGNORE INTO pending_revokes (grant_id, grant_token, reason, created_at, not_before)
             SELECT grant_id, grant_token, 'delete', ?2, 0 FROM schedules WHERE schedule_id = ?1",
            params![schedule_id, now],
        )?;
        tx.execute(
            "UPDATE schedules SET state = 'deleting', enabled = 0, next_fire_at = NULL, backoff_until = NULL,
               version = version + 1, updated_at = ?2 WHERE schedule_id = ?1",
            params![schedule_id, now],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// The gateway confirmed a delete's revoke: remove the schedule, its ledger and its queued
    /// revoke row. `false` when the row was already gone.
    pub fn delete_schedule(&self, schedule_id: &str) -> Result<bool, ScheduleError> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        tx.execute(
            "DELETE FROM pending_revokes WHERE grant_id = (SELECT grant_id FROM schedules WHERE schedule_id = ?1)",
            [schedule_id],
        )?;
        tx.execute("DELETE FROM fires WHERE schedule_id = ?1", [schedule_id])?;
        let n = tx.execute(
            "DELETE FROM schedules WHERE schedule_id = ?1",
            [schedule_id],
        )?;
        tx.commit()?;
        Ok(n > 0)
    }

    /// Record how a run-now ended on its own ledger row (kind `run_now`). The schedule row is not
    /// touched: a run now never advances or completes a schedule.
    pub fn finish_run_now(
        &self,
        schedule_id: &str,
        fire_at: i64,
        outcome: &str,
        run_id: Option<&str>,
        now: i64,
    ) -> Result<(), ScheduleError> {
        self.conn().execute(
            "UPDATE fires SET outcome = ?3, run_id = ?4, last_attempt_at = ?5
             WHERE schedule_id = ?1 AND fire_at = ?2 AND kind = 'run_now'",
            params![schedule_id, fire_at, outcome, run_id, now],
        )?;
        Ok(())
    }

    /// The gateway answered a revoke: drop the queued row, and the `deleting` schedule that was
    /// waiting on it. Returns whether a queued row was removed.
    pub fn finish_revoke_purge(&self, grant_id: &str) -> Result<bool, ScheduleError> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let n = tx.execute(
            "DELETE FROM pending_revokes WHERE grant_id = ?1",
            [grant_id],
        )?;
        tx.execute(
            "DELETE FROM schedules WHERE grant_id = ?1 AND state = 'deleting'",
            [grant_id],
        )?;
        tx.commit()?;
        Ok(n > 0)
    }
}

/// A schedule as the tools show it (list, get, update). Carries the token for the re-bind path;
/// the tools never put it in an answer.
#[derive(Debug, Clone)]
pub struct Detail {
    pub schedule_id: String,
    pub grant_id: String,
    pub token: GrantToken,
    pub grant_expires_at: i64,
    pub target_json: String,
    pub message_preview: String,
    pub owner_user_id: String,
    pub creator_agent_urn: String,
    pub title: String,
    pub kind: String,
    pub expression: String,
    pub timezone: String,
    pub catch_up: String,
    pub enabled: bool,
    pub state: String,
    pub state_reason: Option<String>,
    pub next_fire_at: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
    pub version: i64,
}

const DETAIL_COLS: &str = "schedule_id, grant_id, grant_token, grant_expires_at, target_json, message_preview, owner_user_id, creator_agent_urn, title, kind, expression, timezone, catch_up, enabled, state, state_reason, next_fire_at, created_at, updated_at, version";

fn detail_row(r: &Row<'_>) -> rusqlite::Result<Detail> {
    Ok(Detail {
        schedule_id: r.get(0)?,
        grant_id: r.get(1)?,
        token: GrantToken::new(r.get::<_, String>(2)?),
        grant_expires_at: r.get(3)?,
        target_json: r.get(4)?,
        message_preview: r.get(5)?,
        owner_user_id: r.get(6)?,
        creator_agent_urn: r.get(7)?,
        title: r.get(8)?,
        kind: r.get(9)?,
        expression: r.get(10)?,
        timezone: r.get(11)?,
        catch_up: r.get(12)?,
        enabled: r.get::<_, i64>(13)? != 0,
        state: r.get(14)?,
        state_reason: r.get(15)?,
        next_fire_at: r.get(16)?,
        created_at: r.get(17)?,
        updated_at: r.get(18)?,
        version: r.get(19)?,
    })
}

/// What an insert did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Inserted {
    /// The row was stored.
    New,
    /// The owner's agent already has a schedule with this `clientRequestId`: its id.
    Existing(String),
}

/// A new grant for a schedule (a re-bind) and the old one to give up.
#[derive(Debug, Clone)]
pub struct Rebind {
    pub grant_id: String,
    pub token: GrantToken,
    pub grant_expires_at: i64,
    pub target_json: String,
    pub message_preview: String,
    pub old: PendingRevoke,
}

/// A user's change to one schedule: every mutable column's final value, written in one
/// transaction that bumps `version`. Fails (returns `false`) if the row is no longer at
/// `expected_version`.
#[derive(Debug, Clone)]
pub struct Edit {
    pub schedule_id: String,
    pub expected_version: i64,
    pub title: String,
    pub kind: String,
    pub expression: String,
    pub timezone: String,
    pub catch_up: String,
    pub enabled: bool,
    pub state: String,
    pub state_reason: Option<String>,
    pub next_fire_at: Option<i64>,
    pub rebind: Option<Rebind>,
    /// The time or the grant changed: a fire begun for the old one is abandoned.
    pub abandon_open_fire: bool,
    pub now: i64,
}

/// Owner first: the owner filter comes before anything else; then the creating agent, or a
/// schedule made visible to the whole user.
const VISIBLE: &str = "owner_user_id = ?1 AND (creator_agent_urn = ?2 OR visibility = 'user')";
/// Writes need the owner and the creating agent; visibility never grants a write.
const CREATOR: &str = "owner_user_id = ?1 AND creator_agent_urn = ?2";

impl Store {
    /// The caller's schedules, soonest first (then newest).
    pub fn list_visible(
        &self,
        owner: &str,
        agent: &str,
        enabled_only: bool,
        limit: usize,
    ) -> Result<Vec<Detail>, ScheduleError> {
        let conn = self.conn();
        let sql = format!(
            "SELECT {DETAIL_COLS} FROM schedules WHERE {VISIBLE} {} \
             ORDER BY next_fire_at IS NULL, next_fire_at, created_at DESC, schedule_id LIMIT ?3",
            if enabled_only { "AND enabled = 1" } else { "" }
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![owner, agent, limit as i64], detail_row)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// One schedule the caller may read.
    pub fn get_visible(
        &self,
        id: &str,
        owner: &str,
        agent: &str,
    ) -> Result<Option<Detail>, ScheduleError> {
        let sql =
            format!("SELECT {DETAIL_COLS} FROM schedules WHERE {VISIBLE} AND schedule_id = ?3");
        Ok(self
            .conn()
            .query_row(&sql, params![owner, agent, id], detail_row)
            .optional()?)
    }

    /// One schedule the caller may change (its owner, through the agent that made it).
    pub fn get_for_write(
        &self,
        id: &str,
        owner: &str,
        agent: &str,
    ) -> Result<Option<Detail>, ScheduleError> {
        let sql =
            format!("SELECT {DETAIL_COLS} FROM schedules WHERE {CREATOR} AND schedule_id = ?3");
        Ok(self
            .conn()
            .query_row(&sql, params![owner, agent, id], detail_row)
            .optional()?)
    }

    /// The schedule a `clientRequestId` already made for this owner and agent.
    pub fn find_by_client_request(
        &self,
        owner: &str,
        agent: &str,
        client_request_id: &str,
    ) -> Result<Option<Detail>, ScheduleError> {
        let sql = format!(
            "SELECT {DETAIL_COLS} FROM schedules WHERE {CREATOR} AND client_request_id = ?3"
        );
        Ok(self
            .conn()
            .query_row(&sql, params![owner, agent, client_request_id], detail_row)
            .optional()?)
    }

    /// Insert a schedule with its `clientRequestId`. Atomic: a second insert with the same
    /// owner, agent and id stores nothing and names the first.
    pub fn insert_schedule_for_request(
        &self,
        n: &NewSchedule,
        client_request_id: Option<&str>,
    ) -> Result<Inserted, ScheduleError> {
        let conn = self.conn();
        if let Some(c) = client_request_id {
            let found: Option<String> = conn
                .query_row(
                    "SELECT schedule_id FROM schedules WHERE owner_user_id = ?1 AND creator_agent_urn = ?2 AND client_request_id = ?3",
                    params![n.owner_user_id, n.creator_agent_urn, c],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(id) = found {
                return Ok(Inserted::Existing(id));
            }
        }
        conn.execute(
            "INSERT INTO schedules (schedule_id, grant_id, grant_token, grant_expires_at, target_json,
               message_preview, owner_user_id, creator_agent_urn, title, kind, expression, timezone,
               catch_up, next_fire_at, client_request_id, created_at, updated_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?16)",
            params![
                n.schedule_id,
                n.grant_id,
                n.token.expose(),
                n.grant_expires_at,
                n.target_json,
                n.message_preview,
                n.owner_user_id,
                n.creator_agent_urn,
                n.title,
                n.kind,
                n.expression,
                n.timezone,
                n.catch_up,
                n.next_fire_at,
                client_request_id,
                n.now
            ],
        )?;
        Ok(Inserted::New)
    }

    /// The newest `limit` ledger rows of a schedule, newest first.
    pub fn recent_fires(
        &self,
        schedule_id: &str,
        limit: usize,
    ) -> Result<Vec<FireRow>, ScheduleError> {
        let conn = self.conn();
        let sql = format!(
            "SELECT {FIRE_COLS} FROM fires WHERE schedule_id = ?1 ORDER BY fire_at DESC, kind LIMIT ?2"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![schedule_id, limit as i64], fire_row)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Apply a user's change in one transaction (`version` bumped; the old token of a re-bind
    /// queued for revoke). `false`: the row changed since it was read, nothing was written.
    pub fn apply_edit(&self, e: &Edit) -> Result<bool, ScheduleError> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let n = tx.execute(
            "UPDATE schedules SET title = ?3, kind = ?4, expression = ?5, timezone = ?6, catch_up = ?7,
               enabled = ?8, state = ?9, state_reason = ?10, next_fire_at = ?11, backoff_until = NULL,
               attempts = 0, version = version + 1, updated_at = ?12
             WHERE schedule_id = ?1 AND version = ?2",
            params![
                e.schedule_id,
                e.expected_version,
                e.title,
                e.kind,
                e.expression,
                e.timezone,
                e.catch_up,
                e.enabled as i64,
                e.state,
                e.state_reason,
                e.next_fire_at,
                e.now
            ],
        )?;
        if n == 0 {
            return Ok(false);
        }
        if let Some(r) = &e.rebind {
            tx.execute(
                "UPDATE schedules SET grant_id = ?2, grant_token = ?3, grant_expires_at = ?4,
                   target_json = ?5, message_preview = ?6 WHERE schedule_id = ?1",
                params![
                    e.schedule_id,
                    r.grant_id,
                    r.token.expose(),
                    r.grant_expires_at,
                    r.target_json,
                    r.message_preview
                ],
            )?;
            tx.execute(
                "INSERT OR IGNORE INTO pending_revokes (grant_id, grant_token, reason, created_at, not_before)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    r.old.grant_id,
                    r.old.token.expose(),
                    r.old.reason,
                    r.old.created_at,
                    r.old.not_before
                ],
            )?;
        }
        if e.abandon_open_fire {
            tx.execute(
                "UPDATE fires SET outcome = 'superseded' WHERE schedule_id = ?1 AND kind = 'scheduled'
                   AND (outcome IS NULL OR outcome = ?2)",
                params![e.schedule_id, OUTCOME_BUSY_RETRY],
            )?;
        }
        tx.commit()?;
        Ok(true)
    }
}

fn insert_missed(
    tx: &rusqlite::Transaction<'_>,
    schedule_id: &str,
    m: MissedRun,
    now: i64,
) -> Result<(), ScheduleError> {
    tx.execute(
        "INSERT OR IGNORE INTO fires (schedule_id, fire_at, kind, idempotency_key, outcome, missed_count,
           attempts, first_attempt_at, last_attempt_at)
         VALUES (?1, ?2, 'scheduled', ?3, 'missed', ?4, 0, ?5, ?5)",
        params![
            schedule_id,
            m.first_fire_at,
            idempotency_key(schedule_id, m.first_fire_at, KeyKind::Scheduled),
            m.count,
            now
        ],
    )?;
    Ok(())
}

fn user_version(conn: &Connection) -> Result<u32, ScheduleError> {
    Ok(conn.query_row("PRAGMA user_version", [], |r| r.get::<_, u32>(0))?)
}

fn migrate(conn: &Connection) -> Result<(), ScheduleError> {
    let supported = MIGRATIONS.len() as u32;
    let found = user_version(conn)?;
    if found > supported {
        return Err(ScheduleError::StoreTooNew { found, supported });
    }
    for (i, sql) in MIGRATIONS.iter().enumerate().skip(found as usize) {
        let version = i as u32 + 1;
        let fail = |e: rusqlite::Error| ScheduleError::Migration {
            version,
            message: e.to_string(),
        };
        let script = format!("BEGIN;\n{sql}\nPRAGMA user_version = {version};\nCOMMIT;");
        if let Err(e) = conn.execute_batch(&script) {
            let _ = conn.execute_batch("ROLLBACK;");
            return Err(fail(e));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tables(s: &Store) -> Vec<String> {
        let conn = s.conn();
        let mut stmt = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .unwrap();
        let rows = stmt.query_map([], |r| r.get::<_, String>(0)).unwrap();
        rows.map(|r| r.unwrap()).collect()
    }

    #[test]
    fn fresh_store_has_the_three_tables_at_the_latest_version() {
        let s = Store::open_in_memory().unwrap();
        assert_eq!(s.schema_version().unwrap(), MIGRATIONS.len() as u32);
        assert_eq!(tables(&s), vec!["fires", "pending_revokes", "schedules"]);
    }

    #[test]
    fn migrations_are_idempotent_across_reopen() {
        let t = tempfile::tempdir().unwrap();
        let path = t.path().join("db");
        {
            let s = Store::open(&path).unwrap();
            s.queue_revoke(&revoke("rg_1", 0)).unwrap();
        }
        let s = Store::open(&path).unwrap();
        assert_eq!(s.schema_version().unwrap(), MIGRATIONS.len() as u32);
        assert_eq!(s.due_revokes(10).unwrap().len(), 1);
    }

    #[test]
    fn a_newer_store_is_refused() {
        let t = tempfile::tempdir().unwrap();
        let path = t.path().join("db");
        {
            let c = Connection::open(&path).unwrap();
            c.execute_batch("PRAGMA user_version = 99;").unwrap();
        }
        let err = Store::open(&path).unwrap_err();
        assert!(
            matches!(err, ScheduleError::StoreTooNew { found: 99, .. }),
            "{err}"
        );
    }

    #[test]
    fn schema_checks_hold() {
        let s = Store::open_in_memory().unwrap();
        let conn = s.conn();
        let bad = conn.execute(
            "INSERT INTO schedules (schedule_id, grant_id, grant_token, grant_expires_at, target_json,
               message_preview, owner_user_id, creator_agent_urn, title, kind, expression, timezone,
               created_at, updated_at)
             VALUES ('s','g','t',0,'{}','','u','a','t','weekly','x','UTC',0,0)",
            [],
        );
        assert!(bad.is_err(), "kind CHECK refuses an unknown kind");
    }

    #[cfg(unix)]
    #[test]
    fn database_file_is_0600() {
        use std::os::unix::fs::PermissionsExt;
        let t = tempfile::tempdir().unwrap();
        let path = t.path().join("db");
        let _s = Store::open(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    fn revoke(id: &str, not_before: i64) -> PendingRevoke {
        PendingRevoke {
            grant_id: id.into(),
            token: GrantToken::new(format!("vam_rg_{id}.secret")),
            reason: "delete".into(),
            created_at: 5,
            not_before,
        }
    }

    #[test]
    fn pending_revokes_honour_not_before_and_are_idempotent() {
        let s = Store::open_in_memory().unwrap();
        s.queue_revoke(&revoke("rg_a", 0)).unwrap();
        s.queue_revoke(&revoke("rg_a", 0)).unwrap();
        s.queue_revoke(&revoke("rg_b", 100)).unwrap();
        let due = s.due_revokes(50).unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].token.expose(), "vam_rg_rg_a.secret");
        assert_eq!(s.due_revokes(100).unwrap().len(), 2);
        assert!(s.finish_revoke("rg_a").unwrap());
        assert!(!s.finish_revoke("rg_a").unwrap());
        assert_eq!(s.due_revokes(100).unwrap().len(), 1);
    }
}
