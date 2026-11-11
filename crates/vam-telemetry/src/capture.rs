//! Tool-body capture tiers for guest `StreamLogs` ingest.

use std::fmt;
use std::str::FromStr;

/// Environment variable read by `vamd` (`DaemonConfig`) for guest tool-body capture.
pub const ENV_TELEMETRY_CAPTURE: &str = "VAMD_TELEMETRY_CAPTURE";

/// Maximum redacted preview attached to a `redacted-full` catalog event.
pub const CAPTURE_PREVIEW_MAX_BYTES: usize = 4096;

/// How much guest tool stdout/stderr the host persists and attaches to catalog events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TelemetryCapture {
    /// Name, exit code, and duration only. No CAS write.
    None,
    /// Persist redacted body to CAS; attach the host-computed SHA-256 digest.
    #[default]
    Digest,
    /// Persist redacted body to CAS; attach digest plus a short redacted preview.
    RedactedFull,
}

impl TelemetryCapture {
    /// Parse an env / config token. Unknown values fall back to [`TelemetryCapture::Digest`].
    pub fn parse_env_value(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "none" => Self::None,
            "redacted-full" | "redacted_full" | "redactedfull" => Self::RedactedFull,
            "digest" => Self::Digest,
            _ => Self::Digest,
        }
    }

    pub fn from_env() -> Self {
        match std::env::var(ENV_TELEMETRY_CAPTURE) {
            Ok(v) => Self::parse_env_value(&v),
            Err(_) => Self::Digest,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Digest => "digest",
            Self::RedactedFull => "redacted-full",
        }
    }

    pub fn persist_bodies(self) -> bool {
        !matches!(self, Self::None)
    }

    pub fn include_preview(self) -> bool {
        matches!(self, Self::RedactedFull)
    }
}

impl fmt::Display for TelemetryCapture {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for TelemetryCapture {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self::parse_env_value(s))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_env_falls_back_to_digest() {
        assert_eq!(
            TelemetryCapture::parse_env_value("bogus"),
            TelemetryCapture::Digest
        );
        assert_eq!(
            TelemetryCapture::parse_env_value(""),
            TelemetryCapture::Digest
        );
        assert_eq!(
            TelemetryCapture::parse_env_value("NONE"),
            TelemetryCapture::None
        );
        assert_eq!(
            TelemetryCapture::parse_env_value("redacted-full"),
            TelemetryCapture::RedactedFull
        );
        assert_eq!(
            TelemetryCapture::parse_env_value("digest"),
            TelemetryCapture::Digest
        );
    }
}
