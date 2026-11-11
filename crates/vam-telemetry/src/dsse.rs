//! DSSE (Dead Simple Signing Envelope) wrap for run-outcome attestations at egress.
//!
//! Internal outbox rows keep `SignedEnvelope`. Files / optional HTTP use DSSE:
//! `payloadType` `application/vnd.vam.run-outcome+json`, `payload` standard base64,
//! `signatures` from the existing Ed25519 envelope.

use crate::attestation::RunOutcomeAttestation;
use crate::error::VamTelemetryError;
use serde::{Deserialize, Serialize};
use vam_spec::v1::SignedEnvelope;

pub const DSSE_PAYLOAD_TYPE: &str = "application/vnd.vam.run-outcome+json";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DsseEnvelope {
    #[serde(rename = "payloadType")]
    pub payload_type: String,
    pub payload: String,
    pub signatures: Vec<DsseSignature>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DsseSignature {
    pub keyid: String,
    pub sig: String,
}

/// Wrap attestation JSON + the outbox `SignedEnvelope` as DSSE JSON.
pub fn wrap_attestation_dsse(
    attestation_json: &[u8],
    envelope: &SignedEnvelope,
) -> Result<DsseEnvelope, VamTelemetryError> {
    let keyid = envelope
        .signature
        .as_ref()
        .map(|s| s.key_id.clone())
        .unwrap_or_default();
    let sig_bytes = envelope
        .signature
        .as_ref()
        .map(|s| s.value.as_slice())
        .unwrap_or(&[]);

    Ok(DsseEnvelope {
        payload_type: DSSE_PAYLOAD_TYPE.to_string(),
        payload: encode_base64(attestation_json),
        signatures: vec![DsseSignature {
            keyid,
            sig: encode_base64(sig_bytes),
        }],
    })
}

pub fn dsse_to_value(envelope: &DsseEnvelope) -> Result<serde_json::Value, VamTelemetryError> {
    serde_json::to_value(envelope).map_err(|e| VamTelemetryError::AttestationFailed {
        reason: format!("failed to serialize DSSE envelope: {e}"),
    })
}

/// Decode the DSSE payload back to `RunOutcomeAttestation` (unit-test helper).
pub fn unwrap_attestation_dsse(
    envelope: &DsseEnvelope,
) -> Result<RunOutcomeAttestation, VamTelemetryError> {
    if envelope.payload_type != DSSE_PAYLOAD_TYPE {
        return Err(VamTelemetryError::AttestationFailed {
            reason: format!(
                "unexpected DSSE payloadType '{}', expected '{DSSE_PAYLOAD_TYPE}'",
                envelope.payload_type
            ),
        });
    }
    let bytes =
        decode_base64(&envelope.payload).map_err(|e| VamTelemetryError::AttestationFailed {
            reason: format!("invalid DSSE payload base64: {e}"),
        })?;
    serde_json::from_slice(&bytes).map_err(|e| VamTelemetryError::AttestationFailed {
        reason: format!("failed to deserialize attestation from DSSE payload: {e}"),
    })
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn encode_base64(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    let mut i = 0;
    while i < data.len() {
        let b0 = data[i];
        let b1 = if i + 1 < data.len() { data[i + 1] } else { 0 };
        let b2 = if i + 2 < data.len() { data[i + 2] } else { 0 };
        out.push(B64[(b0 >> 2) as usize] as char);
        out.push(B64[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        if i + 1 < data.len() {
            out.push(B64[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] as char);
        } else {
            out.push('=');
        }
        if i + 2 < data.len() {
            out.push(B64[(b2 & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
        i += 3;
    }
    out
}

fn decode_base64(input: &str) -> Result<Vec<u8>, String> {
    let filtered: Vec<u8> = input.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    if !filtered.len().is_multiple_of(4) {
        return Err("base64 length not a multiple of 4".to_string());
    }
    let mut out = Vec::with_capacity(filtered.len() / 4 * 3);
    for chunk in filtered.chunks(4) {
        let a = b64_val(chunk[0])?;
        let b = b64_val(chunk[1])?;
        let c = if chunk[2] == b'=' {
            0
        } else {
            b64_val(chunk[2])?
        };
        let d = if chunk[3] == b'=' {
            0
        } else {
            b64_val(chunk[3])?
        };
        out.push((a << 2) | (b >> 4));
        if chunk[2] != b'=' {
            out.push((b << 4) | (c >> 2));
        }
        if chunk[3] != b'=' {
            out.push((c << 6) | d);
        }
    }
    Ok(out)
}

fn b64_val(c: u8) -> Result<u8, String> {
    match c {
        b'A'..=b'Z' => Ok(c - b'A'),
        b'a'..=b'z' => Ok(c - b'a' + 26),
        b'0'..=b'9' => Ok(c - b'0' + 52),
        b'+' => Ok(62),
        b'/' => Ok(63),
        _ => Err(format!("invalid base64 byte {c}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attestation::{CostSummary, EvaluationSummary, RunOutcomeAttestation};
    use ed25519_dalek::SigningKey;
    use vam_security::ApprovalSigner;

    #[test]
    fn dsse_wrap_round_trips_attestation() {
        let signing_key = SigningKey::from_bytes(&[77u8; 32]);
        let signer = ApprovalSigner::new("node-key-dsse", signing_key);
        let attestation = RunOutcomeAttestation {
            run_id: "urn:vam:run:dsse-1".to_string(),
            session_id: "sess-dsse".to_string(),
            goal: "attest".to_string(),
            terminal_state: "SUCCEEDED".to_string(),
            terminator: "agent".to_string(),
            trajectory_digest: "abcd".to_string(),
            artifacts: vec![],
            paths: vec![],
            skills: vec![],
            tools: vec!["run".to_string()],
            eval: Some(EvaluationSummary {
                assertion_id: "a1".to_string(),
                evaluator_id: "e1".to_string(),
                passed: true,
            }),
            cost: Some(CostSummary {
                currency: "USD".to_string(),
                spend_micro: 1_250,
                prompt_tokens: 10,
                completion_tokens: 4,
                total_tokens: 14,
                pricing_catalog_version: Some("pricing-v1".to_string()),
            }),
            timestamp_unix_ms: 1_778_000_000_000,
            thread_log_head_seq: None,
            thread_log_head_digest: None,
            workspace: None,
        };
        let envelope = attestation.sign(&signer).expect("sign");
        let json = serde_json::to_vec(&attestation).expect("json");
        let dsse = wrap_attestation_dsse(&json, &envelope).expect("wrap");
        assert_eq!(dsse.payload_type, DSSE_PAYLOAD_TYPE);
        assert_eq!(dsse.signatures[0].keyid, "node-key-dsse");
        assert!(!dsse.payload.is_empty());
        assert!(!dsse.signatures[0].sig.is_empty());

        let recovered = unwrap_attestation_dsse(&dsse).expect("unwrap");
        assert_eq!(recovered, attestation);

        let value = dsse_to_value(&dsse).expect("value");
        assert_eq!(value["payloadType"], "application/vnd.vam.run-outcome+json");
    }

    #[test]
    fn encode_decode_base64_known_vector() {
        assert_eq!(encode_base64(b"f"), "Zg==");
        assert_eq!(encode_base64(b"fo"), "Zm8=");
        assert_eq!(encode_base64(b"foo"), "Zm9v");
        assert_eq!(decode_base64("Zm9v").unwrap(), b"foo");
    }
}
