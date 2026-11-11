use std::path::{Path, PathBuf};

use prost::Message;
use prost_types::Timestamp;
use rusqlite::{params, OptionalExtension};

use crate::catalog_publish::{
    ensure_release_is_newer, latest_released_triple, normalize_release_note,
    release_no_draft_error, released_digest_conflict, unchanged_since_release_conflict,
    ReleaseMeta,
};
use crate::error::OrchestratorError;
use crate::workflow::SqliteWorkflowStore;
use vam_spec::v1::{
    ResourceId, ResourceMetadata, SkillChannel, SkillManifest, SkillPackageRecord, Version,
};
use vam_spec::validation::Validate;

const SKILL_PACKAGE_SELECT_SQL: &str =
    "SELECT urn, major, minor, patch, prerelease, channel, revision, digest,
        blob_relpath, size_bytes, spec_bytes, created_at, updated_at,
        COALESCE(NULLIF(owner_user_id, ''), 'local'),
        COALESCE(NULLIF(catalog_tier, ''), 'user')";

/// Digest-only filename shared by every owner (`sha256-{digest}.tar.gz`).
pub fn skill_blob_filename(digest: &str) -> String {
    format!("sha256-{digest}.tar.gz")
}

/// Owner-namespaced catalog relative path: `{owner}/sha256-{digest}.tar.gz`.
///
/// Empty `owner_user_id` resolves to `local`. The same digest is a distinct
/// on-disk path per owner so two users cannot collide or overwrite.
pub fn skill_blob_relpath(owner_user_id: &str, digest: &str) -> String {
    let owner = crate::owner::resolved_owner(owner_user_id);
    format!("{owner}/{}", skill_blob_filename(digest))
}

/// True when `blob_relpath` is already `{owner}/sha256-….tar.gz`.
pub fn is_owner_namespaced_blob_relpath(blob_relpath: &str) -> bool {
    let Some((owner, file)) = blob_relpath.split_once('/') else {
        return false;
    };
    !owner.is_empty()
        && !owner.contains('/')
        && !file.contains('/')
        && file.starts_with("sha256-")
        && file.ends_with(".tar.gz")
}

pub fn skill_catalog_root(drive9_root: &Path) -> PathBuf {
    drive9_root.join("catalog/skills")
}

impl SqliteWorkflowStore {
    pub fn put_skill_package(
        &self,
        package: &SkillPackageRecord,
    ) -> Result<SkillPackageRecord, OrchestratorError> {
        self.put_skill_package_with_owner(package, crate::users::LOCAL_USER_ID)
    }

    pub fn put_skill_package_with_owner(
        &self,
        package: &SkillPackageRecord,
        owner_user_id: &str,
    ) -> Result<SkillPackageRecord, OrchestratorError> {
        package
            .validate()
            .map_err(|e| OrchestratorError::InvalidContract(e.to_string()))?;

        let urn = package
            .resource_id
            .as_ref()
            .map(|resource_id| resource_id.urn.as_str())
            .filter(|urn| !urn.is_empty())
            .ok_or_else(|| {
                OrchestratorError::InvalidContract(
                    "SkillPackageRecord.resource_id is required".to_string(),
                )
            })?;
        crate::catalog_tier::reject_caller_platform_urn(urn, "skill package")?;
        let version = package.version.as_ref().ok_or_else(|| {
            OrchestratorError::InvalidContract("SkillPackageRecord.version is required".to_string())
        })?;
        let channel = skill_channel_from_i32(package.channel)?;
        let channel_db = channel_db_value(channel);
        let owner = crate::owner::resolved_owner(owner_user_id);
        let blob_relpath = skill_blob_relpath(owner, &package.digest);

        let mut spec_bytes = Vec::new();
        let spec = package.spec.as_ref().ok_or_else(|| {
            OrchestratorError::InvalidContract("SkillPackageRecord.spec is required".to_string())
        })?;
        spec.encode(&mut spec_bytes)
            .map_err(|e| OrchestratorError::Core(e.to_string()))?;

        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;

        crate::catalog_tier::reject_caller_installed_urn(&connection, urn, "skill package")?;
        crate::owner::require_matching_urn_owner(
            &connection,
            "skill_packages",
            urn,
            owner,
            "skill package",
        )?;

        if channel_db == channel_db_value(SkillChannel::Draft) {
            return put_skill_package_draft(
                &connection,
                urn,
                version,
                package,
                &spec_bytes,
                &blob_relpath,
                owner,
            );
        }

        let existing = fetch_skill_package_row(
            &connection,
            urn,
            version.major,
            version.minor,
            version.patch,
            &version.prerelease,
        )?;

        if let Some(row) = existing {
            if row.digest == package.digest {
                return row_to_skill_package_record(row);
            }
            if channel == SkillChannel::Released {
                return Err(OrchestratorError::Conflict(format!(
                    "released skill package '{urn}' {}.{}.{} already exists with a different digest",
                    version.major, version.minor, version.patch
                )));
            }

            let now = unix_timestamp();
            connection.execute(
                "UPDATE skill_packages
                 SET revision = revision + 1,
                     digest = ?1,
                     blob_relpath = ?2,
                     size_bytes = ?3,
                     spec_bytes = ?4,
                     updated_at = ?5
                 WHERE urn = ?6 AND major = ?7 AND minor = ?8 AND patch = ?9 AND prerelease = ?10",
                params![
                    package.digest,
                    blob_relpath,
                    package.size_bytes,
                    spec_bytes,
                    now,
                    urn,
                    version.major,
                    version.minor,
                    version.patch,
                    version.prerelease,
                ],
            )?;
            let updated = fetch_skill_package_row(
                &connection,
                urn,
                version.major,
                version.minor,
                version.patch,
                &version.prerelease,
            )?
            .ok_or_else(|| {
                OrchestratorError::CorruptState(format!(
                    "skill package '{urn}' disappeared after draft replace"
                ))
            })?;
            return row_to_skill_package_record(updated);
        }

        let now = unix_timestamp();
        connection.execute(
            "INSERT INTO skill_packages (
                urn, major, minor, patch, prerelease, channel, revision, digest,
                blob_relpath, size_bytes, spec_bytes, created_at, updated_at, owner_user_id
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1, ?7, ?8, ?9, ?10, ?11, ?11, ?12)",
            params![
                urn,
                version.major,
                version.minor,
                version.patch,
                version.prerelease,
                channel_db,
                package.digest,
                blob_relpath,
                package.size_bytes,
                spec_bytes,
                now,
                owner,
            ],
        )?;

        let inserted = fetch_skill_package_row(
            &connection,
            urn,
            version.major,
            version.minor,
            version.patch,
            &version.prerelease,
        )?
        .ok_or_else(|| {
            OrchestratorError::CorruptState(format!(
                "skill package '{urn}' missing immediately after insert"
            ))
        })?;
        row_to_skill_package_record(inserted)
    }

    /// A skill's manifest for permission views and migration: the catalog `skills` table
    /// first (JSON `put`), then the published package (`vam skill publish`), which is where
    /// real skills live. `version` is the pinned version; `None` takes the owner's draft,
    /// else the latest release.
    pub fn get_skill_manifest_any(
        &self,
        urn: &str,
        version: Option<&Version>,
        owner_user_id: &str,
        include_foreign_drafts: bool,
    ) -> Result<SkillManifest, OrchestratorError> {
        let from_catalog = match version {
            Some(version) => {
                self.get_skill_at_version(urn, version, owner_user_id, include_foreign_drafts)
            }
            None => self.get_skill_visible(urn, owner_user_id),
        };
        match from_catalog {
            Err(OrchestratorError::NotFound(_)) => {}
            other => return other,
        }
        let draft = |this: &Self| {
            this.get_skill_package_for_query_visible(
                urn,
                None,
                SkillChannel::Draft,
                owner_user_id,
                include_foreign_drafts,
            )
        };
        let record = match version {
            Some(version) if version.prerelease == "draft" => draft(self)?,
            Some(version) => self.get_skill_package(urn, version)?,
            None => match draft(self) {
                Err(OrchestratorError::NotFound(_)) => self.get_skill_package_for_query_visible(
                    urn,
                    None,
                    SkillChannel::Released,
                    owner_user_id,
                    include_foreign_drafts,
                )?,
                other => other?,
            },
        };
        record.spec.ok_or_else(|| {
            OrchestratorError::CorruptState(format!("skill package '{urn}' has no manifest"))
        })
    }

    pub fn get_skill_package(
        &self,
        urn: &str,
        version: &Version,
    ) -> Result<SkillPackageRecord, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let row = fetch_skill_package_row(
            &connection,
            urn,
            version.major,
            version.minor,
            version.patch,
            &version.prerelease,
        )?
        .ok_or_else(|| {
            OrchestratorError::NotFound(format!(
                "skill package '{urn}' {}.{}.{}{} not found",
                version.major,
                version.minor,
                version.patch,
                format_prerelease_suffix(&version.prerelease)
            ))
        })?;
        row_to_skill_package_record(row)
    }

    pub fn get_skill_package_for_query(
        &self,
        urn: &str,
        version: Option<&Version>,
        channel: SkillChannel,
    ) -> Result<SkillPackageRecord, OrchestratorError> {
        self.get_skill_package_for_query_filtered(urn, version, channel, None, true)
    }

    /// Draft rows are owner-only unless `include_foreign_drafts`. Released rows are tenant-visible.
    pub fn get_skill_package_for_query_visible(
        &self,
        urn: &str,
        version: Option<&Version>,
        channel: SkillChannel,
        owner_user_id: &str,
        include_foreign_drafts: bool,
    ) -> Result<SkillPackageRecord, OrchestratorError> {
        self.get_skill_package_for_query_filtered(
            urn,
            version,
            channel,
            Some(crate::owner::resolved_owner(owner_user_id)),
            include_foreign_drafts,
        )
    }

    fn get_skill_package_for_query_filtered(
        &self,
        urn: &str,
        version: Option<&Version>,
        channel: SkillChannel,
        caller: Option<&str>,
        include_foreign_drafts: bool,
    ) -> Result<SkillPackageRecord, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;

        // A draft ignores its version: the owner's one draft for the URN.
        if let Some(version) = version.filter(|_| channel_db_value(channel) == "released") {
            let row = fetch_skill_package_row(
                &connection,
                urn,
                version.major,
                version.minor,
                version.patch,
                "",
            )?;
            let Some(row) = row else {
                let available = released_versions(&connection, urn)?;
                return Err(OrchestratorError::NotFound(format!(
                    "skill package '{urn}' {}.{}.{} has no release; released versions: {}",
                    version.major,
                    version.minor,
                    version.patch,
                    if available.is_empty() {
                        "none".to_string()
                    } else {
                        available.join(", ")
                    }
                )));
            };
            return row_to_skill_package_record(row);
        }

        let channel_db = channel_db_value(channel);
        let row = connection
            .query_row(
                &format!(
                    "{SKILL_PACKAGE_SELECT_SQL}
                 FROM skill_packages
                 WHERE urn = ?1 AND channel = ?2
                 ORDER BY major DESC, minor DESC, patch DESC
                 LIMIT 1"
                ),
                params![urn, channel_db],
                read_skill_package_row,
            )
            .optional()
            .map_err(OrchestratorError::Database)?
            .ok_or_else(|| {
                if channel_db == channel_db_value(SkillChannel::Draft) {
                    crate::catalog::no_draft_error("skill", urn)
                } else {
                    OrchestratorError::NotFound(format!(
                        "skill package '{urn}' not found for channel '{channel_db}'"
                    ))
                }
            })?;
        row_to_skill_package_record(skill_package_row_if_visible(
            row,
            urn,
            caller,
            include_foreign_drafts,
        )?)
    }

    pub fn list_skill_packages(&self) -> Result<Vec<SkillPackageRecord>, OrchestratorError> {
        self.list_skill_packages_filtered(None)
    }

    /// Own drafts plus every tenant **released** row. Foreign drafts are omitted
    /// unless `include_foreign_drafts`.
    pub fn list_skill_packages_visible(
        &self,
        owner_user_id: &str,
        include_foreign_drafts: bool,
    ) -> Result<Vec<SkillPackageRecord>, OrchestratorError> {
        let owner = if include_foreign_drafts {
            None
        } else {
            Some(crate::owner::resolved_owner(owner_user_id))
        };
        self.list_skill_packages_filtered(owner)
    }

    fn list_skill_packages_filtered(
        &self,
        owner_user_id: Option<&str>,
    ) -> Result<Vec<SkillPackageRecord>, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let sql = if owner_user_id.is_some() {
            format!(
                "{SKILL_PACKAGE_SELECT_SQL}
             FROM skill_packages
             WHERE channel = 'released' OR owner_user_id = ?1
             ORDER BY urn ASC, major DESC, minor DESC, patch DESC, prerelease ASC"
            )
        } else {
            format!(
                "{SKILL_PACKAGE_SELECT_SQL}
             FROM skill_packages
             ORDER BY urn ASC, major DESC, minor DESC, patch DESC, prerelease ASC"
            )
        };
        let mut stmt = connection.prepare(&sql)?;
        let rows = match owner_user_id {
            Some(owner) => stmt.query_map(params![owner], read_skill_package_row)?,
            None => stmt.query_map([], read_skill_package_row)?,
        };
        rows.map(|row| row.map_err(OrchestratorError::Database))
            .map(|row| row.and_then(row_to_skill_package_record))
            .collect()
    }

    pub fn release_skill_package(
        &self,
        urn: &str,
        version: &Version,
    ) -> Result<SkillPackageRecord, OrchestratorError> {
        self.release_skill_package_with_meta(urn, version, &ReleaseMeta::default())
    }

    pub fn release_skill_package_with_meta(
        &self,
        urn: &str,
        version: &Version,
        meta: &ReleaseMeta<'_>,
    ) -> Result<SkillPackageRecord, OrchestratorError> {
        if !version.prerelease.is_empty() {
            return Err(OrchestratorError::InvalidContract(
                "release version must not include a prerelease suffix".to_string(),
            ));
        }
        let note = normalize_release_note(meta.note)?;

        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        crate::catalog_tier::reject_caller_installed_urn(&connection, urn, "skill package")?;

        let draft = fetch_skill_package_draft_row(&connection, urn)?
            .ok_or_else(|| release_no_draft_error("skill", urn))?;

        let target = (version.major, version.minor, version.patch);
        let latest = latest_released_triple(&connection, "skill_packages", urn)?;
        if let Some(released) = fetch_skill_package_row(
            &connection,
            urn,
            version.major,
            version.minor,
            version.patch,
            "",
        )? {
            if released.digest == draft.digest {
                return row_to_skill_package_record(released);
            }
            return Err(released_digest_conflict("skill", urn, target, latest));
        }
        ensure_release_is_newer(target, latest)?;
        if let Some(latest_at) = latest {
            let (major, minor, patch) = latest_at;
            let latest_row = fetch_skill_package_row(&connection, urn, major, minor, patch, "")?;
            if latest_row.is_some_and(|row| row.digest == draft.digest) {
                return Err(unchanged_since_release_conflict("skill", urn, latest_at));
            }
        }

        let now = unix_timestamp();
        // A NULL `released_by` falls back to the draft's owner, read in the same statement.
        connection.execute(
            "INSERT INTO skill_packages (
                urn, major, minor, patch, prerelease, channel, revision, digest,
                blob_relpath, size_bytes, spec_bytes, created_at, updated_at, owner_user_id,
                release_note, released_by
             ) VALUES (?1, ?2, ?3, ?4, '', 'released', 1, ?5, ?6, ?7, ?8, ?9, ?9,
                (SELECT COALESCE(owner_user_id, 'local') FROM skill_packages
                 WHERE urn = ?1 AND channel = 'draft'),
                ?10,
                COALESCE(?11, (SELECT COALESCE(owner_user_id, 'local') FROM skill_packages
                 WHERE urn = ?1 AND channel = 'draft')))",
            params![
                urn,
                version.major,
                version.minor,
                version.patch,
                draft.digest,
                draft.blob_relpath,
                draft.size_bytes,
                draft.spec_bytes,
                now,
                note,
                meta.released_by,
            ],
        )?;

        let released = fetch_skill_package_row(
            &connection,
            urn,
            version.major,
            version.minor,
            version.patch,
            "",
        )?
        .ok_or_else(|| {
            OrchestratorError::CorruptState(format!(
                "released skill package '{urn}' {}.{}.{} missing after promote",
                version.major, version.minor, version.patch
            ))
        })?;
        row_to_skill_package_record(released)
    }

    /// Rewrite legacy `sha256-{digest}.tar.gz` rows to `{owner}/sha256-{digest}.tar.gz`.
    ///
    /// Owners are already backfilled by [`crate::owner`] (NULL → `local`).
    /// Idempotent. File copy is lazy on admit/resolve so store init stays
    /// independent of Drive9.
    pub fn initialize_skill_package_blob_namespace(&self) -> Result<(), OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;

        let mut stmt = connection.prepare(
            "SELECT urn, major, minor, patch, prerelease, digest, blob_relpath,
                    COALESCE(NULLIF(owner_user_id, ''), 'local')
             FROM skill_packages",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i32>(1)?,
                    row.get::<_, i32>(2)?,
                    row.get::<_, i32>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, String>(7)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        drop(stmt);

        for (urn, major, minor, patch, prerelease, digest, blob_relpath, owner) in rows {
            let canonical = skill_blob_relpath(&owner, &digest);
            if blob_relpath == canonical {
                continue;
            }
            connection.execute(
                "UPDATE skill_packages
                 SET blob_relpath = ?1
                 WHERE urn = ?2 AND major = ?3 AND minor = ?4 AND patch = ?5 AND prerelease = ?6",
                params![canonical, urn, major, minor, patch, prerelease],
            )?;
        }
        Ok(())
    }

    /// Rows whose `blob_relpath` is not `{owner}/sha256-….tar.gz` (should be 0).
    pub fn count_unnamespaced_skill_blobs(&self) -> Result<i64, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let mut stmt = connection.prepare("SELECT blob_relpath FROM skill_packages")?;
        let mut count = 0i64;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        for relpath in rows {
            if !is_owner_namespaced_blob_relpath(&relpath?) {
                count += 1;
            }
        }
        Ok(count)
    }
}

#[derive(Debug, Clone)]
struct SkillPackageRow {
    urn: String,
    major: i32,
    minor: i32,
    patch: i32,
    prerelease: String,
    channel: String,
    revision: i64,
    digest: String,
    blob_relpath: String,
    size_bytes: i64,
    spec_bytes: Vec<u8>,
    created_at: i64,
    updated_at: i64,
    owner_user_id: String,
    catalog_tier: String,
}

fn fetch_skill_package_row(
    connection: &rusqlite::Connection,
    urn: &str,
    major: i32,
    minor: i32,
    patch: i32,
    prerelease: &str,
) -> Result<Option<SkillPackageRow>, OrchestratorError> {
    connection
        .query_row(
            &format!(
                "{SKILL_PACKAGE_SELECT_SQL}
             FROM skill_packages
             WHERE urn = ?1 AND major = ?2 AND minor = ?3 AND patch = ?4 AND prerelease = ?5"
            ),
            params![urn, major, minor, patch, prerelease],
            read_skill_package_row,
        )
        .optional()
        .map_err(OrchestratorError::Database)
}

/// Every released version of `urn`, newest first, as `X.Y.Z`.
fn released_versions(
    connection: &rusqlite::Connection,
    urn: &str,
) -> Result<Vec<String>, OrchestratorError> {
    let mut stmt = connection
        .prepare(
            "SELECT major, minor, patch FROM skill_packages
             WHERE urn = ?1 AND channel = 'released'
             ORDER BY major DESC, minor DESC, patch DESC",
        )
        .map_err(OrchestratorError::Database)?;
    let rows = stmt
        .query_map(params![urn], |row| {
            Ok(format!(
                "{}.{}.{}",
                row.get::<_, i32>(0)?,
                row.get::<_, i32>(1)?,
                row.get::<_, i32>(2)?
            ))
        })
        .map_err(OrchestratorError::Database)?;
    rows.map(|row| row.map_err(OrchestratorError::Database))
        .collect()
}

/// The owner's one draft package for `urn`, found by URN and channel.
fn fetch_skill_package_draft_row(
    connection: &rusqlite::Connection,
    urn: &str,
) -> Result<Option<SkillPackageRow>, OrchestratorError> {
    connection
        .query_row(
            &format!(
                "{SKILL_PACKAGE_SELECT_SQL}
             FROM skill_packages
             WHERE urn = ?1 AND channel = 'draft'"
            ),
            params![urn],
            read_skill_package_row,
        )
        .optional()
        .map_err(OrchestratorError::Database)
}

/// Draft branch of `put_skill_package_with_owner`. One draft row per URN. Its
/// version columns are a hint: the version it was last published with.
fn put_skill_package_draft(
    connection: &rusqlite::Connection,
    urn: &str,
    version: &Version,
    package: &SkillPackageRecord,
    spec_bytes: &[u8],
    blob_relpath: &str,
    owner: &str,
) -> Result<SkillPackageRecord, OrchestratorError> {
    let now = unix_timestamp();

    if let Some(row) = fetch_skill_package_draft_row(connection, urn)? {
        if row.digest == package.digest {
            return row_to_skill_package_record(row);
        }
        let moves = (row.major, row.minor, row.patch, row.prerelease.as_str())
            != (
                version.major,
                version.minor,
                version.patch,
                version.prerelease.as_str(),
            );
        // The draft is the only row that can move. A row already at the new
        // version is therefore a released one.
        if moves
            && fetch_skill_package_row(
                connection,
                urn,
                version.major,
                version.minor,
                version.patch,
                &version.prerelease,
            )?
            .is_some()
        {
            return Err(OrchestratorError::Conflict(format!(
                "released skill package '{urn}' {}.{}.{} already exists; publish the draft at a new version",
                version.major, version.minor, version.patch
            )));
        }

        connection.execute(
            "UPDATE skill_packages
             SET major = ?1, minor = ?2, patch = ?3, prerelease = ?4,
                 revision = revision + 1,
                 digest = ?5,
                 blob_relpath = ?6,
                 size_bytes = ?7,
                 spec_bytes = ?8,
                 updated_at = ?9
             WHERE urn = ?10 AND channel = 'draft'",
            params![
                version.major,
                version.minor,
                version.patch,
                version.prerelease,
                package.digest,
                blob_relpath,
                package.size_bytes,
                spec_bytes,
                now,
                urn,
            ],
        )?;
        let updated = fetch_skill_package_draft_row(connection, urn)?.ok_or_else(|| {
            OrchestratorError::CorruptState(format!(
                "skill package '{urn}' disappeared after draft replace"
            ))
        })?;
        return row_to_skill_package_record(updated);
    }

    if fetch_skill_package_row(
        connection,
        urn,
        version.major,
        version.minor,
        version.patch,
        &version.prerelease,
    )?
    .is_some()
    {
        return Err(OrchestratorError::Conflict(format!(
            "released skill package '{urn}' {}.{}.{} already exists; publish the draft at a new version",
            version.major, version.minor, version.patch
        )));
    }
    connection.execute(
        "INSERT INTO skill_packages (
            urn, major, minor, patch, prerelease, channel, revision, digest,
            blob_relpath, size_bytes, spec_bytes, created_at, updated_at, owner_user_id
         ) VALUES (?1, ?2, ?3, ?4, ?5, 'draft', 1, ?6, ?7, ?8, ?9, ?10, ?10, ?11)",
        params![
            urn,
            version.major,
            version.minor,
            version.patch,
            version.prerelease,
            package.digest,
            blob_relpath,
            package.size_bytes,
            spec_bytes,
            now,
            owner,
        ],
    )?;
    let inserted = fetch_skill_package_draft_row(connection, urn)?.ok_or_else(|| {
        OrchestratorError::CorruptState(format!(
            "skill package '{urn}' missing immediately after draft insert"
        ))
    })?;
    row_to_skill_package_record(inserted)
}

fn read_skill_package_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SkillPackageRow> {
    Ok(SkillPackageRow {
        urn: row.get(0)?,
        major: row.get(1)?,
        minor: row.get(2)?,
        patch: row.get(3)?,
        prerelease: row.get(4)?,
        channel: row.get(5)?,
        revision: row.get(6)?,
        digest: row.get(7)?,
        blob_relpath: row.get(8)?,
        size_bytes: row.get(9)?,
        spec_bytes: row.get(10)?,
        created_at: row.get(11)?,
        updated_at: row.get(12)?,
        owner_user_id: row.get(13)?,
        catalog_tier: row.get(14)?,
    })
}

fn row_to_skill_package_record(
    row: SkillPackageRow,
) -> Result<SkillPackageRecord, OrchestratorError> {
    let spec = SkillManifest::decode(row.spec_bytes.as_slice())
        .map_err(|e| OrchestratorError::Core(e.to_string()))?;
    let channel = match row.channel.as_str() {
        "draft" => SkillChannel::Draft,
        "released" => SkillChannel::Released,
        other => {
            return Err(OrchestratorError::CorruptState(format!(
                "unknown skill package channel '{other}'"
            )))
        }
    };

    Ok(SkillPackageRecord {
        resource_id: Some(ResourceId { urn: row.urn }),
        version: Some(Version {
            major: row.major,
            minor: row.minor,
            patch: row.patch,
            prerelease: row.prerelease,
            build: String::new(),
        }),
        channel: channel as i32,
        digest: row.digest,
        blob_relpath: row.blob_relpath,
        size_bytes: row.size_bytes,
        spec: Some(spec),
        metadata: Some(ResourceMetadata {
            revision: row.revision,
            created_at: Some(timestamp_from_unix(row.created_at)),
            updated_at: Some(timestamp_from_unix(row.updated_at)),
            ..Default::default()
        }),
        owner_user_id: row.owner_user_id,
        read_only: row.catalog_tier == crate::catalog_tier::CATALOG_TIER_MARKETPLACE,
        tier: row.catalog_tier.clone(),
        catalog_tier: row.catalog_tier,
        origin: None,
    })
}

fn skill_channel_from_i32(channel: i32) -> Result<SkillChannel, OrchestratorError> {
    match SkillChannel::try_from(channel).unwrap_or(SkillChannel::Unspecified) {
        SkillChannel::Draft => Ok(SkillChannel::Draft),
        SkillChannel::Released => Ok(SkillChannel::Released),
        SkillChannel::Unspecified => Err(OrchestratorError::InvalidContract(
            "SkillPackageRecord.channel is required".to_string(),
        )),
    }
}

fn channel_db_value(channel: SkillChannel) -> &'static str {
    match channel {
        SkillChannel::Draft => "draft",
        SkillChannel::Released => "released",
        SkillChannel::Unspecified => "draft",
    }
}

fn timestamp_from_unix(secs: i64) -> Timestamp {
    Timestamp {
        seconds: secs,
        nanos: 0,
    }
}

fn format_prerelease_suffix(prerelease: &str) -> String {
    if prerelease.is_empty() {
        String::new()
    } else {
        format!("-{prerelease}")
    }
}

fn skill_package_row_if_visible(
    row: SkillPackageRow,
    urn: &str,
    caller: Option<&str>,
    include_foreign_drafts: bool,
) -> Result<SkillPackageRow, OrchestratorError> {
    if include_foreign_drafts {
        return Ok(row);
    }
    if let Some(caller) = caller {
        if row.channel == channel_db_value(SkillChannel::Draft)
            && crate::owner::resolved_owner(&row.owner_user_id) != caller
        {
            return Err(crate::catalog::no_draft_error("skill", urn));
        }
    }
    Ok(row)
}

fn unix_timestamp() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(i64::MAX)
}

#[cfg(test)]
pub(crate) static DRIVE9_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::OrchestratorError;
    use vam_spec::v1::{ResourceId, SkillChannel, SkillManifest, SkillPackageRecord, Version};

    const DIGEST_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const DIGEST_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const DIGEST_C: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
    const DIGEST_D: &str = "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";

    fn draft_package(digest: &str) -> SkillPackageRecord {
        SkillPackageRecord {
            resource_id: Some(ResourceId {
                urn: "urn:vam:skill:vam.local/vam/assistant-tools".to_string(),
            }),
            version: Some(Version {
                major: 1,
                minor: 0,
                patch: 0,
                prerelease: "draft".to_string(),
                build: String::new(),
            }),
            channel: SkillChannel::Draft as i32,
            digest: digest.to_string(),
            blob_relpath: skill_blob_relpath(crate::users::LOCAL_USER_ID, digest),
            size_bytes: 64,
            spec: Some(SkillManifest {
                resource_id: Some(ResourceId {
                    urn: "urn:vam:skill:vam.local/vam/assistant-tools".to_string(),
                }),
                version: Some(Version {
                    major: 1,
                    minor: 0,
                    patch: 0,
                    prerelease: "draft".to_string(),
                    build: String::new(),
                }),
                description: "Local skill".to_string(),
                system_extension: String::new(),
                tools: vec![],
                runtime_compatibility: None,
                metadata: None,
                locators: vec![],
                capabilities: vec![],
                policy: String::new(),
                playbooks: vec![],
                ..Default::default()
            }),
            metadata: None,
            ..Default::default()
        }
    }

    fn released_package(digest: &str) -> SkillPackageRecord {
        let mut package = draft_package(digest);
        package.channel = SkillChannel::Released as i32;
        package.version = Some(Version {
            major: 1,
            minor: 0,
            patch: 0,
            prerelease: String::new(),
            build: String::new(),
        });
        if let Some(spec) = package.spec.as_mut() {
            spec.version = package.version.clone();
        }
        package
    }

    fn store() -> SqliteWorkflowStore {
        let store = SqliteWorkflowStore::in_memory().expect("in-memory store");
        store.initialize_catalog_schema().expect("catalog schema");
        store
    }

    #[test]
    fn draft_replace_increments_revision_on_new_digest() {
        let store = store();
        let first = store
            .put_skill_package(&draft_package(DIGEST_A))
            .expect("first draft publish");
        assert_eq!(first.metadata.as_ref().map(|m| m.revision), Some(1));

        let second = store
            .put_skill_package(&draft_package(DIGEST_B))
            .expect("draft replace");
        assert_eq!(second.digest, DIGEST_B);
        assert_eq!(second.metadata.as_ref().map(|m| m.revision), Some(2));
    }

    #[test]
    fn manifest_any_reads_a_published_package_that_has_no_catalog_row() {
        use vam_spec::v1::ToolSpec;
        let store = store();
        let urn = "urn:vam:skill:vam.local/vam/assistant-tools";
        let mut package = draft_package(DIGEST_A);
        package.spec.as_mut().expect("spec").tools = vec![ToolSpec {
            name: "send_report".to_string(),
            description: "Send a report".to_string(),
            input_schema_json: "{\"type\":\"object\"}".to_string(),
            execution_provider: Some(vam_spec::v1::tool_spec::ExecutionProvider::LocalExec(
                vam_spec::v1::LocalExecSpec {
                    command: "bun".to_string(),
                    entrypoint: "./execute.js".to_string(),
                    ..Default::default()
                },
            )),
            ..Default::default()
        }];
        store.put_skill_package(&package).expect("draft package");
        store
            .release_skill_package(
                urn,
                &Version {
                    major: 1,
                    ..Default::default()
                },
            )
            .expect("release");

        // Neither the pinned release nor the draft is in the catalog `skills` table.
        let pinned = Version {
            major: 1,
            ..Default::default()
        };
        let released = store
            .get_skill_manifest_any(urn, Some(&pinned), "local", false)
            .expect("released package manifest");
        assert_eq!(released.tools[0].name, "send_report");
        let draft_pin = Version {
            major: 1,
            prerelease: "draft".to_string(),
            ..Default::default()
        };
        let draft = store
            .get_skill_manifest_any(urn, Some(&draft_pin), "local", false)
            .expect("draft package manifest");
        assert_eq!(draft.tools[0].name, "send_report");
        let unpinned = store
            .get_skill_manifest_any(urn, None, "local", false)
            .expect("unpinned manifest");
        assert_eq!(unpinned.tools[0].name, "send_report");
        assert!(matches!(
            store.get_skill_manifest_any("urn:vam:skill:vam.local/vam/none", None, "local", false),
            Err(OrchestratorError::NotFound(_))
        ));
    }

    #[test]
    fn draft_same_digest_is_idempotent() {
        let store = store();
        let first = store
            .put_skill_package(&draft_package(DIGEST_A))
            .expect("first draft publish");
        let second = store
            .put_skill_package(&draft_package(DIGEST_A))
            .expect("idempotent draft publish");
        assert_eq!(first.metadata.as_ref().map(|m| m.revision), Some(1));
        assert_eq!(second.metadata.as_ref().map(|m| m.revision), Some(1));
    }

    fn draft_package_at(digest: &str, minor: i32, patch: i32) -> SkillPackageRecord {
        let mut package = draft_package(digest);
        let version = Version {
            major: 0,
            minor,
            patch,
            prerelease: "draft".to_string(),
            build: String::new(),
        };
        if let Some(spec) = package.spec.as_mut() {
            spec.version = Some(version.clone());
        }
        package.version = Some(version);
        package
    }

    #[test]
    fn draft_at_new_manifest_version_moves_the_single_draft_row() {
        let store = store();
        let urn = "urn:vam:skill:vam.local/vam/assistant-tools";
        store
            .put_skill_package(&draft_package_at(DIGEST_A, 1, 1))
            .expect("draft 0.1.1");
        let moved = store
            .put_skill_package(&draft_package_at(DIGEST_B, 1, 2))
            .expect("draft 0.1.2");

        let version = moved.version.as_ref().expect("version");
        assert_eq!((version.minor, version.patch), (1, 2));
        assert_eq!(moved.metadata.as_ref().map(|m| m.revision), Some(2));
        let rows: i64 = store
            .connection
            .lock()
            .expect("lock")
            .query_row(
                "SELECT COUNT(*) FROM skill_packages WHERE urn = ?1",
                [urn],
                |row| row.get(0),
            )
            .expect("count");
        assert_eq!(rows, 1);

        // Draft lookups ignore the stored version numbers.
        let other_version = Version {
            major: 9,
            minor: 9,
            patch: 9,
            prerelease: "draft".to_string(),
            build: String::new(),
        };
        let by_urn = store
            .get_skill_package_for_query(urn, Some(&other_version), SkillChannel::Draft)
            .expect("draft by urn");
        assert_eq!(by_urn.digest, DIGEST_B);
    }

    #[test]
    fn skill_package_release_records_note_and_releaser_in_history() {
        let store = store();
        let urn = "urn:vam:skill:vam.local/vam/assistant-tools";
        store
            .put_skill_package(&draft_package(DIGEST_A))
            .expect("draft");
        let version = Version {
            major: 1,
            minor: 0,
            patch: 0,
            prerelease: String::new(),
            build: String::new(),
        };
        store
            .release_skill_package_with_meta(
                urn,
                &version,
                &ReleaseMeta {
                    note: " first package ",
                    released_by: Some("operator"),
                },
            )
            .expect("release");
        let history = store
            .list_catalog_versions("skill_packages", urn, crate::users::LOCAL_USER_ID, false)
            .expect("history");
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].channel, SkillChannel::Draft);
        assert!(history[0].note.is_empty());
        assert_eq!(history[1].channel, SkillChannel::Released);
        assert_eq!(history[1].note, "first package");
        assert_eq!(history[1].released_by, "operator");

        let bob = store
            .list_catalog_versions("skill_packages", urn, "bob", false)
            .expect("bob sees the release");
        assert_eq!(bob.len(), 1);
    }

    #[test]
    fn skill_package_release_lifecycle_releases_more_than_once() {
        let store = store();
        let urn = "urn:vam:skill:vam.local/vam/assistant-tools";
        let digest_for = |tag: &str| match tag {
            "a" => DIGEST_A,
            "b" => DIGEST_B,
            "c" => DIGEST_C,
            _ => DIGEST_D,
        };
        crate::catalog_publish::release_scenario::assert_release_lifecycle(
            &store,
            "skill_packages",
            urn,
            "skill",
            |tag| {
                store
                    .put_skill_package(&draft_package(digest_for(tag)))
                    .expect("draft");
            },
            |v| store.release_skill_package(urn, v).map(|_| ()),
        );
        store
            .get_skill_package_for_query(urn, None, SkillChannel::Released)
            .expect("latest release is readable");
    }

    #[test]
    fn released_package_rejects_digest_overwrite() {
        let store = store();
        store
            .put_skill_package(&released_package(DIGEST_A))
            .expect("released publish");
        let err = store
            .put_skill_package(&released_package(DIGEST_B))
            .expect_err("released overwrite");
        assert!(matches!(err, OrchestratorError::Conflict(_)));
    }

    #[test]
    fn non_owner_cannot_version_skill_package() {
        let store = store();
        store.create_user("alice", "alice").expect("alice");
        store.create_user("bob", "bob").expect("bob");
        store
            .put_skill_package_with_owner(&draft_package(DIGEST_A), "alice")
            .expect("alice draft");
        let err = store
            .put_skill_package_with_owner(&draft_package(DIGEST_B), "bob")
            .expect_err("bob cannot version");
        assert!(matches!(err, OrchestratorError::PermissionDenied(_)));
    }

    #[test]
    fn promote_keeps_draft_and_creates_released_row() {
        let store = store();
        store
            .put_skill_package(&draft_package(DIGEST_A))
            .expect("draft publish");
        let released = store
            .release_skill_package(
                "urn:vam:skill:vam.local/vam/assistant-tools",
                &Version {
                    major: 1,
                    minor: 0,
                    patch: 0,
                    prerelease: String::new(),
                    build: String::new(),
                },
            )
            .expect("promote");

        assert_eq!(released.channel, SkillChannel::Released as i32);
        assert_eq!(released.digest, DIGEST_A);

        let draft = store
            .get_skill_package(
                "urn:vam:skill:vam.local/vam/assistant-tools",
                &Version {
                    major: 1,
                    minor: 0,
                    patch: 0,
                    prerelease: "draft".to_string(),
                    build: String::new(),
                },
            )
            .expect("draft still present");
        assert_eq!(draft.channel, SkillChannel::Draft as i32);
        assert_eq!(store.list_skill_packages().expect("list").len(), 2);
    }

    #[test]
    fn version_query_returns_the_release_even_when_the_draft_changed() {
        let store = store();
        let urn = "urn:vam:skill:vam.local/vam/assistant-tools";
        store
            .put_skill_package(&draft_package(DIGEST_A))
            .expect("draft publish");
        let v1 = Version {
            major: 1,
            minor: 0,
            patch: 0,
            prerelease: String::new(),
            build: String::new(),
        };
        store
            .release_skill_package(urn, &v1)
            .expect("release 1.0.0");
        store
            .put_skill_package(&draft_package(DIGEST_B))
            .expect("draft changes after the release");

        let owner = crate::users::LOCAL_USER_ID;
        // The HTTP route turns a versioned GET into a released lookup.
        let by_version = store
            .get_skill_package_for_query_visible(
                urn,
                Some(&v1),
                SkillChannel::Released,
                owner,
                true,
            )
            .expect("version=1.0.0 returns the release");
        assert_eq!(by_version.channel, SkillChannel::Released as i32);
        assert_eq!(by_version.digest, DIGEST_A);

        let by_channel = store
            .get_skill_package_for_query_visible(urn, None, SkillChannel::Released, owner, true)
            .expect("channel=released returns the release");
        assert_eq!(by_channel.digest, DIGEST_A);

        let draft = store
            .get_skill_package_for_query_visible(urn, None, SkillChannel::Draft, owner, true)
            .expect("no version returns the draft");
        assert_eq!(draft.channel, SkillChannel::Draft as i32);
        assert_eq!(draft.digest, DIGEST_B);
    }

    #[test]
    fn version_without_a_release_lists_the_released_versions() {
        let store = store();
        let urn = "urn:vam:skill:vam.local/vam/assistant-tools";
        store
            .put_skill_package(&draft_package(DIGEST_A))
            .expect("draft publish");
        let v1 = Version {
            major: 1,
            minor: 0,
            patch: 0,
            prerelease: String::new(),
            build: String::new(),
        };
        store
            .release_skill_package(urn, &v1)
            .expect("release 1.0.0");
        store
            .put_skill_package(&draft_package(DIGEST_B))
            .expect("draft changes after the release");

        let missing = Version {
            major: 2,
            minor: 0,
            patch: 0,
            prerelease: String::new(),
            build: String::new(),
        };
        match store.get_skill_package_for_query_visible(
            urn,
            Some(&missing),
            SkillChannel::Released,
            crate::users::LOCAL_USER_ID,
            true,
        ) {
            Err(OrchestratorError::NotFound(msg)) => {
                assert!(msg.contains("2.0.0 has no release"), "{msg}");
                assert!(msg.contains("released versions: 1.0.0"), "{msg}");
                assert!(!msg.contains("draft"), "{msg}");
            }
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[test]
    fn promote_conflicts_when_released_digest_differs() {
        let store = store();
        store
            .put_skill_package(&draft_package(DIGEST_A))
            .expect("draft publish");
        store
            .put_skill_package(&released_package(DIGEST_B))
            .expect("released publish");
        let err = store
            .release_skill_package(
                "urn:vam:skill:vam.local/vam/assistant-tools",
                &Version {
                    major: 1,
                    minor: 0,
                    patch: 0,
                    prerelease: String::new(),
                    build: String::new(),
                },
            )
            .expect_err("promote conflict");
        assert!(matches!(err, OrchestratorError::Conflict(_)));
    }

    #[test]
    fn skill_package_write_stamps_owner_and_list_stays_global() {
        let store = store();
        store
            .put_skill_package_with_owner(&draft_package(DIGEST_A), crate::users::OPERATOR_USER_ID)
            .expect("draft op");
        let other = {
            let mut pkg = draft_package(DIGEST_B);
            pkg.resource_id = Some(ResourceId {
                urn: "urn:vam:skill:vam.local/vam/other-tools".to_string(),
            });
            if let Some(spec) = pkg.spec.as_mut() {
                spec.resource_id = pkg.resource_id.clone();
            }
            pkg
        };
        store
            .put_skill_package_with_owner(&other, crate::users::LOCAL_USER_ID)
            .expect("draft loc");

        assert_eq!(
            store
                .skill_package_owner_user_id(
                    "urn:vam:skill:vam.local/vam/assistant-tools",
                    1,
                    0,
                    0,
                    "draft"
                )
                .expect("op owner"),
            crate::users::OPERATOR_USER_ID
        );
        assert_eq!(
            store
                .skill_package_owner_user_id(
                    "urn:vam:skill:vam.local/vam/other-tools",
                    1,
                    0,
                    0,
                    "draft"
                )
                .expect("loc owner"),
            crate::users::LOCAL_USER_ID
        );

        let listed = store.list_skill_packages().expect("list");
        let urns: Vec<_> = listed
            .iter()
            .filter_map(|p| p.resource_id.as_ref().map(|r| r.urn.as_str()))
            .collect();
        assert!(urns.contains(&"urn:vam:skill:vam.local/vam/assistant-tools"));
        assert!(urns.contains(&"urn:vam:skill:vam.local/vam/other-tools"));

        let released = store
            .release_skill_package(
                "urn:vam:skill:vam.local/vam/assistant-tools",
                &Version {
                    major: 1,
                    minor: 0,
                    patch: 0,
                    prerelease: String::new(),
                    build: String::new(),
                },
            )
            .expect("promote");
        assert_eq!(released.channel, SkillChannel::Released as i32);
        assert_eq!(
            store
                .skill_package_owner_user_id(
                    "urn:vam:skill:vam.local/vam/assistant-tools",
                    1,
                    0,
                    0,
                    ""
                )
                .expect("released owner"),
            crate::users::OPERATOR_USER_ID
        );
    }

    #[test]
    fn catalog_visibility_hides_foreign_skill_package_drafts() {
        let store = store();
        store.create_user("alice", "alice").expect("alice");
        store.create_user("bob", "bob").expect("bob");
        store
            .put_skill_package_with_owner(&draft_package(DIGEST_A), "alice")
            .expect("alice draft");
        let bob_draft = {
            let mut pkg = draft_package(DIGEST_B);
            pkg.resource_id = Some(ResourceId {
                urn: "urn:vam:skill:vam.local/vam/bob-tools".to_string(),
            });
            if let Some(spec) = pkg.spec.as_mut() {
                spec.resource_id = pkg.resource_id.clone();
            }
            pkg
        };
        store
            .put_skill_package_with_owner(&bob_draft, "bob")
            .expect("bob draft");
        store
            .release_skill_package(
                "urn:vam:skill:vam.local/vam/bob-tools",
                &Version {
                    major: 1,
                    minor: 0,
                    patch: 0,
                    prerelease: String::new(),
                    build: String::new(),
                },
            )
            .expect("bob released");

        let listed = store
            .list_skill_packages_visible("alice", false)
            .expect("visible list");
        let urns: Vec<_> = listed
            .iter()
            .filter_map(|p| p.resource_id.as_ref().map(|r| r.urn.as_str()))
            .collect();
        assert!(urns.contains(&"urn:vam:skill:vam.local/vam/assistant-tools"));
        assert!(urns.contains(&"urn:vam:skill:vam.local/vam/bob-tools"));
        assert!(
            listed.iter().all(|p| {
                let urn = p.resource_id.as_ref().map(|r| r.urn.as_str()).unwrap_or("");
                if urn == "urn:vam:skill:vam.local/vam/bob-tools" {
                    p.channel == SkillChannel::Released as i32
                } else {
                    true
                }
            }),
            "bob's draft must not appear: {listed:?}"
        );

        store
            .get_skill_package_for_query_visible(
                "urn:vam:skill:vam.local/vam/assistant-tools",
                None,
                SkillChannel::Draft,
                "alice",
                false,
            )
            .expect("alice sees own draft");
        let err = store
            .get_skill_package_for_query_visible(
                "urn:vam:skill:vam.local/vam/bob-tools",
                None,
                SkillChannel::Draft,
                "alice",
                false,
            )
            .expect_err("alice cannot see bob draft");
        assert!(matches!(err, OrchestratorError::NotFound(_)));
        store
            .get_skill_package_for_query_visible(
                "urn:vam:skill:vam.local/vam/bob-tools",
                None,
                SkillChannel::Released,
                "alice",
                false,
            )
            .expect("alice sees bob released");
    }

    #[test]
    fn catalog_visibility_admin_sees_foreign_skill_package_drafts() {
        let store = store();
        store.create_user("alice", "alice").expect("alice");
        store.create_user("admin", "admin").expect("admin");
        store
            .put_skill_package_with_owner(&draft_package(DIGEST_A), "alice")
            .expect("alice draft");

        let member_list = store
            .list_skill_packages_visible("admin", false)
            .expect("member list");
        assert!(
            !member_list.iter().any(|p| {
                p.resource_id.as_ref().map(|r| r.urn.as_str())
                    == Some("urn:vam:skill:vam.local/vam/assistant-tools")
            }),
            "member must not see alice draft: {member_list:?}"
        );
        assert!(matches!(
            store
                .get_skill_package_for_query_visible(
                    "urn:vam:skill:vam.local/vam/assistant-tools",
                    None,
                    SkillChannel::Draft,
                    "admin",
                    false,
                )
                .expect_err("member get"),
            OrchestratorError::NotFound(_)
        ));

        let listed = store
            .list_skill_packages_visible("admin", true)
            .expect("admin list");
        let alice_row = listed
            .iter()
            .find(|p| {
                p.resource_id.as_ref().map(|r| r.urn.as_str())
                    == Some("urn:vam:skill:vam.local/vam/assistant-tools")
            })
            .expect("admin lists alice draft");
        assert_eq!(alice_row.owner_user_id, "alice");
        assert_eq!(alice_row.channel, SkillChannel::Draft as i32);
        assert_eq!(
            alice_row.catalog_tier,
            crate::catalog_tier::CATALOG_TIER_USER
        );

        let got = store
            .get_skill_package_for_query_visible(
                "urn:vam:skill:vam.local/vam/assistant-tools",
                None,
                SkillChannel::Draft,
                "admin",
                true,
            )
            .expect("admin get");
        assert_eq!(got.owner_user_id, "alice");
        assert_eq!(got.channel, SkillChannel::Draft as i32);
        assert_eq!(got.catalog_tier, crate::catalog_tier::CATALOG_TIER_USER);
    }

    #[test]
    fn skill_blob_relpath_namespaces_under_owner() {
        let digest = DIGEST_A;
        let alice = skill_blob_relpath("alice", digest);
        let bob = skill_blob_relpath("bob", digest);
        assert_eq!(alice, format!("alice/{}", skill_blob_filename(digest)));
        assert_eq!(bob, format!("bob/{}", skill_blob_filename(digest)));
        assert_ne!(alice, bob);
        assert_eq!(
            skill_blob_relpath("", digest),
            skill_blob_relpath(crate::users::LOCAL_USER_ID, digest)
        );
        assert!(is_owner_namespaced_blob_relpath(&alice));
        assert!(!is_owner_namespaced_blob_relpath(&skill_blob_filename(
            digest
        )));
    }

    #[test]
    fn put_canonicalizes_legacy_blob_relpath_under_owner() {
        let store = store();
        store.create_user("alice", "alice").expect("alice user");
        let mut package = draft_package(DIGEST_A);
        package.blob_relpath = skill_blob_filename(DIGEST_A);
        let stored = store
            .put_skill_package_with_owner(&package, "alice")
            .expect("put");
        assert_eq!(stored.blob_relpath, skill_blob_relpath("alice", DIGEST_A));
        assert_eq!(store.count_unnamespaced_skill_blobs().expect("count"), 0);
    }

    #[test]
    fn migrate_rewrites_legacy_blob_relpath_to_owner() {
        let store = store();
        store.create_user("alice", "alice").expect("alice user");
        store
            .put_skill_package_with_owner(&draft_package(DIGEST_A), "alice")
            .expect("put");
        {
            let connection = store.connection.lock().expect("lock");
            connection
                .execute(
                    "UPDATE skill_packages SET blob_relpath = ?1",
                    params![skill_blob_filename(DIGEST_A)],
                )
                .expect("legacy path");
        }
        assert_eq!(store.count_unnamespaced_skill_blobs().expect("before"), 1);
        store
            .initialize_skill_package_blob_namespace()
            .expect("migrate");
        assert_eq!(store.count_unnamespaced_skill_blobs().expect("after"), 0);
        let loaded = store
            .get_skill_package(
                "urn:vam:skill:vam.local/vam/assistant-tools",
                &Version {
                    major: 1,
                    minor: 0,
                    patch: 0,
                    prerelease: "draft".to_string(),
                    build: String::new(),
                },
            )
            .expect("get");
        assert_eq!(loaded.blob_relpath, skill_blob_relpath("alice", DIGEST_A));
    }
}
