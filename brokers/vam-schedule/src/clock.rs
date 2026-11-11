//! The clock the schedule broker reads. Production uses [`SystemClock`]; tests move a
//! [`TestClock`] without sleeping.

use std::sync::{Arc, Mutex};

use chrono::{DateTime, Duration, Utc};

/// A source of the current UTC instant.
pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}

/// The wall clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

/// A clock a test sets and advances by hand. Clones share the same time.
#[derive(Debug, Clone)]
pub struct TestClock {
    at: Arc<Mutex<DateTime<Utc>>>,
}

impl TestClock {
    pub fn new(at: DateTime<Utc>) -> Self {
        Self {
            at: Arc::new(Mutex::new(at)),
        }
    }

    pub fn set(&self, at: DateTime<Utc>) {
        let mut g = self.at.lock().unwrap_or_else(|p| p.into_inner());
        *g = at;
    }

    pub fn advance(&self, by: Duration) {
        let mut g = self.at.lock().unwrap_or_else(|p| p.into_inner());
        *g += by;
    }
}

impl Clock for TestClock {
    fn now(&self) -> DateTime<Utc> {
        *self.at.lock().unwrap_or_else(|p| p.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    #[test]
    fn test_clock_moves_without_sleeping() {
        let c = TestClock::new(t("2026-01-01T00:00:00Z"));
        let clone = c.clone();
        c.advance(Duration::hours(2));
        assert_eq!(clone.now(), t("2026-01-01T02:00:00Z"));
        c.set(t("2027-01-01T00:00:00Z"));
        assert_eq!(clone.now(), t("2027-01-01T00:00:00Z"));
    }

    #[test]
    fn system_clock_is_close_to_utc_now() {
        let d = (SystemClock.now() - Utc::now()).num_seconds().abs();
        assert!(d < 5);
    }
}
