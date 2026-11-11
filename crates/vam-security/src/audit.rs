use crate::SecurityError;
use ed25519_dalek::{Signature as Ed25519Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AuditBlock {
    pub index: u64,
    pub previous_hash: String,
    pub timestamp_unix_nanos: u128,
    pub session_id: String,
    pub principal: String,
    pub action: String,
    pub resource: String,
    pub decision: String,
    pub matching_policies: Vec<String>,
    pub diagnostic_message: String,
    pub context_digest: String,
    pub policy_bundle_digest: String,
    pub parent_snapshot_id: Option<String>,
    pub script_digest: Option<String>,
    pub cpu_time_ms: Option<u64>,
    pub peak_rss_bytes: Option<u64>,
    pub wall_time_ms: Option<u64>,
    pub egress_bytes: Option<u64>,
    pub signature: Vec<u8>,
}

impl AuditBlock {
    pub fn compute_block_hash(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(self.index.to_be_bytes());
        hasher.update(self.previous_hash.as_bytes());
        hasher.update(self.timestamp_unix_nanos.to_be_bytes());
        hasher.update(self.session_id.as_bytes());
        hasher.update(self.principal.as_bytes());
        hasher.update(self.action.as_bytes());
        hasher.update(self.resource.as_bytes());
        hasher.update(self.decision.as_bytes());
        for p in &self.matching_policies {
            hasher.update(p.as_bytes());
        }
        hasher.update(self.diagnostic_message.as_bytes());
        hasher.update(self.context_digest.as_bytes());
        hasher.update(self.policy_bundle_digest.as_bytes());
        if let Some(ref ps) = self.parent_snapshot_id {
            hasher.update(ps.as_bytes());
        }
        if let Some(ref sd) = self.script_digest {
            hasher.update(sd.as_bytes());
        }
        if let Some(cpu) = self.cpu_time_ms {
            hasher.update(cpu.to_be_bytes());
        }
        if let Some(rss) = self.peak_rss_bytes {
            hasher.update(rss.to_be_bytes());
        }
        if let Some(wall) = self.wall_time_ms {
            hasher.update(wall.to_be_bytes());
        }
        if let Some(egress) = self.egress_bytes {
            hasher.update(egress.to_be_bytes());
        }
        format!("{:x}", hasher.finalize())
    }

    pub fn signing_bytes(&self) -> Vec<u8> {
        let hash = self.compute_block_hash();
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"vam.v1.audit-block:");
        bytes.extend_from_slice(hash.as_bytes());
        bytes
    }
}

pub struct AuditLedger {
    signing_key: SigningKey,
    key_id: String,
    state: Mutex<LedgerState>,
}

struct LedgerState {
    chain: Vec<AuditBlock>,
    current_hash: String,
    current_index: u64,
    parent_snapshot_id: Option<String>,
}

impl AuditLedger {
    pub fn new(
        key_id: impl Into<String>,
        signing_key: SigningKey,
        initial_parent_snapshot: Option<String>,
    ) -> Self {
        let genesis_hash = if let Some(ref ps) = initial_parent_snapshot {
            let mut hasher = Sha256::new();
            hasher.update(b"fork-from-snapshot:");
            hasher.update(ps.as_bytes());
            format!("{:x}", hasher.finalize())
        } else {
            "0".repeat(64)
        };

        Self {
            signing_key,
            key_id: key_id.into(),
            state: Mutex::new(LedgerState {
                chain: Vec::new(),
                current_hash: genesis_hash,
                current_index: 0,
                parent_snapshot_id: initial_parent_snapshot,
            }),
        }
    }

    pub fn fork_at_snapshot(
        &self,
        new_parent_snapshot_id: String,
    ) -> Result<AuditLedger, SecurityError> {
        let state = self
            .state
            .lock()
            .map_err(|_| SecurityError::ReplayStoreUnavailable)?;
        let mut hasher = Sha256::new();
        hasher.update(b"fork-from-snapshot:");
        hasher.update(new_parent_snapshot_id.as_bytes());
        hasher.update(state.current_hash.as_bytes());
        let fork_hash = format!("{:x}", hasher.finalize());

        Ok(AuditLedger {
            signing_key: self.signing_key.clone(),
            key_id: self.key_id.clone(),
            state: Mutex::new(LedgerState {
                chain: Vec::new(),
                current_hash: fork_hash,
                current_index: 0,
                parent_snapshot_id: Some(new_parent_snapshot_id),
            }),
        })
    }

    pub fn has_parent_snapshot(&self) -> bool {
        self.state
            .lock()
            .map(|s| s.parent_snapshot_id.is_some())
            .unwrap_or(false)
    }

    pub fn parent_snapshot_id(&self) -> Option<String> {
        self.state
            .lock()
            .ok()
            .and_then(|s| s.parent_snapshot_id.clone())
    }

    // argument structs are listed for C2; no behaviour change here
    #[allow(clippy::too_many_arguments)]
    pub fn record_decision(
        &self,
        session_id: &str,
        principal: &str,
        action: &str,
        resource: &str,
        decision: &str,
        matching_policies: Vec<String>,
        diagnostic_message: &str,
        context_json: &serde_json::Value,
        policy_bundle_digest: &str,
    ) -> Result<AuditBlock, SecurityError> {
        self.record_decision_with_telemetry(
            session_id,
            principal,
            action,
            resource,
            decision,
            matching_policies,
            diagnostic_message,
            context_json,
            policy_bundle_digest,
            None,
            None,
            None,
            None,
            None,
        )
    }

    // argument structs are listed for C2; no behaviour change here
    #[allow(clippy::too_many_arguments)]
    pub fn record_decision_with_telemetry(
        &self,
        session_id: &str,
        principal: &str,
        action: &str,
        resource: &str,
        decision: &str,
        matching_policies: Vec<String>,
        diagnostic_message: &str,
        context_json: &serde_json::Value,
        policy_bundle_digest: &str,
        script_digest: Option<String>,
        cpu_time_ms: Option<u64>,
        peak_rss_bytes: Option<u64>,
        wall_time_ms: Option<u64>,
        egress_bytes: Option<u64>,
    ) -> Result<AuditBlock, SecurityError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| SecurityError::ReplayStoreUnavailable)?;

        let mut ctx_hasher = Sha256::new();
        ctx_hasher.update(context_json.to_string().as_bytes());
        let context_digest = format!("{:x}", ctx_hasher.finalize());

        let timestamp_unix_nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);

        let mut block = AuditBlock {
            index: state.current_index,
            previous_hash: state.current_hash.clone(),
            timestamp_unix_nanos,
            session_id: session_id.to_string(),
            principal: principal.to_string(),
            action: action.to_string(),
            resource: resource.to_string(),
            decision: decision.to_string(),
            matching_policies,
            diagnostic_message: diagnostic_message.to_string(),
            context_digest,
            policy_bundle_digest: policy_bundle_digest.to_string(),
            parent_snapshot_id: state.parent_snapshot_id.clone(),
            script_digest,
            cpu_time_ms,
            peak_rss_bytes,
            wall_time_ms,
            egress_bytes,
            signature: Vec::new(),
        };

        let sig = self.signing_key.sign(&block.signing_bytes());
        block.signature = sig.to_bytes().to_vec();

        state.current_hash = block.compute_block_hash();
        state.current_index += 1;
        state.chain.push(block.clone());

        Ok(block)
    }

    pub fn get_chain(&self) -> Result<Vec<AuditBlock>, SecurityError> {
        let state = self
            .state
            .lock()
            .map_err(|_| SecurityError::ReplayStoreUnavailable)?;
        Ok(state.chain.clone())
    }

    pub fn verify_chain(
        blocks: &[AuditBlock],
        verifying_key: &VerifyingKey,
    ) -> Result<(), SecurityError> {
        if blocks.is_empty() {
            return Ok(());
        }

        for (i, block) in blocks.iter().enumerate() {
            if block.index != i as u64 {
                return Err(SecurityError::VerificationFailed);
            }

            if i > 0 {
                let prev_block = &blocks[i - 1];
                if block.previous_hash != prev_block.compute_block_hash() {
                    return Err(SecurityError::VerificationFailed);
                }
            }

            let sig = Ed25519Signature::from_slice(&block.signature)
                .map_err(|_| SecurityError::MalformedSignature)?;

            verifying_key
                .verify(&block.signing_bytes(), &sig)
                .map_err(|_| SecurityError::VerificationFailed)?;
        }

        Ok(())
    }
}
