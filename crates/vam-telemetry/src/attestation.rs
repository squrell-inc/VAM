//! Cryptographically signed run-outcome attestations for durable provenance.
//!
//! Internal system of record on the outbox is [`vam_spec::v1::SignedEnvelope`]
//! (Ed25519, domain-separated). Egress files / optional HTTP wrap the same
//! attestation JSON in DSSE (`wrap_attestation_dsse`).

use crate::error::VamTelemetryError;
use ed25519_dalek::VerifyingKey;
use serde::{Deserialize, Serialize};
use vam_security::ApprovalSigner;
use vam_spec::v1::SignedEnvelope;

pub const RUN_OUTCOME_ATTESTATION_DOMAIN: &str = "vam.v1.run-outcome-attestation";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EvaluationSummary {
    pub assertion_id: String,
    pub evaluator_id: String,
    pub passed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CostSummary {
    pub currency: String,
    pub spend_micro: i64,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub total_tokens: i64,
    pub pricing_catalog_version: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunOutcomeAttestation {
    pub run_id: String,
    pub session_id: String,
    pub goal: String,
    pub terminal_state: String,
    pub terminator: String,
    pub trajectory_digest: String,
    pub artifacts: Vec<String>,
    pub paths: Vec<String>,
    pub skills: Vec<String>,
    pub tools: Vec<String>,
    pub eval: Option<EvaluationSummary>,
    pub cost: Option<CostSummary>,
    pub timestamp_unix_ms: i64,
    /// Head of the thread log after the task's `task_end` record (seq and
    /// record digest): ties the signed outcome to the host-recorded
    /// conversation. Absent when nothing was recorded; claims signed before
    /// these fields existed still verify.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_log_head_seq: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_log_head_digest: Option<String>,
    /// The workspace on both sides of the task: the HEAD it started from, the
    /// HEAD it left (after the end commit, so it covers the conversation), and
    /// the hash of its files alone. Absent when the task produced no commit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<WorkspaceReceipt>,
}

/// See [`RunOutcomeAttestation::workspace`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceReceipt {
    pub head_before: String,
    pub head_after: String,
    pub files_hash: String,
}

impl RunOutcomeAttestation {
    /// Signs this outcome attestation with the given host key signer, producing a portable `SignedEnvelope`.
    pub fn sign(&self, signer: &ApprovalSigner) -> Result<SignedEnvelope, VamTelemetryError> {
        let claims =
            serde_json::to_vec(self).map_err(|e| VamTelemetryError::AttestationFailed {
                reason: format!("failed to serialize run outcome attestation claims: {}", e),
            })?;
        Ok(signer.sign(RUN_OUTCOME_ATTESTATION_DOMAIN, &claims))
    }

    /// Verifies the signature of a `SignedEnvelope` and deserializes the underlying `RunOutcomeAttestation`.
    pub fn verify(
        envelope: &SignedEnvelope,
        expected_key_id: &str,
        verifying_key: &VerifyingKey,
    ) -> Result<Self, VamTelemetryError> {
        vam_security::verify(
            RUN_OUTCOME_ATTESTATION_DOMAIN,
            expected_key_id,
            verifying_key,
            envelope,
        )
        .map_err(|e| VamTelemetryError::AttestationFailed {
            reason: format!(
                "run outcome attestation signature verification failed: {}",
                e
            ),
        })?;

        let attestation: Self = serde_json::from_slice(&envelope.claims).map_err(|e| {
            VamTelemetryError::AttestationFailed {
                reason: format!(
                    "failed to deserialize run outcome attestation claims: {}",
                    e
                ),
            }
        })?;

        Ok(attestation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;

    #[test]
    fn test_sign_and_verify_run_outcome_attestation() {
        let signing_key = SigningKey::from_bytes(&[77u8; 32]);
        let verifying_key = signing_key.verifying_key();
        let key_id = "node-key-test-1";
        let signer = ApprovalSigner::new(key_id, signing_key);

        let attestation = RunOutcomeAttestation {
            run_id: "urn:vam:run:test-run-1".to_string(),
            session_id: "sess-run-test-1".to_string(),
            goal: "Write a fibonacci function in Rust".to_string(),
            terminal_state: "SUCCEEDED".to_string(),
            terminator: "agent".to_string(),
            trajectory_digest: "abcd1234ef5678".to_string(),
            artifacts: vec!["/workspace/src/fib.rs".to_string()],
            paths: vec!["src/fib.rs".to_string()],
            skills: vec!["rust-code".to_string()],
            tools: vec!["write_file".to_string(), "run_command".to_string()],
            eval: Some(EvaluationSummary {
                assertion_id: "assert-fib-passes".to_string(),
                evaluator_id: "eval-cargo-test".to_string(),
                passed: true,
            }),
            cost: Some(CostSummary {
                currency: "USD".to_string(),
                spend_micro: 450_000,
                prompt_tokens: 15_000,
                completion_tokens: 2_500,
                total_tokens: 17_500,
                pricing_catalog_version: Some("2026-v1".to_string()),
            }),
            timestamp_unix_ms: 1772900000000,
            thread_log_head_seq: None,
            thread_log_head_digest: None,
            workspace: None,
        };

        let envelope = attestation.sign(&signer).expect("signing should succeed");
        assert_eq!(envelope.domain, RUN_OUTCOME_ATTESTATION_DOMAIN);

        let verified = RunOutcomeAttestation::verify(&envelope, key_id, &verifying_key)
            .expect("verification should succeed");
        assert_eq!(verified, attestation);

        // Verification fails with wrong key_id
        assert!(RunOutcomeAttestation::verify(&envelope, "wrong-key-id", &verifying_key).is_err());

        // Verification fails with wrong verifying key
        let wrong_key = SigningKey::from_bytes(&[88u8; 32]).verifying_key();
        assert!(RunOutcomeAttestation::verify(&envelope, key_id, &wrong_key).is_err());

        // Verification fails if claims are tampered
        let mut tampered_envelope = envelope.clone();
        tampered_envelope.claims[0] ^= 0xFF;
        assert!(RunOutcomeAttestation::verify(&tampered_envelope, key_id, &verifying_key).is_err());
    }

    #[test]
    fn thread_log_head_is_signed_and_old_claims_still_verify() {
        let signing_key = SigningKey::from_bytes(&[77u8; 32]);
        let verifying_key = signing_key.verifying_key();
        let signer = ApprovalSigner::new("k", signing_key);
        let mut attestation = RunOutcomeAttestation {
            run_id: "r".into(),
            session_id: "s".into(),
            goal: "g".into(),
            terminal_state: "SUCCEEDED".into(),
            terminator: "agent".into(),
            trajectory_digest: String::new(),
            artifacts: vec![],
            paths: vec![],
            skills: vec![],
            tools: vec![],
            eval: None,
            cost: None,
            timestamp_unix_ms: 1,
            thread_log_head_seq: None,
            thread_log_head_digest: None,
            workspace: None,
        };
        // Without a head the claims carry no new fields: the bytes are what an
        // older signer produced.
        let bare = serde_json::to_string(&attestation).unwrap();
        assert!(!bare.contains("thread_log_head"));
        let old: RunOutcomeAttestation = serde_json::from_str(&bare).unwrap();
        assert_eq!(old, attestation);
        // With a head, the digest is part of the signed claims.
        attestation.thread_log_head_seq = Some(42);
        attestation.thread_log_head_digest = Some("ab".repeat(32));
        let envelope = attestation.sign(&signer).unwrap();
        let verified = RunOutcomeAttestation::verify(&envelope, "k", &verifying_key).unwrap();
        assert_eq!(verified.thread_log_head_seq, Some(42));
        assert_eq!(verified.thread_log_head_digest, Some("ab".repeat(32)));
        let mut tampered = envelope.clone();
        let text = String::from_utf8(tampered.claims.clone()).unwrap();
        tampered.claims = text
            .replace(&"ab".repeat(32), &"cd".repeat(32))
            .into_bytes();
        assert!(RunOutcomeAttestation::verify(&tampered, "k", &verifying_key).is_err());

        // The workspace on both sides of the task is signed too.
        attestation.workspace = Some(WorkspaceReceipt {
            head_before: "b1".repeat(32),
            head_after: "a1".repeat(32),
            files_hash: "f1".repeat(32),
        });
        let envelope = attestation.sign(&signer).unwrap();
        let verified = RunOutcomeAttestation::verify(&envelope, "k", &verifying_key).unwrap();
        assert_eq!(verified.workspace, attestation.workspace);
        let mut forged = envelope.clone();
        let text = String::from_utf8(forged.claims.clone()).unwrap();
        forged.claims = text
            .replace(&"a1".repeat(32), &"a2".repeat(32))
            .into_bytes();
        assert!(RunOutcomeAttestation::verify(&forged, "k", &verifying_key).is_err());
    }
}
