//! Short-lived control-plane session tokens issued by `POST /v1/login`.
//!
//! Access tokens (`vam_st_…`) and refresh tokens (`vam_rt_…`) are stored as
//! SHA-256 hashes only. There is no OIDC/SAML/SCIM and no SMTP.

use rand::RngCore;
use rusqlite::{params, OptionalExtension};

use crate::error::OrchestratorError;
use crate::keys::{
    default_scopes_for_role, hash_api_key_secret, ApiKeyRecord, AuthSource, AuthenticatedKey,
};
use crate::users::UserRecord;
use crate::workflow::SqliteWorkflowStore;

/// Access-token prefix (Bearer value). Distinct from API keys (`vam_sk_…`).
pub const ACCESS_TOKEN_PREFIX: &str = "vam_st_";
/// Refresh-token prefix. Not valid as a Bearer credential.
pub const REFRESH_TOKEN_PREFIX: &str = "vam_rt_";

/// Default access-token lifetime (1 hour).
pub const DEFAULT_ACCESS_TTL_SECS: i64 = 3600;
/// Default refresh-token lifetime (7 days).
pub const DEFAULT_REFRESH_TTL_SECS: i64 = 7 * 24 * 3600;

/// Override access TTL (seconds, must be > 0).
pub const ENV_ACCESS_TTL: &str = "VAMD_SESSION_TTL_SECS";
/// Override refresh TTL (seconds, must be > 0).
pub const ENV_REFRESH_TTL: &str = "VAMD_REFRESH_TTL_SECS";

const KIND_ACCESS: &str = "access";
const KIND_REFRESH: &str = "refresh";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssuedSession {
    pub user_id: String,
    pub username: String,
    pub token: String,
    pub expires_at: i64,
    pub refresh_token: String,
    pub refresh_expires_at: i64,
    pub token_id: String,
    pub family_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionTokenRecord {
    pub token_id: String,
    pub family_id: String,
    pub user_id: String,
    pub sha256_secret: String,
    pub kind: String,
    pub created_at: i64,
    pub expires_at: i64,
    pub last_used_at: Option<i64>,
    pub revoked_at: Option<i64>,
}

pub fn access_ttl_secs() -> i64 {
    ttl_from_env(ENV_ACCESS_TTL, DEFAULT_ACCESS_TTL_SECS)
}

pub fn refresh_ttl_secs() -> i64 {
    ttl_from_env(ENV_REFRESH_TTL, DEFAULT_REFRESH_TTL_SECS)
}

fn ttl_from_env(name: &str, default: i64) -> i64 {
    match std::env::var(name) {
        Ok(raw) => raw
            .parse::<i64>()
            .ok()
            .filter(|v| *v > 0)
            .unwrap_or(default),
        Err(_) => default,
    }
}

impl SqliteWorkflowStore {
    pub fn initialize_session_tokens_schema(&self) -> Result<(), OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        connection.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS session_tokens (
                token_id TEXT PRIMARY KEY NOT NULL,
                family_id TEXT NOT NULL,
                user_id TEXT NOT NULL,
                sha256_secret TEXT NOT NULL UNIQUE,
                kind TEXT NOT NULL CHECK(kind IN ('access', 'refresh')),
                created_at INTEGER NOT NULL,
                expires_at INTEGER NOT NULL,
                last_used_at INTEGER,
                revoked_at INTEGER,
                FOREIGN KEY (user_id) REFERENCES users(user_id)
            );
            CREATE INDEX IF NOT EXISTS idx_session_tokens_family ON session_tokens(family_id);
            CREATE INDEX IF NOT EXISTS idx_session_tokens_hash ON session_tokens(sha256_secret);
            CREATE INDEX IF NOT EXISTS idx_session_tokens_user ON session_tokens(user_id);
            ",
        )?;
        Ok(())
    }

    /// Exchange username + password for a short-lived session. Wrong secret,
    /// unknown user, and password-less rows all return `Unauthenticated` (401).
    /// Callers must not log `password`.
    pub fn login_with_password(
        &self,
        username: &str,
        password: &str,
    ) -> Result<IssuedSession, OrchestratorError> {
        let user = match self.resolve_user(username.trim()) {
            Ok(user) => user,
            Err(OrchestratorError::NotFound(_)) => return Err(OrchestratorError::Unauthenticated),
            Err(e) => return Err(e),
        };
        crate::users::reject_deactivated_login(&user)?;
        match self.verify_user_password(&user.user_id, password) {
            Ok(true) => self.issue_session(&user, access_ttl_secs(), refresh_ttl_secs()),
            Ok(false) => Err(OrchestratorError::Unauthenticated),
            Err(e) => Err(e),
        }
    }

    pub fn issue_session(
        &self,
        user: &UserRecord,
        access_ttl_secs: i64,
        refresh_ttl_secs: i64,
    ) -> Result<IssuedSession, OrchestratorError> {
        let now = unix_timestamp();
        let family_id = mint_id("fam");
        let access_id = mint_id("sess");
        let refresh_id = mint_id("ref");
        let access_secret = mint_token_secret(ACCESS_TOKEN_PREFIX);
        let refresh_secret = mint_token_secret(REFRESH_TOKEN_PREFIX);
        let access_expires = now.saturating_add(access_ttl_secs);
        let refresh_expires = now.saturating_add(refresh_ttl_secs);

        self.insert_token_row(
            &access_id,
            &family_id,
            &user.user_id,
            &access_secret,
            KIND_ACCESS,
            now,
            access_expires,
        )?;
        self.insert_token_row(
            &refresh_id,
            &family_id,
            &user.user_id,
            &refresh_secret,
            KIND_REFRESH,
            now,
            refresh_expires,
        )?;

        Ok(IssuedSession {
            user_id: user.user_id.clone(),
            username: user.username.clone(),
            token: access_secret,
            expires_at: access_expires,
            refresh_token: refresh_secret,
            refresh_expires_at: refresh_expires,
            token_id: access_id,
            family_id,
        })
    }

    pub fn refresh_session(
        &self,
        refresh_secret: &str,
    ) -> Result<IssuedSession, OrchestratorError> {
        let record = self.lookup_token_secret(refresh_secret)?;
        let now = unix_timestamp();
        if record.kind != KIND_REFRESH {
            return Err(OrchestratorError::Unauthenticated);
        }
        if record.revoked_at.is_some() || record.expires_at <= now {
            return Err(OrchestratorError::Unauthenticated);
        }
        let user = match self.get_user(&record.user_id) {
            Ok(user) => user,
            Err(OrchestratorError::NotFound(_)) => return Err(OrchestratorError::Unauthenticated),
            Err(e) => return Err(e),
        };
        crate::users::reject_deactivated_login(&user)?;
        self.revoke_session_family(&record.family_id)?;
        self.issue_session(&user, access_ttl_secs(), refresh_ttl_secs())
    }

    pub fn revoke_session_family(&self, family_id: &str) -> Result<u64, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let now = unix_timestamp();
        let changed = connection.execute(
            "UPDATE session_tokens SET revoked_at = ?1
             WHERE family_id = ?2 AND revoked_at IS NULL",
            params![now, family_id],
        )?;
        Ok(changed as u64)
    }

    /// Revoke every live session token for `user_id`. Used on deactivate
    /// and password reset.
    pub fn revoke_sessions_for_user(&self, user_id: &str) -> Result<u64, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let now = unix_timestamp();
        let changed = connection.execute(
            "UPDATE session_tokens SET revoked_at = ?1
             WHERE user_id = ?2 AND revoked_at IS NULL",
            params![now, user_id],
        )?;
        Ok(changed as u64)
    }

    pub fn revoke_session_by_token_id(&self, token_id: &str) -> Result<u64, OrchestratorError> {
        let family_id = {
            let connection = self
                .connection
                .lock()
                .map_err(|_| OrchestratorError::StorageUnavailable)?;
            connection
                .query_row(
                    "SELECT family_id FROM session_tokens WHERE token_id = ?1",
                    [token_id],
                    |row| row.get::<_, String>(0),
                )
                .optional()?
        };
        match family_id {
            Some(family_id) => self.revoke_session_family(&family_id),
            None => Ok(0),
        }
    }

    /// Authenticate a Bearer access token. Refresh tokens are rejected.
    pub fn authenticate_session_bearer(
        &self,
        secret: &str,
    ) -> Result<AuthenticatedKey, OrchestratorError> {
        let record = self.lookup_token_secret(secret)?;
        let now = unix_timestamp();
        if record.kind != KIND_ACCESS {
            return Err(OrchestratorError::Unauthenticated);
        }
        if record.revoked_at.is_some() {
            return Err(OrchestratorError::Unauthenticated);
        }
        if record.expires_at <= now {
            return Err(OrchestratorError::Unauthenticated);
        }
        let user = match self.get_user(&record.user_id) {
            Ok(user) => user,
            Err(OrchestratorError::NotFound(_)) => return Err(OrchestratorError::Unauthenticated),
            Err(e) => return Err(e),
        };
        crate::users::reject_deactivated_user(&user)?;

        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        connection.execute(
            "UPDATE session_tokens SET last_used_at = ?1 WHERE token_id = ?2",
            params![now, record.token_id],
        )?;
        drop(connection);

        Ok(session_authenticated_key(&record, &user))
    }

    pub fn lookup_session_token(
        &self,
        token_id: &str,
    ) -> Result<SessionTokenRecord, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        connection
            .query_row(
                "SELECT token_id, family_id, user_id, sha256_secret, kind,
                        created_at, expires_at, last_used_at, revoked_at
                 FROM session_tokens WHERE token_id = ?1",
                [token_id],
                parse_session_token_row,
            )
            .map_err(|e| {
                if e == rusqlite::Error::QueryReturnedNoRows {
                    OrchestratorError::NotFound(format!("session token '{token_id}' not found"))
                } else {
                    OrchestratorError::Database(e)
                }
            })
    }

    fn lookup_token_secret(&self, secret: &str) -> Result<SessionTokenRecord, OrchestratorError> {
        if secret.is_empty() {
            return Err(OrchestratorError::Unauthenticated);
        }
        let hash = hash_api_key_secret(secret);
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        connection
            .query_row(
                "SELECT token_id, family_id, user_id, sha256_secret, kind,
                        created_at, expires_at, last_used_at, revoked_at
                 FROM session_tokens WHERE sha256_secret = ?1",
                [hash],
                parse_session_token_row,
            )
            .optional()?
            .ok_or(OrchestratorError::Unauthenticated)
    }

    #[allow(clippy::too_many_arguments)] // flat parameter list is the established call shape; a params struct would change callers
    fn insert_token_row(
        &self,
        token_id: &str,
        family_id: &str,
        user_id: &str,
        secret: &str,
        kind: &str,
        created_at: i64,
        expires_at: i64,
    ) -> Result<(), OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let hash = hash_api_key_secret(secret);
        connection.execute(
            "INSERT INTO session_tokens (
                token_id, family_id, user_id, sha256_secret, kind,
                created_at, expires_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![token_id, family_id, user_id, hash, kind, created_at, expires_at],
        )?;
        Ok(())
    }
}

fn session_authenticated_key(record: &SessionTokenRecord, user: &UserRecord) -> AuthenticatedKey {
    let scopes = default_scopes_for_role(user.role);
    AuthenticatedKey {
        record: ApiKeyRecord {
            key_id: record.token_id.clone(),
            name: "session".to_string(),
            sha256_secret: record.sha256_secret.clone(),
            principal: format!("User::\"{}\"", user.username),
            scopes,
            created_at: record.created_at,
            last_used_at: record.last_used_at,
            revoked_at: record.revoked_at,
            owner_user_id: user.user_id.clone(),
            ..Default::default()
        },
        source: AuthSource::Session,
        role: user.role,
    }
}

fn parse_session_token_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SessionTokenRecord> {
    Ok(SessionTokenRecord {
        token_id: row.get(0)?,
        family_id: row.get(1)?,
        user_id: row.get(2)?,
        sha256_secret: row.get(3)?,
        kind: row.get(4)?,
        created_at: row.get(5)?,
        expires_at: row.get(6)?,
        last_used_at: row.get(7)?,
        revoked_at: row.get(8)?,
    })
}

fn mint_token_secret(prefix: &str) -> String {
    let mut bytes = [0u8; 24];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    let mut out = String::with_capacity(prefix.len() + bytes.len() * 2);
    out.push_str(prefix);
    out.push_str(&to_hex(&bytes));
    out
}

fn mint_id(prefix: &str) -> String {
    let mut bytes = [0u8; 8];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    let mut out = String::with_capacity(prefix.len() + 1 + 16);
    out.push_str(prefix);
    out.push('_');
    out.push_str(&to_hex(&bytes));
    out
}

fn to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

fn unix_timestamp() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::users::LOCAL_USER_ID;

    fn user_with_password(secret: &str) -> (SqliteWorkflowStore, UserRecord) {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        store
            .reset_user_password(LOCAL_USER_ID, secret)
            .expect("set password");
        let user = store.get_user(LOCAL_USER_ID).expect("user");
        (store, user)
    }

    #[test]
    fn login_good_password_returns_token() {
        let (store, _) = user_with_password("correct-login-secret");
        let issued = store
            .login_with_password("local", "correct-login-secret")
            .expect("login");
        assert!(issued.token.starts_with(ACCESS_TOKEN_PREFIX));
        assert!(issued.refresh_token.starts_with(REFRESH_TOKEN_PREFIX));
        assert_eq!(issued.user_id, LOCAL_USER_ID);
        assert_eq!(issued.username, "local");
        assert!(issued.expires_at > unix_timestamp());
        let auth = store
            .authenticate_session_bearer(&issued.token)
            .expect("access token authenticates");
        assert_eq!(auth.role, crate::users::UserRole::Member);
        assert!(!auth
            .record
            .scopes
            .iter()
            .any(|s| s == crate::keys::USERS_ADMIN_SCOPE));
        assert!(!auth
            .record
            .scopes
            .iter()
            .any(|s| s == crate::keys::KEYS_ADMIN_SCOPE));
    }

    #[test]
    fn login_bad_password_is_unauthenticated() {
        let (store, _) = user_with_password("correct-login-secret");
        let err = store
            .login_with_password("local", "wrong-password")
            .expect_err("bad password");
        assert!(matches!(err, OrchestratorError::Unauthenticated));
    }

    #[test]
    fn login_unknown_user_is_unauthenticated() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        let err = store
            .login_with_password("nobody", "anything")
            .expect_err("unknown user");
        assert!(matches!(err, OrchestratorError::Unauthenticated));
    }

    #[test]
    fn login_token_expires_is_unauthenticated() {
        let (store, user) = user_with_password("expire-secret");
        let issued = store
            .issue_session(&user, -1, DEFAULT_REFRESH_TTL_SECS)
            .expect("issue");
        let err = store
            .authenticate_session_bearer(&issued.token)
            .expect_err("expired");
        assert!(matches!(err, OrchestratorError::Unauthenticated));
    }

    #[test]
    fn login_refresh_rotates_and_revokes_old() {
        let (store, _) = user_with_password("refresh-secret");
        let first = store
            .login_with_password("local", "refresh-secret")
            .expect("login");
        let second = store
            .refresh_session(&first.refresh_token)
            .expect("refresh");
        assert_ne!(first.token, second.token);
        assert_ne!(first.refresh_token, second.refresh_token);
        assert!(matches!(
            store.authenticate_session_bearer(&first.token),
            Err(OrchestratorError::Unauthenticated)
        ));
        store
            .authenticate_session_bearer(&second.token)
            .expect("new access works");
    }

    #[test]
    fn login_logout_revokes_family() {
        let (store, _) = user_with_password("logout-secret");
        let issued = store
            .login_with_password("local", "logout-secret")
            .expect("login");
        assert!(
            store
                .revoke_session_by_token_id(&issued.token_id)
                .expect("revoke")
                >= 1
        );
        assert!(matches!(
            store.authenticate_session_bearer(&issued.token),
            Err(OrchestratorError::Unauthenticated)
        ));
        assert!(matches!(
            store.refresh_session(&issued.refresh_token),
            Err(OrchestratorError::Unauthenticated)
        ));
    }

    #[test]
    fn login_refresh_token_is_not_a_bearer() {
        let (store, _) = user_with_password("bearer-kind");
        let issued = store
            .login_with_password("local", "bearer-kind")
            .expect("login");
        assert!(matches!(
            store.authenticate_session_bearer(&issued.refresh_token),
            Err(OrchestratorError::Unauthenticated)
        ));
    }
}
