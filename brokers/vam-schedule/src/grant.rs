//! The run-grant token: a bearer secret that is only ever sent to the gateway socket.

use std::fmt;

/// A run-grant token (`vam_rg_<grant_id>.<secret>`). `Debug` and `Display` never show it.
#[derive(Clone, PartialEq, Eq)]
pub struct GrantToken(String);

impl GrantToken {
    pub fn new(token: impl Into<String>) -> Self {
        Self(token.into())
    }

    /// The raw token. Call it only to build a gateway request or to store the row.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for GrantToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("GrantToken(<redacted>)")
    }
}

impl fmt::Display for GrantToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_and_display_hide_the_token() {
        let t = GrantToken::new("vam_rg_rg_ABC.topsecret");
        assert!(!format!("{t:?}").contains("topsecret"));
        assert!(!format!("{t}").contains("topsecret"));
        assert_eq!(t.expose(), "vam_rg_rg_ABC.topsecret");
        let nested = Some(t);
        assert!(!format!("{nested:?}").contains("topsecret"));
    }
}
