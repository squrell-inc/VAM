//! The node's signing keys (E12 Phase 1, RR-1.4).
//!
//! Two secrets live in `<state dir>/signing/`, both created on first start with
//! mode 0600 (the directory 0700) by an atomic write, and read back on every
//! later start so a restart keeps the same keys:
//!
//! - `node-ed25519.key`: the Ed25519 seed that signs run receipts and run
//!   attestations. Key id `ed25519:<first 16 hex of sha256(public key)>`.
//! - `session-master.key`: 32 random bytes from which each session's own
//!   Ed25519 key is derived with HMAC-SHA256. That session key signs capability
//!   tokens and the audit ledger. It is separate from the node key so rotating
//!   the receipt key does not touch session tokens.
//!
//! `signing/retired/*.key|*.pub` hold keys that no longer sign. Only their
//! public halves are loaded and published, so old receipts stay verifiable by
//! key id. Rotation is: move `node-ed25519.key` into `retired/`, restart.
//!
//! Nothing here is ever sent to a guest, and the node API publishes public
//! keys only. There is no built-in or fallback key: with nothing installed,
//! [`session_signing_key`] fails and the session does not start.

use ed25519_dalek::SigningKey;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use thiserror::Error;
use vam_security::receipt::{key_id_for, ReceiptSigner};
use vam_spec::receipt::{hex_decode, hex_encode};
use vam_spec::v1::NodeSigningKey;

const DIR: &str = "signing";
const RETIRED_DIR: &str = "retired";
const NODE_KEY_FILE: &str = "node-ed25519.key";
const MASTER_FILE: &str = "session-master.key";
const SESSION_KEY_DOMAIN: &[u8] = b"vam.session-key.v1\0";

#[derive(Debug, Error)]
pub enum SigningError {
    #[error("cannot access {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("{path} is readable by other users (mode {mode:o}); run chmod 600 on it, or replace the key")]
    LooseMode { path: PathBuf, mode: u32 },
    #[error("{path} is not 64 hex characters (a 32-byte key)")]
    BadKeyFile { path: PathBuf },
    #[error("the node signing keys are not installed")]
    NotInstalled,
}

/// A published public key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicKeyInfo {
    pub key_id: String,
    /// Lowercase hex.
    pub public_key: String,
    /// `"current"` or `"retired"`.
    pub state: &'static str,
}

/// 32 random bytes that never leave the host; `Debug` prints nothing of them.
struct SessionMaster([u8; 32]);

impl std::fmt::Debug for SessionMaster {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SessionMaster(..)")
    }
}

#[derive(Debug)]
pub struct NodeKeys {
    current: ReceiptSigner,
    retired: Vec<PublicKeyInfo>,
    master: SessionMaster,
}

impl NodeKeys {
    /// Reads the keys under `<state_dir>/signing/`, creating the node key and
    /// the session master on first start.
    pub fn load_or_create(state_dir: &Path) -> Result<Self, SigningError> {
        let dir = state_dir.join(DIR);
        fs::create_dir_all(&dir).map_err(|e| io_err(&dir, e))?;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))
            .map_err(|e| io_err(&dir, e))?;

        let seed = load_or_create_secret(&dir.join(NODE_KEY_FILE))?;
        let master = load_or_create_secret(&dir.join(MASTER_FILE))?;
        let current = ReceiptSigner::new(SigningKey::from_bytes(&seed));
        let retired = load_retired(&dir.join(RETIRED_DIR), current.key_id())?;
        Ok(Self {
            current,
            retired,
            master: SessionMaster(master),
        })
    }

    /// Fresh keys that exist only in memory (tests).
    pub fn ephemeral() -> Self {
        Self {
            current: ReceiptSigner::new(SigningKey::from_bytes(&random_32())),
            retired: Vec::new(),
            master: SessionMaster(random_32()),
        }
    }

    /// The key that signs receipts and attestations.
    pub fn current(&self) -> &ReceiptSigner {
        &self.current
    }

    /// The current key, then the retired ones.
    pub fn public_keys(&self) -> Vec<PublicKeyInfo> {
        let mut keys = vec![PublicKeyInfo {
            key_id: self.current.key_id().to_string(),
            public_key: self.current.public_key_hex(),
            state: "current",
        }];
        keys.extend(self.retired.iter().cloned());
        keys
    }

    /// The same list as the `GET /v1/node` field.
    pub fn node_signing_keys(&self) -> Vec<NodeSigningKey> {
        self.public_keys()
            .into_iter()
            .map(|k| NodeSigningKey {
                key_id: k.key_id,
                public_key: k.public_key,
                state: k.state.to_string(),
            })
            .collect()
    }

    /// The Ed25519 key of one session, derived from the session master.
    pub fn session_signing_key(&self, session_id: &str) -> SigningKey {
        let mut message = Vec::with_capacity(SESSION_KEY_DOMAIN.len() + session_id.len());
        message.extend_from_slice(SESSION_KEY_DOMAIN);
        message.extend_from_slice(session_id.as_bytes());
        SigningKey::from_bytes(&hmac_sha256(&self.master.0, &message))
    }

    /// The host-only secret behind effect idempotency keys:
    /// `HMAC-SHA256(master, "vam-effect-key/v1")`. Never sent to a guest or a broker.
    pub fn effect_key_secret(&self) -> crate::effect_key::EffectKeySecret {
        crate::effect_key::EffectKeySecret::new(hmac_sha256(&self.master.0, EFFECT_KEY_DOMAIN))
    }

    /// The one line vamd prints at start: key ids only.
    pub fn startup_line(&self) -> String {
        let retired: Vec<&str> = self.retired.iter().map(|k| k.key_id.as_str()).collect();
        format!(
            "node signing key {} (retired: {})",
            self.current.key_id(),
            if retired.is_empty() {
                "none".to_string()
            } else {
                retired.join(", ")
            }
        )
    }
}

const EFFECT_KEY_DOMAIN: &[u8] = b"vam-effect-key/v1";

/// `HMAC-SHA256(key, message)`.
pub(crate) fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    // HMAC accepts a key of any length.
    match Hmac::<Sha256>::new_from_slice(key) {
        Ok(mut mac) => {
            mac.update(message);
            mac.finalize().into_bytes().into()
        }
        // Unreachable: HMAC takes a key of any length. Fail closed with a zero tag, which
        // no verifier accepts, rather than panic.
        Err(_) => [0u8; 32],
    }
}

fn io_err(path: &Path, source: io::Error) -> SigningError {
    SigningError::Io {
        path: path.to_path_buf(),
        source,
    }
}

fn random_32() -> [u8; 32] {
    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    bytes
}

/// Reads a 32-byte hex secret, or creates it (atomically, mode 0600) when the
/// file does not exist. An existing file that is loose or malformed is an
/// error and is never replaced.
fn load_or_create_secret(path: &Path) -> Result<[u8; 32], SigningError> {
    match fs::symlink_metadata(path) {
        Ok(_) => read_secret(path),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let secret = random_32();
            let mut text = hex_encode(&secret);
            text.push('\n');
            write_atomic(path, text.as_bytes(), 0o600)?;
            Ok(secret)
        }
        Err(e) => Err(io_err(path, e)),
    }
}

fn read_secret(path: &Path) -> Result<[u8; 32], SigningError> {
    let meta = fs::metadata(path).map_err(|e| io_err(path, e))?;
    let mode = meta.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(SigningError::LooseMode {
            path: path.to_path_buf(),
            mode,
        });
    }
    let text = fs::read_to_string(path).map_err(|e| io_err(path, e))?;
    hex_decode(text.trim())
        .ok()
        .and_then(|b| b.try_into().ok())
        .ok_or_else(|| SigningError::BadKeyFile {
            path: path.to_path_buf(),
        })
}

/// Public halves of `retired/*.key` (a seed, as moved there) and `retired/*.pub`
/// (a public key). Key ids come from the key bytes, never the file name.
fn load_retired(dir: &Path, current_id: &str) -> Result<Vec<PublicKeyInfo>, SigningError> {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(io_err(dir, e)),
    };
    let mut out: Vec<PublicKeyInfo> = Vec::new();
    for entry in entries {
        let path = entry.map_err(|e| io_err(dir, e))?.path();
        if !path.is_file() {
            continue;
        }
        let public: [u8; 32] = match path.extension().and_then(|e| e.to_str()) {
            Some("key") => SigningKey::from_bytes(&read_secret(&path)?)
                .verifying_key()
                .to_bytes(),
            Some("pub") => {
                let text = fs::read_to_string(&path).map_err(|e| io_err(&path, e))?;
                hex_decode(text.trim())
                    .ok()
                    .and_then(|b| b.try_into().ok())
                    .ok_or(SigningError::BadKeyFile { path: path.clone() })?
            }
            _ => continue,
        };
        let key_id = key_id_for(&public);
        if key_id == current_id || out.iter().any(|k| k.key_id == key_id) {
            continue;
        }
        out.push(PublicKeyInfo {
            key_id,
            public_key: hex_encode(&public),
            state: "retired",
        });
    }
    out.sort_by(|a, b| a.key_id.cmp(&b.key_id));
    Ok(out)
}

/// Writes `bytes` to a temp file created with `mode`, then renames it over `path`.
fn write_atomic(path: &Path, bytes: &[u8], mode: u32) -> Result<(), SigningError> {
    let tmp = path.with_extension("tmp");
    let _ = fs::remove_file(&tmp);
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(&tmp)
        .map_err(|e| io_err(&tmp, e))?;
    f.write_all(bytes).map_err(|e| io_err(&tmp, e))?;
    f.sync_all().map_err(|e| io_err(&tmp, e))?;
    drop(f);
    fs::rename(&tmp, path).map_err(|e| io_err(path, e))?;
    if let Some(parent) = path.parent() {
        // Make the rename durable; not every filesystem allows it, so ignore failure.
        if let Ok(d) = fs::File::open(parent) {
            let _ = d.sync_all();
        }
    }
    Ok(())
}

static INSTALLED: OnceLock<NodeKeys> = OnceLock::new();

/// Installs the node keys (once). Returns false if keys were already installed.
pub fn install(keys: NodeKeys) -> bool {
    INSTALLED.set(keys).is_ok()
}

/// The installed keys; `None` before vamd installs them.
pub fn node_keys() -> Option<&'static NodeKeys> {
    INSTALLED.get()
}

/// The signing key of `session_id`. Fails closed when no keys are installed.
pub fn session_signing_key(session_id: &str) -> Result<SigningKey, SigningError> {
    node_keys()
        .map(|k| k.session_signing_key(session_id))
        .ok_or(SigningError::NotInstalled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn mode(p: &Path) -> u32 {
        fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    fn hex32(b: &[u8; 32]) -> String {
        hex_encode(b)
    }

    // RFC 4231, test cases 1 and 2.
    #[test]
    fn hmac_sha256_matches_rfc_4231() {
        assert_eq!(
            hex_encode(&hmac_sha256(&[0x0b; 20], b"Hi There")),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
        assert_eq!(
            hex_encode(&hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn first_start_creates_the_keys_with_private_modes() {
        let d = tempfile::tempdir().unwrap();
        let keys = NodeKeys::load_or_create(d.path()).unwrap();
        let dir = d.path().join("signing");
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&dir.join("node-ed25519.key")), 0o600);
        assert_eq!(mode(&dir.join("session-master.key")), 0o600);
        assert!(keys.current().key_id().starts_with("ed25519:"));
        // No temp file is left behind.
        let names: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(names.iter().all(|n| !n.ends_with(".tmp")), "{names:?}");
    }

    #[test]
    fn a_restart_keeps_the_same_key_and_session_keys() {
        let d = tempfile::tempdir().unwrap();
        let first = NodeKeys::load_or_create(d.path()).unwrap();
        let second = NodeKeys::load_or_create(d.path()).unwrap();
        assert_eq!(first.current().key_id(), second.current().key_id());
        assert_eq!(
            first.session_signing_key("s1").to_bytes(),
            second.session_signing_key("s1").to_bytes()
        );
    }

    #[test]
    fn session_keys_differ_per_session_and_per_master() {
        let d1 = tempfile::tempdir().unwrap();
        let d2 = tempfile::tempdir().unwrap();
        let a = NodeKeys::load_or_create(d1.path()).unwrap();
        let b = NodeKeys::load_or_create(d2.path()).unwrap();
        assert_ne!(
            a.session_signing_key("s1").to_bytes(),
            a.session_signing_key("s2").to_bytes()
        );
        assert_ne!(
            a.session_signing_key("s1").to_bytes(),
            b.session_signing_key("s1").to_bytes()
        );
        assert_ne!(a.current().key_id(), b.current().key_id());
    }

    #[test]
    fn no_session_key_is_the_old_constant() {
        let keys = NodeKeys::ephemeral();
        let old = SigningKey::from_bytes(&[42u8; 32]).to_bytes();
        for id in ["s1", "s2", "host-key-s1", ""] {
            assert_ne!(keys.session_signing_key(id).to_bytes(), old);
        }
        assert_ne!(
            keys.current().public_key_hex(),
            hex_encode(
                &SigningKey::from_bytes(&[42u8; 32])
                    .verifying_key()
                    .to_bytes()
            )
        );
    }

    #[test]
    fn the_node_key_is_not_the_session_master() {
        let d = tempfile::tempdir().unwrap();
        NodeKeys::load_or_create(d.path()).unwrap();
        let dir = d.path().join("signing");
        let node = fs::read_to_string(dir.join("node-ed25519.key")).unwrap();
        let master = fs::read_to_string(dir.join("session-master.key")).unwrap();
        assert_ne!(node.trim(), master.trim());
        assert_eq!(node.trim().len(), 64);
        assert_eq!(master.trim().len(), 64);
    }

    #[test]
    fn a_key_file_readable_by_others_is_refused() {
        let d = tempfile::tempdir().unwrap();
        NodeKeys::load_or_create(d.path()).unwrap();
        let f = d.path().join("signing/node-ed25519.key");
        fs::set_permissions(&f, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(
            NodeKeys::load_or_create(d.path()),
            Err(SigningError::LooseMode { .. })
        ));
    }

    #[test]
    fn a_corrupt_key_file_is_an_error_and_is_not_overwritten() {
        let d = tempfile::tempdir().unwrap();
        NodeKeys::load_or_create(d.path()).unwrap();
        let f = d.path().join("signing/session-master.key");
        fs::write(&f, "not a key").unwrap();
        fs::set_permissions(&f, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(matches!(
            NodeKeys::load_or_create(d.path()),
            Err(SigningError::BadKeyFile { .. })
        ));
        assert_eq!(fs::read_to_string(&f).unwrap(), "not a key");
    }

    #[test]
    fn moved_keys_are_listed_as_retired_and_never_current() {
        let d = tempfile::tempdir().unwrap();
        let first = NodeKeys::load_or_create(d.path()).unwrap();
        let old_id = first.current().key_id().to_string();
        let old_pub = first.current().public_key_hex();
        // Rotation: move the file into retired/, restart.
        let retired = d.path().join("signing/retired");
        fs::create_dir_all(&retired).unwrap();
        fs::rename(
            d.path().join("signing/node-ed25519.key"),
            retired.join("2026-10.key"),
        )
        .unwrap();
        // A public-only retired key too.
        let other = SigningKey::from_bytes(&[3u8; 32]);
        let other_pub = other.verifying_key().to_bytes();
        fs::write(retired.join("older.pub"), hex32(&other_pub)).unwrap();

        let second = NodeKeys::load_or_create(d.path()).unwrap();
        assert_ne!(second.current().key_id(), old_id);
        let keys = second.public_keys();
        assert_eq!(keys[0].state, "current");
        let retired_ids: Vec<(&str, &str)> = keys[1..]
            .iter()
            .map(|k| (k.key_id.as_str(), k.state))
            .collect();
        assert!(retired_ids.contains(&(old_id.as_str(), "retired")));
        assert!(retired_ids.contains(&(key_id_for(&other_pub).as_str(), "retired")));
        assert!(keys.iter().any(|k| k.public_key == old_pub));
        assert_eq!(keys.len(), 3);
    }

    #[test]
    fn a_retired_key_id_comes_from_its_bytes_not_its_file_name() {
        let d = tempfile::tempdir().unwrap();
        NodeKeys::load_or_create(d.path()).unwrap();
        let retired = d.path().join("signing/retired");
        fs::create_dir_all(&retired).unwrap();
        let pubk = SigningKey::from_bytes(&[4u8; 32])
            .verifying_key()
            .to_bytes();
        fs::write(retired.join("ed25519:0000000000000000.pub"), hex32(&pubk)).unwrap();
        let keys = NodeKeys::load_or_create(d.path()).unwrap().public_keys();
        assert!(keys.iter().any(|k| k.key_id == key_id_for(&pubk)));
        assert!(!keys.iter().any(|k| k.key_id == "ed25519:0000000000000000"));
    }

    #[test]
    fn only_public_keys_are_published_and_logged() {
        let d = tempfile::tempdir().unwrap();
        let keys = NodeKeys::load_or_create(d.path()).unwrap();
        let dir = d.path().join("signing");
        let seed = fs::read_to_string(dir.join("node-ed25519.key")).unwrap();
        let master = fs::read_to_string(dir.join("session-master.key")).unwrap();
        let (seed, master) = (seed.trim(), master.trim());

        let published = serde_json::to_string(
            &keys
                .public_keys()
                .iter()
                .map(|k| (&k.key_id, &k.public_key, k.state))
                .collect::<Vec<_>>(),
        )
        .unwrap();
        let proto = format!("{:?}", keys.node_signing_keys());
        let line = keys.startup_line();
        let debug = format!("{keys:?}");
        for text in [&published, &proto, &line, &debug] {
            assert!(!text.contains(seed), "seed leaked into: {text}");
            assert!(!text.contains(master), "master leaked into: {text}");
        }
        // A derived session seed is a secret too.
        let derived = hex_encode(&keys.session_signing_key("s1").to_bytes());
        for text in [&published, &proto, &line, &debug] {
            assert!(!text.contains(&derived));
        }
        assert!(line.contains(keys.current().key_id()));
    }

    #[test]
    fn an_attestation_signed_by_the_node_key_verifies_with_the_listed_public_key() {
        let keys = NodeKeys::ephemeral();
        let listed = &keys.node_signing_keys()[0];
        assert_eq!(listed.state, "current");
        let envelope = keys.current().approval_signer().sign(
            vam_telemetry::attestation::RUN_OUTCOME_ATTESTATION_DOMAIN,
            b"{\"runId\":\"r\"}",
        );
        let public: [u8; 32] = hex_decode(&listed.public_key).unwrap().try_into().unwrap();
        let verifying = ed25519_dalek::VerifyingKey::from_bytes(&public).unwrap();
        vam_security::verify(
            vam_telemetry::attestation::RUN_OUTCOME_ATTESTATION_DOMAIN,
            &listed.key_id,
            &verifying,
            &envelope,
        )
        .expect("verifies against the key GET /v1/node lists");
        // The old constant's key does not.
        let old = SigningKey::from_bytes(&[42u8; 32]).verifying_key();
        assert!(vam_security::verify(
            vam_telemetry::attestation::RUN_OUTCOME_ATTESTATION_DOMAIN,
            &listed.key_id,
            &old,
            &envelope,
        )
        .is_err());
    }

    #[test]
    fn nothing_installed_means_no_session_key() {
        // The lib test binary never installs keys in this test; a session must not start.
        if node_keys().is_none() {
            assert!(matches!(
                session_signing_key("s1"),
                Err(SigningError::NotInstalled)
            ));
        }
    }
}
