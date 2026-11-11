use prost::Message;
use std::collections::HashMap;
use std::sync::Arc;
use tracing::{debug, info, warn};

use crate::trajectory::emit_guest_tool_outcome;
use crate::workflow::SqliteWorkflowStore;
use vam_spec::v1::{LogFrame, StreamLogsResponse, VsockEnvelope};
use vam_storage::ChunkStore;
use vam_telemetry::{
    GuestIngressValidator, IngressDecision, TelemetryCapture, CAPTURE_PREVIEW_MAX_BYTES,
};

const MAX_LEASE_BODY_CACHE: usize = 128;

/// Latest redacted stdout/stderr digests (and optional previews) for one guest lease.
#[derive(Debug, Default, Clone)]
struct LeaseLogBodies {
    tool_name: Option<String>,
    stdout_digest: Option<String>,
    stderr_digest: Option<String>,
    stdout_preview: Option<String>,
    stderr_preview: Option<String>,
}

/// Host-side ingestion processor for untrusted guest `StreamLogs` telemetry.
/// Enforces Zero-Trust boundary (Decisions T3, T7, T30, T31):
/// - Drops forged trace contexts and enforces W3C traceparent specifications.
/// - Caps attribute count, key size, and value size to prevent memory exhaustion.
/// - Performs token-bucket rate limiting per session.
/// - Redacts sensitive tokens and spills payloads to CAS per capture tier.
/// - Maintains dual sequence numbers (guest monotonic + host monotonic).
///   Observational only: does not change Cedar / workflow state.
pub struct GuestStreamReceiver {
    pub session_id: String,
    pub run_id: String,
    validator: GuestIngressValidator,
    chunk_store: Option<ChunkStore>,
    workflow_store: Option<Arc<SqliteWorkflowStore>>,
    capture: TelemetryCapture,
    lease_bodies: HashMap<String, LeaseLogBodies>,
    highest_guest_seq: u64,
    processed_count: u64,
    dropped_count: u64,
}

impl GuestStreamReceiver {
    pub fn new(
        session_id: &str,
        run_id: &str,
        chunk_store: Option<ChunkStore>,
        workflow_store: Option<Arc<SqliteWorkflowStore>>,
    ) -> Self {
        let mut validator = GuestIngressValidator::new(session_id);
        if let Some(ref store) = workflow_store {
            if let Some(owner) = host_user_id_for_run(store, run_id, session_id) {
                validator = validator.with_host_user_id(owner);
            }
        }
        Self {
            session_id: session_id.to_string(),
            run_id: run_id.to_string(),
            validator,
            chunk_store,
            workflow_store,
            capture: TelemetryCapture::default(),
            lease_bodies: HashMap::new(),
            highest_guest_seq: 0,
            processed_count: 0,
            dropped_count: 0,
        }
    }

    pub fn with_rate_limits(mut self, tokens_per_sec: f64, max_burst: f64) -> Self {
        self.validator = self.validator.with_rate_limits(tokens_per_sec, max_burst);
        self
    }

    pub fn with_capture(mut self, capture: TelemetryCapture) -> Self {
        self.capture = capture;
        self
    }

    pub fn capture(&self) -> TelemetryCapture {
        self.capture
    }

    /// Highest guest sequence number successfully acknowledged.
    pub fn highest_guest_seq(&self) -> u64 {
        self.highest_guest_seq
    }

    pub fn processed_count(&self) -> u64 {
        self.processed_count
    }

    pub fn dropped_count(&self) -> u64 {
        self.dropped_count
    }

    /// Persist redacted bytes to Drive9 CAS. Returns the host-computed digest.
    /// Never uses a guest-supplied digest as the CAS key.
    fn persist_redacted_body(&self, redacted: &[u8]) -> Option<String> {
        if !self.capture.persist_bodies() || redacted.is_empty() {
            return None;
        }
        let cs = self.chunk_store.as_ref()?;
        match cs.put_chunk(redacted) {
            Ok(hash) => Some(hash),
            Err(e) => {
                warn!(
                    session_id = %self.session_id,
                    error = %e,
                    "Failed to put telemetry log chunk into CAS"
                );
                None
            }
        }
    }

    fn remember_lease_body(
        &mut self,
        lease_id: &str,
        tool_name: Option<&str>,
        channel: &str,
        digest: Option<String>,
        preview: Option<String>,
    ) {
        if lease_id.is_empty() {
            return;
        }
        if self.lease_bodies.len() >= MAX_LEASE_BODY_CACHE
            && !self.lease_bodies.contains_key(lease_id)
        {
            if let Some(old) = self.lease_bodies.keys().next().cloned() {
                self.lease_bodies.remove(&old);
            }
        }
        let entry = self.lease_bodies.entry(lease_id.to_string()).or_default();
        if let Some(name) = tool_name.filter(|n| !n.is_empty()) {
            entry.tool_name = Some(name.to_string());
        }
        match channel {
            "stdout" => {
                entry.stdout_digest = digest;
                entry.stdout_preview = preview;
            }
            "stderr" => {
                entry.stderr_digest = digest;
                entry.stderr_preview = preview;
            }
            _ => {}
        }
    }

    fn emit_observational_event(&self, event_type: &str, payload_json: &str) {
        let Some(ref store) = self.workflow_store else {
            return;
        };
        if let Err(e) = store.append_outbox_event(&self.run_id, event_type, payload_json) {
            warn!(
                run_id = %self.run_id,
                event_type,
                error = %e,
                "Failed to append observational guest telemetry event"
            );
        }
    }

    fn emit_tool_catalog_event(&mut self, frame: &LogFrame) {
        let tool_name = frame
            .attributes
            .get("tool.name")
            .cloned()
            .unwrap_or_default();
        let lease_id = frame
            .attributes
            .get("lease.id")
            .cloned()
            .unwrap_or_default();
        let exit_code = frame
            .attributes
            .get("tool.exit_code")
            .and_then(|s| s.parse::<i32>().ok())
            .unwrap_or(0);

        let bodies = if lease_id.is_empty() {
            None
        } else {
            self.lease_bodies.remove(&lease_id)
        };

        let failed = exit_code != 0;

        let mut payload = serde_json::json!({
            "tool_name": tool_name,
            "exit_code": exit_code,
            "duration_ns": frame.duration_ns,
            "capture": self.capture.as_str(),
        });

        if !lease_id.is_empty() {
            payload["lease_id"] = serde_json::Value::String(lease_id);
        }

        if self.capture.persist_bodies() {
            if let Some(ref bodies) = bodies {
                if let Some(ref d) = bodies.stdout_digest {
                    payload["stdout_digest"] = serde_json::Value::String(d.clone());
                }
                if let Some(ref d) = bodies.stderr_digest {
                    payload["stderr_digest"] = serde_json::Value::String(d.clone());
                }
                if self.capture.include_preview() {
                    if let Some(ref p) = bodies.stdout_preview {
                        payload["stdout_preview"] = serde_json::Value::String(p.clone());
                    }
                    if let Some(ref p) = bodies.stderr_preview {
                        payload["stderr_preview"] = serde_json::Value::String(p.clone());
                    }
                }
            }
        }

        if let Some(ref store) = self.workflow_store {
            emit_guest_tool_outcome(store, &self.run_id, &payload, failed);
        }
    }

    /// Ingests, validates, and sanitizes a single `LogFrame`.
    pub fn process_frame(&mut self, mut raw_frame: LogFrame) -> Result<Option<LogFrame>, String> {
        let guest_seq = raw_frame.sequence_number;

        match self.validator.process_frame(&mut raw_frame) {
            IngressDecision::Accepted {
                host_ingestion_seq,
                spilled_payload,
            } => {
                let is_stdio = raw_frame.channel == "stdout" || raw_frame.channel == "stderr";

                if is_stdio {
                    let redacted_body = spilled_payload
                        .as_deref()
                        .unwrap_or(raw_frame.chunk.as_slice());
                    // Never trust a guest-supplied digest as the CAS key.
                    raw_frame.digest.clear();
                    if let Some(hash) = self.persist_redacted_body(redacted_body) {
                        raw_frame.digest = hash.clone();
                        let preview = if self.capture.include_preview() {
                            Some(redacted_preview(redacted_body))
                        } else {
                            None
                        };
                        let lease_id = raw_frame
                            .attributes
                            .get("lease.id")
                            .map(|s| s.as_str())
                            .unwrap_or("");
                        let tool_name = raw_frame.attributes.get("tool.name").map(|s| s.as_str());
                        self.remember_lease_body(
                            lease_id,
                            tool_name,
                            &raw_frame.channel,
                            Some(hash),
                            preview,
                        );
                    }
                }

                debug!(
                    session_id = %self.session_id,
                    guest_seq = guest_seq,
                    host_seq = host_ingestion_seq,
                    channel = %raw_frame.channel,
                    "Accepted and sanitized guest telemetry frame"
                );

                self.highest_guest_seq = self.highest_guest_seq.max(raw_frame.sequence_number);
                self.processed_count += 1;

                if raw_frame.channel == "span" {
                    info!(
                        target: "guest_telemetry",
                        session_id = %self.session_id,
                        run_id = %self.run_id,
                        span_name = %raw_frame.span_name,
                        duration_ns = raw_frame.duration_ns,
                        clock_domain = %raw_frame.clock_domain,
                        "Guest span observed"
                    );
                }

                let is_tool_exec =
                    raw_frame.channel == "span" && raw_frame.span_name == "tool.exec";
                if is_tool_exec {
                    self.emit_tool_catalog_event(&raw_frame);
                } else {
                    let event_type = if raw_frame.channel == "span" {
                        format!("vam.v1.guest.span.{}", raw_frame.span_name)
                    } else {
                        format!("vam.v1.guest.log.{}", raw_frame.channel)
                    };

                    let payload_json = serde_json::json!({
                        "session_id": self.session_id,
                        "run_id": self.run_id,
                        "channel": raw_frame.channel,
                        "span_name": raw_frame.span_name,
                        "duration_ns": raw_frame.duration_ns,
                        "guest_seq": raw_frame.sequence_number,
                        "host_seq": host_ingestion_seq,
                        "digest": raw_frame.digest,
                        "attributes": raw_frame.attributes,
                    })
                    .to_string();

                    self.emit_observational_event(&event_type, &payload_json);
                }

                Ok(Some(raw_frame))
            }
            IngressDecision::Dropped { reason } => {
                self.dropped_count += 1;
                warn!(
                    session_id = %self.session_id,
                    guest_seq = guest_seq,
                    reason = %reason,
                    "Guest telemetry frame dropped"
                );
                Ok(None)
            }
        }
    }

    /// Ingests a batch of `LogFrame` messages and returns the `StreamLogsResponse`.
    pub fn process_batch(&mut self, frames: Vec<LogFrame>) -> StreamLogsResponse {
        for frame in frames {
            let _ = self.process_frame(frame);
        }

        StreamLogsResponse {
            success: true,
            last_acknowledged_sequence: self.highest_guest_seq,
        }
    }

    /// Handles a VsockEnvelope carrying a `StreamLogs` RPC payload.
    pub fn handle_vsock_envelope(
        &mut self,
        env: &VsockEnvelope,
    ) -> Result<Option<VsockEnvelope>, String> {
        if env.method != "StreamLogs" {
            return Ok(None);
        }

        let frame = LogFrame::decode(&*env.payload)
            .map_err(|e| format!("Failed to decode LogFrame: {}", e))?;

        let _ = self.process_frame(frame)?;

        let response = StreamLogsResponse {
            success: true,
            last_acknowledged_sequence: self.highest_guest_seq,
        };

        let mut resp_payload = Vec::new();
        response
            .encode(&mut resp_payload)
            .map_err(|e| format!("Failed to encode StreamLogsResponse: {}", e))?;

        Ok(Some(VsockEnvelope {
            method: "StreamLogsResponse".to_string(),
            correlation_id: env.correlation_id,
            payload: resp_payload,
            error: None,
            trace_context: env.trace_context.clone(),
        }))
    }
}

fn redacted_preview(bytes: &[u8]) -> String {
    let take = bytes.len().min(CAPTURE_PREVIEW_MAX_BYTES);
    String::from_utf8_lossy(&bytes[..take]).into_owned()
}

fn host_user_id_for_run(
    store: &SqliteWorkflowStore,
    run_id: &str,
    session_id: &str,
) -> Option<String> {
    store
        .run_owner_user_id(run_id)
        .ok()
        .or_else(|| store.session_owner_user_id(session_id).ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Arc;
    use vam_spec::v1::TraceContext;
    use vam_telemetry::DEFAULT_MAX_INLINE_BYTES;

    #[test]
    fn test_guest_stream_receiver_valid_and_forged() {
        let mut receiver = GuestStreamReceiver::new("session-100", "run-100", None, None);

        // 1. Valid frame with conformant W3C traceparent
        let valid_frame = LogFrame {
            instance_id: None,
            timestamp: None,
            channel: "span".to_string(),
            chunk: Vec::new(),
            sequence_number: 1,
            trace_context: Some(TraceContext {
                traceparent: "00-4bf92f3577b34da6a3ce929d0e0e4736-1111222233334444-01".to_string(),
                tracestate: String::new(),
            }),
            duration_ns: 120_000_000,
            span_name: "tool.exec".to_string(),
            attributes: HashMap::new(),
            clock_domain: "guest_domain".to_string(),
            digest: String::new(),
        };

        let res = receiver.process_frame(valid_frame).unwrap();
        assert!(res.is_some());
        let frame = res.unwrap();
        assert!(frame.trace_context.is_some());
        assert_eq!(receiver.highest_guest_seq(), 1);
        assert_eq!(receiver.processed_count(), 1);
        assert_eq!(receiver.dropped_count(), 0);

        // 2. Malformed/forged trace context is sanitized (trace_context removed)
        let forged_frame = LogFrame {
            instance_id: None,
            timestamp: None,
            channel: "span".to_string(),
            chunk: Vec::new(),
            sequence_number: 2,
            trace_context: Some(TraceContext {
                traceparent: "invalid-traceparent-format".to_string(),
                tracestate: String::new(),
            }),
            duration_ns: 50_000_000,
            span_name: "tool.exec".to_string(),
            attributes: HashMap::new(),
            clock_domain: "guest_domain".to_string(),
            digest: String::new(),
        };

        let res2 = receiver.process_frame(forged_frame).unwrap();
        assert!(res2.is_some());
        let frame2 = res2.unwrap();
        assert!(
            frame2.trace_context.is_none(),
            "Forged trace_context must be dropped"
        );
        assert_eq!(receiver.highest_guest_seq(), 2);
    }

    #[test]
    fn test_guest_stream_receiver_cas_spill_large_log() {
        let temp_dir = tempfile::tempdir().unwrap();
        let layout = vam_storage::Drive9Layout::new(temp_dir.path());
        let chunk_store = ChunkStore::new(layout);

        let mut receiver =
            GuestStreamReceiver::new("session-200", "run-200", Some(chunk_store.clone()), None);

        // Create 8 KiB stdout payload
        let large_bytes = vec![b'A'; 8192];
        let frame = LogFrame {
            instance_id: None,
            timestamp: None,
            channel: "stdout".to_string(),
            chunk: large_bytes.clone(),
            sequence_number: 10,
            trace_context: None,
            duration_ns: 0,
            span_name: String::new(),
            attributes: HashMap::new(),
            clock_domain: "guest_domain".to_string(),
            digest: String::new(),
        };

        let processed = receiver.process_frame(frame).unwrap().unwrap();
        assert_eq!(processed.sequence_number, 10);
        assert!(
            !processed.digest.is_empty(),
            "Large payload must have CAS digest set"
        );
        assert!(
            chunk_store.has_chunk(&processed.digest),
            "Host-computed digest must address a CAS chunk"
        );
        assert!(
            processed.chunk.len() > DEFAULT_MAX_INLINE_BYTES,
            "Pointer notation appended"
        );
        assert_eq!(receiver.highest_guest_seq(), 10);
    }

    #[test]
    fn test_guest_stream_receiver_cas_persists_small_body() {
        let temp_dir = tempfile::tempdir().unwrap();
        let layout = vam_storage::Drive9Layout::new(temp_dir.path());
        let chunk_store = ChunkStore::new(layout);

        let mut receiver =
            GuestStreamReceiver::new("session-201", "run-201", Some(chunk_store.clone()), None);

        let frame = LogFrame {
            instance_id: None,
            timestamp: None,
            channel: "stderr".to_string(),
            chunk: b"small body".to_vec(),
            sequence_number: 1,
            trace_context: None,
            duration_ns: 0,
            span_name: String::new(),
            attributes: HashMap::new(),
            clock_domain: "guest_domain".to_string(),
            digest: "guest-forged-digest".to_string(),
        };

        let processed = receiver.process_frame(frame).unwrap().unwrap();
        assert_ne!(
            processed.digest, "guest-forged-digest",
            "Must not trust guest digest as CAS key"
        );
        assert!(chunk_store.has_chunk(&processed.digest));
    }

    #[test]
    fn test_guest_stream_receiver_batch_and_response() {
        let mut receiver = GuestStreamReceiver::new("session-300", "run-300", None, None);

        let mut batch = Vec::new();
        for i in 1..=5 {
            batch.push(LogFrame {
                instance_id: None,
                timestamp: None,
                channel: "stdout".to_string(),
                chunk: format!("log message {}", i).into_bytes(),
                sequence_number: i,
                trace_context: None,
                duration_ns: 0,
                span_name: String::new(),
                attributes: HashMap::new(),
                clock_domain: "guest_domain".to_string(),
                digest: String::new(),
            });
        }

        let resp = receiver.process_batch(batch);
        assert!(resp.success);
        assert_eq!(resp.last_acknowledged_sequence, 5);
        assert_eq!(receiver.highest_guest_seq(), 5);
        assert_eq!(receiver.processed_count(), 5);
    }

    #[test]
    fn guest_user_id_overwritten_by_run_owner() {
        let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
        store.create_user("alice", "alice").expect("user");
        store
            .persist_run_spec_with_owner(
                "run-owned",
                "{}",
                Some("sess-owned"),
                "urn:vam:agent:local.vam.dev/acme/tester",
                None,
                "User::\"alice\"",
                "alice",
            )
            .expect("run");

        let mut receiver = GuestStreamReceiver::new("sess-owned", "run-owned", None, Some(store));
        let mut attrs = HashMap::new();
        attrs.insert("vam.user.id".to_string(), "attacker".to_string());
        attrs.insert("tool.name".to_string(), "bash".to_string());
        let frame = LogFrame {
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
        let processed = receiver.process_frame(frame).unwrap().unwrap();
        assert_eq!(
            processed.attributes.get("vam.user.id").map(String::as_str),
            Some("alice")
        );
        assert_ne!(
            processed.attributes.get("vam.user.id").map(String::as_str),
            Some("attacker")
        );
    }
}
