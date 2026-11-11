//! The delivery log: the broker's only table (design 9.2: delivery facts live in the broker).
//!
//! It holds ids and counters, nothing else. **No token, no action handle, no link code, no agent
//! text, no address** has a column (open question 1: the owner reads this schema). Every value
//! is checked before it is written: an id has a short, plain character set and may not look like
//! a Slack token or a VAM handle. The file is created 0600 inside the 0700 state directory.

use std::path::Path;
use std::sync::Mutex;

use rusqlite::{params, Connection, OptionalExtension};

use crate::error::NotifyError;

/// The columns, in order. A test reads `sqlite_master` against this list.
pub const COLUMNS: [&str; 8] = [
    "delivery_id",
    "notification_id",
    "channel",
    "slack_channel_id",
    "message_ts",
    "sent_at",
    "attempts",
    "last_vendor_status",
];

/// The schema, verbatim (also printed in the wave's PR).
pub const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS deliveries (
    delivery_id        TEXT PRIMARY KEY NOT NULL CHECK (length(delivery_id) BETWEEN 1 AND 128),
    notification_id    TEXT NOT NULL DEFAULT '' CHECK (length(notification_id) <= 128),
    channel            TEXT NOT NULL CHECK (length(channel) BETWEEN 1 AND 16),
    slack_channel_id   TEXT NOT NULL DEFAULT '' CHECK (length(slack_channel_id) <= 32),
    message_ts         TEXT NOT NULL DEFAULT '' CHECK (length(message_ts) <= 32),
    sent_at            INTEGER NOT NULL DEFAULT 0,
    attempts           INTEGER NOT NULL DEFAULT 0,
    last_vendor_status TEXT NOT NULL DEFAULT '' CHECK (length(last_vendor_status) <= 40)
)";

/// One delivery as the log knows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryRow {
    pub delivery_id: String,
    pub notification_id: String,
    pub channel: String,
    pub slack_channel_id: String,
    pub message_ts: String,
    pub sent_at: i64,
    pub attempts: i64,
    pub last_vendor_status: String,
}

impl DeliveryRow {
    /// The message went out (it has a vendor id).
    pub fn is_sent(&self) -> bool {
        !self.message_ts.is_empty()
    }
}

/// An id or status as the log accepts it: plain characters, short, and not shaped like a secret.
pub fn plain(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ':'))
        && !["xox", "xapp", "vam_na_"].iter().any(|p| s.starts_with(p))
}

fn check(field: &str, value: &str, allow_empty: bool) -> Result<(), NotifyError> {
    if (allow_empty && value.is_empty()) || plain(value) {
        Ok(())
    } else {
        Err(NotifyError::Store(format!(
            "{field} is not a plain id (refused: it could be a secret)"
        )))
    }
}

pub struct DeliveryLog {
    conn: Mutex<Connection>,
}

impl DeliveryLog {
    pub fn open(path: &Path) -> Result<Self, NotifyError> {
        create_private(path)?;
        let conn = Connection::open(path).map_err(store)?;
        conn.execute_batch(SCHEMA).map_err(store)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn conn(&self) -> Result<std::sync::MutexGuard<'_, Connection>, NotifyError> {
        self.conn
            .lock()
            .map_err(|_| NotifyError::Store("delivery log lock poisoned".into()))
    }

    pub fn get(&self, delivery_id: &str) -> Result<Option<DeliveryRow>, NotifyError> {
        let conn = self.conn()?;
        get_row(&conn, delivery_id)
    }

    /// Counts one attempt, creating the row on the first. Returns the row after the count.
    pub fn begin_attempt(
        &self,
        delivery_id: &str,
        notification_id: &str,
        channel: &str,
    ) -> Result<DeliveryRow, NotifyError> {
        check("delivery_id", delivery_id, false)?;
        check("notification_id", notification_id, true)?;
        check("channel", channel, false)?;
        let conn = self.conn()?;
        conn.execute(
            "INSERT OR IGNORE INTO deliveries (delivery_id, notification_id, channel) VALUES (?1, ?2, ?3)",
            params![delivery_id, notification_id, channel],
        )
        .map_err(store)?;
        conn.execute(
            "UPDATE deliveries SET attempts = attempts + 1 WHERE delivery_id = ?1",
            params![delivery_id],
        )
        .map_err(store)?;
        get_row(&conn, delivery_id)?.ok_or_else(|| NotifyError::Store("row vanished".into()))
    }

    /// The message went out: its Slack channel and ts.
    pub fn record_sent(
        &self,
        delivery_id: &str,
        slack_channel_id: &str,
        message_ts: &str,
        sent_at: i64,
    ) -> Result<(), NotifyError> {
        check("slack_channel_id", slack_channel_id, false)?;
        check("message_ts", message_ts, false)?;
        self.conn()?
            .execute(
                "UPDATE deliveries SET slack_channel_id = ?2, message_ts = ?3, sent_at = ?4,
                        last_vendor_status = 'ok' WHERE delivery_id = ?1",
                params![delivery_id, slack_channel_id, message_ts, sent_at],
            )
            .map_err(store)?;
        Ok(())
    }

    /// What the vendor last said. A status that is not plain is stored as `other`.
    pub fn record_status(&self, delivery_id: &str, status: &str) -> Result<(), NotifyError> {
        let status = if plain(status) && status.len() <= 40 {
            status
        } else {
            "other"
        };
        self.conn()?
            .execute(
                "UPDATE deliveries SET last_vendor_status = ?2 WHERE delivery_id = ?1",
                params![delivery_id, status],
            )
            .map_err(store)?;
        Ok(())
    }

    /// The column names of the table, for the schema test.
    pub fn columns(&self) -> Result<Vec<(String, String)>, NotifyError> {
        let conn = self.conn()?;
        let mut stmt = conn
            .prepare("SELECT name, type FROM pragma_table_info('deliveries')")
            .map_err(store)?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .map_err(store)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(store)
    }
}

fn get_row(conn: &Connection, delivery_id: &str) -> Result<Option<DeliveryRow>, NotifyError> {
    conn.query_row(
        "SELECT delivery_id, notification_id, channel, slack_channel_id, message_ts, sent_at,
                attempts, last_vendor_status FROM deliveries WHERE delivery_id = ?1",
        params![delivery_id],
        |r| {
            Ok(DeliveryRow {
                delivery_id: r.get(0)?,
                notification_id: r.get(1)?,
                channel: r.get(2)?,
                slack_channel_id: r.get(3)?,
                message_ts: r.get(4)?,
                sent_at: r.get(5)?,
                attempts: r.get(6)?,
                last_vendor_status: r.get(7)?,
            })
        },
    )
    .optional()
    .map_err(store)
}

fn store(e: rusqlite::Error) -> NotifyError {
    NotifyError::Store(e.to_string())
}

/// Creates the file 0600 before SQLite sees it, so it is never wider.
fn create_private(path: &Path) -> Result<(), NotifyError> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path).map_err(|e| NotifyError::io(path, e))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| NotifyError::io(path, e))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open() -> (tempfile::TempDir, DeliveryLog) {
        let tmp = tempfile::tempdir().unwrap();
        let log = DeliveryLog::open(&tmp.path().join("d.db")).unwrap();
        (tmp, log)
    }

    #[test]
    fn schema_has_only_the_listed_columns_and_no_blob() {
        let (_t, log) = open();
        let cols = log.columns().unwrap();
        let names: Vec<&str> = cols.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, COLUMNS);
        for (name, ty) in &cols {
            assert!(ty == "TEXT" || ty == "INTEGER", "{name} is {ty}");
            for banned in [
                "token", "secret", "handle", "code", "text", "body", "address",
            ] {
                assert!(!name.contains(banned), "column {name} could hold a secret");
            }
        }
    }

    #[test]
    fn sqlite_master_holds_only_the_one_table() {
        let (_t, log) = open();
        let conn = log.conn().unwrap();
        let mut stmt = conn
            .prepare("SELECT name FROM sqlite_master WHERE type IN ('table','view','trigger')")
            .unwrap();
        let names: Vec<String> = stmt
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(names
            .iter()
            .all(|n| n == "deliveries" || n.starts_with("sqlite_")));
    }

    #[test]
    fn attempts_count_and_a_sent_row_keeps_its_ts() {
        let (_t, log) = open();
        let r = log.begin_attempt("nd_1", "nt_1", "slack").unwrap();
        assert_eq!((r.attempts, r.is_sent()), (1, false));
        log.record_status("nd_1", "ratelimited").unwrap();
        assert_eq!(
            log.begin_attempt("nd_1", "nt_1", "slack").unwrap().attempts,
            2
        );
        log.record_sent("nd_1", "D1", "1700.000001", 5).unwrap();
        let r = log.get("nd_1").unwrap().unwrap();
        assert!(r.is_sent());
        assert_eq!(
            (r.slack_channel_id.as_str(), r.last_vendor_status.as_str()),
            ("D1", "ok")
        );
        assert!(log.get("nd_2").unwrap().is_none());
    }

    #[test]
    fn a_secret_shaped_value_is_refused_not_stored() {
        let (_t, log) = open();
        for bad in [
            "xoxb-1234-abcd",
            "xapp-1-A0-9",
            "vam_na_abc.def",
            "has space",
            "",
        ] {
            assert!(log.begin_attempt(bad, "n", "slack").is_err(), "{bad}");
        }
        assert!(log.begin_attempt("nd_1", "xoxb-1-2", "slack").is_err());
        log.begin_attempt("nd_1", "nt_1", "slack").unwrap();
        assert!(log.record_sent("nd_1", "D1", "xoxb-secret", 1).is_err());
        log.record_status("nd_1", "a free text with spaces and a secret xoxb-1")
            .unwrap();
        assert_eq!(
            log.get("nd_1").unwrap().unwrap().last_vendor_status,
            "other"
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_file_is_0600() {
        use std::os::unix::fs::PermissionsExt;
        let (t, _log) = open();
        let mode = std::fs::metadata(t.path().join("d.db"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }
}
