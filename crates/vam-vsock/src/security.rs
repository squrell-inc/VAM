use ed25519_dalek::VerifyingKey;
use rand::RngCore;
use std::collections::HashMap;
use std::sync::Mutex;
use vam_spec::v1::SignedEnvelope;

/// In-memory, thread-safe single-use challenge registry.
pub struct ChallengeRegistry {
    challenges: Mutex<HashMap<String, [u8; 32]>>,
}

impl Default for ChallengeRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ChallengeRegistry {
    pub fn new() -> Self {
        Self {
            challenges: Mutex::new(HashMap::new()),
        }
    }

    pub fn generate_challenge(&self, instance_id: &str) -> [u8; 32] {
        let mut challenge = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut challenge);
        let mut lock = self.challenges.lock().unwrap();
        lock.insert(instance_id.to_string(), challenge);
        challenge
    }

    pub fn consume_challenge(&self, instance_id: &str) -> Option<[u8; 32]> {
        let mut lock = self.challenges.lock().unwrap();
        lock.remove(instance_id)
    }
}

/// Seam to verify guest-reported attestations on the host.
pub trait AttestationVerifier: Send + Sync {
    fn verify(&self, instance_id: &str, attestation: &SignedEnvelope) -> Result<(), String>;
}

/// Weak dev-only verifier that bypasses cryptographic checks.
pub struct DevChallengeOnlyVerifier;

impl AttestationVerifier for DevChallengeOnlyVerifier {
    fn verify(&self, _instance_id: &str, _attestation: &SignedEnvelope) -> Result<(), String> {
        Ok(())
    }
}

/// Strong production verifier verifying an Ed25519 signature on the payload.
pub struct Ed25519AttestationVerifier {
    pub expected_key_id: String,
    pub expected_key: VerifyingKey,
}

impl AttestationVerifier for Ed25519AttestationVerifier {
    fn verify(&self, _instance_id: &str, attestation: &SignedEnvelope) -> Result<(), String> {
        vam_security::verify(
            "vam.v1.guest-registration",
            &self.expected_key_id,
            &self.expected_key,
            attestation,
        )
        .map_err(|e| format!("Attestation signature verification failed: {:?}", e))
    }
}
