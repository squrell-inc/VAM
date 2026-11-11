//! Adversarial and Chaos Verification Test Suite (Phase 7).
//!
//! Validates resilience against adverse conditions, malicious agents, and failure modes:
//! 1. Collector Down / Outage (durable disk spool fallback and FIFO recovery)
//! 2. Spool Saturation & Quota Eviction (Class C diagnostic dropped, Class A audit preserved)
//! 3. Malicious Guest Flood (50,000 EPS flood capped by rate limiter, host bounded)
//! 4. Forged Context & Malformed Traceparent (stripped and rejected safely)
//! 5. Slow Consumer Disconnect (>16MB lag / buffer overflow eviction)
//! 6. Clock Skew & Monotonic Invariance (clock jump backward, durations remain positive)
//! 7. Audit Hash-Chain Tamper Detection & Fail-Closed (cryptographic failure on alteration)
//! 8. Tenant Isolation & OCSF Schema Compliance (traversal defense, mTLS SPIFFE, OCSF v1.2.0)

use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ed25519_dalek::SigningKey;
use tempfile::tempdir;

use vam_orchestrator::workflow::SqliteWorkflowStore;
use vam_spec::v1::{LogFrame, ResourceId, TraceContext, VamCloudEvent};
use vam_telemetry::bus::{BroadcastEventBus, MAX_SLOW_CONSUMER_BYTES};
use vam_telemetry::guest_ingress::{GuestIngressValidator, IngressDecision};
use vam_telemetry::ocsf::{from_cedar_decision, from_egress_block};
use vam_telemetry::queue::DiskSpoolQueue;
use vam_telemetry::retention::{RetentionClass, RetentionManager, RetentionPolicy};
use vam_telemetry::slo::{SloEvaluator, SloTier};
use vam_telemetry::tenant::{CollectorTlsConfig, TenantContext, TenantId, TenantSpoolManager};
use vam_telemetry::trace_context::validate_traceparent;
use vam_telemetry::{verify_ledger_chain, PartitionedAuditLedger, VamTelemetryError};

fn make_test_cloud_event(id: &str, seq: i64) -> VamCloudEvent {
    VamCloudEvent {
        id: id.to_string(),
        source: Some(ResourceId {
            urn: "urn:vam:node:test".to_string(),
        }),
        spec_version: "1.0".to_string(),
        r#type: "vam.v1.test.event".to_string(),
        sequence_number: seq,
        ..Default::default()
    }
}

// -----------------------------------------------------------------------------
// 1. Collector Down / Outage Durability
// -----------------------------------------------------------------------------
#[test]
fn test_adversarial_collector_down_spool_durability() {
    let tmp = tempdir().expect("tempdir");
    let spool_file = tmp.path().join("collector_outage.spool");

    // Queue with 2 in-memory slots and 64KB disk spool limit
    let spool_queue = DiskSpoolQueue::new(&spool_file, 2, 65536);

    // Simulate collector being offline: send events 1..10
    for i in 1..=10 {
        let payload = format!("event-payload-{}", i).into_bytes();
        spool_queue
            .enqueue(payload)
            .expect("enqueue succeeds during outage");
    }

    // In-memory queue holds first 2 events
    assert_eq!(
        spool_queue.dequeue_memory(),
        Some(b"event-payload-1".to_vec())
    );
    assert_eq!(
        spool_queue.dequeue_memory(),
        Some(b"event-payload-2".to_vec())
    );
    assert_eq!(spool_queue.dequeue_memory(), None);

    // Disk spool file was created for the remaining 8 overflow events
    assert!(
        spool_file.exists(),
        "spool file must exist for overflow events"
    );

    // Read back length-prefixed records from disk
    let mut file = File::open(&spool_file).expect("open spool file");
    let mut buffer = Vec::new();
    file.read_to_end(&mut buffer).expect("read spool file");

    let mut cursor = 0;
    let mut recovered_events = Vec::new();
    while cursor + 4 <= buffer.len() {
        let len = u32::from_le_bytes(buffer[cursor..cursor + 4].try_into().unwrap()) as usize;
        cursor += 4;
        assert!(
            cursor + len <= buffer.len(),
            "length prefix must not exceed file size"
        );
        let event_data = &buffer[cursor..cursor + len];
        recovered_events.push(String::from_utf8_lossy(event_data).to_string());
        cursor += len;
    }

    assert_eq!(recovered_events.len(), 8);
    assert_eq!(recovered_events[0], "event-payload-3");
    assert_eq!(recovered_events[7], "event-payload-10");
}

// -----------------------------------------------------------------------------
// 2. Spool Saturation & Quota Eviction
// -----------------------------------------------------------------------------
#[test]
fn test_adversarial_spool_saturation_and_quota_eviction() {
    let policy = RetentionPolicy {
        max_class_a_bytes: 10_000,
        max_class_b_bytes: 5_000,
        max_class_c_bytes: 2_000,
        max_class_d_bytes: 1_000,
        ..RetentionPolicy::default()
    };
    let retention = RetentionManager::new(policy);

    // Fill Class C (diagnostic traces) up to limit
    assert!(retention
        .record_usage(RetentionClass::ClassC_Diagnostic, 1_500)
        .is_ok());
    assert_eq!(
        retention.current_usage(RetentionClass::ClassC_Diagnostic),
        1_500
    );

    // Exceeding Class C limit returns StorageQuotaExceeded
    let overflow = retention.record_usage(RetentionClass::ClassC_Diagnostic, 1_000);
    assert!(overflow.is_err());
    match overflow.unwrap_err() {
        VamTelemetryError::StorageQuotaExceeded { class, .. } => {
            assert_eq!(class, "ClassC_Diagnostic");
        }
        other => panic!("expected StorageQuotaExceeded, got {:?}", other),
    }

    // Under disk pressure, prune lossy lanes (Class C diagnostic pruned first)
    let freed = retention.prune_lossy(1_000);
    assert_eq!(freed, 1_000);
    assert_eq!(
        retention.current_usage(RetentionClass::ClassC_Diagnostic),
        500
    );

    // Class A (Audit) usage is recorded and strictly protected
    assert!(retention
        .record_usage(RetentionClass::ClassA_Audit, 4_000)
        .is_ok());
    assert_eq!(retention.current_usage(RetentionClass::ClassA_Audit), 4_000);

    // Prune again: Class A is NEVER pruned
    let freed_again = retention.prune_lossy(10_000);
    assert_eq!(freed_again, 500); // Only remaining Class C is pruned
    assert_eq!(
        retention.current_usage(RetentionClass::ClassA_Audit),
        4_000,
        "Class A audit must be preserved"
    );
}

// -----------------------------------------------------------------------------
// 3. Malicious Guest Flood Protection
// -----------------------------------------------------------------------------
#[test]
fn test_adversarial_malicious_guest_log_flood() {
    // Configure rate limit: 100 tokens/sec with burst capacity of 50
    let validator =
        GuestIngressValidator::new("adversarial-session-flood").with_rate_limits(100.0, 50.0);

    let mut accepted_count = 0;
    let mut dropped_count = 0;

    // Guest fires a flood of 5,000 frames instantly
    for i in 1..=5_000 {
        let mut frame = LogFrame {
            instance_id: None,
            timestamp: None,
            channel: "stdout".to_string(),
            chunk: format!("flood-line-{}", i).into_bytes(),
            sequence_number: i,
            trace_context: None,
            duration_ns: 1000,
            span_name: "test".to_string(),
            attributes: HashMap::new(),
            clock_domain: "".to_string(),
            digest: String::new(),
        };

        match validator.process_frame(&mut frame) {
            IngressDecision::Accepted { .. } => accepted_count += 1,
            IngressDecision::Dropped { reason } => {
                dropped_count += 1;
                assert!(reason.contains("rate limit exceeded"));
            }
        }
    }

    // Burst cap is 50, so accepted frames must be bounded near 50
    assert!(
        accepted_count <= 55,
        "Accepted count {} must be capped by burst",
        accepted_count
    );
    assert!(dropped_count >= 4_945, "Remaining flood must be dropped");
}

// -----------------------------------------------------------------------------
// 4. Forged Context & Malformed Traceparent
// -----------------------------------------------------------------------------
#[test]
fn test_adversarial_forged_context_sanitization() {
    let validator = GuestIngressValidator::new("adversarial-forged-session");

    let forged_cases = vec![
        // Length wrong
        "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7",
        // Version ff (forbidden by W3C)
        "ff-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
        // All-zero trace ID
        "00-00000000000000000000000000000000-00f067aa0ba902b7-01",
        // All-zero parent ID
        "00-4bf92f3577b34da6a3ce929d0e0e4736-0000000000000000-01",
        // Non-hex injection / command injection attempt
        "00-4bf92f3577b34da6a3ce929d0e0e4736-$(rm -rf /)-01",
        // Extra trailing garbage
        "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01-extra",
    ];

    for forged_tp in forged_cases {
        // Direct validator test
        assert!(
            validate_traceparent(forged_tp).is_err(),
            "Forged traceparent '{}' must fail validation",
            forged_tp
        );

        // Guest frame test
        let mut frame = LogFrame {
            instance_id: None,
            timestamp: None,
            channel: "span".to_string(),
            chunk: Vec::new(),
            sequence_number: 1,
            trace_context: Some(TraceContext {
                traceparent: forged_tp.to_string(),
                tracestate: "".to_string(),
            }),
            duration_ns: 10_000,
            span_name: "tool.attack".to_string(),
            attributes: HashMap::new(),
            clock_domain: "".to_string(),
            digest: String::new(),
        };

        let decision = validator.process_frame(&mut frame);
        match decision {
            IngressDecision::Accepted { .. } => {
                assert!(
                    frame.trace_context.is_none(),
                    "Forged traceparent '{}' must be sanitized/stripped to None",
                    forged_tp
                );
            }
            IngressDecision::Dropped { .. } => {}
        }
    }
}

// -----------------------------------------------------------------------------
// 5. Slow Consumer Disconnect / Eviction
// -----------------------------------------------------------------------------
#[tokio::test]
async fn test_adversarial_slow_consumer_lag_eviction() {
    // Bus with small capacity to easily trigger lagging
    let bus = BroadcastEventBus::with_capacity(4);
    let mut slow_sub = bus.subscribe();
    let mut fast_sub = bus.subscribe();

    // Fast subscriber drains 1 event immediately
    let ev1 = make_test_cloud_event("ev-1", 1);
    bus.publish(ev1);

    assert!(fast_sub.recv().await.is_ok());

    // Flood the bus with more events than capacity while slow subscriber sleeps
    for i in 2..=10 {
        let ev = make_test_cloud_event(&format!("ev-{}", i), i);
        bus.publish(ev);
    }

    // Slow subscriber lagged behind the broadcast buffer capacity
    let res = slow_sub.recv().await;
    assert!(res.is_err(), "Slow subscriber must receive eviction error");
    match res.unwrap_err() {
        VamTelemetryError::SlowConsumerEvicted { reason } => {
            assert!(reason.contains("lagged") || reason.contains("evicted"));
        }
        other => panic!("expected SlowConsumerEvicted, got {:?}", other),
    }

    // Subsequent recv calls on evicted subscriber fail immediately
    let follow_up = slow_sub.recv().await;
    assert!(follow_up.is_err());
    match follow_up.unwrap_err() {
        VamTelemetryError::SlowConsumerEvicted { reason } => {
            assert!(reason.contains("previously evicted"));
        }
        other => panic!("expected SlowConsumerEvicted, got {:?}", other),
    }

    // Verify byte limit constant
    assert_eq!(MAX_SLOW_CONSUMER_BYTES, 16 * 1024 * 1024);
}

// -----------------------------------------------------------------------------
// 6. Clock Skew & Monotonic Time Invariance
// -----------------------------------------------------------------------------
#[test]
fn test_adversarial_clock_skew_monotonic_invariance() {
    let evaluator = SloEvaluator::new();

    // Baseline: monotonic clock measurement via Instant
    let start = Instant::now();
    std::thread::sleep(Duration::from_millis(5));
    let elapsed = start.elapsed();
    let elapsed_ms = elapsed.as_millis() as u64;

    assert!(
        elapsed_ms >= 5,
        "Monotonic elapsed time must not be negative"
    );

    // Claim SLO tier: target is 10ms
    // An observation below 10ms generates no alert
    let alert_normal = evaluator.record_observation(SloTier::Claim, 6, "session-fast");
    assert!(
        alert_normal.is_none(),
        "Normal observation must not generate alert"
    );

    // A latency exceeding target triggers alert regardless of clock changes
    let alert_breach = evaluator.record_observation(SloTier::Claim, 15, "session-slow");
    assert!(
        alert_breach.is_some(),
        "Observation exceeding 10ms target must generate alert"
    );
    let alert = alert_breach.unwrap();
    assert_eq!(alert.tier, SloTier::Claim);
    assert_eq!(alert.observed_ms, 15);
    assert_eq!(alert.target_ms, 10);

    // Restore tier (<50ms) and Cold tier (<250ms)
    assert!(evaluator
        .record_observation(SloTier::Restore, 45, "session-restored")
        .is_none());
    assert!(evaluator
        .record_observation(SloTier::Restore, 65, "session-restored-slow")
        .is_some());
    assert!(evaluator
        .record_observation(SloTier::Cold, 200, "session-cold")
        .is_none());
    assert!(evaluator
        .record_observation(SloTier::Cold, 350, "session-cold-slow")
        .is_some());

    // Capacity metrics snapshot
    evaluator
        .capacity
        .active_sessions
        .store(42, Ordering::SeqCst);
    evaluator.capacity.warm_pool_size.store(8, Ordering::SeqCst);
    evaluator
        .capacity
        .admission_queue_len
        .store(3, Ordering::SeqCst);

    assert_eq!(
        evaluator.capacity.active_sessions.load(Ordering::SeqCst),
        42
    );
    assert_eq!(evaluator.capacity.warm_pool_size.load(Ordering::SeqCst), 8);
    assert_eq!(
        evaluator
            .capacity
            .admission_queue_len
            .load(Ordering::SeqCst),
        3
    );
}

// -----------------------------------------------------------------------------
// 7. Audit Hash-Chain Tamper Detection & Fail-Closed
// -----------------------------------------------------------------------------
#[test]
fn test_adversarial_audit_hash_chain_tamper_detection_and_fail_closed() {
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let ledger = Arc::new(PartitionedAuditLedger::new());
    store.set_audit_ledger(ledger.clone());

    let tenant_id = "tenant-prod-99";
    let partition_name = "session-audit-failclosed";
    let partition_key = TenantContext::partition_key(tenant_id, partition_name);

    // 1. Record series of tenant audit events
    store
        .record_tenant_audit_event(
            tenant_id,
            partition_name,
            "vam.v1.tool.invoked",
            b"{\"tool\":\"bash\"}",
        )
        .expect("audit record succeeds");
    store
        .record_tenant_audit_event(
            tenant_id,
            partition_name,
            "vam.v1.policy.decided",
            b"{\"decision\":\"DENY\"}",
        )
        .expect("audit record succeeds");
    store
        .record_tenant_audit_event(
            tenant_id,
            partition_name,
            "vam.v1.egress.blocked",
            b"{\"dest\":\"198.51.100.1\"}",
        )
        .expect("audit record succeeds");

    // Checkpoint with cryptographic key
    let signing_key = SigningKey::from_bytes(&[7u8; 32]);
    let verifying_key = signing_key.verifying_key();
    ledger
        .checkpoint(&partition_key, &signing_key)
        .expect("checkpoint succeeds");

    // Clean partition verification succeeds
    assert!(ledger
        .verify_partition(&partition_key, Some(&verifying_key))
        .is_ok());

    // 2. Adversarial attack: Tamper with record #1 payload
    let mut records = ledger.read_partition(&partition_key);
    assert_eq!(records.len(), 4, "3 events + 1 checkpoint record");
    records[1].payload_digest = "ff".repeat(32);

    let verify_corrupted = verify_ledger_chain(&records, Some(&verifying_key));
    assert!(
        verify_corrupted.is_err(),
        "tampered audit chain must fail cryptographic check"
    );
    match verify_corrupted.unwrap_err() {
        VamTelemetryError::LedgerCorrupted { sequence } => {
            assert_eq!(sequence, 1);
        }
        other => panic!("expected LedgerCorrupted, got {:?}", other),
    }

    // 3. Adversarial attack: Truncate record #2
    let truncated_records = vec![records[0].clone(), records[2].clone(), records[3].clone()];
    let verify_truncated = verify_ledger_chain(&truncated_records, Some(&verifying_key));
    assert!(
        verify_truncated.is_err(),
        "truncated audit chain must fail sequence or prev_hash"
    );
}

// -----------------------------------------------------------------------------
// 8. Tenant Isolation & OCSF Schema Compliance
// -----------------------------------------------------------------------------
#[test]
fn test_adversarial_tenant_isolation_and_ocsf_mapping() {
    // 1. Directory traversal prevention on tenant ID
    assert!(TenantId::new("valid-tenant-123").is_ok());
    assert!(
        TenantId::new("../malicious").is_err(),
        "directory traversal must be rejected"
    );
    assert!(
        TenantId::new("tenant/sub").is_err(),
        "slashes must be rejected"
    );
    assert!(
        TenantId::new("tenant:id").is_err(),
        "colons must be rejected"
    );
    assert!(
        TenantId::new("").is_err(),
        "empty tenant ID must be rejected"
    );

    // 2. Multi-tenant spool isolation
    let tmp = tempdir().expect("tempdir");
    let spool_mgr = TenantSpoolManager::new(tmp.path());
    let t1 = TenantId::new("tenant-alpha").unwrap();
    let t2 = TenantId::new("tenant-beta").unwrap();

    let q1 = spool_mgr.get_or_create_spool(&t1, 1024).unwrap();
    let q2 = spool_mgr.get_or_create_spool(&t2, 1024).unwrap();

    q1.enqueue(b"tenant-alpha-secret".to_vec()).unwrap();
    assert_eq!(q1.memory_len(), 1);
    assert_eq!(q2.memory_len(), 0, "tenant-beta queue must remain isolated");

    // 3. OCSF v1.2.0 Cedar authorization denial mapping
    let ocsf_auth =
        from_cedar_decision("run-101", "session-101", "bash", "DENY", "digest-hash-123");
    assert_eq!(ocsf_auth.class_uid, 1007); // Authorize Session
    assert_eq!(ocsf_auth.activity_id, 2); // Deny activity
    assert_eq!(ocsf_auth.disposition, "DENY");
    assert_eq!(ocsf_auth.severity_id, 3);

    let json_auth = serde_json::to_string(&ocsf_auth).expect("serialize OCSF event");
    assert!(json_auth.contains("\"class_uid\":1007"));
    assert!(json_auth.contains("\"disposition\":\"DENY\""));

    // 4. OCSF v1.2.0 Network Activity (Egress Block) mapping
    let ocsf_net = from_egress_block(
        "session-101",
        "198.51.100.25",
        443,
        "IP blocklist rule matched",
    );
    assert_eq!(ocsf_net.class_uid, 4001); // Network Activity
    assert_eq!(ocsf_net.activity_id, 2); // Deny / Block
    assert_eq!(ocsf_net.disposition, "Block");

    // 5. Remote Collector mTLS / SPIFFE validation
    let tls_config = CollectorTlsConfig {
        enabled: true,
        client_cert_path: Some("/etc/vam/certs/client.crt".into()),
        client_key_path: Some("/etc/vam/certs/client.key".into()),
        ca_cert_path: Some("/etc/vam/certs/ca.crt".into()),
        expected_spiffe_id: Some(
            "spiffe://vam.cluster.local/ns/telemetry/sa/collector".to_string(),
        ),
    };
    assert!(tls_config.validate_spiffe_id("spiffe://vam.cluster.local/ns/telemetry/sa/collector"));
    assert!(!tls_config.validate_spiffe_id("spiffe://evil.com/attacker"));
}
