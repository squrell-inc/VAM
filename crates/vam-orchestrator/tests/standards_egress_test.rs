//! Hermetic Phase 4 standards-egress tests (no OpenRouter / Firecracker).

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use ed25519_dalek::SigningKey;
use tempfile::tempdir;
use vam_orchestrator::egress::{
    drain_outbox_wal, emit_ocsf, install_standards_egress, load_wal_cursor, project_outbox_row,
    EgressConfig, StandardsEgress,
};
use vam_orchestrator::error::OutboxEvent;
use vam_orchestrator::workflow::SqliteWorkflowStore;
use vam_security::ApprovalSigner;
use vam_telemetry::attestation::{CostSummary, RunOutcomeAttestation};
use vam_telemetry::lineage::{build_drive9_dataset, OpenLineageRunEvent};
use vam_telemetry::ocsf::from_cedar_decision;
use vam_telemetry::{
    unwrap_attestation_dsse, wrap_attestation_dsse, write_lineage_jsonl, CLOUDEVENTS_CONTENT_TYPE,
    CLOUDEVENTS_JSONL, FOCUS_JSONL, LINEAGE_JSONL, OCSF_JSONL,
};

fn mock_http_server(statuses: Vec<u16>) -> (String, thread::JoinHandle<Vec<Vec<u8>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock webhook");
    let addr = listener.local_addr().expect("local addr");
    let handle = thread::spawn(move || {
        let mut bodies = Vec::new();
        for status in statuses {
            let (mut sock, _) = listener.accept().expect("accept");
            sock.set_read_timeout(Some(Duration::from_secs(2)))
                .expect("read timeout");
            let mut buf = vec![0u8; 16 * 1024];
            let n = sock.read(&mut buf).unwrap_or(0);
            bodies.push(buf[..n].to_vec());
            let resp =
                format!("HTTP/1.1 {status} X\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            let _ = sock.write_all(resp.as_bytes());
        }
        bodies
    });
    (format!("http://{addr}/"), handle)
}

fn egress_for(dir: &std::path::Path, url: &str) -> StandardsEgress {
    StandardsEgress::new(EgressConfig {
        cloudevents_url: url.to_string(),
        spool_dir: dir.to_path_buf(),
        mirror_otlp: false,
        otlp_enabled: false,
        openlineage_url: String::new(),
    })
}

#[test]
fn cloudevents_http_500_then_200_spools_then_drains() {
    let dir = tempdir().expect("tempdir");
    let (url, server) = mock_http_server(vec![500, 200]);
    let store = SqliteWorkflowStore::in_memory().expect("store");
    store
        .append_outbox_event("urn:vam:run:r-egress", "vam.v1.run.created", "created")
        .expect("append");
    let before = store.read_outbox(0).expect("read");
    assert_eq!(before.len(), 1);
    let original_payload = before[0].payload.clone();

    let egress = egress_for(dir.path(), &url);
    let mut cursor = 0;
    drain_outbox_wal(&store, &egress, &mut cursor);
    assert_eq!(cursor, before[0].sequence);

    // First POST was 500 → body is on the disk spool.
    assert!(
        dir.path().join("cloudevents.spool").exists(),
        "failed HTTP must enqueue the disk spool"
    );

    egress.drain_spools();
    let bodies = server.join().expect("mock thread");
    assert_eq!(bodies.len(), 2);

    let first = String::from_utf8_lossy(&bodies[0]);
    assert!(
        first.contains(CLOUDEVENTS_CONTENT_TYPE) || first.contains("application/cloudevents+json")
    );
    let body_json = first
        .split("\r\n\r\n")
        .nth(1)
        .unwrap_or("")
        .trim_matches('\0');
    let parsed: serde_json::Value = serde_json::from_str(body_json.trim())
        .unwrap_or_else(|_| serde_json::from_slice(&bodies[0]).unwrap_or(serde_json::json!({})));
    if parsed.is_object() && parsed.get("specversion").is_some() {
        assert!(parsed["source"].is_string());
        assert!(parsed["subject"].is_string());
        assert!(parsed.get("dataschema").is_some());
        assert!(!parsed["source"].is_object());
    } else {
        // HTTP request framing: look at the raw body for string URIs.
        assert!(first.contains("\"specversion\":\"1.0\""));
        assert!(first.contains("\"source\":\"urn:vam:node:"));
        assert!(!first.contains("\"source\":{\"urn\""));
        assert!(first.contains("\"dataschema\":"));
    }

    let after = store.read_outbox(0).expect("read after");
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].payload, original_payload);
    assert_eq!(load_wal_cursor(dir.path()), cursor);
}

#[test]
fn files_only_writes_cloudevents_jsonl() {
    let dir = tempdir().expect("tempdir");
    let store = SqliteWorkflowStore::in_memory().expect("store");
    store
        .append_outbox_event(
            "urn:vam:run:r-files",
            "vam.v1.session.booted",
            r#"{"boot_path":"cold"}"#,
        )
        .expect("append");
    let egress = egress_for(dir.path(), "");
    let mut cursor = 0;
    drain_outbox_wal(&store, &egress, &mut cursor);
    let line = std::fs::read_to_string(dir.path().join(CLOUDEVENTS_JSONL)).expect("jsonl");
    let value: serde_json::Value = serde_json::from_str(line.trim()).expect("parse");
    assert_eq!(value["specversion"], "1.0");
    assert!(value["source"].is_string());
    assert!(value["subject"].is_string());
    assert!(value["dataschema"]
        .as_str()
        .unwrap()
        .contains("session.booted"));
}

#[test]
fn ocsf_jsonl_on_deny_fixture() {
    let dir = tempdir().expect("tempdir");
    let egress = Arc::new(egress_for(dir.path(), ""));
    install_standards_egress(egress);
    let event = from_cedar_decision("run-deny", "sess-deny", "bash", "DENY", "digest-9");
    emit_ocsf(&event);
    let line = std::fs::read_to_string(dir.path().join(OCSF_JSONL)).expect("ocsf");
    assert!(line.contains("\"class_uid\":1007"));
    assert!(line.contains("\"disposition\":\"DENY\""));
    assert!(line.contains("run-deny"));
}

#[test]
fn focus_dsse_and_lineage_files() {
    let dir = tempdir().expect("tempdir");
    let egress = egress_for(dir.path(), "");

    let cost = CostSummary {
        currency: "USD".to_string(),
        spend_micro: 12_500,
        prompt_tokens: 100,
        completion_tokens: 20,
        total_tokens: 120,
        pricing_catalog_version: Some("pricing-v1".to_string()),
    };
    egress
        .write_focus(
            "urn:vam:run:cost-1",
            &cost,
            "2026-09-10T00:00:00.000Z",
            "2026-09-10T00:01:00.000Z",
        )
        .expect("focus");
    let focus = std::fs::read_to_string(dir.path().join(FOCUS_JSONL)).expect("focus file");
    assert!(focus.contains("\"BilledCost\":\"0.012500\""));
    assert!(focus.contains("\"BillingCurrency\":\"USD\""));
    assert!(focus.contains("pricing-v1"));

    let signer = ApprovalSigner::new("host-key-e2e", SigningKey::from_bytes(&[11u8; 32]));
    let attestation = RunOutcomeAttestation {
        run_id: "urn:vam:run:cost-1".to_string(),
        session_id: "sess-e2e".to_string(),
        goal: "phase4".to_string(),
        terminal_state: "SUCCEEDED".to_string(),
        terminator: "agent".to_string(),
        trajectory_digest: "deadbeef".to_string(),
        artifacts: vec![],
        paths: vec![],
        skills: vec![],
        tools: vec![],
        eval: None,
        cost: Some(cost.clone()),
        timestamp_unix_ms: 1_778_000_000_000,
        thread_log_head_seq: None,
        thread_log_head_digest: None,
        workspace: None,
    };
    let envelope = attestation.sign(&signer).expect("sign");
    let att_json = serde_json::to_vec(&attestation).expect("json");
    egress.write_dsse(&att_json, &envelope).expect("dsse");
    let dsse_line =
        std::fs::read_to_string(dir.path().join("attestations.jsonl")).expect("dsse file");
    let dsse_val: serde_json::Value = serde_json::from_str(dsse_line.trim()).expect("dsse json");
    assert_eq!(
        dsse_val["payloadType"],
        "application/vnd.vam.run-outcome+json"
    );
    let wrapped = wrap_attestation_dsse(&att_json, &envelope).expect("wrap");
    assert_eq!(
        unwrap_attestation_dsse(&wrapped).expect("unwrap"),
        attestation
    );

    let lineage = OpenLineageRunEvent::new(
        "COMPLETE",
        "2026-09-10T00:01:00.000Z",
        "urn:vam:run:cost-1",
        "vam.orchestrator",
        "run:urn:vam:run:cost-1",
        vec![build_drive9_dataset("ws-in", "man-1", None, None)],
        vec![build_drive9_dataset("ws-out", "man-2", None, None)],
    );
    let lineage_json = serde_json::to_string(&lineage).expect("lineage json");
    write_lineage_jsonl(dir.path(), &lineage_json).expect("lineage write");
    let lineage_line =
        std::fs::read_to_string(dir.path().join(LINEAGE_JSONL)).expect("lineage file");
    assert!(lineage_line.contains("schemaURL") || lineage_line.contains("schemaUrl"));
    assert!(lineage_line.contains("openlineage.io"));
}

#[test]
fn projection_outbox_row_is_cloudevents_json() {
    let row = OutboxEvent {
        sequence: 4,
        run_id: "urn:vam:run:proj".to_string(),
        event_type: "vam.v1.tool.completed".to_string(),
        payload: r#"{"tool_name":"run","exit_code":0}"#.to_string(),
    };
    let ev = project_outbox_row(&row);
    let json = serde_json::to_value(&ev).expect("json");
    assert_eq!(json["specversion"], "1.0");
    assert!(json["source"].is_string());
    assert!(json["subject"].is_string());
    assert!(json.get("dataschema").is_some());
    assert!(!json["source"].is_object());
}
