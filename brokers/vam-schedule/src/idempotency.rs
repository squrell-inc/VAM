//! The idempotency key of one fire.

use chrono::{DateTime, SecondsFormat, TimeZone, Utc};
use sha2::{Digest, Sha256};

/// Which kind of fire a key is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyKind {
    Scheduled,
    RunNow,
}

impl KeyKind {
    pub fn as_str(self) -> &'static str {
        match self {
            KeyKind::Scheduled => "scheduled",
            KeyKind::RunNow => "run_now",
        }
    }
}

/// `hex(sha256("vam.schedule\n" + schedule_id + "\n" + fire_at RFC 3339 UTC + "\n" + kind))`.
/// Every retry of one fire reuses it. (Run-now uses [`run_now_key`].)
pub fn idempotency_key(schedule_id: &str, fire_at_unix: i64, kind: KeyKind) -> String {
    let at: DateTime<Utc> = Utc
        .timestamp_opt(fire_at_unix, 0)
        .single()
        .unwrap_or(DateTime::<Utc>::UNIX_EPOCH);
    let text = format!(
        "vam.schedule\n{schedule_id}\n{}\n{}",
        at.to_rfc3339_opts(SecondsFormat::Secs, true),
        kind.as_str()
    );
    let digest = Sha256::digest(text.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// The key of a run-now: the same text plus the calling run's id, so a retried call dedupes and
/// two presses (two runs) do not.
pub fn run_now_key(schedule_id: &str, fire_at_unix: i64, calling_run_id: &str) -> String {
    let at: DateTime<Utc> = Utc
        .timestamp_opt(fire_at_unix, 0)
        .single()
        .unwrap_or(DateTime::<Utc>::UNIX_EPOCH);
    let text = format!(
        "vam.schedule\n{schedule_id}\n{}\n{}\n{calling_run_id}",
        at.to_rfc3339_opts(SecondsFormat::Secs, true),
        KeyKind::RunNow.as_str()
    );
    let digest = Sha256::digest(text.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_and_distinct() {
        let a = idempotency_key("sch_A", 1_000, KeyKind::Scheduled);
        assert_eq!(a, idempotency_key("sch_A", 1_000, KeyKind::Scheduled));
        assert_eq!(a.len(), 64);
        assert_ne!(a, idempotency_key("sch_A", 1_000, KeyKind::RunNow));
        assert_ne!(a, idempotency_key("sch_A", 1_001, KeyKind::Scheduled));
        assert_ne!(a, idempotency_key("sch_B", 1_000, KeyKind::Scheduled));
    }

    #[test]
    fn matches_the_documented_derivation() {
        let d = Sha256::digest(b"vam.schedule\nsch_A\n1970-01-01T00:16:40Z\nscheduled");
        let want: String = d.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(idempotency_key("sch_A", 1_000, KeyKind::Scheduled), want);
    }
}
