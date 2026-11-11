//! What marketplaces installed: the install record of each listing and the origin of each
//! installed catalog row (CN-4.5).
//!
//! - `node_marketplace_installs`: one row per `(marketplace, listing)` that is installed, with the
//!   tag and commit it is installed at and the artifacts it wrote (URN, version, digest).
//! - `catalog_origins`: one row per installed release (kind, URN, version): the marketplace, the
//!   listing, the tag, the commit and the package digest. A catalog row is told to be installed by
//!   its tier (`marketplace`); this table says from where.
//!
//! Only the import ([`super::import`]) writes either table.

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::error::OrchestratorError;
use crate::workflow::SqliteWorkflowStore;
use vam_spec::v1::CatalogOrigin;

/// The owner id of the rows a marketplace installed: `marketplace:<id>`. It is a deactivated
/// system user (no password, no key, no session, hidden from the user list); the catalog's owner
/// column is a foreign key to `users`.
pub fn owner_of(marketplace_id: &str) -> String {
    format!("marketplace:{marketplace_id}")
}

/// One artifact an install wrote.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstalledArtifact {
    pub kind: String,
    pub urn: String,
    /// The release the install wrote or found: `X.Y.Z`.
    pub version: String,
    /// The digest recorded as the row's package digest.
    pub digest: String,
    /// SHA-256 of the artifact's capability summary at install. An update compares it with the
    /// new one to tell which artifacts now do something else. Empty on installs made before it.
    #[serde(default)]
    pub summary_digest: String,
}

/// An installed listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallRow {
    pub marketplace_id: String,
    pub listing_id: String,
    pub git_ref: String,
    pub commit: String,
    pub installed_at: i64,
    pub installed_by: String,
    pub artifacts: Vec<InstalledArtifact>,
}

/// The `origin` of one installed release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OriginRow {
    pub marketplace: String,
    pub listing: String,
    pub git_ref: String,
    pub commit: String,
    pub package_digest: String,
}

impl OriginRow {
    pub fn to_proto(&self) -> CatalogOrigin {
        CatalogOrigin {
            marketplace: self.marketplace.clone(),
            listing: self.listing.clone(),
            r#ref: self.git_ref.clone(),
            commit: self.commit.clone(),
            package_digest: self.package_digest.clone(),
        }
    }
}

/// The key of a release in `catalog_origins`: URN and `(major, minor, patch)`.
pub type OriginKey = (String, i32, i32, i32);

fn install_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<InstallRow> {
    let artifacts: String = r.get(6)?;
    Ok(InstallRow {
        marketplace_id: r.get(0)?,
        listing_id: r.get(1)?,
        git_ref: r.get(2)?,
        commit: r.get(3)?,
        installed_at: r.get(4)?,
        installed_by: r.get(5)?,
        artifacts: serde_json::from_str(&artifacts).unwrap_or_default(),
    })
}

const INSTALL_COLUMNS: &str =
    "marketplace_id, listing_id, git_ref, commit_sha, installed_at, installed_by, artifacts_json";

/// Writes the install record of a listing (replacing the previous one). Called by the import
/// inside its transaction.
pub(crate) fn write_install(conn: &Connection, row: &InstallRow) -> Result<(), OrchestratorError> {
    let artifacts = serde_json::to_string(&row.artifacts)
        .map_err(|e| OrchestratorError::Core(e.to_string()))?;
    conn.execute(
        "INSERT OR REPLACE INTO node_marketplace_installs
             (marketplace_id, listing_id, git_ref, commit_sha, installed_at, installed_by, artifacts_json)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            row.marketplace_id,
            row.listing_id,
            row.git_ref,
            row.commit,
            row.installed_at,
            row.installed_by,
            artifacts
        ],
    )?;
    Ok(())
}

/// Writes the origin of one installed release. Called by the import inside its transaction.
pub(crate) fn write_origin(
    conn: &Connection,
    kind: &str,
    urn: &str,
    version: (i32, i32, i32),
    origin: &OriginRow,
    now: i64,
) -> Result<(), OrchestratorError> {
    conn.execute(
        "INSERT OR REPLACE INTO catalog_origins
             (kind, urn, major, minor, patch, marketplace_id, listing_id, git_ref, commit_sha,
              package_digest, installed_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            kind,
            urn,
            version.0,
            version.1,
            version.2,
            origin.marketplace,
            origin.listing,
            origin.git_ref,
            origin.commit,
            origin.package_digest,
            now
        ],
    )?;
    Ok(())
}

/// The origins of every installed release of `kind`, by `(urn, major, minor, patch)`.
pub(crate) fn origins_of_kind(
    conn: &Connection,
    kind: &str,
) -> Result<HashMap<OriginKey, OriginRow>, OrchestratorError> {
    let mut stmt = conn.prepare(
        "SELECT urn, major, minor, patch, marketplace_id, listing_id, git_ref, commit_sha, package_digest
         FROM catalog_origins WHERE kind = ?1",
    )?;
    let rows = stmt.query_map([kind], |r| {
        Ok((
            (r.get::<_, String>(0)?, r.get(1)?, r.get(2)?, r.get(3)?),
            OriginRow {
                marketplace: r.get(4)?,
                listing: r.get(5)?,
                git_ref: r.get(6)?,
                commit: r.get(7)?,
                package_digest: r.get(8)?,
            },
        ))
    })?;
    let mut map = HashMap::new();
    for row in rows {
        let (key, origin) = row?;
        map.insert(key, origin);
    }
    Ok(map)
}

impl SqliteWorkflowStore {
    /// Every installed listing of one marketplace.
    pub fn marketplace_installs(
        &self,
        marketplace_id: &str,
    ) -> Result<Vec<InstallRow>, OrchestratorError> {
        let c = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let mut stmt = c.prepare(&format!(
            "SELECT {INSTALL_COLUMNS} FROM node_marketplace_installs
             WHERE marketplace_id = ?1 ORDER BY listing_id"
        ))?;
        let rows = stmt
            .query_map([marketplace_id], install_from_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn marketplace_install(
        &self,
        marketplace_id: &str,
        listing_id: &str,
    ) -> Result<Option<InstallRow>, OrchestratorError> {
        let c = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        Ok(c.query_row(
            &format!(
                "SELECT {INSTALL_COLUMNS} FROM node_marketplace_installs
                 WHERE marketplace_id = ?1 AND listing_id = ?2"
            ),
            params![marketplace_id, listing_id],
            install_from_row,
        )
        .optional()?)
    }

    /// The `publisherDomain` of every trusted marketplace (enabled or not), for resolving the
    /// rows they installed.
    pub fn marketplace_publisher_domains(&self) -> Result<Vec<String>, OrchestratorError> {
        let c = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let mut stmt = c.prepare("SELECT publisher_domain FROM node_marketplaces")?;
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Fills `origin` on the agent records that a marketplace installed.
    pub fn attach_agent_origins(
        &self,
        records: &mut [vam_spec::v1::AgentRecord],
    ) -> Result<(), OrchestratorError> {
        let map = self.origins_for(
            "agent",
            records.iter().any(|r| is_installed(&r.catalog_tier)),
        )?;
        for r in records.iter_mut().filter(|r| is_installed(&r.catalog_tier)) {
            if let Some(o) = lookup(
                &map,
                r.spec.as_ref().and_then(|s| s.resource_id.as_ref()),
                r.version.as_ref(),
            ) {
                r.origin = Some(o.to_proto());
            }
        }
        Ok(())
    }

    /// Fills `origin` on the playbook records that a marketplace installed.
    pub fn attach_playbook_origins(
        &self,
        records: &mut [vam_spec::v1::PlaybookRecord],
    ) -> Result<(), OrchestratorError> {
        let map = self.origins_for(
            "playbook",
            records.iter().any(|r| is_installed(&r.catalog_tier)),
        )?;
        for r in records.iter_mut().filter(|r| is_installed(&r.catalog_tier)) {
            if let Some(o) = lookup(
                &map,
                r.spec.as_ref().and_then(|s| s.resource_id.as_ref()),
                r.version.as_ref(),
            ) {
                r.origin = Some(o.to_proto());
            }
        }
        Ok(())
    }

    /// Fills `origin` on the skill package records that a marketplace installed.
    pub fn attach_skill_package_origins(
        &self,
        records: &mut [vam_spec::v1::SkillPackageRecord],
    ) -> Result<(), OrchestratorError> {
        let map = self.origins_for(
            "skill",
            records.iter().any(|r| is_installed(&r.catalog_tier)),
        )?;
        for r in records.iter_mut().filter(|r| is_installed(&r.catalog_tier)) {
            if let Some(o) = lookup(&map, r.resource_id.as_ref(), r.version.as_ref()) {
                r.origin = Some(o.to_proto());
            }
        }
        Ok(())
    }

    fn origins_for(
        &self,
        kind: &str,
        any_installed: bool,
    ) -> Result<HashMap<OriginKey, OriginRow>, OrchestratorError> {
        if !any_installed {
            return Ok(HashMap::new());
        }
        let c = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        origins_of_kind(&c, kind)
    }

    /// Test-only seam (process-guest builds): forgets every install and returns the node's
    /// marketplace state to its boot state, so scenarios give the same result alone, in a batch
    /// and in any order. It deletes every catalog row of tier `marketplace` (and the origin and
    /// install records), the read state of the preconfigured marketplace, and re-enables it. It
    /// does not touch the pins or the cache, and it never runs in a default `vamd`.
    #[cfg(feature = "process-guest")]
    pub fn reset_marketplace_installs(&self) -> Result<usize, OrchestratorError> {
        let mut c = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let tx = c.transaction()?;
        let mut removed = 0usize;
        for table in [
            "agents",
            "playbooks",
            "skills",
            "policies",
            "skill_packages",
        ] {
            removed += tx.execute(
                &format!("DELETE FROM {table} WHERE catalog_tier = ?1"),
                [crate::catalog_tier::CATALOG_TIER_MARKETPLACE],
            )?;
        }
        removed += tx.execute("DELETE FROM connector_releases", [])?;
        // A connector that was installed is gone, and so is what the admin decided about it.
        let builtin: Vec<String> = vam_spec::connector::builtin_connectors()
            .iter()
            .map(crate::connectors::connector_id)
            .collect();
        let mut stale: Vec<String> = Vec::new();
        {
            let mut stmt = tx.prepare("SELECT connector_id FROM node_enabled_connectors")?;
            for id in stmt.query_map([], |r| r.get::<_, String>(0))? {
                let id = id?;
                if !builtin.contains(&id) {
                    stale.push(id);
                }
            }
        }
        for id in stale {
            tx.execute(
                "DELETE FROM node_enabled_connectors WHERE connector_id = ?1",
                [id],
            )?;
        }
        tx.execute("DELETE FROM catalog_origins", [])?;
        tx.execute("DELETE FROM node_marketplace_installs", [])?;
        tx.execute(
            "UPDATE node_marketplaces SET enabled = 1, last_checked_at = NULL, last_error = NULL,
                 index_digest = NULL, head_commit = NULL, listings_json = '[]', tags_seen_json = '[]'
             WHERE preconfigured = 1",
            [],
        )?;
        tx.commit()?;
        Ok(removed)
    }

    /// Creates the deactivated system user that owns the rows `marketplace_id` installs. Called
    /// by the import inside its transaction scope.
    pub(crate) fn ensure_marketplace_owner(
        conn: &Connection,
        marketplace_id: &str,
        now: i64,
    ) -> Result<String, OrchestratorError> {
        let owner = owner_of(marketplace_id);
        conn.execute(
            "INSERT OR IGNORE INTO users (user_id, username, tenant_id, created_at, role, deactivated_at)
             VALUES (?1, ?1, ?1, ?2, 'member', ?2)",
            params![owner, now],
        )?;
        Ok(owner)
    }
}

fn is_installed(tier: &str) -> bool {
    tier == crate::catalog_tier::CATALOG_TIER_MARKETPLACE
}

fn lookup<'a>(
    map: &'a HashMap<OriginKey, OriginRow>,
    id: Option<&vam_spec::v1::ResourceId>,
    version: Option<&vam_spec::v1::Version>,
) -> Option<&'a OriginRow> {
    let (id, v) = (id?, version?);
    map.get(&(id.urn.clone(), v.major, v.minor, v.patch))
}
