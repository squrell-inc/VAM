//! Portable run evidence pack (attestation, audit chain, trajectory digest, denials).
//!
//! Pack files never include raw prompts, tool args, or guest stdout (T21/T23).
//! Verify is offline: recompute hashes, `verify_ledger_chain`, unwrap DSSE.

use crate::attestation::RunOutcomeAttestation;
use crate::audit::{verify_ledger_chain, AuditRecord};
use crate::dsse::{unwrap_attestation_dsse, DsseEnvelope};
use crate::error::VamTelemetryError;
use crate::redact::{DefaultRedactor, TelemetryRedactor};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use vam_spec::v1::SignedEnvelope;

pub const FILE_MANIFEST: &str = "manifest.json";
pub const FILE_ATTESTATION: &str = "attestation.json";
pub const FILE_ATTESTATION_DSSE: &str = "attestation.dsse.json";
pub const FILE_AUDIT: &str = "audit.jsonl";
pub const FILE_TRAJECTORY: &str = "trajectory.json";
pub const FILE_ARTIFACTS: &str = "artifacts.json";
pub const FILE_DENIALS: &str = "denials.jsonl";
/// The run's signed receipt, byte for byte. Absent from packs of runs that
/// ended before receipts existed.
pub const FILE_RECEIPT: &str = "receipt.json";
/// The signed attestation envelope (`vam.v1.run.attested`), as stored.
pub const FILE_ATTESTATION_ENVELOPE: &str = "attestation.envelope.json";

/// Files a pack may carry. Hashed in the manifest when present; a pack
/// without them still verifies.
pub const OPTIONAL_HASHED_FILES: &[&str] = &[FILE_RECEIPT, FILE_ATTESTATION_ENVELOPE];

pub const HASHED_FILES: &[&str] = &[
    FILE_ATTESTATION,
    FILE_ATTESTATION_DSSE,
    FILE_AUDIT,
    FILE_TRAJECTORY,
    FILE_ARTIFACTS,
    FILE_DENIALS,
];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct EvidenceManifest {
    pub run_id: String,
    pub exported_at_unix_ms: i64,
    pub files: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct EvidencePackResponse {
    pub run_id: String,
    pub exported_at_unix_ms: i64,
    pub files: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TrajectoryPack {
    pub digest: String,
    pub events: Vec<TrajectoryPackEvent>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TrajectoryPackEvent {
    pub sequence: i64,
    pub event_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactsPack {
    pub artifacts: Vec<String>,
    pub paths: Vec<String>,
    pub trajectory_digest: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DenialRow {
    pub event_type: String,
    pub disposition: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args_digest: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub domain_digest: Option<String>,
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub fn to_jsonl<T: Serialize>(items: &[T]) -> Result<String, VamTelemetryError> {
    let mut out = String::new();
    for item in items {
        let line =
            serde_json::to_string(item).map_err(|e| VamTelemetryError::EvidencePackInvalid {
                reason: format!("failed to serialize jsonl row: {e}"),
            })?;
        out.push_str(&line);
        out.push('\n');
    }
    Ok(out)
}

pub fn parse_jsonl<T: for<'de> Deserialize<'de>>(text: &str) -> Result<Vec<T>, VamTelemetryError> {
    let mut items = Vec::new();
    for (idx, line) in text.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let item =
            serde_json::from_str(trimmed).map_err(|e| VamTelemetryError::EvidencePackInvalid {
                reason: format!("invalid jsonl at line {}: {e}", idx + 1),
            })?;
        items.push(item);
    }
    Ok(items)
}

pub fn build_manifest(
    run_id: &str,
    exported_at_unix_ms: i64,
    files: &BTreeMap<String, String>,
) -> EvidenceManifest {
    let mut hashes = BTreeMap::new();
    for name in HASHED_FILES.iter().chain(OPTIONAL_HASHED_FILES) {
        if let Some(content) = files.get(*name) {
            hashes.insert((*name).to_string(), sha256_hex(content.as_bytes()));
        }
    }
    EvidenceManifest {
        run_id: run_id.to_string(),
        exported_at_unix_ms,
        files: hashes,
    }
}

pub fn finish_pack(
    run_id: &str,
    exported_at_unix_ms: i64,
    mut files: BTreeMap<String, String>,
) -> Result<EvidencePackResponse, VamTelemetryError> {
    let manifest = build_manifest(run_id, exported_at_unix_ms, &files);
    let manifest_json = serde_json::to_string_pretty(&manifest).map_err(|e| {
        VamTelemetryError::EvidencePackInvalid {
            reason: format!("failed to serialize manifest: {e}"),
        }
    })?;
    files.insert(FILE_MANIFEST.to_string(), manifest_json);
    Ok(EvidencePackResponse {
        run_id: run_id.to_string(),
        exported_at_unix_ms,
        files,
    })
}

pub fn write_evidence_dir(
    dir: &Path,
    files: &BTreeMap<String, String>,
) -> Result<(), VamTelemetryError> {
    fs::create_dir_all(dir).map_err(|e| VamTelemetryError::SpoolDiskFailure {
        path: dir.display().to_string(),
        details: e.to_string(),
    })?;
    for (name, content) in files {
        if name.contains('/') || name.contains('\\') || name.contains("..") {
            return Err(VamTelemetryError::EvidencePackInvalid {
                reason: format!("refusing to write unsafe evidence file name '{name}'"),
            });
        }
        let path = dir.join(name);
        fs::write(&path, content).map_err(|e| VamTelemetryError::SpoolDiskFailure {
            path: path.display().to_string(),
            details: e.to_string(),
        })?;
    }
    Ok(())
}

/// What a passing verification has to say besides "ok".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PackReport {
    pub notes: Vec<String>,
}

pub fn verify_evidence_pack(dir: &Path) -> Result<(), VamTelemetryError> {
    verify_evidence_pack_report(dir).map(|_| ())
}

fn invalid(reason: impl Into<String>) -> VamTelemetryError {
    VamTelemetryError::EvidencePackInvalid {
        reason: reason.into(),
    }
}

/// [`verify_evidence_pack`], also returning notes the caller should show.
pub fn verify_evidence_pack_report(dir: &Path) -> Result<PackReport, VamTelemetryError> {
    let mut report = PackReport::default();
    let manifest_bytes = read_pack_file(dir, FILE_MANIFEST)?;
    let manifest: EvidenceManifest = serde_json::from_slice(&manifest_bytes).map_err(|e| {
        VamTelemetryError::EvidencePackInvalid {
            reason: format!("invalid manifest.json: {e}"),
        }
    })?;

    for name in HASHED_FILES {
        let expected =
            manifest
                .files
                .get(*name)
                .ok_or_else(|| VamTelemetryError::EvidencePackInvalid {
                    reason: format!("manifest.json missing hash for {name}"),
                })?;
        let bytes = read_pack_file(dir, name)?;
        let actual = sha256_hex(&bytes);
        if actual != *expected {
            return Err(VamTelemetryError::EvidencePackInvalid {
                reason: format!("hash mismatch for {name}"),
            });
        }
    }

    // Optional files: hashed when present; listed-but-missing fails.
    for name in OPTIONAL_HASHED_FILES {
        let present = dir.join(name).exists();
        match (manifest.files.get(*name), present) {
            (Some(expected), true) => {
                if sha256_hex(&read_pack_file(dir, name)?) != *expected {
                    return Err(invalid(format!("hash mismatch for {name}")));
                }
            }
            (Some(_), false) => {
                return Err(invalid(format!("manifest lists {name} but it is missing")));
            }
            (None, true) => {
                return Err(invalid(format!("{name} is not covered by manifest.json")));
            }
            (None, false) => {}
        }
    }

    let audit_text = read_pack_text(dir, FILE_AUDIT)?;
    let records: Vec<AuditRecord> = parse_jsonl(&audit_text)?;
    verify_ledger_chain(&records, None)?;

    let trajectory: TrajectoryPack = serde_json::from_slice(&read_pack_file(dir, FILE_TRAJECTORY)?)
        .map_err(|e| VamTelemetryError::EvidencePackInvalid {
            reason: format!("invalid trajectory.json: {e}"),
        })?;

    let attestation = unwrap_pack_attestation(dir)?;
    if attestation.run_id != manifest.run_id {
        return Err(VamTelemetryError::EvidencePackInvalid {
            reason: format!(
                "attestation run_id '{}' does not match manifest '{}'",
                attestation.run_id, manifest.run_id
            ),
        });
    }
    if attestation.trajectory_digest != trajectory.digest {
        return Err(VamTelemetryError::EvidencePackInvalid {
            reason: "attestation trajectory_digest does not match trajectory.json digest".into(),
        });
    }

    let receipt_key = verify_pack_receipt(dir, &manifest, &trajectory)?;
    verify_pack_envelope(
        dir,
        &manifest,
        &trajectory,
        receipt_key.as_ref(),
        &mut report,
    )?;

    let denials_text = read_pack_text(dir, FILE_DENIALS)?;
    assert_no_secrets(&denials_text)?;
    assert_no_secrets(&read_pack_text(dir, FILE_TRAJECTORY)?)?;
    let denials: Vec<DenialRow> = parse_jsonl(&denials_text)?;
    for (idx, row) in denials.iter().enumerate() {
        if !is_deny_disposition(&row.disposition) {
            return Err(VamTelemetryError::EvidencePackInvalid {
                reason: format!("denials.jsonl line {} is not DENY", idx + 1),
            });
        }
    }

    Ok(report)
}

/// The key a pack's receipt names.
struct ReceiptKey {
    key_id: String,
    public_key: [u8; 32],
}

/// Checks `receipt.json` when the pack has one: signature, appendix digest,
/// run id and trajectory digest. Returns the key it names.
fn verify_pack_receipt(
    dir: &Path,
    manifest: &EvidenceManifest,
    trajectory: &TrajectoryPack,
) -> Result<Option<ReceiptKey>, VamTelemetryError> {
    if !dir.join(FILE_RECEIPT).exists() {
        return Ok(None);
    }
    let bytes = read_pack_file(dir, FILE_RECEIPT)?;
    let verified = vam_security::receipt::verify_receipt(&bytes, &[])
        .map_err(|e| invalid(format!("receipt.json: {e}")))?;
    let doc: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|e| invalid(format!("receipt.json is not JSON: {e}")))?;
    let run_id = doc.pointer("/receipt/runId").and_then(|v| v.as_str());
    if run_id != Some(manifest.run_id.as_str()) {
        return Err(invalid(format!(
            "receipt run id {:?} does not match the pack's '{}'",
            run_id.unwrap_or(""),
            manifest.run_id
        )));
    }
    if let Some(digest) = doc
        .pointer("/receipt/trajectoryDigest")
        .and_then(|v| v.as_str())
    {
        if digest != trajectory.digest {
            return Err(invalid(
                "receipt trajectoryDigest does not match the pack's trajectory.json digest",
            ));
        }
    }
    let public_key: [u8; 32] = vam_spec::receipt::hex_decode(&verified.public_key)
        .ok()
        .and_then(|b| b.try_into().ok())
        .ok_or_else(|| invalid("receipt public key is not 32 bytes"))?;
    Ok(Some(ReceiptKey {
        key_id: verified.key_id,
        public_key,
    }))
}

/// Checks the stored attestation envelope's signature against the node key the
/// receipt names. An envelope signed before the node key existed (`host-key-*`,
/// a public constant) proves nothing: it is reported, not failed.
fn verify_pack_envelope(
    dir: &Path,
    manifest: &EvidenceManifest,
    trajectory: &TrajectoryPack,
    receipt_key: Option<&ReceiptKey>,
    report: &mut PackReport,
) -> Result<(), VamTelemetryError> {
    if !dir.join(FILE_ATTESTATION_ENVELOPE).exists() {
        return Ok(());
    }
    let text = read_pack_text(dir, FILE_ATTESTATION_ENVELOPE)?;
    let envelope: SignedEnvelope =
        vam_spec::encoding::json_to_proto("vam.v1.SignedEnvelope", &text)
            .map_err(|e| invalid(format!("attestation.envelope.json: {e}")))?;
    let envelope_key_id = envelope
        .signature
        .as_ref()
        .map(|s| s.key_id.clone())
        .unwrap_or_default();
    if envelope_key_id.starts_with("host-key-") {
        report
            .notes
            .push("attestation unverifiable (signed before the node key)".to_string());
        return Ok(());
    }
    let Some(key) = receipt_key else {
        report.notes.push(
            "attestation signature not checked (the pack has no receipt to name the node key)"
                .to_string(),
        );
        return Ok(());
    };
    if envelope_key_id != key.key_id {
        return Err(invalid(format!(
            "attestation signed by {envelope_key_id}, but the receipt names {}",
            key.key_id
        )));
    }
    let verifying_key = ed25519_dalek::VerifyingKey::from_bytes(&key.public_key)
        .map_err(|_| invalid("receipt public key is not a valid key"))?;
    let attestation = RunOutcomeAttestation::verify(&envelope, &key.key_id, &verifying_key)
        .map_err(|e| invalid(e.to_string()))?;
    if attestation.run_id != manifest.run_id {
        return Err(invalid("attestation envelope is for another run"));
    }
    if attestation.trajectory_digest != trajectory.digest {
        return Err(invalid(
            "attestation envelope trajectory digest does not match trajectory.json",
        ));
    }
    Ok(())
}

fn unwrap_pack_attestation(dir: &Path) -> Result<RunOutcomeAttestation, VamTelemetryError> {
    let dsse_bytes = read_pack_file(dir, FILE_ATTESTATION_DSSE)?;
    if let Ok(dsse) = serde_json::from_slice::<DsseEnvelope>(&dsse_bytes) {
        return unwrap_attestation_dsse(&dsse);
    }
    let dsse_text = String::from_utf8_lossy(&dsse_bytes);
    if let Ok(envelope) =
        vam_spec::encoding::json_to_proto::<SignedEnvelope>("vam.v1.SignedEnvelope", &dsse_text)
    {
        return serde_json::from_slice(&envelope.claims).map_err(|e| {
            VamTelemetryError::AttestationFailed {
                reason: format!("failed to deserialize SignedEnvelope claims: {e}"),
            }
        });
    }
    let att_bytes = read_pack_file(dir, FILE_ATTESTATION)?;
    let att_text = String::from_utf8_lossy(&att_bytes);
    if let Ok(envelope) =
        vam_spec::encoding::json_to_proto::<SignedEnvelope>("vam.v1.SignedEnvelope", &att_text)
    {
        return serde_json::from_slice(&envelope.claims).map_err(|e| {
            VamTelemetryError::AttestationFailed {
                reason: format!("failed to deserialize SignedEnvelope claims: {e}"),
            }
        });
    }
    serde_json::from_slice(&att_bytes).map_err(|e| VamTelemetryError::AttestationFailed {
        reason: format!("failed to parse attestation.json or attestation.dsse.json: {e}"),
    })
}

fn read_pack_file(dir: &Path, name: &str) -> Result<Vec<u8>, VamTelemetryError> {
    let path = dir.join(name);
    fs::read(&path).map_err(|e| VamTelemetryError::EvidencePackInvalid {
        reason: format!("missing or unreadable {name}: {e}"),
    })
}

fn read_pack_text(dir: &Path, name: &str) -> Result<String, VamTelemetryError> {
    String::from_utf8(read_pack_file(dir, name)?).map_err(|e| {
        VamTelemetryError::EvidencePackInvalid {
            reason: format!("{name} is not valid UTF-8: {e}"),
        }
    })
}

fn assert_no_secrets(text: &str) -> Result<(), VamTelemetryError> {
    let redacted = DefaultRedactor::global().redact_text(text);
    if redacted != text {
        return Err(VamTelemetryError::EvidencePackInvalid {
            reason: "pack file contains secret-looking payload (T21)".into(),
        });
    }
    Ok(())
}

pub fn is_deny_disposition(value: &str) -> bool {
    matches!(
        value.to_ascii_uppercase().as_str(),
        "DENY" | "DENIED" | "BLOCK" | "BLOCKED"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::PartitionedAuditLedger;
    use crate::dsse::wrap_attestation_dsse;
    use crate::{CostSummary, EvaluationSummary};
    use ed25519_dalek::SigningKey;
    use vam_security::ApprovalSigner;

    fn sample_attestation() -> RunOutcomeAttestation {
        RunOutcomeAttestation {
            run_id: "urn:vam:run:evidence-1".to_string(),
            session_id: "sess-evidence".to_string(),
            goal: "sha256:deadbeef".to_string(),
            terminal_state: "SUCCEEDED".to_string(),
            terminator: "agent".to_string(),
            trajectory_digest: "traj-digest-1".to_string(),
            artifacts: vec!["cas:abc".to_string()],
            paths: vec!["drive9://ws/manifests/m1".to_string()],
            skills: vec![],
            tools: vec!["run".to_string()],
            eval: Some(EvaluationSummary {
                assertion_id: "a1".to_string(),
                evaluator_id: "e1".to_string(),
                passed: true,
            }),
            cost: Some(CostSummary {
                currency: "USD".to_string(),
                spend_micro: 10,
                prompt_tokens: 1,
                completion_tokens: 1,
                total_tokens: 2,
                pricing_catalog_version: Some("pricing-v1".to_string()),
            }),
            timestamp_unix_ms: 1_778_000_000_000,
            thread_log_head_seq: None,
            thread_log_head_digest: None,
            workspace: None,
        }
    }

    fn write_intact_pack(dir: &Path) {
        let attestation = sample_attestation();
        let signer = ApprovalSigner::new("node-key-evidence", SigningKey::from_bytes(&[42u8; 32]));
        let envelope = attestation.sign(&signer).expect("sign");
        let att_json = serde_json::to_vec(&attestation).expect("att json");
        let dsse = wrap_attestation_dsse(&att_json, &envelope).expect("dsse");

        let ledger = PartitionedAuditLedger::new();
        ledger
            .record_event_for_user(
                "tenant:local:sess-evidence",
                "vam.v1.policy.decided",
                b"{\"disposition\":\"DENY\"}",
                Some("alice"),
            )
            .expect("audit");
        let audit =
            to_jsonl(&ledger.read_partition("tenant:local:sess-evidence")).expect("audit jsonl");

        let trajectory = TrajectoryPack {
            digest: attestation.trajectory_digest.clone(),
            events: vec![TrajectoryPackEvent {
                sequence: 1,
                event_type: "vam.v1.policy.decided".to_string(),
            }],
        };
        let artifacts = ArtifactsPack {
            artifacts: attestation.artifacts.clone(),
            paths: attestation.paths.clone(),
            trajectory_digest: attestation.trajectory_digest.clone(),
        };
        let denials = to_jsonl(&[DenialRow {
            event_type: "vam.v1.policy.decided".to_string(),
            disposition: "DENY".to_string(),
            tool_name: Some("bash".to_string()),
            args_digest: Some("args-digest".to_string()),
            domain_digest: None,
        }])
        .expect("denials");

        let mut files = BTreeMap::new();
        files.insert(
            FILE_ATTESTATION.to_string(),
            String::from_utf8(att_json).expect("utf8"),
        );
        files.insert(
            FILE_ATTESTATION_DSSE.to_string(),
            serde_json::to_string(&dsse).expect("dsse json"),
        );
        files.insert(FILE_AUDIT.to_string(), audit);
        files.insert(
            FILE_TRAJECTORY.to_string(),
            serde_json::to_string(&trajectory).expect("traj"),
        );
        files.insert(
            FILE_ARTIFACTS.to_string(),
            serde_json::to_string(&artifacts).expect("artifacts"),
        );
        files.insert(FILE_DENIALS.to_string(), denials);
        let pack = finish_pack(&attestation.run_id, 1_778_000_000_000, files).expect("pack");
        write_evidence_dir(dir, &pack.files).expect("write");
    }

    #[test]
    fn intact_fixture_verifies() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_intact_pack(dir.path());
        verify_evidence_pack(dir.path()).expect("intact pack should verify");
        let audit = fs::read_to_string(dir.path().join(FILE_AUDIT)).expect("audit");
        assert!(audit.contains("tenant:local:sess-evidence"));
        assert!(audit.contains("\"user_id\":\"alice\""));
        assert!(!audit.contains("tenant:alice:"));
    }

    #[test]
    fn tampered_audit_jsonl_fails_verify() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_intact_pack(dir.path());
        let audit_path = dir.path().join(FILE_AUDIT);
        let mut records: Vec<AuditRecord> =
            parse_jsonl(&fs::read_to_string(&audit_path).expect("read")).expect("parse");
        records[0].payload_digest = "deadbeef".repeat(8);
        fs::write(&audit_path, to_jsonl(&records).expect("rewrite")).expect("write");
        // Manifest still has the old hash, so verify fails closed on hash or chain.
        let err = verify_evidence_pack(dir.path()).expect_err("tamper must fail");
        let msg = err.to_string();
        assert!(
            msg.contains("hash mismatch") || msg.contains("corrupted") || msg.contains("Audit"),
            "unexpected tamper error: {msg}"
        );
    }

    #[test]
    fn allow_row_in_denials_fails_verify() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_intact_pack(dir.path());
        let denials = to_jsonl(&[DenialRow {
            event_type: "vam.v1.policy.decided".to_string(),
            disposition: "ALLOW".to_string(),
            tool_name: Some("bash".to_string()),
            args_digest: Some("d".to_string()),
            domain_digest: None,
        }])
        .expect("denials");
        let mut files = BTreeMap::new();
        for name in HASHED_FILES {
            files.insert(
                (*name).to_string(),
                fs::read_to_string(dir.path().join(name)).expect("read"),
            );
        }
        files.insert(FILE_DENIALS.to_string(), denials);
        let pack = finish_pack("urn:vam:run:evidence-1", 1, files).expect("pack");
        write_evidence_dir(dir.path(), &pack.files).expect("rewrite");
        let err = verify_evidence_pack(dir.path()).expect_err("ALLOW must fail");
        match err {
            VamTelemetryError::EvidencePackInvalid { reason } => {
                assert!(reason.contains("not DENY"));
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    const NODE_SEED: [u8; 32] = [42u8; 32];

    fn node_signer() -> vam_security::receipt::ReceiptSigner {
        vam_security::receipt::ReceiptSigner::new(SigningKey::from_bytes(&NODE_SEED))
    }

    /// A signed receipt document for the fixture run.
    fn signed_receipt(run_id: &str, trajectory: &str) -> String {
        let appendix = serde_json::json!({"label": "written by the model", "finalReply": "ok"});
        let receipt = serde_json::json!({
            "version": vam_spec::receipt::RECEIPT_VERSION,
            "runId": run_id,
            "state": "SUCCEEDED",
            "trajectoryDigest": trajectory,
            "modelAppendixDigest": vam_security::receipt::appendix_digest(&appendix).unwrap(),
        });
        let signature = vam_security::receipt::sign_receipt(&node_signer(), &receipt).unwrap();
        serde_json::json!({
            "receipt": receipt,
            "modelAppendix": appendix,
            "signature": signature,
        })
        .to_string()
    }

    fn envelope_json(key_id: &str, seed: [u8; 32]) -> String {
        let signer = ApprovalSigner::new(key_id, SigningKey::from_bytes(&seed));
        let envelope = sample_attestation().sign(&signer).expect("sign");
        vam_spec::encoding::proto_to_json("vam.v1.SignedEnvelope", &envelope).expect("json")
    }

    /// Rewrites the pack with `extra` files added and a fresh manifest.
    fn add_files(dir: &Path, extra: &[(&str, String)]) {
        let mut files = BTreeMap::new();
        for name in HASHED_FILES {
            files.insert(
                (*name).to_string(),
                fs::read_to_string(dir.join(name)).expect("read"),
            );
        }
        for (name, content) in extra {
            files.insert((*name).to_string(), content.clone());
        }
        let pack = finish_pack("urn:vam:run:evidence-1", 1, files).expect("pack");
        write_evidence_dir(dir, &pack.files).expect("rewrite");
    }

    fn node_pack(dir: &Path) {
        write_intact_pack(dir);
        let node_id = node_signer().key_id().to_string();
        add_files(
            dir,
            &[
                (
                    FILE_RECEIPT,
                    signed_receipt("urn:vam:run:evidence-1", "traj-digest-1"),
                ),
                (
                    FILE_ATTESTATION_ENVELOPE,
                    envelope_json(&node_id, NODE_SEED),
                ),
            ],
        );
    }

    #[test]
    fn a_pack_with_a_receipt_and_node_signed_attestation_verifies() {
        let dir = tempfile::tempdir().expect("tempdir");
        node_pack(dir.path());
        let report = verify_evidence_pack_report(dir.path()).expect("verifies");
        assert!(report.notes.is_empty(), "{:?}", report.notes);
    }

    #[test]
    fn a_pack_without_a_receipt_still_verifies() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_intact_pack(dir.path());
        assert!(!dir.path().join(FILE_RECEIPT).exists());
        verify_evidence_pack(dir.path()).expect("older packs pass");
    }

    #[test]
    fn a_changed_receipt_fails_the_pack() {
        let dir = tempfile::tempdir().expect("tempdir");
        node_pack(dir.path());
        // Changed on disk: the manifest hash no longer matches.
        let path = dir.path().join(FILE_RECEIPT);
        let text = fs::read_to_string(&path)
            .unwrap()
            .replacen("SUCCEEDED", "SUCCEEDEE", 1);
        fs::write(&path, text).unwrap();
        let err = verify_evidence_pack(dir.path()).expect_err("hash");
        assert!(
            err.to_string().contains("hash mismatch for receipt.json"),
            "{err}"
        );
        // Changed and re-hashed: the signature catches it.
        let text = fs::read_to_string(&path).unwrap();
        let envelope = fs::read_to_string(dir.path().join(FILE_ATTESTATION_ENVELOPE)).unwrap();
        add_files(
            dir.path(),
            &[(FILE_RECEIPT, text), (FILE_ATTESTATION_ENVELOPE, envelope)],
        );
        let err = verify_evidence_pack(dir.path()).expect_err("signature");
        assert!(
            err.to_string().to_lowercase().contains("signature"),
            "{err}"
        );
    }

    #[test]
    fn a_receipt_of_another_run_or_trajectory_fails_the_pack() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_intact_pack(dir.path());
        add_files(
            dir.path(),
            &[(
                FILE_RECEIPT,
                signed_receipt("urn:vam:run:other", "traj-digest-1"),
            )],
        );
        let err = verify_evidence_pack(dir.path()).expect_err("run id");
        assert!(err.to_string().contains("run id"), "{err}");
        add_files(
            dir.path(),
            &[(
                FILE_RECEIPT,
                signed_receipt("urn:vam:run:evidence-1", "other-trajectory"),
            )],
        );
        let err = verify_evidence_pack(dir.path()).expect_err("trajectory");
        assert!(err.to_string().contains("trajectoryDigest"), "{err}");
    }

    #[test]
    fn an_attestation_signed_by_another_key_fails_the_pack() {
        let dir = tempfile::tempdir().expect("tempdir");
        node_pack(dir.path());
        let other = vam_security::receipt::ReceiptSigner::new(SigningKey::from_bytes(&[9u8; 32]));
        add_files(
            dir.path(),
            &[
                (
                    FILE_RECEIPT,
                    fs::read_to_string(dir.path().join(FILE_RECEIPT)).unwrap(),
                ),
                (
                    FILE_ATTESTATION_ENVELOPE,
                    envelope_json(other.key_id(), [9u8; 32]),
                ),
            ],
        );
        let err = verify_evidence_pack(dir.path()).expect_err("different node key");
        assert!(err.to_string().contains("receipt names"), "{err}");
    }

    #[test]
    fn a_forged_attestation_under_the_node_key_id_fails_the_pack() {
        let dir = tempfile::tempdir().expect("tempdir");
        node_pack(dir.path());
        let node_id = node_signer().key_id().to_string();
        // The right key id, signed with some other secret.
        add_files(
            dir.path(),
            &[
                (
                    FILE_RECEIPT,
                    fs::read_to_string(dir.path().join(FILE_RECEIPT)).unwrap(),
                ),
                (
                    FILE_ATTESTATION_ENVELOPE,
                    envelope_json(&node_id, [1u8; 32]),
                ),
            ],
        );
        let err = verify_evidence_pack(dir.path()).expect_err("forged");
        assert!(err.to_string().contains("signature"), "{err}");
    }

    #[test]
    fn an_attestation_signed_before_the_node_key_is_reported_not_failed() {
        let dir = tempfile::tempdir().expect("tempdir");
        node_pack(dir.path());
        add_files(
            dir.path(),
            &[
                (
                    FILE_RECEIPT,
                    fs::read_to_string(dir.path().join(FILE_RECEIPT)).unwrap(),
                ),
                (
                    FILE_ATTESTATION_ENVELOPE,
                    envelope_json("host-key-sess-evidence", [42u8; 32]),
                ),
            ],
        );
        let report = verify_evidence_pack_report(dir.path()).expect("passes");
        assert_eq!(
            report.notes,
            vec!["attestation unverifiable (signed before the node key)".to_string()]
        );
    }

    #[test]
    fn a_missing_envelope_skips_the_attestation_signature_check() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_intact_pack(dir.path());
        add_files(
            dir.path(),
            &[(
                FILE_RECEIPT,
                signed_receipt("urn:vam:run:evidence-1", "traj-digest-1"),
            )],
        );
        verify_evidence_pack(dir.path()).expect("receipt alone verifies");
    }

    #[test]
    fn secret_looking_denial_fails_verify() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_intact_pack(dir.path());
        let denials = "{\"eventType\":\"vam.v1.policy.decided\",\"disposition\":\"DENY\",\"argsDigest\":\"sk-1234567890abcdef1234567890abcdef\"}\n";
        let mut files = BTreeMap::new();
        for name in HASHED_FILES {
            files.insert(
                (*name).to_string(),
                fs::read_to_string(dir.path().join(name)).expect("read"),
            );
        }
        files.insert(FILE_DENIALS.to_string(), denials.to_string());
        let pack = finish_pack("urn:vam:run:evidence-1", 1, files).expect("pack");
        write_evidence_dir(dir.path(), &pack.files).expect("rewrite");
        let err = verify_evidence_pack(dir.path()).expect_err("secret payload must fail");
        match err {
            VamTelemetryError::EvidencePackInvalid { reason } => {
                assert!(reason.contains("secret-looking"));
            }
            other => panic!("unexpected error: {other}"),
        }
    }
}
