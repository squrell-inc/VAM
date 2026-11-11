//! Hermetic Phase 6 enterprise-control tests (no OpenRouter / Firecracker / Lima).

use std::sync::Arc;

use tempfile::tempdir;
use vam_orchestrator::egress::{EgressConfig, StandardsEgress};
use vam_orchestrator::evidence::{append_audit_jsonl, persist_audit_record_to_spool};
use vam_orchestrator::workflow::SqliteWorkflowStore;
use vam_telemetry::{
    current_audit_partition, report_slo_observation, CollectorTlsConfig, OtlpConfig,
    PartitionedAuditLedger, SloEvaluator, SloTier, TenantContext, TenantId, TenantSpoolManager,
    VamMetricsRegistry, METRIC_SLO_BREACHES_TOTAL, OTLP_DISABLED_REMOTE_REQUIRES_MTLS,
};

#[test]
fn tenant_egress_writes_isolated_dirs() {
    let root = tempdir().expect("tempdir");
    let a = TenantId::new("acme").unwrap();
    let b = TenantId::new("globex").unwrap();
    assert!(TenantId::new("../evil").is_err());

    let ctx_a = TenantContext::new(a.clone(), root.path());
    let ctx_b = TenantContext::new(b.clone(), root.path());
    let mgr = TenantSpoolManager::new(root.path());
    mgr.get_or_create_spool(&a, 1024 * 1024).unwrap();
    mgr.get_or_create_spool(&b, 1024 * 1024).unwrap();

    let egress_a = StandardsEgress::new(EgressConfig {
        cloudevents_url: String::new(),
        spool_dir: ctx_a.tenant_spool_dir(),
        mirror_otlp: false,
        otlp_enabled: false,
        openlineage_url: String::new(),
    });
    let egress_b = StandardsEgress::new(EgressConfig {
        cloudevents_url: String::new(),
        spool_dir: ctx_b.tenant_spool_dir(),
        mirror_otlp: false,
        otlp_enabled: false,
        openlineage_url: String::new(),
    });

    let ev = vam_telemetry::from_cedar_decision("run-a", "sess-a", "bash", "DENY", "digest");
    egress_a.write_ocsf(&ev).expect("ocsf a");
    egress_b
        .write_lineage_file("{\"eventType\":\"START\"}")
        .expect("lineage b");

    assert!(ctx_a.tenant_spool_dir().join("ocsf.jsonl").exists());
    assert!(!ctx_a.tenant_spool_dir().join("lineage.jsonl").exists());
    assert!(ctx_b.tenant_spool_dir().join("lineage.jsonl").exists());
    assert!(!ctx_b.tenant_spool_dir().join("ocsf.jsonl").exists());
}

#[test]
fn record_audit_event_uses_tenant_partition_and_audit_dir() {
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let ledger = Arc::new(PartitionedAuditLedger::new());
    store.set_audit_ledger(ledger.clone());

    let session_id = "sess-enterprise-1";
    store
        .record_audit_event(session_id, "vam.v1.session.booted", b"cold")
        .expect("audit");

    let partition = current_audit_partition(session_id);
    assert!(partition.starts_with("tenant:"));
    assert!(partition.ends_with(":sess-enterprise-1"));
    assert_eq!(ledger.read_partition(&partition).len(), 1);
    assert!(ledger.read_partition(session_id).is_empty());

    let dir = tempdir().expect("tempdir");
    let record = ledger
        .read_partition(&partition)
        .into_iter()
        .next()
        .unwrap();
    append_audit_jsonl(&dir.path().join("local"), &record).expect("persist");
    assert!(
        dir.path()
            .join("local")
            .join("audit")
            .join("tenant_local_sess-enterprise-1.jsonl")
            .exists()
            || dir.path().join("local/audit").read_dir().unwrap().any(|e| e
                .unwrap()
                .path()
                .extension()
                .and_then(|x| x.to_str())
                == Some("jsonl"))
    );
}

#[test]
fn record_audit_event_stamps_owner_user_id_not_partition() {
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let ledger = Arc::new(PartitionedAuditLedger::new());
    store.set_audit_ledger(ledger.clone());
    store.create_user("alice", "alice").expect("user");
    store
        .create_session_with_owner(
            "sess-alice",
            None,
            "vm-alice",
            "ws-alice",
            "default",
            "alice",
        )
        .expect("session");

    store
        .record_audit_event("sess-alice", "vam.v1.session.booted", b"cold")
        .expect("audit");

    let partition = current_audit_partition("sess-alice");
    assert_eq!(partition, format!("tenant:local:sess-alice"));
    let record = ledger
        .read_partition(&partition)
        .into_iter()
        .next()
        .expect("record");
    assert_eq!(record.user_id.as_deref(), Some("alice"));
    assert_eq!(record.partition_id, partition);
    assert!(!record.partition_id.contains("alice") || record.partition_id.ends_with(":sess-alice"));
    assert!(ledger.verify_partition(&partition, None).is_ok());
}

#[test]
fn persist_audit_jsonl_under_tenant_audit_dir() {
    let root = tempdir().expect("tempdir");
    let ctx = TenantContext::new(TenantId::new("acme").unwrap(), root.path());
    let ledger = PartitionedAuditLedger::new();
    let record = ledger
        .record_event("tenant:acme:sess-p", "vam.v1.session.booted", b"cold")
        .expect("record");
    append_audit_jsonl(&ctx.tenant_spool_dir(), &record).expect("persist");
    let audit_dir = ctx.tenant_audit_dir();
    assert!(
        audit_dir.exists(),
        "audit dir must be {{spool}}/{{tenant}}/audit"
    );
    let files: Vec<_> = std::fs::read_dir(&audit_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .collect();
    assert_eq!(files.len(), 1);
    let text = std::fs::read_to_string(files[0].path()).unwrap();
    assert!(text.contains("vam.v1.session.booted"));
    persist_audit_record_to_spool(&record);
}

#[test]
fn telemetry_remote_otlp_without_mtls_disabled() {
    let cfg = OtlpConfig {
        enabled: true,
        endpoint: "http://example.com:4318".to_string(),
        policy: Default::default(),
        tls: CollectorTlsConfig::default(),
        disabled_reason: None,
    }
    .resolve();
    assert!(!cfg.enabled);
    assert_eq!(
        cfg.disabled_reason.as_deref(),
        Some(OTLP_DISABLED_REMOTE_REQUIRES_MTLS)
    );
}

#[test]
fn slo_shared_evaluator_restore_counters_persist() {
    let evaluator = SloEvaluator::new();
    let registry = VamMetricsRegistry::new();
    assert!(evaluator
        .report_live(&registry, SloTier::Restore, 60, "s1")
        .is_some());
    assert!(evaluator
        .report_live(&registry, SloTier::Restore, 70, "s2")
        .is_some());
    assert_eq!(evaluator.total_breaches(SloTier::Restore), 2);
    assert_eq!(
        registry.get_counter(METRIC_SLO_BREACHES_TOTAL, &[("tier", "restore")]),
        Some(2)
    );
    let _ = report_slo_observation(SloTier::Cold, 10, "fast-cold");
}
