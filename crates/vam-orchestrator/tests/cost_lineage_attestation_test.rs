use ed25519_dalek::SigningKey;
use std::sync::Arc;
use tempfile::tempdir;

use vam_inference::budget::SessionBudget;
use vam_inference::catalog::ModelCatalog;
use vam_orchestrator::error::OutboxEvent;
use vam_orchestrator::trajectory::persist_trajectory_replay_blob;
use vam_orchestrator::workflow::SqliteWorkflowStore;
use vam_security::ApprovalSigner;
use vam_spec::v1::{Cost, EvaluationResult, ResourceId, ResourceRef};
use vam_storage::{ChunkStore, Drive9Layout};
use vam_telemetry::attestation::{CostSummary, EvaluationSummary, RunOutcomeAttestation};
use vam_telemetry::audit::PartitionedAuditLedger;
use vam_telemetry::bus::BroadcastEventBus;
use vam_telemetry::lineage::{
    build_drive9_dataset, build_knowledge_input_dataset, OpenLineageRunEvent,
};

#[test]
fn test_authoritative_evaluator_recording_event_bus_and_ledger() {
    let store = SqliteWorkflowStore::in_memory().expect("in-memory store should initialize");
    let bus = Arc::new(BroadcastEventBus::new());
    let mut rx = bus.subscribe();
    store.set_event_bus(bus);

    let ledger = Arc::new(PartitionedAuditLedger::new());
    store.set_audit_ledger(ledger.clone());

    let run_res_id = ResourceId {
        urn: "urn:vam:run:local.vam.dev/acme/eval-test-1".to_string(),
    };
    store
        .create_run(&run_res_id)
        .expect("run should be created");

    let eval_res = EvaluationResult {
        passed: true,
        evaluator_ref: Some(ResourceRef {
            resource_id: Some(ResourceId {
                urn: "urn:vam:evaluator:local.vam.dev/acme/accuracy-judge".to_string(),
            }),
            ..Default::default()
        }),
        assertion_id: Some(ResourceId {
            urn: "urn:vam:assertion:local.vam.dev/acme/correct-code".to_string(),
        }),
        confidence_score: 1.0,
        reasoning: "perfect execution".to_string(),
        details: None,
        evidence_artifacts: Vec::new(),
        authoritative: true,
    };

    store
        .record_evaluation(&run_res_id, &eval_res)
        .expect("evaluation should record");

    // 1. Check event bus publication
    let cloud_event = rx
        .try_recv()
        .expect("cloud event should be published to event bus");
    assert_eq!(cloud_event.r#type, "vam.v1.evaluation.recorded");
    assert_eq!(
        cloud_event.subject.as_ref().map(|s| s.urn.as_str()),
        Some("urn:vam:run:local.vam.dev/acme/eval-test-1")
    );

    // 2. Check query method
    let evals = store
        .get_evaluations_for_run("urn:vam:run:local.vam.dev/acme/eval-test-1")
        .expect("should get evals");
    assert_eq!(evals.len(), 1);
    assert_eq!(
        evals[0].0,
        "urn:vam:assertion:local.vam.dev/acme/correct-code"
    );
    assert_eq!(
        evals[0].1,
        "urn:vam:evaluator:local.vam.dev/acme/accuracy-judge"
    );
    assert!(evals[0].2);

    // 3. Check partitioned audit ledger
    let records = ledger.read_partition("vam_audit_evaluation");
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].partition_id, "vam_audit_evaluation");
    assert_eq!(records[0].event_type, "vam.v1.evaluation.recorded");
    assert_eq!(records[0].sequence, 0);
}

#[test]
fn test_signed_run_outcome_attestation_verification() {
    let key = SigningKey::from_bytes(&[7u8; 32]);
    let verifying_key = key.verifying_key();
    let signer = ApprovalSigner::new("host-key-sess-99".to_string(), key);

    let attestation = RunOutcomeAttestation {
        run_id: "urn:vam:run:run-phase6".to_string(),
        session_id: "sess-phase6".to_string(),
        goal: "Build autonomous test pipeline".to_string(),
        terminal_state: "SUCCEEDED".to_string(),
        terminator: "agent".to_string(),
        trajectory_digest: "a1b2c3d4e5f6".to_string(),
        artifacts: vec!["artifact-bundle-1".to_string()],
        paths: vec!["drive9://workspace/ws-1/manifests/man-1".to_string()],
        skills: vec!["skill:git-ops".to_string()],
        tools: vec!["tool:exec_cmd".to_string()],
        eval: Some(EvaluationSummary {
            assertion_id: "assert-success".to_string(),
            evaluator_id: "evaluator-qa".to_string(),
            passed: true,
        }),
        cost: Some(CostSummary {
            currency: "USD".to_string(),
            spend_micro: 12500,
            prompt_tokens: 1500,
            completion_tokens: 350,
            total_tokens: 1850,
            pricing_catalog_version: Some("pricing-v1".to_string()),
        }),
        timestamp_unix_ms: 1770000000000,
        thread_log_head_seq: None,
        thread_log_head_digest: None,
        workspace: None,
    };

    let signed_envelope = attestation
        .sign(&signer)
        .expect("attestation should sign cleanly");
    assert_eq!(
        signed_envelope
            .signature
            .as_ref()
            .map(|s| s.key_id.as_str()),
        Some("host-key-sess-99")
    );
    assert_eq!(signed_envelope.domain, "vam.v1.run-outcome-attestation");

    // Verify correctly
    let recovered =
        RunOutcomeAttestation::verify(&signed_envelope, "host-key-sess-99", &verifying_key)
            .expect("signature should verify");
    assert_eq!(recovered, attestation);

    // Tampered verification must fail
    let rogue_key = SigningKey::from_bytes(&[9u8; 32]).verifying_key();
    let fail_result =
        RunOutcomeAttestation::verify(&signed_envelope, "host-key-sess-99", &rogue_key);
    assert!(fail_result.is_err());
}

#[test]
fn test_openlineage_event_generation_and_facets() {
    let input_knowledge =
        build_knowledge_input_dataset("ref/system-instructions", "sha256:abcd1234");
    let input_drive9 =
        build_drive9_dataset("workspace-initial", "manifest-base", Some(2048), Some(2));
    let output_drive9 =
        build_drive9_dataset("workspace-final", "manifest-head", Some(8192), Some(8));

    let event = OpenLineageRunEvent::new(
        "COMPLETE",
        "2026-09-08T05:15:00.000Z",
        "urn:vam:run:run-lineage",
        "vam.orchestrator",
        "run:urn:vam:run:run-lineage",
        vec![input_knowledge, input_drive9],
        vec![output_drive9],
    );

    assert_eq!(event.event_type, "COMPLETE");
    assert_eq!(event.inputs.len(), 2);
    assert_eq!(event.outputs.len(), 1);

    let serialized = serde_json::to_string(&event).expect("openlineage serialization");
    assert!(serialized.contains("\"eventType\":\"COMPLETE\""));
    assert!(serialized.contains("\"namespace\":\"vam.knowledge\""));
    assert!(serialized.contains("\"manifestHash\":\"manifest-head\""));
    assert!(serialized.contains("\"storageEngine\":\"drive9-ext4-cas\""));
}

#[test]
fn test_cas_trajectory_replay_blob_persistence() {
    let temp = tempdir().expect("tempdir");
    let layout = Drive9Layout::new(temp.path().join("drive9"));
    let chunk_store = ChunkStore::new(layout);

    let events = vec![
        OutboxEvent {
            sequence: 1,
            run_id: "run-cas-1".to_string(),
            event_type: "vam.v1.tool.invoked".to_string(),
            payload: "{}".to_string(),
        },
        OutboxEvent {
            sequence: 2,
            run_id: "run-cas-1".to_string(),
            event_type: "vam.v1.host_tool.completed".to_string(),
            payload: "{\"step_id\":\"step-1\"}".to_string(),
        },
    ];

    let digest = persist_trajectory_replay_blob(&events, &chunk_store)
        .expect("trajectory replay blob should persist");
    assert!(!digest.is_empty());

    // Verify retrieval from CAS
    let chunk_bytes = chunk_store
        .get_chunk(&digest)
        .expect("chunk should exist in CAS");
    let stored_events: Vec<OutboxEvent> = serde_json::from_slice(&chunk_bytes)
        .expect("chunk should deserialize to OutboxEvent slice");
    assert_eq!(stored_events.len(), 2);
    assert_eq!(stored_events[0].event_type, "vam.v1.tool.invoked");
    assert_eq!(stored_events[1].event_type, "vam.v1.host_tool.completed");
}

#[test]
fn test_cost_loop_usage_summary_with_provenance() {
    let catalog = ModelCatalog::new();
    let pricing_entry = catalog
        .get("claude-3-5-sonnet-latest")
        .expect("claude-3-5-sonnet-latest should be in catalog");
    let provenance = catalog.price_provenance();

    let budget = SessionBudget::new(Some(Cost {
        currency: "USD".to_string(),
        micro_amount: 1_000_000, // $1.00 limit
    }));

    // Record usage: 1000 prompt tokens, 500 completion tokens
    let cost = budget
        .record_usage(
            1000,
            500,
            pricing_entry.price_prompt_micro,
            pricing_entry.price_completion_micro,
        )
        .expect("budget usage should record");

    // claude-3-5-sonnet-latest pricing: $3.00/M prompt, $15.00/M completion
    // Prompt: 1000 * 3.00 / 1M = 3000 micro-cents ($0.003)
    // Completion: 500 * 15.00 / 1M = 7500 micro-cents ($0.0075)
    // Total: 10500 micro-cents ($0.0105)
    assert_eq!(cost.currency, "USD");
    assert_eq!(cost.micro_amount, 10500);

    let summary = budget.build_usage_summary("claude-3-5-sonnet-latest", Some(provenance));
    assert_eq!(summary.prompt_tokens, 1000);
    assert_eq!(summary.completion_tokens, 500);
    assert_eq!(summary.total_tokens, 1500);
    assert_eq!(summary.estimated_cost.micro_amount, 10500);
    assert_eq!(summary.model_billed, "claude-3-5-sonnet-latest");

    let prov = summary.provenance.expect("provenance should be present");
    assert_eq!(
        prov.catalog_version,
        vam_inference::ModelCatalog::new().catalog_version()
    );
    assert_eq!(prov.pricing_effective_date, "2026-09-01");
}
