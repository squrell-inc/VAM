//! Sign and verify the run receipt (E12).
//!
//! A receipt document is `{receipt, modelAppendix, signature}`. The node signs
//! the canonical bytes of `receipt` (see [`vam_spec::receipt`]); the model
//! appendix is covered through `receipt.modelAppendixDigest`.
//!
//! Signed bytes: the crate's length-prefixed signing input over the domain
//! [`RECEIPT_DOMAIN`] and the claims `u32_be(len(keyId)) || keyId ||
//! canonical(receipt)`, so the key id is part of what is signed.
//!
//! [`verify_receipt`] works on the JSON it is given and never decodes through
//! the proto types: a field a newer node adds does not break an older verifier.

use crate::signing_input;
use ed25519_dalek::{Signature as Ed25519Signature, Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;
use vam_spec::receipt::{canonical_bytes, digest, hex_decode, hex_encode};

/// Signature domain; the same string as the receipt format version.
pub const RECEIPT_DOMAIN: &str = vam_spec::receipt::RECEIPT_VERSION;
/// The only algorithm a receipt signature may name.
pub const RECEIPT_ALGORITHM: &str = "ed25519";

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ReceiptError {
    #[error("receipt is not a canonical-able JSON document: {0}")]
    Canonical(String),
    #[error("receipt document is malformed: {0}")]
    Malformed(String),
    #[error("signature algorithm '{0}' is not supported")]
    UnsupportedAlgorithm(String),
    #[error("signature key id does not match its public key")]
    KeyIdMismatch,
    #[error("signature does not match the receipt (the receipt, its key id or the signature was changed)")]
    BadSignature,
    #[error("receipt digest does not match the receipt")]
    ReceiptDigest,
    #[error("model appendix does not match the digest the receipt signed")]
    AppendixDigest,
    #[error("signing key {0} is not among the trusted keys")]
    NotTrusted(String),
}

/// `ed25519:<first 16 hex of sha256(public key)>`.
pub fn key_id_for(public_key: &[u8; 32]) -> String {
    let hash = Sha256::digest(public_key);
    format!("ed25519:{}", hex_encode(&hash[..8]))
}

/// A node key that signs receipts. Holds the secret: it never leaves the host.
pub struct ReceiptSigner {
    key_id: String,
    signing_key: SigningKey,
}

impl std::fmt::Debug for ReceiptSigner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReceiptSigner")
            .field("key_id", &self.key_id)
            .finish_non_exhaustive()
    }
}

impl ReceiptSigner {
    pub fn new(signing_key: SigningKey) -> Self {
        let key_id = key_id_for(&signing_key.verifying_key().to_bytes());
        Self {
            key_id,
            signing_key,
        }
    }

    pub fn key_id(&self) -> &str {
        &self.key_id
    }

    pub fn verifying_key(&self) -> VerifyingKey {
        self.signing_key.verifying_key()
    }

    /// An [`crate::ApprovalSigner`] over the same key and id, for the signed
    /// envelopes that are not receipts (the run attestation).
    pub fn approval_signer(&self) -> crate::ApprovalSigner {
        crate::ApprovalSigner::new(self.key_id.clone(), self.signing_key.clone())
    }

    /// Lowercase hex of the public key.
    pub fn public_key_hex(&self) -> String {
        hex_encode(&self.signing_key.verifying_key().to_bytes())
    }
}

/// The `signature` object of a receipt document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReceiptSignature {
    pub algorithm: String,
    pub key_id: String,
    pub public_key: String,
    pub receipt_digest: String,
    pub value: String,
}

/// `sha256:<hex>` of the canonical form of the model appendix; the value the
/// host tier carries as `modelAppendixDigest`.
pub fn appendix_digest(appendix: &Value) -> Result<String, ReceiptError> {
    let bytes = canonical_bytes(appendix).map_err(|e| ReceiptError::Canonical(e.to_string()))?;
    Ok(digest(&bytes))
}

fn signed_claims(key_id: &str, canonical_receipt: &[u8]) -> Vec<u8> {
    let mut claims = Vec::with_capacity(4 + key_id.len() + canonical_receipt.len());
    claims.extend_from_slice(&(key_id.len() as u32).to_be_bytes());
    claims.extend_from_slice(key_id.as_bytes());
    claims.extend_from_slice(canonical_receipt);
    claims
}

/// Signs the host tier. `receipt` is the JSON object the node emits as
/// `receipt`; it should already carry `modelAppendixDigest`.
pub fn sign_receipt(
    signer: &ReceiptSigner,
    receipt: &Value,
) -> Result<ReceiptSignature, ReceiptError> {
    let canonical = canonical_bytes(receipt).map_err(|e| ReceiptError::Canonical(e.to_string()))?;
    let claims = signed_claims(&signer.key_id, &canonical);
    let signature = signer
        .signing_key
        .sign(&signing_input(RECEIPT_DOMAIN, &claims));
    Ok(ReceiptSignature {
        algorithm: RECEIPT_ALGORITHM.to_string(),
        key_id: signer.key_id.clone(),
        public_key: signer.public_key_hex(),
        receipt_digest: digest(&canonical),
        value: hex_encode(&signature.to_bytes()),
    })
}

/// A key a verifier trusts (from `vam node keys`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrustedKey {
    pub key_id: String,
    pub public_key: String,
}

/// Reads the keys of a `vam node keys` document: `{"signingKeys":[{keyId,publicKey,state}]}`.
pub fn trusted_keys_from_json(json: &[u8]) -> Result<Vec<TrustedKey>, ReceiptError> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Doc {
        signing_keys: Vec<TrustedKey>,
    }
    serde_json::from_slice::<Doc>(json)
        .map(|d| d.signing_keys)
        .map_err(|e| ReceiptError::Malformed(format!("signingKeys: {e}")))
}

/// What a passing verification proves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verified {
    pub key_id: String,
    pub public_key: String,
    /// True when the key was matched against a non-empty trusted list.
    pub pinned: bool,
}

/// Verifies a receipt document's JSON text. With an empty `trusted` list only
/// integrity is proved (the embedded key signed it); with keys, the signing
/// key must be one of them.
pub fn verify_receipt(json: &[u8], trusted: &[TrustedKey]) -> Result<Verified, ReceiptError> {
    let doc: Value = serde_json::from_slice(json)
        .map_err(|e| ReceiptError::Malformed(format!("not JSON: {e}")))?;
    let part = |name: &str| {
        doc.get(name)
            .filter(|v| v.is_object())
            .ok_or_else(|| ReceiptError::Malformed(format!("'{name}' object is missing")))
    };
    let receipt = part("receipt")?;
    let appendix = part("modelAppendix")?;
    let sig = part("signature")?;
    let field = |name: &str| {
        sig.get(name)
            .and_then(Value::as_str)
            .ok_or_else(|| ReceiptError::Malformed(format!("signature.{name} is missing")))
    };

    let algorithm = field("algorithm")?;
    if algorithm != RECEIPT_ALGORITHM {
        return Err(ReceiptError::UnsupportedAlgorithm(algorithm.to_string()));
    }
    let key_id = field("keyId")?;
    let public_key_hex = field("publicKey")?;
    let public_key: [u8; 32] = hex_decode(public_key_hex)
        .ok()
        .and_then(|b| b.try_into().ok())
        .ok_or_else(|| {
            ReceiptError::Malformed("signature.publicKey is not 32 bytes of hex".into())
        })?;
    let verifying_key = VerifyingKey::from_bytes(&public_key)
        .map_err(|_| ReceiptError::Malformed("signature.publicKey is not a valid key".into()))?;
    // The id is derived from the key, never trusted as given.
    if key_id != key_id_for(&public_key) {
        return Err(ReceiptError::KeyIdMismatch);
    }

    let canonical = canonical_bytes(receipt).map_err(|e| ReceiptError::Canonical(e.to_string()))?;
    let value = hex_decode(field("value")?)
        .ok()
        .and_then(|b| Ed25519Signature::from_slice(&b).ok())
        .ok_or_else(|| ReceiptError::Malformed("signature.value is not 64 bytes of hex".into()))?;
    verifying_key
        .verify_strict(
            &signing_input(RECEIPT_DOMAIN, &signed_claims(key_id, &canonical)),
            &value,
        )
        .map_err(|_| ReceiptError::BadSignature)?;

    if field("receiptDigest")? != digest(&canonical) {
        return Err(ReceiptError::ReceiptDigest);
    }
    let signed_appendix_digest = receipt
        .get("modelAppendixDigest")
        .and_then(Value::as_str)
        .ok_or_else(|| ReceiptError::Malformed("receipt.modelAppendixDigest is missing".into()))?;
    if signed_appendix_digest != appendix_digest(appendix)? {
        return Err(ReceiptError::AppendixDigest);
    }

    let pinned = !trusted.is_empty();
    if pinned
        && !trusted
            .iter()
            .any(|t| t.key_id == key_id && t.public_key.eq_ignore_ascii_case(public_key_hex))
    {
        return Err(ReceiptError::NotTrusted(key_id.to_string()));
    }
    Ok(Verified {
        key_id: key_id.to_string(),
        public_key: public_key_hex.to_string(),
        pinned,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn signer() -> ReceiptSigner {
        ReceiptSigner::new(SigningKey::from_bytes(&[7u8; 32]))
    }

    fn appendix() -> Value {
        json!({"label": "written by the model, not verified by the host", "finalReply": "done"})
    }

    fn receipt() -> Value {
        json!({
            "header": {"version": "vam.receipt.v1", "runId": "run-1", "generation": 1},
            "effects": {"journaled": false, "statement": "effects not journaled on this version"},
            "modelAppendixDigest": appendix_digest(&appendix()).unwrap(),
        })
    }

    fn document(signer: &ReceiptSigner) -> Value {
        let receipt = receipt();
        let sig = sign_receipt(signer, &receipt).unwrap();
        json!({"receipt": receipt, "modelAppendix": appendix(), "signature": serde_json::to_value(sig).unwrap()})
    }

    fn bytes(v: &Value) -> Vec<u8> {
        serde_json::to_vec(v).unwrap()
    }

    #[test]
    fn key_id_is_first_16_hex_of_sha256_of_the_public_key() {
        let id = signer().key_id().to_string();
        assert!(id.starts_with("ed25519:"));
        assert_eq!(id.len(), "ed25519:".len() + 16);
        let pk = signer().verifying_key().to_bytes();
        let full = hex_encode(&Sha256::digest(pk));
        assert_eq!(id, format!("ed25519:{}", &full[..16]));
    }

    #[test]
    fn golden_vector_fixed_key_gives_fixed_signature() {
        let s = signer();
        let sig = sign_receipt(&s, &receipt()).unwrap();
        assert_eq!(sig.algorithm, "ed25519");
        assert_eq!(sig.key_id, GOLDEN_KEY_ID);
        assert_eq!(sig.public_key, GOLDEN_PUBLIC_KEY);
        assert_eq!(sig.receipt_digest, GOLDEN_RECEIPT_DIGEST);
        assert_eq!(sig.value, GOLDEN_SIGNATURE);
    }

    // Computed independently (Python `cryptography`, key seed [7; 32]) from the
    // signing input in the module docs, not by this code.
    const GOLDEN_KEY_ID: &str = "ed25519:fe812c12f3ab4ce6";
    const GOLDEN_PUBLIC_KEY: &str =
        "ea4a6c63e29c520abef5507b132ec5f9954776aebebe7b92421eea691446d22c";
    const GOLDEN_RECEIPT_DIGEST: &str =
        "sha256:3cd6ffff474528bad7c1075b8c1e02d5a86e911b55ccb51c9dd760a141e45fbd";
    const GOLDEN_SIGNATURE: &str = "6d99fdc6341b8f5a303d2a5c9a79c236aa199e1d4df9556b2afd41dcd67fc22dd29cdd1ab29a583de55b294d26a95da0a3cc4baf070ee62c4fa8db9629ef770e";

    #[test]
    fn a_signed_document_verifies_unpinned() {
        let doc = document(&signer());
        let v = verify_receipt(&bytes(&doc), &[]).unwrap();
        assert_eq!(v.key_id, signer().key_id());
        assert!(!v.pinned);
    }

    #[test]
    fn reordered_keys_and_whitespace_still_verify() {
        let doc = document(&signer());
        let pretty = serde_json::to_vec_pretty(&doc).unwrap();
        verify_receipt(&pretty, &[]).unwrap();
        // Reverse the top-level order by hand.
        let reordered = format!(
            "{{ \"signature\": {}, \"modelAppendix\": {}, \"receipt\": {} }}",
            doc["signature"], doc["modelAppendix"], doc["receipt"]
        );
        verify_receipt(reordered.as_bytes(), &[]).unwrap();
    }

    #[test]
    fn one_changed_byte_in_receipt_fails() {
        let mut doc = document(&signer());
        doc["receipt"]["header"]["runId"] = json!("run-2");
        assert_eq!(
            verify_receipt(&bytes(&doc), &[]),
            Err(ReceiptError::BadSignature)
        );
        // And a single flipped character in the raw text.
        let text = String::from_utf8(bytes(&document(&signer()))).unwrap();
        let tampered = text.replacen("run-1", "run-7", 1);
        assert_eq!(
            verify_receipt(tampered.as_bytes(), &[]),
            Err(ReceiptError::BadSignature)
        );
    }

    fn harness_document(signer: &ReceiptSigner) -> Value {
        let mut receipt = receipt();
        receipt["harness"] = json!({
            "profile": "base",
            "version": "0.1.0",
            "digest": "sha256:aa",
            "overrides": {"loop": {"detect": true}},
        });
        let sig = sign_receipt(signer, &receipt).unwrap();
        json!({"receipt": receipt, "modelAppendix": appendix(), "signature": serde_json::to_value(sig).unwrap()})
    }

    #[test]
    fn a_receipt_with_a_harness_verifies_and_a_tamper_of_it_fails() {
        let doc = harness_document(&signer());
        verify_receipt(&bytes(&doc), &[]).unwrap();
        for tamper in [
            ("overrides", json!({"loop": {"detect": false}})),
            ("profile", json!("frontier")),
            ("digest", json!("sha256:bb")),
        ] {
            let mut bad = doc.clone();
            bad["receipt"]["harness"][tamper.0] = tamper.1;
            assert_eq!(
                verify_receipt(&bytes(&bad), &[]),
                Err(ReceiptError::BadSignature),
                "{}",
                tamper.0
            );
        }
        // Dropping the whole object also breaks the signature.
        let mut dropped = doc.clone();
        dropped["receipt"]
            .as_object_mut()
            .unwrap()
            .remove("harness");
        assert_eq!(
            verify_receipt(&bytes(&dropped), &[]),
            Err(ReceiptError::BadSignature)
        );
    }

    #[test]
    fn a_receipt_stored_before_the_harness_field_still_verifies() {
        // `document` has no `harness` key, as every receipt issued before H-0.3.
        verify_receipt(&bytes(&document(&signer())), &[]).unwrap();
    }

    #[test]
    fn a_changed_appendix_fails_by_digest() {
        let mut doc = document(&signer());
        doc["modelAppendix"]["finalReply"] = json!("something else");
        assert_eq!(
            verify_receipt(&bytes(&doc), &[]),
            Err(ReceiptError::AppendixDigest)
        );
    }

    #[test]
    fn a_changed_receipt_digest_field_fails() {
        let mut doc = document(&signer());
        doc["signature"]["receiptDigest"] = json!("sha256:00");
        assert_eq!(
            verify_receipt(&bytes(&doc), &[]),
            Err(ReceiptError::ReceiptDigest)
        );
    }

    #[test]
    fn a_wrong_key_id_fails() {
        let mut doc = document(&signer());
        doc["signature"]["keyId"] = json!("ed25519:0000000000000000");
        assert_eq!(
            verify_receipt(&bytes(&doc), &[]),
            Err(ReceiptError::KeyIdMismatch)
        );
    }

    #[test]
    fn a_key_swapped_for_the_attackers_own_fails() {
        // The attacker re-signs with their key and fixes the key id to match.
        let attacker = ReceiptSigner::new(SigningKey::from_bytes(&[9u8; 32]));
        let doc = document(&attacker);
        // Unpinned it verifies (integrity only) ...
        verify_receipt(&bytes(&doc), &[]).unwrap();
        // ... pinned to the real node key it does not.
        let trusted = [TrustedKey {
            key_id: signer().key_id().into(),
            public_key: signer().public_key_hex(),
        }];
        assert!(matches!(
            verify_receipt(&bytes(&doc), &trusted),
            Err(ReceiptError::NotTrusted(_))
        ));
    }

    #[test]
    fn pinned_to_the_right_key_passes() {
        let doc = document(&signer());
        let trusted = [TrustedKey {
            key_id: signer().key_id().into(),
            public_key: signer().public_key_hex(),
        }];
        let v = verify_receipt(&bytes(&doc), &trusted).unwrap();
        assert!(v.pinned);
    }

    #[test]
    fn a_trusted_entry_with_the_right_id_but_another_public_key_fails() {
        let doc = document(&signer());
        let trusted = [TrustedKey {
            key_id: signer().key_id().into(),
            public_key: ReceiptSigner::new(SigningKey::from_bytes(&[9u8; 32])).public_key_hex(),
        }];
        assert!(matches!(
            verify_receipt(&bytes(&doc), &trusted),
            Err(ReceiptError::NotTrusted(_))
        ));
    }

    #[test]
    fn a_changed_signature_value_fails() {
        let mut doc = document(&signer());
        let mut v = doc["signature"]["value"].as_str().unwrap().to_string();
        let first = if v.starts_with('0') { "1" } else { "0" };
        v.replace_range(0..1, first);
        doc["signature"]["value"] = json!(v);
        assert_eq!(
            verify_receipt(&bytes(&doc), &[]),
            Err(ReceiptError::BadSignature)
        );
    }

    #[test]
    fn an_unknown_algorithm_is_refused() {
        let mut doc = document(&signer());
        doc["signature"]["algorithm"] = json!("rsa");
        assert!(matches!(
            verify_receipt(&bytes(&doc), &[]),
            Err(ReceiptError::UnsupportedAlgorithm(_))
        ));
    }

    #[test]
    fn a_newer_field_does_not_break_an_old_verifier() {
        let s = signer();
        let mut receipt = receipt();
        receipt["somethingFromTheFuture"] = json!({"nested": [1, 2, 3]});
        let sig = sign_receipt(&s, &receipt).unwrap();
        let mut doc = json!({"receipt": receipt, "modelAppendix": appendix(), "signature": serde_json::to_value(sig).unwrap()});
        doc["topLevelFromTheFuture"] = json!(true);
        verify_receipt(&bytes(&doc), &[]).unwrap();
    }

    #[test]
    fn the_signature_is_domain_separated() {
        // The same claims signed under the capability-token domain must not verify as a receipt.
        let s = signer();
        let receipt = receipt();
        let canonical = canonical_bytes(&receipt).unwrap();
        let claims = signed_claims(s.key_id(), &canonical);
        let wrong = s.signing_key.sign(&signing_input(
            crate::token::TOOL_CAPABILITY_DOMAIN,
            &claims,
        ));
        let mut doc = document(&s);
        doc["signature"]["value"] = json!(hex_encode(&wrong.to_bytes()));
        assert_eq!(
            verify_receipt(&bytes(&doc), &[]),
            Err(ReceiptError::BadSignature)
        );
    }

    #[test]
    fn the_key_id_is_inside_the_signed_bytes() {
        // Signing a different key id's claims with the same key and presenting
        // it under the real key id does not verify.
        let s = signer();
        let receipt = receipt();
        let canonical = canonical_bytes(&receipt).unwrap();
        let claims = signed_claims("ed25519:ffffffffffffffff", &canonical);
        let sig = s.signing_key.sign(&signing_input(RECEIPT_DOMAIN, &claims));
        let mut doc = document(&s);
        doc["signature"]["value"] = json!(hex_encode(&sig.to_bytes()));
        assert_eq!(
            verify_receipt(&bytes(&doc), &[]),
            Err(ReceiptError::BadSignature)
        );
    }

    #[test]
    fn missing_parts_and_floats_are_refused() {
        assert!(matches!(
            verify_receipt(b"{}", &[]),
            Err(ReceiptError::Malformed(_))
        ));
        assert!(matches!(
            verify_receipt(b"not json", &[]),
            Err(ReceiptError::Malformed(_))
        ));
        let mut doc = document(&signer());
        doc["receipt"]["header"]["generation"] = json!(1.5);
        assert!(matches!(
            verify_receipt(&bytes(&doc), &[]),
            Err(ReceiptError::Canonical(_))
        ));
    }

    #[test]
    fn reads_the_keys_a_node_prints() {
        let s = signer();
        let doc = json!({"signingKeys": [
            {"keyId": s.key_id(), "publicKey": s.public_key_hex(), "state": "current"},
            {"keyId": "ed25519:aaaaaaaaaaaaaaaa", "publicKey": "00", "state": "retired"}
        ]});
        let keys = trusted_keys_from_json(&bytes(&doc)).unwrap();
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0].key_id, s.key_id());
        assert!(matches!(
            trusted_keys_from_json(b"{}"),
            Err(ReceiptError::Malformed(_))
        ));
    }

    #[test]
    fn the_signer_debug_output_has_no_secret() {
        let text = format!("{:?}", signer());
        assert!(text.contains("ed25519:"));
        assert!(!text.contains("0707"));
    }
}
