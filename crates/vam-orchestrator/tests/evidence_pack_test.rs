use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use ed25519_dalek::SigningKey;
use tower::ServiceExt;
use vam_core::{CidAllocator, MockVmm};
use vam_orchestrator::api::auth::ControlPlanePdp;
use vam_orchestrator::api::{self, AppState};
use vam_orchestrator::evidence::{append_audit_jsonl, assemble_evidence_pack};
use vam_orchestrator::keys::{mint_api_key_secret, BOOTSTRAP_SCOPES, MEMBER_SCOPES};
use vam_orchestrator::manager::SessionManager;
use vam_orchestrator::secrets::SecretsVault;
use vam_orchestrator::workflow::SqliteWorkflowStore;
use vam_orchestrator::VmmFactory;
use vam_security::ApprovalSigner;
use vam_spec::v1::ResourceId;
use vam_telemetry::{
    current_audit_partition, verify_evidence_pack, write_evidence_dir, EvidencePackResponse,
    PartitionedAuditLedger, RunOutcomeAttestation, FILE_ARTIFACTS, FILE_ATTESTATION,
    FILE_ATTESTATION_DSSE, FILE_AUDIT, FILE_DENIALS, FILE_MANIFEST, FILE_TRAJECTORY,
};

fn bootstrap_store() -> (Arc<SqliteWorkflowStore>, String) {
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    store.initialize_catalog_schema().unwrap();
    let secret = mint_api_key_secret();
    let scopes: Vec<String> = BOOTSTRAP_SCOPES.iter().map(|s| s.to_string()).collect();
    store
        .create_api_key("test-key", "test", &secret, "User::\"tester\"", &scopes)
        .unwrap();
    (store, secret)
}

fn app_state_http_only(store: Arc<SqliteWorkflowStore>) -> AppState {
    if std::env::var("VAM_DRIVE9_ROOT").is_err() {
        std::env::set_var("VAM_DRIVE9_ROOT", "/tmp/vam-test-drive9");
    }
    let vault = SecretsVault::for_tests(store.clone());
    let cid_allocator = Arc::new(CidAllocator::default());
    let vmm_factory: VmmFactory = Arc::new(|_vm_id, _guest_cid| Box::new(MockVmm::new()));
    let (manager, _cmd_rx) = SessionManager::new(store.clone(), cid_allocator, vmm_factory);
    AppState {
        store,
        vault,
        manager,
        pdp: Arc::new(ControlPlanePdp::new().expect("pdp")),
        listen_addr: "127.0.0.1:8080".to_string(),
        draining: Arc::new(AtomicBool::new(false)),
        ready: Arc::new(AtomicBool::new(true)),
        e2e_mock_scripts: false,
    }
}

async fn response_body(response: axum::response::Response) -> String {
    String::from_utf8(
        hyper::body::to_bytes(response.into_body())
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap()
}

fn bearer_get_evidence(run_id: &str, secret: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(format!("/v1/runs/{}/evidence", encode_run_path(run_id)))
        .header("authorization", format!("Bearer {secret}"))
        .body(Body::empty())
        .unwrap()
}

async fn get_evidence(app: axum::Router, secret: &str, run_id: &str) -> (StatusCode, String) {
    let response = app
        .oneshot(bearer_get_evidence(run_id, secret))
        .await
        .unwrap();
    let status = response.status();
    (status, response_body(response).await)
}

fn export_and_verify(body: &str, owner: &str, session_id: &str) {
    let pack: EvidencePackResponse = serde_json::from_str(body).expect("parse pack");
    let audit = &pack.files[FILE_AUDIT];
    let expected_partition = current_audit_partition(session_id);
    assert_eq!(
        expected_partition,
        format!("tenant:local:{session_id}"),
        "partition key rewrite is out of scope"
    );
    assert!(
        audit.contains(&expected_partition),
        "audit must keep tenant:{{id}}:{{session}}: {audit}"
    );
    assert!(
        audit.contains(&format!("\"user_id\":\"{owner}\"")),
        "user_id is a record attribute: {audit}"
    );
    assert!(
        !audit.contains(&format!("tenant:{owner}:")),
        "user_id must not become a partition component: {audit}"
    );
    let dir = tempfile::tempdir().expect("verify dir");
    write_evidence_dir(dir.path(), &pack.files).expect("write pack");
    verify_evidence_pack(dir.path())
        .unwrap_or_else(|e| panic!("verify {owner} pack with existing partition keys: {e}"));
}

fn encode_run_path(run_id: &str) -> String {
    run_id
        .chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => c.to_string(),
            _ => format!("%{:02X}", c as u32),
        })
        .collect()
}

fn member_secret(store: &SqliteWorkflowStore, user_id: &str) -> String {
    store.create_user(user_id, user_id).unwrap();
    let scopes: Vec<String> = MEMBER_SCOPES.iter().map(|s| (*s).to_string()).collect();
    let secret = mint_api_key_secret();
    store
        .create_api_key_owned(
            &format!("{user_id}-key"),
            user_id,
            &secret,
            &format!("User::\"{user_id}\""),
            &scopes,
            user_id,
        )
        .unwrap();
    secret
}

fn seed_attested_run_owned(
    store: &SqliteWorkflowStore,
    owner: &str,
    run_id: &str,
    session_id: &str,
    tool_name: &str,
    trajectory_digest: &str,
) {
    store
        .create_session_with_owner(
            session_id,
            None,
            &format!("vm-{owner}"),
            &format!("{owner}--ws"),
            "default",
            owner,
        )
        .expect("session");
    store
        .persist_run_spec_with_owner(
            run_id,
            "{}",
            Some(session_id),
            &format!("urn:vam:agent:{owner}"),
            None,
            &format!("User::\"{owner}\""),
            owner,
        )
        .expect("bind session");
    seed_attested_events(store, run_id, session_id, tool_name, trajectory_digest);
}

fn seed_attested_run(store: &SqliteWorkflowStore, run_id: &str, session_id: &str) {
    store
        .persist_run_spec(
            run_id,
            "{}",
            Some(session_id),
            "urn:vam:agent:local.vam.dev/acme/tester",
            None,
            "User::\"tester\"",
        )
        .expect("bind session");
    seed_attested_events(store, run_id, session_id, "bash", "traj-digest-evidence");
}

fn seed_attested_events(
    store: &SqliteWorkflowStore,
    run_id: &str,
    session_id: &str,
    tool_name: &str,
    trajectory_digest: &str,
) {
    store
        .append_outbox_event(
            run_id,
            "vam.v1.policy.decided",
            &format!(
                r#"{{"tool_name":"{tool_name}","disposition":"DENY","args_digest":"digest-1"}}"#
            ),
        )
        .expect("deny event");
    store
        .append_outbox_event(
            run_id,
            "vam.v1.tool.invoked",
            &format!(r#"{{"tool_name":"{tool_name}","args_digest":"digest-1"}}"#),
        )
        .expect("tool invoked");

    store
        .record_audit_event(
            session_id,
            "vam.v1.policy.decided",
            br#"{"disposition":"DENY","args_digest":"digest-1"}"#,
        )
        .expect("ledger deny");

    let attestation = RunOutcomeAttestation {
        run_id: run_id.to_string(),
        session_id: session_id.to_string(),
        goal: "do not pack this prompt".to_string(),
        terminal_state: "SUCCEEDED".to_string(),
        terminator: "agent".to_string(),
        trajectory_digest: trajectory_digest.to_string(),
        artifacts: vec!["cas:artifact-1".to_string()],
        paths: vec!["drive9://ws/manifests/m1".to_string()],
        skills: vec![],
        tools: vec![tool_name.to_string()],
        eval: None,
        cost: None,
        timestamp_unix_ms: 1_778_000_000_000,
        thread_log_head_seq: None,
        thread_log_head_digest: None,
        workspace: None,
    };
    let signer = ApprovalSigner::new(
        format!("host-key-{session_id}"),
        SigningKey::from_bytes(&[42u8; 32]),
    );
    let envelope = attestation.sign(&signer).expect("sign");
    let json = vam_spec::encoding::proto_to_json("vam.v1.SignedEnvelope", &envelope).expect("json");
    store
        .append_outbox_event(run_id, "vam.v1.run.attested", &json)
        .expect("attest");
}

#[tokio::test]
async fn attested_run_with_deny_exports_evidence_pack() {
    let (store, secret) = bootstrap_store();
    let ledger = Arc::new(PartitionedAuditLedger::new());
    store.set_audit_ledger(ledger.clone());

    let run_id = "urn:vam:run:local.vam.dev/acme/evidence-ok";
    let session_id = "sess-evidence-ok";
    seed_attested_run(&store, run_id, session_id);

    let pack = assemble_evidence_pack(&store, run_id).expect("assemble");
    assert_eq!(pack.run_id, run_id);
    for name in [
        FILE_MANIFEST,
        FILE_ATTESTATION,
        FILE_ATTESTATION_DSSE,
        FILE_AUDIT,
        FILE_TRAJECTORY,
        FILE_ARTIFACTS,
        FILE_DENIALS,
    ] {
        assert!(
            pack.files.contains_key(name),
            "pack missing file {name}: {:?}",
            pack.files.keys().collect::<Vec<_>>()
        );
    }
    assert!(
        pack.files[FILE_ATTESTATION].contains("sha256:"),
        "goal must be hashed (T21): {}",
        pack.files[FILE_ATTESTATION]
    );
    assert!(
        !pack.files[FILE_ATTESTATION].contains("do not pack this prompt"),
        "raw prompt leaked into attestation.json"
    );
    assert!(
        !pack.files[FILE_TRAJECTORY].contains("args"),
        "trajectory must be types only: {}",
        pack.files[FILE_TRAJECTORY]
    );
    assert!(
        pack.files[FILE_DENIALS].contains("DENY"),
        "denials missing DENY: {}",
        pack.files[FILE_DENIALS]
    );
    assert!(
        pack.files[FILE_AUDIT].contains("vam.v1.policy.decided"),
        "audit chain empty"
    );
    assert!(
        pack.files[FILE_AUDIT].contains("tenant:"),
        "audit partition must stay tenant:{{id}}:{{session}}: {}",
        pack.files[FILE_AUDIT]
    );
    assert!(
        !pack.files[FILE_AUDIT].contains("tenant:tester:"),
        "user_id must not become a partition component: {}",
        pack.files[FILE_AUDIT]
    );
    let verify_dir = tempfile::tempdir().expect("verify dir");
    write_evidence_dir(verify_dir.path(), &pack.files).expect("write pack");
    verify_evidence_pack(verify_dir.path()).expect("existing partition format must verify");

    let app = api::router(app_state_http_only(store));
    let (status, body) = get_evidence(app, &secret, run_id).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn pack_carries_the_stored_receipt_byte_for_byte_and_verifies() {
    let (store, _secret) = bootstrap_store();
    store.set_audit_ledger(Arc::new(PartitionedAuditLedger::new()));
    let run_id = "urn:vam:run:local.vam.dev/acme/evidence-receipt";
    seed_attested_run(&store, run_id, "sess-evidence-receipt");

    // An older run: no receipt, so no receipt.json, and the pack still verifies.
    let old = assemble_evidence_pack(&store, run_id).expect("assemble");
    assert!(!old.files.contains_key(vam_telemetry::FILE_RECEIPT));
    assert!(old
        .files
        .contains_key(vam_telemetry::FILE_ATTESTATION_ENVELOPE));

    let signer = vam_security::receipt::ReceiptSigner::new(SigningKey::from_bytes(&[5u8; 32]));
    let appendix = serde_json::json!({"label": "written by the model", "finalReply": "ok"});
    let receipt = serde_json::json!({
        "version": vam_spec::receipt::RECEIPT_VERSION,
        "runId": run_id,
        "state": "SUCCEEDED",
        "trajectoryDigest": "traj-digest-evidence",
        "modelAppendixDigest": vam_security::receipt::appendix_digest(&appendix).unwrap(),
    });
    let signature = vam_security::receipt::sign_receipt(&signer, &receipt).unwrap();
    let document = serde_json::json!({
        "receipt": receipt, "modelAppendix": appendix, "signature": signature,
    })
    .to_string();
    assert!(store
        .insert_run_receipt(run_id, 1, &document, "sha256:x", signer.key_id())
        .unwrap());

    let pack = assemble_evidence_pack(&store, run_id).expect("assemble");
    assert_eq!(pack.files[vam_telemetry::FILE_RECEIPT], document);
    let manifest: vam_telemetry::EvidenceManifest =
        serde_json::from_str(&pack.files[FILE_MANIFEST]).unwrap();
    assert_eq!(
        manifest.files[vam_telemetry::FILE_RECEIPT],
        vam_telemetry::sha256_hex(document.as_bytes())
    );
    let dir = tempfile::tempdir().expect("dir");
    write_evidence_dir(dir.path(), &pack.files).expect("write");
    // The seeded attestation was signed before the node key (`host-key-*`): reported, not failed.
    let report = vam_telemetry::verify_evidence_pack_report(dir.path()).expect("verifies");
    assert_eq!(
        report.notes,
        vec!["attestation unverifiable (signed before the node key)".to_string()]
    );
}

#[tokio::test]
async fn unfinished_run_without_attestation_returns_409() {
    let (store, secret) = bootstrap_store();
    store.set_audit_ledger(Arc::new(PartitionedAuditLedger::new()));
    let run_id = "urn:vam:run:local.vam.dev/acme/evidence-inflight";
    store
        .create_run(&ResourceId {
            urn: run_id.to_string(),
        })
        .unwrap();

    let err = assemble_evidence_pack(&store, run_id).expect_err("in-flight must fail");
    match err {
        vam_orchestrator::OrchestratorError::Conflict(msg) => {
            assert!(msg.contains("no attestation"), "{msg}");
        }
        other => panic!("expected Conflict, got {other}"),
    }

    let app = api::router(app_state_http_only(store));
    let (status, body) = get_evidence(app, &secret, run_id).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
}

#[test]
fn append_audit_jsonl_writes_partition_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let ledger = PartitionedAuditLedger::new();
    let record = ledger
        .record_event("sess-persist", "vam.v1.session.booted", b"cold")
        .expect("record");
    append_audit_jsonl(dir.path(), &record).expect("persist");
    let path = dir.path().join("audit").join("sess-persist.jsonl");
    let text = std::fs::read_to_string(path).expect("read");
    assert!(text.contains("vam.v1.session.booted"));
}

/// Phase 9: `vam evidence export` / `verify` across two members.
///
/// Export is `GET /v1/runs/{id}/evidence` then `write_evidence_dir` (CLI path).
/// Verify is offline `verify_evidence_pack` (same as `vam evidence verify`).
#[tokio::test]
async fn two_user_evidence_export_verify_isolated() {
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    store.initialize_catalog_schema().unwrap();
    let ledger = Arc::new(PartitionedAuditLedger::new());
    store.set_audit_ledger(ledger);

    let alice = member_secret(&store, "alice");
    let bob = member_secret(&store, "bob");
    let alice_run = "urn:vam:run:local.vam.dev/acme/evidence-alice";
    let bob_run = "urn:vam:run:local.vam.dev/acme/evidence-bob";
    let alice_session = "sess-evidence-alice";
    let bob_session = "sess-evidence-bob";
    seed_attested_run_owned(
        &store,
        "alice",
        alice_run,
        alice_session,
        "alice-bash",
        "traj-alice",
    );
    seed_attested_run_owned(&store, "bob", bob_run, bob_session, "bob-bash", "traj-bob");

    let app = api::router(app_state_http_only(store));

    let (alice_status, alice_body) = get_evidence(app.clone(), &alice, alice_run).await;
    assert_eq!(alice_status, StatusCode::OK, "{alice_body}");
    assert!(alice_body.contains(alice_run), "{alice_body}");
    assert!(!alice_body.contains(bob_run), "{alice_body}");
    assert!(!alice_body.contains("bob-bash"), "{alice_body}");
    assert!(!alice_body.contains(bob_session), "{alice_body}");
    export_and_verify(&alice_body, "alice", alice_session);

    let (bob_status, bob_body) = get_evidence(app.clone(), &bob, bob_run).await;
    assert_eq!(bob_status, StatusCode::OK, "{bob_body}");
    assert!(bob_body.contains(bob_run), "{bob_body}");
    assert!(!bob_body.contains(alice_run), "{bob_body}");
    assert!(!bob_body.contains("alice-bash"), "{bob_body}");
    assert!(!bob_body.contains(alice_session), "{bob_body}");
    export_and_verify(&bob_body, "bob", bob_session);

    let (cross_status, cross_body) = get_evidence(app, &alice, bob_run).await;
    assert_eq!(cross_status, StatusCode::NOT_FOUND, "{cross_body}");
    assert!(!cross_body.contains("bob-bash"), "{cross_body}");
    assert!(!cross_body.contains("\"user_id\":\"bob\""), "{cross_body}");
    assert!(!cross_body.contains("traj-bob"), "{cross_body}");
    assert!(!cross_body.contains("attestation"), "{cross_body}");
    assert!(!cross_body.contains("audit.jsonl"), "{cross_body}");
}
