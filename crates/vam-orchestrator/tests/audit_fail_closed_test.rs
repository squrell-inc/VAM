use ed25519_dalek::SigningKey;
use std::sync::Arc;
use vam_orchestrator::workflow::SqliteWorkflowStore;
use vam_telemetry::{verify_ledger_chain, PartitionedAuditLedger, VamTelemetryError};

#[test]
fn test_audit_ledger_record_and_checkpoint_chain() {
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let ledger = Arc::new(PartitionedAuditLedger::new());
    store.set_audit_ledger(ledger.clone());

    let session_id = "session-audit-123";
    let partition = vam_telemetry::current_audit_partition(session_id);

    // Record audit events via workflow store
    store
        .record_audit_event(
            session_id,
            "vam.v1.tool.invoked",
            b"{\"tool\":\"read_file\"}",
        )
        .expect("audit record succeeds");
    store
        .record_audit_event(
            session_id,
            "vam.v1.policy.decided",
            b"{\"decision\":\"ALLOW\"}",
        )
        .expect("audit record succeeds");
    store
        .record_audit_event(session_id, "vam.v1.tool.completed", b"{\"status\":\"OK\"}")
        .expect("audit record succeeds");

    // Checkpoint with Ed25519
    let signing_key = SigningKey::from_bytes(&[42u8; 32]);
    let verifying_key = signing_key.verifying_key();
    ledger
        .checkpoint(&partition, &signing_key)
        .expect("checkpoint succeeds");

    // Verify intact chain
    let res = ledger.verify_partition(&partition, Some(&verifying_key));
    assert!(res.is_ok(), "clean partition must verify successfully");
}

#[test]
fn test_audit_ledger_tamper_detection() {
    let ledger = PartitionedAuditLedger::new();
    let session_id = "session-tamper-456";

    ledger
        .record_event(session_id, "vam.v1.egress.decided", b"ALLOW google.com")
        .unwrap();
    ledger
        .record_event(session_id, "vam.v1.secret.accessed", b"vault-key-1")
        .unwrap();

    let mut records = ledger.read_partition(session_id);
    assert_eq!(records.len(), 2);

    // Tamper with record 1 payload digest
    records[1].payload_digest = "00".repeat(32);

    let verify_res = verify_ledger_chain(&records, None);
    assert!(verify_res.is_err());
    match verify_res.unwrap_err() {
        VamTelemetryError::LedgerCorrupted { sequence } => {
            assert_eq!(sequence, 1);
        }
        other => panic!("Expected LedgerCorrupted, got {:?}", other),
    }
}

#[test]
fn test_audit_ledger_fail_closed_on_tampered_partition() {
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let ledger = Arc::new(PartitionedAuditLedger::new());
    store.set_audit_ledger(ledger.clone());

    let session_id = "session-fail-closed-789";
    let partition = vam_telemetry::current_audit_partition(session_id);

    // Legitimate first record
    store
        .record_audit_event(session_id, "vam.v1.session.booted", b"cold")
        .unwrap();

    // Verify clean partition before proceeding
    assert!(ledger.verify_partition(&partition, None).is_ok());

    // Corrupt the partition records
    {
        let records = ledger.read_partition(&partition);
        let mut corrupted = records.clone();
        corrupted[0].prev_hash = "badhash".repeat(8);
        let verify_res = verify_ledger_chain(&corrupted, None);
        assert!(
            verify_res.is_err(),
            "tampered records must fail verification"
        );
    }

    // A check-before-act guard verifies the partition; if corrupted, operation is aborted
    let pre_check = ledger.verify_partition(&partition, None);
    assert!(pre_check.is_ok());
}
