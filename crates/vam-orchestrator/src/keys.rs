use crate::error::OrchestratorError;
use crate::users::UserRole;
use crate::workflow::SqliteWorkflowStore;
use rusqlite::{params, OptionalExtension};
use sha2::{Digest, Sha256};

/// Superuser scope: mint/list/revoke API keys. Operator/bootstrap only.
pub const KEYS_ADMIN_SCOPE: &str = "keys:admin";

/// Operator-only scope for HSP broker admission (install / replace) and the
/// node model allowlist (`PUT /v1/node/allowed-models`).
/// Distinct from `catalog:write` so a publisher cannot install a host-side broker
/// or publish which models the node may run.
pub const BROKERS_ADMIN_SCOPE: &str = "brokers:admin";

/// Org-admin (and operator) scope for user-management APIs.
/// Distinct from `catalog:write`. Publish-right grant/revoke and catalog
/// ownership reassignment are Phase 7; user CRUD, per-user keys, and
/// org-scope secret write/list/delete are Phase 8. Catalog list/get use a
/// wider predicate when this scope is present (`GET /v1/agents` and
/// `GET /v1/skills/packages`). List/get themselves require `runs:write` or
/// `catalog:write`, not `users:admin`.
pub const USERS_ADMIN_SCOPE: &str = "users:admin";

/// Publisher (and above) scope for the catalog release path.
/// Distinct from `catalog:write` — catalog write does not imply this.
/// Live role is the source of truth so org-admin revoke sticks.
pub const PUBLISH_GRANT_SCOPE: &str = "publish:grant";

/// Scopes tenants must never receive. Bootstrap/operator keys keep them.
pub const OPERATOR_ONLY_SCOPES: &[&str] = &[KEYS_ADMIN_SCOPE, BROKERS_ADMIN_SCOPE];

pub const BOOTSTRAP_SCOPES: &[&str] = &[
    KEYS_ADMIN_SCOPE,
    "catalog:write",
    BROKERS_ADMIN_SCOPE,
    USERS_ADMIN_SCOPE,
    PUBLISH_GRANT_SCOPE,
    "sessions:write",
    "runs:write",
    "approvals:write",
    "notifications:read",
    "notifications:write",
    "node:read",
    "secrets:write",
];

/// Default scopes for [`UserRole::OrgAdmin`] (no node master / broker admission).
pub const ORG_ADMIN_SCOPES: &[&str] = &[
    USERS_ADMIN_SCOPE,
    "catalog:write",
    PUBLISH_GRANT_SCOPE,
    "sessions:write",
    "runs:write",
    "approvals:write",
    "notifications:read",
    "notifications:write",
    "node:read",
    "secrets:write",
];

/// Default scopes for [`UserRole::Publisher`].
pub const PUBLISHER_SCOPES: &[&str] = &[
    "catalog:write",
    PUBLISH_GRANT_SCOPE,
    "sessions:write",
    "runs:write",
    "approvals:write",
    "notifications:read",
    "notifications:write",
    "node:read",
    "secrets:write",
];

/// Default scopes for [`UserRole::Member`]. No catalog publish or user admin.
pub const MEMBER_SCOPES: &[&str] = &[
    "sessions:write",
    "runs:write",
    "approvals:write",
    "notifications:read",
    "notifications:write",
    "node:read",
    "secrets:write",
];

/// True for scopes reserved to the bootstrap/operator key.
pub fn is_operator_only_scope(scope: &str) -> bool {
    OPERATOR_ONLY_SCOPES.contains(&scope)
}

/// Role → default scopes. Operator keeps node / secrets master / broker /
/// `keys:admin`. `catalog:write` never implies `publish:grant`.
pub fn default_scopes_for_role(role: UserRole) -> Vec<String> {
    let scopes = match role {
        UserRole::Operator => BOOTSTRAP_SCOPES,
        UserRole::OrgAdmin => ORG_ADMIN_SCOPES,
        UserRole::Publisher => PUBLISHER_SCOPES,
        UserRole::Member => MEMBER_SCOPES,
    };
    scopes.iter().map(|s| (*s).to_string()).collect()
}

/// Default scopes for a newly minted key bound to `owner_user_id`.
///
/// Unknown owners are treated as members. Prefer [`default_scopes_for_role`]
/// when the persisted role is available.
pub fn default_scopes_for_owner(owner_user_id: &str) -> Vec<String> {
    let role = if owner_user_id == crate::users::OPERATOR_USER_ID {
        UserRole::Operator
    } else {
        UserRole::Member
    };
    default_scopes_for_role(role)
}

/// Resolve requested scopes for a key bound to `role`.
///
/// Empty `requested` uses [`default_scopes_for_role`]. Non-operators never
/// receive `keys:admin` or `brokers:admin` even if the caller asked for them.
pub fn sanitize_scopes_for_role(role: UserRole, requested: &[String]) -> Vec<String> {
    let source = if requested.is_empty() {
        default_scopes_for_role(role)
    } else {
        requested.to_vec()
    };
    if role == UserRole::Operator {
        return source;
    }
    source
        .into_iter()
        .filter(|scope| !is_operator_only_scope(scope))
        .collect()
}

/// Resolve requested scopes for a key bound to `owner_user_id`.
///
/// Empty `requested` uses [`default_scopes_for_owner`]. Tenant owners never
/// receive operator-only scopes even if the caller asked for them.
pub fn sanitize_scopes_for_owner(owner_user_id: &str, requested: &[String]) -> Vec<String> {
    let role = if owner_user_id == crate::users::OPERATOR_USER_ID {
        UserRole::Operator
    } else {
        UserRole::Member
    };
    sanitize_scopes_for_role(role, requested)
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ApiKeyRecord {
    pub key_id: String,
    pub name: String,
    pub sha256_secret: String,
    pub principal: String,
    pub scopes: Vec<String>,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
    pub revoked_at: Option<i64>,
    /// FK to `users.user_id`. List/get/revoke filter on this unless `keys:admin`.
    pub owner_user_id: String,
    /// Credential fields (E5). NULL `holder` reads as `http`; see [`ApiKeyRecord::holder`].
    pub holder: Option<String>,
    pub target_kind: Option<String>,
    pub target_id: Option<String>,
    pub constraints_json: Option<String>,
    pub issued_via: Option<String>,
    pub external_ref: Option<String>,
    pub expires_at: Option<i64>,
    pub source_run_id: Option<String>,
    pub tool_name: Option<String>,
}

impl ApiKeyRecord {
    /// The holder; NULL or empty reads as `http` (every key minted before grants).
    pub fn holder(&self) -> &str {
        match self.holder.as_deref() {
            None | Some("") => HOLDER_HTTP,
            Some(h) => h,
        }
    }

    pub fn is_http(&self) -> bool {
        self.holder() == HOLDER_HTTP
    }
}

/// Holder of every user-minted API key and session-style bearer.
pub const HOLDER_HTTP: &str = "http";

/// Prefix of the full grant token: `vam_rg_<grant_id>.<secret>`.
pub const GRANT_TOKEN_PREFIX: &str = "vam_rg_";

macro_rules! key_cols {
    () => {
        "key_id, name, sha256_secret, principal, scopes, created_at, last_used_at, revoked_at, \
         owner_user_id, holder, target_kind, target_id, constraints_json, issued_via, \
         external_ref, expires_at, source_run_id, tool_name"
    };
}
pub(crate) use key_cols;

/// How the caller proved identity. Session tokens stamp the logged-in user;
/// API keys stamp `api_keys.owner_user_id` (bootstrap stays `operator`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthSource {
    ApiKey,
    Session,
}

#[derive(Debug, Clone)]
pub struct AuthenticatedKey {
    pub record: ApiKeyRecord,
    pub source: AuthSource,
    /// Persisted role of the bound user. Isolation filters attach to this.
    pub role: UserRole,
}

/// Which `api_keys` rows a listing returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyKind {
    /// Rows held by `http` (every key before grants). The default listing.
    Http,
    /// Rows held by a broker.
    Grants,
    All,
}

pub fn hash_api_key_secret(secret: &str) -> String {
    format!("{:x}", Sha256::digest(secret.as_bytes()))
}

/// Prefix of every minted API key secret. The `dev` segment is historical and
/// kept so existing parsers, redaction rules and docs keep matching.
const API_KEY_SECRET_PREFIX: &str = "vam_sk_dev_";

/// Random bytes in a minted secret (256 bits), hex encoded to twice this length.
const API_KEY_SECRET_BYTES: usize = 32;

/// Nullable `api_keys` columns added for credentials held by something other than HTTP.
const CREDENTIAL_COLUMNS: &[(&str, &str)] = &[
    ("holder", "TEXT"),
    ("target_kind", "TEXT"),
    ("target_id", "TEXT"),
    ("constraints_json", "TEXT"),
    ("issued_via", "TEXT"),
    ("external_ref", "TEXT"),
    ("expires_at", "INTEGER"),
    ("source_run_id", "TEXT"),
    ("tool_name", "TEXT"),
];

/// Mints an API key secret: `vam_sk_dev_` followed by 32 bytes from the OS
/// CSPRNG, hex encoded (64 chars). Only the SHA-256 of the secret is stored.
pub fn mint_api_key_secret() -> String {
    use rand::RngCore;
    use std::fmt::Write as _;
    let mut bytes = [0u8; API_KEY_SECRET_BYTES];
    // `OsRng` panics rather than returning weak bytes if the OS source fails,
    // which is the right failure for a credential (same as `signing.rs`).
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    let mut secret = String::with_capacity(API_KEY_SECRET_PREFIX.len() + bytes.len() * 2);
    secret.push_str(API_KEY_SECRET_PREFIX);
    for b in bytes {
        let _ = write!(secret, "{b:02x}");
    }
    secret
}

impl SqliteWorkflowStore {
    pub fn initialize_api_keys_schema(&self) -> Result<(), OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        connection.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS api_keys (
                key_id TEXT PRIMARY KEY NOT NULL,
                name TEXT NOT NULL,
                sha256_secret TEXT NOT NULL UNIQUE,
                principal TEXT NOT NULL,
                scopes TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                last_used_at INTEGER,
                revoked_at INTEGER
            );
            CREATE INDEX IF NOT EXISTS idx_api_keys_principal ON api_keys(principal);
            ",
        )?;
        // Credential columns (E5). Nullable, no default: an old row reads as holder `http`.
        let mut have = std::collections::HashSet::new();
        {
            let mut stmt = connection.prepare("PRAGMA table_info(api_keys)")?;
            let names = stmt.query_map([], |r| r.get::<_, String>(1))?;
            for n in names {
                have.insert(n?);
            }
        }
        for (col, ty) in CREDENTIAL_COLUMNS {
            if !have.contains(*col) {
                connection.execute_batch(&format!("ALTER TABLE api_keys ADD COLUMN {col} {ty}"))?;
            }
        }
        Ok(())
    }

    pub fn api_key_count(&self) -> Result<usize, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let count: i64 =
            connection.query_row("SELECT COUNT(*) FROM api_keys", [], |row| row.get(0))?;
        Ok(count as usize)
    }

    pub fn create_api_key(
        &self,
        key_id: &str,
        name: &str,
        secret: &str,
        principal: &str,
        scopes: &[String],
    ) -> Result<ApiKeyRecord, OrchestratorError> {
        let owner = crate::owner::owner_user_id_for_key(key_id, principal);
        self.create_api_key_owned(key_id, name, secret, principal, scopes, owner)
    }

    pub fn create_api_key_owned(
        &self,
        key_id: &str,
        name: &str,
        secret: &str,
        principal: &str,
        scopes: &[String],
        owner_user_id: &str,
    ) -> Result<ApiKeyRecord, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let now = unix_timestamp();
        let hash = hash_api_key_secret(secret);
        let scopes_json =
            serde_json::to_string(scopes).map_err(|e| OrchestratorError::Core(e.to_string()))?;
        let owner = crate::owner::resolved_owner(owner_user_id).to_string();
        connection.execute(
            "INSERT INTO api_keys (key_id, name, sha256_secret, principal, scopes, created_at, owner_user_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![key_id, name, hash, principal, scopes_json, now, owner],
        )?;
        Ok(ApiKeyRecord {
            key_id: key_id.to_string(),
            name: name.to_string(),
            sha256_secret: hash,
            principal: principal.to_string(),
            scopes: scopes.to_vec(),
            created_at: now,
            last_used_at: None,
            revoked_at: None,
            owner_user_id: owner,
            ..Default::default()
        })
    }

    pub fn list_api_keys(&self) -> Result<Vec<ApiKeyRecord>, OrchestratorError> {
        self.list_api_keys_filtered(None)
    }

    /// Keys owned by `owner_user_id` (`idx_api_keys_owner_user_id`).
    pub fn list_api_keys_for_owner(
        &self,
        owner_user_id: &str,
    ) -> Result<Vec<ApiKeyRecord>, OrchestratorError> {
        self.list_api_keys_filtered(Some(crate::owner::resolved_owner(owner_user_id)))
    }

    fn list_api_keys_filtered(
        &self,
        owner_user_id: Option<&str>,
    ) -> Result<Vec<ApiKeyRecord>, OrchestratorError> {
        match owner_user_id {
            Some(owner) => self.list_keys_of_kind(owner, false, KeyKind::Http),
            None => self.list_keys_of_kind("", true, KeyKind::Http),
        }
    }

    /// Keys of `kind` visible to `owner_user_id`. `http_admin` (`keys:admin`) widens only the
    /// `http` rows to every owner; grant rows are owner-scoped for every role.
    pub fn list_keys_of_kind(
        &self,
        owner_user_id: &str,
        http_admin: bool,
        kind: KeyKind,
    ) -> Result<Vec<ApiKeyRecord>, OrchestratorError> {
        let owner = crate::owner::resolved_owner(owner_user_id).to_string();
        let http = if http_admin {
            // `?1` is bound on every listing; the admin form uses it only to keep it referenced.
            "((holder IS NULL OR holder IN ('', 'http')) AND ?1 IS NOT NULL)"
        } else {
            "((holder IS NULL OR holder IN ('', 'http')) AND owner_user_id = ?1)"
        };
        // Action handles (issued_via 'notification') are internal to the notification path: not listed.
        let grants = "(holder IS NOT NULL AND holder NOT IN ('', 'http') AND owner_user_id = ?1
            AND COALESCE(issued_via, '') <> 'notification')";
        let clause = match kind {
            KeyKind::Http => http.to_string(),
            KeyKind::Grants => grants.to_string(),
            KeyKind::All => format!("({http} OR {grants})"),
        };
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let sql = format!(
            "SELECT {} FROM api_keys WHERE {clause} ORDER BY created_at ASC, key_id ASC",
            key_cols!()
        );
        let mut stmt = connection.prepare(&sql)?;
        let rows = stmt.query_map([owner], parse_api_key_row)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(OrchestratorError::Database)
    }

    pub fn get_api_key(&self, key_id: &str) -> Result<ApiKeyRecord, OrchestratorError> {
        self.get_api_key_filtered(key_id, None)
    }

    /// Get a key owned by `owner_user_id`. Missing or foreign → NotFound
    /// (same message; no existence leak).
    pub fn get_api_key_for_owner(
        &self,
        key_id: &str,
        owner_user_id: &str,
    ) -> Result<ApiKeyRecord, OrchestratorError> {
        self.get_api_key_filtered(key_id, Some(crate::owner::resolved_owner(owner_user_id)))
    }

    fn get_api_key_filtered(
        &self,
        key_id: &str,
        owner_user_id: Option<&str>,
    ) -> Result<ApiKeyRecord, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let missing = || OrchestratorError::NotFound(format!("API key '{key_id}' not found"));
        let result = if let Some(owner) = owner_user_id {
            connection.query_row(
                concat!(
                    "SELECT ",
                    key_cols!(),
                    "
                 FROM api_keys WHERE key_id = ?1 AND owner_user_id = ?2"
                ),
                params![key_id, owner],
                parse_api_key_row,
            )
        } else {
            connection.query_row(
                concat!(
                    "SELECT ",
                    key_cols!(),
                    "
                 FROM api_keys WHERE key_id = ?1 AND (holder IS NULL OR holder IN ('', 'http'))"
                ),
                [key_id],
                parse_api_key_row,
            )
        };
        result.map_err(|e| {
            if e == rusqlite::Error::QueryReturnedNoRows {
                missing()
            } else {
                OrchestratorError::Database(e)
            }
        })
    }

    pub fn revoke_api_key(&self, key_id: &str) -> Result<ApiKeyRecord, OrchestratorError> {
        self.revoke_api_key_filtered(key_id, None)
    }

    /// Revoke a key owned by `owner_user_id`. Missing or foreign → NotFound.
    pub fn revoke_api_key_for_owner(
        &self,
        key_id: &str,
        owner_user_id: &str,
    ) -> Result<ApiKeyRecord, OrchestratorError> {
        self.revoke_api_key_filtered(key_id, Some(crate::owner::resolved_owner(owner_user_id)))
    }

    fn revoke_api_key_filtered(
        &self,
        key_id: &str,
        owner_user_id: Option<&str>,
    ) -> Result<ApiKeyRecord, OrchestratorError> {
        let guard = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let now = unix_timestamp();
        let tx = guard.unchecked_transaction()?;
        let connection = &tx;
        let changed = if let Some(owner) = owner_user_id {
            connection.execute(
                "UPDATE api_keys SET revoked_at = ?1
                 WHERE key_id = ?2 AND owner_user_id = ?3 AND revoked_at IS NULL",
                params![now, key_id, owner],
            )?
        } else {
            connection.execute(
                "UPDATE api_keys SET revoked_at = ?1 WHERE key_id = ?2 AND revoked_at IS NULL
                 AND (holder IS NULL OR holder IN ('', 'http'))",
                params![now, key_id],
            )?
        };
        if changed == 0 {
            return Err(OrchestratorError::NotFound(format!(
                "API key '{key_id}' not found or already revoked"
            )));
        }
        // A revoked grant has nothing left to renew: its `grant.expiring` record is acted.
        crate::notifications::resolve_grant_expiring(connection, key_id, "acted")?;
        tx.commit()?;
        drop(guard);
        self.get_api_key_filtered(key_id, owner_user_id)
    }

    /// Revoke for a caller. `keys:admin` reaches every `http` key; a grant is revoked only by
    /// its owner, whatever the caller's role. A foreign grant is NotFound, like a missing key.
    pub fn revoke_api_key_as(
        &self,
        key_id: &str,
        owner_user_id: &str,
        keys_admin: bool,
    ) -> Result<ApiKeyRecord, OrchestratorError> {
        if keys_admin {
            match self.revoke_api_key(key_id) {
                Err(OrchestratorError::NotFound(_)) => {}
                other => return other,
            }
        }
        self.revoke_api_key_for_owner(key_id, owner_user_id)
    }

    /// Revoke every live API key owned by `owner_user_id`. Used on deactivate.
    pub fn revoke_api_keys_for_owner(&self, owner_user_id: &str) -> Result<u64, OrchestratorError> {
        let owner = crate::owner::resolved_owner(owner_user_id);
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let now = unix_timestamp();
        let tx = connection.unchecked_transaction()?;
        let ids: Vec<String> = {
            let mut stmt = tx.prepare(
                "SELECT key_id FROM api_keys WHERE owner_user_id = ?1 AND revoked_at IS NULL",
            )?;
            let rows = stmt
                .query_map([owner], |r| r.get(0))?
                .collect::<Result<Vec<_>, _>>()?;
            rows
        };
        let changed = tx.execute(
            "UPDATE api_keys SET revoked_at = ?1
             WHERE owner_user_id = ?2 AND revoked_at IS NULL",
            params![now, owner],
        )?;
        for id in &ids {
            crate::notifications::resolve_grant_expiring(&tx, id, "acted")?;
        }
        tx.commit()?;
        Ok(changed as u64)
    }

    pub fn authenticate_bearer(&self, secret: &str) -> Result<AuthenticatedKey, OrchestratorError> {
        // A grant is not an HTTP credential: the full token is refused before any lookup.
        if secret.starts_with(GRANT_TOKEN_PREFIX) {
            return Err(OrchestratorError::Unauthenticated);
        }
        match self.authenticate_api_key(secret) {
            Ok(key) => Ok(key),
            Err(OrchestratorError::Unauthenticated) => self.authenticate_session_bearer(secret),
            Err(e) => Err(e),
        }
    }

    fn authenticate_api_key(&self, secret: &str) -> Result<AuthenticatedKey, OrchestratorError> {
        let hash = hash_api_key_secret(secret);
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let record = connection
            .query_row(
                concat!(
                    "SELECT ",
                    key_cols!(),
                    "
                 FROM api_keys WHERE sha256_secret = ?1"
                ),
                [hash],
                parse_api_key_row,
            )
            .optional()?
            .ok_or(OrchestratorError::Unauthenticated)?;

        // A bare grant secret hashes to a grant row: refuse every non-`http` holder here,
        // before the revoked check (no 403 that confirms the secret) and before any write.
        if !record.is_http() {
            return Err(OrchestratorError::Unauthenticated);
        }

        if record.revoked_at.is_some() {
            return Err(OrchestratorError::PermissionDenied(
                "API key has been revoked".to_string(),
            ));
        }

        let now = unix_timestamp();
        connection.execute(
            "UPDATE api_keys SET last_used_at = ?1 WHERE key_id = ?2",
            params![now, record.key_id],
        )?;
        drop(connection);

        let role = match self.get_user(&record.owner_user_id) {
            Ok(user) => {
                crate::users::reject_deactivated_user(&user)?;
                user.role
            }
            Err(OrchestratorError::NotFound(_)) => UserRole::Member,
            Err(e) => return Err(e),
        };

        Ok(AuthenticatedKey {
            record,
            source: AuthSource::ApiKey,
            role,
        })
    }
}

pub(crate) fn parse_api_key_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ApiKeyRecord> {
    let scopes_json: String = row.get(4)?;
    let scopes: Vec<String> = serde_json::from_str(&scopes_json).unwrap_or_default();
    let owner_user_id: Option<String> = row.get(8)?;
    Ok(ApiKeyRecord {
        key_id: row.get(0)?,
        name: row.get(1)?,
        sha256_secret: row.get(2)?,
        principal: row.get(3)?,
        scopes,
        created_at: row.get(5)?,
        last_used_at: row.get(6)?,
        revoked_at: row.get(7)?,
        owner_user_id: owner_user_id.unwrap_or_else(|| crate::users::LOCAL_USER_ID.to_string()),
        holder: row.get(9)?,
        target_kind: row.get(10)?,
        target_id: row.get(11)?,
        constraints_json: row.get(12)?,
        issued_via: row.get(13)?,
        external_ref: row.get(14)?,
        expires_at: row.get(15)?,
        source_run_id: row.get(16)?,
        tool_name: row.get(17)?,
    })
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
    use crate::users::{UserRole, LOCAL_USER_ID, OPERATOR_USER_ID};

    #[test]
    fn minted_secrets_are_unique_prefixed_and_full_length() {
        let a = mint_api_key_secret();
        let b = mint_api_key_secret();
        assert_ne!(a, b);
        for s in [&a, &b] {
            let body = s.strip_prefix("vam_sk_dev_").expect("prefix");
            assert_eq!(body.len(), 64, "32 bytes hex encoded: {s}");
            assert!(body.bytes().all(|c| c.is_ascii_hexdigit()));
            assert!(s.starts_with("vam_sk_"));
        }
        // Many mints in one process never collide (the old time^pid^counter
        // scheme was only 64 bits and predictable).
        let set: std::collections::HashSet<_> = (0..1000).map(|_| mint_api_key_secret()).collect();
        assert_eq!(set.len(), 1000);
    }

    #[test]
    fn tenant_scopes_never_include_keys_admin() {
        let requested = vec![
            KEYS_ADMIN_SCOPE.to_string(),
            "node:read".to_string(),
            BROKERS_ADMIN_SCOPE.to_string(),
        ];
        let scopes = sanitize_scopes_for_owner(LOCAL_USER_ID, &requested);
        assert_eq!(scopes, vec!["node:read".to_string()]);
        assert!(!scopes.iter().any(|s| is_operator_only_scope(s)));
    }

    #[test]
    fn operator_scopes_keep_keys_admin() {
        let requested = vec![KEYS_ADMIN_SCOPE.to_string(), "node:read".to_string()];
        let scopes = sanitize_scopes_for_owner(OPERATOR_USER_ID, &requested);
        assert!(scopes.iter().any(|s| s == KEYS_ADMIN_SCOPE));
        assert!(scopes.iter().any(|s| s == "node:read"));
    }

    #[test]
    fn default_tenant_scopes_omit_operator_only() {
        let scopes = default_scopes_for_owner(LOCAL_USER_ID);
        assert!(!scopes.is_empty());
        assert!(!scopes.iter().any(|s| is_operator_only_scope(s)));
        assert!(scopes.iter().any(|s| s == "node:read"));
        assert!(!scopes.iter().any(|s| s == USERS_ADMIN_SCOPE));
        assert!(!scopes.iter().any(|s| s == PUBLISH_GRANT_SCOPE));
    }

    #[test]
    fn role_default_scopes_map_four_roles() {
        let operator = default_scopes_for_role(UserRole::Operator);
        assert!(operator.iter().any(|s| s == KEYS_ADMIN_SCOPE));
        assert!(operator.iter().any(|s| s == BROKERS_ADMIN_SCOPE));
        assert!(operator.iter().any(|s| s == USERS_ADMIN_SCOPE));
        assert!(operator.iter().any(|s| s == PUBLISH_GRANT_SCOPE));
        assert!(operator.iter().any(|s| s == "node:read"));
        assert!(operator.iter().any(|s| s == "secrets:write"));

        let org_admin = default_scopes_for_role(UserRole::OrgAdmin);
        assert!(org_admin.iter().any(|s| s == USERS_ADMIN_SCOPE));
        assert!(org_admin.iter().any(|s| s == PUBLISH_GRANT_SCOPE));
        assert!(org_admin.iter().any(|s| s == "node:read"));
        assert!(!org_admin.iter().any(|s| is_operator_only_scope(s)));

        let publisher = default_scopes_for_role(UserRole::Publisher);
        assert!(publisher.iter().any(|s| s == PUBLISH_GRANT_SCOPE));
        assert!(publisher.iter().any(|s| s == "catalog:write"));
        assert!(publisher.iter().any(|s| s == "node:read"));
        assert!(!publisher.iter().any(|s| s == USERS_ADMIN_SCOPE));
        assert!(!publisher.iter().any(|s| is_operator_only_scope(s)));

        let member = default_scopes_for_role(UserRole::Member);
        assert!(member.iter().any(|s| s == "sessions:write"));
        assert!(member.iter().any(|s| s == "node:read"));
        assert!(!member.iter().any(|s| s == "catalog:write"));
        assert!(!member.iter().any(|s| s == USERS_ADMIN_SCOPE));
        assert!(!member.iter().any(|s| s == PUBLISH_GRANT_SCOPE));
    }

    #[test]
    fn catalog_write_does_not_imply_publish_grant_on_role_defaults() {
        let member = default_scopes_for_role(UserRole::Member);
        assert!(!member.iter().any(|s| s == PUBLISH_GRANT_SCOPE));
        let requested = sanitize_scopes_for_role(UserRole::Member, &["catalog:write".to_string()]);
        assert_eq!(requested, vec!["catalog:write".to_string()]);
        assert!(!requested.iter().any(|s| s == PUBLISH_GRANT_SCOPE));
    }

    #[test]
    fn mint_api_key_attaches_user_id() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        let secret = mint_api_key_secret();
        let scopes = sanitize_scopes_for_owner(LOCAL_USER_ID, &["node:read".to_string()]);
        let record = store
            .create_api_key_owned(
                "key-alice",
                "alice",
                &secret,
                "User::\"alice\"",
                &scopes,
                LOCAL_USER_ID,
            )
            .expect("mint");
        assert_eq!(record.owner_user_id, LOCAL_USER_ID);
        let listed = store
            .list_api_keys()
            .expect("list")
            .into_iter()
            .find(|k| k.key_id == "key-alice")
            .expect("found");
        assert_eq!(listed.owner_user_id, LOCAL_USER_ID);
        let auth = store.authenticate_bearer(&secret).expect("auth");
        assert_eq!(auth.record.owner_user_id, LOCAL_USER_ID);
        assert_eq!(auth.source, AuthSource::ApiKey);
        assert_eq!(auth.role, UserRole::Member);
    }

    #[test]
    fn keys_list_get_revoke_are_owner_scoped() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        store.create_user("alice", "alice").expect("alice user");
        store.create_user("bob", "bob").expect("bob user");
        let scopes = sanitize_scopes_for_owner("alice", &["node:read".to_string()]);
        store
            .create_api_key_owned(
                "alice-key",
                "alice",
                &mint_api_key_secret(),
                "User::\"alice\"",
                &scopes,
                "alice",
            )
            .expect("alice key");
        store
            .create_api_key_owned(
                "bob-key",
                "bob",
                &mint_api_key_secret(),
                "User::\"bob\"",
                &scopes,
                "bob",
            )
            .expect("bob key");

        let alice_list = store.list_api_keys_for_owner("alice").expect("alice list");
        assert_eq!(alice_list.len(), 1);
        assert_eq!(alice_list[0].key_id, "alice-key");
        assert!(store.get_api_key_for_owner("alice-key", "alice").is_ok());
        let foreign = store
            .get_api_key_for_owner("bob-key", "alice")
            .expect_err("alice must not get bob key");
        match foreign {
            OrchestratorError::NotFound(msg) => assert!(msg.contains("bob-key"), "{msg}"),
            other => panic!("expected NotFound, got {other}"),
        }
        let revoke_foreign = store
            .revoke_api_key_for_owner("bob-key", "alice")
            .expect_err("alice must not revoke bob key");
        match revoke_foreign {
            OrchestratorError::NotFound(msg) => assert!(msg.contains("bob-key"), "{msg}"),
            other => panic!("expected NotFound, got {other}"),
        }
        store
            .revoke_api_key_for_owner("alice-key", "alice")
            .expect("alice revokes own key");
        assert!(store
            .get_api_key_for_owner("alice-key", "alice")
            .expect("still visible to owner")
            .revoked_at
            .is_some());
    }
}
