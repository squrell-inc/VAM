use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;
use vam_spec::v1::LogFrame;

use crate::redact::{DefaultRedactor, TelemetryRedactor};
use crate::semconv::VAM_USER_ID;
use crate::trace_context::validate_traceparent;

/// Default maximum inline payload size before spilling to CAS (Decision T31).
pub const DEFAULT_MAX_INLINE_BYTES: usize = 4096;

/// Default maximum number of attributes allowed on a guest frame.
pub const DEFAULT_MAX_ATTRIBUTES: usize = 16;

/// Default maximum attribute key length.
pub const DEFAULT_MAX_ATTR_KEY_LEN: usize = 64;

/// Default maximum attribute value length.
pub const DEFAULT_MAX_ATTR_VAL_LEN: usize = 128;

/// Outcome of validating and processing an incoming guest telemetry frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IngressDecision {
    /// The frame was accepted after sanitization and normalization.
    Accepted {
        /// Host-assigned chronological ingestion sequence counter.
        host_ingestion_seq: u64,
        /// If the payload exceeded inline limits, the redacted full body to spill to CAS.
        spilled_payload: Option<Vec<u8>>,
    },
    /// The frame was rejected/dropped (e.g. rate limit exceeded, invalid structure).
    Dropped { reason: String },
}

/// Token bucket rate limiter for per-session guest telemetry streams.
#[derive(Debug)]
pub struct GuestRateLimiter {
    tokens_per_sec: f64,
    max_burst: f64,
    current_tokens: f64,
    last_update: Instant,
}

impl GuestRateLimiter {
    pub fn new(tokens_per_sec: f64, max_burst: f64) -> Self {
        Self {
            tokens_per_sec,
            max_burst,
            current_tokens: max_burst,
            last_update: Instant::now(),
        }
    }

    /// Default configuration: 500 frames/sec with burst capacity of 1000 frames.
    pub fn default_per_session() -> Self {
        Self::new(500.0, 1000.0)
    }

    /// Try to acquire 1 token. Returns true if permitted, false if rate limited.
    pub fn try_acquire(&mut self) -> bool {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_update).as_secs_f64();
        self.last_update = now;

        self.current_tokens =
            (self.current_tokens + elapsed * self.tokens_per_sec).min(self.max_burst);

        if self.current_tokens >= 1.0 {
            self.current_tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// Host-side ingestion validator for untrusted guest telemetry (Decisions T3, T7, T20, T21, T31).
///
/// Ensures:
/// 1. Traceparent headers originating from guest are strictly validated; forged headers are dropped.
/// 2. Attributes are capped in count and length, and sanitized.
/// 3. Large stdout/stderr payloads (>4 KiB) are redacted and prepared for CAS spill.
/// 4. Monotonic host ingestion sequence is assigned.
/// 5. Host is shielded against flood denial-of-service via token-bucket rate limiting.
pub struct GuestIngressValidator {
    session_id: String,
    rate_limiter: std::sync::Mutex<GuestRateLimiter>,
    ingestion_sequence: AtomicU64,
    max_inline_bytes: usize,
    max_attributes: usize,
    max_attr_key_len: usize,
    max_attr_val_len: usize,
    redactor: DefaultRedactor,
    /// Host-stamped owner from the run record. Guest `vam.user.id` is ignored.
    host_user_id: Option<String>,
}

impl GuestIngressValidator {
    pub fn new(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            rate_limiter: std::sync::Mutex::new(GuestRateLimiter::default_per_session()),
            ingestion_sequence: AtomicU64::new(1),
            max_inline_bytes: DEFAULT_MAX_INLINE_BYTES,
            max_attributes: DEFAULT_MAX_ATTRIBUTES,
            max_attr_key_len: DEFAULT_MAX_ATTR_KEY_LEN,
            max_attr_val_len: DEFAULT_MAX_ATTR_VAL_LEN,
            redactor: DefaultRedactor::default(),
            host_user_id: None,
        }
    }

    /// Stamp `vam.user.id` from the run's `owner_user_id` after guest strip.
    pub fn with_host_user_id(mut self, user_id: impl Into<String>) -> Self {
        let id = user_id.into();
        self.host_user_id = if id.is_empty() { None } else { Some(id) };
        self
    }

    pub fn with_rate_limits(mut self, tokens_per_sec: f64, max_burst: f64) -> Self {
        self.rate_limiter = std::sync::Mutex::new(GuestRateLimiter::new(tokens_per_sec, max_burst));
        self
    }

    pub fn with_max_inline_bytes(mut self, bytes: usize) -> Self {
        self.max_inline_bytes = bytes;
        self
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Process and sanitize an incoming guest `LogFrame`.
    ///
    /// Modifies the frame in-place to enforce safety invariants:
    /// - Normalizes clock domain to `"guest_domain"`
    /// - Drops invalid/forged W3C traceparents
    /// - Truncates excess or oversized attributes
    /// - Redacts sensitive data
    /// - Spills large bodies if over inline threshold
    pub fn process_frame(&self, frame: &mut LogFrame) -> IngressDecision {
        // 1. Enforce per-session rate limit
        {
            let mut limiter = self.rate_limiter.lock().unwrap_or_else(|e| e.into_inner());
            if !limiter.try_acquire() {
                return IngressDecision::Dropped {
                    reason: format!(
                        "Per-session rate limit exceeded for session '{}'",
                        self.session_id
                    ),
                };
            }
        }

        // 2. Clock domain normalization
        if frame.clock_domain.is_empty() {
            frame.clock_domain = "guest_domain".to_string();
        }

        // 3. W3C Trace Context validation (Decision T7: Untrusted Guest Validation)
        if let Some(ref tc) = frame.trace_context {
            if tc.traceparent.is_empty() {
                frame.trace_context = None;
            } else if validate_traceparent(&tc.traceparent).is_err() {
                // Drop forged or malformed trace context
                frame.trace_context = None;
            }
        }

        // 4. Attribute sanitization & cardinality bounding (Decision T20)
        if frame.attributes.len() > self.max_attributes {
            // Retain up to max_attributes
            let keys: Vec<String> = frame.attributes.keys().cloned().collect();
            for key in keys.into_iter().skip(self.max_attributes) {
                frame.attributes.remove(&key);
            }
        }

        let mut sanitized_attrs = std::collections::HashMap::new();
        for (k, v) in frame.attributes.drain() {
            let mut clean_key = k;
            if clean_key.len() > self.max_attr_key_len {
                clean_key.truncate(self.max_attr_key_len);
            }
            let mut clean_val = v;
            if clean_val.len() > self.max_attr_val_len {
                clean_val.truncate(self.max_attr_val_len);
            }
            sanitized_attrs.insert(clean_key, clean_val);
        }
        frame.attributes = sanitized_attrs;

        // 4b. Guest-asserted identity is untrusted; host stamps from the run record.
        strip_guest_user_attributes(&mut frame.attributes);
        if let Some(ref user_id) = self.host_user_id {
            frame
                .attributes
                .insert(VAM_USER_ID.to_string(), user_id.clone());
        }

        // 5. Payload Redaction & CAS Spilling (Decisions T21, T31)
        let mut spilled_payload = None;
        if !frame.chunk.is_empty() {
            let chunk_str = String::from_utf8_lossy(&frame.chunk);
            let redacted_str = self.redactor.redact_text(&chunk_str);
            let redacted_bytes = redacted_str.as_bytes().to_vec();

            if redacted_bytes.len() > self.max_inline_bytes {
                // Compute SHA-256 digest
                let mut hasher = Sha256::new();
                hasher.update(&redacted_bytes);
                let digest = format!("{:x}", hasher.finalize());

                frame.digest = digest.clone();

                // Truncate inline payload with pointer notation
                let mut truncated = redacted_bytes[..self.max_inline_bytes].to_vec();
                truncated.extend_from_slice(
                    format!("\n[Payload spilled to CAS: sha256:{}]", digest).as_bytes(),
                );
                frame.chunk = truncated;

                spilled_payload = Some(redacted_bytes);
            } else {
                frame.chunk = redacted_bytes;
            }
        }

        // 6. Assign host ingestion sequence
        let host_ingestion_seq = self.ingestion_sequence.fetch_add(1, Ordering::SeqCst);

        IngressDecision::Accepted {
            host_ingestion_seq,
            spilled_payload,
        }
    }
}

/// Remove guest-asserted `vam.user.id` / `user_id` from frame attributes.
pub fn strip_guest_user_attributes(attrs: &mut std::collections::HashMap<String, String>) {
    attrs.remove(VAM_USER_ID);
    attrs.remove("user_id");
}

#[cfg(test)]
mod tests {
    use super::*;
    use vam_spec::v1::TraceContext;

    #[test]
    fn test_valid_traceparent_accepted() {
        let validator = GuestIngressValidator::new("session-1");
        let valid_tp = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01".to_string();
        let mut frame = LogFrame {
            instance_id: None,
            timestamp: None,
            channel: "span".to_string(),
            chunk: Vec::new(),
            sequence_number: 1,
            trace_context: Some(TraceContext {
                traceparent: valid_tp.clone(),
                tracestate: "rojo=1".to_string(),
            }),
            duration_ns: 1_000_000,
            span_name: "tool.exec".to_string(),
            attributes: Default::default(),
            clock_domain: "".to_string(),
            digest: String::new(),
        };

        let decision = validator.process_frame(&mut frame);
        match decision {
            IngressDecision::Accepted {
                host_ingestion_seq, ..
            } => {
                assert_eq!(host_ingestion_seq, 1);
                assert_eq!(frame.clock_domain, "guest_domain");
                assert!(frame.trace_context.is_some());
                assert_eq!(frame.trace_context.unwrap().traceparent, valid_tp);
            }
            IngressDecision::Dropped { reason } => {
                panic!("Expected accepted, got dropped: {}", reason)
            }
        }
    }

    #[test]
    fn test_forged_traceparent_dropped() {
        let validator = GuestIngressValidator::new("session-1");
        let forged_tp = "invalid-or-forged-traceparent-garbage".to_string();
        let mut frame = LogFrame {
            instance_id: None,
            timestamp: None,
            channel: "span".to_string(),
            chunk: Vec::new(),
            sequence_number: 2,
            trace_context: Some(TraceContext {
                traceparent: forged_tp,
                tracestate: "".to_string(),
            }),
            duration_ns: 500_000,
            span_name: "tool.exec".to_string(),
            attributes: Default::default(),
            clock_domain: "".to_string(),
            digest: String::new(),
        };

        let decision = validator.process_frame(&mut frame);
        match decision {
            IngressDecision::Accepted { .. } => {
                // Forged traceparent must be stripped
                assert!(
                    frame.trace_context.is_none(),
                    "Forged traceparent should have been stripped"
                );
            }
            IngressDecision::Dropped { reason } => panic!(
                "Expected accepted with stripped traceparent, got dropped: {}",
                reason
            ),
        }
    }

    #[test]
    fn test_attribute_limits_enforced() {
        let validator = GuestIngressValidator::new("session-1");
        let mut attrs = std::collections::HashMap::new();
        for i in 0..25 {
            attrs.insert(format!("key_{:02}", i), format!("val_{:02}", i));
        }
        let long_key = "k".repeat(100);
        let long_val = "v".repeat(200);
        attrs.insert(long_key.clone(), long_val.clone());

        let mut frame = LogFrame {
            instance_id: None,
            timestamp: None,
            channel: "span".to_string(),
            chunk: Vec::new(),
            sequence_number: 3,
            trace_context: None,
            duration_ns: 200_000,
            span_name: "agent.react_iteration".to_string(),
            attributes: attrs,
            clock_domain: "".to_string(),
            digest: String::new(),
        };

        let decision = validator.process_frame(&mut frame);
        match decision {
            IngressDecision::Accepted { .. } => {
                assert!(frame.attributes.len() <= DEFAULT_MAX_ATTRIBUTES);
                for (k, v) in &frame.attributes {
                    assert!(k.len() <= DEFAULT_MAX_ATTR_KEY_LEN);
                    assert!(v.len() <= DEFAULT_MAX_ATTR_VAL_LEN);
                }
            }
            IngressDecision::Dropped { reason } => panic!("Expected accepted, got: {}", reason),
        }
    }

    #[test]
    fn test_rate_limiter_drops_on_saturation() {
        // Create validator with very small burst
        let validator = GuestIngressValidator::new("session-flood").with_rate_limits(1.0, 2.0);

        let mut frame = LogFrame {
            instance_id: None,
            timestamp: None,
            channel: "system".to_string(),
            chunk: b"hello".to_vec(),
            sequence_number: 1,
            trace_context: None,
            duration_ns: 0,
            span_name: "".to_string(),
            attributes: Default::default(),
            clock_domain: "".to_string(),
            digest: String::new(),
        };

        // First 2 should succeed (burst 2)
        assert!(matches!(
            validator.process_frame(&mut frame),
            IngressDecision::Accepted { .. }
        ));
        assert!(matches!(
            validator.process_frame(&mut frame),
            IngressDecision::Accepted { .. }
        ));

        // 3rd frame immediately should be dropped
        let third = validator.process_frame(&mut frame);
        assert!(
            matches!(third, IngressDecision::Dropped { .. }),
            "Third frame should be dropped by rate limiter"
        );
    }

    #[test]
    fn test_cas_spill_large_payload_and_redaction() {
        let validator = GuestIngressValidator::new("session-cas").with_max_inline_bytes(100);

        // Body > 100 bytes with secret key that needs redaction
        let mut raw_chunk = Vec::new();
        raw_chunk.extend_from_slice(b"Log output with token sk-ant-api03-1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef and lots more data to exceed one hundred bytes limit easily for testing CAS spill.");

        let mut frame = LogFrame {
            instance_id: None,
            timestamp: None,
            channel: "stdout".to_string(),
            chunk: raw_chunk,
            sequence_number: 10,
            trace_context: None,
            duration_ns: 0,
            span_name: "".to_string(),
            attributes: Default::default(),
            clock_domain: "".to_string(),
            digest: String::new(),
        };

        let decision = validator.process_frame(&mut frame);
        match decision {
            IngressDecision::Accepted {
                spilled_payload, ..
            } => {
                assert!(!frame.digest.is_empty(), "Digest should be populated");
                assert!(
                    spilled_payload.is_some(),
                    "Spilled payload should be present"
                );

                let spilled = spilled_payload.unwrap();
                let spilled_str = String::from_utf8_lossy(&spilled);
                // Secret should be redacted
                assert!(
                    !spilled_str.contains("sk-ant-api03-1234567890abcdef"),
                    "Raw API key must be redacted"
                );
                assert!(spilled_str.contains("[REDACTED_API_KEY]"));

                // Inline chunk contains pointer to CAS digest
                let inline_str = String::from_utf8_lossy(&frame.chunk);
                assert!(inline_str.contains("[Payload spilled to CAS: sha256:"));
            }
            IngressDecision::Dropped { reason } => panic!("Expected accepted, got: {}", reason),
        }
    }

    #[test]
    fn guest_user_id_stripped_and_host_stamped() {
        let validator = GuestIngressValidator::new("session-1").with_host_user_id("alice");
        let mut attrs = std::collections::HashMap::new();
        attrs.insert(VAM_USER_ID.to_string(), "attacker".to_string());
        attrs.insert("user_id".to_string(), "attacker".to_string());
        attrs.insert("tool.name".to_string(), "bash".to_string());

        let mut frame = LogFrame {
            instance_id: None,
            timestamp: None,
            channel: "span".to_string(),
            chunk: Vec::new(),
            sequence_number: 1,
            trace_context: None,
            duration_ns: 1,
            span_name: "tool.exec".to_string(),
            attributes: attrs,
            clock_domain: String::new(),
            digest: String::new(),
        };

        assert!(matches!(
            validator.process_frame(&mut frame),
            IngressDecision::Accepted { .. }
        ));
        assert_eq!(
            frame.attributes.get(VAM_USER_ID).map(String::as_str),
            Some("alice")
        );
        assert!(!frame.attributes.contains_key("user_id"));
        assert_eq!(
            frame.attributes.get("tool.name").map(String::as_str),
            Some("bash")
        );
    }

    #[test]
    fn guest_user_id_stripped_without_host_stamp() {
        let validator = GuestIngressValidator::new("session-1");
        let mut attrs = std::collections::HashMap::new();
        attrs.insert(VAM_USER_ID.to_string(), "attacker".to_string());
        let mut frame = LogFrame {
            instance_id: None,
            timestamp: None,
            channel: "span".to_string(),
            chunk: Vec::new(),
            sequence_number: 1,
            trace_context: None,
            duration_ns: 1,
            span_name: "tool.exec".to_string(),
            attributes: attrs,
            clock_domain: String::new(),
            digest: String::new(),
        };
        assert!(matches!(
            validator.process_frame(&mut frame),
            IngressDecision::Accepted { .. }
        ));
        assert!(!frame.attributes.contains_key(VAM_USER_ID));
    }
}
