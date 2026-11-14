//! Control-plane users table (identity + optional argon2id password + role).
//!
//! Seeded `operator` / `local` rows stay password-less until an operator
//! reset. `POST /v1/login` exchanges a stored argon2id hash for a session
//! token. `GET /v1/me` returns that caller's `user_id`, `username`, `role`,
//! and live bearer scopes (password session or API key; not `users:admin`).
//! `VAMD_TENANT_ID` remains the telemetry/audit partition key and is
//! intentionally distinct from `users.user_id`.
//!
//! Each user has exactly one [`UserRole`]. Member is the default. HITL
//! reviewer/manager audiences are **not** these roles — they live on
//! [`crate::hitl::HitlApprover`] (decision 9 seam; default = run owner).
//! There is no reviewer or manager role yet. Org-admin grant/revoke of
//! `publish:grant` is `POST`/`DELETE /v1/users/{id}/publish-grant` (Phase 7).
//! Org-admin catalog ownership reassignment is
//! `POST /v1/catalog/reassign-owner` (Phase 7). Org-admin user CRUD is
//! `POST`/`GET /v1/users`, `POST /v1/users/{id}/deactivate` (Phase 8), and
//! `POST /v1/users/{id}/password` (Phase 10).
//! Org-admin per-user API keys are `GET`/`POST /v1/users/{id}/keys` and
//! `DELETE /v1/users/{id}/keys/{key_id}` (`users:admin`). Org-admin org-scope
//! vault write/list/delete is `PUT`/`GET /v1/org/secrets` and
//! `DELETE /v1/org/secrets/{id}` (`users:admin`; metadata + fingerprint only).
//! Deactivate revokes keys and sessions; it does not unpublish catalog rows.
//! Password reset revokes sessions and keeps API keys.

use crate::error::OrchestratorError;
use crate::password::{hash_password, verify_password};
use crate::workflow::SqliteWorkflowStore;
use rusqlite::{params, OptionalExtension};

/// Stable PK for the deployment operator (bootstrap API key maps here).
pub const OPERATOR_USER_ID: &str = "operator";
/// Mutable display name for the operator seed row.
pub const OPERATOR_USERNAME: &str = "operator";

/// Implicit single-user-mode owner. Same schema as multi-user — not a second path.
pub const LOCAL_USER_ID: &str = "local";
/// Mutable display name for the implicit local owner.
pub const LOCAL_USERNAME: &str = "local";

/// Control-plane role persisted on `users`. Exactly one per user.
///
/// HITL reviewer/manager are **not** members of this enum. Decision 9 uses
/// [`crate::hitl::HitlApprover`] (owner default; `Reviewer::` reserved).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UserRole {
    /// Shell / master-key / broker-admission holder (decision 8).
    Operator,
    /// Tenant administrator. Default scopes include `users:admin`.
    OrgAdmin,
    /// Catalog publisher. Default scopes include `publish:grant`.
    Publisher,
    /// Default for new users, including the implicit `local` owner.
    #[default]
    Member,
}

impl UserRole {
    pub const ALL: [UserRole; 4] = [
        UserRole::Operator,
        UserRole::OrgAdmin,
        UserRole::Publisher,
        UserRole::Member,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            UserRole::Operator => "operator",
            UserRole::OrgAdmin => "org_admin",
            UserRole::Publisher => "publisher",
            UserRole::Member => "member",
        }
    }

    pub fn parse(value: &str) -> Result<Self, OrchestratorError> {
        match value {
            "operator" => Ok(UserRole::Operator),
            "org_admin" => Ok(UserRole::OrgAdmin),
            "publisher" => Ok(UserRole::Publisher),
            "member" => Ok(UserRole::Member),
            other => Err(OrchestratorError::InvalidContract(format!(
                "role must be one of operator, org_admin, publisher, member; got '{other}'"
            ))),
        }
    }

    /// Lenient read for migrated / unknown rows. Unknown values become member.
    pub fn from_db(value: &str) -> Self {
        match Self::parse(value) {
            Ok(role) => role,
            Err(_) => UserRole::Member,
        }
    }

    /// Org-scope vault writes (decision 14). Operator and org_admin only.
    pub fn can_write_org_secrets(self) -> bool {
        matches!(self, UserRole::Operator | UserRole::OrgAdmin)
    }

    /// Live `publish:grant` right. Operator, org admin, and publisher hold it.
    /// Member does not. Org admin grant/revoke flips member ↔ publisher.
    pub fn has_publish_grant(self) -> bool {
        matches!(
            self,
            UserRole::Operator | UserRole::OrgAdmin | UserRole::Publisher
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserRecord {
    pub user_id: String,
    pub username: String,
    /// Reserved for future org split; currently equal to `user_id`.
    pub tenant_id: String,
    pub created_at: i64,
    /// True when an argon2id PHC string is stored. The hash itself is not exposed.
    pub has_password: bool,
    pub password_updated_at: Option<i64>,
    pub role: UserRole,
    pub deactivated_at: Option<i64>,
}

impl UserRecord {
    pub fn is_deactivated(&self) -> bool {
        self.deactivated_at.is_some()
    }
}

/// Login and token issue treat a deactivated user as unauthenticated.
pub fn reject_deactivated_login(user: &UserRecord) -> Result<(), OrchestratorError> {
    if user.is_deactivated() {
        Err(OrchestratorError::Unauthenticated)
    } else {
        Ok(())
    }
}

/// Bearer auth after a key/session is already resolved.
pub fn reject_deactivated_user(user: &UserRecord) -> Result<(), OrchestratorError> {
    if user.is_deactivated() {
        Err(OrchestratorError::PermissionDenied(format!(
            "user '{}' is deactivated",
            user.user_id
        )))
    } else {
        Ok(())
    }
}

/// Org-admin user/key APIs must not target the deployment operator row.
pub fn reject_operator_user_admin_target(user_id: &str) -> Result<(), OrchestratorError> {
    if user_id == OPERATOR_USER_ID {
        return Err(OrchestratorError::PermissionDenied(
            "cannot manage operator via users:admin".to_string(),
        ));
    }
    Ok(())
}

/// Result of [`SqliteWorkflowStore::change_own_password`].
#[derive(Debug)]
pub enum ChangeOwnPassword {
    /// The password was replaced and every session of the user revoked.
    Changed(UserRecord),
    /// `current` did not verify (or the user has no password). Nothing changed.
    WrongCurrent,
}

impl SqliteWorkflowStore {
    /// Create the `users` table (idempotent) and seed operator + local rows.
    pub fn initialize_users_schema(&self) -> Result<(), OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        connection.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS users (
                user_id TEXT PRIMARY KEY NOT NULL,
                username TEXT NOT NULL UNIQUE,
                tenant_id TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                password_hash TEXT,
                password_updated_at INTEGER,
                role TEXT NOT NULL DEFAULT 'member'
                    CHECK(role IN ('operator', 'org_admin', 'publisher', 'member')),
                deactivated_at INTEGER
            );
            CREATE INDEX IF NOT EXISTS idx_users_username ON users(username);
            CREATE INDEX IF NOT EXISTS idx_users_tenant_id ON users(tenant_id);
            ",
        )?;
        add_users_password_columns(&connection)?;
        add_users_role_column(&connection)?;
        add_users_deactivated_column(&connection)?;
        drop(connection);
        self.seed_builtin_users()?;
        Ok(())
    }

    /// Insert well-known operator and local users if missing.
    ///
    /// `operator` is always the Operator role (shell / master key). `local`
    /// is the implicit single-user owner on the same schema — member default,
    /// not a second code path.
    pub fn seed_builtin_users(&self) -> Result<(), OrchestratorError> {
        self.ensure_user(OPERATOR_USER_ID, OPERATOR_USERNAME, UserRole::Operator)?;
        self.ensure_user(LOCAL_USER_ID, LOCAL_USERNAME, UserRole::Member)?;
        self.force_operator_role()?;
        Ok(())
    }

    /// Create a control-plane user. `user_id` is the immutable PK and also
    /// the initial `tenant_id`. Username must be unique.
    pub fn create_user(
        &self,
        user_id: &str,
        username: &str,
    ) -> Result<UserRecord, OrchestratorError> {
        self.create_user_with_role(user_id, username, UserRole::Member)
    }

    /// Users that already have an argon2id hash (loginable admins).
    pub fn count_users_with_password(&self) -> Result<usize, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let count: i64 = connection.query_row(
            "SELECT COUNT(*) FROM users
             WHERE password_hash IS NOT NULL AND password_hash != ''",
            [],
            |row| row.get(0),
        )?;
        Ok(count as usize)
    }

    /// Headless first-admin bootstrap: create (or reuse) one loginable admin.
    ///
    /// Idempotent when the same username already has a password. Conflicts
    /// when a *different* user was already bootstrapped. No SMTP and no UI.
    pub fn bootstrap_first_admin(
        &self,
        username: &str,
        password: &str,
    ) -> Result<UserRecord, OrchestratorError> {
        let username = validate_username(username)?.to_string();
        if let Ok(existing) = self.resolve_user(&username) {
            if existing.has_password {
                return Ok(existing);
            }
        }
        if self.count_users_with_password()? > 0 {
            return Err(OrchestratorError::Conflict(
                "first admin already exists; use --reset-password to change a password".to_string(),
            ));
        }
        let user = match self.resolve_user(&username) {
            Ok(user) => user,
            Err(OrchestratorError::NotFound(_)) => {
                self.create_user_with_role(&username, &username, UserRole::OrgAdmin)?
            }
            Err(e) => return Err(e),
        };
        if user.role != UserRole::Operator {
            self.set_user_role(&user.user_id, UserRole::OrgAdmin)?;
        }
        self.reset_user_password(&user.user_id, password)
    }

    /// Create a control-plane user with an explicit role.
    pub fn create_user_with_role(
        &self,
        user_id: &str,
        username: &str,
        role: UserRole,
    ) -> Result<UserRecord, OrchestratorError> {
        let user_id = validate_username(user_id)?;
        let username = validate_username(username)?;
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let now = unix_timestamp();
        match connection.execute(
            "INSERT INTO users (user_id, username, tenant_id, created_at, role)
             VALUES (?1, ?2, ?1, ?3, ?4)",
            params![user_id, username, now, role.as_str()],
        ) {
            Ok(_) => {}
            Err(e) if is_unique_violation(&e) => {
                return Err(OrchestratorError::Conflict(format!(
                    "user '{user_id}' or username '{username}' already exists"
                )));
            }
            Err(e) => return Err(OrchestratorError::Database(e)),
        }
        drop(connection);
        self.get_user(user_id)
    }

    /// Org-admin create: unique username, argon2id password, optional role.
    /// `user_id` defaults to `username`. Cannot create `operator`.
    pub fn create_managed_user(
        &self,
        user_id: &str,
        username: &str,
        password: &str,
        role: UserRole,
    ) -> Result<UserRecord, OrchestratorError> {
        if role == UserRole::Operator {
            return Err(OrchestratorError::PermissionDenied(
                "cannot create operator via users:admin".to_string(),
            ));
        }
        let user_id = validate_username(user_id)?;
        let username = validate_username(username)?;
        let encoded = hash_password(password)?;
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let now = unix_timestamp();
        match connection.execute(
            "INSERT INTO users (user_id, username, tenant_id, created_at, role, password_hash, password_updated_at)
             VALUES (?1, ?2, ?1, ?3, ?4, ?5, ?3)",
            params![user_id, username, now, role.as_str(), encoded],
        ) {
            Ok(_) => {}
            Err(e) if is_unique_violation(&e) => {
                return Err(OrchestratorError::Conflict(format!(
                    "user '{user_id}' or username '{username}' already exists"
                )));
            }
            Err(e) => return Err(OrchestratorError::Database(e)),
        }
        drop(connection);
        self.get_user(user_id)
    }

    /// Mark a user inactive, revoke their API keys and session tokens.
    /// Does not unpublish catalog rows. Idempotent. Cannot deactivate operator.
    pub fn deactivate_user(&self, user_id: &str) -> Result<UserRecord, OrchestratorError> {
        let user = self.get_user(user_id)?;
        if user.user_id == OPERATOR_USER_ID {
            return Err(OrchestratorError::PermissionDenied(
                "cannot deactivate operator".to_string(),
            ));
        }
        if user.is_deactivated() {
            return Ok(user);
        }
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let now = unix_timestamp();
        let changed = connection.execute(
            "UPDATE users SET deactivated_at = ?1 WHERE user_id = ?2 AND deactivated_at IS NULL",
            params![now, user.user_id],
        )?;
        drop(connection);
        if changed == 0 {
            return self.get_user(&user.user_id);
        }
        self.revoke_api_keys_for_owner(&user.user_id)?;
        // A fire the user's grants left in a queue ends with them.
        let _ = self.discard_queued_fires(
            None,
            Some(&user.user_id),
            "the schedule's grant was revoked: its user was deactivated",
        )?;
        self.revoke_sessions_for_user(&user.user_id)?;
        self.get_user(&user.user_id)
    }

    /// Persist a role. Publish-right grant/revoke uses this for member ↔
    /// publisher.
    pub fn set_user_role(
        &self,
        user_id: &str,
        role: UserRole,
    ) -> Result<UserRecord, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let changed = connection.execute(
            "UPDATE users SET role = ?1 WHERE user_id = ?2",
            params![role.as_str(), user_id],
        )?;
        if changed == 0 {
            return Err(OrchestratorError::NotFound(format!(
                "user '{user_id}' not found"
            )));
        }
        drop(connection);
        self.get_user(user_id)
    }

    /// Grant `publish:grant` by promoting a member to publisher.
    ///
    /// Operator, org admin, and publisher already hold the right — idempotent.
    pub fn grant_publish_right(&self, user_id: &str) -> Result<UserRecord, OrchestratorError> {
        let user = self.get_user(user_id)?;
        match user.role {
            UserRole::Operator | UserRole::OrgAdmin | UserRole::Publisher => Ok(user),
            UserRole::Member => self.set_user_role(&user.user_id, UserRole::Publisher),
        }
    }

    /// Revoke `publish:grant` by demoting a publisher to member.
    ///
    /// Already-member is idempotent. Operator and org admin keep the right
    /// (their role is not a publish grant).
    pub fn revoke_publish_right(&self, user_id: &str) -> Result<UserRecord, OrchestratorError> {
        let user = self.get_user(user_id)?;
        match user.role {
            UserRole::Operator | UserRole::OrgAdmin => Err(OrchestratorError::PermissionDenied(
                "cannot revoke publish:grant from operator or org_admin".to_string(),
            )),
            UserRole::Publisher => self.set_user_role(&user.user_id, UserRole::Member),
            UserRole::Member => Ok(user),
        }
    }

    fn ensure_user(
        &self,
        user_id: &str,
        username: &str,
        role: UserRole,
    ) -> Result<(), OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let now = unix_timestamp();
        // user_id is immutable PK; tenant_id mirrors user_id for now.
        // INSERT OR IGNORE keeps existing usernames if an operator renamed them.
        connection.execute(
            "INSERT OR IGNORE INTO users (user_id, username, tenant_id, created_at, role)
             VALUES (?1, ?2, ?1, ?3, ?4)",
            params![user_id, username, now, role.as_str()],
        )?;
        Ok(())
    }

    fn force_operator_role(&self) -> Result<(), OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        connection.execute(
            "UPDATE users SET role = ?1 WHERE user_id = ?2",
            params![UserRole::Operator.as_str(), OPERATOR_USER_ID],
        )?;
        Ok(())
    }

    pub fn get_user(&self, user_id: &str) -> Result<UserRecord, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        connection
            .query_row(
                "SELECT user_id, username, tenant_id, created_at, password_hash, password_updated_at, role, deactivated_at
                 FROM users WHERE user_id = ?1",
                [user_id],
                parse_user_row,
            )
            .map_err(|e| {
                if e == rusqlite::Error::QueryReturnedNoRows {
                    OrchestratorError::NotFound(format!("user '{}' not found", user_id))
                } else {
                    OrchestratorError::Database(e)
                }
            })
    }

    pub fn get_user_by_username(&self, username: &str) -> Result<UserRecord, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        connection
            .query_row(
                "SELECT user_id, username, tenant_id, created_at, password_hash, password_updated_at, role, deactivated_at
                 FROM users WHERE username = ?1",
                [username],
                parse_user_row,
            )
            .map_err(|e| {
                if e == rusqlite::Error::QueryReturnedNoRows {
                    OrchestratorError::NotFound(format!("username '{}' not found", username))
                } else {
                    OrchestratorError::Database(e)
                }
            })
    }

    pub fn list_users(&self) -> Result<Vec<UserRecord>, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let mut stmt = connection.prepare(
            "SELECT user_id, username, tenant_id, created_at, password_hash, password_updated_at, role, deactivated_at
             FROM users WHERE user_id NOT LIKE 'marketplace:%' ORDER BY created_at ASC",
        )?;
        // `marketplace:<id>` rows are the owners of catalog installs (CN-4.5), not people.
        let rows = stmt.query_map([], parse_user_row)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(OrchestratorError::Database)
    }

    /// Rename a user. `user_id` is never changed.
    pub fn update_username(
        &self,
        user_id: &str,
        new_username: &str,
    ) -> Result<UserRecord, OrchestratorError> {
        if new_username.trim().is_empty() {
            return Err(OrchestratorError::InvalidContract(
                "username must be non-empty".to_string(),
            ));
        }
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let changed = connection.execute(
            "UPDATE users SET username = ?1 WHERE user_id = ?2",
            params![new_username, user_id],
        )?;
        if changed == 0 {
            return Err(OrchestratorError::NotFound(format!(
                "user '{}' not found",
                user_id
            )));
        }
        drop(connection);
        self.get_user(user_id)
    }

    pub fn user_count(&self) -> Result<usize, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let count: i64 =
            connection.query_row("SELECT COUNT(*) FROM users", [], |row| row.get(0))?;
        Ok(count as usize)
    }

    /// Resolve by immutable `user_id`, then by unique `username`.
    pub fn resolve_user(&self, user_id_or_username: &str) -> Result<UserRecord, OrchestratorError> {
        match self.get_user(user_id_or_username) {
            Ok(user) => Ok(user),
            Err(OrchestratorError::NotFound(_)) => self.get_user_by_username(user_id_or_username),
            Err(e) => Err(e),
        }
    }

    /// Operator/admin reset: replace the stored argon2id hash. No email.
    ///
    /// `new_password` must not be logged by callers. Seeded users may be
    /// password-less until this is called. Live session tokens for the user
    /// are revoked (stolen password must not keep an old `vam_st_` bearer).
    /// API keys stay valid — they are a separate credential; deactivate
    /// revokes both keys and sessions. Does not create a missing user.
    pub fn reset_user_password(
        &self,
        user_id_or_username: &str,
        new_password: &str,
    ) -> Result<UserRecord, OrchestratorError> {
        let user = self.resolve_user(user_id_or_username)?;
        let encoded = hash_password(new_password)?;
        let now = unix_timestamp();
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let changed = connection.execute(
            "UPDATE users SET password_hash = ?1, password_updated_at = ?2 WHERE user_id = ?3",
            params![encoded, now, user.user_id],
        )?;
        if changed == 0 {
            return Err(OrchestratorError::NotFound(format!(
                "user '{}' not found",
                user.user_id
            )));
        }
        drop(connection);
        self.revoke_sessions_for_user(&user.user_id)?;
        self.get_user(&user.user_id)
    }

    /// Self-service change: the caller proves `current`, then the password is replaced.
    ///
    /// Looks the user up by immutable `user_id` only and has no role check (the subject is
    /// always the caller, operator included). `current` must verify against the stored hash;
    /// a mismatch, or a user with no password, is [`ChangeOwnPassword::WrongCurrent`] and
    /// changes nothing. A `new_password` that matches the stored hash is refused as
    /// `InvalidContract` (reachable only after `current` verified, so it reveals nothing).
    /// Then the same path as [`Self::reset_user_password`]: new hash, `password_updated_at`,
    /// every session and refresh token revoked, API keys untouched. Callers must not log
    /// either password.
    pub fn change_own_password(
        &self,
        user_id: &str,
        current_password: &str,
        new_password: &str,
    ) -> Result<ChangeOwnPassword, OrchestratorError> {
        let user = self.get_user(user_id)?;
        reject_deactivated_login(&user)?;
        let stored = match self.user_password_hash(&user.user_id)? {
            Some(hash) => hash,
            None => return Ok(ChangeOwnPassword::WrongCurrent),
        };
        if !verify_password(current_password, &stored)? {
            return Ok(ChangeOwnPassword::WrongCurrent);
        }
        if verify_password(new_password, &stored)? {
            return Err(OrchestratorError::InvalidContract(
                "new password must differ from the current password".to_string(),
            ));
        }
        let updated = self.reset_user_password(&user.user_id, new_password)?;
        Ok(ChangeOwnPassword::Changed(updated))
    }

    /// Org-admin HTTP reset. Same hash + session-revoke as
    /// [`Self::reset_user_password`], but cannot target `operator` and looks
    /// up by immutable `user_id` only (missing id is 404, not create).
    pub fn admin_reset_user_password(
        &self,
        user_id: &str,
        new_password: &str,
    ) -> Result<UserRecord, OrchestratorError> {
        reject_operator_user_admin_target(user_id)?;
        let user = self.get_user(user_id)?;
        self.reset_user_password(&user.user_id, new_password)
    }

    /// Verify a password against the stored argon2id hash.
    ///
    /// Returns `Ok(false)` when the user has no password or the secret does
    /// not match. Never returns plaintext. Callers must not log `password`.
    pub fn verify_user_password(
        &self,
        user_id_or_username: &str,
        password: &str,
    ) -> Result<bool, OrchestratorError> {
        let user = self.resolve_user(user_id_or_username)?;
        let encoded = self.user_password_hash(&user.user_id)?;
        match encoded {
            Some(hash) if !hash.is_empty() => Ok(verify_password(password, &hash)?),
            _ => Ok(false),
        }
    }

    /// Stored PHC string, if any. For verify/reset internals and tests.
    pub fn user_password_hash(&self, user_id: &str) -> Result<Option<String>, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let hash: Option<String> = connection
            .query_row(
                "SELECT password_hash FROM users WHERE user_id = ?1",
                [user_id],
                |row| row.get(0),
            )
            .map_err(|e| {
                if e == rusqlite::Error::QueryReturnedNoRows {
                    OrchestratorError::NotFound(format!("user '{}' not found", user_id))
                } else {
                    OrchestratorError::Database(e)
                }
            })?;
        Ok(hash.filter(|value| !value.is_empty()))
    }

    /// True when the users table exists (used by migration tests).
    pub fn users_table_exists(&self) -> Result<bool, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let name: Option<String> = connection
            .query_row(
                "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'users'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        Ok(name.is_some())
    }
}

fn parse_user_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<UserRecord> {
    let password_hash: Option<String> = row.get(4)?;
    let role_raw: Option<String> = row.get(6)?;
    Ok(UserRecord {
        user_id: row.get(0)?,
        username: row.get(1)?,
        tenant_id: row.get(2)?,
        created_at: row.get(3)?,
        has_password: password_hash
            .as_deref()
            .map(|h| !h.is_empty())
            .unwrap_or(false),
        password_updated_at: row.get(5)?,
        role: UserRole::from_db(role_raw.as_deref().unwrap_or(UserRole::Member.as_str())),
        deactivated_at: row.get(7)?,
    })
}

fn add_users_password_columns(connection: &rusqlite::Connection) -> Result<(), OrchestratorError> {
    for sql in [
        "ALTER TABLE users ADD COLUMN password_hash TEXT",
        "ALTER TABLE users ADD COLUMN password_updated_at INTEGER",
    ] {
        match connection.execute(sql, []) {
            Ok(_) => {}
            Err(e) if is_duplicate_column(&e) => {}
            Err(e) => return Err(OrchestratorError::Database(e)),
        }
    }
    Ok(())
}

fn add_users_deactivated_column(
    connection: &rusqlite::Connection,
) -> Result<(), OrchestratorError> {
    match connection.execute("ALTER TABLE users ADD COLUMN deactivated_at INTEGER", []) {
        Ok(_) => {}
        Err(e) if is_duplicate_column(&e) => {}
        Err(e) => return Err(OrchestratorError::Database(e)),
    }
    Ok(())
}

fn add_users_role_column(connection: &rusqlite::Connection) -> Result<(), OrchestratorError> {
    match connection.execute(
        "ALTER TABLE users ADD COLUMN role TEXT NOT NULL DEFAULT 'member'",
        [],
    ) {
        Ok(_) => {}
        Err(e) if is_duplicate_column(&e) => {}
        Err(e) => return Err(OrchestratorError::Database(e)),
    }
    Ok(())
}

fn is_duplicate_column(err: &rusqlite::Error) -> bool {
    err.to_string().to_lowercase().contains("duplicate column")
}

fn is_unique_violation(err: &rusqlite::Error) -> bool {
    err.to_string().to_lowercase().contains("unique")
}

fn validate_username(value: &str) -> Result<&str, OrchestratorError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(OrchestratorError::InvalidContract(
            "username must be non-empty".to_string(),
        ));
    }
    if trimmed.len() > 64 {
        return Err(OrchestratorError::InvalidContract(
            "username must be at most 64 characters".to_string(),
        ));
    }
    if !trimmed
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        return Err(OrchestratorError::InvalidContract(
            "username may contain only ASCII letters, digits, '.', '-', and '_'".to_string(),
        ));
    }
    Ok(trimmed)
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
    use crate::password::is_argon2id_phc;
    use rusqlite::Connection;
    use std::path::PathBuf;
    use vam_telemetry::tenant::{DEFAULT_TENANT_ID, ENV_TENANT_ID};

    fn temp_db_path(label: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!(
            "vam-users-{}-{}-{}.db",
            label,
            std::process::id(),
            nanos
        ))
    }

    #[test]
    fn users_table_seeded_on_fresh_open() {
        let store = SqliteWorkflowStore::in_memory().expect("fresh store");
        assert!(store.users_table_exists().expect("table check"));
        assert!(store.user_count().expect("count") >= 2);

        let operator = store.get_user(OPERATOR_USER_ID).expect("operator");
        assert_eq!(operator.username, OPERATOR_USERNAME);
        assert_eq!(operator.tenant_id, OPERATOR_USER_ID);
        assert_eq!(operator.user_id, OPERATOR_USER_ID);
        assert_eq!(operator.role, UserRole::Operator);

        let local = store.get_user(LOCAL_USER_ID).expect("local");
        assert_eq!(local.username, LOCAL_USERNAME);
        assert_eq!(local.tenant_id, LOCAL_USER_ID);
        assert_eq!(local.role, UserRole::Member);
        assert!(!operator.has_password);
        assert!(!local.has_password);
        assert!(operator.password_updated_at.is_none());
        assert!(local.password_updated_at.is_none());
    }

    #[test]
    fn users_table_migrates_existing_db() {
        let path = temp_db_path("migrate");
        // Pre-Phase-1 DB: workflow tables only, no users.
        {
            let conn = Connection::open(&path).expect("legacy open");
            conn.execute_batch(
                "
                PRAGMA foreign_keys = ON;
                CREATE TABLE runs (
                    run_id TEXT PRIMARY KEY NOT NULL,
                    state TEXT NOT NULL,
                    revision INTEGER NOT NULL
                );
                ",
            )
            .expect("legacy schema");
        }

        let store = SqliteWorkflowStore::open(&path).expect("migrate open");
        assert!(store.users_table_exists().expect("users after migrate"));
        let operator = store.get_user(OPERATOR_USER_ID).expect("operator seeded");
        let local = store.get_user(LOCAL_USER_ID).expect("local seeded");
        assert_eq!(operator.tenant_id, operator.user_id);
        assert_eq!(local.tenant_id, local.user_id);

        // Re-open is idempotent.
        let store2 = SqliteWorkflowStore::open(&path).expect("reopen");
        assert_eq!(store2.user_count().expect("count"), 2);

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{}-wal", path.display()));
        let _ = std::fs::remove_file(format!("{}-shm", path.display()));
    }

    #[test]
    fn users_seed_is_idempotent() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        store.seed_builtin_users().expect("reseed");
        store.seed_builtin_users().expect("reseed again");
        assert_eq!(store.user_count().expect("count"), 2);
    }

    #[test]
    fn users_username_is_unique() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        let connection = store.connection.lock().expect("lock");
        let err = connection
            .execute(
                "INSERT INTO users (user_id, username, tenant_id, created_at)
                 VALUES ('other', ?1, 'other', 1)",
                params![OPERATOR_USERNAME],
            )
            .expect_err("duplicate username must fail");
        assert!(
            err.to_string().to_lowercase().contains("unique"),
            "expected unique violation, got: {err}"
        );
    }

    #[test]
    fn users_user_id_is_immutable_pk() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        let renamed = store
            .update_username(LOCAL_USER_ID, "local-renamed")
            .expect("rename username");
        assert_eq!(renamed.user_id, LOCAL_USER_ID);
        assert_eq!(renamed.username, "local-renamed");

        // Lookup by immutable PK still works; username changed.
        let by_id = store.get_user(LOCAL_USER_ID).expect("get by id");
        assert_eq!(by_id.username, "local-renamed");
        assert!(store.get_user_by_username(LOCAL_USERNAME).is_err());
        assert!(store.get_user_by_username("local-renamed").is_ok());
    }

    #[test]
    fn users_tenant_env_partition_key_unchanged() {
        // Control-plane users.tenant_id must not replace VAMD_TENANT_ID.
        assert_eq!(ENV_TENANT_ID, "VAMD_TENANT_ID");
        assert_eq!(DEFAULT_TENANT_ID, "local");
        let store = SqliteWorkflowStore::in_memory().expect("store");
        let local = store.get_user(LOCAL_USER_ID).expect("local");
        // Same string value by coincidence for single-user mode, but different concepts.
        assert_eq!(local.user_id, "local");
        assert_eq!(DEFAULT_TENANT_ID, "local");
    }

    #[test]
    fn users_seeded_rows_remain_passwordless_until_reset() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        for id in [OPERATOR_USER_ID, LOCAL_USER_ID] {
            let user = store.get_user(id).expect("seed");
            assert!(!user.has_password, "{id} must stay password-less");
            assert!(store.user_password_hash(id).expect("hash lookup").is_none());
            assert!(!store.verify_user_password(id, "anything").expect("verify"));
        }
    }

    #[test]
    fn password_hash_verify_success_and_fail() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        let secret = "phase2-local-secret";
        store
            .reset_user_password(LOCAL_USER_ID, secret)
            .expect("set password");

        assert!(store
            .verify_user_password(LOCAL_USER_ID, secret)
            .expect("verify match"));
        assert!(store
            .verify_user_password(LOCAL_USERNAME, secret)
            .expect("verify by username"));
        assert!(!store
            .verify_user_password(LOCAL_USER_ID, "wrong-password")
            .expect("verify mismatch"));

        let stored = store
            .user_password_hash(LOCAL_USER_ID)
            .expect("stored hash")
            .expect("hash present");
        assert!(is_argon2id_phc(&stored));
        assert!(
            !stored.contains(secret),
            "DB must store argon2id PHC, not plaintext"
        );
    }

    #[test]
    fn password_reset_replaces_hash() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        let first = store
            .reset_user_password(OPERATOR_USERNAME, "first-admin-secret")
            .expect("first reset");
        assert!(first.has_password);
        assert!(first.password_updated_at.is_some());
        let old_hash = store
            .user_password_hash(OPERATOR_USER_ID)
            .expect("old hash")
            .expect("present");

        let second = store
            .reset_user_password(OPERATOR_USER_ID, "second-admin-secret")
            .expect("second reset");
        assert!(second.has_password);
        let new_hash = store
            .user_password_hash(OPERATOR_USER_ID)
            .expect("new hash")
            .expect("present");
        assert_ne!(old_hash, new_hash);
        assert!(is_argon2id_phc(&new_hash));
        assert!(!store
            .verify_user_password(OPERATOR_USER_ID, "first-admin-secret")
            .expect("old secret rejected"));
        assert!(store
            .verify_user_password(OPERATOR_USER_ID, "second-admin-secret")
            .expect("new secret accepted"));
        assert!(second.password_updated_at.unwrap_or(0) >= first.password_updated_at.unwrap_or(0));
    }

    #[test]
    fn password_reset_unknown_user_is_not_found() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        let err = store
            .reset_user_password("nobody", "secret")
            .expect_err("missing user");
        assert!(matches!(err, OrchestratorError::NotFound(_)));
    }

    #[test]
    fn password_columns_migrate_existing_users_table() {
        let path = temp_db_path("password-migrate");
        {
            let conn = Connection::open(&path).expect("legacy open");
            conn.execute_batch(
                "
                PRAGMA foreign_keys = ON;
                CREATE TABLE users (
                    user_id TEXT PRIMARY KEY NOT NULL,
                    username TEXT NOT NULL UNIQUE,
                    tenant_id TEXT NOT NULL,
                    created_at INTEGER NOT NULL
                );
                INSERT INTO users (user_id, username, tenant_id, created_at)
                VALUES ('local', 'local', 'local', 1),
                       ('operator', 'operator', 'operator', 1);
                ",
            )
            .expect("legacy users");
        }

        let store = SqliteWorkflowStore::open(&path).expect("migrate open");
        let local = store.get_user(LOCAL_USER_ID).expect("local");
        assert!(!local.has_password);
        store
            .reset_user_password(LOCAL_USER_ID, "migrated-secret")
            .expect("reset after migrate");
        assert!(store
            .verify_user_password(LOCAL_USER_ID, "migrated-secret")
            .expect("verify after migrate"));

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{}-wal", path.display()));
        let _ = std::fs::remove_file(format!("{}-shm", path.display()));
    }

    #[test]
    fn bootstrap_creates_one_admin() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        assert_eq!(store.count_users_with_password().expect("count"), 0);

        let admin = store
            .bootstrap_first_admin("admin", "headless-admin-secret")
            .expect("bootstrap");
        assert_eq!(admin.user_id, "admin");
        assert_eq!(admin.username, "admin");
        assert_eq!(admin.role, UserRole::OrgAdmin);
        assert!(admin.has_password);
        assert_eq!(store.count_users_with_password().expect("count"), 1);
        assert!(store
            .verify_user_password("admin", "headless-admin-secret")
            .expect("verify"));
        assert!(!store
            .verify_user_password("admin", "wrong")
            .expect("mismatch"));

        // Same username is idempotent and does not create a second admin.
        let again = store
            .bootstrap_first_admin("admin", "ignored-on-repeat")
            .expect("idempotent");
        assert_eq!(again.user_id, "admin");
        assert_eq!(store.count_users_with_password().expect("count"), 1);
        assert!(store
            .verify_user_password("admin", "headless-admin-secret")
            .expect("original password kept"));

        let conflict = store
            .bootstrap_first_admin("other-admin", "other-secret")
            .expect_err("second admin");
        assert!(matches!(conflict, OrchestratorError::Conflict(_)));
        assert_eq!(store.user_count().expect("users"), 3);
    }

    #[test]
    fn bootstrap_can_password_seeded_operator() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        let operator = store
            .bootstrap_first_admin(OPERATOR_USERNAME, "operator-headless")
            .expect("bootstrap operator");
        assert_eq!(operator.user_id, OPERATOR_USER_ID);
        assert_eq!(operator.role, UserRole::Operator);
        assert!(operator.has_password);
        assert_eq!(store.user_count().expect("users"), 2);
        assert_eq!(store.count_users_with_password().expect("count"), 1);
    }

    #[test]
    fn role_is_persisted_and_member_is_default() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        let created = store.create_user("alice", "alice").expect("create");
        assert_eq!(created.role, UserRole::Member);
        assert!(!created.role.can_write_org_secrets());
        assert!(UserRole::Operator.can_write_org_secrets());
        assert!(UserRole::OrgAdmin.can_write_org_secrets());
        assert!(!UserRole::Publisher.can_write_org_secrets());
        let loaded = store.get_user("alice").expect("reload");
        assert_eq!(loaded.role, UserRole::Member);

        let publisher = store
            .create_user_with_role("pub", "pub", UserRole::Publisher)
            .expect("publisher");
        assert_eq!(publisher.role, UserRole::Publisher);
        assert_eq!(
            store.get_user("pub").expect("reload publisher").role,
            UserRole::Publisher
        );

        let promoted = store
            .set_user_role("alice", UserRole::OrgAdmin)
            .expect("promote");
        assert_eq!(promoted.role, UserRole::OrgAdmin);
        assert_eq!(
            store.get_user("alice").expect("reload promoted").role,
            UserRole::OrgAdmin
        );
    }

    #[test]
    fn publish_grant_promotes_member_and_revoke_sticks() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        store.create_user("alice", "alice").expect("create");
        assert!(!store
            .get_user("alice")
            .expect("alice")
            .role
            .has_publish_grant());

        let granted = store.grant_publish_right("alice").expect("grant");
        assert_eq!(granted.role, UserRole::Publisher);
        assert!(granted.role.has_publish_grant());
        let again = store
            .grant_publish_right("alice")
            .expect("idempotent grant");
        assert_eq!(again.role, UserRole::Publisher);

        let revoked = store.revoke_publish_right("alice").expect("revoke");
        assert_eq!(revoked.role, UserRole::Member);
        assert!(!revoked.role.has_publish_grant());
        let still = store
            .revoke_publish_right("alice")
            .expect("idempotent revoke");
        assert_eq!(still.role, UserRole::Member);

        store
            .create_user_with_role("admin", "admin", UserRole::OrgAdmin)
            .expect("admin");
        assert!(store
            .grant_publish_right("admin")
            .expect("admin already has grant")
            .role
            .has_publish_grant());
        let err = store
            .revoke_publish_right("admin")
            .expect_err("cannot demote org_admin");
        assert!(matches!(err, OrchestratorError::PermissionDenied(_)));
        assert_eq!(
            store.get_user("admin").expect("admin stays").role,
            UserRole::OrgAdmin
        );
    }

    #[test]
    fn role_rejects_unknown_value() {
        let err = UserRole::parse("reviewer").expect_err("HITL reviewer is not a user role");
        assert!(matches!(err, OrchestratorError::InvalidContract(_)));
        assert_eq!(UserRole::ALL.len(), 4);
        assert_eq!(UserRole::from_db("reviewer"), UserRole::Member);
        assert_eq!(UserRole::from_db("org_admin"), UserRole::OrgAdmin);
    }

    #[test]
    fn role_column_migrates_existing_users_table() {
        let path = temp_db_path("role-migrate");
        {
            let conn = Connection::open(&path).expect("legacy open");
            conn.execute_batch(
                "
                PRAGMA foreign_keys = ON;
                CREATE TABLE users (
                    user_id TEXT PRIMARY KEY NOT NULL,
                    username TEXT NOT NULL UNIQUE,
                    tenant_id TEXT NOT NULL,
                    created_at INTEGER NOT NULL
                );
                INSERT INTO users (user_id, username, tenant_id, created_at)
                VALUES ('local', 'local', 'local', 1),
                       ('operator', 'operator', 'operator', 1),
                       ('alice', 'alice', 'alice', 2);
                ",
            )
            .expect("legacy users");
        }

        let store = SqliteWorkflowStore::open(&path).expect("migrate open");
        assert_eq!(
            store.get_user(OPERATOR_USER_ID).expect("operator").role,
            UserRole::Operator
        );
        assert_eq!(
            store.get_user(LOCAL_USER_ID).expect("local").role,
            UserRole::Member
        );
        assert_eq!(
            store.get_user("alice").expect("alice").role,
            UserRole::Member
        );

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{}-wal", path.display()));
        let _ = std::fs::remove_file(format!("{}-shm", path.display()));
    }

    #[test]
    fn bootstrap_local_promotes_to_org_admin_same_path() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        let local = store
            .bootstrap_first_admin(LOCAL_USERNAME, "local-headless")
            .expect("bootstrap local");
        assert_eq!(local.user_id, LOCAL_USER_ID);
        assert_eq!(local.role, UserRole::OrgAdmin);
        assert_eq!(
            store.get_user(OPERATOR_USER_ID).expect("operator").role,
            UserRole::Operator
        );
    }

    #[test]
    fn create_managed_user_hashes_password_and_defaults_member() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        let created = store
            .create_managed_user("carol", "carol", "carol-secret", UserRole::Member)
            .expect("create");
        assert_eq!(created.user_id, "carol");
        assert_eq!(created.role, UserRole::Member);
        assert!(created.has_password);
        assert!(!created.is_deactivated());
        assert!(store
            .verify_user_password("carol", "carol-secret")
            .expect("verify"));
        let listed = store.list_users().expect("list");
        assert!(listed.iter().any(|u| u.user_id == "carol"));
        let err = store
            .create_managed_user("op2", "op2", "secret", UserRole::Operator)
            .expect_err("no operator via admin");
        assert!(matches!(err, OrchestratorError::PermissionDenied(_)));
    }

    #[test]
    fn deactivate_user_is_idempotent_and_cannot_target_operator() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        store.create_user("alice", "alice").expect("create");
        let first = store.deactivate_user("alice").expect("deactivate");
        assert!(first.is_deactivated());
        let again = store.deactivate_user("alice").expect("idempotent");
        assert_eq!(again.deactivated_at, first.deactivated_at);
        let err = store
            .deactivate_user(OPERATOR_USER_ID)
            .expect_err("operator");
        assert!(matches!(err, OrchestratorError::PermissionDenied(_)));
        assert!(!store
            .get_user(OPERATOR_USER_ID)
            .expect("operator")
            .is_deactivated());
    }

    #[test]
    fn admin_reset_user_password_cannot_target_operator() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        let err = store
            .admin_reset_user_password(OPERATOR_USER_ID, "nope")
            .expect_err("operator");
        assert!(matches!(err, OrchestratorError::PermissionDenied(_)));
        assert!(
            !store
                .get_user(OPERATOR_USER_ID)
                .expect("operator")
                .has_password
        );
    }

    #[test]
    fn admin_reset_user_password_missing_user_is_not_found() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        let err = store
            .admin_reset_user_password("nobody", "secret")
            .expect_err("missing");
        assert!(matches!(err, OrchestratorError::NotFound(_)));
        assert!(store.get_user("nobody").is_err());
    }

    #[test]
    fn password_reset_revokes_sessions_and_keeps_api_keys() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        store
            .create_managed_user("alice", "alice", "old-secret", UserRole::Member)
            .expect("create");
        let issued = store
            .login_with_password("alice", "old-secret")
            .expect("login");
        let secret = crate::keys::mint_api_key_secret();
        let scopes: Vec<String> = crate::keys::MEMBER_SCOPES
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        store
            .create_api_key_owned(
                "alice-ci",
                "alice",
                &secret,
                "User::\"alice\"",
                &scopes,
                "alice",
            )
            .expect("key");

        store
            .admin_reset_user_password("alice", "new-secret")
            .expect("reset");
        assert!(store
            .verify_user_password("alice", "new-secret")
            .expect("new"));
        assert!(!store
            .verify_user_password("alice", "old-secret")
            .expect("old"));
        assert!(matches!(
            store.authenticate_session_bearer(&issued.token),
            Err(OrchestratorError::Unauthenticated)
        ));
        store
            .authenticate_bearer(&secret)
            .expect("api key still valid");
    }

    fn alice_with_key(store: &SqliteWorkflowStore) -> String {
        store
            .create_managed_user("alice", "alice", "old-secret", UserRole::Member)
            .expect("create");
        let secret = crate::keys::mint_api_key_secret();
        let scopes: Vec<String> = crate::keys::MEMBER_SCOPES
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        store
            .create_api_key_owned(
                "alice-ci",
                "alice",
                &secret,
                "User::\"alice\"",
                &scopes,
                "alice",
            )
            .expect("key");
        secret
    }

    #[test]
    fn change_own_password_replaces_hash_revokes_sessions_keeps_keys() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        let key = alice_with_key(&store);
        let issued = store
            .login_with_password("alice", "old-secret")
            .expect("login");
        let before = store.get_user("alice").expect("user");

        let out = store
            .change_own_password("alice", "old-secret", "new-secret")
            .expect("change");
        let ChangeOwnPassword::Changed(user) = out else {
            panic!("expected Changed");
        };
        assert_eq!(user.user_id, "alice");
        assert!(user.has_password);
        assert!(user.password_updated_at >= before.password_updated_at);
        assert!(store
            .verify_user_password("alice", "new-secret")
            .expect("new"));
        assert!(!store
            .verify_user_password("alice", "old-secret")
            .expect("old"));
        assert!(matches!(
            store.authenticate_session_bearer(&issued.token),
            Err(OrchestratorError::Unauthenticated)
        ));
        assert!(matches!(
            store.refresh_session(&issued.refresh_token),
            Err(OrchestratorError::Unauthenticated)
        ));
        store
            .authenticate_bearer(&key)
            .expect("api key still valid");
        let hash = store
            .user_password_hash("alice")
            .expect("hash")
            .expect("some");
        assert!(is_argon2id_phc(&hash));
    }

    #[test]
    fn change_own_password_wrong_current_changes_nothing() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        alice_with_key(&store);
        let issued = store
            .login_with_password("alice", "old-secret")
            .expect("login");
        let hash_before = store.user_password_hash("alice").expect("h");

        let out = store
            .change_own_password("alice", "not-the-secret", "new-secret")
            .expect("outcome");
        assert!(matches!(out, ChangeOwnPassword::WrongCurrent));
        assert_eq!(store.user_password_hash("alice").expect("h"), hash_before);
        store
            .authenticate_session_bearer(&issued.token)
            .expect("session survives");
        // A wrong current that equals the new password is still just wrong.
        let out = store
            .change_own_password("alice", "new-secret", "new-secret")
            .expect("outcome");
        assert!(matches!(out, ChangeOwnPassword::WrongCurrent));
    }

    #[test]
    fn change_own_password_same_password_is_invalid_contract() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        alice_with_key(&store);
        let issued = store
            .login_with_password("alice", "old-secret")
            .expect("login");
        let hash_before = store.user_password_hash("alice").expect("h");

        let err = store
            .change_own_password("alice", "old-secret", "old-secret")
            .expect_err("same");
        match err {
            OrchestratorError::InvalidContract(m) => {
                assert_eq!(m, "new password must differ from the current password")
            }
            other => panic!("expected InvalidContract, got {other:?}"),
        }
        assert_eq!(store.user_password_hash("alice").expect("h"), hash_before);
        store
            .authenticate_session_bearer(&issued.token)
            .expect("session survives");
    }

    #[test]
    fn change_own_password_without_a_stored_password_is_wrong_current() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        let out = store
            .change_own_password(LOCAL_USER_ID, "anything", "new-secret")
            .expect("outcome");
        assert!(matches!(out, ChangeOwnPassword::WrongCurrent));
        assert!(!store.get_user(LOCAL_USER_ID).expect("local").has_password);
    }

    #[test]
    fn change_own_password_unknown_user_is_not_found() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        let err = store
            .change_own_password("nobody", "a", "b")
            .expect_err("missing");
        assert!(matches!(err, OrchestratorError::NotFound(_)));
    }

    #[test]
    fn change_own_password_looks_up_by_user_id_not_username() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        store
            .create_managed_user("uid-1", "bob", "bob-secret", UserRole::Member)
            .expect("create");
        let err = store
            .change_own_password("bob", "bob-secret", "bob-new")
            .expect_err("username is not an id");
        assert!(matches!(err, OrchestratorError::NotFound(_)));
        assert!(matches!(
            store
                .change_own_password("uid-1", "bob-secret", "bob-new")
                .expect("by id"),
            ChangeOwnPassword::Changed(_)
        ));
    }

    #[test]
    fn change_own_password_works_for_operator_and_rejects_deactivated() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        store
            .reset_user_password(OPERATOR_USER_ID, "operator-old")
            .expect("operator password");
        let out = store
            .change_own_password(OPERATOR_USER_ID, "operator-old", "operator-new")
            .expect("operator change");
        assert!(matches!(out, ChangeOwnPassword::Changed(_)));
        assert!(store
            .verify_user_password(OPERATOR_USER_ID, "operator-new")
            .expect("verify"));

        store
            .create_managed_user("dave", "dave", "dave-secret", UserRole::Member)
            .expect("create");
        store.deactivate_user("dave").expect("deactivate");
        let err = store
            .change_own_password("dave", "dave-secret", "dave-new")
            .expect_err("deactivated");
        assert!(matches!(err, OrchestratorError::Unauthenticated));
    }
}
