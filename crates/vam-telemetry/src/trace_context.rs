use crate::error::VamTelemetryError;
use crate::semconv::VAM_USER_ID;
use regex::Regex;
use std::collections::HashSet;
use std::sync::OnceLock;

/// W3C Traceparent pattern: version(2 hex)-trace_id(32 hex)-parent_id(16 hex)-flags(2 hex)
/// Total length is strictly 55 characters.
const W3C_TRACEPARENT_LEN: usize = 55;
const MAX_BAGGAGE_ENTRIES: usize = 5;
const MAX_BAGGAGE_KEY_LEN: usize = 64;
const MAX_BAGGAGE_VAL_LEN: usize = 64;

static TRACEPARENT_REGEX: OnceLock<Regex> = OnceLock::new();

fn traceparent_regex() -> &'static Regex {
    TRACEPARENT_REGEX.get_or_init(|| {
        Regex::new(r"^([0-9a-f]{2})-([0-9a-f]{32})-([0-9a-f]{16})-([0-9a-f]{2})$")
            .expect("static traceparent regex must compile")
    })
}

/// Approved baggage key allowlist for host protection (Decision T7).
///
/// `vam.user.id` is allowlisted so the **host** can stamp it on approved
/// baggage. Guest-asserted values are stripped by [`strip_guest_user_id`]
/// and [`inbound_baggage`] — never trusted as identity.
pub fn default_baggage_allowlist() -> HashSet<&'static str> {
    let mut s = HashSet::new();
    s.insert("vam.tenant.id");
    s.insert(VAM_USER_ID);
    s.insert("vam.sample.rate");
    s.insert("vam.origin");
    s.insert("vam.thread.id");
    s.insert("vam.environment");
    s
}

/// Drop guest-asserted `vam.user.id` (and the `user_id` alias). Host stamps
/// the run owner's id afterwards via [`stamp_host_user_id`].
pub fn strip_guest_user_id(entries: &mut Vec<(String, String)>) {
    entries.retain(|(k, _)| k != VAM_USER_ID && k != "user_id");
}

/// Insert host-stamped `vam.user.id` from the run record. Replaces any
/// remaining guest value.
pub fn stamp_host_user_id(entries: &mut Vec<(String, String)>, owner_user_id: &str) {
    strip_guest_user_id(entries);
    if owner_user_id.is_empty() {
        return;
    }
    entries.push((VAM_USER_ID.to_string(), owner_user_id.to_string()));
}

/// Parsed and validated W3C Traceparent carrier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Traceparent {
    pub version: String,
    pub trace_id: String,
    pub parent_id: String,
    pub flags: String,
}

impl Traceparent {
    /// Parse and validate a W3C traceparent string conforming to RFC specifications.
    pub fn parse(raw: &str) -> Result<Self, VamTelemetryError> {
        let raw = raw.trim();
        if raw.len() != W3C_TRACEPARENT_LEN {
            return Err(VamTelemetryError::InvalidTraceContext {
                details: format!(
                    "traceparent length must be exactly {} characters, found {}",
                    W3C_TRACEPARENT_LEN,
                    raw.len()
                ),
            });
        }

        let caps = traceparent_regex().captures(raw).ok_or_else(|| {
            VamTelemetryError::InvalidTraceContext {
                details: format!("traceparent '{}' does not match W3C hex format", raw),
            }
        })?;

        let version = caps[1].to_string();
        let trace_id = caps[2].to_string();
        let parent_id = caps[3].to_string();
        let flags = caps[4].to_string();

        // W3C spec: version "ff" is forbidden
        if version == "ff" {
            return Err(VamTelemetryError::InvalidTraceContext {
                details: "traceparent version 'ff' is invalid under W3C specification".to_string(),
            });
        }

        // Trace ID must not be all zeros
        if trace_id == "00000000000000000000000000000000" {
            return Err(VamTelemetryError::InvalidTraceContext {
                details: "trace_id cannot be all zeros".to_string(),
            });
        }

        // Parent span ID must not be all zeros
        if parent_id == "0000000000000000" {
            return Err(VamTelemetryError::InvalidTraceContext {
                details: "parent_span_id cannot be all zeros".to_string(),
            });
        }

        Ok(Self {
            version,
            trace_id,
            parent_id,
            flags,
        })
    }

    /// Generate a new root W3C Traceparent with cryptographically random IDs and sampled flag (01).
    pub fn generate_root() -> Self {
        use rand::RngCore;
        let mut rng = rand::thread_rng();
        let mut trace_bytes = [0u8; 16];
        let mut span_bytes = [0u8; 8];
        rng.fill_bytes(&mut trace_bytes);
        rng.fill_bytes(&mut span_bytes);
        if trace_bytes.iter().all(|&b| b == 0) {
            trace_bytes[15] = 1;
        }
        if span_bytes.iter().all(|&b| b == 0) {
            span_bytes[7] = 1;
        }
        Self {
            version: "00".to_string(),
            trace_id: hex_encode(&trace_bytes),
            parent_id: hex_encode(&span_bytes),
            flags: "01".to_string(),
        }
    }

    /// Generate a child Traceparent retaining the same trace_id, with a new span_id.
    pub fn child(&self) -> Self {
        use rand::RngCore;
        let mut rng = rand::thread_rng();
        let mut span_bytes = [0u8; 8];
        rng.fill_bytes(&mut span_bytes);
        if span_bytes.iter().all(|&b| b == 0) {
            span_bytes[7] = 1;
        }
        Self {
            version: self.version.clone(),
            trace_id: self.trace_id.clone(),
            parent_id: hex_encode(&span_bytes),
            flags: self.flags.clone(),
        }
    }

    /// Construct a Traceparent from raw trace_id, span_id, and optional flags.
    pub fn from_ids(
        trace_id: &str,
        span_id: &str,
        flags: Option<&str>,
    ) -> Result<Self, VamTelemetryError> {
        let formatted = format!("00-{}-{}-{}", trace_id, span_id, flags.unwrap_or("01"));
        Self::parse(&formatted)
    }

    /// Construct or derive a valid Traceparent from an optional seed string (like run_id)
    /// and optional parent span.
    pub fn from_seed_or_random(seed: Option<&str>, parent_span: Option<&str>) -> Self {
        use rand::RngCore;
        use sha2::{Digest, Sha256};

        let trace_id = match seed {
            Some(s) if s.len() == 32 && s.chars().all(|c| c.is_ascii_hexdigit()) => {
                s.to_ascii_lowercase()
            }
            Some(s) => {
                let mut hasher = Sha256::new();
                hasher.update(s.as_bytes());
                let result = hasher.finalize();
                hex_encode(&result[0..16])
            }
            None => {
                let mut rng = rand::thread_rng();
                let mut bytes = [0u8; 16];
                rng.fill_bytes(&mut bytes);
                if bytes.iter().all(|&b| b == 0) {
                    bytes[15] = 1;
                }
                hex_encode(&bytes)
            }
        };

        let span_id = match parent_span {
            Some(sp) if sp.len() == 16 && sp.chars().all(|c| c.is_ascii_hexdigit()) => {
                sp.to_ascii_lowercase()
            }
            _ => {
                let mut rng = rand::thread_rng();
                let mut bytes = [0u8; 8];
                rng.fill_bytes(&mut bytes);
                if bytes.iter().all(|&b| b == 0) {
                    bytes[7] = 1;
                }
                hex_encode(&bytes)
            }
        };

        Self {
            version: "00".to_string(),
            trace_id,
            parent_id: span_id,
            flags: "01".to_string(),
        }
    }

    /// Return the canonical W3C traceparent string representation.
    pub fn to_header_value(&self) -> String {
        format!(
            "{}-{}-{}-{}",
            self.version, self.trace_id, self.parent_id, self.flags
        )
    }

    /// Helper to convert into protobuf `vam_spec::v1::TraceContext`.
    pub fn to_proto(&self, tracestate: Option<String>) -> vam_spec::v1::TraceContext {
        vam_spec::v1::TraceContext {
            traceparent: self.to_header_value(),
            tracestate: tracestate.unwrap_or_default(),
        }
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(s, "{:02x}", b);
    }
    s
}

/// Helper to construct a protobuf `vam_spec::v1::TraceContext` from an optional seed/run_id.
pub fn create_trace_context(
    seed: Option<&str>,
    parent_span: Option<&str>,
) -> vam_spec::v1::TraceContext {
    let tp = Traceparent::from_seed_or_random(seed, parent_span);
    tp.to_proto(None)
}

/// Helper to construct a child `TraceContext` from an optional parent `TraceContext` or seed.
pub fn create_child_trace_context(
    parent: Option<&vam_spec::v1::TraceContext>,
    seed: Option<&str>,
) -> vam_spec::v1::TraceContext {
    if let Some(parent_ctx) = parent {
        if let Ok(tp) = Traceparent::parse(&parent_ctx.traceparent) {
            let child = tp.child();
            return child.to_proto(if parent_ctx.tracestate.is_empty() {
                None
            } else {
                Some(parent_ctx.tracestate.clone())
            });
        }
    }
    create_trace_context(seed, None)
}

/// Validate raw traceparent string.
pub fn validate_traceparent(raw: &str) -> Result<Traceparent, VamTelemetryError> {
    Traceparent::parse(raw)
}

/// Extract a validated inbound W3C `traceparent`. Invalid or missing → `None`
/// (caller starts a new root). Never used as auth (Decision T7).
pub fn inbound_traceparent(header_value: Option<&str>) -> Option<Traceparent> {
    let raw = header_value?.trim();
    if raw.is_empty() {
        return None;
    }
    validate_traceparent(raw).ok()
}

/// Optional `tracestate` passthrough. Empty / missing → `None`. Not auth (T7).
pub fn inbound_tracestate(header_value: Option<&str>) -> Option<String> {
    let raw = header_value?.trim();
    if raw.is_empty() {
        return None;
    }
    Some(raw.to_string())
}

/// Optional `baggage` after [`BaggageValidator`]. Invalid entries are dropped,
/// never rejected as a request error (T7).
pub fn inbound_baggage(header_value: Option<&str>) -> Option<Vec<(String, String)>> {
    let raw = header_value?.trim();
    if raw.is_empty() {
        return None;
    }
    let mut entries: Vec<(String, String)> = raw
        .split(',')
        .filter_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            let key = k.trim();
            let val = v.trim();
            if key.is_empty() {
                return None;
            }
            Some((key.to_string(), val.to_string()))
        })
        .collect();
    if entries.is_empty() {
        return None;
    }
    // Guest-asserted identity is untrusted (same rule as trace baggage).
    strip_guest_user_id(&mut entries);
    if entries.is_empty() {
        return None;
    }
    let refs: Vec<(&str, &str)> = entries
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    BaggageValidator::default().validate(&refs).ok()?;
    Some(entries)
}

fn decode_hex_bytes<const N: usize>(hex: &str) -> Option<[u8; N]> {
    if hex.len() != N * 2 {
        return None;
    }
    let mut out = [0u8; N];
    for i in 0..N {
        out[i] = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

/// Remote parent [`opentelemetry::Context`] from a validated traceparent.
pub fn remote_parent_context(tp: &Traceparent) -> Option<opentelemetry::Context> {
    use opentelemetry::trace::{
        SpanContext, SpanId, TraceContextExt, TraceFlags, TraceId, TraceState,
    };

    let trace_id = TraceId::from_bytes(decode_hex_bytes::<16>(&tp.trace_id)?);
    let span_id = SpanId::from_bytes(decode_hex_bytes::<8>(&tp.parent_id)?);
    let sampled = tp
        .flags
        .as_bytes()
        .last()
        .map(|b| b & 1 == 1)
        .unwrap_or(false);
    let flags = if sampled {
        TraceFlags::SAMPLED
    } else {
        TraceFlags::default()
    };
    let span_context = SpanContext::new(trace_id, span_id, flags, true, TraceState::default());
    Some(opentelemetry::Context::new().with_remote_span_context(span_context))
}

/// Continue `span` under a validated inbound parent. Missing parent → no-op
/// (new root). Requires the `tracing-opentelemetry` layer to export.
pub fn apply_inbound_parent(span: &tracing::Span, parent: Option<&Traceparent>) {
    use tracing_opentelemetry::OpenTelemetrySpanExt;
    let Some(tp) = parent else {
        return;
    };
    let Some(cx) = remote_parent_context(tp) else {
        return;
    };
    let _ = span.set_parent(cx);
}

/// Validate untrusted guest baggage (Decision T7).
pub struct BaggageValidator {
    allowlist: HashSet<&'static str>,
}

impl Default for BaggageValidator {
    fn default() -> Self {
        Self {
            allowlist: default_baggage_allowlist(),
        }
    }
}

impl BaggageValidator {
    pub fn new(allowlist: HashSet<&'static str>) -> Self {
        Self { allowlist }
    }

    /// Validate a sequence of key-value baggage items.
    pub fn validate<K, V>(&self, entries: &[(K, V)]) -> Result<(), VamTelemetryError>
    where
        K: AsRef<str>,
        V: AsRef<str>,
    {
        if entries.len() > MAX_BAGGAGE_ENTRIES {
            return Err(VamTelemetryError::BaggageExceeded {
                details: format!(
                    "maximum allowed baggage entries is {}, received {}",
                    MAX_BAGGAGE_ENTRIES,
                    entries.len()
                ),
            });
        }

        for (k, v) in entries {
            let key = k.as_ref();
            let val = v.as_ref();

            if key.len() > MAX_BAGGAGE_KEY_LEN {
                return Err(VamTelemetryError::BaggageExceeded {
                    details: format!(
                        "baggage key '{}' exceeds maximum allowed size of {} bytes",
                        key, MAX_BAGGAGE_KEY_LEN
                    ),
                });
            }

            if val.len() > MAX_BAGGAGE_VAL_LEN {
                return Err(VamTelemetryError::BaggageExceeded {
                    details: format!(
                        "baggage value for key '{}' exceeds maximum allowed size of {} bytes",
                        key, MAX_BAGGAGE_VAL_LEN
                    ),
                });
            }

            if !self.allowlist.contains(key) {
                return Err(VamTelemetryError::BaggageExceeded {
                    details: format!("baggage key '{}' is not in the approved allowlist", key),
                });
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_valid_w3c_traceparent() {
        let raw = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
        let tp = Traceparent::parse(raw).expect("valid traceparent must parse");
        assert_eq!(tp.version, "00");
        assert_eq!(tp.trace_id, "4bf92f3577b34da6a3ce929d0e0e4736");
        assert_eq!(tp.parent_id, "00f067aa0ba902b7");
        assert_eq!(tp.flags, "01");
        assert_eq!(tp.to_header_value(), raw);
    }

    #[test]
    fn test_invalid_w3c_traceparent_all_zeros() {
        let raw = "00-00000000000000000000000000000000-00f067aa0ba902b7-01";
        assert!(Traceparent::parse(raw).is_err());

        let raw2 = "00-4bf92f3577b34da6a3ce929d0e0e4736-0000000000000000-01";
        assert!(Traceparent::parse(raw2).is_err());
    }

    #[test]
    fn test_invalid_w3c_traceparent_version_ff() {
        let raw = "ff-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
        assert!(Traceparent::parse(raw).is_err());
    }

    #[test]
    fn test_invalid_w3c_traceparent_length() {
        let short = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7";
        assert!(Traceparent::parse(short).is_err());

        let long = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-0100";
        assert!(Traceparent::parse(long).is_err());
    }

    #[test]
    fn test_invalid_w3c_traceparent_non_hex() {
        let raw = "00-4bf92f3577b34da6a3ce929d0e0e473z-00f067aa0ba902b7-01";
        assert!(Traceparent::parse(raw).is_err());
    }

    #[test]
    fn test_baggage_allowlist_enforcement() {
        let validator = BaggageValidator::default();

        // Valid allowed entries
        let ok_entries = vec![("vam.tenant.id", "tenant-123"), ("vam.origin", "cli")];
        assert!(validator.validate(&ok_entries).is_ok());

        // Forbidden unapproved key
        let bad_entries = vec![("untrusted.custom.key", "some-value")];
        assert!(validator.validate(&bad_entries).is_err());

        // Value too long (> 64 bytes)
        let long_val = "x".repeat(65);
        let oversized = vec![("vam.tenant.id", long_val.as_str())];
        assert!(validator.validate(&oversized).is_err());

        // Exceed max entries (> 5)
        let too_many = vec![
            ("vam.tenant.id", "1"),
            ("vam.origin", "2"),
            ("vam.sample.rate", "3"),
            ("vam.thread.id", "4"),
            ("vam.environment", "5"),
            (VAM_USER_ID, "6"),
        ];
        assert!(validator.validate(&too_many).is_err());
    }

    #[test]
    fn inbound_baggage_strips_guest_user_id() {
        let mixed = inbound_baggage(Some("vam.origin=cli,vam.user.id=attacker")).expect("kept");
        assert_eq!(mixed, vec![("vam.origin".to_string(), "cli".to_string())]);
        assert!(inbound_baggage(Some("vam.user.id=attacker")).is_none());
        assert!(inbound_baggage(Some("user_id=attacker")).is_none());
    }

    #[test]
    fn host_stamp_replaces_guest_user_id() {
        let mut entries = vec![
            ("vam.origin".to_string(), "cli".to_string()),
            (VAM_USER_ID.to_string(), "attacker".to_string()),
        ];
        stamp_host_user_id(&mut entries, "alice");
        assert_eq!(
            entries,
            vec![
                ("vam.origin".to_string(), "cli".to_string()),
                (VAM_USER_ID.to_string(), "alice".to_string()),
            ]
        );
        let validator = BaggageValidator::default();
        let refs: Vec<(&str, &str)> = entries
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        assert!(validator.validate(&refs).is_ok());
    }

    #[test]
    fn test_traceparent_generate_root_and_child() {
        let root = Traceparent::generate_root();
        assert_eq!(root.version, "00");
        assert_eq!(root.trace_id.len(), 32);
        assert_eq!(root.parent_id.len(), 16);
        assert_eq!(root.flags, "01");

        // Canonical parse must succeed
        let parsed =
            Traceparent::parse(&root.to_header_value()).expect("generated root must parse");
        assert_eq!(parsed, root);

        let child = root.child();
        assert_eq!(child.version, root.version);
        assert_eq!(child.trace_id, root.trace_id);
        assert_eq!(child.flags, root.flags);
        assert_ne!(child.parent_id, root.parent_id);
    }

    #[test]
    fn test_traceparent_from_seed_and_child_context() {
        let ctx = create_trace_context(Some("run-12345"), None);
        assert!(!ctx.traceparent.is_empty());
        let parsed = Traceparent::parse(&ctx.traceparent).expect("seed context must parse");
        assert_eq!(parsed.flags, "01");

        let child_ctx = create_child_trace_context(Some(&ctx), None);
        let child_parsed =
            Traceparent::parse(&child_ctx.traceparent).expect("child context must parse");
        assert_eq!(child_parsed.trace_id, parsed.trace_id);
        assert_ne!(child_parsed.parent_id, parsed.parent_id);
    }

    #[test]
    fn inbound_traceparent_accepts_valid_and_drops_invalid() {
        let valid = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
        let tp = inbound_traceparent(Some(valid)).expect("valid header");
        assert_eq!(tp.trace_id, "4bf92f3577b34da6a3ce929d0e0e4736");
        assert!(inbound_traceparent(None).is_none());
        assert!(inbound_traceparent(Some("")).is_none());
        assert!(inbound_traceparent(Some("not-a-traceparent")).is_none());
        assert!(inbound_traceparent(Some(
            "00-00000000000000000000000000000000-00f067aa0ba902b7-01"
        ))
        .is_none());
    }

    #[test]
    fn inbound_baggage_drops_unapproved_keys() {
        assert!(inbound_baggage(Some("vam.origin=cli")).is_some());
        assert!(inbound_baggage(Some("evil.key=1")).is_none());
        assert!(inbound_baggage(None).is_none());
    }

    #[test]
    fn apply_inbound_parent_continues_trace_id() {
        use opentelemetry::trace::{TraceContextExt, TracerProvider as _};
        use opentelemetry_sdk::trace::SdkTracerProvider;
        use tracing_opentelemetry::OpenTelemetrySpanExt;
        use tracing_subscriber::layer::SubscriberExt;

        let raw = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
        let tp = inbound_traceparent(Some(raw)).expect("valid");
        let provider = SdkTracerProvider::builder().build();
        let tracer = provider.tracer("vamd-test-parent");
        let layer = tracing_opentelemetry::layer().with_tracer(tracer);
        let subscriber = tracing_subscriber::Registry::default().with(layer);

        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!("run");
            apply_inbound_parent(&span, Some(&tp));
            let _g = span.enter();
            let sc = span.context().span().span_context().clone();
            assert_eq!(format!("{:032x}", sc.trace_id()), tp.trace_id);
            assert!(sc.is_valid());
        });
        let _ = provider.shutdown();
    }
}
