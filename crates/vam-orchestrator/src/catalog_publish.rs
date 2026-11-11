//! Draft publish, release promotion, and released digest conflict for versioned
//! catalog rows (`agents`, `playbooks`, `skills`, `policies`).

use prost::Message;
use rusqlite::{params, OptionalExtension};
use sha2::{Digest, Sha256};
use vam_security::{CedarPolicyEngine, PolicyMode};
use vam_spec::v1::{
    AgentPermissions, AgentSpec, PermissionRule, PlaybookSpec, PolicyRecord, ResourceId,
    ResourceMetadata, SkillManifest, Version,
};
use vam_spec::validation::Validate;

use crate::catalog::{
    catalog_channel_from_db, catalog_record_version, CatalogKind, CATALOG_CHANNEL_DRAFT,
    CATALOG_CHANNEL_RELEASED, CATALOG_DRAFT_PRERELEASE, CATALOG_DRAFT_SLOT_MAJOR,
    CATALOG_DRAFT_SLOT_MINOR, CATALOG_DRAFT_SLOT_PATCH, CATALOG_RELEASED_MAJOR,
    CATALOG_RELEASED_MINOR, CATALOG_RELEASED_PATCH, CATALOG_RELEASED_PRERELEASE,
};
use crate::error::OrchestratorError;
use crate::workflow::SqliteWorkflowStore;

pub(crate) fn catalog_release_version() -> Version {
    Version {
        major: CATALOG_RELEASED_MAJOR,
        minor: CATALOG_RELEASED_MINOR,
        patch: CATALOG_RELEASED_PATCH,
        prerelease: String::new(),
        build: String::new(),
    }
}

#[derive(Debug, Clone)]
pub(crate) struct SpecCatalogRow {
    channel: String,
    pub(crate) digest: String,
    pub(crate) spec_bytes: Vec<u8>,
    pub(crate) owner_user_id: String,
}

#[derive(Debug, Clone)]
struct PolicyCatalogRow {
    urn: String,
    major: i32,
    minor: i32,
    patch: i32,
    prerelease: String,
    channel: String,
    revision: i64,
    digest: String,
    cedar: String,
    owner_user_id: String,
    catalog_tier: String,
}

fn kind_noun(kind: CatalogKind) -> &'static str {
    match kind {
        CatalogKind::Agent => "agent",
        CatalogKind::Playbook => "playbook",
        CatalogKind::Skill => "skill",
    }
}

fn fetch_spec_row(
    connection: &rusqlite::Connection,
    table: &str,
    urn: &str,
    major: i32,
    minor: i32,
    patch: i32,
    prerelease: &str,
) -> Result<Option<SpecCatalogRow>, OrchestratorError> {
    fetch_spec_row_where(
        connection,
        table,
        "urn = ?1 AND major = ?2 AND minor = ?3 AND patch = ?4 AND prerelease = ?5",
        params![urn, major, minor, patch, prerelease],
    )
}

/// The owner's one draft for `urn`, found by URN and channel. Its version
/// columns are a storage slot and are not consulted.
fn fetch_spec_draft_row(
    connection: &rusqlite::Connection,
    table: &str,
    urn: &str,
) -> Result<Option<SpecCatalogRow>, OrchestratorError> {
    fetch_spec_row_where(
        connection,
        table,
        "urn = ?1 AND channel = 'draft'",
        params![urn],
    )
}

fn fetch_spec_row_where(
    connection: &rusqlite::Connection,
    table: &str,
    where_sql: &str,
    params: impl rusqlite::Params,
) -> Result<Option<SpecCatalogRow>, OrchestratorError> {
    connection
        .query_row(
            &format!(
                "SELECT channel, digest, spec_bytes, owner_user_id
                 FROM {table}
                 WHERE {where_sql}"
            ),
            params,
            |row| {
                Ok(SpecCatalogRow {
                    channel: row.get(0)?,
                    digest: row.get(1)?,
                    spec_bytes: row.get(2)?,
                    owner_user_id: row.get(3)?,
                })
            },
        )
        .optional()
        .map_err(OrchestratorError::Database)
}

fn fetch_policy_row(
    connection: &rusqlite::Connection,
    urn: &str,
    major: i32,
    minor: i32,
    patch: i32,
    prerelease: &str,
) -> Result<Option<PolicyCatalogRow>, OrchestratorError> {
    fetch_policy_row_where(
        connection,
        "urn = ?1 AND major = ?2 AND minor = ?3 AND patch = ?4 AND prerelease = ?5",
        params![urn, major, minor, patch, prerelease],
    )
}

/// The owner's one policy draft for `urn`, found by URN and channel.
fn fetch_policy_draft_row(
    connection: &rusqlite::Connection,
    urn: &str,
) -> Result<Option<PolicyCatalogRow>, OrchestratorError> {
    fetch_policy_row_where(connection, "urn = ?1 AND channel = 'draft'", params![urn])
}

fn fetch_policy_row_where(
    connection: &rusqlite::Connection,
    where_sql: &str,
    params: impl rusqlite::Params,
) -> Result<Option<PolicyCatalogRow>, OrchestratorError> {
    connection
        .query_row(
            &format!(
                "SELECT urn, major, minor, patch, prerelease, channel, revision, digest, cedar,
                        owner_user_id, catalog_tier
                 FROM policies
                 WHERE {where_sql}"
            ),
            params,
            |row| {
                Ok(PolicyCatalogRow {
                    urn: row.get(0)?,
                    major: row.get(1)?,
                    minor: row.get(2)?,
                    patch: row.get(3)?,
                    prerelease: row.get(4)?,
                    channel: row.get(5)?,
                    revision: row.get(6)?,
                    digest: row.get(7)?,
                    cedar: row.get(8)?,
                    owner_user_id: row.get(9)?,
                    catalog_tier: row.get(10)?,
                })
            },
        )
        .optional()
        .map_err(OrchestratorError::Database)
}

#[allow(clippy::too_many_arguments)] // flat parameter list is the established call shape; a params struct would change callers
fn upsert_spec_draft(
    connection: &rusqlite::Connection,
    kind: CatalogKind,
    urn: &str,
    spec_bytes: &[u8],
    digest: &str,
    owner: &str,
    major: i32,
    minor: i32,
    patch: i32,
) -> Result<SpecCatalogRow, OrchestratorError> {
    let table = kind.table();
    let noun = kind_noun(kind);
    crate::catalog_tier::reject_caller_installed_urn(connection, urn, noun)?;
    crate::owner::require_matching_urn_owner(connection, table, urn, owner, noun)?;
    let existing = fetch_spec_draft_row(connection, table, urn)?;
    let now = crate::catalog::unix_timestamp();

    if let Some(row) = existing {
        if row.digest == digest {
            return Ok(row);
        }
        connection.execute(
            &format!(
                "UPDATE {table}
                 SET revision = revision + 1,
                     digest = ?1,
                     spec_bytes = ?2,
                     updated_at = ?3
                 WHERE urn = ?4 AND channel = 'draft'"
            ),
            params![digest, spec_bytes, now, urn],
        )?;
        return fetch_spec_draft_row(connection, table, urn)?.ok_or_else(|| {
            OrchestratorError::CorruptState(format!(
                "{noun} '{urn}' disappeared after draft replace"
            ))
        });
    }

    connection.execute(
        &format!(
            "INSERT INTO {table} (
                urn, major, minor, patch, prerelease, channel, revision, digest,
                spec_bytes, created_at, updated_at, owner_user_id
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1, ?7, ?8, ?9, ?9, ?10)"
        ),
        params![
            urn,
            major,
            minor,
            patch,
            CATALOG_DRAFT_PRERELEASE,
            CATALOG_CHANNEL_DRAFT,
            digest,
            spec_bytes,
            now,
            owner,
        ],
    )?;
    fetch_spec_draft_row(connection, table, urn)?.ok_or_else(|| {
        OrchestratorError::CorruptState(format!(
            "{noun} '{urn}' missing immediately after draft insert"
        ))
    })
}

#[allow(clippy::too_many_arguments)] // flat parameter list is the established call shape; a params struct would change callers
fn upsert_spec_released(
    connection: &rusqlite::Connection,
    kind: CatalogKind,
    urn: &str,
    spec_bytes: &[u8],
    digest: &str,
    owner: &str,
    major: i32,
    minor: i32,
    patch: i32,
) -> Result<SpecCatalogRow, OrchestratorError> {
    let table = kind.table();
    let noun = kind_noun(kind);
    crate::catalog_tier::reject_caller_installed_urn(connection, urn, noun)?;
    crate::owner::require_matching_urn_owner(connection, table, urn, owner, noun)?;
    let existing = fetch_spec_row(
        connection,
        table,
        urn,
        major,
        minor,
        patch,
        CATALOG_RELEASED_PRERELEASE,
    )?;
    let now = crate::catalog::unix_timestamp();

    if let Some(row) = existing {
        if row.digest == digest {
            return Ok(row);
        }
        return Err(OrchestratorError::Conflict(format!(
            "released {noun} '{urn}' {major}.{minor}.{patch} already exists with a different digest"
        )));
    }

    connection.execute(
        &format!(
            "INSERT INTO {table} (
                urn, major, minor, patch, prerelease, channel, revision, digest,
                spec_bytes, created_at, updated_at, owner_user_id
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1, ?7, ?8, ?9, ?9, ?10)"
        ),
        params![
            urn,
            major,
            minor,
            patch,
            CATALOG_RELEASED_PRERELEASE,
            CATALOG_CHANNEL_RELEASED,
            digest,
            spec_bytes,
            now,
            owner,
        ],
    )?;
    fetch_spec_row(
        connection,
        table,
        urn,
        major,
        minor,
        patch,
        CATALOG_RELEASED_PRERELEASE,
    )?
    .ok_or_else(|| {
        OrchestratorError::CorruptState(format!(
            "{noun} '{urn}' missing immediately after released insert"
        ))
    })
}

/// A semver triple. Tuples compare field by field, which is release order.
pub(crate) type ReleaseTriple = (i32, i32, i32);

fn release_triple(version: &Version) -> ReleaseTriple {
    (version.major, version.minor, version.patch)
}

fn format_triple((major, minor, patch): ReleaseTriple) -> String {
    format!("{major}.{minor}.{patch}")
}

pub(crate) fn release_no_draft_error(noun: &str, urn: &str) -> OrchestratorError {
    OrchestratorError::NotFound(format!("no draft of {noun} '{urn}' to release"))
}

/// The highest released version of `urn` in `table`, if one exists.
pub(crate) fn latest_released_triple(
    connection: &rusqlite::Connection,
    table: &str,
    urn: &str,
) -> Result<Option<ReleaseTriple>, OrchestratorError> {
    connection
        .query_row(
            &format!(
                "SELECT major, minor, patch FROM {table}
                 WHERE urn = ?1 AND channel = 'released'
                 ORDER BY major DESC, minor DESC, patch DESC LIMIT 1"
            ),
            params![urn],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .map_err(OrchestratorError::Database)
}

/// A released row exists at the requested version with different content.
/// The next free version is the latest release with its patch bumped.
pub(crate) fn released_digest_conflict(
    noun: &str,
    urn: &str,
    version: ReleaseTriple,
    latest: Option<ReleaseTriple>,
) -> OrchestratorError {
    let (major, minor, patch) = latest.unwrap_or(version);
    OrchestratorError::Conflict(format!(
        "{noun} '{urn}' {} is already released with different content; the next free version is {major}.{minor}.{}",
        format_triple(version),
        patch.saturating_add(1)
    ))
}

/// A new higher version would store the content the latest release already
/// has. Refused: the release would only relabel it, so edit the draft first.
pub(crate) fn unchanged_since_release_conflict(
    noun: &str,
    urn: &str,
    latest: ReleaseTriple,
) -> OrchestratorError {
    OrchestratorError::Conflict(format!(
        "{noun} '{urn}' has nothing changed since {}; edit the draft before releasing a new version",
        format_triple(latest)
    ))
}

/// A new release must sort above every existing release.
pub(crate) fn ensure_release_is_newer(
    version: ReleaseTriple,
    latest: Option<ReleaseTriple>,
) -> Result<(), OrchestratorError> {
    match latest {
        Some(latest) if version <= latest => Err(OrchestratorError::InvalidContract(format!(
            "release version {} must be greater than the latest release {}",
            format_triple(version),
            format_triple(latest)
        ))),
        _ => Ok(()),
    }
}

/// Longest release note accepted, counted in characters after trimming.
pub(crate) const RELEASE_NOTE_MAX_CHARS: usize = 500;

/// Who released a version and the note they gave. `released_by` falls back to
/// the draft's owner when unset.
#[derive(Debug, Clone, Copy, Default)]
pub struct ReleaseMeta<'a> {
    pub note: &'a str,
    pub released_by: Option<&'a str>,
}

/// Trim a release note and reject one over [`RELEASE_NOTE_MAX_CHARS`].
pub(crate) fn normalize_release_note(note: &str) -> Result<&str, OrchestratorError> {
    let note = note.trim();
    if note.chars().count() > RELEASE_NOTE_MAX_CHARS {
        return Err(OrchestratorError::InvalidContract(format!(
            "release note must be at most {RELEASE_NOTE_MAX_CHARS} characters"
        )));
    }
    Ok(note)
}

/// What a release stores in place of the draft's own bytes: the draft with every
/// ref frozen (`catalog_freeze`), and the digest of the draft it was frozen from.
pub(crate) struct FrozenRelease {
    pub draft_digest: String,
    pub bytes: Vec<u8>,
    pub digest: String,
}

fn release_spec_row(
    connection: &rusqlite::Connection,
    kind: CatalogKind,
    urn: &str,
    version: &Version,
    meta: &ReleaseMeta<'_>,
    frozen: Option<FrozenRelease>,
) -> Result<SpecCatalogRow, OrchestratorError> {
    let table = kind.table();
    let noun = kind_noun(kind);
    crate::catalog_tier::reject_caller_installed_urn(connection, urn, noun)?;
    let note = normalize_release_note(meta.note)?;
    let draft = fetch_spec_draft_row(connection, table, urn)?
        .ok_or_else(|| release_no_draft_error(noun, urn))?;

    if draft.channel != CATALOG_CHANNEL_DRAFT {
        return Err(OrchestratorError::CorruptState(format!(
            "{noun} '{urn}' draft row is not a draft"
        )));
    }
    // The content a release stores and compares: frozen when the kind has refs.
    let (spec_bytes, digest) = match frozen {
        Some(frozen) => {
            if frozen.draft_digest != draft.digest {
                return Err(OrchestratorError::Conflict(format!(
                    "{noun} '{urn}' draft changed while it was being released; release again"
                )));
            }
            (frozen.bytes, frozen.digest)
        }
        None => (draft.spec_bytes.clone(), draft.digest.clone()),
    };

    let target = release_triple(version);
    let latest = latest_released_triple(connection, table, urn)?;
    if let Some(released) = fetch_spec_row(
        connection,
        table,
        urn,
        version.major,
        version.minor,
        version.patch,
        CATALOG_RELEASED_PRERELEASE,
    )? {
        if released.digest == digest {
            return Ok(released);
        }
        return Err(released_digest_conflict(noun, urn, target, latest));
    }
    ensure_release_is_newer(target, latest)?;
    if let Some(latest_at) = latest {
        let (major, minor, patch) = latest_at;
        let latest_row = fetch_spec_row(
            connection,
            table,
            urn,
            major,
            minor,
            patch,
            CATALOG_RELEASED_PRERELEASE,
        )?;
        if latest_row.is_some_and(|row| row.digest == digest) {
            return Err(unchanged_since_release_conflict(noun, urn, latest_at));
        }
    }

    let now = crate::catalog::unix_timestamp();
    let released_by = meta.released_by.unwrap_or(&draft.owner_user_id);
    connection.execute(
        &format!(
            "INSERT INTO {table} (
                urn, major, minor, patch, prerelease, channel, revision, digest,
                spec_bytes, created_at, updated_at, owner_user_id, release_note, released_by
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1, ?7, ?8, ?9, ?9, ?10, ?11, ?12)"
        ),
        params![
            urn,
            version.major,
            version.minor,
            version.patch,
            CATALOG_RELEASED_PRERELEASE,
            CATALOG_CHANNEL_RELEASED,
            digest,
            spec_bytes,
            now,
            draft.owner_user_id,
            note,
            released_by,
        ],
    )?;
    fetch_spec_row(
        connection,
        table,
        urn,
        version.major,
        version.minor,
        version.patch,
        CATALOG_RELEASED_PRERELEASE,
    )?
    .ok_or_else(|| {
        OrchestratorError::CorruptState(format!(
            "released {noun} '{urn}' {}.{}.{} missing after promote",
            version.major, version.minor, version.patch
        ))
    })
}

#[allow(clippy::too_many_arguments)] // flat parameter list is the established call shape; a params struct would change callers
fn upsert_policy_draft(
    connection: &rusqlite::Connection,
    urn: &str,
    cedar: &str,
    digest: &str,
    owner: &str,
    major: i32,
    minor: i32,
    patch: i32,
    seed_platform: bool,
) -> Result<PolicyCatalogRow, OrchestratorError> {
    crate::catalog_tier::reject_caller_installed_urn(connection, urn, "policy")?;
    crate::owner::require_matching_urn_owner(connection, "policies", urn, owner, "policy")?;
    let catalog_tier = crate::catalog_tier::resolve_catalog_tier_for_insert(
        connection,
        "policies",
        urn,
        seed_platform,
    )?;
    let existing = fetch_policy_draft_row(connection, urn)?;
    let now = crate::catalog::unix_timestamp();

    if let Some(row) = existing {
        if row.digest == digest {
            return Ok(row);
        }
        connection.execute(
            "UPDATE policies
             SET revision = revision + 1,
                 digest = ?1,
                 cedar = ?2,
                 updated_at = ?3
             WHERE urn = ?4 AND channel = 'draft'",
            params![digest, cedar, now, urn],
        )?;
        return fetch_policy_draft_row(connection, urn)?.ok_or_else(|| {
            OrchestratorError::CorruptState(format!(
                "policy '{urn}' disappeared after draft replace"
            ))
        });
    }

    connection.execute(
        "INSERT INTO policies (
            urn, major, minor, patch, prerelease, channel, revision, digest, cedar,
            created_at, updated_at, owner_user_id, catalog_tier
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1, ?7, ?8, ?9, ?9, ?10, ?11)",
        params![
            urn,
            major,
            minor,
            patch,
            CATALOG_DRAFT_PRERELEASE,
            CATALOG_CHANNEL_DRAFT,
            digest,
            cedar,
            now,
            owner,
            catalog_tier,
        ],
    )?;
    fetch_policy_draft_row(connection, urn)?.ok_or_else(|| {
        OrchestratorError::CorruptState(format!(
            "policy '{urn}' missing immediately after draft insert"
        ))
    })
}

#[allow(clippy::too_many_arguments)] // flat parameter list is the established call shape; a params struct would change callers
fn upsert_policy_released(
    connection: &rusqlite::Connection,
    urn: &str,
    cedar: &str,
    digest: &str,
    owner: &str,
    major: i32,
    minor: i32,
    patch: i32,
) -> Result<PolicyCatalogRow, OrchestratorError> {
    crate::catalog_tier::reject_caller_installed_urn(connection, urn, "policy")?;
    crate::owner::require_matching_urn_owner(connection, "policies", urn, owner, "policy")?;
    let catalog_tier =
        crate::catalog_tier::resolve_catalog_tier_for_insert(connection, "policies", urn, false)?;
    let existing = fetch_policy_row(
        connection,
        urn,
        major,
        minor,
        patch,
        CATALOG_RELEASED_PRERELEASE,
    )?;
    let now = crate::catalog::unix_timestamp();

    if let Some(row) = existing {
        if row.digest == digest {
            return Ok(row);
        }
        return Err(OrchestratorError::Conflict(format!(
            "released policy '{urn}' {major}.{minor}.{patch} already exists with a different digest"
        )));
    }

    connection.execute(
        "INSERT INTO policies (
            urn, major, minor, patch, prerelease, channel, revision, digest, cedar,
            created_at, updated_at, owner_user_id, catalog_tier
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1, ?7, ?8, ?9, ?9, ?10, ?11)",
        params![
            urn,
            major,
            minor,
            patch,
            CATALOG_RELEASED_PRERELEASE,
            CATALOG_CHANNEL_RELEASED,
            digest,
            cedar,
            now,
            owner,
            catalog_tier,
        ],
    )?;
    fetch_policy_row(
        connection,
        urn,
        major,
        minor,
        patch,
        CATALOG_RELEASED_PRERELEASE,
    )?
    .ok_or_else(|| {
        OrchestratorError::CorruptState(format!(
            "policy '{urn}' missing immediately after released insert"
        ))
    })
}

fn release_policy_row(
    connection: &rusqlite::Connection,
    urn: &str,
    version: &Version,
    meta: &ReleaseMeta<'_>,
) -> Result<PolicyCatalogRow, OrchestratorError> {
    crate::catalog_tier::reject_caller_installed_urn(connection, urn, "policy")?;
    let note = normalize_release_note(meta.note)?;
    let draft = fetch_policy_draft_row(connection, urn)?
        .ok_or_else(|| release_no_draft_error("policy", urn))?;

    if draft.channel != CATALOG_CHANNEL_DRAFT {
        return Err(OrchestratorError::CorruptState(format!(
            "policy '{urn}' draft row is not a draft"
        )));
    }

    let target = release_triple(version);
    let latest = latest_released_triple(connection, "policies", urn)?;
    if let Some(released) = fetch_policy_row(
        connection,
        urn,
        version.major,
        version.minor,
        version.patch,
        CATALOG_RELEASED_PRERELEASE,
    )? {
        if released.digest == draft.digest {
            return Ok(released);
        }
        return Err(released_digest_conflict("policy", urn, target, latest));
    }
    ensure_release_is_newer(target, latest)?;
    if let Some(latest_at) = latest {
        let (major, minor, patch) = latest_at;
        let latest_row = fetch_policy_row(
            connection,
            urn,
            major,
            minor,
            patch,
            CATALOG_RELEASED_PRERELEASE,
        )?;
        if latest_row.is_some_and(|row| row.digest == draft.digest) {
            return Err(unchanged_since_release_conflict("policy", urn, latest_at));
        }
    }

    let now = crate::catalog::unix_timestamp();
    let released_by = meta.released_by.unwrap_or(&draft.owner_user_id);
    connection.execute(
        "INSERT INTO policies (
            urn, major, minor, patch, prerelease, channel, revision, digest, cedar,
            created_at, updated_at, owner_user_id, catalog_tier, release_note, released_by
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1, ?7, ?8, ?9, ?9, ?10, ?11, ?12, ?13)",
        params![
            urn,
            version.major,
            version.minor,
            version.patch,
            CATALOG_RELEASED_PRERELEASE,
            CATALOG_CHANNEL_RELEASED,
            draft.digest,
            draft.cedar,
            now,
            draft.owner_user_id,
            draft.catalog_tier,
            note,
            released_by,
        ],
    )?;
    fetch_policy_row(
        connection,
        urn,
        version.major,
        version.minor,
        version.patch,
        CATALOG_RELEASED_PRERELEASE,
    )?
    .ok_or_else(|| {
        OrchestratorError::CorruptState(format!(
            "released policy '{urn}' {}.{}.{} missing after promote",
            version.major, version.minor, version.patch
        ))
    })
}

fn policy_record_from_row(row: PolicyCatalogRow) -> PolicyRecord {
    let version = catalog_record_version(
        &row.channel,
        row.major,
        row.minor,
        row.patch,
        &row.prerelease,
    );
    PolicyRecord {
        resource_id: Some(ResourceId { urn: row.urn }),
        cedar: row.cedar,
        digest: row.digest,
        catalog_tier: row.catalog_tier,
        metadata: Some(ResourceMetadata {
            revision: row.revision,
            ..Default::default()
        }),
        version: Some(version),
        channel: catalog_channel_from_db(&row.channel) as i32,
    }
}

fn encode_spec<T: Message>(spec: &T) -> Result<(Vec<u8>, String), OrchestratorError> {
    let mut bytes = Vec::new();
    spec.encode(&mut bytes)
        .map_err(|e| OrchestratorError::Core(e.to_string()))?;
    let digest = format!("{:x}", Sha256::digest(&bytes));
    Ok((bytes, digest))
}

fn decode_agent(row: SpecCatalogRow) -> Result<AgentSpec, OrchestratorError> {
    AgentSpec::decode(row.spec_bytes.as_slice()).map_err(|e| OrchestratorError::Core(e.to_string()))
}

fn playbook_ready_for_publish(spec: &PlaybookSpec) -> Result<PlaybookSpec, OrchestratorError> {
    vam_spec::package::playbook_ready_for_publish(spec)
        .map_err(|e| OrchestratorError::InvalidContract(e.0))
}

/// A release is the publish step. On top of the draft checks it requires
/// `activation.when` (AR decision 20); a draft may be saved without it.
fn playbook_ready_for_release(spec: &PlaybookSpec) -> Result<PlaybookSpec, OrchestratorError> {
    vam_spec::package::playbook_ready_for_release(spec)
        .map_err(|e| OrchestratorError::InvalidContract(e.0))
}

fn decode_playbook_bytes(bytes: &[u8]) -> Result<PlaybookSpec, OrchestratorError> {
    PlaybookSpec::decode(bytes).map_err(|e| OrchestratorError::Core(e.to_string()))
}

fn decode_playbook(row: SpecCatalogRow) -> Result<PlaybookSpec, OrchestratorError> {
    PlaybookSpec::decode(row.spec_bytes.as_slice())
        .map_err(|e| OrchestratorError::Core(e.to_string()))
}

fn decode_skill(row: SpecCatalogRow) -> Result<SkillManifest, OrchestratorError> {
    SkillManifest::decode(row.spec_bytes.as_slice())
        .map_err(|e| OrchestratorError::Core(e.to_string()))
}

fn validate_release_version(version: &Version) -> Result<(), OrchestratorError> {
    if !version.prerelease.is_empty() {
        return Err(OrchestratorError::InvalidContract(
            "release version must not include a prerelease suffix".to_string(),
        ));
    }
    Ok(())
}

/// A draft still on `capability_policy` or `hitl_policy`. AX-5.6b migrates it.
fn is_legacy_permissions_draft(spec: &AgentSpec) -> bool {
    spec.capability_policy.is_some() || spec.hitl_policy.is_some()
}

/// Decision 13, written onto the draft so the digest and the release diff see it.
///
/// A draft with neither `permissions` nor a legacy policy would otherwise run
/// on the builtins-only fallback. Publishing fills `tools.default =
/// bySideEffect` and `internet.default = allow`. A draft that already carries
/// `permissions`, or a legacy draft, is stored as written. Released rows are
/// not updated here.
fn apply_publish_permission_defaults(spec: &mut AgentSpec) {
    vam_spec::package::apply_publish_permission_defaults(spec);
}

/// The owner's draft, or any draft when `include_foreign_drafts` (org admin).
/// Missing and foreign drafts share one NotFound, the same as other draft reads.
pub(crate) fn load_agent_draft(
    store: &SqliteWorkflowStore,
    urn: &str,
    owner_user_id: &str,
    include_foreign_drafts: bool,
) -> Result<AgentSpec, OrchestratorError> {
    let connection = store
        .connection
        .lock()
        .map_err(|_| OrchestratorError::StorageUnavailable)?;
    let row = fetch_spec_draft_row(&connection, CatalogKind::Agent.table(), urn)?
        .ok_or_else(|| crate::catalog::no_draft_error("agent", urn))?;
    if !include_foreign_drafts {
        let caller = crate::owner::resolved_owner(owner_user_id);
        let row_owner = crate::owner::resolved_owner(&row.owner_user_id);
        if caller != row_owner {
            return Err(crate::catalog::no_draft_error("agent", urn));
        }
    }
    decode_agent(row)
}

/// Conflict returned when a legacy draft is edited through the permissions API.
pub(crate) fn legacy_draft_conflict(urn: &str) -> OrchestratorError {
    OrchestratorError::Conflict(format!(
        "agent '{urn}' still uses capability_policy or hitl_policy; migrate first"
    ))
}

/// Load the owner's draft, apply `f` to its `permissions`, validate, and store
/// a new draft revision.
///
/// The same revision rule as other draft writes: a changed digest increments
/// `revision`, and identical bytes leave it. A missing draft and someone
/// else's draft are both NotFound. A legacy draft is a conflict (`migrate
/// first`) and is not rewritten. AX-5.5a and `PUT /v1/agents/*urn/permissions`
/// both go through here.
pub fn update_draft_permissions(
    store: &SqliteWorkflowStore,
    urn: &str,
    owner: &str,
    f: impl FnOnce(&mut AgentPermissions),
) -> Result<AgentSpec, OrchestratorError> {
    write_draft_permissions(store, urn, owner, false, f)
}

/// Replace the owner's draft permissions and drop `capability_policy` and
/// `hitl_policy`. The same writer as [`update_draft_permissions`], which
/// refuses a legacy draft so a normal edit cannot skip migration.
pub fn store_migrated_permissions(
    store: &SqliteWorkflowStore,
    urn: &str,
    owner: &str,
    permissions: AgentPermissions,
) -> Result<AgentSpec, OrchestratorError> {
    write_draft_permissions(store, urn, owner, true, move |slot| {
        *slot = permissions;
    })
}

fn write_draft_permissions(
    store: &SqliteWorkflowStore,
    urn: &str,
    owner: &str,
    clear_legacy: bool,
    f: impl FnOnce(&mut AgentPermissions),
) -> Result<AgentSpec, OrchestratorError> {
    let mut spec = load_agent_draft(store, urn, owner, false)?;
    if clear_legacy {
        spec.capability_policy = None;
        spec.hitl_policy = None;
    } else if is_legacy_permissions_draft(&spec) {
        return Err(legacy_draft_conflict(urn));
    }
    let mut permissions = spec.permissions.take().unwrap_or_default();
    f(&mut permissions);
    spec.permissions = Some(permissions);
    store.publish_agent_draft_with_owner(&spec, owner)
}

/// Insert `rule`, or replace the existing rule for the same target.
///
/// Tool targets land in `tools.rules` and host targets in `internet.rules`.
/// The match uses the canonical target, so a different spelling of the same
/// server replaces the earlier rule. The stored target is that canonical
/// spelling when it parses.
pub fn upsert_permission_rule(permissions: &mut AgentPermissions, rule: PermissionRule) {
    let parsed = vam_spec::Target::parse(&rule.target).ok();
    let canonical = parsed.as_ref().map(|target| target.to_string());
    let on_internet = parsed.as_ref().is_some_and(|target| target.is_host());
    let raw = rule.target;
    let stored_target = canonical.clone().unwrap_or_else(|| raw.clone());
    let matches = |existing: &str| -> bool {
        if let Some(canonical) = canonical.as_deref() {
            if vam_spec::Target::parse(existing)
                .ok()
                .map(|target| target.to_string())
                .as_deref()
                == Some(canonical)
            {
                return true;
            }
        }
        existing == raw
    };
    let replacement = PermissionRule {
        target: stored_target,
        decision: rule.decision,
    };
    if on_internet {
        strip_matching_rules(
            permissions.tools.as_mut().map(|tools| &mut tools.rules),
            &matches,
        );
        let internet = permissions.internet.get_or_insert_with(Default::default);
        replace_or_push_rule(&mut internet.rules, &matches, replacement);
    } else {
        strip_matching_rules(
            permissions
                .internet
                .as_mut()
                .map(|internet| &mut internet.rules),
            &matches,
        );
        let tools = permissions.tools.get_or_insert_with(Default::default);
        replace_or_push_rule(&mut tools.rules, &matches, replacement);
    }
}

fn strip_matching_rules(rules: Option<&mut Vec<PermissionRule>>, matches: &impl Fn(&str) -> bool) {
    if let Some(rules) = rules {
        rules.retain(|rule| !matches(&rule.target));
    }
}

fn replace_or_push_rule(
    rules: &mut Vec<PermissionRule>,
    matches: &impl Fn(&str) -> bool,
    replacement: PermissionRule,
) {
    if let Some(index) = rules.iter().position(|rule| matches(&rule.target)) {
        rules[index] = replacement;
        let mut extra = index + 1;
        while extra < rules.len() {
            if matches(&rules[extra].target) {
                rules.remove(extra);
            } else {
                extra += 1;
            }
        }
    } else {
        rules.push(replacement);
    }
}

impl SqliteWorkflowStore {
    /// The draft row a release will freeze, read under a short lock so the freeze
    /// itself (which reads the catalog through the store) runs unlocked.
    pub(crate) fn draft_for_freeze(
        &self,
        kind: CatalogKind,
        urn: &str,
    ) -> Result<SpecCatalogRow, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        fetch_spec_draft_row(&connection, kind.table(), urn)?
            .ok_or_else(|| release_no_draft_error(kind_noun(kind), urn))
    }

    pub fn publish_agent_draft_with_owner(
        &self,
        spec: &AgentSpec,
        owner_user_id: &str,
    ) -> Result<AgentSpec, OrchestratorError> {
        let mut spec = spec.clone();
        apply_publish_permission_defaults(&mut spec);
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
        crate::catalog_tier::reject_caller_platform_urn(urn, "agent")?;
        let (bytes, digest) = encode_spec(&spec)?;
        let owner = crate::owner::resolved_owner(owner_user_id);
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let row = upsert_spec_draft(
            &connection,
            CatalogKind::Agent,
            urn,
            &bytes,
            &digest,
            owner,
            CATALOG_DRAFT_SLOT_MAJOR,
            CATALOG_DRAFT_SLOT_MINOR,
            CATALOG_DRAFT_SLOT_PATCH,
        )?;
        decode_agent(row)
    }

    pub fn release_agent(
        &self,
        urn: &str,
        version: &Version,
    ) -> Result<AgentSpec, OrchestratorError> {
        self.release_agent_with_meta(urn, version, &ReleaseMeta::default())
    }

    pub fn release_agent_with_meta(
        &self,
        urn: &str,
        version: &Version,
        meta: &ReleaseMeta<'_>,
    ) -> Result<AgentSpec, OrchestratorError> {
        validate_release_version(version)?;
        self.require_not_installed(urn, "agent")?;
        let draft = self.draft_for_freeze(CatalogKind::Agent, urn)?;
        let frozen = crate::catalog_freeze::freeze_agent(
            self,
            &draft.spec_bytes,
            &draft.digest,
            &draft.owner_user_id,
        )?;
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let frozen = FrozenRelease {
            draft_digest: draft.digest,
            bytes: frozen.bytes,
            digest: frozen.digest,
        };
        let row = release_spec_row(
            &connection,
            CatalogKind::Agent,
            urn,
            version,
            meta,
            Some(frozen),
        )?;
        decode_agent(row)
    }

    pub fn put_agent_released_with_owner(
        &self,
        spec: &AgentSpec,
        owner_user_id: &str,
    ) -> Result<AgentSpec, OrchestratorError> {
        let urn = spec
            .resource_id
            .as_ref()
            .map(|r| r.urn.as_str())
            .filter(|u| !u.is_empty())
            .ok_or_else(|| {
                OrchestratorError::InvalidContract("AgentSpec.resource_id is required".to_string())
            })?;
        crate::catalog_tier::reject_caller_platform_urn(urn, "agent")?;
        let (bytes, digest) = encode_spec(spec)?;
        let owner = crate::owner::resolved_owner(owner_user_id);
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let row = upsert_spec_released(
            &connection,
            CatalogKind::Agent,
            urn,
            &bytes,
            &digest,
            owner,
            CATALOG_RELEASED_MAJOR,
            CATALOG_RELEASED_MINOR,
            CATALOG_RELEASED_PATCH,
        )?;
        decode_agent(row)
    }

    pub fn publish_playbook_draft_with_owner(
        &self,
        spec: &PlaybookSpec,
        owner_user_id: &str,
    ) -> Result<PlaybookSpec, OrchestratorError> {
        let spec = playbook_ready_for_publish(spec)?;
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
        crate::catalog_tier::reject_caller_platform_urn(urn, "playbook")?;
        let (bytes, digest) = encode_spec(&spec)?;
        let owner = crate::owner::resolved_owner(owner_user_id);
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let row = upsert_spec_draft(
            &connection,
            CatalogKind::Playbook,
            urn,
            &bytes,
            &digest,
            owner,
            CATALOG_DRAFT_SLOT_MAJOR,
            CATALOG_DRAFT_SLOT_MINOR,
            CATALOG_DRAFT_SLOT_PATCH,
        )?;
        decode_playbook(row)
    }

    pub fn release_playbook(
        &self,
        urn: &str,
        version: &Version,
    ) -> Result<PlaybookSpec, OrchestratorError> {
        self.release_playbook_with_meta(urn, version, &ReleaseMeta::default())
    }

    pub fn release_playbook_with_meta(
        &self,
        urn: &str,
        version: &Version,
        meta: &ReleaseMeta<'_>,
    ) -> Result<PlaybookSpec, OrchestratorError> {
        validate_release_version(version)?;
        self.require_not_installed(urn, "playbook")?;
        let draft = self.draft_for_freeze(CatalogKind::Playbook, urn)?;
        playbook_ready_for_release(&decode_playbook_bytes(&draft.spec_bytes)?)?;
        let frozen = crate::catalog_freeze::freeze_playbook(
            self,
            &draft.spec_bytes,
            &draft.digest,
            &draft.owner_user_id,
        )?;
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let frozen = FrozenRelease {
            draft_digest: draft.digest,
            bytes: frozen.bytes,
            digest: frozen.digest,
        };
        let row = release_spec_row(
            &connection,
            CatalogKind::Playbook,
            urn,
            version,
            meta,
            Some(frozen),
        )?;
        decode_playbook(row)
    }

    pub fn put_playbook_released_with_owner(
        &self,
        spec: &PlaybookSpec,
        owner_user_id: &str,
    ) -> Result<PlaybookSpec, OrchestratorError> {
        let spec = playbook_ready_for_release(spec)?;
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
        crate::catalog_tier::reject_caller_platform_urn(urn, "playbook")?;
        let (bytes, digest) = encode_spec(&spec)?;
        let owner = crate::owner::resolved_owner(owner_user_id);
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let row = upsert_spec_released(
            &connection,
            CatalogKind::Playbook,
            urn,
            &bytes,
            &digest,
            owner,
            CATALOG_RELEASED_MAJOR,
            CATALOG_RELEASED_MINOR,
            CATALOG_RELEASED_PATCH,
        )?;
        decode_playbook(row)
    }

    pub fn publish_skill_draft_with_owner(
        &self,
        spec: &SkillManifest,
        owner_user_id: &str,
    ) -> Result<SkillManifest, OrchestratorError> {
        spec.validate()
            .map_err(|e| OrchestratorError::InvalidContract(e.to_string()))?;
        for warning in vam_spec::validation::skill_manifest_warnings(spec) {
            tracing::warn!("{warning}");
        }
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
        crate::catalog_tier::reject_caller_platform_urn(urn, "skill")?;
        let (bytes, digest) = encode_spec(spec)?;
        let owner = crate::owner::resolved_owner(owner_user_id);
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let row = upsert_spec_draft(
            &connection,
            CatalogKind::Skill,
            urn,
            &bytes,
            &digest,
            owner,
            CATALOG_DRAFT_SLOT_MAJOR,
            CATALOG_DRAFT_SLOT_MINOR,
            CATALOG_DRAFT_SLOT_PATCH,
        )?;
        decode_skill(row)
    }

    pub fn release_skill(
        &self,
        urn: &str,
        version: &Version,
    ) -> Result<SkillManifest, OrchestratorError> {
        self.release_skill_with_meta(urn, version, &ReleaseMeta::default())
    }

    pub fn release_skill_with_meta(
        &self,
        urn: &str,
        version: &Version,
        meta: &ReleaseMeta<'_>,
    ) -> Result<SkillManifest, OrchestratorError> {
        validate_release_version(version)?;
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let row = release_spec_row(&connection, CatalogKind::Skill, urn, version, meta, None)?;
        decode_skill(row)
    }

    pub fn put_skill_released_with_owner(
        &self,
        spec: &SkillManifest,
        owner_user_id: &str,
    ) -> Result<SkillManifest, OrchestratorError> {
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
        crate::catalog_tier::reject_caller_platform_urn(urn, "skill")?;
        let (bytes, digest) = encode_spec(spec)?;
        let owner = crate::owner::resolved_owner(owner_user_id);
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let row = upsert_spec_released(
            &connection,
            CatalogKind::Skill,
            urn,
            &bytes,
            &digest,
            owner,
            CATALOG_RELEASED_MAJOR,
            CATALOG_RELEASED_MINOR,
            CATALOG_RELEASED_PATCH,
        )?;
        decode_skill(row)
    }

    pub fn publish_policy_draft_with_owner(
        &self,
        urn: &str,
        cedar: &str,
        owner_user_id: &str,
    ) -> Result<PolicyRecord, OrchestratorError> {
        crate::catalog_tier::reject_caller_platform_urn(urn, "policy")?;
        CedarPolicyEngine::from_default_schema(cedar, PolicyMode::Enforce, 1).map_err(|e| {
            OrchestratorError::InvalidContract(format!("Cedar compile failed: {e}"))
        })?;
        let digest = format!("{:x}", Sha256::digest(cedar.as_bytes()));
        let owner = crate::owner::resolved_owner(owner_user_id);
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let row = upsert_policy_draft(
            &connection,
            urn,
            cedar,
            &digest,
            owner,
            CATALOG_DRAFT_SLOT_MAJOR,
            CATALOG_DRAFT_SLOT_MINOR,
            CATALOG_DRAFT_SLOT_PATCH,
            false,
        )?;
        Ok(policy_record_from_row(row))
    }

    pub fn publish_platform_policy_draft(
        &self,
        urn: &str,
        cedar: &str,
    ) -> Result<PolicyRecord, OrchestratorError> {
        CedarPolicyEngine::from_default_schema(cedar, PolicyMode::Enforce, 1).map_err(|e| {
            OrchestratorError::InvalidContract(format!("Cedar compile failed: {e}"))
        })?;
        let digest = format!("{:x}", Sha256::digest(cedar.as_bytes()));
        let owner = crate::users::OPERATOR_USER_ID;
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let row = upsert_policy_draft(
            &connection,
            urn,
            cedar,
            &digest,
            owner,
            CATALOG_DRAFT_SLOT_MAJOR,
            CATALOG_DRAFT_SLOT_MINOR,
            CATALOG_DRAFT_SLOT_PATCH,
            true,
        )?;
        Ok(policy_record_from_row(row))
    }

    pub fn release_policy(
        &self,
        urn: &str,
        version: &Version,
    ) -> Result<PolicyRecord, OrchestratorError> {
        self.release_policy_with_meta(urn, version, &ReleaseMeta::default())
    }

    pub fn release_policy_with_meta(
        &self,
        urn: &str,
        version: &Version,
        meta: &ReleaseMeta<'_>,
    ) -> Result<PolicyRecord, OrchestratorError> {
        validate_release_version(version)?;
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let row = release_policy_row(&connection, urn, version, meta)?;
        Ok(policy_record_from_row(row))
    }

    pub fn put_policy_released_with_owner(
        &self,
        urn: &str,
        cedar: &str,
        owner_user_id: &str,
    ) -> Result<PolicyRecord, OrchestratorError> {
        crate::catalog_tier::reject_caller_platform_urn(urn, "policy")?;
        CedarPolicyEngine::from_default_schema(cedar, PolicyMode::Enforce, 1).map_err(|e| {
            OrchestratorError::InvalidContract(format!("Cedar compile failed: {e}"))
        })?;
        let digest = format!("{:x}", Sha256::digest(cedar.as_bytes()));
        let owner = crate::owner::resolved_owner(owner_user_id);
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let row = upsert_policy_released(
            &connection,
            urn,
            cedar,
            &digest,
            owner,
            CATALOG_RELEASED_MAJOR,
            CATALOG_RELEASED_MINOR,
            CATALOG_RELEASED_PATCH,
        )?;
        Ok(policy_record_from_row(row))
    }
}

/// Shared acceptance scenario for "release works more than once". Each kind
/// supplies a draft publish keyed by a content tag and a release call.
#[cfg(test)]
pub(crate) mod release_scenario {
    use super::*;

    pub(crate) fn version(major: i32, minor: i32, patch: i32) -> Version {
        Version {
            major,
            minor,
            patch,
            prerelease: String::new(),
            build: String::new(),
        }
    }

    fn released_triples(store: &SqliteWorkflowStore, table: &str, urn: &str) -> Vec<ReleaseTriple> {
        let connection = store.connection.lock().expect("lock");
        let mut stmt = connection
            .prepare(&format!(
                "SELECT major, minor, patch FROM {table}
                 WHERE urn = ?1 AND channel = 'released'
                 ORDER BY major, minor, patch"
            ))
            .expect("prepare");
        stmt.query_map([urn], |row| {
            Ok::<ReleaseTriple, rusqlite::Error>((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .expect("query")
        .map(|row| row.expect("row"))
        .collect()
    }

    fn draft_rows(store: &SqliteWorkflowStore, table: &str, urn: &str) -> i64 {
        store
            .connection
            .lock()
            .expect("lock")
            .query_row(
                &format!("SELECT COUNT(*) FROM {table} WHERE urn = ?1 AND channel = 'draft'"),
                [urn],
                |row| row.get(0),
            )
            .expect("draft count")
    }

    fn latest(store: &SqliteWorkflowStore, table: &str, urn: &str) -> Option<ReleaseTriple> {
        let connection = store.connection.lock().expect("lock");
        latest_released_triple(&connection, table, urn).expect("latest")
    }

    /// Runs the release steps from AX-1.2 against one artifact.
    pub(crate) fn assert_release_lifecycle<E: std::fmt::Debug>(
        store: &SqliteWorkflowStore,
        table: &str,
        urn: &str,
        noun: &str,
        publish_draft: impl Fn(&str),
        release: impl Fn(&Version) -> Result<(), E>,
    ) {
        // 1. Publish a draft and release 0.1.0.
        publish_draft("a");
        release(&version(0, 1, 0)).expect("release 0.1.0");

        // 2. Change the draft and release 0.1.1.
        publish_draft("b");
        release(&version(0, 1, 1)).expect("release 0.1.1");

        // 3. Change the draft and release 1.0.0.
        publish_draft("c");
        release(&version(1, 0, 0)).expect("release 1.0.0");

        // 4. Three released rows and one draft; the latest release is 1.0.0.
        assert_eq!(
            released_triples(store, table, urn),
            vec![(0, 1, 0), (0, 1, 1), (1, 0, 0)]
        );
        assert_eq!(draft_rows(store, table, urn), 1);
        assert_eq!(latest(store, table, urn), Some((1, 0, 0)));

        // 5. A lower version is rejected.
        let err = release(&version(0, 0, 9)).expect_err("release 0.0.9");
        let message = format!("{err:?}");
        assert!(
            message.contains("must be greater than the latest release 1.0.0"),
            "{message}"
        );
        assert_eq!(released_triples(store, table, urn).len(), 3);

        // 6. Releasing 1.0.0 again with the same draft returns the same row.
        release(&version(1, 0, 0)).expect("idempotent release 1.0.0");
        assert_eq!(released_triples(store, table, urn).len(), 3);

        // 6b. The same draft at a higher version is refused: nothing changed.
        let err = release(&version(1, 0, 1)).expect_err("unchanged release 1.0.1");
        let message = format!("{err:?}");
        assert!(
            message.contains(&format!(
                "{noun} '{urn}' has nothing changed since 1.0.0; edit the draft before releasing a new version"
            )),
            "{message}"
        );
        assert_eq!(released_triples(store, table, urn).len(), 3);
        assert_eq!(latest(store, table, urn), Some((1, 0, 0)));

        // 7. A changed draft at an existing version conflicts and names 1.0.1.
        publish_draft("d");
        let err = release(&version(1, 0, 0)).expect_err("conflicting release 1.0.0");
        let message = format!("{err:?}");
        assert!(
            message.contains(&format!(
                "{noun} '{urn}' 1.0.0 is already released with different content; the next free version is 1.0.1"
            )),
            "{message}"
        );
        assert_eq!(draft_rows(store, table, urn), 1);
    }
}

#[cfg(test)]
mod tests {
    use super::release_scenario::{assert_release_lifecycle, version};
    use super::*;
    use crate::error::OrchestratorError;
    use vam_spec::v1::{
        agent_spec::InstructionSource, AgentPermissions, CapabilityPolicyRef, HitlPolicyRef,
        HostDefault, InternetPermissions, PermissionDecision, PermissionRule, PlaybookStep,
        ResourceId, StepMode, ToolDefault, ToolPermissions,
    };

    const TEST_CEDAR: &str = r#"permit(principal, action, resource);"#;
    const OTHER_CEDAR: &str = r#"permit(principal == User::"other", action, resource);"#;

    fn test_agent(urn: &str, instructions: &str) -> AgentSpec {
        AgentSpec {
            resource_id: Some(ResourceId {
                urn: urn.to_string(),
            }),
            display_name: "Release test".to_string(),
            description: "test".to_string(),
            role: "tester".to_string(),
            instruction_source: Some(InstructionSource::SystemInstructions(
                instructions.to_string(),
            )),
            allow_any_playbook: true,
            ..Default::default()
        }
    }

    fn store() -> SqliteWorkflowStore {
        let store = SqliteWorkflowStore::in_memory().expect("in-memory store");
        store.initialize_catalog_schema().expect("catalog schema");
        store
    }

    #[test]
    fn agent_draft_replace_increments_revision_on_new_digest() {
        let store = store();
        let urn = "urn:vam:agent:local.vam.dev/acme/draft-replace";
        let first = store
            .publish_agent_draft_with_owner(&test_agent(urn, "a"), crate::users::LOCAL_USER_ID)
            .expect("first draft");
        assert_eq!(
            first.resource_id.as_ref().map(|r| r.urn.as_str()),
            Some(urn)
        );

        store
            .publish_agent_draft_with_owner(&test_agent(urn, "b"), crate::users::LOCAL_USER_ID)
            .expect("draft replace");
        let conn = store.connection.lock().expect("lock");
        let revision: i64 = conn
            .query_row(
                "SELECT revision FROM agents WHERE urn = ?1 AND prerelease = 'draft'",
                [urn],
                |row| row.get(0),
            )
            .expect("revision");
        assert_eq!(revision, 2);
    }

    #[test]
    fn agent_draft_same_digest_is_idempotent() {
        let store = store();
        let urn = "urn:vam:agent:local.vam.dev/acme/idempotent";
        store
            .publish_agent_draft_with_owner(&test_agent(urn, "same"), crate::users::LOCAL_USER_ID)
            .expect("first");
        store
            .publish_agent_draft_with_owner(&test_agent(urn, "same"), crate::users::LOCAL_USER_ID)
            .expect("second");
        let conn = store.connection.lock().expect("lock");
        let revision: i64 = conn
            .query_row(
                "SELECT revision FROM agents WHERE urn = ?1 AND prerelease = 'draft'",
                [urn],
                |row| row.get(0),
            )
            .expect("revision");
        assert_eq!(revision, 1);
    }

    #[test]
    fn agent_released_rejects_digest_overwrite() {
        let store = store();
        let urn = "urn:vam:agent:local.vam.dev/acme/released-conflict";
        store
            .put_agent_released_with_owner(&test_agent(urn, "a"), crate::users::LOCAL_USER_ID)
            .expect("released a");
        let err = store
            .put_agent_released_with_owner(&test_agent(urn, "b"), crate::users::LOCAL_USER_ID)
            .expect_err("released overwrite");
        assert!(matches!(err, OrchestratorError::Conflict(_)));
    }

    #[test]
    fn agent_promote_keeps_draft_and_creates_released_row() {
        let store = store();
        let urn = "urn:vam:agent:local.vam.dev/acme/promote";
        store
            .publish_agent_draft_with_owner(&test_agent(urn, "ship"), crate::users::LOCAL_USER_ID)
            .expect("draft");
        let released = store
            .release_agent(urn, &catalog_release_version())
            .expect("promote");
        assert_eq!(
            released.resource_id.as_ref().map(|r| r.urn.as_str()),
            Some(urn)
        );
        assert_eq!(store.catalog_row_count("agents", urn).expect("count"), 2);
        store.get_agent(urn).expect("released readable");
    }

    fn test_playbook(urn: &str, instruction: &str) -> PlaybookSpec {
        PlaybookSpec {
            activation: Some(vam_spec::v1::PlaybookActivation::asking(
                "Use when the request matches this test playbook.",
            )),
            resource_id: Some(ResourceId {
                urn: urn.to_string(),
            }),
            display_name: "Release playbook".to_string(),
            description: "test".to_string(),
            steps: vec![PlaybookStep {
                id: "s1".to_string(),
                description: "step".to_string(),
                instruction: instruction.to_string(),
                mode: StepMode::Agentic as i32,
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    #[test]
    fn agent_release_lifecycle_releases_more_than_once() {
        let store = store();
        let urn = "urn:vam:agent:local.vam.dev/acme/release-again";
        assert_release_lifecycle(
            &store,
            "agents",
            urn,
            "agent",
            |tag| {
                store
                    .publish_agent_draft_with_owner(
                        &test_agent(urn, tag),
                        crate::users::LOCAL_USER_ID,
                    )
                    .expect("draft");
            },
            |v| store.release_agent(urn, v).map(|_| ()),
        );
        store.get_agent(urn).expect("latest release is readable");
    }

    #[test]
    fn playbook_release_lifecycle_releases_more_than_once() {
        let store = store();
        let urn = "urn:vam:playbook:local.vam.dev/acme/release-again";
        assert_release_lifecycle(
            &store,
            "playbooks",
            urn,
            "playbook",
            |tag| {
                store
                    .publish_playbook_draft_with_owner(
                        &test_playbook(urn, tag),
                        crate::users::LOCAL_USER_ID,
                    )
                    .expect("draft");
            },
            |v| store.release_playbook(urn, v).map(|_| ()),
        );
        store.get_playbook(urn).expect("latest release is readable");
    }

    #[test]
    fn a_draft_saves_without_when_but_a_release_requires_it() {
        let store = store();
        let urn = "urn:vam:playbook:local.vam.dev/acme/needs-when";
        let mut spec = test_playbook(urn, "Do it.");
        spec.activation = None;

        // A draft may be saved without activation.
        store
            .publish_playbook_draft_with_owner(&spec, crate::users::LOCAL_USER_ID)
            .expect("draft without when");

        // Publishing (a release) refuses it, with a message that says what to add.
        let err = store
            .release_playbook(urn, &catalog_release_version())
            .expect_err("release without when");
        let message = err.to_string();
        assert!(
            message.contains("playbook.activation.when is required"),
            "{message}"
        );
        assert!(message.contains("Add it and publish again."), "{message}");

        // The one-step put (draft plus release) refuses it too, and so does a blank `when`.
        let err = store
            .put_playbook_with_owner(&spec, crate::users::LOCAL_USER_ID)
            .expect_err("put without when");
        assert!(
            err.to_string().contains("activation.when is required"),
            "{err}"
        );
        let err = store
            .put_playbook_released_with_owner(
                &PlaybookSpec {
                    activation: Some(vam_spec::v1::PlaybookActivation::asking("   ")),
                    ..test_playbook(urn, "Do it.")
                },
                crate::users::LOCAL_USER_ID,
            )
            .expect_err("blank when");
        assert!(
            err.to_string().contains("activation.when is required"),
            "{err}"
        );

        // Add the sentence in the draft and the release goes through.
        store
            .publish_playbook_draft_with_owner(
                &test_playbook(urn, "Do it."),
                crate::users::LOCAL_USER_ID,
            )
            .expect("draft with when");
        store
            .release_playbook(urn, &catalog_release_version())
            .expect("release with when");
    }

    #[test]
    fn policy_release_lifecycle_releases_more_than_once() {
        let store = store();
        let urn = "urn:vam:policy:local.vam.dev/acme/release-again";
        assert_release_lifecycle(
            &store,
            "policies",
            urn,
            "policy",
            |tag| {
                let cedar = format!(r#"permit(principal == User::"{tag}", action, resource);"#);
                store
                    .publish_policy_draft_with_owner(urn, &cedar, crate::users::LOCAL_USER_ID)
                    .expect("draft");
            },
            |v| store.release_policy(urn, v).map(|_| ()),
        );
        store.get_policy(urn).expect("latest release is readable");
    }

    /// `major.minor.patch`, with `-prerelease` when the record carries one.
    fn version_label(version: Option<&Version>) -> String {
        let v = version.expect("list record carries a version");
        if v.prerelease.is_empty() {
            format!("{}.{}.{}", v.major, v.minor, v.patch)
        } else {
            format!("{}.{}.{}-{}", v.major, v.minor, v.patch, v.prerelease)
        }
    }

    #[test]
    fn agent_list_records_carry_version_and_only_owner_sees_draft() {
        let store = store();
        store.create_user("bob", "bob").expect("member");
        let owner = crate::users::LOCAL_USER_ID;
        let urn = "urn:vam:agent:local.vam.dev/acme/store-versions";
        store
            .publish_agent_draft_with_owner(&test_agent(urn, "draft"), owner)
            .expect("draft");
        store
            .release_agent(urn, &version(0, 1, 0))
            .expect("release 0.1.0");
        store
            .publish_agent_draft_with_owner(&test_agent(urn, "draft v2"), owner)
            .expect("draft v2");
        store
            .release_agent(urn, &version(1, 0, 0))
            .expect("release 1.0.0");

        let labels = |records: Vec<vam_spec::v1::AgentRecord>| {
            let mut labels: Vec<String> = records
                .iter()
                .filter(|r| {
                    r.spec
                        .as_ref()
                        .and_then(|s| s.resource_id.as_ref())
                        .is_some_and(|id| id.urn == urn)
                })
                .map(|r| version_label(r.version.as_ref()))
                .collect();
            labels.sort();
            labels
        };
        let owner_records = store
            .list_agent_records_visible(owner, false)
            .expect("owner list");
        assert_eq!(
            labels(owner_records),
            ["0.1.0", "1.0.0", "1.0.0-draft"].map(String::from)
        );
        let member_records = store
            .list_agent_records_visible("bob", false)
            .expect("member list");
        assert_eq!(labels(member_records), ["0.1.0", "1.0.0"].map(String::from));
    }

    #[test]
    fn policy_list_records_carry_version_and_only_owner_sees_draft() {
        let store = store();
        store.create_user("bob", "bob").expect("member");
        let owner = crate::users::LOCAL_USER_ID;
        let urn = "urn:vam:policy:local.vam.dev/acme/store-versions";
        let cedar = r#"permit(principal == User::"owner", action, resource);"#;
        store
            .publish_policy_draft_with_owner(urn, cedar, owner)
            .expect("draft");
        store
            .release_policy(urn, &version(0, 1, 0))
            .expect("release 0.1.0");
        let cedar_v2 = r#"permit(principal == User::"owner-v2", action, resource);"#;
        store
            .publish_policy_draft_with_owner(urn, cedar_v2, owner)
            .expect("draft v2");
        store
            .release_policy(urn, &version(1, 0, 0))
            .expect("release 1.0.0");

        let labels = |records: Vec<PolicyRecord>| {
            let mut labels: Vec<String> = records
                .iter()
                .filter(|r| r.resource_id.as_ref().is_some_and(|id| id.urn == urn))
                .map(|r| version_label(r.version.as_ref()))
                .collect();
            labels.sort();
            labels
        };
        let owner_records = store.list_policies_visible(owner).expect("owner list");
        assert_eq!(
            labels(owner_records.clone()),
            ["0.1.0", "1.0.0", "1.0.0-draft"].map(String::from)
        );
        let draft = owner_records
            .iter()
            .find(|r| r.version.as_ref().is_some_and(|v| v.prerelease == "draft"))
            .expect("owner draft");
        assert_eq!(
            draft.channel,
            vam_spec::v1::SkillChannel::Draft as i32,
            "{draft:?}"
        );

        let member_records = store.list_policies_visible("bob").expect("member list");
        assert_eq!(labels(member_records), ["0.1.0", "1.0.0"].map(String::from));
    }

    #[test]
    fn release_without_draft_names_the_release_error() {
        let store = store();
        let urn = "urn:vam:agent:local.vam.dev/acme/no-draft";
        let err = store
            .release_agent(urn, &version(1, 0, 0))
            .expect_err("no draft");
        match err {
            OrchestratorError::NotFound(message) => {
                assert_eq!(message, format!("no draft of agent '{urn}' to release"));
            }
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[test]
    fn policy_promote_conflicts_when_released_digest_differs() {
        let store = store();
        let urn = "urn:vam:policy:local.vam.dev/acme/promote-conflict";
        store
            .publish_policy_draft_with_owner(urn, TEST_CEDAR, crate::users::LOCAL_USER_ID)
            .expect("draft");
        store
            .put_policy_released_with_owner(urn, OTHER_CEDAR, crate::users::LOCAL_USER_ID)
            .expect("other released");
        let err = store
            .release_policy(urn, &catalog_release_version())
            .expect_err("promote conflict");
        assert!(matches!(err, OrchestratorError::Conflict(_)));
    }

    #[test]
    fn non_owner_cannot_version_agent_or_policy() {
        let store = store();
        store.create_user("alice", "alice").expect("alice");
        store.create_user("bob", "bob").expect("bob");
        let agent_urn = "urn:vam:agent:local.vam.dev/acme/owned";
        store
            .publish_agent_draft_with_owner(&test_agent(agent_urn, "alice"), "alice")
            .expect("alice draft");
        let err = store
            .publish_agent_draft_with_owner(&test_agent(agent_urn, "bob"), "bob")
            .expect_err("bob cannot version");
        assert!(matches!(err, OrchestratorError::PermissionDenied(_)));

        let policy_urn = "urn:vam:policy:local.vam.dev/acme/owned";
        store
            .publish_policy_draft_with_owner(policy_urn, TEST_CEDAR, "alice")
            .expect("alice policy");
        let err = store
            .publish_policy_draft_with_owner(policy_urn, OTHER_CEDAR, "bob")
            .expect_err("bob cannot version policy");
        assert!(matches!(err, OrchestratorError::PermissionDenied(_)));
    }

    #[test]
    fn published_agent_run_stamps_caller_not_artifact_owner() {
        let store = store();
        store.create_user("alice", "alice").expect("alice");
        store.create_user("bob", "bob").expect("bob");
        let urn = "urn:vam:agent:local.vam.dev/acme/shared";
        store
            .put_agent_with_owner(&test_agent(urn, "alice"), "alice")
            .expect("alice publishes");
        store
            .persist_run_spec_with_owner(
                "run-bob",
                "{}",
                Some("sess-bob"),
                urn,
                Some("idem-bob"),
                "User::\"bob\"",
                "bob",
            )
            .expect("bob run");
        assert_eq!(store.run_owner_user_id("run-bob").expect("owner"), "bob");
        assert_eq!(
            store.agent_owner_user_id(urn).expect("agent owner"),
            "alice"
        );
    }

    use crate::users::LOCAL_USER_ID;

    const EXAMPLES_LOCAL_CEDAR: &str = r#"permit(principal, action, resource);"#;

    fn examples_local_playbook(urn: &str) -> vam_spec::v1::PlaybookSpec {
        vam_spec::v1::PlaybookSpec {
            activation: Some(vam_spec::v1::PlaybookActivation::asking(
                "Use when the request matches this test playbook.",
            )),
            resource_id: Some(ResourceId {
                urn: urn.to_string(),
            }),
            display_name: "pb".to_string(),
            description: "test".to_string(),
            steps: vec![vam_spec::v1::PlaybookStep {
                id: "s1".to_string(),
                description: "step".to_string(),
                instruction: "Do the step.".to_string(),
                mode: vam_spec::v1::StepMode::Agentic as i32,
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    fn examples_local_skill(urn: &str) -> vam_spec::v1::SkillManifest {
        vam_spec::v1::SkillManifest {
            resource_id: Some(ResourceId {
                urn: urn.to_string(),
            }),
            version: Some(catalog_release_version()),
            description: "test".to_string(),
            ..Default::default()
        }
    }

    fn assert_reserved(err: OrchestratorError, urn: &str) {
        match err {
            OrchestratorError::InvalidContract(msg) => {
                assert!(msg.contains(urn), "{msg}");
                assert!(msg.contains("examples.local"), "{msg}");
            }
            other => panic!("expected InvalidContract, got {other:?}"),
        }
    }

    #[test]
    fn caller_publish_under_examples_local_is_refused_per_kind() {
        let store = store();
        let agent_urn = "urn:vam:agent:examples.local/vam/caller-agent";
        let playbook_urn = "urn:vam:playbook:examples.local/vam/caller-playbook";
        let skill_urn = "urn:vam:skill:examples.local/vam/caller-skill";
        let policy_urn = "urn:vam:policy:examples.local/vam/caller-policy";

        let err = store
            .publish_agent_draft_with_owner(&test_agent(agent_urn, "x"), LOCAL_USER_ID)
            .expect_err("agent draft");
        assert_reserved(err, agent_urn);
        let err = store
            .put_agent_released_with_owner(&test_agent(agent_urn, "x"), LOCAL_USER_ID)
            .expect_err("agent released");
        assert_reserved(err, agent_urn);

        let err = store
            .publish_playbook_draft_with_owner(
                &examples_local_playbook(playbook_urn),
                LOCAL_USER_ID,
            )
            .expect_err("playbook draft");
        assert_reserved(err, playbook_urn);
        let err = store
            .put_playbook_released_with_owner(&examples_local_playbook(playbook_urn), LOCAL_USER_ID)
            .expect_err("playbook released");
        assert_reserved(err, playbook_urn);

        let err = store
            .publish_skill_draft_with_owner(&examples_local_skill(skill_urn), LOCAL_USER_ID)
            .expect_err("skill draft");
        assert_reserved(err, skill_urn);
        let err = store
            .put_skill_released_with_owner(&examples_local_skill(skill_urn), LOCAL_USER_ID)
            .expect_err("skill released");
        assert_reserved(err, skill_urn);

        let err = store
            .publish_policy_draft_with_owner(policy_urn, EXAMPLES_LOCAL_CEDAR, LOCAL_USER_ID)
            .expect_err("policy draft");
        assert_reserved(err, policy_urn);
        let err = store
            .put_policy_released_with_owner(policy_urn, EXAMPLES_LOCAL_CEDAR, LOCAL_USER_ID)
            .expect_err("policy released");
        assert_reserved(err, policy_urn);

        assert_eq!(store.count_examples_local_catalog_rows().expect("count"), 0);
    }

    #[test]
    fn platform_seed_policy_under_examples_local_still_publishes() {
        let store = store();
        const URN: &str = "urn:vam:policy:examples.local/vam/seeded-policy";
        store
            .put_platform_policy(URN, EXAMPLES_LOCAL_CEDAR)
            .expect("platform seed");
        assert_eq!(
            store.catalog_tier_for_urn("policies", URN).expect("tier"),
            crate::catalog_tier::CATALOG_TIER_PLATFORM
        );
    }

    #[test]
    fn caller_publish_outside_examples_local_still_works() {
        let store = store();
        let urn = "urn:vam:agent:local.vam.dev/acme/allowed-agent";
        store
            .publish_agent_draft_with_owner(&test_agent(urn, "x"), LOCAL_USER_ID)
            .expect("local.vam.dev agent draft");
    }

    fn draft_revision(store: &SqliteWorkflowStore, urn: &str) -> i64 {
        let conn = store.connection.lock().expect("lock");
        conn.query_row(
            "SELECT revision FROM agents WHERE urn = ?1 AND prerelease = 'draft'",
            [urn],
            |row| row.get(0),
        )
        .expect("revision")
    }

    fn rule(target: &str, decision: PermissionDecision) -> PermissionRule {
        PermissionRule {
            target: target.to_string(),
            decision: decision as i32,
        }
    }

    #[test]
    fn publish_minimal_agent_stores_permission_defaults() {
        let store = store();
        let urn = "urn:vam:agent:local.vam.dev/acme/defaults";
        let saved = store
            .publish_agent_draft_with_owner(&test_agent(urn, "minimal"), LOCAL_USER_ID)
            .expect("draft");
        let permissions = saved.permissions.expect("defaults stored");
        assert_eq!(
            permissions.tools.expect("tools").default,
            ToolDefault::BySideEffect as i32
        );
        assert_eq!(
            permissions.internet.expect("internet").default,
            HostDefault::Allow as i32
        );
        assert!(saved.capability_policy.is_none());
        assert!(saved.hitl_policy.is_none());
    }

    #[test]
    fn publish_keeps_explicit_permissions() {
        let store = store();
        let urn = "urn:vam:agent:local.vam.dev/acme/explicit";
        let mut spec = test_agent(urn, "explicit");
        spec.permissions = Some(AgentPermissions {
            tools: Some(ToolPermissions {
                default: ToolDefault::Deny as i32,
                rules: vec![rule("builtin:run", PermissionDecision::Deny)],
            }),
            internet: Some(InternetPermissions {
                default: HostDefault::Deny as i32,
                rules: Vec::new(),
            }),
            ..Default::default()
        });
        let saved = store
            .publish_agent_draft_with_owner(&spec, LOCAL_USER_ID)
            .expect("draft");
        let permissions = saved.permissions.expect("permissions");
        assert_eq!(
            permissions.tools.expect("tools").default,
            ToolDefault::Deny as i32
        );
        assert_eq!(
            permissions.internet.expect("internet").default,
            HostDefault::Deny as i32
        );
    }

    #[test]
    fn publish_leaves_a_legacy_draft_unchanged() {
        let store = store();
        let urn = "urn:vam:agent:local.vam.dev/acme/legacy";
        let mut spec = test_agent(urn, "legacy");
        spec.capability_policy = Some(CapabilityPolicyRef {
            security_profile_name: "restricted-egress".to_string(),
            ..Default::default()
        });
        let saved = store
            .publish_agent_draft_with_owner(&spec, LOCAL_USER_ID)
            .expect("legacy draft");
        assert!(saved.permissions.is_none());
        assert_eq!(
            saved
                .capability_policy
                .expect("capability policy")
                .security_profile_name,
            "restricted-egress"
        );

        let mut hitl = test_agent("urn:vam:agent:local.vam.dev/acme/legacy-hitl", "hitl only");
        hitl.hitl_policy = Some(HitlPolicyRef {
            monitored_tools: vec!["db_drop_table".to_string()],
            ..Default::default()
        });
        let saved = store
            .publish_agent_draft_with_owner(&hitl, LOCAL_USER_ID)
            .expect("hitl draft");
        assert!(saved.permissions.is_none());
        assert_eq!(
            saved.hitl_policy.expect("hitl").monitored_tools,
            vec!["db_drop_table".to_string()]
        );
    }

    #[test]
    fn released_put_does_not_fill_permission_defaults() {
        let store = store();
        let urn = "urn:vam:agent:local.vam.dev/acme/already-released";
        let saved = store
            .put_agent_released_with_owner(&test_agent(urn, "historical"), LOCAL_USER_ID)
            .expect("released");
        assert!(
            saved.permissions.is_none(),
            "a released row is stored as written"
        );
    }

    #[test]
    fn update_draft_permissions_replaces_rules_and_bumps_revision() {
        let store = store();
        let urn = "urn:vam:agent:local.vam.dev/acme/edit-permissions";
        store
            .publish_agent_draft_with_owner(&test_agent(urn, "draft"), LOCAL_USER_ID)
            .expect("draft");
        assert_eq!(draft_revision(&store, urn), 1);

        let saved = update_draft_permissions(&store, urn, LOCAL_USER_ID, |permissions| {
            upsert_permission_rule(
                permissions,
                rule(
                    "mcp:https://MCP.Atlassian.com:443/v1/create_issue",
                    PermissionDecision::Ask,
                ),
            );
            upsert_permission_rule(permissions, rule("builtin:run", PermissionDecision::Deny));
        })
        .expect("first edit");
        assert_eq!(draft_revision(&store, urn), 2);
        let rules = saved
            .permissions
            .expect("permissions")
            .tools
            .expect("tools")
            .rules;
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0].target, "mcp:mcp.atlassian.com/v1/create_issue");
        assert_eq!(rules[0].decision, PermissionDecision::Ask as i32);

        let saved = update_draft_permissions(&store, urn, LOCAL_USER_ID, |permissions| {
            upsert_permission_rule(
                permissions,
                rule(
                    "mcp:mcp.atlassian.com/v1/create_issue",
                    PermissionDecision::Allow,
                ),
            );
        })
        .expect("replace same target");
        assert_eq!(draft_revision(&store, urn), 3);
        let rules = saved
            .permissions
            .expect("permissions")
            .tools
            .expect("tools")
            .rules;
        assert_eq!(rules.len(), 2, "the builtin rule stays");
        assert_eq!(rules[0].target, "mcp:mcp.atlassian.com/v1/create_issue");
        assert_eq!(rules[0].decision, PermissionDecision::Allow as i32);
        assert_eq!(rules[1].target, "builtin:run");

        update_draft_permissions(&store, urn, LOCAL_USER_ID, |permissions| {
            upsert_permission_rule(
                permissions,
                rule(
                    "mcp:mcp.atlassian.com/v1/create_issue",
                    PermissionDecision::Allow,
                ),
            );
        })
        .expect("identical upsert");
        assert_eq!(draft_revision(&store, urn), 3);
    }

    #[test]
    fn update_draft_permissions_put_replaces_the_whole_set() {
        let store = store();
        let urn = "urn:vam:agent:local.vam.dev/acme/replace-permissions";
        store
            .publish_agent_draft_with_owner(&test_agent(urn, "draft"), LOCAL_USER_ID)
            .expect("draft");
        let replacement = AgentPermissions {
            tools: Some(ToolPermissions {
                default: ToolDefault::Deny as i32,
                rules: vec![rule("builtin:run", PermissionDecision::Deny)],
            }),
            internet: Some(InternetPermissions {
                default: HostDefault::Deny as i32,
                rules: vec![rule("*.pastebin.com", PermissionDecision::Deny)],
            }),
            ..Default::default()
        };
        let saved = update_draft_permissions(&store, urn, LOCAL_USER_ID, |permissions| {
            *permissions = replacement;
        })
        .expect("replace");
        let permissions = saved.permissions.expect("permissions");
        assert_eq!(
            permissions.tools.expect("tools").default,
            ToolDefault::Deny as i32
        );
        assert_eq!(
            permissions.internet.expect("internet").rules[0].target,
            "*.pastebin.com"
        );
        assert_eq!(draft_revision(&store, urn), 2);
    }

    #[test]
    fn update_draft_permissions_hides_a_foreign_draft() {
        let store = store();
        store.create_user("member", "member").expect("member");
        let urn = "urn:vam:agent:local.vam.dev/acme/admin-draft";
        store
            .publish_agent_draft_with_owner(&test_agent(urn, "admin"), LOCAL_USER_ID)
            .expect("admin draft");
        let err =
            update_draft_permissions(&store, urn, "member", |_| {}).expect_err("foreign draft");
        match err {
            OrchestratorError::NotFound(message) => {
                assert_eq!(message, format!("no draft of agent '{urn}' for this user"));
            }
            other => panic!("expected NotFound, got {other:?}"),
        }
        assert_eq!(draft_revision(&store, urn), 1);
        let missing = update_draft_permissions(
            &store,
            "urn:vam:agent:local.vam.dev/acme/absent",
            "member",
            |_| {},
        )
        .expect_err("missing draft");
        assert!(matches!(missing, OrchestratorError::NotFound(_)));
    }

    #[test]
    fn update_draft_permissions_legacy_draft_asks_to_migrate() {
        let store = store();
        let urn = "urn:vam:agent:local.vam.dev/acme/legacy-edit";
        let mut spec = test_agent(urn, "legacy");
        spec.capability_policy = Some(CapabilityPolicyRef::default());
        store
            .publish_agent_draft_with_owner(&spec, LOCAL_USER_ID)
            .expect("legacy draft");
        let err = update_draft_permissions(&store, urn, LOCAL_USER_ID, |permissions| {
            *permissions = AgentPermissions::default();
        })
        .expect_err("legacy edit");
        match err {
            OrchestratorError::Conflict(message) => {
                assert!(message.contains("migrate first"), "{message}");
            }
            other => panic!("expected Conflict, got {other:?}"),
        }
        let stored = load_agent_draft(&store, urn, LOCAL_USER_ID, false).expect("still legacy");
        assert!(stored.permissions.is_none());
        assert!(stored.capability_policy.is_some());
        assert_eq!(draft_revision(&store, urn), 1);
    }

    #[test]
    fn update_draft_permissions_rejects_an_invalid_rule_without_writing() {
        let store = store();
        let urn = "urn:vam:agent:local.vam.dev/acme/invalid-rule";
        store
            .publish_agent_draft_with_owner(&test_agent(urn, "draft"), LOCAL_USER_ID)
            .expect("draft");
        let err = update_draft_permissions(&store, urn, LOCAL_USER_ID, |permissions| {
            upsert_permission_rule(permissions, rule("skill:nope", PermissionDecision::Allow));
        })
        .expect_err("invalid target");
        match err {
            OrchestratorError::InvalidContract(message) => {
                assert!(
                    message.contains("skill:nope"),
                    "AX-5.1 validation message: {message}"
                );
            }
            other => panic!("expected InvalidContract, got {other:?}"),
        }
        assert_eq!(draft_revision(&store, urn), 1);
        let stored = load_agent_draft(&store, urn, LOCAL_USER_ID, false).expect("unchanged");
        assert!(stored
            .permissions
            .expect("defaults")
            .tools
            .expect("tools")
            .rules
            .is_empty());
    }
}
