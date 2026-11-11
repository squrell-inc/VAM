use crate::error::VamTelemetryError;
use ed25519_dalek::{Signature as Ed25519Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

pub const GENESIS_HASH: &str = "0000000000000000000000000000000000000000000000000000000000000000";

/// A single tamper-evident entry in the cryptographic audit ledger.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AuditRecord {
    pub sequence: u64,
    pub prev_hash: String,
    pub timestamp_unix_nanos: u128,
    pub partition_id: String,
    pub event_type: String,
    pub payload_digest: String,
    pub record_hash: String,
    pub checkpoint_signature: Option<Vec<u8>>,
    /// Host-stamped control-plane owner. Attribution only — not a partition-key
    /// component and not included in [`AuditRecord::compute_hash`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
}

impl AuditRecord {
    /// Compute the canonical SHA-256 hash of this record's fields.
    pub fn compute_hash(
        sequence: u64,
        prev_hash: &str,
        timestamp_unix_nanos: u128,
        partition_id: &str,
        event_type: &str,
        payload_digest: &str,
    ) -> String {
        let mut hasher = Sha256::new();
        hasher.update(sequence.to_be_bytes());
        hasher.update(prev_hash.as_bytes());
        hasher.update(timestamp_unix_nanos.to_be_bytes());
        hasher.update(partition_id.as_bytes());
        hasher.update(event_type.as_bytes());
        hasher.update(payload_digest.as_bytes());
        format!("{:x}", hasher.finalize())
    }

    /// Bytes formatted for Ed25519 checkpoint signing.
    pub fn signing_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"vam.v1.audit-checkpoint:");
        bytes.extend_from_slice(self.record_hash.as_bytes());
        bytes
    }
}

/// Partition state tracking the chain of audit records for a given session or tenant.
#[derive(Debug, Clone)]
struct PartitionChain {
    records: Vec<AuditRecord>,
    last_hash: String,
    next_sequence: u64,
}

impl Default for PartitionChain {
    fn default() -> Self {
        Self {
            records: Vec::new(),
            last_hash: GENESIS_HASH.to_string(),
            next_sequence: 0,
        }
    }
}

/// Thread-safe Partitioned Cryptographic Audit Ledger.
/// Partitions entries by session/tenant to eliminate append bottlenecks while providing
/// non-repudiable hash-chain continuity and Ed25519 checkpointing (Decision T23).
#[derive(Debug, Clone, Default)]
pub struct PartitionedAuditLedger {
    partitions: Arc<Mutex<HashMap<String, PartitionChain>>>,
}

impl PartitionedAuditLedger {
    pub fn new() -> Self {
        Self {
            partitions: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Append an audit event to a specific partition.
    /// Computes SHA-256 of the payload and chains the new record to the partition's head.
    pub fn record_event(
        &self,
        partition_id: &str,
        event_type: &str,
        payload_bytes: &[u8],
    ) -> Result<AuditRecord, VamTelemetryError> {
        self.record_event_for_user(partition_id, event_type, payload_bytes, None)
    }

    /// Append an audit event and optionally stamp `user_id` as a record attribute.
    /// Partition key is unchanged (`tenant:{id}:{session}`).
    pub fn record_event_for_user(
        &self,
        partition_id: &str,
        event_type: &str,
        payload_bytes: &[u8],
        user_id: Option<&str>,
    ) -> Result<AuditRecord, VamTelemetryError> {
        let mut parts =
            self.partitions
                .lock()
                .map_err(|e| VamTelemetryError::SpoolDiskFailure {
                    path: format!("audit_ledger/{partition_id}"),
                    details: format!("mutex poisoned: {e}"),
                })?;

        let chain = parts.entry(partition_id.to_string()).or_default();

        let sequence = chain.next_sequence;
        let prev_hash = chain.last_hash.clone();
        let timestamp_unix_nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);

        let mut payload_hasher = Sha256::new();
        payload_hasher.update(payload_bytes);
        let payload_digest = format!("{:x}", payload_hasher.finalize());

        let record_hash = AuditRecord::compute_hash(
            sequence,
            &prev_hash,
            timestamp_unix_nanos,
            partition_id,
            event_type,
            &payload_digest,
        );

        let record = AuditRecord {
            sequence,
            prev_hash,
            timestamp_unix_nanos,
            partition_id: partition_id.to_string(),
            event_type: event_type.to_string(),
            payload_digest,
            record_hash: record_hash.clone(),
            checkpoint_signature: None,
            user_id: user_id
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
        };

        chain.last_hash = record_hash;
        chain.next_sequence += 1;
        chain.records.push(record.clone());

        Ok(record)
    }

    /// Issue a signed checkpoint on the partition using an Ed25519 signing key.
    /// Checkpoints sign the current chain head to provide cryptographic proof of custody.
    pub fn checkpoint(
        &self,
        partition_id: &str,
        signing_key: &SigningKey,
    ) -> Result<AuditRecord, VamTelemetryError> {
        let mut parts =
            self.partitions
                .lock()
                .map_err(|e| VamTelemetryError::SpoolDiskFailure {
                    path: format!("audit_ledger/{partition_id}"),
                    details: format!("mutex poisoned: {e}"),
                })?;

        let chain = parts.entry(partition_id.to_string()).or_default();

        let sequence = chain.next_sequence;
        let prev_hash = chain.last_hash.clone();
        let timestamp_unix_nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);

        let payload_digest = format!("{:x}", Sha256::digest(b"CHECKPOINT"));
        let event_type = "vam.v1.audit.checkpoint";

        let record_hash = AuditRecord::compute_hash(
            sequence,
            &prev_hash,
            timestamp_unix_nanos,
            partition_id,
            event_type,
            &payload_digest,
        );

        let mut record = AuditRecord {
            sequence,
            prev_hash,
            timestamp_unix_nanos,
            partition_id: partition_id.to_string(),
            event_type: event_type.to_string(),
            payload_digest,
            record_hash: record_hash.clone(),
            checkpoint_signature: None,
            user_id: None,
        };

        let signature = signing_key.sign(&record.signing_bytes());
        record.checkpoint_signature = Some(signature.to_bytes().to_vec());

        chain.last_hash = record_hash;
        chain.next_sequence += 1;
        chain.records.push(record.clone());

        Ok(record)
    }

    /// Read all audit records for a partition.
    pub fn read_partition(&self, partition_id: &str) -> Vec<AuditRecord> {
        self.partitions
            .lock()
            .ok()
            .and_then(|p| p.get(partition_id).map(|c| c.records.clone()))
            .unwrap_or_default()
    }

    /// Verify an in-memory partition against its hash chain and optional Ed25519 checkpoints.
    pub fn verify_partition(
        &self,
        partition_id: &str,
        verifying_key: Option<&VerifyingKey>,
    ) -> Result<(), VamTelemetryError> {
        let records = self.read_partition(partition_id);
        verify_ledger_chain(&records, verifying_key)
    }
}

/// Verify a sequence of audit records for tamper evidence, hash continuity, and checkpoint signatures.
pub fn verify_ledger_chain(
    records: &[AuditRecord],
    verifying_key: Option<&VerifyingKey>,
) -> Result<(), VamTelemetryError> {
    if records.is_empty() {
        return Ok(());
    }

    let mut expected_prev = GENESIS_HASH.to_string();

    for (idx, record) in records.iter().enumerate() {
        let expected_sequence = idx as u64;

        // 1. Sequence check
        if record.sequence != expected_sequence {
            return Err(VamTelemetryError::LedgerCorrupted {
                sequence: record.sequence,
            });
        }

        // 2. Hash link check
        if record.prev_hash != expected_prev {
            return Err(VamTelemetryError::LedgerCorrupted {
                sequence: record.sequence,
            });
        }

        // 3. Recompute record hash
        let recomputed = AuditRecord::compute_hash(
            record.sequence,
            &record.prev_hash,
            record.timestamp_unix_nanos,
            &record.partition_id,
            &record.event_type,
            &record.payload_digest,
        );

        if record.record_hash != recomputed {
            return Err(VamTelemetryError::LedgerCorrupted {
                sequence: record.sequence,
            });
        }

        // 4. Verify checkpoint signature if present
        if let Some(ref sig_bytes) = record.checkpoint_signature {
            if let Some(vk) = verifying_key {
                let sig = Ed25519Signature::from_slice(sig_bytes).map_err(|e| {
                    VamTelemetryError::AuditSignatureInvalid {
                        sequence: record.sequence,
                        details: format!("malformed Ed25519 signature: {e}"),
                    }
                })?;

                vk.verify(&record.signing_bytes(), &sig).map_err(|e| {
                    VamTelemetryError::AuditSignatureInvalid {
                        sequence: record.sequence,
                        details: format!("signature mismatch: {e}"),
                    }
                })?;
            }
        }

        expected_prev = record.record_hash.clone();
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_audit_hash_chain_and_checkpoint() {
        let ledger = PartitionedAuditLedger::new();
        let signing_key = SigningKey::from_bytes(&[42u8; 32]);
        let verifying_key = signing_key.verifying_key();

        let partition = "session-123";

        // Append 3 events
        ledger
            .record_event(
                partition,
                "vam.v1.tool.invoked",
                b"{\"tool\":\"read_file\"}",
            )
            .unwrap();
        ledger
            .record_event(
                partition,
                "vam.v1.policy.decided",
                b"{\"decision\":\"ALLOW\"}",
            )
            .unwrap();
        ledger
            .record_event(partition, "vam.v1.tool.completed", b"{\"status\":\"OK\"}")
            .unwrap();

        // Sign checkpoint
        ledger.checkpoint(partition, &signing_key).unwrap();

        // Verify clean partition
        assert!(ledger
            .verify_partition(partition, Some(&verifying_key))
            .is_ok());

        // Tamper test: mutate a payload digest
        let mut records = ledger.read_partition(partition);
        records[1].payload_digest = "deadbeef".repeat(8);

        let tamper_res = verify_ledger_chain(&records, Some(&verifying_key));
        assert!(tamper_res.is_err());
        match tamper_res.unwrap_err() {
            VamTelemetryError::LedgerCorrupted { sequence } => {
                assert_eq!(sequence, 1);
            }
            other => panic!("Expected LedgerCorrupted, got {:?}", other),
        }
    }

    #[test]
    fn user_id_is_record_attribute_not_hash_input() {
        let ledger = PartitionedAuditLedger::new();
        let partition = "tenant:local:sess-user";
        let with_user = ledger
            .record_event_for_user(partition, "vam.v1.session.booted", b"cold", Some("alice"))
            .unwrap();
        assert_eq!(with_user.user_id.as_deref(), Some("alice"));
        assert_eq!(with_user.partition_id, partition);
        assert!(!with_user.partition_id.contains("alice"));

        let expected = AuditRecord::compute_hash(
            with_user.sequence,
            &with_user.prev_hash,
            with_user.timestamp_unix_nanos,
            &with_user.partition_id,
            &with_user.event_type,
            &with_user.payload_digest,
        );
        assert_eq!(with_user.record_hash, expected);
        assert!(ledger.verify_partition(partition, None).is_ok());
    }

    #[test]
    fn legacy_audit_json_without_user_id_still_verifies() {
        let partition = "tenant:local:sess-legacy";
        let timestamp = 1_778_000_000_000u128;
        let payload_digest = format!("{:x}", Sha256::digest(b"cold"));
        let record_hash = AuditRecord::compute_hash(
            0,
            GENESIS_HASH,
            timestamp,
            partition,
            "vam.v1.session.booted",
            &payload_digest,
        );
        let legacy = format!(
            r#"{{"sequence":0,"prev_hash":"{GENESIS_HASH}","timestamp_unix_nanos":{timestamp},"partition_id":"{partition}","event_type":"vam.v1.session.booted","payload_digest":"{payload_digest}","record_hash":"{record_hash}","checkpoint_signature":null}}"#
        );
        let parsed: AuditRecord = serde_json::from_str(&legacy).expect("legacy json");
        assert_eq!(parsed.user_id, None);
        assert_eq!(parsed.partition_id, partition);
        assert!(verify_ledger_chain(&[parsed], None).is_ok());
    }
}
