use bytes::Bytes;
use prost::Message;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use vam_spec::v1::{LogFrame, TraceContext, VsockEnvelope};
use vam_vsock::frame::{Frame, FrameType};
use vam_vsock::guest_dial;

/// Interval between host flush attempts (Decision: fail-open, bounded guest buffer).
pub const HOST_FLUSH_INTERVAL: Duration = Duration::from_millis(200);
/// Maximum frames drained per flush tick.
pub const HOST_FLUSH_BATCH: usize = 64;

/// Maximum number of events buffered in-memory before oldest are dropped (Decision T12/T30).
pub const MAX_BUFFER_CAPACITY: usize = 5000;

/// Default guest clock domain label (Decision in latency/trace-context).
pub const GUEST_CLOCK_DOMAIN: &str = "guest_domain";

/// Monotonic sequence counter and bounded circular buffer for guest telemetry.
pub struct GuestTelemetryBuffer {
    sequence_number: AtomicU64,
    queue: Mutex<VecDeque<LogFrame>>,
}

impl Default for GuestTelemetryBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl GuestTelemetryBuffer {
    pub fn new() -> Self {
        Self {
            sequence_number: AtomicU64::new(1),
            queue: Mutex::new(VecDeque::with_capacity(MAX_BUFFER_CAPACITY)),
        }
    }

    /// Allocates the next monotonic guest sequence number.
    pub fn next_sequence(&self) -> u64 {
        self.sequence_number.fetch_add(1, Ordering::SeqCst)
    }

    /// Emits a structured span frame with monotonic duration and clock domain.
    pub fn record_span(
        &self,
        span_name: &str,
        duration: Duration,
        trace_context: Option<TraceContext>,
        attributes: HashMap<String, String>,
    ) {
        let seq = self.next_sequence();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let timestamp = prost_types::Timestamp {
            seconds: now.as_secs() as i64,
            nanos: now.subsec_nanos() as i32,
        };

        let frame = LogFrame {
            instance_id: None,
            timestamp: Some(timestamp),
            channel: "span".to_string(),
            chunk: Vec::new(),
            sequence_number: seq,
            trace_context,
            duration_ns: duration.as_nanos() as u64,
            span_name: span_name.to_string(),
            attributes,
            clock_domain: GUEST_CLOCK_DOMAIN.to_string(),
            digest: String::new(),
        };

        self.push_frame(frame);
    }

    /// Emits a log output frame (stdout, stderr, system) correlated to trace context.
    pub fn record_log(&self, channel: &str, chunk: Vec<u8>, trace_context: Option<TraceContext>) {
        self.record_log_with_attrs(channel, chunk, trace_context, HashMap::new());
    }

    /// Emits a log frame with attributes (e.g. `tool.name`, `lease.id`) for host correlation.
    pub fn record_log_with_attrs(
        &self,
        channel: &str,
        chunk: Vec<u8>,
        trace_context: Option<TraceContext>,
        attributes: HashMap<String, String>,
    ) {
        let seq = self.next_sequence();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let timestamp = prost_types::Timestamp {
            seconds: now.as_secs() as i64,
            nanos: now.subsec_nanos() as i32,
        };

        let frame = LogFrame {
            instance_id: None,
            timestamp: Some(timestamp),
            channel: channel.to_string(),
            chunk,
            sequence_number: seq,
            trace_context,
            duration_ns: 0,
            span_name: String::new(),
            attributes,
            clock_domain: GUEST_CLOCK_DOMAIN.to_string(),
            digest: String::new(),
        };

        self.push_frame(frame);
    }

    /// Pushes a frame into the bounded circular buffer. Drops oldest if full.
    fn push_frame(&self, frame: LogFrame) {
        let mut q = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        if q.len() >= MAX_BUFFER_CAPACITY {
            q.pop_front();
        }
        q.push_back(frame);
    }

    /// Drains up to `max_count` frames from the front of the queue.
    pub fn drain_batch(&self, max_count: usize) -> Vec<LogFrame> {
        let mut q = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        let count = max_count.min(q.len());
        let mut batch = Vec::with_capacity(count);
        for _ in 0..count {
            if let Some(frame) = q.pop_front() {
                batch.push(frame);
            }
        }
        batch
    }

    /// Returns the current number of buffered frames.
    pub fn len(&self) -> usize {
        self.queue.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Re-queues drained frames at the front after a failed flush (oldest first).
    /// If the buffer is full, newest frames are dropped to preserve earlier sequence numbers.
    pub fn requeue_front(&self, frames: Vec<LogFrame>) {
        if frames.is_empty() {
            return;
        }
        let mut q = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        for frame in frames.into_iter().rev() {
            if q.len() >= MAX_BUFFER_CAPACITY {
                q.pop_back();
            }
            q.push_front(frame);
        }
    }
}

static GLOBAL_TELEMETRY: OnceLock<GuestTelemetryBuffer> = OnceLock::new();

/// Returns the global guest telemetry circular buffer.
pub fn global_telemetry() -> &'static GuestTelemetryBuffer {
    GLOBAL_TELEMETRY.get_or_init(GuestTelemetryBuffer::new)
}

/// Formats and packs a `LogFrame` into a `ControlGrpc` vsock frame.
pub fn frame_from_log(log_frame: &LogFrame, correlation_id: u32) -> Result<Frame, String> {
    let mut payload = Vec::new();
    log_frame.encode(&mut payload).map_err(|e| e.to_string())?;

    let envelope = VsockEnvelope {
        method: "StreamLogs".to_string(),
        correlation_id,
        payload,
        error: None,
        trace_context: log_frame.trace_context.clone(),
    };

    let mut env_payload = Vec::new();
    envelope
        .encode(&mut env_payload)
        .map_err(|e| e.to_string())?;

    Ok(Frame {
        frame_type: FrameType::ControlGrpc,
        correlation_id,
        payload: Bytes::from(env_payload),
    })
}

/// Drains a batch from `buffer` and writes `StreamLogs` frames on a long-lived stream.
/// On dial/write/read error, re-queues unacknowledged frames and returns Err (fail open).
pub async fn flush_batch_to_stream<S>(
    stream: &mut S,
    buffer: &GuestTelemetryBuffer,
    correlation_id: &mut u32,
) -> Result<usize, String>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let batch = buffer.drain_batch(HOST_FLUSH_BATCH);
    if batch.is_empty() {
        return Ok(0);
    }

    let mut sent = 0usize;
    for (i, log_frame) in batch.iter().enumerate() {
        *correlation_id = correlation_id.wrapping_add(1);
        let frame = match frame_from_log(log_frame, *correlation_id) {
            Ok(f) => f,
            Err(e) => {
                buffer.requeue_front(batch[i..].to_vec());
                return Err(e);
            }
        };
        if let Err(e) = frame.write_to(stream).await {
            buffer.requeue_front(batch[i..].to_vec());
            return Err(e.to_string());
        }
        match Frame::read_from(stream).await {
            Ok(_) => sent += 1,
            Err(e) => {
                buffer.requeue_front(batch[i..].to_vec());
                return Err(e.to_string());
            }
        }
    }
    Ok(sent)
}

/// Spawns the PID-1 host flush loop: one long-lived Port 52 vsock, reconnect with backoff.
pub fn start_host_flush_loop() {
    tokio::spawn(host_flush_loop());
}

async fn host_flush_loop() {
    let mut backoff = Duration::from_millis(50);
    let max_backoff = Duration::from_secs(2);
    let mut stream = None;
    let mut correlation_id: u32 = 2000;

    loop {
        tokio::time::sleep(HOST_FLUSH_INTERVAL).await;
        if global_telemetry().is_empty() {
            continue;
        }

        if stream.is_none() {
            match guest_dial(2, 52).await {
                Ok(s) => {
                    stream = Some(s);
                    backoff = Duration::from_millis(50);
                }
                Err(_) => {
                    tokio::time::sleep(backoff).await;
                    backoff = std::cmp::min(backoff * 2, max_backoff);
                    continue;
                }
            }
        }

        if let Some(ref mut s) = stream {
            if let Err(e) = flush_batch_to_stream(s, global_telemetry(), &mut correlation_id).await
            {
                eprintln!("[guest-agent] telemetry flush failed (fail-open): {e}");
                stream = None;
                tokio::time::sleep(backoff).await;
                backoff = std::cmp::min(backoff * 2, max_backoff);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_circular_buffer_bounds() {
        let buffer = GuestTelemetryBuffer::new();

        for i in 0..6000 {
            buffer.record_log("system", format!("msg {}", i).into_bytes(), None);
        }

        // Buffer must not exceed MAX_BUFFER_CAPACITY
        assert_eq!(buffer.len(), MAX_BUFFER_CAPACITY);

        // Sequence numbers must be strictly increasing
        let drained = buffer.drain_batch(10);
        assert_eq!(drained.len(), 10);
        // First 1000 were dropped, so drained[0] sequence_number is 1001
        assert_eq!(drained[0].sequence_number, 1001);
        assert_eq!(drained[9].sequence_number, 1010);
    }

    #[test]
    fn test_record_span_structure() {
        let buffer = GuestTelemetryBuffer::new();
        let mut attrs = HashMap::new();
        attrs.insert("iteration".to_string(), "1".to_string());

        buffer.record_span(
            "agent.react_iteration",
            Duration::from_millis(150),
            None,
            attrs,
        );

        let drained = buffer.drain_batch(1);
        assert_eq!(drained.len(), 1);
        let span = &drained[0];
        assert_eq!(span.span_name, "agent.react_iteration");
        assert_eq!(span.channel, "span");
        assert_eq!(span.duration_ns, 150_000_000);
        assert_eq!(span.clock_domain, GUEST_CLOCK_DOMAIN);
        assert_eq!(
            span.attributes.get("iteration").map(|s| s.as_str()),
            Some("1")
        );
    }

    #[test]
    fn test_frame_from_log_is_stream_logs_control_grpc() {
        let buffer = GuestTelemetryBuffer::new();
        buffer.record_log("system", b"hello".to_vec(), None);
        let drained = buffer.drain_batch(1);
        let frame = frame_from_log(&drained[0], 42).expect("encode");
        assert_eq!(frame.frame_type, FrameType::ControlGrpc);
        assert_eq!(frame.correlation_id, 42);
        let env = VsockEnvelope::decode(&*frame.payload).expect("envelope");
        assert_eq!(env.method, "StreamLogs");
        assert_eq!(env.correlation_id, 42);
        let log = LogFrame::decode(&*env.payload).expect("log");
        assert_eq!(log.channel, "system");
        assert_eq!(log.chunk, b"hello");
    }

    #[test]
    fn test_requeue_front_preserves_oldest_first() {
        let buffer = GuestTelemetryBuffer::new();
        buffer.record_log("system", b"a".to_vec(), None);
        buffer.record_log("system", b"b".to_vec(), None);
        let drained = buffer.drain_batch(2);
        assert!(buffer.is_empty());
        buffer.requeue_front(drained);
        let again = buffer.drain_batch(2);
        assert_eq!(again[0].chunk, b"a");
        assert_eq!(again[1].chunk, b"b");
    }

    #[tokio::test]
    async fn test_flush_batch_to_stream_roundtrip() {
        use vam_spec::v1::StreamLogsResponse;

        let (mut client, mut server) = tokio::io::duplex(8192);
        let buffer = GuestTelemetryBuffer::new();
        buffer.record_log("system", b"hello-flush".to_vec(), None);

        let server_task = tokio::spawn(async move {
            let frame = Frame::read_from(&mut server).await.expect("guest frame");
            assert_eq!(frame.frame_type, FrameType::ControlGrpc);
            let env = VsockEnvelope::decode(&*frame.payload).expect("env");
            assert_eq!(env.method, "StreamLogs");
            let log = LogFrame::decode(&*env.payload).expect("log");
            assert_eq!(log.chunk, b"hello-flush");

            let resp = StreamLogsResponse {
                success: true,
                last_acknowledged_sequence: log.sequence_number,
            };
            let mut resp_payload = Vec::new();
            resp.encode(&mut resp_payload).expect("resp encode");
            let env_out = VsockEnvelope {
                method: "StreamLogsResponse".to_string(),
                correlation_id: env.correlation_id,
                payload: resp_payload,
                error: None,
                trace_context: None,
            };
            let mut env_bytes = Vec::new();
            env_out.encode(&mut env_bytes).expect("env encode");
            let frame_out = Frame {
                frame_type: FrameType::ControlGrpc,
                correlation_id: env.correlation_id,
                payload: Bytes::from(env_bytes),
            };
            frame_out.write_to(&mut server).await.expect("ack write");
        });

        let mut corr = 1u32;
        let sent = flush_batch_to_stream(&mut client, &buffer, &mut corr)
            .await
            .expect("flush");
        assert_eq!(sent, 1);
        assert!(buffer.is_empty());
        server_task.await.expect("server");
    }

    #[tokio::test]
    async fn test_flush_batch_requeues_when_peer_closed() {
        let (mut client, server) = tokio::io::duplex(64);
        drop(server);
        let buffer = GuestTelemetryBuffer::new();
        buffer.record_log("system", b"keep-me".to_vec(), None);
        let mut corr = 1u32;
        let result = flush_batch_to_stream(&mut client, &buffer, &mut corr).await;
        assert!(result.is_err());
        assert_eq!(buffer.len(), 1);
        let restored = buffer.drain_batch(1);
        assert_eq!(restored[0].chunk, b"keep-me");
    }
}
