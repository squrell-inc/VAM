//! Argon2id password hashing for control-plane users.
//!
//! The encoded PHC string stores algorithm, version, memory/time/parallelism,
//! salt, and digest. Plaintext is never persisted. Seeded `local` / `operator`
//! rows stay password-less until an operator reset (no SMTP).

use argon2::password_hash::{
    rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString,
};
use argon2::{Algorithm, Argon2, Params, Version};
use thiserror::Error;

use crate::error::OrchestratorError;

/// Env var holding the new password for `vamd --reset-password` (never logged).
pub const RESET_PASSWORD_ENV: &str = "VAMD_RESET_PASSWORD";
/// Optional env var naming the user to reset (CLI `--reset-password` wins).
pub const RESET_PASSWORD_USER_ENV: &str = "VAMD_RESET_PASSWORD_USER";
/// Optional username for headless first-admin bootstrap (`vamd --bootstrap-admin`).
pub const BOOTSTRAP_ADMIN_ENV: &str = "VAMD_BOOTSTRAP_ADMIN";
/// Password for headless first-admin bootstrap (never logged).
pub const BOOTSTRAP_ADMIN_PASSWORD_ENV: &str = "VAMD_BOOTSTRAP_ADMIN_PASSWORD";

/// Reject absurd inputs that would turn Argon2 into a DoS vector.
const MAX_PASSWORD_BYTES: usize = 1024;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum PasswordError {
    #[error("password must be non-empty")]
    Empty,
    #[error("password exceeds {MAX_PASSWORD_BYTES} bytes")]
    TooLong,
    #[error("password hashing failed")]
    Hash,
    #[error("password hash is not a valid argon2id PHC string")]
    InvalidHash,
}

impl From<PasswordError> for OrchestratorError {
    fn from(value: PasswordError) -> Self {
        match value {
            PasswordError::Empty => {
                OrchestratorError::InvalidContract("password must be non-empty".to_string())
            }
            PasswordError::TooLong => OrchestratorError::InvalidContract(format!(
                "password exceeds {MAX_PASSWORD_BYTES} bytes"
            )),
            PasswordError::Hash => OrchestratorError::PasswordHash,
            PasswordError::InvalidHash => OrchestratorError::InvalidPasswordHash,
        }
    }
}

/// Argon2id v19 with crate/OWASP defaults (m=19 MiB, t=2, p=1).
pub fn argon2id() -> Result<Argon2<'static>, PasswordError> {
    let params = Params::new(
        Params::DEFAULT_M_COST,
        Params::DEFAULT_T_COST,
        Params::DEFAULT_P_COST,
        None,
    )
    .map_err(|_| PasswordError::Hash)?;
    Ok(Argon2::new(Algorithm::Argon2id, Version::V0x13, params))
}

/// Hash a password to an argon2id PHC string. Callers must not log `password`.
pub fn hash_password(password: &str) -> Result<String, PasswordError> {
    validate_password(password)?;
    let salt = SaltString::generate(&mut OsRng);
    let encoded = argon2id()?
        .hash_password(password.as_bytes(), &salt)
        .map_err(|_| PasswordError::Hash)?;
    let phc = encoded.to_string();
    if !is_argon2id_phc(&phc) {
        return Err(PasswordError::Hash);
    }
    Ok(phc)
}

/// Verify `password` against an argon2id PHC string.
///
/// Returns `Ok(true)` on match, `Ok(false)` on mismatch. Malformed or
/// non-argon2id hashes are errors (they should never have been stored).
pub fn verify_password(password: &str, encoded: &str) -> Result<bool, PasswordError> {
    if password.is_empty() || encoded.is_empty() {
        return Ok(false);
    }
    if password.len() > MAX_PASSWORD_BYTES {
        return Ok(false);
    }
    if !is_argon2id_phc(encoded) {
        return Err(PasswordError::InvalidHash);
    }
    let parsed = PasswordHash::new(encoded).map_err(|_| PasswordError::InvalidHash)?;
    match argon2id()?.verify_password(password.as_bytes(), &parsed) {
        Ok(()) => Ok(true),
        Err(argon2::password_hash::Error::Password) => Ok(false),
        Err(_) => Err(PasswordError::InvalidHash),
    }
}

pub fn is_argon2id_phc(encoded: &str) -> bool {
    encoded.starts_with("$argon2id$")
}

/// Admin reset used by `vamd --reset-password` (any user, including operator).
/// No SMTP. Org-admin HTTP is `POST /v1/users/{id}/password` and cannot
/// reset operator.
pub fn apply_admin_password_reset(
    store: &crate::workflow::SqliteWorkflowStore,
    user_id_or_username: &str,
    new_password: &str,
) -> Result<crate::users::UserRecord, OrchestratorError> {
    store.reset_user_password(user_id_or_username, new_password)
}

/// Headless first-admin bootstrap used by `vamd --bootstrap-admin` and
/// `VAMD_BOOTSTRAP_ADMIN` / `VAMD_BOOTSTRAP_ADMIN_PASSWORD`. No SMTP, no UI.
pub fn apply_headless_admin_bootstrap(
    store: &crate::workflow::SqliteWorkflowStore,
    username: &str,
    password: &str,
) -> Result<crate::users::UserRecord, OrchestratorError> {
    store.bootstrap_first_admin(username, password)
}

/// Same rules for every password a caller sets: non-empty, at most 1024 bytes.
pub(crate) fn validate_password(password: &str) -> Result<(), PasswordError> {
    if password.is_empty() {
        return Err(PasswordError::Empty);
    }
    if password.len() > MAX_PASSWORD_BYTES {
        return Err(PasswordError::TooLong);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_hash_is_argon2id_and_omits_plaintext() {
        let secret = "correct-horse-battery";
        let encoded = hash_password(secret).expect("hash");
        assert!(
            is_argon2id_phc(&encoded),
            "expected argon2id PHC, got {encoded}"
        );
        assert!(
            !encoded.contains(secret),
            "PHC must not contain the plaintext password"
        );
        assert!(!encoded.to_lowercase().contains("correct-horse"));
    }

    #[test]
    fn password_verify_succeeds_for_matching_secret() {
        let secret = "verify-ok-secret";
        let encoded = hash_password(secret).expect("hash");
        assert!(verify_password(secret, &encoded).expect("verify"));
    }

    #[test]
    fn password_verify_fails_for_wrong_secret() {
        let encoded = hash_password("right-password").expect("hash");
        assert!(!verify_password("wrong-password", &encoded).expect("verify"));
    }

    #[test]
    fn password_verify_rejects_non_argon2id_hash() {
        let err = verify_password("x", "$argon2i$v=19$m=16,t=2,p=1$c2FsdHNhbHQ$dGVzdA")
            .expect_err("non-id algorithm");
        assert_eq!(err, PasswordError::InvalidHash);
    }

    #[test]
    fn password_hash_rejects_empty() {
        assert_eq!(hash_password(""), Err(PasswordError::Empty));
    }

    #[test]
    fn password_admin_reset_is_in_process_without_email() {
        let store = crate::workflow::SqliteWorkflowStore::in_memory().expect("store");
        let user = apply_admin_password_reset(&store, crate::users::LOCAL_USER_ID, "admin-set")
            .expect("admin reset");
        assert!(user.has_password);
        assert!(store
            .verify_user_password(crate::users::LOCAL_USER_ID, "admin-set")
            .expect("verify"));
    }

    #[test]
    fn password_headless_bootstrap_of_a_reserved_name_only_reuses_an_existing_admin() {
        // The `vamd --bootstrap-admin` restart path.
        let store = crate::workflow::SqliteWorkflowStore::in_memory().expect("store");
        assert!(apply_headless_admin_bootstrap(&store, "bootstrap", "bootstrap-secret").is_err());
        {
            let connection = store.connection.lock().expect("lock");
            connection
                .execute(
                    "INSERT INTO users (user_id, username, tenant_id, created_at, role)
                     VALUES ('bootstrap', 'bootstrap', 'bootstrap', 1, 'org_admin')",
                    [],
                )
                .expect("legacy admin");
        }
        store
            .reset_user_password("bootstrap", "bootstrap-secret")
            .expect("password");
        let user = apply_headless_admin_bootstrap(&store, "bootstrap", "bootstrap-secret")
            .expect("restart keeps the existing admin");
        assert_eq!(user.user_id, "bootstrap");
        assert!(user.has_password);
    }

    #[test]
    fn password_headless_bootstrap_creates_one_admin() {
        let store = crate::workflow::SqliteWorkflowStore::in_memory().expect("store");
        let user =
            apply_headless_admin_bootstrap(&store, "admin", "bootstrap-secret").expect("bootstrap");
        assert_eq!(user.user_id, "admin");
        assert!(user.has_password);
        assert!(store
            .verify_user_password("admin", "bootstrap-secret")
            .expect("verify"));
    }
}
