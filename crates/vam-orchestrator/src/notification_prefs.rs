//! Per-user notification preferences (`design/notifications.md` section 3.5): per kind on or off,
//! quiet hours, and kinds that are always sent. No digests (slice 3).
//!
//! Defaults when a user sets nothing: the node's kind defaults (`node_notify`), else the table in
//! [`default_on`]. Quiet hours hold every external delivery, approvals too (locked decision 11),
//! except kinds in `always`. This module decides; it delivers nothing (waves 3 and 4 read it when
//! they create delivery rows).

use std::collections::BTreeMap;

use chrono::Timelike;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::error::OrchestratorError;
use crate::node_notify::NodeNotifySettings;
use crate::notifications::{Kind, Severity};
use crate::workflow::SqliteWorkflowStore;

/// A daily window in a time zone, whole minutes. `start` after `end` crosses midnight.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuietHours {
    pub start: String,
    pub end: String,
    pub tz: String,
}

/// What a user may set. `PUT` replaces the whole document.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Prefs {
    /// Kind to `immediate` or `off`. A kind not listed follows the node, then the default.
    #[serde(default)]
    pub kinds: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quiet_hours: Option<QuietHours>,
    /// Kinds sent even inside quiet hours.
    #[serde(default)]
    pub always: Vec<String>,
}

fn invalid(msg: impl Into<String>) -> OrchestratorError {
    OrchestratorError::InvalidContract(msg.into())
}

/// `HH:MM`, two digits each, whole minutes, as minutes since midnight.
fn parse_hhmm(field: &str, s: &str) -> Result<u32, OrchestratorError> {
    let b = s.as_bytes();
    let ok = b.len() == 5 && b[2] == b':' && [0, 1, 3, 4].iter().all(|i| b[*i].is_ascii_digit());
    let bad = || invalid(format!("quietHours.{field} must be HH:MM (00:00 to 23:59)"));
    if !ok {
        return Err(bad());
    }
    let h: u32 = s[0..2].parse().map_err(|_| bad())?;
    let m: u32 = s[3..5].parse().map_err(|_| bad())?;
    if h > 23 || m > 59 {
        return Err(bad());
    }
    Ok(h * 60 + m)
}

/// The built-in default for a kind with no preference and no node default (design section 3.5):
/// decision kinds, `trigger.skipped`, `grant.expiring`, `budget.hit`, `report.arrived` and the
/// delegation warnings go out; `run.finished` only as a warning; `trigger.fired` never.
pub fn default_on(kind: Kind, severity: Severity) -> bool {
    match kind {
        Kind::RunFinished => severity == Severity::Warn,
        Kind::TriggerFired => false,
        _ => true,
    }
}

impl Prefs {
    /// Check and normalise. Unknown kinds, a bad time or zone, an empty window and unknown modes
    /// are refused.
    pub fn validate(mut self) -> Result<Self, OrchestratorError> {
        let mut kinds = BTreeMap::new();
        for (kind, mode) in std::mem::take(&mut self.kinds) {
            if Kind::parse(&kind).is_none() {
                return Err(invalid(format!(
                    "kinds: `{kind}` is not a notification kind"
                )));
            }
            let mode = match mode.as_str() {
                "immediate" | "on" => "immediate",
                "off" => "off",
                other => {
                    return Err(invalid(format!(
                        "kinds.{kind}: `{other}` is not on (immediate) or off"
                    )))
                }
            };
            kinds.insert(kind, mode.to_string());
        }
        self.kinds = kinds;
        for k in &self.always {
            if Kind::parse(k).is_none() {
                return Err(invalid(format!("always: `{k}` is not a notification kind")));
            }
        }
        self.always.sort();
        self.always.dedup();
        if let Some(q) = &self.quiet_hours {
            let start = parse_hhmm("start", &q.start)?;
            let end = parse_hhmm("end", &q.end)?;
            if start == end {
                return Err(invalid("quietHours: start and end are the same minute"));
            }
            if q.tz.parse::<chrono_tz::Tz>().is_err() {
                return Err(invalid(format!(
                    "quietHours.tz: `{}` is not an IANA time zone (for example Europe/Berlin or UTC)",
                    q.tz
                )));
            }
        }
        Ok(self)
    }

    /// Whether `kind` (at `severity`) is sent to channels: the user's setting, else the node's
    /// default, else the built-in one.
    pub fn wants_external(
        &self,
        node: &NodeNotifySettings,
        kind: Kind,
        severity: Severity,
    ) -> bool {
        match self.kinds.get(kind.as_str()).map(String::as_str) {
            Some("immediate") => true,
            Some("off") => false,
            _ => node
                .kind_default(kind)
                .unwrap_or_else(|| default_on(kind, severity)),
        }
    }

    /// `immediate`, `off`, or `warn` (immediate only when the record is a warning), for display.
    pub fn effective_mode(&self, node: &NodeNotifySettings, kind: Kind) -> &'static str {
        let set = self.kinds.contains_key(kind.as_str()) || node.kind_default(kind).is_some();
        if !set && kind == Kind::RunFinished {
            return "warn";
        }
        if self.wants_external(node, kind, Severity::Warn) {
            "immediate"
        } else {
            "off"
        }
    }

    /// Minutes since local midnight in the quiet-hours zone, and the window, at `now`.
    fn local_minute(q: &QuietHours, now: i64) -> Option<(u32, u32, u32)> {
        let tz: chrono_tz::Tz = q.tz.parse().ok()?;
        let at = chrono::DateTime::from_timestamp(now, 0)?.with_timezone(&tz);
        let start = parse_hhmm("start", &q.start).ok()?;
        let end = parse_hhmm("end", &q.end).ok()?;
        Some((at.hour() * 60 + at.minute(), start, end))
    }

    /// Whether `now` (unix seconds) is inside the quiet window: `[start, end)`.
    pub fn in_quiet_hours(&self, now: i64) -> bool {
        self.quiet_hours
            .as_ref()
            .and_then(|q| Self::local_minute(q, now))
            .is_some_and(|(m, start, end)| {
                if start < end {
                    m >= start && m < end
                } else {
                    m >= start || m < end
                }
            })
    }

    /// When a delivery held at `now` may go out: the end of the window, to the minute (a daylight
    /// saving change inside the window moves the true end by up to an hour). `None` outside it.
    pub fn quiet_hours_end(&self, now: i64) -> Option<i64> {
        if !self.in_quiet_hours(now) {
            return None;
        }
        let q = self.quiet_hours.as_ref()?;
        let (m, _, end) = Self::local_minute(q, now)?;
        let remaining = (end + 1440 - m) % 1440;
        Some(now - now.rem_euclid(60) + i64::from(remaining) * 60)
    }

    /// Whether an external delivery of `kind` waits for the end of quiet hours.
    pub fn held_by_quiet_hours(&self, kind: Kind, now: i64) -> bool {
        self.in_quiet_hours(now) && !self.always.iter().any(|k| k == kind.as_str())
    }
}

pub(crate) fn initialize_schema(connection: &Connection) -> Result<(), OrchestratorError> {
    connection.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS notification_prefs (
            user_id TEXT PRIMARY KEY NOT NULL,
            prefs_json TEXT NOT NULL,
            updated_at INTEGER NOT NULL
        );
        ",
    )?;
    Ok(())
}

impl SqliteWorkflowStore {
    /// The user's preferences, or the empty set (all defaults) when none were saved.
    pub fn notification_prefs(&self, user_id: &str) -> Result<Prefs, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let json: Option<String> = connection
            .query_row(
                "SELECT prefs_json FROM notification_prefs WHERE user_id = ?1",
                params![user_id],
                |r| r.get(0),
            )
            .optional()?;
        match json {
            None => Ok(Prefs::default()),
            Some(j) => serde_json::from_str(&j).map_err(|e| OrchestratorError::Core(e.to_string())),
        }
    }

    /// Replace the user's preferences with validated ones.
    pub fn put_notification_prefs(
        &self,
        user_id: &str,
        prefs: &Prefs,
    ) -> Result<(), OrchestratorError> {
        let json =
            serde_json::to_string(prefs).map_err(|e| OrchestratorError::Core(e.to_string()))?;
        let at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        connection.execute(
            "INSERT INTO notification_prefs (user_id, prefs_json, updated_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(user_id) DO UPDATE SET prefs_json = excluded.prefs_json,
                                                updated_at = excluded.updated_at",
            params![user_id, json, at],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prefs(json: &str) -> Result<Prefs, OrchestratorError> {
        serde_json::from_str::<Prefs>(json).unwrap().validate()
    }

    fn quiet(start: &str, end: &str, tz: &str) -> Prefs {
        Prefs {
            quiet_hours: Some(QuietHours {
                start: start.into(),
                end: end.into(),
                tz: tz.into(),
            }),
            ..Prefs::default()
        }
    }

    /// 2026-11-04 00:00:00 UTC.
    const DAY: i64 = 1_793_750_400;

    #[test]
    fn validation_refuses_what_it_cannot_hold() {
        for bad in [
            r#"{"quietHours":{"start":"22:00","end":"22:00","tz":"UTC"}}"#,
            r#"{"quietHours":{"start":"22:00","end":"07:00","tz":"Mars/Base"}}"#,
            r#"{"quietHours":{"start":"22:00:30","end":"07:00","tz":"UTC"}}"#,
            r#"{"quietHours":{"start":"22:0","end":"07:00","tz":"UTC"}}"#,
            r#"{"quietHours":{"start":"24:00","end":"07:00","tz":"UTC"}}"#,
            r#"{"quietHours":{"start":"22:60","end":"07:00","tz":"UTC"}}"#,
            r#"{"kinds":{"nope":"off"}}"#,
            r#"{"kinds":{"run.finished":"digest"}}"#,
            r#"{"always":["nope"]}"#,
        ] {
            assert!(prefs(bad).is_err(), "{bad}");
        }
        assert!(serde_json::from_str::<Prefs>(r#"{"digestAt":"08:00"}"#).is_err());
        let ok = prefs(
            r#"{"kinds":{"run.finished":"on"},"always":["approval.pending","approval.pending"],
                "quietHours":{"start":"22:00","end":"07:00","tz":"Europe/Berlin"}}"#,
        )
        .unwrap();
        assert_eq!(ok.kinds["run.finished"], "immediate");
        assert_eq!(ok.always, vec!["approval.pending"]);
    }

    #[test]
    fn defaults_follow_the_design_then_the_node_then_the_user() {
        let node = NodeNotifySettings::default();
        let p = Prefs::default();
        for k in [
            Kind::ApprovalPending,
            Kind::QuestionAsked,
            Kind::GateOpened,
            Kind::TriggerSkipped,
            Kind::GrantExpiring,
            Kind::BudgetHit,
            Kind::ReportArrived,
        ] {
            assert!(p.wants_external(&node, k, k.severity()), "{k:?}");
        }
        assert!(p.wants_external(&node, Kind::RunFinished, Severity::Warn));
        assert!(!p.wants_external(&node, Kind::RunFinished, Severity::Info));
        assert!(!p.wants_external(&node, Kind::TriggerFired, Severity::Info));
        assert_eq!(p.effective_mode(&node, Kind::RunFinished), "warn");

        let mut node = NodeNotifySettings::default();
        node.kind_defaults
            .insert("run.finished".into(), "immediate".into());
        node.kind_defaults.insert("budget.hit".into(), "off".into());
        assert!(p.wants_external(&node, Kind::RunFinished, Severity::Info));
        assert!(!p.wants_external(&node, Kind::BudgetHit, Severity::Warn));
        assert_eq!(p.effective_mode(&node, Kind::RunFinished), "immediate");

        let user = prefs(r#"{"kinds":{"budget.hit":"on","run.finished":"off"}}"#).unwrap();
        assert!(user.wants_external(&node, Kind::BudgetHit, Severity::Warn));
        assert!(!user.wants_external(&node, Kind::RunFinished, Severity::Warn));
    }

    #[test]
    fn quiet_window_across_midnight() {
        let p = quiet("22:00", "07:00", "UTC");
        assert!(p.in_quiet_hours(DAY + 23 * 3600));
        assert!(p.in_quiet_hours(DAY + 3 * 3600));
        assert!(p.in_quiet_hours(DAY + 6 * 3600 + 59 * 60));
        assert!(!p.in_quiet_hours(DAY + 7 * 3600));
        assert!(!p.in_quiet_hours(DAY + 12 * 3600));
        assert!(p.in_quiet_hours(DAY + 22 * 3600));
        assert!(!p.in_quiet_hours(DAY + 21 * 3600 + 59 * 60));
        // Held until 07:00: from 23:30 the same night, and from 03:00.
        assert_eq!(
            p.quiet_hours_end(DAY + 23 * 3600 + 30 * 60),
            Some(DAY + 31 * 3600)
        );
        assert_eq!(p.quiet_hours_end(DAY + 3 * 3600), Some(DAY + 7 * 3600));
        assert_eq!(p.quiet_hours_end(DAY + 12 * 3600), None);
    }

    #[test]
    fn quiet_window_within_a_day_and_in_a_zone() {
        let p = quiet("12:00", "13:00", "UTC");
        assert!(p.in_quiet_hours(DAY + 12 * 3600 + 30 * 60));
        assert!(!p.in_quiet_hours(DAY + 13 * 3600));
        // Berlin is UTC+1 in November: 22:00-07:00 local is 21:00-06:00 UTC.
        let b = quiet("22:00", "07:00", "Europe/Berlin");
        assert!(b.in_quiet_hours(DAY + 21 * 3600));
        assert!(!b.in_quiet_hours(DAY + 20 * 3600 + 59 * 60));
        assert!(b.in_quiet_hours(DAY + 5 * 3600 + 59 * 60));
        assert!(!b.in_quiet_hours(DAY + 6 * 3600));
        // One whole minute, as the e2e runner sets it.
        let m = quiet("10:15", "10:16", "UTC");
        assert!(m.in_quiet_hours(DAY + 10 * 3600 + 15 * 60 + 59));
        assert!(!m.in_quiet_hours(DAY + 10 * 3600 + 16 * 60));
        // No quiet hours set: never quiet.
        assert!(!Prefs::default().in_quiet_hours(DAY));
    }

    #[test]
    fn quiet_hours_hold_approvals_unless_always() {
        let mut p = quiet("22:00", "07:00", "UTC");
        let night = DAY + 23 * 3600;
        assert!(p.held_by_quiet_hours(Kind::ApprovalPending, night));
        assert!(p.held_by_quiet_hours(Kind::BudgetHit, night));
        p.always = vec!["approval.pending".into()];
        assert!(!p.held_by_quiet_hours(Kind::ApprovalPending, night));
        assert!(p.held_by_quiet_hours(Kind::BudgetHit, night));
        assert!(!p.held_by_quiet_hours(Kind::BudgetHit, DAY + 12 * 3600));
    }

    #[test]
    fn store_keeps_one_document_per_user() {
        let store = SqliteWorkflowStore::in_memory().unwrap();
        assert_eq!(store.notification_prefs("alice").unwrap(), Prefs::default());
        let p = prefs(
            r#"{"always":["gate.opened"],"quietHours":{"start":"22:00","end":"07:00","tz":"UTC"}}"#,
        )
        .unwrap();
        store.put_notification_prefs("alice", &p).unwrap();
        assert_eq!(store.notification_prefs("alice").unwrap(), p);
        assert_eq!(store.notification_prefs("bob").unwrap(), Prefs::default());
        store
            .put_notification_prefs("alice", &Prefs::default())
            .unwrap();
        assert_eq!(store.notification_prefs("alice").unwrap(), Prefs::default());
    }
}
