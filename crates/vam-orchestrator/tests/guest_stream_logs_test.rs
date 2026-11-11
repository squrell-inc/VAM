use prost::Message;
use std::collections::HashMap;
use std::sync::Arc;

use vam_orchestrator::guest_stream::GuestStreamReceiver;
use vam_orchestrator::trajectory::{EVENT_TOOL_COMPLETED, EVENT_TOOL_FAILED};
use vam_orchestrator::workflow::SqliteWorkflowStore;
use vam_spec::v1::{LogFrame, StreamLogsResponse, TraceContext, VsockEnvelope};
use vam_storage::{ChunkStore, Drive9Layout};
use vam_telemetry::TelemetryCapture;

#[tokio::test]
async fn test_e2e_guest_stream_logs_pipeline() {
    let temp_dir = tempfile::tempdir().unwrap();
    let layout = Drive9Layout::new(temp_dir.path().join("drive9"));
    let chunk_store = ChunkStore::new(layout.clone());
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());

    let session_id = "session-e2e-101";
    let run_id = "run-e2e-101";

    let mut receiver = GuestStreamReceiver::new(
        session_id,
        run_id,
        Some(chunk_store.clone()),
        Some(store.clone()),
    );

    // 1. Emulate guest emitting a sequence of spans and logs
    let valid_traceparent = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
    let mut frames = Vec::new();

    // Frame 1: Init / mounts span
    let mut init_attrs = HashMap::new();
    init_attrs.insert("mount_point".to_string(), "/mnt/drive9".to_string());
    frames.push(LogFrame {
        instance_id: None,
        timestamp: None,
        channel: "span".to_string(),
        chunk: Vec::new(),
        sequence_number: 1,
        trace_context: Some(TraceContext {
            traceparent: valid_traceparent.to_string(),
            tracestate: "congo=t61rcWkgMzE".to_string(),
        }),
        duration_ns: 25_000_000,
        span_name: "init.mounts".to_string(),
        attributes: init_attrs,
        clock_domain: "guest_domain".to_string(),
        digest: String::new(),
    });

    // Frame 2: ReAct iteration span
    let mut react_attrs = HashMap::new();
    react_attrs.insert("iteration".to_string(), "1".to_string());
    frames.push(LogFrame {
        instance_id: None,
        timestamp: None,
        channel: "span".to_string(),
        chunk: Vec::new(),
        sequence_number: 2,
        trace_context: Some(TraceContext {
            traceparent: valid_traceparent.to_string(),
            tracestate: String::new(),
        }),
        duration_ns: 850_000_000,
        span_name: "agent.react_iteration".to_string(),
        attributes: react_attrs,
        clock_domain: "guest_domain".to_string(),
        digest: String::new(),
    });

    // Frame 3: Tool exec span with large stdout (> 4 KiB) to trigger CAS spill
    let large_stdout = "X".repeat(6000).into_bytes();
    let mut tool_attrs = HashMap::new();
    tool_attrs.insert("tool.name".to_string(), "grep".to_string());
    frames.push(LogFrame {
        instance_id: None,
        timestamp: None,
        channel: "stdout".to_string(),
        chunk: large_stdout.clone(),
        sequence_number: 3,
        trace_context: Some(TraceContext {
            traceparent: valid_traceparent.to_string(),
            tracestate: String::new(),
        }),
        duration_ns: 0,
        span_name: String::new(),
        attributes: HashMap::new(),
        clock_domain: "guest_domain".to_string(),
        digest: String::new(),
    });

    // Frame 4: Forged traceparent from hostile guest
    frames.push(LogFrame {
        instance_id: None,
        timestamp: None,
        channel: "span".to_string(),
        chunk: Vec::new(),
        sequence_number: 4,
        trace_context: Some(TraceContext {
            traceparent: "bad-trace-id-not-hex".to_string(),
            tracestate: String::new(),
        }),
        duration_ns: 10_000_000,
        span_name: "hostile.exec".to_string(),
        attributes: HashMap::new(),
        clock_domain: "guest_domain".to_string(),
        digest: String::new(),
    });

    // 2. Process batch through the host receiver
    let resp = receiver.process_batch(frames);

    // 3. Verify response acknowledgements
    assert!(resp.success);
    assert_eq!(resp.last_acknowledged_sequence, 4);
    assert_eq!(receiver.processed_count(), 4);

    // 4. Verify CAS spill occurred for Frame 3
    let chunk_files: Vec<_> = std::fs::read_dir(layout.chunks_dir())
        .map(|r| r.collect::<Result<Vec<_>, _>>().unwrap_or_default())
        .unwrap_or_default();
    assert!(
        !chunk_files.is_empty(),
        "Large payload must be written to CAS chunks dir"
    );

    // 5. Verify workflow outbox recorded events
    let events = store.read_outbox(0).unwrap();
    assert!(
        events.len() >= 3,
        "Outbox must record guest events for session"
    );
    assert!(events
        .iter()
        .any(|e| e.event_type.contains("vam.v1.guest.span.init.mounts")));
    assert!(events.iter().any(|e| e
        .event_type
        .contains("vam.v1.guest.span.agent.react_iteration")));
    assert!(events
        .iter()
        .any(|e| e.event_type.contains("vam.v1.guest.log.stdout")));
}

#[tokio::test]
async fn test_vsock_envelope_stream_logs_rpc() {
    let mut receiver = GuestStreamReceiver::new("session-rpc-1", "run-rpc-1", None, None);

    let frame = LogFrame {
        instance_id: None,
        timestamp: None,
        channel: "span".to_string(),
        chunk: Vec::new(),
        sequence_number: 42,
        trace_context: None,
        duration_ns: 15_000_000,
        span_name: "pep.authorize".to_string(),
        attributes: HashMap::new(),
        clock_domain: "guest_domain".to_string(),
        digest: String::new(),
    };

    let mut payload = Vec::new();
    frame.encode(&mut payload).unwrap();

    let env = VsockEnvelope {
        method: "StreamLogs".to_string(),
        correlation_id: 999,
        payload,
        error: None,
        trace_context: None,
    };

    let resp_env = receiver.handle_vsock_envelope(&env).unwrap().unwrap();
    assert_eq!(resp_env.method, "StreamLogsResponse");
    assert_eq!(resp_env.correlation_id, 999);

    let resp = StreamLogsResponse::decode(&*resp_env.payload).unwrap();
    assert!(resp.success);
    assert_eq!(resp.last_acknowledged_sequence, 42);
}

#[tokio::test]
async fn test_guest_stream_brokers_framing_acks_and_outbox() {
    use std::sync::Mutex;
    use vam_orchestrator::brokers::handle_stream_logs_frame;
    use vam_vsock::frame::{Frame, FrameType};

    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let receiver = Arc::new(Mutex::new(GuestStreamReceiver::new(
        "session-broker-1",
        "run-broker-1",
        None,
        Some(store.clone()),
    )));

    let frame = LogFrame {
        instance_id: None,
        timestamp: None,
        channel: "span".to_string(),
        chunk: Vec::new(),
        sequence_number: 7,
        trace_context: None,
        duration_ns: 1_000_000,
        span_name: "init.mounts".to_string(),
        attributes: HashMap::new(),
        clock_domain: "guest_domain".to_string(),
        digest: String::new(),
    };
    let mut payload = Vec::new();
    frame.encode(&mut payload).unwrap();
    let env = VsockEnvelope {
        method: "StreamLogs".to_string(),
        correlation_id: 4242,
        payload,
        error: None,
        trace_context: None,
    };

    let (mut client, mut server) = tokio::io::duplex(4096);
    let handled = handle_stream_logs_frame(&mut server, &env, &receiver).await;
    assert!(handled);

    let ack_frame = Frame::read_from(&mut client).await.unwrap();
    assert_eq!(ack_frame.frame_type, FrameType::ControlGrpc);
    let resp_env = VsockEnvelope::decode(&*ack_frame.payload).unwrap();
    assert_eq!(resp_env.method, "StreamLogsResponse");
    assert_eq!(resp_env.correlation_id, 4242);
    let resp = StreamLogsResponse::decode(&*resp_env.payload).unwrap();
    assert!(resp.success);
    assert_eq!(resp.last_acknowledged_sequence, 7);

    let events = store.read_outbox(0).unwrap();
    assert!(
        events
            .iter()
            .any(|e| e.event_type.contains("vam.v1.guest.span.init.mounts")),
        "StreamLogs must append observational outbox events"
    );
}

fn stdio_frame(seq: u64, channel: &str, body: &[u8], lease_id: &str, tool_name: &str) -> LogFrame {
    let mut attrs = HashMap::new();
    attrs.insert("tool.name".to_string(), tool_name.to_string());
    attrs.insert("lease.id".to_string(), lease_id.to_string());
    LogFrame {
        instance_id: None,
        timestamp: None,
        channel: channel.to_string(),
        chunk: body.to_vec(),
        sequence_number: seq,
        trace_context: None,
        duration_ns: 0,
        span_name: String::new(),
        attributes: attrs,
        clock_domain: "guest_domain".to_string(),
        digest: "guest-forged-digest".to_string(),
    }
}

fn tool_exec_span(seq: u64, exit_code: i32, lease_id: &str, tool_name: &str) -> LogFrame {
    let mut attrs = HashMap::new();
    attrs.insert("tool.name".to_string(), tool_name.to_string());
    attrs.insert("tool.exit_code".to_string(), exit_code.to_string());
    attrs.insert("lease.id".to_string(), lease_id.to_string());
    LogFrame {
        instance_id: None,
        timestamp: None,
        channel: "span".to_string(),
        chunk: Vec::new(),
        sequence_number: seq,
        trace_context: None,
        duration_ns: 42_000_000,
        span_name: "tool.exec".to_string(),
        attributes: attrs,
        clock_domain: "guest_domain".to_string(),
        digest: String::new(),
    }
}

#[tokio::test]
async fn test_guest_stream_failed_tool_exec_emits_catalog_event_on_run_id_with_cas() {
    let temp_dir = tempfile::tempdir().unwrap();
    let layout = Drive9Layout::new(temp_dir.path().join("drive9"));
    let chunk_store = ChunkStore::new(layout);
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());

    let session_id = "session-tool-fail";
    let run_id = "run-tool-fail";
    let lease_id = "lease-tool-fail-1";

    let mut receiver = GuestStreamReceiver::new(
        session_id,
        run_id,
        Some(chunk_store.clone()),
        Some(store.clone()),
    );

    let stderr = b"auth failed Bearer sk-supersecrettokenvalue0123456789 and sk-ant-api03-abcdefghijabcdefghijabcdefghij";
    let resp = receiver.process_batch(vec![
        stdio_frame(1, "stderr", stderr, lease_id, "run"),
        tool_exec_span(2, 2, lease_id, "run"),
    ]);
    assert!(resp.success);

    let run_events = store.read_outbox_for_run(run_id, 0).unwrap();
    assert!(
        run_events.iter().any(|e| e.event_type == EVENT_TOOL_FAILED),
        "failed tool.exec must appear on run_id, got {:?}",
        run_events
            .iter()
            .map(|e| e.event_type.as_str())
            .collect::<Vec<_>>()
    );
    assert!(
        !run_events
            .iter()
            .any(|e| e.event_type.contains("vam.v1.guest.span.tool.exec")),
        "must not emit duplicate guest.span.tool.exec"
    );

    let session_events = store.read_outbox_for_run(session_id, 0).unwrap();
    assert!(
        !session_events
            .iter()
            .any(|e| e.event_type == EVENT_TOOL_FAILED),
        "catalog event must not be keyed by session_id"
    );

    let failed = run_events
        .iter()
        .find(|e| e.event_type == EVENT_TOOL_FAILED)
        .expect("failed event");
    let payload: serde_json::Value = serde_json::from_str(&failed.payload).unwrap();
    assert_eq!(payload["tool_name"], "run");
    assert_eq!(payload["exit_code"], 2);
    assert_eq!(payload["duration_ns"], 42_000_000);
    assert_eq!(payload["lease_id"], lease_id);
    assert_eq!(payload["capture"], "digest");
    let digest = payload["stderr_digest"]
        .as_str()
        .expect("stderr_digest on digest capture");
    assert!(
        chunk_store.has_chunk(digest),
        "CAS must store redacted body"
    );

    let stored = chunk_store.get_chunk(digest).expect("read chunk");
    let stored_str = String::from_utf8_lossy(&stored);
    let payload_str = failed.payload.as_str();
    for secret in [
        "sk-supersecrettokenvalue0123456789",
        "Bearer sk-",
        "sk-ant-",
    ] {
        assert!(
            !stored_str.contains(secret),
            "CAS chunk must not contain secret {secret}"
        );
        assert!(
            !payload_str.contains(secret),
            "outbox payload must not contain secret {secret}"
        );
    }
}

#[tokio::test]
async fn test_guest_stream_capture_none_emits_event_without_cas() {
    let temp_dir = tempfile::tempdir().unwrap();
    let layout = Drive9Layout::new(temp_dir.path().join("drive9"));
    let chunk_store = ChunkStore::new(layout.clone());
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let run_id = "run-capture-none";

    let mut receiver = GuestStreamReceiver::new(
        "session-capture-none",
        run_id,
        Some(chunk_store.clone()),
        Some(store.clone()),
    )
    .with_capture(TelemetryCapture::None);

    let body = b"small stdout body";
    receiver.process_batch(vec![
        stdio_frame(1, "stdout", body, "lease-none", "echo"),
        tool_exec_span(2, 0, "lease-none", "echo"),
    ]);

    let events = store.read_outbox_for_run(run_id, 0).unwrap();
    let completed = events
        .iter()
        .find(|e| e.event_type == EVENT_TOOL_COMPLETED)
        .expect("completed event");
    let payload: serde_json::Value = serde_json::from_str(&completed.payload).unwrap();
    assert_eq!(payload["tool_name"], "echo");
    assert_eq!(payload["exit_code"], 0);
    assert_eq!(payload["capture"], "none");
    assert!(payload.get("stdout_digest").is_none());
    assert!(payload.get("stderr_digest").is_none());

    let chunk_files: Vec<_> = std::fs::read_dir(layout.chunks_dir())
        .map(|r| r.collect::<Result<Vec<_>, _>>().unwrap_or_default())
        .unwrap_or_default();
    assert!(
        chunk_files.is_empty(),
        "capture=none must not write CAS chunks"
    );
}

#[tokio::test]
async fn test_guest_stream_capture_redacted_full_includes_preview() {
    let temp_dir = tempfile::tempdir().unwrap();
    let layout = Drive9Layout::new(temp_dir.path().join("drive9"));
    let chunk_store = ChunkStore::new(layout);
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let run_id = "run-redacted-full";

    let mut receiver = GuestStreamReceiver::new(
        "session-redacted-full",
        run_id,
        Some(chunk_store.clone()),
        Some(store.clone()),
    )
    .with_capture(TelemetryCapture::RedactedFull);

    receiver.process_batch(vec![
        stdio_frame(1, "stdout", b"visible-output", "lease-full", "cat"),
        tool_exec_span(2, 0, "lease-full", "cat"),
    ]);

    let events = store.read_outbox_for_run(run_id, 0).unwrap();
    let completed = events
        .iter()
        .find(|e| e.event_type == EVENT_TOOL_COMPLETED)
        .expect("completed event");
    let payload: serde_json::Value = serde_json::from_str(&completed.payload).unwrap();
    assert_eq!(payload["capture"], "redacted-full");
    assert_eq!(payload["stdout_preview"], "visible-output");
    let digest = payload["stdout_digest"].as_str().expect("digest");
    assert!(chunk_store.has_chunk(digest));
}
