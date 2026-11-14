//! Owner stamps and Phase 3 owner-scoped reads.
//!
//! Session, run, approval, and API-key list/get filter on `owner_user_id`.
//! Cross-user get is NotFound — the same as missing. HITL decide also requires
//! the caller to match the row's [`crate::hitl::HitlApprover`] (default: run
//! owner). Session and run idempotency keys are unique per `owner_user_id`.
//! `keys:admin` may still list/revoke every key. Host secrets use vault key
//! `(scope, owner_user_id, binding_id)`. Catalog list/get returns the caller's
//! own drafts plus tenant published versions. Callers with `users:admin` also
//! see other people's drafts (same GET paths). Identity comes only from the
//! `users` table.

use crate::error::OrchestratorError;
use crate::keys::ApiKeyRecord;
use crate::users::{LOCAL_USER_ID, OPERATOR_USER_ID};
use crate::workflow::SqliteWorkflowStore;
use rusqlite::{params, Connection, OptionalExtension};

/// Cedar principal minted for the deployment bootstrap key.
pub const BOOTSTRAP_PRINCIPAL: &str = "User::\"bootstrap\"";
/// Stable `api_keys.key_id` for the first-start bootstrap key.
pub const BOOTSTRAP_KEY_ID: &str = "bootstrap";

/// Map a calling API key to a `users.user_id`.
///
/// Only the genuine bootstrap key (`key_id` = [`BOOTSTRAP_KEY_ID`], minted
/// server-side at first start) is the operator. Every other key uses the stored
/// `api_keys.owner_user_id`; an empty owner falls back to `local`. The Cedar
/// principal string is never consulted: principals are caller-influenced (a
/// key name, a client-sent field, a username).
pub fn owner_user_id_for_authenticated_key(record: &ApiKeyRecord) -> &str {
    if is_bootstrap_key(record.key_id.as_str()) {
        OPERATOR_USER_ID
    } else if !record.owner_user_id.is_empty() {
        record.owner_user_id.as_str()
    } else {
        LOCAL_USER_ID
    }
}

/// Map a key id to a `users.user_id` for a key with no stored owner. The
/// principal is accepted for call-shape compatibility and ignored.
pub fn owner_user_id_for_key(key_id: &str, _principal: &str) -> &'static str {
    if is_bootstrap_key(key_id) {
        OPERATOR_USER_ID
    } else {
        LOCAL_USER_ID
    }
}

/// True when this is the deployment bootstrap key. Decided by the key id alone.
pub fn is_bootstrap_key(key_id: &str) -> bool {
    key_id == BOOTSTRAP_KEY_ID
}

/// True when a name is reserved for the deployment bootstrap key, ignoring
/// ASCII case: the bootstrap name itself or the bootstrap Cedar principal.
pub fn is_reserved_bootstrap_name(name: &str) -> bool {
    let name = name.trim();
    name.eq_ignore_ascii_case(BOOTSTRAP_KEY_ID) || name.eq_ignore_ascii_case(BOOTSTRAP_PRINCIPAL)
}

/// Refuse a key name or client-sent principal reserved for the bootstrap key.
pub fn reject_reserved_bootstrap_name(field: &str, name: &str) -> Result<(), OrchestratorError> {
    if is_reserved_bootstrap_name(name) {
        return Err(OrchestratorError::InvalidContract(format!(
            "{field} '{BOOTSTRAP_KEY_ID}' is reserved"
        )));
    }
    Ok(())
}

/// Tables that persist a Phase 1 `owner_user_id` stamp (recorded, not ACL).
const OWNER_STAMP_TABLES: &[&str] = &[
    "sessions",
    "runs",
    "approvals",
    "api_keys",
    "host_secrets",
    "agents",
    "playbooks",
    "skills",
    "policies",
    "skill_packages",
];

impl SqliteWorkflowStore {
    /// Add `owner_user_id` (FK → `users`) on control-plane rows; backfill
    /// remaining NULLs. Idempotent. Session/run list/get are owner-scoped;
    /// host-secret lookup uses `(scope, owner_user_id, binding_id)` after
    /// [`Self::initialize_vault_key`]. Catalog list/get is own drafts plus
    /// tenant published versions.
    pub fn initialize_owner_stamps(&self) -> Result<(), OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;

        for table in OWNER_STAMP_TABLES {
            add_owner_column(&connection, table)?;
        }

        connection.execute_batch(
            "
            CREATE INDEX IF NOT EXISTS idx_sessions_owner_user_id ON sessions(owner_user_id);
            CREATE INDEX IF NOT EXISTS idx_runs_owner_user_id ON runs(owner_user_id);
            CREATE INDEX IF NOT EXISTS idx_approvals_owner_user_id ON approvals(owner_user_id);
            CREATE INDEX IF NOT EXISTS idx_api_keys_owner_user_id ON api_keys(owner_user_id);
            CREATE INDEX IF NOT EXISTS idx_host_secrets_owner_user_id ON host_secrets(owner_user_id);
            CREATE INDEX IF NOT EXISTS idx_agents_owner_user_id ON agents(owner_user_id);
            CREATE INDEX IF NOT EXISTS idx_playbooks_owner_user_id ON playbooks(owner_user_id);
            CREATE INDEX IF NOT EXISTS idx_skills_owner_user_id ON skills(owner_user_id);
            CREATE INDEX IF NOT EXISTS idx_policies_owner_user_id ON policies(owner_user_id);
            CREATE INDEX IF NOT EXISTS idx_skill_packages_owner_user_id ON skill_packages(owner_user_id);
            ",
        )?;

        // The genuine bootstrap key, by key id only, is the operator (ALTER DEFAULT may
        // have filled 'local'). A principal string never promotes a row: principals
        // are caller-influenced, so another key's stored owner is never rewritten here.
        connection.execute(
            "UPDATE api_keys SET owner_user_id = ?1 WHERE key_id = ?2",
            params![OPERATOR_USER_ID, BOOTSTRAP_KEY_ID],
        )?;
        for table in OWNER_STAMP_TABLES {
            // table names are a fixed allow-list.
            let sql = format!("UPDATE {table} SET owner_user_id = ?1 WHERE owner_user_id IS NULL");
            connection.execute(&sql, params![LOCAL_USER_ID])?;
        }
        Ok(())
    }

    /// Replace global `UNIQUE(idempotency_key)` on sessions and runs with
    /// `UNIQUE(owner_user_id, idempotency_key)`. Host-secret uniqueness is
    /// migrated separately in [`SqliteWorkflowStore::initialize_vault_key`].
    /// Idempotent. Existing owners are backfilled first so
    /// the composite key never includes a NULL owner.
    pub fn initialize_per_user_idempotency(&self) -> Result<(), OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;

        for table in ["sessions", "runs"] {
            connection.execute(
                &format!(
                    "UPDATE {table} SET owner_user_id = ?1
                     WHERE owner_user_id IS NULL OR owner_user_id = ''"
                ),
                params![LOCAL_USER_ID],
            )?;
        }

        drop_global_idempotency_unique(&connection, "sessions")?;
        drop_global_idempotency_unique(&connection, "runs")?;

        connection.execute_batch(
            "
            CREATE UNIQUE INDEX IF NOT EXISTS idx_sessions_owner_idempotency
                ON sessions(owner_user_id, idempotency_key)
                WHERE idempotency_key IS NOT NULL;
            CREATE UNIQUE INDEX IF NOT EXISTS idx_runs_owner_idempotency
                ON runs(owner_user_id, idempotency_key)
                WHERE idempotency_key IS NOT NULL;
            CREATE INDEX IF NOT EXISTS idx_sessions_observed ON sessions(observed_state);
            CREATE UNIQUE INDEX IF NOT EXISTS idx_sessions_active_cid
                ON sessions(guest_cid) WHERE observed_state NOT IN ('TERMINATED', 'FAILED');
            CREATE UNIQUE INDEX IF NOT EXISTS idx_sessions_active_pid
                ON sessions(vmm_pid) WHERE observed_state NOT IN ('TERMINATED', 'FAILED');
            CREATE UNIQUE INDEX IF NOT EXISTS idx_sessions_active_tap
                ON sessions(tap_device) WHERE observed_state NOT IN ('TERMINATED', 'FAILED');
            CREATE UNIQUE INDEX IF NOT EXISTS idx_sessions_active_cgroup
                ON sessions(cgroup_path) WHERE observed_state NOT IN ('TERMINATED', 'FAILED');
            CREATE INDEX IF NOT EXISTS idx_sessions_owner_user_id ON sessions(owner_user_id);
            CREATE INDEX IF NOT EXISTS idx_runs_owner_user_id ON runs(owner_user_id);
            ",
        )?;
        Ok(())
    }

    pub fn session_owner_user_id(&self, session_id: &str) -> Result<String, OrchestratorError> {
        self.query_owner("sessions", "session_id", session_id, "session")
    }

    pub fn run_owner_user_id(&self, run_id: &str) -> Result<String, OrchestratorError> {
        self.query_owner("runs", "run_id", run_id, "run")
    }

    /// Confirm `run_id` is owned by `owner_user_id`. Missing or foreign →
    /// the same NotFound as a missing run (no existence leak).
    pub fn require_run_owner(
        &self,
        run_id: &str,
        owner_user_id: &str,
    ) -> Result<(), OrchestratorError> {
        let missing = OrchestratorError::NotFound(format!("run '{run_id}' not found"));
        match self.run_owner_user_id(run_id) {
            Ok(owner) if owner == resolved_owner(owner_user_id) => Ok(()),
            Ok(_) => Err(missing),
            Err(OrchestratorError::NotFound(_)) => Err(missing),
            Err(e) => Err(e),
        }
    }

    pub fn approval_owner_user_id(&self, request_id: &str) -> Result<String, OrchestratorError> {
        self.query_owner("approvals", "request_id", request_id, "approval")
    }

    /// Confirm `request_id` is owned by `owner_user_id`. Missing or foreign →
    /// the same ApprovalNotFound as a missing approval (no existence leak).
    pub fn require_approval_owner(
        &self,
        request_id: &str,
        owner_user_id: &str,
    ) -> Result<(), OrchestratorError> {
        let missing = OrchestratorError::ApprovalNotFound(request_id.to_string());
        match self.approval_owner_user_id(request_id) {
            Ok(owner) if owner == resolved_owner(owner_user_id) => Ok(()),
            Ok(_) => Err(missing),
            Err(OrchestratorError::NotFound(_)) => Err(missing),
            Err(e) => Err(e),
        }
    }

    pub fn host_secret_owner_user_id(
        &self,
        scope: crate::secrets::SecretScope,
        owner_user_id: &str,
        binding_id: &str,
    ) -> Result<String, OrchestratorError> {
        Ok(self
            .get_host_secret_metadata(scope, owner_user_id, binding_id)?
            .owner_user_id)
    }

    pub fn agent_owner_user_id(&self, urn: &str) -> Result<String, OrchestratorError> {
        self.query_catalog_owner("agents", urn, "agent")
    }

    pub fn playbook_owner_user_id(&self, urn: &str) -> Result<String, OrchestratorError> {
        self.query_catalog_owner("playbooks", urn, "playbook")
    }

    pub fn skill_owner_user_id(&self, urn: &str) -> Result<String, OrchestratorError> {
        self.query_catalog_owner("skills", urn, "skill")
    }

    pub fn policy_owner_user_id(&self, urn: &str) -> Result<String, OrchestratorError> {
        self.query_catalog_owner("policies", urn, "policy")
    }

    pub fn skill_package_owner_user_id(
        &self,
        urn: &str,
        major: i32,
        minor: i32,
        patch: i32,
        prerelease: &str,
    ) -> Result<String, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        connection
            .query_row(
                "SELECT owner_user_id FROM skill_packages
                 WHERE urn = ?1 AND major = ?2 AND minor = ?3 AND patch = ?4 AND prerelease = ?5",
                params![urn, major, minor, patch, prerelease],
                |row| row.get::<_, Option<String>>(0),
            )
            .map_err(|e| {
                if e == rusqlite::Error::QueryReturnedNoRows {
                    OrchestratorError::NotFound(format!("skill package '{urn}' not found"))
                } else {
                    OrchestratorError::Database(e)
                }
            })?
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                OrchestratorError::InvalidContract(format!(
                    "skill package '{urn}' has NULL owner_user_id"
                ))
            })
    }

    pub fn stamp_session_owner(
        &self,
        session_id: &str,
        owner_user_id: &str,
    ) -> Result<(), OrchestratorError> {
        let owner = resolved_owner(owner_user_id);
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let changed = connection.execute(
            "UPDATE sessions SET owner_user_id = ?1 WHERE session_id = ?2",
            params![owner, session_id],
        )?;
        if changed == 0 {
            return Err(OrchestratorError::NotFound(format!(
                "session '{}' not found",
                session_id
            )));
        }
        Ok(())
    }

    /// Sessions/runs that have an idempotency key but no owner (should be 0).
    pub fn count_null_owners_on_idempotency_keys(&self) -> Result<i64, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let sessions: i64 = connection.query_row(
            "SELECT COUNT(*) FROM sessions
             WHERE idempotency_key IS NOT NULL AND idempotency_key != ''
               AND (owner_user_id IS NULL OR owner_user_id = '')",
            [],
            |row| row.get(0),
        )?;
        let runs: i64 = connection.query_row(
            "SELECT COUNT(*) FROM runs
             WHERE idempotency_key IS NOT NULL AND idempotency_key != ''
               AND (owner_user_id IS NULL OR owner_user_id = '')",
            [],
            |row| row.get(0),
        )?;
        Ok(sessions + runs)
    }

    /// True when `table` still has a single-column UNIQUE on `idempotency_key`.
    pub fn has_global_idempotency_unique(&self, table: &str) -> Result<bool, OrchestratorError> {
        if !is_safe_ident(table) {
            return Err(OrchestratorError::InvalidContract(format!(
                "unsafe table name '{table}'"
            )));
        }
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        has_single_column_unique(&connection, table, "idempotency_key")
    }

    /// Rows still missing an owner after migration (should be 0).
    pub fn count_null_owners(&self) -> Result<i64, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let mut total = 0i64;
        for table in OWNER_STAMP_TABLES {
            // table names are a fixed allow-list.
            let sql = format!("SELECT COUNT(*) FROM {table} WHERE owner_user_id IS NULL");
            let count: i64 = connection.query_row(&sql, [], |row| row.get(0))?;
            total += count;
        }
        Ok(total)
    }

    fn query_catalog_owner(
        &self,
        table: &str,
        urn: &str,
        label: &str,
    ) -> Result<String, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let sql = format!(
            "SELECT owner_user_id FROM {table} WHERE urn = ?1
             ORDER BY CASE channel WHEN 'released' THEN 0 WHEN 'draft' THEN 1 ELSE 2 END,
                      updated_at DESC
             LIMIT 1"
        );
        connection
            .query_row(&sql, [urn], |row| row.get::<_, Option<String>>(0))
            .map_err(|e| {
                if e == rusqlite::Error::QueryReturnedNoRows {
                    OrchestratorError::NotFound(format!("{label} '{urn}' not found"))
                } else {
                    OrchestratorError::Database(e)
                }
            })?
            .ok_or_else(|| {
                OrchestratorError::InvalidContract(format!(
                    "{label} '{urn}' has NULL owner_user_id"
                ))
            })
    }

    /// Confirm `urn` in `table` is owned by `caller_user_id`. Foreign owner is
    /// PermissionDenied (the caller already knows the URN). Missing → NotFound.
    pub fn require_catalog_owner(
        &self,
        table: &str,
        urn: &str,
        caller_user_id: &str,
        label: &str,
    ) -> Result<(), OrchestratorError> {
        let owner = self.query_catalog_owner(table, urn, label)?;
        if owner != resolved_owner(caller_user_id) {
            return Err(OrchestratorError::PermissionDenied(format!(
                "only the artifact owner may publish this {label}"
            )));
        }
        Ok(())
    }

    pub fn skill_package_urn_owner_user_id(&self, urn: &str) -> Result<String, OrchestratorError> {
        self.query_catalog_owner("skill_packages", urn, "skill package")
    }

    /// Org admin reassigns `owner_user_id` on every version of a catalog URN.
    /// Channel is unchanged — published rows stay released.
    pub fn reassign_catalog_owner(
        &self,
        kind: &str,
        urn: &str,
        new_owner_user_id: &str,
    ) -> Result<CatalogOwnerReassignment, OrchestratorError> {
        let (table, label) = catalog_table_for_kind(kind)?;
        let urn = urn.trim();
        if urn.is_empty() {
            return Err(OrchestratorError::InvalidContract(
                "urn is required".to_string(),
            ));
        }
        let new_owner = resolved_owner(new_owner_user_id.trim());
        if new_owner.is_empty() {
            return Err(OrchestratorError::InvalidContract(
                "owner_user_id is required".to_string(),
            ));
        }
        if new_owner.starts_with("marketplace:") {
            return Err(OrchestratorError::InvalidContract(
                "marketplace owners are system accounts and cannot be assigned".to_string(),
            ));
        }
        let _new_user = self.get_user(new_owner)?;
        // M2: an installed row keeps its marketplace owner; reassigning it would hand an org user
        // the right to version it.
        self.require_not_installed(urn, label)?;
        let previous = self.query_catalog_owner(table, urn, label)?;
        let now = crate::catalog::unix_timestamp();
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let rows_updated = connection.execute(
            &format!("UPDATE {table} SET owner_user_id = ?1, updated_at = ?2 WHERE urn = ?3"),
            params![new_owner, now, urn],
        )?;
        if rows_updated == 0 {
            return Err(OrchestratorError::NotFound(format!(
                "{label} '{urn}' not found"
            )));
        }
        Ok(CatalogOwnerReassignment {
            kind: label.to_string(),
            urn: urn.to_string(),
            previous_owner_user_id: previous,
            owner_user_id: new_owner.to_string(),
            rows_updated: i32::try_from(rows_updated).unwrap_or(i32::MAX),
        })
    }

    fn query_owner(
        &self,
        table: &str,
        id_column: &str,
        id: &str,
        label: &str,
    ) -> Result<String, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        // table / column names are compile-time constants from callers.
        let sql = format!("SELECT owner_user_id FROM {table} WHERE {id_column} = ?1");
        connection
            .query_row(&sql, [id], |row| row.get::<_, Option<String>>(0))
            .map_err(|e| {
                if e == rusqlite::Error::QueryReturnedNoRows {
                    OrchestratorError::NotFound(format!("{label} '{id}' not found"))
                } else {
                    OrchestratorError::Database(e)
                }
            })?
            .ok_or_else(|| {
                OrchestratorError::InvalidContract(format!("{label} '{id}' has NULL owner_user_id"))
            })
    }
}

fn add_owner_column(
    connection: &rusqlite::Connection,
    table: &str,
) -> Result<(), OrchestratorError> {
    // table is a fixed allow-list. Prefer a real FK; fall back if this SQLite
    // build rejects REFERENCES on ALTER for a populated table.
    let with_fk =
        format!("ALTER TABLE {table} ADD COLUMN owner_user_id TEXT REFERENCES users(user_id)");
    match connection.execute(&with_fk, []) {
        Ok(_) => return Ok(()),
        Err(e) if is_duplicate_column(&e) => return Ok(()),
        Err(_) => {}
    }
    let plain = format!("ALTER TABLE {table} ADD COLUMN owner_user_id TEXT");
    match connection.execute(&plain, []) {
        Ok(_) => Ok(()),
        Err(e) if is_duplicate_column(&e) => Ok(()),
        Err(e) => Err(OrchestratorError::Database(e)),
    }
}

fn is_duplicate_column(err: &rusqlite::Error) -> bool {
    err.to_string().to_lowercase().contains("duplicate column")
}

/// Result of [`SqliteWorkflowStore::reassign_catalog_owner`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogOwnerReassignment {
    pub kind: String,
    pub urn: String,
    pub previous_owner_user_id: String,
    pub owner_user_id: String,
    pub rows_updated: i32,
}

pub(crate) fn catalog_table_for_kind(
    kind: &str,
) -> Result<(&'static str, &'static str), OrchestratorError> {
    match kind.trim().to_ascii_lowercase().replace('-', "_").as_str() {
        "agent" | "agents" => Ok(("agents", "agent")),
        "playbook" | "playbooks" => Ok(("playbooks", "playbook")),
        "skill" | "skills" => Ok(("skills", "skill")),
        "policy" | "policies" => Ok(("policies", "policy")),
        "skill_package" | "skill_packages" => Ok(("skill_packages", "skill package")),
        other => Err(OrchestratorError::InvalidContract(format!(
            "catalog kind must be agent, playbook, skill, policy, or skill_package; got '{other}'"
        ))),
    }
}

pub(crate) fn resolved_owner(owner_user_id: &str) -> &str {
    if owner_user_id.is_empty() {
        LOCAL_USER_ID
    } else {
        owner_user_id
    }
}

/// Deny when `urn` already exists in `table` under a different owner.
/// First writer (no rows) is allowed. Used by draft upserts that already
/// hold the SQLite lock.
pub(crate) fn require_matching_urn_owner(
    connection: &Connection,
    table: &str,
    urn: &str,
    caller: &str,
    noun: &str,
) -> Result<(), OrchestratorError> {
    if !is_safe_ident(table) {
        return Err(OrchestratorError::InvalidContract(format!(
            "unsafe table name '{table}'"
        )));
    }
    let existing = connection
        .query_row(
            &format!(
                "SELECT owner_user_id FROM {table} WHERE urn = ?1
                 ORDER BY CASE channel WHEN 'released' THEN 0 WHEN 'draft' THEN 1 ELSE 2 END,
                          updated_at DESC
                 LIMIT 1"
            ),
            [urn],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()
        .map_err(OrchestratorError::Database)?
        .flatten()
        .filter(|s| !s.is_empty());
    if let Some(owner) = existing {
        if owner != caller {
            return Err(OrchestratorError::PermissionDenied(format!(
                "only the artifact owner may version this {noun}"
            )));
        }
    }
    Ok(())
}

fn is_safe_ident(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn has_single_column_unique(
    connection: &Connection,
    table: &str,
    column: &str,
) -> Result<bool, OrchestratorError> {
    Ok(!unique_single_column_indexes(connection, table, column)?.is_empty())
}

fn unique_single_column_indexes(
    connection: &Connection,
    table: &str,
    column: &str,
) -> Result<Vec<String>, OrchestratorError> {
    let mut stmt = connection
        .prepare(&format!("PRAGMA index_list({table})"))
        .map_err(OrchestratorError::Database)?;
    let listed = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(1)?, row.get::<_, i64>(2)? != 0))
        })
        .map_err(OrchestratorError::Database)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(OrchestratorError::Database)?;
    drop(stmt);

    let mut matches = Vec::new();
    for (name, unique) in listed {
        if !unique || !is_safe_ident(&name) {
            continue;
        }
        let cols = index_columns(connection, &name)?;
        if cols.len() == 1 && cols[0] == column {
            matches.push(name);
        }
    }
    Ok(matches)
}

fn index_columns(
    connection: &Connection,
    index_name: &str,
) -> Result<Vec<String>, OrchestratorError> {
    let mut stmt = connection
        .prepare(&format!("PRAGMA index_info({index_name})"))
        .map_err(OrchestratorError::Database)?;
    let cols = stmt
        .query_map([], |row| row.get::<_, String>(2))
        .map_err(OrchestratorError::Database)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(OrchestratorError::Database)?;
    Ok(cols)
}

pub(crate) fn drop_global_idempotency_unique(
    connection: &Connection,
    table: &str,
) -> Result<(), OrchestratorError> {
    if !is_safe_ident(table) {
        return Err(OrchestratorError::InvalidContract(format!(
            "unsafe table name '{table}'"
        )));
    }
    for name in unique_single_column_indexes(connection, table, "idempotency_key")? {
        let _ = connection.execute(&format!("DROP INDEX IF EXISTS \"{name}\""), []);
    }
    if has_single_column_unique(connection, table, "idempotency_key")? {
        rebuild_table_stripping_idempotency_unique(connection, table)?;
    }
    Ok(())
}

fn rebuild_table_stripping_idempotency_unique(
    connection: &Connection,
    table: &str,
) -> Result<(), OrchestratorError> {
    let sql: String = connection
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [table],
            |row| row.get(0),
        )
        .map_err(OrchestratorError::Database)?;
    let tmp = format!("{table}__idemp_new");
    if !is_safe_ident(&tmp) {
        return Err(OrchestratorError::InvalidContract(format!(
            "unsafe rebuild table name '{tmp}'"
        )));
    }
    let rewritten = sql
        .replace("idempotency_key TEXT UNIQUE", "idempotency_key TEXT")
        .replacen(
            &format!("IF NOT EXISTS {table}"),
            &format!("IF NOT EXISTS {tmp}"),
            1,
        )
        .replacen(&format!("TABLE {table}"), &format!("TABLE {tmp}"), 1);
    if rewritten.contains("idempotency_key TEXT UNIQUE") {
        return Err(OrchestratorError::InvalidContract(format!(
            "could not strip global idempotency unique from {table}"
        )));
    }
    if rewritten == sql {
        return Err(OrchestratorError::InvalidContract(format!(
            "could not rename {table} for idempotency rebuild"
        )));
    }

    connection
        .execute("PRAGMA foreign_keys = OFF", [])
        .map_err(OrchestratorError::Database)?;
    let rebuilt = (|| -> Result<(), rusqlite::Error> {
        connection.execute_batch(&rewritten)?;
        connection.execute(&format!("INSERT INTO {tmp} SELECT * FROM {table}"), [])?;
        connection.execute(&format!("DROP TABLE {table}"), [])?;
        connection.execute(&format!("ALTER TABLE {tmp} RENAME TO {table}"), [])?;
        Ok(())
    })();
    connection
        .execute("PRAGMA foreign_keys = ON", [])
        .map_err(OrchestratorError::Database)?;
    rebuilt.map_err(OrchestratorError::Database)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::{mint_api_key_secret, BOOTSTRAP_SCOPES};
    use rusqlite::Connection;
    use std::path::PathBuf;
    use vam_spec::v1::ResourceId;

    fn temp_db_path(label: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!(
            "vam-owner-{}-{}-{}.db",
            label,
            std::process::id(),
            nanos
        ))
    }

    fn cleanup(path: &PathBuf) {
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(format!("{}-wal", path.display()));
        let _ = std::fs::remove_file(format!("{}-shm", path.display()));
    }

    fn scopes() -> Vec<String> {
        BOOTSTRAP_SCOPES.iter().map(|s| s.to_string()).collect()
    }

    fn run_id(name: &str) -> ResourceId {
        ResourceId {
            urn: format!("urn:vam:run:local.vam.dev/acme/{name}"),
        }
    }

    #[test]
    fn owner_user_id_mapped_from_calling_key() {
        assert_eq!(
            owner_user_id_for_key(BOOTSTRAP_KEY_ID, BOOTSTRAP_PRINCIPAL),
            OPERATOR_USER_ID
        );
        assert_eq!(
            owner_user_id_for_key(BOOTSTRAP_KEY_ID, "User::\"other\""),
            OPERATOR_USER_ID
        );
        // The principal never promotes a key: it is caller-influenced.
        assert_eq!(
            owner_user_id_for_key("key-1", BOOTSTRAP_PRINCIPAL),
            LOCAL_USER_ID
        );
        assert_eq!(
            owner_user_id_for_key("test-key", "User::\"tester\""),
            LOCAL_USER_ID
        );
        assert_eq!(
            owner_user_id_for_key("key-1", "User::\"alice\""),
            LOCAL_USER_ID
        );
    }

    #[test]
    fn authenticated_key_uses_stored_user_id() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        let secret = mint_api_key_secret();
        let record = store
            .create_api_key_owned(
                "key-local",
                "local-key",
                &secret,
                "User::\"alice\"",
                &scopes(),
                LOCAL_USER_ID,
            )
            .expect("owned key");
        assert_eq!(owner_user_id_for_authenticated_key(&record), LOCAL_USER_ID);
        let auth = store.authenticate_bearer(&secret).expect("auth");
        assert_eq!(
            owner_user_id_for_authenticated_key(&auth.record),
            LOCAL_USER_ID
        );
    }

    fn key_record(key_id: &str, principal: &str, owner: &str) -> ApiKeyRecord {
        ApiKeyRecord {
            key_id: key_id.to_string(),
            name: "k".to_string(),
            sha256_secret: "hash".to_string(),
            principal: principal.to_string(),
            owner_user_id: owner.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn principal_string_never_grants_operator() {
        // A member's key or session whose principal is the bootstrap principal keeps
        // its own owner.
        assert_eq!(
            owner_user_id_for_authenticated_key(&key_record("key-1", BOOTSTRAP_PRINCIPAL, "alice")),
            "alice"
        );
        assert_eq!(
            owner_user_id_for_authenticated_key(&key_record(
                "sess_0123",
                BOOTSTRAP_PRINCIPAL,
                "bootstrap"
            )),
            "bootstrap"
        );
        // No stored owner falls back to local, never to the operator.
        assert_eq!(
            owner_user_id_for_authenticated_key(&key_record("key-1", BOOTSTRAP_PRINCIPAL, "")),
            LOCAL_USER_ID
        );
        assert!(!is_bootstrap_key("key-1"));
        assert!(!is_bootstrap_key("Bootstrap"));
    }

    #[test]
    fn genuine_bootstrap_key_is_the_operator() {
        for owner in ["", LOCAL_USER_ID, OPERATOR_USER_ID] {
            assert_eq!(
                owner_user_id_for_authenticated_key(&key_record(
                    BOOTSTRAP_KEY_ID,
                    BOOTSTRAP_PRINCIPAL,
                    owner
                )),
                OPERATOR_USER_ID,
                "owner {owner:?}"
            );
        }
        assert!(is_bootstrap_key(BOOTSTRAP_KEY_ID));
    }

    #[test]
    fn bootstrap_names_are_reserved_ignoring_case() {
        for reserved in [
            "bootstrap",
            "Bootstrap",
            "BOOTSTRAP",
            " bootstrap ",
            BOOTSTRAP_PRINCIPAL,
            "user::\"BOOTSTRAP\"",
        ] {
            assert!(is_reserved_bootstrap_name(reserved), "{reserved:?}");
            assert!(
                matches!(
                    reject_reserved_bootstrap_name("key name", reserved),
                    Err(OrchestratorError::InvalidContract(_))
                ),
                "{reserved:?}"
            );
        }
        for fine in ["", "bootstrap2", "my-bootstrap", "User::\"alice\"", "alice"] {
            assert!(!is_reserved_bootstrap_name(fine), "{fine:?}");
            reject_reserved_bootstrap_name("key name", fine).expect("allowed");
        }
    }

    #[test]
    fn backfill_never_promotes_a_member_key_by_principal() {
        let path = temp_db_path("bootstrap-principal");
        {
            let store = SqliteWorkflowStore::open(&path).expect("store");
            store.create_user("alice", "alice").expect("alice");
            store
                .create_api_key_owned(
                    "key-member",
                    "member",
                    &mint_api_key_secret(),
                    BOOTSTRAP_PRINCIPAL,
                    &scopes(),
                    "alice",
                )
                .expect("member key with the bootstrap principal");
            store
                .create_api_key(
                    BOOTSTRAP_KEY_ID,
                    "bootstrap",
                    &mint_api_key_secret(),
                    BOOTSTRAP_PRINCIPAL,
                    &scopes(),
                )
                .expect("genuine bootstrap key");
        }
        // A restart runs the backfill again.
        let store = SqliteWorkflowStore::open(&path).expect("reopen");
        store.initialize_owner_stamps().expect("backfill");
        let owner_of = |key_id: &str| {
            store
                .list_api_keys()
                .expect("keys")
                .into_iter()
                .find(|k| k.key_id == key_id)
                .map(|k| k.owner_user_id)
                .expect("key")
        };
        assert_eq!(owner_of("key-member"), "alice");
        assert_eq!(owner_of(BOOTSTRAP_KEY_ID), OPERATOR_USER_ID);
        cleanup(&path);
    }

    #[test]
    fn owner_stamped_on_new_session_run_approval_key() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        let scopes = scopes();
        let secret = mint_api_key_secret();

        let bootstrap = store
            .create_api_key(
                BOOTSTRAP_KEY_ID,
                "bootstrap",
                &secret,
                BOOTSTRAP_PRINCIPAL,
                &scopes,
            )
            .expect("bootstrap key");
        assert_eq!(bootstrap.owner_user_id, OPERATOR_USER_ID);

        let other = store
            .create_api_key(
                "other-key",
                "other",
                &mint_api_key_secret(),
                "User::\"tester\"",
                &scopes,
            )
            .expect("other key");
        assert_eq!(other.owner_user_id, LOCAL_USER_ID);

        store
            .create_session_with_owner("sess-op", None, "vm-op", "ws", "default", OPERATOR_USER_ID)
            .expect("session operator");
        store
            .create_session_with_owner("sess-loc", None, "vm-loc", "ws", "default", LOCAL_USER_ID)
            .expect("session local");
        assert_eq!(
            store.session_owner_user_id("sess-op").expect("op owner"),
            OPERATOR_USER_ID
        );
        assert_eq!(
            store.session_owner_user_id("sess-loc").expect("loc owner"),
            LOCAL_USER_ID
        );

        store
            .persist_run_spec_with_owner(
                &run_id("run-op").urn,
                "{}",
                Some("sess-op"),
                "urn:vam:agent:op",
                Some("idem-op"),
                BOOTSTRAP_PRINCIPAL,
                OPERATOR_USER_ID,
            )
            .expect("run op");
        store
            .persist_run_spec(
                &run_id("run-loc").urn,
                "{}",
                Some("sess-loc"),
                "urn:vam:agent:loc",
                Some("idem-loc"),
                "User::\"tester\"",
            )
            .expect("run loc");
        assert_eq!(
            store
                .run_owner_user_id(&run_id("run-op").urn)
                .expect("run op owner"),
            OPERATOR_USER_ID
        );
        assert_eq!(
            store
                .run_owner_user_id(&run_id("run-loc").urn)
                .expect("run loc owner"),
            LOCAL_USER_ID
        );

        // Approval inherits the parent run's owner.
        let connection = store.connection.lock().expect("lock");
        connection
            .execute(
                "INSERT INTO approvals (
                    request_id, run_id, step_id, attempt_id, checkpoint_digest,
                    continuation_generation, published, session_id, owner_user_id,
                    approver_user_id
                 ) VALUES (?1, ?2, 'step', 'att', 'digest', 0, 1, 'sess-op',
                    (SELECT owner_user_id FROM runs WHERE run_id = ?2),
                    (SELECT owner_user_id FROM runs WHERE run_id = ?2))",
                params!["appr-op", run_id("run-op").urn],
            )
            .expect("approval op");
        connection
            .execute(
                "INSERT INTO approvals (
                    request_id, run_id, step_id, attempt_id, checkpoint_digest,
                    continuation_generation, published, session_id, owner_user_id,
                    approver_user_id
                 ) VALUES (?1, ?2, 'step', 'att', 'digest', 0, 1, 'sess-loc',
                    (SELECT owner_user_id FROM runs WHERE run_id = ?2),
                    (SELECT owner_user_id FROM runs WHERE run_id = ?2))",
                params!["appr-loc", run_id("run-loc").urn],
            )
            .expect("approval loc");
        drop(connection);

        assert_eq!(
            store
                .approval_owner_user_id("appr-op")
                .expect("approval op owner"),
            OPERATOR_USER_ID
        );
        assert_eq!(
            store
                .approval_owner_user_id("appr-loc")
                .expect("approval loc owner"),
            LOCAL_USER_ID
        );
        assert_eq!(
            store
                .get_approval("appr-op")
                .expect("appr op")
                .approver_user_id,
            OPERATOR_USER_ID
        );
        assert_eq!(
            store
                .get_approval("appr-loc")
                .expect("appr loc")
                .approver_user_id,
            LOCAL_USER_ID
        );
        assert_eq!(store.count_null_owners().expect("nulls"), 0);
    }

    #[test]
    fn owner_backfill_leaves_no_nulls() {
        let path = temp_db_path("backfill");
        {
            let conn = Connection::open(&path).expect("legacy open");
            conn.execute_batch(
                "
                PRAGMA foreign_keys = ON;
                CREATE TABLE sessions (
                    session_id TEXT PRIMARY KEY NOT NULL,
                    idempotency_key TEXT UNIQUE,
                    desired_state TEXT NOT NULL,
                    observed_state TEXT NOT NULL,
                    vm_id TEXT NOT NULL UNIQUE,
                    vmm_pid INTEGER,
                    vmm_start_time INTEGER,
                    guest_cid INTEGER,
                    tap_device TEXT,
                    cgroup_path TEXT,
                    workspace_id TEXT NOT NULL,
                    storage_profile_id TEXT NOT NULL,
                    active_manifest_hash TEXT,
                    spiffe_id TEXT,
                    lifecycle_generation INTEGER DEFAULT 1 NOT NULL,
                    created_at INTEGER NOT NULL,
                    updated_at INTEGER NOT NULL
                );
                CREATE TABLE runs (
                    run_id TEXT PRIMARY KEY NOT NULL,
                    state TEXT NOT NULL,
                    revision INTEGER NOT NULL
                );
                CREATE TABLE approvals (
                    request_id TEXT PRIMARY KEY NOT NULL,
                    run_id TEXT NOT NULL,
                    step_id TEXT NOT NULL,
                    attempt_id TEXT NOT NULL,
                    checkpoint_digest TEXT NOT NULL,
                    continuation_generation INTEGER NOT NULL
                );
                CREATE TABLE api_keys (
                    key_id TEXT PRIMARY KEY NOT NULL,
                    name TEXT NOT NULL,
                    sha256_secret TEXT NOT NULL UNIQUE,
                    principal TEXT NOT NULL,
                    scopes TEXT NOT NULL,
                    created_at INTEGER NOT NULL,
                    last_used_at INTEGER,
                    revoked_at INTEGER
                );
                INSERT INTO sessions (
                    session_id, desired_state, observed_state, vm_id,
                    workspace_id, storage_profile_id, created_at, updated_at
                ) VALUES ('sess-old', 'CREATED', 'CREATING', 'vm-old', 'ws', 'default', 1, 1);
                INSERT INTO runs (run_id, state, revision) VALUES ('run-old', 'RUNNING', 0);
                INSERT INTO approvals (
                    request_id, run_id, step_id, attempt_id,
                    checkpoint_digest, continuation_generation
                ) VALUES ('appr-old', 'run-old', 's', 'a', 'd', 0);
                INSERT INTO api_keys (key_id, name, sha256_secret, principal, scopes, created_at)
                VALUES ('bootstrap', 'bootstrap', 'hash-boot', 'User::\"bootstrap\"', '[]', 1);
                INSERT INTO api_keys (key_id, name, sha256_secret, principal, scopes, created_at)
                VALUES ('legacy-key', 'legacy', 'hash-legacy', 'User::\"tester\"', '[]', 2);
                CREATE TABLE host_secrets (
                    binding_id TEXT PRIMARY KEY NOT NULL,
                    kind TEXT NOT NULL,
                    nonce BLOB NOT NULL,
                    ciphertext BLOB NOT NULL,
                    fingerprint TEXT NOT NULL,
                    allowed_destinations_json TEXT NOT NULL DEFAULT '[]',
                    created_at INTEGER NOT NULL,
                    updated_at INTEGER NOT NULL,
                    last_used_at INTEGER,
                    idempotency_key TEXT UNIQUE
                );
                INSERT INTO host_secrets (
                    binding_id, kind, nonce, ciphertext, fingerprint,
                    allowed_destinations_json, created_at, updated_at
                ) VALUES ('openai/api-key', 'inference', x'00', x'00', 'deadbeef', '[]', 1, 1);
                CREATE TABLE agents (
                    urn TEXT PRIMARY KEY NOT NULL,
                    revision INTEGER NOT NULL,
                    digest TEXT NOT NULL,
                    spec_bytes BLOB NOT NULL,
                    created_at INTEGER NOT NULL
                );
                INSERT INTO agents VALUES ('urn:vam:agent:old', 1, 'abc', x'00', 1);
                CREATE TABLE playbooks (
                    urn TEXT PRIMARY KEY NOT NULL,
                    revision INTEGER NOT NULL,
                    digest TEXT NOT NULL,
                    spec_bytes BLOB NOT NULL,
                    created_at INTEGER NOT NULL
                );
                INSERT INTO playbooks VALUES ('urn:vam:playbook:old', 1, 'abc', x'00', 1);
                CREATE TABLE skills (
                    urn TEXT PRIMARY KEY NOT NULL,
                    revision INTEGER NOT NULL,
                    digest TEXT NOT NULL,
                    spec_bytes BLOB NOT NULL,
                    created_at INTEGER NOT NULL
                );
                INSERT INTO skills VALUES ('urn:vam:skill:old', 1, 'abc', x'00', 1);
                CREATE TABLE policies (
                    urn TEXT PRIMARY KEY NOT NULL,
                    revision INTEGER NOT NULL,
                    digest TEXT NOT NULL,
                    cedar TEXT NOT NULL,
                    created_at INTEGER NOT NULL,
                    updated_at INTEGER NOT NULL
                );
                INSERT INTO policies VALUES ('urn:vam:policy:old', 1, 'abc', 'permit(principal, action, resource);', 1, 1);
                CREATE TABLE skill_packages (
                    urn TEXT NOT NULL,
                    major INTEGER NOT NULL,
                    minor INTEGER NOT NULL,
                    patch INTEGER NOT NULL,
                    prerelease TEXT NOT NULL DEFAULT '',
                    channel TEXT NOT NULL CHECK(channel IN ('draft', 'released')),
                    revision INTEGER NOT NULL,
                    digest TEXT NOT NULL,
                    blob_relpath TEXT NOT NULL,
                    size_bytes INTEGER NOT NULL,
                    spec_bytes BLOB NOT NULL,
                    created_at INTEGER NOT NULL,
                    updated_at INTEGER NOT NULL,
                    PRIMARY KEY (urn, major, minor, patch, prerelease)
                );
                INSERT INTO skill_packages (
                    urn, major, minor, patch, prerelease, channel, revision, digest,
                    blob_relpath, size_bytes, spec_bytes, created_at, updated_at
                ) VALUES ('urn:vam:skill:old-pkg', 1, 0, 0, 'draft', 'draft', 1, 'abc', 'sha256-abc.tar.gz', 1, x'00', 1, 1);
                ",
            )
            .expect("legacy rows");
        }

        let store = SqliteWorkflowStore::open(&path).expect("migrate open");
        assert_eq!(store.count_null_owners().expect("nulls"), 0);
        assert_eq!(
            store.session_owner_user_id("sess-old").expect("session"),
            LOCAL_USER_ID
        );
        assert_eq!(
            store.run_owner_user_id("run-old").expect("run"),
            LOCAL_USER_ID
        );
        assert_eq!(
            store.approval_owner_user_id("appr-old").expect("approval"),
            LOCAL_USER_ID
        );

        let keys = store.list_api_keys().expect("keys");
        let bootstrap = keys
            .iter()
            .find(|k| k.key_id == BOOTSTRAP_KEY_ID)
            .expect("bootstrap");
        let legacy = keys
            .iter()
            .find(|k| k.key_id == "legacy-key")
            .expect("legacy");
        assert_eq!(bootstrap.owner_user_id, OPERATOR_USER_ID);
        assert_eq!(legacy.owner_user_id, LOCAL_USER_ID);
        assert_eq!(
            store
                .host_secret_owner_user_id(
                    crate::secrets::SecretScope::Org,
                    crate::secrets::ORG_OWNER_USER_ID,
                    "openai/api-key",
                )
                .expect("secret"),
            crate::secrets::ORG_OWNER_USER_ID
        );
        assert_eq!(
            store
                .agent_owner_user_id("urn:vam:agent:old")
                .expect("agent"),
            LOCAL_USER_ID
        );
        assert_eq!(
            store
                .playbook_owner_user_id("urn:vam:playbook:old")
                .expect("playbook"),
            LOCAL_USER_ID
        );
        assert_eq!(
            store
                .skill_owner_user_id("urn:vam:skill:old")
                .expect("skill"),
            LOCAL_USER_ID
        );
        assert_eq!(
            store
                .policy_owner_user_id("urn:vam:policy:old")
                .expect("policy"),
            LOCAL_USER_ID
        );
        assert_eq!(
            store
                .skill_package_owner_user_id("urn:vam:skill:old-pkg", 1, 0, 0, "draft")
                .expect("package"),
            LOCAL_USER_ID
        );
        assert_eq!(
            store.count_unnamespaced_skill_blobs().expect("blob paths"),
            0
        );
        {
            let connection = store.connection.lock().expect("lock");
            let relpath: String = connection
                .query_row(
                    "SELECT blob_relpath FROM skill_packages WHERE urn = 'urn:vam:skill:old-pkg'",
                    [],
                    |row| row.get(0),
                )
                .expect("blob_relpath");
            assert_eq!(
                relpath,
                crate::skill_packages::skill_blob_relpath(LOCAL_USER_ID, "abc")
            );
        }

        // Re-open is idempotent and still has no NULLs.
        let store2 = SqliteWorkflowStore::open(&path).expect("reopen");
        assert_eq!(store2.count_null_owners().expect("nulls"), 0);

        cleanup(&path);
    }

    #[test]
    fn owner_lists_remain_global() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        store
            .create_session_with_owner("sess-op", None, "vm-op", "ws", "default", OPERATOR_USER_ID)
            .expect("sess op");
        store
            .create_session_with_owner("sess-loc", None, "vm-loc", "ws", "default", LOCAL_USER_ID)
            .expect("sess loc");
        let sessions = store.load_all_sessions().expect("list sessions");
        let ids: Vec<_> = sessions.iter().map(|s| s.session_id.as_str()).collect();
        assert!(ids.contains(&"sess-op"));
        assert!(ids.contains(&"sess-loc"));

        store
            .persist_run_spec_with_owner(
                &run_id("r-op").urn,
                "{}",
                None,
                "agent",
                Some("k-op"),
                BOOTSTRAP_PRINCIPAL,
                OPERATOR_USER_ID,
            )
            .expect("run op");
        store
            .persist_run_spec(
                &run_id("r-loc").urn,
                "{}",
                None,
                "agent",
                Some("k-loc"),
                "User::\"tester\"",
            )
            .expect("run loc");
        let runs = store.list_runs().expect("list runs");
        let run_ids: Vec<_> = runs.iter().map(|(id, _)| id.as_str()).collect();
        assert!(run_ids.contains(&run_id("r-op").urn.as_str()));
        assert!(run_ids.contains(&run_id("r-loc").urn.as_str()));

        let connection = store.connection.lock().expect("lock");
        connection
            .execute(
                "INSERT INTO approvals (
                    request_id, run_id, step_id, attempt_id, checkpoint_digest,
                    continuation_generation, published, session_id, owner_user_id,
                    approver_user_id
                 ) VALUES ('appr-op', ?1, 's', 'a', 'd', 0, 1, 'sess-op', ?2, ?2)",
                params![run_id("r-op").urn, OPERATOR_USER_ID],
            )
            .expect("appr op");
        connection
            .execute(
                "INSERT INTO approvals (
                    request_id, run_id, step_id, attempt_id, checkpoint_digest,
                    continuation_generation, published, session_id, owner_user_id,
                    approver_user_id
                 ) VALUES ('appr-loc', ?1, 's', 'a', 'd', 0, 1, 'sess-loc', ?2, ?2)",
                params![run_id("r-loc").urn, LOCAL_USER_ID],
            )
            .expect("appr loc");
        drop(connection);

        let approvals = store.list_published_approvals().expect("list approvals");
        let appr_ids: Vec<_> = approvals.iter().map(|a| a.request_id.as_str()).collect();
        assert!(appr_ids.contains(&"appr-op"));
        assert!(appr_ids.contains(&"appr-loc"));

        store
            .create_api_key(
                BOOTSTRAP_KEY_ID,
                "bootstrap",
                &mint_api_key_secret(),
                BOOTSTRAP_PRINCIPAL,
                &scopes(),
            )
            .expect("bootstrap");
        store
            .create_api_key(
                "other-key",
                "other",
                &mint_api_key_secret(),
                "User::\"tester\"",
                &scopes(),
            )
            .expect("other");
        let keys = store.list_api_keys().expect("list keys");
        assert!(keys.iter().any(|k| k.key_id == BOOTSTRAP_KEY_ID));
        assert!(keys.iter().any(|k| k.key_id == "other-key"));
    }

    #[test]
    fn owner_filters_sessions_and_runs() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        store
            .create_session_with_owner("sess-op", None, "vm-op", "ws", "default", OPERATOR_USER_ID)
            .expect("sess op");
        store
            .create_session_with_owner("sess-loc", None, "vm-loc", "ws", "default", LOCAL_USER_ID)
            .expect("sess loc");
        let local_sessions = store
            .load_sessions_for_owner(LOCAL_USER_ID)
            .expect("local sessions");
        let local_ids: Vec<_> = local_sessions
            .iter()
            .map(|s| s.session_id.as_str())
            .collect();
        assert_eq!(local_ids, vec!["sess-loc"]);
        let op_sessions = store
            .load_sessions_for_owner(OPERATOR_USER_ID)
            .expect("op sessions");
        let op_ids: Vec<_> = op_sessions.iter().map(|s| s.session_id.as_str()).collect();
        assert_eq!(op_ids, vec!["sess-op"]);

        store
            .persist_run_spec_with_owner(
                &run_id("r-op").urn,
                "{}",
                None,
                "agent",
                Some("k-op-filter"),
                BOOTSTRAP_PRINCIPAL,
                OPERATOR_USER_ID,
            )
            .expect("run op");
        store
            .persist_run_spec_with_owner(
                &run_id("r-loc").urn,
                "{}",
                None,
                "agent",
                Some("k-loc-filter"),
                "User::\"tester\"",
                LOCAL_USER_ID,
            )
            .expect("run loc");
        let local_runs = store
            .list_runs_for_owner(LOCAL_USER_ID)
            .expect("local runs");
        assert_eq!(local_runs.len(), 1);
        assert_eq!(local_runs[0].0, run_id("r-loc").urn);
        let op_runs = store
            .list_runs_for_owner(OPERATOR_USER_ID)
            .expect("op runs");
        assert_eq!(op_runs.len(), 1);
        assert_eq!(op_runs[0].0, run_id("r-op").urn);
    }

    #[test]
    fn owner_get_is_not_found_for_foreign_row() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        store
            .create_session_with_owner("sess-b", None, "vm-b", "ws", "default", "local")
            .expect("sess");
        let err = store
            .load_session_for_owner("sess-b", OPERATOR_USER_ID)
            .expect_err("foreign session");
        match err {
            OrchestratorError::NotFound(msg) => {
                assert_eq!(msg, "session 'sess-b' not found");
            }
            other => panic!("expected NotFound, got {other}"),
        }
        let missing = store
            .load_session_for_owner("no-such", OPERATOR_USER_ID)
            .expect_err("missing session");
        match missing {
            OrchestratorError::NotFound(msg) => {
                assert_eq!(msg, "session 'no-such' not found");
            }
            other => panic!("expected NotFound, got {other}"),
        }

        store
            .persist_run_spec_with_owner(
                &run_id("r-b").urn,
                "{}",
                None,
                "agent",
                Some("k-foreign"),
                "User::\"tester\"",
                LOCAL_USER_ID,
            )
            .expect("run");
        let foreign = store
            .require_run_owner(&run_id("r-b").urn, OPERATOR_USER_ID)
            .expect_err("foreign run");
        match foreign {
            OrchestratorError::NotFound(msg) => {
                assert_eq!(msg, format!("run '{}' not found", run_id("r-b").urn));
            }
            other => panic!("expected NotFound, got {other}"),
        }
        store
            .require_run_owner(&run_id("r-b").urn, LOCAL_USER_ID)
            .expect("owner can read");
    }

    #[test]
    fn owner_filters_approvals_and_keys() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        store
            .persist_run_spec_with_owner(
                &run_id("r-op").urn,
                "{}",
                None,
                "agent",
                Some("k-appr-op"),
                BOOTSTRAP_PRINCIPAL,
                OPERATOR_USER_ID,
            )
            .expect("run op");
        store
            .persist_run_spec_with_owner(
                &run_id("r-loc").urn,
                "{}",
                None,
                "agent",
                Some("k-appr-loc"),
                "User::\"tester\"",
                LOCAL_USER_ID,
            )
            .expect("run loc");
        let connection = store.connection.lock().expect("lock");
        connection
            .execute(
                "INSERT INTO approvals (
                    request_id, run_id, step_id, attempt_id, checkpoint_digest,
                    continuation_generation, published, session_id, owner_user_id,
                    approver_user_id, requested_args_json
                 ) VALUES ('appr-op', ?1, 's', 'a', 'd', 0, 1, 'sess-op', ?2, ?2,
                    '{\"secret\":\"op\"}')",
                params![run_id("r-op").urn, OPERATOR_USER_ID],
            )
            .expect("appr op");
        connection
            .execute(
                "INSERT INTO approvals (
                    request_id, run_id, step_id, attempt_id, checkpoint_digest,
                    continuation_generation, published, session_id, owner_user_id,
                    approver_user_id, requested_args_json
                 ) VALUES ('appr-loc', ?1, 's', 'a', 'd', 0, 1, 'sess-loc', ?2, ?2,
                    '{\"secret\":\"loc\"}')",
                params![run_id("r-loc").urn, LOCAL_USER_ID],
            )
            .expect("appr loc");
        drop(connection);

        let local = store
            .list_published_approvals_for_owner(LOCAL_USER_ID)
            .expect("local approvals");
        assert_eq!(local.len(), 1);
        assert_eq!(local[0].request_id, "appr-loc");
        assert_eq!(local[0].approver_user_id, LOCAL_USER_ID);
        let op = store
            .list_published_approvals_for_owner(OPERATOR_USER_ID)
            .expect("op approvals");
        assert_eq!(op.len(), 1);
        assert_eq!(op[0].request_id, "appr-op");

        let foreign = store
            .get_approval_for_owner("appr-op", LOCAL_USER_ID)
            .expect_err("foreign approval");
        match foreign {
            OrchestratorError::ApprovalNotFound(id) => assert_eq!(id, "appr-op"),
            other => panic!("expected ApprovalNotFound, got {other}"),
        }
        store
            .require_approval_owner("appr-loc", LOCAL_USER_ID)
            .expect("owner can read");
        let foreign_req = store
            .require_approval_owner("appr-op", LOCAL_USER_ID)
            .expect_err("foreign require");
        match foreign_req {
            OrchestratorError::ApprovalNotFound(id) => assert_eq!(id, "appr-op"),
            other => panic!("expected ApprovalNotFound, got {other}"),
        }

        store
            .create_api_key(
                BOOTSTRAP_KEY_ID,
                "bootstrap",
                &mint_api_key_secret(),
                BOOTSTRAP_PRINCIPAL,
                &scopes(),
            )
            .expect("bootstrap");
        store
            .create_api_key_owned(
                "alice-key",
                "alice",
                &mint_api_key_secret(),
                "User::\"alice\"",
                &scopes(),
                LOCAL_USER_ID,
            )
            .expect("alice");
        let local_keys = store
            .list_api_keys_for_owner(LOCAL_USER_ID)
            .expect("local keys");
        assert!(local_keys.iter().any(|k| k.key_id == "alice-key"));
        assert!(!local_keys.iter().any(|k| k.key_id == BOOTSTRAP_KEY_ID));
        let op_keys = store
            .list_api_keys_for_owner(OPERATOR_USER_ID)
            .expect("op keys");
        assert!(op_keys.iter().any(|k| k.key_id == BOOTSTRAP_KEY_ID));
        assert!(!op_keys.iter().any(|k| k.key_id == "alice-key"));
        let foreign_key = store
            .get_api_key_for_owner(BOOTSTRAP_KEY_ID, LOCAL_USER_ID)
            .expect_err("foreign key");
        match foreign_key {
            OrchestratorError::NotFound(msg) => {
                assert!(msg.contains(BOOTSTRAP_KEY_ID), "{msg}");
            }
            other => panic!("expected NotFound, got {other}"),
        }
    }

    #[test]
    fn idempotency_keys_are_unique_per_user() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        store.create_user("alice", "alice").expect("alice user");
        store.create_user("bob", "bob").expect("bob user");

        assert!(store
            .create_session_with_owner("sess-a", Some("k"), "vm-a", "ws-a", "default", "alice")
            .expect("sess a")
            .is_none());
        assert!(store
            .create_session_with_owner("sess-b", Some("k"), "vm-b", "ws-b", "default", "bob")
            .expect("sess b")
            .is_none());

        let alice_replay = store
            .create_session_with_owner("sess-a2", Some("k"), "vm-a2", "ws-a", "default", "alice")
            .expect("alice replay")
            .expect("existing");
        assert_eq!(alice_replay.session_id, "sess-a");
        let bob_sess = store
            .find_session_by_idempotency_key("bob", "k")
            .expect("bob key")
            .expect("bob row");
        assert_eq!(bob_sess.session_id, "sess-b");
        let alice_sess = store
            .find_session_by_idempotency_key("alice", "k")
            .expect("alice key")
            .expect("alice row");
        assert_eq!(alice_sess.session_id, "sess-a");
        assert!(store
            .find_session_by_idempotency_key("alice", "missing")
            .expect("miss")
            .is_none());

        store
            .persist_run_spec_with_owner(
                &run_id("run-a").urn,
                "{}",
                Some("sess-a"),
                "agent-a",
                Some("k"),
                "User::\"alice\"",
                "alice",
            )
            .expect("run a");
        store
            .persist_run_spec_with_owner(
                &run_id("run-b").urn,
                "{}",
                Some("sess-b"),
                "agent-b",
                Some("k"),
                "User::\"bob\"",
                "bob",
            )
            .expect("run b");
        assert_eq!(
            store
                .find_run_by_idempotency_key("alice", "k")
                .expect("alice run"),
            Some(run_id("run-a").urn)
        );
        assert_eq!(
            store
                .find_run_by_idempotency_key("bob", "k")
                .expect("bob run"),
            Some(run_id("run-b").urn)
        );
        assert_eq!(
            store
                .find_run_by_idempotency_key("alice", "k")
                .expect("alice replay run"),
            Some(run_id("run-a").urn)
        );
        assert!(store
            .find_run_by_idempotency_key("bob", "nope")
            .expect("bob miss")
            .is_none());
        assert!(!store
            .has_global_idempotency_unique("sessions")
            .expect("sess"));
        assert!(!store.has_global_idempotency_unique("runs").expect("runs"));
        assert_eq!(
            store
                .count_null_owners_on_idempotency_keys()
                .expect("nulls"),
            0
        );
    }

    #[test]
    fn idempotency_replay_is_scoped_to_caller() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        store.create_user("alice", "alice").expect("alice user");
        store.create_user("bob", "bob").expect("bob user");
        store
            .create_session_with_owner("sess-a", Some("shared"), "vm-a", "ws", "default", "alice")
            .expect("sess");
        store
            .persist_run_spec_with_owner(
                &run_id("run-a").urn,
                "{}",
                Some("sess-a"),
                "agent",
                Some("shared"),
                "User::\"alice\"",
                "alice",
            )
            .expect("run");

        assert!(store
            .find_session_by_idempotency_key("bob", "shared")
            .expect("bob sess")
            .is_none());
        assert!(store
            .find_run_by_idempotency_key("bob", "shared")
            .expect("bob run")
            .is_none());
        let replay = store
            .create_session_with_owner(
                "sess-other",
                Some("shared"),
                "vm-x",
                "ws",
                "default",
                "alice",
            )
            .expect("replay")
            .expect("alice row");
        assert_eq!(replay.session_id, "sess-a");
    }

    #[test]
    fn idempotency_legacy_global_unique_migrates() {
        let path = temp_db_path("idemp-migrate");
        {
            let conn = Connection::open(&path).expect("legacy open");
            conn.execute_batch(
                "
                PRAGMA foreign_keys = ON;
                CREATE TABLE sessions (
                    session_id TEXT PRIMARY KEY NOT NULL,
                    idempotency_key TEXT UNIQUE,
                    desired_state TEXT NOT NULL,
                    observed_state TEXT NOT NULL,
                    vm_id TEXT NOT NULL UNIQUE,
                    vmm_pid INTEGER,
                    vmm_start_time INTEGER,
                    guest_cid INTEGER,
                    tap_device TEXT,
                    cgroup_path TEXT,
                    workspace_id TEXT NOT NULL,
                    storage_profile_id TEXT NOT NULL,
                    active_manifest_hash TEXT,
                    spiffe_id TEXT,
                    lifecycle_generation INTEGER DEFAULT 1 NOT NULL,
                    created_at INTEGER NOT NULL,
                    updated_at INTEGER NOT NULL
                );
                CREATE TABLE runs (
                    run_id TEXT PRIMARY KEY NOT NULL,
                    state TEXT NOT NULL,
                    revision INTEGER NOT NULL,
                    spec_json TEXT,
                    session_id TEXT,
                    agent_urn TEXT,
                    idempotency_key TEXT UNIQUE,
                    owner_principal TEXT,
                    result_json TEXT
                );
                INSERT INTO sessions (
                    session_id, idempotency_key, desired_state, observed_state, vm_id,
                    workspace_id, storage_profile_id, created_at, updated_at
                ) VALUES ('sess-old', 'shared-k', 'CREATED', 'CREATING', 'vm-old', 'ws', 'default', 1, 1);
                INSERT INTO runs (run_id, state, revision, idempotency_key)
                VALUES ('run-old', 'RUNNING', 0, 'shared-k');
                CREATE TABLE host_secrets (
                    binding_id TEXT PRIMARY KEY NOT NULL,
                    kind TEXT NOT NULL,
                    nonce BLOB NOT NULL,
                    ciphertext BLOB NOT NULL,
                    fingerprint TEXT NOT NULL,
                    allowed_destinations_json TEXT NOT NULL DEFAULT '[]',
                    created_at INTEGER NOT NULL,
                    updated_at INTEGER NOT NULL,
                    last_used_at INTEGER,
                    idempotency_key TEXT UNIQUE
                );
                INSERT INTO host_secrets (
                    binding_id, kind, nonce, ciphertext, fingerprint,
                    allowed_destinations_json, created_at, updated_at, idempotency_key
                ) VALUES ('openai/api-key', 'inference', x'00', x'00', 'deadbeef', '[]', 1, 1, 'sec-k');
                ",
            )
            .expect("legacy rows");
        }

        let store = SqliteWorkflowStore::open(&path).expect("migrate open");
        assert_eq!(store.count_null_owners().expect("null owners"), 0);
        assert_eq!(
            store
                .count_null_owners_on_idempotency_keys()
                .expect("idemp nulls"),
            0
        );
        assert!(!store
            .has_global_idempotency_unique("sessions")
            .expect("sess"));
        assert!(!store.has_global_idempotency_unique("runs").expect("runs"));
        assert!(!store
            .has_global_idempotency_unique("host_secrets")
            .expect("secrets scoped"));

        store.create_user("alice", "alice").expect("alice");
        assert!(store
            .create_session_with_owner(
                "sess-alice",
                Some("shared-k"),
                "vm-alice",
                "ws-a",
                "default",
                "alice",
            )
            .expect("alice same key")
            .is_none());
        store
            .persist_run_spec_with_owner(
                &run_id("run-alice").urn,
                "{}",
                Some("sess-alice"),
                "agent",
                Some("shared-k"),
                "User::\"alice\"",
                "alice",
            )
            .expect("alice run same key");

        assert_eq!(
            store
                .find_session_by_idempotency_key(LOCAL_USER_ID, "shared-k")
                .expect("legacy sess")
                .expect("row")
                .session_id,
            "sess-old"
        );
        assert_eq!(
            store
                .find_session_by_idempotency_key("alice", "shared-k")
                .expect("alice sess")
                .expect("row")
                .session_id,
            "sess-alice"
        );
        assert_eq!(
            store
                .find_run_by_idempotency_key(LOCAL_USER_ID, "shared-k")
                .expect("legacy run"),
            Some("run-old".to_string())
        );
        assert_eq!(
            store
                .find_run_by_idempotency_key("alice", "shared-k")
                .expect("alice run"),
            Some(run_id("run-alice").urn)
        );
        cleanup(&path);
    }

    #[test]
    fn idempotency_host_secrets_are_scoped_to_vault_key() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        store.create_user("alice", "alice").expect("alice");
        store.create_user("bob", "bob").expect("bob");
        assert!(!store
            .has_global_idempotency_unique("host_secrets")
            .expect("not global unique"));
        store
            .put_host_secret(
                "openai/api-key",
                crate::secrets::SecretKind::Inference,
                &[0u8; 12],
                &[1u8; 16],
                "fp-a",
                &[],
                Some("vault-k"),
                crate::secrets::SecretScope::User,
                "alice",
            )
            .expect("secret a");
        let replay = store
            .find_host_secret_by_idempotency(crate::secrets::SecretScope::User, "alice", "vault-k")
            .expect("lookup")
            .expect("row");
        assert_eq!(replay.binding_id, "openai/api-key");
        store
            .put_host_secret(
                "openai/api-key",
                crate::secrets::SecretKind::Inference,
                &[0u8; 12],
                &[2u8; 16],
                "fp-b",
                &[],
                Some("vault-k"),
                crate::secrets::SecretScope::User,
                "bob",
            )
            .expect("bob same idempotency");
        let bob = store
            .find_host_secret_by_idempotency(crate::secrets::SecretScope::User, "bob", "vault-k")
            .expect("bob lookup")
            .expect("bob row");
        assert_eq!(bob.fingerprint, "fp-b");
        let alice = store
            .get_host_secret_metadata(crate::secrets::SecretScope::User, "alice", "openai/api-key")
            .expect("alice unchanged");
        assert_eq!(alice.fingerprint, "fp-a");
    }

    #[test]
    fn owner_list_queries_use_owner_user_id_index() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        store
            .create_session_with_owner("sess-ix", None, "vm-ix", "ws", "default", LOCAL_USER_ID)
            .expect("sess");
        store
            .persist_run_spec_with_owner(
                &run_id("r-ix").urn,
                "{}",
                None,
                "agent",
                Some("k-ix"),
                "User::\"tester\"",
                LOCAL_USER_ID,
            )
            .expect("run");
        let connection = store.connection.lock().expect("lock");
        let session_plan = explain_plan(
            &connection,
            "SELECT session_id FROM sessions WHERE owner_user_id = 'local'",
        );
        assert!(
            session_plan.contains("idx_sessions_owner_user_id"),
            "{session_plan}"
        );
        let run_plan = explain_plan(
            &connection,
            "SELECT run_id FROM runs WHERE owner_user_id = 'local'",
        );
        assert!(run_plan.contains("idx_runs_owner_user_id"), "{run_plan}");
        let approval_plan = explain_plan(
            &connection,
            "SELECT request_id FROM approvals WHERE owner_user_id = 'local'",
        );
        assert!(
            approval_plan.contains("idx_approvals_owner_user_id"),
            "{approval_plan}"
        );
        let key_plan = explain_plan(
            &connection,
            "SELECT key_id FROM api_keys WHERE owner_user_id = 'local'",
        );
        assert!(
            key_plan.contains("idx_api_keys_owner_user_id"),
            "{key_plan}"
        );
        let session_idemp_plan = explain_plan(
            &connection,
            "SELECT session_id FROM sessions WHERE owner_user_id = 'local' AND idempotency_key = 'k'",
        );
        assert!(
            session_idemp_plan.contains("idx_sessions_owner_idempotency"),
            "{session_idemp_plan}"
        );
        let run_idemp_plan = explain_plan(
            &connection,
            "SELECT run_id FROM runs WHERE owner_user_id = 'local' AND idempotency_key = 'k'",
        );
        assert!(
            run_idemp_plan.contains("idx_runs_owner_idempotency"),
            "{run_idemp_plan}"
        );
    }

    fn explain_plan(connection: &Connection, sql: &str) -> String {
        let mut stmt = connection
            .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .expect("plan");
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(3))
            .expect("rows");
        let mut out = String::new();
        for row in rows {
            out.push_str(&row.expect("row"));
            out.push('\n');
        }
        out
    }
}
