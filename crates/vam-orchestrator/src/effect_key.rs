//! The idempotency key of one outside call (effect journal).
//!
//! `key = "vamfx_" + base32(first 16 bytes of HMAC-SHA256(K_effect, "vam-effect/v1" 0x00 run_id 0x00
//! call_id 0x00 args_digest))`, base32 being RFC 4648 lowercase without padding (26 characters).
//!
//! `K_effect` is derived from the node master (`signing.rs`) and never leaves the host. No input
//! depends on the session, lease, lifecycle generation, token or time, so a resumed generation
//! derives exactly the same key. The key is stored in the journal row, so a later master rotation
//! does not change the key of a call already recorded. `args_digest` is
//! `vam_spec::canonical_args_digest` (sorted keys, shared by host and guest), as bound by the
//! capability token.

use std::fmt;

use crate::signing::hmac_sha256;

/// Prefix of every key.
pub const KEY_PREFIX: &str = "vamfx_";
const KEY_DOMAIN: &[u8] = b"vam-effect/v1";
const BASE32: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";

/// The host-only secret keying effect keys. `Debug` prints nothing of it.
#[derive(Clone)]
pub struct EffectKeySecret([u8; 32]);

impl EffectKeySecret {
    pub(crate) fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// For tests with fixed vectors.
    #[cfg(test)]
    pub(crate) fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
}

impl fmt::Debug for EffectKeySecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("EffectKeySecret(..)")
    }
}

/// Why a key could not be derived.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EffectKeyError {
    /// An input was empty or held a NUL byte (the separator).
    BadInput(&'static str),
}

impl fmt::Display for EffectKeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadInput(name) => write!(f, "effect key input '{name}' is empty or holds NUL"),
        }
    }
}

impl std::error::Error for EffectKeyError {}

fn check(name: &'static str, value: &str) -> Result<(), EffectKeyError> {
    if value.is_empty() || value.as_bytes().contains(&0) {
        return Err(EffectKeyError::BadInput(name));
    }
    Ok(())
}

/// The key for the call `call_id` of run `run_id` with the arguments digest `args_digest`.
/// `call_id` is the model's id (`<call id>/<step id>` for a nested playbook step).
pub fn derive(
    secret: &EffectKeySecret,
    run_id: &str,
    call_id: &str,
    args_digest: &str,
) -> Result<String, EffectKeyError> {
    check("run_id", run_id)?;
    check("call_id", call_id)?;
    check("args_digest", args_digest)?;
    let mut message = Vec::with_capacity(KEY_DOMAIN.len() + run_id.len() + call_id.len() + 67);
    message.extend_from_slice(KEY_DOMAIN);
    message.push(0);
    message.extend_from_slice(run_id.as_bytes());
    message.push(0);
    message.extend_from_slice(call_id.as_bytes());
    message.push(0);
    message.extend_from_slice(args_digest.as_bytes());
    let tag = hmac_sha256(&secret.0, &message);
    Ok(format!("{KEY_PREFIX}{}", base32(&tag[..16])))
}

/// RFC 4648 base32, lowercase, no padding.
fn base32(bytes: &[u8]) -> String {
    let mut out = String::with_capacity((bytes.len() * 8).div_ceil(5));
    let (mut buffer, mut bits) = (0u32, 0u32);
    for byte in bytes {
        buffer = (buffer << 8) | u32::from(*byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(char::from(BASE32[((buffer >> bits) & 31) as usize]));
        }
        buffer &= (1 << bits) - 1;
    }
    if bits > 0 {
        out.push(char::from(BASE32[((buffer << (5 - bits)) & 31) as usize]));
    }
    out
}

/// First 8 characters of a key, for events and logs (never the whole key).
pub fn key_prefix(key: &str) -> &str {
    key.get(..8).unwrap_or(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secret() -> EffectKeySecret {
        EffectKeySecret::from_bytes(std::array::from_fn(|i| i as u8))
    }
    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    #[test]
    fn fixed_vectors() {
        let s = secret();
        assert_eq!(
            derive(&s, "run_1", "toolu_01", A).unwrap(),
            "vamfx_6r3eq22aqiq6qqoqr2d34iuhw4"
        );
        assert_eq!(
            derive(&s, "run_1", "toolu_01", B).unwrap(),
            "vamfx_mzisx5frcpmahqcqwpyjvu5ycq"
        );
        assert_eq!(
            derive(&s, "run_1", "toolu_01/step-a", A).unwrap(),
            "vamfx_rniaff2o36sfsv6ukxsqrc653q"
        );
        assert_eq!(
            derive(&s, "run_2", "toolu_01", A).unwrap(),
            "vamfx_duucvzkipl52ngtj4zfbbreula"
        );
    }

    #[test]
    fn same_inputs_same_key_and_one_byte_changes_it() {
        let s = secret();
        let k = derive(&s, "run_1", "c1", A).unwrap();
        assert_eq!(k, derive(&s, "run_1", "c1", A).unwrap());
        assert_eq!(k.len(), KEY_PREFIX.len() + 26);
        let other_digest = format!("{}b", &A[..63]);
        assert_ne!(k, derive(&s, "run_1", "c1", &other_digest).unwrap());
        assert_ne!(k, derive(&s, "run_1", "c2", A).unwrap());
        assert_ne!(k, derive(&s, "run_2", "c1", A).unwrap());
        let other = EffectKeySecret::from_bytes([9; 32]);
        assert_ne!(k, derive(&other, "run_1", "c1", A).unwrap());
    }

    #[test]
    fn separators_prevent_shifting_bytes_between_fields() {
        let s = secret();
        assert_ne!(
            derive(&s, "run_1a", "b", A).unwrap(),
            derive(&s, "run_1", "ab", A).unwrap()
        );
    }

    #[test]
    fn empty_or_nul_inputs_fail() {
        let s = secret();
        assert!(derive(&s, "", "c", A).is_err());
        assert!(derive(&s, "r", "c\0x", A).is_err());
        assert!(derive(&s, "r", "c", "").is_err());
    }

    #[test]
    fn base32_matches_rfc_4648_vectors() {
        assert_eq!(base32(b""), "");
        assert_eq!(base32(b"f"), "my");
        assert_eq!(base32(b"fo"), "mzxq");
        assert_eq!(base32(b"foobar"), "mzxw6ytboi");
    }

    #[test]
    fn key_is_stable_across_restarts_and_sessions() {
        // The same node master gives the same secret and key, whatever session or process asks.
        let dir = tempfile::tempdir().unwrap();
        let a = crate::signing::NodeKeys::load_or_create(dir.path()).unwrap();
        let ka = derive(&a.effect_key_secret(), "run_1", "c1", A).unwrap();
        let _ = a.session_signing_key("sess_1");
        let _ = a.session_signing_key("sess_2");
        let b = crate::signing::NodeKeys::load_or_create(dir.path()).unwrap();
        assert_eq!(
            ka,
            derive(&b.effect_key_secret(), "run_1", "c1", A).unwrap()
        );
        let c = crate::signing::NodeKeys::ephemeral();
        assert_ne!(
            ka,
            derive(&c.effect_key_secret(), "run_1", "c1", A).unwrap()
        );
    }

    #[test]
    fn debug_hides_the_secret() {
        assert_eq!(format!("{:?}", secret()), "EffectKeySecret(..)");
    }

    #[test]
    fn prefix_is_eight_characters() {
        assert_eq!(key_prefix("vamfx_6r3eq22aqiq6qqoqr2d34iuhw4"), "vamfx_6r");
    }
}
