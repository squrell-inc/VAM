use crate::error::OrchestratorError;
use crate::workflow::SqliteWorkflowStore;
use rusqlite::OptionalExtension;
use rusqlite::{params, Connection};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DesiredState {
    Created,
    Running,
    Paused,
    Terminated,
}

impl DesiredState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Created => "CREATED",
            Self::Running => "RUNNING",
            Self::Paused => "PAUSED",
            Self::Terminated => "TERMINATED",
        }
    }

    pub fn parse(value: &str) -> Result<Self, OrchestratorError> {
        match value {
            "CREATED" => Ok(Self::Created),
            "RUNNING" => Ok(Self::Running),
            "PAUSED" => Ok(Self::Paused),
            "TERMINATED" => Ok(Self::Terminated),
            _ => Err(OrchestratorError::CorruptState(format!(
                "Invalid desired_state: {}",
                value
            ))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObservedState {
    Creating,
    Booting,
    Running,
    Paused,
    Terminating,
    Terminated,
    Failed,
}

impl ObservedState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Creating => "CREATING",
            Self::Booting => "BOOTING",
            Self::Running => "RUNNING",
            Self::Paused => "PAUSED",
            Self::Terminating => "TERMINATING",
            Self::Terminated => "TERMINATED",
            Self::Failed => "FAILED",
        }
    }

    pub fn parse(value: &str) -> Result<Self, OrchestratorError> {
        match value {
            "CREATING" => Ok(Self::Creating),
            "BOOTING" => Ok(Self::Booting),
            "RUNNING" => Ok(Self::Running),
            "PAUSED" => Ok(Self::Paused),
            "TERMINATING" => Ok(Self::Terminating),
            "TERMINATED" => Ok(Self::Terminated),
            "FAILED" => Ok(Self::Failed),
            _ => Err(OrchestratorError::CorruptState(format!(
                "Invalid observed_state: {}",
                value
            ))),
        }
    }

    /// Maps ObservedState onto physical `vam_core::VmState`.
    pub fn to_vm_state(self) -> vam_core::VmState {
        match self {
            Self::Creating => vam_core::VmState::Configured,
            Self::Booting => vam_core::VmState::Booting,
            Self::Running => vam_core::VmState::Running,
            Self::Paused => vam_core::VmState::Paused,
            Self::Terminating => vam_core::VmState::Halted,
            Self::Terminated => vam_core::VmState::Reaped,
            Self::Failed => vam_core::VmState::Halted,
        }
    }

    pub fn can_transition_to(self, target: ObservedState) -> bool {
        match (self, target) {
            // Setup
            (Self::Creating, Self::Booting) => true,
            (Self::Booting, Self::Running) => true,

            // Pausability
            (Self::Running, Self::Paused) => true,
            (Self::Paused, Self::Running) => true,

            // Teardown / Failures can happen from almost any non-terminal state
            (Self::Creating, Self::Failed)
            | (Self::Creating, Self::Terminating)
            | (Self::Creating, Self::Terminated) => true,
            (Self::Booting, Self::Failed)
            | (Self::Booting, Self::Terminating)
            | (Self::Booting, Self::Terminated) => true,
            (Self::Running, Self::Failed)
            | (Self::Running, Self::Terminating)
            | (Self::Running, Self::Terminated) => true,
            (Self::Paused, Self::Failed)
            | (Self::Paused, Self::Terminating)
            | (Self::Paused, Self::Terminated) => true,

            // Finalizing termination
            (Self::Terminating, Self::Terminated) | (Self::Terminating, Self::Failed) => true,
            (Self::Failed, Self::Terminating) | (Self::Failed, Self::Terminated) => true,

            // Self-transition is always idempotent
            (a, b) if a == b => true,
            _ => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRecord {
    pub session_id: String,
    pub idempotency_key: Option<String>,
    pub desired_state: DesiredState,
    pub observed_state: ObservedState,
    pub vm_id: String,
    pub vmm_pid: Option<u32>,
    pub vmm_start_time: Option<u64>,
    pub guest_cid: Option<u32>,
    pub tap_device: Option<String>,
    pub cgroup_path: Option<String>,
    pub workspace_id: String,
    pub storage_profile_id: String,
    pub active_manifest_hash: Option<String>,
    pub spiffe_id: Option<String>,
    pub lifecycle_generation: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

impl SqliteWorkflowStore {
    pub fn create_session(
        &self,
        session_id: &str,
        idempotency_key: Option<&str>,
        vm_id: &str,
        workspace_id: &str,
        storage_profile_id: &str,
    ) -> Result<Option<SessionRecord>, OrchestratorError> {
        self.create_session_with_owner(
            session_id,
            idempotency_key,
            vm_id,
            workspace_id,
            storage_profile_id,
            crate::users::LOCAL_USER_ID,
        )
    }

    pub fn create_session_with_owner(
        &self,
        session_id: &str,
        idempotency_key: Option<&str>,
        vm_id: &str,
        workspace_id: &str,
        storage_profile_id: &str,
        owner_user_id: &str,
    ) -> Result<Option<SessionRecord>, OrchestratorError> {
        let owner = crate::owner::resolved_owner(owner_user_id);
        // create_run pre-inserts the owner-stamped row, then session boot
        // calls create_session again with the implicit local owner. Adopt the
        // existing row by session_id so vm_id stays unique.
        match self.load_session(session_id) {
            Ok(existing) => return Ok(Some(existing)),
            Err(OrchestratorError::NotFound(_)) => {}
            Err(e) => return Err(e),
        }
        if let Some(key) = idempotency_key {
            if let Some(existing) = self.find_session_by_idempotency_key(owner, key)? {
                return Ok(Some(existing));
            }
        }
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let transaction = connection.unchecked_transaction()?;

        let now = unix_timestamp();

        transaction.execute(
            "INSERT INTO sessions (
                session_id, idempotency_key, desired_state, observed_state,
                vm_id, workspace_id, storage_profile_id, lifecycle_generation,
                created_at, updated_at, owner_user_id
            ) VALUES (?1, ?2, 'CREATED', 'CREATING', ?3, ?4, ?5, 1, ?6, ?6, ?7)",
            params![
                session_id,
                idempotency_key,
                vm_id,
                workspace_id,
                storage_profile_id,
                now,
                owner,
            ],
        )?;

        crate::workflow::append_outbox(
            &transaction,
            session_id,
            "vam.v1.session.created",
            "CREATING",
        )?;

        transaction.commit()?;
        Ok(None)
    }

    /// Lookup is `(owner_user_id, idempotency_key)` — keys are unique per user.
    pub fn find_session_by_idempotency_key(
        &self,
        owner_user_id: &str,
        idempotency_key: &str,
    ) -> Result<Option<SessionRecord>, OrchestratorError> {
        let owner = crate::owner::resolved_owner(owner_user_id);
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        connection
            .query_row(
                "SELECT 
                    session_id, idempotency_key, desired_state, observed_state,
                    vm_id, vmm_pid, vmm_start_time, guest_cid, tap_device, cgroup_path,
                    workspace_id, storage_profile_id, active_manifest_hash, spiffe_id,
                    lifecycle_generation, created_at, updated_at
                 FROM sessions WHERE owner_user_id = ?1 AND idempotency_key = ?2",
                params![owner, idempotency_key],
                session_record_from_row,
            )
            .optional()
            .map_err(OrchestratorError::Database)
    }

    pub fn load_all_sessions(&self) -> Result<Vec<SessionRecord>, OrchestratorError> {
        self.load_sessions(None)
    }

    /// List sessions owned by `owner_user_id` (`idx_sessions_owner_user_id`).
    pub fn load_sessions_for_owner(
        &self,
        owner_user_id: &str,
    ) -> Result<Vec<SessionRecord>, OrchestratorError> {
        self.load_sessions(Some(crate::owner::resolved_owner(owner_user_id)))
    }

    fn load_sessions(
        &self,
        owner_user_id: Option<&str>,
    ) -> Result<Vec<SessionRecord>, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let sql = if owner_user_id.is_some() {
            format!("{SESSION_SELECT} WHERE owner_user_id = ?1")
        } else {
            SESSION_SELECT.to_string()
        };
        let mut stmt = connection.prepare(&sql)?;
        let mut result = Vec::new();
        match owner_user_id {
            Some(owner) => {
                let rows = stmt.query_map([owner], session_record_from_row)?;
                for r in rows {
                    result.push(r?);
                }
            }
            None => {
                let rows = stmt.query_map([], session_record_from_row)?;
                for r in rows {
                    result.push(r?);
                }
            }
        }
        Ok(result)
    }

    /// One page of `owner_user_id`'s sessions, newest first (`created_at`, then `session_id`, both
    /// descending): up to `limit` rows after the cursor `(created_at, session_id)` of the previous
    /// page's last row.
    pub fn load_sessions_page_for_owner(
        &self,
        owner_user_id: &str,
        limit: usize,
        after: Option<(i64, String)>,
    ) -> Result<Vec<SessionRecord>, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let (created, id, first) = match after {
            Some((c, i)) => (c, i, false),
            None => (i64::MAX, String::new(), true),
        };
        let sql = format!(
            "{SESSION_SELECT}
             WHERE owner_user_id = ?1
               AND (?4 OR created_at < ?2 OR (created_at = ?2 AND session_id < ?3))
             ORDER BY created_at DESC, session_id DESC LIMIT ?5"
        );
        let mut stmt = connection.prepare(&sql)?;
        let rows = stmt.query_map(
            params![
                crate::owner::resolved_owner(owner_user_id),
                created,
                id,
                first,
                limit as i64
            ],
            session_record_from_row,
        )?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn load_session(&self, session_id: &str) -> Result<SessionRecord, OrchestratorError> {
        self.load_session_filtered(session_id, None)
    }

    /// Get a session owned by `owner_user_id`. Missing or foreign → NotFound
    /// (same message; no existence leak).
    pub fn load_session_for_owner(
        &self,
        session_id: &str,
        owner_user_id: &str,
    ) -> Result<SessionRecord, OrchestratorError> {
        self.load_session_filtered(
            session_id,
            Some(crate::owner::resolved_owner(owner_user_id)),
        )
    }

    fn load_session_filtered(
        &self,
        session_id: &str,
        owner_user_id: Option<&str>,
    ) -> Result<SessionRecord, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let missing = || OrchestratorError::NotFound(format!("session '{}' not found", session_id));
        let result = if let Some(owner) = owner_user_id {
            connection.query_row(
                &format!("{SESSION_SELECT} WHERE session_id = ?1 AND owner_user_id = ?2"),
                params![session_id, owner],
                session_record_from_row,
            )
        } else {
            connection.query_row(
                &format!("{SESSION_SELECT} WHERE session_id = ?1"),
                [session_id],
                session_record_from_row,
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

    pub fn update_observed_state(
        &self,
        session_id: &str,
        target_state: ObservedState,
    ) -> Result<(), OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let transaction = connection.unchecked_transaction()?;

        let current_state_str: String = transaction.query_row(
            "SELECT observed_state FROM sessions WHERE session_id = ?1",
            [session_id],
            |row| row.get(0),
        )?;
        let current_state = ObservedState::parse(&current_state_str)?;

        if !current_state.can_transition_to(target_state) {
            return Err(OrchestratorError::InvalidState {
                expected: "valid state transition",
                actual: format!(
                    "Illegal transition from {:?} to {:?}",
                    current_state, target_state
                ),
            });
        }

        let now = unix_timestamp();
        transaction.execute(
            "UPDATE sessions SET observed_state = ?1, updated_at = ?2 WHERE session_id = ?3",
            params![target_state.as_str(), now, session_id],
        )?;

        crate::workflow::append_outbox(
            &transaction,
            session_id,
            "vam.v1.session.state-changed",
            target_state.as_str(),
        )?;

        transaction.commit()?;
        Ok(())
    }

    pub fn update_desired_state(
        &self,
        session_id: &str,
        target_state: DesiredState,
    ) -> Result<(), OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let transaction = connection.unchecked_transaction()?;

        let now = unix_timestamp();
        transaction.execute(
            "UPDATE sessions SET desired_state = ?1, updated_at = ?2 WHERE session_id = ?3",
            params![target_state.as_str(), now, session_id],
        )?;

        crate::workflow::append_outbox(
            &transaction,
            session_id,
            "vam.v1.session.desired-state-changed",
            target_state.as_str(),
        )?;

        transaction.commit()?;
        Ok(())
    }

    pub fn record_vmm(
        &self,
        session_id: &str,
        vmm_pid: u32,
        vmm_start_time: u64,
        guest_cid: u32,
    ) -> Result<(), OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let transaction = connection.unchecked_transaction()?;

        let now = unix_timestamp();
        transaction.execute(
            "UPDATE sessions SET vmm_pid = ?1, vmm_start_time = ?2, guest_cid = ?3, updated_at = ?4 WHERE session_id = ?5",
            params![
                vmm_pid as i64,
                vmm_start_time as i64,
                guest_cid as i64,
                now,
                session_id
            ],
        )?;

        crate::workflow::append_outbox(
            &transaction,
            session_id,
            "vam.v1.session.vmm-recorded",
            &format!("pid:{},cid:{}", vmm_pid, guest_cid),
        )?;

        transaction.commit()?;
        Ok(())
    }

    pub fn record_tap_device(
        &self,
        session_id: &str,
        tap_device: &str,
    ) -> Result<(), OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let now = unix_timestamp();
        connection.execute(
            "UPDATE sessions SET tap_device = ?1, updated_at = ?2 WHERE session_id = ?3",
            params![tap_device, now, session_id],
        )?;
        Ok(())
    }

    pub fn bump_lifecycle_generation(&self, session_id: &str) -> Result<i64, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let transaction = connection.unchecked_transaction()?;

        let current_gen: i64 = transaction.query_row(
            "SELECT lifecycle_generation FROM sessions WHERE session_id = ?1",
            [session_id],
            |row| row.get(0),
        )?;
        let next_gen = current_gen + 1;
        let now = unix_timestamp();

        transaction.execute(
            "UPDATE sessions SET lifecycle_generation = ?1, updated_at = ?2 WHERE session_id = ?3",
            params![next_gen, now, session_id],
        )?;

        crate::workflow::append_outbox(
            &transaction,
            session_id,
            "vam.v1.session.generation-bumped",
            &next_gen.to_string(),
        )?;

        transaction.commit()?;
        Ok(next_gen)
    }

    /// Set the session's lifecycle generation to `generation`.
    ///
    /// A cold HITL resume boots a new session and stamps the lease generation
    /// here so the row is higher than the parked session it replaced.
    pub fn assign_lifecycle_generation(
        &self,
        session_id: &str,
        generation: i64,
    ) -> Result<i64, OrchestratorError> {
        if generation <= 0 {
            return Err(OrchestratorError::InvalidContract(
                "lifecycle generation must be positive".into(),
            ));
        }
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let transaction = connection.unchecked_transaction()?;
        let current: i64 = transaction
            .query_row(
                "SELECT lifecycle_generation FROM sessions WHERE session_id = ?1",
                [session_id],
                |row| row.get(0),
            )
            .map_err(|error| {
                if error == rusqlite::Error::QueryReturnedNoRows {
                    OrchestratorError::NotFound(format!("session '{session_id}' not found"))
                } else {
                    OrchestratorError::Database(error)
                }
            })?;
        if current == generation {
            return Ok(generation);
        }
        let now = unix_timestamp();
        transaction.execute(
            "UPDATE sessions SET lifecycle_generation = ?1, updated_at = ?2 WHERE session_id = ?3",
            params![generation, now, session_id],
        )?;
        crate::workflow::append_outbox(
            &transaction,
            session_id,
            "vam.v1.session.generation-bumped",
            &generation.to_string(),
        )?;
        transaction.commit()?;
        Ok(generation)
    }

    pub fn update_active_manifest_hash(
        &self,
        session_id: &str,
        hash: &str,
    ) -> Result<(), OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let now = unix_timestamp();
        connection.execute(
            "UPDATE sessions SET active_manifest_hash = ?1, updated_at = ?2 WHERE session_id = ?3",
            params![hash, now, session_id],
        )?;
        Ok(())
    }

    pub fn record_snapshot(
        &self,
        meta: &crate::manager::SnapshotMetadata,
    ) -> Result<(), OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let kind_str = match meta.kind {
            crate::manager::SnapshotKind::Cold => "Cold",
            crate::manager::SnapshotKind::Warm => "Warm",
        };
        let meta_json =
            serde_json::to_string(meta).map_err(|e| OrchestratorError::Core(e.to_string()))?;
        connection.execute(
            "INSERT INTO snapshots (snapshot_id, session_id, workspace_id, parent_snapshot_id, kind, drive9_manifest_hash, guest_cid, tap_name, netns, fc_version, metadata_json, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                meta.snapshot_id,
                meta.session_id,
                meta.workspace_id,
                meta.parent_snapshot_id,
                kind_str,
                meta.drive9_manifest_hash,
                meta.guest_cid.map(|c| c as i64),
                meta.tap_name,
                meta.netns,
                meta.fc_version,
                meta_json,
                meta.created_at,
            ],
        )?;
        Ok(())
    }

    /// Returns the most recent snapshot for a workspace (host-local fast-path restore).
    pub fn find_latest_snapshot_for_workspace(
        &self,
        workspace_id: &str,
    ) -> Result<Option<crate::manager::SnapshotMetadata>, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let mut stmt = connection.prepare(
            "SELECT metadata_json FROM snapshots
             WHERE workspace_id = ?1
             ORDER BY created_at DESC
             LIMIT 1",
        )?;
        let mut rows = stmt.query(params![workspace_id])?;
        if let Some(row) = rows.next()? {
            let meta_json: String = row.get(0)?;
            let meta: crate::manager::SnapshotMetadata = serde_json::from_str(&meta_json)
                .map_err(|e| OrchestratorError::Core(e.to_string()))?;
            return Ok(Some(meta));
        }
        Ok(None)
    }

    pub fn record_tool_catalog(
        &self,
        session_id: &str,
        tools: &[ToolCatalogEntry],
    ) -> Result<(), OrchestratorError> {
        let mut seen = std::collections::HashSet::new();
        for tool in tools {
            if !seen.insert(tool.tool_name.as_str()) {
                return Err(OrchestratorError::InvalidContract(format!(
                    "tool '{}' is declared more than once in this session",
                    tool.tool_name
                )));
            }
        }
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let transaction = connection.unchecked_transaction()?;
        for tool in tools {
            transaction.execute(
                "INSERT OR REPLACE INTO session_tool_catalog (
                    session_id, tool_name, command, entrypoint, digest, side_effect
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    session_id,
                    tool.tool_name,
                    tool.command,
                    tool.entrypoint,
                    tool.digest,
                    tool.side_effect,
                ],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn get_tool_catalog(
        &self,
        session_id: &str,
    ) -> Result<Vec<ToolCatalogEntry>, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let mut stmt = connection.prepare(
            "SELECT tool_name, command, entrypoint, digest, side_effect
             FROM session_tool_catalog WHERE session_id = ?1",
        )?;
        let rows = stmt.query_map(params![session_id], |row| {
            Ok(ToolCatalogEntry {
                tool_name: row.get(0)?,
                command: row.get(1)?,
                entrypoint: row.get(2)?,
                digest: row.get(3)?,
                side_effect: row.get(4)?,
                // Session rows do not store source rules yet (AX-5.3b).
                skill_name: String::new(),
                approval_required: false,
            })
        })?;
        let mut result = Vec::new();
        for r in rows {
            result.push(r?);
        }
        Ok(result)
    }

    pub fn get_tool(
        &self,
        session_id: &str,
        tool_name: &str,
    ) -> Result<Option<ToolCatalogEntry>, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let mut stmt = connection.prepare(
            "SELECT tool_name, command, entrypoint, digest, side_effect
             FROM session_tool_catalog WHERE session_id = ?1 AND tool_name = ?2",
        )?;
        let mut rows = stmt.query_map(params![session_id, tool_name], |row| {
            Ok(ToolCatalogEntry {
                tool_name: row.get(0)?,
                command: row.get(1)?,
                entrypoint: row.get(2)?,
                digest: row.get(3)?,
                side_effect: row.get(4)?,
                // Session rows do not store source rules yet (AX-5.3b).
                skill_name: String::new(),
                approval_required: false,
            })
        })?;
        if let Some(r) = rows.next() {
            Ok(Some(r?))
        } else {
            Ok(None)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ToolCatalogEntry {
    pub tool_name: String,
    pub command: String,
    pub entrypoint: String,
    pub digest: String,
    pub side_effect: i32,
    /// Manifest `name` of the skill that declared this tool. Empty for builtins
    /// and for rows loaded from `session_tool_catalog` (AX-5.3b stores them).
    pub skill_name: String,
    /// Source rule of `ask` for this tool only. A skill can set it only on
    /// tools that skill declares.
    pub approval_required: bool,
}

pub fn parse_skills_catalog(skills_dir: &std::path::Path) -> Vec<ToolCatalogEntry> {
    let mut catalog = Vec::new();
    if !skills_dir.is_dir() {
        return catalog;
    }
    if let Ok(entries) = std::fs::read_dir(skills_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let manifest_path = path.join("manifest.json");
                if manifest_path.is_file() {
                    if let Ok(content) = std::fs::read_to_string(&manifest_path) {
                        if let Ok(val) = serde_json::from_str::<serde_json::Value>(&content) {
                            // A stored manifest has no `name`; the skill's name is the
                            // last segment of its `resourceId.urn`.
                            let skill_name = val
                                .get("name")
                                .and_then(|v| v.as_str())
                                .or_else(|| {
                                    val.get("resourceId")
                                        .or_else(|| val.get("resource_id"))
                                        .and_then(|id| id.get("urn"))
                                        .and_then(|v| v.as_str())
                                        .and_then(|urn| urn.rsplit('/').next())
                                })
                                .unwrap_or("")
                                .to_string();
                            if let Some(tools) = val.get("tools").and_then(|v| v.as_array()) {
                                for tool in tools {
                                    if let Some(name) = tool.get("name").and_then(|v| v.as_str()) {
                                        if vam_spec::validation::reserved_builtin_tool_names()
                                            .contains(&name)
                                            || matches!(name, "write_note" | "list_workspace")
                                        {
                                            continue;
                                        }
                                        let side_effect = tool
                                            .get("sideEffect")
                                            .or_else(|| tool.get("side_effect"))
                                            .and_then(|v| {
                                                if let Some(n) = v.as_i64() {
                                                    Some(n as i32)
                                                } else if let Some(s) = v.as_str() {
                                                    match s {
                                                        "PURE"
                                                        | "SIDE_EFFECT_CLASS_READ_ONLY"
                                                        | "READ_ONLY" => Some(1),
                                                        "MUTABLE_LOCAL"
                                                        | "SIDE_EFFECT_CLASS_MUTABLE_LOCAL" => {
                                                            Some(2)
                                                        }
                                                        "MUTABLE_EXTERNAL"
                                                        | "SIDE_EFFECT_CLASS_MUTABLE_EXTERNAL" => {
                                                            Some(3)
                                                        }
                                                        _ => Some(0),
                                                    }
                                                } else {
                                                    None
                                                }
                                            })
                                            .unwrap_or(0);
                                        let approval_required = tool
                                            .get("approvalRequired")
                                            .or_else(|| tool.get("approval_required"))
                                            .and_then(|v| v.as_bool())
                                            .unwrap_or(false);

                                        if let Some(host_exec) =
                                            tool.get("hostExec").or_else(|| tool.get("host_exec"))
                                        {
                                            let adapter_id = host_exec
                                                .get("adapterId")
                                                .or_else(|| host_exec.get("adapter_id"))
                                                .and_then(|v| v.as_str())
                                                .unwrap_or("");
                                            if adapter_id.is_empty() {
                                                continue;
                                            }
                                            catalog.push(ToolCatalogEntry {
                                                tool_name: name.to_string(),
                                                command: "host".to_string(),
                                                entrypoint: adapter_id.to_string(),
                                                digest: vam_spec::validation::host_tool_digest(
                                                    adapter_id,
                                                ),
                                                side_effect,
                                                skill_name: skill_name.clone(),
                                                approval_required,
                                            });
                                            continue;
                                        }

                                        let local_exec = tool
                                            .get("localExec")
                                            .or_else(|| tool.get("local_exec"));
                                        let command = local_exec
                                            .and_then(|e| e.get("command"))
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("")
                                            .to_string();
                                        let entrypoint = local_exec
                                            .and_then(|e| e.get("entrypoint"))
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("")
                                            .to_string();

                                        let digest =
                                            match vam_spec::resolve_manifest_relative_entrypoint(
                                                &path,
                                                &entrypoint,
                                            ) {
                                                Ok(script_file_path) => {
                                                    if let Ok(bytes) =
                                                        std::fs::read(&script_file_path)
                                                    {
                                                        format!("{:x}", Sha256::digest(&bytes))
                                                    } else {
                                                        String::new()
                                                    }
                                                }
                                                Err(_) => String::new(),
                                            };

                                        catalog.push(ToolCatalogEntry {
                                            tool_name: name.to_string(),
                                            command,
                                            entrypoint,
                                            digest,
                                            side_effect,
                                            skill_name: skill_name.clone(),
                                            approval_required,
                                        });
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    catalog
}

pub fn builtin_tool_catalog() -> Vec<ToolCatalogEntry> {
    use vam_spec::validation::{builtin_tool_digest, reserved_builtin_tool_names};

    reserved_builtin_tool_names()
        .iter()
        .map(|name| {
            let side_effect = match *name {
                "read_file" | "glob" | "grep" => 1,
                _ => 2,
            };
            ToolCatalogEntry {
                tool_name: (*name).to_string(),
                command: "builtin".to_string(),
                entrypoint: (*name).to_string(),
                digest: builtin_tool_digest(name),
                side_effect,
                skill_name: String::new(),
                approval_required: false,
            }
        })
        .collect()
}

/// Merge runtime builtins with skill and hosted tools; fail-closed on name collision.
pub fn merge_tool_catalog(
    builtins: Vec<ToolCatalogEntry>,
    skills: Vec<ToolCatalogEntry>,
    hosted: Vec<ToolCatalogEntry>,
) -> Result<Vec<ToolCatalogEntry>, OrchestratorError> {
    let mut seen = std::collections::HashSet::new();
    let mut merged = Vec::with_capacity(builtins.len() + skills.len() + hosted.len());
    for entry in builtins {
        seen.insert(entry.tool_name.clone());
        merged.push(entry);
    }
    for entry in skills {
        if !seen.insert(entry.tool_name.clone()) {
            return Err(OrchestratorError::Core(format!(
                "Tool catalog name collision: '{}' is already declared in the session catalog",
                entry.tool_name
            )));
        }
        merged.push(entry);
    }
    for entry in hosted {
        if !seen.insert(entry.tool_name.clone()) {
            return Err(OrchestratorError::Core(format!(
                "Tool catalog name collision: '{}' is already declared in the session catalog",
                entry.tool_name
            )));
        }
        merged.push(entry);
    }
    Ok(merged)
}

/// Process-local cache of parsed skill tool catalogs keyed by skills digest.
#[derive(Debug, Default)]
pub struct ToolCatalogCache {
    skills_by_digest: Mutex<HashMap<String, Vec<ToolCatalogEntry>>>,
}

impl ToolCatalogCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns cached skill tools or parses and stores them on miss.
    pub fn skills_for_digest(
        &self,
        skills_digest: &str,
        skills_dir: &Path,
    ) -> Vec<ToolCatalogEntry> {
        let mut cache = match self.skills_by_digest.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(entries) = cache.get(skills_digest) {
            return entries.clone();
        }
        let parsed = parse_skills_catalog(skills_dir);
        cache.insert(skills_digest.to_string(), parsed.clone());
        parsed
    }
}

pub fn initialize_session_schema(connection: &Connection) -> Result<(), OrchestratorError> {
    connection.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS sessions (
            session_id TEXT PRIMARY KEY NOT NULL,
            idempotency_key TEXT,
            desired_state TEXT NOT NULL CHECK(desired_state IN ('CREATED', 'RUNNING', 'PAUSED', 'TERMINATED')),
            observed_state TEXT NOT NULL CHECK(observed_state IN ('CREATING', 'BOOTING', 'RUNNING', 'PAUSED', 'TERMINATING', 'TERMINATED', 'FAILED')),
            
            -- Process & System Identifiers
            vm_id TEXT NOT NULL UNIQUE,
            vmm_pid INTEGER,
            vmm_start_time INTEGER,  -- Unix ticks of process start to prevent PID reuse bugs
            guest_cid INTEGER,
            tap_device TEXT,
            cgroup_path TEXT,
            
            -- Storage & Identity References
            workspace_id TEXT NOT NULL,
            storage_profile_id TEXT NOT NULL,
            active_manifest_hash TEXT,
            spiffe_id TEXT,
            
            -- Metadata & Lifecycle
            lifecycle_generation INTEGER DEFAULT 1 NOT NULL,
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_sessions_observed ON sessions(observed_state);
        CREATE UNIQUE INDEX IF NOT EXISTS idx_sessions_active_cid ON sessions(guest_cid) WHERE observed_state NOT IN ('TERMINATED', 'FAILED');
        CREATE UNIQUE INDEX IF NOT EXISTS idx_sessions_active_pid ON sessions(vmm_pid) WHERE observed_state NOT IN ('TERMINATED', 'FAILED');
        CREATE UNIQUE INDEX IF NOT EXISTS idx_sessions_active_tap ON sessions(tap_device) WHERE observed_state NOT IN ('TERMINATED', 'FAILED');
        CREATE UNIQUE INDEX IF NOT EXISTS idx_sessions_active_cgroup ON sessions(cgroup_path) WHERE observed_state NOT IN ('TERMINATED', 'FAILED');

        CREATE TABLE IF NOT EXISTS snapshots (
            snapshot_id TEXT PRIMARY KEY NOT NULL,
            session_id TEXT NOT NULL,
            workspace_id TEXT NOT NULL,
            parent_snapshot_id TEXT,
            kind TEXT NOT NULL CHECK(kind IN ('Cold', 'Warm')),
            drive9_manifest_hash TEXT NOT NULL,
            guest_cid INTEGER,
            tap_name TEXT,
            netns TEXT,
            fc_version TEXT,
            metadata_json TEXT NOT NULL,
            created_at INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_snapshots_workspace ON snapshots(workspace_id);

        CREATE TABLE IF NOT EXISTS session_tool_catalog (
            session_id TEXT NOT NULL,
            tool_name TEXT NOT NULL,
            command TEXT NOT NULL,
            entrypoint TEXT NOT NULL,
            digest TEXT NOT NULL,
            side_effect INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY (session_id, tool_name),
            FOREIGN KEY (session_id) REFERENCES sessions(session_id)
        );
        CREATE INDEX IF NOT EXISTS idx_session_tool_catalog_sess ON session_tool_catalog(session_id);

        CREATE TABLE IF NOT EXISTS session_resource_usage (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            session_id TEXT NOT NULL,
            script_digest TEXT NOT NULL,
            tool_name TEXT NOT NULL,
            cpu_time_ms INTEGER NOT NULL,
            peak_rss_bytes INTEGER NOT NULL,
            wall_time_ms INTEGER NOT NULL,
            bytes_written INTEGER NOT NULL,
            exit_signal INTEGER NOT NULL,
            recorded_at INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_session_resource_digest ON session_resource_usage(script_digest);
        CREATE INDEX IF NOT EXISTS idx_session_resource_sess ON session_resource_usage(session_id);
        "
    )?;
    Ok(())
}

impl SqliteWorkflowStore {
    pub fn record_resource_usage(
        &self,
        session_id: &str,
        script_digest: &str,
        tool_name: &str,
        usage: &vam_spec::v1::ResourceUsage,
    ) -> Result<(), OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let now = unix_timestamp();
        connection.execute(
            "INSERT INTO session_resource_usage (
                session_id, script_digest, tool_name, cpu_time_ms,
                peak_rss_bytes, wall_time_ms, bytes_written, exit_signal, recorded_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                session_id,
                script_digest,
                tool_name,
                usage.cpu_time_ms as i64,
                usage.peak_rss_bytes as i64,
                usage.wall_time_ms as i64,
                usage.bytes_written as i64,
                usage.exit_signal as i64,
                now,
            ],
        )?;
        Ok(())
    }

    /// Computes per-digest baseline telemetry: (avg_cpu_time_ms, avg_peak_rss_bytes, avg_wall_time_ms, sample_count)
    pub fn get_digest_resource_baseline(
        &self,
        script_digest: &str,
    ) -> Result<Option<(u64, u64, u64, u64)>, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let mut stmt = connection.prepare(
            "SELECT AVG(cpu_time_ms), AVG(peak_rss_bytes), AVG(wall_time_ms), COUNT(*)
             FROM session_resource_usage
             WHERE script_digest = ?1 AND exit_signal = 0",
        )?;
        let row = stmt.query_row(params![script_digest], |r| {
            let avg_cpu: Option<f64> = r.get(0)?;
            let avg_rss: Option<f64> = r.get(1)?;
            let avg_wall: Option<f64> = r.get(2)?;
            let count: i64 = r.get(3)?;
            Ok((avg_cpu, avg_rss, avg_wall, count))
        })?;

        if row.3 > 0 {
            Ok(Some((
                row.0.unwrap_or(0.0) as u64,
                row.1.unwrap_or(0.0) as u64,
                row.2.unwrap_or(0.0) as u64,
                row.3 as u64,
            )))
        } else {
            Ok(None)
        }
    }
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)] // test module position left as is; moving it is a src/ layout change
mod tests {
    use super::*;

    #[test]
    fn test_parse_skills_catalog_host_exec_digest() {
        let temp_dir =
            std::env::temp_dir().join(format!("vam-host-skills-test-{}", std::process::id()));
        let skill_dir = temp_dir.join("host-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();

        let manifest = serde_json::json!({
            "tools": [
                {
                    "name": "host_echo",
                    "host_exec": { "adapter_id": "echo" },
                    "side_effect": "READ_ONLY"
                }
            ]
        });
        std::fs::write(
            skill_dir.join("manifest.json"),
            serde_json::to_string(&manifest).unwrap(),
        )
        .unwrap();

        let catalog = parse_skills_catalog(&temp_dir);
        assert_eq!(catalog.len(), 1);
        assert_eq!(catalog[0].tool_name, "host_echo");
        assert_eq!(catalog[0].command, "host");
        assert_eq!(catalog[0].entrypoint, "echo");
        assert_eq!(
            catalog[0].digest,
            vam_spec::validation::host_tool_digest("echo")
        );

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_parse_skills_catalog_computes_entrypoint_digest() {
        let temp_dir = std::env::temp_dir().join(format!("vam-skills-test-{}", std::process::id()));
        let skill_dir = temp_dir.join("test-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();

        let script_content = b"print('hello world')";
        std::fs::write(skill_dir.join("main.py"), script_content).unwrap();

        let manifest = serde_json::json!({
            "tools": [
                {
                    "name": "test_tool",
                    "local_exec": {
                        "command": "python",
                        "entrypoint": "main.py"
                    },
                    "side_effect": "READ_ONLY"
                }
            ]
        });
        std::fs::write(
            skill_dir.join("manifest.json"),
            serde_json::to_string(&manifest).unwrap(),
        )
        .unwrap();

        let catalog = parse_skills_catalog(&temp_dir);
        assert_eq!(catalog.len(), 1);
        assert_eq!(catalog[0].tool_name, "test_tool");
        assert_eq!(catalog[0].command, "python");
        assert_eq!(catalog[0].entrypoint, "main.py");
        assert_eq!(catalog[0].side_effect, 1);

        let expected_digest = format!("{:x}", Sha256::digest(script_content));
        assert_eq!(catalog[0].digest, expected_digest);

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_parse_skills_catalog_resolves_relocated_relative_entrypoint() {
        let temp_dir =
            std::env::temp_dir().join(format!("vam-skills-relocated-{}", std::process::id()));
        let skill_dir = temp_dir.join("acme--jira");
        std::fs::create_dir_all(&skill_dir).unwrap();
        let script = b"console.log('relocated')";
        std::fs::write(skill_dir.join("execute.js"), script).unwrap();
        let manifest = serde_json::json!({
            "tools": [{
                "name": "jira_search_issues",
                "localExec": {
                    "command": "bun",
                    "entrypoint": "./execute.js"
                }
            }]
        });
        std::fs::write(
            skill_dir.join("manifest.json"),
            serde_json::to_string(&manifest).unwrap(),
        )
        .unwrap();

        let catalog = parse_skills_catalog(&temp_dir);
        assert_eq!(catalog.len(), 1);
        assert_eq!(catalog[0].entrypoint, "./execute.js");
        assert_eq!(catalog[0].digest, format!("{:x}", Sha256::digest(script)));

        let baked = serde_json::json!({
            "tools": [{
                "name": "jira_search_issues",
                "localExec": {
                    "command": "bun",
                    "entrypoint": "/mnt/drive9/skills/jira-integration/execute.js"
                }
            }]
        });
        std::fs::write(
            skill_dir.join("manifest.json"),
            serde_json::to_string(&baked).unwrap(),
        )
        .unwrap();
        let baked_catalog = parse_skills_catalog(&temp_dir);
        assert_eq!(baked_catalog[0].digest, "");

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_builtin_tool_catalog_digest() {
        let catalog = builtin_tool_catalog();
        assert_eq!(catalog.len(), 9);
        assert!(catalog
            .iter()
            .any(|e| e.tool_name == "read_file" && e.command == "builtin"));
        assert!(catalog
            .iter()
            .any(|e| e.tool_name == "load_skill" && e.command == "builtin"));
        assert_eq!(
            catalog
                .iter()
                .find(|e| e.tool_name == "read_file")
                .unwrap()
                .digest,
            vam_spec::validation::builtin_tool_digest("read_file")
        );
    }

    #[test]
    fn test_merge_tool_catalog_rejects_builtin_collision() {
        let builtins = builtin_tool_catalog();
        let skills = vec![ToolCatalogEntry {
            tool_name: "read_file".to_string(),
            command: "bun".to_string(),
            entrypoint: "evil.js".to_string(),
            digest: "deadbeef".to_string(),
            side_effect: 1,
            skill_name: String::new(),
            approval_required: false,
        }];
        assert!(merge_tool_catalog(builtins, skills, vec![]).is_err());
    }

    #[test]
    fn test_merge_tool_catalog_accepts_disjoint_skills() {
        let builtins = builtin_tool_catalog();
        let skills = vec![ToolCatalogEntry {
            tool_name: "calculator".to_string(),
            command: "bun".to_string(),
            entrypoint: "execute.js".to_string(),
            digest: "abc".to_string(),
            side_effect: 1,
            skill_name: String::new(),
            approval_required: false,
        }];
        let merged = merge_tool_catalog(builtins, skills, vec![]).unwrap();
        assert_eq!(merged.len(), 10);
    }

    #[test]
    fn test_tool_catalog_cache_returns_same_digests() {
        let temp_dir =
            std::env::temp_dir().join(format!("vam-catalog-cache-{}", std::process::id()));
        let skill_dir = temp_dir.join("cache-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(skill_dir.join("main.py"), b"print('cached')").unwrap();
        let manifest = serde_json::json!({
            "tools": [{
                "name": "cached_tool",
                "local_exec": { "command": "python", "entrypoint": "main.py" },
                "side_effect": "READ_ONLY"
            }]
        });
        std::fs::write(
            skill_dir.join("manifest.json"),
            serde_json::to_string(&manifest).unwrap(),
        )
        .unwrap();

        let digest = vam_storage::skills_digest_from_dir(&temp_dir);
        let cache = ToolCatalogCache::new();

        let first = cache.skills_for_digest(&digest, &temp_dir);
        let second = cache.skills_for_digest(&digest, &temp_dir);
        assert_eq!(first, second);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].tool_name, "cached_tool");

        let direct = parse_skills_catalog(&temp_dir);
        assert_eq!(first[0].digest, direct[0].digest);

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn approval_required_applies_only_to_tools_the_manifest_declares() {
        let temp_dir =
            std::env::temp_dir().join(format!("vam-approval-skills-test-{}", std::process::id()));
        let jira = temp_dir.join("jira");
        let other = temp_dir.join("other");
        std::fs::create_dir_all(&jira).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(jira.join("execute.js"), b"export {}").unwrap();
        std::fs::write(other.join("execute.js"), b"export {}").unwrap();

        let jira_manifest = serde_json::json!({
            "name": "jira",
            "tools": [
                {
                    "name": "jira_create_issue",
                    "approvalRequired": true,
                    "localExec": { "command": "bun", "entrypoint": "./execute.js" },
                    "sideEffect": "MUTABLE_EXTERNAL"
                },
                {
                    "name": "jira_search_issues",
                    "approvalRequired": false,
                    "localExec": { "command": "bun", "entrypoint": "./execute.js" },
                    "sideEffect": "READ_ONLY"
                }
            ]
        });
        // A second skill cannot name jira_create_issue. Its own tool stays unset.
        let other_manifest = serde_json::json!({
            "name": "other",
            "tools": [
                {
                    "name": "list_notes",
                    "localExec": { "command": "bun", "entrypoint": "./execute.js" },
                    "sideEffect": "READ_ONLY"
                }
            ]
        });
        std::fs::write(
            jira.join("manifest.json"),
            serde_json::to_string(&jira_manifest).unwrap(),
        )
        .unwrap();
        std::fs::write(
            other.join("manifest.json"),
            serde_json::to_string(&other_manifest).unwrap(),
        )
        .unwrap();

        let mut catalog = parse_skills_catalog(&temp_dir);
        catalog.sort_by(|a, b| a.tool_name.cmp(&b.tool_name));
        assert_eq!(catalog.len(), 3);
        let create = catalog
            .iter()
            .find(|entry| entry.tool_name == "jira_create_issue")
            .expect("create");
        assert!(create.approval_required);
        assert_eq!(create.skill_name, "jira");
        let search = catalog
            .iter()
            .find(|entry| entry.tool_name == "jira_search_issues")
            .expect("search");
        assert!(!search.approval_required);
        assert_eq!(search.skill_name, "jira");
        let notes = catalog
            .iter()
            .find(|entry| entry.tool_name == "list_notes")
            .expect("notes");
        assert!(!notes.approval_required);
        assert_eq!(notes.skill_name, "other");

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn skill_name_falls_back_to_the_last_urn_segment_of_a_stored_manifest() {
        let dir = std::env::temp_dir().join(format!("vam-skill-urn-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let skill = dir.join("jira-integration");
        std::fs::create_dir_all(&skill).unwrap();
        // The shape `vam skill publish` stores: a resource id, no `name`.
        std::fs::write(
            skill.join("manifest.json"),
            r#"{"resourceId":{"urn":"urn:vam:skill:local.vam.dev/vam/jira-integration"},
                "tools":[{"name":"jira_search","sideEffect":"READ_ONLY",
                          "hostExec":{"adapterId":"adapter.jira"}}]}"#,
        )
        .unwrap();
        let catalog = parse_skills_catalog(&dir);
        assert_eq!(catalog.len(), 1);
        assert_eq!(catalog[0].skill_name, "jira-integration");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn create_session_adopts_existing_row_across_owners() {
        let store = crate::workflow::SqliteWorkflowStore::in_memory().expect("store");
        store
            .create_session_with_owner(
                "sess-run-1",
                Some("run-session-1"),
                "vm-sess-run-1",
                "operator--main",
                "default",
                "operator",
            )
            .expect("owner insert");
        let existing = store
            .create_session(
                "sess-run-1",
                Some("run-session-1"),
                "vm-sess-run-1",
                "operator--main",
                "default",
            )
            .expect("boot insert");
        assert_eq!(existing.expect("adopted row").session_id, "sess-run-1");
    }
}

const SESSION_SELECT: &str = "SELECT
                session_id, idempotency_key, desired_state, observed_state,
                vm_id, vmm_pid, vmm_start_time, guest_cid, tap_device, cgroup_path,
                workspace_id, storage_profile_id, active_manifest_hash, spiffe_id,
                lifecycle_generation, created_at, updated_at
             FROM sessions";

fn session_record_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SessionRecord> {
    let desired_state_str: String = row.get(2)?;
    let observed_state_str: String = row.get(3)?;
    Ok(SessionRecord {
        session_id: row.get(0)?,
        idempotency_key: row.get(1)?,
        desired_state: DesiredState::parse(&desired_state_str).unwrap_or(DesiredState::Created),
        observed_state: ObservedState::parse(&observed_state_str)
            .unwrap_or(ObservedState::Creating),
        vm_id: row.get(4)?,
        vmm_pid: row.get::<_, Option<i64>>(5)?.map(|v| v as u32),
        vmm_start_time: row.get::<_, Option<i64>>(6)?.map(|v| v as u64),
        guest_cid: row.get::<_, Option<i64>>(7)?.map(|v| v as u32),
        tap_device: row.get(8)?,
        cgroup_path: row.get(9)?,
        workspace_id: row.get(10)?,
        storage_profile_id: row.get(11)?,
        active_manifest_hash: row.get(12)?,
        spiffe_id: row.get(13)?,
        lifecycle_generation: row.get(14)?,
        created_at: row.get(15)?,
        updated_at: row.get(16)?,
    })
}

fn unix_timestamp() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(i64::MAX)
}
