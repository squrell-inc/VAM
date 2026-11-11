//! Host-side redacted body persistence for PEP and inference telemetry.

use vam_storage::ChunkStore;
use vam_telemetry::{
    DefaultRedactor, TelemetryCapture, TelemetryRedactor, CAPTURE_PREVIEW_MAX_BYTES,
};

pub fn redacted_preview_bytes(bytes: &[u8]) -> String {
    let take = bytes.len().min(CAPTURE_PREVIEW_MAX_BYTES);
    String::from_utf8_lossy(&bytes[..take]).into_owned()
}

pub struct CapturedBody {
    pub cas_digest: Option<String>,
    pub preview: Option<String>,
}

/// Persist redacted bytes to Drive9 when capture tier allows. Never uses guest-supplied digests.
pub fn persist_redacted_bytes(
    capture: TelemetryCapture,
    chunk_store: Option<&ChunkStore>,
    redacted: &[u8],
) -> CapturedBody {
    if !capture.persist_bodies() || redacted.is_empty() {
        return CapturedBody {
            cas_digest: None,
            preview: None,
        };
    }
    let cas_digest = chunk_store.and_then(|cs| cs.put_chunk(redacted).ok());
    let preview = if capture.include_preview() {
        Some(redacted_preview_bytes(redacted))
    } else {
        None
    };
    CapturedBody {
        cas_digest,
        preview,
    }
}

pub fn redact_json_value(mut value: serde_json::Value) -> serde_json::Value {
    DefaultRedactor::default().redact_json(&mut value);
    value
}

pub fn capture_redacted_text(
    capture: TelemetryCapture,
    chunk_store: Option<&ChunkStore>,
    text: &str,
) -> CapturedBody {
    if text.is_empty() {
        return CapturedBody {
            cas_digest: None,
            preview: None,
        };
    }
    let redacted = DefaultRedactor::default().redact_text(text);
    persist_redacted_bytes(capture, chunk_store, redacted.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    use vam_storage::{ChunkStore, Drive9Layout};

    #[test]
    fn none_skips_cas_and_preview() {
        let captured = persist_redacted_bytes(TelemetryCapture::None, None, b"secret=abc");
        assert!(captured.cas_digest.is_none());
        assert!(captured.preview.is_none());
    }

    #[test]
    fn redacted_full_includes_preview_and_cas() {
        let dir = tempdir().expect("tempdir");
        let store = ChunkStore::new(Drive9Layout::new(dir.path().join("drive9")));
        let body = b"{\"api_key\":\"[REDACTED]\"}";
        let captured = persist_redacted_bytes(TelemetryCapture::RedactedFull, Some(&store), body);
        assert!(captured.cas_digest.is_some());
        assert_eq!(
            captured.preview.as_deref(),
            Some("{\"api_key\":\"[REDACTED]\"}")
        );
    }

    #[test]
    fn redact_json_strips_secrets_before_persist() {
        let mut val = serde_json::json!({"token": "Bearer sk-live-secret1234567890"});
        val = redact_json_value(val);
        let s = val.to_string();
        assert!(!s.contains("sk-live"));
        assert!(s.contains("REDACTED") || s.contains("[REDACTED"));
    }
}
