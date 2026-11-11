use crate::{verify, ApprovalSigner, ReplayStore, SecurityError};
use ed25519_dalek::VerifyingKey;
use serde::{Deserialize, Serialize};
use vam_spec::v1::SignedEnvelope;

pub const TOOL_CAPABILITY_DOMAIN: &str = "vam.v1.tool-capability";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityClaims {
    pub session_id: String,
    pub lease_id: String,
    pub lifecycle_generation: u64,
    pub tool_name: String,
    pub arguments_digest: String,
    pub script_digest: String,
    /// The model's id of the call this token was minted for (empty from a guest that sends none).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub call_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_envelope: Option<vam_spec::v1::ResourceEnvelope>,
    pub expires_at: u64,
    pub nonce: String,
}

pub fn mint_capability_token(
    signer: &ApprovalSigner,
    claims: &CapabilityClaims,
) -> Result<SignedEnvelope, SecurityError> {
    let claims_bytes =
        serde_json::to_vec(claims).map_err(|e| SecurityError::TokenSerialization(e.to_string()))?;
    Ok(signer.sign(TOOL_CAPABILITY_DOMAIN, &claims_bytes))
}

pub fn parse_capability_claims(
    envelope: &SignedEnvelope,
) -> Result<CapabilityClaims, SecurityError> {
    serde_json::from_slice::<CapabilityClaims>(&envelope.claims)
        .map_err(|e| SecurityError::TokenSerialization(e.to_string()))
}

// argument structs are listed for C2; no behaviour change here
#[allow(clippy::too_many_arguments)]
pub fn verify_capability_token(
    expected_key_id: &str,
    verifying_key: &VerifyingKey,
    envelope: &SignedEnvelope,
    expected_session_id: &str,
    expected_lease_id: &str,
    expected_generation: u64,
    expected_tool_name: &str,
    expected_arguments_digest: &str,
    expected_script_digest: &str,
    expected_envelope: Option<&vam_spec::v1::ResourceEnvelope>,
    current_time_secs: u64,
    replay_store: &dyn ReplayStore,
) -> Result<CapabilityClaims, SecurityError> {
    verify(
        TOOL_CAPABILITY_DOMAIN,
        expected_key_id,
        verifying_key,
        envelope,
    )?;

    let claims = parse_capability_claims(envelope)?;

    if claims.session_id != expected_session_id {
        return Err(SecurityError::SessionMismatch {
            expected: expected_session_id.to_string(),
            found: claims.session_id,
        });
    }

    if claims.lease_id != expected_lease_id {
        return Err(SecurityError::LeaseMismatch {
            expected: expected_lease_id.to_string(),
            found: claims.lease_id,
        });
    }

    if claims.lifecycle_generation != expected_generation {
        return Err(SecurityError::GenerationMismatch {
            expected: expected_generation,
            found: claims.lifecycle_generation,
        });
    }

    if claims.tool_name != expected_tool_name {
        return Err(SecurityError::ToolMismatch {
            expected: expected_tool_name.to_string(),
            found: claims.tool_name,
        });
    }

    if claims.arguments_digest != expected_arguments_digest {
        return Err(SecurityError::ArgumentsDigestMismatch {
            expected: expected_arguments_digest.to_string(),
            found: claims.arguments_digest,
        });
    }

    if claims.script_digest != expected_script_digest {
        return Err(SecurityError::ScriptDigestMismatch {
            expected: expected_script_digest.to_string(),
            found: claims.script_digest,
        });
    }

    if let Some(expected_env) = expected_envelope {
        if claims.resource_envelope.as_ref() != Some(expected_env) {
            return Err(SecurityError::ResourceEnvelopeMismatch);
        }
    }

    if claims.expires_at < current_time_secs {
        return Err(SecurityError::TokenExpired {
            expires_at: claims.expires_at,
            current_time: current_time_secs,
        });
    }

    // Replay prevention: single-use token consumption
    replay_store.consume(&claims.nonce)?;

    Ok(claims)
}
