//! Page size and opaque cursors for the runs, sessions and approvals lists (IT-2.5).
//!
//! `?pageSize=` is 1..=1000 (default 50; above the max is clamped). `?pageToken=` is the opaque
//! `pagination.nextPageToken` of the previous page: a keyset over the store's row order, never an
//! offset, so rows inserted or decided between pages cannot shift or repeat a page.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde::Deserialize;

use crate::error::OrchestratorError;

pub const DEFAULT_PAGE_SIZE: usize = 50;
pub const MAX_PAGE_SIZE: usize = 1000;

#[derive(Debug, Default, Deserialize)]
pub struct PageQuery {
    #[serde(rename = "pageSize")]
    pub page_size: Option<String>,
    #[serde(rename = "pageToken")]
    pub page_token: Option<String>,
}

fn bad(msg: &str) -> OrchestratorError {
    OrchestratorError::InvalidContract(msg.to_string())
}

impl PageQuery {
    /// The page size: default 50, clamped to 1000. Zero or absent is the default; a negative or
    /// non-numeric value is refused.
    pub fn size(&self) -> Result<usize, OrchestratorError> {
        let Some(raw) = self
            .page_size
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            return Ok(DEFAULT_PAGE_SIZE);
        };
        let n: i64 = raw
            .parse()
            .map_err(|_| bad("pageSize must be an integer from 1 to 1000"))?;
        match n {
            0 => Ok(DEFAULT_PAGE_SIZE),
            n if n < 0 => Err(bad("pageSize must be an integer from 1 to 1000")),
            n => Ok((n as usize).min(MAX_PAGE_SIZE)),
        }
    }

    /// The cursor parts for `kind` (`"runs"`, `"sessions"`, `"approvals"`); `None` for the first page.
    pub fn cursor(&self, kind: &str) -> Result<Option<Vec<String>>, OrchestratorError> {
        let Some(t) = self.page_token.as_deref().filter(|t| !t.is_empty()) else {
            return Ok(None);
        };
        let invalid = || bad("pageToken is not a token this list returned");
        let raw = URL_SAFE_NO_PAD.decode(t).map_err(|_| invalid())?;
        let text = String::from_utf8(raw).map_err(|_| invalid())?;
        let mut parts = text.split('\n');
        if parts.next() != Some("v1") || parts.next() != Some(kind) {
            return Err(invalid());
        }
        let rest: Vec<String> = parts.map(str::to_string).collect();
        if rest.is_empty() {
            return Err(invalid());
        }
        Ok(Some(rest))
    }
}

/// An opaque token for `kind` holding `parts` (the last row's keyset).
pub fn token(kind: &str, parts: &[String]) -> String {
    URL_SAFE_NO_PAD.encode(format!("v1\n{kind}\n{}", parts.join("\n")))
}

/// A cursor of one integer part.
pub fn int_cursor(c: Option<Vec<String>>) -> Result<Option<i64>, OrchestratorError> {
    match c {
        None => Ok(None),
        Some(p) if p.len() == 1 => p[0]
            .parse()
            .map(Some)
            .map_err(|_| bad("pageToken is not a token this list returned")),
        Some(_) => Err(bad("pageToken is not a token this list returned")),
    }
}

/// A cursor of an integer and a string part.
pub fn pair_cursor(c: Option<Vec<String>>) -> Result<Option<(i64, String)>, OrchestratorError> {
    match c {
        None => Ok(None),
        Some(p) if p.len() == 2 => {
            let n = p[0]
                .parse()
                .map_err(|_| bad("pageToken is not a token this list returned"))?;
            Ok(Some((n, p[1].clone())))
        }
        Some(_) => Err(bad("pageToken is not a token this list returned")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(size: Option<&str>, tok: Option<&str>) -> PageQuery {
        PageQuery {
            page_size: size.map(String::from),
            page_token: tok.map(String::from),
        }
    }

    #[test]
    fn default_max_and_refusals() {
        assert_eq!(q(None, None).size().unwrap(), 50);
        assert_eq!(q(Some("0"), None).size().unwrap(), 50);
        assert_eq!(q(Some(""), None).size().unwrap(), 50);
        assert_eq!(q(Some("7"), None).size().unwrap(), 7);
        assert_eq!(q(Some("1000"), None).size().unwrap(), 1000);
        assert_eq!(q(Some("5000"), None).size().unwrap(), 1000);
        assert!(q(Some("-1"), None).size().is_err());
        assert!(q(Some("lots"), None).size().is_err());
    }

    #[test]
    fn tokens_round_trip_and_are_bound_to_their_list() {
        let t = token("runs", &["42".into()]);
        assert_ne!(t, "42", "opaque, not the bare number");
        let c = q(None, Some(&t)).cursor("runs").unwrap();
        assert_eq!(int_cursor(c).unwrap(), Some(42));
        assert!(q(None, Some(&t)).cursor("sessions").is_err());
        let t = token("sessions", &["17".into(), "sess-a".into()]);
        let c = q(None, Some(&t)).cursor("sessions").unwrap();
        assert_eq!(pair_cursor(c).unwrap(), Some((17, "sess-a".to_string())));
        assert_eq!(q(None, None).cursor("runs").unwrap(), None);
        assert_eq!(q(None, Some("")).cursor("runs").unwrap(), None);
    }

    #[test]
    fn a_bad_token_is_refused() {
        for bad in [
            "not base64 !!",
            "AAAA",
            &URL_SAFE_NO_PAD.encode("v2\nruns\n1"),
        ] {
            assert!(q(None, Some(bad)).cursor("runs").is_err(), "{bad}");
        }
        let t = token("runs", &["abc".into()]);
        assert!(int_cursor(q(None, Some(&t)).cursor("runs").unwrap()).is_err());
        let t = token("runs", &["1".into(), "2".into()]);
        assert!(int_cursor(q(None, Some(&t)).cursor("runs").unwrap()).is_err());
    }
}
