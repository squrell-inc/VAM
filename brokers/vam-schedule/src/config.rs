//! Flag parsing helpers.

use std::time::Duration;

use crate::error::ScheduleError;

/// Parse `15m`, `90s`, `2h` (a whole number and one unit). Zero is refused.
pub fn parse_interval(text: &str) -> Result<Duration, ScheduleError> {
    let bad = || {
        ScheduleError::InvalidArgument(format!(
            "interval `{text}`: use a whole number and s, m or h (for example 15m)"
        ))
    };
    let text = text.trim();
    let unit = text.chars().last().ok_or_else(bad)?;
    let mult: u64 = match unit {
        's' => 1,
        'm' => 60,
        'h' => 3600,
        _ => return Err(bad()),
    };
    let n: u64 = text[..text.len() - 1].parse().map_err(|_| bad())?;
    let secs = n.checked_mul(mult).ok_or_else(bad)?;
    if secs == 0 {
        return Err(bad());
    }
    Ok(Duration::from_secs(secs))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_units() {
        assert_eq!(parse_interval("15m").unwrap(), Duration::from_secs(900));
        assert_eq!(parse_interval("90s").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_interval("2h").unwrap(), Duration::from_secs(7200));
    }

    #[test]
    fn refuses_nonsense() {
        for t in ["", "m", "0m", "15", "1.5h", "-3m", "5d", "x5m"] {
            assert!(parse_interval(t).is_err(), "{t}");
        }
    }
}
