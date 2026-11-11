//! `when` parsing and next-fire computation: one-off times (a wall time in an IANA zone, an
//! instant with an offset, or a relative `in` duration) and five-field cron in a zone. Every
//! result is a UTC instant. Ported from the prototype's `next_fire.ts`.

use std::str::FromStr;

use chrono::{DateTime, Duration, FixedOffset, LocalResult, NaiveDateTime, TimeZone, Utc};
use chrono_tz::Tz;
use croner::parser::CronParser;
use croner::Cron;
use serde_json::Value;

/// Cron floor scan: every fire within this many days of the first fire.
pub const FLOOR_SCAN_DAYS: i64 = 31;
/// Cron floor scan: at most this many fires.
pub const FLOOR_SCAN_CAP: usize = 5000;
/// Grace window of a one-off schedule.
pub const ONCE_GRACE: Duration = Duration::hours(24);
/// Upper bound of a cron schedule's grace window.
pub const CRON_GRACE_CAP: Duration = Duration::hours(1);

/// Why a `when` was refused. Every variant is INVALID_ARGUMENT for the tools.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WhenError {
    #[error("when must be an object with exactly one of once or cron")]
    BadShape,
    #[error("when.timezone must be a valid IANA timezone")]
    BadTimezone,
    #[error("when.once.at must be an ISO-8601 datetime")]
    BadDatetime,
    #[error("when.once.in must be an ISO-8601 duration such as PT5S, PT30M, P1DT2H or P1W")]
    BadDuration,
    #[error("when.once.in must be greater than zero")]
    NotPositive,
    #[error("when.cron.expression must be a valid five-field cron")]
    InvalidCron,
    #[error("when.cron.expression does not have a next fire time")]
    NoNextFire,
    #[error("next fire time must be in the future")]
    InThePast,
    #[error("cron fires {got_secs}s apart; the minimum interval is {min_secs}s")]
    BelowFloor { min_secs: i64, got_secs: i64 },
}

impl WhenError {
    /// The tool error code every variant maps to.
    pub fn code(&self) -> &'static str {
        "INVALID_ARGUMENT"
    }
}

/// A one-off fire time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OnceWhen {
    /// A wall time in a zone.
    At { local: NaiveDateTime, tz: Tz },
    /// An instant with an offset.
    Instant(DateTime<FixedOffset>),
    /// A duration counted from when the schedule is saved.
    In(Duration),
}

/// A parsed `when`.
#[derive(Debug, Clone)]
pub enum When {
    Once(OnceWhen),
    Cron {
        expression: String,
        tz: Tz,
        cron: Box<Cron>,
    },
}

fn parse_tz(v: Option<&Value>) -> Result<Tz, WhenError> {
    let s = v.and_then(Value::as_str).map(str::trim).unwrap_or("");
    if s.is_empty() {
        return Err(WhenError::BadTimezone);
    }
    Tz::from_str(s).map_err(|_| WhenError::BadTimezone)
}

fn only_keys(o: &serde_json::Map<String, Value>, keys: &[&str]) -> Result<(), WhenError> {
    if o.keys().all(|k| keys.contains(&k.as_str())) {
        Ok(())
    } else {
        Err(WhenError::BadShape)
    }
}

fn add_part(
    part: &str,
    units: &[(char, i64)],
    total: &mut Duration,
    any: &mut bool,
) -> Result<(), WhenError> {
    let bad = || WhenError::BadDuration;
    let mut num = String::new();
    let mut last = 0usize;
    for c in part.chars() {
        if c.is_ascii_digit() {
            num.push(c);
            continue;
        }
        let (idx, mult) = units
            .iter()
            .enumerate()
            .find(|(_, (u, _))| *u == c)
            .map(|(i, (_, m))| (i + 1, *m))
            .ok_or_else(bad)?;
        if num.is_empty() || idx <= last {
            return Err(bad());
        }
        last = idx;
        let n: i64 = num.parse().map_err(|_| bad())?;
        let secs = n.checked_mul(mult).ok_or_else(bad)?;
        *total = total
            .checked_add(&Duration::try_seconds(secs).ok_or_else(bad)?)
            .ok_or_else(bad)?;
        *any = true;
        num.clear();
    }
    if num.is_empty() {
        Ok(())
    } else {
        Err(bad())
    }
}

/// Parse `PnWnDTnHnMnS` (integer parts, at least one; no years, months, fractions or sign).
pub fn parse_iso_duration(s: &str) -> Result<Duration, WhenError> {
    let rest = s.trim().strip_prefix('P').ok_or(WhenError::BadDuration)?;
    let (date, time) = match rest.split_once('T') {
        Some((d, t)) => (d, Some(t)),
        None => (rest, None),
    };
    if time == Some("") {
        return Err(WhenError::BadDuration);
    }
    let mut total = Duration::zero();
    let mut any = false;
    add_part(date, &[('W', 604_800), ('D', 86_400)], &mut total, &mut any)?;
    if let Some(t) = time {
        add_part(t, &[('H', 3600), ('M', 60), ('S', 1)], &mut total, &mut any)?;
    }
    if !any {
        return Err(WhenError::BadDuration);
    }
    Ok(total)
}

fn parse_naive(s: &str) -> Option<NaiveDateTime> {
    let s = s.trim();
    NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S")
        .or_else(|_| NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M"))
        .or_else(|_| NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S"))
        .or_else(|_| NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M"))
        .ok()
}

/// A wall time in `tz` as a UTC instant. A time inside a spring-forward gap becomes the first
/// valid instant after the gap; a repeated time becomes its first occurrence.
pub fn local_to_utc(local: NaiveDateTime, tz: Tz) -> DateTime<Utc> {
    let mut probe = local;
    // Gaps are at most a few hours; one day of minutes is a safe bound.
    for _ in 0..=1440 {
        match tz.from_local_datetime(&probe) {
            LocalResult::Single(t) => return t.with_timezone(&Utc),
            LocalResult::Ambiguous(first, _) => return first.with_timezone(&Utc),
            LocalResult::None => probe += Duration::minutes(1),
        }
    }
    local.and_utc()
}

impl When {
    /// Parse the `when` object of `schedule_create`.
    pub fn parse(v: &Value) -> Result<When, WhenError> {
        let obj = v.as_object().ok_or(WhenError::BadShape)?;
        if obj.len() != 1 {
            return Err(WhenError::BadShape);
        }
        if let Some(o) = obj.get("once") {
            let o = o.as_object().ok_or(WhenError::BadShape)?;
            only_keys(o, &["at", "timezone", "in"])?;
            return match (o.get("at"), o.get("in")) {
                (Some(_), Some(_)) | (None, None) => Err(WhenError::BadShape),
                (None, Some(i)) => {
                    if o.contains_key("timezone") {
                        return Err(WhenError::BadShape);
                    }
                    let d = parse_iso_duration(i.as_str().ok_or(WhenError::BadDuration)?)?;
                    if d <= Duration::zero() {
                        return Err(WhenError::NotPositive);
                    }
                    Ok(When::Once(OnceWhen::In(d)))
                }
                (Some(a), None) => {
                    let a = a.as_str().ok_or(WhenError::BadDatetime)?.trim();
                    if let Ok(t) = DateTime::parse_from_rfc3339(a) {
                        // The offset is explicit; a timezone beside it must still be valid.
                        if o.contains_key("timezone") {
                            parse_tz(o.get("timezone"))?;
                        }
                        return Ok(When::Once(OnceWhen::Instant(t)));
                    }
                    let tz = parse_tz(o.get("timezone"))?;
                    let local = parse_naive(a).ok_or(WhenError::BadDatetime)?;
                    Ok(When::Once(OnceWhen::At { local, tz }))
                }
            };
        }
        if let Some(c) = obj.get("cron") {
            let c = c.as_object().ok_or(WhenError::BadShape)?;
            only_keys(c, &["expression", "timezone"])?;
            let tz = parse_tz(c.get("timezone"))?;
            let expr = c
                .get("expression")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim();
            if expr.split_whitespace().count() != 5 {
                return Err(WhenError::InvalidCron);
            }
            let cron = CronParser::new()
                .parse(expr)
                .map_err(|_| WhenError::InvalidCron)?;
            return Ok(When::Cron {
                expression: expr.to_string(),
                tz,
                cron: Box::new(cron),
            });
        }
        Err(WhenError::BadShape)
    }

    /// The first fire, strictly after `saved_at` (when the schedule is saved).
    pub fn first_fire(&self, saved_at: DateTime<Utc>) -> Result<DateTime<Utc>, WhenError> {
        let at = match self {
            When::Once(OnceWhen::In(d)) => saved_at + *d,
            When::Once(OnceWhen::Instant(t)) => t.with_timezone(&Utc),
            When::Once(OnceWhen::At { local, tz }) => local_to_utc(*local, *tz),
            When::Cron { .. } => self.next_after(saved_at)?.ok_or(WhenError::NoNextFire)?,
        };
        if at > saved_at {
            Ok(at)
        } else {
            Err(WhenError::InThePast)
        }
    }

    /// The next fire strictly after `after`; `None` for a one-off.
    pub fn next_after(&self, after: DateTime<Utc>) -> Result<Option<DateTime<Utc>>, WhenError> {
        match self {
            When::Once(_) => Ok(None),
            When::Cron { tz, cron, .. } => cron
                .find_next_occurrence(&after.with_timezone(tz), false)
                .map(|t| Some(t.with_timezone(&Utc)))
                .map_err(|_| WhenError::NoNextFire),
        }
    }

    /// Cron floor: every fire within 31 days of `first` (at most 5000, stopping at the first
    /// violation) must be at least `min_interval` after the one before. Always Ok for a one-off.
    pub fn check_floor(
        &self,
        first: DateTime<Utc>,
        min_interval: std::time::Duration,
    ) -> Result<(), WhenError> {
        if matches!(self, When::Once(_)) {
            return Ok(());
        }
        let min = Duration::from_std(min_interval).unwrap_or(Duration::MAX);
        let end = first + Duration::days(FLOOR_SCAN_DAYS);
        let mut prev = first;
        for _ in 1..FLOOR_SCAN_CAP {
            let Some(next) = self.next_after(prev)? else {
                break;
            };
            if next > end {
                break;
            }
            if next - prev < min {
                return Err(WhenError::BelowFloor {
                    min_secs: min.num_seconds(),
                    got_secs: (next - prev).num_seconds(),
                });
            }
            prev = next;
        }
        Ok(())
    }

    /// How late a fire may still run (catch-up after downtime): once 24 h; cron half the gap
    /// to its following fire, at most 1 h. `nominal` is the fire's own time.
    pub fn grace_window(&self, nominal: DateTime<Utc>) -> Duration {
        match self {
            When::Once(_) => ONCE_GRACE,
            When::Cron { .. } => match self.next_after(nominal) {
                Ok(Some(n)) => ((n - nominal) / 2).min(CRON_GRACE_CAP),
                _ => CRON_GRACE_CAP,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn t(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }
    fn cron(expr: &str, tz: &str) -> When {
        When::parse(&json!({"cron": {"expression": expr, "timezone": tz}})).unwrap()
    }
    fn fires(w: &When, from: &str, n: usize) -> Vec<String> {
        let mut out = vec![];
        let mut cur = t(from);
        for _ in 0..n {
            cur = w.next_after(cur).unwrap().unwrap();
            out.push(cur.format("%Y-%m-%dT%H:%M:%SZ").to_string());
        }
        out
    }
    fn once_at(at: &str, tz: &str) -> Result<When, WhenError> {
        When::parse(&json!({"once": {"at": at, "timezone": tz}}))
    }
    fn secs(n: u64) -> std::time::Duration {
        std::time::Duration::from_secs(n)
    }

    // Ported from the prototype.
    #[test]
    fn once_and_cron_compute_a_future_next_fire() {
        let now = t("2026-09-18T12:00:00Z");
        let o = once_at("2099-06-01T15:00:00Z", "UTC").unwrap();
        assert_eq!(o.first_fire(now).unwrap(), t("2099-06-01T15:00:00Z"));
        let c = cron("0 0 1 1 *", "UTC");
        assert_eq!(c.first_fire(now).unwrap(), t("2027-01-01T00:00:00Z"));
    }

    #[test]
    fn past_once_is_refused() {
        let now = t("2026-09-18T12:00:00Z");
        let o = once_at("2020-01-01T00:00:00Z", "UTC").unwrap();
        assert_eq!(o.first_fire(now), Err(WhenError::InThePast));
        let eq = once_at("2026-09-18T12:00:00Z", "UTC").unwrap();
        assert_eq!(eq.first_fire(now), Err(WhenError::InThePast));
    }

    #[test]
    fn bad_timezone_is_refused() {
        assert_eq!(
            once_at("2099-01-01T00:00:00Z", "Not/A/Real/Zone").unwrap_err(),
            WhenError::BadTimezone
        );
        assert_eq!(
            once_at("2099-01-01T00:00", "").unwrap_err(),
            WhenError::BadTimezone
        );
        let e = When::parse(&json!({"cron": {"expression": "0 0 * * *", "timezone": "Mars/Base"}}));
        assert_eq!(e.unwrap_err(), WhenError::BadTimezone);
    }

    #[test]
    fn once_forms() {
        let now = t("2026-09-18T12:00:00Z");
        let w = once_at("2026-11-02T08:00", "Europe/Berlin").unwrap();
        assert_eq!(w.first_fire(now).unwrap(), t("2026-11-02T07:00:00Z"));
        let w = once_at("2026-11-02T08:00:00+01:00", "UTC").unwrap();
        assert_eq!(w.first_fire(now).unwrap(), t("2026-11-02T07:00:00Z"));
        assert_eq!(
            once_at("not a date", "UTC").unwrap_err(),
            WhenError::BadDatetime
        );
        let e = When::parse(&json!({"once": {"at": "2026-11-02T08:00"}}));
        assert_eq!(e.unwrap_err(), WhenError::BadTimezone);
    }

    #[test]
    fn once_in_counts_from_saved_time() {
        let w = When::parse(&json!({"once": {"in": "PT5S"}})).unwrap();
        assert_eq!(
            w.first_fire(t("2026-01-01T00:00:00Z")).unwrap(),
            t("2026-01-01T00:00:05Z")
        );
        assert_eq!(
            w.first_fire(t("2026-01-01T00:10:00Z")).unwrap(),
            t("2026-01-01T00:10:05Z")
        );
    }

    #[test]
    fn durations() {
        assert_eq!(parse_iso_duration("PT30M").unwrap(), Duration::minutes(30));
        assert_eq!(parse_iso_duration("P1DT2H").unwrap(), Duration::hours(26));
        assert_eq!(parse_iso_duration("P1W").unwrap(), Duration::days(7));
        assert_eq!(
            parse_iso_duration("PT1H30M5S").unwrap(),
            Duration::seconds(5405)
        );
        for bad in [
            "", "P", "PT", "5S", "P1Y", "P1M", "PT1.5S", "PT5", "PTS", "PT5M1H", "P1DT", "-PT5S",
        ] {
            assert_eq!(
                parse_iso_duration(bad),
                Err(WhenError::BadDuration),
                "{bad}"
            );
        }
    }

    #[test]
    fn once_in_must_be_positive() {
        for z in ["PT0S", "P0D"] {
            let e = When::parse(&json!({"once": {"in": z}}));
            assert_eq!(e.unwrap_err(), WhenError::NotPositive, "{z}");
        }
        let e = When::parse(&json!({"once": {"in": "-PT5S"}}));
        assert_eq!(e.unwrap_err(), WhenError::BadDuration);
    }

    #[test]
    fn shape_errors() {
        for v in [
            json!("x"),
            json!({}),
            json!({"once": {"at": "2099-01-01T00:00:00Z"}, "cron": {}}),
            json!({"once": {"at": "2099-01-01T00:00:00Z", "in": "PT5S"}}),
            json!({"once": {}}),
            json!({"once": {"in": "PT5S", "timezone": "UTC"}}),
            json!({"hourly": {}}),
        ] {
            assert_eq!(When::parse(&v).unwrap_err(), WhenError::BadShape, "{v}");
        }
    }

    #[test]
    fn cron_needs_five_fields() {
        for e in ["* * * *", "0 0 * * * *", "", "61 * * * *"] {
            let r = When::parse(&json!({"cron": {"expression": e, "timezone": "UTC"}}));
            assert_eq!(r.unwrap_err(), WhenError::InvalidCron, "{e}");
        }
    }

    // DST: 02:30 daily.
    #[test]
    fn berlin_spring_forward() {
        let w = cron("30 2 * * *", "Europe/Berlin");
        // 2027-03-28 02:30 does not exist; fires once, at 03:00 CEST = 01:00Z.
        assert_eq!(
            fires(&w, "2027-03-26T12:00:00Z", 3),
            [
                "2027-03-27T01:30:00Z",
                "2027-03-28T01:00:00Z",
                "2027-03-29T00:30:00Z"
            ]
        );
    }

    #[test]
    fn berlin_fall_back() {
        let w = cron("30 2 * * *", "Europe/Berlin");
        // 2026-10-25 02:30 happens twice; fires once, the first (CEST) = 00:30Z.
        assert_eq!(
            fires(&w, "2026-10-24T12:00:00Z", 3),
            [
                "2026-10-25T00:30:00Z",
                "2026-10-26T01:30:00Z",
                "2026-10-27T01:30:00Z"
            ]
        );
    }

    #[test]
    fn new_york_spring_forward() {
        let w = cron("30 2 * * *", "America/New_York");
        // 2027-03-14 02:30 does not exist; fires once, at 03:00 EDT = 07:00Z.
        assert_eq!(
            fires(&w, "2027-03-12T12:00:00Z", 3),
            [
                "2027-03-13T07:30:00Z",
                "2027-03-14T07:00:00Z",
                "2027-03-15T06:30:00Z"
            ]
        );
    }

    #[test]
    fn new_york_fall_back_0230_happens_once() {
        let w = cron("30 2 * * *", "America/New_York");
        // The repeated hour is 01:00-02:00; 02:30 on 2026-11-01 is EST = 07:30Z.
        assert_eq!(
            fires(&w, "2026-10-31T12:00:00Z", 3),
            [
                "2026-11-01T07:30:00Z",
                "2026-11-02T07:30:00Z",
                "2026-11-03T07:30:00Z"
            ]
        );
    }

    #[test]
    fn new_york_fall_back_0130_fires_once_at_the_first() {
        let w = cron("30 1 * * *", "America/New_York");
        assert_eq!(
            fires(&w, "2026-10-31T12:00:00Z", 3),
            [
                "2026-11-01T05:30:00Z",
                "2026-11-02T06:30:00Z",
                "2026-11-03T06:30:00Z"
            ]
        );
    }

    #[test]
    fn once_wall_time_across_transitions() {
        let now = t("2026-01-01T00:00:00Z");
        let w = once_at("2027-03-28T02:30", "Europe/Berlin").unwrap();
        assert_eq!(w.first_fire(now).unwrap(), t("2027-03-28T01:00:00Z"));
        let w = once_at("2027-03-14T02:30", "America/New_York").unwrap();
        assert_eq!(w.first_fire(now).unwrap(), t("2027-03-14T07:00:00Z"));
        let w = once_at("2026-10-25T02:30", "Europe/Berlin").unwrap();
        assert_eq!(w.first_fire(now).unwrap(), t("2026-10-25T00:30:00Z"));
        let w = once_at("2026-11-01T01:30", "America/New_York").unwrap();
        assert_eq!(w.first_fire(now).unwrap(), t("2026-11-01T05:30:00Z"));
        let w = once_at("2026-11-01T02:30", "America/New_York").unwrap();
        assert_eq!(w.first_fire(now).unwrap(), t("2026-11-01T07:30:00Z"));
    }

    fn floor(expr: &str, saved: &str) -> Result<(), WhenError> {
        let w = cron(expr, "UTC");
        let first = w.first_fire(t(saved)).unwrap();
        w.check_floor(first, secs(900))
    }

    #[test]
    fn floor_cases() {
        let saved = "2026-06-02T00:00:00Z";
        assert!(floor("0 9 * * *", saved).is_ok());
        assert!(floor("*/15 * * * *", saved).is_ok());
        assert!(matches!(
            floor("0,5 9 * * *", saved),
            Err(WhenError::BelowFloor { .. })
        ));
        assert!(matches!(
            floor("*/5 * * * *", saved),
            Err(WhenError::BelowFloor { .. })
        ));
        // Saved on the 2nd: the first fire is 2026-07-01 09:00 and the dense pair
        // 09:00/09:05 is not the first gap seen from the saved time, but is found by the scan.
        assert!(matches!(
            floor("0,5 9 1 * *", saved),
            Err(WhenError::BelowFloor { .. })
        ));
        let o = once_at("2099-01-01T00:00:00Z", "UTC").unwrap();
        assert!(o.check_floor(t("2099-01-01T00:00:00Z"), secs(900)).is_ok());
    }

    #[test]
    fn floor_dense_pair_after_a_sparse_start() {
        // Saved on the 2nd, the first fire is the 15th at 09:00; the next is 09:05, five
        // minutes later, which the floor must refuse.
        let w = cron("0,5 9 15 * *", "UTC");
        let first = w.first_fire(t("2026-06-02T00:00:00Z")).unwrap();
        assert_eq!(first, t("2026-06-15T09:00:00Z"));
        assert!(matches!(
            w.check_floor(first, secs(900)),
            Err(WhenError::BelowFloor { got_secs: 300, .. })
        ));
    }

    #[test]
    fn grace_windows() {
        let o = once_at("2099-01-01T00:00:00Z", "UTC").unwrap();
        assert_eq!(
            o.grace_window(t("2099-01-01T00:00:00Z")),
            Duration::hours(24)
        );
        let n = t("2026-06-02T09:00:00Z");
        assert_eq!(
            cron("*/10 * * * *", "UTC").grace_window(n),
            Duration::minutes(5)
        );
        assert_eq!(cron("0 9 * * *", "UTC").grace_window(n), Duration::hours(1));
        assert_eq!(
            cron("0 * * * *", "UTC").grace_window(n),
            Duration::minutes(30)
        );
    }

    #[test]
    fn errors_map_to_invalid_argument() {
        assert_eq!(WhenError::InThePast.code(), "INVALID_ARGUMENT");
    }
}
