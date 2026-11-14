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

/// Prefix of a generated `user_id` (`usr_` plus 26 characters `[a-z2-7]`, minted by
/// [`crate::spaces::mint_id`]). A new username may not start with it (any letter case), so a
/// login name and a generated id never collide.
pub const GENERATED_USER_ID_PREFIX: &str = "usr_";
/// The name the node keeps for the principal of its own bootstrap key. Never a username.
pub const RESERVED_BOOTSTRAP_NAME: &str = "bootstrap";
/// Longest username, in characters (an email address can be longer than the old 64).
pub const MAX_USERNAME_CHARS: usize = 254;
/// Longest user id. Ids stay at most 64 characters (workspace prefixes embed them).
pub const MAX_USER_ID_CHARS: usize = 64;
/// Attempts at minting a free generated id before a create gives up.
const MAX_GENERATED_ID_ATTEMPTS: usize = 3;
/// The unique index that makes usernames case-insensitively unique.
pub const USERNAME_NOCASE_INDEX: &str = "idx_users_username_nocase";

/// True for the shape of a generated user id: `usr_` plus 26 characters `[a-z2-7]`.
pub fn is_generated_user_id(value: &str) -> bool {
    value
        .strip_prefix(GENERATED_USER_ID_PREFIX)
        .is_some_and(|rest| {
            rest.len() == 26
                && rest
                    .bytes()
                    .all(|c| c.is_ascii_lowercase() || (b'2'..=b'7').contains(&c))
        })
}

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
///
/// Pass the **resolved** user's `user_id`, never the raw path or body value: names are matched
/// ignoring letter case, so `OPERATOR` reaches the operator row only after resolution.
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

/// Usernames that are equal once letter case is ignored (ASCII), found when the case-insensitive
/// unique index could not be created. Each member is `(user_id, username)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsernameClash {
    pub members: Vec<(String, String)>,
}

/// What `vamd` prints, and exits 1 on, while clashes exist. Each group lists its usernames with
/// their user ids; the fix is the offline `--rename-user` flag.
pub fn username_clash_message(clashes: &[UsernameClash]) -> String {
    let mut out =
        String::from("vamd: usernames must be unique ignoring letter case; these clash:\n");
    for group in clashes {
        let line = group
            .members
            .iter()
            .map(|(user_id, username)| format!("{username} (user id {user_id})"))
            .collect::<Vec<_>>()
            .join(", ");
        out.push_str(&format!("  {line}\n"));
    }
    out.push_str("Rename all but one user of each group, then start vamd again:\n");
    out.push_str("  vamd --rename-user <user_id> <new_username>\n");
    out.push_str(
        "Use the same database as for serving (--db-path or VAMD_DB_PATH). A user id and every \
         row it owns stay as they are; only the login name changes.\n",
    );
    out
}

/// Groups of users whose usernames are equal ignoring ASCII case (SQLite `NOCASE` folds ASCII
/// only, and so does this). Each group has two or more members, oldest first.
fn find_username_clashes(
    connection: &rusqlite::Connection,
) -> Result<Vec<UsernameClash>, OrchestratorError> {
    let mut stmt = connection
        .prepare("SELECT user_id, username FROM users ORDER BY created_at ASC, user_id ASC")?;
    let rows = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut groups: Vec<(String, Vec<(String, String)>)> = Vec::new();
    for (user_id, username) in rows {
        let key = username.to_ascii_lowercase();
        match groups.iter_mut().find(|(k, _)| *k == key) {
            Some((_, members)) => members.push((user_id, username)),
            None => groups.push((key, vec![(user_id, username)])),
        }
    }
    Ok(groups
        .into_iter()
        .filter(|(_, members)| members.len() > 1)
        .map(|(_, members)| UsernameClash { members })
        .collect())
}

/// A username must be free: no other user holds it ignoring letter case, and it is not another
/// user's id (ignoring letter case), since resolution tries the id first. `own_user_id` is the
/// row being created or renamed, which never clashes with itself.
fn ensure_username_available(
    connection: &rusqlite::Connection,
    username: &str,
    own_user_id: &str,
) -> Result<(), OrchestratorError> {
    let holder: Option<String> = connection
        .query_row(
            "SELECT username FROM users
             WHERE username = ?1 COLLATE NOCASE AND user_id != ?2 LIMIT 1",
            params![username, own_user_id],
            |row| row.get(0),
        )
        .optional()?;
    if holder.is_some() {
        return Err(OrchestratorError::Conflict(format!(
            "username '{username}' is already taken (usernames are matched ignoring letter case)"
        )));
    }
    let id_holder: Option<String> = connection
        .query_row(
            "SELECT user_id FROM users
             WHERE user_id = ?1 COLLATE NOCASE AND user_id != ?2 LIMIT 1",
            params![username, own_user_id],
            |row| row.get(0),
        )
        .optional()?;
    if id_holder.is_some() {
        return Err(OrchestratorError::InvalidContract(format!(
            "username '{username}' is the id of another user"
        )));
    }
    Ok(())
}

/// How one insert of a `users` row ended.
enum UserInsert {
    Inserted,
    /// The primary key was taken (a generated id may retry).
    IdTaken,
    /// The username was taken (a unique index, whichever one).
    NameTaken,
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
        self.ensure_username_nocase_index()?;
        Ok(())
    }

    /// Make usernames unique ignoring ASCII letter case (decision 16).
    ///
    /// Idempotent, run on every open. With the index present there is nothing to do. Without it,
    /// the users are checked first: when no two usernames are equal ignoring case the unique
    /// index is created; otherwise **no index is created** and the clashing groups are kept on
    /// the store ([`Self::username_clashes`]). A clash is never an `open` error: `vamd` falls
    /// back to an empty in-memory store when `open` fails, which would serve a node with no
    /// users. `vamd` reads the groups instead and refuses to serve or `--init`.
    pub(crate) fn ensure_username_nocase_index(&self) -> Result<(), OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let exists: Option<i64> = connection
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type = 'index' AND name = ?1",
                [USERNAME_NOCASE_INDEX],
                |row| row.get(0),
            )
            .optional()?;
        let clashes = if exists.is_some() {
            Vec::new()
        } else {
            let clashes = find_username_clashes(&connection)?;
            if clashes.is_empty() {
                connection.execute_batch(&format!(
                    "CREATE UNIQUE INDEX IF NOT EXISTS {USERNAME_NOCASE_INDEX}
                     ON users(username COLLATE NOCASE);"
                ))?;
            }
            clashes
        };
        drop(connection);
        *self
            .username_clashes
            .write()
            .map_err(|_| OrchestratorError::StorageUnavailable)? = clashes;
        Ok(())
    }

    /// The groups of usernames that clash ignoring letter case, found when this store was
    /// opened. Empty when the case-insensitive unique index exists.
    pub fn username_clashes(&self) -> Vec<UsernameClash> {
        self.username_clashes
            .read()
            .map(|g| g.clone())
            .unwrap_or_default()
    }

    /// True when the case-insensitive unique index on `users.username` exists.
    pub fn username_nocase_index_exists(&self) -> Result<bool, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let exists: Option<i64> = connection
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type = 'index' AND name = ?1",
                [USERNAME_NOCASE_INDEX],
                |row| row.get(0),
            )
            .optional()?;
        Ok(exists.is_some())
    }

    /// Existing users whose name the new rules would not allow (a username that starts `usr_`,
    /// is `bootstrap`, or equals another user's id, ignoring case). They are kept, not renamed;
    /// resolution prefers the id. `vamd` logs them once at start.
    pub fn legacy_identity_notes(&self) -> Result<Vec<String>, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let mut stmt = connection.prepare(
            "SELECT user_id, username FROM users
             WHERE user_id NOT LIKE 'marketplace:%' ORDER BY created_at ASC, user_id ASC",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let mut notes = Vec::new();
        for (user_id, username) in &rows {
            let lower = username.to_ascii_lowercase();
            if lower.starts_with(GENERATED_USER_ID_PREFIX) {
                notes.push(format!(
                    "username '{username}' (user id {user_id}) starts with '{GENERATED_USER_ID_PREFIX}', which is reserved for generated ids"
                ));
            }
            if lower == RESERVED_BOOTSTRAP_NAME {
                notes.push(format!(
                    "username '{username}' (user id {user_id}) is the name the node keeps for its own key"
                ));
            }
            if let Some((other, _)) = rows
                .iter()
                .find(|(other, _)| other != user_id && other.eq_ignore_ascii_case(username))
            {
                notes.push(format!(
                    "username '{username}' (user id {user_id}) is the id of user '{other}'; the id wins when that name is resolved"
                ));
            }
        }
        Ok(notes)
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
    /// Idempotent when the same username (ignoring letter case) already has a password.
    /// Conflicts when a *different* user was already bootstrapped. A new admin gets a generated
    /// `usr_` id like any other new user. No SMTP and no UI.
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
                self.create_generated_user(&username, None, UserRole::OrgAdmin)?
            }
            Err(e) => return Err(e),
        };
        if user.role != UserRole::Operator {
            self.set_user_role(&user.user_id, UserRole::OrgAdmin)?;
        }
        self.reset_user_password(&user.user_id, password)
    }

    /// Create a control-plane user with an explicit id and role.
    ///
    /// For seeds and tests only: the API never lets a client choose an id (see
    /// [`Self::create_managed_user_generated`]). The id keeps the old rules (ASCII letters,
    /// digits, `.`, `_`, `-`, at most 64); the username follows [`validate_username`].
    pub fn create_user_with_role(
        &self,
        user_id: &str,
        username: &str,
        role: UserRole,
    ) -> Result<UserRecord, OrchestratorError> {
        let user_id = validate_user_id(user_id)?;
        let username = validate_username(username)?;
        match self.insert_user_row(user_id, username, role, None)? {
            UserInsert::Inserted => self.get_user(user_id),
            UserInsert::IdTaken | UserInsert::NameTaken => Err(OrchestratorError::Conflict(
                format!("user '{user_id}' or username '{username}' already exists"),
            )),
        }
    }

    /// Org-admin create with an explicit id: unique username, argon2id password, optional role.
    /// For seeds and tests; `POST /v1/users` uses [`Self::create_managed_user_generated`].
    /// Cannot create `operator`.
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
        let user_id = validate_user_id(user_id)?;
        let username = validate_username(username)?;
        let encoded = hash_password(password)?;
        match self.insert_user_row(user_id, username, role, Some(&encoded))? {
            UserInsert::Inserted => self.get_user(user_id),
            UserInsert::IdTaken | UserInsert::NameTaken => Err(OrchestratorError::Conflict(
                format!("user '{user_id}' or username '{username}' already exists"),
            )),
        }
    }

    /// Org-admin create for `POST /v1/users`: the node mints the id (`usr_` plus 26 characters
    /// `[a-z2-7]`). A unique username (ignoring letter case), an argon2id password and a role.
    /// Cannot create `operator`.
    pub fn create_managed_user_generated(
        &self,
        username: &str,
        password: &str,
        role: UserRole,
    ) -> Result<UserRecord, OrchestratorError> {
        if role == UserRole::Operator {
            return Err(OrchestratorError::PermissionDenied(
                "cannot create operator via users:admin".to_string(),
            ));
        }
        self.create_generated_user(username, Some(password), role)
    }

    /// Create a user under a freshly minted id. The password, when given, is hashed once. A
    /// primary-key clash mints a new id (at most [`MAX_GENERATED_ID_ATTEMPTS`] tries).
    pub(crate) fn create_generated_user(
        &self,
        username: &str,
        password: Option<&str>,
        role: UserRole,
    ) -> Result<UserRecord, OrchestratorError> {
        self.create_generated_user_with(username, password, role, || {
            crate::spaces::mint_id(GENERATED_USER_ID_PREFIX)
        })
    }

    /// [`Self::create_generated_user`] with the id minter passed in, so a test can force a
    /// primary-key clash.
    pub(crate) fn create_generated_user_with(
        &self,
        username: &str,
        password: Option<&str>,
        role: UserRole,
        mut mint: impl FnMut() -> String,
    ) -> Result<UserRecord, OrchestratorError> {
        let username = validate_username(username)?;
        let encoded = password.map(hash_password).transpose()?;
        for _ in 0..MAX_GENERATED_ID_ATTEMPTS {
            let user_id = mint();
            match self.insert_user_row(&user_id, username, role, encoded.as_deref())? {
                UserInsert::Inserted => return self.get_user(&user_id),
                UserInsert::IdTaken => continue,
                UserInsert::NameTaken => {
                    return Err(OrchestratorError::Conflict(format!(
                        "username '{username}' is already taken (usernames are matched ignoring letter case)"
                    )));
                }
            }
        }
        Err(OrchestratorError::Conflict(
            "could not generate an unused user id; try again".to_string(),
        ))
    }

    /// One insert of a `users` row. The username is checked first (taken ignoring letter case,
    /// or another user's id), so the answer does not depend on which unique index exists.
    fn insert_user_row(
        &self,
        user_id: &str,
        username: &str,
        role: UserRole,
        password_hash: Option<&str>,
    ) -> Result<UserInsert, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        ensure_username_available(&connection, username, user_id)?;
        let now = unix_timestamp();
        let result = match password_hash {
            Some(hash) => connection.execute(
                "INSERT INTO users (user_id, username, tenant_id, created_at, role, password_hash, password_updated_at)
                 VALUES (?1, ?2, ?1, ?3, ?4, ?5, ?3)",
                params![user_id, username, now, role.as_str(), hash],
            ),
            None => connection.execute(
                "INSERT INTO users (user_id, username, tenant_id, created_at, role)
                 VALUES (?1, ?2, ?1, ?3, ?4)",
                params![user_id, username, now, role.as_str()],
            ),
        };
        match result {
            Ok(_) => Ok(UserInsert::Inserted),
            Err(e) if is_unique_violation(&e) => {
                if e.to_string().contains("users.user_id") {
                    Ok(UserInsert::IdTaken)
                } else {
                    Ok(UserInsert::NameTaken)
                }
            }
            Err(e) => Err(OrchestratorError::Database(e)),
        }
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

    /// Look a user up by login name, ignoring ASCII letter case.
    ///
    /// With the case-insensitive unique index there is at most one match. On a database that
    /// still has clashing usernames (`vamd` refuses to serve it, but the offline flags run on
    /// it) an exact-case match wins, and a name that matches several rows only by case is a
    /// conflict rather than an arbitrary pick.
    pub fn get_user_by_username(&self, username: &str) -> Result<UserRecord, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let mut stmt = connection.prepare(
            "SELECT user_id, username, tenant_id, created_at, password_hash, password_updated_at, role, deactivated_at
             FROM users WHERE username = ?1 COLLATE NOCASE
             ORDER BY (username = ?1) DESC, created_at ASC, user_id ASC LIMIT 2",
        )?;
        let mut rows = stmt
            .query_map([username], parse_user_row)?
            .collect::<Result<Vec<_>, _>>()?
            .into_iter();
        match (rows.next(), rows.next()) {
            (None, _) => Err(OrchestratorError::NotFound(format!(
                "username '{}' not found",
                username
            ))),
            (Some(only), None) => Ok(only),
            (Some(first), Some(_)) if first.username == username => Ok(first),
            (Some(_), Some(_)) => Err(OrchestratorError::Conflict(format!(
                "username '{username}' matches more than one user ignoring letter case; use the user id"
            ))),
        }
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
    ///
    /// The new name follows [`validate_username`] and must be free: another user holding it
    /// ignoring letter case (or a unique-index violation) is a conflict, and a name equal to
    /// another user's id is refused. A user may change only the letter case of their own name.
    /// No HTTP route renames; `vamd --rename-user` is the offline caller.
    pub fn update_username(
        &self,
        user_id: &str,
        new_username: &str,
    ) -> Result<UserRecord, OrchestratorError> {
        let new_username = validate_username(new_username)?;
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let exists: Option<i64> = connection
            .query_row("SELECT 1 FROM users WHERE user_id = ?1", [user_id], |row| {
                row.get(0)
            })
            .optional()?;
        if exists.is_none() {
            return Err(OrchestratorError::NotFound(format!(
                "user '{}' not found",
                user_id
            )));
        }
        ensure_username_available(&connection, new_username, user_id)?;
        match connection.execute(
            "UPDATE users SET username = ?1 WHERE user_id = ?2",
            params![new_username, user_id],
        ) {
            Ok(_) => {}
            Err(e) if is_unique_violation(&e) => {
                return Err(OrchestratorError::Conflict(format!(
                    "username '{new_username}' is already taken (usernames are matched ignoring letter case)"
                )));
            }
            Err(e) => return Err(OrchestratorError::Database(e)),
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

    /// The one resolution rule for a value that may be an id or a login name (login,
    /// `/v1/users/{x}/keys*`, `POST /v1/keys` `userId`, `vamd --reset-password`): an exact,
    /// case-sensitive `user_id` wins; otherwise the `username` matches ignoring ASCII letter
    /// case. Guards that protect the operator must check the **resolved** `user_id`.
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

/// The id rules, unchanged: non-empty, at most 64 characters, ASCII letters, digits, `.`, `-`,
/// `_`. Existing ids and the ids of seed rows pass; generated ids (`usr_` + 26 of `[a-z2-7]`)
/// pass too.
fn validate_user_id(value: &str) -> Result<&str, OrchestratorError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(OrchestratorError::InvalidContract(
            "user_id must be non-empty".to_string(),
        ));
    }
    if trimmed.len() > MAX_USER_ID_CHARS {
        return Err(OrchestratorError::InvalidContract(format!(
            "user_id must be at most {MAX_USER_ID_CHARS} characters"
        )));
    }
    if !trimmed
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        return Err(OrchestratorError::InvalidContract(
            "user_id may contain only ASCII letters, digits, '.', '-', and '_'".to_string(),
        ));
    }
    Ok(trimmed)
}

/// The login-name rules for a new or renamed username (decision 16): non-empty, at most
/// [`MAX_USERNAME_CHARS`] characters, ASCII letters, digits, `.`, `-`, `_`, `+` and `@`, at most
/// one `@`, not starting with `usr_` (any letter case: generated ids) and not `bootstrap` (any
/// letter case: the node's own key). Leading and trailing white space is trimmed. Whether the
/// name is free (letter case ignored, not another user's id) is checked by the store.
pub fn validate_username(value: &str) -> Result<&str, OrchestratorError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(OrchestratorError::InvalidContract(
            "username must be non-empty".to_string(),
        ));
    }
    if trimmed.chars().count() > MAX_USERNAME_CHARS {
        return Err(OrchestratorError::InvalidContract(format!(
            "username must be at most {MAX_USERNAME_CHARS} characters"
        )));
    }
    if !trimmed.chars().all(|c| {
        c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' || c == '+' || c == '@'
    }) {
        return Err(OrchestratorError::InvalidContract(
            "username may contain only ASCII letters, digits, '.', '-', '_', '+', and '@'"
                .to_string(),
        ));
    }
    if trimmed.matches('@').count() > 1 {
        return Err(OrchestratorError::InvalidContract(
            "username may contain at most one '@'".to_string(),
        ));
    }
    let lower = trimmed.to_ascii_lowercase();
    if lower.starts_with(GENERATED_USER_ID_PREFIX) {
        return Err(OrchestratorError::InvalidContract(format!(
            "username must not start with '{GENERATED_USER_ID_PREFIX}' (reserved for generated user ids)"
        )));
    }
    if lower == RESERVED_BOOTSTRAP_NAME {
        return Err(OrchestratorError::InvalidContract(format!(
            "username '{RESERVED_BOOTSTRAP_NAME}' is reserved"
        )));
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
        assert!(
            is_generated_user_id(&admin.user_id),
            "the first admin gets a generated id, got {}",
            admin.user_id
        );
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
        assert_eq!(again.user_id, admin.user_id);
        // ... also when the name is typed in another letter case.
        let shouted = store
            .bootstrap_first_admin("ADMIN", "ignored-on-repeat")
            .expect("idempotent ignoring case");
        assert_eq!(shouted.user_id, admin.user_id);
        assert_eq!(store.user_count().expect("users"), 3);
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

    // ---- Phase 2: generated ids, login names, resolution (user-identity UI-2.2 to UI-2.4) ----

    fn remove_db(path: &std::path::Path) {
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(format!("{}-wal", path.display()));
        let _ = std::fs::remove_file(format!("{}-shm", path.display()));
    }

    /// A database as an older node left it: a `users` table with the first four columns and
    /// the given `(user_id, username)` rows, no index but the column's `UNIQUE`.
    fn old_schema_db(label: &str, rows: &[(&str, &str)]) -> PathBuf {
        let path = temp_db_path(label);
        let conn = Connection::open(&path).expect("legacy open");
        conn.execute_batch(
            "CREATE TABLE users (
                user_id TEXT PRIMARY KEY NOT NULL,
                username TEXT NOT NULL UNIQUE,
                tenant_id TEXT NOT NULL,
                created_at INTEGER NOT NULL
            );",
        )
        .expect("legacy schema");
        for (i, (user_id, username)) in rows.iter().enumerate() {
            conn.execute(
                "INSERT INTO users (user_id, username, tenant_id, created_at) VALUES (?1, ?2, ?1, ?3)",
                params![user_id, username, 100 + i as i64],
            )
            .expect("legacy row");
        }
        path
    }

    fn invalid(err: OrchestratorError) -> String {
        match err {
            OrchestratorError::InvalidContract(msg) => msg,
            other => panic!("expected InvalidContract, got {other:?}"),
        }
    }

    #[test]
    fn username_accepts_login_names_and_email_addresses() {
        for ok in [
            "alice",
            "a.b-c_d",
            "Ada.Lovelace+work@Example.com",
            "x@y",
            "first+tag+more@mail.example.test",
            "usr",
            "bootstrapper",
            "user_x",
            "  padded  ",
        ] {
            assert!(validate_username(ok).is_ok(), "{ok:?} should be allowed");
        }
        assert_eq!(validate_username("  padded  ").expect("trim"), "padded");
        // 254 characters pass, 255 do not.
        let longest = format!("{}@{}", "a".repeat(100), "b".repeat(153));
        assert_eq!(longest.len(), 254);
        assert!(validate_username(&longest).is_ok());
        let too_long = format!("{longest}c");
        assert!(invalid(validate_username(&too_long).expect_err("255")).contains("254"));
    }

    #[test]
    fn username_rejects_bad_charset_second_at_and_empty() {
        assert!(invalid(validate_username("").expect_err("empty")).contains("non-empty"));
        assert!(invalid(validate_username("   ").expect_err("blank")).contains("non-empty"));
        for bad in [
            "has space",
            "tab\there",
            "ünï",
            "name/slash",
            "name:colon",
            "a,b",
            "a%40b",
            "quote\"d",
        ] {
            let msg = invalid(validate_username(bad).expect_err(bad));
            assert!(msg.contains("ASCII letters"), "{bad:?}: {msg}");
        }
        let msg = invalid(validate_username("a@b@c").expect_err("two at signs"));
        assert!(msg.contains("one '@'"), "{msg}");
    }

    #[test]
    fn username_rejects_reserved_prefix_and_bootstrap_in_any_case() {
        for reserved in ["usr_abc", "USR_abc", "Usr_", "uSr_x@y.z"] {
            let msg = invalid(validate_username(reserved).expect_err(reserved));
            assert!(msg.contains("usr_"), "{reserved}: {msg}");
        }
        for reserved in ["bootstrap", "Bootstrap", "BOOTSTRAP"] {
            let msg = invalid(validate_username(reserved).expect_err(reserved));
            assert!(msg.contains("reserved"), "{reserved}: {msg}");
        }
    }

    #[test]
    fn user_ids_keep_the_old_validator() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        // An id may not hold '@' or '+' (workspace prefixes embed ids), and stops at 64.
        for bad in ["a@b", "a+b", "", &"x".repeat(65)] {
            assert!(
                matches!(
                    store.create_user(bad, "someone"),
                    Err(OrchestratorError::InvalidContract(_))
                ),
                "id {bad:?} should be refused"
            );
        }
        let sixty_four = "x".repeat(64);
        store
            .create_user(&sixty_four, "long-id-user")
            .expect("64 characters");
        // A username that is allowed to be longer than 64 still is not an id.
        let long_name = "n".repeat(100);
        store
            .create_user("short-id", &long_name)
            .expect("100-char name");
        // A generated id passes the id validator.
        let generated = crate::spaces::mint_id(GENERATED_USER_ID_PREFIX);
        assert!(validate_user_id(&generated).is_ok());
        assert!(generated.len() <= MAX_USER_ID_CHARS);
    }

    #[test]
    fn generated_users_get_usr_ids_that_differ_from_the_username() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        let a = store
            .create_managed_user_generated("dana", "dana-secret", UserRole::Member)
            .expect("create a");
        let b = store
            .create_managed_user_generated("erin@example.test", "erin-secret", UserRole::Publisher)
            .expect("create b");
        for user in [&a, &b] {
            assert!(is_generated_user_id(&user.user_id), "{}", user.user_id);
            assert_ne!(user.user_id, user.username);
            assert_eq!(user.tenant_id, user.user_id);
            assert!(user.has_password);
        }
        assert_ne!(a.user_id, b.user_id);
        assert_eq!(b.role, UserRole::Publisher);
        assert_eq!(b.username, "erin@example.test");
        // Looked up by the id, and by the name in any letter case.
        assert_eq!(store.get_user(&a.user_id).expect("by id").username, "dana");
        assert_eq!(
            store
                .resolve_user("ERIN@Example.TEST")
                .expect("by name")
                .user_id,
            b.user_id
        );
        // Operator stays refused.
        assert!(matches!(
            store.create_managed_user_generated("op2", "pw", UserRole::Operator),
            Err(OrchestratorError::PermissionDenied(_))
        ));
    }

    #[test]
    fn generated_id_retries_on_a_primary_key_clash_at_most_three_times() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        store.create_user("usr_taken", "holder").expect("taken id");
        // Two clashes, then a free id: created on the third attempt.
        let mut calls = 0;
        let user = store
            .create_generated_user_with("retry-ok", None, UserRole::Member, || {
                calls += 1;
                if calls < 3 {
                    "usr_taken".to_string()
                } else {
                    "usr_third".to_string()
                }
            })
            .expect("third attempt");
        assert_eq!(calls, 3);
        assert_eq!(user.user_id, "usr_third");
        // Three clashes: it gives up, and leaves no row.
        let mut calls = 0;
        let err = store
            .create_generated_user_with("retry-fail", None, UserRole::Member, || {
                calls += 1;
                "usr_taken".to_string()
            })
            .expect_err("three clashes");
        assert_eq!(calls, 3);
        assert!(matches!(err, OrchestratorError::Conflict(_)), "{err:?}");
        assert!(store.get_user_by_username("retry-fail").is_err());
        // A taken username is a conflict at once, not three retries.
        let mut calls = 0;
        let err = store
            .create_generated_user_with("RETRY-OK", None, UserRole::Member, || {
                calls += 1;
                format!("usr_fresh{calls}")
            })
            .expect_err("username taken");
        assert!(matches!(err, OrchestratorError::Conflict(_)), "{err:?}");
        assert_eq!(calls, 1);
    }

    #[test]
    fn a_username_equal_ignoring_case_to_another_is_a_conflict() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        store
            .create_managed_user_generated("Ada.Lovelace@Example.Test", "pw-ada", UserRole::Member)
            .expect("first");
        for twin in ["ada.lovelace@example.test", "ADA.LOVELACE@EXAMPLE.TEST"] {
            let err = store
                .create_managed_user_generated(twin, "pw-twin", UserRole::Member)
                .expect_err(twin);
            assert!(
                matches!(err, OrchestratorError::Conflict(_)),
                "{twin}: {err:?}"
            );
        }
        // The seed rows hold their names too.
        let err = store
            .create_managed_user_generated("OPERATOR", "pw-op", UserRole::Member)
            .expect_err("operator name");
        assert!(matches!(err, OrchestratorError::Conflict(_)), "{err:?}");
        // The explicit-id methods answer the same, and no row appears.
        let before = store.user_count().expect("count");
        let err = store
            .create_user("fresh-id", "LOCAL")
            .expect_err("local name");
        assert!(matches!(err, OrchestratorError::Conflict(_)), "{err:?}");
        assert_eq!(store.user_count().expect("count"), before);
    }

    #[test]
    fn the_index_enforces_it_below_the_api() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        let connection = store.connection.lock().expect("lock");
        let err = connection
            .execute(
                "INSERT INTO users (user_id, username, tenant_id, created_at)
                 VALUES ('other', 'OPERATOR', 'other', 1)",
                [],
            )
            .expect_err("a case-different duplicate must fail");
        assert!(is_unique_violation(&err), "{err}");
    }

    #[test]
    fn a_username_may_not_equal_another_users_id() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        store.create_user("carol", "caroline").expect("carol");
        for taken in ["carol", "CAROL"] {
            let msg = invalid(
                store
                    .create_managed_user_generated(taken, "pw", UserRole::Member)
                    .expect_err(taken),
            );
            assert!(msg.contains("id of another user"), "{taken}: {msg}");
        }
        // Renaming into another user's id is refused the same way.
        store.create_user("dave", "dave").expect("dave");
        let msg = invalid(store.update_username("dave", "Carol").expect_err("rename"));
        assert!(msg.contains("id of another user"), "{msg}");
        // A user's own id is fine as their own name.
        store
            .update_username("carol", "carol")
            .expect("own id as name");
    }

    #[test]
    fn rename_validates_refuses_a_clash_and_keeps_the_id() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        let ada = store
            .create_managed_user_generated("ada", "pw-ada", UserRole::Member)
            .expect("ada");
        store
            .create_managed_user_generated("bea", "pw-bea", UserRole::Member)
            .expect("bea");
        // Validation.
        for bad in ["", "a b", "x@y@z", "usr_new", "Bootstrap"] {
            assert!(
                matches!(
                    store.update_username(&ada.user_id, bad),
                    Err(OrchestratorError::InvalidContract(_))
                ),
                "rename to {bad:?}"
            );
        }
        // A name another user holds, in any letter case, is a conflict.
        for taken in ["bea", "BEA", "Bea"] {
            assert!(
                matches!(
                    store.update_username(&ada.user_id, taken),
                    Err(OrchestratorError::Conflict(_))
                ),
                "rename to {taken:?}"
            );
        }
        // Unknown user.
        assert!(matches!(
            store.update_username("nobody", "whoever"),
            Err(OrchestratorError::NotFound(_))
        ));
        // Success keeps the id, and a change of letter case only is allowed.
        let renamed = store
            .update_username(&ada.user_id, "Ada.L+x@Example.Test")
            .expect("rename");
        assert_eq!(renamed.user_id, ada.user_id);
        assert_eq!(renamed.username, "Ada.L+x@Example.Test");
        let recased = store
            .update_username(&ada.user_id, "ada.l+x@example.test")
            .expect("recase");
        assert_eq!(recased.username, "ada.l+x@example.test");
        assert!(store.get_user_by_username("ada").is_err());
    }

    #[test]
    fn the_unique_index_is_created_once_and_kept() {
        let path = old_schema_db("index-once", &[("alice", "alice"), ("bob", "Bob")]);
        let store = SqliteWorkflowStore::open(&path).expect("first open");
        assert!(store.username_nocase_index_exists().expect("index"));
        assert!(store.username_clashes().is_empty());
        drop(store);
        let again = SqliteWorkflowStore::open(&path).expect("second open");
        assert!(again.username_nocase_index_exists().expect("index"));
        assert!(again.username_clashes().is_empty());
        // Exactly one index of that name.
        let count: i64 = again
            .connection
            .lock()
            .expect("lock")
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = ?1",
                [USERNAME_NOCASE_INDEX],
                |row| row.get(0),
            )
            .expect("count");
        assert_eq!(count, 1);
        // A fresh store has it too.
        let fresh = SqliteWorkflowStore::in_memory().expect("fresh");
        assert!(fresh.username_nocase_index_exists().expect("index"));
        drop(again);
        remove_db(&path);
    }

    #[test]
    fn a_clash_leaves_no_index_is_held_on_the_store_and_open_still_succeeds() {
        let path = old_schema_db(
            "clash",
            &[
                ("alice", "alice"),
                ("bob", "bob"),
                ("bob-2", "Bob"),
                ("cy", "cy"),
                ("cy-2", "CY"),
                ("cy-3", "Cy"),
            ],
        );
        let store = SqliteWorkflowStore::open(&path).expect("a clash is not an open error");
        assert!(!store.username_nocase_index_exists().expect("index"));
        let clashes = store.username_clashes();
        assert_eq!(clashes.len(), 2, "{clashes:?}");
        assert_eq!(
            clashes[0].members,
            vec![
                ("bob".to_string(), "bob".to_string()),
                ("bob-2".to_string(), "Bob".to_string())
            ]
        );
        assert_eq!(clashes[1].members.len(), 3);
        // Every row is untouched.
        assert_eq!(store.get_user("bob-2").expect("row").username, "Bob");
        let message = username_clash_message(&clashes);
        for needle in [
            "bob (user id bob)",
            "Bob (user id bob-2)",
            "CY (user id cy-2)",
            "vamd --rename-user <user_id> <new_username>",
            "start vamd again",
        ] {
            assert!(
                message.contains(needle),
                "{needle:?} missing in:\n{message}"
            );
        }
        // Reopening with the clash still there stays the same and creates nothing.
        drop(store);
        let again = SqliteWorkflowStore::open(&path).expect("reopen");
        assert!(!again.username_nocase_index_exists().expect("index"));
        assert_eq!(again.username_clashes().len(), 2);
        // Fix every group, then the next open creates the index.
        again
            .update_username("bob-2", "bobby")
            .expect("rename bob-2");
        again
            .update_username("cy-2", "cy-two")
            .expect("rename cy-2");
        // A rename that would still clash is refused.
        assert!(matches!(
            again.update_username("cy-3", "CY"),
            Err(OrchestratorError::Conflict(_))
        ));
        again
            .update_username("cy-3", "cy-three")
            .expect("rename cy-3");
        assert!(!again
            .username_nocase_index_exists()
            .expect("no index until a start"));
        drop(again);
        let fixed = SqliteWorkflowStore::open(&path).expect("open after the fix");
        assert!(fixed.username_nocase_index_exists().expect("index"));
        assert!(fixed.username_clashes().is_empty());
        drop(fixed);
        remove_db(&path);
    }

    #[test]
    fn existing_users_keep_their_ids_names_and_cross_matches() {
        // `usr_old` starts like a generated id, `bootstrap` is the node's key name, and user
        // `y` is named like user `carol`'s id.
        let path = old_schema_db(
            "legacy",
            &[
                ("carol", "caroline"),
                ("y", "carol"),
                ("usr_old", "usr_old"),
                ("bootstrap", "bootstrap"),
            ],
        );
        let store = SqliteWorkflowStore::open(&path).expect("open");
        assert!(store.username_clashes().is_empty());
        // Nothing is renamed.
        assert_eq!(store.get_user("usr_old").expect("row").username, "usr_old");
        assert_eq!(
            store.get_user("bootstrap").expect("row").username,
            "bootstrap"
        );
        // One note per legacy cross-match: `usr_old`, `bootstrap` (both as user and name).
        let notes = store.legacy_identity_notes().expect("notes");
        assert!(
            notes
                .iter()
                .any(|n| n.contains("'usr_old'") && n.contains("reserved for generated ids")),
            "{notes:?}"
        );
        assert!(
            notes
                .iter()
                .any(|n| n.contains("'bootstrap'") && n.contains("its own key")),
            "{notes:?}"
        );
        assert!(
            notes
                .iter()
                .any(|n| n.contains("username 'carol' (user id y) is the id of user 'carol'")),
            "{notes:?}"
        );
        drop(store);
        remove_db(&path);
    }

    #[test]
    fn the_id_wins_over_a_legacy_username_equal_to_it() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        // User `x` is named `carol`, and user `carol` exists under another name.
        store.create_user("x", "carol").expect("x");
        {
            let connection = store.connection.lock().expect("lock");
            // A legacy row the new rules would refuse: its username `dave` equals no id, but the
            // id `carol` below equals user `x`'s username.
            connection
                .execute(
                    "INSERT INTO users (user_id, username, tenant_id, created_at)
                     VALUES ('carol', 'caroline', 'carol', 1)",
                    [],
                )
                .expect("legacy row");
        }
        assert_eq!(
            store.resolve_user("carol").expect("id first").user_id,
            "carol"
        );
        // The name still reaches `x` in another letter case, which is not an id.
        assert_eq!(store.resolve_user("CAROL").expect("name").user_id, "x");
        assert_eq!(
            store.resolve_user("caroline").expect("name").user_id,
            "carol"
        );
        let notes = store.legacy_identity_notes().expect("notes");
        assert!(
            notes
                .iter()
                .any(|n| n.contains("username 'carol' (user id x) is the id of user 'carol'")),
            "{notes:?}"
        );
    }

    #[test]
    fn resolution_is_id_first_then_name_ignoring_case() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        let ada = store
            .create_managed_user_generated("Ada+tag@Example.Test", "pw-ada", UserRole::Member)
            .expect("ada");
        for input in [
            ada.user_id.as_str(),
            "Ada+tag@Example.Test",
            "ada+tag@example.test",
            "ADA+TAG@EXAMPLE.TEST",
        ] {
            let user = store.resolve_user(input).expect(input);
            assert_eq!(user.user_id, ada.user_id, "{input}");
            // The stored name is returned as it was typed at creation.
            assert_eq!(user.username, "Ada+tag@Example.Test");
        }
        assert!(matches!(
            store.resolve_user("nobody"),
            Err(OrchestratorError::NotFound(_))
        ));
        // The id is case-sensitive: the upper-cased id is no user.
        assert!(store.resolve_user(&ada.user_id.to_uppercase()).is_err());
        assert!(store
            .verify_user_password("ADA+TAG@example.test", "pw-ada")
            .expect("verify"));
    }

    #[test]
    fn a_name_that_matches_several_rows_only_by_case_is_ambiguous() {
        let path = old_schema_db("ambiguous", &[("bob", "bob"), ("bob-2", "Bob")]);
        let store = SqliteWorkflowStore::open(&path).expect("open");
        // An exact-case match wins; the offline tools can still reach each.
        assert_eq!(store.resolve_user("bob").expect("exact").user_id, "bob");
        assert_eq!(store.resolve_user("Bob").expect("exact").user_id, "bob-2");
        assert!(matches!(
            store.resolve_user("BOB"),
            Err(OrchestratorError::Conflict(_))
        ));
        drop(store);
        remove_db(&path);
    }

    #[test]
    fn the_operator_guard_on_the_resolved_id_catches_any_letter_case() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        for raw in ["operator", "OPERATOR", "Operator", "oPeRaToR"] {
            let user = store.resolve_user(raw).expect(raw);
            assert!(
                matches!(
                    reject_operator_user_admin_target(&user.user_id),
                    Err(OrchestratorError::PermissionDenied(_))
                ),
                "{raw}"
            );
        }
        let other = store
            .create_managed_user_generated("ordinary", "pw", UserRole::Member)
            .expect("user");
        assert!(reject_operator_user_admin_target(
            &store.resolve_user("ORDINARY").expect("r").user_id
        )
        .is_ok());
        assert_eq!(other.username, "ordinary");
    }

    #[test]
    fn bootstrap_refuses_reserved_names_and_mints_a_generated_id() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        for reserved in ["bootstrap", "usr_boss"] {
            assert!(
                matches!(
                    store.bootstrap_first_admin(reserved, "secret"),
                    Err(OrchestratorError::InvalidContract(_))
                ),
                "{reserved}"
            );
        }
        let admin = store
            .bootstrap_first_admin("Boss@Example.Test", "secret")
            .expect("email admin");
        assert!(is_generated_user_id(&admin.user_id));
        assert_eq!(admin.username, "Boss@Example.Test");
        assert!(store
            .verify_user_password("boss@example.test", "secret")
            .expect("verify"));
    }
}
