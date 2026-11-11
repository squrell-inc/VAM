//! `format` keywords the playbook forms understand (decision 21, G4).
//!
//! `date` is an ISO `YYYY-MM-DD` calendar date. `vam-workspace-file` is a
//! workspace-relative path (a file form value is the path of an upload). Any
//! other format is ignored, as in standard JSON Schema.

use thiserror::Error;

/// `format` value for a calendar date.
pub const FORMAT_DATE: &str = "date";
/// `format` value for a workspace file path.
pub const FORMAT_WORKSPACE_FILE: &str = "vam-workspace-file";
/// Longest accepted workspace path, in characters.
pub const WORKSPACE_PATH_MAX_CHARS: usize = 1024;

#[derive(Debug, Error, PartialEq, Eq)]
#[error("{0}")]
pub struct FormatError(pub String);

/// Check `value` against `format`. Unknown formats pass.
pub fn check_format(format: &str, value: &str) -> Result<(), FormatError> {
    match format {
        FORMAT_DATE => check_date(value),
        FORMAT_WORKSPACE_FILE => check_workspace_file(value),
        _ => Ok(()),
    }
}

fn check_date(value: &str) -> Result<(), FormatError> {
    let bad = || FormatError(format!("'{value}' is not a date (use YYYY-MM-DD)"));
    let bytes = value.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return Err(bad());
    }
    let digits = |range: std::ops::Range<usize>| -> Option<u32> {
        let part = value.get(range)?;
        if part.bytes().all(|b| b.is_ascii_digit()) {
            part.parse().ok()
        } else {
            None
        }
    };
    let (Some(year), Some(month), Some(day)) = (digits(0..4), digits(5..7), digits(8..10)) else {
        return Err(bad());
    };
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    let max_day = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return Err(bad()),
    };
    if day == 0 || day > max_day {
        return Err(bad());
    }
    Ok(())
}

fn check_workspace_file(value: &str) -> Result<(), FormatError> {
    let bad = |why: &str| FormatError(format!("'{value}' is not a workspace file path: {why}"));
    if value.is_empty() {
        return Err(bad("it is empty"));
    }
    if value.chars().count() > WORKSPACE_PATH_MAX_CHARS {
        return Err(bad("it is too long"));
    }
    if value.contains('\0') {
        return Err(bad("it contains a NUL"));
    }
    if value.starts_with('/') || value.starts_with('\\') {
        return Err(bad("it must be relative to the workspace"));
    }
    if value.split(['/', '\\']).any(|part| part == "..") {
        return Err(bad("it must not contain '..'"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates() {
        for ok in ["2026-10-17", "2024-02-29", "2000-02-29"] {
            assert!(check_format("date", ok).is_ok(), "{ok}");
        }
        for bad in [
            "2026-02-30",
            "2023-02-29",
            "1900-02-29",
            "2026-13-01",
            "2026-00-10",
            "26-10-17",
            "2026/10/17",
            "2026-10-1x",
            "",
        ] {
            assert!(check_format("date", bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn workspace_files() {
        for ok in ["uploads/invoice.pdf", "a.txt"] {
            assert!(check_format("vam-workspace-file", ok).is_ok(), "{ok}");
        }
        for bad in ["", "/etc/passwd", "../x", "a/../../b", "a\\..\\b", "a\0b"] {
            assert!(check_format("vam-workspace-file", bad).is_err(), "{bad:?}");
        }
        assert!(check_format("vam-workspace-file", &"a".repeat(1025)).is_err());
    }

    #[test]
    fn unknown_formats_pass() {
        assert!(check_format("email", "not an email").is_ok());
    }
}
