//! Host-side primitives for signed approval claims and replay prevention.
//!
//! The guest never receives an [`ApprovalSigner`]. Callers serialize dedicated,
//! domain-specific protobuf claim messages before signing them.

use ed25519_dalek::{Signature as Ed25519Signature, Signer, SigningKey, Verifier, VerifyingKey};
use std::collections::HashSet;
use std::sync::Mutex;
use thiserror::Error;
use vam_spec::v1::{Signature, SignedEnvelope, Version};

pub mod advertise;
pub mod audit;
pub mod engine;
pub mod legacy_shapes;
pub mod pdp;
pub mod permissions;
pub mod platform_policy;
pub mod receipt;
#[cfg(feature = "secret-forms")]
pub mod secret_forms;
#[cfg(feature = "ssrf-guard")]
pub mod ssrf_guard;
pub mod token;
pub mod vendor_text;

pub use advertise::{
    classify_scoped_tools, classify_tools, compose_policy, hitl_execute_overlay, DecidingScope,
    NamedTool, ScopeExplanation, ScopeLabel, ScopedTool, ToolDecision, ToolEffect,
    BUILTIN_EXECUTE_TOOL_POLICY, BUILTIN_TOOL_NAMES,
};

pub use audit::{AuditBlock, AuditLedger};
pub use engine::{ArcSwapPolicyEngine, CedarPolicyEngine, DEFAULT_CEDAR_SCHEMA};
pub use legacy_shapes::{convert_legacy_capability, LegacyCapabilityConversion};
pub use pdp::{
    DecisionOutcome, EvaluationContext, EvaluationRequest, PolicyDecisionPoint, PolicyMode,
    ScopeRule, ToolEntity,
};
pub use permissions::{
    always_offered, explain_host, explain_tool, format_host_explain, format_tool_explain,
    resolve_internet, resolve_tool, tool_entity, AlwaysOffer, HostExplanation, InternetResolution,
    OrgRule, PermissionsError, SourceRule, ToolQuery,
};
pub use platform_policy::{
    compile_egress_policy, compose_platform_policy, evaluate_egress, evaluate_tool_decision,
    EgressDecision, PLATFORM_TOOL_POLICY,
};
pub use token::{
    mint_capability_token, parse_capability_claims, verify_capability_token, CapabilityClaims,
    TOOL_CAPABILITY_DOMAIN,
};

#[derive(Debug, Error)]
pub enum SecurityError {
    #[error("approval signature uses an unsupported algorithm")]
    UnsupportedAlgorithm,
    #[error("approval signature domain does not match")]
    DomainMismatch,
    #[error("approval signature key ID does not match")]
    KeyMismatch,
    #[error("approval signature bytes are malformed")]
    MalformedSignature,
    #[error("approval signature verification failed")]
    VerificationFailed,
    #[error("approval token '{0}' has already been consumed")]
    TokenAlreadyConsumed(String),
    #[error("replay store lock is poisoned")]
    ReplayStoreUnavailable,
    #[error("schema validation failed: {0}")]
    SchemaValidation(String),
    #[error("policy syntax error: {0}")]
    PolicySyntax(String),
    #[error("stale policy generation: current={current}, attempted={attempted}")]
    StaleGeneration { current: u64, attempted: u64 },
    #[error("token serialization error: {0}")]
    TokenSerialization(String),
    #[error("session mismatch: expected={expected}, found={found}")]
    SessionMismatch { expected: String, found: String },
    #[error("lease mismatch: expected={expected}, found={found}")]
    LeaseMismatch { expected: String, found: String },
    #[error("generation mismatch: expected={expected}, found={found}")]
    GenerationMismatch { expected: u64, found: u64 },
    #[error("tool mismatch: expected={expected}, found={found}")]
    ToolMismatch { expected: String, found: String },
    #[error("arguments digest mismatch: expected={expected}, found={found}")]
    ArgumentsDigestMismatch { expected: String, found: String },
    #[error("script digest mismatch: expected={expected}, found={found}")]
    ScriptDigestMismatch { expected: String, found: String },
    #[error("resource envelope mismatch")]
    ResourceEnvelopeMismatch,
    #[error("token expired at {expires_at}, current time is {current_time}")]
    TokenExpired { expires_at: u64, current_time: u64 },
}

/// Durable orchestrators implement this trait in the same transaction that
/// records an approval decision. The in-memory implementation is only suitable
/// for tests and local development.
pub trait ReplayStore: Send + Sync {
    fn consume(&self, token_id: &str) -> Result<(), SecurityError>;
}

#[derive(Debug, Default)]
pub struct InMemoryReplayStore {
    consumed: Mutex<HashSet<String>>,
}

impl ReplayStore for InMemoryReplayStore {
    fn consume(&self, token_id: &str) -> Result<(), SecurityError> {
        let mut consumed = self
            .consumed
            .lock()
            .map_err(|_| SecurityError::ReplayStoreUnavailable)?;
        if !consumed.insert(token_id.to_string()) {
            return Err(SecurityError::TokenAlreadyConsumed(token_id.to_string()));
        }
        Ok(())
    }
}

#[derive(Debug)]
pub struct ApprovalSigner {
    key_id: String,
    signing_key: SigningKey,
}

impl ApprovalSigner {
    pub fn new(key_id: impl Into<String>, signing_key: SigningKey) -> Self {
        Self {
            key_id: key_id.into(),
            signing_key,
        }
    }

    pub fn key_id(&self) -> &str {
        &self.key_id
    }

    pub fn verifying_key(&self) -> VerifyingKey {
        self.signing_key.verifying_key()
    }

    pub fn sign(&self, domain: &str, claims: &[u8]) -> SignedEnvelope {
        let signature = self
            .signing_key
            .sign(signing_input(domain, claims).as_slice());
        SignedEnvelope {
            domain: domain.to_string(),
            envelope_version: Some(Version {
                major: 1,
                minor: 0,
                patch: 0,
                prerelease: String::new(),
                build: String::new(),
            }),
            claims: claims.to_vec(),
            signature: Some(Signature {
                algorithm: 1,
                key_id: self.key_id.clone(),
                value: signature.to_bytes().to_vec(),
            }),
        }
    }
}

pub fn verify(
    expected_domain: &str,
    expected_key_id: &str,
    verifying_key: &VerifyingKey,
    envelope: &SignedEnvelope,
) -> Result<(), SecurityError> {
    if envelope.domain != expected_domain {
        return Err(SecurityError::DomainMismatch);
    }
    let signature = envelope
        .signature
        .as_ref()
        .ok_or(SecurityError::MalformedSignature)?;
    if signature.algorithm != 1 {
        return Err(SecurityError::UnsupportedAlgorithm);
    }
    if signature.key_id != expected_key_id {
        return Err(SecurityError::KeyMismatch);
    }
    let signature = Ed25519Signature::from_slice(&signature.value)
        .map_err(|_| SecurityError::MalformedSignature)?;
    verifying_key
        .verify(
            signing_input(expected_domain, &envelope.claims).as_slice(),
            &signature,
        )
        .map_err(|_| SecurityError::VerificationFailed)
}

pub(crate) fn signing_input(domain: &str, claims: &[u8]) -> Vec<u8> {
    let domain_bytes = domain.as_bytes();
    let mut input = Vec::with_capacity(8 + domain_bytes.len() + claims.len());
    input.extend_from_slice(&(domain_bytes.len() as u32).to_be_bytes());
    input.extend_from_slice(domain_bytes);
    input.extend_from_slice(&(claims.len() as u32).to_be_bytes());
    input.extend_from_slice(claims);
    input
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signed_claims_are_domain_separated_and_single_use() {
        let signer = ApprovalSigner::new("test-key", SigningKey::from_bytes(&[7; 32]));
        let envelope = signer.sign("vam.v1.hitl-token", b"claims");
        verify(
            "vam.v1.hitl-token",
            "test-key",
            &signer.verifying_key(),
            &envelope,
        )
        .expect("signature should verify");
        assert!(verify(
            "vam.v1.approval-decision",
            "test-key",
            &signer.verifying_key(),
            &envelope,
        )
        .is_err());

        let replay_store = InMemoryReplayStore::default();
        replay_store
            .consume("token-1")
            .expect("first use should pass");
        assert!(replay_store.consume("token-1").is_err());
    }
}
