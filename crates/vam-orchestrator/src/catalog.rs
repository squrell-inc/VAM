use crate::error::OrchestratorError;
use crate::workflow::SqliteWorkflowStore;
use prost::Message;
use rusqlite::{params, OptionalExtension};
use vam_spec::v1::{
    AgentRecord, AgentSpec, PlaybookSpec, PolicyRecord, ResourceId, ResourceMetadata, SkillChannel,
    SkillManifest,
};
use vam_spec::validation::Validate;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CatalogKind {
    Agent,
    Playbook,
    Skill,
}

impl CatalogKind {
    pub(crate) fn table(self) -> &'static str {
        match self {
            Self::Agent => "agents",
            Self::Playbook => "playbooks",
            Self::Skill => "skills",
        }
    }
}

/// SQLite `channel` constraint shared by versioned catalog artifact tables.
pub(crate) const CATALOG_CHANNEL_CHECK_SQL: &str =
    "CHECK(channel IN ('draft', 'released', 'yanked'))";

pub(crate) const CATALOG_RELEASED_MAJOR: i32 = 1;
pub(crate) const CATALOG_RELEASED_MINOR: i32 = 0;
pub(crate) const CATALOG_RELEASED_PATCH: i32 = 0;
pub(crate) const CATALOG_RELEASED_PRERELEASE: &str = "";
pub(crate) const CATALOG_DRAFT_PRERELEASE: &str = "draft";
pub(crate) const CATALOG_CHANNEL_RELEASED: &str = "released";
pub(crate) const CATALOG_CHANNEL_DRAFT: &str = "draft";

/// Fixed row key for a spec draft (`agents`, `playbooks`, `skills`, `policies`).
/// A draft has no version. These numbers are a storage slot only, never a
/// release version. Lookups find the draft by URN and channel, not by these.
pub(crate) const CATALOG_DRAFT_SLOT_MAJOR: i32 = 1;
pub(crate) const CATALOG_DRAFT_SLOT_MINOR: i32 = 0;
pub(crate) const CATALOG_DRAFT_SLOT_PATCH: i32 = 0;

/// Error for a draft that the caller cannot see or that does not exist.
/// Says nothing about versions.
pub(crate) fn no_draft_error(noun: &str, urn: &str) -> OrchestratorError {
    OrchestratorError::NotFound(format!("no draft of {noun} '{urn}' for this user"))
}

/// Draft rows are private to their owner. Released rows are tenant-visible.
/// `include_foreign_drafts` (operators) bypasses the owner check.
fn ensure_draft_visible(
    noun: &str,
    urn: &str,
    channel: &str,
    row_owner: &str,
    owner_user_id: &str,
    include_foreign_drafts: bool,
) -> Result<(), OrchestratorError> {
    if channel == CATALOG_CHANNEL_DRAFT && !include_foreign_drafts {
        let caller = crate::owner::resolved_owner(owner_user_id);
        let row_owner = crate::owner::resolved_owner(row_owner);
        if caller != row_owner {
            return Err(no_draft_error(noun, urn));
        }
    }
    Ok(())
}

fn catalog_table_noun(table: &str) -> &'static str {
    match table {
        "agents" => "agent",
        "playbooks" => "playbook",
        "skills" => "skill",
        _ => "policy",
    }
}

fn versioned_spec_catalog_ddl(table: &str) -> String {
    format!(
        "
            CREATE TABLE IF NOT EXISTS {table} (
                urn TEXT NOT NULL,
                major INTEGER NOT NULL,
                minor INTEGER NOT NULL,
                patch INTEGER NOT NULL,
                prerelease TEXT NOT NULL DEFAULT '',
                channel TEXT NOT NULL {CATALOG_CHANNEL_CHECK_SQL},
                revision INTEGER NOT NULL,
                digest TEXT NOT NULL,
                spec_bytes BLOB NOT NULL,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                owner_user_id TEXT REFERENCES users(user_id),
                catalog_tier TEXT NOT NULL DEFAULT 'user' {CATALOG_TIER_CHECK_SQL},
                PRIMARY KEY (urn, major, minor, patch, prerelease)
            );
        ",
        CATALOG_TIER_CHECK_SQL = crate::catalog_tier::CATALOG_TIER_CHECK_SQL
    )
}

fn versioned_policies_catalog_ddl() -> String {
    format!(
        "
            CREATE TABLE IF NOT EXISTS policies (
                urn TEXT NOT NULL,
                major INTEGER NOT NULL,
                minor INTEGER NOT NULL,
                patch INTEGER NOT NULL,
                prerelease TEXT NOT NULL DEFAULT '',
                channel TEXT NOT NULL {CATALOG_CHANNEL_CHECK_SQL},
                revision INTEGER NOT NULL,
                digest TEXT NOT NULL,
                cedar TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                owner_user_id TEXT REFERENCES users(user_id),
                catalog_tier TEXT NOT NULL DEFAULT 'user' {CATALOG_TIER_CHECK_SQL},
                PRIMARY KEY (urn, major, minor, patch, prerelease)
            );
        ",
        CATALOG_TIER_CHECK_SQL = crate::catalog_tier::CATALOG_TIER_CHECK_SQL
    )
}

fn catalog_table_is_legacy(
    connection: &rusqlite::Connection,
    table: &str,
) -> Result<bool, OrchestratorError> {
    Ok(!table_has_column(connection, table, "major")?)
}

fn table_has_column(
    connection: &rusqlite::Connection,
    table: &str,
    column: &str,
) -> Result<bool, OrchestratorError> {
    let mut stmt = connection
        .prepare(&format!("PRAGMA table_info({table})"))
        .map_err(OrchestratorError::Database)?;
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(OrchestratorError::Database)?;
    for name in rows {
        if name.map_err(OrchestratorError::Database)? == column {
            return Ok(true);
        }
    }
    Ok(false)
}

fn migrate_versioned_spec_table(
    connection: &rusqlite::Connection,
    table: &str,
) -> Result<(), OrchestratorError> {
    let tmp = format!("{table}__versioned_new");
    connection
        .execute("PRAGMA foreign_keys = OFF", [])
        .map_err(OrchestratorError::Database)?;
    let migrated = (|| -> Result<(), rusqlite::Error> {
        connection.execute_batch(&format!(
            "
            CREATE TABLE {tmp} (
                urn TEXT NOT NULL,
                major INTEGER NOT NULL,
                minor INTEGER NOT NULL,
                patch INTEGER NOT NULL,
                prerelease TEXT NOT NULL DEFAULT '',
                channel TEXT NOT NULL {CATALOG_CHANNEL_CHECK_SQL},
                revision INTEGER NOT NULL,
                digest TEXT NOT NULL,
                spec_bytes BLOB NOT NULL,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                owner_user_id TEXT REFERENCES users(user_id),
                PRIMARY KEY (urn, major, minor, patch, prerelease)
            );
            INSERT INTO {tmp} (
                urn, major, minor, patch, prerelease, channel, revision, digest,
                spec_bytes, created_at, updated_at, owner_user_id
            )
            SELECT urn,
                   {CATALOG_RELEASED_MAJOR},
                   {CATALOG_RELEASED_MINOR},
                   {CATALOG_RELEASED_PATCH},
                   '{CATALOG_RELEASED_PRERELEASE}',
                   '{CATALOG_CHANNEL_RELEASED}',
                   revision,
                   digest,
                   spec_bytes,
                   created_at,
                   created_at,
                   COALESCE(owner_user_id, 'local')
            FROM {table};
            DROP TABLE {table};
            ALTER TABLE {tmp} RENAME TO {table};
            "
        ))?;
        Ok(())
    })();
    connection
        .execute("PRAGMA foreign_keys = ON", [])
        .map_err(OrchestratorError::Database)?;
    migrated.map_err(OrchestratorError::Database)
}

fn migrate_versioned_policies_table(
    connection: &rusqlite::Connection,
) -> Result<(), OrchestratorError> {
    const TABLE: &str = "policies";
    let tmp = format!("{TABLE}__versioned_new");
    connection
        .execute("PRAGMA foreign_keys = OFF", [])
        .map_err(OrchestratorError::Database)?;
    let migrated = (|| -> Result<(), rusqlite::Error> {
        connection.execute_batch(&format!(
            "
            CREATE TABLE {tmp} (
                urn TEXT NOT NULL,
                major INTEGER NOT NULL,
                minor INTEGER NOT NULL,
                patch INTEGER NOT NULL,
                prerelease TEXT NOT NULL DEFAULT '',
                channel TEXT NOT NULL {CATALOG_CHANNEL_CHECK_SQL},
                revision INTEGER NOT NULL,
                digest TEXT NOT NULL,
                cedar TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                owner_user_id TEXT REFERENCES users(user_id),
                PRIMARY KEY (urn, major, minor, patch, prerelease)
            );
            INSERT INTO {tmp} (
                urn, major, minor, patch, prerelease, channel, revision, digest, cedar,
                created_at, updated_at, owner_user_id
            )
            SELECT urn,
                   {CATALOG_RELEASED_MAJOR},
                   {CATALOG_RELEASED_MINOR},
                   {CATALOG_RELEASED_PATCH},
                   '{CATALOG_RELEASED_PRERELEASE}',
                   '{CATALOG_CHANNEL_RELEASED}',
                   revision,
                   digest,
                   cedar,
                   created_at,
                   updated_at,
                   COALESCE(owner_user_id, 'local')
            FROM {TABLE};
            DROP TABLE {TABLE};
            ALTER TABLE {tmp} RENAME TO {TABLE};
            "
        ))?;
        Ok(())
    })();
    connection
        .execute("PRAGMA foreign_keys = ON", [])
        .map_err(OrchestratorError::Database)?;
    migrated.map_err(OrchestratorError::Database)
}

/// Tables whose released rows record a release note and who released them.
pub(crate) const CATALOG_RELEASE_META_TABLES: [&str; 5] = [
    "agents",
    "playbooks",
    "skills",
    "policies",
    "skill_packages",
];

/// Add nullable `release_note` and `released_by` to every catalog table. Rows
/// released before this migration read back as an empty note and releaser.
/// Idempotent. Runs after the table rebuilds, which would drop the columns.
pub(crate) fn ensure_release_meta_columns(
    connection: &rusqlite::Connection,
) -> Result<(), OrchestratorError> {
    for table in CATALOG_RELEASE_META_TABLES {
        for column in ["release_note", "released_by"] {
            if !table_has_column(connection, table, column)? {
                connection.execute(&format!("ALTER TABLE {table} ADD COLUMN {column} TEXT"), [])?;
            }
        }
    }
    Ok(())
}

/// Tables that hold owner drafts. `skill_packages` is included.
pub(crate) const CATALOG_DRAFT_TABLES: [&str; 5] = [
    "agents",
    "playbooks",
    "skills",
    "policies",
    "skill_packages",
];

/// Keep one draft row per URN in `table`, then enforce it with a partial unique
/// index. Keeps the newest draft by `updated_at` (ties by `rowid`) and deletes
/// the rest. Blobs stay on disk. Logs the count. Idempotent.
pub(crate) fn enforce_one_draft_per_urn(
    connection: &rusqlite::Connection,
    table: &str,
) -> Result<(), OrchestratorError> {
    let removed = connection.execute(
        &format!(
            "DELETE FROM {table} WHERE channel = 'draft' AND EXISTS (
                SELECT 1 FROM {table} newer
                WHERE newer.urn = {table}.urn AND newer.channel = 'draft'
                  AND (newer.updated_at > {table}.updated_at
                       OR (newer.updated_at = {table}.updated_at
                           AND newer.rowid > {table}.rowid)))"
        ),
        [],
    )?;
    tracing::info!(table, removed, "collapsed duplicate catalog draft rows");
    connection.execute_batch(&format!(
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_{table}_one_draft
             ON {table}(urn) WHERE channel = 'draft';"
    ))?;
    Ok(())
}

fn canonical_catalog_version_sql() -> &'static str {
    "major = ?2 AND minor = ?3 AND patch = ?4 AND prerelease = ?5"
}

fn latest_released_where_sql() -> &'static str {
    "urn = ?1 AND channel = 'released' ORDER BY major DESC, minor DESC, patch DESC LIMIT 1"
}

fn latest_draft_owned_where_sql() -> &'static str {
    "urn = ?1 AND channel = 'draft' AND owner_user_id = ?2 \
     ORDER BY major DESC, minor DESC, patch DESC LIMIT 1"
}

/// Tenant published rows plus the caller's own drafts.
fn catalog_visible_where_sql() -> &'static str {
    "channel = 'released' OR owner_user_id = ?1"
}

fn latest_draft_any_where_sql() -> &'static str {
    "urn = ?1 AND channel = 'draft' ORDER BY major DESC, minor DESC, patch DESC LIMIT 1"
}

fn catalog_channel_to_db(channel: i32) -> &'static str {
    if channel == SkillChannel::Draft as i32 {
        CATALOG_CHANNEL_DRAFT
    } else {
        CATALOG_CHANNEL_RELEASED
    }
}

/// SHA-256 hex of a policy's Cedar text: the value the catalog stores as a
/// policy's digest at publish. Pin enforcement recomputes it from the stored
/// text instead of trusting the `digest` column.
pub fn policy_cedar_digest(cedar: &str) -> String {
    use sha2::{Digest as _, Sha256};
    format!("{:x}", Sha256::digest(cedar.as_bytes()))
}

pub(crate) fn catalog_channel_from_db(channel: &str) -> SkillChannel {
    match channel {
        CATALOG_CHANNEL_DRAFT => SkillChannel::Draft,
        CATALOG_CHANNEL_RELEASED => SkillChannel::Released,
        _ => SkillChannel::Unspecified,
    }
}

/// Version on a list record. A released row keeps its semver. A draft row
/// carries its stored slot with the draft prerelease, because a draft has no
/// version (locked decision 2).
pub(crate) fn catalog_record_version(
    channel: &str,
    major: i32,
    minor: i32,
    patch: i32,
    prerelease: &str,
) -> vam_spec::v1::Version {
    let prerelease = if channel == CATALOG_CHANNEL_DRAFT {
        CATALOG_DRAFT_PRERELEASE
    } else {
        prerelease
    };
    vam_spec::v1::Version {
        major,
        minor,
        patch,
        prerelease: prerelease.to_string(),
        build: String::new(),
    }
}

fn agent_record_from_listing(listing: CatalogListingRow) -> Result<AgentRecord, OrchestratorError> {
    let spec = AgentSpec::decode(&*listing.spec_bytes)
        .map_err(|e| OrchestratorError::Core(e.to_string()))?;
    Ok(AgentRecord {
        spec: Some(spec),
        owner_user_id: listing.owner_user_id,
        channel: catalog_channel_from_db(&listing.channel) as i32,
        read_only: listing.catalog_tier == crate::catalog_tier::CATALOG_TIER_MARKETPLACE,
        tier: listing.catalog_tier.clone(),
        catalog_tier: listing.catalog_tier,
        version: Some(listing.version),
        origin: None,
    })
}

/// Version + digest of a released catalog row.
#[derive(Debug, Clone, PartialEq)]
pub struct CatalogPin {
    pub version: vam_spec::v1::Version,
    pub digest: String,
}

struct CatalogListingRow {
    spec_bytes: Vec<u8>,
    owner_user_id: String,
    channel: String,
    catalog_tier: String,
    version: vam_spec::v1::Version,
}

/// Selects `spec_bytes, owner, channel, catalog_tier, major, minor, patch, prerelease`.
fn catalog_listing_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<CatalogListingRow> {
    let channel: String = row.get(2)?;
    let version = catalog_record_version(
        &channel,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        &row.get::<_, String>(7)?,
    );
    Ok(CatalogListingRow {
        spec_bytes: row.get(0)?,
        owner_user_id: row.get(1)?,
        channel,
        catalog_tier: row.get(3)?,
        version,
    })
}

/// Selects `urn, cedar, digest, revision, catalog_tier, channel, major, minor, patch, prerelease`.
fn policy_record_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<PolicyRecord> {
    let urn: String = row.get(0)?;
    let cedar: String = row.get(1)?;
    let digest: String = row.get(2)?;
    let revision: i64 = row.get(3)?;
    let catalog_tier: String = row.get(4)?;
    let channel: String = row.get(5)?;
    let version = catalog_record_version(
        &channel,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
        &row.get::<_, String>(9)?,
    );
    Ok(PolicyRecord {
        resource_id: Some(ResourceId { urn }),
        cedar,
        digest,
        catalog_tier,
        metadata: Some(ResourceMetadata {
            revision,
            ..Default::default()
        }),
        version: Some(version),
        channel: catalog_channel_from_db(&channel) as i32,
    })
}

fn catalog_version_not_found(
    table: &str,
    urn: &str,
    version: &vam_spec::v1::Version,
) -> OrchestratorError {
    OrchestratorError::NotFound(format!(
        "{table} '{urn}' {}.{}.{}{} not found in catalog",
        version.major,
        version.minor,
        version.patch,
        if version.prerelease.is_empty() {
            String::new()
        } else {
            format!("-{}", version.prerelease)
        }
    ))
}

fn catalog_pin(
    major: i32,
    minor: i32,
    patch: i32,
    prerelease: String,
    digest: String,
) -> CatalogPin {
    CatalogPin {
        version: vam_spec::v1::Version {
            major,
            minor,
            patch,
            prerelease,
            build: String::new(),
        },
        digest,
    }
}

/// One entry in an artifact's version history: a released row, or the caller's draft.
#[derive(Debug, Clone)]
pub struct CatalogVersionRow {
    pub version: vam_spec::v1::Version,
    pub channel: SkillChannel,
    pub digest: String,
    pub released_by: String,
    /// Release time (the row's `created_at`). Zero for a draft.
    pub released_at: i64,
    pub updated_at: i64,
    pub note: String,
}

fn read_catalog_version_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<(CatalogVersionRow, String)> {
    let channel: String = row.get(4)?;
    let owner: Option<String> = row.get(10)?;
    let entry = CatalogVersionRow {
        version: vam_spec::v1::Version {
            major: row.get(0)?,
            minor: row.get(1)?,
            patch: row.get(2)?,
            prerelease: row.get(3)?,
            build: String::new(),
        },
        channel: catalog_channel_from_db(&channel),
        digest: row.get(5)?,
        released_by: row.get(6)?,
        released_at: row.get(7)?,
        updated_at: row.get(8)?,
        note: row.get(9)?,
    };
    Ok((entry, owner.unwrap_or_default()))
}

/// Version history rows for `urn`. Released rows are visible to every catalog
/// reader. The draft is visible to its owner or to `include_foreign_drafts`.
fn query_catalog_versions(
    connection: &rusqlite::Connection,
    table: &str,
    urn: &str,
    caller: &str,
    include_foreign_drafts: bool,
) -> Result<Vec<CatalogVersionRow>, OrchestratorError> {
    if !CATALOG_RELEASE_META_TABLES.contains(&table) {
        return Err(OrchestratorError::InvalidContract(format!(
            "unknown catalog table '{table}'"
        )));
    }
    let mut stmt = connection
        .prepare(&format!(
            "SELECT major, minor, patch, prerelease, channel, digest,
                    COALESCE(released_by, ''), created_at, updated_at,
                    COALESCE(release_note, ''), owner_user_id
             FROM {table}
             WHERE urn = ?1 AND channel IN ('released', 'draft')
             ORDER BY (channel = 'draft') DESC, major DESC, minor DESC, patch DESC"
        ))
        .map_err(OrchestratorError::Database)?;
    let rows = stmt
        .query_map(params![urn], read_catalog_version_row)
        .map_err(OrchestratorError::Database)?;
    let mut versions = Vec::new();
    for row in rows {
        let (entry, owner) = row.map_err(OrchestratorError::Database)?;
        let is_foreign_draft = entry.channel == SkillChannel::Draft
            && !include_foreign_drafts
            && crate::owner::resolved_owner(&owner) != caller;
        if !is_foreign_draft {
            versions.push(entry);
        }
    }
    if versions.is_empty() {
        return Err(OrchestratorError::NotFound(format!(
            "{table} '{urn}' has no versions visible to this caller"
        )));
    }
    Ok(versions)
}

fn assert_catalog_table(table: &str) -> Result<(), OrchestratorError> {
    match table {
        "agents" | "playbooks" | "skills" | "policies" => Ok(()),
        _ => Err(OrchestratorError::InvalidContract(format!(
            "unknown catalog table '{table}'"
        ))),
    }
}

impl SqliteWorkflowStore {
    pub fn initialize_catalog_schema(&self) -> Result<(), OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let batch = format!(
            "{}{}{}{}
            CREATE TABLE IF NOT EXISTS skill_packages (
",
            versioned_spec_catalog_ddl("agents"),
            versioned_spec_catalog_ddl("playbooks"),
            versioned_spec_catalog_ddl("skills"),
            versioned_policies_catalog_ddl(),
        );
        connection.execute_batch(&format!(
            "{batch}
                urn TEXT NOT NULL,
                major INTEGER NOT NULL,
                minor INTEGER NOT NULL,
                patch INTEGER NOT NULL,
                prerelease TEXT NOT NULL DEFAULT '',
                channel TEXT NOT NULL {CATALOG_CHANNEL_CHECK_SQL},
                revision INTEGER NOT NULL,
                digest TEXT NOT NULL,
                blob_relpath TEXT NOT NULL,
                size_bytes INTEGER NOT NULL,
                spec_bytes BLOB NOT NULL,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                owner_user_id TEXT REFERENCES users(user_id),
                catalog_tier TEXT NOT NULL DEFAULT 'user' {CATALOG_TIER_CHECK_SQL},
                PRIMARY KEY (urn, major, minor, patch, prerelease)
            );
            ",
            CATALOG_TIER_CHECK_SQL = crate::catalog_tier::CATALOG_TIER_CHECK_SQL
        ))?;
        ensure_release_meta_columns(&connection)?;
        Ok(())
    }

    /// Rebuild legacy `urn`-PK catalog tables into the versioned `skill_packages`
    /// shape. Existing rows become `1.0.0` with channel `released`. Idempotent.
    pub fn initialize_versioned_catalog_artifacts(&self) -> Result<(), OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        for table in [
            CatalogKind::Agent,
            CatalogKind::Playbook,
            CatalogKind::Skill,
        ] {
            if catalog_table_is_legacy(&connection, table.table())? {
                migrate_versioned_spec_table(&connection, table.table())?;
            }
        }
        if catalog_table_is_legacy(&connection, "policies")? {
            migrate_versioned_policies_table(&connection)?;
        }
        for table in CATALOG_DRAFT_TABLES {
            enforce_one_draft_per_urn(&connection, table)?;
        }
        ensure_release_meta_columns(&connection)?;
        Ok(())
    }

    /// Version history for `urn` in `table`, newest release first. The caller's
    /// draft is first when `caller` owns it or `include_foreign_drafts` is set.
    /// NotFound when nothing is visible to the caller.
    pub fn list_catalog_versions(
        &self,
        table: &str,
        urn: &str,
        caller: &str,
        include_foreign_drafts: bool,
    ) -> Result<Vec<CatalogVersionRow>, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        query_catalog_versions(&connection, table, urn, caller, include_foreign_drafts)
    }

    #[cfg(test)]
    pub(crate) fn catalog_row_count(
        &self,
        table: &str,
        urn: &str,
    ) -> Result<i64, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        connection
            .query_row(
                &format!("SELECT COUNT(*) FROM {table} WHERE urn = ?1"),
                [urn],
                |row| row.get(0),
            )
            .map_err(OrchestratorError::Database)
    }

    #[cfg(test)]
    pub(crate) fn catalog_channel_at_version(
        &self,
        table: &str,
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
                &format!(
                    "SELECT channel FROM {table}
                     WHERE urn = ?1 AND major = ?2 AND minor = ?3 AND patch = ?4 AND prerelease = ?5"
                ),
                params![urn, major, minor, patch, prerelease],
                |row| row.get(0),
            )
            .map_err(OrchestratorError::Database)
    }

    pub fn initialize_runs_extended_schema(&self) -> Result<(), OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        // Additive columns for run metadata (ignore if already present).
        let _ = connection.execute("ALTER TABLE runs ADD COLUMN spec_json TEXT", []);
        let _ = connection.execute("ALTER TABLE runs ADD COLUMN session_id TEXT", []);
        let _ = connection.execute("ALTER TABLE runs ADD COLUMN agent_urn TEXT", []);
        let _ = connection.execute("ALTER TABLE runs ADD COLUMN idempotency_key TEXT", []);
        let _ = connection.execute("ALTER TABLE runs ADD COLUMN owner_principal TEXT", []);
        let _ = connection.execute("ALTER TABLE runs ADD COLUMN result_json TEXT", []);
        let _ = connection.execute(
            "ALTER TABLE runs ADD COLUMN owner_user_id TEXT REFERENCES users(user_id) DEFAULT 'local'",
            [],
        );
        // Unix time when a PAUSED run's keep-warm window ends. Null once parked
        // or while the run is not waiting.
        let _ = connection.execute("ALTER TABLE runs ADD COLUMN warm_until INTEGER", []);
        // Closed active seconds. Parked HITL gaps are not added.
        let _ = connection.execute(
            "ALTER TABLE runs ADD COLUMN active_elapsed_secs INTEGER NOT NULL DEFAULT 0",
            [],
        );
        // Unix start of the open active segment. Null while parked or idle.
        let _ = connection.execute("ALTER TABLE runs ADD COLUMN active_since INTEGER", []);
        // Where a run came from: NULL for a person, `broker:<id>` for a triggered run; and
        // the grant credential that fired it.
        let _ = connection.execute("ALTER TABLE runs ADD COLUMN source TEXT", []);
        let _ = connection.execute("ALTER TABLE runs ADD COLUMN credential_id TEXT", []);
        Ok(())
    }

    pub fn put_agent(&self, spec: &AgentSpec) -> Result<AgentSpec, OrchestratorError> {
        self.put_agent_with_owner(spec, crate::users::LOCAL_USER_ID)
    }

    pub fn put_agent_with_owner(
        &self,
        spec: &AgentSpec,
        owner_user_id: &str,
    ) -> Result<AgentSpec, OrchestratorError> {
        spec.validate()
            .map_err(|e| OrchestratorError::InvalidContract(e.to_string()))?;
        let urn = spec
            .resource_id
            .as_ref()
            .map(|r| r.urn.as_str())
            .filter(|u| !u.is_empty())
            .ok_or_else(|| {
                OrchestratorError::InvalidContract("AgentSpec.resource_id is required".to_string())
            })?;
        self.publish_agent_draft_with_owner(spec, owner_user_id)?;
        self.release_agent(urn, &crate::catalog_publish::catalog_release_version())
    }

    pub fn get_agent(&self, urn: &str) -> Result<AgentSpec, OrchestratorError> {
        self.get_catalog_message(CatalogKind::Agent, urn)
    }

    /// Latest **released** spec, else the caller's own draft. Foreign drafts are NotFound
    /// unless `include_foreign_drafts` (org admin / `users:admin`).
    pub fn get_agent_visible(
        &self,
        urn: &str,
        owner_user_id: &str,
        include_foreign_drafts: bool,
    ) -> Result<AgentSpec, OrchestratorError> {
        self.get_agent_record_visible(urn, owner_user_id, include_foreign_drafts)?
            .spec
            .ok_or_else(|| {
                OrchestratorError::CorruptState(format!("agent '{urn}' listing missing spec"))
            })
    }

    /// Same visibility as [`Self::get_agent_visible`], with owner / channel / tier.
    pub fn get_agent_record_visible(
        &self,
        urn: &str,
        owner_user_id: &str,
        include_foreign_drafts: bool,
    ) -> Result<AgentRecord, OrchestratorError> {
        self.get_catalog_record_visible(
            CatalogKind::Agent,
            urn,
            owner_user_id,
            include_foreign_drafts,
        )
    }

    pub fn get_agent_at_version(
        &self,
        urn: &str,
        version: &vam_spec::v1::Version,
    ) -> Result<AgentSpec, OrchestratorError> {
        let (bytes, _) = self.query_spec_at_version(CatalogKind::Agent.table(), urn, version)?;
        AgentSpec::decode(&*bytes).map_err(|e| OrchestratorError::Core(e.to_string()))
    }

    /// Exact catalog version, with the same draft visibility as GET.
    ///
    /// Released rows are tenant-visible. Draft pins load the owner draft (or a
    /// foreign draft when `include_foreign_drafts`). Other callers get NotFound
    /// using the same missing-version message so a foreign draft is not leaked.
    pub fn get_agent_at_version_visible(
        &self,
        urn: &str,
        version: &vam_spec::v1::Version,
        owner_user_id: &str,
        include_foreign_drafts: bool,
    ) -> Result<AgentSpec, OrchestratorError> {
        let (bytes, _pin, row_owner, channel) =
            self.query_spec_at_version_with_owner("agents", urn, version)?;
        ensure_draft_visible(
            "agent",
            urn,
            &channel,
            &row_owner,
            owner_user_id,
            include_foreign_drafts,
        )?;
        AgentSpec::decode(&*bytes).map_err(|e| OrchestratorError::Core(e.to_string()))
    }

    pub fn latest_released_catalog_pin(
        &self,
        table: &str,
        urn: &str,
    ) -> Result<CatalogPin, OrchestratorError> {
        self.query_latest_released_pin(table, urn)
    }

    /// Pin at an exact version. A draft pin only resolves for its owner (or
    /// when `include_foreign_drafts`); a foreign draft is `no_draft_error`.
    pub fn catalog_pin_at_version(
        &self,
        table: &str,
        urn: &str,
        version: &vam_spec::v1::Version,
        owner_user_id: &str,
        include_foreign_drafts: bool,
    ) -> Result<CatalogPin, OrchestratorError> {
        self.query_catalog_pin_at_version(
            table,
            urn,
            version,
            owner_user_id,
            include_foreign_drafts,
        )
    }

    pub fn list_agents(&self) -> Result<Vec<AgentSpec>, OrchestratorError> {
        Ok(self
            .list_agent_records_filtered(None)?
            .into_iter()
            .filter_map(|row| row.spec)
            .collect())
    }

    /// Own drafts plus every tenant **released** row. Foreign drafts are omitted
    /// unless `include_foreign_drafts`.
    pub fn list_agents_visible(
        &self,
        owner_user_id: &str,
        include_foreign_drafts: bool,
    ) -> Result<Vec<AgentSpec>, OrchestratorError> {
        Ok(self
            .list_agent_records_visible(owner_user_id, include_foreign_drafts)?
            .into_iter()
            .filter_map(|row| row.spec)
            .collect())
    }

    /// Same visibility as [`Self::list_agents_visible`], with owner / channel / tier.
    pub fn list_agent_records_visible(
        &self,
        owner_user_id: &str,
        include_foreign_drafts: bool,
    ) -> Result<Vec<AgentRecord>, OrchestratorError> {
        let owner = if include_foreign_drafts {
            None
        } else {
            Some(crate::owner::resolved_owner(owner_user_id))
        };
        self.list_agent_records_filtered(owner)
    }

    fn list_agent_records_filtered(
        &self,
        owner_user_id: Option<&str>,
    ) -> Result<Vec<AgentRecord>, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let visible = catalog_visible_where_sql();
        let sql = if owner_user_id.is_some() {
            format!(
                "SELECT spec_bytes, COALESCE(NULLIF(owner_user_id, ''), 'local'),
                        channel, catalog_tier, major, minor, patch, prerelease
                 FROM agents WHERE {visible} ORDER BY created_at DESC"
            )
        } else {
            "SELECT spec_bytes, COALESCE(NULLIF(owner_user_id, ''), 'local'),
                    channel, catalog_tier, major, minor, patch, prerelease
             FROM agents ORDER BY created_at DESC"
                .to_string()
        };
        let mut stmt = connection.prepare(&sql)?;
        let rows = match owner_user_id {
            Some(owner) => stmt.query_map(params![owner], catalog_listing_from_row)?,
            None => stmt.query_map([], catalog_listing_from_row)?,
        };
        rows.map(|row| agent_record_from_listing(row.map_err(OrchestratorError::Database)?))
            .collect()
    }

    pub fn put_playbook(&self, spec: &PlaybookSpec) -> Result<PlaybookSpec, OrchestratorError> {
        self.put_playbook_with_owner(spec, crate::users::LOCAL_USER_ID)
    }

    pub fn put_playbook_with_owner(
        &self,
        spec: &PlaybookSpec,
        owner_user_id: &str,
    ) -> Result<PlaybookSpec, OrchestratorError> {
        spec.validate()
            .map_err(|e| OrchestratorError::InvalidContract(e.to_string()))?;
        let urn = spec
            .resource_id
            .as_ref()
            .map(|r| r.urn.as_str())
            .filter(|u| !u.is_empty())
            .ok_or_else(|| {
                OrchestratorError::InvalidContract(
                    "PlaybookSpec.resource_id is required".to_string(),
                )
            })?;
        self.publish_playbook_draft_with_owner(spec, owner_user_id)?;
        self.release_playbook(urn, &crate::catalog_publish::catalog_release_version())
    }

    /// Own drafts plus every **released** playbook row, newest first.
    pub fn list_playbooks_visible(
        &self,
        owner_user_id: &str,
    ) -> Result<Vec<PlaybookSpec>, OrchestratorError> {
        let owner = crate::owner::resolved_owner(owner_user_id);
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let visible = catalog_visible_where_sql();
        let sql =
            format!("SELECT spec_bytes FROM playbooks WHERE {visible} ORDER BY created_at DESC");
        let mut stmt = connection.prepare(&sql)?;
        let rows = stmt.query_map(params![owner], |row| row.get::<_, Vec<u8>>(0))?;
        rows.map(|row| {
            let bytes = row.map_err(OrchestratorError::Database)?;
            PlaybookSpec::decode(&*bytes).map_err(|e| OrchestratorError::Core(e.to_string()))
        })
        .collect()
    }

    /// Same rows and order as [`Self::list_playbooks_visible`], with owner, channel,
    /// tier and version for each.
    pub fn list_playbook_records_visible(
        &self,
        owner_user_id: &str,
    ) -> Result<Vec<vam_spec::v1::PlaybookRecord>, OrchestratorError> {
        let owner = crate::owner::resolved_owner(owner_user_id);
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let visible = catalog_visible_where_sql();
        let sql = format!(
            "SELECT spec_bytes, COALESCE(NULLIF(owner_user_id, ''), 'local'),
                    channel, catalog_tier, major, minor, patch, prerelease
             FROM playbooks WHERE {visible} ORDER BY created_at DESC"
        );
        let mut stmt = connection.prepare(&sql)?;
        let rows = stmt.query_map(params![owner], catalog_listing_from_row)?;
        rows.map(|row| {
            let listing = row.map_err(OrchestratorError::Database)?;
            let spec = PlaybookSpec::decode(&*listing.spec_bytes)
                .map_err(|e| OrchestratorError::Core(e.to_string()))?;
            Ok(vam_spec::v1::PlaybookRecord {
                spec: Some(spec),
                owner_user_id: listing.owner_user_id,
                channel: catalog_channel_from_db(&listing.channel) as i32,
                read_only: listing.catalog_tier == crate::catalog_tier::CATALOG_TIER_MARKETPLACE,
                tier: listing.catalog_tier.clone(),
                catalog_tier: listing.catalog_tier,
                version: Some(listing.version),
                origin: None,
            })
        })
        .collect()
    }

    /// Exact skill manifest version, with the same draft visibility as playbooks.
    pub fn get_skill_at_version(
        &self,
        urn: &str,
        version: &vam_spec::v1::Version,
        owner_user_id: &str,
        include_foreign_drafts: bool,
    ) -> Result<SkillManifest, OrchestratorError> {
        self.get_catalog_message_at_version(
            CatalogKind::Skill,
            urn,
            version,
            owner_user_id,
            include_foreign_drafts,
        )
    }

    pub fn get_playbook(&self, urn: &str) -> Result<PlaybookSpec, OrchestratorError> {
        self.get_catalog_message(CatalogKind::Playbook, urn)
    }

    /// Exact playbook version. Draft pins load only the caller's own draft
    /// (or any draft when `include_foreign_drafts`).
    pub fn get_playbook_at_version(
        &self,
        urn: &str,
        version: &vam_spec::v1::Version,
        owner_user_id: &str,
        include_foreign_drafts: bool,
    ) -> Result<PlaybookSpec, OrchestratorError> {
        self.get_catalog_message_at_version(
            CatalogKind::Playbook,
            urn,
            version,
            owner_user_id,
            include_foreign_drafts,
        )
    }

    pub fn get_playbook_visible(
        &self,
        urn: &str,
        owner_user_id: &str,
    ) -> Result<PlaybookSpec, OrchestratorError> {
        self.get_catalog_message_visible(CatalogKind::Playbook, urn, owner_user_id)
    }

    pub fn put_skill(&self, spec: &SkillManifest) -> Result<SkillManifest, OrchestratorError> {
        self.put_skill_with_owner(spec, crate::users::LOCAL_USER_ID)
    }

    pub fn put_skill_with_owner(
        &self,
        spec: &SkillManifest,
        owner_user_id: &str,
    ) -> Result<SkillManifest, OrchestratorError> {
        spec.validate()
            .map_err(|e| OrchestratorError::InvalidContract(e.to_string()))?;
        let urn = spec
            .resource_id
            .as_ref()
            .map(|r| r.urn.as_str())
            .filter(|u| !u.is_empty())
            .ok_or_else(|| {
                OrchestratorError::InvalidContract(
                    "SkillManifest.resource_id is required".to_string(),
                )
            })?;
        self.publish_skill_draft_with_owner(spec, owner_user_id)?;
        self.release_skill(urn, &crate::catalog_publish::catalog_release_version())
    }

    pub fn get_skill(&self, urn: &str) -> Result<SkillManifest, OrchestratorError> {
        self.get_catalog_message(CatalogKind::Skill, urn)
    }

    pub fn get_skill_visible(
        &self,
        urn: &str,
        owner_user_id: &str,
    ) -> Result<SkillManifest, OrchestratorError> {
        self.get_catalog_message_visible(CatalogKind::Skill, urn, owner_user_id)
    }

    pub fn put_policy(&self, urn: &str, cedar: &str) -> Result<PolicyRecord, OrchestratorError> {
        self.put_policy_with_owner(urn, cedar, crate::users::LOCAL_USER_ID)
    }

    pub fn put_policy_with_owner(
        &self,
        urn: &str,
        cedar: &str,
        owner_user_id: &str,
    ) -> Result<PolicyRecord, OrchestratorError> {
        let urn = urn.trim();
        if urn.is_empty() {
            return Err(OrchestratorError::InvalidContract(
                "policy URN is required".to_string(),
            ));
        }
        if cedar.trim().is_empty() {
            return Err(OrchestratorError::InvalidContract(
                "cedar policy text is required".to_string(),
            ));
        }
        self.publish_policy_draft_with_owner(urn, cedar, owner_user_id)?;
        self.release_policy(urn, &crate::catalog_publish::catalog_release_version())
    }

    /// Insert an `examples.local` policy as a tenant-visible **platform** artifact.
    /// Not called on vamd boot; tests use it to stamp `catalog_tier = platform`.
    pub fn put_platform_policy(
        &self,
        urn: &str,
        cedar: &str,
    ) -> Result<PolicyRecord, OrchestratorError> {
        let urn = urn.trim();
        if urn.is_empty() {
            return Err(OrchestratorError::InvalidContract(
                "policy URN is required".to_string(),
            ));
        }
        if cedar.trim().is_empty() {
            return Err(OrchestratorError::InvalidContract(
                "cedar policy text is required".to_string(),
            ));
        }
        if !crate::catalog_tier::is_platform_example_urn(urn) {
            return Err(OrchestratorError::InvalidContract(
                "platform policy seed requires an examples.local URN".to_string(),
            ));
        }
        self.publish_platform_policy_draft(urn, cedar)?;
        self.release_policy(urn, &crate::catalog_publish::catalog_release_version())
    }

    pub fn get_policy(&self, urn: &str) -> Result<PolicyRecord, OrchestratorError> {
        self.query_policy_row(urn, latest_released_where_sql(), params![urn])
    }

    /// Latest **released** policy, else the caller's own draft. Foreign drafts are NotFound.
    pub fn get_policy_visible(
        &self,
        urn: &str,
        owner_user_id: &str,
    ) -> Result<PolicyRecord, OrchestratorError> {
        match self.get_policy(urn) {
            Ok(record) => Ok(record),
            Err(OrchestratorError::NotFound(_)) => {
                let owner = crate::owner::resolved_owner(owner_user_id);
                self.query_policy_row(urn, latest_draft_owned_where_sql(), params![urn, owner])
            }
            Err(e) => Err(e),
        }
    }

    /// The policy row at an exact version, whatever the channel. A draft version
    /// (`prerelease == "draft"`) names the URN's one draft row and ignores its numbers.
    ///
    /// The returned `digest` is the stored column. Callers that enforce a pin must
    /// compare against [`policy_cedar_digest`] of `cedar`, not this column.
    pub fn get_policy_at_version(
        &self,
        urn: &str,
        version: &vam_spec::v1::Version,
    ) -> Result<PolicyRecord, OrchestratorError> {
        self.query_policy_at_version(urn, version)
            .map(|(record, _)| record)
    }

    /// [`Self::get_policy_at_version`] with draft visibility: a draft version only
    /// resolves for its owner. A foreign draft is NotFound, same as a missing one.
    pub fn get_policy_at_version_visible(
        &self,
        urn: &str,
        version: &vam_spec::v1::Version,
        owner_user_id: &str,
    ) -> Result<PolicyRecord, OrchestratorError> {
        let (record, row_owner) = self.query_policy_at_version(urn, version)?;
        ensure_draft_visible(
            "policy",
            urn,
            catalog_channel_to_db(record.channel),
            &row_owner,
            owner_user_id,
            false,
        )?;
        Ok(record)
    }

    fn query_policy_at_version(
        &self,
        urn: &str,
        version: &vam_spec::v1::Version,
    ) -> Result<(PolicyRecord, String), OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let select = "SELECT urn, cedar, digest, revision, catalog_tier,
                             channel, major, minor, patch, prerelease,
                             COALESCE(NULLIF(owner_user_id, ''), 'local')
                      FROM policies";
        let map_row = |row: &rusqlite::Row<'_>| -> rusqlite::Result<(PolicyRecord, String)> {
            Ok((policy_record_from_row(row)?, row.get(10)?))
        };
        let found = if version.prerelease == CATALOG_DRAFT_PRERELEASE {
            connection.query_row(
                &format!("{select} WHERE urn = ?1 AND channel = 'draft'"),
                params![urn],
                map_row,
            )
        } else {
            connection.query_row(
                &format!(
                    "{select} WHERE urn = ?1 AND {}",
                    canonical_catalog_version_sql()
                ),
                params![
                    urn,
                    version.major,
                    version.minor,
                    version.patch,
                    &version.prerelease,
                ],
                map_row,
            )
        };
        found.map_err(|e| {
            if e == rusqlite::Error::QueryReturnedNoRows {
                if version.prerelease == CATALOG_DRAFT_PRERELEASE {
                    no_draft_error("policy", urn)
                } else {
                    catalog_version_not_found("policy", urn, version)
                }
            } else {
                OrchestratorError::Database(e)
            }
        })
    }

    pub fn list_policies(&self) -> Result<Vec<PolicyRecord>, OrchestratorError> {
        self.list_policies_filtered(None)
    }

    /// Own drafts plus every tenant **released** row. Foreign drafts are omitted.
    pub fn list_policies_visible(
        &self,
        owner_user_id: &str,
    ) -> Result<Vec<PolicyRecord>, OrchestratorError> {
        self.list_policies_filtered(Some(crate::owner::resolved_owner(owner_user_id)))
    }

    fn list_policies_filtered(
        &self,
        owner_user_id: Option<&str>,
    ) -> Result<Vec<PolicyRecord>, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let visible = catalog_visible_where_sql();
        let sql = if owner_user_id.is_some() {
            format!(
                "SELECT urn, cedar, digest, revision, catalog_tier,
                        channel, major, minor, patch, prerelease
                 FROM policies WHERE {visible} ORDER BY updated_at DESC"
            )
        } else {
            "SELECT urn, cedar, digest, revision, catalog_tier,
                    channel, major, minor, patch, prerelease
             FROM policies ORDER BY updated_at DESC"
                .to_string()
        };
        let mut stmt = connection.prepare(&sql)?;
        let rows = match owner_user_id {
            Some(owner) => stmt.query_map(params![owner], policy_record_from_row)?,
            None => stmt.query_map([], policy_record_from_row)?,
        };
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(OrchestratorError::Database)
    }

    fn query_policy_row(
        &self,
        urn: &str,
        where_sql: &str,
        params: impl rusqlite::Params,
    ) -> Result<PolicyRecord, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        connection
            .query_row(
                &format!(
                    "SELECT urn, cedar, digest, revision, catalog_tier,
                            channel, major, minor, patch, prerelease
                     FROM policies WHERE {where_sql}"
                ),
                params,
                policy_record_from_row,
            )
            .map_err(|e| {
                if e == rusqlite::Error::QueryReturnedNoRows {
                    OrchestratorError::NotFound(format!("policy '{urn}' not found in catalog"))
                } else {
                    OrchestratorError::Database(e)
                }
            })
    }

    fn get_catalog_message<T: Message + Default>(
        &self,
        kind: CatalogKind,
        urn: &str,
    ) -> Result<T, OrchestratorError> {
        let (bytes, _) = self.query_latest_released_spec(kind.table(), urn)?;
        T::decode(&*bytes).map_err(|e| OrchestratorError::Core(e.to_string()))
    }

    fn get_catalog_message_visible<T: Message + Default>(
        &self,
        kind: CatalogKind,
        urn: &str,
        owner_user_id: &str,
    ) -> Result<T, OrchestratorError> {
        match self.query_latest_released_spec(kind.table(), urn) {
            Ok((bytes, _)) => {
                T::decode(&*bytes).map_err(|e| OrchestratorError::Core(e.to_string()))
            }
            Err(OrchestratorError::NotFound(_)) => {
                let (bytes, _) =
                    self.query_latest_draft_owned_spec(kind.table(), urn, owner_user_id)?;
                T::decode(&*bytes).map_err(|e| OrchestratorError::Core(e.to_string()))
            }
            Err(e) => Err(e),
        }
    }

    fn get_catalog_record_visible(
        &self,
        kind: CatalogKind,
        urn: &str,
        owner_user_id: &str,
        include_foreign_drafts: bool,
    ) -> Result<AgentRecord, OrchestratorError> {
        match self.query_latest_released_listing(kind.table(), urn) {
            Ok(listing) => agent_record_from_listing(listing),
            Err(OrchestratorError::NotFound(_)) => {
                let listing = if include_foreign_drafts {
                    self.query_latest_draft_listing(kind.table(), urn)?
                } else {
                    self.query_latest_draft_owned_listing(kind.table(), urn, owner_user_id)?
                };
                agent_record_from_listing(listing)
            }
            Err(e) => Err(e),
        }
    }

    fn get_catalog_message_at_version<T: Message + Default>(
        &self,
        kind: CatalogKind,
        urn: &str,
        version: &vam_spec::v1::Version,
        owner_user_id: &str,
        include_foreign_drafts: bool,
    ) -> Result<T, OrchestratorError> {
        let table = kind.table();
        let (bytes, _pin, row_owner, channel) =
            self.query_spec_at_version_with_owner(table, urn, version)?;
        ensure_draft_visible(
            catalog_table_noun(table),
            urn,
            &channel,
            &row_owner,
            owner_user_id,
            include_foreign_drafts,
        )?;
        T::decode(&*bytes).map_err(|e| OrchestratorError::Core(e.to_string()))
    }

    fn query_latest_released_spec(
        &self,
        table: &str,
        urn: &str,
    ) -> Result<(Vec<u8>, CatalogPin), OrchestratorError> {
        assert_catalog_table(table)?;
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let latest = latest_released_where_sql();
        connection
            .query_row(
                &format!(
                    "SELECT spec_bytes, digest, major, minor, patch, prerelease
                     FROM {table} WHERE {latest}"
                ),
                params![urn],
                |row| {
                    let bytes: Vec<u8> = row.get(0)?;
                    let digest: String = row.get(1)?;
                    let major: i32 = row.get(2)?;
                    let minor: i32 = row.get(3)?;
                    let patch: i32 = row.get(4)?;
                    let prerelease: String = row.get(5)?;
                    Ok((bytes, catalog_pin(major, minor, patch, prerelease, digest)))
                },
            )
            .map_err(|e| {
                if e == rusqlite::Error::QueryReturnedNoRows {
                    OrchestratorError::NotFound(format!("{table} '{urn}' not found in catalog"))
                } else {
                    OrchestratorError::Database(e)
                }
            })
    }

    fn query_latest_draft_owned_spec(
        &self,
        table: &str,
        urn: &str,
        owner_user_id: &str,
    ) -> Result<(Vec<u8>, CatalogPin), OrchestratorError> {
        assert_catalog_table(table)?;
        let owner = crate::owner::resolved_owner(owner_user_id);
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let latest = latest_draft_owned_where_sql();
        connection
            .query_row(
                &format!(
                    "SELECT spec_bytes, digest, major, minor, patch, prerelease
                     FROM {table} WHERE {latest}"
                ),
                params![urn, owner],
                |row| {
                    let bytes: Vec<u8> = row.get(0)?;
                    let digest: String = row.get(1)?;
                    let major: i32 = row.get(2)?;
                    let minor: i32 = row.get(3)?;
                    let patch: i32 = row.get(4)?;
                    let prerelease: String = row.get(5)?;
                    Ok((bytes, catalog_pin(major, minor, patch, prerelease, digest)))
                },
            )
            .map_err(|e| {
                if e == rusqlite::Error::QueryReturnedNoRows {
                    OrchestratorError::NotFound(format!("{table} '{urn}' not found in catalog"))
                } else {
                    OrchestratorError::Database(e)
                }
            })
    }

    fn query_latest_released_listing(
        &self,
        table: &str,
        urn: &str,
    ) -> Result<CatalogListingRow, OrchestratorError> {
        self.query_catalog_listing(table, urn, latest_released_where_sql(), params![urn])
    }

    fn query_latest_draft_listing(
        &self,
        table: &str,
        urn: &str,
    ) -> Result<CatalogListingRow, OrchestratorError> {
        self.query_catalog_listing(table, urn, latest_draft_any_where_sql(), params![urn])
    }

    fn query_latest_draft_owned_listing(
        &self,
        table: &str,
        urn: &str,
        owner_user_id: &str,
    ) -> Result<CatalogListingRow, OrchestratorError> {
        let owner = crate::owner::resolved_owner(owner_user_id);
        self.query_catalog_listing(
            table,
            urn,
            latest_draft_owned_where_sql(),
            params![urn, owner],
        )
    }

    fn query_catalog_listing(
        &self,
        table: &str,
        urn: &str,
        where_sql: &str,
        params: impl rusqlite::Params,
    ) -> Result<CatalogListingRow, OrchestratorError> {
        assert_catalog_table(table)?;
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        connection
            .query_row(
                &format!(
                    "SELECT spec_bytes, COALESCE(NULLIF(owner_user_id, ''), 'local'),
                            channel, catalog_tier, major, minor, patch, prerelease
                     FROM {table} WHERE {where_sql}"
                ),
                params,
                catalog_listing_from_row,
            )
            .map_err(|e| {
                if e == rusqlite::Error::QueryReturnedNoRows {
                    OrchestratorError::NotFound(format!("{table} '{urn}' not found in catalog"))
                } else {
                    OrchestratorError::Database(e)
                }
            })
    }

    fn query_spec_at_version(
        &self,
        table: &str,
        urn: &str,
        version: &vam_spec::v1::Version,
    ) -> Result<(Vec<u8>, CatalogPin), OrchestratorError> {
        self.query_spec_at_version_with_owner(table, urn, version)
            .map(|(bytes, pin, _, _)| (bytes, pin))
    }

    fn query_spec_at_version_with_owner(
        &self,
        table: &str,
        urn: &str,
        version: &vam_spec::v1::Version,
    ) -> Result<(Vec<u8>, CatalogPin, String, String), OrchestratorError> {
        assert_catalog_table(table)?;
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let select = format!(
            "SELECT spec_bytes, digest, COALESCE(NULLIF(owner_user_id, ''), 'local'),
                    channel, major, minor, patch, prerelease
             FROM {table}"
        );
        let map_row = |row: &rusqlite::Row<'_>| -> rusqlite::Result<_> {
            let bytes: Vec<u8> = row.get(0)?;
            let digest: String = row.get(1)?;
            let owner_user_id: String = row.get(2)?;
            let channel: String = row.get(3)?;
            let pin = catalog_pin(row.get(4)?, row.get(5)?, row.get(6)?, row.get(7)?, digest);
            Ok((bytes, pin, owner_user_id, channel))
        };
        // A draft pin ignores its numbers: the owner's one draft row for the URN.
        let found = if version.prerelease == CATALOG_DRAFT_PRERELEASE {
            connection.query_row(
                &format!("{select} WHERE urn = ?1 AND channel = 'draft'"),
                params![urn],
                map_row,
            )
        } else {
            connection.query_row(
                &format!(
                    "{select} WHERE urn = ?1 AND {}",
                    canonical_catalog_version_sql()
                ),
                params![
                    urn,
                    version.major,
                    version.minor,
                    version.patch,
                    &version.prerelease,
                ],
                map_row,
            )
        };
        found.map_err(|e| {
            if e == rusqlite::Error::QueryReturnedNoRows {
                if version.prerelease == CATALOG_DRAFT_PRERELEASE {
                    no_draft_error(catalog_table_noun(table), urn)
                } else {
                    catalog_version_not_found(table, urn, version)
                }
            } else {
                OrchestratorError::Database(e)
            }
        })
    }

    fn query_latest_released_pin(
        &self,
        table: &str,
        urn: &str,
    ) -> Result<CatalogPin, OrchestratorError> {
        if table == "policies" {
            assert_catalog_table(table)?;
            let connection = self
                .connection
                .lock()
                .map_err(|_| OrchestratorError::StorageUnavailable)?;
            let latest = latest_released_where_sql();
            return connection
                .query_row(
                    &format!(
                        "SELECT digest, major, minor, patch, prerelease
                         FROM policies WHERE {latest}"
                    ),
                    params![urn],
                    |row| {
                        Ok(catalog_pin(
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                            row.get(0)?,
                        ))
                    },
                )
                .map_err(|e| {
                    if e == rusqlite::Error::QueryReturnedNoRows {
                        OrchestratorError::NotFound(format!("policy '{urn}' not found in catalog"))
                    } else {
                        OrchestratorError::Database(e)
                    }
                });
        }
        self.query_latest_released_spec(table, urn)
            .map(|(_, pin)| pin)
    }

    fn query_catalog_pin_at_version(
        &self,
        table: &str,
        urn: &str,
        version: &vam_spec::v1::Version,
        owner_user_id: &str,
        include_foreign_drafts: bool,
    ) -> Result<CatalogPin, OrchestratorError> {
        if table == "policies" {
            assert_catalog_table(table)?;
            let connection = self
                .connection
                .lock()
                .map_err(|_| OrchestratorError::StorageUnavailable)?;
            // A draft pin ignores its numbers: the one draft row for the URN.
            if version.prerelease == CATALOG_DRAFT_PRERELEASE {
                let (pin, row_owner) = connection
                    .query_row(
                        "SELECT digest, major, minor, patch, prerelease,
                                COALESCE(NULLIF(owner_user_id, ''), 'local')
                         FROM policies WHERE urn = ?1 AND channel = 'draft'",
                        params![urn],
                        |row| {
                            Ok((
                                catalog_pin(
                                    row.get(1)?,
                                    row.get(2)?,
                                    row.get(3)?,
                                    row.get(4)?,
                                    row.get(0)?,
                                ),
                                row.get::<_, String>(5)?,
                            ))
                        },
                    )
                    .map_err(|e| {
                        if e == rusqlite::Error::QueryReturnedNoRows {
                            no_draft_error("policy", urn)
                        } else {
                            OrchestratorError::Database(e)
                        }
                    })?;
                ensure_draft_visible(
                    "policy",
                    urn,
                    CATALOG_CHANNEL_DRAFT,
                    &row_owner,
                    owner_user_id,
                    include_foreign_drafts,
                )?;
                return Ok(pin);
            }
            let version_sql = canonical_catalog_version_sql();
            return connection
                .query_row(
                    &format!("SELECT digest FROM policies WHERE urn = ?1 AND {version_sql}"),
                    params![
                        urn,
                        version.major,
                        version.minor,
                        version.patch,
                        &version.prerelease,
                    ],
                    |row| {
                        Ok(catalog_pin(
                            version.major,
                            version.minor,
                            version.patch,
                            version.prerelease.clone(),
                            row.get(0)?,
                        ))
                    },
                )
                .map_err(|e| {
                    if e == rusqlite::Error::QueryReturnedNoRows {
                        OrchestratorError::NotFound(format!("policy '{urn}' not found in catalog"))
                    } else {
                        OrchestratorError::Database(e)
                    }
                });
        }
        let (_, pin, row_owner, channel) =
            self.query_spec_at_version_with_owner(table, urn, version)?;
        ensure_draft_visible(
            catalog_table_noun(table),
            urn,
            &channel,
            &row_owner,
            owner_user_id,
            include_foreign_drafts,
        )?;
        Ok(pin)
    }

    pub fn persist_run_spec(
        &self,
        run_id: &str,
        spec_json: &str,
        session_id: Option<&str>,
        agent_urn: &str,
        idempotency_key: Option<&str>,
        owner_principal: &str,
    ) -> Result<(), OrchestratorError> {
        let owner_user_id = crate::owner::owner_user_id_for_key("", owner_principal);
        self.persist_run_spec_with_owner(
            run_id,
            spec_json,
            session_id,
            agent_urn,
            idempotency_key,
            owner_principal,
            owner_user_id,
        )
    }

    /// Same as [`Self::persist_run_spec`], stamping an explicit `owner_user_id`.
    #[allow(clippy::too_many_arguments)] // flat parameter list is the established call shape; a params struct would change callers
    pub fn persist_run_spec_with_owner(
        &self,
        run_id: &str,
        spec_json: &str,
        session_id: Option<&str>,
        agent_urn: &str,
        idempotency_key: Option<&str>,
        owner_principal: &str,
        owner_user_id: &str,
    ) -> Result<(), OrchestratorError> {
        self.persist_run_spec_with_origin(
            run_id,
            spec_json,
            session_id,
            agent_urn,
            idempotency_key,
            owner_principal,
            owner_user_id,
            None,
            None,
        )
    }

    /// [`Self::persist_run_spec_with_owner`] that also records where the run came from, in
    /// the same statement as the spec: `source` is `broker:<id>` (None for a person).
    #[allow(clippy::too_many_arguments)]
    pub fn persist_run_spec_with_origin(
        &self,
        run_id: &str,
        spec_json: &str,
        session_id: Option<&str>,
        agent_urn: &str,
        idempotency_key: Option<&str>,
        owner_principal: &str,
        owner_user_id: &str,
        source: Option<&str>,
        credential_id: Option<&str>,
    ) -> Result<(), OrchestratorError> {
        let owner = crate::owner::resolved_owner(owner_user_id);
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        // A queued run (AR-3.8) is a PENDING row with no session until its message
        // starts. Starting adopts that row: same run id, now with its session.
        if session_id.is_some() && self.queue_start_in_progress(run_id) {
            let adopted = connection.execute(
                "UPDATE runs SET state = 'RUNNING', revision = revision + 1, spec_json = ?2,
                     session_id = ?3, agent_urn = ?4, owner_principal = ?5,
                     source = ?7, credential_id = ?8
                 WHERE run_id = ?1 AND state = 'PENDING' AND session_id IS NULL
                   AND owner_user_id = ?6",
                params![
                    run_id,
                    spec_json,
                    session_id,
                    agent_urn,
                    owner_principal,
                    owner,
                    source,
                    credential_id
                ],
            )?;
            if adopted > 0 {
                return Ok(());
            }
        }
        if let Some(key) = idempotency_key {
            if let Some(existing) = connection
                .query_row(
                    "SELECT run_id FROM runs WHERE owner_user_id = ?1 AND idempotency_key = ?2",
                    params![owner, key],
                    |row| row.get::<_, String>(0),
                )
                .optional()?
            {
                if existing != run_id {
                    return Err(OrchestratorError::InvalidContract(
                        "idempotency_key already bound to a different run".to_string(),
                    ));
                }
                return Ok(());
            }
        }
        connection.execute(
            "INSERT OR IGNORE INTO runs (run_id, state, revision, spec_json, session_id, agent_urn, idempotency_key, owner_principal, owner_user_id, source, credential_id, created_at)
             VALUES (?1, 'RUNNING', 0, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![run_id, spec_json, session_id, agent_urn, idempotency_key, owner_principal, owner, source, credential_id, unix_timestamp()],
        )?;
        Ok(())
    }

    /// Where a run came from: `(source, credential_id)`. Both None for a person's run.
    pub fn run_origin(
        &self,
        run_id: &str,
    ) -> Result<(Option<String>, Option<String>), OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        connection
            .query_row(
                "SELECT source, credential_id FROM runs WHERE run_id = ?1",
                [run_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(OrchestratorError::Database)
    }

    /// Lookup is `(owner_user_id, idempotency_key)` — keys are unique per user.
    pub fn find_run_by_idempotency_key(
        &self,
        owner_user_id: &str,
        key: &str,
    ) -> Result<Option<String>, OrchestratorError> {
        let owner = crate::owner::resolved_owner(owner_user_id);
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        connection
            .query_row(
                "SELECT run_id FROM runs WHERE owner_user_id = ?1 AND idempotency_key = ?2",
                params![owner, key],
                |row| row.get(0),
            )
            .optional()
            .map_err(OrchestratorError::Database)
    }

    pub fn get_run_state(&self, run_id: &str) -> Result<String, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        connection
            .query_row(
                "SELECT state FROM runs WHERE run_id = ?1",
                [run_id],
                |row| row.get(0),
            )
            .map_err(|e| {
                if e == rusqlite::Error::QueryReturnedNoRows {
                    OrchestratorError::NotFound(format!("run '{}' not found", run_id))
                } else {
                    OrchestratorError::Database(e)
                }
            })
    }

    pub fn update_run_state(&self, run_id: &str, state: &str) -> Result<(), OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let transaction = connection.unchecked_transaction()?;
        let changed = transaction.execute(
            "UPDATE runs SET state = ?1, revision = revision + 1 WHERE run_id = ?2",
            params![state, run_id],
        )?;
        if changed == 0 {
            return Err(OrchestratorError::NotFound(format!(
                "run '{}' not found",
                run_id
            )));
        }
        crate::workflow::append_outbox(&transaction, run_id, "vam.v1.run.state", state)?;
        if crate::receipt::is_terminal(state) {
            // A run that ends leaves no decision waiting on the user.
            crate::notifications::resolve_for_run(&transaction, run_id, "run_ended")?;
        }
        transaction.commit()?;
        drop(connection);
        if crate::receipt::is_terminal(state) {
            self.queue_wake.notify_one();
        }
        Ok(())
    }

    /// Persist terminal run outputs (JSON object) and mark the run SUCCEEDED/FAILED.
    ///
    /// While the run's workspace is being settled (see `session_end`) the
    /// result is only staged: the run keeps reporting `RUNNING` until the
    /// finalizer publishes it, so no client sees `SUCCEEDED` for a run whose
    /// workspace then fails to commit.
    pub fn complete_run_with_result(
        &self,
        run_id: &str,
        state: &str,
        result_json: &str,
    ) -> Result<(), OrchestratorError> {
        // A recorded user stop wins over what the guest reports after it.
        let (state, result_json) =
            self.session_ends
                .apply_stop_precedence(run_id, state, result_json);
        if self.stage_run_result(run_id, &state, &result_json) {
            return Ok(());
        }
        self.complete_run_with_result_now(run_id, &state, &result_json)
    }

    pub(crate) fn complete_run_with_result_now(
        &self,
        run_id: &str,
        state: &str,
        result_json: &str,
    ) -> Result<(), OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let transaction = connection.unchecked_transaction()?;
        let changed = transaction.execute(
            "UPDATE runs SET state = ?1, revision = revision + 1, result_json = ?2 WHERE run_id = ?3",
            params![state, result_json, run_id],
        )?;
        if changed == 0 {
            return Err(OrchestratorError::NotFound(format!(
                "run '{}' not found",
                run_id
            )));
        }
        crate::workflow::append_outbox(&transaction, run_id, "vam.v1.run.state", state)?;
        crate::workflow::append_outbox(&transaction, run_id, "vam.v1.run.completed", result_json)?;
        transaction.commit()?;
        drop(connection);
        // The run has its end: the stop record has done its work, and a queued
        // message may start.
        self.session_ends.clear_run_stop(run_id);
        self.clear_persisted_run_stop(run_id);
        self.queue_wake.notify_one();
        Ok(())
    }

    /// Loads persisted run context: spec JSON, optional session id, and agent URN.
    #[allow(clippy::type_complexity)] // public signature used by other modules; alias would change nothing observable
    pub fn get_run_spec_context(
        &self,
        run_id: &str,
    ) -> Result<(Option<String>, Option<String>, Option<String>), OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        connection
            .query_row(
                "SELECT spec_json, session_id, agent_urn FROM runs WHERE run_id = ?1",
                [run_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .map_err(|e| {
                if e == rusqlite::Error::QueryReturnedNoRows {
                    OrchestratorError::NotFound(format!("run '{}' not found", run_id))
                } else {
                    OrchestratorError::Database(e)
                }
            })
    }

    /// Set one string field of the stored RunSpec `inputs`. Resume and a fresh
    /// guest read the inputs back from here, so a playbook bound mid-run keeps
    /// its procedure across a park.
    pub fn set_run_input_string(
        &self,
        run_id: &str,
        field: &str,
        value: &str,
    ) -> Result<(), OrchestratorError> {
        self.set_run_input_json(run_id, field, serde_json::Value::String(value.to_string()))
    }

    /// Set one field of the stored RunSpec `inputs` to any JSON value.
    pub fn set_run_input_json(
        &self,
        run_id: &str,
        field: &str,
        value: serde_json::Value,
    ) -> Result<(), OrchestratorError> {
        let (spec_json, _, _) = self.get_run_spec_context(run_id)?;
        let mut spec: serde_json::Value = serde_json::from_str(&spec_json.unwrap_or_default())
            .map_err(|e| OrchestratorError::CorruptState(format!("run spec is not JSON: {e}")))?;
        let object = spec
            .as_object_mut()
            .ok_or_else(|| OrchestratorError::CorruptState("run spec is not an object".into()))?;
        let inputs = object
            .entry("inputs")
            .or_insert_with(|| serde_json::json!({}));
        let inputs = inputs.as_object_mut().ok_or_else(|| {
            OrchestratorError::CorruptState("run inputs are not an object".into())
        })?;
        inputs.insert(field.to_string(), value);
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        connection.execute(
            "UPDATE runs SET spec_json = ?1 WHERE run_id = ?2",
            params![spec.to_string(), run_id],
        )?;
        Ok(())
    }

    pub fn get_run_result_json(&self, run_id: &str) -> Result<Option<String>, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        connection
            .query_row(
                "SELECT result_json FROM runs WHERE run_id = ?1",
                [run_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .map_err(|e| {
                if e == rusqlite::Error::QueryReturnedNoRows {
                    OrchestratorError::NotFound(format!("run '{}' not found", run_id))
                } else {
                    OrchestratorError::Database(e)
                }
            })
    }

    pub fn get_run_session_id(&self, run_id: &str) -> Result<Option<String>, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        connection
            .query_row(
                "SELECT session_id FROM runs WHERE run_id = ?1",
                [run_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(OrchestratorError::Database)
    }

    pub fn find_run_id_for_session(
        &self,
        session_id: &str,
    ) -> Result<Option<String>, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        connection
            .query_row(
                "SELECT run_id FROM runs WHERE session_id = ?1 ORDER BY rowid DESC LIMIT 1",
                [session_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(OrchestratorError::Database)
    }

    /// The run a session belongs to and that run's recorded agent, from one row
    /// (the latest run bound to the session). `None` for a session with no run.
    pub fn run_binding_for_session(
        &self,
        session_id: &str,
    ) -> Result<Option<(String, Option<String>)>, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        connection
            .query_row(
                "SELECT run_id, agent_urn FROM runs WHERE session_id = ?1 ORDER BY rowid DESC LIMIT 1",
                [session_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(OrchestratorError::Database)
    }

    pub fn find_agent_urn_for_session(
        &self,
        session_id: &str,
    ) -> Result<Option<String>, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        connection
            .query_row(
                "SELECT agent_urn FROM runs WHERE session_id = ?1 AND agent_urn IS NOT NULL ORDER BY rowid DESC LIMIT 1",
                [session_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(OrchestratorError::Database)
    }

    pub fn list_runs(&self) -> Result<Vec<(String, String)>, OrchestratorError> {
        self.list_runs_filtered(None)
    }

    /// List runs owned by `owner_user_id` (`idx_runs_owner_user_id`).
    pub fn list_runs_for_owner(
        &self,
        owner_user_id: &str,
    ) -> Result<Vec<(String, String)>, OrchestratorError> {
        self.list_runs_filtered(Some(crate::owner::resolved_owner(owner_user_id)))
    }

    fn list_runs_filtered(
        &self,
        owner_user_id: Option<&str>,
    ) -> Result<Vec<(String, String)>, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let sql = if owner_user_id.is_some() {
            "SELECT run_id, state FROM runs WHERE owner_user_id = ?1 ORDER BY rowid DESC"
        } else {
            "SELECT run_id, state FROM runs ORDER BY rowid DESC"
        };
        let mut stmt = connection.prepare(sql)?;
        match owner_user_id {
            Some(owner) => {
                let rows = stmt.query_map([owner], |row| Ok((row.get(0)?, row.get(1)?)))?;
                rows.collect::<Result<Vec<_>, _>>()
                    .map_err(OrchestratorError::Database)
            }
            None => {
                let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
                rows.collect::<Result<Vec<_>, _>>()
                    .map_err(OrchestratorError::Database)
            }
        }
    }

    /// One page of `owner_user_id`'s runs, newest first: up to `limit` rows older than the cursor
    /// `before_rowid` (the last row of the previous page). Callers ask for `limit + 1` to learn
    /// whether another page exists. Rows are `(run_id, state, rowid)`.
    pub fn list_runs_page_for_owner(
        &self,
        owner_user_id: &str,
        limit: usize,
        before_rowid: Option<i64>,
    ) -> Result<Vec<(String, String, i64)>, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let mut stmt = connection.prepare(
            "SELECT run_id, state, rowid FROM runs
             WHERE owner_user_id = ?1 AND rowid < ?2
             ORDER BY rowid DESC LIMIT ?3",
        )?;
        let rows = stmt.query_map(
            rusqlite::params![
                crate::owner::resolved_owner(owner_user_id),
                before_rowid.unwrap_or(i64::MAX),
                limit as i64
            ],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(OrchestratorError::Database)
    }

    pub fn list_pending_approvals(
        &self,
    ) -> Result<Vec<(String, String, String, String)>, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let mut stmt = connection.prepare(
            "SELECT request_id, run_id, step_id, attempt_id FROM approvals WHERE decision IS NULL ORDER BY rowid ASC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(OrchestratorError::Database)
    }

    pub fn read_outbox_for_run(
        &self,
        run_id: &str,
        after_sequence: i64,
    ) -> Result<Vec<crate::error::OutboxEvent>, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let mut statement = connection.prepare(
            "SELECT sequence, run_id, event_type, payload
             FROM workflow_outbox
             WHERE run_id = ?1 AND sequence > ?2
             ORDER BY sequence ASC",
        )?;
        let rows = statement.query_map(params![run_id, after_sequence], |row| {
            Ok(crate::error::OutboxEvent {
                sequence: row.get(0)?,
                run_id: row.get(1)?,
                event_type: row.get(2)?,
                payload: row.get(3)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(OrchestratorError::Database)
    }
}

pub(crate) fn unix_timestamp() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog_publish::ReleaseMeta;
    use crate::users::{LOCAL_USER_ID, OPERATOR_USER_ID};
    use prost::Message;
    use vam_spec::v1::{
        agent_spec::InstructionSource, PlaybookSpec, PlaybookStep, ResourceId, SkillManifest,
        StepMode, Version,
    };

    fn test_agent(urn: &str) -> AgentSpec {
        AgentSpec {
            resource_id: Some(ResourceId {
                urn: urn.to_string(),
            }),
            display_name: "Catalog owner test".to_string(),
            description: "test".to_string(),
            role: "tester".to_string(),
            instruction_source: Some(InstructionSource::SystemInstructions(
                "test instructions".to_string(),
            )),
            allow_any_playbook: true,
            ..Default::default()
        }
    }

    fn test_playbook(urn: &str) -> PlaybookSpec {
        PlaybookSpec {
            activation: Some(vam_spec::v1::PlaybookActivation::asking(
                "Use when the request matches this test playbook.",
            )),
            resource_id: Some(ResourceId {
                urn: urn.to_string(),
            }),
            display_name: "Catalog owner playbook".to_string(),
            description: "test".to_string(),
            steps: vec![PlaybookStep {
                id: "s1".to_string(),
                description: "step".to_string(),
                instruction: "Do the step.".to_string(),
                mode: StepMode::Agentic as i32,
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    fn test_skill(urn: &str) -> SkillManifest {
        SkillManifest {
            resource_id: Some(ResourceId {
                urn: urn.to_string(),
            }),
            version: Some(Version {
                major: 1,
                minor: 0,
                patch: 0,
                ..Default::default()
            }),
            description: "test skill".to_string(),
            ..Default::default()
        }
    }

    const TEST_CEDAR: &str = r#"
permit (
    principal,
    action == Action::"ExecuteTool",
    resource == Tool::"read_file"
);
"#;

    #[test]
    fn catalog_write_stamps_owner_and_list_stays_global() {
        let store = SqliteWorkflowStore::in_memory().expect("store");

        let agent_op = "urn:vam:agent:local.vam.dev/acme/op";
        let agent_loc = "urn:vam:agent:local.vam.dev/acme/loc";
        store
            .put_agent_with_owner(&test_agent(agent_op), OPERATOR_USER_ID)
            .expect("agent op");
        store
            .put_agent_with_owner(&test_agent(agent_loc), LOCAL_USER_ID)
            .expect("agent loc");
        assert_eq!(
            store.agent_owner_user_id(agent_op).expect("agent op owner"),
            OPERATOR_USER_ID
        );
        assert_eq!(
            store
                .agent_owner_user_id(agent_loc)
                .expect("agent loc owner"),
            LOCAL_USER_ID
        );
        let agents = store.list_agents().expect("list agents");
        let agent_urns: Vec<_> = agents
            .iter()
            .filter_map(|a| a.resource_id.as_ref().map(|r| r.urn.as_str()))
            .collect();
        assert!(agent_urns.contains(&agent_op));
        assert!(agent_urns.contains(&agent_loc));
        let visible_local = store
            .list_agents_visible(LOCAL_USER_ID, false)
            .expect("visible local");
        let visible_local_urns: Vec<_> = visible_local
            .iter()
            .filter_map(|a| a.resource_id.as_ref().map(|r| r.urn.as_str()))
            .collect();
        assert!(visible_local_urns.contains(&agent_op));
        assert!(visible_local_urns.contains(&agent_loc));

        let pb_op = "urn:vam:playbook:local.vam.dev/acme/op";
        let pb_loc = "urn:vam:playbook:local.vam.dev/acme/loc";
        store
            .put_playbook_with_owner(&test_playbook(pb_op), OPERATOR_USER_ID)
            .expect("playbook op");
        store
            .put_playbook_with_owner(&test_playbook(pb_loc), LOCAL_USER_ID)
            .expect("playbook loc");
        assert_eq!(
            store.playbook_owner_user_id(pb_op).expect("pb op"),
            OPERATOR_USER_ID
        );
        assert_eq!(
            store.playbook_owner_user_id(pb_loc).expect("pb loc"),
            LOCAL_USER_ID
        );

        let skill_op = "urn:vam:skill:vam.local/vam/op";
        let skill_loc = "urn:vam:skill:vam.local/vam/loc";
        store
            .put_skill_with_owner(&test_skill(skill_op), OPERATOR_USER_ID)
            .expect("skill op");
        store
            .put_skill_with_owner(&test_skill(skill_loc), LOCAL_USER_ID)
            .expect("skill loc");
        assert_eq!(
            store.skill_owner_user_id(skill_op).expect("skill op"),
            OPERATOR_USER_ID
        );
        assert_eq!(
            store.skill_owner_user_id(skill_loc).expect("skill loc"),
            LOCAL_USER_ID
        );

        let pol_op = "urn:vam:policy:local.vam.dev/acme/op";
        let pol_loc = "urn:vam:policy:local.vam.dev/acme/loc";
        store
            .put_policy_with_owner(pol_op, TEST_CEDAR, OPERATOR_USER_ID)
            .expect("policy op");
        store
            .put_policy_with_owner(pol_loc, TEST_CEDAR, LOCAL_USER_ID)
            .expect("policy loc");
        assert_eq!(
            store.policy_owner_user_id(pol_op).expect("pol op"),
            OPERATOR_USER_ID
        );
        assert_eq!(
            store.policy_owner_user_id(pol_loc).expect("pol loc"),
            LOCAL_USER_ID
        );
        let policies = store.list_policies().expect("list policies");
        let policy_urns: Vec<_> = policies
            .iter()
            .filter_map(|p| p.resource_id.as_ref().map(|r| r.urn.as_str()))
            .collect();
        assert!(policy_urns.contains(&pol_op));
        assert!(policy_urns.contains(&pol_loc));

        assert_eq!(store.count_null_owners().expect("nulls"), 0);
    }

    #[test]
    fn catalog_visibility_hides_foreign_drafts_shows_released() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        store.create_user("alice", "alice").expect("alice");
        store.create_user("bob", "bob").expect("bob");

        let alice_draft = "urn:vam:agent:local.vam.dev/acme/alice-draft";
        let bob_draft = "urn:vam:agent:local.vam.dev/acme/bob-draft";
        let bob_pub = "urn:vam:agent:local.vam.dev/acme/bob-pub";
        store
            .publish_agent_draft_with_owner(&test_agent(alice_draft), "alice")
            .expect("alice draft");
        store
            .publish_agent_draft_with_owner(&test_agent(bob_draft), "bob")
            .expect("bob draft");
        store
            .put_agent_with_owner(&test_agent(bob_pub), "bob")
            .expect("bob published");

        let listed = store.list_agents_visible("alice", false).expect("list");
        let urns: Vec<_> = listed
            .iter()
            .filter_map(|a| a.resource_id.as_ref().map(|r| r.urn.as_str()))
            .collect();
        assert!(urns.contains(&alice_draft), "{urns:?}");
        assert!(!urns.contains(&bob_draft), "{urns:?}");
        assert!(urns.contains(&bob_pub), "{urns:?}");

        store
            .get_agent_visible(alice_draft, "alice", false)
            .expect("owner sees own draft");
        let err = store
            .get_agent_visible(bob_draft, "alice", false)
            .expect_err("alice cannot see bob draft");
        assert!(matches!(err, OrchestratorError::NotFound(_)));
        store
            .get_agent_visible(bob_pub, "alice", false)
            .expect("alice sees bob released");

        let pb_alice = "urn:vam:playbook:local.vam.dev/acme/alice-draft";
        let pb_bob = "urn:vam:playbook:local.vam.dev/acme/bob-draft";
        store
            .publish_playbook_draft_with_owner(&test_playbook(pb_alice), "alice")
            .expect("alice playbook");
        store
            .publish_playbook_draft_with_owner(&test_playbook(pb_bob), "bob")
            .expect("bob playbook");
        store
            .get_playbook_visible(pb_alice, "alice")
            .expect("own playbook draft");
        assert!(matches!(
            store
                .get_playbook_visible(pb_bob, "alice")
                .expect_err("hidden playbook"),
            OrchestratorError::NotFound(_)
        ));

        let skill_alice = "urn:vam:skill:vam.local/vam/alice-draft";
        let skill_bob = "urn:vam:skill:vam.local/vam/bob-draft";
        store
            .publish_skill_draft_with_owner(&test_skill(skill_alice), "alice")
            .expect("alice skill");
        store
            .publish_skill_draft_with_owner(&test_skill(skill_bob), "bob")
            .expect("bob skill");
        store
            .get_skill_visible(skill_alice, "alice")
            .expect("own skill draft");
        assert!(matches!(
            store
                .get_skill_visible(skill_bob, "alice")
                .expect_err("hidden skill"),
            OrchestratorError::NotFound(_)
        ));

        let pol_alice = "urn:vam:policy:local.vam.dev/acme/alice-draft";
        let pol_bob = "urn:vam:policy:local.vam.dev/acme/bob-draft";
        store
            .publish_policy_draft_with_owner(pol_alice, TEST_CEDAR, "alice")
            .expect("alice policy");
        store
            .publish_policy_draft_with_owner(pol_bob, TEST_CEDAR, "bob")
            .expect("bob policy");
        let policies = store.list_policies_visible("alice").expect("policies");
        let pol_urns: Vec<_> = policies
            .iter()
            .filter_map(|p| p.resource_id.as_ref().map(|r| r.urn.as_str()))
            .collect();
        assert!(pol_urns.contains(&pol_alice), "{pol_urns:?}");
        assert!(!pol_urns.contains(&pol_bob), "{pol_urns:?}");
        store
            .get_policy_visible(pol_alice, "alice")
            .expect("own policy draft");
        assert!(matches!(
            store
                .get_policy_visible(pol_bob, "alice")
                .expect_err("hidden policy"),
            OrchestratorError::NotFound(_)
        ));
    }

    #[test]
    fn catalog_visibility_admin_lists_foreign_drafts_with_owner() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        store.create_user("alice", "alice").expect("alice");
        store.create_user("admin", "admin").expect("admin");

        let alice_draft = "urn:vam:agent:local.vam.dev/acme/alice-admin-vis";
        store
            .publish_agent_draft_with_owner(&test_agent(alice_draft), "alice")
            .expect("alice draft");

        let member_list = store
            .list_agents_visible("admin", false)
            .expect("member list");
        let member_urns: Vec<_> = member_list
            .iter()
            .filter_map(|a| a.resource_id.as_ref().map(|r| r.urn.as_str()))
            .collect();
        assert!(
            !member_urns.contains(&alice_draft),
            "member must not see foreign draft: {member_urns:?}"
        );
        assert!(matches!(
            store
                .get_agent_visible(alice_draft, "admin", false)
                .expect_err("member get"),
            OrchestratorError::NotFound(_)
        ));

        let records = store
            .list_agent_records_visible("admin", true)
            .expect("admin list");
        let alice_row = records
            .iter()
            .find(|row| {
                row.spec
                    .as_ref()
                    .and_then(|s| s.resource_id.as_ref())
                    .map(|r| r.urn.as_str())
                    == Some(alice_draft)
            })
            .expect("admin lists alice draft");
        assert_eq!(alice_row.owner_user_id, "alice");
        assert_eq!(alice_row.channel, SkillChannel::Draft as i32);
        assert_eq!(
            alice_row.catalog_tier,
            crate::catalog_tier::CATALOG_TIER_USER
        );

        let got = store
            .get_agent_record_visible(alice_draft, "admin", true)
            .expect("admin get");
        assert_eq!(got.owner_user_id, "alice");
        assert_eq!(got.channel, SkillChannel::Draft as i32);
        assert_eq!(got.catalog_tier, crate::catalog_tier::CATALOG_TIER_USER);
        assert_eq!(
            got.spec
                .as_ref()
                .and_then(|s| s.resource_id.as_ref())
                .map(|r| r.urn.as_str()),
            Some(alice_draft)
        );
    }

    fn temp_db_path(label: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!(
            "vam-catalog-{}-{}-{}.db",
            label,
            std::process::id(),
            nanos
        ))
    }

    fn cleanup_db(path: &std::path::Path) {
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(format!("{}-wal", path.display()));
        let _ = std::fs::remove_file(format!("{}-shm", path.display()));
    }

    #[test]
    fn legacy_catalog_rows_migrate_to_v1_released() {
        let path = temp_db_path("legacy-v1");
        {
            let conn = rusqlite::Connection::open(&path).expect("legacy db");
            conn.execute_batch(
                "
                CREATE TABLE agents (
                    urn TEXT PRIMARY KEY NOT NULL,
                    revision INTEGER NOT NULL,
                    digest TEXT NOT NULL,
                    spec_bytes BLOB NOT NULL,
                    created_at INTEGER NOT NULL
                );
                INSERT INTO agents VALUES ('urn:vam:agent:legacy', 3, 'dig', x'0102', 99);
                ",
            )
            .expect("seed legacy agents");
        }

        let store = SqliteWorkflowStore::open(&path).expect("migrate open");
        assert_eq!(
            store
                .catalog_channel_at_version(
                    "agents",
                    "urn:vam:agent:legacy",
                    CATALOG_RELEASED_MAJOR,
                    CATALOG_RELEASED_MINOR,
                    CATALOG_RELEASED_PATCH,
                    CATALOG_RELEASED_PRERELEASE,
                )
                .expect("channel"),
            CATALOG_CHANNEL_RELEASED
        );
        assert_eq!(
            store
                .agent_owner_user_id("urn:vam:agent:legacy")
                .expect("owner"),
            LOCAL_USER_ID
        );
        cleanup_db(&path);
    }

    #[test]
    fn draft_and_released_coexist_for_same_urn() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        let urn = "urn:vam:agent:local.vam.dev/acme/coexist";
        store
            .publish_agent_draft_with_owner(&test_agent(urn), LOCAL_USER_ID)
            .expect("draft");
        store
            .release_agent(urn, &crate::catalog_publish::catalog_release_version())
            .expect("release");
        assert_eq!(store.catalog_row_count("agents", urn).expect("count"), 2);
        assert_eq!(
            store
                .catalog_channel_at_version("agents", urn, 1, 0, 0, CATALOG_RELEASED_PRERELEASE,)
                .expect("released"),
            CATALOG_CHANNEL_RELEASED
        );
        assert_eq!(
            store
                .catalog_channel_at_version("agents", urn, 1, 0, 0, CATALOG_DRAFT_PRERELEASE)
                .expect("draft"),
            CATALOG_CHANNEL_DRAFT
        );
        store.get_agent(urn).expect("get canonical released");
    }

    #[test]
    fn duplicate_drafts_collapse_to_newest_and_migration_is_idempotent() {
        use prost::Message as _;
        let store = SqliteWorkflowStore::in_memory().expect("store");
        let urn = "urn:vam:agent:local.vam.dev/acme/dupes";
        let named = |name: &str| AgentSpec {
            resource_id: Some(ResourceId {
                urn: urn.to_string(),
            }),
            display_name: name.to_string(),
            role: "tester".to_string(),
            instruction_source: Some(InstructionSource::SystemInstructions(
                "instructions".to_string(),
            )),
            allow_any_playbook: true,
            ..Default::default()
        };
        store
            .publish_agent_draft_with_owner(&named("older"), LOCAL_USER_ID)
            .expect("draft");
        {
            // Rows that predate the index: drop it, then add a second draft.
            let conn = store.connection.lock().expect("lock");
            conn.execute_batch("DROP INDEX idx_agents_one_draft")
                .expect("drop index");
            conn.execute(
                "INSERT INTO agents (
                    urn, major, minor, patch, prerelease, channel, revision, digest,
                    spec_bytes, created_at, updated_at, owner_user_id
                 ) VALUES (?1, 2, 0, 0, 'draft', 'draft', 1, 'dig-newer', ?2, ?3, ?3, 'local')",
                params![urn, named("newer").encode_to_vec(), unix_timestamp() + 100],
            )
            .expect("second draft");
        }

        store
            .initialize_versioned_catalog_artifacts()
            .expect("migrate");
        assert_eq!(store.catalog_row_count("agents", urn).expect("count"), 1);
        assert_eq!(
            store
                .get_agent_visible(urn, LOCAL_USER_ID, false)
                .expect("draft")
                .display_name,
            "newer"
        );

        store
            .initialize_versioned_catalog_artifacts()
            .expect("second run");
        assert_eq!(store.catalog_row_count("agents", urn).expect("count"), 1);
        assert_eq!(
            store
                .get_agent_visible(urn, LOCAL_USER_ID, false)
                .expect("draft")
                .display_name,
            "newer"
        );
    }

    #[test]
    fn version_is_part_of_primary_key() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        let urn = "urn:vam:agent:local.vam.dev/acme/pk";
        store
            .put_agent_released_with_owner(&test_agent(urn), LOCAL_USER_ID)
            .expect("v1 released");
        {
            let conn = store.connection.lock().expect("lock");
            let now = unix_timestamp();
            conn.execute(
                "INSERT INTO agents (
                    urn, major, minor, patch, prerelease, channel, revision, digest,
                    spec_bytes, created_at, updated_at, owner_user_id
                 ) VALUES (?1, 1, 0, 1, '', 'released', 1, 'v101', x'01', ?2, ?2, 'local')",
                params![urn, now],
            )
            .expect("insert 1.0.1");
        }
        assert_eq!(store.catalog_row_count("agents", urn).expect("count"), 2);
    }

    #[test]
    fn get_agent_resolves_latest_released_not_newer_draft() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        let urn = "urn:vam:agent:local.vam.dev/acme/latest";
        store
            .put_agent_released_with_owner(&test_agent(urn), LOCAL_USER_ID)
            .expect("v1 released");
        let mut v101 = test_agent(urn);
        v101.display_name = "released-1.0.1".to_string();
        let mut v101_bytes = Vec::new();
        v101.encode(&mut v101_bytes).expect("encode 1.0.1");
        let mut draft2 = test_agent(urn);
        draft2.display_name = "draft-2.0.0".to_string();
        let mut draft_bytes = Vec::new();
        draft2.encode(&mut draft_bytes).expect("encode draft");
        {
            let conn = store.connection.lock().expect("lock");
            let now = unix_timestamp();
            conn.execute(
                "INSERT INTO agents (
                    urn, major, minor, patch, prerelease, channel, revision, digest,
                    spec_bytes, created_at, updated_at, owner_user_id
                 ) VALUES (?1, 1, 0, 1, '', 'released', 1, 'dig-101', ?2, ?3, ?3, 'local')",
                params![urn, v101_bytes, now],
            )
            .expect("insert 1.0.1 released");
            conn.execute(
                "INSERT INTO agents (
                    urn, major, minor, patch, prerelease, channel, revision, digest,
                    spec_bytes, created_at, updated_at, owner_user_id
                 ) VALUES (?1, 2, 0, 0, 'draft', 'draft', 1, 'dig-draft', ?2, ?3, ?3, 'local')",
                params![urn, draft_bytes, now],
            )
            .expect("insert 2.0.0 draft");
        }
        let got = store.get_agent(urn).expect("latest released");
        assert_eq!(got.display_name, "released-1.0.1");
        let pin = store
            .latest_released_catalog_pin("agents", urn)
            .expect("pin");
        assert_eq!(pin.version.major, 1);
        assert_eq!(pin.version.minor, 0);
        assert_eq!(pin.version.patch, 1);
        assert_eq!(pin.digest, "dig-101");
        let pinned = store
            .get_agent_at_version(
                urn,
                &vam_spec::v1::Version {
                    major: 1,
                    minor: 0,
                    patch: 0,
                    prerelease: String::new(),
                    build: String::new(),
                },
            )
            .expect("pinned 1.0.0");
        assert_eq!(pinned.display_name, "Catalog owner test");
    }

    #[test]
    fn reassign_catalog_owner_keeps_released_and_lets_new_owner_draft() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        store.create_user("alice", "alice").expect("alice");
        store.create_user("bob", "bob").expect("bob");
        let urn = "urn:vam:agent:local.vam.dev/acme/reassign";
        store
            .publish_agent_draft_with_owner(&test_agent(urn), "alice")
            .expect("draft");
        store
            .release_agent(urn, &crate::catalog_publish::catalog_release_version())
            .expect("release");
        assert_eq!(
            store.agent_owner_user_id(urn).expect("owner before"),
            "alice"
        );
        assert_eq!(
            store
                .catalog_channel_at_version("agents", urn, 1, 0, 0, CATALOG_RELEASED_PRERELEASE)
                .expect("released"),
            CATALOG_CHANNEL_RELEASED
        );

        let moved = store
            .reassign_catalog_owner("agent", urn, "bob")
            .expect("reassign");
        assert_eq!(moved.previous_owner_user_id, "alice");
        assert_eq!(moved.owner_user_id, "bob");
        assert_eq!(moved.rows_updated, 2);
        assert_eq!(store.agent_owner_user_id(urn).expect("owner after"), "bob");
        assert_eq!(
            store
                .catalog_channel_at_version("agents", urn, 1, 0, 0, CATALOG_RELEASED_PRERELEASE)
                .expect("still released"),
            CATALOG_CHANNEL_RELEASED
        );
        assert_eq!(
            store
                .catalog_channel_at_version("agents", urn, 1, 0, 0, CATALOG_DRAFT_PRERELEASE)
                .expect("draft remains"),
            CATALOG_CHANNEL_DRAFT
        );
        store.get_agent(urn).expect("released still resolves");

        let err = store
            .publish_agent_draft_with_owner(&test_agent(urn), "alice")
            .expect_err("old owner");
        assert!(matches!(err, OrchestratorError::PermissionDenied(_)));

        let mut next = test_agent(urn);
        next.display_name = "bob next draft".to_string();
        store
            .publish_agent_draft_with_owner(&next, "bob")
            .expect("new owner drafts");
    }

    #[test]
    fn reassign_catalog_owner_rejects_unknown_user_and_kind() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        store.create_user("alice", "alice").expect("alice");
        let urn = "urn:vam:agent:local.vam.dev/acme/reassign-miss";
        store
            .publish_agent_draft_with_owner(&test_agent(urn), "alice")
            .expect("draft");
        let missing_user = store
            .reassign_catalog_owner("agent", urn, "nobody")
            .expect_err("missing user");
        assert!(matches!(missing_user, OrchestratorError::NotFound(_)));
        let bad_kind = store
            .reassign_catalog_owner("widget", urn, "alice")
            .expect_err("kind");
        assert!(matches!(bad_kind, OrchestratorError::InvalidContract(_)));
    }

    fn version_of(prerelease: &str) -> vam_spec::v1::Version {
        vam_spec::v1::Version {
            major: 1,
            minor: 0,
            patch: 0,
            prerelease: prerelease.to_string(),
            build: String::new(),
        }
    }

    fn assert_no_draft(err: OrchestratorError, noun: &str, urn: &str) {
        assert!(
            matches!(&err, OrchestratorError::NotFound(m)
                if *m == format!("no draft of {noun} '{urn}' for this user")),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn draft_playbook_pin_is_owner_only_released_is_tenant_visible() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        store.create_user("alice", "alice").expect("alice");
        store.create_user("bob", "bob").expect("bob");
        let urn = "urn:vam:playbook:local.vam.dev/acme/owner-check";
        let draft = version_of(CATALOG_DRAFT_PRERELEASE);
        store
            .publish_playbook_draft_with_owner(&test_playbook(urn), "alice")
            .expect("alice draft");

        store
            .get_playbook_at_version(urn, &draft, "alice", false)
            .expect("owner loads own draft");
        store
            .get_playbook_at_version(urn, &draft, "bob", true)
            .expect("operator loads foreign draft");
        let err = store
            .get_playbook_at_version(urn, &draft, "bob", false)
            .expect_err("foreign draft");
        assert_no_draft(err, "playbook", urn);
        let err = store
            .catalog_pin_at_version("playbooks", urn, &draft, "bob", false)
            .expect_err("foreign draft pin");
        assert_no_draft(err, "playbook", urn);
        store
            .catalog_pin_at_version("playbooks", urn, &draft, "alice", false)
            .expect("owner pin");
        store
            .catalog_pin_at_version("playbooks", urn, &draft, "bob", true)
            .expect("operator pin");

        store
            .release_playbook(urn, &crate::catalog_publish::catalog_release_version())
            .expect("release");
        let released = version_of(CATALOG_RELEASED_PRERELEASE);
        store
            .get_playbook_at_version(urn, &released, "bob", false)
            .expect("released visible to bob");
        store
            .catalog_pin_at_version("playbooks", urn, &released, "bob", false)
            .expect("released pin visible to bob");
    }

    #[test]
    fn draft_policy_pin_is_owner_only_released_is_tenant_visible() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        store.create_user("alice", "alice").expect("alice");
        store.create_user("bob", "bob").expect("bob");
        let urn = "urn:vam:policy:local.vam.dev/acme/owner-check";
        let draft = version_of(CATALOG_DRAFT_PRERELEASE);
        store
            .publish_policy_draft_with_owner(urn, "permit(principal, action, resource);", "alice")
            .expect("alice draft");

        store
            .catalog_pin_at_version("policies", urn, &draft, "alice", false)
            .expect("owner pin");
        store
            .catalog_pin_at_version("policies", urn, &draft, "bob", true)
            .expect("operator pin");
        let err = store
            .catalog_pin_at_version("policies", urn, &draft, "bob", false)
            .expect_err("foreign draft");
        assert_no_draft(err, "policy", urn);

        store
            .release_policy(urn, &crate::catalog_publish::catalog_release_version())
            .expect("release");
        store
            .catalog_pin_at_version(
                "policies",
                urn,
                &version_of(CATALOG_RELEASED_PRERELEASE),
                "bob",
                false,
            )
            .expect("released pin visible to bob");
    }

    fn note_agent(urn: &str, tag: &str) -> AgentSpec {
        AgentSpec {
            resource_id: Some(ResourceId {
                urn: urn.to_string(),
            }),
            display_name: "Release notes".to_string(),
            description: tag.to_string(),
            role: "tester".to_string(),
            instruction_source: Some(InstructionSource::SystemInstructions(format!("do {tag}"))),
            allow_any_playbook: true,
            ..Default::default()
        }
    }

    fn semver(major: i32, minor: i32, patch: i32) -> Version {
        Version {
            major,
            minor,
            patch,
            prerelease: String::new(),
            build: String::new(),
        }
    }

    fn versions_store() -> SqliteWorkflowStore {
        let store = SqliteWorkflowStore::in_memory().expect("in-memory store");
        store.initialize_catalog_schema().expect("catalog schema");
        store.create_user("alice", "alice").expect("alice");
        store.create_user("bob", "bob").expect("bob");
        store
    }

    #[test]
    fn versions_list_newest_first_with_notes_and_releasers() {
        let store = versions_store();
        let urn = "urn:vam:agent:local.vam.dev/acme/versions";
        store
            .publish_agent_draft_with_owner(&note_agent(urn, "one"), "alice")
            .expect("draft one");
        store
            .release_agent_with_meta(
                urn,
                &semver(1, 0, 0),
                &ReleaseMeta {
                    note: "  first  ",
                    released_by: None,
                },
            )
            .expect("release 1.0.0");
        store
            .publish_agent_draft_with_owner(&note_agent(urn, "two"), "alice")
            .expect("draft two");
        store
            .release_agent_with_meta(
                urn,
                &semver(1, 1, 0),
                &ReleaseMeta {
                    note: "adds jira",
                    released_by: Some("alice"),
                },
            )
            .expect("release 1.1.0");
        store
            .publish_agent_draft_with_owner(&note_agent(urn, "three"), "alice")
            .expect("draft three");
        store
            .release_agent_with_meta(
                urn,
                &semver(1, 2, 0),
                &ReleaseMeta {
                    note: "ops fix",
                    released_by: Some(OPERATOR_USER_ID),
                },
            )
            .expect("operator releases 1.2.0");

        let alice = store
            .list_catalog_versions("agents", urn, "alice", false)
            .expect("alice sees history");
        assert_eq!(alice.len(), 4);
        assert_eq!(alice[0].channel, SkillChannel::Draft);
        assert_eq!(alice[0].version.prerelease, "draft");
        assert!(alice[0].note.is_empty());
        assert_eq!((alice[1].version.minor, alice[1].version.patch), (2, 0));
        assert_eq!(alice[1].released_by, OPERATOR_USER_ID);
        assert_eq!(alice[1].note, "ops fix");
        assert_eq!((alice[2].version.minor, alice[2].version.patch), (1, 0));
        assert_eq!(alice[2].released_by, "alice");
        assert_eq!(alice[2].note, "adds jira");
        assert_eq!(alice[3].note, "first");
        assert_eq!(alice[3].released_by, "alice");

        let bob = store
            .list_catalog_versions("agents", urn, "bob", false)
            .expect("bob sees releases");
        assert_eq!(bob.len(), 3);
        assert!(bob.iter().all(|v| v.channel == SkillChannel::Released));

        let admin = store
            .list_catalog_versions("agents", urn, "bob", true)
            .expect("admin sees the foreign draft");
        assert_eq!(admin.len(), 4);
        assert_eq!(admin[0].channel, SkillChannel::Draft);
    }

    #[test]
    fn versions_for_a_urn_with_nothing_visible_is_not_found() {
        let store = versions_store();
        let urn = "urn:vam:agent:local.vam.dev/acme/only-draft";
        store
            .publish_agent_draft_with_owner(&note_agent(urn, "draft"), "alice")
            .expect("draft");
        let err = store
            .list_catalog_versions("agents", urn, "bob", false)
            .expect_err("bob sees no draft and no release");
        assert!(matches!(err, OrchestratorError::NotFound(_)), "{err:?}");
    }

    #[test]
    fn same_digest_rerelease_keeps_the_first_note_and_releaser() {
        let store = versions_store();
        let urn = "urn:vam:agent:local.vam.dev/acme/same-digest";
        store
            .publish_agent_draft_with_owner(&note_agent(urn, "one"), "alice")
            .expect("draft");
        store
            .release_agent_with_meta(
                urn,
                &semver(1, 0, 0),
                &ReleaseMeta {
                    note: "first",
                    released_by: Some("alice"),
                },
            )
            .expect("release");
        store
            .release_agent_with_meta(
                urn,
                &semver(1, 0, 0),
                &ReleaseMeta {
                    note: "second try",
                    released_by: Some(OPERATOR_USER_ID),
                },
            )
            .expect("same digest is a no-op");
        let history = store
            .list_catalog_versions("agents", urn, "alice", false)
            .expect("history");
        let released: Vec<_> = history
            .iter()
            .filter(|v| v.channel == SkillChannel::Released)
            .collect();
        assert_eq!(released.len(), 1);
        assert_eq!(released[0].note, "first");
        assert_eq!(released[0].released_by, "alice");
    }

    #[test]
    fn release_note_over_500_characters_is_rejected() {
        let store = versions_store();
        let urn = "urn:vam:agent:local.vam.dev/acme/long-note";
        store
            .publish_agent_draft_with_owner(&note_agent(urn, "one"), "alice")
            .expect("draft");
        let too_long = "x".repeat(501);
        let err = store
            .release_agent_with_meta(
                urn,
                &semver(1, 0, 0),
                &ReleaseMeta {
                    note: &too_long,
                    released_by: None,
                },
            )
            .expect_err("501 characters");
        assert!(
            matches!(err, OrchestratorError::InvalidContract(_)),
            "{err:?}"
        );

        let at_limit = "x".repeat(500);
        store
            .release_agent_with_meta(
                urn,
                &semver(1, 0, 0),
                &ReleaseMeta {
                    note: &at_limit,
                    released_by: None,
                },
            )
            .expect("500 characters is accepted");
    }

    #[test]
    fn legacy_released_rows_migrate_with_empty_note_and_releaser() {
        let store = versions_store();
        let urn = "urn:vam:agent:local.vam.dev/acme/legacy";
        store
            .publish_agent_draft_with_owner(&note_agent(urn, "one"), "alice")
            .expect("draft");
        store
            .release_agent(urn, &semver(1, 0, 0))
            .expect("release without a note");

        // Drop the new columns, as an upgrade from a database before this change would have them.
        {
            let connection = store.connection.lock().expect("connection");
            connection
                .execute_batch(
                    "ALTER TABLE agents DROP COLUMN release_note;
                     ALTER TABLE agents DROP COLUMN released_by;",
                )
                .expect("drop columns");
        }
        store
            .initialize_versioned_catalog_artifacts()
            .expect("migration re-adds the columns");

        let history = store
            .list_catalog_versions("agents", urn, "alice", false)
            .expect("legacy rows still list");
        assert_eq!(history.len(), 2);
        assert_eq!(history[1].version.major, 1);
        assert!(history[1].note.is_empty());
        assert!(history[1].released_by.is_empty());
    }

    #[test]
    fn versions_cover_playbook_and_policy() {
        let store = versions_store();

        let playbook_urn = "urn:vam:playbook:local.vam.dev/acme/versions-pb";
        store
            .publish_playbook_draft_with_owner(&test_playbook(playbook_urn), "alice")
            .expect("playbook draft");
        store
            .release_playbook_with_meta(
                playbook_urn,
                &semver(1, 0, 0),
                &ReleaseMeta {
                    note: "playbook first",
                    released_by: None,
                },
            )
            .expect("playbook release");
        let playbook = store
            .list_catalog_versions("playbooks", playbook_urn, "alice", false)
            .expect("playbook history");
        assert_eq!(playbook.len(), 2);
        assert_eq!(playbook[1].note, "playbook first");
        assert_eq!(playbook[1].released_by, "alice");

        let policy_urn = "urn:vam:policy:local.vam.dev/acme/versions-policy";
        store
            .publish_policy_draft_with_owner(
                policy_urn,
                r#"permit(principal, action, resource);"#,
                "alice",
            )
            .expect("policy draft");
        store
            .release_policy_with_meta(
                policy_urn,
                &semver(1, 0, 0),
                &ReleaseMeta {
                    note: "policy first",
                    released_by: Some("alice"),
                },
            )
            .expect("policy release");
        let policy = store
            .list_catalog_versions("policies", policy_urn, "bob", false)
            .expect("bob sees the released policy history");
        assert_eq!(policy.len(), 1);
        assert_eq!(policy[0].note, "policy first");
        assert_eq!(policy[0].released_by, "alice");
    }
}
